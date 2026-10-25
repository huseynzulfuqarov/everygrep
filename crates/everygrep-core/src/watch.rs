//! Keeping an [`Index`] up to date with inotify.
//!
//! Every indexed folder gets a watch. Events are read in batches and applied to
//! the index: creations add nodes (a new folder is scanned at once, since files
//! may appear in it before its watch exists), deletions remove nodes, and a
//! `MOVED_FROM`/`MOVED_TO` pair with the same cookie becomes a rename, which keeps
//! the node id so paths below a renamed folder stay correct for free. A move out
//! of the watched trees is a removal; a move in from outside is a creation.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

use crate::index::Index;
use crate::inotify::{Event, Inotify, Mask, WatchId};
use crate::scan::{ScanOptions, skip_set};
use crate::store::{Meta, NodeId};
use crate::sync::{CatchUpStats, add_tree, catch_up_with};

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
}

/// Watches all indexed folders and applies their changes to the index.
pub struct Watcher {
    inotify: Inotify,
    dir_of: HashMap<WatchId, NodeId>,
    watch_of: HashMap<NodeId, WatchId>,
    skip: HashSet<PathBuf>,
    buf: Vec<u8>,
    /// Folders that could not be watched (for example permission denied).
    pub failed_watches: usize,
}

impl Watcher {
    /// Creates a watcher with a watch on every live folder of `index`. Folders
    /// that a scan with `options` would not enter are not watched either.
    pub fn new(index: &Index, options: &ScanOptions) -> io::Result<Self> {
        let mut w = Self {
            inotify: Inotify::new()?,
            dir_of: HashMap::new(),
            watch_of: HashMap::new(),
            skip: skip_set(options),
            buf: Vec::new(),
            failed_watches: 0,
        };
        let dirs: Vec<NodeId> = index
            .order()
            .iter()
            .filter(|&id| index.store().is_dir(id))
            .collect();
        for dir in dirs {
            w.watch(index, dir);
        }
        Ok(w)
    }

    /// Brings `index` up to date with changes made while nothing was watching
    /// (see [`crate::sync`]) and adjusts the watches to match. Call it after
    /// [`new`](Self::new), so changes made during the catch-up are still reported.
    pub fn catch_up(&mut self, index: &mut Index) -> CatchUpStats {
        let (stats, changes) = catch_up_with(index, &self.skip);
        self.forget(&changes.removed);
        for dir in changes.new_dirs {
            self.watch(index, dir);
        }
        stats
    }

    /// Number of active watches.
    pub fn watch_count(&self) -> usize {
        self.dir_of.len()
    }

    /// Waits up to `timeout` for events and applies them to `index`.
    pub fn poll(&mut self, index: &mut Index, timeout: Option<Duration>) -> io::Result<ApplyStats> {
        let events = self.inotify.read_events(&mut self.buf, timeout)?;
        Ok(self.apply(index, &events))
    }

    /// Applies a batch of events to `index`.
    pub fn apply(&mut self, index: &mut Index, events: &[Event]) -> ApplyStats {
        let mut stats = ApplyStats::default();
        let mut moved_out: HashMap<u32, NodeId> = HashMap::new();
        let mut touched: HashSet<NodeId> = HashSet::new();
        for ev in events {
            if ev.mask.contains(Mask::Q_OVERFLOW) {
                stats.overflow = true;
                continue;
            }
            if ev.mask.contains(Mask::IGNORED) {
                if let Some(dir) = self.dir_of.remove(&ev.wd) {
                    if self.watch_of.get(&dir) == Some(&ev.wd) {
                        self.watch_of.remove(&dir);
                    }
                }
                continue;
            }
            let Some(&dir) = self.dir_of.get(&ev.wd) else {
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
                    stats.removed += self.remove(index, dir);
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
                    moved_out.insert(ev.cookie, node);
                }
            } else if ev.mask.contains(Mask::MOVED_TO) {
                match moved_out.remove(&ev.cookie) {
                    Some(node) if index.rename(node, Some(dir), name).is_ok() => {
                        stats.moved += 1;
                        self.refresh_meta(index, node, &mut stats);
                    }
                    Some(node) => {
                        stats.removed += self.remove(index, node);
                        stats.added += self.add_from_disk(index, dir, name);
                    }
                    None => stats.added += self.add_from_disk(index, dir, name),
                }
            } else if ev.mask.contains(Mask::CREATE) {
                stats.added += self.add_from_disk(index, dir, name);
            } else if ev.mask.contains(Mask::DELETE) {
                if let Some(node) = index.find_child(Some(dir), name) {
                    stats.removed += self.remove(index, node);
                }
            } else if ev
                .mask
                .intersects(Mask::ATTRIB | Mask::CLOSE_WRITE | Mask::MODIFY)
            {
                if let Some(node) = index.find_child(Some(dir), name) {
                    self.refresh_meta(index, node, &mut stats);
                }
            }
        }
        // A move whose destination was never reported left the watched trees.
        for node in moved_out.into_values() {
            stats.removed += self.remove(index, node);
        }
        // Keep folder times current, so the next catch-up skips these folders.
        for dir in touched {
            if index.store().is_live(dir) {
                if let Ok(m) = fs::symlink_metadata(index.store().path(dir)) {
                    index.set_meta(dir, Meta::from_metadata(&m));
                }
            }
        }
        stats
    }

    fn watch(&mut self, index: &Index, dir: NodeId) {
        let path = index.store().path(dir);
        if self.skip.contains(&path) {
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
            Err(_) => self.failed_watches += 1,
        }
    }

    fn remove(&mut self, index: &mut Index, node: NodeId) -> usize {
        let removed = index.remove(node);
        self.forget(&removed);
        removed.len()
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

    fn refresh_meta(&self, index: &mut Index, node: NodeId, stats: &mut ApplyStats) {
        if let Ok(m) = fs::symlink_metadata(index.store().path(node)) {
            index.set_meta(node, Meta::from_metadata(&m));
            stats.updated += 1;
        }
    }

    /// Adds `parent/name` as it is on disk now, including a folder's contents.
    /// Returns the number of nodes added.
    fn add_from_disk(&mut self, index: &mut Index, parent: NodeId, name: &[u8]) -> usize {
        let path = index
            .store()
            .path(parent)
            .join(std::ffi::OsStr::from_bytes(name));
        let mut new_dirs = Vec::new();
        let added = add_tree(index, parent, name, &path, &self.skip, &mut new_dirs);
        for dir in new_dirs {
            self.watch(index, dir);
        }
        added
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
}
