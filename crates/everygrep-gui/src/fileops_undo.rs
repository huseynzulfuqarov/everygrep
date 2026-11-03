//! Session-only, opt-in reversal. Disk work stays on helper threads.
use crate::i18n::{fill, tr, trn};
use crate::{actions, clipboard::Operation, fileops_settings::Settings, transfer};
use everygrep_core::progress::Progress;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_TREE: usize = 100_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mode: u32,
    mtime: (i64, i64),
}
impl Stamp {
    fn read(path: &Path) -> io::Result<Self> {
        let m = fs::symlink_metadata(path)?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mode: m.mode(),
            mtime: (m.mtime(), m.mtime_nsec()),
        })
    }
}
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Rename,
    Move,
    Copy,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Rename => tr("Rename"),
            Self::Move => tr("Move"),
            Self::Copy => tr("Copy"),
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
pub struct FileGuard(Option<std::sync::MutexGuard<'static, ()>>);
impl FileGuard {
    fn new(guard: std::sync::MutexGuard<'static, ()>) -> Self {
        crate::fileops_undo_ui::request_refresh();
        Self(Some(guard))
    }
}
impl Drop for FileGuard {
    fn drop(&mut self) {
        drop(self.0.take());
        crate::fileops_undo_ui::request_refresh();
    }
}
pub fn file_guard() -> FileGuard {
    FileGuard::new(FILES.lock().expect("file operations"))
}
fn try_file_guard() -> Option<FileGuard> {
    FILES.try_lock().ok().map(FileGuard::new)
}

pub fn configure(s: &Settings) {
    let mut c = CONFIG.lock().expect("configuration");
    if c.enabled != s.undo_enabled {
        c.epoch = c.epoch.wrapping_add(1);
    }
    c.enabled = s.undo_enabled;
    c.limit = s.undo_limit;
    drop(c);
    crate::fileops_undo_ui::request_refresh();
}
fn config() -> Config {
    *CONFIG.lock().expect("configuration")
}
/// A non-blocking snapshot for menu actions; workers may hold the journal
/// while reading trees or waiting for a transfer decision.
pub fn availability() -> (bool, bool) {
    let Ok(_files) = FILES.try_lock() else {
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
    fn availability(&self, enabled: bool) -> (bool, bool) {
        (
            enabled && self.cursor > 0,
            enabled && self.cursor < self.entries.len(),
        )
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
        trash: &impl Fn(&Path) -> Result<(), String>,
        progress: &Progress,
    ) -> Option<StepOutcome> {
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
        trash: &impl Fn(&Path) -> Result<(), String>,
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
        trash: &impl Fn(&Path) -> Result<(), String>,
        progress: &Progress,
    ) -> Result<(), String> {
        if progress.is_cancelled() {
            return Err(tr("Operation cancelled.").into());
        }
        if self.kind == Kind::Copy {
            if redo {
                self.source
                    .as_ref()
                    .ok_or_else(|| {
                        tr("The copy source was not stable enough to record.").to_owned()
                    })?
                    .check(&self.from)?;
                ensure_absent(&self.to)?;
                transfer::copy_exact(&self.from, &self.to, progress).map_err(|e| e.to_string())?;
                self.expected = Snapshot::read(&self.to).map_err(|e| e.to_string())?;
            } else {
                self.expected.check(&self.to)?;
                trash(&self.to)?;
            }
        } else {
            let (from, to) = if redo {
                (&self.from, &self.to)
            } else {
                (&self.to, &self.from)
            };
            self.expected.check(from)?;
            ensure_absent(to)?;
            transfer::rename_strict(from, to).map_err(|e| e.to_string())?;
            self.expected = Snapshot::read(to).map_err(|e| e.to_string())?;
        }
        self.applied = redo;
        Ok(())
    }
}
fn ensure_absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(tr("The destination already exists.").into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Renames, and records the rename when undo is on. The answer is a notice
/// for the user when undo history could not follow the rename.
pub fn rename(old: &Path, name: &str) -> Result<Option<String>, actions::RenameError> {
    let _files = try_file_guard().ok_or_else(|| {
        actions::RenameError::Io(io::Error::other(tr("A file operation is still running.")))
    })?;
    let c = config();
    let result = actions::rename(old, name)?;
    let mut note = None;
    if c.enabled {
        if let Some(to) = &result {
            let mut journal = JOURNAL.lock().expect("journal");
            match Snapshot::read(to) {
                Ok(expected) => {
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
    }
    Ok(note)
}

pub fn paste(
    op: Operation,
    sources: &[PathBuf],
    dest: &Path,
    progress: &Progress,
    resolver: Option<transfer::Resolver>,
) -> transfer::Outcome {
    paste_using(
        op,
        sources,
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
    dest: &Path,
    progress: &Progress,
    resolver: Option<transfer::Resolver>,
    replacement: transfer::Replacement,
) -> transfer::Outcome {
    let _files = file_guard();
    let c = config();
    let before: Vec<_> = if c.enabled {
        let mut budget = MAX_TREE;
        sources
            .iter()
            .map(|p| {
                if budget == 0 {
                    return None;
                }
                let snapshot = Snapshot::read_with(p, progress, true).ok()?;
                if snapshot.0.len() > budget {
                    return None;
                }
                budget -= snapshot.0.len();
                Some(snapshot)
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut published = Vec::new();
    let mut budget = MAX_TREE;
    let mut out = if c.enabled {
        transfer::paste_observed_using(op, sources, dest, progress, resolver, replacement, |r| {
            // Not counted: the transfer counts bytes by then.
            let snapshot = Snapshot::read_with(&r.target, progress, false)
                .ok()
                .filter(|s| s.0.len() <= budget);
            if let Some(s) = &snapshot {
                budget -= s.0.len();
            }
            published.push(snapshot);
        })
    } else if resolver.is_some() {
        transfer::paste_with(op, sources, dest, progress, resolver)
    } else {
        transfer::paste(op, sources, dest, progress)
    };
    if c.enabled {
        let mut journal = JOURNAL.lock().expect("journal");
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
                    .and_then(|i| before[i].clone());
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
                let source = source.filter(|s| s.check(&r.source).is_ok());
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
        if entries.is_empty() && !out.records.is_empty() {
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
    let c = config();
    let mut j = JOURNAL.lock().expect("journal");
    j.sync(c);
    View {
        can_undo: c.enabled && j.cursor > 0,
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
                    } else {
                        fill(tr("{0} ({1} items)"), &[&p.display(), &g.len()])
                    }
                };
                let reasons = g
                    .iter()
                    .filter_map(|e| e.skipped.as_deref())
                    .collect::<Vec<_>>();
                let state = if !reasons.is_empty() {
                    reasons.join("; ")
                } else if g.iter().all(|e| e.applied) {
                    tr("Applied").into()
                } else if g.iter().all(|e| !e.applied) {
                    tr("Undone").into()
                } else {
                    tr("Partly reversed").into()
                };
                Row {
                    id: e.id,
                    cells: vec![e.kind.label().into(), path(&e.from), path(&e.to), state],
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
    let mut j = JOURNAL.lock().expect("journal");
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
    let trash = |path: &Path| actions::trash(&[path.to_owned()]);
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

#[cfg(test)]
mod tests {
    use super::*;
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
    fn no_trash(_: &Path) -> Result<(), String> {
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
        let trash =
            |p: &Path| fs::rename(p, p.with_extension("trashed")).map_err(|e| e.to_string());
        let outcome = j.step(false, &trash, &Progress::new()).unwrap();
        assert_eq!(outcome.done, 2);
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
    fn rename_refuses_a_running_paste_without_waiting() {
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
    fn same_paste_overwrite_cannot_make_undo_move_the_overwriting_file() {
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
        let trash = |path: &Path| fs::rename(path, &bin).map_err(|e| e.to_string());
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
        let p = Progress::new();
        assert!(j.step(false, &no_trash, &p).unwrap().skipped.is_empty());
        assert!(
            j.step(
                false,
                &|path| fs::rename(path, &bin).map_err(|e| e.to_string()),
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
