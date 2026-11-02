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
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::index::Index;
use crate::listing::Tree;
use crate::progress::{Cancelled, Progress};
use crate::scan::{ScanOptions, Skip};
use crate::store::{Meta, NodeId};

/// What a catch-up tells its caller about the folders it reads. A closure
/// `FnMut(&Index, NodeId)` is one that only hears of added folders.
pub(crate) trait Folders {
    /// `dir` was added; called before its contents are read.
    fn added(&mut self, index: &Index, dir: NodeId);
    /// `dir` was read but not completely: its listing failed, or some of
    /// its entries could not be looked at. Its time says it is to be read
    /// again.
    fn incomplete(&mut self, _dir: NodeId) {}
}

impl<F: FnMut(&Index, NodeId)> Folders for F {
    fn added(&mut self, index: &Index, dir: NodeId) {
        self(index, dir)
    }
}

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

/// Catch-up of the folder roots in `roots` that tells `folders` of every
/// folder it adds, before reading that folder, and of every folder it could
/// not read completely (see [`Folders`]), and counts every indexed
/// folder it checks in `progress`. Removed ids are left in the index's
/// removal log. Entries that the rules leave out are not added; indexed
/// folders they leave out are removed when checked, and indexed files when
/// their folder is read again.
pub(crate) fn catch_up_with(
    index: &mut Index,
    roots: &[NodeId],
    skip: &Skip,
    folders: &mut dyn Folders,
    progress: &Progress,
) -> CatchUpStats {
    let mut pending = Pending::new(index, roots);
    pending.run(index, skip, folders, progress, None);
    pending.stats
}

/// A catch-up as [`catch_up_with`] does it, done in parts: the folders
/// still to check. Between parts the index may be read, but not changed by
/// anyone else, since the folders left are ids of it.
pub(crate) struct Pending {
    queue: VecDeque<NodeId>,
    pub(crate) stats: CatchUpStats,
}

impl Pending {
    /// A catch-up of the folder roots in `roots`.
    pub(crate) fn new(index: &Index, roots: &[NodeId]) -> Self {
        let store = index.store();
        let queue = roots
            .iter()
            .copied()
            .filter(|&id| store.is_live(id) && store.parent(id).is_none() && store.is_dir(id))
            .collect();
        Self {
            queue,
            stats: CatchUpStats::default(),
        }
    }

    /// Checks folders until none is left (then returns true) or, with a
    /// `deadline`, until it passed; at least one folder is checked.
    pub(crate) fn run(
        &mut self,
        index: &mut Index,
        skip: &Skip,
        folders: &mut dyn Folders,
        progress: &Progress,
        deadline: Option<Instant>,
    ) -> bool {
        while let Some(dir) = self.queue.pop_front() {
            let mode = Mode::default();
            self.queue.extend(check_dir(
                index,
                dir,
                skip,
                mode,
                folders,
                progress,
                &mut self.stats,
            ));
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
        }
        self.queue.is_empty()
    }
}

/// Finishes a scan that was interrupted: `index` holds what a cancelled
/// [`crate::scan::scan_with_progress`] of `roots` read (or what an earlier
/// call of this left), possibly saved and loaded since. Folders it had not
/// read completely are read; folders it had read are checked as a catch-up
/// does, so changes made in between are found too. Unlike a catch-up, new
/// folders are read one at a time, so a cancel is seen after each folder.
/// `progress` counts the entries added. When it is cancelled, `index` again
/// holds a state this can go on from. When it ends, `index` is compacted
/// (entries removed meanwhile leave no empty slots), so it can be grafted
/// into another index like a fresh scan.
pub fn resume_scan(
    index: &mut Index,
    roots: &[PathBuf],
    options: &ScanOptions,
    progress: &Progress,
) -> Result<CatchUpStats, Cancelled> {
    let skip = Skip::new(options);
    let mut stats = CatchUpStats::default();
    for root in roots {
        let name = root.as_os_str().as_bytes();
        if index.find_child(None, name).is_some() {
            continue;
        }
        // Not reached before the cancel.
        if let Ok(m) = fs::symlink_metadata(root) {
            let meta = Meta {
                mtime_ns: 0,
                ..skip.meta_of(&m)
            };
            index.insert(None, name, m.is_dir(), meta);
            stats.added += 1;
            progress.add(1);
        }
    }
    let store = index.store();
    let mut queue: VecDeque<NodeId> = index
        .roots()
        .into_iter()
        .filter(|&id| store.is_dir(id))
        .collect();
    let mode = Mode {
        force: false,
        shallow: true,
    };
    while let Some(dir) = queue.pop_front() {
        progress.check()?;
        let added = stats.added;
        let subfolders = check_dir(
            index,
            dir,
            &skip,
            mode,
            &mut |_: &Index, _| {},
            &Progress::new(),
            &mut stats,
        );
        progress.add((stats.added - added) as u64);
        queue.extend(subfolders);
    }
    index.take_removed();
    if index.needs_compaction() {
        index.compact_in_place();
    }
    Ok(stats)
}

/// How [`check_dir`] reads a folder.
#[derive(Clone, Copy, Default)]
struct Mode {
    /// Read it even when its time is the same.
    force: bool,
    /// Add new subfolders without reading them, and return them to be
    /// checked, instead of adding their whole subtree at once.
    shallow: bool,
}

/// Like [`catch_up_with`] for the folders `dirs` only, without going into
/// their subfolders: each is read again when its time changed, or always
/// with `force`. For folders that were not watched (see
/// [`crate::watch::Watcher::recheck_unwatched`]) or were unmounted.
pub(crate) fn recheck_dirs(
    index: &mut Index,
    dirs: &[NodeId],
    skip: &Skip,
    force: bool,
    folders: &mut dyn Folders,
) -> CatchUpStats {
    let mut stats = CatchUpStats::default();
    for &dir in dirs {
        if index.store().is_live(dir) && index.store().is_dir(dir) {
            let progress = Progress::new();
            let mode = Mode {
                force,
                shallow: false,
            };
            check_dir(index, dir, skip, mode, folders, &progress, &mut stats);
        }
    }
    stats
}

/// Checks the indexed folder `dir` as described in [`catch_up_with`], read
/// as `mode` says, and returns its subfolders that still have to be checked.
fn check_dir(
    index: &mut Index,
    dir: NodeId,
    skip: &Skip,
    mode: Mode,
    folders: &mut dyn Folders,
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
    if !mode.force && meta.mtime_ns == index.store().meta(dir).mtime_ns {
        return kids
            .into_iter()
            .filter(|&k| index.store().is_dir(k))
            .collect();
    }

    // Read from the root of the tree, and only if it is still the folder
    // just looked at, not a link that replaced it or a folder above it.
    let Some(mut entries) = Tree::open(&root_path(index, dir))
        .and_then(|t| t.list(&path))
        .ok()
        .filter(|l| l.identity().ok() == Some((m.dev(), m.ino())))
    else {
        stats.errors += 1;
        return Vec::new();
    };
    // Name -> (is folder, metadata if `lstat` worked). A listing that fails part
    // way leaves the folder as it is, so unread entries are not taken as gone.
    let mut on_disk: HashMap<Vec<u8>, (bool, Option<Meta>)> = HashMap::new();
    let mut listing_failed = false;
    // Some entries were listed but could not be looked at.
    let mut incomplete = false;
    while let Some(entry) = entries.read() {
        let Ok(entry) = entry else {
            listing_failed = true;
            break;
        };
        let name = entry.name().to_vec();
        let Ok(is_dir) = entries.is_dir(&entry) else {
            listing_failed = true;
            break;
        };
        if !skip.rules.accepts(&path, &name, is_dir) {
            continue;
        }
        // No permission to look at the entry (folder readable but not
        // searchable), or it vanished: the listing still gives its name
        // and type, so additions and removals are applied, and the
        // folder is read again next time to fill in what is missing.
        let meta = skip.entry_meta(&entries, &entry, is_dir).ok();
        incomplete |= meta.is_none();
        on_disk.insert(name, (is_dir, meta));
    }
    if listing_failed {
        stats.errors += 1;
        folders.incomplete(dir);
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
                    on_disk.insert(name, listed);
                }
            }
        }
    }
    for (name, listed) in on_disk {
        match listed {
            (true, Some(meta)) if mode.shallow => {
                let child = path.join(OsStr::from_bytes(&name));
                if skip.accepts(&child, true) {
                    // Read when it is checked; until then its time is one
                    // no folder has.
                    let id = index.insert(
                        Some(dir),
                        &name,
                        true,
                        Meta {
                            mtime_ns: 0,
                            ..meta
                        },
                    );
                    folders.added(index, id);
                    subfolders.push(id);
                    stats.added += 1;
                }
            }
            listed => {
                stats.added += add_listed(index, dir, &name, &path, listed, skip, folders);
            }
        }
    }
    let mtime_ns = if incomplete {
        // The saved time differs from the folder's, so it is read again.
        folders.incomplete(dir);
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
    folders: &mut dyn Folders,
) -> usize {
    let path = dir_path.join(OsStr::from_bytes(name));
    if meta.is_some() {
        return add_tree(index, dir, name, &path, skip, folders);
    }
    if !skip.accepts(&path, is_dir) {
        return 0;
    }
    let id = index.insert(Some(dir), name, is_dir, Meta::default());
    if is_dir {
        folders.added(index, id);
    }
    1
}

/// Path of the root of the tree that `node` is in.
fn root_path(index: &Index, node: NodeId) -> PathBuf {
    let store = index.store();
    let mut top = node;
    while let Some(p) = store.parent(top) {
        top = p;
    }
    PathBuf::from(OsStr::from_bytes(store.name(top)))
}

/// Adds the entry `name` at `path` under `parent`, and for a folder all entries
/// below it, leaving out what the rules leave out. `folders` hears of each
/// added folder before its contents are read, and of each one not read
/// completely. Returns how many nodes were added.
pub(crate) fn add_tree(
    index: &mut Index,
    parent: NodeId,
    name: &[u8],
    path: &Path,
    skip: &Skip,
    folders: &mut dyn Folders,
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
    // Folders are read from the root of the tree, so none of them can be
    // replaced by a link that is followed (see [`crate::listing`]).
    let tree = Tree::open(&root_path(index, parent)).ok();
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
        folders.added(index, dir_id);
        let Some(mut entries) = tree.as_ref().and_then(|t| t.list(&dir_path).ok()) else {
            folders.incomplete(dir_id);
            continue;
        };
        let mut complete = true;
        while let Some(entry) = entries.read() {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let Ok(is_dir) = entries.is_dir(&entry) else {
                complete = false;
                continue;
            };
            if !skip.rules.accepts(&dir_path, entry.name(), is_dir) {
                continue;
            }
            let Ok(meta) = skip.entry_meta(&entries, &entry, is_dir) else {
                complete = false;
                continue;
            };
            let child = index.insert(Some(dir_id), entry.name(), is_dir, meta);
            added += 1;
            if is_dir {
                pending.push((child, dir_path.join(entry.name_os())));
            }
        }
        if complete {
            index.set_meta(dir_id, complete_meta);
        } else {
            folders.incomplete(dir_id);
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

    /// A folder next to `base` with `deep/secret.txt` in it.
    fn outside(base: &Path) -> PathBuf {
        let outside = base.with_extension("outside");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(outside.join("deep")).unwrap();
        fs::write(outside.join("deep/secret.txt"), b"").unwrap();
        outside
    }

    #[test]
    fn catch_up_does_not_follow_a_folder_replaced_by_a_link() {
        let base = tree("swap-catch-up");
        let outside = outside(&base);
        let mut index = index_of(&base);
        fs::write(base.join("keep/deep/new.txt"), b"").unwrap();
        // `keep` is checked and unchanged; just before its changed subfolder
        // `deep` is read, `keep` becomes a link to the outside.
        let (keep, deep, target) = (base.join("keep"), base.join("keep/deep"), outside.clone());
        let moved = base.with_extension("moved");
        let hook_moved = moved.clone();
        let _hook = crate::listing::hook::set(move |dir| {
            if dir == deep {
                fs::rename(&keep, &hook_moved).unwrap();
                std::os::unix::fs::symlink(&target, &keep).unwrap();
            }
        });
        let stats = catch_up(&mut index, &ScanOptions::default());
        drop(_hook);
        let found = paths(&index);
        fs::remove_dir_all(&base).unwrap();
        fs::remove_dir_all(&outside).unwrap();
        fs::remove_dir_all(&moved).unwrap();

        assert!(
            !found.contains(&base.join("keep/deep/secret.txt")),
            "{found:?}"
        );
        assert!(!found.contains(&base.join("keep/deep/new.txt")));
        assert!(found.contains(&base.join("keep/deep/b.txt")));
        assert_eq!(stats.errors, 1);
    }

    #[test]
    fn add_tree_does_not_follow_a_folder_replaced_by_a_link() {
        let base = tree("swap-add-tree");
        let outside = outside(&base);
        let mut index = index_of(&base);
        let added = base.join("added");
        fs::create_dir_all(added.join("deep")).unwrap();
        let deep = added.join("deep");
        let root = index.roots()[0];
        add_tree(
            &mut index,
            root,
            b"added",
            &added,
            &Skip::new(&ScanOptions::default()),
            // Called for each new folder before it is read.
            &mut |idx: &Index, dir| {
                if idx.store().path(dir) == deep {
                    fs::remove_dir(&deep).unwrap();
                    std::os::unix::fs::symlink(outside.join("deep"), &deep).unwrap();
                }
            },
        );
        let found = paths(&index);
        fs::remove_dir_all(&base).unwrap();
        fs::remove_dir_all(&outside).unwrap();

        assert!(found.contains(&deep));
        assert!(!found.contains(&deep.join("secret.txt")), "{found:?}");
        assert_eq!(index.store().meta(find(&index, &deep)).mtime_ns, 0);
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
            &mut |_: &Index, _| {
                fs::set_permissions(&added, fs::Permissions::from_mode(0o000)).unwrap()
            },
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
    fn an_interrupted_scan_goes_on_where_it_stopped() {
        use crate::persist;
        use crate::scan::scan_with_progress;
        let base =
            std::env::temp_dir().join(format!("everygrep-sync-resume-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in 0..40 {
            let dir = base.join(format!("d{d}"));
            fs::create_dir_all(dir.join("sub")).unwrap();
            for f in 0..30 {
                fs::write(dir.join(format!("f{f}.txt")), b"x").unwrap();
            }
            fs::write(dir.join("sub/s.txt"), b"s").unwrap();
        }
        let roots = [base.clone()];
        let options = ScanOptions::default();
        let full = index_of(&base).len();

        let mut store = NodeStore::new();
        let progress = Progress::new();
        progress.cancel_when_done(300);
        assert!(scan_with_progress(&mut store, &roots, &options, &progress).is_err());
        let partial = Index::new(store);
        assert!(
            partial.len() > 1 && partial.len() < full,
            "{}",
            partial.len()
        );
        // Saved and loaded, as when the program ends in between.
        let file = base.with_extension("partial");
        persist::save(&partial, &roots, &file).unwrap();
        let mut index = persist::load(&file).unwrap().index;
        fs::remove_file(&file).unwrap();
        let before = index.len();
        // Changes made meanwhile are found as well.
        fs::write(base.join("d0/later.txt"), b"l").unwrap();
        fs::remove_file(base.join("d39/f0.txt")).unwrap();
        fs::create_dir(base.join("new")).unwrap();

        // Interrupted a second time, then finished.
        let progress = Progress::new();
        progress.cancel_when_done(200);
        assert!(resume_scan(&mut index, &roots, &options, &progress).is_err());
        assert!(index.len() > before);
        let progress = Progress::new();
        let stats = resume_scan(&mut index, &roots, &options, &progress).unwrap();
        let fresh = index_of(&base);
        let folders = fresh
            .order()
            .iter()
            .filter(|&id| fresh.store().is_dir(id))
            .count();
        let found = (paths(&index), folder_sizes(&index));
        let again = catch_up(&mut index, &options);
        fs::remove_dir_all(&base).unwrap();

        assert_eq!(found.0, paths(&fresh));
        assert_eq!(found.1, folder_sizes(&fresh));
        // Only what was left was read: not every folder, not every entry.
        assert!(stats.dirs_rescanned < folders, "{stats:?}");
        assert!(stats.added < fresh.len() - before, "{stats:?}");
        assert_eq!(progress.done(), stats.added as u64);
        // The finished index is complete: a catch-up reads nothing.
        assert_eq!(again.dirs_rescanned, 0, "{again:?}");
    }

    #[test]
    fn a_finished_resumed_scan_can_be_added_to_another_index() {
        use crate::scan::scan_with_progress;
        let base = std::env::temp_dir().join(format!(
            "everygrep-sync-resume-graft-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        for d in 0..10 {
            let dir = base.join(format!("d{d}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..10 {
                fs::write(dir.join(format!("f{f}.txt")), b"x").unwrap();
            }
        }
        let roots = [base.clone()];
        let options = ScanOptions::default();
        let mut store = NodeStore::new();
        let progress = Progress::new();
        progress.cancel_when_done(40);
        assert!(scan_with_progress(&mut store, &roots, &options, &progress).is_err());
        let mut index = Index::new(store);
        // A file that was read before the cancel is deleted meanwhile.
        let read = index
            .order()
            .iter()
            .find(|&id| !index.store().is_dir(id))
            .map(|id| index.store().path(id))
            .expect("a file was read");
        fs::remove_file(&read).unwrap();

        resume_scan(&mut index, &roots, &options, &Progress::new()).unwrap();
        let mut window = Index::new(NodeStore::new());
        let added = window.graft(&index);
        let found = paths(&window);
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(added.len(), 1);
        assert!(!found.contains(&read));
        assert_eq!(found.len(), 1 + 10 + 10 * 10 - 1);
    }

    #[test]
    fn a_folder_found_just_before_a_cancel_is_read_when_the_scan_goes_on() {
        use crate::scan::scan_with_progress;
        let base = std::env::temp_dir().join(format!(
            "everygrep-sync-resume-batch-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        // More folders than one progress batch, each with a file in it.
        for d in 0..1100 {
            let dir = base.join(format!("d{d:04}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("f.txt"), b"x").unwrap();
        }
        let roots = [base.clone()];
        let options = ScanOptions::default();
        let mut store = NodeStore::new();
        let progress = Progress::new();
        // The root, then the check after the first batch of its entries.
        progress.cancel_when_done(1 + 1024);
        assert!(scan_with_progress(&mut store, &roots, &options, &progress).is_err());
        let mut index = Index::new(store);
        assert_eq!(index.len(), 1 + 1024);
        resume_scan(&mut index, &roots, &options, &Progress::new()).unwrap();
        let fresh = index_of(&base);
        let (found, again) = (paths(&index), catch_up(&mut index, &options));
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(found.len(), 1 + 2 * 1100);
        assert_eq!(found, paths(&fresh));
        assert_eq!(again.dirs_rescanned, 0, "{again:?}");
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
