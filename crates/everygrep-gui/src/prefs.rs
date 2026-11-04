//! The settings window and applying settings to the running program.
//!
//! Every change is applied at once and written to the settings file (see
//! [`crate::settings`]). Folders to index and excluded folders are written
//! only for the window over the default index: a window started with
//! `--root` or `--index-file` changes its own folders for the session.

use crate::i18n::tr;

#[path = "prefs_filelists.rs"]
mod filelists;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, QueryOptions};
use everygrep_core::rules::{IndexRules, RuleSpec};
use everygrep_core::sort::FolderSize;
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};

use crate::MenuChoice;

use crate::autostart;
use crate::commands::{self, COMMANDS, Chord, Keymap, Scope};
use crate::engine::{Engine, IndexState};
use crate::format::{self, SizeStandard, SizeUnit};
use crate::history::History;
use crate::history_csv;
use crate::i18n;
use crate::look::{self, Scheme};
use crate::portal::ColorScheme;
use crate::schedule::{self, Schedule};
use crate::search_history::{self, Order, Searches};
use crate::settings::{
    self, Columns, Density, LanguageChoice, MAX_HISTORY, OnClose, Page, Settings, ThemeChoice,
};
use crate::{AppWindow, Command, DriveRow, HistoryWindow, SettingsWindow, ShortcutCommand, labels};

/// Names of the settings pages for `--settings=<page>`, in page order.
pub const PAGES: [&str; 6] = [
    "indexing",
    "search",
    "appearance",
    "window",
    "keyboard",
    "advanced",
];

/// The page with the keyboard settings.
pub const KEYBOARD_PAGE: usize = 4;

/// The command a desktop shortcut runs, and the flags of its variants.
const COMMAND: &str = "everygrep-gui";

/// How often the settings file is looked at for changes made outside the
/// window.
const RELOAD_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

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
const SIZE_FREQ_WIDTH: f32 = 110.0;
const EXT_FREQ_WIDTH: f32 = 110.0;
const NAME_FREQ_WIDTH: f32 = 110.0;
const CRC32_WIDTH: f32 = 90.0;
const SHA256_WIDTH: f32 = 300.0;
const MD5_WIDTH: f32 = 270.0;
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
        writer: &settings::Writer,
    ) {
        self.roots = roots;
        self.rules = rules;
        if let Some(path) = path.filter(|_| self.saved) {
            let (path, setup) = (path.to_path_buf(), self.clone());
            writer.run(move || {
                let failed = settings::update(&path, |s| setup.apply_to(s)).err();
                let _ = slint::invoke_from_event_loop(move || {
                    with(|p| p.set_error(failed.map(|e| labels::cannot_save(&e))));
                });
            });
        }
    }

    fn apply_to(&self, settings: &mut Settings) {
        if self.saved {
            settings.roots = self.roots.clone();
            settings.set_rule_spec(&self.rules);
        }
    }

    /// Takes over the folders and rules the engine indexes, `roots` (as
    /// [`everygrep_core::scan::normalize_roots`] gives them) and `rules`, after a new index was
    /// cancelled, and saves them as [`change`](Self::change) does. Folders
    /// left empty stay empty: they stand for the folders the index has.
    /// `resolve` gives the folders listed here as the engine resolved them
    /// (see [`crate::engine::Engine::resolved_roots`]), so a folder spelled
    /// through a link keeps its spelling. Returns whether anything changed.
    pub fn follow(
        &mut self,
        roots: &[PathBuf],
        rules: &RuleSpec,
        path: Option<&Path>,
        writer: &settings::Writer,
        resolve: &dyn Fn(&[PathBuf]) -> Vec<PathBuf>,
    ) -> bool {
        let same_roots = self.roots.is_empty() || resolve(&self.roots) == roots;
        if same_roots && self.rules == *rules {
            return false;
        }
        let roots = if same_roots {
            self.roots.clone()
        } else {
            roots.to_vec()
        };
        self.change(roots, rules.clone(), path, writer);
        true
    }
}

/// Whether Scan now and Rebuild index now are offered in `state`: not while
/// the folders are being read, which a click would only start again.
fn can_scan_now(state: IndexState) -> bool {
    state != IndexState::Scanning
}

/// Keeps the settings list tidy without resolving paths on the UI thread.
fn listed_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted = roots.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut result: Vec<PathBuf> = Vec::new();
    for root in sorted {
        if !result.iter().any(|kept| root.starts_with(kept)) {
            result.push(root);
        }
    }
    result
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
    /// Saves the settings and the history off the window's thread.
    writer: settings::Writer,
    /// Why the last change of the rules was refused, shown next to them.
    rule_error: RefCell<String>,
    /// Why the typed rescan time was refused.
    rescan_error: RefCell<String>,
    /// The Advanced page: a replacement of every setting waiting for the
    /// user's answer, and the outcome of the last export or import.
    pending: RefCell<Option<Pending>>,
    settings_note: RefCell<String>,
    /// Looks every [`RELOAD_EVERY`] whether the settings file changed
    /// outside the window; the stamp of the version last seen.
    reload_timer: slint::Timer,
    property_timer: slint::Timer,
    /// The raw settings table: the rows listed for its filter, in order,
    /// the key of the chosen one, and why the last edit was refused.
    raw_listed: RefCell<Vec<settings::RawRow>>,
    raw_chosen: RefCell<Option<String>>,
    raw_error: RefCell<String>,
    raw_lang: Cell<Option<i18n::Lang>>,
    /// Keys changed in the table since its filter last changed: they stay
    /// listed under "Only changed settings" (see [`raw_rows_for`]).
    raw_kept: RefCell<Vec<String>>,
    seen: Arc<std::sync::Mutex<Option<settings::Stamp>>>,
    /// The main window asks whether to apply a settings file changed
    /// outside; the user agreed, so the next read is applied; the lines
    /// of the last outside change turned down, not asked about again.
    reload_asked: Cell<bool>,
    reload_approved: Cell<bool>,
    reload_declined: RefCell<std::collections::BTreeSet<String>>,
    state: Cell<(IndexState, usize)>,
    /// A folder chooser is open.
    choosing: Cell<bool>,
    engine: Arc<Engine>,
    main: slint::Weak<AppWindow>,
    pub history: RefCell<History>,
    pub searches: RefCell<Searches>,
    pub history_epoch: Cell<u64>,
    history_revision: Cell<u64>,
    history_generation: Cell<crate::history_sync::Generation>,
    history_reader: Arc<Mutex<crate::history_sync::Reader>>,
    history_syncing: Cell<bool>,
    retention: Cell<search_history::Retention>,
    window: RefCell<Option<SettingsWindow>>,
    history_window: RefCell<Option<HistoryWindow>>,
    history_listed: RefCell<Vec<String>>,
    history_order: Cell<(Order, bool)>,
    history_busy: Cell<bool>,
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
    /// The open Add or Edit dialog of the Keyboard page.
    key_dialog: Cell<Option<KeyDialog>>,
}

/// A replacement of every setting that waits for the user to agree.
enum Pending {
    /// The settings read from this file.
    Import(
        Box<Settings>,
        PathBuf,
        Option<crate::saved_searches::Library>,
    ),
    ResetAll,
}

/// The Keyboard page's dialog: the command, which of its keys is changed
/// (`None` adds one), where the key works and the key pressed.
#[derive(Clone, Copy, Debug)]
struct KeyDialog {
    cmd: Command,
    replacing: Option<usize>,
    scope: Scope,
    chord: Option<Chord>,
}

thread_local! {
    static PREFS: RefCell<Option<Rc<Prefs>>> = RefCell::default();
}

/// Whether an outside change of the settings, the `lines` that differ,
/// is asked about: not when the user turned down these same differences
/// (a later save in this window keeps them in the file).
/// Whether a settings file changed since it was read is asked about before
/// it applies: only with "Ask first" on, for an edit made outside EveryGrep
/// (`own` false) that the user has not agreed to already.
fn asks_first(ask_reload: bool, own: bool, approved: bool) -> bool {
    ask_reload && !own && !approved
}

fn ask_about(
    lines: &std::collections::BTreeSet<String>,
    declined: &std::collections::BTreeSet<String>,
) -> bool {
    !lines.is_empty() && !lines.is_subset(declined)
}

/// Chooses the settings to apply after a file reload.
fn reload_candidate(
    read: Settings,
    current: &Settings,
    declined: &std::collections::BTreeSet<String>,
    preserve_declined: bool,
) -> Settings {
    if preserve_declined && !declined.is_empty() {
        read.keeping_declined(current, declined)
    } else {
        read
    }
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
        let on_disk = settings.history_on_disk;
        let retention = search_history::Retention::new(settings.history_days);
        let history_max = if settings.history_enabled {
            settings.history_size
        } else {
            0
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
            history: RefCell::new(History::new(history_max)),
            searches: RefCell::default(),
            history_epoch: Cell::new(0),
            history_revision: Cell::new(0),
            history_generation: Cell::new(crate::history_sync::Generation::Unknown),
            history_reader: Arc::default(),
            history_syncing: Cell::new(false),
            retention: Cell::new(retention),
            settings: RefCell::new(settings),
            index: RefCell::new(index),
            read_error,
            error: RefCell::new(error),
            writer: settings::Writer::start(),
            rule_error: RefCell::default(),
            rescan_error: RefCell::default(),
            pending: RefCell::default(),
            settings_note: RefCell::default(),
            reload_timer: slint::Timer::default(),
            property_timer: slint::Timer::default(),
            raw_listed: RefCell::default(),
            raw_chosen: RefCell::default(),
            raw_error: RefCell::default(),
            raw_lang: Cell::new(None),
            raw_kept: RefCell::default(),
            seen: Arc::default(),
            reload_asked: Cell::new(false),
            reload_approved: Cell::new(false),
            reload_declined: RefCell::default(),
            state: Cell::new((IndexState::Loading, 0)),
            choosing: Cell::new(false),
            engine,
            main: main.as_weak(),
            window: RefCell::default(),
            history_window: RefCell::default(),
            history_listed: RefCell::default(),
            history_order: Cell::new((Order::Date, true)),
            history_busy: Cell::new(false),
            desktop_scheme: Cell::new(None),
            scheme_set: Cell::new(false),
            language_set: Cell::new(false),
            listed_drives: RefCell::default(),
            key_filter: RefCell::default(),
            listed_commands: RefCell::default(),
            key_chosen: Cell::new(None),
            key_dialog: Cell::new(None),
        });
        PREFS.with(|p| *p.borrow_mut() = Some(prefs.clone()));
        prefs.sync_history();
        if on_disk {
            prefs.change_searches_with(
                |_| {},
                |p, ok| {
                    if ok {
                        let mut entries = p.searches.borrow().0.clone();
                        entries.sort_by_key(|e| e.last);
                        p.history
                            .borrow_mut()
                            .seed(entries.into_iter().map(|e| e.search).collect());
                    }
                },
            );
        }
        prefs
            .reload_timer
            .start(slint::TimerMode::Repeated, RELOAD_EVERY, || {
                with(|p| {
                    p.look_for_outside_changes();
                    p.sync_history();
                })
            });
        prefs
    }

    /// Asks the saving thread to look at the settings file: after the saves
    /// asked for before, so the window's own changes are on disk. A
    /// version not seen before is read and handed back.
    fn look_for_outside_changes(&self) {
        if !self.settings.borrow().reload_changed || self.read_error.is_some() {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let seen = Arc::clone(&self.seen);
        let approved = self.reload_approved.get();
        self.writer.read_at_revision(
            move || {
                let now = settings::stamp(&path);
                let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
                if *seen == now {
                    return None;
                }
                *seen = now;
                drop(seen);
                settings::load_marked(&path).ok()
            },
            move |read, at| {
                let Some(read) = read else {
                    if approved {
                        // Nothing readable to apply; the next change asks.
                        let _ = slint::invoke_from_event_loop(|| {
                            with(|p| p.reload_approved.set(false));
                        });
                    }
                    return;
                };
                let (read, own, local) = read;
                let _ = slint::invoke_from_event_loop(move || {
                    with(|p| p.changed_outside(read, own, local, at));
                });
            },
        );
    }

    /// The settings file as read by [`look_for_outside_changes`]: when it
    /// differs from the running settings, and the window saved nothing
    /// since it was read, it is applied (not saved again). With "Ask
    /// first" on, the main window asks before; a change turned down is
    /// not asked about again, until the file changes in another way.
    /// `own`: an EveryGrep instance saved this version, so it applies without
    /// a question. A local save preserves this window's declined values;
    /// another instance's accepted settings can replace them.
    ///
    /// [`look_for_outside_changes`]: Self::look_for_outside_changes
    fn changed_outside(&self, read: Settings, own: bool, local: bool, asked: u64) {
        if self.writer.asked() != asked || !self.settings.borrow().reload_changed {
            // Saves came meanwhile; the next look reads the file again.
            *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }
        let approved = self.reload_approved.replace(false);
        let preserve_declined = local && !approved;
        let read = reload_candidate(
            read,
            &self.settings.borrow(),
            &self.reload_declined.borrow(),
            preserve_declined,
        );
        if read.to_text() == self.settings.borrow().to_text() {
            return;
        }
        if asks_first(self.settings.borrow().ask_reload, own, approved) {
            if self.reload_asked.get() {
                // Asked already; agreeing reads the file again.
                return;
            }
            let lines = read.differing_lines(&self.settings.borrow());
            if !ask_about(&lines, &self.reload_declined.borrow()) {
                return;
            }
            let Some(m) = self.main.upgrade() else { return };
            if m.get_confirm_open() {
                // Another question is open: ask at a later look.
                *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return;
            }
            *self.reload_declined.borrow_mut() = lines;
            self.reload_asked.set(true);
            crate::ask_reload_settings(&m);
            return;
        }
        if !preserve_declined {
            self.reload_declined.borrow_mut().clear();
        }
        let bad = read.problems.len();
        self.replace_all(read, false);
        if let Some(m) = self.main.upgrade() {
            crate::info(&m, labels::settings_reloaded(bad));
        }
    }

    /// The answer to the main window's question about a settings file
    /// changed outside: yes reads it again and applies it, no keeps the
    /// running settings. Nothing happens when nothing was asked.
    pub fn reload_answered(&self, yes: bool) {
        if !self.reload_asked.replace(false) {
            return;
        }
        if yes {
            self.reload_approved.set(true);
            *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.look_for_outside_changes();
        }
    }

    /// Changes the settings and saves them on the saving thread; a failure
    /// is shown when it comes.
    fn change(&self, f: impl Fn(&mut Settings) + Send + 'static) {
        f(&mut self.settings.borrow_mut());
        if self.read_error.is_some() {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        self.writer.run(move || {
            let failed = settings::update(&path, &f).err();
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| p.set_error(failed.map(|e| labels::cannot_save(&e))));
            });
        });
    }

    /// Runs `job` on the saving thread, after the settings and history
    /// saves asked before it (the session file of the tabs uses it). It
    /// must not write the settings file.
    pub fn save_in_order(&self, job: impl FnOnce() + Send + 'static) {
        self.writer.run_apart(job);
    }

    /// Waits at most `wait` for the settings and the history to be saved,
    /// before the program ends.
    pub fn finish_saving(&self, wait: std::time::Duration) -> bool {
        self.writer.wait_done(wait)
    }

    /// Search-page saves go to the same saving thread as the history, so
    /// their order with the history writes is kept and changing these
    /// controls never blocks the window on a lock, a write or an fsync.
    fn change_search_settings(&self, change: impl Fn(&mut Settings) + Send + 'static) {
        self.change(change);
        self.refresh();
    }

    fn set_error(&self, error: Option<String>) {
        let text = error
            .or_else(|| self.read_error.clone())
            .unwrap_or_default();
        *self.error.borrow_mut() = text;
        self.refresh();
    }

    /// Records once per navigation visit, so the pause, Enter and opening a
    /// result cannot count the same visit three times.
    pub fn record(&self, query: &str) {
        let s = self.settings.borrow();
        if !s.history_enabled || s.history_size == 0 || query.trim().is_empty() {
            return;
        }
        drop(s);
        if self.history.borrow_mut().record(query) {
            let query = query.to_owned();
            let time = search_history::now();
            self.change_searches(move |entries| entries.record(&query, time));
        }
    }

    /// Every disk operation applies a delta to the latest locked file on
    /// the helper thread. Jobs and completion notifications keep their order.
    pub fn change_searches(&self, change: impl Fn(&mut Searches) + Send + 'static) {
        self.change_searches_with(change, |_, _| {});
    }

    fn change_searches_with(
        &self,
        change: impl Fn(&mut Searches) + Send + 'static,
        done: impl FnOnce(&Prefs, bool) + Send + 'static,
    ) {
        let s = self.settings.borrow();
        let days = s.history_days;
        let on_disk = s.history_on_disk && self.read_error.is_none();
        drop(s);
        let time = search_history::now();
        let revision = self.history_revision.get().wrapping_add(1);
        self.history_revision.set(revision);
        self.searches.borrow_mut().apply(days, time, &change);
        self.searches
            .borrow_mut()
            .limit_to(self.settings.borrow().history_entries);
        if on_disk {
            let path = settings::history_path();
            let epoch = self.history_epoch.get();
            let settings_path = self.path.clone();
            let reader = self.history_reader.clone();
            let generation = self.history_generation.get();
            self.writer.run_apart(move || {
                let result = path
                    .ok_or_else(|| {
                        std::io::Error::other(i18n::tr("The configuration folder is unavailable."))
                    })
                    .and_then(|path| {
                        let settings_path = settings_path
                            .ok_or_else(|| std::io::Error::other("settings path unavailable"))?;
                        let generation = reader
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .generation_for(epoch, generation);
                        crate::history_sync::update(&settings_path, &path, generation, time, change)
                    });
                let _ = slint::invoke_from_event_loop(move || {
                    with(|p| {
                        if p.history_epoch.get() == epoch {
                            let ok = match result {
                                Ok(Some(entries)) => p.finish_history(revision, Ok(entries)),
                                Ok(None) => true,
                                Err(e) => p.finish_history(revision, Err(e)),
                            };
                            done(p, ok);
                        }
                    })
                });
            });
        } else {
            done(self, true);
        }
        self.fill_history();
        self.fill_history_window();
    }

    pub fn fill_history(&self) {
        self.sync_history();
        if self.history_syncing.get() {
            if let Some(w) = self.main.upgrade() {
                w.set_history_rows(ModelRc::default());
            }
        } else {
            self.fill_history_cached();
        }
    }

    fn sync_history(&self) {
        if self.read_error.is_some() || self.history_syncing.replace(true) {
            return;
        }
        let (Some(path), Some(settings_path)) = (settings::history_path(), self.path.clone())
        else {
            self.history_syncing.set(false);
            return;
        };
        let reader = self.history_reader.clone();
        let revision = self.history_revision.get();
        self.writer.read_at_revision(
            move || {
                reader
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .read(&settings_path, &path)
            },
            move |result, asked| {
                let _ = slint::invoke_from_event_loop(move || {
                    with(|p| {
                        p.history_syncing.set(false);
                        if p.writer.asked() != asked || p.history_revision.get() != revision {
                            p.history_reader
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .invalidate();
                            return;
                        }
                        match result {
                            Ok(read) => {
                                p.history_reader
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .generation = read.generation;
                                p.history_generation
                                    .set(crate::history_sync::Generation::Known(read.generation));
                                let max = if read.policy.history_enabled {
                                    read.policy.history_size
                                } else {
                                    0
                                };
                                {
                                    let mut s = p.settings.borrow_mut();
                                    s.history_enabled = read.policy.history_enabled;
                                    s.history_on_disk = read.policy.history_on_disk;
                                    s.history_size = read.policy.history_size;
                                    s.history_days = read.policy.history_days;
                                    s.history_entries = read.policy.history_entries;
                                    s.history_sort = read.policy.history_sort;
                                    s.always_suggestions = read.policy.always_suggestions;
                                }
                                if read.cleared {
                                    p.history_epoch.set(p.history_epoch.get().wrapping_add(1));
                                    p.searches.borrow_mut().0.clear();
                                    p.history.borrow_mut().set_max(0);
                                }
                                if let Some(entries) = read.entries {
                                    *p.searches.borrow_mut() = entries;
                                }
                                p.history.borrow_mut().set_max(max);
                                crate::tabs_ui::history_policy(max, read.cleared);
                                crate::tabs_ui::session_policy();
                                if p.retention.get().committed != read.policy.history_days {
                                    p.retention.set(search_history::Retention::new(
                                        read.policy.history_days,
                                    ));
                                }
                                p.searches
                                    .borrow_mut()
                                    .limit_to(read.policy.history_entries);
                                p.fill_history_cached();
                                p.fill_history_window();
                                p.refresh();
                            }
                            Err(e) => p.history_failure(&e),
                        }
                    })
                });
            },
        );
    }

    fn fill_history_cached(&self) {
        let Some(w) = self.main.upgrade() else { return };
        let s = self.settings.borrow();
        let mut entries = self.searches.borrow_mut();
        entries.retain_days(s.history_days, search_history::now());
        let rows = if w.get_history_full() {
            entries.sorted(s.history_sort, "")
        } else {
            entries.suggestions(&w.get_query(), s.history_sort)
        };
        let rows: Vec<SharedString> = rows.into_iter().map(|e| e.search.as_str().into()).collect();
        w.set_history_rows(ModelRc::new(VecModel::from(rows)));
        w.set_history_sort(
            Order::ALL
                .iter()
                .position(|o| *o == s.history_sort)
                .unwrap_or(0) as i32,
        );
        w.set_history_suggestions(
            s.history_enabled && s.history_size > 0 && (s.always_suggestions || !s.as_you_type),
        );
    }

    pub fn set_history_sort(&self, index: i32) {
        if let Some(order) = usize::try_from(index)
            .ok()
            .and_then(|i| Order::ALL.get(i))
            .copied()
        {
            self.change_search_settings(move |s| s.history_sort = order);
            self.fill_history();
        }
    }

    fn history_failure(&self, e: &std::io::Error) {
        let text = i18n::fill(i18n::tr("Could not update search history: {}"), &[e]);
        self.history_message(&text, true);
        if let Some(w) = self.main.upgrade() {
            crate::error(&w, text.clone());
        }
        self.set_error(Some(text));
    }

    fn finish_history(&self, revision: u64, result: std::io::Result<Searches>) -> bool {
        match result {
            Ok(entries) => {
                if self.history_revision.get() == revision {
                    *self.searches.borrow_mut() = entries;
                    self.fill_history();
                    self.fill_history_window();
                }
                true
            }
            Err(e) => {
                self.history_failure(&e);
                false
            }
        }
    }

    fn clear_saved_history(&self, notify: bool) {
        if self.read_error.is_some() {
            return;
        }
        let path = settings::history_path();
        let epoch = self.history_epoch.get();
        let reader = self.history_reader.clone();
        let generation = self.history_generation.get();
        self.writer.run_apart(move || {
            let result = path.map_or(Ok(()), |p| {
                if notify {
                    let after = search_history::clear_generation(&p)?;
                    reader
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remember_clear(epoch, generation, after);
                    Ok(())
                } else {
                    search_history::clear_data(&p)
                }
            });
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    if p.history_epoch.get() == epoch {
                        if let Err(e) = result {
                            p.history_failure(&e);
                        }
                    }
                })
            });
        });
    }

    pub fn clear_history(&self) {
        self.history_revision
            .set(self.history_revision.get().wrapping_add(1));
        self.history_epoch
            .set(self.history_epoch.get().wrapping_add(1));
        let s = self.settings.borrow();
        let max = if s.history_enabled { s.history_size } else { 0 };
        drop(s);
        self.history.borrow_mut().set_max(0);
        self.history.borrow_mut().set_max(max);
        crate::tabs_ui::history_policy(max, true);
        self.searches.borrow_mut().0.clear();
        self.clear_saved_history(true);
        self.fill_history();
        self.fill_history_window();
        self.history_message("", false);
        self.set_error(None);
    }

    pub fn open_history(self: &Rc<Self>) {
        if self.history_window.borrow().is_none() {
            let w = match HistoryWindow::new() {
                Ok(w) => w,
                Err(e) => {
                    self.set_error(Some(e.to_string()));
                    return;
                }
            };
            w.on_filter_changed(|| with(|p| p.fill_history_window()));
            w.on_sort_changed(|column, descending| {
                with(|p| {
                    let order = match column {
                        0 => Order::Text,
                        1 => Order::Count,
                        _ => Order::Date,
                    };
                    p.history_order.set((order, descending));
                    p.fill_history_window();
                })
            });
            w.on_delete_entry(|index| with(|p| p.delete_history_entry(index)));
            w.on_clear_history(|| with(|p| p.clear_history()));
            w.on_transfer(|import| with(|p| p.transfer_history(import)));
            w.on_close_window({
                let weak = w.as_weak();
                move || {
                    if let Some(w) = weak.upgrade() {
                        let _ = w.hide();
                    }
                }
            });
            *self.history_window.borrow_mut() = Some(w);
        }
        self.change_searches(|_| {});
        let scheme = look::resolve(self.settings.borrow().theme, self.desktop_scheme.get());
        if let Some(w) = self.history_window.borrow().as_ref() {
            w.invoke_set_color_scheme(scheme as i32);
            // An open window may be behind the main window; showing it again
            // gives a new surface, which GNOME brings to the front (see `raise`
            // in main.rs).
            let _ = w.hide();
            let _ = w.show();
        }
    }

    fn fill_history_window(&self) {
        let window = self.history_window.borrow();
        let Some(w) = window.as_ref() else { return };
        let entries = self.searches.borrow();
        let (order, descending) = self.history_order.get();
        let mut rows = entries.sorted(order, &w.get_filter_text());
        if descending == (order == Order::Text) {
            rows.reverse();
        }
        let zone = everygrep_core::localtime::Zone::local();
        let mut listed = Vec::with_capacity(rows.len());
        let rows: Vec<ModelRc<StandardListViewItem>> = rows
            .into_iter()
            .map(|e| {
                listed.push(e.search.clone());
                let time = e.last.min(search_history::MAX_TIMESTAMP) as i64;
                let (y, m, d) = zone.local_date(time);
                let secs = zone.to_local(time).rem_euclid(86_400);
                let date = format!(
                    "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
                    secs / 3600,
                    secs % 3600 / 60,
                    secs % 60
                );
                let cells = vec![e.search.clone(), e.count.to_string(), date];
                ModelRc::new(VecModel::from(
                    cells
                        .into_iter()
                        .map(|text| StandardListViewItem::from(SharedString::from(text)))
                        .collect::<Vec<_>>(),
                ))
            })
            .collect();
        w.set_rows(ModelRc::new(VecModel::from(rows)));
        w.set_current(-1);
        *self.history_listed.borrow_mut() = listed;
    }

    fn history_message(&self, text: &str, error: bool) {
        if let Some(w) = self.history_window.borrow().as_ref() {
            w.set_notice(text.into());
            w.set_notice_error(error);
        }
    }

    fn delete_history_entry(&self, index: i32) {
        let query = usize::try_from(index)
            .ok()
            .and_then(|i| self.history_listed.borrow().get(i).cloned());
        let Some(query) = query else { return };
        self.history_epoch
            .set(self.history_epoch.get().wrapping_add(1));
        self.history.borrow_mut().remove(&query);
        crate::tabs_ui::remove_history(&query);
        let removed = query.clone();
        self.change_searches_with(
            move |entries| entries.0.retain(|e| e.search != removed),
            move |p, ok| {
                if ok {
                    p.history_message("", false);
                }
            },
        );
    }

    fn transfer_history(&self, import: bool) {
        if self.history_busy.replace(true) {
            return;
        }
        self.history_message("", false);
        if let Some(w) = self.history_window.borrow().as_ref() {
            w.set_busy(true);
        }
        let mut snapshot = self.searches.borrow().clone();
        snapshot.retain_days(self.settings.borrow().history_days, search_history::now());
        let epoch = self.history_epoch.get();
        let title = if import {
            i18n::tr("Import search history")
        } else {
            i18n::tr("Export search history")
        };
        let accept = if import {
            i18n::tr("Import")
        } else {
            i18n::tr("Export")
        };
        let filter_name = i18n::tr("CSV files");
        std::thread::spawn(move || {
            enum Outcome {
                Cancelled,
                Imported(Searches),
                Exported,
            }
            let filters = [crate::portal::SaveFilter {
                name: filter_name,
                pattern: "*.csv",
            }];
            let result: Result<Outcome, String> = (|| {
                if import {
                    let Some(path) = crate::portal::open_file(title, accept, &filters)? else {
                        return Ok(Outcome::Cancelled);
                    };
                    history_csv::import(&path)
                        .map(Outcome::Imported)
                        .map_err(|e| e.to_string())
                } else {
                    let Some((path, _)) =
                        crate::portal::save_file(title, accept, "search-history.csv", &filters)?
                    else {
                        return Ok(Outcome::Cancelled);
                    };
                    history_csv::export(&path, &snapshot)
                        .map(|_| Outcome::Exported)
                        .map_err(|e| e.to_string())
                }
            })();
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    p.history_busy.set(false);
                    if let Some(w) = p.history_window.borrow().as_ref() {
                        w.set_busy(false);
                    }
                    match result {
                        Ok(Outcome::Cancelled) => {}
                        Ok(Outcome::Imported(entries)) => {
                            if p.history_epoch.get() != epoch {
                                return;
                            }
                            p.change_searches_with(
                                move |s| s.merge(&entries),
                                |p, ok| {
                                    if ok {
                                        p.history_message(
                                            i18n::tr("Search history imported."),
                                            false,
                                        );
                                    }
                                },
                            );
                        }
                        Ok(Outcome::Exported) => {
                            p.history_message(i18n::tr("Search history exported."), false)
                        }
                        Err(e) => p.history_message(
                            &i18n::fill(i18n::tr("History file operation failed: {}"), &[&e]),
                            true,
                        ),
                    }
                })
            });
        });
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
        crate::saved_ui::apply_scheme(scheme as i32);
        crate::advanced::apply_scheme(scheme as i32);
        crate::fileops_undo_ui::apply_scheme(scheme as i32);
        crate::debug_log_ui::apply_scheme(scheme as i32);
        if let Some(m) = self.main.upgrade() {
            m.invoke_set_color_scheme(scheme as i32);
        }
        if let Some(w) = self.history_window.borrow().as_ref() {
            w.invoke_set_color_scheme(scheme as i32);
        }
        if let Ok(window) = self.window.try_borrow() {
            if let Some(w) = window.as_ref() {
                w.invoke_set_color_scheme(scheme as i32);
            }
        }
    }

    /// Shows or hides the main window's menu bar and status line.
    pub fn apply_menu_bar(&self) {
        if let Some(m) = self.main.upgrade() {
            let s = self.settings.borrow();
            m.set_show_menu_bar(s.menu_bar);
            m.set_show_status_bar(s.status_bar);
        }
    }

    /// Changes whether the main window has a status line.
    pub fn set_status_bar(&self, on: bool) {
        self.change(move |s| s.status_bar = on);
        self.apply_menu_bar();
        self.refresh();
    }

    /// Changes whether the main window has a menu bar.
    pub fn set_menu_bar(&self, on: bool) {
        self.change(move |s| s.menu_bar = on);
        self.apply_menu_bar();
        self.refresh();
    }

    pub fn set_preview(&self, visible: bool, width: f32) {
        self.change(move |s| {
            s.preview_visible = visible;
            s.preview_width = width.clamp(180.0, 700.0);
        });
        self.refresh();
    }

    pub fn preview_options(&self) -> (crate::preview::Options, everygrep_core::content::CodePage) {
        let s = self.settings.borrow();
        (s.preview, s.content.code_page)
    }

    fn preview_changed(&self) {
        let Some(w) = self.window.borrow().as_ref().map(|w| w.as_weak()) else {
            return;
        };
        let Some(w) = w.upgrade() else {
            return;
        };
        let options = crate::preview::Options::edited(&w);
        let visible = w.get_preview_visible();
        self.change(move |s| {
            s.preview = options;
            s.preview_visible = visible;
        });
        self.fill_menus();
        self.refresh();
    }

    pub fn set_folders(&self, visible: bool, width: f32, recursive: bool, dim_empty: bool) {
        self.change(move |s| {
            s.folders_visible = visible;
            s.folders_width = width.clamp(140.0, 600.0);
            s.folders_recursive = recursive;
            s.folders_dim_empty = dim_empty;
        });
    }

    /// Changes what folders show in the Size column.
    pub fn set_folder_size(&self, folder_size: FolderSize) {
        self.change(move |s| s.folder_size = folder_size);
        self.apply_folder_size();
        self.refresh();
    }

    /// Puts the check marks of the main window's View menu on the chosen
    /// theme, density and folder size.
    pub fn fill_menus(&self) {
        crate::fileops_undo::configure(&self.settings.borrow().fileops);
        crate::fileops_undo_ui::refresh_open();
        let Some(m) = self.main.upgrade() else { return };
        let s = self.settings.borrow();
        m.set_theme_items(choices(&ThemeChoice::ALL, s.theme, labels::theme));
        m.set_density_items(choices(&Density::ALL, s.density, labels::density));
        m.set_folder_size_items(choices(
            &FolderSize::ALL,
            s.folder_size,
            labels::folder_size,
        ));
        m.set_file_undo_enabled(s.fileops.undo_enabled);
        m.set_drag_to_folders(s.fileops.drag_to_folders);
        m.set_status_details(s.status_details);
        m.set_show_menu_bar(s.menu_bar);
        m.set_show_status_bar(s.status_bar);
        m.set_preview_visible(s.preview_visible);
        m.set_preview_width(s.preview_width);
        m.set_folders_visible(s.folders_visible);
        m.set_folders_width(s.folders_width);
        m.set_folders_recursive(s.folders_recursive);
        m.set_folders_dim_empty(s.folders_dim_empty);
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
        w.set_show_md5(w.get_show_md5() || c.show_md5);
        w.set_show_sha256(w.get_show_sha256() || c.show_sha256);
        w.set_show_crc32(w.get_show_crc32() || c.show_crc32);
        w.set_show_name_freq(w.get_show_name_freq() || c.show_name_freq);
        w.set_show_ext_freq(w.get_show_ext_freq() || c.show_ext_freq);
        w.set_show_size_freq(w.get_show_size_freq() || c.show_size_freq);
        w.set_show_full_path(c.show_full_path);
        c.full_path
            .into_iter()
            .for_each(|v| w.set_full_path_want(v));
        w.set_show_parent_path(c.show_parent_path);
        c.parent_path
            .into_iter()
            .for_each(|v| w.set_parent_path_want(v));
        w.set_show_stem(c.show_stem);
        c.stem.into_iter().for_each(|v| w.set_stem_want(v));
        w.set_show_name_length(c.show_name_length);
        c.name_length
            .into_iter()
            .for_each(|v| w.set_name_length_want(v));
        w.set_show_path_length(c.show_path_length);
        c.path_length
            .into_iter()
            .for_each(|v| w.set_path_length_want(v));
        w.set_show_disk_size(c.show_disk_size);
        c.disk_size
            .into_iter()
            .for_each(|v| w.set_disk_size_want(v));
        w.set_show_total_size(c.show_total_size);
        c.total_size
            .into_iter()
            .for_each(|v| w.set_total_size_want(v));
        w.set_show_child_count(c.show_child_count);
        c.child_count
            .into_iter()
            .for_each(|v| w.set_child_count_want(v));
        w.set_show_child_files(c.show_child_files);
        c.child_files
            .into_iter()
            .for_each(|v| w.set_child_files_want(v));
        w.set_show_child_folders(c.show_child_folders);
        c.child_folders
            .into_iter()
            .for_each(|v| w.set_child_folders_want(v));
        w.set_show_desc_count(c.show_desc_count);
        c.desc_count
            .into_iter()
            .for_each(|v| w.set_desc_count_want(v));
        w.set_show_desc_files(c.show_desc_files);
        c.desc_files
            .into_iter()
            .for_each(|v| w.set_desc_files_want(v));
        w.set_show_desc_folders(c.show_desc_folders);
        c.desc_folders
            .into_iter()
            .for_each(|v| w.set_desc_folders_want(v));
        w.set_show_folder_depth(c.show_folder_depth);
        c.folder_depth
            .into_iter()
            .for_each(|v| w.set_folder_depth_want(v));
        w.set_show_hard_links(c.show_hard_links);
        c.hard_links
            .into_iter()
            .for_each(|v| w.set_hard_links_want(v));
        w.set_show_shortcut_target(c.show_shortcut_target);
        c.shortcut_target
            .into_iter()
            .for_each(|v| w.set_shortcut_target_want(v));
        w.set_show_owner(c.show_owner);
        c.owner.into_iter().for_each(|v| w.set_owner_want(v));
        w.set_show_width(c.show_width);
        c.width.into_iter().for_each(|v| w.set_width_want(v));
        w.set_show_height(c.show_height);
        c.height.into_iter().for_each(|v| w.set_height_want(v));
        w.set_show_dimensions(c.show_dimensions);
        c.dimensions
            .into_iter()
            .for_each(|v| w.set_dimensions_want(v));
        w.set_show_length(c.show_length);
        c.length.into_iter().for_each(|v| w.set_length_want(v));
        w.set_show_bit_rate(c.show_bit_rate);
        c.bit_rate.into_iter().for_each(|v| w.set_bit_rate_want(v));
        w.set_show_title(c.show_title);
        c.title.into_iter().for_each(|v| w.set_title_want(v));
        w.set_show_artist(c.show_artist);
        c.artist.into_iter().for_each(|v| w.set_artist_want(v));
        w.set_show_album(c.show_album);
        c.album.into_iter().for_each(|v| w.set_album_want(v));
        w.set_show_year(c.show_year);
        c.year.into_iter().for_each(|v| w.set_year_want(v));
        w.set_show_track(c.show_track);
        c.track.into_iter().for_each(|v| w.set_track_want(v));
        w.set_show_genre(c.show_genre);
        c.genre.into_iter().for_each(|v| w.set_genre_want(v));
        w.set_show_rating(c.show_rating);
        c.rating.into_iter().for_each(|v| w.set_rating_want(v));
        w.set_show_tags(c.show_tags);
        c.tags.into_iter().for_each(|v| w.set_tags_want(v));
        w.set_show_page_count(c.show_page_count);
        c.page_count
            .into_iter()
            .for_each(|v| w.set_page_count_want(v));
        w.set_show_word_count(c.show_word_count);
        c.word_count
            .into_iter()
            .for_each(|v| w.set_word_count_want(v));
        w.set_show_camera_maker(c.show_camera_maker);
        c.camera_maker
            .into_iter()
            .for_each(|v| w.set_camera_maker_want(v));
        w.set_show_camera_model(c.show_camera_model);
        c.camera_model
            .into_iter()
            .for_each(|v| w.set_camera_model_want(v));
        w.set_show_date_taken(c.show_date_taken);
        c.date_taken
            .into_iter()
            .for_each(|v| w.set_date_taken_want(v));
        w.set_show_gps_lat(c.show_gps_lat);
        c.gps_lat.into_iter().for_each(|v| w.set_gps_lat_want(v));
        w.set_show_gps_long(c.show_gps_long);
        w.set_show_sha1(c.show_sha1);
        w.set_show_sha384(c.show_sha384);
        w.set_show_sha512(c.show_sha512);
        w.set_show_xxhash(c.show_xxhash);
        w.set_show_dupe_group(c.show_dupe_group);
        c.gps_long.into_iter().for_each(|v| w.set_gps_long_want(v));
        c.sha1.into_iter().for_each(|v| w.set_sha1_want(v));
        c.sha384.into_iter().for_each(|v| w.set_sha384_want(v));
        c.sha512.into_iter().for_each(|v| w.set_sha512_want(v));
        c.xxhash.into_iter().for_each(|v| w.set_xxhash_want(v));
        c.dupe_group
            .into_iter()
            .for_each(|v| w.set_dupe_group_want(v));
        if let Some(n) = c.name {
            w.set_name_want(n);
            w.set_name_set(true);
        }
        c.folder.into_iter().for_each(|v| w.set_folder_want(v));
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
            show_md5: w.get_show_md5(),
            show_sha256: w.get_show_sha256(),
            show_crc32: w.get_show_crc32(),
            show_name_freq: w.get_show_name_freq(),
            show_ext_freq: w.get_show_ext_freq(),
            show_size_freq: w.get_show_size_freq(),
            show_full_path: w.get_show_full_path(),
            full_path: Some(w.get_full_path_want()),
            show_parent_path: w.get_show_parent_path(),
            parent_path: Some(w.get_parent_path_want()),
            show_stem: w.get_show_stem(),
            stem: Some(w.get_stem_want()),
            show_name_length: w.get_show_name_length(),
            name_length: Some(w.get_name_length_want()),
            show_path_length: w.get_show_path_length(),
            path_length: Some(w.get_path_length_want()),
            show_disk_size: w.get_show_disk_size(),
            disk_size: Some(w.get_disk_size_want()),
            show_total_size: w.get_show_total_size(),
            total_size: Some(w.get_total_size_want()),
            show_child_count: w.get_show_child_count(),
            child_count: Some(w.get_child_count_want()),
            show_child_files: w.get_show_child_files(),
            child_files: Some(w.get_child_files_want()),
            show_child_folders: w.get_show_child_folders(),
            child_folders: Some(w.get_child_folders_want()),
            show_desc_count: w.get_show_desc_count(),
            desc_count: Some(w.get_desc_count_want()),
            show_desc_files: w.get_show_desc_files(),
            desc_files: Some(w.get_desc_files_want()),
            show_desc_folders: w.get_show_desc_folders(),
            desc_folders: Some(w.get_desc_folders_want()),
            show_folder_depth: w.get_show_folder_depth(),
            folder_depth: Some(w.get_folder_depth_want()),
            show_hard_links: w.get_show_hard_links(),
            hard_links: Some(w.get_hard_links_want()),
            show_shortcut_target: w.get_show_shortcut_target(),
            shortcut_target: Some(w.get_shortcut_target_want()),
            show_owner: w.get_show_owner(),
            owner: Some(w.get_owner_want()),
            show_width: w.get_show_width(),
            width: Some(w.get_width_want()),
            show_height: w.get_show_height(),
            height: Some(w.get_height_want()),
            show_dimensions: w.get_show_dimensions(),
            dimensions: Some(w.get_dimensions_want()),
            show_length: w.get_show_length(),
            length: Some(w.get_length_want()),
            show_bit_rate: w.get_show_bit_rate(),
            bit_rate: Some(w.get_bit_rate_want()),
            show_title: w.get_show_title(),
            title: Some(w.get_title_want()),
            show_artist: w.get_show_artist(),
            artist: Some(w.get_artist_want()),
            show_album: w.get_show_album(),
            album: Some(w.get_album_want()),
            show_year: w.get_show_year(),
            year: Some(w.get_year_want()),
            show_track: w.get_show_track(),
            track: Some(w.get_track_want()),
            show_genre: w.get_show_genre(),
            genre: Some(w.get_genre_want()),
            show_rating: w.get_show_rating(),
            rating: Some(w.get_rating_want()),
            show_tags: w.get_show_tags(),
            tags: Some(w.get_tags_want()),
            show_page_count: w.get_show_page_count(),
            page_count: Some(w.get_page_count_want()),
            show_word_count: w.get_show_word_count(),
            word_count: Some(w.get_word_count_want()),
            show_camera_maker: w.get_show_camera_maker(),
            camera_maker: Some(w.get_camera_maker_want()),
            show_camera_model: w.get_show_camera_model(),
            camera_model: Some(w.get_camera_model_want()),
            show_date_taken: w.get_show_date_taken(),
            date_taken: Some(w.get_date_taken_want()),
            show_gps_lat: w.get_show_gps_lat(),
            gps_lat: Some(w.get_gps_lat_want()),
            show_gps_long: w.get_show_gps_long(),
            show_sha1: w.get_show_sha1(),
            show_sha384: w.get_show_sha384(),
            show_sha512: w.get_show_sha512(),
            show_xxhash: w.get_show_xxhash(),
            show_dupe_group: w.get_show_dupe_group(),
            gps_long: Some(w.get_gps_long_want()),
            sha1: Some(w.get_sha1_want()),
            sha384: Some(w.get_sha384_want()),
            sha512: Some(w.get_sha512_want()),
            xxhash: Some(w.get_xxhash_want()),
            dupe_group: Some(w.get_dupe_group_want()),
            name: w.get_name_set().then(|| w.get_name_want().round()),
            folder: (w.get_folder_want() >= 0.5).then(|| w.get_folder_want().round()),
            size: differs(w.get_size_want(), SIZE_WIDTH),
            date: differs(w.get_date_want(), DATE_WIDTH),
            type_: differs(w.get_type_want(), TYPE_WIDTH),
            ext: differs(w.get_ext_want(), EXT_WIDTH),
            created: differs(w.get_created_want(), CREATED_WIDTH),
            accessed: differs(w.get_accessed_want(), ACCESSED_WIDTH),
            attrs: differs(w.get_attr_want(), ATTRS_WIDTH),
            md5: differs(w.get_md5_want(), MD5_WIDTH),
            sha256: differs(w.get_sha256_want(), SHA256_WIDTH),
            crc32: differs(w.get_crc32_want(), CRC32_WIDTH),
            name_freq: differs(w.get_name_freq_want(), NAME_FREQ_WIDTH),
            ext_freq: differs(w.get_ext_freq_want(), EXT_FREQ_WIDTH),
            size_freq: differs(w.get_size_freq_want(), SIZE_FREQ_WIDTH),
        };
        if self.settings.borrow().columns != columns {
            self.change(move |s| s.columns = columns.clone());
        }
    }

    /// The folders to show on the Indexing page: the chosen ones, or the
    /// ones the index covers when none were chosen.
    fn shown_roots(&self) -> Vec<PathBuf> {
        let roots = self.index.borrow().roots.clone();
        if roots.is_empty() {
            self.engine.roots()
        } else {
            listed_roots(&roots)
        }
    }

    /// Changes the folders to index and the rules, reusing unchanged coverage.
    /// Rules that do not compile change nothing: the reason is shown on the
    /// Indexing page and false returned.
    fn set_index(&self, roots: Vec<PathBuf>, rules: RuleSpec) -> bool {
        if let Err(e) = IndexRules::new(rules.clone()) {
            *self.rule_error.borrow_mut() = labels::bad_rule(&e);
            self.refresh();
            return false;
        }
        self.rule_error.borrow_mut().clear();
        let path = self.index_path();
        self.index
            .borrow_mut()
            .change(roots, rules, path.as_deref(), &self.writer);
        self.index
            .borrow()
            .apply_to(&mut self.settings.borrow_mut());
        self.set_error(None);
        self.update_index();
        true
    }

    /// Where folder and rule changes are saved: nowhere when the settings
    /// file could not be read and these are the saved settings.
    fn index_path(&self) -> Option<PathBuf> {
        if self.read_error.is_some() && self.index.borrow().saved {
            None
        } else {
            self.path.clone()
        }
    }

    /// A new index was cancelled ([`crate::engine::Stopped::Rescan`]): the
    /// engine went back to the folders and rules of the index it kept, and
    /// the Indexing page and the settings file follow, so the next start
    /// does not index the cancelled ones.
    pub fn rescan_stopped(&self) {
        let details = self.engine.file_details();
        if details != self.settings.borrow().index_file_details {
            self.keep_file_details(details);
            self.refresh();
        }
        let (roots, rules) = self.engine.setup();
        let path = self.index_path();
        if !self
            .index
            .borrow_mut()
            .follow(&roots, &rules, path.as_deref(), &self.writer, &|r| {
                self.engine.resolved_roots(r)
            })
        {
            return;
        }
        self.index
            .borrow()
            .apply_to(&mut self.settings.borrow_mut());
        self.set_error(None);
        if let Ok(window) = self.window.try_borrow() {
            if let Some(w) = window.as_ref() {
                self.fill_rule_texts(w);
            }
        }
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
        self.set_watch_changes(on);
    }

    /// Turns watching for file changes on or off: the Indexing page's
    /// switch, and Index → Pause index updates the other way round.
    /// Resuming catches up with what changed meanwhile.
    pub fn set_watch_changes(&self, on: bool) {
        self.change(move |s| s.watch_changes = on);
        self.engine.set_watching(on);
        self.apply_watch_changes();
        self.refresh();
    }

    /// Reads file sizes and times into the index, or only names, and
    /// indexes again. Saved only for the window over the default index, as
    /// the folders are.
    fn set_file_details(&self, on: bool) {
        if self.settings.borrow().index_file_details == on {
            return;
        }
        self.keep_file_details(on);
        self.engine.set_file_details(on);
        self.rebuild();
        self.refresh();
    }

    /// Takes `on` as the file details setting, saving it when the folders
    /// are saved ones.
    fn keep_file_details(&self, on: bool) {
        if self.index.borrow().saved && self.index_path().is_some() {
            self.change(move |s| s.index_file_details = on);
        } else {
            self.settings.borrow_mut().index_file_details = on;
        }
    }

    /// The Indexing page's rescan choice changed: a kind, its hours or its
    /// time. A time that cannot be read changes nothing and is said.
    fn rescan_changed(&self) {
        let Some(w) = self.window.borrow().as_ref().map(|w| w.as_weak()) else {
            return;
        };
        let Some(w) = w.upgrade() else { return };
        let kind = usize::try_from(w.get_rescan_kind())
            .unwrap_or(0)
            .min(Schedule::KINDS.len() - 1);
        let hours = u32::try_from(w.get_rescan_hours())
            .unwrap_or(1)
            .clamp(1, schedule::MAX_HOURS);
        let time = match schedule::parse_time(&w.get_rescan_time()) {
            Some(time) => {
                self.rescan_error.borrow_mut().clear();
                w.set_rescan_time(schedule::time_text(time).into());
                time
            }
            None => {
                *self.rescan_error.borrow_mut() = labels::bad_rescan_time().to_owned();
                self.settings.borrow().rescan_time
            }
        };
        self.change(move |s| {
            s.rescan = kind;
            s.rescan_hours = hours;
            s.rescan_time = time;
        });
        self.apply_schedule();
        self.refresh();
    }

    /// Advanced → Export settings…: asks where on a helper thread and writes
    /// the settings there as the settings file has them.
    fn export_settings(&self) {
        if self.choosing.replace(true) {
            return;
        }
        let settings = self.settings.borrow().clone();
        std::thread::spawn(move || {
            let text = settings::config_dir()
                .ok_or_else(|| std::io::Error::other("settings path unavailable"))
                .and_then(|dir| {
                    crate::settings_bundle::export(&settings, &dir.join("saved-searches"))
                });
            let filters = [crate::portal::SaveFilter {
                name: labels::settings_files(),
                pattern: "*.txt",
            }];
            let chosen = crate::portal::save_file(
                labels::export_settings_title(),
                labels::export_accept(),
                "everygrep-settings.txt",
                &filters,
            );
            let outcome = match chosen {
                Ok(Some((path, _))) => Some(
                    text.and_then(|text| settings::write_atomic(&path, text.as_bytes()))
                        .map(|()| path)
                        .map_err(|e| labels::cannot_save(&e)),
                ),
                Ok(None) => None,
                Err(e) => Some(Err(labels::chooser_failed(&e))),
            };
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    p.choosing.set(false);
                    match outcome {
                        Some(Ok(path)) => p.note_settings(labels::settings_exported(&path)),
                        Some(Err(e)) => p.set_error(Some(e)),
                        None => {}
                    }
                });
            });
        });
    }

    /// Advanced → Import settings…: asks for a file and reads it on a
    /// helper thread, then asks whether to replace every setting with it.
    /// A file without a single known setting is refused.
    fn import_settings(&self) {
        if self.choosing.replace(true) {
            return;
        }
        std::thread::spawn(move || {
            let chosen = crate::portal::choose_file(
                labels::import_settings_title(),
                labels::import_accept(),
            );
            let outcome = match chosen {
                Ok(Some(path)) => Some(match std::fs::read(&path) {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let empty = text.lines().all(|l| {
                            let l = l.trim_ascii();
                            l.is_empty() || l.starts_with('#')
                        });
                        if settings::known_lines(&text) == 0 && !empty {
                            Err(labels::not_a_settings_file(&path))
                        } else {
                            crate::settings_bundle::attachment(&text)
                                .map(|library| (Settings::parse(&text), path.clone(), library))
                                .map_err(|e| labels::cannot_read(&path, &e))
                        }
                    }
                    Err(e) => Err(labels::cannot_read(&path, &e)),
                }),
                Ok(None) => None,
                Err(e) => Some(Err(labels::chooser_failed(&e))),
            };
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    p.choosing.set(false);
                    match outcome {
                        Some(Ok((read, path, library))) => {
                            *p.pending.borrow_mut() =
                                Some(Pending::Import(Box::new(read), path, library));
                            p.refresh();
                        }
                        Some(Err(e)) => p.set_error(Some(e)),
                        None => {}
                    }
                });
            });
        });
    }

    /// The answer to the Advanced page's question.
    fn pending_answered(&self, yes: bool) {
        let Some(pending) = self.pending.borrow_mut().take() else {
            return;
        };
        if !yes {
            self.refresh();
            return;
        }
        match pending {
            Pending::Import(read, path, library) => {
                if let Some(library) = library {
                    self.import_saved_settings(*read, path, library);
                    return;
                }
                let bad = read.problems.len();
                self.replace_all(*read, true);
                self.note_settings(labels::settings_imported(&path, bad));
            }
            Pending::ResetAll => {
                self.replace_all(Settings::default(), true);
                self.note_settings(labels::settings_reset().to_owned());
            }
        }
    }

    fn import_saved_settings(
        &self,
        read: Settings,
        path: PathBuf,
        library: crate::saved_searches::Library,
    ) {
        self.choosing.set(true);
        let at = self.writer.asked();
        std::thread::spawn(move || {
            let result = settings::config_dir()
                .ok_or_else(|| std::io::Error::other("settings path unavailable"))
                .and_then(|dir| {
                    crate::saved_searches::update(&dir.join("saved-searches"), |current| {
                        current.merge_import(&library)
                    })
                });
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    p.choosing.set(false);
                    match result {
                        Ok((library, ())) => {
                            crate::saved_ui::imported(library);
                            if p.writer.asked() != at {
                                p.set_error(Some(i18n::tr("Saved searches imported. Settings changed meanwhile; import again to replace them.").into()));
                                return;
                            }
                            let bad = read.problems.len();
                            p.replace_all(read, true);
                            p.note_settings(labels::settings_imported(&path, bad));
                        }
                        Err(e) => p.set_error(Some(i18n::fill(
                            i18n::tr("Cannot save saved searches: {}"),
                            &[&e],
                        ))),
                    }
                })
            });
        });
    }

    fn note_settings(&self, text: String) {
        *self.settings_note.borrow_mut() = text;
        self.set_error(None);
    }

    /// Takes `new` as every setting (an import, a reset of all, the file
    /// changed outside) and applies it to the running program, with `save`
    /// saving it in place of the file. The
    /// folders to index, their rules, file details and the index folder are
    /// taken only by the window over the default index; when they change,
    /// the index is built again. Column widths apply at the next start.
    fn replace_all(&self, mut new: Settings, save: bool) {
        new.problems.clear();
        let old = self.settings.borrow().clone();
        if save {
            let kept = new.clone();
            self.change(move |s| *s = kept.clone());
        } else {
            *self.settings.borrow_mut() = new.clone();
        }
        commands::set_current(Keymap::with_overrides(&new.shortcuts));
        if let Some(m) = self.main.upgrade() {
            crate::apply_keys(&m);
        }
        self.apply_look();
        self.apply_as_you_type();
        // The days to keep shown in Search settings follow; the history on
        // disk follows "keep the history on disk" as when it is switched.
        self.retention
            .set(search_history::Retention::new(new.history_days));
        self.history_storage_changed(old.history_on_disk);
        self.apply_history();
        self.apply_content();
        self.apply_folder_size();
        self.apply_schedule();
        self.engine.set_watching(new.watch_changes);
        self.apply_watch_changes();
        self.engine.set_drive_settings(new.drives.clone());
        self.apply_file_lists();
        if self.index.borrow().saved {
            let roots = if new.roots.is_empty() {
                self.engine.roots()
            } else {
                listed_roots(&new.roots)
            };
            let rules = new.rule_spec();
            let (indexed_roots, indexed_rules) = self.engine.setup();
            // Compared as resolved: the engine holds them resolved, or as
            // asked while a new index waits to be built.
            let same = self.engine.resolved_roots(&roots)
                == self.engine.resolved_roots(&indexed_roots)
                && rules == indexed_rules
                && new.index_file_details == self.engine.file_details();
            {
                let mut index = self.index.borrow_mut();
                index.roots = new.roots.clone();
                index.rules = rules;
            }
            self.engine.set_file_details(new.index_file_details);
            if !same {
                self.update_index();
            }
            if new.index_folder != old.index_folder {
                self.set_index_folder(new.index_folder.clone());
            }
        }
        self.rescan_error.borrow_mut().clear();
        self.rule_error.borrow_mut().clear();
        if let Some(w) = self.window.borrow().as_ref() {
            self.fill_rule_texts(w);
        }
        self.refresh();
    }

    /// Asks for the folder of the index file on a helper thread.
    fn choose_index_folder(&self) {
        if self.choosing.replace(true) {
            return;
        }
        let (title, accept) = (labels::choose_index_folder(), labels::choose_accept());
        std::thread::spawn(move || {
            let result = crate::portal::choose_folder(title, accept);
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    p.choosing.set(false);
                    match result {
                        Ok(Some(folder)) => p.set_index_folder(Some(folder)),
                        Ok(None) => {}
                        Err(e) => p.set_error(Some(labels::chooser_failed(&e))),
                    }
                });
            });
        });
    }

    /// Keeps the index file in `folder` (`None` for the cache folder): on a
    /// helper thread it checks that a file can be made there, then moves
    /// the index there and saves it at once. The setting changes only when
    /// that worked.
    pub fn set_index_folder(&self, folder: Option<PathBuf>) {
        if !self.index.borrow().saved || self.index_path().is_none() {
            return;
        }
        let engine = Arc::clone(&self.engine);
        let _ = std::thread::Builder::new()
            .name("move index".into())
            .spawn(move || {
                let folder = folder.map(|f| std::fs::canonicalize(&f).unwrap_or(f));
                let file = match &folder {
                    Some(f) => f.join(settings::INDEX_FILE_NAME),
                    None => match everygrep_core::persist::default_path() {
                        Some(f) => f,
                        None => return,
                    },
                };
                // The cache folder chosen by hand is the default.
                let folder = folder
                    .filter(|_| Some(&file) != everygrep_core::persist::default_path().as_ref());

                let writable = file
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| settings::create_temp(&file))
                    .map(|(_, temp)| {
                        let _ = std::fs::remove_file(temp);
                    });
                let writable = writable.and_then(|()| engine.move_index_file(file.clone()));
                let _ = slint::invoke_from_event_loop(move || {
                    with(|p| match writable {
                        Ok(()) => {
                            let kept = folder.clone();
                            p.change(move |s| s.index_folder = kept.clone());
                            p.set_error(None);
                            p.refresh_details();
                        }
                        Err(e) => {
                            // An import or the raw table may have saved the
                            // folder already: the setting follows the index.
                            let current = p
                                .engine
                                .index_file()
                                .filter(|f| {
                                    Some(f) != everygrep_core::persist::default_path().as_ref()
                                })
                                .and_then(|f| f.parent().map(Path::to_path_buf));
                            if p.settings.borrow().index_folder != current {
                                p.change(move |s| s.index_folder = current.clone());
                            }
                            p.set_error(Some(labels::cannot_use_index_folder(&file, &e)));
                        }
                    });
                });
            });
    }

    /// Counts the index on a helper thread and shows the numbers on the
    /// Indexing page when they come.
    fn refresh_details(&self) {
        let engine = Arc::clone(&self.engine);
        let _ = std::thread::Builder::new()
            .name("index details".into())
            .spawn(move || {
                let stats = engine.stats();
                let zone = everygrep_core::localtime::Zone::local();
                let text = labels::index_details(&stats, &zone);
                let _ = slint::invoke_from_event_loop(move || {
                    with(|p| {
                        if let Some(w) = p.window.borrow().as_ref() {
                            w.set_index_details(text.into());
                        }
                    });
                });
            });
    }

    /// Gives the engine the rescan schedule.
    pub fn apply_schedule(&self) {
        self.engine.set_schedule(self.settings.borrow().schedule());
    }

    /// Shows in the Index menu whether updates are paused.
    pub fn apply_watch_changes(&self) {
        if let Some(m) = self.main.upgrade() {
            m.set_index_paused(!self.settings.borrow().watch_changes);
        }
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
        w.set_rescan_time(schedule::time_text(self.settings.borrow().rescan_time).into());
        let index = self.index.borrow();
        w.set_include_files(index.rules.include_files.as_str().into());
        w.set_exclude_files(index.rules.exclude_files.as_str().into());
    }

    /// Builds the index again from scratch over the folders to index
    /// (Index → Rebuild index now, and the button of the Indexing page).
    pub fn rebuild(&self) {
        self.request_index(false);
    }

    fn update_index(&self) {
        self.request_index(true);
    }

    fn request_index(&self, incremental: bool) {
        let (roots, rules) = {
            let index = self.index.borrow();
            let roots = if index.roots.is_empty() {
                self.engine.roots()
            } else {
                index.roots.clone()
            };
            (roots, index.rules())
        };
        if incremental {
            self.engine.reconfigure(&roots, rules);
        } else {
            self.engine.reindex(&roots, rules);
        }
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
                    crate::debug_log::note!("cannot open the settings window: {e}");
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
        self.refresh_details();
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
        w.set_file_details(self.settings.borrow().index_file_details);
        w.set_index_file(
            self.engine
                .index_file()
                .map(|f| f.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        w.set_index_file_default(self.settings.borrow().index_folder.is_none());
        w.set_index_file_changeable(index.saved && self.index_path().is_some());
        {
            let s = self.settings.borrow();
            w.set_rescan_names(ModelRc::new(VecModel::from(
                labels::rescan_kinds()
                    .into_iter()
                    .map(SharedString::from)
                    .collect::<Vec<_>>(),
            )));
            w.set_rescan_kind(s.rescan as i32);
            w.set_rescan_hours(s.rescan_hours as i32);
            w.set_rescan_hours_max(schedule::MAX_HOURS as i32);
            w.set_rescan_error(self.rescan_error.borrow().as_str().into());
        }
        w.set_rule_error(self.rule_error.borrow().as_str().into());
        w.set_index_note(if index.saved {
            "".into()
        } else {
            labels::index_from_command_line().into()
        });
        let (state, indexed) = self.state.get();
        w.set_index_status(labels::index_status(state, indexed).into());
        w.set_can_scan_now(can_scan_now(state));

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
        drop(s);
        self.fill_file_lists(w);
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
        w.set_default_prefix(o.prefix);
        w.set_default_suffix(o.suffix);
        w.set_default_punctuation(o.ignore_punctuation);
        w.set_default_whitespace(o.ignore_whitespace);
        w.set_hidden_files(
            HiddenFiles::ALL
                .iter()
                .position(|&h| h == o.hidden_files)
                .unwrap_or(0) as i32,
        );
        w.set_hidden_names(ModelRc::new(VecModel::from(
            HiddenFiles::ALL
                .into_iter()
                .map(|h| SharedString::from(labels::hidden_files(h)))
                .collect::<Vec<_>>(),
        )));
        w.set_default_regex(o.regex);
        w.set_and_before_or(o.and_before_or);
        w.set_literal_operators(o.literal_operators);
        w.set_wildcards_anywhere(o.wildcards_anywhere);
        w.set_as_you_type(s.as_you_type);
        w.set_show_mode_badges(s.show_mode_badges);
        w.set_filter_wrap(s.filter_wrap);
        w.set_select_search_on_click(s.select_search_on_click);
        w.set_clear_selection_on_search(s.clear_selection_on_search);
        w.set_hide_results_when_empty(s.hide_results_when_empty);
        w.set_search_delay(s.search_delay_ms as i32);
        w.set_history_max(MAX_HISTORY as i32);
        w.set_history_size(s.history_size as i32);
        w.set_history_entries(s.history_entries as i32);
        w.set_history_on_disk(s.history_on_disk);
        s.content.fill_window(w);
        s.properties.fill(w);
        s.preview.fill(w);
        w.set_preview_visible(s.preview_visible);
        w.set_hash_cache_used(
            crate::content_settings::hash_cache_used(self.engine.hashes.bytes()).into(),
        );
        w.set_always_suggestions(s.always_suggestions);
        w.set_history_enabled(s.history_enabled);
        w.set_history_days(self.retention.get().draft as i32);
        w.set_committed_history_days(s.history_days as i32);

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
        w.set_open_searches_in_tabs(s.open_searches_in_tabs);
        w.set_switch_to_new_tab(s.switch_to_new_tab);
        w.set_restore_session(s.restore_session);
        w.set_remember_columns(s.remember_columns);
        w.set_rename_name_only(s.rename_name_only);
        s.fileops.fill_window(w);
        w.set_path_click_opens_folder(s.path_click_opens_folder);
        w.set_copy_full_path_quotes(s.copy_full_path_quotes);
        w.set_export_protect_formulas(s.export_protect_formulas);
        w.set_drive_root_labels(s.drive_root_labels);
        w.set_type_to_jump(s.type_to_jump);
        w.set_open_confirm_max(settings::MAX_OPEN_CONFIRM as i32);
        w.set_open_confirm_over(s.open_confirm_over as i32);
        w.set_tab_results_max(settings::MAX_TAB_RESULTS as i32);
        w.set_tab_results_kept(s.tab_results_kept as i32);
        w.set_focus_on_show(s.focus_on_show);
        w.set_dates_newest_first(s.dates_newest_first);
        w.set_sizes_largest_first(s.sizes_largest_first);

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
        w.set_status_bar(s.status_bar);
        w.set_alternate_rows(s.alternate_rows);
        w.set_size_unit_names(ModelRc::new(VecModel::from(
            SizeUnit::ALL
                .iter()
                .map(|&u| SharedString::from(labels::size_unit(u)))
                .collect::<Vec<_>>(),
        )));
        w.set_size_unit(
            SizeUnit::ALL
                .iter()
                .position(|&u| u == s.size_unit)
                .unwrap_or(0) as i32,
        );
        w.set_size_standard_names(ModelRc::new(VecModel::from(
            SizeStandard::ALL
                .iter()
                .map(|&t| SharedString::from(labels::size_standard(t)))
                .collect::<Vec<_>>(),
        )));
        w.set_size_standard(
            SizeStandard::ALL
                .iter()
                .position(|&t| t == s.size_standard)
                .unwrap_or(0) as i32,
        );
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
        w.set_export_filters(s.export_filters);
        w.set_export_bookmarks(s.export_bookmarks);
        w.set_settings_file(
            self.path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        w.set_settings_writable(self.path.is_some() && self.read_error.is_none());
        let (text, action) = match &*self.pending.borrow() {
            Some(Pending::Import(_, file, _)) => (
                labels::confirm_import(file),
                labels::import_action().to_owned(),
            ),
            Some(Pending::ResetAll) => (
                labels::confirm_reset_all().to_owned(),
                labels::reset_all_action().to_owned(),
            ),
            None => (String::new(), String::new()),
        };
        w.set_pending_text(text.into());
        w.set_pending_action(action.into());
        w.set_settings_note(self.settings_note.borrow().as_str().into());
        w.set_reload_changed(self.settings.borrow().reload_changed);
        w.set_ask_reload(self.settings.borrow().ask_reload);
        self.fill_raw(w);
        w.set_error(self.error.borrow().as_str().into());
    }

    /// The raw settings table: the rows for the filter, the chosen one kept
    /// by its key. The value being edited is set only when a row is chosen,
    /// so filling again does not undo typing.
    fn fill_raw(&self, w: &SettingsWindow) {
        let rows = raw_rows_for(
            self.settings.borrow().raw_rows(),
            &w.get_raw_filter(),
            w.get_raw_only_changed(),
            &self.raw_kept.borrow(),
        );
        // A new model scrolls the table back to the top: only when the rows
        // (or the language of their notes) changed.
        let lang = i18n::current();
        let same =
            *self.raw_listed.borrow() == rows && self.raw_lang.replace(Some(lang)) == Some(lang);
        let item = |t: &str| StandardListViewItem::from(SharedString::from(t));
        let table: Vec<ModelRc<StandardListViewItem>> = rows
            .iter()
            .map(|r| {
                let default = match (&r.default, r.known) {
                    (Some(d), _) => d.clone(),
                    (None, true) => labels::raw_list().to_owned(),
                    (None, false) => labels::raw_unknown().to_owned(),
                };
                ModelRc::new(VecModel::from(vec![
                    item(&r.key),
                    item(&r.value),
                    item(&default),
                ]))
            })
            .collect();
        if !same {
            w.set_raw_rows(ModelRc::new(VecModel::from(table)));
        }
        let chosen = self.raw_chosen.borrow().clone();
        let current = chosen.and_then(|key| rows.iter().position(|r| r.key == key));
        w.set_raw_current(current.map_or(-1, |i| i as i32));
        let row = current.map(|i| &rows[i]);
        w.set_raw_editable(row.is_some_and(settings::RawRow::editable));
        w.set_raw_toggle(
            row.is_some_and(|r| r.editable() && matches!(r.value.as_str(), "true" | "false")),
        );
        w.set_raw_resettable(row.is_some_and(|r| r.editable() && r.modified()));
        w.set_raw_error(self.raw_error.borrow().as_str().into());
        *self.raw_listed.borrow_mut() = rows;
    }

    /// A row of the raw table was chosen: its value goes into the field.
    fn raw_chosen(&self, i: usize) {
        let row = self.raw_listed.borrow().get(i).cloned();
        *self.raw_chosen.borrow_mut() = row.as_ref().map(|r| r.key.clone());
        self.raw_error.borrow_mut().clear();
        if let (Some(w), Some(r)) = (self.window.borrow().as_ref(), &row) {
            w.set_raw_value(r.value.as_str().into());
        }
        self.refresh();
    }

    /// Sets the chosen key to `value` (`None`: its default) and applies
    /// every setting, as an import does; a value that does not fit is
    /// refused with a note.
    fn raw_set(&self, value: Option<String>) {
        let Some(key) = self.raw_chosen.borrow().clone() else {
            return;
        };
        let new = self.settings.borrow().with_raw(&key, value.as_deref());
        match new {
            Some(new) => {
                self.raw_error.borrow_mut().clear();
                let mut kept = self.raw_kept.borrow_mut();
                if !kept.contains(&key) {
                    kept.push(key.clone());
                }
                drop(kept);
                self.replace_all(new, true);
                let shown = self
                    .settings
                    .borrow()
                    .raw_rows()
                    .into_iter()
                    .find(|r| r.key == key)
                    .map(|r| r.value);
                if let (Some(w), Some(v)) = (self.window.borrow().as_ref(), shown) {
                    w.set_raw_value(v.into());
                }
            }
            None => {
                *self.raw_error.borrow_mut() =
                    labels::raw_refused(&key, value.as_deref().unwrap_or(""));
            }
        }
        self.refresh();
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
        self.fill_key_dialog(w);
    }

    /// Opens the dialog for the chosen command: a new key (`replacing` is
    /// `None`) or its `replacing`th key.
    fn open_key_dialog(&self, replacing: Option<usize>) {
        let Some(cmd) = self.key_chosen.get() else {
            return;
        };
        let current = replacing.and_then(|n| commands::with_current(|k| k.of(cmd).nth(n).copied()));
        if replacing.is_some() && current.is_none() {
            return;
        }
        // A new key works where the command's first default key does.
        let scope = current.map(|b| b.scope).unwrap_or_else(|| {
            commands::spec(cmd)
                .and_then(|s| s.defaults.first())
                .map_or(Scope::Global, |d| d.0)
        });
        self.key_dialog.set(Some(KeyDialog {
            cmd,
            replacing,
            scope,
            chord: current.map(|b| b.chord),
        }));
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_key_dialog_open(true);
        }
        self.refresh();
    }

    fn close_key_dialog(&self) {
        self.key_dialog.set(None);
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_key_dialog_open(false);
        }
        self.refresh();
    }

    /// The dialog's texts: the key pressed, the commands that have it and
    /// whether it can be used.
    fn fill_key_dialog(&self, w: &SettingsWindow) {
        let Some(d) = self.key_dialog.get() else {
            return;
        };
        let label = |c: Command| commands::spec(c).map_or("", |s| (s.label)());
        let scopes: Vec<SharedString> = [
            labels::scope_global_choice(),
            Scope::Search.name(),
            Scope::List.name(),
        ]
        .into_iter()
        .map(SharedString::from)
        .collect();
        w.set_scope_names(ModelRc::new(VecModel::from(scopes)));
        w.set_key_scope(Scope::ALL.iter().position(|&s| s == d.scope).unwrap_or(0) as i32);
        w.set_key_dialog_title(
            if d.replacing.is_some() {
                labels::edit_shortcut()
            } else {
                labels::add_shortcut()
            }
            .into(),
        );
        w.set_key_dialog_command(
            commands::spec(d.cmd)
                .map(|s| format!("{} | {}", s.category.name(), (s.label)()))
                .unwrap_or_default()
                .into(),
        );
        let (captured, used, also, refused) = match d.chord {
            None => (String::new(), String::new(), String::new(), None),
            Some(chord) => commands::with_current(|k| {
                let used: Vec<String> = k
                    .conflicts(d.cmd, d.scope, chord)
                    .into_iter()
                    .map(|c| format!("{} ({})", label(c), d.scope.name()))
                    .collect();
                let also: Vec<String> = k
                    .also_used(d.cmd, d.scope, chord)
                    .into_iter()
                    .map(|(scope, c)| labels::also_used(scope.name(), label(c)))
                    .collect();
                (
                    chord.display(),
                    used.join("\n"),
                    also.join("\n"),
                    commands::refusal(d.scope, chord),
                )
            }),
        };
        let saved_owner = d.chord.and_then(crate::saved_ui::shortcut_owner);
        w.set_key_can_apply(d.chord.is_some() && refused.is_none() && saved_owner.is_none());
        w.set_key_captured(captured.into());
        w.set_key_used_by(
            if refused.is_some() {
                String::new()
            } else {
                used
            }
            .into(),
        );
        w.set_key_also_used(
            if refused.is_some() {
                String::new()
            } else {
                also
            }
            .into(),
        );
        w.set_key_refused(
            saved_owner
                .map(|owner| i18n::fill(i18n::tr("Shortcut already used by {}."), &[&owner]))
                .unwrap_or_else(|| refused.unwrap_or("").into())
                .into(),
        );
    }

    /// OK in the dialog: gives the command the key.
    fn apply_key_dialog(&self) {
        let Some(d) = self.key_dialog.get() else {
            return;
        };
        let Some(chord) = d.chord.filter(|&c| commands::refusal(d.scope, c).is_none()) else {
            return;
        };
        if crate::saved_ui::shortcut_owner(chord).is_some() {
            self.refresh();
            return;
        }
        let mut keymap = commands::with_current(Keymap::clone);
        keymap.put(d.cmd, d.scope, chord, d.replacing);
        self.key_dialog.set(None);
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_key_dialog_open(false);
            w.set_key_binding_current(-1);
        }
        self.set_keymap(keymap);
    }

    /// Uses `keymap` from now on: saves the commands whose keys differ from
    /// the defaults and gives the main window the new keys.
    fn set_keymap(&self, keymap: Keymap) {
        let overrides = keymap.overrides();
        commands::set_current(keymap);
        self.change(move |s| s.set_shortcuts(overrides.clone()));
        if let Some(m) = self.main.upgrade() {
            crate::apply_keys(&m);
        }
        self.refresh();
    }

    /// Puts every command back to its default keys.
    fn restore_keys(&self) {
        commands::set_current(Keymap::default());
        self.change(move |s| s.reset_shortcuts());
        if let Some(m) = self.main.upgrade() {
            crate::apply_keys(&m);
        }
        self.refresh();
    }

    fn connect(self: &Rc<Self>, w: &SettingsWindow) {
        let weak = w.as_weak();
        let engine = self.engine.clone();
        self.property_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(250),
            move || {
                let Some(w) = weak.upgrade() else {
                    return;
                };
                let text = if engine
                    .property_running
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    let progress = engine.property_progress.lock().expect("property progress");
                    crate::i18n::fill(
                        tr("Property indexing: {}%"),
                        &[&progress.percent().unwrap_or(0).to_string()],
                    )
                } else {
                    tr("Property indexing idle").to_owned()
                };
                w.set_property_progress(text.into());
            },
        );
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
        w.on_rescan_changed(on(|p| p.rescan_changed()));
        w.on_refresh_details(on(|p| p.refresh_details()));
        w.on_choose_index_folder(on(|p| p.choose_index_folder()));
        w.on_default_index_folder(on(|p| p.set_index_folder(None)));
        w.on_file_details_changed(on(|p| {
            let on = p.window.borrow().as_ref().map(|w| w.get_file_details());
            if let Some(on) = on {
                p.set_file_details(on);
            }
        }));
        w.on_add_exclude_pattern({
            let me = me.clone();
            move |text| {
                if let Some(p) = me.upgrade() {
                    p.add_exclude_pattern(&text);
                }
            }
        });
        w.on_rebuild(on(|p| p.rebuild()));
        Prefs::connect_file_lists(self, w);
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
        w.on_clear_hash_cache(on(|p| p.clear_hash_cache()));
        w.on_property_selection(on(|p| {
            if let Some(w) = p.window.borrow().as_ref() {
                p.settings.borrow().properties.fill_rule(w);
            }
        }));
        w.on_properties_changed(on(|p| p.properties_changed()));
        w.on_scan_properties(on(|p| p.engine.request_property_index()));
        w.on_cancel_properties(on(|p| p.engine.cancel_property_index()));
        w.on_clear_properties(on(|p| {
            p.engine.cancel_property_index();
            let store = p.engine.properties.clone();
            std::thread::spawn(move || {
                let _ = store.clear();
            });
        }));
        w.on_edit_history_days(on_index(|p, days| {
            let mut retention = p.retention.get();
            retention.edit(days as i32);
            p.retention.set(retention);
        }));
        w.on_commit_history_days(on(|p| {
            let mut retention = p.retention.get();
            let days = retention.commit();
            p.retention.set(retention);
            p.change_search_settings(move |s| s.history_days = days);
            p.apply_history();
        }));
        w.on_clear_history(on(|p| p.clear_history()));
        w.on_window_changed(on(|p| p.window_changed()));
        w.on_session_answer(|yes| {
            with(|p| {
                if let Some(w) = p.window.borrow().as_ref() {
                    w.set_session_confirm(false);
                }
                if yes {
                    p.change(crate::session::enable_with_history);
                    p.apply_history();
                    p.apply_behaviour();
                }
                p.refresh();
            })
        });
        w.on_preview_changed(on(|p| p.preview_changed()));
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
                crate::info(&m, labels::command_copied().into());
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
        w.on_add_binding(on(|p| p.open_key_dialog(None)));
        w.on_edit_binding(on_index(|p, i| p.open_key_dialog(Some(i))));
        w.on_key_dialog_cancelled(on(|p| p.close_key_dialog()));
        w.on_key_dialog_accepted(on(|p| p.apply_key_dialog()));
        w.on_key_scope_changed(on(|p| {
            let scope = p
                .window
                .borrow()
                .as_ref()
                .and_then(|w| Scope::from_index(w.get_key_scope()));
            if let (Some(mut d), Some(scope)) = (p.key_dialog.get(), scope) {
                d.scope = scope;
                p.key_dialog.set(Some(d));
                p.refresh();
            }
        }));
        w.on_key_pressed_in_dialog({
            let me = me.clone();
            move |text, ctrl, alt, shift, meta| {
                let Some(p) = me.upgrade() else { return };
                let (Some(mut d), Some(chord)) = (
                    p.key_dialog.get(),
                    Chord::from_event(&text, ctrl, alt, shift, meta),
                ) else {
                    return;
                };
                d.chord = Some(chord);
                p.key_dialog.set(Some(d));
                p.refresh();
            }
        });
        w.on_reset_page(on_index(|p, i| p.reset(i)));
        w.on_export_settings(on(|p| p.export_settings()));
        w.on_export_options_changed(on(|p| {
            let window = p.window.borrow();
            let Some(w) = window.as_ref() else { return };
            let (filters, bookmarks) = (w.get_export_filters(), w.get_export_bookmarks());
            p.change(move |s| {
                s.export_filters = filters;
                s.export_bookmarks = bookmarks;
            });
        }));
        w.on_raw_filter_changed(on(|p| {
            p.raw_kept.borrow_mut().clear();
            p.refresh();
        }));
        w.on_raw_chosen(on_index(|p, i| p.raw_chosen(i)));
        w.on_raw_apply(on(|p| {
            let value = p
                .window
                .borrow()
                .as_ref()
                .map(|w| w.get_raw_value().to_string());
            if value.is_some() {
                p.raw_set(value);
            }
        }));
        w.on_raw_toggle_value(on(|p| {
            let value = p
                .window
                .borrow()
                .as_ref()
                .map(|w| w.get_raw_value().to_string());
            let flipped = match value.as_deref().map(str::trim) {
                Some("true") => "false",
                Some("false") => "true",
                _ => return,
            };
            p.raw_set(Some(flipped.to_owned()));
        }));
        w.on_raw_reset(on(|p| p.raw_set(None)));
        w.on_raw_copy(on(|p| {
            let key = p.raw_chosen.borrow().clone();
            let row = key.and_then(|k| p.raw_listed.borrow().iter().find(|r| r.key == k).cloned());
            if let (Some(w), Some(r)) = (p.window.borrow().as_ref(), row) {
                w.invoke_copy_text(format!("{} = {}", r.key, r.value).into());
            }
        }));
        w.on_reload_changed_toggled(on(|p| {
            let on = p.window.borrow().as_ref().map(|w| w.get_reload_changed());
            if let Some(on) = on {
                p.change(move |s| s.reload_changed = on);
                // Looked at afresh when turned on again.
                *p.seen.lock().unwrap_or_else(|e| e.into_inner()) = None;
                p.refresh();
            }
        }));
        w.on_ask_reload_toggled(on(|p| {
            let on = p.window.borrow().as_ref().map(|w| w.get_ask_reload());
            if let Some(on) = on {
                p.change(move |s| s.ask_reload = on);
                p.refresh();
            }
        }));
        w.on_import_settings(on(|p| p.import_settings()));
        w.on_reset_all_settings(on(|p| {
            *p.pending.borrow_mut() = Some(Pending::ResetAll);
            p.refresh();
        }));
        w.on_pending_answered({
            let me = me.clone();
            move |yes| {
                if let Some(p) = me.upgrade() {
                    p.pending_answered(yes);
                }
            }
        });
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
        self.change(move |s| {
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
        let owned = uuid.to_owned();
        self.change(move |s| {
            s.drives.ignored.retain(|u| *u != owned);
            if indexed {
                s.drives.ignored.push(owned.clone());
            }
        });
        self.engine
            .set_drive_settings(self.settings.borrow().drives.clone());
        self.engine.ignore_drive(uuid, indexed);
        self.refresh();
    }

    /// Sets the visibility filter through the same saved choice as Settings.
    pub fn choose_hidden_files(&self, index: i32) {
        let Some(&mode) = HiddenFiles::ALL.get(index as usize) else {
            return;
        };
        self.change_search_settings(move |s| s.options.hidden_files = mode);
        self.apply_behaviour();
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
            hidden_files: HiddenFiles::ALL
                .get(w.get_hidden_files() as usize)
                .copied()
                .unwrap_or_default(),
            regex: w.get_default_regex(),
            and_before_or: w.get_and_before_or(),
            literal_operators: w.get_literal_operators(),
            wildcards_anywhere: w.get_wildcards_anywhere(),
            filter: usize::try_from(w.get_default_filter())
                .ok()
                .and_then(|i| Filter::ALL.get(i).copied())
                .unwrap_or_default(),
        };
        let size = usize::try_from(w.get_history_size())
            .unwrap_or(0)
            .min(MAX_HISTORY);
        let was_on_disk = self.settings.borrow().history_on_disk;
        let entry_limit = w.get_history_entries().clamp(1, 10000) as usize;
        let on_disk = w.get_history_on_disk();
        let enabled = w.get_history_enabled();
        let suggestions = w.get_always_suggestions();
        let as_you_type = w.get_as_you_type();
        let filter_wrap = w.get_filter_wrap();
        let show_mode_badges = w.get_show_mode_badges();
        let select_on_click = w.get_select_search_on_click();
        let clear_selection = w.get_clear_selection_on_search();
        let hide_empty = w.get_hide_results_when_empty();
        let delay = u32::try_from(w.get_search_delay())
            .unwrap_or(0)
            .min(crate::settings::MAX_SEARCH_DELAY);
        let content = crate::content_settings::Settings::from_window(&w);
        let history_before = self.settings.borrow().clone();
        let mut history_wanted = history_before.clone();
        history_wanted.history_size = size;
        history_wanted.history_entries = entry_limit;
        history_wanted.history_on_disk = on_disk;
        history_wanted.history_enabled = enabled;
        history_wanted.always_suggestions = suggestions;
        self.change_search_settings(move |s| {
            s.content = content.clone();
            s.options = options;
            s.as_you_type = as_you_type;
            s.show_mode_badges = show_mode_badges;
            s.filter_wrap = filter_wrap;
            s.select_search_on_click = select_on_click;
            s.clear_selection_on_search = clear_selection;
            s.hide_results_when_empty = hide_empty;
            s.search_delay_ms = delay;
            crate::history_sync::apply_choices(s, &history_before, &history_wanted);
        });
        self.history_storage_changed(was_on_disk);
        self.apply_history();
        self.apply_as_you_type();
        self.apply_behaviour();
        self.apply_content();
    }

    fn properties_changed(&self) {
        let window = self.window.borrow();
        let Some(w) = window.as_ref() else {
            return;
        };
        let edited = self.settings.borrow().properties.edited(w);
        match edited {
            Ok(next) => {
                w.set_property_error("".into());
                let saved = next.clone();
                self.change(move |s| s.properties = saved.clone());
                self.engine.set_property_options(next);
            }
            Err(e) => w.set_property_error(
                crate::i18n::fill(tr("Invalid property filters: {}"), &[&e]).into(),
            ),
        }
    }

    /// Gives the engine the content search settings; a change searches
    /// again.
    fn apply_content(&self) {
        let (options, hash_bytes, compare) = {
            let s = self.settings.borrow();
            (
                s.content.options(),
                s.content.hash_cache_bytes(),
                s.content.compare(),
            )
        };
        self.engine.set_content_options(options);
        self.engine.set_compare_options(compare);
        self.engine.hashes.set_max_bytes(hash_bytes);
        self.engine
            .set_property_options(self.settings.borrow().properties.clone());
    }

    /// Empties the hash cache and removes its file.
    fn clear_hash_cache(&self) {
        self.engine.hashes.clear_later(|result, used| {
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    if let Some(w) = p.window.borrow().as_ref() {
                        w.set_hash_cache_used(
                            crate::content_settings::hash_cache_used(used).into(),
                        );
                    }
                    if let Err(e) = result {
                        p.set_error(Some(i18n::fill(
                            i18n::tr("Could not remove the hash cache file: {}"),
                            &[&e.to_string()],
                        )));
                    }
                });
            });
        });
    }

    /// A saved query may override parsing for its own expression. The next
    /// manual edit returns to the policy shown in Search settings.
    pub fn restore_parse_options(&self, w: &AppWindow) {
        let options = self.settings.borrow().options;
        w.set_wildcards_anywhere(options.wildcards_anywhere);
        w.set_literal_operators(options.literal_operators);
        w.set_and_before_or(options.and_before_or);
    }

    /// Applies interaction settings and search policy to the main window.
    /// A changed search policy reruns the current query.
    pub fn apply_behaviour(&self) {
        let Some(m) = self.main.upgrade() else { return };
        self.fill_history();
        let s = self.settings.borrow();
        let old_options = crate::options(&m);
        self.restore_parse_options(&m);
        m.set_hidden_files(
            HiddenFiles::ALL
                .iter()
                .position(|&h| h == s.options.hidden_files)
                .unwrap_or(0) as i32,
        );
        m.set_show_mode_badges(s.show_mode_badges);
        m.set_filter_wrap(s.filter_wrap);
        m.set_select_search_on_click(s.select_search_on_click);
        self.engine.set_hide_when_empty(s.hide_results_when_empty);
        crate::with_model(&m, |model| model.apply_behaviour(&s));
        m.set_alternate_rows(s.alternate_rows);
        m.set_path_click_opens_folder(s.path_click_opens_folder);
        m.set_open_searches_in_tabs(s.open_searches_in_tabs);
        m.set_switch_to_new_tab(s.switch_to_new_tab);
        m.set_restore_session(s.restore_session);
        drop(s);
        crate::tabs_ui::session_policy();
        if old_options != crate::options(&m) {
            crate::show_badges(&m);
            crate::search_now(&m, &self.engine);
        }
    }

    /// Gives the main window the "search as you type" setting and whether
    /// searches that read files wait for Enter meanwhile.
    pub fn apply_as_you_type(&self) {
        let (on, contents_on_enter, delay) = {
            let s = self.settings.borrow();
            (s.as_you_type, s.content.on_enter, s.search_delay_ms)
        };
        crate::typing::with(|t| t.set_delay(delay));
        if let Some(m) = self.main.upgrade() {
            crate::set_as_you_type(&m, &self.engine, on, contents_on_enter);
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
        let status_bar = w.get_status_bar();
        let alternate_rows = w.get_alternate_rows();
        let size_unit = usize::try_from(w.get_size_unit())
            .ok()
            .and_then(|i| SizeUnit::ALL.get(i).copied())
            .unwrap_or_default();
        let size_standard = usize::try_from(w.get_size_standard())
            .ok()
            .and_then(|i| SizeStandard::ALL.get(i).copied())
            .unwrap_or_default();
        let language = usize::try_from(w.get_language())
            .ok()
            .and_then(|i| LanguageChoice::ALL.get(i).copied())
            .unwrap_or_default();
        self.change(move |s| {
            s.theme = theme;
            s.density = density;
            s.status_details = details;
            s.menu_bar = menu_bar;
            s.status_bar = status_bar;
            s.alternate_rows = alternate_rows;
            s.size_unit = size_unit;
            s.size_standard = size_standard;
            s.language = language;
        });
        self.apply_look();
        self.refresh();
    }

    /// Applies the settings of the Appearance page to the windows.
    pub fn color_scheme(&self) -> i32 {
        look::resolve(self.settings.borrow().theme, self.desktop_scheme.get()) as i32
    }

    pub fn apply_look(&self) {
        self.apply_language();
        self.apply_theme();
        self.apply_density();
        self.apply_status_details();
        self.apply_menu_bar();
        self.apply_behaviour();
        self.apply_size_style();
        self.fill_menus();
    }

    /// Shows sizes in the format of the settings, when it changed.
    pub fn apply_size_style(&self) {
        let style = {
            let s = self.settings.borrow();
            format::SizeStyle {
                unit: s.size_unit,
                standard: s.size_standard,
            }
        };
        if format::size_style() == style {
            return;
        }
        format::set_size_style(style);
        if let Some(m) = self.main.upgrade() {
            crate::with_model(&m, |model| model.relabel());
            crate::refresh_status(&m);
        }
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
        self.change(move |s| s.status_details = on);
        self.apply_status_details();
        self.fill_menus();
        self.refresh();
    }

    /// Changes the density setting and repacks the list.
    pub fn set_density(&self, density: Density) {
        self.change(move |s| s.density = density);
        self.apply_density();
        self.fill_menus();
        self.refresh();
    }

    /// Changes the theme setting and recolors the windows.
    pub fn set_theme(&self, theme: ThemeChoice) {
        self.change(move |s| s.theme = theme);
        self.apply_theme();
        self.fill_menus();
        self.refresh();
    }

    /// Follows a change of "keep the history on disk": turned on, the
    /// searches in memory join those on disk; turned off, the saved history
    /// is deleted (memory keeps it).
    fn history_storage_changed(&self, was_on_disk: bool) {
        let on_disk = self.settings.borrow().history_on_disk;
        if on_disk && !was_on_disk {
            let memory = self.searches.borrow().clone();
            self.change_searches(move |entries| {
                let mut seen: std::collections::HashSet<_> =
                    entries.0.iter().map(|e| e.search.clone()).collect();
                for e in &memory.0 {
                    if seen.insert(e.search.clone()) {
                        entries.0.push(e.clone());
                    }
                }
            });
        }
        if was_on_disk && !on_disk {
            self.clear_saved_history(false);
        }
    }

    /// Apply recording and retention without writing a stale snapshot.
    fn apply_history(&self) {
        let s = self.settings.borrow();
        let size = if s.history_enabled { s.history_size } else { 0 };
        drop(s);
        self.history.borrow_mut().set_max(size);
        let days = self.settings.borrow().history_days;
        self.searches
            .borrow_mut()
            .retain_days(days, search_history::now());
        crate::tabs_ui::history_policy(size, false);
        self.change_searches(|_| {});
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
        let open_tabs = w.get_open_searches_in_tabs();
        let switch_tabs = w.get_switch_to_new_tab();
        let mut restore = w.get_restore_session();
        if restore && !self.settings.borrow().restore_session && self.needs_session_history() {
            restore = false;
            w.set_session_confirm(true);
        }
        let remember = w.get_remember_columns();
        let name_only = w.get_rename_name_only();
        let fileops = crate::fileops_settings::Settings::from_window(&w);
        let path_click = w.get_path_click_opens_folder();
        let quotes = w.get_copy_full_path_quotes();
        let protect_formulas = w.get_export_protect_formulas();
        let root_labels = w.get_drive_root_labels();
        let type_to_jump = w.get_type_to_jump();
        let open_over = usize::try_from(w.get_open_confirm_over())
            .unwrap_or(0)
            .min(settings::MAX_OPEN_CONFIRM);
        let focus_on_show = w.get_focus_on_show();
        let tab_results = usize::try_from(w.get_tab_results_kept())
            .unwrap_or(0)
            .min(settings::MAX_TAB_RESULTS);
        let dates_newest_first = w.get_dates_newest_first();
        let sizes_largest_first = w.get_sizes_largest_first();
        self.change(move |s| {
            s.open_searches_in_tabs = open_tabs;
            s.switch_to_new_tab = switch_tabs;
            s.restore_session = restore;
            s.dates_newest_first = dates_newest_first;
            s.sizes_largest_first = sizes_largest_first;
            s.on_close = on_close;
            s.folder_size = folder_size;
            s.remember_columns = remember;
            s.rename_name_only = name_only;
            s.fileops = fileops.clone();
            s.path_click_opens_folder = path_click;
            s.copy_full_path_quotes = quotes;
            s.export_protect_formulas = protect_formulas;
            s.drive_root_labels = root_labels;
            s.type_to_jump = type_to_jump;
            s.open_confirm_over = open_over;
            s.tab_results_kept = tab_results;
            s.focus_on_show = focus_on_show;
            if !remember {
                s.columns = Columns::default();
            }
        });
        self.apply_folder_size();
        self.apply_behaviour();
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
            self.change(move |s| {
                s.drives = Default::default();
                s.file_lists.clear();
                s.hide_offline = false;
                s.watch_changes = true;
                s.index_file_details = true;
                s.rescan = 0;
                s.rescan_hours = settings::DEFAULT_RESCAN_HOURS;
                s.rescan_time = settings::DEFAULT_RESCAN_TIME;
            });
            self.rescan_error.borrow_mut().clear();
            self.apply_schedule();
            self.engine.set_watching(true);
            self.engine.set_file_details(true);
            self.apply_watch_changes();
            self.engine
                .set_drive_settings(self.settings.borrow().drives.clone());
            self.apply_file_lists();
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
        let was_on_disk = self.settings.borrow().history_on_disk;
        if page == Page::Search {
            self.change_search_settings(move |s| s.reset(page));
        } else {
            self.change(move |s| s.reset(page));
        }
        if page == Page::Search {
            self.retention.set(search_history::Retention::new(
                self.settings.borrow().history_days,
            ));
            self.history_storage_changed(was_on_disk);
            self.apply_history();
            self.apply_as_you_type();
            self.apply_content();
        }
        self.apply_behaviour();
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
            let result = crate::portal::choose_folders(title, accept).map(|chosen| {
                chosen.map(|paths| {
                    paths
                        .into_iter()
                        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
                        .collect::<Vec<_>>()
                })
            });
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
        let chosen = chosen.iter().cloned();
        let mut list_roots = self.shown_roots();
        let mut rules = self.index.borrow().rules.clone();
        if roots {
            list_roots.extend(chosen);
            list_roots = listed_roots(&list_roots);
        } else {
            for c in chosen {
                if !rules.folders.contains(&c) {
                    rules.folders.push(c);
                }
            }
        }
        self.set_index(list_roots, rules);
    }

    fn needs_session_history(&self) -> bool {
        let s = self.settings.borrow();
        !s.history_enabled || !s.history_on_disk || s.history_size == 0
    }

    pub fn set_tab_preference(self: &Rc<Self>, index: i32, on: bool) {
        if index == 2 && on && self.needs_session_history() {
            if let Some(main) = self.main.upgrade() {
                main.set_restore_session(self.settings.borrow().restore_session);
            }
            self.open(Some(3));
            if let Some(w) = self.window.borrow().as_ref() {
                w.set_session_confirm(true);
            }
            return;
        }
        self.change(move |s| match index {
            0 => s.open_searches_in_tabs = on,
            1 => s.switch_to_new_tab = on,
            2 => s.restore_session = on,
            _ => {}
        });
        self.apply_behaviour();
        self.refresh();
    }

    pub fn restore_search_policy(&self, w: &AppWindow) {
        self.restore_parse_options(w);
        let hidden = self.settings.borrow().options.hidden_files;
        w.set_hidden_files(
            HiddenFiles::ALL
                .iter()
                .position(|h| *h == hidden)
                .unwrap_or(0) as i32,
        );
    }

    pub fn search_policy(&self) -> QueryOptions {
        self.settings.borrow().options
    }

    pub fn history_limit(&self) -> usize {
        let s = self.settings.borrow();
        if s.history_enabled { s.history_size } else { 0 }
    }

    pub fn restore_session(&self) -> bool {
        let s = self.settings.borrow();
        crate::session::can_persist(
            s.restore_session,
            s.history_enabled && s.history_size > 0,
            s.history_on_disk,
        )
    }

    pub fn open_searches_in_tabs(&self) -> bool {
        self.settings.borrow().open_searches_in_tabs
    }
    pub fn switch_to_new_tab(&self) -> bool {
        self.settings.borrow().switch_to_new_tab
    }

    /// Preferences of the file action modules.
    pub fn fileops_settings(&self) -> crate::fileops_settings::Settings {
        self.settings.borrow().fileops.clone()
    }

    /// Whether renaming preselects the name without its extension.
    pub fn rename_name_only(&self) -> bool {
        self.settings.borrow().rename_name_only
    }

    /// Opening more items than this at once asks first; 0 never asks.
    /// How many background tabs keep their results in memory.
    pub fn tab_results_kept(&self) -> usize {
        self.settings.borrow().tab_results_kept
    }

    pub fn open_confirm_over(&self) -> usize {
        self.settings.borrow().open_confirm_over
    }

    /// Whether Copy full path puts each path in double quotes.
    pub fn copy_full_path_quotes(&self) -> bool {
        self.settings.borrow().copy_full_path_quotes
    }

    /// The last successful result-export file type.
    pub fn export_format(&self) -> crate::export::Format {
        self.settings.borrow().export_format
    }

    pub fn export_protect_formulas(&self) -> bool {
        self.settings.borrow().export_protect_formulas
    }

    pub fn set_export_format(&self, format: crate::export::Format) {
        self.change(move |s| s.export_format = format);
    }

    /// Whether typing in the result list jumps to a name.
    pub fn type_to_jump(&self) -> bool {
        self.settings.borrow().type_to_jump
    }

    /// Whether showing the window focuses the search field.
    pub fn focus_on_show(&self) -> bool {
        self.settings.borrow().focus_on_show
    }

    /// Whether the first click on the header of `column` (numbered as the
    /// window's sort column) sorts it descending.
    pub fn first_descending(&self, column: i32) -> bool {
        let s = self.settings.borrow();
        crate::first_descending(column, s.dates_newest_first, s.sizes_largest_first)
    }

    /// Whether closing the main window should only hide it.
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

/// The rows of the raw settings table for its filter text (matched in the
/// key or the value, ignoring case) and "Only changed settings". A setting
/// changed in the table (`kept`) stays listed under "Only changed settings"
/// after it went back to its default, so that the rows do not move under
/// the pointer and the chosen row keeps its place; a new filter drops it.
fn raw_rows_for(
    rows: Vec<settings::RawRow>,
    filter: &str,
    only_changed: bool,
    kept: &[String],
) -> Vec<settings::RawRow> {
    let filter = filter.to_lowercase();
    rows.into_iter()
        .filter(|r| !only_changed || r.modified() || kept.contains(&r.key))
        .filter(|r| {
            filter.is_empty()
                || r.key.to_lowercase().contains(&filter)
                || r.value.to_lowercase().contains(&filter)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::scan::normalize_roots;

    fn wait_saved(writer: &settings::Writer) {
        assert!(writer.wait_done(std::time::Duration::from_secs(5)));
    }
    use crate::settings::tests::Scratch;

    #[test]
    fn saves_of_other_everygrep_windows_apply_without_a_question() {
        assert!(asks_first(true, false, false), "an edit made outside");
        assert!(!asks_first(true, true, false), "another window saved it");
        assert!(!asks_first(true, false, true), "the user agreed already");
        assert!(!asks_first(false, false, false), "Ask first is off");
    }

    #[test]
    fn a_turned_down_outside_change_is_not_asked_about_again() {
        let mine = Settings::default();
        let outside = Settings::parse("history-size = 7\n");
        let lines = outside.differing_lines(&mine);
        let none = std::collections::BTreeSet::new();
        assert!(
            ask_about(&lines, &none),
            "a new outside change is asked about"
        );
        assert!(!ask_about(&none, &none), "nothing differs, nothing to ask");
        // Turned down; a later save here writes over the file but keeps the
        // outside line, so the same difference comes back.
        assert!(!ask_about(&lines, &lines));
        // A save here of the same setting makes the difference smaller.
        let mut smaller = lines.clone();
        smaller.pop_first();
        assert!(!ask_about(&smaller, &lines));
        // Another outside change is asked about again.
        let more = Settings::parse("history-size = 7\nhistory-days = 3\n").differing_lines(&mine);
        assert!(ask_about(&more, &lines));
    }

    #[test]
    fn a_cancelled_reload_stays_declined_after_unrelated_local_saves() {
        let dir = Scratch::new("reload-local-save");
        let path = dir.0.join("settings");
        let mut running = Settings::default();
        let outside = Settings::parse("history-size = 7\n");
        outside.save(&path).unwrap();
        let declined = outside.differing_lines(&running);
        // Cancel the outside change, then save an unrelated local setting.
        running.menu_bar = false;
        settings::update(&path, |s| s.menu_bar = false).unwrap();
        let (read, own, local) = settings::load_marked(&path).unwrap();
        assert!(own && local);
        let applied = reload_candidate(read, &running, &declined, local);
        assert_eq!(applied.history_size, running.history_size);
        assert!(!applied.menu_bar);
        // Declined values must survive more than one local save/reload.
        running.status_details = false;
        settings::update(&path, |s| s.status_details = false).unwrap();
        let (read, _, local) = settings::load_marked(&path).unwrap();
        let applied = reload_candidate(read, &running, &declined, local);
        assert_eq!(applied.history_size, running.history_size);
        assert!(!applied.menu_bar && !applied.status_details);
    }

    #[test]
    fn a_declined_reload_can_be_accepted_by_another_window_or_explicit_approval() {
        let running = Settings::default();
        let read = Settings::parse("history-size = 7\n");
        let declined = read.differing_lines(&running);
        // Another instance's save, or a user-approved reload, is not filtered.
        let applied = reload_candidate(read, &running, &declined, false);
        assert_eq!(applied.history_size, 7);
    }

    #[test]
    fn a_setting_reset_in_only_changed_stays_in_its_place() {
        let row = |key: &str, value: &str, default: &str| settings::RawRow {
            key: key.to_owned(),
            value: value.to_owned(),
            default: Some(default.to_owned()),
            known: true,
        };
        // theme was reset (or toggled back) to its default in the table.
        let rows = || {
            vec![
                row("density", "compact", "comfortable"),
                row("file-undo-enabled", "true", "false"),
                row("theme", "system", "system"),
                row("type-to-jump", "false", "true"),
            ]
        };
        let keys = |rows: Vec<settings::RawRow>| -> Vec<String> {
            rows.into_iter().map(|r| r.key).collect()
        };
        assert_eq!(
            keys(raw_rows_for(rows(), "", true, &[])),
            ["density", "file-undo-enabled", "type-to-jump"]
        );
        let kept = ["theme".to_owned()];
        assert_eq!(
            keys(raw_rows_for(rows(), "", true, &kept)),
            ["density", "file-undo-enabled", "theme", "type-to-jump"]
        );
        // The text filter still applies to a kept row, and with every
        // setting listed nothing changes.
        assert_eq!(
            keys(raw_rows_for(rows(), "TYPE", true, &kept)),
            ["type-to-jump"]
        );
        assert_eq!(keys(raw_rows_for(rows(), "", false, &[])).len(), 4);
        assert_eq!(
            keys(raw_rows_for(rows(), "compact", false, &[])),
            ["density"]
        );
    }

    #[test]
    fn command_line_folders_are_never_saved() {
        let dir = Scratch::new("setup");
        let path = dir.0.join("settings");
        let writer = settings::Writer::start();
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
            setup.change(
                vec![PathBuf::from("/tmp/demo"), PathBuf::from("/tmp/more")],
                RuleSpec {
                    folders: vec![PathBuf::from("/tmp/demo/x")],
                    ..RuleSpec::default()
                },
                Some(&path),
                &writer,
            );
            wait_saved(&writer);
            assert_eq!(setup.roots.len(), 2);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[test]
    fn default_window_uses_and_saves_the_settings() {
        let dir = Scratch::new("setup-saved");
        let path = dir.0.join("settings");
        let writer = settings::Writer::start();
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
        setup.change(
            vec![PathBuf::from("/srv")],
            rules.clone(),
            Some(&path),
            &writer,
        );
        wait_saved(&writer);
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
    fn a_folder_spelled_through_a_link_keeps_its_spelling() {
        let dir = Scratch::new("setup-follow-link");
        let path = dir.0.join("settings");
        let writer = settings::Writer::start();
        let link = PathBuf::from("/home/me/link");
        let target = PathBuf::from("/data/target");
        let mut setup = IndexSetup::new(&[], None, &Settings::default());
        setup.change(
            vec![link.clone()],
            RuleSpec::default(),
            Some(&path),
            &writer,
        );
        wait_saved(&writer);
        std::fs::remove_file(&path).unwrap();
        let resolve = |roots: &[PathBuf]| -> Vec<PathBuf> {
            roots
                .iter()
                .map(|r| {
                    if *r == link {
                        target.clone()
                    } else {
                        r.clone()
                    }
                })
                .collect()
        };
        // The engine kept the same folder, resolved: nothing changes.
        assert!(!setup.follow(
            std::slice::from_ref(&target),
            &RuleSpec::default(),
            Some(&path),
            &writer,
            &resolve
        ));
        wait_saved(&writer);
        assert_eq!(setup.roots, std::slice::from_ref(&link));
        assert!(!path.exists(), "not written");
        // Another folder: taken over.
        let other = PathBuf::from("/data/other");
        assert!(setup.follow(
            std::slice::from_ref(&other),
            &RuleSpec::default(),
            Some(&path),
            &writer,
            &resolve
        ));
        wait_saved(&writer);
        assert_eq!(setup.roots, [other]);
    }

    #[test]
    fn scan_buttons_are_off_only_while_the_folders_are_read() {
        assert!(!can_scan_now(IndexState::Scanning));
        for state in [
            IndexState::Loading,
            IndexState::CatchingUp,
            IndexState::Watching,
            IndexState::Static,
            IndexState::Unwatched,
        ] {
            assert!(can_scan_now(state), "{state:?}");
        }
    }

    #[test]
    fn a_cancelled_new_index_puts_the_saved_folders_back() {
        let dir = Scratch::new("setup-follow");
        let path = dir.0.join("settings");
        let writer = settings::Writer::start();
        let (a, b) = (dir.0.join("a"), dir.0.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut setup = IndexSetup::new(&[], None, &Settings::default());
        setup.change(vec![a.clone()], RuleSpec::default(), Some(&path), &writer);
        wait_saved(&writer);
        let kept = normalize_roots(std::slice::from_ref(&a));
        // Asked for, then cancelled: the engine went back to "a".
        let asked = RuleSpec {
            exclude_files: "*.tmp".into(),
            ..RuleSpec::default()
        };
        let (release, blocked) = std::sync::mpsc::channel();
        writer.run_apart(move || {
            blocked.recv().unwrap();
        });
        setup.change(vec![b], asked, Some(&path), &writer);
        assert!(setup.follow(
            &kept,
            &RuleSpec::default(),
            Some(&path),
            &writer,
            &normalize_roots
        ));
        // Reaching here while the writer is blocked proves the UI calls did
        // not wait for storage. Rollback must be written after the request.
        release.send(()).unwrap();
        wait_saved(&writer);
        assert_eq!(setup.roots, kept);
        assert_eq!(setup.rules, RuleSpec::default());
        let back = Settings::load(&path).unwrap();
        assert_eq!(back.roots, kept);
        assert_eq!(back.rule_spec(), RuleSpec::default());
        // Nothing to take over: the file is not written.
        std::fs::remove_file(&path).unwrap();
        assert!(!setup.follow(
            &kept,
            &RuleSpec::default(),
            Some(&path),
            &writer,
            &normalize_roots
        ));
        wait_saved(&writer);
        assert!(!path.exists());
        // Folders left empty stay empty.
        let mut empty = IndexSetup::new(&[], None, &Settings::default());
        assert!(!empty.follow(&kept, &RuleSpec::default(), None, &writer, &normalize_roots));
        assert!(empty.roots.is_empty());
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
