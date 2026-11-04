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
use std::process::{Command, Stdio};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, StatxFlags};

use crate::i18n::{fill, tr};

/// Paths passed to one `gio trash` call, to stay well below the argument
/// size limit.
const TRASH_BATCH: usize = 256;

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

/// The `gio trash` commands for `paths`, in batches.
pub fn trash_commands(paths: &[PathBuf]) -> Vec<Command> {
    paths
        .chunks(TRASH_BATCH)
        .map(|batch| {
            let mut cmd = Command::new("gio");
            // After `--` every argument is a location, even one starting with `-`.
            cmd.arg("trash").arg("--").args(batch);
            cmd
        })
        .collect()
}

/// Moves `paths` to the trash. Returns an error message for the user.
pub fn trash(paths: &[PathBuf]) -> Result<(), String> {
    for mut cmd in trash_commands(paths) {
        let out = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| fill(tr("Cannot run gio: {}."), &[&e]))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err.lines().next().unwrap_or("").trim();
            return Err(if first.is_empty() {
                tr("Cannot move to the trash.").to_owned()
            } else {
                fill(tr("Cannot move to the trash: {}"), &[&first])
            });
        }
    }
    Ok(())
}

/// What a move to the trash did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TrashOutcome {
    /// Items no longer at their place afterwards.
    pub moved: usize,
    /// Items still at their place.
    pub kept: Vec<PathBuf>,
    /// The first error `gio` (or running it) gave, for the user.
    pub error: Option<String>,
}

/// Moves `paths` to the trash, reporting each item that stayed. Unlike
/// [`trash`], all batches run, also after one failed: `gio` goes on after
/// an item it cannot move, so afterwards each item of a failed batch is
/// looked at, and the ones still at their place are reported.
pub fn trash_all(paths: &[PathBuf]) -> TrashOutcome {
    trash_with(paths, trash)
}

/// [`trash`] with the command for one batch passed in.
fn trash_with(
    paths: &[PathBuf],
    mut run: impl FnMut(&[PathBuf]) -> Result<(), String>,
) -> TrashOutcome {
    let mut out = TrashOutcome::default();
    for batch in paths.chunks(TRASH_BATCH) {
        match run(batch) {
            Ok(()) => out.moved += batch.len(),
            Err(e) => {
                out.error.get_or_insert(e);
                for path in batch {
                    if std::fs::symlink_metadata(path).is_ok() {
                        out.kept.push(path.clone());
                    } else {
                        out.moved += 1;
                    }
                }
            }
        }
    }
    out
}

/// Which item a path named when the user asked to delete it: device,
/// inode and type, of the entry itself (a link, not what it points to).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    dev: u64,
    ino: u64,
    kind: u32,
}

impl Identity {
    fn from_meta(m: &std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            kind: m.mode() & S_IFMT,
        }
    }

    /// The identities of the items at `paths` now; items that cannot be
    /// read are left out (they are not deleted later).
    pub fn of_all(paths: &[PathBuf]) -> Vec<Self> {
        paths
            .iter()
            .filter_map(|p| std::fs::symlink_metadata(p).ok())
            .map(|m| Self::from_meta(&m))
            .collect()
    }
}

/// `S_IFMT`: the type bits of a mode.
const S_IFMT: u32 = 0o170000;

/// Deletes `paths` permanently: folders with all their contents, links as
/// links (never what they point to). Only items whose [`Identity`] is in
/// `confirmed` are deleted: an item replaced at its path after the user
/// confirmed (a save by an editor, a sync) is refused and kept. A folder
/// that is, or holds, another file system (a mounted drive or share, a
/// bind mount, a btrfs subvolume) is not deleted at all: see
/// [`remove_tree`]. Returns the paths that failed.
pub fn delete_permanently(paths: &[PathBuf], confirmed: &[Identity]) -> Vec<(PathBuf, io::Error)> {
    let mut failed = Vec::new();
    for path in paths {
        let result = match std::fs::symlink_metadata(path) {
            Ok(m) if !confirmed.contains(&Identity::from_meta(&m)) => Err(io::Error::other(tr(
                "another item is at this path now than when the delete was confirmed, so it was kept",
            ))),
            Ok(m) if m.is_dir() => remove_tree(path, (m.dev(), m.ino())),
            Ok(_) => std::fs::remove_file(path),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            failed.push((path.clone(), e));
        }
    }
    failed
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
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent_fd = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let name = CString::new(name.as_bytes())?;
    let root = open_subdir(parent_fd.as_fd(), &name)?;
    let st = rustix::fs::fstat(&root)?;
    if (st.st_dev, st.st_ino) != expected {
        return Err(io::Error::other(tr(
            "another item is at this path now than when the delete was confirmed, so it was kept",
        )));
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
    fn trash_runs_gio_in_batches_with_an_option_terminator() {
        let paths: Vec<PathBuf> = (0..TRASH_BATCH + 3)
            .map(|i| PathBuf::from(format!("/t/-{i}")))
            .collect();
        let cmds = trash_commands(&paths);
        assert_eq!(cmds.len(), 2);
        let args: Vec<&OsStr> = cmds[1].get_args().collect();
        assert_eq!(cmds[1].get_program(), "gio");
        assert_eq!(&args[..2], [OsStr::new("trash"), OsStr::new("--")]);
        assert_eq!(args.len(), 2 + 3);
        assert_eq!(cmds[0].get_args().count(), 2 + TRASH_BATCH);
    }

    #[test]
    fn a_failed_trash_batch_reports_each_kept_item_and_the_rest_goes_on() {
        let dir = TempDir::new("trash-batches");
        let bin = dir.0.join("bin");
        fs::create_dir(&bin).unwrap();
        let paths: Vec<PathBuf> = (0..TRASH_BATCH + 5)
            .map(|i| {
                let p = dir.0.join(format!("f{i}"));
                fs::write(&p, "x").unwrap();
                p
            })
            .collect();
        let stuck = [paths[3].clone(), paths[7].clone()];
        let mut batches = 0;
        let out = trash_with(&paths, |batch| {
            batches += 1;
            // Like gio: every item it can move, then a failure.
            for p in batch.iter().filter(|p| !stuck.contains(p)) {
                fs::rename(p, bin.join(p.file_name().unwrap())).unwrap();
            }
            if batch.iter().any(|p| stuck.contains(p)) {
                Err("Cannot move to the trash: no permission".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(batches, 2, "the batch after the failed one ran");
        assert_eq!(out.kept, stuck);
        assert_eq!(out.moved, paths.len() - 2);
        assert_eq!(
            out.error.as_deref(),
            Some("Cannot move to the trash: no permission")
        );
        assert!(paths[TRASH_BATCH + 4].symlink_metadata().is_err());

        let fine = trash_with(&[], |_| unreachable!());
        assert_eq!(fine, TrashOutcome::default());
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
        let failed = delete_permanently(&all, &Identity::of_all(&all));
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
                &Identity::of_all(std::slice::from_ref(&tree))
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
            let failed = delete_permanently(one, &Identity::of_all(one));
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert!(failed[0].1.to_string().contains("inner"), "{failed:?}");
            assert_eq!(
                fs::read_to_string(inner.join("data.txt")).unwrap(),
                "precious"
            );
            assert!(victim.join("own.txt").exists());
            // Picking the mount point itself is refused too.
            let one = std::slice::from_ref(&inner);
            let failed = delete_permanently(one, &Identity::of_all(one));
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
        let confirmed = Identity::of_all(&asked);
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
        assert!(failed[0].1.to_string().contains("kept"), "{failed:?}");
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
    fn descriptions_name_one_item_or_count_many() {
        assert_eq!(describe(&[PathBuf::from("/a/b.txt")]), "“b.txt”");
        assert_eq!(
            describe(&[PathBuf::from("/a"), PathBuf::from("/b")]),
            "2 items"
        );
    }
}
