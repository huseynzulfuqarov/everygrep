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
