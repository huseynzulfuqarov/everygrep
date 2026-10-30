//! The searchable index: node storage plus a name-sorted order of all live nodes.

use std::cmp::Ordering;

use crate::fold::fold;
use crate::order::{Position, SortedIds};
use crate::store::{Meta, NodeId, NodeStore};

/// Node storage together with the order of live nodes sorted by name.
///
/// Nodes are ordered by folded name, then raw name, then parent. Searching walks
/// this order, so results come out sorted by name without a separate sort step,
/// and the same order finds a node by parent and name in `O(log n)`.
pub struct Index {
    store: NodeStore,
    by_name: SortedIds,
    /// Ids removed since the last [`take_removed`](Index::take_removed).
    removed_log: Vec<NodeId>,
}

/// Why a rename was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameError {
    /// The node has been removed.
    Removed,
    /// The new parent is the node itself or one of its descendants.
    IntoOwnSubtree,
    /// The node at the target is the node's own ancestor; replacing it would
    /// remove the node itself.
    OntoAncestor,
}

impl Index {
    /// Builds the index and its name order from a filled store.
    pub fn new(mut store: NodeStore) -> Self {
        store.shrink_to_fit();
        let mut ids: Vec<NodeId> = (0..store.len())
            .map(|i| NodeId(u32::try_from(i).expect("store ids fit in u32")))
            .filter(|&id| store.is_live(id))
            .collect();
        ids.sort_unstable_by(|&a, &b| compare(&store, a, b));
        Self {
            store,
            by_name: SortedIds::from_sorted(ids),
            removed_log: Vec::new(),
        }
    }

    /// Reassembles an index from a loaded store and name order. The order must
    /// contain every node exactly once and be sorted.
    pub(crate) fn from_parts(store: NodeStore, by_name: Vec<NodeId>) -> Result<Self, &'static str> {
        if by_name.len() != store.live_count() {
            return Err("name order has wrong length");
        }
        let mut seen = vec![false; store.len()];
        for id in &by_name {
            match seen.get_mut(id.index()) {
                Some(s) if !*s => *s = true,
                _ => return Err("name order is not a permutation"),
            }
        }
        if by_name
            .windows(2)
            .any(|w| compare(&store, w[0], w[1]) != Ordering::Less)
        {
            return Err("name order is not sorted");
        }
        Ok(Self {
            store,
            by_name: SortedIds::from_sorted(by_name),
            removed_log: Vec::new(),
        })
    }

    /// The underlying node storage.
    pub fn store(&self) -> &NodeStore {
        &self.store
    }

    /// Live node ids sorted by name.
    pub fn order(&self) -> &SortedIds {
        &self.by_name
    }

    /// Number of live nodes.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the index has no live nodes.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Approximate heap memory used by the index, in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.store.heap_bytes() + self.by_name.heap_bytes()
    }

    /// Finds the live node called `name` directly under `parent` (`None` for roots).
    pub fn find_child(&self, parent: Option<NodeId>, name: &[u8]) -> Option<NodeId> {
        self.position_of_key(&fold(name), name, parent)
            .ok()
            .map(|pos| self.by_name.blocks()[pos.0][pos.1])
    }

    /// Adds a node, or updates the metadata of an existing node with the same
    /// parent and name. If the existing node changed between file and folder, it
    /// is replaced (with its subtree). Returns the node id.
    pub fn insert(
        &mut self,
        parent: Option<NodeId>,
        name: &[u8],
        is_dir: bool,
        meta: Meta,
    ) -> NodeId {
        if let Some(existing) = self.find_child(parent, name) {
            if self.store.is_dir(existing) == is_dir {
                self.store.set_meta(existing, meta);
                return existing;
            }
            self.remove(existing);
        }
        let id = self.store.add(parent, name, is_dir, meta);
        self.insert_into_order(id);
        id
    }

    /// Replaces the metadata of a live node.
    pub fn set_meta(&mut self, id: NodeId, meta: Meta) {
        if self.store.is_live(id) {
            self.store.set_meta(id, meta);
        }
    }

    /// Removes a node and, for a folder, all nodes below it. Returns the ids of
    /// the removed nodes.
    pub fn remove(&mut self, id: NodeId) -> Vec<NodeId> {
        if !self.store.is_live(id) {
            return Vec::new();
        }
        let doomed = if self.store.is_dir(id) {
            self.subtree(id)
        } else {
            vec![id]
        };
        for &node in &doomed {
            let pos = self
                .position_of(node)
                .expect("live node is in the name order");
            self.by_name.remove_at(pos);
        }
        for &node in &doomed {
            self.store.remove(node);
        }
        self.removed_log.extend_from_slice(&doomed);
        doomed
    }

    /// Returns and clears the ids of all nodes removed since the last call,
    /// including nodes replaced by [`insert`](Self::insert) or
    /// [`rename`](Self::rename). Owners of per-node state (such as watches) use it
    /// to drop that state.
    pub fn take_removed(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.removed_log)
    }

    /// Moves and/or renames a node. A node already at the target is removed
    /// first, like `rename(2)` replacing its destination.
    pub fn rename(
        &mut self,
        id: NodeId,
        new_parent: Option<NodeId>,
        new_name: &[u8],
    ) -> Result<(), RenameError> {
        if !self.store.is_live(id) {
            return Err(RenameError::Removed);
        }
        let mut cur = new_parent;
        while let Some(p) = cur {
            if p == id {
                return Err(RenameError::IntoOwnSubtree);
            }
            cur = self.store.parent(p);
        }
        if let Some(target) = self.find_child(new_parent, new_name) {
            if target == id {
                return Ok(());
            }
            let mut up = self.store.parent(id);
            while let Some(a) = up {
                if a == target {
                    return Err(RenameError::OntoAncestor);
                }
                up = self.store.parent(a);
            }
            self.remove(target);
        }
        let pos = self
            .position_of(id)
            .expect("live node is in the name order");
        self.by_name.remove_at(pos);
        self.store.set_parent(id, new_parent);
        self.store.set_name(id, new_name);
        self.insert_into_order(id);
        Ok(())
    }

    /// Whether removed nodes or unused name bytes take enough room that
    /// [`compacted`](Self::compacted) is worth it.
    pub fn needs_compaction(&self) -> bool {
        let dead = self.store.len() - self.store.live_count();
        dead > 0 || self.store.garbage_bytes() > 0
    }

    /// Whether removed slots or unused name bytes make up more than a quarter of
    /// the store, and are enough to matter. A long-running watcher calls this
    /// after applying changes and then [`compact_in_place`](Self::compact_in_place).
    pub fn should_compact(&self) -> bool {
        const MIN_DEAD: usize = 10_000;
        const MIN_GARBAGE: usize = 1 << 20;
        let dead = self.store.len() - self.store.live_count();
        let garbage = self.store.garbage_bytes();
        (dead >= MIN_DEAD && dead * 4 > self.store.len())
            || (garbage >= MIN_GARBAGE && garbage * 4 > self.store.name_bytes())
    }

    /// Replaces the index with its compacted form and returns the map from old to
    /// new ids (`None` for removed nodes). Every id held outside the index must be
    /// translated with it. The removal log must have been taken before.
    pub fn compact_in_place(&mut self) -> Vec<Option<NodeId>> {
        debug_assert!(
            self.removed_log.is_empty(),
            "take_removed before compacting"
        );
        let (compact, remap) = self.compacted();
        *self = compact;
        remap
    }

    /// Returns a copy without removed nodes and unused name bytes, plus the map
    /// from old to new ids.
    pub fn compacted(&self) -> (Index, Vec<Option<NodeId>>) {
        let (store, remap) = self.store.compacted();
        // The remap keeps the relative order of ids, so parent keys keep their
        // order and the name order stays sorted.
        let ids: Vec<NodeId> = self
            .by_name
            .iter()
            .filter_map(|id| remap[id.index()])
            .collect();
        let index = Index {
            store,
            by_name: SortedIds::from_sorted(ids),
            removed_log: Vec::new(),
        };
        (index, remap)
    }

    fn insert_into_order(&mut self, id: NodeId) {
        let folded = self.store.folded_name(id).to_vec();
        let name = self.store.name(id).to_vec();
        match self.position_of_key(&folded, &name, self.store.parent(id)) {
            Ok(_) => unreachable!("parent and name are unique"),
            Err(pos) => self.by_name.insert_at(pos, id),
        }
    }

    fn position_of(&self, id: NodeId) -> Option<Position> {
        self.position_of_key(
            self.store.folded_name(id),
            self.store.name(id),
            self.store.parent(id),
        )
        .ok()
    }

    fn position_of_key(
        &self,
        folded: &[u8],
        name: &[u8],
        parent: Option<NodeId>,
    ) -> Result<Position, Position> {
        let parent_key = parent.map_or(0, |p| p.0 + 1);
        let store = &self.store;
        self.by_name.search_by(|id| {
            store
                .folded_name(id)
                .cmp(folded)
                .then_with(|| store.name(id).cmp(name))
                .then_with(|| store.parent_key(id).cmp(&parent_key))
        })
    }

    /// The live folder `root` and every live node below it, found through the
    /// child lists in time proportional to the subtree.
    fn subtree(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            out.push(node);
            stack.extend(self.store.children(node));
        }
        out
    }
}

fn compare(store: &NodeStore, a: NodeId, b: NodeId) -> Ordering {
    store
        .folded_name(a)
        .cmp(store.folded_name(b))
        .then_with(|| store.name(a).cmp(store.name(b)))
        .then_with(|| store.parent_key(a).cmp(&store.parent_key(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{EvalScratch, Query, QueryOptions};
    use crate::search::search;

    fn names(index: &Index) -> Vec<Vec<u8>> {
        index
            .order()
            .iter()
            .map(|id| index.store().name(id).to_vec())
            .collect()
    }

    #[test]
    fn order_is_case_insensitive_by_name() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        for name in [&b"beta"[..], b"Alpha", b"alpha", b"Gamma"] {
            s.add(Some(root), name, false, Meta::default());
        }
        let index = Index::new(s);
        let expected: Vec<Vec<u8>> = [&b"/r"[..], b"Alpha", b"alpha", b"beta", b"Gamma"]
            .iter()
            .map(|n| n.to_vec())
            .collect();
        assert_eq!(names(&index), expected);
    }

    #[test]
    fn insert_find_rename_remove() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let docs = index.insert(Some(root), b"Docs", true, Meta::default());
        let a = index.insert(Some(docs), b"a.txt", false, Meta::default());
        let b = index.insert(Some(root), b"a.txt", false, Meta::default());
        assert_ne!(a, b);
        assert_eq!(index.find_child(Some(docs), b"a.txt"), Some(a));
        assert_eq!(index.find_child(Some(root), b"a.txt"), Some(b));
        assert_eq!(
            index.insert(
                Some(docs),
                b"a.txt",
                false,
                Meta {
                    size: 5,
                    mtime_ns: 0
                }
            ),
            a
        );
        assert_eq!(index.store().meta(a).size, 5);

        // Renaming over an existing name replaces it.
        index.rename(a, Some(root), b"a.txt").unwrap();
        assert!(!index.store().is_live(b));
        assert_eq!(index.store().path(a), std::path::PathBuf::from("/r/a.txt"));
        assert_eq!(
            index.rename(root, Some(docs), b"x"),
            Err(RenameError::IntoOwnSubtree)
        );

        assert_eq!(index.remove(docs), vec![docs]);
        assert_eq!(index.len(), 2);
        assert_eq!(index.find_child(Some(root), b"Docs"), None);
    }

    #[test]
    fn rename_onto_own_ancestor_is_refused_and_removals_are_logged() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let a = index.insert(Some(root), b"a", true, Meta::default());
        let x = index.insert(Some(a), b"x", false, Meta::default());
        index.take_removed();
        assert_eq!(
            index.rename(x, Some(root), b"a"),
            Err(RenameError::OntoAncestor)
        );
        assert!(index.store().is_live(x) && index.store().is_live(a));

        // A file replaced by a folder of the same name is logged as removed.
        let f = index.insert(Some(root), b"f", false, Meta::default());
        let d = index.insert(Some(root), b"f", true, Meta::default());
        assert_ne!(f, d);
        assert_eq!(index.take_removed(), vec![f]);
        // Renaming over an existing node logs the replaced node.
        let y = index.insert(Some(root), b"y", false, Meta::default());
        index.rename(x, Some(root), b"y").unwrap();
        assert_eq!(index.take_removed(), vec![y]);
        assert!(index.take_removed().is_empty());
    }

    #[test]
    fn removing_a_folder_removes_its_subtree() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let top = index.insert(Some(root), b"top", true, Meta::default());
        let mid = index.insert(Some(top), b"mid", true, Meta::default());
        index.insert(Some(mid), b"deep.txt", false, Meta::default());
        index.insert(Some(root), b"keep.txt", false, Meta::default());
        assert_eq!(index.remove(top).len(), 3);
        assert_eq!(names(&index), vec![b"/r".to_vec(), b"keep.txt".to_vec()]);
    }

    #[test]
    fn compacting_in_place_keeps_content_and_maps_ids() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let mut files = Vec::new();
        for i in 0..30_000 {
            files.push(index.insert(
                Some(root),
                format!("f{i}.txt").as_bytes(),
                false,
                Meta::default(),
            ));
        }
        assert!(!index.should_compact());
        for &f in files
            .iter()
            .step_by(3)
            .chain(files.iter().skip(1).step_by(3))
        {
            index.remove(f);
        }
        index.take_removed();
        assert!(index.should_compact());
        let kept = files[2];
        let before: Vec<std::path::PathBuf> = index
            .order()
            .iter()
            .map(|id| index.store().path(id))
            .collect();
        let remap = index.compact_in_place();
        let after: Vec<std::path::PathBuf> = index
            .order()
            .iter()
            .map(|id| index.store().path(id))
            .collect();
        assert_eq!(before, after);
        assert_eq!(index.store().len(), index.len());
        assert!(!index.should_compact());
        assert_eq!(remap[files[0].index()], None);
        let new_kept = remap[kept.index()].unwrap();
        assert_eq!(index.store().name(new_kept), b"f2.txt");
        check_consistent(&index);
    }

    /// Small deterministic generator so the random test is reproducible.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    #[test]
    fn random_operations_keep_order_and_search_consistent() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let mut dirs = vec![root];
        let words = [
            "Alpha", "beta", "GAMMA", "delta", "ÉTÉ", "zz", "a.b", "Photo",
        ];
        for step in 0..4000 {
            let live_dirs: Vec<NodeId> = dirs
                .iter()
                .copied()
                .filter(|&d| index.store().is_live(d))
                .collect();
            let parent = live_dirs[rng.next(live_dirs.len())];
            let name = format!("{}{}", words[rng.next(words.len())], rng.next(40));
            match rng.next(10) {
                0..=4 => {
                    index.insert(Some(parent), name.as_bytes(), false, Meta::default());
                }
                5 | 6 => {
                    dirs.push(index.insert(Some(parent), name.as_bytes(), true, Meta::default()))
                }
                7 => {
                    let ids: Vec<NodeId> = index.order().iter().collect();
                    let victim = ids[rng.next(ids.len())];
                    if victim != root {
                        index.remove(victim);
                    }
                }
                _ => {
                    let ids: Vec<NodeId> = index.order().iter().collect();
                    let node = ids[rng.next(ids.len())];
                    if node != root {
                        let _ = index.rename(node, Some(parent), name.as_bytes());
                    }
                }
            }
            if step % 500 == 499 {
                check_consistent(&index);
            }
        }
        check_consistent(&index);
        let (compact, _) = index.compacted();
        check_consistent(&compact);
        assert_eq!(compact.len(), index.len());
        assert!(!compact.needs_compaction());
    }

    /// Random inserts (with sizes), size changes, kind changes, removals,
    /// moves and renames, with compactions in between: after every step each
    /// folder's total and item count equal a recount from scratch.
    #[test]
    fn random_operations_keep_folder_sizes_exact() {
        for seed in [1u64, 0xdead_beef, 0x0123_4567_89ab_cdef] {
            let mut rng = Rng(seed);
            let mut index = Index::new(NodeStore::new());
            let roots = [
                index.insert(None, b"/r", true, Meta::default()),
                index.insert(None, b"/s", true, Meta::default()),
            ];
            let mut dirs = roots.to_vec();
            let size = |rng: &mut Rng| match rng.next(4) {
                0 => 0,
                1 => rng.next(100) as u64,
                2 => rng.next(1 << 20) as u64,
                _ => (rng.next(1 << 20) as u64) << 20,
            };
            for step in 0..1500 {
                dirs.retain(|&d| index.store().is_live(d));
                let parent = dirs[rng.next(dirs.len())];
                // Few names, so inserts often hit an existing node.
                let name = format!("n{}", rng.next(12));
                let ids: Vec<NodeId> = index.order().iter().collect();
                let some = ids[rng.next(ids.len())];
                match rng.next(12) {
                    0..=3 => {
                        let meta = Meta {
                            size: size(&mut rng),
                            mtime_ns: 1,
                        };
                        index.insert(Some(parent), name.as_bytes(), false, meta);
                    }
                    4 | 5 => {
                        let meta = Meta {
                            size: size(&mut rng),
                            mtime_ns: 2,
                        };
                        dirs.push(index.insert(Some(parent), name.as_bytes(), true, meta));
                    }
                    6 | 7 => {
                        let meta = Meta {
                            size: size(&mut rng),
                            mtime_ns: 3,
                        };
                        index.set_meta(some, meta);
                    }
                    8 => {
                        if !roots.contains(&some) {
                            index.remove(some);
                        }
                    }
                    _ => {
                        if !roots.contains(&some) {
                            let _ = index.rename(some, Some(parent), name.as_bytes());
                        }
                    }
                }
                check_folder_sizes(index.store());
                if step % 300 == 299 {
                    index.take_removed();
                    let remap = index.compact_in_place();
                    dirs = dirs.iter().filter_map(|d| remap[d.index()]).collect();
                    let new_roots: Vec<NodeId> =
                        roots.iter().map(|r| remap[r.index()].unwrap()).collect();
                    assert_eq!(new_roots, roots, "roots keep the lowest ids");
                    check_consistent(&index);
                }
            }
            check_consistent(&index);
            // Saving and loading recomputes the same totals, also when some
            // folders were created after the files moved into them.
            let path = std::env::temp_dir().join(format!(
                "everygrep-index-sizes-{seed}-{}.bin",
                std::process::id()
            ));
            crate::persist::save(&index, &[], &path).unwrap();
            let loaded = crate::persist::load(&path).unwrap().index;
            std::fs::remove_file(&path).unwrap();
            check_consistent(&loaded);
            let totals = |index: &Index| -> Vec<(std::path::PathBuf, u64, u32)> {
                let mut v: Vec<_> = index
                    .order()
                    .iter()
                    .map(|id| {
                        let s = index.store();
                        (s.path(id), s.meta(id).size, s.items(id))
                    })
                    .collect();
                v.sort();
                v
            };
            assert_eq!(totals(&loaded), totals(&index));
        }
    }

    #[test]
    fn folder_sizes_follow_each_kind_of_change() {
        let mut index = Index::new(NodeStore::new());
        let meta = |size| Meta { size, mtime_ns: 0 };
        let root = index.insert(None, b"/r", true, meta(0));
        let a = index.insert(Some(root), b"a", true, meta(999));
        let b = index.insert(Some(a), b"b", true, meta(0));
        let f = index.insert(Some(b), b"f", false, meta(100));
        let g = index.insert(Some(a), b"g", false, meta(10));
        let size = |index: &Index, id| index.store().meta(id).size;
        // A new folder starts empty, whatever size it was given.
        assert_eq!((size(&index, a), size(&index, root)), (110, 110));
        assert_eq!(index.store().items(a), 2);
        // Size change.
        index.set_meta(f, meta(300));
        assert_eq!((size(&index, b), size(&index, root)), (300, 310));
        // A folder's own metadata keeps its total.
        index.set_meta(
            a,
            Meta {
                size: 0,
                mtime_ns: 9,
            },
        );
        assert_eq!((size(&index, a), index.store().meta(a).mtime_ns), (310, 9));
        // Move out of a folder, then rename in place.
        index.rename(f, Some(root), b"f2").unwrap();
        assert_eq!((size(&index, b), size(&index, a)), (0, 10));
        assert_eq!(size(&index, root), 310);
        // Moving a folder moves its total.
        index.rename(a, None, b"/x").unwrap();
        assert_eq!(size(&index, root), 300);
        assert_eq!(index.store().items(root), 1);
        // A file replaced by a folder of the same name.
        let d = index.insert(Some(root), b"f2", true, meta(5));
        assert_eq!((size(&index, d), size(&index, root)), (0, 0));
        // Removing a folder takes its whole subtree.
        let h = index.insert(Some(b), b"h", false, meta(7));
        assert_eq!(size(&index, a), 17);
        index.remove(b);
        assert_eq!((size(&index, a), index.store().items(a)), (10, 1));
        assert!(!index.store().is_live(h));
        check_folder_sizes(index.store());
        assert_eq!(index.store().items(g), 0);
    }

    /// Recounts every folder's total size and item count from scratch and
    /// compares them with the kept ones.
    fn check_folder_sizes(store: &NodeStore) {
        let n = store.len();
        let mut total = vec![0u64; n];
        let mut items = vec![0u32; n];
        for i in 0..n {
            let id = NodeId(i as u32);
            if !store.is_live(id) {
                continue;
            }
            if let Some(p) = store.parent(id) {
                items[p.index()] += 1;
            }
            if store.is_dir(id) {
                continue;
            }
            let size = store.meta(id).size;
            let mut up = store.parent(id);
            while let Some(p) = up {
                total[p.index()] = total[p.index()].wrapping_add(size);
                up = store.parent(p);
            }
        }
        for i in 0..n {
            let id = NodeId(i as u32);
            if store.is_live(id) {
                assert_eq!(store.items(id), items[i], "items of {:?}", store.path(id));
                if store.is_dir(id) {
                    assert_eq!(
                        store.meta(id).size,
                        total[i],
                        "size of {:?}",
                        store.path(id)
                    );
                }
            }
        }
    }

    fn check_consistent(index: &Index) {
        let store = index.store();
        check_folder_sizes(store);
        let ids: Vec<NodeId> = index.order().iter().collect();
        let live: Vec<NodeId> = (0..store.len())
            .map(|i| NodeId(i as u32))
            .filter(|&id| store.is_live(id))
            .collect();
        assert_eq!(ids.len(), live.len());
        assert!(
            ids.windows(2)
                .all(|w| compare(store, w[0], w[1]) == Ordering::Less)
        );
        for &id in &live {
            if let Some(p) = store.parent(id) {
                assert!(store.is_live(p), "live node under removed parent");
                assert_eq!(
                    store.children(p).filter(|&c| c == id).count(),
                    1,
                    "child list"
                );
            }
            assert!(
                store
                    .children(id)
                    .all(|c| store.is_live(c) && store.parent(c) == Some(id))
            );
            assert_eq!(index.find_child(store.parent(id), store.name(id)), Some(id));
        }
        let m = Query::parse("a", QueryOptions::default()).unwrap();
        let mut scratch = EvalScratch::default();
        let expected: Vec<NodeId> = ids
            .iter()
            .copied()
            .filter(|&id| m.matches(store, id, &mut scratch))
            .collect();
        assert_eq!(search(index, &m, 4), expected);
    }
}
