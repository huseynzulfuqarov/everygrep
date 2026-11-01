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
use everygrep_core::rules::{IndexRules, RuleSpec};
use everygrep_core::scan::normalize_roots;
use everygrep_core::sort::FolderSize;
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};

use crate::MenuChoice;

use crate::autostart;
use crate::commands::{self, COMMANDS, Keymap};
use crate::engine::{Engine, IndexState};
use crate::history::{self, History};
use crate::i18n;
use crate::look::{self, Scheme};
use crate::portal::ColorScheme;
use crate::settings::{
    self, Columns, Density, LanguageChoice, MAX_HISTORY, OnClose, Page, Settings, ThemeChoice,
};
use crate::{AppWindow, Command, DriveRow, SettingsWindow, ShortcutCommand, labels};

/// Names of the settings pages for `--settings=<page>`, in page order.
pub const PAGES: [&str; 5] = ["indexing", "search", "appearance", "window", "keyboard"];

/// The page with the keyboard settings.
pub const KEYBOARD_PAGE: usize = 4;

/// The command a desktop shortcut runs, and the flags of its variants.
const COMMAND: &str = "everygrep-gui";

/// The commands on the Keyboard page for a desktop shortcut: plain, toggle,
/// new window.
fn shortcut_commands() -> [String; 3] {
    [
        COMMAND.to_owned(),
        format!("{COMMAND} --toggle"),
        format!("{COMMAND} --new-window"),
    ]
}

/// Default widths of the main window's columns, in pixels (see app.slint).
const SIZE_WIDTH: f32 = 96.0;
const DATE_WIDTH: f32 = 120.0;
const TYPE_WIDTH: f32 = 130.0;
const EXT_WIDTH: f32 = 80.0;
const CREATED_WIDTH: f32 = 120.0;
const ACCESSED_WIDTH: f32 = 120.0;
const ATTRS_WIDTH: f32 = 64.0;

/// The folders a window indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSetup {
    /// Folders to index; empty stands for the saved index's folders, or the
    /// home folder.
    pub roots: Vec<PathBuf>,
    /// Excluded folders, file filters and their switch.
    pub rules: RuleSpec,
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
                rules: s.rule_spec(),
                saved: true,
            }
        } else {
            Self {
                roots: roots.to_vec(),
                rules: RuleSpec::default(),
                saved: false,
            }
        }
    }

    /// The compiled rules. The saved ones were checked when they were
    /// read, so they compile; anything else leaves nothing out.
    pub fn rules(&self) -> IndexRules {
        IndexRules::new(self.rules.clone()).unwrap_or_default()
    }

    /// Changes the folders, and the settings file at `path` when they are
    /// the saved ones.
    pub fn change(
        &mut self,
        roots: Vec<PathBuf>,
        rules: RuleSpec,
        path: Option<&Path>,
    ) -> std::io::Result<()> {
        self.roots = roots;
        self.rules = rules;
        match path {
            Some(path) if self.saved => settings::update(path, |s| {
                s.roots = self.roots.clone();
                s.set_rule_spec(&self.rules);
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
    /// Why the last change of the rules was refused, shown next to them.
    rule_error: RefCell<String>,
    state: Cell<(IndexState, usize)>,
    /// A folder chooser is open.
    choosing: Cell<bool>,
    engine: Arc<Engine>,
    main: slint::Weak<AppWindow>,
    pub history: RefCell<History>,
    window: RefCell<Option<SettingsWindow>>,
    /// The desktop's light or dark setting, once the portal told it.
    desktop_scheme: Cell<Option<ColorScheme>>,
    /// Whether the windows were given colors; until then they follow the
    /// toolkit's own reading of the desktop setting.
    scheme_set: Cell<bool>,
    /// Whether the language was given to the `.slint` texts.
    language_set: Cell<bool>,
    /// UUIDs of the drives in the settings window's list, in its order.
    listed_drives: RefCell<Vec<String>>,
    /// The Keyboard page: the text of its filter, the commands listed for
    /// it in order, and the chosen command.
    key_filter: RefCell<String>,
    listed_commands: RefCell<Vec<Command>>,
    key_chosen: Cell<Option<Command>>,
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

/// Reads the settings file and chooses the language of the texts made in
/// Rust from it. A file that cannot be read gives the defaults and the
/// reason.
pub fn load() -> (Option<PathBuf>, Settings, Option<String>) {
    let Some(path) = settings::path() else {
        i18n::set(LanguageChoice::System.resolve());
        return (None, Settings::default(), None);
    };
    let (settings, error) = match Settings::load(&path) {
        Ok(s) => (s, None),
        Err(e) => (Settings::default(), Some(e)),
    };
    i18n::set(settings.language.resolve());
    let why = error.map(|e| labels::cannot_read(&path, &e));
    (Some(path), settings, why)
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
            (None, None) => labels::no_settings_folder().to_owned(),
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
            rule_error: RefCell::default(),
            state: Cell::new((IndexState::Loading, 0)),
            choosing: Cell::new(false),
            engine,
            main: main.as_weak(),
            window: RefCell::default(),
            desktop_scheme: Cell::new(None),
            scheme_set: Cell::new(false),
            language_set: Cell::new(false),
            listed_drives: RefCell::default(),
            key_filter: RefCell::default(),
            listed_commands: RefCell::default(),
            key_chosen: Cell::new(None),
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

    /// Notes the desktop's light or dark setting and recolors the windows
    /// when the theme follows it.
    pub fn set_desktop_scheme(&self, scheme: ColorScheme) {
        self.desktop_scheme.set(Some(scheme));
        self.apply_theme();
    }

    /// Gives the windows the colors of the theme setting.
    pub fn apply_theme(&self) {
        let scheme = look::resolve(self.settings.borrow().theme, self.desktop_scheme.get());
        // Until the desktop's setting is known the toolkit's own reading of
        // it stays in place, so the first frame has the right colors.
        if scheme == Scheme::Toolkit && !self.scheme_set.get() {
            return;
        }
        self.scheme_set.set(true);
        if let Some(m) = self.main.upgrade() {
            m.invoke_set_color_scheme(scheme as i32);
        }
        if let Ok(window) = self.window.try_borrow() {
            if let Some(w) = window.as_ref() {
                w.invoke_set_color_scheme(scheme as i32);
            }
        }
    }

    /// Shows or hides the main window's menu bar.
    pub fn apply_menu_bar(&self) {
        if let Some(m) = self.main.upgrade() {
            m.set_show_menu_bar(self.settings.borrow().menu_bar);
        }
    }

    /// Changes whether the main window has a menu bar.
    pub fn set_menu_bar(&self, on: bool) {
        self.change(|s| s.menu_bar = on);
        self.apply_menu_bar();
        self.refresh();
    }

    /// Changes what folders show in the Size column.
    pub fn set_folder_size(&self, folder_size: FolderSize) {
        self.change(|s| s.folder_size = folder_size);
        self.apply_folder_size();
        self.refresh();
    }

    /// Puts the check marks of the main window's View menu on the chosen
    /// theme, density and folder size.
    pub fn fill_menus(&self) {
        let Some(m) = self.main.upgrade() else { return };
        let s = self.settings.borrow();
        m.set_theme_items(choices(&ThemeChoice::ALL, s.theme, labels::theme));
        m.set_density_items(choices(&Density::ALL, s.density, labels::density));
        m.set_folder_size_items(choices(
            &FolderSize::ALL,
            s.folder_size,
            labels::folder_size,
        ));
        m.set_status_details(s.status_details);
        m.set_show_menu_bar(s.menu_bar);
    }

    /// Packs the main window's list as the density setting says.
    pub fn apply_density(&self) {
        let compact = self.settings.borrow().density == Density::Compact;
        if let Some(m) = self.main.upgrade() {
            m.invoke_set_compact(compact);
        }
    }

    /// Shows or hides the details of the selection in the status line.
    pub fn apply_status_details(&self) {
        let on = self.settings.borrow().status_details;
        if let Some(m) = self.main.upgrade() {
            crate::with_model(&m, |model| model.set_details(on));
            crate::refresh_status(&m);
        }
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
        w.set_show_created(w.get_show_created() || c.show_created);
        w.set_show_accessed(w.get_show_accessed() || c.show_accessed);
        w.set_show_attrs(w.get_show_attrs() || c.show_attrs);
        if let Some(n) = c.name {
            w.set_name_want(n);
            w.set_name_set(true);
        }
        c.size.into_iter().for_each(|v| w.set_size_want(v));
        c.date.into_iter().for_each(|v| w.set_date_want(v));
        c.type_.into_iter().for_each(|v| w.set_type_want(v));
        c.ext.into_iter().for_each(|v| w.set_ext_want(v));
        c.created.into_iter().for_each(|v| w.set_created_want(v));
        c.accessed.into_iter().for_each(|v| w.set_accessed_want(v));
        c.attrs.into_iter().for_each(|v| w.set_attr_want(v));
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
            show_created: w.get_show_created(),
            show_accessed: w.get_show_accessed(),
            show_attrs: w.get_show_attrs(),
            name: w.get_name_set().then(|| w.get_name_want().round()),
            size: differs(w.get_size_want(), SIZE_WIDTH),
            date: differs(w.get_date_want(), DATE_WIDTH),
            type_: differs(w.get_type_want(), TYPE_WIDTH),
            ext: differs(w.get_ext_want(), EXT_WIDTH),
            created: differs(w.get_created_want(), CREATED_WIDTH),
            accessed: differs(w.get_accessed_want(), ACCESSED_WIDTH),
            attrs: differs(w.get_attr_want(), ATTRS_WIDTH),
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

    /// Changes the folders to index and the rules, and rebuilds the index.
    /// Rules that do not compile change nothing: the reason is shown on the
    /// Indexing page and false returned.
    fn set_index(&self, roots: Vec<PathBuf>, rules: RuleSpec) -> bool {
        if let Err(e) = IndexRules::new(rules.clone()) {
            *self.rule_error.borrow_mut() = labels::bad_rule(&e);
            self.refresh();
            return false;
        }
        self.rule_error.borrow_mut().clear();
        let result = if self.read_error.is_some() && self.index.borrow().saved {
            self.index.borrow_mut().change(roots, rules, None)
        } else {
            let path = self.path.clone();
            self.index
                .borrow_mut()
                .change(roots, rules, path.as_deref())
        };
        self.set_error(result.err().map(|e| labels::cannot_save(&e)));
        self.rebuild();
        true
    }

    /// The rules being edited, with `f` applied.
    fn rules_with(&self, f: impl FnOnce(&mut RuleSpec)) -> RuleSpec {
        let mut rules = self.index.borrow().rules.clone();
        f(&mut rules);
        rules
    }

    /// The Indexing page's switch or file filters changed.
    fn rules_changed(&self) {
        let Some(w) = self.window.borrow().as_ref().map(|w| w.as_weak()) else {
            return;
        };
        let Some(w) = w.upgrade() else { return };
        let rules = self.rules_with(|r| {
            r.enabled = w.get_use_excludes();
            r.include_files = w.get_include_files().trim().to_owned();
            r.exclude_files = w.get_exclude_files().trim().to_owned();
        });
        if rules == self.index.borrow().rules {
            self.rule_error.borrow_mut().clear();
            self.refresh();
            return;
        }
        let roots = self.index.borrow().roots.clone();
        if self.set_index(roots, rules) {
            self.fill_rule_texts(&w);
        }
    }

    /// The Indexing page's "Watch for changes" switch changed.
    fn watch_changed(&self) {
        let Some(on) = self.window.borrow().as_ref().map(|w| w.get_watch_changes()) else {
            return;
        };
        self.change(|s| s.watch_changes = on);
        self.engine.set_watching(on);
        self.refresh();
    }

    /// Adds a folder pattern typed on the Indexing page.
    fn add_exclude_pattern(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let item = PathBuf::from(text);
        if self.index.borrow().rules.folders.contains(&item) {
            self.clear_pattern_field();
            return;
        }
        let rules = self.rules_with(|r| r.folders.push(item));
        let roots = self.index.borrow().roots.clone();
        if self.set_index(roots, rules) {
            self.clear_pattern_field();
        }
    }

    fn clear_pattern_field(&self) {
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_exclude_pattern("".into());
        }
    }

    /// Puts the saved file filters into the text fields. Not part of
    /// [`fill`](Self::fill), which runs on every index state change and
    /// would undo typing.
    fn fill_rule_texts(&self, w: &SettingsWindow) {
        let index = self.index.borrow();
        w.set_include_files(index.rules.include_files.as_str().into());
        w.set_exclude_files(index.rules.exclude_files.as_str().into());
    }

    fn rebuild(&self) {
        let (roots, rules) = {
            let index = self.index.borrow();
            let roots = if index.roots.is_empty() {
                self.engine.roots()
            } else {
                index.roots.clone()
            };
            (roots, index.rules())
        };
        self.engine.reindex(&roots, rules);
    }

    /// Shows the settings window at `page` (see [`PAGES`]), or brings it
    /// forward.
    pub fn open(self: &Rc<Self>, page: Option<usize>) {
        if self.window.borrow().is_none() {
            match SettingsWindow::new() {
                Ok(w) => {
                    self.connect(&w);
                    *self.window.borrow_mut() = Some(w);
                    self.apply_theme();
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
        self.fill_rule_texts(w);
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
        w.set_excludes(items(&index.rules.folders));
        w.set_excludes_count(labels::folder_count(index.rules.folders.len()).into());
        w.set_use_excludes(index.rules.enabled);
        w.set_watch_changes(self.settings.borrow().watch_changes);
        w.set_rule_error(self.rule_error.borrow().as_str().into());
        w.set_index_note(if index.saved {
            "".into()
        } else {
            labels::index_from_command_line().into()
        });
        let (state, indexed) = self.state.get();
        w.set_index_status(labels::index_status(state, indexed).into());

        let s = self.settings.borrow();
        w.set_drives_enabled(self.engine.drives_enabled());
        w.set_index_drives(s.drives.auto);
        let removed: Vec<SharedString> = [true, false]
            .into_iter()
            .map(|keep| labels::removed_drives(keep).into())
            .collect();
        w.set_removed_names(ModelRc::new(VecModel::from(removed)));
        w.set_removed_choice(if s.drives.keep_offline { 0 } else { 1 });
        let drives = self.engine.drives();
        let zone = everygrep_core::localtime::Zone::local();
        let rows: Vec<DriveRow> = drives
            .iter()
            .map(|d| DriveRow {
                title: labels::drive_title(&d.label, &d.uuid).into(),
                details: labels::drive_details(d, &zone).into(),
                online: d.online,
                can_forget: !d.online && (d.indexed || d.ignored || d.last_seen > 0),
                action: labels::drive_action(d.ignored || !d.indexed).into(),
            })
            .collect();
        *self.listed_drives.borrow_mut() = drives.into_iter().map(|d| d.uuid).collect();
        w.set_drives(ModelRc::new(VecModel::from(rows)));
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
        w.set_default_prefix(o.prefix);
        w.set_default_suffix(o.suffix);
        w.set_default_punctuation(o.ignore_punctuation);
        w.set_default_whitespace(o.ignore_whitespace);
        w.set_default_regex(o.regex);
        w.set_as_you_type(s.as_you_type);
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
        w.set_start_on_login(autostart::path().is_some_and(|p| autostart::is_enabled(&p)));
        w.set_start_on_login_note(labels::start_on_login_note(s.on_close).into());
        let sizes: Vec<SharedString> = FolderSize::ALL
            .iter()
            .map(|&f| labels::folder_size(f).into())
            .collect();
        w.set_folder_size_names(ModelRc::new(VecModel::from(sizes)));
        w.set_folder_size(
            FolderSize::ALL
                .iter()
                .position(|&f| f == s.folder_size)
                .unwrap_or(0) as i32,
        );
        w.set_folder_size_note(labels::folder_size_note(s.folder_size).into());
        w.set_remember_columns(s.remember_columns);
        w.set_rename_name_only(s.rename_name_only);
        w.set_focus_on_show(s.focus_on_show);

        let themes: Vec<SharedString> = ThemeChoice::ALL
            .iter()
            .map(|&t| labels::theme(t).into())
            .collect();
        w.set_theme_names(ModelRc::new(VecModel::from(themes)));
        w.set_theme(
            ThemeChoice::ALL
                .iter()
                .position(|&t| t == s.theme)
                .unwrap_or(0) as i32,
        );
        let densities: Vec<SharedString> = Density::ALL
            .iter()
            .map(|&d| labels::density(d).into())
            .collect();
        w.set_status_details(s.status_details);
        w.set_menu_bar(s.menu_bar);
        w.set_density_names(ModelRc::new(VecModel::from(densities)));
        w.set_density(
            Density::ALL
                .iter()
                .position(|&d| d == s.density)
                .unwrap_or(0) as i32,
        );
        let languages: Vec<SharedString> = LanguageChoice::ALL
            .iter()
            .map(|&l| labels::language(l).into())
            .collect();
        w.set_language_names(ModelRc::new(VecModel::from(languages)));
        w.set_language(
            LanguageChoice::ALL
                .iter()
                .position(|&l| l == s.language)
                .unwrap_or(0) as i32,
        );

        let commands: Vec<ShortcutCommand> = shortcut_commands()
            .into_iter()
            .zip(labels::shortcut_commands())
            .map(|(command, (title, note))| ShortcutCommand {
                title: title.into(),
                command: command.into(),
                note: note.into(),
            })
            .collect();
        w.set_commands(ModelRc::new(VecModel::from(commands)));
        w.set_shortcut_steps(labels::shortcut_steps().into());
        self.fill_keys(w);
        w.set_error(self.error.borrow().as_str().into());
    }

    /// The Keyboard page: the commands that match the filter with their
    /// keys, and the keys of the chosen one.
    fn fill_keys(&self, w: &SettingsWindow) {
        let item = |text: String| StandardListViewItem::from(SharedString::from(text));
        let filter = self.key_filter.borrow().clone();
        let (rows, listed, bindings) = commands::with_current(|k| {
            let listed: Vec<Command> = COMMANDS
                .iter()
                .filter(|spec| commands::matches_filter(&filter, spec, k))
                .map(|spec| spec.cmd)
                .collect();
            let rows: Vec<StandardListViewItem> = listed
                .iter()
                .filter_map(|&c| commands::spec(c))
                .map(|spec| {
                    let keys: Vec<String> = k
                        .of(spec.cmd)
                        .map(|b| commands::binding_text(b.scope, b.chord))
                        .collect();
                    let head = format!("{} | {}", spec.category.name(), (spec.label)());
                    item(if keys.is_empty() {
                        head
                    } else {
                        format!("{head}  —  {}", keys.join(", "))
                    })
                })
                .collect();
            let bindings: Vec<StandardListViewItem> = self
                .key_chosen
                .get()
                .map(|c| {
                    k.of(c)
                        .map(|b| item(commands::binding_text(b.scope, b.chord)))
                        .collect()
                })
                .unwrap_or_default();
            (rows, listed, bindings)
        });
        let current = self
            .key_chosen
            .get()
            .and_then(|c| listed.iter().position(|&l| l == c))
            .map_or(-1, |i| i as i32);
        if bindings.len() as i32 <= w.get_key_binding_current() {
            w.set_key_binding_current(-1);
        }
        // The lists are replaced only when they changed, so a refresh keeps
        // their scroll position.
        let same = |model: ModelRc<StandardListViewItem>, rows: &[StandardListViewItem]| {
            model.row_count() == rows.len() && model.iter().zip(rows).all(|(a, b)| a.text == b.text)
        };
        if !same(w.get_key_commands(), &rows) {
            w.set_key_commands(ModelRc::new(VecModel::from(rows)));
        }
        w.set_key_current(current);
        if !same(w.get_key_bindings(), &bindings) {
            w.set_key_bindings(ModelRc::new(VecModel::from(bindings)));
        }
        *self.listed_commands.borrow_mut() = listed;
    }

    /// Uses `keymap` from now on: saves the commands whose keys differ from
    /// the defaults and gives the main window the new keys.
    fn set_keymap(&self, keymap: Keymap) {
        let overrides = keymap.overrides();
        commands::set_current(keymap);
        self.change(|s| s.set_shortcuts(overrides.clone()));
        if let Some(m) = self.main.upgrade() {
            crate::apply_keys(&m);
        }
        self.refresh();
    }

    /// Puts every command back to its default keys.
    fn restore_keys(&self) {
        commands::set_current(Keymap::default());
        self.change(|s| s.reset_shortcuts());
        if let Some(m) = self.main.upgrade() {
            crate::apply_keys(&m);
        }
        self.refresh();
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
                let rules = p.index.borrow().rules.clone();
                p.set_index(roots, rules);
            }
        }));
        w.on_remove_exclude(on_index(|p, i| {
            if i < p.index.borrow().rules.folders.len() {
                let rules = p.rules_with(|r| {
                    r.folders.remove(i);
                });
                let roots = p.index.borrow().roots.clone();
                p.set_index(roots, rules);
            }
        }));
        w.on_rules_changed(on(|p| p.rules_changed()));
        w.on_watch_changed(on(|p| p.watch_changed()));
        w.on_add_exclude_pattern({
            let me = me.clone();
            move |text| {
                if let Some(p) = me.upgrade() {
                    p.add_exclude_pattern(&text);
                }
            }
        });
        w.on_rebuild(on(|p| p.rebuild()));
        w.on_drives_changed(on(|p| p.drives_changed()));
        w.on_forget_drive(on_index(|p, i| {
            let uuid = p.listed_drives.borrow().get(i).cloned();
            if let Some(uuid) = uuid {
                p.engine.forget_drive(&uuid);
            }
        }));
        w.on_toggle_drive(on_index(|p, i| {
            let uuid = p.listed_drives.borrow().get(i).cloned();
            if let Some(uuid) = uuid {
                p.toggle_drive(&uuid);
            }
        }));
        w.on_search_changed(on(|p| p.search_changed()));
        w.on_window_changed(on(|p| p.window_changed()));
        w.on_start_on_login_changed({
            let me = me.clone();
            move |on| {
                if let Some(p) = me.upgrade() {
                    p.set_start_on_login(on);
                }
            }
        });
        w.on_appearance_changed(on(|p| p.appearance_changed()));
        w.on_copy_command(on_index(|p, i| {
            let Some(command) = shortcut_commands().into_iter().nth(i) else {
                return;
            };
            if let Some(w) = p.window.borrow().as_ref() {
                w.invoke_copy_text(command.into());
            }
            if let Some(m) = p.main.upgrade() {
                m.set_notice_error(false);
                m.set_notice(labels::command_copied().into());
            }
        }));
        w.on_key_filter_edited({
            let me = me.clone();
            move |text| {
                if let Some(p) = me.upgrade() {
                    *p.key_filter.borrow_mut() = text.to_string();
                    p.refresh();
                }
            }
        });
        w.on_key_command_chosen({
            let me = me.clone();
            move |i| {
                if let Some(p) = me.upgrade() {
                    let chosen = usize::try_from(i)
                        .ok()
                        .and_then(|i| p.listed_commands.borrow().get(i).copied());
                    if chosen != p.key_chosen.get() {
                        p.key_chosen.set(chosen);
                        if let Some(w) = p.window.borrow().as_ref() {
                            w.set_key_binding_current(-1);
                        }
                    }
                    p.refresh();
                }
            }
        });
        w.on_remove_binding(on_index(|p, i| {
            let Some(cmd) = p.key_chosen.get() else {
                return;
            };
            let mut keymap = commands::with_current(Keymap::clone);
            if keymap.remove(cmd, i) {
                if let Some(w) = p.window.borrow().as_ref() {
                    w.set_key_binding_current(-1);
                }
                p.set_keymap(keymap);
            }
        }));
        w.on_restore_keys(on(|p| p.restore_keys()));
        w.on_reset_page(on_index(|p, i| p.reset(i)));
        w.on_close_window(on(|p| {
            if let Some(w) = p.window.borrow().as_ref() {
                let _ = w.hide();
            }
        }));
    }

    /// The drive settings of the Indexing page changed.
    fn drives_changed(&self) {
        let Some(w) = self.window.borrow().as_ref().map(|w| w.as_weak()) else {
            return;
        };
        let Some(w) = w.upgrade() else { return };
        let auto = w.get_index_drives();
        let keep = w.get_removed_choice() != 1;
        self.change(|s| {
            s.drives.auto = auto;
            s.drives.keep_offline = keep;
        });
        self.engine
            .set_drive_settings(self.settings.borrow().drives.clone());
        self.refresh();
    }

    /// Stops indexing a drive that is indexed, or indexes one that is not.
    fn toggle_drive(&self, uuid: &str) {
        let indexed = self
            .engine
            .drives()
            .iter()
            .any(|d| d.uuid == uuid && d.indexed && !d.ignored);
        self.change(|s| {
            s.drives.ignored.retain(|u| u != uuid);
            if indexed {
                s.drives.ignored.push(uuid.to_owned());
            }
        });
        self.engine
            .set_drive_settings(self.settings.borrow().drives.clone());
        self.engine.ignore_drive(uuid, indexed);
        self.refresh();
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
            prefix: w.get_default_prefix(),
            suffix: w.get_default_suffix(),
            ignore_punctuation: w.get_default_punctuation(),
            ignore_whitespace: w.get_default_whitespace(),
            regex: w.get_default_regex(),
            filter: usize::try_from(w.get_default_filter())
                .ok()
                .and_then(|i| Filter::ALL.get(i).copied())
                .unwrap_or_default(),
        };
        let size = usize::try_from(w.get_history_size())
            .unwrap_or(0)
            .min(MAX_HISTORY);
        let on_disk = w.get_history_on_disk();
        let as_you_type = w.get_as_you_type();
        self.change(|s| {
            s.options = options;
            s.as_you_type = as_you_type;
            s.history_size = size;
            s.history_on_disk = on_disk;
        });
        self.apply_history();
        self.apply_as_you_type();
    }

    /// Gives the main window the "search as you type" setting.
    pub fn apply_as_you_type(&self) {
        let on = self.settings.borrow().as_you_type;
        if let Some(m) = self.main.upgrade() {
            crate::set_as_you_type(&m, &self.engine, on);
        }
    }

    fn appearance_changed(&self) {
        let Some(w) = self.window.borrow().as_ref().map(|w| w.as_weak()) else {
            return;
        };
        let Some(w) = w.upgrade() else { return };
        let theme = usize::try_from(w.get_theme())
            .ok()
            .and_then(|i| ThemeChoice::ALL.get(i).copied())
            .unwrap_or_default();
        let density = usize::try_from(w.get_density())
            .ok()
            .and_then(|i| Density::ALL.get(i).copied())
            .unwrap_or_default();
        let details = w.get_status_details();
        let menu_bar = w.get_menu_bar();
        let language = usize::try_from(w.get_language())
            .ok()
            .and_then(|i| LanguageChoice::ALL.get(i).copied())
            .unwrap_or_default();
        self.change(|s| {
            s.theme = theme;
            s.density = density;
            s.status_details = details;
            s.menu_bar = menu_bar;
            s.language = language;
        });
        self.apply_look();
        self.refresh();
    }

    /// Applies the settings of the Appearance page to the windows.
    pub fn apply_look(&self) {
        self.apply_language();
        self.apply_theme();
        self.apply_density();
        self.apply_status_details();
        self.apply_menu_bar();
        self.fill_menus();
    }

    /// Shows the windows in the language of the language setting, when it
    /// is not the one shown. The `.slint` texts change at once; the texts
    /// made in Rust are set again.
    pub fn apply_language(&self) {
        let lang = self.settings.borrow().language.resolve();
        let ui_set = self.language_set.replace(true);
        if ui_set && lang == i18n::current() {
            return;
        }
        i18n::set(lang);
        i18n::apply_to_ui();
        if let Some(m) = self.main.upgrade() {
            crate::relabel(&m);
        }
        self.fill_menus();
        self.refresh();
    }

    /// Changes whether the status line describes the selection.
    pub fn set_status_details(&self, on: bool) {
        self.change(|s| s.status_details = on);
        self.apply_status_details();
        self.fill_menus();
        self.refresh();
    }

    /// Changes the density setting and repacks the list.
    pub fn set_density(&self, density: Density) {
        self.change(|s| s.density = density);
        self.apply_density();
        self.fill_menus();
        self.refresh();
    }

    /// Changes the theme setting and recolors the windows.
    pub fn set_theme(&self, theme: ThemeChoice) {
        self.change(|s| s.theme = theme);
        self.apply_theme();
        self.fill_menus();
        self.refresh();
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
        let folder_size = usize::try_from(w.get_folder_size())
            .ok()
            .and_then(|i| FolderSize::ALL.get(i).copied())
            .unwrap_or_default();
        let remember = w.get_remember_columns();
        let name_only = w.get_rename_name_only();
        let focus_on_show = w.get_focus_on_show();
        self.change(|s| {
            s.on_close = on_close;
            s.folder_size = folder_size;
            s.remember_columns = remember;
            s.rename_name_only = name_only;
            s.focus_on_show = focus_on_show;
            if !remember {
                s.columns = Columns::default();
            }
        });
        self.apply_folder_size();
        self.refresh();
    }

    /// Writes or removes the login start entry. The switch then shows
    /// whether the entry exists, so a failure leaves it as it was.
    fn set_start_on_login(&self, on: bool) {
        let result = match autostart::path() {
            Some(path) if on => {
                autostart::current_exe().and_then(|exe| autostart::enable(&path, &exe))
            }
            Some(path) => autostart::disable(&path),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no home folder",
            )),
        };
        match result {
            Ok(()) => self.refresh(),
            Err(e) => self.set_error(Some(labels::cannot_change_start_on_login(&e))),
        }
    }

    /// Shows the chosen folder size in the main window, when it changed.
    fn apply_folder_size(&self) {
        self.fill_menus();
        let folder_size = self.settings.borrow().folder_size;
        if let Some(m) = self.main.upgrade() {
            let mut shown = None;
            crate::with_model(&m, |model| shown = Some(model.folder_size()));
            if shown != Some(folder_size) {
                crate::set_folder_size(&m, &self.engine, folder_size);
            }
        }
    }

    fn reset(&self, page: usize) {
        let page = match page {
            0 => Page::Indexing,
            1 => Page::Search,
            2 => Page::Appearance,
            3 => Page::Window,
            _ => return,
        };
        if page == Page::Indexing {
            self.change(|s| {
                s.drives = Default::default();
                s.watch_changes = true;
            });
            self.engine.set_watching(true);
            self.engine
                .set_drive_settings(self.settings.borrow().drives.clone());
            let saved = self.index.borrow().saved;
            if saved {
                self.set_index(Vec::new(), RuleSpec::default());
            } else {
                // A command-line window goes back to its own folders.
                let roots = self.index.borrow().roots.clone();
                self.set_index(roots, RuleSpec::default());
            }
            if let Some(w) = self.window.borrow().as_ref() {
                self.fill_rule_texts(w);
                w.set_exclude_pattern("".into());
            }
            return;
        }
        self.change(|s| s.reset(page));
        if page == Page::Search {
            self.apply_history();
            self.apply_as_you_type();
        }
        if page == Page::Appearance {
            self.apply_look();
        }
        if page == Page::Window {
            self.apply_folder_size();
            self.set_start_on_login(false);
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
            labels::choose_roots()
        } else {
            labels::choose_excludes()
        };
        let accept = labels::choose_accept();
        std::thread::spawn(move || {
            let result = crate::portal::choose_folders(title, accept);
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
        let mut rules = self.index.borrow().rules.clone();
        if roots {
            list_roots.extend(chosen);
            list_roots = normalize_roots(&list_roots);
        } else {
            for c in chosen {
                if !rules.folders.contains(&c) {
                    rules.folders.push(c);
                }
            }
        }
        self.set_index(list_roots, rules);
    }

    /// Whether renaming preselects the name without its extension.
    pub fn rename_name_only(&self) -> bool {
        self.settings.borrow().rename_name_only
    }

    /// Whether closing the main window should only hide it.
    /// Whether showing the window focuses the search field.
    pub fn focus_on_show(&self) -> bool {
        self.settings.borrow().focus_on_show
    }

    pub fn hide_on_close(&self) -> bool {
        self.settings.borrow().on_close == OnClose::Hide
    }
}

/// Menu items for a group of choices, the chosen one checked. A new list
/// each time, so the menu shows fresh check marks.
pub fn choices<T: Copy + PartialEq>(
    all: &[T],
    chosen: T,
    name: fn(T) -> &'static str,
) -> ModelRc<MenuChoice> {
    let items: Vec<MenuChoice> = all
        .iter()
        .map(|&c| MenuChoice {
            title: name(c).into(),
            checked: c == chosen,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// The page index for a `--settings=<page>` name; `shortcut`, the
/// Keyboard page's old name, still opens it.
pub fn page_index(name: &str) -> Option<usize> {
    match name {
        "shortcut" => Some(KEYBOARD_PAGE),
        _ => PAGES.iter().position(|p| *p == name),
    }
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
            assert_eq!(setup.rules, RuleSpec::default(), "saved excludes stay out");
            setup
                .change(
                    vec![PathBuf::from("/tmp/demo"), PathBuf::from("/tmp/more")],
                    RuleSpec {
                        folders: vec![PathBuf::from("/tmp/demo/x")],
                        ..RuleSpec::default()
                    },
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
        assert_eq!(setup.rules.folders, [PathBuf::from("/home/me/tmp")]);
        let rules = RuleSpec {
            enabled: false,
            folders: vec!["*.git".into()],
            include_files: String::new(),
            exclude_files: "*.tmp".into(),
        };
        setup
            .change(vec![PathBuf::from("/srv")], rules.clone(), Some(&path))
            .unwrap();
        let back = Settings::load(&path).unwrap();
        assert_eq!(back.roots, [PathBuf::from("/srv")]);
        assert_eq!(back.rule_spec(), rules);
        assert_eq!(
            IndexSetup::new(&[], None, &back).rules(),
            IndexRules::new(rules).unwrap()
        );
        // Without saved folders the window keeps today's behaviour.
        let setup = IndexSetup::new(&[], None, &Settings::default());
        assert!(setup.saved && setup.roots.is_empty());
        assert_eq!(setup.rules, RuleSpec::default());
    }

    #[test]
    fn page_names() {
        assert_eq!(page_index("indexing"), Some(0));
        assert_eq!(page_index("appearance"), Some(2));
        assert_eq!(page_index("keyboard"), Some(4));
        assert_eq!(page_index("shortcut"), Some(4));
        assert_eq!(PAGES[KEYBOARD_PAGE], "keyboard");
        assert_eq!(page_index("nope"), None);
    }
}
