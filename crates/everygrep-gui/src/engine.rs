//! The engine side of the window: loading the index, searching on a worker
//! thread, applying file changes on a watcher thread, and taking removable
//! drives in and out.
//!
//! The index lives behind a read-write lock. Searches and the result table take
//! short read locks; the watcher takes the write lock only while it applies a
//! batch of changes, never while it waits for them.
//!
//! Long operations (the first index, indexing again, reading a drive,
//! catching up, a search or a sort) are listed as tasks with a
//! [`Progress`]. The window asks for the one to show ([`Engine::busy`]) a few
//! times a second while any runs, and can cancel it ([`Engine::cancel`]);
//! a cancelled operation keeps what was there before and is reported
//! through [`Hooks::on_stopped`].
//!
//! Removable drives (see [`everygrep_core::drives`]) are roots of the same
//! index, so they are searched, sorted and shown like any folder. Each is
//! saved to a file of its own and loaded at the start as offline; when it is
//! mounted it comes online, is caught up and watched; when it goes away its
//! entries stay, marked offline, or are forgotten, as the settings say.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use everygrep_core::drives::{self, Drive, DriveEnv, DriveInfo, MountWatch};
use everygrep_core::index::Index;
use everygrep_core::persist;
use everygrep_core::progress::{self, Progress};
use everygrep_core::query::{HighlightTerm, Query, QueryOptions};
use everygrep_core::rules::IndexRules;
use everygrep_core::scan::{ScanOptions, normalize_roots, scan_with_progress};
use everygrep_core::search::search_with_progress;
use everygrep_core::sort::{DetailCache, Sort, sort_cached, sort_with_progress};
use everygrep_core::store::{NodeId, NodeStore};
use everygrep_core::sync::folders_below;
use everygrep_core::watch::Watcher;

/// What the index is doing, for the status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexState {
    Loading,
    Scanning,
    CatchingUp,
    Watching,
    /// Loaded, but changes are not watched (inotify unavailable).
    Static,
    /// Loaded; watching for changes is turned off in the settings.
    Unwatched,
}

/// The finished result of one search.
pub struct SearchResult {
    pub ids: Arc<Vec<NodeId>>,
    /// Text terms of the query, for highlighting matches in names.
    pub terms: Arc<Vec<HighlightTerm>>,
    pub took: Duration,
    pub error: Option<String>,
    /// The query text that produced the ids.
    pub query: String,
    /// [`Engine::epoch`] when the ids were taken; they are valid only while
    /// it is unchanged.
    pub epoch: u64,
}

/// Callbacks into the UI. They are called from engine threads and must hand
/// the work over to the UI thread themselves.
pub struct Hooks {
    pub on_results: Box<dyn Fn(SearchResult) + Send + Sync>,
    pub on_state: Box<dyn Fn(IndexState, usize) + Send + Sync>,
    /// A task started while none was running: time to ask [`Engine::busy`]
    /// again and again until [`Engine::is_busy`] is false.
    pub on_busy: Box<dyn Fn() + Send + Sync>,
    /// A task was cancelled.
    pub on_stopped: Box<dyn Fn(Stopped) + Send + Sync>,
}

/// A long operation, as the status line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskKind {
    /// The first index of the folders to index.
    Indexing,
    /// Indexing them again; the old index stays until the new one is done.
    Rescanning,
    /// Reading a removable drive for the first time (its name).
    Drive(String),
    /// Catching up with changes made while nothing watched.
    Updating,
    Searching,
    Sorting,
    /// Pasting files that were copied.
    Copying,
    /// Pasting files that were cut.
    Moving,
}

/// The long operation the status line shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    /// For [`Engine::cancel`].
    pub id: u64,
    pub kind: TaskKind,
    /// Entries, folders or names done.
    pub done: u64,
    /// `None` when the amount of work is not known.
    pub percent: Option<u8>,
    pub cancellable: bool,
}

/// A cancelled operation, and what was kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The first index: nothing is saved; the index stays empty until it is
    /// built again.
    Index,
    /// Indexing again: the previous index and its folders stay.
    Rescan,
    /// A drive's first reading (its name): the drive is not indexed until
    /// it is connected again.
    Drive(String),
    /// A search: the previous results stay.
    Search,
    /// A new order: the previous results stay in this, their order.
    Sort(Sort),
}

/// Tasks younger than this are not shown, so quick ones do not flicker.
pub const SHOW_AFTER: Duration = Duration::from_millis(300);
/// How often the window asks for the progress: at most ten times a second.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(100);

struct Task {
    id: u64,
    kind: Mutex<TaskKind>,
    started: Instant,
    progress: Progress,
    cancellable: bool,
}

/// The long operations running now, oldest first.
struct Tasks {
    list: Mutex<Vec<Arc<Task>>>,
    next_id: AtomicU64,
    on_busy: Box<dyn Fn() + Send + Sync>,
}

impl Tasks {
    fn new(on_busy: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            list: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            on_busy,
        })
    }

    /// Lists a task until the returned guard is dropped.
    fn begin(self: &Arc<Self>, kind: TaskKind, cancellable: bool) -> TaskGuard {
        let task = Arc::new(Task {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            kind: Mutex::new(kind),
            started: Instant::now(),
            progress: Progress::new(),
            cancellable,
        });
        let first = {
            let mut list = self.list.lock().expect("lock");
            list.push(Arc::clone(&task));
            list.len() == 1
        };
        if first {
            (self.on_busy)();
        }
        TaskGuard {
            tasks: Arc::clone(self),
            task,
        }
    }

    /// The oldest task that has run for [`SHOW_AFTER`] at `now`.
    fn shown(&self, now: Instant) -> Option<Busy> {
        let list = self.list.lock().expect("lock");
        let task = list
            .iter()
            .find(|t| now.saturating_duration_since(t.started) >= SHOW_AFTER)?;
        Some(Busy {
            id: task.id,
            kind: task.kind.lock().expect("lock").clone(),
            done: task.progress.done(),
            percent: task.progress.percent(),
            cancellable: task.cancellable,
        })
    }

    fn cancel(&self, id: u64) {
        let list = self.list.lock().expect("lock");
        if let Some(task) = list.iter().find(|t| t.id == id && t.cancellable) {
            task.progress.cancel();
        }
    }

    /// Cancels the running search or sort, if any.
    fn cancel_search(&self) {
        let list = self.list.lock().expect("lock");
        for task in list.iter() {
            let kind = task.kind.lock().expect("lock");
            if matches!(*kind, TaskKind::Searching | TaskKind::Sorting) {
                task.progress.cancel();
            }
        }
    }
}

/// A running task; dropping it ends the task.
struct TaskGuard {
    tasks: Arc<Tasks>,
    task: Arc<Task>,
}

impl TaskGuard {
    fn progress(&self) -> &Progress {
        &self.task.progress
    }

    fn set_kind(&self, kind: TaskKind) {
        *self.task.kind.lock().expect("lock") = kind;
    }
}

/// A long operation of the window itself (a paste), shown in the status
/// line like the engine's own until it is dropped. It can be cancelled.
pub struct Job(TaskGuard);

impl Job {
    pub fn progress(&self) -> &Progress {
        self.0.progress()
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let mut list = self.tasks.list.lock().expect("lock");
        list.retain(|t| t.id != self.task.id);
    }
}

/// How removable drives are handled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveSettings {
    /// Index a drive that was never indexed as soon as it is mounted.
    pub auto: bool,
    /// When a drive goes away, keep its entries (shown as offline) rather
    /// than forgetting them.
    pub keep_offline: bool,
    /// UUIDs of drives that are never indexed.
    pub ignored: Vec<String>,
}

impl Default for DriveSettings {
    fn default() -> Self {
        Self {
            auto: true,
            keep_offline: true,
            ignored: Vec::new(),
        }
    }
}

/// A known drive, as the settings window lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveStatus {
    pub uuid: String,
    pub label: String,
    /// Where it is mounted, or was mounted last.
    pub path: PathBuf,
    pub online: bool,
    /// It is being read for the first time.
    pub scanning: bool,
    /// Its entries are in the index.
    pub indexed: bool,
    pub ignored: bool,
    pub entries: u64,
    /// Seconds since 1970 when it was last connected; 0 when unknown.
    pub last_seen: i64,
}

/// A drive the engine knows of.
struct Volume {
    info: DriveInfo,
    /// The name of its root in the index (normally its mount point); `None`
    /// when its entries are not in the index.
    root: Option<PathBuf>,
    online: bool,
    /// The rules its entries were read with, saved with them; meaningful
    /// only while `root` is set.
    rules: IndexRules,
}

/// Changes to the drives asked for by the window, carried out on the index
/// thread.
enum DriveCommand {
    /// Drop the drive's entries, its index file and its notes.
    Forget(String),
    /// Stop indexing the drive (drop its entries, keep its notes) or index
    /// it again.
    Ignore(String, bool),
}

/// A drive read for the first time on a helper thread, waiting to be added;
/// `None` when the reading was cancelled.
struct Scanned {
    drive: Drive,
    index: Option<Index>,
    rules: IndexRules,
}

/// Shared engine state.
pub struct Engine {
    pub index: RwLock<Index>,
    roots: Mutex<Vec<PathBuf>>,
    options: Mutex<ScanOptions>,
    /// Set by [`Engine::reindex`]; the index thread scans again when it sees
    /// it.
    rescan: AtomicBool,
    /// Whether file changes are watched (see [`Engine::set_watching`]).
    watching: AtomicBool,
    index_file: Option<PathBuf>,
    /// Bumped for every new query; older results are dropped.
    generation: AtomicU64,
    query: Mutex<QueryState>,
    /// Ids currently shown, so they can be translated after a compaction.
    shown: Mutex<Arc<Vec<NodeId>>>,
    /// Bumped, under the index write lock, whenever node ids change meaning
    /// (compaction, a new scan). Ids taken in an older epoch must not be used.
    epoch: AtomicU64,
    dirty: AtomicBool,
    stop: AtomicBool,
    searches: Mutex<Option<mpsc::Sender<u64>>>,
    hooks: Hooks,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Removable drives: where to look (`None` turns them off), the
    /// settings, the known drives, requests from the window, drives read on
    /// helper threads, and whether the mount table changed.
    drive_env: Option<DriveEnv>,
    drive_settings: Mutex<DriveSettings>,
    volumes: Mutex<Vec<Volume>>,
    drive_commands: Mutex<Vec<DriveCommand>>,
    scanned: Arc<Mutex<Vec<Scanned>>>,
    scanning: Mutex<HashSet<String>>,
    /// Drives whose first reading was cancelled: not read again until they
    /// are connected again (or asked for in the settings).
    not_now: Mutex<HashSet<String>>,
    mounts_changed: Arc<AtomicBool>,
    tasks: Arc<Tasks>,
    /// The folders to index and the rules the index was built with (or,
    /// after a cancelled first index, was to be built with); a cancelled
    /// new index goes back to them.
    indexed_with: Mutex<(Vec<PathBuf>, IndexRules)>,
    /// The first index was cancelled: the index of the folders is not
    /// complete and must not be saved.
    incomplete: AtomicBool,
    /// The query that gave the results shown; a cancelled search goes back
    /// to it.
    applied: Mutex<QueryState>,
    /// Creation times or attributes read by the last sort, used by the next
    /// one while file changes are applied (see [`DetailCache`]). Only the
    /// search thread uses it.
    details: Mutex<DetailCache>,
    /// Bumped, under the index write lock, whenever the details cache must
    /// forget all values: new ids (with `epoch`), and the start of every
    /// watch session and lost events, since changes were not applied
    /// meanwhile.
    details_generation: AtomicU64,
    /// Whether file changes are being applied to the index now (the watch
    /// loop runs). Only then is the details cache used.
    applying_changes: AtomicBool,
}

/// What the search thread runs: the query text, its options and the result order.
#[derive(Clone, Default)]
struct QueryState {
    text: String,
    options: QueryOptions,
    sort: Sort,
}

const REFRESH_EVERY: Duration = Duration::from_millis(500);
const SAVE_EVERY: Duration = Duration::from_secs(300);

impl Engine {
    /// Creates the engine and starts loading in the background. Entries
    /// that `rules` leave out are not indexed. With `watch`, file changes
    /// are applied as they happen. With `drives`, removable drives found
    /// there are indexed as the settings say.
    pub fn start(
        roots: Vec<PathBuf>,
        rules: IndexRules,
        watch: bool,
        index_file: Option<PathBuf>,
        drives: Option<(DriveEnv, DriveSettings)>,
        mut hooks: Hooks,
    ) -> Arc<Self> {
        let on_busy = std::mem::replace(&mut hooks.on_busy, Box::new(|| {}));
        let (drive_env, drive_settings) = match drives {
            Some((env, settings)) => (Some(env), settings),
            None => (None, DriveSettings::default()),
        };
        let engine = Arc::new(Self {
            index: RwLock::new(Index::new(NodeStore::new())),
            roots: Mutex::new(normalize_roots(&roots)),
            options: Mutex::new(ScanOptions {
                rules,
                ..ScanOptions::default()
            }),
            rescan: AtomicBool::new(false),
            watching: AtomicBool::new(watch),
            index_file: index_file.or_else(persist::default_path),
            generation: AtomicU64::new(0),
            query: Mutex::new(QueryState::default()),
            shown: Mutex::new(Arc::new(Vec::new())),
            epoch: AtomicU64::new(0),
            details: Mutex::new(DetailCache::new()),
            details_generation: AtomicU64::new(0),
            applying_changes: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            searches: Mutex::new(None),
            hooks,
            threads: Mutex::new(Vec::new()),
            drive_env,
            drive_settings: Mutex::new(drive_settings),
            volumes: Mutex::new(Vec::new()),
            drive_commands: Mutex::new(Vec::new()),
            scanned: Arc::new(Mutex::new(Vec::new())),
            scanning: Mutex::new(HashSet::new()),
            not_now: Mutex::new(HashSet::new()),
            mounts_changed: Arc::new(AtomicBool::new(false)),
            tasks: Tasks::new(on_busy),
            indexed_with: Mutex::new((Vec::new(), IndexRules::default())),
            incomplete: AtomicBool::new(false),
            applied: Mutex::new(QueryState::default()),
        });
        // Watched from before the index thread first looks at the drives, so
        // a drive mounted after that look is never missed, however late the
        // mount thread starts.
        let mount_watch = engine
            .drive_env
            .as_ref()
            .and_then(|env| MountWatch::new(&env.mountinfo).ok());
        let (tx, rx) = mpsc::channel();
        *engine.searches.lock().expect("lock") = Some(tx);
        let e = Arc::clone(&engine);
        let searcher = std::thread::Builder::new()
            .name("search".into())
            .spawn(move || e.search_loop(&rx))
            .expect("start search thread");
        let e = Arc::clone(&engine);
        let indexer = std::thread::Builder::new()
            .name("index".into())
            .spawn(move || e.index_loop())
            .expect("start index thread");
        engine
            .threads
            .lock()
            .expect("lock")
            .extend([searcher, indexer]);
        if let Some(watch) = mount_watch {
            #[cfg(test)]
            let path = engine.drive_env.as_ref().map(|env| env.mountinfo.clone());
            let e = Arc::clone(&engine);
            // Sleeps in poll until the mount table changes; ends with the
            // process like the index thread.
            let _ = std::thread::Builder::new()
                .name("mounts".into())
                .spawn(move || {
                    #[cfg(test)]
                    if let Some(path) = path {
                        tests::slow_thread_start(&path);
                    }
                    e.mount_loop(watch);
                });
        }
        engine
    }

    fn mount_loop(&self, mut watch: MountWatch) {
        while !self.stop.load(Ordering::SeqCst) {
            match watch.wait(Duration::from_secs(1)) {
                Ok(true) => self.mounts_changed.store(true, Ordering::SeqCst),
                Ok(false) => {}
                Err(_) => std::thread::sleep(Duration::from_secs(1)),
            }
        }
    }

    /// Changes how drives are handled; drives are taken in or out to match
    /// soon after.
    pub fn set_drive_settings(&self, settings: DriveSettings) {
        *self.drive_settings.lock().expect("lock") = settings;
        self.mounts_changed.store(true, Ordering::SeqCst);
    }

    /// Whether removable drives are handled at all.
    pub fn drives_enabled(&self) -> bool {
        self.drive_env.is_some()
    }

    /// The long operation to show in the status line: the oldest that has
    /// run for [`SHOW_AFTER`].
    pub fn busy(&self) -> Option<Busy> {
        self.tasks.shown(Instant::now())
    }

    /// Whether any long operation runs, shown or not yet.
    pub fn is_busy(&self) -> bool {
        !self.tasks.list.lock().expect("lock").is_empty()
    }

    /// Lists a job of the window's own in the status line until the
    /// returned [`Job`] is dropped.
    pub fn begin_job(&self, kind: TaskKind) -> Job {
        Job(self.tasks.begin(kind, true))
    }

    /// Asks the operation [`Busy::id`] to stop, if it can.
    pub fn cancel(&self, id: u64) {
        self.tasks.cancel(id);
    }

    /// Cancels every task that can be cancelled.
    #[cfg(test)]
    fn cancel_all(&self) {
        for task in self.tasks.list.lock().expect("lock").iter() {
            if task.cancellable {
                task.progress.cancel();
            }
        }
    }

    /// The known drives, connected ones first, then by name.
    pub fn drives(&self) -> Vec<DriveStatus> {
        let ignored = self.drive_settings.lock().expect("lock").ignored.clone();
        let scanning = self.scanning.lock().expect("lock").clone();
        let mut out: Vec<DriveStatus> = self
            .volumes
            .lock()
            .expect("lock")
            .iter()
            .map(|v| DriveStatus {
                uuid: v.info.uuid.clone(),
                label: v.info.label.clone(),
                path: v.info.path.clone(),
                online: v.online,
                scanning: scanning.contains(&v.info.uuid),
                indexed: v.root.is_some(),
                ignored: ignored.contains(&v.info.uuid),
                entries: if v.root.is_some() { v.info.entries } else { 0 },
                last_seen: v.info.last_seen,
            })
            .collect();
        out.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
                .then_with(|| a.uuid.cmp(&b.uuid))
        });
        out
    }

    /// Drops a drive's entries, its index file and its notes. A connected
    /// drive is read again when drives are indexed automatically.
    pub fn forget_drive(&self, uuid: &str) {
        self.queue_drive(DriveCommand::Forget(uuid.to_owned()));
    }

    /// Stops indexing a drive (its entries go, its notes stay so it can be
    /// indexed again), or indexes it again. The settings must list it as
    /// ignored or not already (see [`set_drive_settings`]).
    ///
    /// [`set_drive_settings`]: Engine::set_drive_settings
    pub fn ignore_drive(&self, uuid: &str, ignore: bool) {
        self.queue_drive(DriveCommand::Ignore(uuid.to_owned(), ignore));
    }

    fn queue_drive(&self, command: DriveCommand) {
        self.drive_commands.lock().expect("lock").push(command);
        self.mounts_changed.store(true, Ordering::SeqCst);
    }

    /// The folders the index covers. Empty until the index is loaded or the
    /// first scan has started.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().expect("lock").clone()
    }

    /// Indexes `roots` again from scratch with `rules`, in the background.
    /// Empty `roots` stand for the home folder. The status hook reports the
    /// scan as for a first start; the new index is saved as soon as it is
    /// complete.
    pub fn reindex(&self, roots: &[PathBuf], rules: IndexRules) {
        *self.roots.lock().expect("lock") = normalize_roots(roots);
        self.options.lock().expect("lock").rules = rules;
        self.rescan.store(true, Ordering::SeqCst);
    }

    /// Starts or stops watching for file changes. Stopping removes every
    /// inotify watch; starting again catches up with the changes made
    /// meanwhile, then watches again.
    pub fn set_watching(&self, on: bool) {
        self.watching.store(on, Ordering::SeqCst);
    }

    fn scan_options(&self) -> ScanOptions {
        self.options.lock().expect("lock").clone()
    }

    /// Runs `text` as the current query. Returns the generation of the request.
    pub fn set_query(&self, text: &str, options: QueryOptions) -> u64 {
        let mut q = self.query.lock().expect("lock");
        q.text = text.to_owned();
        q.options = options;
        // Under the lock, so a cancelled search never undoes this request.
        self.request_search()
    }

    /// Changes the result order and runs the current query again. The order
    /// stays for later queries. Returns the generation of the request.
    pub fn set_sort(&self, sort: Sort) -> u64 {
        let mut q = self.query.lock().expect("lock");
        q.sort = sort;
        self.request_search()
    }

    /// Runs the current query again, reading the details a sort needs
    /// (creation times, attributes) afresh. Returns the generation of the
    /// request.
    pub fn refresh(&self) -> u64 {
        let _query = self.query.lock().expect("lock");
        self.forget_details();
        self.request_search()
    }

    /// Records the ids now shown in the table.
    pub fn set_shown(&self, ids: Arc<Vec<NodeId>>) {
        *self.shown.lock().expect("lock") = ids;
    }

    /// The current id epoch. Read it under an index lock to pair it with ids.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Whether `generation` is still the newest request.
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    fn request_search(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // The running search or sort is for an older request: its results
        // would be dropped, so stop it rather than wait for it. A search
        // that begins after this sees the new generation (see
        // `search_loop`).
        self.tasks.cancel_search();
        if let Some(tx) = self.searches.lock().expect("lock").as_ref() {
            let _ = tx.send(generation);
        }
        generation
    }

    fn search_loop(&self, rx: &mpsc::Receiver<u64>) {
        while let Ok(mut generation) = rx.recv() {
            // Only the newest request matters.
            while let Ok(newer) = rx.try_recv() {
                generation = newer;
            }
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let state = self.query.lock().expect("lock").clone();
            let started = Instant::now();
            let mut terms = Vec::new();
            let mut epoch = self.epoch();
            let (ids, error) = match Query::parse(&state.text, state.options) {
                Ok(query) => {
                    terms = query.highlight_terms();
                    let index = self.index.read().expect("index lock");
                    epoch = self.epoch();
                    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
                    let task = self.tasks.begin(TaskKind::Searching, true);
                    // A request that came before the task was listed could
                    // not cancel it; it is stale already.
                    if !self.is_current(generation) {
                        task.progress().cancel();
                    }
                    let found = search_with_progress(&index, &query, threads, task.progress())
                        .and_then(|mut ids| {
                            if self.is_current(generation) {
                                task.set_kind(TaskKind::Sorting);
                                task.progress().restart(None);
                                self.sort(&index, &mut ids, state.sort, threads, task.progress())?;
                                // `limit:` keeps the first results in this order.
                                query.apply_limit(&mut ids);
                            }
                            Ok(ids)
                        });
                    drop(task);
                    drop(index);
                    match found {
                        Ok(ids) => (ids, None),
                        Err(progress::Cancelled) => {
                            self.search_cancelled(generation, &state);
                            continue;
                        }
                    }
                }
                Err(e) => (Vec::new(), Some(e.to_string())),
            };
            if self.is_current(generation) {
                *self.applied.lock().expect("lock") = state.clone();
                (self.hooks.on_results)(SearchResult {
                    ids: Arc::new(ids),
                    terms: Arc::new(terms),
                    took: started.elapsed(),
                    error,
                    query: state.text,
                    epoch,
                });
            }
        }
    }

    /// Sorts results of `index` (read-locked by the caller), with the
    /// details cache while file changes are applied. Without them nothing
    /// says what changed, so details are read again and the cache is
    /// dropped.
    fn sort(
        &self,
        index: &Index,
        ids: &mut [NodeId],
        sort: Sort,
        threads: usize,
        progress: &Progress,
    ) -> Result<(), progress::Cancelled> {
        let mut details = self.details.lock().expect("lock");
        if self.applying_changes.load(Ordering::SeqCst) {
            details.set_generation(self.details_generation.load(Ordering::SeqCst));
            sort_cached(index, ids, sort, threads, progress, &mut details)
        } else {
            details.clear();
            drop(details);
            sort_with_progress(index, ids, sort, threads, progress)
        }
    }

    /// Ids change meaning; call under the index write lock.
    fn bump_epoch(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.forget_details();
    }

    /// Makes the next sort read all details again; call under the index
    /// write lock.
    fn forget_details(&self) {
        self.details_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// A search or sort for `state` was cancelled: the results shown stay,
    /// and so does the query that gave them, unless a newer request came.
    /// A newer request also cancels it (see `request_search`); that is not
    /// a stop the user asked for, so nothing is reported then.
    fn search_cancelled(&self, generation: u64, state: &QueryState) {
        let mut q = self.query.lock().expect("lock");
        if !self.is_current(generation) {
            return;
        }
        let applied = self.applied.lock().expect("lock").clone();
        let stopped = if state.sort == applied.sort {
            Stopped::Search
        } else {
            Stopped::Sort(applied.sort)
        };
        *q = applied;
        drop(q);
        (self.hooks.on_stopped)(stopped);
    }

    /// Swaps in a new index; ids of the old one become invalid.
    fn replace_index(&self, new: Index) {
        let mut index = self.index.write().expect("index lock");
        *index = new;
        self.bump_epoch();
    }

    fn report(&self, state: IndexState) {
        let len = self.index.read().expect("index lock").len();
        (self.hooks.on_state)(state, len);
    }

    fn index_loop(&self) {
        self.report(IndexState::Loading);
        let mut loaded = false;
        if let Some(path) = &self.index_file {
            if let Ok(saved) = persist::load(path) {
                let mut roots = self.roots.lock().expect("lock");
                if roots.is_empty() || *roots == saved.roots {
                    *roots = saved.roots;
                    let rules = self.scan_options().rules;
                    // An index built with other rules, or saved without
                    // them, is built again over its folders.
                    if saved.rules_hash == Some(persist::rules_fingerprint(&rules.spec())) {
                        *self.indexed_with.lock().expect("lock") = (roots.clone(), rules);
                        self.replace_index(saved.index);
                        if saved.reordered {
                            // Store the new order, so it is not sorted again.
                            self.dirty.store(true, Ordering::SeqCst);
                        }
                        loaded = true;
                    }
                }
            }
        }
        if !loaded && !self.scan_fresh(TaskKind::Indexing) {
            // Nothing is saved until a new index is complete; the next
            // start indexes again.
            self.incomplete.store(true, Ordering::SeqCst);
            *self.indexed_with.lock().expect("lock") = (self.roots(), self.scan_options().rules);
            (self.hooks.on_stopped)(Stopped::Index);
        }
        self.load_drives();
        let (_, mut drives_to_catch_up) = self.sync_drives(None);
        self.request_search();

        // Changes during any scan precede watch installation too.
        let mut catch_up = true;
        while !self.stop.load(Ordering::SeqCst) {
            if !self.watching.load(Ordering::SeqCst) {
                self.wait_unwatched();
                // Turned on again: what changed meanwhile is caught up.
                catch_up = true;
            } else {
                catch_up = self.watch_once(catch_up, &mut drives_to_catch_up);
            }
            if self.rescan.swap(false, Ordering::SeqCst) && !self.stop.load(Ordering::SeqCst) {
                if self.scan_fresh(TaskKind::Rescanning) {
                    *self.shown.lock().expect("lock") = Arc::new(Vec::new());
                    self.request_search();
                    self.save();
                    catch_up = true;
                } else {
                    // The previous index stays, with its folders; nothing
                    // watched it during the scan, so it is caught up.
                    let (roots, rules) = self.indexed_with.lock().expect("lock").clone();
                    *self.roots.lock().expect("lock") = roots;
                    self.options.lock().expect("lock").rules = rules;
                    catch_up = true;
                    (self.hooks.on_stopped)(Stopped::Rescan);
                }
            }
        }
    }

    /// Whether the index thread should leave its current loop: to stop, to
    /// scan again, or because watching was turned on or off.
    fn interrupted(&self, watching: bool) -> bool {
        self.stop.load(Ordering::SeqCst)
            || self.rescan.load(Ordering::SeqCst)
            || self.watching.load(Ordering::SeqCst) != watching
    }

    /// Waits, without any watch, until watching is turned on, a rescan is
    /// asked for or the engine stops. Drives still come and go.
    fn wait_unwatched(&self) {
        self.report(IndexState::Unwatched);
        while !self.interrupted(false) {
            std::thread::sleep(Duration::from_millis(250));
            if self.mounts_changed.swap(false, Ordering::SeqCst) && self.sync_drives(None).0 {
                self.dirty.store(true, Ordering::SeqCst);
                self.request_search();
                self.report(IndexState::Unwatched);
            }
        }
    }

    /// Watches until interrupted, catching up first when `catch_up` is set
    /// (or with the roots of drives that came online). The watcher and all
    /// its watches are dropped on return. Returns whether a catch-up is
    /// still due.
    fn watch_once(&self, catch_up: bool, drives_to_catch_up: &mut Vec<NodeId>) -> bool {
        let watcher = Watcher::new(
            &self.index.read().expect("index lock"),
            &self.scan_options(),
        );
        match watcher {
            Ok(mut watcher) => {
                if catch_up || !drives_to_catch_up.is_empty() {
                    self.report(IndexState::CatchingUp);
                    let task = self.tasks.begin(TaskKind::Updating, false);
                    let mut index = self.index.write().expect("index lock");
                    let roots = if catch_up {
                        online_roots(&index)
                    } else {
                        std::mem::take(drives_to_catch_up)
                    };
                    task.progress()
                        .set_total(Some(folders_below(&index, &roots)));
                    let stats = watcher.catch_up_with_progress(&mut index, &roots, task.progress());
                    drop(index);
                    drop(task);
                    if stats.added + stats.removed + stats.updated > 0 {
                        self.dirty.store(true, Ordering::SeqCst);
                        self.request_search();
                    }
                }
                drives_to_catch_up.clear();
                self.report(IndexState::Watching);
                {
                    // A catch-up finds new and removed entries, not changed
                    // attributes, so details read before this session (or
                    // while nothing was watched) are not trusted.
                    let _index = self.index.write().expect("index lock");
                    self.forget_details();
                    self.applying_changes.store(true, Ordering::SeqCst);
                }
                self.watch_loop(&mut watcher);
                self.applying_changes.store(false, Ordering::SeqCst);
                false
            }
            Err(_) => {
                self.report(IndexState::Static);
                while !self.interrupted(true) {
                    std::thread::sleep(Duration::from_millis(250));
                    if self.mounts_changed.swap(false, Ordering::SeqCst) && self.sync_drives(None).0
                    {
                        self.dirty.store(true, Ordering::SeqCst);
                        self.request_search();
                        self.report(IndexState::Static);
                    }
                }
                // Not caught up: nothing watched meanwhile.
                catch_up
            }
        }
    }

    /// Builds the index from scratch over the current roots (the home
    /// folder when there are none) and swaps it in. Returns false, with the
    /// index as it was, when the task (shown as `kind`) is cancelled.
    fn scan_fresh(&self, kind: TaskKind) -> bool {
        self.report(IndexState::Scanning);
        let roots = {
            let mut roots = self.roots.lock().expect("lock");
            if roots.is_empty() {
                *roots = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .into_iter()
                    .collect();
            }
            roots.clone()
        };
        let options = self.scan_options();
        let task = self.tasks.begin(kind, true);
        task.progress().set_total(self.estimate(&roots, &options));
        let mut store = NodeStore::new();
        if scan_with_progress(&mut store, &roots, &options, task.progress()).is_err() {
            return false;
        }
        let fresh = self.with_drives(Index::new(store));
        if task.progress().is_cancelled() {
            return false;
        }
        self.replace_index(fresh);
        *self.indexed_with.lock().expect("lock") = (roots, options.rules);
        self.incomplete.store(false, Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
        true
    }

    /// The expected number of entries of a scan of `roots`: as many as the
    /// index has now when it covers the same folders, otherwise what the
    /// file systems say (see [`progress::estimate_scan`]).
    fn estimate(&self, roots: &[PathBuf], options: &ScanOptions) -> Option<u64> {
        let same = {
            let (indexed, _) = &*self.indexed_with.lock().expect("lock");
            !self.incomplete.load(Ordering::SeqCst) && indexed.as_slice() == roots
        };
        if same {
            let volumes = self.volumes.lock().expect("lock");
            let drives: u64 = volumes
                .iter()
                .filter(|v| v.root.is_some())
                .map(|v| v.info.entries)
                .sum();
            let all = self.index.read().expect("index lock").len() as u64;
            if all > drives {
                return Some(all - drives);
            }
        }
        progress::estimate_scan(roots, &options.rules.excluded_paths())
    }

    /// Applies file changes until the engine stops or a rescan is asked for.
    fn watch_loop(&self, watcher: &mut Watcher) {
        let mut last_refresh = Instant::now();
        let mut last_save = Instant::now();
        let mut pending_refresh = false;
        while !self.interrupted(true) {
            if self.mounts_changed.swap(false, Ordering::SeqCst)
                && self.sync_drives(Some(watcher)).0
            {
                self.dirty.store(true, Ordering::SeqCst);
                pending_refresh = true;
            }
            match watcher.wait(Some(Duration::from_millis(250))) {
                Ok(events) if !events.is_empty() => {
                    let mut index = self.index.write().expect("index lock");
                    let stats = watcher.apply(&mut index, &events);
                    if index.should_compact() {
                        let remap = watcher.compact(&mut index);
                        self.bump_epoch();
                        let mut shown = self.shown.lock().expect("lock");
                        *shown =
                            Arc::new(shown.iter().filter_map(|id| remap[id.index()]).collect());
                        pending_refresh = true;
                    }
                    if stats.overflow {
                        // Events were lost: details may have changed unseen.
                        self.forget_details();
                        drop(index);
                        self.rescan_after_overflow(watcher);
                        pending_refresh = true;
                    } else if stats.added + stats.removed + stats.moved + stats.updated > 0 {
                        pending_refresh = true;
                    }
                    if pending_refresh {
                        self.dirty.store(true, Ordering::SeqCst);
                    }
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
            if pending_refresh && last_refresh.elapsed() >= REFRESH_EVERY {
                pending_refresh = false;
                last_refresh = Instant::now();
                self.request_search();
                self.report(IndexState::Watching);
            }
            if self.dirty.load(Ordering::SeqCst) && last_save.elapsed() >= SAVE_EVERY {
                self.save();
                last_save = Instant::now();
            }
        }
    }

    fn rescan_after_overflow(&self, watcher: &mut Watcher) {
        self.report(IndexState::Scanning);
        let roots = self.roots.lock().expect("lock").clone();
        let options = self.scan_options();
        let task = self.tasks.begin(TaskKind::Rescanning, true);
        task.progress().set_total(self.estimate(&roots, &options));
        let mut store = NodeStore::new();
        if scan_with_progress(&mut store, &roots, &options, task.progress()).is_err() {
            drop(task);
            // The previous index stays and is caught up instead, which
            // finds added and removed entries.
            self.report(IndexState::CatchingUp);
            watcher.catch_up(&mut self.index.write().expect("index lock"));
            self.report(IndexState::Watching);
            (self.hooks.on_stopped)(Stopped::Rescan);
            return;
        }
        drop(task);
        let fresh = self.with_drives(Index::new(store));
        let mut index = self.index.write().expect("index lock");
        *index = fresh;
        self.bump_epoch();
        if let Ok(mut fresh) = Watcher::new(&index, &options) {
            fresh.catch_up(&mut index);
            *watcher = fresh;
        }
        drop(index);
        // Saves record what this index was built with.
        *self.indexed_with.lock().expect("lock") = (roots, options.rules);
        *self.shown.lock().expect("lock") = Arc::new(Vec::new());
    }

    /// Saves the index if it changed since the last save: the folders to
    /// index to the index file, each connected drive to its own file.
    /// Drives that are not connected were saved when they went away.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        // The folders and rules the index in memory was built with, which
        // differ from the asked-for ones while a new index is being built.
        let (roots, rules) = self.indexed_with.lock().expect("lock").clone();
        let rules = rules.spec();
        let mut volumes = self.volumes.lock().expect("lock");
        let index = self.index.read().expect("index lock");
        let drive_roots: Vec<NodeId> = volumes
            .iter()
            .filter_map(|v| root_id(&index, v.root.as_deref()?))
            .collect();
        let mut ok = true;
        // An index cancelled before it was complete is never saved.
        let own_file = self
            .index_file
            .as_ref()
            .filter(|_| !self.incomplete.load(Ordering::SeqCst));
        if let Some(path) = own_file {
            ok = if drive_roots.is_empty() {
                persist::save_with_rules(&index, &roots, &rules, path).is_ok()
            } else {
                let own: Vec<NodeId> = index
                    .roots()
                    .into_iter()
                    .filter(|r| !drive_roots.contains(r))
                    .collect();
                persist::save_with_rules(&index.extract(&own).0, &roots, &rules, path).is_ok()
            };
        }
        if let Some(env) = &self.drive_env {
            for v in volumes.iter_mut().filter(|v| v.online) {
                ok &= save_drive(env, &index, v).is_ok();
            }
        }
        if !ok {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// `fresh` with the drives of the current index added to it (their
    /// entries as they are, online or offline), for a new scan of the
    /// folders to index.
    fn with_drives(&self, mut fresh: Index) -> Index {
        let volumes = self.volumes.lock().expect("lock");
        let old = self.index.read().expect("index lock");
        for v in volumes.iter() {
            let Some(name) = v.root.as_deref() else {
                continue;
            };
            if let (Some(root), None) = (root_id(&old, name), root_id(&fresh, name)) {
                fresh.graft(&old.extract(&[root]).0);
            }
        }
        fresh
    }

    /// Loads the saved index of every known drive into the index, marked
    /// offline until the drive is found mounted.
    fn load_drives(&self) {
        let Some(env) = &self.drive_env else { return };
        let ignored = self.drive_settings.lock().expect("lock").ignored.clone();
        let configured = self.roots();
        let rules = self.scan_options().rules;
        let fingerprint = Some(persist::rules_fingerprint(&rules.spec()));
        let mut volumes = self.volumes.lock().expect("lock");
        for mut info in drives::known_drives(env) {
            let mut root = None;
            if !ignored.contains(&info.uuid) {
                // A drive read with other rules (or saved without them) is
                // read again when it is connected.
                if let Some(saved) = persist::load(&env.index_path(&info.uuid))
                    .ok()
                    .filter(|saved| saved.rules_hash == fingerprint)
                {
                    let mut drive = saved.index;
                    if let [r] = drive.roots()[..] {
                        let name = path_of(drive.store().name(r));
                        if !overlaps(&configured, &name) {
                            let mut index = self.index.write().expect("index lock");
                            let free = free_name(&index, &name, &info.uuid);
                            if free != name {
                                let _ = drive.rename(r, None, free.as_os_str().as_bytes());
                            }
                            drive.set_offline(r, true);
                            index.graft(&drive);
                            info.entries = drive.len() as u64;
                            root = Some(free);
                        }
                    }
                }
            }
            volumes.push(Volume {
                info,
                root,
                online: false,
                rules: rules.clone(),
            });
        }
    }

    /// Brings the drives in the index in line with the mount table, the
    /// settings and the window's requests. With a watcher, drives that come
    /// online are watched and caught up at once; without one (before
    /// watching starts) their roots are returned for a catch-up. Returns
    /// whether the index or the list of drives changed.
    fn sync_drives(&self, mut watcher: Option<&mut Watcher>) -> (bool, Vec<NodeId>) {
        let Some(env) = &self.drive_env else {
            return (false, Vec::new());
        };
        let settings = self.drive_settings.lock().expect("lock").clone();
        let commands = std::mem::take(&mut *self.drive_commands.lock().expect("lock"));
        let scanned = std::mem::take(&mut *self.scanned.lock().expect("lock"));
        let mut volumes = self.volumes.lock().expect("lock");
        let mut changed = !commands.is_empty();
        let mut to_catch_up = Vec::new();
        let now = unix_now();

        for command in commands {
            match command {
                DriveCommand::Forget(uuid) => {
                    if let Some(i) = volumes.iter().position(|v| v.info.uuid == uuid) {
                        self.drop_entries(&mut volumes[i], watcher.as_deref_mut());
                        volumes.remove(i);
                    }
                    let _ = drives::forget(env, &uuid, true);
                    self.not_now.lock().expect("lock").remove(&uuid);
                }
                DriveCommand::Ignore(uuid, true) => {
                    if let Some(v) = volumes.iter_mut().find(|v| v.info.uuid == uuid) {
                        self.drop_entries(v, watcher.as_deref_mut());
                        v.info.entries = 0;
                        if env.info_path(&uuid).exists() {
                            let _ = v.info.save(env);
                        }
                    }
                    let _ = drives::forget(env, &uuid, false);
                }
                // Its notes mark it as asked for; it is read below, now or
                // when it is mounted.
                DriveCommand::Ignore(uuid, false) => {
                    if let Some(v) = volumes.iter().find(|v| v.info.uuid == uuid) {
                        let _ = v.info.save(env);
                    }
                    self.not_now.lock().expect("lock").remove(&uuid);
                }
            }
        }

        let Ok(mounted) = drives::removable_drives(env) else {
            return (changed, to_catch_up);
        };
        let configured = self.roots();
        let mounted: Vec<Drive> = mounted
            .into_iter()
            .filter(|d| !overlaps(&configured, &d.mount_point))
            .collect();
        let at = |uuid: &str| mounted.iter().find(|d| d.uuid == uuid);

        // Drives that went away (or moved, or are no longer wanted) first, so
        // their mount points are free for the drives that are there now.
        for i in 0..volumes.len() {
            let v = &volumes[i];
            if !v.online {
                continue;
            }
            let still = at(&v.info.uuid)
                .is_some_and(|d| v.root.as_deref().is_none_or(|r| r == d.mount_point));
            if still {
                continue;
            }
            changed = true;
            self.not_now.lock().expect("lock").remove(&v.info.uuid);
            let v = &mut volumes[i];
            v.online = false;
            v.info.last_seen = now;
            if v.root.is_some() {
                if settings.keep_offline {
                    self.take_offline(env, v, watcher.as_deref_mut());
                } else {
                    self.drop_entries(v, watcher.as_deref_mut());
                    v.info.entries = 0;
                    let _ = drives::forget(env, &v.info.uuid, false);
                    let _ = v.info.save(env);
                }
            } else if env.info_path(&v.info.uuid).exists() {
                let _ = v.info.save(env);
            }
        }
        // Drives seen only while connected and never indexed are not kept.
        volumes.retain(|v| v.online || v.root.is_some() || env.info_path(&v.info.uuid).exists());

        // Drives read for the first time on a helper thread.
        for Scanned {
            drive,
            index: new,
            rules,
        } in scanned
        {
            self.scanning.lock().expect("lock").remove(&drive.uuid);
            let Some(new) = new else {
                self.not_now
                    .lock()
                    .expect("lock")
                    .insert(drive.uuid.clone());
                (self.hooks.on_stopped)(Stopped::Drive(drive_name(&drive)));
                changed = true;
                continue;
            };
            let Some(v) = volumes.iter_mut().find(|v| v.info.uuid == drive.uuid) else {
                continue;
            };
            if !v.online || v.root.is_some() || v.info.path != drive.mount_point {
                continue;
            }
            let mut index = self.index.write().expect("index lock");
            if root_id(&index, &drive.mount_point).is_some() {
                continue;
            }
            let root = index.graft(&new)[0];
            v.root = Some(drive.mount_point.clone());
            v.rules = rules;
            v.info.entries = new.len() as u64;
            let _ = v.info.save(env);
            changed = true;
            match watcher.as_deref_mut() {
                Some(w) => {
                    w.watch_root(&index, root);
                    // Changes made while it was being read.
                    w.catch_up_roots(&mut index, &[root]);
                }
                None => to_catch_up.push(root),
            }
        }

        for drive in &mounted {
            let i = match volumes.iter().position(|v| v.info.uuid == drive.uuid) {
                Some(i) => i,
                None => {
                    volumes.push(Volume {
                        info: new_info(drive, now),
                        root: None,
                        online: false,
                        rules: IndexRules::default(),
                    });
                    volumes.len() - 1
                }
            };
            let was_online = std::mem::replace(&mut volumes[i].online, true);
            if !was_online {
                changed = true;
                let v = &mut volumes[i];
                v.info.label = drive.label.clone();
                v.info.last_seen = now;
                if v.root.is_none() {
                    v.info.path = drive.mount_point.clone();
                }
            }
            if settings.ignored.contains(&drive.uuid) {
                // Remembered, so the list can offer to index it again.
                if !was_online {
                    let _ = volumes[i].info.save(env);
                }
                continue;
            }
            if volumes[i].root.is_some() {
                if was_online {
                    continue;
                }
                // Known: back online, at its new mount point if it moved.
                self.free_mount_point(&mut volumes, i, &drive.mount_point);
                let v = &mut volumes[i];
                let mut index = self.index.write().expect("index lock");
                let Some(root) = v.root.as_deref().and_then(|r| root_id(&index, r)) else {
                    v.root = None;
                    continue;
                };
                if v.root.as_deref() != Some(&drive.mount_point) {
                    let _ = index.rename(root, None, drive.mount_point.as_os_str().as_bytes());
                    v.root = Some(drive.mount_point.clone());
                }
                index.set_offline(root, false);
                v.info.path = drive.mount_point.clone();
                let _ = v.info.save(env);
                match watcher.as_deref_mut() {
                    Some(w) => {
                        w.watch_root(&index, root);
                        w.catch_up_roots(&mut index, &[root]);
                    }
                    None => to_catch_up.push(root),
                }
            } else if (settings.auto || env.info_path(&drive.uuid).exists())
                && !self.not_now.lock().expect("lock").contains(&drive.uuid)
            {
                // Not in the index: read it when drives are indexed
                // automatically, or when this one was indexed before or
                // asked for (it has notes then).
                self.free_mount_point(&mut volumes, i, &drive.mount_point);
                self.start_scan(drive.clone());
            }
        }
        drop(volumes);
        if changed {
            self.request_search();
            let state = if watcher.is_some() {
                IndexState::Watching
            } else {
                IndexState::Static
            };
            self.report(state);
        }
        (changed, to_catch_up)
    }

    /// Reads a drive for the first time on a helper thread; the result is
    /// added by the next [`sync_drives`](Self::sync_drives).
    fn start_scan(&self, drive: Drive) {
        if !self
            .scanning
            .lock()
            .expect("lock")
            .insert(drive.uuid.clone())
        {
            return;
        }
        let options = self.scan_options();
        let (scanned, changed) = (Arc::clone(&self.scanned), Arc::clone(&self.mounts_changed));
        let tasks = Arc::clone(&self.tasks);
        let _ = std::thread::Builder::new()
            .name("drive scan".into())
            .spawn(move || {
                let task = tasks.begin(TaskKind::Drive(drive_name(&drive)), true);
                let roots = std::slice::from_ref(&drive.mount_point);
                task.progress().set_total(progress::estimate_scan(
                    roots,
                    &options.rules.excluded_paths(),
                ));
                let mut store = NodeStore::new();
                let index = scan_with_progress(&mut store, roots, &options, task.progress())
                    .ok()
                    .map(|_| Index::new(store));
                drop(task);
                scanned.lock().expect("lock").push(Scanned {
                    drive,
                    index,
                    rules: options.rules,
                });
                changed.store(true, Ordering::SeqCst);
            });
    }

    /// Renames the root of another drive that sits at `mount_point` (a
    /// drive that is not connected, last seen there), so drive `keep` can
    /// take that name.
    fn free_mount_point(&self, volumes: &mut [Volume], keep: usize, mount_point: &Path) {
        let mut index = self.index.write().expect("index lock");
        let Some(taken) = root_id(&index, mount_point) else {
            return;
        };
        for (i, v) in volumes.iter_mut().enumerate() {
            if i != keep && v.root.as_deref() == Some(mount_point) {
                let free = free_name(&index, mount_point, &v.info.uuid);
                let _ = index.rename(taken, None, free.as_os_str().as_bytes());
                v.root = Some(free);
            }
        }
    }

    /// Marks a drive's root offline, stops watching it and saves its
    /// entries to its own file.
    fn take_offline(&self, env: &DriveEnv, v: &mut Volume, watcher: Option<&mut Watcher>) {
        let Some(name) = v.root.clone() else { return };
        {
            let mut index = self.index.write().expect("index lock");
            let Some(root) = root_id(&index, &name) else {
                v.root = None;
                return;
            };
            if let Some(w) = watcher {
                w.unwatch_root(&index, root);
            }
            index.set_offline(root, true);
        }
        let index = self.index.read().expect("index lock");
        if save_drive(env, &index, v).is_err() {
            let _ = v.info.save(env);
        }
    }

    /// Removes a drive's entries from the index.
    fn drop_entries(&self, v: &mut Volume, watcher: Option<&mut Watcher>) {
        let Some(name) = v.root.take() else { return };
        let mut index = self.index.write().expect("index lock");
        if let Some(root) = root_id(&index, &name) {
            if let Some(w) = watcher {
                w.unwatch_root(&index, root);
            }
            index.remove(root);
            index.take_removed();
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Stops the threads and saves the index. The search thread is joined; the
    /// index thread is not, because a first scan cannot be interrupted and the
    /// window should close at once. It ends with the process.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Dropping the sender wakes the search thread so it sees the stop flag.
        self.searches.lock().expect("lock").take();
        let mut threads = self.threads.lock().expect("lock");
        if !threads.is_empty() {
            let _ = threads.remove(0).join();
        }
        drop(threads);
        self.save();
    }
}

/// The roots of `index` whose drive is connected.
fn online_roots(index: &Index) -> Vec<NodeId> {
    index
        .roots()
        .into_iter()
        .filter(|&r| !index.is_offline(r))
        .collect()
}

/// A drive's name for the status line: its label, or where it is mounted.
fn drive_name(drive: &Drive) -> String {
    if drive.label.is_empty() {
        drive.mount_point.display().to_string()
    } else {
        drive.label.clone()
    }
}

/// The root of the index named `name`.
fn root_id(index: &Index, name: &Path) -> Option<NodeId> {
    index.find_child(None, name.as_os_str().as_bytes())
}

fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// Whether `path` lies in one of `roots` or holds one of them: a drive
/// there would list entries twice.
fn overlaps(roots: &[PathBuf], path: &Path) -> bool {
    roots
        .iter()
        .any(|r| path.starts_with(r) || r.starts_with(path))
}

/// `wanted` when no root has that name, otherwise `wanted (uuid)`, with a
/// number added if even that is taken. Roots must have different names.
fn free_name(index: &Index, wanted: &Path, uuid: &str) -> PathBuf {
    if root_id(index, wanted).is_none() {
        return wanted.to_path_buf();
    }
    (1..)
        .map(|n| {
            let mut name = wanted.as_os_str().to_owned();
            name.push(if n == 1 {
                format!(" ({uuid})")
            } else {
                format!(" ({uuid}) {n}")
            });
            PathBuf::from(name)
        })
        .find(|p| root_id(index, p).is_none())
        .expect("a free name")
}

/// Saves a drive's entries to its own file and its notes.
fn save_drive(env: &DriveEnv, index: &Index, v: &mut Volume) -> std::io::Result<()> {
    let Some(name) = v.root.as_deref() else {
        return Ok(());
    };
    let Some(root) = root_id(index, name) else {
        return Ok(());
    };
    let (drive, _) = index.extract(&[root]);
    v.info.entries = drive.len() as u64;
    persist::save_with_rules(
        &drive,
        &[name.to_path_buf()],
        &v.rules.spec(),
        &env.index_path(&v.info.uuid),
    )?;
    v.info.save(env)
}

/// Notes about a drive seen for the first time.
fn new_info(drive: &Drive, now: i64) -> DriveInfo {
    DriveInfo {
        uuid: drive.uuid.clone(),
        label: drive.label.clone(),
        path: drive.mount_point.clone(),
        last_seen: now,
        entries: 0,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;
    use std::fs;
    use std::path::Path;

    /// Rules that exclude `paths`.
    fn folders(paths: Vec<PathBuf>) -> IndexRules {
        IndexRules::new(everygrep_core::rules::RuleSpec {
            folders: paths,
            ..Default::default()
        })
        .unwrap()
    }

    /// Names of the results for `query`, once the engine reports `state`
    /// with the index in place.
    struct Probe {
        engine: Arc<Engine>,
        states: mpsc::Receiver<IndexState>,
        results: mpsc::Receiver<SearchResult>,
        stopped: mpsc::Receiver<Stopped>,
        /// While set, the next task to begin when none runs is cancelled at
        /// once.
        armed: Arc<AtomicBool>,
        /// While it holds a receiver, the next task to begin when none runs
        /// sends on `held` and waits until the receiver gets a message.
        hold: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
        held: mpsc::Receiver<()>,
    }

    impl Probe {
        fn start(roots: Vec<PathBuf>, exclude: Vec<PathBuf>, index: PathBuf) -> Self {
            Self::start_with(roots, exclude, index, None)
        }

        /// Like [`start`](Self::start), cancelling the first task at once.
        fn start_armed(roots: Vec<PathBuf>, index: PathBuf) -> Self {
            Self::start_full(roots, Vec::new(), index, None, true, true)
        }

        /// Like [`start`](Self::start), with watching turned off.
        fn start_unwatched(roots: Vec<PathBuf>, index: PathBuf) -> Self {
            Self::start_full(roots, Vec::new(), index, None, false, false)
        }

        fn start_with(
            roots: Vec<PathBuf>,
            exclude: Vec<PathBuf>,
            index: PathBuf,
            drives: Option<(DriveEnv, DriveSettings)>,
        ) -> Self {
            Self::start_full(roots, exclude, index, drives, false, true)
        }

        fn start_full(
            roots: Vec<PathBuf>,
            exclude: Vec<PathBuf>,
            index: PathBuf,
            drives: Option<(DriveEnv, DriveSettings)>,
            armed: bool,
            watch: bool,
        ) -> Self {
            let (state_tx, states) = mpsc::channel();
            let (result_tx, results) = mpsc::channel();
            let (stopped_tx, stopped) = mpsc::channel();
            let state_tx = Mutex::new(state_tx);
            let result_tx = Mutex::new(result_tx);
            let stopped_tx = Mutex::new(stopped_tx);
            let armed = Arc::new(AtomicBool::new(armed));
            let hold: Arc<Mutex<Option<mpsc::Receiver<()>>>> = Arc::default();
            let (held_tx, held) = mpsc::channel();
            let held_tx = Mutex::new(held_tx);
            let slot: Arc<std::sync::OnceLock<std::sync::Weak<Engine>>> = Arc::default();
            let hooks = Hooks {
                on_results: Box::new(move |r| {
                    let _ = result_tx.lock().unwrap().send(r);
                }),
                on_state: Box::new(move |s, _| {
                    let _ = state_tx.lock().unwrap().send(s);
                }),
                on_busy: Box::new({
                    let (armed, slot) = (Arc::clone(&armed), Arc::clone(&slot));
                    let hold = Arc::clone(&hold);
                    move || {
                        let release = hold.lock().unwrap().take();
                        if let Some(release) = release {
                            let _ = held_tx.lock().unwrap().send(());
                            let _ = release.recv_timeout(Duration::from_secs(20));
                        }
                        if !armed.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        // The first task may begin before `start` returns.
                        let engine = loop {
                            if let Some(e) = slot.get() {
                                break e.upgrade();
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        };
                        if let Some(engine) = engine {
                            engine.cancel_all();
                        }
                    }
                }),
                on_stopped: Box::new(move |s| {
                    let _ = stopped_tx.lock().unwrap().send(s);
                }),
            };
            let engine = Engine::start(roots, folders(exclude), watch, Some(index), drives, hooks);
            let _ = slot.set(Arc::downgrade(&engine));
            Self {
                engine,
                states,
                results,
                stopped,
                armed,
                hold,
                held,
            }
        }

        fn wait_stopped(&self) -> Stopped {
            self.stopped
                .recv_timeout(Duration::from_secs(20))
                .expect("a cancel")
        }

        fn wait_for(&self, state: IndexState) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.states.recv_timeout(left) {
                    Ok(s)
                        if s == state
                            || (state == IndexState::Watching && s == IndexState::Static) =>
                    {
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => panic!("no {state:?}: {e}"),
                }
            }
        }

        fn names(&self, query: &str) -> Vec<String> {
            while self.results.try_recv().is_ok() {}
            self.engine.set_query(query, QueryOptions::default());
            loop {
                let r = self
                    .results
                    .recv_timeout(Duration::from_secs(20))
                    .expect("results");
                if r.query != query {
                    continue;
                }
                let index = self.engine.index.read().unwrap();
                let mut names: Vec<String> = r
                    .ids
                    .iter()
                    .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                    .collect();
                names.sort();
                return names;
            }
        }
    }

    #[test]
    fn reindex_takes_new_roots_and_excludes_and_saves() {
        let dir = Scratch::new("reindex");
        let root = |p: &str| dir.0.join(p);
        for f in ["a/one.txt", "a/skip/two.txt", "b/three.txt"] {
            fs::create_dir_all(root(f).parent().unwrap()).unwrap();
            fs::write(root(f), "").unwrap();
        }
        let index = root("index.bin");
        let probe = Probe::start(vec![root("a")], Vec::new(), index.clone());
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt", "two.txt"]);

        probe
            .engine
            .reindex(&[root("a"), root("b")], folders(vec![root("a/skip")]));
        probe.wait_for(IndexState::Scanning);
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt", "three.txt"]);
        // The excluded folder is left out with its own entry.
        assert!(probe.names("skip").is_empty());
        let mut roots = probe.engine.roots();
        roots.sort();
        let canonical = fs::canonicalize(&dir.0).unwrap();
        assert_eq!(roots, [canonical.join("a"), canonical.join("b")]);
        // The new index was saved at once, with its roots.
        let saved = persist::load(&index).unwrap();
        assert_eq!(saved.roots, roots);
        probe.engine.shutdown();
    }

    #[test]
    fn fresh_index_and_successful_rescan_catch_changes_before_watch_installation() {
        let dir = Scratch::new("scan-watch-gap");
        let root = dir.0.join("files");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("one.txt"), b"one").unwrap();
        let (state_tx, states) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let hooks = Hooks {
            on_results: Box::new(|_| {}),
            on_busy: Box::new(|| {}),
            on_stopped: Box::new(|_| {}),
            on_state: Box::new(move |state, _| {
                state_tx.send(state).unwrap();
                if state == IndexState::Scanning {
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        };
        let engine = Engine::start(
            vec![root.clone()],
            IndexRules::default(),
            true,
            Some(dir.0.join("index.bin")),
            None,
            hooks,
        );
        let wait_for = |expected| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let state = states
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if state == expected {
                    break;
                }
            }
        };
        for (phase, scanned_entries) in [("first", 2), ("rescan", 3)] {
            if phase == "rescan" {
                engine.reindex(std::slice::from_ref(&root), IndexRules::default());
            }
            wait_for(IndexState::Scanning);
            // Hold publication of the fresh tree. The scan's completed-entry
            // counter is a barrier after its last listing, before watches exist.
            let old = engine.index.read().unwrap();
            release_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let scanned = engine.tasks.list.lock().unwrap().iter().any(|task| {
                    matches!(
                        *task.kind.lock().unwrap(),
                        TaskKind::Indexing | TaskKind::Rescanning
                    ) && task.progress.done() == scanned_entries
                });
                if scanned {
                    break;
                }
                assert!(Instant::now() < deadline, "scan did not reach publication");
                std::thread::yield_now();
            }
            let gap = root.join(format!("gap-{phase}.txt"));
            fs::write(&gap, b"created before watches").unwrap();
            assert!(find(&old, &gap).is_none());
            drop(old);
            wait_for(IndexState::Watching);
            assert!(
                find(&engine.index.read().unwrap(), &gap).is_some(),
                "{phase} missed the gap"
            );
        }
        engine.shutdown();
    }

    #[test]
    fn watching_can_be_turned_off_and_on() {
        let dir = Scratch::new("unwatched");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("one.txt"), "").unwrap();
        let probe = Probe::start_unwatched(vec![a.clone()], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        // Off: changes are not applied.
        fs::write(a.join("two.txt"), "").unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(probe.names("txt"), ["one.txt"]);

        // On: what changed meanwhile is caught up, and then watched.
        probe.engine.set_watching(true);
        probe.wait_for(IndexState::CatchingUp);
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt", "two.txt"]);
        fs::write(a.join("three.txt"), "").unwrap();
        let canonical = fs::canonicalize(&a).unwrap();
        until(&probe.engine, "the new file", |i| {
            find(i, &canonical.join("three.txt")).is_some()
        });

        // Off again.
        probe.engine.set_watching(false);
        probe.wait_for(IndexState::Unwatched);
        fs::remove_file(a.join("one.txt")).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(probe.names("txt"), ["one.txt", "three.txt", "two.txt"]);
        probe.engine.shutdown();
    }

    #[test]
    fn an_index_in_an_older_name_order_is_sorted_again_and_saved() {
        let dir = Scratch::new("old-order-index");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        for name in ["file10", "file2", "cay", "Çay", "Zoo"] {
            fs::write(a.join(name), "").unwrap();
        }
        // A file with the window's rules in the name order of version 4
        // (folded bytes, then raw bytes). Files of version 4 itself have no
        // rules and are built again.
        let index_file = dir.0.join("index.bin");
        let roots = normalize_roots(std::slice::from_ref(&a));
        let mut store = NodeStore::new();
        everygrep_core::scan::scan(&mut store, &roots, &ScanOptions::default());
        let index = Index::new(store);
        let rules = everygrep_core::rules::RuleSpec::default();
        persist::save_with_rules(&index, &roots, &rules, &index_file).unwrap();
        let s = index.store();
        let mut old: Vec<NodeId> = index.order().iter().collect();
        old.sort_by(|&x, &y| (s.folded_name(x), s.name(x)).cmp(&(s.folded_name(y), s.name(y))));
        assert!(!old.iter().copied().eq(index.order().iter()));
        persist::rewrite_order(&index_file, 6, &old).unwrap();
        assert!(persist::load(&index_file).unwrap().reordered);

        // Unwatched, nothing but the new order changes the loaded index.
        let probe = Probe::start_unwatched(vec![a.clone()], index_file.clone());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match probe.states.recv_timeout(left).expect("unwatched") {
                IndexState::Unwatched => break,
                IndexState::Scanning => panic!("the saved index was not loaded"),
                _ => {}
            }
        }
        assert!(probe.engine.dirty.load(Ordering::SeqCst));
        probe.engine.set_query("", QueryOptions::default());
        let names: Vec<String> = loop {
            let r = probe
                .results
                .recv_timeout(Duration::from_secs(20))
                .expect("results");
            if r.query.is_empty() && r.ids.len() == 6 {
                let index = probe.engine.index.read().unwrap();
                break r
                    .ids
                    .iter()
                    .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                    .collect();
            }
        };
        let at = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert!(at("file2") < at("file10"), "{names:?}");
        assert_eq!(at("cay").abs_diff(at("Çay")), 1, "{names:?}");

        // Closing saves the new order, which needs no sorting.
        probe.engine.shutdown();
        assert_eq!(fs::read(&index_file).unwrap()[8..12], 6u32.to_le_bytes());
        let again = persist::load(&index_file).unwrap();
        assert!(!again.reordered);
        assert_eq!(again.rules_hash, Some(persist::rules_fingerprint(&rules)));
        assert!(again.index.order().iter().eq(index.order().iter()));
    }

    /// Waits until the engine watches (or cannot) and returns whether it
    /// scanned on the way, instead of using the saved index.
    fn scanned_at_start(probe: &Probe) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut scanned = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match probe.states.recv_timeout(left).expect("watching") {
                IndexState::Scanning => scanned = true,
                IndexState::Watching | IndexState::Static => return scanned,
                _ => {}
            }
        }
    }

    #[test]
    fn index_saved_without_or_with_other_rules_is_built_again() {
        let dir = Scratch::new("cache-rules");
        let a = dir.0.join("a");
        for f in ["one.txt", "skip/two.txt"] {
            fs::create_dir_all(a.join(f).parent().unwrap()).unwrap();
            fs::write(a.join(f), "").unwrap();
        }
        let index_file = dir.0.join("index.bin");
        let roots = normalize_roots(std::slice::from_ref(&a));
        // A cache without rules (an older window), holding the whole tree.
        let mut store = NodeStore::new();
        everygrep_core::scan::scan(&mut store, &roots, &ScanOptions::default());
        persist::save(&Index::new(store), &roots, &index_file).unwrap();

        let skip = vec![roots[0].join("skip")];
        let probe = Probe::start(roots.clone(), skip.clone(), index_file.clone());
        assert!(
            scanned_at_start(&probe),
            "loaded a cache with unknown rules"
        );
        assert_eq!(probe.names("txt"), ["one.txt"]);
        let rules = probe.engine.scan_options().rules.spec();
        probe.engine.shutdown();
        let saved = persist::load(&index_file).unwrap();
        assert_eq!(saved.rules_hash, Some(persist::rules_fingerprint(&rules)));

        // The same rules load it.
        let probe = Probe::start(roots.clone(), skip, index_file.clone());
        assert!(!scanned_at_start(&probe), "same rules scanned again");
        assert_eq!(probe.names("txt"), ["one.txt"]);
        probe.engine.shutdown();

        // Other rules build it again, over the saved folders when none are
        // asked for.
        let probe = Probe::start(Vec::new(), Vec::new(), index_file.clone());
        assert!(scanned_at_start(&probe), "loaded a cache with other rules");
        assert_eq!(probe.names("txt"), ["one.txt", "two.txt"]);
        assert_eq!(probe.engine.roots(), roots);
        probe.engine.shutdown();
        let saved = persist::load(&index_file).unwrap();
        assert_eq!(
            saved.rules_hash,
            Some(persist::rules_fingerprint(&Default::default()))
        );
    }

    #[test]
    fn sorting_by_attributes_follows_changes_watched_or_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new("attributes");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        for name in ["x1.txt", "x2.txt", "x3.txt"] {
            fs::write(a.join(name), "").unwrap();
        }
        let chmod = |name: &str, mode| {
            fs::set_permissions(a.join(name), fs::Permissions::from_mode(mode)).unwrap();
        };
        let probe = Probe::start(vec![a.clone()], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        probe.engine.set_sort(Sort {
            key: everygrep_core::sort::SortKey::Attributes,
            ..Sort::default()
        });
        // Results in their order, asked for until they are `want`.
        let wait_order = |want: [&str; 3]| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                while probe.results.try_recv().is_ok() {}
                probe.engine.set_query("txt", QueryOptions::default());
                let r = probe
                    .results
                    .recv_timeout(Duration::from_secs(20))
                    .expect("results");
                let index = probe.engine.index.read().unwrap();
                let got: Vec<String> = r
                    .ids
                    .iter()
                    .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                    .collect();
                drop(index);
                if got == want {
                    return;
                }
                assert!(Instant::now() < deadline, "{got:?}, not {want:?}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        // Plain < X < R; all plain first.
        wait_order(["x1.txt", "x2.txt", "x3.txt"]);
        chmod("x1.txt", 0o444);
        wait_order(["x2.txt", "x3.txt", "x1.txt"]);

        // A change while nothing watches is still seen after watching again,
        // though no event reports it.
        probe.engine.set_watching(false);
        probe.wait_for(IndexState::Unwatched);
        chmod("x2.txt", 0o755);
        probe.engine.set_watching(true);
        probe.wait_for(IndexState::Watching);
        wait_order(["x3.txt", "x2.txt", "x1.txt"]);

        // Unwatched, details are read again for every sort.
        probe.engine.set_watching(false);
        probe.wait_for(IndexState::Unwatched);
        chmod("x3.txt", 0o444);
        wait_order(["x2.txt", "x1.txt", "x3.txt"]);
        probe.engine.shutdown();
    }

    #[test]
    fn refresh_runs_the_same_query_again() {
        let dir = Scratch::new("refresh");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("one.txt"), "").unwrap();
        let probe = Probe::start_unwatched(vec![a.clone()], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        while probe.results.try_recv().is_ok() {}
        let generation = probe.engine.refresh();
        let r = probe
            .results
            .recv_timeout(Duration::from_secs(20))
            .expect("results after a refresh");
        assert_eq!(r.query, "txt", "the same query runs again");
        assert_eq!(r.ids.len(), 1);
        assert!(probe.engine.is_current(generation));
        probe.engine.shutdown();
    }

    #[test]
    fn tasks_show_after_a_while_and_only_cancellable_ones_stop() {
        let calls = Arc::new(AtomicU64::new(0));
        let tasks = Tasks::new(Box::new({
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let a = tasks.begin(TaskKind::Indexing, true);
        let b = tasks.begin(TaskKind::Updating, false);
        // Only the first of tasks running together wakes the window.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Quick tasks are never shown.
        assert_eq!(tasks.shown(a.task.started), None);
        assert_eq!(
            tasks.shown(a.task.started + SHOW_AFTER - Duration::from_millis(1)),
            None
        );
        a.progress().set_total(Some(200));
        a.progress().add(100);
        let later = b.task.started + SHOW_AFTER;
        assert_eq!(
            tasks.shown(later),
            Some(Busy {
                id: a.task.id,
                kind: TaskKind::Indexing,
                done: 100,
                percent: Some(50),
                cancellable: true,
            })
        );
        tasks.cancel(b.task.id);
        assert!(!b.progress().is_cancelled());
        tasks.cancel(a.task.id);
        assert!(a.progress().is_cancelled());
        drop(a);
        let shown = tasks.shown(later).unwrap();
        assert_eq!((shown.kind, shown.cancellable), (TaskKind::Updating, false));
        drop(b);
        assert_eq!(tasks.shown(later + SHOW_AFTER), None);
        let _c = tasks.begin(TaskKind::Searching, true);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancelled_first_index_is_never_saved() {
        let dir = Scratch::new("cancel-first");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.txt"), "").unwrap();
        let index = dir.0.join("index.bin");
        let probe = Probe::start_armed(vec![root.clone()], index.clone());
        assert_eq!(probe.wait_stopped(), Stopped::Index);
        probe.wait_for(IndexState::Watching);
        assert!(probe.names("txt").is_empty());
        // Neither a save now nor one after other changes writes the index.
        probe.engine.dirty.store(true, Ordering::SeqCst);
        probe.engine.save();
        assert!(!index.exists());

        // Built again, it is complete and saved.
        probe
            .engine
            .reindex(&probe.engine.roots(), IndexRules::default());
        probe.wait_for(IndexState::Scanning);
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        let saved = persist::load(&index).unwrap();
        assert_eq!(saved.index.len(), 2);
        probe.engine.shutdown();
    }

    #[test]
    fn cancelled_rescan_keeps_the_previous_index_and_folders() {
        let dir = Scratch::new("cancel-rescan");
        let root = |p: &str| dir.0.join(p);
        for f in ["a/one.txt", "b/two.txt"] {
            fs::create_dir_all(root(f).parent().unwrap()).unwrap();
            fs::write(root(f), "").unwrap();
        }
        let index = root("index.bin");
        let probe = Probe::start(vec![root("a")], Vec::new(), index.clone());
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        probe.engine.dirty.store(true, Ordering::SeqCst);
        probe.engine.save();
        let before = fs::read(&index).unwrap();
        let roots = probe.engine.roots();

        probe.armed.store(true, Ordering::SeqCst);
        probe
            .engine
            .reindex(&[root("b")], folders(vec![root("a/never")]));
        assert_eq!(probe.wait_stopped(), Stopped::Rescan);
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        assert_eq!(probe.engine.roots(), roots);
        assert!(probe.engine.scan_options().rules.is_empty());
        probe.engine.save();
        assert_eq!(fs::read(&index).unwrap(), before);
        // Still watched: a new file shows up.
        fs::write(root("a/three.txt"), "").unwrap();
        until(&probe.engine, "the new file", |i| {
            find(i, &fs::canonicalize(root("a")).unwrap().join("three.txt")).is_some()
        });
        probe.engine.shutdown();
    }

    #[test]
    fn count_keeps_the_first_results_of_the_sort() {
        let dir = Scratch::new("count");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        for (name, size) in [("s1.txt", 1), ("s3.txt", 3), ("s2.txt", 2), ("s4.log", 4)] {
            fs::write(a.join(name), "x".repeat(size)).unwrap();
        }
        let probe = Probe::start(vec![a], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("limit:2 txt").len(), 2);
        probe.engine.set_sort(Sort {
            key: everygrep_core::sort::SortKey::Size,
            descending: true,
            ..Sort::default()
        });
        // In result order: the two largest text files.
        let ordered = |query: &str| -> Vec<String> {
            while probe.results.try_recv().is_ok() {}
            probe.engine.set_query(query, QueryOptions::default());
            loop {
                let r = probe
                    .results
                    .recv_timeout(Duration::from_secs(20))
                    .expect("results");
                if r.query != query {
                    continue;
                }
                assert!(r.error.is_none(), "{query}: {:?}", r.error);
                let index = probe.engine.index.read().unwrap();
                return r
                    .ids
                    .iter()
                    .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                    .collect();
            }
        };
        assert_eq!(ordered("limit:2 txt"), ["s3.txt", "s2.txt"]);
        assert_eq!(ordered("file: limit:1"), ["s4.log"]);
        assert!(ordered("limit:0 txt").is_empty());
        assert_eq!(ordered("limit:10 txt").len(), 3);
        probe.engine.shutdown();
    }

    #[test]
    fn cancelled_sort_keeps_the_previous_order() {
        let dir = Scratch::new("cancel-sort");
        fs::create_dir_all(dir.0.join("a")).unwrap();
        fs::write(dir.0.join("a/one.txt"), "").unwrap();
        let probe = Probe::start(vec![dir.0.join("a")], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        while probe.results.try_recv().is_ok() {}

        probe.armed.store(true, Ordering::SeqCst);
        let by_size = Sort {
            key: everygrep_core::sort::SortKey::Size,
            descending: true,
            ..Sort::default()
        };
        probe.engine.set_sort(by_size);
        assert_eq!(probe.wait_stopped(), Stopped::Sort(Sort::default()));
        assert_eq!(probe.engine.query.lock().unwrap().sort, Sort::default());
        assert!(
            probe
                .results
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "a cancelled sort sends no results"
        );

        // A cancelled search keeps the query that gave the results.
        probe.armed.store(true, Ordering::SeqCst);
        probe
            .engine
            .set_query("nothing-like-this", QueryOptions::default());
        assert_eq!(probe.wait_stopped(), Stopped::Search);
        assert_eq!(probe.engine.query.lock().unwrap().text, "txt");
        probe.engine.shutdown();
    }

    #[test]
    fn cancelled_empty_query_keeps_the_previous_results_and_query() {
        let dir = Scratch::new("cancel-empty-query");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.txt"), "").unwrap();
        fs::write(root.join("two.md"), "").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        while probe.results.try_recv().is_ok() {}

        probe.armed.store(true, Ordering::SeqCst);
        probe.engine.set_query("", QueryOptions::default());
        assert_eq!(probe.wait_stopped(), Stopped::Search);
        assert_eq!(probe.engine.query.lock().unwrap().text, "txt");
        assert_eq!(probe.engine.applied.lock().unwrap().text, "txt");
        assert!(
            probe
                .results
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "a cancelled empty query sends no replacement results"
        );
        assert_eq!(probe.names("md"), ["two.md"]);
        probe.engine.shutdown();
    }

    #[test]
    fn newer_query_cancels_the_running_search_without_a_stop() {
        let dir = Scratch::new("newer-query");
        fs::create_dir_all(dir.0.join("a")).unwrap();
        fs::write(dir.0.join("a/one.txt"), "").unwrap();
        fs::write(dir.0.join("a/two.md"), "").unwrap();
        let probe = Probe::start(vec![dir.0.join("a")], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt"]);

        // The next search stops right after it is listed, as if it were slow.
        let (release, wait) = mpsc::channel();
        *probe.hold.lock().unwrap() = Some(wait);
        probe.engine.set_query("o", QueryOptions::default());
        probe
            .held
            .recv_timeout(Duration::from_secs(20))
            .expect("the search begins");
        let running = probe.engine.tasks.list.lock().unwrap().clone();
        let search = running
            .iter()
            .find(|t| *t.kind.lock().unwrap() == TaskKind::Searching)
            .expect("a running search");
        assert!(!search.progress.is_cancelled());

        probe.engine.set_query("md", QueryOptions::default());
        assert!(
            search.progress.is_cancelled(),
            "a newer query cancels the running search"
        );
        release.send(()).unwrap();
        let r = probe
            .results
            .recv_timeout(Duration::from_secs(20))
            .expect("results");
        assert_eq!(r.query, "md", "the older search sends no results");
        assert_eq!(r.ids.len(), 1);
        assert!(
            probe
                .stopped
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "a search cancelled by a newer query is not reported as stopped"
        );
        assert_eq!(probe.engine.query.lock().unwrap().text, "md");
        probe.engine.shutdown();
    }

    /// A pretend system with removable drives: plain folders as mount
    /// points, device links and a mount table, all in a scratch folder.
    struct FakeDrives {
        dir: PathBuf,
        env: DriveEnv,
        mounts: Mutex<Vec<String>>,
    }

    impl FakeDrives {
        fn new(dir: &Path) -> Self {
            let media = dir.join("media");
            let by_uuid = dir.join("dev/disk/by-uuid");
            fs::create_dir_all(&media).unwrap();
            fs::create_dir_all(&by_uuid).unwrap();
            fs::create_dir_all(dir.join("dev/disk/by-label")).unwrap();
            fs::create_dir_all(dir.join("mnt/data")).unwrap();
            fs::write(dir.join("mnt/data/fixed-disk-file.txt"), "").unwrap();
            let env = DriveEnv {
                mountinfo: dir.join("mountinfo"),
                media_dirs: vec![media],
                by_uuid,
                by_label: dir.join("dev/disk/by-label"),
                volumes_dir: dir.join("volumes"),
            };
            let fake = Self {
                dir: dir.to_path_buf(),
                env,
                mounts: Mutex::new(Vec::new()),
            };
            // A fixed disk under /mnt-like folder, with a UUID: never a drive.
            fake.link("FIXED-DATA", "sdx5");
            fake.add_line(&format!(
                "40 1 8:85 / {} ro - ext4 {} ro",
                dir.join("mnt/data").display(),
                dir.join("dev/sdx5").display()
            ));
            fake
        }

        fn link(&self, uuid: &str, device: &str) {
            let link = self.env.by_uuid.join(uuid);
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(format!("../../{device}"), link).unwrap();
        }

        fn add_line(&self, line: &str) {
            let mut mounts = self.mounts.lock().unwrap();
            mounts.push(line.to_owned());
            fs::write(&self.env.mountinfo, mounts.join("\n") + "\n").unwrap();
        }

        /// Mounts device `device` (UUID `uuid`) at `media/<name>`, which
        /// must exist.
        fn mount(&self, uuid: &str, device: &str, name: &str) {
            self.link(uuid, device);
            let point = self.dir.join("media").join(name);
            self.add_line(&format!(
                "50 1 8:1 / {} rw - exfat {} rw",
                point.display(),
                self.dir.join("dev").join(device).display()
            ));
        }

        fn unmount_all(&self) {
            let mut mounts = self.mounts.lock().unwrap();
            mounts.retain(|l| !l.contains("/media/"));
            fs::write(&self.env.mountinfo, mounts.join("\n") + "\n").unwrap();
        }
    }

    /// Mount tables whose watching thread starts late, as it can on a busy
    /// system.
    static SLOW_MOUNT_THREADS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Delays the start of the thread that watches `mountinfo`, if a test
    /// asked for it.
    pub(super) fn slow_thread_start(mountinfo: &Path) {
        if SLOW_MOUNT_THREADS
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == mountinfo)
        {
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    /// Waits until `check` holds for the engine's index.
    fn until(engine: &Engine, what: &str, check: impl Fn(&Index) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if check(&engine.index.read().unwrap()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    fn find(index: &Index, path: &Path) -> Option<NodeId> {
        index
            .order()
            .iter()
            .find(|&id| index.store().path(id) == path)
    }

    #[test]
    fn drives_come_and_go_and_keep_their_entries() {
        let dir = Scratch::new("drives");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("notes.txt"), "").unwrap();
        let usb = dir.0.join("media/USB");
        fs::create_dir_all(usb.join("photos")).unwrap();
        fs::write(usb.join("photos/beach.jpg"), "jpg").unwrap();
        let index_file = dir.0.join("index.bin");
        let settings = DriveSettings::default();
        let probe = Probe::start_with(
            vec![home.clone()],
            Vec::new(),
            index_file.clone(),
            Some((fake.env.clone(), settings.clone())),
        );
        probe.wait_for(IndexState::Watching);
        let e = &probe.engine;

        // Plugged in: read and watched.
        fake.mount("A1B2-C3D4", "sdz1", "USB");
        until(e, "the drive", |i| {
            find(i, &usb.join("photos/beach.jpg")).is_some()
        });
        fs::write(usb.join("photos/dune.jpg"), "jpg").unwrap();
        until(e, "a new file on the drive", |i| {
            find(i, &usb.join("photos/dune.jpg")).is_some()
        });
        let status = e.drives();
        assert_eq!(status.len(), 1);
        assert!(status[0].online && status[0].indexed && !status[0].ignored);
        assert_eq!(status[0].uuid, "A1B2-C3D4");
        // The fixed disk in the same table is never indexed.
        assert!(probe.names("fixed-disk-file").is_empty());

        // Removed: the entries stay, offline, and the drive is saved.
        fake.unmount_all();
        until(e, "the drive offline", |i| {
            find(i, &usb.join("photos/dune.jpg")).is_some_and(|id| i.is_offline(id))
        });
        assert!(!e.drives()[0].online);
        assert!(fake.env.index_path("A1B2-C3D4").exists());
        let info = drives::known_drives(&fake.env);
        assert_eq!(info[0].entries, 4);
        assert_eq!(info[0].path, usb);
        // Searchable while offline; changes on the folder are not seen.
        assert_eq!(probe.names("dune"), ["dune.jpg"]);
        fs::remove_file(usb.join("photos/beach.jpg")).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(probe.names("beach"), ["beach.jpg"]);

        // Plugged in again at another mount point: same entries, renamed
        // root, caught up.
        let other = dir.0.join("media/USB1");
        fs::rename(&usb, &other).unwrap();
        fs::write(other.join("photos/new.jpg"), "jpg").unwrap();
        fake.mount("A1B2-C3D4", "sdz1", "USB1");
        until(e, "the drive back", |i| {
            find(i, &other.join("photos/new.jpg")).is_some_and(|id| !i.is_offline(id))
        });
        assert!(probe.names("beach").is_empty());
        assert_eq!(probe.names("jpg"), ["dune.jpg", "new.jpg"]);
        assert_eq!(e.drives()[0].path, other);

        // The index file holds the home folder only; the drive has its own.
        e.shutdown();
        let saved = persist::load(&index_file).unwrap();
        assert_eq!(saved.roots, [fs::canonicalize(&home).unwrap()]);
        assert_eq!(saved.index.len(), 2);
        let drive = persist::load(&fake.env.index_path("A1B2-C3D4")).unwrap();
        assert_eq!(drive.roots, std::slice::from_ref(&other));
        assert_eq!(drive.index.len(), 4);
        // Both were saved with the rules they were read with.
        let rules = Some(persist::rules_fingerprint(&Default::default()));
        assert_eq!((saved.rules_hash, drive.rules_hash), (rules, rules));

        // Started with other rules, the drive's saved entries are not used:
        // it is read again when it is connected.
        fake.unmount_all();
        let probe = Probe::start_with(
            vec![home.clone()],
            vec![home.join("never")],
            index_file.clone(),
            Some((fake.env.clone(), settings.clone())),
        );
        probe.wait_for(IndexState::Watching);
        assert!(probe.names("jpg").is_empty());
        assert_eq!(probe.engine.drives().len(), 1);
        assert!(!probe.engine.drives()[0].indexed);
        probe.engine.shutdown();
        assert_eq!(
            persist::load(&fake.env.index_path("A1B2-C3D4"))
                .unwrap()
                .rules_hash,
            rules
        );

        // A new start shows the drive's entries, offline until it is found.
        fake.unmount_all();
        let probe = Probe::start_with(
            vec![home.clone()],
            Vec::new(),
            index_file,
            Some((fake.env.clone(), settings)),
        );
        probe.wait_for(IndexState::Watching);
        until(&probe.engine, "the saved drive", |i| {
            find(i, &other.join("photos/new.jpg")).is_some_and(|id| i.is_offline(id))
        });
        // Forgetting drops entries and files.
        probe.engine.forget_drive("A1B2-C3D4");
        until(&probe.engine, "the drive forgotten", |i| {
            find(i, &other.join("photos/new.jpg")).is_none()
        });
        assert!(probe.engine.drives().is_empty());
        assert!(!fake.env.index_path("A1B2-C3D4").exists());
        assert!(!fake.env.info_path("A1B2-C3D4").exists());
        probe.engine.shutdown();
    }

    #[test]
    fn a_drive_mounted_before_the_mount_thread_runs_is_seen() {
        let dir = Scratch::new("drives-early");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let usb = dir.0.join("media/USB");
        fs::create_dir_all(&usb).unwrap();
        fs::write(usb.join("early.txt"), "").unwrap();
        SLOW_MOUNT_THREADS
            .lock()
            .unwrap()
            .push(fake.env.mountinfo.clone());
        let probe = Probe::start_with(
            vec![home],
            Vec::new(),
            dir.0.join("index.bin"),
            Some((fake.env.clone(), DriveSettings::default())),
        );
        // The drives were looked at once; the mount thread has not started.
        probe.wait_for(IndexState::Watching);
        fake.mount("EARLY-1", "sdx1", "USB");
        until(&probe.engine, "the drive", |i| {
            find(i, &usb.join("early.txt")).is_some()
        });
        probe.engine.shutdown();
    }

    #[test]
    fn removed_drives_can_be_forgotten_or_never_indexed() {
        let dir = Scratch::new("drives-forget");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let card = dir.0.join("media/CARD");
        fs::create_dir_all(&card).unwrap();
        fs::write(card.join("clip.mp4"), "").unwrap();
        let settings = DriveSettings {
            keep_offline: false,
            ..DriveSettings::default()
        };
        let probe = Probe::start_with(
            vec![home],
            Vec::new(),
            dir.0.join("index.bin"),
            Some((fake.env.clone(), settings.clone())),
        );
        probe.wait_for(IndexState::Watching);
        let e = &probe.engine;
        fake.mount("CARD-1", "sdy1", "CARD");
        until(e, "the card", |i| find(i, &card.join("clip.mp4")).is_some());
        fake.unmount_all();
        until(e, "the card forgotten", |i| {
            find(i, &card.join("clip.mp4")).is_none()
        });
        // Its notes stay, its index does not.
        let status = e.drives();
        assert_eq!(status.len(), 1);
        assert!(!status[0].online && !status[0].indexed);
        assert!(!fake.env.index_path("CARD-1").exists());

        // Never index it: plugged in again, it stays out.
        e.set_drive_settings(DriveSettings {
            ignored: vec!["CARD-1".into()],
            ..settings.clone()
        });
        e.ignore_drive("CARD-1", true);
        fake.mount("CARD-1", "sdy1", "CARD");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !e.drives()[0].online {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(e.drives()[0].online && e.drives()[0].ignored);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(probe.names("clip").is_empty());
        // Index it again.
        e.set_drive_settings(settings);
        e.ignore_drive("CARD-1", false);
        until(e, "the card again", |i| {
            find(i, &card.join("clip.mp4")).is_some()
        });
        e.shutdown();
    }
}
