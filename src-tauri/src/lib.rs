mod admin;
mod ai;
mod classify;
mod diff;
mod dups;
mod index;
mod mapview;
mod mcp;
mod meaning;
mod ops;
mod places;
mod plans;
mod preview;
mod programs;
mod scan;
mod shellmenu;
mod watch;
mod win;

use index::{EntryInfo, Index, FLAG_DIR, FLAG_NOT_LINK, FLAG_REPARSE};
use scan::sync;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};

pub(crate) type Shared = Arc<RwLock<Index>>;

/// Live changes go into the cache file once a drive has been quiet this long...
const SAVE_QUIET: Duration = Duration::from_secs(60);
/// ...but one drive's cache (a few hundred MB on a big disk) is not
/// rewritten more often than this...
const SAVE_GAP: Duration = Duration::from_secs(600);
/// ...and a drive that never goes quiet is still saved this often.
const SAVE_ANYWAY: Duration = Duration::from_secs(1800);
/// A failed save waits SAVE_GAP before the next try, twice that after a
/// second failure in a row, and so on up to this.
const SAVE_RETRY_MAX: Duration = Duration::from_secs(4 * 3600);
/// Free space a save leaves on the disk that holds the cache files. A full
/// disk is what brings people to this app; a save never takes its last bytes.
const SAVE_RESERVE: u64 = 256 << 20;
/// The same for the save right after a scan, which matters more: without it
/// the next start has to scan again. Still never the disk's last bytes.
const SCAN_SAVE_RESERVE: u64 = 64 << 20;
const SCAN_NOT_SAVED_FULL: &str = "디스크 공간이 부족해서 스캔 결과를 저장하지 못했어요. 다음에 켜면 다시 스캔해야 해요.";
/// How long `live_status` waits for watchers that are still starting.
const STARTING_WAIT: Duration = Duration::from_secs(3);
/// How often the thread that runs syncs by themselves looks for one to run.
const AUTO_TICK: Duration = Duration::from_secs(2);
/// How often a running sync tells how far it got.
const SYNC_PROGRESS_EVERY: Duration = Duration::from_secs(1);
/// Threads of a sync that runs by itself (in background mode: low CPU and I/O priority).
const SYNC_THREADS: usize = 4;
const SYNC_RUNNING: &str = "관리자 권한 없이 폴더를 다시 훑는 중이에요";
const SYNC_REPLACED: &str = "그사이에 이 드라이브를 다시 스캔해서 이번 동기화 결과는 쓰지 않았어요";
const SCANNING_NOW: &str = "이 드라이브를 스캔하는 중이에요. 스캔이 끝나면 최신 상태가 돼요";
const SYNCING_NOW: &str = "이 드라이브를 이미 동기화하는 중이에요. 끝나면 바로 반영돼요";
const SYNC_KEEPS_LINKS: &str =
    "하드 링크가 여러 개인 파일이 지워져 어긋난 크기는 동기화로 바로잡히지 않아요. ▾ → \"전체 다시 스캔\"으로 다시 세어 주세요.";

/// When a drive's index first and last took a live change that its cache
/// file does not have yet.
#[derive(Clone, Copy)]
struct Dirty {
    since: Instant,
    last: Instant,
}

/// A drive whose last save failed, and how many failed in a row.
#[derive(Clone, Copy)]
struct Failed {
    at: Instant,
    count: u32,
}

impl Failed {
    fn holds_off(&self, now: Instant) -> bool {
        let wait = SAVE_GAP.saturating_mul(1 << self.count.saturating_sub(1).min(8)).min(SAVE_RETRY_MAX);
        now.saturating_duration_since(self.at) < wait
    }
}

/// Whether a drive's live changes are due to be written: it has been quiet
/// a while and its file was not written just before, or it has kept changing
/// for long. Never while a failed save is waiting out its back-off.
fn save_due(d: &Dirty, saved: Option<Instant>, failed: Option<&Failed>, now: Instant) -> bool {
    if failed.is_some_and(|f| f.holds_off(now)) {
        return false;
    }
    let since = |t: Instant| now.saturating_duration_since(t);
    let gap = saved.is_none_or(|t| since(t) >= SAVE_GAP);
    (since(d.last) >= SAVE_QUIET && gap) || since(d.since) >= SAVE_ANYWAY
}

#[derive(Default)]
pub(crate) struct AppState {
    indexes: Mutex<HashMap<String, Shared>>,
    /// One lock per drive, held while that drive's cache file is written.
    save_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Drives with a background save queued that has not started yet.
    queued: Mutex<HashSet<String>>,
    /// Live-update watchers by drive. Dropping one stops it. Held while a
    /// drive's index is replaced, so its watcher changes along with it.
    watchers: Mutex<HashMap<String, watch::Watcher>>,
    /// Drives whose watcher `load_cached` is still starting.
    starting: Mutex<HashSet<String>>,
    started: Condvar,
    dirty: Mutex<HashMap<String, Dirty>>,
    /// When each drive's cache file was last written.
    saved: Mutex<HashMap<String, Instant>>,
    failed: Mutex<HashMap<String, Failed>>,
    /// Previous scans loaded for comparisons (`scan_diff`).
    prev: Mutex<Previous>,
    /// One lock per drive, held while its previous scan is read from disk,
    /// so comparisons asked for together share one load.
    prev_loads: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The volume each index in memory was made from (`win::volume_id`): a
    /// sync takes in only an index of the disk under that letter now.
    volumes: Mutex<HashMap<String, Option<String>>>,
    /// Scans and syncs running, by drive.
    jobs: Mutex<sync::Jobs>,
    /// Syncs waiting to run by themselves (`auto_sync`).
    auto: Mutex<sync::AutoSync>,
    /// For drives loaded from their cache at startup: when the app that
    /// wrote the cache was closed with the drive's watcher keeping up
    /// (FILETIME, `save_on_exit`), so the cache was what was on disk then.
    /// Taken out when the drive's startup sync is decided (`auto_sync_wanted`).
    in_step: Mutex<HashMap<String, i64>>,
}

// A sync keeps no scan to compare with (`save_synced`): only a scan from the ▾ menu does.
// The window shows this as it is, with nothing added.
const NO_PREV: &str = "이전 스캔이 없어요 — 드라이브 버튼 옆 ▾ → \"전체 다시 스캔\"으로 한 번 더 스캔하면 비교할 수 있어요 (\"동기화\"는 몇 번 해도 비교할 스캔을 남기지 않아요)";
const NO_PREV_ROOM: &str =
    "디스크 공간이 부족해서 이전 스캔을 남겨 두지 못했어요 — 공간을 비운 뒤 다시 스캔하면 그다음 스캔부터 비교할 수 있어요";
/// Children per folder in a comparison at most.
const DIFF_LIMIT: usize = 64;
/// A loaded previous scan takes about as much memory as the drive's index;
/// one no comparison has used for this long is let go (read again when needed).
const PREV_IDLE: Duration = Duration::from_secs(300);

/// The previous scan of each drive (`<letter>.prev.idx`), once a comparison
/// has loaded it. It stays until the next scan of that drive replaces the
/// file, or until no comparison has used it for PREV_IDLE.
#[derive(Default)]
struct Previous {
    loaded: HashMap<String, Loaded>,
    /// How many times each drive's previous scan file was replaced, so a load
    /// that read the file being replaced is not kept.
    turns: HashMap<String, u64>,
    /// Drives whose previous scan, or the copy of the current scan that would
    /// become it, was dropped or not kept for want of disk space (since the
    /// last previous scan kept).
    no_room: HashSet<String>,
}

struct Loaded {
    ix: Arc<Index>,
    used: Instant,
}

impl Previous {
    fn turn(&self, drive: &str) -> u64 {
        self.turns.get(drive).copied().unwrap_or(0)
    }

    /// The loaded previous scan of `drive`, now used again.
    fn get(&mut self, drive: &str, now: Instant) -> Option<Arc<Index>> {
        let l = self.loaded.get_mut(drive)?;
        l.used = now;
        Some(l.ix.clone())
    }

    /// Keeps `ix`, read from the file as it was at `turn`, unless the file
    /// was replaced since. False when it was: `ix` may be the older file.
    fn keep(&mut self, drive: &str, turn: u64, ix: Arc<Index>, now: Instant) -> bool {
        if self.turn(drive) != turn {
            return false;
        }
        self.loaded.insert(drive.to_string(), Loaded { ix, used: now });
        true
    }

    /// The drive's previous scan file was replaced or removed (a new scan, a
    /// save that needed its room): what was loaded from it is let go.
    fn replaced(&mut self, drive: &str) {
        self.loaded.remove(drive);
        *self.turns.entry(drive.to_string()).or_default() += 1;
    }

    /// Takes out the previous scans no comparison used for `idle`. The file
    /// is unchanged, so the next comparison reads it again. The caller drops
    /// them, after letting go of the lock.
    fn idle(&mut self, now: Instant, idle: Duration) -> Vec<Arc<Index>> {
        let old: Vec<String> =
            self.loaded.iter().filter(|(_, l)| now.saturating_duration_since(l.used) >= idle).map(|(d, _)| d.clone()).collect();
        old.iter().filter_map(|d| self.loaded.remove(d)).map(|l| l.ix).collect()
    }

    /// Why `drive` has no previous scan to compare with.
    fn none(&self, drive: &str) -> &'static str {
        if self.no_room.contains(drive) { NO_PREV_ROOM } else { NO_PREV }
    }
}

fn storage_full(r: &std::io::Result<u64>) -> bool {
    r.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::StorageFull)
}

/// Whether removing `kept` (a file kept for comparisons) would make the room
/// that a save of `shared` to `cache`, leaving `reserve`, lacked. It frees
/// nothing while it is still the cache file under another name (a hard link
/// that no save has parted from it yet). When the free space cannot be told,
/// the disk said it was full, and any room helps.
fn frees_enough(kept: &Path, cache: &Path, shared: &Shared, reserve: u64) -> bool {
    let Ok(len) = std::fs::metadata(kept).map(|m| m.len()) else { return false };
    if win::same_entry(kept, cache) {
        return false;
    }
    let need = shared.read().unwrap().cache_len() as u64;
    cache.parent().and_then(win::free_space).is_none_or(|free| free.saturating_add(len) >= need.saturating_add(reserve))
}

impl AppState {
    pub(crate) fn get(&self, drive: &str) -> Option<Shared> {
        self.indexes.lock().unwrap().get(drive).cloned()
    }

    fn put(&self, ix: Index) -> Shared {
        let drive = ix.drive.clone();
        let shared = Arc::new(RwLock::new(ix));
        self.indexes.lock().unwrap().insert(drive, shared.clone());
        shared
    }

    /// Makes a fresh scan the drive's index, followed from now on by
    /// `watcher`, which started before the scan did and holds what changed
    /// while it ran. The old watcher stops; it was applying changes to the
    /// index being replaced. `volume`: the disk the scan was made of.
    fn install(&self, ix: Index, watcher: watch::Watcher, volume: Option<String>) {
        self.install_over(ix, watcher, volume, None);
    }

    /// `install`, when `over` is None; with Some (a sync), only while the
    /// drive's index is still that one (`Some(None)`: still none), so a sync
    /// that began before a scan finished never replaces the scan. False, with
    /// nothing changed (`watcher` stops), when it is not. A sync's watcher
    /// takes over a lost hard link from the one it replaces: a sync does not
    /// count those bytes again, only a full scan does. Either way the drive
    /// is caught up (sync::AutoSync::installed).
    fn install_over(&self, ix: Index, watcher: watch::Watcher, volume: Option<String>, over: Option<&Option<Shared>>) -> bool {
        let drive = ix.drive.clone();
        // A watcher still starting for the old index (`watch_cached`) checks
        // under this lock which index is current, so it cannot win over this one.
        let mut watchers = self.watchers.lock().unwrap();
        if let Some(base) = over {
            let still = match (self.get(&drive), base) {
                (Some(now), Some(base)) => Arc::ptr_eq(&now, base),
                (now, base) => now.is_none() && base.is_none(),
            };
            if !still {
                return false;
            }
            // By the watcher's own mark, not its status: an error since may have replaced that.
            if watchers.get(&drive).is_some_and(watch::Watcher::lost_links) {
                watcher.keep_links_lost();
            }
        }
        let shared = self.put(ix);
        self.volumes.lock().unwrap().insert(drive.clone(), volume);
        // Before `attach`, which tells what the new watcher missed so far (`Live::stale`).
        self.auto.lock().unwrap().installed(&drive);
        watcher.attach(shared);
        watchers.insert(drive, watcher);
        true
    }

    pub(crate) fn all(&self) -> Vec<Shared> {
        self.indexes.lock().unwrap().values().cloned().collect()
    }

    /// Writes the drive's current index to its cache file. Saves of one drive
    /// run one at a time and each writes whatever index is current when it
    /// starts, so an older snapshot never lands on top of a newer one.
    fn save(&self, drive: &str) -> std::io::Result<()> {
        self.save_keeping(drive, SAVE_RESERVE)
    }

    /// `save`, leaving at least `reserve` bytes free on the disk.
    fn save_keeping(&self, drive: &str, reserve: u64) -> std::io::Result<()> {
        self.save_then(drive, reserve, |_| {})
    }

    /// `save_keeping`, then `after` with the bytes written once the file is
    /// in place, still in the drive's save turn: no other save can have
    /// written the file in between.
    ///
    /// The files kept for comparisons give way to the cache. When the disk is
    /// too full for the cache, the copy of the current scan goes first, then
    /// the previous scan, each only when that makes the room the save lacked,
    /// and the save is tried again after each.
    fn save_then(&self, drive: &str, reserve: u64, after: impl FnOnce(u64)) -> std::io::Result<()> {
        let lock = self.save_locks.lock().unwrap().entry(drive.to_string()).or_default().clone();
        let _turn = lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.queued.lock().unwrap().remove(drive);
        // This save takes in every live change so far.
        let dirty = self.dirty.lock().unwrap().remove(drive);
        let Some(shared) = self.get(drive) else { return Ok(()) };
        let path = scan::cache_path(drive);
        let mut result = write_cache(&shared, &path, reserve);
        if storage_full(&result) && frees_enough(&scan::scan_copy_path(drive), &path, &shared, reserve) {
            scan::drop_scan_copy(drive);
            self.prev.lock().unwrap().no_room.insert(drive.to_string());
            result = write_cache(&shared, &path, reserve);
        }
        if storage_full(&result) && frees_enough(&scan::prev_cache_path(drive), &path, &shared, reserve) {
            scan::drop_previous(drive);
            let mut prev = self.prev.lock().unwrap();
            prev.replaced(drive);
            prev.no_room.insert(drive.to_string());
            drop(prev);
            result = write_cache(&shared, &path, reserve);
        }
        let now = Instant::now();
        match &result {
            Ok(written) => {
                self.saved.lock().unwrap().insert(drive.to_string(), now);
                self.failed.lock().unwrap().remove(drive);
                after(*written);
            }
            Err(_) => {
                // The file still lacks what this save was for (live changes, a
                // rename, a fresh scan); the saver tries again after a back-off.
                let d = dirty.unwrap_or(Dirty { since: now, last: now });
                self.dirty
                    .lock()
                    .unwrap()
                    .entry(drive.to_string())
                    .and_modify(|e| e.since = e.since.min(d.since))
                    .or_insert(d);
                let mut failed = self.failed.lock().unwrap();
                let f = failed.entry(drive.to_string()).or_insert(Failed { at: now, count: 0 });
                f.at = now;
                f.count += 1;
            }
        }
        result.map(drop)
    }

    /// The save right after a scan of `drive` (made from `volume`), leaving
    /// SCAN_SAVE_RESERVE free. In the same save turn, before any live change
    /// can reach the file, it tags the cache with its volume and keeps the
    /// copy of the scan that the next scan compares with (scan::keep_scan).
    /// The copy shares the cache's disk space only until the first live save
    /// writes the cache anew, so it is kept only when the disk has room for
    /// it and, after it, for a live save that still leaves SAVE_RESERVE.
    fn save_scan(&self, drive: &str, volume: Option<&str>) -> std::io::Result<()> {
        self.save_then(drive, SCAN_SAVE_RESERVE, |written| {
            scan::write_volume_tag(drive, volume);
            let path = scan::cache_path(drive);
            let room = path
                .parent()
                .and_then(win::free_space)
                .is_none_or(|free| free >= written.saturating_mul(2).saturating_add(SAVE_RESERVE));
            if !(room && scan::keep_scan(drive, volume).is_ok()) {
                // A copy still there is not of this scan (keep_previous took this drive's), yet
                // the next scan would take it for one.
                scan::drop_scan_copy(drive);
            }
            if !room {
                self.prev.lock().unwrap().no_room.insert(drive.to_string());
            }
        })
    }

    /// The save right after a sync (made from `volume`): its cache, tagged
    /// with its volume, leaving SCAN_SAVE_RESERVE free. Unlike `save_scan` it
    /// keeps no copy of itself for comparisons: the previous scan and the
    /// copy of the last full scan stay as they are, so the next full scan
    /// still compares with the scan before it, not with a sync.
    fn save_synced(&self, drive: &str, volume: Option<&str>) -> std::io::Result<()> {
        self.save_then(drive, SCAN_SAVE_RESERVE, |_| scan::synced(drive, volume))
    }

    /// Whether a failed save of the drive is still waiting out its back-off.
    fn holds_off(&self, drive: &str) -> bool {
        self.failed.lock().unwrap().get(drive).is_some_and(|f| f.holds_off(Instant::now()))
    }

    /// Notes a live change to a drive's index; `save_dirty_caches` writes it later.
    fn changed(&self, drive: &str) {
        let now = Instant::now();
        self.dirty
            .lock()
            .unwrap()
            .entry(drive.to_string())
            .and_modify(|d| d.last = now)
            .or_insert(Dirty { since: now, last: now });
    }

    /// A watcher `load_cached` started is in place (or was not needed).
    fn started(&self, drive: &str) {
        self.starting.lock().unwrap().remove(drive);
        self.started.notify_all();
    }

    /// Before a new scan of `drive` (made from `volume`) becomes its index:
    /// the copy of the scan it replaces becomes the previous scan
    /// (scan::keep_previous), and a previous scan loaded before is let go
    /// either way (it may be of the disk that had this letter before). Takes
    /// the drive's save turn, so no save writes the files meanwhile.
    fn keep_previous(&self, drive: &str, volume: Option<&str>) {
        let lock = self.save_locks.lock().unwrap().entry(drive.to_string()).or_default().clone();
        let _turn = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let kept = scan::keep_previous(drive, volume).unwrap_or(false);
        let mut prev = self.prev.lock().unwrap();
        prev.replaced(drive);
        if kept {
            prev.no_room.remove(drive);
        }
    }

    /// The drive's previous scan if it is older than `than` (the scan time
    /// of the drive's index in memory). Read from disk the first time, and
    /// again after it was let go for being idle (seconds on a big drive; call
    /// it on a blocking thread). Only the header of a file that is not older,
    /// which cannot be compared, is ever read.
    fn previous(&self, drive: &str, than: i64) -> Result<Arc<Index>, String> {
        let lock = self.prev_loads.lock().unwrap().entry(drive.to_string()).or_default().clone();
        let _turn = lock.lock().unwrap_or_else(PoisonError::into_inner);
        // A new scan replacing the file while it is read means reading it again.
        for _ in 0..3 {
            let (turn, none) = {
                let mut prev = self.prev.lock().unwrap();
                if let Some(ix) = prev.get(drive, Instant::now()) {
                    return Ok(ix);
                }
                (prev.turn(drive), prev.none(drive))
            };
            if !scan::previous_scan(drive).is_some_and(|h| h.scanned_at < than) {
                return Err(none.into());
            }
            let ix = Arc::new(load_previous(drive, none)?);
            if self.prev.lock().unwrap().keep(drive, turn, ix.clone(), Instant::now()) {
                return Ok(ix);
            }
        }
        Err("스캔하는 중이라 비교하지 못했어요. 잠시 뒤에 다시 해 주세요".into())
    }
}

/// Reads the drive's previous scan file (whose header `scan::previous_scan`
/// found to be of this drive and volume). `none` says why there is none.
fn load_previous(drive: &str, none: &str) -> Result<Index, String> {
    match Index::load(&scan::prev_cache_path(drive)) {
        Ok(ix) if ix.drive == drive => Ok(ix),
        Ok(_) => Err(none.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(none.into()),
        Err(e) => Err(format!("이전 스캔을 읽지 못했어요 ({e}). 한 번 더 스캔하면 비교할 수 있어요")),
    }
}

/// Writes an index to its cache file and returns the bytes written. The
/// index is turned into bytes under its read lock, which takes a moment; the
/// disk is written with no lock held, so the watcher and every command on the
/// drive can go on meanwhile. At least `reserve` bytes stay free on the disk.
fn write_cache(shared: &Shared, path: &Path, reserve: u64) -> std::io::Result<u64> {
    let need = shared.read().unwrap().cache_len() as u64;
    if let Some(free) = path.parent().and_then(win::free_space)
        && free < need.saturating_add(reserve)
    {
        return Err(std::io::Error::new(std::io::ErrorKind::StorageFull, "디스크 공간이 부족해서 인덱스를 저장하지 않았어요"));
    }
    let bytes = shared.read().unwrap().to_bytes();
    index::write_cache(path, &bytes)?;
    Ok(bytes.len() as u64)
}

/// Writes the caches of drives that took live changes, once they are due,
/// and lets go of previous scans no comparison has used for PREV_IDLE.
fn save_dirty_caches(app: AppHandle) {
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let state = app.state::<AppState>();
        let idle = state.prev.lock().unwrap().idle(Instant::now(), PREV_IDLE);
        drop(idle);
        let due: Vec<String> = {
            let now = Instant::now();
            let saved = state.saved.lock().unwrap();
            let failed = state.failed.lock().unwrap();
            let dirty = state.dirty.lock().unwrap();
            dirty
                .iter()
                .filter(|&(drive, d)| save_due(d, saved.get(drive).copied(), failed.get(drive), now))
                .map(|(drive, _)| drive.clone())
                .collect()
        };
        for drive in due {
            let _ = state.save(&drive);
        }
    }
}

/// Stops every watcher, then writes each cache that is missing live changes,
/// before the process ends. The watcher threads are not waited for.
///
/// A drive in step until now (`in_step_at_exit`) whose cache then holds
/// every change is noted as in step at this moment (scan::write_in_step),
/// so the next start need not sync it right away (`auto_sync_wanted`): the
/// index's own scan time says nothing of the hours of live changes since.
fn save_on_exit(app: &AppHandle) {
    let state = app.state::<AppState>();
    let now = index::now_filetime();
    // A scan or sync still running puts another index in place, and saves it, after this.
    let busy = |drive: &str| state.jobs.lock().unwrap().busy(drive);
    let statuses: Vec<(String, watch::Status)> = {
        let mut watchers = state.watchers.lock().unwrap();
        let s = watchers.iter().map(|(d, w)| (d.clone(), w.status())).collect();
        watchers.clear();
        s
    };
    let keeping_up = in_step_at_exit(statuses, &state.jobs.lock().unwrap(), &state.auto.lock().unwrap());
    save_all(&state);
    for drive in keeping_up {
        // A save that failed, now or before, left changes out of the file.
        let behind = state.dirty.lock().unwrap().contains_key(&drive) || state.failed.lock().unwrap().contains_key(&drive);
        if !behind && !busy(&drive) && scan::cache_path(&drive).is_file() {
            scan::write_in_step(&drive, now);
        }
    }
}

/// Writes each cache that is missing live changes (or a rename or trash
/// whose background save has not started yet, which is not in `dirty`), and
/// waits for background saves still writing: the exit would cut them off,
/// leaving the older file in place.
fn save_all(state: &AppState) {
    let mut due: HashSet<String> = state.dirty.lock().unwrap().keys().cloned().collect();
    due.extend(state.queued.lock().unwrap().iter().cloned());
    for drive in due {
        let _ = state.save(&drive);
    }
    let turns: Vec<Arc<Mutex<()>>> = state.save_locks.lock().unwrap().values().cloned().collect();
    for turn in turns {
        drop(turn.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// Of the watchers' `statuses` at exit, the drives whose index is what is on
/// the disk: watching (no change missed), with no scan or sync of them
/// running, and caught up (sync::AutoSync::caught_up), a scan or sync since
/// the app started having put their index in place. A drive still waiting
/// for its startup sync (queued behind another drive's, put off, or the app
/// closed within its first seconds) holds the cache from before the app
/// started, which misses what changed while it was closed, however well its
/// watcher kept up since.
fn in_step_at_exit(statuses: Vec<(String, watch::Status)>, jobs: &sync::Jobs, auto: &sync::AutoSync) -> Vec<String> {
    statuses.into_iter().filter(|(d, s)| s.state == "watching" && !jobs.busy(d) && auto.caught_up(d)).map(|(d, _)| d).collect()
}

/// Passes live index changes on to the window and queues the cache save.
struct Live(AppHandle);

#[derive(Serialize, Clone)]
struct IndexChanged<'a> {
    drive: &'a str,
    dirs: Vec<String>,
    full: bool,
    /// Folders removed, or renamed or moved away, by their old paths (none
    /// inside another): a view at or below one of them shows what is gone.
    removed: Vec<String>,
}

#[derive(Serialize, Clone)]
struct IndexStale<'a> {
    drive: &'a str,
    reason: &'a str,
}

impl watch::Sink for Live {
    fn changed(&self, drive: &str, dirs: Vec<String>, full: bool, removed: Vec<String>) {
        self.0.state::<AppState>().changed(drive);
        let _ = self.0.emit("index-changed", IndexChanged { drive, dirs, full, removed });
    }

    fn stale(&self, drive: &str, reason: &str) {
        // Synced again a little later by `auto_sync`, when that helps then;
        // never for a lost hard link, which only a full scan puts right.
        if !watch::is_links_lost(reason) {
            self.0.state::<AppState>().auto.lock().unwrap().went_stale(drive, Instant::now());
        }
        let _ = self.0.emit("index-stale", IndexStale { drive, reason });
    }
}

const NETWORK_OFF: &str = "네트워크 드라이브는 실시간으로 반영하지 않아요. 바뀐 내용은 다시 스캔하면 보여요.";
const KIND_OFF: &str = "이 종류의 드라이브는 실시간으로 반영하지 않아요. 바뀐 내용은 다시 스캔하면 보여요.";

/// Live updates for `drive`, or the reason it has none. Only local disks
/// are watched; a network share's notifications stop with the connection
/// and miss what other computers do, so a rescan stays the way there.
/// With `shared` None the watcher holds its changes until `install`.
fn new_watcher(app: &AppHandle, drive: &str, shared: Option<Shared>) -> watch::Watcher {
    match win::drive_kind(drive) {
        "fixed" | "removable" => watch::start(drive, shared, Arc::new(Live(app.clone()))),
        "network" => watch::Watcher::off(NETWORK_OFF),
        _ => watch::Watcher::off(KIND_OFF),
    }
}

/// Starts live updates for an index loaded from its cache file, unless a
/// rescan replaced that index meanwhile (it brought its own watcher).
fn watch_cached(app: &AppHandle, drive: &str, shared: Shared) {
    let watcher = new_watcher(app, drive, Some(shared.clone()));
    let state = app.state::<AppState>();
    let mut watchers = state.watchers.lock().unwrap();
    if state.get(drive).is_some_and(|now| Arc::ptr_eq(&now, &shared)) {
        watchers.insert(drive.to_string(), watcher);
    }
}

/// Saves a drive's cache again in the background after a rename or trash, so
/// the change survives a restart. A burst of changes shares one queued save.
fn save_later(app: &AppHandle, drive: String) {
    let state = app.state::<AppState>();
    // While saves of this drive keep failing, the saver tries after its back-off.
    if state.holds_off(&drive) {
        state.changed(&drive);
        return;
    }
    if !state.queued.lock().unwrap().insert(drive.clone()) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = app.state::<AppState>().save(&drive);
    });
}

pub(crate) fn drive_of(path: &str) -> Option<String> {
    scan::normalize_drive(path.get(..2)?)
}

/// Runs `f` on a blocking thread. A panic in it comes back as an Err string.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct DriveInfo {
    #[serde(flatten)]
    drive: win::Drive,
    indexed: bool,
    method: Option<String>,
    scanned_at: Option<i64>,
    scan_ms: Option<u64>,
    entries: Option<usize>,
}

#[derive(Serialize)]
struct AppInfo {
    version: String,
    elevated: bool,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo { version: env!("CARGO_PKG_VERSION").into(), elevated: win::is_elevated() }
}

// ---- v0.8: in-app updates (Updater.tsx) ----------------------------------------
//
// The updater plugin checks, downloads and checks the signature; the window
// asks before any of it is installed. On Windows the plugin starts the
// installer and then ends this process with `std::process::exit`, so no
// `RunEvent::Exit` comes: what that does (stop the work, write the caches) is
// done first by `update_prepare`. The installer (passive, `/R`) closes every
// other process of this program file by force and starts the app again.

const UPDATE_PLAN_ELSEWHERE: &str = "다른 fine-file 창에서 정리 계획을 실행하거나 되돌리는 중이에요. 설치 프로그램이 그 창을 강제로 꺼서 하던 단계가 되돌리기 기록에 빠질 수 있으니, 끝난 뒤에 업데이트해 주세요";
const UPDATE_RELOCATION_ELSEWHERE: &str = "다른 fine-file 창에서 폴더를 다른 드라이브로 옮기거나 되돌리는 중이에요. 끝난 뒤에 업데이트해 주세요";
const UPDATE_LINK_ELSEWHERE: &str = "다른 fine-file 창에서 중복 파일을 하드 링크로 합치는 중이에요. 끝난 뒤에 업데이트해 주세요";
const UPDATE_STILL_BUSY: &str = "작업이 아직 다 멈추지 않아서 업데이트를 설치하지 않았어요. 멈추게 한 작업은 그대로 멈춰요(정리 계획은 하던 단계까지만 해요). 잠시 뒤에 다시 해 주세요";

/// What the update installer would cut short elsewhere: the other processes
/// of this program file (other windows, an MCP server a Claude app started),
/// and whether one of them runs a plan, a relocation or a merge of
/// duplicates into hard links now.
#[derive(Serialize)]
struct UpdateOthers {
    processes: u32,
    plan: bool,
    relocation: bool,
    link: bool,
}

#[tauri::command(async)]
fn update_others() -> UpdateOthers {
    UpdateOthers {
        processes: win::other_instances(),
        plan: plans::running_elsewhere(),
        relocation: ops::relocating_elsewhere(),
        link: ops::linking_elsewhere(),
    }
}

/// Readies the app for the update installer, just before it starts. False,
/// with nothing done, while work of this window is going that the user did
/// not agree to stop: a copy, move or relocation job not among `stop_jobs`
/// (those the window told them about), or a plan run or undone here other
/// than `stop_plan`. Work started during the download is asked about first.
/// What was agreed to stops as when the window closes (`confirm_close`): the
/// plan after the step it is at, written down; the jobs cancelled (also those
/// the plan's step started), their unfinished copies removed. Then every
/// cache is written. Refused while a plan, relocation or merge into hard
/// links of another window runs, or a relocation cut short is being put
/// right, since the installer would end those mid-way.
#[tauri::command]
async fn update_prepare(app: AppHandle, stop_jobs: Vec<u64>, stop_plan: Option<String>) -> Result<bool, String> {
    blocking(move || {
        if plans::running_elsewhere() {
            return Err(UPDATE_PLAN_ELSEWHERE.into());
        }
        if ops::relocating_elsewhere() {
            return Err(UPDATE_RELOCATION_ELSEWHERE.into());
        }
        if ops::linking_elsewhere() {
            return Err(UPDATE_LINK_ELSEWHERE.into());
        }
        if ops::recovering() {
            return Err(ops::RECOVER_BUSY.into());
        }
        if ops::active_jobs() > 0 || plans::running() {
            let plan = plans::running_id();
            if ops::own_jobs().iter().any(|id| !stop_jobs.contains(id)) || (plan.is_some() && plan != stop_plan) {
                return Ok(false);
            }
            // First no next step, then the jobs (a step may wait on one), then the step itself.
            plans::stop_and_wait(Duration::ZERO);
            ops::cancel_all();
            let jobs_stopped = ops::wait_all(CLOSE_JOB_WAIT);
            let plan_stopped = plans::stop_and_wait(CLOSE_PLAN_WAIT);
            if !(jobs_stopped && plan_stopped) {
                plans::reopen();
                return Err(UPDATE_STILL_BUSY.into());
            }
        }
        save_all(&app.state::<AppState>());
        Ok(true)
    })
    .await?
}

/// The installer did not start (its UAC prompt declined, say) after the
/// updater had hidden the windows to quit (`cleanup_before_exit`): they come
/// back, and plans may run again. The app goes on as before.
#[tauri::command]
fn update_install_failed(app: AppHandle) {
    plans::reopen();
    for w in app.webview_windows().values() {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Asking drives can stall (a mapped drive whose server is gone, a disc
/// spinning up), so it never runs on the UI thread.
#[tauri::command]
async fn get_drives(state: State<'_, AppState>) -> Result<Vec<DriveInfo>, String> {
    let drives = blocking(win::drives).await?;
    // An index's lock can be busy for a moment (a batch of live changes),
    // which an async worker must not wait out.
    let indexes = state.indexes.lock().unwrap().clone();
    blocking(move || {
        drives
            .into_iter()
            .map(|d| {
                let ix = indexes.get(&d.letter).map(|i| i.read().unwrap());
                DriveInfo {
                    indexed: ix.is_some(),
                    method: ix.as_ref().map(|i| i.method.clone()),
                    scanned_at: ix.as_ref().map(|i| i.scanned_at),
                    scan_ms: ix.as_ref().map(|i| i.scan_ms),
                    entries: ix.as_ref().map(|i| i.nodes.len()),
                    drive: d,
                }
            })
            .collect()
    })
    .await
}

/// Loads the saved index of every drive that has one, so sizes show up
/// instantly, and starts live updates for them. Returns every indexed drive.
#[tauri::command]
async fn load_cached(app: AppHandle, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    // A drive already in memory (the window was reloaded) keeps that index:
    // it is at least as new as its file and may hold live changes not saved yet.
    let have: HashSet<String> = state.indexes.lock().unwrap().keys().cloned().collect();
    let skip = have.clone();
    let (loaded, in_step) = blocking(move || {
        scan::migrate_old_cache();
        scan::sweep_temp_files();
        // Before anything can write a cache again (a save, a scan).
        let in_step = scan::take_in_step();
        let loaded = win::drive_letters()
            .into_iter()
            .filter(|d| !skip.contains(d))
            // A cache made from another disk that now has this letter would show its folders.
            .filter(|d| scan::cache_matches_volume(d))
            .filter_map(|d| Index::load(&scan::cache_path(&d)).ok().filter(|ix| ix.drive == d))
            .map(|ix| {
                let volume = win::volume_id(&ix.drive);
                (ix, volume)
            })
            .collect::<Vec<_>>();
        (loaded, in_step)
    })
    .await?;
    {
        let mut kept_up = state.in_step.lock().unwrap();
        for (ix, _) in &loaded {
            if let Some(&at) = in_step.get(&ix.drive) {
                kept_up.insert(ix.drive.clone(), at);
            }
        }
    }
    let fresh: Vec<(String, Shared)> = loaded
        .into_iter()
        .map(|(ix, volume)| {
            state.volumes.lock().unwrap().insert(ix.drive.clone(), volume);
            (ix.drive.clone(), state.put(ix))
        })
        .collect();
    let mut names: Vec<String> = have.into_iter().chain(fresh.iter().map(|(d, _)| d.clone())).collect();
    names.sort();
    // The window does not wait for live updates: opening a drive's root can
    // take seconds (a disk spinning up). Each drive starts on its own.
    state.starting.lock().unwrap().extend(fresh.iter().map(|(d, _)| d.clone()));
    // What changed while the app was closed: each drive is synced once, one
    // after another, in the background (`auto_sync` checks each when its turn comes).
    let mut queue: Vec<String> = fresh.iter().map(|(d, _)| d.clone()).collect();
    queue.sort();
    state.auto.lock().unwrap().after_startup(queue, Instant::now());
    for (drive, shared) in fresh {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            // Even after a panic, so `live_status` does not keep waiting for it.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| watch_cached(&app, &drive, shared)));
            app.state::<AppState>().started(&drive);
        });
    }
    Ok(names)
}

#[derive(Serialize)]
struct ScanResult {
    method: String,
    entries: usize,
    scan_ms: u64,
    note: Option<String>,
}

const MFT_NOT_HERE: &str = "MFT 고속 스캔은 이 컴퓨터의 NTFS 드라이브에서만 쓸 수 있어서 일반 탐색으로 스캔했어요.";

/// A scan of a drive under way (`sync::Jobs`), until dropped.
struct ScanTurn {
    app: AppHandle,
    drive: String,
}

impl ScanTurn {
    fn new(app: &AppHandle, drive: &str) -> ScanTurn {
        app.state::<AppState>().jobs.lock().unwrap().scan_started(drive);
        ScanTurn { app: app.clone(), drive: drive.to_string() }
    }
}

impl Drop for ScanTurn {
    fn drop(&mut self) {
        self.app.state::<AppState>().jobs.lock().unwrap_or_else(PoisonError::into_inner).scan_ended(&self.drive);
    }
}

/// A full scan. mode: "auto" or "mft" (the MFT, with administrator rights,
/// falling back to a folder walk) or "walk". The MFT is read without a UAC
/// prompt when the app runs elevated or an admin session is on (admin.rs);
/// otherwise the scan helper asks once (scan/mod.rs). "mft" says so when the
/// drive has no MFT to read. A sync of the drive under way stops (the scan
/// replaces whatever it would bring).
#[tauri::command]
async fn scan_drive(drive: String, mode: String, app: AppHandle, state: State<'_, AppState>) -> Result<ScanResult, String> {
    let drive = scan::normalize_drive(&drive).ok_or("드라이브 이름이 올바르지 않아요")?;
    let (mft, asked_mft) = match mode.as_str() {
        "auto" => (true, false),
        "mft" => (true, true),
        "walk" => (false, false),
        _ => return Err("알 수 없는 스캔 방식이에요".into()),
    };
    let _turn = ScanTurn::new(&app, &drive);
    let d = drive.clone();
    let a = app.clone();
    let (ix, note, volume, watcher) = blocking(move || -> Result<(Index, Option<String>, Option<String>, watch::Watcher), String> {
        let volume = win::volume_id(&d);
        // Live updates for the new index start before the scan reads anything,
        // so what changes while it runs is held and applied to it afterwards.
        let watcher = new_watcher(&a, &d, None);
        let walk = |note: Option<String>| scan::scan_walk(&d).map(|ix| (ix, note)).map_err(|e| e.to_string());
        // Only a local NTFS volume has an MFT to read. A network drive may
        // report NTFS too, but a UAC prompt for it could never help.
        let local_ntfs = win::drive(&d)
            .is_some_and(|v| v.fs.eq_ignore_ascii_case("NTFS") && matches!(v.kind.as_str(), "fixed" | "removable"));
        let (ix, note) = if !mft {
            walk(None)?
        } else if !local_ntfs {
            walk(asked_mft.then(|| MFT_NOT_HERE.into()))?
        } else if !win::is_elevated() && !win::exe_in_program_files() && !cfg!(debug_assertions) {
            // Elevating a binary the user can overwrite would hand administrator rights to
            // whatever replaced it. Development builds (target\debug) are exempt.
            walk(Some("MFT 고속 스캔은 \"모든 사용자용\"(Program Files)으로 설치했을 때만 써요. 이번에는 일반 탐색으로 스캔했어요.".into()))?
        } else {
            match read_mft(&d) {
                Ok(ix) => (ix, None),
                Err(scan::HelperError::Cancelled) => walk(Some(
                    "관리자 권한을 허락하지 않아 일반 탐색으로 스캔했어요. 일부 보호된 폴더는 빠져 있어요.".into(),
                ))?,
                Err(scan::HelperError::Helper(e)) => walk(Some(format!("관리자 도우미 실행 실패, 일반 탐색으로 대체: {e}")))?,
                Err(scan::HelperError::Scan(e)) => walk(Some(format!("MFT 읽기 실패, 일반 탐색으로 대체: {e}")))?,
            }
        };
        // Before the new index is in place, so no save can have written it yet:
        // the copy kept of the scan being replaced becomes the previous scan.
        a.state::<AppState>().keep_previous(&d, volume.as_deref());
        Ok((ix, note, volume, watcher))
    })
    .await??;
    let mut result = ScanResult { method: ix.method.clone(), entries: ix.nodes.len(), scan_ms: ix.scan_ms, note };
    state.install(ix, watcher, volume.clone());
    let saved = blocking(move || {
        // The cache is written here, as the normal user, never by the elevated helper.
        app.state::<AppState>().save_scan(&drive, volume.as_deref()).map_err(|e| (e.kind(), e.to_string()))
    })
    .await;
    // The scan is in use either way; the user learns it will not be there next time.
    let problem = match saved {
        Ok(Ok(())) => None,
        Ok(Err((std::io::ErrorKind::StorageFull, _))) => Some(SCAN_NOT_SAVED_FULL.to_string()),
        Ok(Err((_, e))) | Err(e) => Some(format!("스캔 결과를 저장하지 못했어요 ({e}). 다음에 켜면 다시 스캔해야 해요.")),
    };
    if let Some(p) = problem {
        add_note(&mut result, p);
    }
    Ok(result)
}

fn add_note(result: &mut ScanResult, more: String) {
    result.note = Some(match result.note.take() {
        Some(n) => format!("{n} {more}"),
        None => more,
    });
}

/// The MFT of `drive`, read with administrator rights: here when the app
/// runs elevated, through the admin session when one is on (no prompt), else
/// by the scan helper after a UAC prompt. A session that breaks meanwhile is
/// as if there were none.
fn read_mft(drive: &str) -> Result<Index, scan::HelperError> {
    if win::is_elevated() {
        return scan::scan_mft_guarded(drive).map_err(|e| scan::HelperError::Scan(e.to_string()));
    }
    match admin::scan_mft(drive) {
        None | Some(Err(scan::HelperError::Helper(_))) => scan::scan_mft_elevated(drive),
        Some(read) => read,
    }
}

#[derive(Serialize, Clone)]
struct SyncProgress<'a> {
    drive: &'a str,
    /// "running", "done" or "failed".
    state: &'a str,
    detail: Option<String>,
    /// "startup", "stale" or "user" (sync::Reason).
    reason: &'a str,
}

/// A sync of a drive under way (`sync::Jobs`), until dropped.
struct SyncTurn<'a> {
    app: &'a AppHandle,
    drive: &'a str,
    ctl: Arc<scan::walk::Control>,
}

impl Drop for SyncTurn<'_> {
    fn drop(&mut self) {
        self.app.state::<AppState>().jobs.lock().unwrap_or_else(PoisonError::into_inner).sync_ended(self.drive, &self.ctl);
    }
}

/// Syncs `drive` (scan/sync.rs): a folder walk with normal rights, merged
/// with the drive's index until now, then put in place as a scan is (a new
/// watcher takes over, the cache is saved) except that the previous scan
/// kept for comparisons stays as it is. Never asks for administrator rights.
/// Refused while a scan or another sync of the drive runs. Tells how it goes
/// in "sync-progress" events. `background`: in background mode (low CPU and
/// I/O priority), for syncs that run by themselves.
fn sync_now(app: &AppHandle, drive: &str, reason: sync::Reason, background: bool) -> Result<ScanResult, String> {
    let state = app.state::<AppState>();
    let ctl = state.jobs.lock().unwrap().sync_started(drive).map_err(|busy| match busy {
        sync::Busy::Scanning => SCANNING_NOW.to_string(),
        sync::Busy::Syncing => SYNCING_NOW.to_string(),
    })?;
    let turn = SyncTurn { app, drive, ctl: ctl.clone() };
    if reason == sync::Reason::Stale {
        // Only a stale sync that starts makes the next one wait longer.
        state.auto.lock().unwrap().ran(drive, Instant::now());
    }
    let progress = |state: &str, detail: Option<String>| {
        let _ = app.emit("sync-progress", SyncProgress { drive, state, detail, reason: reason.as_str() });
    };
    progress("running", Some(SYNC_RUNNING.into()));
    let done = std::panic::catch_unwind(AssertUnwindSafe(|| run_sync(app, drive, &ctl, background, &progress)))
        .unwrap_or_else(|_| Err("내부 오류로 동기화하지 못했어요. 다시 스캔해 주세요".into()));
    // Over before the window hears so: what it asks `live_status` then is no longer "syncing".
    drop(turn);
    match &done {
        Ok(r) => {
            let mut detail = format!("다시 맞췄어요 (항목 {}개, {:.1}초)", r.entries, r.scan_ms as f64 / 1000.0);
            if let Some(note) = &r.note {
                detail = format!("{detail}. {note}");
            }
            progress("done", Some(detail));
        }
        Err(e) => progress("failed", Some(e.clone())),
    }
    done
}

fn run_sync(
    app: &AppHandle,
    drive: &str,
    ctl: &scan::walk::Control,
    background: bool,
    progress: &(dyn Fn(&str, Option<String>) + Sync),
) -> Result<ScanResult, String> {
    let state = app.state::<AppState>();
    let volume = win::volume_id(drive);
    let current = state.get(drive);
    // Only an index of the disk under this letter now is taken in.
    let known = state.volumes.lock().unwrap().get(drive).cloned();
    let base = current.clone().filter(|_| known.as_ref() == Some(&volume));
    // As for a scan: started before the walk reads anything, holding what
    // changes meanwhile for the new index.
    let watcher = new_watcher(app, drive, None);
    let cluster = win::cluster_size(drive).unwrap_or(0);
    let failed = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::Interrupted => SYNC_REPLACED.to_string(),
        _ => format!("동기화하지 못했어요: {e}"),
    };
    // The walk, which takes long, at low priority when the sync runs by
    // itself; the merge, which holds the drive's index, at normal priority.
    let work = || sync::walk_drive(drive, ctl);
    let found = with_progress(ctl, progress, || match background.then(background_pool).flatten() {
        Some(pool) => pool.install(work),
        None => work(),
    })
    .map_err(failed)?;
    let synced = sync::finish(drive, found, base.as_deref(), cluster, ctl).map_err(failed)?;
    let ix = synced.index;
    let note = match (synced.kept, synced.unopened) {
        (0, 0) => None,
        (0, n) => Some(format!("열 수 없는 폴더 {n}개는 빠져 있어요. 전체 스캔(MFT)으로 다시 세면 들어가요.")),
        (k, n) if k == n => Some(format!("열 수 없는 폴더 {k}개는 지난 스캔에서 찾은 내용을 그대로 썼어요.")),
        (k, n) => Some(format!(
            "열 수 없는 폴더 {n}개 중 {k}개는 지난 스캔에서 찾은 내용을 그대로 썼고, 나머지는 빠져 있어요."
        )),
    };
    let mut result = ScanResult { method: ix.method.clone(), entries: ix.nodes.len(), scan_ms: ix.scan_ms, note };
    if ctl.cancelled() || !state.install_over(ix, watcher, volume.clone(), Some(&current)) {
        return Err(SYNC_REPLACED.into());
    }
    // The window reads what it shows of this drive again.
    let _ = app.emit("index-changed", IndexChanged { drive, dirs: Vec::new(), full: true, removed: Vec::new() });
    if state.watchers.lock().unwrap().get(drive).is_some_and(watch::Watcher::lost_links) {
        add_note(&mut result, SYNC_KEEPS_LINKS.into());
    }
    let problem = match state.save_synced(drive, volume.as_deref()) {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::StorageFull => Some(SCAN_NOT_SAVED_FULL.to_string()),
        Err(e) => Some(format!("스캔 결과를 저장하지 못했어요 ({e}). 다음에 켜면 다시 스캔해야 해요.")),
    };
    if let Some(p) = problem {
        add_note(&mut result, p);
    }
    Ok(result)
}

/// A rayon pool whose threads run in background mode, for the walks of syncs
/// that run by themselves.
fn background_pool() -> Option<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(SYNC_THREADS)
        .thread_name(|i| format!("sync {i}"))
        .start_handler(|_| win::background_thread())
        .build()
        .ok()
}

/// Runs `work`, telling every SYNC_PROGRESS_EVERY how many entries the walk
/// found so far.
fn with_progress<T>(ctl: &scan::walk::Control, progress: &(dyn Fn(&str, Option<String>) + Sync), work: impl FnOnce() -> T) -> T {
    /// Ends the ticker however `work` ends (a panic too): the scope waits for it.
    struct Done<'a>(&'a AtomicBool, std::thread::Thread);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
            self.1.unpark();
        }
    }
    let finished = AtomicBool::new(false);
    std::thread::scope(|s| {
        let ticker = s.spawn(|| {
            let mut told = 0;
            loop {
                std::thread::park_timeout(SYNC_PROGRESS_EVERY);
                if finished.load(Ordering::Relaxed) {
                    return;
                }
                let seen = ctl.seen.load(Ordering::Relaxed);
                if seen != told {
                    told = seen;
                    progress("running", Some(format!("{SYNC_RUNNING} (항목 {seen}개)")));
                }
            }
        });
        let _done = Done(&finished, ticker.thread().clone());
        work()
    })
}

/// A quick re-sync of a drive without administrator rights: a folder walk
/// merged with the current index (folders the walk cannot open keep what the
/// last scan found). Progress arrives as "sync-progress" events (reason
/// "user"). Network drives are synced only this way, never by themselves.
#[tauri::command]
async fn sync_drive(drive: String, app: AppHandle) -> Result<ScanResult, String> {
    let drive = scan::normalize_drive(&drive).ok_or("드라이브 이름이 올바르지 않아요")?;
    blocking(move || sync_now(&app, &drive, sync::Reason::User, false)).await?
}

/// Runs syncs that run by themselves, one at a time: once per drive after
/// startup, and a little after a drive's watcher went stale (sync::AutoSync).
/// When its turn comes each is checked again (`auto_sync_wanted`), and none
/// ever asks for administrator rights.
fn auto_sync(app: AppHandle) {
    loop {
        std::thread::sleep(AUTO_TICK);
        let next = app.state::<AppState>().auto.lock().unwrap().next(Instant::now());
        let Some((drive, reason)) = next else { continue };
        if auto_sync_wanted(&app, &drive, reason) {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| sync_now(&app, &drive, reason, true)));
        }
    }
}

/// Whether a sync that runs by itself still makes sense for `drive` now: an
/// indexed local disk (fixed or removable) that no scan or sync is busy
/// with; after startup, one no scan or sync since the app started caught
/// up (sync::AutoSync::caught_up). One that was in step only a few minutes
/// before the app started (scanned or synced then, or the app closed with
/// its watcher keeping up: sync::startup_due, `save_on_exit`) is put off for
/// sync::FRESH instead of skipped, so what changed in those minutes is still
/// taken in. After its watcher went stale: one whose watcher is still stale
/// in a way a sync puts right (a scan since brought a new one).
fn auto_sync_wanted(app: &AppHandle, drive: &str, reason: sync::Reason) -> bool {
    let state = app.state::<AppState>();
    let Some(shared) = state.get(drive) else { return false };
    let kind = win::drive_kind(drive);
    if !matches!(kind, "fixed" | "removable") || state.jobs.lock().unwrap().busy(drive) {
        return false;
    }
    match reason {
        sync::Reason::Startup => {
            if state.auto.lock().unwrap().caught_up(drive) {
                return false;
            }
            let scanned = shared.read().unwrap().scanned_at;
            // Used once: the sync it puts off is not put off again for it.
            let kept_up = state.in_step.lock().unwrap().remove(drive).unwrap_or(i64::MIN);
            if sync::startup_due(kind, scanned.max(kept_up), index::now_filetime()) {
                return true;
            }
            state.auto.lock().unwrap().defer(drive, Instant::now() + sync::FRESH);
            false
        }
        sync::Reason::Stale => state.watchers.lock().unwrap().get(drive).is_some_and(|w| w.status().sync_helps()),
        sync::Reason::User => true,
    }
}

#[derive(Serialize)]
struct LiveStatus {
    drive: String,
    /// "watching", "stale" (changes were missed; a sync or rescan makes it
    /// exact), "syncing" (a sync runs), "off" or "error".
    state: &'static str,
    detail: Option<String>,
}

/// How each indexed drive keeps up with changes on disk. Right after
/// startup the watchers may still be opening their drives; the answer waits
/// a moment for them, and a drive still starting after that counts as
/// watching (if its start fails, "index-stale" says so).
#[tauri::command]
async fn live_status(app: AppHandle) -> Result<Vec<LiveStatus>, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        let starting = {
            let waiting = state.starting.lock().unwrap();
            let (waiting, _) = state.started.wait_timeout_while(waiting, STARTING_WAIT, |s| !s.is_empty()).unwrap();
            waiting.clone()
        };
        let mut drives: Vec<String> = state.indexes.lock().unwrap().keys().cloned().collect();
        drives.sort();
        let syncing: HashSet<String> = drives.iter().filter(|d| state.jobs.lock().unwrap().syncing(d)).cloned().collect();
        let watchers = state.watchers.lock().unwrap();
        drives
            .into_iter()
            .map(|drive| {
                let s = match watchers.get(&drive) {
                    _ if syncing.contains(&drive) => watch::Status { state: "syncing", detail: Some(SYNC_RUNNING.into()) },
                    Some(w) => w.status(),
                    None if starting.contains(&drive) => watch::Status { state: "watching", detail: None },
                    None => watch::Status { state: "off", detail: None },
                };
                LiveStatus { drive, state: s.state, detail: s.detail }
            })
            .collect()
    })
    .await
}

#[derive(Serialize)]
struct Listing {
    path: String,
    indexed: bool,
    current: Option<EntryInfo>,
    entries: Vec<EntryInfo>,
}

#[tauri::command]
async fn list_dir(path: String, state: State<'_, AppState>) -> Result<Listing, String> {
    // A network folder (\\server\share\...) has no index: it is listed live, like an unscanned
    // drive. Known folders redirected to a share (company PCs) open this way from the sidebar.
    let network = path.starts_with(r"\\") && !path.starts_with(r"\\?\") && !path.starts_with(r"\\.\");
    let drive = drive_of(&path);
    if drive.is_none() && !network {
        return Err("경로가 올바르지 않아요".into());
    }
    if let Some(shared) = drive.as_deref().and_then(|d| state.get(d)) {
        let p = path.clone();
        let found = blocking(move || indexed_listing(&shared.read().unwrap(), &p)).await?;
        if let Some((current, entries)) = found {
            return Ok(Listing { path: current.path.clone(), indexed: true, current: Some(current), entries });
        }
    }
    // Not indexed yet (or created after the scan): list it live; folder sizes are unknown.
    let p = path.clone();
    let entries = blocking(move || live_list(&p)).await??;
    Ok(Listing { path, indexed: false, current: None, entries })
}

/// The folder and its children from the index. None for a path the index
/// does not know, and for a junction or folder link: scanners record the link
/// but not what it points to, so that is listed live instead of as empty.
/// A reparse point known to hold its own contents (FLAG_NOT_LINK, such as a
/// cloud-files folder) is listed from the index even when empty.
fn indexed_listing(ix: &Index, path: &str) -> Option<(EntryInfo, Vec<EntryInfo>)> {
    let n = ix.find(path)?;
    let link = FLAG_DIR | FLAG_REPARSE;
    if ix.nodes[n as usize].flags & (link | FLAG_NOT_LINK) == link && ix.children_of(n).next().is_none() {
        return None;
    }
    Some((ix.info(n), ix.list(n)))
}

fn live_list(path: &str) -> Result<Vec<EntryInfo>, String> {
    use std::os::windows::fs::MetadataExt;
    let read = std::fs::read_dir(path).map_err(|e| format!("폴더를 열 수 없어요: {e}"))?;
    Ok(read
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            let attrs = m.file_attributes();
            // From the attribute: std's is_dir() is false for junctions and folder symlinks.
            let is_dir = attrs & 0x10 != 0;
            Some(EntryInfo {
                name: e.file_name().to_string_lossy().into_owned(),
                path: e.path().to_string_lossy().into_owned(),
                is_dir,
                size: if is_dir { 0 } else { m.file_size() },
                alloc: 0,
                modified: m.last_write_time() as i64,
                files: 0,
                hidden: attrs & 2 != 0,
                system: attrs & 4 != 0,
                reparse: attrs & 0x400 != 0,
            })
        })
        .collect())
}

#[derive(Serialize)]
struct SearchResult {
    total: usize,
    items: Vec<EntryInfo>,
    elapsed_ms: u64,
    searched_drives: Vec<String>,
}

#[tauri::command]
async fn search(query: String, under: Option<String>, limit: Option<usize>, state: State<'_, AppState>) -> Result<SearchResult, String> {
    let limit = limit.unwrap_or(1000).min(20_000);
    let targets: Vec<Shared> = match under.as_deref().and_then(drive_of) {
        Some(d) => state.get(&d).into_iter().collect(),
        None => state.all(),
    };
    blocking(move || {
        let t = Instant::now();
        let mut total = 0;
        let mut items = Vec::new();
        let mut searched = Vec::new();
        for shared in targets {
            let ix = shared.read().unwrap();
            let root = under.as_deref().and_then(|u| ix.find(u));
            if under.is_some() && root.is_none() {
                continue;
            }
            let (n, mut found) = ix.search(&query, root, limit);
            total += n;
            items.append(&mut found);
            searched.push(ix.drive.clone());
        }
        items.sort_by_key(|e| std::cmp::Reverse(e.size));
        items.truncate(limit);
        SearchResult { total, items, elapsed_ms: t.elapsed().as_millis() as u64, searched_drives: searched }
    })
    .await
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    win::open_path(&path)
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    win::reveal_path(&path)
}

#[tauri::command]
async fn rename_path(path: String, new_name: String, app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    // win::rename takes both names exactly (`abc.` is never its neighbour `abc`), so the
    // new one is first made what Windows would make of it: no dots or spaces at its end.
    let new_name = ops::check_name(&new_name)?.to_string();
    let drive = drive_of(&path);
    let shared = drive.as_deref().and_then(|d| state.get(d));
    let (dst, changed) = blocking(move || -> Result<(String, bool), String> {
        let src = std::path::PathBuf::from(&path);
        let dst = src.with_file_name(&new_name);
        // No exists() check first: the OS refuses a name that is taken (with
        // no race), and still allows a case-only change such as é → É.
        win::rename(&src, &dst).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => "같은 이름이 이미 있어요".to_string(),
            _ => format!("이름을 바꿀 수 없어요: {e}"),
        })?;
        let changed = shared.is_some_and(|shared| {
            let mut ix = shared.write().unwrap();
            ix.find(&path).is_some_and(|n| ix.rename(n, &new_name))
        });
        Ok((dst.to_string_lossy().into_owned(), changed))
    })
    .await??;
    if let Some(d) = drive.filter(|_| changed) {
        save_later(&app, d);
    }
    Ok(dst)
}

#[derive(Serialize)]
struct TrashResult {
    moved: Vec<String>,
    failed: Vec<(String, String)>,
}

const NO_RECYCLE_BIN: &str = "이 드라이브에는 휴지통이 없어서 지우지 않았어요 (지우면 되돌릴 수 없어요)";
const BIN_DISABLED: &str = "이 드라이브는 휴지통을 쓰지 않도록 설정돼 있어서 지우지 않았어요 (지우면 되돌릴 수 없어요)";
const BIN_TOO_SMALL: &str = "휴지통보다 커서 지우지 않았어요. 휴지통 속성에서 최대 크기를 늘리면 옮길 수 있어요";
const ODD_NAME: &str = "이름이 점(.)이나 공백으로 끝나는 항목은 휴지통으로 옮길 수 없어서 지우지 않았어요";

/// Moves items to the Recycle Bin. Never deletes permanently: on a drive
/// without a Recycle Bin (network, USB stick, memory card, SUBST) Windows
/// would destroy the item, so it is refused and reported in `failed`.
#[tauri::command]
async fn trash_paths(paths: Vec<String>, app: AppHandle, state: State<'_, AppState>) -> Result<TrashResult, String> {
    let shared: HashMap<String, Shared> =
        paths.iter().filter_map(|p| drive_of(p)).filter_map(|d| state.get(&d).map(|s| (d, s))).collect();
    let (result, changed) = blocking(move || {
        let mut result = TrashResult { moved: Vec::new(), failed: Vec::new() };
        let mut changed = HashSet::new();
        for p in paths {
            if !win::has_recycle_bin(&p) {
                result.failed.push((p, NO_RECYCLE_BIN.into()));
                continue;
            }
            // The shell takes `abc.` for `abc` and would put that one in the bin.
            if !win::shell_can_name(Path::new(&p)) {
                result.failed.push((p, ODD_NAME.into()));
                continue;
            }
            // On a disk that has a Recycle Bin, Windows still deletes for good (after its
            // own prompt) when the bin is switched off or the item is bigger than the bin.
            let policy = win::recycle_policy(&p);
            if policy.nuke {
                result.failed.push((p, BIN_DISABLED.into()));
                continue;
            }
            let size = drive_of(&p)
                .and_then(|d| shared.get(&d))
                .and_then(|s| {
                    let ix = s.read().unwrap();
                    ix.find(&p).map(|n| ix.nodes[n as usize].size)
                })
                .or_else(|| std::fs::symlink_metadata(&p).ok().filter(|m| m.is_file()).map(|m| m.len()));
            if let (Some(size), Some(max)) = (size, policy.max_bytes)
                && size > max
            {
                result.failed.push((p, format!("{BIN_TOO_SMALL} (휴지통 최대 {}MB)", max >> 20)));
                continue;
            }
            match trash::delete(&p) {
                Ok(()) => {
                    if let Some((d, s)) = drive_of(&p).and_then(|d| shared.get(&d).map(|s| (d, s))) {
                        let mut ix = s.write().unwrap();
                        if let Some(n) = ix.find(&p) {
                            ix.remove(n);
                            changed.insert(d);
                        }
                    }
                    result.moved.push(p);
                }
                Err(e) => result.failed.push((p, e.to_string())),
            }
        }
        (result, changed)
    })
    .await?;
    for d in changed {
        save_later(&app, d);
    }
    Ok(result)
}

/// The size map of an indexed folder (see mapview.rs).
#[tauri::command]
async fn tree_map(path: String, depth: Option<u32>, min_ratio: Option<f64>, state: State<'_, AppState>) -> Result<mapview::MapNode, String> {
    let drive = drive_of(&path).ok_or("경로가 올바르지 않아요")?;
    let shared = state.get(&drive).ok_or("이 드라이브를 먼저 스캔해 주세요")?;
    let depth = depth.unwrap_or(4).clamp(1, 8);
    let min_ratio = min_ratio.unwrap_or(0.005).clamp(0.0, 0.1);
    blocking(move || {
        let ix = shared.read().unwrap();
        let node = ix.find(&path).ok_or("이 폴더는 스캔 결과에 없어요. 다시 스캔해 주세요")?;
        Ok(mapview::build(&ix, node, depth, min_ratio))
    })
    .await?
}

/// Starts a duplicate search over the given folders (indexed drives only) and
/// returns its id; progress arrives as "dup-progress" events.
#[tauri::command]
async fn find_duplicates(app: AppHandle, roots: Vec<String>, min_size: Option<u64>, state: State<'_, AppState>) -> Result<u64, String> {
    let min_size = min_size.unwrap_or(1 << 20);
    let mut targets = Vec::new();
    for root in roots {
        let drive = drive_of(&root).ok_or("경로가 올바르지 않아요")?;
        let shared = state.get(&drive).ok_or_else(|| format!("{drive} 드라이브를 먼저 스캔해 주세요"))?;
        targets.push((shared, root));
    }
    let candidates = blocking(move || {
        let mut all = Vec::new();
        for (shared, root) in targets {
            let ix = shared.read().unwrap();
            // As for tree_map: a folder the scan does not know is said so, not
            // searched as if it were empty. So is a junction or link, which the
            // index does not follow (a relocated folder, say).
            let node = ix.find(&root).ok_or_else(|| format!("이 폴더는 스캔 결과에 없어요. 다시 스캔해 주세요 ({root})"))?;
            let link = FLAG_DIR | FLAG_REPARSE;
            if ix.nodes[node as usize].flags & (link | FLAG_NOT_LINK) == link {
                return Err(format!("링크(정션) 폴더라서 찾지 않았어요. 링크가 가리키는 폴더에서 찾아 주세요 ({root})"));
            }
            all.extend(dups::collect(&ix, node, min_size));
        }
        Ok(all)
    })
    .await??;
    Ok(dups::start(app, candidates))
}

/// How `path` changed since the scan before the current one (see diff.rs),
/// down to `depth` levels (1..=6, 3 if not given). The index now, with its
/// live changes, is compared with the previous scan as that scan found it.
#[tauri::command]
async fn scan_diff(path: String, depth: Option<u32>, app: AppHandle, state: State<'_, AppState>) -> Result<diff::DiffNode, String> {
    let drive = drive_of(&path).ok_or("경로가 올바르지 않아요")?;
    let shared = state.get(&drive).ok_or("이 드라이브를 먼저 스캔해 주세요")?;
    let depth = depth.unwrap_or(3).clamp(1, 6);
    blocking(move || {
        let state = app.state::<AppState>();
        // The index in memory may be that same scan (loaded from a cache the
        // new scan could not replace), or an older one than the file.
        let now_at = shared.read().unwrap().scanned_at;
        let prev = state.previous(&drive, now_at)?;
        let ix = shared.read().unwrap();
        if prev.scanned_at >= ix.scanned_at {
            return Err(state.prev.lock().unwrap().none(&drive).to_string());
        }
        diff::build(&ix, &prev, &path, depth, DIFF_LIMIT).ok_or_else(|| "이 폴더는 스캔 결과에 없어요. 다시 스캔해 주세요".to_string())
    })
    .await?
}

/// Whether a drive has a previous scan to compare with, when the two were
/// made and how. Reads only the previous file's header, not the index.
#[tauri::command]
async fn diff_info(drive: String, state: State<'_, AppState>) -> Result<diff::DiffInfo, String> {
    let drive = scan::normalize_drive(&drive).ok_or("드라이브 이름이 올바르지 않아요")?;
    let shared = state.get(&drive);
    let loaded = state.prev.lock().unwrap().loaded.get(&drive).map(|l| (l.ix.scanned_at, l.ix.method.clone()));
    blocking(move || {
        let now = shared.map(|s| {
            let ix = s.read().unwrap();
            (ix.scanned_at, ix.method.clone())
        });
        let prev = loaded.or_else(|| scan::previous_scan(&drive).map(|h| (h.scanned_at, h.method)));
        let has_prev = matches!((&now, &prev), (Some((now, _)), Some((prev, _))) if prev < now);
        let ((scanned_at, method), (prev_scanned_at, prev_method)) = (now.unzip(), prev.unzip());
        // What each saw, for the warning about scans made differently: a sync sees what an MFT scan does.
        let (method, prev_method) = (method.map(diff::coverage), prev_method.map(diff::coverage));
        diff::DiffInfo { drive, has_prev, scanned_at, prev_scanned_at, method, prev_method }
    })
    .await
}

#[derive(Serialize)]
struct ModifierKeys {
    ctrl: bool,
    shift: bool,
}

/// Ctrl and Shift as they are held at this moment, for a drop to choose copy
/// or move the way Explorer does. The page gets no key events while a drag is
/// over the window (OLE runs the drag loop), so the keys it saw last may be
/// long gone, or the ones held now never reached it (a drag from another
/// program). Not on the UI thread, which may still be finishing the drop.
#[tauri::command]
async fn modifier_keys() -> ModifierKeys {
    let (ctrl, shift) = win::ctrl_shift_down();
    ModifierKeys { ctrl, shift }
}

/// Runs the MCP server when started as `fine-file.exe --mcp`.
pub fn run_mcp(args: &[String]) -> i32 {
    mcp::run(args)
}

/// Runs the admin-session helper when started as `fine-file.exe --admin-helper <pipe>`.
pub fn run_admin_helper(args: &[String]) -> i32 {
    admin::run_helper(args)
}

/// Runs the elevated MFT helper when started as
/// `fine-file.exe --scan-helper C: \\.\pipe\fine-file-<pid>-<hex>`.
pub fn run_scan_helper(args: &[String]) -> i32 {
    scan::run_helper(args)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_drag::init())
        // Updates (v0.8): plugins.updater in tauri.conf.json; checked and installed from Updater.tsx.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // The taskbar and Alt+Tab show a window's big icon, which Tauri leaves unset.
            for w in app.webview_windows().values() {
                if let Ok(h) = w.hwnd() {
                    win::set_window_icons(h.0 as usize);
                }
            }
            let handle = app.handle().clone();
            std::thread::Builder::new().name("cache saver".into()).spawn(move || save_dirty_caches(handle))?;
            let handle = app.handle().clone();
            std::thread::Builder::new().name("auto sync".into()).spawn(move || auto_sync(handle))?;
            // A relocation or undo cut short by a crash or a power cut is put right (ops.rs).
            std::thread::Builder::new().name("relocation recovery".into()).spawn(ops::recover_relocations)?;
            // So is a merge of duplicates into hard links; what it did shows as a job.
            let handle = app.handle().clone();
            std::thread::Builder::new().name("link recovery".into()).spawn(move || ops::recover_links(handle))?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            get_drives,
            load_cached,
            scan_drive,
            live_status,
            list_dir,
            search,
            open_path,
            reveal_path,
            rename_path,
            trash_paths,
            ops::create_folder,
            ops::start_transfer,
            ops::resolve_conflict,
            ops::cancel_job,
            ops::jobs,
            ops::pending_conflicts,
            ops::clipboard_read,
            ops::clipboard_write,
            preview::thumbnail,
            preview::preview_info,
            tree_map,
            find_duplicates,
            dups::cancel_dup,
            dups::dup_results,
            ops::start_relocate,
            ops::relocations,
            ops::undo_relocate,
            ops::start_dup_link,
            admin::admin_session_start,
            admin::admin_session_status,
            admin::admin_session_stop,
            admin::admin_list,
            admin::admin_read,
            scan_diff,
            diff_info,
            shellmenu::shell_menu,
            modifier_keys,
            ai::ai_get_settings,
            ai::ai_set_settings,
            ai::ai_key_status,
            ai::ai_set_key,
            ai::ai_test,
            ai::ai_preview,
            ai::ai_explain,
            ai::ai_plan,
            ai::jev_risk,
            meaning::jev_find,
            plans::plans_list,
            plans::plan_get,
            plans::plan_rules,
            plans::plan_update,
            plans::plan_execute,
            plans::plan_undo,
            plans::plan_delete,
            programs::programs_list,
            programs::program_uninstall,
            programs::program_leftovers,
            programs::program_leftovers_plan,
            sync_drive,
            update_others,
            update_prepare,
            update_install_failed,
            places::known_places,
            places::explorer_pins,
            places::places_get,
            places::places_set,
            places::path_kind,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|app, event| match event {
        RunEvent::WindowEvent { label, event: WindowEvent::CloseRequested { api, .. }, .. }
            if ops::active_jobs() > 0 || plans::running() =>
        {
            api.prevent_close();
            confirm_close(app, &label);
        }
        RunEvent::Exit => {
            // A plan step under way finishes and is written down in its undo journal; no other starts.
            plans::stop_and_wait(Duration::ZERO);
            // A copy cut off midway never has its real name (it is written under a hidden
            // partial name, ops.rs); cancelled jobs also remove that partial file before they stop.
            ops::cancel_all();
            save_on_exit(app);
            ops::wait_all(EXIT_JOB_WAIT);
            plans::stop_and_wait(EXIT_PLAN_WAIT);
        }
        _ => {}
    });
}

/// How long closing the app waits for cancelled jobs to stop...
const CLOSE_JOB_WAIT: Duration = Duration::from_secs(10);
const EXIT_JOB_WAIT: Duration = Duration::from_secs(5);
/// ...and for a plan's step under way to finish and be written down (a big
/// folder to the Recycle Bin, a big file compressed, can take a while).
const CLOSE_PLAN_WAIT: Duration = Duration::from_secs(120);
const EXIT_PLAN_WAIT: Duration = Duration::from_secs(30);
const CLOSE_TITLE: &str = "fine-file";
const CLOSE_PROMPT: &str = "복사·이동 작업이 아직 끝나지 않았어요.\n\n지금 끄면 작업을 취소해요. 쓰다 만 복사본은 지우고, 다 옮기지 못한 항목의 원본은 그대로 남겨요.\n\n그래도 끌까요?";
const CLOSE_PLAN_PROMPT: &str = "정리 계획을 실행하거나 되돌리는 중이에요.\n\n지금 끄면 하던 단계까지만 마치고 되돌리기 기록을 저장한 뒤에 꺼요. 남은 단계는 하지 않아요 (한 단계는 나중에 되돌릴 수 있어요). 복사·이동 작업과 다른 드라이브로 옮기는 중인 단계는 취소하고, 다 옮기지 못한 항목의 원본은 그대로 남겨요.\n\n그래도 끌까요?";

/// Closing the window while a copy or move is going, or a plan runs or is
/// undone, asks first. Yes stops the plan after the step it is at (with how
/// to undo it written down), cancels the jobs, waits for both to clean up
/// after themselves, then quits. The question is a message box on a thread
/// of its own, so the window keeps drawing (and the work keeps going) while
/// it is open.
fn confirm_close(app: &AppHandle, label: &str) {
    static ASKING: AtomicBool = AtomicBool::new(false);
    if ASKING.swap(true, Ordering::SeqCst) {
        return;
    }
    let owner = app.get_webview_window(label).and_then(|w| w.hwnd().ok()).map(|h| h.0 as usize);
    let app = app.clone();
    let prompt = if plans::running() { CLOSE_PLAN_PROMPT } else { CLOSE_PROMPT };
    let asked = std::thread::Builder::new().name("close prompt".into()).spawn(move || {
        if win::ask_yes_no(owner, CLOSE_TITLE, prompt) {
            // First no next step, then the jobs (a step may wait on one), then the step itself.
            plans::stop_and_wait(Duration::ZERO);
            ops::cancel_all();
            ops::wait_all(CLOSE_JOB_WAIT);
            plans::stop_and_wait(CLOSE_PLAN_WAIT);
            app.exit(0);
        }
        ASKING.store(false, Ordering::SeqCst);
    });
    if asked.is_err() {
        ASKING.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use index::RawEntry;

    #[test]
    fn the_updater_reads_a_signed_https_release_and_installs_it_passively() {
        use base64::Engine;
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["bundle"]["createUpdaterArtifacts"], true, "the build writes the installer's .sig");
        let updater: tauri_plugin_updater::Config = serde_json::from_value(conf["plugins"]["updater"].clone()).unwrap();
        let endpoints: Vec<&str> = updater.endpoints.iter().map(|u| u.as_str()).collect();
        assert_eq!(endpoints, ["https://github.com/toxh13/fine-file/releases/latest/download/latest.json"]);
        assert!(updater.require_signed_version, "a release is only what its signature says it is");
        assert!(!updater.allow_downgrades && !updater.dangerous_insecure_transport_protocol);
        assert_eq!(updater.windows.unwrap().install_mode.to_string(), "passive");
        let key = base64::engine::general_purpose::STANDARD.decode(updater.pubkey.trim()).unwrap();
        assert!(String::from_utf8(key).unwrap().starts_with("untrusted comment: minisign public key: 8453D46DA3F3AACE\n"));
    }

    #[test]
    fn links_list_live_folders_list_from_the_index() {
        let e = |id, parent, name: &str, flags| RawEntry { id, parent, name: name.into(), flags, size: 0, alloc: 0, modified: 0 };
        let raw = vec![
            e(5, 5, ".", FLAG_DIR),
            e(6, 5, "Application Data", FLAG_DIR | FLAG_REPARSE),
            // A cloud-files folder is a reparse point whose children are indexed.
            e(7, 5, "OneDrive", FLAG_DIR | FLAG_REPARSE),
            e(8, 7, "doc.txt", 0),
            e(9, 5, "plain", FLAG_DIR),
            // Known to be a placeholder: an empty one is still the index's to list.
            e(10, 5, "Empty cloud", FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK),
        ];
        let ix = Index::build("C:", "test", 0, raw, 5);
        assert!(indexed_listing(&ix, "C:\\Application Data").is_none());
        assert_eq!(indexed_listing(&ix, "C:\\OneDrive").unwrap().1.len(), 1);
        assert!(indexed_listing(&ix, "C:\\Empty cloud").unwrap().1.is_empty());
        assert!(indexed_listing(&ix, "C:\\plain").unwrap().1.is_empty());
        assert!(indexed_listing(&ix, "C:\\missing").is_none());
    }

    #[test]
    fn failed_saves_back_off() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let d = Dirty { since: t0, last: t0 };
        // Quiet for a minute with no recent save: due. Changing for 30 min: due anyway.
        assert!(!save_due(&d, None, None, at(30)));
        assert!(save_due(&d, None, None, at(60)));
        assert!(!save_due(&d, Some(at(30)), None, at(120)));
        assert!(save_due(&Dirty { since: t0, last: at(1790) }, Some(at(1700)), None, at(1800)));
        // After failures the next try waits 10 min, 20 min, 40 min, ... up to 4 h,
        // the busy-drive rule included.
        let failed = |count| Failed { at: at(60), count };
        assert!(!save_due(&d, None, Some(&failed(1)), at(60 + 599)));
        assert!(save_due(&d, None, Some(&failed(1)), at(60 + 600)));
        assert!(!save_due(&d, None, Some(&failed(2)), at(60 + 1199)));
        assert!(save_due(&d, None, Some(&failed(2)), at(60 + 1200)));
        assert!(!save_due(&d, None, Some(&failed(3)), at(1800)));
        assert!(!save_due(&d, None, Some(&failed(40)), at(60 + 4 * 3600 - 1)));
        assert!(save_due(&d, None, Some(&failed(40)), at(60 + 4 * 3600)));
    }

    #[test]
    fn a_rescan_lets_go_of_the_loaded_previous_scan() {
        let root = || RawEntry { id: 5, parent: 5, name: ".".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: 0 };
        let ix = || Arc::new(Index::build("C:", "test", 0, vec![root()], 5));
        let t0 = Instant::now();
        let mut prev = Previous::default();
        let t = prev.turn("C:");
        assert!(prev.keep("C:", t, ix(), t0));
        assert!(prev.loaded.contains_key("C:"));
        // A scan of another drive leaves it.
        prev.replaced("D:");
        assert!(prev.loaded.contains_key("C:"));
        // A scan of the drive drops it, and a load that began before is not kept.
        let before = prev.turn("C:");
        prev.replaced("C:");
        assert!(!prev.loaded.contains_key("C:"));
        assert!(!prev.keep("C:", before, ix(), t0));
        assert!(prev.loaded.is_empty());
        let now = prev.turn("C:");
        assert!(prev.keep("C:", now, ix(), t0));
    }

    #[test]
    fn an_unused_previous_scan_is_let_go() {
        let root = || RawEntry { id: 5, parent: 5, name: ".".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: 0 };
        let ix = || Arc::new(Index::build("C:", "test", 0, vec![root()], 5));
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut prev = Previous::default();
        assert!(prev.keep("C:", 0, ix(), t0));
        assert!(prev.keep("D:", 0, ix(), t0));
        // C: is compared again a minute later, D: is not.
        assert!(prev.get("C:", at(60)).is_some());
        assert!(prev.idle(at(299), PREV_IDLE).is_empty());
        assert_eq!(prev.idle(at(300), PREV_IDLE).len(), 1);
        assert!(prev.loaded.contains_key("C:") && !prev.loaded.contains_key("D:"));
        assert_eq!(prev.idle(at(360), PREV_IDLE).len(), 1);
        assert!(prev.loaded.is_empty());
        // The file was not replaced: a load now is kept.
        let t = prev.turn("C:");
        assert!(prev.keep("C:", t, ix(), at(400)));
    }

    #[test]
    fn a_kept_scan_gives_way_only_when_that_makes_room() {
        let root = RawEntry { id: 5, parent: 5, name: ".".into(), flags: FLAG_DIR, size: 0, alloc: 0, modified: 0 };
        let shared: Shared = Arc::new(RwLock::new(Index::build("C:", "test", 0, vec![root], 5)));
        let dir = std::env::temp_dir().join(format!("ff-room-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (cache, prev, copy) = (dir.join("C.idx"), dir.join("C.prev.idx"), dir.join("C.scan.idx"));
        std::fs::write(&cache, shared.read().unwrap().to_bytes()).unwrap();
        std::fs::write(&prev, shared.read().unwrap().to_bytes()).unwrap();
        // Nothing there, or still the cache under another name: removing it frees nothing.
        assert!(!frees_enough(&copy, &cache, &shared, 0));
        if std::fs::hard_link(&cache, &copy).is_ok() {
            assert!(!frees_enough(&copy, &cache, &shared, 0));
        }
        // A file of its own frees its size, which may or may not be enough.
        assert!(frees_enough(&prev, &cache, &shared, 0));
        assert!(!frees_enough(&prev, &cache, &shared, u64::MAX / 2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_drives_caught_up_this_session_are_in_step_at_exit() {
        let t0 = Instant::now();
        let watching = || watch::Status { state: "watching", detail: None };
        let statuses = |drives: &[&str]| drives.iter().map(|d| (d.to_string(), watching())).collect::<Vec<_>>();
        let mut auto = sync::AutoSync::default();
        let mut jobs = sync::Jobs::default();
        auto.after_startup(["C:".to_string(), "D:".to_string()], t0);
        // Closed within the first seconds: no startup sync ran, nothing is in step.
        assert!(in_step_at_exit(statuses(&["C:", "D:"]), &jobs, &auto).is_empty());
        // C:'s startup sync ran; D:'s still waits behind it.
        assert_eq!(auto.next(t0 + sync::STARTUP_DELAY), Some(("C:".into(), sync::Reason::Startup)));
        auto.installed("C:");
        assert_eq!(in_step_at_exit(statuses(&["C:", "D:"]), &jobs, &auto), ["C:"]);
        // Put off (in step a few minutes before startup): still not in step.
        assert_eq!(auto.next(t0 + sync::STARTUP_DELAY), Some(("D:".into(), sync::Reason::Startup)));
        auto.defer("D:", t0 + sync::FRESH);
        assert_eq!(in_step_at_exit(statuses(&["C:", "D:"]), &jobs, &auto), ["C:"]);
        // E: scanned for the first time this session.
        auto.installed("E:");
        let mut up = in_step_at_exit(statuses(&["C:", "D:", "E:"]), &jobs, &auto);
        up.sort();
        assert_eq!(up, ["C:", "E:"]);
        // Caught up, but missing changes since, or being synced again: not in step.
        let stale = vec![("C:".to_string(), watch::Status { state: "stale", detail: None })];
        assert!(in_step_at_exit(stale, &jobs, &auto).is_empty());
        let ctl = jobs.sync_started("E:").unwrap();
        assert!(in_step_at_exit(statuses(&["E:"]), &jobs, &auto).is_empty());
        jobs.sync_ended("E:", &ctl);
        assert_eq!(in_step_at_exit(statuses(&["E:"]), &jobs, &auto), ["E:"]);
    }

    #[test]
    fn no_previous_scan_says_why() {
        let mut prev = Previous::default();
        assert_eq!(prev.none("C:"), NO_PREV);
        prev.no_room.insert("C:".into());
        assert_eq!(prev.none("C:"), NO_PREV_ROOM);
        assert_eq!(prev.none("D:"), NO_PREV);
    }

    #[test]
    fn the_installer_refreshes_icons() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let hooks = conf["bundle"]["windows"]["nsis"]["installerHooks"].as_str().unwrap();
        let nsh = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(hooks)).unwrap();
        assert!(nsh.contains("!macro NSIS_HOOK_POSTINSTALL"), "{nsh}");
        assert!(nsh.contains("ie4uinit.exe\" -show") && nsh.contains("shell32::SHChangeNotify"), "{nsh}");
    }

    #[test]
    fn live_listing_shows_junctions_as_folders() {
        let Some(root) = scan::walk::tests::tree_with_junction("live") else {
            eprintln!("mklink /J is not available; skipped");
            return;
        };
        let listed = live_list(&root.to_string_lossy());
        let inside = live_list(&root.join("link").to_string_lossy());
        std::fs::remove_dir_all(&root).unwrap();
        let link = listed.unwrap().into_iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_dir && link.reparse);
        assert_eq!(link.size, 0);
        assert_eq!(inside.unwrap().len(), 1);
    }
}
