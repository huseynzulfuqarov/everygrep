//! The settings window and applying settings to the running program.
//!
//! Every change is applied at once and written to the settings file (see
//! [`crate::settings`]). Folders to index and excluded folders are written
//! only for the window over the default index: a window started with
//! `--root` or `--index-file` changes its own folders for the session.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use everygrep_core::filter::Filter;
use everygrep_core::query::QueryOptions;
use everygrep_core::scan::normalize_roots;
use slint::{ComponentHandle, ModelRc, SharedString, StandardListViewItem, VecModel};

use crate::engine::{Engine, IndexState};
use crate::history::{self, History};
use crate::settings::{self, Columns, MAX_HISTORY, OnClose, Page, Settings};
use crate::{AppWindow, SettingsWindow, labels};

/// Names of the settings pages for `--settings=<page>`, in page order.
pub const PAGES: [&str; 4] = ["indexing", "search", "window", "shortcut"];

/// The command a desktop shortcut runs.
const COMMAND: &str = "everygrep-gui";

/// Default widths of the main window's columns, in pixels (see app.slint).
const SIZE_WIDTH: f32 = 96.0;
const DATE_WIDTH: f32 = 120.0;
const TYPE_WIDTH: f32 = 130.0;
const EXT_WIDTH: f32 = 80.0;

/// The folders a window indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSetup {
    /// Folders to index; empty stands for the saved index's folders, or the
    /// home folder.
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<PathBuf>,
    /// Whether these are the saved settings. False for a window started with
    /// its own `--root` or `--index-file`: its changes are not saved.
    pub saved: bool,
}

impl IndexSetup {
    /// The folders for a window started with `roots` and `index_file` from
    /// the command line.
    pub fn new(roots: &[PathBuf], index_file: Option<&Path>, s: &Settings) -> Self {
        if roots.is_empty() && index_file.is_none() {
            Self {
                roots: s.roots.clone(),
                excludes: s.excludes.clone(),
                saved: true,
            }
        } else {
            Self {
                roots: roots.to_vec(),
                excludes: Vec::new(),
                saved: false,
            }
        }
    }

    /// Changes the folders, and the settings file at `path` when they are
    /// the saved ones.
    pub fn change(
        &mut self,
        roots: Vec<PathBuf>,
        excludes: Vec<PathBuf>,
        path: Option<&Path>,
    ) -> std::io::Result<()> {
        self.roots = roots;
        self.excludes = excludes;
        match path {
            Some(path) if self.saved => settings::update(path, |s| {
                s.roots = self.roots.clone();
                s.excludes = self.excludes.clone();
            })
            .map(drop),
            _ => Ok(()),
        }
    }
}

/// Settings of the running program.
pub struct Prefs {
    /// Where the settings are saved; `None` without a home folder.
    path: Option<PathBuf>,
    settings: RefCell<Settings>,
    index: RefCell<IndexSetup>,
    /// Why the settings file could not be read; nothing is written then, so
    /// a file we do not understand is never replaced.
    read_error: Option<String>,
    /// The last problem, shown in the settings window.
    error: RefCell<String>,
    state: Cell<(IndexState, usize)>,
    /// A folder chooser is open.
    choosing: Cell<bool>,
    engine: Arc<Engine>,
    main: slint::Weak<AppWindow>,
    pub history: RefCell<History>,
    window: RefCell<Option<SettingsWindow>>,
}

thread_local! {
    static PREFS: RefCell<Option<Rc<Prefs>>> = RefCell::default();
}

/// Runs `f` with the program's settings, once they are set up.
pub fn with(f: impl FnOnce(&Rc<Prefs>)) {
    if let Some(p) = PREFS.with(|p| p.borrow().clone()) {
        f(&p);
    }
}

/// Reads the settings file. A file that cannot be read gives the defaults
/// and the reason.
pub fn load() -> (Option<PathBuf>, Settings, Option<String>) {
    let Some(path) = settings::path() else {
        return (None, Settings::default(), None);
    };
    match Settings::load(&path) {
        Ok(s) => (Some(path), s, None),
        Err(e) => {
            let why = labels::cannot_read(&path, &e);
            (Some(path), Settings::default(), Some(why))
        }
    }
}

impl Prefs {
    /// Sets up the program's settings; see [`with`].
    pub fn install(
        path: Option<PathBuf>,
        settings: Settings,
        read_error: Option<String>,
        index: IndexSetup,
        engine: Arc<Engine>,
        main: &AppWindow,
    ) -> Rc<Prefs> {
        let saved = if settings.history_on_disk && settings.history_size > 0 {
            settings::history_path()
                .and_then(|p| history::load(&p).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let error = match (&read_error, &path) {
            (Some(e), _) => e.clone(),
            (None, None) => labels::NO_SETTINGS_FOLDER.to_owned(),
            (None, Some(_)) if !settings.problems.is_empty() => {
                labels::bad_lines(settings.problems.len())
            }
            _ => String::new(),
        };
        let prefs = Rc::new(Prefs {
            path,
            history: RefCell::new(History::with_entries(settings.history_size, saved)),
            settings: RefCell::new(settings),
            index: RefCell::new(index),
            read_error,
            error: RefCell::new(error),
            state: Cell::new((IndexState::Loading, 0)),
            choosing: Cell::new(false),
            engine,
            main: main.as_weak(),
            window: RefCell::default(),
        });
        PREFS.with(|p| *p.borrow_mut() = Some(prefs.clone()));
        prefs
    }

    /// Changes the settings and saves them.
    fn change(&self, f: impl Fn(&mut Settings)) {
        f(&mut self.settings.borrow_mut());
        if self.read_error.is_some() {
            return;
        }
        let Some(path) = &self.path else { return };
        let result = settings::update(path, &f);
        self.set_error(result.err().map(|e| labels::cannot_save(&e)));
    }

    fn set_error(&self, error: Option<String>) {
        let text = error
            .or_else(|| self.read_error.clone())
            .unwrap_or_default();
        *self.error.borrow_mut() = text;
        self.refresh();
    }

    /// Records a search in the history, and on disk when it is kept there.
    pub fn record(&self, query: &str) {
        if self.history.borrow_mut().record(query) {
            self.save_history();
        }
    }

    /// Saves the history when it is kept on disk.
    pub fn save_history(&self) {
        let s = self.settings.borrow();
        if !s.history_on_disk || s.history_size == 0 || self.read_error.is_some() {
            return;
        }
        if let Some(path) = settings::history_path() {
            let _ = history::save(&path, self.history.borrow().entries());
        }
    }

    /// Notes the index state for the Indexing page.
    pub fn set_state(&self, state: IndexState, indexed: usize) {
        self.state.set((state, indexed));
        self.refresh();
    }

    /// Puts the remembered columns into the main window.
    pub fn restore_columns(&self, w: &AppWindow) {
        let s = self.settings.borrow();
        if !s.remember_columns {
            return;
        }
        let c = &s.columns;
        w.set_show_type(w.get_show_type() || c.show_type);
        w.set_show_ext(w.get_show_ext() || c.show_ext);
        if let Some(n) = c.name {
            w.set_name_want(n);
            w.set_name_set(true);
        }
        c.size.into_iter().for_each(|v| w.set_size_want(v));
        c.date.into_iter().for_each(|v| w.set_date_want(v));
        c.type_.into_iter().for_each(|v| w.set_type_want(v));
        c.ext.into_iter().for_each(|v| w.set_ext_want(v));
    }

    /// Keeps the main window's columns, when they are remembered.
    pub fn save_columns(&self, w: &AppWindow) {
        if !self.settings.borrow().remember_columns {
            return;
        }
        let differs = |v: f32, default: f32| ((v - default).abs() >= 0.5).then_some(v.round());
        let columns = Columns {
            show_type: w.get_show_type(),
            show_ext: w.get_show_ext(),
            name: w.get_name_set().then(|| w.get_name_want().round()),
            size: differs(w.get_size_want(), SIZE_WIDTH),
            date: differs(w.get_date_want(), DATE_WIDTH),
            type_: differs(w.get_type_want(), TYPE_WIDTH),
            ext: differs(w.get_ext_want(), EXT_WIDTH),
        };
        if self.settings.borrow().columns != columns {
            self.change(|s| s.columns = columns.clone());
        }
    }

    /// The folders to show on the Indexing page: the chosen ones, or the
    /// ones the index covers when none were chosen.
    fn shown_roots(&self) -> Vec<PathBuf> {
        let roots = self.index.borrow().roots.clone();
        if roots.is_empty() {
            self.engine.roots()
        } else {
            normalize_roots(&roots)
        }
    }

    /// Changes the folders to index and rebuilds the index.
    fn set_index(&self, roots: Vec<PathBuf>, excludes: Vec<PathBuf>) {
        let result = if self.read_error.is_some() && self.index.borrow().saved {
            self.index.borrow_mut().change(roots, excludes, None)
        } else {
            let path = self.path.clone();
            self.index
                .borrow_mut()
                .change(roots, excludes, path.as_deref())
        };
        self.set_error(result.err().map(|e| labels::cannot_save(&e)));
        self.rebuild();
    }

    fn rebuild(&self) {
        let (roots, excludes) = {
            let index = self.index.borrow();
            let roots = if index.roots.is_empty() {
                self.engine.roots()
            } else {
                index.roots.clone()
            };
            (roots, index.excludes.clone())
        };
        self.engine.reindex(&roots, excludes);
    }

    /// Shows the settings window at `page` (see [`PAGES`]), or brings it
    /// forward.
    pub fn open(self: &Rc<Self>, page: Option<usize>) {
        if self.window.borrow().is_none() {
            match SettingsWindow::new() {
                Ok(w) => {
                    self.connect(&w);
                    *self.window.borrow_mut() = Some(w);
                }
                Err(e) => {
                    eprintln!("everygrep-gui: cannot open the settings window: {e}");
                    return;
                }
            }
        }
        let window = self.window.borrow();
        let Some(w) = window.as_ref() else { return };
        if let Some(page) = page {
            w.set_page(page.min(PAGES.len() - 1) as i32);
        }
        self.fill(w);
        // As for the main window (see `raise` in main.rs): showing again
        // gives a new surface, which GNOME brings to the front.
        let _ = w.hide();
        let _ = w.show();
        w.invoke_focus_pages();
    }

    /// Updates the settings window, if it is open.
    fn refresh(&self) {
        if let Ok(window) = self.window.try_borrow() {
            if let Some(w) = window.as_ref() {
                self.fill(w);
            }
        }
    }

    fn fill(&self, w: &SettingsWindow) {
        let items = |paths: &[PathBuf]| -> ModelRc<StandardListViewItem> {
            let items: Vec<StandardListViewItem> = paths
                .iter()
                .map(|p| StandardListViewItem::from(SharedString::from(p.display().to_string())))
                .collect();
            ModelRc::new(VecModel::from(items))
        };
        let roots = self.shown_roots();
        let index = self.index.borrow();
        w.set_roots(items(&roots));
        w.set_roots_count(labels::folder_count(roots.len()).into());
        w.set_excludes(items(&index.excludes));
        w.set_excludes_count(labels::folder_count(index.excludes.len()).into());
        w.set_index_note(if index.saved {
            "".into()
        } else {
            labels::INDEX_FROM_COMMAND_LINE.into()
        });
        let (state, indexed) = self.state.get();
        w.set_index_status(labels::index_status(state, indexed).into());

        let s = self.settings.borrow();
        let filters: Vec<SharedString> = Filter::ALL
            .iter()
            .map(|&f| labels::filter(f).into())
            .collect();
        w.set_filter_names(ModelRc::new(VecModel::from(filters)));
        let o = &s.options;
        w.set_default_filter(Filter::ALL.iter().position(|&f| f == o.filter).unwrap_or(0) as i32);
        w.set_default_case(o.case_sensitive);
        w.set_default_words(o.whole_words);
        w.set_default_path(o.match_path);
        w.set_default_diacritics(o.match_diacritics);
        w.set_history_max(MAX_HISTORY as i32);
        w.set_history_size(s.history_size as i32);
        w.set_history_on_disk(s.history_on_disk);

        let closes: Vec<SharedString> = OnClose::ALL
            .iter()
            .map(|&o| labels::on_close(o).into())
            .collect();
        w.set_close_names(ModelRc::new(VecModel::from(closes)));
        w.set_on_close(
            OnClose::ALL
                .iter()
                .position(|&o| o == s.on_close)
                .unwrap_or(0) as i32,
        );
        w.set_on_close_note(labels::on_close_note(s.on_close).into());
        w.set_remember_columns(s.remember_columns);

        w.set_command(COMMAND.into());
        w.set_shortcut_steps(labels::SHORTCUT_STEPS.into());
        w.set_error(self.error.borrow().as_str().into());
    }

    fn connect(self: &Rc<Self>, w: &SettingsWindow) {
        let me: Weak<Prefs> = Rc::downgrade(self);
        let on = |f: fn(&Rc<Prefs>)| {
            let me = me.clone();
            move || {
                if let Some(p) = me.upgrade() {
                    f(&p);
                }
            }
        };
        let on_index = |f: fn(&Rc<Prefs>, usize)| {
            let me = me.clone();
            move |i: i32| {
                if let (Some(p), Ok(i)) = (me.upgrade(), usize::try_from(i)) {
                    f(&p, i);
                }
            }
        };
        w.on_add_roots(on(|p| p.choose(true)));
        w.on_add_excludes(on(|p| p.choose(false)));
        w.on_remove_root(on_index(|p, i| {
            let mut roots = p.shown_roots();
            if i < roots.len() && roots.len() > 1 {
                roots.remove(i);
                let excludes = p.index.borrow().excludes.clone();
                p.set_index(roots, excludes);
            }
        }));
        w.on_remove_exclude(on_index(|p, i| {
            let mut excludes = p.index.borrow().excludes.clone();
            if i < excludes.len() {
                excludes.remove(i);
                let roots = p.index.borrow().roots.clone();
                p.set_index(roots, excludes);
            }
        }));
        w.on_rebuild(on(|p| p.rebuild()));
        w.on_search_changed(on(|p| p.search_changed()));
        w.on_window_changed(on(|p| p.window_changed()));
        w.on_copy_command(on(|p| {
            if let Some(w) = p.window.borrow().as_ref() {
                w.invoke_copy_text(COMMAND.into());
            }
            if let Some(m) = p.main.upgrade() {
                m.set_notice_error(false);
                m.set_notice(labels::COMMAND_COPIED.into());
            }
        }));
        w.on_reset_page(on_index(|p, i| p.reset(i)));
        w.on_close_window(on(|p| {
            if let Some(w) = p.window.borrow().as_ref() {
                let _ = w.hide();
            }
        }));
    }

    fn search_changed(&self) {
        let Some(w) = self.window.borrow().as_ref().map(|w| w.as_weak()) else {
            return;
        };
        let Some(w) = w.upgrade() else { return };
        let options = QueryOptions {
            case_sensitive: w.get_default_case(),
            whole_words: w.get_default_words(),
            match_path: w.get_default_path(),
            match_diacritics: w.get_default_diacritics(),
            filter: usize::try_from(w.get_default_filter())
                .ok()
                .and_then(|i| Filter::ALL.get(i).copied())
                .unwrap_or_default(),
        };
        let size = usize::try_from(w.get_history_size())
            .unwrap_or(0)
            .min(MAX_HISTORY);
        let on_disk = w.get_history_on_disk();
        self.change(|s| {
            s.options = options;
            s.history_size = size;
            s.history_on_disk = on_disk;
        });
        self.apply_history();
    }

    /// Applies the history settings: its size, and keeping it on disk or
    /// removing the saved copy.
    fn apply_history(&self) {
        let (size, on_disk) = {
            let s = self.settings.borrow();
            (s.history_size, s.history_on_disk)
        };
        self.history.borrow_mut().set_max(size);
        if on_disk && size > 0 {
            self.save_history();
        } else if self.read_error.is_none() {
            if let Some(path) = settings::history_path() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn window_changed(&self) {
        let Some(w) = self.window.borrow().as_ref().map(|w| w.as_weak()) else {
            return;
        };
        let Some(w) = w.upgrade() else { return };
        let on_close = usize::try_from(w.get_on_close())
            .ok()
            .and_then(|i| OnClose::ALL.get(i).copied())
            .unwrap_or_default();
        let remember = w.get_remember_columns();
        self.change(|s| {
            s.on_close = on_close;
            s.remember_columns = remember;
            if !remember {
                s.columns = Columns::default();
            }
        });
        self.refresh();
    }

    fn reset(&self, page: usize) {
        let page = match page {
            0 => Page::Indexing,
            1 => Page::Search,
            2 => Page::Window,
            _ => return,
        };
        if page == Page::Indexing {
            let saved = self.index.borrow().saved;
            if saved {
                self.set_index(Vec::new(), Vec::new());
            } else {
                // A command-line window goes back to its own folders.
                let roots = self.index.borrow().roots.clone();
                self.set_index(roots, Vec::new());
            }
            return;
        }
        self.change(|s| s.reset(page));
        if page == Page::Search {
            self.apply_history();
        }
        self.refresh();
    }

    /// Opens the folder chooser on a helper thread; the chosen folders are
    /// added to the folders to index (`roots`) or to the excluded ones.
    fn choose(&self, roots: bool) {
        if self.choosing.replace(true) {
            return;
        }
        let title = if roots {
            labels::CHOOSE_ROOTS
        } else {
            labels::CHOOSE_EXCLUDES
        };
        std::thread::spawn(move || {
            let result = crate::portal::choose_folders(title, labels::CHOOSE_ACCEPT);
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    p.choosing.set(false);
                    match result {
                        Ok(Some(chosen)) if !chosen.is_empty() => p.add(roots, &chosen),
                        Ok(_) => {}
                        Err(e) => p.set_error(Some(labels::chooser_failed(&e))),
                    }
                });
            });
        });
    }

    fn add(&self, roots: bool, chosen: &[PathBuf]) {
        let chosen = chosen
            .iter()
            .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()));
        let mut list_roots = self.shown_roots();
        let mut excludes = self.index.borrow().excludes.clone();
        if roots {
            list_roots.extend(chosen);
            list_roots = normalize_roots(&list_roots);
        } else {
            for c in chosen {
                if !excludes.contains(&c) {
                    excludes.push(c);
                }
            }
        }
        self.set_index(list_roots, excludes);
    }

    /// Whether closing the main window should only hide it.
    pub fn hide_on_close(&self) -> bool {
        self.settings.borrow().on_close == OnClose::Hide
    }
}

/// The page index for a `--settings=<page>` name.
pub fn page_index(name: &str) -> Option<usize> {
    PAGES.iter().position(|p| *p == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;

    #[test]
    fn command_line_folders_are_never_saved() {
        let dir = Scratch::new("setup");
        let path = dir.0.join("settings");
        let mut saved = Settings::default();
        saved.roots = vec![PathBuf::from("/home/me")];
        saved.excludes = vec![PathBuf::from("/home/me/.cache")];
        saved.save(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        for (roots, file) in [
            (vec![PathBuf::from("/tmp/demo")], None),
            (vec![], Some(PathBuf::from("/tmp/demo-index.bin"))),
            (
                vec![PathBuf::from("/tmp/demo")],
                Some(PathBuf::from("/tmp/i.bin")),
            ),
        ] {
            let mut setup = IndexSetup::new(&roots, file.as_deref(), &saved);
            assert!(!setup.saved);
            assert_eq!(setup.roots, roots);
            assert!(setup.excludes.is_empty(), "saved excludes stay out");
            setup
                .change(
                    vec![PathBuf::from("/tmp/demo"), PathBuf::from("/tmp/more")],
                    vec![PathBuf::from("/tmp/demo/x")],
                    Some(&path),
                )
                .unwrap();
            assert_eq!(setup.roots.len(), 2);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[test]
    fn default_window_uses_and_saves_the_settings() {
        let dir = Scratch::new("setup-saved");
        let path = dir.0.join("settings");
        let s = Settings::parse("root = /home/me\nexclude = /home/me/tmp\nfilter = code\n");
        let mut setup = IndexSetup::new(&[], None, &s);
        assert!(setup.saved);
        assert_eq!(setup.roots, [PathBuf::from("/home/me")]);
        assert_eq!(setup.excludes, [PathBuf::from("/home/me/tmp")]);
        setup
            .change(vec![PathBuf::from("/srv")], Vec::new(), Some(&path))
            .unwrap();
        let back = Settings::load(&path).unwrap();
        assert_eq!(back.roots, [PathBuf::from("/srv")]);
        assert!(back.excludes.is_empty());
        // Without saved folders the window keeps today's behaviour.
        let setup = IndexSetup::new(&[], None, &Settings::default());
        assert!(setup.saved && setup.roots.is_empty() && setup.excludes.is_empty());
    }

    #[test]
    fn page_names() {
        assert_eq!(page_index("indexing"), Some(0));
        assert_eq!(page_index("shortcut"), Some(3));
        assert_eq!(page_index("nope"), None);
    }
}
