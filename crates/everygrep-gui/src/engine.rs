//! The engine side of the window: loading the index, searching on a worker
//! thread, applying file changes on a watcher thread, and taking removable
//! drives in and out.
//!
//! The index lives behind a read-write lock. Searches and the result table take
//! short read locks; the watcher takes the write lock only while it applies a
//! batch of changes, never while it waits for them.
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

/// A drive read for the first time on a helper thread, waiting to be added.
struct Scanned {
    drive: Drive,
    index: Index,
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
    /// Removable drives: where to look (`None` turns them off), the
    /// settings, the known drives, requests from the window, drives read on
    /// helper threads, and whether the mount table changed.
    drive_env: Option<DriveEnv>,
    drive_settings: Mutex<DriveSettings>,
    volumes: Mutex<Vec<Volume>>,
    drive_commands: Mutex<Vec<DriveCommand>>,
    scanned: Arc<Mutex<Vec<Scanned>>>,
    scanning: Mutex<HashSet<String>>,
    mounts_changed: Arc<AtomicBool>,
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
    /// `exclude` are not entered by scans. With `drives`, removable drives
    /// found there are indexed as the settings say.
    pub fn start(
        roots: Vec<PathBuf>,
        exclude: Vec<PathBuf>,
        index_file: Option<PathBuf>,
        drives: Option<(DriveEnv, DriveSettings)>,
        hooks: Hooks,
    ) -> Arc<Self> {
        let (drive_env, drive_settings) = match drives {
            Some((env, settings)) => (Some(env), settings),
            None => (None, DriveSettings::default()),
        };
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
            drive_env,
            drive_settings: Mutex::new(drive_settings),
            volumes: Mutex::new(Vec::new()),
            drive_commands: Mutex::new(Vec::new()),
            scanned: Arc::new(Mutex::new(Vec::new())),
            scanning: Mutex::new(HashSet::new()),
            mounts_changed: Arc::new(AtomicBool::new(false)),
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
        if let Some(env) = &engine.drive_env {
            let path = env.mountinfo.clone();
            let e = Arc::clone(&engine);
            // Sleeps in poll until the mount table changes; ends with the
            // process like the index thread.
            let _ = std::thread::Builder::new()
                .name("mounts".into())
                .spawn(move || e.mount_loop(&path));
        }
        engine
    }

    fn mount_loop(&self, path: &Path) {
        let Ok(mut watch) = MountWatch::new(path) else {
            return;
        };
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
        self.load_drives();
        let (_, mut drives_to_catch_up) = self.sync_drives(None);
        self.request_search();

        let mut catch_up = loaded;
        while !self.stop.load(Ordering::SeqCst) {
            let watcher = Watcher::new(
                &self.index.read().expect("index lock"),
                &self.scan_options(),
            );
            match watcher {
                Ok(mut watcher) => {
                    if catch_up || !drives_to_catch_up.is_empty() {
                        self.report(IndexState::CatchingUp);
                        let mut index = self.index.write().expect("index lock");
                        let stats = if catch_up {
                            watcher.catch_up(&mut index)
                        } else {
                            watcher.catch_up_roots(&mut index, &drives_to_catch_up)
                        };
                        drop(index);
                        if stats.added + stats.removed + stats.updated > 0 {
                            self.dirty.store(true, Ordering::SeqCst);
                            self.request_search();
                        }
                    }
                    drives_to_catch_up.clear();
                    self.report(IndexState::Watching);
                    self.watch_loop(&mut watcher);
                }
                Err(_) => {
                    self.report(IndexState::Static);
                    while !self.stop.load(Ordering::SeqCst) && !self.rescan.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(250));
                        if self.mounts_changed.swap(false, Ordering::SeqCst)
                            && self.sync_drives(None).0
                        {
                            self.dirty.store(true, Ordering::SeqCst);
                            self.request_search();
                            self.report(IndexState::Static);
                        }
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
        let fresh = self.with_drives(Index::new(store));
        self.replace_index(fresh);
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Applies file changes until the engine stops or a rescan is asked for.
    fn watch_loop(&self, watcher: &mut Watcher) {
        let mut last_refresh = Instant::now();
        let mut last_save = Instant::now();
        let mut pending_refresh = false;
        while !self.stop.load(Ordering::SeqCst) && !self.rescan.load(Ordering::SeqCst) {
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
        let fresh = self.with_drives(Index::new(store));
        let mut index = self.index.write().expect("index lock");
        *index = fresh;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut fresh) = Watcher::new(&index, &options) {
            fresh.catch_up(&mut index);
            *watcher = fresh;
        }
        *self.shown.lock().expect("lock") = Arc::new(Vec::new());
    }

    /// Saves the index if it changed since the last save: the folders to
    /// index to the index file, each connected drive to its own file.
    /// Drives that are not connected were saved when they went away.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let roots = self.roots.lock().expect("lock").clone();
        let mut volumes = self.volumes.lock().expect("lock");
        let index = self.index.read().expect("index lock");
        let drive_roots: Vec<NodeId> = volumes
            .iter()
            .filter_map(|v| root_id(&index, v.root.as_deref()?))
            .collect();
        let mut ok = true;
        if let Some(path) = &self.index_file {
            ok = if drive_roots.is_empty() {
                persist::save(&index, &roots, path).is_ok()
            } else {
                let own: Vec<NodeId> = index
                    .roots()
                    .into_iter()
                    .filter(|r| !drive_roots.contains(r))
                    .collect();
                persist::save(&index.extract(&own).0, &roots, path).is_ok()
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
        let mut volumes = self.volumes.lock().expect("lock");
        for mut info in drives::known_drives(env) {
            let mut root = None;
            if !ignored.contains(&info.uuid) {
                if let Ok(saved) = persist::load(&env.index_path(&info.uuid)) {
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
        for Scanned { drive, index: new } in scanned {
            self.scanning.lock().expect("lock").remove(&drive.uuid);
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
            } else if settings.auto || env.info_path(&drive.uuid).exists() {
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
        let _ = std::thread::Builder::new()
            .name("drive scan".into())
            .spawn(move || {
                let mut store = NodeStore::new();
                scan(
                    &mut store,
                    std::slice::from_ref(&drive.mount_point),
                    &options,
                );
                let index = Index::new(store);
                scanned.lock().expect("lock").push(Scanned { drive, index });
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
    persist::save(&drive, &[name.to_path_buf()], &env.index_path(&v.info.uuid))?;
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

    /// Names of the results for `query`, once the engine reports `state`
    /// with the index in place.
    struct Probe {
        engine: Arc<Engine>,
        states: mpsc::Receiver<IndexState>,
        results: mpsc::Receiver<SearchResult>,
    }

    impl Probe {
        fn start(roots: Vec<PathBuf>, exclude: Vec<PathBuf>, index: PathBuf) -> Self {
            Self::start_with(roots, exclude, index, None)
        }

        fn start_with(
            roots: Vec<PathBuf>,
            exclude: Vec<PathBuf>,
            index: PathBuf,
            drives: Option<(DriveEnv, DriveSettings)>,
        ) -> Self {
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
                engine: Engine::start(roots, exclude, Some(index), drives, hooks),
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
            fs::create_dir_all(dir.join("mnt/D")).unwrap();
            fs::write(dir.join("mnt/D/windows-file.txt"), "").unwrap();
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
            fake.link("WINDOWS-D", "nvme0n1p5");
            fake.add_line(&format!(
                "40 1 259:5 / {} ro - ntfs3 {} ro",
                dir.join("mnt/D").display(),
                dir.join("dev/nvme0n1p5").display()
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
        assert!(probe.names("windows-file").is_empty());

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
