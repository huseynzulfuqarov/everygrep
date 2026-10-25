//! Keeping an [`Index`] up to date with inotify.
//!
//! Every indexed folder gets a watch. Events are read in batches and applied to
//! the index: creations add nodes (a new folder is scanned at once, since files
//! may appear in it before its watch exists), deletions remove nodes, and a
//! `MOVED_FROM`/`MOVED_TO` pair with the same cookie becomes a rename, which keeps
//! the node id so paths below a renamed folder stay correct for free. A move out
//! of the watched trees is a removal; a move in from outside is a creation.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use crate::index::Index;
use crate::inotify::{Event, Inotify, Mask, WatchId};
use crate::store::{Meta, NodeId};

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
    buf: Vec<u8>,
    /// Folders that could not be watched (for example permission denied).
    pub failed_watches: usize,
}

impl Watcher {
    /// Creates a watcher with a watch on every live folder of `index`.
    pub fn new(index: &Index) -> io::Result<Self> {
        let mut w = Self {
            inotify: Inotify::new()?,
            dir_of: HashMap::new(),
            watch_of: HashMap::new(),
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
        stats
    }

    fn watch(&mut self, index: &Index, dir: NodeId) {
        let path = index.store().path(dir);
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
        for id in &removed {
            if let Some(wd) = self.watch_of.remove(id) {
                self.dir_of.remove(&wd);
                // The folder may still exist elsewhere (moved out), so stop watching it.
                let _ = self.inotify.remove_watch(wd);
            }
        }
        removed.len()
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
        let Ok(m) = fs::symlink_metadata(&path) else {
            return 0; // already gone again
        };
        let id = index.insert(Some(parent), name, m.is_dir(), Meta::from_metadata(&m));
        if !m.is_dir() {
            return 1;
        }
        self.watch(index, id);
        1 + self.scan_new_folder(index, id, &path)
    }

    fn scan_new_folder(&mut self, index: &mut Index, dir: NodeId, path: &Path) -> usize {
        let mut added = 0;
        let mut pending = vec![(dir, path.to_path_buf())];
        while let Some((dir_id, dir_path)) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir_path) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(m) = entry.metadata() else { continue };
                let id = index.insert(
                    Some(dir_id),
                    entry.file_name().as_bytes(),
                    m.is_dir(),
                    Meta::from_metadata(&m),
                );
                added += 1;
                if m.is_dir() {
                    self.watch(index, id);
                    pending.push((id, entry.path()));
                }
            }
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use crate::store::NodeStore;
    use std::path::PathBuf;
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
            let watcher = Watcher::new(&index).unwrap();
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
