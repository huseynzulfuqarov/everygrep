//! Session-only file reversal. Disk work stays on helper threads.
use crate::i18n::{fill, tr, trn};
use crate::{actions, clipboard::Operation, fileops_settings::Settings, transfer};
use everygrep_core::progress::Progress;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_TREE: usize = 100_000;
use crate::transfer::FileStamp as Stamp;
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot(Vec<(PathBuf, Stamp)>);
impl Snapshot {
    fn read(path: &Path) -> io::Result<Self> {
        Self::read_with(path, &Progress::new(), false)
    }
    /// `count`: adds each item read to `progress`, for the status line
    /// while a paste reads its sources before it starts.
    fn read_with(path: &Path, progress: &Progress, count: bool) -> io::Result<Self> {
        fn visit(
            root: &Path,
            rel: &Path,
            out: &mut Vec<(PathBuf, Stamp)>,
            progress: &Progress,
            count: bool,
        ) -> io::Result<()> {
            if progress.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if out.len() >= MAX_TREE {
                return Err(io::Error::other(tr(
                    "The folder is too large for undo history.",
                )));
            }
            let path = if rel.as_os_str().is_empty() {
                root.to_owned()
            } else {
                root.join(rel)
            };
            #[cfg(test)]
            crate::fileops_test::at("snapshot-entry", &path);
            let stamp = Stamp::read(&path)?;
            let is_dir = fs::symlink_metadata(&path)?.is_dir();
            out.push((rel.into(), stamp));
            if count {
                progress.add(1);
            }
            if is_dir {
                let mut names = fs::read_dir(path)?
                    .map(|e| {
                        if progress.is_cancelled() {
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        e.map(|e| e.file_name())
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                names.sort();
                for name in names {
                    visit(root, &rel.join(name), out, progress, count)?;
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        visit(path, Path::new(""), &mut out, progress, count)?;
        Ok(Self(out))
    }
    fn check(&self, path: &Path) -> Result<(), String> {
        match Self::read(path) {
            Ok(now) if now == *self => Ok(()),
            Ok(_) => Err(tr("The item changed since the recorded operation.").into()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Snapshot {
    fn trash_checked(
        &self,
        path: &Path,
        send: &crate::trashbin::Backend<'_>,
    ) -> Result<Stamp, String> {
        self.check(path)?;
        let before = BeforeRename::read(path).map_err(|e| e.to_string())?;
        if before.snapshot != *self {
            return Err(tr("The item changed since the recorded operation.").into());
        }
        let parent = std::sync::Arc::new(
            transfer::open_dir(
                path.parent()
                    .ok_or_else(|| tr("The original folder no longer exists.").to_owned())?,
            )
            .map_err(|e| e.to_string())?,
        );
        let name = path
            .file_name()
            .ok_or_else(|| tr("The item changed since the recorded operation.").to_owned())?;
        let stamp = self.0[0].1;
        let held = transfer::Isolation::take_recorded(
            parent,
            name,
            path,
            (stamp.dev, stamp.ino),
            Some((crate::oprecord::Kind::Trash, Path::new(""))),
        )
        .map_err(|e| e.to_string())?;
        let ready = before
            .after(&held.path())
            .map_err(|e| held.restore_error(e).to_string())?;
        #[cfg(test)]
        crate::fileops_test::at("trash-checked", path);
        if let Err(e) = send(&held.path(), path, ready.0[0].1) {
            return Err(held.restore_error(io::Error::other(e)).to_string());
        }
        // The expected entry is durably in the Trash. Only an empty private
        // holder is eligible for removal; unexpected children are kept.
        if let Err(e) = held.clear_folder() {
            crate::debug_log::note!(
                "trash holder cleanup at {}: {e}",
                held.recovery_path().display()
            );
        }
        Ok(Stamp::of(
            &before.root.metadata().map_err(|e| e.to_string())?,
        ))
    }
}
pub(crate) fn trash_file_checked(
    path: &Path,
    expected: Stamp,
    send: &crate::trashbin::Backend<'_>,
) -> Result<(), String> {
    Snapshot(vec![(PathBuf::new(), expected)])
        .trash_checked(path, send)
        .map(|_| ())
}

impl Snapshot {
    /// Holds the item at `path` aside; with `to`, recorded on disk as on
    /// its way there.
    fn isolate(
        &self,
        path: &Path,
        to: Option<&Path>,
    ) -> Result<(BeforeRename, transfer::Isolation, Snapshot), String> {
        self.check(path)?;
        let before = BeforeRename::read(path).map_err(|e| e.to_string())?;
        if before.snapshot != *self {
            return Err(tr("The item changed since the recorded operation.").into());
        }
        let parent = std::sync::Arc::new(
            transfer::open_dir(
                path.parent()
                    .ok_or_else(|| tr("The original folder no longer exists.").to_owned())?,
            )
            .map_err(|e| e.to_string())?,
        );
        let held = transfer::Isolation::take_recorded(
            parent,
            path.file_name()
                .ok_or_else(|| tr("The item changed since the recorded operation.").to_owned())?,
            path,
            (self.0[0].1.dev, self.0[0].1.ino),
            to.map(|to| (crate::oprecord::Kind::Rename, to)),
        )
        .map_err(|e| e.to_string())?;
        let ready = before
            .after(&held.path())
            .map_err(|e| held.restore_error(e).to_string())?;
        Ok((before, held, ready))
    }
    fn rename_checked(&self, from: &Path, to: &Path) -> Result<Self, String> {
        let source = self.0[0].1;
        let same_item = Stamp::read(to)
            .is_ok_and(|target| (target.dev, target.ino) == (source.dev, source.ino));
        if !same_item {
            ensure_absent(to)?;
        }
        let (before, held, ready) = self.isolate(from, Some(to))?;
        if let Err(e) = transfer::rename_expected(&held.path(), to, ready.0[0].1) {
            return Err(undo_io_error(held.restore_error(e)));
        }
        let result = before.after(to).map_err(|e| e.to_string());
        if let Err(e) = held.clear_folder() {
            crate::debug_log::note!("rename holder cleanup: {e}");
        }
        result
    }
    fn remove_empty_checked(&self, path: &Path) -> Result<(), String> {
        if self.0.len() != 1 {
            return Err(tr("The item changed since the recorded operation.").into());
        }
        let (_, held, _) = self.isolate(path, None)?;
        held.remove(true).map_err(|e| e.to_string())
    }
}

/// The inode selected before rename stays pinned until its resulting snapshot
/// is checked. A post-publication pathname cannot replace this identity.
#[derive(Clone)]
struct BeforeRename {
    snapshot: Snapshot,
    root: std::sync::Arc<fs::File>,
}
impl BeforeRename {
    fn read(path: &Path) -> io::Result<Self> {
        Self::read_with(path, &Progress::new(), false)
    }
    fn read_with(path: &Path, progress: &Progress, count: bool) -> io::Result<Self> {
        let root = std::sync::Arc::new(transfer::pin_path(path)?);
        let snapshot = Snapshot::read_with(path, progress, count)?;
        if Stamp::of(&root.metadata()?) != snapshot.0[0].1 {
            return Err(io::Error::other(tr(
                "The item changed since the recorded operation.",
            )));
        }
        Ok(Self { snapshot, root })
    }
    fn after(&self, path: &Path) -> io::Result<Snapshot> {
        let after = Snapshot::read(path)?;
        let held = Stamp::of(&self.root.metadata()?);
        if after.0.len() != self.snapshot.0.len()
            || after.0[0].1 != held
            || !self.snapshot.0[0].1.same_except_ctime(&held)
            || self
                .snapshot
                .0
                .iter()
                .zip(&after.0)
                .enumerate()
                .any(|(i, (before, after))| before.0 != after.0 || (i > 0 && before.1 != after.1))
        {
            return Err(io::Error::other(tr(
                "The item changed since the recorded operation.",
            )));
        }
        Ok(after)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Rename,
    Move,
    Copy,
    /// A folder made by New folder: `from` and `to` are its path.
    NewFolder,
    /// A file sent to the trash: `from` and `to` are where it was.
    Trash,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Rename => tr("Rename"),
            Self::Move => tr("Move"),
            Self::Copy => tr("Copy"),
            Self::NewFolder => tr("New folder"),
            Self::Trash => tr("Move to trash"),
        }
    }
    /// The fixed English word in exported history.
    fn word(self) -> &'static str {
        match self {
            Self::Rename => "Rename",
            Self::Move => "Move",
            Self::Copy => "Copy",
            Self::NewFolder => "New folder",
            Self::Trash => "Trash",
        }
    }
}
#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    kind: Kind,
    from: PathBuf,
    to: PathBuf,
    expected: Snapshot,
    source: Option<Snapshot>,
    applied: bool,
    skipped: Option<String>,
}
#[derive(Default)]
struct Journal {
    entries: Vec<Entry>,
    cursor: usize,
    epoch: u64,
    next_id: u64,
}
#[derive(Clone, Copy)]
struct Config {
    enabled: bool,
    limit: usize,
    epoch: u64,
}
static CONFIG: Mutex<Config> = Mutex::new(Config {
    enabled: false,
    limit: 100,
    epoch: 0,
});
static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    entries: Vec::new(),
    cursor: 0,
    epoch: 0,
    next_id: 0,
});
// Serializes this program's destructive operations, never held by the UI.
static FILES: Mutex<()> = Mutex::new(());

type QueueNotice = Box<dyn Fn(bool)>;
thread_local! {
    static HELD_FILES: std::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> = const { std::cell::RefCell::new(None) };
    static QUEUED: std::cell::RefCell<Option<QueueNotice>> = const { std::cell::RefCell::new(None) };
}
pub struct FileGuard(std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for FileGuard {
    fn drop(&mut self) {
        HELD_FILES.with(|held| {
            held.borrow_mut().take();
        });
        crate::fileops_undo_ui::request_refresh();
    }
}
fn held(guard: std::sync::MutexGuard<'static, ()>) -> FileGuard {
    HELD_FILES.with(|held| *held.borrow_mut() = Some(guard));
    crate::fileops_undo_ui::request_refresh();
    FileGuard(std::marker::PhantomData)
}
/// Takes the slot without telling the UI, recovering it after a worker fault.
fn try_files() -> Option<std::sync::MutexGuard<'static, ()>> {
    match FILES.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}
fn try_file_guard() -> Option<FileGuard> {
    try_files().map(held)
}
pub fn queue_notice(weak: slint::Weak<crate::AppWindow>) {
    QUEUED.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |queued| {
            let _ = weak.upgrade_in_event_loop(move |w| {
                crate::info(
                    &w,
                    (if queued {
                        tr("File operation queued.")
                    } else {
                        tr("File operation running…")
                    })
                    .into(),
                )
            });
        }))
    });
}
fn wait_for_files() -> std::sync::MutexGuard<'static, ()> {
    match FILES.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            QUEUED.with(|slot| {
                if let Some(notice) = slot.borrow().as_ref() {
                    notice(true);
                }
            });
            let guard = FILES.lock().unwrap_or_else(|e| e.into_inner());
            QUEUED.with(|slot| {
                if let Some(notice) = slot.borrow().as_ref() {
                    notice(false);
                }
            });
            guard
        }
    }
}
pub fn file_guard() -> FileGuard {
    held(wait_for_files())
}
/// A user question reserves no disk-operation slot. The caller revalidates
/// source and destination after the answer and before changing either.
pub fn while_waiting<T>(wait: impl FnOnce() -> T) -> T {
    let released = HELD_FILES.with(|held| held.borrow_mut().take());
    let resume = released.is_some();
    drop(released);
    let result = wait();
    if resume {
        HELD_FILES.with(|held| *held.borrow_mut() = Some(wait_for_files()));
    }
    result
}
pub fn catch_worker<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map_err(|_| {
        let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        journal.entries.clear();
        journal.cursor = 0;
        JOURNAL.clear_poison();
        FILES.clear_poison();
        tr("The file operation stopped unexpectedly. Undo history was cleared. Try again.")
            .to_owned()
    })
}

pub fn configure(s: &Settings) {
    let mut c = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if c.enabled != s.undo_enabled {
        c.epoch = c.epoch.wrapping_add(1);
    }
    c.enabled = s.undo_enabled;
    c.limit = s.undo_limit;
    drop(c);
    crate::fileops_undo_ui::request_refresh();
}
fn config() -> Config {
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner())
}
/// A non-blocking snapshot for menu actions; workers may hold the journal
/// while reading trees or waiting for a transfer decision. Reading asks for
/// no refresh: the UI refreshes by calling this, so a request here would
/// queue refreshes without end.
pub fn availability() -> (bool, bool) {
    let Some(_files) = try_files() else {
        return (false, false);
    };
    let Ok(mut journal) = JOURNAL.try_lock() else {
        return (false, false);
    };
    let config = config();
    journal.sync(config);
    journal.availability(config.enabled)
}

struct SkippedItem {
    name: String,
    reason: String,
}

#[derive(Default)]
struct StepOutcome {
    done: usize,
    skipped: Vec<SkippedItem>,
}

impl Journal {
    fn advance_rename(&mut self, before: Stamp, after: Stamp) {
        if (before.dev, before.ino) != (after.dev, after.ino) {
            return;
        }
        for other in &mut self.entries {
            for snapshot in std::iter::once(&mut other.expected).chain(other.source.iter_mut()) {
                for (_, stamp) in &mut snapshot.0 {
                    if *stamp == before {
                        *stamp = after;
                    }
                }
            }
        }
    }
    fn availability(&self, enabled: bool) -> (bool, bool) {
        (
            enabled && (self.cursor > 0 || self.retryable_undo_end().is_some()),
            enabled && self.cursor < self.entries.len(),
        )
    }

    // A failed undo remains applied. Retrying the adjacent group repeats
    // the original identity, snapshot and no-replace checks for each item.
    fn retryable_undo_end(&self) -> Option<usize> {
        let id = self.entries.get(self.cursor)?.id;
        let end = self.entries[self.cursor..]
            .iter()
            .position(|e| e.id != id)
            .map_or(self.entries.len(), |n| self.cursor + n);
        self.entries[self.cursor..end]
            .iter()
            .any(|e| e.applied && e.skipped.is_some())
            .then_some(end)
    }

    fn sync(&mut self, c: Config) {
        if !c.enabled || c.epoch != self.epoch {
            self.entries.clear();
            self.cursor = 0;
            self.epoch = c.epoch;
        }
        while self.entries.chunk_by(|a, b| a.id == b.id).count() > c.limit {
            let id = self.entries[0].id;
            let n = self.entries.iter().take_while(|e| e.id == id).count();
            self.entries.drain(..n);
            self.cursor = self.cursor.saturating_sub(n);
        }
    }
    /// `None` when undo is off or was switched since the operation began.
    fn push_group(&mut self, entries: Vec<Entry>, c: Config) -> Option<Result<(), bool>> {
        let now = config();
        self.sync(now);
        if !now.enabled || now.epoch != c.epoch {
            return None;
        }
        Some(self.append_group(entries, now))
    }
    /// An operation changed the disk but cannot be recorded: drops the
    /// older steps, so that undo never skips over it. True when steps that
    /// could still be undone were dropped.
    fn forget(&mut self, c: Config) -> bool {
        let now = config();
        self.sync(now);
        if !now.enabled || now.epoch != c.epoch {
            return false;
        }
        let dropped = self.cursor > 0;
        self.entries.clear();
        self.cursor = 0;
        dropped
    }
    /// Records one operation as one step. `Err(dropped)` when it is too
    /// large to record; `dropped` tells whether undoable steps were removed.
    fn append_group(&mut self, mut entries: Vec<Entry>, c: Config) -> Result<(), bool> {
        for entry in &mut entries {
            if entry.kind == Kind::Move
                && let Some(before) = entry.source.take()
            {
                self.advance_rename(before.0[0].1, entry.expected.0[0].1);
            }
        }
        // A new disk operation invalidates redo, even if it cannot be recorded.
        self.entries.truncate(self.cursor);
        let weight = |e: &Entry| e.expected.0.len() + e.source.as_ref().map_or(0, |s| s.0.len());
        let added = entries.iter().map(weight).sum::<usize>();
        if added > MAX_TREE {
            // Never retain just a tail of a paste. Older steps go too, so
            // that undo never skips over this operation.
            let dropped = self.cursor > 0;
            self.entries.clear();
            self.cursor = 0;
            return Err(dropped);
        }
        if entries.is_empty() {
            return Ok(());
        }
        // Make room by dropping the oldest whole operations.
        let mut held = self.entries.iter().map(weight).sum::<usize>();
        while added + held > MAX_TREE {
            let id = self.entries[0].id;
            let n = self.entries.iter().take_while(|e| e.id == id).count();
            held -= self.entries[..n].iter().map(weight).sum::<usize>();
            self.entries.drain(..n);
        }
        self.next_id = self.next_id.wrapping_add(1);
        for e in &mut entries {
            e.id = self.next_id;
        }
        self.entries.extend(entries);
        self.cursor = self.entries.len();
        self.sync(c);
        Ok(())
    }
    fn step(
        &mut self,
        redo: bool,
        trash: &impl Fn(&Path, &Path, Stamp) -> Result<(), String>,
        progress: &Progress,
    ) -> Option<StepOutcome> {
        if !redo && let Some(end) = self.retryable_undo_end() {
            let cursor = self.cursor;
            let mut outcome = StepOutcome::default();
            for index in (cursor..end).rev() {
                if progress.is_cancelled() {
                    break;
                }
                let e = &mut self.entries[index];
                if !e.applied || e.skipped.is_none() {
                    continue;
                }
                let name = crate::format::visible_name(
                    &e.to
                        .file_name()
                        .unwrap_or(e.to.as_os_str())
                        .to_string_lossy(),
                )
                .into_owned();
                e.skipped = None;
                self.cursor = index + 1;
                match self.step_one(false, trash, progress)? {
                    Ok(()) => outcome.done += 1,
                    Err(reason) => {
                        // Cancellation returns before step_one records a skip.
                        // Preserve the applied entry so another Undo can retry.
                        self.entries[index].skipped = Some(reason.clone());
                        outcome.skipped.push(SkippedItem { name, reason });
                    }
                }
            }
            self.cursor = cursor;
            return Some(outcome);
        }
        let index = if redo {
            self.cursor
        } else {
            self.cursor.checked_sub(1)?
        };
        let id = self.entries.get(index)?.id;
        let mut outcome = StepOutcome::default();
        loop {
            let index = if redo {
                Some(self.cursor)
            } else {
                self.cursor.checked_sub(1)
            };
            if index
                .and_then(|i| self.entries.get(i))
                .is_none_or(|e| e.id != id)
            {
                break;
            }
            let entry = &self.entries[index?];
            let path = if entry.applied {
                &entry.to
            } else {
                &entry.from
            };
            let name = crate::format::visible_name(
                &path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy(),
            )
            .into_owned();
            match self.step_one(redo, trash, progress)? {
                Ok(()) => outcome.done += 1,
                Err(reason) => outcome.skipped.push(SkippedItem { name, reason }),
            }
            if progress.is_cancelled() {
                break;
            }
        }
        Some(outcome)
    }
    fn step_one(
        &mut self,
        redo: bool,
        trash: &impl Fn(&Path, &Path, Stamp) -> Result<(), String>,
        progress: &Progress,
    ) -> Option<Result<(), String>> {
        let index = if redo {
            if self.cursor >= self.entries.len() {
                return None;
            }
            self.cursor
        } else {
            self.cursor.checked_sub(1)?
        };
        let e = &mut self.entries[index];
        let old_copy = (redo && e.kind == Kind::Copy).then(|| (e.to.clone(), e.expected.clone()));
        let old_stamp = e.expected.0[0].1;
        let result = if let Some(why) = &e.skipped {
            Err(why.clone())
        } else if e.applied == redo {
            Err(tr("This step is no longer available.").into())
        } else {
            e.reverse(redo, trash, progress)
        };
        if result.is_err() && progress.is_cancelled() {
            return Some(result);
        }
        if let Err(reason) = &result {
            e.skipped = Some(reason.clone());
        }
        if result.is_ok() {
            let new_stamp = self.entries[index].expected.0[0].1;
            if self.entries[index].kind != Kind::Copy {
                self.advance_rename(old_stamp, new_stamp);
            }
            if let Some((path, old)) = old_copy {
                let new = self.entries[index].expected.clone();
                for (i, other) in self.entries.iter_mut().enumerate() {
                    if i == index {
                        continue;
                    }
                    let live = if other.applied {
                        &other.to
                    } else {
                        &other.from
                    };
                    if *live == path && other.expected == old {
                        other.expected = new.clone();
                    }
                    if other.from == path && other.source.as_ref() == Some(&old) {
                        other.source = Some(new.clone());
                    }
                }
            }
        }
        self.cursor = if redo { index + 1 } else { index };
        Some(result)
    }
}
impl Entry {
    fn reverse(
        &mut self,
        redo: bool,
        trash: &impl Fn(&Path, &Path, Stamp) -> Result<(), String>,
        progress: &Progress,
    ) -> Result<(), String> {
        if progress.is_cancelled() {
            return Err(tr("Operation cancelled.").into());
        }
        if self.kind == Kind::NewFolder {
            if redo {
                ensure_absent(&self.to)?;
                fs::create_dir(&self.to).map_err(|e| e.to_string())?;
                self.expected = Snapshot::read(&self.to).map_err(|e| e.to_string())?;
            } else {
                // Unchanged means still empty: anything put in it since
                // changed the folder's modification time. `remove_dir`
                // removes empty folders only.
                self.expected.remove_empty_checked(&self.to)?;
            }
        } else if self.kind == Kind::Trash {
            if redo {
                self.expected.check(&self.from)?;
                self.expected.0[0].1 = self.expected.trash_checked(&self.from, trash)?;
            } else {
                let stamp = self.expected.0[0].1;
                let identity = crate::trashbin::Identity {
                    dev: stamp.dev,
                    ino: stamp.ino,
                    len: stamp.len,
                    mtime: stamp.mtime,
                };
                let found = crate::trashbin::find(&self.from, identity)
                    .ok_or_else(|| tr("The item is no longer in the trash.").to_owned())?;
                let info = crate::transfer::pin_path(&found.info).ok();
                self.expected = self.expected.rename_checked(&found.file, &self.from)?;
                if let Some(info) = info {
                    let _ = crate::trashbin::remove_description(&found.info, &info);
                }
            }
        } else if self.kind == Kind::Copy {
            if redo {
                self.source
                    .as_ref()
                    .ok_or_else(|| {
                        tr("The copy source was not stable enough to record.").to_owned()
                    })?
                    .check(&self.from)?;
                ensure_absent(&self.to)?;
                let expected = Snapshot(
                    transfer::copy_exact(&self.from, &self.to, progress)
                        .map_err(|e| e.to_string())?,
                );
                #[cfg(test)]
                crate::fileops_test::at("redo-copy-journal", &self.to);
                expected.check(&self.to)?;
                self.expected = expected;
            } else {
                self.expected.check(&self.to)?;
                self.expected.trash_checked(&self.to, trash)?;
            }
        } else {
            let (from, to) = if redo {
                (&self.from, &self.to)
            } else {
                (&self.to, &self.from)
            };
            self.expected = self.expected.rename_checked(from, to)?;
        }
        self.applied = redo;
        Ok(())
    }
}

#[cfg(test)]
fn reverse_rename(
    from: &Path,
    to: &Path,
    case_alias: bool,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), String> {
    if !case_alias {
        ensure_absent(to)?;
        return rename(from, to).map_err(|e| e.to_string());
    }
    // Once the old spelling is staged, both spellings are vacant on a
    // case-insensitive filesystem; publication never replaces another entry.
    let parent = from
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput).to_string())?;
    let (staged, record) =
        transfer::stage_rename(from, parent, crate::oprecord::Kind::CaseRename, to)
            .map_err(|e| e.to_string())?;
    let result = ensure_absent(to).and_then(|()| rename(&staged, to).map_err(|e| e.to_string()));
    if result.is_err()
        && let Err(e) = rename(&staged, from)
    {
        return Err(fill(
            tr("Could not restore the source; it remains at {0}: {1}"),
            &[&staged.display(), &e],
        ));
    }
    crate::oprecord::finish(record);
    result
}

fn undo_io_error(error: io::Error) -> String {
    if error.kind() == io::ErrorKind::AlreadyExists {
        tr("The destination already exists.").into()
    } else {
        error.to_string()
    }
}

fn ensure_absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(tr("The destination already exists.").into()),
        Err(e) => Err(e.to_string()),
    }
}

/// [`rename_chosen`] of the item at `old` as it is now.
#[cfg(test)]
pub fn rename(old: &Path, name: &str) -> Result<Option<String>, actions::RenameError> {
    rename_chosen(old, name, &actions::Chosen::of(&[old.to_path_buf()]))
}

/// Renames, and records the rename when undo is on. The answer is a notice
/// for the user when undo history could not follow the rename. Only the
/// item `chosen` when the rename started is renamed: another item put at
/// `old` meanwhile is refused.
pub fn rename_chosen(
    old: &Path,
    name: &str,
    chosen: &actions::Chosen,
) -> Result<Option<String>, actions::RenameError> {
    let _files = try_file_guard().ok_or_else(|| {
        actions::RenameError::Io(io::Error::other(tr("A file operation is still running.")))
    })?;
    let c = config();
    let before = before_chosen(old, chosen).map_err(actions::RenameError::Io)?;
    let result = actions::rename(old, name)?;
    let mut note = None;
    if c.enabled
        && let Some(to) = &result
    {
        let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        crate::fileops_test::at("rename-journal", to);
        match before
            .as_ref()
            .ok_or_else(|| io::Error::other(tr("The item changed since the recorded operation.")))
            .and_then(|b| b.after(to))
        {
            Ok(expected) => {
                journal.advance_rename(before.as_ref().unwrap().snapshot.0[0].1, expected.0[0].1);
                // A rename tree always fits after older steps go.
                let _ = journal.push_group(
                    vec![Entry {
                        id: 0,
                        kind: Kind::Rename,
                        from: old.into(),
                        to: to.clone(),
                        expected,
                        source: None,
                        applied: true,
                        skipped: None,
                    }],
                    c,
                );
            }
            Err(_) => {
                let what = tr("The rename could not be recorded for undo.");
                note = Some(if journal.forget(c) {
                    format!("{what} {}", tr("Earlier undo steps were removed."))
                } else {
                    what.into()
                });
            }
        }
    }
    Ok(note)
}

/// The undo snapshot of the item at `old`, which must be one of the items
/// `chosen`. Checked before and again after the snapshot, which can take
/// long for a big folder: an item put in place meanwhile is refused, also
/// when it made the snapshot fail, and the snapshot is of the item checked.
/// `None` when the item is still the one chosen but no snapshot could be
/// read (a file inside could not be read, or the folder is too large): it
/// is changed without undo then, and the user is told.
fn before_chosen(old: &Path, chosen: &actions::Chosen) -> io::Result<Option<BeforeRename>> {
    let still_chosen = || -> io::Result<fs::Metadata> {
        let now = fs::symlink_metadata(old)?;
        chosen.check(&now, false)?;
        Ok(now)
    };
    still_chosen()?;
    let before = BeforeRename::read(old);
    let now = Stamp::of(&still_chosen()?);
    match before {
        Ok(b) => {
            let pinned = Stamp::of(&b.root.metadata()?);
            if (pinned.dev, pinned.ino) != (now.dev, now.ino) {
                return Err(actions::changed_since_chosen());
            }
            Ok(Some(b))
        }
        Err(_) => Ok(None),
    }
}

/// What a batch rename did.
#[derive(Debug, Default)]
pub struct BatchOutcome {
    /// Items renamed, as (old path, new path).
    pub done: Vec<(PathBuf, PathBuf)>,
    /// Items not renamed, with the reason.
    pub failed: Vec<(PathBuf, String)>,
    /// A notice when undo history could not follow.
    pub note: Option<String>,
}

/// Renames each item to its new name, never replacing anything, and
/// records the renamed ones as one undo step. An item that cannot be
/// renamed (its name taken meanwhile, gone, no permission, or not one of
/// the items `chosen` when the preview opened) is reported and the others
/// go on; undo then reverses the ones that were renamed.
pub fn rename_batch(
    items: &[(PathBuf, String)],
    chosen: &actions::Chosen,
) -> Result<BatchOutcome, String> {
    let _files =
        try_file_guard().ok_or_else(|| tr("A file operation is still running.").to_owned())?;
    let c = config();
    let mut out = BatchOutcome::default();
    let mut stamps = Vec::new();
    for (from, name) in items {
        let before = match before_chosen(from, chosen) {
            Ok(before) => before,
            Err(e) => {
                out.failed.push((from.clone(), e.to_string()));
                continue;
            }
        };
        match actions::rename(from, name) {
            Ok(Some(to)) => {
                #[cfg(test)]
                crate::fileops_test::at("rename-journal", &to);
                let expected = before.as_ref().and_then(|b| b.after(&to).ok());
                stamps.push((before, expected));
                out.done.push((from.clone(), to));
            }
            Ok(None) => {}
            Err(e) => out.failed.push((from.clone(), e.to_string())),
        }
    }
    if !c.enabled || out.done.is_empty() {
        return Ok(out);
    }
    let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = Vec::new();
    for ((from, to), (before, expected)) in out.done.iter().zip(stamps) {
        let Some(expected) = expected else {
            entries.clear();
            break;
        };
        if let Some(before) = before {
            journal.advance_rename(before.snapshot.0[0].1, expected.0[0].1);
        }
        entries.push(Entry {
            id: 0,
            kind: Kind::Rename,
            from: from.clone(),
            to: to.clone(),
            expected,
            source: None,
            applied: true,
            skipped: None,
        });
    }
    let what = tr("The rename could not be recorded for undo.");
    // Never a step with only some of the renamed items.
    let dropped = if entries.len() != out.done.len() {
        Some(journal.forget(c))
    } else {
        match journal.push_group(entries, c) {
            Some(Err(dropped)) => Some(dropped),
            _ => None,
        }
    };
    out.note = dropped.map(|dropped| {
        if dropped {
            format!("{what} {}", tr("Earlier undo steps were removed."))
        } else {
            what.into()
        }
    });
    Ok(out)
}

/// What cleaning up duplicates did.
#[derive(Debug, Default)]
pub struct DupesOutcome {
    pub trashed: Vec<PathBuf>,
    /// Bytes of the trashed files.
    pub bytes: u64,
    /// Copies left where they were, and why.
    pub kept: Vec<(PathBuf, String)>,
    pub note: Option<String>,
    pub cancelled: bool,
}

fn left(out: &mut DupesOutcome, path: &Path, why: &str) {
    out.kept.push((path.to_owned(), why.to_owned()));
}

/// Whether the opened file is the copy the preview saw.
fn opened_as_seen(
    file: &everygrep_core::hash::OpenFile,
    seen: &crate::fileops_dupes::Copy,
) -> bool {
    let k = file.key;
    k.dev == seen.dev
        && k.ino == seen.ino
        && k.size == seen.size
        && k.mtime_ns
            == seen
                .mtime
                .0
                .saturating_mul(1_000_000_000)
                .saturating_add(seen.mtime.1)
}

/// Sends the copies the preview marked for the trash there, group by
/// group, and records them as one undo step. A copy goes only when, just
/// before: the group's first kept copy is still the file of the preview
/// and at its place, the copy is too, it is not the same file (a hard
/// link), and both hold the same bytes. The kept copy is checked once
/// more while the copy is held aside, right before it is sent. Anything
/// else leaves the copy where it is, with the reason.
pub fn trash_duplicates(
    groups: &[crate::fileops_dupes::Group],
    trash: &dyn Fn(&Path, &Path, Stamp) -> Result<(), String>,
    progress: &Progress,
) -> DupesOutcome {
    use everygrep_core::hash::{self, CompareError};
    let _files = file_guard();
    let c = config();
    let mut out = DupesOutcome::default();
    let mut entries = Vec::new();
    'groups: for g in groups {
        if g.skipped.is_some() {
            continue;
        }
        // Never without a copy that stays.
        let Some(keeper) = g.keep.first() else {
            continue;
        };
        let leave_all = |out: &mut DupesOutcome, why: &str| {
            for t in &g.trash {
                out.kept.push((t.path.clone(), why.to_owned()));
            }
        };
        let kept_file = match hash::open(&keeper.path) {
            Ok(f) if opened_as_seen(&f, keeper) => f,
            _ => {
                leave_all(
                    &mut out,
                    tr("The copy to keep changed or is gone since the preview."),
                );
                continue;
            }
        };
        for (n, t) in g.trash.iter().enumerate() {
            if progress.is_cancelled() {
                out.cancelled = true;
                break 'groups;
            }
            let file = match hash::open(&t.path) {
                Ok(f) if opened_as_seen(&f, t) => f,
                _ => {
                    left(
                        &mut out,
                        &t.path,
                        tr("It changed or is gone since the preview."),
                    );
                    continue;
                }
            };
            if (t.dev, t.ino) == (keeper.dev, keeper.ino) {
                left(
                    &mut out,
                    &t.path,
                    tr("The same file as another copy (a hard link): left as it is."),
                );
                continue;
            }
            match hash::same_contents(&kept_file, &file, progress) {
                Ok(true) => {}
                Ok(false) => {
                    left(
                        &mut out,
                        &t.path,
                        tr("Its contents differ from the copy that stays."),
                    );
                    continue;
                }
                Err(CompareError::Cancelled) => {
                    out.cancelled = true;
                    break 'groups;
                }
                Err(CompareError::Unreadable { .. }) => {
                    left(
                        &mut out,
                        &t.path,
                        tr("It could not be compared with the copy that stays."),
                    );
                    continue;
                }
            }
            drop(file);
            // Both still at their places, as compared.
            if !keeper.unchanged() {
                for t in &g.trash[n..] {
                    out.kept.push((
                        t.path.clone(),
                        tr("The copy to keep changed or is gone since the preview.").into(),
                    ));
                }
                continue 'groups;
            }
            let Some(mut expected) = Snapshot::read(&t.path).ok().filter(|_| t.unchanged()) else {
                left(
                    &mut out,
                    &t.path,
                    tr("It changed or is gone since the preview."),
                );
                continue;
            };
            // The copy is held aside now. Right before it leaves, the
            // copy that stays must still be there, as it was compared;
            // otherwise this one goes back to its place.
            let guarded = |actual: &Path, original: &Path, stamp: Stamp| {
                if !kept_file.unchanged() || !keeper.unchanged() {
                    return Err(
                        tr("The copy to keep changed or is gone, so this copy was kept.")
                            .to_owned(),
                    );
                }
                trash(actual, original, stamp)
            };
            match expected.trash_checked(&t.path, &guarded) {
                Ok(stamp) => expected.0[0].1 = stamp,
                Err(e) => {
                    left(&mut out, &t.path, &e);
                    continue;
                }
            }
            out.trashed.push(t.path.clone());
            out.bytes += t.size;
            entries.push(Entry {
                id: 0,
                kind: Kind::Trash,
                from: t.path.clone(),
                to: t.path.clone(),
                expected,
                source: None,
                applied: true,
                skipped: None,
            });
        }
    }
    if c.enabled && !entries.is_empty() {
        let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(Err(dropped)) = journal.push_group(entries, c) {
            let what = tr("The operation could not be recorded for undo.");
            out.note = Some(if dropped {
                format!("{what} {}", tr("Earlier undo steps were removed."))
            } else {
                what.into()
            });
        }
    }
    out
}

/// Per-item results survive a partial trash failure, including later items.
#[derive(Debug, Default)]
pub struct TrashOutcome {
    pub trashed: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub note: Option<String>,
}

/// Plain Trash uses the same identity snapshots and grouped journal as
/// duplicate cleanup. Individual desktop calls cannot hide partial batches.
/// Only the items `chosen` (when Trash was used or its question opened)
/// are moved: another item put at one of the paths meanwhile is kept and
/// reported.
pub fn trash(
    paths: &[PathBuf],
    chosen: &actions::Chosen,
    send: &dyn Fn(&Path, &Path, Stamp) -> Result<(), String>,
) -> TrashOutcome {
    let _files = file_guard();
    let c = config();
    let mut out = TrashOutcome::default();
    let mut entries = Vec::new();
    for path in paths {
        // The snapshot must be of the item chosen; `trash_checked` then
        // moves only the item the snapshot was read from.
        let chosen_now = || -> io::Result<Stamp> {
            let now = fs::symlink_metadata(path)?;
            chosen.check(&now, false)?;
            Ok(Stamp::of(&now))
        };
        let read = chosen_now().and_then(|_| {
            let expected = Snapshot::read(path)?;
            let now = chosen_now()?;
            let root = expected.0[0].1;
            if (root.dev, root.ino) != (now.dev, now.ino) {
                return Err(actions::changed_since_chosen());
            }
            Ok(expected)
        });
        let mut expected = match read {
            Ok(expected) => expected,
            Err(e) => {
                out.failed.push((path.clone(), e.to_string()));
                continue;
            }
        };
        match expected.trash_checked(path, send) {
            Ok(stamp) => expected.0[0].1 = stamp,
            Err(error) => {
                out.failed.push((path.clone(), error));
                continue;
            }
        }
        out.trashed.push(path.clone());
        entries.push(Entry {
            id: 0,
            kind: Kind::Trash,
            from: path.clone(),
            to: path.clone(),
            expected,
            source: None,
            applied: true,
            skipped: None,
        });
    }
    if c.enabled && !entries.is_empty() {
        let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(Err(dropped)) = journal.push_group(entries, c) {
            let what = tr("The operation could not be recorded for undo.");
            out.note = Some(if dropped {
                format!("{what} {}", tr("Earlier undo steps were removed."))
            } else {
                what.into()
            });
        }
    }
    out
}

/// Makes the folder `name` in `parent`, and records it when undo is on.
/// Never takes an existing name: the caller offers a free one. The answer
/// has a notice for the user when undo history could not follow.
pub fn new_folder(
    parent: &Path,
    name: &str,
) -> Result<(PathBuf, Option<String>), actions::RenameError> {
    let _files = try_file_guard().ok_or_else(|| {
        actions::RenameError::Io(io::Error::other(tr("A file operation is still running.")))
    })?;
    actions::validate_name(name)?;
    if !fs::metadata(parent).is_ok_and(|m| m.is_dir()) {
        return Err(actions::RenameError::Io(io::Error::other(tr(
            "The folder no longer exists.",
        ))));
    }
    let c = config();
    let target = parent.join(name);
    fs::create_dir(&target).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            actions::RenameError::Exists(name.to_owned())
        } else if e.raw_os_error() == Some(36) {
            actions::RenameError::TooLong
        } else {
            actions::RenameError::Io(e)
        }
    })?;
    let note = record_one(Kind::NewFolder, &target, &target, c);
    Ok((target, note))
}

/// Records one applied item as one step when undo is on. The answer is a
/// notice for the user when it could not be recorded.
fn record_one(kind: Kind, from: &Path, to: &Path, c: Config) -> Option<String> {
    if !c.enabled {
        return None;
    }
    let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    match Snapshot::read(to) {
        Ok(expected) => {
            let _ = journal.push_group(
                vec![Entry {
                    id: 0,
                    kind,
                    from: from.into(),
                    to: to.into(),
                    expected,
                    source: None,
                    applied: true,
                    skipped: None,
                }],
                c,
            );
            None
        }
        Err(_) => {
            let what = tr("The operation could not be recorded for undo.");
            Some(if journal.forget(c) {
                format!("{what} {}", tr("Earlier undo steps were removed."))
            } else {
                what.into()
            })
        }
    }
}

/// Copies or moves `sources` into `dest` and records it for undo. With
/// `chosen`, only those items are taken (see [`transfer::paste_chosen`]).
pub fn paste(
    op: Operation,
    sources: &[PathBuf],
    chosen: Option<&actions::Chosen>,
    dest: &Path,
    progress: &Progress,
    resolver: Option<transfer::Resolver>,
) -> transfer::Outcome {
    paste_using(
        op,
        sources,
        chosen,
        dest,
        progress,
        resolver,
        transfer::Replacement::default(),
    )
}

/// [`paste`] with the desktop trash replaceable, for tests.
fn paste_using(
    op: Operation,
    sources: &[PathBuf],
    chosen: Option<&actions::Chosen>,
    dest: &Path,
    progress: &Progress,
    resolver: Option<transfer::Resolver>,
    replacement: transfer::Replacement,
) -> transfer::Outcome {
    let _files = file_guard();
    let c = config();
    let resolver = resolver.map(|resolve| -> transfer::Resolver {
        Box::new(move |question, progress| while_waiting(|| resolve(question, progress)))
    });
    let before: Vec<_> = if c.enabled {
        let mut budget = MAX_TREE;
        sources
            .iter()
            .map(|p| {
                if budget == 0 {
                    return None;
                }
                let snapshot = BeforeRename::read_with(p, progress, true).ok()?;
                if snapshot.snapshot.0.len() > budget {
                    return None;
                }
                budget -= snapshot.snapshot.0.len();
                Some(snapshot)
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut published = Vec::new();
    let mut budget = MAX_TREE;
    let mut out = if c.enabled {
        transfer::paste_chosen(
            op,
            sources,
            chosen,
            dest,
            progress,
            resolver,
            replacement,
            |r| {
                // Not counted: the transfer counts bytes by then.
                let snapshot = if let Some(produced) = &r.produced {
                    let expected = Snapshot(produced.snapshot());
                    expected.check(&r.target).ok().map(|()| expected)
                } else if op == Operation::Cut {
                    sources
                        .iter()
                        .position(|s| s == &r.source)
                        .and_then(|i| before[i].as_ref())
                        .and_then(|b| b.after(&r.target).ok())
                } else {
                    None
                }
                .filter(|s| s.0.len() <= budget);
                if let Some(s) = &snapshot {
                    budget -= s.0.len();
                }
                published.push(snapshot);
            },
        )
    } else {
        transfer::paste_chosen(
            op,
            sources,
            chosen,
            dest,
            progress,
            resolver,
            replacement,
            |_| {},
        )
    };
    if c.enabled {
        let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries = Vec::new();
        let mut incomplete = false;
        for (r, expected) in out.records.iter().zip(published) {
            if r.overwritten {
                out.history_omitted = true;
                continue;
            }
            if let Some(expected) = expected {
                let source = sources
                    .iter()
                    .position(|s| s == &r.source)
                    .and_then(|i| before[i].as_ref().map(|b| b.snapshot.clone()));
                // Cross-device moves are excluded: reversing a partly removed
                // source needs a separate recovery transaction.
                if op == Operation::Cut
                    && source
                        .as_ref()
                        .is_none_or(|s| s.0[0].1.dev != expected.0[0].1.dev)
                {
                    out.history_omitted = true;
                    continue;
                }
                let source = if op == Operation::Copy {
                    source.filter(|s| s.check(&r.source).is_ok())
                } else {
                    source
                };
                entries.push(Entry {
                    id: 0,
                    kind: if op == Operation::Copy {
                        Kind::Copy
                    } else {
                        Kind::Move
                    },
                    from: r.source.clone(),
                    to: r.target.clone(),
                    expected,
                    source,
                    applied: true,
                    skipped: None,
                });
            } else {
                incomplete = true;
            }
        }
        if incomplete {
            entries.clear();
            out.history_omitted = true;
        }
        if entries.is_empty() && out.changed {
            out.history_omitted = true;
            // Nothing of a paste that changed the disk is recorded.
            out.history_cleared = journal.forget(c);
        } else if let Some(Err(dropped)) = journal.push_group(entries, c) {
            out.history_omitted = true;
            out.history_cleared = dropped;
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct Row {
    pub id: u64,
    pub cells: Vec<String>,
}
pub struct View {
    pub rows: Vec<Row>,
    pub can_undo: bool,
    pub can_redo: bool,
}
pub fn view(filter: i32) -> View {
    view_in(filter, true)
}
pub fn export_view() -> View {
    view_in(0, false)
}
fn view_in(filter: i32, localized: bool) -> View {
    let c = config();
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    j.sync(c);
    View {
        can_undo: c.enabled && (j.cursor > 0 || j.retryable_undo_end().is_some()),
        can_redo: c.enabled && j.cursor < j.entries.len(),
        rows: j
            .entries
            .chunk_by(|a, b| a.id == b.id)
            .rev()
            .filter(|g| filter == 0 || filter == g[0].kind as i32 + 1)
            .map(|g| {
                let e = &g[0];
                let path = |p: &Path| {
                    if g.len() == 1 {
                        p.display().to_string()
                    } else if localized {
                        fill(tr("{0} ({1} items)"), &[&p.display(), &g.len()])
                    } else {
                        format!("{} ({} items)", p.display(), g.len())
                    }
                };
                let reasons = g
                    .iter()
                    .filter_map(|e| e.skipped.as_deref())
                    .collect::<Vec<_>>();
                let state = if !reasons.is_empty() {
                    if localized {
                        reasons.join("; ")
                    } else {
                        "Skipped".into()
                    }
                } else if g.iter().all(|e| e.applied) {
                    (if localized { tr("Applied") } else { "Applied" }).into()
                } else if g.iter().all(|e| !e.applied) {
                    (if localized { tr("Undone") } else { "Undone" }).into()
                } else {
                    (if localized {
                        tr("Partly reversed")
                    } else {
                        "Partly reversed"
                    })
                    .into()
                };
                Row {
                    id: e.id,
                    cells: vec![
                        (if localized {
                            e.kind.label()
                        } else {
                            e.kind.word()
                        })
                        .into(),
                        path(&e.from),
                        path(&e.to),
                        state,
                    ],
                }
            })
            .collect(),
    }
}
#[derive(Clone, Copy)]
pub enum Request {
    Undo,
    Redo,
    Through(u64),
}
pub fn run(request: Request, progress: &Progress) -> (String, bool) {
    let Some(_files) = try_file_guard() else {
        return (tr("A file operation is still running.").into(), true);
    };
    let c = config();
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    j.sync(c);
    let mut outcomes = Vec::new();
    let count = match request {
        Request::Through(id) => j
            .entries
            .iter()
            .position(|e| e.id == id)
            .filter(|&i| i < j.cursor)
            .map_or(0, |i| {
                j.entries[i..j.cursor].chunk_by(|a, b| a.id == b.id).count()
            }),
        _ => 1,
    };
    let trash =
        |actual: &Path, original: &Path, stamp: Stamp| actions::trash(actual, original, stamp);
    for _ in 0..count {
        if progress.is_cancelled() {
            break;
        }
        match j.step(matches!(request, Request::Redo), &trash, progress) {
            Some(outcome) => outcomes.push(outcome),
            None => break,
        }
    }
    let text = reverse_summary(matches!(request, Request::Redo), &outcomes);
    (text, outcomes.iter().any(|o| !o.skipped.is_empty()))
}

fn skipped_reasons(skipped: &[&SkippedItem]) -> String {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for item in skipped {
        if let Some((_, names)) = groups.iter_mut().find(|(reason, _)| *reason == item.reason) {
            if !names.contains(&item.name.as_str()) {
                names.push(&item.name);
            }
        } else {
            groups.push((&item.reason, vec![&item.name]));
        }
    }
    groups
        .into_iter()
        .map(|(reason, names)| {
            let mut shown = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
            if names.len() > 3 {
                shown.push_str(&fill(tr(" and {} more"), &[&(names.len() - 3)]));
            }
            format!("{shown}: {reason}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn reverse_summary(redo: bool, outcomes: &[StepOutcome]) -> String {
    if outcomes.is_empty() {
        return tr("No file operations to undo or redo.").into();
    }
    let mut parts = Vec::new();
    let done = outcomes.iter().filter(|o| o.skipped.is_empty()).count();
    let skipped = outcomes.iter().filter(|o| o.done == 0).count();
    if done > 0 {
        let text = if redo {
            trn("Redid {} step.", "Redid {} steps.", done)
        } else {
            trn("Undid {} step.", "Undid {} steps.", done)
        };
        parts.push(fill(text, &[&done]));
    }
    if skipped > 0 {
        parts.push(fill(
            trn("Skipped {} step.", "Skipped {} steps.", skipped),
            &[&skipped],
        ));
    }
    for outcome in outcomes
        .iter()
        .filter(|o| o.done > 0 && !o.skipped.is_empty())
    {
        parts.push(
            if redo {
                tr("Partly redid a step.")
            } else {
                tr("Partly undid a step.")
            }
            .into(),
        );
        let text = if redo {
            trn("Redid {} item.", "Redid {} items.", outcome.done)
        } else {
            trn("Undid {} item.", "Undid {} items.", outcome.done)
        };
        parts.push(fill(text, &[&outcome.done]));
        parts.push(fill(
            trn(
                "Skipped {} item.",
                "Skipped {} items.",
                outcome.skipped.len(),
            ),
            &[&outcome.skipped.len()],
        ));
    }
    if redo && outcomes.iter().all(|o| o.done == 0) {
        parts.push(tr("No files were changed.").into());
    }
    let reasons: Vec<_> = outcomes.iter().flat_map(|o| o.skipped.iter()).collect();
    if !reasons.is_empty() {
        parts.push(skipped_reasons(&reasons));
    }
    parts.join(" ")
}

pub fn csv(rows: &[Row]) -> String {
    let mut out = String::from("Action,Source,Destination,State\r\n");
    for row in rows {
        for (i, cell) in row.cells.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(&cell.replace('"', "\"\""));
            out.push('"');
        }
        out.push_str("\r\n");
    }
    out
}

/// Runs `f` with undo on and an empty session journal; the caller holds
/// [`tests_lock`].
#[cfg(test)]
pub fn with_undo_on(f: impl FnOnce()) {
    let before = config();
    *CONFIG.lock().unwrap() = Config {
        enabled: true,
        limit: 100,
        epoch: before.epoch.wrapping_add(1),
    };
    JOURNAL.lock().unwrap().sync(config());
    f();
    *CONFIG.lock().unwrap() = Config {
        epoch: before.epoch.wrapping_add(2),
        ..before
    };
    JOURNAL.lock().unwrap().sync(config());
}

/// Undoes (or redoes) the last step with the test trash: (done, skipped).
#[cfg(test)]
pub fn step_for_tests(redo: bool) -> (usize, usize) {
    let trash = crate::trashbin::test_send;
    let out = JOURNAL
        .lock()
        .unwrap()
        .step(redo, &trash, &Progress::new())
        .unwrap();
    (out.done, out.skipped.len())
}

/// Serializes tests that use the session journal or the file lock.
#[cfg(test)]
pub fn tests_lock() -> std::sync::MutexGuard<'static, ()> {
    tests::TEST_JOURNAL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) static TEST_JOURNAL: Mutex<()> = Mutex::new(());
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "everygrep-undo-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn entry(kind: Kind, from: PathBuf, to: PathBuf, source: Option<Snapshot>) -> Entry {
        Entry {
            id: 1,
            kind,
            expected: Snapshot::read(&to).unwrap(),
            from,
            to,
            source,
            applied: true,
            skipped: None,
        }
    }
    fn journal(mut entries: Vec<Entry>) -> Journal {
        for (i, e) in entries.iter_mut().enumerate() {
            e.id = i as u64 + 1;
        }
        Journal {
            next_id: entries.len() as u64,
            cursor: entries.len(),
            entries,
            ..Journal::default()
        }
    }
    fn no_trash(_: &Path, _: &Path, _: Stamp) -> Result<(), String> {
        panic!("unexpected trash");
    }

    #[test]
    fn summaries_distinguish_redo_pluralize_and_only_report_real_skips() {
        for redo in [false, true] {
            for n in [1, 2] {
                let outcomes: Vec<_> = (0..n)
                    .map(|_| StepOutcome {
                        done: 1,
                        skipped: vec![],
                    })
                    .collect();
                let verb = if redo { "Redid" } else { "Undid" };
                let plural = if n == 1 { "step" } else { "steps" };
                assert_eq!(
                    reverse_summary(redo, &outcomes),
                    format!("{verb} {n} {plural}.")
                );
            }
        }
        let outcomes: Vec<_> = (0..2)
            .map(|_| StepOutcome {
                done: 0,
                skipped: vec![SkippedItem {
                    name: "a.txt".into(),
                    reason: "Changed externally.".into(),
                }],
            })
            .collect();
        assert_eq!(
            reverse_summary(false, &outcomes),
            "Skipped 2 steps. a.txt: Changed externally."
        );
    }

    #[test]
    fn redo_over_a_skipped_step_reports_that_no_files_changed() {
        let t = Temp::new();
        let a = t.0.join("old");
        let b = t.0.join("new");
        fs::write(&b, "recorded").unwrap();
        let mut j = journal(vec![entry(Kind::Rename, a.clone(), b.clone(), None)]);
        fs::write(&a, "occupied").unwrap();
        let p = Progress::new();
        assert_eq!(j.step(false, &no_trash, &p).unwrap().done, 0);
        let outcome = j.step(true, &no_trash, &p).unwrap();
        let text = reverse_summary(true, &[outcome]);
        assert!(text.contains("Skipped 1 step."), "{text}");
        assert!(text.contains("No files were changed."), "{text}");
        assert!(text.contains("The destination already exists."), "{text}");
        assert_eq!(fs::read(&a).unwrap(), b"occupied");
        assert_eq!(fs::read(&b).unwrap(), b"recorded");
    }

    #[test]
    fn partial_step_reports_successful_and_skipped_items() {
        let t = Temp::new();
        let mut entries = Vec::new();
        for i in 0..3 {
            let to = t.0.join(format!("copy-{i}"));
            fs::write(&to, "original").unwrap();
            entries.push(entry(Kind::Copy, t.0.join(format!("source-{i}")), to, None));
        }
        let changed = entries[1].to.clone();
        let mut j = journal(entries);
        for e in &mut j.entries {
            e.id = 1;
        }
        fs::write(&changed, "changed contents").unwrap();
        let trash = |actual: &Path, original: &Path, stamp: Stamp| {
            transfer::rename_expected(actual, &original.with_extension("trashed"), stamp)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        let outcome = j.step(false, &trash, &Progress::new()).unwrap();
        assert_eq!(
            outcome.done,
            2,
            "{:?}",
            outcome
                .skipped
                .iter()
                .map(|s| &s.reason)
                .collect::<Vec<_>>()
        );
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].name, "copy-1");
        assert!(t.0.join("copy-0.trashed").exists());
        assert!(t.0.join("copy-2.trashed").exists());
        assert_eq!(fs::read(changed).unwrap(), b"changed contents");
        let text = reverse_summary(false, &[outcome]);
        assert!(
            text.starts_with("Partly undid a step. Undid 2 items. Skipped 1 item."),
            "{text}"
        );
    }

    #[test]
    fn skipped_names_are_grouped_by_reason_and_bounded() {
        let items: Vec<_> = (0..6)
            .map(|i| SkippedItem {
                name: format!("file-{i}"),
                reason: "Changed.".into(),
            })
            .collect();
        let refs: Vec<_> = items.iter().chain(items[..1].iter()).collect();
        assert_eq!(
            skipped_reasons(&refs),
            "file-0, file-1, file-2 and 3 more: Changed."
        );
    }

    #[test]
    fn actions_follow_history_position_and_enable_setting() {
        let t = Temp::new();
        let from = t.0.join("from");
        let to = t.0.join("to");
        fs::write(&to, "data").unwrap();
        let mut j = journal(vec![entry(Kind::Rename, from, to, None)]);
        assert_eq!(Journal::default().availability(true), (false, false));
        assert_eq!(j.availability(true), (true, false));
        j.cursor = 0;
        assert_eq!(j.availability(true), (false, true));
        assert_eq!(j.availability(false), (false, false));
    }

    #[test]
    fn file_guard_disables_actions_and_releases_on_early_return() {
        let _test = TEST_JOURNAL.lock().unwrap();
        fn attempt() {
            let _guard = file_guard();
            assert_eq!(availability(), (false, false));
            assert!(try_file_guard().is_none());
        }
        attempt();
        let _guard = file_guard();
        assert_eq!(availability(), (false, false));
    }

    #[test]
    fn exported_history_uses_fixed_english_actions_and_states() {
        let _test = TEST_JOURNAL.lock().unwrap();
        let before = config();
        let c = Config {
            enabled: true,
            limit: 100,
            epoch: before.epoch.wrapping_add(1),
        };
        *CONFIG.lock().unwrap() = c;
        let t = Temp::new();
        let target = t.0.join("target");
        fs::write(&target, "x").unwrap();
        {
            let mut j = JOURNAL.lock().unwrap();
            j.sync(c);
            j.push_group(vec![entry(Kind::Copy, t.0.join("source"), target, None)], c)
                .unwrap()
                .unwrap();
        }
        let rows = export_view().rows;
        assert_eq!(rows[0].cells[0], "Copy");
        assert_eq!(rows[0].cells[3], "Applied");
        JOURNAL.lock().unwrap().entries[0].skipped = Some("yerli səbəb".into());
        let text = csv(&export_view().rows);
        assert!(text.contains("Skipped"));
        assert!(!text.contains("yerli səbəb"));
        *CONFIG.lock().unwrap() = before;
        JOURNAL.lock().unwrap().sync(before);
    }

    #[test]
    fn poisoned_file_and_journal_locks_recover_after_a_worker_fault() {
        let _test = TEST_JOURNAL.lock().unwrap();
        assert!(
            catch_worker(|| {
                let _files = file_guard();
                let _journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
                panic!("injected file worker fault");
            })
            .is_err()
        );
        let _files = file_guard();
        let j = JOURNAL.lock().unwrap();
        assert!(j.entries.is_empty());
        assert_eq!(j.cursor, 0);
        drop(j);
        drop(_files);
        let t = Temp::new();
        let old = t.0.join("old");
        fs::write(&old, "kept").unwrap();
        rename(&old, "new").unwrap();
        assert_eq!(fs::read(t.0.join("new")).unwrap(), b"kept");
    }

    /// Puts a new file at `path` the way an editor saves: the old one goes
    /// to `old`.
    fn replace_by_save(path: &Path, old: &Path, bytes: &str) {
        fs::rename(path, old).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn rename_keeps_an_item_replaced_after_the_dialog_opened() {
        let _test = TEST_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        let path = t.0.join("chosen.txt");
        fs::write(&path, "chosen").unwrap();
        // The dialog opens.
        let chosen = actions::Chosen::of(std::slice::from_ref(&path));
        replace_by_save(&path, &t.0.join("chosen.old"), "never chosen");
        let error = rename_chosen(&path, "renamed.txt", &chosen).unwrap_err();
        assert!(error.to_string().contains("nothing was done"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"never chosen");
        assert!(!t.0.join("renamed.txt").exists());
    }

    #[test]
    fn rename_keeps_a_folder_replaced_while_its_undo_snapshot_is_read() {
        let _test = TEST_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        let folder = t.0.join("chosen");
        fs::create_dir(&folder).unwrap();
        for n in ["a", "b"] {
            fs::write(folder.join(n), n).unwrap();
        }
        let chosen = actions::Chosen::of(std::slice::from_ref(&folder));
        // A sync puts another folder in place while the snapshot is read.
        let (swap, old) = (folder.clone(), t.0.join("chosen-old"));
        crate::fileops_test::arm_path("snapshot-entry", &folder.join("a"), move |_| {
            fs::rename(&swap, &old).unwrap();
            fs::create_dir(&swap).unwrap();
            fs::write(swap.join("new"), "never chosen").unwrap();
        });
        let error = rename_chosen(&folder, "renamed", &chosen).unwrap_err();
        crate::fileops_test::fired();
        assert!(error.to_string().contains("nothing was done"), "{error}");
        assert!(folder.join("new").is_file());
        assert!(!t.0.join("renamed").exists());
        assert!(t.0.join("chosen-old/a").is_file());
    }

    #[test]
    fn rename_refuses_a_folder_or_link_put_in_place_and_a_removed_item() {
        let _test = TEST_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        let p = |n: &str| t.0.join(n);
        for n in ["to-folder", "to-link", "gone", "same"] {
            fs::write(p(n), "x").unwrap();
        }
        let chosen = |n: &str| actions::Chosen::of(&[p(n)]);
        let (to_folder, to_link, gone, same) = (
            chosen("to-folder"),
            chosen("to-link"),
            chosen("gone"),
            chosen("same"),
        );
        fs::remove_file(p("to-folder")).unwrap();
        fs::create_dir(p("to-folder")).unwrap();
        fs::remove_file(p("to-link")).unwrap();
        std::os::unix::fs::symlink(p("same"), p("to-link")).unwrap();
        fs::remove_file(p("gone")).unwrap();
        // Written in place: still the item chosen, renamed as usual.
        fs::write(p("same"), "longer content").unwrap();

        assert!(rename_chosen(&p("to-folder"), "a", &to_folder).is_err());
        assert!(rename_chosen(&p("to-link"), "b", &to_link).is_err());
        assert!(rename_chosen(&p("gone"), "c", &gone).is_err());
        assert!(p("to-folder").is_dir());
        assert!(fs::symlink_metadata(p("to-link")).unwrap().is_symlink());
        for n in ["a", "b", "c"] {
            assert!(fs::symlink_metadata(p(n)).is_err(), "{n}");
        }
        assert!(rename_chosen(&p("same"), "d", &same).is_ok());
        assert_eq!(fs::read(p("d")).unwrap(), b"longer content");
    }

    #[test]
    fn paste_of_a_cut_keeps_an_item_replaced_after_the_cut() {
        let _test = TEST_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        let src = t.0.join("cut.txt");
        let dest = t.0.join("dest");
        fs::write(&src, "cut").unwrap();
        fs::create_dir(&dest).unwrap();
        let held = vec![src.clone()];
        let chosen = actions::Chosen::of(&held);
        replace_by_save(&src, &t.0.join("cut.old"), "never cut");
        let out = paste(
            Operation::Cut,
            &held,
            Some(&chosen),
            &dest,
            &Progress::new(),
            None,
        );
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert_eq!(fs::read(&src).unwrap(), b"never cut");
        assert!(!dest.join("cut.txt").exists());
    }

    #[test]
    fn paste_takes_only_the_unchanged_chosen_items_of_a_mixed_selection() {
        let _test = TEST_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        for op in [Operation::Cut, Operation::Copy] {
            let t = Temp::new();
            let p = |n: &str| t.0.join(n);
            let dest = p("dest");
            fs::create_dir(&dest).unwrap();
            for n in ["same", "to-file", "to-folder", "to-link", "gone", "edited"] {
                fs::write(p(n), n).unwrap();
            }
            fs::create_dir(p("folder")).unwrap();
            fs::write(p("folder/inner"), "inner").unwrap();
            let names = [
                "same",
                "to-file",
                "to-folder",
                "to-link",
                "gone",
                "edited",
                "folder",
            ];
            let sources: Vec<_> = names.into_iter().map(p).collect();
            let chosen = actions::Chosen::of(&sources);
            replace_by_save(&p("to-file"), &p("to-file.old"), "never chosen");
            fs::remove_file(p("to-folder")).unwrap();
            fs::create_dir(p("to-folder")).unwrap();
            fs::remove_file(p("to-link")).unwrap();
            std::os::unix::fs::symlink(p("same"), p("to-link")).unwrap();
            fs::remove_file(p("gone")).unwrap();
            // Written in place: still the chosen item, taken as usual.
            fs::write(p("edited"), "edited later").unwrap();

            let out = paste(op, &sources, Some(&chosen), &dest, &Progress::new(), None);
            let failed: Vec<_> = out.failed.iter().map(|(s, _)| s.clone()).collect();
            assert_eq!(
                failed,
                [p("to-file"), p("to-folder"), p("to-link"), p("gone")],
                "{op:?}"
            );
            for (s, e) in &out.failed {
                if s != &p("gone") {
                    assert!(e.to_string().contains("nothing was done"), "{e}");
                }
            }
            for n in ["to-file", "to-folder", "to-link", "gone"] {
                assert!(fs::symlink_metadata(dest.join(n)).is_err(), "{op:?} {n}");
            }
            assert_eq!(fs::read(p("to-file")).unwrap(), b"never chosen");
            assert!(p("to-folder").is_dir());
            assert_eq!(fs::read(dest.join("same")).unwrap(), b"same");
            assert_eq!(fs::read(dest.join("edited")).unwrap(), b"edited later");
            assert_eq!(fs::read(dest.join("folder/inner")).unwrap(), b"inner");
            assert_eq!(p("same").exists(), op == Operation::Copy);
            assert_eq!(p("folder").exists(), op == Operation::Copy);
        }
    }

    #[test]
    fn paste_refuses_an_item_whose_folder_was_swapped_after_the_cut() {
        let _test = TEST_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        let folder = t.0.join("folder");
        let other = t.0.join("other");
        let dest = t.0.join("dest");
        for d in [&folder, &other, &dest] {
            fs::create_dir(d).unwrap();
        }
        fs::write(folder.join("item"), "chosen").unwrap();
        fs::write(other.join("item"), "elsewhere").unwrap();
        let sources = vec![folder.join("item")];
        let chosen = actions::Chosen::of(&sources);
        fs::rename(&folder, t.0.join("folder.old")).unwrap();
        std::os::unix::fs::symlink(&other, &folder).unwrap();
        let out = paste(
            Operation::Cut,
            &sources,
            Some(&chosen),
            &dest,
            &Progress::new(),
            None,
        );
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert_eq!(fs::read(other.join("item")).unwrap(), b"elsewhere");
        assert!(!dest.join("item").exists());
    }

    #[test]
    fn a_replace_refuses_a_source_rewritten_after_the_cut() {
        let _test = TEST_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        let src = t.0.join("doc");
        let dest = t.0.join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&src, "chosen").unwrap();
        fs::write(dest.join("doc"), "to be replaced").unwrap();
        let sources = vec![src.clone()];
        let chosen = actions::Chosen::of(&sources);
        fs::write(&src, "rewritten").unwrap();
        let out = paste_using(
            Operation::Cut,
            &sources,
            Some(&chosen),
            &dest,
            &Progress::new(),
            Some(Box::new(|_, _| transfer::Decision::Overwrite)),
            transfer::Replacement::with_trash(|_, _, _| panic!("nothing is replaced")),
        );
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert_eq!(fs::read(&src).unwrap(), b"rewritten");
        assert_eq!(fs::read(dest.join("doc")).unwrap(), b"to be replaced");
    }

    #[test]
    fn rename_refuses_a_running_paste_without_waiting() {
        let _test = TEST_JOURNAL.lock().unwrap();
        let t = Temp::new();
        let path = t.0.join("original");
        fs::write(&path, "keep").unwrap();
        let _paste = file_guard();
        let error = rename(&path, "new").unwrap_err();
        assert!(error.to_string().contains("still running"));
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        assert!(!t.0.join("new").exists());
    }

    #[test]
    fn rename_chain_undo_redo_never_replaces_an_occupied_name() {
        let t = Temp::new();
        let a = t.0.join("a");
        let b = t.0.join("b");
        let c = t.0.join("c");
        fs::write(&a, "original").unwrap();
        transfer::rename_strict(&a, &b).unwrap();
        let first = entry(Kind::Rename, a.clone(), b.clone(), None);
        transfer::rename_strict(&b, &c).unwrap();
        let second = entry(Kind::Rename, b.clone(), c.clone(), None);
        let mut j = journal(vec![first, second]);
        j.advance_rename(j.entries[0].expected.0[0].1, j.entries[1].expected.0[0].1);
        let p = Progress::new();
        assert!(j.step(false, &no_trash, &p).unwrap().skipped.is_empty());
        assert!(j.step(false, &no_trash, &p).unwrap().skipped.is_empty());
        assert_eq!(fs::read(&a).unwrap(), b"original");
        assert!(j.step(true, &no_trash, &p).unwrap().skipped.is_empty());
        assert!(j.step(true, &no_trash, &p).unwrap().skipped.is_empty());
        fs::write(&b, "unrelated").unwrap();
        assert!(j.step(false, &no_trash, &p).unwrap().done == 0);
        assert_eq!(fs::read(&b).unwrap(), b"unrelated");
        assert_eq!(fs::read(&c).unwrap(), b"original");
    }
    #[test]

    fn failed_publication_clears_earlier_undo_after_trashing_a_target() {
        let _test = TEST_JOURNAL.lock().unwrap();
        let before = config();
        let c = Config {
            enabled: true,
            limit: 100,
            epoch: before.epoch.wrapping_add(1),
        };
        *CONFIG.lock().unwrap() = c;
        let t = Temp::new();
        let src = t.0.join("source");
        let dst = t.0.join("dest");
        fs::create_dir(&dst).unwrap();
        let prior = t.0.join("prior");
        fs::write(&prior, "prior").unwrap();
        {
            let mut j = JOURNAL.lock().unwrap();
            j.sync(c);
            j.push_group(
                vec![entry(Kind::Rename, t.0.join("earlier"), prior, None)],
                c,
            )
            .unwrap()
            .unwrap();
        }
        fs::write(&src, "source data").unwrap();
        fs::write(dst.join("source"), "old target").unwrap();
        let out = paste_using(
            Operation::Cut,
            std::slice::from_ref(&src),
            None,
            &dst,
            &Progress::new(),
            Some(Box::new(|q, _| {
                if q.error.is_some() {
                    transfer::Decision::Skip
                } else {
                    transfer::Decision::Overwrite
                }
            })),
            transfer::Replacement::with_trash(|actual, target, stamp| {
                let bin = target.parent().unwrap().parent().unwrap().join("trashed");
                transfer::rename_expected(actual, &bin, stamp)?;
                fs::write(target, "new occupant")
            }),
        );
        assert!(out.changed && out.history_cleared && out.history_omitted);
        assert!(JOURNAL.lock().unwrap().entries.is_empty());
        assert_eq!(fs::read(src).unwrap(), b"source data");
        assert_eq!(fs::read(dst.join("source")).unwrap(), b"new occupant");
        assert_eq!(fs::read(t.0.join("trashed")).unwrap(), b"old target");
        *CONFIG.lock().unwrap() = before;
        JOURNAL.lock().unwrap().sync(before);
    }

    #[test]
    fn same_paste_overwrite_cannot_make_undo_move_the_overwriting_file() {
        let _test = TEST_JOURNAL.lock().unwrap();
        // The only test that records into the session journal itself.
        let before = config();
        *CONFIG.lock().unwrap() = Config {
            enabled: true,
            limit: 100,
            epoch: before.epoch.wrapping_add(1),
        };
        for op in [Operation::Cut, Operation::Copy] {
            let t = Temp::new();
            let x = t.0.join("X/readme");
            let y = t.0.join("Y/readme");
            let dest = t.0.join("D");
            fs::create_dir(x.parent().unwrap()).unwrap();
            fs::create_dir(y.parent().unwrap()).unwrap();
            fs::create_dir(&dest).unwrap();
            fs::write(&x, "X data").unwrap();
            fs::write(&y, "Y data").unwrap();
            let out = paste_using(
                op,
                &[x.clone(), y.clone()],
                None,
                &dest,
                &Progress::new(),
                Some(Box::new(|_, _| transfer::Decision::Overwrite)),
                transfer::test_replacement(),
            );
            assert_eq!(out.done.len(), 2);
            assert_eq!(out.replaced, 1);
            let mut j = JOURNAL.lock().unwrap();
            // X's step is recorded with X's identity; Y's overwrite is not.
            assert_eq!(j.entries.len(), 1);
            assert_eq!(j.entries[0].from, x);
            assert!(j.step(false, &no_trash, &Progress::new()).unwrap().done == 0);
            j.entries.clear();
            j.cursor = 0;
            drop(j);
            assert_eq!(fs::read(dest.join("readme")).unwrap(), b"Y data");
            if op == Operation::Cut {
                assert!(!x.exists());
                assert!(!y.exists());
            } else {
                assert_eq!(fs::read(x).unwrap(), b"X data");
            }
        }
        *CONFIG.lock().unwrap() = Config {
            epoch: before.epoch.wrapping_add(2),
            ..before
        };
    }

    #[test]
    fn cancelling_a_retry_preserves_the_applied_entry_and_retry_availability() {
        let t = Temp::new();
        let copy = t.0.join("copy");
        fs::write(&copy, "unchanged").unwrap();
        let mut e = entry(Kind::Copy, t.0.join("source"), copy.clone(), None);
        e.skipped = Some("Temporary failure.".into());
        let mut j = journal(vec![e]);
        j.cursor = 0;
        let p = Progress::new();
        let cancelled = j
            .step(
                false,
                &|_, _, _| {
                    p.cancel();
                    Err("Operation cancelled.".into())
                },
                &p,
            )
            .unwrap();
        assert_eq!(cancelled.done, 0);
        assert_eq!(cancelled.skipped.len(), 1);
        assert!(j.entries[0].applied);
        assert!(j.entries[0].skipped.is_some());
        assert_eq!(j.availability(true), (true, true));
        assert_eq!(fs::read(copy).unwrap(), b"unchanged");
    }

    #[test]
    fn undo_collision_can_be_retried_without_overwriting_or_losing_identity_checks() {
        let t = Temp::new();
        let a = t.0.join("old");
        let b = t.0.join("new");
        fs::write(&b, "recorded").unwrap();
        let mut j = journal(vec![entry(Kind::Rename, a.clone(), b.clone(), None)]);
        fs::write(&a, "occupied").unwrap();
        let p = Progress::new();
        let refused = j.step(false, &no_trash, &p).unwrap();
        assert_eq!(refused.skipped[0].reason, "The destination already exists.");
        assert_eq!(j.availability(true), (true, true));
        assert_eq!(j.step(false, &no_trash, &p).unwrap().done, 0);
        assert_eq!(fs::read(&a).unwrap(), b"occupied");
        assert_eq!(fs::read(&b).unwrap(), b"recorded");
        fs::remove_file(&a).unwrap();
        assert_eq!(j.step(false, &no_trash, &p).unwrap().done, 1);
        assert_eq!(fs::read(&a).unwrap(), b"recorded");
        assert!(!b.exists());
        assert_eq!(j.availability(true), (false, true));
        assert_eq!(j.step(true, &no_trash, &p).unwrap().done, 1);
        fs::write(&b, "external change").unwrap();
        assert_eq!(j.step(false, &no_trash, &p).unwrap().done, 0);
        assert_eq!(j.step(false, &no_trash, &p).unwrap().done, 0);
        assert_eq!(fs::read(&b).unwrap(), b"external change");
        assert!(!a.exists());
        assert_eq!(
            undo_io_error(io::ErrorKind::AlreadyExists.into()),
            "The destination already exists."
        );
    }

    #[test]
    fn changed_or_replaced_items_are_skipped_with_a_reason() {
        let t = Temp::new();
        let a = t.0.join("a");
        let b = t.0.join("b");
        fs::write(&b, "first").unwrap();
        let e = entry(Kind::Move, a.clone(), b.clone(), None);
        let mut j = journal(vec![e]);
        fs::write(&b, "changed contents").unwrap();
        assert!(
            j.step(false, &no_trash, &Progress::new()).unwrap().skipped[0]
                .reason
                .contains("changed")
        );
        assert_eq!(j.cursor, 0);
        assert!(j.entries[0].skipped.is_some());
        assert!(!a.exists());
        let e = entry(Kind::Rename, a, b.clone(), None);
        let mut j = journal(vec![e]);
        fs::rename(&b, t.0.join("hold-inode")).unwrap();
        fs::write(&b, "replacement").unwrap();
        assert!(j.step(false, &no_trash, &Progress::new()).unwrap().done == 0);
        assert_eq!(fs::read(&b).unwrap(), b"replacement");
    }
    #[test]
    fn cancelled_history_walk_stops_before_reading_the_tree() {
        let p = Progress::new();
        p.cancel();
        let t = Temp::new();
        assert_eq!(
            Snapshot::read_with(&t.0, &p, true).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn the_walk_before_a_paste_counts_the_items_it_reads() {
        let t = Temp::new();
        fs::create_dir(t.0.join("sub")).unwrap();
        fs::write(t.0.join("a"), "a").unwrap();
        fs::write(t.0.join("sub/b"), "b").unwrap();
        let p = Progress::new();
        assert_eq!(Snapshot::read_with(&t.0, &p, false).unwrap().0.len(), 4);
        assert_eq!(p.done(), 0);
        Snapshot::read_with(&t.0, &p, true).unwrap();
        assert_eq!(p.done(), 4);
        let shown = crate::labels::busy(&crate::engine::Busy {
            id: 0,
            kind: crate::engine::TaskKind::Copying,
            done: p.done(),
            percent: None,
            cancellable: true,
        });
        assert!(shown.contains('4'), "{shown}");
    }

    #[test]
    fn directory_snapshots_detect_child_changes_and_do_not_follow_links() {
        let t = Temp::new();
        let dir = t.0.join("dir");
        let outside = t.0.join("outside");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("child"), "original").unwrap();
        fs::write(&outside, "one").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let snap = Snapshot::read(&dir).unwrap();
        fs::write(&outside, "two").unwrap();
        assert!(snap.check(&dir).is_ok());
        fs::write(dir.join("child"), "changed contents").unwrap();
        assert!(snap.check(&dir).is_err());
        let snap = Snapshot::read(&dir).unwrap();
        fs::write(dir.join("new"), "new").unwrap();
        assert!(snap.check(&dir).is_err());
    }
    #[test]
    fn copy_undo_uses_trash_and_redo_requires_the_original_source() {
        let t = Temp::new();
        let src = t.0.join("source");
        let copy = t.0.join("copy");
        let bin = t.0.join("bin");
        fs::write(&src, "keep").unwrap();
        let source = Snapshot::read(&src).unwrap();
        transfer::copy_exact(&src, &copy, &Progress::new()).unwrap();
        let mut j = journal(vec![entry(
            Kind::Copy,
            src.clone(),
            copy.clone(),
            Some(source),
        )]);
        let trash = |actual: &Path, _: &Path, stamp: Stamp| {
            transfer::rename_expected(actual, &bin, stamp)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        assert!(
            j.step(false, &trash, &Progress::new())
                .unwrap()
                .skipped
                .is_empty()
        );
        assert!(!copy.exists());
        assert_eq!(fs::read(&bin).unwrap(), b"keep");
        assert!(
            j.step(true, &no_trash, &Progress::new())
                .unwrap()
                .skipped
                .is_empty()
        );
        assert_eq!(fs::read(&copy).unwrap(), b"keep");
        fs::remove_file(&bin).unwrap();
        assert!(
            j.step(false, &trash, &Progress::new())
                .unwrap()
                .skipped
                .is_empty()
        );
        fs::write(&src, "changed").unwrap();
        assert!(j.step(true, &no_trash, &Progress::new()).unwrap().done == 0);
        assert!(!copy.exists());
    }
    #[test]
    fn redo_copy_updates_the_identity_of_a_later_recorded_rename() {
        let t = Temp::new();
        let src = t.0.join("source");
        let copy = t.0.join("copy");
        let renamed = t.0.join("renamed");
        let bin = t.0.join("bin");
        fs::write(&src, "keep").unwrap();
        let source = Snapshot::read(&src).unwrap();
        transfer::copy_exact(&src, &copy, &Progress::new()).unwrap();
        let first = entry(Kind::Copy, src, copy.clone(), Some(source));
        transfer::rename_strict(&copy, &renamed).unwrap();
        let mut second = entry(Kind::Rename, copy.clone(), renamed.clone(), None);
        second.id = 2;
        let mut j = journal(vec![first, second]);
        j.advance_rename(j.entries[0].expected.0[0].1, j.entries[1].expected.0[0].1);
        let p = Progress::new();
        assert!(j.step(false, &no_trash, &p).unwrap().skipped.is_empty());
        assert!(
            j.step(
                false,
                &|actual: &Path, _: &Path, stamp: Stamp| transfer::rename_expected(
                    actual, &bin, stamp
                )
                .map(|_| ())
                .map_err(|e| e.to_string()),
                &p
            )
            .unwrap()
            .skipped
            .is_empty()
        );
        assert!(j.step(true, &no_trash, &p).unwrap().skipped.is_empty());
        assert!(j.step(true, &no_trash, &p).unwrap().skipped.is_empty());
        assert_eq!(fs::read(renamed).unwrap(), b"keep");
    }
    #[test]
    fn history_is_bounded_disabled_history_is_cleared_and_csv_quotes_paths() {
        let t = Temp::new();
        let path = t.0.join("file");
        fs::write(&path, "keep").unwrap();
        let e = entry(Kind::Rename, t.0.join("old"), path, None);
        let mut j = journal(vec![e.clone(), e.clone(), e]);
        j.sync(Config {
            enabled: true,
            limit: 2,
            epoch: 0,
        });
        assert_eq!(j.entries.len(), 2);
        assert_eq!(j.cursor, 2);
        j.sync(Config {
            enabled: false,
            limit: 2,
            epoch: 1,
        });
        assert!(j.entries.is_empty());
        assert_eq!(j.cursor, 0);
        let text = csv(&[Row {
            id: 1,
            cells: vec![
                "Rename".into(),
                "a,\"b\n".into(),
                "dest".into(),
                "Applied".into(),
            ],
        }]);
        assert!(text.contains("\"a,\"\"b\n\""));
        assert!(text.ends_with("\r\n"));
    }
    #[test]
    fn one_paste_is_one_step_and_the_limit_counts_whole_operations() {
        let t = Temp::new();
        let c = Config {
            enabled: true,
            limit: 2,
            epoch: 0,
        };
        let mut j = Journal::default();
        let mut groups = Vec::new();
        for (group, count) in [("old", 1), ("paste", 5)] {
            let mut entries = Vec::new();
            for i in 0..count {
                let from = t.0.join(format!("{group}-source-{i}"));
                let to = t.0.join(format!("{group}-target-{i}"));
                fs::write(&to, "keep").unwrap();
                entries.push(entry(Kind::Move, from.clone(), to.clone(), None));
                groups.push((from, to));
            }
            assert!(j.append_group(entries, c).is_ok());
        }
        assert_eq!(j.entries.chunk_by(|a, b| a.id == b.id).count(), 2);
        assert!(
            j.step(false, &no_trash, &Progress::new())
                .unwrap()
                .skipped
                .is_empty()
        );
        for (from, to) in &groups[1..] {
            assert!(from.exists());
            assert!(!to.exists());
        }
        assert!(groups[0].1.exists());
        assert!(
            j.step(true, &no_trash, &Progress::new())
                .unwrap()
                .skipped
                .is_empty()
        );
        for (from, to) in &groups[1..] {
            assert!(!from.exists());
            assert!(to.exists());
        }
    }

    #[test]
    fn oversized_history_group_is_not_partly_recorded_and_drops_older_steps() {
        let t = Temp::new();
        let to = t.0.join("target");
        fs::write(&to, "keep").unwrap();
        let e = entry(Kind::Move, t.0.join("source"), to, None);
        let c = Config {
            enabled: true,
            limit: 100,
            epoch: 0,
        };
        let mut j = Journal::default();
        assert!(j.append_group(vec![e.clone()], c).is_ok());
        let mut huge = e;
        huge.expected.0 = vec![huge.expected.0[0].clone(); MAX_TREE + 1];
        assert_eq!(j.append_group(vec![huge], c), Err(true));
        // Undo must not skip over the unrecorded operation.
        assert!(j.entries.is_empty());
        assert_eq!(j.cursor, 0);
    }

    #[test]
    fn a_full_history_drops_its_oldest_steps_so_later_operations_are_recorded() {
        let t = Temp::new();
        let c = Config {
            enabled: true,
            limit: 100,
            epoch: 0,
        };
        let sized = |name: &str, n: usize| {
            let to = t.0.join(name);
            fs::write(&to, name).unwrap();
            let mut e = entry(Kind::Move, t.0.join(format!("{name}-from")), to, None);
            e.expected.0 = vec![e.expected.0[0].clone(); n];
            e
        };
        let mut j = Journal::default();
        // A big move, then a paste that fits only without it.
        assert!(j.append_group(vec![sized("big", 80_000)], c).is_ok());
        assert!(j.append_group(vec![sized("next", 15_000)], c).is_ok());
        assert_eq!(j.entries.len(), 2);
        let after = j.append_group(vec![sized("paste", 25_000)], c);
        assert!(after.is_ok());
        assert_eq!(j.entries.len(), 2, "the oldest whole step made room");
        assert!(j.entries.iter().all(|e| !e.to.ends_with("big")));
        // A rename after that is recorded, and undo reverses it, not the move.
        let a = t.0.join("a");
        let b = t.0.join("b");
        fs::write(&a, "renamed").unwrap();
        transfer::rename_strict(&a, &b).unwrap();
        assert!(
            j.append_group(vec![entry(Kind::Rename, a.clone(), b.clone(), None)], c)
                .is_ok()
        );
        assert!(
            j.step(false, &no_trash, &Progress::new())
                .unwrap()
                .skipped
                .is_empty()
        );
        assert_eq!(fs::read(&a).unwrap(), b"renamed");
        assert!(!b.exists());
        assert!(t.0.join("big").exists());
    }

    #[test]
    fn exact_copy_preserves_existing_files_and_dangling_links() {
        let t = Temp::new();
        let src = t.0.join("src");
        let dst = t.0.join("dst");
        fs::write(&src, "source").unwrap();
        fs::write(&dst, "existing").unwrap();
        assert!(transfer::copy_exact(&src, &dst, &Progress::new()).is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"existing");
        fs::remove_file(&dst).unwrap();
        std::os::unix::fs::symlink(t.0.join("missing"), &dst).unwrap();
        assert!(transfer::copy_exact(&src, &dst, &Progress::new()).is_err());
        assert!(fs::symlink_metadata(dst).unwrap().is_symlink());
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    #[test]
    fn restoring_mtime_does_not_hide_an_external_same_size_edit() {
        let path = std::env::temp_dir().join(format!("everygrep-ctime-{}", std::process::id()));
        fs::write(&path, b"old").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let snapshot = Snapshot::read(&path).unwrap();
        fs::write(&path, b"new").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(snapshot.check(&path).is_err());
        fs::remove_file(&path).unwrap();
    }
    #[test]
    fn queued_workers_report_waiting_then_running() {
        let _test = super::tests::TEST_JOURNAL.lock().unwrap();
        let guard = file_guard();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            QUEUED.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |queued| {
                    tx.send(queued).unwrap();
                }))
            });
            let _guard = file_guard();
        });
        assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        drop(guard);
        assert!(!rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn a_question_releases_the_file_slot_until_its_answer() {
        let _test = super::tests::TEST_JOURNAL.lock().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = file_guard();
            while_waiting(|| {
                ready_tx.send(()).unwrap();
                answer_rx.recv().unwrap();
            });
            assert!(try_file_guard().is_none());
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let guard = try_file_guard().expect("the question must not hold the slot");
        drop(guard);
        answer_tx.send(()).unwrap();
        worker.join().unwrap();
    }
}

#[cfg(test)]
mod case_tests {
    use super::*;
    #[test]
    fn case_insensitive_no_replace_rename_is_undoable_and_redoable() {
        let dir = std::env::temp_dir().join(format!("everygrep-case-undo-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let lower = dir.join("a.txt");
        let upper = dir.join("A.txt");
        fs::write(&lower, "keep").unwrap();
        let insensitive = |from: &Path, to: &Path| {
            if from
                .file_name()
                .unwrap()
                .eq_ignore_ascii_case(to.file_name().unwrap())
            {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            transfer::rename_strict(from, to)
        };
        assert_eq!(
            insensitive(&lower, &upper).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        reverse_rename(&lower, &upper, true, insensitive).unwrap();
        reverse_rename(&upper, &lower, true, insensitive).unwrap();
        reverse_rename(&lower, &upper, true, insensitive).unwrap();
        assert_eq!(fs::read(&upper).unwrap(), b"keep");
        fs::write(&lower, "occupied").unwrap();
        assert!(reverse_rename(&upper, &lower, false, transfer::rename_strict).is_err());
        assert!(reverse_rename(&upper, &lower, true, transfer::rename_strict).is_err());
        assert_eq!(fs::read(&lower).unwrap(), b"occupied");
        assert_eq!(fs::read(&upper).unwrap(), b"keep");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod new_item_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "everygrep-undo-new-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("work")).unwrap();
            crate::trashbin::set_test_home(Some(path.join("Trash")));
            Self(path)
        }
        fn work(&self, name: &str) -> PathBuf {
            self.0.join("work").join(name)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            crate::trashbin::set_test_home(None);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Runs `f` with undo on and a fresh session journal.
    fn with_undo(f: impl FnOnce()) {
        let _test = super::tests::TEST_JOURNAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = config();
        *CONFIG.lock().unwrap() = Config {
            enabled: true,
            limit: 100,
            epoch: before.epoch.wrapping_add(1),
        };
        JOURNAL.lock().unwrap().sync(config());
        f();
        *CONFIG.lock().unwrap() = Config {
            epoch: before.epoch.wrapping_add(2),
            ..before
        };
        JOURNAL.lock().unwrap().sync(config());
    }

    fn step(redo: bool) -> StepOutcome {
        let trash = crate::trashbin::test_send;
        JOURNAL
            .lock()
            .unwrap()
            .step(redo, &trash, &Progress::new())
            .unwrap()
    }

    #[test]
    fn new_folder_is_created_once_and_undo_redo_follow_it() {
        with_undo(|| {
            let t = Temp::new();
            let parent = t.0.join("work");
            let (made, note) = new_folder(&parent, "New Folder").unwrap();
            assert!(note.is_none());
            assert!(made.is_dir());
            assert!(matches!(
                new_folder(&parent, "New Folder"),
                Err(actions::RenameError::Exists(_))
            ));
            assert!(step(false).skipped.is_empty());
            assert!(!made.exists());
            assert!(step(true).skipped.is_empty());
            assert!(made.is_dir());
            // Something put in it since: undo leaves it and its contents.
            fs::write(made.join("kept.txt"), "data").unwrap();
            assert_eq!(step(false).done, 0);
            assert_eq!(fs::read(made.join("kept.txt")).unwrap(), b"data");
        });
    }

    #[test]
    fn new_folder_refuses_bad_names_taken_names_and_missing_parents() {
        with_undo(|| {
            let t = Temp::new();
            let parent = t.0.join("work");
            for bad in ["", "a/b", ".", "..", "a\0b", &"x".repeat(256)] {
                assert!(new_folder(&parent, bad).is_err(), "{bad:?}");
            }
            fs::write(parent.join("file"), "keep").unwrap();
            std::os::unix::fs::symlink(t.0.join("missing"), parent.join("dangling")).unwrap();
            for taken in ["file", "dangling"] {
                assert!(matches!(
                    new_folder(&parent, taken),
                    Err(actions::RenameError::Exists(_))
                ));
            }
            assert_eq!(fs::read(parent.join("file")).unwrap(), b"keep");
            assert!(!t.0.join("missing").exists());
            assert!(new_folder(&t.0.join("gone"), "x").is_err());
            assert!(!t.0.join("gone").exists());
            assert!(new_folder(&parent.join("file"), "x").is_err());
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
            assert!(JOURNAL.lock().unwrap().entries.is_empty());
        });
    }

    #[test]
    fn new_folder_redo_never_takes_a_name_used_meanwhile() {
        with_undo(|| {
            let t = Temp::new();
            let (made, _) = new_folder(&t.0.join("work"), "N").unwrap();
            assert!(step(false).skipped.is_empty());
            fs::write(&made, "someone else's file").unwrap();
            assert_eq!(step(true).done, 0);
            assert_eq!(fs::read(&made).unwrap(), b"someone else's file");
        });
    }

    #[test]
    fn new_folder_waits_for_no_running_operation() {
        let _test = super::tests::TEST_JOURNAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        let _running = file_guard();
        assert!(new_folder(&t.0.join("work"), "N").is_err());
        assert!(!t.work("N").exists());
    }

    #[test]
    fn plain_trash_is_the_next_undo_step_and_restores_every_success() {
        with_undo(|| {
            let t = Temp::new();
            let parent = t.0.join("work");
            let unrelated = new_folder(&parent, "previous").unwrap().0;
            let a = t.work("a");
            let b = t.work("b");
            fs::write(&a, "first").unwrap();
            fs::write(&b, "second").unwrap();
            let out = trash(
                &[a.clone(), b.clone()],
                &actions::Chosen::of(&[a.clone(), b.clone()]),
                &crate::trashbin::test_send,
            );
            assert_eq!(out.trashed, [a.clone(), b.clone()], "{:?}", out.failed);
            assert!(out.failed.is_empty() && out.note.is_none());
            let undo = step(false);
            assert_eq!(undo.done, 2);
            assert!(undo.skipped.is_empty());
            assert_eq!(fs::read(&a).unwrap(), b"first");
            assert_eq!(fs::read(&b).unwrap(), b"second");
            assert!(
                unrelated.is_dir(),
                "Ctrl+Z must undo Trash before the earlier operation"
            );
            assert_eq!(step(true).done, 2);
            assert!(!a.exists() && !b.exists());
        });
    }

    #[test]
    fn partial_trash_returns_concrete_results_and_keeps_trying_later_items() {
        with_undo(|| {
            let t = Temp::new();
            let a = t.work("a");
            let b = t.work("b");
            let c = t.work("c");
            for p in [&a, &b, &c] {
                fs::write(p, p.file_name().unwrap().as_encoded_bytes()).unwrap();
            }
            let out = trash(
                &[a.clone(), b.clone(), c.clone()],
                &actions::Chosen::of(&[a.clone(), b.clone(), c.clone()]),
                &|actual, path, stamp| {
                    if path == b {
                        Err("permission denied for b".into())
                    } else {
                        crate::trashbin::test_send(actual, path, stamp)
                    }
                },
            );
            assert_eq!(out.trashed, [a.clone(), c.clone()]);
            assert_eq!(out.failed, [(b.clone(), "permission denied for b".into())]);
            assert!(!a.exists() && !c.exists());
            assert_eq!(fs::read(&b).unwrap(), b"b");
            assert_eq!(step(false).done, 2);
            assert_eq!(fs::read(&a).unwrap(), b"a");
            assert_eq!(fs::read(&c).unwrap(), b"c");
        });
    }

    #[test]
    fn copy_history_cannot_adopt_a_foreign_target_before_journal_capture() {
        with_undo(|| {
            let t = Temp::new();
            let src = t.work("source");
            let dest = t.work("dest");
            fs::write(&src, "selected bytes").unwrap();
            fs::create_dir(&dest).unwrap();
            let foreign = t.work("foreign");
            fs::write(&foreign, "unique foreign bytes").unwrap();
            let saved = t.work("saved-copy");
            crate::fileops_test::arm("before-record", move |target| {
                fs::rename(target, &saved).unwrap();
                fs::rename(&foreign, target).unwrap();
            });
            let out = paste_using(
                Operation::Copy,
                std::slice::from_ref(&src),
                None,
                &dest,
                &Progress::new(),
                None,
                transfer::test_replacement(),
            );
            crate::fileops_test::fired();
            assert_eq!(out.done.len(), 1);
            assert!(out.history_omitted);
            assert!(JOURNAL.lock().unwrap().entries.is_empty());
            assert_eq!(
                fs::read(dest.join("source")).unwrap(),
                b"unique foreign bytes"
            );
            assert_eq!(fs::read(src).unwrap(), b"selected bytes");
        });
    }

    #[test]
    fn copy_history_does_not_adopt_a_child_added_before_journal_capture() {
        with_undo(|| {
            let t = Temp::new();
            let src = t.work("tree");
            let dest = t.work("dest");
            fs::create_dir(&src).unwrap();
            fs::write(src.join("selected"), "selected bytes").unwrap();
            fs::create_dir(&dest).unwrap();
            crate::fileops_test::arm("before-record", |target| {
                fs::write(target.join("foreign"), "unique foreign bytes").unwrap()
            });
            let out = paste_using(
                Operation::Copy,
                std::slice::from_ref(&src),
                None,
                &dest,
                &Progress::new(),
                None,
                transfer::test_replacement(),
            );
            crate::fileops_test::fired();
            assert!(out.history_omitted);
            assert!(JOURNAL.lock().unwrap().entries.is_empty());
            assert_eq!(
                fs::read(dest.join("tree/foreign")).unwrap(),
                b"unique foreign bytes"
            );
        });
    }

    #[test]
    fn single_rename_history_cannot_adopt_a_post_publication_replacement() {
        with_undo(|| {
            let t = Temp::new();
            let src = t.work("source");
            let target = t.work("renamed");
            fs::write(&src, "selected bytes").unwrap();
            let foreign = t.work("foreign");
            fs::write(&foreign, "unique bytes").unwrap();
            let saved = t.work("saved");
            crate::fileops_test::arm("rename-journal", move |path| {
                fs::rename(path, &saved).unwrap();
                fs::rename(&foreign, path).unwrap();
            });
            let note = rename(&src, "renamed").unwrap();
            crate::fileops_test::fired();
            assert!(note.is_some());
            assert!(JOURNAL.lock().unwrap().entries.is_empty());
            assert_eq!(fs::read(target).unwrap(), b"unique bytes");
        });
    }

    #[test]
    fn bulk_rename_history_cannot_adopt_a_post_publication_replacement() {
        with_undo(|| {
            let t = Temp::new();
            let src = t.work("source");
            let target = t.work("renamed");
            fs::write(&src, "selected bytes").unwrap();
            let foreign = t.work("foreign");
            fs::write(&foreign, "unique bytes").unwrap();
            let saved = t.work("saved");
            crate::fileops_test::arm("rename-journal", move |path| {
                fs::rename(path, &saved).unwrap();
                fs::rename(&foreign, path).unwrap();
            });
            let chosen = actions::Chosen::of(std::slice::from_ref(&src));
            let out = rename_batch(&[(src, "renamed".into())], &chosen).unwrap();
            crate::fileops_test::fired();
            assert_eq!(out.done.len(), 1);
            assert!(out.note.is_some());
            assert!(JOURNAL.lock().unwrap().entries.is_empty());
            assert_eq!(fs::read(target).unwrap(), b"unique bytes");
        });
    }

    #[test]
    fn copy_redo_cannot_adopt_a_replacement_before_updating_history() {
        with_undo(|| {
            let t = Temp::new();
            let src = t.work("source");
            let dest = t.work("dest");
            fs::write(&src, "selected bytes").unwrap();
            fs::create_dir(&dest).unwrap();
            let out = paste_using(
                Operation::Copy,
                std::slice::from_ref(&src),
                None,
                &dest,
                &Progress::new(),
                None,
                transfer::test_replacement(),
            );
            assert_eq!(out.done.len(), 1);
            assert_eq!(step(false).done, 1);
            let foreign = t.work("foreign");
            fs::write(&foreign, "unique bytes").unwrap();
            let saved = t.work("saved");
            crate::fileops_test::arm("redo-copy-journal", move |path| {
                fs::rename(path, &saved).unwrap();
                fs::rename(&foreign, path).unwrap();
            });
            let redo = step(true);
            crate::fileops_test::fired();
            assert_eq!(redo.done, 0);
            assert_eq!(redo.skipped.len(), 1);
            assert_eq!(fs::read(dest.join("source")).unwrap(), b"unique bytes");
        });
    }

    #[test]
    fn rename_stamp_propagation_never_changes_the_recorded_inode() {
        let t = Temp::new();
        let a = t.work("a");
        let b = t.work("b");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let first = Stamp::read(&a).unwrap();
        let other = Stamp::read(&b).unwrap();
        let mut j = Journal::default();
        let mut entry = trash_entry(&a);
        entry.expected = Snapshot::read(&a).unwrap();
        j.entries.push(entry);
        j.advance_rename(first, other);
        assert_eq!(j.entries[0].expected.0[0].1, first);
    }

    #[test]
    fn native_trash_undo_redo_preserves_a_tree_links_and_external_hard_links() {
        use std::os::unix::fs::{MetadataExt, symlink};
        with_undo(|| {
            let t = Temp::new();
            let tree = t.work("tree");
            let link = t.work("link");
            let outside = t.work("outside");
            let alias = t.work("alias");
            fs::create_dir(&tree).unwrap();
            fs::write(tree.join("file"), "selected bytes").unwrap();
            fs::write(&outside, "outside unique bytes").unwrap();
            fs::hard_link(tree.join("file"), &alias).unwrap();
            symlink(&outside, tree.join("nested-link")).unwrap();
            symlink(&outside, &link).unwrap();
            let inode = fs::metadata(&alias).unwrap().ino();
            let out = trash(
                &[tree.clone(), link.clone()],
                &actions::Chosen::of(&[tree.clone(), link.clone()]),
                &crate::trashbin::test_send,
            );
            assert_eq!(out.trashed.len(), 2, "{:?}", out.failed);
            assert!(fs::symlink_metadata(&tree).is_err());
            assert!(fs::symlink_metadata(&link).is_err());
            assert_eq!(step(false).done, 2);
            assert_eq!(fs::read(tree.join("file")).unwrap(), b"selected bytes");
            assert_eq!(fs::metadata(tree.join("file")).unwrap().ino(), inode);
            assert_eq!(fs::read_link(tree.join("nested-link")).unwrap(), outside);
            assert_eq!(fs::read_link(&link).unwrap(), outside);
            assert_eq!(step(true).done, 2);
            assert_eq!(
                fs::metadata(t.0.join("Trash/files/tree/file"))
                    .unwrap()
                    .ino(),
                inode
            );
            assert_eq!(fs::read(&alias).unwrap(), b"selected bytes");
            assert_eq!(fs::read(&outside).unwrap(), b"outside unique bytes");
            assert_eq!(step(false).done, 2);
        });
    }

    #[test]
    fn copy_undo_keeps_a_new_file_at_the_visible_name_after_isolation() {
        with_undo(|| {
            let t = Temp::new();
            let src = t.work("source");
            let dest = t.work("dest");
            fs::write(&src, "selected bytes").unwrap();
            fs::create_dir(&dest).unwrap();
            let out = paste_using(
                Operation::Copy,
                std::slice::from_ref(&src),
                None,
                &dest,
                &Progress::new(),
                None,
                transfer::test_replacement(),
            );
            assert_eq!(out.done.len(), 1);
            crate::fileops_test::arm("trash-checked", |path| {
                fs::write(path, "new unique visible file").unwrap()
            });
            let undo = step(false);
            crate::fileops_test::fired();
            assert_eq!(undo.done, 1);
            assert_eq!(
                fs::read(dest.join("source")).unwrap(),
                b"new unique visible file"
            );
            assert_eq!(
                fs::read(t.0.join("Trash/files/source")).unwrap(),
                b"selected bytes"
            );
        });
    }

    #[test]
    fn trash_rechecks_the_inode_after_the_final_native_publication_gap() {
        with_undo(|| {
            let t = Temp::new();
            let file = t.work("file");
            let parked = t.work("parked");
            fs::write(&file, "selected bytes").unwrap();
            let chosen = actions::Chosen::of(std::slice::from_ref(&file));
            let out = trash(
                std::slice::from_ref(&file),
                &chosen,
                &|actual, original, stamp| {
                    let held = actual.to_owned();
                    let saved = parked.clone();
                    crate::fileops_test::arm_path("trash-publish-checked", original, move |_| {
                        fs::rename(&held, saved).unwrap();
                        fs::write(&held, "foreign unique bytes").unwrap();
                    });
                    crate::trashbin::test_send(actual, original, stamp)
                },
            );
            crate::fileops_test::fired();
            assert!(out.trashed.is_empty());
            assert_eq!(out.failed.len(), 1);
            assert_eq!(fs::read(&file).unwrap(), b"foreign unique bytes");
            assert_eq!(fs::read(&parked).unwrap(), b"selected bytes");
            assert_eq!(fs::read_dir(t.0.join("Trash/files")).unwrap().count(), 0);
        });
    }

    #[test]
    fn undo_rename_restores_a_surprise_source_instead_of_moving_it_to_the_old_name() {
        with_undo(|| {
            let t = Temp::new();
            let a = t.work("a");
            let b = t.work("b");
            let saved = t.work("saved");
            fs::write(&a, "selected bytes").unwrap();
            rename(&a, "b").unwrap();
            crate::fileops_test::arm_path("isolate-checked", &b, move |path| {
                fs::rename(path, saved).unwrap();
                fs::write(path, "foreign unique bytes").unwrap();
            });
            let undo = step(false);
            crate::fileops_test::fired();
            assert_eq!(undo.done, 0);
            assert_eq!(undo.skipped.len(), 1);
            assert!(!a.exists());
            assert_eq!(fs::read(b).unwrap(), b"foreign unique bytes");
        });
    }

    #[test]
    fn new_folder_undo_never_removes_a_replacement_or_its_children() {
        with_undo(|| {
            let t = Temp::new();
            let (folder, _) = new_folder(&t.0.join("work"), "fresh").unwrap();
            let saved = t.work("saved");
            crate::fileops_test::arm_path("isolate-checked", &folder, move |path| {
                fs::rename(path, saved).unwrap();
                fs::create_dir(path).unwrap();
                fs::write(path.join("unique"), "foreign bytes").unwrap();
            });
            let undo = step(false);
            crate::fileops_test::fired();
            assert_eq!(undo.done, 0);
            assert_eq!(fs::read(folder.join("unique")).unwrap(), b"foreign bytes");
        });
    }

    #[test]
    fn trash_undo_refuses_an_in_place_edit_even_when_mtime_is_restored() {
        with_undo(|| {
            let t = Temp::new();
            let file = t.work("file");
            fs::write(&file, "AAAA").unwrap();
            let chosen = actions::Chosen::of(std::slice::from_ref(&file));
            let out = trash(
                std::slice::from_ref(&file),
                &chosen,
                &crate::trashbin::test_send,
            );
            assert_eq!(out.trashed.len(), 1);
            let target = t.0.join("Trash/files/file");
            let old = fs::metadata(&target).unwrap().modified().unwrap();
            fs::write(&target, "BBBB").unwrap();
            fs::File::open(&target).unwrap().set_modified(old).unwrap();
            let undo = step(false);
            assert_eq!(undo.done, 0);
            assert!(!file.exists());
            assert_eq!(fs::read(target).unwrap(), b"BBBB");
        });
    }

    fn trash_entry(path: &Path) -> Entry {
        Entry {
            id: 1,
            kind: Kind::Trash,
            from: path.into(),
            to: path.into(),
            expected: Snapshot::read(path).unwrap(),
            source: None,
            applied: true,
            skipped: None,
        }
    }

    #[test]
    fn trash_steps_restore_from_and_return_to_the_trash() {
        let t = Temp::new();
        let file = t.work("dupe.txt");
        fs::write(&file, "copy").unwrap();
        let mut entry = trash_entry(&file);
        crate::trashbin::test_trash(&file).unwrap();
        let stamp = entry.expected.0[0].1;
        let found = crate::trashbin::find(
            &file,
            crate::trashbin::Identity {
                dev: stamp.dev,
                ino: stamp.ino,
                len: stamp.len,
                mtime: stamp.mtime,
            },
        )
        .unwrap();
        entry.expected = Snapshot::read(&found.file).unwrap();
        let mut j = Journal {
            entries: vec![entry],
            cursor: 1,
            next_id: 1,
            ..Journal::default()
        };
        let trash = crate::trashbin::test_send;
        let p = Progress::new();
        assert!(j.step(false, &trash, &p).unwrap().skipped.is_empty());
        assert_eq!(fs::read(&file).unwrap(), b"copy");
        assert!(j.step(true, &trash, &p).unwrap().skipped.is_empty());
        assert!(!file.exists());
        // The name is taken meanwhile: undo refuses and keeps both.
        fs::write(&file, "other").unwrap();
        assert_eq!(j.step(false, &trash, &p).unwrap().done, 0);
        assert_eq!(fs::read(&file).unwrap(), b"other");
        assert_eq!(fs::read_dir(t.0.join("Trash/files")).unwrap().count(), 1);
    }

    #[test]
    fn trash_redo_skips_a_file_changed_after_its_restore() {
        let t = Temp::new();
        let file = t.work("dupe.txt");
        fs::write(&file, "copy").unwrap();
        let mut entry = trash_entry(&file);
        crate::trashbin::test_trash(&file).unwrap();
        let stamp = entry.expected.0[0].1;
        let found = crate::trashbin::find(
            &file,
            crate::trashbin::Identity {
                dev: stamp.dev,
                ino: stamp.ino,
                len: stamp.len,
                mtime: stamp.mtime,
            },
        )
        .unwrap();
        entry.expected = Snapshot::read(&found.file).unwrap();
        let mut j = Journal {
            entries: vec![entry],
            cursor: 1,
            next_id: 1,
            ..Journal::default()
        };
        let trash = crate::trashbin::test_send;
        let p = Progress::new();
        assert!(j.step(false, &trash, &p).unwrap().skipped.is_empty());
        fs::write(&file, "edited after the restore").unwrap();
        assert_eq!(j.step(true, &trash, &p).unwrap().done, 0);
        assert_eq!(fs::read(&file).unwrap(), b"edited after the restore");
    }

    #[test]
    fn trash_keeps_an_item_replaced_after_it_was_chosen() {
        with_undo(|| {
            let t = Temp::new();
            let (a, b) = (t.work("a"), t.work("b"));
            fs::write(&a, "chosen").unwrap();
            fs::write(&b, "b").unwrap();
            let chosen = actions::Chosen::of(&[a.clone(), b.clone()]);
            // An editor saves while the question is open.
            fs::rename(&a, t.work("a.old")).unwrap();
            fs::write(&a, "never chosen").unwrap();
            let out = trash(
                &[a.clone(), b.clone()],
                &chosen,
                &crate::trashbin::test_send,
            );
            assert_eq!(out.trashed, std::slice::from_ref(&b), "{:?}", out.failed);
            assert_eq!(out.failed.len(), 1);
            assert!(
                out.failed[0].1.contains("nothing was done"),
                "{:?}",
                out.failed
            );
            assert_eq!(fs::read(&a).unwrap(), b"never chosen");
        });
    }

    #[test]
    fn a_batch_rename_keeps_an_item_replaced_after_the_preview_opened() {
        with_undo(|| {
            let t = Temp::new();
            let (a, b) = (t.work("a"), t.work("b"));
            fs::write(&a, "chosen").unwrap();
            fs::write(&b, "b").unwrap();
            let chosen = actions::Chosen::of(&[a.clone(), b.clone()]);
            fs::rename(&a, t.work("a.old")).unwrap();
            fs::write(&a, "never chosen").unwrap();
            let items = vec![
                (a.clone(), "new a".to_owned()),
                (b.clone(), "new b".to_owned()),
            ];
            let out = rename_batch(&items, &chosen).unwrap();
            assert_eq!(out.done, [(b.clone(), t.work("new b"))], "{:?}", out.failed);
            assert_eq!(out.failed.len(), 1);
            assert!(
                out.failed[0].1.contains("nothing was done"),
                "{:?}",
                out.failed
            );
            assert_eq!(fs::read(&a).unwrap(), b"never chosen");
            assert!(!t.work("new a").exists());
        });
    }

    #[test]
    fn a_batch_rename_is_one_undo_step_for_every_renamed_item() {
        with_undo(|| {
            let t = Temp::new();
            let names = ["a.txt", "b.txt", "c.txt"];
            for n in names {
                fs::write(t.work(n), n).unwrap();
            }
            let items: Vec<_> = names
                .iter()
                .map(|n| (t.work(n), format!("2024 {n}")))
                .collect();
            let paths: Vec<_> = items.iter().map(|(p, _)| p.clone()).collect();
            let out = rename_batch(&items, &actions::Chosen::of(&paths)).unwrap();
            assert_eq!(out.done.len(), 3);
            assert!(out.failed.is_empty() && out.note.is_none());
            assert_eq!(JOURNAL.lock().unwrap().cursor, 3);
            let undone = step(false);
            assert_eq!((undone.done, undone.skipped.len()), (3, 0));
            for n in names {
                assert_eq!(fs::read(t.work(n)).unwrap(), n.as_bytes());
            }
            assert_eq!(step(true).done, 3);
            for n in names {
                assert_eq!(
                    fs::read(t.work(&format!("2024 {n}"))).unwrap(),
                    n.as_bytes()
                );
            }
        });
    }

    #[test]
    fn a_batch_rename_reports_items_it_could_not_rename_and_undoes_the_rest() {
        with_undo(|| {
            let t = Temp::new();
            for n in ["a", "b", "c"] {
                fs::write(t.work(n), n).unwrap();
            }
            // Taken after the preview: never replaced.
            fs::write(t.work("new b"), "someone else").unwrap();
            let items = vec![
                (t.work("a"), "new a".to_owned()),
                (t.work("b"), "new b".to_owned()),
                (t.work("gone"), "new gone".to_owned()),
                (t.work("c"), "new c".to_owned()),
            ];
            let paths: Vec<_> = items.iter().map(|(p, _)| p.clone()).collect();
            let out = rename_batch(&items, &actions::Chosen::of(&paths)).unwrap();
            assert_eq!(out.done.len(), 2);
            assert_eq!(out.failed.len(), 2);
            assert!(out.failed[0].1.contains("already exists"));
            assert_eq!(fs::read(t.work("new b")).unwrap(), b"someone else");
            assert_eq!(fs::read(t.work("b")).unwrap(), b"b");
            // Changed outside after the rename: undo leaves it, restores c.
            fs::write(t.work("new a"), "edited").unwrap();
            let undone = step(false);
            assert_eq!((undone.done, undone.skipped.len()), (1, 1));
            assert_eq!(fs::read(t.work("c")).unwrap(), b"c");
            assert_eq!(fs::read(t.work("new a")).unwrap(), b"edited");
            assert!(!t.work("a").exists());
        });
    }

    #[test]
    fn a_batch_rename_refuses_to_start_while_another_operation_runs() {
        let _test = super::tests::TEST_JOURNAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let t = Temp::new();
        fs::write(t.work("a"), "a").unwrap();
        let _running = file_guard();
        let chosen = actions::Chosen::of(&[t.work("a")]);
        assert!(rename_batch(&[(t.work("a"), "b".into())], &chosen).is_err());
        assert!(t.work("a").exists() && !t.work("b").exists());
    }

    #[test]
    fn exported_history_names_the_new_kinds_in_english() {
        assert_eq!(Kind::NewFolder.word(), "New folder");
        assert_eq!(Kind::Trash.word(), "Trash");
    }
}
