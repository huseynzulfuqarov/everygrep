//! Advanced Search keeps a session draft and replaces the main field on Apply.
use crate::advanced_query::Fields;
use crate::saved_searches::NamedFilter;
use crate::{AdvancedWindow, AppWindow};
use everygrep_core::query::QueryOptions;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;

struct State {
    main: slint::Weak<AppWindow>,
    window: RefCell<Option<AdvancedWindow>>,
    filters: RefCell<Vec<NamedFilter>>,
}
thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }
fn with(f: impl FnOnce(&State)) {
    let state = STATE.with(|s| s.borrow().clone());
    if let Some(s) = state {
        f(&s);
    }
}
pub fn connect(main: &AppWindow) {
    STATE.with(|s| {
        *s.borrow_mut() = Some(Rc::new(State {
            main: main.as_weak(),
            window: RefCell::new(None),
            filters: RefCell::new(Vec::new()),
        }))
    });
    main.on_advanced_search(|| with(State::open));
}
pub fn apply_scheme(scheme: i32) {
    with(|s| {
        if let Some(w) = s.window.borrow().as_ref() {
            w.invoke_set_color_scheme(scheme);
        }
    });
}
impl State {
    fn open(&self) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        if self.window.borrow().is_none() {
            let w = match AdvancedWindow::new() {
                Ok(w) => w,
                Err(e) => {
                    crate::error(&main, e.to_string());
                    return;
                }
            };
            w.on_use_query(|| with(State::apply));
            w.on_choose_folder({
                let weak = w.as_weak();
                move || {
                    let Some(w) = weak.upgrade() else { return };
                    if w.get_choosing() {
                        return;
                    }
                    w.set_choosing(true);
                    let weak = w.as_weak();
                    std::thread::spawn(move || {
                        let result = crate::portal::choose_folder(
                            crate::i18n::tr("Located in"),
                            crate::labels::choose_accept(),
                        );
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            w.set_choosing(false);
                            match chosen_location(result) {
                                Ok(Some(path)) => {
                                    w.set_location(path.into());
                                    w.set_notice("".into());
                                }
                                Ok(None) => {}
                                Err(e) => w.set_notice(e.into()),
                            }
                        });
                    });
                }
            });
            w.on_close_window({
                let weak = w.as_weak();
                move || {
                    if let Some(w) = weak.upgrade() {
                        let _ = w.hide();
                    }
                }
            });
            *self.window.borrow_mut() = Some(w);
        }
        let filters = crate::saved_ui::filter_choices();
        let selected = crate::saved_ui::selected(&main);
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_filter_names(main.get_filter_names());
            w.set_filter_index(
                selected
                    .and_then(|f| filters.iter().position(|g| *g == f))
                    .map_or(-1, |i| i as i32),
            );
            w.set_match_case(main.get_match_case());
            w.set_whole_words(main.get_whole_words());
            w.set_diacritics(main.get_match_diacritics());
            w.set_notice("".into());
        }
        *self.filters.borrow_mut() = filters;
        crate::prefs::with(|p| apply_scheme(p.color_scheme()));
        if let Some(w) = self.window.borrow().as_ref() {
            // An open window may be behind the main window. As for the main
            // window (see `raise` in main.rs), showing it again gives a new
            // surface, which GNOME brings to the front with the focus.
            let _ = w.hide();
            let _ = w.show();
        }
    }
    fn apply(&self) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        let window = self.window.borrow();
        let Some(w) = window.as_ref() else { return };
        let fields = Fields {
            all: w.get_all_words().to_string(),
            phrase: w.get_phrase().to_string(),
            any: w.get_any_words().to_string(),
            none: w.get_none_words().to_string(),
            in_path: w.get_in_path(),
            location: w.get_location().to_string(),
            recursive: w.get_recursive(),
            kind: w.get_kind(),
            extensions: w.get_extensions().to_string(),
            min_size: w.get_min_size().to_string(),
            max_size: w.get_max_size().to_string(),
            size_unit: w.get_size_unit(),
            dates: [
                (
                    w.get_modified_from().to_string(),
                    w.get_modified_to().to_string(),
                ),
                (
                    w.get_created_from().to_string(),
                    w.get_created_to().to_string(),
                ),
                (
                    w.get_accessed_from().to_string(),
                    w.get_accessed_to().to_string(),
                ),
            ],
            content: w.get_content_text().to_string(),
            content_case: w.get_content_case(),
            content_whole_words: w.get_content_whole_words(),
            content_regex: w.get_content_regex(),
            content_encoding: w.get_content_encoding(),
            dupe_mode: w.get_dupe_mode(),
            dupe_by: w.get_dupe_by(),
        };
        let options = QueryOptions {
            case_sensitive: w.get_match_case(),
            whole_words: w.get_whole_words(),
            match_diacritics: w.get_diacritics(),
            hidden_files: crate::options(&main).hidden_files,
            and_before_or: main.get_and_before_or(),
            literal_operators: main.get_literal_operators(),
            ..QueryOptions::default()
        };
        let query = match fields.build(options) {
            Ok(q) => q,
            Err(e) => {
                w.set_notice(e.into());
                return;
            }
        };
        let filter = usize::try_from(w.get_filter_index())
            .ok()
            .and_then(|i| self.filters.borrow().get(i).cloned());
        main.invoke_history_record();
        crate::set_options(&main, options);
        main.set_wildcards_anywhere(false);
        if let Some(filter) = filter {
            crate::saved_ui::select_filter(&main, &filter);
        } else {
            main.set_filter_index(-1);
        }
        main.set_query(query.as_str().into());
        crate::show_badges(&main);
        main.invoke_generated_query_edited(query.into());
        main.invoke_focus_search();
        let _ = w.hide();
    }
}

#[cfg(test)]
mod tests {
    /// Enter in any text field of the form runs Use query, and the window
    /// opens low enough for a laptop screen of 810 px with its title bar.
    #[test]
    fn form_fits_and_enter_uses_the_query() {
        let source = include_str!("../ui/advanced.slint");
        let fields = source.matches("LineEdit {").count();
        assert!(fields > 10);
        assert_eq!(
            source.matches("accepted => { root.use-query(); }").count(),
            fields
        );
        let height = source
            .lines()
            .find_map(|l| l.trim().strip_prefix("preferred-height: "))
            .and_then(|v| v.strip_suffix("px;"))
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap();
        assert!(height <= 700, "{height}");
    }
}

fn chosen_location(
    result: Result<Option<std::path::PathBuf>, String>,
) -> Result<Option<String>, String> {
    result.and_then(|path| {
        path.map(|path| {
            path.into_os_string()
                .into_string()
                .map_err(|_| crate::i18n::tr("The folder name is not valid UTF-8.").to_owned())
        })
        .transpose()
    })
}

#[cfg(test)]
mod chooser_tests {
    #[test]
    fn choosing_preserves_literal_paths_and_cancellation_does_not_edit() {
        use super::chosen_location;
        assert_eq!(chosen_location(Ok(None)), Ok(None));
        assert_eq!(
            chosen_location(Ok(Some("/tmp/a * \"q".into()))),
            Ok(Some("/tmp/a * \"q".into()))
        );
        assert!(chosen_location(Err("failed".into())).is_err());
        use std::os::unix::ffi::OsStringExt;
        assert!(chosen_location(Ok(Some(std::ffi::OsString::from_vec(vec![255]).into()))).is_err());
    }
}
