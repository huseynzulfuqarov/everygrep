//! The selector and menus share one collection of filters and bookmarks.
use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, QueryOptions};
use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};

use crate::engine::Engine;
use crate::i18n::{fill, tr};
use crate::saved_searches::{self, Bookmark, Library, NamedFilter};
use crate::{AppWindow, MenuChoice, SavedWindow, labels, settings};

struct State {
    main: slint::Weak<AppWindow>,
    engine: Arc<Engine>,
    path: Option<PathBuf>,
    library: RefCell<Library>,
    window: RefCell<Option<SavedWindow>>,
    bookmarks: Cell<bool>,
    editing: Cell<Option<u64>>,
    draft: RefCell<QueryOptions>,
    pending_delete: RefCell<Option<u64>>,
    draft_bookmark: RefCell<Option<Bookmark>>,
    // A frozen bookmark filter, shown in the same selector while in use.
    snapshot: RefCell<Option<NamedFilter>>,
}
thread_local! { static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) }; }
fn with<R>(f: impl FnOnce(&State) -> R) -> Option<R> {
    let state = STATE.with(|s| s.borrow().clone());
    state.as_deref().map(f)
}

pub fn connect(w: &AppWindow, engine: &Arc<Engine>) {
    let path = settings::config_dir().map(|d| d.join("saved-searches"));
    let loaded = path.as_deref().map(Library::load).transpose();
    let (library, error) = match loaded {
        Ok(l) => (l.unwrap_or_default(), None),
        Err(e) => (Library::default(), Some(e)),
    };
    let state = Rc::new(State {
        main: w.as_weak(),
        engine: engine.clone(),
        path,
        library: RefCell::new(library),
        window: RefCell::new(None),
        bookmarks: Cell::new(false),
        editing: Cell::new(None),
        draft: RefCell::new(QueryOptions::default()),
        pending_delete: RefCell::new(None),
        draft_bookmark: RefCell::new(None),
        snapshot: RefCell::new(None),
    });
    STATE.with(|s| *s.borrow_mut() = Some(state));
    w.on_add_filter(|| {
        with(|s| {
            s.open(false);
            s.begin(true);
        });
    });
    w.on_organize_filters(|| {
        with(|s| s.open(false));
    });
    w.on_restore_filters(|| {
        with(|s| {
            s.open(false);
            if let Some(w) = s.window.borrow().as_ref() {
                w.set_editing(false);
                w.set_confirming(true);
            }
        });
    });
    w.on_add_bookmark(|| {
        with(|s| {
            s.open(true);
            s.begin(true);
        });
    });
    w.on_organize_bookmarks(|| {
        with(|s| s.open(true));
    });
    w.on_open_bookmark(|i| {
        with(|s| {
            let entry = usize::try_from(i)
                .ok()
                .and_then(|i| s.library.borrow().bookmarks.get(i).cloned());
            if let Some(entry) = entry {
                s.activate(&entry);
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
        let l = s.library.borrow();
        l.filters.get(i).cloned().or_else(|| {
            (i == l.filters.len())
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
        let l = s.library.borrow();
        let snapshot = s.snapshot.borrow();
        let entries: Vec<_> = l.filters.iter().chain(snapshot.iter()).collect();
        w.set_filter_names(ModelRc::new(VecModel::from(
            entries.iter().map(|f| f.title().into()).collect::<Vec<_>>(),
        )));
        w.set_filter_items(ModelRc::new(VecModel::from(
            entries
                .iter()
                .enumerate()
                .map(|(i, f)| MenuChoice {
                    title: f.title().into(),
                    checked: i as i32 == w.get_filter_index(),
                })
                .collect::<Vec<_>>(),
        )));
        w.set_bookmark_names(ModelRc::new(VecModel::from(
            l.bookmarks
                .iter()
                .map(|b| b.name.as_str().into())
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
    fn open(&self, bookmarks: bool) {
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
                        let name = if s.bookmarks.get() {
                            s.library
                                .borrow()
                                .bookmarks
                                .iter()
                                .find(|b| b.id == id)
                                .map(|b| b.name.clone())
                        } else {
                            s.library
                                .borrow()
                                .filters
                                .iter()
                                .find(|f| f.id == id)
                                .map(NamedFilter::title)
                        };
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
                        s.change(|l| {
                            if s.bookmarks.get() {
                                l.remove_bookmark(id)
                            } else {
                                l.remove_filter(id)
                            }
                        });
                    }
                });
            });
            w.on_recover(|| {
                with(State::recover);
            });
            w.on_move_entry(|step| {
                with(|s| {
                    if let Some(id) = s.current_id() {
                        if s.change(|l| {
                            if s.bookmarks.get() {
                                l.move_bookmark(id, step)
                            } else {
                                l.move_filter(id, step)
                            }
                        }) {
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
            w.on_open_entry(|| {
                with(|s| {
                    if let Some(id) = s.current_id() {
                        let entry = s
                            .library
                            .borrow()
                            .bookmarks
                            .iter()
                            .find(|b| b.id == id)
                            .cloned();
                        if let Some(b) = entry {
                            s.activate(&b);
                            if let Some(w) = s.window.borrow().as_ref() {
                                let _ = w.hide();
                            }
                        }
                    }
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
        if self.bookmarks.replace(bookmarks) != bookmarks {
            if let Some(w) = self.window.borrow().as_ref() {
                w.set_current(-1);
                w.set_editing(false);
                w.set_confirming(false);
                w.set_deleting(false);
                w.set_resetting(false);
                w.set_notice("".into());
            }
        }
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_bookmarks(bookmarks);
        }
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

            let l = self.library.borrow();
            let values: Vec<_> = if self.bookmarks.get() {
                l.bookmarks
                    .iter()
                    .map(|b| (b.manager_title(), b.search.clone()))
                    .collect()
            } else {
                l.filters
                    .iter()
                    .map(|f| (f.manager_title(), f.search.clone()))
                    .collect()
            };
            let rows: Vec<ModelRc<StandardListViewItem>> = values
                .iter()
                .map(|(name, search)| {
                    ModelRc::new(VecModel::from(vec![
                        StandardListViewItem::from(name.as_str()),
                        StandardListViewItem::from(search.as_str()),
                    ]))
                })
                .collect();
            w.set_rows(ModelRc::new(VecModel::from(rows)));
            if w.get_current() >= values.len() as i32 {
                w.set_current(-1);
            }
            let mut kinds = if self.bookmarks.get() {
                l.filters.iter().map(NamedFilter::title).collect::<Vec<_>>()
            } else {
                Filter::ALL
                    .into_iter()
                    .map(|f| labels::filter(f).into())
                    .collect()
            };
            if self.bookmarks.get()
                && let Some(b) = self.draft_bookmark.borrow().as_ref()
            {
                kinds.push(fill(tr("{} (snapshot)"), &[&b.filter.title()]));
            }
            w.set_kinds(ModelRc::new(VecModel::from(
                kinds.into_iter().map(Into::into).collect::<Vec<_>>(),
            )));
            w.set_sort_names(ModelRc::new(VecModel::from(
                [
                    tr("Name"),
                    tr("Folder"),
                    tr("Size"),
                    tr("Modified"),
                    tr("Type"),
                    tr("Extension"),
                    tr("Created"),
                    tr("Accessed"),
                    tr("Attributes"),
                ]
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>(),
            )));
        }
    }
    fn ids(&self) -> Vec<u64> {
        let l = self.library.borrow();
        if self.bookmarks.get() {
            l.bookmarks.iter().map(|b| b.id).collect()
        } else {
            l.filters.iter().map(|f| f.id).collect()
        }
    }
    fn current_id(&self) -> Option<u64> {
        let index = self.window.borrow().as_ref()?.get_current();
        self.ids().get(usize::try_from(index).ok()?).copied()
    }
    fn select_row(&self, id: u64) {
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_current(
                self.ids()
                    .iter()
                    .position(|i| *i == id)
                    .map_or(-1, |i| i as i32),
            );
        }
    }
    fn begin(&self, new: bool) {
        let Some(main) = self.main.upgrade() else {
            return;
        };
        let id = if new {
            None
        } else {
            let Some(id) = self.current_id() else { return };
            Some(id)
        };
        self.editing.set(id);
        let (name, search, options, kind) = if self.bookmarks.get() {
            let entry = if let Some(id) = id {
                self.library
                    .borrow()
                    .bookmarks
                    .iter()
                    .find(|b| b.id == id)
                    .cloned()
            } else {
                Some(Bookmark {
                    id: 0,
                    name: String::new(),
                    search: main.get_query().to_string(),
                    options: crate::options(&main),
                    filter: selected(&main).unwrap_or_else(|| Library::default().filters.remove(0)),
                    sort_column: main.get_sort_column(),
                    descending: main.get_sort_descending(),
                })
            };
            let Some(entry) = entry else { return };
            let values = (
                entry.name.clone(),
                entry.search.clone(),
                entry.options,
                self.library.borrow().filters.len() as i32,
            );
            if let Some(w) = self.window.borrow().as_ref() {
                w.set_sort_column(entry.sort_column);
                w.set_descending(entry.descending);
            }
            *self.draft_bookmark.borrow_mut() = Some(entry);
            values
        } else {
            let entry = if let Some(id) = id {
                self.library
                    .borrow()
                    .filters
                    .iter()
                    .find(|f| f.id == id)
                    .cloned()
            } else {
                Some(NamedFilter {
                    id: 0,
                    name: String::new(),
                    search: main.get_query().to_string(),
                    options: QueryOptions {
                        hidden_files: HiddenFiles::Show,
                        ..crate::options(&main)
                    },
                })
            };
            let Some(entry) = entry else { return };
            let name = if new { String::new() } else { entry.title() };
            let kind = Filter::ALL
                .iter()
                .position(|f| *f == entry.options.filter)
                .unwrap_or(0) as i32;
            (name, entry.search, entry.options, kind)
        };
        *self.draft.borrow_mut() = options;
        self.fill();
        let window = self.window.borrow();
        let Some(w) = window.as_ref() else { return };
        w.set_entry_name(name.into());
        w.set_search_text(search.into());
        w.set_kind(kind);
        let o = options;
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
        let (name, text, mut o, kind, sort_column, descending) = {
            let window = self.window.borrow();
            let Some(w) = window.as_ref() else { return };
            if w.get_entry_name().trim().is_empty() {
                w.set_notice(tr("Enter a name.").into());
                w.set_notice_error(true);
                return;
            }
            let mut o = *self.draft.borrow();
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
            (
                w.get_entry_name(),
                w.get_search_text(),
                o,
                w.get_kind(),
                w.get_sort_column(),
                w.get_descending(),
            )
        };
        let id = self.editing.get();
        let mut saved = None;
        let bookmark = if self.bookmarks.get() {
            let Some(mut b) = self.draft_bookmark.borrow().clone() else {
                return;
            };
            if let Some(f) = usize::try_from(kind)
                .ok()
                .and_then(|i| self.library.borrow().filters.get(i).cloned())
            {
                b.filter = f;
            }
            o.filter = b.filter.options.filter;
            b.name = name.to_string();
            b.search = text.to_string();
            b.options = o;
            b.sort_column = sort_column;
            b.descending = descending;
            Some(b)
        } else {
            o.filter = Filter::ALL.get(kind as usize).copied().unwrap_or_default();
            None
        };
        if self.change(|l| {
            saved = Some(if let Some(b) = bookmark {
                l.put_bookmark(id, b)?
            } else {
                l.put_filter(id, &name, &text, o)?
            });
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
    fn activate(&self, b: &Bookmark) {
        let Some(w) = self.main.upgrade() else { return };
        w.invoke_history_record();
        let live = self
            .library
            .borrow()
            .filters
            .iter()
            .position(|f| *f == b.filter);
        w.set_query(b.search.as_str().into());
        w.set_hidden_files(
            everygrep_core::query::HiddenFiles::ALL
                .iter()
                .position(|h| *h == b.options.hidden_files)
                .unwrap_or(0) as i32,
        );
        w.set_literal_operators(b.options.literal_operators);
        w.set_and_before_or(b.options.and_before_or);
        w.set_wildcards_anywhere(b.options.wildcards_anywhere);
        crate::set_options(&w, b.options);
        *self.snapshot.borrow_mut() = live.is_none().then(|| b.filter.clone());
        w.set_notice("".into());
        w.set_filter_index(live.unwrap_or(self.library.borrow().filters.len()) as i32);
        crate::show_badges(&w);
        match b.sort_column {
            4 => w.set_show_type(true),
            5 => w.set_show_ext(true),
            6 => w.set_show_created(true),
            7 => w.set_show_accessed(true),
            8 => w.set_show_attrs(true),
            _ => {}
        }
        crate::apply_detail_columns(&w);
        w.set_sort_column(b.sort_column);
        w.set_sort_descending(b.descending);
        w.invoke_sort_changed(b.sort_column, b.descending);
        crate::typing::with(|t| t.searched());
        crate::search_now(&w, &self.engine);
        w.invoke_focus_search();
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
                .and_then(|f| library.filters.iter().position(|g| g.id == f.id))
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
