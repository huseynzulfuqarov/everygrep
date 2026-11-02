//! Compact storage for file and folder nodes.
//!
//! Nodes live in parallel columns indexed by [`NodeId`]. Names are stored once in a
//! shared byte pool, together with a folded copy used for matching, so a search
//! never has to allocate or fold per comparison.
//!
//! Every folder also knows the total size of the files below it, at any depth,
//! and the number of its direct children. Both are kept up to date on every
//! change: adding, removing or moving a node, or changing a file's size, adds
//! the difference to each folder on the way up to the root, which costs one
//! step per level. After a load they are computed once, bottom-up.
//!
//! Each file counts with the size stored for it: a file with several hard
//! links counts once per link, and a symbolic link counts with the length of
//! its target path (links are never followed). Folders that are not entered
//! (excluded folders, virtual file systems) have no children and count as 0.
//! Sums wrap around at 2^64 bytes instead of overflowing.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::fold::fold_into;

/// Identifier of a node inside a [`NodeStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    /// Index of the node in the store columns.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Basic metadata kept for every node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// Size in bytes. For a folder in a [`NodeStore`] this is the total size
    /// of all files below it, kept by the store.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch; 0 when it is
    /// not known (not read, or a folder whose listing failed and that is
    /// read again by the next catch-up). It is shown empty and matches no
    /// date.
    pub mtime_ns: i64,
}

impl Meta {
    /// Metadata of a directory entry as returned by `lstat`. Folders get size 0.
    pub fn from_metadata(m: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            size: if m.is_dir() { 0 } else { m.len() },
            mtime_ns: m
                .mtime()
                .saturating_mul(1_000_000_000)
                .saturating_add(m.mtime_nsec()),
        }
    }
}

/// Marks a folder in the packed parent column.
const DIR_BIT: u32 = 1 << 31;
/// Marks a removed node. Its slot stays until the store is compacted.
const DEAD_BIT: u32 = 1 << 30;
/// Bits of the packed parent column that hold the parent id + 1.
const PARENT_MASK: u32 = DEAD_BIT - 1;
/// "No node" in the child and sibling link columns.
const NONE: u32 = u32::MAX;
/// Marks a folded name that is identical to the raw name and therefore not stored.
const SAME_AS_RAW: u32 = u32::MAX;

/// Column storage of all nodes.
///
/// Per node it keeps a packed parent/folder word, the position and length of the
/// raw name, the position and length of the folded name (shared with the raw name
/// when folding changes nothing) and the metadata.
#[derive(Default)]
pub struct NodeStore {
    /// Bit 31: folder flag. Bit 30: removed. Bits 0..30: parent id + 1, or 0 for a root.
    parent_kind: Vec<u32>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    folded_start: Vec<u32>,
    folded_len: Vec<u16>,
    meta: Vec<Meta>,
    name_pool: Vec<u8>,
    folded_pool: Vec<u8>,
    /// Children of each node as a doubly linked list, so a subtree can be walked
    /// and a node unlinked without scanning the whole store. Not saved; rebuilt
    /// on load.
    first_child: Vec<u32>,
    next_sibling: Vec<u32>,
    prev_sibling: Vec<u32>,
    /// Number of live direct children of each node (0 for files). Not
    /// saved; counted on load.
    items: Vec<u32>,
    scratch: Vec<u8>,
    dead: usize,
    garbage: usize,
}

impl NodeStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of node slots, including removed nodes that are not compacted yet.
    pub fn len(&self) -> usize {
        self.parent_kind.len()
    }

    /// Number of nodes that are not removed.
    pub fn live_count(&self) -> usize {
        self.len() - self.dead
    }

    /// Whether the node has not been removed.
    pub fn is_live(&self, id: NodeId) -> bool {
        self.parent_kind[id.index()] & DEAD_BIT == 0
    }

    /// Whether the store has no nodes.
    pub fn is_empty(&self) -> bool {
        self.parent_kind.is_empty()
    }

    /// Adds a node and returns its id.
    ///
    /// A root node has no parent and stores its full path as its name.
    ///
    /// # Panics
    ///
    /// Panics if the store would exceed 2^30 - 1 nodes, 4 GiB of names, or a name
    /// longer than 65 535 bytes.
    ///
    /// A new folder has no children yet, so its size starts at 0 whatever
    /// `meta` says; a file's size is added to every folder above it.
    pub fn add(&mut self, parent: Option<NodeId>, name: &[u8], is_dir: bool, meta: Meta) -> NodeId {
        let meta = if is_dir {
            Meta { size: 0, ..meta }
        } else {
            meta
        };
        let id = self.push_node(name, is_dir, meta);
        if let Some(p) = parent {
            self.set_parent_word(id, Some(p));
            self.link(id, p);
            self.add_up(Some(p), meta.size, true);
        }
        id
    }

    /// Appends an unlinked root-like node with exactly `meta`.
    fn push_node(&mut self, name: &[u8], is_dir: bool, meta: Meta) -> NodeId {
        let id = u32::try_from(self.len())
            .ok()
            .filter(|&i| i < PARENT_MASK)
            .expect("too many nodes");
        let (name_start, name_len, folded_start, folded_len) = self.push_names(name);
        self.parent_kind.push(if is_dir { DIR_BIT } else { 0 });
        self.name_start.push(name_start);
        self.name_len.push(name_len);
        self.folded_start.push(folded_start);
        self.folded_len.push(folded_len);
        self.meta.push(meta);
        self.first_child.push(NONE);
        self.next_sibling.push(NONE);
        self.prev_sibling.push(NONE);
        self.items.push(0);
        NodeId(id)
    }

    fn set_parent_word(&mut self, id: NodeId, parent: Option<NodeId>) {
        let word = &mut self.parent_kind[id.index()];
        *word = (*word & !PARENT_MASK) | parent.map_or(0, |p| p.0 + 1);
    }

    /// Puts `id` at the front of `parent`'s child list. Folder sizes are left
    /// alone; see [`add_up`](Self::add_up).
    fn link(&mut self, id: NodeId, parent: NodeId) {
        let (i, p) = (id.index(), parent.index());
        let next = self.first_child[p];
        self.next_sibling[i] = next;
        self.prev_sibling[i] = NONE;
        if next != NONE {
            self.prev_sibling[next as usize] = id.0;
        }
        self.first_child[p] = id.0;
        self.items[p] += 1;
    }

    /// Adds (or with `add` false, subtracts) `bytes` to the size of `from` and
    /// every folder above it. Stops at a removed folder: its size was already
    /// taken from the folders above it when it was removed, so the nodes below
    /// it can be removed in any order.
    fn add_up(&mut self, from: Option<NodeId>, bytes: u64, add: bool) {
        if bytes == 0 {
            return;
        }
        let mut cur = from;
        while let Some(p) = cur {
            if !self.is_live(p) {
                break;
            }
            let size = &mut self.meta[p.index()].size;
            *size = if add {
                size.wrapping_add(bytes)
            } else {
                size.wrapping_sub(bytes)
            };
            cur = self.parent(p);
        }
    }

    /// Takes `id` out of its parent's child list.
    fn unlink(&mut self, id: NodeId) {
        let Some(parent) = self.parent(id) else {
            return;
        };
        let i = id.index();
        let (prev, next) = (self.prev_sibling[i], self.next_sibling[i]);
        if prev == NONE {
            if self.first_child[parent.index()] == id.0 {
                self.first_child[parent.index()] = next;
            }
        } else {
            self.next_sibling[prev as usize] = next;
        }
        if next != NONE {
            self.prev_sibling[next as usize] = prev;
        }
        self.next_sibling[i] = NONE;
        self.prev_sibling[i] = NONE;
        self.items[parent.index()] -= 1;
    }

    /// Live children of a node, in no particular order.
    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut cur = self.first_child[id.index()];
        std::iter::from_fn(move || {
            (cur != NONE).then(|| {
                let id = NodeId(cur);
                cur = self.next_sibling[cur as usize];
                id
            })
        })
    }

    fn push_names(&mut self, name: &[u8]) -> (u32, u16, u32, u16) {
        let (name_start, name_len) = push(&mut self.name_pool, name);
        self.scratch.clear();
        fold_into(name, &mut self.scratch);
        let (folded_start, folded_len) = if self.scratch == name {
            (SAME_AS_RAW, name_len)
        } else {
            push(&mut self.folded_pool, &self.scratch)
        };
        (name_start, name_len, folded_start, folded_len)
    }

    /// Marks a node as removed. Its name bytes become garbage until compaction.
    /// Its size (for a folder, the total of its subtree) is taken from the folders
    /// above it. Nodes below a removed folder must be removed too; that may
    /// happen before or after the folder itself.
    pub fn remove(&mut self, id: NodeId) {
        let i = id.index();
        if self.parent_kind[i] & DEAD_BIT == 0 {
            let parent = self.parent(id);
            self.add_up(parent, self.meta[i].size, false);
            self.unlink(id);
            self.parent_kind[i] |= DEAD_BIT;
            self.dead += 1;
            self.garbage += self.name_len[i] as usize + self.stored_folded_len(i);
        }
    }

    /// Gives a node a new name. The old name bytes become garbage.
    pub fn set_name(&mut self, id: NodeId, name: &[u8]) {
        let i = id.index();
        self.garbage += self.name_len[i] as usize + self.stored_folded_len(i);
        let (ns, nl, fs, fl) = self.push_names(name);
        self.name_start[i] = ns;
        self.name_len[i] = nl;
        self.folded_start[i] = fs;
        self.folded_len[i] = fl;
    }

    /// Moves a node under a new parent. Its size moves with it.
    pub fn set_parent(&mut self, id: NodeId, parent: Option<NodeId>) {
        let size = if self.is_live(id) {
            self.meta[id.index()].size
        } else {
            0
        };
        self.add_up(self.parent(id), size, false);
        self.unlink(id);
        self.set_parent_word(id, parent);
        if let Some(p) = parent {
            self.link(id, p);
        }
        self.add_up(parent, size, true);
    }

    /// Replaces the metadata of a node. A folder keeps its total size and
    /// takes only the modification time; a file's change in size is passed on
    /// to the folders above it.
    pub fn set_meta(&mut self, id: NodeId, meta: Meta) {
        let i = id.index();
        let old = self.meta[i].size;
        if self.is_dir(id) {
            self.meta[i].mtime_ns = meta.mtime_ns;
            return;
        }
        self.meta[i] = meta;
        if self.is_live(id) {
            let parent = self.parent(id);
            if meta.size >= old {
                self.add_up(parent, meta.size - old, true);
            } else {
                self.add_up(parent, old - meta.size, false);
            }
        }
    }

    /// Number of live direct children of a node; 0 for files.
    pub fn items(&self, id: NodeId) -> u32 {
        self.items[id.index()]
    }

    /// Name bytes no longer used by any live node.
    pub fn garbage_bytes(&self) -> usize {
        self.garbage
    }

    /// Bytes held by both name pools, used and unused.
    pub fn name_bytes(&self) -> usize {
        self.name_pool.len() + self.folded_pool.len()
    }

    fn stored_folded_len(&self, i: usize) -> usize {
        if self.folded_start[i] == SAME_AS_RAW {
            0
        } else {
            self.folded_len[i] as usize
        }
    }

    /// Key that orders nodes with equal names by parent: 0 for roots, parent id + 1
    /// otherwise.
    pub(crate) fn parent_key(&self, id: NodeId) -> u32 {
        self.parent_kind[id.index()] & PARENT_MASK
    }

    /// Returns a copy without removed nodes and unused name bytes, plus the map
    /// from old to new ids. New ids keep the relative order of the old ones.
    ///
    /// # Panics
    ///
    /// Panics if a live node has a removed parent.
    pub fn compacted(&self) -> (NodeStore, Vec<Option<NodeId>>) {
        self.compacted_where(|id| self.is_live(id))
    }

    /// Like [`compacted`](Self::compacted), but keeps only the live nodes for
    /// which `keep` is true. The parent of every kept node must be kept.
    ///
    /// # Panics
    ///
    /// Panics if a kept node has a parent that is not kept.
    pub fn compacted_where(
        &self,
        keep: impl Fn(NodeId) -> bool,
    ) -> (NodeStore, Vec<Option<NodeId>>) {
        let mut remap = Vec::with_capacity(self.len());
        let mut next = 0u32;
        for i in 0..self.len() {
            let id = NodeId(i as u32);
            if self.is_live(id) && keep(id) {
                remap.push(Some(NodeId(next)));
                next += 1;
            } else {
                remap.push(None);
            }
        }
        // A parent can have a higher id than its child (after moves), so add all
        // nodes first and link parents in a second pass.
        // Folder sizes are copied as they are; the tree of live nodes does not
        // change.
        let mut out = NodeStore::new();
        for (i, kept) in remap.iter().enumerate() {
            if kept.is_some() {
                let id = NodeId(i as u32);
                out.push_node(self.name(id), self.is_dir(id), self.meta(id));
            }
        }
        for i in 0..self.len() {
            let id = NodeId(i as u32);
            if let (Some(new_id), Some(parent)) = (remap[i], self.parent(id)) {
                let new_parent = remap[parent.index()].expect("kept node with a dropped parent");
                out.set_parent_word(new_id, Some(new_parent));
                out.link(new_id, new_parent);
            }
        }
        out.shrink_to_fit();
        (out, remap)
    }

    /// Appends every live node of `other` (with its subtree links, folder
    /// sizes and item counts) and returns the id the first of them got: a
    /// node `n` of `other` becomes `base + n` when `other` has no removed
    /// nodes. Roots of `other` stay roots here.
    ///
    /// # Panics
    ///
    /// Panics if `other` has removed nodes, or if the store would grow too
    /// large (see [`add`](Self::add)).
    pub fn append(&mut self, other: &NodeStore) -> u32 {
        assert_eq!(other.dead, 0, "append a compacted store");
        let base = u32::try_from(self.len()).expect("too many nodes");
        for i in 0..other.len() {
            let id = NodeId(i as u32);
            self.push_node(other.name(id), other.is_dir(id), other.meta(id));
        }
        for i in 0..other.len() {
            if let Some(parent) = other.parent(NodeId(i as u32)) {
                let (new, new_parent) = (NodeId(base + i as u32), NodeId(base + parent.0));
                self.set_parent_word(new, Some(new_parent));
                self.link(new, new_parent);
            }
        }
        base
    }

    /// The root above a node (the node itself for a root).
    pub fn root_of(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            cur = p;
        }
        cur
    }

    /// Releases spare capacity after a bulk build.
    pub fn shrink_to_fit(&mut self) {
        self.parent_kind.shrink_to_fit();
        self.name_start.shrink_to_fit();
        self.name_len.shrink_to_fit();
        self.folded_start.shrink_to_fit();
        self.folded_len.shrink_to_fit();
        self.meta.shrink_to_fit();
        self.name_pool.shrink_to_fit();
        self.folded_pool.shrink_to_fit();
        self.first_child.shrink_to_fit();
        self.next_sibling.shrink_to_fit();
        self.prev_sibling.shrink_to_fit();
        self.items.shrink_to_fit();
        self.scratch = Vec::new();
    }

    /// Parent of a node, `None` for roots.
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        match self.parent_kind[id.index()] & PARENT_MASK {
            0 => None,
            p => Some(NodeId(p - 1)),
        }
    }

    /// Whether the node is a folder.
    pub fn is_dir(&self, id: NodeId) -> bool {
        self.parent_kind[id.index()] & DIR_BIT != 0
    }

    /// Raw name bytes of a node.
    pub fn name(&self, id: NodeId) -> &[u8] {
        let i = id.index();
        slice(&self.name_pool, self.name_start[i], self.name_len[i])
    }

    /// Folded name bytes of a node, used for case-insensitive matching.
    pub fn folded_name(&self, id: NodeId) -> &[u8] {
        let i = id.index();
        match self.folded_start[i] {
            SAME_AS_RAW => self.name(id),
            start => slice(&self.folded_pool, start, self.folded_len[i]),
        }
    }

    /// Metadata of a node.
    pub fn meta(&self, id: NodeId) -> Meta {
        self.meta[id.index()]
    }

    /// Full path of a node, built by walking up to its root.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts = vec![id];
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            parts.push(p);
            cur = p;
        }
        let mut path = PathBuf::new();
        for part in parts.iter().rev() {
            path.push(OsStr::from_bytes(self.name(*part)));
        }
        path
    }

    /// Approximate heap memory used by the store, in bytes.
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        (self.parent_kind.capacity()
            + self.name_start.capacity()
            + self.folded_start.capacity()
            + self.first_child.capacity()
            + self.next_sibling.capacity()
            + self.prev_sibling.capacity()
            + self.items.capacity())
            * size_of::<u32>()
            + (self.name_len.capacity() + self.folded_len.capacity()) * size_of::<u16>()
            + self.meta.capacity() * size_of::<Meta>()
            + self.name_pool.capacity()
            + self.folded_pool.capacity()
            + self.scratch.capacity()
    }
}

/// Raw columns of a [`NodeStore`], used to save and load an index.
pub(crate) struct Columns {
    pub parent_kind: Vec<u32>,
    pub name_start: Vec<u32>,
    pub name_len: Vec<u16>,
    pub meta: Vec<Meta>,
    pub name_pool: Vec<u8>,
}

impl NodeStore {
    /// Borrows the raw columns for saving.
    pub(crate) fn columns(&self) -> ColumnsRef<'_> {
        ColumnsRef {
            parent_kind: &self.parent_kind,
            name_start: &self.name_start,
            name_len: &self.name_len,
            meta: &self.meta,
            name_pool: &self.name_pool,
        }
    }

    /// Rebuilds a store from loaded columns after checking every length, range and
    /// parent link, so a damaged file can never cause an out-of-bounds access or
    /// an inconsistent index. Folded names are recomputed rather than trusted.
    pub(crate) fn from_columns(c: Columns) -> Result<Self, &'static str> {
        let n = c.parent_kind.len();
        if [c.name_start.len(), c.name_len.len(), c.meta.len()]
            .iter()
            .any(|&l| l != n)
        {
            return Err("column lengths differ");
        }
        if n >= PARENT_MASK as usize {
            return Err("too many nodes");
        }
        for i in 0..n {
            let word = c.parent_kind[i];
            if word & DEAD_BIT != 0 {
                return Err("removed node in saved index");
            }
            let parent = (word & PARENT_MASK) as usize;
            if parent > n || parent == i + 1 {
                return Err("bad parent link");
            }
            if parent != 0 && c.parent_kind[parent - 1] & DIR_BIT == 0 {
                return Err("parent is not a folder");
            }
            let end = (c.name_start[i] as usize).checked_add(c.name_len[i] as usize);
            if !end.is_some_and(|e| e <= c.name_pool.len()) {
                return Err("name out of range");
            }
        }
        // A parent chain must end at a root; walking it must not loop.
        let mut state = vec![0u8; n]; // 0 = unknown, 1 = on current walk, 2 = reaches a root
        let mut walk = Vec::new();
        for start in 0..n {
            let mut cur = start;
            while state[cur] == 0 {
                state[cur] = 1;
                walk.push(cur);
                match c.parent_kind[cur] & PARENT_MASK {
                    0 => break,
                    p => cur = p as usize - 1,
                }
            }
            if state[cur] == 1 && c.parent_kind[cur] & PARENT_MASK != 0 {
                return Err("parent cycle");
            }
            for &w in &walk {
                state[w] = 2;
            }
            walk.clear();
        }

        let mut store = Self {
            parent_kind: c.parent_kind,
            name_start: c.name_start,
            name_len: c.name_len,
            folded_start: Vec::with_capacity(n),
            folded_len: Vec::with_capacity(n),
            meta: c.meta,
            name_pool: c.name_pool,
            folded_pool: Vec::new(),
            first_child: vec![NONE; n],
            next_sibling: vec![NONE; n],
            prev_sibling: vec![NONE; n],
            items: vec![0; n],
            scratch: Vec::new(),
            dead: 0,
            garbage: 0,
        };
        for i in 0..n {
            let name = slice(&store.name_pool, store.name_start[i], store.name_len[i]);
            store.scratch.clear();
            fold_into(name, &mut store.scratch);
            let (fs, fl) = if store.scratch == name {
                (SAME_AS_RAW, store.name_len[i])
            } else {
                let (fs, fl) = push(&mut store.folded_pool, &store.scratch);
                (fs, fl)
            };
            store.folded_start.push(fs);
            store.folded_len.push(fl);
        }
        for i in 0..n {
            let id = NodeId(i as u32);
            if let Some(p) = store.parent(id) {
                store.link(id, p);
            }
        }
        store.compute_folder_sizes();
        store.shrink_to_fit();
        Ok(store)
    }

    /// Sets the size of every folder to the total of the files below it,
    /// ignoring the sizes stored for folders. The store must have no removed
    /// nodes.
    fn compute_folder_sizes(&mut self) {
        let n = self.len();
        let mut parents_first = true;
        for i in 0..n {
            if self.parent_kind[i] & DIR_BIT != 0 {
                self.meta[i].size = 0;
            }
            parents_first &= self.parent(NodeId(i as u32)).is_none_or(|p| p.index() < i);
        }
        if parents_first {
            // The common case (a scan adds parents before their children):
            // going backwards finishes every folder before its own parent.
            for i in (0..n).rev() {
                if let Some(p) = self.parent(NodeId(i as u32)) {
                    let size = self.meta[i].size;
                    let total = &mut self.meta[p.index()].size;
                    *total = total.wrapping_add(size);
                }
            }
            return;
        }
        // After moves a parent can come after its children: add each folder
        // to its parent once all of its own children are counted.
        let mut waiting = self.items.clone();
        let mut ready: Vec<usize> = (0..n).filter(|&i| waiting[i] == 0).collect();
        while let Some(i) = ready.pop() {
            if let Some(p) = self.parent(NodeId(i as u32)) {
                let size = self.meta[i].size;
                let p = p.index();
                self.meta[p].size = self.meta[p].size.wrapping_add(size);
                waiting[p] -= 1;
                if waiting[p] == 0 {
                    ready.push(p);
                }
            }
        }
    }
}

/// Borrowed view of the raw columns of a [`NodeStore`].
pub(crate) struct ColumnsRef<'a> {
    pub parent_kind: &'a [u32],
    pub name_start: &'a [u32],
    pub name_len: &'a [u16],
    pub meta: &'a [Meta],
    pub name_pool: &'a [u8],
}

fn push(pool: &mut Vec<u8>, bytes: &[u8]) -> (u32, u16) {
    let start = u32::try_from(pool.len()).expect("name pool exceeds 4 GiB");
    let len = u16::try_from(bytes.len()).expect("name longer than 65535 bytes");
    pool.extend_from_slice(bytes);
    (start, len)
}

fn slice(pool: &[u8], start: u32, len: u16) -> &[u8] {
    let start = start as usize;
    &pool[start..start + len as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (NodeStore, NodeId) {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/data", true, Meta::default());
        let docs = s.add(Some(root), b"Docs", true, Meta::default());
        let file = s.add(
            Some(docs),
            b"Report.PDF",
            false,
            Meta {
                size: 42,
                mtime_ns: 7,
            },
        );
        (s, file)
    }

    #[test]
    fn stores_names_and_metadata() {
        let (s, file) = sample();
        assert_eq!(s.len(), 3);
        assert_eq!(s.name(file), b"Report.PDF");
        assert_eq!(s.folded_name(file), b"report.pdf");
        assert_eq!(
            s.meta(file),
            Meta {
                size: 42,
                mtime_ns: 7
            }
        );
        assert!(!s.is_dir(file));
    }

    #[test]
    fn builds_full_path() {
        let (s, file) = sample();
        assert_eq!(s.path(file), PathBuf::from("/data/Docs/Report.PDF"));
    }

    #[test]
    fn folded_copy_is_shared_when_unchanged() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/", true, Meta::default());
        let lower = s.add(Some(root), b"readme.txt", false, Meta::default());
        let mixed = s.add(Some(root), b"ReadMe.txt", false, Meta::default());
        assert_eq!(s.folded_name(lower), b"readme.txt");
        assert_eq!(s.folded_name(mixed), b"readme.txt");
        assert_eq!(s.folded_pool.len(), b"readme.txt".len());
        assert_eq!(s.parent(lower), Some(root));
        assert!(s.is_dir(root) && !s.is_dir(lower));
    }

    fn columns_with_parents(parent_kind: Vec<u32>) -> Columns {
        let n = parent_kind.len();
        Columns {
            parent_kind,
            name_start: vec![0; n],
            name_len: vec![1; n],
            meta: vec![Meta::default(); n],
            name_pool: b"x".to_vec(),
        }
    }

    #[test]
    fn from_columns_accepts_valid_tree_and_rejects_cycles() {
        // 0 is a root, 1 -> 0, 2 -> 1
        assert!(
            NodeStore::from_columns(columns_with_parents(vec![DIR_BIT, 1 | DIR_BIT, 2])).is_ok()
        );
        // 0 -> 1 -> 0 has no root
        let cycle = NodeStore::from_columns(columns_with_parents(vec![DIR_BIT | 2, DIR_BIT | 1]));
        assert_eq!(cycle.err(), Some("parent cycle"));
        // parent id out of range
        assert!(NodeStore::from_columns(columns_with_parents(vec![0, 9])).is_err());
        // a file cannot be a parent
        assert!(NodeStore::from_columns(columns_with_parents(vec![0, 1])).is_err());
    }

    #[test]
    fn loading_recomputes_folder_sizes_in_any_id_order() {
        // 0: file in 3, 1: root, 2: file in 1, 3: folder in 1 (after its
        // child, as after a move). Saved folder sizes are not trusted.
        let mut c = columns_with_parents(vec![4, DIR_BIT, 2, 2 | DIR_BIT]);
        let sizes = [5, 1000, 7, 1000];
        for (m, size) in c.meta.iter_mut().zip(sizes) {
            m.size = size;
        }
        let s = NodeStore::from_columns(c).unwrap();
        let size = |i: u32| (s.meta(NodeId(i)).size, s.items(NodeId(i)));
        assert_eq!(size(1), (12, 2));
        assert_eq!(size(3), (5, 1));
        assert_eq!(size(0), (5, 0));

        // Parents first: the fast path.
        let mut c = columns_with_parents(vec![DIR_BIT, 1 | DIR_BIT, 2, 1]);
        c.meta[2].size = 3;
        c.meta[3].size = 4;
        let s = NodeStore::from_columns(c).unwrap();
        assert_eq!(s.meta(NodeId(0)).size, 7);
        assert_eq!(s.meta(NodeId(1)).size, 3);
    }

    #[test]
    fn compaction_keeps_folder_sizes() {
        let (mut s, file) = sample();
        let docs = s.parent(file).unwrap();
        let extra = s.add(
            Some(docs),
            b"x",
            false,
            Meta {
                size: 8,
                mtime_ns: 0,
            },
        );
        s.remove(file);
        let (c, remap) = s.compacted();
        let docs = remap[docs.index()].unwrap();
        assert_eq!((c.meta(docs).size, c.items(docs)), (8, 1));
        assert_eq!(c.meta(c.parent(docs).unwrap()).size, 8);
        assert!(remap[extra.index()].is_some());
    }

    #[test]
    fn remove_rename_and_compact() {
        let (mut s, file) = sample();
        let docs = s.parent(file).unwrap();
        let extra = s.add(Some(docs), b"Old.txt", false, Meta::default());
        s.set_name(extra, b"new.txt");
        s.set_parent(extra, s.parent(docs));
        assert_eq!(s.path(extra), PathBuf::from("/data/new.txt"));
        s.remove(file);
        assert!(!s.is_live(file));
        assert_eq!(s.live_count(), 3);
        assert!(s.garbage_bytes() >= b"Old.txt".len() + b"Report.PDF".len());

        let (c, remap) = s.compacted();
        assert_eq!(c.len(), 3);
        assert_eq!(remap[file.index()], None);
        let new_extra = remap[extra.index()].unwrap();
        assert_eq!(c.path(new_extra), PathBuf::from("/data/new.txt"));
        assert_eq!(c.garbage_bytes(), 0);
    }

    #[test]
    fn keeps_non_utf8_names() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/", true, Meta::default());
        let odd = s.add(Some(root), b"bad\xffname", false, Meta::default());
        assert_eq!(s.name(odd), b"bad\xffname");
        assert_eq!(s.path(odd).as_os_str().as_bytes(), b"/bad\xffname");
    }
}
