//! Keeping an [`Index`] up to date with inotify.
//!
//! Every indexed folder gets a watch. Events are read in batches and applied to
//! the index: creations add nodes, deletions remove nodes, and a
//! `MOVED_FROM`/`MOVED_TO` pair with the same cookie becomes a rename, which keeps
//! the node id so paths below a renamed folder stay correct for free. A move out
//! of the watched trees is a removal; a move in from outside is a creation.
//! The two halves of a rename may be read in two batches, so a `MOVED_FROM`
//! without its `MOVED_TO` waits for one more batch before it counts as a
//! move out.
//!
//! A new folder is watched before its contents are read, so entries created
//! while it is being read are either seen by the read or reported as events.
//!
//! A folder's saved modification time tells a later catch-up (see
//! [`crate::sync`]) that the index matches the folder as of that time. A time
//! read while applying a batch may already cover changes whose events are
//! still queued, so it is kept only once the next batch, read after it, has
//! been applied. Until then the folder keeps its older time, and a catch-up
//! after an exit or a dropped watcher reads it again.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::index::Index;
use crate::inotify::{Event, Inotify, Mask, WatchId};
use crate::progress::Progress;
use crate::scan::{ScanOptions, Skip};
use crate::store::{Meta, NodeId};
use crate::sync::{
    CatchUpStats, Folders, Pending, add_tree, catch_up_with, online_roots, recheck_dirs,
};

/// How long [`Watcher::catch_up_shared`] holds the index's write lock at a
/// time (the last folder of a batch may take longer).
pub const CATCH_UP_BATCH: Duration = Duration::from_millis(15);

/// What happened while applying a batch of events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyStats {
    /// Nodes added, including the contents of new folders.
    pub added: usize,
    /// Nodes removed, including subtrees.
    pub removed: usize,
    /// Nodes renamed or moved inside the watched trees.
    pub moved: usize,
    /// Nodes whose metadata changed.
    pub updated: usize,
    /// The kernel dropped events; the index may be stale and needs a rescan.
    pub overflow: bool,
    /// The index was compacted: node ids held outside it are no longer valid.
    pub compacted: bool,
    /// Folders whose file system was unmounted; see
    /// [`Watcher::take_unmounted`].
    pub unmounted: usize,
}

/// The inotify instance and the mapping between watches and folder nodes.
struct Watches {
    inotify: Inotify,
    /// The folder nodes of each watch. The kernel gives one watch per folder,
    /// so a folder that shows up twice in the indexed trees (through a bind
    /// mount) has two nodes for one watch.
    dir_of: HashMap<WatchId, Vec<NodeId>>,
    watch_of: HashMap<NodeId, WatchId>,
    /// Folders whose watch could not be added, with the kind of error.
    failed: HashMap<NodeId, io::ErrorKind>,
}

impl Watches {
    fn watch(&mut self, index: &Index, dir: NodeId, skip: &Skip) {
        let path = index.store().path(dir);
        if !skip.enters(&path) {
            return;
        }
        match self.inotify.add_watch(&path, Mask::INDEX_EVENTS) {
            Ok(wd) => {
                if self.watch_of.get(&dir).is_some_and(|&old| old != wd) {
                    self.detach(dir);
                }
                // A node that still has this watch (the same folder under
                // another path, or a stale node about to be removed) keeps it.
                let dirs = self.dir_of.entry(wd).or_default();
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
                self.watch_of.insert(dir, wd);
                self.failed.remove(&dir);
            }
            Err(e) => {
                self.failed.insert(dir, e.kind());
            }
        }
    }

    /// Translates node ids after the index was compacted.
    fn remap(&mut self, remap: &[Option<NodeId>]) {
        let old = std::mem::take(&mut self.dir_of);
        self.watch_of.clear();
        self.failed = std::mem::take(&mut self.failed)
            .into_iter()
            .filter_map(|(dir, kind)| Some((remap.get(dir.index()).copied().flatten()?, kind)))
            .collect();
        for (wd, dirs) in old {
            let dirs: Vec<NodeId> = dirs
                .into_iter()
                .filter_map(|dir| remap.get(dir.index()).copied().flatten())
                .collect();
            if dirs.is_empty() {
                let _ = self.inotify.remove_watch(wd);
                continue;
            }
            for &new in &dirs {
                self.watch_of.insert(new, wd);
            }
            self.dir_of.insert(wd, dirs);
        }
    }

    /// Takes `dir` off its watch, and returns the watch when no other node
    /// has it any more.
    fn detach(&mut self, dir: NodeId) -> Option<WatchId> {
        let wd = self.watch_of.remove(&dir)?;
        let dirs = self.dir_of.get_mut(&wd)?;
        dirs.retain(|&d| d != dir);
        if !dirs.is_empty() {
            return None;
        }
        self.dir_of.remove(&wd);
        Some(wd)
    }

    /// Drops the watches of removed folders.
    fn forget(&mut self, removed: &[NodeId]) {
        for id in removed {
            self.failed.remove(id);
            if let Some(wd) = self.detach(*id) {
                // The folder may still exist elsewhere (moved out), so stop watching it.
                let _ = self.inotify.remove_watch(wd);
            }
        }
    }
}

/// Watches all indexed folders and applies their changes to the index.
pub struct Watcher {
    watches: Watches,
    skip: Skip,
    buf: Vec<u8>,
    /// Folder times read at the end of the last batch, kept by the next one.
    unsettled: HashMap<NodeId, Meta>,
    /// Where the next [`recheck_unwatched`](Self::recheck_unwatched) starts.
    recheck_from: usize,
    /// Folders found unmounted by the last batch.
    unmounted: Vec<NodeId>,
    /// Moves whose `MOVED_TO` was not in the last batch: the node, back at
    /// its old name, and that name.
    waiting_moves: HashMap<MoveKey, (NodeId, Vec<u8>)>,
    /// Folders with an entry that could not be looked at (readable but not
    /// searchable), found by an event or left so by the scan, a catch-up
    /// or the reading of a new folder. Their time is not kept, so a
    /// catch-up reads them again, and they are listed again when they get
    /// an event, until a listing is complete.
    incomplete: HashSet<NodeId>,
    /// Folders where a created entry was not found because the folder was
    /// no longer at its indexed path: it was moved, and its move is in a
    /// batch not applied yet. Each is listed again once the index has it at
    /// a path that is a folder; its time is not kept until then.
    moved_away: HashSet<NodeId>,
}

impl Watcher {
    /// Creates a watcher with a watch on every live folder of `index` below
    /// a root that is not offline. Folders that a scan with `options` would
    /// not enter are not watched either, and events apply the same rules as
    /// the scan: entries it would leave out are not added.
    pub fn new(index: &Index, options: &ScanOptions) -> io::Result<Self> {
        let mut w = Self {
            watches: Watches {
                inotify: Inotify::new()?,
                dir_of: HashMap::new(),
                watch_of: HashMap::new(),
                failed: HashMap::new(),
            },
            skip: Skip::new(options),
            buf: Vec::new(),
            unsettled: HashMap::new(),
            recheck_from: 0,
            unmounted: Vec::new(),
            waiting_moves: HashMap::new(),
            incomplete: HashSet::new(),
            moved_away: HashSet::new(),
        };
        for root in online_roots(index) {
            w.watch_root(index, root);
        }
        Ok(w)
    }

    /// Watches every folder below `root` (a root that came online, or was
    /// added with [`Index::graft`]).
    pub fn watch_root(&mut self, index: &Index, root: NodeId) {
        let store = index.store();
        if !store.is_live(root) || !store.is_dir(root) {
            return;
        }
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            self.watches.watch(index, dir, &self.skip);
            stack.extend(store.children(dir).filter(|&k| store.is_dir(k)));
        }
        self.note_incomplete_below(index, &[root]);
    }

    /// Marks the watched folders below `tops` whose listing was not complete
    /// (a scan or catch-up left their time unknown) as incomplete, so they
    /// are listed again when they get an event instead of having their
    /// time kept.
    fn note_incomplete_below(&mut self, index: &Index, tops: &[NodeId]) {
        let store = index.store();
        let mut stack: Vec<NodeId> = tops
            .iter()
            .copied()
            .filter(|&d| store.is_live(d) && store.is_dir(d))
            .collect();
        while let Some(dir) = stack.pop() {
            if store.meta(dir).mtime_ns == 0 && self.watches.watch_of.contains_key(&dir) {
                self.incomplete.insert(dir);
            }
            stack.extend(store.children(dir).filter(|&k| store.is_dir(k)));
        }
    }

    /// Stops watching the folders below `root`, for a root that goes
    /// offline. Events already read for them are ignored too, since the
    /// mapping from their watches is gone, and so are moves still waiting
    /// for their other half: their entries stay at their old names.
    pub fn unwatch_root(&mut self, index: &Index, root: NodeId) {
        let store = index.store();
        if !store.is_live(root) {
            return;
        }
        let mut stack = vec![root];
        let mut dirs = Vec::new();
        while let Some(dir) = stack.pop() {
            dirs.push(dir);
            stack.extend(store.children(dir).filter(|&k| store.is_dir(k)));
        }
        self.watches.forget(&dirs);
        let below = |id: NodeId| store.is_live(id) && store.root_of(id) == root;
        self.waiting_moves.retain(|_, (node, _)| !below(*node));
        self.unsettled.retain(|&dir, _| !below(dir));
        self.incomplete.retain(|&dir| !below(dir));
        self.moved_away.retain(|&dir| !below(dir));
    }

    /// Brings `index` up to date with changes made while nothing was watching
    /// (see [`crate::sync`]) and adjusts the watches to match. Call it after
    /// [`new`](Self::new), so changes made during the catch-up are still reported.
    /// Offline roots are skipped.
    pub fn catch_up(&mut self, index: &mut Index) -> CatchUpStats {
        let roots = online_roots(index);
        self.catch_up_roots(index, &roots)
    }

    /// Like [`catch_up`](Self::catch_up), for the given roots only (for
    /// example a drive that was plugged in again). Watch them first with
    /// [`watch_root`](Self::watch_root).
    pub fn catch_up_roots(&mut self, index: &mut Index, roots: &[NodeId]) -> CatchUpStats {
        self.catch_up_with_progress(index, roots, &Progress::new())
    }

    /// Like [`catch_up_roots`](Self::catch_up_roots), counting every folder
    /// it checks in `progress` (see [`crate::sync::folders_below`] for the
    /// total).
    pub fn catch_up_with_progress(
        &mut self,
        index: &mut Index,
        roots: &[NodeId],
        progress: &Progress,
    ) -> CatchUpStats {
        let mut visit = Visit::new(&mut self.watches, &self.skip);
        let stats = catch_up_with(index, roots, &self.skip, &mut visit, progress);
        let incomplete = visit.incomplete;
        self.watches.forget(&index.take_removed());
        self.keep_incomplete(index, incomplete);
        stats
    }

    /// Like [`catch_up_with_progress`](Self::catch_up_with_progress) on an
    /// index that others read meanwhile: the write lock is held for a batch
    /// of folders at a time, about [`CATCH_UP_BATCH`] long, so a reader waits
    /// at most about that long instead of for the whole catch-up. Only
    /// readers may use the index meanwhile: the folders left to check are ids
    /// of it. Once `stop` says so after a batch, the catch-up ends early; the
    /// folders not checked keep their saved times, so a later catch-up
    /// checks them.
    pub fn catch_up_shared(
        &mut self,
        index: &RwLock<Index>,
        roots: &[NodeId],
        progress: &Progress,
        stop: &dyn Fn() -> bool,
    ) -> CatchUpStats {
        let write = || index.write().unwrap_or_else(PoisonError::into_inner);
        let mut pending =
            Pending::new(&index.read().unwrap_or_else(PoisonError::into_inner), roots);
        let mut visit = Visit::new(&mut self.watches, &self.skip);
        let mut guard = loop {
            let mut guard = write();
            let deadline = Instant::now() + CATCH_UP_BATCH;
            let done = pending.run(&mut guard, &self.skip, &mut visit, progress, Some(deadline));
            if done || stop() {
                break guard;
            }
            drop(guard);
            // Readers that waited get the lock before the next batch.
            std::thread::sleep(Duration::from_millis(1));
        };
        let incomplete = visit.incomplete;
        self.watches.forget(&guard.take_removed());
        self.keep_incomplete(&guard, incomplete);
        drop(guard);
        pending.stats
    }

    /// Number of active watches.
    pub fn watch_count(&self) -> usize {
        self.watches.dir_of.len()
    }

    /// Folders that could not be watched (for example permission denied,
    /// or the limit of watches reached). Changes inside them are found by
    /// [`recheck_unwatched`](Self::recheck_unwatched).
    pub fn failed_watches(&self) -> usize {
        self.watches.failed.len()
    }

    /// How many folders could not be watched, by the kind of error:
    /// `StorageFull` when the limit of inotify watches
    /// (`fs.inotify.max_user_watches`) was reached, `PermissionDenied` for a
    /// folder that cannot be read (and so has no entries indexed either).
    pub fn failed_watch_kinds(&self) -> impl Iterator<Item = io::ErrorKind> + '_ {
        self.watches.failed.values().copied()
    }

    /// Tries again to watch up to `max` of the folders that could not be
    /// watched, and reads again those whose time changed, as a catch-up
    /// would. Each call goes on where the last one stopped. Call it from time
    /// to time while [`failed_watches`](Self::failed_watches) is not zero:
    /// without a watch, changes inside such a folder are not reported.
    pub fn recheck_unwatched(&mut self, index: &mut Index, max: usize) -> CatchUpStats {
        // Folders removed by the owner of the index without this watcher.
        self.watches
            .failed
            .retain(|&dir, _| index.store().is_live(dir));
        let mut failed: Vec<NodeId> = self.watches.failed.keys().copied().collect();
        failed.sort_unstable_by_key(|id| id.index());
        let start = failed.partition_point(|id| id.index() < self.recheck_from);
        let dirs: Vec<NodeId> = failed[start..]
            .iter()
            .chain(&failed[..start])
            .take(max)
            .copied()
            .collect();
        self.recheck_from = dirs.last().map_or(0, |id| id.index() + 1);
        let mut limit_reached = false;
        for &dir in &dirs {
            if !limit_reached && index.store().is_live(dir) {
                self.watches.watch(index, dir, &self.skip);
                // The others would fail the same way.
                limit_reached = self.watches.failed.get(&dir) == Some(&io::ErrorKind::StorageFull);
            }
        }
        let mut visit = Visit::new(&mut self.watches, &self.skip);
        let stats = recheck_dirs(index, &dirs, &self.skip, false, &mut visit);
        let incomplete = visit.incomplete;
        self.watches.forget(&index.take_removed());
        self.keep_incomplete(index, incomplete);
        stats
    }

    /// Whether [`apply`](Self::apply) has work to do even without events:
    /// folder times from the last batch to keep. A caller that applies only
    /// non-empty batches should also apply an empty one when this holds.
    pub fn has_pending(&self) -> bool {
        !self.unsettled.is_empty() || !self.waiting_moves.is_empty()
    }

    /// Waits up to `timeout` for events and applies them to `index`. When enough
    /// removed slots have piled up, the index is also compacted (see
    /// [`ApplyStats::compacted`]).
    pub fn poll(&mut self, index: &mut Index, timeout: Option<Duration>) -> io::Result<ApplyStats> {
        let events = self.wait(timeout)?;
        Ok(self.apply_batch(index, &events))
    }

    /// Waits up to `timeout` for events without touching the index. Together
    /// with [`apply_batch`](Self::apply_batch) this lets a caller that shares the
    /// index between threads hold its lock only while changes are applied.
    pub fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Vec<Event>> {
        self.watches.inotify.read_events(&mut self.buf, timeout)
    }

    /// Applies events read by [`wait`](Self::wait) and compacts the index when
    /// enough removed slots have piled up.
    pub fn apply_batch(&mut self, index: &mut Index, events: &[Event]) -> ApplyStats {
        let mut stats = self.apply(index, events);
        if index.should_compact() {
            self.compact(index);
            stats.compacted = true;
        }
        stats
    }

    /// Compacts `index` in place and moves the watches to the new node ids.
    /// Returns the map from old to new ids, for translating ids held elsewhere
    /// (such as a list of search results).
    pub fn compact(&mut self, index: &mut Index) -> Vec<Option<NodeId>> {
        self.watches.forget(&index.take_removed());
        let remap = index.compact_in_place();
        self.watches.remap(&remap);
        self.unsettled = std::mem::take(&mut self.unsettled)
            .into_iter()
            .filter_map(|(dir, meta)| Some((remap.get(dir.index()).copied().flatten()?, meta)))
            .collect();
        self.unmounted = std::mem::take(&mut self.unmounted)
            .into_iter()
            .filter_map(|dir| remap.get(dir.index()).copied().flatten())
            .collect();
        self.waiting_moves = std::mem::take(&mut self.waiting_moves)
            .into_iter()
            .filter_map(|(key, (node, name))| {
                Some((key, (remap.get(node.index()).copied().flatten()?, name)))
            })
            .collect();
        self.incomplete = std::mem::take(&mut self.incomplete)
            .into_iter()
            .filter_map(|dir| remap.get(dir.index()).copied().flatten())
            .collect();
        self.moved_away = std::mem::take(&mut self.moved_away)
            .into_iter()
            .filter_map(|dir| remap.get(dir.index()).copied().flatten())
            .collect();
        remap
    }

    /// Applies a batch of events to `index`.
    pub fn apply(&mut self, index: &mut Index, events: &[Event]) -> ApplyStats {
        let mut stats = ApplyStats::default();
        // Nodes between MOVED_FROM and MOVED_TO, parked under a name that no real
        // file can have, so a new entry with the old name is not mistaken for them.
        // They are keyed by cookie and alias (see `Watches::dir_of`).
        let mut moved_out: HashMap<MoveKey, NodeId> = HashMap::new();
        // The names moved away in this batch.
        let mut moved_names: HashMap<MoveKey, Vec<u8>> = HashMap::new();
        // Moves left over from the last batch: this one is their last chance.
        let mut last_chance: HashSet<MoveKey> = HashSet::new();
        for (key, (node, _)) in std::mem::take(&mut self.waiting_moves) {
            // An offline root keeps its entries as they were.
            if !index.store().is_live(node) || index.is_offline(node) {
                continue;
            }
            let parent = index.store().parent(node);
            if index.rename(node, parent, &parked_name(key)).is_ok() {
                moved_out.insert(key, node);
                last_chance.insert(key);
            } else {
                stats.removed += index.remove(node).len();
            }
        }
        // Folders whose time is read again at the end.
        let mut touched: HashSet<NodeId> = HashSet::new();
        // Entries that were not found where the index put them when their event
        // was applied, and folders where an entry could not be looked at.
        let mut missed: Vec<(NodeId, Vec<u8>)> = Vec::new();
        let mut incomplete: HashSet<NodeId> = HashSet::new();
        let mut unmounted: HashSet<NodeId> = HashSet::new();
        // Folders that had an event about one of their entries.
        let mut event_dirs: HashSet<NodeId> = HashSet::new();
        for ev in events {
            if ev.mask.contains(Mask::Q_OVERFLOW) {
                stats.overflow = true;
                continue;
            }
            if ev.mask.contains(Mask::UNMOUNT) {
                // An IGNORED event follows and drops the watch.
                if let Some(dirs) = self.watches.dir_of.get(&ev.wd) {
                    unmounted.extend(dirs);
                }
                continue;
            }
            if ev.mask.contains(Mask::IGNORED) {
                for dir in self.watches.dir_of.remove(&ev.wd).unwrap_or_default() {
                    if self.watches.watch_of.get(&dir) == Some(&ev.wd) {
                        self.watches.watch_of.remove(&dir);
                    }
                }
                continue;
            }
            let Some(dirs) = self.watches.dir_of.get(&ev.wd).cloned() else {
                continue;
            };
            // Every indexed folder that shows the watched one: normally one,
            // more when a bind mount shows it again inside the indexed trees.
            for (alias, dir) in dirs.into_iter().enumerate() {
                let key = (ev.cookie, alias);
                if !index.store().is_live(dir) {
                    continue;
                }
                let Some(name) = ev.name.as_deref().map(|n| n.as_bytes()) else {
                    // Events about the watched folder itself: its parent reports them too,
                    // except for a root, which has no parent.
                    if index.store().parent(dir).is_none() {
                        if ev.mask.intersects(Mask::DELETE_SELF | Mask::MOVE_SELF) {
                            stats.removed += index.remove(dir).len();
                        } else if ev.mask.contains(Mask::ATTRIB) {
                            // Its attributes may be kept outside the index, and
                            // its permissions decide what can be read below it.
                            index.note_subtree_changed(dir);
                        }
                    }
                    continue;
                };
                event_dirs.insert(dir);

                if ev
                    .mask
                    .intersects(Mask::CREATE | Mask::DELETE | Mask::MOVED_FROM | Mask::MOVED_TO)
                {
                    touched.insert(dir);
                }
                if ev.mask.contains(Mask::MOVED_FROM) {
                    if let Some(node) = index.find_child(Some(dir), name) {
                        if index.rename(node, Some(dir), &parked_name(key)).is_ok() {
                            if let Some(previous) = moved_out.insert(key, node) {
                                stats.removed += index.remove(previous).len();
                            }
                            last_chance.remove(&key);
                            moved_names.insert(key, name.to_vec());
                        } else {
                            stats.removed += index.remove(node).len();
                        }
                    }
                } else if ev.mask.contains(Mask::MOVED_TO) {
                    match moved_out.remove(&key) {
                        Some(node)
                            if self.keeps_on_move(index, node, dir, name)
                                && index.rename(node, Some(dir), name).is_ok() =>
                        {
                            stats.moved += 1;
                            if self.skip.with_metadata || index.store().is_dir(node) {
                                self.refresh_meta(index, node, &mut stats, &mut touched);
                            }
                        }
                        Some(node) => {
                            stats.removed += index.remove(node).len();
                            self.add_or_miss(index, dir, name, &mut stats, &mut missed);
                        }
                        None => self.add_or_miss(index, dir, name, &mut stats, &mut missed),
                    }
                } else if ev.mask.contains(Mask::CREATE) {
                    self.add_or_miss(index, dir, name, &mut stats, &mut missed);
                } else if ev.mask.contains(Mask::DELETE) {
                    if let Some(node) = index.find_child(Some(dir), name) {
                        stats.removed += index.remove(node).len();
                    }
                } else if ev
                    .mask
                    .intersects(Mask::ATTRIB | Mask::CLOSE_WRITE | Mask::MODIFY)
                {
                    if let Some(node) = index.find_child(Some(dir), name) {
                        if self.skip.with_metadata || index.store().is_dir(node) {
                            self.refresh_meta(index, node, &mut stats, &mut touched);
                        }
                        if ev.mask.contains(Mask::ATTRIB) && index.store().is_dir(node) {
                            // A folder's permissions decide what can be read
                            // below it, without events for the nodes there.
                            index.note_subtree_changed(node);
                        }
                    }
                }
            }
        }
        // A move whose destination was not reported may have its other half in
        // the next batch: it goes back to its old name until then (the index
        // lags behind the disk as for any event not read yet). One left over
        // from the last batch, or whose old name is taken again, left the
        // watched trees.
        for (key, node) in moved_out {
            let back = match moved_names.remove(&key) {
                Some(name) if !last_chance.contains(&key) => {
                    let parent = index.store().parent(node);
                    (index.find_child(parent, &name).is_none()
                        && index.rename(node, parent, &name).is_ok())
                    .then_some(name)
                }
                _ => None,
            };
            match back {
                Some(name) => {
                    self.waiting_moves.insert(key, (node, name));
                }
                None => stats.removed += index.remove(node).len(),
            }
        }
        // An entry created in a folder that was then renamed was looked for at
        // the folder's old path. Look again where the folder is now.
        for (dir, name) in missed {
            if !index.store().is_live(dir) || index.find_child(Some(dir), &name).is_some() {
                continue;
            }
            match self.add_from_disk(index, dir, &name) {
                Ok(added) => stats.added += added,
                // Gone again, unless the folder itself is gone from its
                // indexed path: then it moved too, and the events of that
                // move come in a later batch.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if !is_dir_at(&index.store().path(dir)) {
                        self.moved_away.insert(dir);
                    }
                }
                // Not allowed to look: the folder keeps its older time, so a
                // catch-up reads it again.
                Err(_) => {
                    incomplete.insert(dir);
                }
            }
        }
        // A folder where an entry could not be looked at is listed again
        // when it has an event (in this batch: the one that could not be
        // looked at; later, say, the permission change that makes it
        // searchable again). The listing gives names and types even where
        // entries cannot be looked at; the folder stays marked until a
        // listing is complete.
        self.incomplete.retain(|&dir| index.store().is_live(dir));
        self.incomplete.extend(incomplete);
        // A folder whose move was applied by now is listed at its new place,
        // which gives the entries looked for at the old one.
        self.moved_away
            .retain(|&dir| index.store().is_live(dir) && !index.is_offline(dir));
        let arrived: Vec<NodeId> = self
            .moved_away
            .iter()
            .copied()
            .filter(|&dir| is_dir_at(&index.store().path(dir)))
            .collect();
        for dir in arrived {
            self.moved_away.remove(&dir);
            let listed = self.relist(index, dir);
            stats.added += listed.added;
            stats.removed += listed.removed;
            stats.updated += listed.updated;
        }
        let relist: Vec<NodeId> = self
            .incomplete
            .iter()
            .copied()
            .filter(|dir| event_dirs.contains(dir) || touched.contains(dir))
            .collect();
        for dir in relist {
            self.incomplete.remove(&dir);
            let listed = self.relist(index, dir);
            stats.added += listed.added;
            stats.removed += listed.removed;
            stats.updated += listed.updated;
        }
        // The times read at the end of the previous batch: every event queued
        // before them has now been read and applied. Only a watched folder
        // reports its changes; one without a watch keeps its older time, and
        // so does one still marked incomplete.
        let incomplete = &self.incomplete;
        let moved_away = &self.moved_away;
        let keeps_time = |index: &Index, watches: &Watches, dir: NodeId| {
            index.store().is_live(dir)
                && !incomplete.contains(&dir)
                && !moved_away.contains(&dir)
                && watches.watch_of.contains_key(&dir)
        };
        for (dir, meta) in std::mem::take(&mut self.unsettled) {
            if keeps_time(index, &self.watches, dir) {
                index.set_meta(dir, meta);
            }
        }
        for dir in touched {
            if keeps_time(index, &self.watches, dir) {
                if let Ok(m) = fs::symlink_metadata(index.store().path(dir)) {
                    self.unsettled.insert(dir, Meta::from_metadata(&m));
                }
            }
        }
        // The topmost of them: below them all was on the same file system.
        self.unmounted = unmounted
            .iter()
            .copied()
            .filter(|&dir| {
                let store = index.store();
                store.is_live(dir)
                    && std::iter::successors(store.parent(dir), |&p| store.parent(p))
                        .all(|up| !unmounted.contains(&up))
            })
            .collect();
        stats.unmounted = self.unmounted.len();
        self.watches.forget(&index.take_removed());
        stats
    }

    /// The folders whose file system was unmounted, found by the last
    /// [`apply`](Self::apply): the topmost of them, normally the mount point.
    /// Their watches are gone, and their path now shows what is below the
    /// mount (or nothing). The index is left as it was: the caller decides
    /// whether to keep the entries (a removable drive that goes offline) or to
    /// [`reread`](Self::reread) the folder.
    pub fn take_unmounted(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.unmounted)
    }

    /// Replaces what the index has below `dir` with what is at its path now,
    /// and watches it again. For a folder whose file system was unmounted
    /// (see [`take_unmounted`](Self::take_unmounted)). A folder that is gone
    /// is removed.
    pub fn reread(&mut self, index: &mut Index, dir: NodeId) -> CatchUpStats {
        if !index.store().is_live(dir) || !index.store().is_dir(dir) {
            return CatchUpStats::default();
        }
        let kids: Vec<NodeId> = index.store().children(dir).collect();
        let removed: usize = kids.into_iter().map(|k| index.remove(k).len()).sum();
        self.watches.forget(&index.take_removed());
        self.unsettled.remove(&dir);
        // The old watch is gone with the file system.
        self.watches.detach(dir);
        self.watches.watch(index, dir, &self.skip);
        let mut stats = self.relist(index, dir);
        stats.removed += removed;
        stats
    }

    /// Adds `parent/name` from disk, or notes it in `missed` when it cannot
    /// be looked at now.
    fn add_or_miss(
        &mut self,
        index: &mut Index,
        parent: NodeId,
        name: &[u8],
        stats: &mut ApplyStats,
        missed: &mut Vec<(NodeId, Vec<u8>)>,
    ) {
        match self.add_from_disk(index, parent, name) {
            Ok(added) => stats.added += added,
            Err(_) => missed.push((parent, name.to_vec())),
        }
    }

    /// Reads the metadata of `node` again. A folder's time is only noted in
    /// `touched`, to be kept as described in the module documentation.
    fn refresh_meta(
        &self,
        index: &mut Index,
        node: NodeId,
        stats: &mut ApplyStats,
        touched: &mut HashSet<NodeId>,
    ) {
        if index.store().is_dir(node) {
            index.note_changed(node);
            touched.insert(node);
            stats.updated += 1;
        } else if let Ok(m) = fs::symlink_metadata(index.store().path(node)) {
            index.set_meta(node, Meta::from_metadata(&m));
            stats.updated += 1;
        }
    }

    /// Whether `node`, moved to `name` in `dir`, can keep its subtree as it
    /// is. Not when the rules leave it out under its new name, or when they
    /// look at paths and it is a folder: what they leave out below it may
    /// differ at its new place, so it is read again.
    fn keeps_on_move(&self, index: &Index, node: NodeId, dir: NodeId, name: &[u8]) -> bool {
        let rules = &self.skip.rules;
        if rules.is_empty() {
            return true;
        }
        let is_dir = index.store().is_dir(node);
        if is_dir && rules.uses_paths() {
            return false;
        }
        rules.accepts(&index.store().path(dir), name, is_dir)
    }

    /// Adds `parent/name` as it is on disk now, including a folder's contents,
    /// watching each new folder before reading it. Returns the number of nodes
    /// added, or the error when the entry itself cannot be looked at.
    fn add_from_disk(
        &mut self,
        index: &mut Index,
        parent: NodeId,
        name: &[u8],
    ) -> io::Result<usize> {
        let path = index
            .store()
            .path(parent)
            .join(std::ffi::OsStr::from_bytes(name));
        fs::symlink_metadata(&path)?;
        let mut visit = Visit::new(&mut self.watches, &self.skip);
        let added = add_tree(index, parent, name, &path, &self.skip, &mut visit);
        let incomplete = visit.incomplete;
        self.keep_incomplete(index, incomplete);
        Ok(added)
    }

    /// Lists the folder `dir` again, watching new folders below it, and
    /// marks what is still incomplete.
    fn relist(&mut self, index: &mut Index, dir: NodeId) -> CatchUpStats {
        let mut visit = Visit::new(&mut self.watches, &self.skip);
        let stats = recheck_dirs(index, &[dir], &self.skip, true, &mut visit);
        let mut incomplete = visit.incomplete;
        if stats.errors != 0 {
            // Not even looked at, say under a folder that is not searchable.
            incomplete.push(dir);
        }
        self.watches.forget(&index.take_removed());
        self.keep_incomplete(index, incomplete);
        stats
    }

    /// Marks the watched ones of `dirs`, folders that could not be read
    /// completely, as incomplete (see the field `incomplete`).
    fn keep_incomplete(&mut self, index: &Index, dirs: Vec<NodeId>) {
        self.incomplete.extend(
            dirs.into_iter().filter(|dir| {
                index.store().is_live(*dir) && self.watches.watch_of.contains_key(dir)
            }),
        );
    }
}

/// Watches each folder that a catch-up adds, and collects those it could
/// not read completely.
struct Visit<'a> {
    watches: &'a mut Watches,
    skip: &'a Skip,
    incomplete: Vec<NodeId>,
}

impl<'a> Visit<'a> {
    fn new(watches: &'a mut Watches, skip: &'a Skip) -> Self {
        Self {
            watches,
            skip,
            incomplete: Vec::new(),
        }
    }
}

impl Folders for Visit<'_> {
    fn added(&mut self, index: &Index, dir: NodeId) {
        self.watches.watch(index, dir, self.skip);
    }

    fn incomplete(&mut self, dir: NodeId) {
        self.incomplete.push(dir);
    }
}

/// Whether `path` is a folder now (not followed if it is a link).
fn is_dir_at(path: &std::path::Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// A move: its cookie, and which of the folders that show the watched one
/// it happened in.
type MoveKey = (u32, usize);

/// Temporary name of a node whose move has not been matched yet. File names
/// cannot contain NUL, so it never collides with a real entry.
fn parked_name((cookie, alias): MoveKey) -> Vec<u8> {
    let mut name = b"\0moving\0".to_vec();
    name.extend_from_slice(&cookie.to_le_bytes());
    name.extend_from_slice(&alias.to_le_bytes());
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use crate::store::NodeStore;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    struct Fixture {
        base: PathBuf,
        index: Index,
        watcher: Watcher,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("everygrep-watch-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(base.join("root/docs")).unwrap();
            fs::create_dir_all(base.join("outside")).unwrap();
            fs::write(base.join("root/docs/a.txt"), b"a").unwrap();
            let mut store = NodeStore::new();
            scan(&mut store, &[base.join("root")], &ScanOptions::default());
            let index = Index::new(store);
            let watcher = Watcher::new(&index, &ScanOptions::default()).unwrap();
            Self {
                base,
                index,
                watcher,
            }
        }

        fn root(&self) -> PathBuf {
            self.base.join("root")
        }

        /// Applies events until `done` holds or two seconds pass.
        fn settle(&mut self, done: impl Fn(&Index) -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                self.watcher
                    .poll(&mut self.index, Some(Duration::from_millis(50)))
                    .unwrap();
                if done(&self.index) {
                    return true;
                }
            }
            false
        }

        fn paths(&self) -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = self
                .index
                .order()
                .iter()
                .map(|id| self.index.store().path(id))
                .collect();
            v.sort();
            v
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn has(index: &Index, path: &Path) -> bool {
        index
            .order()
            .iter()
            .any(|id| index.store().path(id) == path)
    }

    #[test]
    fn live_changes_follow_the_rules() {
        use crate::rules::{IndexRules, RuleSpec};
        let base =
            std::env::temp_dir().join(format!("everygrep-watch-rules-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root/docs")).unwrap();
        fs::create_dir_all(base.join("outside/build/deep")).unwrap();
        fs::write(base.join("outside/build/deep/o.txt"), b"o").unwrap();
        let options = ScanOptions {
            rules: IndexRules::new(RuleSpec {
                folders: vec!["build".into()],
                include_files: "*.txt;*.tmp".into(),
                exclude_files: "*.tmp;regex:/docs/secret".into(),
                ..RuleSpec::default()
            })
            .unwrap(),
            ..ScanOptions::default()
        };
        let mut store = NodeStore::new();
        scan(&mut store, &[base.join("root")], &options);
        let mut index = Index::new(store);
        let mut watcher = Watcher::new(&index, &options).unwrap();
        let root = base.join("root");
        let settle = |index: &mut Index, watcher: &mut Watcher, done: &dyn Fn(&Index) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                watcher
                    .poll(index, Some(Duration::from_millis(50)))
                    .unwrap();
                if done(index) {
                    return true;
                }
            }
            false
        };
        fs::write(root.join("docs/a.tmp"), b"t").unwrap();
        fs::write(root.join("docs/b.pdf"), b"p").unwrap();
        fs::write(root.join("docs/secret.txt"), b"s").unwrap();
        fs::create_dir_all(root.join("build/x")).unwrap();
        fs::write(root.join("docs/ok.txt"), b"k").unwrap();
        assert!(settle(&mut index, &mut watcher, &|i| has(
            i,
            &root.join("docs/ok.txt")
        )));
        // A folder moved in brings what the rules allow.
        fs::rename(base.join("outside/build/deep"), root.join("docs/deep")).unwrap();
        assert!(settle(&mut index, &mut watcher, &|i| has(
            i,
            &root.join("docs/deep/o.txt")
        )));
        // A rename into an excluded name removes the file.
        fs::rename(root.join("docs/ok.txt"), root.join("docs/ok.tmp")).unwrap();
        assert!(settle(&mut index, &mut watcher, &|i| !has(
            i,
            &root.join("docs/ok.txt")
        )));
        // A folder renamed to an excluded name goes with its contents.
        fs::rename(root.join("docs/deep"), root.join("docs/build")).unwrap();
        assert!(settle(&mut index, &mut watcher, &|i| !has(
            i,
            &root.join("docs/deep/o.txt")
        )));
        watcher
            .poll(&mut index, Some(Duration::from_millis(100)))
            .unwrap();
        let mut found: Vec<PathBuf> = index
            .order()
            .iter()
            .map(|id| index.store().path(id))
            .collect();
        found.sort();
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(found, [root.clone(), root.join("docs")]);
    }

    #[test]
    fn a_path_rule_is_checked_again_when_a_folder_moves() {
        use crate::rules::{IndexRules, RuleSpec};
        let base =
            std::env::temp_dir().join(format!("everygrep-watch-pathrule-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root/work/sub")).unwrap();
        fs::write(base.join("root/work/sub/n.log"), b"l").unwrap();
        let options = ScanOptions {
            rules: IndexRules::new(RuleSpec {
                exclude_files: "old/**.log".into(),
                ..RuleSpec::default()
            })
            .unwrap(),
            ..ScanOptions::default()
        };
        let mut store = NodeStore::new();
        scan(&mut store, &[base.join("root")], &options);
        let mut index = Index::new(store);
        let mut watcher = Watcher::new(&index, &options).unwrap();
        let root = base.join("root");
        assert!(has(&index, &root.join("work/sub/n.log")));
        fs::rename(root.join("work"), root.join("old")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !has(&index, &root.join("old/sub")) && Instant::now() < deadline {
            watcher
                .poll(&mut index, Some(Duration::from_millis(50)))
                .unwrap();
        }
        let moved_log = has(&index, &root.join("old/sub/n.log"));
        let moved_dir = has(&index, &root.join("old/sub"));
        fs::remove_dir_all(&base).unwrap();
        assert!(moved_dir);
        assert!(!moved_log, "the log below the moved folder is now excluded");
    }

    #[test]
    fn create_modify_and_delete_files() {
        let mut f = Fixture::new("files");
        let new = f.root().join("docs/new.txt");
        fs::write(&new, b"hello").unwrap();
        assert!(f.settle(|i| has(i, &new)));
        let id = f
            .index
            .order()
            .iter()
            .find(|&id| f.index.store().path(id) == new)
            .unwrap();
        fs::write(&new, b"hello, longer").unwrap();
        assert!(f.settle(|i| i.store().meta(id).size == 13));
        fs::remove_file(&new).unwrap();
        assert!(f.settle(|i| !has(i, &new)));
    }

    #[test]
    fn attribute_changes_are_logged_for_files_folders_and_roots() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = Fixture::new("attrib");
        let root = f.index.roots()[0];
        let docs = f.index.find_child(Some(root), b"docs").unwrap();
        let a = f.index.find_child(Some(docs), b"a.txt").unwrap();
        for (path, id, mode) in [
            (f.root().join("docs/a.txt"), a, 0o444),
            (f.root().join("docs"), docs, 0o555),
            // A root has no watched parent; only its own watch sees this.
            (f.root(), root, 0o555),
        ] {
            let seq = f.index.change_seq();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                f.settle(|i| i.changed_since(seq).is_none_or(|c| c.contains(&id))),
                "{}",
                path.display()
            );
        }
        for path in [f.root(), f.root().join("docs")] {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn new_nested_folders_are_scanned_and_watched() {
        let mut f = Fixture::new("nested");
        let deep = f.root().join("x/y/z");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.txt"), b"d").unwrap();
        assert!(f.settle(|i| has(i, &deep.join("deep.txt"))));
        // The new deepest folder must be watched too.
        let later = deep.join("later.txt");
        fs::write(&later, b"l").unwrap();
        assert!(f.settle(|i| has(i, &later)));
    }

    #[test]
    fn renaming_a_folder_updates_paths_below_it() {
        let mut f = Fixture::new("rename");
        let old_file = f.root().join("docs/a.txt");
        let new_dir = f.root().join("papers");
        let before = f.index.len();
        fs::rename(f.root().join("docs"), &new_dir).unwrap();
        assert!(f.settle(|i| has(i, &new_dir.join("a.txt"))));
        assert!(!has(&f.index, &old_file));
        assert_eq!(f.index.len(), before);
        // Still watched under its new name.
        fs::write(new_dir.join("b.txt"), b"b").unwrap();
        assert!(f.settle(|i| has(i, &new_dir.join("b.txt"))));
    }

    #[test]
    fn a_file_created_before_its_folder_is_renamed_is_kept() {
        let mut f = Fixture::new("create-then-rename");
        fs::write(f.root().join("docs/new.txt"), b"n").unwrap();
        let papers = f.root().join("papers");
        // Both changes are queued before any event is read.
        fs::rename(f.root().join("docs"), &papers).unwrap();
        assert!(f.settle(|i| has(i, &papers.join("a.txt")) && has(i, &papers.join("new.txt"))));
        assert!(!has(&f.index, &f.root().join("docs/new.txt")));
        // The renamed folder is still watched.
        fs::write(papers.join("later.txt"), b"l").unwrap();
        assert!(f.settle(|i| has(i, &papers.join("later.txt"))));
    }

    #[test]
    fn a_file_created_before_its_folder_is_renamed_in_the_next_batch_is_kept() {
        let mut f = Fixture::new("create-then-later-rename");
        let papers = f.root().join("papers");
        let reports = f.root().join("reports");
        fs::write(f.root().join("docs/new.txt"), b"n").unwrap();
        let created = read_until(&mut f.watcher, Mask::CREATE);
        // Renamed after the creation was read and before it is applied, so
        // the rename is only in the next batch.
        fs::rename(f.root().join("docs"), &papers).unwrap();
        f.watcher.apply(&mut f.index, &created);
        let renamed = read_until(&mut f.watcher, Mask::MOVED_TO);
        f.watcher.apply(&mut f.index, &renamed);
        f.watcher.apply(&mut f.index, &[]);
        assert!(has(&f.index, &papers.join("a.txt")));
        assert!(has(&f.index, &papers.join("new.txt")));
        // The same with the two halves of the rename in two more batches.
        fs::write(papers.join("other.txt"), b"o").unwrap();
        let created = read_until(&mut f.watcher, Mask::CREATE);
        fs::rename(&papers, &reports).unwrap();
        f.watcher.apply(&mut f.index, &created);
        let renamed = read_until(&mut f.watcher, Mask::MOVED_TO);
        let to = renamed
            .iter()
            .position(|e| e.mask.contains(Mask::MOVED_TO))
            .unwrap();
        f.watcher.apply(&mut f.index, &renamed[..to]);
        f.watcher.apply(&mut f.index, &renamed[to..]);
        f.watcher.apply(&mut f.index, &[]);
        assert!(has(&f.index, &reports.join("other.txt")));
        assert!(has(&f.index, &reports.join("new.txt")));
        assert_eq!(f.index.len(), 5, "{:?}", f.paths());
        // The folder's time is kept only with its entries complete.
        assert_eq!(f.watcher.catch_up(&mut f.index).dirs_rescanned, 0);
        assert_eq!(f.index.len(), 5);
    }

    #[test]
    fn a_folder_time_covers_only_changes_that_were_applied() {
        let mut f = Fixture::new("unapplied");
        let docs = f.root().join("docs");
        fs::write(docs.join("first.txt"), b"1").unwrap();
        let events = f.watcher.wait(Some(Duration::from_secs(2))).unwrap();
        assert!(!events.is_empty());
        // Made after the events were read, before they are applied.
        fs::write(docs.join("second.txt"), b"2").unwrap();
        f.watcher.apply(&mut f.index, &events);
        assert!(has(&f.index, &docs.join("first.txt")));
        assert!(!has(&f.index, &docs.join("second.txt")));
        // The watcher goes away with the second event unread (an exit, or
        // watching turned off); the next start catches up.
        let mut index = std::mem::replace(&mut f.index, Index::new(NodeStore::new()));
        f.watcher = Watcher::new(&index, &ScanOptions::default()).unwrap();
        let stats = f.watcher.catch_up(&mut index);
        assert!(has(&index, &docs.join("second.txt")), "{stats:?}");
    }

    #[test]
    fn folder_times_are_kept_once_the_next_batch_is_applied() {
        let mut f = Fixture::new("settled-times");
        let docs = f.root().join("docs");
        fs::write(docs.join("new.txt"), b"n").unwrap();
        assert!(f.settle(|i| has(i, &docs.join("new.txt"))));
        f.watcher
            .poll(&mut f.index, Some(Duration::from_millis(50)))
            .unwrap();
        assert!(!f.watcher.has_pending());
        // Nothing changed since: a catch-up reads no folder again.
        assert_eq!(f.watcher.catch_up(&mut f.index).dirs_rescanned, 0);
        let id = f
            .index
            .order()
            .iter()
            .find(|&id| f.index.store().path(id) == docs)
            .unwrap();
        let on_disk = Meta::from_metadata(&fs::symlink_metadata(&docs).unwrap());
        assert_eq!(f.index.store().meta(id).mtime_ns, on_disk.mtime_ns);
    }

    #[test]
    fn a_folder_that_cannot_be_searched_keeps_its_entries_and_is_listed_again() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = Fixture::new("unsearchable");
        let docs = f.root().join("docs");
        let new = docs.join("new.txt");
        /// Gives the folder its permissions back, also when the test fails.
        struct Searchable(PathBuf);
        impl Drop for Searchable {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let _restore = Searchable(docs.clone());
        // Created, and the folder loses its search permission before the
        // event is applied: the new file cannot be looked at.
        fs::write(&new, b"new").unwrap();
        fs::set_permissions(&docs, fs::Permissions::from_mode(0o644)).unwrap();
        if fs::symlink_metadata(&new).is_ok() {
            fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
            panic!("test requires an unprivileged user");
        }
        assert!(f.settle(|i| i.len() == 4));
        // Known by name, without size or time.
        assert_eq!(
            f.index.store().meta(node_in(&f.index, &new)),
            Meta::default()
        );
        f.settle(|_| false);
        // The folder keeps an older time, so a catch-up reads it.
        let time = |f: &Fixture| {
            let stored = f.index.store().meta(node_in(&f.index, &docs)).mtime_ns;
            let on_disk = Meta::from_metadata(&fs::symlink_metadata(&docs).unwrap()).mtime_ns;
            stored == on_disk
        };
        assert!(!time(&f));
        // Searchable again: its permission change lists it again.
        fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(f.settle(|i| i.store().meta(node_in(i, &new)).size == 3));
        f.settle(|_| false);
        assert!(time(&f));
        assert_eq!(f.watcher.catch_up(&mut f.index).dirs_rescanned, 0);
    }

    #[test]
    fn a_folder_left_incomplete_before_watching_is_listed_again() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!(
            "everygrep-watch-incomplete-before-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let (u, v) = (root.join("u"), root.join("v"));
        fs::create_dir_all(&u).unwrap();
        fs::create_dir_all(&v).unwrap();
        fs::write(u.join("f.txt"), b"fffff").unwrap();
        fs::write(v.join("a.txt"), b"a").unwrap();
        /// Gives the folders their permissions back and removes them, also
        /// when the test fails.
        struct Cleanup(PathBuf, [PathBuf; 2]);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for dir in &self.1 {
                    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
                }
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(base.clone(), [u.clone(), v.clone()]);
        let searchable = |dir: &Path, yes: bool| {
            let mode = if yes { 0o755 } else { 0o644 };
            fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
        };
        // `u` is readable but not searchable when it is scanned.
        searchable(&u, false);
        if fs::symlink_metadata(u.join("f.txt")).is_ok() {
            panic!("test requires an unprivileged user");
        }
        let mut store = NodeStore::new();
        scan(
            &mut store,
            std::slice::from_ref(&root),
            &ScanOptions::default(),
        );
        let mut index = Index::new(store);
        // `v` gets a file while nothing watches, and is not searchable when
        // it is caught up.
        fs::write(v.join("b.txt"), b"bbbbb").unwrap();
        searchable(&v, false);
        let mut watcher = Watcher::new(&index, &ScanOptions::default()).unwrap();
        watcher.catch_up(&mut index);
        let size = |index: &Index, path: &Path| index.store().meta(node_in(index, path)).size;
        assert_eq!(size(&index, &u.join("f.txt")), 0);
        assert_eq!(size(&index, &v.join("b.txt")), 0);
        // Searchable again: the permission changes list them again.
        searchable(&u, true);
        searchable(&v, true);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && (size(&index, &u.join("f.txt")) != 5 || size(&index, &v.join("b.txt")) != 5)
        {
            watcher
                .poll(&mut index, Some(Duration::from_millis(50)))
                .unwrap();
        }
        for _ in 0..3 {
            watcher
                .poll(&mut index, Some(Duration::from_millis(50)))
                .unwrap();
        }
        assert_eq!(size(&index, &u.join("f.txt")), 5);
        assert_eq!(size(&index, &v.join("b.txt")), 5);
        assert_eq!(watcher.catch_up(&mut index).dirs_rescanned, 0);
    }

    fn node_in(index: &Index, path: &Path) -> NodeId {
        index
            .order()
            .iter()
            .find(|&id| index.store().path(id) == path)
            .unwrap()
    }

    #[test]
    fn folders_that_could_not_be_watched_are_checked_again() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = Fixture::new("unwatched");
        let docs = f.root().join("docs");
        // Indexed, but not readable while the watches are added.
        fs::set_permissions(&docs, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&docs).is_ok() {
            fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
            panic!("test requires an unprivileged user");
        }
        f.watcher = Watcher::new(&f.index, &ScanOptions::default()).unwrap();
        fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(f.watcher.failed_watches(), 1);
        assert_eq!(
            f.watcher.failed_watch_kinds().collect::<Vec<_>>(),
            [io::ErrorKind::PermissionDenied]
        );
        // Without a watch nothing reports these.
        fs::write(docs.join("new.txt"), b"n").unwrap();
        fs::remove_file(docs.join("a.txt")).unwrap();
        f.settle(|_| false);
        assert!(!has(&f.index, &docs.join("new.txt")));
        assert!(has(&f.index, &docs.join("a.txt")));

        let stats = f.watcher.recheck_unwatched(&mut f.index, 100);
        assert_eq!((stats.added, stats.removed), (1, 1), "{stats:?}");
        assert_eq!(f.watcher.failed_watches(), 0);
        assert!(has(&f.index, &docs.join("new.txt")));
        assert!(!has(&f.index, &docs.join("a.txt")));
        // Watched from now on.
        fs::write(docs.join("later.txt"), b"l").unwrap();
        assert!(f.settle(|i| has(i, &docs.join("later.txt"))));
    }

    #[test]
    fn rechecks_of_unwatched_folders_take_turns() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = Fixture::new("unwatched-turns");
        let locked: Vec<PathBuf> = (0..3).map(|i| f.root().join(format!("l{i}"))).collect();
        for dir in &locked {
            fs::create_dir(dir).unwrap();
        }
        let mut store = NodeStore::new();
        scan(&mut store, &[f.root()], &ScanOptions::default());
        f.index = Index::new(store);
        for dir in &locked {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
        }
        f.watcher = Watcher::new(&f.index, &ScanOptions::default()).unwrap();
        assert_eq!(f.watcher.failed_watches(), 3);
        for dir in &locked {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Two at a time: the second call picks up the one left over.
        f.watcher.recheck_unwatched(&mut f.index, 2);
        assert_eq!(f.watcher.failed_watches(), 1);
        f.watcher.recheck_unwatched(&mut f.index, 2);
        assert_eq!(f.watcher.failed_watches(), 0);
    }

    /// The events the kernel queues for a watched folder whose file system
    /// is unmounted.
    fn unmount_events(w: &Watcher, dirs: &[NodeId]) -> Vec<Event> {
        let mut events = Vec::new();
        for dir in dirs {
            let wd = w.watches.watch_of[dir];
            for mask in [Mask::UNMOUNT, Mask::IGNORED] {
                events.push(Event {
                    wd,
                    mask,
                    cookie: 0,
                    name: None,
                });
            }
        }
        events
    }

    #[test]
    fn an_unmounted_folder_is_reported_and_read_again() {
        let mut f = Fixture::new("unmount");
        let mnt = f.root().join("mnt");
        fs::create_dir_all(mnt.join("sub")).unwrap();
        fs::write(mnt.join("sub/on-disk.txt"), b"d").unwrap();
        assert!(f.settle(|i| has(i, &mnt.join("sub/on-disk.txt"))));
        f.settle(|_| false);
        let find = |i: &Index, p: &Path| i.order().iter().find(|&id| i.store().path(id) == p);
        let mnt_id = find(&f.index, &mnt).unwrap();
        let sub_id = find(&f.index, &mnt.join("sub")).unwrap();
        let unmount = unmount_events(&f.watcher, &[sub_id, mnt_id]);
        // What the mount covered: an empty folder with one file of its own.
        // Its events are read and dropped; the unmount events stand for them.
        fs::remove_dir_all(mnt.join("sub")).unwrap();
        fs::write(mnt.join("below.txt"), b"b").unwrap();
        while !f
            .watcher
            .wait(Some(Duration::from_millis(100)))
            .unwrap()
            .is_empty()
        {}

        let stats = f.watcher.apply(&mut f.index, &unmount);
        assert_eq!(stats.unmounted, 1);
        // The index is left alone until the owner decides.
        assert!(has(&f.index, &mnt.join("sub/on-disk.txt")));
        assert_eq!(f.watcher.take_unmounted(), [mnt_id]);
        let stats = f.watcher.reread(&mut f.index, mnt_id);
        assert_eq!((stats.added, stats.removed), (1, 2), "{stats:?}");
        assert!(!has(&f.index, &mnt.join("sub")));
        assert!(has(&f.index, &mnt.join("below.txt")));
        assert!(has(&f.index, &f.root().join("docs/a.txt")));
        // Watched again.
        fs::write(mnt.join("later.txt"), b"l").unwrap();
        assert!(f.settle(|i| has(i, &mnt.join("later.txt"))));
    }

    #[test]
    fn an_unmounted_root_is_reported() {
        let mut f = Fixture::new("unmount-root");
        let root = f.index.roots()[0];
        let docs = f.index.find_child(Some(root), b"docs").unwrap();
        let events = unmount_events(&f.watcher, &[docs, root]);
        let stats = f.watcher.apply(&mut f.index, &events);
        assert_eq!(stats.unmounted, 1);
        assert_eq!(f.watcher.take_unmounted(), [root]);
        assert!(f.watcher.take_unmounted().is_empty());
        assert_eq!(f.watcher.watch_count(), 0);
        f.watcher.reread(&mut f.index, root);
        let root_path = f.root();
        assert_eq!(
            f.paths(),
            [
                root_path.clone(),
                root_path.join("docs"),
                root_path.join("docs/a.txt")
            ]
        );
        assert_eq!(f.watcher.watch_count(), 2);
    }

    /// Reads events until one has `mask`.
    fn read_until(w: &mut Watcher, mask: Mask) -> Vec<Event> {
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !events.iter().any(|e: &Event| e.mask.contains(mask)) && Instant::now() < deadline {
            events.extend(w.wait(Some(Duration::from_millis(50))).unwrap());
        }
        events
    }

    #[test]
    fn a_rename_read_in_two_batches_stays_a_rename() {
        let mut f = Fixture::new("split-rename");
        for i in 0..20 {
            fs::write(f.root().join(format!("docs/f{i}.txt")), b"x").unwrap();
        }
        assert!(f.settle(|i| i.len() == 23));
        f.settle(|_| false);
        let docs = f
            .index
            .find_child(Some(f.index.roots()[0]), b"docs")
            .unwrap();
        let papers = f.root().join("papers");
        fs::rename(f.root().join("docs"), &papers).unwrap();
        let events = read_until(&mut f.watcher, Mask::MOVED_TO);
        let to = events
            .iter()
            .position(|e| e.mask.contains(Mask::MOVED_TO))
            .unwrap();
        let first = f.watcher.apply(&mut f.index, &events[..to]);
        assert_eq!(first.removed, 0, "{first:?}");
        assert!(f.watcher.has_pending());
        // Between the batches the folder is still at its old name.
        assert!(has(&f.index, &f.root().join("docs/a.txt")));
        let second = f.watcher.apply(&mut f.index, &events[to..]);
        assert_eq!((second.moved, second.added, second.removed), (1, 0, 0));
        assert_eq!(
            f.index.find_child(Some(f.index.roots()[0]), b"papers"),
            Some(docs)
        );
        assert!(has(&f.index, &papers.join("f7.txt")));
        assert_eq!(f.index.len(), 23);
    }

    #[test]
    fn a_rename_cut_by_its_root_going_offline_keeps_the_entries() {
        let mut f = Fixture::new("split-rename-offline");
        let root = f.index.roots()[0];
        let docs = f.root().join("docs");
        fs::rename(&docs, f.root().join("papers")).unwrap();
        let events = read_until(&mut f.watcher, Mask::MOVED_TO);
        let to = events
            .iter()
            .position(|e| e.mask.contains(Mask::MOVED_TO))
            .unwrap();
        f.watcher.apply(&mut f.index, &events[..to]);
        assert!(f.watcher.has_pending());
        // The root goes offline (a drive unplugged) between the halves.
        f.watcher.unwatch_root(&f.index, root);
        f.index.set_offline(root, true);
        let second = f.watcher.apply(&mut f.index, &events[to..]);
        let third = f.watcher.apply(&mut f.index, &[]);
        assert_eq!((second.removed, third.removed), (0, 0));
        assert!(has(&f.index, &docs.join("a.txt")), "{:?}", f.paths());
        assert_eq!(f.index.len(), 3);
        assert!(!f.watcher.has_pending());
    }

    #[test]
    fn a_move_out_is_removed_after_one_more_batch() {
        let mut f = Fixture::new("split-move-out");
        let file = f.root().join("docs/a.txt");
        fs::rename(&file, f.base.join("outside/a.txt")).unwrap();
        let events = read_until(&mut f.watcher, Mask::MOVED_FROM);
        assert_eq!(f.watcher.apply(&mut f.index, &events).removed, 0);
        assert!(has(&f.index, &file));
        assert!(f.watcher.has_pending());
        assert_eq!(f.watcher.apply(&mut f.index, &[]).removed, 1);
        assert!(!has(&f.index, &file));
    }

    #[test]
    fn a_folder_indexed_under_two_nodes_keeps_both_current() {
        // Nested roots give one folder two nodes, as a bind mount inside an
        // indexed tree would, without mounting anything.
        let base =
            std::env::temp_dir().join(format!("everygrep-watch-alias-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("x/sub")).unwrap();
        let mut store = NodeStore::new();
        let roots = [base.join("x"), base.join("x/sub")];
        scan(&mut store, &roots, &ScanOptions::default());
        let mut index = Index::new(store);
        let mut watcher = Watcher::new(&index, &ScanOptions::default()).unwrap();
        assert_eq!(watcher.watch_count(), 2, "x and x/sub");
        let count = |index: &Index, path: &Path| {
            index
                .order()
                .iter()
                .filter(|&id| index.store().path(id) == path)
                .count()
        };
        let settle = |index: &mut Index, watcher: &mut Watcher, done: &dyn Fn(&Index) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !done(index) && Instant::now() < deadline {
                watcher
                    .poll(index, Some(Duration::from_millis(50)))
                    .unwrap();
            }
            done(index)
        };
        let new = base.join("x/sub/new.txt");
        fs::write(&new, b"n").unwrap();
        let both = settle(&mut index, &mut watcher, &|i| count(i, &new) == 2);
        fs::remove_file(&new).unwrap();
        let none = settle(&mut index, &mut watcher, &|i| count(i, &new) == 0);
        // Renaming it moves one node and removes the other (its root moved
        // away); the watch stays for the one left.
        fs::rename(base.join("x/sub"), base.join("x/moved")).unwrap();
        let later = base.join("x/moved/later.txt");
        settle(&mut index, &mut watcher, &|i| {
            count(i, &base.join("x/moved")) == 1
        });
        fs::write(&later, b"l").unwrap();
        let still = settle(&mut index, &mut watcher, &|i| count(i, &later) == 1);
        let roots = index.roots().len();
        drop(watcher);
        fs::remove_dir_all(&base).unwrap();
        assert!(both, "created under both nodes");
        assert!(none, "removed under both nodes");
        assert_eq!(roots, 1);
        assert!(still, "the remaining node is still watched");
    }

    #[test]
    fn moving_out_removes_and_moving_in_adds() {
        let mut f = Fixture::new("inout");
        let outside = f.base.join("outside/docs");
        fs::rename(f.root().join("docs"), &outside).unwrap();
        let root = f.root();
        assert!(f.settle(|i| !has(i, &root.join("docs/a.txt"))));
        assert_eq!(f.paths(), vec![f.root()]);
        // The moved-out folder is no longer watched: changes there are ignored.
        fs::write(outside.join("ignored.txt"), b"i").unwrap();
        fs::rename(&outside, f.root().join("back")).unwrap();
        let back = f.root().join("back");
        assert!(f.settle(|i| has(i, &back.join("a.txt")) && has(i, &back.join("ignored.txt"))));
    }

    #[test]
    fn watching_without_file_metadata_preserves_the_policy() {
        let base =
            std::env::temp_dir().join(format!("everygrep-watch-no-meta-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("old.txt"), b"old").unwrap();
        let options = ScanOptions {
            with_metadata: false,
            ..ScanOptions::default()
        };
        let mut store = NodeStore::new();
        scan(&mut store, std::slice::from_ref(&base), &options);
        let mut index = Index::new(store);
        let mut watcher = Watcher::new(&index, &options).unwrap();
        fs::write(base.join("old.txt"), b"changed content").unwrap();
        fs::rename(base.join("old.txt"), base.join("renamed.txt")).unwrap();
        fs::create_dir(base.join("new")).unwrap();
        fs::write(base.join("new/added.txt"), b"new content").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut moved = 0;
        while !(has(&index, &base.join("new/added.txt")) && has(&index, &base.join("renamed.txt")))
            && Instant::now() < deadline
        {
            moved += watcher
                .poll(&mut index, Some(Duration::from_millis(50)))
                .unwrap()
                .moved;
        }
        assert!(has(&index, &base.join("new/added.txt")));
        assert!(has(&index, &base.join("renamed.txt")));
        assert!(!has(&index, &base.join("old.txt")));
        assert_eq!(moved, 1, "the rename was applied as a move");
        for id in index.order().iter().filter(|&id| !index.store().is_dir(id)) {
            assert_eq!(index.store().meta(id), Meta::default());
        }
        drop(watcher);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn catch_up_adds_offline_changes_and_watches_new_folders() {
        let base =
            std::env::temp_dir().join(format!("everygrep-watch-offline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root")).unwrap();
        let mut store = NodeStore::new();
        scan(&mut store, &[base.join("root")], &ScanOptions::default());
        let mut index = Index::new(store);
        // Changes while not running.
        fs::create_dir_all(base.join("root/offline/inner")).unwrap();
        fs::write(base.join("root/offline/inner/o.txt"), b"o").unwrap();

        let mut watcher = Watcher::new(&index, &ScanOptions::default()).unwrap();
        let stats = watcher.catch_up(&mut index);
        assert_eq!(stats.added, 3, "{stats:?}");
        assert!(has(&index, &base.join("root/offline/inner/o.txt")));
        // The folder found by the catch-up is watched.
        let later = base.join("root/offline/inner/later.txt");
        fs::write(&later, b"l").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !has(&index, &later) && Instant::now() < deadline {
            watcher
                .poll(&mut index, Some(Duration::from_millis(50)))
                .unwrap();
        }
        fs::remove_dir_all(&base).unwrap();
        assert!(has(&index, &later));
    }

    /// An index of `base/root` with `n` folders of one file each, after
    /// which every folder gets a second file, one in ten is removed and a new
    /// one is made, so a catch-up reads every folder again.
    fn changed_tree(base: &Path, n: usize) -> Index {
        let _ = fs::remove_dir_all(base);
        let root = base.join("root");
        for i in 0..n {
            fs::create_dir_all(root.join(format!("d{i}"))).unwrap();
            fs::write(root.join(format!("d{i}/a.txt")), b"a").unwrap();
        }
        let mut store = NodeStore::new();
        scan(
            &mut store,
            std::slice::from_ref(&root),
            &ScanOptions::default(),
        );
        for i in 0..n {
            if i % 10 == 0 {
                fs::remove_dir_all(root.join(format!("d{i}"))).unwrap();
            } else {
                fs::write(root.join(format!("d{i}/b.txt")), b"b").unwrap();
            }
        }
        fs::create_dir_all(root.join("new/inner")).unwrap();
        fs::write(root.join("new/inner/n.txt"), b"n").unwrap();
        Index::new(store)
    }

    fn sorted_paths(index: &Index) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = index
            .order()
            .iter()
            .map(|id| index.store().path(id))
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn a_shared_catch_up_lets_readers_in_between_batches() {
        let base =
            std::env::temp_dir().join(format!("everygrep-watch-shared-{}", std::process::id()));
        let index = changed_tree(&base, 4000);
        let options = ScanOptions::default();
        let mut watcher = Watcher::new(&index, &options).unwrap();
        let roots = index.roots();
        let total = crate::sync::folders_below(&index, &roots);
        let index = std::sync::Arc::new(RwLock::new(index));
        let progress = std::sync::Arc::new(Progress::new());
        progress.set_total(Some(total));
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reader = std::thread::spawn({
            let (index, progress, running) = (index.clone(), progress.clone(), running.clone());
            move || {
                use std::sync::atomic::Ordering;
                let mut seen = Vec::new();
                while running.load(Ordering::SeqCst) {
                    let index = index.read().unwrap();
                    seen.push(progress.done());
                    drop(index);
                    std::thread::sleep(Duration::from_micros(200));
                }
                seen
            }
        });
        let stats = watcher.catch_up_shared(&index, &roots, &progress, &|| false);
        running.store(false, std::sync::atomic::Ordering::SeqCst);
        let seen = reader.join().unwrap();
        let done = progress.done();
        assert!(
            seen.iter().any(|&d| d > 0 && d < done),
            "no read between the first and the last folder ({done} folders)"
        );
        assert!(stats.added > 0 && stats.removed > 0, "{stats:?}");

        // The same result as a fresh scan, and new folders are watched.
        let mut store = NodeStore::new();
        scan(&mut store, &[base.join("root")], &options);
        let Ok(index) = std::sync::Arc::try_unwrap(index) else {
            panic!("the index is still shared");
        };
        let index = index.into_inner().unwrap();
        assert_eq!(sorted_paths(&index), sorted_paths(&Index::new(store)));
        let folders = index
            .order()
            .iter()
            .filter(|&id| index.store().is_dir(id))
            .count();
        assert_eq!(watcher.watch_count(), folders);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_stopped_shared_catch_up_leaves_the_rest_to_the_next_one() {
        let base =
            std::env::temp_dir().join(format!("everygrep-watch-stopped-{}", std::process::id()));
        let index = changed_tree(&base, 4000);
        let options = ScanOptions::default();
        let mut watcher = Watcher::new(&index, &options).unwrap();
        let roots = index.roots();
        let total = crate::sync::folders_below(&index, &roots);
        let index = RwLock::new(index);
        let progress = Progress::new();
        watcher.catch_up_shared(&index, &roots, &progress, &|| true);
        assert!(progress.done() < total, "{} of {total}", progress.done());

        let mut index = index.into_inner().unwrap();
        watcher.catch_up(&mut index);
        let mut store = NodeStore::new();
        scan(&mut store, &[base.join("root")], &options);
        assert_eq!(sorted_paths(&index), sorted_paths(&Index::new(store)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn moved_out_then_recreated_with_same_name_stays_indexed() {
        let mut f = Fixture::new("recreate");
        let file = f.root().join("docs/a.txt");
        let dir = f.root().join("docs");
        // Both changes land in the same batch: the old file leaves, a new one appears.
        fs::rename(&file, f.base.join("outside/a.txt")).unwrap();
        fs::write(&file, b"new").unwrap();
        assert!(f.settle(|i| has(i, &file)));
        f.watcher
            .poll(&mut f.index, Some(Duration::from_millis(200)))
            .unwrap();
        assert!(has(&f.index, &file), "re-created file was dropped");
        // The same for a folder.
        fs::rename(&dir, f.base.join("outside/docs")).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("fresh.txt"), b"f").unwrap();
        assert!(f.settle(|i| has(i, &dir.join("fresh.txt"))));
        // Let any remaining events arrive, then check nothing was lost.
        f.watcher
            .poll(&mut f.index, Some(Duration::from_millis(200)))
            .unwrap();
        assert!(has(&f.index, &dir));
        assert!(has(&f.index, &dir.join("fresh.txt")));
        assert!(!has(&f.index, &dir.join("a.txt")));
        // The re-created folder is watched.
        fs::write(dir.join("later.txt"), b"l").unwrap();
        assert!(f.settle(|i| has(i, &dir.join("later.txt"))));
    }

    #[test]
    fn watches_keep_working_after_compaction() {
        let mut f = Fixture::new("compact");
        let docs = f.root().join("docs");
        for i in 0..20 {
            fs::create_dir(f.root().join(format!("tmp{i}"))).unwrap();
        }
        assert!(f.settle(|i| i.len() == 23));
        for i in 0..20 {
            fs::remove_dir(f.root().join(format!("tmp{i}"))).unwrap();
        }
        assert!(f.settle(|i| i.len() == 3));
        let watches_before = f.watcher.watch_count();
        f.watcher.compact(&mut f.index);
        assert_eq!(f.index.store().len(), 3, "removed slots are gone");
        assert_eq!(f.watcher.watch_count(), watches_before);
        // Events in folders that survived still reach the right nodes.
        fs::write(docs.join("after.txt"), b"x").unwrap();
        assert!(f.settle(|i| has(i, &docs.join("after.txt"))));
        fs::create_dir(f.root().join("newdir")).unwrap();
        fs::write(f.root().join("newdir/n.txt"), b"n").unwrap();
        let n = f.root().join("newdir/n.txt");
        assert!(f.settle(|i| has(i, &n)));
    }

    /// Folder paths with their total size and item count.
    fn folder_sizes(index: &Index) -> Vec<(PathBuf, u64, u32)> {
        let store = index.store();
        let mut v: Vec<_> = index
            .order()
            .iter()
            .filter(|&id| store.is_dir(id))
            .map(|id| (store.path(id), store.meta(id).size, store.items(id)))
            .collect();
        v.sort();
        v
    }

    fn scanned_sizes(root: &Path) -> Vec<(PathBuf, u64, u32)> {
        let mut store = NodeStore::new();
        scan(&mut store, &[root.to_path_buf()], &ScanOptions::default());
        folder_sizes(&Index::new(store))
    }

    #[test]
    fn folder_sizes_follow_live_changes() {
        let mut f = Fixture::new("sizes");
        let root = f.root();
        let same = |f: &mut Fixture| f.settle(|i| folder_sizes(i) == scanned_sizes(&root));
        fs::create_dir_all(root.join("x/y")).unwrap();
        fs::write(root.join("x/y/big.bin"), vec![0u8; 5000]).unwrap();
        fs::write(root.join("x/small.txt"), b"12345").unwrap();
        assert!(same(&mut f), "create");
        fs::write(root.join("x/small.txt"), vec![1u8; 700]).unwrap();
        assert!(same(&mut f), "grow");
        fs::rename(root.join("x/y/big.bin"), root.join("docs/big.bin")).unwrap();
        assert!(same(&mut f), "move between folders");
        fs::rename(root.join("docs/big.bin"), f.base.join("outside/big.bin")).unwrap();
        assert!(same(&mut f), "move out");
        fs::rename(f.base.join("outside/big.bin"), root.join("x/y/in.bin")).unwrap();
        assert!(same(&mut f), "move in");
        fs::rename(root.join("x"), root.join("docs/x2")).unwrap();
        assert!(same(&mut f), "move a folder");
        fs::remove_dir_all(root.join("docs/x2")).unwrap();
        assert!(same(&mut f), "remove a folder");
        let docs = folder_sizes(&f.index);
        assert_eq!(docs[1], (root.join("docs"), 1, 1));
    }

    #[test]
    fn removing_a_folder_tree_removes_all_nodes() {
        let mut f = Fixture::new("rmtree");
        fs::create_dir_all(f.root().join("docs/sub")).unwrap();
        fs::write(f.root().join("docs/sub/s.txt"), b"s").unwrap();
        let sub = f.root().join("docs/sub/s.txt");
        assert!(f.settle(|i| has(i, &sub)));
        fs::remove_dir_all(f.root().join("docs")).unwrap();
        let root = f.root();
        assert!(f.settle(|i| i.len() == 1 && has(i, &root)));
    }

    #[test]
    fn offline_roots_are_not_watched_or_caught_up() {
        let mut f = Fixture::new("offline-root");
        // A second root, as a drive would be, then "unplugged".
        let drive = f.base.join("drive");
        fs::create_dir_all(drive.join("photos")).unwrap();
        fs::write(drive.join("photos/p1.jpg"), b"p").unwrap();
        let mut store = NodeStore::new();
        scan(
            &mut store,
            std::slice::from_ref(&drive),
            &ScanOptions::default(),
        );
        let root = f.index.graft(&Index::new(store))[0];
        f.watcher.watch_root(&f.index, root);
        fs::write(drive.join("photos/p2.jpg"), b"p").unwrap();
        assert!(f.settle(|i| has(i, &drive.join("photos/p2.jpg"))));

        f.watcher.unwatch_root(&f.index, root);
        f.index.set_offline(root, true);
        let gone = f.base.join("drive-elsewhere");
        fs::rename(&drive, &gone).unwrap();
        // Nothing is removed: no events, and the catch-up skips the root.
        let stats = f.watcher.catch_up(&mut f.index);
        assert_eq!(stats.removed, 0);
        f.settle(|_| false);
        assert!(has(&f.index, &drive.join("photos/p1.jpg")));
        assert!(has(&f.index, &drive.join("photos/p2.jpg")));
        // A new watcher leaves it alone too.
        let w = Watcher::new(&f.index, &ScanOptions::default()).unwrap();
        assert_eq!(w.watch_count(), 2, "root and root/docs only");

        // Back at the same place: online again, caught up, watched.
        fs::rename(&gone, &drive).unwrap();
        fs::remove_file(drive.join("photos/p1.jpg")).unwrap();
        f.index.set_offline(root, false);
        f.watcher.watch_root(&f.index, root);
        let stats = f.watcher.catch_up_roots(&mut f.index, &[root]);
        assert_eq!(stats.removed, 1);
        assert!(!has(&f.index, &drive.join("photos/p1.jpg")));
        fs::write(drive.join("photos/p3.jpg"), b"p").unwrap();
        assert!(f.settle(|i| has(i, &drive.join("photos/p3.jpg"))));
    }
}
