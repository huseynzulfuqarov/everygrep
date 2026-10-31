//! Catching up with changes made while the index was not watched.
//!
//! Adding or removing an entry changes its folder's modification time. Catching
//! up therefore reads the time of every indexed folder and re-reads only folders
//! whose time differs from the saved one. Folders that vanished are removed with
//! their subtree; new folders are scanned completely.
//!
//! Changing a file's contents does not touch its folder, so sizes and times of
//! files in unchanged folders are not refreshed here; a full rescan does that.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::index::Index;
use crate::progress::Progress;
use crate::scan::{ScanOptions, Skip};
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
/// what is and is not detected. Offline roots (see [`Index::set_offline`])
/// are left as they are.
pub fn catch_up(index: &mut Index, options: &ScanOptions) -> CatchUpStats {
    let roots = online_roots(index);
    let stats = catch_up_with(
        index,
        &roots,
        &Skip::new(options),
        &mut |_: &Index, _| {},
        &Progress::new(),
    );
    // Nobody keeps per-node state here, so the removal log is not needed.
    index.take_removed();
    stats
}

/// Number of folders a catch-up of `roots` checks at most: all indexed
/// folders below them, the roots included.
pub fn folders_below(index: &Index, roots: &[NodeId]) -> u64 {
    let store = index.store();
    let mut count = 0;
    let mut pending: Vec<NodeId> = roots
        .iter()
        .copied()
        .filter(|&r| store.is_live(r) && store.is_dir(r))
        .collect();
    while let Some(dir) = pending.pop() {
        count += 1;
        pending.extend(store.children(dir).filter(|&k| store.is_dir(k)));
    }
    count
}

/// The roots that are not offline.
pub(crate) fn online_roots(index: &Index) -> Vec<NodeId> {
    index
        .roots()
        .into_iter()
        .filter(|r| !index.offline_roots().contains(r))
        .collect()
}

/// Catch-up of the folder roots in `roots` that calls `on_new_dir` for every
/// folder it adds, before reading that folder, and counts every indexed
/// folder it checks in `progress`. Removed ids are left in the index's
/// removal log. Entries that the rules leave out are not added; indexed
/// folders they leave out are removed when checked, and indexed files when
/// their folder is read again.
pub(crate) fn catch_up_with(
    index: &mut Index,
    roots: &[NodeId],
    skip: &Skip,
    on_new_dir: &mut dyn FnMut(&Index, NodeId),
    progress: &Progress,
) -> CatchUpStats {
    let mut stats = CatchUpStats::default();

    let store = index.store();
    let mut queue: VecDeque<NodeId> = roots
        .iter()
        .copied()
        .filter(|&id| store.is_live(id) && store.parent(id).is_none() && store.is_dir(id))
        .collect();

    while let Some(dir) = queue.pop_front() {
        if !index.store().is_live(dir) {
            continue;
        }
        stats.dirs_checked += 1;
        progress.add(1);
        let path = index.store().path(dir);
        let m = match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => m,
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                // Not allowed to look (for example under an unsearchable folder):
                // keep what is indexed.
                stats.errors += 1;
                continue;
            }
            _ => {
                stats.removed += index.remove(dir).len();
                continue;
            }
        };
        let meta = Meta::from_metadata(&m);
        if index.store().parent(dir).is_some() && !skip.accepts(&path, true) {
            stats.removed += index.remove(dir).len();
            continue;
        }
        let kids: Vec<NodeId> = index.store().children(dir).collect();
        if !skip.enters(&path) {
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
        // Name -> (is folder, metadata if `lstat` worked). A listing that fails part
        // way leaves the folder as it is, so unread entries are not taken as gone.
        let mut on_disk: HashMap<Vec<u8>, (bool, Option<Meta>)> = HashMap::new();
        let mut listing_failed = false;
        for entry in entries {
            let Ok(entry) = entry else {
                listing_failed = true;
                break;
            };
            let name = entry.file_name().as_bytes().to_vec();
            if let Ok(ft) = entry.file_type() {
                if !skip.rules.accepts(&path, &name, ft.is_dir()) {
                    continue;
                }
            }
            match entry.metadata() {
                Ok(em) => {
                    on_disk.insert(name, (em.is_dir(), Some(Meta::from_metadata(&em))));
                }
                // No permission to look at entries (folder readable but not
                // searchable), or the entry vanished: keep it with its old data.
                Err(_) => match entry.file_type() {
                    Ok(ft) => {
                        on_disk.insert(name, (ft.is_dir(), None));
                    }
                    Err(_) => listing_failed = true,
                },
            }
        }
        if listing_failed {
            stats.errors += 1;
            continue;
        }
        stats.dirs_rescanned += 1;
        for kid in kids {
            if !index.store().is_live(kid) {
                continue;
            }
            let name = index.store().name(kid).to_vec();
            match on_disk.remove(&name) {
                Some((is_dir, meta)) if is_dir == index.store().is_dir(kid) => {
                    if is_dir {
                        // A folder keeps its saved time until it is visited itself;
                        // updating it here would hide its own changes.
                        queue.push_back(kid);
                    } else if let Some(m) = meta.filter(|&m| m != index.store().meta(kid)) {
                        index.set_meta(kid, m);
                        stats.updated += 1;
                    }
                }
                still_there => {
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
/// below it, leaving out what the rules leave out. `on_new_dir` is called
/// for each added folder before its contents are read. Returns how many
/// nodes were added.
pub(crate) fn add_tree(
    index: &mut Index,
    parent: NodeId,
    name: &[u8],
    path: &Path,
    skip: &Skip,
    on_new_dir: &mut dyn FnMut(&Index, NodeId),
) -> usize {
    let Ok(m) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !skip.accepts(path, m.is_dir()) {
        return 0;
    }
    let id = index.insert(Some(parent), name, m.is_dir(), Meta::from_metadata(&m));
    if !m.is_dir() {
        return 1;
    }
    let mut added = 1;
    let mut pending = vec![(id, path.to_path_buf())];
    while let Some((dir_id, dir_path)) = pending.pop() {
        if !skip.enters(&dir_path) {
            continue;
        }
        on_new_dir(index, dir_id);
        let Ok(entries) = fs::read_dir(&dir_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(em) = entry.metadata() else { continue };
            if !skip
                .rules
                .accepts(&dir_path, entry.file_name().as_bytes(), em.is_dir())
            {
                continue;
            }
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
    use crate::rules::{IndexRules, RuleSpec};
    use crate::scan::scan;
    use crate::store::NodeStore;
    use std::path::PathBuf;

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

    #[test]
    fn catch_up_matches_a_fresh_scan() {
        let base = tree("match");
        let mut index = index_of(&base);

        fs::write(base.join("keep/new.txt"), b"n").unwrap();
        fs::write(base.join("keep/deep/b.txt"), b"bigger").unwrap();
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
        assert_eq!(folder_sizes(&index), folder_sizes(&fresh));
        assert!(stats.removed >= 4, "{stats:?}");
        assert!(stats.added >= 6, "{stats:?}");
    }

    #[test]
    fn catch_up_follows_the_rules() {
        let base = tree("rules");
        let mut index = index_of(&base);
        fs::write(base.join("keep/new.tmp"), b"t").unwrap();
        fs::write(base.join("keep/new.txt"), b"t").unwrap();
        fs::create_dir_all(base.join("keep/cache/inner")).unwrap();
        let options = ScanOptions {
            rules: IndexRules::new(RuleSpec {
                folders: vec!["cache".into(), base.join("gone")],
                exclude_files: "*.tmp".into(),
                ..RuleSpec::default()
            })
            .unwrap(),
            ..ScanOptions::default()
        };
        catch_up(&mut index, &options);
        let found = paths(&index);
        let mut fresh = NodeStore::new();
        scan(&mut fresh, std::slice::from_ref(&base), &options);
        let fresh = paths(&Index::new(fresh));
        fs::remove_dir_all(&base).unwrap();
        assert!(found.contains(&base.join("keep/new.txt")));
        assert!(!found.contains(&base.join("keep/new.tmp")));
        assert!(!found.contains(&base.join("keep/cache")));
        assert!(!found.contains(&base.join("gone")), "indexed, now excluded");
        assert_eq!(found, fresh);
    }

    #[test]
    fn catch_up_progress_counts_the_folders_checked() {
        let root = tree("progress");
        let mut index = index_of(&root);
        let roots = online_roots(&index);
        let folders = folders_below(&index, &roots);
        let progress = Progress::new();
        let stats = catch_up_with(
            &mut index,
            &roots,
            &Skip::new(&ScanOptions::default()),
            &mut |_: &Index, _| {},
            &progress,
        );
        fs::remove_dir_all(&root).unwrap();
        assert!(folders > 1);
        assert_eq!(progress.done(), folders);
        assert_eq!(stats.dirs_checked as u64, folders);
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
    fn unsearchable_folder_keeps_its_entries() {
        use std::os::unix::fs::PermissionsExt;
        let base = tree("locked");
        let locked = base.join("keep");
        let mut index = index_of(&base);
        fs::write(locked.join("added.txt"), b"x").unwrap();
        // Readable but not searchable: listing works, lstat of entries fails.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o444)).unwrap();
        catch_up(&mut index, &ScanOptions::default());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let found = paths(&index);
        fs::remove_dir_all(&base).unwrap();
        assert!(found.contains(&locked.join("a.txt")), "{found:?}");
        assert!(found.contains(&locked.join("deep/b.txt")), "{found:?}");
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
