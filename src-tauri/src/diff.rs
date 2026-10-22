//! Comparing a drive's index with the one from the scan before it: which
//! folders grew, shrank, appeared or went away.
//!
//! The previous scan is kept as `<letter>.prev.idx` next to the cache when a
//! new scan replaces it (scan/mod.rs `keep_previous`, from the copy kept of
//! it as it was scanned), and loaded only when a comparison is asked for.

use crate::index::{FLAG_DELETED, FLAG_DIR, FLAG_HARDLINK, Index};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

/// Nodes in one comparison at most. Past this the folders that changed least
/// are left unopened, like folders at the depth limit.
const MAX_NODES: usize = 10_000;

#[derive(Serialize, Clone)]
pub struct DiffNode {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// Now (0 when removed).
    pub size: u64,
    /// At the previous scan (0 when new).
    pub prev_size: u64,
    pub files: u32,
    pub prev_files: u32,
    /// "grown" | "shrunk" | "new" | "removed" | "same"
    pub status: String,
    /// Biggest change (|size − prev_size|) first; only changed children unless
    /// none changed. Empty at the depth limit.
    pub children: Vec<DiffNode>,
}

#[derive(Serialize, Clone)]
pub struct DiffInfo {
    pub drive: String,
    pub has_prev: bool,
    /// FILETIME of the current and the previous scan.
    pub scanned_at: Option<i64>,
    pub prev_scanned_at: Option<i64>,
    /// How each was made: "mft" or "walk" (a sync of an MFT scan counts as
    /// "mft", `coverage`). A walk leaves out the folders this user cannot
    /// open, which an MFT scan counts, so where the two differ those folders
    /// show as new or removed although nothing changed.
    pub method: Option<String>,
    pub prev_method: Option<String>,
}

/// What a scan made with `method` saw, for the warning about two scans made
/// differently (DiffInfo's methods): a sync (scan/sync.rs) keeps what an MFT
/// scan found in the folders a walk cannot open, so it is "mft" here; "walk"
/// stays "walk".
pub fn coverage(method: String) -> String {
    if crate::scan::sync::sees_protected(&method) { "mft".into() } else { method }
}

/// One entry of the comparison: its node in the current index, in the
/// previous one, or both.
#[derive(Clone, Copy)]
struct Pair {
    now: Option<u32>,
    prev: Option<u32>,
}

/// A node of the comparison while it is being built: children are slot numbers.
struct Slot {
    pair: Pair,
    name: String,
    path: String,
    level: u32,
    kids: Vec<usize>,
}

/// The comparison of `path` in `now` with the same path in `prev`, down to
/// `depth` levels (1..=6), at most `limit` children per folder.
///
/// The path is looked up in each index as `Index::find` does (names compared
/// case-insensitively, an exact match first); None when neither has it. One
/// that is only in `prev` is a "removed" root, one only in `now` a "new" one.
/// Children are matched by name the same way. Removed entries and extra hard
/// links (which folder totals leave out) are skipped on both sides, so the
/// changes of a folder's children add up to the folder's own.
///
/// The folders that changed most are opened first, so if the comparison
/// reaches its node limit (`MAX_NODES`) it is the smallest changes that stay
/// closed.
pub fn build(now: &Index, prev: &Index, path: &str, depth: u32, limit: usize) -> Option<DiffNode> {
    let sides = Sides { now, prev };
    let root = Pair { now: now.find(path), prev: prev.find(path) };
    let path = match root {
        Pair { now: Some(n), .. } => now.path_of(n),
        Pair { prev: Some(p), .. } => prev.path_of(p),
        _ => return None,
    };
    let depth = depth.clamp(1, 6);
    let mut slots = vec![Slot { pair: root, name: sides.name(root).to_string(), path, level: 0, kids: Vec::new() }];
    // Folders still to open, biggest change first (then in the order they were listed).
    let mut open: BinaryHeap<(u64, Reverse<usize>)> = BinaryHeap::new();
    if sides.is_dir(root) {
        open.push((sides.delta(root), Reverse(0)));
    }
    while let Some((_, Reverse(s))) = open.pop() {
        let room = MAX_NODES.saturating_sub(slots.len());
        if room == 0 {
            break;
        }
        let level = slots[s].level;
        for c in sides.listed(slots[s].pair, limit.min(room)) {
            let name = sides.name(c).to_string();
            let path = join(&slots[s].path, &name);
            let k = slots.len();
            if level + 1 < depth && sides.is_dir(c) {
                open.push((sides.delta(c), Reverse(k)));
            }
            slots.push(Slot { pair: c, name, path, level: level + 1, kids: Vec::new() });
            slots[s].kids.push(k);
        }
    }
    Some(assemble(&sides, &mut slots, 0))
}

/// The two indexes being compared.
struct Sides<'a> {
    now: &'a Index,
    prev: &'a Index,
}

impl Sides<'_> {
    fn sizes(&self, p: Pair) -> (u64, u64) {
        (p.now.map_or(0, |n| self.now.nodes[n as usize].size), p.prev.map_or(0, |o| self.prev.nodes[o as usize].size))
    }

    fn files(&self, p: Pair) -> (u32, u32) {
        (p.now.map_or(0, |n| self.now.nodes[n as usize].files), p.prev.map_or(0, |o| self.prev.nodes[o as usize].files))
    }

    /// |size − prev_size|
    fn delta(&self, p: Pair) -> u64 {
        let (size, prev_size) = self.sizes(p);
        size.abs_diff(prev_size)
    }

    /// What it is now, or was when it is gone.
    fn is_dir(&self, p: Pair) -> bool {
        match p {
            Pair { now: Some(n), .. } => is_dir(self.now, n),
            Pair { prev: Some(o), .. } => is_dir(self.prev, o),
            _ => false,
        }
    }

    /// Its name now (which may differ in case from the old one), or when it was there.
    fn name(&self, p: Pair) -> &str {
        match p {
            Pair { now: Some(n), .. } => self.now.name(n),
            Pair { prev: Some(o), .. } => self.prev.name(o),
            _ => "",
        }
    }

    fn status(&self, p: Pair) -> &'static str {
        match p {
            Pair { now: Some(_), prev: None } => "new",
            Pair { now: None, prev: Some(_) } => "removed",
            _ => {
                let (size, prev_size) = self.sizes(p);
                match size.cmp(&prev_size) {
                    std::cmp::Ordering::Greater => "grown",
                    std::cmp::Ordering::Less => "shrunk",
                    std::cmp::Ordering::Equal => "same",
                }
            }
        }
    }

    /// Anything to see: a size, a file count or a kind that is not what it was.
    fn changed(&self, p: Pair) -> bool {
        match p {
            Pair { now: Some(n), prev: Some(o) } => {
                let (files, prev_files) = self.files(p);
                self.delta(p) != 0 || files != prev_files || is_dir(self.now, n) != is_dir(self.prev, o)
            }
            _ => true,
        }
    }

    /// The children of `p` to show, at most `most`: the changed ones, or all
    /// of them when none changed. Biggest change first, then biggest, then
    /// by name, so a comparison keeps its order between calls.
    fn listed(&self, p: Pair, most: usize) -> Vec<Pair> {
        let all = self.children(p);
        let mut listed: Vec<Pair> = all.iter().copied().filter(|&c| self.changed(c)).collect();
        if listed.is_empty() {
            listed = all;
        }
        let biggest = |c: Pair| {
            let (size, prev_size) = self.sizes(c);
            size.max(prev_size)
        };
        listed.sort_by_cached_key(|&c| (Reverse(self.delta(c)), Reverse(biggest(c)), fold(self.name(c)), self.name(c).to_string()));
        listed.truncate(most);
        listed
    }

    /// Every counted child of `p` on either side, matched by name. A folder
    /// on one side only has only new (or only removed) children.
    fn children(&self, p: Pair) -> Vec<Pair> {
        let now_kids = p.now.map(|n| counted_children(self.now, n)).unwrap_or_default();
        let prev_kids = p.prev.map(|o| counted_children(self.prev, o)).unwrap_or_default();
        let matched = match_names(self.now, &now_kids, self.prev, &prev_kids);
        let taken: HashSet<u32> = matched.iter().flatten().copied().collect();
        let mut out: Vec<Pair> = now_kids.iter().zip(matched).map(|(&n, o)| Pair { now: Some(n), prev: o }).collect();
        out.extend(prev_kids.into_iter().filter(|o| !taken.contains(o)).map(|o| Pair { now: None, prev: Some(o) }));
        out
    }
}

/// For each of `now_kids`, the one of `prev_kids` with the same name, if any.
/// Names are compared case-insensitively, as `Index::find` does. Where a
/// case-sensitive folder holds several ("Build" and "build"), each takes the
/// one with exactly its name first, then any left over.
fn match_names(now: &Index, now_kids: &[u32], prev: &Index, prev_kids: &[u32]) -> Vec<Option<u32>> {
    let mut matched = vec![None; now_kids.len()];
    if now_kids.is_empty() || prev_kids.is_empty() {
        return matched;
    }
    let mut by_name: HashMap<String, Vec<u32>> = HashMap::with_capacity(prev_kids.len());
    for &o in prev_kids {
        by_name.entry(fold(prev.name(o))).or_default().push(o);
    }
    let keys: Vec<String> = now_kids.iter().map(|&n| fold(now.name(n))).collect();
    let mut taken: HashSet<u32> = HashSet::new();
    for exact in [true, false] {
        for (i, &n) in now_kids.iter().enumerate() {
            if matched[i].is_some() {
                continue;
            }
            let Some(same) = by_name.get(&keys[i]) else { continue };
            let pick = same.iter().copied().find(|&o| !taken.contains(&o) && (!exact || prev.name(o) == now.name(n)));
            if let Some(o) = pick {
                taken.insert(o);
                matched[i] = Some(o);
            }
        }
    }
    matched
}

/// The children of `i` that count towards its size: not removed, and not an
/// extra hard link of a file counted elsewhere.
fn counted_children(ix: &Index, i: u32) -> Vec<u32> {
    if !is_dir(ix, i) {
        return Vec::new();
    }
    ix.children_of(i).filter(|&c| ix.nodes[c as usize].flags & (FLAG_DELETED | FLAG_HARDLINK) == 0).collect()
}

fn is_dir(ix: &Index, i: u32) -> bool {
    ix.nodes[i as usize].flags & FLAG_DIR != 0
}

/// Lowercases a name the way index.rs `fold` does (one character at a time,
/// only where the lowercase letter has the same UTF-8 length), so two names
/// match here exactly when `Index::find` would take one for the other.
fn fold(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() {
                return c.to_ascii_lowercase();
            }
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(one), None) if one.len_utf8() == c.len_utf8() => one,
                _ => c,
            }
        })
        .collect()
}

/// `parent`'s path with `name` below it (the drive root already ends in `\`).
fn join(parent: &str, name: &str) -> String {
    if parent.ends_with('\\') { format!("{parent}{name}") } else { format!("{parent}\\{name}") }
}

fn assemble(sides: &Sides, slots: &mut [Slot], s: usize) -> DiffNode {
    let kids = std::mem::take(&mut slots[s].kids);
    let children = kids.into_iter().map(|k| assemble(sides, slots, k)).collect();
    let slot = &mut slots[s];
    let p = slot.pair;
    let (size, prev_size) = sides.sizes(p);
    let (files, prev_files) = sides.files(p);
    DiffNode {
        name: std::mem::take(&mut slot.name),
        path: std::mem::take(&mut slot.path),
        is_dir: sides.is_dir(p),
        size,
        prev_size,
        files,
        prev_files,
        status: sides.status(p).into(),
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_counts_as_what_it_kept() {
        assert_eq!(coverage("sync".into()), "mft");
        assert_eq!(coverage("mft".into()), "mft");
        assert_eq!(coverage("walk".into()), "walk");
    }
    use crate::index::RawEntry;

    fn entry(id: u64, parent: u64, name: &str, dir: bool, size: u64) -> RawEntry {
        RawEntry { id, parent, name: name.into(), flags: if dir { FLAG_DIR } else { 0 }, size, alloc: size, modified: 0 }
    }

    /// An index of C: from (id, parent, name, is_dir, size); the root is id 1.
    fn index(entries: &[(u64, u64, &str, bool, u64)]) -> Index {
        let mut raw = vec![entry(1, 1, ".", true, 0)];
        raw.extend(entries.iter().map(|&(id, parent, name, dir, size)| entry(id, parent, name, dir, size)));
        Index::build("C:", "test", 0, raw, 1)
    }

    /// ```text
    /// before                       now
    /// C:\                          C:\
    ///   Games          500           Games          900
    ///     big.pak      500             big.pak      900
    ///   Downloads      300           Downloads      100
    ///     a.zip        200             a.zip        100
    ///     b.iso        100           Photos         250   (new)
    ///   Old            120             x.jpg        250
    ///     log.txt      120           Docs            40
    ///   Docs            40             r.pdf         40
    ///     r.pdf         40
    /// ```
    fn before() -> Index {
        index(&[
            (2, 1, "Games", true, 0),
            (3, 2, "big.pak", false, 500),
            (4, 1, "Downloads", true, 0),
            (5, 4, "a.zip", false, 200),
            (6, 4, "b.iso", false, 100),
            (7, 1, "Old", true, 0),
            (8, 7, "log.txt", false, 120),
            (9, 1, "Docs", true, 0),
            (10, 9, "r.pdf", false, 40),
        ])
    }

    fn after() -> Index {
        index(&[
            (2, 1, "Games", true, 0),
            (3, 2, "big.pak", false, 900),
            (4, 1, "Downloads", true, 0),
            (5, 4, "a.zip", false, 100),
            (20, 1, "Photos", true, 0),
            (21, 20, "x.jpg", false, 250),
            (9, 1, "Docs", true, 0),
            (10, 9, "r.pdf", false, 40),
        ])
    }

    fn summary(n: &DiffNode) -> (&str, &str, u64, u64) {
        (n.name.as_str(), n.status.as_str(), n.size, n.prev_size)
    }

    #[test]
    fn statuses_and_biggest_change_first() {
        let (now, prev) = (after(), before());
        let d = build(&now, &prev, "C:\\", 3, 64).unwrap();
        assert_eq!(summary(&d), ("C:", "grown", 1290, 960));
        assert_eq!((d.path.as_str(), d.files, d.prev_files), ("C:\\", 4, 5));
        // Unchanged Docs is left out; the rest by |change|: +400, +250, −200, −120.
        let kids: Vec<_> = d.children.iter().map(summary).collect();
        assert_eq!(
            kids,
            [("Games", "grown", 900, 500), ("Photos", "new", 250, 0), ("Downloads", "shrunk", 100, 300), ("Old", "removed", 0, 120)]
        );
        let downloads = &d.children[2];
        assert_eq!(downloads.path, "C:\\Downloads");
        assert_eq!(downloads.children.iter().map(summary).collect::<Vec<_>>(), [("a.zip", "shrunk", 100, 200), ("b.iso", "removed", 0, 100)]);
        assert_eq!(d.children[1].children.iter().map(summary).collect::<Vec<_>>(), [("x.jpg", "new", 250, 0)]);
        assert_eq!(d.children[3].children.iter().map(summary).collect::<Vec<_>>(), [("log.txt", "removed", 0, 120)]);
        assert!(d.children[3].is_dir && !downloads.children[1].is_dir);
        // The children's changes add up to the folder's.
        let sum: i64 = d.children.iter().map(|c| c.size as i64 - c.prev_size as i64).sum();
        assert_eq!(sum, 1290 - 960);
    }

    #[test]
    fn depth_and_limit() {
        let (now, prev) = (after(), before());
        let d = build(&now, &prev, "C:\\", 1, 64).unwrap();
        assert_eq!(d.children.len(), 4);
        assert!(d.children.iter().all(|c| c.children.is_empty()));
        let d = build(&now, &prev, "C:\\", 3, 2).unwrap();
        assert_eq!(d.children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Games", "Photos"]);
        // Out of range depths are brought into 1..=6.
        assert_eq!(build(&now, &prev, "C:\\", 0, 64).unwrap().children.len(), 4);
        assert!(build(&now, &prev, "C:\\", 99, 64).is_some());
    }

    #[test]
    fn nothing_changed_lists_everything() {
        let (a, b) = (before(), before());
        let d = build(&a, &b, "C:\\", 2, 64).unwrap();
        assert_eq!(d.status, "same");
        let kids: Vec<_> = d.children.iter().map(summary).collect();
        assert_eq!(kids, [("Games", "same", 500, 500), ("Downloads", "same", 300, 300), ("Old", "same", 120, 120), ("Docs", "same", 40, 40)]);
        assert_eq!(d.children[1].children.len(), 2);
    }

    #[test]
    fn a_folder_on_one_side_only() {
        let (now, prev) = (after(), before());
        let gone = build(&now, &prev, "C:\\Old", 3, 64).unwrap();
        assert_eq!(summary(&gone), ("Old", "removed", 0, 120));
        assert_eq!((gone.path.as_str(), gone.is_dir, gone.prev_files), ("C:\\Old", true, 1));
        assert_eq!(gone.children.iter().map(summary).collect::<Vec<_>>(), [("log.txt", "removed", 0, 120)]);
        let new = build(&now, &prev, "c:\\photos\\", 3, 64).unwrap();
        assert_eq!(summary(&new), ("Photos", "new", 250, 0));
        assert_eq!(new.path, "C:\\Photos");
        assert_eq!(new.children[0].path, "C:\\Photos\\x.jpg");
        assert!(build(&now, &prev, "C:\\Nowhere", 3, 64).is_none());
        assert!(build(&now, &prev, "D:\\Games", 3, 64).is_none());
    }

    #[test]
    fn names_match_case_insensitively_exact_first() {
        // A case-sensitive folder holding both "Build" and "build".
        let prev = index(&[
            (2, 1, "Build", true, 0),
            (3, 2, "out.bin", false, 100),
            (4, 1, "build", true, 0),
            (5, 4, "tmp", false, 10),
            (6, 1, "ÄRGER.txt", false, 5),
        ]);
        let now = index(&[
            (2, 1, "build", true, 0),
            (3, 2, "tmp", false, 30),
            (4, 1, "BUILD", true, 0),
            (5, 4, "OUT.bin", false, 100),
            (6, 1, "ärger.txt", false, 5),
        ]);
        let d = build(&now, &prev, "C:\\", 2, 64).unwrap();
        // "build" pairs with "build" (exact), "BUILD" with what is left ("Build").
        // The renamed-in-case ärger.txt is the same file.
        let kids: Vec<_> = d.children.iter().map(summary).collect();
        assert_eq!(kids, [("build", "grown", 30, 10)]);
        assert_eq!(d.children[0].children.iter().map(summary).collect::<Vec<_>>(), [("tmp", "grown", 30, 10)]);
        let other = build(&now, &prev, "C:\\BUILD", 2, 64).unwrap();
        assert_eq!(summary(&other), ("BUILD", "same", 100, 100));
        // The path is found as Index::find finds it: exact name first.
        let exact = build(&now, &prev, "C:\\build", 1, 64).unwrap();
        assert_eq!(exact.size, 30);
    }

    #[test]
    fn removed_and_hard_linked_entries_are_not_counted() {
        let mut prev = index(&[(2, 1, "a", true, 0), (3, 2, "f.txt", false, 50), (4, 2, "gone.txt", false, 70)]);
        let n = prev.find("C:\\a\\gone.txt").unwrap();
        prev.remove(n);
        let mut raw = vec![entry(1, 1, ".", true, 0), entry(2, 1, "a", true, 0), entry(3, 2, "f.txt", false, 50)];
        let mut link = entry(4, 2, "link.txt", false, 50);
        link.flags |= FLAG_HARDLINK;
        raw.push(link);
        let now = Index::build("C:", "test", 0, raw, 1);
        let d = build(&now, &prev, "C:\\a", 2, 64).unwrap();
        assert_eq!(summary(&d), ("a", "same", 50, 50));
        // Nothing changed, so the one counted file is listed as it is.
        assert_eq!(d.children.iter().map(summary).collect::<Vec<_>>(), [("f.txt", "same", 50, 50)]);
    }

    #[test]
    fn the_biggest_changes_are_opened_first() {
        // Three new folders of 5000 files each: not all of it fits.
        let mut raw = vec![entry(1, 1, ".", true, 0)];
        for (f, name) in ["small", "mid", "big"].into_iter().enumerate() {
            let dir = 10 + f as u64;
            raw.push(entry(dir, 1, name, true, 0));
            for i in 0..5000 {
                raw.push(entry(dir * 10_000 + i, dir, &format!("f{i}.bin"), false, f as u64 + 1));
            }
        }
        let now = Index::build("C:", "test", 0, raw, 1);
        let prev = index(&[]);
        let d = build(&now, &prev, "C:\\", 2, 20_000).unwrap();
        let count = |n: &DiffNode| n.children.len();
        assert_eq!(d.children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["big", "mid", "small"]);
        // The root and its three, all of "big", then what is left for "mid".
        assert_eq!(1 + 3 + d.children.iter().map(count).sum::<usize>(), MAX_NODES);
        assert_eq!(count(&d.children[0]), 5000);
        assert_eq!(count(&d.children[1]), MAX_NODES - 4 - 5000);
        assert_eq!(count(&d.children[2]), 0);
    }

    #[test]
    fn fold_matches_the_index() {
        // A name the index finds under another case is matched here too, and the reverse.
        let ix = index(&[(2, 1, "ΣΟΦΙΑ İstanbul Ωmega", false, 1)]);
        for probe in ["σοφια i̇stanbul ωmega", "σοφια İstanbul ωmega", "ΣΟΦΙΑ İSTANBUL ΩMEGA", "σοφια İstanbul \u{2126}mega"] {
            let found = ix.find(&format!("C:\\{probe}")).is_some();
            assert_eq!(found, fold(probe) == fold("ΣΟΦΙΑ İstanbul Ωmega"), "{probe}");
        }
    }
}
