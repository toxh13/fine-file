//! Fallback scanner: walks folders in parallel with normal user rights.
//! Slower than reading the MFT, and folders the user cannot open are skipped.

use crate::index::{RawEntry, FLAG_DIR, FLAG_HIDDEN, FLAG_NOT_LINK, FLAG_REPARSE, FLAG_SYSTEM};
use crate::win;
use std::ffi::OsString;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE: u32 = 0x400;

pub const ROOT_ID: u64 = 0;

/// What a walk found.
pub struct Walked {
    /// Every entry below the root, the root itself first; ids run from 0
    /// (ROOT_ID) up without gaps, and a folder's id is lower than its children's.
    pub entries: Vec<RawEntry>,
    /// Folders that are there but could not be listed (access denied, in use
    /// by the system, ...): their walk id and path. They are in `entries`,
    /// with nothing below them. A folder that was gone by the time it was
    /// opened is not one of them.
    pub unopened: Vec<(u64, PathBuf)>,
}

/// A walk watched and stopped from another thread.
#[derive(Default)]
pub struct Control {
    /// Set to stop: folders not opened yet are left out (the result is then
    /// incomplete and of no use).
    pub cancel: AtomicBool,
    /// Entries found so far.
    pub seen: AtomicU64,
}

impl Control {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Scans `root` (e.g. `C:\`) and returns every entry below it.
pub fn scan(root: &str) -> Vec<RawEntry> {
    walk(root, &Control::default()).entries
}

/// `scan`, reporting the folders it could not open, and watched by `ctl`.
pub fn walk(root: &str, ctl: &Control) -> Walked {
    let next_id = AtomicU64::new(1);
    let out = Mutex::new(vec![RawEntry {
        id: ROOT_ID,
        parent: ROOT_ID,
        name: String::new(),
        flags: FLAG_DIR,
        size: 0,
        alloc: 0,
        modified: 0,
    }]);
    let unopened = Mutex::new(Vec::new());
    let w = Walk { next_id: &next_id, out: &out, unopened: &unopened, ctl };
    rayon::scope(|s| visit(s, PathBuf::from(root), ROOT_ID, w));
    Walked { entries: out.into_inner().unwrap(), unopened: unopened.into_inner().unwrap() }
}

/// What every folder visit of one walk shares.
#[derive(Clone, Copy)]
struct Walk<'s> {
    next_id: &'s AtomicU64,
    out: &'s Mutex<Vec<RawEntry>>,
    unopened: &'s Mutex<Vec<(u64, PathBuf)>>,
    ctl: &'s Control,
}

fn visit<'s>(s: &rayon::Scope<'s>, dir: PathBuf, dir_id: u64, w: Walk<'s>) {
    if w.ctl.cancelled() {
        return;
    }
    let opened = match std::fs::read_dir(&dir) {
        // A name Windows changes on the way in (a trailing dot, made by WSL,
        // git or a `\\?\` tool) is listed, but its plain path is not found.
        // Its verbatim path takes it as it is, and so do its children's.
        Err(e) if gone(&e) => match verbatim(&dir) {
            Some(v) => std::fs::read_dir(v),
            None => Err(e),
        },
        other => other,
    };
    let read = match opened {
        Ok(read) => read,
        Err(e) => {
            if !gone(&e) {
                w.unopened.lock().unwrap().push((dir_id, plain(dir)));
            }
            return;
        }
    };
    let mut local = Vec::new();
    for entry in read.flatten() {
        // On Windows this metadata comes from the directory listing itself,
        // so it costs no extra system call per file.
        let Ok(meta) = entry.metadata() else { continue };
        let attrs = meta.file_attributes();
        // From the attribute, not meta.is_dir(): std says "not a folder" for
        // junctions and folder symlinks, which would list them as 0-byte files.
        let is_dir = attrs & ATTR_DIRECTORY != 0;
        let id = w.next_id.fetch_add(1, Ordering::Relaxed);
        let size = if is_dir { 0 } else { meta.file_size() };
        let tag = folder_tag(attrs, meta.file_type().is_symlink(), || entry.path());
        let flags = flags_of(attrs, tag);
        local.push(RawEntry {
            id,
            parent: dir_id,
            name: entry.file_name().to_string_lossy().into_owned(),
            flags,
            size,
            alloc: size,
            modified: meta.last_write_time() as i64,
        });
        // Never follow junctions, symlinks or other reparse points: they can
        // point at folders counted elsewhere. A cloud-files placeholder
        // holds its own contents (listing one the sync app has not filled
        // in yet may make it fetch that listing).
        if is_dir && flags & (FLAG_REPARSE | FLAG_NOT_LINK) != FLAG_REPARSE {
            let path = entry.path();
            s.spawn(move |s| visit(s, path, id, w));
        }
    }
    w.ctl.seen.fetch_add(local.len() as u64, Ordering::Relaxed);
    w.out.lock().unwrap().append(&mut local);
}

/// A folder that went away (or stopped being one) between being listed and
/// being opened: nothing was missed there. (Asked after its verbatim path
/// was tried too, `verbatim`.)
fn gone(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

const VERBATIM: &str = r"\\?\";

/// `dir` (`C:\...`) as a verbatim path (`\\?\C:\...`), which Windows takes
/// as it is: no trailing dots or spaces dropped. None for a path that is
/// not on a drive letter, or already verbatim.
fn verbatim(dir: &Path) -> Option<PathBuf> {
    match dir.components().next() {
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_)) => {
            let mut v = OsString::from(VERBATIM);
            v.push(dir.as_os_str());
            Some(PathBuf::from(v))
        }
        _ => None,
    }
}

/// `dir` without the `\\?\` that `verbatim` put in front, as paths are
/// written everywhere else (an index finds `C:\...`).
fn plain(dir: PathBuf) -> PathBuf {
    match dir.to_str().and_then(|s| s.strip_prefix(VERBATIM)) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => dir,
    }
}

/// The reparse tag `flags_of` needs for the entry at `path`: asked of
/// Windows only for a folder that is a reparse point std does not take for
/// a link (`link`, from `FileType::is_symlink`), as std cannot tell a
/// cloud-files placeholder from other such tags. 0 for everything else,
/// and when the tag cannot be read.
pub fn folder_tag(attrs: u32, link: bool, path: impl FnOnce() -> PathBuf) -> u32 {
    let reparse_dir = ATTR_DIRECTORY | ATTR_REPARSE;
    if attrs & reparse_dir != reparse_dir || link {
        return 0;
    }
    win::reparse_tag(&path()).unwrap_or(0)
}

/// Index flags for Windows file attributes and, for a reparse point, its
/// tag. Folder-ness comes from the attribute, so junctions and folder
/// symlinks count as folders. A cloud-files placeholder folder gets
/// FLAG_NOT_LINK; any other reparse point is taken for a link.
pub fn flags_of(attrs: u32, tag: u32) -> u32 {
    let mut flags = 0;
    if attrs & ATTR_DIRECTORY != 0 {
        flags |= FLAG_DIR;
    }
    if attrs & ATTR_HIDDEN != 0 {
        flags |= FLAG_HIDDEN;
    }
    if attrs & ATTR_SYSTEM != 0 {
        flags |= FLAG_SYSTEM;
    }
    if attrs & ATTR_REPARSE != 0 {
        flags |= FLAG_REPARSE;
        if attrs & ATTR_DIRECTORY != 0 && super::is_cloud_tag(tag) {
            flags |= FLAG_NOT_LINK;
        }
    }
    flags
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A temp folder with `real\a.bin` (10 bytes) and a junction `link` to
    /// `real`. None when junctions cannot be made here.
    pub(crate) fn tree_with_junction(tag: &str) -> Option<PathBuf> {
        let root = std::env::temp_dir().join(format!("ff-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real")).ok()?;
        std::fs::write(root.join("real").join("a.bin"), [0u8; 10]).ok()?;
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("link"))
            .arg(root.join("real"))
            .output()
            .is_ok_and(|o| o.status.success());
        made.then_some(root)
    }

    #[test]
    fn junctions_are_folders_that_are_never_followed() {
        let Some(root) = tree_with_junction("walk") else {
            eprintln!("mklink /J is not available; skipped");
            return;
        };
        let raw = scan(&format!("{}\\", root.display()));
        // Junctions are told from the listing's tag; a plain folder has none.
        assert_eq!(win::reparse_tag(&root.join("link")), Some(0xA000_0003));
        assert_eq!(win::reparse_tag(&root.join("real")), Some(0));
        assert_eq!(win::reparse_tag(&root.join("missing")), None);
        assert_eq!(folder_tag(ATTR_DIRECTORY | ATTR_REPARSE, false, || root.join("link")), 0xA000_0003);
        assert_eq!(folder_tag(ATTR_DIRECTORY | ATTR_REPARSE, true, || unreachable!()), 0);
        std::fs::remove_dir_all(&root).unwrap();
        let link = raw.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.flags & (FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK), FLAG_DIR | FLAG_REPARSE);
        assert_eq!(link.size, 0);
        // a.bin is found once, through the real folder only.
        assert_eq!(raw.iter().filter(|e| e.name == "a.bin").count(), 1);
    }

    #[test]
    fn folders_that_cannot_be_opened_are_reported() {
        let root = std::env::temp_dir().join(format!("ff-walk-denied-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locked").join("inner")).unwrap();
        std::fs::write(root.join("locked").join("secret.bin"), [0u8; 4]).unwrap();
        std::fs::create_dir_all(root.join("open")).unwrap();
        std::fs::write(root.join("open").join("a.bin"), [0u8; 2]).unwrap();
        let icacls = |args: &[&str]| {
            std::process::Command::new("icacls").arg(root.join("locked")).args(args).output().is_ok_and(|o| o.status.success())
        };
        // Nobody, the owner included, may list it (the owner may still change that).
        if !icacls(&["/deny", "*S-1-1-0:(RD)"]) {
            eprintln!("icacls is not available; skipped");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        // An account that the deny does not stop (an elevated CI runner with backup rights) still
        // lists it: nothing to test there.
        if std::fs::read_dir(root.join("locked")).is_ok() {
            eprintln!("this account can list a folder denied to everyone; skipped");
            icacls(&["/remove:d", "*S-1-1-0"]);
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let walked = walk(&format!("{}\\", root.display()), &Control::default());
        let restored = icacls(&["/remove:d", "*S-1-1-0"]);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(restored);
        let locked = walked.entries.iter().find(|e| e.name == "locked").unwrap();
        assert_eq!(walked.unopened.len(), 1);
        assert_eq!(walked.unopened[0].0, locked.id);
        assert!(walked.unopened[0].1.ends_with("locked"));
        // Listed, with nothing below it; the rest as usual.
        assert!(walked.entries.iter().all(|e| e.parent != locked.id));
        assert!(walked.entries.iter().any(|e| e.name == "a.bin"));
        // Ids from 0 without gaps, each folder before its children.
        let mut ids: Vec<u64> = walked.entries.iter().map(|e| e.id).collect();
        ids.sort();
        assert_eq!(ids, (0..walked.entries.len() as u64).collect::<Vec<_>>());
        assert!(walked.entries.iter().all(|e| e.parent < e.id || e.id == ROOT_ID));
    }

    #[test]
    fn folders_whose_names_windows_would_change_are_walked() {
        let root = std::env::temp_dir().join(format!("ff-walk-dots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // As WSL or git make them: a trailing dot, which a plain path loses.
        let odd = verbatim(&root.join("build.")).unwrap();
        std::fs::create_dir_all(odd.join("inner.")).unwrap();
        std::fs::write(odd.join("out.bin"), [0u8; 6]).unwrap();
        std::fs::write(odd.join("inner.").join("deep.bin"), [0u8; 4]).unwrap();
        assert_eq!(std::fs::read_dir(root.join("build.")).err().map(|e| gone(&e)), Some(true));
        let walked = walk(&format!("{}\\", root.display()), &Control::default());
        std::fs::remove_file(odd.join("inner.").join("deep.bin")).unwrap();
        std::fs::remove_dir(odd.join("inner.")).unwrap();
        std::fs::remove_file(odd.join("out.bin")).unwrap();
        std::fs::remove_dir(&odd).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(walked.unopened.is_empty(), "{:?}", walked.unopened);
        let id_of = |name: &str| walked.entries.iter().find(|e| e.name == name).map(|e| (e.id, e.parent)).unwrap();
        let (build, _) = id_of("build.");
        let (inner, inner_parent) = id_of("inner.");
        assert_eq!(inner_parent, build);
        assert_eq!(id_of("out.bin").1, build);
        assert_eq!(id_of("deep.bin").1, inner);
        // Paths go back as they are written elsewhere.
        assert_eq!(plain(odd.clone()), root.join("build."));
        assert_eq!(plain(root.clone()), root);
        assert_eq!(verbatim(&odd), None);
        assert_eq!(verbatim(Path::new(r"\\server\share\x")), None);
    }

    #[test]
    fn a_cancelled_walk_stops() {
        let ctl = Control::default();
        ctl.cancel.store(true, Ordering::Relaxed);
        let walked = walk(&format!("{}\\", std::env::temp_dir().display()), &ctl);
        // Only the root: no folder was opened.
        assert_eq!(walked.entries.len(), 1);
        assert_eq!(ctl.seen.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn only_cloud_placeholder_folders_are_marked() {
        let dir_reparse = ATTR_DIRECTORY | ATTR_REPARSE;
        // IO_REPARSE_TAG_CLOUD, _CLOUD_6 and _CLOUD_F.
        for cloud in [0x9000_001A, 0x9000_601A, 0x9000_F01A] {
            assert_eq!(flags_of(dir_reparse, cloud), FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK, "{cloud:#x}");
        }
        // Mount point, symlink, ProjFS, WCI, WCI_1, DFS, an app execution alias, an unread tag.
        for other in [0xA000_0003, 0xA000_000C, 0x9000_001C, 0x8000_0018, 0x9000_1018, 0x8000_000A, 0x8000_001B, 0] {
            assert_eq!(flags_of(dir_reparse, other), FLAG_DIR | FLAG_REPARSE, "{other:#x}");
        }
        // Files are never followed anyway.
        assert_eq!(flags_of(ATTR_REPARSE, 0x9000_001A), FLAG_REPARSE);
        assert_eq!(flags_of(ATTR_DIRECTORY | ATTR_HIDDEN, 0), FLAG_DIR | FLAG_HIDDEN);
    }
}
