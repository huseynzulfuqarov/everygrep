//! EveryGrep desktop window.

mod actions;
mod engine;
mod filetype;
mod format;
mod history;
mod instance;
mod labels;
mod open;
mod portal;
mod prefs;
mod results;
mod selection;
mod settings;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use everygrep_core::filter::Filter;
use everygrep_core::query::QueryOptions;
use everygrep_core::sort::{Sort, SortKey};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use engine::{Engine, Hooks, IndexState, SearchResult};
use prefs::{IndexSetup, Prefs};
use results::ResultsModel;

slint::include_modules!();

/// Selection modes of the window's `select` callback.
const SELECT_TOGGLE: i32 = 1;
const SELECT_EXTEND: i32 = 2;

/// Wayland app id; equals the desktop entry and icon file names.
const APP_ID: &str = "az.huseynz.EveryGrep";

/// A query typed and left alone this long is kept in the search history.
const HISTORY_PAUSE: Duration = Duration::from_secs(1);

/// `--settings` opens the settings window this long after the main window.
const SETTINGS_DELAY: Duration = Duration::from_millis(300);

fn main() -> Result<(), slint::PlatformError> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(raw.iter().cloned()) {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(Command::Version) => {
            println!("everygrep-gui {}", everygrep_core::VERSION);
            return Ok(());
        }
        Err(e) => {
            eprintln!("everygrep-gui: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    // Held until the window closes: dropping it releases the bus name.
    let _instance = if args.new_instance {
        None
    } else {
        let (roots, file) = instance::identity(&args.roots, args.index_file.as_deref());
        let name = instance::bus_name(
            &roots,
            file.as_deref(),
            instance::default_index().as_deref(),
        );
        match instance::start(&name, &raw, Box::new(queue_present)) {
            instance::Role::Primary(connection) => Some(connection),
            instance::Role::HandedOver => return Ok(()),
            instance::Role::Alone => None,
        }
    };
    let (settings_path, settings, settings_error) = prefs::load();
    let setup = IndexSetup::new(&args.roots, args.index_file.as_deref(), &settings);
    // Options given on the command line take the place of the saved ones.
    let start_options = if args.options_given {
        args.options
    } else {
        settings.options
    };
    let window = AppWindow::new()?;
    // Read when the native window is created on first show, so it must be set
    // before `run()`. Matches the desktop entry, so the shell shows our icon.
    slint::set_xdg_app_id(APP_ID)?;
    let weak = window.as_weak();

    let hooks = Hooks {
        on_results: Box::new({
            let weak = weak.clone();
            move |result: SearchResult| {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        show_results(&w, result);
                    }
                });
            }
        }),
        on_state: Box::new({
            let weak = weak.clone();
            move |state: IndexState, indexed: usize| {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        with_model(&w, |m| m.set_state(state, indexed));
                        refresh_status(&w);
                    }
                    prefs::with(|p| p.set_state(state, indexed));
                });
            }
        }),
    };
    let engine = Engine::start(
        setup.roots.clone(),
        setup.excludes.clone(),
        args.index_file,
        hooks,
    );
    let model = Rc::new(ResultsModel::new(engine.clone()));
    window.set_results(ModelRc::from(model.clone()));
    let filter_names: Vec<SharedString> = Filter::ALL
        .iter()
        .map(|&f| labels::filter(f).into())
        .collect();
    window.set_filter_names(ModelRc::new(VecModel::from(filter_names)));
    set_options(&window, start_options);
    window.set_show_type(args.show_type);
    window.set_show_ext(args.show_ext);
    let prefs = Prefs::install(
        settings_path,
        settings,
        settings_error,
        setup,
        engine.clone(),
        &window,
    );
    prefs.restore_columns(&window);
    refresh_status(&window);
    // The search thread runs the query now and again once the index is
    // loaded, so the first results appear as soon as they can.
    if !args.query.is_empty() || start_options != QueryOptions::default() {
        window.set_query(args.query.as_str().into());
        engine.set_query(&args.query, start_options);
        prefs.record(&args.query);
    }

    let pause = Rc::new(Timer::default());
    window.on_query_edited({
        let (engine, weak, pause) = (engine.clone(), weak.clone(), pause.clone());
        move |text| {
            let Some(w) = weak.upgrade() else { return };
            engine.set_query(&text, options(&w));
            w.set_notice("".into());
            pause.start(TimerMode::SingleShot, HISTORY_PAUSE, move || {
                prefs::with(|p| p.record(&text));
            });
        }
    });
    window.on_options_changed({
        let (engine, weak) = (engine.clone(), weak.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                engine.set_query(&w.get_query(), options(&w));
                show_badges(&w);
            }
        }
    });
    window.on_history_record({
        let (weak, pause, prefs) = (weak.clone(), pause.clone(), prefs.clone());
        move || {
            pause.stop();
            if let Some(w) = weak.upgrade() {
                prefs.record(&w.get_query());
            }
        }
    });
    window.on_history_step({
        let (engine, weak, pause, prefs) =
            (engine.clone(), weak.clone(), pause.clone(), prefs.clone());
        move |step| {
            let Some(w) = weak.upgrade() else { return };
            pause.stop();
            let shown = w.get_query();
            let next = {
                let mut history = prefs.history.borrow_mut();
                if step < 0 {
                    history.back(&shown)
                } else {
                    history.forward(&shown)
                }
                .map(str::to_owned)
            };
            // Back and Forward record text that was typed but not kept yet.
            prefs.save_history();
            if let Some(text) = next {
                w.set_query(text.as_str().into());
                // Setting the text keeps the old cursor offset; put the caret
                // at the end of the restored query instead.
                w.invoke_place_search_caret(i32::try_from(text.len()).unwrap_or(i32::MAX));
                engine.set_query(&text, options(&w));
                w.set_notice("".into());
            }
        }
    });
    window.on_open_settings({
        let prefs = prefs.clone();
        move || prefs.open(None)
    });
    window.on_quit({
        let (weak, prefs) = (weak.clone(), prefs.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                prefs.save_columns(&w);
            }
            let _ = slint::quit_event_loop();
        }
    });
    window.on_sort_changed({
        let engine = engine.clone();
        move |column, descending| {
            engine.set_sort(sort_for_column(column, descending));
        }
    });
    window.on_select({
        let weak = weak.clone();
        let model = model.clone();
        move |row, mode| {
            let Ok(row) = usize::try_from(row) else {
                return;
            };
            match mode {
                SELECT_TOGGLE => model.toggle(row),
                SELECT_EXTEND => model.extend_to(row),
                _ => model.select_only(row),
            }
            if let Some(w) = weak.upgrade() {
                refresh_status(&w);
            }
        }
    });
    window.on_select_all({
        let weak = weak.clone();
        let model = model.clone();
        move || {
            model.select_all();
            if let Some(w) = weak.upgrade() {
                refresh_status(&w);
            }
        }
    });
    window.on_clear_selection({
        let weak = weak.clone();
        let model = model.clone();
        move || {
            let had = model.clear_selection();
            if let Some(w) = weak.upgrade() {
                refresh_status(&w);
            }
            had
        }
    });
    window.on_is_selected({
        let model = model.clone();
        move |row| usize::try_from(row).is_ok_and(|r| model.is_selected(r))
    });
    connect_actions(&window, &model);
    ON_PRESENT.with(|p| {
        *p.borrow_mut() = Some(Box::new({
            let (weak, engine, pause, prefs) = (weak.clone(), engine.clone(), pause, prefs.clone());
            move |request| {
                if let Some(w) = weak.upgrade() {
                    present(&w, &engine, &pause, &prefs, request);
                }
            }
        }));
    });
    // Requests that came in while the window was being set up.
    run_presents();

    // The window is hidden and shown again to raise it (see `raise`), so
    // the event loop must not stop when it is hidden. Closing it quits, or
    // only hides it when the settings say so; a later launch shows it again.
    window.window().on_close_requested({
        let (weak, engine, prefs) = (weak.clone(), engine.clone(), prefs.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                prefs.save_columns(&w);
            }
            if prefs.hide_on_close() {
                // Keep the index on disk current in case the session ends
                // while the window is hidden.
                let engine = engine.clone();
                std::thread::spawn(move || engine.save());
            } else {
                let _ = slint::quit_event_loop();
            }
            slint::CloseRequestResponse::HideWindow
        }
    });
    window.show()?;
    if let Some(page) = args.settings_page {
        // Opened once the main window is on screen, so the settings window
        // is mapped after it and stays in front.
        let prefs = prefs.clone();
        Timer::single_shot(SETTINGS_DELAY, move || prefs.open(Some(page)));
    }
    slint::run_event_loop_until_quit()?;
    window.hide()?;
    engine.shutdown();
    Ok(())
}

/// Requests from later launches, waiting for the UI thread.
static PRESENTS: Mutex<Vec<instance::Request>> = Mutex::new(Vec::new());

/// Applies a later launch's request to the window.
type Presenter = Box<dyn Fn(instance::Request)>;

thread_local! {
    /// Set once the window is ready.
    static ON_PRESENT: RefCell<Option<Presenter>> = RefCell::default();
}

/// Called on the bus thread: queues the request and wakes the UI thread.
fn queue_present(request: instance::Request) {
    PRESENTS.lock().expect("lock").push(request);
    // Fails only before the event loop exists; the queue is run once the
    // window is ready.
    let _ = slint::invoke_from_event_loop(run_presents);
}

/// Runs the queued requests on the UI thread, once the window is ready.
fn run_presents() {
    ON_PRESENT.with(|p| {
        let Some(present) = &*p.borrow() else { return };
        let requests = std::mem::take(&mut *PRESENTS.lock().expect("lock"));
        requests.into_iter().for_each(present);
    });
}

/// What a later launch changes in the running window.
#[derive(Debug, Default, PartialEq, Eq)]
struct Presentation {
    /// Settings page to open.
    settings_page: Option<usize>,
    /// New query, when the launch had query words.
    query: Option<String>,
    /// New filter and matching modes, when the launch set any of them.
    options: Option<QueryOptions>,
    /// Optional columns to show; columns already shown stay.
    show_type: bool,
    show_ext: bool,
}

/// The changes a later launch's arguments ask for. Arguments that do not
/// parse (they were checked by the launch already) change nothing.
fn presentation(args: &[String]) -> Presentation {
    match parse_args(args.iter().cloned()) {
        Ok(Command::Run(a)) => Presentation {
            query: (!a.query.is_empty()).then_some(a.query),
            options: a.options_given.then_some(a.options),
            show_type: a.show_type,
            show_ext: a.show_ext,
            settings_page: a.settings_page,
        },
        _ => Presentation::default(),
    }
}

/// Applies a later launch's request and brings the window forward with the
/// search field focused.
fn present(
    w: &AppWindow,
    engine: &Engine,
    pause: &Timer,
    prefs: &Rc<Prefs>,
    request: instance::Request,
) {
    let p = presentation(&request.args);
    if let Some(o) = p.options {
        set_options(w, o);
    }
    w.set_show_type(w.get_show_type() || p.show_type);
    w.set_show_ext(w.get_show_ext() || p.show_ext);
    w.invoke_focus_search();
    if let Some(query) = &p.query {
        pause.stop();
        w.set_query(query.as_str().into());
        w.invoke_place_search_caret(i32::try_from(query.len()).unwrap_or(i32::MAX));
        prefs.record(query);
        w.set_notice("".into());
    }
    if p.query.is_some() || p.options.is_some() {
        engine.set_query(&w.get_query(), options(w));
    }
    raise(w, request.activation_token);
    if let Some(page) = p.settings_page {
        prefs.open(Some(page));
    }
}

/// Brings the window forward.
///
/// On Wayland a client cannot raise its own window: winit's
/// `focus_window` does nothing there and unminimizing is ignored, and the
/// only way to use an activation token in winit 0.30 is at window creation,
/// which Slint does not expose. So the window is hidden and shown again:
/// Slint then creates a new surface at the same size, and GNOME gives a new
/// window the focus. The token from the later launch is not used yet.
fn raise(w: &AppWindow, _activation_token: Option<String>) {
    let _ = w.window().hide();
    let _ = w.window().show();
}

/// More items than this are not opened at once; it is almost always a slip.
const MAX_OPEN: usize = 20;
/// Moving more items than this to the trash asks first.
const TRASH_CONFIRM: usize = 50;

/// What the confirmation dialog will do when accepted.
enum Pending {
    Trash(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
}

/// Open, copy, rename, trash and delete.
fn connect_actions(window: &AppWindow, model: &Rc<ResultsModel>) {
    let pending: Rc<RefCell<Option<Pending>>> = Rc::default();
    let weak = window.as_weak();
    let row = |r: i32| usize::try_from(r).ok();

    window.on_activate({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let paths = model.targets(row(r));
            if let Some(w) = weak.upgrade() {
                w.invoke_history_record();
            }
            if paths.len() > MAX_OPEN {
                if let Some(w) = weak.upgrade() {
                    error(
                        &w,
                        format!("Select at most {MAX_OPEN} items to open them at once."),
                    );
                }
                return;
            }
            paths.iter().for_each(|p| open::open(p));
        }
    });
    window.on_open_folder({
        let model = model.clone();
        move |r| {
            let paths = model.targets(row(r));
            open::show_in_folder(&paths[..paths.len().min(MAX_OPEN)]);
        }
    });
    window.on_copy_names({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let paths = model.targets(row(r));
            if let (Some(w), false) = (weak.upgrade(), paths.is_empty()) {
                w.invoke_copy_text(actions::names_text(&paths).into());
                info(&w, copied(paths.len(), "name", "names"));
            }
        }
    });
    window.on_copy_paths({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let paths = model.targets(row(r));
            if let (Some(w), false) = (weak.upgrade(), paths.is_empty()) {
                w.invoke_copy_text(actions::paths_text(&paths).into());
                info(&w, copied(paths.len(), "path", "paths"));
            }
        }
    });

    window.on_rename_start({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let (Some(w), Some(r)) = (weak.upgrade(), row(r)) else {
                return;
            };
            if model.is_selected(r) && model.selected_count() > 1 {
                error(&w, "Select one item to rename it.".into());
                return;
            }
            let Some((name, is_dir)) = model.name(r) else {
                return;
            };
            model.set_renaming(model.id(r));
            w.set_rename_stem(actions::stem_len(&name, is_dir) as i32);
            w.set_rename_text(name.into());
            w.set_rename_row(r as i32);
            w.set_notice("".into());
        }
    });
    window.on_rename_commit({
        let (weak, model) = (weak.clone(), model.clone());
        move |text| {
            let Some(w) = weak.upgrade() else { return };
            let Some(old) = model.renaming_row().and_then(|r| model.path(r)) else {
                stop_renaming(&w, &model);
                return;
            };
            match actions::rename(&old, &text) {
                Ok(_) => {
                    stop_renaming(&w, &model);
                    w.set_notice("".into());
                }
                Err(e) => error(&w, e.to_string()),
            }
        }
    });
    window.on_rename_cancel({
        let (weak, model) = (weak.clone(), model.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                stop_renaming(&w, &model);
            }
        }
    });

    window.on_trash({
        let (weak, model, pending) = (weak.clone(), model.clone(), pending.clone());
        move |r| {
            let paths = model.targets(row(r));
            let Some(w) = weak.upgrade() else { return };
            if paths.len() > TRASH_CONFIRM {
                ask(
                    &w,
                    format!("Move {} to the trash?", actions::describe(&paths)),
                    "You can restore them from the trash.",
                    "Move to trash",
                );
                *pending.borrow_mut() = Some(Pending::Trash(paths));
            } else if !paths.is_empty() {
                run_trash(&w, paths);
            }
        }
    });
    window.on_delete_request({
        let (weak, model, pending) = (weak.clone(), model.clone(), pending.clone());
        move |r| {
            let paths = model.targets(row(r));
            let Some(w) = weak.upgrade() else { return };
            if paths.is_empty() {
                return;
            }
            ask(
                &w,
                format!("Delete {} permanently?", actions::describe(&paths)),
                "This cannot be undone. Folders are deleted with all their contents.",
                "Delete",
            );
            *pending.borrow_mut() = Some(Pending::Delete(paths));
        }
    });
    window.on_confirm_accepted({
        let (weak, pending) = (weak.clone(), pending.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_confirm_open(false);
            match pending.borrow_mut().take() {
                Some(Pending::Trash(paths)) => run_trash(&w, paths),
                Some(Pending::Delete(paths)) => run_delete(&w, paths),
                None => {}
            }
            w.invoke_focus_list();
        }
    });
    window.on_confirm_cancelled({
        let weak = weak.clone();
        move || {
            pending.borrow_mut().take();
            if let Some(w) = weak.upgrade() {
                w.set_confirm_open(false);
                w.invoke_focus_list();
            }
        }
    });
}

fn copied(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("Copied the {one}.")
    } else {
        format!("Copied {} {many}.", format::count(n))
    }
}

fn info(w: &AppWindow, text: String) {
    w.set_notice_error(false);
    w.set_notice(text.into());
}

fn error(w: &AppWindow, text: String) {
    w.set_notice_error(true);
    w.set_notice(text.into());
}

fn ask(w: &AppWindow, title: String, text: &str, action: &str) {
    w.set_confirm_title(title.into());
    w.set_confirm_text(text.into());
    w.set_confirm_action(action.into());
    w.set_confirm_open(true);
}

fn stop_renaming(w: &AppWindow, model: &ResultsModel) {
    model.set_renaming(None);
    w.set_rename_row(-1);
    w.invoke_focus_list();
}

/// Moves items to the trash on a helper thread and reports the outcome.
fn run_trash(w: &AppWindow, paths: Vec<PathBuf>) {
    let what = actions::describe(&paths);
    info(w, format!("Moving {what} to the trash…"));
    let weak = w.as_weak();
    std::thread::spawn(move || {
        let result = actions::trash(&paths);
        let _ = weak.upgrade_in_event_loop(move |w| match result {
            Ok(()) => info(&w, format!("Moved {what} to the trash.")),
            Err(e) => error(&w, e),
        });
    });
}

/// Deletes items on a helper thread and reports the outcome.
fn run_delete(w: &AppWindow, paths: Vec<PathBuf>) {
    let what = actions::describe(&paths);
    info(w, format!("Deleting {what}…"));
    let weak = w.as_weak();
    std::thread::spawn(move || {
        let failed = actions::delete_permanently(&paths);
        let _ = weak.upgrade_in_event_loop(move |w| match failed.as_slice() {
            [] => info(&w, format!("Deleted {what}.")),
            [(path, e), ..] => error(
                &w,
                format!(
                    "Could not delete {} of {}: {}: {e}.",
                    format::count(failed.len()),
                    format::count(paths.len()),
                    path.display()
                ),
            ),
        });
    });
}

/// Command line of the window.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    roots: Vec<PathBuf>,
    index_file: Option<PathBuf>,
    /// Words that are not options, joined by spaces: the initial query.
    query: String,
    /// Initial matching modes and filter.
    options: QueryOptions,
    /// Whether any of the filter and matching mode options was given.
    options_given: bool,
    /// Optional columns shown from the start.
    show_type: bool,
    show_ext: bool,
    /// Open a window of its own instead of handing over to a running one.
    new_instance: bool,
    /// Open the settings window at this page (see [`prefs::PAGES`]).
    settings_page: Option<usize>,
}

const USAGE: &str = "\
usage: everygrep-gui [options] [query words...]

Words after the options become the first query. After -- every argument is a
query word.

options:
  --root <path>          folder to index (repeatable, default: saved roots or $HOME)
  --index-file <path>    index location (default: ~/.cache/everygrep/index.bin)
  --filter <name>        start with a filter: all, audio, archive, code, document,
                         executable, folder, image, video
  --case                 match case exactly
  --whole-words          match whole words only
  --match-path           match the full path instead of the name
  --diacritics           match diacritics exactly (by default e also finds é)
  --columns <list>       show optional columns: type, extension (comma separated)
  --new-instance         open another window even if one is already open
  --settings[=<page>]    open the settings window too, at a page: indexing,
                         search, window, shortcut
  -h, --help             print this help
  -V, --version          print the version";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(Args),
    Help,
    Version,
}

/// `everygrep-gui [options] [query words...]`, see [`USAGE`].
/// Without roots the saved roots, or the home folder, are used. An unknown
/// option or value is an error, so a mistyped flag never opens a window on
/// the default roots; after `--` every argument is a query word.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut out = Args::default();
    let mut words: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--root" => out.roots.push(PathBuf::from(value()?)),
            "--index-file" => out.index_file = Some(PathBuf::from(value()?)),
            "--filter" => {
                let name = value()?;
                out.options.filter =
                    Filter::from_id(&name).ok_or_else(|| format!("unknown filter {name}"))?;
                out.options_given = true;
            }
            "--case" | "--whole-words" | "--match-path" | "--diacritics" => {
                let mode = match a.as_str() {
                    "--case" => &mut out.options.case_sensitive,
                    "--whole-words" => &mut out.options.whole_words,
                    "--match-path" => &mut out.options.match_path,
                    _ => &mut out.options.match_diacritics,
                };
                *mode = true;
                out.options_given = true;
            }
            "--new-instance" => out.new_instance = true,
            "--settings" => out.settings_page = Some(0),
            _ if a.starts_with("--settings=") => {
                let name = &a["--settings=".len()..];
                out.settings_page = Some(
                    prefs::page_index(name)
                        .ok_or_else(|| format!("unknown settings page {name}"))?,
                );
            }
            "--columns" => {
                for column in value()?.split(',') {
                    match column.trim() {
                        "type" => out.show_type = true,
                        "extension" => out.show_ext = true,
                        other => return Err(format!("unknown column {other}")),
                    }
                }
            }
            "--" => words.extend(args.by_ref()),
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => words.push(a),
        }
    }
    out.query = words.join(" ");
    Ok(Command::Run(out))
}

/// The sort for a header column of the window (0 name, 1 folder, 2 size,
/// 3 modified, 4 type, 5 extension). The folder column sorts by folder, then
/// name.
fn sort_for_column(column: i32, descending: bool) -> Sort {
    let key = match column {
        1 => SortKey::Path,
        2 => SortKey::Size,
        3 => SortKey::Modified,
        4 => SortKey::Type,
        5 => SortKey::Extension,
        _ => SortKey::Name,
    };
    Sort {
        key,
        descending,
        ..Sort::default()
    }
}

/// The matching modes and filter set in the window.
fn options(w: &AppWindow) -> QueryOptions {
    QueryOptions {
        case_sensitive: w.get_match_case(),
        whole_words: w.get_whole_words(),
        match_path: w.get_match_path(),
        match_diacritics: w.get_match_diacritics(),
        filter: usize::try_from(w.get_filter_index())
            .ok()
            .and_then(|i| Filter::ALL.get(i).copied())
            .unwrap_or_default(),
    }
}

/// Sets the window's toggles and filter selector.
fn set_options(w: &AppWindow, o: QueryOptions) {
    w.set_match_case(o.case_sensitive);
    w.set_whole_words(o.whole_words);
    w.set_match_path(o.match_path);
    w.set_match_diacritics(o.match_diacritics);
    let index = Filter::ALL.iter().position(|&f| f == o.filter).unwrap_or(0);
    w.set_filter_index(index as i32);
    show_badges(w);
}

fn show_badges(w: &AppWindow) {
    let badges: Vec<SharedString> = labels::mode_badges(&options(w))
        .into_iter()
        .map(SharedString::from)
        .collect();
    w.set_badges(ModelRc::new(VecModel::from(badges)));
}

fn with_model(w: &AppWindow, f: impl FnOnce(&ResultsModel)) {
    let results = w.get_results();
    if let Some(m) = results.as_any().downcast_ref::<ResultsModel>() {
        f(m);
    }
}

fn show_results(w: &AppWindow, result: SearchResult) {
    let count = result.ids.len();
    with_model(w, |m| {
        m.set_result(&result);
        // An item being renamed may have moved in the list, or be gone.
        let row = m.renaming_row();
        if row.is_none() {
            m.set_renaming(None);
        }
        w.set_rename_row(row.map_or(-1, |r| r as i32));
    });
    if count == 0 {
        w.set_current(-1);
    } else if w.get_current() < 0 || w.get_current() as usize >= count {
        w.set_current(0);
    }
    w.set_message(match result.error {
        Some(e) => format!("Cannot search: {e}").into(),
        None => "".into(),
    });
    refresh_status(w);
}

fn refresh_status(w: &AppWindow) {
    let mut message = None;
    with_model(w, |m| {
        let (left, right) = m.status();
        w.set_status_left(left.into());
        w.set_status_right(right.into());
        message = match m.state() {
            IndexState::Loading if m.row_count() == 0 => Some("Loading the index…"),
            IndexState::Scanning if m.row_count() == 0 => {
                Some("Indexing your files. This happens once; later starts are instant.")
            }
            _ => None,
        };
    });
    if let Some(text) = message {
        w.set_message(text.into());
    } else if w.get_message().starts_with("Loading") || w.get_message().starts_with("Indexing") {
        w.set_message("".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Command, String> {
        parse_args(list.iter().map(|s| (*s).to_owned()))
    }

    fn args(list: &[&str]) -> Args {
        match parse(list) {
            Ok(Command::Run(a)) => a,
            other => panic!("{list:?} gave {other:?}"),
        }
    }

    #[test]
    fn help_version_and_mistakes_never_open_a_window() {
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["a", "-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-V"]), Ok(Command::Version));
        // `--help` after `--` is a query word.
        assert_eq!(args(&["--", "--help"]).query, "--help");
        for wrong in [
            &["--fast"][..],
            &["--root"],
            &["--index-file"],
            &["--filter", "nope"],
            &["--filter"],
            &["--columns", "size"],
            &["--columns"],
        ] {
            assert!(parse(wrong).is_err(), "{wrong:?} was accepted");
        }
    }

    #[test]
    fn positional_words_form_the_initial_query() {
        let a = args(&[
            "--root",
            "/tmp/a",
            "report",
            "--index-file",
            "/tmp/i",
            "pdf",
        ]);
        assert_eq!(a.roots, [PathBuf::from("/tmp/a")]);
        assert_eq!(a.index_file, Some(PathBuf::from("/tmp/i")));
        assert_eq!(a.query, "report pdf");
        assert_eq!(args(&[]), Args::default());
        // After `--` options are words too.
        assert_eq!(args(&["a", "--", "--root", "b"]).query, "a --root b");
        assert!(args(&["--", "--root", "b"]).roots.is_empty());
        // Single-dash words such as `-draft` are query words.
        assert_eq!(args(&["-x", "ext:pdf"]).query, "-x ext:pdf");
    }

    #[test]
    fn matching_modes_filter_and_columns() {
        let a = args(&[
            "--filter",
            "image",
            "--case",
            "--diacritics",
            "--columns",
            "type, extension",
            "img",
        ]);
        assert_eq!(a.options.filter, Filter::Picture);
        assert!(a.options.case_sensitive && a.options.match_diacritics);
        assert!(!a.options.whole_words && !a.options.match_path);
        assert!(a.show_type && a.show_ext);
        assert_eq!(a.query, "img");
        let a = args(&["--whole-words", "--match-path"]);
        assert!(a.options.whole_words && a.options.match_path);
        assert_eq!(a.options.filter, Filter::All);
        assert!(!a.show_type && !a.show_ext);
    }

    fn present_args(list: &[&str]) -> Presentation {
        presentation(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn a_bare_later_launch_only_raises_the_window() {
        assert_eq!(present_args(&[]), Presentation::default());
        // Roots, index file and --new-instance are not the window's business.
        assert_eq!(
            present_args(&["--root", "/tmp", "--index-file", "/tmp/i", "--new-instance"]),
            Presentation::default()
        );
        // Arguments the launch would have refused change nothing.
        assert_eq!(present_args(&["--fast", "x"]), Presentation::default());
        assert_eq!(present_args(&["--help"]), Presentation::default());
    }

    #[test]
    fn later_launch_sets_query_modes_and_columns() {
        let p = present_args(&["report", "pdf"]);
        assert_eq!(p.query.as_deref(), Some("report pdf"));
        assert_eq!(p.options, None);
        let p = present_args(&["--filter", "image", "--case", "--columns", "type"]);
        assert_eq!(p.query, None);
        let o = p.options.expect("modes given");
        assert_eq!(o.filter, Filter::Picture);
        assert!(o.case_sensitive && !o.whole_words);
        assert!(p.show_type && !p.show_ext);
        // `--filter all` is a choice too: it resets the filter.
        assert_eq!(
            present_args(&["--filter", "all"]).options,
            Some(QueryOptions::default())
        );
        for flag in ["--whole-words", "--match-path", "--diacritics"] {
            assert!(present_args(&[flag]).options.is_some(), "{flag}");
        }
    }

    #[test]
    fn settings_flag_opens_a_page() {
        assert_eq!(args(&["x"]).settings_page, None);
        assert_eq!(args(&["--settings"]).settings_page, Some(0));
        assert_eq!(args(&["--settings=window"]).settings_page, Some(2));
        assert!(parse(&["--settings=nope"]).is_err());
        assert_eq!(present_args(&["--settings=search"]).settings_page, Some(1));
        assert_eq!(args(&["--", "--settings"]).query, "--settings");
    }

    #[test]
    fn new_instance_flag() {
        assert!(args(&["--new-instance", "x"]).new_instance);
        assert!(!args(&["x"]).new_instance);
        assert_eq!(args(&["--", "--new-instance"]).query, "--new-instance");
    }

    #[test]
    fn header_columns_map_to_sort_keys() {
        let keys: Vec<SortKey> = (0..6).map(|c| sort_for_column(c, false).key).collect();
        assert_eq!(
            keys,
            [
                SortKey::Name,
                SortKey::Path,
                SortKey::Size,
                SortKey::Modified,
                SortKey::Type,
                SortKey::Extension
            ]
        );
        assert!(sort_for_column(2, true).descending);
        assert_eq!(sort_for_column(9, false).key, SortKey::Name);
    }
}
