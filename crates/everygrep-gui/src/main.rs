//! EveryGrep desktop window.

mod engine;
mod filetype;
mod format;
mod open;
mod results;
mod selection;

use std::path::PathBuf;
use std::rc::Rc;

use everygrep_core::query::QueryOptions;
use everygrep_core::sort::{Sort, SortKey};
use slint::{ComponentHandle, Model, ModelRc};

use engine::{Engine, Hooks, IndexState, SearchResult};
use results::ResultsModel;

slint::include_modules!();

/// Selection modes of the window's `select` callback.
const SELECT_TOGGLE: i32 = 1;
const SELECT_EXTEND: i32 = 2;

/// Wayland app id; equals the desktop entry and icon file names.
const APP_ID: &str = "az.huseynz.EveryGrep";

fn main() -> Result<(), slint::PlatformError> {
    let args = parse_args(std::env::args().skip(1));
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
                });
            }
        }),
    };
    let engine = Engine::start(args.roots, args.index_file, hooks);
    let model = Rc::new(ResultsModel::new(engine.clone()));
    window.set_results(ModelRc::from(model.clone()));
    refresh_status(&window);
    // The search thread runs the query now and again once the index is
    // loaded, so the first results appear as soon as they can.
    if !args.query.is_empty() {
        window.set_query(args.query.as_str().into());
        engine.set_query(&args.query, QueryOptions::default());
    }

    window.on_query_edited({
        let engine = engine.clone();
        move |text| {
            engine.set_query(&text, QueryOptions::default());
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
    window.on_activate({
        let model = model.clone();
        move |row| {
            if let Some(path) = usize::try_from(row).ok().and_then(|r| model.path(r)) {
                open::open(&path);
            }
        }
    });
    window.on_open_folder({
        let model = model.clone();
        move |row| {
            if let Some(path) = usize::try_from(row).ok().and_then(|r| model.path(r)) {
                open::show_in_folder(&path);
            }
        }
    });

    window.run()?;
    engine.shutdown();
    Ok(())
}

/// Command line of the window.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    roots: Vec<PathBuf>,
    index_file: Option<PathBuf>,
    /// Words that are not options, joined by spaces: the initial query.
    query: String,
}

/// `everygrep-gui [--root <path>]... [--index-file <path>] [query words...]`.
/// Without roots the saved roots, or the home folder, are used. Unknown
/// options are ignored; after `--` every argument is a query word.
fn parse_args(args: impl IntoIterator<Item = String>) -> Args {
    let mut out = Args::default();
    let mut words: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => out.roots.extend(args.next().map(PathBuf::from)),
            "--index-file" => out.index_file = args.next().map(PathBuf::from),
            "--" => words.extend(args.by_ref()),
            _ if a.starts_with("--") => {}
            _ => words.push(a),
        }
    }
    out.query = words.join(" ");
    out
}

/// The sort for a header column of the window (0 name, 1 folder, 2 size,
/// 3 modified). The folder column sorts by folder, then name.
fn sort_for_column(column: i32, descending: bool) -> Sort {
    let key = match column {
        1 => SortKey::Path,
        2 => SortKey::Size,
        3 => SortKey::Modified,
        _ => SortKey::Name,
    };
    Sort { key, descending }
}

fn with_model(w: &AppWindow, f: impl FnOnce(&ResultsModel)) {
    let results = w.get_results();
    if let Some(m) = results.as_any().downcast_ref::<ResultsModel>() {
        f(m);
    }
}

fn show_results(w: &AppWindow, result: SearchResult) {
    let count = result.ids.len();
    with_model(w, |m| m.set_result(&result));
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

    fn args(list: &[&str]) -> Args {
        parse_args(list.iter().map(|s| (*s).to_owned()))
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
        // Unknown options are skipped; after `--` options are words too.
        assert_eq!(
            args(&["--fast", "a", "--", "--root", "b"]).query,
            "a --root b"
        );
        assert!(args(&["--", "--root", "b"]).roots.is_empty());
        // Single-dash words such as `-draft` are query words.
        assert_eq!(args(&["-x", "ext:pdf"]).query, "-x ext:pdf");
    }

    #[test]
    fn header_columns_map_to_sort_keys() {
        let keys: Vec<SortKey> = (0..4).map(|c| sort_for_column(c, false).key).collect();
        assert_eq!(
            keys,
            [
                SortKey::Name,
                SortKey::Path,
                SortKey::Size,
                SortKey::Modified
            ]
        );
        assert!(sort_for_column(2, true).descending);
        assert_eq!(sort_for_column(9, false).key, SortKey::Name);
    }
}
