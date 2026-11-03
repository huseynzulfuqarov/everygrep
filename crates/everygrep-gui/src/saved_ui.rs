//! The selector and menus share one collection of filters and bookmarks.
use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, QueryOptions};
use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};

use crate::commands::{self, Chord};
use crate::engine::Engine;
use crate::i18n::{fill, tr};
use crate::saved_searches::{self, Bookmark, Library, NamedFilter, Target};
use crate::{AppWindow, MenuEntry, SavedWindow, labels, settings};

struct State {
    main: slint::Weak<AppWindow>,
    engine: Arc<Engine>,
    path: Option<PathBuf>,
    library: RefCell<Library>,
    window: RefCell<Option<SavedWindow>>,
    bookmarks: Cell<bool>,
    editing: Cell<Option<u64>>,
    draft: RefCell<QueryOptions>,
    draft_shortcut: Cell<Option<Chord>>,
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
    let conflicts = commands::with_current(|keys| {
        library
            .shortcuts
            .iter()
            .filter_map(|key| {
                let name = library.target_name(key.target)?;
                shortcut_conflict(&name, key.chord, keys)
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    let state = Rc::new(State {
        main: w.as_weak(),
        engine: engine.clone(),
        path,
        library: RefCell::new(library),
        window: RefCell::new(None),
        bookmarks: Cell::new(false),
        editing: Cell::new(None),
        draft: RefCell::new(QueryOptions::default()),
        draft_shortcut: Cell::new(None),
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
    w.on_saved_key(|text, ctrl, alt, shift, meta| {
        let Some(chord) = Chord::from_event(&text, ctrl, alt, shift, meta) else {
            return false;
        };
        with(|s| {
            // Command bindings have priority even after an external settings edit.
            if commands::with_current(|k| k.bindings().iter().any(|b| b.chord == chord)) {
                return false;
            }
            let target = s
                .library
                .borrow()
                .shortcuts
                .iter()
                .find(|key| key.chord == chord)
                .map(|key| key.target);
            match target {
                Some(Target::Filter(id)) => {
                    if let Some(w) = s.main.upgrade() {
                        let i = s.library.borrow().filters.iter().position(|f| f.id == id);
                        if let Some(i) = i {
                            w.set_filter_index(i as i32);
                            w.invoke_options_changed();
                        }
                    }
                    true
                }
                Some(Target::Bookmark(id)) => {
                    let b = s
                        .library
                        .borrow()
                        .bookmarks
                        .iter()
                        .find(|b| b.id == id)
                        .cloned();
                    if let Some(b) = b {
                        s.activate(&b);
                    }
                    true
                }
                None => false,
            }
        })
        .unwrap_or(false)
    });
    if let Some(e) = error {
        crate::error(w, fill(tr("Cannot read saved searches: {}"), &[&e]));
    } else if !conflicts.is_empty() {
        crate::error(w, conflicts);
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

pub fn filter_choices() -> Vec<NamedFilter> {
    with(|s| {
        s.library
            .borrow()
            .filters
            .iter()
            .chain(s.snapshot.borrow().iter())
            .cloned()
            .collect()
    })
    .unwrap_or_default()
}

pub fn using_snapshot(w: &AppWindow) -> bool {
    with(|s| {
        w.get_filter_index() as usize == s.library.borrow().filters.len()
            && s.snapshot.borrow().is_some()
    })
    .unwrap_or(false)
}

pub fn select_snapshot(w: &AppWindow, filter: &NamedFilter) {
    with(|s| {
        *s.snapshot.borrow_mut() = Some(filter.clone());
        w.set_filter_index(s.library.borrow().filters.len() as i32);
        fill_filters(w);
    });
}

pub fn tab_filter(
    w: &AppWindow,
    filter: Option<&NamedFilter>,
    frozen: bool,
) -> Option<NamedFilter> {
    let resolved = with(|s| {
        let resolved = filter.map(|f| resolve_tab_filter(&s.library.borrow().filters, f, frozen));
        *s.snapshot.borrow_mut() = None;
        resolved
    })
    .flatten();
    if let Some(filter) = &resolved {
        if frozen {
            select_snapshot(w, filter);
        } else {
            select_filter(w, filter);
        }
    } else {
        fill_filters(w);
    }
    resolved
}

pub fn filter_for_tab(filter: &NamedFilter, frozen: bool) -> NamedFilter {
    with(|s| resolve_tab_filter(&s.library.borrow().filters, filter, frozen))
        .unwrap_or_else(|| filter.clone())
}

fn resolve_tab_filter(library: &[NamedFilter], filter: &NamedFilter, frozen: bool) -> NamedFilter {
    if frozen {
        return filter.clone();
    }
    library
        .iter()
        .find(|f| f.id == filter.id)
        .unwrap_or(filter)
        .clone()
}

pub fn select_filter(w: &AppWindow, filter: &NamedFilter) {
    with(|s| {
        let live = s.library.borrow().filters.iter().position(|f| f == filter);
        *s.snapshot.borrow_mut() = live.is_none().then(|| filter.clone());
        w.set_filter_index(live.unwrap_or(s.library.borrow().filters.len()) as i32);
        fill_filters(w);
    });
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
            entries
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    if i == l.filters.len() {
                        fill(tr("{} (snapshot)"), &[&f.title()]).into()
                    } else {
                        f.title().into()
                    }
                })
                .collect::<Vec<_>>(),
        )));
        let current = usize::try_from(w.get_filter_index()).ok();
        let shown = crate::menus::window(entries.len(), current, crate::menus::SAVED);
        w.set_more_filters(shown.len() < entries.len());
        w.set_filter_items(ModelRc::new(VecModel::from(
            shown
                .map(|i| {
                    let f = entries[i];
                    MenuEntry {
                        title: if i == l.filters.len() {
                            fill(tr("{} (snapshot)"), &[&f.title()]).into()
                        } else {
                            item_label(&f.title(), l.shortcut(Target::Filter(f.id))).into()
                        },
                        checked: Some(i) == current,
                        index: i as i32,
                    }
                })
                .collect::<Vec<_>>(),
        )));
        let shown = crate::menus::window(l.bookmarks.len(), None, crate::menus::SAVED);
        w.set_more_bookmarks(shown.len() < l.bookmarks.len());
        w.set_bookmark_items(ModelRc::new(VecModel::from(
            l.bookmarks[shown.clone()]
                .iter()
                .zip(shown)
                .map(|(b, i)| MenuEntry {
                    title: item_label(&b.name, l.shortcut(Target::Bookmark(b.id))).into(),
                    checked: false,
                    index: i as i32,
                })
                .collect::<Vec<_>>(),
        )));
    });
}

fn item_label(name: &str, chord: Option<Chord>) -> String {
    chord.map_or_else(|| name.into(), |c| format!("{name} ({})", c.display()))
}

/// Also used by keyboard settings, so conflicts are checked in both directions.
pub fn shortcut_owner(chord: Chord) -> Option<String> {
    with(|s| {
        let disk = s.path.as_deref().and_then(|p| Library::load(p).ok());
        disk.as_ref()
            .unwrap_or(&s.library.borrow())
            .shortcut_owner(None, chord)
    })
    .flatten()
}

fn shortcut_conflict(name: &str, chord: Chord, keys: &commands::Keymap) -> Option<String> {
    let owners = keys
        .bindings()
        .iter()
        .filter(|b| b.chord == chord)
        .filter_map(|b| commands::spec(b.cmd).map(|s| (s.label)()))
        .collect::<Vec<_>>();
    (!owners.is_empty()).then(|| {
        format!(
            "{name}: {}",
            fill(tr("Shortcut already used by {}."), &[&owners.join(", ")])
        )
    })
}

pub fn shortcut_rows() -> Vec<crate::ShortcutRow> {
    with(|s| {
        let l = s.library.borrow();
        l.shortcuts
            .iter()
            .filter_map(|key| {
                l.target_name(key.target).map(|name| crate::ShortcutRow {
                    keys: key.chord.display().into(),
                    action: commands::with_current(|keys| {
                        shortcut_conflict(&name, key.chord, keys)
                    })
                    .unwrap_or(name)
                    .into(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
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
            w.on_capture_shortcut(|text, ctrl, alt, shift, meta| {
                with(|s| {
                    if let Some(chord) = Chord::from_event(&text, ctrl, alt, shift, meta) {
                        s.draft_shortcut.set(Some(chord));
                        if let Some(w) = s.window.borrow().as_ref() {
                            w.set_shortcut_text(chord.display().into());
                        }
                    }
                });
            });
            w.on_clear_shortcut(|| {
                with(|s| {
                    s.draft_shortcut.set(None);
                    if let Some(w) = s.window.borrow().as_ref() {
                        w.set_shortcut_text("".into());
                    }
                });
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
            // An open window may be behind the main window; showing it again
            // gives a new surface, which GNOME brings to the front (see `raise`
            // in main.rs).
            let _ = w.hide();
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
        let target = id.map(|id| {
            if self.bookmarks.get() {
                Target::Bookmark(id)
            } else {
                Target::Filter(id)
            }
        });
        let key = target.and_then(|t| self.library.borrow().shortcut(t));
        self.draft_shortcut.set(key);
        w.set_shortcut_text(key.map_or_else(String::new, |c| c.display()).into());
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
        let shortcut = self.draft_shortcut.get();
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
            let target = if self.bookmarks.get() {
                Target::Bookmark(saved.expect("saved identity"))
            } else {
                Target::Filter(saved.expect("saved identity"))
            };
            commands::with_current(|k| l.set_shortcut(target, shortcut, k))?;
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
        select_snapshot(&w, &b.filter);
        w.set_notice("".into());
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
                if let Some(w) = self.main.upgrade() {
                    crate::apply_keys(&w);
                }
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

#[cfg(test)]
mod tab_tests {
    use super::*;

    #[test]
    fn loaded_saved_shortcut_conflicts_are_visible_in_keyboard_rows() {
        let chord = Chord::parse("Ctrl+1").unwrap();
        let message =
            shortcut_conflict("My bookmark", chord, &commands::Keymap::default()).unwrap();
        assert!(message.contains("My bookmark"));
        assert!(message.contains("Select tab 1"));
        assert!(
            shortcut_conflict(
                "Free",
                Chord::parse("Ctrl+Alt+9").unwrap(),
                &commands::Keymap::default()
            )
            .is_none()
        );
    }

    #[test]
    fn ordinary_tabs_follow_live_filter_ids_and_keep_deleted_snapshots() {
        let original = NamedFilter {
            id: 42,
            name: "Docs".into(),
            search: "ext:pdf".into(),
            options: QueryOptions::default(),
        };
        let edited = NamedFilter {
            search: "ext:pdf;odt".into(),
            ..original.clone()
        };
        assert_eq!(
            resolve_tab_filter(std::slice::from_ref(&edited), &original, false),
            edited
        );
        assert_eq!(resolve_tab_filter(&[], &original, false), original);
        assert_eq!(
            resolve_tab_filter(std::slice::from_ref(&edited), &original, true),
            original
        );
        let mut changed_kind = edited.clone();
        changed_kind.options.filter = Filter::Audio;
        let tab = crate::tabs::SearchState {
            options: QueryOptions {
                filter: Filter::Document,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            tab.effective_options(QueryOptions::default(), Some(&changed_kind))
                .filter,
            Filter::Audio
        );
    }
}
