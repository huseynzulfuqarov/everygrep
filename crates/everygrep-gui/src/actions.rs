//! File actions on result rows: text for the clipboard, rename, trash and
//! permanent delete. The index is not touched here; the watcher sees the
//! changes on disk and updates it.

use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::{Command, Stdio};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, StatxFlags};

use crate::i18n::{fill, tr};

/// Longest file name Linux file systems accept, in bytes.
const NAME_MAX: usize = 255;

/// Full paths, one per line, for the clipboard; with `quote` each one
/// quoted as [`quote`] does.
pub fn paths_text(paths: &[PathBuf], quote: bool) -> String {
    join_lines(paths.iter().map(|p| p.as_os_str()), quote)
}

/// Whether opening `n` items at once asks first: more than `over` of them,
/// unless `over` is 0 (never ask).
pub fn open_needs_confirm(n: usize, over: usize) -> bool {
    over > 0 && n > over
}

/// Characters that a shell still reads inside double quotes: `$` and
/// backticks run commands, `\` and `"` escape, `!` is history expansion in
/// bash and zsh. A line break would split one item over two lines.
const DOUBLE_QUOTE_SPECIAL: &[char] = &['$', '`', '"', '\\', '!', '\n'];

/// One item quoted for pasting into a shell (sh, bash, zsh, fish) or a
/// file dialog. Usually in double quotes, `"/home/me/a b.txt"`; an item with
/// a character a shell reads inside double quotes goes in single quotes
/// instead, with `'` written as `'\''` and `\` as `'\\'`, so the shell takes
/// it as one literal word and never runs a part of it.
pub fn quote(item: &str) -> String {
    if !item.contains(DOUBLE_QUOTE_SPECIAL) {
        return format!("\"{item}\"");
    }
    let mut out = String::with_capacity(item.len() + 2);
    out.push('\'');
    for c in item.chars() {
        match c {
            '\'' => out.push_str("'\\''"),
            // fish reads `\\` and `\'` inside single quotes, so a backslash
            // goes outside them.
            '\\' => out.push_str("'\\\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// File names, one per line, for the clipboard.
pub fn names_text(paths: &[PathBuf]) -> String {
    join_lines(
        paths.iter().map(|p| p.file_name().unwrap_or(p.as_os_str())),
        false,
    )
}

/// The containing folders of `paths`, each once, in the order first met.
pub fn folders(paths: &[PathBuf]) -> Vec<&Path> {
    let mut seen = std::collections::HashSet::new();
    paths
        .iter()
        .filter_map(|p| p.parent())
        .filter(|f| !f.as_os_str().is_empty())
        .filter(|f| seen.insert(*f))
        .collect()
}

/// The containing folders, one per line, for the clipboard. A folder is
/// given once even when several items are in it, in the order first met;
/// with `quote` each one quoted as [`quote`] does.
pub fn folders_text(paths: &[PathBuf], quote: bool) -> String {
    join_lines(folders(paths).into_iter().map(|f| f.as_os_str()), quote)
}

/// Joins with `\n`, each item quoted with `quote`. Bytes that are not UTF-8
/// become U+FFFD; the clipboard takes text only.
fn join_lines<'a>(items: impl Iterator<Item = &'a OsStr>, quote: bool) -> String {
    let mut out = String::new();
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let text = String::from_utf8_lossy(item.as_bytes());
        if quote {
            out.push_str(&self::quote(&text));
        } else {
            out.push_str(&text);
        }
    }
    out
}

/// Length in bytes of the part of a name a rename field preselects: the
/// part before the extension with `name_only`, otherwise the whole name.
pub fn rename_selection(name: &str, is_dir: bool, name_only: bool) -> usize {
    if name_only {
        stem_len(name, is_dir)
    } else {
        name.len()
    }
}

/// Length in bytes of the part of a name before its extension. Folders and
/// names without an extension are taken whole.
pub fn stem_len(name: &str, is_dir: bool) -> usize {
    if is_dir {
        return name.len();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => dot,
        _ => name.len(),
    }
}

/// Why a rename was refused or failed.
#[derive(Debug)]
pub enum RenameError {
    Empty,
    Slash,
    Nul,
    Reserved,
    TooLong,
    Exists(String),
    Io(io::Error),
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenameError::Empty => f.write_str(tr("A name cannot be empty.")),
            RenameError::Slash => f.write_str(tr("A name cannot contain “/”.")),
            RenameError::Nul => f.write_str(tr("A name cannot contain a null character.")),
            RenameError::Reserved => f.write_str(tr("“.” and “..” cannot be used as names.")),
            RenameError::TooLong => f.write_str(tr("The name is too long.")),
            RenameError::Exists(n) => {
                f.write_str(&fill(tr("“{}” already exists in this folder."), &[n]))
            }
            RenameError::Io(e) => f.write_str(&fill(tr("Cannot rename: {}."), &[e])),
        }
    }
}

/// Checks a new name for `old` and returns the new path. `Ok(None)` means
/// the name did not change: also when `old`'s name is not UTF-8 and
/// `new_name` is the name as shown, with U+FFFD for those bytes (renaming
/// to it would replace the bytes nobody edited).
pub fn rename_target(old: &Path, new_name: &str) -> Result<Option<PathBuf>, RenameError> {
    // First: a lossy name can be longer than the limit.
    if let Some(name) = old.file_name() {
        let name = name.as_bytes();
        if name == new_name.as_bytes()
            || (std::str::from_utf8(name).is_err() && String::from_utf8_lossy(name) == new_name)
        {
            return Ok(None);
        }
    }
    validate_name(new_name)?;
    let target = old.with_file_name(new_name);
    if std::fs::symlink_metadata(&target).is_ok() && !case_only(old, &target) {
        return Err(RenameError::Exists(new_name.to_owned()));
    }
    Ok(Some(target))
}

/// Linux name syntax, without touching the filesystem.
pub fn validate_name(new_name: &str) -> Result<(), RenameError> {
    if new_name.is_empty() {
        return Err(RenameError::Empty);
    }
    if new_name.contains('/') {
        return Err(RenameError::Slash);
    }
    if new_name.contains('\0') {
        return Err(RenameError::Nul);
    }
    if new_name == "." || new_name == ".." {
        return Err(RenameError::Reserved);
    }
    if new_name.len() > NAME_MAX {
        return Err(RenameError::TooLong);
    }
    Ok(())
}

/// Whether renaming `old` to `target` only changes the case of its name on a
/// case-insensitive file system: `target` finds the item itself, and the
/// folder has no entry of exactly that name. A hard link under another
/// name, also one that differs only in case, is the same item too, but a
/// clash.
pub(crate) fn case_only(old: &Path, target: &Path) -> bool {
    if old.parent() != target.parent() {
        return false;
    }
    let (Some(a), Some(b)) = (old.file_name(), target.file_name()) else {
        return false;
    };
    a != b
        && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        && std::fs::symlink_metadata(old)
            .and_then(|m| Ok((m, std::fs::symlink_metadata(target)?)))
            .is_ok_and(|(m, t)| m.dev() == t.dev() && m.ino() == t.ino())
        && !has_entry(target)
}

/// Whether the folder of `path` lists an entry named exactly (byte for
/// byte) like it. An unreadable folder counts as having it.
pub(crate) fn has_entry(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return true;
    };
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|e| e.file_name() == name),
        Err(_) => true,
    }
}

/// Renames `old` to `new_name` in the same folder. Returns the new path, or
/// `None` if the name did not change.
///
/// Every publication refuses replacement, including a change of case on a
/// case-insensitive filesystem, which first vacates the old spelling.
pub fn rename(old: &Path, new_name: &str) -> Result<Option<PathBuf>, RenameError> {
    let Some(target) = rename_target(old, new_name)? else {
        return Ok(None);
    };
    if case_only(old, &target) {
        return rename_case(old, &target, new_name).map(Some);
    }
    crate::transfer::rename_noreplace(old, &target).map_err(|e| rename_error(e, new_name))?;
    Ok(Some(target))
}

fn rename_error(e: io::Error, new_name: &str) -> RenameError {
    if e.kind() == io::ErrorKind::AlreadyExists {
        RenameError::Exists(new_name.to_owned())
    } else if e.raw_os_error() == Some(36) {
        RenameError::TooLong
    } else {
        RenameError::Io(e)
    }
}

/// Changes only the case of `old`'s name to `target` on a case-insensitive
/// file system. A plain rename there finds the item itself as the target
/// and may change nothing while reporting success. So the item first gets
/// a free hidden name in its folder, then the new spelling; neither step
/// replaces anything. Afterwards the folder must list exactly the new
/// name, or the rename is reported as failed.
fn rename_case(old: &Path, target: &Path, new_name: &str) -> Result<PathBuf, RenameError> {
    rename_case_only(old, target, crate::transfer::rename_noreplace)
        .map_err(|e| rename_error(e, new_name))?;
    kept_new_case(target)
}

fn rename_case_only(
    old: &Path,
    target: &Path,
    publish: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = old.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let staged = crate::transfer::stage_rename(old, parent)?;
    if let Err(error) = publish(&staged, target) {
        if let Err(restore) = crate::transfer::rename_noreplace(&staged, old) {
            return Err(io::Error::other(fill(
                tr("the original name could not be restored; the item is now at {0}: {1}"),
                &[&staged.display(), &restore],
            )));
        }
        return Err(error);
    }
    Ok(())
}

/// `target` when its folder lists exactly that name after a change of
/// case; otherwise the file system did not take the new spelling.
fn kept_new_case(target: &Path) -> Result<PathBuf, RenameError> {
    if has_entry(target) {
        Ok(target.to_path_buf())
    } else {
        Err(RenameError::Io(io::Error::other(tr(
            "the file system kept the name in its old case",
        ))))
    }
}

/// The already-isolated entry is published to the freedesktop Trash without
/// a child process resolving a mutable source pathname.
pub fn trash(
    actual: &Path,
    original: &Path,
    stamp: crate::transfer::FileStamp,
) -> Result<(), String> {
    crate::trashbin::put_from(actual, original, stamp)
}

/// Which item a path named when the user chose it for an action: device,
/// inode and type of the entry itself (a link, not what it points to), and
/// for a regular file its size, modification time and change time. The
/// change time is set by the kernel on every write, rename or permission
/// change and cannot be set back from user space, so a rewrite that keeps
/// the size and the modification time is still seen (on file systems with
/// fine-grained times).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    dev: u64,
    ino: u64,
    kind: u32,
    /// Size, modification time and change time (seconds, nanoseconds) of a
    /// regular file.
    content: Option<(u64, Time, Time)>,
}

/// A file time: seconds and nanoseconds.
type Time = (i64, i64);

impl Identity {
    pub(crate) fn of(m: &std::fs::Metadata) -> Self {
        let kind = m.mode() & S_IFMT;
        Self {
            dev: m.dev(),
            ino: m.ino(),
            kind,
            content: (kind == S_IFREG).then(|| {
                (
                    m.size(),
                    (m.mtime(), m.mtime_nsec()),
                    (m.ctime(), m.ctime_nsec()),
                )
            }),
        }
    }

    /// The same entry: device, inode and type; the content may differ.
    fn same_item(&self, other: &Self) -> bool {
        (self.dev, self.ino, self.kind) == (other.dev, other.ino, other.kind)
    }
}

/// `S_IFMT`: the type bits of a mode; `S_IFREG`: a regular file.
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;

/// The items the user chose for an action, as they were then (when the
/// question or dialog opened, the drag started, or Cut or Copy was used).
/// Checked again right before each item is acted on, so another item put at
/// one of the paths meanwhile (a save by an editor, a sync) is never
/// touched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chosen(Vec<Identity>);

impl Chosen {
    /// The items at `paths` now; items that cannot be read are left out
    /// (nothing is done with them later).
    pub fn of(paths: &[PathBuf]) -> Self {
        Self(
            paths
                .iter()
                .filter_map(|p| std::fs::symlink_metadata(p).ok())
                .map(|m| Identity::of(&m))
                .collect(),
        )
    }

    /// Whether `m` is one of the chosen items; with `unchanged` also with
    /// the size, modification time and change time it had (a regular
    /// file).
    pub(crate) fn holds(&self, m: &std::fs::Metadata, unchanged: bool) -> bool {
        let now = Identity::of(m);
        self.0.iter().any(|c| {
            if unchanged {
                *c == now
            } else {
                c.same_item(&now)
            }
        })
    }

    /// [`holds`](Self::holds) as a result: the error says the item changed.
    pub(crate) fn check(&self, m: &std::fs::Metadata, unchanged: bool) -> io::Result<()> {
        if self.holds(m, unchanged) {
            Ok(())
        } else {
            Err(changed_since_chosen())
        }
    }
}

/// The error for an item that is not the one the user chose any more.
pub(crate) fn changed_since_chosen() -> io::Error {
    io::Error::other(tr("it changed after you chose it; nothing was done"))
}

/// Deletes `paths` permanently: folders with all their contents, links as
/// links (never what they point to). Only the items in `confirmed`, as they
/// were then, are deleted: an item replaced at its path after the user
/// confirmed (a save by an editor, a sync), or a file written since, is
/// kept and reported. Each item is looked at through its open folder right
/// before it is removed. A folder that is, or holds, another file system (a
/// mounted drive or share, a bind mount, a btrfs subvolume) is not deleted
/// at all: see [`remove_tree`]. Returns the paths that failed.
pub fn delete_permanently(paths: &[PathBuf], confirmed: &Chosen) -> Vec<(PathBuf, io::Error)> {
    let mut failed = Vec::new();
    for path in paths {
        if let Err(e) = delete_one(path, confirmed) {
            failed.push((path.clone(), e));
        }
    }
    failed
}

/// The folder of `path`, opened, and the name in it. Opened as a handle
/// only (`O_PATH`): entries are looked at, opened and removed through it by
/// name, so a folder that may be written to but not listed works.
fn open_parent(path: &Path) -> io::Result<(OwnedFd, CString)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let fd = rustix::fs::open(
        parent,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok((fd, CString::new(name.as_bytes())?))
}

fn delete_one(path: &Path, confirmed: &Chosen) -> io::Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    confirmed.check(&m, true)?;
    if m.is_dir() {
        return remove_tree(path, (m.dev(), m.ino()));
    }
    // Looked at again through the open folder, so the entry removed is the
    // one checked even when a folder above was renamed meanwhile.
    let (parent, name) = open_parent(path)?;
    let st = rustix::fs::statat(&parent, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)?;
    let content = (st.st_mode & S_IFMT == S_IFREG).then_some((
        st.st_size as u64,
        (st.st_mtime, st.st_mtime_nsec as i64),
        (st.st_ctime, st.st_ctime_nsec as i64),
    ));
    let now = Identity {
        dev: st.st_dev,
        ino: st.st_ino,
        kind: st.st_mode & S_IFMT,
        content,
    };
    if now != Identity::of(&m) {
        return Err(changed_since_chosen());
    }
    Ok(rustix::fs::unlinkat(
        &parent,
        name.as_c_str(),
        AtFlags::empty(),
    )?)
}

/// Where a folder's entries live: its device and the mount it was reached
/// through. Two folders of one tree with different places are on different
/// file systems, or one is mounted over (a bind mount of the same file
/// system has the same device but another mount).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    dev: (u32, u32),
    /// `None` where the kernel does not report it (before Linux 5.8); the
    /// device alone is compared then.
    mount: Option<u64>,
}

/// The [`Place`] of the open folder `dir`.
fn place_of(dir: BorrowedFd<'_>) -> io::Result<Place> {
    let st = rustix::fs::statx(
        dir,
        c"",
        AtFlags::EMPTY_PATH,
        StatxFlags::TYPE | StatxFlags::MNT_ID,
    )?;
    Ok(Place {
        dev: (st.stx_dev_major, st.stx_dev_minor),
        mount: (st.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(st.stx_mnt_id),
    })
}

/// The error for a folder that is on another file system than the folder
/// being deleted.
fn other_file_system(path: &Path) -> io::Error {
    io::Error::other(fill(
        tr("{} is another drive or file system (a mount point), so nothing was deleted from it"),
        &[&path.display()],
    ))
}

/// Opens the folder `name` in `dir` for reading, refusing a symbolic link.
fn open_subdir(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Deletes the folder `path` with all it holds, on its own file system
/// only. Folders are opened by descriptor, one below the other, never by
/// following a symbolic link, so a folder swapped for a link meanwhile is
/// not followed. First the whole tree is checked: when the folder is itself
/// a mount point, or anything below it is on another file system or mount,
/// nothing is deleted. While deleting, every folder is checked again before
/// its contents are touched.
/// `expected` is the folder's device and inode as checked before: a
/// folder put at `path` in between is not touched.
pub(crate) fn remove_tree(path: &Path, expected: (u64, u64)) -> io::Result<()> {
    remove_tree_with(path, expected, &place_of)
}

/// [`remove_tree`] with the place of a folder passed in, so a mount can be
/// simulated in tests.
fn remove_tree_with(
    path: &Path,
    expected: (u64, u64),
    place: &dyn Fn(BorrowedFd<'_>) -> io::Result<Place>,
) -> io::Result<()> {
    let (parent_fd, name) = open_parent(path)?;
    let root = open_subdir(parent_fd.as_fd(), &name)?;
    let st = rustix::fs::fstat(&root)?;
    if (st.st_dev, st.st_ino) != expected {
        return Err(changed_since_chosen());
    }
    let here = place(root.as_fd())?;
    if here != place(parent_fd.as_fd())? {
        return Err(other_file_system(path));
    }
    // A first pass only looks, so a mount deep inside stops the delete
    // before anything is gone.
    let mut root = Dir::new(root)?;
    walk(&mut root, path, here, place, false)?;
    root.rewind();
    walk(&mut root, path, here, place, true)?;
    Ok(rustix::fs::unlinkat(
        parent_fd.as_fd(),
        name.as_c_str(),
        AtFlags::REMOVEDIR,
    )?)
}

/// Goes through the open folder `dir` (at `path`, on `here`) and the
/// folders below it; with `remove`, deletes their entries. A folder below
/// on another place is an error.
fn walk(
    entries: &mut Dir,
    path: &Path,
    here: Place,
    place: &dyn Fn(BorrowedFd<'_>) -> io::Result<Place>,
    remove: bool,
) -> io::Result<()> {
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let fd = entries.fd()?;
        let is_dir = match entry.file_type() {
            FileType::Directory => true,
            FileType::Unknown => FileType::from_raw_mode(
                rustix::fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode,
            )
            .is_dir(),
            _ => false,
        };
        let full = path.join(OsStr::from_bytes(name.to_bytes()));
        if is_dir {
            let sub = open_subdir(fd, name)?;
            if place(sub.as_fd())? != here {
                return Err(other_file_system(&full));
            }
            walk(&mut Dir::new(sub)?, &full, here, place, remove)?;
            if remove {
                rustix::fs::unlinkat(fd, name, AtFlags::REMOVEDIR)?;
            }
        } else if remove {
            rustix::fs::unlinkat(fd, name, AtFlags::empty())?;
        }
    }
    Ok(())
}

/// "“name”" for one item, "N items" for more.
pub fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!(
            "“{}”",
            crate::format::visible_name(&String::from_utf8_lossy(
                one.file_name().unwrap_or(one.as_os_str()).as_bytes()
            ))
        ),
        many => crate::labels::items(many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh empty folder under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("everygrep-gui-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn clipboard_text_is_one_item_per_line() {
        let paths = [PathBuf::from("/a/b c.txt"), PathBuf::from("/d/e")];
        assert_eq!(paths_text(&paths, false), "/a/b c.txt\n/d/e");
        assert_eq!(names_text(&paths), "b c.txt\ne");
        assert_eq!(paths_text(&paths[..1], false), "/a/b c.txt");
        assert_eq!(paths_text(&[], false), "");
        let odd = [PathBuf::from(OsStr::from_bytes(b"/x/\xffy"))];
        assert_eq!(names_text(&odd), "\u{fffd}y");
    }

    #[test]
    fn copy_path_gives_each_containing_folder_once() {
        let paths = [
            PathBuf::from("/a/b c/x.txt"),
            PathBuf::from("/d/e"),
            PathBuf::from("/a/b c/y"),
            PathBuf::from("/"),
        ];
        assert_eq!(folders_text(&paths, false), "/a/b c\n/d");
        assert_eq!(folders_text(&paths[1..2], false), "/d");
        assert_eq!(folders_text(&[PathBuf::from("/top")], false), "/");
        assert_eq!(folders_text(&[], false), "");
        assert_eq!(folders(&paths).len(), 2);
    }

    #[test]
    fn stem_excludes_the_extension() {
        assert_eq!(stem_len("report.pdf", false), 6);
        assert_eq!(stem_len("a.tar.gz", false), 5);
        assert_eq!(stem_len(".bashrc", false), 7);
        assert_eq!(stem_len("notes", false), 5);
        assert_eq!(stem_len("photos.2026", true), 11);
        assert_eq!(stem_len("şəkil.png", false), "şəkil".len());
    }

    #[test]
    fn opening_many_items_asks_above_the_threshold() {
        assert!(!open_needs_confirm(1, 32));
        assert!(!open_needs_confirm(32, 32));
        assert!(open_needs_confirm(33, 32));
        assert!(open_needs_confirm(2, 1));
        assert!(!open_needs_confirm(10_000, 0), "0 never asks");
    }

    #[test]
    fn quoting_wraps_every_item() {
        assert_eq!(quote("/a b/c.txt"), "\"/a b/c.txt\"");
        assert_eq!(quote("/a/it's"), "\"/a/it's\"");
        assert_eq!(quote("/a/$x"), "'/a/$x'");
        assert_eq!(quote("/a/it's $x"), "'/a/it'\\''s $x'");
        assert_eq!(quote("/a\\b"), "'/a'\\\\'b'");
        assert_eq!(quote("/a\nb"), "'/a\nb'");
        let paths = [
            PathBuf::from("/x/one.txt"),
            PathBuf::from("/y/two words.pdf"),
        ];
        assert_eq!(
            paths_text(&paths, true),
            "\"/x/one.txt\"\n\"/y/two words.pdf\""
        );
        assert_eq!(folders_text(&paths, true), "\"/x\"\n\"/y\"");
    }

    /// What `sh` (and bash, zsh, fish when installed) make of one quoted
    /// path given to `printf`: the words it gets, NUL-separated.
    fn shell_words(quoted: &str) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for shell in ["sh", "bash", "zsh", "fish"] {
            let script = format!("printf '%s\\0' {quoted}");
            let run = Command::new(shell)
                .arg("-c")
                .arg(&script)
                .env("HOME", "/nonexistent")
                .stdin(Stdio::null())
                .output();
            if let Ok(o) = run {
                out.push((shell.to_owned(), o.stdout));
            }
        }
        out
    }

    #[test]
    fn quoted_copies_are_taken_literally_by_shells() {
        let names = [
            "/x/$(echo PWNED)",
            "/x/`echo PWNED`",
            "/x/$HOME",
            "/x/a\"b",
            "/x/it's",
            "/x/back\\slash\\",
            "/x/a\\'b",
            "/x/!!",
            "/x/two\nlines",
            "/x/plain name.txt",
        ];
        for name in names {
            let text = paths_text(&[PathBuf::from(name)], true);
            let got = shell_words(&text);
            assert!(got.iter().any(|(s, _)| s == "sh"), "sh runs");
            for (shell, words) in got {
                assert_eq!(
                    String::from_utf8_lossy(&words),
                    format!("{name}\0"),
                    "{shell} given {text}"
                );
            }
        }
    }

    #[test]
    fn rename_selects_the_name_or_all_of_it() {
        assert_eq!(rename_selection("report.pdf", false, true), 6);
        assert_eq!(rename_selection("report.pdf", false, false), 10);
        assert_eq!(rename_selection("photos.2026", true, true), 11);
        assert_eq!(rename_selection(".bashrc", false, false), 7);
    }

    #[test]
    fn invalid_names_are_refused() {
        let old = Path::new("/nonexistent-dir/x.txt");
        assert!(matches!(rename_target(old, ""), Err(RenameError::Empty)));
        assert!(matches!(rename_target(old, "a/b"), Err(RenameError::Slash)));
        assert!(matches!(rename_target(old, "a\0b"), Err(RenameError::Nul)));
        assert!(matches!(
            rename_target(old, ".."),
            Err(RenameError::Reserved)
        ));
        let long = "x".repeat(256);
        assert!(matches!(
            rename_target(old, &long),
            Err(RenameError::TooLong)
        ));
        assert!(matches!(rename_target(old, "x.txt"), Ok(None)));
        assert_eq!(
            rename_target(old, "y.txt").unwrap(),
            Some(PathBuf::from("/nonexistent-dir/y.txt"))
        );
    }

    #[test]
    fn rename_refuses_to_replace_and_renames_otherwise() {
        let dir = TempDir::new("rename");
        let a = dir.0.join("a.txt");
        let b = dir.0.join("b.txt");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let err = rename(&a, "b.txt").unwrap_err();
        assert!(matches!(err, RenameError::Exists(_)));
        assert_eq!(err.to_string(), "“b.txt” already exists in this folder.");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b");

        let c = rename(&a, "c.txt").unwrap().unwrap();
        assert_eq!(c, dir.0.join("c.txt"));
        assert!(!a.exists());
        assert_eq!(fs::read_to_string(&c).unwrap(), "a");
        assert!(matches!(rename(&a, "d.txt"), Err(RenameError::Io(_))));
    }

    #[test]
    fn accepting_the_shown_name_of_a_name_that_is_not_utf8_changes_nothing() {
        let dir = TempDir::new("rename-lossy");
        let old = dir.0.join(OsStr::from_bytes(b"a\xff.txt"));
        fs::write(&old, "x").unwrap();
        // The editors show (and give back) the name with U+FFFD.
        let shown = String::from_utf8_lossy(old.file_name().unwrap().as_bytes()).into_owned();
        assert_eq!(shown, "a\u{fffd}.txt");
        assert!(matches!(rename_target(&old, &shown), Ok(None)));
        assert!(matches!(rename(&old, &shown), Ok(None)));
        assert_eq!(fs::read_to_string(&old).unwrap(), "x");
        assert!(!dir.0.join(&shown).exists());
        // Also when the shown name is longer than the limit in bytes.
        let long = dir.0.join(OsStr::from_bytes(&[0xff; 200]));
        fs::write(&long, "y").unwrap();
        let shown = "\u{fffd}".repeat(200);
        assert!(matches!(rename(&long, &shown), Ok(None)));
        assert_eq!(fs::read_to_string(&long).unwrap(), "y");
        // A real new name still renames it.
        let new = rename(&old, "b.txt").unwrap().unwrap();
        assert_eq!(fs::read_to_string(new).unwrap(), "x");
    }

    #[test]
    fn another_link_to_the_file_clashes_even_under_another_case() {
        let dir = TempDir::new("rename-case");
        let (x, upper) = (dir.0.join("x.txt"), dir.0.join("X.txt"));
        fs::write(&x, "x").unwrap();
        // A hard link under the other case: on a case-sensitive file system
        // "X.txt" is an entry of its own, not "x.txt" found again, so it is
        // taken (a rename onto it would change nothing and say "renamed").
        fs::hard_link(&x, &upper).unwrap();
        assert!(matches!(
            rename_target(&x, "X.txt"),
            Err(RenameError::Exists(_))
        ));
        assert!(matches!(rename(&x, "X.txt"), Err(RenameError::Exists(_))));
        assert!(x.exists() && upper.exists());
        assert!(has_entry(&upper) && !has_entry(&dir.0.join("x.TXT")));
        // The same file under an unrelated name is a different entry.
        let other = dir.0.join("other.txt");
        fs::hard_link(&upper, &other).unwrap();
        assert!(matches!(
            rename_target(&upper, "other.txt"),
            Err(RenameError::Exists(_))
        ));
        assert!(matches!(
            rename(&upper, "other.txt"),
            Err(RenameError::Exists(_))
        ));
        assert!(upper.exists() && other.exists());
    }

    #[test]
    fn case_only_rename_refuses_unsupported_staging_without_changing_either_item() {
        let dir = TempDir::new("case-unsupported");
        let old = dir.0.join("Notes.txt");
        let target = dir.0.join("NOTES.txt");
        fs::write(&old, "selected").unwrap();
        for errno in [22, 38, 95] {
            for occupied in [false, true] {
                if occupied {
                    fs::write(&target, "unrelated").unwrap();
                }
                crate::transfer::RENAME_ERRNO.with(|value| value.set(Some(errno)));
                let result = rename_case(&old, &target, "NOTES.txt");
                crate::transfer::RENAME_ERRNO.with(|value| value.set(None));
                let Err(RenameError::Io(error)) = result else {
                    panic!("unsupported staging must refuse the rename");
                };
                assert_eq!(error.kind(), io::ErrorKind::Unsupported);
                assert!(
                    error
                        .to_string()
                        .contains("cannot rename without replacement")
                );
                assert_eq!(fs::read_to_string(&old).unwrap(), "selected");
                if occupied {
                    assert_eq!(fs::read_to_string(&target).unwrap(), "unrelated");
                    fs::remove_file(&target).unwrap();
                } else {
                    assert!(!target.exists());
                }
                assert_eq!(names(&dir.0), ["Notes.txt"]);
            }
        }
    }

    #[test]
    fn case_only_stages_before_publication_and_preserves_a_racing_target() {
        let dir = TempDir::new("case-stage");
        let old = dir.0.join("a.txt");
        let target = dir.0.join("A.txt");
        fs::write(&old, "selected").unwrap();
        rename_case_only(&old, &target, |from, to| {
            assert!(
                !from
                    .file_name()
                    .unwrap()
                    .eq_ignore_ascii_case(to.file_name().unwrap())
            );
            crate::transfer::rename_noreplace(from, to)
        })
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"selected");
        fs::rename(&target, &old).unwrap();
        let error = rename_case_only(&old, &target, |from, to| {
            fs::write(to, "unique racing target").unwrap();
            crate::transfer::rename_noreplace(from, to)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&old).unwrap(), b"selected");
        assert_eq!(fs::read(&target).unwrap(), b"unique racing target");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    #[test]
    fn rename_counts_utf8_bytes_and_preserves_a_dangling_target() {
        let dir = TempDir::new("rename-unicode");
        let old = dir.0.join("old");
        fs::write(&old, "keep").unwrap();
        assert!(matches!(
            rename(&old, &"ə".repeat(128)),
            Err(RenameError::TooLong)
        ));
        assert_eq!(fs::read_to_string(&old).unwrap(), "keep");
        let link = dir.0.join("taken");
        std::os::unix::fs::symlink(dir.0.join("missing"), &link).unwrap();
        assert!(matches!(rename(&old, "taken"), Err(RenameError::Exists(_))));
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        let renamed = rename(&old, &"ə".repeat(127)).unwrap().unwrap();
        assert_eq!(fs::read_to_string(renamed).unwrap(), "keep");
    }

    #[test]
    fn native_trash_preserves_option_like_and_non_utf8_original_names() {
        let dir = TempDir::new("trash-names");
        crate::trashbin::set_test_home(Some(dir.0.join("Trash")));
        let path = dir.0.join(OsStr::from_bytes(b"-option\xff\n%"));
        fs::write(&path, "bytes").unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        trash(&path, &path, crate::transfer::FileStamp::of(&meta)).unwrap();
        assert!(!path.exists());
        crate::trashbin::restore(&path, crate::trashbin::Identity::of(&meta)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"bytes");
        crate::trashbin::set_test_home(None);
    }

    #[test]
    fn a_failed_trash_batch_reports_each_kept_item_and_the_rest_goes_on() {
        let _lock = crate::fileops_undo::tests_lock();
        let dir = TempDir::new("trash-batches");
        let bin = dir.0.join("Trash");
        crate::trashbin::set_test_home(Some(bin.clone()));
        // Continue beyond the former command batch boundary after failures.
        let paths: Vec<PathBuf> = (0..256 + 5)
            .map(|i| {
                let p = dir.0.join(format!("f{i}"));
                fs::write(&p, "x").unwrap();
                p
            })
            .collect();
        let stuck = [paths[3].clone(), paths[7].clone()];
        let attempts = std::cell::Cell::new(0);
        crate::fileops_undo::with_undo_on(|| {
            let out = crate::fileops_undo::trash(&paths, &|actual, original, stamp| {
                attempts.set(attempts.get() + 1);
                if stuck.contains(&original.to_path_buf()) {
                    Err("Cannot move to the trash: no permission".into())
                } else {
                    crate::trashbin::test_send(actual, original, stamp)
                }
            });
            assert_eq!(attempts.get(), paths.len(), "all later items were tried");
            assert_eq!(
                out.failed
                    .iter()
                    .map(|(p, _)| p.clone())
                    .collect::<Vec<_>>(),
                stuck
            );
            assert!(out.failed.iter().all(|(_, e)| e.contains("no permission")));
            assert_eq!(
                out.trashed,
                paths
                    .iter()
                    .filter(|p| !stuck.contains(p))
                    .cloned()
                    .collect::<Vec<_>>()
            );
            for path in &paths {
                assert_eq!(path.exists(), stuck.contains(path));
            }
            for path in &stuck {
                assert_eq!(fs::read_to_string(path).unwrap(), "x");
            }
            assert_eq!(
                fs::read_dir(bin.join("files")).unwrap().count(),
                paths.len() - 2
            );
            let empty = crate::fileops_undo::trash(&[], &|_, _, _| unreachable!());
            assert!(empty.trashed.is_empty() && empty.failed.is_empty() && empty.note.is_none());
        });
        crate::trashbin::set_test_home(None);
    }

    #[test]
    fn delete_removes_files_and_folders_but_not_link_targets() {
        let dir = TempDir::new("delete");
        let keep = dir.0.join("keep");
        fs::create_dir(&keep).unwrap();
        fs::write(keep.join("inside"), "x").unwrap();
        let file = dir.0.join("f.txt");
        fs::write(&file, "x").unwrap();
        let tree = dir.0.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/g"), "x").unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&keep, &link).unwrap();

        let missing = dir.0.join("missing");
        let all = [file.clone(), tree.clone(), link.clone(), missing.clone()];
        let failed = delete_permanently(&all, &Chosen::of(&all));
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, missing);
        assert!(!file.exists() && !tree.exists());
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(keep.join("inside").is_file());
    }

    /// A [`Place`] that says the folder with inode `mounted` is on another
    /// device, as if a drive were mounted there.
    fn fake_mount(mounted: u64) -> impl Fn(BorrowedFd<'_>) -> io::Result<Place> {
        move |fd| {
            let real = place_of(fd)?;
            Ok(if rustix::fs::fstat(fd)?.st_ino == mounted {
                Place {
                    dev: (real.dev.0, real.dev.1 + 1),
                    ..real
                }
            } else {
                real
            })
        }
    }

    #[test]
    fn delete_never_crosses_into_another_file_system() {
        let dir = TempDir::new("delete-mount");
        let tree = dir.0.join("tree");
        fs::create_dir_all(tree.join("a/mnt/deep")).unwrap();
        fs::write(tree.join("first.txt"), "x").unwrap();
        fs::write(tree.join("a/second.txt"), "x").unwrap();
        fs::write(tree.join("a/mnt/data.txt"), "precious").unwrap();
        let mnt = fs::metadata(tree.join("a/mnt")).unwrap().ino();
        let m = fs::metadata(&tree).unwrap();
        let tree_id = (m.dev(), m.ino());
        let err = remove_tree_with(&tree, tree_id, &fake_mount(mnt)).unwrap_err();
        assert!(err.to_string().contains("a/mnt"), "{err}");
        assert!(err.to_string().contains("nothing was deleted"), "{err}");
        // Checked before anything was removed: all of it is still there.
        assert_eq!(
            fs::read_to_string(tree.join("a/mnt/data.txt")).unwrap(),
            "precious"
        );
        assert!(tree.join("first.txt").exists() && tree.join("a/second.txt").exists());
        assert!(tree.join("a/mnt/deep").is_dir());

        // The folder itself on another device than its parent: refused.
        let top = fs::metadata(&tree).unwrap().ino();
        assert!(remove_tree_with(&tree, tree_id, &fake_mount(top)).is_err());
        assert!(tree.join("first.txt").exists());

        // On one file system the whole tree goes.
        remove_tree(&tree, tree_id).unwrap();
        assert!(fs::symlink_metadata(&tree).is_err());
    }

    #[test]
    fn delete_does_not_follow_links_inside_a_folder() {
        let dir = TempDir::new("delete-links");
        let outside = dir.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "x").unwrap();
        let tree = dir.0.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("sub/to-outside")).unwrap();
        std::os::unix::fs::symlink(outside.join("keep.txt"), tree.join("file-link")).unwrap();
        assert!(
            delete_permanently(
                std::slice::from_ref(&tree),
                &Chosen::of(std::slice::from_ref(&tree))
            )
            .is_empty()
        );
        assert!(fs::symlink_metadata(&tree).is_err());
        assert!(outside.join("keep.txt").is_file());
    }

    #[test]
    fn delete_stops_at_a_real_mount() {
        if let Some(base) = crate::mount_ns::inside() {
            // In the namespace: base/victim/inner is a tmpfs of its own.
            let victim = base.join("victim");
            let inner = victim.join("inner");
            assert_ne!(
                fs::metadata(&inner).unwrap().dev(),
                fs::metadata(&victim).unwrap().dev(),
                "inner is a mount"
            );
            let one = std::slice::from_ref(&victim);
            let failed = delete_permanently(one, &Chosen::of(one));
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert!(failed[0].1.to_string().contains("inner"), "{failed:?}");
            assert_eq!(
                fs::read_to_string(inner.join("data.txt")).unwrap(),
                "precious"
            );
            assert!(victim.join("own.txt").exists());
            // Picking the mount point itself is refused too.
            let one = std::slice::from_ref(&inner);
            let failed = delete_permanently(one, &Chosen::of(one));
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert!(inner.join("data.txt").exists());
            return;
        }
        let dir = TempDir::new("delete-real-mount");
        fs::create_dir_all(dir.0.join("victim/inner")).unwrap();
        fs::write(dir.0.join("victim/own.txt"), "x").unwrap();
        crate::mount_ns::run(
            "actions::tests::delete_stops_at_a_real_mount",
            &dir.0,
            "mount -t tmpfs tmpfs \"$1/victim/inner\" || exit 77
            printf precious > \"$1/victim/inner/data.txt\" || exit 1",
        );
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn case_only_rename_on_a_case_insensitive_folder_changes_the_name() {
        if let Some(base) = crate::mount_ns::inside() {
            let d = base.join("cf/d");
            let abc = d.join("Abc");
            fs::write(&abc, "abc").unwrap();
            assert!(
                fs::symlink_metadata(d.join("ABC")).is_ok(),
                "case-insensitive"
            );
            let new = match rename(&abc, "ABC") {
                Ok(Some(new)) => new,
                Err(RenameError::Io(e)) if e.kind() == io::ErrorKind::Unsupported => {
                    assert!(e.to_string().contains("cannot rename without replacement"));
                    assert_eq!(names(&d), ["Abc"]);
                    assert_eq!(fs::read_to_string(&abc).unwrap(), "abc");
                    return;
                }
                other => panic!("unexpected rename result: {other:?}"),
            };
            assert_eq!(new, d.join("ABC"));
            assert_eq!(names(&d), ["ABC"]);
            assert_eq!(fs::read_to_string(&new).unwrap(), "abc");
            // A folder with contents, and recorded for undo.
            let _lock = crate::fileops_undo::tests_lock();
            crate::fileops_undo::with_undo_on(|| {
                fs::create_dir(d.join("Pics")).unwrap();
                fs::write(d.join("Pics/x"), "x").unwrap();
                assert!(crate::fileops_undo::rename(&d.join("Pics"), "PICS").is_ok());
                assert_eq!(names(&d), ["ABC", "PICS"]);
                assert_eq!(crate::fileops_undo::step_for_tests(false), (1, 0));
                assert_eq!(names(&d), ["ABC", "Pics"]);
            });
            assert_eq!(fs::read_to_string(d.join("Pics/x")).unwrap(), "x");
            return;
        }
        let dir = TempDir::new("rename-casefold");
        crate::mount_ns::run(
            "actions::tests::case_only_rename_on_a_case_insensitive_folder_changes_the_name",
            &dir.0,
            crate::mount_ns::CASEFOLD_SETUP,
        );
    }

    #[test]
    fn a_case_rename_through_a_hidden_name_keeps_the_item() {
        // On a case-sensitive folder the two steps work too: the item ends
        // under the new spelling, and nothing is left behind.
        let dir = TempDir::new("rename-case-steps");
        let old = dir.0.join("Notes.txt");
        fs::write(&old, "n").unwrap();
        let target = dir.0.join("NOTES.txt");
        assert_eq!(rename_case(&old, &target, "NOTES.txt").unwrap(), target);
        assert_eq!(names(&dir.0), ["NOTES.txt"]);
        assert_eq!(fs::read_to_string(&target).unwrap(), "n");
        // A target that appears meanwhile is never replaced: the item gets
        // its old name back.
        let other = dir.0.join("notes.txt");
        fs::write(&other, "other").unwrap();
        assert!(matches!(
            rename_case(&target, &other, "notes.txt"),
            Err(RenameError::Exists(_))
        ));
        assert_eq!(names(&dir.0), ["NOTES.txt", "notes.txt"]);
        assert_eq!(fs::read_to_string(&other).unwrap(), "other");
        assert_eq!(fs::read_to_string(&target).unwrap(), "n");
    }

    #[test]
    fn a_confirmed_delete_keeps_an_item_replaced_meanwhile() {
        let dir = TempDir::new("delete-identity");
        let file = dir.0.join("doc.txt");
        fs::write(&file, "old").unwrap();
        let folder = dir.0.join("tree");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a"), "a").unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let asked = [file.clone(), folder.clone(), link.clone()];
        let confirmed = Chosen::of(&asked);
        // While the question is open, an editor saves by writing a new
        // file and renaming it over the old one, and a sync puts another
        // folder in place of the old one.
        let temp = dir.0.join(".doc.txt.new");
        fs::write(&temp, "new").unwrap();
        fs::rename(&temp, &file).unwrap();
        let old_folder = dir.0.join("tree-old");
        fs::rename(&folder, &old_folder).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("b"), "b").unwrap();

        let failed = delete_permanently(&asked, &confirmed);
        let paths: Vec<_> = failed.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(paths, [file.clone(), folder.clone()]);
        assert!(
            failed[0].1.to_string().contains("nothing was done"),
            "{failed:?}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert!(folder.join("b").is_file());
        // The link did not change: deleted, its target kept.
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(old_folder.join("a").is_file());

        // The folder checked before is not the one opened: refused too.
        let m = fs::metadata(&old_folder).unwrap();
        assert!(remove_tree(&folder, (m.dev(), m.ino())).is_err());
        assert!(folder.join("b").is_file());
    }

    #[test]
    fn a_confirmed_folder_delete_keeps_a_folder_or_link_put_in_its_place() {
        let dir = TempDir::new("delete-folder-swap");
        let outside = dir.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "keep").unwrap();
        let by_link = dir.0.join("by-link");
        let by_folder = dir.0.join("by-folder");
        for f in [&by_link, &by_folder] {
            fs::create_dir(f).unwrap();
            fs::write(f.join("old.txt"), "old").unwrap();
        }
        let asked = [by_link.clone(), by_folder.clone()];
        let confirmed = Chosen::of(&asked);
        fs::rename(&by_link, dir.0.join("by-link-old")).unwrap();
        std::os::unix::fs::symlink(&outside, &by_link).unwrap();
        fs::rename(&by_folder, dir.0.join("by-folder-old")).unwrap();
        fs::create_dir(&by_folder).unwrap();
        fs::write(by_folder.join("new.txt"), "new").unwrap();
        let failed = delete_permanently(&asked, &confirmed);
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(outside.join("keep.txt").is_file());
        assert!(fs::symlink_metadata(&by_link).unwrap().is_symlink());
        assert!(by_folder.join("new.txt").is_file());
    }

    #[test]
    fn a_confirmed_delete_removes_only_the_unchanged_items_of_a_mixed_selection() {
        let dir = TempDir::new("delete-mixed");
        let p = |n: &str| dir.0.join(n);
        for n in ["same", "to-folder", "to-link", "gone", "edited"] {
            fs::write(p(n), "12345").unwrap();
        }
        fs::create_dir(p("folder")).unwrap();
        fs::write(p("folder/inner"), "x").unwrap();
        let asked: Vec<_> = ["same", "to-folder", "to-link", "gone", "edited", "folder"]
            .into_iter()
            .map(p)
            .collect();
        let confirmed = Chosen::of(&asked);
        fs::remove_file(p("to-folder")).unwrap();
        fs::create_dir(p("to-folder")).unwrap();
        fs::remove_file(p("to-link")).unwrap();
        std::os::unix::fs::symlink(p("same"), p("to-link")).unwrap();
        fs::remove_file(p("gone")).unwrap();
        // Rewritten in place: the same inode and size, a new change time.
        fs::write(p("edited"), "54321").unwrap();

        let failed = delete_permanently(&asked, &confirmed);
        let paths: Vec<_> = failed.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            paths,
            [p("to-folder"), p("to-link"), p("gone"), p("edited")]
        );
        for (path, e) in &failed {
            if path != &p("gone") {
                assert!(e.to_string().contains("nothing was done"), "{e}");
            }
        }
        assert!(fs::symlink_metadata(p("same")).is_err());
        assert!(fs::symlink_metadata(p("folder")).is_err());
        assert!(p("to-folder").is_dir());
        assert!(fs::symlink_metadata(p("to-link")).unwrap().is_symlink());
        assert_eq!(fs::read(p("edited")).unwrap(), b"54321");
    }

    #[test]
    fn a_confirmed_delete_keeps_a_file_rewritten_with_its_old_size_and_time() {
        let dir = TempDir::new("delete-same-time");
        let file = dir.0.join("doc.txt");
        fs::write(&file, "12345").unwrap();
        let before = fs::metadata(&file).unwrap().modified().unwrap();
        let asked = [file.clone()];
        let confirmed = Chosen::of(&asked);
        // A writer that keeps the time (or a file system with coarse
        // times): the same inode, size and modification time, new bytes.
        fs::write(&file, "54321").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), before);

        let failed = delete_permanently(&asked, &confirmed);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].1.to_string().contains("nothing was done"));
        assert_eq!(fs::read(&file).unwrap(), b"54321");
    }

    #[test]
    fn a_confirmed_delete_works_in_a_folder_that_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("delete-unlisted");
        let folder = dir.0.join("drop-box");
        fs::create_dir(&folder).unwrap();
        let file = folder.join("doc.txt");
        fs::write(&file, "x").unwrap();
        let tree = folder.join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("inner"), "y").unwrap();
        let asked = [file.clone(), tree.clone()];
        let confirmed = Chosen::of(&asked);
        // Write and search, but no read: the names are known, the folder
        // cannot be listed.
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o300)).unwrap();
        let failed = delete_permanently(&asked, &confirmed);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_empty(), "{failed:?}");
        assert!(fs::symlink_metadata(&file).is_err());
        assert!(fs::symlink_metadata(&tree).is_err());
    }

    #[test]
    fn descriptions_name_one_item_or_count_many() {
        assert_eq!(describe(&[PathBuf::from("/a/b.txt")]), "“b.txt”");
        assert_eq!(
            describe(&[PathBuf::from("/a"), PathBuf::from("/b")]),
            "2 items"
        );
    }
}
