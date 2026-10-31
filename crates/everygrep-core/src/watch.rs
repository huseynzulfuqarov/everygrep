//! Keeping an [`Index`] up to date with inotify.
//!
//! Every indexed folder gets a watch. Events are read in batches and applied to
//! the index: creations add nodes, deletions remove nodes, and a
//! `MOVED_FROM`/`MOVED_TO` pair with the same cookie becomes a rename, which keeps
//! the node id so paths below a renamed folder stay correct for free. A move out
//! of the watched trees is a removal; a move in from outside is a creation.
//!
//! A new folder is watched before its contents are read, so entries created
//! while it is being read are either seen by the read or reported as events.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;

use crate::index::Index;
use crate::inotify::{Event, Inotify, Mask, WatchId};
use crate::progress::Progress;
use crate::scan::{ScanOptions, Skip};
use crate::store::{Meta, NodeId};
use crate::sync::{CatchUpStats, add_tree, catch_up_with, online_roots};

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
}

/// The inotify instance and the mapping between watches and folder nodes.
struct Watches {
    inotify: Inotify,
    dir_of: HashMap<WatchId, NodeId>,
    watch_of: HashMap<NodeId, WatchId>,
    failed: usize,
}

impl Watches {
    fn watch(&mut self, index: &Index, dir: NodeId, skip: &Skip) {
        let path = index.store().path(dir);
        if !skip.enters(&path) {
            return;
        }
        match self.inotify.add_watch(&path, Mask::INDEX_EVENTS) {
            Ok(wd) => {
                if let Some(old) = self.dir_of.insert(wd, dir) {
                    // Same inode under a new node (re-created after a missed event).
                    self.watch_of.remove(&old);
                }
                self.watch_of.insert(dir, wd);
            }
            Err(_) => self.failed += 1,
        }
    }

    /// Translates node ids after the index was compacted.
    fn remap(&mut self, remap: &[Option<NodeId>]) {
        let old = std::mem::take(&mut self.dir_of);
        self.watch_of.clear();
        for (wd, dir) in old {
            match remap.get(dir.index()).copied().flatten() {
                Some(new) => {
                    self.dir_of.insert(wd, new);
                    self.watch_of.insert(new, wd);
                }
                None => {
                    let _ = self.inotify.remove_watch(wd);
                }
            }
        }
    }

    /// Drops the watches of removed folders.
    fn forget(&mut self, removed: &[NodeId]) {
        for id in removed {
            if let Some(wd) = self.watch_of.remove(id) {
                self.dir_of.remove(&wd);
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
                failed: 0,
            },
            skip: Skip::new(options),
            buf: Vec::new(),
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
    }

    /// Stops watching the folders below `root`, for a root that goes
    /// offline. Events already read for them are ignored too, since the
    /// mapping from their watches is gone.
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
        let watches = &mut self.watches;
        let skip = &self.skip;
        let stats = catch_up_with(
            index,
            roots,
            skip,
            &mut |idx: &Index, dir| watches.watch(idx, dir, skip),
            progress,
        );
        self.watches.forget(&index.take_removed());
        stats
    }

    /// Number of active watches.
    pub fn watch_count(&self) -> usize {
        self.watches.dir_of.len()
    }

    /// Folders that could not be watched (for example permission denied).
    pub fn failed_watches(&self) -> usize {
        self.watches.failed
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
        remap
    }

    /// Applies a batch of events to `index`.
    pub fn apply(&mut self, index: &mut Index, events: &[Event]) -> ApplyStats {
        let mut stats = ApplyStats::default();
        // Nodes between MOVED_FROM and MOVED_TO, parked under a name that no real
        // file can have, so a new entry with the old name is not mistaken for them.
        let mut moved_out: HashMap<u32, NodeId> = HashMap::new();
        let mut touched: HashSet<NodeId> = HashSet::new();
        for ev in events {
            if ev.mask.contains(Mask::Q_OVERFLOW) {
                stats.overflow = true;
                continue;
            }
            if ev.mask.contains(Mask::IGNORED) {
                if let Some(dir) = self.watches.dir_of.remove(&ev.wd) {
                    if self.watches.watch_of.get(&dir) == Some(&ev.wd) {
                        self.watches.watch_of.remove(&dir);
                    }
                }
                continue;
            }
            let Some(&dir) = self.watches.dir_of.get(&ev.wd) else {
                continue;
            };
            if !index.store().is_live(dir) {
                continue;
            }
            let Some(name) = ev.name.as_deref().map(|n| n.as_bytes()) else {
                // Events about the watched folder itself: its parent reports them too,
                // except for a root, which has no parent.
                if ev.mask.intersects(Mask::DELETE_SELF | Mask::MOVE_SELF)
                    && index.store().parent(dir).is_none()
                {
                    stats.removed += index.remove(dir).len();
                }
                continue;
            };

            if ev
                .mask
                .intersects(Mask::CREATE | Mask::DELETE | Mask::MOVED_FROM | Mask::MOVED_TO)
            {
                touched.insert(dir);
            }
            if ev.mask.contains(Mask::MOVED_FROM) {
                if let Some(node) = index.find_child(Some(dir), name) {
                    if index
                        .rename(node, Some(dir), &parked_name(ev.cookie))
                        .is_ok()
                    {
                        if let Some(previous) = moved_out.insert(ev.cookie, node) {
                            stats.removed += index.remove(previous).len();
                        }
                    } else {
                        stats.removed += index.remove(node).len();
                    }
                }
            } else if ev.mask.contains(Mask::MOVED_TO) {
                match moved_out.remove(&ev.cookie) {
                    Some(node)
                        if self.keeps_on_move(index, node, dir, name)
                            && index.rename(node, Some(dir), name).is_ok() =>
                    {
                        stats.moved += 1;
                        refresh_meta(index, node, &mut stats);
                    }
                    Some(node) => {
                        stats.removed += index.remove(node).len();
                        stats.added += self.add_from_disk(index, dir, name);
                    }
                    None => stats.added += self.add_from_disk(index, dir, name),
                }
            } else if ev.mask.contains(Mask::CREATE) {
                stats.added += self.add_from_disk(index, dir, name);
            } else if ev.mask.contains(Mask::DELETE) {
                if let Some(node) = index.find_child(Some(dir), name) {
                    stats.removed += index.remove(node).len();
                }
            } else if ev
                .mask
                .intersects(Mask::ATTRIB | Mask::CLOSE_WRITE | Mask::MODIFY)
            {
                if let Some(node) = index.find_child(Some(dir), name) {
                    refresh_meta(index, node, &mut stats);
                }
            }
        }
        // A move whose destination was never reported left the watched trees.
        for node in moved_out.into_values() {
            stats.removed += index.remove(node).len();
        }
        // Keep folder times current, so the next catch-up skips these folders.
        for dir in touched {
            if index.store().is_live(dir) {
                if let Ok(m) = fs::symlink_metadata(index.store().path(dir)) {
                    index.set_meta(dir, Meta::from_metadata(&m));
                }
            }
        }
        self.watches.forget(&index.take_removed());
        stats
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
    /// added.
    fn add_from_disk(&mut self, index: &mut Index, parent: NodeId, name: &[u8]) -> usize {
        let path = index
            .store()
            .path(parent)
            .join(std::ffi::OsStr::from_bytes(name));
        let watches = &mut self.watches;
        let skip = &self.skip;
        add_tree(index, parent, name, &path, skip, &mut |idx: &Index, dir| {
            watches.watch(idx, dir, skip)
        })
    }
}

/// Temporary name of a node whose move has not been matched yet. File names
/// cannot contain NUL, so it never collides with a real entry.
fn parked_name(cookie: u32) -> Vec<u8> {
    let mut name = b"\0moving\0".to_vec();
    name.extend_from_slice(&cookie.to_le_bytes());
    name
}

fn refresh_meta(index: &mut Index, node: NodeId, stats: &mut ApplyStats) {
    if let Ok(m) = fs::symlink_metadata(index.store().path(node)) {
        index.set_meta(node, Meta::from_metadata(&m));
        stats.updated += 1;
    }
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
