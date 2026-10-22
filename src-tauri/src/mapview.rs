//! The size map (icicle, sunburst, treemap): a folder's subtree, trimmed to a
//! few levels, with small items folded into "other".

use crate::classify;
use crate::index::{FLAG_DELETED, FLAG_DIR, FLAG_HARDLINK, Index};
use serde::Serialize;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

/// Children listed per folder at most; the rest go into "other".
const MAX_CHILDREN: usize = 64;
/// Nodes in one map at most. With a tiny `min_ratio` a deep map could list
/// 64 children at every level; past this the smaller folders are left
/// unopened, like folders at the depth limit.
const MAX_NODES: usize = 10_000;

#[derive(Serialize, Clone)]
pub struct MapNode {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub files: u32,
    pub is_dir: bool,
    /// classify::category
    pub category: String,
    /// Biggest first. Empty for files and for folders at the depth limit.
    pub children: Vec<MapNode>,
    /// Children too small to list (below min_ratio of the map's root), summed.
    pub other_size: u64,
    pub other_count: u32,
}

/// A map node while the map is being built: children are slot numbers.
struct Slot {
    id: u32,
    name: String,
    path: String,
    level: u32,
    kids: Vec<usize>,
    other_size: u64,
    other_count: u32,
}

/// `node`'s subtree down to `depth` levels (1..=8). Children smaller than
/// `min_ratio` (0.0..0.1) of `node`'s size are folded into other_size/other_count.
///
/// A folder lists at most 64 children, biggest first, and always its biggest
/// one, so a folder with only small things in it does not look empty. Empty
/// files and folders are folded too. Removed entries and extra hard links
/// (which folder totals leave out) are skipped, so a folder's listed sizes
/// plus `other_size` add up to its size. Only listed folders are opened:
/// what is folded is counted from its total, never walked.
///
/// The biggest folders are opened first, so if the map reaches its node
/// limit (`MAX_NODES`) it is the smallest ones that stay closed.
pub fn build(ix: &Index, node: u32, depth: u32, min_ratio: f64) -> MapNode {
    let root_size = ix.nodes[node as usize].size;
    let ratio = if min_ratio.is_finite() { min_ratio.max(0.0) } else { 0.0 };
    // Rounded up, so a child of exactly the ratio is listed. The cast saturates.
    let threshold = (ratio * root_size as f64).ceil() as u64;

    let mut slots = vec![Slot {
        id: node,
        name: ix.name(node).to_string(),
        path: ix.path_of(node),
        level: 0,
        kids: Vec::new(),
        other_size: 0,
        other_count: 0,
    }];
    // Folders still to open, biggest first (then in the order they were listed).
    let mut open: BinaryHeap<(u64, Reverse<usize>)> = BinaryHeap::new();
    if depth > 0 && is_dir(ix, node) {
        open.push((root_size, Reverse(0)));
    }
    while let Some((_, Reverse(s))) = open.pop() {
        let room = MAX_NODES.saturating_sub(slots.len());
        if room == 0 {
            break;
        }
        let (dir, level) = (slots[s].id, slots[s].level);
        let (listed, other_size, other_count) = pick(ix, dir, threshold, room.min(MAX_CHILDREN));
        slots[s].other_size = other_size;
        slots[s].other_count = other_count;
        for c in listed {
            let path = join(&slots[s].path, ix.name(c));
            let k = slots.len();
            slots.push(Slot {
                id: c,
                name: ix.name(c).to_string(),
                path,
                level: level + 1,
                kids: Vec::new(),
                other_size: 0,
                other_count: 0,
            });
            slots[s].kids.push(k);
            if level + 1 < depth && is_dir(ix, c) {
                open.push((ix.nodes[c as usize].size, Reverse(k)));
            }
        }
    }
    assemble(ix, &mut slots, 0)
}

fn is_dir(ix: &Index, i: u32) -> bool {
    ix.nodes[i as usize].flags & FLAG_DIR != 0
}

/// `parent`'s path with `name` below it (the drive root already ends in `\`).
fn join(parent: &str, name: &str) -> String {
    if parent.ends_with('\\') { format!("{parent}{name}") } else { format!("{parent}\\{name}") }
}

/// Folder `dir`'s children to list, biggest first (at most `most`, at least
/// one if it has any), and the size and number of the rest.
fn pick(ix: &Index, dir: u32, threshold: u64, most: usize) -> (Vec<u32>, u64, u32) {
    let mut big: Vec<(u64, u32)> = Vec::new();
    let mut biggest: Option<(u64, u32)> = None;
    let (mut total, mut count) = (0u64, 0u32);
    for c in ix.children_of(dir) {
        let n = &ix.nodes[c as usize];
        if n.flags & (FLAG_DELETED | FLAG_HARDLINK) != 0 {
            continue;
        }
        total = total.saturating_add(n.size);
        count += 1;
        if n.size > 0 && n.size >= threshold {
            big.push((n.size, c));
        } else if biggest.is_none_or(|b| by_size((n.size, c), b) == Ordering::Less) {
            biggest = Some((n.size, c));
        }
    }
    if big.is_empty() {
        big.extend(biggest);
    }
    let order = |a: &(u64, u32), b: &(u64, u32)| by_size(*a, *b);
    if big.len() > most {
        big.select_nth_unstable_by(most, order);
        big.truncate(most);
    }
    // Same sizes by name, so the map keeps its order between calls.
    big.sort_unstable_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            let (x, y) = (ix.name(a.1), ix.name(b.1));
            x.chars().flat_map(char::to_lowercase).cmp(y.chars().flat_map(char::to_lowercase)).then(a.1.cmp(&b.1))
        })
    });
    let listed: u64 = big.iter().fold(0u64, |sum, &(size, _)| sum.saturating_add(size));
    let other_count = count - big.len() as u32;
    (big.into_iter().map(|(_, c)| c).collect(), total.saturating_sub(listed), other_count)
}

/// Biggest first, then the lower node number.
fn by_size(a: (u64, u32), b: (u64, u32)) -> Ordering {
    b.0.cmp(&a.0).then(a.1.cmp(&b.1))
}

fn assemble(ix: &Index, slots: &mut [Slot], s: usize) -> MapNode {
    let kids = std::mem::take(&mut slots[s].kids);
    let children = kids.into_iter().map(|k| assemble(ix, slots, k)).collect();
    let slot = &mut slots[s];
    let n = &ix.nodes[slot.id as usize];
    let is_dir = n.flags & FLAG_DIR != 0;
    let name = std::mem::take(&mut slot.name);
    let path = std::mem::take(&mut slot.path);
    MapNode {
        category: classify::category(&path, &name, is_dir).into(),
        name,
        path,
        size: n.size,
        files: n.files,
        is_dir,
        children,
        other_size: slot.other_size,
        other_count: slot.other_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Meta, RawEntry, ROOT};

    fn entry(id: u64, parent: u64, name: &str, dir: bool, size: u64) -> RawEntry {
        RawEntry {
            id,
            parent,
            name: name.into(),
            flags: if dir { FLAG_DIR } else { 0 },
            size,
            alloc: size,
            modified: 0,
        }
    }

    /// C:\ (id 1) with folders and files of known sizes.
    ///
    /// ```text
    /// C:\                          1000
    ///   Users                       600
    ///     me                        600
    ///       Pictures                400  (a.jpg 250, b.jpg 150)
    ///       Documents               150  (r.pdf 100, s.txt 50)
    ///       notes.txt                50
    ///   GitHub                      300
    ///     app                       300
    ///       node_modules            290  (many small files)
    ///       main.rs                  10
    ///   tiny1.txt                    40
    ///   tiny2.txt                    30
    ///   tiny3.txt                    30
    /// ```
    fn sample() -> Index {
        let mut raw = vec![
            entry(1, 1, ".", true, 0),
            entry(2, 1, "Users", true, 0),
            entry(3, 2, "me", true, 0),
            entry(4, 3, "Pictures", true, 0),
            entry(5, 4, "a.jpg", false, 250),
            entry(6, 4, "b.jpg", false, 150),
            entry(7, 3, "Documents", true, 0),
            entry(8, 7, "r.pdf", false, 100),
            entry(9, 7, "s.txt", false, 50),
            entry(10, 3, "notes.txt", false, 50),
            entry(11, 1, "GitHub", true, 0),
            entry(12, 11, "app", true, 0),
            entry(13, 12, "node_modules", true, 0),
            entry(14, 12, "main.rs", false, 10),
            entry(15, 1, "tiny1.txt", false, 40),
            entry(16, 1, "tiny2.txt", false, 30),
            entry(17, 1, "tiny3.txt", false, 30),
        ];
        // node_modules: 29 files of 10 bytes.
        for i in 0..29 {
            raw.push(entry(100 + i, 13, &format!("m{i}.js"), false, 10));
        }
        Index::build("C:", "test", 0, raw, 1)
    }

    fn names(n: &MapNode) -> Vec<&str> {
        n.children.iter().map(|c| c.name.as_str()).collect()
    }

    fn child<'a>(n: &'a MapNode, name: &str) -> &'a MapNode {
        n.children.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} not in {}", n.path))
    }

    /// Every opened folder's listed sizes and "other" add up to its size,
    /// children come biggest first, and paths are the index's own.
    fn check_tree(ix: &Index, n: &MapNode) -> usize {
        let opened = !n.children.is_empty() || n.other_count > 0;
        if opened {
            let listed: u64 = n.children.iter().map(|c| c.size).sum();
            assert_eq!(listed + n.other_size, n.size, "{}", n.path);
        }
        assert!(n.children.len() <= MAX_CHILDREN);
        assert!(n.children.windows(2).all(|w| w[0].size >= w[1].size), "{}", n.path);
        assert_eq!(ix.find(&n.path).map(|i| ix.path_of(i)).as_deref(), Some(n.path.as_str()));
        1 + n.children.iter().map(|c| check_tree(ix, c)).sum::<usize>()
    }

    #[test]
    fn lists_big_children_and_folds_small_ones() {
        let ix = sample();
        let map = build(&ix, ROOT, 1, 0.05);
        assert_eq!(map.path, r"C:\");
        assert_eq!(map.name, "C:");
        assert_eq!(map.size, 1000);
        assert!(map.is_dir);
        // 5% of 1000 is 50: the three tiny files are folded.
        assert_eq!(names(&map), ["Users", "GitHub"]);
        assert_eq!((map.other_size, map.other_count), (100, 3));
        // Depth 1: the children are not opened.
        assert!(map.children.iter().all(|c| c.children.is_empty() && c.other_count == 0));
        assert_eq!(child(&map, "Users").files, 5);
        check_tree(&ix, &map);
    }

    #[test]
    fn depth_and_paths() {
        let ix = sample();
        let map = build(&ix, ROOT, 4, 0.05);
        let me = child(child(&map, "Users"), "me");
        assert_eq!(me.path, r"C:\Users\me");
        assert_eq!(names(me), ["Pictures", "Documents", "notes.txt"]);
        let pictures = child(me, "Pictures");
        assert_eq!(pictures.path, r"C:\Users\me\Pictures");
        // Level 3 is opened, level 4 (the files) is not a folder.
        assert_eq!(names(pictures), ["a.jpg", "b.jpg"]);
        assert_eq!(child(pictures, "a.jpg").path, r"C:\Users\me\Pictures\a.jpg");
        assert!(!child(pictures, "a.jpg").is_dir);
        assert_eq!(child(pictures, "a.jpg").files, 0);
        // Depth 3 stops above the files.
        let map3 = build(&ix, ROOT, 3, 0.05);
        let me3 = child(child(&map3, "Users"), "me");
        assert!(child(me3, "Pictures").children.is_empty());
        assert_eq!(child(me3, "Pictures").other_count, 0);
        check_tree(&ix, &map);
        check_tree(&ix, &map3);
    }

    #[test]
    fn ratio_is_of_the_maps_root() {
        let ix = sample();
        // Documents (150) under C:\ at 10% (100): r.pdf (100) stays, s.txt (50) is folded.
        let map = build(&ix, ROOT, 4, 0.1);
        let docs = child(child(child(&map, "Users"), "me"), "Documents");
        assert_eq!(names(docs), ["r.pdf"]);
        assert_eq!((docs.other_size, docs.other_count), (50, 1));
        // The same folder as the map's root lists both.
        let at = ix.find(r"C:\Users\me\Documents").unwrap();
        let map = build(&ix, at, 2, 0.1);
        assert_eq!(names(&map), ["r.pdf", "s.txt"]);
        assert_eq!(map.other_count, 0);
    }

    #[test]
    fn keeps_the_biggest_child_of_a_folder_of_small_things() {
        let ix = sample();
        let map = build(&ix, ROOT, 4, 0.05);
        let nm = child(child(child(&map, "GitHub"), "app"), "node_modules");
        // 29 files of 10 bytes, each below 50: the first by name stays.
        assert_eq!(names(nm), ["m0.js"]);
        assert_eq!((nm.other_size, nm.other_count), (280, 28));
        check_tree(&ix, &map);
    }

    #[test]
    fn categories() {
        let ix = sample();
        let map = build(&ix, ROOT, 4, 0.0);
        assert_eq!(map.category, "other");
        let users = child(&map, "Users");
        assert_eq!(users.category, "personal");
        let me = child(users, "me");
        assert_eq!(child(me, "Pictures").category, "media");
        assert_eq!(child(me, "Documents").category, "document");
        assert_eq!(child(child(me, "Documents"), "r.pdf").category, "document");
        let app = child(child(&map, "GitHub"), "app");
        assert_eq!(app.category, "dev");
        assert_eq!(child(app, "node_modules").category, "dev");
    }

    #[test]
    fn at_most_64_children() {
        let mut raw = vec![entry(1, 1, ".", true, 0), entry(2, 1, "big", true, 0)];
        for i in 0..100 {
            raw.push(entry(10 + i, 2, &format!("f{i:03}"), false, 1000 + i));
        }
        let ix = Index::build("D:", "test", 0, raw, 1);
        let map = build(&ix, ROOT, 2, 0.0);
        let big = child(&map, "big");
        assert_eq!(big.children.len(), 64);
        assert_eq!(big.children[0].name, "f099");
        assert_eq!(big.children[63].name, "f036");
        assert_eq!(big.other_count, 36);
        assert_eq!(big.other_size, (1000..1036).sum::<u64>());
        check_tree(&ix, &map);
    }

    #[test]
    fn same_sizes_go_by_name() {
        let raw = vec![
            entry(1, 1, ".", true, 0),
            entry(2, 1, "b", false, 10),
            entry(3, 1, "C", false, 10),
            entry(4, 1, "a", false, 10),
            entry(5, 1, "z", false, 20),
        ];
        let ix = Index::build("D:", "test", 0, raw, 1);
        assert_eq!(names(&build(&ix, ROOT, 1, 0.0)), ["z", "a", "b", "C"]);
    }

    #[test]
    fn skips_removed_entries_and_extra_hard_links() {
        let mut raw = vec![
            entry(1, 1, ".", true, 0),
            entry(2, 1, "keep", false, 100),
            entry(3, 1, "gone", true, 0),
            entry(4, 3, "inside", false, 500),
            entry(5, 1, "link", false, 100),
        ];
        raw[4].flags |= FLAG_HARDLINK;
        let mut ix = Index::build("D:", "test", 0, raw, 1);
        let gone = ix.find(r"D:\gone").unwrap();
        ix.remove(gone);
        let map = build(&ix, ROOT, 3, 0.0);
        assert_eq!(map.size, 100);
        assert_eq!(names(&map), ["keep"]);
        assert_eq!((map.other_size, map.other_count), (0, 0));
        check_tree(&ix, &map);
    }

    #[test]
    fn empty_entries_are_folded() {
        let raw = vec![
            entry(1, 1, ".", true, 0),
            entry(2, 1, "data", false, 10),
            entry(3, 1, "empty", true, 0),
            entry(4, 1, "zero.txt", false, 0),
        ];
        let ix = Index::build("D:", "test", 0, raw, 1);
        let map = build(&ix, ROOT, 2, 0.0);
        assert_eq!(names(&map), ["data"]);
        assert_eq!((map.other_size, map.other_count), (0, 2));
        // A folder of empty things still shows one of them.
        let raw = vec![entry(1, 1, ".", true, 0), entry(2, 1, "e1", true, 0), entry(3, 1, "e2", true, 0)];
        let ix = Index::build("D:", "test", 0, raw, 1);
        let map = build(&ix, ROOT, 2, 0.01);
        assert_eq!(names(&map), ["e1"]);
        assert_eq!((map.other_size, map.other_count), (0, 1));
    }

    #[test]
    fn sees_live_changes() {
        let mut ix = sample();
        let meta = Meta { flags: 0, size: 5000, alloc: 5000, modified: 0 };
        ix.insert(ROOT, "huge.iso", meta).unwrap();
        let map = build(&ix, ROOT, 1, 0.05);
        assert_eq!(map.size, 6000);
        assert_eq!(names(&map), ["huge.iso", "Users", "GitHub"]);
        assert_eq!(child(&map, "huge.iso").category, "archive");
        // 5% of 6000 is 300: GitHub (300) stays, the tiny files (100) do not.
        assert_eq!((map.other_size, map.other_count), (100, 3));
        check_tree(&ix, &map);
    }

    #[test]
    fn a_file_or_depth_zero_has_no_children() {
        let ix = sample();
        let file = ix.find(r"C:\tiny1.txt").unwrap();
        let map = build(&ix, file, 4, 0.0);
        assert_eq!(map.path, r"C:\tiny1.txt");
        assert!(!map.is_dir);
        assert!(map.children.is_empty());
        let map = build(&ix, ROOT, 0, 0.0);
        assert!(map.children.is_empty());
        assert_eq!(map.other_count, 0);
    }

    #[test]
    fn odd_ratios() {
        let ix = sample();
        // NaN, infinity and negatives count as 0; a ratio above 1 lists only the biggest.
        assert_eq!(names(&build(&ix, ROOT, 1, f64::NAN)).len(), 5);
        assert_eq!(names(&build(&ix, ROOT, 1, -1.0)).len(), 5);
        assert_eq!(names(&build(&ix, ROOT, 1, f64::INFINITY)).len(), 5);
        assert_eq!(names(&build(&ix, ROOT, 1, 2.0)), ["Users"]);
    }

    #[test]
    fn node_limit_opens_the_biggest_folders_first() {
        // 64 folders of 64 folders of 4 files: 16,448 entries under the root.
        let mut raw = vec![entry(1, 1, ".", true, 0)];
        let mut id = 2;
        for a in 0..64u64 {
            let top = id;
            raw.push(entry(top, 1, &format!("a{a:02}"), true, 0));
            id += 1;
            for b in 0..64u64 {
                let mid = id;
                raw.push(entry(mid, top, &format!("b{b:02}"), true, 0));
                id += 1;
                for f in 0..4u64 {
                    // Bigger toward a00, so the opening order is known.
                    raw.push(entry(id, mid, &format!("f{f}"), false, 1 + (64 - a) * 100 + f));
                    id += 1;
                }
            }
        }
        let ix = Index::build("D:", "test", 0, raw, 1);
        let map = build(&ix, ROOT, 8, 0.0);
        let count = check_tree(&ix, &map);
        assert_eq!(count, MAX_NODES);
        // The biggest top folder is opened all the way, the smallest is not.
        assert!(child(&map, "a00").children.iter().all(|b| b.children.len() == 4));
        assert!(child(&map, "a63").children.iter().all(|b| b.children.is_empty()));
    }

    /// The map of a 3-million-entry index opens only what it lists:
    /// `cargo test --release mapview::tests::big_index -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn big_index() {
        // 300 folders of 100 folders of 100 files, plus one folder of 100,000 files.
        let mut raw = vec![entry(1, 1, ".", true, 0)];
        let mut id = 2;
        for a in 0..300u64 {
            let top = id;
            raw.push(entry(top, 1, &format!("a{a}"), true, 0));
            id += 1;
            for b in 0..100u64 {
                let mid = id;
                raw.push(entry(mid, top, &format!("b{b}"), true, 0));
                id += 1;
                for f in 0..100u64 {
                    raw.push(entry(id, mid, &format!("f{f}"), false, (a * 7 + b * 3 + f) % 1000 + 1));
                    id += 1;
                }
            }
        }
        let wide = id;
        raw.push(entry(wide, 1, "wide", true, 0));
        id += 1;
        for f in 0..100_000u64 {
            raw.push(entry(id, wide, &format!("w{f}"), false, 5000 + f % 97));
            id += 1;
        }
        let ix = Index::build("D:", "test", 0, raw, 1);
        for (depth, ratio) in [(4, 0.005), (8, 0.0001), (8, 0.0)] {
            let start = std::time::Instant::now();
            let map = build(&ix, ROOT, depth, ratio);
            let took = start.elapsed();
            let count = check_tree(&ix, &map);
            println!("{} entries, depth {depth}, ratio {ratio}: {count} map nodes in {took:?}", ix.nodes.len());
        }
    }
}
