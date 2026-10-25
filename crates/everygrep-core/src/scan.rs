//! Building a [`NodeStore`] by walking directory trees.

use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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
    /// Read size and modification time of every entry (one `lstat` per entry).
    pub with_metadata: bool,
    /// Directories that are not entered. Their own entry is still indexed.
    pub exclude: Vec<PathBuf>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            with_metadata: true,
            exclude: Vec::new(),
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
/// systems ([`VIRTUAL_FS_TYPES`]) and folders in `options.exclude` are indexed
/// but not entered.
pub fn scan(store: &mut NodeStore, roots: &[PathBuf], options: &ScanOptions) -> ScanStats {
    let skip = skip_set(options);
    let mut stats = ScanStats::default();
    for root in roots {
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
        if meta.is_dir() {
            stats.dirs += 1;
            walk(store, id, root, options, &skip, &mut stats);
        } else {
            stats.files += 1;
        }
    }
    stats
}

fn walk(
    store: &mut NodeStore,
    root_id: NodeId,
    root: &Path,
    options: &ScanOptions,
    skip: &HashSet<PathBuf>,
    stats: &mut ScanStats,
) {
    let mut pending = vec![(root_id, root.to_path_buf())];
    while let Some((dir_id, dir)) = pending.pop() {
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
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                stats.errors += 1;
                continue;
            };
            let is_dir = file_type.is_dir();
            let meta = if options.with_metadata {
                match entry.metadata() {
                    Ok(m) => meta_of(&m, options),
                    Err(_) => {
                        stats.errors += 1;
                        Meta::default()
                    }
                }
            } else {
                Meta::default()
            };
            let id = store.add(Some(dir_id), entry.file_name().as_bytes(), is_dir, meta);
            if is_dir {
                stats.dirs += 1;
                let path = entry.path();
                if !skip.contains(&path) {
                    pending.push((id, path));
                }
            } else {
                stats.files += 1;
            }
        }
    }
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

/// Folders that are indexed but not entered: virtual file system mount points and
/// the configured excludes.
pub(crate) fn skip_set(options: &ScanOptions) -> HashSet<PathBuf> {
    let mut skip = virtual_mount_points();
    skip.extend(options.exclude.iter().cloned());
    skip
}

fn meta_of(m: &fs::Metadata, options: &ScanOptions) -> Meta {
    if options.with_metadata {
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

    #[test]
    fn scans_tree_without_following_links_and_honours_excludes() {
        let base = temp_tree("walk");
        let mut store = NodeStore::new();
        let options = ScanOptions {
            exclude: vec![base.join("skip")],
            ..ScanOptions::default()
        };
        let stats = scan(&mut store, std::slice::from_ref(&base), &options);
        fs::remove_dir_all(&base).unwrap();

        let paths: Vec<PathBuf> = (0..store.len())
            .map(|i| store.path(NodeId(i as u32)))
            .collect();
        assert!(paths.contains(&base.join("a/b/two.TXT")));
        assert!(paths.contains(&base.join("skip")));
        assert!(!paths.contains(&base.join("skip/inner")));
        assert!(paths.contains(&base.join("link-to-root")));
        // root, a, a/b, skip  = 4 folders; one.txt, two.TXT, link = 3 files
        assert_eq!(
            stats,
            ScanStats {
                dirs: 4,
                files: 3,
                errors: 0
            }
        );
        let two = (0..store.len())
            .map(|i| NodeId(i as u32))
            .find(|&id| store.name(id) == b"two.TXT")
            .unwrap();
        assert_eq!(store.meta(two).size, 2);
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
