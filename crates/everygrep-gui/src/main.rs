//! EveryGrep desktop window.

mod actions;
mod advanced;
mod advanced_query;
mod autostart;
mod clipboard;
mod columns;
mod commands;
mod content_settings;
mod diagnostics;
mod engine;
mod export;
mod fileops_conflict;
mod fileops_rename;
mod fileops_search;
mod fileops_settings;
mod fileops_shortcut;
mod fileops_undo;
mod fileops_undo_ui;
mod fileops_value;
mod filetype;
mod folders;
mod format;
mod hash_columns;
mod hashes;
mod help;
mod history;
mod history_csv;
mod history_sync;
mod i18n;
mod instance;
mod jump;
mod labels;
mod look;
mod menus;
mod open;
mod portal;
mod prefs;
mod properties;
mod property_settings;
mod results;
mod saved_searches;
mod saved_ui;
mod schedule;
mod search_errors;
mod search_history;
mod selection;
mod session;
mod settings;
mod settings_bundle;
mod sort_dialog;
mod stalled;
mod tabs;
mod tabs_ui;
mod transfer;
mod typing;
mod x11clip;

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, QueryOptions};
use everygrep_core::sort::{FolderSize, Sort, SortKey};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use engine::{Engine, Hooks, IndexState, Notice, SearchResult, Stopped};
use i18n::{fill, tr, trn};
use prefs::{IndexSetup, Prefs};
use results::{DetailColumns, Offline, ResultsModel, Targets};
use settings::{Density, ThemeChoice};

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
    let raw_os: Vec<OsString> = std::env::args_os().skip(1).collect();
    // A running window takes over the query words and options only (its
    // folders are those of its bus name), so the text form is enough.
    let raw: Vec<String> = raw_os
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let args = match parse_args(raw_os) {
        Ok(Cli::Run(args)) => args,
        Ok(Cli::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(Cli::Version) => {
            println!("everygrep-gui {}", everygrep_core::VERSION);
            return Ok(());
        }
        Err(e) => {
            eprintln!("everygrep-gui: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    // Read first: a dialog about a stalled window (below) is shown in the
    // chosen language.
    let (settings_path, settings, settings_error) = prefs::load();
    commands::set_current(commands::Keymap::with_overrides(&settings.shortcuts));
    let (session_roots, session_file) = instance::identity(&args.roots, args.index_file.as_deref());
    let session_start = session::Start {
        scope: instance::bus_name(
            &session_roots,
            session_file.as_deref(),
            instance::default_index().as_deref(),
        ),
        restore: session::should_restore(
            session::can_persist(
                settings.restore_session,
                settings.history_enabled && settings.history_size > 0,
                settings.history_on_disk,
            ),
            args.options_given || !args.query.is_empty(),
            args.new_instance || args.new_window,
        ),
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
        let launch = if args.background {
            instance::Launch::Background
        } else if args.new_window {
            instance::Launch::NewWindow
        } else if args.toggle {
            instance::Launch::Toggle
        } else {
            instance::Launch::Show
        };
        let mut role = instance::start(&name, &raw, launch, Box::new(queue_present));
        loop {
            role = match role {
                instance::Role::Primary(connection) => break Some(connection),
                instance::Role::HandedOver => return Ok(()),
                instance::Role::Alone => break None,
                instance::Role::Stalled(s) => {
                    eprintln!("everygrep-gui: {}", stalled::describe(s.stall));
                    match stalled::resolve(s)? {
                        Some(next) => next,
                        None => return Ok(()),
                    }
                }
            }
        }
    };
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
        on_busy: Box::new(|| {
            let _ = slint::invoke_from_event_loop(run_on_busy);
        }),
        on_stopped: Box::new({
            let weak = weak.clone();
            move |stopped: Stopped| {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        show_stopped(&w, &stopped);
                    }
                    if stopped == Stopped::Rescan {
                        prefs::with(|p| p.rescan_stopped());
                    }
                });
            }
        }),
        on_notice: Box::new({
            let weak = weak.clone();
            move |notice: Notice| {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        info(&w, labels::notice(&notice));
                    }
                });
            }
        }),
    };
    let window_args = new_window_args(&args.roots, args.index_file.as_deref());
    let drives =
        everygrep_core::drives::DriveEnv::from_env().map(|env| (env, settings.drives.clone()));
    let engine = Engine::start(
        setup.roots.clone(),
        everygrep_core::scan::ScanOptions {
            rules: setup.rules(),
            with_metadata: settings.index_file_details,
        },
        settings.watch_changes,
        // The window over the default index keeps it where the settings say.
        if setup.saved {
            settings.index_file()
        } else {
            args.index_file
        },
        drives,
        hooks,
    );
    engine.set_content_options(settings.content.options());
    engine
        .hashes
        .set_max_bytes(settings.content.hash_cache_bytes());
    engine.set_property_options(settings.properties.clone());
    properties::start_indexer(engine.clone());
    let model = Rc::new(ResultsModel::new(engine.clone()));
    model.set_show_root_labels(settings.drive_root_labels);
    // Created, Accessed and Attributes are read off the window's thread.
    model.read_details_with(results::DetailReader::start({
        let weak = window.as_weak();
        move |batch| {
            let _ = weak.upgrade_in_event_loop(move |w| with_model(&w, |m| m.take_details(batch)));
        }
    }));
    // So are the hash columns.
    model.read_hashes_with(hash_columns::HashReader::start(engine.hashes.clone(), {
        let weak = window.as_weak();
        move |batch| {
            let _ = weak.upgrade_in_event_loop(move |w| with_model(&w, |m| m.take_hashes(batch)));
        }
    }));
    model.read_properties_with(properties::Reader::start(engine.clone(), {
        let weak = window.as_weak();
        move |batch| {
            let _ =
                weak.upgrade_in_event_loop(move |w| with_model(&w, |m| m.take_properties(batch)));
        }
    }));
    window.on_property_view_changed({
        let model = model.clone();
        move || model.viewport_changed()
    });
    window.set_results(ModelRc::from(model.clone()));
    let progress_timer = Rc::new(Timer::default());
    let shown_task = Rc::new(Cell::new(None));
    ON_BUSY.with(|f| {
        let (weak, engine) = (weak.clone(), engine.clone());
        let (timer, shown) = (progress_timer.clone(), shown_task.clone());
        *f.borrow_mut() = Some(Box::new(move || {
            watch_progress(&weak, &engine, &timer, &shown);
        }));
    });
    // Tasks may have begun before the event loop could hear of them.
    run_on_busy();
    window.on_cancel_task({
        let (engine, shown) = (engine.clone(), shown_task.clone());
        move || {
            if let Some(id) = shown.get() {
                engine.cancel(id);
            }
        }
    });
    saved_ui::connect(&window, &engine);
    advanced::connect(&window);
    set_filter_names(&window);
    set_options(&window, start_options);
    window.set_show_type(args.show_type);
    window.set_show_ext(args.show_ext);
    window.set_show_created(args.show_created);
    window.set_show_accessed(args.show_accessed);
    window.set_show_attrs(args.show_attrs);
    let folder_size = settings.folder_size;
    if folder_size != FolderSize::default() {
        set_folder_size(&window, &engine, folder_size);
    }
    let prefs = Prefs::install(
        settings_path,
        settings,
        settings_error,
        setup,
        engine.clone(),
        &window,
    );
    prefs.restore_columns(&window);
    apply_detail_columns(&window);
    window.on_detail_columns_changed({
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                apply_detail_columns(&w);
            }
        }
    });
    prefs.apply_look();
    prefs.apply_as_you_type();
    prefs.apply_behaviour();
    prefs.apply_watch_changes();
    prefs.apply_schedule();
    // Follows the desktop's light or dark setting for the System theme.
    std::thread::spawn(|| {
        let _ = portal::watch_color_scheme(|scheme| {
            let _ = slint::invoke_from_event_loop(move || {
                prefs::with(|p| p.set_desktop_scheme(scheme));
            });
        });
    });
    refresh_status(&window);
    // The search thread runs the query now and again once the index is
    // loaded, so the first results appear as soon as they can.
    let initial_options = options(&window);
    if !args.query.is_empty() || initial_options != QueryOptions::default() {
        window.set_query(args.query.as_str().into());
        run_query(&window, &engine);
        prefs.record(&args.query);
    }

    let pause = Rc::new(Timer::default());
    folders::connect(&window, &engine, window_args.clone());
    tabs_ui::connect(&window, &engine, &model, &prefs, &pause, session_start);
    let query_edit = |manual: bool| {
        let (engine, weak, pause, prefs) =
            (engine.clone(), weak.clone(), pause.clone(), prefs.clone());
        move |text: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            pause.stop();
            with_model(&w, |m| m.forget_properties());
            if manual {
                prefs.restore_parse_options(&w);
            }
            prefs.fill_history();
            let reads = field_reads_files(&w);
            if !typing::with(|t| t.edited(reads)) {
                wait_for_enter(&w);
                return;
            }
            run_query(&w, &engine);
            info(&w, String::new());
            let epoch = prefs.history_epoch.get();
            let prefs = prefs.clone();
            pause.start(TimerMode::SingleShot, HISTORY_PAUSE, move || {
                if prefs.history_epoch.get() == epoch {
                    prefs.record(&text);
                }
            });
        }
    };
    window.on_query_edited(query_edit(true));
    window.on_split_location(|text| {
        let Some(found) = fileops_search::location(&text) else {
            return LocationSplit {
                rest: text,
                ..Default::default()
            };
        };
        let rest = &text[found.len..];
        LocationSplit {
            label: found.label().into(),
            detail: found.display().into(),
            scope: text[..found.len].into(),
            scope_bytes: i32::try_from(found.len).unwrap_or(i32::MAX),
            rest: rest.strip_prefix(' ').unwrap_or(rest).into(),
        }
    });
    window.on_generated_query_edited(query_edit(false));
    window.on_refresh({
        let engine = engine.clone();
        move || {
            engine.refresh();
        }
    });
    window.on_choose_hidden_files(|i| prefs::with(|p| p.choose_hidden_files(i)));
    window.on_options_changed({
        let (engine, weak) = (engine.clone(), weak.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                let reads = field_reads_files(&w);
                if typing::with(|t| t.edited(reads)) {
                    run_query(&w, &engine);
                } else {
                    wait_for_enter(&w);
                }
                show_badges(&w);
            }
        }
    });
    window.on_search_now({
        let (engine, weak, pause) = (engine.clone(), weak.clone(), pause.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            if typing::with(|t| t.enter()) == typing::Enter::Search {
                pause.stop();
                search_now(&w, &engine);
            }
        }
    });
    window.on_organize_history({
        let prefs = prefs.clone();
        move || prefs.open_history()
    });
    window.on_history_list_requested({
        let prefs = prefs.clone();
        move || {
            prefs.change_searches(|_| {});
        }
    });
    window.on_history_sort_changed({
        let prefs = prefs.clone();
        move |i| prefs.set_history_sort(i)
    });
    window.on_history_picked({
        let (weak, engine, pause) = (weak.clone(), engine.clone(), pause.clone());
        move |text| {
            pause.stop();
            let Some(w) = weak.upgrade() else { return };
            w.set_query(text.clone());
            w.invoke_place_search_caret(i32::try_from(text.len()).unwrap_or(i32::MAX));
            typing::with(|t| t.searched());
            search_now(&w, &engine);
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
            prefs.record(&shown);
            let next = {
                let mut history = prefs.history.borrow_mut();
                if step < 0 {
                    history.back(&shown)
                } else {
                    history.forward(&shown)
                }
                .map(str::to_owned)
            };
            if let Some(text) = next {
                w.set_query(text.as_str().into());
                // Setting the text keeps the old cursor offset; put the caret
                // at the end of the restored query instead.
                w.invoke_place_search_caret(i32::try_from(text.len()).unwrap_or(i32::MAX));
                run_query(&w, &engine);
                typing::with(|t| t.searched());
                w.set_search_pending(false);
                info(&w, String::new());
            }
        }
    });
    window.on_dupe_term({
        let (engine, weak, pause) = (engine.clone(), weak.clone(), pause.clone());
        move |kind| {
            let Some(w) = weak.upgrade() else { return };
            pause.stop();
            let term = match kind {
                0 => Some("duplicates:"),
                1 => Some("duplicates:name"),
                2 => Some("duplicates:size"),
                _ => None,
            };
            let text = everygrep_core::dupe::replace_term(&w.get_query(), term);
            w.set_query(text.as_str().into());
            w.invoke_place_search_caret(i32::try_from(text.len()).unwrap_or(i32::MAX));
            run_query(&w, &engine);
            typing::with(|t| t.searched());
            w.set_search_pending(false);
            info(&w, String::new());
        }
    });
    window.on_open_settings({
        let prefs = prefs.clone();
        move || prefs.open(None)
    });
    window.on_open_keyboard_settings({
        let prefs = prefs.clone();
        move || prefs.open(Some(prefs::KEYBOARD_PAGE))
    });
    window.on_close_window({
        let (weak, engine, prefs) = (weak.clone(), engine.clone(), prefs.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                close_window(&w, &engine, &prefs, prefs.hide_on_close());
            }
        }
    });
    fileops_search::connect(&window, &model, &engine, window_args.clone());
    window.on_new_window(move || {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).args(&window_args).spawn();
        }
    });
    connect_menus(&window);
    window.on_quit({
        let (weak, prefs) = (weak.clone(), prefs.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                prefs.save_columns(&w);
            }
            // From now on no request is taken (the loop ends after this turn).
            instance::set_closing();
            let _ = slint::quit_event_loop();
        }
    });
    window.on_advanced_sort_opening({
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                sort_dialog::opening(&w);
            }
        }
    });
    window.on_sort_changed({
        let (engine, model, weak) = (engine.clone(), model.clone(), weak.clone());
        move |column, descending| {
            let folders = model.folder_size();
            engine.set_sort(match weak.upgrade() {
                Some(w) => sort_dialog::window_sort(&w, folders),
                None => sort_for_column(column, descending, folders),
            });
            tabs_ui::submitted_sort(column, descending);
        }
    });
    window.on_drag_columns(columns::drag_in_window);
    window.on_filter_columns(columns::filter_choices);
    window.on_fit_texts({
        let model = model.clone();
        move |column, first, count| {
            let first = usize::try_from(first).unwrap_or(0);
            let count = usize::try_from(count).unwrap_or(0);
            let texts = model.column_texts(column, first..first.saturating_add(count));
            let mut widest = results::widest(texts, 3).into_iter();
            let mut next = || SharedString::from(widest.next().unwrap_or_default());
            FitTexts {
                a: next(),
                b: next(),
                c: next(),
            }
        }
    });
    window.on_first_descending(|column| {
        let mut descending = false;
        prefs::with(|p| descending = p.first_descending(column));
        descending
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
    window.on_invert_selection({
        let weak = weak.clone();
        let model = model.clone();
        move || {
            model.invert_selection();
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
    fileops_undo_ui::connect(&window, &engine);
    connect_clipboard(&window, &model, &engine);
    connect_export(&window, &model, &engine);
    connect_troubleshooting(&window, &engine);
    connect_settings_file(&window);
    connect_jump(&window, &model);
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
                close_window(&w, &engine, &prefs, prefs.hide_on_close());
            }
            slint::CloseRequestResponse::HideWindow
        }
    });
    // Started on login with closing set to hide: the window is shown by
    // the next launch. Without the bus name no launch could reach it, and
    // with closing set to quit hiding makes no sense, so it is shown.
    let hidden = args.background && _instance.is_some() && prefs.hide_on_close();
    if !hidden {
        window.show()?;
    }
    if let Some(page) = args.settings_page {
        // Opened once the main window is on screen, so the settings window
        // is mapped after it and stays in front.
        let prefs = prefs.clone();
        Timer::single_shot(SETTINGS_DELAY, move || prefs.open(Some(page)));
    }
    slint::run_event_loop_until_quit()?;
    // Later launches wait for this process to end instead of handing over;
    // those whose request was not run are told so.
    instance::set_closing();
    let left = std::mem::take(&mut *PRESENTS.lock().expect("lock"));
    left.into_iter().for_each(instance::Request::refuse);
    // Gives the clipboard up and closes the XWayland connection.
    X11.with(|x| x.borrow_mut().take());
    tabs_ui::flush();
    window.hide()?;
    // Settings and history changed last (columns, searches) are on disk
    // before the program ends, unless the disk does not answer.
    prefs.finish_saving(engine::QUIT_WAIT);
    engine.shutdown();
    Ok(())
}

/// Arguments that open another window over the same index.
fn new_window_args(roots: &[PathBuf], index_file: Option<&std::path::Path>) -> Vec<OsString> {
    let mut out = vec![OsString::from("--new-instance")];
    for root in roots {
        out.push("--root".into());
        out.push(root.into());
    }
    if let Some(file) = index_file {
        out.push("--index-file".into());
        out.push(file.into());
    }
    out
}

fn set_filter_names(w: &AppWindow) {
    saved_ui::fill_filters(w);
}

/// Gives the window the keys in use: the menu bar's keys, the ⋯ button's
/// label and the keyboard shortcuts dialog.
pub fn apply_keys(w: &AppWindow) {
    let (menu_keys, more) = commands::with_current(|k| {
        let items = [
            Command::Settings,
            Command::KeyboardSettings,
            Command::SearchSyntaxHelp,
            Command::ShortcutsHelp,
            Command::About,
            Command::MenuBar,
            Command::Quit,
        ]
        .map(|c| k.labelled(c));
        (k.menu_keys(), fill(tr("More: {}"), &[&items.join(", ")]))
    });
    w.set_menu_keys(menu_keys);
    w.set_more_label(more.into());
    set_shortcuts(w);
}

/// The keyboard shortcuts dialog, from the keys in use.
fn set_shortcuts(w: &AppWindow) {
    let mut shortcuts: Vec<ShortcutRow> = commands::with_current(|k| k.help_rows())
        .into_iter()
        .map(|(keys, action)| ShortcutRow {
            keys: keys.into(),
            action: action.into(),
        })
        .collect();
    shortcuts.extend(saved_ui::shortcut_rows());
    w.set_shortcuts(ModelRc::new(VecModel::from(shortcuts)));
}

/// The pages of the help window, in the current language.
fn set_help(w: &AppWindow) {
    let model = |rows: Vec<help::Row>| {
        let rows: Vec<HelpRow> = rows
            .into_iter()
            .map(|r| HelpRow {
                example: r.example.into(),
                meaning: r.meaning.into(),
            })
            .collect();
        ModelRc::new(VecModel::from(rows))
    };
    w.set_help_syntax(model(help::syntax()));
    w.set_help_regex(model(help::regex()));
    w.set_help_command_line(model(help::command_line()));
}

/// Sets the texts made in Rust again, after the language changed. The
/// `.slint` texts follow by themselves; a note in the status line is
/// dropped rather than left in the old language.
pub fn relabel(w: &AppWindow) {
    tabs_ui::refresh();
    set_filter_names(w);
    apply_keys(w);
    set_help(w);
    show_badges(w);
    with_model(w, |m| m.relabel());
    info(w, String::new());
    refresh_status(w);
}

/// The View menu's appearance choices and the Help menu's dialogs.
fn connect_menus(w: &AppWindow) {
    w.set_version(everygrep_core::VERSION.into());
    apply_keys(w);
    set_help(w);
    w.on_resolve_key(|scope, text, ctrl, alt, shift, meta| {
        let chord = commands::Chord::from_event(&text, ctrl, alt, shift, meta);
        match (commands::Scope::from_index(scope), chord) {
            (Some(scope), Some(chord)) => {
                commands::with_current(|k| k.resolve(scope, &chord)).unwrap_or(Command::None)
            }
            _ => Command::None,
        }
    });
    let pick = |all_len: usize, i: i32| usize::try_from(i).ok().filter(|&i| i < all_len);
    w.on_choose_theme(move |i| {
        let i = pick(ThemeChoice::ALL.len(), i);
        prefs::with(|p| match i {
            Some(i) => p.set_theme(ThemeChoice::ALL[i]),
            None => p.fill_menus(),
        });
    });
    w.on_choose_density(move |i| {
        let i = pick(Density::ALL.len(), i);
        prefs::with(|p| match i {
            Some(i) => p.set_density(Density::ALL[i]),
            None => p.fill_menus(),
        });
    });
    w.on_choose_folder_size(move |i| {
        let i = pick(FolderSize::ALL.len(), i);
        prefs::with(|p| match i {
            Some(i) => p.set_folder_size(FolderSize::ALL[i]),
            None => p.fill_menus(),
        });
    });
    w.on_status_details_changed(|on| prefs::with(|p| p.set_status_details(on)));
    w.on_rebuild_index(|| prefs::with(|p| p.rebuild()));
    w.on_index_paused_changed(|paused| prefs::with(|p| p.set_watch_changes(!paused)));
    w.on_menu_bar_changed(|on| prefs::with(|p| p.set_menu_bar(on)));
}

/// Requests from later launches, waiting for the UI thread.
static PRESENTS: Mutex<Vec<instance::Request>> = Mutex::new(Vec::new());

/// Applies a later launch's request to the window.
type Presenter = Box<dyn Fn(instance::Request)>;

thread_local! {
    /// Set once the window is ready.
    static ON_PRESENT: RefCell<Option<Presenter>> = RefCell::default();
}

thread_local! {
    /// Starts showing the long operations; set once the window is ready.
    static ON_BUSY: RefCell<Option<Box<dyn Fn()>>> = RefCell::default();
}

/// Runs on the UI thread when a long operation begins.
fn run_on_busy() {
    ON_BUSY.with(|f| {
        if let Some(f) = &*f.borrow() {
            f();
        }
    });
}

/// Shows the long operation in the status line, every
/// [`engine::PROGRESS_EVERY`] until none runs. `shown` keeps the one shown,
/// for its Cancel button.
fn watch_progress(
    weak: &slint::Weak<AppWindow>,
    engine: &Arc<Engine>,
    timer: &Rc<Timer>,
    shown: &Rc<Cell<Option<u64>>>,
) {
    if timer.running() {
        return;
    }
    let tick = {
        let (weak, engine, shown) = (weak.clone(), engine.clone(), shown.clone());
        let timer: Weak<Timer> = Rc::downgrade(timer);
        move || {
            let Some(w) = weak.upgrade() else { return };
            let busy = engine.busy();
            shown.set(busy.as_ref().map(|b| b.id));
            let text = busy.as_ref().map(labels::busy).unwrap_or_default();
            w.set_progress_text(text.into());
            w.set_can_cancel(busy.as_ref().is_some_and(|b| b.cancellable));
            if busy.is_none() && !engine.is_busy() {
                if let Some(t) = timer.upgrade() {
                    t.stop();
                }
            }
        }
    };
    timer.start(TimerMode::Repeated, engine::PROGRESS_EVERY, tick);
}

/// Says in the status line what a cancel kept; a cancelled sort puts the
/// column header back to the order shown, or to a newer order asked for
/// before this was handled.
fn show_stopped(w: &AppWindow, stopped: &Stopped) {
    if let Stopped::Sort(mut sort) = *stopped {
        with_model(w, |m| sort = m.asked_sort());
        w.set_sort_column(column_of(sort.key));
        w.set_sort_descending(sort.descending);
    }
    info(w, labels::stopped(stopped));
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
    show_created: bool,
    show_accessed: bool,
    show_attrs: bool,
}

/// The changes a later launch's arguments ask for. Arguments that do not
/// parse (they were checked by the launch already) change nothing.
fn presentation(args: &[String], defaults: QueryOptions) -> Presentation {
    match parse_args_with_options(args.iter().map(OsString::from), defaults) {
        Ok(Cli::Run(a)) => Presentation {
            query: (!a.query.is_empty()).then_some(a.query),
            options: a.options_given.then_some(a.options),
            show_type: a.show_type,
            show_ext: a.show_ext,
            show_created: a.show_created,
            show_accessed: a.show_accessed,
            show_attrs: a.show_attrs,
            settings_page: a.settings_page,
        },
        _ => Presentation::default(),
    }
}

/// Closes the main window: hides it (`hide`) or quits. The columns are
/// saved either way.
fn close_window(w: &AppWindow, engine: &Arc<Engine>, prefs: &Prefs, hide: bool) {
    tabs_ui::close_picker();
    prefs.save_columns(w);
    if hide {
        let _ = w.window().hide();
        // Keep the index on disk current in case the session ends while
        // the window is hidden.
        let engine = engine.clone();
        std::thread::spawn(move || engine.save());
    } else {
        // From now on no request is taken (the loop ends after this turn).
        instance::set_closing();
        let _ = slint::quit_event_loop();
    }
}

/// What a later launch does with the window.
#[derive(Debug, PartialEq, Eq)]
enum PresentAction {
    /// Apply the launch's arguments and bring the window forward.
    Show,
    /// Hide the window, as closing it does when it only hides.
    Hide,
    /// End the program, as closing the window does by default.
    Quit,
}

/// A toggle closes a window that is shown and has the keyboard focus, the
/// way closing it would; anything else (a plain launch, a hidden window,
/// a window behind others) shows it.
fn present_action(
    toggle: bool,
    visible: bool,
    focused: bool,
    hide_on_close: bool,
) -> PresentAction {
    match (toggle && visible && focused, hide_on_close) {
        (false, _) => PresentAction::Show,
        (true, true) => PresentAction::Hide,
        (true, false) => PresentAction::Quit,
    }
}

/// Whether the main window has the keyboard focus, judged by the search
/// field and the list (Slint 1.18 does not tell whether a window is active;
/// the focused item loses the focus when another window gets it).
fn has_focus(w: &AppWindow) -> bool {
    w.get_typing_here()
}

/// Applies a later launch's request and brings the window forward, with the
/// search field focused unless the settings say otherwise; a toggle closes
/// a window that is in front instead.
fn present(
    w: &AppWindow,
    engine: &Arc<Engine>,
    pause: &Timer,
    prefs: &Rc<Prefs>,
    request: instance::Request,
) {
    let action = present_action(
        request.toggle,
        w.window().is_visible(),
        has_focus(w),
        prefs.hide_on_close(),
    );
    match action {
        PresentAction::Show => {}
        PresentAction::Hide => return close_window(w, engine, prefs, true),
        PresentAction::Quit => return close_window(w, engine, prefs, false),
    }
    let p = presentation(&request.args, prefs.search_policy());
    if p.query.is_some() {
        info(w, String::new());
    }
    if prefs.open_searches_in_tabs() && p.query.is_some() {
        tabs_ui::open_search(
            p.query.as_deref().unwrap_or(""),
            p.options.unwrap_or_else(|| options(w)),
            p.options.is_none(),
            &settings::Columns {
                show_type: p.show_type,
                show_ext: p.show_ext,
                show_created: p.show_created,
                show_accessed: p.show_accessed,
                show_attrs: p.show_attrs,
                ..Default::default()
            },
        );
        prefs.record(p.query.as_deref().unwrap_or(""));
        if prefs.focus_on_show() && !w.get_confirm_open() && !w.get_fileops_rename_open() {
            w.invoke_focus_search();
        }
        raise(w, request.activation_token);
        if let Some(page) = p.settings_page {
            prefs.open(Some(page));
        }
        return;
    }
    if let Some(o) = p.options {
        prefs.restore_search_policy(w);
        set_options(w, o);
    }
    w.set_show_type(w.get_show_type() || p.show_type);
    w.set_show_ext(w.get_show_ext() || p.show_ext);
    w.set_show_created(w.get_show_created() || p.show_created);
    w.set_show_accessed(w.get_show_accessed() || p.show_accessed);
    w.set_show_attrs(w.get_show_attrs() || p.show_attrs);
    apply_detail_columns(w);
    if prefs.focus_on_show() {
        w.invoke_focus_search();
    }
    if let Some(query) = &p.query {
        pause.stop();
        w.set_query(query.as_str().into());
        w.invoke_place_search_caret(i32::try_from(query.len()).unwrap_or(i32::MAX));
        prefs.record(query);
    }
    if p.query.is_some() || p.options.is_some() {
        run_query(w, engine);
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

/// Items shown at most at once in the file manager by Open containing
/// folder (one call of `ShowItems`).
const MAX_OPEN: usize = 20;
/// Moving more items than this to the trash asks first.
const TRASH_CONFIRM: usize = 50;

/// What the confirmation dialog will do when accepted. The items are
/// checked again then (see [`ResultsModel::recheck`]).
enum Pending {
    Open(Targets),
    Trash(Targets),
    Delete(Targets),
}

/// Open, copy, rename, trash and delete.
fn connect_actions(window: &AppWindow, model: &Rc<ResultsModel>) {
    let rename_dialog = fileops_rename::Dialog::connect(window);
    fileops_conflict::connect(window);
    fileops_shortcut::connect(window, model);
    let pending: Rc<RefCell<Option<Pending>>> = Rc::default();
    let weak = window.as_weak();
    let row = |r: i32| usize::try_from(r).ok();

    window.on_activate({
        let (weak, model, pending) = (weak.clone(), model.clone(), pending.clone());
        move |r| {
            let Some(targets) = online_targets(&weak, &model, row(r)) else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            w.invoke_history_record();
            let mut over = settings::DEFAULT_OPEN_CONFIRM;
            prefs::with(|p| over = p.open_confirm_over());
            let count = targets.paths.len();
            if actions::open_needs_confirm(count, over) {
                ask(
                    &w,
                    fill(
                        trn("Open {} item?", "Open {} items?", count),
                        &[&format::count(count)],
                    ),
                    tr("Each one opens in its own application."),
                    tr("Open"),
                );
                *pending.borrow_mut() = Some(Pending::Open(targets));
                return;
            }
            targets.paths.iter().for_each(|p| open::open(p));
        }
    });
    window.on_open_with({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let Some(Targets { paths, .. }) = online_targets(&weak, &model, row(r)) else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            let [path] = paths.as_slice() else {
                if !paths.is_empty() {
                    info(
                        &w,
                        tr("Select one item to open it with another application.").into(),
                    );
                }
                return;
            };
            w.invoke_history_record();
            let (path, weak) = (path.clone(), weak.clone());
            // The chooser is the portal's; waiting for it blocks.
            std::thread::spawn(move || {
                if let Err(e) = portal::open_with(&path) {
                    let text = fill(tr("Cannot show the application chooser: {}"), &[&e]);
                    let _ = weak.upgrade_in_event_loop(move |w| error(&w, text));
                }
            });
        }
    });
    window.on_open_folder({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let Some(Targets { paths, .. }) = online_targets(&weak, &model, row(r)) else {
                return;
            };
            open::show_in_folder(&paths[..paths.len().min(MAX_OPEN)]);
        }
    });
    window.on_copy_names({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let paths = model.targets(row(r));
            if let (Some(w), false) = (weak.upgrade(), paths.is_empty()) {
                w.invoke_copy_text(actions::names_text(&paths).into());
                info(&w, labels::copied_names(paths.len()));
            }
        }
    });
    window.on_copy_paths({
        let (weak, model) = (weak.clone(), model.clone());
        move |r, quoted| {
            let paths = model.targets(row(r));
            if let (Some(w), false) = (weak.upgrade(), paths.is_empty()) {
                let mut quote = quoted;
                prefs::with(|p| quote |= p.copy_full_path_quotes());
                let text = actions::paths_text(&paths, quote);
                w.invoke_copy_text(text.into());
                info(&w, labels::copied_paths(paths.len()));
            }
        }
    });
    window.on_copy_folders({
        let (weak, model) = (weak.clone(), model.clone());
        move |r, quoted| {
            let paths = model.targets(row(r));
            let count = actions::folders(&paths).len();
            if let (Some(w), true) = (weak.upgrade(), count > 0) {
                let text = actions::folders_text(&paths, quoted);
                w.invoke_copy_text(text.as_str().into());
                info(&w, labels::copied_folders(count));
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
                info(&w, tr("Select one item to rename it.").into());
                return;
            }
            if model.targets_offline(Some(r)) {
                info(&w, labels::offline_refused().into());
                return;
            }
            let Some((name, is_dir)) = model.name(r) else {
                return;
            };
            if let Some(path) = model.path(r) {
                if rename_dialog.start(&w, path, &name, is_dir) {
                    return;
                }
            }
            model.set_renaming(model.id(r));
            let mut name_only = true;
            prefs::with(|p| name_only = p.rename_name_only());
            w.set_rename_stem(actions::rename_selection(&name, is_dir, name_only) as i32);
            w.set_rename_text(name.into());
            w.set_rename_row(r as i32);
            info(&w, String::new());
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
            if w.get_rename_busy() {
                return;
            }
            w.set_rename_busy(true);
            let weak = w.as_weak();
            std::thread::spawn(move || {
                let result = fileops_undo::catch_worker(|| fileops_undo::rename(&old, &text))
                    .unwrap_or_else(|e| Err(actions::RenameError::Io(std::io::Error::other(e))));
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.set_rename_busy(false);
                    match result {
                        Ok(note) => {
                            w.invoke_rename_cancel();
                            match note {
                                Some(note) => info(&w, note),
                                None => info(&w, String::new()),
                            }
                        }
                        Err(e) => error(&w, e.to_string()),
                    }
                    fileops_undo_ui::refresh_open();
                });
            });
        }
    });
    window.on_rename_cancel({
        let (weak, model) = (weak.clone(), model.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                if !w.get_rename_busy() {
                    stop_renaming(&w, &model);
                }
            }
        }
    });

    window.on_trash({
        let (weak, model, pending) = (weak.clone(), model.clone(), pending.clone());
        move |r| {
            let Some(targets) = online_targets(&weak, &model, row(r)) else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            if targets.paths.len() > TRASH_CONFIRM {
                ask(
                    &w,
                    fill(
                        tr("Move {} to the trash?"),
                        &[&actions::describe(&targets.paths)],
                    ),
                    tr("You can restore them from the trash."),
                    tr("Move to trash"),
                );
                *pending.borrow_mut() = Some(Pending::Trash(targets));
            } else if !targets.paths.is_empty() {
                run_trash(&w, targets.paths);
            }
        }
    });
    window.on_delete_request({
        let (weak, model, pending) = (weak.clone(), model.clone(), pending.clone());
        move |r| {
            let Some(targets) = online_targets(&weak, &model, row(r)) else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            if targets.paths.is_empty() {
                return;
            }
            ask(
                &w,
                fill(
                    tr("Delete {} permanently?"),
                    &[&actions::describe(&targets.paths)],
                ),
                tr("This cannot be undone. Folders are deleted with all their contents."),
                tr("Delete"),
            );
            *pending.borrow_mut() = Some(Pending::Delete(targets));
        }
    });
    window.on_confirm_accepted({
        let (weak, model, pending) = (weak.clone(), model.clone(), pending.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_confirm_open(false);
            let taken = pending.borrow_mut().take();
            // A drive may have been disconnected while the dialog was open.
            let checked = |targets| match model.recheck(targets) {
                Ok(paths) => Some(paths),
                Err(Offline) => {
                    info(&w, labels::offline_refused().into());
                    None
                }
            };
            match taken {
                Some(Pending::Open(t)) => {
                    if let Some(paths) = checked(t) {
                        paths.iter().for_each(|p| open::open(p));
                    }
                }
                Some(Pending::Trash(t)) => {
                    if let Some(paths) = checked(t) {
                        run_trash(&w, paths);
                    }
                }
                Some(Pending::Delete(t)) => {
                    if let Some(paths) = checked(t) {
                        run_delete(&w, paths);
                    }
                }
                // The settings file question (see `ask_reload_settings`).
                None => prefs::with(|p| p.reload_answered(true)),
            }
            w.invoke_focus_list();
        }
    });
    window.on_confirm_cancelled({
        let weak = weak.clone();
        move || {
            if pending.borrow_mut().take().is_none() {
                prefs::with(|p| p.reload_answered(false));
            }
            if let Some(w) = weak.upgrade() {
                w.set_confirm_open(false);
                w.invoke_focus_list();
            }
        }
    });
}

thread_local! {
    /// Files this window put on the clipboard with Cut or Copy.
    static HELD: RefCell<Option<clipboard::Held>> = RefCell::default();
    /// The XWayland clipboard: connected on first use, `Some(None)` when
    /// there is none.
    static X11: RefCell<Option<Option<x11clip::X11Clipboard>>> = RefCell::default();
    /// Files of a cut that were moved here already. Pasted again while
    /// none of them is left, the cut is ignored: they are gone.
    static CUT_PASTED: RefCell<Option<Vec<PathBuf>>> = RefCell::default();
}

/// Runs `f` with the XWayland clipboard, connecting on first use.
fn with_x11<R>(f: impl FnOnce(Option<&x11clip::X11Clipboard>) -> R) -> R {
    X11.with(|x| {
        let mut x = x.borrow_mut();
        let clip = x.get_or_insert_with(x11clip::X11Clipboard::connect);
        f(clip.as_ref())
    })
}

/// Finds what Paste pastes and gives it to `then` on the UI thread: our own
/// files while we hold the clipboard, else another program's from the
/// XWayland clipboard, else files written as text on Slint's clipboard.
/// Asking another program does not block the window: `then` runs once it
/// answered (or after the clipboard's wait).
fn files_to_paste(
    w: &AppWindow,
    then: impl FnOnce(&AppWindow, Option<(clipboard::Operation, Vec<PathBuf>)>) + Send + 'static,
) {
    let finish = move |w: &AppWindow, from_x11: Option<(clipboard::Operation, Vec<PathBuf>)>| {
        let found = from_x11.or_else(|| {
            let text = w.invoke_paste_text().to_string();
            HELD.with(|h| clipboard::to_paste(h.borrow().as_ref(), &text))
        });
        let found = found.filter(|(op, paths)| {
            !CUT_PASTED.with(|c| clipboard::moved_already(*op, paths, c.borrow().as_deref()))
        });
        then(w, found);
    };
    match with_x11(|x| x.map(|x| x.owns())) {
        Some(false) => {
            let weak = w.as_weak();
            with_x11(|x| {
                if let Some(x) = x {
                    x.read_then(move |found| {
                        let _ = weak.upgrade_in_event_loop(move |w| finish(&w, found));
                    });
                }
            });
        }
        Some(true) => {
            let held = HELD.with(|h| h.borrow().as_ref().map(|h| (h.op, h.paths.clone())));
            finish(w, held);
        }
        None => finish(w, None),
    }
}

/// Cut, Copy and Paste of files (see [`clipboard`]). Paste copies or moves
/// on a helper thread, shown in the status line as a job that can be
/// cancelled.
fn connect_clipboard(window: &AppWindow, model: &Rc<ResultsModel>, engine: &Arc<Engine>) {
    // One paste at a time.
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::sync::atomic::Ordering::SeqCst;
    let weak = window.as_weak();
    let row = |r: i32| usize::try_from(r).ok();

    let put = {
        let (weak, model) = (weak.clone(), model.clone());
        move |r: i32, op: clipboard::Operation| {
            let Some(Targets { paths, .. }) = online_targets(&weak, &model, row(r)) else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            if paths.is_empty() {
                return;
            }
            let text = actions::paths_text(&paths, false);
            let held = clipboard::Held { op, paths, text };
            HELD.with(|h| *h.borrow_mut() = Some(held.clone()));
            info(&w, labels::on_clipboard(op, held.paths.len()));
            // Other programs get the files through XWayland, offered without
            // waiting for the display; without it, or when the clipboard
            // could not be taken, the full paths as text (unless something
            // else was copied meanwhile).
            let offered = with_x11(|x| {
                let Some(x) = x else { return false };
                let weak = weak.clone();
                let offered = held.clone();
                x.offer_then(held.clone(), move |taken| {
                    let _ = weak.upgrade_in_event_loop(move |w| {
                        let text = HELD.with(|h| {
                            clipboard::fallback_text(h.borrow().as_ref(), &offered, taken)
                                .map(str::to_owned)
                        });
                        if let Some(text) = text {
                            w.invoke_copy_text(text.into());
                        }
                    });
                });
                true
            });
            if !offered {
                w.invoke_copy_text(held.text.as_str().into());
            }
        }
    };
    let put = Rc::new(put);
    window.on_copy_items({
        let put = put.clone();
        move |r| put(r, clipboard::Operation::Copy)
    });
    window.on_cut_items(move |r| put(r, clipboard::Operation::Cut));

    let engine = engine.clone();
    window.on_paste_items({
        let (weak, model) = (weak.clone(), model.clone());
        move |r| {
            let Some(Targets { paths: targets, .. }) = online_targets(&weak, &model, row(r)) else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            let dest = match targets.as_slice() {
                [] => return,
                [one] if one.is_dir() => one.clone(),
                [_] => {
                    info(&w, tr("Select a folder to paste into.").into());
                    return;
                }
                _ => {
                    info(&w, tr("Select one folder to paste into.").into());
                    return;
                }
            };
            // Taken before the clipboard is asked, so a second paste while
            // another program answers is refused too.
            if BUSY.swap(true, SeqCst) {
                info(&w, tr("Another paste is still running.").into());
                return;
            }
            info(&w, String::new());
            let (engine, weak) = (engine.clone(), weak.clone());
            files_to_paste(&w, move |w, found| {
                let Some((op, sources)) = found else {
                    BUSY.store(false, SeqCst);
                    info(w, tr("The clipboard holds no files to paste.").into());
                    return;
                };
                let folder = dest.file_name().map_or_else(
                    || dest.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let mut ask = false;
                prefs::with(|p| ask = p.fileops_settings().conflict_ask);
                let resolver = ask.then(|| fileops_conflict::resolver(weak.clone()));
                std::thread::spawn(move || {
                    let kind = match op {
                        clipboard::Operation::Copy => engine::TaskKind::Copying,
                        clipboard::Operation::Cut => engine::TaskKind::Moving,
                    };
                    fileops_undo::queue_notice(weak.clone());
                    let result = fileops_undo::catch_worker(|| {
                        let job = engine.begin_job(kind);
                        fileops_undo::paste(op, &sources, &dest, job.progress(), resolver)
                    });
                    let queued = weak.upgrade_in_event_loop(move |w| {
                        w.invoke_fileops_conflict_answer(5);
                        // Keep the next paste out until this paste's dialog
                        // answer and clipboard completion have been handled.
                        BUSY.store(false, SeqCst);
                        fileops_undo_ui::refresh_open();
                        let out = match result {
                            Ok(out) => out,
                            Err(e) => {
                                error(&w, e);
                                return;
                            }
                        };
                        let (text, failed) = labels::pasted(op, &out, &folder);
                        if failed {
                            error(&w, text.clone());
                        } else {
                            info(&w, text.clone());
                        }
                        if out.history_cleared && !failed && !out.cancelled {
                            w.set_notice(tr("Paste finished; undo history cleared.").into());
                            w.set_notice_details(format::visible_name(&text).as_ref().into());
                            w.set_notice_has_details(true);
                        }
                        // A cut is pasted once; after a failure or a cancel it
                        // stays, so pasting again moves what is left.
                        if op == clipboard::Operation::Cut
                            && out.failed.is_empty()
                            && out.skipped == 0
                            && !out.cancelled
                        {
                            let ours = HELD.with(|h| {
                                let mut h = h.borrow_mut();
                                let ours = h.as_ref().is_some_and(|h| h.paths == sources);
                                if ours {
                                    *h = None;
                                }
                                ours
                            });
                            with_x11(|x| {
                                let x = x?;
                                if ours {
                                    x.clear();
                                } else if !x.owns() {
                                    // Another program's cut: the clipboard
                                    // thread empties the clipboard if it still
                                    // holds it, as file managers do after
                                    // pasting a cut. What this window held is
                                    // not on the clipboard any more.
                                    HELD.with(|h| *h.borrow_mut() = None);
                                    x.empty_if_holding((
                                        clipboard::Operation::Cut,
                                        sources.clone(),
                                    ));
                                }
                                Some(())
                            });
                            CUT_PASTED.with(|c| *c.borrow_mut() = Some(sources));
                        }
                    });
                    if queued.is_err() {
                        BUSY.store(false, SeqCst);
                    }
                });
            });
        }
    });
}

/// File → Export results: choose format and scope, then a portal destination.
fn connect_export(window: &AppWindow, model: &Rc<ResultsModel>, engine: &Arc<Engine>) {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::sync::atomic::Ordering::SeqCst;
    let pending = Rc::new(RefCell::new(None));
    window.on_export_results({
        let (weak, model, pending) = (window.as_weak(), model.clone(), pending.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            let (ids, epoch) = model.snapshot();
            if ids.is_empty() {
                info(&w, tr("There are no results to export.").into());
                return;
            }
            if BUSY.swap(true, SeqCst) {
                return;
            }
            let selection = export::Selection::new(ids, model.selected_ids());
            let count = selection.selected_count();
            w.set_export_selection_label(
                fill(
                    trn(
                        "Only the {} selected result",
                        "Only the {} selected results",
                        count,
                    ),
                    &[&format::count(count)],
                )
                .into(),
            );
            w.set_export_has_selection(count > 0);
            w.set_export_only_selected(selection.only_selected);
            prefs::with(|p| {
                w.set_export_format(
                    export::Format::ALL
                        .iter()
                        .position(|f| *f == p.export_format())
                        .unwrap_or(0) as i32,
                )
            });
            *pending.borrow_mut() = Some((selection, epoch));
            w.set_export_open(true);
        }
    });
    window.on_export_choice({
        let (weak, engine) = (window.as_weak(), engine.clone());
        move |accepted| {
            let Some(w) = weak.upgrade() else { return };
            w.set_export_open(false);
            w.invoke_focus_list();
            let Some((mut selection, epoch)) = pending.borrow_mut().take() else {
                return;
            };
            if !accepted {
                BUSY.store(false, SeqCst);
                return;
            }
            let chosen = export::Format::ALL
                .get(w.get_export_format() as usize)
                .copied()
                .unwrap_or(export::Format::Csv);
            selection.only_selected =
                w.get_export_only_selected() && selection.selected_count() > 0;
            let ids = selection.rows();
            let mut protect_formulas = true;
            prefs::with(|p| protect_formulas = p.export_protect_formulas());
            let (engine, weak) = (engine.clone(), weak.clone());
            std::thread::spawn(move || {
                let name = format!("everygrep-results.{}", chosen.extension());
                let pattern = format!("*.{}", chosen.extension());
                let outcome = match portal::save_file(
                    tr("Export results"),
                    tr("Export"),
                    &name,
                    &[portal::SaveFilter {
                        name: chosen.extension(),
                        pattern: &pattern,
                    }],
                ) {
                    Ok(Some((target, _))) => {
                        let format = export::saved_format(&target, chosen);
                        exporting_note(&weak, ids.len());
                        Some(
                            write_export(&engine, &ids, epoch, &target, format, protect_formulas)
                                .map(|n| (n, target, format)),
                        )
                    }
                    Ok(None) => None,
                    Err(e) => Some(Err(fill(tr("Cannot open the save dialog: {}"), &[&e]))),
                };
                BUSY.store(false, SeqCst);
                let _ = weak.upgrade_in_event_loop(move |w| match outcome {
                    Some(Ok((n, target, format))) => {
                        prefs::with(|p| p.set_export_format(format));
                        info(
                            &w,
                            fill(
                                trn("Exported {} result to {}.", "Exported {} results to {}.", n),
                                &[&format::count(n), &target.display()],
                            ),
                        );
                    }
                    Some(Err(e)) => error(&w, e),
                    None => {}
                });
            });
        }
    });
}

/// Help → Copy troubleshooting information: counts the index on a helper
/// thread, then puts the report on the clipboard.
fn connect_troubleshooting(window: &AppWindow, engine: &Arc<Engine>) {
    let (weak, engine) = (window.as_weak(), engine.clone());
    window.on_copy_troubleshooting_info(move || {
        let (weak, engine) = (weak.clone(), engine.clone());
        let language = i18n::current().code();
        std::thread::spawn(move || {
            let facts = diagnostics::Facts::gather(&engine.stats(), language);
            let text = diagnostics::report(&facts);
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.invoke_copy_text(text.into());
                info(&w, tr("Troubleshooting information copied.").into());
            });
        });
    });
}

/// Typing in the result list moves to a name that starts with the typed
/// text (see `jump.rs`), unless Settings → Window turned it off. Moving
/// the cursor another way, a shortcut or leaving the list ends the jump.
fn connect_jump(window: &AppWindow, model: &Rc<ResultsModel>) {
    let (weak, model) = (window.as_weak(), model.clone());
    let jump = Rc::new(RefCell::new(jump::Jump::default()));
    let reset = jump.clone();
    window.on_jump_reset(move || reset.borrow_mut().clear());
    window.on_jump_typed(move |text| {
        let Some(w) = weak.upgrade() else { return -1 };
        let mut on = true;
        prefs::with(|p| on = p.type_to_jump());
        if !on {
            return -1;
        }
        let typed = jump.borrow_mut().typed(&text, std::time::Instant::now());
        let Some(look) = typed else {
            // Another key starts the next jump afresh.
            jump.borrow_mut().clear();
            return -1;
        };
        let current = usize::try_from(w.get_current()).ok();
        match model.jump(&look, current) {
            Some(row) => {
                info(&w, fill(tr("Jump to: {}"), &[&look.shown]));
                i32::try_from(row).unwrap_or(-2)
            }
            None => {
                error(&w, fill(tr("No name starts with {}."), &[&look.shown]));
                -2
            }
        }
    });
}

/// Tools → Open the settings file, in the desktop's application for it.
/// Changes saved there apply when "Apply changes made to the settings file
/// outside this window" is on (the default), after a question unless "Ask
/// first" is off. Looking at the
/// file can wait on a slow or hung file system, so it runs on a helper
/// thread; the window only gets the status line.
fn connect_settings_file(window: &AppWindow) {
    let weak = window.as_weak();
    window.on_open_settings_file(move || {
        let weak = weak.clone();
        std::thread::spawn(move || {
            let note = match diagnostics::settings_file(settings::path()) {
                Ok(path) => {
                    open::open(&path);
                    tr("Opening the settings file…")
                }
                Err(why) => why,
            };
            let _ = weak.upgrade_in_event_loop(move |w| info(&w, note.into()));
        });
    });
}

/// Shows that an export of `n` results is being written.
fn exporting_note(weak: &slint::Weak<AppWindow>, n: usize) {
    let _ = weak.upgrade_in_event_loop(move |w| {
        info(
            &w,
            fill(
                trn("Exporting {} result…", "Exporting {} results…", n),
                &[&format::count(n)],
            ),
        );
    });
}

/// Writes the results `ids` (taken in `epoch`) to `target`. The index is
/// read in chunks, so file changes are not held up by a long export; if
/// the ids change meaning meanwhile, the export stops.
fn write_export(
    engine: &Engine,
    ids: &[everygrep_core::store::NodeId],
    epoch: u64,
    target: &std::path::Path,
    format: export::Format,
    protect_formulas: bool,
) -> Result<usize, String> {
    use std::os::unix::ffi::OsStrExt;
    const CHUNK: usize = 20_000;
    let failed = |e: std::io::Error| fill(tr("Cannot export: {}"), &[&e]);
    let mut out = export::Writer::create(
        target,
        format,
        everygrep_core::localtime::Zone::local(),
        protect_formulas,
    )
    .map_err(failed)?;
    for chunk in ids.chunks(CHUNK) {
        let index = engine.index.read().expect("index lock");
        if engine.epoch() != epoch {
            out.abandon();
            return Err(tr("The results changed while they were exported. Export again.").into());
        }
        let store = index.store();
        for &id in chunk {
            if id.index() >= store.len() || !store.is_live(id) {
                continue;
            }
            let path = store.path(id);
            let name = path.file_name().unwrap_or(path.as_os_str()).as_bytes();
            let folder = path.parent().map_or(&b""[..], |p| p.as_os_str().as_bytes());
            let meta = store.meta(id);
            let row = export::Row {
                name,
                folder,
                size: meta.size,
                mtime_ns: meta.mtime_ns,
                offline: index.is_offline(id),
            };
            if let Err(e) = out.row(&row) {
                out.abandon();
                return Err(failed(e));
            }
        }
    }
    out.finish().map_err(failed)
}

/// The items of a file action on `row`, checked and taken at once; none,
/// with a notice, when a drive that is not connected holds any of them:
/// their paths may now belong to other files, or to nothing.
fn online_targets(
    weak: &slint::Weak<AppWindow>,
    model: &ResultsModel,
    row: Option<usize>,
) -> Option<Targets> {
    match model.checked_targets(row) {
        Ok(targets) => Some(targets),
        Err(Offline) => {
            if let Some(w) = weak.upgrade() {
                info(&w, labels::offline_refused().into());
            }
            None
        }
    }
}

fn info(w: &AppWindow, text: String) {
    w.set_notice_has_details(false);
    w.set_notice_details(text.clone().into());
    w.set_notice_error(false);
    w.set_notice(format::visible_name(&text).as_ref().into());
}

fn error(w: &AppWindow, text: String) {
    w.set_notice_has_details(false);
    w.set_notice_details(text.clone().into());
    w.set_notice_error(true);
    w.set_notice(format::visible_name(&text).as_ref().into());
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

/// Asks whether to apply the settings file changed outside this window;
/// the answer goes to [`prefs::Prefs::reload_answered`]. No file action
/// is pending meanwhile: the dialog is modal.
fn ask_reload_settings(w: &AppWindow) {
    ask(
        w,
        labels::settings_changed_outside().to_owned(),
        labels::settings_changed_outside_ask(),
        labels::settings_reload_action(),
    );
}

/// Moves items to the trash on a helper thread and reports the outcome.
fn run_trash(w: &AppWindow, paths: Vec<PathBuf>) {
    let what = actions::describe(&paths);
    info(w, fill(tr("Moving {} to the trash…"), &[&what]));
    let weak = w.as_weak();
    std::thread::spawn(move || {
        fileops_undo::queue_notice(weak.clone());
        let result = fileops_undo::catch_worker(|| {
            let _files = fileops_undo::file_guard();
            actions::trash(&paths)
        })
        .unwrap_or_else(Err);
        let _ = weak.upgrade_in_event_loop(move |w| match result {
            Ok(()) => info(&w, fill(tr("Moved {} to the trash."), &[&what])),
            Err(e) => error(&w, e),
        });
    });
}

/// Deletes items on a helper thread and reports the outcome.
fn run_delete(w: &AppWindow, paths: Vec<PathBuf>) {
    let what = actions::describe(&paths);
    info(w, fill(tr("Deleting {}…"), &[&what]));
    let weak = w.as_weak();
    std::thread::spawn(move || {
        fileops_undo::queue_notice(weak.clone());
        let result = fileops_undo::catch_worker(|| {
            let _files = fileops_undo::file_guard();
            actions::delete_permanently(&paths)
        });
        let failed = match result {
            Ok(failed) => failed,
            Err(e) => {
                let _ = weak.upgrade_in_event_loop(move |w| error(&w, e));
                return;
            }
        };
        let _ = weak.upgrade_in_event_loop(move |w| match failed.as_slice() {
            [] => info(&w, fill(tr("Deleted {}."), &[&what])),
            [(path, e), ..] => error(
                &w,
                fill(
                    tr("Could not delete {} of {}: {}: {}."),
                    &[
                        &format::count(failed.len()),
                        &format::count(paths.len()),
                        &path.display(),
                        e,
                    ],
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
    show_created: bool,
    show_accessed: bool,
    show_attrs: bool,
    /// Open a window of its own instead of handing over to a running one.
    new_instance: bool,
    /// Started on login: when closing only hides the window, load the
    /// index and wait without showing it; do nothing when a window for
    /// the index runs already.
    background: bool,
    /// Show the window, or hide it when it is in front (see
    /// [`instance::Launch::Toggle`]).
    toggle: bool,
    /// Open another window over the same index even when one runs.
    new_window: bool,
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
  --prefix               terms must match at the start of the name
  --suffix               terms must match at the end of the name
  --ignore-punctuation   leave punctuation out of names and terms
  --ignore-whitespace    leave whitespace out of names and terms
  --regex                the whole query is one regular expression
  --columns <list>       show optional columns: type, extension, created,
                         accessed, attributes (comma separated)
  --new-instance         open another window even if one is already open
  --toggle               show the window, or hide it when it is in front
                         (quit instead when closing the window quits)
  --new-window           open another window over the same index
  --background           start without a window when closing only hides it
                         (used when starting on login); nothing happens when
                         EveryGrep already runs
  --settings[=<page>]    open the settings window too, at a page: indexing,
                         search, appearance, window, keyboard, advanced
  -h, --help             print this help
  -V, --version          print the version";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Run(Args),
    Help,
    Version,
}

/// `everygrep-gui [options] [query words...]`, see [`USAGE`].
/// Without roots the saved roots, or the home folder, are used. An unknown
/// option or value is an error, so a mistyped flag never opens a window on
/// the default roots; after `--` every argument is a query word.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Cli, String> {
    parse_args_with_options(args, QueryOptions::default())
}

fn parse_args_with_options(
    args: impl IntoIterator<Item = OsString>,
    defaults: QueryOptions,
) -> Result<Cli, String> {
    let mut out = Args {
        options: defaults,
        ..Default::default()
    };
    let mut words: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    // Paths are taken as they are; every other argument must be text.
    let text = |a: OsString| {
        a.into_string()
            .map_err(|a| format!("{} is not valid UTF-8", a.to_string_lossy()))
    };
    while let Some(a) = args.next() {
        if a == "--root" || a == "--index-file" {
            let path = PathBuf::from(
                args.next()
                    .ok_or_else(|| format!("{} needs a value", a.to_string_lossy()))?,
            );
            if a == "--root" {
                out.roots.push(path);
            } else {
                out.index_file = Some(path);
            }
            continue;
        }
        let a = text(a)?;
        let mut value = || text(args.next().ok_or_else(|| format!("{a} needs a value"))?);
        match a.as_str() {
            "-h" | "--help" => return Ok(Cli::Help),
            "-V" | "--version" => return Ok(Cli::Version),
            "--filter" => {
                let name = value()?;
                out.options.filter =
                    Filter::from_id(&name).ok_or_else(|| format!("unknown filter {name}"))?;
                out.options_given = true;
            }
            "--case"
            | "--whole-words"
            | "--match-path"
            | "--diacritics"
            | "--prefix"
            | "--suffix"
            | "--ignore-punctuation"
            | "--ignore-whitespace"
            | "--regex" => {
                let o = &mut out.options;
                let mode = match a.as_str() {
                    "--case" => &mut o.case_sensitive,
                    "--whole-words" => &mut o.whole_words,
                    "--match-path" => &mut o.match_path,
                    "--prefix" => &mut o.prefix,
                    "--suffix" => &mut o.suffix,
                    "--ignore-punctuation" => &mut o.ignore_punctuation,
                    "--ignore-whitespace" => &mut o.ignore_whitespace,
                    "--regex" => &mut o.regex,
                    _ => &mut o.match_diacritics,
                };
                *mode = true;
                out.options_given = true;
            }
            "--new-instance" => out.new_instance = true,
            "--background" => out.background = true,
            "--toggle" => out.toggle = true,
            "--new-window" => out.new_window = true,
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
                        "created" => out.show_created = true,
                        "accessed" => out.show_accessed = true,
                        "attributes" => out.show_attrs = true,
                        other => return Err(format!("unknown column {other}")),
                    }
                }
            }
            "--" => {
                for word in args.by_ref() {
                    words.push(text(word)?);
                }
            }
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => words.push(a),
        }
    }
    out.query = words.join(" ");
    Ok(Cli::Run(out))
}

/// The sort for a header column of the window (0 name, 1 folder, 2 size,
/// 3 modified, 4 type, 5 extension). The folder column sorts by folder, then
/// name; folders sort by size as `folders` says.
/// Whether the first click on the header of `column` sorts it descending:
/// the date columns (Modified, Created, Accessed) with `dates`, Size with
/// `sizes`; every other column starts ascending.
pub fn first_descending(column: i32, dates: bool, sizes: bool) -> bool {
    match column {
        2 => sizes,
        3 | 6 | 7 => dates,
        _ => false,
    }
}

fn sort_for_column(column: i32, descending: bool, folders: FolderSize) -> Sort {
    let key = match column {
        1 => SortKey::Path,
        2 => SortKey::Size,
        3 => SortKey::Modified,
        4 => SortKey::Type,
        5 => SortKey::Extension,
        6 => SortKey::Created,
        7 => SortKey::Accessed,
        8 => SortKey::Attributes,
        12 => SortKey::NameFrequency,
        13 => SortKey::ExtensionFrequency,
        14 => SortKey::SizeFrequency,
        c if everygrep_core::property::Property::from_column(c).is_some() => SortKey::Property(
            everygrep_core::property::Property::from_column(c).expect("property column"),
        ),
        _ => SortKey::Name,
    };
    Sort {
        key,
        descending,
        folders,
        then: [None; 2],
    }
}

/// Whether the result table can be sorted by `column` (a column that sorts
/// by a key of its own), as tab sessions and bookmarks keep it.
pub fn is_sort_column(column: i32) -> bool {
    column_of(sort_for_column(column, false, FolderSize::default()).key) == column
}

/// The column of the result table that sorts by `key`; the reverse of
/// [`sort_for_column`].
fn column_of(key: SortKey) -> i32 {
    match key {
        SortKey::Name => 0,
        SortKey::Path => 1,
        SortKey::Size => 2,
        SortKey::Modified => 3,
        SortKey::Type => 4,
        SortKey::Extension => 5,
        SortKey::Created => 6,
        SortKey::Accessed => 7,
        SortKey::Attributes => 8,
        SortKey::NameFrequency => 12,
        SortKey::ExtensionFrequency => 13,
        SortKey::SizeFrequency => 14,
        SortKey::Property(p) => p.column(),
    }
}

/// Shows folder sizes as `folders` says and sorts by them; the rows change
/// with the new result.
fn set_folder_size(w: &AppWindow, engine: &Engine, folders: FolderSize) {
    with_model(w, |m| m.set_folder_size(folders));
    engine.set_sort(sort_dialog::window_sort(w, folders));
}

/// The matching modes and filter set in the window.
/// Notes that the changed search waits for Enter (search as you type is
/// off).
fn wait_for_enter(w: &AppWindow) {
    w.set_search_pending(true);
    w.set_notice_error(false);
    // Searching as you type, only a search that reads files waits.
    let notice = if typing::with(|t| t.as_you_type()) {
        tr("Press Enter to search file contents.")
    } else {
        tr("Press Enter to search.")
    };
    info(w, notice.into());
}

/// Whether the search in the field, with the chosen filter, reads files.
fn field_reads_files(w: &AppWindow) -> bool {
    let filter = saved_ui::selected(w).map(|f| (f.search, f.options));
    typing::reads_files(&w.get_query(), options(w), filter.as_ref())
}

/// Runs the search in the field now and keeps it in the history.
pub fn search_now(w: &AppWindow, engine: &Engine) {
    let query = w.get_query();
    run_query(w, engine);
    w.set_search_pending(false);
    info(w, String::new());
    prefs::with(|p| p.record(&query));
}

/// Changes whether the search follows every change, and whether searches
/// that read files wait for Enter meanwhile; a search that was waiting for
/// Enter runs when it no longer has to.
pub fn set_as_you_type(w: &AppWindow, engine: &Engine, on: bool, contents_on_enter: bool) {
    let reads = field_reads_files(w);
    if typing::with(|t| t.set_as_you_type(on, contents_on_enter, reads)) {
        search_now(w, engine);
    }
}

fn run_query(w: &AppWindow, engine: &Engine) {
    with_model(w, |m| m.forget_properties());
    tabs_ui::submitted(w);
    let filter = saved_ui::selected(w).map(|f| (f.search, f.options));
    engine.set_filtered_query(&w.get_query(), options(w), filter);
}

fn options(w: &AppWindow) -> QueryOptions {
    QueryOptions {
        case_sensitive: w.get_match_case(),
        whole_words: w.get_whole_words(),
        match_path: w.get_match_path(),
        match_diacritics: w.get_match_diacritics(),
        prefix: w.get_match_prefix(),
        suffix: w.get_match_suffix(),
        ignore_punctuation: w.get_ignore_punctuation(),
        ignore_whitespace: w.get_ignore_whitespace(),
        hidden_files: HiddenFiles::ALL
            .get(w.get_hidden_files() as usize)
            .copied()
            .unwrap_or_default(),
        regex: w.get_use_regex(),
        and_before_or: w.get_and_before_or(),
        literal_operators: w.get_literal_operators(),
        wildcards_anywhere: w.get_wildcards_anywhere(),
        filter: saved_ui::selected(w).map_or(Filter::All, |f| f.options.filter),
    }
}

/// Sets the window's toggles and filter selector.
fn set_options(w: &AppWindow, o: QueryOptions) {
    w.set_match_case(o.case_sensitive);
    w.set_whole_words(o.whole_words);
    w.set_match_path(o.match_path);
    w.set_match_diacritics(o.match_diacritics);
    w.set_match_prefix(o.prefix);
    w.set_match_suffix(o.suffix);
    w.set_ignore_punctuation(o.ignore_punctuation);
    w.set_ignore_whitespace(o.ignore_whitespace);
    w.set_use_regex(o.regex);
    saved_ui::select_builtin(w, o.filter);
    show_badges(w);
}

fn show_badges(w: &AppWindow) {
    let badges: Vec<SharedString> = labels::mode_badges(&options(w))
        .into_iter()
        .map(SharedString::from)
        .collect();
    w.set_badges(ModelRc::new(VecModel::from(badges)));
    // A new list, so the Filter menu shows a fresh check mark.
    saved_ui::fill_filters(w);
}

/// Tells the result rows which columns read from the file system are shown.
fn apply_detail_columns(w: &AppWindow) {
    let columns = DetailColumns {
        created: w.get_show_created(),
        accessed: w.get_show_accessed(),
        attributes: w.get_show_attrs(),
    };
    let hashes = hash_columns::HashColumns {
        md5: w.get_show_md5(),
        sha256: w.get_show_sha256(),
        crc32: w.get_show_crc32(),
    };
    let frequencies = results::FrequencyColumns {
        name: w.get_show_name_freq(),
        extension: w.get_show_ext_freq(),
        size: w.get_show_size_freq(),
    };
    with_model(w, |m| {
        m.set_properties(vec![
            w.get_show_full_path(),
            w.get_show_parent_path(),
            w.get_show_stem(),
            w.get_show_name_length(),
            w.get_show_path_length(),
            w.get_show_disk_size(),
            w.get_show_total_size(),
            w.get_show_child_count(),
            w.get_show_child_files(),
            w.get_show_child_folders(),
            w.get_show_desc_count(),
            w.get_show_desc_files(),
            w.get_show_desc_folders(),
            w.get_show_folder_depth(),
            w.get_show_hard_links(),
            w.get_show_shortcut_target(),
            w.get_show_owner(),
            w.get_show_width(),
            w.get_show_height(),
            w.get_show_dimensions(),
            w.get_show_length(),
            w.get_show_bit_rate(),
            w.get_show_title(),
            w.get_show_artist(),
            w.get_show_album(),
            w.get_show_year(),
            w.get_show_track(),
            w.get_show_genre(),
            w.get_show_rating(),
            w.get_show_tags(),
            w.get_show_page_count(),
            w.get_show_word_count(),
            w.get_show_camera_maker(),
            w.get_show_camera_model(),
            w.get_show_date_taken(),
            w.get_show_gps_lat(),
            w.get_show_gps_long(),
        ]);
        m.set_detail_columns(columns);
        m.set_hash_columns(hashes);
        m.set_frequency_columns(frequencies);
    });
}

fn with_model(w: &AppWindow, f: impl FnOnce(&ResultsModel)) {
    let results = w.get_results();
    if let Some(m) = results.as_any().downcast_ref::<ResultsModel>() {
        f(m);
    }
}

fn show_results(w: &AppWindow, result: SearchResult) {
    let mut current = false;
    with_model(w, |m| current = m.is_current(&result));
    if !current {
        return;
    }
    let count = result.ids.len();
    let mut kept = None;
    with_model(w, |m| {
        kept = m.set_result(&result);
        // An item being renamed may have moved in the list, or be gone.
        let row = m.renaming_row();
        if row.is_none() {
            m.set_renaming(None);
        }
        w.set_rename_row(row.map_or(-1, |r| r as i32));
    });
    if count == 0 {
        w.set_current(-1);
    } else if let Some(row) = kept {
        // The selection was kept for a new query: actions start from it.
        w.set_current(row as i32);
    } else if w.get_current() < 0 || w.get_current() as usize >= count {
        w.set_current(0);
    }
    w.set_message_details(result.error.as_deref().unwrap_or_default().into());
    w.set_message_error(result.error.is_some());
    w.set_message(match result.error {
        Some(e) => fill(tr("Cannot search: {}"), &[&search_errors::message(&e)]).into(),
        None => "".into(),
    });
    refresh_status(w);
}

fn refresh_status(w: &AppWindow) {
    let mut message = None;
    with_model(w, |m| {
        let status = m.status();
        w.set_status_count(status.count.into());
        w.set_status_detail(status.detail.into());
        w.set_status_index(status.index.into());
        w.set_status_right(status.time.into());
        message = if m.row_count() == 0 {
            labels::loading_message(m.state())
        } else {
            None
        };
    });
    if let Some(text) = message {
        w.set_message_error(false);
        w.set_message(text.into());
    } else if is_loading_message(&w.get_message()) {
        w.set_message("".into());
    }
}

/// Whether `text` is one of the loading messages, in any language (the
/// language may have changed since it was shown).
fn is_loading_message(text: &str) -> bool {
    !text.is_empty()
        && i18n::Lang::ALL.into_iter().any(|lang| {
            [
                "Loading the index…",
                "Indexing your files. This happens once; later starts are instant.",
            ]
            .into_iter()
            .any(|m| i18n::tr_in(lang, m) == text)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A right-click on the empty area below the rows opens its own menu:
    /// the area is behind the list, which lets right-clicks no row takes
    /// through (a flickable only handles the left button).
    #[test]
    fn the_empty_area_below_the_rows_has_a_menu() {
        let ui = include_str!("../ui/app.slint");
        let menu = ui.find("blank-menu := ContextMenuArea").expect("menu");
        let list = ui.find("list := ListView").expect("list");
        let area = &ui[menu..list];
        assert!(area.contains("event.button == PointerEventButton.right"));
        assert!(area.contains("blank-menu.show("));
        for command in [
            "select-all",
            "invert-selection",
            "export-results",
            "refresh",
            "organize-columns",
            "fit-columns-to-window",
        ] {
            assert!(
                area.contains(&format!("root.run(Command.{command})")),
                "{command}"
            );
        }
    }

    /// Left and Right in the result list scroll it sideways, within the
    /// room the table has, and Ctrl goes to the edge.
    #[test]
    fn left_and_right_scroll_the_list_sideways() {
        let ui = include_str!("../ui/app.slint");
        let keys = &ui[ui.find("keys := FocusScope").unwrap()..];
        let at = keys
            .find("event.text == Key.LeftArrow || event.text == Key.RightArrow")
            .expect("handled");
        let handler = &keys[at..at + keys[at..].find("return accept;").unwrap()];
        for part in [
            "root.jump-reset();",
            "event.modifiers.control ? root.scroll-room : root.sideways-step",
            "min(0px, root.results-scroll-x + step)",
            "max(-root.scroll-room, root.results-scroll-x - step)",
        ] {
            assert!(handler.contains(part), "{part}");
        }
        assert!(ui.contains("property <length> sideways-step: 48px;"));
    }

    fn parse(list: &[&str]) -> Result<Cli, String> {
        parse_args(list.iter().map(OsString::from))
    }

    #[test]
    fn paths_keep_bytes_that_are_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let bad = std::ffi::OsStr::from_bytes(b"/data/\xff");
        let list = [
            OsString::from("--root"),
            bad.into(),
            "--index-file".into(),
            bad.into(),
        ];
        let Ok(Cli::Run(a)) = parse_args(list) else {
            panic!("refused")
        };
        assert_eq!(a.roots, [PathBuf::from(bad)]);
        assert_eq!(a.index_file.as_deref(), Some(std::path::Path::new(bad)));
        let words = [OsString::from("--"), bad.into()];
        assert!(parse_args(words).unwrap_err().contains("not valid UTF-8"));
    }

    fn args(list: &[&str]) -> Args {
        match parse(list) {
            Ok(Cli::Run(a)) => a,
            other => panic!("{list:?} gave {other:?}"),
        }
    }

    #[test]
    fn sort_columns_round_trip() {
        for column in 0..9 {
            let sort = sort_for_column(column, false, FolderSize::default());
            assert_eq!(column_of(sort.key), column);
        }
    }

    #[test]
    fn help_version_and_mistakes_never_open_a_window() {
        assert_eq!(parse(&["--help"]), Ok(Cli::Help));
        assert_eq!(parse(&["a", "-h"]), Ok(Cli::Help));
        assert_eq!(parse(&["-V"]), Ok(Cli::Version));
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
            "type, extension,attributes",
            "img",
        ]);
        assert_eq!(a.options.filter, Filter::Picture);
        assert!(a.options.case_sensitive && a.options.match_diacritics);
        assert!(!a.options.whole_words && !a.options.match_path);
        assert!(a.show_type && a.show_ext && a.show_attrs);
        assert!(!a.show_created && !a.show_accessed);
        assert_eq!(a.query, "img");
        let a = args(&["--whole-words", "--match-path"]);
        assert!(a.options.whole_words && a.options.match_path);
        let a = args(&["--prefix", "--suffix", "--ignore-punctuation"]);
        assert!(a.options.prefix && a.options.suffix && a.options.ignore_punctuation);
        assert!(!a.options.ignore_whitespace && !a.options.regex);
        let a = args(&["--ignore-whitespace", "--regex", "a|b"]);
        assert!(a.options.ignore_whitespace && a.options.regex && a.options_given);
        assert_eq!(a.query, "a|b");
        assert_eq!(a.options.filter, Filter::All);
        assert!(!a.show_type && !a.show_ext);
        let a = args(&["--columns", "created,accessed"]);
        assert!(a.show_created && a.show_accessed && !a.show_attrs);
    }

    fn present_args(list: &[&str]) -> Presentation {
        presentation(
            &list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            QueryOptions::default(),
        )
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
        assert!(p.show_type && !p.show_ext && !p.show_created);
        let p = present_args(&["--columns", "created,attributes"]);
        assert!(p.show_created && p.show_attrs && !p.show_accessed);
        // `--filter all` is a choice too: it resets the filter.
        assert_eq!(
            present_args(&["--filter", "all"]).options,
            Some(QueryOptions::default())
        );
        for flag in [
            "--whole-words",
            "--match-path",
            "--diacritics",
            "--prefix",
            "--suffix",
            "--ignore-punctuation",
            "--ignore-whitespace",
            "--regex",
        ] {
            assert!(present_args(&[flag]).options.is_some(), "{flag}");
        }
    }

    #[test]
    fn forwarded_case_search_uses_settings_policy_and_preserves_layout() {
        let policy = QueryOptions {
            whole_words: true,
            hidden_files: HiddenFiles::Only,
            and_before_or: true,
            literal_operators: true,
            wildcards_anywhere: true,
            ..Default::default()
        };
        let p = presentation(
            &["--case", "--columns", "created", "foo"].map(str::to_owned),
            policy,
        );
        assert!(p.options.unwrap().whole_words);
        let current = tabs::SearchState {
            columns: settings::Columns {
                show_type: true,
                name: Some(419.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let search = current.launched(
            p.query.as_deref().unwrap(),
            p.options.unwrap(),
            false,
            &settings::Columns {
                show_created: p.show_created,
                ..Default::default()
            },
        );
        let effective = tabs::with_policy(search.options, policy);
        assert_eq!(search.query, "foo");
        assert!(
            effective.case_sensitive
                && effective.and_before_or
                && effective.literal_operators
                && effective.wildcards_anywhere
        );
        assert_eq!(effective.hidden_files, HiddenFiles::Only);
        assert!(search.columns.show_type && search.columns.show_created);
        assert_eq!(search.columns.name, Some(419.0));
        assert_eq!(present_args(&["--regex"]).query, None);
    }

    #[test]
    fn settings_flag_opens_a_page() {
        assert_eq!(args(&["x"]).settings_page, None);
        assert_eq!(args(&["--settings"]).settings_page, Some(0));
        assert_eq!(args(&["--settings=window"]).settings_page, Some(3));
        assert_eq!(args(&["--settings=appearance"]).settings_page, Some(2));
        assert!(parse(&["--settings=nope"]).is_err());
        assert_eq!(present_args(&["--settings=search"]).settings_page, Some(1));
        assert_eq!(args(&["--settings=keyboard"]).settings_page, Some(4));
        assert_eq!(args(&["--settings=shortcut"]).settings_page, Some(4));
        assert_eq!(args(&["--", "--settings"]).query, "--settings");
    }

    #[test]
    fn a_new_window_opens_over_the_same_index() {
        assert_eq!(new_window_args(&[], None), ["--new-instance"]);
        let args = new_window_args(
            &[PathBuf::from("/a b"), PathBuf::from("/c")],
            Some(std::path::Path::new("/tmp/i.bin")),
        );
        assert_eq!(
            args,
            [
                "--new-instance",
                "--root",
                "/a b",
                "--root",
                "/c",
                "--index-file",
                "/tmp/i.bin"
            ]
        );
        // The arguments parse back to the same index.
        let back = parse_args(args.iter().cloned());
        let Ok(Cli::Run(a)) = back else {
            panic!("{back:?}")
        };
        assert!(a.new_instance && a.query.is_empty());
        assert_eq!(a.roots, [PathBuf::from("/a b"), PathBuf::from("/c")]);
    }

    #[test]
    fn a_toggle_closes_only_a_window_in_front() {
        use PresentAction::*;
        // (toggle, visible, focused, hide on close)
        assert_eq!(present_action(false, true, true, true), Show);
        assert_eq!(present_action(false, false, false, false), Show);
        assert_eq!(present_action(true, true, true, true), Hide);
        assert_eq!(present_action(true, true, true, false), Quit);
        // Hidden, or behind other windows: brought forward.
        assert_eq!(present_action(true, false, false, true), Show);
        assert_eq!(present_action(true, true, false, true), Show);
        assert_eq!(present_action(true, true, false, false), Show);
    }

    #[test]
    fn toggle_and_new_window_flags() {
        assert!(args(&["--toggle"]).toggle);
        assert!(args(&["--new-window"]).new_window);
        let a = args(&["x"]);
        assert!(!a.toggle && !a.new_window);
        // The window being toggled ignores the flags themselves.
        assert_eq!(present_args(&["--toggle"]), Presentation::default());
        assert_eq!(present_args(&["--new-window"]), Presentation::default());
    }

    #[test]
    fn background_flag() {
        assert!(args(&["--background"]).background);
        assert!(!args(&["x"]).background);
        assert_eq!(args(&["--", "--background"]).query, "--background");
    }

    #[test]
    fn new_instance_flag() {
        assert!(args(&["--new-instance", "x"]).new_instance);
        assert!(!args(&["x"]).new_instance);
        assert_eq!(args(&["--", "--new-instance"]).query, "--new-instance");
    }

    #[test]
    fn first_clicks_sort_dates_and_sizes_as_set() {
        for column in 0..=8 {
            assert!(!first_descending(column, false, false), "{column}");
        }
        let dates: Vec<i32> = (0..=8)
            .filter(|&c| first_descending(c, true, false))
            .collect();
        assert_eq!(dates, [3, 6, 7], "Modified, Created, Accessed");
        let sizes: Vec<i32> = (0..=8)
            .filter(|&c| first_descending(c, false, true))
            .collect();
        assert_eq!(sizes, [2]);
        assert!(!first_descending(0, true, true), "names start ascending");
        assert!(
            !first_descending(8, true, true),
            "attributes start ascending"
        );
    }

    #[test]
    fn header_columns_map_to_sort_keys() {
        let keys: Vec<SortKey> = (0..9)
            .map(|c| sort_for_column(c, false, FolderSize::Total).key)
            .collect();
        assert_eq!(
            keys,
            [
                SortKey::Name,
                SortKey::Path,
                SortKey::Size,
                SortKey::Modified,
                SortKey::Type,
                SortKey::Extension,
                SortKey::Created,
                SortKey::Accessed,
                SortKey::Attributes
            ]
        );
        assert!(sort_for_column(2, true, FolderSize::Total).descending);
        assert_eq!(
            sort_for_column(2, false, FolderSize::Items).folders,
            FolderSize::Items
        );
        assert_eq!(
            sort_for_column(9, false, FolderSize::None).key,
            SortKey::Name
        );
    }
}
