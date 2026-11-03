//! Building a [`NodeStore`] by walking directory trees.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::listing::{Entry, Listing, Tree};
use crate::progress::{Cancelled, Progress};
use crate::rules::IndexRules;
use crate::store::{Meta, NodeId, NodeStore};

/// File system types that hold no real files and are skipped by default.
pub const VIRTUAL_FS_TYPES: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "proc",
    "pstore",
    "securityfs",
    "sysfs",
    "tracefs",
];

/// Settings for a scan.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Read file sizes and modification times (one `lstat` per file).
    /// Directory modification times are always kept for catch-up.
    pub with_metadata: bool,
    /// Folders and files left out of the index (see [`crate::rules`]).
    pub rules: IndexRules,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            with_metadata: true,
            rules: IndexRules::default(),
        }
    }
}

/// Counters collected during a scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Folders indexed.
    pub dirs: u64,
    /// Files and other non-folder entries indexed.
    pub files: u64,
    /// Entries or folders that could not be read (for example permission denied).
    pub errors: u64,
}

/// Walks every root and adds all entries below it to `store`.
///
/// Symbolic links are indexed but never followed. Mount points of virtual file
/// systems ([`VIRTUAL_FS_TYPES`]) are indexed but not entered. Entries that
/// `options.rules` leave out are not indexed, and an excluded folder is not
/// entered.
pub fn scan(store: &mut NodeStore, roots: &[PathBuf], options: &ScanOptions) -> ScanStats {
    scan_with_progress(store, roots, options, &Progress::new()).unwrap_or_default()
}

/// Entries added between two updates of the progress.
const PROGRESS_BATCH: u64 = 1024;

/// Like [`scan`], counting every entry added to `store` in `progress` and
/// stopping when it is cancelled. The cancel is seen after each folder and
/// after every [`PROGRESS_BATCH`] entries of a large folder; `store` then
/// holds part of the tree, with every folder that was not read completely
/// marked, so [`crate::sync::resume_scan`] can finish it later.
pub fn scan_with_progress(
    store: &mut NodeStore,
    roots: &[PathBuf],
    options: &ScanOptions,
    progress: &Progress,
) -> Result<ScanStats, Cancelled> {
    scan_reusing(store, roots, options, progress, None)
}

/// The index a replacement index may take listings from (see
/// [`update_with_progress`]).
pub struct Source<'a> {
    pub index: &'a Index,
    /// The roots (normalized) and options `index` was built with.
    pub roots: &'a [PathBuf],
    pub options: &'a ScanOptions,
    /// Folders whose changes were not observed (their watch failed): their
    /// listings are read again instead of copied.
    pub unobserved: HashSet<PathBuf>,
    /// File sizes and times may be older than the disk (the index was loaded
    /// from its file and caught up by folder times only, or changes were not
    /// watched for a while): with file details, they are read again for every
    /// copied file.
    pub stale_file_details: bool,
}

impl<'a> Source<'a> {
    /// A source whose folders and file details were all kept current.
    pub fn current(index: &'a Index, roots: &'a [PathBuf], options: &'a ScanOptions) -> Self {
        Self {
            index,
            roots,
            options,
            unobserved: HashSet::new(),
            stale_file_details: false,
        }
    }
}

/// Builds a replacement index, reading only new coverage when the new rules
/// are a restriction of the old ones. `roots` and the source roots must
/// already be normalized. Only these roots are reused; unrelated drive trees
/// stay out. Returns whether reuse was safe (false means a full scan was
/// performed). The source is unchanged, including when cancelled.
pub fn update_with_progress(
    store: &mut NodeStore,
    roots: &[PathBuf],
    options: &ScanOptions,
    source: Source<'_>,
    progress: &Progress,
) -> Result<bool, Cancelled> {
    let Source {
        index,
        roots: previous_roots,
        options: previous_options,
        unobserved,
        stale_file_details,
    } = source;
    let reopen = options.rules.incremental_parents(&previous_options.rules);
    if options.with_metadata != previous_options.with_metadata || reopen.is_none() {
        scan_with_progress(store, roots, options, progress)?;
        return Ok(false);
    }
    let mut reusable = HashMap::new();
    for path in previous_roots {
        if let Some(id) = index.find_child(None, path.as_os_str().as_bytes()) {
            reusable.insert(path.clone(), id);
            // A formerly nested folder can now be an explicit root, including
            // one whose parent is excluded by the new rules.
            for root in roots {
                if let Ok(relative) = root.strip_prefix(path) {
                    let mut found = Some(id);
                    for part in relative.components() {
                        found = found.and_then(|parent| {
                            index.find_child(Some(parent), part.as_os_str().as_bytes())
                        });
                    }
                    if let Some(id) = found {
                        reusable.insert(root.clone(), id);
                    }
                }
            }
        }
    }
    let reuse = Reuse {
        index,
        roots: reusable,
        reopen: reopen.unwrap_or_default(),
        unobserved,
        refresh_files: stale_file_details && options.with_metadata,
    };
    scan_reusing(store, roots, options, progress, Some(&reuse))?;
    Ok(true)
}

struct Reuse<'a> {
    index: &'a Index,
    roots: HashMap<PathBuf, NodeId>,
    reopen: HashSet<PathBuf>,
    unobserved: HashSet<PathBuf>,
    /// Copied files get their size and time from the disk.
    refresh_files: bool,
}

impl Reuse<'_> {
    fn find(&self, path: &Path) -> Option<NodeId> {
        if self.reopen.contains(path) || self.unobserved.contains(path) {
            return None;
        }
        if let Some(&id) = self.roots.get(path) {
            return (self.index.store().is_dir(id) && self.index.store().meta(id).mtime_ns != 0)
                .then_some(id);
        }
        for (root, &id) in &self.roots {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let mut found = Some(id);
            for part in relative.components() {
                found = found.and_then(|parent| {
                    self.index
                        .find_child(Some(parent), part.as_os_str().as_bytes())
                });
            }
            if let Some(id) = found {
                return (self.index.store().is_dir(id)
                    && self.index.store().meta(id).mtime_ns != 0)
                    .then_some(id);
            }
        }
        None
    }
}

fn scan_reusing(
    store: &mut NodeStore,
    roots: &[PathBuf],
    options: &ScanOptions,
    progress: &Progress,
    reuse: Option<&Reuse<'_>>,
) -> Result<ScanStats, Cancelled> {
    let skip = Skip::new(options);
    let mut stats = ScanStats::default();
    for root in roots {
        progress.check()?;
        let meta = match fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let id = store.add(
            None,
            root.as_os_str().as_bytes(),
            meta.is_dir(),
            meta_of(&meta, options),
        );
        progress.add(1);
        if meta.is_dir() {
            stats.dirs += 1;
            walk(store, id, root, &skip, &mut stats, progress, reuse)?;
        } else {
            stats.files += 1;
        }
    }
    progress.check()?;
    Ok(stats)
}

fn walk(
    store: &mut NodeStore,
    root_id: NodeId,
    root: &Path,
    skip: &Skip,
    stats: &mut ScanStats,
    progress: &Progress,
    reuse: Option<&Reuse<'_>>,
) -> Result<(), Cancelled> {
    let mut pending = vec![(root_id, root.to_path_buf())];
    // Without the root no folder can be read: the root then fails like a
    // folder whose listing fails.
    let tree = Tree::open(root);
    let result = walk_pending(
        store,
        tree.as_ref().ok(),
        &mut pending,
        skip,
        stats,
        progress,
        reuse,
    );
    if result.is_err() {
        // Cancelled: the folders not read yet keep a time no folder has, so a
        // catch-up of what was read (see [`crate::sync::resume_scan`]) reads
        // them.
        for (dir_id, _) in pending {
            let meta = store.meta(dir_id);
            store.set_meta(
                dir_id,
                Meta {
                    mtime_ns: 0,
                    ..meta
                },
            );
        }
    }
    result
}

/// Reads the folders in `pending` and the folders found below them, all
/// below the root of `tree`. A folder is taken off `pending` only once it is
/// being read.
fn walk_pending(
    store: &mut NodeStore,
    tree: Option<&Tree>,
    pending: &mut Vec<(NodeId, PathBuf)>,
    skip: &Skip,
    stats: &mut ScanStats,
    progress: &Progress,
    reuse: Option<&Reuse<'_>>,
) -> Result<(), Cancelled> {
    let mut unreported = 0;
    while !pending.is_empty() {
        progress.add(std::mem::take(&mut unreported));
        progress.check()?;
        let Some((dir_id, dir)) = pending.pop() else {
            break;
        };
        if let Some((reuse, old)) = reuse.and_then(|r| r.find(&dir).map(|id| (r, id))) {
            // Keep the time associated with the copied listing. Installing a
            // new watcher catches up changes made while the old one was down.
            store.set_meta(dir_id, reuse.index.store().meta(old));
            let source = reuse.index.store();
            let mut complete = true;
            let mut looked_at = 0;
            for child in source.children(old) {
                let name = source.name(child);
                let is_dir = source.is_dir(child);
                // Rejected entries are not progress (the total is what the
                // new index holds), but even a filter dropping a huge listing
                // must check cancellation between small batches.
                looked_at += 1;
                if looked_at == PROGRESS_BATCH {
                    looked_at = 0;
                    progress.add(std::mem::take(&mut unreported));
                    progress.check()?;
                }
                if !skip.rules.accepts(&dir, name, is_dir) {
                    continue;
                }
                let meta = if reuse.refresh_files && !is_dir {
                    let path = dir.join(std::ffi::OsStr::from_bytes(name));
                    match fs::symlink_metadata(&path) {
                        Ok(m) if !m.is_dir() => skip.meta_of(&m),
                        // Gone or replaced by a folder: the folder is read
                        // again by the catch-up that follows, as after a
                        // partial listing.
                        _ => {
                            complete = false;
                            stats.errors += 1;
                            Meta::default()
                        }
                    }
                } else {
                    source.meta(child)
                };
                let id = store.add(Some(dir_id), name, is_dir, meta);
                unreported += 1;
                if is_dir {
                    stats.dirs += 1;
                    let path = dir.join(std::ffi::OsStr::from_bytes(name));
                    if skip.enters(&path) {
                        pending.push((id, path));
                    }
                } else {
                    stats.files += 1;
                }
            }
            if !complete {
                store.set_meta(
                    dir_id,
                    Meta {
                        mtime_ns: 0,
                        ..store.meta(dir_id)
                    },
                );
            }
            continue;
        }
        // A failed or partial listing must be retried even if chmod leaves
        // the directory's modification time unchanged.
        let mut complete_meta = store.meta(dir_id);
        if reuse.is_some() {
            // Its time may come from a copied, older listing of its parent;
            // read before the listing, as a full scan does.
            if let Some(m) = fs::symlink_metadata(&dir).ok().filter(|m| m.is_dir()) {
                complete_meta.mtime_ns = Meta::from_metadata(&m).mtime_ns;
            }
        }
        store.set_meta(
            dir_id,
            Meta {
                mtime_ns: 0,
                ..complete_meta
            },
        );
        let mut complete = true;
        let Some(mut entries) = tree.and_then(|t| t.list(&dir).ok()) else {
            stats.errors += 1;
            continue;
        };
        while let Some(entry) = entries.read() {
            let Ok(entry) = entry else {
                stats.errors += 1;
                complete = false;
                continue;
            };
            let Ok(is_dir) = entries.is_dir(&entry) else {
                complete = false;
                stats.errors += 1;
                continue;
            };
            let name = entry.name();
            if !skip.rules.accepts(&dir, name, is_dir) {
                continue;
            }
            let meta = if skip.with_metadata || is_dir {
                match entries.meta(&entry) {
                    Ok(m) => m,
                    Err(_) => {
                        complete = false;
                        stats.errors += 1;
                        Meta::default()
                    }
                }
            } else {
                Meta::default()
            };
            let id = store.add(Some(dir_id), name, is_dir, meta);
            if is_dir {
                stats.dirs += 1;
                let path = dir.join(entry.name_os());
                if skip.enters(&path) {
                    pending.push((id, path));
                }
            } else {
                stats.files += 1;
            }
            // Only once a new folder is in `pending`, which a cancel marks
            // as not read.
            unreported += 1;
            if unreported == PROGRESS_BATCH {
                progress.add(std::mem::take(&mut unreported));
                progress.check()?;
            }
        }
        if complete {
            store.set_meta(dir_id, complete_meta);
        }
    }
    progress.add(unreported);
    Ok(())
}

/// Turns root paths into absolute, symlink-free paths, drops duplicates and drops
/// roots that lie inside another root (they would be indexed twice). Paths that
/// cannot be resolved are made absolute and kept, so the scan can report them.
pub fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut resolved: Vec<PathBuf> = roots
        .iter()
        .map(|r| {
            fs::canonicalize(r)
                .or_else(|_| std::path::absolute(r))
                .unwrap_or_else(|_| r.clone())
        })
        .collect();
    resolved.sort();
    resolved.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for root in resolved {
        if !out.iter().any(|kept| root.starts_with(kept)) {
            out.push(root);
        }
    }
    out
}

/// What a walk leaves out: virtual file system mount points, which are
/// indexed but not entered, and the entries the rules leave out.
pub(crate) struct Skip {
    virtual_mounts: HashSet<PathBuf>,
    pub(crate) rules: IndexRules,
    pub(crate) with_metadata: bool,
}

impl Skip {
    pub(crate) fn new(options: &ScanOptions) -> Self {
        Self {
            virtual_mounts: virtual_mount_points(),
            rules: options.rules.clone(),
            with_metadata: options.with_metadata,
        }
    }

    /// Reads entry metadata only when needed for directory catch-up or
    /// requested for files. File type comes from the directory listing.
    pub(crate) fn entry_meta(
        &self,
        listing: &Listing,
        entry: &Entry,
        is_dir: bool,
    ) -> std::io::Result<Meta> {
        if self.with_metadata || is_dir {
            listing.meta(entry)
        } else {
            Ok(Meta::default())
        }
    }

    pub(crate) fn meta_of(&self, m: &fs::Metadata) -> Meta {
        if self.with_metadata || m.is_dir() {
            Meta::from_metadata(m)
        } else {
            Meta::default()
        }
    }

    /// Whether the indexed folder at `path` is read.
    pub(crate) fn enters(&self, path: &Path) -> bool {
        !self.virtual_mounts.contains(path)
    }

    /// Whether the entry at `path` is indexed (always for a root).
    pub(crate) fn accepts(&self, path: &Path, is_dir: bool) -> bool {
        self.rules.accepts_path(path, is_dir)
    }
}

fn meta_of(m: &fs::Metadata, options: &ScanOptions) -> Meta {
    if options.with_metadata || m.is_dir() {
        Meta::from_metadata(m)
    } else {
        Meta::default()
    }
}

/// Mount points of virtual file systems, read from `/proc/self/mountinfo`.
fn virtual_mount_points() -> HashSet<PathBuf> {
    let Ok(info) = fs::read_to_string("/proc/self/mountinfo") else {
        return HashSet::new();
    };
    info.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mount_point = left.split(' ').nth(4)?;
            let fs_type = right.split(' ').next()?;
            VIRTUAL_FS_TYPES
                .contains(&fs_type)
                .then(|| PathBuf::from(unescape_mount_path(mount_point)))
        })
        .collect()
}

/// Mount paths in mountinfo escape space, tab, newline and backslash as octal.
fn unescape_mount_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let v = (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tree(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("everygrep-scan-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b")).unwrap();
        fs::create_dir_all(base.join("skip/inner")).unwrap();
        fs::write(base.join("a/one.txt"), b"1").unwrap();
        fs::write(base.join("a/b/two.TXT"), b"22").unwrap();
        fs::write(base.join("skip/inner/hidden.txt"), b"").unwrap();
        std::os::unix::fs::symlink("/", base.join("link-to-root")).unwrap();
        base
    }

    fn rules(folders: &[PathBuf], include: &str, exclude: &str, enabled: bool) -> IndexRules {
        IndexRules::new(crate::rules::RuleSpec {
            enabled,
            folders: folders.to_vec(),
            include_files: include.into(),
            exclude_files: exclude.into(),
        })
        .unwrap()
    }

    fn scanned_paths(base: &Path, options: &ScanOptions) -> (Vec<PathBuf>, ScanStats) {
        let mut store = NodeStore::new();
        let stats = scan(&mut store, &[base.to_path_buf()], options);
        let mut paths: Vec<PathBuf> = (0..store.len())
            .map(|i| store.path(NodeId(i as u32)))
            .collect();
        paths.sort();
        (paths, stats)
    }

    fn snapshot(index: &Index) -> Vec<(PathBuf, bool, Meta, u32)> {
        let store = index.store();
        let mut result: Vec<_> = index
            .order()
            .iter()
            .map(|id| {
                (
                    store.path(id),
                    store.is_dir(id),
                    store.meta(id),
                    store.items(id),
                )
            })
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    fn find_path(index: &Index, path: &Path) -> Option<NodeId> {
        index.roots().iter().copied().find_map(|root| {
            let relative = path.strip_prefix(index.store().path(root)).ok()?;
            relative.components().try_fold(root, |parent, part| {
                index.find_child(Some(parent), part.as_os_str().as_bytes())
            })
        })
    }

    fn full(roots: &[PathBuf], options: &ScanOptions) -> Index {
        let mut store = NodeStore::new();
        scan(&mut store, roots, options);
        Index::new(store)
    }

    #[test]
    fn incremental_roots_match_full_rebuilds_and_only_read_new_coverage() {
        use std::sync::{Arc, Mutex};
        let base = temp_tree("incremental-roots");
        let alias = base.join("alias");
        std::os::unix::fs::symlink(base.join("a"), &alias).unwrap();
        let cases = [
            (
                vec![base.join("a")],
                vec![base.join("a"), base.join("skip")],
                vec![base.join("skip"), base.join("skip/inner")],
            ),
            // A new enclosing root reads its own listing and uncovered siblings.
            (
                vec![base.join("a/b"), base.join("skip")],
                vec![base.clone()],
                vec![base.clone(), base.join("a")],
            ),
            // Removing an enclosing root retains the requested nested tree.
            (
                vec![base.clone(), base.join("a")],
                vec![base.join("a/b")],
                vec![],
            ),
            // Duplicate, nested and symlink aliases have one normalized root.
            (
                vec![base.join("a")],
                vec![alias, base.join("a"), base.join("a/b")],
                vec![],
            ),
            (
                vec![base.join("a"), base.join("skip")],
                vec![base.join("a")],
                vec![],
            ),
            (vec![base.join("a")], vec![base.join("missing")], vec![]),
        ];
        for (old, new, mut expected_reads) in cases {
            let old = normalize_roots(&old);
            let new = normalize_roots(&new);
            let options = ScanOptions::default();
            let previous = full(&old, &options);
            let before = snapshot(&previous);
            let reads = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&reads);
            let hook = crate::listing::hook::set(move |path| {
                recorded.lock().unwrap().push(path.to_path_buf())
            });
            let mut store = NodeStore::new();
            let progress = Progress::new();
            assert!(
                update_with_progress(
                    &mut store,
                    &new,
                    &options,
                    Source::current(&previous, &old, &options),
                    &progress
                )
                .unwrap()
            );
            drop(hook);
            let mut actual_reads = reads.lock().unwrap().clone();
            actual_reads.sort();
            expected_reads.sort();
            assert_eq!(actual_reads, expected_reads, "{old:?} -> {new:?}");
            assert_eq!(
                snapshot(&Index::new(store)),
                snapshot(&full(&new, &options))
            );
            assert_eq!(snapshot(&previous), before, "source stays intact");
            if !new.ends_with(&[base.join("missing")]) {
                assert!(progress.done() > 0);
            }
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn incremental_filters_and_fallbacks_match_full_rebuilds() {
        let base = temp_tree("incremental-rules");
        fs::write(base.join("a/music.mp3"), b"music").unwrap();
        let all = IndexRules::default();
        let exact = rules(&[base.join("skip")], "", "", true);
        let pattern = rules(&["sk*".into()], "", "", true);
        let cases = [
            (all.clone(), exact.clone(), true),
            (exact.clone(), all.clone(), true), // Reopen only the exact folder.
            (all.clone(), pattern.clone(), true),
            (pattern.clone(), all.clone(), false),
            (all.clone(), rules(&[], "*.txt;*.mp3", "*.bak", true), true),
            (
                rules(&[], "*.txt;*.mp3", "", true),
                rules(&[], "*.txt", "two*", true),
                true,
            ),
            (rules(&[], "*.txt", "", true), all.clone(), false),
            (rules(&[], "", "*.txt", true), all.clone(), false),
            (
                exact.clone(),
                rules(&[base.join("skip")], "", "", false),
                true,
            ),
            (pattern, rules(&["sk*".into()], "", "", false), false),
            (exact, rules(&[base.join("a")], "*.txt", "", true), true),
            (
                all.clone(),
                rules(&["regex:^b$".into()], "regex:(?i)txt$", "two*", true),
                true,
            ),
        ];
        for (old_rules, new_rules, expected_reuse) in cases {
            // Include promotion of a formerly excluded folder to an explicit
            // root: roots bypass folder filters, descendants still obey them.
            for (old_roots, new_roots) in [
                (vec![base.clone()], vec![base.clone()]),
                (vec![base.clone()], vec![base.join("skip")]),
                (vec![base.join("skip")], vec![base.clone()]),
            ] {
                for with_metadata in [true, false] {
                    let old = ScanOptions {
                        with_metadata,
                        rules: old_rules.clone(),
                    };
                    let new = ScanOptions {
                        with_metadata,
                        rules: new_rules.clone(),
                    };
                    let previous = full(&old_roots, &old);
                    let mut store = NodeStore::new();
                    assert_eq!(
                        update_with_progress(
                            &mut store,
                            &new_roots,
                            &new,
                            Source::current(&previous, &old_roots, &old),
                            &Progress::new()
                        )
                        .unwrap(),
                        expected_reuse
                    );
                    assert_eq!(
                        snapshot(&Index::new(store)),
                        snapshot(&full(&new_roots, &new)),
                        "{old_rules:?} -> {new_rules:?}"
                    );
                }
            }
        }
        // File metadata cannot be recovered from a names-only index.
        let roots = [base.clone()];
        let old = ScanOptions {
            with_metadata: false,
            ..ScanOptions::default()
        };
        let previous = full(&roots, &old);
        let mut store = NodeStore::new();
        assert!(
            !update_with_progress(
                &mut store,
                &roots,
                &ScanOptions::default(),
                Source::current(&previous, &roots, &old),
                &Progress::new()
            )
            .unwrap()
        );
        assert_eq!(
            snapshot(&Index::new(store)),
            snapshot(&full(&roots, &ScanOptions::default()))
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_file_root_that_became_a_folder_is_scanned_instead_of_reused() {
        let base = temp_tree("incremental-root-type");
        let root = base.join("flip");
        fs::write(&root, b"file").unwrap();
        let roots = [root.clone()];
        let options = ScanOptions::default();
        let previous = full(&roots, &options);
        fs::remove_file(&root).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("child.txt"), b"child").unwrap();
        let mut store = NodeStore::new();
        update_with_progress(
            &mut store,
            &roots,
            &options,
            Source::current(&previous, &roots, &options),
            &Progress::new(),
        )
        .unwrap();
        assert_eq!(
            snapshot(&Index::new(store)),
            snapshot(&full(&roots, &options))
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn reopening_an_exact_exclusion_reads_only_its_parent_and_new_tree() {
        use std::sync::{Arc, Mutex};
        let base = temp_tree("incremental-reopen");
        let roots = [base.clone()];
        let old = ScanOptions {
            rules: rules(&[base.join("skip")], "", "", true),
            ..ScanOptions::default()
        };
        let previous = full(&roots, &old);
        let reads = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&reads);
        let hook = crate::listing::hook::set(move |path| {
            recorded.lock().unwrap().push(path.to_path_buf())
        });
        let mut store = NodeStore::new();
        update_with_progress(
            &mut store,
            &roots,
            &ScanOptions::default(),
            Source::current(&previous, &roots, &old),
            &Progress::new(),
        )
        .unwrap();
        drop(hook);
        let mut reads = reads.lock().unwrap().clone();
        reads.sort();
        assert_eq!(
            reads,
            [base.clone(), base.join("skip"), base.join("skip/inner")]
        );
        assert_eq!(
            snapshot(&Index::new(store)),
            snapshot(&full(&roots, &ScanOptions::default()))
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unobserved_folders_are_read_again_and_the_rest_is_copied() {
        use std::sync::{Arc, Mutex};
        let base = temp_tree("incremental-unobserved");
        let roots = [base.clone()];
        let options = ScanOptions::default();
        let previous = full(&roots, &options);
        // Missed because the folder had no watch.
        fs::write(base.join("a/b/missed.txt"), b"m").unwrap();
        let reads = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&reads);
        let hook = crate::listing::hook::set(move |path| {
            recorded.lock().unwrap().push(path.to_path_buf())
        });
        let mut store = NodeStore::new();
        let mut source = Source::current(&previous, &roots, &options);
        source.unobserved.insert(base.join("a/b"));
        assert!(
            update_with_progress(&mut store, &roots, &options, source, &Progress::new()).unwrap()
        );
        drop(hook);
        assert_eq!(*reads.lock().unwrap(), [base.join("a/b")]);
        assert_eq!(
            snapshot(&Index::new(store)),
            snapshot(&full(&roots, &options))
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn stale_file_details_are_read_again_without_reading_folders() {
        use std::sync::{Arc, Mutex};
        let base = temp_tree("incremental-stale-details");
        let roots = [base.clone()];
        let options = ScanOptions::default();
        let previous = full(&roots, &options);
        // Rewritten in place: no folder time changes, so a catch-up by
        // folder times keeps the old size and time.
        fs::write(base.join("a/one.txt"), b"rewritten in place").unwrap();
        fs::remove_file(base.join("a/b/two.TXT")).unwrap();
        let narrower = ScanOptions {
            rules: rules(&[], "", "*.bak", true),
            ..ScanOptions::default()
        };
        let copied = |stale: bool| {
            let reads = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&reads);
            let hook = crate::listing::hook::set(move |path| {
                recorded.lock().unwrap().push(path.to_path_buf())
            });
            let mut store = NodeStore::new();
            let mut source = Source::current(&previous, &roots, &options);
            source.stale_file_details = stale;
            assert!(
                update_with_progress(&mut store, &roots, &narrower, source, &Progress::new())
                    .unwrap()
            );
            drop(hook);
            assert!(reads.lock().unwrap().is_empty(), "no folder is listed");
            Index::new(store)
        };
        let size = |index: &Index| {
            let id = find_path(index, &base.join("a/one.txt")).unwrap();
            index.store().meta(id).size
        };
        assert_eq!(size(&copied(false)), 1, "a current source is trusted");
        let mut refreshed = copied(true);
        assert_eq!(size(&refreshed), b"rewritten in place".len() as u64);
        // The removed file's folder is marked for the catch-up, which drops it.
        crate::sync::catch_up(&mut refreshed, &narrower);
        assert_eq!(snapshot(&refreshed), snapshot(&full(&roots, &narrower)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn progress_of_a_reused_tree_counts_only_kept_entries() {
        let base = temp_tree("incremental-progress");
        for i in 0..3000 {
            fs::write(base.join(format!("a/drop{i}.tmp")), b"").unwrap();
        }
        let roots = [base.clone()];
        let previous = full(&roots, &ScanOptions::default());
        let narrower = ScanOptions {
            rules: rules(&[], "", "*.tmp", true),
            ..ScanOptions::default()
        };
        let progress = Progress::new();
        let mut store = NodeStore::new();
        assert!(
            update_with_progress(
                &mut store,
                &roots,
                &narrower,
                Source::current(&previous, &roots, &ScanOptions::default()),
                &progress
            )
            .unwrap()
        );
        let fresh = Progress::new();
        scan_with_progress(&mut NodeStore::new(), &roots, &narrower, &fresh).unwrap();
        assert_eq!(progress.done(), fresh.done());
        assert_eq!(progress.done(), Index::new(store).len() as u64);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn incremental_cancel_preserves_source_and_copied_times_allow_catch_up() {
        let base = temp_tree("incremental-cancel");
        let roots = [base.clone()];
        let options = ScanOptions::default();
        let previous = full(&roots, &options);
        let before = snapshot(&previous);
        let cancelled = Progress::new();
        cancelled.cancel_when_done(2);
        let mut store = NodeStore::new();
        assert_eq!(
            update_with_progress(
                &mut store,
                &roots,
                &options,
                Source::current(&previous, &roots, &options),
                &cancelled
            ),
            Err(Cancelled)
        );
        assert_eq!(snapshot(&previous), before);
        // A change made while watches were down must not be hidden by a fresh
        // directory time paired with a copied, older listing.
        fs::write(base.join("late.txt"), b"late").unwrap();
        let mut store = NodeStore::new();
        update_with_progress(
            &mut store,
            &roots,
            &options,
            Source::current(&previous, &roots, &options),
            &Progress::new(),
        )
        .unwrap();
        let mut updated = Index::new(store);
        crate::sync::catch_up(&mut updated, &options);
        assert_eq!(snapshot(&updated), snapshot(&full(&roots, &options)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn scans_tree_without_following_links_and_honours_excludes() {
        let base = temp_tree("walk");
        let mut store = NodeStore::new();
        let options = ScanOptions {
            rules: rules(&[base.join("skip")], "", "", true),
            ..ScanOptions::default()
        };
        let stats = scan(&mut store, std::slice::from_ref(&base), &options);
        fs::remove_dir_all(&base).unwrap();

        let paths: Vec<PathBuf> = (0..store.len())
            .map(|i| store.path(NodeId(i as u32)))
            .collect();
        assert!(paths.contains(&base.join("a/b/two.TXT")));
        assert!(
            !paths.contains(&base.join("skip")),
            "excluded with its entry"
        );
        assert!(!paths.contains(&base.join("skip/inner")));
        assert!(paths.contains(&base.join("link-to-root")));
        // root, a, a/b = 3 folders; one.txt, two.TXT, link = 3 files
        assert_eq!(
            stats,
            ScanStats {
                dirs: 3,
                files: 3,
                errors: 0
            }
        );
        let two = (0..store.len())
            .map(|i| NodeId(i as u32))
            .find(|&id| store.name(id) == b"two.TXT")
            .unwrap();
        assert_eq!(store.meta(two).size, 2);
        // Folder totals: one.txt + two.TXT + the link (its target "/" is one
        // byte long).
        let size_of = |name: &[u8]| {
            let id = (0..store.len())
                .map(|i| NodeId(i as u32))
                .find(|&id| store.name(id) == name)
                .unwrap();
            (store.meta(id).size, store.items(id))
        };
        assert_eq!(size_of(base.as_os_str().as_bytes()), (4, 2));
        assert_eq!(size_of(b"a"), (3, 2));
    }

    #[test]
    fn a_queued_folder_replaced_by_a_link_is_not_followed() {
        let base = temp_tree("swap");
        let outside = base.with_extension("outside");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"").unwrap();
        // `a` was listed as a folder; it becomes a link just before it is read.
        let (a, target) = (base.join("a"), outside.clone());
        let _hook = crate::listing::hook::set(move |dir| {
            if dir == a {
                fs::remove_dir_all(&a).unwrap();
                std::os::unix::fs::symlink(&target, &a).unwrap();
            }
        });
        let mut store = NodeStore::new();
        let stats = scan(
            &mut store,
            std::slice::from_ref(&base),
            &ScanOptions::default(),
        );
        drop(_hook);
        fs::remove_dir_all(&base).unwrap();
        fs::remove_dir_all(&outside).unwrap();

        let paths: Vec<PathBuf> = (0..store.len())
            .map(|i| store.path(NodeId(i as u32)))
            .collect();
        assert!(!paths.contains(&base.join("a/secret.txt")), "{paths:?}");
        let a = (0..store.len())
            .map(|i| NodeId(i as u32))
            .find(|&id| store.path(id) == base.join("a"))
            .unwrap();
        assert!(store.is_dir(a));
        assert_eq!(store.meta(a).mtime_ns, 0, "read again by the next catch-up");
        assert_eq!(stats.errors, 1);
    }

    #[test]
    fn a_switched_off_exclude_list_indexes_excluded_folders() {
        let base = temp_tree("off");
        let options = ScanOptions {
            rules: rules(&[base.join("skip")], "*.txt", "*", false),
            ..ScanOptions::default()
        };
        let (paths, stats) = scanned_paths(&base, &options);
        fs::remove_dir_all(&base).unwrap();
        assert!(paths.contains(&base.join("skip/inner/hidden.txt")));
        assert_eq!(stats.dirs + stats.files, 9);
    }

    #[test]
    fn patterns_leave_out_folders_and_files() {
        let base = temp_tree("patterns");
        fs::write(base.join("a/keep.mp3"), b"").unwrap();
        fs::write(base.join("a/b/old.mp3"), b"").unwrap();
        let options = ScanOptions {
            rules: rules(
                &["SK*".into(), "regex:^b$".into()],
                "*.txt;*.mp3",
                "regex:^two",
                true,
            ),
            ..ScanOptions::default()
        };
        let (paths, _) = scanned_paths(&base, &options);
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.strip_prefix(&base).unwrap().display().to_string())
            .collect();
        // `skip` by wildcard (case ignored), `b` by regex, `two.TXT` excluded,
        // the link not included; folders are kept by the file lists.
        assert_eq!(names, ["", "a", "a/keep.mp3", "a/one.txt"]);

        // A path pattern.
        let options = ScanOptions {
            rules: rules(&[], "", "a/**.txt", true),
            ..ScanOptions::default()
        };
        let (paths, _) = scanned_paths(&base, &options);
        fs::remove_dir_all(&base).unwrap();
        assert!(!paths.contains(&base.join("a/one.txt")));
        assert!(!paths.contains(&base.join("a/b/two.TXT")));
        assert!(paths.contains(&base.join("skip/inner/hidden.txt")));
    }

    #[test]
    fn progress_counts_every_entry_and_cancel_stops_the_scan() {
        let base = temp_tree("progress");
        for i in 0..2500 {
            fs::write(base.join(format!("a/many-{i}")), b"").unwrap();
        }
        let options = ScanOptions::default();
        let progress = Progress::new();
        let mut store = NodeStore::new();
        let stats =
            scan_with_progress(&mut store, std::slice::from_ref(&base), &options, &progress)
                .unwrap();
        assert_eq!(progress.done(), store.len() as u64);
        assert_eq!(stats.dirs + stats.files, store.len() as u64);

        // Cancelled before it starts: nothing is read.
        let cancelled = Progress::new();
        cancelled.cancel();
        let mut part = NodeStore::new();
        let result =
            scan_with_progress(&mut part, std::slice::from_ref(&base), &options, &cancelled);
        assert_eq!(result, Err(Cancelled));
        assert!(part.is_empty());

        // Cancelled part way (from another thread, as the window does): the
        // scan stops early and reports it.
        let midway = Progress::new();
        let mut part = NodeStore::new();
        let result = std::thread::scope(|s| {
            s.spawn(|| {
                while midway.done() == 0 {
                    std::thread::yield_now();
                }
                midway.cancel();
            });
            // Ends either way; with a cancel it must say so.
            scan_with_progress(&mut part, std::slice::from_ref(&base), &options, &midway)
        });
        fs::remove_dir_all(&base).unwrap();
        if midway.is_cancelled() {
            assert!(result.is_err() || part.len() == store.len());
        }
        assert!(part.len() <= store.len());
    }

    #[test]
    fn roots_are_resolved_deduplicated_and_not_nested() {
        let base = temp_tree("roots");
        let link = base.join("link-to-a");
        std::os::unix::fs::symlink(base.join("a"), &link).unwrap();
        let roots = normalize_roots(&[
            base.join("a/b"),
            link.clone(),
            base.join("a"),
            base.join("skip"),
            base.join("skip"),
        ]);
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(roots, vec![base.join("a"), base.join("skip")]);
    }

    #[test]
    fn unescapes_mount_paths() {
        assert_eq!(unescape_mount_path(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount_path("/plain"), "/plain");
    }
}
