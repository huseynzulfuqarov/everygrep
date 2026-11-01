//! The result table model. Rows are built on demand, so only the rows on screen
//! cost anything, however many results there are.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use everygrep_core::details::Details;
use everygrep_core::localtime::Zone;
use std::ops::Range;

use everygrep_core::query::{HighlightTerm, highlight_spans, merge_spans};
use everygrep_core::sort::FolderSize;
use everygrep_core::store::NodeId;
use slint::{Model, ModelNotify, ModelTracker, SharedString};

use crate::engine::{Engine, IndexState, SearchResult};
use crate::filetype::{self, KindIcon};
use crate::format;
use crate::labels;
use crate::selection::{Changed, Selection};
use crate::{Marked, ResultRow};

/// Rows of the result table, backed by a list of node ids.
pub struct ResultsModel {
    engine: Arc<Engine>,
    ids: RefCell<Arc<Vec<NodeId>>>,
    /// Engine epoch of `ids`; when the engine moves on, the ids are stale.
    epoch: Cell<u64>,
    /// Query text of `ids`; a new query starts with an empty selection.
    query: RefCell<String>,
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
    /// Whether the status line describes the selected items.
    details: Cell<bool>,
    /// Which of the columns read from the file system are shown.
    detail_columns: Cell<DetailColumns>,
    /// Details read for rows that were on screen, until the next result.
    detail_cache: RefCell<HashMap<NodeId, Details>>,
    notify: ModelNotify,
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
            engine,
            ids: RefCell::new(Arc::new(Vec::new())),
            epoch: Cell::new(u64::MAX),
            query: RefCell::new(String::new()),
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
            details: Cell::new(true),
            detail_columns: Cell::new(DetailColumns::default()),
            detail_cache: RefCell::new(HashMap::new()),
            notify: ModelNotify::default(),
        }
    }

    /// Replaces the rows with a search result. The selection stays on the
    /// nodes still in the list when the query is the same (a new sort order or
    /// a refresh after file changes); a new query clears it.
    pub fn set_result(&self, result: &SearchResult) {
        *self.terms.borrow_mut() = Arc::clone(&result.terms);
        self.now_ns.set(now_ns());
        self.took.set(result.took);
        self.engine.set_shown(Arc::clone(&result.ids));
        {
            let mut selection = self.selection.borrow_mut();
            let mut query = self.query.borrow_mut();
            if self.epoch.get() != result.epoch {
                selection.reset();
                self.renaming.set(None);
            } else if *query != result.query {
                selection.reset();
            } else {
                selection.retain(&result.ids);
            }
            query.clone_from(&result.query);
        }
        self.epoch.set(result.epoch);
        *self.ids.borrow_mut() = Arc::clone(&result.ids);
        // A new result also follows file changes, so read the details again.
        self.detail_cache.borrow_mut().clear();
        self.notify.reset();
    }

    /// Says which columns read from the file system are shown, and shows
    /// the rows again when that changed.
    pub fn set_detail_columns(&self, columns: DetailColumns) {
        if self.detail_columns.replace(columns) != columns {
            self.relabel();
        }
    }

    /// Details of a node for the columns shown, from the cache or the file
    /// system. Entries on a drive that is not connected are not read.
    fn details_of(&self, index: &everygrep_core::index::Index, id: NodeId) -> Details {
        if let Some(d) = self.detail_cache.borrow().get(&id) {
            return *d;
        }
        let store = index.store();
        let d = if index.is_offline(id) {
            Details::default()
        } else {
            Details::read(&store.path(id), store.name(id))
        };
        let mut cache = self.detail_cache.borrow_mut();
        if cache.len() >= MAX_CACHED_DETAILS {
            cache.clear();
        }
        cache.insert(id, d);
        d
    }

    /// Changes the selection with `f` and refreshes the rows that changed.
    fn change_selection(&self, f: impl FnOnce(&mut Selection, &[NodeId]) -> Changed) {
        let ids = Arc::clone(&self.ids.borrow());
        let changed = f(&mut self.selection.borrow_mut(), &ids);
        match changed {
            Changed::None => {}
            Changed::Rows(rows) => rows.into_iter().for_each(|r| self.notify.row_changed(r)),
            // Each call only touches the rows on screen, so this stays cheap.
            Changed::All => (0..ids.len()).for_each(|r| self.notify.row_changed(r)),
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
        let rows = self.ids.borrow().len();
        // Each call only touches the rows on screen, so this stays cheap.
        (0..rows).for_each(|r| self.notify.row_changed(r));
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
        let index = [
            labels::indexed(self.indexed.get()).as_str(),
            labels::state_word(self.state.get()),
        ]
        .join(labels::STATUS_GAP);
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
        let (name_spans, folder_spans) = cell_spans(&name, parent, &self.terms.borrow());
        let folder = parent
            .map(|p| self.folder_cell(p, &folder_spans))
            .unwrap_or_default();
        let columns = self.detail_columns.get();
        let details = if columns.any() {
            self.details_of(&index, id)
        } else {
            Details::default()
        };
        let time = |shown: bool, ns: Option<i64>| match ns {
            Some(ns) if shown => format::date(ns, now, &self.zone).into(),
            _ => SharedString::new(),
        };
        Some(ResultRow {
            name: marked(&name, &name_spans),
            folder,
            size: size_text(is_dir, meta.size, store.items(id), self.folder_size.get()).into(),
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
            is_dir,
            kind: kind.icon_index(),
            selected: self.selection.borrow().contains(id),
            offline: index.is_offline(id),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Text of the Size column. A folder's size is the total of the files below
/// it.
fn size_text(is_dir: bool, size: u64, items: u32, folders: FolderSize) -> String {
    match (is_dir, folders) {
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
        p0: plain[0].into(),
        b0: bold[0].into(),
        p1: plain[1].into(),
        b1: bold[1].into(),
        p2: plain[2].into(),
        b2: bold[2].into(),
        p3: plain[3].into(),
        b3: bold[3].into(),
        p4: plain[4].into(),
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::query::{Query, QueryOptions};

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
    fn folder_size_column_follows_the_setting() {
        let gb = 3 << 30;
        assert_eq!(size_text(true, gb, 12, FolderSize::Total), "3.0 GB");
        assert_eq!(size_text(true, gb, 12, FolderSize::Items), "12 items");
        assert_eq!(size_text(true, gb, 1, FolderSize::Items), "1 item");
        assert_eq!(size_text(true, gb, 0, FolderSize::Items), "0 items");
        assert_eq!(size_text(true, gb, 12, FolderSize::None), "");
        for f in FolderSize::ALL {
            assert_eq!(size_text(false, 512, 0, f), "512 B");
        }
        assert_eq!(
            size_text(true, 0, 12_345, FolderSize::Items),
            "12\u{2009}345 items"
        );
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
