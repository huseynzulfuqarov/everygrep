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
        queue.extend(check_dir(
            index, dir, skip, on_new_dir, progress, &mut stats,
        ));
    }
    stats
}

/// Like [`catch_up_with`] for the folders `dirs` only, without going into
/// their subfolders: each is read again when its time changed. For folders
/// that were not watched (see [`crate::watch::Watcher::recheck_unwatched`]).
pub(crate) fn recheck_dirs(
    index: &mut Index,
    dirs: &[NodeId],
    skip: &Skip,
    on_new_dir: &mut dyn FnMut(&Index, NodeId),
) -> CatchUpStats {
    let mut stats = CatchUpStats::default();
    for &dir in dirs {
        if index.store().is_live(dir) && index.store().is_dir(dir) {
            check_dir(index, dir, skip, on_new_dir, &Progress::new(), &mut stats);
        }
    }
    stats
}

/// Checks the indexed folder `dir` as described in [`catch_up_with`] and
/// returns its subfolders that still have to be checked.
fn check_dir(
    index: &mut Index,
    dir: NodeId,
    skip: &Skip,
    on_new_dir: &mut dyn FnMut(&Index, NodeId),
    progress: &Progress,
    stats: &mut CatchUpStats,
) -> Vec<NodeId> {
    if !index.store().is_live(dir) {
        return Vec::new();
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
            return Vec::new();
        }
        _ => {
            stats.removed += index.remove(dir).len();
            return Vec::new();
        }
    };
    let meta = Meta::from_metadata(&m);
    if index.store().parent(dir).is_some() && !skip.accepts(&path, true) {
        stats.removed += index.remove(dir).len();
        return Vec::new();
    }
    let kids: Vec<NodeId> = index.store().children(dir).collect();
    if !skip.enters(&path) {
        index.set_meta(dir, meta);
        return Vec::new();
    }
    if meta.mtime_ns == index.store().meta(dir).mtime_ns {
        return kids
            .into_iter()
            .filter(|&k| index.store().is_dir(k))
            .collect();
    }

    let Ok(entries) = fs::read_dir(&path) else {
        stats.errors += 1;
        return Vec::new();
    };
    // Name -> (is folder, metadata if `lstat` worked). A listing that fails part
    // way leaves the folder as it is, so unread entries are not taken as gone.
    let mut on_disk: HashMap<Vec<u8>, (bool, Option<Meta>)> = HashMap::new();
    let mut listing_failed = false;
    // Some entries were listed but could not be looked at.
    let mut incomplete = false;
    for entry in entries {
        let Ok(entry) = entry else {
            listing_failed = true;
            break;
        };
        let name = entry.file_name().as_bytes().to_vec();
        let Ok(ft) = entry.file_type() else {
            listing_failed = true;
            break;
        };
        if !skip.rules.accepts(&path, &name, ft.is_dir()) {
            continue;
        }
        // No permission to look at the entry (folder readable but not
        // searchable), or it vanished: the listing still gives its name
        // and type, so additions and removals are applied, and the
        // folder is read again next time to fill in what is missing.
        let meta = skip.entry_meta(&entry, ft.is_dir()).ok();
        incomplete |= meta.is_none();
        on_disk.insert(name, (ft.is_dir(), meta));
    }
    if listing_failed {
        stats.errors += 1;
        return Vec::new();
    }
    stats.dirs_rescanned += 1;
    stats.errors += usize::from(incomplete);
    let mut subfolders = Vec::new();
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
                    subfolders.push(kid);
                } else if let Some(m) = meta.filter(|&m| m != index.store().meta(kid)) {
                    index.set_meta(kid, m);
                    stats.updated += 1;
                }
            }
            still_there => {
                stats.removed += index.remove(kid).len();
                if let Some(listed) = still_there {
                    stats.added += add_listed(index, dir, &name, &path, listed, skip, on_new_dir);
                }
            }
        }
    }
    for (name, listed) in on_disk {
        stats.added += add_listed(index, dir, &name, &path, listed, skip, on_new_dir);
    }
    let mtime_ns = if incomplete {
        // The saved time differs from the folder's, so it is read again.
        index.store().meta(dir).mtime_ns
    } else {
        meta.mtime_ns
    };
    index.set_meta(dir, Meta { mtime_ns, ..meta });
    subfolders
}

/// Adds the listed entry `name` of the folder `dir` at `dir_path`: with
/// [`add_tree`] when it could be looked at, otherwise by name and type alone,
/// with unknown size and time (a folder is then read when it is visited).
fn add_listed(
    index: &mut Index,
    dir: NodeId,
    name: &[u8],
    dir_path: &Path,
    (is_dir, meta): (bool, Option<Meta>),
    skip: &Skip,
    on_new_dir: &mut dyn FnMut(&Index, NodeId),
) -> usize {
    let path = dir_path.join(OsStr::from_bytes(name));
    if meta.is_some() {
        return add_tree(index, dir, name, &path, skip, on_new_dir);
    }
    if !skip.accepts(&path, is_dir) {
        return 0;
    }
    let id = index.insert(Some(dir), name, is_dir, Meta::default());
    if is_dir {
        on_new_dir(index, id);
    }
    1
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
    let id = index.insert(Some(parent), name, m.is_dir(), skip.meta_of(&m));
    if !m.is_dir() {
        return 1;
    }
    let mut added = 1;
    let mut pending = vec![(id, path.to_path_buf())];
    while let Some((dir_id, dir_path)) = pending.pop() {
        if !skip.enters(&dir_path) {
            continue;
        }
        let complete_meta = index.store().meta(dir_id);
        index.set_meta(
            dir_id,
            Meta {
                mtime_ns: 0,
                ..complete_meta
            },
        );
        on_new_dir(index, dir_id);
        let Ok(entries) = fs::read_dir(&dir_path) else {
            continue;
        };
        let mut complete = true;
        for entry in entries {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let Ok(ft) = entry.file_type() else {
                complete = false;
                continue;
            };
            if !skip
                .rules
                .accepts(&dir_path, entry.file_name().as_bytes(), ft.is_dir())
            {
                continue;
            }
            let Ok(meta) = skip.entry_meta(&entry, ft.is_dir()) else {
                complete = false;
                continue;
            };
            let child = index.insert(
                Some(dir_id),
                entry.file_name().as_bytes(),
                ft.is_dir(),
                meta,
            );
            added += 1;
            if ft.is_dir() {
                pending.push((child, entry.path()));
            }
        }
        if complete {
            index.set_meta(dir_id, complete_meta);
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

    fn find(index: &Index, path: &Path) -> NodeId {
        index
            .order()
            .iter()
            .find(|&id| index.store().path(id) == path)
            .expect("indexed")
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
    fn catch_up_without_file_metadata_keeps_directory_times_and_zero_file_metadata() {
        let base = tree("no-meta");
        let options = ScanOptions {
            with_metadata: false,
            ..ScanOptions::default()
        };
        let mut store = NodeStore::new();
        scan(&mut store, std::slice::from_ref(&base), &options);
        let mut index = Index::new(store);
        assert_eq!(catch_up(&mut index, &options).dirs_rescanned, 0);
        fs::write(base.join("keep/new.txt"), b"new content").unwrap();
        fs::create_dir_all(base.join("new/deep")).unwrap();
        fs::write(base.join("new/deep/file.txt"), b"new nested content").unwrap();
        let stats = catch_up(&mut index, &options);
        assert!(stats.added >= 4);
        assert!(paths(&index).contains(&base.join("new/deep/file.txt")));
        for id in index.order().iter() {
            let meta = index.store().meta(id);
            if index.store().is_dir(id) {
                assert_ne!(meta.mtime_ns, 0);
            } else {
                assert_eq!(meta, Meta::default());
            }
        }
        assert_eq!(catch_up(&mut index, &options).dirs_rescanned, 0);
        fs::remove_dir_all(&base).unwrap();
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
    fn unsearchable_folder_follows_its_listing_and_is_read_again_later() {
        use std::os::unix::fs::PermissionsExt;
        let base = tree("listed-only");
        let locked = base.join("keep");
        let mut index = index_of(&base);
        let saved_time = index.store().meta(find(&index, &locked)).mtime_ns;
        fs::remove_file(locked.join("a.txt")).unwrap();
        fs::write(locked.join("added.txt"), b"added").unwrap();
        fs::create_dir(locked.join("new-dir")).unwrap();
        fs::write(locked.join("new-dir/inner.txt"), b"i").unwrap();
        // Readable but not searchable: listing works, lstat of entries fails.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(
            fs::symlink_metadata(locked.join("added.txt")).is_err(),
            "test requires an unprivileged user"
        );
        let stats = catch_up(&mut index, &ScanOptions::default());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let found = paths(&index);
        assert!(!found.contains(&locked.join("a.txt")), "{found:?}");
        assert!(found.contains(&locked.join("added.txt")), "{found:?}");
        assert!(found.contains(&locked.join("new-dir")), "{found:?}");
        assert!(found.contains(&locked.join("deep/b.txt")), "{found:?}");
        assert_eq!((stats.added, stats.removed), (2, 1), "{stats:?}");
        // Its saved time is kept, so the next catch-up reads it again.
        assert_eq!(
            index.store().meta(find(&index, &locked)).mtime_ns,
            saved_time
        );
        assert_eq!(
            index.store().meta(find(&index, &locked.join("added.txt"))),
            Meta::default()
        );

        // Searchable again: sizes, times and the new folder's contents.
        catch_up(&mut index, &ScanOptions::default());
        let fresh = index_of(&base);
        let sizes = (folder_sizes(&index), folder_sizes(&fresh));
        let added = index.store().meta(find(&index, &locked.join("added.txt")));
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(paths(&index), paths(&fresh));
        assert_eq!(sizes.0, sizes.1);
        assert_eq!(added.size, 5);
    }

    #[test]
    fn failed_initial_listing_is_retried_after_permissions_return() {
        use std::os::unix::fs::PermissionsExt;
        let base = tree("initial-listing-failure");
        let locked = base.join("keep");
        let before = Meta::from_metadata(&fs::metadata(&locked).unwrap()).mtime_ns;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            fs::read_dir(&locked).is_err(),
            "test requires an unprivileged user"
        );
        let mut index = index_of(&base);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            Meta::from_metadata(&fs::metadata(&locked).unwrap()).mtime_ns,
            before
        );
        assert!(!paths(&index).contains(&locked.join("a.txt")));
        catch_up(&mut index, &ScanOptions::default());
        assert_eq!(paths(&index), paths(&index_of(&base)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn failed_add_tree_listing_is_retried_after_permissions_return() {
        use std::os::unix::fs::PermissionsExt;
        let base = tree("added-listing-failure");
        let mut index = index_of(&base);
        let added = base.join("added");
        fs::create_dir(&added).unwrap();
        fs::write(added.join("secret.txt"), b"secret").unwrap();
        let before = Meta::from_metadata(&fs::metadata(&added).unwrap()).mtime_ns;
        let root = index.roots()[0];
        add_tree(
            &mut index,
            root,
            b"added",
            &added,
            &Skip::new(&ScanOptions::default()),
            &mut |_, _| fs::set_permissions(&added, fs::Permissions::from_mode(0o000)).unwrap(),
        );
        assert!(
            fs::read_dir(&added).is_err(),
            "test requires an unprivileged user"
        );
        fs::set_permissions(&added, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            Meta::from_metadata(&fs::metadata(&added).unwrap()).mtime_ns,
            before
        );
        assert!(!paths(&index).contains(&added.join("secret.txt")));
        catch_up(&mut index, &ScanOptions::default());
        assert_eq!(paths(&index), paths(&index_of(&base)));
        fs::remove_dir_all(&base).unwrap();
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
