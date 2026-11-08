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
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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
    trash: fn(&Path, &Path, FileStamp) -> io::Result<()>,
}
#[cfg(test)]
impl Replacement {
    pub fn with_trash(trash: fn(&Path, &Path, FileStamp) -> io::Result<()>) -> Self {
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
            trash: |actual, original, stamp| {
                crate::actions::trash(actual, original, stamp).map_err(io::Error::other)
            },
        }
    }
}
#[cfg(test)]
pub fn test_replacement() -> Replacement {
    Replacement {
        available: |_| true,
        trash: |actual, path, stamp| {
            let bin = path.parent().unwrap().parent().unwrap().join("test-trash");
            fs::create_dir_all(&bin)?;
            rename_expected(actual, &temp_name(&bin), stamp).map(|_| ())
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
    pub(crate) produced: Option<Produced>,
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
#[cfg(test)]
pub fn paste(op: Operation, sources: &[PathBuf], dest: &Path, progress: &Progress) -> Outcome {
    paste_with(op, sources, dest, progress, None)
}

/// An optional resolver runs on the worker, never on the window thread.
#[cfg(test)]
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
#[cfg(test)]
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

#[cfg(test)]
pub fn paste_observed_using(
    op: Operation,
    sources: &[PathBuf],
    dest: &Path,
    progress: &Progress,
    resolver: Option<Resolver>,
    replacement: Replacement,
    observe: impl FnMut(&Record),
) -> Outcome {
    paste_chosen(
        op,
        sources,
        None,
        dest,
        progress,
        resolver,
        replacement,
        observe,
    )
}

/// [`paste_observed_using`] of the items `chosen` (when Cut or Copy was
/// used, the drag started or the folder chooser opened): a source that is
/// not one of them any more (another item was put at its path meanwhile) is
/// left alone and reported; with `None` every source is taken as it is.
#[allow(clippy::too_many_arguments)]
pub fn paste_chosen(
    op: Operation,
    sources: &[PathBuf],
    chosen: Option<&crate::actions::Chosen>,
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
    if let Some(Err(e)) = chosen.map(|c| c.check_folder(&dest)) {
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
        chosen: chosen.cloned(),
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
                let mut record = Record {
                    source: src.clone(),
                    target: target.clone(),
                    overwritten: run.replaced,
                    produced: run.produced.take(),
                };
                #[cfg(test)]
                crate::fileops_test::at("before-record", &target);
                observe(&record);
                // The observer captured the frozen proof while its inodes were
                // pinned. Do not keep a descriptor per file for the whole batch.
                record.produced = None;
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
    produced: Option<Produced>,
    /// The items chosen for this paste, if known (see [`paste_chosen`]).
    chosen: Option<crate::actions::Chosen>,
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
    run.produced = None;
    let meta = fs::symlink_metadata(src)?;
    if let Some(chosen) = &run.chosen
        && !chosen.holds(&meta, false)
    {
        return Ok(Pasted::Failed(Failure::Io(
            crate::actions::changed_since_chosen(),
        )));
    }
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
    if let Some(resolver) = &run.resolver
        && let Ok(existing) = fs::symlink_metadata(&target)
    {
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
                // What replaces another file must be the content chosen.
                if let Some(chosen) = &run.chosen
                    && !chosen.holds(&meta, true)
                {
                    return Ok(Pasted::Failed(Failure::Io(
                        crate::actions::changed_since_chosen(),
                    )));
                }
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
        let (target, copied, produced) = copy_in(src, &meta, dest, name, progress, true, run)?;
        run.changed = true;
        return Ok(match remove_copied(&copied, &produced) {
            None => {
                run.produced = Some(produced);
                Pasted::New(target)
            }
            Some(e) => Pasted::Failed(Failure::NotRemoved(e)),
        });
    }
    match op {
        Operation::Copy => {
            let (target, _, produced) = copy_in(src, &meta, dest, name, progress, false, run)?;
            run.produced = Some(produced);
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
        crate::fileops_undo::trash_file_checked(
            &expected.path,
            expected.stamp,
            &|actual, original, stamp| {
                (run.replacement.trash)(actual, original, stamp).map_err(|e| e.to_string())
            },
        )
        .map_err(io::Error::other)?;
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
    static COPY_IO_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static COPY_SYNC_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
        match rename_noreplace(src, &temp) {
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
        let key = (name.to_owned(), meta.is_dir());
        let counter = run.next.get(&key).copied();
        let target = run.free_name(dest, name, meta.is_dir())?;
        match move_rename(src, &target) {
            Ok(()) => return Ok(Pasted::New(target)),
            // Taken since we looked: try the next free name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                // The name was not used: the copy below may take it.
                match counter {
                    Some(n) => run.next.insert(key, n),
                    None => run.next.remove(&key),
                };
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let (target, copied, produced) = copy_in(src, meta, dest, name, progress, true, run)?;
    run.changed = true;
    match remove_copied(&copied, &produced) {
        None => {
            run.produced = Some(produced);
            Ok(Pasted::New(target))
        }
        Some(e) => Ok(Pasted::Failed(Failure::NotRemoved(e))),
    }
}

/// The rename of a plain move; tests can make it cross file systems.
fn move_rename(src: &Path, target: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FORCE_EXDEV.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(EXDEV));
    }
    rename_noreplace(src, target)
}

/// A source entry that was copied, as it was when it was read.
#[derive(Debug)]
struct Copied {
    path: PathBuf,
    is_dir: bool,
    metadata_error: Option<io::Error>,
    anchor: Option<SourceAnchor>,
    stamp: FileStamp,
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
            metadata_error: None,
            anchor: None,
            stamp: FileStamp::of(meta),
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

/// A source entry held while it is copied: its folder's descriptor, its
/// name there, and the entry itself, pinned. For a move, a file or link
/// below the top entry is let go once copied (so a big tree never needs a
/// descriptor per file); its device and inode stay in [`Copied`], and it is
/// opened again, checked against them, when it is verified for removal.
#[derive(Debug)]
struct SourceAnchor {
    parent: std::sync::Arc<File>,
    name: OsString,
    handle: Option<std::sync::Arc<File>>,
}
impl SourceAnchor {
    fn path(&self) -> PathBuf {
        fd_path(&self.parent).join(&self.name)
    }
    fn handle(&self) -> io::Result<&std::sync::Arc<File>> {
        self.handle.as_ref().ok_or_else(source_changed)
    }
    fn readable(&self) -> io::Result<File> {
        open_readable(&format!("/proc/self/fd/{}", self.handle()?.as_raw_fd()), 0)
    }
}
/// Opens `path` for reading without waiting (O_NONBLOCK) and without
/// changing its access time where allowed; `extra` flags are added.
fn open_readable(path: &str, extra: i32) -> io::Result<File> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(0o1000000 | 0o4000 | extra)
        .open(path)
    {
        Err(e) if e.raw_os_error() == Some(1) => OpenOptions::new()
            .read(true)
            .custom_flags(0o4000 | extra)
            .open(path),
        result => result,
    }
}
fn source_changed() -> io::Error {
    io::Error::other(tr("The copy source changed while copying."))
}
impl Copied {
    fn pinned(path: &Path, meta: &Metadata) -> io::Result<Self> {
        let parent = std::sync::Arc::new(open_dir(
            path.parent()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
        )?);
        Self::pinned_in(parent, path, meta)
    }
    /// Pins the entry named like `path` in the open folder `parent`. It
    /// must be the entry `meta` describes; a link is held as the link.
    fn pinned_in(parent: std::sync::Arc<File>, path: &Path, meta: &Metadata) -> io::Result<Self> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
            .to_owned();
        let handle = std::sync::Arc::new(
            OpenOptions::new()
                .read(true)
                .custom_flags(0o10000000 | 0o400000)
                .open(fd_path(&parent).join(&name))?,
        );
        if FileStamp::of(&handle.metadata()?) != FileStamp::of(meta) {
            return Err(source_changed());
        }
        let mut copied = Self::new(path, meta);
        copied.anchor = Some(SourceAnchor {
            parent,
            name,
            handle: Some(handle),
        });
        Ok(copied)
    }
    fn check_before_removal(&self) -> io::Result<()> {
        let anchor = self.anchor.as_ref().ok_or_else(source_changed)?;
        let now = fs::symlink_metadata(anchor.path())?;
        let held = match &anchor.handle {
            Some(handle) => FileStamp::of(&handle.metadata()?),
            None => self.stamp,
        };
        if FileStamp::of(&now) != self.stamp || held != self.stamp {
            return Err(io::Error::other(fill(
                tr("“{}” changed after it was copied"),
                &[&self.path.display()],
            )));
        }
        if now.is_file() && now.nlink() > 1 {
            return Err(io::Error::other(fill(
                tr("“{}” changed after it was copied"),
                &[&self.path.display()],
            )));
        }
        Ok(())
    }
    fn check_held(&self, path: &Path, renamed: bool) -> io::Result<()> {
        let now = FileStamp::of(&fs::symlink_metadata(path)?);
        let expected = self.stamp;
        let same = if self.is_dir {
            (now.dev, now.ino, now.mode, now.uid, now.gid)
                == (
                    expected.dev,
                    expected.ino,
                    expected.mode,
                    expected.uid,
                    expected.gid,
                )
        } else if renamed {
            expected.same_except_ctime(&now)
        } else {
            expected == now
        };
        if same { Ok(()) } else { Err(source_changed()) }
    }
    /// Checks the source entry, held aside at `held`, against its copy.
    fn verify_held_bytes(&self, produced: &ProducedNode, held: &Path) -> io::Result<()> {
        let anchor = self.anchor.as_ref().ok_or_else(source_changed)?;
        if self.is_dir || produced.entry.digest.is_some() {
            let file = match &anchor.handle {
                Some(_) => anchor.readable()?,
                None => {
                    // Let go after it was copied: opened again (never
                    // through a link) and checked to be the same inode.
                    let path = held.to_str().ok_or_else(source_changed)?;
                    let file = open_readable(path, 0o400000)?;
                    let m = file.metadata()?;
                    if (m.dev(), m.ino()) != (self.dev, self.ino) {
                        return Err(source_changed());
                    }
                    file
                }
            };
            let target = produced.entry.open().map_err(|_| source_changed())?;
            if !crate::transfer_metadata::same_attributes(&file, &target)? {
                return Err(source_changed());
            }
            if let Some(digest) = produced.entry.digest
                && everygrep_core::hash::sha256_open_file(&file, &Progress::new()).ok()
                    != Some(digest)
            {
                return Err(source_changed());
            }
        }
        Ok(())
    }
}

fn source_ancestors(
    copied: &[Copied],
    parents: &[Option<usize>],
    root: &Isolation,
    index: usize,
) -> io::Result<()> {
    if let Some(parent) = parents[index] {
        source_ancestors(copied, parents, root, parent)?;
    }
    let entry = &copied[index];
    if index == 0 {
        entry.check_held(&root.path(), true)
    } else {
        entry.check_held(
            &entry.anchor.as_ref().ok_or_else(source_changed)?.path(),
            false,
        )
    }
}

fn remove_pinned_sources(copied: &[Copied], produced: &Produced) -> io::Result<()> {
    let Some(first) = copied.first() else {
        return Ok(());
    };
    #[cfg(test)]
    crate::fileops_test::note("source-cleanup", &first.path);
    // Validate the whole selected tree before making even the first change.
    for entry in copied {
        entry.check_before_removal()?;
    }
    produced.check()?;
    let mut paths = HashMap::new();
    let mut parents = Vec::with_capacity(copied.len());
    for (i, entry) in copied.iter().enumerate() {
        parents.push(entry.path.parent().and_then(|p| paths.get(p)).copied());
        if entry.is_dir {
            paths.insert(entry.path.clone(), i);
        }
    }
    let anchor = first.anchor.as_ref().ok_or_else(source_changed)?;
    let root = Isolation::take(
        anchor.parent.clone(),
        &anchor.name,
        &first.path,
        (first.dev, first.ino),
    )?;
    let mut root = Some(root);
    let mut failure = None;
    for index in (0..copied.len()).rev() {
        let entry = &copied[index];
        let checked = source_ancestors(copied, &parents, root.as_ref().unwrap(), index)
            .and_then(|()| produced.check_node(index))
            .and_then(|()| produced.nodes[index].entry.verify_bytes())
            .and_then(|()| produced.check_node(index));
        if let Err(e) = checked {
            failure = Some(e);
            break;
        }
        if index == 0 {
            if let Err(e) = entry.verify_held_bytes(
                &produced.nodes[index],
                &root.as_ref().ok_or_else(source_changed)?.path(),
            ) {
                failure = Some(e);
                break;
            }
            let held = root.take().unwrap();
            held.remove(entry.is_dir)?;
        } else {
            let anchor = entry.anchor.as_ref().ok_or_else(source_changed)?;
            let held = match Isolation::take(
                anchor.parent.clone(),
                &anchor.name,
                &entry.path,
                (entry.dev, entry.ino),
            ) {
                Ok(held) => held,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            let checked = entry
                .check_held(&held.path(), true)
                .and_then(|()| entry.verify_held_bytes(&produced.nodes[index], &held.path()))
                .and_then(|()| produced.check_node(index))
                .and_then(|()| produced.nodes[index].entry.verify_bytes());
            if let Err(e) = checked {
                failure = Some(held.restore_error(e));
                break;
            }
            if let Err(e) = held.remove(entry.is_dir) {
                failure = Some(e);
                break;
            }
        }
    }
    if let Some(error) = failure {
        return Err(root.as_ref().unwrap().restore_error(error));
    }
    Ok(())
}

/// Removes the source entries that were copied, deepest first. A folder
/// that still holds something (a special file, or a file added meanwhile)
/// stays, and so does an entry that was changed or replaced after it was
/// read for the copy. Returns the first error.
fn remove_copied(copied: &[Copied], produced: &Produced) -> Option<io::Error> {
    // Metadata is part of a move. Never remove even the first source entry
    // if any part of the copy could not preserve it.
    if let Some(c) = copied.iter().find(|c| c.metadata_error.is_some()) {
        return Some(io::Error::other(fill(
            tr("Cannot preserve metadata for “{}”: {}. Source kept."),
            &[&c.path.display(), c.metadata_error.as_ref().unwrap()],
        )));
    }
    if !produced.durable {
        return Some(unverified_copy());
    }
    remove_pinned_sources(copied, produced).err()
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
) -> Result<(PathBuf, Vec<Copied>, Produced), Stop> {
    let mut copied = Vec::new();
    let mut created = Vec::new();
    let mut buf = vec![0; CHUNK];
    let mut tries = 0;
    let temp = loop {
        let temp = temp_name(dest);
        let mut copier = Copier {
            progress,
            sync,
            buf: &mut buf,
            copied: &mut copied,
            created: &mut created,
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
            Err(stop) if copier.made => return Err(cleaned_up(&temp, &created, stop)),
            Err(stop) => return Err(stop),
        }
    };
    // Cancelled after the last read: the copy is not published (and a
    // move does not remove its source).
    if progress.is_cancelled() {
        return Err(cleaned_up(&temp, &created, Stop::Cancelled));
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
                    &created,
                    io::Error::other(tr("The copy source changed while copying.")).into(),
                ));
            }
        }
        let mut published = false;
        return match publish_created(
            &created,
            &target,
            progress,
            sync,
            &mut run.changed,
            &mut published,
        ) {
            Ok(produced) => Ok((target, copied, produced)),
            Err(e) => Err(publication_failed(
                &temp, &created, &target, e, published, progress,
            )),
        };
    }
    if let Some(expected) = run.replace.take() {
        let mut published = false;
        let mut publish = || -> io::Result<Produced> {
            // Verify our completed inode before removing the consented target.
            let _ = Produced::before_publication(&created, progress)?;
            let now = fs::symlink_metadata(&expected.path)?;
            if !now.is_file() || !expected.unchanged(&now) {
                return Err(io::Error::other(tr(
                    "The destination changed while copying. Retry to review it.",
                )));
            }
            crate::fileops_undo::trash_file_checked(
                &expected.path,
                expected.stamp,
                &|actual, original, stamp| {
                    (run.replacement.trash)(actual, original, stamp).map_err(|e| e.to_string())
                },
            )
            .map_err(io::Error::other)?;
            run.replacements += 1;
            run.replaced = true;
            publish_created(&created, &expected.path, progress, sync, &mut run.changed, &mut published)
                .map_err(|e| io::Error::other(fill(
                    tr("The previous file was sent to the trash, but the new file could not be published: {}"), &[&e],
                )))
        };
        return match publish() {
            Ok(produced) => {
                run.replaced = true;
                Ok((expected.path, copied, produced))
            }
            Err(e) => Err(publication_failed(
                &temp,
                &created,
                &expected.path,
                e,
                published,
                progress,
            )),
        };
    }
    loop {
        let target = match run.free_name(dest, name, meta.is_dir()) {
            Ok(t) => t,
            Err(e) => return Err(cleaned_up(&temp, &created, e.into())),
        };
        let mut published = false;
        match publish_created(
            &created,
            &target,
            progress,
            sync,
            &mut run.changed,
            &mut published,
        ) {
            Ok(produced) => return Ok((target, copied, produced)),
            Err(e) if !published && e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(publication_failed(
                    &temp, &created, &target, e, published, progress,
                ));
            }
        }
    }
}

/// Recreate a recorded copy at its exact path, with no replacement fallback.
pub fn copy_exact(
    src: &Path,
    target: &Path,
    progress: &Progress,
) -> io::Result<Vec<(PathBuf, FileStamp)>> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
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
        .map(|(_, _, proof)| proof.snapshot())
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

/// Each entry belongs to this copy only while its device and inode match.
/// Parent descriptors prevent a renamed/replaced ancestor redirecting cleanup.
/// Folders and the top entry stay open while the copy lives; a file or link
/// below is let go once written ([`release`](Self::release)) and opened
/// again through its folder's descriptor when it is checked, so a big tree
/// never needs a descriptor per file.
#[derive(Clone, Debug)]
struct Created {
    path: PathBuf,
    parent: std::sync::Arc<File>,
    name: OsString,
    dev: u64,
    ino: u64,
    is_dir: bool,
    is_link: bool,
    handle: Option<std::sync::Arc<File>>,
    /// Whether [`handle`](Self::handle) was let go: it is opened again.
    released: bool,
    digest: Option<[u8; 32]>,
}
impl Created {
    fn new(
        path: &Path,
        parent: std::sync::Arc<File>,
        meta: &Metadata,
        handle: Option<std::sync::Arc<File>>,
    ) -> io::Result<Self> {
        Ok(Self {
            path: path.into(),
            parent,
            name: path
                .file_name()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
                .to_owned(),
            dev: meta.dev(),
            ino: meta.ino(),
            is_dir: meta.is_dir(),
            is_link: meta.is_symlink(),
            handle,
            released: false,
            digest: None,
        })
    }
    fn anchored(&self) -> PathBuf {
        fd_path(&self.parent).join(&self.name)
    }
    /// Lets the descriptor of a written file or link go (not of a folder,
    /// which holds others, and not of a file its owner could not open
    /// again for reading).
    fn release(&mut self) {
        let readable = self
            .handle
            .as_ref()
            .and_then(|h| h.metadata().ok())
            .is_some_and(|m| self.is_link || m.mode() & 0o400 != 0);
        if !self.is_dir && readable {
            self.handle = None;
            self.released = true;
        }
    }
    /// The entry, open: the held descriptor, or once let go, the entry of
    /// that name in the held folder, which must still be the same inode.
    fn open(&self) -> io::Result<std::sync::Arc<File>> {
        if let Some(handle) = &self.handle {
            return Ok(handle.clone());
        }
        if !self.released {
            return Err(unverified_copy());
        }
        // O_PATH | O_NOFOLLOW for a link; O_NOFOLLOW | O_NONBLOCK |
        // O_NOATIME for a file, so nothing else is followed or waited on.
        let flags = if self.is_link {
            0o10000000 | 0o400000
        } else {
            0o400000 | 0o4000 | 0o1000000
        };
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(self.anchored())?;
        let m = file.metadata()?;
        if (m.dev(), m.ino(), m.is_dir()) != (self.dev, self.ino, self.is_dir) {
            return Err(io::Error::other(tr(
                "The temporary copy changed. Unrecognized items were kept.",
            )));
        }
        Ok(std::sync::Arc::new(file))
    }
    fn check(&self) -> io::Result<()> {
        let m = fs::symlink_metadata(self.anchored())?;
        if (m.dev(), m.ino(), m.is_dir()) != (self.dev, self.ino, self.is_dir) {
            return Err(io::Error::other(tr(
                "The temporary copy changed. Unrecognized items were kept.",
            )));
        }
        Ok(())
    }
    fn verify_bytes(&self) -> io::Result<()> {
        if self.digest.is_none() {
            return Ok(());
        }
        let handle = self.open()?;
        self.verify_bytes_of(&handle)
    }
    /// [`verify_bytes`](Self::verify_bytes) through `handle`, this entry
    /// open.
    fn verify_bytes_of(&self, handle: &File) -> io::Result<()> {
        if let Some(digest) = self.digest
            && everygrep_core::hash::sha256_open_file(handle, &Progress::new()).ok() != Some(digest)
        {
            return Err(unverified_copy());
        }
        Ok(())
    }
    fn unlink(&self) -> io::Result<()> {
        self.check()?;
        // Opened before it is moved aside, so it is checked again as the
        // same inode wherever its name is then.
        let handle = match self.digest {
            Some(_) => Some(self.open()?),
            None => None,
        };
        if let Some(handle) = &handle {
            self.verify_bytes_of(handle)?;
        }
        let held = Isolation::take(
            self.parent.clone(),
            &self.name,
            &self.path,
            (self.dev, self.ino),
        )?;
        if let Some(handle) = &handle
            && let Err(e) = self.verify_bytes_of(handle)
        {
            return Err(held.restore_error(e));
        }
        held.remove(self.is_dir)
    }
}
pub(crate) fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/.", file.as_raw_fd()))
}
pub(crate) fn require_private(file: &File) -> io::Result<()> {
    let meta = file.metadata()?;
    // SAFETY: geteuid has no arguments and cannot fail.
    let owner = unsafe { geteuid() };
    if meta.mode() & 0o077 != 0 || meta.uid() != owner {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            tr("This filesystem cannot keep temporary data private. The source was kept."),
        ));
    }
    Ok(())
}
pub(crate) fn pin_path(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(0o10000000 | 0o400000)
        .open(path)
}
pub(crate) fn open_dir(path: &Path) -> io::Result<File> {
    // Path::parent normalizes the trailing dot in our descriptor paths. Only
    // this process's numeric fd links may be followed to reopen the held dir.
    let descriptor = path.parent() == Some(Path::new("/proc/self/fd"))
        && path.file_name().is_some_and(|n| {
            !n.as_bytes().is_empty() && n.as_bytes().iter().all(u8::is_ascii_digit)
        });
    let actual = if descriptor {
        path.join(".")
    } else {
        path.to_owned()
    };
    OpenOptions::new()
        .read(true)
        .custom_flags(0o200000 | 0o400000)
        .open(actual)
}

/// A completed copy is bound to the descriptors used to write it. Each
/// child's parent descriptor and relative name remain part of that proof.
#[derive(Debug)]
pub(crate) struct Produced {
    nodes: Vec<ProducedNode>,
    durable: bool,
}
#[derive(Debug)]
struct ProducedNode {
    entry: Created,
    expected: FileStamp,
    parent_index: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) len: u64,
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mtime: (i64, i64),
    pub(crate) ctime: (i64, i64),
}
impl FileStamp {
    pub(crate) fn read(path: &Path) -> io::Result<Self> {
        Ok(Self::of(&fs::symlink_metadata(path)?))
    }

    pub(crate) fn of(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
    pub(crate) fn same_except_ctime(&self, other: &Self) -> bool {
        let mut other = *other;
        other.ctime = self.ctime;
        other == *self
    }
}
impl Produced {
    pub(crate) fn snapshot(&self) -> Vec<(PathBuf, FileStamp)> {
        let root = &self.nodes[0].entry.path;
        let mut out: Vec<(PathBuf, FileStamp)> = self
            .nodes
            .iter()
            .map(|n| {
                (
                    n.entry
                        .path
                        .strip_prefix(root)
                        .expect("copy manifest root")
                        .into(),
                    n.expected,
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn before_publication(created: &[Created], progress: &Progress) -> io::Result<Self> {
        let mut nodes: Vec<ProducedNode> = Vec::with_capacity(created.len());
        let mut directories = HashMap::new();
        for entry in created {
            entry.check()?;
            let handle = entry.open()?;
            let meta = handle.metadata()?;
            let expected = FileStamp::of(&meta);
            if let Some(digest) = entry.digest {
                if everygrep_core::hash::sha256_open_file(&handle, progress).ok() != Some(digest) {
                    return Err(unverified_copy());
                }
                // A verification read must not change the preserved access time
                // on filesystems that ignore O_NOATIME.
                handle.set_times(
                    fs::FileTimes::new()
                        .set_accessed(meta.accessed()?)
                        .set_modified(meta.modified()?),
                )?;
            }
            let parent_index = entry
                .path
                .parent()
                .and_then(|p| directories.get(p))
                .copied();
            if entry.is_dir {
                directories.insert(entry.path.clone(), nodes.len());
            }
            nodes.push(ProducedNode {
                entry: entry.clone(),
                expected: if entry.digest.is_some() {
                    FileStamp::of(&handle.metadata()?)
                } else {
                    expected
                },
                parent_index,
            });
        }
        Ok(Self {
            nodes,
            durable: false,
        })
    }
    fn check_node(&self, index: usize) -> io::Result<()> {
        let node = &self.nodes[index];
        if let Some(parent) = node.parent_index {
            self.check_node(parent)?;
        }
        let m = fs::symlink_metadata(node.entry.anchored())?;
        if FileStamp::of(&m) != node.expected {
            return Err(unverified_copy());
        }
        Ok(())
    }
    fn check(&self) -> io::Result<()> {
        for index in 0..self.nodes.len() {
            self.check_node(index)?;
        }
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if COPY_SYNC_ERROR.with(|v| v.replace(false)) {
            return Err(io::Error::from_raw_os_error(5));
        }
        // Deepest first: each folder is flushed after every entry in it,
        // and the folder holding the published name last.
        for node in self.nodes.iter().rev() {
            let handle = node.entry.open()?;
            if !handle.metadata()?.is_symlink() {
                handle.sync_all()?;
                #[cfg(test)]
                crate::fileops_test::note(
                    "fsync",
                    &fd_target(&node.entry.parent).join(&node.entry.name),
                );
            }
        }
        self.nodes[0].entry.parent.sync_all()?;
        #[cfg(test)]
        crate::fileops_test::note("fsync", &fd_target(&self.nodes[0].entry.parent));
        self.check()?;
        self.durable = true;
        Ok(())
    }
}
/// Where an open descriptor's entry is now, for test logs.
#[cfg(test)]
fn fd_target(file: &File) -> PathBuf {
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap_or_default()
}
fn unverified_copy() -> io::Error {
    io::Error::other(tr(
        "The completed copy could not be verified. The source was kept.",
    ))
}

/// Publish a regular file using its open descriptor; the source of linkat is
/// the inode we wrote, even if its old temporary name was replaced. Folders
/// and symlinks use held parent descriptors and are checked again afterwards.
fn publish_created(
    created: &[Created],
    target: &Path,
    progress: &Progress,
    sync: bool,
    changed: &mut bool,
    published: &mut bool,
) -> io::Result<Produced> {
    let mut produced = Produced::before_publication(created, progress)?;
    let root = produced.nodes.first_mut().ok_or_else(unverified_copy)?;
    let target_parent = std::sync::Arc::new(open_dir(
        target
            .parent()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
    )?);
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    root.entry.check()?;
    let handle = root.entry.handle.as_ref().ok_or_else(unverified_copy)?;
    if handle.metadata()?.is_file() {
        let source = CString::new(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
        let target_name = CString::new(name.as_bytes())?;
        // SAFETY: the proc fd path names our live descriptor; AT_SYMLINK_FOLLOW
        // follows that proc link, not a mutable source pathname. linkat refuses
        // an existing destination atomically.
        let rc = unsafe {
            linkat(
                AT_FDCWD,
                source.as_ptr(),
                target_parent.as_raw_fd(),
                target_name.as_ptr(),
                0x400,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        *changed = true;
        *published = true;
        // Remove only our temporary name. A replacement stays and makes this
        // operation fail without deleting the move source.
        root.entry.unlink()?;
    } else {
        rename_at(&root.entry.parent, &root.entry.name, &target_parent, name)?;
        *changed = true;
        *published = true;
    }
    root.entry.parent = target_parent;
    root.entry.name = name.into();
    // Our link/rename changes ctime, but cannot authorize different contents,
    // ownership, permissions or modification time on the held inode.
    let after = FileStamp::of(&handle.metadata()?);
    if !root.expected.same_except_ctime(&after) {
        return Err(unverified_copy());
    }
    root.expected = after;
    #[cfg(test)]
    crate::fileops_test::at("copy-published", target);
    produced.check()?;
    if sync {
        produced.sync()?;
    }
    Ok(produced)
}

fn publication_failed(
    temp: &Path,
    created: &[Created],
    target: &Path,
    error: io::Error,
    published: bool,
    progress: &Progress,
) -> Stop {
    if published {
        return io::Error::other(fill(
            tr("The copy at “{}” could not be verified: {}. The source was kept."),
            &[&target.display(), &error],
        ))
        .into();
    }
    let stop = if progress.is_cancelled() {
        Stop::Cancelled
    } else {
        error.into()
    };
    cleaned_up(temp, created, stop)
}
/// Bind a no-replace namespace move to the checked source inode. A surprise
/// object taken in the final lookup gap is rolled back without overwriting.
pub(crate) fn rename_expected(
    from: &Path,
    to: &Path,
    expected: FileStamp,
) -> io::Result<FileStamp> {
    let old_parent = open_dir(from.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let new_parent = open_dir(to.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let old = from.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let new = to.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let pinned = pin_path(&fd_path(&old_parent).join(old))?;
    if FileStamp::of(&pinned.metadata()?) != expected
        || FileStamp::read(&fd_path(&old_parent).join(old))? != expected
    {
        return Err(source_changed());
    }
    #[cfg(test)]
    crate::fileops_test::at("rename-checked", from);
    rename_at(&old_parent, old, &new_parent, new)?;
    let moved = FileStamp::read(&fd_path(&new_parent).join(new))?;
    if !expected.same_except_ctime(&moved) {
        if let Err(e) = rename_at(&new_parent, new, &old_parent, old) {
            return Err(io::Error::other(fill(
                tr("Could not restore the source; it remains at {0}: {1}"),
                &[&to.display(), &e],
            )));
        }
        return Err(source_changed());
    }
    Ok(moved)
}

/// A private holding directory narrows the check-to-unlink window: an entry
/// taken from the visible name is verified again before it can be removed.
pub(crate) struct Isolation {
    parent: std::sync::Arc<File>,
    name: OsString,
    folder: std::sync::Arc<File>,
    folder_name: OsString,
    folder_id: (u64, u64),
    logical: PathBuf,
    expected: (u64, u64),
}
fn same_inode(meta: &Metadata, id: (u64, u64)) -> bool {
    (meta.dev(), meta.ino()) == id
}
pub(crate) fn rename_at(from: &File, old: &OsStr, to: &File, new: &OsStr) -> io::Result<()> {
    let a = CString::new(old.as_bytes())?;
    let b = CString::new(new.as_bytes())?;
    rename_noreplace_with(Path::new(old), Path::new(new), |_, _| {
        // SAFETY: live directory descriptors and valid single-component names.
        let rc = unsafe {
            renameat2(
                from.as_raw_fd(),
                a.as_ptr(),
                to.as_raw_fd(),
                b.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
}
pub(crate) fn unlink_at(parent: &File, name: &OsStr, directory: bool) -> io::Result<()> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: live directory descriptor, valid name, no recursive removal.
    let rc = unsafe {
        unlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            if directory { 0x200 } else { 0 },
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
impl Isolation {
    pub(crate) fn take(
        parent: std::sync::Arc<File>,
        name: &OsStr,
        logical: &Path,
        expected: (u64, u64),
    ) -> io::Result<Self> {
        use std::hash::BuildHasher;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let visible = fd_path(&parent).join(name);
        if !same_inode(&fs::symlink_metadata(&visible)?, expected) {
            return Err(io::Error::other(tr(
                "The item changed since the recorded operation.",
            )));
        }
        let mut tries = 0;
        let (folder_name, folder, folder_id) = loop {
            if tries >= TEMP_TRIES {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            tries += 1;
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let random = std::collections::hash_map::RandomState::new().hash_one(n);
            let cell = OsString::from(format!(
                ".everygrep-held-{}-{random:016x}",
                std::process::id()
            ));
            let path = fd_path(&parent).join(&cell);
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    #[cfg(test)]
                    crate::fileops_test::at("holding-created", &path);
                    let made = fs::symlink_metadata(&path)?;
                    let file = std::sync::Arc::new(open_dir(&path)?);
                    if !same_inode(&file.metadata()?, (made.dev(), made.ino())) {
                        return Err(io::Error::other(tr(
                            "The temporary copy changed. Unrecognized items were kept.",
                        )));
                    }
                    if let Err(error) = require_private(&file) {
                        if fs::symlink_metadata(&path)
                            .is_ok_and(|m| same_inode(&m, (made.dev(), made.ino())))
                        {
                            let _ = unlink_at(&parent, &cell, true);
                        }
                        return Err(error);
                    }
                    break (cell, file, (made.dev(), made.ino()));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let held = Self {
            parent,
            name: name.into(),
            folder,
            folder_name,
            folder_id,
            logical: logical.into(),
            expected,
        };
        let checked = fs::symlink_metadata(&visible).and_then(|m| {
            if same_inode(&m, expected) {
                Ok(())
            } else {
                Err(io::Error::other(tr(
                    "The item changed since the recorded operation.",
                )))
            }
        });
        if let Err(e) = checked {
            let _ = held.clear_folder();
            return Err(e);
        }
        #[cfg(test)]
        crate::fileops_test::at("isolate-checked", logical);
        if let Err(e) = rename_at(&held.parent, &held.name, &held.folder, OsStr::new("item")) {
            let _ = held.clear_folder();
            return Err(e);
        }
        #[cfg(test)]
        crate::fileops_test::at("isolate-taken", logical);
        if let Err(e) = held.check() {
            return Err(held.restore_error(e));
        }
        Ok(held)
    }
    pub(crate) fn path(&self) -> PathBuf {
        fd_path(&self.folder).join("item")
    }
    pub(crate) fn recovery_path(&self) -> PathBuf {
        fs::read_link(format!("/proc/self/fd/{}", self.folder.as_raw_fd()))
            .unwrap_or_else(|_| {
                self.logical
                    .parent()
                    .unwrap_or(Path::new("/"))
                    .join(&self.folder_name)
            })
            .join("item")
    }
    fn check(&self) -> io::Result<()> {
        if !same_inode(&fs::symlink_metadata(self.path())?, self.expected) {
            return Err(io::Error::other(tr(
                "The item changed since the recorded operation.",
            )));
        }
        Ok(())
    }
    pub(crate) fn clear_folder(&self) -> io::Result<()> {
        let path = fd_path(&self.parent).join(&self.folder_name);
        if !same_inode(&fs::symlink_metadata(path)?, self.folder_id) {
            return Err(io::Error::other(tr(
                "The temporary copy changed. Unrecognized items were kept.",
            )));
        }
        unlink_at(&self.parent, &self.folder_name, true)
    }
    pub(crate) fn restore(&self) -> io::Result<()> {
        // Even a foreign item accidentally taken in the rename gap is kept.
        rename_at(&self.folder, OsStr::new("item"), &self.parent, &self.name)?;
        self.clear_folder()
    }
    pub(crate) fn restore_error(&self, error: io::Error) -> io::Error {
        match self.restore() {
            Ok(()) => error,
            Err(restore) => io::Error::other(fill(
                tr("Could not restore the source; it remains at {0}: {1}"),
                &[&self.recovery_path().display(), &restore],
            )),
        }
    }
    pub(crate) fn remove(self, directory: bool) -> io::Result<()> {
        if let Err(e) = self
            .check()
            .and_then(|()| unlink_at(&self.folder, OsStr::new("item"), directory))
        {
            return Err(self.restore_error(e));
        }
        self.clear_folder()
    }
}

fn cleaned_up(temp: &Path, created: &[Created], stop: Stop) -> Stop {
    match remove_partial(created) {
        Ok(()) => stop,
        Err(error) => Stop::PartLeft {
            cause: Box::new(stop),
            part: temp.to_owned(),
            error,
        },
    }
}

/// Remove only the recorded entries, never recursively remove a path tree.
fn remove_partial(created: &[Created]) -> io::Result<()> {
    let Some(root) = created.first() else {
        return Ok(());
    };
    match root.check() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(handle) = &root.handle {
                if handle.metadata()?.nlink() == 0 {
                    return Ok(());
                }
                let moved = fs::read_link(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
                return Err(io::Error::other(fill(
                    tr("The partial copy moved; it remains at {}."),
                    &[&moved.display()],
                )));
            }
            return Err(e);
        }
        other => other?,
    }
    // Read-only modes applied to our directories do not prevent cleanup.
    // fchmod acts on our descriptor, never on a replacement at its old name.
    for entry in created.iter().filter(|e| e.is_dir) {
        entry.check()?;
        if let Some(dir) = &entry.handle {
            let mode = dir.metadata()?.mode();
            dir.set_permissions(fs::Permissions::from_mode(mode | 0o700))?;
        }
    }
    let mut first_error = None;
    for entry in created.iter().rev() {
        if let Err(e) = entry.unlink() {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

struct Copier<'a> {
    progress: &'a Progress,
    /// Flush every file to disk before it counts as copied (for moves,
    /// whose source is removed afterwards).
    sync: bool,
    buf: &'a mut Vec<u8>,
    copied: &'a mut Vec<Copied>,
    created: &'a mut Vec<Created>,
    special: &'a mut u64,
    /// Whether the copy's top entry was made, so there is something of
    /// ours to remove when it stops.
    made: bool,
}

impl Copier<'_> {
    fn output_parent(&self, dst: &Path) -> io::Result<std::sync::Arc<File>> {
        let parent = dst
            .parent()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        if let Some(created) = self.created.iter().find(|e| e.path == parent && e.is_dir) {
            return created.handle.clone().ok_or_else(|| {
                io::Error::other(tr("The temporary copy could not be verified. Source kept."))
            });
        }
        if !self.created.is_empty() {
            return Err(io::Error::other(tr(
                "The temporary copy could not be verified. Source kept.",
            )));
        }
        Ok(std::sync::Arc::new(open_dir(parent)?))
    }

    fn tree(&mut self, src: &Path, meta: &Metadata, dst: &Path) -> Result<(), Stop> {
        self.entry(None, src, meta, dst)
    }

    /// Copies the entry `src` (described by `meta`) to `dst`. Below the
    /// selected item, `parent` is the open source folder that holds it:
    /// every entry is opened relative to its folder's descriptor, never
    /// through a path, so a folder renamed or replaced by a link while the
    /// copy runs cannot lead it outside the selected tree. Only folders on
    /// the way down stay open.
    fn entry(
        &mut self,
        parent: Option<std::sync::Arc<File>>,
        src: &Path,
        meta: &Metadata,
        dst: &Path,
    ) -> Result<(), Stop> {
        if self.progress.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        #[cfg(test)]
        crate::fileops_test::at("copy-pin", src);
        let mut source = match parent {
            Some(parent) => Copied::pinned_in(parent, src, meta)?,
            None => Copied::pinned(src, meta)?,
        };
        #[cfg(test)]
        crate::fileops_test::at("copy-open", src);
        let anchor = source.anchor.as_ref().ok_or_else(source_changed)?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            // The target is read from the link that was pinned.
            let target = rustix::fs::readlinkat(&**anchor.handle()?, c"", Vec::new())
                .map_err(io::Error::from)?;
            let parent = self.output_parent(dst)?;
            let actual = fd_path(&parent).join(
                dst.file_name()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
            );
            std::os::unix::fs::symlink(OsStr::from_bytes(target.as_bytes()), &actual)?;
            self.made = true;
            let handle = std::sync::Arc::new(
                OpenOptions::new()
                    .read(true)
                    .custom_flags(0o10000000 | 0o400000)
                    .open(&actual)?,
            );
            let made = handle.metadata()?;
            self.created
                .push(Created::new(dst, parent, &made, Some(handle))?);
            if self.sync {
                source.metadata_error =
                    crate::transfer_metadata::link(&anchor.path(), &actual, meta).err();
            }
            self.release_last();
            self.keep(source);
        } else if kind.is_file() {
            let from = anchor.readable()?;
            source.metadata_error = self.file(src, meta, dst, from)?;
            self.release_last();
            self.keep(source);
        } else if kind.is_dir() {
            // The folder that was pinned, opened for reading through the
            // held descriptor: the name may point elsewhere by now.
            let dir = std::sync::Arc::new(open_dir(&fd_path(anchor.handle()?))?);
            let opened = dir.metadata()?;
            if !opened.is_dir() || (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
                return Err(source_changed().into());
            }
            let parent = self.output_parent(dst)?;
            let actual = fd_path(&parent).join(
                dst.file_name()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
            );
            fs::DirBuilder::new().mode(0o700).create(&actual)?;
            self.made = true;
            let made = fs::symlink_metadata(&actual)?;
            let created_index = self.created.len();
            self.created.push(Created::new(dst, parent, &made, None)?);
            let handle = std::sync::Arc::new(open_dir(&actual)?);
            let opened = handle.metadata()?;
            if (opened.dev(), opened.ino()) != (made.dev(), made.ino()) {
                return Err(io::Error::other(tr(
                    "The temporary copy changed. Unrecognized items were kept.",
                ))
                .into());
            }
            self.created[created_index].handle = Some(handle.clone());
            #[cfg(test)]
            crate::fileops_test::at("copy-created", &actual);
            require_private(&handle)?;
            let index = self.copied.len();
            self.keep(source);
            // Entries are listed and looked at through the open folder;
            // each is opened (and checked) relative to it.
            for entry in fs::read_dir(fd_path(&dir))? {
                let entry = entry?;
                let m = entry.metadata()?;
                let name = entry.file_name();
                self.entry(Some(dir.clone()), &src.join(&name), &m, &dst.join(&name))?;
            }
            if self.sync {
                self.copied[index].metadata_error =
                    crate::transfer_metadata::file(&dir, &handle, meta).err();
            } else {
                handle.set_permissions(fs::Permissions::from_mode(mode(meta)))?;
                if let Ok(t) = meta.modified() {
                    handle.set_modified(t)?;
                }
            }
        } else {
            // Never opened: a FIFO, socket or device is only counted.
            *self.special += 1;
        }
        Ok(())
    }

    /// Lets go of the descriptor of the file or link just written, unless it
    /// is the top entry (see [`Created::release`]).
    fn release_last(&mut self) {
        if self.created.len() > 1
            && let Some(last) = self.created.last_mut()
        {
            last.release();
        }
    }

    /// Records a copied source entry. Only a move needs the entry held
    /// until its source is removed; a copy lets it go, so a big tree does
    /// not keep a descriptor open per source entry.
    fn keep(&mut self, mut source: Copied) {
        if !self.sync {
            source.anchor = None;
        } else if !self.copied.is_empty()
            && !source.is_dir
            && let Some(anchor) = &mut source.anchor
        {
            anchor.handle = None;
        }
        self.copied.push(source);
    }

    /// Copies the regular file `src`, already open as `from` through its
    /// pinned descriptor, to `dst`.
    fn file(
        &mut self,
        src: &Path,
        meta: &Metadata,
        dst: &Path,
        mut from: File,
    ) -> Result<Option<io::Error>, Stop> {
        // Copying then unlinking aliases changes their shared ctime and
        // breaks the original hard-link relationship. Until that relationship
        // can be moved as one verified group, refuse before deleting anything.
        if self.sync && meta.nlink() > 1 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, tr(
                "Hard-linked files cannot be moved across filesystems safely. Use Copy; the source was kept.",
            )).into());
        }
        // The bytes must all come from one version of the source: the one
        // that was looked at, opened, and still the same once read in full.
        let seen = Copied::new(src, meta);
        if !seen.unchanged(&from.metadata()?) {
            return Err(source_changed().into());
        }
        let parent = self.output_parent(dst)?;
        let actual = fd_path(&parent).join(
            dst.file_name()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
        );
        let to = std::sync::Arc::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(0o1000000)
                .open(&actual)?,
        );
        self.made = true;
        self.created.push(Created::new(
            dst,
            parent,
            &to.metadata()?,
            Some(to.clone()),
        )?);
        #[cfg(test)]
        crate::fileops_test::at("copy-created", &actual);
        require_private(&to)?;
        let created_index = self.created.len() - 1;
        let mut digest = everygrep_core::hash::Sha256Stream::default();
        let mut total = 0u64;
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
            #[cfg(test)]
            crate::fileops_test::at("copy-read", src);
            #[cfg(test)]
            if COPY_IO_ERROR.with(|v| v.replace(false)) {
                return Err(io::Error::from_raw_os_error(28).into());
            }
            (&*to).write_all(&self.buf[..n])?;
            digest.update(&self.buf[..n]);
            self.progress.add(n as u64);
            total += n as u64;
        }
        if total != seen.len || !seen.unchanged(&from.metadata()?) {
            return Err(source_changed().into());
        }
        self.created[created_index].digest = Some(digest.finish());
        #[cfg(test)]
        crate::fileops_test::at("copy-written", &actual);
        let metadata_error = if self.sync {
            crate::transfer_metadata::file(&from, &to, meta).err()
        } else {
            to.set_permissions(fs::Permissions::from_mode(mode(meta)))?;
            if let Ok(t) = meta.modified() {
                to.set_modified(t)?;
            }
            None
        };
        if self.sync {
            to.sync_all()?;
        }
        Ok(metadata_error)
    }
}

/// The permission bits to give a copy: those of the source without
/// set-user-ID and set-group-ID.
fn mode(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o1777
}

/// Bytes of the files in `path`, links not followed. Folders are opened by
/// descriptor, one below the other (never through a link), like the copy
/// walk, so a folder swapped for a link meanwhile is not counted through.
/// Stops counting once the paste is cancelled.
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
    #[cfg(test)]
    crate::fileops_test::at("size-folder", path);
    let Ok(dir) = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return 0;
    };
    folder_size(dir, path, progress)
}

/// [`tree_size`] of the open folder `dir` (at `path`).
fn folder_size(dir: std::os::fd::OwnedFd, path: &Path, progress: &Progress) -> u64 {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let Ok(mut entries) = rustix::fs::Dir::new(dir) else {
        return 0;
    };
    let mut total = 0u64;
    while let Some(Ok(entry)) = entries.read() {
        if progress.is_cancelled() {
            break;
        }
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let Ok(fd) = entries.fd() else {
            break;
        };
        let Ok(st) = rustix::fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        let kind = rustix::fs::FileType::from_raw_mode(st.st_mode);
        if kind.is_file() {
            total = total.saturating_add(st.st_size as u64);
        } else if kind.is_dir() {
            let child = path.join(OsStr::from_bytes(name.to_bytes()));
            #[cfg(test)]
            crate::fileops_test::at("size-folder", &child);
            if let Ok(sub) = rustix::fs::openat(
                fd,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                total = total.saturating_add(folder_size(sub, &child, progress));
            }
        }
    }
    total
}

/// Why `sources` cannot be copied or moved into `dest` at all: `dest` is
/// not a folder, or is one of the folders or inside one. `None` when the
/// transfer can start (each item is still checked again as it goes).
pub fn refusal(sources: &[PathBuf], dest: &Path) -> Option<&'static str> {
    let Ok(dest) = fs::canonicalize(dest) else {
        return Some(tr("The destination folder no longer exists."));
    };
    if !dest.is_dir() {
        return Some(tr("The destination is not a folder."));
    }
    let into_itself = sources.iter().any(|s| {
        fs::symlink_metadata(s).is_ok_and(|m| m.is_dir())
            && fs::canonicalize(s).is_ok_and(|s| dest.starts_with(s))
    });
    into_itself.then(|| tr("A folder cannot be pasted into itself."))
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
    fn geteuid() -> u32;
    fn linkat(
        olddirfd: c_int,
        oldpath: *const c_char,
        newdirfd: c_int,
        newpath: *const c_char,
        flags: c_int,
    ) -> c_int;
    fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int;
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

/// Renames without replacement. If the filesystem cannot guarantee that
/// atomically, leave both paths alone. A check followed by plain rename (or
/// link followed by path-based unlink) cannot provide this guarantee.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_noreplace_with(from, to, rename_strict)
}

fn rename_noreplace_with(
    from: &Path,
    to: &Path,
    atomic: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    atomic(from, to).map_err(|e| {
        if matches!(e.raw_os_error(), Some(22 | 38 | 95)) || e.kind() == io::ErrorKind::Unsupported
        {
            io::Error::new(
                io::ErrorKind::Unsupported,
                tr("This filesystem cannot rename without replacement. Both items were kept."),
            )
        } else {
            e
        }
    })
}

/// `renameat2` with `RENAME_NOREPLACE`: atomic, never replaces the target.
pub(crate) fn rename_strict(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(errno) = RENAME_ERRNO.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(errno));
    }
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
thread_local! {
    pub(crate) static RENAME_ERRNO: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, symlink};

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
    fn refusal_names_missing_destinations_files_and_folders_into_themselves() {
        let t = TempDir::new("refusal");
        let a = t.path("a");
        let child = t.path("a/child");
        fs::create_dir_all(&child).unwrap();
        let file = t.path("file");
        fs::write(&file, "x").unwrap();
        let other = t.path("other");
        fs::create_dir(&other).unwrap();
        assert!(refusal(std::slice::from_ref(&a), &a).is_some());
        assert!(refusal(std::slice::from_ref(&a), &child).is_some());
        assert!(refusal(std::slice::from_ref(&a), &file).is_some());
        assert!(refusal(std::slice::from_ref(&a), &t.path("missing")).is_some());
        assert!(refusal(&[a.clone(), file.clone()], &other).is_none());
        // A link to the folder is the folder: refused through it too.
        symlink(&child, t.path("to-child")).unwrap();
        assert!(refusal(std::slice::from_ref(&a), &t.path("to-child")).is_some());
        // A link to a folder is moved as a link: the folder it points to is
        // a valid destination.
        symlink(&other, t.path("link")).unwrap();
        assert!(refusal(&[t.path("link")], &other).is_none());
        // A file into its own folder is no refusal (it gets a free name or
        // stays where it is).
        assert!(refusal(std::slice::from_ref(&file), &t.0).is_none());
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
            trash: |_, _, _| Err(io::Error::other("trash unavailable")),
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
            trash: |_actual, target, _stamp| {
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
    fn a_move_across_file_systems_copies_and_then_removes_only_the_source() {
        let t = TempDir::new("move-exdev");
        let src = t.path("tree");
        write(&src.join("a.txt"), "alpha");
        write(&src.join("sub/b.txt"), "beta");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        write(&dest.join("tree/keep"), "existing");
        FORCE_EXDEV.with(|f| f.set(true));
        let out = paste_with(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
            None,
        );
        FORCE_EXDEV.with(|f| f.set(false));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        // The name was taken: the copy got a free one, the old folder stays.
        assert_eq!(out.done, [dest.join("tree (2)")]);
        assert_eq!(read(&dest.join("tree (2)/a.txt")), "alpha");
        assert_eq!(read(&dest.join("tree (2)/sub/b.txt")), "beta");
        assert_eq!(read(&dest.join("tree/keep")), "existing");
        assert!(!src.exists());
    }

    /// The flushes of a move across file systems, in order, up to the
    /// removal of its source.
    fn flushes_of_a_move(src: &Path, dest: &Path) -> Vec<(&'static str, PathBuf)> {
        FORCE_EXDEV.with(|f| f.set(true));
        crate::fileops_test::start_log();
        let out = paste_with(
            Operation::Cut,
            &[src.to_path_buf()],
            dest,
            &Progress::new(),
            None,
        );
        let log = crate::fileops_test::take_log();
        FORCE_EXDEV.with(|f| f.set(false));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!src.exists());
        log
    }

    #[test]
    fn a_move_across_file_systems_flushes_every_copied_folder_before_removing_the_source() {
        let t = TempDir::new("move-exdev-flush");
        let src = t.path("tree");
        write(&src.join("a.txt"), "alpha");
        write(&src.join("sub/b.txt"), "beta");
        write(&src.join("sub/deep/c.txt"), "gamma");
        fs::create_dir(src.join("empty")).unwrap();
        symlink("a.txt", src.join("link")).unwrap();
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let log = flushes_of_a_move(&src, &dest);
        let dest = fs::canonicalize(&dest).unwrap();
        let top = dest.join("tree");
        let cleanup = log
            .iter()
            .position(|(event, _)| *event == "source-cleanup")
            .expect("the source is removed");
        let flushed = |path: &Path| {
            log.iter()
                .position(|(event, p)| *event == "fsync" && p == path)
                .unwrap_or_else(|| panic!("{} is never flushed: {log:?}", path.display()))
        };
        let folders = ["", "sub", "sub/deep", "empty"];
        let files = ["a.txt", "sub/b.txt", "sub/deep/c.txt"];
        for rel in folders.iter().chain(&files) {
            let path = if rel.is_empty() {
                top.clone()
            } else {
                top.join(rel)
            };
            let at = flushed(&path);
            assert!(at < cleanup, "{rel} flushed after the source went");
            // A folder is flushed after every entry in it.
            if let Some(parent) = path.parent().filter(|p| p.starts_with(&top)) {
                assert!(at < flushed(parent), "{rel} flushed after its folder");
            }
        }
        // The folder that holds the published name comes last.
        let last = flushed(&dest);
        assert!(last > flushed(&top) && last < cleanup);
        assert_eq!(
            log[..cleanup].iter().filter(|(e, _)| *e == "fsync").count(),
            folders.len() + files.len() + 1,
            "{log:?}"
        );
        assert_eq!(read(&top.join("sub/deep/c.txt")), "gamma");
    }

    #[test]
    fn a_file_moved_across_file_systems_and_its_new_folder_are_flushed_before_the_source_goes() {
        let t = TempDir::new("move-exdev-flush-file");
        let src = t.path("a.txt");
        write(&src, "alpha");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let log = flushes_of_a_move(&src, &dest);
        let dest = fs::canonicalize(&dest).unwrap();
        let events: Vec<_> = log.iter().map(|(e, p)| (*e, p.clone())).collect();
        assert_eq!(
            events,
            [
                ("fsync", dest.join("a.txt")),
                ("fsync", dest.clone()),
                ("source-cleanup", src.clone()),
            ]
        );
    }

    #[test]
    fn a_move_across_file_systems_into_an_unwritable_folder_keeps_the_source() {
        let t = TempDir::new("move-exdev-denied");
        let src = t.path("file.txt");
        write(&src, "data");
        let dest = t.path("locked");
        fs::create_dir(&dest).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o555)).unwrap();
        // Root may write anyway; the check is only meaningful otherwise.
        let writable = fs::write(dest.join("probe"), "").is_ok();
        FORCE_EXDEV.with(|f| f.set(true));
        let out = paste_with(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
            None,
        );
        FORCE_EXDEV.with(|f| f.set(false));
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
        if !writable {
            assert_eq!(out.failed.len(), 1);
            assert!(out.done.is_empty());
            assert_eq!(read(&src), "data");
            assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
        }
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
        let mut created = Vec::new();
        let meta = fs::symlink_metadata(&big).unwrap();
        let temp = temp_name(&dest);
        progress.cancel();
        let mut c = Copier {
            progress: &progress,
            sync: false,
            buf: &mut buf,
            copied: &mut copied,
            created: &mut created,
            special: &mut special,
            made: false,
        };
        // Past the check at the top of the tree: the file is made, and the
        // read loop sees the cancel.
        assert!(matches!(
            c.file(&big, &meta, &temp, File::open(&big).unwrap()),
            Err(Stop::Cancelled)
        ));
        assert!(c.made);
        remove_partial(&created).unwrap();
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
        let created = created_tree(&part);
        remove_partial(&created).unwrap();
        assert!(fs::symlink_metadata(&part).is_err(), "partial copy left");
        // Nothing there is not an error.
        remove_partial(&created).unwrap();
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
    fn moving_by_copy_keeps_the_whole_source_when_an_entry_is_added() {
        // The fallback for another file system, run directly: the source
        // keeps whatever was not copied.
        let t = TempDir::new("move-copy");
        let src = t.path("src/tree");
        write(&src.join("f"), "f");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let meta = fs::symlink_metadata(&src).unwrap();
        let mut run = Run::default();
        let (target, copied, produced) = copy_in(
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
        assert!(remove_copied(&copied, &produced).is_some());
        assert_eq!(names(&src), ["f", "late"]);
        assert_eq!(read(&dest.join("tree/f")), "f");
        // A fresh checked copy can move the unchanged tree normally.
        fs::remove_file(src.join("late")).unwrap();
        let next = t.path("next");
        fs::create_dir(&next).unwrap();
        let meta = fs::symlink_metadata(&src).unwrap();
        let (_, fresh, proof) = copy_in(
            &src,
            &meta,
            &next,
            OsStr::new("tree"),
            &Progress::new(),
            true,
            &mut Run::default(),
        )
        .unwrap();
        assert!(remove_copied(&fresh, &proof).is_none());
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
        let (_, copied, produced) = copy_in(
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
        let e = remove_copied(&copied, &produced).expect("the change is reported");
        assert!(e.to_string().contains("changed"), "{e}");
        assert_eq!(read(&src.join("f")), "v2, longer");
        assert_eq!(read(&dest.join("tree/f")), "v1");
        // A changed tree is kept in full for another review.
        assert_eq!(names(&src), ["f", "g"]);
    }

    #[test]
    fn cross_device_move_preserves_xattrs_permissions_and_times() {
        let t = TempDir::new("move-metadata");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        let file = src.join("secret");
        write(&file, "payload");
        fs::create_dir(&dest).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o2770)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o4700)).unwrap();
        crate::transfer_metadata::set_test_attr(&file, "user.audit-data", b"irreplaceable tag");
        crate::transfer_metadata::set_test_attr(&src, "user.folder-data", b"folder tag");
        let times = fs::FileTimes::new()
            .set_accessed(std::time::UNIX_EPOCH + std::time::Duration::new(1234567890, 123456789))
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(1234567891, 987654321));
        File::open(&file).unwrap().set_times(times).unwrap();
        let expected = fs::metadata(&file).unwrap();
        FORCE_EXDEV.with(|v| v.set(true));
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        FORCE_EXDEV.with(|v| v.set(false));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!src.exists());
        let target = dest.join("tree/secret");
        let now = fs::metadata(&target).unwrap();
        assert_eq!(now.mode() & 0o7777, 0o4700);
        assert_eq!(
            fs::metadata(dest.join("tree")).unwrap().mode() & 0o7777,
            0o2770
        );
        assert_eq!(
            (now.mtime(), now.mtime_nsec(), now.atime(), now.atime_nsec()),
            (
                expected.mtime(),
                expected.mtime_nsec(),
                expected.atime(),
                expected.atime_nsec()
            )
        );
        assert_eq!(
            crate::transfer_metadata::test_attrs(&target)[b"user.audit-data".as_slice()],
            b"irreplaceable tag"
        );
        assert_eq!(
            crate::transfer_metadata::test_attrs(&dest.join("tree"))
                [b"user.folder-data".as_slice()],
            b"folder tag"
        );
        assert_eq!(read(&target), "payload");
    }

    #[test]
    fn cross_device_move_preserves_access_and_default_acls() {
        let t = TempDir::new("move-acl");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("file"), "payload");
        fs::create_dir(&dest).unwrap();
        // Linux POSIX ACL xattr format: version, then tag/permissions/id.
        let mut acl = 2u32.to_le_bytes().to_vec();
        let named_uid = fs::metadata(&src).unwrap().uid();
        for (tag, perm, id) in [
            (1u16, 7u16, u32::MAX),
            (2, 4, named_uid),
            (4, 5, u32::MAX),
            (16, 5, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        for (path, name) in [
            (&src, "system.posix_acl_access"),
            (&src, "system.posix_acl_default"),
            (&src.join("file"), "system.posix_acl_access"),
        ] {
            crate::transfer_metadata::set_test_attr(path, name, &acl);
        }
        FORCE_EXDEV.with(|v| v.set(true));
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        FORCE_EXDEV.with(|v| v.set(false));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!src.exists());
        for (path, name) in [
            (dest.join("tree"), "system.posix_acl_access"),
            (dest.join("tree"), "system.posix_acl_default"),
            (dest.join("tree/file"), "system.posix_acl_access"),
        ] {
            assert_eq!(
                crate::transfer_metadata::test_attrs(&path)[name.as_bytes()],
                acl
            );
        }
    }

    #[test]
    fn metadata_failure_keeps_the_entire_move_source() {
        let t = TempDir::new("move-metadata-unsupported");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("a"), "first");
        write(&src.join("b"), "second");
        crate::transfer_metadata::set_test_attr(&src.join("b"), "user.required", b"tag");
        fs::create_dir(&dest).unwrap();
        FORCE_EXDEV.with(|v| v.set(true));
        crate::transfer_metadata::FAIL_SET.with(|v| v.set(true));
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        crate::transfer_metadata::FAIL_SET.with(|v| v.set(false));
        FORCE_EXDEV.with(|v| v.set(false));
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::NotRemoved(_))]
        ));
        assert!(out.failed[0].1.to_string().contains("Source kept"));
        assert_eq!(read(&src.join("a")), "first");
        assert_eq!(read(&src.join("b")), "second");
        assert_eq!(read(&dest.join("tree/a")), "first");
        assert_eq!(read(&dest.join("tree/b")), "second");
    }

    #[test]
    fn cross_device_move_preserves_symlink_metadata_without_following_it() {
        let t = TempDir::new("move-link-metadata");
        let src = t.path("src/link");
        let dest = t.path("dest");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::create_dir(&dest).unwrap();
        symlink("missing-target", &src).unwrap();
        crate::transfer_metadata::age_test_link(&src);
        let before = fs::symlink_metadata(&src).unwrap();
        FORCE_EXDEV.with(|v| v.set(true));
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        FORCE_EXDEV.with(|v| v.set(false));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let now = fs::symlink_metadata(dest.join("link")).unwrap();
        assert_eq!(
            (
                now.uid(),
                now.gid(),
                now.mtime(),
                now.mtime_nsec(),
                now.atime(),
                now.atime_nsec()
            ),
            (
                before.uid(),
                before.gid(),
                before.mtime(),
                before.mtime_nsec(),
                before.atime(),
                before.atime_nsec()
            )
        );
        assert!(fs::symlink_metadata(&src).is_err());
        assert_eq!(
            fs::read_link(dest.join("link")).unwrap(),
            Path::new("missing-target")
        );
    }

    #[test]
    fn a_cross_device_tree_with_hard_links_keeps_every_source_entry() {
        let t = TempDir::new("hardlink-move");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("a"), "same inode");
        fs::hard_link(src.join("a"), src.join("b")).unwrap();
        write(&src.join("plain"), "independent");
        fs::create_dir(&dest).unwrap();
        FORCE_EXDEV.with(|v| v.set(true));
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        FORCE_EXDEV.with(|v| v.set(false));
        assert!(out.done.is_empty());
        assert_eq!(out.failed.len(), 1);
        assert!(out.failed[0].1.to_string().contains("source was kept"));
        assert_eq!(names(&src), ["a", "b", "plain"]);
        assert_eq!(
            fs::metadata(src.join("a")).unwrap().ino(),
            fs::metadata(src.join("b")).unwrap().ino()
        );
        assert_eq!(read(&src.join("a")), "same inode");
        assert_eq!(read(&src.join("plain")), "independent");
        assert!(names(&dest).is_empty());
        // The ordinary Copy choice remains available.
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(read(&dest.join("tree/a")), "same inode");
        assert_eq!(read(&dest.join("tree/b")), "same inode");
    }

    #[test]
    fn same_filesystem_hard_link_moves_keep_the_relationship() {
        let t = TempDir::new("hardlink-same-fs");
        let src = t.path("src/a");
        let alias = t.path("src/b");
        let dest = t.path("dest");
        write(&src, "shared");
        fs::hard_link(&src, &alias).unwrap();
        fs::create_dir(&dest).unwrap();
        let out = paste(
            Operation::Cut,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!src.exists());
        assert_eq!(
            fs::metadata(dest.join("a")).unwrap().ino(),
            fs::metadata(&alias).unwrap().ino()
        );
        assert_eq!(read(&alias), "shared");
    }

    #[test]
    fn a_private_copy_is_restricted_before_the_first_byte_is_written() {
        let t = TempDir::new("private-in-progress");
        let src = t.path("src/secret");
        let dest = t.path("dest");
        write(&src, &"s".repeat(CHUNK * 2));
        fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir(&dest).unwrap();
        crate::fileops_test::arm("copy-created", |path| {
            let m = fs::symlink_metadata(path).unwrap();
            assert!(m.is_file());
            assert_eq!(m.len(), 0);
            assert_eq!(
                m.mode() & 0o077,
                0,
                "partial private data must not be readable by others"
            );
        });
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        crate::fileops_test::fired();
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(
            fs::metadata(dest.join("secret")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn staging_directories_start_private_and_get_the_final_mode() {
        let t = TempDir::new("private-directory");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("child"), "bytes");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
        fs::create_dir(&dest).unwrap();
        crate::fileops_test::arm("copy-created", |path| {
            let m = fs::symlink_metadata(path).unwrap();
            assert!(m.is_dir());
            assert_eq!(m.mode() & 0o077, 0);
        });
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        crate::fileops_test::fired();
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(
            fs::metadata(dest.join("tree")).unwrap().mode() & 0o777,
            0o750
        );
        assert_eq!(read(&dest.join("tree/child")), "bytes");
    }

    #[test]
    fn an_inherited_acl_cannot_make_the_partial_copy_readable() {
        let t = TempDir::new("private-inherited-acl");
        let src = t.path("src/secret");
        let dest = t.path("dest");
        write(&src, "bytes");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir(&dest).unwrap();
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (1u16, 7u16, u32::MAX),
            (2, 7, fs::metadata(&src).unwrap().uid()),
            (4, 7, u32::MAX),
            (16, 7, u32::MAX),
            (32, 7, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        crate::transfer_metadata::set_test_attr(&dest, "system.posix_acl_default", &acl);
        crate::fileops_test::arm("copy-created", |path| {
            // For an extended ACL the group mode bits are the effective mask
            // on every named user/group entry as well.
            assert_eq!(fs::symlink_metadata(path).unwrap().mode() & 0o077, 0);
        });
        let out = paste(Operation::Copy, &[src], &dest, &Progress::new());
        crate::fileops_test::fired();
        assert!(out.failed.is_empty(), "{:?}", out.failed);
    }

    #[test]
    fn an_io_error_keeps_a_foreign_replacement_during_cleanup() {
        let t = TempDir::new("cleanup-io-foreign");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let (d, parked) = (dest.clone(), t.path("parked-output"));
        crate::fileops_test::arm("copy-read", move |_| {
            let temp = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
            fs::rename(&temp, parked).unwrap();
            write(&temp.join("foreign/unique"), "unique bytes");
            COPY_IO_ERROR.with(|v| v.set(true));
        });
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        crate::fileops_test::fired();
        assert!(!out.cancelled);
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::PartLeft { cause: Some(_), .. })]
        ));
        let temp = fs::read_dir(&dest).unwrap().next().unwrap().unwrap().path();
        assert_eq!(read(&temp.join("foreign/unique")), "unique bytes");
        assert_eq!(fs::metadata(&src).unwrap().len(), (CHUNK * 2) as u64);
    }

    #[test]
    fn cleanup_rechecks_the_object_after_isolating_its_name() {
        let t = TempDir::new("cleanup-isolation-gap");
        let part = t.path("partial");
        let parked = t.path("parked");
        write(&part, "our partial copy");
        let made = created_tree(&part);
        let p = parked.clone();
        crate::fileops_test::arm("isolate-checked", move |path| {
            fs::rename(path, p).unwrap();
            write(path, "unique replacement after the check");
        });
        assert!(remove_partial(&made).is_err());
        crate::fileops_test::fired();
        assert_eq!(read(&part), "unique replacement after the check");
        assert_eq!(read(&parked), "our partial copy");
        assert_eq!(fs::read_dir(&t.0).unwrap().count(), 2);
    }

    #[test]
    fn cancellation_reports_a_partial_copy_moved_out_of_its_temp_name() {
        let t = TempDir::new("cleanup-relocated");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let progress = std::sync::Arc::new(Progress::new());
        let parked = t.path("parked-output");
        let (p, d, to) = (progress.clone(), dest.clone(), parked.clone());
        crate::fileops_test::arm("copy-read", move |_| {
            let temp = fs::read_dir(d).unwrap().next().unwrap().unwrap().path();
            fs::rename(temp, to).unwrap();
            p.cancel();
        });
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &progress,
        );
        crate::fileops_test::fired();
        assert!(out.cancelled);
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::PartLeft { .. })]
        ));
        assert!(
            out.failed[0]
                .1
                .to_string()
                .contains(&parked.display().to_string())
        );
        assert_eq!(fs::metadata(src).unwrap().len(), (CHUNK * 2) as u64);
        assert!(parked.exists());
    }

    fn created_tree(root: &Path) -> Vec<Created> {
        fn visit(path: &Path, out: &mut Vec<Created>) {
            let m = fs::symlink_metadata(path).unwrap();
            let parent = std::sync::Arc::new(open_dir(path.parent().unwrap()).unwrap());
            let handle = m
                .is_dir()
                .then(|| std::sync::Arc::new(open_dir(path).unwrap()));
            out.push(Created::new(path, parent, &m, handle).unwrap());
            if m.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    visit(&entry.unwrap().path(), out);
                }
            }
        }
        let mut out = Vec::new();
        visit(root, &mut out);
        out
    }

    #[test]
    fn cancel_keeps_a_foreign_directory_replacing_the_partial_file() {
        let t = TempDir::new("cleanup-foreign-root");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let progress = std::sync::Arc::new(Progress::new());
        let (p, d, parked) = (progress.clone(), dest.clone(), t.path("our-copy-parked"));
        crate::fileops_test::arm("copy-read", move |_| {
            let temp = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
            fs::rename(&temp, parked).unwrap();
            write(&temp.join("foreign-unique-data"), "never copied");
            p.cancel();
        });
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &progress,
        );
        crate::fileops_test::fired();
        assert!(out.cancelled);
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::PartLeft { .. })]
        ));
        let temp = fs::read_dir(&dest).unwrap().next().unwrap().unwrap().path();
        assert_eq!(read(&temp.join("foreign-unique-data")), "never copied");
        assert_eq!(fs::metadata(&src).unwrap().len(), (CHUNK * 2) as u64);
    }

    #[test]
    fn cleanup_never_recurses_into_an_unrecorded_child() {
        let t = TempDir::new("cleanup-foreign-child");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("x"), &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let progress = std::sync::Arc::new(Progress::new());
        let (p, d) = (progress.clone(), dest.clone());
        crate::fileops_test::arm("copy-read", move |_| {
            let temp = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
            write(&temp.join("foreign/unique"), "keep foreign data");
            p.cancel();
        });
        let out = paste(Operation::Copy, &[src], &dest, &progress);
        crate::fileops_test::fired();
        assert!(out.cancelled);
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::PartLeft { .. })]
        ));
        let temp = fs::read_dir(&dest).unwrap().next().unwrap().unwrap().path();
        assert_eq!(read(&temp.join("foreign/unique")), "keep foreign data");
        assert!(
            !temp.join("x").exists(),
            "our recorded partial child can be removed"
        );
    }

    #[test]
    fn cleanup_keeps_a_replacement_at_a_recorded_child_name() {
        let t = TempDir::new("cleanup-replaced-child");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("x"), &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let progress = std::sync::Arc::new(Progress::new());
        let (p, d, parked) = (progress.clone(), dest.clone(), t.path("parked-output"));
        crate::fileops_test::arm("copy-read", move |_| {
            let temp = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
            fs::rename(temp.join("x"), parked).unwrap();
            write(&temp.join("x"), "unique replacement");
            p.cancel();
        });
        let out = paste(Operation::Copy, &[src], &dest, &progress);
        crate::fileops_test::fired();
        assert!(out.cancelled);
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::PartLeft { .. })]
        ));
        let temp = fs::read_dir(&dest).unwrap().next().unwrap().unwrap().path();
        assert_eq!(read(&temp.join("x")), "unique replacement");
    }

    fn move_across_devices(source: &Path, dest: &Path) -> Outcome {
        FORCE_EXDEV.with(|v| v.set(true));
        let out = paste(Operation::Cut, &[source.into()], dest, &Progress::new());
        FORCE_EXDEV.with(|v| v.set(false));
        out
    }

    #[test]
    fn move_never_publishes_a_foreign_replacement_of_the_written_inode() {
        let t = TempDir::new("publish-inode");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let d = dest.clone();
        crate::fileops_test::arm("copy-read", move |_| {
            let temp = fs::read_dir(d).unwrap().next().unwrap().unwrap().path();
            fs::remove_file(&temp).unwrap();
            write(&temp, "unique foreign bytes");
        });
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert!(!out.failed.is_empty());
        assert!(out.done.is_empty());
        assert_eq!(fs::read(&src).unwrap(), vec![b'A'; CHUNK * 2]);
        assert!(!dest.join("x").exists());
        let temp = fs::read_dir(&dest).unwrap().next().unwrap().unwrap().path();
        assert_eq!(read(&temp), "unique foreign bytes");
    }

    #[test]
    fn move_keeps_the_source_if_its_published_destination_disappears() {
        let t = TempDir::new("publish-removed");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, "unique original bytes");
        fs::create_dir(&dest).unwrap();
        crate::fileops_test::arm("copy-published", |target| fs::remove_file(target).unwrap());
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert_eq!(out.failed.len(), 1);
        assert!(out.changed);
        assert!(out.done.is_empty());
        assert_eq!(read(&src), "unique original bytes");
        assert!(!dest.join("x").exists());
    }

    #[test]
    fn a_completed_copy_changed_through_its_inode_is_kept_and_not_published() {
        let t = TempDir::new("publish-corrupt");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        crate::fileops_test::arm("copy-written", |temp| write(temp, "external unique bytes"));
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::PartLeft { .. })]
        ));
        assert_eq!(fs::read(&src).unwrap(), vec![b'A'; CHUNK * 2]);
        assert!(!dest.join("x").exists());
        let temp = fs::read_dir(&dest).unwrap().next().unwrap().unwrap().path();
        assert_eq!(read(&temp), "external unique bytes");
    }

    #[test]
    fn a_sync_error_after_publication_never_removes_the_move_source() {
        let t = TempDir::new("publish-sync");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, "payload");
        fs::create_dir(&dest).unwrap();
        crate::fileops_test::arm("copy-published", |_| COPY_SYNC_ERROR.with(|v| v.set(true)));
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert_eq!(out.failed.len(), 1);
        assert!(out.done.is_empty());
        assert_eq!(read(&src), "payload");
        assert_eq!(read(&dest.join("x")), "payload");
        assert!(out.failed[0].1.to_string().contains("source was kept"));
    }

    #[test]
    fn a_replaced_destination_subdirectory_cannot_authorize_source_cleanup() {
        let t = TempDir::new("publish-subdir");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("sub/x"), "selected bytes");
        fs::create_dir(&dest).unwrap();
        let parked = t.path("parked");
        crate::fileops_test::arm("copy-published", move |target| {
            fs::rename(target.join("sub"), &parked).unwrap();
            write(&target.join("sub/foreign"), "unrelated bytes");
        });
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert_eq!(out.failed.len(), 1);
        assert_eq!(read(&src.join("sub/x")), "selected bytes");
        assert_eq!(read(&dest.join("tree/sub/foreign")), "unrelated bytes");
    }

    #[test]
    fn an_unverified_copy_does_not_trash_the_consented_destination() {
        let t = TempDir::new("publish-verify-before-trash");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, "selected bytes");
        write(&dest.join("x"), "consented old bytes");
        crate::fileops_test::arm("copy-written", |temp| write(temp, "external unique data"));
        let out = paste_with(
            Operation::Copy,
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
        );
        crate::fileops_test::fired();
        assert_eq!(out.replaced, 0);
        assert!(!out.failed.is_empty());
        assert_eq!(read(&src), "selected bytes");
        assert_eq!(read(&dest.join("x")), "consented old bytes");
        assert!(!t.path("test-trash").exists());
    }

    #[test]
    fn coalesced_destination_times_do_not_authorize_deleting_different_source_bytes() {
        let t = TempDir::new("publish-coarse-times");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, "AAAA");
        fs::create_dir(&dest).unwrap();
        let meta = fs::symlink_metadata(&src).unwrap();
        let (target, copied, mut produced) = copy_in(
            &src,
            &meta,
            &dest,
            OsStr::new("x"),
            &Progress::new(),
            true,
            &mut Run::default(),
        )
        .unwrap();
        let saved = fs::metadata(&target).unwrap().modified().unwrap();
        write(&target, "BBBB");
        File::open(&target).unwrap().set_modified(saved).unwrap();
        // Model a filesystem whose timestamp precision coalesces this edit.
        // Device, inode, size, mode, uid/gid and mtime already match.
        let changed = fs::metadata(&target).unwrap();
        produced.nodes[0].expected.ctime = (changed.ctime(), changed.ctime_nsec());
        produced.check().unwrap();
        assert!(remove_copied(&copied, &produced).is_some());
        assert_eq!(read(&src), "AAAA");
        assert_eq!(read(&target), "BBBB");
    }

    #[test]
    fn source_replacement_in_the_check_to_isolation_gap_is_never_unlinked() {
        let t = TempDir::new("source-unlink-gap");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, "copied old bytes");
        let spare = t.path("spare");
        write(&spare, "unique replacement never copied");
        fs::create_dir(&dest).unwrap();
        let parked = t.path("saved-original");
        crate::fileops_test::arm_path("isolate-checked", &src, move |path| {
            fs::rename(path, &parked).unwrap();
            fs::rename(&spare, path).unwrap();
        });
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::NotRemoved(_))]
        ));
        assert_eq!(read(&src), "unique replacement never copied");
        assert_eq!(read(&dest.join("x")), "copied old bytes");
    }

    #[test]
    fn a_foreign_object_taken_in_a_cleanup_gap_is_kept_when_its_name_is_occupied() {
        let t = TempDir::new("source-restore-occupied");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, "copied bytes");
        fs::create_dir(&dest).unwrap();
        let parked = t.path("parked");
        let original = src.clone();
        crate::fileops_test::arm_path("isolate-checked", &src, move |path| {
            fs::rename(path, parked).unwrap();
            write(path, "foreign unique bytes");
            crate::fileops_test::arm("isolate-taken", move |_| write(&original, "new occupant"));
        });
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::NotRemoved(_))]
        ));
        assert_eq!(read(&src), "new occupant");
        let cell = fs::read_dir(src.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .as_bytes()
                    .starts_with(b".everygrep-held-")
            })
            .unwrap();
        assert_eq!(read(&cell.join("item")), "foreign unique bytes");
        assert!(
            out.failed[0]
                .1
                .to_string()
                .contains(&cell.display().to_string())
        );
    }

    #[test]
    fn cleanup_rejects_a_filesystem_that_cannot_protect_the_holding_folder() {
        let t = TempDir::new("holding-permissions");
        let src = t.path("source");
        write(&src, "keep bytes");
        let meta = fs::metadata(&src).unwrap();
        let parent = std::sync::Arc::new(open_dir(&t.0).unwrap());
        crate::fileops_test::arm("holding-created", |p| {
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap()
        });
        let result = Isolation::take(parent, OsStr::new("source"), &src, (meta.dev(), meta.ino()));
        crate::fileops_test::fired();
        assert!(matches!(result,Err(ref e) if e.kind()==io::ErrorKind::Unsupported));
        assert_eq!(read(&src), "keep bytes");
        assert_eq!(fs::read_dir(&t.0).unwrap().count(), 1);
    }

    #[test]
    fn source_child_replacement_after_its_last_check_is_restored_instead_of_deleted() {
        let t = TempDir::new("source-child-gap");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("x"), "selected bytes");
        fs::create_dir(&dest).unwrap();
        let source_dir = std::sync::Arc::new(open_dir(&src).unwrap());
        let parked = t.path("parked-child");
        crate::fileops_test::arm_path("isolate-checked", &src.join("x"), move |_| {
            let actual = fd_path(&source_dir).join("x");
            fs::rename(&actual, &parked).unwrap();
            write(&actual, "foreign child bytes");
        });
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert!(matches!(
            out.failed.as_slice(),
            [(_, Failure::NotRemoved(_))]
        ));
        assert_eq!(read(&src.join("x")), "foreign child bytes");
        assert_eq!(read(&dest.join("tree/x")), "selected bytes");
    }

    #[test]
    fn a_filesystem_ignoring_private_file_mode_is_rejected_before_reading_data() {
        let t = TempDir::new("private-file-ignored");
        let src = t.path("src/secret");
        let dest = t.path("dest");
        write(&src, &"S".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        crate::fileops_test::arm("copy-created", |path| {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(fs::metadata(path).unwrap().len(), 0);
        });
        let progress = Progress::new();
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &progress,
        );
        crate::fileops_test::fired();
        assert_eq!(out.failed.len(), 1);
        assert_eq!(progress.done(), 0);
        assert_eq!(fs::metadata(&src).unwrap().len(), (CHUNK * 2) as u64);
        assert!(names(&dest).is_empty());
    }

    #[test]
    fn a_filesystem_ignoring_private_directory_mode_is_rejected_before_copying_children() {
        let t = TempDir::new("private-dir-ignored");
        let src = t.path("src/tree");
        let dest = t.path("dest");
        write(&src.join("secret"), "private bytes");
        fs::create_dir(&dest).unwrap();
        crate::fileops_test::arm("copy-created", |path| {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap()
        });
        let progress = Progress::new();
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &progress,
        );
        crate::fileops_test::fired();
        assert_eq!(out.failed.len(), 1);
        assert_eq!(progress.done(), 0);
        assert_eq!(read(&src.join("secret")), "private bytes");
        assert!(names(&dest).is_empty());
    }

    #[test]
    fn reopening_a_descriptor_parent_keeps_its_directory_identity() {
        let t = TempDir::new("descriptor-parent");
        let fd = open_dir(&t.0).unwrap();
        let path = fd_path(&fd).join("child");
        let parent = open_dir(path.parent().unwrap()).unwrap();
        assert_eq!(
            (fd.metadata().unwrap().dev(), fd.metadata().unwrap().ino()),
            (
                parent.metadata().unwrap().dev(),
                parent.metadata().unwrap().ino()
            )
        );
        symlink(&t.0, t.path("link")).unwrap();
        assert!(
            open_dir(&t.path("link")).is_err(),
            "ordinary directory links are still refused"
        );
    }

    #[test]
    fn overwrite_never_trashes_a_new_occupant_of_the_consented_name() {
        let t = TempDir::new("replace-isolated");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, "new source");
        write(&dest.join("x"), "consented target");
        crate::trashbin::set_test_home(Some(t.path("Trash")));
        let replacement = Replacement::with_trash(|actual, original, stamp| {
            crate::trashbin::put_from(actual, original, stamp).map_err(io::Error::other)
        });
        crate::fileops_test::arm("trash-checked", |path| write(path, "new unique occupant"));
        let out = paste_observed_using(
            Operation::Copy,
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
            replacement,
            |_| {},
        );
        crate::fileops_test::fired();
        crate::trashbin::set_test_home(None);
        assert!(!out.failed.is_empty());
        assert_eq!(read(&src), "new source");
        assert_eq!(read(&dest.join("x")), "new unique occupant");
        assert_eq!(read(&t.path("Trash/files/x")), "consented target");
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
    fn unsupported_no_replace_keeps_files_directories_and_links() {
        for errno in [22, 38, 95] {
            let t = TempDir::new(&format!("unsupported-{errno}"));
            for kind in ["file", "dir", "link"] {
                let source = t.path(kind);
                match kind {
                    "dir" => fs::create_dir(&source).unwrap(),
                    "link" => symlink("unfollowed", &source).unwrap(),
                    _ => write(&source, "selected bytes"),
                }
                let target = t.path(&format!("{kind}-target"));
                // A target arriving just as the filesystem rejects the atomic
                // operation must survive, even if hard links are unavailable.
                let error = rename_noreplace_with(&source, &target, |_, to| {
                    write(to, "unique destination bytes");
                    Err(io::Error::from_raw_os_error(errno))
                })
                .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::Unsupported);
                assert!(error.to_string().contains("Both items were kept"));
                assert!(fs::symlink_metadata(&source).is_ok());
                assert_eq!(read(&target), "unique destination bytes");
                fs::remove_file(&target).unwrap();
                // Refuse on unsupported filesystems even when the target is free.
                assert!(
                    rename_noreplace_with(&source, &target, |_, _| {
                        Err(io::Error::from_raw_os_error(errno))
                    })
                    .is_err()
                );
                assert!(fs::symlink_metadata(&source).is_ok());
                assert!(!target.exists());
            }
        }
    }

    #[test]
    fn no_replace_preserves_other_errors() {
        let t = TempDir::new("noreplace-denied");
        let source = t.path("source");
        write(&source, "keep");
        let error = rename_noreplace_with(&source, &t.path("target"), |_, _| {
            Err(io::Error::from_raw_os_error(13))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(read(&source), "keep");
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

    #[test]
    fn a_copy_never_publishes_bytes_from_two_versions_of_the_source() {
        let t = TempDir::new("torn-copy");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let s = src.clone();
        crate::fileops_test::arm("copy-read", move |_| write(&s, &"B".repeat(CHUNK * 2)));
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        crate::fileops_test::fired();
        assert!(out.done.is_empty(), "{:?}", out.done);
        assert_eq!(out.failed.len(), 1);
        assert!(
            out.failed[0]
                .1
                .to_string()
                .contains("The copy source changed while copying."),
            "{}",
            out.failed[0].1
        );
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&src), "B".repeat(CHUNK * 2));
    }

    #[test]
    fn a_move_keeps_a_source_rewritten_while_it_is_read() {
        let t = TempDir::new("torn-move");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 2));
        fs::create_dir(&dest).unwrap();
        let s = src.clone();
        crate::fileops_test::arm("copy-read", move |_| write(&s, &"B".repeat(CHUNK * 2)));
        let out = move_across_devices(&src, &dest);
        crate::fileops_test::fired();
        assert!(out.done.is_empty(), "{:?}", out.done);
        assert_eq!(out.failed.len(), 1);
        assert!(
            out.failed[0]
                .1
                .to_string()
                .contains("The copy source changed while copying.")
        );
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&src), "B".repeat(CHUNK * 2));
    }

    #[test]
    fn a_source_that_shrinks_while_it_is_read_is_not_copied() {
        let t = TempDir::new("torn-shrink");
        let src = t.path("src/x");
        let dest = t.path("dest");
        write(&src, &"A".repeat(CHUNK * 3));
        fs::create_dir(&dest).unwrap();
        let s = src.clone();
        crate::fileops_test::arm("copy-read", move |_| {
            File::options()
                .write(true)
                .open(&s)
                .unwrap()
                .set_len(CHUNK as u64)
                .unwrap()
        });
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&src),
            &dest,
            &Progress::new(),
        );
        crate::fileops_test::fired();
        assert!(out.done.is_empty(), "{:?}", out.done);
        assert_eq!(out.failed.len(), 1);
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(fs::metadata(&src).unwrap().len(), CHUNK as u64);
    }

    #[test]
    fn a_folder_with_a_file_changed_while_read_leaves_no_partial_copy() {
        let t = TempDir::new("torn-tree");
        let tree = t.path("src/tree");
        let other = t.path("src/other");
        let changing = tree.join("b");
        write(&tree.join("a"), "first");
        write(&changing, &"A".repeat(CHUNK * 2));
        write(&other, "unrelated");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let s = changing.clone();
        crate::fileops_test::arm_path("copy-read", &changing, move |_| {
            write(&s, &"B".repeat(CHUNK * 2))
        });
        let out = paste(
            Operation::Copy,
            &[tree.clone(), other.clone()],
            &dest,
            &Progress::new(),
        );
        crate::fileops_test::fired();
        // The changed item fails as a whole; the other item of the batch
        // is still copied.
        assert_eq!(out.done, [dest.join("other")]);
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed[0].0, tree);
        assert_eq!(names(&dest), ["other"]);
        assert_eq!(read(&tree.join("a")), "first");
        assert_eq!(read(&changing), "B".repeat(CHUNK * 2));
    }

    #[test]
    fn a_source_replaced_before_it_is_opened_is_not_copied() {
        let t = TempDir::new("swapped-before-open");
        let src = t.path("src/x");
        write(&src, "selected bytes");
        let meta = fs::symlink_metadata(&src).unwrap();
        fs::remove_file(&src).unwrap();
        write(&src, "another file at the same name");
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let mut buf = vec![0; CHUNK];
        let (mut copied, mut created, mut special) = (Vec::new(), Vec::new(), 0);
        let mut c = Copier {
            progress: &Progress::new(),
            sync: false,
            buf: &mut buf,
            copied: &mut copied,
            created: &mut created,
            special: &mut special,
            made: false,
        };
        let temp = temp_name(&dest);
        assert!(matches!(c.tree(&src, &meta, &temp), Err(Stop::Io(_))));
        assert!(!c.made);
        assert!(names(&dest).is_empty());
    }

    /// Whether a regular file under `dir` (links not followed) holds `bytes`.
    fn holds(dir: &Path, bytes: &[u8]) -> bool {
        fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
            let kind = e.file_type().unwrap();
            if kind.is_dir() {
                holds(&e.path(), bytes)
            } else {
                kind.is_file() && fs::read(e.path()).unwrap() == bytes
            }
        })
    }

    fn fifo(path: &Path) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
    }

    /// Runs `work` on its own thread. `None` when it is still blocked after
    /// two seconds; a writer is then opened at `unblock` so it can end. A
    /// worker still blocked after that is left behind (never joined), so
    /// the test fails instead of hanging.
    fn within_deadline(
        unblock: PathBuf,
        work: impl FnOnce() -> Outcome + Send + 'static,
    ) -> Option<Outcome> {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        if let Ok(out) = rx.recv_timeout(std::time::Duration::from_secs(2)) {
            worker.join().unwrap();
            return Some(out);
        }
        for _ in 0..100 {
            if OpenOptions::new()
                .write(true)
                .custom_flags(0o4000)
                .open(&unblock)
                .is_ok()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok() {
            let _ = worker.join();
        }
        None
    }

    #[test]
    fn the_deadline_gives_up_on_a_worker_that_stays_blocked() {
        let t = TempDir::new("deadline");
        let start = std::time::Instant::now();
        let out = within_deadline(t.path("no-reader"), || {
            let (_keep, never) = std::sync::mpsc::channel::<()>();
            let _ = never.recv();
            Outcome::default()
        });
        assert!(out.is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(30));
    }

    fn copy(src: &Path, dest: &Path) -> Outcome {
        paste(Operation::Copy, &[src.to_owned()], dest, &Progress::new())
    }

    fn changed_while_copying(out: &Outcome) -> bool {
        out.failed.len() == 1
            && out.failed[0]
                .1
                .to_string()
                .contains("The copy source changed while copying.")
    }

    #[test]
    fn copy_reads_the_opened_folder_when_its_name_becomes_a_link() {
        let t = TempDir::new("walk-root-link");
        let tree = t.path("src/tree");
        let (parked, outside, dest) = (t.path("parked"), t.path("outside"), t.path("dest"));
        write(&tree.join("selected"), "selected");
        write(&outside.join("DO-NOT-TOUCH"), "outside bytes");
        fs::create_dir(&dest).unwrap();
        let (s, p, o) = (tree.clone(), parked.clone(), outside.clone());
        crate::fileops_test::arm_path("copy-open", &tree, move |_| {
            fs::rename(&s, &p).unwrap();
            symlink(&o, &s).unwrap();
        });
        let out = copy(&tree, &dest);
        crate::fileops_test::fired();
        assert!(!holds(&dest, b"outside bytes"), "{:?}", names(&dest));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(read(&dest.join("tree/selected")), "selected");
        assert_eq!(read(&outside.join("DO-NOT-TOUCH")), "outside bytes");
    }

    #[test]
    fn move_never_takes_files_from_a_folder_linked_in_after_the_open() {
        let t = TempDir::new("walk-move-link");
        let tree = t.path("src/tree");
        let (parked, outside, dest) = (t.path("parked"), t.path("outside"), t.path("dest"));
        write(&tree.join("selected"), "selected");
        write(&outside.join("DO-NOT-TOUCH"), "outside bytes");
        fs::create_dir(&dest).unwrap();
        let (s, p, o) = (tree.clone(), parked.clone(), outside.clone());
        crate::fileops_test::arm_path("copy-open", &tree, move |_| {
            fs::rename(&s, &p).unwrap();
            symlink(&o, &s).unwrap();
        });
        let out = move_across_devices(&tree, &dest);
        crate::fileops_test::fired();
        assert!(!holds(&dest, b"outside bytes"), "{:?}", names(&dest));
        assert_eq!(read(&outside.join("DO-NOT-TOUCH")), "outside bytes");
        assert_eq!(read(&parked.join("selected")), "selected");
        // The selected folder no longer has its name, so it is not removed.
        assert!(
            matches!(out.failed.as_slice(), [(_, Failure::NotRemoved(_))]),
            "{:?}",
            out.failed
        );
    }

    #[test]
    fn copy_never_follows_a_child_folder_swapped_for_a_link() {
        let t = TempDir::new("walk-child-link");
        let tree = t.path("src/tree");
        let sub = tree.join("sub");
        let (parked, outside, dest) = (t.path("parked"), t.path("outside"), t.path("dest"));
        write(&tree.join("a"), "a");
        write(&sub.join("inner"), "inner");
        write(&outside.join("DO-NOT-TOUCH"), "outside bytes");
        fs::create_dir(&dest).unwrap();
        let (s, p, o) = (sub.clone(), parked.clone(), outside.clone());
        crate::fileops_test::arm_path("copy-open", &sub, move |_| {
            fs::rename(&s, &p).unwrap();
            symlink(&o, &s).unwrap();
        });
        let out = copy(&tree, &dest);
        crate::fileops_test::fired();
        assert!(!holds(&dest, b"outside bytes"), "{:?}", names(&dest));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(read(&dest.join("tree/a")), "a");
        assert_eq!(read(&dest.join("tree/sub/inner")), "inner");
        assert_eq!(read(&outside.join("DO-NOT-TOUCH")), "outside bytes");
    }

    #[test]
    fn a_file_swapped_for_a_link_after_it_is_opened_is_refused_without_following() {
        let t = TempDir::new("walk-file-link");
        let src = t.path("src/x");
        let (parked, outside, dest) = (t.path("parked"), t.path("private"), t.path("dest"));
        write(&src, "selected");
        write(&outside, "unselected private file");
        fs::create_dir(&dest).unwrap();
        let (s, p, o) = (src.clone(), parked.clone(), outside.clone());
        crate::fileops_test::arm_path("copy-open", &src, move |_| {
            fs::rename(&s, &p).unwrap();
            symlink(&o, &s).unwrap();
        });
        let out = copy(&src, &dest);
        crate::fileops_test::fired();
        // The held file was renamed away (its status changed): refused,
        // and the link put at its name is never followed.
        assert!(changed_while_copying(&out), "{:?}", out.failed);
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&outside), "unselected private file");
    }

    #[test]
    fn a_fifo_swapped_in_after_it_is_opened_is_refused_without_blocking() {
        let t = TempDir::new("walk-fifo-after");
        let src = t.path("src/x");
        let (parked, pipe, dest) = (t.path("parked"), t.path("pipe"), t.path("dest"));
        write(&src, "selected");
        fifo(&pipe);
        fs::create_dir(&dest).unwrap();
        let (s, d) = (src.clone(), dest.clone());
        let out = within_deadline(src.clone(), move || {
            let (s2, p) = (s.clone(), parked);
            crate::fileops_test::arm_path("copy-open", &s, move |_| {
                fs::rename(&s2, &p).unwrap();
                fs::rename(&pipe, &s2).unwrap();
            });
            let out = copy(&s, &d);
            crate::fileops_test::fired();
            out
        })
        .expect("the copy blocked on a FIFO");
        assert!(changed_while_copying(&out), "{:?}", out.failed);
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert!(fs::symlink_metadata(&src).unwrap().file_type().is_fifo());
    }

    #[test]
    fn copy_never_reads_through_an_ancestor_swapped_for_a_link() {
        let t = TempDir::new("walk-ancestor-link");
        let tree = t.path("src/tree");
        let sub = tree.join("sub");
        let (parked, outside, dest) = (t.path("parked"), t.path("outside"), t.path("dest"));
        write(&sub.join("inner"), "inner");
        // The same names outside the selection: only the bytes differ.
        write(&outside.join("sub/inner"), "outside bytes");
        fs::create_dir(&dest).unwrap();
        let (s, p, o) = (tree.clone(), parked.clone(), outside.clone());
        crate::fileops_test::arm_path("copy-open", &sub, move |_| {
            fs::rename(&s, &p).unwrap();
            symlink(&o, &s).unwrap();
        });
        let out = copy(&tree, &dest);
        crate::fileops_test::fired();
        assert!(!holds(&dest, b"outside bytes"), "{:?}", names(&dest));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(read(&dest.join("tree/sub/inner")), "inner");
        assert_eq!(read(&outside.join("sub/inner")), "outside bytes");
    }

    /// `tree` with `sub/inner` and a file `a`, plus `outside/DO-NOT-TOUCH`.
    fn walk_fixture(t: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let tree = t.path("src/tree");
        write(&tree.join("a"), "a");
        write(&tree.join("sub/inner"), "inner");
        write(&t.path("outside/DO-NOT-TOUCH"), "outside bytes");
        fs::create_dir(t.path("dest")).unwrap();
        (tree, t.path("outside"), t.path("dest"))
    }

    /// Replaces `path` with a link to `to` when `event` reaches it.
    fn link_at(event: &'static str, path: &Path, to: &Path, parked: &Path) {
        let (s, o, p) = (path.to_owned(), to.to_owned(), parked.to_owned());
        crate::fileops_test::arm_path(event, path, move |_| {
            fs::rename(&s, &p).unwrap();
            symlink(&o, &s).unwrap();
        });
    }

    #[test]
    fn a_folder_swapped_for_a_link_before_it_is_opened_is_refused() {
        let t = TempDir::new("walk-root-before");
        let (tree, outside, dest) = walk_fixture(&t);
        link_at("copy-pin", &tree, &outside, &t.path("parked"));
        let out = copy(&tree, &dest);
        crate::fileops_test::fired();
        assert!(changed_while_copying(&out), "{:?}", out.failed);
        assert!(out.done.is_empty());
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&outside.join("DO-NOT-TOUCH")), "outside bytes");
    }

    #[test]
    fn a_child_folder_swapped_for_a_link_before_it_is_opened_fails_the_item() {
        let t = TempDir::new("walk-child-before");
        let (tree, outside, dest) = walk_fixture(&t);
        let other = t.path("src/other");
        write(&other, "other");
        link_at("copy-pin", &tree.join("sub"), &outside, &t.path("parked"));
        let out = paste(
            Operation::Copy,
            &[tree.clone(), other.clone()],
            &dest,
            &Progress::new(),
        );
        crate::fileops_test::fired();
        assert!(changed_while_copying(&out), "{:?}", out.failed);
        assert_eq!(out.failed[0].0, tree);
        // The rest of the batch still goes; no partial copy of the tree stays.
        assert_eq!(out.done, [dest.join("other")]);
        assert_eq!(names(&dest), ["other"]);
        assert!(!holds(&dest, b"outside bytes"));
        assert_eq!(read(&t.path("parked/inner")), "inner");
    }

    #[test]
    fn a_file_swapped_for_a_link_before_it_is_opened_fails_the_item() {
        let t = TempDir::new("walk-file-before");
        let (tree, outside, dest) = walk_fixture(&t);
        let private = outside.join("DO-NOT-TOUCH");
        link_at("copy-pin", &tree.join("a"), &private, &t.path("parked"));
        let out = copy(&tree, &dest);
        crate::fileops_test::fired();
        assert!(changed_while_copying(&out), "{:?}", out.failed);
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&private), "outside bytes");
    }

    #[test]
    fn a_fifo_swapped_in_before_it_is_opened_is_refused_without_blocking() {
        let t = TempDir::new("walk-fifo-before");
        let (tree, _, dest) = walk_fixture(&t);
        let pipe = t.path("pipe");
        fifo(&pipe);
        let file = tree.join("a");
        let (f, p, tr, d) = (file.clone(), t.path("parked"), tree.clone(), dest.clone());
        let out = within_deadline(file.clone(), move || {
            let f2 = f.clone();
            crate::fileops_test::arm_path("copy-pin", &f, move |_| {
                fs::rename(&f2, &p).unwrap();
                fs::rename(&pipe, &f2).unwrap();
            });
            let out = copy(&tr, &d);
            crate::fileops_test::fired();
            out
        })
        .expect("the copy blocked on a FIFO");
        assert!(changed_while_copying(&out), "{:?}", out.failed);
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert!(fs::symlink_metadata(&file).unwrap().file_type().is_fifo());
    }

    #[test]
    fn a_fifo_in_the_tree_is_counted_and_never_opened() {
        let t = TempDir::new("walk-fifo-plain");
        let (tree, _, dest) = walk_fixture(&t);
        fifo(&tree.join("pipe"));
        let (tr, d) = (tree.clone(), dest.clone());
        let out = within_deadline(tree.join("pipe"), move || copy(&tr, &d))
            .expect("the copy blocked on a FIFO");
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(out.skipped_special, 1);
        assert_eq!(names(&dest.join("tree")), ["a", "sub"]);
    }

    #[test]
    fn a_source_entry_removed_during_the_walk_fails_the_item_without_a_partial_copy() {
        let t = TempDir::new("walk-removed");
        let (tree, _, dest) = walk_fixture(&t);
        let inner = tree.join("sub/inner");
        let gone = inner.clone();
        crate::fileops_test::arm_path("copy-pin", &inner, move |_| fs::remove_file(&gone).unwrap());
        let out = copy(&tree, &dest);
        crate::fileops_test::fired();
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert!(matches!(&out.failed[0].1, Failure::Io(e) if e.kind() == io::ErrorKind::NotFound));
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&tree.join("a")), "a");
    }

    #[test]
    fn a_move_keeps_its_source_when_an_entry_is_removed_during_the_walk() {
        let t = TempDir::new("walk-removed-move");
        let (tree, _, dest) = walk_fixture(&t);
        let inner = tree.join("sub/inner");
        let gone = inner.clone();
        crate::fileops_test::arm_path("copy-pin", &inner, move |_| fs::remove_file(&gone).unwrap());
        let out = move_across_devices(&tree, &dest);
        crate::fileops_test::fired();
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&tree.join("a")), "a");
        assert!(tree.join("sub").is_dir());
    }

    #[test]
    fn cancel_during_the_walk_leaves_no_partial_copy() {
        let t = TempDir::new("walk-cancel");
        let (tree, _, dest) = walk_fixture(&t);
        let progress = std::sync::Arc::new(Progress::new());
        let p = progress.clone();
        crate::fileops_test::arm_path("copy-open", &tree.join("sub"), move |_| p.cancel());
        let out = paste(
            Operation::Copy,
            std::slice::from_ref(&tree),
            &dest,
            &progress,
        );
        crate::fileops_test::fired();
        assert!(out.cancelled);
        assert!(out.done.is_empty());
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(read(&tree.join("sub/inner")), "inner");
    }

    #[test]
    fn a_link_inside_the_tree_is_copied_as_the_link() {
        let t = TempDir::new("walk-plain-link");
        let (tree, outside, dest) = walk_fixture(&t);
        symlink(&outside, tree.join("to-outside")).unwrap();
        let out = copy(&tree, &dest);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let copied = dest.join("tree/to-outside");
        assert!(fs::symlink_metadata(&copied).unwrap().is_symlink());
        assert_eq!(fs::read_link(&copied).unwrap(), outside);
        assert!(!holds(&dest, b"outside bytes"));
    }

    #[test]
    fn a_tree_two_hundred_folders_deep_copies_and_moves() {
        let t = TempDir::new("walk-deep");
        let tree = t.path("src/deep");
        let mut at = tree.clone();
        for i in 0..200 {
            at.push("d");
            write(&at.join("f"), &i.to_string());
        }
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let out = copy(&tree, &dest);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let moved = t.path("moved");
        fs::create_dir(&moved).unwrap();
        let out = move_across_devices(&tree, &moved);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!tree.exists());
        for root in [dest.join("deep"), moved.join("deep")] {
            let mut at = root;
            for i in 0..200 {
                at.push("d");
                assert_eq!(read(&at.join("f")), i.to_string());
            }
        }
    }

    #[test]
    fn a_folder_with_twenty_thousand_files_copies_and_moves() {
        let t = TempDir::new("walk-wide");
        let tree = t.path("src/wide");
        fs::create_dir_all(&tree).unwrap();
        for i in 0..20_000 {
            fs::write(tree.join(i.to_string()), i.to_string()).unwrap();
        }
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let out = copy(&tree, &dest);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let moved = t.path("moved");
        fs::create_dir(&moved).unwrap();
        let out = move_across_devices(&tree, &moved);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!tree.exists());
        for root in [dest.join("wide"), moved.join("wide")] {
            assert_eq!(fs::read_dir(&root).unwrap().count(), 20_000);
            assert_eq!(read(&root.join("19999")), "19999");
        }
    }

    #[test]
    fn the_size_count_never_follows_a_folder_swapped_for_a_link() {
        let t = TempDir::new("size-swap");
        let sub = t.path("tree/sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("small"), "12345").unwrap();
        let outside = t.path("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("big"), vec![0u8; 1 << 20]).unwrap();
        // Looked at as a folder, then swapped for a link before it is read.
        let (swap, aside) = (sub.clone(), t.path("sub-aside"));
        crate::fileops_test::arm_path("size-folder", &sub, move |_| {
            fs::rename(&swap, &aside).unwrap();
            symlink(&outside, &swap).unwrap();
        });
        let size = tree_size(&t.path("tree"), &Progress::new());
        crate::fileops_test::fired();
        assert!(size < 1 << 20, "{size}");
        // Unchanged, the folders are counted in full.
        fs::remove_file(&sub).unwrap();
        fs::rename(t.path("sub-aside"), &sub).unwrap();
        fs::write(t.path("tree/top"), "123").unwrap();
        assert_eq!(tree_size(&t.path("tree"), &Progress::new()), 8);
    }

    /// The open file limit for [`copy_and_move_with_few_descriptors`].
    const FEW_DESCRIPTORS: u64 = 256;
    const FEW_DESCRIPTORS_CHILD: &str = "EVERYGREP_TEST_FEW_DESCRIPTORS";

    #[test]
    fn a_tree_of_five_thousand_files_copies_and_moves_with_256_open_files() {
        // The limit is lowered in a child run of this test binary, never in
        // the binary running the other tests.
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transfer::tests::copy_and_move_with_few_descriptors",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FEW_DESCRIPTORS_CHILD, "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("1 passed"), "{text}");
    }

    #[test]
    #[ignore = "run in a child process by a_tree_of_five_thousand_files_copies_and_moves_with_256_open_files"]
    fn copy_and_move_with_few_descriptors() {
        assert!(std::env::var_os(FEW_DESCRIPTORS_CHILD).is_some());
        #[repr(C)]
        struct Rlimit {
            current: u64,
            maximum: u64,
        }
        unsafe extern "C" {
            fn getrlimit(resource: c_int, limit: *mut Rlimit) -> c_int;
            fn setrlimit(resource: c_int, limit: *const Rlimit) -> c_int;
        }
        const RLIMIT_NOFILE: c_int = 7;
        let t = TempDir::new("few-descriptors");
        let tree = t.path("src/tree");
        for d in 0..5 {
            let folder = tree.join(format!("d{d}"));
            fs::create_dir_all(&folder).unwrap();
            for i in 0..1000 {
                fs::write(folder.join(i.to_string()), format!("{d}/{i}")).unwrap();
            }
        }
        let mut limit = Rlimit {
            current: 0,
            maximum: 0,
        };
        // SAFETY: plain calls with a valid pointer to a C `struct rlimit`.
        assert_eq!(unsafe { getrlimit(RLIMIT_NOFILE, &mut limit) }, 0);
        limit.current = FEW_DESCRIPTORS.min(limit.maximum);
        assert_eq!(unsafe { setrlimit(RLIMIT_NOFILE, &limit) }, 0);
        let dest = t.path("dest");
        fs::create_dir(&dest).unwrap();
        let out = copy(&tree, &dest);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        let moved = t.path("moved");
        fs::create_dir(&moved).unwrap();
        let out = move_across_devices(&tree, &moved);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!tree.exists());
        for root in [dest.join("tree"), moved.join("tree")] {
            for d in 0..5 {
                let folder = root.join(format!("d{d}"));
                assert_eq!(fs::read_dir(&folder).unwrap().count(), 1000);
                assert_eq!(read(&folder.join("999")), format!("{d}/999"));
            }
        }
    }
}
