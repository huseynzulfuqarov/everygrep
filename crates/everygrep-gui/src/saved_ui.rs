//! The existing filter selector and Search menu share this one collection.
use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, QueryOptions};
use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};

use crate::engine::Engine;
use crate::i18n::{fill, tr};
use crate::saved_searches::{self, Library, NamedFilter};
use crate::{AppWindow, MenuChoice, SavedWindow, labels, settings};

struct State {
    main: slint::Weak<AppWindow>,
    path: Option<PathBuf>,
    library: RefCell<Library>,
    window: RefCell<Option<SavedWindow>>,
    editing: RefCell<Option<u64>>,
    draft: RefCell<QueryOptions>,
    pending_delete: RefCell<Option<u64>>,
    snapshot: RefCell<Option<NamedFilter>>,
}
thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }
fn with<R>(f: impl FnOnce(&State) -> R) -> Option<R> {
    let state = STATE.with(|s| s.borrow().clone());
    state.as_deref().map(f)
}

pub fn connect(w: &AppWindow, _engine: &Arc<Engine>) {
    let path = settings::config_dir().map(|d| d.join("saved-searches"));
    let loaded = path.as_deref().map(Library::load).transpose();
    let (library, error) = match loaded {
        Ok(l) => (l.unwrap_or_default(), None),
        Err(e) => (Library::default(), Some(e)),
    };
    let state = Rc::new(State {
        main: w.as_weak(),
        path,
        library: RefCell::new(library),
        window: RefCell::new(None),
        editing: RefCell::new(None),
        draft: RefCell::new(QueryOptions::default()),
        pending_delete: RefCell::new(None),
        snapshot: RefCell::new(None),
    });
    STATE.with(|s| *s.borrow_mut() = Some(state));
    w.on_add_filter(|| {
        with(|s| {
            s.open();
            s.begin(true);
        });
    });
    w.on_organize_filters(|| {
        with(State::open);
    });
    w.on_restore_filters(|| {
        with(|s| {
            s.open();
            if let Some(w) = s.window.borrow().as_ref() {
                w.set_confirming(true);
            }
        });
    });
    if let Some(e) = error {
        crate::error(w, fill(tr("Cannot read saved searches: {}"), &[&e]));
    }
}

pub fn selected(w: &AppWindow) -> Option<NamedFilter> {
    with(|s| {
        let i = usize::try_from(w.get_filter_index()).ok()?;
        let library = s.library.borrow();
        library.filters.get(i).cloned().or_else(|| {
            (i == library.filters.len())
                .then(|| s.snapshot.borrow().clone())
                .flatten()
        })
    })
    .flatten()
}

pub fn select_builtin(w: &AppWindow, filter: Filter) {
    with(|s| {
        let (original, index) = s.library.borrow().builtin_selection(filter);
        *s.snapshot.borrow_mut() = index.is_none().then(|| original.clone());
        w.set_filter_index(index.unwrap_or(s.library.borrow().filters.len()) as i32);
        fill_filters(w);
        if index.is_none() {
            w.set_notice(
                fill(
                    tr("Using the original built-in filter: {}."),
                    &[&original.title()],
                )
                .into(),
            );
            w.set_notice_error(false);
        }
    });
}

pub fn fill_filters(w: &AppWindow) {
    with(|s| {
        let library = s.library.borrow();
        let snapshot = s.snapshot.borrow();
        let entries: Vec<_> = library.filters.iter().chain(snapshot.iter()).collect();
        let title = |i: usize, f: &NamedFilter| {
            if i == library.filters.len() {
                fill(tr("{} (snapshot)"), &[&f.title()])
            } else {
                f.title()
            }
        };
        w.set_filter_names(ModelRc::new(VecModel::from(
            entries
                .iter()
                .enumerate()
                .map(|(i, f)| title(i, f).into())
                .collect::<Vec<_>>(),
        )));
        w.set_filter_items(ModelRc::new(VecModel::from(
            entries
                .iter()
                .enumerate()
                .map(|(i, f)| MenuChoice {
                    title: title(i, f).into(),
                    checked: i as i32 == w.get_filter_index(),
                })
                .collect::<Vec<_>>(),
        )));
    });
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
        if self.window.borrow().is_none() {
            let w = match SavedWindow::new() {
                Ok(w) => w,
                Err(e) => {
                    if let Some(m) = self.main.upgrade() {
                        crate::error(&m, e.to_string());
                    }
                    return;
                }
            };
            w.on_begin_edit(|new| {
                with(|s| s.begin(new));
            });
            w.on_save_entry(|| {
                with(State::save);
            });
            w.on_request_remove(|| {
                with(|s| {
                    if let Some(id) = s.current_id() {
                        let name = s
                            .library
                            .borrow()
                            .filters
                            .iter()
                            .find(|f| f.id == id)
                            .map(NamedFilter::title);
                        if let (Some(name), Some(w)) = (name, s.window.borrow().as_ref()) {
                            *s.pending_delete.borrow_mut() = Some(id);
                            w.set_confirm_name(name.into());
                            w.set_deleting(true);
                        }
                    }
                });
            });
            w.on_remove_entry(|| {
                with(|s| {
                    let id = s.pending_delete.borrow_mut().take();
                    if let Some(id) = id {
                        s.change(|l| l.remove_filter(id));
                    }
                });
            });
            w.on_recover(|| {
                with(State::recover);
            });
            w.on_move_entry(|step| {
                with(|s| {
                    if let Some(id) = s.current_id() {
                        if s.change(|l| l.move_filter(id, step)) {
                            s.select_row(id);
                        }
                    }
                });
            });
            w.on_restore(|| {
                with(|s| {
                    s.change(|l| {
                        l.restore_filters();
                        Ok(())
                    });
                });
            });
            w.on_close_window({
                let weak = w.as_weak();
                move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_editing(false);
                        w.set_deleting(false);
                        w.set_resetting(false);
                        w.set_confirming(false);
                        let _ = w.hide();
                    }
                }
            });
            *self.window.borrow_mut() = Some(w);
        }
        // Reload without writing when another window has edited the list.
        if let Some(path) = &self.path {
            match Library::load(path) {
                Ok(l) => {
                    self.replace(l);
                    if let Some(w) = self.window.borrow().as_ref() {
                        w.set_recoverable(false);
                    }
                }
                Err(e) => {
                    self.message(fill(tr("Cannot read saved searches: {}"), &[&e]), true);
                    if let Some(w) = self.window.borrow().as_ref() {
                        w.set_recoverable(true);
                    }
                }
            }
        }
        self.fill();
        crate::prefs::with(|p| apply_scheme(p.color_scheme()));
        if let Some(w) = self.window.borrow().as_ref() {
            let _ = w.show();
        }
    }

    fn fill(&self) {
        if let Some(m) = self.main.upgrade() {
            fill_filters(&m);
        }
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_hidden_names(ModelRc::new(VecModel::from(
                HiddenFiles::ALL
                    .into_iter()
                    .map(|h| labels::hidden_files(h).into())
                    .collect::<Vec<_>>(),
            )));

            let rows: Vec<ModelRc<StandardListViewItem>> = self
                .library
                .borrow()
                .filters
                .iter()
                .map(|f| {
                    ModelRc::new(VecModel::from(vec![
                        StandardListViewItem::from(f.manager_title().as_str()),
                        StandardListViewItem::from(f.search.as_str()),
                    ]))
                })
                .collect();
            w.set_rows(ModelRc::new(VecModel::from(rows)));
            if w.get_current() >= self.library.borrow().filters.len() as i32 {
                w.set_current(-1);
            }
            w.set_kinds(ModelRc::new(VecModel::from(
                Filter::ALL
                    .into_iter()
                    .map(|f| labels::filter(f).into())
                    .collect::<Vec<_>>(),
            )));
        }
    }

    fn current_id(&self) -> Option<u64> {
        let index = self.window.borrow().as_ref()?.get_current();
        self.library
            .borrow()
            .filters
            .get(usize::try_from(index).ok()?)
            .map(|f| f.id)
    }
    fn select_row(&self, id: u64) {
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_current(
                self.library
                    .borrow()
                    .filters
                    .iter()
                    .position(|f| f.id == id)
                    .map_or(-1, |i| i as i32),
            );
        }
    }
    fn begin(&self, new: bool) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        let entry = if new {
            NamedFilter {
                id: 0,
                name: String::new(),
                search: main.get_query().to_string(),
                options: QueryOptions {
                    hidden_files: HiddenFiles::Show,
                    ..crate::options(&main)
                },
            }
        } else {
            let Some(id) = self.current_id() else { return };
            let Some(f) = self
                .library
                .borrow()
                .filters
                .iter()
                .find(|f| f.id == id)
                .cloned()
            else {
                return;
            };
            f
        };
        *self.editing.borrow_mut() = (!new).then_some(entry.id);
        *self.draft.borrow_mut() = entry.options;
        let window = self.window.borrow();
        let Some(w) = window.as_ref() else { return };
        w.set_entry_name(if new { "".into() } else { entry.title().into() });
        w.set_search_text(entry.search.into());
        let o = entry.options;
        w.set_kind(Filter::ALL.iter().position(|f| *f == o.filter).unwrap_or(0) as i32);
        w.set_match_case(o.case_sensitive);
        w.set_whole_words(o.whole_words);
        w.set_match_path(o.match_path);
        w.set_diacritics(o.match_diacritics);
        w.set_prefix(o.prefix);
        w.set_suffix(o.suffix);
        w.set_punctuation(o.ignore_punctuation);
        w.set_whitespace(o.ignore_whitespace);
        w.set_regex(o.regex);
        w.set_hidden_mode(
            HiddenFiles::ALL
                .iter()
                .position(|h| *h == o.hidden_files)
                .unwrap_or(0) as i32,
        );
        w.set_literal_operators(o.literal_operators);
        w.set_and_before_or(o.and_before_or);
        w.set_wildcards_anywhere(o.wildcards_anywhere);
        w.set_editing(true);
        w.set_confirming(false);
        w.set_notice("".into());
        w.invoke_focus_name();
    }

    fn save(&self) {
        let (name, text, o) = {
            let window = self.window.borrow();
            let Some(w) = window.as_ref() else { return };
            if w.get_entry_name().trim().is_empty() {
                w.set_notice(tr("Enter a name.").into());
                w.set_notice_error(true);
                return;
            }
            let mut o = *self.draft.borrow();
            o.filter = Filter::ALL
                .get(w.get_kind() as usize)
                .copied()
                .unwrap_or_default();
            o.case_sensitive = w.get_match_case();
            o.whole_words = w.get_whole_words();
            o.match_path = w.get_match_path();
            o.match_diacritics = w.get_diacritics();
            o.prefix = w.get_prefix();
            o.suffix = w.get_suffix();
            o.ignore_punctuation = w.get_punctuation();
            o.ignore_whitespace = w.get_whitespace();
            o.regex = w.get_regex();
            o.hidden_files = HiddenFiles::ALL
                .get(w.get_hidden_mode() as usize)
                .copied()
                .unwrap_or_default();
            o.literal_operators = w.get_literal_operators();
            o.and_before_or = w.get_and_before_or();
            o.wildcards_anywhere = w.get_wildcards_anywhere();
            (w.get_entry_name(), w.get_search_text(), o)
        };
        let id = *self.editing.borrow();
        let mut saved = None;
        if self.change(|l| {
            saved = Some(l.put_filter(id, &name, &text, o)?);
            Ok(())
        }) {
            if let Some(id) = saved {
                self.select_row(id);
            }
            if let Some(w) = self.window.borrow().as_ref() {
                w.set_editing(false);
            }
        }
    }
    fn message(&self, text: String, error: bool) {
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_notice(text.clone().into());
            w.set_notice_error(error);
        }
        if error && let Some(w) = self.main.upgrade() {
            crate::error(&w, text);
        }
    }
    fn replace(&self, library: Library) {
        let old = self.main.upgrade().and_then(|w| selected(&w));
        let using_snapshot = self.main.upgrade().is_some_and(|w| {
            w.get_filter_index() as usize == self.library.borrow().filters.len()
                && self.snapshot.borrow().is_some()
        });
        let (fallback, fallback_index) = library.builtin_selection(Filter::All);
        let next = if using_snapshot {
            Some(library.filters.len())
        } else {
            old.as_ref()
                .and_then(|entry| library.filters.iter().position(|f| f.id == entry.id))
                .or(fallback_index)
        };
        if next.is_none() {
            *self.snapshot.borrow_mut() = Some(fallback);
        }
        let next = next.unwrap_or(library.filters.len());
        *self.library.borrow_mut() = library;
        if let Some(w) = self.main.upgrade() {
            w.set_filter_index(next as i32);
            fill_filters(&w);
            crate::show_badges(&w);
            if selected(&w) != old {
                w.invoke_options_changed();
            }
        }
    }
    fn recover(&self) {
        let Some(path) = &self.path else {
            return;
        };
        match saved_searches::recover(path) {
            Ok((library, backup)) => {
                self.replace(library);
                self.fill();
                if let Some(w) = self.window.borrow().as_ref() {
                    w.set_recoverable(false);
                    w.set_current(-1);
                }
                self.message(
                    fill(
                        tr("Saved searches reset. A copy was saved to {}."),
                        &[&backup.display()],
                    ),
                    false,
                );
            }
            Err(e) => self.message(fill(tr("Cannot save searches: {}"), &[&e]), true),
        }
    }

    fn change(&self, change: impl FnOnce(&mut Library) -> io::Result<()>) -> bool {
        let Some(path) = &self.path else {
            self.message(tr("The configuration folder is unavailable.").into(), true);
            return false;
        };
        match saved_searches::update(path, change) {
            Ok((library, ())) => {
                self.replace(library);
                self.fill();
                self.message(String::new(), false);
                true
            }
            Err(e) => {
                self.message(fill(tr("Cannot save searches: {}"), &[&e]), true);
                false
            }
        }
    }
}
