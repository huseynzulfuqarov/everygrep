//! EveryGrep desktop window.

mod engine;
mod format;
mod open;
mod results;

use std::path::PathBuf;
use std::rc::Rc;

use everygrep_core::query::QueryOptions;
use slint::{ComponentHandle, Model, ModelRc};

use engine::{Engine, Hooks, IndexState, SearchResult};
use results::ResultsModel;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let (roots, index_file) = parse_args();
    let window = AppWindow::new()?;
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
    let engine = Engine::start(roots, index_file, hooks);
    let model = Rc::new(ResultsModel::new(engine.clone()));
    window.set_results(ModelRc::from(model.clone()));
    refresh_status(&window);

    window.on_query_edited({
        let engine = engine.clone();
        move |text| {
            engine.set_query(&text, QueryOptions::default());
        }
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

/// `--root <path>` (repeatable) and `--index-file <path>`. Without roots the
/// saved roots, or the home folder, are used.
fn parse_args() -> (Vec<PathBuf>, Option<PathBuf>) {
    let mut roots = Vec::new();
    let mut index_file = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => roots.extend(args.next().map(PathBuf::from)),
            "--index-file" => index_file = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    (roots, index_file)
}

fn with_model(w: &AppWindow, f: impl FnOnce(&ResultsModel)) {
    let results = w.get_results();
    if let Some(m) = results.as_any().downcast_ref::<ResultsModel>() {
        f(m);
    }
}

fn show_results(w: &AppWindow, result: SearchResult) {
    let count = result.ids.len();
    with_model(w, |m| m.set_ids(result.ids, result.took));
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
