//! Catching up with changes made while the index was not watched.
//!
//! Adding or removing an entry changes its folder's modification time. Catching
//! up therefore reads the time of every indexed folder and re-reads only folders
//! whose time differs from the saved one. Folders that vanished are removed with
//! their subtree; new folders are scanned completely.
//!
//! Changing a file's contents does not touch its folder, so sizes and times of
//! files in unchanged folders are not refreshed here; a full rescan does that.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::scan::{ScanOptions, skip_set};
use crate::store::{Meta, NodeId};

/// Counters from [`catch_up`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatchUpStats {
    /// Folders whose modification time was read.
    pub dirs_checked: usize,
    /// Folders whose contents were read again because their time changed.
    pub dirs_rescanned: usize,
    /// Nodes added.
    pub added: usize,
    /// Nodes removed.
    pub removed: usize,
    /// Nodes whose metadata was refreshed.
    pub updated: usize,
    /// Folders that could not be read.
    pub errors: usize,
}

/// Brings `index` up to date with the disk. See the module documentation for
/// what is and is not detected.
pub fn catch_up(index: &mut Index, options: &ScanOptions) -> CatchUpStats {
    let stats = catch_up_with(index, &skip_set(options), &mut |_: &Index, _| {});
    // Nobody keeps per-node state here, so the removal log is not needed.
    index.take_removed();
    stats
}

/// Catch-up that calls `on_new_dir` for every folder it adds, before reading
/// that folder. Removed ids are left in the index's removal log.
pub(crate) fn catch_up_with(
    index: &mut Index,
    skip: &HashSet<PathBuf>,
    on_new_dir: &mut dyn FnMut(&Index, NodeId),
) -> CatchUpStats {
    let mut stats = CatchUpStats::default();

    let store = index.store();
    let mut children: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut queue = VecDeque::new();
    for id in index.order().iter() {
        match store.parent(id) {
            Some(p) => children.entry(p).or_default().push(id),
            None if store.is_dir(id) => queue.push_back(id),
            None => {}
        }
    }

    while let Some(dir) = queue.pop_front() {
        if !index.store().is_live(dir) {
            continue;
        }
        stats.dirs_checked += 1;
        let path = index.store().path(dir);
        let m = match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => m,
            _ => {
                stats.removed += index.remove(dir).len();
                continue;
            }
        };
        let meta = Meta::from_metadata(&m);
        let kids = children.remove(&dir).unwrap_or_default();
        if skip.contains(&path) {
            index.set_meta(dir, meta);
            continue;
        }
        if meta.mtime_ns == index.store().meta(dir).mtime_ns {
            queue.extend(kids.into_iter().filter(|&k| index.store().is_dir(k)));
            continue;
        }

        let Ok(entries) = fs::read_dir(&path) else {
            stats.errors += 1;
            continue;
        };
        stats.dirs_rescanned += 1;
        let mut on_disk: HashMap<Vec<u8>, Meta> = HashMap::new();
        let mut disk_is_dir: HashMap<Vec<u8>, bool> = HashMap::new();
        for entry in entries.flatten() {
            let Ok(em) = entry.metadata() else { continue };
            let name = entry.file_name().as_bytes().to_vec();
            disk_is_dir.insert(name.clone(), em.is_dir());
            on_disk.insert(name, Meta::from_metadata(&em));
        }
        for kid in kids {
            if !index.store().is_live(kid) {
                continue;
            }
            let name = index.store().name(kid).to_vec();
            match (on_disk.remove(&name), disk_is_dir.get(&name)) {
                (Some(m), Some(&is_dir)) if is_dir == index.store().is_dir(kid) => {
                    if is_dir {
                        // A folder keeps its saved time until it is visited itself;
                        // updating it here would hide its own changes.
                        queue.push_back(kid);
                    } else if index.store().meta(kid) != m {
                        index.set_meta(kid, m);
                        stats.updated += 1;
                    }
                }
                (still_there, _) => {
                    stats.removed += index.remove(kid).len();
                    if still_there.is_some() {
                        stats.added += add_tree(
                            index,
                            dir,
                            &name,
                            &path.join(OsStr::from_bytes(&name)),
                            skip,
                            on_new_dir,
                        );
                    }
                }
            }
        }
        for name in on_disk.into_keys() {
            stats.added += add_tree(
                index,
                dir,
                &name,
                &path.join(OsStr::from_bytes(&name)),
                skip,
                on_new_dir,
            );
        }
        index.set_meta(dir, meta);
    }
    stats
}

/// Adds the entry `name` at `path` under `parent`, and for a folder all entries
/// below it. `on_new_dir` is called for each added folder before its contents
/// are read. Returns how many nodes were added.
pub(crate) fn add_tree(
    index: &mut Index,
    parent: NodeId,
    name: &[u8],
    path: &Path,
    skip: &HashSet<PathBuf>,
    on_new_dir: &mut dyn FnMut(&Index, NodeId),
) -> usize {
    let Ok(m) = fs::symlink_metadata(path) else {
        return 0;
    };
    let id = index.insert(Some(parent), name, m.is_dir(), Meta::from_metadata(&m));
    if !m.is_dir() {
        return 1;
    }
    let mut added = 1;
    let mut pending = vec![(id, path.to_path_buf())];
    while let Some((dir_id, dir_path)) = pending.pop() {
        if skip.contains(&dir_path) {
            continue;
        }
        on_new_dir(index, dir_id);
        let Ok(entries) = fs::read_dir(&dir_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(em) = entry.metadata() else { continue };
            let child = index.insert(
                Some(dir_id),
                entry.file_name().as_bytes(),
                em.is_dir(),
                Meta::from_metadata(&em),
            );
            added += 1;
            if em.is_dir() {
                pending.push((child, entry.path()));
            }
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;
    use crate::store::NodeStore;

    fn tree(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("everygrep-sync-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("keep/deep")).unwrap();
        fs::create_dir_all(base.join("gone/sub")).unwrap();
        fs::create_dir_all(base.join("old-name")).unwrap();
        fs::write(base.join("keep/a.txt"), b"a").unwrap();
        fs::write(base.join("keep/deep/b.txt"), b"b").unwrap();
        fs::write(base.join("gone/sub/c.txt"), b"c").unwrap();
        fs::write(base.join("old-name/d.txt"), b"d").unwrap();
        fs::write(base.join("flip"), b"file that becomes a folder").unwrap();
        base
    }

    fn index_of(root: &Path) -> Index {
        let mut store = NodeStore::new();
        scan(&mut store, &[root.to_path_buf()], &ScanOptions::default());
        Index::new(store)
    }

    fn paths(index: &Index) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = index
            .order()
            .iter()
            .map(|id| index.store().path(id))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn catch_up_matches_a_fresh_scan() {
        let base = tree("match");
        let mut index = index_of(&base);

        fs::write(base.join("keep/new.txt"), b"n").unwrap();
        fs::remove_file(base.join("keep/a.txt")).unwrap();
        fs::remove_dir_all(base.join("gone")).unwrap();
        fs::rename(base.join("old-name"), base.join("new-name")).unwrap();
        fs::create_dir_all(base.join("keep/deep/x/y")).unwrap();
        fs::write(base.join("keep/deep/x/y/z.txt"), b"z").unwrap();
        fs::remove_file(base.join("flip")).unwrap();
        fs::create_dir(base.join("flip")).unwrap();
        fs::write(base.join("flip/inside.txt"), b"i").unwrap();

        let stats = catch_up(&mut index, &ScanOptions::default());
        let fresh = index_of(&base);
        fs::remove_dir_all(&base).unwrap();

        assert_eq!(paths(&index), paths(&fresh));
        assert!(stats.removed >= 4, "{stats:?}");
        assert!(stats.added >= 6, "{stats:?}");
    }

    #[test]
    fn unchanged_tree_reads_no_folder_contents() {
        let base = tree("same");
        let mut index = index_of(&base);
        let stats = catch_up(&mut index, &ScanOptions::default());
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(stats.dirs_rescanned, 0, "{stats:?}");
        assert_eq!(stats.dirs_checked, 6);
        assert_eq!((stats.added, stats.removed), (0, 0));
    }

    #[test]
    fn missing_root_is_removed() {
        let base = tree("root");
        let mut index = index_of(&base);
        fs::remove_dir_all(&base).unwrap();
        catch_up(&mut index, &ScanOptions::default());
        assert!(index.is_empty());
    }
}
