//! The result table model. Rows are built on demand, so only the rows on screen
//! cost anything, however many results there are.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use everygrep_core::localtime::Zone;
use everygrep_core::query::{NameTerm, match_span};
use everygrep_core::sort::FolderSize;
use everygrep_core::store::NodeId;
use slint::{Model, ModelNotify, ModelTracker, SharedString};

use crate::ResultRow;
use crate::engine::{Engine, IndexState, SearchResult};
use crate::filetype::{self, KindIcon};
use crate::format;
use crate::labels;
use crate::selection::{Changed, Selection};

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
    terms: RefCell<Arc<Vec<NameTerm>>>,
    zone: Zone,
    home: Option<Vec<u8>>,
    now_ns: Cell<i64>,
    state: Cell<IndexState>,
    indexed: Cell<usize>,
    took: Cell<Duration>,
    folder_size: Cell<FolderSize>,
    /// Whether the status line describes the selected items.
    details: Cell<bool>,
    notify: ModelNotify,
}

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
        self.notify.reset();
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

    /// The rows an action applies to: the selection when `row` is part of it
    /// (or no row is given), otherwise `row` alone. Paths of nodes that no
    /// longer exist are left out.
    pub fn targets(&self, row: Option<usize>) -> Vec<PathBuf> {
        let ids = Arc::clone(&self.ids.borrow());
        let use_selection = row.is_none_or(|r| self.is_selected(r));
        let picked: Vec<NodeId> = if use_selection {
            self.selection.borrow().selected(&ids)
        } else {
            Vec::new()
        };
        let picked = if picked.is_empty() {
            row.and_then(|r| ids.get(r).copied()).into_iter().collect()
        } else {
            picked
        };
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
            return Some(labels::item_details(
                &size,
                &modified,
                &path.to_string_lossy(),
            ));
        }
        let (files, bytes) = picked
            .iter()
            .filter(|id| live(id) && !store.is_dir(**id))
            .fold((0, 0u64), |(n, b), &id| (n + 1, b + store.meta(id).size));
        Some(labels::selection_summary(selected, Some((files, bytes))))
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

    /// Text for the left and right side of the status line.
    pub fn status(&self) -> (String, String) {
        let results = self.ids.borrow().len();
        let indexed = labels::indexed(self.indexed.get());
        let state = labels::state_word(self.state.get());
        let mut left = labels::results(results);
        let selected = self.selected_count();
        let described = if self.details.get() {
            self.selection_text(selected)
        } else {
            None
        };
        if let Some(text) = described {
            left.push_str(labels::STATUS_GAP);
            left.push_str(&text);
        } else if selected > 1 {
            left.push_str(labels::STATUS_GAP);
            left.push_str(&labels::selected(selected));
        }
        for part in [indexed.as_str(), state] {
            left.push_str(labels::STATUS_GAP);
            left.push_str(part);
        }
        let ms = self.took.get().as_secs_f64() * 1e3;
        let right = if ms < 10.0 {
            format!("{ms:.1} ms")
        } else {
            format!("{ms:.0} ms")
        };
        (left, right)
    }

    fn folder_text(&self, folder: &[u8]) -> SharedString {
        let shown = match &self.home {
            Some(home)
                if folder.starts_with(home)
                    && matches!(folder.get(home.len()), None | Some(b'/')) =>
            {
                let mut s = b"~".to_vec();
                s.extend_from_slice(&folder[home.len()..]);
                String::from_utf8_lossy(&s).into_owned()
            }
            _ => String::from_utf8_lossy(folder).into_owned(),
        };
        shown.into()
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
        let folder = store
            .parent(id)
            .map(|p| self.folder_text(store.path(p).as_os_str().as_bytes()))
            .unwrap_or_default();
        let now = self.now_ns.get();
        let raw_name = store.name(id);
        let kind = filetype::classify(raw_name, is_dir);
        let name = String::from_utf8_lossy(raw_name);
        let extension = labels::extension(&name, is_dir);
        let type_name = labels::type_name(kind, extension).into();
        let extension = extension.into();
        let (before, matched, after) = split_name(&name, &self.terms.borrow());
        Some(ResultRow {
            name_before: before.into(),
            name_match: matched.into(),
            name_after: after.into(),
            folder,
            size: size_text(is_dir, meta.size, store.items(id), self.folder_size.get()).into(),
            modified: format::date(meta.mtime_ns, now, &self.zone).into(),
            modified_full: format::timestamp(meta.mtime_ns, &self.zone).into(),
            type_name,
            extension,
            is_dir,
            kind: kind.icon_index(),
            selected: self.selection.borrow().contains(id),
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

/// Splits a name into the text before the highlighted match, the match and the
/// text after it. Without a match, the whole name is "before".
fn split_name<'a>(name: &'a str, terms: &[NameTerm]) -> (&'a str, &'a str, &'a str) {
    match match_span(name, terms) {
        Some(r) => (&name[..r.start], &name[r.start..r.end], &name[r.end..]),
        None => (name, "", ""),
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

    fn split(name: &str, q: &str) -> (String, String, String) {
        let terms = Query::parse(q, QueryOptions::default())
            .unwrap()
            .name_terms();
        let (a, b, c) = split_name(name, &terms);
        (a.into(), b.into(), c.into())
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
    fn names_split_around_the_first_match() {
        assert_eq!(
            split("Report-2026.pdf", "2026"),
            ("Report-".into(), "2026".into(), ".pdf".into())
        );
        assert_eq!(
            split("Report.pdf", "rep"),
            ("".into(), "Rep".into(), "ort.pdf".into())
        );
        assert_eq!(
            split("Report.pdf", "!rep"),
            ("Report.pdf".into(), "".into(), "".into())
        );
        // Replacement characters for invalid bytes keep offsets valid.
        let lossy = String::from_utf8_lossy(b"a\xffb.txt");
        assert_eq!(
            split(&lossy, "b.t"),
            ("a\u{fffd}".into(), "b.t".into(), "xt".into())
        );
    }
}
