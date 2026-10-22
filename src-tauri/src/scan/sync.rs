//! Quick sync: brings a drive's index up to date with normal user rights, so
//! no UAC prompt. A folder walk (walk.rs) finds what is on the drive now, and
//! the drive's index until now (`base`) fills in what a walk cannot see:
//!
//! - every folder the walk could not open (access denied, ...) keeps what
//!   `base` has below it: an MFT scan saw into those folders, a walk does not;
//! - a file with the size and time it had in `base` keeps the allocated size
//!   and the hard-link marks `base` found for it. A walk can tell neither: it
//!   counts allocated = size, and every name of a hard-linked file in full.
//!   Other files get their size rounded up to whole clusters, the way live
//!   changes are counted on an MFT index;
//! - the NTFS metafiles at the root (`$MFT`, `$LogFile`, `$Extend` and what
//!   is in it, ...) stay as `base` has them: an MFT scan lists them, a walk
//!   never sees them.
//!
//! A sync of an index that saw into protected folders (an MFT scan, or a sync
//! of one) is made with method "sync"; a sync of a walk is a walk again.
//!
//! Also here: when syncs run by themselves (`AutoSync`), and which scans and
//! syncs of each drive are running (`Jobs`).

use super::walk::{self, Control, Walked};
use crate::index::{Index, RawEntry, FLAG_DELETED, FLAG_DIR, FLAG_HARDLINK, FLAG_LINKED, FLAG_NOT_LINK, FLAG_REPARSE, ROOT};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

/// The method of a sync that kept what an MFT scan saw in protected folders.
pub const SYNC: &str = "sync";
pub const CANCELLED: &str = "스캔이 시작돼서 동기화를 멈췄어요";

/// Whether an index made with `method` saw into folders the user cannot open.
pub fn sees_protected(method: &str) -> bool {
    method == "mft" || method == SYNC
}

/// The new index of a sync, and how it came about.
pub struct Synced {
    pub index: Index,
    /// Folders the walk could not open...
    pub unopened: usize,
    /// ...and how many of them kept what the index before found below them
    /// (`Merged::kept`).
    pub kept: usize,
}

/// What the walk of a sync found (`walk_drive`), for `finish`.
pub struct Found {
    walked: Walked,
    started: Instant,
}

/// The first half of a sync: walks `drive`. Fails when the drive's root
/// cannot be opened, as `scan_walk` does, and when `ctl` was cancelled. The
/// half that takes long; it can run at low priority.
pub fn walk_drive(drive: &str, ctl: &Control) -> io::Result<Found> {
    let root = format!("{drive}\\");
    std::fs::read_dir(&root).map_err(|e| io::Error::new(e.kind(), format!("드라이브를 열 수 없어요: {e}")))?;
    let started = Instant::now();
    let mut walked = walk::walk(&root, ctl);
    if ctl.cancelled() {
        return Err(cancelled());
    }
    // Sorted before `finish` takes the index's lock; `merge` finds them in order then.
    by_parent(&mut walked.entries);
    Ok(Found { walked, started })
}

/// The second half: merges `base`, the drive's index until now, into what
/// the walk found (see the module doc) and makes the new index. `base` is
/// read-locked for the merge, which is short but keeps the drive's watcher
/// waiting meanwhile (and, behind it, whatever else would read the drive):
/// best not run at low priority. `cluster` is the drive's cluster size (0
/// when unknown).
pub fn finish(drive: &str, found: Found, base: Option<&RwLock<Index>>, cluster: u64, ctl: &Control) -> io::Result<Synced> {
    let Found { walked, started } = found;
    let (merged, method) = match base {
        Some(base) => {
            let ix = base.read().unwrap_or_else(PoisonError::into_inner);
            let method = if sees_protected(&ix.method) { SYNC } else { "walk" };
            (merge(walked, Some(&ix), cluster), method)
        }
        None => (merge(walked, None, cluster), "walk"),
    };
    if ctl.cancelled() {
        return Err(cancelled());
    }
    let ms = started.elapsed().as_millis() as u64;
    let index = Index::build(drive, method, ms, merged.entries, walk::ROOT_ID);
    Ok(Synced { index, unopened: merged.unopened, kept: merged.kept })
}

/// Both halves of a sync, one after the other.
#[cfg(test)]
fn sync(drive: &str, base: Option<&RwLock<Index>>, cluster: u64, ctl: &Control) -> io::Result<Synced> {
    finish(drive, walk_drive(drive, ctl)?, base, cluster, ctl)
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, CANCELLED)
}

/// What `merge` makes: entries for `Index::build` (root id `walk::ROOT_ID`).
pub struct Merged {
    pub entries: Vec<RawEntry>,
    pub unopened: usize,
    /// Unopened folders that took what the base found below them: every one
    /// the base has when it saw into protected folders, else (a walk's base,
    /// which could not open them either) only those it had anything below.
    pub kept: usize,
}

const NONE: u32 = u32::MAX;

/// The NTFS metafiles at a volume's root. An MFT scan lists them, and they
/// take room (`$MFT` alone is gigabytes on a disk with millions of files);
/// FindFirstFile never does, so a walk cannot tell them gone. NTFS reserves
/// these names at the root: nothing else is ever called so there.
const METAFILES: &[&str] =
    &["$MFT", "$MFTMirr", "$LogFile", "$Volume", "$AttrDef", "$Bitmap", "$Boot", "$BadClus", "$Secure", "$UpCase", "$Quota", "$Extend"];

fn is_metafile(name: &str) -> bool {
    METAFILES.iter().any(|m| m.eq_ignore_ascii_case(name))
}

/// Each folder's children next to each other, the folders in the order the
/// walk found them: a folder's id is lower than its children's, so every
/// folder's group comes after its parent's.
fn by_parent(entries: &mut [RawEntry]) {
    entries.par_sort_unstable_by_key(|e| (e.parent, e.id));
}

/// A live folder of `ix` whose contents are in `ix`: not a junction or
/// other link (a cloud placeholder holds its own).
fn holds_contents(ix: &Index, n: u32) -> bool {
    let f = ix.nodes[n as usize].flags;
    f & (FLAG_DIR | FLAG_DELETED) == FLAG_DIR && (f & FLAG_REPARSE == 0 || f & FLAG_NOT_LINK != 0)
}

fn is_live(ix: &Index, n: u32) -> bool {
    ix.nodes[n as usize].flags & FLAG_DELETED == 0
}

/// `size` in whole clusters of `unit` bytes (0: as it is).
fn rounded(size: u64, unit: u64) -> u64 {
    if unit == 0 { size } else { size.div_ceil(unit).saturating_mul(unit) }
}

/// The walk's entries with what `base` adds (see the module doc). Without a
/// base they are the walk's as they are. `cluster` counts only for a base
/// that saw into protected folders (an MFT-like index); a walk's sizes stay
/// a walk's.
pub fn merge(walked: Walked, base: Option<&Index>, cluster: u64) -> Merged {
    let Walked { mut entries, unopened } = walked;
    let Some(base) = base else {
        return Merged { entries, unopened: unopened.len(), kept: 0 };
    };
    let unit = if sees_protected(&base.method) { cluster } else { 0 };
    by_parent(&mut entries);
    // Walk ids run from 0 without gaps; grafted entries get ids after them.
    let first_free = entries.iter().map(|e| e.id).max().map_or(0, |m| m + 1);
    // The node in `base` of each walked folder, by walk id.
    let mut base_of = vec![NONE; first_free as usize];
    if let Some(root) = base_of.get_mut(walk::ROOT_ID as usize) {
        *root = ROOT;
    }
    let mut exact: HashMap<&str, u32> = HashMap::new();
    let mut folded: HashMap<String, u32> = HashMap::new();
    let mut start = 0;
    while start < entries.len() {
        let parent = entries[start].parent;
        let len = entries[start..].iter().position(|e| e.parent != parent).unwrap_or(entries.len() - start);
        let group = &mut entries[start..start + len];
        start += len;
        let b = base_of.get(parent as usize).copied().unwrap_or(NONE);
        let mapped = b != NONE && holds_contents(base, b);
        if mapped {
            // Names are mostly what they were; the case-blind map is made
            // only for a folder where one is not found as it is.
            exact.clear();
            folded.clear();
            for c in base.children_of(b).filter(|&c| is_live(base, c)) {
                exact.entry(base.name(c)).or_insert(c);
            }
        }
        for e in group.iter_mut() {
            if e.id == e.parent {
                continue; // the root
            }
            let dir = e.flags & FLAG_DIR != 0;
            let found = if !mapped {
                None
            } else if let Some(&c) = exact.get(e.name.as_str()) {
                Some(c)
            } else {
                if folded.is_empty() {
                    for c in base.children_of(b).filter(|&c| is_live(base, c)) {
                        folded.entry(base.name(c).to_lowercase()).or_insert(c);
                    }
                }
                folded.get(&e.name.to_lowercase()).copied()
            };
            let node = found.map(|c| (c, base.nodes[c as usize]));
            if dir {
                if let Some((c, n)) = node
                    && n.flags & FLAG_DIR != 0
                {
                    base_of[e.id as usize] = c;
                }
                continue;
            }
            match node {
                Some((_, n)) if n.flags & FLAG_DIR == 0 && n.size == e.size && n.modified == e.modified => {
                    e.alloc = n.alloc;
                    e.flags |= n.flags & (FLAG_HARDLINK | FLAG_LINKED);
                }
                _ => e.alloc = rounded(e.size, unit),
            }
        }
    }
    let protected = sees_protected(&base.method);
    let mut kept = 0;
    for (id, path) in &unopened {
        let b = match base_of.get(*id as usize).copied() {
            Some(b) if b != NONE => Some(b),
            // A name the case-blind map could not match on the way there.
            _ => base.find(&path.to_string_lossy()),
        };
        let Some(b) = b.filter(|&b| holds_contents(base, b)) else { continue };
        // A walk's base could not open the folder either: what it has below
        // it is nothing, unless the folder opened back then.
        if graft(base, b, *id, first_free, &mut entries) > 0 || protected {
            kept += 1;
        }
    }
    if protected {
        keep_metafiles(base, first_free, &mut entries);
    }
    Merged { entries, unopened: unopened.len(), kept }
}

/// Appends the base's NTFS metafiles (`METAFILES`) that the walk did not
/// list to the walk's root, each with what is below it (`$Extend`).
fn keep_metafiles(base: &Index, first_free: u64, out: &mut Vec<RawEntry>) {
    // The walk's entries are in `by_parent` order, the root's group first
    // (the root is its own parent); what was grafted comes after them.
    let walked: Vec<String> = out
        .iter()
        .take_while(|e| e.parent == walk::ROOT_ID)
        .filter(|e| e.id != walk::ROOT_ID && e.name.starts_with('$'))
        .map(|e| e.name.to_lowercase())
        .collect();
    let metafiles: Vec<u32> = base
        .children_of(ROOT)
        .filter(|&c| is_live(base, c) && is_metafile(base.name(c)))
        .filter(|&c| !walked.contains(&base.name(c).to_lowercase()))
        .collect();
    for c in metafiles {
        let n = base.nodes[c as usize];
        let id = first_free + c as u64;
        out.push(RawEntry {
            id,
            parent: walk::ROOT_ID,
            name: base.name(c).to_string(),
            flags: n.flags,
            size: n.size,
            alloc: n.alloc,
            modified: n.modified,
        });
        if holds_contents(base, c) {
            graft(base, c, id, first_free, out);
        }
    }
}

/// Appends everything below `base` node `from` as entries below walk folder
/// `to`, each with id `first_free` + its node number. Returns how many.
fn graft(base: &Index, from: u32, to: u64, first_free: u64, out: &mut Vec<RawEntry>) -> usize {
    let before = out.len();
    let mut stack: Vec<u32> = base.children_of(from).filter(|&c| is_live(base, c)).collect();
    while let Some(c) = stack.pop() {
        let n = base.nodes[c as usize];
        let parent = if n.parent == from { to } else { first_free + n.parent as u64 };
        out.push(RawEntry {
            id: first_free + c as u64,
            parent,
            name: base.name(c).to_string(),
            flags: n.flags,
            size: n.size,
            alloc: n.alloc,
            modified: n.modified,
        });
        if n.flags & FLAG_DIR != 0 {
            stack.extend(base.children_of(c).filter(|&k| is_live(base, k)));
        }
    }
    out.len() - before
}

// ---- when syncs run by themselves -----------------------------------------------

/// After startup, a drive scanned (or synced) less than this long ago, or
/// whose watcher was keeping up when the app was closed less than this long
/// ago, is not synced right away but this long later (`AutoSync::defer`):
/// a quick restart does not walk the drive at once, and what changed in
/// those few minutes the app was closed is still taken in.
pub const FRESH: Duration = Duration::from_secs(600);
/// Startup syncs begin this long after the cached indexes were loaded, so
/// the window shows and the watchers start first.
pub const STARTUP_DELAY: Duration = Duration::from_secs(5);
/// A drive whose watcher went stale is synced this long later (a burst of
/// changes is usually over by then)...
pub const STALE_DEBOUNCE: Duration = Duration::from_secs(30);
/// ...but not sooner than this after the last sync for that reason, twice
/// that after a second one in a row, and so on up to 16 times: a drive whose
/// changes keep overflowing is not walked again and again.
pub const STALE_GAP: Duration = Duration::from_secs(600);
/// A drive that goes stale this long or longer after its last stale sync
/// started (it kept up all that while) starts from the first gap again. Told
/// by when it went stale, not by when its sync is due: the gaps grow past it.
const STALE_RESET: Duration = Duration::from_secs(2 * 3600);
const FILETIME_SEC: i64 = 10_000_000;

/// Why a sync ran (the "reason" of "sync-progress").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Once per drive after the cached indexes were loaded: what changed while the app was closed.
    Startup,
    /// The drive's watcher missed changes.
    Stale,
    /// Asked for.
    User,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Startup => "startup",
            Reason::Stale => "stale",
            Reason::User => "user",
        }
    }
}

/// Whether a drive of `kind` (`win::drive_kind`) whose index was last known
/// to be what is on the disk at `known_at` (when it was scanned or synced,
/// or later when the app was closed with its watcher keeping up: lib.rs
/// `save_on_exit`) is synced after startup at `now` (both FILETIME) right
/// away; a local disk that is not is synced FRESH later instead. Only local
/// disks: a network drive is synced when asked to. With the clock set back
/// (`known_at` "later" than now) its age cannot be told; a sync is cheap.
pub fn startup_due(kind: &str, known_at: i64, now: i64) -> bool {
    let age = now.saturating_sub(known_at);
    let fresh = FRESH.as_secs() as i64 * FILETIME_SEC;
    matches!(kind, "fixed" | "removable") && (age >= fresh || age < -60 * FILETIME_SEC)
}

/// Syncs waiting to run by themselves. One runs at a time (the caller's one
/// thread takes them from `next`); whether one still makes sense when its
/// turn comes (the drive is not being scanned, its watcher is still stale)
/// is for the caller to check then. Also which drives took in, since the
/// app started, what changed while it was closed (`caught_up`).
#[derive(Default)]
pub struct AutoSync {
    startup: VecDeque<String>,
    startup_at: Option<Instant>,
    /// Startup syncs put off (`defer`), and when each is due.
    later: HashMap<String, Instant>,
    /// Drives whose watcher went stale, and when.
    stale: HashMap<String, Instant>,
    /// Stale syncs `next` handed out, and when their drive went stale, until
    /// `ran` says the sync started.
    taken: HashMap<String, Instant>,
    /// When each drive's last stale sync started, and how many in a row.
    last: HashMap<String, (Instant, u32)>,
    /// Drives a scan or sync put a new index in place for since the app
    /// started (`installed`).
    caught_up: HashSet<String>,
}

impl AutoSync {
    /// Indexes of `drives` were loaded from their cache files at `now`.
    pub fn after_startup(&mut self, drives: impl IntoIterator<Item = String>, now: Instant) {
        self.startup.extend(drives);
        self.startup_at.get_or_insert(now + STARTUP_DELAY);
    }

    /// The startup sync of `drive` runs at `at` instead of now (FRESH).
    pub fn defer(&mut self, drive: &str, at: Instant) {
        self.later.insert(drive.to_string(), at);
    }

    pub fn went_stale(&mut self, drive: &str, now: Instant) {
        self.stale.insert(drive.to_string(), now);
    }

    /// A scan or sync put a new index of `drive` in place, followed by a
    /// new watcher that started before it read the drive. What changed while
    /// the app was closed is in it, so the drive's startup sync is no longer
    /// waited for, and what the old watcher missed is in it too (the new one
    /// tells its own trouble when it is attached, after this). The back-off
    /// of stale syncs stays.
    pub fn installed(&mut self, drive: &str) {
        self.startup.retain(|d| d != drive);
        self.later.remove(drive);
        self.stale.remove(drive);
        self.taken.remove(drive);
        self.caught_up.insert(drive.to_string());
    }

    /// Whether a scan or sync since the app started put `drive`'s index in
    /// place (`installed`). Until then an index loaded from its cache file
    /// misses what changed while the app was closed, however well its
    /// watcher keeps up: its startup sync is still to come.
    pub fn caught_up(&self, drive: &str) -> bool {
        self.caught_up.contains(drive)
    }

    /// When the stale sync of `drive` is due, if one is waiting.
    pub fn stale_due(&self, drive: &str) -> Option<Instant> {
        let at = *self.stale.get(drive)?;
        let soonest = at + STALE_DEBOUNCE;
        Some(match self.in_a_row(drive, at) {
            Some((t, n)) => soonest.max(t + STALE_GAP.saturating_mul(1 << n.saturating_sub(1).min(4))),
            None => soonest,
        })
    }

    /// The last stale sync of `drive` and how many in a row it was, when the
    /// drive going stale at `at` follows on from it (STALE_RESET).
    fn in_a_row(&self, drive: &str, at: Instant) -> Option<(Instant, u32)> {
        self.last.get(drive).copied().filter(|&(t, _)| at.saturating_duration_since(t) < STALE_RESET)
    }

    /// The sync to run at `now`, taken off the list: startup syncs first, in
    /// the order given, then those put off whose time has come, then stale
    /// drives whose time has come, earliest first. A stale sync counts for
    /// the back-off only once `ran` says it started: one the caller finds
    /// not needed any more (the drive was synced by hand meanwhile, its
    /// watcher lost a hard link) does not make the next one wait longer.
    pub fn next(&mut self, now: Instant) -> Option<(String, Reason)> {
        if self.startup_at.is_some_and(|t| now >= t)
            && let Some(d) = self.startup.pop_front()
        {
            return Some((d, Reason::Startup));
        }
        if let Some((drive, _)) =
            self.later.iter().filter(|&(_, &due)| due <= now).map(|(d, &due)| (d.clone(), due)).min_by_key(|&(_, due)| due)
        {
            self.later.remove(&drive);
            return Some((drive, Reason::Startup));
        }
        let (drive, _) = self
            .stale
            .keys()
            .filter_map(|d| self.stale_due(d).filter(|&due| due <= now).map(|due| (d.clone(), due)))
            .min_by_key(|&(_, due)| due)?;
        let at = self.stale.remove(&drive).unwrap_or(now);
        self.taken.insert(drive.clone(), at);
        Some((drive, Reason::Stale))
    }

    /// The stale sync of `drive` that `next` handed out started at `now`:
    /// the next one waits a gap longer (STALE_GAP). Nothing for a drive
    /// `next` did not hand one out for.
    pub fn ran(&mut self, drive: &str, now: Instant) {
        let Some(at) = self.taken.remove(drive) else { return };
        let n = self.in_a_row(drive, at).map_or(1, |(_, n)| n + 1);
        self.last.insert(drive.to_string(), (now, n));
    }
}

/// What keeps a sync from starting.
#[derive(Debug, PartialEq, Eq)]
pub enum Busy {
    Scanning,
    Syncing,
}

/// The scans and syncs running, by drive.
#[derive(Default)]
pub struct Jobs {
    scans: HashMap<String, u32>,
    syncs: HashMap<String, Arc<Control>>,
}

impl Jobs {
    /// A scan of `drive` starts. A sync of it under way is told to stop: the
    /// scan's index will replace whatever it brings.
    pub fn scan_started(&mut self, drive: &str) {
        *self.scans.entry(drive.to_string()).or_default() += 1;
        if let Some(ctl) = self.syncs.get(drive) {
            ctl.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn scan_ended(&mut self, drive: &str) {
        if let Some(n) = self.scans.get_mut(drive) {
            *n -= 1;
            if *n == 0 {
                self.scans.remove(drive);
            }
        }
    }

    /// A sync of `drive` starts, unless a scan or another sync of it runs.
    pub fn sync_started(&mut self, drive: &str) -> Result<Arc<Control>, Busy> {
        if self.scans.contains_key(drive) {
            return Err(Busy::Scanning);
        }
        if self.syncs.contains_key(drive) {
            return Err(Busy::Syncing);
        }
        let ctl = Arc::new(Control::default());
        self.syncs.insert(drive.to_string(), ctl.clone());
        Ok(ctl)
    }

    pub fn sync_ended(&mut self, drive: &str, ctl: &Arc<Control>) {
        if self.syncs.get(drive).is_some_and(|c| Arc::ptr_eq(c, ctl)) {
            self.syncs.remove(drive);
        }
    }

    pub fn syncing(&self, drive: &str) -> bool {
        self.syncs.contains_key(drive)
    }

    pub fn busy(&self, drive: &str) -> bool {
        self.scans.contains_key(drive) || self.syncs.contains_key(drive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{FLAG_HIDDEN, FLAG_SYSTEM};
    use std::path::PathBuf;

    fn raw(id: u64, parent: u64, name: &str, flags: u32, size: u64, alloc: u64, modified: i64) -> RawEntry {
        RawEntry { id, parent, name: name.into(), flags, size, alloc, modified }
    }

    /// An MFT scan of a drive: alloc in 4 KiB clusters, a protected folder
    /// with contents, a file with two names (WinSxS-style: the second name
    /// does not count again), and NTFS metafiles at the root.
    fn mft_base() -> Index {
        let d = FLAG_DIR;
        let meta = FLAG_HIDDEN | FLAG_SYSTEM;
        let entries = vec![
            raw(0, 5, "$MFT", meta, 262_144, 262_144, 1),
            raw(2, 5, "$LogFile", meta, 65_536, 65_536, 1),
            raw(50, 5, "$Extend", d | meta, 0, 0, 1),
            raw(51, 50, "$UsnJrnl", meta, 32_768, 32_768, 2),
            raw(52, 50, "$RmMetadata", d | meta, 0, 0, 1),
            raw(53, 52, "$Repair", meta, 4096, 4096, 1),
            raw(5, 5, ".", d, 0, 0, 0),
            raw(10, 5, "Users", d, 0, 0, 1),
            raw(11, 10, "me", d, 0, 0, 1),
            raw(12, 11, "keep.txt", 0, 100, 4096, 7),
            raw(13, 11, "changed.txt", 0, 100, 4096, 7),
            raw(14, 11, "deleted.txt", 0, 5000, 8192, 7),
            raw(20, 5, "System Volume Information", d | FLAG_HIDDEN, 0, 0, 1),
            raw(21, 20, "tracking.log", 0, 20_000, 20_480, 3),
            raw(22, 20, "deep", d, 0, 0, 3),
            raw(23, 22, "store.bin", 0, 1_000_000, 1_003_520, 3),
            raw(30, 5, "Windows", d, 0, 0, 1),
            raw(31, 30, "System32", d, 0, 0, 1),
            raw(32, 31, "kernel.dll", FLAG_LINKED, 9000, 12_288, 4),
            raw(33, 30, "WinSxS", d, 0, 0, 1),
            raw(34, 33, "kernel.dll", FLAG_HARDLINK, 9000, 12_288, 4),
        ];
        Index::build("C:", "mft", 0, entries, 5)
    }

    /// What a walk of that drive finds now: the protected folder cannot be
    /// opened, `deleted.txt` is gone, `changed.txt` grew, `new.txt` is new.
    fn walk_now() -> Walked {
        let d = FLAG_DIR;
        let entries = vec![
            raw(0, 0, "", d, 0, 0, 0),
            raw(1, 0, "Users", d, 0, 0, 1),
            raw(2, 0, "System Volume Information", d | FLAG_HIDDEN, 0, 0, 1),
            raw(3, 0, "Windows", d, 0, 0, 1),
            raw(4, 1, "me", d, 0, 0, 1),
            raw(5, 3, "System32", d, 0, 0, 1),
            raw(6, 3, "WinSxS", d, 0, 0, 1),
            raw(7, 4, "keep.txt", 0, 100, 100, 7),
            // The case of a name changed: still the same file.
            raw(8, 4, "CHANGED.txt", 0, 300, 300, 9),
            raw(9, 4, "new.txt", 0, 10, 10, 9),
            raw(10, 5, "kernel.dll", 0, 9000, 9000, 4),
            raw(11, 6, "kernel.dll", 0, 9000, 9000, 4),
        ];
        Walked { entries, unopened: vec![(2, PathBuf::from(r"C:\System Volume Information"))] }
    }

    fn totals(ix: &Index, path: &str) -> (u64, u64, u32) {
        let n = ix.nodes[ix.find(path).unwrap() as usize];
        (n.size, n.alloc, n.files)
    }

    #[test]
    fn a_sync_keeps_protected_folders_and_follows_the_rest() {
        let base = mft_base();
        let m = merge(walk_now(), Some(&base), 4096);
        assert_eq!((m.unopened, m.kept), (1, 1));
        let ix = Index::build("C:", SYNC, 0, m.entries, walk::ROOT_ID);
        // The folder the walk could not open keeps what the MFT scan found in it.
        assert_eq!(totals(&ix, r"C:\System Volume Information"), (1_020_000, 1_024_000, 2));
        assert_eq!(totals(&ix, r"C:\System Volume Information\deep\store.bin"), (1_000_000, 1_003_520, 0));
        assert!(ix.info(ix.find(r"C:\System Volume Information").unwrap()).hidden);
        // Gone is gone, new is new, a changed file is as it is now (in whole clusters).
        assert!(ix.find(r"C:\Users\me\deleted.txt").is_none());
        assert_eq!(totals(&ix, r"C:\Users\me\new.txt"), (10, 4096, 0));
        assert_eq!(totals(&ix, r"C:\Users\me\CHANGED.txt"), (300, 4096, 0));
        // An unchanged file keeps what the MFT said it takes on disk.
        assert_eq!(totals(&ix, r"C:\Users\me\keep.txt"), (100, 4096, 0));
        assert_eq!(totals(&ix, r"C:\Users\me"), (410, 3 * 4096, 3));
        // The second name of a hard-linked file still does not count twice.
        assert_eq!(totals(&ix, r"C:\Windows"), (9000, 12_288, 1));
        let sxs = ix.find(r"C:\Windows\WinSxS\kernel.dll").unwrap();
        assert!(ix.nodes[sxs as usize].flags & FLAG_HARDLINK != 0);
        // The metafiles a walk never lists are still there, `$Extend` with all in it.
        let meta = 262_144 + 65_536 + 32_768 + 4096;
        assert_eq!(totals(&ix, r"C:\$MFT"), (262_144, 262_144, 0));
        assert_eq!(totals(&ix, r"C:\$Extend"), (36_864, 36_864, 2));
        assert_eq!(totals(&ix, r"C:\$Extend\$RmMetadata\$Repair"), (4096, 4096, 0));
        assert!(ix.info(ix.find(r"C:\$LogFile").unwrap()).system);
        assert_eq!(ix.nodes[ROOT as usize].size, 1_020_000 + 410 + 9000 + meta);
        // The root total moved only by what changed on disk (+200 +10 −5000, in clusters).
        let before = base.nodes[ROOT as usize];
        assert_eq!(before.size, 1_020_000 + 5200 + 9000 + meta);
        assert_eq!(ix.nodes[ROOT as usize].alloc, before.alloc - 8192 + 4096);
        // A sync of the sync keeps them too.
        let again = merge(walk_now(), Some(&ix), 4096);
        let ix2 = Index::build("C:", SYNC, 0, again.entries, walk::ROOT_ID);
        assert_eq!(ix2.nodes[ROOT as usize].size, ix.nodes[ROOT as usize].size);
        assert!(ix2.find(r"C:\$Extend\$UsnJrnl").is_some());
    }

    #[test]
    fn metafiles_are_kept_only_from_an_mft_base_and_only_once() {
        // A walk that does list one (it is not one of the walked names twice).
        let mut walked = walk_now();
        walked.entries.push(raw(12, 0, "$MFT", FLAG_HIDDEN | FLAG_SYSTEM, 262_144, 262_144, 1));
        let m = merge(walked, Some(&mft_base()), 4096);
        assert_eq!(m.entries.iter().filter(|e| e.name.eq_ignore_ascii_case("$mft")).count(), 1);
        assert_eq!(m.entries.iter().filter(|e| e.name == "$LogFile").count(), 1);
        // On a drive a walk indexed (FAT32, exFAT: names anyone can use), a `$Boot` gone is gone.
        let mut walk_base = walk_now();
        walk_base.entries.push(raw(12, 0, "$Boot", 0, 10, 10, 1));
        let walk_base = Index::build("E:", "walk", 0, walk_base.entries, walk::ROOT_ID);
        let m = merge(walk_now(), Some(&walk_base), 4096);
        assert!(m.entries.iter().all(|e| e.name != "$Boot"));
    }

    #[test]
    fn without_a_base_a_sync_is_a_walk() {
        let m = merge(walk_now(), None, 4096);
        assert_eq!((m.unopened, m.kept), (1, 0));
        let ix = Index::build("C:", "walk", 0, m.entries, walk::ROOT_ID);
        assert_eq!(totals(&ix, r"C:\System Volume Information"), (0, 0, 0));
        // Every name counts, sizes as they are.
        assert_eq!(totals(&ix, r"C:\Windows"), (18_000, 18_000, 2));
        assert_eq!(totals(&ix, r"C:\Users\me\keep.txt"), (100, 100, 0));
    }

    #[test]
    fn a_walk_base_gives_nothing_it_did_not_have() {
        // A walk could not open the protected folder either: it stays empty,
        // and sizes are not rounded to clusters.
        let walk_base = Index::build("C:", "walk", 0, walk_now().entries, walk::ROOT_ID);
        let m = merge(walk_now(), Some(&walk_base), 4096);
        // Nothing was kept, so the sync does not say it was.
        assert_eq!((m.unopened, m.kept), (1, 0));
        let ix = Index::build("C:", "walk", 0, m.entries, walk::ROOT_ID);
        assert_eq!(totals(&ix, r"C:\System Volume Information"), (0, 0, 0));
        assert_eq!(totals(&ix, r"C:\Users\me\new.txt"), (10, 10, 0));
        assert_eq!(totals(&ix, r"C:\Users\me"), (410, 410, 3));
        // What a walk before found in a folder it could open then is kept, and said so.
        let mut opened = walk_now();
        opened.entries.push(raw(12, 2, "log.txt", 0, 7, 7, 1));
        let walk_base = Index::build("C:", "walk", 0, opened.entries, walk::ROOT_ID);
        let m = merge(walk_now(), Some(&walk_base), 4096);
        assert_eq!((m.unopened, m.kept), (1, 1));
        let ix = Index::build("C:", "walk", 0, m.entries, walk::ROOT_ID);
        assert_eq!(totals(&ix, r"C:\System Volume Information"), (7, 7, 1));
    }

    #[test]
    fn a_link_in_the_base_is_not_followed() {
        let mut base = mft_base();
        let svi = base.find(r"C:\System Volume Information").unwrap();
        base.nodes[svi as usize].flags |= FLAG_REPARSE;
        let m = merge(walk_now(), Some(&base), 4096);
        assert_eq!((m.unopened, m.kept), (1, 0));
        let ix = Index::build("C:", SYNC, 0, m.entries, walk::ROOT_ID);
        assert_eq!(totals(&ix, r"C:\System Volume Information"), (0, 0, 0));
    }

    #[test]
    fn a_real_walk_and_sync() {
        let root = std::env::temp_dir().join(format!("ff-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a").join("one.bin"), [0u8; 10]).unwrap();
        std::fs::write(root.join("gone.bin"), [0u8; 5]).unwrap();
        let drive = root.to_string_lossy().into_owned();
        let base = Index::build(&drive, "mft", 0, walk::scan(&format!("{drive}\\")), walk::ROOT_ID);
        std::fs::remove_file(root.join("gone.bin")).unwrap();
        std::fs::write(root.join("a").join("two.bin"), [0u8; 3]).unwrap();
        let shared = RwLock::new(base);
        let s = sync(&drive, Some(&shared), 4096, &Control::default()).unwrap();
        assert_eq!(s.index.method, SYNC);
        assert!(s.index.find(&format!(r"{drive}\gone.bin")).is_none());
        let two = s.index.find(&format!(r"{drive}\a\two.bin")).unwrap();
        assert_eq!((s.index.nodes[two as usize].size, s.index.nodes[two as usize].alloc), (3, 4096));
        // Unchanged: the base's allocation (a walk's, here) stays.
        let one = s.index.find(&format!(r"{drive}\a\one.bin")).unwrap();
        assert_eq!(s.index.nodes[one as usize].alloc, 10);
        // Stopped: no index.
        let ctl = Control::default();
        ctl.cancel.store(true, Ordering::Relaxed);
        assert_eq!(sync(&drive, Some(&shared), 4096, &ctl).err().unwrap().kind(), io::ErrorKind::Interrupted);
        std::fs::remove_dir_all(&root).unwrap();
        // A root that does not open is an error, never an empty index.
        assert!(sync(&drive, None, 0, &Control::default()).is_err());
    }

    #[test]
    fn startup_syncs_skip_fresh_and_network_drives() {
        let now = 133_000_000_000_000_000i64;
        let min = 60 * FILETIME_SEC;
        assert!(startup_due("fixed", now - 11 * min, now));
        assert!(startup_due("removable", now - 10 * min, now));
        assert!(!startup_due("fixed", now - 9 * min, now));
        assert!(!startup_due("fixed", now, now));
        // Only when asked: network drives, and anything else that is not a local disk.
        for kind in ["network", "cdrom", "ram", "unknown"] {
            assert!(!startup_due(kind, now - 1000 * min, now), "{kind}");
        }
        // A clock set back.
        assert!(startup_due("fixed", now + 5 * min, now));
        assert!(!startup_due("fixed", now + min / 2, now));
    }

    #[test]
    fn automatic_syncs_wait_their_turn() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut auto = AutoSync::default();
        assert_eq!(auto.next(t0), None);
        auto.after_startup(["C:".to_string(), "D:".to_string()], t0);
        // The window and the watchers go first.
        assert_eq!(auto.next(at(4)), None);
        assert_eq!(auto.next(at(5)), Some(("C:".into(), Reason::Startup)));
        // A drive gone stale meanwhile waits for the startup syncs, and its debounce.
        auto.went_stale("E:", at(5));
        assert_eq!(auto.next(at(6)), Some(("D:".into(), Reason::Startup)));
        assert_eq!(auto.next(at(34)), None);
        assert_eq!(auto.stale_due("E:"), Some(at(35)));
        // Stale again within the debounce: counted from the last time.
        auto.went_stale("E:", at(20));
        assert_eq!(auto.next(at(35)), None);
        assert_eq!(auto.next(at(50)), Some(("E:".into(), Reason::Stale)));
        auto.ran("E:", at(50));
        assert_eq!(auto.next(at(51)), None);
        assert_eq!(auto.stale_due("E:"), None);
        // Overflowing again soon after: 10 minutes after that sync, then 20, then 40.
        auto.went_stale("E:", at(60));
        assert_eq!(auto.stale_due("E:"), Some(at(50 + 600)));
        assert_eq!(auto.next(at(649)), None);
        assert_eq!(auto.next(at(650)), Some(("E:".into(), Reason::Stale)));
        auto.ran("E:", at(650));
        auto.went_stale("E:", at(700));
        assert_eq!(auto.stale_due("E:"), Some(at(650 + 1200)));
        assert_eq!(auto.next(at(1850)), Some(("E:".into(), Reason::Stale)));
        auto.ran("E:", at(1850));
        auto.went_stale("E:", at(1900));
        assert_eq!(auto.stale_due("E:"), Some(at(1850 + 2400)));
        // Quiet for two hours after a sync (whatever the gap): the next one counts as the first again.
        auto.went_stale("E:", at(1850 + 7200));
        assert_eq!(auto.stale_due("E:"), Some(at(1850 + 7200 + 30)));
        assert_eq!(auto.next(at(1850 + 7200 + 30)), Some(("E:".into(), Reason::Stale)));
        auto.ran("E:", at(1850 + 7200 + 30));
        assert_eq!(auto.last["E:"].1, 1);
        // Two drives due: the one due first goes first.
        auto.went_stale("F:", at(20_000));
        auto.went_stale("G:", at(19_990));
        assert_eq!(auto.next(at(20_100)), Some(("G:".into(), Reason::Stale)));
        assert_eq!(auto.next(at(20_100)), Some(("F:".into(), Reason::Stale)));
        assert_eq!(Reason::Startup.as_str(), "startup");
        assert_eq!(Reason::Stale.as_str(), "stale");
        assert_eq!(Reason::User.as_str(), "user");
    }

    #[test]
    fn a_drive_that_keeps_overflowing_stays_at_the_longest_gap() {
        let t0 = Instant::now();
        let mut auto = AutoSync::default();
        auto.went_stale("C:", t0);
        let mut syncs = Vec::new();
        for _ in 0..9 {
            let due = auto.stale_due("C:").unwrap();
            assert_eq!(auto.next(due - Duration::from_secs(1)), None);
            assert_eq!(auto.next(due), Some(("C:".into(), Reason::Stale)));
            auto.ran("C:", due);
            syncs.push(due);
            // Each sync's watcher overflows again five minutes later.
            auto.went_stale("C:", due + Duration::from_secs(300));
        }
        let gaps: Vec<u64> = syncs.windows(2).map(|w| (w[1] - w[0]).as_secs() / 60).collect();
        assert_eq!(gaps, [10, 20, 40, 80, 160, 160, 160, 160]);
        assert_eq!(syncs[0], t0 + STALE_DEBOUNCE);
        // Then it keeps up for two hours: back to the first gap.
        let last = *syncs.last().unwrap();
        auto.stale.clear();
        auto.went_stale("C:", last + STALE_RESET);
        assert_eq!(auto.stale_due("C:"), Some(last + STALE_RESET + STALE_DEBOUNCE));
        auto.next(last + STALE_RESET + STALE_DEBOUNCE).unwrap();
        auto.ran("C:", last + STALE_RESET + STALE_DEBOUNCE);
        auto.went_stale("C:", last + STALE_RESET + Duration::from_secs(60));
        assert_eq!(auto.stale_due("C:"), Some(last + STALE_RESET + STALE_DEBOUNCE + STALE_GAP));
    }

    #[test]
    fn a_stale_sync_that_does_not_run_does_not_count() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut auto = AutoSync::default();
        auto.went_stale("C:", t0);
        // Handed out, but not needed when its turn came (synced by hand, a hard link lost).
        assert_eq!(auto.next(at(30)), Some(("C:".into(), Reason::Stale)));
        auto.went_stale("C:", at(100));
        assert_eq!(auto.stale_due("C:"), Some(at(130)));
        assert_eq!(auto.next(at(130)), Some(("C:".into(), Reason::Stale)));
        auto.ran("C:", at(130));
        // Only the one that ran counts: the next waits the first gap.
        auto.went_stale("C:", at(200));
        assert_eq!(auto.stale_due("C:"), Some(at(130 + 600)));
        assert_eq!(auto.last["C:"].1, 1);
        // `ran` for a drive nothing was handed out for changes nothing.
        auto.ran("D:", at(200));
        auto.ran("C:", at(201));
        assert!(!auto.last.contains_key("D:"));
        assert_eq!(auto.last["C:"], (at(130), 1));
    }

    #[test]
    fn a_scan_or_sync_takes_the_place_of_the_syncs_waiting() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut auto = AutoSync::default();
        auto.after_startup(["C:".to_string(), "D:".to_string(), "E:".to_string()], t0);
        assert!(!auto.caught_up("C:") && !auto.caught_up("D:"));
        // C:'s startup sync runs; D: is synced by hand before its turn.
        assert_eq!(auto.next(at(5)), Some(("C:".into(), Reason::Startup)));
        auto.installed("C:");
        auto.went_stale("D:", at(5));
        auto.installed("D:");
        assert!(auto.caught_up("C:") && auto.caught_up("D:") && !auto.caught_up("E:"));
        // D:'s startup sync and the stale one of its old watcher are not waited for.
        assert_eq!(auto.stale_due("D:"), None);
        assert_eq!(auto.next(at(6)), Some(("E:".into(), Reason::Startup)));
        // E: was in step a few minutes before startup: synced FRESH later.
        auto.defer("E:", at(6) + FRESH);
        assert_eq!(auto.next(at(7)), None);
        assert!(!auto.caught_up("E:"));
        assert_eq!(auto.next(at(6) + FRESH), Some(("E:".into(), Reason::Startup)));
        assert_eq!(auto.next(at(6) + FRESH), None);
        // A put-off sync goes before a stale one due as long.
        auto.defer("E:", at(2000));
        auto.went_stale("F:", at(1000));
        assert_eq!(auto.next(at(2000)), Some(("E:".into(), Reason::Startup)));
        assert_eq!(auto.next(at(2000)), Some(("F:".into(), Reason::Stale)));
        // A scan installed while a put-off sync waits: it is not waited for either.
        auto.defer("G:", at(3000));
        auto.installed("G:");
        assert_eq!(auto.next(at(3000)), None);
        // F:'s stale sync starts; its old watcher misses more while it runs.
        auto.ran("F:", at(2000));
        auto.went_stale("F:", at(2100));
        auto.installed("F:");
        assert_eq!(auto.stale_due("F:"), None);
        // The back-off stays across it: the new watcher missing changes soon waits the gap.
        auto.went_stale("F:", at(2200));
        assert_eq!(auto.stale_due("F:"), Some(at(2000 + 600)));
    }

    #[test]
    fn no_sync_while_a_scan_or_sync_of_the_drive_runs() {
        let mut jobs = Jobs::default();
        let c = jobs.sync_started("C:").unwrap();
        assert!(jobs.syncing("C:") && jobs.busy("C:"));
        assert_eq!(jobs.sync_started("C:").err(), Some(Busy::Syncing));
        // Another drive is not held up.
        let d = jobs.sync_started("D:").unwrap();
        // A scan starting stops the sync of its drive only.
        jobs.scan_started("C:");
        assert!(c.cancelled() && !d.cancelled());
        jobs.sync_ended("C:", &c);
        assert!(!jobs.syncing("C:") && jobs.busy("C:"));
        assert_eq!(jobs.sync_started("C:").err(), Some(Busy::Scanning));
        // Two scans at once: busy until both end.
        jobs.scan_started("C:");
        jobs.scan_ended("C:");
        assert_eq!(jobs.sync_started("C:").err(), Some(Busy::Scanning));
        jobs.scan_ended("C:");
        assert!(!jobs.busy("C:"));
        let again = jobs.sync_started("C:").unwrap();
        // An old sync ending does not end the new one.
        jobs.sync_ended("C:", &c);
        assert!(jobs.syncing("C:"));
        jobs.sync_ended("C:", &again);
        jobs.sync_ended("D:", &d);
        assert!(!jobs.busy("C:") && !jobs.busy("D:"));
    }
}
