//! Building a [`NodeStore`] by walking directory trees.

use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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
            walk(store, id, root, options, &skip, &mut stats, progress)?;
        } else {
            stats.files += 1;
        }
    }
    Ok(stats)
}

fn walk(
    store: &mut NodeStore,
    root_id: NodeId,
    root: &Path,
    options: &ScanOptions,
    skip: &Skip,
    stats: &mut ScanStats,
    progress: &Progress,
) -> Result<(), Cancelled> {
    let mut pending = vec![(root_id, root.to_path_buf())];
    let result = walk_pending(store, &mut pending, options, skip, stats, progress);
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

/// Reads the folders in `pending` and the folders found below them. A
/// folder is taken off `pending` only once it is being read.
fn walk_pending(
    store: &mut NodeStore,
    pending: &mut Vec<(NodeId, PathBuf)>,
    options: &ScanOptions,
    skip: &Skip,
    stats: &mut ScanStats,
    progress: &Progress,
) -> Result<(), Cancelled> {
    let mut unreported = 0;
    while !pending.is_empty() {
        progress.add(std::mem::take(&mut unreported));
        progress.check()?;
        let Some((dir_id, dir)) = pending.pop() else {
            break;
        };
        // A failed or partial listing must be retried even if chmod leaves
        // the directory's modification time unchanged.
        let complete_meta = store.meta(dir_id);
        store.set_meta(
            dir_id,
            Meta {
                mtime_ns: 0,
                ..complete_meta
            },
        );
        let mut complete = true;
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                stats.errors += 1;
                complete = false;
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                complete = false;
                stats.errors += 1;
                continue;
            };
            let is_dir = file_type.is_dir();
            let name = entry.file_name();
            if !skip.rules.accepts(&dir, name.as_bytes(), is_dir) {
                continue;
            }
            let meta = if options.with_metadata || is_dir {
                match entry.metadata() {
                    Ok(m) => meta_of(&m, options),
                    Err(_) => {
                        complete = false;
                        stats.errors += 1;
                        Meta::default()
                    }
                }
            } else {
                Meta::default()
            };
            let id = store.add(Some(dir_id), name.as_bytes(), is_dir, meta);
            unreported += 1;
            if unreported == PROGRESS_BATCH {
                progress.add(std::mem::take(&mut unreported));
                progress.check()?;
            }
            if is_dir {
                stats.dirs += 1;
                let path = entry.path();
                if skip.enters(&path) {
                    pending.push((id, path));
                }
            } else {
                stats.files += 1;
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
    pub(crate) fn entry_meta(&self, entry: &fs::DirEntry, is_dir: bool) -> std::io::Result<Meta> {
        if self.with_metadata || is_dir {
            entry.metadata().map(|m| Meta::from_metadata(&m))
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
