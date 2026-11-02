//! The engine side of the window: loading the index, searching on a worker
//! thread, applying file changes on a watcher thread, and taking removable
//! drives in and out.
//!
//! The index lives behind a read-write lock. Searches and the result table take
//! short read locks; the watcher takes the write lock only while it applies a
//! batch of changes, never while it waits for them.
//!
//! A query with `content:` terms reads files: the index part runs under a
//! read lock as usual, the files are read after it is released (see
//! [`everygrep_core::content`]), and the matches found so far are handed
//! to the window about once a second while they are read.
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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use everygrep_core::content::{ContentCache, ContentOptions, Decisions, decide_all};
use everygrep_core::drives::{self, Drive, DriveEnv, DriveInfo, MountWatch};
use everygrep_core::dupe;
use everygrep_core::hashcache::DEFAULT_MAX_BYTES;
use everygrep_core::index::Index;
use everygrep_core::persist;
use everygrep_core::progress::{self, Progress};
use everygrep_core::query::{HighlightTerm, Query, QueryOptions};
use everygrep_core::rules::{IndexRules, RuleSpec};
use everygrep_core::scan::{ScanOptions, normalize_roots, scan_with_progress};
use everygrep_core::search::{Staged, search_staged, search_with_progress};
use everygrep_core::sort::{DetailCache, Sort, sort_cached, sort_with_progress};
use everygrep_core::store::{NodeId, NodeStore};
use everygrep_core::sync::{folders_below, resume_scan};
use everygrep_core::watch::Watcher;

use crate::hashes::HashStore;
use crate::schedule::Schedule;

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
    /// The request it answers (as [`Engine::set_query`] returns it). It is
    /// handed to the window later; a newer request may have come by then,
    /// whose own result follows (see [`Engine::is_current`]).
    pub generation: u64,
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
    /// Something the user should know that is not an error of a request.
    pub on_notice: Box<dyn Fn(Notice) + Send + Sync>,
}

/// Numbers about the index, for the Indexing page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub files: u64,
    pub folders: u64,
    /// Sum of the sizes of the files (0 without file details).
    pub file_bytes: u64,
    /// Folders to index, and drives in the index with how many are
    /// connected.
    pub roots: usize,
    pub drives: usize,
    pub drives_online: usize,
    /// Approximate memory of the index.
    pub memory: usize,
    pub index_file: Option<PathBuf>,
    /// Size of the index file on disk, when it exists.
    pub index_file_bytes: Option<u64>,
    /// When the folders were last scanned from scratch while the program
    /// ran (seconds since 1970); `None` when the saved index was used.
    pub last_full_scan: Option<i64>,
    pub unwatched: Unwatched,
    pub file_details: bool,
}

/// A note for the user from the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The saved index could not be used and is built again: damaged or of
    /// another version (`damaged`), or not readable (the error).
    IndexRebuilt { damaged: bool, error: String },
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
    /// Reading files for the `content:` terms of the search.
    SearchingContent,
    /// Reading files to compare their contents for a duplicate search.
    ComparingFiles,
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
    /// A search that was reading file contents: the matches found so far
    /// stay, and file changes do not start it again.
    ContentSearch,
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

    /// Cancels every running drive scan.
    fn cancel_drive_scans(&self) {
        let list = self.list.lock().expect("lock");
        for task in list.iter() {
            if matches!(*task.kind.lock().expect("lock"), TaskKind::Drive(_)) {
                task.progress.cancel();
            }
        }
    }

    /// Cancels the running new index of the folders, if any.
    fn cancel_rescan(&self) {
        let list = self.list.lock().expect("lock");
        for task in list.iter() {
            if *task.kind.lock().expect("lock") == TaskKind::Rescanning {
                task.progress.cancel();
            }
        }
    }

    /// Cancels the running search or sort, if any.
    fn cancel_search(&self) {
        let list = self.list.lock().expect("lock");
        for task in list.iter() {
            let kind = task.kind.lock().expect("lock");
            if matches!(
                *kind,
                TaskKind::Searching
                    | TaskKind::SearchingContent
                    | TaskKind::ComparingFiles
                    | TaskKind::Sorting
            ) {
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
    /// The rules (and whether file details were read) its entries were
    /// read with, saved with them; meaningful only while `root` is set.
    options: ScanOptions,
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
    /// What was read: all of the drive, or with `complete` false the part
    /// read before the scan was cancelled.
    index: Index,
    complete: bool,
    options: ScanOptions,
}

/// Shared engine state.
pub struct Engine {
    pub index: RwLock<Index>,
    roots: Mutex<Vec<PathBuf>>,
    options: Mutex<ScanOptions>,
    /// Set by [`Engine::reindex`]; the index thread scans again when it sees
    /// it.
    rescan: AtomicBool,
    /// Counts the calls of [`Engine::reindex`], under the `roots` lock, so a
    /// cancelled new index goes back to the previous folders only when no
    /// newer request came meanwhile.
    reindexed: AtomicU64,
    /// Whether file changes are watched (see [`Engine::set_watching`]).
    watching: AtomicBool,
    index_file: Mutex<Option<PathBuf>>,
    /// The index file the index was in before [`Engine::move_index_file`],
    /// removed once the index is saved at its new place.
    moved_from: Mutex<Option<PathBuf>>,
    /// Bumped for every new query; older results are dropped.
    generation: AtomicU64,
    query: Mutex<QueryState>,
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
    /// The threads reading drives, so quitting can wait for them to save
    /// what they read (see [`Engine::shutdown`]).
    drive_scans: Mutex<Vec<JoinHandle<()>>>,
    /// Drives whose first reading was cancelled: not read again until they
    /// are connected again (or asked for in the settings).
    not_now: Mutex<HashSet<String>>,
    mounts_changed: Arc<AtomicBool>,
    tasks: Arc<Tasks>,
    /// The folders to index and the rules the index was built with (or,
    /// after a cancelled first index, was to be built with); a cancelled
    /// new index goes back to them.
    indexed_with: Mutex<(Vec<PathBuf>, ScanOptions)>,
    /// The first index was cancelled: the index of the folders is not
    /// complete and must not be saved.
    incomplete: AtomicBool,
    /// The query that gave the results shown; a cancelled search goes back
    /// to it.
    applied: Mutex<QueryState>,
    /// The request whose complete results were shown last, and the index
    /// version they were found in (see [`Engine::settled_search`]).
    answered: Mutex<(u64, Option<u64>)>,
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
    /// Indexed folders the watcher could not watch, for the status line.
    unwatched: Mutex<Unwatched>,
    /// How files are read for `content:` terms.
    content_options: Mutex<ContentOptions>,
    /// Answers of content terms, so a search again (another order, file
    /// changes) reads only what changed.
    content_cache: Mutex<ContentCache>,
    content_state: Mutex<ContentState>,
    /// Hashes of files, for duplicate searches and the hash columns.
    pub hashes: Arc<HashStore>,
    /// A frequency column is shown: count after each sort (see
    /// [`Index::frequencies`]).
    frequency_columns: AtomicBool,
    /// When the folders to index are scanned again by themselves, the time
    /// of the last full scan (or of the start) and of the last look at the
    /// schedule, in seconds since 1970, and the local time zone, read on
    /// the index thread when first needed.
    schedule: Mutex<Schedule>,
    last_full_scan: AtomicI64,
    /// Whether the folders were scanned from scratch since the start.
    scanned_this_session: AtomicBool,
    schedule_checked: AtomicI64,
    zone: std::sync::OnceLock<everygrep_core::localtime::Zone>,
}

/// The reading of file contents for a search.
#[derive(Debug, Default)]
struct ContentState {
    /// Files are being read now (for `content:` terms or a duplicate
    /// search).
    running: bool,
    /// Files changed meanwhile: search again when the reading is done.
    refresh: bool,
    /// The user stopped the last reading, and its matches so far are shown:
    /// file changes do not start it again.
    stopped: bool,
}

/// Indexed folders whose changes are not reported, because their watch
/// could not be added. They are checked again from time to time
/// ([`RECHECK_UNWATCHED_EVERY`]). Folders that cannot be read are not
/// counted: none of their entries are indexed either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unwatched {
    pub folders: usize,
    /// The limit of inotify watches was reached.
    pub limit_reached: bool,
}

/// What the search thread runs: the query text, its options and the result order.
#[derive(Clone, Default, PartialEq)]
struct QueryState {
    text: String,
    options: QueryOptions,
    filter_query: Option<(String, QueryOptions)>,
    sort: Sort,
}

/// A request whose complete results were shown, with the index version and
/// id epoch they were found in (see [`Engine::settled_search`]). A tab keeps
/// it to show its results again without searching while nothing changed.
#[derive(Clone, PartialEq)]
pub struct SettledSearch {
    state: QueryState,
    version: u64,
    epoch: u64,
}

/// A content search the user stopped, with the request whose matches so
/// far are shown (see [`Engine::stopped_search`]). A tab keeps it to show
/// that search stopped again.
#[derive(Clone, PartialEq)]
pub struct StoppedSearch(QueryState);

/// The hash cache's file, next to the index file.
pub const HASH_FILE: &str = "hashes.bin";

const REFRESH_EVERY: Duration = Duration::from_millis(500);
/// While files are read for a content search, the matches so far are shown
/// first after this long, then at most this often.
const PARTIAL_FIRST: Duration = Duration::from_millis(500);
const PARTIAL_EVERY: Duration = Duration::from_secs(1);
/// Most threads reading files for a content search.
const CONTENT_THREADS: usize = 8;
const SAVE_EVERY: Duration = Duration::from_secs(300);
/// How often folders that could not be watched are watched again or, failing
/// that, checked for changes, and how many of them at most each time.
const RECHECK_UNWATCHED_EVERY: Duration = Duration::from_secs(10);
const RECHECK_UNWATCHED_MAX: usize = 5000;
/// How often a watcher is tried again while none could be made (no inotify
/// instance left, as after lost events when another program holds them all).
const RETRY_WATCHER_EVERY: Duration = Duration::from_secs(10);
/// How long quitting waits for cancelled drive scans to save what they
/// read. A scan sees the cancel after each folder, and the save writes to
/// the cache folder, so this is normally well under a second; a drive that
/// hangs in a read must not keep the program from ending.
pub const QUIT_WAIT: Duration = Duration::from_secs(5);

impl Engine {
    /// Creates the engine and starts loading in the background. Entries
    /// that `rules` leave out are not indexed. With `watch`, file changes
    /// are applied as they happen. With `drives`, removable drives found
    /// there are indexed as the settings say.
    pub fn start(
        roots: Vec<PathBuf>,
        options: ScanOptions,
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
        let index_file = index_file.or_else(persist::default_path);
        // Next to the index file, like the saved drives.
        let hash_file = index_file.as_ref().map(|f| f.with_file_name(HASH_FILE));
        let engine = Arc::new(Self {
            index: RwLock::new(Index::new(NodeStore::new())),
            roots: Mutex::new(normalize_roots(&roots)),
            options: Mutex::new(options),
            rescan: AtomicBool::new(false),
            reindexed: AtomicU64::new(0),
            watching: AtomicBool::new(watch),
            index_file: Mutex::new(index_file),
            moved_from: Mutex::new(None),
            generation: AtomicU64::new(0),
            query: Mutex::new(QueryState::default()),
            epoch: AtomicU64::new(0),
            details: Mutex::new(DetailCache::new()),
            details_generation: AtomicU64::new(0),
            applying_changes: AtomicBool::new(false),
            unwatched: Mutex::new(Unwatched::default()),
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
            drive_scans: Mutex::new(Vec::new()),
            not_now: Mutex::new(HashSet::new()),
            mounts_changed: Arc::new(AtomicBool::new(false)),
            tasks: Tasks::new(on_busy),
            indexed_with: Mutex::new((Vec::new(), ScanOptions::default())),
            incomplete: AtomicBool::new(false),
            applied: Mutex::new(QueryState::default()),
            answered: Mutex::new((0, None)),
            content_options: Mutex::new(ContentOptions::default()),
            content_cache: Mutex::new(ContentCache::new()),
            content_state: Mutex::new(ContentState::default()),
            hashes: Arc::new(HashStore::new(hash_file, DEFAULT_MAX_BYTES)),
            frequency_columns: AtomicBool::new(false),
            schedule: Mutex::new(Schedule::Never),
            last_full_scan: AtomicI64::new(unix_now()),
            scanned_this_session: AtomicBool::new(false),
            schedule_checked: AtomicI64::new(unix_now()),
            zone: std::sync::OnceLock::new(),
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

    fn note_full_scan(&self) {
        self.last_full_scan.store(unix_now(), Ordering::SeqCst);
        self.scanned_this_session.store(true, Ordering::SeqCst);
    }

    /// Counts what the index holds. Walks every entry under the read lock
    /// (about a millisecond for 100 000 entries) and reads the size of the
    /// index file, so it is called off the window's thread.
    pub fn stats(&self) -> IndexStats {
        let mut stats = IndexStats {
            index_file: self.index_file(),
            unwatched: self.unwatched(),
            file_details: self.indexed_file_details(),
            last_full_scan: self
                .scanned_this_session
                .load(Ordering::SeqCst)
                .then(|| self.last_full_scan.load(Ordering::SeqCst)),
            ..IndexStats::default()
        };
        {
            let index = self.index.read().expect("index lock");
            let store = index.store();
            for id in (0..store.len()).map(|i| NodeId(i as u32)) {
                if !store.is_live(id) {
                    continue;
                }
                if store.is_dir(id) {
                    stats.folders += 1;
                } else {
                    stats.files += 1;
                    stats.file_bytes = stats.file_bytes.saturating_add(store.meta(id).size);
                }
            }
            stats.memory = index.heap_bytes();
        }
        let drives: Vec<DriveStatus> = self.drives().into_iter().filter(|d| d.indexed).collect();
        stats.drives = drives.len();
        stats.drives_online = drives.iter().filter(|d| d.online).count();
        stats.roots = self.indexed_with.lock().expect("lock").0.len();
        stats.index_file_bytes = self
            .index_file()
            .and_then(|f| std::fs::metadata(f).ok())
            .map(|m| m.len());
        stats
    }

    /// The file the index is saved in; `None` keeps it in memory only.
    pub fn index_file(&self) -> Option<PathBuf> {
        self.index_file.lock().expect("lock").clone()
    }

    /// Saves the index in `to` from now on, with the hash cache next to
    /// it. The index is written there at the next save (call
    /// [`save`](Self::save) for at once); only then is the old file
    /// removed, so a save that fails leaves it in place. Drive indexes stay
    /// in the volumes folder.
    pub fn move_index_file(&self, to: PathBuf) {
        let old = {
            let mut file = self.index_file.lock().expect("lock");
            if file.as_ref() == Some(&to) {
                return;
            }
            file.replace(to.clone())
        };
        if let Some(old) = old {
            // Moved twice before a save: the first place still has it.
            self.moved_from.lock().expect("lock").get_or_insert(old);
        }
        self.hashes.move_to(to.with_file_name(HASH_FILE));
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Scans the folders to index again by themselves on `schedule` (see
    /// [`Schedule::due`]), while file changes are watched.
    pub fn set_schedule(&self, schedule: Schedule) {
        *self.schedule.lock().expect("lock") = schedule;
    }

    /// Asks for a new index of the current folders and rules when the
    /// schedule says one is due. Checked once a second at most, by the
    /// index thread while it watches.
    fn check_schedule(&self) {
        let now = unix_now();
        let checked = self.schedule_checked.load(Ordering::SeqCst);
        if now == checked {
            return;
        }
        self.schedule_checked.store(now, Ordering::SeqCst);
        let schedule = *self.schedule.lock().expect("lock");
        if schedule == Schedule::Never || self.rescan.load(Ordering::SeqCst) {
            return;
        }
        let zone = self
            .zone
            .get_or_init(everygrep_core::localtime::Zone::local);
        let last = self.last_full_scan.load(Ordering::SeqCst);
        if schedule.due(last, checked, now, zone) {
            let _roots = self.roots.lock().expect("lock");
            self.reindexed.fetch_add(1, Ordering::SeqCst);
            self.rescan.store(true, Ordering::SeqCst);
        }
    }

    /// Whether the next new index reads file sizes and modification times
    /// (see [`ScanOptions::with_metadata`]); [`reindex`](Self::reindex)
    /// builds it. A saved index read the other way is not loaded.
    pub fn set_file_details(&self, on: bool) {
        self.options.lock().expect("lock").with_metadata = on;
    }

    /// Whether the index asked for last reads file sizes and times: after a
    /// cancelled new index, whether the index that was kept does.
    pub fn file_details(&self) -> bool {
        self.options.lock().expect("lock").with_metadata
    }

    /// Whether the index in memory holds file sizes and times; differs from
    /// [`file_details`](Self::file_details) while a new index is built.
    pub fn indexed_file_details(&self) -> bool {
        self.indexed_with.lock().expect("lock").1.with_metadata
    }

    /// Indexes `roots` again from scratch with `rules`, in the background.
    /// Empty `roots` stand for the home folder. The status hook reports the
    /// scan as for a first start; the new index is saved as soon as it is
    /// complete. A new index of an older request that is still running is
    /// cancelled: it would be replaced at once. That is not reported as a
    /// stop, and the folders and rules stay the newest ones.
    pub fn reindex(&self, roots: &[PathBuf], rules: IndexRules) {
        let mut current = self.roots.lock().expect("lock");
        *current = normalize_roots(roots);
        self.options.lock().expect("lock").rules = rules;
        self.reindexed.fetch_add(1, Ordering::SeqCst);
        self.rescan.store(true, Ordering::SeqCst);
        drop(current);
        self.tasks.cancel_rescan();
    }

    /// The folders to index and the rules asked for last: after a cancelled
    /// new index (see [`Stopped::Rescan`]) the ones of the index that was
    /// kept.
    pub fn setup(&self) -> (Vec<PathBuf>, RuleSpec) {
        let roots = self.roots.lock().expect("lock");
        let rules = self.options.lock().expect("lock").rules.spec();
        (roots.clone(), rules)
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
    #[cfg(test)]
    pub fn set_query(&self, text: &str, options: QueryOptions) -> u64 {
        self.set_filtered_query(text, options, None)
    }

    /// Runs the field and a selected named filter as independent expressions.
    pub fn set_filtered_query(
        &self,
        text: &str,
        options: QueryOptions,
        filter: Option<(String, QueryOptions)>,
    ) -> u64 {
        let mut q = self.query.lock().expect("lock");
        q.filter_query = filter;
        q.text = text.to_owned();
        q.options = options;
        self.content_state.lock().expect("lock").stopped = false;
        // Under the lock, so a cancelled search never undoes this request.
        self.request_search()
    }

    /// The content search the user stopped, when the results shown are its
    /// matches so far for the request asked last.
    pub fn stopped_search(&self) -> Option<StoppedSearch> {
        let q = self.query.lock().expect("lock");
        let stopped = self.content_state.lock().expect("lock").stopped;
        let applied = self.applied.lock().expect("lock");
        (stopped && *applied == *q).then(|| StoppedSearch(q.clone()))
    }

    /// The request asked last, when its complete results are shown and no
    /// newer request, file reading or stop came after them.
    pub fn settled_search(&self) -> Option<SettledSearch> {
        let q = self.query.lock().expect("lock");
        let content = self.content_state.lock().expect("lock");
        if content.running || content.stopped {
            return None;
        }
        drop(content);
        let (generation, version) = *self.answered.lock().expect("lock");
        if generation != self.generation.load(Ordering::SeqCst)
            || *self.applied.lock().expect("lock") != *q
        {
            return None;
        }
        Some(SettledSearch {
            state: q.clone(),
            version: version?,
            epoch: self.epoch(),
        })
    }

    /// Makes `settled` the current request again without searching, when
    /// the query, its filter and the order asked for are the ones it had and
    /// the index has not changed since (same ids, same version): its results
    /// (kept by the caller) are still the answer. A search or sort running
    /// for another request is cancelled. Never waits for the index: while
    /// it is being changed, `None` (search again), as when anything differs.
    pub fn show_settled(
        &self,
        settled: &SettledSearch,
        text: &str,
        options: QueryOptions,
        filter: Option<(String, QueryOptions)>,
        sort: Sort,
    ) -> Option<u64> {
        let state = QueryState {
            text: text.to_owned(),
            options,
            filter_query: filter,
            sort,
        };
        if state != settled.state || self.epoch() != settled.epoch {
            return None;
        }
        let index = self.index.try_read().ok()?;
        if index.version() != settled.version || self.epoch() != settled.epoch {
            return None;
        }
        drop(index);
        let mut q = self.query.lock().expect("lock");
        *q = state.clone();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.tasks.cancel_search();
        let mut content = self.content_state.lock().expect("lock");
        content.stopped = false;
        content.refresh = false;
        drop(content);
        *self.applied.lock().expect("lock") = state;
        *self.answered.lock().expect("lock") = (generation, Some(settled.version));
        Some(generation)
    }

    /// Makes `stopped` the current request again, still stopped, when the
    /// query, its filter and the order asked for are the ones it had: its
    /// matches so far (kept by the caller) stay shown and no file is read
    /// again, as after the stop. A search or sort running for another
    /// request is cancelled. Returns the generation of the request, `None`
    /// (nothing changed) when the request differs.
    pub fn show_stopped(
        &self,
        stopped: &StoppedSearch,
        text: &str,
        options: QueryOptions,
        filter: Option<(String, QueryOptions)>,
        sort: Sort,
    ) -> Option<u64> {
        let state = QueryState {
            text: text.to_owned(),
            options,
            filter_query: filter,
            sort,
        };
        if state != stopped.0 {
            return None;
        }
        let mut q = self.query.lock().expect("lock");
        *q = state.clone();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.tasks.cancel_search();
        let mut content = self.content_state.lock().expect("lock");
        content.stopped = true;
        content.refresh = false;
        drop(content);
        *self.applied.lock().expect("lock") = state;
        Some(generation)
    }

    /// Changes the result order and runs the current query again. The order
    /// stays for later queries. Returns the generation of the request.
    pub fn set_sort(&self, sort: Sort) -> u64 {
        let mut q = self.query.lock().expect("lock");
        q.sort = sort;
        self.content_state.lock().expect("lock").stopped = false;
        self.request_search()
    }

    /// Runs the current query again, reading the details a sort needs
    /// (creation times, attributes) afresh. Returns the generation of the
    /// request.
    pub fn refresh(&self) -> u64 {
        let _query = self.query.lock().expect("lock");
        self.content_state.lock().expect("lock").stopped = false;
        self.forget_details();
        self.request_search()
    }

    /// The result order asked for last: after a cancelled sort the order of
    /// the results shown, unless a newer order was asked for meanwhile.
    pub fn asked_sort(&self) -> Sort {
        self.query.lock().expect("lock").sort
    }

    /// The current id epoch. Read it under an index lock to pair it with ids.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Whether `generation` is still the newest request.
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    /// How files are read for `content:` terms from now on. A change runs
    /// the current query again, with every file read again.
    pub fn set_content_options(&self, options: ContentOptions) {
        let mut current = self.content_options.lock().expect("lock");
        if *current == options {
            return;
        }
        *current = options;
        drop(current);
        let _query = self.query.lock().expect("lock");
        self.content_state.lock().expect("lock").stopped = false;
        self.request_search();
    }

    /// The query runs again because files changed. While files are read
    /// for a content search, it runs again only when that is done (the
    /// answers are cached, so only changed files are read again); after the
    /// user stopped one, file changes leave its matches alone.
    fn request_refresh(&self) {
        let mut state = self.content_state.lock().expect("lock");
        if state.running {
            state.refresh = true;
        } else if !state.stopped {
            drop(state);
            self.request_search();
        }
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
            let mut version = None;
            let (ids, error) = match Query::parse(&state.text, state.options).and_then(|query| {
                match &state.filter_query {
                    Some((text, options)) => {
                        Query::parse(text, *options).and_then(|filter| query.and(filter))
                    }
                    None => Ok(query),
                }
            }) {
                Ok(query) if !query.content_terms().is_empty() || query.dupes().is_some() => {
                    terms = query.highlight_terms();
                    let terms = Arc::new(std::mem::take(&mut terms));
                    let found = self.search_contents(generation, &state, &query, started, &terms);
                    match found {
                        Ok(Some((ids, at, searched))) => {
                            epoch = at;
                            let mut q = self.content_state.lock().expect("lock");
                            q.stopped = false;
                            drop(q);
                            if self.is_current(generation) {
                                *self.applied.lock().expect("lock") = state.clone();
                                *self.answered.lock().expect("lock") = (generation, Some(searched));
                                (self.hooks.on_results)(SearchResult {
                                    ids: Arc::new(ids),
                                    terms,
                                    took: started.elapsed(),
                                    error: None,
                                    query: state.text,
                                    epoch,
                                    generation,
                                });
                            }
                            self.refresh_after_contents();
                            continue;
                        }
                        Ok(None) => break,
                        Err(Stopped::ContentSearch) => {
                            self.content_stopped(generation, &state);
                            continue;
                        }
                        Err(_) => {
                            // Stopped by the user or for a newer request:
                            // changes seen meanwhile need no search of
                            // their own.
                            self.content_state.lock().expect("lock").refresh = false;
                            self.search_cancelled(generation, &state);
                            continue;
                        }
                    }
                }
                Ok(query) => {
                    terms = query.highlight_terms();
                    let index = self.index.read().expect("index lock");
                    // Quitting began while the index was held.
                    if self.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    epoch = self.epoch();
                    version = Some(index.version());
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
                *self.answered.lock().expect("lock") = (generation, version);
                (self.hooks.on_results)(SearchResult {
                    ids: Arc::new(ids),
                    terms: Arc::new(terms),
                    took: started.elapsed(),
                    error,
                    query: state.text,
                    epoch,
                    generation,
                });
            }
        }
    }

    /// Runs a query with `content:` terms: the index part under a read lock,
    /// then the files without it, showing the matches so far now and then.
    /// Returns the sorted results, the epoch of their ids and the version of
    /// the index they were found in, `None` when
    /// quitting began, or why it stopped: [`Stopped::ContentSearch`] when
    /// the user cancelled it after matches were shown, [`Stopped::Search`]
    /// for any other cancel (or when the ids changed meaning meanwhile; it
    /// is asked for again then).
    fn search_contents(
        &self,
        generation: u64,
        state: &QueryState,
        query: &Query,
        started: Instant,
        terms: &Arc<Vec<HighlightTerm>>,
    ) -> Result<Option<(Vec<NodeId>, u64, u64)>, Stopped> {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let task = self.tasks.begin(TaskKind::Searching, true);
        if !self.is_current(generation) {
            task.progress().cancel();
        }
        let (staged, epoch, version) = {
            let index = self.index.read().expect("index lock");
            if self.stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let epoch = self.epoch();
            let staged = search_staged(&index, query, threads, task.progress())
                .map_err(|_| Stopped::Search)?;
            (staged, epoch, index.version())
        };
        let decisions = Decisions::new(staged.pending.len());
        let mut shown = false;
        if !staged.pending.is_empty() {
            task.set_kind(TaskKind::SearchingContent);
            task.progress().restart(Some(staged.pending.len() as u64));
            self.content_state.lock().expect("lock").running = true;
            let options = *self.content_options.lock().expect("lock");
            let reading = Instant::now();
            let mut last: Option<(Instant, usize)> = None;
            let read = decide_all(
                &staged.pending,
                query.content_terms(),
                &options,
                Some(&self.content_cache),
                threads.min(CONTENT_THREADS),
                task.progress(),
                &decisions,
                || {
                    let now = Instant::now();
                    let due = match last {
                        None => now.duration_since(reading) >= PARTIAL_FIRST,
                        Some((at, matched)) => {
                            now.duration_since(at) >= PARTIAL_EVERY
                                && decisions.matched() != matched
                        }
                    };
                    // Duplicates are known only at the end.
                    if due && query.dupes().is_none() {
                        last = Some((now, decisions.matched()));
                        shown |= self.show_partial(
                            generation, state, query, &staged, &decisions, epoch, started, terms,
                        );
                    }
                },
            );
            self.content_state.lock().expect("lock").running = false;
            read.map_err(|_| self.why_stopped(generation, shown))?;
        }
        let Some(mut index) = self.read_again(epoch)? else {
            return Ok(None);
        };
        let mut ids = staged.results(&decisions);
        let dupes = match query.dupes() {
            None => None,
            Some(spec) => {
                let grouped = dupe::group(&index, &ids, spec);
                if grouped.candidates.is_empty() {
                    Some(grouped.dupes)
                } else {
                    // The files are compared without the index, like
                    // contents are read.
                    drop(index);
                    task.set_kind(TaskKind::ComparingFiles);
                    self.content_state.lock().expect("lock").running = true;
                    let compared = dupe::compare_contents(
                        grouped,
                        Some(self.hashes.cache()),
                        threads.min(CONTENT_THREADS),
                        task.progress(),
                    );
                    self.content_state.lock().expect("lock").running = false;
                    let dupes = compared.map_err(|_| Stopped::Search)?;
                    let Some(again) = self.read_again(epoch)? else {
                        return Ok(None);
                    };
                    index = again;
                    Some(dupes)
                }
            }
        };
        if let Some(dupes) = &dupes {
            ids = dupes.to_sort();
        }
        if self.is_current(generation) {
            task.set_kind(TaskKind::Sorting);
            task.progress().restart(None);
            self.sort(&index, &mut ids, state.sort, threads, task.progress())
                .map_err(|_| self.why_stopped(generation, shown))?;
            if let Some(dupes) = &dupes {
                ids = dupes.arrange(&ids);
            }
            query.apply_limit(&mut ids);
        }
        Ok(Some((ids, epoch, version)))
    }

    /// The index again after files were read without it: `None` when
    /// quitting began; when ids changed meaning meanwhile, the search is
    /// asked for again (the answers and hashes are cached) and this one
    /// stops.
    fn read_again(
        &self,
        epoch: u64,
    ) -> Result<Option<std::sync::RwLockReadGuard<'_, Index>>, Stopped> {
        let index = self.index.read().expect("index lock");
        if self.stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if self.epoch() != epoch {
            drop(index);
            self.request_search();
            return Err(Stopped::Search);
        }
        Ok(Some(index))
    }

    /// Why a content search for `generation` was cancelled: the user stopped
    /// it after matches were `shown`, or anything else.
    fn why_stopped(&self, generation: u64, shown: bool) -> Stopped {
        if shown && self.is_current(generation) && !self.stop.load(Ordering::SeqCst) {
            Stopped::ContentSearch
        } else {
            Stopped::Search
        }
    }

    /// Hands the matches found so far to the window, sorted, unless the ids
    /// changed meaning or a newer request came. Returns whether it did.
    #[allow(clippy::too_many_arguments)]
    fn show_partial(
        &self,
        generation: u64,
        state: &QueryState,
        query: &Query,
        staged: &Staged,
        decisions: &Decisions,
        epoch: u64,
        started: Instant,
        terms: &Arc<Vec<HighlightTerm>>,
    ) -> bool {
        let index = self.index.read().expect("index lock");
        if self.epoch() != epoch || !self.is_current(generation) {
            return false;
        }
        let mut ids = staged.results(decisions);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        if self
            .sort(&index, &mut ids, state.sort, threads, &Progress::new())
            .is_err()
        {
            return false;
        }
        query.apply_limit(&mut ids);
        drop(index);
        *self.applied.lock().expect("lock") = state.clone();
        (self.hooks.on_results)(SearchResult {
            ids: Arc::new(ids),
            terms: Arc::clone(terms),
            took: started.elapsed(),
            error: None,
            query: state.text.clone(),
            epoch,
            generation,
        });
        true
    }

    /// Files changed while the contents were read: search again now.
    fn refresh_after_contents(&self) {
        let mut state = self.content_state.lock().expect("lock");
        if std::mem::take(&mut state.refresh) {
            drop(state);
            self.request_search();
        }
    }

    /// The user cancelled a content search after its first matches were
    /// shown: they stay, with the query that found them.
    fn content_stopped(&self, generation: u64, state: &QueryState) {
        let q = self.query.lock().expect("lock");
        if !self.is_current(generation) {
            return;
        }
        let mut content = self.content_state.lock().expect("lock");
        content.stopped = true;
        content.refresh = false;
        drop(content);
        *self.applied.lock().expect("lock") = state.clone();
        drop(q);
        (self.hooks.on_stopped)(Stopped::ContentSearch);
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
        if self.frequency_columns.load(Ordering::SeqCst) {
            // The rows read the counts built last; build them while the
            // index is held anyway.
            index.frequencies();
        }
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

    /// Whether a frequency column is shown, so each search counts the
    /// index (when it changed) before its results are shown.
    pub fn set_frequency_columns(&self, on: bool) {
        self.frequency_columns.store(on, Ordering::SeqCst);
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

    /// Indexed folders that are not watched, for the status line.
    pub fn unwatched(&self) -> Unwatched {
        *self.unwatched.lock().expect("lock")
    }

    /// Records the folders `watcher` could not watch (none without one).
    /// Returns whether that changed.
    fn note_unwatched(&self, watcher: Option<&Watcher>) -> bool {
        let mut now = Unwatched::default();
        for kind in watcher.into_iter().flat_map(Watcher::failed_watch_kinds) {
            if kind != std::io::ErrorKind::PermissionDenied {
                now.folders += 1;
                now.limit_reached |= kind == std::io::ErrorKind::StorageFull;
            }
        }
        let mut unwatched = self.unwatched.lock().expect("lock");
        let changed = *unwatched != now;
        *unwatched = now;
        changed
    }

    fn index_loop(&self) {
        self.report(IndexState::Loading);
        let mut loaded = false;
        let saved = self.index_file().and_then(|path| {
            persist::load(&path)
                .map_err(|e| {
                    // A missing file is the first start; anything else is
                    // said, since the index is then built again.
                    if e.kind() != std::io::ErrorKind::NotFound {
                        (self.hooks.on_notice)(Notice::IndexRebuilt {
                            damaged: e.kind() == std::io::ErrorKind::InvalidData,
                            error: e.to_string(),
                        });
                    }
                })
                .ok()
        });
        if let Some(saved) = saved {
            let mut roots = self.roots.lock().expect("lock");
            if roots.is_empty() || *roots == saved.roots {
                *roots = saved.roots;
                let options = self.scan_options();
                // An index built with other rules or file details, or
                // saved without them, is built again over its folders.
                if saved.rules_hash == Some(fingerprint(&options)) {
                    *self.indexed_with.lock().expect("lock") = (roots.clone(), options);
                    self.replace_index(saved.index);
                    if saved.reordered {
                        // Store the new order, so it is not sorted again.
                        self.dirty.store(true, Ordering::SeqCst);
                    }
                    loaded = true;
                }
            }
        }
        if !loaded && self.scan_fresh(TaskKind::Indexing).is_err() {
            // Nothing is saved until a new index is complete; the next
            // start indexes again.
            self.incomplete.store(true, Ordering::SeqCst);
            *self.indexed_with.lock().expect("lock") = (self.roots(), self.scan_options());
            (self.hooks.on_stopped)(Stopped::Index);
        }
        self.load_drives();
        let (_, mut drives_to_catch_up) = self.sync_drives(None);
        self.request_search();

        // Changes during any scan precede watch installation too.
        let mut catch_up = true;
        while !self.stop.load(Ordering::SeqCst) {
            if self.rescan.load(Ordering::SeqCst) {
                // A new index is due: nothing to watch until it is built.
            } else if !self.watching.load(Ordering::SeqCst) {
                self.wait_unwatched();
                // Turned on again: what changed meanwhile is caught up.
                catch_up = true;
            } else {
                catch_up = self.watch_once(catch_up, &mut drives_to_catch_up);
            }
            if self.rescan.swap(false, Ordering::SeqCst) && !self.stop.load(Ordering::SeqCst) {
                // Nothing watched the index during the scan: it is caught
                // up either way.
                catch_up = true;
                match self.scan_fresh(TaskKind::Rescanning) {
                    Ok(()) => {
                        self.request_search();
                        self.save();
                    }
                    Err(request) => {
                        // The previous index stays, with its folders, unless
                        // a newer request came: that one is built next.
                        let mut roots = self.roots.lock().expect("lock");
                        if self.reindexed.load(Ordering::SeqCst) == request {
                            let (indexed, options) =
                                self.indexed_with.lock().expect("lock").clone();
                            *roots = indexed;
                            *self.options.lock().expect("lock") = options;
                            drop(roots);
                            (self.hooks.on_stopped)(Stopped::Rescan);
                        }
                    }
                }
            }
        }
    }

    /// Whether the index thread should leave its current loop: to stop, to
    /// scan again, or because watching was turned on or off.
    fn interrupted(&self, watching: bool) -> bool {
        if watching {
            self.check_schedule();
        }
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
    /// (or with the roots of drives that came online). Without a watcher it
    /// waits for one (see [`Engine::wait_for_watcher`]). The watcher and all
    /// its watches are dropped on return. Returns whether a catch-up is
    /// still due.
    fn watch_once(&self, catch_up: bool, drives_to_catch_up: &mut Vec<NodeId>) -> bool {
        let watcher = self.new_watcher(
            &self.index.read().expect("index lock"),
            &self.scan_options(),
        );
        let (mut watcher, catch_up) = match watcher {
            Ok(watcher) => (watcher, catch_up),
            // Nothing watched the index meanwhile: once a watcher is made,
            // it is caught up in full. Interrupted before that, it is not.
            Err(_) => match self.wait_for_watcher() {
                Some(watcher) => (watcher, true),
                None => return true,
            },
        };
        if catch_up || !drives_to_catch_up.is_empty() {
            self.report(IndexState::CatchingUp);
            let task = self.tasks.begin(TaskKind::Updating, false);
            let roots = if catch_up {
                online_roots(&self.index.read().expect("index lock"))
            } else {
                std::mem::take(drives_to_catch_up)
            };
            task.progress().set_total(Some(folders_below(
                &self.index.read().expect("index lock"),
                &roots,
            )));
            // The index is locked for short batches, so the window can draw
            // rows and quitting can save in between.
            let stats = watcher.catch_up_shared(&self.index, &roots, task.progress(), &|| {
                self.interrupted(true)
            });
            drop(task);
            if stats.added + stats.removed + stats.updated > 0 {
                self.dirty.store(true, Ordering::SeqCst);
                self.request_search();
            }
            if self.interrupted(true) {
                // Cut short: what was not checked is caught up next time.
                return true;
            }
        }
        drives_to_catch_up.clear();
        self.note_unwatched(Some(&watcher));
        self.report(IndexState::Watching);
        {
            // A catch-up finds new and removed entries, not changed
            // attributes, so details read before this session (or
            // while nothing was watched) are not trusted.
            let _index = self.index.write().expect("index lock");
            self.forget_details();
            self.applying_changes.store(true, Ordering::SeqCst);
        }
        // Without a watcher for an index built again, nothing
        // watched it since: a catch-up is due.
        let lost = !self.watch_loop(&mut watcher);
        self.applying_changes.store(false, Ordering::SeqCst);
        self.note_unwatched(None);
        lost
    }

    /// Without a watcher (no inotify instance could be made, for example
    /// after lost events while other programs hold every instance), the index
    /// is shown as not watched and stays as it is, while drives still come
    /// and go. A watcher is tried again every [`RETRY_WATCHER_EVERY`].
    /// Returns it, or nothing when the index thread is interrupted first.
    fn wait_for_watcher(&self) -> Option<Watcher> {
        self.report(IndexState::Static);
        let mut last_try = Instant::now();
        while !self.interrupted(true) {
            std::thread::sleep(Duration::from_millis(250));
            if self.mounts_changed.swap(false, Ordering::SeqCst) && self.sync_drives(None).0 {
                self.dirty.store(true, Ordering::SeqCst);
                self.request_search();
                self.report(IndexState::Static);
            }
            if last_try.elapsed() >= RETRY_WATCHER_EVERY {
                last_try = Instant::now();
                let watcher = self.new_watcher(
                    &self.index.read().expect("index lock"),
                    &self.scan_options(),
                );
                if let Ok(watcher) = watcher {
                    return Some(watcher);
                }
            }
        }
        None
    }

    /// A watcher of `index`. In tests it can be made to fail, as when no
    /// more inotify instances are allowed.
    fn new_watcher(&self, index: &Index, options: &ScanOptions) -> std::io::Result<Watcher> {
        #[cfg(test)]
        if tests::listed(&tests::FAILING_WATCHERS, self.index_file().as_deref()) {
            return Err(std::io::Error::other("no watcher in this test"));
        }
        Watcher::new(index, options)
    }

    /// Builds the index from scratch over the current roots (the home
    /// folder when there are none) and swaps it in. When the task (shown as
    /// `kind`) is cancelled, the index stays as it was and the error holds
    /// the [`Engine::reindex`] count the scan was for.
    fn scan_fresh(&self, kind: TaskKind) -> Result<(), u64> {
        self.report(IndexState::Scanning);
        let (roots, options, request) = {
            let mut roots = self.roots.lock().expect("lock");
            if roots.is_empty() {
                *roots = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .into_iter()
                    .collect();
            }
            let request = self.reindexed.load(Ordering::SeqCst);
            (roots.clone(), self.scan_options(), request)
        };
        let task = self.tasks.begin(kind, true);
        task.progress().set_total(self.estimate(&roots, &options));
        let mut store = NodeStore::new();
        if scan_with_progress(&mut store, &roots, &options, task.progress()).is_err() {
            return Err(request);
        }
        let (fresh, left_out) = self.with_drives(Index::new(store), &roots, &options);
        if task.progress().is_cancelled() {
            return Err(request);
        }
        self.replace_index(fresh);
        self.read_drives_again(&left_out);
        self.note_full_scan();
        *self.indexed_with.lock().expect("lock") = (roots, options);
        self.incomplete.store(false, Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
        Ok(())
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

    /// Applies file changes until the engine stops or a rescan is asked for
    /// (then returns true), or until the index is built again after lost
    /// events and no watcher could be made for it (then returns false: the
    /// old one must not be used on the new index).
    fn watch_loop(&self, watcher: &mut Watcher) -> bool {
        let mut last_refresh = Instant::now();
        let mut last_save = Instant::now();
        let mut last_recheck = Instant::now();
        let mut pending_refresh = false;
        while !self.interrupted(true) {
            if self.mounts_changed.swap(false, Ordering::SeqCst) {
                let (changed, drives) = self.sync_drives(Some(watcher));
                // Drives that came back (or were read) are watched already;
                // what changed on them meanwhile is caught up here, outside
                // the drive lock and a batch of folders at a time.
                let stats = if drives.is_empty() {
                    Default::default()
                } else {
                    watcher.catch_up_shared(&self.index, &drives, &Progress::new(), &|| {
                        self.interrupted(true)
                    })
                };
                if changed || stats.added + stats.removed + stats.updated > 0 {
                    self.dirty.store(true, Ordering::SeqCst);
                    pending_refresh = true;
                }
            }
            match watcher.wait(Some(Duration::from_millis(250))) {
                Ok(events) if !events.is_empty() || watcher.has_pending() => {
                    let mut index = self.index.write().expect("index lock");
                    #[allow(unused_mut)]
                    let mut stats = watcher.apply(&mut index, &events);
                    #[cfg(test)]
                    if tests::take(&tests::OVERFLOWS, self.index_file().as_deref()) {
                        stats.overflow = true;
                    }
                    if index.should_compact() {
                        // Ids held outside the index are dropped with the
                        // epoch: results of the old one are searched again.
                        watcher.compact(&mut index);
                        self.bump_epoch();
                        pending_refresh = true;
                    }
                    if stats.overflow {
                        // Events were lost: details may have changed unseen.
                        self.forget_details();
                        drop(index);
                        if !self.rescan_after_overflow(watcher) {
                            self.request_search();
                            return false;
                        }
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
            if self.reread_unmounted(watcher) {
                self.dirty.store(true, Ordering::SeqCst);
                pending_refresh = true;
            }
            if watcher.failed_watches() > 0 && last_recheck.elapsed() >= RECHECK_UNWATCHED_EVERY {
                last_recheck = Instant::now();
                let stats = watcher.recheck_unwatched(
                    &mut self.index.write().expect("index lock"),
                    RECHECK_UNWATCHED_MAX,
                );
                if stats.added + stats.removed + stats.updated > 0 {
                    self.dirty.store(true, Ordering::SeqCst);
                    pending_refresh = true;
                }
            }
            if self.note_unwatched(Some(watcher)) {
                self.report(IndexState::Watching);
            }
            if pending_refresh && last_refresh.elapsed() >= REFRESH_EVERY {
                pending_refresh = false;
                last_refresh = Instant::now();
                self.request_refresh();
                self.report(IndexState::Watching);
            }
            if self.dirty.load(Ordering::SeqCst) && last_save.elapsed() >= SAVE_EVERY {
                self.save();
                last_save = Instant::now();
            }
        }
        true
    }

    /// Reads again the folders whose file system was unmounted (see
    /// [`Watcher::take_unmounted`]), except on a removable drive: that one goes
    /// offline with its entries when the drives are synced. Returns whether
    /// the index changed.
    fn reread_unmounted(&self, watcher: &mut Watcher) -> bool {
        let unmounted = watcher.take_unmounted();
        if unmounted.is_empty() {
            return false;
        }
        let drives: Vec<PathBuf> = self
            .volumes
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|v| v.root.clone())
            .collect();
        let mut index = self.index.write().expect("index lock");
        let mut changed = false;
        for dir in unmounted {
            let store = index.store();
            if !store.is_live(dir) {
                continue;
            }
            let root = std::iter::successors(Some(dir), |&d| store.parent(d))
                .last()
                .unwrap_or(dir);
            if drives
                .iter()
                .any(|d| d.as_os_str().as_bytes() == store.name(root))
            {
                continue;
            }
            let stats = watcher.reread(&mut index, dir);
            changed |= stats.added + stats.removed + stats.updated > 0;
        }
        changed
    }

    /// Builds the index again after lost events, with a new `watcher` for
    /// it. Returns false when no watcher could be made: the index was
    /// replaced, and the old watcher's folders are ids of the old one.
    /// Cancelled, the old index stays with its watcher and is caught up.
    fn rescan_after_overflow(&self, watcher: &mut Watcher) -> bool {
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
            let roots = online_roots(&self.index.read().expect("index lock"));
            watcher.catch_up_shared(&self.index, &roots, &Progress::new(), &|| {
                self.interrupted(true)
            });
            self.report(IndexState::Watching);
            // A request to index again cancels it too; that one runs next.
            if !self.rescan.load(Ordering::SeqCst) {
                (self.hooks.on_stopped)(Stopped::Rescan);
            }
            return true;
        }
        drop(task);
        let (fresh, left_out) = self.with_drives(Index::new(store), &roots, &options);
        let mut index = self.index.write().expect("index lock");
        *index = fresh;
        self.bump_epoch();
        let watched = match self.new_watcher(&index, &options) {
            Ok(mut fresh) => {
                fresh.catch_up(&mut index);
                *watcher = fresh;
                true
            }
            Err(_) => false,
        };
        drop(index);
        self.read_drives_again(&left_out);
        self.note_full_scan();
        // Saves record what this index was built with.
        *self.indexed_with.lock().expect("lock") = (roots, options);
        watched
    }

    /// Saves the index if it changed since the last save: the folders to
    /// index to the index file, each connected drive to its own file.
    /// Drives that are not connected were saved when they went away.
    pub fn save(&self) {
        self.save_by(None);
    }

    /// [`save`](Self::save), giving up at `deadline` when the drives or the
    /// index stay locked until then (a catch-up holds the index while it
    /// reads folders); the index then stays to be saved.
    fn save_by(&self, deadline: Option<Instant>) {
        // A cache: a failed save is tried again with the next one.
        let _ = self.hashes.save();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        // The folders and rules the index in memory was built with, which
        // differ from the asked-for ones while a new index is being built.
        let (roots, options) = self.indexed_with.lock().expect("lock").clone();
        let fingerprint = fingerprint(&options);
        let locked = match deadline {
            None => Some((
                self.volumes.lock().expect("lock"),
                self.index.read().expect("index lock"),
            )),
            Some(deadline) => {
                retry_until(deadline, || self.volumes.try_lock().ok()).and_then(|volumes| {
                    Some((
                        volumes,
                        retry_until(deadline, || self.index.try_read().ok())?,
                    ))
                })
            }
        };
        let Some((mut volumes, index)) = locked else {
            self.dirty.store(true, Ordering::SeqCst);
            return;
        };
        let drive_roots: Vec<NodeId> = volumes
            .iter()
            .filter_map(|v| root_id(&index, v.root.as_deref()?))
            .collect();
        let mut ok = true;
        // An index cancelled before it was complete is never saved.
        let own_file = self
            .index_file()
            .filter(|_| !self.incomplete.load(Ordering::SeqCst));
        if let Some(path) = &own_file {
            ok = if drive_roots.is_empty() {
                persist::save_with_fingerprint(&index, &roots, fingerprint, path).is_ok()
            } else {
                let own: Vec<NodeId> = index
                    .roots()
                    .into_iter()
                    .filter(|r| !drive_roots.contains(r))
                    .collect();
                persist::save_with_fingerprint(&index.extract(&own).0, &roots, fingerprint, path)
                    .is_ok()
            };
            if ok {
                // Moved: the old file goes once the new one is written.
                let mut moved_from = self.moved_from.lock().expect("lock");
                if let Some(old) = moved_from.take().filter(|old| old != path) {
                    let _ = std::fs::remove_file(old);
                }
            }
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
    /// folders `roots` with `rules`. Drives read with other rules, or that
    /// lie in or hold one of `roots` (their entries would be listed twice),
    /// are left out; their UUIDs are returned, for
    /// [`read_drives_again`](Self::read_drives_again) once `fresh` is used.
    fn with_drives(
        &self,
        mut fresh: Index,
        roots: &[PathBuf],
        options: &ScanOptions,
    ) -> (Index, Vec<String>) {
        let wanted = fingerprint(options);
        let volumes = self.volumes.lock().expect("lock");
        let old = self.index.read().expect("index lock");
        let mut left_out = Vec::new();
        for v in volumes.iter() {
            let Some(name) = v.root.as_deref() else {
                continue;
            };
            if fingerprint(&v.options) != wanted || overlaps(roots, name) {
                left_out.push(v.info.uuid.clone());
                continue;
            }
            if let (Some(root), None) = (root_id(&old, name), root_id(&fresh, name)) {
                fresh.graft(&old.extract(&[root]).0);
            }
        }
        (fresh, left_out)
    }

    /// The drives `uuids` were left out of the new index (see
    /// [`with_drives`](Self::with_drives)): they are no longer indexed, and
    /// are read again with the new rules when they are connected, at once
    /// for those that are, unless a folder to index holds them.
    fn read_drives_again(&self, uuids: &[String]) {
        if uuids.is_empty() {
            return;
        }
        for v in self
            .volumes
            .lock()
            .expect("lock")
            .iter_mut()
            .filter(|v| uuids.contains(&v.info.uuid))
        {
            v.root = None;
            v.info.entries = 0;
        }
        self.mounts_changed.store(true, Ordering::SeqCst);
    }

    /// Loads the saved index of every known drive into the index, marked
    /// offline until the drive is found mounted.
    fn load_drives(&self) {
        let Some(env) = &self.drive_env else { return };
        let ignored = self.drive_settings.lock().expect("lock").ignored.clone();
        let configured = self.roots();
        let options = self.scan_options();
        let wanted = Some(fingerprint(&options));
        let mut volumes = self.volumes.lock().expect("lock");
        for mut info in drives::known_drives(env) {
            let mut root = None;
            if !ignored.contains(&info.uuid) {
                // A drive read with other rules (or saved without them) is
                // read again when it is connected.
                if let Some(saved) = persist::load(&env.index_path(&info.uuid))
                    .ok()
                    .filter(|saved| saved.rules_hash == wanted)
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
                options: options.clone(),
            });
        }
    }

    /// Brings the drives in the index in line with the mount table, the
    /// settings and the window's requests. With a watcher, drives that come
    /// online are watched at once. Their roots are returned for a catch-up,
    /// which the caller runs outside the drive lock: at once with a watcher,
    /// otherwise when watching starts. Returns whether the index or the list
    /// of drives changed.
    fn sync_drives(&self, mut watcher: Option<&mut Watcher>) -> (bool, Vec<NodeId>) {
        let Some(env) = &self.drive_env else {
            return (false, Vec::new());
        };
        let settings = self.drive_settings.lock().expect("lock").clone();
        let commands = std::mem::take(&mut *self.drive_commands.lock().expect("lock"));
        let mut volumes = self.volumes.lock().expect("lock");
        // Quitting: drives read meanwhile are kept by `shutdown`, and the
        // index is not saved again.
        if self.stop.load(Ordering::SeqCst) {
            return (false, Vec::new());
        }
        let scanned = std::mem::take(&mut *self.scanned.lock().expect("lock"));
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
            complete,
            options,
        } in scanned
        {
            self.scanning.lock().expect("lock").remove(&drive.uuid);
            if !complete {
                // The scan thread kept what it read (see `start_scan`).
                self.not_now
                    .lock()
                    .expect("lock")
                    .insert(drive.uuid.clone());
                (self.hooks.on_stopped)(Stopped::Drive(drive_name(&drive)));
                changed = true;
                continue;
            }
            let Some(v) = volumes.iter_mut().find(|v| v.info.uuid == drive.uuid) else {
                continue;
            };
            if !v.online {
                // Removed while it was read: folders that could not be read
                // any more are marked, so the next scan goes on from here.
                let _ = save_partial(env, &drive, &new, &options);
                continue;
            }
            if v.root.is_some() || v.info.path != drive.mount_point {
                continue;
            }
            if new.roots().len() != 1 {
                // Its mount point could not be read: there is nothing to
                // add, and reading it again at once would find nothing
                // either. It is read when it is connected again.
                self.not_now
                    .lock()
                    .expect("lock")
                    .insert(drive.uuid.clone());
                continue;
            }
            let mut index = self.index.write().expect("index lock");
            if root_id(&index, &drive.mount_point).is_some() {
                continue;
            }
            let [root] = index.graft(&new)[..] else {
                unreachable!("one root was checked above");
            };
            v.root = Some(drive.mount_point.clone());
            v.options = options;
            v.info.entries = new.len() as u64;
            let _ = v.info.save(env);
            let _ = std::fs::remove_file(env.partial_path(&drive.uuid));
            changed = true;
            // Changes made while it was being read are caught up by the
            // caller.
            if let Some(w) = watcher.as_deref_mut() {
                w.watch_root(&index, root);
            }
            to_catch_up.push(root);
        }

        for drive in &mounted {
            let i = match volumes.iter().position(|v| v.info.uuid == drive.uuid) {
                Some(i) => i,
                None => {
                    volumes.push(Volume {
                        info: new_info(drive, now),
                        root: None,
                        online: false,
                        options: ScanOptions::default(),
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
                if let Some(w) = watcher.as_deref_mut() {
                    w.watch_root(&index, root);
                }
                to_catch_up.push(root);
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
    /// added by the next [`sync_drives`](Self::sync_drives). A scan that is
    /// cancelled keeps what it read in the drive's partial file, and the next
    /// scan of the drive goes on from there (see [`resume_scan`]).
    fn start_scan(&self, drive: Drive) {
        // Under this lock a scan is either started before quitting began,
        // and then cancelled and waited for, or not at all.
        let mut threads = self.drive_scans.lock().expect("lock");
        if self.stop.load(Ordering::SeqCst)
            || !self
                .scanning
                .lock()
                .expect("lock")
                .insert(drive.uuid.clone())
        {
            return;
        }
        threads.retain(|t| !t.is_finished());
        let options = self.scan_options();
        let (scanned, changed) = (Arc::clone(&self.scanned), Arc::clone(&self.mounts_changed));
        let task = self.tasks.begin(TaskKind::Drive(drive_name(&drive)), true);
        let env = self.drive_env.clone();
        let drive_uuid = drive.uuid.clone();
        let thread = std::thread::Builder::new()
            .name("drive scan".into())
            .spawn(move || {
                #[cfg(test)]
                tests::stuck_drive(&drive.mount_point);
                let roots = std::slice::from_ref(&drive.mount_point);
                task.progress().set_total(progress::estimate_scan(
                    roots,
                    &options.rules.excluded_paths(),
                ));
                let partial = env
                    .as_ref()
                    .and_then(|env| load_partial(env, &drive, &options));
                let (index, complete) = match partial {
                    Some(mut index) => {
                        // What was read before counts as done.
                        task.progress().add(index.len() as u64);
                        let done = resume_scan(&mut index, roots, &options, task.progress());
                        (index, done.is_ok())
                    }
                    None => {
                        let mut store = NodeStore::new();
                        let done = scan_with_progress(&mut store, roots, &options, task.progress());
                        (Index::new(store), done.is_ok())
                    }
                };
                drop(task);
                if let Some(env) = env.as_ref().filter(|_| !complete) {
                    let _ = save_partial(env, &drive, &index, &options);
                }
                scanned.lock().expect("lock").push(Scanned {
                    drive,
                    index,
                    complete,
                    options,
                });
                changed.store(true, Ordering::SeqCst);
            });
        match thread {
            Ok(thread) => threads.push(thread),
            Err(_) => {
                self.scanning.lock().expect("lock").remove(&drive_uuid);
            }
        }
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

    /// Stops the threads and saves the index. The running search or sort is
    /// cancelled and the search thread waited for; the index thread is not,
    /// because a first scan or a catch-up cannot be interrupted and the
    /// window should close at once. It ends with the process.
    ///
    /// Drive scans that are still running are cancelled, and their part read
    /// so far is saved to the drive's partial file, so the next start goes on
    /// from there. Quitting waits at most [`QUIT_WAIT`] in all: for the
    /// search thread, for those scans, and for the index to be free to save
    /// (a catch-up holds it). A scan stuck on a drive that does not answer
    /// ends with the process, and that drive is read again from where its
    /// last save left it; an index not saved is caught up at the next start.
    pub fn shutdown(&self) {
        self.shutdown_within(QUIT_WAIT);
    }

    /// [`shutdown`](Self::shutdown), waiting at most `wait`. Returns whether
    /// every drive scan ended in time.
    fn shutdown_within(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        self.stop.store(true, Ordering::SeqCst);
        let scans = {
            let mut scans = self.drive_scans.lock().expect("lock");
            self.tasks.cancel_drive_scans();
            std::mem::take(&mut *scans)
        };
        self.tasks.cancel_search();
        // Dropping the sender wakes the search thread so it sees the stop flag.
        self.searches.lock().expect("lock").take();
        let search = {
            let mut threads = self.threads.lock().expect("lock");
            (!threads.is_empty()).then(|| threads.remove(0))
        };
        // A search that waits for the index ends when it gets it.
        if let Some(search) = search {
            if retry_until(deadline, || search.is_finished().then_some(())).is_some() {
                let _ = search.join();
            }
        }
        let mut ended = true;
        for scan in scans {
            ended &= retry_until(deadline, || scan.is_finished().then_some(())).is_some();
        }
        self.save_by(Some(deadline));
        self.keep_scanned_drives(deadline);
        ended
    }

    /// Saves drives whose reading ended but that were not added to the index
    /// yet as partial files, so the next start only checks them again.
    /// Cancelled ones were saved by their scan thread.
    fn keep_scanned_drives(&self, deadline: Instant) {
        let Some(env) = &self.drive_env else { return };
        // `sync_drives` takes them under this lock and stops taking them
        // once quitting began.
        let Some(_volumes) = retry_until(deadline, || self.volumes.try_lock().ok()) else {
            return;
        };
        let scanned = std::mem::take(&mut *self.scanned.lock().expect("lock"));
        for s in scanned.into_iter().filter(|s| s.complete) {
            let _ = save_partial(env, &s.drive, &s.index, &s.options);
        }
    }
}

/// Calls `attempt` until it gives a value or `deadline` passes; at least
/// once.
fn retry_until<T>(deadline: Instant, mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    loop {
        if let Some(value) = attempt() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
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
    persist::save_with_fingerprint(
        &drive,
        &[name.to_path_buf()],
        fingerprint(&v.options),
        &env.index_path(&v.info.uuid),
    )?;
    v.info.save(env)
}

/// The identity of an index built with `options`: its rules and whether
/// file sizes and times were read (see [`persist::index_fingerprint`]).
fn fingerprint(options: &ScanOptions) -> u64 {
    persist::index_fingerprint(&options.rules.spec(), options.with_metadata)
}

/// Saves the part of `drive` read so far (`index`, read with `options`),
/// for the next scan to go on from.
fn save_partial(
    env: &DriveEnv,
    drive: &Drive,
    index: &Index,
    options: &ScanOptions,
) -> std::io::Result<()> {
    persist::save_with_fingerprint(
        index,
        std::slice::from_ref(&drive.mount_point),
        fingerprint(options),
        &env.partial_path(&drive.uuid),
    )
}

/// The part of `drive` that an earlier scan read, if it was read with
/// `options`, with its root renamed to where the drive is mounted now.
fn load_partial(env: &DriveEnv, drive: &Drive, options: &ScanOptions) -> Option<Index> {
    let path = env.partial_path(&drive.uuid);
    let saved = persist::load(&path).ok()?;
    let mut index = saved.index;
    let roots = index.roots();
    if saved.rules_hash != Some(fingerprint(options)) || roots.len() > 1 {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    if let Some(&root) = roots.first() {
        let name = drive.mount_point.as_os_str().as_bytes();
        index.rename(root, None, name).ok()?;
    }
    Some(index)
}

#[cfg(test)]
mod partial_tests {
    use super::*;
    use everygrep_core::rules::RuleSpec;
    use everygrep_core::store::Meta;

    fn paths(index: &Index) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = index
            .order()
            .iter()
            .map(|id| index.store().path(id))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_partly_read_drive_goes_on_at_its_new_mount_point() {
        let base =
            std::env::temp_dir().join(format!("everygrep-engine-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (first, second) = (base.join("media/OLD"), base.join("media/NEW"));
        for d in ["a", "b/deep"] {
            std::fs::create_dir_all(first.join(d)).unwrap();
            std::fs::write(first.join(d).join("f.txt"), b"f").unwrap();
        }
        std::fs::create_dir_all(base.join("volumes")).unwrap();
        let env = DriveEnv {
            mountinfo: base.join("mountinfo"),
            media_dirs: vec![base.join("media")],
            by_uuid: base.join("by-uuid"),
            by_label: base.join("by-label"),
            volumes_dir: base.join("volumes"),
        };
        let mut drive = Drive {
            uuid: "AB-12".into(),
            label: String::new(),
            mount_point: first.clone(),
        };
        let options = ScanOptions::default();
        // What a scan cancelled before reading "b" leaves: "b" without its
        // contents, marked as not read.
        let mut store = NodeStore::new();
        scan_with_progress(
            &mut store,
            std::slice::from_ref(&first),
            &options,
            &Progress::new(),
        )
        .unwrap();
        let mut index = Index::new(store);
        let root = index.roots()[0];
        let b = index.find_child(Some(root), b"b").unwrap();
        for kid in index.store().children(b).collect::<Vec<_>>() {
            index.remove(kid);
        }
        let meta = index.store().meta(b);
        index.set_meta(
            b,
            Meta {
                mtime_ns: 0,
                ..meta
            },
        );
        save_partial(&env, &drive, &index, &options).unwrap();

        // Connected again elsewhere.
        std::fs::rename(&first, &second).unwrap();
        drive.mount_point = second.clone();
        let mut resumed = load_partial(&env, &drive, &options).unwrap();
        resume_scan(
            &mut resumed,
            std::slice::from_ref(&second),
            &options,
            &Progress::new(),
        )
        .unwrap();
        let mut fresh = NodeStore::new();
        scan_with_progress(
            &mut fresh,
            std::slice::from_ref(&second),
            &options,
            &Progress::new(),
        )
        .unwrap();
        let (found, expected) = (paths(&resumed), paths(&Index::new(fresh)));

        // Read with other rules, it is not used, and it is gone.
        let other = ScanOptions {
            rules: IndexRules::new(RuleSpec {
                exclude_files: "*.txt".into(),
                ..RuleSpec::default()
            })
            .unwrap(),
            ..ScanOptions::default()
        };
        let with_other_rules = load_partial(&env, &drive, &other).is_none();
        let removed = !env.partial_path(&drive.uuid).exists();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(found, expected);
        assert!(found.contains(&second.join("b/deep/f.txt")));
        assert!(with_other_rules && removed);
    }
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
        notices: mpsc::Receiver<Notice>,
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
            let (notice_tx, notices) = mpsc::channel();
            let notice_tx = Mutex::new(notice_tx);
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
                on_notice: Box::new(move |n| {
                    let _ = notice_tx.lock().unwrap().send(n);
                }),
            };
            let options = ScanOptions {
                rules: folders(exclude),
                ..ScanOptions::default()
            };
            let engine = Engine::start(roots, options, watch, Some(index), drives, hooks);
            let _ = slot.set(Arc::downgrade(&engine));
            Self {
                engine,
                states,
                results,
                stopped,
                notices,
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
            let mut names = self.listed(query);
            names.sort();
            names
        }

        /// The names of the results of `query` in their order.
        fn listed(&self, query: &str) -> Vec<String> {
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
                return r
                    .ids
                    .iter()
                    .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                    .collect();
            }
        }
    }

    #[test]
    fn an_index_without_file_details_is_saved_apart_and_not_loaded_with_them() {
        let dir = Scratch::new("file-details");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("one.txt"), "12345").unwrap();
        let index_file = dir.0.join("index.bin");
        let roots = normalize_roots(std::slice::from_ref(&a));
        let size_of = |probe: &Probe| {
            let index = probe.engine.index.read().unwrap();
            let root = index.roots()[0];
            let id = index.find_child(Some(root), b"one.txt").unwrap();
            index.store().meta(id).size
        };

        let probe = Probe::start(roots.clone(), Vec::new(), index_file.clone());
        probe.wait_for(IndexState::Watching);
        assert!(probe.engine.file_details());
        assert_eq!(size_of(&probe), 5);
        probe.engine.set_file_details(false);
        probe.engine.reindex(&roots, IndexRules::default());
        probe.wait_for(IndexState::Scanning);
        probe.wait_for(IndexState::Watching);
        assert!(!probe.engine.file_details());
        assert_eq!(size_of(&probe), 0, "sizes were read");
        probe.engine.shutdown();
        let saved = persist::load(&index_file).unwrap();
        let plain = RuleSpec::default();
        assert_eq!(
            saved.rules_hash,
            Some(persist::index_fingerprint(&plain, false))
        );

        // A window that reads file details builds it again.
        let probe = Probe::start(roots.clone(), Vec::new(), index_file.clone());
        assert!(scanned_at_start(&probe), "loaded an index without details");
        assert_eq!(size_of(&probe), 5);
        probe.engine.shutdown();
        let saved = persist::load(&index_file).unwrap();
        assert_eq!(saved.rules_hash, Some(persist::rules_fingerprint(&plain)));
    }

    #[test]
    fn a_due_schedule_scans_again_while_watching_but_not_while_paused() {
        let dir = Scratch::new("schedule");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("one.txt"), "").unwrap();
        let roots = normalize_roots(std::slice::from_ref(&a));
        let probe = Probe::start(roots.clone(), Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        // A file whose folder did not change is found by a full scan only;
        // here it is simply made while nothing reports it.
        probe.engine.set_watching(false);
        probe.wait_for(IndexState::Unwatched);
        let two_hours_ago = unix_now() - 2 * 3600;
        probe
            .engine
            .last_full_scan
            .store(two_hours_ago, Ordering::SeqCst);
        probe.engine.set_schedule(Schedule::EveryHours(1));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            probe.states.try_iter().all(|s| s != IndexState::Scanning),
            "scanned while paused"
        );
        probe.engine.set_watching(true);
        probe.wait_for(IndexState::Scanning);
        probe.wait_for(IndexState::Watching);
        assert!(probe.engine.last_full_scan.load(Ordering::SeqCst) > two_hours_ago);
        // Not due again right after.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(probe.states.try_iter().all(|s| s != IndexState::Scanning));
        probe.engine.shutdown();
    }

    #[test]
    fn stats_count_files_folders_and_sizes() {
        let dir = Scratch::new("stats");
        let a = dir.0.join("a");
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::write(a.join("one.txt"), "123").unwrap();
        fs::write(a.join("sub/two.txt"), "4567").unwrap();
        let index_file = dir.0.join("index.bin");
        let probe = Probe::start(vec![a.clone()], Vec::new(), index_file.clone());
        probe.wait_for(IndexState::Watching);
        let stats = probe.engine.stats();
        // The root and sub.
        assert_eq!((stats.files, stats.folders), (2, 2));
        assert_eq!(stats.file_bytes, 7);
        assert_eq!(stats.roots, 1);
        assert_eq!((stats.drives, stats.drives_online), (0, 0));
        assert!(stats.memory > 0);
        assert!(stats.file_details);
        assert!(stats.last_full_scan.is_some(), "it scanned at the start");
        assert_eq!(stats.index_file.as_deref(), Some(index_file.as_path()));
        probe.engine.shutdown();
        assert!(fs::metadata(&index_file).unwrap().len() > 0);

        let probe = Probe::start(vec![a], Vec::new(), index_file.clone());
        probe.wait_for(IndexState::Watching);
        let stats = probe.engine.stats();
        assert_eq!(stats.last_full_scan, None, "the saved index was used");
        assert_eq!(
            stats.index_file_bytes,
            Some(fs::metadata(&index_file).unwrap().len())
        );
        probe.engine.shutdown();
    }

    #[test]
    fn a_moved_index_file_is_written_at_its_new_place_and_the_old_one_removed() {
        let dir = Scratch::new("move-index");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("one.txt"), "").unwrap();
        let (old, new) = (dir.0.join("index.bin"), dir.0.join("elsewhere/index.bin"));
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        let roots = normalize_roots(std::slice::from_ref(&a));
        let probe = Probe::start(roots.clone(), Vec::new(), old.clone());
        probe.wait_for(IndexState::Watching);
        probe.engine.save();
        assert!(old.exists());
        probe.engine.move_index_file(new.clone());
        assert_eq!(probe.engine.index_file(), Some(new.clone()));
        probe.engine.save();
        assert!(new.exists(), "not written at the new place");
        assert!(!old.exists(), "the old file stayed");
        probe.engine.shutdown();

        let probe = Probe::start(roots, Vec::new(), new);
        assert!(!scanned_at_start(&probe), "the moved index was not used");
        assert_eq!(probe.names("one"), ["one.txt"]);
        probe.engine.shutdown();
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
            on_notice: Box::new(|_| {}),
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
            ScanOptions::default(),
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

    #[test]
    fn readers_wait_only_briefly_while_a_long_catch_up_runs() {
        let dir = Scratch::new("catch-up-batches");
        let a = dir.0.join("a");
        const FOLDERS: usize = 10_000;
        for i in 0..FOLDERS {
            fs::create_dir_all(a.join(format!("d{i}"))).unwrap();
            fs::write(a.join(format!("d{i}/one.txt")), "").unwrap();
        }
        let index_file = dir.0.join("index.bin");
        let roots = normalize_roots(std::slice::from_ref(&a));
        let mut store = NodeStore::new();
        everygrep_core::scan::scan(&mut store, &roots, &ScanOptions::default());
        let rules = everygrep_core::rules::RuleSpec::default();
        persist::save_with_rules(&Index::new(store), &roots, &rules, &index_file).unwrap();
        // Every folder changed while the program was not running.
        for i in 0..FOLDERS {
            fs::write(a.join(format!("d{i}/two.txt")), "").unwrap();
        }

        let probe = Probe::start(vec![a.clone()], Vec::new(), index_file);
        let engine = Arc::clone(&probe.engine);
        let running = Arc::new(AtomicBool::new(true));
        // Reads the index as the window does for each row on screen.
        let reader = std::thread::spawn({
            let running = Arc::clone(&running);
            move || {
                let mut longest = Duration::ZERO;
                while running.load(Ordering::SeqCst) {
                    let asked = Instant::now();
                    let index = engine.index.read().unwrap();
                    longest = longest.max(asked.elapsed());
                    drop(index);
                    std::thread::sleep(Duration::from_millis(1));
                }
                longest
            }
        });
        let mut caught_up = false;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            match probe.states.recv_timeout(Duration::from_millis(100)) {
                Ok(IndexState::CatchingUp) => caught_up = true,
                Ok(IndexState::Watching) => break,
                _ => {}
            }
        }
        running.store(false, Ordering::SeqCst);
        let longest = reader.join().unwrap();
        assert!(caught_up, "no catch-up");
        let last = a
            .canonicalize()
            .unwrap()
            .join(format!("d{}/two.txt", FOLDERS - 1));
        assert!(find(&probe.engine.index.read().unwrap(), &last).is_some());
        assert!(
            longest < Duration::from_millis(150),
            "a reader waited {longest:?} for the catch-up"
        );
        probe.engine.shutdown();
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
    fn a_damaged_index_is_said_and_built_again_but_a_missing_one_is_not() {
        let dir = Scratch::new("damaged-index");
        let a = dir.0.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("one.txt"), "").unwrap();
        let index_file = dir.0.join("index.bin");
        let roots = normalize_roots(std::slice::from_ref(&a));

        let probe = Probe::start(roots.clone(), Vec::new(), index_file.clone());
        assert!(scanned_at_start(&probe));
        probe.engine.shutdown();
        assert!(
            probe.notices.try_recv().is_err(),
            "a first start is no news"
        );

        let mut bytes = fs::read(&index_file).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&index_file, bytes).unwrap();
        let probe = Probe::start(roots, Vec::new(), index_file.clone());
        assert!(scanned_at_start(&probe), "used a damaged index");
        assert_eq!(probe.names("one"), ["one.txt"]);
        match probe.notices.try_recv() {
            Ok(Notice::IndexRebuilt { damaged, .. }) => assert!(damaged),
            other => panic!("no notice: {other:?}"),
        }
        probe.engine.shutdown();
        assert!(persist::load(&index_file).is_ok(), "not saved again");
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
    fn a_newer_request_to_index_again_is_never_undone() {
        let dir = Scratch::new("rescan-newer");
        let root = |p: &str| dir.0.join(p);
        for f in ["a/one.txt", "b/two.txt", "c/three.txt"] {
            fs::create_dir_all(root(f).parent().unwrap()).unwrap();
            fs::write(root(f), "").unwrap();
        }
        let probe = Probe::start(vec![root("a")], Vec::new(), root("index.bin"));
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt"]);

        // The new index of "b" waits right after it is listed, as if slow.
        let (release, wait) = mpsc::channel();
        *probe.hold.lock().unwrap() = Some(wait);
        probe.engine.reindex(&[root("b")], IndexRules::default());
        probe
            .held
            .recv_timeout(Duration::from_secs(20))
            .expect("the new index begins");
        // Meanwhile "c" is asked for, then the "b" one is cancelled.
        let excluded = folders(vec![root("c/never")]);
        probe.engine.reindex(&[root("c")], excluded.clone());
        let running = probe.engine.tasks.list.lock().unwrap().clone();
        for task in running {
            probe.engine.cancel(task.id);
        }
        release.send(()).unwrap();

        let c = fs::canonicalize(root("c")).unwrap();
        until(&probe.engine, "the newer folders indexed", |i| {
            find(i, &c.join("three.txt")).is_some()
        });
        assert_eq!(probe.names("txt"), ["three.txt"]);
        assert_eq!(probe.engine.roots(), std::slice::from_ref(&c));
        assert_eq!(probe.engine.scan_options().rules.spec(), excluded.spec());
        probe.engine.shutdown();
    }

    #[test]
    fn no_old_watcher_is_used_on_the_index_built_after_lost_events() {
        let dir = Scratch::new("overflow-no-watcher");
        let root = dir.0.join("a");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.txt"), "").unwrap();
        let index = dir.0.join("index.bin");
        let probe = Probe::start(vec![root.clone()], Vec::new(), index.clone());
        probe.wait_for(IndexState::Watching);
        let root = fs::canonicalize(&root).unwrap();

        // The next change comes with lost events, and the index built again
        // after them gets no watcher of its own.
        FAILING_WATCHERS.lock().unwrap().push(index.clone());
        OVERFLOWS.lock().unwrap().push(index.clone());
        fs::write(root.join("two.txt"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut states = Vec::new();
        while !states.contains(&IndexState::Static) && Instant::now() < deadline {
            if let Ok(s) = probe.states.recv_timeout(Duration::from_millis(100)) {
                states.push(s);
            }
        }
        let gone = OVERFLOWS.lock().unwrap().is_empty();
        let unwatched = probe.engine.index.read().unwrap().len();
        // Not watched: a new file is not seen, and nothing else changes.
        fs::write(root.join("three.txt"), "").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let after = probe.engine.index.read().unwrap().len();
        FAILING_WATCHERS.lock().unwrap().retain(|p| p != &index);
        assert!(gone, "no lost events seen");
        assert!(states.contains(&IndexState::Static), "{states:?}");
        assert_eq!(unwatched, after);
        assert!(find(&probe.engine.index.read().unwrap(), &root.join("two.txt")).is_some());

        // Watched again once it can be: the change is caught up.
        probe.engine.set_watching(false);
        probe.wait_for(IndexState::Unwatched);
        probe.engine.set_watching(true);
        until(&probe.engine, "the file made while nothing watched", |i| {
            find(i, &root.join("three.txt")).is_some()
        });
        probe.engine.shutdown();
    }

    #[test]
    fn watching_starts_again_by_itself_once_a_watcher_can_be_made() {
        let dir = Scratch::new("overflow-watcher-retry");
        let root = dir.0.join("a");
        fs::create_dir_all(&root).unwrap();
        let index = dir.0.join("index.bin");
        let probe = Probe::start(vec![root.clone()], Vec::new(), index.clone());
        probe.wait_for(IndexState::Watching);
        let root = fs::canonicalize(&root).unwrap();

        // Lost events, and no watcher for the index built after them.
        FAILING_WATCHERS.lock().unwrap().push(index.clone());
        OVERFLOWS.lock().unwrap().push(index.clone());
        fs::write(root.join("one.txt"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut is_static = false;
        while !is_static && Instant::now() < deadline {
            is_static =
                probe.states.recv_timeout(Duration::from_millis(100)) == Ok(IndexState::Static);
        }
        fs::write(root.join("two.txt"), "").unwrap();

        // Watchers can be made again (an inotify instance was freed): the
        // next try, without any toggle or restart, catches up and watches.
        FAILING_WATCHERS.lock().unwrap().retain(|p| p != &index);
        assert!(is_static, "never shown as not watched");
        let deadline = Instant::now() + RETRY_WATCHER_EVERY + Duration::from_secs(10);
        let mut watching = false;
        while !watching && Instant::now() < deadline {
            watching =
                probe.states.recv_timeout(Duration::from_millis(100)) == Ok(IndexState::Watching);
        }
        assert!(watching, "not watched again");
        until(&probe.engine, "the file made while nothing watched", |i| {
            find(i, &root.join("two.txt")).is_some()
        });
        fs::write(root.join("three.txt"), "").unwrap();
        until(&probe.engine, "a file made once watched again", |i| {
            find(i, &root.join("three.txt")).is_some()
        });
        probe.engine.shutdown();
    }

    #[test]
    fn quitting_does_not_wait_long_for_an_index_held_by_a_catch_up() {
        let dir = Scratch::new("quit-held-index");
        fs::create_dir_all(dir.0.join("a")).unwrap();
        fs::write(dir.0.join("a/one.txt"), "").unwrap();
        let probe = Probe::start(vec![dir.0.join("a")], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        // Something holds the index for long, as a catch-up of many folders
        // does, while a search waits for it and the index is to be saved.
        let engine = Arc::clone(&probe.engine);
        let (locked, is_locked) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _index = engine.index.write().unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(3));
        });
        is_locked.recv().unwrap();
        probe.engine.dirty.store(true, Ordering::SeqCst);
        probe.engine.set_query("one", QueryOptions::default());
        std::thread::sleep(Duration::from_millis(50));

        let start = Instant::now();
        probe.engine.shutdown_within(Duration::from_millis(300));
        let took = start.elapsed();
        holder.join().unwrap();
        assert!(took < Duration::from_millis(1500), "{took:?}");
        // Not saved: it still is to be saved.
        assert!(probe.engine.dirty.load(Ordering::SeqCst));
    }

    #[test]
    fn a_result_shown_after_its_index_changed_does_not_break_compaction() {
        let dir = Scratch::new("stale-result");
        let root = dir.0.join("a");
        fs::create_dir_all(&root).unwrap();
        for f in 0..12_000 {
            fs::write(root.join(format!("f{f:05}")), "").unwrap();
        }
        fs::write(root.join("keep.txt"), "").unwrap();
        let probe = Probe::start(vec![root.clone()], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        let e = &probe.engine;
        // A result taken before the index changed reaches the table late:
        // its ids lie past the index now.
        let len = e.index.read().unwrap().len() as u32;
        let model = crate::results::ResultsModel::new(Arc::clone(e));
        model.set_result(&SearchResult {
            ids: Arc::new(vec![NodeId(len + 100)]),
            terms: Arc::new(Vec::new()),
            took: Duration::ZERO,
            error: None,
            query: String::new(),
            epoch: e.epoch().wrapping_sub(1),
            generation: 0,
        });
        // Enough entries go to compact the index.
        for f in 0..12_000 {
            fs::remove_file(root.join(format!("f{f:05}"))).unwrap();
        }
        until(e, "the files gone", |i| i.len() == 2);
        let epoch = e.epoch();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!e.index.is_poisoned());
        assert_eq!(probe.names("keep"), ["keep.txt"]);
        assert!(epoch > 1, "compacted (or built again) after the deletes");
        e.shutdown();
    }

    #[test]
    fn row_details_are_read_without_holding_up_the_window() {
        use crate::results::{DetailColumns, DetailReader, ResultsModel};
        use slint::Model;
        let dir = Scratch::new("row-details");
        fs::create_dir_all(dir.0.join("a")).unwrap();
        fs::write(dir.0.join("a/slow.txt"), "").unwrap();
        let probe = Probe::start(vec![dir.0.join("a")], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        while probe.results.try_recv().is_ok() {}
        probe.engine.set_query("slow", QueryOptions::default());
        let result = loop {
            let r = probe.results.recv_timeout(Duration::from_secs(20)).unwrap();
            if r.query == "slow" && r.ids.len() == 1 {
                break r;
            }
        };
        let slow = fs::canonicalize(dir.0.join("a/slow.txt")).unwrap();
        crate::results::tests::slow_details(&slow);

        let model = ResultsModel::new(Arc::clone(&probe.engine));
        let (deliver, batches) = mpsc::channel();
        let deliver = Mutex::new(deliver);
        model.read_details_with(DetailReader::start(move |batch| {
            let _ = deliver.lock().unwrap().send(batch);
        }));
        model.set_result(&result);
        model.set_detail_columns(DetailColumns {
            created: true,
            accessed: true,
            attributes: true,
        });
        let asked = Instant::now();
        let row = model.row_data(0).unwrap();
        let took = asked.elapsed();
        assert!(took < Duration::from_millis(300), "the row took {took:?}");
        assert!(row.accessed.is_empty(), "not read yet");
        // Shown again before the details came: asked only once.
        model.row_data(0).unwrap();
        let batch = batches.recv_timeout(Duration::from_secs(10)).unwrap();
        model.take_details(batch);
        assert!(!model.row_data(0).unwrap().accessed.is_empty());
        assert!(batches.recv_timeout(Duration::from_millis(1500)).is_err());
        probe.engine.shutdown();
    }

    #[test]
    fn a_shown_tab_asks_for_row_details_again_and_drops_older_answers() {
        use crate::results::{DetailColumns, DetailReader, ResultsModel};
        use slint::Model;
        let dir = Scratch::new("tab-details");
        fs::create_dir_all(dir.0.join("a")).unwrap();
        fs::write(dir.0.join("a/one.txt"), "").unwrap();
        let probe = Probe::start(vec![dir.0.join("a")], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        while probe.results.try_recv().is_ok() {}
        probe.engine.set_query("one", QueryOptions::default());
        let result = loop {
            let r = probe.results.recv_timeout(Duration::from_secs(20)).unwrap();
            if r.query == "one" && r.ids.len() == 1 {
                break r;
            }
        };
        let model = ResultsModel::new(Arc::clone(&probe.engine));
        let (deliver, batches) = mpsc::channel();
        let deliver = Mutex::new(deliver);
        model.read_details_with(DetailReader::start(move |batch| {
            let _ = deliver.lock().unwrap().send(batch);
        }));
        model.set_result(&result);
        model.set_detail_columns(DetailColumns {
            created: false,
            accessed: true,
            attributes: false,
        });
        // Asked while this tab was shown; the answer comes after a switch.
        assert!(model.row_data(0).unwrap().accessed.is_empty());
        let before_switch = batches.recv_timeout(Duration::from_secs(10)).unwrap();
        let tab = model.freeze();
        model.restore(None);
        model.restore(Some(&tab));
        // The tab shown again asks once more, and the older answer is not
        // taken for it.
        assert!(model.row_data(0).unwrap().accessed.is_empty());
        model.take_details(before_switch);
        assert!(
            model.row_data(0).unwrap().accessed.is_empty(),
            "older answer"
        );
        let after_switch = batches
            .recv_timeout(Duration::from_secs(10))
            .expect("asked again");
        model.take_details(after_switch);
        assert!(!model.row_data(0).unwrap().accessed.is_empty());
        probe.engine.shutdown();
    }

    #[test]
    fn thirty_frozen_tabs_share_rows_keep_selection_and_reject_old_epochs() {
        use slint::Model;
        let dir = Scratch::new("tab-snapshots");
        let root = dir.0.join("files");
        fs::create_dir(&root).unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index"));
        probe.wait_for(IndexState::Unwatched);
        let model = crate::results::ResultsModel::new(probe.engine.clone());
        // Synthetic ids measure state exchange independently of filesystem size.
        // No row data or file action is requested for these synthetic rows.
        let ids = Arc::new((0..1_000_000).map(NodeId).collect::<Vec<_>>());
        let mut snapshots = Vec::new();
        for i in 0..30 {
            model.set_result(&SearchResult {
                ids: ids.clone(),
                terms: Arc::default(),
                took: Duration::ZERO,
                error: None,
                query: i.to_string(),
                epoch: probe.engine.epoch(),
                generation: 0,
            });
            model.select_only(i);
            snapshots.push(model.freeze());
        }
        let start = Instant::now();
        for i in 0..3000 {
            model.restore(Some(&snapshots[i % 30]));
            std::hint::black_box(model.freeze());
        }
        eprintln!(
            "30 tabs, 1M synthetic rows: result state exchange {:.3} us/switch (3000 switches, debug build)",
            start.elapsed().as_secs_f64() * 1_000_000.0 / 3000.0
        );
        for (i, snapshot) in snapshots.iter().enumerate() {
            model.restore(Some(snapshot));
            assert_eq!(model.row_count(), 1_000_000);
            assert!(model.is_selected(i));
            assert!(!model.is_selected((i + 1) % 30));
            assert!(Arc::ptr_eq(&ids, &model.snapshot().0));
        }
        probe.engine.epoch.fetch_add(1, Ordering::SeqCst);
        model.restore(Some(&snapshots[0]));
        assert_eq!(model.row_count(), 0);
        model.restore(None);
        assert_eq!(model.row_count(), 0);
        probe.engine.shutdown();
    }

    #[test]
    fn results_name_the_request_they_answer() {
        let dir = Scratch::new("result-generation");
        fs::create_dir_all(dir.0.join("a")).unwrap();
        fs::write(dir.0.join("a/one.txt"), "").unwrap();
        let probe = Probe::start(vec![dir.0.join("a")], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        while probe.results.try_recv().is_ok() {}
        let asked = probe.engine.set_query("one", QueryOptions::default());
        let r = loop {
            let r = probe.results.recv_timeout(Duration::from_secs(20)).unwrap();
            if r.query == "one" {
                break r;
            }
        };
        assert_eq!(r.generation, asked);
        let model = crate::results::ResultsModel::new(Arc::clone(&probe.engine));
        assert!(model.is_current(&r));
        // A newer request came before the window took it: it is dropped.
        let newer = probe.engine.set_query("two", QueryOptions::default());
        assert!(newer > asked && !model.is_current(&r));
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
    fn content_search_finds_files_and_follows_the_content_settings() {
        let dir = Scratch::new("content-search");
        let root = dir.0.join("files");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), "the needle\n").unwrap();
        fs::write(root.join("b.txt"), "hay\n").unwrap();
        fs::write(
            root.join("sub/c.md"),
            format!("{}\nneedle\n", "x".repeat(500)),
        )
        .unwrap();
        let probe = Probe::start_unwatched(vec![root.clone()], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("content:needle"), ["a.txt", "c.md"]);
        assert_eq!(
            probe.names("content:needle | b.txt"),
            ["a.txt", "b.txt", "c.md"]
        );
        assert_eq!(probe.engine.content_cache.lock().unwrap().len(), 3);
        assert_eq!(probe.names("content:needle"), ["a.txt", "c.md"]);
        // Another order searches again with the cached answers: a file
        // changed behind the index's back keeps its answer.
        fs::write(root.join("b.txt"), "needle now\n").unwrap();
        while probe.results.try_recv().is_ok() {}
        probe.engine.set_sort(Sort {
            key: everygrep_core::sort::SortKey::Size,
            ..Sort::default()
        });
        let r = probe.results.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(r.ids.len(), 2);
        // New settings read every file again: c.md is now too large.
        while probe.results.try_recv().is_ok() {}
        probe.engine.set_content_options(ContentOptions {
            max_size: 100,
            ..ContentOptions::default()
        });
        let r = probe.results.recv_timeout(Duration::from_secs(20)).unwrap();
        let index = probe.engine.index.read().unwrap();
        let mut names: Vec<String> = r
            .ids
            .iter()
            .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
            .collect();
        drop(index);
        names.sort();
        assert_eq!(names, ["a.txt", "b.txt"]);
        // Searching again with the same settings changes nothing.
        let generation = probe.engine.generation.load(Ordering::SeqCst);
        probe.engine.set_content_options(ContentOptions {
            max_size: 100,
            ..ContentOptions::default()
        });
        assert_eq!(probe.engine.generation.load(Ordering::SeqCst), generation);
        probe.engine.shutdown();
    }

    #[test]
    fn a_filter_and_the_field_with_content_terms_each_read_their_own() {
        let dir = Scratch::new("content-filter");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "alpha\n").unwrap();
        fs::write(root.join("b.txt"), "beta\n").unwrap();
        fs::write(root.join("ab.txt"), "alpha beta\n").unwrap();
        fs::write(root.join("none.txt"), "gamma\n").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        let filtered = |field: &str, filter: &str| -> Result<Vec<String>, String> {
            while probe.results.try_recv().is_ok() {}
            let options = QueryOptions::default();
            probe
                .engine
                .set_filtered_query(field, options, Some((filter.to_owned(), options)));
            loop {
                let r = probe
                    .results
                    .recv_timeout(Duration::from_secs(20))
                    .expect("results");
                if r.query != field {
                    continue;
                }
                if let Some(e) = r.error {
                    return Err(e);
                }
                let index = probe.engine.index.read().unwrap();
                let mut names: Vec<String> = r
                    .ids
                    .iter()
                    .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                    .collect();
                names.sort();
                return Ok(names);
            }
        };
        assert_eq!(
            filtered("content:alpha", "content:beta").unwrap(),
            ["ab.txt"]
        );
        assert_eq!(
            filtered("content:beta", "!content:alpha").unwrap(),
            ["b.txt"]
        );
        assert_eq!(
            filtered("content:alpha", "content:beta | content:gamma").unwrap(),
            ["ab.txt"]
        );
        assert_eq!(
            filtered("ext:txt", "content:alpha").unwrap(),
            ["a.txt", "ab.txt"]
        );
        // Seven content terms together are refused, as in one query.
        let error = filtered(
            "content:a content:b content:c content:d",
            "content:e content:f content:g",
        )
        .unwrap_err();
        assert!(error.contains("at most 6 content terms"), "{error}");
        probe.engine.shutdown();
    }

    #[test]
    fn a_tab_shown_again_searches_only_when_the_index_changed() {
        let dir = Scratch::new("settled-tab");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "same\n").unwrap();
        fs::write(root.join("b.txt"), "same\n").unwrap();
        let probe = Probe::start(vec![root.clone()], Vec::new(), dir.0.join("index.bin"));
        probe.wait_for(IndexState::Watching);
        let engine = &probe.engine;
        let generation = || engine.generation.load(Ordering::SeqCst);
        let options = QueryOptions::default();
        let sort = engine.asked_sort();
        // A duplicate search, complete: its tab keeps it.
        assert_eq!(probe.names("duplicates:"), ["a.txt", "b.txt"]);
        let dupes = engine.settled_search().expect("complete");
        assert_eq!(probe.names("b.txt"), ["b.txt"]);
        assert!(engine.settled_search().is_some());
        // Shown again unchanged: current, without a search.
        while probe.results.try_recv().is_ok() {}
        let before = generation();
        assert!(
            engine
                .show_settled(&dupes, "duplicates:", options, None, by_size(sort))
                .is_none()
        );
        assert_eq!(generation(), before, "another order is searched");
        let shown = engine
            .show_settled(&dupes, "duplicates:", options, None, sort)
            .expect("unchanged");
        assert_eq!(generation(), shown);
        assert!(
            probe
                .results
                .recv_timeout(Duration::from_millis(700))
                .is_err()
        );
        assert_eq!(engine.applied.lock().unwrap().text, "duplicates:");
        assert!(engine.settled_search().is_some(), "still complete");
        // After a file change the tab searches again.
        let version = engine.index.read().unwrap().version();
        fs::write(root.join("c.txt"), "same\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while engine.index.read().unwrap().version() == version {
            assert!(Instant::now() < deadline, "change not seen");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            engine
                .show_settled(&dupes, "duplicates:", options, None, sort)
                .is_none()
        );
        assert_eq!(probe.names("duplicates:"), ["a.txt", "b.txt", "c.txt"]);
        probe.engine.shutdown();

        fn by_size(sort: Sort) -> Sort {
            Sort {
                key: everygrep_core::sort::SortKey::Size,
                ..sort
            }
        }
    }

    #[test]
    fn a_stopped_content_search_shown_again_stays_stopped() {
        let dir = Scratch::new("content-stopped-tab");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "needle\n").unwrap();
        fs::write(root.join("b.txt"), "other\n").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        let engine = &probe.engine;
        let generation = || engine.generation.load(Ordering::SeqCst);
        assert_eq!(probe.names("content:needle"), ["a.txt"]);
        assert!(engine.stopped_search().is_none(), "not stopped");
        // The user stops it (its matches so far stay shown).
        while probe.stopped.try_recv().is_ok() {}
        let state = engine.query.lock().unwrap().clone();
        engine.content_stopped(generation(), &state);
        assert_eq!(probe.wait_stopped(), Stopped::ContentSearch);
        let stopped = engine.stopped_search().expect("stopped");
        // Another tab searches.
        assert_eq!(probe.names("b.txt"), ["b.txt"]);
        assert!(engine.stopped_search().is_none());
        // A different request does not count as the stopped one.
        let options = QueryOptions::default();
        let sort = engine.asked_sort();
        let by_size = Sort {
            key: everygrep_core::sort::SortKey::Size,
            ..sort
        };
        let before = generation();
        assert!(
            engine
                .show_stopped(&stopped, "content:needle", options, None, by_size)
                .is_none()
        );
        assert!(
            engine
                .show_stopped(&stopped, "content:needlE", options, None, sort)
                .is_none()
        );
        assert_eq!(generation(), before);
        // Its tab shown again: current and still stopped, nothing searched.
        while probe.results.try_recv().is_ok() {}
        let shown = engine
            .show_stopped(&stopped, "content:needle", options, None, sort)
            .expect("the same request");
        assert_eq!(generation(), shown);
        assert!(
            probe
                .results
                .recv_timeout(Duration::from_millis(700))
                .is_err()
        );
        assert!(engine.stopped_search().is_some());
        assert_eq!(engine.applied.lock().unwrap().text, "content:needle");
        // File changes leave it alone; asking for a search runs it.
        engine.request_refresh();
        assert_eq!(generation(), shown);
        assert_eq!(probe.names("content:needle"), ["a.txt"]);
        assert!(engine.stopped_search().is_none());
        probe.engine.shutdown();
    }

    #[test]
    fn file_changes_wait_for_a_content_search_and_leave_a_stopped_one() {
        let dir = Scratch::new("content-refresh");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "needle\n").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("content:needle"), ["a.txt"]);
        let engine = &probe.engine;
        let generation = || engine.generation.load(Ordering::SeqCst);

        // While files are read, a change is noted and searched after.
        let before = generation();
        engine.content_state.lock().unwrap().running = true;
        engine.request_refresh();
        assert_eq!(generation(), before);
        engine.content_state.lock().unwrap().running = false;
        engine.refresh_after_contents();
        assert_eq!(generation(), before + 1);
        engine.refresh_after_contents();
        assert_eq!(generation(), before + 1, "once");
        assert_eq!(probe.names("content:needle"), ["a.txt"]);

        // Stopped by the user after matches were shown: they stay, and file
        // changes do not start it again until the user asks for a search.
        while probe.stopped.try_recv().is_ok() {}
        let state = engine.query.lock().unwrap().clone();
        engine.content_stopped(generation(), &state);
        assert_eq!(probe.wait_stopped(), Stopped::ContentSearch);
        assert_eq!(engine.applied.lock().unwrap().text, "content:needle");
        let before = generation();
        engine.request_refresh();
        assert_eq!(generation(), before);
        engine.set_sort(Sort::default());
        assert!(!engine.content_state.lock().unwrap().stopped);
        engine.request_refresh();
        assert_eq!(generation(), before + 2);
        // A stop for an older request is not reported.
        engine.content_stopped(before, &state);
        assert!(
            probe
                .stopped
                .recv_timeout(Duration::from_millis(200))
                .is_err()
        );
        probe.engine.shutdown();
    }

    #[test]
    fn duplicates_are_compared_off_the_lock_kept_in_groups_and_their_hashes_saved() {
        let dir = Scratch::new("dupes");
        let root = dir.0.join("files");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), "same").unwrap();
        fs::write(root.join("sub/b.txt"), "same").unwrap();
        fs::write(root.join("c.txt"), "diff").unwrap();
        let big = vec![7u8; 100_000];
        fs::write(root.join("big1"), &big).unwrap();
        fs::write(root.join("big2"), &big).unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(
            probe.listed("duplicates:"),
            ["a.txt", "b.txt", "big1", "big2"]
        );
        // Groups stay together in the window's order.
        probe.engine.set_sort(Sort {
            key: everygrep_core::sort::SortKey::Size,
            descending: true,
            ..Sort::default()
        });
        assert_eq!(
            probe.listed("duplicates:"),
            ["big1", "big2", "a.txt", "b.txt"]
        );
        assert_eq!(probe.listed("extra-copies:"), ["big2", "b.txt"]);
        assert_eq!(probe.listed("unique: file:"), ["c.txt"]);
        // Without contents nothing is read.
        assert_eq!(
            probe.listed("duplicates:ext txt"),
            ["a.txt", "b.txt", "c.txt"]
        );
        assert_eq!(
            probe.listed("duplicates:name limit:1"),
            Vec::<String>::new()
        );
        // Ends of the four of size 4 and the two large files, and the whole
        // of those two.
        assert_eq!(probe.engine.hashes.cache().lock().unwrap().len(), 5);
        assert!(!probe.engine.content_state.lock().unwrap().running);
        probe.engine.shutdown();
        assert!(dir.0.join("hashes.bin").exists());
    }

    #[test]
    fn searches_count_frequencies_only_while_a_frequency_column_is_shown() {
        let dir = Scratch::new("frequencies");
        let root = dir.0.join("files");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), "1").unwrap();
        fs::write(root.join("sub/a.txt"), "2").unwrap();
        fs::write(root.join("b.md"), "33").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("a.txt"), ["a.txt", "a.txt"]);
        assert!(
            probe
                .engine
                .index
                .read()
                .unwrap()
                .last_frequencies()
                .is_none()
        );
        probe.engine.set_frequency_columns(true);
        assert_eq!(probe.names("b."), ["b.md"]);
        {
            let index = probe.engine.index.read().unwrap();
            let f = index.last_frequencies().unwrap();
            assert!(f.is_for(&index));
            let a = index
                .order()
                .iter()
                .find(|&id| index.store().name(id) == b"a.txt");
            assert_eq!(f.name(a.unwrap()), 2);
        }
        // Sorting by one works the same way.
        probe.engine.set_sort(Sort {
            key: everygrep_core::sort::SortKey::NameFrequency,
            descending: true,
            ..Sort::default()
        });
        assert_eq!(probe.listed("file:"), ["a.txt", "a.txt", "b.md"]);
        probe.engine.shutdown();
    }

    #[test]
    fn a_cancelled_duplicate_search_keeps_the_previous_results() {
        let dir = Scratch::new("dupes-cancel");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.txt"), "same").unwrap();
        fs::write(root.join("two.txt"), "same").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("one"), ["one.txt"]);
        while probe.results.try_recv().is_ok() {}
        probe.armed.store(true, Ordering::SeqCst);
        probe
            .engine
            .set_query("duplicates:", QueryOptions::default());
        assert_eq!(probe.wait_stopped(), Stopped::Search);
        assert_eq!(probe.engine.query.lock().unwrap().text, "one");
        assert!(
            probe
                .results
                .recv_timeout(Duration::from_millis(200))
                .is_err()
        );
        assert_eq!(probe.names("duplicates:"), ["one.txt", "two.txt"]);
        probe.engine.shutdown();
    }

    #[test]
    fn a_content_search_cancelled_before_matches_were_shown_keeps_the_previous_results() {
        let dir = Scratch::new("content-cancel");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.txt"), "needle\n").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("txt"), ["one.txt"]);
        while probe.results.try_recv().is_ok() {}
        probe.armed.store(true, Ordering::SeqCst);
        probe
            .engine
            .set_query("content:needle", QueryOptions::default());
        assert_eq!(probe.wait_stopped(), Stopped::Search);
        assert_eq!(probe.engine.query.lock().unwrap().text, "txt");
        assert!(!probe.engine.content_state.lock().unwrap().running);
        assert!(
            probe
                .results
                .recv_timeout(Duration::from_millis(200))
                .is_err()
        );
        probe.engine.shutdown();
    }

    #[test]
    fn matches_so_far_are_shown_sorted_for_the_current_request_only() {
        let dir = Scratch::new("content-partial");
        let root = dir.0.join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("small.txt"), "needle\n").unwrap();
        fs::write(
            root.join("large.txt"),
            format!("needle {}\n", "x".repeat(100)),
        )
        .unwrap();
        fs::write(root.join("other.txt"), "hay\n").unwrap();
        let probe = Probe::start_unwatched(vec![root], dir.0.join("index.bin"));
        probe.wait_for(IndexState::Unwatched);
        assert_eq!(probe.names("content:needle"), ["large.txt", "small.txt"]);
        let engine = &probe.engine;
        let query = Query::parse("content:needle", QueryOptions::default()).unwrap();
        let (staged, epoch) = {
            let index = engine.index.read().unwrap();
            (
                search_staged(&index, &query, 1, &Progress::new()).unwrap(),
                engine.epoch(),
            )
        };
        let decisions = Decisions::new(staged.pending.len());
        decide_all(
            &staged.pending,
            query.content_terms(),
            &ContentOptions::default(),
            None,
            1,
            &Progress::new(),
            &decisions,
            || {},
        )
        .unwrap();
        let state = QueryState {
            text: "content:needle".into(),
            options: QueryOptions::default(),
            filter_query: None,
            sort: Sort {
                key: everygrep_core::sort::SortKey::Size,
                descending: true,
                ..Sort::default()
            },
        };
        let terms = Arc::new(Vec::new());
        let generation = engine.set_query("other", QueryOptions::default());
        let _ = probe.names("other");
        while probe.results.try_recv().is_ok() {}
        // An older request shows nothing.
        let shown = engine.show_partial(
            generation - 1,
            &state,
            &query,
            &staged,
            &decisions,
            epoch,
            Instant::now(),
            &terms,
        );
        assert!(!shown);
        let generation = engine.generation.load(Ordering::SeqCst);
        assert!(engine.show_partial(
            generation,
            &state,
            &query,
            &staged,
            &decisions,
            epoch,
            Instant::now(),
            &terms,
        ));
        let r = probe.results.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            (r.generation, r.query.as_str()),
            (generation, "content:needle")
        );
        let index = engine.index.read().unwrap();
        let names: Vec<&[u8]> = r.ids.iter().map(|&id| index.store().name(id)).collect();
        assert_eq!(names, [&b"large.txt"[..], b"small.txt"], "largest first");
        drop(index);
        assert_eq!(engine.applied.lock().unwrap().text, "content:needle");
        // Ids of another epoch are never shown.
        assert!(!engine.show_partial(
            generation,
            &state,
            &query,
            &staged,
            &decisions,
            epoch + 1,
            Instant::now(),
            &terms,
        ));
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

    /// Index files of engines whose next batch of file changes counts as
    /// lost events (a queue overflow), and of engines that cannot make a
    /// new watcher.
    pub(super) static OVERFLOWS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    pub(super) static FAILING_WATCHERS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Whether a test listed the engine with `index_file` in `list`.
    pub(super) fn listed(list: &Mutex<Vec<PathBuf>>, index_file: Option<&Path>) -> bool {
        let list = list.lock().unwrap();
        index_file.is_some_and(|f| list.iter().any(|p| p == f))
    }

    /// Like [`listed`], taking the engine off the list.
    pub(super) fn take(list: &Mutex<Vec<PathBuf>>, index_file: Option<&Path>) -> bool {
        let mut list = list.lock().unwrap();
        let before = list.len();
        list.retain(|p| Some(p.as_path()) != index_file);
        list.len() != before
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

    /// Mount points whose drive scan hangs before reading anything, as on a
    /// drive that does not answer, until they are taken off the list.
    static STUCK_DRIVES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Waits while a test lists `mount_point` as stuck (20 s at most).
    pub(super) fn stuck_drive(mount_point: &Path) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline
            && STUCK_DRIVES
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == mount_point)
        {
            std::thread::sleep(Duration::from_millis(10));
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
    fn a_drive_whose_mount_point_cannot_be_read_is_left_out() {
        let dir = Scratch::new("drives-unreadable");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let usb = dir.0.join("media/USB");
        fs::create_dir_all(&usb).unwrap();
        fs::write(usb.join("later.txt"), "").unwrap();
        let probe = Probe::start_with(
            vec![home],
            Vec::new(),
            dir.0.join("index.bin"),
            Some((fake.env.clone(), DriveSettings::default())),
        );
        probe.wait_for(IndexState::Watching);
        let e = &probe.engine;
        // Listed in the mount table, but its mount point cannot be looked
        // at: its reading finds nothing, not even its root.
        fake.mount("GHOST-1", "sdx3", "GHOST");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !e
            .drives()
            .iter()
            .any(|d| d.uuid == "GHOST-1" && !d.scanning)
        {
            assert!(Instant::now() < deadline, "the drive was never read");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(600));
        assert!(!e.index.is_poisoned() && !e.volumes.is_poisoned());
        let ghost = e.drives().into_iter().find(|d| d.uuid == "GHOST-1");
        assert!(ghost.is_some_and(|d| !d.indexed));
        // The index thread still takes drives in.
        fake.mount("REAL-1", "sdx4", "USB");
        until(e, "the next drive", |i| {
            find(i, &usb.join("later.txt")).is_some()
        });
        e.shutdown();
    }

    /// How many entries of `index` have the path `path`.
    fn count(index: &Index, path: &Path) -> usize {
        index
            .order()
            .iter()
            .filter(|&id| index.store().path(id) == path)
            .count()
    }

    #[test]
    fn indexing_again_reads_drives_with_the_new_rules_and_roots() {
        let dir = Scratch::new("drives-reindex");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let usb = dir.0.join("media/USB");
        fs::create_dir_all(&usb).unwrap();
        fs::write(usb.join("junk.tmp"), "").unwrap();
        fs::write(usb.join("keep.txt"), "").unwrap();
        let probe = Probe::start_with(
            vec![home.clone()],
            Vec::new(),
            dir.0.join("index.bin"),
            Some((fake.env.clone(), DriveSettings::default())),
        );
        probe.wait_for(IndexState::Watching);
        let e = &probe.engine;
        fake.mount("RULE-1", "sdx1", "USB");
        until(e, "the drive", |i| find(i, &usb.join("junk.tmp")).is_some());

        // New rules leave the drive's *.tmp files out too.
        let no_tmp = IndexRules::new(everygrep_core::rules::RuleSpec {
            exclude_files: "*.tmp".into(),
            ..Default::default()
        })
        .unwrap();
        e.reindex(std::slice::from_ref(&home), no_tmp);
        probe.wait_for(IndexState::Scanning);
        until(e, "the drive read with the new rules", |i| {
            find(i, &usb.join("junk.tmp")).is_none() && find(i, &usb.join("keep.txt")).is_some()
        });

        // A folder to index that holds the drive: its entries once.
        e.reindex(&[home, dir.0.join("media")], IndexRules::default());
        probe.wait_for(IndexState::Scanning);
        probe.wait_for(IndexState::Watching);
        until(e, "the drive's entries once", |i| {
            count(i, &usb.join("keep.txt")) == 1
        });
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(count(&e.index.read().unwrap(), &usb.join("keep.txt")), 1);
        assert!(!e.drives()[0].indexed);
        e.shutdown();
    }

    #[test]
    fn an_action_confirmed_after_its_drive_went_away_is_refused() {
        let dir = Scratch::new("drives-confirm");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let usb = dir.0.join("media/USB");
        fs::create_dir_all(&usb).unwrap();
        fs::write(usb.join("photo.jpg"), "").unwrap();
        let probe = Probe::start_with(
            vec![home],
            Vec::new(),
            dir.0.join("index.bin"),
            Some((fake.env.clone(), DriveSettings::default())),
        );
        probe.wait_for(IndexState::Watching);
        let e = &probe.engine;
        fake.mount("CONF-1", "sdx1", "USB");
        until(e, "the drive", |i| {
            find(i, &usb.join("photo.jpg")).is_some()
        });
        while probe.results.try_recv().is_ok() {}
        e.set_query("photo", QueryOptions::default());
        let r = loop {
            let r = probe.results.recv_timeout(Duration::from_secs(20)).unwrap();
            if r.query == "photo" && !r.ids.is_empty() {
                break r;
            }
        };
        let model = crate::results::ResultsModel::new(Arc::clone(e));
        model.set_result(&r);
        let asked = model.checked_targets(Some(0)).unwrap();
        assert_eq!(asked.paths, [usb.join("photo.jpg")]);
        let again = model.checked_targets(Some(0)).unwrap();
        assert_eq!(model.recheck(again).unwrap(), [usb.join("photo.jpg")]);

        // Removed while the confirmation was open.
        fake.unmount_all();
        until(e, "the drive offline", |i| {
            find(i, &usb.join("photo.jpg")).is_some_and(|id| i.is_offline(id))
        });
        assert_eq!(model.recheck(asked).unwrap_err(), crate::results::Offline);
        assert!(model.checked_targets(Some(0)).is_err());
        e.shutdown();
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

    /// A drive of `folders` folders with `files` empty files each.
    fn fill_drive(root: &Path, folders: usize, files: usize) -> u64 {
        for d in 0..folders {
            let dir = root.join(format!("folder{d:05}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..files {
                fs::write(dir.join(format!("file{f:04}.dat")), "").unwrap();
            }
        }
        (1 + folders * (1 + files)) as u64
    }

    /// The progress of the running drive scan, if one runs.
    fn drive_scan_done(engine: &Engine) -> Option<u64> {
        let list = engine.tasks.list.lock().unwrap();
        list.iter()
            .find(|t| matches!(*t.kind.lock().unwrap(), TaskKind::Drive(_)))
            .map(|t| t.progress.done())
    }

    /// Mounts a drive of `total` entries, quits once `at` of them are read,
    /// and returns how long quitting took and how many entries the partial
    /// file holds.
    fn quit_while_reading(
        fake: &FakeDrives,
        home: &Path,
        index: &Path,
        at: u64,
    ) -> (Duration, u64) {
        let probe = Probe::start_with(
            vec![home.to_path_buf()],
            Vec::new(),
            index.to_path_buf(),
            Some((fake.env.clone(), DriveSettings::default())),
        );
        probe.wait_for(IndexState::Watching);
        fake.mount("AAAA-1111", "sdx1", "USB");
        let deadline = Instant::now() + Duration::from_secs(60);
        while drive_scan_done(&probe.engine).is_none_or(|done| done < at) {
            assert!(Instant::now() < deadline, "the drive scan never got far");
            std::thread::sleep(Duration::from_micros(200));
        }
        let start = Instant::now();
        assert!(probe.engine.shutdown_within(QUIT_WAIT));
        let took = start.elapsed();
        let partial = persist::load(&fake.env.partial_path("AAAA-1111")).expect("a partial file");
        (took, partial.index.len() as u64)
    }

    #[test]
    fn quitting_while_a_drive_is_read_keeps_what_was_read_for_the_next_start() {
        let dir = Scratch::new("drives-quit");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let usb = dir.0.join("media/USB");
        let total = fill_drive(&usb, 400, 100);
        let index = dir.0.join("index.bin");

        let (took, kept) = quit_while_reading(&fake, &home, &index, 2000);
        eprintln!("quit took {took:?}; {kept} of {total} entries kept");
        assert!(kept > 0 && kept < total, "kept {kept} of {total}");
        assert!(took < QUIT_WAIT);
        // Nothing of it went into the index file of the folders.
        assert_eq!(persist::load(&index).unwrap().index.len(), 1);
        // A file that was read is deleted before the drive is read again.
        let partial = persist::load(&fake.env.partial_path("AAAA-1111")).unwrap();
        let deleted = partial
            .index
            .order()
            .iter()
            .find(|&id| !partial.index.store().is_dir(id))
            .map(|id| partial.index.store().path(id))
            .expect("a file was read");
        fs::remove_file(&deleted).unwrap();

        // The next start goes on from there and ends with the whole drive.
        let probe = Probe::start_with(
            vec![home],
            Vec::new(),
            index,
            Some((fake.env.clone(), DriveSettings::default())),
        );
        until(&probe.engine, "the whole drive", |i| {
            find(i, &usb.join("folder00399/file0099.dat")).is_some()
                && find(i, &usb.join("folder00000/file0001.dat")).is_some()
        });
        until(&probe.engine, "the partial file gone", |_| {
            !fake.env.partial_path("AAAA-1111").exists()
        });
        let entries = probe.engine.drives()[0].entries;
        assert_eq!(entries, total - 1);
        assert!(find(&probe.engine.index.read().unwrap(), &deleted).is_none());
        probe.engine.shutdown();
    }

    #[test]
    fn quitting_does_not_wait_long_for_a_drive_that_hangs() {
        let dir = Scratch::new("drives-stuck");
        let fake = FakeDrives::new(&dir.0);
        let home = dir.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let usb = dir.0.join("media/USB");
        fs::create_dir_all(&usb).unwrap();
        STUCK_DRIVES.lock().unwrap().push(usb.clone());
        let probe = Probe::start_with(
            vec![home],
            Vec::new(),
            dir.0.join("index.bin"),
            Some((fake.env.clone(), DriveSettings::default())),
        );
        probe.wait_for(IndexState::Watching);
        fake.mount("BBBB-2222", "sdx2", "USB");
        let deadline = Instant::now() + Duration::from_secs(20);
        while drive_scan_done(&probe.engine).is_none() {
            assert!(Instant::now() < deadline, "no drive scan");
            std::thread::sleep(Duration::from_millis(5));
        }
        let start = Instant::now();
        let ended = probe.engine.shutdown_within(Duration::from_millis(300));
        let took = start.elapsed();
        STUCK_DRIVES.lock().unwrap().retain(|p| p != &usb);
        assert!(!ended);
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_secs(3),
            "{took:?}"
        );
        // Let go, the scan ends (cancelled) before the scratch folder goes.
        let deadline = Instant::now() + Duration::from_secs(20);
        while probe.engine.scanned.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the scan never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Quit delays for larger drives: `cargo test -p everygrep-gui --release
    /// quit_delay -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing only"]
    fn quit_delay_while_reading_a_drive() {
        for (folders, files) in [(1000, 100), (2000, 250)] {
            for share in [4, 2] {
                let dir = Scratch::new("drives-quit-timing");
                let fake = FakeDrives::new(&dir.0);
                let home = dir.0.join("home");
                fs::create_dir_all(&home).unwrap();
                let total = fill_drive(&dir.0.join("media/USB"), folders, files);
                let at = total / share;
                let (took, kept) = quit_while_reading(&fake, &home, &dir.0.join("index.bin"), at);
                eprintln!("drive of {total} entries, quit at {at}: {took:?}, {kept} kept");
            }
        }
    }
}
