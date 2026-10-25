//! The result table model. Rows are built on demand, so only the rows on screen
//! cost anything, however many results there are.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use everygrep_core::localtime::Zone;
use everygrep_core::store::NodeId;
use slint::{Model, ModelNotify, ModelTracker, SharedString};

use crate::ResultRow;
use crate::engine::{Engine, IndexState};
use crate::format;

/// Rows of the result table, backed by a list of node ids.
pub struct ResultsModel {
    engine: Arc<Engine>,
    ids: RefCell<Arc<Vec<NodeId>>>,
    zone: Zone,
    home: Option<Vec<u8>>,
    now_ns: Cell<i64>,
    state: Cell<IndexState>,
    indexed: Cell<usize>,
    took: Cell<Duration>,
    notify: ModelNotify,
}

impl ResultsModel {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            ids: RefCell::new(Arc::new(Vec::new())),
            zone: Zone::local(),
            home: std::env::var_os("HOME").map(|h| h.as_bytes().to_vec()),
            now_ns: Cell::new(0),
            state: Cell::new(IndexState::Loading),
            indexed: Cell::new(0),
            took: Cell::new(Duration::ZERO),
            notify: ModelNotify::default(),
        }
    }

    /// Replaces the rows.
    pub fn set_ids(&self, ids: Arc<Vec<NodeId>>, took: Duration) {
        self.now_ns.set(now_ns());
        self.took.set(took);
        self.engine.set_shown(Arc::clone(&ids));
        *self.ids.borrow_mut() = ids;
        self.notify.reset();
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
        (id.index() < store.len() && store.is_live(id)).then(|| store.path(id))
    }

    /// Text for the left and right side of the status line.
    pub fn status(&self) -> (String, String) {
        let results = self.ids.borrow().len();
        let indexed = format::count(self.indexed.get());
        let state = match self.state.get() {
            IndexState::Loading => "loading",
            IndexState::Scanning => "indexing",
            IndexState::CatchingUp => "catching up",
            IndexState::Watching => "watching for changes",
            IndexState::Static => "not watching changes",
        };
        let left = format!(
            "{} {}  ·  {indexed} indexed  ·  {state}",
            format::count(results),
            if results == 1 { "result" } else { "results" },
        );
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
        if id.index() >= store.len() || !store.is_live(id) {
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
        Some(ResultRow {
            name: String::from_utf8_lossy(store.name(id)).as_ref().into(),
            folder,
            size: if is_dir {
                SharedString::new()
            } else {
                format::size(meta.size).into()
            },
            modified: format::date(meta.mtime_ns, now, &self.zone).into(),
            modified_full: format::timestamp(meta.mtime_ns, &self.zone).into(),
            is_dir,
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}
