//! Live index updates with normal user rights. One thread per indexed local
//! drive asks Windows for every change below the drive's root
//! (ReadDirectoryChangesW), gathers them for about 300 ms, looks at what is
//! on disk now and applies the batch to the index under one write lock.
//!
//! When Windows has to drop changes (its buffer overflowed during a burst),
//! the drive is marked stale: the index keeps working and keeps taking the
//! changes that do arrive, but may miss some until the drive is synced again
//! (lib.rs does that by itself a little later, without administrator rights:
//! scan/sync.rs) or rescanned.
//!
//! A rescan starts its watcher before it reads anything. That watcher holds
//! the changes it hears until the new index is attached and applies them to
//! it first, so what changed while the scan ran is not lost.
//!
//! Hard links: a file's bytes are counted through one of its names
//! (FLAG_LINKED when it has others, which carry FLAG_HARDLINK). Live changes
//! cannot tell which names belong to one file, so a hard link made while the
//! app runs is counted a second time, and a write through an extra link
//! changes only that name's size. Removing the counted name takes the bytes
//! out of the totals although the other names still hold them; that marks
//! the drive stale.

use crate::index::{Index, Meta, RawEntry, FLAG_DELETED, FLAG_DIR, FLAG_NOT_LINK, FLAG_REPARSE, ROOT};
use crate::scan::walk;
use crate::win;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Register_Notification, CM_Unregister_Notification, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICECUSTOMEVENT,
    CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE, CM_NOTIFY_ACTION_DEVICEREMOVEPENDING,
    CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0_0, CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE, CR_SUCCESS,
    HCMNOTIFICATION,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GUID_IO_VOLUME_LOCK;

/// Changes are gathered this long, then applied together.
const COALESCE: Duration = Duration::from_millis(300);
/// The notification buffer of a local drive. Windows keeps a kernel buffer of
/// the same size for changes that come while no read is waiting, or while the
/// watcher applies a batch. A system drive writes all the time (logs, caches,
/// updates); 64 KiB overflowed there in bursts, and every overflow means the
/// drive must be synced again.
const BUF_LOCAL: usize = 1 << 20;
/// Windows takes no more than this over the network (anything bigger fails).
/// Network drives are not watched now; this is for when they are.
const BUF_NETWORK: usize = 64 << 10;
const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_ATTRIBUTES;
/// Past this many changed folders a batch is reported as "everything changed".
const MAX_DIRS: usize = 256;
/// How long an eject (or a volume lock) waits for the watcher to close the root.
const RELEASE_WAIT_MS: u32 = 5000;
/// After an eject request or an error the root is tried again this often, this many times.
const REOPEN_PAUSE_MS: u32 = 5000;
const REOPEN_TRIES: u32 = 12;
/// Errors in a row, with no read getting through in between, before the
/// watcher gives up on the drive.
const MAX_FAILS: u32 = 5;
/// A read that something other than the watcher cancelled is started again
/// (`Got::Cancelled`). Only this many cancels in a row, each less than
/// CANCEL_BURST after the one before and no read getting through between
/// them, count as an error.
const MAX_CANCELS: u32 = 5;
const CANCEL_BURST: Duration = Duration::from_secs(1);
/// Changes a watcher holds at most while a rescan runs (a path modified
/// again and again counts once).
const MAX_HELD: usize = 200_000;
/// Changes applied under one write lock at most. The rest follow straight
/// away, each part under a lock of its own, so commands reading the drive
/// get in between (a rescan can hand over MAX_HELD changes at once).
const CHUNK: usize = 5000;

// A drive marked stale for one of these is synced again by itself (lib.rs `auto_sync`).
// Not "soon": a drive that keeps missing changes is synced less and less often (sync::AutoSync).
const LOST: &str = "변경이 한꺼번에 많이 생겨서 일부 알림을 놓쳤어요. 관리자 권한 없이 저절로 다시 맞춰요 (자주 놓치면 간격이 길어져요).";
const PAUSED: &str = "실시간 반영이 잠시 멈춘 사이에 바뀐 내용이 있을 수 있어요. 관리자 권한 없이 저절로 다시 맞춰요 (자주 놓치면 간격이 길어져요).";
const REOPENED: &str = "변경 알림이 끊겼다가 다시 이어졌어요. 그동안 바뀐 내용은 관리자 권한 없이 저절로 다시 맞춰요 (자주 놓치면 간격이 길어져요).";
const BROKEN: &str = "내부 오류로 실시간 반영을 멈췄어요. 다시 스캔하면 다시 켜져요.";
const GONE: &str = "드라이브를 다시 열 수 없어서 실시간 반영을 멈췄어요. 다시 스캔하면 다시 켜져요.";
const LINKS: &str = "하드 링크가 여러 개인 파일이 지워져서 폴더 크기가 실제와 다를 수 있어요. 정확한 크기는 전체 스캔(MFT)으로 다시 세어 주세요.";

/// How a drive's index keeps up with the disk (`live_status`).
#[derive(Clone)]
pub struct Status {
    /// "watching", "stale", "off" or "error".
    pub state: &'static str,
    pub detail: Option<String>,
}

impl Status {
    /// Whether a quick sync (scan/sync.rs) brings the index back in step:
    /// changes were missed. Not after a counted hard link was removed: a sync
    /// keeps the hard-link marks of the files it does not see change, so only
    /// a full scan can count those bytes again.
    pub fn sync_helps(&self) -> bool {
        self.state == "stale" && !self.links_lost()
    }

    /// Whether a counted hard link was removed: only a full scan counts those bytes again.
    pub fn links_lost(&self) -> bool {
        self.state == "stale" && is_links_lost(self.detail.as_deref().unwrap_or_default())
    }
}

/// Whether `reason`, as a watcher reports it (`Sink::stale`), is a counted
/// hard link lost, which no sync puts right.
pub fn is_links_lost(reason: &str) -> bool {
    reason == LINKS
}

/// Where a watcher reports to.
pub trait Sink: Send + Sync {
    /// A batch of changes is in the index. `dirs`: folders whose direct
    /// children changed; empty with `full` when there were too many to list.
    /// `removed`: folders that were removed, or renamed or moved away, by
    /// their old paths, none inside another. Whatever was shown at or below
    /// one of them is not there any more.
    fn changed(&self, drive: &str, dirs: Vec<String>, full: bool, removed: Vec<String>);
    /// Changes were lost; the index works but may be out of date.
    fn stale(&self, drive: &str, reason: &str);
}

/// A running watcher, or the reason a drive is not watched. Dropping it
/// stops the thread without waiting for it.
pub struct Watcher {
    stop: Option<Arc<Event>>,
    status: Arc<Mutex<Status>>,
    /// Set once a counted hard link was lost (or kept, `keep_links_lost`):
    /// the drive stays stale for that whatever else happens, until a full
    /// scan brings a new watcher.
    links: Arc<AtomicBool>,
    /// For a watcher started without an index: what `attach` needs.
    held: Option<Held>,
}

struct Held {
    drive: String,
    sink: Arc<dyn Sink>,
    /// None when the watcher could not start.
    target: Option<Arc<Target>>,
}

impl Watcher {
    /// A drive that is not watched, and why.
    pub fn off(detail: &str) -> Watcher {
        let status = Arc::new(Mutex::new(Status { state: "off", detail: Some(detail.into()) }));
        Watcher { stop: None, status, links: Arc::default(), held: None }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Whether this watcher's drive lost a counted hard link (or took the
    /// loss over, `keep_links_lost`). Unlike `Status::links_lost`, still true
    /// after an error replaced the status: the bytes are counted nowhere
    /// whatever the watcher went through since.
    pub fn lost_links(&self) -> bool {
        self.links.load(Ordering::Relaxed)
    }

    /// For a watcher taking over from one whose drive lost a counted hard
    /// link (`Status::links_lost`), as a sync's does: the sync keeps the
    /// hard-link marks of the files it does not see change, so those bytes
    /// are still counted nowhere, and the drive stays stale for that until a
    /// full scan. Called before `attach`, which tells the window.
    pub fn keep_links_lost(&self) {
        self.links.store(true, Ordering::Relaxed);
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(status.state, "watching" | "stale") {
            *status = Status { state: "stale", detail: Some(LINKS.into()) };
        }
    }

    /// Gives a watcher started without an index (`start` with None) the
    /// index to apply its changes to, the ones it held so far first. What
    /// went wrong while it waited (lost changes, a failed start) is reported
    /// now, when it concerns this index.
    pub fn attach(&self, shared: Arc<RwLock<Index>>) {
        let Some(held) = &self.held else { return };
        let unit = held.target.as_ref().map(|t| t.unit_for(&shared));
        // Under the status lock, which the watcher holds while it decides
        // whether to report, so nothing is told twice or not at all.
        let status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if let (Some(target), Some(unit)) = (&held.target, unit) {
            target.set(shared, unit);
        }
        let report = (status.state != "watching").then(|| status.detail.clone().unwrap_or_default());
        drop(status);
        if let Some(reason) = report {
            held.sink.stale(&held.drive, &reason);
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The thread closes its handles on its own; nobody waits for it, so
        // closing the app never hangs on a drive.
        if let Some(stop) = &self.stop {
            stop.set();
        }
    }
}

/// The index a watcher applies changes to.
struct Target {
    /// The index and the unit of its allocated sizes; None until attached.
    index: Mutex<Option<(Arc<RwLock<Index>>, u64)>>,
    /// Set when an index is attached, which wakes a watcher holding changes.
    ready: Event,
    /// The drive's cluster size.
    cluster: u64,
}

impl Target {
    /// An MFT index counts the clusters a file takes (and so does a sync of
    /// one, scan/sync.rs); live changes round to them the same way. A walk
    /// counts allocated = size, and so do its updates.
    fn unit_for(&self, shared: &RwLock<Index>) -> u64 {
        let mft = crate::scan::sync::sees_protected(&shared.read().unwrap_or_else(PoisonError::into_inner).method);
        if mft { self.cluster } else { 0 }
    }

    fn set(&self, shared: Arc<RwLock<Index>>, unit: u64) {
        *self.index.lock().unwrap_or_else(PoisonError::into_inner) = Some((shared, unit));
        self.ready.set();
    }

    fn get(&self) -> Option<(Arc<RwLock<Index>>, u64)> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn attached(&self) -> bool {
        self.index.lock().unwrap_or_else(PoisonError::into_inner).is_some()
    }
}

/// Starts watching `drive` (`C:`) and applying its changes to `shared`.
/// With None the changes are held until an index is attached
/// (`Watcher::attach`): a rescan starts its watcher so, before it reads the drive.
pub fn start(drive: &str, shared: Option<Arc<RwLock<Index>>>, sink: Arc<dyn Sink>) -> Watcher {
    let status = Arc::new(Mutex::new(Status { state: "watching", detail: None }));
    let links = Arc::new(AtomicBool::new(false));
    let held = shared.is_none();
    let root = format!("{drive}\\");
    let (stop, ready) = match (Event::new(), Event::new()) {
        (Ok(stop), Ok(ready)) => (Arc::new(stop), ready),
        (Err(e), _) | (_, Err(e)) => return failed(drive, sink, status, links, e, held),
    };
    let target = Arc::new(Target { index: Mutex::new(None), ready, cluster: win::cluster_size(drive).unwrap_or(0) });
    if let Some(shared) = shared {
        let unit = target.unit_for(&shared);
        target.set(shared, unit);
    }
    let buf = buffer_for(win::drive_kind(drive));
    // Only opened here; the watcher's own thread starts every read (`Worker::run`).
    let first = match Root::open(&root, buf) {
        Ok(r) => r,
        Err(e) => return failed(drive, sink, status, links, e, held),
    };
    let worker = Worker {
        drive: drive.to_string(),
        ignore: data_dir_on(&root),
        volume: win::volume_id(drive),
        root,
        buf,
        target: target.clone(),
        sink: sink.clone(),
        status: status.clone(),
        links: links.clone(),
        stop: stop.clone(),
    };
    // Back only once the thread's first read is waiting: Windows records a
    // folder's changes from its first read on, and a rescan reads the drive
    // straight after this, counting on what changes meanwhile being heard.
    let (started, first_read) = sync_channel(1);
    let begun = match std::thread::Builder::new().name(format!("watch {drive}")).spawn(move || worker.run(first, started)) {
        Ok(_) => first_read.recv().unwrap_or_else(|_| Err(io::Error::other("감시 스레드가 첫 읽기 전에 멈췄어요"))),
        Err(e) => Err(e),
    };
    match begun {
        Ok(()) => Watcher {
            stop: Some(stop),
            status,
            links,
            held: held.then(|| Held { drive: drive.to_string(), sink, target: Some(target) }),
        },
        Err(e) => failed(drive, sink, status, links, e, held),
    }
}

/// The notification buffer for a drive of `kind` (`win::drive_kind`).
fn buffer_for(kind: &str) -> usize {
    if kind == "network" { BUF_NETWORK } else { BUF_LOCAL }
}

/// A watcher that could not start: the index stays as it is until rescanned.
/// One started ahead of a rescan says so when the new index is attached.
fn failed(drive: &str, sink: Arc<dyn Sink>, status: Arc<Mutex<Status>>, links: Arc<AtomicBool>, e: io::Error, held: bool) -> Watcher {
    let detail = format!("변경 알림을 켤 수 없어서 실시간 반영을 하지 못해요 ({e}). 다시 스캔하면 정확해져요.");
    *status.lock().unwrap_or_else(PoisonError::into_inner) = Status { state: "error", detail: Some(detail.clone()) };
    if !held {
        sink.stale(drive, &detail);
    }
    Watcher { stop: None, status, links, held: held.then(|| Held { drive: drive.to_string(), sink, target: None }) }
}

/// The app's data folder (caches, WebView data), lowercase, when it is on
/// this drive. Saving a cache must not come back as a change to apply.
fn data_dir_on(root: &str) -> Option<String> {
    let dir = crate::scan::data_dir().to_string_lossy().to_lowercase();
    dir.starts_with(&root.to_lowercase()).then(|| dir.trim_end_matches('\\').to_string())
}

fn is_ignored(path: &str, ignore: Option<&str>) -> bool {
    ignore.is_some_and(|dir| {
        let p = path.to_lowercase();
        p.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
    })
}

// ---- the watcher thread -----------------------------------------------------

struct Worker {
    drive: String,
    /// The watched folder with a trailing backslash (`C:\`).
    root: String,
    /// Bytes of the notification buffer (`buffer_for`).
    buf: usize,
    target: Arc<Target>,
    sink: Arc<dyn Sink>,
    status: Arc<Mutex<Status>>,
    /// `Watcher::links`.
    links: Arc<AtomicBool>,
    stop: Arc<Event>,
    ignore: Option<String>,
    /// The volume at the start; a root that comes back must still be it.
    volume: Option<String>,
}

enum End {
    Stop,
    /// The drive is being ejected or locked; the root had to be closed.
    Released,
    Failed(io::Error),
    /// A panic while applying changes: the index may be half-updated.
    Broken,
}

impl Worker {
    /// `started` hears how the first read went; after one that failed the
    /// thread ends there (`start` reports it).
    fn run(self, first: Root, started: SyncSender<io::Result<()>>) {
        let mut root = first;
        // Here, not in `start`: Windows cancels the reads a thread started when
        // that thread ends (this handle has no completion port), and `start`
        // runs on pooled threads of the async runtime, which end when idle. So
        // every read is started on this thread.
        let begun = root.read();
        let ok = begun.is_ok();
        let _ = started.send(begun);
        if !ok {
            return;
        }
        // Ends in a row that were errors, with no read getting through in between.
        let mut fails = 0;
        loop {
            let mut read = false;
            let end = catch_unwind(AssertUnwindSafe(|| self.watch(&mut root, &mut read))).unwrap_or(End::Broken);
            // Closing the root lets an eject or a volume lock go ahead.
            drop(root);
            if read {
                fails = 0;
            }
            let after_error = matches!(end, End::Failed(_));
            match end {
                End::Stop => return,
                End::Broken => {
                    self.report("error", BROKEN);
                    return;
                }
                End::Released => {}
                End::Failed(e) => {
                    fails += 1;
                    // A root that opens but whose reads keep failing (some file
                    // systems that pass for local disks) is not reopened forever.
                    if fails >= MAX_FAILS {
                        let reason = format!("변경 알림이 계속 끊겨서 실시간 반영을 멈췄어요 ({e}). 다시 스캔하면 다시 켜져요.");
                        self.report("error", &reason);
                        return;
                    }
                    // Told even when the drive was stale already (missed changes,
                    // a cancel just before, the reconnect after an earlier error):
                    // the window then stops saying a sync comes by itself, which
                    // `auto_sync_wanted` turns down while this lasts.
                    let reason = format!("변경 알림이 끊겼어요 ({e}). 잠시 뒤 다시 이어 볼게요.");
                    self.report("error", &reason);
                }
            }
            root = match self.reopen(fails, after_error) {
                Some(r) => r,
                None => return,
            };
            if after_error {
                self.reconnected();
            } else {
                self.mark_stale(PAUSED);
            }
        }
    }

    /// Reads changes until stopped, released or broken. Sets `read` once a
    /// read has come through.
    fn watch(&self, root: &mut Root, read: &mut bool) -> End {
        const STOPPED: u32 = WAIT_OBJECT_0;
        const READ: u32 = WAIT_OBJECT_0 + 1;
        const RELEASE: u32 = WAIT_OBJECT_0 + 2;
        const READY: u32 = WAIT_OBJECT_0 + 3;
        let mut batch = Vec::new();
        // Paths `batch` has as Modified.
        let mut modified = HashSet::new();
        let mut old = None;
        let mut due: Option<Instant> = None;
        let mut target = self.target.get();
        let mut cancels = Cancels::default();
        // A root comes with its read started (`run`, `reopen`); one without
        // gets it here, on this thread like every read.
        if !root.pending
            && let Err(e) = root.read()
        {
            return End::Failed(e);
        }
        loop {
            let wait = due.map_or(INFINITE, |d| d.saturating_duration_since(Instant::now()).as_millis() as u32);
            let handles = [self.stop.0, root.io.0, root.pnp.events.release.0, self.target.ready.0];
            // Until an index is attached, its arrival wakes the thread too.
            let count = if target.is_some() { 3 } else { 4 };
            match unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, wait) } {
                STOPPED => return End::Stop,
                READ => {
                    match root.finish() {
                        Ok(Got::Changes(raw)) => {
                            *read = true;
                            cancels = Cancels::default();
                            let from = batch.len();
                            collect(&self.root, self.ignore.as_deref(), raw, &mut old, &mut batch);
                            keep_once(&mut batch, from, &mut modified);
                        }
                        Ok(Got::Lost) => {
                            *read = true;
                            cancels = Cancels::default();
                            // The new name for a waiting old name may be among what
                            // was lost; the next unrelated one must not pair with it.
                            unpaired(&mut old, self.ignore.as_deref(), &mut batch);
                            self.mark_stale(LOST);
                        }
                        Ok(Got::Cancelled) => {
                            // Not by this watcher, which cancels a read only once it has
                            // left here (closing the root), but by a thread that started
                            // the read and ended, or another program. Started again
                            // below; what came meanwhile may be lost, which a sync that
                            // runs by itself puts right. Only cancels that keep coming
                            // straight away are an error.
                            if self.stopping() {
                                return End::Stop;
                            }
                            if cancels.again(Instant::now()) {
                                return End::Failed(io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED as i32));
                            }
                            unpaired(&mut old, self.ignore.as_deref(), &mut batch);
                            self.mark_stale(PAUSED);
                        }
                        Err(e) => return End::Failed(e),
                    }
                    // Straight away, before applying anything, so few changes wait in the kernel.
                    if let Err(e) = root.read() {
                        return End::Failed(e);
                    }
                    if target.is_none() && batch.len() > MAX_HELD {
                        // More than is worth holding: the new index needs a rescan anyway.
                        batch.clear();
                        modified.clear();
                        self.mark_stale(LOST);
                    }
                    if !batch.is_empty() && due.is_none() {
                        due = Some(Instant::now() + COALESCE);
                    }
                }
                RELEASE => return End::Released,
                READY | WAIT_TIMEOUT => {}
                _ => return End::Failed(io::Error::last_os_error()),
            }
            if target.is_none() {
                target = self.target.get();
                if target.is_none() {
                    // Held until the rescan's index is attached.
                    due = None;
                    continue;
                }
                // What was held goes in straight away.
                if !batch.is_empty() {
                    due = Some(Instant::now());
                }
            }
            let Some((shared, unit)) = &target else { continue };
            if due.is_some_and(|d| Instant::now() >= d) {
                let now = next_chunk(&mut batch, &mut modified);
                // What did not fit is due straight away.
                due = (!batch.is_empty()).then(Instant::now);
                match self.flush(shared, *unit, now) {
                    Some(later) if !later.is_empty() => {
                        batch.extend(later);
                        due.get_or_insert_with(|| Instant::now() + COALESCE);
                    }
                    Some(_) => {}
                    None => return End::Broken,
                }
            }
        }
    }

    /// Applies a batch. Returns changes to look at again with the next batch
    /// (folders that turned up without their contents), or None after a panic.
    fn flush(&self, shared: &RwLock<Index>, unit: u64, changes: Vec<Change>) -> Option<Vec<Change>> {
        let prep = prepare(&changes, unit);
        let mut ix = shared.write().unwrap_or_else(PoisonError::into_inner);
        // Caught while the lock is still held, so a panic cannot poison it
        // for every command that reads this drive.
        let done = catch_unwind(AssertUnwindSafe(|| {
            let out = apply(&mut ix, &changes, prep, unit);
            if ix.wants_compact() {
                ix.compact();
            }
            out
        }));
        drop(ix);
        let out = done.ok()?;
        if out.links_lost {
            self.links.store(true, Ordering::Relaxed);
            self.mark_stale(LINKS);
        }
        if out.full || !out.dirs.is_empty() || !out.removed.is_empty() {
            self.sink.changed(&self.drive, out.dirs, out.full, out.removed);
        }
        Some(out.later)
    }

    /// Opens the root again once the drive is back to normal: an eject that
    /// did not happen, a volume lock that ended, an error that passed. None
    /// on stop, or when it is still gone after a while; an ejected drive's
    /// letter may later belong to another disk, so it is not waited for.
    /// After `fails` errors in a row each try waits longer. `after_error`:
    /// the window heard of an error, so it hears when the drive stays gone too.
    fn reopen(&self, fails: u32, after_error: bool) -> Option<Root> {
        let pause = REOPEN_PAUSE_MS << fails.saturating_sub(1).min(3);
        for _ in 0..REOPEN_TRIES {
            if unsafe { WaitForSingleObject(self.stop.0, pause) } == WAIT_OBJECT_0 {
                return None;
            }
            if win::volume_id(&self.drive) != self.volume {
                continue;
            }
            // Its read starts here, on this thread; a root whose read fails is not back yet.
            if let Ok(root) = Root::open(&self.root, self.buf).and_then(|mut r| r.read().map(|()| r)) {
                return Some(root);
            }
        }
        if after_error {
            self.report("off", GONE);
        } else {
            self.set("off", GONE);
        }
        None
    }

    fn stopping(&self) -> bool {
        unsafe { WaitForSingleObject(self.stop.0, 0) == WAIT_OBJECT_0 }
    }

    fn set(&self, state: &'static str, detail: &str) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = Status { state, detail: Some(detail.into()) };
    }

    /// The root is open again after an error. What changed while it was not
    /// was not heard: the drive is stale, which a sync that runs by itself
    /// puts right (`Status::sync_helps`, lib.rs `Live::stale`), not in error
    /// any more. The window is told even though it heard of trouble before,
    /// so it stops showing the error. A lost hard link still outranks this.
    fn reconnected(&self) {
        let reason = if self.links.load(Ordering::Relaxed) { LINKS } else { REOPENED };
        self.report("stale", reason);
    }

    /// Sets the status and tells the window, whatever it was told before:
    /// an error, and the reconnect after it, change what the window shows
    /// and whether a sync runs by itself. While the watcher holds changes
    /// for an index to come, `attach` tells.
    fn report(&self, state: &'static str, reason: &str) {
        // Under the status lock, which `attach` holds too, so nothing is told twice or not at all.
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status = Status { state, detail: Some(reason.into()) };
        let tell = self.target.attached();
        drop(status);
        if tell {
            self.sink.stale(&self.drive, reason);
        }
    }

    /// Marks the drive stale. Only the first time is reported: it stays
    /// stale until a rescan starts a new watcher. A counted hard link lost
    /// is the exception: it replaces the reason before it (a sync helps
    /// that, not this) and is told too, and no later reason replaces it.
    fn mark_stale(&self, reason: &str) {
        let reason = if self.links.load(Ordering::Relaxed) { LINKS } else { reason };
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        let was = status.state;
        let links_now = reason == LINKS && status.detail.as_deref() != Some(LINKS);
        if was == "stale" && !links_now {
            return;
        }
        *status = Status { state: "stale", detail: Some(reason.into()) };
        // An error before this was reported already.
        let tell = (was == "watching" || was == "stale") && self.target.attached();
        drop(status);
        if tell {
            self.sink.stale(&self.drive, reason);
        }
    }
}

// ---- Windows handles --------------------------------------------------------

/// A manual-reset event, closed when dropped. Event handles work from any thread.
struct Event(HANDLE);

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn new() -> io::Result<Event> {
        let h = unsafe { CreateEventW(null(), 1, 0, null()) };
        if h.is_null() { Err(io::Error::last_os_error()) } else { Ok(Event(h)) }
    }

    fn set(&self) {
        unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The watched root, opened for change notifications, with its one
/// overlapped read.
struct Root {
    dir: HANDLE,
    io: Event,
    /// Boxed, like the buffer: the kernel writes to both until the read ends.
    ov: Box<OVERLAPPED>,
    /// u32s, because the API wants the buffer DWORD-aligned.
    buf: Box<[u32]>,
    /// Bytes in `buf`.
    len: usize,
    pending: bool,
    pnp: Pnp,
}

// The handles are only ever used by the thread that owns the Root.
unsafe impl Send for Root {}

/// What a read that ended brought (`Root::finish`).
enum Got {
    Changes(Vec<(u32, String)>),
    /// Windows dropped changes because more came than the buffers hold.
    Lost,
    /// The read was cancelled (ERROR_OPERATION_ABORTED). Windows does that
    /// to the reads a thread started when the thread ends, and anyone may
    /// cancel them (CancelIoEx); the watcher itself only when it closes the root.
    Cancelled,
}

impl Root {
    /// `len`: bytes of the notification buffer, a multiple of 4. Starts no
    /// read: a read must be started by the thread that waits for it (`read`).
    fn open(path: &str, len: usize) -> io::Result<Root> {
        let io = Event::new()?;
        let dir = unsafe {
            CreateFileW(
                win::wide(path).as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if dir == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // Without this, the open root would make "safely remove" fail.
        let pnp = match Pnp::register(dir) {
            Ok(p) => p,
            Err(e) => {
                unsafe { CloseHandle(dir) };
                return Err(e);
            }
        };
        Ok(Root {
            dir,
            io,
            ov: Box::new(unsafe { std::mem::zeroed() }),
            buf: vec![0u32; len / 4].into_boxed_slice(),
            len: len / 4 * 4,
            pending: false,
            pnp,
        })
    }

    /// Starts the next read of changes anywhere below the root. Only on the
    /// watcher's thread: Windows cancels it if the thread that started it
    /// ends first (the handle is not tied to a completion port).
    fn read(&mut self) -> io::Result<()> {
        unsafe { ResetEvent(self.io.0) };
        *self.ov = unsafe { std::mem::zeroed() };
        self.ov.hEvent = self.io.0;
        let ok = unsafe {
            ReadDirectoryChangesW(self.dir, self.buf.as_mut_ptr().cast(), self.len as u32, 1, FILTER, null_mut(), &mut *self.ov, None)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        self.pending = true;
        Ok(())
    }

    /// Ends the read that completed and returns what it brought.
    fn finish(&mut self) -> io::Result<Got> {
        self.pending = false;
        let mut n = 0u32;
        if unsafe { GetOverlappedResult(self.dir, &*self.ov, &mut n, 0) } == 0 {
            return match unsafe { GetLastError() } {
                ERROR_NOTIFY_ENUM_DIR => Ok(Got::Lost),
                ERROR_OPERATION_ABORTED => Ok(Got::Cancelled),
                e => Err(io::Error::from_raw_os_error(e as i32)),
            };
        }
        if n == 0 {
            return Ok(Got::Lost);
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), (n as usize).min(self.len)) };
        Ok(Got::Changes(parse(bytes)))
    }
}

/// Reads cancelled by something other than the watcher, in a row
/// (`Got::Cancelled`).
#[derive(Default)]
struct Cancels {
    count: u32,
    last: Option<Instant>,
}

impl Cancels {
    /// One more at `now`. True once they keep coming straight after one
    /// another (MAX_CANCELS, each within CANCEL_BURST of the one before):
    /// then starting the read again does not help, and it is an error. The
    /// caller starts afresh after a read that gets through.
    fn again(&mut self, now: Instant) -> bool {
        let soon = self.last.is_some_and(|t| now.saturating_duration_since(t) < CANCEL_BURST);
        self.count = if soon { self.count + 1 } else { 1 };
        self.last = Some(now);
        self.count >= MAX_CANCELS
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        unsafe {
            if self.pending {
                CancelIoEx(self.dir, &*self.ov);
                // The kernel may write to ov and buf until the cancelled read ends.
                let mut n = 0u32;
                GetOverlappedResult(self.dir, &*self.ov, &mut n, 1);
            }
            CloseHandle(self.dir);
        }
        // The root is closed: an eject waiting for it can go ahead. `pnp`
        // is dropped after this, which unregisters.
        self.pnp.events.released.set();
    }
}

/// Windows' notice that the drive under a handle is about to be ejected
/// (or locked, e.g. for formatting), so the watcher closes the root in time.
struct Pnp {
    handle: HCMNOTIFICATION,
    /// The callback's own reference to `events`.
    context: *const PnpEvents,
    events: Arc<PnpEvents>,
}

struct PnpEvents {
    /// Set by the callback: close the root.
    release: Event,
    /// Set by the watcher once it has.
    released: Event,
}

impl Pnp {
    fn register(dir: HANDLE) -> io::Result<Pnp> {
        let events = Arc::new(PnpEvents { release: Event::new()?, released: Event::new()? });
        let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
        filter.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
        filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE;
        filter.u.DeviceHandle = CM_NOTIFY_FILTER_0_0 { hTarget: dir };
        let context = Arc::into_raw(events.clone());
        let mut handle: HCMNOTIFICATION = null_mut();
        let cr = unsafe { CM_Register_Notification(&filter, context.cast(), Some(on_device), &mut handle) };
        if cr != CR_SUCCESS {
            drop(unsafe { Arc::from_raw(context) });
            return Err(io::Error::other(format!("장치 알림 등록 실패 (CONFIGRET {cr})")));
        }
        Ok(Pnp { handle, context, events })
    }
}

impl Drop for Pnp {
    fn drop(&mut self) {
        unsafe {
            // Waits for a callback that is still running; only then may its context go.
            CM_Unregister_Notification(self.handle);
            drop(Arc::from_raw(self.context));
        }
    }
}

/// Runs on a system thread. The eject or lock goes ahead once the watcher
/// has closed the root, or after a few seconds if it is busy.
unsafe extern "system" fn on_device(
    _: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    _: u32,
) -> u32 {
    let leaving = match action {
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE | CM_NOTIFY_ACTION_DEVICEREMOVEPENDING | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE => true,
        CM_NOTIFY_ACTION_DEVICECUSTOMEVENT if !data.is_null() => {
            let g = unsafe { (*data).u.DeviceHandle.EventGuid };
            let l = GUID_IO_VOLUME_LOCK;
            (g.data1, g.data2, g.data3, g.data4) == (l.data1, l.data2, l.data3, l.data4)
        }
        _ => false,
    };
    if leaving {
        let events = unsafe { &*context.cast::<PnpEvents>() };
        events.release.set();
        unsafe { WaitForSingleObject(events.released.0, RELEASE_WAIT_MS) };
    }
    ERROR_SUCCESS
}

// ---- notifications ----------------------------------------------------------

/// Reads FILE_NOTIFY_INFORMATION records: (action, name relative to the
/// watched folder). Every offset and length is checked against the buffer.
fn parse(buf: &[u8]) -> Vec<(u32, String)> {
    let u32_at = |at: usize| buf.get(at..at.checked_add(4)?).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    let mut at = 0usize;
    while let (Some(next), Some(action), Some(len)) = (u32_at(at), u32_at(at + 4), u32_at(at + 8)) {
        let start = at + 12;
        let Some(name) = start.checked_add(len as usize).and_then(|end| buf.get(start..end)) else { break };
        let units: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        if !units.is_empty() {
            out.push((action, String::from_utf16_lossy(&units)));
        }
        if next == 0 {
            break;
        }
        at = match at.checked_add(next as usize) {
            Some(a) => a,
            None => break,
        };
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
enum Change {
    Added(String),
    Removed(String),
    Modified(String),
    Renamed(String, String),
}

/// Turns notifications into changes with absolute paths, pairing each
/// RENAMED_OLD_NAME with the RENAMED_NEW_NAME after it (`old` carries an
/// unpaired old name over to the next read). Paths in `ignore` are dropped;
/// a rename across its edge becomes a plain add or remove.
fn collect(root: &str, ignore: Option<&str>, raw: Vec<(u32, String)>, old: &mut Option<String>, out: &mut Vec<Change>) {
    for (action, name) in raw {
        let path = long_name(root, format!("{root}{name}"));
        let ignored = is_ignored(&path, ignore);
        if action != FILE_ACTION_RENAMED_NEW_NAME {
            unpaired(old, ignore, out);
        }
        match action {
            FILE_ACTION_ADDED if !ignored => out.push(Change::Added(path)),
            FILE_ACTION_REMOVED if !ignored => out.push(Change::Removed(path)),
            FILE_ACTION_MODIFIED if !ignored => out.push(Change::Modified(path)),
            FILE_ACTION_RENAMED_OLD_NAME => *old = Some(path),
            FILE_ACTION_RENAMED_NEW_NAME => match (old.take().filter(|o| !is_ignored(o, ignore)), ignored) {
                (Some(o), false) => out.push(Change::Renamed(o, path)),
                (Some(o), true) => out.push(Change::Removed(o)),
                (None, false) => out.push(Change::Added(path)),
                (None, true) => {}
            },
            _ => {}
        }
    }
}

/// An old name whose new name will not come (it left for somewhere we
/// cannot see, or its record was lost): the entry left that place.
fn unpaired(old: &mut Option<String>, ignore: Option<&str>, out: &mut Vec<Change>) {
    if let Some(o) = old.take().filter(|o| !is_ignored(o, ignore)) {
        out.push(Change::Removed(o));
    }
}

/// Drops the Modified changes in `batch[from..]` whose path the batch has
/// as Modified already. `apply` reads each modified path once, after the
/// rest, so the copies only take room, which counts while a rescan runs.
fn keep_once(batch: &mut Vec<Change>, from: usize, modified: &mut HashSet<String>) {
    let new = batch.split_off(from);
    batch.extend(new.into_iter().filter(|c| match c {
        Change::Modified(p) => modified.insert(p.clone()),
        _ => true,
    }));
}

/// Takes the first CHUNK changes off `batch`, to apply now, and forgets
/// their Modified paths in `modified`, which lists those `batch` still has.
/// Every change is checked against the disk when it is applied, so a burst
/// cut in two ends in the same state as one applied whole.
fn next_chunk(batch: &mut Vec<Change>, modified: &mut HashSet<String>) -> Vec<Change> {
    let rest = batch.split_off(batch.len().min(CHUNK));
    let now = std::mem::replace(batch, rest);
    for c in &now {
        if let Change::Modified(p) = c {
            modified.remove(p);
        }
    }
    now
}

/// Windows reports a change under the name it was made through, which can
/// be an 8.3 alias (`PROGRA~1`). The index holds long names only, and an
/// alias taken as new would add a second copy of the folder, so a path gets
/// its long name from the deepest part that still exists: a removed file in
/// `C:\Users\FIRSTN~1\Temp` keeps its own name under the long folder names.
/// A leaf that is itself an alias of something gone keeps the alias (see
/// `Apply::unknown_gone`).
fn long_name(root: &str, path: String) -> String {
    if !path[root.len()..].contains('~') {
        return path;
    }
    let mut head = path.len();
    loop {
        let part = &path[..head];
        let long = win::long_path(part);
        let same_root = long.get(..root.len()).is_some_and(|r| r.eq_ignore_ascii_case(root));
        if same_root && long != part {
            return format!("{long}{}", &path[head..]);
        }
        // Not there (or nothing to change): one level up, while an alias is left there.
        match part.rfind('\\') {
            Some(cut) if cut >= root.len() && path[root.len()..cut].contains('~') => head = cut,
            _ => return path,
        }
    }
}

/// Whether a name has the form of an 8.3 alias (`PROGRA~1`, `LONGFI~2.TXT`).
fn is_alias(name: &str) -> bool {
    let len = |s: &str| s.chars().count();
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let Some((base, number)) = stem.rsplit_once('~') else { return false };
    (1..=6).contains(&len(base))
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && len(stem) <= 8
        && len(ext) <= 3
        && !stem.contains(['.', ' '])
}

// ---- applying a batch -------------------------------------------------------

/// What the disk says about a path now.
#[derive(Clone, Copy)]
enum Disk {
    Missing,
    /// It could not be looked at (access denied, delete pending, ...).
    Unknown,
    Found(Meta),
}

/// Never follows a link: a junction is recorded as itself.
fn stat(path: &str, unit: u64) -> Disk {
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            let attrs = m.file_attributes();
            let tag = walk::folder_tag(attrs, m.file_type().is_symlink(), || path.into());
            let flags = walk::flags_of(attrs, tag);
            let size = if flags & FLAG_DIR != 0 { 0 } else { m.file_size() };
            Disk::Found(Meta { flags, size, alloc: alloc_of(size, unit), modified: m.last_write_time() as i64 })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Disk::Missing,
        Err(_) => Disk::Unknown,
    }
}

fn alloc_of(size: u64, unit: u64) -> u64 {
    if unit == 0 { size } else { size.div_ceil(unit).saturating_mul(unit) }
}

/// A folder that holds its own contents (a plain one, or a cloud-files
/// placeholder), as opposed to a file or a junction, folder link or other
/// reparse point.
fn is_plain_dir(m: Meta) -> bool {
    m.flags & FLAG_DIR != 0 && !is_link(m.flags)
}

/// A junction, symlink or other reparse point not known to hold its own
/// contents (only cloud-files placeholders are).
fn is_link(flags: u32) -> bool {
    flags & (FLAG_REPARSE | FLAG_NOT_LINK) == FLAG_REPARSE
}

/// `C:\a\b` → (`C:\a`, `b`).
fn split(path: &str) -> Option<(&str, &str)> {
    path.rsplit_once('\\').filter(|(_, name)| !name.is_empty())
}

/// Whether `path` lies strictly inside folder `dir` (ASCII case ignored).
fn is_inside(path: &str, dir: &str) -> bool {
    path.len() > dir.len() + 1 && path.as_bytes()[dir.len()] == b'\\' && path.get(..dir.len()).is_some_and(|p| p.eq_ignore_ascii_case(dir))
}

/// Whether `path` lies strictly inside one of `dirs`, which holds folder
/// paths ASCII-lowercased (`is_inside` for each, by looking up every folder
/// above `path` instead of comparing with every one of `dirs`).
fn inside_any(dirs: &HashSet<String>, path: &str) -> bool {
    if dirs.is_empty() {
        return false;
    }
    let p = path.to_ascii_lowercase();
    p.match_indices('\\').any(|(at, _)| at + 1 < p.len() && dirs.contains(&p[..at]))
}

/// Disk state of the batch's paths and the contents of new folders, read
/// before the index is locked.
struct Prepared {
    disk: HashMap<String, Disk>,
    /// New folders (none inside another) with everything below them. Each
    /// walk ran after every change in the batch had happened, so changes
    /// inside these folders are already in it.
    walks: Vec<(String, Vec<RawEntry>)>,
    /// Lowercase names now in each folder (by lowercase path) where
    /// something went away under what looks like an 8.3 alias.
    listings: HashMap<String, HashSet<String>>,
}

fn prepare(changes: &[Change], unit: u64) -> Prepared {
    let mut disk = HashMap::new();
    // New folders as reported, and by lowercase path (`inside_any`).
    let mut roots: Vec<String> = Vec::new();
    let mut lower: HashSet<String> = HashSet::new();
    let look = |p: &str, disk: &mut HashMap<String, Disk>| *disk.entry(p.to_string()).or_insert_with(|| stat(p, unit));
    for c in changes {
        let p = match c {
            Change::Added(p) | Change::Removed(p) | Change::Modified(p) | Change::Renamed(_, p) => p,
        };
        if inside_any(&lower, p) {
            continue;
        }
        let d = look(p, &mut disk);
        if let (Change::Added(_), Disk::Found(m)) = (c, d)
            && is_plain_dir(m)
            && lower.insert(p.to_ascii_lowercase())
        {
            roots.push(p.clone());
        }
    }
    // A folder that came after one inside it: only the outer one is walked.
    roots.retain(|r| !inside_any(&lower, r));
    let walks = roots
        .into_iter()
        .map(|r| {
            let mut below = walk::scan(&format!("{r}\\"));
            for e in &mut below {
                e.alloc = alloc_of(e.size, unit);
            }
            (r, below)
        })
        .collect();
    // Read after the stats: an entry missing here is gone.
    let mut listings = HashMap::new();
    for c in changes {
        let (Change::Removed(p) | Change::Renamed(p, _)) = c else { continue };
        let Some((dir, leaf)) = split(p) else { continue };
        let key = dir.to_lowercase();
        if !is_alias(leaf) || listings.contains_key(&key) {
            continue;
        }
        if let Ok(read) = std::fs::read_dir(format!("{dir}\\")) {
            let names: HashSet<String> = read.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect();
            listings.insert(key, names);
        }
    }
    Prepared { disk, walks, listings }
}

struct Applied {
    dirs: Vec<String>,
    full: bool,
    /// Folders removed, or renamed or moved away, by their old paths; none
    /// lies inside another.
    removed: Vec<String>,
    /// New folders that went in without their contents; walked next batch.
    later: Vec<Change>,
    /// A counted hard link was removed: its file may still be on disk
    /// through another name, which the totals leave out.
    links_lost: bool,
}

/// Applies changes in order, each checked against what the disk shows now,
/// so a burst such as "create temp, write, rename over the old file" ends
/// in the right state even though its middle steps are long gone.
fn apply(ix: &mut Index, changes: &[Change], prep: Prepared, unit: u64) -> Applied {
    let roots: HashSet<String> = prep.walks.iter().map(|(p, _)| p.to_ascii_lowercase()).collect();
    let mut walks: HashMap<String, Vec<RawEntry>> = prep.walks.into_iter().collect();
    let links = ix.linked_removed();
    let mut a = Apply {
        ix,
        disk: prep.disk,
        listings: prep.listings,
        unit,
        roots,
        touched: HashSet::new(),
        later: Vec::new(),
        gone: Vec::new(),
        prune: Vec::new(),
    };
    // A changed size or time is read from disk anyway, so once per path is
    // enough, after the batch has put every entry where it now is.
    let mut modified: Vec<&str> = Vec::new();
    let mut seen = HashSet::new();
    for c in changes {
        match c {
            Change::Added(p) if !a.is_new(p) => match walks.remove(p) {
                Some(below) => a.place(p, below),
                None => a.upsert(p),
            },
            Change::Removed(p) if !a.is_new(p) => a.removed(p),
            Change::Modified(p) if seen.insert(p.as_str()) => modified.push(p),
            Change::Renamed(o, n) => a.renamed(o, n),
            _ => {}
        }
    }
    for p in modified {
        if !a.is_new(p) {
            a.upsert(p);
        }
    }
    // Also last, once every entry is where the batch put it.
    for dir in std::mem::take(&mut a.prune) {
        a.prune_gone(&dir);
    }
    let (removed, too_many) = outermost(std::mem::take(&mut a.gone));
    let full = a.touched.len() > MAX_DIRS || too_many;
    let mut dirs: Vec<String> = if full { Vec::new() } else { a.touched.iter().map(|&n| a.ix.path_of(n)).collect() };
    dirs.sort();
    dirs.dedup();
    Applied { dirs, full, removed, later: a.later, links_lost: a.ix.linked_removed() != links }
}

/// The paths that lie inside no other one, sorted, and whether there were
/// more than MAX_DIRS of them (only that many are kept).
fn outermost(mut paths: Vec<String>) -> (Vec<String>, bool) {
    // Shorter first, so a folder comes before what was inside it.
    paths.sort_by_key(|p| p.len());
    let mut kept: Vec<String> = Vec::new();
    let mut too_many = false;
    for p in paths {
        if kept.iter().any(|k| k.eq_ignore_ascii_case(&p) || is_inside(&p, k)) {
            continue;
        }
        if kept.len() == MAX_DIRS {
            too_many = true;
            break;
        }
        kept.push(p);
    }
    kept.sort();
    (kept, too_many)
}

struct Apply<'a> {
    ix: &'a mut Index,
    disk: HashMap<String, Disk>,
    listings: HashMap<String, HashSet<String>>,
    unit: u64,
    /// Folders walked for this batch, ASCII-lowercased (`inside_any`).
    roots: HashSet<String>,
    /// Folders whose listing changed.
    touched: HashSet<u32>,
    later: Vec<Change>,
    /// Old paths of folders removed or moved away.
    gone: Vec<String>,
    /// Folders to check against their listing at the end (`unknown_gone`).
    prune: Vec<String>,
}

impl Apply<'_> {
    fn look(&mut self, p: &str) -> Disk {
        let unit = self.unit;
        *self.disk.entry(p.to_string()).or_insert_with(|| stat(p, unit))
    }

    /// Inside a folder walked for this batch, which already shows it.
    fn is_new(&self, p: &str) -> bool {
        inside_any(&self.roots, p)
    }

    fn touch(&mut self, n: u32) {
        self.touched.insert(n);
    }

    /// A count or size changed in `n`, so every folder up to the root lists
    /// a new value.
    fn touch_up(&mut self, mut n: u32) {
        for _ in 0..self.ix.nodes.len() {
            self.touched.insert(n);
            if n == ROOT {
                break;
            }
            n = self.ix.nodes[n as usize].parent;
        }
    }

    /// The folder at `path`, adding it (and folders above it) when the disk
    /// has it and the index does not; its other contents follow with a walk
    /// next batch. None for a file, for something not on disk, and for a
    /// junction or folder link whose contents the index does not hold
    /// (those are listed live, which a partial list would break).
    fn folder(&mut self, path: &str) -> Option<u32> {
        if let Some(n) = self.ix.locate(path) {
            let flags = self.ix.nodes[n as usize].flags;
            let untracked_link = is_link(flags) && self.ix.children_of(n).next().is_none();
            return (flags & FLAG_DIR != 0 && !untracked_link).then_some(n);
        }
        let (parent_path, name) = split(path)?;
        let Disk::Found(meta) = self.look(path) else { return None };
        if !is_plain_dir(meta) {
            return None;
        }
        let parent = self.folder(parent_path)?;
        let n = self.ix.insert(parent, name, meta)?;
        self.touch_up(parent);
        self.later.push(Change::Added(path.to_string()));
        Some(n)
    }

    /// Brings the entry at `p` in line with the disk: updates it, or adds it
    /// when the index does not have it yet.
    fn upsert(&mut self, p: &str) {
        let Disk::Found(meta) = self.look(p) else { return };
        let Some((parent_path, name)) = split(p) else { return };
        let Some(parent) = self.folder(parent_path) else { return };
        let known = self.ix.locate_child(parent, name);
        match known {
            Some(n) if (self.ix.nodes[n as usize].flags ^ meta.flags) & FLAG_DIR == 0 => {
                let before = self.ix.nodes[n as usize];
                let renamed = self.ix.name(n) != name && self.ix.rename(n, name);
                if self.ix.update(n, meta) || renamed {
                    let after = self.ix.nodes[n as usize];
                    if (after.size, after.alloc) != (before.size, before.alloc) {
                        self.touch_up(parent);
                    } else {
                        self.touch(parent);
                    }
                }
            }
            // New, or a folder where a file was (or the other way round).
            _ => {
                if let Some(n) = known
                    && self.ix.nodes[n as usize].flags & FLAG_DIR != 0
                {
                    // What was in the folder went with it.
                    self.gone.push(self.ix.path_of(n));
                }
                if self.ix.insert(parent, name, meta).is_some() {
                    self.touch_up(parent);
                    if is_plain_dir(meta) {
                        self.later.push(Change::Added(p.to_string()));
                    }
                }
            }
        }
    }

    /// A new folder and everything a walk found in it.
    fn place(&mut self, p: &str, below: Vec<RawEntry>) {
        let Disk::Found(meta) = self.look(p) else { return };
        let Some((parent_path, name)) = split(p) else { return };
        let Some(parent) = self.folder(parent_path) else { return };
        if let Some(top) = self.ix.insert_tree(parent, name, meta, below, walk::ROOT_ID) {
            self.touch_up(parent);
            self.touch(top);
        }
    }

    fn removed(&mut self, p: &str) {
        // Back again already; its ADDED comes in this batch or the next one.
        if matches!(self.look(p), Disk::Found(_)) {
            return;
        }
        match self.ix.locate(p) {
            Some(n) => self.drop_node(n),
            None => self.unknown_gone(p),
        }
    }

    /// A path the index does not know went away. When it was an 8.3 alias
    /// that no longer resolves, it named an entry the index has by its long
    /// name, so that folder is checked against its listing after the batch.
    fn unknown_gone(&mut self, p: &str) {
        if let Some((dir, _)) = split(p)
            && self.listings.contains_key(&dir.to_lowercase())
            && !self.prune.iter().any(|d| d == dir)
        {
            self.prune.push(dir.to_string());
        }
    }

    /// Drops the entries of folder `dir` that its listing (read after the
    /// batch's changes had happened) does not have.
    fn prune_gone(&mut self, dir: &str) {
        if self.is_new(dir) {
            return;
        }
        let Some(parent) = self.ix.locate(dir) else { return };
        let Some(names) = self.listings.get(&dir.to_lowercase()) else { return };
        let gone: Vec<u32> = self
            .ix
            .children_of(parent)
            .filter(|&c| self.ix.nodes[c as usize].flags & FLAG_DELETED == 0 && !names.contains(&self.ix.name(c).to_lowercase()))
            .collect();
        for n in gone {
            self.drop_node(n);
        }
    }

    fn drop_node(&mut self, n: u32) {
        let parent = self.ix.nodes[n as usize].parent;
        if self.ix.nodes[n as usize].flags & FLAG_DIR != 0 {
            // Anyone looking at the folder itself sees it go.
            self.touch(n);
            self.gone.push(self.ix.path_of(n));
        }
        self.ix.remove(n);
        self.touch_up(parent);
    }

    fn renamed(&mut self, old: &str, new: &str) {
        match (self.is_new(old), self.is_new(new)) {
            (true, true) => {}
            // Out of a new folder: the walk did not see it, so add it as it is now.
            (true, false) => self.upsert(new),
            // Into a new folder, whose walk has it.
            (false, true) => match self.ix.locate(old) {
                Some(n) => self.drop_node(n),
                None => self.unknown_gone(old),
            },
            (false, false) => match self.ix.locate(old) {
                Some(n) => self.moved(n, new),
                // Never known (or renamed by this app already): take it as it is now.
                None => {
                    self.upsert(new);
                    self.unknown_gone(old);
                }
            },
        }
    }

    /// Re-parents a known entry, keeping what is below it.
    fn moved(&mut self, n: u32, new: &str) {
        let target = split(new).and_then(|(parent_path, name)| Some((self.folder(parent_path)?, name)));
        let Some((parent, name)) = target else {
            // Gone to a place the index cannot hold it.
            self.drop_node(n);
            return;
        };
        let from = self.ix.nodes[n as usize].parent;
        let was = (self.ix.nodes[n as usize].flags & FLAG_DIR != 0).then(|| self.ix.path_of(n));
        if !self.ix.move_to(n, parent, name) {
            self.drop_node(n);
            self.upsert(new);
            return;
        }
        // Whatever was shown at the folder's old path, or below it, moved away.
        if let Some(was) = was
            && was.to_lowercase() != new.to_lowercase()
        {
            self.gone.push(was);
        }
        self.touch_up(from);
        self.touch_up(parent);
        // Saving by rename brings a new size along.
        match self.look(new) {
            Disk::Found(meta) if (self.ix.nodes[n as usize].flags ^ meta.flags) & FLAG_DIR == 0 => {
                self.ix.update(n, meta);
            }
            Disk::Found(_) => {
                self.drop_node(n);
                self.upsert(new);
            }
            // Moved on again; the change that did it comes later.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut b = Vec::new();
        b.extend(next.to_le_bytes());
        b.extend(action.to_le_bytes());
        b.extend((units.len() as u32).to_le_bytes());
        b.extend(units);
        b
    }

    /// Records chained as Windows lays them out, each on a DWORD boundary.
    fn buffer(items: &[(u32, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, &(action, name)) in items.iter().enumerate() {
            let mut r = record(0, action, name);
            while !r.len().is_multiple_of(4) {
                r.push(0);
            }
            if i + 1 < items.len() {
                let next = r.len() as u32;
                r[0..4].copy_from_slice(&next.to_le_bytes());
            }
            out.extend(r);
        }
        out
    }

    #[test]
    fn parses_notification_records() {
        let items = [(FILE_ACTION_ADDED, "새 폴더"), (FILE_ACTION_RENAMED_OLD_NAME, "a\\old.txt"), (FILE_ACTION_RENAMED_NEW_NAME, "b\\n.txt")];
        let buf = buffer(&items);
        let want: Vec<(u32, String)> = items.iter().map(|&(a, n)| (a, n.to_string())).collect();
        assert_eq!(parse(&buf), want);
        // Cut anywhere, a buffer gives whole records only, and never panics.
        for n in 0..buf.len() {
            let got = parse(&buf[..n]);
            assert!(want.starts_with(&got), "{n}");
        }
        // An offset past the end ends the chain.
        let mut bad = buffer(&[(1, "x"), (2, "y")]);
        bad[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&bad), [(1, "x".to_string())]);
        let mut bad = buffer(&[(1, "x")]);
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse(&bad).is_empty());
    }

    #[test]
    fn pairs_renames_and_skips_our_data_folder() {
        use Change::*;
        let s = |p: &str| p.to_string();
        let raw = vec![
            (FILE_ACTION_ADDED, s("x.txt")),
            (FILE_ACTION_RENAMED_OLD_NAME, s("x.txt")),
            (FILE_ACTION_RENAMED_NEW_NAME, s("y.txt")),
            (FILE_ACTION_MODIFIED, s("Data\\App\\index\\C.idx")),
            (FILE_ACTION_MODIFIED, s("Data\\App")),
            (FILE_ACTION_MODIFIED, s("Data\\Apple")),
            (FILE_ACTION_RENAMED_OLD_NAME, s("Data\\App\\out.txt")),
            (FILE_ACTION_RENAMED_NEW_NAME, s("out.txt")),
            (FILE_ACTION_RENAMED_OLD_NAME, s("in.txt")),
            (FILE_ACTION_RENAMED_NEW_NAME, s("Data\\App\\in.txt")),
            (FILE_ACTION_RENAMED_OLD_NAME, s("lonely.txt")),
            (FILE_ACTION_REMOVED, s("z.txt")),
            (FILE_ACTION_RENAMED_OLD_NAME, s("split.txt")),
        ];
        let (mut old, mut out) = (None, Vec::new());
        collect("C:\\", Some("c:\\data\\app"), raw, &mut old, &mut out);
        assert_eq!(
            out,
            [
                Added(s("C:\\x.txt")),
                Renamed(s("C:\\x.txt"), s("C:\\y.txt")),
                Modified(s("C:\\Data\\Apple")),
                Added(s("C:\\out.txt")),
                Removed(s("C:\\in.txt")),
                Removed(s("C:\\lonely.txt")),
                Removed(s("C:\\z.txt")),
            ]
        );
        // A pair split across two reads still pairs.
        assert_eq!(old.as_deref(), Some("C:\\split.txt"));
        collect("C:\\", None, vec![(FILE_ACTION_RENAMED_NEW_NAME, s("joined.txt"))], &mut old, &mut out);
        assert_eq!(out.last(), Some(&Renamed(s("C:\\split.txt"), s("C:\\joined.txt"))));
        assert!(old.is_none());

        // Lost records in between: the old name left, and the next new name
        // (from another rename) is not paired with it.
        let mut out = Vec::new();
        collect("C:\\", None, vec![(FILE_ACTION_RENAMED_OLD_NAME, s("a.txt"))], &mut old, &mut out);
        unpaired(&mut old, None, &mut out);
        collect("C:\\", None, vec![(FILE_ACTION_RENAMED_NEW_NAME, s("y.txt"))], &mut old, &mut out);
        assert_eq!(out, [Removed(s("C:\\a.txt")), Added(s("C:\\y.txt"))]);
    }

    #[test]
    fn repeated_modifications_are_held_once() {
        use Change::*;
        let s = |p: &str| p.to_string();
        let mut modified = HashSet::new();
        let mut batch = vec![Modified(s("a")), Modified(s("a"))];
        // What came before `from` is left alone.
        keep_once(&mut batch, 1, &mut modified);
        assert_eq!(batch.len(), 2);
        batch.extend([Modified(s("a")), Added(s("b")), Modified(s("b")), Modified(s("b")), Removed(s("a"))]);
        keep_once(&mut batch, 2, &mut modified);
        assert_eq!(batch, [Modified(s("a")), Modified(s("a")), Added(s("b")), Modified(s("b")), Removed(s("a"))]);
    }

    #[test]
    fn alias_shapes() {
        for name in ["PROGRA~1", "LONGFI~1.TXT", "TH0A83~1.TXT", "a~12", "firstn~1"] {
            assert!(is_alias(name), "{name}");
        }
        for name in ["~DF1234.tmp", "plain.txt", "a~b", "toolongname~1", "LONGFI~1.TEXT", "a.b~1", "a b~1", "x~"] {
            assert!(!is_alias(name), "{name}");
        }
    }

    #[test]
    fn data_folder_is_matched_on_whole_names() {
        let ignore = data_dir_on("C:\\").filter(|_| crate::scan::data_dir().starts_with("C:\\"));
        if let Some(dir) = ignore.as_deref() {
            assert!(is_ignored(&format!("{dir}\\index\\C.idx"), ignore.as_deref()));
            assert!(is_ignored(&dir.to_uppercase(), ignore.as_deref()));
            assert!(!is_ignored(&format!("{dir}x\\a"), ignore.as_deref()));
        }
        assert!(data_dir_on("Q:\\").is_none());
        assert!(is_inside("C:\\a\\b", "C:\\A"));
        assert!(!is_inside("C:\\a", "C:\\a"));
        assert!(!is_inside("C:\\ab", "C:\\a"));
        let dirs = HashSet::from(["c:\\a".to_string(), "c:\\x\\é".to_string()]);
        assert!(inside_any(&dirs, "C:\\A\\b") && inside_any(&dirs, "C:\\X\\é\\deep\\f.txt"));
        for p in ["C:\\a", "C:\\a\\", "C:\\ab\\c", "C:\\x", "D:\\a\\b"] {
            assert!(!inside_any(&dirs, p), "{p}");
        }
        assert!(!inside_any(&HashSet::new(), "C:\\a\\b"));
        assert_eq!(split("C:\\a\\b.txt"), Some(("C:\\a", "b.txt")));
        assert_eq!(split("C:\\a"), Some(("C:", "a")));
        assert_eq!(alloc_of(1, 4096), 4096);
        assert_eq!(alloc_of(0, 4096), 0);
        assert_eq!(alloc_of(8193, 0), 8193);
    }

    #[test]
    fn short_names_become_long() {
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let t = Tree::new("short");
        t.write("Long Folder Name\\file.txt", 1);
        let root = format!("{}\\", t.drive);
        let long = t.path("Long Folder Name\\file.txt");
        let mut buf = vec![0u16; 1024];
        let n = unsafe { GetShortPathNameW(win::wide(&long).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        // Only the part below the stand-in root is reported by Windows.
        let short = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        let tail: Vec<&str> = short.rsplit('\\').take(2).collect();
        if n == 0 || tail.len() < 2 || !tail[1].contains('~') {
            eprintln!("this volume makes no 8.3 names; skipped");
            return;
        }
        assert_eq!(long_name(&root, format!("{root}{}\\{}", tail[1], tail[0])), long);
        // Gone, it keeps the alias: there is no long name to find.
        let gone = format!("{root}GONE~1.TXT");
        assert_eq!(long_name(&root, gone.clone()), gone);
        assert_eq!(long_name(&root, format!("{root}plain.txt")), format!("{root}plain.txt"));
        // Something gone inside a folder reported by its alias still gets the
        // folder's long name, so the index can find it.
        let folder = t.path("Long Folder Name");
        assert_eq!(long_name(&root, format!("{root}{}\\gone.txt", tail[1])), format!("{folder}\\gone.txt"));
        assert_eq!(long_name(&root, format!("{root}{}\\no\\x~1", tail[1])), format!("{folder}\\no\\x~1"));
    }

    #[test]
    fn removals_under_an_alias_leave_the_index() {
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let t = Tree::new("alias");
        t.write("dir\\Long File Name.txt", 8);
        t.write("dir\\keep.txt", 1);
        let long = t.path("dir\\Long File Name.txt");
        let mut buf = vec![0u16; 1024];
        let n = unsafe { GetShortPathNameW(win::wide(&long).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let short = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        let leaf = short.rsplit('\\').next().unwrap_or("").to_string();
        if n == 0 || !is_alias(&leaf) {
            eprintln!("this volume makes no 8.3 names; skipped");
            return;
        }
        let mut ix = t.index();
        assert_eq!(t.size(&ix, "dir"), Some(9));
        std::fs::remove_file(&long).unwrap();
        // Windows names the file by the alias it was deleted through.
        let out = run(&mut ix, vec![Change::Removed(t.path(&format!("dir\\{leaf}")))]);
        assert!(t.size(&ix, "dir\\Long File Name.txt").is_none());
        assert_eq!(t.size(&ix, "dir"), Some(1));
        assert!(out.dirs.contains(&t.path("dir")));
        // Noise about an alias that names nothing changes nothing.
        let out = run(&mut ix, vec![Change::Removed(t.path("dir\\NOPE~1.TXT"))]);
        assert_eq!(t.size(&ix, "dir"), Some(1));
        assert!(out.dirs.is_empty());
    }

    /// A temp folder that stands in for a drive: its path is the index's
    /// drive name, the way `C:` is.
    struct Tree {
        root: PathBuf,
        drive: String,
    }

    impl Tree {
        fn new(tag: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("ff-watch-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let drive = root.to_string_lossy().into_owned();
            Tree { root, drive }
        }

        fn write(&self, rel: &str, len: usize) {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![0u8; len]).unwrap();
        }

        fn path(&self, rel: &str) -> String {
            format!("{}\\{rel}", self.drive)
        }

        fn index(&self) -> Index {
            Index::build(&self.drive, "walk", 0, walk::scan(&format!("{}\\", self.drive)), walk::ROOT_ID)
        }

        fn size(&self, ix: &Index, rel: &str) -> Option<u64> {
            let path = if rel.is_empty() { self.drive.clone() } else { self.path(rel) };
            ix.find(&path).map(|n| ix.nodes[n as usize].size)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn run(ix: &mut Index, changes: Vec<Change>) -> Applied {
        let prep = prepare(&changes, 0);
        apply(ix, &changes, prep, 0)
    }

    #[test]
    fn batches_follow_the_disk() {
        use Change::*;
        let t = Tree::new("batch");
        t.write("a\\one.txt", 10);
        t.write("keep.txt", 5);
        t.write("gone.txt", 1);
        std::fs::create_dir_all(t.root.join("b")).unwrap();
        let mut ix = t.index();
        assert_eq!(t.size(&ix, ""), Some(16));

        // What happened on disk since the scan, and what Windows said about it.
        t.write("a\\two.txt", 20);
        t.write("keep.txt", 50);
        std::fs::rename(t.root.join("a\\one.txt"), t.root.join("b\\uno.txt")).unwrap();
        t.write("new\\deep\\x.bin", 7);
        t.write("new\\y.bin", 3);
        std::fs::remove_file(t.root.join("gone.txt")).unwrap();
        let out = run(
            &mut ix,
            vec![
                Added(t.path("a\\two.txt")),
                Modified(t.path("keep.txt")),
                Modified(t.path("keep.txt")),
                Renamed(t.path("a\\one.txt"), t.path("b\\uno.txt")),
                Added(t.path("new")),
                Added(t.path("new\\deep")),
                Added(t.path("new\\deep\\x.bin")),
                Added(t.path("new\\y.bin")),
                Removed(t.path("gone.txt")),
            ],
        );
        assert_eq!(t.size(&ix, ""), Some(90));
        assert_eq!(t.size(&ix, "a"), Some(20));
        assert_eq!(t.size(&ix, "b\\uno.txt"), Some(10));
        assert_eq!(t.size(&ix, "new"), Some(10));
        assert_eq!(t.size(&ix, "new\\deep\\x.bin"), Some(7));
        assert!(t.size(&ix, "gone.txt").is_none() && t.size(&ix, "a\\one.txt").is_none());
        assert_eq!(ix.search("x.bin", None, 10).0, 1);
        assert!(!out.full && out.later.is_empty());
        for dir in ["", "a", "b", "new"] {
            let path = if dir.is_empty() { format!("{}\\", t.drive) } else { t.path(dir) };
            assert!(out.dirs.contains(&path), "{path} in {:?}", out.dirs);
        }
        assert!(out.removed.is_empty());

        // Saving by rename: temp file written, then renamed over the old one.
        t.write("doc.tmp", 30);
        std::fs::rename(t.root.join("doc.tmp"), t.root.join("keep.txt")).unwrap();
        run(&mut ix, vec![Added(t.path("doc.tmp")), Removed(t.path("keep.txt")), Renamed(t.path("doc.tmp"), t.path("keep.txt"))]);
        assert_eq!(t.size(&ix, "keep.txt"), Some(30));
        assert!(t.size(&ix, "doc.tmp").is_none());

        // A folder moves with what is in it, without a walk; its old path is reported gone.
        std::fs::rename(t.root.join("new"), t.root.join("b\\new2")).unwrap();
        let out = run(&mut ix, vec![Renamed(t.path("new"), t.path("b\\new2"))]);
        assert_eq!(t.size(&ix, "b"), Some(20));
        assert_eq!(t.size(&ix, "b\\new2\\deep\\x.bin"), Some(7));
        assert_eq!(out.removed, [t.path("new")]);
        // A case-only rename leaves the path as it was.
        std::fs::rename(t.root.join("b\\new2"), t.root.join("b\\NEW2")).unwrap();
        let out = run(&mut ix, vec![Renamed(t.path("b\\new2"), t.path("b\\NEW2"))]);
        assert!(out.removed.is_empty());
        assert_eq!(t.size(&ix, "b\\NEW2\\deep\\x.bin"), Some(7));

        // A whole folder removed, inner folders first as Windows reports it.
        t.write("a\\sub\\s.txt", 0);
        run(&mut ix, vec![Added(t.path("a\\sub"))]);
        std::fs::remove_dir_all(t.root.join("a")).unwrap();
        let out = run(
            &mut ix,
            vec![Removed(t.path("a\\sub\\s.txt")), Removed(t.path("a\\sub")), Removed(t.path("a\\two.txt")), Removed(t.path("a"))],
        );
        assert!(t.size(&ix, "a").is_none());
        assert_eq!(t.size(&ix, ""), Some(50));
        assert!(out.dirs.contains(&t.path("a")));
        assert_eq!(out.removed, [t.path("a")]);

        // A folder the index never saw comes in first, then its contents.
        t.write("late\\inner\\z.txt", 4);
        let out = run(&mut ix, vec![Modified(t.path("late\\inner\\z.txt"))]);
        assert_eq!(t.size(&ix, "late\\inner\\z.txt"), Some(4));
        assert_eq!(out.later, [Added(t.path("late")), Added(t.path("late\\inner"))]);
        t.write("late\\other.txt", 6);
        let out = run(&mut ix, out.later);
        assert_eq!(t.size(&ix, "late"), Some(10));
        assert!(out.later.is_empty());

        // Paths that are not there, twice-reported adds and plain noise change nothing.
        let before = t.size(&ix, "");
        let out = run(&mut ix, vec![Added(t.path("nope\\x")), Modified(t.path("keep.txt")), Removed(t.path("nope"))]);
        assert_eq!(t.size(&ix, ""), before);
        assert!(out.dirs.is_empty() && !out.full && !out.links_lost);
    }

    #[test]
    fn big_batches_are_applied_in_parts() {
        use Change::*;
        let t = Tree::new("chunks");
        t.write("keep.txt", 1);
        let mut ix = t.index();
        t.write("new\\inner\\x.bin", 7);
        t.write("new\\y.bin", 3);
        t.write("keep.txt", 5);
        // Noise first, so a new folder and what is inside it fall into different parts.
        let mut batch: Vec<Change> = (0..CHUNK - 1).map(|i| Modified(t.path(&format!("noise{i}.txt")))).collect();
        batch.extend([
            Added(t.path("new")),
            Added(t.path("new\\inner")),
            Added(t.path("new\\inner\\x.bin")),
            Modified(t.path("keep.txt")),
            Added(t.path("new\\y.bin")),
        ]);
        let mut modified: HashSet<String> = HashSet::new();
        keep_once(&mut batch, 0, &mut modified);
        let first = next_chunk(&mut batch, &mut modified);
        assert_eq!((first.len(), batch.len()), (CHUNK, 4));
        // Only what still waits is listed, so a repeat of it is still dropped.
        assert_eq!(modified, HashSet::from([t.path("keep.txt")]));
        assert!(run(&mut ix, first).later.is_empty());
        assert_eq!(t.size(&ix, "new"), Some(10));
        let out = run(&mut ix, next_chunk(&mut batch, &mut modified));
        assert!(batch.is_empty() && modified.is_empty() && out.later.is_empty());
        assert_eq!(t.size(&ix, ""), Some(15));
        assert_eq!(t.size(&ix, "new\\inner\\x.bin"), Some(7));
        assert_eq!(ix.search("y.bin", None, 10).0, 1);
    }

    #[test]
    fn removing_a_counted_hard_link_is_flagged() {
        let t = Tree::new("links");
        t.write("one.bin", 5);
        t.write("two.bin", 6);
        let mut ix = t.index();
        let one = ix.find(&t.path("one.bin")).unwrap();
        ix.nodes[one as usize].flags |= crate::index::FLAG_LINKED;
        std::fs::remove_file(t.root.join("two.bin")).unwrap();
        assert!(!run(&mut ix, vec![Change::Removed(t.path("two.bin"))]).links_lost);
        std::fs::remove_file(t.root.join("one.bin")).unwrap();
        assert!(run(&mut ix, vec![Change::Removed(t.path("one.bin"))]).links_lost);
    }

    #[test]
    fn placeholder_folders_take_their_contents() {
        let t = Tree::new("cloud");
        std::fs::create_dir_all(t.root.join("cloud")).unwrap();
        std::fs::create_dir_all(t.root.join("link")).unwrap();
        let mut ix = t.index();
        // As an empty cloud-files folder and an empty junction look in an MFT index.
        let cloud = ix.find(&t.path("cloud")).unwrap();
        ix.nodes[cloud as usize].flags |= FLAG_REPARSE | FLAG_NOT_LINK;
        let link = ix.find(&t.path("link")).unwrap();
        ix.nodes[link as usize].flags |= FLAG_REPARSE;
        t.write("cloud\\photo.jpg", 30);
        t.write("link\\seen-through.jpg", 30);
        run(&mut ix, vec![Change::Added(t.path("cloud\\photo.jpg")), Change::Added(t.path("link\\seen-through.jpg"))]);
        assert_eq!(t.size(&ix, "cloud\\photo.jpg"), Some(30));
        assert_eq!(t.size(&ix, "cloud"), Some(30));
        assert!(t.size(&ix, "link\\seen-through.jpg").is_none());
        let meta = |flags| Meta { flags, size: 0, alloc: 0, modified: 0 };
        assert!(is_plain_dir(meta(FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK)));
        assert!(!is_plain_dir(meta(FLAG_DIR | FLAG_REPARSE)));
    }

    #[test]
    fn untracked_links_stay_listed_live() {
        let Some(root) = walk::tests::tree_with_junction("watch") else {
            eprintln!("mklink /J is not available; skipped");
            return;
        };
        let drive = root.to_string_lossy().into_owned();
        let mut ix = Index::build(&drive, "walk", 0, walk::scan(&format!("{drive}\\")), walk::ROOT_ID);
        std::fs::write(root.join("real").join("b.bin"), [0u8; 3]).unwrap();
        // Seen through the link, the new file must not become the link's only child.
        run(&mut ix, vec![Change::Added(format!("{drive}\\link\\b.bin")), Change::Added(format!("{drive}\\real\\b.bin"))]);
        let link = ix.find(&format!("{drive}\\link")).unwrap();
        assert!(ix.children_of(link).next().is_none());
        assert_eq!(ix.nodes[ROOT as usize].size, 13);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[derive(Default)]
    struct Seen {
        changed: Mutex<Vec<(String, Vec<String>, bool)>>,
        removed: Mutex<Vec<String>>,
        stale: Mutex<Vec<String>>,
    }

    impl Sink for Seen {
        fn changed(&self, drive: &str, dirs: Vec<String>, full: bool, removed: Vec<String>) {
            self.changed.lock().unwrap().push((drive.to_string(), dirs, full));
            self.removed.lock().unwrap().extend(removed);
        }
        fn stale(&self, _: &str, reason: &str) {
            self.stale.lock().unwrap().push(reason.to_string());
        }
    }

    fn wait_for(what: &str, ok: impl Fn() -> bool) {
        let t = Instant::now();
        while !ok() {
            assert!(t.elapsed() < Duration::from_secs(15), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn gone_soon(root: &Path) {
        wait_for("the watched folder to be deletable", || std::fs::remove_dir_all(root).is_ok() || !root.exists());
    }

    #[test]
    fn watches_a_folder_live() {
        let t = Tree::new("live");
        t.write("a\\one.txt", 10);
        let shared = Arc::new(RwLock::new(t.index()));
        let seen = Arc::new(Seen::default());
        let w = start(&t.drive, Some(shared.clone()), seen.clone());
        assert_eq!(w.status().state, "watching", "{:?}", w.status().detail);

        t.write("new\\deep\\x.bin", 70);
        t.write("a\\one.txt", 40);
        std::fs::rename(t.root.join("a\\one.txt"), t.root.join("moved.txt")).unwrap();
        wait_for("the changes to reach the index", || {
            let ix = shared.read().unwrap();
            t.size(&ix, "new\\deep\\x.bin") == Some(70) && t.size(&ix, "moved.txt") == Some(40) && t.size(&ix, "") == Some(110)
        });
        assert!(t.size(&shared.read().unwrap(), "a\\one.txt").is_none());
        let root_dir = format!("{}\\", t.drive);
        assert!(seen.changed.lock().unwrap().iter().any(|(d, dirs, _)| d == &t.drive && dirs.contains(&root_dir)));
        assert!(seen.stale.lock().unwrap().is_empty());
        assert_eq!(w.status().state, "watching");

        // Stopping closes the root without waiting on anything.
        let stopped = Instant::now();
        drop(w);
        assert!(stopped.elapsed() < Duration::from_millis(100));
        gone_soon(&t.root);
    }

    #[test]
    fn a_watcher_started_from_a_thread_that_ends_keeps_watching() {
        let t = Tree::new("pooled");
        let shared = Arc::new(RwLock::new(t.index()));
        let seen = Arc::new(Seen::default());
        // Started the way lib.rs starts them: from a pooled thread of the async
        // runtime (`blocking`), which ends once it has been idle a while. Windows
        // cancels the reads a thread started when it ends.
        let (drive, ix, sink) = (t.drive.clone(), shared.clone(), seen.clone());
        let w = std::thread::spawn(move || start(&drive, Some(ix), sink)).join().unwrap();
        assert_eq!(w.status().state, "watching", "{:?}", w.status().detail);
        // A quiet drive: nothing changes for a while after that thread is gone.
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(w.status().state, "watching", "{:?}", w.status().detail);
        t.write("later.txt", 7);
        wait_for("a change after the starting thread ended", || shared.read().unwrap().find(&t.path("later.txt")).is_some());
        assert_eq!(w.status().state, "watching", "{:?}", w.status().detail);
        assert!(seen.stale.lock().unwrap().is_empty(), "{:?}", seen.stale.lock().unwrap());
        drop(w);
        gone_soon(&t.root);
    }

    #[test]
    fn a_root_that_will_not_open_is_an_error() {
        let t = Tree::new("missing");
        let shared = Arc::new(RwLock::new(t.index()));
        std::fs::remove_dir_all(&t.root).unwrap();
        let seen = Arc::new(Seen::default());
        let w = start(&t.drive, Some(shared.clone()), seen.clone());
        assert_eq!(w.status().state, "error");
        assert!(w.status().detail.is_some());
        assert_eq!(seen.stale.lock().unwrap().len(), 1);
        // Started ahead of a rescan, it says so only once the new index is in.
        let seen = Arc::new(Seen::default());
        let w = start(&t.drive, None, seen.clone());
        assert_eq!(w.status().state, "error");
        assert!(seen.stale.lock().unwrap().is_empty());
        w.attach(shared);
        assert_eq!(seen.stale.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_rescan_keeps_what_changed_while_it_ran() {
        let t = Tree::new("rescan");
        t.write("a.txt", 10);
        t.write("dir\\b.txt", 1);
        let seen = Arc::new(Seen::default());
        // Started before the scan, as `scan_drive` does.
        let w = start(&t.drive, None, seen.clone());
        assert_eq!(w.status().state, "watching", "{:?}", w.status().detail);
        let scanned = t.index();
        // Changed after the scan read the disk, before its index is in place.
        t.write("during.txt", 5);
        std::fs::remove_file(t.root.join("a.txt")).unwrap();
        std::fs::rename(t.root.join("dir"), t.root.join("moved")).unwrap();
        for _ in 0..50 {
            t.write("busy.log", 3);
        }
        std::thread::sleep(COALESCE * 3);
        assert!(seen.changed.lock().unwrap().is_empty());
        let shared = Arc::new(RwLock::new(scanned));
        w.attach(shared.clone());
        wait_for("the held changes to reach the new index", || {
            let ix = shared.read().unwrap();
            t.size(&ix, "during.txt") == Some(5)
                && t.size(&ix, "a.txt").is_none()
                && t.size(&ix, "moved\\b.txt") == Some(1)
                && t.size(&ix, "busy.log") == Some(3)
                && t.size(&ix, "") == Some(9)
        });
        assert!(seen.removed.lock().unwrap().contains(&t.path("dir")));
        assert!(seen.stale.lock().unwrap().is_empty());
        assert_eq!(w.status().state, "watching");
        // And it goes on from there like any watcher.
        t.write("after.txt", 2);
        wait_for("a later change", || shared.read().unwrap().find(&t.path("after.txt")).is_some());
        drop(w);
        gone_soon(&t.root);
    }

    #[test]
    fn only_missed_changes_call_for_a_sync() {
        let status = |state, detail: &str| Status { state, detail: Some(detail.into()) };
        assert!(status("stale", LOST).sync_helps());
        assert!(status("stale", PAUSED).sync_helps());
        assert!(status("stale", REOPENED).sync_helps());
        assert!(!status("stale", LINKS).sync_helps());
        assert!(!status("error", BROKEN).sync_helps());
        assert!(!status("off", GONE).sync_helps());
        assert!(!Status { state: "watching", detail: None }.sync_helps());
        assert!(status("stale", LINKS).links_lost() && !status("stale", LOST).links_lost());
    }

    /// A watcher thread's work for `drive`, applying to `shared` and telling
    /// `seen`, not running yet.
    fn worker(drive: &str, shared: Arc<RwLock<Index>>, seen: Arc<Seen>) -> Worker {
        let target = Arc::new(Target { index: Mutex::new(None), ready: Event::new().unwrap(), cluster: 0 });
        target.set(shared, 0);
        Worker {
            drive: drive.into(),
            root: format!("{drive}\\"),
            buf: BUF_LOCAL,
            target,
            sink: seen,
            status: Arc::new(Mutex::new(Status { state: "watching", detail: None })),
            links: Arc::default(),
            stop: Arc::new(Event::new().unwrap()),
            ignore: None,
            volume: None,
        }
    }

    #[test]
    fn windows_cancels_a_read_when_its_thread_ends() {
        let t = Tree::new("thread-end");
        let root = Root::open(&format!("{}\\", t.drive), BUF_LOCAL).unwrap();
        assert!(!root.pending);
        // What `start` used to do: the first read, started by the calling thread.
        let mut root = std::thread::spawn(move || {
            let mut root = root;
            root.read().unwrap();
            root
        })
        .join()
        .unwrap();
        assert_eq!(unsafe { WaitForSingleObject(root.io.0, 5000) }, WAIT_OBJECT_0);
        // Told apart from an error: the watcher starts it again.
        assert!(matches!(root.finish(), Ok(Got::Cancelled)));
        drop(root);
        gone_soon(&t.root);
    }

    /// Another handle to what `h` has open, closed when dropped.
    struct Dup(HANDLE);

    impl Drop for Dup {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn duplicate(h: HANDLE) -> Dup {
        use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut out: HANDLE = null_mut();
        let ok = unsafe { DuplicateHandle(GetCurrentProcess(), h, GetCurrentProcess(), &mut out, 0, 0, DUPLICATE_SAME_ACCESS) };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        Dup(out)
    }

    #[test]
    fn cancelled_reads_start_again_and_a_reconnect_is_stale() {
        let t = Tree::new("cancel");
        let shared = Arc::new(RwLock::new(t.index()));
        let seen = Arc::new(Seen::default());
        let w = worker(&t.drive, shared.clone(), seen.clone());
        let (status, stop) = (w.status.clone(), w.stop.clone());
        let now = || status.lock().unwrap().clone();
        let root = Root::open(&format!("{}\\", t.drive), BUF_LOCAL).unwrap();
        // Cancelling through another handle to the open root cancels the
        // watcher's read, and this one stays valid after the watcher closes its own.
        let other = duplicate(root.dir);
        let cancel = || unsafe { CancelIoEx(other.0, null()) } != 0;
        let (started, first_read) = sync_channel(1);
        let thread = std::thread::spawn(move || w.run(root, started));
        first_read.recv().unwrap().unwrap();

        // One cancel: the read starts again, changes still come, and the drive
        // is stale for what it may have missed, which a sync puts right by itself.
        wait_for("a read to cancel", cancel);
        t.write("one.txt", 1);
        wait_for("a change after the cancel", || shared.read().unwrap().find(&t.path("one.txt")).is_some());
        assert_eq!((now().state, now().detail.as_deref()), ("stale", Some(PAUSED)));
        assert!(now().sync_helps());
        assert_eq!(*seen.stale.lock().unwrap(), [PAUSED]);

        // Cancels straight after one another are an error, and the root is opened again.
        wait_for("cancels in a row to be an error", || {
            cancel();
            now().state == "error"
        });
        assert!(now().detail.unwrap().contains("995"), "{:?}", now().detail);
        // Told although the drive was stale already, so the window stops saying
        // a sync comes by itself (none does while it is in error).
        wait_for("the error to be told", || seen.stale.lock().unwrap().len() == 2);
        let error = seen.stale.lock().unwrap()[1].clone();
        assert!(error.contains("995"), "{error}");
        // Back, it is stale, not in error: told, so the window drops the error
        // and a sync is lined up (lib.rs `Live::stale`, `auto_sync_wanted`).
        // (The status is set before the window is told.)
        wait_for("the root to be opened again", || seen.stale.lock().unwrap().len() == 3);
        assert_eq!((now().state, now().detail.as_deref()), ("stale", Some(REOPENED)));
        assert!(now().sync_helps());
        assert_eq!(*seen.stale.lock().unwrap(), [PAUSED, error.as_str(), REOPENED]);
        // Reading on its own thread from there.
        t.write("two.txt", 2);
        wait_for("a change after the reconnect", || shared.read().unwrap().find(&t.path("two.txt")).is_some());
        assert_eq!(now().state, "stale");

        stop.set();
        thread.join().unwrap();
        drop(other);
        gone_soon(&t.root);
    }

    #[test]
    fn only_cancels_that_keep_coming_are_an_error() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        // Now and then: started again every time.
        let mut c = Cancels::default();
        assert!((0..20).all(|i| !c.again(at(i * 1500))));
        // Straight after one another: the fifth is an error.
        let mut c = Cancels::default();
        let got: Vec<bool> = (0..MAX_CANCELS as u64).map(|i| c.again(at(i * 100))).collect();
        assert_eq!(got, [false, false, false, false, true]);
        // A pause starts the count again.
        let mut c = Cancels::default();
        assert!((0..4).all(|i| !c.again(at(i * 100))));
        assert!(!c.again(at(300) + CANCEL_BURST));
        assert!((1..4).all(|i| !c.again(at(300 + i * 100) + CANCEL_BURST)));
        assert!(c.again(at(700) + CANCEL_BURST));
    }

    #[test]
    fn a_lost_hard_link_outranks_missed_changes() {
        let seen = Arc::new(Seen::default());
        let w = worker("Q:", Arc::new(RwLock::new(Index::build("Q:", "mft", 0, Vec::new(), 0))), seen.clone());
        let now = || w.status.lock().unwrap().clone();
        w.mark_stale(LOST);
        w.mark_stale(PAUSED);
        // Missed changes: told once, the first reason stays.
        assert_eq!(*seen.stale.lock().unwrap(), [LOST]);
        assert!(now().sync_helps());
        // A counted link lost afterwards takes over and is told: no sync puts it right.
        w.links.store(true, Ordering::Relaxed);
        w.mark_stale(LINKS);
        assert_eq!(*seen.stale.lock().unwrap(), [LOST, LINKS]);
        assert!(now().links_lost() && !now().sync_helps());
        // No missed changes after it replace it.
        w.mark_stale(LOST);
        assert_eq!(seen.stale.lock().unwrap().len(), 2);
        // An error is told over it, and the pause after it does not replace it.
        w.report("error", "끊겼어요");
        w.mark_stale(PAUSED);
        assert!(now().links_lost());
        assert_eq!(*seen.stale.lock().unwrap(), [LOST, LINKS, "끊겼어요"]);
        // Back after an error, it is that again, told over the error.
        w.report("error", "끊겼어요");
        w.reconnected();
        assert!(now().links_lost() && !now().sync_helps());
        assert_eq!(*seen.stale.lock().unwrap(), [LOST, LINKS, "끊겼어요", "끊겼어요", LINKS]);
    }

    #[test]
    fn a_watcher_taking_over_keeps_a_lost_hard_link() {
        let t = Tree::new("keep-links");
        let shared = Arc::new(RwLock::new(t.index()));
        let seen = Arc::new(Seen::default());
        // A sync's watcher: started without an index, told before it gets one.
        let w = start(&t.drive, None, seen.clone());
        w.keep_links_lost();
        assert!(seen.stale.lock().unwrap().is_empty());
        w.attach(shared.clone());
        assert_eq!(*seen.stale.lock().unwrap(), [LINKS]);
        assert!(w.status().links_lost() && !w.status().sync_helps() && w.lost_links());
        // It still applies what changes.
        t.write("after.txt", 2);
        wait_for("a later change", || shared.read().unwrap().find(&t.path("after.txt")).is_some());
        assert!(w.status().links_lost());
        // A watcher that is off stays off.
        let off = Watcher::off(GONE);
        off.keep_links_lost();
        assert_eq!(off.status().state, "off");
        // One whose status an error replaced after the loss still has it to hand over.
        let broken = Watcher::off(GONE);
        broken.links.store(true, Ordering::Relaxed);
        *broken.status.lock().unwrap() = Status { state: "error", detail: Some(BROKEN.into()) };
        assert!(broken.lost_links() && !broken.status().links_lost());
        assert!(is_links_lost(LINKS) && !is_links_lost(LOST) && !is_links_lost(""));
        drop(w);
        gone_soon(&t.root);
    }

    #[test]
    fn local_drives_get_the_big_buffer() {
        for kind in ["fixed", "removable", "unknown"] {
            assert_eq!(buffer_for(kind), 1 << 20, "{kind}");
        }
        assert_eq!(buffer_for("network"), 64 << 10);
        let t = Tree::new("buffer");
        let root = Root::open(&format!("{}\\", t.drive), buffer_for("fixed")).unwrap();
        assert_eq!((root.len, root.buf.len() * 4), (1 << 20, 1 << 20));
        drop(root);
    }

    #[test]
    fn overflow_marks_the_drive_stale() {
        let t = Tree::new("overflow");
        let shared = Arc::new(RwLock::new(t.index()));
        let seen = Arc::new(Seen::default());
        let w = start(&t.drive, Some(shared.clone()), seen.clone());
        {
            // Holding the index keeps the watcher from reading on, so a burst
            // overflows what Windows can hold for it.
            let _busy = shared.write().unwrap();
            t.write("first.txt", 1);
            std::thread::sleep(COALESCE * 3);
            // About 310 bytes a notification: well over the 1 MiB buffer.
            let long = "x".repeat(150);
            for i in 0..6000 {
                std::fs::File::create(t.root.join(format!("{long}{i}"))).unwrap();
            }
        }
        wait_for("the overflow to be reported", || !seen.stale.lock().unwrap().is_empty());
        assert_eq!(w.status().state, "stale");
        assert_eq!(seen.stale.lock().unwrap().len(), 1);
        // What comes after the overflow still reaches the index.
        t.write("after.txt", 9);
        wait_for("a later change", || shared.read().unwrap().find(&t.path("after.txt")).is_some());
        assert_eq!(w.status().state, "stale");
        drop(w);
        gone_soon(&t.root);
    }
}
