//! Duplicate finder: files of equal size from the index, then a hash of a few
//! blocks, then a full hash (blake3) of what still matches. Runs as a
//! background job; progress arrives as "dup-progress" events (DupProgress,
//! at most ~5 per second and on every phase change).
//!
//! A job only reads. Every file is opened with read, write and delete
//! sharing, so it never stands in another program's way, and never through
//! a link (FILE_FLAG_OPEN_REPARSE_POINT). A file that is not what the index
//! said any more (another size or time, gone, a folder or a link now) is
//! left out, and so is one that cannot be opened, and one that is only in
//! the cloud: reading that would make the sync app download it. Two names
//! of one file (hard links) are one file, not duplicates. What was left out
//! is counted in the events' detail.
//!
//! Every std::fs call goes through `win::verbatim`, so a name ending in a dot
//! or a space is that name and not its neighbour without the dot.

use crate::index::{Index, FLAG_DELETED, FLAG_DIR, FLAG_HARDLINK, FLAG_NOT_LINK, FLAG_REPARSE};
use crate::win;
use rayon::prelude::*;
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};

/// Bytes in each block of the quick hash (the first, middle and last one).
const BLOCK: u64 = 64 * 1024;
/// A file up to this size is read whole by the quick hash, which is then
/// its full hash too.
const WHOLE: u64 = 3 * BLOCK;
/// Read size of the full hash.
const FULL_BUF: usize = 1 << 20;
/// Hashing threads at most: a few, since a disk reads best in one stream.
const THREADS: usize = 3;
/// Jobs kept with their results; older finished ones are forgotten.
const KEEP_JOBS: usize = 5;

const NO_JOB: &str = "중복 찾기 작업을 찾을 수 없어요";
const FAILED: &str = "예기치 못한 문제로 중복 찾기를 멈췄어요";

/// A file the index knows, worth comparing.
pub struct Candidate {
    pub path: String,
    pub size: u64,
    pub modified: i64,
}

#[derive(Serialize, Clone)]
pub struct DupProgress {
    pub id: u64,
    /// "size" | "partial" | "full" | "done" | "cancelled" | "failed"
    pub phase: String,
    /// Files in "size"; bytes read in "partial" and "full".
    pub done: u64,
    pub total: u64,
    /// Groups found so far, and the bytes they waste (all copies but one).
    pub groups: u64,
    pub wasted: u64,
    /// The files left out so far and why; why a job failed.
    pub detail: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct DupFile {
    pub path: String,
    pub modified: i64,
    /// The file's category ("program", "game", "system", ...): "system",
    /// "program", "game" or "dev" whenever the folders it is in are such
    /// (`classify::program_category`, app data counted as "program"), else
    /// `classify::category`. The window keeps automatic picks away from
    /// those four.
    pub category: &'static str,
}

#[derive(Serialize, Clone)]
pub struct DupGroup {
    pub size: u64,
    /// blake3 of the whole content, in hex.
    pub hash: String,
    /// Newest first.
    pub files: Vec<DupFile>,
}

#[derive(Serialize)]
pub struct DupReport {
    pub id: u64,
    /// DupProgress.phase of the job now.
    pub state: String,
    /// Most wasted bytes first.
    pub groups: Vec<DupGroup>,
    pub wasted: u64,
    pub scanned_files: u64,
    /// The last event's detail once the job is over (the files left out and
    /// why, why it failed), so it can stay with the results.
    pub detail: Option<String>,
}

/// Files under `root` (live, not hard-link extras, at least `min_size`
/// bytes, and never empty). Folder links are not followed; cloud-files
/// placeholder folders are, since what they hold is on this drive.
pub fn collect(ix: &Index, root: u32, min_size: u64) -> Vec<Candidate> {
    let min_size = min_size.max(1);
    let wanted = |flags: u32, size: u64| flags & (FLAG_DIR | FLAG_DELETED | FLAG_HARDLINK | FLAG_REPARSE) == 0 && size >= min_size;
    let mut out = Vec::new();
    let Some(top) = ix.nodes.get(root as usize) else { return out };
    if top.flags & FLAG_DELETED != 0 {
        return out;
    }
    if top.flags & FLAG_DIR == 0 {
        if wanted(top.flags, top.size) {
            out.push(Candidate { path: ix.path_of(root), size: top.size, modified: top.modified });
        }
        return out;
    }
    // A stack, not recursion: folders can nest deeper than a thread's stack.
    let mut stack = vec![(root, ix.path_of(root))];
    while let Some((dir, path)) = stack.pop() {
        // The drive root's path ends in `\` already.
        let base = path.strip_suffix('\\').unwrap_or(&path);
        for c in ix.children_of(dir) {
            let n = &ix.nodes[c as usize];
            if n.flags & FLAG_DELETED != 0 {
                continue;
            }
            if n.flags & FLAG_DIR != 0 {
                if n.flags & (FLAG_REPARSE | FLAG_NOT_LINK) != FLAG_REPARSE {
                    stack.push((c, format!("{base}\\{}", ix.name(c))));
                }
            } else if wanted(n.flags, n.size) {
                out.push(Candidate { path: format!("{base}\\{}", ix.name(c)), size: n.size, modified: n.modified });
            }
        }
    }
    out
}

/// Starts a job over `candidates` and returns its id at once.
pub fn start(app: AppHandle, candidates: Vec<Candidate>) -> u64 {
    launch(app, candidates, Settings::app())
}

#[tauri::command]
pub fn cancel_dup(id: u64) -> Result<(), String> {
    let mut reg = registry();
    let job = reg.get(id).ok_or(NO_JOB)?;
    job.cancel.store(true, Ordering::Relaxed);
    Ok(())
}

/// The groups a job has found so far (all of them once it is done; those
/// confirmed before a cancel too). Off the main thread: there may be many.
#[tauri::command]
pub async fn dup_results(id: u64) -> Result<DupReport, String> {
    tauri::async_runtime::spawn_blocking(move || report(id)).await.map_err(|e| e.to_string())?
}

fn report(id: u64) -> Result<DupReport, String> {
    let (state, mut groups, wasted, scanned_files, detail) = {
        let mut reg = registry();
        let job = reg.get(id).ok_or(NO_JOB)?;
        (job.state, job.groups.clone(), job.wasted, job.scanned_files, job.detail.clone())
    };
    groups.sort_by(|a, b| {
        wasted_by(b).cmp(&wasted_by(a)).then_with(|| b.size.cmp(&a.size)).then_with(|| a.hash.cmp(&b.hash))
    });
    Ok(DupReport { id, state: state.into(), groups, wasted, scanned_files, detail })
}

/// The groups found so far by the job `id`, or by every job kept (the
/// newest job's first), for merging copies into hard links (ops.rs,
/// `start_dup_link`). Each job's groups are unsorted.
pub(crate) fn found(id: Option<u64>) -> Result<Vec<DupGroup>, String> {
    let reg = registry();
    let jobs: Vec<&Job> = reg.jobs.iter().rev().filter(|j| id.is_none_or(|id| j.id == id)).collect();
    if jobs.is_empty() {
        return Err(NO_JOB.into());
    }
    Ok(jobs.into_iter().flat_map(|j| j.groups.iter().cloned()).collect())
}

/// Bytes a group would free if one copy were kept.
fn wasted_by(g: &DupGroup) -> u64 {
    g.size.saturating_mul(g.files.len().saturating_sub(1) as u64)
}

/// Where a job's events go: the window, or a test.
trait Events: Send + Sync + 'static {
    fn progress(&self, p: &DupProgress);
}

impl Events for AppHandle {
    fn progress(&self, p: &DupProgress) {
        let _ = self.emit("dup-progress", p);
    }
}

/// How a job runs; tests turn the knobs.
#[derive(Clone, Copy)]
struct Settings {
    /// Progress events of a job are at least this far apart (phase changes excepted).
    gap: Duration,
    threads: usize,
}

impl Settings {
    fn app() -> Settings {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        Settings { gap: Duration::from_millis(200), threads: cores.clamp(1, THREADS) }
    }
}

/// One job as `dup_results` sees it.
struct Job {
    id: u64,
    cancel: Arc<AtomicBool>,
    state: &'static str,
    groups: Vec<DupGroup>,
    wasted: u64,
    scanned_files: u64,
    /// Set when it ends: DupReport.detail.
    detail: Option<String>,
}

struct Registry {
    last_id: u64,
    /// Oldest first.
    jobs: Vec<Job>,
}

impl Registry {
    fn get(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    /// Forgets the oldest finished jobs while there are more than KEEP_JOBS.
    /// A running job is never forgotten.
    fn prune(&mut self) {
        while self.jobs.len() > KEEP_JOBS {
            let Some(old) = self.jobs.iter().position(|j| is_finished(j.state)) else { break };
            self.jobs.remove(old);
        }
    }
}

fn is_finished(state: &str) -> bool {
    matches!(state, "done" | "cancelled" | "failed")
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry { last_id: 0, jobs: Vec::new() });

/// The registry, also after a thread panicked while holding it: it only
/// holds results and flags, nothing a half-done update could break.
fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn launch(events: impl Events, candidates: Vec<Candidate>, settings: Settings) -> u64 {
    let (id, run) = new_run(events, candidates.len(), settings);
    let job = run.clone();
    let threads = settings.threads.max(1);
    let spawned = std::thread::Builder::new().name(format!("dups {id}")).spawn(move || job.go(candidates, threads));
    if let Err(e) = spawned {
        run.finish("failed", Some(&format!("중복 찾기를 시작할 수 없어요: {e}")));
    }
    id
}

/// Runs a search over `candidates` on this thread to its end and gives what
/// it found, for a caller that wants the answer rather than a job to watch
/// (plans.rs). It is in the registry like any job, so `cancel_dup` stops it.
pub(crate) fn find_now(candidates: Vec<Candidate>) -> Result<DupReport, String> {
    struct Quiet;
    impl Events for Quiet {
        fn progress(&self, _: &DupProgress) {}
    }
    let settings = Settings::app();
    let (id, run) = new_run(Quiet, candidates.len(), settings);
    run.go(candidates, settings.threads.max(1));
    report(id)
}

/// Puts a new job in the registry and gives its id and the Run that does it.
fn new_run(events: impl Events, scanned: usize, settings: Settings) -> (u64, Arc<Run>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let id = {
        let mut reg = registry();
        reg.last_id += 1;
        let id = reg.last_id;
        reg.jobs.push(Job {
            id,
            cancel: cancel.clone(),
            state: "size",
            groups: Vec::new(),
            wasted: 0,
            scanned_files: scanned as u64,
            detail: None,
        });
        reg.prune();
        id
    };
    let run = Arc::new(Run {
        id,
        events: Box::new(events),
        cancel,
        gap: settings.gap,
        phase: Mutex::new("size"),
        done: AtomicU64::new(0),
        total: AtomicU64::new(0),
        groups: AtomicU64::new(0),
        wasted: AtomicU64::new(0),
        skipped: Default::default(),
        last_event: Mutex::new(None),
    });
    (id, run)
}

/// Why a file was left out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Skip {
    /// Not what the index said: another size or time, gone, or no plain file now.
    Changed,
    Unreadable,
    /// Online-only in a cloud-sync folder.
    Cloud,
}

impl Skip {
    fn slot(self) -> usize {
        self as usize
    }
}

/// The job was cancelled.
struct Cancelled;

/// Why reading one file stopped.
enum Halt {
    Skip(Skip),
    Cancelled,
}

impl From<Cancelled> for Halt {
    fn from(_: Cancelled) -> Halt {
        Halt::Cancelled
    }
}

/// Why a whole job stopped.
enum Stop {
    Cancelled,
    Failed(String),
}

impl From<Cancelled> for Stop {
    fn from(_: Cancelled) -> Stop {
        Stop::Cancelled
    }
}

/// One job, on its own thread and the hashing threads it shares itself with.
struct Run {
    id: u64,
    events: Box<dyn Events>,
    cancel: Arc<AtomicBool>,
    gap: Duration,
    phase: Mutex<&'static str>,
    done: AtomicU64,
    total: AtomicU64,
    groups: AtomicU64,
    wasted: AtomicU64,
    /// Files left out, by `Skip::slot`.
    skipped: [AtomicU64; 3],
    last_event: Mutex<Option<Instant>>,
}

impl Run {
    fn go(&self, candidates: Vec<Candidate>, threads: usize) {
        let worked = catch_unwind(AssertUnwindSafe(|| self.work(candidates, threads)));
        match worked {
            Ok(Ok(())) => self.finish("done", None),
            Ok(Err(Stop::Cancelled)) => self.finish("cancelled", None),
            Ok(Err(Stop::Failed(why))) => self.finish("failed", Some(&why)),
            Err(_) => self.finish("failed", Some(FAILED)),
        }
    }

    fn work(&self, mut cands: Vec<Candidate>, threads: usize) -> Result<(), Stop> {
        // Size: only a size that two files share can hold duplicates.
        self.begin("size", cands.len() as u64);
        cands.sort_unstable_by(|a, b| a.size.cmp(&b.size).then_with(|| a.path.cmp(&b.path)));
        // Overlapping roots give some files twice.
        cands.dedup_by(|a, b| a.path == b.path);
        if let Some(job) = registry().get(self.id) {
            job.scanned_files = cands.len() as u64;
        }
        let mut shared = Vec::new();
        let mut i = 0;
        while i < cands.len() {
            let n = cands[i..].iter().take_while(|c| c.size == cands[i].size).count();
            if n >= 2 {
                shared.extend(i..i + n);
            }
            i += n;
        }
        self.done.store(cands.len() as u64, Ordering::Relaxed);
        self.check()?;

        let id = self.id;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(move |n| format!("dups {id} hash {n}"))
            .build()
            .map_err(|e| Stop::Failed(format!("중복 찾기를 시작할 수 없어요: {e}")))?;

        // Partial: a few blocks of each, in path order so that a disk reads
        // folder by folder.
        let mut order = shared;
        order.sort_unstable_by(|&a, &b| cands[a].path.cmp(&cands[b].path));
        self.begin("partial", order.iter().map(|&i| quick_len(cands[i].size)).sum());
        let probes: Vec<Option<Probe>> =
            pool.install(|| order.par_iter().map(|&i| self.quick(&cands[i])).collect::<Result<_, _>>())?;
        let mut buckets: HashMap<(u64, [u8; 32]), Vec<Named>> = HashMap::new();
        for (&i, probe) in order.iter().zip(probes) {
            if let Some(p) = probe {
                buckets.entry((cands[i].size, p.hash)).or_default().push((i, p.key));
            }
        }
        let mut small = Vec::new();
        let mut big = Vec::new();
        for ((size, hash), files) in buckets {
            let files = one_name_each(files);
            if files.len() < 2 {
                continue;
            }
            // The quick hash read all of a small file: that match is final.
            if size <= WHOLE {
                small.push(group(&cands, size, hash, files));
            } else {
                big.push((size, files));
            }
        }
        self.add(small);
        self.check()?;

        // Full: all of every file whose blocks matched another's. Biggest
        // groups first, so the threads end at about the same time.
        big.sort_unstable_by_key(|(size, files)| Reverse(size.saturating_mul(files.len() as u64)));
        self.begin("full", big.iter().map(|(size, files)| size.saturating_mul(files.len() as u64)).sum());
        pool.install(|| big.par_iter().try_for_each(|(size, files)| self.full_group(&cands, *size, files)))?;
        Ok(())
    }

    /// The quick hash of one file; None when it is left out.
    fn quick(&self, c: &Candidate) -> Result<Option<Probe>, Cancelled> {
        self.check()?;
        let mut read = 0;
        match self.quick_read(c, &mut read) {
            Ok(p) => Ok(Some(p)),
            Err(Halt::Skip(why)) => {
                self.skip(why, quick_len(c.size) - read);
                Ok(None)
            }
            Err(Halt::Cancelled) => Err(Cancelled),
        }
    }

    fn quick_read(&self, c: &Candidate, read: &mut u64) -> Result<Probe, Halt> {
        let (mut file, key) = open(c, false).map_err(Halt::Skip)?;
        let mut buf = vec![0; BLOCK as usize];
        let hash = hash_parts(&mut file, &quick_ranges(c.size), &mut buf, &mut |n| self.step(n, read))?;
        unchanged(&file, c).map_err(Halt::Skip)?;
        Ok(Probe { hash: *hash.as_bytes(), key })
    }

    /// Full hashes of one group whose quick hashes matched; adds the
    /// duplicates found.
    fn full_group(&self, cands: &[Candidate], size: u64, files: &[usize]) -> Result<(), Cancelled> {
        let mut buf = vec![0; FULL_BUF];
        let mut keys = HashSet::new();
        let mut by_hash: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
        let mut hashed = 0;
        for (n, &i) in files.iter().enumerate() {
            let left = files.len() - n;
            if hashed + left < 2 {
                // The rest cannot make a pair any more.
                self.advance(size.saturating_mul(left as u64));
                break;
            }
            self.check()?;
            let mut read = 0;
            match self.full_read(&cands[i], &mut keys, &mut buf, &mut read) {
                Ok(Some(hash)) => {
                    by_hash.entry(hash).or_default().push(i);
                    hashed += 1;
                }
                // Another name of a file hashed already: one file, not two.
                Ok(None) => self.advance(size),
                Err(Halt::Skip(why)) => self.skip(why, size - read),
                Err(Halt::Cancelled) => return Err(Cancelled),
            }
        }
        let found = by_hash
            .into_iter()
            .filter(|(_, f)| f.len() >= 2)
            .map(|(hash, f)| group(cands, size, hash, f))
            .collect();
        self.add(found);
        Ok(())
    }

    /// The full hash of one file; None when it is a file in `keys` already
    /// (through another hard link), which then is not read.
    fn full_read(
        &self,
        c: &Candidate,
        keys: &mut HashSet<FileKey>,
        buf: &mut [u8],
        read: &mut u64,
    ) -> Result<Option<[u8; 32]>, Halt> {
        let (mut file, key) = open(c, true).map_err(Halt::Skip)?;
        if key.is_some_and(|k| !keys.insert(k)) {
            return Ok(None);
        }
        let hash = hash_parts(&mut file, &[(0, c.size)], buf, &mut |n| self.step(n, read))?;
        unchanged(&file, c).map_err(Halt::Skip)?;
        Ok(Some(*hash.as_bytes()))
    }

    /// `n` more bytes of a file read; the reading goes on unless the job was cancelled.
    fn step(&self, n: u64, read: &mut u64) -> Result<(), Halt> {
        *read += n;
        self.advance(n);
        self.check().map_err(Halt::from)
    }

    fn check(&self) -> Result<(), Cancelled> {
        if self.cancel.load(Ordering::Relaxed) { Err(Cancelled) } else { Ok(()) }
    }

    fn advance(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
        self.tick();
    }

    /// Counts a file left out; `unread` of its bytes count as done.
    fn skip(&self, why: Skip, unread: u64) {
        self.skipped[why.slot()].fetch_add(1, Ordering::Relaxed);
        self.advance(unread);
    }

    fn add(&self, found: Vec<DupGroup>) {
        if found.is_empty() {
            return;
        }
        let wasted: u64 = found.iter().map(wasted_by).sum();
        self.groups.fetch_add(found.len() as u64, Ordering::Relaxed);
        self.wasted.fetch_add(wasted, Ordering::Relaxed);
        if let Some(job) = registry().get(self.id) {
            job.groups.extend(found);
            job.wasted += wasted;
        }
        self.tick();
    }

    fn begin(&self, phase: &'static str, total: u64) {
        *lock(&self.phase) = phase;
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        if let Some(job) = registry().get(self.id) {
            job.state = phase;
        }
        self.emit(None);
    }

    fn finish(&self, state: &'static str, problem: Option<&str>) {
        *lock(&self.phase) = state;
        if state == "done" {
            self.done.store(self.total.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        let detail = self.snapshot(problem).detail;
        if let Some(job) = registry().get(self.id) {
            job.state = state;
            job.detail = detail;
        }
        self.emit(problem);
    }

    /// A progress event, unless one went out less than `gap` ago.
    fn tick(&self) {
        // Another thread is sending one right now: this one is not needed.
        let Ok(mut last) = self.last_event.try_lock() else { return };
        if last.is_some_and(|t| t.elapsed() < self.gap) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        self.events.progress(&self.snapshot(None));
    }

    /// A progress event now.
    fn emit(&self, problem: Option<&str>) {
        *lock(&self.last_event) = Some(Instant::now());
        self.events.progress(&self.snapshot(problem));
    }

    fn snapshot(&self, problem: Option<&str>) -> DupProgress {
        let detail = match (problem, self.summary()) {
            (Some(p), Some(s)) => Some(format!("{p} · {s}")),
            (p, s) => p.map(str::to_string).or(s),
        };
        DupProgress {
            id: self.id,
            phase: lock(&self.phase).to_string(),
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            groups: self.groups.load(Ordering::Relaxed),
            wasted: self.wasted.load(Ordering::Relaxed),
            detail,
        }
    }

    /// The files left out so far, if any.
    fn summary(&self) -> Option<String> {
        let kinds = [
            (Skip::Changed, "스캔한 뒤 바뀌었거나 없어진 파일", ""),
            (Skip::Unreadable, "열 수 없는 파일", ""),
            (Skip::Cloud, "온라인 전용 파일", "(읽으면 내려받게 돼서 읽지 않았어요)"),
        ];
        let parts: Vec<String> = kinds
            .into_iter()
            .filter_map(|(why, what, note)| match self.skipped[why.slot()].load(Ordering::Relaxed) {
                0 => None,
                n => Some(format!("{what} {n}개{note}")),
            })
            .collect();
        (!parts.is_empty()).then(|| format!("건너뛴 파일: {}", parts.join(", ")))
    }
}

/// A file after the quick hash.
struct Probe {
    hash: [u8; 32],
    key: Option<FileKey>,
}

/// Which file a handle is open on: the volume serial and the 128-bit file
/// id (ReFS, Dev Drives: the 64-bit index is not unique there), or the
/// 64-bit file index where that is all there is (marked, so the two kinds
/// never match). As in `win::same_entry`.
pub(crate) type FileKey = (u64, [u8; 16]);

/// A candidate (its index) and which file it names.
type Named = (usize, Option<FileKey>);

/// What an open handle says about its file.
pub(crate) struct Facts {
    pub(crate) attrs: u32,
    pub(crate) size: u64,
    /// FILETIME
    pub(crate) modified: i64,
    /// None when the file system gives no usable id (some network servers
    /// give 0 for every file).
    pub(crate) key: Option<FileKey>,
    /// Names (hard links) the file has.
    pub(crate) links: u32,
}

pub(crate) fn facts(file: &File) -> Option<Facts> {
    let h = file.as_raw_handle() as HANDLE;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
        return None;
    }
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    let time = info.ftLastWriteTime;
    let modified = (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64) as i64;
    let mut ex: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ex_size = std::mem::size_of::<FILE_ID_INFO>() as u32;
    let key = if unsafe { GetFileInformationByHandleEx(h, FileIdInfo, (&raw mut ex).cast(), ex_size) } != 0 {
        (ex.FileId.Identifier != [0; 16]).then_some((ex.VolumeSerialNumber, ex.FileId.Identifier))
    } else {
        let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
        let mut long = [0u8; 16];
        long[..8].copy_from_slice(&index.to_le_bytes());
        (index != 0).then_some((info.dwVolumeSerialNumber as u64 | (1 << 63), long))
    };
    Some(Facts { attrs: info.dwFileAttributes, size, modified, key, links: info.nNumberOfLinks })
}

/// Not downloaded yet from a cloud-sync provider (or moved off to slower
/// storage): reading it fetches it.
pub(crate) fn online_only(attrs: u32) -> bool {
    attrs & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_OFFLINE) != 0
}

/// Whether a file with these attributes, size and time is still the
/// candidate as the index knew it.
fn judge(c: &Candidate, attrs: u32, size: u64, modified: i64) -> Result<(), Skip> {
    if online_only(attrs) {
        return Err(Skip::Cloud);
    }
    if attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 || size != c.size || modified != c.modified {
        return Err(Skip::Changed);
    }
    Ok(())
}

/// Opens a candidate to read, if it is still what the index said, and tells
/// which file it is.
fn open(c: &Candidate, sequential: bool) -> Result<(File, Option<FileKey>), Skip> {
    let path = win::verbatim(Path::new(&c.path));
    let lost = |e: io::Error| if e.kind() == io::ErrorKind::NotFound { Skip::Changed } else { Skip::Unreadable };
    // The attributes first, through a handle that cannot read: an
    // online-only file is never opened for reading.
    let meta = std::fs::symlink_metadata(&path).map_err(lost)?;
    judge(c, meta.file_attributes(), meta.file_size(), meta.last_write_time() as i64)?;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT | if sequential { FILE_FLAG_SEQUENTIAL_SCAN } else { 0 };
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(flags)
        .open(&path)
        .map_err(lost)?;
    // Again from the handle: it may have changed in between.
    let f = facts(&file).ok_or(Skip::Unreadable)?;
    judge(c, f.attrs, f.size, f.modified)?;
    Ok((file, f.key))
}

/// After reading: whether the file stayed as it was meanwhile.
fn unchanged(file: &File, c: &Candidate) -> Result<(), Skip> {
    let f = facts(file).ok_or(Skip::Unreadable)?;
    judge(c, f.attrs, f.size, f.modified)
}

/// Bytes the quick hash reads of a file this size.
fn quick_len(size: u64) -> u64 {
    size.min(WHOLE)
}

/// (offset, length) of what the quick hash reads: all of a small file, else
/// the first, middle and last block.
fn quick_ranges(size: u64) -> Vec<(u64, u64)> {
    if size <= WHOLE {
        vec![(0, size)]
    } else {
        vec![(0, BLOCK), ((size - BLOCK) / 2, BLOCK), (size - BLOCK, BLOCK)]
    }
}

/// blake3 of these (offset, length) parts of a file, read `buf` at a time.
/// `step` hears of every block read and may stop the reading.
fn hash_parts(
    file: &mut File,
    parts: &[(u64, u64)],
    buf: &mut [u8],
    step: &mut dyn FnMut(u64) -> Result<(), Halt>,
) -> Result<blake3::Hash, Halt> {
    let failed = |e: io::Error| {
        // Shorter than it said: it changed while it was read.
        Halt::Skip(if e.kind() == io::ErrorKind::UnexpectedEof { Skip::Changed } else { Skip::Unreadable })
    };
    let mut hasher = blake3::Hasher::new();
    for &(at, len) in parts {
        file.seek(SeekFrom::Start(at)).map_err(failed)?;
        let mut left = len;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..n]).map_err(failed)?;
            hasher.update(&buf[..n]);
            left -= n as u64;
            step(n as u64)?;
        }
    }
    Ok(hasher.finalize())
}

/// One name per file: a later name of a file listed already (a hard link)
/// is dropped. Files without a usable id are all kept.
fn one_name_each(files: Vec<Named>) -> Vec<usize> {
    let mut seen = HashSet::new();
    files.into_iter().filter(|(_, key)| key.is_none_or(|k| seen.insert(k))).map(|(i, _)| i).collect()
}

/// DupFile.category: what a program needs where it is first
/// (`classify::program_category`, which a cache folder of a game does not
/// hide), else `classify::category`.
fn category_of(path: &str, name: &str) -> &'static str {
    crate::classify::program_category(path, name, false).unwrap_or_else(|| crate::classify::category(path, name, false))
}

fn group(cands: &[Candidate], size: u64, hash: [u8; 32], files: Vec<usize>) -> DupGroup {
    let file = |c: &Candidate| {
        let name = c.path.rsplit('\\').next().unwrap_or(&c.path);
        DupFile { path: c.path.clone(), modified: c.modified, category: category_of(&c.path, name) }
    };
    let mut files: Vec<DupFile> = files.into_iter().map(|i| file(&cands[i])).collect();
    files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
    DupGroup { size, hash: blake3::Hash::from(hash).to_hex().to_string(), files }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RawEntry;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::SystemTime;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, SetFileAttributesW};

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let dir = std::env::temp_dir().join(format!("ff-dups-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Temp(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// Writes a file, with its time set `age` seconds back from a fixed point.
        fn file(&self, name: &str, bytes: &[u8], age: u64) -> Candidate {
            let p = self.path(name);
            fs::write(&p, bytes).unwrap();
            let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 - age);
            File::options().write(true).open(&p).unwrap().set_modified(when).unwrap();
            candidate(&p)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(win::verbatim(&self.0));
        }
    }

    /// A candidate as the index would give it for `p` now.
    fn candidate(p: &Path) -> Candidate {
        let m = fs::metadata(p).unwrap();
        Candidate { path: p.to_string_lossy().into_owned(), size: m.len(), modified: m.last_write_time() as i64 }
    }

    /// Bytes that differ from file to file of one `seed`.
    fn content(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3] ^ seed).collect()
    }

    /// A job's events, for a test. Cancels its job when it hears of the
    /// phase `cancel_at`.
    struct Sink {
        tx: mpsc::Sender<DupProgress>,
        cancel_at: Option<&'static str>,
    }

    impl Events for Sink {
        fn progress(&self, p: &DupProgress) {
            if self.cancel_at == Some(p.phase.as_str()) {
                cancel_dup(p.id).unwrap();
            }
            let _ = self.tx.send(p.clone());
        }
    }

    /// Runs a job to its end: its report and all its events.
    fn run(cands: Vec<Candidate>, cancel_at: Option<&'static str>) -> (DupReport, Vec<DupProgress>) {
        let (tx, rx) = mpsc::channel();
        let id = launch(Sink { tx, cancel_at }, cands, Settings { gap: Duration::ZERO, threads: 2 });
        let mut seen = Vec::new();
        loop {
            let p = rx.recv_timeout(Duration::from_secs(60)).expect("the job ends");
            let last = is_finished(&p.phase);
            seen.push(p);
            if last {
                break;
            }
        }
        (report(id).unwrap(), seen)
    }

    fn names(g: &DupGroup) -> Vec<String> {
        g.files.iter().map(|f| Path::new(&f.path).file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    fn hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    fn set_offline(p: &Path) {
        let w: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let attrs = GetFileAttributesW(w.as_ptr());
            assert!(SetFileAttributesW(w.as_ptr(), attrs | FILE_ATTRIBUTE_OFFLINE) != 0);
        }
    }

    #[test]
    fn finds_files_of_equal_content_only() {
        let t = Temp::new("equal");
        let x = content(1000, 1);
        let y = content(5000, 2);
        let mut cands = vec![
            t.file("x1.bin", &x, 30),
            t.file("x2.bin", &x, 10),
            t.file("other.bin", &content(1000, 3), 0),
            t.file("longer.bin", &[&x[..], b"!"].concat(), 0),
            t.file("y-old.bin", &y, 300),
            t.file("y-new.bin", &y, 100),
            t.file("y-mid.bin", &y, 200),
        ];
        // Changed since the scan: another time, gone, now only in the cloud.
        let mut stale = t.file("stale.bin", &x, 0);
        stale.modified += 1;
        cands.push(stale);
        cands.push(Candidate { path: t.path("gone.bin").to_string_lossy().into_owned(), size: 1000, modified: 0 });
        let cloud = t.file("cloud.bin", &y, 0);
        set_offline(&t.path("cloud.bin"));
        cands.push(cloud);
        // The same root picked twice gives each file twice.
        cands.push(candidate(&t.path("x1.bin")));

        let (report, events) = run(cands, None);
        assert_eq!(report.state, "done");
        assert_eq!(report.scanned_files, 10);
        assert_eq!(report.groups.len(), 2);
        let (ys, xs) = (&report.groups[0], &report.groups[1]);
        assert_eq!(names(ys), ["y-new.bin", "y-mid.bin", "y-old.bin"]);
        assert_eq!((ys.size, &ys.hash), (5000, &hex(&y)));
        assert_eq!(names(xs), ["x2.bin", "x1.bin"]);
        assert_eq!((xs.size, &xs.hash), (1000, &hex(&x)));
        assert_eq!(report.wasted, 2 * 5000 + 1000);

        let phases: Vec<&str> = events.iter().map(|p| p.phase.as_str()).collect();
        assert_eq!(phases.first(), Some(&"size"));
        assert!(phases.contains(&"partial") && phases.contains(&"full"));
        let last = events.last().unwrap();
        assert_eq!((last.phase.as_str(), last.groups, last.wasted), ("done", 2, 11000));
        assert_eq!(last.done, last.total);
        let detail = last.detail.as_deref().unwrap();
        assert!(detail.contains("바뀌었거나 없어진 파일 2개"), "{detail}");
        assert!(detail.contains("온라인 전용 파일 1개"), "{detail}");
        assert!(!detail.contains("열 수 없는"), "{detail}");
        // It stays with the results.
        assert_eq!(report.detail.as_deref(), Some(detail));
        let named = |f: &DupFile| category_of(&f.path, f.path.rsplit('\\').next().unwrap());
        assert!(report.groups.iter().flat_map(|g| &g.files).all(|f| f.category == named(f)));
    }

    #[test]
    fn matching_blocks_are_not_enough() {
        let t = Temp::new("partial");
        let a = content(1 << 20, 7);
        let mut b = a.clone();
        // Between the first and the middle block.
        b[300_000] ^= 0xFF;
        let cands = vec![t.file("a.bin", &a, 0), t.file("b.bin", &b, 0), t.file("a-copy.bin", &a, 5)];
        // The quick hashes of a and b do match.
        let quick = |name: &str| {
            let mut f = File::open(t.path(name)).unwrap();
            let mut buf = vec![0; BLOCK as usize];
            match hash_parts(&mut f, &quick_ranges(1 << 20), &mut buf, &mut |_| Ok(())) {
                Ok(h) => h,
                Err(_) => panic!("read {name}"),
            }
        };
        assert_eq!(quick("a.bin"), quick("b.bin"));

        let (report, events) = run(cands, None);
        assert_eq!(report.state, "done");
        assert_eq!(report.groups.len(), 1);
        assert_eq!(names(&report.groups[0]), ["a.bin", "a-copy.bin"]);
        assert_eq!(report.groups[0].hash, hex(&a));
        let full = events.iter().find(|p| p.phase == "full").unwrap();
        assert_eq!(full.total, 3 << 20);
        let last = events.last().unwrap();
        assert_eq!((last.done, last.total, last.detail.as_deref()), (3 << 20, 3 << 20, None));
    }

    #[test]
    fn hard_links_are_one_file() {
        let t = Temp::new("links");
        let small = content(4000, 4);
        let big = content(400_000, 5);
        let mut cands = vec![t.file("s.bin", &small, 0), t.file("s-copy.bin", &small, 9)];
        fs::hard_link(t.path("s.bin"), t.path("s-link.bin")).unwrap();
        // Links alone, small and big, with no real copy.
        t.file("lone.bin", &content(4000, 6), 0);
        fs::hard_link(t.path("lone.bin"), t.path("lone-link.bin")).unwrap();
        t.file("b.bin", &big, 0);
        fs::hard_link(t.path("b.bin"), t.path("b-link.bin")).unwrap();
        for name in ["s-link.bin", "lone.bin", "lone-link.bin", "b.bin", "b-link.bin"] {
            cands.push(candidate(&t.path(name)));
        }

        let (report, _) = run(cands, None);
        assert_eq!(report.state, "done");
        assert_eq!(report.groups.len(), 1, "{:?}", report.groups.iter().map(names).collect::<Vec<_>>());
        let found = names(&report.groups[0]);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&"s-copy.bin".to_string()));
        assert!(found.contains(&"s.bin".to_string()) != found.contains(&"s-link.bin".to_string()));
        assert_eq!(report.wasted, 4000);
    }

    #[test]
    fn cancel_keeps_what_was_found() {
        let t = Temp::new("cancel");
        let small = content(2000, 8);
        let big = content(1 << 20, 9);
        let cands =
            vec![t.file("s1.bin", &small, 0), t.file("s2.bin", &small, 0), t.file("b1.bin", &big, 0), t.file("b2.bin", &big, 0)];
        let (report, events) = run(cands, Some("full"));
        assert_eq!(report.state, "cancelled");
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].size, 2000);
        let last = events.last().unwrap();
        assert_eq!((last.phase.as_str(), last.groups), ("cancelled", 1));
        assert!(last.done < last.total);

        assert_eq!(cancel_dup(u64::MAX), Err(NO_JOB.to_string()));
        assert!(super::report(u64::MAX).is_err());
    }

    #[test]
    fn collect_walks_the_index() {
        let e = |id: u64, parent: u64, name: &str, flags: u32, size: u64| RawEntry {
            id,
            parent,
            name: name.into(),
            flags,
            size,
            alloc: size,
            modified: id as i64,
        };
        let mut raw = vec![
            e(1, 1, ".", FLAG_DIR, 0),
            e(2, 1, "A", FLAG_DIR, 0),
            e(3, 2, "big.bin", 0, 5000),
            e(4, 2, "small.bin", 0, 10),
            e(5, 2, "empty.bin", 0, 0),
            e(6, 2, "extra-link.bin", FLAG_HARDLINK, 5000),
            e(7, 2, "symlink.bin", FLAG_REPARSE, 5000),
            e(8, 1, "junction", FLAG_DIR | FLAG_REPARSE, 0),
            e(9, 8, "behind-link.bin", 0, 5000),
            e(10, 1, "OneDrive", FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK, 0),
            e(11, 10, "synced.bin", 0, 5000),
            e(12, 2, "removed.bin", 0, 5000),
        ];
        // Deep folders, to show there is no recursion to run out of stack.
        let mut parent = 2;
        for id in 100..600 {
            raw.push(e(id, parent, "d", FLAG_DIR, 0));
            parent = id;
        }
        raw.push(e(1000, parent, "deep.bin", 0, 5000));
        let mut ix = Index::build("C:", "test", 0, raw, 1);
        let removed = ix.find(r"C:\A\removed.bin").unwrap();
        ix.remove(removed);

        let mut got: Vec<(String, u64, i64)> =
            collect(&ix, crate::index::ROOT, 0).into_iter().map(|c| (c.path, c.size, c.modified)).collect();
        got.sort();
        let deep = format!(r"C:\A\{}deep.bin", "d\\".repeat(500));
        let mut want = vec![
            (r"C:\A\big.bin".to_string(), 5000, 3),
            (r"C:\A\small.bin".to_string(), 10, 4),
            (deep, 5000, 1000),
            (r"C:\OneDrive\synced.bin".to_string(), 5000, 11),
        ];
        want.sort();
        assert_eq!(got, want);

        let a = ix.find(r"C:\A").unwrap();
        let big: Vec<String> = collect(&ix, a, 100).into_iter().map(|c| c.path).collect();
        assert_eq!(big.len(), 2);
        assert!(big.contains(&r"C:\A\big.bin".to_string()));
        // A file as the root is itself the one candidate.
        let file = ix.find(r"C:\A\big.bin").unwrap();
        assert_eq!(collect(&ix, file, 1).len(), 1);
        assert!(collect(&ix, file, 6000).is_empty());
        assert!(collect(&ix, removed, 1).is_empty());
    }

    #[test]
    fn quick_ranges_cover_small_files_whole() {
        assert_eq!(quick_ranges(100), [(0, 100)]);
        assert_eq!(quick_ranges(WHOLE), [(0, WHOLE)]);
        assert_eq!(quick_len(WHOLE + 1), WHOLE);
        let size = 1 << 20;
        let r = quick_ranges(size);
        assert_eq!(r, [(0, BLOCK), ((size - BLOCK) / 2, BLOCK), (size - BLOCK, BLOCK)]);
        assert!(r[0].0 + r[0].1 <= r[1].0 && r[1].0 + r[1].1 <= r[2].0);
    }

    #[test]
    fn prune_forgets_old_finished_jobs_only() {
        let job = |id: u64, state: &'static str| Job {
            id,
            cancel: Arc::new(AtomicBool::new(false)),
            state,
            groups: Vec::new(),
            wasted: 0,
            scanned_files: 0,
            detail: None,
        };
        let states = ["full", "done", "cancelled", "partial", "failed", "done", "size", "done"];
        let mut reg = Registry { last_id: 8, jobs: states.iter().zip(1..).map(|(&s, id)| job(id, s)).collect() };
        reg.prune();
        let ids: Vec<u64> = reg.jobs.iter().map(|j| j.id).collect();
        assert_eq!(ids, [1, 4, 6, 7, 8]);
        // Only running jobs: none can go.
        let mut busy = Registry { last_id: 7, jobs: (1..=7).map(|id| job(id, "full")).collect() };
        busy.prune();
        assert_eq!(busy.jobs.len(), 7);
    }

    #[test]
    fn hard_link_names_collapse_to_the_first() {
        let k = Some((1, [2; 16]));
        assert_eq!(one_name_each(vec![(0, k), (1, None), (2, k), (3, None), (4, Some((1, [3; 16])))]), [0, 1, 3, 4]);
    }
}
