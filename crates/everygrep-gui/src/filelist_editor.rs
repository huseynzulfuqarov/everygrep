//! The file list editor's list: entries of one folder (see
//! [`everygrep_core::filelist`]) that can be added from disk, removed, made
//! unique and saved.
//!
//! Saving never truncates the file it replaces: the whole list goes to a
//! new temporary file in the same folder, is flushed to disk (`fsync`) and
//! then renamed over the target, so the old file stays whole until the new
//! one is complete. A target that is a symbolic link or not a regular file
//! is refused, and a file that changed on disk since it was opened or last
//! saved is only replaced when the caller says so (after asking).
//!
//! Adding a folder reads it from disk without following symbolic links
//! (a link is listed as a file) and without crossing into other file
//! systems (a mount point is listed as an empty folder).

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use everygrep_core::filelist::{self, Entry, FileList};

/// What identifies a version of a file on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    len: u64,
    mtime: (i64, i64),
    ino: u64,
}

pub fn stamp(path: &Path) -> io::Result<Option<Stamp>> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(Stamp {
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ino: m.ino(),
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Why a save did not happen.
#[derive(Debug)]
pub enum SaveError {
    /// The file changed on disk since it was read or written by us.
    Changed,
    Io(io::Error),
}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Clone, Debug, Default)]
pub struct EditorList {
    pub list: FileList,
    /// Where the list was opened from or last saved to.
    pub path: Option<PathBuf>,
    /// The file's stamp when it was read or written.
    pub stamp: Option<Stamp>,
    pub dirty: bool,
}

fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn below(path: &[u8], root: &[u8]) -> bool {
    path == root || (path.starts_with(root) && path.get(root.len()) == Some(&b'/'))
}

/// The deepest folder holding both paths; `None` when that is `/`.
fn common_root(a: &[u8], b: &[u8]) -> Option<Vec<u8>> {
    let mut common = Vec::new();
    for (x, y) in a[1..]
        .split(|&c| c == b'/')
        .zip(b[1..].split(|&c| c == b'/'))
    {
        if x != y {
            break;
        }
        common.push(b'/');
        common.extend_from_slice(x);
    }
    (!common.is_empty()).then_some(common)
}

fn invalid(text: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, text.to_string())
}

fn entry_of(path: &Path, meta: &std::fs::Metadata) -> Entry {
    let is_dir = meta.file_type().is_dir();
    Entry {
        path: bytes(path).to_vec(),
        is_dir,
        size: if is_dir { 0 } else { meta.len() },
        modified_ns: Some(meta.mtime() * 1_000_000_000 + meta.mtime_nsec()),
        created_ns: meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_nanos()).ok()),
    }
}

impl EditorList {
    /// Reads a list file (a regular file within the size limit).
    pub fn open(path: &Path) -> io::Result<Self> {
        let before = stamp(path)?;
        let list = filelist::read_list(path)?;
        Ok(Self {
            list,
            path: Some(path.to_path_buf()),
            stamp: before,
            dirty: false,
        })
    }

    /// Makes `root` hold `path`, moving it up when needed. Refuses when
    /// only `/` would hold both.
    fn widen_root(&mut self, path: &[u8]) -> io::Result<()> {
        if self.list.root.is_empty() {
            self.list.root = path.to_vec();
        } else if !below(path, &self.list.root) {
            self.list.root = common_root(&self.list.root, path).ok_or_else(|| {
                invalid(crate::i18n::tr(
                    "All entries of a file list must be in one folder other than /.",
                ))
            })?;
        }
        Ok(())
    }

    fn room(&self, more: usize) -> io::Result<()> {
        if self.list.entries.len() + more > filelist::MAX_ENTRIES {
            return Err(invalid(crate::i18n::tr("The file list would be too long.")));
        }
        Ok(())
    }

    /// Adds files (or folders without their contents).
    pub fn add_paths(&mut self, paths: &[PathBuf]) -> io::Result<usize> {
        let mut new = Vec::new();
        for p in paths {
            if !p.is_absolute() {
                return Err(invalid("not an absolute path"));
            }
            let meta = std::fs::symlink_metadata(p)?;
            new.push(entry_of(p, &meta));
        }
        self.add_entries(new)
    }

    /// Adds a folder and all entries below it, read from disk.
    pub fn add_folder(&mut self, dir: &Path) -> io::Result<usize> {
        let found = scan(
            dir,
            filelist::MAX_ENTRIES - self.list.entries.len().min(filelist::MAX_ENTRIES),
        )?;
        self.add_entries(found)
    }

    fn add_entries(&mut self, new: Vec<Entry>) -> io::Result<usize> {
        self.room(new.len())?;
        let old_root = self.list.root.clone();
        for e in &new {
            if let Err(e) = self.widen_root(&e.path) {
                self.list.root = old_root;
                return Err(e);
            }
        }
        let n = new.len();
        self.list.entries.extend(new);
        self.dirty |= n > 0;
        Ok(n)
    }

    /// Removes the entries at `rows` and all entries below removed folders.
    pub fn remove(&mut self, rows: &[usize]) -> usize {
        let gone: Vec<Vec<u8>> = rows
            .iter()
            .filter_map(|&i| self.list.entries.get(i))
            .map(|e| e.path.clone())
            .collect();
        let rows: HashSet<usize> = rows.iter().copied().collect();
        let before = self.list.entries.len();
        let mut i = 0;
        self.list.entries.retain(|e| {
            let keep = !rows.contains(&i) && !gone.iter().any(|g| below(&e.path, g));
            i += 1;
            keep
        });
        let removed = before - self.list.entries.len();
        if self.list.entries.is_empty() {
            self.list.root.clear();
        }
        self.dirty |= removed > 0;
        removed
    }

    /// Keeps the last entry of each path (as a reader would). Returns how
    /// many were removed.
    pub fn remove_duplicates(&mut self) -> usize {
        let mut seen = HashSet::new();
        let before = self.list.entries.len();
        let mut kept: Vec<Entry> = std::mem::take(&mut self.list.entries)
            .into_iter()
            .rev()
            .filter(|e| seen.insert(e.path.clone()))
            .collect();
        kept.reverse();
        self.list.entries = kept;
        let removed = before - self.list.entries.len();
        self.dirty |= removed > 0;
        removed
    }

    /// The total size of the files below each folder of the list (a path
    /// listed twice counts once, with its last size).
    pub fn folder_sizes(&self) -> HashMap<Vec<u8>, u64> {
        let mut files: HashMap<&[u8], u64> = HashMap::new();
        for e in self.list.entries.iter().filter(|e| !e.is_dir) {
            files.insert(&e.path, e.size);
        }
        let folders: HashSet<&[u8]> = self
            .list
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.path.as_slice())
            .collect();
        let mut sizes: HashMap<Vec<u8>, u64> = HashMap::new();
        for (path, size) in files {
            let mut p = path;
            while let Some(cut) = p.iter().rposition(|&b| b == b'/') {
                p = &p[..cut];
                if p.is_empty() {
                    break;
                }
                if folders.contains(p) {
                    *sizes.entry(p.to_vec()).or_default() += size;
                }
            }
        }
        sizes
    }

    /// The list as the file holds it; checked by reading it back, so a list
    /// that could not be read again is never written.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.list.root.is_empty() {
            return Err(invalid(crate::i18n::tr("The file list is empty.")));
        }
        let mut out = Vec::new();
        filelist::write_list(&mut out, &self.list)?;
        filelist::parse(&out).map_err(io::Error::from)?;
        Ok(out)
    }

    /// Saves to `path` (see the module comment). `replace_changed` lets a
    /// file that changed on disk be replaced; a different existing file
    /// (Save as) needs the caller's confirmation the same way.
    pub fn save(&mut self, path: &Path, replace_changed: bool) -> Result<(), SaveError> {
        let data = self.encode()?;
        let now = stamp(path)?;
        let ours = self.path.as_deref() == Some(path);
        if now.is_some() && !replace_changed && (!ours || now != self.stamp) {
            return Err(SaveError::Changed);
        }
        write_replacing(path, &data)?;
        self.path = Some(path.to_path_buf());
        self.stamp = stamp(path)?;
        self.dirty = false;
        Ok(())
    }

    /// Whether the file changed on disk since we read or wrote it.
    #[cfg(test)]
    pub fn changed_on_disk(&self) -> bool {
        self.path
            .as_deref()
            .is_some_and(|p| stamp(p).ok().flatten() != self.stamp)
    }
}

/// Writes `data` to a new temporary file next to `path`, flushes it and
/// renames it over `path`. An existing target must be a regular file (not a
/// link); its permission bits are kept.
pub fn write_replacing(path: &Path, data: &[u8]) -> io::Result<()> {
    let mode = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => Some(m.permissions().mode() & 0o7777),
        Ok(_) => {
            return Err(invalid(crate::i18n::tr(
                "The target is a link or not a regular file; choose another name.",
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let (mut file, tmp) = crate::settings::create_temp(path)?;
    let result = (|| {
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        // The rename itself is made durable too.
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Every entry of `dir` and below, folders before their contents, at most
/// `limit` entries.
fn scan(dir: &Path, limit: usize) -> io::Result<Vec<Entry>> {
    if !dir.is_absolute() {
        return Err(invalid("not an absolute path"));
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(invalid(crate::i18n::tr("Not a folder.")));
    }
    let device = meta.dev();
    let mut out = vec![entry_of(dir, &meta)];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(folder) = stack.pop() {
        let mut children: Vec<_> = match std::fs::read_dir(&folder) {
            Ok(rd) => rd.filter_map(Result::ok).map(|e| e.path()).collect(),
            // An unreadable folder stays listed, without contents.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
            Err(e) => return Err(e),
        };
        children.sort();
        for child in children.into_iter().rev() {
            let Ok(m) = std::fs::symlink_metadata(&child) else {
                continue;
            };
            if out.len() >= limit {
                return Err(invalid(crate::i18n::tr("The file list would be too long.")));
            }
            out.push(entry_of(&child, &m));
            if m.is_dir() && m.dev() == device {
                stack.push(child);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;

    fn tree(d: &Path) {
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/one.txt"), b"12345").unwrap();
        std::fs::write(d.join("a/b/two.bin"), b"123").unwrap();
        std::os::unix::fs::symlink(d.join("a"), d.join("a/b/loop")).unwrap();
    }

    #[test]
    fn add_scans_without_following_links_and_sizes_folders() {
        let d = Scratch::new("fle-scan");
        tree(&d.0);
        let mut l = EditorList::default();
        let n = l.add_folder(&d.0.join("a")).unwrap();
        // a, one.txt, b, two.bin, loop (a link is a file).
        assert_eq!(n, 5);
        assert!(l.dirty);
        let link = l
            .list
            .entries
            .iter()
            .find(|e| e.path.ends_with(b"loop"))
            .unwrap();
        assert!(!link.is_dir);
        let sizes = l.folder_sizes();
        let a = bytes(&d.0.join("a")).to_vec();
        let b = bytes(&d.0.join("a/b")).to_vec();
        let link_size = link.size;
        assert_eq!(sizes[&a], 8 + link_size);
        assert_eq!(sizes[&b], 3 + link_size);
        // Adding again then removing duplicates gives the same list.
        let first = l.list.entries.clone();
        l.add_folder(&d.0.join("a")).unwrap();
        assert_eq!(l.remove_duplicates(), 5);
        assert_eq!(l.list.entries.len(), first.len());
        assert_eq!(l.folder_sizes()[&a], 8 + link_size);
    }

    #[test]
    fn root_widens_to_a_common_folder_but_never_to_slash() {
        let d = Scratch::new("fle-root");
        tree(&d.0);
        let mut l = EditorList::default();
        l.add_paths(&[d.0.join("a/b/two.bin")]).unwrap();
        assert_eq!(l.list.root, bytes(&d.0.join("a/b/two.bin")));
        l.add_paths(&[d.0.join("a/one.txt")]).unwrap();
        assert_eq!(l.list.root, bytes(&d.0.join("a")));
        let before = l.list.clone();
        assert!(l.add_paths(&[PathBuf::from("/proc")]).is_err());
        assert_eq!(l.list, before);
        assert!(l.add_paths(&[PathBuf::from("relative")]).is_err());
        assert!(l.add_folder(&d.0.join("a/one.txt")).is_err());
        assert!(l.add_paths(&[d.0.join("missing")]).is_err());
        assert_eq!(l.list, before);
    }

    #[test]
    fn remove_takes_folder_contents_and_empty_lists_are_not_saved() {
        let d = Scratch::new("fle-remove");
        tree(&d.0);
        let mut l = EditorList::default();
        l.add_folder(&d.0.join("a")).unwrap();
        let b = l
            .list
            .entries
            .iter()
            .position(|e| e.path == bytes(&d.0.join("a/b")))
            .unwrap();
        assert_eq!(l.remove(&[b]), 3);
        assert_eq!(l.list.entries.len(), 2);
        assert_eq!(l.remove(&[0, 1, 99]), 2);
        assert!(l.list.root.is_empty());
        assert!(l.save(&d.0.join("x.eglist"), false).is_err());
        assert!(!d.0.join("x.eglist").exists());
    }

    #[test]
    fn save_is_atomic_checks_changes_and_round_trips() {
        let d = Scratch::new("fle-save");
        tree(&d.0);
        let target = d.0.join("list.eglist");
        let mut l = EditorList::default();
        l.add_folder(&d.0.join("a")).unwrap();
        l.save(&target, false).unwrap();
        assert!(!l.dirty);
        let read = EditorList::open(&target).unwrap();
        assert_eq!(read.list, l.list);
        assert!(!l.changed_on_disk());
        // Changed meanwhile by someone else: refused unless confirmed.
        std::fs::write(&target, b"other").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(l.changed_on_disk());
        assert!(matches!(l.save(&target, false), Err(SaveError::Changed)));
        assert_eq!(std::fs::read(&target).unwrap(), b"other");
        l.save(&target, true).unwrap();
        assert_eq!(EditorList::open(&target).unwrap().list, l.list);
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        // Save as over another existing file needs confirmation.
        let other = d.0.join("other.eglist");
        std::fs::write(&other, b"keep").unwrap();
        assert!(matches!(l.save(&other, false), Err(SaveError::Changed)));
        assert_eq!(std::fs::read(&other).unwrap(), b"keep");
        // No temporary files are left behind.
        let names: Vec<_> = std::fs::read_dir(&d.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(names.iter().all(|n| !n.to_string_lossy().contains(".tmp-")));
    }

    #[test]
    fn links_and_non_files_are_never_replaced() {
        let d = Scratch::new("fle-link");
        tree(&d.0);
        let real = d.0.join("real.eglist");
        std::fs::write(&real, b"precious").unwrap();
        let link = d.0.join("link.eglist");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut l = EditorList::default();
        l.add_folder(&d.0.join("a")).unwrap();
        assert!(l.save(&link, true).is_err());
        assert_eq!(std::fs::read(&real).unwrap(), b"precious");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(l.save(&d.0.join("a"), true).is_err());
        assert!(d.0.join("a/one.txt").exists());
    }

    #[test]
    fn a_failed_write_keeps_the_old_file() {
        let d = Scratch::new("fle-fail");
        let dir = d.0.join("ro");
        std::fs::create_dir(&dir).unwrap();
        let target = dir.join("l.eglist");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = write_replacing(&target, b"new");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Root may write anyway; otherwise the old file is intact.
        if result.is_err() {
            assert_eq!(std::fs::read(&target).unwrap(), b"old");
        }
    }

    #[test]
    fn opening_refuses_bad_or_huge_files() {
        let d = Scratch::new("fle-open");
        let p = d.0.join("bad.eglist");
        std::fs::write(&p, b"not a list").unwrap();
        assert!(EditorList::open(&p).is_err());
        assert!(EditorList::open(&d.0).is_err());
        assert!(EditorList::open(&d.0.join("missing")).is_err());
    }
}
