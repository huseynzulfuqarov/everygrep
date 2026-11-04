//! Pasting files into a folder: copying or moving them.
//!
//! By default nothing is replaced. When a name is taken in the folder, the item
//! gets the first free name of the form `name (2).ext`, `name (3).ext`, …
//! A copy is written under a temporary name in the target folder and gets
//! its real name only when it is complete, so a failure or a cancel leaves
//! no partial item (when the partial copy cannot be removed, the failure
//! names it). An existing entry under the temporary name is never touched.
//! A move is a rename when the folder is on the same file
//! system; otherwise the item is copied the same way and only the entries
//! that were copied are removed from the source afterwards, so the source
//! is never deleted when the move fails; an entry changed or replaced since
//! it was read for the copy stays too. Links are copied as links; special
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

/// A decision applies to one top-level item, except SkipAll (this paste only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Overwrite,
    Skip,
    SkipAll,
    KeepBoth,
    Retry,
    Cancel,
}

#[derive(Clone, Debug)]
pub struct Question {
    pub source: PathBuf,
    pub target: PathBuf,
    pub error: Option<String>,
    pub can_overwrite: bool,
    pub source_details: String,
    pub target_details: String,
    pub trash_available: bool,
}

pub type Resolver = Box<dyn Fn(Question, &Progress) -> Decision + Send>;

/// Desktop trash operations are replaceable in tests; tests only move temp files.
#[derive(Clone, Copy)]
pub struct Replacement {
    available: fn(&Path) -> bool,
    trash: fn(&Path) -> io::Result<()>,
}
#[cfg(test)]
impl Replacement {
    pub fn with_trash(trash: fn(&Path) -> io::Result<()>) -> Self {
        Self {
            available: |_| true,
            trash,
        }
    }
}
impl Default for Replacement {
    fn default() -> Self {
        Self {
            available: |path| {
                std::process::Command::new("gio")
                    .args(["info", "-a", "access::can-trash", "--"])
                    .arg(path)
                    .env("LC_ALL", "C")
                    .stdin(std::process::Stdio::null())
                    .output()
                    .is_ok_and(|out| {
                        out.status.success()
                            && String::from_utf8_lossy(&out.stdout)
                                .lines()
                                .any(|line| line.trim() == "access::can-trash: TRUE")
                    })
            },
            trash: |path| crate::actions::trash(&[path.to_owned()]).map_err(io::Error::other),
        }
    }
}
#[cfg(test)]
pub fn test_replacement() -> Replacement {
    Replacement {
        available: |_| true,
        trash: |path| {
            let bin = path.parent().unwrap().parent().unwrap().join("test-trash");
            fs::create_dir_all(&bin)?;
            rename_strict(path, &temp_name(&bin))
        },
    }
}
fn details(meta: &Metadata) -> String {
    let ns = meta
        .mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec());
    let date = crate::format::timestamp(ns, &everygrep_core::localtime::Zone::local());
    if meta.is_dir() {
        fill(tr("Modified: {}"), &[&date])
    } else {
        fill(tr("Size: {0} bytes; modified: {1}"), &[&meta.len(), &date])
    }
}

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
    pub records: Vec<Record>,
    /// Visible paths or metadata changed, including failed publication.
    pub changed: bool,
    pub history_omitted: bool,
    /// Older undo steps were removed because this paste was not recorded.
    pub history_cleared: bool,
    /// The new items, in the order of the sources.
    pub done: Vec<PathBuf>,
    /// How many of them got another name because theirs was taken.
    pub renamed: usize,
    pub replaced: usize,
    /// Items moved into the folder they are already in: left as they are.
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: Vec<(PathBuf, Failure)>,
    /// Special files left out of copied folders.
    pub skipped_special: u64,
    /// The paste was cancelled; the item in progress was not pasted and the
    /// rest were not tried.
    pub cancelled: bool,
}

/// Successful source/destination pairs, including whether data was replaced.
#[derive(Debug)]
pub struct Record {
    pub source: PathBuf,
    pub target: PathBuf,
    pub overwritten: bool,
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
    paste_with(op, sources, dest, progress, None)
}

/// An optional resolver runs on the worker, never on the window thread.
pub fn paste_with(
    op: Operation,
    sources: &[PathBuf],
    dest: &Path,
    progress: &Progress,
    resolver: Option<Resolver>,
) -> Outcome {
    paste_observed(op, sources, dest, progress, resolver, |_| {})
}

/// Capture each published item's identity before another source can replace it.
pub fn paste_observed(
    op: Operation,
    sources: &[PathBuf],
    dest: &Path,
    progress: &Progress,
    resolver: Option<Resolver>,
    observe: impl FnMut(&Record),
) -> Outcome {
    paste_observed_using(
        op,
        sources,
        dest,
        progress,
        resolver,
        Replacement::default(),
        observe,
    )
}

pub fn paste_observed_using(
    op: Operation,
    sources: &[PathBuf],
    dest: &Path,
    progress: &Progress,
    resolver: Option<Resolver>,
    replacement: Replacement,
    mut observe: impl FnMut(&Record),
) -> Outcome {
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

    let mut run = Run {
        resolver,
        replacement,
        ..Run::default()
    };
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
                let record = Record {
                    source: src.clone(),
                    target: target.clone(),
                    overwritten: run.replaced,
                };
                observe(&record);
                out.records.push(record);
                out.done.push(target);
            }
            Ok(Pasted::Unchanged) => out.unchanged += 1,
            Ok(Pasted::Skipped) => out.skipped += 1,
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
    out.changed = run.changed
        || run.replacements > 0
        || !out.records.is_empty()
        || out
            .failed
            .iter()
            .any(|(_, failure)| matches!(failure, Failure::PartLeft { .. }));
    out.replaced = run.replacements;
    out.skipped_special = run.special;
    out
}

/// What one paste keeps from item to item.
#[derive(Default)]
struct Run {
    changed: bool,
    /// Special files left out so far.
    special: u64,
    resolver: Option<Resolver>,
    skip_all: bool,
    skip_errors: bool,
    replace: Option<Copied>,
    replaced: bool,
    replacements: usize,
    replacement: Replacement,
    exact: Option<PathBuf>,
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
    Skipped,
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
    loop {
        match paste_attempt(op, src, dest, progress, run) {
            Err(Stop::Io(e)) if run.resolver.is_some() => {
                if run.skip_errors {
                    return Ok(Pasted::Failed(Failure::Io(e)));
                }
                let q = Question {
                    source: src.into(),
                    target: dest.join(src.file_name().unwrap_or_default()),
                    error: Some(e.to_string()),
                    can_overwrite: false,
                    source_details: String::new(),
                    target_details: String::new(),
                    trash_available: false,
                };
                match run.resolver.as_ref().expect("resolver")(q, progress) {
                    Decision::Retry => continue,
                    Decision::SkipAll => {
                        run.skip_errors = true;
                        return Ok(Pasted::Failed(Failure::Io(e)));
                    }
                    Decision::Skip => return Ok(Pasted::Failed(Failure::Io(e))),
                    _ => return Err(Stop::Cancelled),
                }
            }
            result => return result,
        }
    }
}

fn paste_attempt(
    op: Operation,
    src: &Path,
    dest: &Path,
    progress: &Progress,
    run: &mut Run,
) -> Result<Pasted, Stop> {
    run.replace = None;
    run.replaced = false;
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
    let target = dest.join(name);
    if let Some(resolver) = &run.resolver {
        if let Ok(existing) = fs::symlink_metadata(&target) {
            if run.skip_all {
                return Ok(Pasted::Skipped);
            }
            let trash_available = (run.replacement.available)(&target);
            let can_overwrite = trash_available
                && meta.is_file()
                && existing.is_file()
                && (meta.dev(), meta.ino()) != (existing.dev(), existing.ino());
            let q = Question {
                source: src.into(),
                target: target.clone(),
                error: None,
                can_overwrite,
                source_details: details(&meta),
                target_details: details(&existing),
                trash_available,
            };
            match resolver(q, progress) {
                Decision::Overwrite if can_overwrite => {
                    run.replace = Some(Copied::new(&target, &existing))
                }
                Decision::KeepBoth => {}
                Decision::Retry => {
                    return Err(io::Error::other(tr("The destination already exists.")).into());
                }
                Decision::SkipAll => {
                    run.skip_all = true;
                    return Ok(Pasted::Skipped);
                }
                Decision::Skip => return Ok(Pasted::Skipped),
                _ => return Err(Stop::Cancelled),
            }
        }
    }
    if progress.is_cancelled() {
        return Err(Stop::Cancelled);
    }
    if run.resolver.is_some() && !Copied::new(src, &meta).unchanged(&fs::symlink_metadata(src)?) {
        return Err(io::Error::other(tr("The item changed since the recorded operation.")).into());
    }
    if op == Operation::Cut && run.replace.is_some() {
        if fs::metadata(dest)?.dev() == meta.dev()
            && let Some(pasted) = move_replace(src, &meta, dest, progress, run)?
        {
            return Ok(pasted);
        }
        let (target, copied) = copy_in(src, &meta, dest, name, progress, true, run)?;
        run.changed = true;
        return Ok(match remove_copied(&copied) {
            None => Pasted::New(target),
            Some(e) => Pasted::Failed(Failure::NotRemoved(e)),
        });
    }
    match op {
        Operation::Copy => {
            let (target, _) = copy_in(src, &meta, dest, name, progress, false, run)?;
            Ok(Pasted::New(target))
        }
        Operation::Cut => move_in(src, &meta, dest, name, progress, run),
    }
}

/// Stage the original inode on the destination filesystem before trashing
/// the checked target. No copy is needed; inode identity and permissions survive.
/// `None` when the rename crosses mounts of the same filesystem (EXDEV):
/// nothing was changed and the caller copies instead.
fn move_replace(
    src: &Path,
    meta: &Metadata,
    dest: &Path,
    progress: &Progress,
    run: &mut Run,
) -> Result<Option<Pasted>, Stop> {
    let expected = run.replace.take().expect("replacement target");
    let source = Copied::new(src, meta);
    let unchanged_target = || -> io::Result<()> {
        let now = fs::symlink_metadata(&expected.path)?;
        if !now.is_file() || !expected.unchanged(&now) {
            return Err(io::Error::other(tr(
                "The destination changed while copying. Retry to review it.",
            )));
        }
        Ok(())
    };
    unchanged_target()?;
    if !source.unchanged(&fs::symlink_metadata(src)?) {
        return Err(io::Error::other(tr("The copy source changed while copying.")).into());
    }
    let staged = match stage_for_replace(src, dest) {
        Ok(staged) => staged,
        Err(e) if e.raw_os_error() == Some(EXDEV) => {
            run.replace = Some(expected);
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    run.changed = true;
    let publish = (|| -> Result<(), Stop> {
        let now = fs::symlink_metadata(&staged)?;
        if now.dev() != meta.dev()
            || now.ino() != meta.ino()
            || now.len() != meta.len()
            || now.mode() != meta.mode()
            || (now.mtime(), now.mtime_nsec()) != (meta.mtime(), meta.mtime_nsec())
        {
            return Err(io::Error::other(tr("The copy source changed while copying.")).into());
        }
        if progress.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        unchanged_target()?;
        (run.replacement.trash)(&expected.path)?;
        run.replacements += 1;
        run.replaced = true;
        rename_strict(&staged, &expected.path).map_err(|e| io::Error::other(fill(tr("The previous file was sent to the trash, but the new file could not be published: {}"), &[&e])))?;
        Ok(())
    })();
    match publish {
        Ok(()) => Ok(Some(Pasted::New(expected.path))),
        Err(error) => {
            if let Err(restore) = rename_strict(&staged, src) {
                return Err(io::Error::other(fill(
                    tr("Could not restore the source; it remains at {0}: {1}"),
                    &[&staged.display(), &restore],
                ))
                .into());
            }
            Err(error)
        }
    }
}

/// `rename` error for another mount, even of the same filesystem.
const EXDEV: i32 = 18;

#[cfg(test)]
thread_local! {
    static FORCE_EXDEV: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn stage_for_replace(src: &Path, folder: &Path) -> io::Result<PathBuf> {
    #[cfg(test)]
    if FORCE_EXDEV.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(EXDEV));
    }
    stage_rename(src, folder)
}

/// Renames `src` to a free hidden name in `folder`. The staged entry is the
/// user's original, so its name does not look like a disposable part file.
pub(crate) fn stage_rename(src: &Path, folder: &Path) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..TEMP_TRIES {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let temp = folder.join(format!(".everygrep-moving-{}-{n}", std::process::id()));
        match rename_strict(src, &temp) {
            Ok(()) => return Ok(temp),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
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
    run.changed = true;
    match remove_copied(&copied) {
        None => Ok(Pasted::New(target)),
        Some(e) => Ok(Pasted::Failed(Failure::NotRemoved(e))),
    }
}

/// A source entry that was copied, as it was when it was read.
#[derive(Debug)]
struct Copied {
    path: PathBuf,
    is_dir: bool,
    dev: u64,
    ino: u64,
    len: u64,
    /// Modification and status change times, in seconds and nanoseconds.
    times: [(i64, i64); 2],
}

impl Copied {
    fn new(path: &Path, meta: &Metadata) -> Self {
        Self {
            path: path.to_owned(),
            is_dir: meta.is_dir(),
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            times: [
                (meta.mtime(), meta.mtime_nsec()),
                (meta.ctime(), meta.ctime_nsec()),
            ],
        }
    }

    /// Whether `now` is still the entry that was copied. A folder only has
    /// to be the same folder: whether it is empty is up to its removal.
    fn unchanged(&self, now: &Metadata) -> bool {
        let same = now.dev() == self.dev && now.ino() == self.ino;
        if self.is_dir {
            return same && now.is_dir();
        }
        let now = Copied::new(&self.path, now);
        same && now.len == self.len && now.times == self.times
    }
}

/// Removes the source entries that were copied, deepest first. A folder
/// that still holds something (a special file, or a file added meanwhile)
/// stays, and so does an entry that was changed or replaced after it was
/// read for the copy. Returns the first error.
fn remove_copied(copied: &[Copied]) -> Option<io::Error> {
    let mut first_error = None;
    for c in copied.iter().rev() {
        let removed = match fs::symlink_metadata(&c.path) {
            Ok(now) if !c.unchanged(&now) => Err(io::Error::other(fill(
                tr("“{}” changed after it was copied"),
                &[&c.path.display()],
            ))),
            Ok(_) if c.is_dir => fs::remove_dir(&c.path),
            Ok(_) => fs::remove_file(&c.path),
            Err(e) => Err(e),
        };
        if let Err(e) = removed {
            first_error.get_or_insert(e);
        }
    }
    first_error
}

/// Copies `src` into `dest` under a temporary name, then gives it the first
/// free name. Returns the new path and the source entries copied, parents
/// before their contents.
fn copy_in(
    src: &Path,
    meta: &Metadata,
    dest: &Path,
    name: &OsStr,
    progress: &Progress,
    sync: bool,
    run: &mut Run,
) -> Result<(PathBuf, Vec<Copied>), Stop> {
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
    if let Some(target) = run.exact.take() {
        // Exact-path redo must publish the source that was checked, even
        // when another process writes to it while the copy is running.
        for entry in &copied {
            let stable = fs::symlink_metadata(&entry.path).is_ok_and(|m| {
                let now = Copied::new(&entry.path, &m);
                now.dev == entry.dev
                    && now.ino == entry.ino
                    && now.len == entry.len
                    && now.times == entry.times
                    && now.is_dir == entry.is_dir
            });
            if !stable {
                return Err(cleaned_up(
                    &temp,
                    io::Error::other(tr("The copy source changed while copying.")).into(),
                ));
            }
        }
        return match rename_strict(&temp, &target) {
            Ok(()) => Ok((target, copied)),
            Err(e) => Err(cleaned_up(&temp, e.into())),
        };
    }
    if let Some(expected) = run.replace.take() {
        let mut publish = || -> io::Result<()> {
            let now = fs::symlink_metadata(&expected.path)?;
            if !now.is_file() || !expected.unchanged(&now) {
                return Err(io::Error::other(tr(
                    "The destination changed while copying. Retry to review it.",
                )));
            }
            (run.replacement.trash)(&expected.path)?;
            run.replacements += 1;
            run.replaced = true;
            rename_strict(&temp, &expected.path).map_err(|e| io::Error::other(fill(
                tr("The previous file was sent to the trash, but the new file could not be published: {}"), &[&e],
            )))
        };
        return match publish() {
            Ok(()) => {
                run.replaced = true;
                Ok((expected.path, copied))
            }
            Err(e) => Err(cleaned_up(&temp, e.into())),
        };
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

/// Recreate a recorded copy at its exact path, with no replacement fallback.
pub fn copy_exact(src: &Path, target: &Path, progress: &Progress) -> io::Result<()> {
    let parent = target.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = target.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() && fs::canonicalize(parent)?.starts_with(fs::canonicalize(src)?) {
        return Err(io::Error::other(tr(
            "A folder cannot be pasted into itself.",
        )));
    }
    let mut run = Run {
        exact: Some(target.into()),
        ..Run::default()
    };
    copy_in(src, &meta, parent, name, progress, false, &mut run)
        .map(|_| ())
        .map_err(|e| match e {
            Stop::Io(e) => e,
            Stop::Cancelled => io::Error::other(tr("Operation cancelled.")),
            Stop::PartLeft { part, error, .. } => io::Error::other(fill(
                tr("the partial copy “{}” could not be removed: {}"),
                &[&part.display(), &error],
            )),
        })
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
    copied: &'a mut Vec<Copied>,
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
            self.copied.push(Copied::new(src, meta));
        } else if kind.is_file() {
            self.file(src, meta, dst)?;
            self.copied.push(Copied::new(src, meta));
        } else if kind.is_dir() {
            fs::create_dir(dst)?;
            self.made = true;
            self.copied.push(Copied::new(src, meta));
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

/// [`free_name`] from the first number on, for a new item's name.
pub(crate) fn first_free(dir: &Path, name: &OsStr, is_dir: bool) -> io::Result<PathBuf> {
    free_name(dir, name, is_dir, &mut 2)
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
/// replacing. On file systems without this kind of rename (NFS, SMB, many
/// FUSE file systems) a file or link gets a new hard link and loses its old
/// name, which never replaces either; a folder (or a file where hard links
/// are not supported) gets a check and a plain rename, which could replace
/// an item created in between.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_noreplace_with(from, to, rename_strict)
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
    if fs::symlink_metadata(from).is_ok_and(|m| !m.is_dir()) {
        // `link` does not follow a link at `from` and fails when `to` exists.
        match fs::hard_link(from, to) {
            Ok(()) => {
                return fs::remove_file(from).inspect_err(|_| {
                    let _ = fs::remove_file(to);
                });
            }
            Err(e) if !no_hard_links(&e) => return Err(e),
            Err(_) => {}
        }
    }
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(from, to)
}

/// Whether `e` from `link` means another way is needed: EPERM or
/// EOPNOTSUPP where the file system has no hard links (some FUSE file
/// systems and SMB shares), EMLINK when the file has too many.
fn no_hard_links(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(1 | 95 | 31)) || e.kind() == io::ErrorKind::Unsupported
}

/// `renameat2` with `RENAME_NOREPLACE`: atomic, never replaces the target.
pub(crate) fn rename_strict(from: &Path, to: &Path) -> io::Result<()> {
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

    fn paste_with(
        op: Operation,
        sources: &[PathBuf],
        dest: &Path,
        progress: &Progress,
        resolver: Option<Resolver>,
    ) -> Outcome {
        paste_observed_using(
            op,
            sources,
            dest,
            progress,
            resolver,
            test_replacement(),
            |_| {},
        )
    }

    #[test]
    fn folder_question_details_do_not_claim_a_byte_size() {
        let dir = TempDir::new("folder-details");
        let folder = details(&fs::symlink_metadata(&dir.0).unwrap());
        assert!(!folder.contains("Size:"));
        let file = dir.path("file");
        fs::write(&file, b"abc").unwrap();
        assert!(details(&fs::symlink_metadata(file).unwrap()).contains("3 bytes"));
    }

    #[test]
    fn unavailable_or_failed_trash_never_permanently_replaces_the_target() {
        let t = TempDir::new("trash-failure");
        let src = t.path("src/x");
        let dest = t.path("dst");
        write(&src, "new");
        write(&dest.join("x"), "old");
        let unavailable = Replacement {
            available: |_| false,
            ..test_replacement()
        };
        let out = paste_observed_using(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
            Some(Box::new(|q, _| {
                assert!(!q.trash_available);
                assert!(!q.can_overwrite);
                assert!(q.source_details.contains("3 bytes"));
                assert!(q.target_details.contains("3 bytes"));
                Decision::Overwrite
            })),
            unavailable,
            |_| {},
        );
        assert!(out.cancelled);
        assert_eq!(read(&dest.join("x")), "old");
        let fails = Replacement {
            trash: |_| Err(io::Error::other("trash unavailable")),
            ..test_replacement()
        };
        let out = paste_observed_using(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
            Some(Box::new(|q, _| {
                if q.error.is_some() {
                    Decision::Skip
                } else {
                    Decision::Overwrite
                }
            })),
            fails,
            |_| {},
        );
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.replaced, 0);
        assert_eq!(read(&src), "new");
        assert_eq!(read(&dest.join("x")), "old");
        assert_eq!(names(&dest), ["x"]);
    }

    /// A fresh empty folder under the system temp dir, removed on drop.
    #[test]
    fn conflict_choices_preserve_or_replace_complete_files() {
        for decision in [
            Decision::KeepBoth,
            Decision::Skip,
            Decision::Overwrite,
            Decision::Cancel,
        ] {
            let t = TempDir::new("conflict");
            let src = t.0.join("src");
            let dst = t.0.join("dst");
            fs::create_dir(&src).unwrap();
            fs::create_dir(&dst).unwrap();
            fs::write(src.join("x"), b"new").unwrap();
            fs::write(dst.join("x"), b"old").unwrap();
            let out = paste_with(
                Operation::Copy,
                &[src.join("x")],
                &dst,
                &Progress::new(),
                Some(Box::new(move |q, _| {
                    assert!(q.can_overwrite);
                    decision
                })),
            );
            assert_eq!(
                fs::read(dst.join("x")).unwrap(),
                if decision == Decision::Overwrite {
                    b"new"
                } else {
                    b"old"
                }
            );
            assert_eq!(
                out.done.len(),
                usize::from(matches!(decision, Decision::KeepBoth | Decision::Overwrite))
            );
            assert_eq!(out.skipped, usize::from(decision == Decision::Skip));
            assert_eq!(out.cancelled, decision == Decision::Cancel);
            assert_eq!(fs::read(src.join("x")).unwrap(), b"new");
            if decision == Decision::Overwrite {
                assert_eq!(out.replaced, 1);
                let bin = t.path("test-trash");
                let old = fs::read_dir(bin).unwrap().next().unwrap().unwrap().path();
                assert_eq!(read(&old), "old");
                let (message, error) = crate::labels::pasted(Operation::Copy, &out, "dst");
                assert!(!error);
                assert!(message.contains("trash"));
            }
            if decision == Decision::KeepBoth {
                assert_eq!(fs::read(dst.join("x (2)")).unwrap(), b"new");
            }
            assert!(!fs::read_dir(&dst).unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .as_bytes()
                    .starts_with(b".everygrep-")
            }));
        }
    }

    #[test]
    fn skip_all_is_scoped_to_conflicts_and_retry_can_recover() {
        let t = TempDir::new("skip-retry");
        let src = t.0.join("src");
        let dst = t.0.join("dst");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        for n in ["a", "b", "c"] {
            fs::write(src.join(n), n).unwrap();
        }
        for n in ["a", "b"] {
            fs::write(dst.join(n), "old").unwrap();
        }
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let out = paste_with(
            Operation::Cut,
            &[src.join("a"), src.join("b"), src.join("c")],
            &dst,
            &Progress::new(),
            Some(Box::new(move |_, _| {
                seen.fetch_add(1, Ordering::Relaxed);
                Decision::SkipAll
            })),
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(out.skipped, 2);
        assert_eq!(out.done, [dst.join("c")]);
        let missing = src.join("missing");
        let create = missing.clone();
        let out = paste_with(
            Operation::Copy,
            &[missing],
            &dst,
            &Progress::new(),
            Some(Box::new(move |q, _| {
                assert!(q.error.is_some());
                fs::write(&create, "recovered").unwrap();
                Decision::Retry
            })),
        );
        assert_eq!(out.done, [dst.join("missing")]);
    }

    #[test]
    fn skip_all_conflicts_still_reports_io_errors_and_their_reason() {
        let t = TempDir::new("skip-errors");
        let src = t.path("src");
        let dst = t.path("dst");
        write(&src.join("a"), "new");
        write(&dst.join("a"), "old");
        let denied = src.join("b");
        write(&denied, "private");
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
        let out = paste_with(
            Operation::Copy,
            &[src.join("a"), denied, src.join("missing")],
            &dst,
            &Progress::new(),
            Some(Box::new(|_, _| Decision::SkipAll)),
        );
        assert_eq!(out.skipped, 1);
        assert!(!out.failed.is_empty());
        if let Some((_, Failure::Io(e))) = out.failed.iter().find(|(p, _)| p.ends_with("b")) {
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        }
        assert!(out.failed.iter().any(|(p, _)| p.ends_with("missing")));
        let (text, error) = crate::labels::pasted(Operation::Copy, &out, "dst");
        assert!(error);
        assert!(text.contains(&out.failed[0].1.to_string()));
    }

    #[test]
    fn folders_links_and_same_inode_cannot_be_overwritten() {
        let t = TempDir::new("no-overwrite");
        let src = t.0.join("src");
        let dst = t.0.join("dst");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::create_dir(src.join("folder")).unwrap();
        fs::create_dir(dst.join("folder")).unwrap();
        fs::write(src.join("file"), "safe").unwrap();
        std::os::unix::fs::symlink(src.join("file"), dst.join("file")).unwrap();
        fs::hard_link(src.join("file"), dst.join("hard")).unwrap();
        fs::hard_link(src.join("file"), src.join("hard")).unwrap();
        for name in ["folder", "file", "hard"] {
            let out = paste_with(
                Operation::Copy,
                &[src.join(name)],
                &dst,
                &Progress::new(),
                Some(Box::new(|q, _| {
                    assert!(!q.can_overwrite);
                    Decision::Overwrite
                })),
            );
            assert!(out.cancelled);
            assert!(out.done.is_empty());
        }
        assert_eq!(fs::read(src.join("file")).unwrap(), b"safe");
    }

    #[test]

    fn failed_replacement_keeps_a_new_source_entry_and_names_the_staged_original() {
        let t = TempDir::new("move-replace-rollback");
        let source = t.0.join("source");
        let dest = t.0.join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "original").unwrap();
        fs::write(dest.join("source"), "old target").unwrap();
        let replacement = Replacement {
            available: |_| true,
            trash: |target| {
                fs::write(
                    target.parent().unwrap().parent().unwrap().join("source"),
                    "new occupant",
                )?;
                Err(io::Error::other("injected trash failure"))
            },
        };
        let out = paste_observed_using(
            Operation::Cut,
            std::slice::from_ref(&source),
            &dest,
            &Progress::new(),
            Some(Box::new(|q, _| {
                if q.error.is_some() {
                    Decision::Skip
                } else {
                    Decision::Overwrite
                }
            })),
            replacement,
            |_| {},
        );
        assert_eq!(fs::read(&source).unwrap(), b"new occupant");
        assert_eq!(fs::read(dest.join("source")).unwrap(), b"old target");
        assert_eq!(out.failed.len(), 1);
        let staged = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.file_name().unwrap() != "source")
            .unwrap();
        assert_eq!(fs::read(&staged).unwrap(), b"original");
        assert!(format!("{:?}", out.failed).contains(&staged.display().to_string()));
    }

    #[test]
    fn cut_replace_across_mounts_of_one_filesystem_falls_back_to_copying() {
        let t = TempDir::new("move-replace-exdev");
        let src = t.0.join("source");
        let dest = t.0.join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&src, "new").unwrap();
        fs::write(dest.join("source"), "old").unwrap();
        let inode = fs::metadata(&src).unwrap().ino();
        FORCE_EXDEV.with(|f| f.set(true));
        let out = paste_observed_using(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
            Some(Box::new(|_, _| Decision::Overwrite)),
            test_replacement(),
            |_| {},
        );
        FORCE_EXDEV.with(|f| f.set(false));
        assert_eq!(out.done, [dest.join("source")]);
        assert!(out.failed.is_empty());
        assert_eq!(out.replaced, 1);
        assert!(!src.exists());
        assert_eq!(fs::read(dest.join("source")).unwrap(), b"new");
        assert_ne!(fs::metadata(dest.join("source")).unwrap().ino(), inode);
        let trashed: Vec<_> = fs::read_dir(t.0.join("test-trash")).unwrap().collect();
        assert_eq!(trashed.len(), 1);
    }

    #[test]
    fn same_filesystem_cut_replace_preserves_the_inode_and_hard_links() {
        let t = TempDir::new("move-replace-inode");
        let src = t.0.join("source");
        let dest = t.0.join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&src, "new").unwrap();
        fs::hard_link(&src, t.0.join("link")).unwrap();
        fs::write(dest.join("source"), "old").unwrap();
        let inode = fs::metadata(&src).unwrap().ino();
        let out = paste_observed_using(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
            Some(Box::new(|_, _| Decision::Overwrite)),
            test_replacement(),
            |_| {},
        );
        assert_eq!(out.done, [dest.join("source")]);
        assert!(!src.exists());
        assert_eq!(fs::metadata(dest.join("source")).unwrap().ino(), inode);
        assert_eq!(fs::metadata(t.0.join("link")).unwrap().ino(), inode);
        fs::write(dest.join("source"), "updated").unwrap();
        assert_eq!(fs::read(t.0.join("link")).unwrap(), b"updated");
    }

    #[test]
    fn overwrite_rechecks_the_destination_and_moves_only_after_publication() {
        let t = TempDir::new("replace-changed");
        let src = t.0.join("src");
        let dst = t.0.join("dst");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("x"), "source").unwrap();
        fs::write(dst.join("x"), "old").unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = paste_with(
            Operation::Cut,
            &[src.join("x")],
            &dst,
            &Progress::new(),
            Some(Box::new(move |q, _| {
                if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    fs::write(q.target, "changed destination").unwrap();
                    Decision::Overwrite
                } else {
                    assert!(q.error.is_some());
                    Decision::Skip
                }
            })),
        );
        assert!(out.done.is_empty());
        assert_eq!(out.failed.len(), 1);
        assert_eq!(fs::read(dst.join("x")).unwrap(), b"changed destination");
        assert!(src.join("x").exists());
        let out = paste_with(
            Operation::Cut,
            &[src.join("x")],
            &dst,
            &Progress::new(),
            Some(Box::new(|_, _| Decision::Overwrite)),
        );
        assert_eq!(out.done, [dst.join("x")]);
        assert!(!src.join("x").exists());
        assert_eq!(fs::read(dst.join("x")).unwrap(), b"source");
    }

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
        let entries: Vec<_> = copied.iter().map(|c| (c.path.clone(), c.is_dir)).collect();
        assert_eq!(entries, [(src.clone(), true), (src.join("f"), false)]);
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
    fn a_move_by_copy_keeps_a_source_changed_after_it_was_copied() {
        let t = TempDir::new("move-changed");
        let src = t.path("src/tree");
        write(&src.join("f"), "v1");
        write(&src.join("g"), "g");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let meta = fs::symlink_metadata(&src).unwrap();
        let mut run = Run::default();
        let (_, copied) = copy_in(
            &src,
            &meta,
            &dest,
            OsStr::new("tree"),
            &Progress::new(),
            true,
            &mut run,
        )
        .unwrap();
        // Another program writes a file after it was copied.
        write(&src.join("f"), "v2, longer");
        let e = remove_copied(&copied).expect("the change is reported");
        assert!(e.to_string().contains("changed after it was copied"), "{e}");
        assert_eq!(read(&src.join("f")), "v2, longer");
        assert_eq!(read(&dest.join("tree/f")), "v1");
        // The unchanged file still goes.
        assert_eq!(names(&src), ["f"]);
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
        // A folder is checked and renamed.
        fs::create_dir(t.path("dir")).unwrap();
        let e = rename_noreplace_with(&t.path("dir"), &b, unsupported).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        rename_noreplace_with(&t.path("dir"), &t.path("dir2"), unsupported).unwrap();
        assert!(t.path("dir2").is_dir() && !t.path("dir").exists());
        // Other errors are reported as they are, without a second attempt.
        let denied = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(13));
        let e = rename_noreplace_with(&t.path("c"), &t.path("d"), denied).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(read(&t.path("c")), "a");
    }

    #[test]
    fn the_fallback_rename_of_a_file_never_replaces_one_that_arrives_meanwhile() {
        use std::sync::{Arc, Barrier};
        let t = TempDir::new("noreplace-race");
        let unsupported = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(22));
        let target = t.path("target");
        let mut replaced = 0;
        for round in 0..300 {
            let _ = fs::remove_file(&target);
            let barrier = Arc::new(Barrier::new(2));
            let threads: Vec<_> = ["a", "b"]
                .into_iter()
                .map(|who| {
                    let from = t.path(&format!("{who}-{round}"));
                    write(&from, who);
                    let (to, barrier) = (target.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        rename_noreplace_with(&from, &to, unsupported).is_ok()
                    })
                })
                .collect();
            let ok = threads
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|&ok| ok)
                .count();
            if ok == 2 {
                replaced += 1;
            }
        }
        assert_eq!(
            replaced, 0,
            "both renames succeeded, one replacing the other"
        );
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
