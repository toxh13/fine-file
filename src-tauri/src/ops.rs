//! Copy and move jobs with progress, name conflicts and cancel; new folders;
//! and the Windows file clipboard (so copy/cut/paste works with Explorer).
//!
//! Events: "job-progress" (JobInfo, at most ~10 per second per job and on
//! every state change) and "job-conflict" (Conflict, the job then waits for
//! `resolve_conflict`; `jobs` and `pending_conflicts` give it again to a
//! window that reloaded meanwhile).
//!
//! Nothing here destroys the user's data. Copies never write over a name
//! that is taken. An item the user chose to overwrite goes to the Recycle
//! Bin first; on a drive without one (or with it switched off) it is left
//! alone and the reason lands in the job's errors. Only an item of the same
//! kind is overwritten: a file never makes room for a folder or the other
//! way round, and a real folder never makes room for a link. The only things
//! removed for good are the sources of a move across volumes, each only
//! after its whole top-level item went through and the copy has the
//! source's size, and an unfinished copy of our own when a job is cancelled
//! or a file fails (also by closing the app, which cancels its jobs and
//! waits for that). A relocation (v0.4, at the end of this file) is a move
//! too: it removes the original folder for good only once all of it is at
//! the target, checked entry by entry, and the junction left in its place
//! leads there; its undo does the same the other way round, and so does
//! `recover` at the next start for one cut short by a crash. A merge of
//! duplicates into hard links (v0.8.2, at the end of this file) removes
//! nothing either: each copy it replaces goes to the Recycle Bin, and one cut
//! short by a crash is put right at the next start too (`recover_links`).
//!
//! A file is copied under a hidden name of its own next to where it goes
//! (`.name.ff-partial`) and gets its real name only once all of it is there,
//! so a copy cut short by a crash, a kill or a power cut never looks
//! finished. Only the partial file a job made itself is ever removed: one
//! that is there already (another job's, or one left by a crash) is left
//! alone and the next free name is taken.
//!
//! Every std::fs call on a job's paths goes through `win::verbatim`, and the
//! file APIs get `win::wide_path`, so a name ending in a dot or a space is
//! that name and not its neighbour without the dot.

use crate::{dups, links, win};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, OsStr, OsString};
use std::fs::{DirEntry, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::Foundation::{
    GlobalFree, RtlNtStatusToDosError, ERROR_ACCESS_DENIED, ERROR_DIR_NOT_EMPTY, ERROR_DISK_FULL, ERROR_FILENAME_EXCED_RANGE,
    ERROR_FILE_NOT_FOUND, ERROR_FILE_TOO_LARGE, ERROR_HANDLE_DISK_FULL, ERROR_INVALID_NAME, ERROR_LOCK_VIOLATION,
    ERROR_NOT_SAME_DEVICE, ERROR_PATH_NOT_FOUND, ERROR_PRIVILEGE_NOT_HELD, ERROR_REQUEST_ABORTED, ERROR_SHARING_VIOLATION,
    ERROR_SUCCESS, ERROR_WRITE_PROTECT, GENERIC_READ, GENERIC_WRITE, HANDLE, HGLOBAL, INVALID_HANDLE_VALUE, LocalFree, NTSTATUS,
    POINT,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    GetAce, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, InitializeSecurityDescriptor,
    SetKernelObjectSecurity, SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, ACE_HEADER, ACL,
    DACL_SECURITY_INFORMATION, INHERITED_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    CopyFileExW, CreateDirectoryW, CreateSymbolicLinkW, FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo,
    FileDispositionInfoEx, GetDriveTypeW, GetFileInformationByHandleEx,
    GetFileAttributesW, GetVolumeInformationByHandleW, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW, MoveFileExW, ReOpenFile, RemoveDirectoryW, SetFileAttributesW, SetFileInformationByHandle, DELETE,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM,
    FILE_ATTRIBUTE_TEMPORARY, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_LIST_DIRECTORY, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, INVALID_FILE_ATTRIBUTES, READ_CONTROL, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
    SYMBOLIC_LINK_FLAG_DIRECTORY, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::UI::Shell::{
    DragQueryFileW, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86,
    FOLDERID_Profile, FOLDERID_UserProfiles, FOLDERID_Windows, SHGetKnownFolderPath, DROPFILES,
};

/// Finished jobs `jobs()` still lists; older ones are forgotten.
const KEEP_FINISHED: usize = 50;
/// Errors listed per job; past this they are only counted.
const MAX_ERRORS: usize = 1000;
/// A running job's progress events carry only its newest errors (the whole
/// list goes with the last event and with `jobs`): a thousand paths ten
/// times a second would be megabytes of JSON for the window.
const EVENT_ERRORS: usize = 50;
/// The speed is measured over gaps at least this long (seconds)...
const SPEED_SAMPLE: f64 = 0.25;
/// ...and smoothed over about this long (seconds).
const SPEED_SMOOTHING: f64 = 2.0;
/// OpenClipboard tries: another program (a clipboard manager, say) may hold
/// the clipboard for a moment.
const CLIPBOARD_TRIES: u32 = 8;
const CLIPBOARD_RETRY: Duration = Duration::from_millis(20);
/// The end of the name a file is copied under until it is whole.
const PARTIAL_EXT: &str = ".ff-partial";
/// Partial names tried per file (`.a.ff-partial`, `.a (2).ff-partial`, ...).
const PARTIAL_TRIES: u32 = 100;
/// UTF-16 units a name may have (NTFS, exFAT, FAT32 long names).
const MAX_NAME: usize = 255;
/// Attributes SetFileAttributes changes; the others it leaves as they are.
const SETTABLE: u32 = FILE_ATTRIBUTE_READONLY
    | FILE_ATTRIBUTE_HIDDEN
    | FILE_ATTRIBUTE_SYSTEM
    | FILE_ATTRIBUTE_ARCHIVE
    | FILE_ATTRIBUTE_TEMPORARY
    | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;

// From WinBase.h, WinIoCtl.h and WinNT.h; not worth three more windows-sys features.
const COPY_FILE_FAIL_IF_EXISTS: u32 = 1;
const COPY_FILE_COPY_SYMLINK: u32 = 0x800;
/// Never onto a name that is taken; and a file listed as a file that has
/// become a symbolic link since is copied as the link, not as what it leads to.
const COPY_FLAGS: u32 = COPY_FILE_FAIL_IF_EXISTS | COPY_FILE_COPY_SYMLINK;
const PROGRESS_CONTINUE: u32 = 0;
const PROGRESS_CANCEL: u32 = 1;
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

const NO_JOB: &str = "작업을 찾을 수 없어요";
const NAME_TAKEN: &str = "같은 이름이 이미 있어요";
const BAD_NAME: &str = "이름에 쓸 수 없는 문자가 있어요 (\\ / : * ? \" < > |)";
const EMPTY_NAME: &str = "이름이 비어 있어요";
const RESERVED_NAME: &str = "Windows가 따로 쓰는 이름이라 쓸 수 없어요 (CON, PRN, AUX, NUL, COM1~9, LPT1~9)";
const BAD_PATH: &str = "경로가 올바르지 않아요";
const DEST_MISSING: &str = "대상 폴더를 찾을 수 없어요";
const WHOLE_DRIVE: &str = "드라이브 전체는 복사하거나 옮길 수 없어요";
const INTO_ITSELF: &str = "폴더를 그 폴더 안으로 복사하거나 옮길 수 없어요";
const NO_FREE_NAME: &str = "새 이름을 고를 수 없어요";
const NO_RECYCLE_BIN: &str = "이 드라이브에는 휴지통이 없어서 덮어쓰지 않았어요 (덮어쓰면 원래 항목을 되돌릴 수 없어요)";
const BIN_DISABLED: &str =
    "이 드라이브는 휴지통을 쓰지 않도록 설정돼 있어서 덮어쓰지 않았어요 (덮어쓰면 원래 항목을 되돌릴 수 없어요)";
const BIN_TOO_SMALL: &str = "원래 항목이 휴지통보다 커서 덮어쓰지 않았어요. 휴지통 속성에서 최대 크기를 늘리면 덮어쓸 수 있어요";
const NOT_RECYCLED: &str = "원래 항목을 휴지통으로 옮기지 못해서 덮어쓰지 않았어요";
const HOLDS_SOURCE: &str = "원본이 들어 있는 폴더라서 덮어쓰지 않았어요";
const KIND_DIFFERS: &str = "이미 있는 항목과 종류(파일, 폴더, 링크)가 달라서 덮어쓰지 않았어요";
const ODD_NAME: &str = "이름이 점(.)이나 공백으로 끝나는 항목은 휴지통으로 옮길 수 없어서 덮어쓰지 않았어요";
const SIZE_MISMATCH: &str = "복사본의 크기가 원본과 달라서 원본을 지우지 않았어요";
const SOURCE_CHANGED: &str = "옮기는 동안 원본이 바뀌어서 원본을 남겨 두었어요";
const FOLDER_LEFT: &str = "원본 폴더에 새로 생긴 항목이 있어서 폴더를 남겨 두었어요";
const NO_SYMLINK_RIGHT: &str =
    "심볼릭 링크를 만들 권한이 없어서 복사하지 않았어요 (Windows 설정에서 개발자 모드를 켜면 만들 수 있어요)";
const LINK_KIND: &str = "이 종류의 링크는 복사하지 않았어요";
const CLIPBOARD_BUSY: &str = "다른 프로그램이 클립보드를 쓰고 있어요. 잠시 후 다시 해 보세요";
const NO_PARTIAL_NAME: &str = "복사하는 동안 쓸 임시 이름을 고를 수 없어요";
const TURNED_LINK: &str = "목록을 읽은 뒤에 링크로 바뀐 폴더라서 들어가지 않았어요";
/// What a path to an entry leads to is not the entry its folder holds (a folder on the way set as a mount point).
const ENTRY_CHANGED: &str = "목록을 읽은 뒤에 다른 파일로 바뀌어서 복사하지 않았어요";
const NO_SECURITY_COPY: &str = "권한 설정을 그대로 옮기지 못했어요";

#[derive(Serialize, Clone)]
pub struct JobInfo {
    pub id: u64,
    /// "copy", "move", "relocate" (`start_relocate`), "restore"
    /// (`undo_relocate`), "link" (`start_dup_link`; also `recover_links`
    /// at start, telling what it did with merges cut short) or "unlink"
    /// (`start_unlink`)
    pub op: String,
    /// "running" | "waiting" (on a conflict) | "done" | "failed" | "cancelled"
    pub state: String,
    pub dest: String,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_items: u64,
    pub done_items: u64,
    /// Path being copied right now.
    pub current: Option<String>,
    pub speed_bps: u64,
    /// (path, Korean reason) for items that could not be copied or moved,
    /// the first MAX_ERRORS of them. While the job runs its events list only
    /// the newest EVENT_ERRORS.
    pub errors: Vec<(String, String)>,
    /// How many items could not be copied or moved, also those past what
    /// `errors` lists.
    pub error_count: u64,
    /// "link": bytes of the merged copies now in the Recycle Bin, which come
    /// free once it is emptied. 0 for the other jobs.
    pub freed_bytes: u64,
}

#[derive(Serialize, Clone)]
pub struct Conflict {
    pub job: u64,
    pub source: String,
    pub target: String,
    /// The incoming item is a folder (or a link to one).
    pub is_dir: bool,
    /// The item already there is a folder (or a link to one). Overwriting
    /// merges folder into folder; items of different kinds are not overwritten.
    pub target_is_dir: bool,
    /// The incoming item is a junction or symbolic link.
    pub is_link: bool,
    /// The item already there is a junction or symbolic link. A link and a
    /// real file or folder are of different kinds.
    pub target_is_link: bool,
    /// File sizes; 0 for a folder or a link (not counted).
    pub source_size: u64,
    pub target_size: u64,
    /// FILETIME
    pub source_modified: i64,
    pub target_modified: i64,
}

#[derive(Serialize)]
pub struct ClipboardFiles {
    pub paths: Vec<String>,
    /// True when the files were cut (Explorer's "Preferred DropEffect" = move).
    pub cut: bool,
}

/// Runs `f` on a blocking thread. A panic in it comes back as an Err string.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())
}

/// Makes `parent\name` and returns its path. Fails if the name is taken.
#[tauri::command]
pub async fn create_folder(parent: String, name: String) -> Result<String, String> {
    blocking(move || make_folder(Path::new(&parent), &name)).await?
}

fn make_folder(parent: &Path, name: &str) -> Result<String, String> {
    let name = check_name(name)?;
    if !parent.is_absolute() {
        return Err(BAD_PATH.into());
    }
    let path = parent.join(name);
    let wide = win::wide_path(&path);
    if unsafe { CreateDirectoryW(wide.as_ptr(), null()) } == 0 {
        let e = io::Error::last_os_error();
        return Err(match e.kind() {
            // Also when a file has that name.
            io::ErrorKind::AlreadyExists => NAME_TAKEN.into(),
            io::ErrorKind::NotFound => "폴더를 찾을 수 없어요".into(),
            _ => format!("폴더를 만들 수 없어요: {}", reason(&e)),
        });
    }
    Ok(path.to_string_lossy().into_owned())
}

/// A new item's name as Explorer would take it: spaces around it and dots
/// at its end dropped (Windows drops those dots anyway). Fails for names
/// Windows cannot use. Also for renames, which take the new name exactly.
pub(crate) fn check_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name == "." || name == ".." || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) || name.chars().any(char::is_control)
    {
        return Err(BAD_NAME);
    }
    let name = name.trim_end_matches(['.', ' ']);
    if name.is_empty() {
        return Err(EMPTY_NAME);
    }
    if is_device_name(name) {
        return Err(RESERVED_NAME);
    }
    Ok(name)
}

/// CON, PRN, AUX, NUL, COM1-9, LPT1-9 (also with an extension, `nul.txt`):
/// Windows opens the device instead of a file of that name.
fn is_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let mut chars = base.chars();
    let head: String = chars.by_ref().take(3).collect();
    matches!(head.as_str(), "COM" | "LPT") && matches!((chars.next(), chars.next()), (Some('1'..='9' | '¹' | '²' | '³'), None))
}

/// op: "copy" | "move". Returns the job id at once; progress arrives as events.
#[tauri::command]
pub async fn start_transfer(app: AppHandle, op: String, sources: Vec<String>, dest: String) -> Result<u64, String> {
    blocking(move || start(&op, sources, dest, app, Settings::APP)).await?
}

/// choice: "skip" | "overwrite" | "rename" | "cancel". With `apply_all`, later
/// conflicts in the same job get the same answer without asking.
#[tauri::command]
pub fn resolve_conflict(job: u64, choice: String, apply_all: bool) -> Result<(), String> {
    let choice = Choice::parse(&choice).ok_or_else(|| format!("알 수 없는 선택이에요 ({choice})"))?;
    let mut reg = registry();
    let entry = reg.get(job).ok_or(NO_JOB)?;
    // An answer that comes late (the job was cancelled meanwhile) changes nothing.
    if entry.waiting {
        entry.answer = Some((choice, apply_all));
        drop(reg);
        jobs_state().answered.notify_all();
    }
    Ok(())
}

#[tauri::command]
pub fn cancel_job(job: u64) -> Result<(), String> {
    let mut reg = registry();
    let entry = reg.get(job).ok_or(NO_JOB)?;
    // Set under the lock, so a job about to wait for an answer sees it.
    entry.cancel.store(true, Ordering::SeqCst);
    drop(reg);
    jobs_state().answered.notify_all();
    Ok(())
}

/// Every job of this session, newest first, with all their errors. The
/// questions jobs are waiting on go out again as "job-conflict": a window
/// that (re)loaded after they were sent asks for the jobs first thing.
#[tauri::command]
pub fn jobs(app: AppHandle) -> Vec<JobInfo> {
    let list = job_list();
    for c in pending_conflicts() {
        app.conflict(&c);
    }
    list
}

/// The questions jobs are waiting on, oldest job first.
#[tauri::command]
pub fn pending_conflicts() -> Vec<Conflict> {
    registry().jobs.iter().filter(|e| e.waiting).filter_map(|e| e.pending.clone()).collect()
}

fn job_list() -> Vec<JobInfo> {
    registry().jobs.iter().rev().map(|e| e.info.clone()).collect()
}

/// Jobs still running or waiting on an answer.
pub fn active_jobs() -> usize {
    registry().jobs.iter().filter(|e| !is_finished(&e.info.state)).count()
}

/// The ids of the jobs still going that the user started, not a plan's
/// step (lib.rs: those the user agreed to stop for an update).
pub fn own_jobs() -> Vec<u64> {
    registry().jobs.iter().filter(|e| !e.for_plan && !is_finished(&e.info.state)).map(|e| e.info.id).collect()
}

/// Marks the job `id` as a plan step's.
fn for_plan(id: u64) {
    if let Some(e) = registry().get(id) {
        e.for_plan = true;
    }
}

/// Cancels every job still going. Each removes its unfinished copy and stops.
pub fn cancel_all() {
    let reg = registry();
    for e in &reg.jobs {
        e.cancel.store(true, Ordering::SeqCst);
    }
    drop(reg);
    jobs_state().answered.notify_all();
}

/// Waits until no job is going, at most `limit`. True when none is.
pub fn wait_all(limit: Duration) -> bool {
    let reg = registry();
    let (reg, _) = jobs_state()
        .ended
        .wait_timeout_while(reg, limit, |r| r.jobs.iter().any(|e| !is_finished(&e.info.state)))
        .unwrap_or_else(PoisonError::into_inner);
    !reg.jobs.iter().any(|e| !is_finished(&e.info.state))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Choice {
    Skip,
    Overwrite,
    Rename,
    Cancel,
}

impl Choice {
    fn parse(s: &str) -> Option<Choice> {
        Some(match s {
            "skip" => Choice::Skip,
            "overwrite" => Choice::Overwrite,
            "rename" => Choice::Rename,
            "cancel" => Choice::Cancel,
            _ => return None,
        })
    }
}

/// Where a job's events go: the window, or a test (or, for a plan's job,
/// whatever the plan reports to).
pub(crate) trait Events: Send + 'static {
    fn progress(&self, job: &JobInfo);
    fn conflict(&self, conflict: &Conflict);
}

impl Events for Box<dyn Events> {
    fn progress(&self, job: &JobInfo) {
        (**self).progress(job);
    }

    fn conflict(&self, conflict: &Conflict) {
        (**self).conflict(conflict);
    }
}

impl Events for AppHandle {
    fn progress(&self, job: &JobInfo) {
        let _ = self.emit("job-progress", job);
    }

    fn conflict(&self, conflict: &Conflict) {
        let _ = self.emit("job-conflict", conflict);
    }
}

/// How a job runs; tests turn the knobs.
#[derive(Clone, Copy)]
struct Settings {
    /// Progress events of one job are at least this far apart (state changes excepted).
    gap: Duration,
    /// Moves within a volume rename instead of copying, and a relocation
    /// must go to another volume. Off only in tests, which have no second
    /// drive: then any two folders count as on two volumes (and a merge into
    /// hard links turns every copy down).
    rename: bool,
    /// Makes the junction a relocation leaves at the original's path
    /// (link, pointing to); tests make it fail.
    junction: fn(&Path, &Path) -> io::Result<()>,
    /// Makes the hard link a merge puts in place of a copy (new name, the
    /// open file that stays); tests make it fail.
    hard_link: fn(&Path, &File) -> io::Result<()>,
    /// Sends the old bytes of a merged copy to the Recycle Bin, once
    /// `binless` found nothing in the way; tests keep the real bin out of it.
    bin: fn(&Path) -> Result<(), String>,
    /// What the link rule says of a file a merge would make one with another
    /// (path, size): `links::verdict`; tests judge their own folder.
    rule: fn(&Path, u64, &links::Markers) -> links::Verdict,
}

impl Settings {
    const APP: Settings = Settings {
        gap: Duration::from_millis(100),
        rename: true,
        junction: create_junction,
        hard_link: create_hard_link,
        bin: to_bin,
        rule: links::verdict,
    };
}

struct Entry {
    info: JobInfo,
    cancel: Arc<AtomicBool>,
    /// Set while the job waits on a conflict; only then is an answer taken.
    waiting: bool,
    /// The conflict it waits on, to send again.
    pending: Option<Conflict>,
    answer: Option<(Choice, bool)>,
    /// A plan's step started it (`move_and_wait` and the like), not the user.
    for_plan: bool,
}

#[derive(Default)]
struct Registry {
    last_id: u64,
    /// Oldest first.
    jobs: Vec<Entry>,
}

impl Registry {
    fn get(&mut self, id: u64) -> Option<&mut Entry> {
        self.jobs.iter_mut().find(|e| e.info.id == id)
    }

    /// Forgets the oldest finished jobs past KEEP_FINISHED.
    fn prune(&mut self) {
        let finished = self.jobs.iter().filter(|e| is_finished(&e.info.state)).count();
        let mut extra = finished.saturating_sub(KEEP_FINISHED);
        self.jobs.retain(|e| {
            let forget = extra > 0 && is_finished(&e.info.state);
            extra -= forget as usize;
            !forget
        });
    }
}

fn is_finished(state: &str) -> bool {
    matches!(state, "done" | "failed" | "cancelled")
}

struct Jobs {
    registry: Mutex<Registry>,
    /// Signalled when a conflict is answered or a job is cancelled.
    answered: Condvar,
    /// Signalled when a job has finished.
    ended: Condvar,
}

fn jobs_state() -> &'static Jobs {
    static JOBS: OnceLock<Jobs> = OnceLock::new();
    JOBS.get_or_init(|| Jobs { registry: Mutex::default(), answered: Condvar::new(), ended: Condvar::new() })
}

/// The registry, also after a thread panicked while holding it: it only
/// holds progress copies and answers, nothing a half-done update could break.
fn registry() -> MutexGuard<'static, Registry> {
    jobs_state().registry.lock().unwrap_or_else(PoisonError::into_inner)
}

fn start(op: &str, sources: Vec<String>, dest: String, events: impl Events, settings: Settings) -> Result<u64, String> {
    let is_move = match op {
        "copy" => false,
        "move" => true,
        _ => return Err(format!("알 수 없는 작업이에요 ({op})")),
    };
    let dest_path = PathBuf::from(&dest);
    // Following a link is fine here: that is the folder the user picked to put things in.
    if !dest_path.is_absolute() || !std::fs::metadata(win::verbatim(&dest_path)).is_ok_and(|m| m.is_dir()) {
        return Err(DEST_MISSING.into());
    }
    let sources = top_level(sources);
    if sources.is_empty() {
        return Err("복사하거나 옮길 항목이 없어요".into());
    }
    launch(new_job(op, is_move, sources, dest, events, settings), Run::work)
}

/// Puts a new job in the registry and gives the Run that does it.
fn new_job(op: &str, is_move: bool, sources: Vec<PathBuf>, dest: String, events: impl Events, settings: Settings) -> Run {
    let cancel = Arc::new(AtomicBool::new(false));
    let dest_path = PathBuf::from(&dest);
    let mut info = JobInfo {
        id: 0,
        op: op.into(),
        state: "running".into(),
        dest,
        total_bytes: 0,
        done_bytes: 0,
        total_items: 0,
        done_items: 0,
        current: None,
        speed_bps: 0,
        errors: Vec::new(),
        error_count: 0,
        freed_bytes: 0,
    };
    {
        let mut reg = registry();
        reg.last_id += 1;
        info.id = reg.last_id;
        reg.jobs.push(Entry { info: info.clone(), cancel: cancel.clone(), waiting: false, pending: None, answer: None, for_plan: false });
        reg.prune();
    }
    Run {
        id: info.id,
        is_move,
        sources,
        dest: dest_path,
        events: Box::new(events),
        cancel,
        settings,
        info,
        last_event: None,
        speed: Speed::new(),
        always: None,
        deep: true,
        handled: 0,
        to_remove: Vec::new(),
        gave_up: false,
        keep_security: false,
    }
}

/// Runs a new job on a thread of its own, doing `work`. Gives its id.
fn launch(run: Run, work: impl FnOnce(&mut Run) -> Result<(), Stop> + Send + 'static) -> Result<u64, String> {
    let id = run.id;
    let spawned = std::thread::Builder::new().name(format!("job {id}")).spawn(move || run.go(work));
    if let Err(e) = spawned {
        registry().jobs.retain(|j| j.info.id != id);
        return Err(format!("작업을 시작할 수 없어요: {e}"));
    }
    Ok(id)
}

/// The sources to work on, in the order given. Repeats are dropped, and so
/// are items inside another source: they go along with it.
fn top_level(sources: Vec<String>) -> Vec<PathBuf> {
    let key = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let all: HashSet<String> = sources.iter().map(|s| key(Path::new(s))).collect();
    let mut seen = HashSet::new();
    sources
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| !p.ancestors().skip(1).any(|a| all.contains(&key(a))) && seen.insert(key(p)))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    File,
    Dir,
    /// A junction or symbolic link (to a folder when `dir`). Never followed.
    Link { dir: bool },
}

impl Kind {
    fn is_dir(self) -> bool {
        matches!(self, Kind::Dir | Kind::Link { dir: true })
    }

    fn is_link(self) -> bool {
        matches!(self, Kind::Link { .. })
    }
}

/// From `symlink_metadata` or a listing. std takes junctions and symbolic
/// links for links (name surrogates) but not cloud-files placeholders, whose
/// contents are really there and get copied.
fn kind_of(m: &Metadata) -> Kind {
    let dir = m.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0;
    if m.file_type().is_symlink() {
        Kind::Link { dir }
    } else if dir {
        Kind::Dir
    } else {
        Kind::File
    }
}

fn file_len(m: &Metadata) -> u64 {
    if kind_of(m) == Kind::File { m.len() } else { 0 }
}

/// The job was cancelled.
#[derive(Debug)]
struct Stop;

/// A source a move across volumes lets go of once its copy is done.
struct Removal {
    path: PathBuf,
    kind: Kind,
    /// A file's size and modified time right after it was copied; it is
    /// kept if either has changed since.
    stamp: (u64, u64),
    /// Which entry it was when it was copied (as opened in its folder,
    /// `Within`, else `key_at`), where that can be told: one found at its
    /// path later must be the same, not one a folder above it, swapped for a
    /// junction or set as a mount point meanwhile, leads to.
    key: Option<dups::FileKey>,
}

/// An entry opened in the folder that holds it (`open_child`), and that
/// folder: what a folder's children carry (`put_dir`), so each is dealt with
/// as the very entry its folder holds, not as whatever a path to it leads to
/// now. A held folder cannot be renamed, but an empty one can be set as a
/// mount point in place, which a path through it would follow.
#[derive(Clone, Copy)]
struct Within<'a> {
    /// The folder, held open (`put_dir`).
    folder: &'a File,
    /// The entry itself (a link as the link), with READ_CONTROL for a copy
    /// that keeps its permissions and DELETE for a move within the volume.
    entry: &'a File,
}

enum Copied {
    Done,
    /// The name got taken meanwhile; nothing is left of the copy.
    Taken,
    Cancelled,
    /// Why, in Korean; nothing is left of the copy.
    Failed(String),
}

/// Bytes per second, smoothed over the last few seconds.
struct Speed {
    since: Instant,
    bytes: u64,
    rate: f64,
    primed: bool,
}

impl Speed {
    fn new() -> Speed {
        Speed { since: Instant::now(), bytes: 0, rate: 0.0, primed: false }
    }

    fn sample(&mut self, now: Instant, done: u64) -> u64 {
        let dt = now.duration_since(self.since).as_secs_f64();
        if dt >= SPEED_SAMPLE {
            let rate = done.saturating_sub(self.bytes) as f64 / dt;
            self.rate = if self.primed { self.rate + (1.0 - (-dt / SPEED_SMOOTHING).exp()) * (rate - self.rate) } else { rate };
            self.primed = true;
            self.since = now;
            self.bytes = done;
        }
        self.rate as u64
    }

    /// Measures afresh from now, so time spent counting or waiting for an answer does not count.
    fn restart(&mut self, now: Instant, done: u64) {
        self.since = now;
        self.bytes = done;
    }
}

/// One job, on its own thread.
struct Run {
    id: u64,
    is_move: bool,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    events: Box<dyn Events>,
    cancel: Arc<AtomicBool>,
    settings: Settings,
    /// This job's progress; the registry gets a copy with every event.
    info: JobInfo,
    last_event: Option<Instant>,
    speed: Speed,
    /// An answer given "for all conflicts".
    always: Option<Choice>,
    /// Whether the item at hand was counted all the way down. A move within
    /// a volume counts only the entries it renames.
    deep: bool,
    /// Entries transferred, skipped, or found already in place.
    handled: u64,
    /// Sources of a move across volumes to remove once their top-level item
    /// is through. Children come before their folder.
    to_remove: Vec<Removal>,
    /// The job could not do what it is for and undid what it had done (a
    /// relocation): it ends as failed, whatever it got through on the way.
    gave_up: bool,
    /// What is copied keeps its permissions (`keep_own_security`): a
    /// relocation and its undo, whose copy takes the folder's place.
    keep_security: bool,
}

impl Run {
    fn go(mut self, work: impl FnOnce(&mut Run) -> Result<(), Stop>) {
        let worked = catch_unwind(AssertUnwindSafe(|| work(&mut self)));
        let state = match worked {
            // Given up, or nothing went anywhere and only because of errors.
            Ok(Ok(())) if self.gave_up || (!self.info.errors.is_empty() && self.handled == 0) => "failed",
            Ok(Ok(())) => "done",
            Ok(Err(Stop)) => "cancelled",
            Err(_) => {
                let dest = self.dest.clone();
                self.error(&dest, "예기치 못한 문제로 작업을 멈췄어요");
                "failed"
            }
        };
        self.finish(state);
    }

    fn work(&mut self) -> Result<(), Stop> {
        self.publish();
        // First pass: what there is to do, and how much.
        let mut plan = Vec::new();
        for src in std::mem::take(&mut self.sources) {
            self.check()?;
            if !src.is_absolute() {
                self.error(&src, BAD_PATH);
                continue;
            }
            let Some(name) = src.file_name().map(OsStr::to_os_string) else {
                self.error(&src, WHOLE_DRIVE);
                continue;
            };
            let meta = match std::fs::symlink_metadata(win::verbatim(&src)) {
                Ok(m) => m,
                Err(e) => {
                    self.error(&src, reason(&e));
                    continue;
                }
            };
            if kind_of(&meta) == Kind::Dir && inside(&self.dest, &src) {
                self.error(&src, INTO_ITSELF);
                continue;
            }
            let deep = !(self.is_move && self.settings.rename && win::same_volume(&src, &self.dest));
            self.measure(&src, &meta, deep, true)?;
            plan.push((src, name, meta, deep));
        }
        self.publish();
        // The speed starts with the first bytes, not from zero during the count.
        self.speed = Speed::new();
        for (src, name, meta, deep) in plan {
            self.deep = deep;
            let target = self.dest.join(&name);
            if let Err(stop) = self.transfer(&src, &meta, target, None) {
                // The sources of an item cut short stay where they are.
                self.to_remove.clear();
                return Err(stop);
            }
            self.remove_moved();
        }
        Ok(())
    }

    fn check(&self) -> Result<(), Stop> {
        if self.cancel.load(Ordering::SeqCst) { Err(Stop) } else { Ok(()) }
    }

    /// Bytes and entries in `path`: only the entry itself unless `deep`, and
    /// never through a link. With `count` they go into the job's totals as
    /// they are found.
    fn measure(&mut self, path: &Path, meta: &Metadata, deep: bool, count: bool) -> Result<(u64, u64), Stop> {
        let mut sum = (file_len(meta), 1);
        if count {
            self.info.total_bytes += sum.0;
            self.info.total_items += 1;
        }
        if !deep || kind_of(meta) != Kind::Dir {
            return Ok(sum);
        }
        let cancel = self.cancel.clone();
        let finished = walk(path, &cancel, |bytes, items| {
            sum.0 += bytes;
            sum.1 += items;
            if count {
                self.info.total_bytes += bytes;
                self.info.total_items += items;
                self.tick();
            }
        });
        if finished { Ok(sum) } else { Err(Stop) }
    }

    /// Copies or moves the entry `src` to `target`, a folder with everything
    /// in it. Ok(true) when all of it got there, so a move may let go of the
    /// source. `within` is `src` opened in the folder that holds it, where
    /// that folder is held (a folder's children carry it, `meta` read from
    /// it): `src` is then dealt with through those handles, not down a path,
    /// so a folder swapped for a mount point is never entered nor copied or
    /// removed from. None at the top, where there is no held folder.
    fn transfer(&mut self, src: &Path, meta: &Metadata, mut target: PathBuf, within: Option<Within>) -> Result<bool, Stop> {
        self.check()?;
        self.info.current = Some(src.to_string_lossy().into_owned());
        self.tick();
        let kind = kind_of(meta);
        // A name that gets taken between the look and the copy is asked about again.
        for _ in 0..3 {
            let mut merge = false;
            if let Ok(existing) = std::fs::symlink_metadata(win::verbatim(&target)) {
                let choice = if win::same_entry(src, &target) {
                    // Pasted where it already is: a copy takes the next free
                    // name, a move has nothing to do.
                    if self.is_move {
                        self.handled += 1;
                        self.pass_over(src, meta)?;
                        return Ok(false);
                    }
                    Choice::Rename
                } else {
                    self.ask(src, meta, &target, &existing)?
                };
                match choice {
                    Choice::Skip => {
                        self.handled += 1;
                        self.pass_over(src, meta)?;
                        return Ok(false);
                    }
                    Choice::Rename => match free_name(&target, kind.is_dir()) {
                        Some(free) => target = free,
                        None => return self.give_up(src, meta, NO_FREE_NAME.into()),
                    },
                    // Explorer does not put a file in place of a folder either (or the other
                    // way round), and a real folder never goes to the bin to make room for a link.
                    Choice::Overwrite if kind != kind_of(&existing) => return self.give_up(src, meta, KIND_DIFFERS.into()),
                    // Folder onto folder: the contents go in, each name asked about on its own.
                    Choice::Overwrite if kind == Kind::Dir => merge = true,
                    Choice::Overwrite => {
                        if let Err(why) = recycle(src, &target, &existing) {
                            return self.give_up(src, meta, why);
                        }
                    }
                    Choice::Cancel => return Err(Stop),
                }
            }
            if let Some(complete) = self.put(src, meta, kind, &target, merge, within)? {
                return Ok(complete);
            }
        }
        self.give_up(src, meta, NAME_TAKEN.into())
    }

    /// Records why `src` was not transferred and counts it as dealt with.
    fn give_up(&mut self, src: &Path, meta: &Metadata, why: String) -> Result<bool, Stop> {
        self.error(src, why);
        self.pass_over(src, meta)?;
        Ok(false)
    }

    /// Counts `src` (with what is inside, as far as it was counted) as dealt
    /// with although nothing was done with it.
    fn pass_over(&mut self, src: &Path, meta: &Metadata) -> Result<(), Stop> {
        let (bytes, items) = self.measure(src, meta, self.deep, false)?;
        self.info.done_bytes += bytes;
        self.info.done_items += items;
        self.tick();
        Ok(())
    }

    /// Puts `src` at `target`, which is free (or the folder to merge into).
    /// None when the name got taken meanwhile. `within`: `src` as opened in
    /// its held folder (`transfer`).
    fn put(&mut self, src: &Path, meta: &Metadata, kind: Kind, target: &Path, merge: bool, within: Option<Within>) -> Result<Option<bool>, Stop> {
        if self.is_move && !self.deep && !merge {
            // The very entry its folder holds, through its handle, where there is one.
            let renamed = match within {
                Some(w) => rename_open(w.entry, target),
                None => rename_entry(src, target),
            };
            match renamed {
                Ok(()) => {
                    self.info.done_bytes += file_len(meta);
                    self.info.done_items += 1;
                    self.handled += 1;
                    self.tick();
                    return Ok(Some(true));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
                Err(e) if e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE as i32) => {
                    // Another volume after all (a junction or mount point on
                    // the way): copy it, counting all that is in it.
                    let (bytes, items) = self.measure(src, meta, true, false)?;
                    self.info.total_bytes += bytes - file_len(meta);
                    self.info.total_items += items - 1;
                    self.deep = true;
                    let put = self.put(src, meta, kind, target, merge, within);
                    self.deep = false;
                    return put;
                }
                Err(e) => {
                    self.give_up(src, meta, reason(&e))?;
                    return Ok(Some(false));
                }
            }
        }
        match kind {
            Kind::File => self.put_file(src, meta, target, within.map(|w| w.entry)),
            Kind::Link { dir } => self.put_link(src, target, dir, within.map(|w| w.entry)),
            Kind::Dir => self.put_dir(src, meta, target, merge, within),
        }
    }

    /// `entry`: `src` as opened in its held folder (`Within`), where it is.
    fn put_file(&mut self, src: &Path, meta: &Metadata, target: &Path, entry: Option<&File>) -> Result<Option<bool>, Stop> {
        // Which file it is, where that can be told from its folder: what is copied must be that very file.
        let expect = entry.and_then(dups::facts).and_then(|f| f.key);
        match self.copy_file(src, meta, target, expect) {
            Copied::Done => {}
            Copied::Taken => return Ok(None),
            Copied::Cancelled => return Err(Stop),
            Copied::Failed(why) => {
                self.error(src, why);
                return Ok(Some(false));
            }
        }
        self.handled += 1;
        if !self.keep_own_security(src, target, entry) {
            return Ok(Some(false));
        }
        if self.is_move {
            // Opened, not listed: a listing's size can lag behind for a file still being written.
            let now = match entry {
                Some(e) => dups::facts(e).map(|f| (f.size, f.modified as u64)),
                None => std::fs::symlink_metadata(win::verbatim(src)).ok().map(|m| (m.len(), m.last_write_time())),
            };
            match (now, std::fs::symlink_metadata(win::verbatim(target))) {
                (Some(now), Ok(copy)) if now.0 == copy.len() => self.to_remove.push(Removal {
                    path: src.to_path_buf(),
                    kind: Kind::File,
                    stamp: now,
                    key: if entry.is_some() { expect } else { key_at(src) },
                }),
                _ => {
                    self.error(src, SIZE_MISMATCH);
                    return Ok(Some(false));
                }
            }
        }
        Ok(Some(true))
    }

    /// `entry`: `src` as opened in its held folder (`Within`), where it is.
    fn put_link(&mut self, src: &Path, target: &Path, dir: bool, entry: Option<&File>) -> Result<Option<bool>, Stop> {
        // Which link it is, as its folder holds it: a path to it that leads to another now is not copied.
        let key = match entry {
            Some(e) => dups::facts(e).and_then(|f| f.key),
            None => key_at(src),
        };
        if entry.is_some() && key_at(src) != key {
            self.info.done_items += 1;
            self.tick();
            self.error(src, ENTRY_CHANGED);
            return Ok(Some(false));
        }
        let made = copy_link(src, target, dir);
        if matches!(&made, Err(e) if e.kind() == io::ErrorKind::AlreadyExists) {
            return Ok(None);
        }
        self.info.done_items += 1;
        self.tick();
        if let Err(e) = made {
            self.error(src, link_reason(&e));
            return Ok(Some(false));
        }
        self.handled += 1;
        if !self.keep_own_security(src, target, entry) {
            return Ok(Some(false));
        }
        if self.is_move {
            self.to_remove.push(Removal { path: src.to_path_buf(), kind: Kind::Link { dir }, stamp: (0, 0), key });
        }
        Ok(Some(true))
    }

    /// With `keep_security` (a relocation and its undo): gives the copy
    /// `target` the DACL of `src` (`entry`, as opened in its folder, where it
    /// is), its own or all passed down, as it is (`copy_security`): the
    /// copy's folders are their owner's alone until all they hold is in
    /// (`FILLING_SDDL`), so nothing gets what it keeps from them. False, with
    /// why in the errors, when that cannot be done.
    fn keep_own_security(&mut self, src: &Path, target: &Path, entry: Option<&File>) -> bool {
        if !self.keep_security {
            return true;
        }
        let copied = match entry {
            Some(from) => copy_security(from, target, true),
            None => open_itself(src, READ_CONTROL, false).and_then(|from| copy_security(&from, target, true)),
        };
        if let Err(e) = copied {
            self.error(src, format!("{NO_SECURITY_COPY}: {}", reason(&e)));
            return false;
        }
        true
    }

    fn put_dir(&mut self, src: &Path, meta: &Metadata, target: &Path, merge: bool, within: Option<Within>) -> Result<Option<bool>, Stop> {
        if !merge {
            let wide = win::wide_path(target);
            if unsafe { CreateDirectoryW(wide.as_ptr(), null()) } == 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::AlreadyExists {
                    return Ok(None);
                }
                self.give_up(src, meta, format!("폴더를 만들 수 없어요: {}", reason(&e)))?;
                return Ok(Some(false));
            }
        }
        self.info.done_items += 1;
        self.handled += 1;
        self.tick();
        let mut complete = true;
        // Held open as itself while what is in it is copied (`open_itself`): neither it nor
        // a folder above it can be renamed meanwhile. Opened through its own parent's handle
        // where there is one (`open_child`), so this very folder is reached, not one a swap on
        // the way might lead to. One that became a link since it was listed is not entered.
        // Listed through that handle too (`names_in`), and each entry opened in it (`open_child`):
        // an empty folder can still be set as a mount point in place while it is held, and a
        // path through it would then lead elsewhere. No FILE_TRAVERSE: an open relative to a
        // handle needs none, and asking for it would only turn down folders that do not grant it.
        let access = FILE_LIST_DIRECTORY | if self.keep_security { READ_CONTROL } else { 0 };
        let opened = match (within, src.file_name()) {
            (Some(w), Some(name)) => open_child(w.folder, name, access, true),
            _ => open_itself(src, access, true),
        };
        let held = opened.and_then(|dir| kind_open(&dir).map(|(kind, _)| (dir, kind)));
        let mut key = None;
        let listed = match held {
            Err(e) => Err(format!("폴더를 열 수 없어요: {}", reason(&e))),
            Ok((_, kind)) if kind != Kind::Dir => Err(TURNED_LINK.to_string()),
            Ok((dir, _)) => {
                key = dups::facts(&dir).and_then(|f| f.key);
                names_in(&dir).map(|names| (dir, names)).map_err(|e| format!("폴더를 열 수 없어요: {}", reason(&e)))
            }
        };
        let held = match listed {
            Err(why) => {
                self.error(src, why);
                complete = false;
                None
            }
            Ok((held, names)) => {
                // Each entry with what it is dealt with by: its permissions read (a copy that
                // keeps them), renamed (a move within the volume).
                let rights = if self.keep_security { READ_CONTROL } else { 0 } | if self.is_move && !self.deep { DELETE } else { 0 };
                for (name, listed_dir) in names {
                    // Not a listing's path, which has its `\\?\`: paths are shown as they were given.
                    let child_src = src.join(&name);
                    // In this folder, as itself, through its handle: never down a path.
                    let (entry, child) = match open_child(&held, &name, rights, false).and_then(|f| f.metadata().map(|m| (f, m))) {
                        Ok(c) => c,
                        Err(e) => {
                            self.error(&child_src, reason(&e));
                            complete = false;
                            continue;
                        }
                    };
                    // A folder that is a link now (swapped since it was listed): not entered, nor taken for a link of the user's.
                    if listed_dir && kind_of(&child).is_link() {
                        self.error(&child_src, TURNED_LINK);
                        complete = false;
                        continue;
                    }
                    // A merge within a volume counted only the folder; its contents count as they come.
                    if !self.deep {
                        self.info.total_bytes += file_len(&child);
                        self.info.total_items += 1;
                    }
                    let within = Within { folder: &held, entry: &entry };
                    complete &= self.transfer(&child_src, &child, target.join(&name), Some(within))?;
                }
                Some(held)
            }
        };
        if !merge {
            copy_folder_look(meta, target);
        }
        // A copy that keeps its permissions: the folder's own go on last, once all it holds
        // is in with its own (from the bottom up). Until then it is its owner's alone, as the
        // whole copy is (`FILLING_SDDL`): nothing in it is open to more than that meanwhile,
        // and a folder that lets nobody add to it is filled all the same.
        if self.keep_security && !merge
            && let Some(dir) = &held
            && let Err(e) = copy_security(dir, target, true)
        {
            self.error(src, format!("{NO_SECURITY_COPY}: {}", reason(&e)));
            complete = false;
        }
        if self.is_move && complete {
            self.to_remove.push(Removal { path: src.to_path_buf(), kind: Kind::Dir, stamp: (0, 0), key });
        }
        Ok(Some(complete))
    }

    /// CopyFileEx: contents, attributes, timestamps and alternate streams,
    /// never onto a name that is taken. It writes to a hidden partial file
    /// next to `target` (`partial_name`), which is renamed to `target`, again
    /// never over anything, once the copy is whole: a copy cut short by a
    /// crash or a kill never has the real name. Cancel takes effect at the
    /// next progress callback; the partial file is removed on cancel and on
    /// failure. With `expect` (which file `src` is, as its folder holds it),
    /// the file CopyFileEx opened must be that one before anything of it is
    /// written (`copy_progress`): a path that leads elsewhere now is not
    /// copied from.
    fn copy_file(&mut self, src: &Path, meta: &Metadata, target: &Path, expect: Option<dups::FileKey>) -> Copied {
        let len = meta.len();
        let base = self.info.done_bytes;
        // Taken since it was looked at: asked about again, before anything is written.
        if std::fs::symlink_metadata(win::verbatim(target)).is_ok() {
            return Copied::Taken;
        }
        let from = win::wide_path(src);
        let mut failed = NO_PARTIAL_NAME.to_string();
        for n in 1..=PARTIAL_TRIES {
            let Some(partial) = partial_name(target, n) else { break };
            let mut p = CopyProgress::new(self, base, len, &partial, expect);
            let ok = unsafe {
                CopyFileExW(from.as_ptr(), p.partial.as_ptr(), Some(copy_progress), (&raw mut p).cast(), null_mut(), COPY_FLAGS)
            } != 0;
            let err = io::Error::last_os_error();
            let (started, other, checked, wide) = (p.started, p.other, p.checked, p.partial);
            // Not seen by the callback (it is called for each stream, an empty one too, so this
            // is the odd case): asked of its path at least, before the copy gets the name.
            let unseen = ok && expect.is_some() && !checked && key_at(src) != expect;
            if ok && !unseen {
                return self.name_copy(&partial, &wide, meta, target, base);
            }
            // Once started, the partial file is our own unfinished copy: the
            // copy only begins on a name that was free.
            if started || unseen {
                remove_partial(&partial);
            }
            if other || unseen {
                failed = ENTRY_CHANGED.into();
                break;
            }
            if self.check().is_err() || err.raw_os_error() == Some(ERROR_REQUEST_ABORTED as i32) {
                self.info.done_bytes = base;
                return Copied::Cancelled;
            }
            // Someone else's partial file (another job's, or one a crash left): never touched.
            if !started && err.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            failed = reason(&err);
            break;
        }
        self.info.done_bytes = base + len;
        self.info.done_items += 1;
        self.tick();
        Copied::Failed(failed)
    }

    /// Gives the whole copy at `partial` the source's attributes (hidden only
    /// if that is) and its real name, never over anything. Removes it when
    /// that cannot be done.
    fn name_copy(&mut self, partial: &Path, wide: &[u16], meta: &Metadata, target: &Path, base: u64) -> Copied {
        let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
        let want = (attrs & SETTABLE & !FILE_ATTRIBUTE_HIDDEN) | (meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN);
        if attrs != INVALID_FILE_ATTRIBUTES && attrs & SETTABLE != want {
            unsafe { SetFileAttributesW(wide.as_ptr(), if want == 0 { FILE_ATTRIBUTE_NORMAL } else { want }) };
        }
        let to = win::wide_path(target);
        if unsafe { MoveFileExW(wide.as_ptr(), to.as_ptr(), 0) } == 0 {
            let e = io::Error::last_os_error();
            remove_partial(partial);
            if e.kind() == io::ErrorKind::AlreadyExists {
                self.info.done_bytes = base;
                return Copied::Taken;
            }
            self.info.done_bytes = base + meta.len();
            self.info.done_items += 1;
            self.tick();
            return Copied::Failed(reason(&e));
        }
        self.info.done_bytes = base + meta.len();
        self.info.done_items += 1;
        self.tick();
        Copied::Done
    }

    /// Asks the user (or uses the answer given for all) what to do about
    /// `target`, which is in the way of `src`.
    fn ask(&mut self, src: &Path, meta: &Metadata, target: &Path, existing: &Metadata) -> Result<Choice, Stop> {
        let always = match self.always {
            // "Overwrite" for all was given for items of one kind. A folder in the way of a
            // file, or a link in the way of a folder, cannot be overwritten: asked on its own.
            Some(Choice::Overwrite) if kind_of(meta) != kind_of(existing) => None,
            always => always,
        };
        let choice = match always {
            Some(c) => c,
            None => self.wait_for_answer(Conflict {
                job: self.id,
                source: src.to_string_lossy().into_owned(),
                target: target.to_string_lossy().into_owned(),
                is_dir: kind_of(meta).is_dir(),
                target_is_dir: kind_of(existing).is_dir(),
                is_link: kind_of(meta).is_link(),
                target_is_link: kind_of(existing).is_link(),
                source_size: file_len(meta),
                target_size: file_len(existing),
                source_modified: meta.last_write_time() as i64,
                target_modified: existing.last_write_time() as i64,
            })?,
        };
        if choice == Choice::Cancel {
            self.cancel.store(true, Ordering::SeqCst);
            return Err(Stop);
        }
        Ok(choice)
    }

    fn wait_for_answer(&mut self, conflict: Conflict) -> Result<Choice, Stop> {
        if let Some(e) = registry().get(self.id) {
            e.waiting = true;
            e.pending = Some(conflict.clone());
            e.answer = None;
        }
        self.info.state = "waiting".into();
        self.publish();
        self.events.conflict(&conflict);
        let mut reg = registry();
        let answer = loop {
            let Some(e) = reg.get(self.id) else { break None };
            if self.cancel.load(Ordering::SeqCst) {
                e.waiting = false;
                e.pending = None;
                break None;
            }
            if let Some(a) = e.answer.take() {
                e.waiting = false;
                e.pending = None;
                break Some(a);
            }
            reg = jobs_state().answered.wait(reg).unwrap_or_else(PoisonError::into_inner);
        };
        drop(reg);
        let (choice, for_all) = answer.ok_or(Stop)?;
        if for_all {
            self.always = Some(choice);
        }
        self.info.state = "running".into();
        self.speed.restart(Instant::now(), self.info.done_bytes);
        self.publish();
        Ok(choice)
    }

    /// Lets go of the sources a move across volumes has copied, children first.
    fn remove_moved(&mut self) {
        let mut kept: Vec<PathBuf> = Vec::new();
        for r in std::mem::take(&mut self.to_remove) {
            // A folder still holding something that stayed is no news.
            if r.kind == Kind::Dir && kept.iter().any(|k| k.starts_with(&r.path)) {
                kept.push(r.path);
                continue;
            }
            if let Err(why) = remove_source(&r) {
                self.error(&r.path, why);
                kept.push(r.path);
            }
        }
    }

    /// Records a problem. Past MAX_ERRORS it is only counted: `error_count`
    /// tells the window how many more there are, and it says so (once).
    fn error(&mut self, path: &Path, why: impl Into<String>) {
        self.info.error_count += 1;
        if self.info.errors.len() < MAX_ERRORS {
            self.info.errors.push((path.to_string_lossy().into_owned(), why.into()));
        }
    }

    /// Sends progress if the last event is long enough ago.
    fn tick(&mut self) {
        if self.last_event.is_none_or(|t| t.elapsed() >= self.settings.gap) {
            self.publish();
        }
    }

    /// Stores the job's progress for `jobs()` and sends it to the window.
    fn publish(&mut self) {
        let now = Instant::now();
        self.last_event = Some(now);
        self.info.speed_bps = if self.info.state == "running" { self.speed.sample(now, self.info.done_bytes) } else { 0 };
        if let Some(e) = registry().get(self.id) {
            e.info = self.info.clone();
        }
        let errors = &self.info.errors;
        if is_finished(&self.info.state) || errors.len() <= EVENT_ERRORS {
            self.events.progress(&self.info);
            return;
        }
        let newest = errors[errors.len() - EVENT_ERRORS..].to_vec();
        let all = std::mem::replace(&mut self.info.errors, newest);
        self.events.progress(&self.info);
        self.info.errors = all;
    }

    fn finish(&mut self, state: &str) {
        self.info.state = state.into();
        self.info.current = None;
        self.publish();
        registry().prune();
        jobs_state().ended.notify_all();
    }
}

/// What a job's CopyFileEx calls back with (`copy_progress`).
struct CopyProgress {
    run: *mut Run,
    base: u64,
    len: u64,
    /// The file being written, NUL-terminated (`win::wide_path`).
    partial: Vec<u16>,
    /// The copy has made the file; until then a failure wrote nothing.
    started: bool,
    /// Which file the copy must be made of (`Run::copy_file`), if that is told.
    expect: Option<dups::FileKey>,
    /// The file CopyFileEx opened was looked at against `expect`...
    checked: bool,
    /// ...and was another: the copy was called off before any of it was written.
    other: bool,
}

impl CopyProgress {
    fn new(run: &mut Run, base: u64, len: u64, partial: &Path, expect: Option<dups::FileKey>) -> CopyProgress {
        CopyProgress { run, base, len, partial: win::wide_path(partial), started: false, expect, checked: false, other: false }
    }
}

/// CopyFileEx's progress: the file it writes hidden from the start, the
/// file it reads checked against the one expected before anything of it is
/// written, the job's bytes done, and a cancel at the next block.
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn copy_progress(
    _total: i64,
    done: i64,
    _stream_size: i64,
    _stream_done: i64,
    _stream: u32,
    _reason: u32,
    from: HANDLE,
    _to: HANDLE,
    data: *const c_void,
) -> u32 {
    // SAFETY: `data` is the CopyProgress of the caller, alive for the whole
    // copy, and the Run is not touched elsewhere while it runs.
    let p = unsafe { &mut *(data as *mut CopyProgress) };
    if !p.started {
        p.started = true;
        // Hidden while unfinished. Attributes can be changed while CopyFileEx
        // has the file open: share modes do not cover them.
        let attrs = unsafe { GetFileAttributesW(p.partial.as_ptr()) };
        if attrs != INVALID_FILE_ATTRIBUTES {
            unsafe { SetFileAttributesW(p.partial.as_ptr(), (attrs & SETTABLE) | FILE_ATTRIBUTE_HIDDEN) };
        }
        // The first call comes as the first stream is about to be copied: nothing of it is
        // written yet. CopyFileEx's own handle, borrowed (it closes it).
        if let Some(want) = p.expect {
            let source = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(from as RawHandle) });
            p.checked = true;
            if dups::facts(&source).and_then(|f| f.key) != Some(want) {
                p.other = true;
                return PROGRESS_CANCEL;
            }
        }
    }
    let run = unsafe { &mut *p.run };
    run.info.done_bytes = p.base + (done.max(0) as u64).min(p.len);
    run.tick();
    if run.cancel.load(Ordering::SeqCst) { PROGRESS_CANCEL } else { PROGRESS_CONTINUE }
}

/// Sends `target` to the Recycle Bin to make room for `src`, or says why it
/// stays. Never deletes for good: the same checks as `trash_paths`.
fn recycle(src: &Path, target: &Path, existing: &Metadata) -> Result<(), String> {
    // The shell would take `abc.` for `abc` and put that one in the bin.
    if !win::shell_can_name(target) {
        return Err(ODD_NAME.into());
    }
    // A folder that holds the source would take the source along.
    if kind_of(existing) == Kind::Dir && inside(src, target) {
        return Err(HOLDS_SOURCE.into());
    }
    let size = || {
        let mut size = file_len(existing);
        if kind_of(existing) == Kind::Dir {
            walk(target, &AtomicBool::new(false), |bytes, _| size += bytes);
        }
        size
    };
    if let Some(why) = binless(target, size) {
        return Err(match why {
            Binless::OddName => ODD_NAME.into(),
            Binless::NoBin => NO_RECYCLE_BIN.into(),
            Binless::Disabled => BIN_DISABLED.into(),
            Binless::TooBig(max) => format!("{BIN_TOO_SMALL} (휴지통 최대 {}MB)", max >> 20),
            Binless::Unknown => NOT_RECYCLED.into(),
        });
    }
    to_bin(target).map_err(|why| format!("{NOT_RECYCLED}: {why}"))
}

/// Why an item would not go to the Recycle Bin: Windows would delete it for
/// good instead, or the shell would take another item.
enum Binless {
    /// A name on the way ends in a dot or a space: the shell would take
    /// `abc.` for `abc`.
    OddName,
    /// Not on a local fixed disk (a network share, a USB stick, a SUBST drive).
    NoBin,
    /// The bin is switched off there.
    Disabled,
    /// Bigger than the bin: its limit in bytes.
    TooBig(u64),
    /// Where it really is cannot be told.
    Unknown,
}

/// Why `target` cannot go to the Recycle Bin, if so: the checks of
/// `trash_paths`, on the path as given and where it really is. `size` (its
/// bytes) is asked only when the bin has a limit.
fn binless(target: &Path, size: impl FnOnce() -> u64) -> Option<Binless> {
    if !win::shell_can_name(target) {
        return Some(Binless::OddName);
    }
    // trash resolves junctions in the folders above the item, so the bin that
    // counts is the one of the drive the item really is on. Both must do.
    let real = target
        .parent()
        .and_then(|p| std::fs::canonicalize(win::verbatim(p)).ok())
        .zip(target.file_name())
        .map(|(parent, name)| win::long_path(&parent.join(name).to_string_lossy()));
    let Some(real) = real else { return Some(Binless::Unknown) };
    let mut limit: Option<u64> = None;
    for t in [target.to_string_lossy().into_owned(), real] {
        if !win::has_recycle_bin(&t) {
            return Some(Binless::NoBin);
        }
        // On a disk that has a Recycle Bin, Windows still deletes for good (after
        // its own prompt) when the bin is switched off or the item is bigger than the bin.
        let policy = win::recycle_policy(&t);
        if policy.nuke {
            return Some(Binless::Disabled);
        }
        if let Some(max) = policy.max_bytes {
            limit = Some(limit.map_or(max, |l| l.min(max)));
        }
    }
    limit.filter(|&max| size() > max).map(Binless::TooBig)
}

/// Sends `target` to the Recycle Bin, once `binless` found nothing in the
/// way, and checks it is gone from where it was. Fails with the shell's
/// reason.
fn to_bin(target: &Path) -> Result<(), String> {
    trash::delete(target).map_err(|e| e.to_string())?;
    if std::fs::symlink_metadata(win::verbatim(target)).is_ok() {
        return Err("그대로 남아 있어요".into());
    }
    Ok(())
}

/// Walks the folder `path` without following links, reporting the file
/// bytes and the entries of each folder in it. False when `cancel` was set.
fn walk(path: &Path, cancel: &AtomicBool, each: impl FnMut(u64, u64)) -> bool {
    matches!(walk_until(path, cancel, |_, _| false, each), Walked::Done)
}

/// How `walk_until` ended.
enum Walked {
    Done,
    Cancelled,
    /// At this entry, which `refuse` turned down.
    Refused(PathBuf),
}

/// `walk`, stopping at the first entry `refuse` turns down (given the entry
/// and what its listing says, which opens nothing).
fn walk_until(path: &Path, cancel: &AtomicBool, mut refuse: impl FnMut(&DirEntry, &Metadata) -> bool, mut each: impl FnMut(u64, u64)) -> Walked {
    // The folders found below keep the `\\?\` of their listing.
    let mut stack = vec![win::verbatim(path)];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::SeqCst) {
            return Walked::Cancelled;
        }
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let (mut bytes, mut items) = (0, 0);
        for entry in read.flatten() {
            // On Windows this comes from the listing itself: no extra system call per file.
            let Ok(m) = entry.metadata() else { continue };
            if refuse(&entry, &m) {
                return Walked::Refused(entry.path());
            }
            items += 1;
            match kind_of(&m) {
                Kind::File => bytes += m.len(),
                Kind::Dir => stack.push(entry.path()),
                Kind::Link { .. } => {}
            }
        }
        each(bytes, items);
    }
    Walked::Done
}

/// Whether `inner` is the folder `outer` or somewhere in it, also when one of
/// them is reached through a junction or a short (8.3) name.
fn inside(inner: &Path, outer: &Path) -> bool {
    let text = |p: &Path| format!("{}\\", win::long_path(&p.to_string_lossy()).trim_end_matches('\\').to_lowercase());
    if text(inner).starts_with(&text(outer)) {
        return true;
    }
    // Final paths have each name as it is on disk, so they compare exactly.
    match (std::fs::canonicalize(win::verbatim(inner)), std::fs::canonicalize(win::verbatim(outer))) {
        (Ok(i), Ok(o)) => i.starts_with(&o),
        _ => false,
    }
}

/// `name (2).ext`, `name (3).ext`, ...: the first free name next to `target`.
/// A folder's name is not split at a dot.
fn free_name(target: &Path, is_dir: bool) -> Option<PathBuf> {
    let name = target.file_name()?;
    let as_path = Path::new(name);
    let (stem, ext) = match (is_dir, as_path.file_stem(), as_path.extension()) {
        (false, Some(stem), Some(ext)) => (stem, Some(ext)),
        _ => (name, None),
    };
    (2..10_000)
        .map(|n| {
            let mut candidate = stem.to_os_string();
            candidate.push(format!(" ({n})"));
            if let Some(ext) = ext {
                candidate.push(".");
                candidate.push(ext);
            }
            target.with_file_name(candidate)
        })
        .find(|p| matches!(std::fs::symlink_metadata(win::verbatim(p)), Err(e) if e.kind() == io::ErrorKind::NotFound))
}

/// `.name.ff-partial` next to `target`, where the copy of a file is written
/// until it is whole; `.name (2).ff-partial` and so on (`n`) when that is
/// taken. A long name is cut so the whole fits in MAX_NAME.
fn partial_name(target: &Path, n: u32) -> Option<PathBuf> {
    marked_name(target, true, PARTIAL_EXT, n)
}

/// `target`'s name with `ext` after it, and a dot in front when `hide`
/// (`.name.ff-partial`, `name.ff-old`), next to it; `name (2)` and so on
/// (`n`) when that is taken. A long name is cut so the whole fits in MAX_NAME.
fn marked_name(target: &Path, hide: bool, ext: &str, n: u32) -> Option<PathBuf> {
    let name: Vec<u16> = target.file_name()?.encode_wide().collect();
    let tail = if n < 2 { ext.to_string() } else { format!(" ({n}){ext}") };
    let tail: Vec<u16> = tail.encode_utf16().collect();
    let mut keep = name.len().min(MAX_NAME - hide as usize - tail.len());
    // Not between the two halves of a surrogate pair.
    if keep < name.len() && keep > 0 && (0xD800..0xDC00).contains(&name[keep - 1]) {
        keep -= 1;
    }
    let mut marked = if hide { vec![b'.' as u16] } else { Vec::new() };
    marked.extend_from_slice(&name[..keep]);
    marked.extend(tail);
    Some(target.with_file_name(OsString::from_wide(&marked)))
}

/// Removes an unfinished copy this job made. A read-only source gives a
/// read-only copy, which would refuse to go.
fn remove_partial(partial: &Path) {
    let wide = win::wide_path(partial);
    unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
    let _ = std::fs::remove_file(win::verbatim(partial));
}

/// Moves within a volume. Never replaces anything.
fn rename_entry(src: &Path, target: &Path) -> io::Result<()> {
    let (from, to) = (win::wide_path(src), win::wide_path(target));
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Gives a folder we made the times and attributes of the one it copies.
/// Done after its contents, which would otherwise bump its modified time.
fn copy_folder_look(meta: &Metadata, target: &Path) {
    use std::fs::FileTimes;
    use std::os::windows::fs::FileTimesExt;
    let dir = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(win::verbatim(target));
    if let Ok(dir) = dir {
        let mut times = FileTimes::new();
        if let Ok(t) = meta.modified() {
            times = times.set_modified(t);
        }
        if let Ok(t) = meta.accessed() {
            times = times.set_accessed(t);
        }
        if let Ok(t) = meta.created() {
            times = times.set_created(t);
        }
        let _ = dir.set_times(times);
    }
    let keep = FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_ARCHIVE
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    let attrs = meta.file_attributes() & keep;
    if attrs != 0 {
        let wide = win::wide_path(target);
        unsafe { SetFileAttributesW(wide.as_ptr(), attrs) };
    }
}

/// Makes `target` a link to wherever `src` points, without going there: a
/// junction gets the same mount point data, a symbolic link the same
/// (relative or absolute) target. Other kinds of link are not copied.
fn copy_link(src: &Path, target: &Path, dir: bool) -> io::Result<()> {
    match win::reparse_tag(src) {
        Some(IO_REPARSE_TAG_SYMLINK) => {
            let mut to: Vec<u16> = std::fs::read_link(win::verbatim(src))?.as_os_str().encode_wide().collect();
            // read_link gives `\\?\C:\x` for `C:\x`; the copy gets the plain form, as mklink makes it.
            let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
            if to.starts_with(&verbatim) && to.get(5) == Some(&(b':' as u16)) {
                to.drain(..4);
            }
            to.push(0);
            let link = win::wide_path(target);
            let flags = SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE | if dir { SYMBOLIC_LINK_FLAG_DIRECTORY } else { 0 };
            if unsafe { CreateSymbolicLinkW(link.as_ptr(), to.as_ptr(), flags) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        Some(IO_REPARSE_TAG_MOUNT_POINT) if dir => copy_junction(src, target),
        _ => Err(io::ErrorKind::Unsupported.into()),
    }
}

/// Junctions need no special rights: an empty folder gets the source's
/// reparse data as it is. A copied junction keeps what its folder passes down,
/// as every other copied entry does (`owner_only` off).
fn copy_junction(src: &Path, target: &Path) -> io::Result<()> {
    make_reparse_folder(target, &read_reparse(src)?, false)
}

/// The reparse data of the link at `path`, read without following it.
fn read_reparse(path: &Path) -> io::Result<Vec<u8>> {
    let link = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(win::verbatim(path))?;
    let mut data = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let mut len = 0u32;
    let got = unsafe {
        DeviceIoControl(
            link.as_raw_handle() as HANDLE,
            FSCTL_GET_REPARSE_POINT,
            null(),
            0,
            data.as_mut_ptr().cast(),
            data.len() as u32,
            &mut len,
            null_mut(),
        )
    };
    if got == 0 {
        return Err(io::Error::last_os_error());
    }
    data.truncate(len as usize);
    Ok(data)
}

/// An owner-only, protected security descriptor (`D:P(A;;FA;;;OW)`): only
/// whoever owns the object may touch it, nothing passed down. A junction a
/// relocation leaves starts life with this, until its recorded permissions go
/// on, so it is never, even for a moment, open to more than its maker.
fn owner_only_sd() -> Option<Security> {
    sddl_to_security("D:P(A;;FA;;;OW)").ok()
}

/// Makes the folder `target`, never over anything, and gives it the reparse
/// data `data` (a junction's). With `owner_only` it is made owner-only and
/// protected (`owner_only_sd`) rather than with what its folder passes down,
/// for a junction whose recorded permissions go on right after. Nothing is
/// left of it when that fails.
fn make_reparse_folder(target: &Path, data: &[u8], owner_only: bool) -> io::Result<()> {
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let wide = win::wide_path(target);
    let lock = owner_only.then(owner_only_sd).flatten();
    let sa = lock.as_ref().map(|sd| SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    });
    let sa_ptr: *const SECURITY_ATTRIBUTES = sa.as_ref().map_or(null(), |s| s);
    if unsafe { CreateDirectoryW(wide.as_ptr(), sa_ptr) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let set = || -> io::Result<()> {
        let dir = OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(win::verbatim(target))?;
        let mut unused = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                dir.as_raw_handle() as HANDLE,
                FSCTL_SET_REPARSE_POINT,
                data.as_ptr().cast(),
                data.len() as u32,
                null_mut(),
                0,
                &mut unused,
                null_mut(),
            )
        };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    };
    // The empty folder made just now is ours to take back.
    set().inspect_err(|_| {
        let _ = std::fs::remove_dir(win::verbatim(target));
    })
}

/// Removes a source a move has copied, if it is still what was copied: the
/// same entry (`Removal::key`), of the same kind, a file of the same size
/// and time. Opened as itself and removed through that very handle, so what
/// goes is what was looked at: a link as the link, never what it points to.
/// Never recursive: a folder goes only once it is empty.
fn remove_source(r: &Removal) -> Result<(), String> {
    let failed = |e: &io::Error| format!("원본을 지우지 못했어요: {}", reason(e));
    let entry = match open_to_delete(&r.path, false) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(failed(&e)),
    };
    let (now, attrs) = kind_open(&entry).map_err(|e| failed(&e))?;
    let facts = dups::facts(&entry);
    let same = r.key.is_none_or(|k| facts.as_ref().and_then(|f| f.key) == Some(k));
    let unchanged = same
        && match r.kind {
            Kind::File => now == Kind::File && facts.as_ref().is_some_and(|f| (f.size, f.modified as u64) == r.stamp),
            kind => now == kind,
        };
    if !unchanged {
        return Err(SOURCE_CHANGED.into());
    }
    delete_open(&entry, attrs).map_err(|e| match e.raw_os_error() {
        Some(c) if c == ERROR_DIR_NOT_EMPTY as i32 => FOLDER_LEFT.into(),
        _ => failed(&e),
    })
}

/// A short Korean reason for a failed file operation; Windows's own message
/// for the rarer ones.
pub(crate) fn reason(e: &io::Error) -> String {
    let Some(code) = e.raw_os_error().map(|c| c as u32) else { return e.to_string() };
    let short = match code {
        ERROR_ACCESS_DENIED => "권한이 없어요",
        ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => "다른 프로그램이 쓰고 있어요",
        ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL => "디스크 공간이 부족해요",
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => "찾을 수 없어요",
        ERROR_FILENAME_EXCED_RANGE => "경로가 너무 길어요",
        // FAT32 holds files up to 4 GB.
        ERROR_FILE_TOO_LARGE => "파일이 너무 커서 이 드라이브에 담을 수 없어요",
        ERROR_WRITE_PROTECT => "쓰기가 막힌 드라이브예요",
        _ => return e.to_string(),
    };
    short.into()
}

fn link_reason(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(c) if c == ERROR_PRIVILEGE_NOT_HELD as i32 => NO_SYMLINK_RIGHT.into(),
        _ if e.kind() == io::ErrorKind::Unsupported => LINK_KIND.into(),
        _ => format!("링크를 만들지 못했어요: {}", reason(e)),
    }
}

/// Not on the window's thread: another program may hold the clipboard for a
/// moment, and Explorer hands over its file list only when asked for it,
/// which waits on Explorer. The clipboard opens and closes on one thread.
#[tauri::command]
pub async fn clipboard_read() -> Result<ClipboardFiles, String> {
    blocking(read_files).await?
}

#[tauri::command]
pub async fn clipboard_write(paths: Vec<String>, cut: bool) -> Result<(), String> {
    blocking(move || write_files(&paths, cut, false)).await?
}

/// The clipboard, open on this thread until dropped.
struct Clipboard;

impl Clipboard {
    fn open() -> Result<Clipboard, String> {
        for _ in 0..CLIPBOARD_TRIES {
            if unsafe { OpenClipboard(null_mut()) } != 0 {
                return Ok(Clipboard);
            }
            std::thread::sleep(CLIPBOARD_RETRY);
        }
        Err(CLIPBOARD_BUSY.into())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

/// Explorer's "copy or cut" flag next to the file list.
fn drop_effect_format() -> u32 {
    unsafe { RegisterClipboardFormatW(win::wide("Preferred DropEffect").as_ptr()) }
}

/// Files on the clipboard (CF_HDROP), and whether they were cut. No files: an empty list.
fn read_files() -> Result<ClipboardFiles, String> {
    let _clipboard = Clipboard::open()?;
    let mut files = ClipboardFiles { paths: Vec::new(), cut: false };
    let list = unsafe { GetClipboardData(CF_HDROP as u32) };
    if list.is_null() {
        return Ok(files);
    }
    let count = unsafe { DragQueryFileW(list, u32::MAX, null_mut(), 0) };
    for i in 0..count {
        let len = unsafe { DragQueryFileW(list, i, null_mut(), 0) };
        let mut buf = vec![0u16; len as usize + 1];
        let n = unsafe { DragQueryFileW(list, i, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n > 0 {
            files.paths.push(String::from_utf16_lossy(&buf[..n.min(len as usize)]));
        }
    }
    let effect = unsafe {
        let h = GetClipboardData(drop_effect_format());
        let p = if h.is_null() || GlobalSize(h) < 4 { null_mut() } else { GlobalLock(h) };
        if p.is_null() {
            None
        } else {
            let v = (p as *const u32).read_unaligned();
            GlobalUnlock(h);
            Some(v)
        }
    };
    // Explorer's cut is "move" alone; copy offers copy (and link).
    files.cut = effect.is_some_and(|e| e & DROPEFFECT_MOVE != 0 && e & DROPEFFECT_COPY == 0);
    Ok(files)
}

/// Puts `paths` on the clipboard as Explorer does, so it (and any other
/// program) can paste them: CF_HDROP and "Preferred DropEffect".
/// `out_of_history` keeps it out of the clipboard history (tests).
fn write_files(paths: &[String], cut: bool, out_of_history: bool) -> Result<(), String> {
    if paths.is_empty() {
        return Err("클립보드에 넣을 항목이 없어요".into());
    }
    // DROPFILES, then each path with its NUL, then one more NUL.
    let header = DROPFILES { pFiles: std::mem::size_of::<DROPFILES>() as u32, pt: POINT { x: 0, y: 0 }, fNC: 0, fWide: 1 };
    let mut bytes: Vec<u8> =
        unsafe { std::slice::from_raw_parts((&raw const header).cast::<u8>(), std::mem::size_of::<DROPFILES>()) }.to_vec();
    for p in paths {
        bytes.extend(p.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes));
    }
    bytes.extend([0, 0]);
    let effect = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY | DROPEFFECT_LINK };
    let _clipboard = Clipboard::open()?;
    if unsafe { EmptyClipboard() } == 0 {
        return Err(CLIPBOARD_BUSY.into());
    }
    set_clipboard(CF_HDROP as u32, &bytes)?;
    set_clipboard(drop_effect_format(), &effect.to_le_bytes())?;
    if out_of_history {
        let format = unsafe { RegisterClipboardFormatW(win::wide("ExcludeClipboardContentFromMonitorProcessing").as_ptr()) };
        set_clipboard(format, &[0; 4])?;
    }
    Ok(())
}

/// Hands a copy of `data` to the open clipboard, which then owns it.
fn set_clipboard(format: u32, data: &[u8]) -> Result<(), String> {
    let failed = || "클립보드에 넣지 못했어요".to_string();
    unsafe {
        let block: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, data.len());
        if block.is_null() {
            return Err(failed());
        }
        let p = GlobalLock(block);
        if p.is_null() {
            GlobalFree(block);
            return Err(failed());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
        GlobalUnlock(block);
        if SetClipboardData(format, block).is_null() {
            GlobalFree(block);
            return Err(failed());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_OFFLINE;
    use windows_sys::Win32::System::DataExchange::EnumClipboardFormats;

    const TEST: Settings = Settings {
        gap: Duration::ZERO,
        rename: true,
        junction: create_junction,
        hard_link: create_hard_link,
        bin: test_bin,
        rule: |_, _, _| links::Verdict::Allowed,
    };

    /// The Recycle Bin of tests, which keep the real one out of it: `x\.c.ff-link`
    /// goes to `xin .c.ff-link`.
    fn test_bin(p: &Path) -> Result<(), String> {
        let folder = p.parent().unwrap();
        let bin = folder.parent().unwrap().join("bin");
        fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
        let name = format!("{} {}", folder.file_name().unwrap().to_string_lossy(), p.file_name().unwrap().to_string_lossy());
        fs::rename(win::verbatim(p), bin.join(name)).map_err(|e| e.to_string())
    }

    /// Answers conflicts from a list (then "cancel"), or not at all with
    /// `hold`; can cancel its job halfway through a file.
    #[derive(Default)]
    struct Sink {
        answers: Mutex<VecDeque<(&'static str, bool)>>,
        conflicts: Arc<Mutex<Vec<Conflict>>>,
        /// (state, errors listed, error_count) of each progress event.
        events: Arc<Mutex<Vec<(String, usize, u64)>>>,
        cancel_mid_file: bool,
        /// A folder whose entries (name, attributes) go into `peeked` halfway through a file.
        peek: Option<PathBuf>,
        peeked: Arc<Mutex<Vec<(String, u32)>>>,
        hold: bool,
        /// Told of every progress event (a test that changes the disk while a job runs).
        on_progress: Option<Arc<OnProgress>>,
    }

    type OnProgress = dyn Fn(&JobInfo) + Send + Sync;

    impl Events for Sink {
        fn progress(&self, job: &JobInfo) {
            self.events.lock().unwrap().push((job.state.clone(), job.errors.len(), job.error_count));
            if let Some(f) = &self.on_progress {
                f(job);
            }
            let mid_file = job.done_bytes > 0 && job.done_bytes < job.total_bytes;
            if let Some(dir) = self.peek.as_ref().filter(|_| mid_file) {
                let mut peeked = self.peeked.lock().unwrap();
                if peeked.is_empty() {
                    for e in fs::read_dir(dir).unwrap().flatten() {
                        // Opened, not listed: the listing's copy of the attributes may lag behind.
                        let attrs = fs::symlink_metadata(win::verbatim(&e.path())).map_or(0, |m| m.file_attributes());
                        peeked.push((e.file_name().to_string_lossy().into_owned(), attrs));
                    }
                }
            }
            if self.cancel_mid_file && mid_file {
                cancel_job(job.id).unwrap();
            }
        }

        fn conflict(&self, c: &Conflict) {
            self.conflicts.lock().unwrap().push(c.clone());
            if self.hold {
                return;
            }
            let (choice, all) = self.answers.lock().unwrap().pop_front().unwrap_or(("cancel", false));
            resolve_conflict(c.job, choice.into(), all).unwrap();
        }
    }

    /// A folder of its own under %TEMP%, removed afterwards.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Temp {
            let dir = std::env::temp_dir().join(format!("ff-ops-{name}-{}", std::process::id()));
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

    fn wait(id: u64) -> JobInfo {
        let t = Instant::now();
        loop {
            if let Some(j) = job_list().into_iter().find(|j| j.id == id)
                && is_finished(&j.state)
            {
                return j;
            }
            assert!(t.elapsed() < Duration::from_secs(60), "job {id} did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn run_with(op: &str, sources: &[&Path], dest: &Path, answers: &[(&'static str, bool)], settings: Settings, cancel_mid_file: bool) -> (JobInfo, Vec<Conflict>) {
        let conflicts = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink { answers: Mutex::new(answers.iter().copied().collect()), conflicts: conflicts.clone(), cancel_mid_file, ..Sink::default() };
        let sources = sources.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let id = start(op, sources, dest.to_string_lossy().into_owned(), sink, settings).unwrap();
        let job = wait(id);
        let conflicts = conflicts.lock().unwrap().clone();
        (job, conflicts)
    }

    fn run(op: &str, sources: &[&Path], dest: &Path, answers: &[(&'static str, bool)]) -> (JobInfo, Vec<Conflict>) {
        run_with(op, sources, dest, answers, TEST, false)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn copies_files_and_folders() {
        let t = Temp::new("copy");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        fs::write(src.join("a.txt"), "hello").unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        fs::File::options().write(true).open(src.join("a.txt")).unwrap().set_modified(old).unwrap();
        let folder = src.join("folder");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::create_dir(folder.join("empty")).unwrap();
        fs::write(folder.join("sub").join("b.bin"), vec![7u8; 3000]).unwrap();
        let hidden = win::wide_path(&folder.join("sub"));
        unsafe { SetFileAttributesW(hidden.as_ptr(), FILE_ATTRIBUTE_HIDDEN) };

        let (job, conflicts) = run("copy", &[&src.join("a.txt"), &folder], &dst, &[]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty() && conflicts.is_empty());
        assert_eq!((job.done_items, job.total_items), (5, 5));
        assert_eq!((job.done_bytes, job.total_bytes), (3005, 3005));
        assert_eq!(job.current, None);
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(fs::metadata(dst.join("a.txt")).unwrap().modified().unwrap(), old);
        assert_eq!(fs::read(dst.join("folder").join("sub").join("b.bin")).unwrap(), vec![7u8; 3000]);
        assert!(dst.join("folder").join("empty").is_dir());
        assert!(fs::metadata(dst.join("folder").join("sub")).unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0);
        // Each file under its real name, no longer hidden, and no partial file left.
        assert_eq!(names(&dst), ["a.txt", "folder"]);
        assert_eq!(names(&dst.join("folder").join("sub")), ["b.bin"]);
        assert_eq!(fs::metadata(dst.join("a.txt")).unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN, 0);
        // A copy leaves its sources alone.
        assert_eq!(fs::read_to_string(src.join("a.txt")).unwrap(), "hello");
        assert!(folder.join("sub").join("b.bin").exists());
    }

    #[test]
    fn copies_past_260_characters() {
        let t = Temp::new("long");
        let src = t.dir("src");
        fs::write(src.join("f.txt"), "deep").unwrap();
        let mut deep = t.0.join("dst");
        while deep.as_os_str().len() < 300 {
            deep.push("a-rather-long-folder-name");
        }
        fs::create_dir_all(&deep).unwrap();
        let (job, _) = run("copy", &[&src], &deep, &[]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(fs::read_to_string(deep.join("src").join("f.txt")).unwrap(), "deep");
    }

    #[test]
    fn pasting_where_it_is_picks_the_next_free_name() {
        let t = Temp::new("same");
        let src = t.dir("src");
        fs::write(src.join("a.txt"), "a").unwrap();
        fs::create_dir(src.join("folder")).unwrap();
        fs::write(src.join("folder").join("in.txt"), "in").unwrap();
        for _ in 0..2 {
            let (job, conflicts) = run("copy", &[&src.join("a.txt"), &src.join("folder")], &src, &[]);
            assert_eq!(job.state, "done", "{:?}", job.errors);
            assert!(conflicts.is_empty());
        }
        assert_eq!(names(&src), ["a (2).txt", "a (3).txt", "a.txt", "folder", "folder (2)", "folder (3)"]);
        assert_eq!(fs::read_to_string(src.join("folder (3)").join("in.txt")).unwrap(), "in");
        // A move to where it already is does nothing, quietly.
        let (job, _) = run("move", &[&src.join("a.txt")], &src, &[]);
        assert_eq!((job.state.as_str(), job.errors.len()), ("done", 0));
        assert_eq!(fs::read_to_string(src.join("a.txt")).unwrap(), "a");
    }

    #[test]
    fn free_names_count_up() {
        let t = Temp::new("names");
        let d = &t.0;
        fs::write(d.join("a.txt"), "").unwrap();
        fs::write(d.join("a (2).txt"), "").unwrap();
        let free = |name: &str, is_dir| free_name(&d.join(name), is_dir).unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(free("a.txt", false), "a (3).txt");
        assert_eq!(free("archive.tar.gz", false), "archive.tar (2).gz");
        assert_eq!(free("my.folder", true), "my.folder (2)");
        assert_eq!(free(".gitignore", false), ".gitignore (2)");
        assert_eq!(free("README", false), "README (2)");
    }

    #[test]
    fn conflicts_are_asked_and_remembered() {
        let t = Temp::new("conflict");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        for n in ["a", "b", "c"] {
            fs::write(src.join(format!("{n}.txt")), format!("new {n}")).unwrap();
        }
        fs::write(dst.join("a.txt"), "old a").unwrap();
        fs::write(dst.join("b.txt"), "old b").unwrap();
        let all: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| src.join(format!("{n}.txt"))).collect();
        let all: Vec<&Path> = all.iter().map(PathBuf::as_path).collect();

        // "Skip" for all: asked once, the rest goes through.
        let (job, conflicts) = run("copy", &all, &dst, &[("skip", true)]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(conflicts.len(), 1);
        let c = &conflicts[0];
        assert_eq!((c.source.as_str(), c.target.as_str()), (&*all[0].to_string_lossy(), &*dst.join("a.txt").to_string_lossy()));
        assert_eq!((c.is_dir, c.source_size, c.target_size), (false, 5, 5));
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "old a");
        assert_eq!(fs::read_to_string(dst.join("b.txt")).unwrap(), "old b");
        assert_eq!(fs::read_to_string(dst.join("c.txt")).unwrap(), "new c");

        // "Rename" keeps both.
        let (job, conflicts) = run("copy", &all[..1], &dst, &[("rename", false)]);
        assert_eq!((job.state.as_str(), conflicts.len()), ("done", 1));
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "old a");
        assert_eq!(fs::read_to_string(dst.join("a (2).txt")).unwrap(), "new a");

        // "Cancel" stops the job; nothing after it happens.
        fs::remove_file(dst.join("c.txt")).unwrap();
        let (job, _) = run("copy", &all, &dst, &[("cancel", false)]);
        assert_eq!(job.state, "cancelled");
        assert!(!dst.join("c.txt").exists());
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "old a");
    }

    #[test]
    fn overwrite_never_recycles_the_folder_holding_the_source() {
        let t = Temp::new("holds");
        let outer = t.dir("x");
        let a = outer.join("a");
        fs::create_dir(&a).unwrap();
        fs::write(a.join("a"), "inner").unwrap();
        // The file x\a\a pasted into x meets the folder x\a, which holds it. A file
        // never takes a folder's place anyway.
        let (job, conflicts) = run("copy", &[&a.join("a")], &outer, &[("overwrite", false)]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(a.join("a").to_string_lossy().into_owned(), KIND_DIFFERS.to_string())]);
        assert_eq!(fs::read_to_string(a.join("a")).unwrap(), "inner");
        // And the bin is never asked to take it: the check comes first.
        assert_eq!(recycle(&a.join("a"), &a, &fs::symlink_metadata(&a).unwrap()), Err(HOLDS_SOURCE.to_string()));
        assert!(a.join("a").exists());
    }

    /// Puts two small items in the Recycle Bin, so it runs only when asked:
    /// `cargo test overwrite_goes_through -- --ignored`
    #[test]
    #[ignore]
    fn overwrite_goes_through_the_recycle_bin() {
        let t = Temp::new("overwrite");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        fs::write(src.join("a.txt"), "new").unwrap();
        fs::write(dst.join("a.txt"), "old").unwrap();
        fs::write(src.join("b.txt"), "new b").unwrap();
        fs::write(dst.join("b.txt"), "old b").unwrap();
        // A folder in the way of a file: asked again despite "for all", and never overwritten.
        fs::write(src.join("d"), "file").unwrap();
        fs::create_dir(dst.join("d")).unwrap();
        fs::write(dst.join("d").join("inside.txt"), "x").unwrap();
        let paths = [src.join("a.txt"), src.join("d"), src.join("b.txt")];
        let sources: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let (job, conflicts) = run("copy", &sources, &dst, &[("overwrite", true), ("overwrite", false)]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(conflicts.len(), 2);
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "new");
        assert_eq!(fs::read_to_string(dst.join("b.txt")).unwrap(), "new b");
        assert_eq!(fs::read_to_string(dst.join("d").join("inside.txt")).unwrap(), "x");
        assert_eq!(job.errors, [(src.join("d").to_string_lossy().into_owned(), KIND_DIFFERS.to_string())]);
    }

    #[test]
    fn moves_within_a_volume_by_renaming() {
        let t = Temp::new("move");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        fs::create_dir(src.join("m")).unwrap();
        fs::write(src.join("m").join("x.txt"), "x").unwrap();
        // A second name for the same file tells a rename from a copy.
        fs::hard_link(src.join("m").join("x.txt"), t.0.join("keep.txt")).unwrap();
        let (job, _) = run("move", &[&src.join("m")], &dst, &[]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!((job.done_items, job.total_items), (1, 1));
        assert!(!src.join("m").exists());
        assert!(win::same_entry(&dst.join("m").join("x.txt"), &t.0.join("keep.txt")));
    }

    #[test]
    fn moving_onto_a_folder_merges_and_keeps_what_stayed() {
        let t = Temp::new("merge");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        let (from, into) = (src.join("m"), dst.join("m"));
        fs::create_dir(&from).unwrap();
        fs::create_dir(&into).unwrap();
        fs::write(from.join("x.txt"), "src x").unwrap();
        fs::write(from.join("y.txt"), "src y").unwrap();
        fs::write(into.join("x.txt"), "dst x").unwrap();
        fs::write(into.join("other.txt"), "other").unwrap();
        let (job, conflicts) = run("move", &[&from], &dst, &[("overwrite", false), ("skip", false)]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(conflicts.len(), 2);
        assert!(conflicts[0].is_dir && !conflicts[1].is_dir);
        assert_eq!(conflicts[1].target, into.join("x.txt").to_string_lossy());
        assert_eq!(fs::read_to_string(into.join("x.txt")).unwrap(), "dst x");
        assert_eq!(fs::read_to_string(into.join("y.txt")).unwrap(), "src y");
        assert_eq!(fs::read_to_string(into.join("other.txt")).unwrap(), "other");
        // The skipped file stays, and so does its folder.
        assert_eq!(names(&from), ["x.txt"]);
        assert_eq!(job.done_items, job.total_items);
    }

    #[test]
    fn items_of_different_kinds_are_never_overwritten() {
        let t = Temp::new("kinds");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        let elsewhere = t.dir("elsewhere");
        let (from, into) = (src.join("p"), dst.join("p"));
        // A file meets a folder full of things, and a folder meets a file.
        fs::create_dir_all(into.join("report")).unwrap();
        fs::write(into.join("report").join("inside.txt"), "keep").unwrap();
        fs::create_dir_all(from.join("notes")).unwrap();
        fs::write(from.join("notes").join("n.txt"), "n").unwrap();
        fs::write(from.join("report"), "a file").unwrap();
        fs::write(into.join("notes"), "old notes").unwrap();
        // A junction meets a real folder.
        fs::create_dir(into.join("nm")).unwrap();
        fs::write(into.join("nm").join("keep.txt"), "real").unwrap();
        let junction = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(from.join("nm"))
            .arg(&elsewhere)
            .output()
            .is_ok_and(|o| o.status.success());
        // "Overwrite for all" on the folders merges them; the rest is asked about on its own.
        let answers = [("overwrite", true), ("overwrite", false), ("overwrite", false), ("overwrite", false)];
        for op in ["copy", "move"] {
            let (job, conflicts) = run(op, &[&from], &dst, &answers);
            assert_eq!(job.state, "done", "{:?}", job.errors);
            let mut expected = vec![from.join("notes"), from.join("report")];
            if junction {
                expected.push(from.join("nm"));
            }
            assert_eq!(conflicts.len(), 1 + expected.len(), "{op}");
            let mut errors = job.errors.clone();
            errors.sort();
            let mut want: Vec<(String, String)> =
                expected.iter().map(|p| (p.to_string_lossy().into_owned(), KIND_DIFFERS.to_string())).collect();
            want.sort();
            assert_eq!(errors, want, "{op}");
            let about = |p: &Path| conflicts.iter().find(|c| c.target == p.to_string_lossy()).unwrap().clone();
            assert!(about(&into).is_dir && about(&into).target_is_dir);
            assert!(!about(&into).is_link && !about(&into).target_is_link);
            if junction {
                assert!(about(&into.join("nm")).is_link && !about(&into.join("nm")).target_is_link);
                assert!(about(&into.join("nm")).is_dir && about(&into.join("nm")).target_is_dir);
            }
            assert!(!about(&into.join("report")).is_dir && about(&into.join("report")).target_is_dir);
            assert!(about(&into.join("notes")).is_dir && !about(&into.join("notes")).target_is_dir);
            assert_eq!(about(&into.join("notes")).target_size, 9);
            // Everything that was there still is, as it was.
            assert_eq!(fs::read_to_string(into.join("report").join("inside.txt")).unwrap(), "keep");
            assert_eq!(fs::read_to_string(into.join("notes")).unwrap(), "old notes");
            assert_eq!(win::reparse_tag(&into.join("nm")), Some(0));
            assert_eq!(fs::read_to_string(into.join("nm").join("keep.txt")).unwrap(), "real");
            // And so is every source.
            assert_eq!(fs::read_to_string(from.join("report")).unwrap(), "a file");
            assert_eq!(fs::read_to_string(from.join("notes").join("n.txt")).unwrap(), "n");
        }
        if junction {
            assert_eq!(win::reparse_tag(&from.join("nm")), Some(IO_REPARSE_TAG_MOUNT_POINT));
            fs::remove_dir(from.join("nm")).unwrap();
        }
    }

    #[test]
    fn a_name_ending_in_a_dot_is_not_its_neighbour() {
        let t = Temp::new("dots");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        let x = src.join("x");
        fs::create_dir(&x).unwrap();
        // Through `\\?\`: plain Win32 calls would make (and find) `abc` for `abc.`.
        fs::write(win::verbatim(&x.join("abc")), "B").unwrap();
        fs::write(win::verbatim(&x.join("abc.")), "A").unwrap();
        fs::create_dir(x.join("dir")).unwrap();
        fs::write(x.join("dir").join("other.txt"), "in dir").unwrap();
        fs::create_dir(win::verbatim(&x.join("dir."))).unwrap();
        fs::write(win::verbatim(&x.join("dir.").join("in.txt")), "in dir.").unwrap();
        let read = |p: &Path| fs::read_to_string(win::verbatim(p)).unwrap();

        let (job, conflicts) = run("copy", &[&x], &dst, &[]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty() && conflicts.is_empty(), "{:?}, {} conflicts", job.errors, conflicts.len());
        let copy = dst.join("x");
        assert_eq!((read(&copy.join("abc")), read(&copy.join("abc."))), ("B".to_string(), "A".to_string()));
        assert_eq!(names(&win::verbatim(&copy.join("dir."))), ["in.txt"]);
        assert_eq!(names(&copy.join("dir")), ["other.txt"]);

        // A move across volumes takes each of them, and lets go of each source.
        let moved = t.dir("moved");
        let (job, _) = run_with("move", &[&x], &moved, &[], Settings { rename: false, ..TEST }, false);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert!(!x.exists());
        let there = moved.join("x");
        assert_eq!((read(&there.join("abc")), read(&there.join("abc."))), ("B".to_string(), "A".to_string()));
        assert_eq!(read(&there.join("dir.").join("in.txt")), "in dir.");

        // The Recycle Bin cannot tell `abc.` from `abc`, so that one is not overwritten.
        let (job, conflicts) = run("copy", &[&there.join("abc.")], &copy, &[("overwrite", false)]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(job.errors, [(there.join("abc.").to_string_lossy().into_owned(), ODD_NAME.to_string())]);
        assert_eq!((read(&copy.join("abc")), read(&copy.join("abc."))), ("B".to_string(), "A".to_string()));
    }

    #[test]
    fn running_events_carry_only_the_newest_errors() {
        let t = Temp::new("errors");
        let src = t.dir("src");
        let dst = t.dir("dst");
        fs::write(src.join("a.txt"), "a").unwrap();
        let many = EVENT_ERRORS + 10;
        let mut sources: Vec<String> = (0..many).map(|i| src.join(format!("missing {i}")).to_string_lossy().into_owned()).collect();
        sources.push(src.join("a.txt").to_string_lossy().into_owned());
        let sink = Sink::default();
        let events = sink.events.clone();
        let id = start("copy", sources, dst.to_string_lossy().into_owned(), sink, TEST).unwrap();
        let job = wait(id);
        assert_eq!(job.state, "done");
        assert_eq!((job.errors.len(), job.error_count), (many, many as u64));
        // The last event is sent just after `jobs` has the job as done.
        let t0 = Instant::now();
        while !events.lock().unwrap().iter().any(|(state, _, _)| is_finished(state)) {
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        let events = events.lock().unwrap();
        assert!(events.iter().any(|(state, listed, count)| state == "running" && *listed == EVENT_ERRORS && *count == many as u64));
        for (state, listed, _) in events.iter() {
            assert!(is_finished(state) || *listed <= EVENT_ERRORS, "{state}: {listed}");
        }
        assert_eq!(events.last().map(|(s, listed, _)| (s.as_str(), *listed)), Some(("done", many)));
    }

    #[test]
    fn a_waiting_conflict_can_be_asked_for_again() {
        let t = Temp::new("pending");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        fs::write(src.join("a.txt"), "new").unwrap();
        fs::write(dst.join("a.txt"), "old").unwrap();
        // Nobody answers the event, as after a reload of the window.
        let sink = Sink { hold: true, ..Sink::default() };
        let id = start("copy", vec![src.join("a.txt").to_string_lossy().into()], dst.to_string_lossy().into(), sink, TEST).unwrap();
        let t0 = Instant::now();
        let c = loop {
            if let Some(c) = pending_conflicts().into_iter().find(|c| c.job == id) {
                break c;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "no conflict");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!((c.source, c.target), (src.join("a.txt").to_string_lossy().into(), dst.join("a.txt").to_string_lossy().into()));
        assert!(!c.is_dir && !c.target_is_dir);
        resolve_conflict(id, "skip".into(), false).unwrap();
        let job = wait(id);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(pending_conflicts().iter().all(|c| c.job != id));
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "old");
    }

    #[test]
    fn moves_across_volumes_by_copying_then_removing() {
        let t = Temp::new("across");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        let target = t.dir("target");
        fs::write(target.join("keep.txt"), "keep").unwrap();
        let c = src.join("c");
        fs::create_dir_all(c.join("sub")).unwrap();
        fs::write(c.join("f1.txt"), "one").unwrap();
        fs::write(c.join("sub").join("f2.txt"), "two").unwrap();
        let junction = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(c.join("link"))
            .arg(&target)
            .output()
            .is_ok_and(|o| o.status.success());
        let settings = Settings { rename: false, ..TEST };
        let (job, _) = run_with("move", &[&c], &dst, &[], settings, false);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert!(!c.exists());
        assert_eq!(fs::read_to_string(dst.join("c").join("f1.txt")).unwrap(), "one");
        assert_eq!(fs::read_to_string(dst.join("c").join("sub").join("f2.txt")).unwrap(), "two");
        if junction {
            // The junction came along as a junction; what it points to was neither copied nor removed.
            assert_eq!(job.total_items, 5);
            assert_eq!(win::reparse_tag(&dst.join("c").join("link")), Some(IO_REPARSE_TAG_MOUNT_POINT));
            assert_eq!(fs::read_to_string(dst.join("c").join("link").join("keep.txt")).unwrap(), "keep");
            assert_eq!(names(&target), ["keep.txt"]);
            fs::remove_dir(dst.join("c").join("link")).unwrap();
        } else {
            eprintln!("mklink /J failed; the junction part was skipped");
        }
    }

    #[test]
    fn cancel_removes_the_unfinished_copy() {
        let t = Temp::new("cancel");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        let big = src.join("big.bin");
        let data: Vec<u8> = (0..64u32 << 20).map(|i| ((i * 31) >> 7) as u8).collect();
        fs::write(&big, &data).unwrap();
        let peeked = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink { cancel_mid_file: true, peek: Some(dst.clone()), peeked: peeked.clone(), ..Sink::default() };
        let id = start("copy", vec![big.to_string_lossy().into()], dst.to_string_lossy().into(), sink, TEST).unwrap();
        let job = wait(id);
        assert_eq!(job.state, "cancelled");
        assert!(job.done_bytes < job.total_bytes);
        // Halfway through, the copy was there only under its hidden partial name.
        let peeked = peeked.lock().unwrap().clone();
        assert_eq!(peeked.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), [".big.bin.ff-partial"]);
        assert!(peeked[0].1 & FILE_ATTRIBUTE_HIDDEN != 0, "{:#x}", peeked[0].1);
        // And nothing of it is left.
        assert!(names(&dst).is_empty(), "{:?}", names(&dst));
        assert_eq!(fs::metadata(&big).unwrap().len(), data.len() as u64);
    }

    #[test]
    fn a_finished_copy_gets_its_name_and_look_and_leaves_other_partials_alone() {
        let t = Temp::new("partial-name");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        fs::write(src.join("plain.txt"), "plain").unwrap();
        fs::write(src.join("hidden.txt"), "hidden").unwrap();
        fs::write(src.join("ro.txt"), "read-only").unwrap();
        let set = |p: &Path, attrs: u32| unsafe { SetFileAttributesW(win::wide_path(p).as_ptr(), attrs) };
        set(&src.join("hidden.txt"), FILE_ATTRIBUTE_HIDDEN);
        set(&src.join("ro.txt"), FILE_ATTRIBUTE_READONLY);
        // A name as long as a name may be: its partial name is cut to fit.
        let long = format!("{}.txt", "가".repeat(MAX_NAME - 4));
        fs::write(src.join(&long), "long").unwrap();
        // Another job's partial file, or one a crash left: it is not ours to touch.
        fs::write(dst.join(".plain.txt.ff-partial"), "someone else's").unwrap();

        let sources = ["plain.txt", "hidden.txt", "ro.txt", long.as_str()].map(|n| src.join(n));
        let sources: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
        let (job, conflicts) = run("copy", &sources, &dst, &[]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty() && conflicts.is_empty());
        let mut want = vec![".plain.txt.ff-partial".to_string(), "hidden.txt".into(), long.clone(), "plain.txt".into(), "ro.txt".into()];
        want.sort();
        assert_eq!(names(&dst), want);
        assert_eq!(fs::read_to_string(dst.join(".plain.txt.ff-partial")).unwrap(), "someone else's");
        assert_eq!(fs::read_to_string(dst.join("plain.txt")).unwrap(), "plain");
        assert_eq!(fs::read_to_string(dst.join(&long)).unwrap(), "long");
        let attrs = |n: &str| fs::metadata(dst.join(n)).unwrap().file_attributes();
        assert_eq!(attrs("plain.txt") & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY), 0);
        assert!(attrs("hidden.txt") & FILE_ATTRIBUTE_HIDDEN != 0);
        assert_eq!(attrs("ro.txt") & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY), FILE_ATTRIBUTE_READONLY);
        assert_eq!(fs::read_to_string(dst.join("ro.txt")).unwrap(), "read-only");
        for p in [src.join("ro.txt"), dst.join("ro.txt")] {
            set(&p, FILE_ATTRIBUTE_NORMAL);
        }
    }

    #[test]
    fn partial_names_fit_and_count_up() {
        let name = |target: &str, n| partial_name(Path::new(target), n).unwrap().to_string_lossy().into_owned();
        assert_eq!(name(r"C:\d\a.txt", 1), r"C:\d\.a.txt.ff-partial");
        assert_eq!(name(r"C:\d\a.txt", 3), r"C:\d\.a.txt (3).ff-partial");
        assert_eq!(name(r"C:\d\abc.", 1), r"C:\d\.abc..ff-partial");
        let long = "x".repeat(MAX_NAME);
        let cut = partial_name(&Path::new(r"C:\d").join(&long), 12).unwrap();
        let cut = cut.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(cut.encode_utf16().count(), MAX_NAME);
        assert!(cut.starts_with(".xxx") && cut.ends_with(" (12).ff-partial"), "{cut}");
        // A surrogate pair is not split: the cut would fall between its halves, so the emoji goes.
        let emoji = format!("{}😀😀", "x".repeat(MAX_NAME - 13));
        let cut = partial_name(&Path::new(r"C:\d").join(&emoji), 1).unwrap();
        let cut = cut.file_name().unwrap().to_str().expect("still valid UTF-16").to_string();
        assert_eq!(cut.encode_utf16().count(), MAX_NAME - 1);
        assert!(cut.ends_with("xx.ff-partial"), "{cut}");
        assert!(partial_name(Path::new(r"C:\"), 1).is_none());
    }

    #[test]
    fn errors_past_the_limit_are_only_counted() {
        let t = Temp::new("many-errors");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        let many = MAX_ERRORS + 25;
        let sources = (0..many).map(|i| src.join(format!("missing {i}")).to_string_lossy().into_owned()).collect();
        let id = start("copy", sources, dst.to_string_lossy().into_owned(), Sink::default(), TEST).unwrap();
        let job = wait(id);
        assert_eq!(job.state, "failed");
        // The window says how many more there are, from error_count: no note row of our own.
        assert_eq!((job.errors.len(), job.error_count), (MAX_ERRORS, many as u64));
        assert!(job.errors.iter().all(|(_, why)| why == "찾을 수 없어요"));
    }

    #[test]
    fn a_folder_never_goes_into_itself() {
        let t = Temp::new("itself");
        let folder = t.dir("folder");
        let sub = folder.join("sub");
        fs::create_dir(&sub).unwrap();
        for op in ["copy", "move"] {
            let (job, _) = run(op, &[&folder], &sub, &[]);
            assert_eq!(job.state, "failed");
            assert_eq!(job.errors, [(folder.to_string_lossy().into_owned(), INTO_ITSELF.to_string())]);
            assert!(names(&sub).is_empty());
            let (job, _) = run(op, &[&folder], &folder, &[]);
            assert_eq!(job.errors[0].1, INTO_ITSELF);
        }
    }

    #[test]
    fn one_bad_item_does_not_stop_the_rest() {
        let t = Temp::new("partial");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        fs::write(src.join("a.txt"), "a").unwrap();
        let missing = src.join("missing.txt");
        let (job, _) = run("copy", &[&missing, &src.join("a.txt")], &dst, &[]);
        assert_eq!(job.state, "done");
        assert_eq!(job.errors, [(missing.to_string_lossy().into_owned(), "찾을 수 없어요".to_string())]);
        assert!(dst.join("a.txt").exists());
        assert!(start("copy", vec![src.join("a.txt").to_string_lossy().into()], t.0.join("nope").to_string_lossy().into(), Sink::default(), TEST)
        .is_err());
    }

    #[test]
    fn sources_inside_other_sources_are_dropped() {
        let got = top_level(vec![r"C:\a".into(), r"C:\a\b".into(), r"c:\A\".into(), r"C:\ab".into()]);
        assert_eq!(got, [PathBuf::from(r"C:\a"), PathBuf::from(r"C:\ab")]);
    }

    #[test]
    fn wide_paths_lift_the_length_limit_only_when_safe() {
        let w = |p: &str| String::from_utf16_lossy(&win::wide_path(Path::new(p)));
        assert_eq!(w(r"C:\a\b"), "\\\\?\\C:\\a\\b\0");
        assert_eq!(w(r"\\server\share\x"), "\\\\?\\UNC\\server\\share\\x\0");
        for asis in [r"C:\a\..\b", r"C:\a\.\b", "C:/a", r"C:\a\\b", r"\\?\C:\a", r"\\.\pipe\x", r"a\b", r"C:a"] {
            assert_eq!(w(asis), format!("{asis}\0"), "{asis}");
        }
        assert_eq!(w(r"C:\a\"), "\\\\?\\C:\\a\\\0");
    }

    #[test]
    fn new_folder_names_are_checked() {
        let t = Temp::new("mkdir");
        let d = &t.0;
        for bad in [
            "", "   ", ".", "..", "...", "a/b", r"a\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b", "tab\there", "CON", "con",
            "Nul.txt", "COM1", "lpt9.log", "AUX ", "com¹",
        ] {
            assert!(make_folder(d, bad).is_err(), "{bad:?}");
        }
        assert!(names(d).is_empty());
        let made = make_folder(d, " 새 폴더 ").unwrap();
        assert_eq!(made, d.join("새 폴더").to_string_lossy());
        assert!(d.join("새 폴더").is_dir());
        assert_eq!(make_folder(d, "새 폴더"), Err(NAME_TAKEN.to_string()));
        fs::write(d.join("file"), "").unwrap();
        assert_eq!(make_folder(d, "file"), Err(NAME_TAKEN.to_string()));
        // Windows would drop the dot anyway.
        assert_eq!(make_folder(d, "abc.").unwrap(), d.join("abc").to_string_lossy());
        for ok in ["COM10", "CONSOLE", "con-notes", "LPT", "nul2.txt"] {
            assert!(make_folder(d, ok).is_ok(), "{ok}");
        }
        assert_eq!(make_folder(Path::new("relative"), "x"), Err(BAD_PATH.to_string()));
        assert!(make_folder(&d.join("missing"), "x").is_err());
    }

    /// What the clipboard held, to put back after the test. Memory formats
    /// only: bitmaps and metafiles are handles (their DIB forms are kept).
    fn save_clipboard() -> Option<Vec<(u32, Vec<u8>)>> {
        let _clipboard = Clipboard::open().ok()?;
        let mut saved = Vec::new();
        let mut format = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            if matches!(format, 2 | 3 | 9 | 14 | 0x80..=0x8E | 0x200..=0x3FF) {
                continue;
            }
            unsafe {
                let h = GetClipboardData(format);
                let p = if h.is_null() { null_mut() } else { GlobalLock(h) };
                if !p.is_null() {
                    saved.push((format, std::slice::from_raw_parts(p.cast::<u8>(), GlobalSize(h)).to_vec()));
                    GlobalUnlock(h);
                }
            }
        }
        Some(saved)
    }

    fn restore_clipboard(saved: Vec<(u32, Vec<u8>)>) {
        let Ok(_clipboard) = Clipboard::open() else { return };
        unsafe { EmptyClipboard() };
        for (format, data) in saved {
            let _ = set_clipboard(format, &data);
        }
    }

    #[test]
    fn clipboard_round_trip() {
        let Some(saved) = save_clipboard() else {
            eprintln!("clipboard busy or unavailable; skipped");
            return;
        };
        let paths = vec![r"C:\fine-file test\한글 이름.txt".to_string(), r"D:\other folder".to_string()];
        let result = (|| -> Result<(ClipboardFiles, ClipboardFiles), String> {
            write_files(&paths, true, true)?;
            let cut = read_files()?;
            write_files(&paths[..1], false, true)?;
            Ok((cut, read_files()?))
        })();
        restore_clipboard(saved);
        let (cut, copied) = match result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("clipboard busy; skipped: {e}");
                return;
            }
        };
        assert_eq!(cut.paths, paths);
        assert!(cut.cut);
        assert_eq!(copied.paths, paths[..1]);
        assert!(!copied.cut);
    }

    // ---- v0.4: relocation -------------------------------------------------------

    /// Relocations run one at a time (`Busy`), so their tests take turns.
    fn one_relocation_at_a_time() -> MutexGuard<'static, ()> {
        static TURN: Mutex<()> = Mutex::new(());
        TURN.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tests have no second drive: any two folders count as on two volumes.
    const ACROSS: Settings = Settings { rename: false, ..TEST };

    fn book_in(t: &Temp) -> Book {
        Book(t.0.join(RELOCATIONS_FILE))
    }

    fn relocate_in(t: &Temp, source: &Path, dest: &Path, settings: Settings) -> Result<JobInfo, String> {
        Ok(wait(begin_relocate(source, dest, Sink::default(), settings, book_in(t))?))
    }

    fn restore_in(t: &Temp, id: &str) -> Result<JobInfo, String> {
        Ok(wait(begin_restore(id, Sink::default(), ACROSS, book_in(t))?))
    }

    /// Why a relocation or undo did not start.
    fn refusal(started: Result<JobInfo, String>) -> String {
        match started {
            Ok(job) => panic!("job {} started: {:?}", job.id, job.errors),
            Err(why) => why,
        }
    }

    fn set_attrs(p: &Path, attrs: u32) {
        unsafe { SetFileAttributesW(win::wide_path(p).as_ptr(), attrs) };
    }

    /// `home\게임 data` with files, folders, a read-only file and a junction to `elsewhere`.
    fn game_folder(home: &Path, elsewhere: &Path) -> PathBuf {
        let folder = home.join("게임 data");
        fs::create_dir_all(folder.join("sub").join("deeper")).unwrap();
        fs::create_dir(folder.join("empty")).unwrap();
        fs::write(folder.join("a.txt"), "hello").unwrap();
        fs::write(folder.join("sub").join("b.bin"), vec![7u8; 3000]).unwrap();
        fs::write(folder.join("ro.txt"), "read-only").unwrap();
        set_attrs(&folder.join("ro.txt"), FILE_ATTRIBUTE_READONLY);
        create_junction(&folder.join("link"), elsewhere).unwrap();
        folder
    }

    #[test]
    fn a_relocated_folder_is_reached_through_its_junction_and_comes_back() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate");
        let (home, other, elsewhere) = (t.dir("home"), t.dir("other drive"), t.dir("elsewhere"));
        fs::write(elsewhere.join("keep.txt"), "keep").unwrap();
        let folder = game_folder(&home, &elsewhere);
        let original = real_path(&folder).unwrap();
        let target = real_path(&other).unwrap().join("게임 data");

        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!((job.op.as_str(), job.state.as_str()), ("relocate", "done"), "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert_eq!((job.done_bytes, job.total_bytes), (3014, 3014));
        assert_eq!(job.done_items, job.total_items);
        // The folder is at the target, and where it was a junction leads there.
        assert_eq!(win::reparse_tag(&folder), Some(IO_REPARSE_TAG_MOUNT_POINT));
        assert_eq!(junction_target(&folder), Some(target.clone()));
        assert_eq!(fs::read_to_string(folder.join("a.txt")).unwrap(), "hello");
        assert_eq!(fs::read(target.join("sub").join("b.bin")).unwrap(), vec![7u8; 3000]);
        assert!(target.join("sub").join("deeper").is_dir() && target.join("empty").is_dir());
        assert!(fs::metadata(target.join("ro.txt")).unwrap().permissions().readonly());
        // The junction inside came along as a junction; what it points to was not touched.
        assert_eq!(junction_target(&target.join("link")), Some(elsewhere.clone()));
        assert_eq!(names(&elsewhere), ["keep.txt"]);
        // Neither `.ff-old` nor `.ff-relocating` is left.
        assert_eq!(names(&home), ["게임 data"]);
        assert_eq!(names(&other), ["게임 data"]);
        let list = book_in(&t).list().unwrap();
        assert_eq!(list.len(), 1);
        let r = list[0].clone();
        assert_eq!((r.original.as_str(), r.target.as_str()), (&*original.to_string_lossy(), &*target.to_string_lossy()));
        assert_eq!((r.state.as_str(), r.size), ("active", 3014));

        // Undo: a real folder again, where it was, with all of it.
        let job = restore_in(&t, &r.id).unwrap();
        assert_eq!((job.op.as_str(), job.state.as_str()), ("restore", "done"), "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert_eq!(win::reparse_tag(&folder), Some(0));
        assert_eq!(fs::read_to_string(folder.join("a.txt")).unwrap(), "hello");
        assert_eq!(fs::read(folder.join("sub").join("b.bin")).unwrap(), vec![7u8; 3000]);
        assert!(folder.join("sub").join("deeper").is_dir() && folder.join("empty").is_dir());
        assert!(fs::metadata(folder.join("ro.txt")).unwrap().permissions().readonly());
        assert_eq!(junction_target(&folder.join("link")), Some(elsewhere.clone()));
        assert_eq!(names(&elsewhere), ["keep.txt"]);
        assert_eq!(names(&home), ["게임 data"]);
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert_eq!(book_in(&t).list().unwrap()[0].state, "restored");
        assert_eq!(refusal(restore_in(&t, &r.id)), ALREADY_RESTORED);
        assert_eq!(refusal(restore_in(&t, "no such id")), NO_RECORD);
        set_attrs(&folder.join("ro.txt"), FILE_ATTRIBUTE_NORMAL);
        fs::remove_dir(folder.join("link")).unwrap();
    }

    #[test]
    fn relocation_turns_down_what_it_must_not_move() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-no");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("f");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(folder.join("x.txt"), "x").unwrap();
        fs::write(home.join("file.txt"), "file").unwrap();
        create_junction(&home.join("link"), &folder).unwrap();
        let refused = |source: &Path, dest: &Path, settings| refusal(relocate_in(&t, source, dest, settings));
        // Both temp folders are on one drive, which the real check turns down.
        assert_eq!(refused(&folder, &other, Settings::APP), SAME_DRIVE);
        assert!(refused(&home.join("missing"), &other, ACROSS).starts_with(SOURCE_MISSING));
        assert_eq!(refused(&home.join("file.txt"), &other, ACROSS), ONLY_FOLDERS);
        assert_eq!(refused(&home.join("link"), &other, ACROSS), SPECIAL_FOLDER);
        assert_eq!(refused(Path::new(r"C:\"), &other, ACROSS), WHOLE_DRIVE_TOO);
        assert_eq!(refused(Path::new("relative"), &other, ACROSS), BAD_PATH);
        let windows = known_folder(&FOLDERID_Windows).unwrap();
        let profile = known_folder(&FOLDERID_Profile).unwrap();
        for (place, why) in [
            (windows.clone(), NO_WINDOWS),
            (windows.join("System32"), NO_WINDOWS),
            (known_folder(&FOLDERID_ProgramFiles).unwrap(), NO_PROGRAMS),
            (known_folder(&FOLDERID_ProgramData).unwrap(), NO_PROGRAM_DATA),
            (profile.clone(), NO_PROFILE),
            (known_folder(&FOLDERID_UserProfiles).unwrap(), NO_PROFILE),
            // It holds the app's data folder.
            (profile.join("AppData"), HOLDS_GUARDED),
        ] {
            assert_eq!(refused(&place, &other, ACROSS), why, "{}", place.display());
        }
        // Where it would go.
        assert_eq!(refused(&folder, &home.join("nope"), ACROSS), DEST_MISSING);
        assert_eq!(refused(&folder, &folder.join("sub"), ACROSS), INTO_ITSELF);
        assert_eq!(refused(&folder, &windows, ACROSS), DEST_GUARDED);
        assert!(refused(&folder, &home, ACROSS).starts_with(TARGET_TAKEN));
        assert!(!in_sync_root(&real_path(&other).unwrap()));
        // A OneDrive folder, where this computer has one (only its listing is read).
        if let Some(sync) = std::env::var_os("OneDrive").map(PathBuf::from).and_then(|p| real_path(&p))
            && win::reparse_tag(&sync).is_some_and(crate::scan::is_cloud_tag)
        {
            assert!(in_sync_root(&sync));
            assert_eq!(refused(&folder, &sync, ACROSS), DEST_CLOUD);
        }
        // One at a time.
        let busy = Busy::claim(&book_in(&t)).unwrap();
        assert_eq!(refused(&folder, &other, ACROSS), RELOCATE_BUSY);
        drop(busy);
        // Nothing was touched, and nothing written down.
        assert_eq!(names(&folder), ["sub", "x.txt"]);
        assert!(names(&other).is_empty());
        assert!(book_in(&t).list().unwrap().is_empty());
        fs::remove_dir(home.join("link")).unwrap();
    }

    #[test]
    fn a_folder_in_use_stays_where_it_is() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-in-use");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("app");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("db.dat"), "data").unwrap();
        // Open as a program would, without letting it be renamed or deleted.
        let hold = |p: &Path| fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(p).unwrap();

        let held = hold(&folder.join("db.dat"));
        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors.iter().map(|(_, why)| why.as_str()).collect::<Vec<_>>(), [IN_USE]);
        assert_eq!(win::reparse_tag(&folder), Some(0));
        assert_eq!(fs::read_to_string(folder.join("db.dat")).unwrap(), "data");
        assert_eq!(names(&home), ["app"]);
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert!(book_in(&t).list().unwrap().is_empty());
        drop(held);

        // Moved; then in use at the target, so the undo leaves it there.
        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        let id = book_in(&t).list().unwrap()[0].id.clone();
        let held = hold(&other.join("app").join("db.dat"));
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors.iter().map(|(_, why)| why.as_str()).collect::<Vec<_>>(), [IN_USE]);
        assert_eq!(junction_target(&folder), Some(real_path(&other).unwrap().join("app")));
        assert_eq!(fs::read_to_string(folder.join("db.dat")).unwrap(), "data");
        assert_eq!(names(&home), ["app"]);
        assert_eq!(names(&other), ["app"]);
        assert_eq!(book_in(&t).list().unwrap()[0].state, "active");
        drop(held);
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(win::reparse_tag(&folder), Some(0));
        assert!(names(&other).is_empty());
    }

    #[test]
    fn a_junction_that_cannot_be_made_puts_everything_back() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-rollback");
        let (home, other, elsewhere) = (t.dir("home"), t.dir("other"), t.dir("elsewhere"));
        let folder = game_folder(&home, &elsewhere);
        let settings = Settings { junction: |_, _| Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)), ..ACROSS };
        let job = relocate_in(&t, &folder, &other, settings).unwrap();
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors.len(), 1, "{:?}", job.errors);
        assert_eq!(job.errors[0].1, format!("{NO_JUNCTION}: 권한이 없어요"));
        // The folder is back as it was; the copy is gone, and nothing was written down.
        assert_eq!(win::reparse_tag(&folder), Some(0));
        assert_eq!(fs::read_to_string(folder.join("a.txt")).unwrap(), "hello");
        assert_eq!(fs::read(folder.join("sub").join("b.bin")).unwrap(), vec![7u8; 3000]);
        assert_eq!(junction_target(&folder.join("link")), Some(elsewhere.clone()));
        assert_eq!(names(&home), ["게임 data"]);
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert!(book_in(&t).list().unwrap().is_empty());
        set_attrs(&folder.join("ro.txt"), FILE_ATTRIBUTE_NORMAL);
        fs::remove_dir(folder.join("link")).unwrap();
    }

    #[test]
    fn a_broken_relocation_shows_as_broken_and_is_not_undone() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-broken");
        let (home, other, elsewhere) = (t.dir("home"), t.dir("other"), t.dir("elsewhere"));
        let folder = home.join("cache");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("c.bin"), "cache").unwrap();
        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        let target = real_path(&other).unwrap().join("cache");
        let id = book_in(&t).list().unwrap()[0].id.clone();
        let state = || book_in(&t).list().unwrap()[0].state.clone();
        // The junction removed by hand: broken, and the target is left alone.
        fs::remove_dir(&folder).unwrap();
        assert_eq!(state(), "broken");
        assert_eq!(refusal(restore_in(&t, &id)), BROKEN);
        assert_eq!(fs::read_to_string(target.join("c.bin")).unwrap(), "cache");
        // A junction that leads elsewhere is no better.
        create_junction(&folder, &elsewhere).unwrap();
        assert_eq!(state(), "broken");
        assert_eq!(remove_junction(&folder, &target).unwrap_err().to_string(), NOT_OUR_JUNCTION);
        assert!(folder.exists());
        // Back as it was, it can be undone again.
        fs::remove_dir(&folder).unwrap();
        create_junction(&folder, &target).unwrap();
        assert_eq!(state(), "active");
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(fs::read_to_string(folder.join("c.bin")).unwrap(), "cache");
        assert!(names(&other).is_empty());
        assert!(names(&elsewhere).is_empty());
    }

    #[test]
    fn a_cancelled_relocation_leaves_nothing_behind() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-cancel");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("big");
        fs::create_dir(&folder).unwrap();
        let data: Vec<u8> = (0..64u32 << 20).map(|i| ((i * 31) >> 7) as u8).collect();
        fs::write(folder.join("big.bin"), &data).unwrap();
        let sink = Sink { cancel_mid_file: true, ..Sink::default() };
        let job = wait(begin_relocate(&folder, &other, sink, ACROSS, book_in(&t)).unwrap());
        assert_eq!(job.state, "cancelled");
        assert_eq!(win::reparse_tag(&folder), Some(0));
        assert_eq!(fs::metadata(folder.join("big.bin")).unwrap().len(), data.len() as u64);
        assert_eq!(names(&home), ["big"]);
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert!(book_in(&t).list().unwrap().is_empty());
    }

    #[test]
    fn tallies_tell_a_changed_folder_and_removal_never_follows_links() {
        let t = Temp::new("tally");
        let (a, b, outside) = (t.dir("a"), t.dir("b"), t.dir("outside"));
        fs::write(outside.join("keep.txt"), "keep").unwrap();
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        let stamp = |p: &Path, at: SystemTime| fs::File::options().write(true).open(p).unwrap().set_modified(at).unwrap();
        for d in [&a, &b] {
            fs::create_dir(d.join("sub")).unwrap();
            fs::write(d.join("sub").join("f.txt"), "same").unwrap();
            stamp(&d.join("sub").join("f.txt"), when);
        }
        let never = AtomicBool::new(false);
        let tally_of = |p: &Path| match tally(p, &never) {
            Ok(t) => t,
            Err(_) => panic!("no tally of {}", p.display()),
        };
        assert_eq!(tally_of(&a), tally_of(&b));
        let ta = tally_of(&a);
        assert_eq!((ta.files, ta.folders, ta.links, ta.bytes), (1, 1, 0, 4));
        // The same size at another time.
        stamp(&b.join("sub").join("f.txt"), when + Duration::from_secs(2));
        assert_ne!(tally_of(&a), tally_of(&b));
        stamp(&b.join("sub").join("f.txt"), when);
        assert_eq!(tally_of(&a), tally_of(&b));
        // The same counts under another name.
        fs::rename(b.join("sub").join("f.txt"), b.join("sub").join("g.txt")).unwrap();
        let tb = tally_of(&b);
        assert_eq!((tb.files, tb.folders, tb.bytes), (ta.files, ta.folders, ta.bytes));
        assert_ne!(tb.digest, ta.digest);

        // Junctions made here read back, also one made by mklink.
        create_junction(&b.join("link"), &outside).unwrap();
        assert_eq!(fs::read_to_string(b.join("link").join("keep.txt")).unwrap(), "keep");
        assert_eq!(junction_target(&b.join("link")), Some(outside.clone()));
        assert_eq!(tally_of(&b).links, 1);
        let by_mklink = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(t.0.join("mk"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if by_mklink {
            assert_eq!(junction_target(&t.0.join("mk")), Some(outside.clone()));
            assert!(remove_tree(&t.0.join("mk")).is_err(), "a link is not a folder to empty");
            remove_junction(&t.0.join("mk"), &outside).unwrap();
            assert!(!t.0.join("mk").exists());
        }
        // Removal takes everything, read-only too, and links only as links.
        fs::write(b.join("ro.txt"), "ro").unwrap();
        set_attrs(&b.join("ro.txt"), FILE_ATTRIBUTE_READONLY);
        fs::create_dir(b.join("ro dir")).unwrap();
        set_attrs(&b.join("ro dir"), FILE_ATTRIBUTE_READONLY);
        remove_tree(&b).unwrap();
        assert!(!b.exists());
        assert_eq!(names(&outside), ["keep.txt"]);
    }

    #[test]
    fn a_record_file_that_cannot_be_read_is_put_aside() {
        let t = Temp::new("book");
        let book = book_in(&t);
        fs::write(&book.0, "not json").unwrap();
        // Said so, rather than shown as no relocations at all.
        assert!(book.list().err().is_some_and(|e| e.starts_with(BAD_BOOK)));
        assert!(book.read().is_err());
        let record = |id: &str| Relocation {
            id: id.into(),
            original: t.0.join("gone").to_string_lossy().into(),
            target: t.0.join("also gone").to_string_lossy().into(),
            size: 1,
            created_at: filetime_now(),
            state: "active".into(),
            pending: None,
            root_security: None,
        };
        book.keep(&record("1")).unwrap();
        let list = book.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].state, "broken");
        // Spoilt once more: the file put aside first is kept, the second goes next to it.
        fs::write(&book.0, "not json either").unwrap();
        book.keep(&record("2")).unwrap();
        assert_eq!(book.list().unwrap().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["2"]);
        let all = names(&t.0);
        let mut bad: Vec<String> = all.iter().filter(|n| n.ends_with(".bad")).map(|n| fs::read_to_string(t.0.join(n)).unwrap()).collect();
        bad.sort();
        assert_eq!(bad, ["not json", "not json either"]);
        // Nothing but these three: the writes went through temp files, renamed.
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(all.iter().all(|n| n.starts_with("relocations.json")), "{all:?}");
    }

    #[test]
    fn cloud_files_are_never_moved_either_way() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-cloud");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("sync");
        fs::create_dir_all(folder.join("deep")).unwrap();
        fs::write(folder.join("a.txt"), "a").unwrap();
        let online = folder.join("deep").join("online.docx");
        fs::write(&online, "only in the cloud").unwrap();
        let attrs = |p: &Path| fs::symlink_metadata(win::verbatim(p)).unwrap().file_attributes();
        let (plain, offline) = (attrs(&online), attrs(&online) | FILE_ATTRIBUTE_OFFLINE);
        set_attrs(&online, offline);
        // Turned down while counting: nothing copied, nothing touched, nothing written down.
        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "failed");
        let errors: Vec<&str> = job.errors.iter().map(|(_, why)| why.as_str()).collect();
        assert_eq!(errors, [format!("{HOLDS_CLOUD}: {}", real_path(&online).unwrap().display())]);
        assert_eq!(job.done_bytes, 0);
        assert_eq!(win::reparse_tag(&folder), Some(0));
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert!(book_in(&t).list().unwrap().is_empty());
        assert!(tally(&folder, &AtomicBool::new(false)).is_err(), "the last check turns it down too");

        // Moved while it was not; the undo turns it down once it is.
        set_attrs(&online, plain);
        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        let target = real_path(&other).unwrap().join("sync");
        let moved = target.join("deep").join("online.docx");
        set_attrs(&moved, offline);
        let id = book_in(&t).list().unwrap()[0].id.clone();
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "failed");
        let errors: Vec<&str> = job.errors.iter().map(|(_, why)| why.as_str()).collect();
        assert_eq!(errors, [format!("{HOLDS_CLOUD}: {}", moved.display())]);
        assert_eq!(junction_target(&folder), Some(target.clone()));
        assert_eq!(names(&home), ["sync"]);
        assert_eq!(book_in(&t).list().unwrap()[0].state, "active");
        set_attrs(&moved, plain);
        assert_eq!(restore_in(&t, &id).unwrap().state, "done");
        assert_eq!(win::reparse_tag(&folder), Some(0));
    }

    #[test]
    fn a_folder_that_may_not_be_renamed_is_turned_down_before_anything_is_copied() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-rights");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("locked");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("x.txt"), "x").unwrap();
        let icacls = |p: &Path, args: &[&str]| std::process::Command::new("icacls").arg(p).args(args).output().is_ok_and(|o| o.status.success());
        // Everyone is denied removing the folder, and anything in the folder it is in.
        let denied = icacls(&folder, &["/deny", "*S-1-1-0:(DE)"]) && icacls(&home, &["/deny", "*S-1-1-0:(DC)"]);
        let started = denied.then(|| relocate_in(&t, &folder, &other, ACROSS));
        let rights_back = icacls(&folder, &["/remove:d", "*S-1-1-0"]) & icacls(&home, &["/remove:d", "*S-1-1-0"]);
        let Some(started) = started else {
            eprintln!("icacls is not available; skipped");
            return;
        };
        assert!(rights_back);
        assert_eq!(refusal(started), NO_RIGHT_MOVE);
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert_eq!(names(&folder), ["x.txt"]);
        assert!(may_switch(&folder));
    }

    #[test]
    fn a_relocation_that_cannot_be_written_down_says_where_it_went() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-unrecorded");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("cache");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("c.bin"), "cache").unwrap();
        // The record file's folder is a file: nothing can be written there.
        fs::write(t.0.join("a file"), "").unwrap();
        let book = Book(t.0.join("a file").join(RELOCATIONS_FILE));
        let job = wait(begin_relocate(&folder, &other, Sink::default(), ACROSS, book).unwrap());
        // Moved all the same, and the reason says what to undo by hand.
        assert_eq!(job.state, "done", "{:?}", job.errors);
        let (original, target) = (real_path(&home).unwrap().join("cache"), real_path(&other).unwrap().join("cache"));
        assert_eq!(job.errors.len(), 1, "{:?}", job.errors);
        let why = &job.errors[0].1;
        assert!(why.starts_with(NOT_RECORDED), "{why}");
        assert!(why.contains(&format!("{} → {}", original.display(), target.display())), "{why}");
        assert_eq!(junction_target(&folder), Some(target.clone()));
        assert_eq!(fs::read_to_string(folder.join("c.bin")).unwrap(), "cache");
        assert_eq!(names(&home), ["cache"]);
        fs::remove_dir(&folder).unwrap();
    }

    /// `from`'s folders and files again at `to`, sizes and modified times and all.
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir(to).unwrap();
        for e in fs::read_dir(from).unwrap().flatten() {
            let (src, dst) = (e.path(), to.join(e.file_name()));
            if e.file_type().unwrap().is_dir() {
                copy_tree(&src, &dst);
            } else {
                fs::copy(&src, &dst).unwrap();
            }
        }
    }

    fn tally_of(path: &Path) -> Tally {
        tally(path, &AtomicBool::new(false)).ok().expect("a tally")
    }

    /// A folder with something in it, and what it holds.
    fn filled(path: &Path) -> Tally {
        fs::create_dir_all(path.join("sub")).unwrap();
        fs::write(path.join("a.txt"), "hello").unwrap();
        fs::write(path.join("sub").join("b.bin"), vec![7u8; 3000]).unwrap();
        tally_of(path)
    }

    /// The record of a job cut short in `state` ("relocating", "restoring").
    fn cut_short(t: &Temp, state: &str, paths: [&Path; 4], tally: Tally) -> String {
        cut_short_keeping(t, state, paths, tally, None)
    }

    /// `cut_short`, with the folder's own permissions recorded (`root_security`).
    fn cut_short_keeping(t: &Temp, state: &str, [original, target, old, copy]: [&Path; 4], tally: Tally, root: Option<RootSecurity>) -> String {
        let created_at = filetime_now();
        let record = Relocation {
            id: format!("{created_at:016x}"),
            original: shown(original),
            target: shown(target),
            size: tally.bytes,
            created_at,
            state: state.into(),
            pending: Some(Pending { old: shown(old), copy: shown(copy), tally }),
            root_security: root,
        };
        book_in(t).keep(&record).unwrap();
        record.id
    }

    fn state_of(t: &Temp, id: &str) -> Option<String> {
        book_in(t).list().unwrap().into_iter().find(|r| r.id == id).map(|r| r.state)
    }

    #[test]
    fn relocations_cut_short_are_put_right_at_the_next_start() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("recover-relocate");
        let (home, other) = (t.dir("home"), t.dir("other"));

        // Cut short in the last check: the folder is at `.ff-old`, the copy still hidden.
        let folder = home.join("lib");
        let want = filled(&folder);
        let (old, copy, target) = (home.join("lib.ff-old"), other.join(".lib.ff-relocating"), other.join("lib"));
        copy_tree(&folder, &copy);
        fs::rename(&folder, &old).unwrap();
        let taken = cut_short(&t, "relocating", [&folder, &target, &old, &copy], want);
        assert_eq!(state_of(&t, &taken).as_deref(), Some("broken"));
        recover(&book_in(&t));
        assert_eq!(tally_of(&folder), want);
        assert_eq!(names(&home), ["lib"]);
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert_eq!(state_of(&t, &taken), None);

        // Cut short after the junction: only the original's removal was left.
        let folder = home.join("lib2");
        let want = filled(&folder);
        let (old, target) = (home.join("lib2.ff-old"), other.join("lib2"));
        copy_tree(&folder, &target);
        fs::rename(&folder, &old).unwrap();
        create_junction(&folder, &real_path(&target).unwrap()).unwrap();
        let switched = cut_short(&t, "relocating", [&folder, &real_path(&target).unwrap(), &old, &other.join(".lib2.ff-relocating")], want);
        assert_eq!(state_of(&t, &switched).as_deref(), Some("active"));
        recover(&book_in(&t));
        assert!(gone(&old));
        assert_eq!(fs::read_to_string(folder.join("a.txt")).unwrap(), "hello");
        assert!(book_in(&t).read().unwrap().iter().all(|r| r.pending.is_none()));
        assert_eq!(state_of(&t, &switched).as_deref(), Some("active"));
        assert_eq!(restore_in(&t, &switched).unwrap().state, "done");
        assert_eq!(tally_of(&folder), want);

        // Where it cannot tell (a program made the folder again; or the folder there
        // is not the one moved), nothing is touched, and it shows as broken.
        let folder = home.join("lib3");
        let want = filled(&folder);
        let (old, copy) = (home.join("lib3.ff-old"), other.join(".lib3.ff-relocating"));
        copy_tree(&folder, &copy);
        fs::rename(&folder, &old).unwrap();
        fs::create_dir(&folder).unwrap();
        let unsure = cut_short(&t, "relocating", [&folder, &other.join("lib3"), &old, &copy], want);
        let folder = home.join("lib4");
        let want4 = filled(&other.join("lib4"));
        fs::create_dir(&folder).unwrap();
        let replaced = cut_short(&t, "relocating", [&folder, &other.join("lib4"), &home.join("lib4.ff-old"), &other.join(".lib4.ff-relocating")], want4);
        recover(&book_in(&t));
        assert_eq!((tally_of(&old), tally_of(&copy), tally_of(&other.join("lib4"))), (want, want, want4));
        assert_eq!(state_of(&t, &unsure).as_deref(), Some("broken"));
        assert_eq!(state_of(&t, &replaced).as_deref(), Some("broken"));

        // A tally written another way (an older build) matches nothing: the
        // folder gets its name back, but neither copy is deleted.
        let folder = home.join("lib5");
        let want5 = filled(&folder);
        assert_eq!(want5.scheme, TALLY_SCHEME);
        let (old, copy) = (home.join("lib5.ff-old"), other.join(".lib5.ff-relocating"));
        copy_tree(&folder, &copy);
        fs::rename(&folder, &old).unwrap();
        let older = Tally { scheme: 0, ..want5 };
        let stale = cut_short(&t, "relocating", [&folder, &other.join("lib5"), &old, &copy], older);
        let read_back = book_in(&t).read().unwrap().into_iter().find(|r| r.id == stale).and_then(|r| r.pending).unwrap();
        assert_eq!(read_back.tally, older);
        recover(&book_in(&t));
        assert_eq!((tally_of(&folder), tally_of(&copy)), (want5, want5));
        // A record from before `scheme` was written down reads as scheme 0.
        let text = serde_json::to_string(&want5).unwrap().replace(&format!(r#","scheme":{TALLY_SCHEME}"#), "");
        assert!(!text.contains("scheme"), "{text}");
        assert_eq!(serde_json::from_str::<Tally>(&text).unwrap(), older);

        // Never while a relocation or undo runs, in this process or another.
        let book = book_in(&t);
        let lock = hold_exclusive(&book.lock_path()).unwrap();
        assert_eq!(Busy::claim(&book).err().as_deref(), Some(RELOCATE_BUSY));
        // The list of another window: a relocation running there is not shown as broken.
        assert_eq!(state_of(&t, &unsure), None);
        // Nor while another window's start puts things right, which is said so.
        let recovery = hold_exclusive(&book.recovery_lock_path()).unwrap();
        assert_eq!(Busy::claim(&book).err().as_deref(), Some(RECOVER_BUSY));
        drop((lock, recovery));
        assert_eq!(state_of(&t, &unsure).as_deref(), Some("broken"));
        let busy = Busy::claim(&book).unwrap();
        assert!(busy.lock.is_some() && busy.recovery.is_none());
        assert_eq!(Busy::claim(&book).err().as_deref(), Some(RELOCATE_BUSY));
        drop(busy);
        // This process's own start.
        let busy = Busy::claim_for_recovery(&book).unwrap();
        assert!(busy.lock.is_some() && busy.recovery.is_some());
        assert_eq!(Busy::claim(&book).err().as_deref(), Some(RECOVER_BUSY));
        drop(busy);
        assert!(!Busy::taken(&book));
        drop(Busy::claim(&book).unwrap());
    }

    #[test]
    fn a_folder_made_again_meanwhile_keeps_the_record() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-remade");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("game");
        let want = filled(&folder);
        // A launcher makes the folder again just before the junction would go there.
        let settings = Settings {
            junction: |link, _| fs::create_dir(link).and(Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32))),
            ..ACROSS
        };
        let job = relocate_in(&t, &folder, &other, settings).unwrap();
        assert_eq!(job.state, "failed");
        assert!(job.errors.iter().any(|(_, why)| why.contains(STUCK)), "{:?}", job.errors);
        // Nothing is lost, and the record stays (broken) until the user has looked.
        assert_eq!(tally_of(&home.join("game.ff-old")), want);
        assert_eq!(tally_of(&other.join("game")), want);
        let states = || book_in(&t).list().unwrap().into_iter().map(|r| r.state).collect::<Vec<_>>();
        assert_eq!(states(), ["broken"]);
        recover(&book_in(&t));
        assert_eq!(tally_of(&home.join("game.ff-old")), want);
        assert_eq!(tally_of(&other.join("game")), want);
        assert_eq!(states(), ["broken"]);
    }

    #[test]
    fn undos_cut_short_are_put_right_at_the_next_start() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("recover-restore");
        let (home, other) = (t.dir("home"), t.dir("other"));

        // Cut short once the folder was taken away: its junction leads nowhere.
        let (folder, target) = (home.join("lib"), real_path(&other).unwrap().join("lib"));
        let want = filled(&target);
        create_junction(&folder, &target).unwrap();
        let (old, copy) = (other.join("lib.ff-old"), home.join(".lib.ff-restoring"));
        copy_tree(&target, &copy);
        fs::rename(&target, &old).unwrap();
        let taken = cut_short(&t, "restoring", [&folder, &target, &old, &copy], want);
        assert_eq!(state_of(&t, &taken).as_deref(), Some("broken"));
        recover(&book_in(&t));
        assert!(linked(&folder, &target));
        assert_eq!(tally_of(&target), want);
        assert_eq!(names(&home), ["lib"]);
        assert_eq!(names(&other), ["lib"]);
        assert_eq!(state_of(&t, &taken).as_deref(), Some("active"));

        // Cut short with the junction gone too: it is made again.
        fs::rename(&target, &old).unwrap();
        fs::remove_dir(&folder).unwrap();
        copy_tree(&old, &copy);
        let unlinked = cut_short(&t, "restoring", [&folder, &target, &old, &copy], want);
        recover(&book_in(&t));
        assert!(linked(&folder, &target));
        assert_eq!(names(&home), ["lib"]);
        assert_eq!(state_of(&t, &unlinked).as_deref(), Some("active"));

        // Given up with the folder back but not its junction (an undo that could not
        // make it again): the junction is made.
        fs::remove_dir(&folder).unwrap();
        copy_tree(&target, &copy);
        let half_back = cut_short(&t, "restoring", [&folder, &target, &old, &copy], want);
        recover(&book_in(&t));
        assert!(linked(&folder, &target));
        assert_eq!(names(&home), ["lib"]);
        assert_eq!(names(&other), ["lib"]);
        assert_eq!(state_of(&t, &half_back).as_deref(), Some("active"));

        // Cut short once the copy had the folder's name: only the old target was left.
        let (folder, target) = (home.join("lib2"), real_path(&other).unwrap().join("lib2"));
        let want = filled(&folder);
        let old = other.join("lib2.ff-old");
        copy_tree(&folder, &old);
        let named = cut_short(&t, "restoring", [&folder, &target, &old, &home.join(".lib2.ff-restoring")], want);
        assert_eq!(state_of(&t, &named).as_deref(), Some("restored"));
        recover(&book_in(&t));
        assert!(gone(&old));
        assert_eq!(tally_of(&folder), want);
        assert_eq!(state_of(&t, &named).as_deref(), Some("restored"));
        assert_eq!(names(&other), ["lib"]);
        fs::remove_dir(home.join("lib")).unwrap();
    }

    #[test]
    fn an_elevated_start_leaves_what_was_cut_short_for_a_normal_one() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("recover-elevated");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("lib");
        let want = filled(&folder);
        let (old, copy, target) = (home.join("lib.ff-old"), other.join(".lib.ff-relocating"), other.join("lib"));
        copy_tree(&folder, &copy);
        fs::rename(&folder, &old).unwrap();
        cut_short(&t, "relocating", [&folder, &target, &old, &copy], want);
        // Elevated: nothing is touched, and a job that fails says why, by the folder.
        let job = wait(recover_relocations_in(&book_in(&t), true, Sink::default()).unwrap());
        assert_eq!((job.op.as_str(), job.state.as_str()), ("restore", "failed"));
        assert_eq!(job.errors, [(folder.to_string_lossy().into_owned(), ELEVATED_JOURNALS.to_string())]);
        assert!(gone(&folder));
        assert_eq!((tally_of(&old), tally_of(&copy)), (want, want));
        assert!(book_in(&t).read().unwrap()[0].pending.is_some());
        // The next start with normal rights puts it right; then there is nothing left to tell.
        assert!(recover_relocations_in(&book_in(&t), false, Sink::default()).is_none());
        assert_eq!(tally_of(&folder), want);
        assert_eq!(names(&home), ["lib"]);
        assert!(names(&other).is_empty(), "{:?}", names(&other));
        assert!(recover_relocations_in(&book_in(&t), true, Sink::default()).is_none());

        // A merge cut short: its page stays for a normal start, which puts the copy back.
        let book = LinkBook(t.0.join("merges"));
        let data = content(50_000, 16);
        let (keep, copy, _) = twins(&t, "x.bin", &data);
        let s = cut_short_merge(&book, &keep, &copy, false);
        let job = wait(recover_links_in(&book, true, test_bin, Sink::default()).unwrap());
        assert_eq!((job.op.as_str(), job.state.as_str()), ("link", "failed"));
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), ELEVATED_JOURNALS.to_string())]);
        assert_eq!(book.pages().len(), 1);
        assert!(gone(&copy) && !gone(&s.temp));
        let job = next_start(&book, test_bin).unwrap();
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), LINK_CUT_BACK.to_string())]);
        untouched(&keep, &copy, &data, &data);
        assert!(recover_links_in(&book, true, test_bin, Sink::default()).is_none());
    }

    #[test]
    fn records_that_say_more_than_a_job_would_have_left_are_not_acted_on() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("recover-forged");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let places = guarded_places();
        let record = |state: &str, [original, target, old, copy]: [&Path; 4]| Relocation {
            id: "x".into(),
            original: shown(original),
            target: shown(target),
            size: 0,
            created_at: 0,
            state: state.into(),
            pending: Some(Pending { old: shown(old), copy: shown(copy), tally: Tally::default() }),
            root_security: None,
        };
        let holds = |state: &str, paths: [&Path; 4]| {
            let r = record(state, paths);
            record_holds(&r, r.pending.as_ref().unwrap(), &places)
        };
        let (lib, there) = (home.join("lib"), other.join("lib"));
        // What a relocation and an undo of this app leave.
        assert!(holds("relocating", [&lib, &there, &home.join("lib.ff-old"), &other.join(".lib.ff-relocating")]));
        assert!(holds("relocating", [&lib, &there, &home.join("LIB (2).ff-old"), &other.join(".lib (3).ff-relocating")]));
        assert!(holds("restoring", [&lib, &there, &other.join("lib.ff-old"), &home.join(".lib.ff-restoring")]));
        // Not what is to be removed or put back: another folder, one elsewhere, the other kind of name.
        assert!(!holds("relocating", [&lib, &there, &home.join("docs"), &other.join(".lib.ff-relocating")]));
        assert!(!holds("relocating", [&lib, &there, &other.join("lib.ff-old"), &other.join(".lib.ff-relocating")]));
        assert!(!holds("relocating", [&lib, &there, &home.join("lib.ff-old"), &home.join(".lib.ff-relocating")]));
        assert!(!holds("relocating", [&lib, &there, &home.join("lib.ff-old"), &other.join(".lib.ff-restoring")]));
        assert!(!holds("relocating", [&lib, &there, &home.join("lib.ff-old"), &other.join(".docs.ff-relocating")]));
        assert!(!holds("relocating", [&lib, &there, Path::new(""), &other.join(".lib.ff-relocating")]));
        assert!(!holds("active", [&lib, &there, &home.join("lib.ff-old"), &other.join(".lib.ff-relocating")]));
        // Places a relocation never takes a folder from nor puts one: Windows, every Program Files, the app's data.
        let windows = known_folder(&FOLDERID_Windows).unwrap();
        let drivers = windows.join("System32").join("drivers");
        let next_to = |p: &Path, name: &str| p.parent().unwrap().join(name);
        assert!(!holds("relocating", [&drivers, &other.join("drivers"), &next_to(&drivers, "drivers.ff-old"), &other.join(".drivers.ff-relocating")]));
        assert!(!holds("restoring", [&lib, &windows.join("lib"), &windows.join("lib.ff-old"), &home.join(".lib.ff-restoring")]));
        let programs = Path::new(r"D:\Program Files\App");
        assert!(!holds("relocating", [&lib, programs, &home.join("lib.ff-old"), Path::new(r"D:\Program Files\.App.ff-relocating")]));
        let data = crate::scan::data_dir().join("plans");
        assert!(!holds("relocating", [&data, &there, &next_to(&data, "plans.ff-old"), &other.join(".plans.ff-relocating")]));
        // Undo from the list checks both ends the same way.
        assert!(movable(&lib, &places) && movable(&there, &places));
        assert!(!movable(&drivers, &places) && !movable(&windows.join("x"), &places) && !movable(programs, &places));
        assert!(!movable(&data, &places) && !movable(Path::new("relative"), &places));

        // Such a record at a start: a folder of the user's that it calls the one taken
        // away is not given another name, however well its tally matches.
        let docs = home.join("docs");
        let want = filled(&docs);
        let forged = Relocation { pending: Some(Pending { old: shown(&docs), copy: shown(&other.join(".lib.ff-relocating")), tally: want }), ..record("relocating", [&lib, &there, &docs, &docs]) };
        book_in(&t).keep(&forged).unwrap();
        recover(&book_in(&t));
        assert_eq!(tally_of(&docs), want);
        assert!(gone(&lib));
        assert_eq!(book_in(&t).list().unwrap()[0].state, "broken");

        // A merge's page whose hidden name is not the copy's own: the file there is not moved.
        let lbook = LinkBook(t.0.join("merges"));
        let data = content(40_000, 17);
        let (keep, copy, _) = twins(&t, "y.bin", &data);
        let stranger = copy.with_file_name("stranger.bin");
        fs::rename(&copy, &stranger).unwrap();
        let s = Switch {
            path: copy.clone(),
            temp: stranger.clone(),
            keep: keep.clone(),
            kept: key(&keep),
            copy: key(&stranger),
            attrs: attrs(&stranger),
            size: 40_000,
            modified: fs::metadata(&stranger).unwrap().last_write_time() as i64,
        };
        let mut page = lbook.page().unwrap();
        page.write(&s).unwrap();
        drop(page);
        let job = next_start(&lbook, test_bin).unwrap();
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), LINK_CUT_ODD.to_string())]);
        assert!(gone(&copy));
        assert_eq!(fs::read(&stranger).unwrap(), data);
        assert!(lbook.pages().is_empty());
        // Nor one for a file of Windows itself.
        let notepad = windows.join("notepad.exe");
        let page_of = |path: &Path| Switch {
            path: path.into(),
            temp: marked_name(path, true, LINK_EXT, 1).unwrap(),
            keep: s.keep.clone(),
            kept: s.kept,
            copy: s.copy,
            attrs: s.attrs,
            size: s.size,
            modified: s.modified,
        };
        assert!(!switch_holds(&page_of(&notepad), Some(&windows)));
        assert!(switch_holds(&page_of(&copy), Some(&windows)));
    }

    /// The DACL of the entry at `path` itself: whether it is protected, and its ACEs in SDDL, sorted.
    fn dacl_of(path: &Path) -> (bool, Vec<String>) {
        use windows_sys::Win32::Security::Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1};
        let have = Security::of(&open_itself(path, READ_CONTROL, false).unwrap(), false).unwrap();
        let mut text: PWSTR = null_mut();
        let ok = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(have.0, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut text, null_mut()) };
        assert!(ok != 0, "{}", io::Error::last_os_error());
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        unsafe { LocalFree(text.cast()) };
        let mut aces: Vec<String> = sddl.split('(').skip(1).map(|a| a.trim_end_matches(')').to_string()).collect();
        aces.sort();
        (have.protected(), aces)
    }

    /// Gives `path` the DACL `sddl` the way Explorer's security tab does (`D:P(...)`:
    /// protected; `D:(...)`: these ACEs besides what its folder passes down).
    fn set_dacl(path: &Path, sddl: &str) {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
        };
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let text = win::wide(sddl);
        assert!(unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } != 0);
        let sd = Security(sd);
        let protect = if sddl.starts_with("D:P") { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
        let name = win::wide(&path.to_string_lossy());
        let e = unsafe {
            SetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | protect, null_mut(), null_mut(), sd.dacl().unwrap(), null())
        };
        assert_eq!(e, ERROR_SUCCESS, "{}", path.display());
    }

    /// Gives `path` the protected DACL `sddl` as it is (`put_dacl`): nothing read back, nothing passed on.
    fn put_sddl(path: &Path, sddl: &str) {
        use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let text = win::wide(sddl);
        assert!(unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } != 0);
        let sd = Security(sd);
        put_dacl(&open_itself(path, WRITE_DAC, false).unwrap(), sd.dacl().unwrap(), true).unwrap();
    }

    /// Owner rights (whoever owns it: the user, or Administrators for an elevated test), SYSTEM and
    /// Administrators may do anything; the machine's users may only read.
    const KEPT_TO_ITSELF: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

    #[test]
    fn a_relocated_folder_keeps_its_permissions_both_ways() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-acl");
        let (home, other) = (t.dir("home"), t.dir("other drive"));
        let folder = home.join("app");
        fs::create_dir_all(folder.join("open")).unwrap();
        fs::write(folder.join("app.exe"), "exe").unwrap();
        fs::write(folder.join("open").join("shared.txt"), "shared").unwrap();
        fs::write(folder.join("own.txt"), "own").unwrap();
        fs::create_dir(folder.join("plain")).unwrap();
        fs::write(folder.join("plain").join("p.txt"), "p").unwrap();
        // The folder keeps to itself; one folder in it also lets signed-in users change it;
        // one file has permissions all of its own; the rest get what passes down.
        set_dacl(&folder, KEPT_TO_ITSELF);
        set_dacl(&folder.join("open"), "D:(A;OICI;0x1301bf;;;AU)");
        set_dacl(&folder.join("own.txt"), "D:P(A;;FA;;;OW)(A;;FA;;;SY)");
        let entries = ["", "app.exe", "open", "open\\shared.txt", "own.txt", "plain", "plain\\p.txt"];
        let at = |root: &Path, e: &str| if e.is_empty() { root.to_path_buf() } else { root.join(e) };
        let before: Vec<(bool, Vec<String>)> = entries.iter().map(|e| dacl_of(&at(&folder, e))).collect();
        assert!(before[0].0 && before[4].0 && !before[2].0);
        assert!(before[2].1.iter().any(|a| a.ends_with(";AU") && !a.contains("ID")), "{:?}", before[2]);
        // Where it goes, the folder passes down its own (the temp folder's: the user's, and more).
        let there_by_default = dacl_of(&other);

        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        let target = real_path(&other).unwrap().join("app");
        for (e, want) in entries.iter().zip(&before) {
            let got = dacl_of(&at(&target, e));
            if e.is_empty() {
                // Protected, every ACE its own, and the same as where it came from.
                assert!(got.0, "the folder is protected from what passes down there");
                assert_eq!(got.1, want.1);
                assert!(got.1.iter().all(|a| !a.contains("ID")));
                assert!(there_by_default.1.iter().filter(|a| !want.1.contains(a)).all(|a| !got.1.contains(a)));
            } else {
                assert_eq!(&got, want, "{e}");
            }
        }

        // And back again: the same permissions where it was.
        let id = book_in(&t).list().unwrap()[0].id.clone();
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        for (e, want) in entries.iter().zip(&before) {
            let got = dacl_of(&at(&folder, e));
            assert_eq!(got.1, want.1, "{e}");
            assert_eq!(got.0, want.0, "{e}");
        }
    }

    #[test]
    fn a_relocation_that_cannot_keep_the_permissions_changes_nothing() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-acl-no");
        let (home, other) = (t.dir("home"), t.dir("other drive"));
        // Its permissions cannot be read, not even by its owner: the folder's own, or one file's.
        for (name, unreadable) in [("root", ""), ("inner", "secret.bin")] {
            let folder = home.join(name);
            let want = filled(&folder);
            fs::write(folder.join("secret.bin"), "s").unwrap();
            let shut = "D:P(D;;RC;;;OW)(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
            let shut_one = if unreadable.is_empty() { folder.clone() } else { folder.join(unreadable) };
            put_sddl(&shut_one, shut);
            let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
            assert_eq!(job.state, "failed", "{name}: {:?}", job.errors);
            let why = &job.errors.last().unwrap().1;
            if unreadable.is_empty() {
                assert!(why.starts_with("권한 설정을 그대로 옮기지 못해서 옮기지 않았어요"), "{why}");
            } else {
                // CopyFileEx reads a file's security too, so it may turn the file down before
                // `keep_own_security` gets to it: either way the file is named and nothing moves.
                assert!(job.errors.iter().any(|(p, _)| p.ends_with("secret.bin")), "{:?}", job.errors);
                assert!(why.starts_with("복사하지 못한 항목이 있어서 옮기지 않았어요"), "{why}");
            }
            // Nothing switched, no copy left, no record.
            assert_eq!(win::reparse_tag(&folder), Some(0));
            assert!(names(&other).is_empty(), "{name}: {:?}", names(&other));
            assert!(book_in(&t).list().unwrap().is_empty());
            assert_eq!(tally_of(&folder).files, want.files + 1);
            put_sddl(&shut_one, KEPT_TO_ITSELF);
        }
    }

    #[test]
    fn an_inherited_folder_comes_back_inheriting_and_its_junction_carries_its_permissions() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-acl-inherit");
        let (home, other) = (t.dir("home"), t.dir("other drive"));
        let folder = home.join("app");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a.txt"), "a").unwrap();
        // Not protected: one ACE of its own, the rest passed down from `home`.
        set_dacl(&folder, "D:(A;OICI;0x1301bf;;;AU)");
        let before = dacl_of(&folder);
        assert!(!before.0, "an inherited DACL, not a protected one");
        assert!(before.1.iter().any(|a| a.ends_with(";AU") && !a.contains("ID")), "its own ACE: {:?}", before.1);
        assert!(before.1.iter().any(|a| a.contains("ID")), "and inherited ones: {:?}", before.1);

        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        // The junction left in its place is reached under the folder's own permissions.
        assert_eq!(win::reparse_tag(&folder), Some(IO_REPARSE_TAG_MOUNT_POINT));
        assert_eq!(dacl_of(&folder), before, "the junction carries the folder's DACL");

        // Undo: the folder is back, still inheriting — not turned into a protected copy.
        let id = book_in(&t).list().unwrap()[0].id.clone();
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(win::reparse_tag(&folder), Some(0));
        assert_eq!(dacl_of(&folder), before, "its own permissions back exactly, inherited still inherited");

        // A record from before this was kept (no root_security) is left to the copy
        // routines: the folder comes back as their protected copy, and the undo still goes.
        let folder = home.join("plain");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a.txt"), "a").unwrap();
        set_dacl(&folder, "D:(A;OICI;0x1301bf;;;AU)");
        assert!(!dacl_of(&folder).0);
        let job = relocate_in(&t, &folder, &other, ACROSS).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        let book = book_in(&t);
        let id = book.list().unwrap().iter().find(|r| r.original.ends_with("plain")).unwrap().id.clone();
        let bare = book.read().unwrap().into_iter().find(|r| r.id == id).map(|r| Relocation { root_security: None, ..r }).unwrap();
        book.keep(&bare).unwrap();
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(dacl_of(&folder).0, "without a record the copy's protected DACL is left as it is");
    }

    #[test]
    fn a_folder_that_lets_nobody_add_to_it_is_relocated_its_copy_its_owners_while_filled() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("relocate-acl-locked");
        let (home, other) = (t.dir("home"), t.dir("other drive"));
        let folder = home.join("locked");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(folder.join("a.txt"), "a").unwrap();
        fs::write(folder.join("sub").join("b.bin"), content(2 << 20, 31)).unwrap();
        fs::write(folder.join("secret.kdbx"), "s").unwrap();
        // Signed-in users may read, run and delete, passed down, but nobody may add anything;
        // one file is its owner's alone, stricter than its folder.
        set_dacl(&folder, "D:P(A;OICI;0x1300a9;;;AU)(A;OICI;FA;;;SY)");
        set_dacl(&folder.join("secret.kdbx"), "D:P(A;;FA;;;OW)(A;;FA;;;SY)");
        let entries = ["", "a.txt", "sub", "sub\\b.bin", "secret.kdbx"];
        let at = |root: &Path, e: &str| if e.is_empty() { root.to_path_buf() } else { root.join(e) };
        let before: Vec<(bool, Vec<String>)> = entries.iter().map(|e| dacl_of(&at(&folder, e))).collect();
        // What the copy and a folder in it are while the job is at `sub\b.bin`.
        let filling = Arc::new(Mutex::new(None));
        let (seen, copy) = (filling.clone(), other.join(".locked.ff-relocating"));
        let hook = move |job: &JobInfo| {
            if job.current.as_deref().is_some_and(|c| c.ends_with(r"sub\b.bin")) {
                seen.lock().unwrap().get_or_insert_with(|| (dacl_of(&copy), dacl_of(&copy.join("sub"))));
            }
        };
        let sink = Sink { on_progress: Some(Arc::new(hook)), ..Sink::default() };
        let job = wait(begin_relocate(&folder, &other, sink, ACROSS, book_in(&t)).unwrap());
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        let (top, sub) = filling.lock().unwrap().clone().expect("seen while filled");
        assert_eq!(top, (true, vec!["A;OICI;FA;;;OW".to_string()]), "the copy is its owner's alone while filled");
        assert_eq!(sub, (false, vec!["A;OICIID;FA;;;OW".to_string()]), "and so is all that goes in");
        // Each with what it had, the folder protected with every ACE its own.
        let target = real_path(&other).unwrap().join("locked");
        for (e, want) in entries.iter().zip(&before) {
            assert_eq!(&dacl_of(&at(&target, e)), want, "{e}");
        }
        // And back again, the same.
        let id = book_in(&t).list().unwrap()[0].id.clone();
        let job = restore_in(&t, &id).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        for (e, want) in entries.iter().zip(&before) {
            assert_eq!(&dacl_of(&at(&folder, e)), want, "{e}");
        }
        set_dacl(&folder.join("secret.kdbx"), "D:P(A;;FA;;;AU)");
        set_dacl(&folder, "D:P(A;OICI;FA;;;AU)(A;OICI;FA;;;SY)");
    }

    #[test]
    fn a_start_puts_on_the_permissions_a_job_cut_short_did_not() {
        let _turn = one_relocation_at_a_time();
        let t = Temp::new("recover-acl");
        let (home, other) = (t.dir("home"), t.dir("other"));
        let folder = home.join("app");
        let want = filled(&folder);
        set_dacl(&folder, "D:(A;OICI;0x1301bf;;;AU)");
        let before = dacl_of(&folder);
        let rs = root_security(&folder).unwrap();

        // A relocation cut short right after its junction was made: owner-only still.
        let (old, target) = (home.join("app.ff-old"), other.join("app"));
        copy_tree(&folder, &target);
        fs::rename(&folder, &old).unwrap();
        let target = real_path(&target).unwrap();
        create_junction(&folder, &target).unwrap();
        assert_ne!(dacl_of(&folder), before);
        let id = cut_short_keeping(&t, "relocating", [&folder, &target, &old, &other.join(".app.ff-relocating")], want, Some(rs.clone()));
        recover(&book_in(&t));
        assert_eq!(state_of(&t, &id).as_deref(), Some("active"));
        assert_eq!(dacl_of(&folder), before, "the junction carries the folder's permissions");
        assert!(gone(&old));

        // An undo cut short right after its copy got the folder's name: the copy's protected DACL still.
        fs::remove_dir(&folder).unwrap();
        copy_tree(&target, &folder);
        put_sddl(&folder, "D:P(A;OICI;FA;;;OW)(A;OICI;0x1301bf;;;AU)");
        let old = other.join("app.ff-old");
        fs::rename(&target, &old).unwrap();
        let id = cut_short_keeping(&t, "restoring", [&folder, &target, &old, &home.join(".app.ff-restoring")], want, Some(rs));
        recover(&book_in(&t));
        assert_eq!(state_of(&t, &id).as_deref(), Some("restored"));
        assert_eq!(dacl_of(&folder), before, "its own permissions back, inherited still inherited");
        assert!(gone(&old));
        assert_eq!(tally_of(&folder), want);
    }

    #[test]
    fn a_child_is_opened_in_its_parent_not_down_a_path() {
        let t = Temp::new("open-child");
        let (root, outside) = (t.dir("root"), t.dir("outside"));
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("f.txt"), "f").unwrap();
        create_junction(&root.join("link"), &outside).unwrap();
        // Held with no right of its own: an open relative to it needs none (FILE_TRAVERSE neither).
        let parent = open_itself(&root, 0, true).unwrap();
        // A real child folder, opened in the held parent: the very entry its path names.
        let sub = open_child(&parent, OsStr::new("sub"), 0, true).unwrap();
        assert_eq!(names(&root.join("sub")), ["f.txt"]);
        let f = open_child(&sub, OsStr::new("f.txt"), 0, false).unwrap();
        assert_eq!(dups::facts(&f).and_then(|f| f.key), key_at(&root.join("sub").join("f.txt")));
        drop(f);
        assert_eq!(kind_open(&sub).unwrap().0, Kind::Dir);
        assert_eq!(dups::facts(&sub).and_then(|f| f.key), key_at(&root.join("sub")));
        // A junction child is the reparse point itself, never what it leads to.
        let link = open_child(&parent, OsStr::new("link"), 0, false).unwrap();
        assert_eq!(kind_open(&link).unwrap().0, Kind::Link { dir: true });
        assert_eq!(names(&outside), [] as [String; 0]);
        // Anything that would lead out of the parent is turned down.
        for bad in ["", ".", "..", "a\\b", "a/b"] {
            assert!(open_child(&parent, OsStr::new(bad), 0, false).is_err(), "{bad}");
        }
        drop((parent, sub, link));
        fs::remove_dir(root.join("link")).unwrap();
    }

    /// Read only for signed-in users (icacls `(R)`, Explorer's "Read" without "Read & execute":
    /// no FILE_TRAVERSE), passed down; SYSTEM may do anything. Its owner may still change it.
    const READ_ONLY: &str = "D:P(A;OICI;0x120089;;;AU)(A;OICI;FA;;;SY)";

    #[test]
    fn folders_that_only_let_users_read_are_copied_and_removed() {
        let t = Temp::new("read-only-acl");
        let (src, dst) = (t.dir("src"), t.dir("dst"));
        let ro = src.join("ro");
        fs::create_dir_all(ro.join("sub")).unwrap();
        fs::write(ro.join("a.txt"), "a").unwrap();
        fs::write(ro.join("sub").join("b.txt"), "b").unwrap();
        set_dacl(&ro, READ_ONLY);
        let job = wait(start("copy", vec![shown(&ro)], shown(&dst), Sink::default(), TEST).unwrap());
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert_eq!(fs::read_to_string(dst.join("ro").join("a.txt")).unwrap(), "a");
        assert_eq!(fs::read_to_string(dst.join("ro").join("sub").join("b.txt")).unwrap(), "b");
        // An empty one (a copy that could not be filled) goes: DELETE alone, which its folder
        // grants, is all it takes.
        let empty = t.dir("empty");
        set_dacl(&empty, READ_ONLY);
        remove_tree(&empty).unwrap();
        assert!(gone(&empty));
        // A file a program holds without letting others read it goes too: nothing asked of it
        // is checked against how it is shared but DELETE.
        let tree = t.dir("tree");
        fs::write(tree.join("log.txt"), "log").unwrap();
        let held = OpenOptions::new().write(true).share_mode(FILE_SHARE_DELETE).open(tree.join("log.txt")).unwrap();
        remove_tree(&tree).unwrap();
        drop(held);
        assert!(gone(&tree));
        set_dacl(&ro, "D:P(A;OICI;FA;;;AU)(A;OICI;FA;;;SY)");
    }

    /// Sets the empty folder `dir` as a mount point to `to`, in place: what another program can
    /// do to a folder a job holds open, which it cannot rename.
    fn mount_in_place(dir: &Path, to: &Path) {
        let data = junction_data(to).unwrap();
        let f = OpenOptions::new().write(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).open(win::verbatim(dir)).unwrap();
        let mut unused = 0u32;
        let set = unsafe {
            DeviceIoControl(f.as_raw_handle() as HANDLE, FSCTL_SET_REPARSE_POINT, data.as_ptr().cast(), data.len() as u32, null_mut(), 0, &mut unused, null_mut())
        };
        assert!(set != 0, "{}", io::Error::last_os_error());
    }

    #[test]
    fn copies_and_moves_never_follow_a_held_folder_set_as_a_mount_point() {
        let t = Temp::new("mounted");
        let (src, dst, outside) = (t.dir("src"), t.dir("dst"), t.dir("outside"));
        let names_there = ["a.bin", "b.txt", "c.txt"];
        for (name, text) in names_there.iter().zip(["secret a", "secret b", ""]) {
            fs::write(outside.join(name), text).unwrap();
        }
        // `op` on src\<name>, whose folder `x` is emptied and set as a mount point to `outside` in
        // place when the job gets to its file `at` (`x` held and listed by then).
        let run_mounting = |op: &str, name: &str, at: &str, settings: Settings| {
            let x = src.join(name).join("x");
            fs::create_dir_all(&x).unwrap();
            for n in names_there {
                fs::write(x.join(n), format!("mine {n}")).unwrap();
            }
            let (done, at, out, inside) = (Arc::new(AtomicBool::new(false)), x.join(at), outside.clone(), x.clone());
            let hook = move |job: &JobInfo| {
                if job.current.as_deref() == Some(&*at.to_string_lossy()) && !done.swap(true, Ordering::SeqCst) {
                    for n in names(&inside) {
                        fs::remove_file(inside.join(n)).unwrap();
                    }
                    mount_in_place(&inside, &out);
                }
            };
            let sink = Sink { on_progress: Some(Arc::new(hook)), ..Sink::default() };
            let job = wait(start(op, vec![shown(&src.join(name))], shown(&dst), sink, settings).unwrap());
            (job, x)
        };
        let at_x = |x: &Path, n: &str, why: &str| (x.join(n).to_string_lossy().into_owned(), why.to_string());
        let outside_intact = || {
            for (name, text) in names_there.iter().zip(["secret a", "secret b", ""]) {
                assert_eq!(fs::read_to_string(outside.join(name)).unwrap(), text, "{name}");
            }
        };

        // A copy, the folder mounted as it gets to its first file: the file CopyFileEx opens down
        // the path is not the one the folder holds, and the rest are not in the folder any more.
        let (job, x) = run_mounting("copy", "copy", "a.bin", TEST);
        assert_eq!(job.errors[0], at_x(&x, "a.bin", ENTRY_CHANGED));
        // Opened in the folder itself, which is a mount point now: turned down (Windows says it
        // cannot resolve the name), never followed.
        let paths: Vec<&str> = job.errors.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, [at_x(&x, "a.bin", "").0, at_x(&x, "b.txt", "").0, at_x(&x, "c.txt", "").0]);
        assert!(names(&dst.join("copy").join("x")).is_empty(), "nothing from outside: {:?}", names(&dst.join("copy").join("x")));
        outside_intact();
        fs::remove_dir(&x).unwrap();

        // A move to another volume, mounted at its last file (an empty one), the others copied by
        // then: that one is not copied from outside, and what is outside under the names of the
        // others is not removed for them.
        let (job, x) = run_mounting("move", "move", "c.txt", ACROSS);
        assert_eq!(job.errors, [at_x(&x, "c.txt", ENTRY_CHANGED), at_x(&x, "a.bin", SOURCE_CHANGED), at_x(&x, "b.txt", SOURCE_CHANGED)]);
        assert_eq!(names(&dst.join("move").join("x")), ["a.bin", "b.txt"]);
        assert_eq!(fs::read_to_string(dst.join("move").join("x").join("a.bin")).unwrap(), "mine a.bin");
        outside_intact();
        fs::remove_dir(&x).unwrap();
    }

    #[test]
    fn copies_and_moves_never_follow_a_folder_swapped_for_a_junction() {
        let t = Temp::new("swapped");
        let (src, dst, outside) = (t.dir("src"), t.dir("dst"), t.dir("outside"));
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        let stamped = |p: &Path, text: &str| {
            fs::write(p, text).unwrap();
            fs::File::options().write(true).open(p).unwrap().set_modified(when).unwrap();
        };
        stamped(&outside.join("f.txt"), "same");
        let big = content(3 << 20, 18);
        // Runs `swap` once, when the job is in the middle of the file `at`.
        let run_swapping = |op: &str, source: &Path, settings: Settings, at: PathBuf, swap: Box<dyn Fn() + Send + Sync>| {
            let done = Arc::new(AtomicBool::new(false));
            let hook = move |job: &JobInfo| {
                if job.current.as_deref() == Some(&*at.to_string_lossy()) && !done.swap(true, Ordering::SeqCst) {
                    swap();
                }
            };
            let sink = Sink { on_progress: Some(Arc::new(hook)), ..Sink::default() };
            wait(start(op, vec![source.to_string_lossy().into_owned()], dst.to_string_lossy().into_owned(), sink, settings).unwrap())
        };

        // Swapped after its folder was listed, before it is entered: not entered.
        let c = src.join("c");
        fs::create_dir_all(c.join("zz")).unwrap();
        fs::write(c.join("aa.bin"), &big).unwrap();
        fs::write(c.join("zz").join("mine.txt"), "mine").unwrap();
        let (zz, out) = (c.join("zz"), outside.clone());
        let swap = Box::new(move || {
            fs::rename(&zz, zz.with_file_name("zz-real")).unwrap();
            create_junction(&zz, &out).unwrap();
        });
        let job = run_swapping("copy", &c, TEST, c.join("aa.bin"), swap);
        assert!(job.errors.contains(&(c.join("zz").to_string_lossy().into_owned(), TURNED_LINK.to_string())), "{:?}", job.errors);
        // (Told apart as it is opened in its folder, before anything is made for it.)
        let left = dst.join("c").join("zz");
        assert!(!left.exists() || names(&left).is_empty(), "nothing of where it leads was copied");
        assert_eq!(fs::read(dst.join("c").join("aa.bin")).unwrap(), big);
        assert_eq!(names(&outside), ["f.txt"]);
        fs::remove_dir(c.join("zz")).unwrap();

        // Swapped after it was copied, before the move lets go of it: what it leads to now is
        // not removed, though it has the name, size and time of what was copied.
        let m = src.join("m");
        fs::create_dir_all(m.join("sub")).unwrap();
        stamped(&m.join("sub").join("f.txt"), "same");
        fs::write(m.join("zz.bin"), &big).unwrap();
        let (sub, out) = (m.join("sub"), outside.clone());
        let swap = Box::new(move || {
            fs::rename(&sub, sub.with_file_name("sub-real")).unwrap();
            create_junction(&sub, &out).unwrap();
        });
        let job = run_swapping("move", &m, Settings { rename: false, ..TEST }, m.join("zz.bin"), swap);
        assert_eq!(job.errors, [(m.join("sub").join("f.txt").to_string_lossy().into_owned(), SOURCE_CHANGED.to_string())]);
        assert_eq!(fs::read_to_string(dst.join("m").join("sub").join("f.txt")).unwrap(), "same");
        assert_eq!(fs::read_to_string(outside.join("f.txt")).unwrap(), "same");
        assert_eq!(fs::read_to_string(m.join("sub-real").join("f.txt")).unwrap(), "same");
        // What was not swapped went as a move does.
        assert!(gone(&m.join("zz.bin")));
        assert_eq!(junction_target(&m.join("sub")), Some(outside.clone()));
        fs::remove_dir(m.join("sub")).unwrap();
    }

    #[test]
    fn removal_takes_what_became_a_link_as_the_link_and_held_folders_stay_put() {
        let t = Temp::new("remove-swapped");
        let (tree, outside) = (t.dir("tree"), t.dir("outside"));
        fs::write(outside.join("keep.txt"), "keep").unwrap();
        // Each entry is opened as itself: one that is a junction now (whatever its folder's
        // listing said) is a link, and goes as one.
        create_junction(&tree.join("was a folder"), &outside).unwrap();
        let entry = open_to_delete(&tree.join("was a folder"), true).unwrap();
        let (kind, attrs) = kind_open(&entry).unwrap();
        assert_eq!(kind, Kind::Link { dir: true });
        delete_open(&entry, attrs).unwrap();
        drop(entry);
        assert!(gone(&tree.join("was a folder")));
        assert_eq!(names(&outside), ["keep.txt"]);
        // A folder held while what is in it goes: neither it nor the folder above it can be
        // renamed (and so swapped for a junction) meanwhile.
        fs::create_dir_all(tree.join("a").join("b")).unwrap();
        let held = open_itself(&tree.join("a").join("b"), DELETE, true).unwrap();
        assert_eq!(kind_open(&held).unwrap().0, Kind::Dir);
        assert!(fs::rename(tree.join("a").join("b"), tree.join("a").join("c")).is_err());
        assert!(fs::rename(tree.join("a"), tree.join("z")).is_err());
        drop(held);
        // The whole tree: junctions in it, read-only files and folders, nothing followed.
        create_junction(&tree.join("a").join("b").join("link"), &outside).unwrap();
        fs::write(tree.join("a").join("ro.txt"), "ro").unwrap();
        set_attrs(&tree.join("a").join("ro.txt"), FILE_ATTRIBUTE_READONLY);
        set_attrs(&tree.join("a").join("b"), FILE_ATTRIBUTE_READONLY);
        remove_tree(&tree).unwrap();
        assert!(gone(&tree));
        assert_eq!(names(&outside), ["keep.txt"]);
    }

    #[test]
    fn marked_names_for_relocation() {
        let name = |target: &str, hide, ext, n| marked_name(Path::new(target), hide, ext, n).unwrap().to_string_lossy().into_owned();
        assert_eq!(name(r"C:\d\Steam", false, OLD_EXT, 1), r"C:\d\Steam.ff-old");
        assert_eq!(name(r"C:\d\Steam", false, OLD_EXT, 2), r"C:\d\Steam (2).ff-old");
        assert_eq!(name(r"D:\Steam", true, RELOCATING_EXT, 1), r"D:\.Steam.ff-relocating");
        let long = "x".repeat(MAX_NAME);
        let cut = marked_name(&Path::new(r"C:\d").join(&long), false, OLD_EXT, 3).unwrap();
        assert_eq!(cut.file_name().unwrap().to_string_lossy().encode_utf16().count(), MAX_NAME);
    }

    // ---- v0.8.2: duplicates made into hard links ------------------------------

    /// Bytes that differ from file to file of one `seed`.
    fn content(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3] ^ seed).collect()
    }

    /// What the duplicate finder makes of `paths` as they are now.
    fn search(paths: &[&Path]) -> Vec<dups::DupGroup> {
        let cands = paths
            .iter()
            .map(|p| {
                let m = fs::metadata(p).unwrap();
                dups::Candidate { path: p.to_string_lossy().into_owned(), size: m.len(), modified: m.last_write_time() as i64 }
            })
            .collect();
        dups::find_now(cands).unwrap().groups
    }

    fn link_group(keep: &Path, others: &[&Path]) -> LinkGroup {
        LinkGroup { keep: keep.to_string_lossy().into_owned(), others: others.iter().map(|p| p.to_string_lossy().into_owned()).collect() }
    }

    /// The pages of merges in the test folder that holds `keep`'s folder.
    fn book_for(keep: &Path) -> LinkBook {
        LinkBook(keep.parent().unwrap().parent().unwrap().join("merges"))
    }

    /// Merges `others` into `keep` as the finder `found` them and waits for
    /// the job, which leaves no page behind.
    fn merge(found: &[dups::DupGroup], keep: &Path, others: &[&Path], settings: Settings, cancel_mid_file: bool) -> JobInfo {
        merge_asking(found, keep, others, settings, cancel_mid_file, false)
    }

    /// `merge`, with big models and archives in programs' folders too when `allow_programs`.
    fn merge_asking(found: &[dups::DupGroup], keep: &Path, others: &[&Path], settings: Settings, cancel_mid_file: bool, allow_programs: bool) -> JobInfo {
        let sink = Sink { cancel_mid_file, ..Sink::default() };
        let book = book_for(keep);
        let job = wait(begin_link(vec![link_group(keep, others)], found, sink, settings, &book, allow_programs).unwrap());
        assert!(book.pages().is_empty(), "{:?}", book.pages());
        job
    }

    fn key(p: &Path) -> dups::FileKey {
        key_at(p).unwrap()
    }

    fn names_of(p: &Path) -> u32 {
        dups::facts(&File::open(p).unwrap()).unwrap().links
    }

    fn attrs(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().file_attributes()
    }

    /// `a\name` and `b\name`, both `bytes`, and what the finder makes of them.
    fn twins(t: &Temp, name: &str, bytes: &[u8]) -> (PathBuf, PathBuf, Vec<dups::DupGroup>) {
        let (keep, copy) = (t.dir("a").join(name), t.dir("b").join(name));
        fs::write(&keep, bytes).unwrap();
        fs::write(&copy, bytes).unwrap();
        let found = search(&[&keep, &copy]);
        assert_eq!(found.len(), 1);
        (keep, copy, found)
    }

    /// Both files as they were: their own contents, not one file, nothing next to them.
    fn untouched(keep: &Path, copy: &Path, keep_bytes: &[u8], copy_bytes: &[u8]) {
        assert_eq!(fs::read(keep).unwrap(), keep_bytes);
        assert_eq!(fs::read(copy).unwrap(), copy_bytes);
        assert_ne!(key(keep), key(copy));
        assert_eq!((names_of(keep), names_of(copy)), (1, 1));
        let name = copy.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(names(copy.parent().unwrap()), [name]);
    }

    #[test]
    fn duplicates_become_one_file_with_two_names() {
        let t = Temp::new("link");
        let data = content(3 << 20, 1);
        let (keep, copy) = (t.dir("a").join("model.gguf"), t.dir("b").join("model.gguf"));
        let third = t.dir("c").join("model.gguf");
        for p in [&keep, &copy, &third] {
            fs::write(p, &data).unwrap();
        }
        // Read-only files take links too (a copy only when it is read-only as well: its name keeps the mark).
        for p in [&keep, &copy, &third] {
            set_attrs(p, FILE_ATTRIBUTE_READONLY);
        }
        let found = search(&[&keep, &copy, &third]);
        assert_eq!(found.len(), 1);
        assert_ne!(key(&keep), key(&copy));

        let job = merge(&found, &keep, &[&copy], TEST, false);
        assert_eq!((job.op.as_str(), job.state.as_str()), ("link", "done"), "{:?}", job.errors);
        assert_eq!((job.done_items, job.total_items), (1, 1));
        assert_eq!((job.done_bytes, job.total_bytes), (6 << 20, 6 << 20));
        assert_eq!(Path::new(&job.dest), t.0.as_path());
        // One file under two names, each opening all of it; the name merged
        // has the attributes of the file that stays.
        assert_eq!(key(&keep), key(&copy));
        assert!(win::same_entry(&keep, &copy));
        assert_eq!(names_of(&keep), 2);
        assert_eq!(fs::read(&keep).unwrap(), data);
        assert_eq!(fs::read(&copy).unwrap(), data);
        assert!(fs::metadata(&copy).unwrap().permissions().readonly());
        assert_eq!(names(&t.0.join("a")), ["model.gguf"]);
        // The copy's own bytes went to the bin, with its own attributes (not hidden).
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert_eq!(names(&t.0.join("b")), ["model.gguf"]);
        let binned = t.0.join("bin").join("b .model.gguf.ff-link");
        assert_eq!(fs::read(&binned).unwrap(), data);
        assert_eq!(attrs(&binned) & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY), FILE_ATTRIBUTE_READONLY);
        assert_eq!(job.freed_bytes, 3 << 20);
        // Written down, as the window shows it.
        let ledger = book_for(&keep).ledger();
        let shown_at = |p: &Path| p.to_string_lossy().into_owned();
        let merged = links::Merge { kept: shown_at(&keep), linked: shown_at(&copy), size: 3 << 20, at: 0 };
        let read = ledger.read();
        assert_eq!(read.len(), 1);
        assert_eq!((&read[0].kept, &read[0].linked, read[0].size), (&merged.kept, &merged.linked, merged.size));
        assert!(links::still_linked(&read[0]));

        // Again from the same result: the merged copy is one file with it
        // already, and the third one is merged now.
        let again = merge(&found, &keep, &[&copy, &third], TEST, false);
        assert_eq!(again.state, "done", "{:?}", again.errors);
        assert!(again.errors.is_empty(), "{:?}", again.errors);
        assert_eq!((again.done_items, again.total_items), (2, 2));
        assert_eq!(key(&third), key(&keep));
        assert_eq!(names_of(&keep), 3);
        assert_eq!(again.freed_bytes, 3 << 20);
        assert_eq!(names(&t.0.join("bin")), ["b .model.gguf.ff-link", "c .model.gguf.ff-link"]);
        // The one merged now goes in too; the one that was one file already does not again.
        let linked: Vec<String> = ledger.read().into_iter().map(|m| m.linked).collect();
        assert_eq!(linked, [shown_at(&copy), shown_at(&third)]);
    }

    #[test]
    fn a_copy_the_bin_turns_down_stays_hidden_and_is_reported() {
        let t = Temp::new("link-left");
        let data = content(300_000, 8);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        let full = Settings { bin: |_| Err("휴지통이 받지 않았어요".into()), ..TEST };
        let job = merge(&found, &keep, &[&copy], full, false);
        // Merged all the same: one file under both names.
        assert_eq!(job.state, "done");
        assert_eq!(key(&keep), key(&copy));
        assert_eq!(fs::read(&copy).unwrap(), data);
        let left = t.0.join("b").join(".x.bin.ff-link");
        assert_eq!(job.errors, [(left.to_string_lossy().into_owned(), format!("{LINK_LEFT}(.x.bin.ff-link)을 휴지통에 넣지 못했어요: 휴지통이 받지 않았어요"))]);
        assert_eq!(fs::read(&left).unwrap(), data);
        assert_ne!(key(&left), key(&keep));
        assert!(attrs(&left) & FILE_ATTRIBUTE_HIDDEN != 0);
        assert_eq!(job.freed_bytes, 0);
    }

    /// Puts a small file in the Recycle Bin, so it runs only when asked:
    /// `cargo test a_merged_copy_goes -- --ignored`
    #[test]
    #[ignore]
    fn a_merged_copy_goes_to_the_recycle_bin() {
        let t = Temp::new("link-bin");
        let data = content(50_000, 9);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        let job = merge(&found, &keep, &[&copy], Settings { bin: to_bin, ..TEST }, false);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert_eq!(key(&keep), key(&copy));
        assert_eq!(names(&t.0.join("b")), ["x.bin"]);
        assert_eq!(job.freed_bytes, 50_000);
    }

    #[test]
    fn copies_that_differ_are_turned_down_before_anything_is_touched() {
        let t = Temp::new("link-differ");
        let data = content(1 << 20, 2);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        // Another content since the search, with the same size and modified time.
        let when = fs::metadata(&copy).unwrap().modified().unwrap();
        let mut other = data.clone();
        other[700_000] ^= 1;
        fs::write(&copy, &other).unwrap();
        File::options().write(true).open(&copy).unwrap().set_modified(when).unwrap();
        let job = merge(&found, &keep, &[&copy], TEST, false);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), LINK_DIFFERENT.to_string())]);
        assert_eq!(job.freed_bytes, 0);
        untouched(&keep, &copy, &data, &other);

        // A modified time that changed is turned down without reading.
        File::options().write(true).open(&copy).unwrap().set_modified(when + Duration::from_secs(5)).unwrap();
        let job = merge(&found, &keep, &[&copy], TEST, false);
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), LINK_CHANGED.to_string())]);
        assert_eq!(job.done_bytes, job.total_bytes);
        untouched(&keep, &copy, &data, &other);

        // When the file that stays changed, none of its group is merged.
        fs::write(&copy, &data).unwrap();
        let found = search(&[&keep, &copy]);
        let when = fs::metadata(&keep).unwrap().modified().unwrap();
        fs::write(&keep, &other).unwrap();
        File::options().write(true).open(&keep).unwrap().set_modified(when).unwrap();
        let job = merge(&found, &keep, &[&copy], TEST, false);
        assert_eq!(job.state, "failed");
        let (at, why) = &job.errors[0];
        assert_eq!((Path::new(at), why.as_str()), (copy.as_path(), format!("{KEEP_FAILED}(x.bin) — {LINK_CHANGED}").as_str()));
        untouched(&keep, &copy, &other, &data);
    }

    #[test]
    fn files_in_use_are_turned_down_before_they_are_read() {
        let t = Temp::new("link-busy");
        let data = content(2 << 20, 10);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        let busy = format!("{LINK_NO_OPEN}: 다른 프로그램이 쓰고 있어요");
        // A copy a program holds open without letting it be renamed.
        let held = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&copy).unwrap();
        let job = merge(&found, &keep, &[&copy], TEST, false);
        drop(held);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), busy.clone())]);
        // Only the file that stays was read.
        assert_eq!(job.done_bytes, job.total_bytes);
        untouched(&keep, &copy, &data, &data);
        // A file that stays which a program is writing to: its whole group waits.
        let writing = fs::OpenOptions::new().write(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(&keep).unwrap();
        let job = merge(&found, &keep, &[&copy], TEST, false);
        drop(writing);
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), format!("{KEEP_FAILED}(x.bin) — {busy}"))]);
        untouched(&keep, &copy, &data, &data);
        // Let go of, both merge.
        let job = merge(&found, &keep, &[&copy], TEST, false);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(key(&keep), key(&copy));
    }

    #[test]
    fn copies_on_another_drive_are_turned_down() {
        let t = Temp::new("link-drive");
        let data = content(100_000, 3);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        // Tests have no second drive: ACROSS takes any two files for two drives.
        let job = merge(&found, &keep, &[&copy], ACROSS, false);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), LINK_OTHER_DRIVE.to_string())]);
        untouched(&keep, &copy, &data, &data);
    }

    #[test]
    fn a_cancelled_merge_leaves_both_files_as_they_were() {
        let t = Temp::new("link-cancel");
        let data = content(3 << 20, 4);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        let job = merge(&found, &keep, &[&copy], TEST, true);
        assert_eq!(job.state, "cancelled");
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert!(job.done_bytes < job.total_bytes);
        assert_eq!(job.freed_bytes, 0);
        untouched(&keep, &copy, &data, &data);
    }

    #[test]
    fn a_link_that_cannot_be_made_puts_the_copy_back() {
        let t = Temp::new("link-undo");
        let data = content(200_000, 5);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        // Read-only both, as a merge takes them (`links::pair_refusal`).
        set_attrs(&copy, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE);
        set_attrs(&keep, FILE_ATTRIBUTE_READONLY);
        let refused = Settings { hard_link: |_, _| Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)), ..TEST };
        let job = merge(&found, &keep, &[&copy], refused, false);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), format!("{LINK_NOT_MADE}: 권한이 없어요"))]);
        untouched(&keep, &copy, &data, &data);
        // Its name and its own attributes back, not hidden.
        assert_eq!(attrs(&copy) & SETTABLE, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE);

        // A link made that then does not hold is taken away again.
        let made = Settings {
            hard_link: |link, existing| {
                create_hard_link(link, existing)?;
                Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32))
            },
            ..TEST
        };
        set_attrs(&keep, FILE_ATTRIBUTE_READONLY);
        let job = merge(&found, &keep, &[&copy], made, false);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), format!("{LINK_NOT_MADE}: 권한이 없어요"))]);
        untouched(&keep, &copy, &data, &data);
        assert_eq!(attrs(&copy) & SETTABLE, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE);
        assert_eq!(attrs(&keep) & SETTABLE, FILE_ATTRIBUTE_READONLY);
    }

    /// `link`'s neighbour folder `folder` in the test folder (`b\x` gives `folder\x`).
    fn beside(link: &Path, folder: &str) -> PathBuf {
        link.parent().unwrap().parent().unwrap().join(folder).join(link.file_name().unwrap())
    }

    #[test]
    fn the_link_leads_to_the_file_that_was_read_even_if_another_took_its_path() {
        let t = Temp::new("link-swapped");
        let data = content(400_000, 11);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        let read = key(&keep);
        // Once both were read, a program renames the file that stays away and
        // puts a new one at its path (an update of its own, say).
        let swapped = Settings {
            hard_link: |link, existing| {
                let keep = beside(link, "a");
                fs::rename(&keep, keep.with_extension("old"))?;
                fs::write(&keep, b"something else")?;
                create_hard_link(link, existing)
            },
            ..TEST
        };
        let job = merge(&found, &keep, &[&copy], swapped, false);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        // The copy's name leads to the file that was compared, not to the new one.
        let moved = keep.with_extension("old");
        assert_eq!((key(&copy), key(&moved)), (read, read));
        assert_eq!(fs::read(&copy).unwrap(), data);
        assert_eq!(fs::read(&keep).unwrap(), b"something else");
        assert_eq!(names_of(&keep), 1);
    }

    #[test]
    fn a_link_that_leads_elsewhere_is_taken_away_again() {
        let t = Temp::new("link-astray");
        let data = content(150_000, 12);
        let (keep, copy, found) = twins(&t, "x.bin", &data);
        let stranger = t.dir("z").join("x.bin");
        fs::write(&stranger, b"another file").unwrap();
        // A link call that went to whatever was at a path.
        let astray = Settings { hard_link: |link, _| fs::hard_link(beside(link, "z"), link), ..TEST };
        let job = merge(&found, &keep, &[&copy], astray, false);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(copy.to_string_lossy().into_owned(), format!("{LINK_NOT_MADE}: {LINK_ELSEWHERE}"))]);
        // The name it made is gone again, the copy has its own back, the other file is as it was.
        untouched(&keep, &copy, &data, &data);
        assert_eq!(names_of(&stranger), 1);
        assert_eq!(fs::read(&stranger).unwrap(), b"another file");
    }

    #[test]
    fn a_files_last_name_is_never_taken_away() {
        let t = Temp::new("link-last");
        let only = t.0.join("only.bin");
        fs::write(&only, b"x").unwrap();
        assert!(!unlink_extra(&only));
        assert_eq!(fs::read(&only).unwrap(), b"x");
        assert!(!unlink_extra(&t.0));
        let second = t.0.join("second.bin");
        fs::hard_link(&only, &second).unwrap();
        set_attrs(&only, FILE_ATTRIBUTE_READONLY);
        assert!(unlink_extra(&second));
        assert_eq!(names(&t.0), ["only.bin"]);
        assert_eq!(attrs(&only) & SETTABLE, FILE_ATTRIBUTE_READONLY);
        set_attrs(&only, FILE_ATTRIBUTE_NORMAL);
    }

    /// A merge of `copy` into `keep` cut short once the copy had its hidden
    /// name and, when `linked`, its own name had become a link: the page it
    /// leaves in `book`, and what is on it.
    fn cut_short_merge(book: &LinkBook, keep: &Path, copy: &Path, linked: bool) -> Switch {
        let m = fs::metadata(copy).unwrap();
        let s = Switch {
            path: copy.into(),
            temp: marked_name(copy, true, LINK_EXT, 1).unwrap(),
            keep: keep.into(),
            kept: key(keep),
            copy: key(copy),
            attrs: attrs(copy),
            size: m.len(),
            modified: m.last_write_time() as i64,
        };
        let mut page = book.page().unwrap();
        page.write(&s).unwrap();
        fs::rename(copy, &s.temp).unwrap();
        set_attrs(&s.temp, s.attrs | FILE_ATTRIBUTE_HIDDEN);
        if linked {
            fs::hard_link(keep, copy).unwrap();
        }
        // Let go of with the switch on it, as by a process that ended.
        drop(page);
        assert_eq!(book.pages().len(), 1);
        s
    }

    /// What `recover_links` does at a start, with the test bin `bin`: its job, if one.
    fn next_start(book: &LinkBook, bin: fn(&Path) -> Result<(), String>) -> Option<JobInfo> {
        mend_links(book, bin, Sink::default()).map(wait)
    }

    #[test]
    fn merges_cut_short_are_put_right_at_the_next_start() {
        let t = Temp::new("link-recover");
        let book = LinkBook(t.0.join("merges"));
        let data = content(250_000, 13);
        let (keep, copy, _) = twins(&t, "x.bin", &data);
        let shown_at = |p: &Path| p.to_string_lossy().into_owned();
        set_attrs(&copy, FILE_ATTRIBUTE_ARCHIVE);

        // Cut between the rename and the link: the copy gets its name back.
        let s = cut_short_merge(&book, &keep, &copy, false);
        assert!(gone(&copy));
        let job = next_start(&book, test_bin).unwrap();
        assert_eq!((job.op.as_str(), job.state.as_str()), ("link", "failed"));
        assert_eq!(job.errors, [(shown_at(&copy), LINK_CUT_BACK.to_string())]);
        assert!(book.pages().is_empty());
        untouched(&keep, &copy, &data, &data);
        assert_eq!(attrs(&copy) & SETTABLE, FILE_ATTRIBUTE_ARCHIVE);
        assert!(gone(&s.temp));
        // Nothing left: the next start does nothing.
        assert!(next_start(&book, test_bin).is_none());

        // Cut once the link was made: the copy's old bytes go to the bin.
        let s = cut_short_merge(&book, &keep, &copy, true);
        let job = next_start(&book, test_bin).unwrap();
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert!(job.errors.is_empty());
        assert_eq!(job.freed_bytes, 250_000);
        assert_eq!(key(&copy), key(&keep));
        assert!(gone(&s.temp));
        let binned = t.0.join("bin").join("b .x.bin.ff-link");
        assert_eq!(fs::read(&binned).unwrap(), data);
        assert_eq!(attrs(&binned) & FILE_ATTRIBUTE_HIDDEN, 0);
        assert!(book.pages().is_empty());
    }

    #[test]
    fn what_the_next_start_cannot_put_right_is_told() {
        let t = Temp::new("link-recover-left");
        let book = LinkBook(t.0.join("merges"));
        let data = content(120_000, 14);
        let (keep, copy, _) = twins(&t, "x.bin", &data);
        let shown_at = |p: &Path| p.to_string_lossy().into_owned();

        // The bin turns the old bytes down: they stay, hidden, and the user is told.
        let s = cut_short_merge(&book, &keep, &copy, true);
        let job = next_start(&book, |_| Err("휴지통이 받지 않았어요".into())).unwrap();
        assert_eq!(job.state, "done");
        let left = format!("{LINK_LEFT}(.x.bin.ff-link)을 휴지통에 넣지 못했어요: 휴지통이 받지 않았어요");
        assert_eq!(job.errors, [(shown_at(&s.temp), left)]);
        assert_eq!(job.freed_bytes, 0);
        assert!(attrs(&s.temp) & FILE_ATTRIBUTE_HIDDEN != 0);
        assert_eq!(key(&copy), key(&keep));
        assert!(book.pages().is_empty());
        set_attrs(&s.temp, FILE_ATTRIBUTE_NORMAL);
        fs::remove_file(&s.temp).unwrap();
        fs::remove_file(&copy).unwrap();
        fs::write(&copy, &data).unwrap();

        // A copy that cannot get its name back yet (a program holds it without
        // letting it be renamed): the page stays, and the next start does it.
        let s = cut_short_merge(&book, &keep, &copy, false);
        let held = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&s.temp).unwrap();
        let job = next_start(&book, test_bin).unwrap();
        drop(held);
        assert_eq!(job.state, "failed");
        let stuck = format!("{LINK_CUT_STUCK}: 다른 프로그램이 쓰고 있어요. 파일은 지금 {}에 있어요", shown_at(&s.temp));
        assert_eq!(job.errors, [(shown_at(&copy), stuck)]);
        assert_eq!(book.pages().len(), 1);
        let job = next_start(&book, test_bin).unwrap();
        assert_eq!(job.errors, [(shown_at(&copy), LINK_CUT_BACK.to_string())]);
        untouched(&keep, &copy, &data, &data);
        assert!(book.pages().is_empty());

        // Its name taken meanwhile by another file: both stay as they are.
        let s = cut_short_merge(&book, &keep, &copy, false);
        fs::write(&copy, b"new").unwrap();
        let job = next_start(&book, test_bin).unwrap();
        let taken = format!("{LINK_CUT_TAKEN}. 원래 파일은 지금 {}에 있어요", shown_at(&s.temp));
        assert_eq!(job.errors, [(shown_at(&copy), taken)]);
        assert_eq!(fs::read(&copy).unwrap(), b"new");
        assert_eq!(fs::read(&s.temp).unwrap(), data);
        assert!(book.pages().is_empty());
    }

    #[test]
    fn pages_of_merges_still_running_are_left_alone() {
        let t = Temp::new("link-recover-busy");
        let book = LinkBook(t.0.join("merges"));
        let data = content(60_000, 15);
        let (keep, copy, _) = twins(&t, "x.bin", &data);
        // A merge of this process, in the middle of its switch.
        let mut page = book.page().unwrap();
        let s = Switch {
            path: copy.clone(),
            temp: marked_name(&copy, true, LINK_EXT, 1).unwrap(),
            keep: keep.clone(),
            kept: key(&keep),
            copy: key(&copy),
            attrs: attrs(&copy),
            size: 60_000,
            modified: fs::metadata(&copy).unwrap().last_write_time() as i64,
        };
        page.write(&s).unwrap();
        fs::rename(&copy, &s.temp).unwrap();
        assert!(next_start(&book, test_bin).is_none());
        assert!(gone(&copy));
        assert!(!linking_in(&book), "this process's own merge is no other's");
        // Another process's.
        let other = hold_exclusive(&book.0.join("1-0-1.json")).unwrap();
        assert!(linking_in(&book));
        drop(other);
        assert!(!linking_in(&book));
        // Pages with nothing on them, or only half written, just go.
        fs::write(book.0.join("1-0-2.json"), b"{\"path\":").unwrap();
        fs::rename(&s.temp, &copy).unwrap();
        page.clear();
        drop(page);
        assert!(next_start(&book, test_bin).is_none());
        assert!(book.pages().is_empty());
        untouched(&keep, &copy, &data, &data);
    }

    #[test]
    fn merges_are_checked_against_the_search_before_they_start() {
        let t = Temp::new("link-plan");
        let (x, y) = (content(10_000, 6), content(10_000, 7));
        let (x1, x2, y1, y2) = (t.0.join("x1"), t.0.join("x2"), t.0.join("y1"), t.0.join("y2"));
        for (p, bytes) in [(&x1, &x), (&x2, &x), (&y1, &y), (&y2, &y)] {
            fs::write(p, bytes).unwrap();
        }
        let found = search(&[&x1, &x2, &y1, &y2]);
        assert_eq!(found.len(), 2);
        let book = LinkBook(t.0.join("merges"));
        let refused = |groups: Vec<LinkGroup>| match begin_link(groups, &found, Sink::default(), TEST, &book, false) {
            Ok(id) => panic!("job {id} started: {:?}", wait(id).errors),
            Err(why) => why,
        };
        let shown = |p: &Path| p.to_string_lossy().into_owned();
        assert_eq!(refused(vec![link_group(&x1, &[])]), NOTHING_TO_LINK);
        let many: Vec<String> = (0..=MAX_LINKS).map(|i| format!(r"C:\x\{i}")).collect();
        assert!(refused(vec![LinkGroup { keep: shown(&x1), others: many }]).starts_with(TOO_MANY_LINKS));
        let stranger = t.0.join("stranger");
        assert_eq!(refused(vec![link_group(&x1, &[&stranger])]), format!("{NOT_IN_RESULT}: {}", shown(&stranger)));
        assert_eq!(refused(vec![link_group(&x1, &[&x1])]), format!("{LINK_TWICE}: {}", shown(&x1)));
        // The same file in two groups, also written another way.
        let upper = PathBuf::from(shown(&x2).to_uppercase());
        assert_eq!(refused(vec![link_group(&x1, &[&x2]), link_group(&upper, &[&x1])]), format!("{LINK_TWICE}: {}", shown(&upper)));
        assert_eq!(refused(vec![link_group(&x1, &[&y1])]), format!("{NOT_ONE_GROUP}: {}", shown(&y1)));
        // Paths are taken as the search wrote them.
        let plans = plan_links(vec![link_group(&upper, &[&x1]), link_group(&y2, &[&y1])], &found).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].keep.0, x2);
        assert_eq!(plans[0].others.iter().map(|(p, _)| p.as_path()).collect::<Vec<_>>(), [x1.as_path()]);
        assert_eq!((plans[1].size, plans[1].hash), (10_000, *blake3::hash(&y).as_bytes()));
        assert_eq!(plans[1].keep.1, fs::metadata(&y2).unwrap().last_write_time() as i64);
        assert_eq!(common_folder([&x1, &y2, &t.0.join("sub").join("z")]), t.0);
        assert_eq!(common_folder([&PathBuf::from(r"C:\a\b\c"), &PathBuf::from(r"D:\a\b")]), PathBuf::from(r"C:\"));
    }

    #[test]
    fn files_of_windows_itself_are_never_merged() {
        assert!(in_windows(Path::new(r"C:\Windows\System32\drivers\etc\hosts"), None));
        assert!(in_windows(Path::new(r"D:\Recovery\x.wim"), None));
        let windows = known_folder(&FOLDERID_Windows);
        if let Some(w) = &windows {
            assert!(in_windows(&w.join("notepad.exe"), Some(w)));
            assert!(matches!(open_for_link(&w.join("notepad.exe"), 1, 0, Some(w), false), Err(why) if why == LINK_SYSTEM));
        }
        let t = Temp::new("link-system");
        assert!(!in_windows(&t.0.join("x.bin"), windows.as_deref()));
    }

    // ---- v0.10.3: the link rule in the merge, names made files of their own --------

    /// The app's link rule (`links::verdict`), with the test folder taken for one in `seen_in`.
    fn seen_in(seen_in: &str, path: &Path, size: u64, markers: &links::Markers) -> links::Verdict {
        let rest = path.strip_prefix(std::env::temp_dir()).unwrap();
        let seen = Path::new(seen_in).join(rest);
        links::verdict_as(&seen.to_string_lossy(), path, size, markers)
    }

    /// The rule as for a folder in the user's Downloads...
    const IN_DOWNLOADS: Settings = Settings { rule: |p, size, m| seen_in(r"C:\Users\me\Downloads", p, size, m), ..TEST };
    /// ...and in a program's app data.
    const IN_APP_DATA: Settings = Settings { rule: |p, size, m| seen_in(r"C:\Users\me\AppData\Local", p, size, m), ..TEST };

    /// `keep` and `copy` (in folders of their own in `t`), both `bytes`, and what the finder makes of them.
    fn pair_in(t: &Temp, keep: &str, copy: &str, bytes: &[u8]) -> (PathBuf, PathBuf, Vec<dups::DupGroup>) {
        let (keep, copy) = (t.0.join(keep), t.0.join(copy));
        for p in [&keep, &copy] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
        let found = search(&[&keep, &copy]);
        assert_eq!(found.len(), 1);
        (keep, copy, found)
    }

    #[test]
    fn the_merge_keeps_to_the_link_rule() {
        let t = Temp::new("link-rule");
        let data = content(70_000, 21);
        let shown_at = |p: &Path| p.to_string_lossy().into_owned();
        let refused = |keep: &Path, copy: &Path, found: &[dups::DupGroup], settings: Settings, why: String| {
            let job = merge(found, keep, &[copy], settings, false);
            assert_eq!(job.state, "failed");
            assert_eq!(job.errors, [(shown_at(copy), why)]);
            untouched(keep, copy, &data, &data);
        };

        // The user's own pictures merge.
        let (keep, copy, found) = pair_in(&t, r"a\photo.jpg", r"b\photo.jpg", &data);
        let job = merge(&found, &keep, &[&copy], IN_DOWNLOADS, false);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(key(&keep), key(&copy));

        // What programs rewrite in place: never, the file that stays neither.
        let (keep, copy, found) = pair_in(&t, r"c\settings.json", r"d\settings.json", &data);
        refused(&keep, &copy, &found, IN_DOWNLOADS, format!("{KEEP_FAILED}(settings.json) — {}", links::LB_REWRITTEN));
        // A mod pack's, by its folders' names or by what they hold.
        let (keep, copy, found) = pair_in(&t, r"e\pack.zip", r"f\mods\pack.zip", &data);
        refused(&keep, &copy, &found, IN_DOWNLOADS, links::LB_GAME.into());
        refused(&copy, &keep, &found, IN_DOWNLOADS, format!("{KEEP_FAILED}(pack.zip) — {}", links::LB_GAME));
        let (keep, copy, found) = pair_in(&t, r"g\icon.png", r"Big Adventure\icon.png", &data);
        fs::write(t.0.join("Big Adventure").join("eula.txt"), b"eula=true").unwrap();
        // (Not `untouched`: the copy's folder holds more than the copy now.)
        let job = merge(&found, &keep, &[&copy], IN_DOWNLOADS, false);
        assert_eq!(job.errors, [(shown_at(&copy), links::LB_GAME.to_string())]);
        assert_ne!(key(&keep), key(&copy));

        // A big model in a program's folder: only when the user asks.
        let (keep, copy, found) = pair_in(&t, r"h\model.gguf", r"i\model.gguf", &data);
        refused(&keep, &copy, &found, IN_APP_DATA, format!("{KEEP_FAILED}(model.gguf) — {}", links::LB_PROGRAM_OPTIN));
        let job = merge_asking(&found, &keep, &[&copy], IN_APP_DATA, false, true);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_eq!(key(&keep), key(&copy));
        // Anything else there, not even then.
        let (keep, copy, found) = pair_in(&t, r"j\tool.dll", r"k\tool.dll", &data);
        let job = merge_asking(&found, &keep, &[&copy], IN_APP_DATA, false, true);
        assert_eq!(job.errors, [(shown_at(&copy), format!("{KEEP_FAILED}(tool.dll) — {}", links::LB_PROGRAM))]);
        untouched(&keep, &copy, &data, &data);
        // Nor two models whose folders pass down different permissions, as a Store app's package
        // folder does (an ACE for the app's own SID): one name would lose them. Asking does not help.
        let (keep, copy, found) = pair_in(&t, r"h2\model.gguf", r"i2\model.gguf", &data);
        set_dacl(&t.0.join("i2"), "D:(A;OICI;FA;;;S-1-15-3-1024-1065365936-1281604716-3511738428-1654721687-432734479-3232135806-4053264122-3456934681)");
        let job = merge_asking(&found, &keep, &[&copy], IN_APP_DATA, false, true);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(shown_at(&copy), links::LB_SECURITY_APPS.to_string())]);
        untouched(&keep, &copy, &data, &data);

        // Two files that would not keep their own attributes or permissions as one.
        let (keep, copy, found) = pair_in(&t, r"l\hidden.jpg", r"m\hidden.jpg", &data);
        set_attrs(&copy, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_ARCHIVE);
        refused(&keep, &copy, &found, IN_DOWNLOADS, links::LB_ATTRS.into());
        let (keep, copy, found) = pair_in(&t, r"n\own.jpg", r"o\own.jpg", &data);
        set_dacl(&copy, KEPT_TO_ITSELF);
        refused(&keep, &copy, &found, IN_DOWNLOADS, links::LB_SECURITY.into());
        // As the app has it: the test folder is app data and a cache, nothing merges.
        let (keep, copy, found) = pair_in(&t, r"p\photo.jpg", r"q\photo.jpg", &data);
        refused(&keep, &copy, &found, Settings { rule: links::verdict, ..TEST }, format!("{KEEP_FAILED}(photo.jpg) — {}", links::LB_CACHE));
        // The finder says the same of each file.
        assert!(found[0].files.iter().all(|f| f.link_block == Some(links::LB_CACHE) && !f.link_optin));
    }

    /// The id of the file at `p`, its times (created, modified) and its names.
    fn stamp(p: &Path) -> (dups::FileKey, SystemTime, SystemTime, u32) {
        let m = fs::metadata(p).unwrap();
        (key(p), m.created().unwrap(), m.modified().unwrap(), names_of(p))
    }

    /// Makes `paths` files of their own, with the ledger next to the test folder `t`.
    fn unlink(t: &Temp, paths: &[&Path]) -> JobInfo {
        let ledger = links::Ledger::beside(&t.0.join("merges"));
        wait(begin_unlink(paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(), Sink::default(), TEST, ledger).unwrap())
    }

    #[test]
    fn unlinked_names_become_files_of_their_own() {
        let t = Temp::new("unlink");
        let data = content(2 << 20, 22);
        let (a, b, c) = (t.dir("a").join("x.zip"), t.dir("b").join("x.zip"), t.dir("c").join("x.zip"));
        fs::write(&a, &data).unwrap();
        // Downloaded: the mark of the web, an alternate stream, goes along.
        let zone = |p: &Path| PathBuf::from(format!("{}:Zone.Identifier", p.display()));
        fs::write(zone(&a), b"[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        File::options().write(true).open(&a).unwrap().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)).unwrap();
        fs::hard_link(&a, &b).unwrap();
        fs::hard_link(&a, &c).unwrap();
        set_attrs(&a, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE);
        let shown_at = |p: &Path| p.to_string_lossy().into_owned();
        let ledger = links::Ledger::beside(&t.0.join("merges"));
        let merge = |linked: &Path| links::Merge { kept: shown_at(&a), linked: shown_at(linked), size: 2 << 20, at: 1 };
        ledger.add(&merge(&b)).unwrap();
        ledger.add(&merge(&c)).unwrap();
        let before = stamp(&a);
        assert_eq!(before.3, 3);

        let job = unlink(&t, &[&b]);
        assert_eq!((job.op.as_str(), job.state.as_str()), ("unlink", "done"), "{:?}", job.errors);
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert_eq!((job.done_items, job.total_items), (1, 1));
        assert_eq!((job.done_bytes, job.total_bytes), (6 << 20, 6 << 20));
        // `b` is a file of its own with the same content, times, attributes and stream; `a` and `c` are one file still.
        let (mine, theirs) = (stamp(&b), stamp(&a));
        assert_ne!(mine.0, theirs.0);
        assert_eq!(key(&c), theirs.0);
        assert_eq!((mine.1, mine.2, mine.3), (before.1, before.2, 1));
        assert_eq!((theirs.1, theirs.2, theirs.3), (before.1, before.2, 2));
        assert_eq!(fs::read(&b).unwrap(), data);
        assert_eq!(fs::read(&a).unwrap(), data);
        assert_eq!(fs::read(zone(&b)).unwrap(), fs::read(zone(&a)).unwrap());
        assert_eq!(attrs(&b) & SETTABLE, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE);
        assert_eq!(attrs(&a) & SETTABLE, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE);
        // Nothing left beside it, and the ledger forgets the merge that is undone.
        assert_eq!(names(&t.0.join("b")), ["x.zip"]);
        assert_eq!(ledger.read(), [merge(&c)]);

        // The last two names: both files of their own (the second has only itself left by then).
        let job = unlink(&t, &[&a, &c]);
        assert_eq!(job.state, "done");
        assert!(job.errors.is_empty(), "{:?}", job.errors);
        assert_ne!(key(&a), key(&c));
        assert_eq!((names_of(&a), names_of(&c)), (1, 1));
        assert_eq!(fs::read(&c).unwrap(), data);
        assert!(ledger.read().is_empty());
        for p in [&a, &b, &c] {
            set_attrs(p, FILE_ATTRIBUTE_NORMAL);
        }
    }

    #[test]
    fn unlink_turns_down_what_it_may_not_touch_and_leaves_nothing_behind() {
        let t = Temp::new("unlink-refused");
        let data = content(300_000, 23);
        let shown_at = |p: &Path| p.to_string_lossy().into_owned();
        let only = t.dir("o").join("only.bin");
        fs::write(&only, &data).unwrap();
        let gone_one = t.0.join("gone.bin");
        let remote = PathBuf::from(r"\\nas\share\x.bin");
        let mut refusals = vec![
            (only.clone(), UNLINK_LAST.to_string()),
            (gone_one.clone(), UNLINK_GONE.to_string()),
            (remote.clone(), UNLINK_NOT_LOCAL.to_string()),
            (t.dir("o").clone(), UNLINK_NOT_PLAIN.to_string()),
        ];
        // Windows's own programs are names of files in WinSxS.
        let dism = known_folder(&FOLDERID_Windows).map(|w| w.join("System32").join("Dism").join("DismCore.dll")).filter(|p| p.exists());
        if let Some(d) = &dism {
            refusals.push((d.clone(), UNLINK_SYSTEM.to_string()));
        }
        let paths: Vec<&Path> = refusals.iter().map(|(p, _)| p.as_path()).collect();
        let job = unlink(&t, &paths);
        assert_eq!(job.state, "failed");
        let want: Vec<(String, String)> = refusals.iter().map(|(p, why)| (shown_at(p), why.clone())).collect();
        assert_eq!(job.errors, want);
        assert_eq!(fs::read(&only).unwrap(), data);
        assert!(matches!(begin_unlink(Vec::new(), Sink::default(), TEST, links::Ledger::beside(&t.0)), Err(why) if why == NOTHING_TO_UNLINK));

        // A name a program holds without letting it go: the copy made for it goes again, nothing changes.
        let (a, b) = (t.dir("a").join("x.bin"), t.dir("b").join("x.bin"));
        fs::write(&a, &data).unwrap();
        fs::hard_link(&a, &b).unwrap();
        let held = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&b).unwrap();
        let job = unlink(&t, &[&b]);
        drop(held);
        assert_eq!(job.state, "failed");
        assert_eq!(job.errors, [(shown_at(&b), format!("{UNLINK_NOT_PUT}: 다른 프로그램이 쓰고 있어요"))]);
        assert_eq!(key(&a), key(&b));
        assert_eq!(names(&t.0.join("b")), ["x.bin"]);
        assert_eq!(fs::read(&b).unwrap(), data);
        // Let go of, it goes through.
        let job = unlink(&t, &[&b]);
        assert_eq!(job.state, "done", "{:?}", job.errors);
        assert_ne!(key(&a), key(&b));
    }
}

// ---- v0.4: move a folder to another drive and leave a junction ------------------
//
// A relocation moves a folder that fills a drive (a game library, a package
// cache) to another local NTFS drive and leaves a junction where it was, so
// programs keep finding it by its old path. It is one job (op "relocate",
// the usual "job-progress" events, cancel with `cancel_job`):
//
// 0. before the job starts: the checks in `plan_relocation`, the right to
//    rename the folder and to make a junction in its place among them, and
//    a destination outside any cloud-sync folder (`in_sync_root`);
// 1. count the folder, and check the other drive has room for it and 1 GiB
//    more; anything in it that belongs to a cloud-sync folder (a OneDrive
//    placeholder, an online-only file) turns it down here, before anything
//    is copied (`in_the_cloud`);
// 2. copy it into a new hidden folder there (`.name.ff-relocating`) with the
//    copy routines above (files under partial names, links copied as links,
//    never followed); the copy gets the permissions of the folder it lands in,
//    as a copy in Explorer does;
// 3. check the copy against the original: every entry's place and kind, each
//    file's size and modified time, and so the counts and the bytes;
// 4. write the relocation down as "relocating" (`relocations.json` in the
//    app's data folder), with where the original goes and where the copy is;
//    rename the original to `name.ff-old` next to it, which Windows refuses
//    while a program has anything in it open, and check once more that it
//    holds just what the copy holds, so nothing written in between is lost;
// 5. give the copy the folder's name, and make the junction at the
//    original's path, pointing to it;
// 6. write the relocation down as "active";
// 7. delete `name.ff-old` for good (and write step 6 again if it failed: a
//    full drive is the likeliest reason, and step 7 may have made room).
//
// Step 7 is move semantics, as for a move across drives: the original goes
// only once all of it is at the target, checked entry by entry, and the
// junction leads there. It does not go to the Recycle Bin, which would keep a
// second full copy on the drive the user wants room on. When a step from 4 on
// fails, what was done is undone (the renames back, our copy removed, the
// record too) and the reason goes into the job's errors; the job ends as
// failed. Cancel works up to the check in step 4 (the original then goes back
// to its name); from step 5 on the job finishes. A job cut short from step 4
// on (a crash, a power cut) is put right at the next start (`recover`).
//
// Undo (`undo_relocate`, op "restore") is the same the other way round: copy
// the target back into `.name.ff-restoring` next to the junction, check,
// write it down as "restoring", rename the target to `name.ff-old` (in use:
// it stays), check again, remove the junction (the junction alone, never what
// it points to), give the copy the original name, mark the record "restored",
// and delete the target folder for good.
//
// One relocation or undo runs at a time, among all the app's processes (`Busy`).

/// Room a relocation leaves on the drive it copies to, past what it copies.
const SPACE_MARGIN: u64 = 1 << 30;
/// The hidden folder a relocation copies into, next to where the folder goes...
const RELOCATING_EXT: &str = ".ff-relocating";
/// ...and the one an undo copies back into, next to the junction.
const RESTORING_EXT: &str = ".ff-restoring";
/// The name a folder being moved has until its copy has taken its place.
const OLD_EXT: &str = ".ff-old";
const RELOCATIONS_FILE: &str = "relocations.json";
// From WinBase.h and WinNT.h, like the ones at the top.
const DRIVE_FIXED: u32 = 3;
const FILE_SUPPORTS_REPARSE_POINTS: u32 = 0x80;

const RELOCATE_BUSY: &str = "다른 폴더를 옮기거나 되돌리는 중이에요. 끝난 뒤에 다시 해 보세요";
pub(crate) const RECOVER_BUSY: &str = "지난번에 끝나지 않은 옮기기를 정리하는 중이에요. 잠시 후 다시 해 보세요";
const SOURCE_MISSING: &str = "옮길 폴더를 찾을 수 없어요";
const ONLY_FOLDERS: &str = "폴더만 다른 드라이브로 옮길 수 있어요";
const SPECIAL_FOLDER: &str = "정션·심볼릭 링크, 다른 드라이브를 붙인 폴더, 클라우드 폴더 같은 특수한 폴더는 옮길 수 없어요";
const NOT_LOCAL: &str = "이 컴퓨터의 드라이브(드라이브 문자가 있는)에 있는 폴더만 옮길 수 있어요";
const WHOLE_DRIVE_TOO: &str = "드라이브 전체는 옮길 수 없어요";
const NO_WINDOWS: &str = "Windows 폴더는 옮길 수 없어요";
const NO_PROGRAMS: &str = "프로그램 설치 폴더(Program Files)는 옮길 수 없어요";
const NO_PROGRAM_DATA: &str = "ProgramData 폴더 전체는 옮길 수 없어요. 안에 있는 폴더는 하나씩 옮길 수 있어요";
const NO_PROFILE: &str = "사용자 폴더 전체는 옮길 수 없어요. 안에 있는 폴더는 하나씩 옮길 수 있어요";
const NO_SYSTEM: &str = "Windows가 쓰는 시스템 폴더는 옮길 수 없어요";
const NO_APP_DATA: &str = "fine-file이 쓰는 폴더는 옮길 수 없어요";
const HOLDS_GUARDED: &str = "옮기면 안 되는 폴더(Windows, 프로그램, 사용자 폴더 등)가 들어 있어서 옮길 수 없어요";
const NO_JUNCTION_HERE: &str = "이 드라이브에는 정션을 만들 수 없어요. 이 컴퓨터에 붙은 NTFS 드라이브의 폴더만 옮길 수 있어요";
const DEST_NOT_LOCAL: &str = "옮겨 갈 곳은 이 컴퓨터에 붙은 NTFS 드라이브여야 해요 (USB 메모리, 네트워크 드라이브 말고)";
const SAME_DRIVE: &str = "같은 드라이브 안으로는 옮길 수 없어요. 다른 드라이브를 고르세요";
const DRIVE_UNKNOWN: &str = "드라이브를 확인할 수 없어서 옮기지 않았어요";
const DEST_GUARDED: &str = "그 폴더에는 옮길 수 없어요 (Windows나 프로그램이 쓰는 폴더예요)";
const DEST_CLOUD: &str = "원드라이브 같은 클라우드 동기화 폴더 안으로는 옮길 수 없어요 (동기화 앱이 옮긴 파일을 클라우드에 올리거나 온라인 전용으로 바꿀 수 있어요)";
const TARGET_TAKEN: &str = "옮겨 갈 곳에 같은 이름이 이미 있어요";
const NO_COPY_FOLDER: &str = "복사본을 담을 폴더를 만들 수 없어요";
const IN_USE: &str = "사용 중인 파일이 있어서 옮기지 못했어요 — 프로그램을 닫고 다시 시도하세요";
const NO_JUNCTION: &str = "정션을 만들지 못해서 옮기지 않았어요";
const NO_JUNCTION_SECURITY: &str = "정션에 원래 폴더의 권한을 그대로 옮기지 못해서 옮기지 않았어요";
/// The junction stays (a move done, an undo put back) but did not get the folder's permissions.
const JUNCTION_BARE: &str = "정션에 원래 폴더의 권한을 입히지 못했어요 (지금은 소유자만 열 수 있어요)";
/// An undo is done, but the folder did not get its own permissions back exactly.
const FOLDER_BARE: &str = "되돌렸지만 폴더의 원래 권한을 그대로 돌려놓지 못했어요 (옮겨 갔던 곳의 권한 복사본이 남아 있어요)";
const JUNCTION_STAYS: &str = "정션을 지우지 못해서 되돌리지 않았어요";
const NOT_OUR_JUNCTION: &str = "정션이 옮긴 폴더가 아닌 다른 곳을 가리키고 있어요";
const STUCK: &str = "원래대로 되돌리지도 못했어요";
const NOT_RECORDED: &str = "옮긴 기록을 저장하지 못했어요";
const OLD_LEFT: &str = "다 옮긴 뒤 남은 원래 폴더를 다 지우지 못했어요";
const COPY_LEFT: &str = "복사본을 다 지우지 못했어요";
const NOT_A_FOLDER: &str = "폴더가 아니라서 지우지 않았어요";
const UNREADABLE: &str = "폴더를 열 수 없어요";
const BAD_BOOK: &str = "옮긴 기록 파일을 읽을 수 없어요";
const NO_RECORD: &str = "옮긴 기록을 찾을 수 없어요";
const ALREADY_RESTORED: &str = "이미 되돌린 폴더예요";
const BROKEN: &str = "정션이나 옮긴 폴더가 없어졌거나 바뀌어서 되돌릴 수 없어요";
const HOLDS_CLOUD: &str = "원드라이브 같은 클라우드 동기화 폴더의 항목이 들어 있어서 옮길 수 없어요 (옮기려면 모두 내려받아야 하고, 원래 자리에서 지우면 클라우드에서도 지워져요)";
const NO_RIGHT_MOVE: &str = "이 폴더를 옮길 권한이 없어요 (폴더 이름을 바꾸고 그 자리에 정션을 만들 수 있어야 해요)";
const NO_RIGHT_RESTORE: &str = "되돌릴 권한이 없어요 (옮겨 둔 폴더와 정션의 이름을 바꾸거나 지울 수 있어야 해요)";
const RECORD_GUARDED: &str = "옮긴 기록에 적힌 폴더가 옮기거나 되돌리면 안 되는 곳이라서 되돌리지 않았어요";
/// Why a relocation's undo, a plan's undo and the start's putting right of
/// what was cut short are not done while the app runs elevated: they go by
/// records in the app's data folder, which any program of this account can
/// change, and would then do what those say with rights such programs lack.
pub(crate) const ELEVATED_JOURNALS: &str = "관리자 권한으로 실행 중에는 되돌리기와 중단된 작업 복구를 하지 않아요. 일반 권한으로 다시 켜 주세요";

#[derive(Serialize, Deserialize, Clone)]
pub struct Relocation {
    pub id: String,
    /// Where the folder used to be; now a junction pointing at `target`.
    pub original: String,
    pub target: String,
    pub size: u64,
    /// FILETIME
    pub created_at: i64,
    /// "active" | "restored" | "broken" (the junction or the target is gone).
    /// The file also has "relocating" and "restoring": a job between taking
    /// the folder away and finishing (`Book::list` shows what that amounts to).
    pub state: String,
    /// Only in the file, while a job is in that stretch: what `recover`
    /// needs to put things right after a crash or a power cut there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Pending>,
    /// The original folder's own permissions (`root_security`), taken before
    /// it was moved, so an undo puts them back exactly (not the protected
    /// copy the move leaves on the target) and the junction left in its place
    /// carries them. A record from before v0.10.3 has none: the undo then
    /// leaves the folder's permissions as the move's copy routines made them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_security: Option<RootSecurity>,
}

/// A folder's own DACL and owner, kept so a relocation's undo and its junction
/// can put them back as they were — an inherited DACL inherited, a protected
/// one protected, not turned into the other.
#[derive(Serialize, Deserialize, Clone)]
struct RootSecurity {
    /// The owner and DACL as SDDL (`O:...D:...`), `security_sddl`.
    sddl: String,
    /// Whether the DACL was protected from what its folder passes down.
    protected: bool,
}

/// Where a relocation or undo has the folder while it switches it over.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Pending {
    /// Where the folder is taken away to (`name.ff-old`), written down before it is...
    old: String,
    /// ...our checked copy, until it gets the folder's name...
    copy: String,
    /// ...and what both hold.
    tally: Tally,
}

/// Moves the folder `source` into `dest_parent` (on another drive) and leaves
/// a junction at `source`, so programs keep finding it. Runs as a job
/// (op "relocate", the usual "job-progress" events) and returns its id.
#[tauri::command]
pub async fn start_relocate(app: AppHandle, source: String, dest_parent: String) -> Result<u64, String> {
    blocking(move || begin_relocate(Path::new(&source), Path::new(&dest_parent), app, Settings::APP, Book::app())).await?
}

/// Every folder moved this way (kept across restarts), newest first. Fails
/// (BAD_BOOK) when the record file cannot be read.
#[tauri::command]
pub async fn relocations() -> Result<Vec<Relocation>, String> {
    blocking(|| Book::app().list()).await?
}

/// Moves a relocated folder back and removes the junction (job op "restore").
/// Not while the app runs elevated (ELEVATED_JOURNALS).
#[tauri::command]
pub async fn undo_relocate(app: AppHandle, id: String) -> Result<u64, String> {
    journals_refused()?;
    blocking(move || begin_restore(&id, app, Settings::APP, Book::app())).await?
}

/// ELEVATED_JOURNALS while this process runs elevated.
pub(crate) fn journals_refused() -> Result<(), String> {
    if win::is_elevated() { Err(ELEVATED_JOURNALS.into()) } else { Ok(()) }
}

/// Puts right what a relocation or an undo that was cut short (a crash, a
/// power cut, the app killed) left behind (`recover`). Run once at start,
/// on a thread of its own. Does nothing while a relocation or undo runs, in
/// this process or another of the app's.
pub fn recover_relocations(app: AppHandle) {
    recover_relocations_in(&Book::app(), win::is_elevated(), app);
}

/// `recover_relocations` with the book `book` (tests keep their own).
/// Elevated, nothing is put right (ELEVATED_JOURNALS): a job that fails (op
/// "restore") tells so, by each folder left as it is; its id, if there is one.
fn recover_relocations_in(book: &Book, elevated: bool, events: impl Events) -> Option<u64> {
    if elevated {
        let cut: Vec<PathBuf> = book.read().unwrap_or_default().into_iter().filter(|r| r.pending.is_some()).map(|r| r.original.into()).collect();
        return tell_left_alone("restore", cut, events);
    }
    if let Ok(busy) = Busy::claim_for_recovery(book)
        && busy.lock.is_some()
    {
        recover(book);
    }
    None
}

/// A job of `op` that fails, telling of each of `left` (an item a job cut
/// short left behind) that it stays as it is while the app runs elevated
/// (ELEVATED_JOURNALS); its id. None when `left` is empty.
fn tell_left_alone(op: &str, left: Vec<PathBuf>, events: impl Events) -> Option<u64> {
    if left.is_empty() {
        return None;
    }
    let dest = shown(&common_folder(&left));
    let run = new_job(op, false, Vec::new(), dest, events, Settings::APP);
    launch(run, move |run| {
        for path in &left {
            run.error(path, ELEVATED_JOURNALS);
        }
        run.gave_up = true;
        Ok(())
    })
    .ok()
}

/// Checks that `source` may go to `dest_parent`, then starts the job.
fn begin_relocate(source: &Path, dest_parent: &Path, events: impl Events, settings: Settings, book: Book) -> Result<u64, String> {
    let busy = Busy::claim(&book)?;
    let plan = plan_relocation(source, dest_parent, settings)?;
    let dest = plan.to.parent().map_or_else(String::new, shown);
    let run = new_job("relocate", false, vec![plan.from.clone()], dest, events, settings);
    launch(run, move |run| {
        let _busy = busy;
        let done = run.relocate_steps(&plan, &book);
        run.settle(&plan.from, done)
    })
}

/// Checks that the relocation `id` can be undone, then starts the job.
fn begin_restore(id: &str, events: impl Events, settings: Settings, book: Book) -> Result<u64, String> {
    let busy = Busy::claim(&book)?;
    let record = book.read()?.into_iter().find(|r| r.id == id).ok_or(NO_RECORD)?;
    if record.state == "restored" {
        return Err(ALREADY_RESTORED.into());
    }
    let (original, target) = (PathBuf::from(&record.original), PathBuf::from(&record.target));
    // One still "relocating" or "restoring" whose junction leads to its folder
    // is as good as active (only its record was not brought up to date).
    if !matches!(record.state.as_str(), "active" | "relocating" | "restoring") || !linked(&original, &target) {
        return Err(BROKEN.into());
    }
    // The record file is the app's, but any program of this account can write it:
    // both folders must still be where a relocation may take a folder from and put one.
    let places = guarded_places();
    if !movable(&original, &places) || !movable(&target, &places) {
        return Err(RECORD_GUARDED.into());
    }
    // The folder is renamed out of the way, the junction removed and the copy
    // named in its place: rights for that are not something to find out only
    // after the whole copy.
    if !may_switch(&target) || !may_switch(&original) {
        return Err(NO_RIGHT_RESTORE.into());
    }
    let dest = original.parent().map_or_else(String::new, shown);
    let run = new_job("restore", false, vec![target], dest, events, settings);
    launch(run, move |run| {
        let _busy = busy;
        let done = run.restore_steps(&record, &book);
        run.settle(&original, done)
    })
}

/// One relocation or undo at a time, so two of them never move a folder the
/// other is copying or checking (and `recover` never touches one that is
/// running): one in this process (`RELOCATING`), and one among all the app's
/// processes by holding the lock file next to the record file open without
/// sharing (`relocations.lock`); Windows closes it when a process ends, by a
/// crash too. Let go of when the job's work is over. The turn `recover`
/// takes at start is marked as such (`RECOVERING`, and a second lock file,
/// `relocations-recovery.lock`, for the other processes), so a relocation
/// asked for meanwhile is told why it has to wait (RECOVER_BUSY).
struct Busy {
    lock: Option<File>,
    /// Held while `recover` has the turn.
    recovery: Option<File>,
    recovering: bool,
}

static RELOCATING: AtomicBool = AtomicBool::new(false);
/// This process's turn is `recover`'s.
static RECOVERING: AtomicBool = AtomicBool::new(false);

/// Whether this process puts right a relocation cut short (`recover`), which
/// cannot be cancelled. Asked before an update (lib.rs).
pub(crate) fn recovering() -> bool {
    RECOVERING.load(Ordering::SeqCst)
}

/// Whether another of the app's processes has the relocation turn (`Busy`):
/// a relocation, its undo or `recover`. Asked before an update, whose
/// installer would end it. Only one process holds the lock at a time.
pub(crate) fn relocating_elsewhere() -> bool {
    !RELOCATING.load(Ordering::SeqCst) && held_elsewhere(&Book::app().lock_path())
}

/// Opens `path` for nobody else: Busy's locks.
fn hold_exclusive(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(path)
}

/// Whether another process holds the lock file `path` (`hold_exclusive`).
/// Asked by opening it with every kind of sharing, which only an exclusive
/// holder refuses; a file that is not there is held by nobody, and none is made.
fn held_elsewhere(path: &Path) -> bool {
    let opened = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).open(path);
    matches!(opened, Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32))
}

impl Busy {
    fn claim(book: &Book) -> Result<Busy, String> {
        Busy::take(book, false)
    }

    /// The turn for `recover`, marked as such.
    fn claim_for_recovery(book: &Book) -> Result<Busy, String> {
        Busy::take(book, true)
    }

    fn take(book: &Book, recovering: bool) -> Result<Busy, String> {
        if RELOCATING.swap(true, Ordering::SeqCst) {
            return Err(if RECOVERING.load(Ordering::SeqCst) { RECOVER_BUSY } else { RELOCATE_BUSY }.into());
        }
        if recovering {
            RECOVERING.store(true, Ordering::SeqCst);
        }
        let mut busy = Busy { lock: None, recovery: None, recovering };
        match hold_exclusive(&book.lock_path()) {
            Ok(lock) => busy.lock = Some(lock),
            Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {
                let why = if held_elsewhere(&book.recovery_lock_path()) { RECOVER_BUSY } else { RELOCATE_BUSY };
                return Err(why.into());
            }
            // No room for even an empty file, say: the turn in this process still holds.
            Err(_) => {}
        }
        if recovering {
            busy.recovery = hold_exclusive(&book.recovery_lock_path()).ok();
        }
        Ok(busy)
    }

    /// Whether a relocation or undo (or `recover`) has the turn now, in
    /// this process or another of the app's.
    fn taken(book: &Book) -> bool {
        RELOCATING.load(Ordering::SeqCst) || held_elsewhere(&book.lock_path())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.recovery.take();
        self.lock.take();
        if self.recovering {
            RECOVERING.store(false, Ordering::SeqCst);
        }
        RELOCATING.store(false, Ordering::SeqCst);
    }
}

/// What a relocation moves where.
struct Plan {
    /// The folder, where it really is (`real_path`).
    from: PathBuf,
    meta: Metadata,
    /// Its path on the other drive.
    to: PathBuf,
}

/// Everything a relocation needs, checked before it starts. Room on the
/// other drive is checked again once the job has counted the folder.
fn plan_relocation(source: &Path, dest_parent: &Path, settings: Settings) -> Result<Plan, String> {
    if !source.is_absolute() || !dest_parent.is_absolute() {
        return Err(BAD_PATH.into());
    }
    let meta = std::fs::symlink_metadata(win::verbatim(source)).map_err(|e| format!("{SOURCE_MISSING}: {}", reason(&e)))?;
    if meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(ONLY_FOLDERS.into());
    }
    // A junction or link (also one an earlier relocation left), a mounted
    // drive, a cloud-files folder.
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SPECIAL_FOLDER.into());
    }
    // With the junctions above it resolved: the folder is moved, and the
    // junction made, where it really is.
    let from = real_path(source).ok_or(NOT_LOCAL)?;
    let Some(name) = from.file_name().map(OsStr::to_os_string) else { return Err(WHOLE_DRIVE_TOO.into()) };
    if let Some(why) = guarded(&from, false) {
        return Err(why.into());
    }
    let here = volume(&from).filter(|v| v.fixed && v.links).ok_or(NO_JUNCTION_HERE)?;
    // Following a link is fine here: that is the folder the user picked.
    if !std::fs::metadata(win::verbatim(dest_parent)).is_ok_and(|m| m.is_dir()) {
        return Err(DEST_MISSING.into());
    }
    // A junction can only lead to a local drive; a removable one may be gone tomorrow.
    let dest = real_path(dest_parent).ok_or(DEST_NOT_LOCAL)?;
    let there = volume(&dest).filter(|v| v.fixed && v.fs.eq_ignore_ascii_case("NTFS")).ok_or(DEST_NOT_LOCAL)?;
    if settings.rename {
        match (here.id, there.id) {
            (Some(a), Some(b)) if !a.eq_ignore_ascii_case(&b) => {}
            (Some(_), Some(_)) => return Err(SAME_DRIVE.into()),
            _ => return Err(DRIVE_UNKNOWN.into()),
        }
    }
    if inside(&dest, &from) {
        return Err(INTO_ITSELF.into());
    }
    if let Some(why) = guarded(&dest, true) {
        return Err(why.into());
    }
    // The sync app would upload the copy and may turn what is in it into
    // placeholders: the last check would then fail after the whole copy, and
    // removing that copy from inside the sync root removes it from the cloud.
    if in_sync_root(&dest) {
        return Err(DEST_CLOUD.into());
    }
    let to = dest.join(name);
    if std::fs::symlink_metadata(win::verbatim(&to)).is_ok() {
        return Err(format!("{TARGET_TAKEN}: {}", shown(&to)));
    }
    // Known now, not after the whole copy: a folder the user may read but not
    // rename (one owned by SYSTEM in ProgramData, say) would be copied in full
    // only to be turned down at the switch.
    if !may_switch(&from) {
        return Err(NO_RIGHT_MOVE.into());
    }
    let free = win::free_space(&dest);
    if free.is_none_or(|f| f < SPACE_MARGIN) {
        return Err(no_room(&to, SPACE_MARGIN, free));
    }
    Ok(Plan { from, meta, to })
}

/// Why the real path `path` may not be moved (with `dest`: may not have a
/// folder moved into it), if so.
fn guarded(path: &Path, dest: bool) -> Option<&'static str> {
    guarded_among(path, dest, &guarded_places())
}

/// `guarded` with the folders of `guarded_places` looked up already.
fn guarded_among(path: &Path, dest: bool, places: &[(PathBuf, bool, &'static str)]) -> Option<&'static str> {
    // On every drive: the folders Windows keeps at the root, and every Program Files.
    let top = path.components().find_map(|c| match c {
        Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
        _ => None,
    });
    if let Some(top) = top {
        let why = match top.as_str() {
            "system volume information" | "$recycle.bin" => Some(NO_SYSTEM),
            t if t.starts_with("program files") => Some(NO_PROGRAMS),
            _ => None,
        };
        if why.is_some() {
            return if dest { Some(DEST_GUARDED) } else { why };
        }
    }
    if dest {
        return places.iter().any(|(place, tree, _)| *tree && inside(path, place)).then_some(DEST_GUARDED);
    }
    // The guarded folder itself, or a folder in a guarded tree...
    if let Some((_, _, why)) = places.iter().find(|(place, tree, _)| inside(path, place) && (*tree || inside(place, path))) {
        return Some(why);
    }
    // ...or one that holds a guarded folder (`C:\Users`, `%USERPROFILE%\AppData`).
    places.iter().any(|(place, _, _)| inside(place, path)).then_some(HOLDS_GUARDED)
}

/// Folders Windows and programs need where they are: (folder, guarded with
/// everything in it, why). ProgramData and the profile folders are guarded
/// only as wholes: the folders in them may move.
fn guarded_places() -> Vec<(PathBuf, bool, &'static str)> {
    let mut places = Vec::new();
    places.extend(known_folder(&FOLDERID_Windows).map(|p| (p, true, NO_WINDOWS)));
    for id in [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX64, &FOLDERID_ProgramFilesX86] {
        places.extend(known_folder(id).map(|p| (p, true, NO_PROGRAMS)));
    }
    places.push((crate::scan::data_dir(), true, NO_APP_DATA));
    places.extend(known_folder(&FOLDERID_ProgramData).map(|p| (p, false, NO_PROGRAM_DATA)));
    places.extend(known_folder(&FOLDERID_Profile).map(|p| (p, false, NO_PROFILE)));
    if let Some(users) = known_folder(&FOLDERID_UserProfiles) {
        // Every account's profile folder, Public and Default too.
        for e in std::fs::read_dir(&users).into_iter().flatten().flatten() {
            if e.metadata().is_ok_and(|m| kind_of(&m).is_dir()) {
                places.push((users.join(e.file_name()), false, NO_PROFILE));
            }
        }
        places.push((users, false, NO_PROFILE));
    }
    places
}

/// A known folder's path (`FOLDERID_*`), when this Windows has one.
fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let mut p: PWSTR = null_mut();
        let hr = SHGetKnownFolderPath(id, 0, null_mut(), &mut p);
        let path = if hr >= 0 && !p.is_null() {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            Some(PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p, len))))
        } else {
            None
        };
        if !p.is_null() {
            CoTaskMemFree(p.cast());
        }
        path.filter(|p| !p.as_os_str().is_empty())
    }
}

/// Where `path` really is: `C:\...`, with every junction on the way
/// resolved and each name as it is on disk. None when it cannot be found,
/// is not on a drive with a letter (a network share, a volume without one),
/// or is not valid Unicode (it could not be written down as it is).
fn real_path(path: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(win::verbatim(path)).ok()?;
    let rest = real.to_str()?.strip_prefix(r"\\?\")?;
    let b = rest.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\').then(|| PathBuf::from(rest))
}

/// A path as the window shows it: without the `\\?\` of a listing.
pub(crate) fn shown(path: &Path) -> String {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.get(1..3) == Some(r":\") => rest.to_string(),
        _ => text.into_owned(),
    }
}

/// The volume that holds a real path (`C:\...`).
struct Volume {
    /// Its GUID path (`\\?\Volume{...}\`), which tells two volumes apart.
    id: Option<String>,
    /// A local fixed disk: not removable, not on the network.
    fixed: bool,
    /// "NTFS", "ReFS", "exFAT", ...
    fs: String,
    /// It can hold junctions.
    links: bool,
}

fn volume(path: &Path) -> Option<Volume> {
    let text = |buf: &[u16]| String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]);
    let mut buf = [0u16; 1024];
    if unsafe { GetVolumePathNameW(win::wide_path(path).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } == 0 {
        return None;
    }
    // It comes back with the `\\?\` it was asked with.
    let mut root: Vec<u16> = buf[..buf.iter().position(|&c| c == 0)?].to_vec();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    if root.starts_with(&verbatim) && root.get(5) == Some(&(b':' as u16)) {
        root.drain(..4);
    }
    root.push(0);
    let fixed = unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED;
    let (mut flags, mut fs) = (0u32, [0u16; 64]);
    let ok = unsafe {
        GetVolumeInformationW(root.as_ptr(), null_mut(), 0, null_mut(), null_mut(), &mut flags, fs.as_mut_ptr(), fs.len() as u32)
    };
    if ok == 0 {
        return None;
    }
    let mut guid = [0u16; 64];
    let named = unsafe { GetVolumeNameForVolumeMountPointW(root.as_ptr(), guid.as_mut_ptr(), guid.len() as u32) } != 0;
    Some(Volume { id: named.then(|| text(&guid)), fixed, fs: text(&fs), links: flags & FILE_SUPPORTS_REPARSE_POINTS != 0 })
}

fn no_room(to: &Path, need: u64, free: Option<u64>) -> String {
    let gb = |bytes: u64| format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64);
    let path = shown(to);
    let drive = path.get(..2).unwrap_or_default();
    match free {
        Some(free) => format!("{drive} 드라이브에 공간이 부족해요 (여유 1 GB를 더해 {} 필요, {} 남음)", gb(need), gb(free)),
        None => format!("{drive} 드라이브에 남은 공간을 확인할 수 없어요"),
    }
}

/// A rename refused because something in the folder is open. Windows gives
/// ERROR_ACCESS_DENIED for a folder with a file open in it; the rights to
/// rename it were checked before the job started (`may_switch`).
fn in_use(e: &io::Error) -> bool {
    matches!(e.raw_os_error().map(|c| c as u32), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_ACCESS_DENIED))
}

/// Whether the rights allow the switch a relocation or undo makes at `path`:
/// renaming it (DELETE on it, which Windows also grants for FILE_DELETE_CHILD
/// on its folder) and putting a new folder or junction in its place
/// (FILE_ADD_SUBDIRECTORY on its folder). Asked by opening them so, which
/// changes nothing. A link there is the link itself. Only a refusal counts:
/// anything open in the folder shows when it is renamed (`in_use`), and what
/// cannot be opened for other reasons fails later with its own reason.
fn may_switch(path: &Path) -> bool {
    let denied = |p: &Path, access: u32| {
        let opened = OpenOptions::new()
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(win::verbatim(p));
        matches!(opened, Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32))
    };
    !denied(path, DELETE) && !path.parent().is_some_and(|parent| denied(parent, FILE_ADD_SUBDIRECTORY))
}

/// Whether a listed entry belongs to a cloud-sync folder (OneDrive and
/// others using the cloud-files API): a placeholder, also one kept on this
/// device, or a file that is only in the cloud. Copying one downloads it,
/// and removing it from its sync root removes it from the cloud too. Told
/// from the listing's attributes, and for a reparse point that is not a link
/// from its tag (`win::reparse_tag` lists it, opening nothing); one whose
/// tag cannot be read counts as one.
fn in_the_cloud(listed: &Metadata, path: impl FnOnce() -> PathBuf) -> bool {
    let attrs = listed.file_attributes();
    crate::preview::is_online_only(attrs)
        || (attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && !listed.file_type().is_symlink()
            && win::reparse_tag(&path()).is_none_or(crate::scan::is_cloud_tag))
}

/// Whether the real path `path` is in a cloud-sync folder (OneDrive and
/// others using the cloud-files API): it or a folder above it is a
/// placeholder, the sync root itself among them. Told from the listing of
/// each (`win::reparse_tag`), which opens nothing. A sync root is in the
/// user's own folders, so its listing can always be read; a folder whose
/// listing cannot be read is not taken for one.
fn in_sync_root(path: &Path) -> bool {
    path.ancestors()
        .filter(|p| p.parent().is_some())
        .any(|p| win::reparse_tag(p).is_some_and(crate::scan::is_cloud_tag))
}

/// Whether `path` is a folder of its own (not a link, not missing).
fn is_plain_dir(path: &Path) -> bool {
    !path.as_os_str().is_empty() && std::fs::symlink_metadata(win::verbatim(path)).is_ok_and(|m| kind_of(&m) == Kind::Dir)
}

/// Whether nothing is at `path` (nor was a path ever written down).
fn gone(path: &Path) -> bool {
    path.as_os_str().is_empty() || matches!(std::fs::symlink_metadata(win::verbatim(path)), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Whether `path` is a folder that holds just what `want` says.
fn holds(path: &Path, want: &Tally) -> bool {
    is_plain_dir(path) && tally(path, &AtomicBool::new(false)).is_ok_and(|t| t == *want)
}

/// Now as a FILETIME (100 ns ticks since 1601).
fn filetime_now() -> i64 {
    const UNIX_TO_FILETIME: i64 = 116_444_736_000_000_000;
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| (d.as_nanos() / 100) as i64) + UNIX_TO_FILETIME
}

/// Where relocations are written down: `relocations.json` in the app's data
/// folder (tests keep their own).
#[derive(Clone)]
struct Book(PathBuf);

impl Book {
    fn app() -> Book {
        Book(crate::scan::data_dir().join(RELOCATIONS_FILE))
    }

    /// The file `Busy` holds open (`relocations.lock`).
    fn lock_path(&self) -> PathBuf {
        self.0.with_extension("lock")
    }

    /// The one it also holds while `recover` has the turn.
    fn recovery_lock_path(&self) -> PathBuf {
        self.0.with_file_name("relocations-recovery.lock")
    }

    fn read(&self) -> Result<Vec<Relocation>, String> {
        match std::fs::read(&self.0) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{BAD_BOOK}: {e}")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{BAD_BOOK}: {}", reason(&e))),
        }
    }

    /// Every relocation, newest first, as the window shows it: "active",
    /// "restored" or "broken". An active one whose junction is gone, is no
    /// longer a junction or leads elsewhere, or whose target is gone, shows
    /// as broken; the file keeps it active, since a drive that is only
    /// unplugged comes back. One a job left "relocating" or "restoring" shows
    /// as what it amounts to: active once its junction leads to its folder,
    /// restored once its folder is back; while a relocation or undo runs (or
    /// `recover` puts one right), here or in another of the app's windows, a
    /// relocation not yet switched over is left out and an undo is still
    /// active. Otherwise it is broken: `recover` could not tell what to do.
    fn list(&self) -> Result<Vec<Relocation>, String> {
        let running = Busy::taken(self);
        let mut list = self.read()?;
        list.retain_mut(|r| {
            let (original, target) = (Path::new(&r.original), Path::new(&r.target));
            let state = match r.state.as_str() {
                "restored" => "restored",
                "active" | "relocating" | "restoring" if linked(original, target) => "active",
                "relocating" if running => return false,
                "restoring" if running => "active",
                "restoring" if is_plain_dir(original) && r.pending.as_ref().is_none_or(|p| gone(Path::new(&p.copy))) => "restored",
                _ => "broken",
            };
            r.state = state.into();
            r.pending = None;
            true
        });
        list.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(list)
    }

    /// Changes the list and writes it back atomically (temp file, flush,
    /// rename). Changes take turns. A file that cannot be understood is put
    /// aside (`put_aside`), not written over.
    fn change(&self, f: impl FnOnce(&mut Vec<Relocation>)) -> Result<(), String> {
        static TURN: Mutex<()> = Mutex::new(());
        let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
        let mut list = match std::fs::read(&self.0) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(list) => list,
                Err(_) => {
                    self.put_aside()?;
                    Vec::new()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(reason(&e)),
        };
        f(&mut list);
        let bytes = serde_json::to_vec_pretty(&list).map_err(|e| e.to_string())?;
        crate::index::write_cache(&self.0, &bytes).map_err(|e| reason(&e))
    }

    /// Writes `record` down in place of the one with its id (or as a new one).
    fn keep(&self, record: &Relocation) -> Result<(), String> {
        self.change(|list| match list.iter_mut().find(|r| r.id == record.id) {
            Some(r) => *r = record.clone(),
            None => list.push(record.clone()),
        })
    }

    fn forget(&self, id: &str) -> Result<(), String> {
        self.change(|list| list.retain(|r| r.id != id))
    }

    /// Moves the record file out of the way under a name of its own
    /// (`relocations.json.<FILETIME>.bad`), never over one put aside before:
    /// it may be the only note of where earlier relocations went.
    fn put_aside(&self) -> Result<(), String> {
        let stamp = filetime_now();
        for n in 1..=PARTIAL_TRIES {
            let mut name = self.0.clone().into_os_string();
            name.push(if n == 1 { format!(".{stamp:016x}.bad") } else { format!(".{stamp:016x}-{n}.bad") });
            match win::rename(&self.0, Path::new(&name)) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("{BAD_BOOK}: {}", reason(&e))),
            }
        }
        Err(format!("{BAD_BOOK}: {NO_FREE_NAME}"))
    }
}

/// What `recover` made of one record.
enum Mended {
    /// Left as it is.
    Left,
    /// Everything is as before the relocation: no record.
    Undone,
    /// The record is this now ("active", "restored").
    Now(&'static str),
}

/// Puts right what a relocation or undo cut short left: each record still
/// "relocating" or "restoring", whose `pending` says where the folder was
/// being taken away to and where our checked copy was. Done as the job
/// itself would have gone on:
/// - a relocation whose junction leads to its folder had only the removal of
///   the original left (`name.ff-old`): it goes now, and the record is active;
/// - one that took the folder away and did not get as far gives it back its
///   name, and our copy goes, whether it got its name yet or not; so does one
///   that never took the folder away. The record goes too;
/// - an undo whose copy has the folder's name had only the removal of the
///   folder it came from left: that goes now, and the record is restored;
/// - one that took the folder away and did not get as far puts it back, and
///   its junction if that went already; our copy goes, and the record is
///   active again.
///
/// A folder is only ever removed while it still holds just what was checked
/// (`holds`): a folder taken away only once its copy has its place (move
/// semantics), a copy only once the folder is back where it was. Anything
/// else is left alone for the user to look at, and shows as broken. So is a
/// record that says more than a job of this app would have left (`record_holds`).
fn recover(book: &Book) {
    let Ok(list) = book.read() else { return };
    let places = guarded_places();
    for r in list {
        let Some(p) = &r.pending else { continue };
        if !record_holds(&r, p, &places) {
            continue;
        }
        let (original, target, old, copy) = (Path::new(&r.original), Path::new(&r.target), Path::new(&p.old), Path::new(&p.copy));
        let root = r.root_security.as_ref();
        let mended = match r.state.as_str() {
            "relocating" => recover_relocation(original, target, old, copy, &p.tally, root),
            "restoring" => recover_restore(original, target, old, copy, &p.tally, root),
            _ => Mended::Left,
        };
        let _ = match mended {
            Mended::Left => Ok(()),
            Mended::Undone => book.forget(&r.id),
            Mended::Now(state) => book.keep(&Relocation { state: state.into(), pending: None, ..r.clone() }),
        };
    }
}

/// `root`: the folder's own permissions as recorded (`Relocation::root_security`).
fn recover_relocation(original: &Path, target: &Path, old: &Path, copy: &Path, want: &Tally, root: Option<&RootSecurity>) -> Mended {
    if linked(original, target) {
        // Made owner-only, the job may have stopped before it got the folder's permissions:
        // they go on now (again, if it got them).
        let _ = dress_junction(original, root);
        if holds(old, want) {
            let _ = remove_tree(old);
        }
        return Mended::Now("active");
    }
    if gone(original) && is_plain_dir(old) {
        if win::rename(old, original).is_err() {
            return Mended::Left;
        }
    } else if !(is_plain_dir(original) && gone(old)) {
        return Mended::Left;
    }
    // The folder is where it was. Under its hidden name our copy never was
    // anything else; under the folder's name on the other drive it goes only
    // while the folder here still holds the same (else that one may be all
    // there is: the user's to look at).
    if holds(copy, want) {
        let _ = remove_tree(copy);
    }
    if !gone(target) {
        if !(holds(original, want) && holds(target, want)) {
            return Mended::Left;
        }
        let _ = remove_tree(target);
    }
    Mended::Undone
}

fn recover_restore(original: &Path, target: &Path, old: &Path, copy: &Path, want: &Tally, root: Option<&RootSecurity>) -> Mended {
    if is_plain_dir(original) && gone(copy) {
        // What the copy came from goes only while the copy still holds the
        // same: otherwise this folder may not be ours.
        let ours = holds(original, want);
        // Its own permissions back on it, which the job may have stopped short of (the
        // copy has a protected copy of them): only on the very copy the job made.
        if ours && let Some(rs) = root {
            let _ = put_root_security(original, rs);
        }
        if ours && holds(old, want) {
            let _ = remove_tree(old);
        }
        return Mended::Now("restored");
    }
    if gone(target) && is_plain_dir(old) && win::rename(old, target).is_err() {
        return Mended::Left;
    }
    if gone(original) && is_plain_dir(target) && create_junction(original, target).is_err() {
        return Mended::Left;
    }
    if !linked(original, target) || !gone(old) {
        return Mended::Left;
    }
    // Back to active: the junction leads to the folder again and carries the
    // folder's own permissions (it was remade owner-only just now).
    let _ = dress_junction(original, root);
    if holds(copy, want) {
        let _ = remove_tree(copy);
    }
    Mended::Now("active")
}

/// Whether a record cut short says only what a relocation or undo of this
/// app would have left: its folder and its target where a relocation may
/// take a folder from and put one (`movable`), and the folder taken away and
/// the copy under the names the job gives them (`marked_name`), next to the
/// folders they stand for. The record file is in the app's data folder,
/// which any program of this account can write: nothing is moved or removed
/// on its word alone.
fn record_holds(r: &Relocation, p: &Pending, places: &[(PathBuf, bool, &'static str)]) -> bool {
    let (original, target) = (Path::new(&r.original), Path::new(&r.target));
    // A relocation takes the folder away next to itself and copies next to the
    // target; an undo the other way round.
    let (old_by, copy_by, copy_ext) = match r.state.as_str() {
        "relocating" => (original, target, RELOCATING_EXT),
        "restoring" => (target, original, RESTORING_EXT),
        _ => return false,
    };
    movable(original, places)
        && movable(target, places)
        && marked_as(Path::new(&p.old), old_by, false, OLD_EXT)
        && marked_as(Path::new(&p.copy), copy_by, true, copy_ext)
}

/// Whether a relocation may take the folder at `path` away and put one
/// there: a full path, not guarded itself, in a folder nothing may be moved
/// into (`guarded_among`, as `plan_relocation` checks both ends).
fn movable(path: &Path, places: &[(PathBuf, bool, &'static str)]) -> bool {
    path.is_absolute()
        && guarded_among(path, false, places).is_none()
        && path.parent().is_some_and(|dir| guarded_among(dir, true, places).is_none())
}

/// Whether `path` is one of the names `marked_name` gives next to `next_to`
/// (with `hide` and `ext`), in that very folder.
fn marked_as(path: &Path, next_to: &Path, hide: bool, ext: &str) -> bool {
    let (Some(dir), Some(name), Some(home)) = (path.parent(), path.file_name(), next_to.parent()) else { return false };
    if !(same_text(dir, home) || win::same_entry(dir, home)) {
        return false;
    }
    (1..=PARTIAL_TRIES)
        .filter_map(|n| marked_name(next_to, hide, ext, n))
        .any(|m| m.file_name().is_some_and(|m| same_text(Path::new(m), Path::new(name))))
}

/// Whether `original` is still a junction to `target`, and `target` a folder.
fn linked(original: &Path, target: &Path) -> bool {
    junction_target(original).is_some_and(|to| same_text(&to, target))
        && std::fs::symlink_metadata(win::verbatim(target)).is_ok_and(|m| kind_of(&m) == Kind::Dir)
}

fn same_text(a: &Path, b: &Path) -> bool {
    let text = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    text(a) == text(b)
}

/// Where the junction at `path` leads (`D:\x`), read from the junction
/// itself; None when it is not a junction.
fn junction_target(path: &Path) -> Option<PathBuf> {
    let data = read_reparse(path).ok()?;
    let half = |i: usize| data.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    if u32::from_le_bytes(data.get(..4)?.try_into().ok()?) != IO_REPARSE_TAG_MOUNT_POINT {
        return None;
    }
    // After the header: the substitute name's offset and length (in bytes, from the names' start at 16).
    let (offset, len) = (half(8)?, half(10)?);
    let mut name: Vec<u16> = data.get(16 + offset..16 + offset + len)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let nt: Vec<u16> = r"\??\".encode_utf16().collect();
    if name.starts_with(&nt) {
        name.drain(..4);
    }
    Some(PathBuf::from(OsString::from_wide(&name)))
}

/// Makes `link` a junction to the folder `to` (`D:\x`), as `mklink /J`
/// does. Never over anything: `link` must be free.
pub(crate) fn create_junction(link: &Path, to: &Path) -> io::Result<()> {
    // Owner-only until its permissions go on (a relocation then puts the
    // original folder's on): the stand-in for a folder is never, for a moment,
    // open to more than its maker.
    make_reparse_folder(link, &junction_data(to)?, true)
}

/// The reparse data of a junction to the folder `to` (`D:\x`).
fn junction_data(to: &Path) -> io::Result<Vec<u8>> {
    let mut print: Vec<u16> = to.as_os_str().encode_wide().collect();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    if print.starts_with(&verbatim) {
        print.drain(..4);
    }
    let substitute: Vec<u16> = r"\??\".encode_utf16().chain(print.iter().copied()).collect();
    let (s, p) = (substitute.len() * 2, print.len() * 2);
    // Past the 8-byte header: the names' offsets and lengths (8 bytes), then each name and a NUL.
    let body = 8 + s + 2 + p + 2;
    if 8 + body > MAXIMUM_REPARSE_DATA_BUFFER_SIZE {
        return Err(io::Error::from_raw_os_error(ERROR_FILENAME_EXCED_RANGE as i32));
    }
    let mut data = Vec::with_capacity(8 + body);
    data.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    // Data length, reserved; substitute name offset and length, print name offset and length.
    for half in [body, 0, 0, s, s + 2, p] {
        data.extend((half as u16).to_le_bytes());
    }
    for unit in substitute.iter().chain([&0]).chain(&print).chain([&0]) {
        data.extend(unit.to_le_bytes());
    }
    Ok(data)
}

/// Removes the junction `link` if it leads to `target`: the junction alone
/// (RemoveDirectoryW on it), never what it points to.
fn remove_junction(link: &Path, target: &Path) -> io::Result<()> {
    if !junction_target(link).is_some_and(|to| same_text(&to, target)) {
        return Err(io::Error::other(NOT_OUR_JUNCTION));
    }
    let wide = win::wide_path(link);
    if unsafe { RemoveDirectoryW(wide.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What a folder holds, to tell whether a copy is whole and the original
/// unchanged since: its entries of each kind, the bytes of its files, and a
/// digest of every entry's place, kind and (for a file) size and modified
/// time. Links are counted as links, never followed.
///
/// A tally is kept in `relocations.json` (`Pending`) and compared at a later
/// start, maybe by a newer build: the digest is blake3, which stays the same
/// across Rust and app versions, and `scheme` says how it was made. A tally
/// of another scheme (an older file, or a future change to what is hashed)
/// never matches, so `recover` leaves such a folder alone rather than
/// deleting it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
struct Tally {
    files: u64,
    folders: u64,
    links: u64,
    bytes: u64,
    digest: u64,
    #[serde(default)]
    scheme: u32,
}

/// `Tally::scheme` of the tallies this build makes.
const TALLY_SCHEME: u32 = 1;

/// One entry's part of `Tally::digest`: its place below the root (names
/// joined by `\`, in UTF-16), its kind and, for a file, its size and
/// modified time. Every field has a fixed width or a length in front.
fn entry_digest(place: &[u16], kind: u8, extra: &[u64]) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(&(place.len() as u64).to_le_bytes());
    for unit in place {
        h.update(&unit.to_le_bytes());
    }
    h.update(&[kind]);
    for x in extra {
        h.update(&x.to_le_bytes());
    }
    let out = h.finalize();
    u64::from_le_bytes(out.as_bytes()[..8].try_into().unwrap_or_default())
}

/// The tally of the folder `root`. Fails when a folder in it cannot be
/// read: what it holds cannot be vouched for then; and when anything in it
/// belongs to a cloud-sync folder (`in_the_cloud`), which a relocation never
/// takes away from there.
fn tally(root: &Path, cancel: &AtomicBool) -> Result<Tally, Halt> {
    let mut t = Tally { scheme: TALLY_SCHEME, ..Tally::default() };
    // Each folder with its place below `root`, names joined by `\`.
    let mut stack: Vec<(PathBuf, Vec<u16>)> = vec![(win::verbatim(root), Vec::new())];
    while let Some((dir, place)) = stack.pop() {
        if cancel.load(Ordering::SeqCst) {
            return Err(Halt::Stop);
        }
        let unreadable = |e: io::Error| Halt::Why(format!("{UNREADABLE}: {} ({})", shown(&dir), reason(&e)));
        for entry in std::fs::read_dir(&dir).map_err(unreadable)? {
            let entry = entry.map_err(unreadable)?;
            let listed = entry.metadata().map_err(unreadable)?;
            if in_the_cloud(&listed, || entry.path()) {
                return Err(Halt::Why(format!("{HOLDS_CLOUD}: {}", shown(&entry.path()))));
            }
            let mut name = place.clone();
            if !name.is_empty() {
                name.push(b'\\' as u16);
            }
            name.extend(entry.file_name().encode_wide());
            let digest = match kind_of(&listed) {
                Kind::File => {
                    // Opened, not listed: a listing can lag behind (a file still
                    // being written, the other names of a hard-linked file).
                    let m = std::fs::symlink_metadata(entry.path()).unwrap_or(listed);
                    t.files += 1;
                    t.bytes += m.len();
                    entry_digest(&name, 0, &[m.len(), m.last_write_time()])
                }
                Kind::Dir => {
                    t.folders += 1;
                    let digest = entry_digest(&name, 1, &[]);
                    stack.push((entry.path(), name));
                    digest
                }
                Kind::Link { dir } => {
                    t.links += 1;
                    entry_digest(&name, 2, &[dir as u64])
                }
            };
            t.digest = t.digest.wrapping_add(digest);
        }
    }
    Ok(t)
}

/// Deletes the folder `root` and everything in it for good. A link in it
/// goes as a link: what it points to is never entered or touched, and
/// `root` is left alone if it is a link itself. Only for a folder whose
/// contents are safe elsewhere (what a relocation moved, once its checked
/// copy has taken its place) or a copy of our own.
///
/// Depth first, by handles: each folder is opened as itself and held
/// (`open_itself`) from before it is listed until it goes, and so is every
/// folder above it, so none of them can be swapped for a junction meanwhile.
/// Each entry is opened as itself too, relative to the handle of the folder
/// that holds it (`open_child`, never down a fresh path), and goes through
/// that handle: one a folder turned into since it was listed (a junction, a
/// symbolic link, a mount point set in place) is the reparse point, and goes
/// as the link it now is, never entered.
fn remove_tree(root: &Path) -> Result<(), String> {
    /// A folder held open while what is in it goes.
    struct Level {
        path: PathBuf,
        dir: File,
        attrs: u32,
        entries: Option<std::fs::ReadDir>,
    }
    // What did not go, with why.
    let mut failed: Vec<(PathBuf, io::Error)> = Vec::new();
    let level = |path: PathBuf, dir: File, attrs: u32, failed: &mut Vec<(PathBuf, io::Error)>| {
        let entries = std::fs::read_dir(&path).map_err(|e| failed.push((path.clone(), e))).ok();
        Level { path, dir, attrs, entries }
    };
    let top = win::verbatim(root);
    let opened = open_to_delete(&top, true).and_then(|f| kind_open(&f).map(|(kind, attrs)| (f, kind, attrs)));
    let mut stack = match opened {
        Ok((dir, Kind::Dir, attrs)) => vec![level(top, dir, attrs, &mut failed)],
        Ok(_) => return Err(NOT_A_FOLDER.into()),
        Err(e) => return Err(reason(&e)),
    };
    while let Some(at) = stack.last_mut() {
        let entry = match at.entries.as_mut().and_then(Iterator::next) {
            Some(Ok(entry)) => entry,
            Some(Err(e)) => {
                failed.push((at.path.clone(), e));
                continue;
            }
            None => {
                // What it held is gone (or stays, and so does the folder): it goes as it is let go of.
                if let Err(e) = delete_open(&at.dir, at.attrs) {
                    failed.push((at.path.clone(), e));
                }
                stack.pop();
                continue;
            }
        };
        let (name, path) = (entry.file_name(), entry.path());
        // In the held folder, through its handle: an entry swapped in since the
        // listing is still resolved in this very folder, not down a path.
        let opened = open_child_to_delete(&at.dir, &name, true).and_then(|f| kind_open(&f).map(|(kind, attrs)| (f, kind, attrs)));
        match opened {
            Ok((dir, Kind::Dir, attrs)) => {
                let next = level(path, dir, attrs, &mut failed);
                stack.push(next);
            }
            // A file, or a link (also one a folder was swapped for since it was listed): the entry itself.
            Ok((entry, _, attrs)) => {
                if let Err(e) = delete_open(&entry, attrs) {
                    failed.push((path, e));
                }
            }
            Err(e) => failed.push((path, e)),
        }
    }
    match failed.first() {
        None => Ok(()),
        Some((path, e)) => Err(format!("{}개 항목을 지우지 못했어요 ({}: {})", failed.len(), shown(path), reason(e))),
    }
}

/// Opens the entry at `path` as itself (a link as the link, never what it
/// points to; a folder too), with `access` and the right to read its
/// attributes. Held (`hold`) without FILE_SHARE_DELETE: while it is open
/// nobody can rename or remove it, nor a folder above it (NTFS renames no
/// folder with something open in it), so a path through it leads where it
/// did when it was opened.
fn open_itself(path: &Path, access: u32, hold: bool) -> io::Result<File> {
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | if hold { 0 } else { FILE_SHARE_DELETE };
    OpenOptions::new()
        .access_mode(access | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(share)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(win::verbatim(path))
}

/// `open_itself` with DELETE, and with the right to change its attributes
/// where that is given (`delete_open` may need it for a read-only entry).
fn open_to_delete(path: &Path, hold: bool) -> io::Result<File> {
    open_itself(path, DELETE | FILE_WRITE_ATTRIBUTES, hold).or_else(|e| match e.raw_os_error() {
        Some(c) if c == ERROR_ACCESS_DENIED as i32 => open_itself(path, DELETE, hold),
        _ => Err(e),
    })
}

/// NTSTATUS failures are NT codes; these turn into the Win32 errors the rest
/// of the file reads (`reason`).
const STATUS_SUCCESS: NTSTATUS = 0;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
/// NtCreateFile: open an existing object only.
const FILE_OPEN: u32 = 1;
/// NtCreateFile CreateOptions (ntifs.h): the reparse point itself, a folder
/// (backup intent), synchronous.
const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x0000_4000;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const FILE_OPEN_REPARSE_POINT_OPT: u32 = 0x0020_0000;

/// A counted UTF-16 string (ntdef.h), for `ObjectAttributes`.
#[repr(C)]
struct UnicodeString {
    /// Bytes in use, and the buffer's size, both without a NUL.
    length: u16,
    maximum: u16,
    buffer: *mut u16,
}

/// What to open, relative to a folder handle (ntdef.h); only the fields used.
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    /// The folder the name is resolved in; NtCreateFile goes no further up.
    root: HANDLE,
    name: *const UnicodeString,
    attributes: u32,
    security: *const c_void,
    sqos: *const c_void,
}

/// Opens the entry `name` directly inside the folder open as `parent`, as
/// itself (a link, a mount point among them, is the reparse point, never what
/// it leads to). Resolved in that very folder through its handle, not down a
/// path a folder swapped for a mount point in the meantime could bend: what is
/// opened is a real child of `parent`. `parent` needs no particular right for
/// it (FILE_TRAVERSE least of all: an open relative to a handle is not a
/// traverse of it, and asking for that right only turns down folders that do
/// not grant it, or that a program holds without letting others read).
/// `access` and `hold` are as `open_itself`.
fn open_child(parent: &File, name: &OsStr, access: u32, hold: bool) -> io::Result<File> {
    let mut wide: Vec<u16> = name.encode_wide().collect();
    // A single name in `parent`, never a path or `.`/`..` that would lead out
    // of it (a listing never gives those, but the open goes relative to `parent`).
    let out = wide.is_empty() || wide.contains(&(b'\\' as u16)) || wide.contains(&(b'/' as u16)) || name == "." || name == "..";
    if out {
        return Err(io::Error::from_raw_os_error(ERROR_INVALID_NAME as i32));
    }
    let bytes = (wide.len() * 2) as u16;
    let us = UnicodeString { length: bytes, maximum: bytes, buffer: wide.as_mut_ptr() };
    let oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root: parent.as_raw_handle() as HANDLE,
        name: &us,
        attributes: OBJ_CASE_INSENSITIVE,
        security: null(),
        sqos: null(),
    };
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | if hold { 0 } else { FILE_SHARE_DELETE };
    let options = FILE_OPEN_REPARSE_POINT_OPT | FILE_OPEN_FOR_BACKUP_INTENT | FILE_SYNCHRONOUS_IO_NONALERT;
    let mut handle: HANDLE = null_mut();
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let status = unsafe {
        NtCreateFile(&mut handle, access | FILE_READ_ATTRIBUTES | SYNCHRONIZE, &oa, &mut status_block, null(), 0, share, FILE_OPEN, options, null(), 0)
    };
    if status != STATUS_SUCCESS {
        return Err(io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32));
    }
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

/// `open_child` with DELETE, and with the right to change its attributes
/// where that is given (as `open_to_delete`): a folder's handle is then the
/// one its own children are opened through, which needs no right of its own.
fn open_child_to_delete(parent: &File, name: &OsStr, hold: bool) -> io::Result<File> {
    open_child(parent, name, DELETE | FILE_WRITE_ATTRIBUTES, hold).or_else(|e| match e.raw_os_error() {
        Some(c) if c == ERROR_ACCESS_DENIED as i32 => open_child(parent, name, DELETE, hold),
        _ => Err(e),
    })
}

/// The names in the folder open as `dir` (with FILE_LIST_DIRECTORY), read
/// through that very handle, without `.` and `..`: what this folder holds,
/// never what a path to it may lead to by now (an empty folder set as a mount
/// point in place while it is held). With each, whether it was listed as a
/// folder (not a link to one).
fn names_in(dir: &File) -> io::Result<Vec<(OsString, bool)>> {
    use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;
    use windows_sys::Win32::Storage::FileSystem::{FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FILE_FULL_DIR_INFO};
    let h = dir.as_raw_handle() as HANDLE;
    // 64 KiB, aligned as the records are (8).
    let mut buf = vec![0u64; 8 << 10];
    let mut class = FileFullDirectoryRestartInfo;
    let mut names = Vec::new();
    let (dot, dots): (&[u16], &[u16]) = (&[b'.' as u16], &[b'.' as u16, b'.' as u16]);
    loop {
        if unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32) } == 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) { Ok(names) } else { Err(e) };
        }
        class = FileFullDirectoryInfo;
        let mut at = 0usize;
        loop {
            // SAFETY: Windows wrote whole records into `buf`, each at an 8-aligned offset, its
            // name (FileNameLength bytes) running on past the struct.
            let (next, name, attrs, tag) = unsafe {
                let record = buf.as_ptr().cast::<u8>().add(at).cast::<FILE_FULL_DIR_INFO>();
                let len = (*record).FileNameLength as usize / 2;
                let start = std::ptr::addr_of!((*record).FileName).cast::<u16>();
                // A reparse point's tag is where the size of its extended attributes would be.
                ((*record).NextEntryOffset as usize, std::slice::from_raw_parts(start, len), (*record).FileAttributes, (*record).EaSize)
            };
            if name != dot && name != dots {
                // A name surrogate (a junction, a symbolic link) is a link, as `kind_of` has it.
                let link = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & 0x2000_0000 != 0;
                names.push((OsString::from_wide(name), attrs & FILE_ATTRIBUTE_DIRECTORY != 0 && !link));
            }
            if next == 0 {
                break;
            }
            at += next;
        }
    }
}

/// What the entry open as `file` (`open_itself`) is, as `kind_of` tells a
/// listed one (a junction or symbolic link is a link, a cloud-files folder a
/// folder), and its attributes.
fn kind_open(file: &File) -> io::Result<(Kind, u32)> {
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as u32;
    if unsafe { GetFileInformationByHandleEx(file.as_raw_handle() as HANDLE, FileAttributeTagInfo, (&raw mut info).cast(), size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let attrs = info.FileAttributes;
    let dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    // A name surrogate, as std's `is_symlink` has it.
    let link = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && info.ReparseTag & 0x2000_0000 != 0;
    Ok((if link { Kind::Link { dir } } else if dir { Kind::Dir } else { Kind::File }, attrs))
}

/// Marks the entry open as `file` (`open_itself` with DELETE) to go as it is
/// closed: a link as the link, a folder only when it is empty; read-only or
/// not (`attrs`, its attributes). A Windows older than 10 (1809), or a file
/// system without POSIX semantics, knows only the old way, for which the
/// read-only mark is taken off first (and put back should it still fail).
fn delete_open(file: &File, attrs: u32) -> io::Result<()> {
    let h = file.as_raw_handle() as HANDLE;
    let ex = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    if unsafe { SetFileInformationByHandle(h, FileDispositionInfoEx, (&raw const ex).cast(), std::mem::size_of_val(&ex) as u32) } != 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(ERROR_DIR_NOT_EMPTY as i32) {
        return Err(e);
    }
    let read_only = attrs & FILE_ATTRIBUTE_READONLY != 0;
    let put_attrs = |attrs: u32| {
        let attrs = attrs & SETTABLE;
        let basic = FILE_BASIC_INFO { FileAttributes: if attrs == 0 { FILE_ATTRIBUTE_NORMAL } else { attrs }, ..unsafe { std::mem::zeroed() } };
        unsafe { SetFileInformationByHandle(h, FileBasicInfo, (&raw const basic).cast(), std::mem::size_of_val(&basic) as u32) };
    };
    if read_only {
        put_attrs(attrs & !FILE_ATTRIBUTE_READONLY);
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    if unsafe { SetFileInformationByHandle(h, FileDispositionInfo, (&raw const info).cast(), std::mem::size_of_val(&info) as u32) } != 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if read_only {
        put_attrs(attrs);
    }
    Err(e)
}

/// Makes the hidden folder a copy goes into, next to `to` and named after it
/// with `ext`, its owner's alone (`FILLING_SDDL`) from the moment it is
/// there. A new one: a folder of that name already there is not ours, and
/// the next free name is taken.
fn make_copy_folder(to: &Path, ext: &str) -> Result<PathBuf, Halt> {
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let lock = sddl_to_security(FILLING_SDDL).map_err(|e| Halt::Why(format!("{NO_COPY_FOLDER}: {}", reason(&e))))?;
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: lock.0, bInheritHandle: 0 };
    for n in 1..=PARTIAL_TRIES {
        let Some(copy) = marked_name(to, true, ext, n) else { break };
        let wide = win::wide_path(&copy);
        if unsafe { CreateDirectoryW(wide.as_ptr(), &sa) } != 0 {
            return Ok(copy);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(Halt::Why(format!("{NO_COPY_FOLDER}: {}", reason(&e))));
        }
    }
    Err(Halt::Why(NO_COPY_FOLDER.into()))
}

// ---- v0.10.2: a relocated folder keeps its permissions --------------------------
//
// A copy gets what its new folder passes down: on another drive that often
// lets every signed-in user change what is in it, and a relocation's copy
// takes the folder's place behind a junction, where nobody would notice. So
// the copy's folder is made its owner's alone, protected, passing that down
// (`FILLING_SDDL`), and everything goes in under that: nothing in it is open
// to more than its owner while it is filled, and a folder that lets nobody
// add to it (read and run only) is filled all the same. Each entry gets the
// DACL of what it copies once it is in (`Run::keep_own_security`), its own
// or all passed down, as it is; a folder once all it holds is in (from the
// bottom up). The copy's folder gets the folder's DACL last (and its owner,
// where Windows lets that be set), protected from what passes down there and
// with every ACE its own (`keep_root_security`). Written through the
// kernel's own call (SetKernelObjectSecurity): as it is, with nothing worked
// out from the folder above or passed on below.

/// A relocation's or an undo's copy while it is filled: its owner's alone
/// (OW: the user, or Administrators for an elevated app), protected, passed
/// down to all that goes in.
const FILLING_SDDL: &str = "D:P(A;OICI;FA;;;OW)";

/// A security descriptor GetSecurityInfo gave, let go of with LocalFree.
struct Security(PSECURITY_DESCRIPTOR);

impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

impl Security {
    /// The DACL, and the owner with `owner`, of the entry open as `file`
    /// (with READ_CONTROL).
    fn of(file: &File, owner: bool) -> io::Result<Security> {
        let what = DACL_SECURITY_INFORMATION | if owner { OWNER_SECURITY_INFORMATION } else { 0 };
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let h = file.as_raw_handle() as HANDLE;
        let e = unsafe { GetSecurityInfo(h, SE_FILE_OBJECT, what, null_mut(), null_mut(), null_mut(), null_mut(), &mut sd) };
        if e != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        Ok(Security(sd))
    }

    /// Whether the DACL is protected from what its folder passes down.
    fn protected(&self) -> bool {
        let (mut control, mut revision) = (0u16, 0u32);
        unsafe { GetSecurityDescriptorControl(self.0, &mut control, &mut revision) != 0 && control & SE_DACL_PROTECTED != 0 }
    }

    /// The DACL; null for none, which lets everyone in.
    fn dacl(&self) -> io::Result<*mut ACL> {
        let (mut present, mut acl, mut defaulted) = (0, null_mut(), 0);
        if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut acl, &mut defaulted) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(if present != 0 { acl } else { null_mut() })
    }

    fn owner(&self) -> PSID {
        let (mut owner, mut defaulted) = (null_mut(), 0);
        if unsafe { GetSecurityDescriptorOwner(self.0, &mut owner, &mut defaulted) } != 0 { owner } else { null_mut() }
    }

    /// Whether the DACL is the entry's own rather than all passed down from
    /// its folder: protected, with an ACE of its own, or none at all.
    fn own(&self) -> io::Result<bool> {
        let acl = self.dacl()?;
        Ok(acl.is_null() || self.protected() || aces(acl).any(|ace| unsafe { (*ace).AceFlags } as u32 & INHERITED_ACE == 0))
    }
}

/// The headers of the ACEs in `acl` (not null).
fn aces(acl: *const ACL) -> impl Iterator<Item = *mut ACE_HEADER> {
    let count = unsafe { (*acl).AceCount } as u32;
    (0..count).filter_map(move |i| {
        let mut ace = null_mut();
        (unsafe { GetAce(acl, i, &mut ace) } != 0).then_some(ace.cast::<ACE_HEADER>())
    })
}

/// A copy of `acl` (not null) with every ACE its own, none marked as passed down.
fn own_aces(acl: *const ACL) -> Vec<u32> {
    let size = unsafe { (*acl).AclSize } as usize;
    // An ACL is aligned like a DWORD.
    let mut buf = vec![0u32; size.div_ceil(4)];
    unsafe { std::ptr::copy_nonoverlapping(acl.cast::<u8>(), buf.as_mut_ptr().cast::<u8>(), size) };
    for ace in aces(buf.as_mut_ptr().cast()) {
        unsafe { (*ace).AceFlags &= !(INHERITED_ACE as u8) };
    }
    buf
}

/// Writes `dacl` (null: none) to the entry open as `file` (with WRITE_DAC),
/// as it is; protected from what its folder passes down when `protected`.
fn put_dacl(file: &File, dacl: *const ACL, protected: bool) -> io::Result<()> {
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let p: PSECURITY_DESCRIPTOR = (&raw mut sd).cast();
    let what = DACL_SECURITY_INFORMATION | if protected { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
    let ok = unsafe {
        InitializeSecurityDescriptor(p, SECURITY_DESCRIPTOR_REVISION) != 0
            && SetSecurityDescriptorDacl(p, 1, dacl, 0) != 0
            && SetSecurityDescriptorControl(p, SE_DACL_PROTECTED, if protected { SE_DACL_PROTECTED } else { 0 }) != 0
            && SetKernelObjectSecurity(file.as_raw_handle() as HANDLE, what, p) != 0
    };
    if ok { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Makes `owner` the owner of the entry open as `file` (with WRITE_OWNER):
/// Windows lets that be the user or a group the user is in (elevated:
/// Administrators), nobody else.
fn put_owner(file: &File, owner: PSID) -> io::Result<()> {
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let p: PSECURITY_DESCRIPTOR = (&raw mut sd).cast();
    let ok = unsafe {
        InitializeSecurityDescriptor(p, SECURITY_DESCRIPTOR_REVISION) != 0
            && SetSecurityDescriptorOwner(p, owner, 0) != 0
            && SetKernelObjectSecurity(file.as_raw_handle() as HANDLE, OWNER_SECURITY_INFORMATION, p) != 0
    };
    if ok { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The owner and DACL of the folder `from`, read before a relocation or an
/// undo copies anything (`keep_root_security` puts them on the copy once it
/// is filled): one whose permissions cannot be read is not moved.
fn root_security_of(from: &Path) -> io::Result<Security> {
    Security::of(&open_itself(from, READ_CONTROL, false)?, true)
}

/// Gives the filled folder `copy` the permissions `have` of the folder it
/// copies (`root_security_of`): its DACL protected and with every ACE its own
/// (so it is what it is there, whatever passes down at `copy`), and its owner
/// where Windows lets that be set. Last, once everything in it has its own
/// (`Run::keep_own_security`), so it changes nothing below it.
fn keep_root_security(have: &Security, copy: &Path) -> io::Result<()> {
    let dacl = have.dacl()?;
    let to = open_itself(copy, WRITE_DAC | WRITE_OWNER | READ_CONTROL, true).or_else(|_| open_itself(copy, WRITE_DAC | READ_CONTROL, true))?;
    // Made by the job and held from here: what gets the permissions is the folder made.
    if kind_open(&to)?.0 != Kind::Dir {
        return Err(io::Error::other(TURNED_LINK));
    }
    let owner = have.owner();
    if !owner.is_null() {
        let _ = put_owner(&to, owner);
    }
    if dacl.is_null() {
        return put_dacl(&to, null(), true);
    }
    let own = own_aces(dacl);
    put_dacl(&to, own.as_ptr().cast(), true)
}

/// Gives the copy `target` the DACL of the entry open as `from` (with
/// READ_CONTROL) where that is the entry's own (`Security::own`): a
/// protected one as it is, with every ACE its own; any other as it is, ACEs
/// passed down and all, which are what the copy's folder passes down too.
/// One whose DACL all comes from its folder is left as it is: so does the
/// copy's, made in the same folder (an unlink's).
fn copy_own_security(from: &File, target: &Path) -> io::Result<()> {
    copy_security(from, target, false)
}

/// `copy_own_security`, and with `all` a DACL all passed down too, as it is
/// (ACEs passed down and all): a relocation's copy, whose folders pass down
/// nothing of their own while it is filled (`FILLING_SDDL`), gets on each
/// entry what its folder will pass down once it has its own. Written through
/// the raw call: nothing is worked out again, nothing passed on below.
fn copy_security(from: &File, target: &Path, all: bool) -> io::Result<()> {
    let have = Security::of(from, false)?;
    if !all && !have.own()? {
        return Ok(());
    }
    let dacl = have.dacl()?;
    let to = open_itself(target, WRITE_DAC | READ_CONTROL, false)?;
    if dacl.is_null() {
        return put_dacl(&to, null(), true);
    }
    if !have.protected() {
        return put_dacl(&to, dacl, false);
    }
    let own = own_aces(dacl);
    put_dacl(&to, own.as_ptr().cast(), true)
}

/// The owner and DACL of the folder `path`, as SDDL, and whether the DACL is
/// protected — kept in the relocation record (`RootSecurity`) so an undo puts
/// the folder's own permissions back exactly. None when they cannot be read.
fn root_security(path: &Path) -> Option<RootSecurity> {
    let have = Security::of(&open_itself(path, READ_CONTROL, false).ok()?, true).ok()?;
    Some(RootSecurity { sddl: security_sddl(&have)?, protected: have.protected() })
}

/// `sd`'s owner and DACL as an SDDL string (`O:...D:...`).
fn security_sddl(sd: &Security) -> Option<String> {
    use windows_sys::Win32::Security::Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1};
    let mut text: PWSTR = null_mut();
    let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    if unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(sd.0, SDDL_REVISION_1, what, &mut text, null_mut()) } == 0 || text.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { LocalFree(text.cast()) };
    Some(sddl)
}

/// A security descriptor parsed from SDDL, let go of with LocalFree (the
/// allocator ConvertString... uses).
fn sddl_to_security(sddl: &str) -> io::Result<Security> {
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    let text = win::wide(sddl);
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Security(sd))
}

/// Puts the recorded owner and DACL `rs` back on the folder `path`, exactly as
/// they were: a protected DACL as it is, an inherited one as it is too,
/// unprotected, ACEs passed down and all (the folder it is in is the same as
/// before, so they are what it passes down). The owner where Windows lets it
/// be set. Used by an undo on the folder it brings back and on the junction a
/// relocation leaves.
///
/// Through the raw kernel call (`put_dacl`): this entry alone. SetSecurityInfo
/// would work the inherited ACEs out again and pass them on to everything in
/// the folder, which the copy routines gave them already: the same result,
/// at a cost that grows with the tree, with no progress and no cancel. The
/// handle is the entry itself (a junction included, never what it leads to).
fn put_root_security(path: &Path, rs: &RootSecurity) -> io::Result<()> {
    let sd = sddl_to_security(&rs.sddl)?;
    let to = open_itself(path, WRITE_DAC | WRITE_OWNER | READ_CONTROL, false).or_else(|_| open_itself(path, WRITE_DAC | READ_CONTROL, false))?;
    // The owner on its own (the user, or a group the user is in): one Windows
    // will not let be set does not keep the DACL from going on.
    let owner = sd.owner();
    if !owner.is_null() {
        let _ = put_owner(&to, owner);
    }
    put_dacl(&to, sd.dacl()?, rs.protected)
}

/// Clears the junction `link` to what its folder passes down (a present but
/// empty DACL, unprotected, so SetSecurityInfo fills in the inherited ACEs):
/// for a relocation with no recorded folder permissions (an older one, or one
/// whose folder could not be read), as a junction made before this was kept
/// would have been — never left owner-only, which could shut a service out of
/// a relocated app folder.
fn inherit_junction(link: &Path) -> io::Result<()> {
    const ACL_REVISION: u8 = 2;
    let mut empty = [0u32; 2];
    // An empty ACL: 8-byte header, no ACEs.
    unsafe {
        let acl = empty.as_mut_ptr().cast::<ACL>();
        (*acl).AclRevision = ACL_REVISION;
        (*acl).AclSize = std::mem::size_of::<ACL>() as u16;
    }
    let to = open_itself(link, WRITE_DAC | READ_CONTROL, false)?;
    let what = DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION;
    let e = unsafe { SetSecurityInfo(to.as_raw_handle() as HANDLE, SE_FILE_OBJECT, what, null_mut(), null_mut(), empty.as_ptr().cast(), null_mut()) };
    if e == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(e as i32)) }
}

/// Puts the recorded original folder permissions on the junction `link`
/// (`put_root_security`), or, with none, lets it inherit from its folder
/// (`inherit_junction`): for the paths that have already committed to the
/// junction (an undo put back, a start putting a job cut short right), which
/// tell a failure (JUNCTION_BARE) where there is a job to tell it in. Again
/// on a junction that has them already changes nothing.
fn dress_junction(link: &Path, rs: Option<&RootSecurity>) -> io::Result<()> {
    match rs {
        Some(rs) => put_root_security(link, rs),
        None => inherit_junction(link),
    }
}

/// Why a relocation or an undo stopped short.
enum Halt {
    /// Cancelled.
    Stop,
    /// The reason, for the job's errors.
    Why(String),
}

impl From<Stop> for Halt {
    fn from(_: Stop) -> Halt {
        Halt::Stop
    }
}

impl Run {
    /// Ends a relocation or an undo: the reason it stopped goes into the
    /// errors (by `path`), and the job fails.
    fn settle(&mut self, path: &Path, done: Result<(), Halt>) -> Result<(), Stop> {
        match done {
            Ok(()) => Ok(()),
            Err(Halt::Stop) => Err(Stop),
            Err(Halt::Why(why)) => {
                self.error(path, why);
                self.gave_up = true;
                Ok(())
            }
        }
    }

    /// Shows `path` as what the job is at.
    fn at(&mut self, path: &Path) {
        self.info.current = Some(shown(path));
        self.publish();
    }

    /// "옮기지" or "되돌리지", for the reasons.
    fn not_done(&self) -> &'static str {
        if self.info.op == "restore" { "되돌리지" } else { "옮기지" }
    }

    fn relocate_steps(&mut self, plan: &Plan, book: &Book) -> Result<(), Halt> {
        let (from, to) = (&plan.from, &plan.to);
        // The folder's own permissions, taken while it is still in place, so
        // the undo puts them back exactly and the junction left behind carries
        // them. None (unreadable) leaves the undo to the move's copy routines.
        let root = root_security(from);
        let (copy, tally) = self.copy_whole(from, &plan.meta, to, RELOCATING_EXT)?;
        let created_at = filetime_now();
        let mut record = Relocation {
            id: format!("{created_at:016x}"),
            original: shown(from),
            target: shown(to),
            size: tally.bytes,
            created_at,
            state: "relocating".into(),
            pending: Some(Pending { old: String::new(), copy: shown(&copy), tally }),
            root_security: root,
        };
        // Written down before the folder is taken away (`take_away` says
        // where to), so a crash from here on is put right at the next start
        // (`recover`). A record that cannot be written (a full drive, often
        // the very one this is to make room on) does not stop the move: it
        // is tried again at the end, once the original is gone.
        let mut noted = false;
        let taken = self.take_away(from, &tally, &mut |old| {
            if let Some(p) = record.pending.as_mut() {
                p.old = shown(old);
            }
            noted = book.keep(&record).is_ok();
        });
        // Given up, with the folder back in its place (not just a folder of that
        // name, which a program may have made meanwhile): there was no relocation.
        let unnote = |noted: bool| {
            if noted && is_plain_dir(from) && record.pending.as_ref().is_none_or(|p| gone(Path::new(&p.old))) {
                let _ = book.forget(&record.id);
            }
        };
        let old = match taken {
            Ok(old) => old,
            Err(halt) => {
                self.discard(&copy);
                unnote(noted);
                return Err(halt);
            }
        };
        // From here on the job finishes: each step goes through or is undone.
        self.at(to);
        if let Err(e) = win::rename(&copy, to) {
            let why = format!("복사본의 이름을 바꾸지 못해서 옮기지 않았어요: {}", reason(&e));
            let halt = self.put_back(&old, from, &copy, why);
            unnote(noted);
            return Err(halt);
        }
        if let Err(e) = (self.settings.junction)(from, to) {
            let halt = self.put_back(&old, from, to, format!("{NO_JUNCTION}: {}", reason(&e)));
            unnote(noted);
            return Err(halt);
        }
        // The junction (made owner-only) gets the original folder's own
        // permissions, so it is reached as the folder was. A failure undoes
        // the switch like a junction that could not be made, as long as the
        // junction goes again; one that stays leads to the whole checked copy,
        // so the move is done (as the next start would finish it) and the job
        // says the junction is still its owner's alone. So does one that
        // could not take on what its folder passes down (no recorded permissions).
        let dressed = match &record.root_security {
            Some(rs) => match put_root_security(from, rs) {
                Err(e) if remove_junction(from, to).is_ok() => {
                    let halt = self.put_back(&old, from, to, format!("{NO_JUNCTION_SECURITY}: {}", reason(&e)));
                    unnote(noted);
                    return Err(halt);
                }
                dressed => dressed,
            },
            None => inherit_junction(from),
        };
        if let Err(e) = dressed {
            self.error(from, format!("{JUNCTION_BARE}: {}", reason(&e)));
        }
        let done = Relocation { state: "active".into(), pending: None, ..record.clone() };
        let saved = book.keep(&done);
        // Move semantics: all of it is at `to` now, checked entry by entry,
        // and the junction leads there.
        self.at(&old);
        if let Err(why) = remove_tree(&old) {
            self.error(&old, format!("{OLD_LEFT}: {why}"));
        }
        // Once more with the room the removal just made. Should that fail too,
        // a record written before the switch still shows it as active.
        if saved.is_err()
            && let Err(why) = book.keep(&done)
            && !noted
        {
            self.error(from, format!("{NOT_RECORDED} ({why}). 되돌릴 때 쓰세요: {} → {} (정션)", shown(from), shown(to)));
        }
        Ok(())
    }

    fn restore_steps(&mut self, record: &Relocation, book: &Book) -> Result<(), Halt> {
        let (original, target) = (PathBuf::from(&record.original), PathBuf::from(&record.target));
        let meta = std::fs::symlink_metadata(win::verbatim(&target))
            .ok()
            .filter(|m| kind_of(m) == Kind::Dir)
            .ok_or_else(|| Halt::Why(BROKEN.into()))?;
        let (copy, tally) = self.copy_whole(&target, &meta, &original, RESTORING_EXT)?;
        // Written down before the folder is taken away, as for a relocation.
        let mut pending = Relocation {
            state: "restoring".into(),
            pending: Some(Pending { old: String::new(), copy: shown(&copy), tally }),
            ..record.clone()
        };
        let mut noted = false;
        let taken = self.take_away(&target, &tally, &mut |old| {
            if let Some(p) = pending.pending.as_mut() {
                p.old = shown(old);
            }
            noted = book.keep(&pending).is_ok();
        });
        // Given up, with the folder and its junction back: active as before.
        let unnote = |noted: bool| {
            if noted && linked(&original, &target) {
                let _ = book.keep(&Relocation { state: "active".into(), pending: None, ..record.clone() });
            }
        };
        let old = match taken {
            Ok(old) => old,
            Err(halt) => {
                self.discard(&copy);
                unnote(noted);
                return Err(halt);
            }
        };
        // From here on the job finishes: each step goes through or is undone.
        self.at(&original);
        if let Err(e) = remove_junction(&original, &target) {
            let halt = self.put_back(&old, &target, &copy, format!("{JUNCTION_STAYS}: {}", reason(&e)));
            unnote(noted);
            return Err(halt);
        }
        if let Err(e) = win::rename(&copy, &original) {
            let why = format!("복사본의 이름을 바꾸지 못해서 되돌리지 않았어요: {}", reason(&e));
            // As it was: the folder back at the target, and the junction to it
            // with the original folder's permissions again.
            if win::rename(&old, &target).and_then(|()| (self.settings.junction)(&original, &target)).is_ok() {
                if let Err(e) = dress_junction(&original, record.root_security.as_ref()) {
                    self.error(&original, format!("{JUNCTION_BARE}: {}", reason(&e)));
                }
                self.discard(&copy);
                unnote(noted);
                return Err(Halt::Why(why));
            }
            let at = if std::fs::symlink_metadata(win::verbatim(&old)).is_ok() { &old } else { &target };
            return Err(Halt::Why(format!("{why}. {STUCK}. 폴더는 {}에, 복사본은 {}에 있어요", shown(at), shown(&copy))));
        }
        // The folder is back where it was; its own permissions go back on it
        // exactly (the move's copy left a protected copy of them). The folder
        // is restored either way, and for a protected folder the copy already
        // matches: a failure is told, not undone.
        if let Some(rs) = &record.root_security
            && let Err(e) = put_root_security(&original, rs)
        {
            self.error(&original, format!("{FOLDER_BARE}: {}", reason(&e)));
        }
        let done = Relocation { state: "restored".into(), pending: None, ..record.clone() };
        let saved = book.keep(&done);
        // Move semantics, the other way: all of it is back at `original`, checked entry by entry.
        self.at(&old);
        if let Err(why) = remove_tree(&old) {
            self.error(&old, format!("{OLD_LEFT}: {why}"));
        }
        if saved.is_err()
            && let Err(why) = book.keep(&done)
            && !noted
        {
            self.error(&original, format!("{NOT_RECORDED}: {why}"));
        }
        Ok(())
    }

    /// Counts `from`, checks that the drive `to` is on has room for it,
    /// copies it into a new hidden folder next to `to` (named with `ext`) and
    /// checks the copy holds just what `from` does. Gives the copy and its
    /// tally; on failure or cancel the copy is removed again.
    fn copy_whole(&mut self, from: &Path, meta: &Metadata, to: &Path, ext: &str) -> Result<(PathBuf, Tally), Halt> {
        self.at(from);
        self.count_movable(from)?;
        self.publish();
        let need = self.info.total_bytes.saturating_add(SPACE_MARGIN);
        let free = to.parent().and_then(win::free_space);
        if free.is_none_or(|f| f < need) {
            return Err(Halt::Why(no_room(to, need, free)));
        }
        let no_security = |run: &Run, e: &io::Error| Halt::Why(format!("권한 설정을 그대로 옮기지 못해서 {} 않았어요: {}", run.not_done(), reason(e)));
        // The folder's permissions, read before anything is copied: one whose cannot be is not moved.
        let root = root_security_of(from).map_err(|e| no_security(self, &e))?;
        // Its owner's alone while it is filled; everything in it gets its own as it goes in,
        // and the copy's folder the folder's last, once nothing in it is to come.
        let copy = make_copy_folder(to, ext)?;
        self.keep_security = true;
        let errors = self.info.error_count;
        // The speed starts with the first bytes, not from zero during the count.
        self.speed = Speed::new();
        // Nothing in a new folder is in the way: no question is ever asked.
        self.always = Some(Choice::Skip);
        let filled = self.fill(from, meta, &copy, errors).and_then(|tally| match keep_root_security(&root, &copy) {
            Ok(()) => Ok(tally),
            Err(e) => Err(no_security(self, &e)),
        });
        match filled {
            Ok(tally) => Ok((copy, tally)),
            Err(halt) => {
                self.discard(&copy);
                Err(halt)
            }
        }
    }

    /// Counts the folder `from` into the job's totals, as `measure` does, and
    /// turns it down as soon as anything in it belongs to a cloud-sync folder
    /// (`in_the_cloud`), before anything is copied: the copy would download
    /// every online-only file, and removing the original afterwards would
    /// take the placeholders out of their sync root (and so out of the cloud).
    fn count_movable(&mut self, from: &Path) -> Result<(), Halt> {
        self.info.total_items += 1;
        let cancel = self.cancel.clone();
        let walked = walk_until(
            from,
            &cancel,
            |entry, listed| in_the_cloud(listed, || entry.path()),
            |bytes, items| {
                self.info.total_bytes += bytes;
                self.info.total_items += items;
                self.tick();
            },
        );
        match walked {
            Walked::Done => Ok(()),
            Walked::Cancelled => Err(Halt::Stop),
            Walked::Refused(at) => Err(Halt::Why(format!("{HOLDS_CLOUD}: {}", shown(&at)))),
        }
    }

    /// Copies what is in `from` into the folder `copy` and checks the copy.
    fn fill(&mut self, from: &Path, meta: &Metadata, copy: &Path, errors: u64) -> Result<Tally, Halt> {
        // The top of a relocation: `from` has no held parent, so it is opened by path.
        let complete = self.put_dir(from, meta, copy, true, None)?;
        copy_folder_look(meta, copy);
        if complete != Some(true) || self.info.error_count > errors {
            return Err(Halt::Why(format!("복사하지 못한 항목이 있어서 {} 않았어요", self.not_done())));
        }
        self.at(from);
        let (original, copied) = (tally(from, &self.cancel)?, tally(copy, &self.cancel)?);
        if original != copied {
            return Err(Halt::Why(format!("복사하는 동안 폴더 내용이 바뀌어서 {} 않았어요", self.not_done())));
        }
        Ok(copied)
    }

    /// Renames the folder `from` out of the way (`name.ff-old` next to it),
    /// which Windows refuses while a program has anything in it open, and
    /// checks it still holds just what `want` says, so nothing written to
    /// it since it was copied is lost. Gives where it went. When that fails
    /// it is back at `from`, or the reason says where it is. `note` is told
    /// each name before the folder is renamed to it.
    fn take_away(&mut self, from: &Path, want: &Tally, note: &mut dyn FnMut(&Path)) -> Result<PathBuf, Halt> {
        self.at(from);
        for n in 1..=PARTIAL_TRIES {
            let Some(old) = marked_name(from, false, OLD_EXT, n) else { break };
            // One that is there already (a try that went wrong) is not ours to touch.
            if std::fs::symlink_metadata(win::verbatim(&old)).is_ok() {
                continue;
            }
            note(&old);
            if let Err(e) = win::rename(from, &old) {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                if in_use(&e) {
                    return Err(Halt::Why(IN_USE.into()));
                }
                return Err(Halt::Why(format!("폴더 이름을 바꾸지 못해서 {} 않았어요: {}", self.not_done(), reason(&e))));
            }
            let now = tally(&old, &self.cancel);
            if matches!(&now, Ok(t) if t == want) {
                return Ok(old);
            }
            let halt = match now {
                Ok(_) => Halt::Why(format!("마지막 확인에서 폴더 내용이 바뀌어 있어서 {} 않았어요", self.not_done())),
                Err(halt) => halt,
            };
            if win::rename(&old, from).is_ok() {
                return Err(halt);
            }
            let stuck = format!("{STUCK}. 폴더는 지금 {}에 있어요", shown(&old));
            return Err(match halt {
                Halt::Why(why) => Halt::Why(format!("{why}. {stuck}")),
                Halt::Stop => {
                    self.error(from, stuck);
                    Halt::Stop
                }
            });
        }
        Err(Halt::Why(NO_FREE_NAME.into()))
    }

    /// Undoes the switch so far: the folder taken away to `old` goes back to
    /// `home`, and our `copy` is removed. Gives `why` for the errors; if the
    /// folder cannot go back, nothing is removed and it says where things are.
    fn put_back(&mut self, old: &Path, home: &Path, copy: &Path, why: String) -> Halt {
        if win::rename(old, home).is_ok() {
            self.discard(copy);
            return Halt::Why(why);
        }
        Halt::Why(format!("{why}. {STUCK}. 원래 폴더는 {}에, 복사본은 {}에 있어요", shown(old), shown(copy)))
    }

    /// Removes a copy of ours; what is left of it goes into the errors.
    fn discard(&mut self, copy: &Path) {
        if let Err(why) = remove_tree(copy) {
            self.error(copy, format!("{COPY_LEFT}: {why}"));
        }
    }
}

// ---- For optimization plans (plans.rs) -------------------------------------------
//
// A plan's steps run through the engines above, each as a job of its own that
// shows in the window's jobs panel like any other (`events`: the window's
// handle in the app), and the plan waits for it.

/// Waits until the job `id` has finished, and gives it as it ended.
fn wait_for(id: u64) -> Result<JobInfo, String> {
    let ended = |reg: &Registry| reg.jobs.iter().find(|e| e.info.id == id).is_none_or(|e| is_finished(&e.info.state));
    let reg = registry();
    let reg = jobs_state().ended.wait_while(reg, |r| !ended(r)).unwrap_or_else(PoisonError::into_inner);
    reg.jobs.iter().find(|e| e.info.id == id).map(|e| e.info.clone()).ok_or_else(|| NO_JOB.into())
}

/// For plans: moves `src` into the folder `dest` the way a pasted move does
/// (to another volume: copy, check, and only then remove the source), and
/// waits for the job. Never over anything, and nothing is asked (`always`
/// is Skip): an item whose name is taken stays where it is, so the caller
/// checks where `src` ended up. Gives the job as it ended.
pub(crate) fn move_and_wait(events: Box<dyn Events>, src: &Path, dest: &Path) -> Result<JobInfo, String> {
    if !src.is_absolute() || !std::fs::metadata(win::verbatim(dest)).is_ok_and(|m| m.is_dir()) {
        return Err(DEST_MISSING.into());
    }
    let mut run = new_job("move", true, vec![src.to_path_buf()], shown(dest), events, Settings::APP);
    run.always = Some(Choice::Skip);
    for_plan(run.id);
    let id = launch(run, Run::work)?;
    wait_for(id)
}

/// For plans: why `source` cannot be relocated into `dest_parent` now, if so:
/// the checks `start_relocate` makes before it starts a job.
pub(crate) fn relocation_refused(source: &Path, dest_parent: &Path) -> Option<String> {
    plan_relocation(source, dest_parent, Settings::APP).err()
}

/// For plans: relocates `source` into `dest_parent` as `start_relocate` does
/// and waits for the job. Gives the job as it ended and, when it went
/// through, the relocation's record (for `restore_and_wait`).
pub(crate) fn relocate_and_wait(events: Box<dyn Events>, source: &Path, dest_parent: &Path) -> Result<(JobInfo, Option<Relocation>), String> {
    // Where it really is: once moved, `source` is a junction to the new place.
    let from = real_path(source).ok_or(NOT_LOCAL)?;
    let since = filetime_now();
    let book = Book::app();
    let id = begin_relocate(source, dest_parent, events, Settings::APP, book.clone())?;
    for_plan(id);
    let job = wait_for(id)?;
    let record = if job.state == "done" {
        book.read()?
            .into_iter()
            .filter(|r| r.state == "active" && r.created_at >= since && same_text(Path::new(&r.original), &from))
            .max_by_key(|r| r.created_at)
            .map(|r| Relocation { pending: None, ..r })
    } else {
        None
    };
    Ok((job, record))
}

/// For plans: undoes the relocation `id` as `undo_relocate` does and waits
/// for the job. Gives the job as it ended.
pub(crate) fn restore_and_wait(events: Box<dyn Events>, id: &str) -> Result<JobInfo, String> {
    let job = begin_restore(id, events, Settings::APP, Book::app())?;
    for_plan(job);
    wait_for(job)
}

/// For plans: the folders a relocation never moves (`guarded_places`),
/// looked up once for checking many paths.
pub(crate) struct Guards(Vec<(PathBuf, bool, &'static str)>);

impl Guards {
    pub(crate) fn now() -> Guards {
        Guards(guarded_places())
    }

    /// Why the item `path` must stay where it is and as it is, if so: as
    /// written, and where it really is (with the junctions on the way resolved).
    pub(crate) fn why(&self, path: &Path) -> Option<&'static str> {
        guarded_among(path, false, &self.0).or_else(|| real_path(path).and_then(|real| guarded_among(&real, false, &self.0)))
    }

    /// Why nothing may be put into the folder `dir`, if so.
    pub(crate) fn why_into(&self, dir: &Path) -> Option<&'static str> {
        guarded_among(dir, true, &self.0).or_else(|| real_path(dir).and_then(|real| guarded_among(&real, true, &self.0)))
    }
}

// ---- v0.8.2: duplicates made into hard links ------------------------------------
//
// `start_dup_link` (job op "link", the usual "job-progress" events, cancel
// with `cancel_job`) takes, for groups the duplicate finder found (dups.rs),
// the file that stays and the copies to merge into it. Each copy is replaced
// by an NTFS hard link to the file that stays: its path still opens the same
// content, stored once. Sizes, modified times and hashes come from the
// finder's own result, not from the window. For each copy:
//
// 1. look, changing nothing: the link rule (links.rs, v0.10.3) allows both
//    (by where they are and what they are; a big model or archive in a
//    program's folder only with `allow_programs`); both are plain files (no
//    link or other reparse point, nothing only in the cloud) outside
//    Windows's own folders, on one local NTFS volume, of the size and
//    modified time the finder saw, and not one file already (then there is
//    nothing to do); they have the same read-only, hidden and system
//    attributes, owner and permissions (which the copy's name would take on
//    from the file that stays); the copy has no other name (merging it would
//    free nothing), the file that stays has room for one more (NTFS: 1023),
//    and the Recycle Bin takes the copy;
// 2. read both whole (blake3) through handles that let nobody write to them
//    until the copy is merged, and compare them with the finder's hash;
// 3. write down what is about to happen (`LinkBook`), all the way to the
//    disk; rename the copy to a hidden `.name.ff-link` next to it, make the
//    hard link under its name from the handle of the file that stays (so it
//    is the very file that was read, even if another one has been put at its
//    path since), and check that the name leads to it;
// 4. send `.name.ff-link`, with the copy's own attributes back, to the
//    Recycle Bin (the checks of `trash_paths`): its bytes come free once the
//    bin is emptied (`JobInfo.freed_bytes`). Then the note goes, and the
//    merge is written in the ledger (`links::Ledger`), which `link_ledger`
//    and `hardlinks_find` show and `start_unlink` can undo.
//
// Nothing is changed before step 3. A step 3 that fails is undone: the name
// made is taken away again (the one the link call made, whatever it leads
// to, or one of the file that stays; never a file's last name) and the copy
// gets its name and attributes back; when that cannot be done the reason
// says where the copy is. A copy the bin turns down at step 4 stays where it
// is, hidden, and the job's errors say so (LINK_LEFT). A merge cut short in
// steps 3 or 4 (a crash, a power cut, the app killed) is put right at the
// next start from its note (`recover_links`): the copy gets its name back,
// or, when the link was made, its old bytes go to the bin; what is left
// shows in a job of its own. The merged name has the attributes, times and
// permissions of the file that stays: a hard link is one file under two
// names, so a program that writes into it in place changes it under both.
// Cancel works between copies and while reading.

/// The hidden name a merged copy has until it is in the Recycle Bin.
const LINK_EXT: &str = ".ff-link";
/// Copies one merge takes at most.
const MAX_LINKS: usize = 10_000;
/// Names an NTFS file can have.
const MAX_FILE_NAMES: u32 = 1023;
/// Read size of the full hash.
const HASH_BUF: usize = 1 << 20;

const NOTHING_TO_LINK: &str = "합칠 파일이 없어요";
const TOO_MANY_LINKS: &str = "한 번에 합칠 수 있는 파일은 10,000개까지예요";
const NO_DUP_RESULT: &str = "중복 찾기 결과를 찾을 수 없어요. 중복을 다시 찾아 주세요";
const NOT_IN_RESULT: &str = "중복 찾기 결과에 없는 파일이에요";
const NOT_ONE_GROUP: &str = "남길 파일과 같은 묶음(내용이 같은 파일들)에 있는 파일이 아니에요";
const LINK_TWICE: &str = "같은 파일이 두 번 들어 있어요";
/// The start of the reason given to every copy of a group whose file that
/// stays could not be used.
const KEEP_FAILED: &str = "남길 파일";
const LINK_SYSTEM: &str = "Windows 시스템 폴더의 파일은 합치지 않아요";
const LINK_NOT_PLAIN: &str = "링크나 특수한 파일이라서 합치지 않았어요";
const LINK_CLOUD: &str = "온라인 전용 파일이라서 합치지 않았어요 (읽으면 내려받게 돼요)";
const LINK_GONE: &str = "중복을 찾은 뒤 없어져서 합치지 않았어요";
const LINK_CHANGED: &str = "중복을 찾은 뒤 바뀌어서 합치지 않았어요";
const LINK_NO_OPEN: &str = "파일을 열 수 없어서 합치지 않았어요";
const LINK_NO_READ: &str = "파일을 다 읽지 못해서 합치지 않았어요";
const LINK_NOT_NTFS: &str = "NTFS 드라이브가 아니라서 합칠 수 없어요 (하드 링크를 만들 수 없어요)";
const LINK_OTHER_DRIVE: &str = "남길 파일과 다른 드라이브에 있어서 합칠 수 없어요 (하드 링크는 한 드라이브 안에서만 만들 수 있어요)";
const LINK_HAS_NAMES: &str = "이 파일에는 다른 이름(하드 링크)도 있어서 합쳐도 공간이 생기지 않아요";
const LINK_FULL: &str = "남길 파일의 이름(하드 링크)이 1023개로 꽉 차서 더 합칠 수 없어요";
const LINK_DIFFERENT: &str = "내용이 남길 파일과 달라서 합치지 않았어요";
const LINK_ODD_NAME: &str = "이름이 점(.)이나 공백으로 끝나는 폴더에 있어서 합치지 않았어요 (남는 사본을 휴지통으로 옮길 수 없어요)";
const LINK_NO_BIN: &str = "이 드라이브에는 휴지통이 없어서 합치지 않았어요 (합치면 남는 사본을 되돌릴 수 없어요)";
const LINK_BIN_OFF: &str =
    "이 드라이브는 휴지통을 쓰지 않도록 설정돼 있어서 합치지 않았어요 (합치면 남는 사본을 되돌릴 수 없어요)";
const LINK_BIN_SMALL: &str = "휴지통보다 커서 합치지 않았어요. 휴지통 속성에서 최대 크기를 늘리면 합칠 수 있어요";
const LINK_BIN_UNKNOWN: &str = "남는 사본을 휴지통으로 옮길 수 있는지 확인할 수 없어서 합치지 않았어요";
const LINK_NO_RENAME: &str = "이름을 바꾸지 못해서 합치지 않았어요";
const LINK_NOT_MADE: &str = "하드 링크를 만들지 못해서 합치지 않았어요";
const LINK_ELSEWHERE: &str = "만든 이름이 남길 파일로 이어지지 않아요";
/// The start of the reason for a copy that was merged but whose old bytes
/// the Recycle Bin turned down; the path is where they are.
const LINK_LEFT: &str = "합쳤지만 남은 사본";
const LINK_NO_BOOK: &str = "합치는 기록을 남길 수 없어서 합치지 않았어요";
const LINK_NOT_RECORDED: &str = "합치는 기록을 저장하지 못해서 합치지 않았어요";
// What the start after a merge cut short (`recover_links`) did with a copy.
const LINK_CUT_BACK: &str = "지난번에 합치던 중에 fine-file이 꺼져서, 합치지 않고 원래대로 돌려놓았어요";
const LINK_CUT_STUCK: &str = "지난번에 합치던 중에 fine-file이 꺼졌는데, 원래대로 되돌리지 못했어요";
const LINK_CUT_TAKEN: &str = "지난번에 합치던 중에 fine-file이 꺼진 뒤 이 이름에 다른 파일이 생겨서 되돌리지 않았어요";
const LINK_CUT_GONE: &str = "지난번에 합치던 중에 fine-file이 꺼진 뒤 이 파일이 없어졌어요";
const LINK_CUT_CHANGED: &str = "합친 뒤에 바뀌어서 그대로 두었어요";
const LINK_CUT_ODD: &str = "지난번에 합치던 중에 fine-file이 꺼졌는데, 기록에 적힌 위치가 합칠 수 없는 곳이라서 손대지 않았어요";

/// One group of `start_dup_link`, with paths as the duplicate finder gave
/// them (DupFile.path).
#[derive(Deserialize)]
pub struct LinkGroup {
    /// The file that stays as it is.
    pub keep: String,
    /// Its copies, each to become a hard link to `keep`.
    pub others: Vec<String>,
}

/// Makes each copy in `groups` a hard link to the file its group keeps, so
/// that every path still opens the same content, stored once (see above).
/// `dup` is the duplicate search the groups come from (DupReport.id); without
/// it, the newest search that has them. The groups are checked against that
/// search's result before a job (op "link") starts; returns its id. A file
/// the link rule does not allow (DupFile.link_block) is turned down in the
/// job, unless it is one the user may ask for (`link_optin`) and
/// `allow_programs` asks.
#[tauri::command]
pub async fn start_dup_link(app: AppHandle, groups: Vec<LinkGroup>, dup: Option<u64>, allow_programs: Option<bool>) -> Result<u64, String> {
    blocking(move || {
        let found = dups::found(dup).map_err(|_| NO_DUP_RESULT.to_string())?;
        begin_link(groups, &found, app, Settings::APP, &LinkBook::app(), allow_programs.unwrap_or(false))
    })
    .await?
}

/// Checks `groups` against what the duplicate finder `found`, then starts
/// the job, which writes down in `book` what it is doing (and in its ledger
/// what it did).
fn begin_link(
    groups: Vec<LinkGroup>,
    found: &[dups::DupGroup],
    events: impl Events,
    settings: Settings,
    book: &LinkBook,
    allow_programs: bool,
) -> Result<u64, String> {
    let plans = plan_links(groups, found)?;
    let mut page = book.page()?;
    let how = LinkHow { allow_programs, ledger: book.ledger(), markers: links::Markers::default() };
    let all = plans.iter().flat_map(|p| std::iter::once(&p.keep.0).chain(p.others.iter().map(|(o, _)| o)));
    let dest = shown(&common_folder(all));
    let run = new_job("link", false, Vec::new(), dest, events, settings);
    launch(run, move |run| run.link_all(&plans, &mut page, &how))
}

/// What a merge goes by besides its groups.
struct LinkHow {
    /// The user asked for big models and archives in programs' folders too
    /// (`links::Verdict::OptIn`).
    allow_programs: bool,
    /// Where each merge done is written down.
    ledger: links::Ledger,
    /// What the folders hold, for the link rule; looked at once per merge.
    markers: links::Markers,
}

/// One group to merge, as the duplicate finder found it.
struct LinkPlan {
    size: u64,
    /// blake3 of the whole content.
    hash: [u8; 32],
    /// The file that stays, with its modified time (FILETIME) as the finder saw it...
    keep: (PathBuf, i64),
    /// ...and the copies, with theirs.
    others: Vec<(PathBuf, i64)>,
}

/// What the job does for `groups`: each file in them must be in a group the
/// duplicate finder `found` (the first group that has it counts), each group's
/// files in one such group, and no file twice.
fn plan_links(groups: Vec<LinkGroup>, found: &[dups::DupGroup]) -> Result<Vec<LinkPlan>, String> {
    let count: usize = groups.iter().map(|g| g.others.len()).sum();
    if count == 0 {
        return Err(NOTHING_TO_LINK.into());
    }
    if count > MAX_LINKS {
        return Err(format!("{TOO_MANY_LINKS} (고른 파일 {count}개)"));
    }
    let text = |p: &str| p.strip_prefix(r"\\?\").unwrap_or(p).trim_end_matches('\\').to_lowercase();
    let mut files: HashMap<String, (usize, &dups::DupFile)> = HashMap::new();
    for (i, g) in found.iter().enumerate() {
        for f in &g.files {
            files.entry(text(&f.path)).or_insert((i, f));
        }
    }
    let mut seen = HashSet::new();
    let mut look = |p: &str| -> Result<(usize, (PathBuf, i64)), String> {
        let key = text(p);
        let &(group, file) = files.get(&key).ok_or_else(|| format!("{NOT_IN_RESULT}: {p}"))?;
        if !seen.insert(key) {
            return Err(format!("{LINK_TWICE}: {p}"));
        }
        Ok((group, (PathBuf::from(&file.path), file.modified)))
    };
    let mut plans = Vec::new();
    for g in groups.into_iter().filter(|g| !g.others.is_empty()) {
        let (group, keep) = look(&g.keep)?;
        let mut others = Vec::new();
        for other in &g.others {
            let (of, file) = look(other)?;
            if of != group {
                return Err(format!("{NOT_ONE_GROUP}: {other}"));
            }
            others.push(file);
        }
        let hash = blake3::Hash::from_hex(&found[group].hash).map_err(|_| NO_DUP_RESULT.to_string())?;
        plans.push(LinkPlan { size: found[group].size, hash: *hash.as_bytes(), keep, others });
    }
    Ok(plans)
}

/// The deepest folder that holds all of `paths` (a drive's root when they
/// are on several drives): the job's `dest`.
fn common_folder<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> PathBuf {
    let text = |p: &Path| format!("{}\\", p.to_string_lossy().trim_end_matches('\\').to_lowercase());
    let mut common: Option<PathBuf> = None;
    for path in paths {
        let Some(parent) = path.parent() else { continue };
        let mut c = common.take().unwrap_or_else(|| parent.to_path_buf());
        while !text(parent).starts_with(&text(&c)) && c.pop() {}
        common = Some(c);
    }
    common.unwrap_or_default()
}

/// A file of a merge, open for reading so that nobody can write to it (no
/// FILE_SHARE_WRITE) until it is let go of: what was read is what gets
/// merged. Renaming it and making links to it still work.
struct Held {
    file: File,
    attrs: u32,
    key: dups::FileKey,
    /// What its names share (`links::sharing_of`); None when that cannot be read.
    sharing: Option<links::Sharing>,
    /// Names (hard links) it had when it was opened.
    links: u32,
    /// Its size and modified time (FILETIME), as the duplicate finder saw them.
    size: u64,
    modified: i64,
}

/// Opens the file at `path` for a merge, if it is still the plain file of
/// `size` bytes and `modified` time the duplicate finder saw, on a local
/// NTFS volume, outside Windows's own folders (`windows`: where Windows is).
/// A copy (`copy`) is opened so that it could be renamed too: one a program
/// holds open without letting it be renamed is turned down before it is read.
fn open_for_link(path: &Path, size: u64, modified: i64, windows: Option<&Path>, copy: bool) -> Result<Held, String> {
    if in_windows(path, windows) {
        return Err(LINK_SYSTEM.into());
    }
    let exact = win::verbatim(path);
    let lost = |e: io::Error| match e.kind() {
        io::ErrorKind::NotFound => LINK_GONE.to_string(),
        _ => format!("{LINK_NO_OPEN}: {}", reason(&e)),
    };
    // The attributes first, through a handle that cannot read: an online-only
    // file is never opened for reading.
    let meta = std::fs::symlink_metadata(&exact).map_err(lost)?;
    judge_link(meta.file_attributes(), meta.file_size(), meta.last_write_time() as i64, size, modified)?;
    let file = OpenOptions::new()
        .access_mode(if copy { GENERIC_READ | DELETE } else { GENERIC_READ })
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(&exact)
        .map_err(lost)?;
    // Again from the handle: it may have changed in between.
    let facts = dups::facts(&file).ok_or(LINK_NO_OPEN)?;
    judge_link(facts.attrs, facts.size, facts.modified, size, modified)?;
    let key = facts.key.filter(|_| on_ntfs(&file)).ok_or(LINK_NOT_NTFS)?;
    let sharing = links::sharing_of(&file);
    Ok(Held { file, attrs: facts.attrs, key, sharing, links: facts.links, size, modified })
}

/// Whether a file with these attributes, size and modified time is still the
/// plain file of `size` bytes and `modified` time the duplicate finder saw.
fn judge_link(attrs: u32, now_size: u64, now_modified: i64, size: u64, modified: i64) -> Result<(), String> {
    if dups::online_only(attrs) {
        return Err(LINK_CLOUD.into());
    }
    if attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err(LINK_NOT_PLAIN.into());
    }
    if (now_size, now_modified) != (size, modified) {
        return Err(LINK_CHANGED.into());
    }
    Ok(())
}

/// Whether `path` is in a folder of Windows itself, as written or where it
/// really is: `classify`'s "system" (`\Windows`, `\Recovery`, `\$Recycle.Bin`
/// and the like at a drive's root) or the Windows folder wherever it is.
pub(crate) fn in_windows(path: &Path, windows: Option<&Path>) -> bool {
    let system = |p: &Path| {
        let text = p.to_string_lossy();
        let name = text.rsplit('\\').next().unwrap_or_default();
        crate::classify::program_category(&text, name, false) == Some("system") || windows.is_some_and(|w| inside(p, w))
    };
    system(path) || real_path(path).is_some_and(|real| system(&real))
}

/// Whether the open `file` is on an NTFS volume, which can give it more names.
fn on_ntfs(file: &File) -> bool {
    let mut fs = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle() as HANDLE,
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    } != 0;
    let len = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
    ok && String::from_utf16_lossy(&fs[..len]).eq_ignore_ascii_case("NTFS")
}

/// Which file `path` names (`dups::FileKey`); a link there is taken as itself.
pub(crate) fn key_at(path: &Path) -> Option<dups::FileKey> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(win::verbatim(path))
        .ok()?;
    dups::facts(&file)?.key
}

/// Gives the file at `path` the attributes `attrs` (those SetFileAttributes changes).
fn put_attrs(path: &Path, attrs: u32) {
    let attrs = attrs & SETTABLE;
    unsafe { SetFileAttributesW(win::wide_path(path).as_ptr(), if attrs == 0 { FILE_ATTRIBUTE_NORMAL } else { attrs }) };
}

/// FILE_LINK_INFORMATION (ntifs.h); the name runs on past the struct.
#[repr(C)]
struct LinkInformation {
    /// ReplaceIfExists (a BOOLEAN, in newer headers the ULONG `Flags`): never.
    replace: u32,
    /// None: `name` is a whole NT path.
    root: HANDLE,
    /// In bytes, without a nul.
    name_length: u32,
    name: [u16; 1],
}

/// FileLinkInformation (FILE_INFORMATION_CLASS, wdm.h).
const FILE_LINK_INFORMATION: i32 = 11;
/// FileRenameInformation: its FILE_RENAME_INFORMATION is laid out as LinkInformation.
const FILE_RENAME_INFORMATION: i32 = 10;

// What CreateHardLinkW does underneath, but on a handle: one feature of
// windows-sys (Wdk) for one function is not worth it.
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtSetInformationFile(file: HANDLE, io: *mut IO_STATUS_BLOCK, info: *const c_void, length: u32, class: i32) -> NTSTATUS;
    /// Opens a file by name relative to `attrs.root` (`open_child`): no path is
    /// walked, so a folder on the way cannot be swapped for a mount point.
    #[allow(clippy::too_many_arguments)]
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        attrs: *const ObjectAttributes,
        io: *mut IO_STATUS_BLOCK,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> NTSTATUS;
}

/// Makes `link` a new name of the open file `existing`, never over anything.
/// Made from the handle, not from a path: the name leads to the very file
/// that was opened (and read), even if another one has been put at its
/// path since.
fn create_hard_link(link: &Path, existing: &File) -> io::Result<()> {
    let name = nt_name(link)?;
    // Through a handle of its own on that very file, closed right after: the
    // file's change time and archive mark are set then, as by CreateHardLinkW
    // (else only once `existing` is closed, past an undo that puts them back).
    // With the right to change its attributes where that is given (Windows
    // asks it of sandboxed programs), else without.
    let all = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let reopen = |access: u32| unsafe { ReOpenFile(existing.as_raw_handle() as HANDLE, access | SYNCHRONIZE, all, 0) };
    let mut h = reopen(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES);
    if h == INVALID_HANDLE_VALUE {
        h = reopen(FILE_READ_ATTRIBUTES);
    }
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let same = unsafe { File::from_raw_handle(h as RawHandle) };
    set_name(&same, &name, FILE_LINK_INFORMATION)
}

/// Moves the entry open as `file` (with DELETE) to `target`, on its volume,
/// never over anything: as `rename_entry` does, but the very entry opened
/// (in its folder, `Within`), not whatever a path to it leads to by now.
fn rename_open(file: &File, target: &Path) -> io::Result<()> {
    set_name(file, &nt_name(target)?, FILE_RENAME_INFORMATION)
}

/// `path` as the kernel names it: `\\?\C:\x` is `\??\C:\x` (`\\?\UNC\...` likewise).
fn nt_name(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide = win::wide_path(path);
    wide.pop();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let Some(rest) = wide.strip_prefix(verbatim.as_slice()) else {
        return Err(io::Error::from_raw_os_error(ERROR_INVALID_NAME as i32));
    };
    Ok(r"\??\".encode_utf16().chain(rest.iter().copied()).collect())
}

/// Gives the open `file` the name `name` (`nt_name`), never over anything: a
/// new one (FILE_LINK_INFORMATION) or in place of its own
/// (FILE_RENAME_INFORMATION, laid out the same).
fn set_name(file: &File, name: &[u16], class: i32) -> io::Result<()> {
    let head = std::mem::offset_of!(LinkInformation, name);
    let size = head + name.len() * 2;
    // Zeroed (no replacing, no root folder) and aligned for the struct.
    let mut buf = vec![0u64; size.max(std::mem::size_of::<LinkInformation>()).div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<LinkInformation>();
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let status = unsafe {
        (*info).name_length = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), buf.as_mut_ptr().cast::<u8>().add(head).cast::<u16>(), name.len());
        NtSetInformationFile(file.as_raw_handle() as HANDLE, &mut status_block, buf.as_ptr().cast(), size as u32, class)
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32));
    }
    Ok(())
}

/// Takes the name `path` away from a file that has another name too: never
/// a file's last name, nor a link or a folder. The name goes as its handle
/// closes, read-only or not, and also while the file is open under another
/// name (POSIX semantics); a Windows older than 10 (1809) knows only the old
/// way, which turns a read-only file down. Whether the name went.
fn unlink_extra(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(win::verbatim(path))
    else {
        return false;
    };
    let plain = |attrs: u32| attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) == 0;
    if !dups::facts(&file).is_some_and(|f| f.links >= 2 && plain(f.attrs)) {
        return false;
    }
    let ex = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    let (h, size) = (file.as_raw_handle() as HANDLE, std::mem::size_of_val(&ex) as u32);
    let marked = unsafe { SetFileInformationByHandle(h, FileDispositionInfoEx, (&raw const ex).cast(), size) != 0 } || delete_on_close(&file);
    drop(file);
    marked && gone(path)
}

/// Writes down in `ledger` that `linked` became a name of the file at
/// `kept`. A ledger that cannot be written changes nothing of the merge,
/// which is done: it only shows less of what was merged.
fn record_merge(ledger: &links::Ledger, kept: &Path, linked: &Path, size: u64) {
    let _ = ledger.add(&links::Merge { kept: shown(kept), linked: shown(linked), size, at: filetime_now() });
}

/// Why a merge does not start on a copy the Recycle Bin would not take.
fn link_bin_reason(why: Binless) -> String {
    match why {
        Binless::OddName => LINK_ODD_NAME.into(),
        Binless::NoBin => LINK_NO_BIN.into(),
        Binless::Disabled => LINK_BIN_OFF.into(),
        Binless::TooBig(max) => format!("{LINK_BIN_SMALL} (휴지통 최대 {}MB)", max >> 20),
        Binless::Unknown => LINK_BIN_UNKNOWN.into(),
    }
}

/// Undoes a switch that went wrong: takes away the name made at `path` and
/// gives the copy at `temp` its name back, with its own attributes `attrs`
/// when they were changed. The name there goes when the link call said it
/// made it (`made`: our rename had just freed the name, so the link is ours,
/// whatever file it turned out to lead to) or when it leads to the file that
/// stays (`kept`; a call that failed may have made it all the same), and
/// only while its file has another name (`unlink_extra`). The file that
/// stays (at `keep`, while it is still there) gets its attributes back, as a
/// name made and taken away marks it for backup (archive). Gives `why`, and
/// where the copy is when it cannot go back.
fn link_undo(kept: &Held, keep: &Path, path: &Path, temp: &Path, made: bool, attrs: Option<u32>, why: String) -> String {
    if (made || key_at(path) == Some(kept.key)) && unlink_extra(path) && key_at(keep) == Some(kept.key) {
        put_attrs(keep, kept.attrs);
    }
    let back = win::rename(temp, path);
    if let Some(attrs) = attrs {
        put_attrs(if back.is_ok() { path } else { temp }, attrs);
    }
    match back {
        Ok(()) => why,
        Err(_) => format!("{why}. {STUCK}. 파일은 지금 {}에 있어요", shown(temp)),
    }
}

/// How a copy came out of a merge.
enum Merged {
    /// It is a name of the file that stays now, and its old bytes are in the Recycle Bin.
    Binned,
    /// It is a name of the file that stays now, but the bin turned its old
    /// bytes down: they are still at this hidden name, and why.
    Left(PathBuf, String),
    /// It was one file with the one that stays already.
    Same,
}

impl Run {
    /// A merge: each group in turn, each switch written down on `page` first.
    fn link_all(&mut self, plans: &[LinkPlan], page: &mut Page, how: &LinkHow) -> Result<(), Stop> {
        // Every copy is read whole, and once per group the file that stays.
        for p in plans {
            self.info.total_items += p.others.len() as u64;
            self.info.total_bytes += p.size.saturating_mul(p.others.len() as u64 + 1);
        }
        self.publish();
        self.speed = Speed::new();
        let windows = known_folder(&FOLDERID_Windows);
        let mut buf = vec![0; HASH_BUF];
        for plan in plans {
            self.check()?;
            self.link_group(plan, windows.as_deref(), &mut buf, page, how)?;
        }
        Ok(())
    }

    /// Merges the copies of one group into the file it keeps, which is read
    /// once for all of them and held meanwhile, so that nobody writes to it.
    fn link_group(&mut self, plan: &LinkPlan, windows: Option<&Path>, buf: &mut [u8], page: &mut Page, how: &LinkHow) -> Result<(), Stop> {
        let (keep, modified) = (&plan.keep.0, plan.keep.1);
        let base = self.info.done_bytes;
        self.info.current = Some(keep.to_string_lossy().into_owned());
        self.tick();
        // The file that stays must be one the rule allows too: what it is becomes what all of them are.
        let refused = (self.settings.rule)(keep, plan.size, &how.markers).refusal(how.allow_programs);
        let opened = match refused {
            Some(why) => Err(why.to_string()),
            None => open_for_link(keep, plan.size, modified, windows, false),
        };
        let kept = match opened {
            Ok(mut held) => match self.hash_whole(&mut held.file, plan.size, buf) {
                Ok(hash) if hash == plan.hash => Ok(held),
                Ok(_) => Err(Halt::Why(LINK_CHANGED.into())),
                Err(halt) => Err(halt),
            },
            Err(why) => Err(Halt::Why(why)),
        };
        let kept = match kept {
            Ok(kept) => kept,
            Err(Halt::Stop) => return Err(Stop),
            Err(Halt::Why(why)) => {
                let name = keep.file_name().map_or_else(|| shown(keep), |n| n.to_string_lossy().into_owned());
                let why = format!("{KEEP_FAILED}({name}) — {why}");
                for (other, _) in &plan.others {
                    self.error(other, why.clone());
                }
                let n = plan.others.len() as u64;
                self.info.done_bytes = base + plan.size.saturating_mul(n + 1);
                self.info.done_items += n;
                self.tick();
                return Ok(());
            }
        };
        for other in &plan.others {
            self.check()?;
            let start = self.info.done_bytes;
            self.info.current = Some(other.0.to_string_lossy().into_owned());
            self.tick();
            match self.link_one(&kept, plan, other, windows, buf, page, how) {
                Ok(Merged::Binned) => {
                    self.handled += 1;
                    self.info.freed_bytes += plan.size;
                    record_merge(&how.ledger, keep, &other.0, plan.size);
                }
                Ok(Merged::Left(temp, why)) => {
                    self.handled += 1;
                    let name = temp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.error(&temp, format!("{LINK_LEFT}({name})을 휴지통에 넣지 못했어요: {why}"));
                    record_merge(&how.ledger, keep, &other.0, plan.size);
                }
                Ok(Merged::Same) => self.handled += 1,
                Err(Halt::Why(why)) => self.error(&other.0, why),
                Err(Halt::Stop) => return Err(Stop),
            }
            self.info.done_bytes = start + plan.size;
            self.info.done_items += 1;
            self.tick();
        }
        Ok(())
    }

    /// Merges the copy at `path` into the file that stays (`kept`): steps 1
    /// to 4 above. Nothing is changed before the copy has been read whole.
    #[allow(clippy::too_many_arguments)]
    fn link_one(
        &mut self,
        kept: &Held,
        plan: &LinkPlan,
        (path, modified): &(PathBuf, i64),
        windows: Option<&Path>,
        buf: &mut [u8],
        page: &mut Page,
        how: &LinkHow,
    ) -> Result<Merged, Halt> {
        // Merged before: then it has the times of the file that stays, not its own.
        if key_at(path) == Some(kept.key) {
            return Ok(Merged::Same);
        }
        let verdict = (self.settings.rule)(path, plan.size, &how.markers);
        if let Some(why) = verdict.refusal(how.allow_programs) {
            return Err(Halt::Why(why.into()));
        }
        let mut copy = open_for_link(path, plan.size, *modified, windows, true).map_err(Halt::Why)?;
        if copy.key == kept.key {
            return Ok(Merged::Same);
        }
        // The volume serials; tests take any two files for two volumes.
        if !self.settings.rename || copy.key.0 != kept.key.0 {
            return Err(Halt::Why(LINK_OTHER_DRIVE.into()));
        }
        // Its name would take on the attributes, owner and permissions of the file that stays.
        let (Some(k), Some(c)) = (&kept.sharing, &copy.sharing) else { return Err(Halt::Why(links::LB_NO_SECURITY.into())) };
        if let Some(why) = links::pair_refusal(k, c) {
            // A model in a program's folder, asked for: asking does not help when the permissions differ.
            let mine = match verdict {
                links::Verdict::OptIn(_) => verdict,
                _ => (self.settings.rule)(&plan.keep.0, plan.size, &how.markers),
            };
            return Err(Halt::Why(links::security_why(why, mine).into()));
        }
        if copy.links > 1 {
            return Err(Halt::Why(LINK_HAS_NAMES.into()));
        }
        match dups::facts(&kept.file) {
            Some(f) if f.links < MAX_FILE_NAMES => {}
            Some(_) => return Err(Halt::Why(LINK_FULL.into())),
            None => return Err(Halt::Why(format!("{KEEP_FAILED} — {LINK_NO_OPEN}"))),
        }
        let temp = marked_name(path, true, LINK_EXT, 1).ok_or_else(|| Halt::Why(NO_FREE_NAME.into()))?;
        if let Some(why) = binless(&temp, || plan.size) {
            return Err(Halt::Why(link_bin_reason(why)));
        }
        if self.hash_whole(&mut copy.file, plan.size, buf)? != plan.hash {
            return Err(Halt::Why(LINK_DIFFERENT.into()));
        }
        // Nobody could write to either since they were opened.
        let merged = self.switch_to_link(kept, copy, path, &plan.keep.0, page);
        // Done, undone, or (when it could not be undone) told where the copy
        // is: nothing is left for the next start to put right.
        page.clear();
        merged.map_err(Halt::Why)
    }

    /// Steps 3 and 4: what is about to happen written down on `page`, the
    /// copy `copy` at `path` out of the way, a hard link to the file that
    /// stays (`kept` at `keep`) in its place, and the copy to the Recycle Bin.
    fn switch_to_link(&self, kept: &Held, copy: Held, path: &Path, keep: &Path, page: &mut Page) -> Result<Merged, String> {
        let attrs = copy.attrs;
        let mut temp = None;
        for n in 1..=PARTIAL_TRIES {
            let Some(t) = marked_name(path, true, LINK_EXT, n) else { break };
            let note = Switch {
                path: path.to_path_buf(),
                temp: t.clone(),
                keep: keep.to_path_buf(),
                kept: kept.key,
                copy: copy.key,
                attrs,
                size: copy.size,
                modified: copy.modified,
            };
            page.write(&note).map_err(|e| format!("{LINK_NOT_RECORDED}: {}", reason(&e)))?;
            match win::rename(path, &t) {
                Ok(()) => {
                    temp = Some(t);
                    break;
                }
                // One that is there already (another merge's, or one the bin
                // turned down and the user was told of) is not ours to touch.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("{LINK_NO_RENAME}: {}", reason(&e))),
            }
        }
        let temp = temp.ok_or(NO_FREE_NAME)?;
        // The very file that was read, not one put at its name since.
        if key_at(&temp) != Some(copy.key) {
            return Err(link_undo(kept, keep, path, &temp, false, None, LINK_CHANGED.into()));
        }
        put_attrs(&temp, attrs | FILE_ATTRIBUTE_HIDDEN);
        // From the handle held since the file that stays was read: the link
        // leads to that file, whatever is at its path by now.
        let mut made = false;
        let linked = (self.settings.hard_link)(path, &kept.file).and_then(|()| {
            made = true;
            if key_at(path) == Some(kept.key) { Ok(()) } else { Err(io::Error::other(LINK_ELSEWHERE)) }
        });
        if let Err(e) = linked {
            return Err(link_undo(kept, keep, path, &temp, made, Some(attrs), format!("{LINK_NOT_MADE}: {}", reason(&e))));
        }
        // Let go of, so that it can go; with its own attributes, as it was.
        drop(copy);
        put_attrs(&temp, attrs);
        match (self.settings.bin)(&temp) {
            Ok(()) => Ok(Merged::Binned),
            Err(why) => {
                put_attrs(&temp, attrs | FILE_ATTRIBUTE_HIDDEN);
                Ok(Merged::Left(temp, why))
            }
        }
    }

    /// blake3 of all `size` bytes of `file`, read `buf` at a time into the
    /// job's bytes done.
    fn hash_whole(&mut self, file: &mut File, size: u64, buf: &mut [u8]) -> Result<[u8; 32], Halt> {
        self.hash_whole_as(file, size, buf, (LINK_CHANGED, LINK_NO_READ))
    }

    /// `hash_whole`, telling a file shorter than `size` (it changed) and one
    /// that cannot be read by these reasons.
    fn hash_whole_as(&mut self, file: &mut File, size: u64, buf: &mut [u8], (changed, unread): (&str, &str)) -> Result<[u8; 32], Halt> {
        let failed = |e: io::Error| {
            // Shorter than it said: it changed meanwhile.
            Halt::Why(if e.kind() == io::ErrorKind::UnexpectedEof { changed.into() } else { format!("{unread}: {}", reason(&e)) })
        };
        file.seek(SeekFrom::Start(0)).map_err(failed)?;
        let mut hasher = blake3::Hasher::new();
        let mut left = size;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..n]).map_err(failed)?;
            hasher.update(&buf[..n]);
            left -= n as u64;
            self.info.done_bytes += n as u64;
            self.tick();
            self.check()?;
        }
        Ok(*hasher.finalize().as_bytes())
    }

    /// The job `recover_links` shows: each switch it put right, told as the
    /// merge itself would have told it.
    fn tell_mended(&mut self, mended: Vec<(Switch, Mend)>) {
        self.info.total_items = mended.len() as u64;
        for (s, mend) in mended {
            match mend {
                Mend::Nothing => {}
                Mend::Finished => {
                    self.handled += 1;
                    self.info.freed_bytes += s.size;
                }
                Mend::Left(why) => {
                    self.handled += 1;
                    self.error(&s.temp, why);
                }
                Mend::Back => self.error(&s.path, LINK_CUT_BACK),
                Mend::Stuck(why) | Mend::Lost(why) => self.error(&s.path, why),
            }
            self.info.done_items += 1;
        }
    }
}

// ---- v0.8.2: merges cut short --------------------------------------------------
//
// Each merge has a page of its own in the `LinkBook`, which holds the switch
// it is making (steps 3 and 4 above) from just before the copy is renamed
// until the copy's old bytes are in the bin, or the switch is undone, or the
// job has told where the copy is. The page goes when the job is over. One
// left behind by a merge cut short (a crash, a power cut, the app killed,
// the update installer ending another of the app's processes) is read at the
// next start (`recover_links`), and the switch on it put right as the merge
// would have gone on (`mend_link`).

/// The folder in the app's data folder that holds the pages.
const LINK_BOOK_DIR: &str = "link-merges";

/// Where merges write down the switch they are making: a folder (tests keep
/// their own) with a page (`<pid>-<time>-<n>.json`) per merge running. A
/// page is held open for nobody else while its merge runs, so the start of
/// another of the app's processes leaves it alone, and an update asks first
/// (`linking_elsewhere`).
struct LinkBook(PathBuf);

impl LinkBook {
    fn app() -> LinkBook {
        LinkBook(crate::scan::data_dir().join(LINK_BOOK_DIR))
    }

    /// A new page, for one merge.
    fn page(&self) -> Result<Page, String> {
        let failed = |e: io::Error| format!("{LINK_NO_BOOK}: {}", reason(&e));
        std::fs::create_dir_all(&self.0).map_err(failed)?;
        let stamp = filetime_now();
        for n in 1..=PARTIAL_TRIES {
            let path = self.0.join(format!("{}-{stamp:016x}-{n}.json", std::process::id()));
            match open_page(&path, true) {
                Ok(file) => return Ok(Page { file, pending: false }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(failed(e)),
            }
        }
        Err(format!("{LINK_NO_BOOK}: {NO_FREE_NAME}"))
    }

    /// Every page in it, held or not.
    fn pages(&self) -> Vec<PathBuf> {
        let Ok(read) = std::fs::read_dir(&self.0) else { return Vec::new() };
        read.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect()
    }

    /// Where the merges done are written down: next to the book.
    fn ledger(&self) -> links::Ledger {
        links::Ledger::beside(&self.0)
    }
}

/// Opens the page at `path` for nobody else: a new one (`new`) to write, or
/// one left behind to put right what it says.
fn open_page(path: &Path, new: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(new)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(0)
        .open(path)
}

/// Marks the open `file` (opened with DELETE) to go once it is closed.
fn delete_on_close(file: &File) -> bool {
    let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    let size = std::mem::size_of_val(&info) as u32;
    unsafe { SetFileInformationByHandle(file.as_raw_handle() as HANDLE, FileDispositionInfo, (&raw const info).cast(), size) != 0 }
}

/// A merge's page: at most one switch on it at a time.
struct Page {
    file: File,
    /// What is on it may need putting right.
    pending: bool,
}

impl Page {
    /// Writes `switch` down, all the way to the disk, before any of it is
    /// done. A page cut short while being written is one whose switch had
    /// not started.
    fn write(&mut self, switch: &Switch) -> io::Result<()> {
        let bytes = serde_json::to_vec(switch).map_err(io::Error::other)?;
        self.pending = true;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()
    }

    /// Nothing on it needs putting right any more.
    fn clear(&mut self) {
        if self.file.set_len(0).is_ok() {
            self.pending = false;
        }
    }
}

impl Drop for Page {
    /// A page with nothing to put right goes. One that still has (its job
    /// stopped on a fault of its own in the middle of a switch) stays for
    /// the next start.
    fn drop(&mut self) {
        if !self.pending {
            delete_on_close(&self.file);
        }
    }
}

/// One switch of a merge: the copy at `path` renamed to `temp`, and `path`
/// made a name of the file that stays.
#[derive(Serialize, Deserialize)]
struct Switch {
    path: PathBuf,
    temp: PathBuf,
    /// The file that stays, by its path and which file it is.
    keep: PathBuf,
    kept: dups::FileKey,
    /// Which file the copy is, and its own attributes, size and modified time.
    copy: dups::FileKey,
    attrs: u32,
    size: u64,
    modified: i64,
}

/// What the start made of a switch cut short.
enum Mend {
    /// Nothing was left to do.
    Nothing,
    /// The link was made: the copy's old bytes went to the bin now.
    Finished,
    /// The link was made, but the copy's old bytes stay, hidden: why (LINK_LEFT).
    Left(String),
    /// The copy has its name back, not merged.
    Back,
    /// Could not be put right now: why. The page stays, for the next start.
    Stuck(String),
    /// Nothing of ours to put right any more, but something to tell: why.
    Lost(String),
}

/// Puts right what merges cut short left (`mend_link`), and shows what was
/// done and what is left for the user in a job of its own (op "link"). Run
/// once at start, on a thread of its own. The pages of merges still running,
/// in this process or another of the app's, are left alone.
pub fn recover_links(app: AppHandle) {
    recover_links_in(&LinkBook::app(), win::is_elevated(), to_bin, app);
}

/// `recover_links` with the book `book` and the bin `bin` (tests keep their
/// own). Elevated, nothing is put right (ELEVATED_JOURNALS): the pages stay
/// for a start with normal rights, and a job that fails (op "link") tells
/// so, by each copy left as it is; its id, if there is one.
fn recover_links_in(book: &LinkBook, elevated: bool, bin: fn(&Path) -> Result<(), String>, events: impl Events) -> Option<u64> {
    if !elevated {
        return mend_links(book, bin, events);
    }
    let left = book
        .pages()
        .iter()
        .filter_map(|path| open_page(path, false).ok().and_then(|mut file| read_switch(&mut file)))
        .map(|s| s.path)
        .collect();
    tell_left_alone("link", left, events)
}

/// The switch on an open page; None for an empty page, and for one that
/// cannot be read: cut short while being written, before anything of its
/// switch.
fn read_switch(file: &mut File) -> Option<Switch> {
    let mut text = Vec::new();
    file.read_to_end(&mut text).ok()?;
    serde_json::from_slice::<Switch>(&text).ok()
}

/// Whether another of the app's processes is merging now (holds a page).
/// Asked before an update, whose installer would end it in the middle of a
/// switch.
pub(crate) fn linking_elsewhere() -> bool {
    linking_in(&LinkBook::app())
}

fn linking_in(book: &LinkBook) -> bool {
    // This process's own pages are held too.
    let own = format!("{}-", std::process::id());
    book.pages()
        .iter()
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&own)))
        .any(|p| held_elsewhere(p))
}

/// `recover_links` with the book `book` and the bin `bin` (tests keep their
/// own): the id of the job that tells what was done, if anything was.
fn mend_links(book: &LinkBook, bin: fn(&Path) -> Result<(), String>, events: impl Events) -> Option<u64> {
    let mut mended = Vec::new();
    let windows = known_folder(&FOLDERID_Windows);
    for path in book.pages() {
        // Held: its merge runs.
        let Ok(mut file) = open_page(&path, false) else { continue };
        let mend = read_switch(&mut file).map(|s| {
            // The page is in the app's data folder, which any program of this account can write.
            let mend = if switch_holds(&s, windows.as_deref()) { mend_link(&s, bin) } else { Mend::Lost(LINK_CUT_ODD.into()) };
            // Merged after all: written down as the merge would have.
            if matches!(mend, Mend::Finished | Mend::Left(_)) {
                record_merge(&book.ledger(), &s.keep, &s.path, s.size);
            }
            (s, mend)
        });
        if !matches!(mend, Some((_, Mend::Stuck(_)))) {
            delete_on_close(&file);
        }
        drop(file);
        mended.extend(mend.filter(|(_, m)| !matches!(m, Mend::Nothing)));
    }
    if mended.is_empty() {
        return None;
    }
    let dest = shown(&common_folder(mended.iter().map(|(s, _)| &s.path)));
    let run = new_job("link", false, Vec::new(), dest, events, Settings::APP);
    launch(run, move |run| {
        run.tell_mended(mended);
        Ok(())
    })
    .ok()
}

/// Puts right one switch cut short, as the merge would have gone on:
/// - the link made (cut before the bin): the copy's old bytes go to the bin
///   (`bin_leftover`);
/// - the copy's own name free (cut between the rename and the link, or while
///   undoing): the copy gets its name back, with its own attributes;
/// - its own name taken by another file meanwhile: both stay as they are,
///   and the user is told where the copy is.
///
/// Only the copy itself is touched: a file at its hidden name that is not
/// the copy is not ours, and nothing is done to a name that leads to neither.
fn mend_link(s: &Switch, bin: fn(&Path) -> Result<(), String>) -> Mend {
    let stuck = |why: String| Mend::Stuck(format!("{LINK_CUT_STUCK}: {why}. 파일은 지금 {}에 있어요", shown(&s.temp)));
    match key_at(&s.temp) {
        Some(k) if k == s.copy => {}
        None if !gone(&s.temp) => return stuck(LINK_NO_OPEN.into()),
        // It never got its hidden name, or it went on from there (to the bin
        // or back) before the cut. Its name gone all the same is someone else's doing.
        _ if gone(&s.path) => return Mend::Lost(format!("{LINK_CUT_GONE}. 같은 내용은 {}에 있어요", shown(&s.keep))),
        _ => return Mend::Nothing,
    }
    match key_at(&s.path) {
        Some(k) if k == s.kept => bin_leftover(s, bin),
        None if gone(&s.path) => match win::rename(&s.temp, &s.path) {
            Ok(()) => {
                put_attrs(&s.path, s.attrs);
                Mend::Back
            }
            Err(e) => stuck(reason(&e)),
        },
        None => stuck(LINK_NO_OPEN.into()),
        Some(_) => Mend::Lost(format!("{LINK_CUT_TAKEN}. 원래 파일은 지금 {}에 있어요", shown(&s.temp))),
    }
}

/// Whether a page says only what a merge of this app would have written: a
/// copy and a file that stays outside Windows's own folders (as
/// `open_for_link` takes them), and the copy's hidden name next to it as
/// `switch_to_link` gives it.
fn switch_holds(s: &Switch, windows: Option<&Path>) -> bool {
    s.path.is_absolute()
        && s.keep.is_absolute()
        && !in_windows(&s.path, windows)
        && !in_windows(&s.keep, windows)
        && marked_as(&s.temp, &s.path, true, LINK_EXT)
}

/// The old bytes of a copy merged before the cut, to the Recycle Bin: only
/// while they are what was merged (size and modified time), and only where
/// the bin takes them rather than deleting them for good (`binless`).
fn bin_leftover(s: &Switch, bin: fn(&Path) -> Result<(), String>) -> Mend {
    let name = s.temp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let left = |why: &str| Mend::Left(format!("{LINK_LEFT}({name})을 휴지통에 넣지 못했어요: {why}"));
    let stamp = |m: Metadata| (m.file_size(), m.last_write_time() as i64);
    if std::fs::symlink_metadata(win::verbatim(&s.temp)).map(stamp).ok() != Some((s.size, s.modified)) {
        return left(LINK_CUT_CHANGED);
    }
    if let Some(why) = binless(&s.temp, || s.size) {
        return left(&bin_refusal(why));
    }
    put_attrs(&s.temp, s.attrs);
    match bin(&s.temp) {
        Ok(()) => Mend::Finished,
        Err(why) => {
            put_attrs(&s.temp, s.attrs | FILE_ATTRIBUTE_HIDDEN);
            left(&why)
        }
    }
}

/// Why the bin would not take a copy's old bytes, short.
fn bin_refusal(why: Binless) -> String {
    match why {
        Binless::OddName => "이름이 점(.)이나 공백으로 끝나는 폴더에 있어요".into(),
        Binless::NoBin => "이 드라이브에는 휴지통이 없어요".into(),
        Binless::Disabled => "이 드라이브는 휴지통을 쓰지 않도록 설정돼 있어요".into(),
        Binless::TooBig(max) => format!("휴지통보다 커요 (휴지통 최대 {}MB)", max >> 20),
        Binless::Unknown => "휴지통으로 옮길 수 있는지 확인할 수 없어요".into(),
    }
}

// ---- v0.10.3: names of a hard-linked file made files of their own ---------------
//
// `start_unlink` (job op "unlink", the usual "job-progress" events, cancel
// with `cancel_job`) takes names of files that have more than one (hard
// links; `links::hardlinks_find` lists them) and makes each a file of its
// own with the same content, times and attributes, so that a program that
// writes to it no longer changes the others. For each name:
//
// 1. look, changing nothing: a plain file (no link or other reparse point,
//    nothing only in the cloud) on a local fixed NTFS volume, with another
//    name besides this one, none of its names in Windows's own folders
//    (`links::protected`), and room on the drive for one more copy and
//    SPACE_MARGIN;
// 2. open the file through another of its names, so that nobody can write
//    to it until this name is done, and copy it beside the name to a hidden
//    `.name.ff-unlink` (CopyFileEx: data, alternate streams, attributes;
//    never onto anything);
// 3. read both whole (blake3) and compare them; give the copy the file's
//    times, and its permissions where they are its own; write it all the
//    way to the disk (the rename below is written through, the data not);
// 4. right after checking the name still leads to the file, put the copy in
//    its place in one step (MoveFileEx, replacing it, written through).
//
// Nothing is deleted and nothing goes to the Recycle Bin: what the name held
// is still there under the file's other names. A copy whose name fails
// anywhere before the end of step 4 is removed again (it is our own), and
// the name is as it was. Merges in the ledger that are undone so are taken
// out of it. Cancel works between names and while copying or reading.

/// The hidden name a name's own copy has until it takes the name's place.
const UNLINK_EXT: &str = ".ff-unlink";

const NOTHING_TO_UNLINK: &str = "풀 파일이 없어요";
const TOO_MANY_UNLINKS: &str = "한 번에 풀 수 있는 파일은 10,000개까지예요";
const UNLINK_SYSTEM: &str = "Windows 시스템 폴더의 파일은 풀지 않아요";
const UNLINK_NOT_LOCAL: &str = "이 컴퓨터에 붙은 NTFS 드라이브의 파일만 풀 수 있어요 (USB 메모리, 네트워크 드라이브 말고)";
const UNLINK_NOT_PLAIN: &str = "링크나 특수한 파일이라서 풀지 않았어요";
const UNLINK_CLOUD: &str = "온라인 전용 파일이라서 풀지 않았어요 (읽으면 내려받게 돼요)";
const UNLINK_GONE: &str = "파일을 찾을 수 없어요";
const UNLINK_LAST: &str = "다른 이름(하드 링크)이 없는 파일이라 풀 것이 없어요";
const UNLINK_NO_OPEN: &str = "파일을 열 수 없어서 풀지 않았어요";
const UNLINK_NO_READ: &str = "파일을 다 읽지 못해서 풀지 않았어요";
const UNLINK_CHANGED: &str = "푸는 동안 파일이 바뀌어서 풀지 않았어요";
const UNLINK_NO_COPY: &str = "복사본을 만들지 못해서 풀지 않았어요";
const UNLINK_DIFFERENT: &str = "복사본이 원래 내용과 달라서 풀지 않았어요";
const UNLINK_NO_SECURITY: &str = "권한 설정을 그대로 옮기지 못해서 풀지 않았어요";
const UNLINK_NOT_PUT: &str = "복사본을 제자리에 놓지 못해서 풀지 않았어요";

/// Makes each of `paths` (names of files with more than one) a file of its
/// own (see above). Returns the id of the job (op "unlink") at once; a name
/// it may not touch is turned down in the job, with why.
#[tauri::command]
pub async fn start_unlink(app: AppHandle, paths: Vec<String>) -> Result<u64, String> {
    blocking(move || begin_unlink(paths, app, Settings::APP, links::Ledger::app())).await?
}

fn begin_unlink(paths: Vec<String>, events: impl Events, settings: Settings, ledger: links::Ledger) -> Result<u64, String> {
    let mut seen = HashSet::new();
    let paths: Vec<PathBuf> = paths.into_iter().filter(|p| seen.insert(p.trim_end_matches('\\').to_lowercase())).map(PathBuf::from).collect();
    if paths.is_empty() {
        return Err(NOTHING_TO_UNLINK.into());
    }
    if paths.len() > MAX_LINKS {
        return Err(format!("{TOO_MANY_UNLINKS} (고른 파일 {}개)", paths.len()));
    }
    let dest = shown(&common_folder(&paths));
    let run = new_job("unlink", false, Vec::new(), dest, events, settings);
    launch(run, move |run| run.unlink_all(&paths, &ledger))
}

/// Whether the entry at `path` is on a local fixed NTFS volume, where it
/// really is (a drive with a letter: `real_path`).
fn local_ntfs(path: &Path) -> bool {
    real_path(path).and_then(|real| volume(&real)).is_some_and(|v| v.fixed && v.fs.eq_ignore_ascii_case("NTFS"))
}

/// Opens a name of a file whose other name is to become a file of its own:
/// to read it, with nobody writing to it meanwhile (no FILE_SHARE_WRITE),
/// and to set its attributes where that is given. Replacing its other names
/// still works (FILE_SHARE_DELETE).
fn open_shared(path: &Path) -> io::Result<File> {
    let open = |access: u32| {
        OpenOptions::new()
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN)
            .open(win::verbatim(path))
    };
    open(GENERIC_READ | FILE_WRITE_ATTRIBUTES).or_else(|e| match e.raw_os_error() {
        Some(c) if c == ERROR_ACCESS_DENIED as i32 => open(GENERIC_READ),
        _ => Err(e),
    })
}

/// Why the name `path` could not be replaced (`e`). Windows says "denied"
/// also when a program holds the file open without letting its names go,
/// which a look with every kind of sharing tells apart.
fn replace_reason(path: &Path, e: &io::Error) -> String {
    if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        let look = OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(win::verbatim(path));
        if let Err(held) = look
            && held.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
        {
            return reason(&held);
        }
    }
    reason(e)
}

/// The times and attributes of the file open as `file`.
fn basic_info(file: &File) -> Option<FILE_BASIC_INFO> {
    let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as u32;
    let ok = unsafe { GetFileInformationByHandleEx(file.as_raw_handle() as HANDLE, FileBasicInfo, (&raw mut info).cast(), size) } != 0;
    ok.then_some(info)
}

/// Sets on the file open as `file` (with FILE_WRITE_ATTRIBUTES) the times of
/// `info` that are not 0, and its attributes unless they are 0. Whether
/// that was done.
fn put_basic(file: &File, info: &FILE_BASIC_INFO) -> bool {
    let size = std::mem::size_of_val(info) as u32;
    unsafe { SetFileInformationByHandle(file.as_raw_handle() as HANDLE, FileBasicInfo, (info as *const FILE_BASIC_INFO).cast(), size) != 0 }
}

/// Gives the file open as `file` the attributes `attrs` (those
/// SetFileAttributes changes), its times as they are.
fn put_attrs_open(file: &File, attrs: u32) -> bool {
    let attrs = attrs & SETTABLE;
    let info = FILE_BASIC_INFO { FileAttributes: if attrs == 0 { FILE_ATTRIBUTE_NORMAL } else { attrs }, ..unsafe { std::mem::zeroed() } };
    put_basic(file, &info)
}

impl Run {
    /// An unlink: each name in turn.
    fn unlink_all(&mut self, paths: &[PathBuf], ledger: &links::Ledger) -> Result<(), Stop> {
        // Each name is copied once and read twice (`unlink_switch`).
        let sizes: Vec<u64> = paths.iter().map(|p| std::fs::symlink_metadata(win::verbatim(p)).map_or(0, |m| file_len(&m))).collect();
        self.info.total_items = paths.len() as u64;
        self.info.total_bytes = sizes.iter().map(|s| s.saturating_mul(3)).sum();
        self.publish();
        self.speed = Speed::new();
        let windows = known_folder(&FOLDERID_Windows);
        let mut buf = vec![0; HASH_BUF];
        let mut freed = HashSet::new();
        // The files whose names were made files of their own in this job.
        let mut split = HashSet::new();
        let mut ended = Ok(());
        for (path, &size) in paths.iter().zip(&sizes) {
            if let Err(stop) = self.check() {
                ended = Err(stop);
                break;
            }
            let start = self.info.done_bytes;
            self.info.current = Some(path.to_string_lossy().into_owned());
            self.tick();
            // The last name of a file whose other names this job made files of their own: one already.
            let left = key_at(path).filter(|k| split.contains(k) && links::names_of(path).is_some_and(|n| n.len() == 1));
            let done = match left {
                Some(_) => Ok(()),
                None => self.unlink_one(path, windows.as_deref(), &mut buf).map(|shared| {
                    split.insert(shared);
                }),
            };
            match done {
                Ok(()) => {
                    self.handled += 1;
                    freed.insert(shown(path).to_lowercase());
                    freed.extend(real_path(path).map(|p| shown(&p).to_lowercase()));
                }
                Err(Halt::Why(why)) => self.error(path, why),
                Err(Halt::Stop) => {
                    ended = Err(Stop);
                    break;
                }
            }
            self.info.done_bytes = start + size.saturating_mul(3);
            self.info.done_items += 1;
            self.tick();
        }
        // The merges written down that these names were in and that are undone now.
        if !freed.is_empty() {
            let named = |p: &str| freed.contains(&p.to_lowercase());
            let _ = ledger.forget(|m| (named(&m.kept) || named(&m.linked)) && !links::still_linked(m));
        }
        ended
    }

    /// Makes the name `path` a file of its own: steps 1 to 4 above. Nothing
    /// is changed before its copy has been compared. Gives which file it was
    /// a name of, which keeps its other names.
    fn unlink_one(&mut self, path: &Path, windows: Option<&Path>, buf: &mut [u8]) -> Result<dups::FileKey, Halt> {
        let refuse = |why: &str| Halt::Why(why.into());
        if !path.is_absolute() {
            return Err(refuse(BAD_PATH));
        }
        // A share on the network (`\\server\share`) is told by its name, without asking the network.
        if shown(path).starts_with(r"\\") {
            return Err(refuse(UNLINK_NOT_LOCAL));
        }
        let lost = |e: io::Error| match e.kind() {
            io::ErrorKind::NotFound => refuse(UNLINK_GONE),
            _ => Halt::Why(format!("{UNLINK_NO_OPEN}: {}", reason(&e))),
        };
        let meta = std::fs::symlink_metadata(win::verbatim(path)).map_err(lost)?;
        if !local_ntfs(path) {
            return Err(refuse(UNLINK_NOT_LOCAL));
        }
        if links::protected(path, windows) {
            return Err(refuse(UNLINK_SYSTEM));
        }
        if dups::online_only(meta.file_attributes()) {
            return Err(refuse(UNLINK_CLOUD));
        }
        if kind_of(&meta) != Kind::File || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(refuse(UNLINK_NOT_PLAIN));
        }
        // Every name of the file: none may be Windows's, and another must be left.
        let names = links::names_of(path).ok_or_else(|| refuse(UNLINK_NO_OPEN))?;
        if names.iter().any(|n| links::protected(Path::new(n), windows)) {
            return Err(refuse(UNLINK_SYSTEM));
        }
        let me = key_at(path).ok_or_else(|| refuse(UNLINK_NO_OPEN))?;
        let real = real_path(path).unwrap_or_else(|| path.to_path_buf());
        let others: Vec<&String> = names.iter().filter(|n| !same_text(Path::new(n), &real)).collect();
        if others.is_empty() {
            return Err(refuse(UNLINK_LAST));
        }
        // Held through another name, so that this one can be replaced.
        let mut opened = Err(io::Error::from(io::ErrorKind::NotFound));
        for other in others {
            opened = open_shared(Path::new(other));
            if opened.is_ok() {
                break;
            }
        }
        let mut held = opened.map_err(|e| Halt::Why(format!("{UNLINK_NO_OPEN}: {}", reason(&e))))?;
        let facts = dups::facts(&held).ok_or_else(|| refuse(UNLINK_NO_OPEN))?;
        if facts.key != Some(me) {
            return Err(refuse(UNLINK_CHANGED));
        }
        if facts.links < 2 {
            return Err(refuse(UNLINK_LAST));
        }
        let need = facts.size.saturating_add(SPACE_MARGIN);
        match win::free_space(path.parent().unwrap_or(path)) {
            Some(free) if free >= need => {}
            free => return Err(Halt::Why(no_room(path, need, free))),
        }
        let temp = self.unlink_copy(path, facts.size)?;
        let done = self.unlink_switch(path, &temp, &mut held, &facts, buf);
        // Our own copy, which did not get the name.
        if done.is_err() && !gone(&temp) {
            remove_partial(&temp);
        }
        done.map(|()| me)
    }

    /// Step 2: copies the file at `path` (`size` bytes) beside it under a
    /// free hidden `.name.ff-unlink`, never onto anything (CopyFileEx, with
    /// alternate streams and attributes). A copy cut short is removed.
    fn unlink_copy(&mut self, path: &Path, size: u64) -> Result<PathBuf, Halt> {
        let from = win::wide_path(path);
        let base = self.info.done_bytes;
        for n in 1..=PARTIAL_TRIES {
            let Some(temp) = marked_name(path, true, UNLINK_EXT, n) else { break };
            let mut p = CopyProgress::new(self, base, size, &temp, None);
            let ok = unsafe {
                CopyFileExW(from.as_ptr(), p.partial.as_ptr(), Some(copy_progress), (&raw mut p).cast(), null_mut(), COPY_FLAGS)
            } != 0;
            let err = io::Error::last_os_error();
            if ok {
                return Ok(temp);
            }
            // Once started, it is our own unfinished copy: the copy only begins on a name that was free.
            if p.started {
                remove_partial(&temp);
            }
            if self.check().is_err() || err.raw_os_error() == Some(ERROR_REQUEST_ABORTED as i32) {
                return Err(Halt::Stop);
            }
            // One that is there already (another job's, one a crash left) is not ours to touch.
            if !p.started && err.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(Halt::Why(format!("{UNLINK_NO_COPY}: {}", reason(&err))));
        }
        Err(Halt::Why(format!("{UNLINK_NO_COPY}: {NO_FREE_NAME}")))
    }

    /// Steps 3 and 4: the copy at `temp` compared with the file (`held`,
    /// open through another of its names, `facts` what it said then), given
    /// its times and its own permissions, and put in the place of `path`.
    fn unlink_switch(&mut self, path: &Path, temp: &Path, held: &mut File, facts: &dups::Facts, buf: &mut [u8]) -> Result<(), Halt> {
        // Our own copy, opened to write (to flush it, below): the read-only mark CopyFileEx gave
        // it is off until it has the name, when it gets the file's attributes.
        let attrs = unsafe { GetFileAttributesW(win::wide_path(temp).as_ptr()) };
        if attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_READONLY != 0 {
            put_attrs(temp, attrs & !FILE_ATTRIBUTE_READONLY);
        }
        // Held from here as well: nobody else writes to it until it has the name.
        let mut copy = OpenOptions::new()
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN)
            .open(win::verbatim(temp))
            .map_err(|e| Halt::Why(format!("{UNLINK_NO_COPY}: {}", reason(&e))))?;
        let copied = dups::facts(&copy).ok_or_else(|| Halt::Why(UNLINK_NO_COPY.into()))?;
        if copied.size != facts.size || copied.key.is_none() || copied.key == facts.key {
            return Err(Halt::Why(UNLINK_DIFFERENT.into()));
        }
        let texts = (UNLINK_CHANGED, UNLINK_NO_READ);
        let theirs = self.hash_whole_as(held, facts.size, buf, texts)?;
        let ours = self.hash_whole_as(&mut copy, facts.size, buf, texts)?;
        if ours != theirs {
            return Err(Halt::Why(UNLINK_DIFFERENT.into()));
        }
        // The file's times (CopyFileEx gives it only some of them); its attributes once it has the name.
        let basic = basic_info(held).ok_or_else(|| Halt::Why(UNLINK_NO_OPEN.into()))?;
        let times = FILE_BASIC_INFO {
            CreationTime: basic.CreationTime,
            LastAccessTime: basic.LastAccessTime,
            LastWriteTime: basic.LastWriteTime,
            ..unsafe { std::mem::zeroed() }
        };
        if !put_basic(&copy, &times) {
            return Err(Halt::Why(format!("{UNLINK_NO_COPY}: {}", reason(&io::Error::last_os_error()))));
        }
        copy_own_security(held, temp).map_err(|e| Halt::Why(format!("{UNLINK_NO_SECURITY}: {}", reason(&e))))?;
        // All of it on the disk before it takes the name: MOVEFILE_WRITE_THROUGH writes the rename
        // through, not the copy's data, which a power cut right after could leave as zeros there.
        copy.sync_all().map_err(|e| Halt::Why(format!("{UNLINK_NO_COPY}: {}", reason(&e))))?;
        // A read-only file's names cannot be replaced: the mark (the file's, at every name) is off for the moment.
        let read_only = facts.attrs & FILE_ATTRIBUTE_READONLY != 0;
        if read_only && !put_attrs_open(held, facts.attrs & !FILE_ATTRIBUTE_READONLY) {
            return Err(Halt::Why(format!("{UNLINK_NOT_PUT}: {}", reason(&io::Error::last_os_error()))));
        }
        // The name still leads to the file, right before it is replaced.
        let moved = if key_at(path) != facts.key {
            Err(Halt::Why(UNLINK_CHANGED.into()))
        } else {
            let (from, to) = (win::wide_path(temp), win::wide_path(path));
            if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
                Err(Halt::Why(format!("{UNLINK_NOT_PUT}: {}", replace_reason(path, &io::Error::last_os_error()))))
            } else {
                Ok(())
            }
        };
        if read_only {
            put_attrs_open(held, facts.attrs);
        }
        moved?;
        // The name's own file now: the file's attributes, not hidden unless it was.
        put_attrs_open(&copy, facts.attrs);
        Ok(())
    }
}
