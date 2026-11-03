//! The result table model. Rows are built on demand, so only the rows on screen
//! cost anything, however many results there are.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use everygrep_core::details::Details;
use everygrep_core::localtime::Zone;
use std::ops::Range;

use everygrep_core::query::{HighlightTerm, highlight_spans, merge_spans};
use everygrep_core::sort::{FolderSize, Sort};
use everygrep_core::store::NodeId;
use slint::{Model, ModelNotify, ModelTracker, SharedString};

use crate::engine::{Engine, IndexState, SearchResult, SettledSearch};
use crate::filetype::{self, KindIcon};
use crate::format::{self, visible_name};
use crate::hash_columns::{HashBatch, HashColumns, HashReader, HashRequest};
use crate::labels;
use crate::selection::{Changed, Selection};
use crate::{Marked, ResultRow};
use everygrep_core::hashcache::Cached;

/// Rows of the result table, backed by a list of node ids.
pub struct ResultsModel {
    /// Drive labels by root name, shown as `Label (root)` in the Name cell
    /// of a drive's root; empty when that is turned off. Read again with
    /// every result.
    root_labels: RefCell<std::collections::HashMap<Vec<u8>, String>>,
    show_root_labels: Cell<bool>,
    settled: RefCell<Option<SettledSearch>>,
    generation: Cell<u64>,
    engine: Arc<Engine>,
    ids: RefCell<Arc<Vec<NodeId>>>,
    /// The duplicate group number of each row, empty when the rows are not
    /// in groups (see [`SearchResult::groups`]).
    groups: RefCell<Arc<Vec<u32>>>,
    /// Engine epoch of `ids`; when the engine moves on, the ids are stale.
    epoch: Cell<u64>,
    /// Query text of `ids`; a new query starts with an empty selection,
    /// unless `clear_on_search` is off.
    query: RefCell<String>,
    /// Whether a new query clears the selection; off keeps the selected
    /// items that the new result lists.
    clear_on_search: Cell<bool>,
    selection: RefCell<Selection>,
    /// The node whose name is being edited.
    renaming: Cell<Option<NodeId>>,
    terms: RefCell<Arc<Vec<HighlightTerm>>>,
    zone: Zone,
    home: Option<Vec<u8>>,
    now_ns: Cell<i64>,
    state: Cell<IndexState>,
    indexed: Cell<usize>,
    took: Cell<Duration>,
    folder_size: Cell<FolderSize>,
    /// Whether the index shown holds file sizes (see
    /// [`Engine::indexed_file_details`]); without them sizes are empty.
    sizes_known: Cell<bool>,
    /// Whether the status line describes the selected items.
    details: Cell<bool>,
    /// Which of the columns read from the file system are shown.
    detail_columns: Cell<DetailColumns>,
    /// Details read for rows that were on screen, until the next result.
    detail_cache: RefCell<HashMap<NodeId, Details>>,
    /// Reads details off the window's thread; without one they are read
    /// when a row is shown.
    detail_reader: RefCell<Option<DetailReader>>,
    /// Nodes whose details were asked of the reader and did not come yet.
    detail_pending: RefCell<HashSet<NodeId>>,
    /// Which hash columns are shown, the hashes found for rows on screen
    /// (until the next result), the thread that finds them, and the nodes
    /// asked for.
    hash_columns: Cell<HashColumns>,
    hash_cache: RefCell<HashMap<NodeId, Cached>>,
    hash_reader: RefCell<Option<HashReader>>,
    hash_pending: RefCell<HashSet<NodeId>>,
    /// Which frequency columns are shown.
    frequency_columns: Cell<FrequencyColumns>,
    property_columns: RefCell<Vec<bool>>,
    property_cache: RefCell<HashMap<NodeId, Vec<String>>>,
    property_pending: RefCell<HashSet<NodeId>>,
    property_reader: RefCell<Option<crate::properties::Reader>>,
    notify: ModelNotify,
}

/// Which of the Same name, Same extension and Same size columns are shown. The
/// counts are over the whole index (see [`everygrep_core::frequency`]);
/// the engine counts them after each search while one is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrequencyColumns {
    pub name: bool,
    pub extension: bool,
    pub size: bool,
}

impl FrequencyColumns {
    pub fn any(self) -> bool {
        self.name || self.extension || self.size
    }
}

/// Reads the details of rows on screen (Created, Accessed, Attributes) on a
/// thread of its own, so a slow or hanging file system never holds up the
/// window: the rows show those cells empty until their values come (see
/// [`ResultsModel::take_details`]).
pub struct DetailReader {
    requests: mpsc::Sender<DetailRequest>,
    /// Requests made for another list than this one are skipped.
    current: Arc<AtomicU64>,
}

struct DetailRequest {
    list: u64,
    row: usize,
    id: NodeId,
    path: PathBuf,
    name: Vec<u8>,
}

/// Details a [`DetailReader`] read, for [`ResultsModel::take_details`].
pub struct DetailBatch {
    found: Vec<(u64, usize, NodeId, Details)>,
}

/// At most this many rows are read before what was read is passed on.
const DETAIL_BATCH: usize = 64;

impl DetailReader {
    /// Starts the reading thread; `deliver` gets each batch read, on that
    /// thread.
    pub fn start(deliver: impl Fn(DetailBatch) + Send + 'static) -> Self {
        let (requests, asked) = mpsc::channel::<DetailRequest>();
        let current = Arc::new(AtomicU64::new(0));
        let list = Arc::clone(&current);
        let _ = std::thread::Builder::new()
            .name("row-details".into())
            .spawn(move || {
                while let Ok(first) = asked.recv() {
                    let mut found = Vec::new();
                    for r in std::iter::once(first).chain(asked.try_iter().take(DETAIL_BATCH - 1)) {
                        if r.list == list.load(Ordering::SeqCst) {
                            found.push((r.list, r.row, r.id, read_details(&r.path, &r.name)));
                        }
                    }
                    if !found.is_empty() {
                        deliver(DetailBatch { found });
                    }
                }
            });
        Self { requests, current }
    }
}

/// The details of the entry at `path` (named `name`).
fn read_details(path: &Path, name: &[u8]) -> Details {
    #[cfg(test)]
    if tests::SLOW_DETAILS
        .lock()
        .unwrap()
        .iter()
        .any(|p| p == path)
    {
        std::thread::sleep(Duration::from_secs(1));
    }
    Details::read(path, name)
}

/// Frozen rows and selection for a background tab. Node ids are shared, not copied.
#[derive(Clone)]
pub struct ResultSnapshot {
    generation: u64,
    pub settled: Option<SettledSearch>,
    ids: Arc<Vec<NodeId>>,
    groups: Arc<Vec<u32>>,
    epoch: u64,
    query: String,
    selection: Selection,
    terms: Arc<Vec<HighlightTerm>>,
    took: Duration,
}

impl ResultSnapshot {
    /// Number of rows kept.
    #[cfg(test)]
    pub fn rows(&self) -> usize {
        self.ids.len()
    }
}

/// A file action was refused: a drive that is not connected holds some of
/// its items, whose paths may now belong to other files, or to nothing.
#[derive(Debug, PartialEq, Eq)]
pub struct Offline;

/// The items of a file action, as [`ResultsModel::checked_targets`] took
/// them.
#[derive(Debug)]
pub struct Targets {
    ids: Vec<NodeId>,
    epoch: u64,
    pub paths: Vec<PathBuf>,
}

/// The columns whose values are not in the index (see
/// [`everygrep_core::details`]). Nothing is read while all are hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DetailColumns {
    pub created: bool,
    pub accessed: bool,
    pub attributes: bool,
}

impl DetailColumns {
    fn any(self) -> bool {
        self.created || self.accessed || self.attributes
    }
}

/// The parts of the status line: the result count, the details of the
/// selection (shortened first when room runs out), the index and the time
/// the last search took.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub count: String,
    pub detail: String,
    pub index: String,
    pub time: String,
}

/// Details kept for at most this many rows; scrolling far enough starts over.
const MAX_CACHED_DETAILS: usize = 4096;

/// More selected items than this are counted but not summed up by size.
const MAX_SUMMED: usize = 100_000;

impl ResultsModel {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            root_labels: RefCell::default(),
            show_root_labels: Cell::new(true),
            settled: RefCell::new(None),
            generation: Cell::new(0),
            engine,
            ids: RefCell::new(Arc::new(Vec::new())),
            groups: RefCell::new(Arc::new(Vec::new())),
            epoch: Cell::new(u64::MAX),
            query: RefCell::new(String::new()),
            clear_on_search: Cell::new(true),
            selection: RefCell::new(Selection::default()),
            renaming: Cell::new(None),
            terms: RefCell::new(Arc::new(Vec::new())),
            zone: Zone::local(),
            home: std::env::var_os("HOME").map(|h| h.as_bytes().to_vec()),
            now_ns: Cell::new(0),
            state: Cell::new(IndexState::Loading),
            indexed: Cell::new(0),
            took: Cell::new(Duration::ZERO),
            folder_size: Cell::new(FolderSize::default()),
            sizes_known: Cell::new(true),
            details: Cell::new(true),
            detail_columns: Cell::new(DetailColumns::default()),
            detail_cache: RefCell::new(HashMap::new()),
            detail_reader: RefCell::new(None),
            detail_pending: RefCell::new(HashSet::new()),
            hash_columns: Cell::new(HashColumns::default()),
            hash_cache: RefCell::new(HashMap::new()),
            hash_reader: RefCell::new(None),
            hash_pending: RefCell::new(HashSet::new()),
            frequency_columns: Cell::new(FrequencyColumns::default()),
            property_columns: RefCell::new(Vec::new()),
            property_cache: RefCell::new(HashMap::new()),
            property_pending: RefCell::new(HashSet::new()),
            property_reader: RefCell::new(None),
            notify: ModelNotify::default(),
        }
    }

    pub fn freeze(&self) -> ResultSnapshot {
        ResultSnapshot {
            settled: self.settled.borrow().clone(),
            generation: self.generation.get(),
            ids: self.ids.borrow().clone(),
            groups: self.groups.borrow().clone(),
            epoch: self.epoch.get(),
            query: self.query.borrow().clone(),
            selection: self.selection.borrow().clone(),
            terms: self.terms.borrow().clone(),
            took: self.took.get(),
        }
    }

    /// Never show ids from before an index rebuild; the activation search follows.
    pub fn restore(&self, snapshot: Option<&ResultSnapshot>) {
        self.read_root_labels();
        let snapshot = snapshot.filter(|s| s.epoch == self.engine.epoch());
        *self.settled.borrow_mut() = snapshot.and_then(|s| s.settled.clone());
        self.generation.set(snapshot.map_or(0, |s| s.generation));
        *self.ids.borrow_mut() = snapshot.map(|s| s.ids.clone()).unwrap_or_default();
        *self.groups.borrow_mut() = snapshot.map(|s| s.groups.clone()).unwrap_or_default();
        self.epoch.set(snapshot.map_or(u64::MAX, |s| s.epoch));
        *self.query.borrow_mut() = snapshot.map(|s| s.query.clone()).unwrap_or_default();
        *self.selection.borrow_mut() = snapshot.map(|s| s.selection.clone()).unwrap_or_default();
        *self.terms.borrow_mut() = snapshot.map(|s| s.terms.clone()).unwrap_or_default();
        self.took.set(snapshot.map_or(Duration::ZERO, |s| s.took));
        self.renaming.set(None);
        // Details and hashes asked for the rows shown before are read again
        // for these rows; answers still on their way are dropped.
        self.forget_details();
        self.notify.reset();
    }

    /// Whether a new query clears the selection (the default) or keeps the
    /// selected items it lists.
    pub fn read_properties_with(&self, reader: crate::properties::Reader) {
        *self.property_reader.borrow_mut() = Some(reader);
    }
    pub fn set_properties(&self, columns: Vec<bool>) {
        if *self.property_columns.borrow() != columns {
            *self.property_columns.borrow_mut() = columns;
            self.forget_properties();
            self.notify.reset();
        }
    }
    pub fn viewport_changed(&self) {
        self.property_pending.borrow_mut().clear();
        if let Some(r) = self.property_reader.borrow().as_ref() {
            r.cancel();
        }
        self.notify.reset();
    }
    pub fn forget_properties(&self) {
        self.property_cache.borrow_mut().clear();
        self.property_pending.borrow_mut().clear();
        if let Some(r) = self.property_reader.borrow().as_ref() {
            r.cancel();
        }
    }
    fn properties_of(&self, id: NodeId, row: usize) -> Vec<SharedString> {
        if let Some(values) = self.property_cache.borrow().get(&id) {
            return values.iter().map(|v| v.as_str().into()).collect();
        }
        if self.property_columns.borrow().iter().any(|v| *v)
            && let Some(reader) = self.property_reader.borrow().as_ref()
            && !self.property_pending.borrow().contains(&id)
            && reader.request(
                row,
                id,
                self.epoch.get(),
                self.property_columns.borrow().clone(),
            )
        {
            self.property_pending.borrow_mut().insert(id);
        }
        vec![SharedString::new(); everygrep_core::property::Property::ALL.len()]
    }
    pub fn take_properties(&self, batch: crate::properties::Batch) {
        let reader = self.property_reader.borrow();
        let Some(reader) = reader.as_ref() else {
            return;
        };
        if batch.serial != reader.serial() {
            return;
        }
        self.property_pending.borrow_mut().remove(&batch.id);
        if self.property_cache.borrow().len() > 1024 {
            self.property_cache.borrow_mut().clear();
        }
        self.property_cache
            .borrow_mut()
            .insert(batch.id, batch.values);
        if self.ids.borrow().get(batch.row) == Some(&batch.id) {
            self.notify.row_changed(batch.row);
        }
    }

    pub fn set_clear_on_search(&self, on: bool) {
        self.clear_on_search.set(on);
    }

    /// Whether `result` answers the newest request. An older one is not
    /// shown: the newer request's result follows.
    pub fn is_current(&self, result: &SearchResult) -> bool {
        self.engine.is_current(result.generation)
    }

    /// The result order the engine runs (see [`Engine::asked_sort`]).
    pub fn asked_sort(&self) -> Sort {
        self.engine.asked_sort()
    }

    /// Replaces the rows with a search result. The selection stays on the
    /// nodes still in the list when the query is the same (a new sort order or
    /// a refresh after file changes); a new query clears it, or keeps the
    /// selected items it lists when clearing is off. Returns the first
    /// selected row when a selection was kept for a new query.
    pub fn set_result(&self, result: &SearchResult) -> Option<usize> {
        self.read_root_labels();
        *self.settled.borrow_mut() = result.settled.clone();
        self.generation.set(result.generation);
        let mut carried = false;
        *self.terms.borrow_mut() = Arc::clone(&result.terms);
        self.now_ns.set(now_ns());
        self.took.set(result.took);
        {
            let mut selection = self.selection.borrow_mut();
            let mut query = self.query.borrow_mut();
            if self.epoch.get() != result.epoch {
                selection.reset();
                self.renaming.set(None);
            } else if *query != result.query {
                if self.clear_on_search.get() {
                    selection.reset();
                } else {
                    selection.carry_over(&self.ids.borrow(), &result.ids);
                    carried = true;
                }
            } else {
                selection.retain(&result.ids);
            }
            query.clone_from(&result.query);
        }
        self.epoch.set(result.epoch);
        *self.ids.borrow_mut() = Arc::clone(&result.ids);
        *self.groups.borrow_mut() = Arc::clone(&result.groups);
        // A new result also follows file changes, so read the details again.
        self.forget_details();
        self.notify.reset();
        if !carried {
            return None;
        }
        let selection = self.selection.borrow();
        result.ids.iter().position(|&id| selection.contains(id))
    }

    /// Says which columns read from the file system are shown, and shows
    /// the rows again when that changed.
    /// Shows drive labels in the Name cell of drive roots, or not.
    pub fn set_show_root_labels(&self, on: bool) {
        if self.show_root_labels.replace(on) != on {
            self.read_root_labels();
            self.relabel();
        }
    }

    /// Keeps the labels read last while the drive list is busy. A label
    /// comes from the drive itself, so its control and direction marks are
    /// shown visibly, as in names.
    fn read_root_labels(&self) {
        if !self.show_root_labels.get() {
            self.root_labels.borrow_mut().clear();
        } else if let Some(labels) = self.engine.root_labels() {
            *self.root_labels.borrow_mut() = labels
                .into_iter()
                .map(|(root, label)| (root, visible_name(&label).into_owned()))
                .collect();
        }
    }

    pub fn set_detail_columns(&self, columns: DetailColumns) {
        if self.detail_columns.replace(columns) != columns {
            self.relabel();
        }
    }

    /// Says which hash columns are shown, and shows the rows again when
    /// that changed.
    pub fn set_hash_columns(&self, columns: HashColumns) {
        if self.hash_columns.replace(columns) != columns {
            self.forget_hashes();
            self.relabel();
        }
    }

    /// Which frequency columns are shown.
    pub fn frequency_columns(&self) -> FrequencyColumns {
        self.frequency_columns.get()
    }

    /// Says which frequency columns are shown. Showing the first one counts
    /// the index now (the search runs again), so the cells fill in.
    pub fn set_frequency_columns(&self, columns: FrequencyColumns) {
        let before = self.frequency_columns.replace(columns);
        if before == columns {
            return;
        }
        self.engine.set_frequency_columns(columns.any());
        if columns.any() && !before.any() {
            self.engine.refresh();
        }
        self.relabel();
    }

    /// Finds the hashes of rows with `reader` from now on; without one the
    /// hash columns stay empty (files are never read on the window's
    /// thread).
    pub fn read_hashes_with(&self, reader: HashReader) {
        self.forget_hashes();
        *self.hash_reader.borrow_mut() = Some(reader);
    }

    fn forget_hashes(&self) {
        self.hash_cache.borrow_mut().clear();
        self.hash_pending.borrow_mut().clear();
        if let Some(reader) = &*self.hash_reader.borrow() {
            reader.new_list();
        }
    }

    /// The hashes known for the file of `row`; a file not looked at yet is
    /// asked of the reader and has none until they come.
    fn hashes_of(&self, index: &everygrep_core::index::Index, id: NodeId, row: usize) -> Cached {
        if let Some(h) = self.hash_cache.borrow().get(&id) {
            return *h;
        }
        let store = index.store();
        if store.is_dir(id) || index.is_offline(id) {
            return Cached::default();
        }
        if let Some(reader) = &*self.hash_reader.borrow() {
            if self.hash_pending.borrow_mut().insert(id) {
                let request = HashRequest {
                    list: reader.list(),
                    row,
                    id,
                    path: store.path(id),
                    size: store.meta(id).size,
                    columns: self.hash_columns.get(),
                };
                if !reader.ask(request) {
                    self.hash_pending.borrow_mut().remove(&id);
                }
            }
        }
        Cached::default()
    }

    /// Takes hashes the [`HashReader`] found and shows them in their rows;
    /// those found for an older list are dropped.
    pub fn take_hashes(&self, batch: HashBatch) {
        let current = match &*self.hash_reader.borrow() {
            Some(reader) => reader.list(),
            None => return,
        };
        for (list, row, id, hashes) in batch.found {
            if list != current || !self.hash_pending.borrow_mut().remove(&id) {
                continue;
            }
            let mut cache = self.hash_cache.borrow_mut();
            if cache.len() >= MAX_CACHED_DETAILS {
                cache.clear();
            }
            cache.insert(id, hashes);
            drop(cache);
            if self.ids.borrow().get(row) == Some(&id) {
                self.notify.row_changed(row);
            }
        }
    }

    /// Reads the details of rows with `reader` from now on.
    pub fn read_details_with(&self, reader: DetailReader) {
        self.forget_details();
        *self.detail_reader.borrow_mut() = Some(reader);
    }

    /// Drops the details read and asked for: they belong to another list.
    fn forget_details(&self) {
        self.forget_properties();
        self.forget_hashes();
        self.detail_cache.borrow_mut().clear();
        self.detail_pending.borrow_mut().clear();
        if let Some(reader) = &*self.detail_reader.borrow() {
            reader.current.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn cache_details(&self, id: NodeId, d: Details) {
        let mut cache = self.detail_cache.borrow_mut();
        if cache.len() >= MAX_CACHED_DETAILS {
            cache.clear();
        }
        cache.insert(id, d);
    }

    /// Details of the node of `row` for the columns shown, from the cache or
    /// the file system. Entries on a drive that is not connected are not
    /// read. With a [`DetailReader`], a node not read yet is asked of it and
    /// shows no details until they come.
    fn details_of(&self, index: &everygrep_core::index::Index, id: NodeId, row: usize) -> Details {
        if let Some(d) = self.detail_cache.borrow().get(&id) {
            return *d;
        }
        let store = index.store();
        if index.is_offline(id) {
            self.cache_details(id, Details::default());
            return Details::default();
        }
        if let Some(reader) = &*self.detail_reader.borrow() {
            if self.detail_pending.borrow_mut().insert(id) {
                let request = DetailRequest {
                    list: reader.current.load(Ordering::SeqCst),
                    row,
                    id,
                    path: store.path(id),
                    name: store.name(id).to_vec(),
                };
                if reader.requests.send(request).is_err() {
                    self.detail_pending.borrow_mut().remove(&id);
                }
            }
            return Details::default();
        }
        let d = read_details(&store.path(id), store.name(id));
        self.cache_details(id, d);
        d
    }

    /// Takes details a [`DetailReader`] read and shows them in their rows;
    /// those read for an older list are dropped.
    pub fn take_details(&self, batch: DetailBatch) {
        let current = match &*self.detail_reader.borrow() {
            Some(reader) => reader.current.load(Ordering::SeqCst),
            None => return,
        };
        for (list, row, id, d) in batch.found {
            if list != current || !self.detail_pending.borrow_mut().remove(&id) {
                continue;
            }
            self.cache_details(id, d);
            if self.ids.borrow().get(row) == Some(&id) {
                self.notify.row_changed(row);
            }
        }
    }

    /// Changes the selection with `f` and refreshes the rows that changed.
    fn change_selection(&self, f: impl FnOnce(&mut Selection, &[NodeId]) -> Changed) {
        let ids = Arc::clone(&self.ids.borrow());
        let changed = f(&mut self.selection.borrow_mut(), &ids);
        match changed {
            Changed::None => {}
            Changed::Rows(rows) => rows.into_iter().for_each(|r| self.notify.row_changed(r)),
            // One notice for the view, not one per row of a long list: it
            // reads again only the rows on screen.
            Changed::All => self.notify.reset(),
        }
    }

    pub fn select_only(&self, row: usize) {
        self.change_selection(|s, ids| s.select_only(ids, row));
    }

    pub fn toggle(&self, row: usize) {
        self.change_selection(|s, ids| s.toggle(ids, row));
    }

    pub fn extend_to(&self, row: usize) {
        self.change_selection(|s, ids| s.extend_to(ids, row));
    }

    pub fn select_all(&self) {
        self.change_selection(Selection::select_all);
    }

    pub fn invert_selection(&self) {
        self.change_selection(Selection::invert);
    }

    /// Clears the selection; returns whether anything was selected.
    pub fn clear_selection(&self) -> bool {
        let had = self.selected_count() > 0;
        self.change_selection(Selection::clear);
        had
    }

    pub fn is_selected(&self, row: usize) -> bool {
        self.ids
            .borrow()
            .get(row)
            .is_some_and(|&id| self.selection.borrow().contains(id))
    }

    /// The row a jump (see `jump.rs`) lands on from row `current`.
    pub fn jump(&self, look: &crate::jump::Look, current: Option<usize>) -> Option<usize> {
        let ids = self.ids.borrow();
        let index = self.engine.index.read().ok()?;
        if self.engine.epoch() != self.epoch.get() {
            return None;
        }
        let store = index.store();
        crate::jump::find(ids.len(), current, look.from_current, |row| {
            let id = ids[row];
            id.index() < store.len()
                && store.is_live(id)
                && crate::jump::folded(store.name(id)).starts_with(&look.prefix)
        })
    }

    /// The selected ids, in the order shown.
    pub fn selected_ids(&self) -> Vec<NodeId> {
        self.selection.borrow().selected(&self.ids.borrow())
    }

    pub fn selected_count(&self) -> usize {
        self.selection.borrow().count(self.ids.borrow().len())
    }

    pub fn set_renaming(&self, id: Option<NodeId>) {
        self.renaming.set(id);
    }

    /// Current row of the node being renamed, if it is still listed.
    pub fn renaming_row(&self) -> Option<usize> {
        self.renaming.get().and_then(|id| self.row_of(id))
    }

    /// The listed ids and the engine epoch they belong to.
    pub fn snapshot(&self) -> (Arc<Vec<NodeId>>, u64) {
        (Arc::clone(&self.ids.borrow()), self.epoch.get())
    }

    /// Node id of a row.
    pub fn id(&self, row: usize) -> Option<NodeId> {
        self.ids.borrow().get(row).copied()
    }

    /// Row of a node id in the current list.
    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.ids.borrow().iter().position(|&i| i == id)
    }

    /// Whether any of the rows an action on `row` applies to (see
    /// [`targets`](Self::targets)) is on a drive that is not connected.
    pub fn targets_offline(&self, row: Option<usize>) -> bool {
        let picked = self.picked(row);
        let Ok(index) = self.engine.index.read() else {
            return false;
        };
        if index.offline_roots().is_empty() || self.engine.epoch() != self.epoch.get() {
            return false;
        }
        let store = index.store();
        picked
            .into_iter()
            .any(|id| id.index() < store.len() && store.is_live(id) && index.is_offline(id))
    }

    /// The items an action on `row` applies to (see
    /// [`targets`](Self::targets)), checked and taken under one look at the
    /// index: an error when a drive that is not connected holds any of them.
    pub fn checked_targets(&self, row: Option<usize>) -> Result<Targets, Offline> {
        let picked = self.picked(row);
        let epoch = self.epoch.get();
        let mut targets = Targets {
            ids: Vec::new(),
            epoch,
            paths: Vec::new(),
        };
        let Ok(index) = self.engine.index.read() else {
            return Ok(targets);
        };
        if self.engine.epoch() != epoch {
            return Ok(targets);
        }
        let store = index.store();
        targets.ids = picked
            .into_iter()
            .filter(|id| id.index() < store.len() && store.is_live(*id))
            .collect();
        if targets.ids.iter().any(|&id| index.is_offline(id)) {
            return Err(Offline);
        }
        targets.paths = targets.ids.iter().map(|&id| store.path(id)).collect();
        Ok(targets)
    }

    /// The paths of `targets` when their action runs later, after a
    /// confirmation: where the items are now, or an error when a drive
    /// holding any of them was disconnected meanwhile. After a new index
    /// (other ids) the paths taken before are kept, unless they lie on a
    /// drive that is not connected.
    pub fn recheck(&self, targets: Targets) -> Result<Vec<PathBuf>, Offline> {
        let Ok(index) = self.engine.index.read() else {
            return Ok(targets.paths);
        };
        let store = index.store();
        if self.engine.epoch() == targets.epoch {
            let live: Vec<NodeId> = targets
                .ids
                .into_iter()
                .filter(|id| id.index() < store.len() && store.is_live(*id))
                .collect();
            if live.iter().any(|&id| index.is_offline(id)) {
                return Err(Offline);
            }
            return Ok(live.into_iter().map(|id| store.path(id)).collect());
        }
        let offline: Vec<PathBuf> = index
            .offline_roots()
            .iter()
            .filter(|r| r.index() < store.len() && store.is_live(**r))
            .map(|&r| store.path(r))
            .collect();
        if targets
            .paths
            .iter()
            .any(|p| offline.iter().any(|r| p.starts_with(r)))
        {
            return Err(Offline);
        }
        Ok(targets.paths)
    }

    /// The ids of the rows an action on `row` applies to.
    fn picked(&self, row: Option<usize>) -> Vec<NodeId> {
        let ids = Arc::clone(&self.ids.borrow());
        let use_selection = row.is_none_or(|r| self.is_selected(r));
        let picked: Vec<NodeId> = if use_selection {
            self.selection.borrow().selected(&ids)
        } else {
            Vec::new()
        };
        if picked.is_empty() {
            row.and_then(|r| ids.get(r).copied()).into_iter().collect()
        } else {
            picked
        }
    }

    /// The rows an action applies to: the selection when `row` is part of it
    /// (or no row is given), otherwise `row` alone. Paths of nodes that no
    /// longer exist are left out.
    pub fn targets(&self, row: Option<usize>) -> Vec<PathBuf> {
        let picked = self.picked(row);
        let Ok(index) = self.engine.index.read() else {
            return Vec::new();
        };
        if self.engine.epoch() != self.epoch.get() {
            return Vec::new();
        }
        let store = index.store();
        picked
            .into_iter()
            .filter(|id| id.index() < store.len() && store.is_live(*id))
            .map(|id| store.path(id))
            .collect()
    }

    /// Name of a row and whether it is a folder.
    pub fn name(&self, row: usize) -> Option<(String, bool)> {
        let id = self.id(row)?;
        let index = self.engine.index.read().ok()?;
        let store = index.store();
        (self.engine.epoch() == self.epoch.get() && id.index() < store.len() && store.is_live(id))
            .then(|| {
                (
                    String::from_utf8_lossy(store.name(id)).into_owned(),
                    store.is_dir(id),
                )
            })
    }

    /// Build from one index read, retaining raw names and typed metadata.
    /// This never stats a path or parses a rounded, translated cell string.
    pub fn value_query(&self, row: usize, column: i32, case: bool) -> Option<String> {
        use crate::fileops_value::{Value, build};
        let id = self.id(row)?;
        let index = self.engine.index.read().ok()?;
        let store = index.store();
        if self.engine.epoch() != self.epoch.get()
            || id.index() >= store.len()
            || !store.is_live(id)
        {
            return None;
        }
        let name = store.name(id);
        let is_dir = store.is_dir(id);
        let meta = store.meta(id);
        match column {
            0 => build(Value::Name(name), case),
            1 => build(Value::Folder(&store.path(store.parent(id)?)), case),
            2 if self.sizes_known.get()
                && (!is_dir || self.folder_size.get() == FolderSize::Total) =>
            {
                build(Value::Size(meta.size), case)
            }
            3 if meta.mtime_ns != 0 => {
                let (y, m, d) = self
                    .zone
                    .local_date(meta.mtime_ns.div_euclid(1_000_000_000));
                build(Value::Day(y, m, d), case)
            }
            5 if !is_dir => {
                let ext = name
                    .iter()
                    .rposition(|&b| b == b'.')
                    .filter(|&i| i > 0)
                    .map_or(&b""[..], |i| &name[i + 1..]);
                build(Value::Extension(ext), case)
            }
            _ => None,
        }
    }

    pub fn filename_queries(&self, row: usize, case: bool) -> Option<[String; 4]> {
        let id = self.id(row)?;
        let index = self.engine.index.read().ok()?;
        let store = index.store();
        if self.engine.epoch() != self.epoch.get()
            || id.index() >= store.len()
            || !store.is_live(id)
        {
            return None;
        }
        Some(std::array::from_fn(|i| {
            if i >= 2 {
                crate::fileops_value::stem(store.name(id), store.is_dir(id), i % 2 == 1, case)
            } else {
                crate::fileops_value::filename(store.name(id), i % 2 == 1, case)
            }
        }))
    }

    /// What the Size column shows for folders. Rows change with the next
    /// result.
    pub fn set_folder_size(&self, f: FolderSize) {
        self.folder_size.set(f);
    }

    pub fn folder_size(&self) -> FolderSize {
        self.folder_size.get()
    }

    /// Shows the rows again, after the language changed (the Type, Size and
    /// Modified columns have words in them).
    pub fn relabel(&self) {
        // The view reads again only the rows on screen.
        self.notify.reset();
    }

    /// The texts of `column` (numbered as the window's sort column) in the
    /// rows `rows`, as the cells show them; rows past the end are left out.
    pub fn column_texts(&self, column: i32, rows: Range<usize>) -> Vec<String> {
        let end = rows.end.min(self.row_count());
        (rows.start.min(end)..end)
            .filter_map(|r| self.row_data(r))
            .map(|d| match column {
                0 => plain(&d.name),
                1 => plain(&d.folder),
                2 => d.size.into(),
                3 => d.modified.into(),
                4 => d.type_name.into(),
                5 => d.extension.into(),
                6 => d.created.into(),
                7 => d.accessed.into(),
                8 => d.attributes.into(),
                9 => d.md5.into(),
                10 => d.sha256.into(),
                11 => d.crc32.into(),
                12 => d.name_freq.into(),
                13 => d.ext_freq.into(),
                14 => d.size_freq.into(),
                c => d
                    .properties
                    .row_data((c - 15) as usize)
                    .unwrap_or_default()
                    .into(),
            })
            .collect()
    }

    /// Whether the status line describes the selected items.
    pub fn set_details(&self, on: bool) {
        self.details.set(on);
    }

    /// The status line's description of the selection: details of one item,
    /// or the number and size of several. `None` without a selection.
    fn selection_text(&self, selected: usize) -> Option<String> {
        if selected == 0 {
            return None;
        }
        let ids = Arc::clone(&self.ids.borrow());
        let index = self.engine.index.read().ok()?;
        let store = index.store();
        if self.engine.epoch() != self.epoch.get() {
            return None;
        }
        let live = |id: &NodeId| id.index() < store.len() && store.is_live(*id);
        if selected > MAX_SUMMED {
            return Some(labels::selection_summary(selected, None));
        }
        let picked: Vec<NodeId> = self.selection.borrow().selected(&ids);
        if let [id] = picked[..] {
            if !live(&id) {
                return None;
            }
            let meta = store.meta(id);
            let size = size_text(
                store.is_dir(id),
                meta.size,
                store.items(id),
                self.folder_size.get(),
                self.sizes_known.get(),
            );
            let modified = format::timestamp(meta.mtime_ns, &self.zone);
            let path = store.path(id);
            let home = self.home.as_deref().map(String::from_utf8_lossy);
            let path = format::short_path(
                &path.to_string_lossy(),
                home.as_deref(),
                format::STATUS_PATH_MAX,
            );
            return Some(labels::item_details(
                &size,
                &modified,
                &path,
                index.is_offline(id),
            ));
        }
        let (files, bytes) = picked
            .iter()
            .filter(|id| live(id) && !store.is_dir(**id))
            .fold((0, 0u64), |(n, b), &id| (n + 1, b + store.meta(id).size));
        let mut text = labels::selection_summary(selected, Some((files, bytes)));
        if !index.offline_roots().is_empty() {
            let offline = picked
                .iter()
                .filter(|&&id| live(&id) && index.is_offline(id))
                .count();
            if offline > 0 {
                text.push_str(labels::STATUS_GAP);
                text.push_str(&labels::offline_count(offline));
            }
        }
        Some(text)
    }

    pub fn set_state(&self, state: IndexState, indexed: usize) {
        self.state.set(state);
        self.indexed.set(indexed);
        let known = self.engine.indexed_file_details();
        if self.sizes_known.replace(known) != known {
            self.relabel();
        }
    }

    pub fn state(&self) -> IndexState {
        self.state.get()
    }

    /// Full path of a row, if it still exists in the index.
    pub fn path(&self, row: usize) -> Option<PathBuf> {
        let id = *self.ids.borrow().get(row)?;
        let index = self.engine.index.read().ok()?;
        let store = index.store();
        (self.engine.epoch() == self.epoch.get() && id.index() < store.len() && store.is_live(id))
            .then(|| store.path(id))
    }

    /// Texts of the status line.
    pub fn status(&self) -> Status {
        let results = self.ids.borrow().len();
        let mut count = labels::results(results);
        let selected = self.selected_count();
        let detail = if self.details.get() {
            self.selection_text(selected)
        } else {
            None
        };
        if detail.is_none() && selected > 1 {
            count.push_str(labels::STATUS_GAP);
            count.push_str(&labels::selected(selected));
        }
        let mut index = [
            labels::indexed(self.indexed.get()).as_str(),
            labels::state_word(self.state.get()),
        ]
        .join(labels::STATUS_GAP);
        let unwatched = self.engine.unwatched();
        if self.state.get() == IndexState::Watching && unwatched.folders > 0 {
            index.push_str(labels::STATUS_GAP);
            index.push_str(&labels::unwatched(unwatched));
        }
        let ms = self.took.get().as_secs_f64() * 1e3;
        let time = if ms < 10.0 {
            format!("{ms:.1} ms")
        } else {
            format!("{ms:.0} ms")
        };
        Status {
            count,
            detail: detail.unwrap_or_default(),
            index,
            time,
        }
    }

    /// The Folder column: `folder` with the home folder shown as `~`, and
    /// `spans` (ranges of the folder as text) highlighted.
    fn folder_cell(&self, folder: &[u8], spans: &[Range<usize>]) -> Marked {
        let text = String::from_utf8_lossy(folder);
        let home = self.home.as_deref().filter(|home| {
            folder.starts_with(home) && matches!(folder.get(home.len()), None | Some(b'/'))
        });
        match home {
            Some(home) => {
                // The home folder ends where a `/` follows, so its text is a
                // prefix of the folder's text.
                let cut = String::from_utf8_lossy(home).len();
                let shown = format!("~{}", &text[cut..]);
                marked(&shown, &tilde_spans(spans, cut))
            }
            None => marked(&text, spans),
        }
    }
}

impl Model for ResultsModel {
    type Data = ResultRow;

    fn row_count(&self) -> usize {
        self.ids.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<ResultRow> {
        let id = *self.ids.borrow().get(row)?;
        let index = self.engine.index.read().ok()?;
        let store = index.store();
        if self.engine.epoch() != self.epoch.get()
            || id.index() >= store.len()
            || !store.is_live(id)
        {
            // The index changed since this list was made; a fresh one is on its way.
            return Some(ResultRow::default());
        }
        let meta = store.meta(id);
        let is_dir = store.is_dir(id);
        let now = self.now_ns.get();
        let raw_name = store.name(id);
        let kind = filetype::classify(raw_name, is_dir);
        let name = String::from_utf8_lossy(raw_name);
        let extension = labels::extension(&name, is_dir);
        let type_name = labels::type_name(kind, extension).into();
        let extension = extension.into();
        let parent = store.parent(id).map(|p| store.path(p));
        let parent = parent.as_ref().map(|p| p.as_os_str().as_bytes());
        let (mut name_spans, folder_spans) = cell_spans(&name, parent, &self.terms.borrow());
        let name = match self.root_labels.borrow().get(raw_name) {
            Some(label) if parent.is_none() => {
                let (text, spans) = labelled_root(label, &name, &name_spans);
                name_spans = spans;
                std::borrow::Cow::Owned(text)
            }
            _ => name,
        };
        let folder = parent
            .map(|p| self.folder_cell(p, &folder_spans))
            .unwrap_or_default();
        let columns = self.detail_columns.get();
        let details = if columns.any() {
            self.details_of(&index, id, row)
        } else {
            Details::default()
        };
        let hash_columns = self.hash_columns.get();
        let hashes = if hash_columns.any() {
            self.hashes_of(&index, id, row)
        } else {
            Cached::default()
        };
        let shown = self.frequency_columns.get();
        let freq = if shown.any() {
            index.last_frequencies()
        } else {
            None
        };
        let count = |on: bool, n: Option<u32>| match n {
            Some(n) if on && n > 0 => SharedString::from(format::count(n as usize)),
            _ => SharedString::new(),
        };
        let hex = |shown: bool, bytes: Option<&[u8]>| match bytes {
            Some(b) if shown => SharedString::from(everygrep_core::hash::hex(b)),
            _ => SharedString::new(),
        };
        let time = |shown: bool, ns: Option<i64>| match ns {
            Some(ns) if shown => format::date(ns, now, &self.zone).into(),
            _ => SharedString::new(),
        };
        Some(ResultRow {
            name: marked(&name, &name_spans),
            folder,
            size: size_text(
                is_dir,
                meta.size,
                store.items(id),
                self.folder_size.get(),
                self.sizes_known.get(),
            )
            .into(),
            modified: format::date(meta.mtime_ns, now, &self.zone).into(),
            modified_full: format::timestamp(meta.mtime_ns, &self.zone).into(),
            type_name,
            extension,
            created: time(columns.created, details.created_ns),
            accessed: time(columns.accessed, details.accessed_ns),
            attributes: details
                .attributes
                .filter(|_| columns.attributes)
                .map(|a| a.letters().into())
                .unwrap_or_default(),
            md5: hex(hash_columns.md5, hashes.md5.as_ref().map(|h| &h[..])),
            sha256: hex(hash_columns.sha256, hashes.sha256.as_ref().map(|h| &h[..])),
            crc32: hex(
                hash_columns.crc32,
                hashes.crc32.map(u32::to_be_bytes).as_ref().map(|h| &h[..]),
            ),
            name_freq: count(shown.name, freq.as_ref().map(|f| f.name(id))),
            ext_freq: count(shown.extension, freq.as_ref().map(|f| f.extension(id))),
            size_freq: count(shown.size, freq.as_ref().map(|f| f.size(id))),
            properties: slint::ModelRc::new(slint::VecModel::from(self.properties_of(id, row))),
            is_dir,
            kind: kind.icon_index(),
            selected: self.selection.borrow().contains(id),
            offline: index.is_offline(id),
            group: self
                .groups
                .borrow()
                .get(row)
                .map_or(0, |&g| i32::try_from(g).unwrap_or(i32::MAX)),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The text of a marked cell without its marks.
fn plain(m: &Marked) -> String {
    [
        &m.p0, &m.b0, &m.p1, &m.b1, &m.p2, &m.b2, &m.p3, &m.b3, &m.p4,
    ]
    .iter()
    .map(|s| s.as_str())
    .collect()
}

/// A rough width of `text` in tenths of an average character: narrow
/// letters and punctuation count less, wide letters and East Asian
/// characters more. It only picks which texts the window measures.
fn rough_width(text: &str) -> u32 {
    text.chars()
        .map(|c| match c {
            'i' | 'l' | 'j' | 'I' | '.' | ',' | ':' | ';' | '\'' | '!' | '|' | ' ' => 4,
            'f' | 't' | 'r' | '(' | ')' | '[' | ']' | '-' | '/' => 5,
            'm' | 'w' | 'M' | 'W' | '@' | '%' => 13,
            'A'..='Z' => 11,
            '\u{1100}'..='\u{115f}' | '\u{2e80}'..='\u{a4cf}' | '\u{ac00}'..='\u{d7a3}' => 18,
            '\u{f900}'..='\u{faff}' | '\u{fe30}'..='\u{fe4f}' | '\u{ff00}'..='\u{ff60}' => 18,
            _ => 9,
        })
        .sum()
}

/// The `n` texts that look widest, widest first, without repeats; the
/// window measures them to size a column to fit.
pub fn widest(texts: Vec<String>, n: usize) -> Vec<String> {
    let mut texts = texts;
    texts.sort_by_key(|t| std::cmp::Reverse(rough_width(t)));
    let mut out: Vec<String> = Vec::with_capacity(n);
    for t in texts {
        if out.len() == n {
            break;
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Text of the Size column. A folder's size is the total of the files below
/// it. Without `known` sizes (an index without file details) only the
/// number of items is shown.
fn size_text(is_dir: bool, size: u64, items: u32, folders: FolderSize, known: bool) -> String {
    match (is_dir, folders) {
        (false, _) | (true, FolderSize::Total) if !known => String::new(),
        (false, _) | (true, FolderSize::Total) => format::size(size),
        (true, FolderSize::Items) => labels::items(items as usize),
        (true, FolderSize::None) => String::new(),
    }
}

/// Highlighted runs per cell; later ones are shown plain (see `Marked` in
/// `app.slint`).
const MAX_MARKS: usize = 4;

/// Ranges to highlight in a row's name and in its folder (the parent's path
/// as text, `None` for a root). Name terms mark the name; path terms mark
/// the full path, which is split between the two cells.
fn cell_spans(
    name: &str,
    parent: Option<&[u8]>,
    terms: &[HighlightTerm],
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut name_spans = highlight_spans(name, terms, false);
    if !terms.iter().any(|t| t.on_path) {
        return (name_spans, Vec::new());
    }
    // The full path as the matcher sees it (`NodeStore::path`): the parent,
    // a `/` unless the parent ends with one, and the name.
    let folder = parent.map(String::from_utf8_lossy).unwrap_or_default();
    let mut full = folder.clone().into_owned();
    if !full.is_empty() && !full.ends_with('/') {
        full.push('/');
    }
    let name_start = full.len();
    full.push_str(name);
    let mut folder_spans = Vec::new();
    for r in highlight_spans(&full, terms, true) {
        if r.start < folder.len() {
            folder_spans.push(r.start..r.end.min(folder.len()));
        }
        if r.end > name_start {
            name_spans.push(r.start.max(name_start) - name_start..r.end - name_start);
        }
    }
    (merge_spans(name_spans), folder_spans)
}

/// `spans` of a folder's text moved to the text shown with its first `cut`
/// bytes (the home folder) replaced by `~`; a run inside the home folder
/// marks the `~`.
fn tilde_spans(spans: &[Range<usize>], cut: usize) -> Vec<Range<usize>> {
    let start = |i: usize| if i < cut { 0 } else { i - cut + 1 };
    let end = |i: usize| if i <= cut { 1 } else { i - cut + 1 };
    merge_spans(
        spans
            .iter()
            .map(|r| start(r.start)..end(r.end))
            .filter(|r| r.start < r.end)
            .collect(),
    )
}

/// `text` split into plain and bold pieces around `spans` (sorted, not
/// overlapping, on character boundaries).
/// The Name cell of a drive root, `Label (root)`, with the matched spans of
/// the root name moved behind the label.
fn labelled_root(label: &str, name: &str, spans: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    let shift = label.len() + 2;
    let spans = spans
        .iter()
        .map(|r| r.start + shift..r.end + shift)
        .collect();
    (format!("{label} ({name})"), spans)
}

fn marked(text: &str, spans: &[Range<usize>]) -> Marked {
    let mut plain: [&str; MAX_MARKS + 1] = [""; MAX_MARKS + 1];
    let mut bold: [&str; MAX_MARKS] = [""; MAX_MARKS];
    let mut at = 0;
    let mut used = 0;
    for r in spans.iter().take(MAX_MARKS) {
        plain[used] = &text[at..r.start];
        bold[used] = &text[r.start..r.end];
        at = r.end;
        used += 1;
    }
    plain[used] = &text[at..];
    Marked {
        p0: visible_name(plain[0]).as_ref().into(),
        b0: visible_name(bold[0]).as_ref().into(),
        p1: visible_name(plain[1]).as_ref().into(),
        b1: visible_name(bold[1]).as_ref().into(),
        p2: visible_name(plain[2]).as_ref().into(),
        b2: visible_name(bold[2]).as_ref().into(),
        p3: visible_name(plain[3]).as_ref().into(),
        b3: visible_name(bold[3]).as_ref().into(),
        p4: visible_name(plain[4]).as_ref().into(),
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    #[test]
    fn the_widest_looking_texts_are_measured() {
        let texts = ["iiii", "MMMM", "abcd", "MMMM", "日本語", ""]
            .map(String::from)
            .to_vec();
        assert_eq!(widest(texts.clone(), 3), ["日本語", "MMMM", "abcd"]);
        assert_eq!(widest(texts, 10).len(), 5, "repeats are dropped");
        assert!(widest(Vec::new(), 3).is_empty());
        assert!(rough_width("lll") < rough_width("www"));
        assert_eq!(rough_width(""), 0);
    }

    #[test]
    fn marks_are_left_out_of_the_measured_text() {
        let m = Marked {
            p0: "re".into(),
            b0: "port".into(),
            p1: "-20".into(),
            b1: "26".into(),
            p2: ".pdf".into(),
            ..Marked::default()
        };
        assert_eq!(plain(&m), "report-2026.pdf");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use everygrep_core::query::{Query, QueryOptions};

    #[test]
    fn a_drive_root_shows_its_label_with_the_matches_moved() {
        let (text, spans) = labelled_root("USB DISK", "/run/media/u/USB", &[11..14, 0..1]);
        assert_eq!(text, "USB DISK (/run/media/u/USB)");
        assert_eq!(&text[spans[0].clone()], "u/U");
        assert_eq!(&text[spans[1].clone()], "/");
    }

    /// Paths whose details take a second to read, as on a file system that
    /// hardly answers.
    pub(super) static SLOW_DETAILS: std::sync::Mutex<Vec<PathBuf>> =
        std::sync::Mutex::new(Vec::new());

    pub(crate) fn slow_details(path: &Path) {
        SLOW_DETAILS.lock().unwrap().push(path.to_path_buf());
    }

    fn terms(q: &str) -> Vec<HighlightTerm> {
        Query::parse(q, QueryOptions::default())
            .unwrap()
            .highlight_terms()
    }

    /// The pieces of a marked text, without the empty ones at the end.
    fn pieces(m: &Marked) -> Vec<String> {
        let mut v: Vec<String> = [
            &m.p0, &m.b0, &m.p1, &m.b1, &m.p2, &m.b2, &m.p3, &m.b3, &m.p4,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        while v.last().is_some_and(String::is_empty) {
            v.pop();
        }
        v
    }

    fn pairs(spans: &[Range<usize>]) -> Vec<(usize, usize)> {
        spans.iter().map(|r| (r.start, r.end)).collect()
    }

    fn name_pieces(name: &str, q: &str) -> Vec<String> {
        let (spans, _) = cell_spans(name, Some(b"/x"), &terms(q));
        pieces(&marked(name, &spans))
    }

    #[test]
    fn control_characters_are_visible_without_changing_match_offsets() {
        assert_eq!(visible_name("yeni\nsətir.txt"), "yeni⏎sətir.txt");
        assert_eq!(visible_name("a\tb\rc\x1bd\x7f"), "a⇥b␍c␛d␡");
        assert_eq!(visible_name("x\u{85}\u{2028}\u{2029}"), "x\\u{85}⏎⏎");
        assert_eq!(visible_name("normal-ş.txt"), "normal-ş.txt");
        let m = marked("a\nş.txt", std::slice::from_ref(&(2..4)));
        assert_eq!(m.p0, "a⏎");
        assert_eq!(m.b0, "ş");
        assert_eq!(m.p1, ".txt");
        let controls: String = (0..=0x9f).filter_map(char::from_u32).collect();
        assert!(!visible_name(&controls).chars().any(char::is_control));
    }

    #[test]
    fn folder_size_column_follows_the_setting() {
        let gb = 3 << 30;
        assert_eq!(size_text(true, gb, 12, FolderSize::Total, true), "3.0 GB");
        assert_eq!(size_text(true, gb, 12, FolderSize::Items, true), "12 items");
        assert_eq!(size_text(true, gb, 1, FolderSize::Items, true), "1 item");
        assert_eq!(size_text(true, gb, 0, FolderSize::Items, true), "0 items");
        assert_eq!(size_text(true, gb, 12, FolderSize::None, true), "");
        for f in FolderSize::ALL {
            assert_eq!(size_text(false, 512, 0, f, true), "512 B");
        }
        assert_eq!(
            size_text(true, 0, 12_345, FolderSize::Items, true),
            "12\u{2009}345 items"
        );
        // An index without file details: no sizes, only item counts.
        for f in FolderSize::ALL {
            assert_eq!(size_text(false, 0, 0, f, false), "");
        }
        assert_eq!(size_text(true, 0, 12, FolderSize::Total, false), "");
        assert_eq!(size_text(true, 0, 12, FolderSize::Items, false), "12 items");
    }

    #[test]
    fn names_are_marked_at_every_match() {
        assert_eq!(
            name_pieces("Report-2026.pdf", "2026"),
            ["Report-", "2026", ".pdf"]
        );
        assert_eq!(name_pieces("Report.pdf", "rep"), ["", "Rep", "ort.pdf"]);
        assert_eq!(name_pieces("Report.pdf", "!rep"), ["Report.pdf"]);
        assert_eq!(
            name_pieces("rep-a-rep.pdf", "rep pdf"),
            ["", "rep", "-a-", "rep", ".", "pdf"]
        );
        // Replacement characters for invalid bytes keep offsets valid.
        let lossy = String::from_utf8_lossy(b"a\xffb.txt");
        assert_eq!(name_pieces(&lossy, "b.t"), ["a\u{fffd}", "b.t", "xt"]);
    }

    #[test]
    fn runs_after_the_fourth_stay_plain() {
        let m = marked(
            "a1a2a3a4a5a",
            &highlight_spans("a1a2a3a4a5a", &terms("a"), false),
        );
        assert_eq!(pieces(&m), ["", "a", "1", "a", "2", "a", "3", "a", "4a5a"]);
    }

    #[test]
    fn path_terms_mark_the_folder_and_the_name() {
        let t = terms("path:src main");
        let (name, folder) = cell_spans("main-src.rs", Some(b"/home/me/src"), &t);
        assert_eq!(
            pieces(&marked("main-src.rs", &name)),
            ["", "main", "-", "src", ".rs"]
        );
        assert_eq!(
            pieces(&marked("/home/me/src", &folder)),
            ["/home/me/", "src"]
        );
        // A run across the `/` marks both sides, not the `/`.
        let (name, folder) = cell_spans("src.rs", Some(b"/a/b"), &terms("b/src"));
        assert_eq!((pairs(&name), pairs(&folder)), (vec![(0, 3)], vec![(3, 4)]));
        // Children of `/` and roots.
        let (name, folder) = cell_spans("srv", Some(b"/"), &terms("path:/s"));
        assert_eq!((pairs(&name), pairs(&folder)), (vec![(0, 1)], vec![(0, 1)]));
        let (name, folder) = cell_spans("/srv", None, &terms("path:srv"));
        assert_eq!((pairs(&name), pairs(&folder)), (vec![(1, 4)], vec![]));
        // Without path terms nothing in the folder is marked.
        let (_, folder) = cell_spans("src", Some(b"/src"), &terms("src"));
        assert!(folder.is_empty());
    }

    fn tilde(spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let spans: Vec<Range<usize>> = spans.iter().map(|&(a, b)| a..b).collect();
        pairs(&tilde_spans(&spans, 8))
    }

    #[test]
    fn marks_in_the_home_folder_move_to_the_tilde() {
        // "/home/me" (8 bytes) is shown as "~".
        assert_eq!(tilde(&[(9, 12)]), [(2, 5)]);
        assert_eq!(tilde(&[(1, 5)]), [(0, 1)]);
        assert_eq!(tilde(&[(6, 12)]), [(0, 5)]);
        assert_eq!(tilde(&[(1, 3), (4, 6)]), [(0, 1)]);
        assert_eq!(tilde(&[(8, 9)]), [(1, 2)]);
    }
}
