//! Pasting files into a folder: copying or moving them.
//!
//! Nothing is ever replaced. When a name is taken in the folder, the item
//! gets the first free name of the form `name (2).ext`, `name (3).ext`, …
//! A copy is written under a temporary name in the target folder and gets
//! its real name only when it is complete, so a failure or a cancel leaves
//! no partial item (when the partial copy cannot be removed, the failure
//! names it). An existing entry under the temporary name is never touched.
//! A move is a rename when the folder is on the same file
//! system; otherwise the item is copied the same way and only the entries
//! that were copied are removed from the source afterwards, so the source
//! is never deleted when the move fails. Links are copied as links; special
//! files (sockets, pipes, devices) are skipped and stay in the source.

use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString, c_char, c_int, c_uint};
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use everygrep_core::progress::Progress;

use crate::clipboard::Operation;
use crate::i18n::{fill, tr};

/// Size of one read when copying a file; cancel is checked between reads.
const CHUNK: usize = 1 << 20;

/// Longest file name Linux file systems accept, in bytes.
const NAME_MAX: usize = 255;

/// Why an item was not pasted.
#[derive(Debug)]
pub enum Failure {
    /// A folder pasted into itself or into a folder inside it.
    IntoItself,
    Io(io::Error),
    /// Moved by copying, but some of the original could not be removed;
    /// the copy is complete and nothing was lost.
    NotRemoved(io::Error),
    /// The copy stopped (for `cause`, or a cancel when `None`) and its
    /// hidden partial copy `part` could not be removed.
    PartLeft {
        cause: Option<io::Error>,
        part: PathBuf,
        error: io::Error,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::IntoItself => f.write_str(tr("A folder cannot be pasted into itself.")),
            Failure::Io(e) => write!(f, "{e}"),
            Failure::NotRemoved(e) => f.write_str(&fill(
                tr("copied, but the original could not be removed: {}"),
                &[e],
            )),
            Failure::PartLeft { cause, part, error } => {
                let left = fill(
                    tr("the partial copy “{}” could not be removed: {}"),
                    &[&part.display(), error],
                );
                match cause {
                    Some(c) => write!(f, "{c}; {left}"),
                    None => f.write_str(&left),
                }
            }
        }
    }
}

/// What a paste did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The new items, in the order of the sources.
    pub done: Vec<PathBuf>,
    /// How many of them got another name because theirs was taken.
    pub renamed: usize,
    /// Items moved into the folder they are already in: left as they are.
    pub unchanged: usize,
    pub failed: Vec<(PathBuf, Failure)>,
    /// Special files left out of copied folders.
    pub skipped_special: u64,
    /// The paste was cancelled; the item in progress was not pasted and the
    /// rest were not tried.
    pub cancelled: bool,
}

/// Why copying stopped.
#[derive(Debug)]
enum Stop {
    Cancelled,
    Io(io::Error),
    /// Stopped for `cause`, and the partial copy `part` could not be
    /// removed.
    PartLeft {
        cause: Box<Stop>,
        part: PathBuf,
        error: io::Error,
    },
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Io(e)
    }
}

/// Copies (`Operation::Copy`) or moves (`Operation::Cut`) `sources` into
/// the folder `dest`. Progress is counted in bytes copied; cancel through
/// `progress` stops at the next read.
pub fn paste(op: Operation, sources: &[PathBuf], dest: &Path, progress: &Progress) -> Outcome {
    let mut out = Outcome::default();
    let dest = match fs::canonicalize(dest) {
        Ok(d) => d,
        Err(e) => {
            out.failed = sources
                .iter()
                .map(|s| {
                    (
                        s.clone(),
                        Failure::Io(io::Error::new(e.kind(), e.to_string())),
                    )
                })
                .collect();
            return out;
        }
    };
    let dest_dev = fs::metadata(&dest).map(|m| m.dev()).ok();
    let total: u64 = sources
        .iter()
        .filter(|s| {
            op == Operation::Copy || fs::symlink_metadata(s).map(|m| m.dev()).ok() != dest_dev
        })
        .map(|s| tree_size(s, progress))
        .sum();
    progress.restart(Some(total));

    let mut run = Run::default();
    for src in sources {
        if progress.is_cancelled() {
            out.cancelled = true;
            break;
        }
        match paste_one(op, src, &dest, progress, &mut run) {
            Ok(Pasted::New(target)) => {
                if target.file_name() != src.file_name() {
                    out.renamed += 1;
                }
                out.done.push(target);
            }
            Ok(Pasted::Unchanged) => out.unchanged += 1,
            Ok(Pasted::Failed(f)) => out.failed.push((src.clone(), f)),
            Err(Stop::Cancelled) => {
                out.cancelled = true;
                break;
            }
            Err(Stop::Io(e)) => out.failed.push((src.clone(), Failure::Io(e))),
            Err(Stop::PartLeft { cause, part, error }) => {
                let (cause, cancelled) = match *cause {
                    Stop::Io(e) => (Some(e), false),
                    _ => (None, true),
                };
                out.failed
                    .push((src.clone(), Failure::PartLeft { cause, part, error }));
                if cancelled {
                    out.cancelled = true;
                    break;
                }
            }
        }
    }
    out.skipped_special = run.special;
    out
}

/// What one paste keeps from item to item.
#[derive(Default)]
struct Run {
    /// Special files left out so far.
    special: u64,
    /// For each name (and whether it is a folder's), the first number
    /// `name (N)` not yet known to be taken in the target folder.
    next: HashMap<(OsString, bool), u64>,
}

impl Run {
    /// [`free_name`] in the target folder `dir`, starting after the number
    /// the last item of the same name got.
    fn free_name(&mut self, dir: &Path, name: &OsStr, is_dir: bool) -> io::Result<PathBuf> {
        let from = self.next.entry((name.to_owned(), is_dir)).or_insert(2);
        free_name(dir, name, is_dir, from)
    }
}

/// What became of one item.
enum Pasted {
    New(PathBuf),
    /// Moved into the folder it is already in.
    Unchanged,
    Failed(Failure),
}

fn paste_one(
    op: Operation,
    src: &Path,
    dest: &Path,
    progress: &Progress,
    run: &mut Run,
) -> Result<Pasted, Stop> {
    let meta = fs::symlink_metadata(src)?;
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    if meta.is_dir() && dest.starts_with(fs::canonicalize(src)?) {
        return Ok(Pasted::Failed(Failure::IntoItself));
    }
    let parent = src.parent().map(fs::canonicalize).transpose()?;
    if op == Operation::Cut && parent.as_deref() == Some(dest) {
        return Ok(Pasted::Unchanged);
    }
    match op {
        Operation::Copy => {
            let (target, _) = copy_in(src, &meta, dest, name, progress, false, run)?;
            Ok(Pasted::New(target))
        }
        Operation::Cut => move_in(src, &meta, dest, name, progress, run),
    }
}

/// Moves `src` into `dest` by renaming, or by copying and then removing
/// what was copied when the folder is on another file system.
fn move_in(
    src: &Path,
    meta: &Metadata,
    dest: &Path,
    name: &OsStr,
    progress: &Progress,
    run: &mut Run,
) -> Result<Pasted, Stop> {
    loop {
        let target = run.free_name(dest, name, meta.is_dir())?;
        match rename_noreplace(src, &target) {
            Ok(()) => return Ok(Pasted::New(target)),
            // Taken since we looked: try the next free name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => break,
            Err(e) => return Err(e.into()),
        }
    }
    let (target, copied) = copy_in(src, meta, dest, name, progress, true, run)?;
    match remove_copied(&copied) {
        None => Ok(Pasted::New(target)),
        Some(e) => Ok(Pasted::Failed(Failure::NotRemoved(e))),
    }
}

/// Removes the source entries that were copied, deepest first. A folder
/// that still holds something (a special file, or a file added meanwhile)
/// stays. Returns the first error.
fn remove_copied(copied: &[(PathBuf, bool)]) -> Option<io::Error> {
    let mut first_error = None;
    for (path, is_dir) in copied.iter().rev() {
        let removed = if *is_dir {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        };
        if let Err(e) = removed {
            first_error.get_or_insert(e);
        }
    }
    first_error
}

/// Copies `src` into `dest` under a temporary name, then gives it the first
/// free name. Returns the new path and the source entries copied (with
/// whether each is a folder), parents before their contents.
fn copy_in(
    src: &Path,
    meta: &Metadata,
    dest: &Path,
    name: &OsStr,
    progress: &Progress,
    sync: bool,
    run: &mut Run,
) -> Result<(PathBuf, Vec<(PathBuf, bool)>), Stop> {
    let mut copied = Vec::new();
    let mut buf = vec![0; CHUNK];
    let mut tries = 0;
    let temp = loop {
        let temp = temp_name(dest);
        let mut copier = Copier {
            progress,
            sync,
            buf: &mut buf,
            copied: &mut copied,
            special: &mut run.special,
            made: false,
        };
        match copier.tree(src, meta, &temp) {
            Ok(()) => break temp,
            // The hidden name is taken by an entry that is not ours (left
            // by a crash, or by another program): keep it, take the next.
            Err(Stop::Io(e))
                if e.kind() == io::ErrorKind::AlreadyExists
                    && !copier.made
                    && tries < TEMP_TRIES =>
            {
                tries += 1;
            }
            Err(stop) if copier.made => return Err(cleaned_up(&temp, stop)),
            Err(stop) => return Err(stop),
        }
    };
    // Cancelled after the last read: the copy is not published (and a
    // move does not remove its source).
    if progress.is_cancelled() {
        return Err(cleaned_up(&temp, Stop::Cancelled));
    }
    loop {
        let target = match run.free_name(dest, name, meta.is_dir()) {
            Ok(t) => t,
            Err(e) => return Err(cleaned_up(&temp, e.into())),
        };
        match rename_noreplace(&temp, &target) {
            Ok(()) => return Ok((target, copied)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(cleaned_up(&temp, e.into())),
        }
    }
}

/// How many hidden temporary names one copy tries before it gives up.
const TEMP_TRIES: u32 = 100;

/// Removes our partial copy `temp` after the copy stopped for `stop`, and
/// tells when it could not be removed.
fn cleaned_up(temp: &Path, stop: Stop) -> Stop {
    match remove_partial(temp) {
        Ok(()) => stop,
        Err(error) => Stop::PartLeft {
            cause: Box::new(stop),
            part: temp.to_owned(),
            error,
        },
    }
}

/// Removes a temporary copy of ours. Its folders are made writable first,
/// since a copied read-only folder could not be emptied otherwise.
fn remove_partial(temp: &Path) -> io::Result<()> {
    match fs::symlink_metadata(temp) {
        Ok(m) if m.is_dir() => {
            writable_tree(temp);
            fs::remove_dir_all(temp)
        }
        Ok(_) => fs::remove_file(temp),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Gives the owner full access to the folder `dir` and the folders in it,
/// links not followed. Errors are left to the removal that follows.
fn writable_tree(dir: &Path) {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
    }
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            writable_tree(&entry.path());
        }
    }
}

struct Copier<'a> {
    progress: &'a Progress,
    /// Flush every file to disk before it counts as copied (for moves,
    /// whose source is removed afterwards).
    sync: bool,
    buf: &'a mut Vec<u8>,
    copied: &'a mut Vec<(PathBuf, bool)>,
    special: &'a mut u64,
    /// Whether the copy's top entry was made, so there is something of
    /// ours to remove when it stops.
    made: bool,
}

impl Copier<'_> {
    /// Copies the entry `src` (described by `meta`, links not followed) to
    /// `dst`, which must not exist.
    fn tree(&mut self, src: &Path, meta: &Metadata, dst: &Path) -> Result<(), Stop> {
        // Folders and links read no data, so the read loop alone would not
        // see a cancel in a tree of them.
        if self.progress.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        let kind = meta.file_type();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
            self.made = true;
            self.copied.push((src.to_owned(), false));
        } else if kind.is_file() {
            self.file(src, meta, dst)?;
            self.copied.push((src.to_owned(), false));
        } else if kind.is_dir() {
            fs::create_dir(dst)?;
            self.made = true;
            self.copied.push((src.to_owned(), true));
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                // DirEntry::metadata does not follow links.
                let m = entry.metadata()?;
                self.tree(&entry.path(), &m, &dst.join(entry.file_name()))?;
            }
            // Last, so a read-only folder can still be filled.
            fs::set_permissions(dst, fs::Permissions::from_mode(mode(meta)))?;
            if let Ok(t) = meta.modified() {
                let _ = File::open(dst).and_then(|d| d.set_modified(t));
            }
        } else {
            *self.special += 1;
        }
        Ok(())
    }

    fn file(&mut self, src: &Path, meta: &Metadata, dst: &Path) -> Result<(), Stop> {
        let mut from = File::open(src)?;
        let mut to = OpenOptions::new().write(true).create_new(true).open(dst)?;
        self.made = true;
        loop {
            if self.progress.is_cancelled() {
                return Err(Stop::Cancelled);
            }
            let n = match from.read(self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            to.write_all(&self.buf[..n])?;
            self.progress.add(n as u64);
        }
        to.set_permissions(fs::Permissions::from_mode(mode(meta)))?;
        if let Ok(t) = meta.modified() {
            to.set_modified(t)?;
        }
        if self.sync {
            to.sync_all()?;
        }
        Ok(())
    }
}

/// The permission bits to give a copy: those of the source without
/// set-user-ID and set-group-ID.
fn mode(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o1777
}

/// Bytes of the files in `path`, links not followed. Stops counting once
/// the paste is cancelled.
fn tree_size(path: &Path, progress: &Progress) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() || progress.is_cancelled() {
        return 0;
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| tree_size(&e.path(), progress))
        .sum()
}

/// `dir/name` when nothing has that name, else the first free
/// `stem (N)ext`, trying N from `*from` (at least 2) on; `*from` becomes
/// the number after the one used. Folders have no extension. A dangling
/// link counts as taken; an error other than "not found" is returned, since
/// the name may well be taken. The stem is shortened (never inside a UTF-8
/// sequence) so that the name with its number fits in [`NAME_MAX`] bytes.
fn free_name(dir: &Path, name: &OsStr, is_dir: bool, from: &mut u64) -> io::Result<PathBuf> {
    let first = dir.join(name);
    if !taken(&first)? {
        return Ok(first);
    }
    let bytes = name.as_bytes();
    let (stem, ext) = match bytes.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 && !is_dir => bytes.split_at(dot),
        _ => (bytes, &b""[..]),
    };
    let mut n = (*from).max(2);
    loop {
        let path = dir.join(OsString::from_vec(numbered(bytes, stem, ext, n)));
        if !taken(&path)? {
            *from = n + 1;
            return Ok(path);
        }
        n += 1;
    }
}

/// `stem (n)ext` in at most [`NAME_MAX`] bytes. An extension too long to
/// keep is treated as part of the name.
fn numbered(name: &[u8], stem: &[u8], ext: &[u8], n: u64) -> Vec<u8> {
    let mark = format!(" ({n})");
    let (stem, ext) = if mark.len() + ext.len() >= NAME_MAX {
        (name, &b""[..])
    } else {
        (stem, ext)
    };
    let mut end = stem.len().min(NAME_MAX - mark.len() - ext.len());
    // Back off over UTF-8 continuation bytes, to cut between characters.
    while end > 0 && end < stem.len() && stem[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    let mut out = stem[..end].to_vec();
    out.extend_from_slice(mark.as_bytes());
    out.extend_from_slice(ext);
    out
}

/// Whether something (a dangling link too) has the name `path`.
fn taken(path: &Path) -> io::Result<bool> {
    probed();
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
thread_local! {
    /// Names [`free_name`] looked at on this thread (tests only).
    static PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one name looked at (tests only).
fn probed() {
    #[cfg(test)]
    PROBES.with(|p| p.set(p.get() + 1));
}

/// A name in `dir` for a copy in progress; hidden, and unique to this
/// process and paste.
fn temp_name(dir: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".everygrep-paste-{}-{n}.part", std::process::id()))
}

unsafe extern "C" {
    fn renameat2(
        olddirfd: c_int,
        oldpath: *const c_char,
        newdirfd: c_int,
        newpath: *const c_char,
        flags: c_uint,
    ) -> c_int;
}

/// `AT_FDCWD` from `<fcntl.h>`: paths are relative to the working folder.
const AT_FDCWD: c_int = -100;
/// `RENAME_NOREPLACE` from `<linux/fs.h>`.
const RENAME_NOREPLACE: c_uint = 1;

/// Renames `from` to `to`, failing with `AlreadyExists` instead of
/// replacing. File systems without this kind of rename (NFS, SMB, many FUSE
/// file systems) get a check and a plain rename, which could replace a file
/// created in between.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_noreplace_with(from, to, renameat2_noreplace)
}

/// [`rename_noreplace`] with the atomic rename passed in, so the fallback
/// can be tested on file systems that do support it.
fn rename_noreplace_with(
    from: &Path,
    to: &Path,
    atomic: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    match atomic(from, to) {
        // EINVAL: the file system does not support RENAME_NOREPLACE (or the
        // rename itself is invalid, which the plain rename reports again).
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
        result => return result,
    }
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(from, to)
}

/// `renameat2` with `RENAME_NOREPLACE`: atomic, never replaces the target.
fn renameat2_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let a = CString::new(from.as_os_str().as_bytes())?;
    let b = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both are valid C strings that outlive the call.
    let rc = unsafe { renameat2(AT_FDCWD, a.as_ptr(), AT_FDCWD, b.as_ptr(), RENAME_NOREPLACE) };
    if rc == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fresh empty folder under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "everygrep-gui-test-paste-{tag}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // Read-only folders made by a test must be writable again
            // before their contents can go.
            fn writable(dir: &Path) {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
                for e in fs::read_dir(dir).into_iter().flatten().flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        writable(&e.path());
                    }
                }
            }
            writable(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    /// Names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn free_names_never_clash() {
        let t = TempDir::new("names");
        let dir = t.path("d");
        fs::create_dir(&dir).unwrap();
        let n = |name: &str, is_dir| free_name(&dir, OsStr::new(name), is_dir, &mut 2).unwrap();
        assert_eq!(n("a.txt", false), dir.join("a.txt"));
        write(&dir.join("a.txt"), "");
        assert_eq!(n("a.txt", false), dir.join("a (2).txt"));
        write(&dir.join("a (2).txt"), "");
        assert_eq!(n("a.txt", false), dir.join("a (3).txt"));
        write(&dir.join("x.tar.gz"), "");
        assert_eq!(n("x.tar.gz", false), dir.join("x.tar (2).gz"));
        write(&dir.join(".bashrc"), "");
        assert_eq!(n(".bashrc", false), dir.join(".bashrc (2)"));
        fs::create_dir(dir.join("photos.2026")).unwrap();
        assert_eq!(n("photos.2026", true), dir.join("photos.2026 (2)"));
        symlink(dir.join("missing"), dir.join("dangling")).unwrap();
        assert_eq!(n("dangling", false), dir.join("dangling (2)"));
        // A folder whose entries cannot be looked at gives no name.
        let shut = t.path("shut");
        fs::create_dir(&shut).unwrap();
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o600)).unwrap();
        let denied = fs::symlink_metadata(shut.join("x"))
            .is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);
        // (Running as root, the entry can be looked at anyway.)
        if denied {
            assert!(free_name(&shut, OsStr::new("x"), false, &mut 2).is_err());
        }
    }

    #[test]
    fn a_name_of_the_longest_length_gets_a_shortened_free_name() {
        let t = TempDir::new("name-max");
        let dir = t.path("d");
        let long = format!("{}.txt", "x".repeat(251));
        write(&dir.join(&long), "x");
        let out = paste(Operation::Copy, &[dir.join(&long)], &dir, &Progress::new());
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let short = format!("{} (2).txt", "x".repeat(247));
        assert_eq!(read(&dir.join(&short)), "x");
        // A name in another script is cut between characters, and a name
        // that is not UTF-8 between whole sequences.
        let wide = format!("{}.txt", "ə".repeat(125));
        write(&dir.join(&wide), "");
        let got = free_name(&dir, OsStr::new(&wide), false, &mut 2).unwrap();
        let got = got.file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(got, format!("{} (2).txt", "ə".repeat(123)));
        let mut odd = vec![0xff];
        odd.extend("ə".repeat(127).as_bytes());
        write(&dir.join(OsStr::from_bytes(&odd)), "");
        let got = free_name(&dir, OsStr::from_bytes(&odd), false, &mut 2).unwrap();
        let got = got.file_name().unwrap().as_bytes().to_vec();
        assert!(got.len() <= 255, "{}", got.len());
        assert!(got.ends_with(" (2)".as_bytes()));
        assert!(std::str::from_utf8(&got[1..]).is_ok());
    }

    #[test]
    fn many_items_of_one_name_do_not_search_from_the_start_each_time() {
        let t = TempDir::new("same-names");
        let dest = t.path("dest");
        write(&dest.join("README.md"), "0");
        let sources: Vec<PathBuf> = (0..200)
            .map(|i| {
                let p = t.path(&format!("src/{i}/README.md"));
                write(&p, &i.to_string());
                p
            })
            .collect();
        PROBES.with(|p| p.set(0));
        let out = paste(Operation::Copy, &sources, &dest, &Progress::new());
        let probes = PROBES.with(|p| p.get());
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(read(&dest.join("README (201).md")), "199");
        assert!(probes < 4 * 200, "{probes} names looked at");
    }

    #[test]
    fn copy_keeps_both_on_a_clash_and_never_replaces() {
        let t = TempDir::new("copy");
        let src = t.path("src/a.txt");
        write(&src, "new");
        let dest = t.path("dest");
        write(&dest.join("a.txt"), "old");
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let dest_real = fs::canonicalize(&dest).unwrap();
        assert_eq!(out.done, [dest_real.join("a (2).txt")]);
        assert_eq!(out.renamed, 1);
        assert_eq!(read(&dest.join("a.txt")), "old");
        assert_eq!(read(&dest.join("a (2).txt")), "new");
        assert_eq!(read(&src), "new");
        assert_eq!(names(&dest), ["a (2).txt", "a.txt"]);
    }

    #[test]
    fn copy_into_the_same_folder_makes_a_second_item() {
        let t = TempDir::new("same");
        let src = t.path("d/a.txt");
        write(&src, "x");
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &t.path("d"),
            &Progress::new(),
        );
        assert_eq!(out.done.len(), 1);
        assert_eq!(names(&t.path("d")), ["a (2).txt", "a.txt"]);
    }

    #[test]
    fn copy_of_a_folder_keeps_contents_links_and_modes() {
        let t = TempDir::new("tree");
        let src = t.path("src/tree");
        write(&src.join("sub/f.txt"), "f");
        write(&src.join("g.sh"), "g");
        fs::set_permissions(src.join("g.sh"), fs::Permissions::from_mode(0o750)).unwrap();
        symlink("sub/f.txt", src.join("link")).unwrap();
        fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o555)).unwrap();
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let copy = dest.join("tree");
        assert_eq!(read(&copy.join("sub/f.txt")), "f");
        assert_eq!(
            fs::metadata(copy.join("g.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(
            fs::read_link(copy.join("link")).unwrap(),
            PathBuf::from("sub/f.txt")
        );
        assert_eq!(
            fs::metadata(copy.join("sub")).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert_eq!(names(&dest), ["tree"]);
        // The source is untouched.
        assert_eq!(read(&src.join("sub/f.txt")), "f");
    }

    #[test]
    fn a_failed_copy_leaves_no_partial_item() {
        let t = TempDir::new("partial");
        let src = t.path("src/tree");
        write(&src.join("ok.txt"), "ok");
        let secret = src.join("secret.txt");
        write(&secret, "s");
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(&secret).is_ok() {
            // Running as root: every file can be read, nothing to test.
            return;
        }
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        assert!(out.done.is_empty());
        assert_eq!(out.failed.len(), 1);
        assert!(matches!(out.failed[0].1, Failure::Io(_)));
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
    }

    #[test]
    fn cancel_stops_and_removes_the_item_in_progress() {
        let t = TempDir::new("cancel");
        let a = t.path("src/a.txt");
        let b = t.path("src/b.txt");
        write(&a, "a");
        write(&b, "b");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let progress = Progress::new();
        progress.cancel();
        let out = paste(Operation::Copy, &[a, b], &dest, &progress);
        assert!(out.cancelled);
        assert!(out.done.is_empty() && out.failed.is_empty());
        assert!(names(&dest).is_empty());

        // Cancelled while a file is being copied: its temporary copy goes.
        let progress = Progress::new();
        let big = t.path("src/big");
        write(&big, &"x".repeat(CHUNK * 2));
        let mut buf = vec![0; CHUNK];
        let (mut copied, mut special) = (Vec::new(), 0);
        let meta = fs::symlink_metadata(&big).unwrap();
        let temp = temp_name(&dest);
        progress.cancel();
        let mut c = Copier {
            progress: &progress,
            sync: false,
            buf: &mut buf,
            copied: &mut copied,
            special: &mut special,
            made: false,
        };
        // Past the check at the top of the tree: the file is made, and the
        // read loop sees the cancel.
        assert!(matches!(c.file(&big, &meta, &temp), Err(Stop::Cancelled)));
        assert!(c.made);
        remove_partial(&temp).unwrap();
        assert!(names(&dest).is_empty());
    }

    #[test]
    fn a_paste_never_removes_an_entry_it_did_not_make() {
        let t = TempDir::new("foreign-temp");
        let src = t.path("src/a.txt");
        write(&src, "a");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        // Entries under the hidden names the next pastes of this process
        // would use: left by an earlier crash, or by another program.
        let next = temp_name(&dest);
        let name = next.file_name().unwrap().to_string_lossy().into_owned();
        let (head, n) = name.trim_end_matches(".part").rsplit_once('-').unwrap();
        let n: u64 = n.parse().unwrap();
        let victims: Vec<PathBuf> = (n + 1..n + 65)
            .map(|i| dest.join(format!("{head}-{i}.part")))
            .collect();
        for v in &victims {
            write(&v.join("keep"), "keep");
        }
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        for v in &victims {
            assert_eq!(read(&v.join("keep")), "keep", "{v:?}");
        }
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(read(&dest.join("a.txt")), "a");
    }

    #[test]
    fn a_partial_copy_with_a_read_only_folder_is_removed() {
        let t = TempDir::new("ro-partial");
        let part = t.path("dest/.everygrep-paste-test.part");
        write(&part.join("ro/sub/f"), "f");
        fs::set_permissions(part.join("ro/sub"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(part.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(part.join("ro/probe"), "").is_ok() {
            // Running as root: the folder is writable anyway.
            return;
        }
        remove_partial(&part).unwrap();
        assert!(fs::symlink_metadata(&part).is_err(), "partial copy left");
        // Nothing there is not an error.
        remove_partial(&part).unwrap();
    }

    #[test]
    fn cancel_stops_a_tree_of_folders_and_links() {
        let t = TempDir::new("cancel-tree");
        let src = t.path("src/tree");
        for i in 0..20 {
            fs::create_dir_all(src.join(format!("d{i}/e"))).unwrap();
            symlink("d0", src.join(format!("l{i}"))).unwrap();
        }
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let progress = Progress::new();
        progress.cancel();
        let meta = fs::symlink_metadata(&src).unwrap();
        let mut run = Run::default();
        let got = copy_in(
            &src,
            &meta,
            &dest,
            OsStr::new("tree"),
            &progress,
            true,
            &mut run,
        );
        assert!(matches!(got, Err(Stop::Cancelled)), "{got:?}");
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(fs::read_dir(&src).unwrap().count(), 40);
    }

    #[test]
    fn cut_moves_and_renames_on_a_clash() {
        let t = TempDir::new("cut");
        let a = t.path("src/a.txt");
        let tree = t.path("src/tree");
        write(&a, "new");
        write(&tree.join("f"), "f");
        let dest = t.path("dest");
        write(&dest.join("a.txt"), "old");
        let out = paste(
            Operation::Cut,
            &[a.clone(), tree.clone()],
            &dest,
            &Progress::new(),
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(out.done.len(), 2);
        assert_eq!(out.renamed, 1);
        assert!(!a.exists() && !tree.exists());
        assert_eq!(read(&dest.join("a.txt")), "old");
        assert_eq!(read(&dest.join("a (2).txt")), "new");
        assert_eq!(read(&dest.join("tree/f")), "f");
    }

    #[test]
    fn cut_into_its_own_folder_changes_nothing() {
        let t = TempDir::new("cut-same");
        let a = t.path("d/a.txt");
        write(&a, "a");
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&a),
            &t.path("d"),
            &Progress::new(),
        );
        assert_eq!(out.unchanged, 1);
        assert!(out.done.is_empty() && out.failed.is_empty());
        assert_eq!(names(&t.path("d")), ["a.txt"]);
    }

    #[test]
    fn a_folder_is_never_pasted_into_itself() {
        let t = TempDir::new("itself");
        let tree = t.path("tree");
        write(&tree.join("sub/f"), "f");
        for op in [Operation::Copy, Operation::Cut] {
            for dest in [tree.clone(), tree.join("sub")] {
                let out = paste(op, std::slice::from_ref(&tree), &dest, &Progress::new());
                assert_eq!(out.unchanged, 0);
                assert!(out.done.is_empty());
                assert!(matches!(out.failed[..], [(_, Failure::IntoItself)]));
            }
        }
        assert_eq!(names(&tree), ["sub"]);
        assert_eq!(names(&tree.join("sub")), ["f"]);
    }

    #[test]
    fn a_failed_move_keeps_the_source() {
        let t = TempDir::new("move-fail");
        let a = t.path("src/a.txt");
        write(&a, "a");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(dest.join("probe"), "").is_ok() {
            // Running as root: the folder is writable anyway.
            return;
        }
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&a),
            &dest,
            &Progress::new(),
        );
        assert!(out.done.is_empty());
        assert!(matches!(out.failed[..], [(_, Failure::Io(_))]));
        assert_eq!(read(&a), "a");

        // A missing source is reported and nothing else happens.
        let gone = t.path("src/gone");
        let out = paste(Operation::Cut, &[gone], &t.path("src"), &Progress::new());
        assert!(matches!(out.failed[..], [(_, Failure::Io(_))]));
    }

    #[test]
    fn moving_by_copy_removes_only_what_was_copied() {
        // The fallback for another file system, run directly: the source
        // keeps whatever was not copied.
        let t = TempDir::new("move-copy");
        let src = t.path("src/tree");
        write(&src.join("f"), "f");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let meta = fs::symlink_metadata(&src).unwrap();
        let mut run = Run::default();
        let (target, copied) = copy_in(
            &src,
            &meta,
            &dest,
            OsStr::new("tree"),
            &Progress::new(),
            true,
            &mut run,
        )
        .unwrap();
        assert_eq!(target, dest.join("tree"));
        assert_eq!(copied, [(src.clone(), true), (src.join("f"), false)]);
        // A file added after the copy is not in the list and stays.
        write(&src.join("late"), "l");
        assert!(remove_copied(&copied).is_some());
        assert_eq!(names(&src), ["late"]);
        assert_eq!(read(&dest.join("tree/f")), "f");
        // Without it, the whole source goes.
        fs::remove_file(src.join("late")).unwrap();
        assert!(remove_copied(&copied[..1]).is_none());
        assert!(!src.exists());
    }

    #[test]
    fn rename_never_replaces() {
        let t = TempDir::new("noreplace");
        let (a, b) = (t.path("a"), t.path("b"));
        write(&a, "a");
        write(&b, "b");
        let e = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&b), "b");
        rename_noreplace(&a, &t.path("c")).unwrap();
        assert_eq!(read(&t.path("c")), "a");
    }

    #[test]
    fn rename_falls_back_where_the_atomic_rename_is_unsupported() {
        let t = TempDir::new("noreplace-fallback");
        let (a, b) = (t.path("a"), t.path("b"));
        write(&a, "a");
        write(&b, "b");
        // What NFS or an SMB share answers to RENAME_NOREPLACE.
        let unsupported = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(22));
        let e = rename_noreplace_with(&a, &b, unsupported).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&b), "b");
        rename_noreplace_with(&a, &t.path("c"), unsupported).unwrap();
        assert_eq!(read(&t.path("c")), "a");
        assert!(!a.exists());
        // Other errors are reported as they are, without a second attempt.
        let denied = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(13));
        let e = rename_noreplace_with(&t.path("c"), &t.path("d"), denied).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(read(&t.path("c")), "a");
    }

    #[test]
    fn progress_counts_the_bytes_to_copy() {
        let t = TempDir::new("progress");
        let src = t.path("src/tree");
        write(&src.join("a"), "12345");
        write(&src.join("b/c"), "678");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let progress = Progress::new();
        paste(Operation::Copy, &[src], &dest, &progress);
        assert_eq!(progress.total(), Some(8));
        assert_eq!(progress.done(), 8);
    }

    #[test]
    fn failures_read_plainly() {
        assert_eq!(
            Failure::IntoItself.to_string(),
            "A folder cannot be pasted into itself."
        );
        let e = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(
            Failure::NotRemoved(e)
                .to_string()
                .starts_with("copied, but the original could not be removed: ")
        );
        let left = |cause| Failure::PartLeft {
            cause,
            part: PathBuf::from("/d/.p"),
            error: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert_eq!(
            left(None).to_string(),
            "the partial copy “/d/.p” could not be removed: permission denied"
        );
        assert!(
            left(Some(io::Error::from(io::ErrorKind::NotFound)))
                .to_string()
                .ends_with("; the partial copy “/d/.p” could not be removed: permission denied")
        );
    }
}
