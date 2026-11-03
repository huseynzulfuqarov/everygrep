//! Indexed folders only. Tree traversal and sorting run on a single worker.
use crate::{AppWindow, FolderEntry, engine::Engine};
use everygrep_core::{index::Index, store::NodeId};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    expanded: HashSet<PathBuf>,
    selected: Option<PathBuf>,
    cursor: Option<PathBuf>,
    scroll_y: f32,
}
#[derive(Clone)]
struct Row {
    id: NodeId,
    path: PathBuf,
    name: String,
    depth: i32,
    children: bool,
    expanded: bool,
    offline: bool,
}
struct Ask {
    visible: bool,
    serial: u64,
    view: View,
    all: Option<bool>,
}
struct Answer {
    epoch: u64,
    serial: u64,
    rows: Vec<Row>,
    view: View,
}
struct State {
    main: slint::Weak<AppWindow>,
    engine: Arc<Engine>,
    base: Vec<OsString>,
    view: RefCell<View>,
    rows: RefCell<Vec<Row>>,
    serial: std::cell::Cell<u64>,
    epoch: std::cell::Cell<u64>,
    visible: std::cell::Cell<bool>,
    send: mpsc::Sender<Ask>,
    poll: Timer,
}
thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }
fn with<R>(f: impl FnOnce(&State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow().as_ref().map(|s| f(s)))
}
pub fn freeze() -> View {
    with(State::capture).unwrap_or_default()
}
pub fn restore(view: &View) {
    with(|s| {
        *s.view.borrow_mut() = view.clone();
        s.rows.borrow_mut().clear();
        if let Some(w) = s.main.upgrade() {
            w.set_folder_entries(ModelRc::default());
            w.set_folder_current(-1);
        }
        s.ask(None);
    });
}

fn build(index: &Index, mut view: View, all: Option<bool>) -> (Vec<Row>, View) {
    if all == Some(false) {
        view.expanded.clear();
    }
    let store = index.store();
    let mut pending: Vec<(NodeId, i32)> =
        index.roots().into_iter().rev().map(|id| (id, 0)).collect();
    let mut rows = Vec::new();
    while let Some((id, depth)) = pending.pop() {
        if !store.is_dir(id) {
            continue;
        }
        let path = store.path(id);
        let mut children: Vec<_> = store.children(id).filter(|&c| store.is_dir(c)).collect();
        children.sort_unstable_by(|&a, &b| {
            everygrep_core::collate::collate(store.folded_name(a), store.folded_name(b))
                .then_with(|| store.name(a).cmp(store.name(b)))
        });
        if all == Some(true) && !children.is_empty() {
            view.expanded.insert(path.clone());
        }
        let expanded = view.expanded.contains(&path);
        rows.push(Row {
            id,
            name: String::from_utf8_lossy(store.name(id)).into(),
            path,
            depth,
            children: !children.is_empty(),
            expanded,
            offline: index.is_offline(id),
        });
        if expanded {
            pending.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
        }
    }
    (rows, view)
}

pub fn connect(w: &AppWindow, engine: &Arc<Engine>, base: Vec<OsString>) {
    let (send, receive) = mpsc::channel::<Ask>();
    let (done, answers) = mpsc::channel::<Answer>();
    let worker = engine.clone();
    std::thread::spawn(move || {
        let Ok(mut ask) = receive.recv() else { return };
        let mut previous = None;
        loop {
            for newer in receive.try_iter() {
                if newer.serial > ask.serial {
                    ask = newer;
                }
            }
            if ask.visible {
                let index = worker.index.read().expect("index lock");
                let key = (index.stamp(), index.version(), ask.serial);
                if previous != Some(key) {
                    previous = Some(key);
                    let epoch = worker.epoch();
                    let (rows, view) = build(&index, ask.view.clone(), ask.all.take());
                    drop(index);
                    // Keep worker state on timeouts; no repeated UI cloning of
                    // a fully expanded tree while the index stays unchanged.
                    ask.view = view.clone();
                    if done
                        .send(Answer {
                            epoch,
                            serial: ask.serial,
                            rows,
                            view,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            match receive.recv_timeout(Duration::from_millis(500)) {
                Ok(newer) => ask = newer,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    let state = Rc::new(State {
        main: w.as_weak(),
        engine: engine.clone(),
        base,
        view: RefCell::new(View::default()),
        rows: RefCell::new(Vec::new()),
        serial: std::cell::Cell::new(0),
        epoch: std::cell::Cell::new(0),
        visible: std::cell::Cell::new(w.get_folders_visible()),
        send,
        poll: Timer::default(),
    });
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));
    state.ask(None);
    w.on_folders_changed(|| {
        with(|s| {
            if let Some(w) = s.main.upgrade() {
                crate::prefs::with(|p| {
                    p.set_folders(
                        w.get_folders_visible(),
                        w.get_folders_width(),
                        w.get_folders_recursive(),
                    )
                });
                s.ask(None);
            }
        });
    });
    w.on_folder_action(|row, kind| {
        with(|s| s.action(row, kind));
    });
    let weak = Rc::downgrade(&state);
    state
        .poll
        .start(TimerMode::Repeated, Duration::from_millis(200), move || {
            let Some(s) = weak.upgrade() else { return };
            let Some(w) = s.main.upgrade() else { return };
            if w.get_folders_visible() != s.visible.get() {
                s.ask(None);
            }
            // Never wait for the index or disk on the window thread.
            for answer in answers.try_iter() {
                if answer.serial != s.serial.get() {
                    continue;
                }
                let old_cursor = usize::try_from(w.get_folder_current())
                    .ok()
                    .and_then(|i| s.rows.borrow().get(i).map(|r| r.path.clone()));
                let restoring = s.rows.borrow().is_empty();
                let cursor = old_cursor
                    .or_else(|| answer.view.cursor.clone())
                    .or_else(|| answer.view.selected.clone());
                let current = cursor
                    .and_then(|p| answer.rows.iter().position(|r| r.path == p))
                    .map(|i| i as i32)
                    .unwrap_or_else(|| if answer.rows.is_empty() { -1 } else { 0 });
                let scroll_y = answer.view.scroll_y;
                s.epoch.set(answer.epoch);
                *s.view.borrow_mut() = answer.view;
                let selected = s.view.borrow().selected.clone();
                w.set_folder_entries(ModelRc::new(VecModel::from(
                    answer
                        .rows
                        .iter()
                        .map(|r| FolderEntry {
                            name: r.name.as_str().into(),
                            depth: r.depth,
                            children: r.children,
                            expanded: r.expanded,
                            selected: selected.as_ref() == Some(&r.path),
                            offline: r.offline,
                        })
                        .collect::<Vec<_>>(),
                )));
                *s.rows.borrow_mut() = answer.rows;
                if restoring {
                    w.set_folders_scroll_y(scroll_y);
                }
                w.set_folder_current(current);
            }
        });
}
impl State {
    fn capture(&self) -> View {
        let mut view = self.view.borrow().clone();
        if let Some(w) = self.main.upgrade() {
            if !self.rows.borrow().is_empty() {
                view.cursor = usize::try_from(w.get_folder_current())
                    .ok()
                    .and_then(|i| self.rows.borrow().get(i).map(|r| r.path.clone()));
                view.scroll_y = w.get_folders_scroll_y();
            }
        }
        view
    }
    fn ask(&self, all: Option<bool>) {
        self.serial.set(self.serial.get().wrapping_add(1));
        let visible = self.main.upgrade().is_some_and(|w| w.get_folders_visible());
        self.visible.set(visible);
        let _ = self.send.send(Ask {
            visible,
            serial: self.serial.get(),
            view: self.capture(),
            all,
        });
    }
    fn action(&self, row: i32, kind: i32) {
        let Some(w) = self.main.upgrade() else { return };
        if kind == 9 {
            let selected = self.view.borrow().selected.clone();
            let row = self
                .rows
                .borrow()
                .iter()
                .position(|r| Some(&r.path) == selected.as_ref());
            if let Some(row) = row {
                self.action(row as i32, 0);
            }
            return;
        }
        if kind == 2 || kind == 3 {
            self.ask(Some(kind == 2));
            return;
        }
        if kind == 8 {
            let row = usize::try_from(w.get_current()).ok();
            let mut path = None;
            crate::with_model(&w, |m| {
                path = row.and_then(|r| {
                    m.path(r).and_then(|p| {
                        if m.name(r)?.1 {
                            Some(p)
                        } else {
                            p.parent().map(PathBuf::from)
                        }
                    })
                });
            });
            if let Some(path) = path {
                let mut view = self.view.borrow_mut();
                for parent in path.ancestors().skip(1) {
                    view.expanded.insert(parent.into());
                }
                view.cursor = Some(path.clone());
                view.selected = Some(path);
                drop(view);
                // A locate request has a deliberate cursor, rather than the
                // previous tree row. Install it like a restored tab.
                self.rows.borrow_mut().clear();
                w.set_folder_entries(ModelRc::default());
                w.set_folder_current(-1);
                self.ask(None);
            }
            return;
        }
        let Some(r) = usize::try_from(row)
            .ok()
            .and_then(|i| self.rows.borrow().get(i).cloned())
        else {
            return;
        };
        if kind == 1 {
            let mut view = self.view.borrow_mut();
            if !view.expanded.remove(&r.path) {
                view.expanded.insert(r.path);
            }
            drop(view);
            self.ask(None);
            return;
        }
        if kind == 4 {
            let (engine, epoch, weak) = (self.engine.clone(), self.epoch.get(), w.as_weak());
            std::thread::spawn(move || {
                let online = engine
                    .index
                    .read()
                    .ok()
                    .is_some_and(|index| still_online(&index, &r, epoch, engine.epoch()));
                if online {
                    crate::open::open(&r.path);
                } else {
                    let _ = weak.upgrade_in_event_loop(|w| {
                        crate::info(
                            &w,
                            crate::i18n::tr("The folder is no longer available.").into(),
                        );
                    });
                }
            });
            return;
        }
        let Some(query) = crate::fileops_search::query(
            &r.path,
            true,
            false,
            w.get_folders_recursive(),
            false,
            true,
        ) else {
            return;
        };
        let mut options = crate::options(&w);
        options.regex = false;
        match kind {
            5 => crate::tabs_ui::open_search(&query, options, true, &Default::default()),
            6 => {
                let args = crate::fileops_search::window_args(&self.base, &query, options);
                match std::env::current_exe()
                    .and_then(|exe| std::process::Command::new(exe).args(args).spawn())
                {
                    Ok(child) => crate::open::reap(child),
                    Err(e) => crate::error(
                        &w,
                        crate::i18n::fill(
                            crate::i18n::tr("Cannot open a search window: {}"),
                            &[&e],
                        ),
                    ),
                }
            }
            _ => {
                self.view.borrow_mut().selected = Some(r.path);
                w.invoke_history_record();
                w.set_query(query.into());
                crate::set_options(&w, options);
                crate::search_now(&w, &self.engine);
                self.ask(None);
            }
        }
    }
}
/// A removed, renamed, replaced-index or newly offline row cannot open a path
/// that may now belong to a different mounted drive.
fn still_online(index: &Index, row: &Row, epoch: u64, current: u64) -> bool {
    let store = index.store();
    epoch == current
        && row.id.index() < store.len()
        && store.is_live(row.id)
        && store.is_dir(row.id)
        && !index.is_offline(row.id)
        && store.path(row.id) == row.path
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::store::{Meta, NodeStore};
    #[test]
    fn indexed_tree_is_sorted_expandable_and_ignores_files() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/index-only", true, Meta::default());
        let z = store.add(Some(root), b"z", true, Meta::default());
        store.add(Some(root), b"a", true, Meta::default());
        store.add(Some(root), b"file", false, Meta::default());
        store.add(Some(z), b"deep", true, Meta::default());
        let index = Index::new(store);
        let (rows, _) = build(&index, View::default(), None);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].children);
        let (rows, view) = build(&index, View::default(), Some(true));
        assert_eq!(
            rows.iter()
                .map(|r| (r.name.as_str(), r.depth))
                .collect::<Vec<_>>(),
            [("/tmp/index-only", 0), ("a", 1), ("z", 1), ("deep", 2)]
        );
        let (rows, view) = build(&index, view, Some(false));
        assert_eq!(rows.len(), 1);
        assert!(view.expanded.is_empty());
    }
    #[test]
    fn children_use_the_result_lists_natural_order() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/tree-order", true, Meta::default());
        for name in [b"a10".as_slice(), b"a2", b"A1"] {
            store.add(Some(root), name, true, Meta::default());
        }
        let (rows, _) = build(&Index::new(store), View::default(), Some(true));
        assert_eq!(
            rows[1..]
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["A1", "a2", "a10"]
        );
    }

    #[test]
    fn removed_children_and_offline_roots_follow_the_index() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/tree-offline", true, Meta::default());
        let child = store.add(Some(root), b"child", true, Meta::default());
        let mut index = Index::new(store);
        let (_, view) = build(&index, View::default(), Some(true));
        index.set_offline(root, true);
        let (rows, _) = build(&index, view.clone(), None);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.offline));
        index.remove(child);
        let (rows, _) = build(&index, view, None);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].children);
    }

    #[test]
    fn tree_state_can_be_frozen_without_sharing_mutations_between_tabs() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/tree-tabs", true, Meta::default());
        store.add(Some(root), b"child", true, Meta::default());
        let index = Index::new(store);
        let (_, first) = build(&index, View::default(), Some(true));
        let mut other = first.clone();
        other.selected = Some(PathBuf::from("/tmp/tree-tabs/child"));
        let (_, other) = build(&index, other, Some(false));
        assert_eq!(build(&index, first.clone(), None).0.len(), 2);
        assert_eq!(build(&index, other, None).0.len(), 1);
        assert!(first.selected.is_none());
    }

    #[test]
    fn opening_refuses_stale_and_offline_rows() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/tree-open", true, Meta::default());
        let mut index = Index::new(store);
        let (rows, _) = build(&index, View::default(), None);
        assert!(still_online(&index, &rows[0], 1, 1));
        assert!(!still_online(&index, &rows[0], 1, 2));
        index.set_offline(root, true);
        assert!(!still_online(&index, &rows[0], 1, 1));
        index.set_offline(root, false);
        index.remove(root);
        assert!(!still_online(&index, &rows[0], 1, 1));
    }

    #[test]
    fn raw_folder_names_keep_exact_paths_despite_lossy_labels() {
        use std::os::unix::ffi::OsStrExt;
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/tree-raw", true, Meta::default());
        store.add(Some(root), b"a\xff\"|<>*?", true, Meta::default());
        let index = Index::new(store);
        let (rows, _) = build(&index, View::default(), Some(true));
        assert_eq!(
            rows[1].path.as_os_str().as_bytes(),
            b"/tmp/tree-raw/a\xff\"|<>*?"
        );
        assert!(rows[1].name.contains('\u{fffd}'));
    }

    #[test]
    fn panel_settings_round_trip_and_reject_bad_widths() {
        let s = crate::settings::Settings::parse(
            "folders-visible = true\nfolders-width = 310\nfolders-subfolders = false\n",
        );
        let s = crate::settings::Settings::parse(&s.to_text());
        assert!(s.folders_visible);
        assert_eq!(s.folders_width, 310.0);
        assert!(!s.folders_recursive);
        let s = crate::settings::Settings::parse("folders-width = NaN\n");
        assert_eq!(s.folders_width, 220.0);
        assert_eq!(s.problems.len(), 1);
    }
}
