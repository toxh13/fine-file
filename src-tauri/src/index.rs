//! In-memory file index for one drive: a flat node table plus a child table.
//!
//! Every scanner (MFT reader, directory walker) produces `RawEntry` values and
//! hands them to `Index::build`, which links parents, sums folder sizes and
//! builds the lookup tables used for browsing and search. Live changes
//! (`insert`, `update`, `move_to`, `remove`) then keep it in step with the disk.

use rayon::prelude::*;
use serde::Serialize;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

pub const FLAG_DIR: u32 = 1;
pub const FLAG_HIDDEN: u32 = 2;
pub const FLAG_SYSTEM: u32 = 4;
pub const FLAG_REPARSE: u32 = 8;
pub const FLAG_DELETED: u32 = 16;
/// An extra hard link of a file: listed and searchable with its own size, but
/// left out of folder totals so the file's bytes are counted only once.
pub const FLAG_HARDLINK: u32 = 32;
/// With FLAG_DIR and FLAG_REPARSE: a cloud-files placeholder folder
/// (OneDrive and the like). What is below it is on this drive and indexed
/// like any folder's contents. A reparse point without it is taken for a
/// junction, symlink or other link (ProjFS, WCI, DFS, ...), whose contents
/// are counted elsewhere or not at all.
pub const FLAG_NOT_LINK: u32 = 64;
/// The counted name of a file that has further hard links (FLAG_HARDLINK on
/// those). Removing it live takes the file's bytes out of the totals although
/// the other links still hold them (`linked_removed`).
pub const FLAG_LINKED: u32 = 128;

pub const ROOT: u32 = 0;
const NO_PARENT: u32 = u32::MAX;
/// Bytes per node in the cache file.
const NODE_BYTES: usize = 40;
/// A folder with at least this many children gets a name map the first time
/// a change looks something up in it (`locate`).
const MAP_FROM: usize = 256;
/// Name map entries kept at most, over all folders (about 20 bytes each).
const MAP_MAX: usize = 1 << 20;

/// Folded-name key → child, for one folder.
type NameMap = HashMap<u64, u32, BuildHasherDefault<Prehashed>>;

/// The keys of a `NameMap` are hashes already; they are used as they are.
#[derive(Default)]
struct Prehashed(u64);

impl Hasher for Prehashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ b as u64;
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

/// One entry as reported by a scanner. `id` and `parent` are scanner-local
/// keys (MFT record numbers or walk ids); the root has `parent == id`.
pub struct RawEntry {
    pub id: u64,
    pub parent: u64,
    pub name: String,
    pub flags: u32,
    pub size: u64,
    pub alloc: u64,
    pub modified: i64,
}

#[derive(Clone, Copy)]
pub struct Node {
    pub parent: u32,
    pub name_start: u32,
    pub name_len: u32,
    pub flags: u32,
    /// Files: logical size. Folders: sum of everything below.
    pub size: u64,
    pub alloc: u64,
    /// FILETIME (100 ns ticks since 1601-01-01 UTC).
    pub modified: i64,
    /// Folders: number of files below. Files: 0.
    pub files: u32,
}

/// What a live change knows about one entry (see `Index::insert`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// FLAG_DIR, FLAG_HIDDEN, FLAG_SYSTEM, FLAG_REPARSE, FLAG_NOT_LINK and,
    /// for an extra hard link, FLAG_HARDLINK. Other bits are ignored.
    pub flags: u32,
    pub size: u64,
    pub alloc: u64,
    pub modified: i64,
}

/// What one node adds to every folder above it.
#[derive(Clone, Copy, Default)]
struct Weight {
    size: u64,
    alloc: u64,
    files: u32,
}

pub struct Index {
    pub drive: String,
    pub method: String,
    pub scanned_at: i64,
    pub scan_ms: u64,
    pub nodes: Vec<Node>,
    names: String,
    lower: String,
    child_start: Vec<u32>,
    children: Vec<u32>,
    /// Children linked after the child table was built (live inserts and
    /// moves), per parent. The built table can still list a node that has
    /// moved away since, so `children_of` checks each node's parent.
    extra: HashMap<u32, Vec<u32>>,
    /// Name lookups for big folders, so a burst of changes in a folder of
    /// 80,000 entries does not compare every name for each change. Some: every
    /// live child is in the map. None: two live children have one key (the
    /// same name in a case-sensitive folder, or a hash clash), so that folder
    /// is searched instead. Built by `locate`, kept in step by every change,
    /// dropped by `finish`.
    by_name: HashMap<u32, Option<NameMap>>,
    /// Entries in all of `by_name`.
    mapped: usize,
    /// Counted hard links (FLAG_LINKED) removed so far.
    linked_removed: u64,
    /// Removed nodes, and name bytes no node uses any more. `compact` drops both.
    dead_nodes: usize,
    dead_names: usize,
}

#[derive(Serialize, Clone)]
pub struct EntryInfo {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub alloc: u64,
    pub modified: i64,
    pub files: u32,
    pub hidden: bool,
    pub system: bool,
    pub reparse: bool,
}

impl Index {
    pub fn build(drive: &str, method: &str, scan_ms: u64, mut raw: Vec<RawEntry>, root_id: u64) -> Index {
        // Map scanner ids to dense node indices; the root always becomes node 0.
        raw.par_sort_unstable_by_key(|e| e.id);
        let root_pos = raw.iter().position(|e| e.id == root_id);
        let mut order: Vec<usize> = Vec::with_capacity(raw.len() + 1);
        let mut nodes: Vec<Node> = Vec::with_capacity(raw.len() + 1);
        let mut names = String::new();

        nodes.push(Node {
            parent: NO_PARENT,
            name_start: 0,
            name_len: 0,
            flags: FLAG_DIR,
            size: 0,
            alloc: 0,
            modified: root_pos.map(|p| raw[p].modified).unwrap_or(0),
            files: 0,
        });
        names.push_str(drive);
        nodes[0].name_len = drive.len() as u32;

        for (i, e) in raw.iter().enumerate() {
            if Some(i) == root_pos {
                continue;
            }
            order.push(i);
            let dir = e.flags & FLAG_DIR != 0;
            // Folders cannot be hard-linked.
            let mut flags = if dir { e.flags & !(FLAG_HARDLINK | FLAG_LINKED) } else { e.flags };
            // Name offsets are u32. Past 4 GiB of names an entry keeps its slot
            // (so node indices still line up) but is hidden instead of wrapping.
            let (start, len) = match name_slot(names.len(), e.name.len()) {
                Some(slot) => {
                    names.push_str(&e.name);
                    slot
                }
                None => {
                    flags |= FLAG_DELETED;
                    (0, 0)
                }
            };
            nodes.push(Node {
                parent: NO_PARENT,
                name_start: start,
                name_len: len,
                flags,
                size: if dir { 0 } else { e.size },
                alloc: if dir { 0 } else { e.alloc },
                modified: e.modified,
                files: 0,
            });
        }

        // Resolve parents with a binary search over the sorted scanner ids.
        let ids: Vec<u64> = raw.iter().map(|e| e.id).collect();
        let lookup = |id: u64| -> Option<u32> {
            if id == root_id {
                return Some(ROOT);
            }
            let pos = ids.binary_search(&id).ok()?;
            if Some(pos) == root_pos {
                return Some(ROOT);
            }
            // node index = position in `order` + 1
            let before_root = root_pos.map(|r| (r < pos) as usize).unwrap_or(0);
            Some((pos - before_root + 1) as u32)
        };
        // An entry whose parent is missing, is itself, or is a file has no real
        // path (the MFT changed while it was read). Showing it under the root
        // would make up a path that may name a different file, so it is parked
        // under the root but hidden; finish() hides everything below it too.
        for (n, &i) in order.iter().enumerate() {
            let node = n + 1;
            match lookup(raw[i].parent) {
                Some(p) if p as usize != node => nodes[node].parent = p,
                _ => orphan(&mut nodes[node]),
            }
        }
        for node in 1..nodes.len() {
            let p = nodes[node].parent as usize;
            if nodes[p].flags & FLAG_DIR == 0 {
                orphan(&mut nodes[node]);
            }
        }
        drop(raw);

        let mut index = Index {
            drive: drive.to_string(),
            method: method.to_string(),
            scanned_at: now_filetime(),
            scan_ms,
            nodes,
            names,
            lower: String::new(),
            child_start: Vec::new(),
            children: Vec::new(),
            extra: HashMap::new(),
            by_name: HashMap::new(),
            mapped: 0,
            linked_removed: 0,
            dead_nodes: 0,
            dead_names: 0,
        };
        index.finish();
        index
    }

    /// Builds child tables, folder totals and the lowercase search copy from
    /// the parent links alone. Returns how many nodes had to be cut off a
    /// parent cycle.
    fn finish(&mut self) -> usize {
        self.extra.clear();
        self.by_name.clear();
        self.mapped = 0;
        let cut = self.break_cycles();
        let n = self.nodes.len();
        let mut counts = vec![0u32; n + 1];
        for i in 1..n {
            counts[self.nodes[i].parent as usize + 1] += 1;
        }
        for i in 0..n {
            counts[i + 1] += counts[i];
        }
        let mut fill = counts.clone();
        let mut children = vec![0u32; n.saturating_sub(1)];
        for i in 1..n {
            let p = self.nodes[i].parent as usize;
            children[fill[p] as usize] = i as u32;
            fill[p] += 1;
        }
        self.child_start = counts;
        self.children = children;

        // Breadth-first from the root: every parent comes before its children.
        // After break_cycles() every node is reachable, so this lists them all.
        let mut order: Vec<u32> = Vec::with_capacity(n);
        order.push(ROOT);
        let mut head = 0;
        while head < order.len() {
            let c = order[head];
            head += 1;
            order.extend_from_slice(self.built_children(c));
        }

        // A removed or hidden folder hides everything below it.
        for &i in &order[1..] {
            let p = self.nodes[i as usize].parent as usize;
            if self.nodes[p].flags & FLAG_DELETED != 0 {
                self.nodes[i as usize].flags |= FLAG_DELETED;
            }
        }

        // Folder totals: walk that order backwards so children finish before parents.
        for node in &mut self.nodes {
            if node.flags & FLAG_DIR != 0 {
                node.size = 0;
                node.alloc = 0;
                node.files = 0;
            }
        }
        for &i in order[1..].iter().rev() {
            let node = self.nodes[i as usize];
            if node.flags & (FLAG_DELETED | FLAG_HARDLINK) != 0 {
                continue;
            }
            let files = if node.flags & FLAG_DIR != 0 { node.files } else { 1 };
            let p = &mut self.nodes[node.parent as usize];
            p.size = p.size.saturating_add(node.size);
            p.alloc = p.alloc.saturating_add(node.alloc);
            p.files += files;
        }

        // Folded per character, so every name keeps its byte offsets.
        self.lower = fold(&self.names);

        let live = self.nodes.iter().filter(|n| n.flags & FLAG_DELETED == 0);
        let live_names: usize = live.map(|n| n.name_len as usize).sum();
        self.dead_nodes = self.nodes.iter().filter(|n| n.flags & FLAG_DELETED != 0).count();
        self.dead_names = self.names.len().saturating_sub(live_names);
        cut
    }

    /// Makes every parent chain end at the root in O(n). A node that closes a
    /// parent cycle (or points outside the table) is detached to the root and
    /// hidden like an orphan. Returns how many nodes were cut.
    fn break_cycles(&mut self) -> usize {
        const UNSEEN: u8 = 0;
        const ON_WALK: u8 = 1;
        const DONE: u8 = 2;
        let n = self.nodes.len();
        let mut state = vec![UNSEEN; n];
        state[ROOT as usize] = DONE;
        let mut walk = Vec::new();
        let mut cut = 0;
        for start in 1..n {
            let mut cur = start;
            while state[cur] == UNSEEN {
                state[cur] = ON_WALK;
                walk.push(cur);
                let p = self.nodes[cur].parent as usize;
                if p >= n || state[p] == ON_WALK {
                    orphan(&mut self.nodes[cur]);
                    cut += 1;
                    break;
                }
                cur = p;
            }
            for &i in &walk {
                state[i] = DONE;
            }
            walk.clear();
        }
        cut
    }

    pub fn name(&self, i: u32) -> &str {
        let n = &self.nodes[i as usize];
        let s = n.name_start as usize;
        &self.names[s..s + n.name_len as usize]
    }

    fn lower_name(&self, i: u32) -> &str {
        let n = &self.nodes[i as usize];
        let s = n.name_start as usize;
        &self.lower[s..s + n.name_len as usize]
    }

    /// Every node whose parent is `i`, removed ones included.
    pub fn children_of(&self, i: u32) -> impl Iterator<Item = u32> + '_ {
        self.built_children(i)
            .iter()
            .chain(self.extra.get(&i).into_iter().flatten())
            .copied()
            .filter(move |&c| self.nodes[c as usize].parent == i)
    }

    /// The children `finish` found, in ascending order. Nodes added since
    /// have no slot here.
    fn built_children(&self, i: u32) -> &[u32] {
        match (self.child_start.get(i as usize), self.child_start.get(i as usize + 1)) {
            (Some(&s), Some(&e)) => &self.children[s as usize..e as usize],
            _ => &[],
        }
    }

    /// The live child of `parent` with this name, compared case-insensitively.
    /// A case-sensitive folder can hold several ("Build" and "build"): then
    /// the one with exactly this name, else the first of them.
    pub fn child_named(&self, parent: u32, name: &str) -> Option<u32> {
        if let Some(Some(map)) = self.by_name.get(&parent) {
            let &c = map.get(&name_key(name))?;
            // A mapped folder has one live child per key, so this is the only match.
            if self.is_live(c) && self.nodes[c as usize].parent == parent && self.named(c, name) {
                return Some(c);
            }
            // Another name with the same key: look the slow way to be sure.
        }
        let mut first = None;
        for c in self.children_of(parent).filter(|&c| self.is_live(c) && self.named(c, name)) {
            if self.name(c) == name {
                return Some(c);
            }
            first.get_or_insert(c);
        }
        first
    }

    /// `child_named` for code about to change the index: a big folder gets
    /// a name map first, so the many lookups a burst of changes makes in it
    /// stay quick.
    pub fn locate_child(&mut self, parent: u32, name: &str) -> Option<u32> {
        self.map_folder(parent);
        self.child_named(parent, name)
    }

    /// Whether node `i` has this name, compared case-insensitively.
    fn named(&self, i: u32, name: &str) -> bool {
        self.lower_name(i).chars().eq(name.chars().map(fold_char))
    }

    /// Node `i`'s name map key.
    fn key(&self, i: u32) -> u64 {
        key_of(self.lower_name(i).chars())
    }

    /// Gives a big live folder its name map, unless it has one (or cannot
    /// have one), or the maps already hold all they may.
    fn map_folder(&mut self, parent: u32) {
        if self.by_name.contains_key(&parent) {
            return;
        }
        // Counts nodes that moved away too, so it is an upper bound.
        let most = self.built_children(parent).len() + self.extra.get(&parent).map_or(0, Vec::len);
        if most < MAP_FROM || self.mapped + most > MAP_MAX || !self.is_live_dir(parent) {
            return;
        }
        let mut map = NameMap::with_capacity_and_hasher(most, Default::default());
        let mut clash = false;
        for c in self.children_of(parent).filter(|&c| self.is_live(c)) {
            if map.insert(self.key(c), c).is_some() {
                clash = true;
                break;
            }
        }
        if clash {
            self.by_name.insert(parent, None);
        } else {
            self.mapped += map.len();
            self.by_name.insert(parent, Some(map));
        }
    }

    /// Enters live child `c` in its folder's name map, if the folder has one.
    fn map_add(&mut self, parent: u32, c: u32) {
        let key = self.key(c);
        let Some(slot) = self.by_name.get_mut(&parent) else { return };
        let Some(map) = slot.as_mut() else { return };
        let before = map.len();
        let fits = match map.entry(key) {
            Entry::Vacant(v) => {
                v.insert(c);
                true
            }
            Entry::Occupied(o) => *o.get() == c,
        };
        if fits {
            self.mapped += map.len() - before;
        } else {
            // A second live child with this key: the map could no longer
            // answer for both, so the folder is searched from now on.
            self.mapped -= map.len();
            *slot = None;
        }
    }

    /// Takes child `c`, under the name it has now, out of its folder's name map.
    fn map_remove(&mut self, parent: u32, c: u32) {
        let key = self.key(c);
        if let Some(Some(map)) = self.by_name.get_mut(&parent)
            && map.get(&key) == Some(&c)
        {
            map.remove(&key);
            self.mapped -= 1;
        }
    }

    pub fn path_of(&self, i: u32) -> String {
        if i == ROOT {
            return format!("{}\\", self.drive);
        }
        let mut parts = Vec::new();
        let mut cur = i;
        // finish() leaves no parent cycles; the length cap is only a safety net.
        while cur != ROOT && cur != NO_PARENT && parts.len() < self.nodes.len() {
            parts.push(self.name(cur));
            cur = self.nodes[cur as usize].parent;
        }
        parts.reverse();
        format!("{}\\{}", self.drive, parts.join("\\"))
    }

    /// Finds the node for an absolute path such as `C:\Users\me`.
    pub fn find(&self, path: &str) -> Option<u32> {
        let mut cur = ROOT;
        for part in parts(&self.drive, path)? {
            cur = self.child_named(cur, part)?;
        }
        Some(cur)
    }

    /// `find` for code about to change the index (see `locate_child`).
    pub fn locate(&mut self, path: &str) -> Option<u32> {
        let mut cur = ROOT;
        for part in parts(&self.drive, path)? {
            cur = self.locate_child(cur, part)?;
        }
        Some(cur)
    }

    fn is_live(&self, i: u32) -> bool {
        self.nodes[i as usize].flags & FLAG_DELETED == 0
    }

    fn is_live_dir(&self, i: u32) -> bool {
        self.nodes.get(i as usize).is_some_and(|n| n.flags & (FLAG_DIR | FLAG_DELETED) == FLAG_DIR)
    }

    pub fn info(&self, i: u32) -> EntryInfo {
        let n = &self.nodes[i as usize];
        EntryInfo {
            name: self.name(i).to_string(),
            path: self.path_of(i),
            is_dir: n.flags & FLAG_DIR != 0,
            size: n.size,
            alloc: n.alloc,
            modified: n.modified,
            files: n.files,
            hidden: n.flags & FLAG_HIDDEN != 0,
            system: n.flags & FLAG_SYSTEM != 0,
            reparse: n.flags & FLAG_REPARSE != 0,
        }
    }

    pub fn list(&self, i: u32) -> Vec<EntryInfo> {
        self.children_of(i).filter(|&c| self.is_live(c)).map(|c| self.info(c)).collect()
    }

    /// Case-insensitive search. Space-separated terms must all appear in the name;
    /// `*` and `?` act as wildcards within a term.
    pub fn search(&self, query: &str, under: Option<u32>, limit: usize) -> (usize, Vec<EntryInfo>) {
        let terms: Vec<Term> = query.split_whitespace().map(|t| Term::new(&fold(t))).collect();
        if terms.is_empty() {
            return (0, Vec::new());
        }
        let mut hits: Vec<u32> = (1..self.nodes.len() as u32)
            .into_par_iter()
            .filter(|&i| {
                self.is_live(i) && {
                    let name = self.lower_name(i);
                    terms.iter().all(|t| t.matches(name))
                }
            })
            .filter(|&i| under.is_none_or(|u| u == ROOT || self.is_under(i, u)))
            .collect();
        let total = hits.len();
        hits.par_sort_unstable_by_key(|&i| std::cmp::Reverse(self.nodes[i as usize].size));
        hits.truncate(limit);
        (total, hits.into_iter().map(|i| self.info(i)).collect())
    }

    fn is_under(&self, mut i: u32, ancestor: u32) -> bool {
        // finish() leaves no parent cycles; the step cap is only a safety net.
        for _ in 0..=self.nodes.len() {
            if i == ancestor {
                return true;
            }
            if i == NO_PARENT {
                return false;
            }
            i = self.nodes[i as usize].parent;
        }
        false
    }

    /// Marks a node as removed and takes its size out of every ancestor.
    pub fn remove(&mut self, i: u32) {
        if i == ROOT || !self.is_live(i) {
            return;
        }
        let gone = self.weight(i);
        let parent = self.nodes[i as usize].parent;
        self.map_remove(parent, i);
        let mut stack = vec![i];
        while let Some(cur) = stack.pop() {
            let n = &mut self.nodes[cur as usize];
            // Everything below a removed node is removed already.
            if n.flags & FLAG_DELETED != 0 {
                continue;
            }
            n.flags |= FLAG_DELETED;
            if n.flags & FLAG_LINKED != 0 {
                self.linked_removed += 1;
            }
            self.dead_nodes += 1;
            self.dead_names += n.name_len as usize;
            if let Some(Some(map)) = self.by_name.remove(&cur) {
                self.mapped -= map.len();
            }
            stack.extend(self.children_of(cur));
        }
        self.shift(parent, gone, Weight::default());
    }

    /// How many counted hard links (FLAG_LINKED) were removed so far. Their
    /// files may still be on disk through another link, whose bytes the
    /// totals then leave out.
    pub fn linked_removed(&self) -> u64 {
        self.linked_removed
    }

    /// Gives a node a new name. Returns false (keeping the old name) only when
    /// the name table would pass the 4 GiB that u32 offsets can address.
    pub fn rename(&mut self, i: u32, new_name: &str) -> bool {
        let parent = self.nodes[i as usize].parent;
        let live = self.is_live(i);
        if live {
            self.map_remove(parent, i);
        }
        let done = self.set_name(i, new_name);
        if live {
            self.map_add(parent, i);
        }
        done
    }

    /// `rename` without the name maps.
    fn set_name(&mut self, i: u32, new_name: &str) -> bool {
        let Some((start, len)) = name_slot(self.names.len(), new_name.len()) else {
            return false;
        };
        self.names.push_str(new_name);
        self.lower.push_str(&fold(new_name));
        let n = &mut self.nodes[i as usize];
        self.dead_names += n.name_len as usize;
        n.name_start = start;
        n.name_len = len;
        true
    }

    // ---- live changes -------------------------------------------------------

    /// Adds an entry to folder `parent`. A live child that already has the
    /// name is updated instead, or replaced when one of the two is a folder
    /// and the other is not. A new folder starts empty; what is inside it
    /// comes as further inserts (or use `insert_tree`). None when `parent`
    /// is not a live folder, the name cannot be a file name, or a table is full.
    pub fn insert(&mut self, parent: u32, name: &str, meta: Meta) -> Option<u32> {
        if !self.is_live_dir(parent) {
            return None;
        }
        if let Some(old) = self.locate_child(parent, name) {
            if (self.nodes[old as usize].flags ^ meta.flags) & FLAG_DIR == 0 {
                // The same entry; only the case of its name may differ.
                if self.name(old) != name {
                    self.rename(old, name);
                }
                self.update(old, meta);
                return Some(old);
            }
            self.remove(old);
        }
        self.push_node(parent, name, meta)
    }

    /// Inserts folder `name` into `parent` with everything below it, as a
    /// scanner reported it: `below` holds entries whose parent chains lead to
    /// `root_id`, the folder itself (an entry for it is ignored). A live
    /// child of that name is replaced. Entries hanging off a file or off an
    /// id that is not there are skipped.
    pub fn insert_tree(&mut self, parent: u32, name: &str, meta: Meta, mut below: Vec<RawEntry>, root_id: u64) -> Option<u32> {
        if !self.is_live_dir(parent) || meta.flags & FLAG_DIR == 0 {
            return None;
        }
        if let Some(old) = self.locate_child(parent, name) {
            self.remove(old);
        }
        let top = self.push_node(parent, name, meta)?;
        // Scanners give a folder its id before they list it, so in id order
        // every parent comes before its children.
        below.sort_unstable_by_key(|e| e.id);
        let mut placed: HashMap<u64, u32> = HashMap::with_capacity(below.len() + 1);
        placed.insert(root_id, top);
        for e in below {
            let Some(&p) = placed.get(&e.parent).filter(|&&p| e.id != root_id && self.is_live_dir(p)) else { continue };
            let meta = Meta { flags: e.flags, size: e.size, alloc: e.alloc, modified: e.modified };
            // Names inside one new folder are unique, so no lookup is needed.
            if let Some(n) = self.push_node(p, &e.name, meta) {
                placed.insert(e.id, n);
            }
        }
        Some(top)
    }

    /// Appends a node to `parent` without looking for one of the same name.
    fn push_node(&mut self, parent: u32, name: &str, meta: Meta) -> Option<u32> {
        // u32::MAX stands for "no parent", so it can never be a node.
        if name.is_empty() || name.contains(['\\', '/']) || self.nodes.len() >= NO_PARENT as usize {
            return None;
        }
        let (start, len) = name_slot(self.names.len(), name.len())?;
        self.names.push_str(name);
        self.lower.push_str(&fold(name));
        let dir = meta.flags & FLAG_DIR != 0;
        // Folders cannot be hard-linked.
        let keep = FLAG_DIR | FLAG_HIDDEN | FLAG_SYSTEM | FLAG_REPARSE | FLAG_NOT_LINK | if dir { 0 } else { FLAG_HARDLINK };
        let i = self.nodes.len() as u32;
        self.nodes.push(Node {
            parent,
            name_start: start,
            name_len: len,
            flags: meta.flags & keep,
            size: if dir { 0 } else { meta.size },
            alloc: if dir { 0 } else { meta.alloc },
            modified: meta.modified,
            files: 0,
        });
        self.extra.entry(parent).or_default().push(i);
        self.map_add(parent, i);
        self.shift(parent, Weight::default(), self.weight(i));
        Some(i)
    }

    /// Takes in an entry's new size, time and attributes; size changes reach
    /// every folder above it. A folder's own size stays the sum of what is
    /// below it. Returns whether anything changed.
    pub fn update(&mut self, i: u32, meta: Meta) -> bool {
        let old = self.nodes[i as usize];
        if old.flags & FLAG_DELETED != 0 {
            return false;
        }
        let attrs = FLAG_HIDDEN | FLAG_SYSTEM | FLAG_REPARSE | FLAG_NOT_LINK;
        let mut new = old;
        new.flags = (old.flags & !attrs) | (meta.flags & attrs);
        new.modified = meta.modified;
        if old.flags & FLAG_DIR == 0 {
            new.size = meta.size;
            new.alloc = meta.alloc;
        }
        if (new.flags, new.size, new.alloc, new.modified) == (old.flags, old.size, old.alloc, old.modified) {
            return false;
        }
        let before = self.weight(i);
        self.nodes[i as usize] = new;
        self.shift(old.parent, before, self.weight(i));
        true
    }

    /// Moves a node into folder `parent` as `name`, as a rename or move on
    /// disk does: its totals leave the folders above its old place and join
    /// those above the new one. A live entry already at the target is
    /// replaced. False, with nothing changed, when the node or the folder is
    /// gone, the folder lies inside the node, or the name table is full.
    pub fn move_to(&mut self, i: u32, parent: u32, name: &str) -> bool {
        let bad_name = name.is_empty() || name.contains(['\\', '/']);
        if i == ROOT || bad_name || !self.is_live(i) || !self.is_live_dir(parent) || self.is_under(parent, i) {
            return false;
        }
        let other = self.locate_child(parent, name).filter(|&o| o != i);
        let old = self.nodes[i as usize].parent;
        self.map_remove(old, i);
        if self.name(i) != name && !self.set_name(i, name) {
            self.map_add(old, i);
            return false;
        }
        if let Some(o) = other {
            self.remove(o);
        }
        if old != parent {
            let w = self.weight(i);
            self.shift(old, w, Weight::default());
            if let Some(list) = self.extra.get_mut(&old) {
                list.retain(|&c| c != i);
            }
            self.nodes[i as usize].parent = parent;
            // Back in the folder it was built under: its old slot counts again.
            if self.built_children(parent).binary_search(&i).is_err() {
                self.extra.entry(parent).or_default().push(i);
            }
            self.shift(parent, Weight::default(), w);
        }
        self.map_add(parent, i);
        true
    }

    /// Whether removed nodes and replaced names take up enough room to be
    /// worth the pause of `compact`.
    pub fn wants_compact(&self) -> bool {
        let nodes = self.dead_nodes > 100_000 && self.dead_nodes * 4 > self.nodes.len();
        let names = self.dead_names > 16 << 20 && self.dead_names * 4 > self.names.len();
        nodes || names
    }

    /// Drops removed nodes and name bytes nothing uses any more, and rebuilds
    /// the child table. Node numbers change, so none may be kept across it.
    pub fn compact(&mut self) {
        // Breadth-first over live nodes: parents get their new number first.
        let mut order = vec![ROOT];
        let mut head = 0;
        while head < order.len() {
            let c = order[head];
            head += 1;
            order.extend(self.children_of(c).filter(|&k| self.is_live(k)));
        }
        let mut renumber = vec![NO_PARENT; self.nodes.len()];
        for (new, &old) in order.iter().enumerate() {
            renumber[old as usize] = new as u32;
        }
        // Live names took less room than all names did, so offsets still fit in u32.
        let mut names = String::with_capacity(self.names.len().saturating_sub(self.dead_names));
        let nodes = order
            .iter()
            .map(|&old| {
                let n = self.nodes[old as usize];
                let name_start = names.len() as u32;
                names.push_str(self.name(old));
                let parent = if old == ROOT { NO_PARENT } else { renumber[n.parent as usize] };
                Node { parent, name_start, ..n }
            })
            .collect();
        self.nodes = nodes;
        self.names = names;
        self.finish();
    }

    /// What node `i` adds to the totals above it. Removed nodes and extra
    /// hard links add nothing.
    fn weight(&self, i: u32) -> Weight {
        let n = &self.nodes[i as usize];
        if n.flags & (FLAG_DELETED | FLAG_HARDLINK) != 0 {
            return Weight::default();
        }
        let files = if n.flags & FLAG_DIR != 0 { n.files } else { 1 };
        Weight { size: n.size, alloc: n.alloc, files }
    }

    /// Replaces `take` with `give` in folder `from` and every folder above it.
    fn shift(&mut self, from: u32, take: Weight, give: Weight) {
        let mut p = from;
        // Parent chains have no cycles; the step cap is only a safety net.
        for _ in 0..self.nodes.len() {
            if p == NO_PARENT {
                break;
            }
            let n = &mut self.nodes[p as usize];
            n.size = n.size.saturating_sub(take.size).saturating_add(give.size);
            n.alloc = n.alloc.saturating_sub(take.alloc).saturating_add(give.alloc);
            n.files = n.files.saturating_sub(take.files).saturating_add(give.files);
            p = n.parent;
        }
    }

    // ---- cache file ---------------------------------------------------------

    /// The cache file's bytes (what `save_to` writes), made in memory: the
    /// caller can let go of the index's lock before the disk is touched.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.cache_len());
        // Writing to memory cannot fail.
        let _ = self.write_body(&mut out);
        out
    }

    /// Bytes in the cache file of this index.
    pub fn cache_len(&self) -> usize {
        let header = MAGIC.len() + 8 + self.drive.len() + 8 + self.method.len() + 8 + 8 + 8;
        header + self.nodes.len() * NODE_BYTES + 8 + self.names.len()
    }

    pub fn load(path: &Path) -> io::Result<Index> {
        Index::load_from(&mut File::open(path)?)
    }

    /// Writes the cache format to any stream (a file, or the scan helper's pipe).
    pub fn save_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, w);
        self.write_body(&mut w)?;
        w.flush()
    }

    fn write_body(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(MAGIC)?;
        write_str(w, &self.drive)?;
        write_str(w, &self.method)?;
        w.write_all(&self.scanned_at.to_le_bytes())?;
        w.write_all(&self.scan_ms.to_le_bytes())?;
        w.write_all(&(self.nodes.len() as u64).to_le_bytes())?;
        let mut b = [0u8; NODE_BYTES];
        for n in &self.nodes {
            b[0..4].copy_from_slice(&n.parent.to_le_bytes());
            b[4..8].copy_from_slice(&n.name_start.to_le_bytes());
            b[8..12].copy_from_slice(&n.name_len.to_le_bytes());
            b[12..16].copy_from_slice(&n.flags.to_le_bytes());
            b[16..24].copy_from_slice(&n.size.to_le_bytes());
            b[24..32].copy_from_slice(&n.alloc.to_le_bytes());
            b[32..40].copy_from_slice(&n.modified.to_le_bytes());
            w.write_all(&b)?;
        }
        write_str(w, &self.names)
    }

    /// Reads what `save_to` wrote. Every count, offset and parent is checked, so
    /// a damaged stream gives `InvalidData`/`UnexpectedEof`, never a panic.
    /// Reads are buffered, so it may consume bytes past the end of the index.
    pub fn load_from(r: &mut impl Read) -> io::Result<Index> {
        let mut r = BufReader::with_capacity(1 << 20, r);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid("unknown index format"));
        }
        let drive = read_str(&mut r, 256)?;
        let method = read_str(&mut r, 256)?;
        let scanned_at = read_i64(&mut r)?;
        let scan_ms = read_i64(&mut r)? as u64;
        // Node indices are u32, and u32::MAX means "no parent".
        let count = read_i64(&mut r)? as u64;
        if count == 0 || count >= NO_PARENT as u64 {
            return Err(invalid("bad node count"));
        }
        // Grow with what actually arrives, so a bogus count cannot reserve gigabytes.
        let mut nodes = Vec::with_capacity((count as usize).min(1 << 20));
        let mut buf = [0u8; NODE_BYTES];
        for _ in 0..count {
            r.read_exact(&mut buf)?;
            let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
            let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
            nodes.push(Node {
                parent: u32_at(0),
                name_start: u32_at(4),
                name_len: u32_at(8),
                flags: u32_at(12),
                size: u64_at(16),
                alloc: u64_at(24),
                modified: u64_at(32) as i64,
                files: 0,
            });
        }
        let names = read_str(&mut r, u32::MAX as usize)?;
        check_nodes(&nodes, &names)?;
        let mut index = Index {
            drive,
            method,
            scanned_at,
            scan_ms,
            nodes,
            names,
            lower: String::new(),
            child_start: Vec::new(),
            children: Vec::new(),
            extra: HashMap::new(),
            by_name: HashMap::new(),
            mapped: 0,
            linked_removed: 0,
            dead_nodes: 0,
            dead_names: 0,
        };
        // build() never saves a cycle, so one here means the file is damaged.
        if index.finish() > 0 {
            return Err(invalid("parent cycle in index"));
        }
        Ok(index)
    }
}

const MAGIC: &[u8; 8] = b"FFIDX\x00\x00\x01";

/// Writes cache bytes (`Index::to_bytes`) to `path` atomically: temp file,
/// flush to disk, rename.
pub fn write_cache(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A unique temp name, so overlapping saves of one drive never share a file.
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}-{seq}.tmp", std::process::id()));
    let written = File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    let result = written.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    // A save cut off by the process exiting leaves its temp file behind;
    // `scan::sweep_temp_files` clears those at the next start.
    result
}

/// The names below the drive in an absolute path (`C:\Users\me` → `Users`,
/// `me`), or None when the path is on another drive.
fn parts<'p>(drive: &str, path: &'p str) -> Option<impl Iterator<Item = &'p str> + use<'p>> {
    let path = path.trim_end_matches(['\\', '/']);
    let rest = path.get(drive.len()..)?;
    if !path[..drive.len()].eq_ignore_ascii_case(drive) {
        return None;
    }
    Some(rest.split(['\\', '/']).filter(|p| !p.is_empty()))
}

/// A name's `NameMap` key: a hash of its folded characters.
fn name_key(name: &str) -> u64 {
    key_of(name.chars().map(fold_char))
}

fn key_of(folded: impl Iterator<Item = char>) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    for c in folded {
        h.write_u32(c as u32);
    }
    h.finish()
}

/// Parks a node with no trustworthy parent under the root, hidden.
fn orphan(node: &mut Node) {
    node.parent = ROOT;
    node.flags |= FLAG_DELETED;
}

/// Offset and length for a name appended at `at`, if both fit in u32.
fn name_slot(at: usize, len: usize) -> Option<(u32, u32)> {
    let end = at.checked_add(len)?;
    u32::try_from(end).ok()?;
    Some((at as u32, len as u32))
}

/// Checks a loaded node table: the root has no parent, every other parent is
/// an existing folder, and every name range lies on char boundaries in `names`.
fn check_nodes(nodes: &[Node], names: &str) -> io::Result<()> {
    for (i, n) in nodes.iter().enumerate() {
        let start = n.name_start as usize;
        let end = start.checked_add(n.name_len as usize);
        if !end.is_some_and(|e| names.is_char_boundary(start) && names.is_char_boundary(e)) {
            return Err(invalid("name out of range"));
        }
        let p = n.parent as usize;
        let parent_ok = if i == ROOT as usize {
            n.parent == NO_PARENT
        } else {
            p < nodes.len() && p != i && nodes[p].flags & FLAG_DIR != 0
        };
        if !parent_ok {
            return Err(invalid("bad parent"));
        }
    }
    Ok(())
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    w.write_all(&(s.len() as u64).to_le_bytes())?;
    w.write_all(s.as_bytes())
}

fn read_i64(r: &mut impl Read) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

fn read_str(r: &mut impl Read, max: usize) -> io::Result<String> {
    let len = read_i64(r)? as u64;
    if len > max as u64 {
        return Err(invalid("string too long"));
    }
    // `take` lets the buffer grow only with bytes that really arrive.
    let mut b = Vec::with_capacity((len as usize).min(1 << 20));
    r.by_ref().take(len).read_to_end(&mut b)?;
    if b.len() as u64 != len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    String::from_utf8(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Lowercases one character at a time, keeping each one's UTF-8 length, so
/// offsets into `names` stay valid for the lowercase copy. No context rules
/// apply (Σ is always σ), so a name folds the same alone or in the joined buffer.
fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(fold_char(c));
    }
    out
}

fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(one), None) if one.len_utf8() == c.len_utf8() => one,
        // İ → i̇ (two chars), Ω (ohm) → ω (shorter): keep these as they are.
        _ => c,
    }
}

pub fn now_filetime() -> i64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64 / 100)
        .unwrap_or(0);
    unix + 116_444_736_000_000_000
}

/// A search term with optional `*` / `?` wildcards.
struct Term {
    raw: String,
    wild: bool,
}

impl Term {
    fn new(t: &str) -> Term {
        Term { raw: t.to_string(), wild: t.contains(['*', '?']) }
    }

    fn matches(&self, name: &str) -> bool {
        if !self.wild {
            return name.contains(&self.raw);
        }
        // Wildcard terms match the whole name, like Explorer and Everything.
        wildcard(self.raw.as_bytes(), name.as_bytes())
    }
}

/// Glob match over UTF-8 bytes: `?` takes one whole character and `*`
/// backtracks by whole characters, so positions in `s` stay on char boundaries.
fn wildcard(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == b'?' {
            pi += 1;
            si += utf8_len(s[si]);
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += utf8_len(s[mark]);
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Byte length of the UTF-8 character that starts with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn sample() -> Index {
        let raw = vec![
            entry(5, 5, ".", true, 0),
            entry(10, 5, "Users", true, 0),
            entry(11, 10, "me", true, 0),
            entry(12, 11, "Video.MP4", false, 300),
            entry(13, 11, "notes.txt", false, 20),
            entry(14, 5, "pagefile.sys", false, 1000),
            entry(15, 99, "orphan.bin", false, 7),
        ];
        Index::build("C:", "test", 0, raw, 5)
    }

    fn bytes(ix: &Index) -> Vec<u8> {
        let mut out = Vec::new();
        ix.save_to(&mut out).unwrap();
        out
    }

    fn file(size: u64) -> Meta {
        Meta { flags: 0, size, alloc: size, modified: 7 }
    }

    fn folder() -> Meta {
        Meta { flags: FLAG_DIR, size: 0, alloc: 0, modified: 7 }
    }

    /// (size, alloc, files) of the node at `path`.
    fn totals(ix: &Index, path: &str) -> (u64, u64, u32) {
        let n = ix.nodes[ix.find(path).unwrap() as usize];
        (n.size, n.alloc, n.files)
    }

    /// Every live folder's totals equal a fresh sum over its children, no
    /// node is listed twice, and the lowercase copy lines up with the names.
    fn assert_consistent(ix: &Index) {
        assert_eq!(ix.lower.len(), ix.names.len());
        for i in 0..ix.nodes.len() as u32 {
            let kids: Vec<u32> = ix.children_of(i).collect();
            let unique: std::collections::HashSet<u32> = kids.iter().copied().collect();
            assert_eq!(unique.len(), kids.len(), "{}", ix.path_of(i));
            let n = ix.nodes[i as usize];
            if n.flags & (FLAG_DIR | FLAG_DELETED) != FLAG_DIR {
                continue;
            }
            let sum = kids.iter().map(|&c| ix.weight(c)).fold((0, 0, 0), |s, w| (s.0 + w.size, s.1 + w.alloc, s.2 + w.files));
            assert_eq!((n.size, n.alloc, n.files), sum, "{}", ix.path_of(i));
        }
    }

    #[test]
    fn sums_folder_sizes() {
        let ix = sample();
        assert_eq!(ix.nodes[ROOT as usize].size, 1320);
        let me = ix.find("C:\\users\\ME").unwrap();
        assert_eq!(ix.nodes[me as usize].size, 320);
        assert_eq!(ix.nodes[me as usize].files, 2);
        assert_eq!(ix.path_of(me), "C:\\Users\\me");
    }

    #[test]
    fn search_with_terms_and_wildcards() {
        let ix = sample();
        assert_eq!(ix.search("video", None, 10).0, 1);
        assert_eq!(ix.search("*.mp4", None, 10).0, 1);
        assert_eq!(ix.search("*.mp", None, 10).0, 0);
        assert_eq!(ix.search("no tes", None, 10).0, 1);
        let users = ix.find("C:\\Users").unwrap();
        assert_eq!(ix.search("s", Some(users), 10).0, 2);
    }

    #[test]
    fn remove_updates_ancestors() {
        let mut ix = sample();
        let video = ix.find("C:\\Users\\me\\Video.MP4").unwrap();
        ix.remove(video);
        assert_eq!(ix.nodes[ROOT as usize].size, 1020);
        assert!(ix.find("C:\\Users\\me\\Video.MP4").is_none());
    }

    #[test]
    fn save_and_load_round_trip() {
        let ix = sample();
        let path = std::env::temp_dir().join(format!("ff-test-{}.idx", std::process::id()));
        let bytes = ix.to_bytes();
        assert_eq!(bytes.len(), ix.cache_len());
        assert_eq!(bytes, self::bytes(&ix));
        write_cache(&path, &bytes).unwrap();
        let back = Index::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(back.nodes.len(), ix.nodes.len());
        assert_eq!(back.nodes[ROOT as usize].size, 1320);
        assert!(back.find("C:\\Users\\me\\notes.txt").is_some());
        assert_eq!(back.search("orphan", None, 10).0, 0);
    }

    #[test]
    fn orphans_are_hidden_with_their_subtree() {
        let raw = vec![
            entry(5, 5, ".", true, 0),
            entry(10, 5, "real", true, 0),
            entry(11, 10, "kept.txt", false, 5),
            // Parent record 99 was never read: no real path for this folder.
            entry(20, 99, "lost", true, 0),
            entry(21, 20, "inside.txt", false, 100),
            // Hangs off a file.
            entry(30, 11, "under-file.txt", false, 50),
            // Its own parent.
            entry(40, 40, "self", true, 0),
        ];
        let ix = Index::build("D:", "test", 0, raw, 5);
        assert_eq!(ix.nodes[ROOT as usize].size, 5);
        assert_eq!(ix.nodes[ROOT as usize].files, 1);
        let names: Vec<String> = ix.list(ROOT).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["real"]);
        for q in ["lost", "inside", "under-file", "self"] {
            assert_eq!(ix.search(q, None, 10).0, 0, "{q}");
        }
        assert!(ix.find("D:\\lost").is_none());
        assert!(ix.find("D:\\inside.txt").is_none());
    }

    #[test]
    fn hard_links_are_listed_but_counted_once() {
        let link = |id: u64, parent: u64, name: &str| RawEntry { flags: FLAG_HARDLINK, ..entry(id, parent, name, false, 400) };
        let raw = vec![
            entry(5, 5, ".", true, 0),
            entry(10, 5, "a", true, 0),
            entry(11, 5, "b", true, 0),
            entry(12, 10, "big.bin", false, 400),
            link(12 | 1 << 48, 11, "big-link.bin"),
            link(12 | 2 << 48, 10, "big-copy.bin"),
        ];
        let mut ix = Index::build("C:", "test", 0, raw, 5);
        let root = ix.nodes[ROOT as usize];
        assert_eq!((root.size, root.alloc, root.files), (400, 400, 1));
        let a = ix.find("C:\\a").unwrap();
        let b = ix.find("C:\\b").unwrap();
        assert_eq!((ix.nodes[a as usize].size, ix.nodes[a as usize].files), (400, 1));
        assert_eq!((ix.nodes[b as usize].size, ix.nodes[b as usize].files), (0, 0));
        // Each link is still listed and found with the file's size.
        let listed = ix.list(b);
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].size, listed[0].path.as_str()), (400, "C:\\b\\big-link.bin"));
        assert_eq!(ix.search("big", None, 10).0, 3);

        // Removing an extra link leaves the totals alone.
        let extra = ix.find("C:\\a\\big-copy.bin").unwrap();
        ix.remove(extra);
        assert_eq!(ix.nodes[ROOT as usize].size, 400);
        assert_eq!(ix.nodes[a as usize].size, 400);
        // Removing a folder takes out only what it counted.
        ix.remove(b);
        assert_eq!(ix.nodes[ROOT as usize].size, 400);
        ix.remove(a);
        assert_eq!(ix.nodes[ROOT as usize].size, 0);
        assert_eq!(ix.nodes[ROOT as usize].files, 0);

        // The flag survives the cache.
        let back = Index::load_from(&mut bytes(&Index::build(
            "C:",
            "test",
            0,
            vec![entry(5, 5, ".", true, 0), entry(12, 5, "f", false, 9), link(12 | 1 << 48, 5, "g")],
            5,
        )).as_slice())
        .unwrap();
        assert_eq!(back.nodes[ROOT as usize].size, 9);
        assert_eq!(back.list(ROOT).len(), 2);
    }

    #[test]
    fn parent_cycles_are_cut_and_hidden() {
        let raw = vec![
            entry(5, 5, ".", true, 0),
            entry(10, 5, "ok", true, 0),
            entry(11, 10, "ok.txt", false, 1),
            // 2-cycle and 3-cycle that never reach the root, each with a file below.
            entry(20, 21, "x", true, 0),
            entry(21, 20, "y", true, 0),
            entry(22, 21, "in-two.txt", false, 10),
            entry(30, 32, "p", true, 0),
            entry(31, 30, "q", true, 0),
            entry(32, 31, "r", true, 0),
            entry(33, 32, "in-three.txt", false, 100),
        ];
        let ix = Index::build("C:", "test", 0, raw, 5);
        assert_eq!(ix.nodes[ROOT as usize].size, 1);
        for i in 0..ix.nodes.len() as u32 {
            assert!(ix.is_under(i, ROOT));
            ix.path_of(i);
        }
        // Only the root, "ok" and "ok.txt" stay visible.
        let live: Vec<&str> = (0..ix.nodes.len() as u32).filter(|&i| ix.is_live(i)).map(|i| ix.name(i)).collect();
        assert_eq!(live, ["C:", "ok", "ok.txt"]);
        assert_eq!(ix.search("in-", None, 10).0, 0);
        let ok = ix.find("C:\\ok").unwrap();
        assert_eq!(ix.search("*.txt", Some(ok), 10).0, 1);

        // A cycle in a cache file is damage: load refuses it instead of hanging.
        let mut ix = sample();
        let users = ix.find("C:\\Users").unwrap();
        let me = ix.find("C:\\Users\\me").unwrap();
        ix.nodes[users as usize].parent = me;
        let err = Index::load_from(&mut bytes(&ix).as_slice()).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn deep_trees_sum_past_any_depth_cap() {
        let depth = 6000u64;
        let mut raw = vec![entry(0, 0, "", true, 0)];
        for d in 1..=depth {
            raw.push(entry(d, d - 1, "d", true, 0));
        }
        raw.push(entry(depth + 1, depth, "leaf.bin", false, 42));
        raw.push(entry(depth + 2, depth / 2, "mid.bin", false, 8));
        let ix = Index::build("E:", "test", 0, raw, 0);
        assert_eq!(ix.nodes[ROOT as usize].size, 50);
        assert_eq!(ix.nodes[ROOT as usize].files, 2);
        let top = ix.find("E:\\d").unwrap();
        assert_eq!(ix.nodes[top as usize].size, 50);
    }

    #[test]
    fn unicode_names_fold_per_character() {
        let raw = vec![
            entry(5, 5, ".", true, 0),
            entry(10, 5, "İzmir", false, 1),
            entry(11, 5, "사진", true, 0),
            // Ohm sign: its lowercase (ω) is shorter in UTF-8.
            entry(12, 11, "10\u{2126}.pdf", false, 2),
            entry(13, 5, "Ärger.txt", false, 3),
            entry(14, 5, "ΟΔΟΣ", false, 4),
            entry(15, 5, "Alpha", false, 5),
            entry(16, 5, "\u{212A}elvin", false, 6),
        ];
        let mut ix = Index::build("C:", "test", 0, raw, 5);
        assert_eq!(ix.lower.len(), ix.names.len());
        for i in 0..ix.nodes.len() as u32 {
            assert_eq!(ix.lower_name(i).chars().count(), ix.name(i).chars().count());
        }
        // Non-ASCII letters fold even when other names have length-changing ones.
        assert_eq!(ix.search("Ärger", None, 10).0, 1);
        assert_eq!(ix.search("ärger", None, 10).0, 1);
        assert_eq!(ix.search("ΟΔΟΣ", None, 10).0, 1);
        assert_eq!(ix.search("οδοσ", None, 10).0, 1);
        assert_eq!(ix.search("İzmir", None, 10).0, 1);
        assert_eq!(ix.search("10\u{2126}", None, 10).0, 1);
        assert_eq!(ix.search("alpha", None, 10).0, 1);
        assert!(ix.find("C:\\사진\\10\u{3C9}.PDF").is_none());
        assert!(ix.find("C:\\사진\\10\u{2126}.PDF").is_some());
        assert!(ix.find("C:\\ärger.TXT").is_some());

        let f = ix.find("C:\\Alpha").unwrap();
        assert!(ix.rename(f, "ÉCOLE"));
        assert_eq!(ix.lower.len(), ix.names.len());
        assert_eq!(ix.find("C:\\école"), Some(f));
        assert_eq!(ix.search("éco", None, 10).0, 1);
    }

    #[test]
    fn question_mark_takes_one_character() {
        let w = |p: &str, s: &str| wildcard(p.as_bytes(), s.as_bytes());
        assert!(w("?.txt", "가.txt"));
        assert!(!w("???.txt", "가.txt"));
        assert!(w("??.jpg", "사진.jpg"));
        assert!(!w("a???", "a가"));
        assert!(w("a?", "a가"));
        assert!(w("*?xc", "가xc"));
        // Backtracking must not land inside a character.
        assert!(!w("*??xc", "가xc"));
        assert!(w("*진", "사진"));
        assert!(w("*.mp4", "video.mp4"));
        assert!(!w("*.mp", "video.mp4"));
        assert!(w("*", ""));
        assert!(!w("?", ""));
        let raw = vec![entry(5, 5, ".", true, 0), entry(10, 5, "사진.jpg", false, 1), entry(11, 5, "a.jpg", false, 1)];
        let ix = Index::build("C:", "test", 0, raw, 5);
        assert_eq!(ix.search("??.jpg", None, 10).0, 1);
        assert_eq!(ix.search("?.jpg", None, 10).0, 1);
    }

    #[test]
    fn name_offsets_never_wrap() {
        assert_eq!(name_slot(10, 5), Some((10, 5)));
        assert_eq!(name_slot(u32::MAX as usize - 5, 5), Some((u32::MAX - 5, 5)));
        assert_eq!(name_slot(u32::MAX as usize - 5, 6), None);
        assert_eq!(name_slot(usize::MAX, 1), None);
    }

    #[test]
    fn corrupt_cache_never_panics() {
        let good = bytes(&sample());
        assert!(Index::load_from(&mut good.as_slice()).is_ok());
        // Damage each byte in a few ways, and cut the file at each length.
        let mut cases: Vec<Vec<u8>> = (0..good.len()).map(|n| good[..n].to_vec()).collect();
        for at in 0..good.len() {
            for v in [0x00, 0xFF, 0x7F, good[at] ^ 0x01, good[at] ^ 0x80] {
                let mut b = good.clone();
                b[at] = v;
                cases.push(b);
            }
        }
        for b in cases {
            if let Ok(ix) = Index::load_from(&mut b.as_slice()) {
                // Whatever loads must be safe to browse and search.
                for i in 0..ix.nodes.len() as u32 {
                    let info = ix.info(i);
                    ix.find(&info.path);
                    ix.list(i);
                    assert!(ix.is_under(i, ROOT));
                }
                ix.search("e", None, 100);
                ix.search("*?", Some(ROOT), 100);
            }
        }
    }

    #[test]
    fn corrupt_cache_specific_cases() {
        let good = bytes(&sample());
        // Header: magic(8) + drive(8 + 2) + method(8 + 4) + scanned_at(8) + scan_ms(8) = 46.
        let count_at = 46;
        let nodes_at = count_at + 8;
        let load = |b: &[u8]| Index::load_from(&mut &b[..]).err().map(|e| e.kind());

        let mut b = good.clone();
        b[count_at..nodes_at].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::InvalidData));
        b[count_at..nodes_at].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::InvalidData));
        b[count_at..nodes_at].copy_from_slice(&1_000_000u64.to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::UnexpectedEof));

        // A huge drive-string length.
        let mut b = good.clone();
        b[8..16].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::InvalidData));

        // Node 1's parent out of range, and node 0 given a parent.
        let mut b = good.clone();
        b[nodes_at + 40..nodes_at + 44].copy_from_slice(&999u32.to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::InvalidData));
        let mut b = good.clone();
        b[nodes_at..nodes_at + 4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::InvalidData));

        // A name range past the end of the names blob.
        let mut b = good.clone();
        b[nodes_at + 48..nodes_at + 52].copy_from_slice(&5000u32.to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::InvalidData));

        // Zeroed names (e.g. unflushed data after a power loss).
        let names_len_at = good.len() - sample().names.len() - 8;
        let mut b = good.clone();
        b.truncate(names_len_at);
        b.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(load(&b), Some(io::ErrorKind::InvalidData));

        // A name range that splits a multi-byte character.
        let raw = vec![entry(5, 5, ".", true, 0), entry(10, 5, "사진", false, 1)];
        let mut ix = Index::build("C:", "test", 0, raw, 5);
        ix.nodes[1].name_len = 4;
        assert_eq!(load(&bytes(&ix)), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn inserts_add_to_every_folder_above() {
        let mut ix = sample();
        let me = ix.find("C:\\Users\\me").unwrap();
        let docs = ix.insert(me, "Docs", folder()).unwrap();
        let f = ix.insert(docs, "보고서.PDF", file(50)).unwrap();
        assert_eq!(totals(&ix, "C:\\"), (1370, 1370, 4));
        assert_eq!(totals(&ix, "C:\\Users\\me"), (370, 370, 3));
        assert_eq!(totals(&ix, "C:\\Users\\me\\Docs"), (50, 50, 1));
        // Found, listed and searched like scanned nodes.
        assert_eq!(ix.find("c:\\users\\me\\docs\\보고서.pdf"), Some(f));
        assert_eq!(ix.path_of(f), "C:\\Users\\me\\Docs\\보고서.PDF");
        assert_eq!(ix.list(me).len(), 3);
        assert_eq!(ix.search("보고서", None, 10).0, 1);
        assert_eq!(ix.search("*.pdf", Some(me), 10).0, 1);
        // The same name again is an update, not a second entry.
        assert_eq!(ix.insert(docs, "보고서.pdf", file(80)), Some(f));
        assert_eq!(ix.name(f), "보고서.pdf");
        assert_eq!(totals(&ix, "C:\\"), (1400, 1400, 4));
        // A folder where the file was replaces it.
        let d = ix.insert(docs, "보고서.pdf", folder()).unwrap();
        assert_ne!(d, f);
        assert_eq!(totals(&ix, "C:\\Users\\me\\Docs"), (0, 0, 0));
        assert_eq!(ix.list(docs).len(), 1);
        // An extra hard link is listed but not counted.
        ix.insert(me, "link.bin", Meta { flags: FLAG_HARDLINK, ..file(300) }).unwrap();
        assert_eq!(totals(&ix, "C:\\Users\\me"), (320, 320, 2));
        // Nothing goes under a file, under a removed folder, or by a bad name.
        let notes = ix.find("C:\\Users\\me\\notes.txt").unwrap();
        assert!(ix.insert(notes, "x", file(1)).is_none());
        assert!(ix.insert(me, "a\\b", file(1)).is_none());
        assert!(ix.insert(me, "", file(1)).is_none());
        ix.remove(docs);
        assert!(ix.insert(docs, "late.txt", file(1)).is_none());
        assert_consistent(&ix);
    }

    #[test]
    fn updates_carry_size_changes_up() {
        let mut ix = sample();
        let video = ix.find("C:\\Users\\me\\Video.MP4").unwrap();
        let meta = Meta { flags: FLAG_HIDDEN, size: 100, alloc: 4096, modified: 9 };
        assert!(ix.update(video, meta));
        assert_eq!(totals(&ix, "C:\\"), (1120, 5116, 3));
        assert_eq!(totals(&ix, "C:\\Users"), (120, 4116, 2));
        assert!(ix.info(video).hidden);
        assert_eq!(ix.info(video).modified, 9);
        assert!(!ix.update(video, meta));
        // A folder's size stays the sum of its children.
        let users = ix.find("C:\\Users").unwrap();
        assert!(ix.update(users, Meta { flags: FLAG_DIR, size: 5, alloc: 5, modified: 3 }));
        assert_eq!(totals(&ix, "C:\\Users"), (120, 4116, 2));
        assert_eq!(ix.info(users).modified, 3);
        assert_consistent(&ix);
    }

    #[test]
    fn moves_carry_totals_between_folders() {
        let mut ix = sample();
        let me = ix.find("C:\\Users\\me").unwrap();
        let users = ix.find("C:\\Users").unwrap();
        let video = ix.find("C:\\Users\\me\\Video.MP4").unwrap();
        // Up to the root, renamed on the way.
        assert!(ix.move_to(video, ROOT, "clip.mp4"));
        assert!(ix.find("C:\\Users\\me\\Video.MP4").is_none());
        assert_eq!(ix.find("C:\\CLIP.mp4"), Some(video));
        assert_eq!(totals(&ix, "C:\\Users"), (20, 20, 1));
        assert_eq!(totals(&ix, "C:\\"), (1320, 1320, 3));
        // A folder moves with everything in it.
        let new = ix.insert(ROOT, "new", folder()).unwrap();
        assert!(ix.move_to(me, new, "me2"));
        assert_eq!(totals(&ix, "C:\\new"), (20, 20, 1));
        assert_eq!(totals(&ix, "C:\\Users"), (0, 0, 0));
        assert!(ix.find("C:\\new\\me2\\notes.txt").is_some());
        assert_eq!(ix.search("notes", Some(new), 10).0, 1);
        assert_eq!(ix.search("notes", Some(users), 10).0, 0);
        // And back where it was built, without being listed twice.
        assert!(ix.move_to(me, users, "me"));
        assert_eq!(ix.list(users).len(), 1);
        assert!(ix.list(new).is_empty());
        // Never into itself.
        assert!(!ix.move_to(users, me, "loop"));
        assert!(!ix.move_to(users, users, "loop"));
        // A case-only rename keeps the node.
        let notes = ix.find("C:\\Users\\me\\notes.txt").unwrap();
        assert!(ix.move_to(notes, me, "NOTES.txt"));
        assert_eq!(ix.list(me).iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["NOTES.txt"]);
        // Moving onto a taken name replaces that entry (saving by rename).
        let tmp = ix.insert(me, "notes.tmp", file(25)).unwrap();
        assert!(ix.move_to(tmp, me, "notes.txt"));
        assert_eq!(ix.find("C:\\Users\\me\\notes.txt"), Some(tmp));
        assert_eq!(totals(&ix, "C:\\Users\\me"), (25, 25, 1));
        assert_consistent(&ix);
    }

    #[test]
    fn removes_take_live_nodes_out_too() {
        let mut ix = sample();
        let me = ix.find("C:\\Users\\me").unwrap();
        let docs = ix.insert(me, "Docs", folder()).unwrap();
        ix.insert(docs, "a.txt", file(5)).unwrap();
        let b = ix.insert(docs, "b.txt", file(6)).unwrap();
        ix.remove(b);
        assert_eq!(totals(&ix, "C:\\Users\\me\\Docs"), (5, 5, 1));
        ix.remove(me);
        assert_eq!(totals(&ix, "C:\\"), (1000, 1000, 1));
        assert!(ix.find("C:\\Users\\me\\Docs\\a.txt").is_none());
        assert_eq!(ix.search("a.txt", None, 10).0, 0);
        // Removing twice changes nothing.
        ix.remove(me);
        assert_eq!(totals(&ix, "C:\\"), (1000, 1000, 1));
        assert_consistent(&ix);
    }

    #[test]
    fn inserted_trees_are_linked_and_summed() {
        let mut ix = sample();
        let users = ix.find("C:\\Users").unwrap();
        // As a walk of the new folder reports it, in no particular order.
        let below = vec![
            entry(3, 1, "deep.txt", false, 7),
            entry(0, 0, "", true, 0),
            entry(1, 0, "sub", true, 0),
            entry(2, 0, "top.txt", false, 3),
            entry(4, 2, "under-a-file", false, 100),
            entry(5, 77, "lost", false, 100),
        ];
        let top = ix.insert_tree(users, "you", folder(), below, 0).unwrap();
        assert_eq!(totals(&ix, "C:\\Users\\you"), (10, 10, 2));
        assert_eq!(totals(&ix, "C:\\"), (1330, 1330, 5));
        assert!(ix.find("C:\\Users\\you\\sub\\deep.txt").is_some());
        assert!(ix.find("C:\\Users\\you\\top.txt\\under-a-file").is_none());
        assert_eq!(ix.search("lost", None, 10).0, 0);
        // A second walk of it replaces what was there.
        let again = ix.insert_tree(users, "YOU", folder(), vec![entry(1, 0, "only.txt", false, 1)], 0).unwrap();
        assert_ne!(again, top);
        assert_eq!(totals(&ix, "C:\\Users\\you"), (1, 1, 1));
        assert_eq!(ix.list(users).len(), 2);
        assert_consistent(&ix);
    }

    #[test]
    fn live_changes_survive_the_cache_and_compaction() {
        let mut ix = sample();
        let me = ix.find("C:\\Users\\me").unwrap();
        let docs = ix.insert(me, "Docs", folder()).unwrap();
        ix.insert(docs, "a.txt", file(5)).unwrap();
        let video = ix.find("C:\\Users\\me\\Video.MP4").unwrap();
        assert!(ix.move_to(video, docs, "v.mp4"));
        let page = ix.find("C:\\pagefile.sys").unwrap();
        ix.remove(page);
        let check = |ix: &Index| {
            assert_eq!(totals(ix, "C:\\"), (325, 325, 3));
            assert_eq!(totals(ix, "C:\\Users\\me\\Docs"), (305, 305, 2));
            assert!(ix.find("C:\\Users\\me\\Docs\\v.mp4").is_some());
            assert!(ix.find("C:\\Users\\me\\Video.MP4").is_none());
            assert!(ix.find("C:\\pagefile.sys").is_none());
            assert_eq!(ix.search("a.txt", None, 10).0, 1);
            assert_consistent(ix);
        };
        check(&ix);
        check(&Index::load_from(&mut bytes(&ix).as_slice()).unwrap());
        let before = ix.nodes.len();
        ix.compact();
        check(&ix);
        // pagefile.sys and the orphan are dropped, along with the old names.
        assert_eq!(ix.nodes.len(), before - 2);
        assert_eq!((ix.dead_nodes, ix.dead_names), (0, 0));
        // It still takes changes after the rebuild.
        let docs = ix.find("C:\\Users\\me\\Docs").unwrap();
        ix.insert(docs, "b.txt", file(1)).unwrap();
        assert_eq!(totals(&ix, "C:\\"), (326, 326, 4));
        assert_consistent(&ix);
    }

    #[test]
    fn many_removals_ask_for_compaction() {
        let mut raw = vec![entry(0, 0, "", true, 0)];
        raw.extend((1..=200_000u64).map(|i| entry(i, 0, "f", false, 1)));
        let mut ix = Index::build("C:", "test", 0, raw, 0);
        assert!(!ix.wants_compact());
        let all: Vec<u32> = ix.children_of(ROOT).collect();
        for &c in &all[..120_000] {
            ix.remove(c);
        }
        assert!(ix.wants_compact());
        ix.compact();
        assert!(!ix.wants_compact());
        assert_eq!(ix.nodes.len(), 80_001);
        assert_eq!(totals(&ix, "C:\\"), (80_000, 80_000, 80_000));
    }

    #[test]
    fn big_folders_are_looked_up_by_name_map() {
        let mut raw = vec![entry(0, 0, "", true, 0), entry(1, 0, "big", true, 0), entry(2, 0, "small", true, 0)];
        raw.extend((0..1000u64).map(|i| entry(10 + i, 1, &format!("File{i}.txt"), false, 1)));
        raw.push(entry(5000, 2, "a.txt", false, 1));
        let mut ix = Index::build("C:", "test", 0, raw, 0);
        let big = ix.find("C:\\big").unwrap();
        let small = ix.find("C:\\small").unwrap();
        assert_eq!(ix.locate("C:\\BIG\\file7.TXT"), ix.find("C:\\big\\File7.txt"));
        assert!(matches!(ix.by_name.get(&big), Some(Some(m)) if m.len() == 1000));
        assert!(!ix.by_name.contains_key(&small));
        assert_eq!(ix.mapped, 1000);
        // Every change keeps the map in step with the folder.
        let n = ix.insert(big, "New.bin", file(5)).unwrap();
        assert_eq!(ix.locate_child(big, "new.BIN"), Some(n));
        let f3 = ix.find("C:\\big\\File3.txt").unwrap();
        assert!(ix.rename(f3, "Renamed.txt"));
        assert!(ix.locate_child(big, "File3.txt").is_none());
        assert_eq!(ix.locate_child(big, "renamed.txt"), Some(f3));
        let f4 = ix.find("C:\\big\\File4.txt").unwrap();
        assert!(ix.move_to(f4, small, "moved.txt"));
        assert!(ix.locate_child(big, "File4.txt").is_none());
        assert_eq!(ix.locate_child(small, "moved.txt"), Some(f4));
        let f5 = ix.find("C:\\big\\File5.txt").unwrap();
        assert!(ix.move_to(f5, big, "File6.txt"));
        assert_eq!(ix.locate_child(big, "file6.txt"), Some(f5));
        assert!(ix.locate_child(big, "File5.txt").is_none());
        let f6 = ix.find("C:\\big\\File8.txt").unwrap();
        ix.remove(f6);
        assert!(ix.locate_child(big, "File8.txt").is_none());
        // Back into the folder it came from.
        assert!(ix.move_to(f4, big, "File4.txt"));
        assert_eq!(ix.locate_child(big, "file4.txt"), Some(f4));
        // The map answers exactly what a search of the folder would.
        for i in 0..1000 {
            let name = format!("file{i}.TXT");
            let slow = ix.children_of(big).find(|&c| ix.is_live(c) && ix.named(c, &name));
            assert_eq!(ix.locate_child(big, &name), slow, "{name}");
        }
        assert_eq!(ix.mapped, ix.by_name.values().flatten().map(|m| m.len()).sum::<usize>());
        assert_consistent(&ix);
        // A second child with the same folded name (a case-sensitive folder):
        // the folder goes back to being searched, and both stay findable.
        let twin = ix.push_node(big, "FILE1.TXT", file(1)).unwrap();
        assert!(matches!(ix.by_name.get(&big), Some(None)));
        assert!(ix.locate_child(big, "file1.txt").is_some());
        assert_eq!(ix.locate_child(big, "FILE1.TXT"), Some(twin));
        assert!(ix.locate_child(big, "File1.txt").is_some_and(|c| c != twin));
        ix.remove(twin);
        assert!(ix.locate_child(big, "File1.txt").is_some());
        // A removed folder takes its map along; a rebuild drops them all.
        ix.remove(big);
        assert!(ix.by_name.is_empty() && ix.mapped == 0);
        ix.locate("C:\\small\\moved.txt");
        ix.compact();
        assert!(ix.by_name.is_empty());
    }

    #[test]
    fn exact_names_win_in_case_sensitive_folders() {
        let raw = vec![
            entry(5, 5, ".", true, 0),
            entry(10, 5, "Build", true, 0),
            entry(11, 5, "build", true, 0),
            entry(12, 10, "upper.txt", false, 1),
            entry(13, 11, "lower.txt", false, 2),
        ];
        let mut ix = Index::build("C:", "test", 0, raw, 5);
        let upper = ix.find("C:\\Build").unwrap();
        let lower = ix.find("C:\\build").unwrap();
        assert_ne!(upper, lower);
        assert_eq!((ix.name(upper), ix.name(lower)), ("Build", "build"));
        // Whichever comes first in the folder, each path leads to its own folder.
        assert!(ix.find("C:\\build\\lower.txt").is_some() && ix.find("C:\\Build\\upper.txt").is_some());
        assert!(ix.find("C:\\build\\upper.txt").is_none() && ix.locate("C:\\Build\\lower.txt").is_none());
        assert_eq!(ix.locate("C:\\build\\lower.txt"), ix.find("C:\\build\\lower.txt"));
        // No exact match: one of them.
        assert!(ix.find("C:\\BUILD").is_some_and(|n| n == upper || n == lower));
        // A change to one leaves the other's name alone.
        assert_eq!(ix.insert(ROOT, "build", folder()), Some(lower));
        assert_eq!(ix.insert(ROOT, "Build", folder()), Some(upper));
        assert_eq!((ix.name(upper), ix.name(lower)), ("Build", "build"));
        assert_consistent(&ix);
    }

    #[test]
    fn removing_a_counted_hard_link_is_counted() {
        let raw = vec![
            entry(5, 5, ".", true, 0),
            entry(10, 5, "a", true, 0),
            RawEntry { flags: FLAG_LINKED, ..entry(12, 10, "big.bin", false, 400) },
            RawEntry { flags: FLAG_HARDLINK, ..entry(12 | 1 << 48, 5, "big-link.bin", false, 400) },
            RawEntry { flags: FLAG_DIR | FLAG_LINKED, ..entry(13, 5, "dir", true, 0) },
        ];
        let mut ix = Index::build("C:", "test", 0, raw, 5);
        // A folder never carries the flag.
        assert_eq!(ix.nodes[ix.find("C:\\dir").unwrap() as usize].flags, FLAG_DIR);
        let link = ix.find("C:\\big-link.bin").unwrap();
        ix.remove(link);
        assert_eq!(ix.linked_removed(), 0);
        let a = ix.find("C:\\a").unwrap();
        ix.remove(a);
        assert_eq!(ix.linked_removed(), 1);
        assert_eq!(totals(&ix, "C:\\"), (0, 0, 0));
    }

    #[test]
    fn placeholder_folders_keep_their_flag() {
        let mut ix = sample();
        let cloud = Meta { flags: FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK, ..folder() };
        let c = ix.insert(ROOT, "OneDrive", cloud).unwrap();
        assert_eq!(ix.nodes[c as usize].flags, cloud.flags);
        // Seen without the placeholder attributes, it is a plain folder.
        assert!(ix.update(c, folder()));
        assert_eq!(ix.nodes[c as usize].flags, FLAG_DIR);
        assert!(ix.update(c, cloud));
        assert_eq!(ix.nodes[c as usize].flags, cloud.flags);
    }
}
