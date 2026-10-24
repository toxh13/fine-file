//! Hard links (v0.10.3): which duplicates may be merged into one file under
//! two names (the link rule), the ledger of the merges done, and the files on
//! disk that have more than one name (`hardlinks_find`), whose names ops.rs's
//! `start_unlink` makes files of their own again.
//!
//! The rule is one: dups.rs gives each file of its results what it says
//! (DupFile.link_block, link_optin), and ops.rs's merge (`start_dup_link`)
//! asks it again for both files of every pair, right before the pair is
//! merged, and turns down what it does not allow. A hard link is one file: a
//! program that rewrites it in place, changes its attributes or permissions,
//! or checks and repairs its own folder (a game launcher does) changes it
//! under every name. So:
//!
//! - never, whatever is asked: a file of a game, a launcher, a mod pack or a
//!   server pack, an instance, a save or a backup; of Windows; of a code
//!   project (`.git`, `node_modules`, a venv); of a cache; a file programs
//!   rewrite in place (settings, saves, databases, logs: `classify::LinkKind`);
//!   and two files whose owners, permissions (DACL) or read-only, hidden and
//!   system attributes differ, or that are on two volumes;
//! - by default: two files in the user's own folders (Downloads, Documents,
//!   Desktop, Pictures, Videos, Music, cloud drives) that are written once and
//!   then only read: pictures, video, sound, archives, disk images, setup
//!   files, PDFs and model files;
//! - only when the user asks (`allow_programs`): a model's weights or a zip,
//!   7z or iso archive in a program's or app data's folder, the same big model
//!   kept by two programs. Everything else there, never. Nor, even then, two
//!   such files whose permissions differ: a Store app's package folder
//!   (`Packages\<app>`) passes down an ACE for the app's own SID that a plain
//!   app data folder does not, and one name would lose it
//!   (`LB_SECURITY_APPS`). 0.8.2's own case, the same model in
//!   `AppData\Local\VideoTranslator\models` and in the Codex package's copy of
//!   that folder, is such a pair.
//!
//! Where a file is goes by its path (`classify::link_place`), as written and
//! where it really is through any junction on the way (the stricter of the
//! two), and by what the folders it is in hold below the user's (or the
//! program's) own folder (`Markers`): a server pack unpacked in Downloads has
//! a `server.properties` or a `mods` folder, a code project a `.git`.
//!
//! Every merge done from 0.10.3 on is written down in the ledger
//! (`link-ledger.jsonl` in the app's data folder, a line per merge, appended
//! all the way to the disk; rewritten atomically when it is cut down to its
//! newest LEDGER_MOST, in half of LEDGER_BYTES). Merges before that were
//! never written down.

use crate::classify::{self, LinkKind, LinkPlace};
use crate::{dups, ops, win};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::{Mutex, MutexGuard, PoisonError};
use windows_sys::Win32::Foundation::{GetLastError, LocalFree, ERROR_MORE_DATA, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{GetLengthSid, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileNameW, FindNextFileNameW, GetFileAttributesW, GetVolumePathNameW, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, INVALID_FILE_ATTRIBUTES, READ_CONTROL,
};

// Why a file is not merged, as the window shows it (DupFile.link_block, the
// merge job's errors).
pub(crate) const LB_SYSTEM: &str = "Windows 시스템 폴더의 파일은 합치지 않아요";
pub(crate) const LB_GAME: &str = "게임·모드팩 폴더 안 파일이라 합치지 않아요 (게임이 파일을 고치거나 다시 받아요)";
pub(crate) const LB_GAME_BACKUP: &str = "게임·모드팩 백업 안 파일이라 합치지 않아요 (백업은 원본과 따로 있어야 해요)";
pub(crate) const LB_BACKUP: &str = "백업 폴더 안 파일이라 합치지 않아요 (백업은 원본과 따로 있어야 해요)";
pub(crate) const LB_DEV: &str = "개발 폴더(코드·패키지) 안 파일이라 합치지 않아요 (개발 도구가 파일을 고치거나 다시 만들어요)";
pub(crate) const LB_CACHE: &str = "캐시·임시 폴더 안 파일이라 합치지 않아요 (프로그램이 지우거나 다시 만들어요)";
pub(crate) const LB_REWRITTEN: &str = "설정·저장처럼 프로그램이 고쳐 쓰는 파일이라 합치지 않아요";
pub(crate) const LB_DOCUMENT: &str = "문서처럼 고쳐 쓰는 파일이라 합치지 않아요";
pub(crate) const LB_KIND: &str = "이런 종류의 파일은 합치지 않아요 (사진·영상·음악·압축·설치 파일·PDF·모델 파일만 합쳐요)";
pub(crate) const LB_PLACE: &str = "내 폴더(다운로드·문서·바탕 화면·사진·동영상·음악·원드라이브) 밖의 파일이라 합치지 않아요";
pub(crate) const LB_PROGRAM: &str = "프로그램·앱 데이터 폴더 안 파일이라 합치지 않아요 (프로그램이 파일을 고치거나 다시 받아요)";
pub(crate) const LB_PROGRAM_OPTIN: &str =
    "프로그램 폴더 안 파일이라 \"프로그램 폴더 안의 큰 모델·압축 파일도 합치기\"를 켜야 합쳐요";
pub(crate) const LB_SECURITY: &str = "권한이나 소유자가 달라서 합치지 않아요";
/// LB_SECURITY of a file the user may ask for (`Verdict::OptIn`): asking does not change it.
pub(crate) const LB_SECURITY_APPS: &str =
    "권한이 달라서 \"프로그램 폴더 안의 큰 모델·압축 파일도 합치기\"를 켜도 합치지 않아요 (Microsoft Store 앱 폴더처럼 앱마다 권한이 따로 있어요)";
pub(crate) const LB_NO_SECURITY: &str = "권한을 확인할 수 없어서 합치지 않아요";
pub(crate) const LB_ATTRS: &str = "읽기 전용·숨김·시스템 속성이 달라서 합치지 않아요";
pub(crate) const LB_OTHER_DRIVE: &str = "다른 드라이브의 파일과는 합칠 수 없어요 (하드 링크는 한 드라이브 안에서만 만들 수 있어요)";

const NO_ROOT: &str = "폴더를 읽을 수 없어요";
const BAD_ROOT: &str = "경로가 올바르지 않아요";

/// Files `hardlinks_find` looks at, at most.
const FIND_MOST: usize = 300_000;
/// Threads that open the files `hardlinks_find` found.
const FIND_THREADS: usize = 4;
const LEDGER_FILE: &str = "link-ledger.jsonl";
/// Merges the ledger keeps (the newest); it is cut down to that, and to half
/// of LEDGER_BYTES, once it has grown past LEDGER_BYTES.
const LEDGER_MOST: usize = 20_000;
const LEDGER_BYTES: u64 = 8 << 20;

/// The attributes two names of one file always share, which a merge would
/// change at one of them were they not the same.
const SHARED_ATTRS: u32 = FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- the rule -----------------------------------------------------------------

/// What the rule says of one file, by where it is and what it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Verdict {
    Allowed,
    /// Only when the user asks (`allow_programs`): a big model or archive in
    /// a program's folder. Why it is not merged otherwise.
    OptIn(&'static str),
    /// Never: why.
    Blocked(&'static str),
}

impl Verdict {
    /// Why a merge with this file is turned down, if it is (`allow_programs`:
    /// the user asked for big models and archives in programs' folders too).
    pub(crate) fn refusal(self, allow_programs: bool) -> Option<&'static str> {
        match self {
            Verdict::Allowed => None,
            Verdict::OptIn(_) if allow_programs => None,
            Verdict::OptIn(why) | Verdict::Blocked(why) => Some(why),
        }
    }
}

/// What the rule says of the file at `path`, of `size` bytes: as written and
/// where it really is (`verdict_through`).
pub(crate) fn verdict(path: &Path, size: u64, markers: &Markers) -> Verdict {
    verdict_through(&path.to_string_lossy(), path, size, markers, ops::shown)
}

/// `verdict_as` of the file at `path` judged as at `seen`, and again where it
/// really is, with every junction or mount point on the way resolved (judged
/// as `seen_real` says it is seen: the app shows it as it is): the stricter of
/// the two, as `ops::in_windows` looks at both. A folder in Downloads that
/// leads into a game's is the game's. One that cannot be resolved is judged as
/// written (the merge opens it next, and does not get far).
pub(crate) fn verdict_through(seen: &str, path: &Path, size: u64, markers: &Markers, seen_real: impl Fn(&Path) -> String) -> Verdict {
    let written = verdict_as(seen, path, size, markers);
    if matches!(written, Verdict::Blocked(_)) {
        return written;
    }
    // `\\?\C:\...`, or `\\?\Volume{...}\...` for a volume without a letter, which is no folder of the user's.
    let Ok(real) = std::fs::canonicalize(win::verbatim(path)) else { return written };
    match (written, verdict_as(&seen_real(&real), &real, size, markers)) {
        (_, Verdict::Blocked(why)) => Verdict::Blocked(why),
        (Verdict::OptIn(why), _) | (_, Verdict::OptIn(why)) => Verdict::OptIn(why),
        _ => Verdict::Allowed,
    }
}

/// `verdict` of the file at `real`, judged as if it were at `seen` (the same
/// path for the app; tests take their folder for one in Downloads). What the
/// folders hold (`Markers`) is looked at in `real`'s.
pub(crate) fn verdict_as(seen: &str, real: &Path, size: u64, markers: &Markers) -> Verdict {
    let name = seen.rsplit(['\\', '/']).next().unwrap_or_default();
    let (below, user) = match classify::link_place(seen, name) {
        LinkPlace::System => return Verdict::Blocked(LB_SYSTEM),
        LinkPlace::Game { backup } => return Verdict::Blocked(if backup { LB_GAME_BACKUP } else { LB_GAME }),
        LinkPlace::Dev => return Verdict::Blocked(LB_DEV),
        LinkPlace::Cache => return Verdict::Blocked(LB_CACHE),
        LinkPlace::Backup => return Verdict::Blocked(LB_BACKUP),
        LinkPlace::Elsewhere => return Verdict::Blocked(LB_PLACE),
        LinkPlace::Program { below } => (below, false),
        LinkPlace::User { below } => (below, true),
    };
    let said = match (classify::link_kind(name, size), user) {
        (LinkKind::Rewritten, _) => return Verdict::Blocked(LB_REWRITTEN),
        (LinkKind::Bulky | LinkKind::Settled, true) => Verdict::Allowed,
        (LinkKind::Bulky, false) => Verdict::OptIn(LB_PROGRAM_OPTIN),
        (_, false) => return Verdict::Blocked(LB_PROGRAM),
        (LinkKind::Document, true) => return Verdict::Blocked(LB_DOCUMENT),
        (LinkKind::Other, true) => return Verdict::Blocked(LB_KIND),
    };
    match markers.tree(real, below) {
        Some(why) => Verdict::Blocked(why),
        None => said,
    }
}

/// Entries whose presence in a folder (below the user's own folder or a
/// program's) makes it a game's or a mod pack's (a server pack unpacked in
/// Downloads, a game run from the Desktop) or a code project's.
const MARKERS: &[(&str, &str)] = &[
    ("server.properties", LB_GAME),
    ("eula.txt", LB_GAME),
    ("user_jvm_args.txt", LB_GAME),
    ("mods", LB_GAME),
    ("minecraftinstance.json", LB_GAME),
    ("instance.cfg", LB_GAME),
    ("mmc-pack.json", LB_GAME),
    ("modrinth.index.json", LB_GAME),
    ("level.dat", LB_GAME),
    ("steam_api64.dll", LB_GAME),
    ("steam_api.dll", LB_GAME),
    ("unityplayer.dll", LB_GAME),
    ("gameassembly.dll", LB_GAME),
    (".git", LB_DEV),
    (".hg", LB_DEV),
    (".svn", LB_DEV),
    ("pyvenv.cfg", LB_DEV),
    ("package.json", LB_DEV),
    ("cargo.toml", LB_DEV),
];

/// What folders hold, as far as the rule goes (`MARKERS`), each looked at
/// once. One per search or merge: folders change.
#[derive(Default)]
pub(crate) struct Markers(Mutex<HashMap<String, Option<&'static str>>>);

impl Markers {
    /// Why the `below` nearest folders the file `real` is in keep it from
    /// being merged, if they do.
    fn tree(&self, real: &Path, below: usize) -> Option<&'static str> {
        real.ancestors().skip(1).take(below).find_map(|dir| self.of(dir))
    }

    /// Whether one of the `below` nearest folders the file `real` is in holds
    /// what a game's or a pack's folder does.
    fn game(&self, real: &Path, below: usize) -> bool {
        real.ancestors().skip(1).take(below).any(|dir| self.of(dir) == Some(LB_GAME))
    }

    fn of(&self, dir: &Path) -> Option<&'static str> {
        let key = dir.to_string_lossy().to_lowercase();
        if let Some(&known) = lock(&self.0).get(&key) {
            return known;
        }
        // Asked of the folder's own entry, which opens nothing.
        let there = |name: &str| unsafe { GetFileAttributesW(win::wide_path(&dir.join(name)).as_ptr()) } != INVALID_FILE_ATTRIBUTES;
        let found = MARKERS.iter().find(|(name, _)| there(name)).map(|&(_, why)| why);
        lock(&self.0).insert(key, found);
        found
    }
}

/// Whether the file at `path` is a game's, a launcher's, a mod pack's or a
/// server pack's, by its folders' names (`classify::link_place`) or by what
/// they hold below the user's or a program's own folder (`Markers`): one
/// unpacked in Downloads too, which `classify` alone takes for the user's.
/// The duplicate finder calls it "game" (DupFile.category), which automatic
/// picks leave alone.
pub(crate) fn in_game(path: &Path, markers: &Markers) -> bool {
    in_game_as(&path.to_string_lossy(), path, markers)
}

/// `in_game` of the file at `real`, judged as if it were at `seen`.
fn in_game_as(seen: &str, real: &Path, markers: &Markers) -> bool {
    let name = seen.rsplit(['\\', '/']).next().unwrap_or_default();
    match classify::link_place(seen, name) {
        LinkPlace::Game { .. } => true,
        LinkPlace::User { below } | LinkPlace::Program { below } => markers.game(real, below),
        _ => false,
    }
}

/// What two names of one file always share: the volume, the read-only,
/// hidden and system attributes, the owner and the DACL. A copy merged into
/// a file with others would take them on.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Sharing {
    volume: u64,
    attrs: u32,
    /// The owner's SID as it is stored; empty for none.
    owner: Vec<u8>,
    /// The DACL as it is stored; None for none (everyone may do anything).
    dacl: Option<Vec<u8>>,
}

/// What the file open as `file` (with READ_CONTROL) shares with its other
/// names; None when its id or its security cannot be read.
pub(crate) fn sharing_of(file: &File) -> Option<Sharing> {
    let facts = dups::facts(file)?;
    let volume = facts.key?.0;
    let (mut owner, mut dacl, mut sd): (PSID, *mut ACL, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut(), null_mut());
    let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let h = file.as_raw_handle() as HANDLE;
    if unsafe { GetSecurityInfo(h, SE_FILE_OBJECT, what, &mut owner, null_mut(), &mut dacl, null_mut(), &mut sd) } != ERROR_SUCCESS {
        return None;
    }
    // SAFETY: both point into `sd`, let go of only below.
    let bytes = |p: *const u8, len: usize| unsafe { std::slice::from_raw_parts(p, len) }.to_vec();
    let owner = if owner.is_null() { Vec::new() } else { bytes(owner.cast(), unsafe { GetLengthSid(owner) } as usize) };
    let dacl = (!dacl.is_null()).then(|| bytes(dacl.cast(), unsafe { (*dacl).AclSize } as usize));
    unsafe { LocalFree(sd) };
    Some(Sharing { volume, attrs: facts.attrs & SHARED_ATTRS, owner, dacl })
}

/// `sharing_of` the file at `path`, opened only to read its attributes and
/// security (a link there as the link).
pub(crate) fn sharing_at(path: &Path) -> Option<Sharing> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(win::verbatim(path))
        .ok()?;
    sharing_of(&file)
}

/// Why two files may not become one, if so: once merged, one of the two
/// names would have other attributes or permissions than it has now.
pub(crate) fn pair_refusal(a: &Sharing, b: &Sharing) -> Option<&'static str> {
    if a.volume != b.volume {
        Some(LB_OTHER_DRIVE)
    } else if a.attrs != b.attrs {
        Some(LB_ATTRS)
    } else if a.owner != b.owner || a.dacl != b.dacl {
        Some(LB_SECURITY)
    } else {
        None
    }
}

/// `why` (a `pair_refusal`) as told of a file the rule says `mine` of: one
/// the user may ask for (a model in a program's folder) is told that asking
/// does not help when the permissions differ (LB_SECURITY_APPS).
pub(crate) fn security_why(why: &'static str, mine: Verdict) -> &'static str {
    if why == LB_SECURITY && matches!(mine, Verdict::OptIn(_)) { LB_SECURITY_APPS } else { why }
}

/// DupFile.link_block and link_optin for each of the equal files `paths`
/// (`size` bytes each) of one group the duplicate finder found: the rule
/// for each file, and a file that may be merged with none of the others
/// that may be (their owners, permissions, attributes or volumes differ
/// from its own) is blocked for that. A pair that differs while each file
/// has another to go with is turned down by the merge itself.
pub(crate) fn judge_group(paths: &[&str], size: u64, markers: &Markers) -> Vec<(Option<&'static str>, bool)> {
    let verdicts: Vec<Verdict> = paths.iter().map(|p| verdict(Path::new(p), size, markers)).collect();
    // Only what may be merged at all is opened.
    let shares: Vec<Option<Sharing>> =
        paths.iter().zip(&verdicts).map(|(p, v)| (!matches!(v, Verdict::Blocked(_))).then(|| sharing_at(Path::new(p))).flatten()).collect();
    judged(&verdicts, &shares)
}

/// `judge_group` from what the rule says of each file and what each shares
/// with its other names (None: it could not be read, which the merge turns
/// down itself).
fn judged(verdicts: &[Verdict], shares: &[Option<Sharing>]) -> Vec<(Option<&'static str>, bool)> {
    let open = |j: usize| !matches!(verdicts[j], Verdict::Blocked(_));
    (0..verdicts.len())
        .map(|i| {
            let mine = match verdicts[i] {
                Verdict::Blocked(why) => return (Some(why), false),
                v => v,
            };
            let refusals: Vec<Option<&'static str>> = (0..verdicts.len())
                .filter(|&j| j != i && open(j))
                .filter_map(|j| Some(pair_refusal(shares[i].as_ref()?, shares[j].as_ref()?)))
                .collect();
            match refusals.first() {
                Some(&Some(why)) if refusals.iter().all(Option::is_some) => (Some(security_why(why, mine)), false),
                _ => match mine {
                    Verdict::OptIn(why) => (Some(why), true),
                    _ => (None, false),
                },
            }
        })
        .collect()
}

// ---- the ledger ---------------------------------------------------------------

/// One merge done: `linked` became a name of the file at `kept`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Merge {
    pub(crate) kept: String,
    pub(crate) linked: String,
    pub(crate) size: u64,
    /// FILETIME
    pub(crate) at: i64,
}

/// A merge as `link_ledger` gives it.
#[derive(Serialize)]
pub struct LinkRecord {
    pub kept: String,
    pub linked: String,
    pub size: u64,
    /// FILETIME
    pub at: i64,
    /// The two names are one file still.
    pub still_linked: bool,
}

/// Where merges are written down (tests keep their own).
pub(crate) struct Ledger {
    path: PathBuf,
    most: usize,
    bytes: u64,
}

impl Ledger {
    pub(crate) fn app() -> Ledger {
        Ledger::at(crate::scan::data_dir().join(LEDGER_FILE))
    }

    /// The ledger next to the folder `dir` (ops.rs: its book of merges running).
    pub(crate) fn beside(dir: &Path) -> Ledger {
        Ledger::at(dir.with_file_name(LEDGER_FILE))
    }

    fn at(path: PathBuf) -> Ledger {
        Ledger { path, most: LEDGER_MOST, bytes: LEDGER_BYTES }
    }

    /// Writes `merge` down at the end, all the way to the disk. A line cut
    /// short (a crash while it was written) is ended first, so it costs only
    /// itself. Past LEDGER_BYTES, only the newest LEDGER_MOST stay, in half of
    /// LEDGER_BYTES at most: the next cut is that far off again, also when
    /// long names make LEDGER_MOST lines more than LEDGER_BYTES (else every
    /// merge from then on would write the whole ledger again).
    pub(crate) fn add(&self, merge: &Merge) -> io::Result<()> {
        let _turn = turn();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new().read(true).append(true).create(true).open(&self.path)?;
        let len = file.metadata()?.len();
        let mut line = Vec::new();
        if len > 0 {
            let mut last = [0u8];
            file.seek(SeekFrom::Start(len - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.push(b'\n');
            }
        }
        serde_json::to_writer(&mut line, merge)?;
        line.push(b'\n');
        // One write: an append-only handle puts it at the end in one piece.
        file.write_all(&line)?;
        file.sync_all()?;
        let grown = file.metadata()?.len() > self.bytes;
        drop(file);
        if grown { self.rewrite(|_| false, self.bytes / 2) } else { Ok(()) }
    }

    /// Every merge written down, oldest first; a line that cannot be read is
    /// left out.
    pub(crate) fn read(&self) -> Vec<Merge> {
        let Ok(bytes) = std::fs::read(&self.path) else { return Vec::new() };
        bytes.split(|&b| b == b'\n').filter_map(|line| serde_json::from_slice(line).ok()).collect()
    }

    /// Takes away the merges `gone` says no longer hold.
    pub(crate) fn forget(&self, gone: impl Fn(&Merge) -> bool) -> io::Result<()> {
        let _turn = turn();
        self.rewrite(gone, self.bytes)
    }

    /// Writes the ledger again without what `gone` says, and with only the
    /// newest `most` that fit in `room` bytes (the newest always):
    /// atomically (temp file, flush, rename).
    fn rewrite(&self, gone: impl Fn(&Merge) -> bool, room: u64) -> io::Result<()> {
        let mut all = self.read();
        all.retain(|m| !gone(m));
        let mut kept: Vec<Vec<u8>> = Vec::new();
        let mut size = 0u64;
        for m in all.iter().rev().take(self.most) {
            let mut line = serde_json::to_vec(m)?;
            line.push(b'\n');
            if !kept.is_empty() && size + line.len() as u64 > room {
                break;
            }
            size += line.len() as u64;
            kept.push(line);
        }
        let bytes: Vec<u8> = kept.into_iter().rev().flatten().collect();
        crate::index::write_cache(&self.path, &bytes)
    }
}

/// Changes to a ledger take turns within the app.
fn turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    lock(&TURN)
}

/// Whether the two names of a merge are one file still.
pub(crate) fn still_linked(m: &Merge) -> bool {
    match (ops::key_at(Path::new(&m.kept)), ops::key_at(Path::new(&m.linked))) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Every merge written down, newest first, as it is on disk now.
#[tauri::command]
pub async fn link_ledger() -> Result<Vec<LinkRecord>, String> {
    tauri::async_runtime::spawn_blocking(|| records(&Ledger::app())).await.map_err(|e| e.to_string())
}

fn records(ledger: &Ledger) -> Vec<LinkRecord> {
    let mut all = ledger.read();
    all.reverse();
    all.into_iter()
        .map(|m| {
            let still_linked = still_linked(&m);
            LinkRecord { kept: m.kept, linked: m.linked, size: m.size, at: m.at, still_linked }
        })
        .collect()
}

// ---- files with more than one name ----------------------------------------------

/// One file with more than one name, as `hardlinks_find` found it.
#[derive(Serialize, Debug)]
pub struct HardlinkGroup {
    /// Which file it is (volume and file id), within one result.
    pub id: String,
    pub size: u64,
    /// FILETIME
    pub modified: i64,
    /// Every name of the file, the drive in front, also those outside the root.
    pub paths: Vec<String>,
    /// Per name: in the folder searched.
    pub inside: Vec<bool>,
    /// The ledger has a merge that made one of the names.
    pub by_fine_file: bool,
    /// A name is in Windows's own folders (WinSxS and the like): not to be
    /// made a file of its own (`start_unlink` turns it down).
    pub protected: bool,
}

#[derive(Serialize, Debug)]
pub struct HardlinkScan {
    pub root: String,
    /// Files looked at.
    pub scanned: u64,
    /// It stopped at FIND_MOST files: there may be more.
    pub truncated: bool,
    /// Biggest first.
    pub groups: Vec<HardlinkGroup>,
}

/// Walks `root` on disk (not the index; links to folders are not followed)
/// for files with more than one name (hard link), and lists all the names
/// of each. Looks at FIND_MOST files at most.
#[tauri::command]
pub async fn hardlinks_find(root: String) -> Result<HardlinkScan, String> {
    tauri::async_runtime::spawn_blocking(move || find_links(Path::new(&root), FIND_MOST, &Ledger::app()))
        .await
        .map_err(|e| e.to_string())?
}

fn find_links(root: &Path, most: usize, ledger: &Ledger) -> Result<HardlinkScan, String> {
    if !root.is_absolute() {
        return Err(BAD_ROOT.into());
    }
    let unreadable = |e: io::Error| format!("{NO_ROOT}: {}", ops::reason(&e));
    let meta = std::fs::symlink_metadata(win::verbatim(root)).map_err(unreadable)?;
    let mut files = Vec::new();
    let mut truncated = false;
    if meta.is_file() {
        files.push(root.to_path_buf());
    } else {
        // The folders found below keep the `\\?\` of their listing.
        let mut stack = vec![win::verbatim(root)];
        let mut top = true;
        'walk: while let Some(dir) = stack.pop() {
            // The root's own listing must be read; a folder below that cannot be is left out.
            let read = match std::fs::read_dir(&dir) {
                Ok(read) => read,
                Err(e) if top => return Err(unreadable(e)),
                Err(_) => continue,
            };
            top = false;
            for entry in read.flatten() {
                // On Windows this comes from the listing itself; a junction or symbolic link is one.
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_symlink() {
                    continue;
                }
                if kind.is_dir() {
                    stack.push(entry.path());
                } else if files.len() >= most {
                    truncated = true;
                    break 'walk;
                } else {
                    files.push(entry.path());
                }
            }
        }
    }
    let pool = rayon::ThreadPoolBuilder::new().num_threads(FIND_THREADS).build().map_err(|e| e.to_string())?;
    let looked: Vec<Option<dups::Facts>> = pool.install(|| files.par_iter().map(|p| facts_at(p)).collect());
    let root_text = |p: &Path| format!("{}\\", ops::shown(p).trim_end_matches('\\').to_lowercase());
    let roots = [Some(root_text(root)), std::fs::canonicalize(win::verbatim(root)).ok().map(|r| root_text(&r))];
    let windows = win::known_folder_path(&windows_sys::Win32::UI::Shell::FOLDERID_Windows);
    let made: HashSet<String> = ledger.read().iter().map(|m| text_of(&m.linked)).collect();
    let mut seen = HashSet::new();
    let mut groups = Vec::new();
    for (path, facts) in files.iter().zip(looked) {
        let Some(facts) = facts else { continue };
        let Some(key) = facts.key.filter(|_| facts.links >= 2) else { continue };
        if !seen.insert(key) {
            continue;
        }
        let paths = names_of(path).unwrap_or_else(|| vec![ops::shown(path)]);
        // Both end in `\`: the root itself, when it is a file, is in it too.
        let inside = paths.iter().map(|p| roots.iter().flatten().any(|r| format!("{}\\", text_of(p)).starts_with(r.as_str()))).collect();
        let protected = paths.iter().any(|p| protected(Path::new(p), windows.as_deref()));
        let by_fine_file = paths.iter().any(|p| made.contains(&text_of(p)));
        let id = format!("{:016x}-{}", key.0, key.1.iter().map(|b| format!("{b:02x}")).collect::<String>());
        groups.push(HardlinkGroup { id, size: facts.size, modified: facts.modified, paths, inside, by_fine_file, protected });
    }
    groups.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.paths.cmp(&b.paths)));
    Ok(HardlinkScan { root: ops::shown(root), scanned: files.len() as u64, truncated, groups })
}

/// A path as names are compared: as shown, in lower case, without a `\` at its end.
fn text_of(path: &str) -> String {
    ops::shown(Path::new(path)).trim_end_matches('\\').to_lowercase()
}

/// What the file at `path` is (a link there as the link), opened only to
/// read its attributes.
fn facts_at(path: &Path) -> Option<dups::Facts> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(win::verbatim(path))
        .ok()?;
    dups::facts(&file)
}

/// Every name of the file at `path` (where it really is: through a junction
/// on the way, the names on the volume it leads to), each with the volume's
/// drive (or the folder it is mounted at) in front. None when they cannot
/// be listed.
pub(crate) fn names_of(path: &Path) -> Option<Vec<String>> {
    let real = std::fs::canonicalize(win::verbatim(path)).ok()?;
    let wide = win::wide_path(&real);
    let mut root = vec![0u16; 1024];
    if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    let root = String::from_utf16_lossy(&root[..root.iter().position(|&c| c == 0)?]);
    let root = ops::shown(Path::new(&root));
    let root = root.trim_end_matches('\\');
    let mut names = Vec::new();
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let mut find = unsafe { FindFirstFileNameW(wide.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
    if find == INVALID_HANDLE_VALUE && unsafe { GetLastError() } == ERROR_MORE_DATA {
        buf.resize(len as usize + 1, 0);
        len = buf.len() as u32;
        find = unsafe { FindFirstFileNameW(wide.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
    }
    if find == INVALID_HANDLE_VALUE {
        return None;
    }
    loop {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        // `\Users\me\x`, below the volume's root.
        names.push(format!("{root}{}", String::from_utf16_lossy(&buf[..n])));
        len = buf.len() as u32;
        if unsafe { FindNextFileNameW(find, &mut len, buf.as_mut_ptr()) } != 0 {
            continue;
        }
        if unsafe { GetLastError() } != ERROR_MORE_DATA {
            break;
        }
        buf.resize(len as usize + 1, 0);
        len = buf.len() as u32;
        if unsafe { FindNextFileNameW(find, &mut len, buf.as_mut_ptr()) } == 0 {
            break;
        }
    }
    unsafe { FindClose(find) };
    Some(names)
}

/// Whether a name of a file is one Windows keeps (its folders: WinSxS, the
/// Windows folder wherever it is, the Store's `WindowsApps`): such a file
/// is never made a file of its own.
pub(crate) fn protected(path: &Path, windows: Option<&Path>) -> bool {
    let text = path.to_string_lossy().to_lowercase();
    ops::in_windows(path, windows) || text.contains(r"\windowsapps\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let dir = std::env::temp_dir().join(format!("ff-links-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Temp(dir)
        }

        fn dir(&self, name: &str) -> PathBuf {
            let d = self.0.join(name);
            fs::create_dir_all(&d).unwrap();
            d
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(win::verbatim(&self.0));
        }
    }

    /// The rule for a path that is not there: by the path alone.
    fn rule(path: &str, size: u64) -> Verdict {
        verdict(Path::new(path), size, &Markers::default())
    }

    #[test]
    fn the_rule_keeps_games_mod_packs_and_backups_out() {
        use Verdict::*;
        let me = r"C:\Users\toxh1";
        let pack = format!(r"{me}\curseforge\minecraft\Instances\PS Homegrown - Sunlit Valley");
        for (path, size, want) in [
            // What 0.8.2 merged in the user's mod pack: never.
            (format!(r"{pack}\mods\create-1.20.1.jar"), 10 << 20, Blocked(LB_GAME)),
            (format!(r"{pack}\config\portable_blueprints\house.bp"), 2_000, Blocked(LB_GAME)),
            (format!(r"{pack}\saves\Valley\portable_blueprints\house.bp"), 2_000, Blocked(LB_GAME)),
            (format!(r"{pack}\resourcepacks\look.zip"), 50 << 20, Blocked(LB_GAME)),
            (format!(r"{me}\curseforge\minecraft\Backups\Sunlit Valley\mods\jna-5.12.1.jar"), 2 << 20, Blocked(LB_GAME_BACKUP)),
            (format!(r"{me}\Downloads\Abyssal-Ascent-server-pack\libraries\jna-5.12.1.jar"), 2 << 20, Blocked(LB_GAME)),
            (format!(r"{me}\Downloads\Abyssal-Ascent-server-pack\server-icon.png"), 9_000, Blocked(LB_GAME)),
            (format!(r"{me}\AppData\Local\Programs\REA\ghidra_11.2\Ghidra\Framework\lib\jna-5.12.1.jar"), 2 << 20, Blocked(LB_PROGRAM)),
            (format!(r"{me}\AppData\Roaming\.minecraft\versions\1.20.1\1.20.1.jar"), 20 << 20, Blocked(LB_GAME)),
            // The user's own: pictures, videos, archives, setup files in Downloads and the like.
            (format!(r"{me}\Downloads\photo.jpg"), 3 << 20, Allowed),
            (format!(r"{me}\Downloads\Win11_24H2_Korean_x64.iso"), 5 << 30, Allowed),
            (format!(r"{me}\Downloads\setup.exe"), 80 << 20, Allowed),
            (format!(r"{me}\Videos\2024\clip.mkv"), 1 << 30, Allowed),
            (format!(r"{me}\OneDrive\사진\가족.heic"), 3 << 20, Allowed),
            (format!(r"{me}\Documents\manual.pdf"), 3 << 20, Allowed),
            (format!(r"{me}\Downloads\Qwen3-8B-Q4_K_M.gguf"), 5 << 30, Allowed),
            (r"D:\사진\제주\a.jpg".to_string(), 3 << 20, Allowed),
            // ...but not what programs rewrite in place, documents, or anything else.
            (format!(r"{me}\Downloads\settings.json"), 1_000, Blocked(LB_REWRITTEN)),
            (format!(r"{me}\Documents\config.toml"), 1_000, Blocked(LB_REWRITTEN)),
            (format!(r"{me}\Desktop\notes.txt"), 1_000, Blocked(LB_REWRITTEN)),
            (format!(r"{me}\Documents\report.docx"), 100_000, Blocked(LB_DOCUMENT)),
            (format!(r"{me}\Downloads\tool.jar"), 100_000, Blocked(LB_KIND)),
            (format!(r"{me}\Downloads\small.bin"), 100_000, Blocked(LB_KIND)),
            (format!(r"{me}\movie.mp4"), 1 << 30, Blocked(LB_PLACE)),
            (r"D:\Anime\a.mkv".to_string(), 1 << 30, Blocked(LB_PLACE)),
            (r"D:\Backup\2024\photo.jpg".to_string(), 3 << 20, Blocked(LB_BACKUP)),
            (r"D:\GitHub\app\logo.png".to_string(), 30_000, Blocked(LB_DEV)),
            (format!(r"{me}\.cache\huggingface\hub\m\model.safetensors"), 5 << 30, Blocked(LB_CACHE)),
            (r"C:\Windows\WinSxS\amd64_x\a.dll".to_string(), 30_000, Blocked(LB_SYSTEM)),
            // A big model or archive in a program's folder: only when asked. (0.8.2's own pair is these
            // two, which the pair rule then turns down all the same: the package folder's permissions differ.)
            (format!(r"{me}\AppData\Local\VideoTranslator\models\Qwen3-8B-Q4_K_M.gguf"), 5 << 30, OptIn(LB_PROGRAM_OPTIN)),
            (
                format!(r"{me}\AppData\Local\Packages\OpenAI.Codex_2p2nqsd0c76g0\LocalCache\Local\VideoTranslator\models\Qwen3-8B-Q4_K_M.gguf"),
                5 << 30,
                OptIn(LB_PROGRAM_OPTIN),
            ),
            (format!(r"{me}\AppData\Local\Vendor\weights.bin"), 200 << 20, OptIn(LB_PROGRAM_OPTIN)),
            (r"C:\Program Files\App\data.7z".to_string(), 200 << 20, OptIn(LB_PROGRAM_OPTIN)),
            (format!(r"{me}\AppData\Local\Vendor\weights.bin"), 50 << 20, Blocked(LB_PROGRAM)),
            (format!(r"{me}\AppData\Local\Vendor\app.dll"), 50 << 20, Blocked(LB_PROGRAM)),
            (format!(r"{me}\AppData\Roaming\Vendor\settings.json"), 1_000, Blocked(LB_REWRITTEN)),
        ] {
            assert_eq!(rule(&path, size), want, "{path}");
        }
        assert_eq!(Allowed.refusal(false), None);
        assert_eq!(OptIn(LB_PROGRAM_OPTIN).refusal(false), Some(LB_PROGRAM_OPTIN));
        assert_eq!(OptIn(LB_PROGRAM_OPTIN).refusal(true), None);
        assert_eq!(Blocked(LB_GAME).refusal(true), Some(LB_GAME));
    }

    #[test]
    fn a_pack_unpacked_in_downloads_is_told_by_what_it_holds() {
        let t = Temp::new("markers");
        // A server pack whose name says nothing, and a plain folder of pictures.
        let pack = t.dir("Big Adventure");
        fs::write(pack.join("eula.txt"), b"eula=true").unwrap();
        let photos = t.dir("trip");
        let code = t.dir("site");
        fs::create_dir_all(code.join(".git")).unwrap();
        let deep = t.dir(r"site\assets\img");
        let seen = |real: &Path| {
            let rest = real.strip_prefix(&t.0).unwrap();
            Path::new(r"C:\Users\me\Downloads").join(rest).to_string_lossy().into_owned()
        };
        let markers = Markers::default();
        let judge = |real: PathBuf| verdict_as(&seen(&real), &real, 1 << 20, &markers);
        assert_eq!(judge(pack.join("icon.png")), Verdict::Blocked(LB_GAME));
        assert_eq!(judge(photos.join("a.jpg")), Verdict::Allowed);
        assert_eq!(judge(deep.join("logo.png")), Verdict::Blocked(LB_DEV));
        // Only the folders below Downloads count: what is next to the file in Downloads itself does not.
        assert_eq!(judge(t.0.join("b.jpg")), Verdict::Allowed);
        // The duplicate finder calls such a file a game's (DupFile.category), so automatic picks leave it alone.
        let game = |real: PathBuf| in_game_as(&seen(&real), &real, &markers);
        assert!(game(pack.join("icon.png")));
        assert!(!game(photos.join("a.jpg")) && !game(deep.join("logo.png")));
        let named = |p: &str| in_game(Path::new(p), &Markers::default());
        assert!(named(r"C:\Users\me\Downloads\Abyssal-Ascent-server-pack\mods\x.jar"));
        assert!(named(r"C:\Users\me\curseforge\minecraft\Instances\Sunlit Valley\mods\x.jar"));
        assert!(!named(r"C:\Users\me\Downloads\photo.jpg") && !named(r"C:\Users\me\Documents\report.docx"));
        // Looked at once: a marker put there later is not seen by the same search.
        fs::write(photos.join("server.properties"), b"").unwrap();
        assert_eq!(judge(photos.join("a.jpg")), Verdict::Allowed);
        assert_eq!(verdict_as(&seen(&photos.join("a.jpg")), &photos.join("a.jpg"), 1, &Markers::default()), Verdict::Blocked(LB_GAME));
    }

    #[test]
    fn a_folder_that_leads_into_a_game_is_the_games() {
        let t = Temp::new("through");
        let packs = t.dir(r"curseforge\minecraft\Instances\X\resourcepacks");
        fs::write(packs.join("look.zip"), b"zip").unwrap();
        fs::write(t.dir("photos").join("a.jpg"), b"jpg").unwrap();
        let dl = t.dir("dl");
        crate::ops::create_junction(&dl.join("packs"), &packs).unwrap();
        crate::ops::create_junction(&dl.join("pics"), &t.0.join("photos")).unwrap();
        // The test folder as if it were Downloads, its paths as written and as they really are (`\\?\C:\...`).
        let home = fs::canonicalize(&t.0).unwrap();
        let as_seen = |p: &Path| {
            let rest = p.strip_prefix(&home).or_else(|_| p.strip_prefix(&t.0)).unwrap();
            Path::new(r"C:\Users\me\Downloads").join(rest).to_string_lossy().into_owned()
        };
        let markers = Markers::default();
        let judge = |p: &Path| verdict_through(&as_seen(p), p, 3, &markers, as_seen);
        // As written, a zip in Downloads; where it really is, a mod pack's.
        let look = dl.join("packs").join("look.zip");
        assert_eq!(verdict_as(&as_seen(&look), &look, 3, &markers), Verdict::Allowed);
        assert_eq!(judge(&look), Verdict::Blocked(LB_GAME));
        // A junction to the user's own pictures leads to the user's own.
        assert_eq!(judge(&dl.join("pics").join("a.jpg")), Verdict::Allowed);
        for j in ["packs", "pics"] {
            fs::remove_dir(dl.join(j)).unwrap();
        }
    }

    /// Gives `path` the protected DACL `sddl` (`D:P(...)`), only what it says.
    pub(crate) fn set_sddl(path: &Path, sddl: &str) {
        put_sddl(path, sddl, true);
    }

    /// Gives `path` the ACEs `sddl` (`D:(...)`) besides what its folder passes down.
    fn set_sddl_inherited(path: &Path, sddl: &str) {
        put_sddl(path, sddl, false);
    }

    fn put_sddl(path: &Path, sddl: &str, protected: bool) {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_DACL_SECURITY_INFORMATION,
        };
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let text = win::wide(sddl);
        assert!(unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } != 0);
        let (mut present, mut acl, mut defaulted) = (0, null_mut(), 0);
        assert!(unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) } != 0);
        let name = win::wide(&path.to_string_lossy());
        let what = DACL_SECURITY_INFORMATION | if protected { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
        let e = unsafe { SetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, what, null_mut(), null_mut(), acl, std::ptr::null()) };
        unsafe { LocalFree(sd) };
        assert_eq!(e, ERROR_SUCCESS, "{}", path.display());
    }

    /// The owner, SYSTEM and Administrators may do anything; nobody else anything.
    pub(crate) const OWN_DACL: &str = "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)";

    #[test]
    fn pairs_must_share_attributes_and_permissions() {
        let t = Temp::new("pairs");
        let (a, b, c, d) = (t.0.join("a.jpg"), t.0.join("b.jpg"), t.0.join("c.jpg"), t.0.join("d.jpg"));
        for p in [&a, &b, &c, &d] {
            fs::write(p, b"same").unwrap();
        }
        let share = |p: &Path| sharing_at(p).unwrap();
        assert_eq!(pair_refusal(&share(&a), &share(&b)), None);
        // Read-only at one only.
        let mut ro = fs::metadata(&d).unwrap().permissions();
        ro.set_readonly(true);
        fs::set_permissions(&d, ro).unwrap();
        assert_eq!(pair_refusal(&share(&a), &share(&d)), Some(LB_ATTRS));
        // Permissions of its own at one only.
        set_sddl(&c, OWN_DACL);
        assert_eq!(pair_refusal(&share(&a), &share(&c)), Some(LB_SECURITY));
        assert_eq!(pair_refusal(&share(&a), &share(&b)), None);

        // In a group: the file that goes with none of the others is blocked for that.
        use Verdict::*;
        let (sa, sb, sc) = (sharing_at(&a), sharing_at(&b), sharing_at(&c));
        let opt = OptIn(LB_PROGRAM_OPTIN);
        let shares = [sa, sb, sc];
        assert_eq!(judged(&[Allowed, Allowed, Allowed], &shares), [(None, false), (None, false), (Some(LB_SECURITY), false)]);
        assert_eq!(judged(&[opt, Allowed, opt], &shares), [(Some(LB_PROGRAM_OPTIN), true), (None, false), (Some(LB_SECURITY_APPS), false)]);
        // With nothing else that may be merged, nothing to go with is no reason of its own.
        assert_eq!(judged(&[Allowed, Blocked(LB_GAME), Blocked(LB_KIND)], &shares), [(None, false), (Some(LB_GAME), false), (Some(LB_KIND), false)]);
        // Two that differ, and nothing else: neither goes with anything.
        let two = [sharing_at(&a), sharing_at(&c)];
        assert_eq!(judged(&[Allowed, Allowed], &two), [(Some(LB_SECURITY), false), (Some(LB_SECURITY), false)]);
        // Two models of two programs whose folders give different permissions (a Store app's package
        // folder adds an ACE for its own SID): not offered, and told that asking would not help.
        assert_eq!(judged(&[opt, opt], &two), [(Some(LB_SECURITY_APPS), false), (Some(LB_SECURITY_APPS), false)]);
        assert_eq!(judged(&[opt, Allowed], &two), [(Some(LB_SECURITY_APPS), false), (Some(LB_SECURITY), false)]);
        // As told in a group: the files' folders pass down different ACEs (as `Packages\<app>` does).
        let (apps, store) = (t.dir("apps"), t.dir("store"));
        set_sddl_inherited(&store, "D:(A;OICI;FA;;;S-1-15-3-1024-1065365936-1281604716-3511738428-1654721687-432734479-3232135806-4053264122-3456934681)");
        let (m1, m2) = (apps.join("model.gguf"), store.join("model.gguf"));
        for p in [&m1, &m2] {
            fs::write(p, b"same").unwrap();
        }
        assert_eq!(pair_refusal(&share(&m1), &share(&m2)), Some(LB_SECURITY));
        assert_eq!(judged(&[opt, opt], &[sharing_at(&m1), sharing_at(&m2)]), [(Some(LB_SECURITY_APPS), false), (Some(LB_SECURITY_APPS), false)]);
        // One whose security could not be read is left to the merge.
        assert_eq!(judged(&[Allowed, Allowed], &[sharing_at(&a), None]), [(None, false), (None, false)]);
        // The temp folder is app data and a cache to the rule: all blocked by place.
        let paths = [a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()];
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let all = judge_group(&refs, 4, &Markers::default());
        assert!(all.iter().all(|(why, optin)| *why == Some(LB_CACHE) && !optin), "{all:?}");
    }

    #[test]
    fn the_ledger_appends_reads_and_is_cut_down_to_its_newest() {
        let t = Temp::new("ledger");
        let ledger = Ledger { path: t.0.join("data").join(LEDGER_FILE), most: 3, bytes: 400 };
        assert!(ledger.read().is_empty());
        let merge = |n: i64| Merge { kept: format!(r"C:\a\{n}.zip"), linked: format!(r"C:\b\{n}.zip"), size: 10, at: n };
        for n in 1..=2 {
            ledger.add(&merge(n)).unwrap();
        }
        assert_eq!(ledger.read(), [merge(1), merge(2)]);
        // A line cut short by a crash costs only itself.
        let mut f = OpenOptions::new().append(true).open(&ledger.path).unwrap();
        f.write_all(br#"{"kept":"C:\\a\\x.zip","lin"#).unwrap();
        drop(f);
        ledger.add(&merge(3)).unwrap();
        assert_eq!(ledger.read(), [merge(1), merge(2), merge(3)]);
        // Past its size, only the newest `most` stay.
        for n in 4..=8 {
            ledger.add(&merge(n)).unwrap();
        }
        let left = ledger.read();
        assert!(left.len() <= 6 && left.len() >= 3, "{left:?}");
        assert_eq!(left.last(), Some(&merge(8)));
        assert!(fs::metadata(&ledger.path).unwrap().len() <= 400 + 100);
        ledger.forget(|m| m.at == 8).unwrap();
        assert_eq!(ledger.read().last(), Some(&merge(7)));
        assert!(ledger.read().len() <= 3);

        // Long names, so that `most` lines are more than `bytes`: a cut goes down to half of
        // `bytes`, not to `most` lines still over it, which every add would then write again.
        let long = Ledger { path: t.0.join("long.jsonl"), most: 100, bytes: 4000 };
        let deep = "깊은 폴더".repeat(20);
        // (Lines of one length: two digits each.)
        let named = |n: i64| Merge { kept: format!(r"C:\{deep}\{n:02}.zip"), linked: format!(r"D:\{deep}\{n:02}.zip"), size: 10, at: 10 + n };
        let mut sizes = Vec::new();
        for n in 0..40 {
            long.add(&named(n)).unwrap();
            sizes.push(fs::metadata(&long.path).unwrap().len());
        }
        assert!(sizes.iter().all(|&s| s <= 4000), "{sizes:?}");
        // Once cut, adds only append for a while: each makes the file one line longer.
        let line = sizes[1] - sizes[0];
        let cuts = sizes.windows(2).filter(|w| w[1] != w[0] + line).count();
        assert!(cuts >= 1 && cuts * 3 <= sizes.len(), "{cuts} cuts: {sizes:?}");
        assert_eq!(long.read().last(), Some(&named(39)));
        // The newest stays even when it alone is more than the room.
        let tiny = Ledger { path: t.0.join("tiny.jsonl"), most: 100, bytes: 10 };
        tiny.add(&named(1)).unwrap();
        tiny.add(&named(2)).unwrap();
        assert_eq!(tiny.read(), [named(2)]);

        // As the window gets them: newest first, and whether the two are one file still.
        let shown = Ledger::at(t.0.join("shown.jsonl"));
        shown.add(&merge(1)).unwrap();
        let (kept, linked) = (t.0.join("kept.zip"), t.0.join("linked.zip"));
        fs::write(&kept, b"zip").unwrap();
        fs::hard_link(&kept, &linked).unwrap();
        let real = Merge { kept: kept.to_string_lossy().into_owned(), linked: linked.to_string_lossy().into_owned(), size: 3, at: 99 };
        shown.add(&real).unwrap();
        let list = records(&shown);
        assert_eq!((list[0].kept.as_str(), list[0].still_linked), (real.kept.as_str(), true));
        assert!(!list[1].still_linked);
        fs::remove_file(&linked).unwrap();
        fs::write(&linked, b"zip").unwrap();
        assert!(!records(&shown)[0].still_linked);
    }

    #[test]
    fn every_name_of_a_file_is_found_also_outside_the_folder() {
        let t = Temp::new("find");
        let root = t.dir("root");
        let (a, b, c) = (root.join("a.bin"), t.dir(r"root\sub").join("b.bin"), t.dir("outside").join("c.bin"));
        fs::write(&a, vec![7u8; 5000]).unwrap();
        fs::hard_link(&a, &b).unwrap();
        fs::hard_link(&a, &c).unwrap();
        fs::write(root.join("alone.bin"), b"one name").unwrap();
        // A junction in the folder is not followed.
        crate::ops::create_junction(&root.join("link"), &t.0.join("outside")).unwrap();
        let ledger = Ledger::at(t.0.join(LEDGER_FILE));
        ledger.add(&Merge { kept: a.to_string_lossy().into_owned(), linked: b.to_string_lossy().into_owned(), size: 5000, at: 1 }).unwrap();

        let scan = find_links(&root, FIND_MOST, &ledger).unwrap();
        assert_eq!((scan.scanned, scan.truncated), (3, false));
        assert_eq!(scan.groups.len(), 1, "{:?}", scan.groups);
        let g = &scan.groups[0];
        assert_eq!((g.size, g.by_fine_file, g.protected), (5000, true, false));
        let mut got: Vec<(String, bool)> = g.paths.iter().cloned().zip(g.inside.iter().copied()).collect();
        got.sort();
        let real = |p: &Path| ops::shown(&fs::canonicalize(p).unwrap());
        let mut want = vec![(real(&a), true), (real(&b), true), (real(&c), false)];
        want.sort();
        assert_eq!(got, want);
        // Cut short at its cap, and said so.
        let cut = find_links(&root, 1, &ledger).unwrap();
        assert!(cut.truncated);
        assert_eq!(cut.scanned, 1);
        // A folder that is not there.
        assert!(find_links(&t.0.join("gone"), FIND_MOST, &ledger).unwrap_err().starts_with(NO_ROOT));
        assert_eq!(find_links(Path::new("relative"), FIND_MOST, &ledger).unwrap_err(), BAD_ROOT);
        fs::remove_dir(root.join("link")).unwrap();
    }

    #[test]
    fn names_in_windows_own_folders_are_protected() {
        let windows = win::known_folder_path(&windows_sys::Win32::UI::Shell::FOLDERID_Windows);
        assert!(protected(Path::new(r"C:\Windows\WinSxS\amd64_x\a.dll"), windows.as_deref()));
        assert!(protected(Path::new(r"C:\Program Files\WindowsApps\Pkg\a.dll"), windows.as_deref()));
        assert!(!protected(Path::new(r"C:\Users\me\Downloads\a.zip"), windows.as_deref()));
        // Windows's own programs are names of files in WinSxS.
        let Some(w) = windows else { return };
        let scan = find_links(&w.join("System32").join("Dism"), FIND_MOST, &Ledger::at(std::env::temp_dir().join("ff-links-none.jsonl"))).unwrap();
        assert!(scan.groups.iter().all(|g| g.protected), "{:?}", scan.groups);
        assert!(scan.groups.iter().any(|g| g.paths.iter().any(|p| p.to_lowercase().contains(r"\winsxs\")) && g.inside.contains(&false)));
    }
}
