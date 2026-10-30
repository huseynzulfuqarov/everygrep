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
use everygrep_core::query::{NameTerm, Query, QueryOptions};
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
    /// Text terms of the query, for highlighting matches in names.
    pub terms: Arc<Vec<NameTerm>>,
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
}

/// Shared engine state.
pub struct Engine {
    pub index: RwLock<Index>,
    roots: Mutex<Vec<PathBuf>>,
    options: Mutex<ScanOptions>,
    /// Set by [`Engine::reindex`]; the index thread scans again when it sees
    /// it.
    rescan: AtomicBool,
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
    /// Creates the engine and starts loading in the background. Folders in
    /// `exclude` are not entered by scans.
    pub fn start(
        roots: Vec<PathBuf>,
        exclude: Vec<PathBuf>,
        index_file: Option<PathBuf>,
        hooks: Hooks,
    ) -> Arc<Self> {
        let engine = Arc::new(Self {
            index: RwLock::new(Index::new(NodeStore::new())),
            roots: Mutex::new(normalize_roots(&roots)),
            options: Mutex::new(ScanOptions {
                exclude,
                ..ScanOptions::default()
            }),
            rescan: AtomicBool::new(false),
            index_file: index_file.or_else(persist::default_path),
            generation: AtomicU64::new(0),
            query: Mutex::new(QueryState::default()),
            shown: Mutex::new(Arc::new(Vec::new())),
            epoch: AtomicU64::new(0),
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

    /// The folders the index covers. Empty until the index is loaded or the
    /// first scan has started.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().expect("lock").clone()
    }

    /// Indexes `roots` again from scratch, without entering `exclude`, in the
    /// background. Empty `roots` stand for the home folder. The status hook
    /// reports the scan as for a first start; the new index is saved as soon
    /// as it is complete.
    pub fn reindex(&self, roots: &[PathBuf], exclude: Vec<PathBuf>) {
        *self.roots.lock().expect("lock") = normalize_roots(roots);
        self.options.lock().expect("lock").exclude = exclude;
        self.rescan.store(true, Ordering::SeqCst);
    }

    fn scan_options(&self) -> ScanOptions {
        self.options.lock().expect("lock").clone()
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
                    terms = query.name_terms();
                    let index = self.index.read().expect("index lock");
                    epoch = self.epoch();
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
                    terms: Arc::new(terms),
                    took: started.elapsed(),
                    error,
                    query: state.text,
                    epoch,
                });
            }
        }
    }

    /// Swaps in a new index; ids of the old one become invalid.
    fn replace_index(&self, new: Index) {
        let mut index = self.index.write().expect("index lock");
        *index = new;
        self.epoch.fetch_add(1, Ordering::SeqCst);
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
                    self.replace_index(saved.index);
                    loaded = true;
                }
            }
        }
        if !loaded {
            self.scan_fresh();
        }
        self.request_search();

        let mut catch_up = loaded;
        while !self.stop.load(Ordering::SeqCst) {
            let watcher = Watcher::new(
                &self.index.read().expect("index lock"),
                &self.scan_options(),
            );
            match watcher {
                Ok(mut watcher) => {
                    if catch_up {
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
                Err(_) => {
                    self.report(IndexState::Static);
                    while !self.stop.load(Ordering::SeqCst) && !self.rescan.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            if self.rescan.swap(false, Ordering::SeqCst) && !self.stop.load(Ordering::SeqCst) {
                self.scan_fresh();
                *self.shown.lock().expect("lock") = Arc::new(Vec::new());
                self.request_search();
                self.save();
                catch_up = false;
            }
        }
    }

    /// Builds the index from scratch over the current roots (the home
    /// folder when there are none) and swaps it in.
    fn scan_fresh(&self) {
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
        scan(&mut store, &roots, &self.scan_options());
        self.replace_index(Index::new(store));
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Applies file changes until the engine stops or a rescan is asked for.
    fn watch_loop(&self, watcher: &mut Watcher) {
        let mut last_refresh = Instant::now();
        let mut last_save = Instant::now();
        let mut pending_refresh = false;
        while !self.stop.load(Ordering::SeqCst) && !self.rescan.load(Ordering::SeqCst) {
            match watcher.wait(Some(Duration::from_millis(250))) {
                Ok(events) if !events.is_empty() => {
                    let mut index = self.index.write().expect("index lock");
                    let stats = watcher.apply(&mut index, &events);
                    if index.should_compact() {
                        let remap = watcher.compact(&mut index);
                        self.epoch.fetch_add(1, Ordering::SeqCst);
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
        let options = self.scan_options();
        let mut store = NodeStore::new();
        scan(&mut store, &roots, &options);
        let mut index = self.index.write().expect("index lock");
        *index = Index::new(store);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut fresh) = Watcher::new(&index, &options) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;
    use std::fs;

    /// Names of the results for `query`, once the engine reports `state`
    /// with the index in place.
    struct Probe {
        engine: Arc<Engine>,
        states: mpsc::Receiver<IndexState>,
        results: mpsc::Receiver<SearchResult>,
    }

    impl Probe {
        fn start(roots: Vec<PathBuf>, exclude: Vec<PathBuf>, index: PathBuf) -> Self {
            let (state_tx, states) = mpsc::channel();
            let (result_tx, results) = mpsc::channel();
            let state_tx = Mutex::new(state_tx);
            let result_tx = Mutex::new(result_tx);
            let hooks = Hooks {
                on_results: Box::new(move |r| {
                    let _ = result_tx.lock().unwrap().send(r);
                }),
                on_state: Box::new(move |s, _| {
                    let _ = state_tx.lock().unwrap().send(s);
                }),
            };
            Self {
                engine: Engine::start(roots, exclude, Some(index), hooks),
                states,
                results,
            }
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
            .reindex(&[root("a"), root("b")], vec![root("a/skip")]);
        probe.wait_for(IndexState::Scanning);
        probe.wait_for(IndexState::Watching);
        assert_eq!(probe.names("txt"), ["one.txt", "three.txt"]);
        // The excluded folder itself is still listed.
        assert_eq!(probe.names("skip"), ["skip"]);
        let mut roots = probe.engine.roots();
        roots.sort();
        let canonical = fs::canonicalize(&dir.0).unwrap();
        assert_eq!(roots, [canonical.join("a"), canonical.join("b")]);
        // The new index was saved at once, with its roots.
        let saved = persist::load(&index).unwrap();
        assert_eq!(saved.roots, roots);
        probe.engine.shutdown();
    }
}
