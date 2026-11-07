//! Paste or import filename lists without expanding them into OR terms.
use crate::i18n::{fill, tr};
use crate::{AppWindow, NameListWindow};
use everygrep_core::namelist::{self, Options};
use everygrep_core::query::QueryOptions;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::io::Read;
use std::path::Path;
use std::rc::Rc;

struct State {
    main: slint::Weak<AppWindow>,
    window: RefCell<Option<NameListWindow>>,
}

thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }

fn with(f: impl FnOnce(&State)) {
    let state = STATE.with(|state| state.borrow().clone());
    if let Some(state) = state {
        f(&state);
    }
}

pub fn connect(main: &AppWindow) {
    STATE.with(|state| {
        *state.borrow_mut() = Some(Rc::new(State {
            main: main.as_weak(),
            window: RefCell::new(None),
        }))
    });
    main.on_search_name_list(|| with(State::open));
}

pub fn apply_scheme(scheme: i32, style: i32) {
    with(|state| {
        if let Some(window) = state.window.borrow().as_ref() {
            window.invoke_set_color_scheme(scheme, style);
        }
    });
}

fn list_error(error: namelist::Error) -> String {
    crate::search_errors::message(&format!("name list: {error}"))
}

/// Bound reads even when the selected file grows after it was opened. Never
/// replace the current draft on cancellation or on any validation failure.
fn load(path: &Path) -> Result<String, String> {
    let read = || -> std::io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other(tr("Choose a regular text file.")));
        }
        let mut bytes = Vec::new();
        file.take(namelist::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let bytes = read().map_err(|e| fill(tr("Could not load the name list: {}"), &[&e]))?;
    if bytes.len() > namelist::MAX_BYTES {
        return Err(list_error(namelist::Error::TooLarge));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| tr("The name list must be a UTF-8 text file.").to_owned())?;
    namelist::query(&text, Options::default()).map_err(list_error)?;
    Ok(text)
}

/// The list's matching options are encoded in its query. Preserve the
/// window's case choice so replacing this query does not change later searches.
/// Generated query syntax still needs ordinary query parsing, as before.
fn prepare_search(
    text: &str,
    list: Options,
    window: QueryOptions,
) -> Result<(String, QueryOptions), namelist::Error> {
    let query = namelist::query(text, list)?;
    Ok((
        query,
        QueryOptions {
            regex: false,
            ..window
        },
    ))
}

impl State {
    fn open(&self) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        if self.window.borrow().is_none() {
            let window = match NameListWindow::new() {
                Ok(window) => window,
                Err(e) => {
                    crate::error(&main, e.to_string());
                    return;
                }
            };
            window.on_use_list(|| with(State::apply));
            window.on_load_list({
                let weak = window.as_weak();
                move || {
                    let Some(window) = weak.upgrade() else { return };
                    if window.get_busy() {
                        return;
                    }
                    window.set_busy(true);
                    let weak = window.as_weak();
                    std::thread::spawn(move || {
                        let result = crate::portal::open_file(
                            tr("Load name list"),
                            crate::labels::choose_accept(),
                            &[
                                crate::portal::SaveFilter {
                                    name: tr("Text files"),
                                    pattern: "*.txt",
                                },
                                crate::portal::SaveFilter {
                                    name: tr("All files"),
                                    pattern: "*",
                                },
                            ],
                        )
                        .and_then(|path| path.map(|path| load(&path)).transpose());
                        let _ = weak.upgrade_in_event_loop(move |window| {
                            window.set_busy(false);
                            match result {
                                Ok(Some(text)) => {
                                    window.set_names_text(text.into());
                                    window.set_notice("".into());
                                }
                                Ok(None) => {}
                                Err(e) => window.set_notice(e.into()),
                            }
                        });
                    });
                }
            });
            window.on_close_window({
                let weak = window.as_weak();
                move || {
                    if let Some(window) = weak.upgrade() {
                        let _ = window.hide();
                    }
                }
            });
            *self.window.borrow_mut() = Some(window);
        }
        crate::prefs::with(|p| apply_scheme(p.color_scheme(), p.dark_style()));
        if let Some(window) = self.window.borrow().as_ref() {
            window.set_notice("".into());
            let _ = window.hide();
            let _ = window.show();
        }
    }

    fn apply(&self) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        let window = self.window.borrow();
        let Some(window) = window.as_ref() else {
            return;
        };
        if window.get_busy() {
            return;
        }
        let (query, options) = match prepare_search(
            &window.get_names_text(),
            Options {
                case_sensitive: window.get_match_case(),
                ignore_extension: window.get_ignore_extension(),
                contains: window.get_contains(),
            },
            crate::options(&main),
        ) {
            Ok(query) => query,
            Err(e) => {
                window.set_notice(list_error(e).into());
                return;
            }
        };
        main.invoke_history_record();
        main.set_use_regex(options.regex);
        main.set_match_case(options.case_sensitive);
        main.set_query(query.as_str().into());
        crate::show_badges(&main);
        main.invoke_generated_query_edited(query.into());
        main.invoke_focus_search();
        let _ = window.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn list_case_is_scoped_and_later_plain_searches_keep_the_window_choice() {
        use everygrep_core::query::{EvalScratch, Query};
        use everygrep_core::store::{Meta, NodeStore};

        let mut store = NodeStore::new();
        let root = store.add(None, b"/missing", true, Meta::default());
        let mixed = store.add(Some(root), b"Report.txt", false, Meta::default());
        let lower = store.add(Some(root), b"report.txt", false, Meta::default());
        for window_case in [false, true] {
            for list_case in [false, true] {
                for ignore_extension in [false, true] {
                    for contains in [false, true] {
                        let window = QueryOptions {
                            case_sensitive: window_case,
                            regex: true,
                            whole_words: true,
                            ..QueryOptions::default()
                        };
                        let list = Options {
                            case_sensitive: list_case,
                            ignore_extension,
                            contains,
                        };
                        let (text, after) = prepare_search("Report.txt", list, window).unwrap();
                        assert_eq!(
                            after,
                            QueryOptions {
                                regex: false,
                                ..window
                            }
                        );
                        let query = Query::parse(&text, after).unwrap();
                        assert!(query.matches(&store, mixed, &mut EvalScratch::default()));
                        assert_eq!(
                            query.matches(&store, lower, &mut EvalScratch::default()),
                            !list_case
                        );
                        // Replacing the generated query must use the unchanged window choice.
                        let plain = Query::parse("REPORT", after).unwrap();
                        assert_eq!(
                            plain.matches(&store, mixed, &mut EvalScratch::default()),
                            !window_case
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn imports_validate_utf8_content_and_limits_in_temporary_files() {
        struct Temp(std::path::PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Temp(std::env::temp_dir().join(format!(
                "everygrep-name-list-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        std::fs::create_dir(&dir.0).unwrap();
        let path = dir.0.join("names.txt");
        std::fs::write(&path, "\u{feff}Report.txt\r\nŞəkil.png\r\n").unwrap();
        assert_eq!(load(&path).unwrap(), "\u{feff}Report.txt\r\nŞəkil.png\r\n");
        for bytes in [b"".as_slice(), b"a/b\n", b"a\0b", b"\xff"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(load(&path).is_err());
        }
        let ten_thousand = (0..namelist::MAX_NAMES)
            .map(|n| format!("name-{n}.txt\n"))
            .collect::<String>();
        std::fs::write(&path, &ten_thousand).unwrap();
        assert_eq!(load(&path).unwrap(), ten_thousand);
        std::fs::write(&path, "name\n".repeat(namelist::MAX_NAMES + 1)).unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, vec![b'a'; namelist::MAX_BYTES + 1]).unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(load(&path).is_err());
        assert!(load(&dir.0).is_err());
    }
}
