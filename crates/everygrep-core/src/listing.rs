//! Reading folders without following symbolic links.
//!
//! A walk learns that an entry is a folder when it reads the parent, and
//! reads the folder later. In between, the folder (or a folder above it) can
//! be replaced by a symbolic link, and opening it by path would then read
//! whatever the link points to. Folders are therefore opened relative to the
//! root of their tree, with no symbolic link allowed anywhere below the root:
//! one `openat2` with `RESOLVE_NO_SYMLINKS`, or, where the kernel does not
//! have `openat2`, one `openat` with `O_NOFOLLOW` per path component. A
//! folder replaced by a link then fails to open and is handled like any
//! other folder that cannot be read. Only the root and the folder being read
//! are open at a time.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{AtFlags, Dir, DirEntry, FileType, Mode, OFlags, ResolveFlags, Stat};
use rustix::io::Errno;

use crate::store::Meta;

/// The kernel refused `openat2` once, so components are opened one by one.
static NO_OPENAT2: AtomicBool = AtomicBool::new(false);

/// A root folder that folders below it are opened from.
pub(crate) struct Tree {
    root: PathBuf,
    /// The root, opened for path lookups only (`O_PATH`).
    fd: OwnedFd,
}

impl Tree {
    /// Opens the root folder `root`. Folders above it are followed as
    /// usual; `root` itself must be a folder, not a symbolic link.
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        let fd = rustix::fs::open(
            root,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            root: root.to_path_buf(),
            fd,
        })
    }

    /// Opens the folder `dir`, the root or a folder below it, for reading.
    pub(crate) fn list(&self, dir: &Path) -> io::Result<Listing> {
        #[cfg(test)]
        hook::before_open(dir);
        let rel = dir
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let fd = if rel.as_os_str().is_empty() {
            self.open_root()?
        } else if NO_OPENAT2.load(Ordering::Relaxed) {
            open_by_components(self.fd.as_fd(), rel)?
        } else {
            match open_beneath(self.fd.as_fd(), rel) {
                Err(Errno::NOSYS | Errno::PERM) => {
                    // Too old a kernel, or blocked by a sandbox.
                    NO_OPENAT2.store(true, Ordering::Relaxed);
                    open_by_components(self.fd.as_fd(), rel)?
                }
                fd => fd?,
            }
        };
        Ok(Listing { dir: Dir::new(fd)? })
    }

    /// Opens the root itself for reading. Looking up `.` in it would need
    /// search permission on it, which reading it does not, so it is opened
    /// by its path again, still not followed if it became a link, and taken
    /// only if it is the folder opened first.
    fn open_root(&self) -> io::Result<OwnedFd> {
        let fd = rustix::fs::open(&self.root, read_flags(), Mode::empty())?;
        let (now, first) = (rustix::fs::fstat(&fd)?, rustix::fs::fstat(&self.fd)?);
        if (now.st_dev, now.st_ino) != (first.st_dev, first.st_ino) {
            return Err(io::Error::other("the root folder was replaced"));
        }
        Ok(fd)
    }
}

/// Flags for the folder that is read.
fn read_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// The path to open for `rel` below a folder: `.` for the folder itself.
fn or_dot(rel: &Path) -> &Path {
    if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    }
}

/// Opens `rel` below `root` with one `openat2` that fails on any symbolic
/// link on the way.
fn open_beneath(root: BorrowedFd<'_>, rel: &Path) -> Result<OwnedFd, Errno> {
    rustix::fs::openat2(
        root,
        or_dot(rel),
        read_flags(),
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
}

/// Opens `rel` below `root` one component at a time, none of them followed
/// when it is a symbolic link. Folders on the way are opened for lookups
/// only, so like a path lookup they need search but not read permission.
fn open_by_components(root: BorrowedFd<'_>, rel: &Path) -> io::Result<OwnedFd> {
    let mut parts = rel.iter().peekable();
    if parts.peek().is_none() {
        return Ok(rustix::fs::openat(root, ".", read_flags(), Mode::empty())?);
    }
    let mut cur: Option<OwnedFd> = None;
    while let Some(part) = parts.next() {
        let at = cur.as_ref().map_or(root, |fd| fd.as_fd());
        let flags = if parts.peek().is_some() {
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            read_flags()
        };
        cur = Some(rustix::fs::openat(at, part, flags, Mode::empty())?);
    }
    cur.ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

/// An open folder whose entries are read one by one.
pub(crate) struct Listing {
    dir: Dir,
}

/// An entry of a [`Listing`].
pub(crate) struct Entry(DirEntry);

impl Entry {
    pub(crate) fn name(&self) -> &[u8] {
        self.0.file_name().to_bytes()
    }

    pub(crate) fn name_os(&self) -> &OsStr {
        OsStr::from_bytes(self.name())
    }
}

impl Listing {
    /// The next entry other than `.` and `..`, or `None` at the end. After
    /// an error the listing ends.
    pub(crate) fn read(&mut self) -> Option<io::Result<Entry>> {
        loop {
            match self.dir.read()? {
                Ok(e) if matches!(e.file_name().to_bytes(), b"." | b"..") => {}
                Ok(e) => return Some(Ok(Entry(e))),
                Err(e) => return Some(Err(e.into())),
            }
        }
    }

    /// Whether `entry` is a folder (not a link to one). The type comes from
    /// the listing; file systems that do not report it are asked with an
    /// `lstat`.
    pub(crate) fn is_dir(&self, entry: &Entry) -> io::Result<bool> {
        match entry.0.file_type() {
            FileType::Unknown => Ok(FileType::from_raw_mode(self.stat(entry)?.st_mode).is_dir()),
            t => Ok(t.is_dir()),
        }
    }

    /// Metadata of `entry` itself, as [`Meta::from_metadata`] gives it.
    pub(crate) fn meta(&self, entry: &Entry) -> io::Result<Meta> {
        Ok(meta_of_stat(&self.stat(entry)?))
    }

    /// Device and inode number of the open folder.
    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        let st = self.dir.stat()?;
        #[allow(clippy::unnecessary_cast)] // The field types differ between targets.
        Ok((st.st_dev as u64, st.st_ino as u64))
    }

    fn stat(&self, entry: &Entry) -> io::Result<Stat> {
        Ok(rustix::fs::statat(
            self.dir.fd()?,
            entry.0.file_name(),
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }
}

/// Like [`Meta::from_metadata`]: folders get size 0.
#[allow(clippy::unnecessary_cast)] // The field types differ between targets.
fn meta_of_stat(st: &Stat) -> Meta {
    let is_dir = FileType::from_raw_mode(st.st_mode).is_dir();
    Meta {
        size: if is_dir { 0 } else { st.st_size as u64 },
        mtime_ns: (st.st_mtime as i64)
            .saturating_mul(1_000_000_000)
            .saturating_add(st.st_mtime_nsec as i64),
    }
}

/// Tests: a function called with each folder just before it is opened.
#[cfg(test)]
pub(crate) mod hook {
    use std::cell::RefCell;
    use std::path::Path;

    type Hook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static BEFORE_OPEN: RefCell<Option<Hook>> = RefCell::new(None);
    }

    /// Calls `f` with every folder this thread opens until the returned
    /// guard is dropped.
    pub(crate) fn set(f: impl FnMut(&Path) + 'static) -> Guard {
        BEFORE_OPEN.with(|h| *h.borrow_mut() = Some(Box::new(f)));
        Guard
    }

    pub(crate) fn before_open(dir: &Path) {
        BEFORE_OPEN.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f(dir);
            }
        });
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            BEFORE_OPEN.with(|h| *h.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn tree(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("everygrep-listing-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root/a/b")).unwrap();
        fs::create_dir_all(base.join("outside/b")).unwrap();
        fs::write(base.join("root/a/b/inside.txt"), b"").unwrap();
        fs::write(base.join("outside/b/secret.txt"), b"").unwrap();
        base
    }

    fn names(tree: &Tree, dir: &Path) -> io::Result<Vec<Vec<u8>>> {
        let mut listing = tree.list(dir)?;
        let mut out = Vec::new();
        while let Some(e) = listing.read() {
            out.push(e?.name().to_vec());
        }
        out.sort();
        Ok(out)
    }

    #[test]
    fn opens_folders_below_the_root_but_no_symbolic_link() {
        let base = tree("links");
        let root = base.join("root");
        let tree = Tree::open(&root).unwrap();
        assert_eq!(names(&tree, &root).unwrap(), vec![b"a".to_vec()]);
        assert_eq!(
            names(&tree, &root.join("a/b")).unwrap(),
            vec![b"inside.txt".to_vec()]
        );
        assert!(
            tree.list(&base.join("outside")).is_err(),
            "not below the root"
        );

        // `a` (above the folder read) and then `b` (the folder read) become
        // links to the outside: neither is followed, with `openat2` or
        // component by component.
        fs::rename(root.join("a"), base.join("a-moved")).unwrap();
        std::os::unix::fs::symlink(base.join("outside"), root.join("a")).unwrap();
        let target = root.join("a/b");
        assert!(tree.list(&target).is_err());
        let rel = target.strip_prefix(&root).unwrap();
        assert!(open_by_components(tree.fd.as_fd(), rel).is_err());
        fs::remove_file(root.join("a")).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        std::os::unix::fs::symlink(base.join("outside/b"), root.join("a/b")).unwrap();
        assert!(tree.list(&target).is_err());
        assert!(open_by_components(tree.fd.as_fd(), rel).is_err());

        // A root that is a link is not opened.
        assert!(Tree::open(&root.join("a/b")).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn components_need_only_search_permission_on_the_way() {
        let base = tree("search");
        let root = base.join("root");
        let tree = Tree::open(&root).unwrap();
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o311)).unwrap();
        let rel = Path::new("a/b");
        let by_parts = open_by_components(tree.fd.as_fd(), rel).map(|_| ());
        let beneath = open_beneath(tree.fd.as_fd(), rel).map(|_| ());
        let unreadable = open_by_components(tree.fd.as_fd(), Path::new("a")).map(|_| ());
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&base).unwrap();
        by_parts.unwrap();
        beneath.unwrap();
        assert_eq!(
            unreadable.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn a_root_that_can_be_read_but_not_searched_is_listed() {
        let base = tree("root-unsearchable");
        let root = base.join("root");
        fs::write(root.join("x"), b"").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o444)).unwrap();
        let tree = Tree::open(&root).unwrap();
        let listed = names(&tree, &root);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        // A root replaced by another folder after it was opened is not read.
        fs::rename(&root, base.join("root-moved")).unwrap();
        fs::create_dir(&root).unwrap();
        let replaced = tree.list(&root).map(|_| ());
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(listed.unwrap(), vec![b"a".to_vec(), b"x".to_vec()]);
        assert!(replaced.is_err());
    }

    #[test]
    fn metadata_matches_the_standard_library() {
        let base = tree("meta");
        let root = base.join("root");
        fs::write(root.join("a/file"), b"12345").unwrap();
        std::os::unix::fs::symlink("b", root.join("a/link")).unwrap();
        let tree = Tree::open(&root).unwrap();
        let mut listing = tree.list(&root.join("a")).unwrap();
        let mut seen = 0;
        while let Some(e) = listing.read() {
            let e = e.unwrap();
            let path = root.join("a").join(e.name_os());
            let m = fs::symlink_metadata(&path).unwrap();
            assert_eq!(listing.is_dir(&e).unwrap(), m.is_dir());
            assert_eq!(listing.meta(&e).unwrap(), Meta::from_metadata(&m));
            seen += 1;
        }
        let m = fs::metadata(root.join("a")).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(listing.identity().unwrap(), (m.dev(), m.ino()));
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(seen, 3);
    }
}
