//! Custom tab strip backed by Rust state and one shared result view.
use crate::engine::Engine;
use crate::i18n::fill;
use crate::i18n::tr;
use crate::prefs::Prefs;
use crate::results::ResultsModel;
use crate::session::{self, Session, Writer};
use crate::settings::Columns;
use crate::tabs::{SearchState, Tab, Tabs, tab_options};
use crate::{AppWindow, Command, TabEntry, TabsWindow};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct State {
    main: slint::Weak<AppWindow>,
    engine: Arc<Engine>,
    model: Rc<ResultsModel>,
    prefs: Rc<Prefs>,
    pause: Rc<Timer>,
    tabs: RefCell<Tabs>,
    picker: RefCell<Option<TabsWindow>>,
    poll: Timer,
    last_submitted: RefCell<SearchState>,
    scope: String,
    writer: RefCell<Writer>,
    started: Instant,
    session_error: RefCell<Option<String>>,
}
thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }
fn with<R>(f: impl FnOnce(&Rc<State>) -> R) -> Option<R> {
    let state = STATE.with(|s| s.borrow().clone());
    state.as_ref().map(f)
}

pub fn connect(
    w: &AppWindow,
    engine: &Arc<Engine>,
    model: &Rc<ResultsModel>,
    prefs: &Rc<Prefs>,
    pause: &Rc<Timer>,
    start: session::Start,
) {
    let mut tabs = Tabs::default();
    tabs.open[0].search = read(w);
    let path = crate::settings::config_dir().map(|d| d.join("session"));
    let mut restored = false;
    let mut session_error = None;
    if start.restore {
        match path.as_deref().map(Session::load).transpose() {
            Ok(Some(Some(session))) => {
                if let Some(previous) = session.restore(&start.scope) {
                    tabs = previous;
                    restored = true;
                }
            }
            Ok(_) => {}
            Err(e) => {
                session_error = Some(fill(
                    tr("Cannot restore tabs: {}"),
                    &[&session_detail(path.as_deref(), &e)],
                ))
            }
        }
    }
    tabs.history_policy(prefs.history_limit(), false);
    let state = Rc::new(State {
        main: w.as_weak(),
        engine: engine.clone(),
        model: model.clone(),
        prefs: prefs.clone(),
        pause: pause.clone(),
        tabs: RefCell::new(tabs),
        picker: RefCell::new(None),
        poll: Timer::default(),
        last_submitted: RefCell::new(read(w)),
        scope: start.scope,
        writer: RefCell::new(Writer::new(path)),
        started: Instant::now(),
        session_error: RefCell::new(None),
    });
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));
    w.on_tab_command(|command| {
        with(|s| s.command(command));
    });
    w.on_tab_selected(|i| {
        if let Ok(i) = usize::try_from(i) {
            with(|s| s.change(|t| t.select(i)));
        }
    });
    w.on_tab_closed(|i| {
        if let Ok(i) = usize::try_from(i) {
            with(|s| s.change(|t| t.close(&[i])));
        }
    });
    w.on_closed_tab_selected(|i| {
        if let Ok(i) = usize::try_from(i) {
            with(|s| {
                s.change(|t| {
                    if let Some(i) = t.closed.len().checked_sub(i + 1) {
                        t.reopen(i);
                    }
                })
            });
        }
    });
    w.on_tab_preference_changed(|i, on| crate::prefs::with(|p| p.set_tab_preference(i, on)));
    let weak = Rc::downgrade(&state);
    // Observe programmatic edits too (bookmarks, history, column layout), without
    // copying result ids or selection and without installing more search callbacks.
    state
        .poll
        .start(TimerMode::Repeated, Duration::from_millis(250), move || {
            if let Some(s) = weak.upgrade() {
                s.observe();
            }
        });
    if restored {
        state.install(w);
    }
    if let Some(error) = session_error {
        crate::error(w, error);
    }
    state.fill();
}

pub fn history_policy(max: usize, clear: bool) {
    with(|s| {
        if clear || max == 0 {
            s.pause.stop();
            if let Some(w) = s.main.upgrade() {
                s.observe_search(&w);
            }
        }
        s.tabs.borrow_mut().history_policy(max, clear);
        s.fill();
        if clear || max == 0 {
            s.forget_session();
        }
    });
}

pub fn remove_history(query: &str) {
    with(|s| {
        s.pause.stop();
        if let Some(w) = s.main.upgrade() {
            s.observe_search(&w);
        }
        s.tabs.borrow_mut().remove_history(query);
        s.fill();
        s.forget_session();
    });
}

pub fn session_policy() {
    with(|s| {
        if !s.prefs.restore_session() {
            s.forget_session();
        }
    });
}

pub fn close_picker() {
    with(|s| s.close_picker());
}

pub fn submitted_sort(column: i32, descending: bool) {
    with(|s| {
        let mut search = s.last_submitted.borrow_mut();
        search.sort_column = column;
        search.descending = descending;
    });
}

pub fn submitted(w: &AppWindow) {
    with(|s| *s.last_submitted.borrow_mut() = read(w));
}

pub fn flush() {
    with(|s| {
        let Some(w) = s.main.upgrade() else { return };
        s.observe_search(&w);
        let snapshot = Session::capture(&s.scope, &s.tabs.borrow());
        let result = s
            .writer
            .borrow_mut()
            .finish(s.prefs.restore_session(), snapshot);
        s.report_write(result);
    });
}

pub fn refresh() {
    with(|s| s.fill());
}

pub fn open_search(
    query: &str,
    options: everygrep_core::query::QueryOptions,
    keep_filter: bool,
    columns: &Columns,
) {
    with(|s| {
        let Some(w) = s.main.upgrade() else { return };
        let search = read(&w).launched(query, options, keep_filter, columns);
        let activate = s.prefs.switch_to_new_tab() && !blocked(&w);
        if activate {
            s.store(&w);
        }
        s.tabs.borrow_mut().add(
            Tab {
                search,
                ..Default::default()
            },
            activate,
        );
        if activate {
            s.install(&w);
        }
        s.fill();
    });
}

fn session_detail(path: Option<&std::path::Path>, error: &std::io::Error) -> String {
    let path = path.map(|p| p.display().to_string()).unwrap_or_default();
    format!(
        "{path}: {error}\n{}",
        tr("To reset saved tabs, turn session restore off and on.")
    )
}

fn blocked(w: &AppWindow) -> bool {
    w.get_confirm_open() || w.get_fileops_rename_open()
}

fn read(w: &AppWindow) -> SearchState {
    SearchState {
        query: w.get_query().into(),
        options: tab_options(crate::options(w)),
        filter: crate::saved_ui::selected(w),
        filter_snapshot: crate::saved_ui::using_snapshot(w),
        sort_column: w.get_sort_column(),
        descending: w.get_sort_descending(),
        columns: read_columns(w),
    }
}

/// The shown columns and their widths in the window, for a tab.
fn read_columns(w: &AppWindow) -> Columns {
    Columns {
        show_type: w.get_show_type(),
        show_ext: w.get_show_ext(),
        show_created: w.get_show_created(),
        show_accessed: w.get_show_accessed(),
        show_attrs: w.get_show_attrs(),
        show_md5: w.get_show_md5(),
        show_sha256: w.get_show_sha256(),
        show_crc32: w.get_show_crc32(),
        show_name_freq: w.get_show_name_freq(),
        show_ext_freq: w.get_show_ext_freq(),
        show_size_freq: w.get_show_size_freq(),
        name: w.get_name_set().then(|| w.get_name_want()),
        // Folder has a width of its own only while the columns are wider
        // than the window; 0 means it takes the room the others leave.
        folder: (w.get_folder_want() >= 0.5).then(|| w.get_folder_want()),
        size: Some(w.get_size_want()),
        date: Some(w.get_date_want()),
        type_: Some(w.get_type_want()),
        ext: Some(w.get_ext_want()),
        created: Some(w.get_created_want()),
        accessed: Some(w.get_accessed_want()),
        attrs: Some(w.get_attr_want()),
        md5: Some(w.get_md5_want()),
        sha256: Some(w.get_sha256_want()),
        crc32: Some(w.get_crc32_want()),
        name_freq: Some(w.get_name_freq_want()),
        ext_freq: Some(w.get_ext_freq_want()),
        size_freq: Some(w.get_size_freq_want()),
    }
}

/// Puts a tab's columns and widths into the window.
fn install_columns(w: &AppWindow, c: &Columns) {
    w.set_show_type(c.show_type);
    w.set_show_ext(c.show_ext);
    w.set_show_created(c.show_created);
    w.set_show_accessed(c.show_accessed);
    w.set_show_attrs(c.show_attrs);
    w.set_show_md5(c.show_md5);
    w.set_show_sha256(c.show_sha256);
    w.set_show_crc32(c.show_crc32);
    w.set_show_name_freq(c.show_name_freq);
    w.set_show_ext_freq(c.show_ext_freq);
    w.set_show_size_freq(c.show_size_freq);
    w.set_name_set(c.name.is_some());
    if let Some(v) = c.name {
        w.set_name_want(v);
    }
    w.set_folder_want(c.folder.unwrap_or(0.0));
    c.size.into_iter().for_each(|v| w.set_size_want(v));
    c.date.into_iter().for_each(|v| w.set_date_want(v));
    c.type_.into_iter().for_each(|v| w.set_type_want(v));
    c.ext.into_iter().for_each(|v| w.set_ext_want(v));
    c.created.into_iter().for_each(|v| w.set_created_want(v));
    c.accessed.into_iter().for_each(|v| w.set_accessed_want(v));
    c.attrs.into_iter().for_each(|v| w.set_attr_want(v));
    c.md5.into_iter().for_each(|v| w.set_md5_want(v));
    c.sha256.into_iter().for_each(|v| w.set_sha256_want(v));
    c.crc32.into_iter().for_each(|v| w.set_crc32_want(v));
    c.name_freq
        .into_iter()
        .for_each(|v| w.set_name_freq_want(v));
    c.ext_freq.into_iter().for_each(|v| w.set_ext_freq_want(v));
    c.size_freq
        .into_iter()
        .for_each(|v| w.set_size_freq_want(v));
}

fn title(tab: &Tab) -> String {
    if tab.search.query.is_empty() {
        tr("New tab").into()
    } else {
        tab.search.query.clone()
    }
}

impl State {
    fn forget_session(&self) {
        let result = self.writer.borrow_mut().forget();
        self.report_write(result);
    }

    fn observe(&self) {
        let Some(w) = self.main.upgrade() else { return };
        self.observe_search(&w);
        let snapshot = Session::capture(&self.scope, &self.tabs.borrow());
        let result = self.writer.borrow_mut().observe(
            self.prefs.restore_session(),
            snapshot,
            self.started.elapsed(),
        );
        self.report_write(result);
    }

    fn observe_search(&self, w: &AppWindow) {
        let search = read(w);
        let changed = {
            let mut tabs = self.tabs.borrow_mut();
            let active = tabs.active;
            let changed = tabs.open[active].search != search;
            tabs.open[active].search = search;
            changed
        };
        if changed {
            self.fill();
        }
    }

    fn report_write(&self, result: std::io::Result<()>) {
        if let Err(e) = result {
            let message = fill(
                tr("Cannot save tabs: {}"),
                &[&session_detail(self.writer.borrow().path(), &e)],
            );
            if self.session_error.borrow().as_ref() != Some(&message) {
                eprintln!("everygrep-gui: cannot save tabs: {e}");
                if let Some(w) = self.main.upgrade() {
                    crate::error(&w, message.clone());
                }
                *self.session_error.borrow_mut() = Some(message);
            }
        }
    }

    fn store(&self, w: &AppWindow) {
        // End a rename and flush the old tab's delayed history record before leaving.
        self.pause.stop();
        w.invoke_history_record();
        crate::stop_renaming(w, &self.model);
        let mut tabs = self.tabs.borrow_mut();
        let active = tabs.active;
        let tab = &mut tabs.open[active];
        tab.search = read(w);
        tab.result = Some(self.model.freeze());
        tab.history = self.prefs.history.borrow().clone();
        tab.current = w.get_current();
        tab.scroll_y = w.get_results_scroll_y();
        tab.scroll_x = w.get_results_scroll_x();
        tab.pending = w.get_search_pending();
        tab.submitted = Some(self.last_submitted.borrow().clone());
    }

    fn install(&self, w: &AppWindow) {
        let tabs = self.tabs.borrow();
        let tab = &tabs.open[tabs.active];
        let search = &tab.search;
        w.set_query(search.query.as_str().into());
        let policy = self.prefs.search_policy();
        // Clear the previous tab's extra slot before selecting this tab's built-in.
        crate::saved_ui::tab_filter(w, None, false);
        crate::set_options(w, search.options);
        self.prefs.restore_search_policy(w);
        crate::show_badges(w);
        if search.filter.is_some() {
            crate::saved_ui::tab_filter(w, search.filter.as_ref(), search.filter_snapshot);
        }
        w.set_sort_column(search.sort_column);
        w.set_sort_descending(search.descending);
        install_columns(w, &search.columns);
        crate::apply_detail_columns(w);
        self.model.restore(tab.result.as_ref());
        w.set_current(if self.model.row_count() == 0 {
            -1
        } else {
            tab.current.clamp(0, self.model.row_count() as i32 - 1)
        });
        w.set_results_scroll_y(tab.scroll_y);
        // After the tab's columns, so the table has its width; kept within
        // the room the table has now (the window may have changed since).
        w.set_results_scroll_x(tab.scroll_x.clamp(-w.get_scroll_room(), 0.0));
        let mut history = tab.history.clone();
        history.set_max(self.prefs.history_limit());
        *self.prefs.history.borrow_mut() = history;
        let pending = tab.pending && !crate::typing::with(|t| t.as_you_type());
        let submitted = if pending {
            tab.submitted.as_ref().unwrap_or(search)
        } else {
            search
        };
        self.engine.set_sort(crate::sort_for_column(
            submitted.sort_column,
            submitted.descending,
            self.model.folder_size(),
        ));
        // Only the visible tab updates. Unsubmitted text keeps waiting for Enter.
        let filter = submitted
            .filter
            .as_ref()
            .map(|f| crate::saved_ui::filter_for_tab(f, submitted.filter_snapshot));
        self.engine.set_filtered_query(
            &submitted.query,
            submitted.effective_options(policy, filter.as_ref()),
            filter.map(|f| (f.search, f.options)),
        );
        *self.last_submitted.borrow_mut() = submitted.clone();
        crate::typing::with(|t| {
            t.searched();
            if pending {
                t.edited();
            }
        });
        w.set_search_pending(pending);
        w.set_message("".into());
        if pending {
            crate::wait_for_enter(w);
        } else {
            w.set_notice("".into());
        }
        crate::refresh_status(w);
        w.invoke_focus_search();
    }

    fn change(&self, change: impl Fn(&mut Tabs)) {
        let Some(w) = self.main.upgrade() else { return };
        if blocked(&w) {
            return;
        }
        let mut current = self.tabs.borrow().clone();
        current.open[current.active].search = read(&w);
        let mut preview = current.clone();
        change(&mut preview);
        self.close_picker();
        if current.same_layout(&preview) {
            return;
        }
        self.store(&w);
        change(&mut self.tabs.borrow_mut());
        self.install(&w);
        self.fill();
    }

    fn fill(&self) {
        let Some(w) = self.main.upgrade() else { return };
        let tabs = self.tabs.borrow();
        let rows: Vec<_> = tabs
            .open
            .iter()
            .enumerate()
            .map(|(i, t)| TabEntry {
                title: title(t).into(),
                checked: i == tabs.active,
            })
            .collect();
        w.set_tabs(ModelRc::new(VecModel::from(rows)));
        w.set_active_tab(tabs.active as i32);
        w.set_closed_tab_names(ModelRc::new(VecModel::from(
            tabs.closed
                .iter()
                .rev()
                .map(|t| title(t).into())
                .collect::<Vec<slint::SharedString>>(),
        )));
    }

    fn close_picker(&self) {
        if let Some(picker) = self.picker.borrow_mut().take() {
            let _ = picker.hide();
        }
    }

    fn picker(self: &Rc<Self>, multiple: bool) {
        self.close_picker();
        let Ok(w) = TabsWindow::new() else { return };
        let rows = Rc::new(VecModel::from(
            self.tabs
                .borrow()
                .open
                .iter()
                .map(|t| TabEntry {
                    title: title(t).into(),
                    checked: false,
                })
                .collect::<Vec<_>>(),
        ));
        w.set_tabs(ModelRc::from(rows.clone()));
        w.set_multiple(multiple);
        let weak = Rc::downgrade(self);
        w.on_chosen(move |i| {
            if let (Some(s), Ok(i)) = (weak.upgrade(), usize::try_from(i)) {
                s.change(|t| t.select(i));
            }
        });
        let marked = rows.clone();
        w.on_marked(move |i, checked| {
            if let Ok(i) = usize::try_from(i) {
                if let Some(mut row) = marked.row_data(i) {
                    row.checked = checked;
                    marked.set_row_data(i, row);
                }
            }
        });
        let weak = Rc::downgrade(self);
        w.on_close_marked(move || {
            if let Some(s) = weak.upgrade() {
                let indexes: Vec<_> = (0..rows.row_count())
                    .filter(|i| rows.row_data(*i).is_some_and(|t| t.checked))
                    .collect();
                s.change(|t| t.close(&indexes));
            }
        });
        let _ = w.show();
        *self.picker.borrow_mut() = Some(w);
    }

    fn command(self: &Rc<Self>, command: Command) {
        if self.main.upgrade().is_none_or(|w| blocked(&w)) {
            return;
        }
        if command == Command::ListTabs {
            self.picker(false);
            return;
        }
        if command == Command::CloseMultipleTabs {
            self.picker(true);
            return;
        }
        self.change(|t| match command {
            Command::NewTab => {
                let mut search = t.open[t.active].search.clone();
                search.query.clear();
                t.add(
                    Tab {
                        search,
                        ..Default::default()
                    },
                    true,
                );
            }
            Command::DuplicateTab => t.add(t.open[t.active].clone(), true),
            Command::CloseTab => t.close(&[t.active]),
            Command::NextTab => t.step(true),
            Command::PreviousTab => t.step(false),
            Command::SelectTab1 => t.select(0),
            Command::SelectTab2 => t.select(1),
            Command::SelectTab3 => t.select(2),
            Command::SelectTab4 => t.select(3),
            Command::SelectTab5 => t.select(4),
            Command::SelectTab6 => t.select(5),
            Command::SelectTab7 => t.select(6),
            Command::SelectTab8 => t.select(7),
            Command::SelectTab9 => t.select(8),
            Command::SelectTab10 => t.select(9),
            Command::CloseLeftTabs => t.close(&(0..t.active).collect::<Vec<_>>()),
            Command::CloseRightTabs => t.close(&(t.active + 1..t.open.len()).collect::<Vec<_>>()),
            Command::CloseOtherTabs => t.close(
                &(0..t.open.len())
                    .filter(|i| *i != t.active)
                    .collect::<Vec<_>>(),
            ),
            Command::ReopenTab => {
                if let Some(i) = t.closed.len().checked_sub(1) {
                    t.reopen(i);
                }
            }
            Command::ReopenAllTabs => t.reopen_all(),
            _ => {}
        });
    }
}
