pub mod mft;
pub mod sync;
pub mod walk;

use crate::index::Index;
use crate::win;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Whether a reparse tag is one of the cloud-files tags
/// (IO_REPARSE_TAG_CLOUD to IO_REPARSE_TAG_CLOUD_F, in any of their
/// variants): a OneDrive-style placeholder, whose folders hold their own
/// contents on this drive. Other tags that are not links (ProjFS, WCI, DFS,
/// ...) may stand for contents elsewhere, so their folders are not followed.
pub fn is_cloud_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == 0x9000_001A
}

/// Reads the MFT directly. Requires administrator rights.
pub fn scan_mft(drive: &str) -> io::Result<Index> {
    let t = Instant::now();
    let raw = mft::scan(drive)?;
    Ok(Index::build(drive, "mft", t.elapsed().as_millis() as u64, raw, mft::ROOT_RECORD))
}

/// `scan_mft` with a panic in the parser turned into an error, so a volume it
/// cannot make sense of costs this one scan, not the process.
pub fn scan_mft_guarded(drive: &str) -> io::Result<Index> {
    guarded(|| scan_mft(drive))
}

fn guarded(f: impl FnOnce() -> io::Result<Index>) -> io::Result<Index> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p.downcast_ref::<&str>().copied().or(p.downcast_ref::<String>().map(String::as_str));
        Err(io::Error::other(format!("내부 오류: {}", msg.unwrap_or("panic"))))
    })
}

/// Walks folders with normal rights. Works on any file system. Fails when the
/// drive's root cannot be opened (a network drive whose server is gone, an
/// ejected disc), so an empty result never replaces a good cache.
pub fn scan_walk(drive: &str) -> io::Result<Index> {
    let root = format!("{drive}\\");
    std::fs::read_dir(&root).map_err(|e| io::Error::new(e.kind(), format!("드라이브를 열 수 없어요: {e}")))?;
    let t = Instant::now();
    let raw = walk::scan(&root);
    Ok(Index::build(drive, "walk", t.elapsed().as_millis() as u64, raw, walk::ROOT_ID))
}

/// Must match `identifier` in tauri.conf.json: the uninstaller's "delete app
/// data" option removes `%LOCALAPPDATA%\<identifier>`.
const APP_ID: &str = "com.toxh13.fine-file";

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn data_dir() -> PathBuf {
    local_app_data().join(APP_ID)
}

fn index_dir() -> PathBuf {
    data_dir().join("index")
}

/// The cache files of one drive in `dir`: `<letter>.<ext>`.
fn file_in(dir: &Path, drive: &str, ext: &str) -> PathBuf {
    dir.join(format!("{}.{ext}", &drive[..1]))
}

pub fn cache_path(drive: &str) -> PathBuf {
    file_in(&index_dir(), drive, "idx")
}

/// The scan before the current one (`<letter>.prev.idx`), kept for comparing
/// the two (diff.rs). Only a new scan puts a file here (`keep_previous`).
pub fn prev_cache_path(drive: &str) -> PathBuf {
    file_in(&index_dir(), drive, "prev.idx")
}

/// The copy of the current scan (`<letter>.scan.idx`, its volume in
/// `.scan.vol`). The cache takes every live change, rename and trash after
/// the scan and keeps the scan's `scanned_at`, so by the next scan it no
/// longer is the scan it says it is. This copy is: `keep_scan` makes it right
/// after the scan's cache is written, and the next scan makes it the previous
/// scan (`keep_previous`).
pub fn scan_copy_path(drive: &str) -> PathBuf {
    file_in(&index_dir(), drive, "scan.idx")
}

/// Which volume a drive's cache was made from (`<letter>.vol`, holding the
/// volume GUID path). Another disk can later get the same letter.
fn volume_tag_path(drive: &str) -> PathBuf {
    file_in(&index_dir(), drive, "vol")
}

pub fn write_volume_tag(drive: &str, volume: Option<&str>) {
    tag_in(&index_dir(), drive, volume);
}

fn tag_in(dir: &Path, drive: &str, volume: Option<&str>) {
    let path = file_in(dir, drive, "vol");
    let _ = match volume {
        Some(v) => std::fs::write(path, v),
        None => std::fs::remove_file(path),
    };
}

/// Called right after a sync's cache (sync.rs) was written, in the same save
/// turn: tags the cache with the volume it was made from, and that is all.
/// The copy of the last full scan (`keep_scan`) and the previous scan stay
/// as they are, so the next full scan still compares with the full scan
/// before it, never with a sync, which shows what an earlier scan found in
/// the folders it could not open.
pub fn synced(drive: &str, volume: Option<&str>) {
    tag_in(&index_dir(), drive, volume);
}

/// Notes that the drive's cache, just written, was what is on the disk at
/// `at` (FILETIME): the app is closing, a scan or sync of this session
/// caught the drive up and its watcher kept up since (lib.rs
/// `save_on_exit`). `<letter>.live`, read once by the next start
/// (`take_in_step`).
pub fn write_in_step(drive: &str, at: i64) {
    write_in_step_in(&index_dir(), drive, at);
}

fn write_in_step_in(dir: &Path, drive: &str, at: i64) {
    let _ = std::fs::write(file_in(dir, drive, "live"), at.to_string());
}

/// What `write_in_step` noted, by drive. Every note is taken away as the
/// app starts, of drives that are not there now too: one speaks for a cache
/// only until an app that read it may write it again, and that app notes it
/// anew when it closes in step.
pub fn take_in_step() -> HashMap<String, i64> {
    take_in_step_in(&index_dir())
}

fn take_in_step_in(dir: &Path) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    for c in 'A'..='Z' {
        let drive = format!("{c}:");
        let path = file_in(dir, &drive, "live");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let _ = std::fs::remove_file(&path);
        if let Ok(at) = text.trim().parse::<i64>() {
            out.insert(drive, at);
        }
    }
    out
}

/// Whether the cache for `drive` belongs to the volume mounted there now.
/// Drives without a volume GUID (network drives) cannot be told apart, so
/// their cache is trusted.
pub fn cache_matches_volume(drive: &str) -> bool {
    tag_matches(&volume_tag_path(drive), win::volume_id(drive).as_deref())
}

/// `cache_matches_volume` for the previous scan (`<letter>.prev.vol`).
pub fn prev_matches_volume(drive: &str) -> bool {
    tag_matches(&file_in(&index_dir(), drive, "prev.vol"), win::volume_id(drive).as_deref())
}

fn tag_matches(tag: &Path, now: Option<&str>) -> bool {
    match now {
        None => true,
        Some(now) => std::fs::read_to_string(tag).is_ok_and(|t| t.trim() == now),
    }
}

/// The start of a cache file, read without loading the index behind it.
pub struct CacheHeader {
    pub drive: String,
    /// "mft", "walk" or "sync" (sync.rs).
    pub method: String,
    /// FILETIME of the scan.
    pub scanned_at: i64,
}

/// The first bytes `Index::save_to` writes (index.rs `MAGIC`); the test
/// `header_reads_what_the_index_writes` keeps the two in step.
const CACHE_MAGIC: &[u8; 8] = b"FFIDX\x00\x00\x01";

/// Reads the header of a cache file: magic, drive, method, scan time. A few
/// hundred bytes at most, however big the index is.
pub fn read_header(path: &Path) -> io::Result<CacheHeader> {
    let mut r = io::BufReader::with_capacity(1024, std::fs::File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != CACHE_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown index format"));
    }
    let drive = read_short_str(&mut r)?;
    let method = read_short_str(&mut r)?;
    let mut at = [0u8; 8];
    r.read_exact(&mut at)?;
    Ok(CacheHeader { drive, method, scanned_at: i64::from_le_bytes(at) })
}

fn read_short_str(r: &mut impl Read) -> io::Result<String> {
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len);
    if len > 256 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "string too long"));
    }
    let mut b = vec![0u8; len as usize];
    r.read_exact(&mut b)?;
    String::from_utf8(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The header of the drive's previous scan, if it has one made from the
/// volume mounted there now.
pub fn previous_scan(drive: &str) -> Option<CacheHeader> {
    if !prev_matches_volume(drive) {
        return None;
    }
    read_header(&prev_cache_path(drive)).ok().filter(|h| h.drive == drive)
}

/// Called by a new scan of `drive` (made from `volume`) before its cache is
/// written: the copy of the scan it replaces (`keep_scan`) becomes the
/// previous scan, replacing the one before. Only when that copy is of this
/// drive and volume. Otherwise the previous scan stays as it is: an older
/// scan of this disk (the comparison shows its date), or one of a disk that
/// had this letter before (never compared). The cache file is never taken:
/// by now it holds every live change since that scan. True when kept.
pub fn keep_previous(drive: &str, volume: Option<&str>) -> io::Result<bool> {
    keep_previous_in(&index_dir(), drive, volume)
}

fn keep_previous_in(dir: &Path, drive: &str, volume: Option<&str>) -> io::Result<bool> {
    let (copy, copy_tag) = (file_in(dir, drive, "scan.idx"), file_in(dir, drive, "scan.vol"));
    let tag = std::fs::read_to_string(&copy_tag).ok();
    if !same_volume(tag.as_deref(), volume) || !read_header(&copy).is_ok_and(|h| h.drive == drive) {
        return Ok(false);
    }
    let (prev, prev_tag) = (file_in(dir, drive, "prev.idx"), file_in(dir, drive, "prev.vol"));
    // The old pair stops counting first: cut off half-way, this leaves a
    // previous scan whose volume cannot be confirmed, which is never compared.
    remove_if_there(&prev_tag)?;
    // A rename: the copy is not the cache, so the cache stays put whatever happens here.
    if let Err(e) = std::fs::rename(&copy, &prev) {
        // Untagged, it would still be compared on a drive without a volume GUID.
        let _ = std::fs::remove_file(&prev);
        return Err(e);
    }
    let _ = std::fs::remove_file(&copy_tag);
    if let Some(v) = volume {
        std::fs::write(&prev_tag, v)?;
    }
    Ok(true)
}

/// Called right after a scan's cache was written, in the same save turn (so
/// no live change is in the file yet): keeps the copy of it that the next
/// scan compares with (`keep_previous`), tagged with `volume`. A hard link
/// where the file system has them (NTFS): it takes no room until the first
/// live save writes the cache anew, and as much as the cache from then on.
/// Elsewhere a copy. The caller checks that the disk has room for that.
pub fn keep_scan(drive: &str, volume: Option<&str>) -> io::Result<()> {
    keep_scan_in(&index_dir(), drive, volume)
}

fn keep_scan_in(dir: &Path, drive: &str, volume: Option<&str>) -> io::Result<()> {
    let (copy, copy_tag) = (file_in(dir, drive, "scan.idx"), file_in(dir, drive, "scan.vol"));
    remove_if_there(&copy_tag)?;
    if let Err(e) = link_or_copy(&file_in(dir, drive, "idx"), &copy) {
        // The copy of an earlier scan, or of another disk, would pass for this one.
        let _ = std::fs::remove_file(&copy);
        return Err(e);
    }
    if let Some(v) = volume {
        std::fs::write(&copy_tag, v)?;
    }
    Ok(())
}

/// Removes the copy of the current scan, so the next scan keeps no previous
/// one. True when there was one.
pub fn drop_scan_copy(drive: &str) -> bool {
    drop_pair(&index_dir(), drive, "scan")
}

/// Removes the previous scan. True when there was one.
pub fn drop_previous(drive: &str) -> bool {
    drop_pair(&index_dir(), drive, "prev")
}

/// Removes `<letter>.<name>.idx` and its tag (the tag first, as `keep_previous` does).
fn drop_pair(dir: &Path, drive: &str, name: &str) -> bool {
    let _ = std::fs::remove_file(file_in(dir, drive, &format!("{name}.vol")));
    std::fs::remove_file(file_in(dir, drive, &format!("{name}.idx"))).is_ok()
}

fn remove_if_there(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether a cache tagged `tag` (the contents of `<letter>.vol`, if any) was
/// made from `volume`. Stricter than `cache_matches_volume`: a drive without
/// a volume GUID (a network drive) only matches a cache that has no tag either.
fn same_volume(tag: Option<&str>, volume: Option<&str>) -> bool {
    match (tag, volume) {
        (Some(t), Some(v)) => t.trim() == v,
        (None, None) => true,
        _ => false,
    }
}

/// Makes `to` a file holding what `from` holds, leaving `from` as it is: a
/// hard link where the file system has them (NTFS), else a copy. Made under
/// a temp name first, so `to` is never there half-written.
fn link_or_copy(from: &Path, to: &Path) -> io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // `<letter>.scan.<pid>-<n>.tmp`: one `sweep_temp_files` clears if it is left behind.
    let tmp = to.with_extension(format!("{}-{seq}.tmp", std::process::id()));
    let made = std::fs::hard_link(from, &tmp).or_else(|_| std::fs::copy(from, &tmp).map(drop));
    made.and_then(|()| std::fs::rename(&tmp, to)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Caches used to live in `%LOCALAPPDATA%\fine-file`, which the uninstaller
/// does not know about. Moves them over once and removes the old folder.
pub fn migrate_old_cache() {
    migrate_from(&local_app_data().join("fine-file"), &data_dir().join("index"));
}

fn migrate_from(old_root: &Path, new: &Path) {
    let old = old_root.join("index");
    if !old.is_dir() {
        return;
    }
    let moved = !new.exists()
        && new.parent().is_some_and(|p| std::fs::create_dir_all(p).is_ok())
        && std::fs::rename(&old, new).is_ok();
    if moved {
        // The old helper's error files and temp files are of no use now.
        remove_known(new, &["err", "tmp"]);
    } else {
        remove_known(&old, &["idx", "err", "tmp"]);
        let _ = std::fs::remove_dir(&old);
    }
    let _ = std::fs::remove_dir(old_root);
}

/// Deletes temp files that `Index::save` left behind when the app was closed
/// mid-save (`C.<pid>-<n>.tmp`, as large as the index). Ones younger than ten
/// minutes are kept, in case another running copy is still writing them.
pub fn sweep_temp_files() {
    sweep_temp_in(&data_dir().join("index"), std::time::Duration::from_secs(600));
}

fn sweep_temp_in(dir: &Path, min_age: std::time::Duration) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let ours = name.len() > 6
            && name.as_bytes()[0].is_ascii_uppercase()
            && name.as_bytes()[1] == b'.'
            && name.ends_with(".tmp");
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|a| a >= min_age));
        if ours && old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Removes only the `<letter>.<ext>` files this app wrote.
fn remove_known(dir: &Path, exts: &[&str]) {
    for c in 'A'..='Z' {
        for ext in exts {
            let _ = std::fs::remove_file(dir.join(format!("{c}.{ext}")));
        }
    }
}

/// Accepts `C`, `c:`, `C:\` and returns `C:`.
pub fn normalize_drive(s: &str) -> Option<String> {
    let c = s.chars().next()?.to_ascii_uppercase();
    if !c.is_ascii_uppercase() {
        return None;
    }
    let rest = &s[1..];
    if !(rest.is_empty() || rest == ":" || rest == ":\\" || rest == ":/") {
        return None;
    }
    Some(format!("{c}:"))
}

pub enum HelperError {
    /// The user said no to the UAC prompt.
    Cancelled,
    /// The helper could not be started or gave no usable answer.
    Helper(String),
    /// The helper ran but could not read the MFT.
    Scan(String),
}

/// Helper replies: one status byte, then the index (`Index::save_to` bytes)
/// or a UTF-8 message.
const REPLY_OK: u8 = 0;
const REPLY_ERR: u8 = 1;

/// Reads the MFT in an elevated copy of this program (one UAC prompt) and
/// receives the index through a pipe. The helper writes no files; the caller
/// saves the cache as the normal user, in the normal user's profile.
pub fn scan_mft_elevated(drive: &str) -> Result<Index, HelperError> {
    let mut helper = win::run_elevated_piped(&format!("--scan-helper {drive}")).map_err(|e| match e {
        win::ElevateError::Cancelled => HelperError::Cancelled,
        win::ElevateError::Failed(e) => HelperError::Helper(e),
    })?;
    read_reply(&mut helper, drive).map_err(|e| match e {
        HelperError::Helper(msg) => match helper.exit_code() {
            Some(code) => HelperError::Helper(format!("{msg} (종료 코드 {code})")),
            None => HelperError::Helper(msg),
        },
        e => e,
    })
}

fn write_reply(w: &mut impl Write, result: &io::Result<Index>) -> io::Result<()> {
    match result {
        Ok(ix) => {
            w.write_all(&[REPLY_OK])?;
            ix.save_to(w)
        }
        Err(e) => {
            w.write_all(&[REPLY_ERR])?;
            w.write_all(e.to_string().as_bytes())?;
            w.flush()
        }
    }
}

fn read_reply(r: &mut impl Read, drive: &str) -> Result<Index, HelperError> {
    let mut status = [0u8];
    if r.read_exact(&mut status).is_err() {
        return Err(HelperError::Helper("도우미가 결과 없이 끝났어요".into()));
    }
    match status[0] {
        REPLY_OK => {
            let ix = Index::load_from(r).map_err(|e| HelperError::Helper(format!("결과를 읽지 못했어요: {e}")))?;
            if ix.drive != drive || ix.method != "mft" {
                return Err(HelperError::Helper("다른 드라이브의 결과가 왔어요".into()));
            }
            Ok(ix)
        }
        REPLY_ERR => {
            let mut msg = Vec::new();
            let _ = r.take(64 << 10).read_to_end(&mut msg);
            Err(HelperError::Scan(String::from_utf8_lossy(&msg).into_owned()))
        }
        _ => Err(HelperError::Helper("알 수 없는 응답이 왔어요".into())),
    }
}

/// Entry point of the elevated helper:
/// `fine-file.exe --scan-helper C: \\.\pipe\fine-file-<pid>-<16 hex>`.
/// It never touches the file system: the index, or why it failed, goes only
/// into the pipe the unelevated app created, and that app saves the cache.
pub fn run_helper(args: &[String]) -> i32 {
    let [drive, pipe] = args else { return 2 };
    let (Some(drive), Some(server)) = (normalize_drive(drive), win::helper_pipe_server(pipe)) else { return 2 };
    // Connect first: with nobody to report to, there is no point in scanning.
    let Ok(mut out) = win::open_pipe_client(pipe, server) else { return 3 };
    let result = scan_mft_guarded(&drive);
    match write_reply(&mut out, &result) {
        Ok(()) if result.is_ok() => 0,
        Ok(()) => 1,
        Err(_) => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{RawEntry, FLAG_DIR};

    fn index(drive: &str, method: &str) -> Index {
        let e = |id, parent, name: &str, flags, size| RawEntry { id, parent, name: name.into(), flags, size, alloc: size, modified: 0 };
        Index::build(drive, method, 7, vec![e(5, 5, ".", FLAG_DIR, 0), e(6, 5, "a.txt", 0, 42)], 5)
    }

    fn reply(result: io::Result<Index>) -> Vec<u8> {
        let mut out = Vec::new();
        write_reply(&mut out, &result).unwrap();
        out
    }

    #[test]
    fn helper_reply_round_trip() {
        let bytes = reply(Ok(index("C:", "mft")));
        let ix = read_reply(&mut bytes.as_slice(), "C:").ok().unwrap();
        assert_eq!(ix.nodes[0].size, 42);
        assert!(ix.find("C:\\a.txt").is_some());
        // A reply for another drive, or from a walk, is not taken.
        assert!(matches!(read_reply(&mut bytes.as_slice(), "D:"), Err(HelperError::Helper(_))));
        let walk = reply(Ok(index("C:", "walk")));
        assert!(matches!(read_reply(&mut walk.as_slice(), "C:"), Err(HelperError::Helper(_))));
        // A helper that died half-way, or never wrote.
        for n in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(matches!(read_reply(&mut &bytes[..n], "C:"), Err(HelperError::Helper(_))), "{n}");
        }
        assert!(matches!(read_reply(&mut &[9u8][..], "C:"), Err(HelperError::Helper(_))));
    }

    #[test]
    fn helper_error_reaches_the_app() {
        let bytes = reply(Err(io::Error::other("볼륨을 열 수 없어요")));
        match read_reply(&mut bytes.as_slice(), "C:") {
            Err(HelperError::Scan(msg)) => assert_eq!(msg, "볼륨을 열 수 없어요"),
            _ => panic!("expected a scan error"),
        }
    }

    #[test]
    fn panics_become_errors() {
        let err = guarded(|| panic!("bad run at {}", 3)).err().unwrap();
        assert!(err.to_string().contains("bad run at 3"), "{err}");
        let err = guarded(|| panic!("plain")).err().unwrap();
        assert!(err.to_string().contains("plain"), "{err}");
        assert!(guarded(|| Ok(index("C:", "mft"))).is_ok());
    }

    #[test]
    fn helper_refuses_anything_but_a_drive_and_its_pipe() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(run_helper(&args(&["C:"])), 2);
        assert_eq!(run_helper(&args(&["C:", r"C:\Users\me\AppData\Local\fine-file\index\C.idx"])), 2);
        assert_eq!(run_helper(&args(&["CC", r"\\.\pipe\fine-file-1-0123456789abcdef"])), 2);
        assert_eq!(run_helper(&args(&["C:", r"\\.\pipe\fine-file-1-0123456789abcdef", "x"])), 2);
        // Well-formed, but nobody is listening: it stops before scanning.
        assert_eq!(run_helper(&args(&["C:", r"\\.\pipe\fine-file-1-0123456789abcdef"])), 3);
    }

    #[test]
    fn helper_reports_through_the_pipe() {
        // Elevated, this would really read the whole C: MFT.
        if win::is_elevated() {
            return;
        }
        let pipe = win::tests::OwnPipe::new();
        let args = vec!["C:".to_string(), pipe.name.clone()];
        let helper = std::thread::spawn(move || run_helper(&args));
        let reply = read_reply(&mut pipe.accept(), "C:");
        // Without admin rights the volume cannot be opened, and the helper says so.
        assert_eq!(helper.join().unwrap(), 1);
        assert!(matches!(reply, Err(HelperError::Scan(msg)) if !msg.is_empty()));
    }

    #[test]
    fn sweep_removes_only_our_temp_files() {
        let dir = std::env::temp_dir().join(format!("ff-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = ["C.123-0.tmp", "C.idx", "notes.tmp", "D.idx.tmp", "C.scan.123-0.tmp", "C.scan.idx", "C.prev.idx", "C.prev.vol"];
        for f in files {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        // Too young: kept.
        sweep_temp_in(&dir, std::time::Duration::from_secs(3600));
        assert!(dir.join("C.123-0.tmp").exists());
        sweep_temp_in(&dir, std::time::Duration::ZERO);
        assert!(!dir.join("C.123-0.tmp").exists());
        assert!(!dir.join("D.idx.tmp").exists());
        // A link `keep_scan` left behind.
        assert!(!dir.join("C.scan.123-0.tmp").exists());
        for kept in ["C.idx", "C.scan.idx", "C.prev.idx", "C.prev.vol", "notes.tmp"] {
            assert!(dir.join(kept).exists(), "{kept}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The cache bytes of a scan of `drive` made at `at`, whose one file is `size` bytes.
    fn scanned_sized(drive: &str, at: i64, size: u64) -> Vec<u8> {
        let e = |id, parent, name: &str, flags, size| RawEntry { id, parent, name: name.into(), flags, size, alloc: size, modified: 0 };
        let mut ix = Index::build(drive, "mft", 7, vec![e(5, 5, ".", FLAG_DIR, 0), e(6, 5, "a.txt", 0, size)], 5);
        ix.scanned_at = at;
        ix.to_bytes()
    }

    fn scanned(drive: &str, at: i64) -> Vec<u8> {
        scanned_sized(drive, at, 42)
    }

    fn scanned_at(path: &Path) -> i64 {
        read_header(path).unwrap().scanned_at
    }

    /// (scan time, drive total) of a cache file.
    fn contents(path: &Path) -> (i64, u64) {
        let ix = Index::load(path).unwrap();
        (ix.scanned_at, ix.nodes[crate::index::ROOT as usize].size)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn no_temp_files(dir: &Path) {
        let left: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(left.iter().all(|f| !f.ends_with(".tmp")), "{left:?}");
    }

    #[test]
    fn in_step_notes_are_read_once() {
        let dir = temp_dir("in-step");
        write_in_step_in(&dir, "C:", 133_000_000_000_000_000);
        write_in_step_in(&dir, "E:", 5);
        std::fs::write(dir.join("F.live"), "not a time").unwrap();
        let notes = take_in_step_in(&dir);
        assert_eq!(notes, HashMap::from([("C:".to_string(), 133_000_000_000_000_000), ("E:".to_string(), 5)]));
        // Taken away, the bad one too: the next start finds none.
        assert!(take_in_step_in(&dir).is_empty());
        assert!(!dir.join("F.live").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn header_reads_what_the_index_writes() {
        let dir = temp_dir("header");
        let path = dir.join("D.idx");
        std::fs::write(&path, scanned("D:", 133_000_000_000_000_000)).unwrap();
        let h = read_header(&path).unwrap();
        assert_eq!((h.drive.as_str(), h.method.as_str(), h.scanned_at), ("D:", "mft", 133_000_000_000_000_000));
        // Another format, a cut-off file, a missing one.
        std::fs::write(&path, b"FFIDX\x00\x00\x02rest").unwrap();
        assert_eq!(read_header(&path).err().unwrap().kind(), io::ErrorKind::InvalidData);
        std::fs::write(&path, &scanned("D:", 1)[..20]).unwrap();
        assert!(read_header(&path).is_err());
        assert_eq!(read_header(&dir.join("E.idx")).err().unwrap().kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_scan_keeps_the_one_before() {
        use crate::index::write_cache;
        let dir = temp_dir("keep");
        let (cur, copy, prev) = (dir.join("C.idx"), dir.join("C.scan.idx"), dir.join("C.prev.idx"));
        let tag = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        // No scan yet, or a cache from before copies were kept: nothing to keep.
        assert!(!keep_previous_in(&dir, "C:", Some("vol-a")).unwrap());
        write_cache(&cur, &scanned("C:", 50)).unwrap();
        std::fs::write(dir.join("C.vol"), "vol-a").unwrap();
        assert!(!keep_previous_in(&dir, "C:", Some("vol-a")).unwrap());
        assert!(!prev.exists());

        // Monday's scan: its cache, and the copy of it.
        write_cache(&cur, &scanned_sized("C:", 100, 42)).unwrap();
        keep_scan_in(&dir, "C:", Some("vol-a")).unwrap();
        assert_eq!(tag("C.scan.vol").as_deref(), Some("vol-a"));
        // The week's live saves, renames and trashes rewrite the cache under
        // Monday's time; the copy stays what the scan found.
        write_cache(&cur, &scanned_sized("C:", 100, 5000)).unwrap();
        write_cache(&cur, &scanned_sized("C:", 100, 9000)).unwrap();
        assert_eq!((contents(&cur), contents(&copy)), ((100, 9000), (100, 42)));

        // Friday's scan compares with Monday's scan, not with the last save.
        assert!(keep_previous_in(&dir, "C:", Some("vol-a")).unwrap());
        assert_eq!(contents(&prev), (100, 42));
        assert_eq!(tag("C.prev.vol").as_deref(), Some("vol-a"));
        assert!(!copy.exists() && tag("C.scan.vol").is_none());
        // The cache is left alone until the new one is written (a failed save loses nothing).
        assert_eq!(contents(&cur), (100, 9000));
        write_cache(&cur, &scanned_sized("C:", 200, 7)).unwrap();
        keep_scan_in(&dir, "C:", Some("vol-a")).unwrap();
        write_cache(&cur, &scanned_sized("C:", 200, 8)).unwrap();
        assert_eq!((contents(&cur), contents(&copy), contents(&prev)), ((200, 8), (200, 7), (100, 42)));

        // The next scan replaces the previous one.
        assert!(keep_previous_in(&dir, "C:", Some("vol-a")).unwrap());
        assert_eq!(contents(&prev), (200, 7));
        // One whose save failed made no copy: the scan before stays the previous one.
        assert!(!keep_previous_in(&dir, "C:", Some("vol-a")).unwrap());
        assert_eq!(contents(&prev), (200, 7));
        no_temp_files(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sync_keeps_the_scans_to_compare() {
        use crate::index::write_cache;
        let dir = temp_dir("keep-sync");
        let (cur, copy, prev) = (dir.join("C.idx"), dir.join("C.scan.idx"), dir.join("C.prev.idx"));
        // Monday's scan, then Wednesday's: Monday's is the previous one.
        write_cache(&cur, &scanned_sized("C:", 100, 1)).unwrap();
        keep_scan_in(&dir, "C:", Some("vol-a")).unwrap();
        assert!(keep_previous_in(&dir, "C:", Some("vol-a")).unwrap());
        write_cache(&cur, &scanned_sized("C:", 200, 2)).unwrap();
        keep_scan_in(&dir, "C:", Some("vol-a")).unwrap();
        // Thursday's sync: a new cache with its tag, and nothing else moves.
        write_cache(&cur, &scanned_sized("C:", 300, 3)).unwrap();
        std::fs::remove_file(dir.join("C.vol")).ok();
        tag_in(&dir, "C:", Some("vol-a"));
        assert_eq!((contents(&cur), contents(&copy), contents(&prev)), ((300, 3), (200, 2), (100, 1)));
        assert_eq!(std::fs::read_to_string(dir.join("C.vol")).unwrap(), "vol-a");
        // Friday's scan compares with Wednesday's scan, not with Thursday's sync.
        assert!(keep_previous_in(&dir, "C:", Some("vol-a")).unwrap());
        assert_eq!(contents(&prev), (200, 2));
        no_temp_files(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn another_volume_is_not_kept() {
        let dir = temp_dir("keep-vol");
        let (copy, prev) = (dir.join("E.scan.idx"), dir.join("E.prev.idx"));
        std::fs::write(&copy, scanned("E:", 100)).unwrap();
        std::fs::write(dir.join("E.scan.vol"), "vol-a\n").unwrap();
        std::fs::write(&prev, scanned("E:", 50)).unwrap();
        std::fs::write(dir.join("E.prev.vol"), "vol-a").unwrap();
        // Another disk with this letter now, or one that cannot be told apart.
        assert!(!keep_previous_in(&dir, "E:", Some("vol-b")).unwrap());
        assert!(!keep_previous_in(&dir, "E:", None).unwrap());
        assert_eq!(scanned_at(&prev), 50);
        assert_eq!(std::fs::read_to_string(dir.join("E.prev.vol")).unwrap(), "vol-a");
        // A copy with no tag is not known to be of this volume.
        std::fs::remove_file(dir.join("E.scan.vol")).unwrap();
        assert!(!keep_previous_in(&dir, "E:", Some("vol-a")).unwrap());
        // A file that is not a scan of this drive.
        std::fs::write(dir.join("E.scan.vol"), "vol-a").unwrap();
        std::fs::write(&copy, scanned("F:", 100)).unwrap();
        assert!(!keep_previous_in(&dir, "E:", Some("vol-a")).unwrap());
        std::fs::write(&copy, b"not an index").unwrap();
        assert!(!keep_previous_in(&dir, "E:", Some("vol-a")).unwrap());
        assert_eq!(scanned_at(&prev), 50);

        // A drive without a volume GUID (network) keeps its own untagged scans,
        // and its previous scan has no tag either.
        std::fs::remove_file(dir.join("E.scan.vol")).unwrap();
        std::fs::write(&copy, scanned("E:", 300)).unwrap();
        assert!(keep_previous_in(&dir, "E:", None).unwrap());
        assert_eq!(scanned_at(&prev), 300);
        assert!(!dir.join("E.prev.vol").exists());
        // Its copy is untagged too, and replaces a tagged one of another disk.
        std::fs::write(dir.join("E.idx"), scanned("E:", 400)).unwrap();
        std::fs::write(dir.join("E.scan.vol"), "vol-a").unwrap();
        keep_scan_in(&dir, "E:", None).unwrap();
        assert!(!dir.join("E.scan.vol").exists());
        assert_eq!(scanned_at(&copy), 400);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_copy_that_cannot_be_made_is_not_left_half_there() {
        let dir = temp_dir("keep-fail");
        // An older copy, and no cache to copy from now.
        std::fs::write(dir.join("C.scan.idx"), scanned("C:", 100)).unwrap();
        std::fs::write(dir.join("C.scan.vol"), "vol-a").unwrap();
        assert!(keep_scan_in(&dir, "C:", Some("vol-a")).is_err());
        assert!(!dir.join("C.scan.idx").exists() && !dir.join("C.scan.vol").exists());
        no_temp_files(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropping_a_kept_scan_takes_its_tag() {
        let dir = temp_dir("drop");
        assert!(!drop_pair(&dir, "C:", "prev"));
        for f in ["C.idx", "C.vol", "C.prev.idx", "C.prev.vol", "C.scan.idx", "C.scan.vol"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        assert!(drop_pair(&dir, "C:", "prev"));
        assert!(!dir.join("C.prev.idx").exists() && !dir.join("C.prev.vol").exists());
        assert!(drop_pair(&dir, "C:", "scan"));
        assert!(!dir.join("C.scan.idx").exists() && !dir.join("C.scan.vol").exists());
        // The cache is never one of them.
        assert!(dir.join("C.idx").exists() && dir.join("C.vol").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn previous_volume_tag_is_checked_like_the_cache() {
        let dir = temp_dir("tag");
        let tag = dir.join("C.prev.vol");
        assert!(!tag_matches(&tag, Some("vol-a")));
        assert!(tag_matches(&tag, None));
        std::fs::write(&tag, "vol-a\r\n").unwrap();
        assert!(tag_matches(&tag, Some("vol-a")));
        assert!(!tag_matches(&tag, Some("vol-b")));
        assert!(prev_cache_path("D:").ends_with(format!("{APP_ID}\\index\\D.prev.idx")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_lives_under_the_bundle_identifier() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], APP_ID);
        assert!(cache_path("D:").ends_with(format!("{APP_ID}\\index\\D.idx")));
    }

    #[test]
    fn old_cache_moves_once() {
        let base = std::env::temp_dir().join(format!("ff-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (old_root, new) = (base.join("fine-file"), base.join(APP_ID).join("index"));
        std::fs::create_dir_all(old_root.join("index")).unwrap();
        std::fs::write(old_root.join("index").join("C.idx"), "c").unwrap();
        std::fs::write(old_root.join("index").join("C.err"), "e").unwrap();
        migrate_from(&old_root, &new);
        assert_eq!(std::fs::read_to_string(new.join("C.idx")).unwrap(), "c");
        assert!(!new.join("C.err").exists());
        assert!(!old_root.exists());

        // With a new cache already there, the old files are just dropped.
        std::fs::create_dir_all(old_root.join("index")).unwrap();
        std::fs::write(old_root.join("index").join("C.idx"), "old").unwrap();
        migrate_from(&old_root, &new);
        assert_eq!(std::fs::read_to_string(new.join("C.idx")).unwrap(), "c");
        assert!(!old_root.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}

/// Real-disk checks, run by hand:
/// `FF_DRIVE=D: cargo test --release real_ -- --ignored --nocapture`
/// (the MFT one needs an elevated terminal).
#[cfg(test)]
mod real {
    use super::*;

    fn report(ix: &Index) {
        let root = crate::index::ROOT;
        println!(
            "{} via {}: {} entries, {:.1} GB, {} ms",
            ix.drive,
            ix.method,
            ix.nodes.len(),
            ix.nodes[root as usize].size as f64 / 1e9,
            ix.scan_ms
        );
        let mut top: Vec<_> = ix.list(root);
        top.sort_by_key(|e| std::cmp::Reverse(e.size));
        for e in top.iter().take(8) {
            println!("  {:>8.2} GB  {}", e.size as f64 / 1e9, e.name);
        }
        let t = Instant::now();
        let (n, _) = ix.search("*.mp4", None, 100);
        println!("  search *.mp4: {n} hits in {} ms", t.elapsed().as_millis());
    }

    #[test]
    #[ignore]
    fn real_walk() {
        let drive = std::env::var("FF_DRIVE").unwrap_or("C:".into());
        report(&scan_walk(&drive).expect("walk scan"));
    }

    #[test]
    #[ignore]
    fn real_mft() {
        let drive = std::env::var("FF_DRIVE").unwrap_or("C:".into());
        report(&scan_mft(&drive).expect("MFT scan (needs admin)"));
    }
}
