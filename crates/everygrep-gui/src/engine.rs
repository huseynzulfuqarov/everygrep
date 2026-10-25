//! The engine side of the window: loading the index, searching on a worker
//! thread, and applying file changes on a watcher thread.
//!
//! The index lives behind a read-write lock. Searches and the result table take
//! short read locks; the watcher takes the write lock only while it applies a
//! batch of changes, never while it waits for them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use everygrep_core::index::Index;
use everygrep_core::persist;
use everygrep_core::query::{Query, QueryOptions};
use everygrep_core::scan::{ScanOptions, normalize_roots, scan};
use everygrep_core::search::search;
use everygrep_core::sort::{Sort, sort};
use everygrep_core::store::{NodeId, NodeStore};
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
}

/// The finished result of one search.
pub struct SearchResult {
    pub ids: Arc<Vec<NodeId>>,
    pub took: Duration,
    pub error: Option<String>,
}

/// Callbacks into the UI. They are called from engine threads and must hand
/// the work over to the UI thread themselves.
pub struct Hooks {
    pub on_results: Box<dyn Fn(SearchResult) + Send + Sync>,
    pub on_state: Box<dyn Fn(IndexState, usize) + Send + Sync>,
}

/// Shared engine state.
pub struct Engine {
    pub index: RwLock<Index>,
    roots: Mutex<Vec<PathBuf>>,
    options: ScanOptions,
    index_file: Option<PathBuf>,
    /// Bumped for every new query; older results are dropped.
    generation: AtomicU64,
    query: Mutex<QueryState>,
    /// Ids currently shown, so they can be translated after a compaction.
    shown: Mutex<Arc<Vec<NodeId>>>,
    dirty: AtomicBool,
    stop: AtomicBool,
    searches: Mutex<Option<mpsc::Sender<u64>>>,
    hooks: Hooks,
    threads: Mutex<Vec<JoinHandle<()>>>,
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
    /// Creates the engine and starts loading in the background.
    pub fn start(roots: Vec<PathBuf>, index_file: Option<PathBuf>, hooks: Hooks) -> Arc<Self> {
        let engine = Arc::new(Self {
            index: RwLock::new(Index::new(NodeStore::new())),
            roots: Mutex::new(normalize_roots(&roots)),
            options: ScanOptions::default(),
            index_file: index_file.or_else(persist::default_path),
            generation: AtomicU64::new(0),
            query: Mutex::new(QueryState::default()),
            shown: Mutex::new(Arc::new(Vec::new())),
            dirty: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            searches: Mutex::new(None),
            hooks,
            threads: Mutex::new(Vec::new()),
        });
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
        engine
    }

    /// Runs `text` as the current query. Returns the generation of the request.
    pub fn set_query(&self, text: &str, options: QueryOptions) -> u64 {
        {
            let mut q = self.query.lock().expect("lock");
            q.text = text.to_owned();
            q.options = options;
        }
        self.request_search()
    }

    /// Changes the result order and runs the current query again. The order
    /// stays for later queries. Returns the generation of the request.
    pub fn set_sort(&self, sort: Sort) -> u64 {
        self.query.lock().expect("lock").sort = sort;
        self.request_search()
    }

    /// Records the ids now shown in the table.
    pub fn set_shown(&self, ids: Arc<Vec<NodeId>>) {
        *self.shown.lock().expect("lock") = ids;
    }

    /// Whether `generation` is still the newest request.
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    fn request_search(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
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
            let (ids, error) = match Query::parse(&state.text, state.options) {
                Ok(query) => {
                    let index = self.index.read().expect("index lock");
                    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
                    let mut ids = search(&index, &query, threads);
                    if self.is_current(generation) {
                        sort(&index, &mut ids, state.sort, threads);
                    }
                    (ids, None)
                }
                Err(e) => (Vec::new(), Some(e.to_string())),
            };
            if self.is_current(generation) {
                (self.hooks.on_results)(SearchResult {
                    ids: Arc::new(ids),
                    took: started.elapsed(),
                    error,
                });
            }
        }
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
                    *self.index.write().expect("index lock") = saved.index;
                    loaded = true;
                }
            }
        }
        if !loaded {
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
            let mut store = NodeStore::new();
            scan(&mut store, &roots, &self.options);
            *self.index.write().expect("index lock") = Index::new(store);
            self.dirty.store(true, Ordering::SeqCst);
        }
        self.request_search();

        let watcher = Watcher::new(&self.index.read().expect("index lock"), &self.options);
        let Ok(mut watcher) = watcher else {
            self.report(IndexState::Static);
            return;
        };
        if loaded {
            self.report(IndexState::CatchingUp);
            let stats = watcher.catch_up(&mut self.index.write().expect("index lock"));
            if stats.added + stats.removed + stats.updated > 0 {
                self.dirty.store(true, Ordering::SeqCst);
                self.request_search();
            }
        }
        self.report(IndexState::Watching);
        self.watch_loop(&mut watcher);
    }

    fn watch_loop(&self, watcher: &mut Watcher) {
        let mut last_refresh = Instant::now();
        let mut last_save = Instant::now();
        let mut pending_refresh = false;
        while !self.stop.load(Ordering::SeqCst) {
            match watcher.wait(Some(Duration::from_millis(250))) {
                Ok(events) if !events.is_empty() => {
                    let mut index = self.index.write().expect("index lock");
                    let stats = watcher.apply(&mut index, &events);
                    if index.should_compact() {
                        let remap = watcher.compact(&mut index);
                        let mut shown = self.shown.lock().expect("lock");
                        *shown =
                            Arc::new(shown.iter().filter_map(|id| remap[id.index()]).collect());
                        pending_refresh = true;
                    }
                    if stats.overflow {
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
        let mut store = NodeStore::new();
        scan(&mut store, &roots, &self.options);
        let mut index = self.index.write().expect("index lock");
        *index = Index::new(store);
        if let Ok(mut fresh) = Watcher::new(&index, &self.options) {
            fresh.catch_up(&mut index);
            *watcher = fresh;
        }
        *self.shown.lock().expect("lock") = Arc::new(Vec::new());
    }

    /// Saves the index if it changed since the last save.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let Some(path) = &self.index_file else { return };
        let roots = self.roots.lock().expect("lock").clone();
        let index = self.index.read().expect("index lock");
        if persist::save(&index, &roots, path).is_err() {
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
