//! The searchable index: node storage plus a name-sorted order of all live nodes.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::collate::collate;
use crate::fold::fold;
use crate::order::{Position, SortedIds};
use crate::store::{Meta, NodeId, NodeStore};

/// Node storage together with the order of live nodes sorted by name.
///
/// Nodes are ordered by folded name in natural, diacritic-folded order (see
/// [`collate`]), then by raw name, then by parent. Searching walks
/// this order, so results come out sorted by name without a separate sort step,
/// and the same order finds a node by parent and name in `O(log n)`.
pub struct Index {
    store: NodeStore,
    by_name: SortedIds,
    /// Ids removed since the last [`take_removed`](Index::take_removed).
    removed_log: Vec<NodeId>,
    /// Roots whose folder is not reachable now (a drive that was removed).
    /// Their nodes stay searchable but are not read, watched or caught up.
    offline: Vec<NodeId>,
    /// Nodes whose details read from the file system may have changed.
    changes: ChangeLog,
    /// Different for every index built; see [`stamp`](Index::stamp).
    stamp: u64,
}

/// Most entries the change log holds; one more resets it.
const CHANGE_LOG_MAX: usize = 1 << 16;

/// Ids of nodes whose details from the file system (attributes, creation
/// time; see [`crate::details`]) may differ from what was read before, for
/// readers that keep such details (see
/// [`DetailCache`](crate::sort::DetailCache)).
///
/// Each entry has a sequence number. A reader remembers the number it has
/// seen up to and asks for what came after it. The log is never drained by
/// a reader, so several can follow it; instead it is reset when it grows
/// past [`CHANGE_LOG_MAX`] or when whole drives change. A reset skips one
/// number, so every reader that was behind or exactly up to date learns
/// that it has to forget all it holds. At most 256 KiB (4 bytes per entry).
#[derive(Default)]
struct ChangeLog {
    /// Sequence number of `ids[0]`.
    start: u64,
    ids: Vec<NodeId>,
}

impl ChangeLog {
    fn push(&mut self, id: NodeId) {
        if self.ids.len() >= CHANGE_LOG_MAX {
            self.reset();
        }
        self.ids.push(id);
    }

    fn extend(&mut self, ids: &[NodeId]) {
        if self.ids.len() + ids.len() > CHANGE_LOG_MAX {
            self.reset();
            if ids.len() > CHANGE_LOG_MAX {
                // A reset says any node may have changed, and that covers
                // these ids too.
                return;
            }
        }
        self.ids.extend_from_slice(ids);
    }

    fn reset(&mut self) {
        self.start = self.end() + 1;
        self.ids.clear();
    }

    fn end(&self) -> u64 {
        self.start + self.ids.len() as u64
    }

    fn since(&self, seq: u64) -> Option<&[NodeId]> {
        let from = usize::try_from(seq.checked_sub(self.start)?).ok()?;
        self.ids.get(from..)
    }
}

/// The stamp of the next index built.
fn next_stamp() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, AtomicOrdering::Relaxed)
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
        sort_by_name(&store, &mut ids);
        Self {
            store,
            by_name: SortedIds::from_sorted(ids),
            removed_log: Vec::new(),
            offline: Vec::new(),
            changes: ChangeLog::default(),
            stamp: next_stamp(),
        }
    }

    /// Reassembles an index from a loaded store and name order. The order must
    /// contain every node exactly once. An order that is not sorted by the
    /// current rules (saved by an older version) is sorted again; the flag
    /// returned with the index tells whether that happened.
    pub(crate) fn from_parts(
        store: NodeStore,
        mut by_name: Vec<NodeId>,
    ) -> Result<(Self, bool), &'static str> {
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
        let reordered = by_name
            .windows(2)
            .any(|w| compare(&store, w[0], w[1]) != Ordering::Less);
        if reordered {
            sort_by_name(&store, &mut by_name);
            if by_name
                .windows(2)
                .any(|w| compare(&store, w[0], w[1]) == Ordering::Equal)
            {
                return Err("two nodes with the same parent and name");
            }
        }
        let index = Self {
            store,
            by_name: SortedIds::from_sorted(by_name),
            removed_log: Vec::new(),
            offline: Vec::new(),
            changes: ChangeLog::default(),
            stamp: next_stamp(),
        };
        Ok((index, reordered))
    }

    /// The live roots, in name order.
    pub fn roots(&self) -> Vec<NodeId> {
        self.by_name
            .iter()
            .filter(|&id| self.store.parent(id).is_none())
            .collect()
    }

    /// Marks a root as offline (its folder cannot be reached, as for a
    /// removed drive) or online again. Offline roots keep their nodes; scans,
    /// catch-ups and watchers leave them alone.
    ///
    /// A change resets the change log (see
    /// [`changed_since`](Self::changed_since)): a drive that comes back may
    /// have been changed elsewhere, with nothing to say which entries.
    pub fn set_offline(&mut self, root: NodeId, offline: bool) {
        debug_assert!(self.store.parent(root).is_none(), "only roots go offline");
        let was = self.offline.contains(&root);
        self.offline.retain(|&r| r != root);
        if offline && self.store.is_live(root) {
            self.offline.push(root);
        }
        if was != self.offline.contains(&root) {
            self.changes.reset();
        }
    }

    /// A number different for every index built (also by
    /// [`compacted`](Self::compacted), [`compact_in_place`](Self::compact_in_place)
    /// and [`extract`](Self::extract)) and never reused in the process. State
    /// kept per node id outside the index belongs to the index with this
    /// stamp; under another stamp the same ids may mean other nodes.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    /// The sequence number the next entry of the change log gets. Pass it to
    /// [`changed_since`](Self::changed_since) later.
    pub fn change_seq(&self) -> u64 {
        self.changes.end()
    }

    /// Ids of nodes whose file system details (attributes, creation time,
    /// see [`crate::details`]) may have changed since `seq` was
    /// [`change_seq`](Self::change_seq), in no particular order and possibly
    /// repeated. `None` when the log was reset since: then any node may have
    /// changed.
    ///
    /// Logged are nodes given new metadata ([`set_meta`](Self::set_meta),
    /// [`insert`](Self::insert) of an existing name), renamed or moved, or
    /// removed, and those passed to [`note_changed`](Self::note_changed) or,
    /// with all nodes below them, to
    /// [`note_subtree_changed`](Self::note_subtree_changed). New nodes are not logged: their ids were never handed out before.
    /// The log resets when a root goes offline or online, and when it would
    /// hold more than 65 536 entries.
    pub fn changed_since(&self, seq: u64) -> Option<&[NodeId]> {
        self.changes.since(seq)
    }

    /// Logs `id` as changed (see [`changed_since`](Self::changed_since))
    /// without changing anything else, for changes only the caller knows of.
    pub fn note_changed(&mut self, id: NodeId) {
        self.changes.push(id);
    }

    /// Logs a live folder and every node below it as changed, for a change
    /// of the folder that can change what can be read below it (its
    /// permissions, or a move to another folder). A subtree larger than the log resets it, which says
    /// that any node may have changed.
    pub fn note_subtree_changed(&mut self, dir: NodeId) {
        if !self.store.is_live(dir) {
            return;
        }
        let mut ids = Vec::new();
        let mut stack = vec![dir];
        while let Some(node) = stack.pop() {
            if ids.len() >= CHANGE_LOG_MAX {
                self.changes.reset();
                return;
            }
            ids.push(node);
            stack.extend(self.store.children(node));
        }
        self.changes.extend(&ids);
    }

    /// Roots marked offline.
    pub fn offline_roots(&self) -> &[NodeId] {
        &self.offline
    }

    /// Whether a node lies below an offline root (or is one).
    pub fn is_offline(&self, id: NodeId) -> bool {
        !self.offline.is_empty() && self.offline.contains(&self.store.root_of(id))
    }

    /// A new index with only the given roots and the nodes below them, in
    /// the same name order, plus the map from this index's ids to the new
    /// ones. Offline marks of those roots are kept.
    pub fn extract(&self, roots: &[NodeId]) -> (Index, Vec<Option<NodeId>>) {
        let mut keep = vec![false; self.store.len()];
        for &root in roots {
            if self.store.is_live(root) && self.store.parent(root).is_none() {
                for node in self.subtree(root) {
                    keep[node.index()] = true;
                }
            }
        }
        let (store, remap) = self.store.compacted_where(|id| keep[id.index()]);
        let ids: Vec<NodeId> = self
            .by_name
            .iter()
            .filter_map(|id| remap[id.index()])
            .collect();
        let offline = self
            .offline
            .iter()
            .filter_map(|r| remap[r.index()])
            .collect();
        let index = Index {
            store,
            by_name: SortedIds::from_sorted(ids),
            removed_log: Vec::new(),
            offline,
            changes: ChangeLog::default(),
            stamp: next_stamp(),
        };
        (index, remap)
    }

    /// Adds all nodes of `other` (built separately, for example a drive's
    /// saved index) and returns the new ids of its roots. Existing ids keep
    /// their meaning; the name order is merged in one pass. Offline marks of
    /// `other` are taken over.
    ///
    /// # Panics
    ///
    /// Panics if a root of `other` has the same name as a root here, or if
    /// `other` has removed nodes.
    pub fn graft(&mut self, other: &Index) -> Vec<NodeId> {
        let other_roots = other.roots();
        for &r in &other_roots {
            assert!(
                self.find_child(None, other.store.name(r)).is_none(),
                "graft: a root of that name exists"
            );
        }
        let base = self.store.append(&other.store);
        let shift = |id: NodeId| NodeId(base + id.0);
        let mut merged = Vec::with_capacity(self.by_name.len() + other.by_name.len());
        {
            let store = &self.store;
            let mut mine = self.by_name.iter().peekable();
            let mut theirs = other.by_name.iter().map(shift).peekable();
            loop {
                let next = match (mine.peek(), theirs.peek()) {
                    (Some(&a), Some(&b)) => {
                        if compare(store, a, b) == Ordering::Greater {
                            theirs.next()
                        } else {
                            mine.next()
                        }
                    }
                    (Some(_), None) => mine.next(),
                    (None, Some(_)) => theirs.next(),
                    (None, None) => break,
                };
                merged.extend(next);
            }
        }
        self.by_name = SortedIds::from_sorted(merged);
        self.offline.extend(other.offline.iter().map(|&r| shift(r)));
        other_roots.into_iter().map(shift).collect()
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
                self.changes.push(existing);
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
            self.changes.push(id);
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
        self.offline.retain(|&r| r != id);
        self.removed_log.extend_from_slice(&doomed);
        self.changes.extend(&doomed);
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
        let moved = self.store.parent(id) != new_parent;
        self.store.set_parent(id, new_parent);
        self.store.set_name(id, new_name);
        self.insert_into_order(id);
        // The name decides the hidden attribute. A folder moved elsewhere
        // can take all entries below it where they cannot be read (a folder
        // without search permission for this user, moved there by another).
        if moved && self.store.is_dir(id) {
            self.note_subtree_changed(id);
        } else {
            self.changes.push(id);
        }
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
        let offline = self
            .offline
            .iter()
            .filter_map(|r| remap[r.index()])
            .collect();
        let index = Index {
            store,
            by_name: SortedIds::from_sorted(ids),
            removed_log: Vec::new(),
            offline,
            changes: ChangeLog::default(),
            stamp: next_stamp(),
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
            collate(store.folded_name(id), folded)
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

/// Lists shorter than this are sorted on one thread.
const PARALLEL_SORT_MIN: usize = 64 * 1024;

/// Sorts `ids` into the name order, on one thread per CPU for long lists:
/// each sorts a run, then pairs of runs are merged, also in parallel.
fn sort_by_name(store: &NodeStore, ids: &mut [NodeId]) {
    let cmp = |a: &NodeId, b: &NodeId| compare(store, *a, *b);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    if threads == 1 || ids.len() < PARALLEL_SORT_MIN {
        ids.sort_unstable_by(cmp);
        return;
    }
    let run = ids.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for chunk in ids.chunks_mut(run) {
            scope.spawn(move || chunk.sort_unstable_by(cmp));
        }
    });
    let mut merged = vec![NodeId(0); ids.len()];
    let mut width = run;
    while width < ids.len() {
        std::thread::scope(|scope| {
            for (from, to) in ids.chunks(2 * width).zip(merged.chunks_mut(2 * width)) {
                scope.spawn(move || merge_runs(from, width, to, cmp));
            }
        });
        ids.copy_from_slice(&merged);
        width *= 2;
    }
}

/// Merges the sorted runs `from[..mid]` and `from[mid..]` into `to`.
fn merge_runs(
    from: &[NodeId],
    mid: usize,
    to: &mut [NodeId],
    cmp: impl Fn(&NodeId, &NodeId) -> Ordering,
) {
    let (left, right) = from.split_at(mid.min(from.len()));
    let (mut i, mut j) = (0, 0);
    for slot in to.iter_mut() {
        let take_right =
            i == left.len() || (j < right.len() && cmp(&right[j], &left[i]) == Ordering::Less);
        if take_right {
            *slot = right[j];
            j += 1;
        } else {
            *slot = left[i];
            i += 1;
        }
    }
}

fn compare(store: &NodeStore, a: NodeId, b: NodeId) -> Ordering {
    collate(store.folded_name(a), store.folded_name(b))
        .then_with(|| store.name(a).cmp(store.name(b)))
        .then_with(|| store.parent_key(a).cmp(&store.parent_key(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{EvalScratch, Query, QueryOptions};
    use crate::search::search;
    use std::path::Path;

    fn names(index: &Index) -> Vec<Vec<u8>> {
        index
            .order()
            .iter()
            .map(|id| index.store().name(id).to_vec())
            .collect()
    }

    #[test]
    fn order_is_natural_and_ignores_case_and_diacritics() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        let names_in = [
            "beta", "Alpha", "alpha", "Gamma", "file10", "file2", "Çay", "cay", "Şəkil", "Sekil",
            "zoo", "Ölçü", "a-b", "ab",
        ];
        for name in names_in {
            s.add(Some(root), name.as_bytes(), false, Meta::default());
        }
        let index = Index::new(s);
        let expected: Vec<Vec<u8>> = [
            "/r", "a-b", "ab", "Alpha", "alpha", "beta", "cay", "Çay", "file2", "file10", "Gamma",
            "Ölçü", "Sekil", "Şəkil", "zoo",
        ]
        .iter()
        .map(|n| n.as_bytes().to_vec())
        .collect();
        assert_eq!(names(&index), expected);
    }

    #[test]
    fn long_lists_sort_on_several_threads_like_on_one() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        let dirs: Vec<NodeId> = (0..300)
            .map(|d| {
                s.add(
                    Some(root),
                    format!("d{d}").as_bytes(),
                    true,
                    Meta::default(),
                )
            })
            .collect();
        let words = ["Çay", "cay", "file", "a-b", "ab", "Ölçü", "z"];
        for i in 0..(PARALLEL_SORT_MIN * 2 + 77) {
            let h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40;
            let name = format!("{}{}", words[i % words.len()], h % 5000);
            s.add(
                Some(dirs[i % dirs.len()]),
                name.as_bytes(),
                false,
                Meta::default(),
            );
        }
        let index = Index::new(s);
        let ids: Vec<NodeId> = index.order().iter().collect();
        let mut serial = ids.clone();
        serial.sort_unstable_by(|&a, &b| compare(index.store(), a, b));
        assert_eq!(ids, serial);
        check_consistent(&index);
    }

    #[test]
    fn names_that_collate_equal_stay_apart() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let sub = index.insert(Some(root), b"sub", true, Meta::default());
        let names_in: [&[u8]; 4] = [b"a-b", b"ab", b"AB", "äb".as_bytes()];
        let mut made = Vec::new();
        for parent in [root, sub] {
            for name in names_in {
                made.push((
                    parent,
                    name,
                    index.insert(Some(parent), name, false, Meta::default()),
                ));
            }
        }
        check_consistent(&index);
        assert_eq!(index.len(), 2 + 8);
        for &(parent, name, id) in &made {
            assert_eq!(index.find_child(Some(parent), name), Some(id));
        }
        // Renaming to a name that collates equal to a neighbour keeps both.
        let (_, _, ab) = made[1];
        index.rename(ab, Some(root), b"a'b").unwrap();
        check_consistent(&index);
        assert_eq!(index.find_child(Some(root), b"a'b"), Some(ab));
        assert_eq!(index.find_child(Some(root), b"ab"), None);
        assert_eq!(index.find_child(Some(root), b"a-b"), Some(made[0].2));
        index.remove(made[0].2);
        check_consistent(&index);
        assert_eq!(index.find_child(Some(root), b"a'b"), Some(ab));
    }

    #[test]
    fn an_unsorted_saved_order_is_sorted_again() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        for name in ["file10", "file2", "Zoo", "Çay"] {
            s.add(Some(root), name.as_bytes(), false, Meta::default());
        }
        let fresh = Index::new(s.compacted().0);
        let backwards: Vec<NodeId> = (0..5).rev().map(NodeId).collect();
        let (index, reordered) = Index::from_parts(s.compacted().0, backwards).unwrap();
        assert!(reordered);
        assert_eq!(names(&index), names(&fresh));
        let sorted: Vec<NodeId> = fresh.order().iter().collect();
        let (_, reordered) = Index::from_parts(s.compacted().0, sorted).unwrap();
        assert!(!reordered);

        // Not a permutation, or two nodes with one name in one folder.
        let short = vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3), NodeId(3)];
        assert!(Index::from_parts(s.compacted().0, short).is_err());
        s.add(Some(root), b"Zoo", false, Meta::default());
        let all: Vec<NodeId> = (0..6).map(NodeId).collect();
        assert_eq!(
            Index::from_parts(s, all).err(),
            Some("two nodes with the same parent and name")
        );
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

    fn tree(root: &[u8], files: &[&[u8]]) -> Index {
        let mut s = NodeStore::new();
        let r = s.add(None, root, true, Meta::default());
        let sub = s.add(Some(r), b"Sub", true, Meta::default());
        for (i, f) in files.iter().enumerate() {
            s.add(
                Some(if i % 2 == 0 { r } else { sub }),
                f,
                false,
                Meta {
                    size: 10,
                    mtime_ns: 1,
                },
            );
        }
        Index::new(s)
    }

    fn sorted_like_new(index: &Index) {
        // The order equals a fresh sort of the same store.
        let ids: Vec<NodeId> = index.order().iter().collect();
        let mut fresh = ids.clone();
        fresh.sort_by(|&a, &b| compare(index.store(), a, b));
        assert_eq!(ids, fresh);
        assert_eq!(ids.len(), index.store().live_count());
    }

    #[test]
    fn graft_merges_another_index_and_extract_takes_it_out() {
        let mut main = tree(b"/home/u", &[b"a.txt", b"b.txt", b"Zed.md", b"same"]);
        let drive = tree(
            b"/run/media/u/USB",
            &[b"b.txt", b"c.txt", b"same", b"A.TXT"],
        );
        let before: Vec<Vec<u8>> = names(&main);
        let roots = main.graft(&drive);
        assert_eq!(roots.len(), 1);
        assert_eq!(main.store().name(roots[0]), b"/run/media/u/USB");
        assert_eq!(main.len(), 6 + 6);
        sorted_like_new(&main);
        // Sizes and child lists came along.
        assert_eq!(main.store().meta(roots[0]).size, 40);
        assert_eq!(main.store().items(roots[0]), 3);
        let usb_b = main.find_child(Some(roots[0]), b"b.txt").unwrap();
        assert_eq!(
            main.store().path(usb_b),
            Path::new("/run/media/u/USB/b.txt")
        );

        // Taking the drive out again gives the same nodes; the rest stays.
        let (out, _) = main.extract(&[roots[0]]);
        assert_eq!(names(&out), names(&drive));
        sorted_like_new(&out);
        let home = main.find_child(None, b"/home/u").unwrap();
        let (home_only, _) = main.extract(&[home]);
        assert_eq!(names(&home_only), before);
        assert_eq!(home_only.store().meta(NodeId(0)).size, 40);
    }

    #[test]
    #[should_panic(expected = "a root of that name exists")]
    fn graft_refuses_a_second_root_of_the_same_name() {
        let mut main = tree(b"/x", &[b"a"]);
        main.graft(&tree(b"/x", &[b"b"]));
    }

    #[test]
    fn offline_roots_survive_compaction_and_go_with_removal() {
        let mut main = tree(b"/home/u", &[b"a.txt", b"b.txt"]);
        let usb = main.graft(&tree(b"/media/u/USB", &[b"c.txt", b"d.txt"]))[0];
        main.set_offline(usb, true);
        let c = main.find_child(Some(usb), b"c.txt").unwrap();
        let home = main.find_child(None, b"/home/u").unwrap();
        assert!(main.is_offline(usb) && main.is_offline(c));
        assert!(!main.is_offline(home));
        // Remove something before the drive so ids shift on compaction.
        let a = main.find_child(Some(home), b"a.txt").unwrap();
        main.remove(a);
        main.take_removed();
        let remap = main.compact_in_place();
        let usb = remap[usb.index()].unwrap();
        assert_eq!(main.offline_roots(), [usb]);
        let (out, map) = main.extract(&[usb]);
        assert_eq!(out.offline_roots(), [map[usb.index()].unwrap()]);
        main.set_offline(usb, false);
        assert!(main.offline_roots().is_empty());
        main.set_offline(usb, true);
        main.remove(usb);
        assert!(main.offline_roots().is_empty());
    }

    /// Ids logged since `seq`, sorted, or `None` when the log was reset.
    fn changed(index: &Index, seq: u64) -> Option<Vec<NodeId>> {
        index.changed_since(seq).map(|ids| {
            let mut ids = ids.to_vec();
            ids.sort_unstable();
            ids
        })
    }

    #[test]
    fn detail_changes_are_logged() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let d = index.insert(Some(root), b"d", true, Meta::default());
        let x = index.insert(Some(d), b"x", false, Meta::default());
        let y = index.insert(Some(root), b"y", false, Meta::default());
        let seq = index.change_seq();
        // New nodes are not logged: no reader can know their ids yet.
        assert_eq!(changed(&index, seq), Some(vec![]));

        index.set_meta(y, Meta::default());
        assert_eq!(changed(&index, seq), Some(vec![y]));
        let seq = index.change_seq();
        // An existing node found again by insert gets new metadata.
        index.insert(Some(root), b"y", false, Meta::default());
        assert_eq!(changed(&index, seq), Some(vec![y]));
        let seq = index.change_seq();
        index.rename(x, Some(root), b".x").unwrap();
        assert_eq!(changed(&index, seq), Some(vec![x]));
        let seq = index.change_seq();
        // Renaming over a node logs the moved node and the replaced one.
        index.rename(x, Some(root), b"y").unwrap();
        assert_eq!(changed(&index, seq), Some(vec![x, y]));
        let seq = index.change_seq();
        let z = index.insert(Some(d), b"z", false, Meta::default());
        index.remove(d);
        assert_eq!(changed(&index, seq), Some(vec![d, z]));
        let seq = index.change_seq();
        index.note_changed(root);
        assert_eq!(changed(&index, seq), Some(vec![root]));
        assert_eq!(changed(&index, index.change_seq()), Some(vec![]));
    }

    #[test]
    fn change_log_resets_when_drives_come_and_go_or_it_grows_too_long() {
        let mut main = tree(b"/home/u", &[b"a.txt"]);
        let usb = main.graft(&tree(b"/media/u/USB", &[b"c.txt"]))[0];
        let seq = main.change_seq();
        main.set_offline(usb, true);
        assert_eq!(main.changed_since(seq), None);
        let seq = main.change_seq();
        assert_eq!(changed(&main, seq), Some(vec![]));
        // Already offline: nothing changes.
        main.set_offline(usb, true);
        assert_eq!(changed(&main, seq), Some(vec![]));
        main.set_offline(usb, false);
        assert_eq!(main.changed_since(seq), None);

        let seq = main.change_seq();
        for _ in 0..CHANGE_LOG_MAX {
            main.note_changed(usb);
        }
        assert_eq!(
            main.changed_since(seq).map(<[NodeId]>::len),
            Some(CHANGE_LOG_MAX)
        );
        main.note_changed(usb);
        assert_eq!(main.changed_since(seq), None);
        assert_eq!(changed(&main, main.change_seq()), Some(vec![]));
    }

    #[test]
    fn a_subtree_change_logs_every_node_below_the_folder() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/r", true, Meta::default());
        let a = store.add(Some(root), b"a", true, Meta::default());
        let sub = store.add(Some(a), b"sub", true, Meta::default());
        let x = store.add(Some(sub), b"x", false, Meta::default());
        let y = store.add(Some(a), b"y", false, Meta::default());
        let big = store.add(Some(root), b"big", true, Meta::default());
        for i in 0..=CHANGE_LOG_MAX {
            store.add(
                Some(big),
                format!("f{i}").as_bytes(),
                false,
                Meta::default(),
            );
        }
        let mut index = Index::new(store);
        let seq = index.change_seq();
        index.note_subtree_changed(a);
        assert_eq!(changed(&index, seq), Some(vec![a, sub, x, y]));
        // Too many for the log: it resets, which covers them all.
        let seq = index.change_seq();
        index.note_subtree_changed(big);
        assert_eq!(index.changed_since(seq), None);
        let seq = index.change_seq();
        index.note_subtree_changed(y);
        assert_eq!(changed(&index, seq), Some(vec![y]));
    }

    #[test]
    fn a_folder_moved_to_another_folder_logs_all_nodes_below_it() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/r", true, Meta::default());
        let a = store.add(Some(root), b"a", true, Meta::default());
        let x = store.add(Some(a), b"x", false, Meta::default());
        let other = store.add(Some(root), b"other", true, Meta::default());
        let mut index = Index::new(store);
        // A new name in the same folder: only the folder.
        let seq = index.change_seq();
        index.rename(a, Some(root), b"b").unwrap();
        assert_eq!(changed(&index, seq), Some(vec![a]));
        // Into another folder: the folder and all below it.
        let seq = index.change_seq();
        index.rename(a, Some(other), b"b").unwrap();
        assert_eq!(changed(&index, seq), Some(vec![a, x]));
        // A file moved: only the file.
        let seq = index.change_seq();
        index.rename(x, Some(root), b"x").unwrap();
        assert_eq!(changed(&index, seq), Some(vec![x]));
    }

    #[test]
    fn every_new_index_has_its_own_stamp() {
        let mut main = tree(b"/home/u", &[b"a.txt", b"b.txt"]);
        let first = main.stamp();
        let a = main
            .find_child(main.roots().first().copied(), b"a.txt")
            .unwrap();
        main.remove(a);
        main.take_removed();
        let (copy, _) = main.compacted();
        let (part, _) = main.extract(&main.roots());
        assert!(copy.stamp() != first && part.stamp() != first && copy.stamp() != part.stamp());
        main.compact_in_place();
        assert_ne!(main.stamp(), first);
        assert_ne!(Index::new(NodeStore::new()).stamp(), main.stamp());
    }
}
