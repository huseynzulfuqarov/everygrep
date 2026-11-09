//! Records of file operations that, for a moment, keep a user's item only
//! under a hidden staging name: a move that replaces a file, a change of
//! only the case of a name, and an item held aside before it goes to the
//! trash or to its new name.
//!
//! Before such a step, a small record is written to
//! `$XDG_STATE_HOME/everygrep/interrupted/` (by default
//! `~/.local/state/everygrep/interrupted/`): the item's original path, its
//! staging path, where the operation was taking it, the item's identity
//! (device, inode, type, size, modification and change time) and the step
//! reached. Each record is written to a temporary file, flushed, renamed to
//! its name, and the folder is flushed. The record goes away when the
//! operation is complete or the item is back at its original name.
//!
//! When the program starts it reads the records left by a program that is
//! no longer running ([`recover`]). An item still at its staging path with
//! the recorded identity goes back to its original name when that name is
//! free (a rename that never replaces anything). In every other case
//! nothing is moved or deleted and the user is told the exact paths.
//! Nothing is ever done to a hidden file just because of its name.

use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::i18n::{fill, tr};
use crate::transfer::{fd_path, open_dir};

/// The first line of every record.
const HEADER: &str = "everygrep interrupted operation 2";
/// Records are small; a bigger file is not one of ours.
const MAX_RECORD: u64 = 64 << 10;

/// What the operation was doing with the item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A move that replaces a file: the item waits under a hidden name in
    /// the destination folder while the replaced file goes to the trash.
    MoveReplace,
    /// A change of only the case of a name: the item waits under a hidden
    /// name in its folder before it gets the new spelling.
    CaseRename,
    /// An item held aside in a private folder before it goes to the trash.
    Trash,
    /// An item held aside in a private folder before it gets its new path
    /// (undo and redo of a rename or move).
    Rename,
}

impl Kind {
    fn code(self) -> &'static str {
        match self {
            Kind::MoveReplace => "move-replace",
            Kind::CaseRename => "case-rename",
            Kind::Trash => "trash",
            Kind::Rename => "rename",
        }
    }
    fn parse(code: &str) -> Option<Self> {
        [
            Kind::MoveReplace,
            Kind::CaseRename,
            Kind::Trash,
            Kind::Rename,
        ]
        .into_iter()
        .find(|k| k.code() == code)
    }
    /// Whether the item waits in a private holding folder of its own.
    fn held(self) -> bool {
        matches!(self, Kind::Trash | Kind::Rename)
    }
}

/// The step an operation reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Recorded; the item may or may not be at its staging path yet.
    Staging,
    /// The item is at its staging path.
    Staged,
    /// A move that replaces: the replaced file went to the trash.
    TargetTrashed,
}

impl Step {
    fn code(self) -> &'static str {
        match self {
            Step::Staging => "staging",
            Step::Staged => "staged",
            Step::TargetTrashed => "target-trashed",
        }
    }
    fn parse(code: &str) -> Option<Self> {
        [Step::Staging, Step::Staged, Step::TargetTrashed]
            .into_iter()
            .find(|s| s.code() == code)
    }
}

/// What identifies the staged item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    dev: u64,
    ino: u64,
    /// `f` file, `d` folder, `l` link, `o` anything else.
    kind: u8,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Identity {
    pub(crate) fn of(m: &Metadata) -> Self {
        let t = m.file_type();
        Self {
            dev: m.dev(),
            ino: m.ino(),
            kind: if t.is_file() {
                b'f'
            } else if t.is_dir() {
                b'd'
            } else if t.is_symlink() {
                b'l'
            } else {
                b'o'
            },
            size: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
    /// The same item, unchanged except for its change time, which every
    /// rename of it sets.
    fn same_item(&self, now: &Self) -> bool {
        (self.dev, self.ino, self.kind, self.size, self.mtime)
            == (now.dev, now.ino, now.kind, now.size, now.mtime)
    }
}

/// One record, as written to disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    /// The program that wrote it: process id and start time.
    owner: (u32, u64),
    kind: Kind,
    step: Step,
    original: PathBuf,
    staging: PathBuf,
    /// Where the item was going; empty for the trash.
    target: PathBuf,
    /// The device and inode of the folder of `original`: the item goes
    /// back only into that same folder.
    parent: (u64, u64),
    identity: Identity,
    /// The reason the user was last told the item was kept, so a later
    /// start does not tell it again.
    reported: Option<String>,
}

/// Bytes of a path that stand for themselves in a record; every other byte
/// is written as `%XX`, so any name (non-UTF-8, a new line) fits one line.
fn encode(path: &Path) -> String {
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_graphic() && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Option<PathBuf> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else if bytes[i].is_ascii_graphic() {
            out.push(bytes[i]);
            i += 1;
        } else {
            return None;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

fn time(t: (i64, i64)) -> String {
    format!("{} {}", t.0, t.1)
}

impl Record {
    fn to_text(&self) -> String {
        let i = &self.identity;
        let mut text = format!(
            "{HEADER}\nowner {} {}\nkind {}\nstep {}\noriginal {}\nstaging {}\ntarget {}\n\
             parent {} {}\ndev {}\nino {}\ntype {}\nsize {}\nmtime {}\nctime {}\n",
            self.owner.0,
            self.owner.1,
            self.kind.code(),
            self.step.code(),
            encode(&self.original),
            encode(&self.staging),
            encode(&self.target),
            self.parent.0,
            self.parent.1,
            i.dev,
            i.ino,
            i.kind as char,
            i.size,
            time(i.mtime),
            time(i.ctime),
        );
        if let Some(reason) = &self.reported {
            text.push_str(&format!("reported {}\n", encode(Path::new(reason))));
        }
        text.push_str("end\n");
        text
    }

    /// The record in `text`; `None` for anything incomplete or unexpected.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.strip_suffix('\n')?.split('\n');
        if lines.next()? != HEADER {
            return None;
        }
        let mut field = |key: &str| -> Option<&str> {
            let line = lines.next()?;
            line.strip_prefix(key)?.strip_prefix(' ')
        };
        let pair = |v: &str| -> Option<(i64, i64)> {
            let (a, b) = v.split_once(' ')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        };
        let owner = field("owner")?.split_once(' ')?;
        let owner = (owner.0.parse().ok()?, owner.1.parse().ok()?);
        let kind = Kind::parse(field("kind")?)?;
        let step = Step::parse(field("step")?)?;
        let original = decode(field("original")?)?;
        let staging = decode(field("staging")?)?;
        let target = decode(field("target")?).unwrap_or_default();
        let parent = field("parent")?.split_once(' ')?;
        let parent = (parent.0.parse().ok()?, parent.1.parse().ok()?);
        let dev = field("dev")?.parse().ok()?;
        let ino = field("ino")?.parse().ok()?;
        let kind_byte = match field("type")?.as_bytes() {
            [k @ (b'f' | b'd' | b'l' | b'o')] => *k,
            _ => return None,
        };
        let size = field("size")?.parse().ok()?;
        let mtime = pair(field("mtime")?)?;
        let ctime = pair(field("ctime")?)?;
        let mut reported = None;
        let mut rest = lines.next()?;
        if let Some(reason) = rest.strip_prefix("reported ") {
            reported = Some(decode(reason)?.into_os_string().into_string().ok()?);
            rest = lines.next()?;
        }
        if rest != "end" || lines.next().is_some() {
            return None;
        }
        if !original.is_absolute()
            || !staging.is_absolute()
            || original.file_name().is_none()
            || staging.file_name().is_none()
            || (kind != Kind::Trash && !target.is_absolute())
        {
            return None;
        }
        Some(Self {
            owner,
            kind,
            step,
            original,
            staging,
            target,
            parent,
            identity: Identity {
                dev,
                ino,
                kind: kind_byte,
                size,
                mtime,
                ctime,
            },
            reported,
        })
    }
}

#[cfg(test)]
thread_local! {
    /// The state folder in tests; without one nothing is recorded.
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Sets the state folder of this thread's tests: records go to
/// `<dir>/everygrep/interrupted/`.
#[cfg(test)]
pub(crate) fn set_test_dir(dir: Option<PathBuf>) {
    TEST_DIR.with(|d| *d.borrow_mut() = dir);
}

/// The two folders below the state folder that hold the records.
const APP: &str = "everygrep";
const RECORDS: &str = "interrupted";

/// The state folder (`$XDG_STATE_HOME`, else `~/.local/state`); `None`
/// (tests only) records nothing.
fn state_base() -> io::Result<Option<PathBuf>> {
    #[cfg(test)]
    {
        Ok(TEST_DIR.with(|d| d.borrow().clone()))
    }
    #[cfg(not(test))]
    {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|h| h.join(".local/state"))
            })
            .ok_or_else(|| io::Error::other(tr("neither XDG_STATE_HOME nor HOME is set")))?;
        Ok(Some(state))
    }
}

/// The folder of the records, as shown to the user.
fn folder() -> io::Result<Option<PathBuf>> {
    Ok(state_base()?.map(|b| b.join(APP).join(RECORDS)))
}

/// This program: its process id and start time (in clock ticks after
/// boot), so a later program with the same id is not taken for it.
fn this_owner() -> (u32, u64) {
    static OWNER: std::sync::OnceLock<(u32, u64)> = std::sync::OnceLock::new();
    *OWNER.get_or_init(|| {
        let pid = std::process::id();
        (pid, start_time(pid).ok().flatten().unwrap_or(0))
    })
}

/// When process `pid` started; `None` when there is no such process.
fn start_time(pid: u32) -> io::Result<Option<u64>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // The name in parentheses may hold spaces; field 22 is the start time.
    let after = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?
        .1;
    after
        .split_whitespace()
        .nth(19)
        .and_then(|v| v.parse().ok())
        .map(Some)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
}

/// Whether the program that wrote a record may still be running it. When
/// that cannot be told, it is taken as running and the record is left.
fn owner_running(owner: (u32, u64)) -> bool {
    owner == this_owner() || start_time(owner.0).map_or(true, |s| s == Some(owner.1))
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

/// Whether a record folder or record `m` describes is this user's own and
/// no one else can change it.
fn private_to_us(m: &Metadata) -> bool {
    // SAFETY: geteuid has no arguments and cannot fail.
    m.uid() == unsafe { geteuid() } && m.mode() & 0o022 == 0
}

/// The error for a record folder that is not used.
fn not_private(path: &Path) -> io::Error {
    io::Error::other(fill(
        tr("“{}” is a link, belongs to another user, or can be changed by others; it is not used"),
        &[&path.display()],
    ))
}

/// The folder `name` in `parent`, never through a link; made (`0700`)
/// when it is missing and `create` is set (then `made` is set). `None`
/// when it is missing.
fn open_child(
    parent: &File,
    name: &str,
    create: bool,
    made: &mut bool,
) -> io::Result<Option<File>> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(parent, name, flags, Mode::empty()) {
        Ok(fd) => return Ok(Some(File::from(fd))),
        Err(rustix::io::Errno::NOENT) if create => {}
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    match rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => *made = true,
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Some(File::from(rustix::fs::openat(
        parent,
        name,
        flags,
        Mode::empty(),
    )?)))
}

/// Makes the missing folders of `path`, top down, each `0700`; the
/// folders that got a new entry (with their paths) are added to `changed`.
fn make_folders(path: &Path, changed: &mut Vec<(File, PathBuf)>) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut at = path;
    loop {
        match fs::metadata(at) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(at);
                at = at.parent().ok_or(e)?;
            }
            Err(e) => return Err(e),
        }
    }
    for folder in missing.into_iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(folder) {
            Ok(()) => {
                let parent = folder.parent().unwrap_or(Path::new("/"));
                changed.push((open_dir(parent)?, parent.to_owned()));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The state folder `base` (as the user or the system set it), opened.
/// When it is a link (say to a folder on another drive), the link is
/// followed, and the folder it leads to must belong to this user and be
/// closed to others; the folders of this program below it are never
/// links (see [`open_folder`]).
fn open_base(base: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    match open_dir(base) {
        // A link (or not a folder): looked at where it leads.
        Err(e) if matches!(e.raw_os_error(), Some(40 | 20)) => {}
        other => return other,
    }
    let dir = File::from(rustix::fs::open(
        base,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if !private_to_us(&dir.metadata()?) {
        return Err(not_private(base));
    }
    Ok(dir)
}

/// The record folder, opened from the state folder: the two folders of
/// this program below it are never reached through a link, and each must
/// belong to this user and be closed to others. With `create`, missing
/// folders are made, and the folder above each new one is flushed (the
/// deepest first) before the folder is used, so the records written in it
/// are not lost with a folder that never reached the disk. Otherwise a
/// missing folder gives `None`.
fn open_folder(create: bool) -> io::Result<Option<File>> {
    let Some(base) = state_base()? else {
        return Ok(None);
    };
    let mut changed = Vec::new();
    if create {
        make_folders(&base, &mut changed)?;
    }
    let mut dir = match open_base(&base) {
        Ok(dir) => dir,
        Err(e) if !create && e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut shown = base;
    for name in [APP, RECORDS] {
        let above = shown.clone();
        shown.push(name);
        let mut made = false;
        let child = match open_child(&dir, name, create, &mut made) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            // A link, or not a folder.
            Err(e) if matches!(e.raw_os_error(), Some(40 | 20)) => return Err(not_private(&shown)),
            Err(e) => return Err(e),
        };
        if !private_to_us(&child.metadata()?) {
            return Err(not_private(&shown));
        }
        let parent = std::mem::replace(&mut dir, child);
        if made {
            changed.push((parent, above));
        }
    }
    for (folder, path) in changed.iter().rev() {
        sync_folder(folder, path)?;
    }
    Ok(Some(dir))
}

/// Whether `path` is absolute and written in its one plain form: no empty,
/// `.` or `..` part, no `/` at the end.
fn plain_absolute(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    bytes.len() > 1
        && bytes[0] == b'/'
        && bytes[1..]
            .split(|&b| b == b'/')
            .all(|part| !matches!(part, b"" | b"." | b".."))
}

/// Whether `name` is `<prefix><pid>-<rest>` with `rest` made of what `ok`
/// takes (and `len` bytes long, when given).
fn staging_name(
    name: &OsStr,
    prefix: &str,
    pid: u32,
    len: Option<usize>,
    ok: fn(&u8) -> bool,
) -> bool {
    let start = format!("{prefix}{pid}-");
    name.as_bytes()
        .strip_prefix(start.as_bytes())
        .is_some_and(|rest| {
            !rest.is_empty() && len.is_none_or(|l| rest.len() == l) && rest.iter().all(ok)
        })
}

/// Whether `record` describes an operation of this program: plain absolute
/// paths, and a staging path of the program that wrote it, where that
/// operation puts it (next to the target for a move that replaces, next to
/// the original for a change of case, in a holding folder next to the
/// original for a held item).
fn our_operation(record: &Record) -> bool {
    let (original, staging, target) = (&record.original, &record.staging, &record.target);
    let target_ok = if record.kind == Kind::Trash {
        target.as_os_str().is_empty()
    } else {
        plain_absolute(target)
    };
    if !plain_absolute(original) || !plain_absolute(staging) || !target_ok {
        return false;
    }
    let pid = record.owner.0;
    let moving = |name: Option<&OsStr>| {
        name.is_some_and(|n| staging_name(n, ".everygrep-moving-", pid, None, u8::is_ascii_digit))
    };
    match record.kind {
        Kind::MoveReplace => staging.parent() == target.parent() && moving(staging.file_name()),
        Kind::CaseRename => {
            staging.parent() == original.parent()
                && target.parent() == original.parent()
                && moving(staging.file_name())
        }
        Kind::Trash | Kind::Rename => {
            let holder = staging.parent();
            staging.file_name() == Some(OsStr::new("item"))
                && holder.and_then(Path::parent) == original.parent()
                && holder.and_then(Path::file_name).is_some_and(|n| {
                    staging_name(n, ".everygrep-held-", pid, Some(16), |b| {
                        b.is_ascii_digit() || (b'a'..=b'f').contains(b)
                    })
                })
        }
    }
}

/// `path` with its folder written in its one real form (links resolved).
fn real(path: &Path) -> io::Result<PathBuf> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    Ok(fs::canonicalize(parent)?.join(name))
}

/// Writes `text` as the record `name` in `dir`: a temporary file, flushed,
/// renamed over the record, then the folder flushed.
fn write_record(dir: &File, name: &OsStr, text: &str) -> io::Result<()> {
    let mut temp = std::ffi::OsString::from(".");
    temp.push(name);
    temp.push(".tmp");
    let temp_path = fd_path(dir).join(&temp);
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(0o400000)
            .open(&temp_path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp_path, fd_path(dir).join(name))?;
        dir.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    written
}

fn remove_record(dir: &File, name: &OsStr) -> io::Result<()> {
    #[cfg(test)]
    crate::fileops_test::note("record-removed", Path::new(name));
    match fs::remove_file(fd_path(dir).join(name)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    dir.sync_all()
}

#[cfg(test)]
thread_local! {
    /// Makes every flush of a folder fail, as on a failing disk.
    static FOLDER_SYNC_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Flushes the folder `dir` (at `path`) so the renames in it last.
fn sync_folder(dir: &File, path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FOLDER_SYNC_FAILS.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(5));
    }
    dir.sync_all()?;
    #[cfg(test)]
    crate::fileops_test::note("folder-synced", path);
    #[cfg(not(test))]
    let _ = path;
    Ok(())
}

/// Flushes every folder whose entries the operation of `record` changed:
/// the original's, the target's and the staging path's (for a held item
/// its holding folder is gone, and its removal is in the original's
/// folder). A folder that is gone holds nothing to flush.
fn sync_folders(record: &Record) -> io::Result<()> {
    let mut folders: Vec<&Path> = Vec::new();
    let staging = if record.kind.held() {
        None
    } else {
        record.staging.parent()
    };
    for folder in [record.original.parent(), record.target.parent(), staging]
        .into_iter()
        .flatten()
    {
        if !folder.as_os_str().is_empty() && !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    for folder in folders {
        match open_dir(folder) {
            Ok(dir) => sync_folder(&dir, folder)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Human-readable OS text for recovery notices; custom errors keep their text.
fn error_text(error: &io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => text
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&text)
            .to_owned(),
        None => text,
    }
}

/// The error of an operation that could not be recorded: nothing was
/// changed.
pub(crate) fn not_recorded(error: io::Error) -> io::Error {
    io::Error::other(fill(
        tr("Cannot record the operation for recovery after a crash: {}. Nothing was changed."),
        &[&error_text(&error)],
    ))
}

/// The record of a running operation. Dropped without [`finish`]
/// (Self::finish), the record stays: the item may still be at its staging
/// path, and the next start looks at it.
#[derive(Debug)]
pub(crate) struct Pending {
    dir: File,
    name: std::ffi::OsString,
    record: Record,
}

/// Starts the record of an operation `kind` that is about to put the item
/// `meta` describes, now at `original`, at the hidden path `staging`, on its
/// way to `target`. Nothing is recorded (`None`) only in tests without a
/// record folder. An error means nothing was recorded, and the caller must
/// not change anything.
pub(crate) fn begin(
    kind: Kind,
    original: &Path,
    staging: &Path,
    target: &Path,
    meta: &Metadata,
) -> io::Result<Option<Pending>> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let Some(dir) = open_folder(true).map_err(not_recorded)? else {
        return Ok(None);
    };
    let owner = this_owner();
    let name = std::ffi::OsString::from(format!(
        "{}-{}-{}.op",
        owner.0,
        owner.1,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let target = if target.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        real(target).map_err(not_recorded)?
    };
    let original = real(original).map_err(not_recorded)?;
    let parent = fs::metadata(original.parent().unwrap_or(Path::new("/")))
        .map(|m| (m.dev(), m.ino()))
        .map_err(not_recorded)?;
    let record = Record {
        owner,
        kind,
        step: Step::Staging,
        original,
        staging: real(staging).map_err(not_recorded)?,
        target,
        parent,
        identity: Identity::of(meta),
        reported: None,
    };
    // Never a record the next start would not act on.
    if !our_operation(&record) {
        return Err(not_recorded(io::Error::other(tr(
            "the paths do not fit the operation",
        ))));
    }
    write_record(&dir, &name, &record.to_text()).map_err(not_recorded)?;
    Ok(Some(Pending { dir, name, record }))
}

impl Pending {
    fn save(&self) {
        if let Err(e) = write_record(&self.dir, &self.name, &self.record.to_text()) {
            // The earlier step stays on disk, which recovery also handles.
            crate::debug_log::note!("operation record {:?}: {e}", self.name);
        }
    }
    /// Another staging path is tried (its name was taken).
    pub(crate) fn retarget(&mut self, staging: &Path) -> io::Result<()> {
        self.record.staging = real(staging).map_err(not_recorded)?;
        write_record(&self.dir, &self.name, &self.record.to_text()).map_err(not_recorded)
    }
    /// The item is at its staging path, as `meta` describes it now.
    pub(crate) fn staged(&mut self, meta: &Metadata) {
        self.record.identity = Identity::of(meta);
        self.record.step = Step::Staged;
        self.save();
    }
    pub(crate) fn step(&mut self, step: Step) {
        self.record.step = step;
        self.save();
    }
    /// The operation is complete, or the item is back at its original
    /// name: the folders it changed are flushed, then the record goes.
    /// When a folder cannot be flushed the record stays, and the next
    /// start finds the item where it is.
    pub(crate) fn finish(self) {
        if let Err(e) = sync_folders(&self.record) {
            crate::debug_log::note!("operation record {:?} kept: {e}", self.name);
            return;
        }
        if let Err(e) = remove_record(&self.dir, &self.name) {
            crate::debug_log::note!("operation record {:?}: {e}", self.name);
        }
    }
}

/// Ends `pending` when there is one.
pub(crate) fn finish(pending: Option<Pending>) {
    if let Some(p) = pending {
        p.finish();
    }
}

/// An item that was left where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Kept {
    pub(crate) staging: PathBuf,
    pub(crate) original: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) reason: String,
}

/// What [`recover`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Recovery {
    /// Items put back at their original name: (staging path, original).
    pub(crate) restored: Vec<(PathBuf, PathBuf)>,
    /// Items left in place, not told before.
    pub(crate) kept: Vec<Kept>,
    /// Items put back whose folders could not be flushed afterwards
    /// (original path, error); their records stay.
    pub(crate) unflushed: Vec<(PathBuf, String)>,
    /// Records that could not be read; set aside as `<name>.unreadable`.
    pub(crate) unreadable: Vec<PathBuf>,
    /// Records that were not used: a link, another user's, open to others,
    /// or not describing an operation of this program; set aside as
    /// `<name>.untrusted`.
    pub(crate) untrusted: Vec<PathBuf>,
    /// Records of operations that were complete; removed.
    pub(crate) complete: usize,
    /// The record folder could not be read.
    pub(crate) error: Option<String>,
}

impl Recovery {
    pub(crate) fn has_news(&self) -> bool {
        !self.restored.is_empty()
            || !self.kept.is_empty()
            || !self.unreadable.is_empty()
            || !self.untrusted.is_empty()
            || !self.unflushed.is_empty()
            || self.error.is_some()
    }

    /// The notice for the user: short text, details, and whether something
    /// needs the user. `None` when there is nothing to tell.
    pub(crate) fn notice(&self) -> Option<(String, String, bool)> {
        if !self.has_news() {
            return None;
        }
        use crate::format::{visible_name, visible_path as vp};
        // Names are escaped piece by piece: the details keep real line breaks.
        let mut lines = Vec::new();
        for (staging, original) in &self.restored {
            lines.push(fill(
                tr("Put back “{0}” from “{1}”."),
                &[&vp(original), &vp(staging)],
            ));
        }
        for k in &self.kept {
            let (message, target) = if k.target.as_os_str().is_empty() {
                (
                    tr(
                        "Left “{0}” where it is ({1}). Its original path: “{2}”; the operation was taking it to {3}.",
                    ),
                    tr("the trash").to_owned(),
                )
            } else {
                (
                    tr(
                        "Left “{0}” where it is ({1}). Its original path: “{2}”; the operation was taking it to “{3}”.",
                    ),
                    vp(&k.target),
                )
            };
            lines.push(fill(
                message,
                &[
                    &vp(&k.staging),
                    &visible_name(&k.reason),
                    &vp(&k.original),
                    &target,
                ],
            ));
        }
        for path in &self.unreadable {
            lines.push(fill(
                tr("The record “{}” could not be read; nothing was changed."),
                &[&vp(path)],
            ));
        }
        for (original, error) in &self.unflushed {
            lines.push(fill(
                tr("“{0}” was put back, but the change could not be written to disk ({1}); its record was kept."),
                &[&vp(original), &visible_name(error)],
            ));
        }
        for path in &self.untrusted {
            lines.push(fill(
                tr("The record “{}” was not used: it is a link, belongs to another user, can be changed by others, or does not describe an operation of this program. Nothing was changed."),
                &[&vp(path)],
            ));
        }
        if let Some(e) = &self.error {
            lines.push(visible_name(e).into_owned());
        }
        let attention = !self.kept.is_empty()
            || !self.unflushed.is_empty()
            || !self.unreadable.is_empty()
            || !self.untrusted.is_empty()
            || self.error.is_some();
        let short = if attention {
            tr("An interrupted file operation needs your attention.")
        } else {
            tr("An interrupted file operation was recovered.")
        };
        Some((short.into(), lines.join("\n"), attention))
    }
}

/// Reads the records left by programs that are no longer running and acts
/// on each one (see the module notes). Run once at start, off the window
/// thread, while no file operation runs in this program.
pub(crate) fn recover() -> Recovery {
    let mut out = Recovery::default();
    let dir = match open_folder(false) {
        Ok(Some(dir)) => dir,
        Ok(None) => return out,
        Err(e) => {
            out.error = Some(fill(
                tr("The records of interrupted file operations could not be read: {}"),
                &[&error_text(&e)],
            ));
            return out;
        }
    };
    // Another program starting at the same time looks at the same records:
    // one at a time.
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(0o400000)
        .open(fd_path(&dir).join(".lock"))
        .and_then(|f| {
            rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
            Ok(f)
        }) {
        Ok(lock) => lock,
        Err(e) => {
            out.error = Some(fill(
                tr("The records of interrupted file operations could not be read: {}"),
                &[&error_text(&e)],
            ));
            return out;
        }
    };
    let mut names: Vec<_> = match fs::read_dir(fd_path(&dir)) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.as_bytes().ends_with(b".op") && !n.as_bytes().starts_with(b"."))
            .collect(),
        Err(e) => {
            out.error = Some(fill(
                tr("The records of interrupted file operations could not be read: {}"),
                &[&error_text(&e)],
            ));
            return out;
        }
    };
    names.sort();
    for name in names {
        let path = fd_path(&dir).join(&name);
        let shown = folder().ok().flatten().unwrap_or_default().join(&name);
        let set_aside = |suffix: &str| {
            let mut aside = name.clone();
            aside.push(suffix);
            let _ = fs::rename(&path, fd_path(&dir).join(&aside));
            let _ = dir.sync_all();
            shown.with_file_name(aside)
        };
        let record = match read_record(&path) {
            Ok(Found::Record(record)) if our_operation(&record) => record,
            Ok(Found::Record(_) | Found::Untrusted) => {
                out.untrusted.push(set_aside(".untrusted"));
                continue;
            }
            Ok(Found::Unreadable) => {
                out.unreadable.push(set_aside(".unreadable"));
                continue;
            }
            Err(e) => {
                crate::debug_log::note!("operation record {}: {e}", shown.display());
                continue;
            }
        };
        if owner_running(record.owner) {
            continue;
        }
        match settle(&record) {
            Settled::Restored(unflushed) => {
                out.restored
                    .push((record.staging.clone(), record.original.clone()));
                if let Some(e) = unflushed {
                    out.unflushed.push((record.original.clone(), e));
                } else if let Err(e) = remove_record(&dir, &name) {
                    crate::debug_log::note!("operation record {}: {e}", shown.display());
                }
            }
            Settled::Complete => {
                out.complete += 1;
                // What a stopped program renamed may not be on disk yet.
                if let Err(e) = sync_folders(&record).and_then(|()| remove_record(&dir, &name)) {
                    crate::debug_log::note!("operation record {}: {e}", shown.display());
                }
            }
            Settled::Kept(reason) => {
                if record.reported.as_deref() == Some(reason.as_str()) {
                    continue;
                }
                out.kept.push(Kept {
                    staging: record.staging.clone(),
                    original: record.original.clone(),
                    target: record.target.clone(),
                    reason: reason.clone(),
                });
                let mut told = record;
                told.reported = Some(reason);
                if let Err(e) = write_record(&dir, &name, &told.to_text()) {
                    crate::debug_log::note!("operation record {}: {e}", shown.display());
                }
            }
        }
    }
    drop(lock);
    out
}

/// What a record file holds.
enum Found {
    Record(Record),
    /// Not a complete record.
    Unreadable,
    /// A link, not a plain file, another user's, or open to changes by
    /// others: not read at all.
    Untrusted,
}

/// The record at `path`.
fn read_record(path: &Path) -> io::Result<Found> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(0o400000 | 0o4000)
        .open(path)
    {
        Ok(f) => f,
        // A link (O_NOFOLLOW) is not one of our records.
        Err(e) if e.raw_os_error() == Some(40) => return Ok(Found::Untrusted),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || !private_to_us(&meta) {
        return Ok(Found::Untrusted);
    }
    if meta.len() > MAX_RECORD {
        return Ok(Found::Unreadable);
    }
    let mut text = String::new();
    if file.take(MAX_RECORD).read_to_string(&mut text).is_err() {
        return Ok(Found::Unreadable);
    }
    Ok(Record::parse(&text).map_or(Found::Unreadable, Found::Record))
}

enum Settled {
    /// The item is back at its original name; with the error when its
    /// folders could not be flushed.
    Restored(Option<String>),
    /// The operation was complete, or never moved the item.
    Complete,
    /// Left as it is, for this reason.
    Kept(String),
}

/// Where the item of `record` is, and when it is plainly at its staging
/// path, puts it back.
fn settle(record: &Record) -> Settled {
    let expected = record.identity;
    let lookup = |path: &Path| fs::symlink_metadata(path).map(|m| Identity::of(&m));
    match lookup(&record.staging) {
        Ok(now) => {
            // Until the item is known to be staged, the staging rename may
            // or may not have happened, and it sets the change time.
            let same = if record.step == Step::Staging {
                expected.same_item(&now)
            } else {
                expected == now
            };
            if !same {
                return Settled::Kept(tr("it changed since the operation started").into());
            }
            match put_back(record) {
                Ok(unflushed) => Settled::Restored(unflushed),
                Err(reason) => Settled::Kept(reason),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let at = |path: &Path| lookup(path).is_ok_and(|now| expected.same_item(&now));
            let published = if record.kind == Kind::Trash {
                crate::trashbin::find(
                    &record.original,
                    crate::trashbin::Identity {
                        dev: expected.dev,
                        ino: expected.ino,
                        len: expected.size,
                        mtime: expected.mtime,
                    },
                )
                .is_some()
            } else {
                at(&record.target)
            };
            if at(&record.original) || published {
                if record.kind.held() {
                    remove_holder(&record.staging);
                }
                Settled::Complete
            } else {
                Settled::Kept(tr("it is no longer at the staging path").into())
            }
        }
        Err(e) => Settled::Kept(error_text(&e)),
    }
}

/// Renames the staged item back to its original name, never replacing
/// anything, and checks that the item renamed is the recorded one. Then
/// both folders are flushed; `Some` error when they could not be.
fn put_back(record: &Record) -> Result<Option<String>, String> {
    let parent = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_default();
    let from_dir = open_plain_dir(&parent(&record.staging)).map_err(|e| error_text(&e))?;
    let to_dir = open_plain_dir(&parent(&record.original)).map_err(|e| error_text(&e))?;
    if to_dir
        .metadata()
        .map(|m| (m.dev(), m.ino()))
        .map_err(|e| error_text(&e))?
        != record.parent
    {
        return Err(tr("its original folder is not the same folder any more").into());
    }
    let from = record.staging.file_name().unwrap_or_default();
    let to = record.original.file_name().unwrap_or_default();
    let taken = || tr("its original name is taken").to_owned();
    match fs::symlink_metadata(fd_path(&to_dir).join(to)) {
        Ok(_) => return Err(taken()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(error_text(&e)),
    }
    // Held by its descriptor while it is checked, and looked at again
    // through its folder right before and right after the rename: Linux
    // cannot rename a descriptor, only a name.
    let pinned = File::from(
        rustix::fs::openat(
            &from_dir,
            from,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|e| error_text(&io::Error::from(e)))?,
    );
    let now = Identity::of(&pinned.metadata().map_err(|e| error_text(&e))?);
    if !record.identity.same_item(&now) {
        return Err(changed());
    }
    let pin = (now.dev, now.ino);
    #[cfg(test)]
    crate::fileops_test::at("recovery-checked", &record.staging);
    if inode_at(&from_dir, from) != Some(pin) {
        return Err(changed());
    }
    #[cfg(test)]
    crate::fileops_test::at("recovery-renaming", &record.staging);
    match crate::transfer::rename_at(&from_dir, from, &to_dir, to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(taken()),
        Err(e) => return Err(error_text(&e)),
    }
    #[cfg(test)]
    crate::fileops_test::at("recovery-renamed", &record.staging);
    let moved = inode_at(&to_dir, to);
    if moved != Some(pin) {
        // Another item took the staging name in the last moment and was
        // renamed instead: it goes back, never over anything, and only
        // while it is still the item that was renamed.
        let back = match moved {
            Some(id) if inode_at(&to_dir, to) == Some(id) => {
                crate::transfer::rename_at(&to_dir, to, &from_dir, from)
            }
            _ => Err(io::Error::other(changed())),
        };
        return Err(match back {
            Ok(()) => changed(),
            Err(e) => fill(
                tr(
                    "another item took the staging path in the last moment and was moved to “{0}”; it could not be moved back to “{1}” ({2}), so it was left there",
                ),
                &[
                    &record.original.display(),
                    &record.staging.display(),
                    &error_text(&e),
                ],
            ),
        });
    }
    let staging_folder = parent(&record.staging);
    let flushed = sync_folder(&from_dir, &staging_folder).and_then(|()| {
        // The holder goes from the original folder, flushed after it.
        if record.kind.held() {
            remove_holder(&record.staging);
        }
        sync_folder(&to_dir, &parent(&record.original))
    });
    Ok(flushed.err().map(|e| error_text(&e)))
}

/// The device and inode of the entry `name` in `dir` (a link itself).
fn inode_at(dir: &File, name: &OsStr) -> Option<(u64, u64)> {
    rustix::fs::statat(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .ok()
        .map(|st| (st.st_dev, st.st_ino))
}

fn changed() -> String {
    tr("it changed since the operation started").into()
}

/// The folder at the absolute `path`, reached without following any link.
fn open_plain_dir(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};
    Ok(File::from(rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )?))
}

/// Removes the private holding folder of a held item when it is empty.
fn remove_holder(staging: &Path) {
    if let Some(holder) = staging.parent()
        && let Err(e) = fs::remove_dir(holder)
        && e.kind() != io::ErrorKind::NotFound
    {
        crate::debug_log::note!("holding folder {}: {e}", holder.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_escapes_names_and_keeps_one_line_per_item() {
        let found = Recovery {
            restored: vec![("/s/.x\n1".into(), "/s/Put back \u{202e}a\nb".into())],
            unreadable: vec!["/r/rec\n.op".into()],
            ..Default::default()
        };
        let (_, details, _) = found.notice().unwrap();
        assert_eq!(details.lines().count(), 2, "{details}");
        assert!(details.contains("“/s/Put back \\u{202e}a⏎b”"));
        assert!(details.contains("“/s/.x⏎1”"));
        assert!(details.contains("“/r/rec⏎.op”"));
        assert!(!details.contains('\u{202e}'));
    }
    use crate::clipboard::Operation;
    use crate::transfer::Decision;
    use everygrep_core::progress::Progress;

    /// The record folder below a test's temp folder.
    const RECORDS_REL: &str = "state/everygrep/interrupted";

    /// A temp folder that is also the state folder (`state/`, records in
    /// [`RECORDS_REL`]) and the trash (`trash/`) of this thread while it
    /// lives.
    struct Temp(PathBuf);
    impl Temp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "everygrep-gui-test-oprecord-{tag}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir.join(RECORDS_REL))
                .unwrap();
            set_test_dir(Some(dir.join("state")));
            crate::trashbin::set_test_home(Some(dir.join("trash")));
            Self(dir)
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.path(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
        fn records(&self) -> Vec<PathBuf> {
            let mut out: Vec<_> = fs::read_dir(self.path(RECORDS_REL))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| !p.file_name().unwrap().as_bytes().starts_with(b"."))
                .collect();
            out.sort();
            out
        }
        fn record(&self) -> Record {
            let records = self.records();
            assert_eq!(records.len(), 1, "{records:?}");
            Record::parse(&fs::read_to_string(&records[0]).unwrap()).unwrap()
        }
        /// The hidden entries of folder `rel`.
        fn hidden(&self, rel: &str) -> Vec<PathBuf> {
            fs::read_dir(self.path(rel))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .as_bytes()
                        .starts_with(b".everygrep-")
                })
                .collect()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            set_test_dir(None);
            crate::trashbin::set_test_home(None);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const CRASH: &str = "EVERYGREP_TEST_CRASH";
    const CRASH_ROOT: &str = "EVERYGREP_TEST_CRASH_ROOT";
    /// Exit code of a child that stopped where it was told to.
    const CRASHED: i32 = 90;

    /// Runs `scenario` in a child run of this test binary, which stops dead
    /// (no unwinding, no clean-up) at the step `at`, like a program that is
    /// killed or crashes there.
    fn crash(t: &Temp, scenario: &str, at: &str) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "oprecord::tests::crashing_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CRASH, format!("{scenario}:{at}"))
            .env(CRASH_ROOT, &t.0)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(CRASHED),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "run in a child process by the crash tests of this module"]
    fn crashing_child() {
        let (Some(what), Some(root)) = (std::env::var(CRASH).ok(), std::env::var_os(CRASH_ROOT))
        else {
            return;
        };
        let root = PathBuf::from(root);
        assert!(root.starts_with(std::env::temp_dir()));
        set_test_dir(Some(root.join("state")));
        crate::trashbin::set_test_home(Some(root.join("trash")));
        let (scenario, at) = what.split_once(':').unwrap();
        let at: &'static str = String::leak(at.to_owned());
        if at != "in-trash" {
            crate::fileops_test::arm(at, |_| std::process::exit(CRASHED));
        }
        match scenario {
            "move-replace" => {
                fn fake_trash(
                    actual: &Path,
                    original: &Path,
                    stamp: crate::transfer::FileStamp,
                ) -> io::Result<()> {
                    crate::trashbin::test_send(actual, original, stamp)
                        .map_err(io::Error::other)?;
                    if std::env::var(CRASH).unwrap().ends_with(":in-trash") {
                        std::process::exit(CRASHED);
                    }
                    Ok(())
                }
                let _ = crate::transfer::paste_observed_using(
                    Operation::Cut,
                    &[root.join("src/x")],
                    &root.join("dest"),
                    &Progress::new(),
                    Some(Box::new(|_, _| Decision::Overwrite)),
                    crate::transfer::Replacement::with_trash(fake_trash),
                    |_| {},
                );
            }
            "case-rename" => {
                let _ = crate::actions::rename_case_only(
                    &root.join("dir/Name"),
                    &root.join("dir/NAME"),
                    crate::transfer::rename_noreplace,
                );
            }
            "trash" => {
                let path = root.join("dir/doomed");
                let stamp = crate::transfer::FileStamp::read(&path).unwrap();
                let _ = crate::fileops_undo::trash_file_checked(
                    &path,
                    stamp,
                    &crate::trashbin::test_send,
                );
            }
            other => panic!("{other}"),
        }
        std::process::exit(0);
    }

    /// The move that replaces `dest/x` with `src/x`, stopped at `at`.
    fn move_replace_crashed_at(at: &str) -> Temp {
        let t = Temp::new(&format!("replace-{at}"));
        t.write("src/x", "original source");
        t.write("dest/x", "old destination");
        crash(&t, "move-replace", at);
        t
    }

    #[test]
    fn a_move_replace_stopped_after_the_target_went_to_the_trash_is_put_back_at_the_next_start() {
        let t = move_replace_crashed_at("in-trash");
        // As before: neither name is there, the bytes are only hidden.
        assert!(!t.path("src/x").exists() && !t.path("dest/x").exists());
        let staged: Vec<_> = t
            .hidden("dest")
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .as_bytes()
                    .starts_with(b".everygrep-moving-")
            })
            .collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(fs::read(&staged[0]).unwrap(), b"original source");
        assert_eq!(
            fs::read(t.path("trash/files/x")).unwrap(),
            b"old destination"
        );
        // Now there is a record of it, and one of the replaced file, which
        // is in the trash but whose holding folder is still there.
        let records: Vec<_> = t
            .records()
            .iter()
            .map(|p| Record::parse(&fs::read_to_string(p).unwrap()).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .any(|r| r.kind == Kind::Trash && r.original == t.path("dest/x"))
        );
        let record = records
            .into_iter()
            .find(|r| r.kind == Kind::MoveReplace)
            .unwrap();
        assert_eq!(record.kind, Kind::MoveReplace);
        assert_eq!(record.step, Step::Staged);
        assert_eq!(record.original, t.path("src/x"));
        assert_eq!(record.staging, staged[0]);
        assert_eq!(record.target, t.path("dest/x"));
        let found = recover();
        assert_eq!(found.restored, [(staged[0].clone(), t.path("src/x"))]);
        assert!(found.kept.is_empty() && found.unreadable.is_empty());
        assert_eq!(found.complete, 1, "the replaced file is in the trash");
        assert_eq!(fs::read(t.path("src/x")).unwrap(), b"original source");
        assert!(t.hidden("dest").is_empty() && t.records().is_empty());
        assert_eq!(
            fs::read(t.path("trash/files/x")).unwrap(),
            b"old destination"
        );
        // Running it again changes nothing.
        assert_eq!(recover(), Recovery::default());
        assert_eq!(fs::read(t.path("src/x")).unwrap(), b"original source");
    }

    #[test]
    fn a_move_replace_stopped_at_each_step_leaves_the_source_recoverable() {
        // Recorded, not yet staged: the source never moved.
        let t = move_replace_crashed_at("staging-recorded");
        assert_eq!(t.record().step, Step::Staging);
        let found = recover();
        assert!(!found.has_news(), "{found:?}");
        assert_eq!(found.complete, 1);
        assert_eq!(fs::read(t.path("src/x")).unwrap(), b"original source");
        assert_eq!(fs::read(t.path("dest/x")).unwrap(), b"old destination");
        assert!(t.records().is_empty());
        drop(t);
        // Staged, the target not touched yet: the source goes back.
        for at in ["staged", "replace-target-trashed"] {
            let t = move_replace_crashed_at(at);
            let step = t.record().step;
            assert_eq!(
                step,
                if at == "staged" {
                    Step::Staged
                } else {
                    Step::TargetTrashed
                }
            );
            let found = recover();
            assert_eq!(found.restored.len(), 1, "{at}: {found:?}");
            assert_eq!(fs::read(t.path("src/x")).unwrap(), b"original source");
            assert!(t.hidden("dest").is_empty() && t.records().is_empty());
            if at == "staged" {
                assert_eq!(fs::read(t.path("dest/x")).unwrap(), b"old destination");
            } else {
                assert_eq!(
                    fs::read(t.path("trash/files/x")).unwrap(),
                    b"old destination"
                );
            }
        }
        // Published, the record not yet removed: the move was complete.
        let t = move_replace_crashed_at("replace-published");
        let found = recover();
        assert!(!found.has_news(), "{found:?}");
        assert_eq!(found.complete, 1);
        assert_eq!(fs::read(t.path("dest/x")).unwrap(), b"original source");
        assert!(!t.path("src/x").exists() && t.records().is_empty());
    }

    #[test]
    fn a_case_rename_stopped_at_each_step_is_recovered() {
        for (at, restored) in [
            ("staging-recorded", false),
            ("staged", true),
            ("case-published", false),
        ] {
            let t = Temp::new(&format!("case-{at}"));
            t.write("dir/Name", "kept bytes");
            crash(&t, "case-rename", at);
            assert_eq!(t.record().kind, Kind::CaseRename);
            let found = recover();
            assert!(
                found.kept.is_empty() && found.unreadable.is_empty(),
                "{found:?}"
            );
            assert_eq!(
                found.restored.len(),
                usize::from(restored),
                "{at}: {found:?}"
            );
            let name = if at == "case-published" {
                "dir/NAME"
            } else {
                "dir/Name"
            };
            assert_eq!(fs::read(t.path(name)).unwrap(), b"kept bytes", "{at}");
            assert!(t.hidden("dir").is_empty() && t.records().is_empty(), "{at}");
        }
    }

    #[test]
    fn an_item_held_for_the_trash_when_the_program_stopped_is_put_back() {
        let t = Temp::new("held-trash");
        t.write("dir/doomed", "keep me");
        crash(&t, "trash", "trash-checked");
        assert!(!t.path("dir/doomed").exists());
        assert_eq!(t.hidden("dir").len(), 1, "the private holder");
        assert_eq!(t.record().kind, Kind::Trash);
        let found = recover();
        assert_eq!(found.restored.len(), 1, "{found:?}");
        assert_eq!(fs::read(t.path("dir/doomed")).unwrap(), b"keep me");
        assert!(t.hidden("dir").is_empty(), "the empty holder goes");
        assert!(t.records().is_empty());
    }

    #[test]
    fn an_item_that_reached_the_trash_before_the_stop_needs_nothing() {
        let t = Temp::new("held-trash-done");
        t.write("dir/doomed", "in the trash");
        crash(&t, "trash", "isolate-taken");
        // Stopped while held: put back. Then trashed fully and the record
        // left, as if the program stopped right after.
        assert_eq!(recover().restored.len(), 1);
        let path = t.path("dir/doomed");
        let meta = fs::symlink_metadata(&path).unwrap();
        let holder = t.path(&format!(
            "dir/.everygrep-held-{}-0123456789abcdef",
            std::process::id()
        ));
        fs::create_dir(&holder).unwrap();
        let pending = begin(
            Kind::Trash,
            &path,
            &holder.join("item"),
            Path::new(""),
            &meta,
        )
        .unwrap()
        .unwrap();
        crate::trashbin::test_trash(&path).unwrap();
        as_stopped(pending);
        let found = recover();
        assert!(!found.has_news(), "{found:?}");
        assert_eq!(found.complete, 1);
        assert!(t.records().is_empty());
        assert!(!path.exists());
    }

    /// Leaves the record of `pending` as a program that stopped would.
    fn as_stopped(mut pending: Pending) {
        // The same process id (the staging names carry it), another start.
        pending.record.owner.1 += 1;
        pending.save();
    }

    /// An item staged by hand, its record left by a stopped program.
    fn staged_by_hand(t: &Temp, name: &str) -> (PathBuf, PathBuf) {
        let original = t.write(&format!("dir/{name}"), name);
        fs::create_dir_all(t.path("dest")).unwrap();
        let staging = t.path(&format!(
            "dest/.everygrep-moving-{}-{}",
            std::process::id(),
            name.as_bytes()[0]
        ));
        let meta = fs::symlink_metadata(&original).unwrap();
        let mut pending = begin(
            Kind::MoveReplace,
            &original,
            &staging,
            &t.path(&format!("dest/{name}")),
            &meta,
        )
        .unwrap()
        .unwrap();
        fs::rename(&original, &staging).unwrap();
        pending.staged(&fs::symlink_metadata(&staging).unwrap());
        as_stopped(pending);
        (original, staging)
    }

    #[test]
    fn an_item_whose_original_name_is_taken_stays_where_it_is_and_is_reported_once() {
        let t = Temp::new("taken");
        let (original, staging) = staged_by_hand(&t, "a");
        fs::write(&original, "someone else").unwrap();
        let found = recover();
        assert!(found.restored.is_empty());
        assert_eq!(
            found.kept,
            [Kept {
                staging: staging.clone(),
                original: original.clone(),
                target: t.path("dest/a"),
                reason: "its original name is taken".into(),
            }]
        );
        assert_eq!(fs::read(&staging).unwrap(), b"a");
        assert_eq!(fs::read(&original).unwrap(), b"someone else");
        // The record stays, and the user is not told again.
        assert_eq!(t.records().len(), 1);
        assert!(!recover().has_news());
        assert_eq!(fs::read(&staging).unwrap(), b"a");
        // Once the name is free, the item goes back.
        fs::rename(&original, t.path("dir/elsewhere")).unwrap();
        assert_eq!(recover().restored, [(staging.clone(), original.clone())]);
        assert_eq!(fs::read(&original).unwrap(), b"a");
        assert!(t.records().is_empty());
    }

    #[test]
    fn an_item_changed_or_gone_from_its_staging_path_is_left_alone() {
        let t = Temp::new("changed");
        let (a, a_staging) = staged_by_hand(&t, "a");
        let (b, b_staging) = staged_by_hand(&t, "b");
        // Another item at a's staging path; b is no longer there.
        fs::remove_file(&a_staging).unwrap();
        fs::write(&a_staging, "another").unwrap();
        fs::rename(&b_staging, t.path("dir/moved-b")).unwrap();
        let found = recover();
        assert!(found.restored.is_empty());
        let reasons: Vec<_> = found
            .kept
            .iter()
            .map(|k| (k.staging.clone(), k.reason.as_str()))
            .collect();
        assert_eq!(
            reasons,
            [
                (a_staging.clone(), "it changed since the operation started"),
                (b_staging.clone(), "it is no longer at the staging path"),
            ]
        );
        assert_eq!(fs::read(&a_staging).unwrap(), b"another");
        assert_eq!(fs::read(t.path("dir/moved-b")).unwrap(), b"b");
        assert!(!a.exists() && !b.exists());
        assert_eq!(t.records().len(), 2);
        assert!(!recover().has_news());
    }

    #[test]
    fn an_item_rewritten_at_its_staging_path_is_left_alone() {
        let t = Temp::new("rewritten");
        let (original, staging) = staged_by_hand(&t, "a");
        fs::write(&staging, "rewritten, longer").unwrap();
        let found = recover();
        assert_eq!(found.kept.len(), 1);
        assert!(!original.exists());
        assert_eq!(fs::read(&staging).unwrap(), b"rewritten, longer");
    }

    #[test]
    fn an_item_is_not_put_into_another_folder_made_at_its_original_folder_path() {
        let t = Temp::new("parent-replaced");
        let (original, staging) = staged_by_hand(&t, "a");
        // The original folder moved away; a new folder took its path.
        fs::rename(t.path("dir"), t.path("dir-moved")).unwrap();
        fs::create_dir(t.path("dir")).unwrap();
        let found = recover();
        assert!(found.restored.is_empty(), "{found:?}");
        assert_eq!(found.kept.len(), 1, "{found:?}");
        assert!(found.has_news());
        assert_eq!(fs::read(&staging).unwrap(), b"a");
        assert!(!original.exists());
        assert_eq!(fs::read_dir(t.path("dir")).unwrap().count(), 0);
        // Its own folder back at the path: the item goes back.
        fs::remove_dir(t.path("dir")).unwrap();
        fs::rename(t.path("dir-moved"), t.path("dir")).unwrap();
        assert_eq!(recover().restored, [(staging, original.clone())]);
        assert_eq!(fs::read(&original).unwrap(), b"a");
    }

    /// The folder flushes and record removals noted while `f` runs.
    fn flushes(f: impl FnOnce()) -> Vec<(&'static str, PathBuf)> {
        crate::fileops_test::start_log();
        f();
        crate::fileops_test::take_log()
            .into_iter()
            .filter(|(e, _)| matches!(*e, "folder-synced" | "record-removed"))
            .collect()
    }

    /// Asserts that `log` flushes each of `folders` and then removes one record.
    fn flushed_then_removed(log: &[(&'static str, PathBuf)], folders: &[PathBuf]) {
        let removed = log
            .iter()
            .position(|(e, _)| *e == "record-removed")
            .unwrap_or_else(|| panic!("no record removed: {log:?}"));
        for folder in folders {
            assert!(
                log[..removed]
                    .iter()
                    .any(|(e, p)| *e == "folder-synced" && p == folder),
                "{} not flushed before the record went: {log:?}",
                folder.display()
            );
        }
    }

    #[test]
    fn a_finished_move_replace_flushes_both_folders_before_its_record_goes() {
        let t = Temp::new("flush-replace");
        let src = t.write("src/x", "original source");
        t.write("dest/x", "old destination");
        let log = flushes(|| {
            let out = crate::transfer::paste_observed_using(
                Operation::Cut,
                std::slice::from_ref(&src),
                &t.path("dest"),
                &Progress::new(),
                Some(Box::new(|_, _| Decision::Overwrite)),
                crate::transfer::test_replacement(),
                |_| {},
            );
            assert!(out.failed.is_empty(), "{:?}", out.failed);
        });
        // The replaced file's record (held for the trash) goes first, then
        // the move's, right after both its folders are flushed.
        let moved = log
            .iter()
            .rposition(|(e, _)| *e == "record-removed")
            .unwrap();
        let before = log[..moved]
            .iter()
            .rposition(|(e, _)| *e == "record-removed")
            .map_or(0, |i| i + 1);
        flushed_then_removed(&log[before..=moved], &[t.path("src"), t.path("dest")]);
        assert!(t.records().is_empty());
    }

    #[test]
    fn a_finished_case_rename_flushes_its_folder_before_its_record_goes() {
        let t = Temp::new("flush-case");
        let src = t.write("dir/Name", "bytes");
        let log = flushes(|| {
            crate::actions::rename_case_only(
                &src,
                &t.path("dir/NAME"),
                crate::transfer::rename_noreplace,
            )
            .unwrap();
        });
        flushed_then_removed(&log, &[t.path("dir")]);
        assert!(t.records().is_empty());
    }

    #[test]
    fn a_record_stays_when_the_folders_of_its_finished_operation_cannot_be_flushed() {
        let t = Temp::new("flush-fails");
        let src = t.write("dir/Name", "bytes");
        FOLDER_SYNC_FAILS.with(|f| f.set(true));
        let done = crate::actions::rename_case_only(
            &src,
            &t.path("dir/NAME"),
            crate::transfer::rename_noreplace,
        );
        FOLDER_SYNC_FAILS.with(|f| f.set(false));
        done.unwrap();
        assert_eq!(t.records().len(), 1, "the record is kept");
        // The next start finds the rename complete and removes it.
        let mut record = t.record();
        record.owner.1 += 1;
        fs::write(&t.records()[0], record.to_text()).unwrap();
        let found = recover();
        assert!(!found.has_news(), "{found:?}");
        assert_eq!(found.complete, 1);
        assert!(t.records().is_empty());
        assert_eq!(fs::read(t.path("dir/NAME")).unwrap(), b"bytes");
    }

    #[test]
    fn recovery_flushes_both_folders_before_it_removes_a_record() {
        let t = Temp::new("flush-recovery");
        let (original, staging) = staged_by_hand(&t, "a");
        let log = flushes(|| {
            assert_eq!(recover().restored.len(), 1);
        });
        flushed_then_removed(&log, &[t.path("dir"), t.path("dest")]);
        assert_eq!(fs::read(&original).unwrap(), b"a");
        assert!(!staging.exists());
    }

    #[test]
    fn recovery_keeps_the_record_when_it_cannot_flush_what_it_put_back() {
        let t = Temp::new("flush-recovery-fails");
        let (original, _) = staged_by_hand(&t, "a");
        FOLDER_SYNC_FAILS.with(|f| f.set(true));
        let found = recover();
        FOLDER_SYNC_FAILS.with(|f| f.set(false));
        assert_eq!(found.restored.len(), 1);
        assert_eq!(found.unflushed.len(), 1, "{found:?}");
        let (_, details, attention) = found.notice().unwrap();
        assert!(
            attention && details.contains("its record was kept"),
            "{details}"
        );
        assert_eq!(fs::read(&original).unwrap(), b"a");
        assert_eq!(t.records().len(), 1, "the record is kept");
        // Next time the item is found back home and the record goes.
        let found = recover();
        assert!(!found.has_news(), "{found:?}");
        assert!(t.records().is_empty());
    }

    #[test]
    fn the_first_record_flushes_every_new_folder_above_it_deepest_first() {
        let t = Temp::new("first-record");
        // Nothing of the state folder exists yet, two levels above it too.
        fs::remove_dir_all(t.path("state")).unwrap();
        set_test_dir(Some(t.path("state/deeper")));
        let src = t.write("dir/Name", "bytes");
        crate::fileops_test::start_log();
        let pending = begin(
            Kind::CaseRename,
            &src,
            &t.path(&format!("dir/.everygrep-moving-{}-1", std::process::id())),
            &t.path("dir/NAME"),
            &fs::symlink_metadata(&src).unwrap(),
        )
        .unwrap()
        .unwrap();
        let log = crate::fileops_test::take_log();
        let synced: Vec<_> = log
            .iter()
            .filter(|(e, _)| *e == "folder-synced")
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(
            synced,
            [
                t.path("state/deeper/everygrep"),
                t.path("state/deeper"),
                t.path("state"),
                t.0.clone(),
            ]
        );
        let mode = |rel: &str| fs::metadata(t.path(rel)).unwrap().mode() & 0o777;
        assert_eq!(mode("state/deeper/everygrep/interrupted"), 0o700);
        assert_eq!(mode("state/deeper/everygrep"), 0o700);
        // Once there, nothing more is flushed for the folders.
        crate::fileops_test::start_log();
        drop(pending);
        let again = begin(
            Kind::CaseRename,
            &src,
            &t.path(&format!("dir/.everygrep-moving-{}-2", std::process::id())),
            &t.path("dir/NAME"),
            &fs::symlink_metadata(&src).unwrap(),
        )
        .unwrap();
        assert!(again.is_some());
        assert!(
            crate::fileops_test::take_log()
                .iter()
                .all(|(e, _)| *e != "folder-synced")
        );
    }

    #[test]
    fn an_item_swapped_in_at_the_last_moment_is_moved_back_or_both_paths_are_reported() {
        let t = Temp::new("swap-late");
        let (original, staging) = staged_by_hand(&t, "a");
        let away = t.path("dest/away");
        let other = staging.clone();
        // Right before the rename another item takes the staging name, and
        // right after it yet another one.
        crate::fileops_test::arm("recovery-renaming", move |_| {
            fs::rename(&other, &away).unwrap();
            fs::write(&other, "foreign").unwrap();
            crate::fileops_test::arm("recovery-renamed", move |path| {
                fs::write(path, "third").unwrap();
            });
        });
        let found = recover();
        crate::fileops_test::fired();
        assert!(found.restored.is_empty(), "{found:?}");
        assert_eq!(found.kept.len(), 1, "{found:?}");
        let reason = &found.kept[0].reason;
        assert!(
            reason.contains(&original.display().to_string())
                && reason.contains(&staging.display().to_string()),
            "{reason}"
        );
        // Nothing more is moved: each item stays where it is now.
        assert_eq!(fs::read(&original).unwrap(), b"foreign");
        assert_eq!(fs::read(&staging).unwrap(), b"third");
        assert_eq!(fs::read(t.path("dest/away")).unwrap(), b"a");
    }

    #[test]
    fn an_item_swapped_in_after_the_check_and_moved_back_is_reported() {
        let t = Temp::new("swap-moved-back");
        let (original, staging) = staged_by_hand(&t, "a");
        let away = t.path("dest/away");
        let other = staging.clone();
        crate::fileops_test::arm("recovery-renaming", move |_| {
            fs::rename(&other, &away).unwrap();
            fs::write(&other, "foreign").unwrap();
        });
        let found = recover();
        crate::fileops_test::fired();
        assert!(found.restored.is_empty(), "{found:?}");
        assert_eq!(found.kept.len(), 1, "{found:?}");
        assert!(!original.exists());
        assert_eq!(fs::read(&staging).unwrap(), b"foreign");
        assert_eq!(fs::read(t.path("dest/away")).unwrap(), b"a");
    }

    #[test]
    fn an_item_swapped_in_before_the_last_look_is_never_renamed() {
        let t = Temp::new("swap-early");
        let (original, staging) = staged_by_hand(&t, "a");
        let away = t.path("dest/away");
        let other = staging.clone();
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let put = seen.clone();
        crate::fileops_test::arm("recovery-checked", move |_| {
            fs::rename(&other, &away).unwrap();
            fs::write(&other, "foreign").unwrap();
            put.set(Some(Identity::of(&fs::symlink_metadata(&other).unwrap())));
        });
        let found = recover();
        crate::fileops_test::fired();
        // Never renamed, not even there and back: its change time is the same.
        assert_eq!(
            seen.get(),
            Some(Identity::of(&fs::symlink_metadata(&staging).unwrap())),
            "the foreign item was renamed"
        );
        assert_eq!(found.kept.len(), 1, "{found:?}");
        assert!(!original.exists());
        assert_eq!(fs::read(&staging).unwrap(), b"foreign");
    }

    #[test]
    fn two_left_records_are_both_put_back() {
        let t = Temp::new("two");
        let (a, a_staging) = staged_by_hand(&t, "a");
        let (b, b_staging) = staged_by_hand(&t, "b");
        let mut found = recover().restored;
        found.sort();
        let mut expected = vec![(a_staging, a.clone()), (b_staging, b.clone())];
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(fs::read(&a).unwrap(), b"a");
        assert_eq!(fs::read(&b).unwrap(), b"b");
        assert!(t.records().is_empty() && t.hidden("dest").is_empty());
        assert_eq!(recover(), Recovery::default());
    }

    #[test]
    fn a_record_of_a_running_program_is_not_touched() {
        let t = Temp::new("running");
        let original = t.write("dir/a", "a");
        let staging = t.path(&format!("dest/.everygrep-moving-{}-1", std::process::id()));
        fs::create_dir_all(t.path("dest")).unwrap();
        let meta = fs::symlink_metadata(&original).unwrap();
        let pending = begin(
            Kind::MoveReplace,
            &original,
            &staging,
            &t.path("dest/a"),
            &meta,
        )
        .unwrap()
        .unwrap();
        fs::rename(&original, &staging).unwrap();
        // This program is running it.
        assert_eq!(recover(), Recovery::default());
        assert!(staging.exists() && t.records().len() == 1);
        fs::rename(&staging, &original).unwrap();
        pending.finish();
        assert!(t.records().is_empty());
    }

    #[test]
    fn an_unreadable_or_partial_record_is_set_aside_with_a_notice_and_nothing_else() {
        let t = Temp::new("unreadable");
        let (original, staging) = staged_by_hand(&t, "a");
        let full = t.records().pop().unwrap();
        let text = fs::read_to_string(&full).unwrap();
        // Cut short, and plain garbage.
        fs::write(&full, &text[..text.len() / 2]).unwrap();
        fs::write(
            t.path(&format!("{RECORDS_REL}/9-9-9.op")),
            b"\xff\x00 not a record",
        )
        .unwrap();
        let found = recover();
        assert!(found.restored.is_empty() && found.kept.is_empty());
        assert_eq!(found.unreadable.len(), 2, "{found:?}");
        assert!(found.has_news());
        assert!(!original.exists());
        assert_eq!(fs::read(&staging).unwrap(), b"a");
        let names: Vec<_> = t
            .records()
            .iter()
            .map(|p| p.extension().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["unreadable", "unreadable"]);
        assert_eq!(recover(), Recovery::default());
    }

    #[test]
    fn nothing_is_changed_when_the_operation_cannot_be_recorded() {
        let t = Temp::new("unrecorded");
        let src = t.write("src/x", "original source");
        t.write("dest/x", "old destination");
        // The record folder cannot be made: a file has its name.
        fs::remove_dir(t.path(RECORDS_REL)).unwrap();
        fs::write(t.path(RECORDS_REL), "").unwrap();
        let out = crate::transfer::paste_observed_using(
            Operation::Cut,
            std::slice::from_ref(&src),
            &t.path("dest"),
            &Progress::new(),
            // The error is shown as a question: the answer skips the item.
            Some(Box::new(|q, _| {
                if q.error.is_some() {
                    Decision::Skip
                } else {
                    Decision::Overwrite
                }
            })),
            crate::transfer::test_replacement(),
            |_| {},
        );
        assert!(out.done.is_empty(), "{out:?}");
        assert!(
            matches!(&out.failed[..], [(_, crate::transfer::Failure::Io(e))] if e.to_string().contains("Nothing was changed")),
            "{out:?}"
        );
        assert_eq!(fs::read(&src).unwrap(), b"original source");
        assert_eq!(fs::read(t.path("dest/x")).unwrap(), b"old destination");
        assert!(t.hidden("dest").is_empty());
        let e = crate::actions::rename_case_only(
            &src,
            &t.path("src/X"),
            crate::transfer::rename_noreplace,
        )
        .unwrap_err();
        assert!(e.to_string().contains("Nothing was changed"), "{e}");
        assert_eq!(fs::read(&src).unwrap(), b"original source");
        assert!(t.hidden("src").is_empty());
    }

    #[test]
    fn a_finished_move_replace_and_case_rename_leave_no_record() {
        let t = Temp::new("finished");
        let src = t.write("src/x", "original source");
        t.write("dest/x", "old destination");
        let out = crate::transfer::paste_observed_using(
            Operation::Cut,
            std::slice::from_ref(&src),
            &t.path("dest"),
            &Progress::new(),
            Some(Box::new(|_, _| Decision::Overwrite)),
            crate::transfer::test_replacement(),
            |_| {},
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(fs::read(t.path("dest/x")).unwrap(), b"original source");
        crate::actions::rename_case_only(
            &t.path("dest/x"),
            &t.path("dest/X"),
            crate::transfer::rename_noreplace,
        )
        .unwrap();
        let x = t.path("dest/X");
        crate::fileops_undo::trash_file_checked(
            &x,
            crate::transfer::FileStamp::read(&x).unwrap(),
            &crate::trashbin::test_send,
        )
        .unwrap();
        assert!(!x.exists());
        assert!(t.records().is_empty());
        assert!(t.hidden("dest").is_empty());
    }

    #[test]
    fn recovery_errors_use_plain_os_text_and_preserve_custom_reasons() {
        for (code, expected) in [
            (13, "Permission denied"),
            (28, "No space left on device"),
            (30, "Read-only file system"),
        ] {
            let error = io::Error::from_raw_os_error(code);
            assert_eq!(error_text(&error), expected);
            assert_eq!(
                not_recorded(error).to_string(),
                format!(
                    "Cannot record the operation for recovery after a crash: {expected}. Nothing was changed."
                )
            );
        }
        let custom = "custom reason (os error 13)";
        assert_eq!(error_text(&io::Error::other(custom)), custom);
    }

    #[test]
    fn the_notice_names_every_path_and_asks_for_attention_only_when_needed() {
        assert_eq!(Recovery::default().notice(), None);
        let complete = Recovery {
            complete: 3,
            ..Recovery::default()
        };
        assert_eq!(complete.notice(), None, "a finished operation is not news");
        let restored = Recovery {
            restored: vec![("/d/.everygrep-moving-1-0".into(), "/s/x".into())],
            ..Recovery::default()
        };
        let (text, details, attention) = restored.notice().unwrap();
        assert_eq!(text, "An interrupted file operation was recovered.");
        assert_eq!(details, "Put back “/s/x” from “/d/.everygrep-moving-1-0”.");
        assert!(!attention);
        let kept = Recovery {
            kept: vec![
                Kept {
                    staging: "/d/.everygrep-moving-1-0".into(),
                    original: "/s/x".into(),
                    target: "/d/x".into(),
                    reason: "its original name is taken".into(),
                },
                Kept {
                    staging: "/s/.everygrep-held-1-2/item".into(),
                    original: "/s/y".into(),
                    target: PathBuf::new(),
                    reason: "it is no longer at the staging path".into(),
                },
            ],
            unreadable: vec!["/state/1-2-3.op.unreadable".into()],
            ..restored
        };
        let (text, details, attention) = kept.notice().unwrap();
        assert_eq!(text, "An interrupted file operation needs your attention.");
        assert!(attention);
        let lines: Vec<_> = details.lines().collect();
        assert_eq!(
            lines,
            [
                "Put back “/s/x” from “/d/.everygrep-moving-1-0”.",
                "Left “/d/.everygrep-moving-1-0” where it is (its original name is taken). \
                 Its original path: “/s/x”; the operation was taking it to “/d/x”.",
                "Left “/s/.everygrep-held-1-2/item” where it is (it is no longer at the \
                 staging path). Its original path: “/s/y”; the operation was taking it to \
                 the trash.",
                "The record “/state/1-2-3.op.unreadable” could not be read; nothing was changed.",
            ]
        );
    }

    /// The record file names in the record folder, set aside or not.
    fn record_names(t: &Temp) -> Vec<String> {
        let mut out: Vec<_> = fs::read_dir(t.path(RECORDS_REL))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    /// Asserts that `recover` acted on nothing: `staging` keeps its bytes,
    /// `original` is free, and the user is told about `record`.
    fn ignored_with_notice(found: &Recovery, staging: &Path, original: &Path, record: &str) {
        assert!(found.restored.is_empty(), "{found:?}");
        assert!(found.has_news(), "{found:?}");
        let (_, details, attention) = found.notice().unwrap();
        assert!(attention, "{details}");
        assert!(details.contains(record), "{details}");
        assert!(staging.exists(), "{}", staging.display());
        assert!(
            fs::symlink_metadata(original).is_err(),
            "{}",
            original.display()
        );
    }

    #[test]
    fn a_record_others_could_change_is_ignored_with_a_notice() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new("open-record");
        let (original, staging) = staged_by_hand(&t, "a");
        let record = t.records().pop().unwrap();
        fs::set_permissions(&record, fs::Permissions::from_mode(0o666)).unwrap();
        let found = recover();
        let name = record.file_name().unwrap().to_str().unwrap();
        ignored_with_notice(&found, &staging, &original, name);
        // Told once: the record is set aside and not looked at again.
        assert!(!recover().has_news());
        assert!(staging.exists());
    }

    #[test]
    fn a_record_folder_others_could_change_is_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new("open-folder");
        let (original, staging) = staged_by_hand(&t, "a");
        let folder = t.path(RECORDS_REL);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o777)).unwrap();
        let found = recover();
        ignored_with_notice(&found, &staging, &original, "interrupted");
        // Nothing is recorded there either: the operation is refused.
        let src = t.write("src/x", "source");
        t.write("dest/x", "old");
        let e = crate::actions::rename_case_only(
            &src,
            &t.path("src/X"),
            crate::transfer::rename_noreplace,
        )
        .unwrap_err();
        assert!(e.to_string().contains("Nothing was changed"), "{e}");
        assert_eq!(fs::read(&src).unwrap(), b"source");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn a_record_folder_reached_through_a_link_is_not_used() {
        let t = Temp::new("linked-folder");
        let (original, staging) = staged_by_hand(&t, "a");
        // The records now sit elsewhere, and the folder name is a link.
        fs::rename(t.path("state/everygrep"), t.path("elsewhere")).unwrap();
        std::os::unix::fs::symlink(t.path("elsewhere"), t.path("state/everygrep")).unwrap();
        let found = recover();
        ignored_with_notice(&found, &staging, &original, "everygrep");
        assert_eq!(
            fs::read_dir(t.path("elsewhere/interrupted"))
                .unwrap()
                .count(),
            1,
            "the record there is not touched"
        );
        let src = t.write("src/x", "source");
        let e = crate::actions::rename_case_only(
            &src,
            &t.path("src/X"),
            crate::transfer::rename_noreplace,
        )
        .unwrap_err();
        assert!(e.to_string().contains("Nothing was changed"), "{e}");
        assert!(t.hidden("src").is_empty());
    }

    #[test]
    fn a_state_folder_that_is_a_link_to_a_private_folder_of_ours_is_used() {
        let t = Temp::new("linked-base");
        // The state folder the user set up is a link to a private folder.
        fs::rename(t.path("state"), t.path("real-state")).unwrap();
        std::os::unix::fs::symlink(t.path("real-state"), t.path("state")).unwrap();
        let src = t.write("src/x", "original source");
        t.write("dest/x", "old destination");
        let out = crate::transfer::paste_observed_using(
            Operation::Cut,
            std::slice::from_ref(&src),
            &t.path("dest"),
            &Progress::new(),
            Some(Box::new(|_, _| Decision::Overwrite)),
            crate::transfer::test_replacement(),
            |_| {},
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(fs::read(t.path("dest/x")).unwrap(), b"original source");
        // A change of case is recorded in the folder the link leads to.
        let real = t.path("real-state/everygrep/interrupted");
        let seen = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = seen.clone();
        let watched = real.clone();
        crate::fileops_test::arm("case-published", move |_| {
            count.set(fs::read_dir(&watched).unwrap().count());
        });
        crate::actions::rename_case_only(
            &t.path("dest/x"),
            &t.path("dest/X"),
            crate::transfer::rename_noreplace,
        )
        .unwrap();
        crate::fileops_test::fired();
        assert_eq!(seen.get(), 1, "the record was written");
        let x = t.path("dest/X");
        crate::fileops_undo::trash_file_checked(
            &x,
            crate::transfer::FileStamp::read(&x).unwrap(),
            &crate::trashbin::test_send,
        )
        .unwrap();
        assert!(!x.exists());
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
        assert!(!recover().has_news());
    }

    #[test]
    fn a_state_folder_that_is_a_link_to_a_folder_others_can_change_is_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new("linked-base-open");
        fs::rename(t.path("state"), t.path("real-state")).unwrap();
        std::os::unix::fs::symlink(t.path("real-state"), t.path("state")).unwrap();
        fs::set_permissions(t.path("real-state"), fs::Permissions::from_mode(0o770)).unwrap();
        let refused = |t: &Temp| {
            let src = t.write("src/x", "source");
            let e = crate::actions::rename_case_only(
                &src,
                &t.path("src/X"),
                crate::transfer::rename_noreplace,
            )
            .unwrap_err();
            assert!(e.to_string().contains("Nothing was changed"), "{e}");
            assert!(e.to_string().contains("state"), "{e}");
            assert!(t.hidden("src").is_empty());
            assert_eq!(fs::read(&src).unwrap(), b"source");
            fs::remove_file(&src).unwrap();
        };
        refused(&t);
        let found = recover();
        assert!(found.has_news(), "{found:?}");
        // A link to a folder of another user (the shared temporary folder
        // belongs to root): nothing is made there.
        let made = Path::new("/tmp").join(APP);
        let there = made.exists();
        fs::remove_file(t.path("state")).unwrap();
        std::os::unix::fs::symlink("/tmp", t.path("state")).unwrap();
        refused(&t);
        assert_eq!(made.exists(), there);
    }

    /// A record, written by hand, of an item now at `staging` (created
    /// with some bytes), left by a stopped program.
    fn forged(t: &Temp, kind: Kind, original: &Path, staging: &Path, target: &Path) -> String {
        fs::create_dir_all(staging.parent().unwrap()).unwrap();
        fs::write(staging, "victim").unwrap();
        let me = this_owner();
        let record = Record {
            owner: (me.0, me.1 + 1),
            kind,
            step: Step::Staged,
            original: original.to_owned(),
            staging: staging.to_owned(),
            target: target.to_owned(),
            parent: fs::metadata(original.parent().unwrap())
                .map(|m| (m.dev(), m.ino()))
                .unwrap_or_default(),
            identity: Identity::of(&fs::symlink_metadata(staging).unwrap()),
            reported: None,
        };
        let name = format!("{}-{}-77.op", me.0, me.1 + 1);
        let path = t.path(&format!("{RECORDS_REL}/{name}"));
        let _ = fs::remove_file(&path);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(record.to_text().as_bytes())
            .unwrap();
        name
    }

    #[test]
    fn a_record_whose_paths_are_not_one_of_our_operations_is_ignored() {
        let pid = std::process::id();
        let t = Temp::new("forged");
        fs::create_dir_all(t.path("dir")).unwrap();
        fs::create_dir_all(t.path("dest")).unwrap();
        let moving = |dir: &str, owner: u32| t.path(&format!("{dir}/.everygrep-moving-{owner}-3"));
        let cases: Vec<(&str, Kind, PathBuf, PathBuf, PathBuf)> = vec![
            // Any file, not a staging name of ours.
            (
                "not a staging name",
                Kind::MoveReplace,
                t.path("dir/a"),
                t.path("dest/victim"),
                t.path("dest/a"),
            ),
            // A staging name, but in another folder than the target's.
            (
                "another folder",
                Kind::MoveReplace,
                t.path("dir/a"),
                moving("other", pid),
                t.path("dest/a"),
            ),
            // A staging name of another program.
            (
                "another program",
                Kind::CaseRename,
                t.path("dir/a"),
                moving("dir", pid + 1),
                t.path("dir/A"),
            ),
            // A case rename into another folder.
            (
                "case rename elsewhere",
                Kind::CaseRename,
                t.path("dest/a"),
                moving("dir", pid),
                t.path("dir/A"),
            ),
            // An original path that climbs out.
            (
                "parent step",
                Kind::CaseRename,
                t.path("dir/../dir/a"),
                moving("dir", pid),
                t.path("dir/A"),
            ),
            (
                "not normal",
                Kind::CaseRename,
                PathBuf::from(format!("{}//a", t.path("dir").display())),
                moving("dir", pid),
                t.path("dir/A"),
            ),
            // A held item outside a holding folder of its parent.
            (
                "held elsewhere",
                Kind::Trash,
                t.path("dir/a"),
                t.path(&format!("dest/.everygrep-held-{pid}-0123456789abcdef/item")),
                PathBuf::new(),
            ),
            (
                "held without a holder",
                Kind::Rename,
                t.path("dir/a"),
                t.path("dir/item"),
                t.path("dest/a"),
            ),
        ];
        for (what, kind, original, staging, target) in cases {
            let name = forged(&t, kind, &original, &staging, &target);
            let found = recover();
            ignored_with_notice(&found, &staging, &original, &name);
            assert_eq!(fs::read(&staging).unwrap(), b"victim", "{what}");
            assert!(
                record_names(&t).iter().all(|n| !n.ends_with(".op")),
                "{what}: {:?}",
                record_names(&t)
            );
            fs::remove_file(&staging).unwrap();
        }
    }

    #[test]
    fn a_record_reads_back_with_any_path_bytes() {
        let meta = fs::symlink_metadata("/").unwrap();
        let record = Record {
            owner: (42, 7),
            kind: Kind::MoveReplace,
            step: Step::TargetTrashed,
            original: PathBuf::from(std::ffi::OsString::from_vec(
                b"/tmp/a b/\xff%x\nline".to_vec(),
            )),
            staging: PathBuf::from("/tmp/.everygrep-moving-1-0"),
            target: PathBuf::from("/tmp/dest/x"),
            parent: (3, 4),
            identity: Identity::of(&meta),
            reported: Some("its original name is taken".into()),
        };
        let text = record.to_text();
        assert_eq!(text.lines().count(), 16);
        assert_eq!(Record::parse(&text), Some(record));
    }

    #[test]
    fn a_partial_or_changed_record_is_not_read() {
        let meta = fs::symlink_metadata("/").unwrap();
        let record = Record {
            owner: (42, 7),
            kind: Kind::Trash,
            step: Step::Staged,
            original: PathBuf::from("/tmp/a"),
            staging: PathBuf::from("/tmp/.everygrep-held-1-2/item"),
            target: PathBuf::new(),
            parent: (3, 4),
            identity: Identity::of(&meta),
            reported: None,
        };
        let text = record.to_text();
        assert!(Record::parse(&text).is_some());
        for cut in 0..text.len() - 1 {
            assert_eq!(Record::parse(&text[..cut]), None, "cut at {cut}");
        }
        assert_eq!(Record::parse(&text.replace("/tmp/a", "tmp/a")), None);
        assert_eq!(
            Record::parse(&text.replace("kind trash", "kind other")),
            None
        );
        assert_eq!(Record::parse(&format!("{text}more\n")), None);
    }

    #[test]
    fn a_running_program_is_told_apart_from_one_that_reused_its_id() {
        let me = this_owner();
        assert!(me.1 > 0);
        assert!(owner_running(me));
        assert!(!owner_running((me.0, me.1 + 1)));
        // No process has this id (above the kernel's limit).
        assert!(!owner_running((u32::MAX, 1)));
    }
}
