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
use crate::saved_searches::{self, Bookmark, Kind, Layout, Library, NamedFilter, Target};
use crate::{AppWindow, BookmarkFolder, MenuEntry, SavedWindow, labels, settings};

enum CsvOutcome {
    Import(String),
    Exported,
}

fn csv_failure(import: bool, bookmarks: bool) -> &'static str {
    match (import, bookmarks) {
        (true, true) => tr("Cannot import bookmarks: {}"),
        (true, false) => tr("Cannot import filters: {}"),
        (false, true) => tr("Cannot export bookmarks: {}"),
        (false, false) => tr("Cannot export filters: {}"),
    }
}

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
    // The kind of row being edited (bookmarks), and what the Columns and
    // Sort choices of the editor stand for.
    draft_kind: Cell<Kind>,
    draft_columns: RefCell<ColumnChoices>,
    draft_sorts: RefCell<Vec<Option<i32>>>,
    // The filter whose layout was last put into the window, so a layout is
    // applied when a filter is chosen, not on every option change.
    applied_filter: Cell<Option<u64>>,
}

/// The editor's Columns choices: keep the window's, take the window's
/// current ones, or keep the stored ones (when they differ).
#[derive(Default)]
struct ColumnChoices {
    current: Vec<String>,
    stored: Option<Vec<String>>,
}

impl ColumnChoices {
    fn titles(&self) -> Vec<String> {
        let mut titles = vec![
            tr("Keep the window's columns").to_string(),
            fill(tr("Columns shown now: {}"), &[&column_list(&self.current)]),
        ];
        if let Some(stored) = self.stored.as_ref().filter(|s| **s != self.current) {
            titles.push(fill(tr("Saved columns: {}"), &[&column_list(stored)]));
        }
        titles
    }
    fn index(&self) -> i32 {
        match &self.stored {
            None => 0,
            Some(s) if *s == self.current => 1,
            Some(_) => 2,
        }
    }
    fn chosen(&self, index: i32) -> Option<Vec<String>> {
        match index {
            1 => Some(self.current.clone()),
            2 => self.stored.clone(),
            _ => None,
        }
    }
}

fn column_list(names: &[String]) -> String {
    if names.is_empty() {
        tr("standard only").into()
    } else {
        names.join(", ")
    }
}

/// Puts a stored set of optional columns into the window.
fn show_columns(w: &AppWindow, names: &[String]) {
    crate::tabs_ui::show_columns(w, names);
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
        draft_kind: Cell::new(Kind::Search),
        draft_columns: RefCell::new(ColumnChoices::default()),
        draft_sorts: RefCell::new(Vec::new()),
        applied_filter: Cell::new(None),
    });
    STATE.with(|s| *s.borrow_mut() = Some(state));
    let first = selected(w).map(|f| f.id);
    with(|s| s.applied_filter.set(first));
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
    w.on_find_filter(|| {
        with(|s| {
            s.open(false);
            if let Some(w) = s.window.borrow().as_ref() {
                w.invoke_focus_find();
            }
        });
    });
    w.on_find_bookmark(|| {
        with(|s| {
            s.open(true);
            if let Some(w) = s.window.borrow().as_ref() {
                w.invoke_focus_find();
            }
        });
    });
    w.on_append_bookmark(|i| {
        with(|s| {
            let entry = usize::try_from(i)
                .ok()
                .and_then(|i| s.library.borrow().bookmarks.get(i).cloned())
                .filter(Bookmark::is_search);
            if let Some(entry) = entry {
                s.append(&entry);
            }
        });
    });
    w.on_open_bookmark(|i| {
        with(|s| {
            let entry = usize::try_from(i)
                .ok()
                .and_then(|i| s.library.borrow().bookmarks.get(i).cloned())
                .filter(Bookmark::is_search);
            if let Some(entry) = entry {
                s.activate(&entry);
            }
        });
    });
    w.on_add_starter_bookmarks(|| {
        with(|s| {
            s.open(true);
            s.add_starter();
        });
    });
    w.on_saved_key(|text, ctrl, alt, shift, meta| {
        let chords = crate::physical::chords(&text, ctrl, alt, shift, meta);
        if chords.is_empty() {
            return false;
        }
        with(|s| {
            // Command bindings have priority even after an external settings edit.
            if commands::with_current(|k| k.bindings().iter().any(|b| chords.contains(&b.chord))) {
                return false;
            }
            let target = chords.iter().find_map(|chord| {
                s.library
                    .borrow()
                    .shortcuts
                    .iter()
                    .find(|key| key.chord == *chord)
                    .map(|key| key.target)
            });
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
                        .find(|b| b.id == id && b.is_search())
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
    with(|s| s.applied_filter.set(resolved.as_ref().map(|f| f.id)));
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
            crate::info(
                w,
                fill(
                    tr("Using the original built-in filter: {}."),
                    &[&original.title()],
                ),
            );
        }
    });
}

pub fn fill_filters(w: &AppWindow) {
    with(|s| {
        let l = s.library.borrow();
        let snapshot = s.snapshot.borrow();
        let entries: Vec<_> = l.filters.iter().chain(snapshot.iter()).collect();
        w.set_saved_copy_index(if snapshot.is_some() {
            l.filters.len() as i32
        } else {
            -1
        });
        w.set_filter_names(ModelRc::new(VecModel::from(
            entries
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    if i == l.filters.len() {
                        fill(tr("{} (saved copy)"), &[&f.title()]).into()
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
                            fill(tr("{} (saved copy)"), &[&f.title()]).into()
                        } else {
                            item_label(&f.title(), l.shortcut(Target::Filter(f.id))).into()
                        },
                        checked: Some(i) == current,
                        index: i as i32,
                    }
                })
                .collect::<Vec<_>>(),
        )));
        let (folders, top, more) = bookmark_menu(&l);
        w.set_more_bookmarks(more);
        w.set_bookmark_items(ModelRc::new(VecModel::from(top)));
        w.set_bookmark_folders(ModelRc::new(VecModel::from(folders)));
    });
}

/// The menu's title for a separator row: the menu shows it as a line.
/// (The toolkit marks its own separators with this title.)
const MENU_SEPARATOR: &str = "\u{E001}⸺";

fn menu_entry(l: &Library, i: usize) -> MenuEntry {
    let b = &l.bookmarks[i];
    MenuEntry {
        title: if b.kind == Kind::Separator {
            MENU_SEPARATOR.into()
        } else {
            item_label(&b.name, l.shortcut(Target::Bookmark(b.id))).into()
        },
        checked: false,
        index: i as i32,
    }
}

/// Folders become submenus (listed first), the other top-level rows follow
/// in their order; a long list shows its first part.
fn bookmark_menu(l: &Library) -> (Vec<BookmarkFolder>, Vec<MenuEntry>, bool) {
    let limit = crate::menus::SAVED;
    let folder_count = l
        .bookmarks
        .iter()
        .filter(|b| b.kind == Kind::Folder)
        .count();
    let mut more = folder_count > limit;
    let folders = l
        .bookmarks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.kind == Kind::Folder)
        .take(limit)
        .map(|(index, f)| {
            let all: Vec<_> = l
                .bookmarks
                .iter()
                .enumerate()
                .filter(|(_, b)| b.folder == f.id)
                .map(|(i, _)| menu_entry(l, i))
                .collect();
            let truncated = all.len() > limit;
            more |= truncated;
            BookmarkFolder {
                index: index as i32,
                title: f.name.as_str().into(),
                more: truncated,
                items: ModelRc::new(VecModel::from(
                    all.into_iter().take(limit).collect::<Vec<_>>(),
                )),
            }
        })
        .collect();
    let top: Vec<usize> = (0..l.bookmarks.len())
        .filter(|i| l.bookmarks[*i].folder == 0 && l.bookmarks[*i].kind != Kind::Folder)
        .collect();
    let shown = crate::menus::window(top.len(), None, crate::menus::SAVED);
    more |= shown.len() < top.len();
    let entries = top[shown].iter().map(|i| menu_entry(l, *i)).collect();
    (folders, entries, more)
}

/// Puts the chosen filter's columns and sort into the window when another
/// filter is chosen (not on every change of the matching options).
pub fn apply_filter_layout(w: &AppWindow) {
    let Some(filter) = selected(w) else { return };
    let changed = with(|s| s.applied_filter.replace(Some(filter.id)) != Some(filter.id));
    if changed != Some(true) {
        return;
    }
    apply_layout(w, &filter.layout);
}

fn apply_layout(w: &AppWindow, layout: &Layout) {
    if let Some(columns) = &layout.columns {
        show_columns(w, columns);
    }
    if let Some((column, descending)) = layout.sort {
        sort_by(w, column, descending);
    }
}

/// Sorts by `column`, showing it first when it is a hidden optional column.
fn sort_by(w: &AppWindow, column: i32, descending: bool) {
    match column {
        4 => w.set_show_type(true),
        5 => w.set_show_ext(true),
        6 => w.set_show_created(true),
        7 => w.set_show_accessed(true),
        8 => w.set_show_attrs(true),
        _ => {}
    }
    crate::apply_detail_columns(w);
    w.set_sort_column(column);
    w.set_sort_descending(descending);
    crate::sort_dialog::clear_then(w);
    w.invoke_sort_changed(column, descending);
}

/// Titles of the first nine sort columns (Name … Attributes).
fn base_sort_names() -> Vec<String> {
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
    .map(String::from)
    .collect()
}

fn kind_number(kind: Kind) -> i32 {
    match kind {
        Kind::Search => 0,
        Kind::Folder => 1,
        Kind::Separator => 2,
    }
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

pub fn apply_scheme(scheme: i32, style: i32) {
    with(|s| {
        if let Some(w) = s.window.borrow().as_ref() {
            w.invoke_set_color_scheme(scheme, style);
        }
    });
}

impl State {
    fn open(&self, bookmarks: bool) {
        if self.window.borrow().is_none() {
            let w = match SavedWindow::new() {
                Ok(w) => {
                    crate::physical::watch(w.window());
                    w
                }
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
                    if let Some(chord) = crate::physical::chord(&text, ctrl, alt, shift, meta) {
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
            w.on_add_starter(|| {
                with(State::add_starter);
            });
            w.on_new_group(|kind| {
                with(|s| {
                    s.new_group(if kind == 1 {
                        Kind::Folder
                    } else {
                        Kind::Separator
                    })
                });
            });
            w.on_import_csv(|| {
                with(|s| s.csv(true));
            });
            w.on_export_csv(|| {
                with(|s| s.csv(false));
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
                    if let Some(id) = s.current_id()
                        && s.change(|l| {
                            if s.bookmarks.get() {
                                l.move_bookmark(id, step)
                            } else {
                                l.move_filter(id, step)
                            }
                        })
                    {
                        s.select_row(id);
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
            w.on_find_changed(|| {
                with(|s| {
                    if let Some(w) = s.window.borrow().as_ref() {
                        w.set_current(-1);
                    }
                    s.fill();
                });
            });
            w.on_copy_search(|| {
                with(|s| {
                    if let Some(id) = s.current_id() {
                        let library = s.library.borrow();
                        if let Some(b) = library.bookmarks.iter().find(|b| b.id == id)
                            && let Some(main) = s.main.upgrade()
                        {
                            main.invoke_copy_text(b.search.as_str().into());
                        }
                    }
                });
            });
            w.on_append_entry(|| {
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
                            s.append(&b);
                        }
                    }
                });
            });
            w.on_open_all_entries(|| {
                with(|s| {
                    let needle = s
                        .window
                        .borrow()
                        .as_ref()
                        .map(|w| w.get_find_text())
                        .unwrap_or_default();
                    if let Some(main) = s.main.upgrade() {
                        main.invoke_open_all_bookmarks(needle);
                    }
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
        if self.bookmarks.replace(bookmarks) != bookmarks
            && let Some(w) = self.window.borrow().as_ref()
        {
            // The text found among bookmarks is not one to find among
            // filters.
            self.editing.set(None);
            self.draft_shortcut.set(None);
            self.draft_bookmark.borrow_mut().take();
            self.pending_delete.borrow_mut().take();
            *self.draft_columns.borrow_mut() = ColumnChoices::default();
            self.draft_sorts.borrow_mut().clear();
            *self.draft.borrow_mut() = QueryOptions::default();
            w.set_shortcut_text("".into());
            w.invoke_clear_editor();
            w.set_find_text("".into());
            w.set_current(-1);
            w.set_editing(false);
            w.set_confirming(false);
            w.set_deleting(false);
            w.set_resetting(false);
            w.set_notice("".into());
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
        crate::prefs::with(|p| apply_scheme(p.color_scheme(), p.dark_style()));
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
            let visible = self.ids();
            let values: Vec<_> = if self.bookmarks.get() {
                let depth: std::collections::HashMap<u64, u8> = l
                    .bookmark_tree()
                    .into_iter()
                    .map(|(i, d)| (l.bookmarks[i].id, d))
                    .collect();
                visible
                    .iter()
                    .filter_map(|id| l.bookmarks.iter().find(|b| b.id == *id))
                    .map(|b| {
                        let indent = if depth.get(&b.id) == Some(&1) {
                            "    "
                        } else {
                            ""
                        };
                        (format!("{indent}{}", b.manager_title()), b.search.clone())
                    })
                    .collect()
            } else {
                l.filters
                    .iter()
                    .filter(|f| visible.contains(&f.id))
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
            if w.get_current() < 0 && !w.get_editing() {
                w.invoke_clear_editor();
                w.set_shortcut_text("".into());
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
                kinds.push(fill(tr("{} (saved copy)"), &[&b.filter.title()]));
            }
            w.set_kinds(ModelRc::new(VecModel::from(
                kinds.into_iter().map(Into::into).collect::<Vec<_>>(),
            )));
            w.set_folder_names(ModelRc::new(VecModel::from(
                std::iter::once(tr("No folder").to_string())
                    .chain(l.bookmark_folders().iter().map(|f| f.name.clone()))
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )));
            w.set_row_kinds(ModelRc::new(VecModel::from(
                visible
                    .iter()
                    .map(|id| {
                        l.bookmarks
                            .iter()
                            .find(|b| b.id == *id)
                            .filter(|_| self.bookmarks.get())
                            .map_or(0, |b| kind_number(b.kind))
                    })
                    .collect::<Vec<_>>(),
            )));
            w.set_sort_names(ModelRc::new(VecModel::from(
                base_sort_names()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )));
        }
    }
    fn ids(&self) -> Vec<u64> {
        let l = self.library.borrow();
        let needle = self
            .window
            .borrow()
            .as_ref()
            .map(|w| w.get_find_text().to_string())
            .unwrap_or_default();
        if self.bookmarks.get() {
            if needle.trim().is_empty() {
                l.bookmark_tree()
                    .into_iter()
                    .map(|(i, _)| l.bookmarks[i].id)
                    .collect()
            } else {
                l.bookmarks
                    .iter()
                    .filter(|b| b.is_search() && bookmark_matches(&b.name, &b.search, &needle))
                    .map(|b| b.id)
                    .collect()
            }
        } else {
            l.filters
                .iter()
                .filter(|f| bookmark_matches(&f.title(), &f.search, &needle))
                .map(|f| f.id)
                .collect()
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
        self.begin_kind(new, Kind::Search);
    }

    /// The folder new rows go to: the selected folder, or the folder of the
    /// selected row.
    fn target_folder(&self) -> u64 {
        let l = self.library.borrow();
        self.current_id()
            .and_then(|id| l.bookmarks.iter().find(|b| b.id == id))
            .map_or(0, |b| {
                if b.kind == Kind::Folder {
                    b.id
                } else {
                    b.folder
                }
            })
    }

    fn new_group(&self, kind: Kind) {
        if kind == Kind::Separator {
            let folder = self.target_folder();
            let mut added = None;
            if self.change(|l| {
                added = Some(l.put_bookmark(None, Bookmark::group(Kind::Separator, "", folder))?);
                Ok(())
            }) && let Some(id) = added
            {
                self.select_row(id);
            }
        } else {
            self.begin_kind(true, kind);
        }
    }

    fn begin_kind(&self, new: bool, new_kind: Kind) {
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
                let folder = if new_kind == Kind::Folder {
                    0
                } else {
                    self.target_folder()
                };
                Some(if new_kind == Kind::Search {
                    Bookmark {
                        id: 0,
                        name: String::new(),
                        search: main.get_query().to_string(),
                        options: crate::options(&main),
                        filter: selected(&main)
                            .unwrap_or_else(|| Library::default().filters.remove(0)),
                        sort_column: main.get_sort_column(),
                        descending: main.get_sort_descending(),
                        columns: None,
                        kind: Kind::Search,
                        folder,
                    }
                } else {
                    Bookmark::group(new_kind, "", folder)
                })
            };
            let Some(entry) = entry else { return };
            if entry.kind == Kind::Separator {
                return;
            }
            self.draft_kind.set(entry.kind);
            *self.draft_columns.borrow_mut() = ColumnChoices {
                current: crate::tabs_ui::shown_columns(&main),
                stored: entry.columns.clone(),
            };
            if let Some(w) = self.window.borrow().as_ref() {
                w.set_entry_kind(kind_number(entry.kind));
                let folder = self
                    .library
                    .borrow()
                    .bookmark_folders()
                    .iter()
                    .position(|f| f.id == entry.folder)
                    .map_or(0, |i| i as i32 + 1);
                w.set_folder_choice(folder);
            }
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
                    layout: Layout::default(),
                })
            };
            let Some(entry) = entry else { return };
            self.draft_kind.set(Kind::Search);
            *self.draft_columns.borrow_mut() = ColumnChoices {
                current: crate::tabs_ui::shown_columns(&main),
                stored: entry.layout.columns.clone(),
            };
            let mut sorts: Vec<Option<i32>> =
                std::iter::once(None).chain((0..9).map(Some)).collect();
            if let Some((column, _)) = entry.layout.sort
                && !sorts.contains(&Some(column))
            {
                sorts.push(Some(column));
            }
            let sort_index = sorts
                .iter()
                .position(|c| *c == entry.layout.sort.map(|(c, _)| c))
                .unwrap_or(0);
            if let Some(w) = self.window.borrow().as_ref() {
                w.set_entry_kind(0);
                w.set_filter_sort(sort_index as i32);
                w.set_descending(entry.layout.sort.is_some_and(|(_, d)| d));
            }
            *self.draft_sorts.borrow_mut() = sorts;
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
        {
            let choices = self.draft_columns.borrow();
            w.set_column_choices(ModelRc::new(VecModel::from(
                choices
                    .titles()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )));
            w.set_column_choice(choices.index());
            let names = base_sort_names();
            w.set_filter_sorts(ModelRc::new(VecModel::from(
                self.draft_sorts
                    .borrow()
                    .iter()
                    .map(|c| match c {
                        None => tr("Keep the window's sort").to_string(),
                        Some(c) => names
                            .get(*c as usize)
                            .cloned()
                            .unwrap_or_else(|| fill(tr("Column {}"), &[&(c + 1)])),
                    })
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )));
        }
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
        let (name, text, mut o, kind, sort_column, descending, columns, folder_choice, sort_choice) = {
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
                self.draft_columns.borrow().chosen(w.get_column_choice()),
                w.get_folder_choice(),
                w.get_filter_sort(),
            )
        };
        let folder = usize::try_from(folder_choice - 1)
            .ok()
            .and_then(|i| {
                self.library
                    .borrow()
                    .bookmark_folders()
                    .get(i)
                    .map(|f| f.id)
            })
            .unwrap_or(0);
        let shortcut = self.draft_shortcut.get();
        let id = self.editing.get();
        let mut saved = None;
        let bookmark = if self.bookmarks.get() {
            let Some(mut b) = self.draft_bookmark.borrow().clone() else {
                return;
            };
            if b.kind != Kind::Search {
                b.name = name.to_string();
                b.folder = if b.kind == Kind::Folder { 0 } else { folder };
                let id = self.editing.get();
                let mut saved = None;
                if self.change(|l| {
                    saved = Some(l.put_bookmark(id, b)?);
                    Ok(())
                }) {
                    if let Some(id) = saved {
                        self.select_row(id);
                    }
                    if let Some(w) = self.window.borrow().as_ref() {
                        w.set_editing(false);
                    }
                }
                return;
            }
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
            b.columns = columns.clone();
            b.folder = folder;
            Some(b)
        } else {
            o.filter = Filter::ALL.get(kind as usize).copied().unwrap_or_default();
            None
        };
        if self.change(|l| {
            saved = Some(if let Some(b) = bookmark {
                l.put_bookmark(id, b)?
            } else {
                l.put_filter(
                    id,
                    &name,
                    &text,
                    o,
                    Layout {
                        columns,
                        sort: usize::try_from(sort_choice)
                            .ok()
                            .and_then(|i| self.draft_sorts.borrow().get(i).copied().flatten())
                            .map(|c| (c, descending)),
                    },
                )?
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
        if let Some(columns) = &b.columns {
            show_columns(&w, columns);
        }
        sort_by(&w, b.sort_column, b.descending);
        crate::typing::with(|t| t.searched());
        crate::search_now(&w, &self.engine);
        w.invoke_focus_search();
    }
    /// Adds the bookmark's search to the search field as one more
    /// condition and searches; the window's options and filter stay.
    fn append(&self, b: &Bookmark) {
        if !b.is_search() {
            return;
        }
        let Some(w) = self.main.upgrade() else { return };
        w.invoke_history_record();
        let text = saved_searches::append_search(&w.get_query(), &b.search, w.get_and_before_or());
        w.set_query(text.as_str().into());
        w.invoke_place_search_caret(i32::try_from(text.len()).unwrap_or(i32::MAX));
        crate::typing::with(|t| t.searched());
        crate::search_now(&w, &self.engine);
        w.invoke_focus_search();
    }
    fn message(&self, text: String, error: bool) {
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_notice(text.clone().into());
            w.set_notice_error(error);
        }
        if let Some(w) = self.main.upgrade() {
            if error {
                crate::error(&w, text);
            } else {
                crate::info(&w, text);
            }
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

    fn add_starter(&self) {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let Some(home) = home.filter(|h| h.is_absolute()) else {
            self.message(tr("The home folder is unknown.").into(), true);
            return;
        };
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|c| c.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let downloads = saved_searches::download_dir(Some(&config), &home);
        let mut added = 0;
        if self.change(|l| {
            added = l.add_starter_pack(&downloads)?;
            Ok(())
        }) {
            self.message(
                if added == 0 {
                    tr("The ready-made searches are already there.").into()
                } else {
                    fill(
                        tr("Added {} ready-made searches to the folder “{}”."),
                        &[&added, &tr("Ready-made searches")],
                    )
                },
                false,
            );
        }
    }

    /// Imports or exports the shown list (filters or bookmarks) as CSV.
    /// The file dialog runs off the UI thread; an import is applied under
    /// the library lock, all or nothing.
    fn csv(&self, import: bool) {
        let bookmarks = self.bookmarks.get();
        let snapshot = if bookmarks {
            crate::saved_csv::encode_bookmarks(&self.library.borrow())
        } else {
            crate::saved_csv::encode_filters(&self.library.borrow())
        };
        let title = match (import, bookmarks) {
            (true, true) => tr("Import bookmarks"),
            (true, false) => tr("Import filters"),
            (false, true) => tr("Export bookmarks"),
            (false, false) => tr("Export filters"),
        };
        let accept = if import { tr("Import") } else { tr("Export") };
        let file_name = if bookmarks {
            "bookmarks.csv"
        } else {
            "filters.csv"
        };
        let filter_name = tr("CSV files");
        if let Some(w) = self.window.borrow().as_ref() {
            w.set_notice("".into());
        }
        std::thread::spawn(move || {
            let filters = [crate::portal::SaveFilter {
                name: filter_name,
                pattern: "*.csv",
            }];
            // A cancelled chooser leaves the current status alone.
            let result: Result<Option<CsvOutcome>, String> = (|| {
                if import {
                    let Some(path) = crate::portal::open_file(title, accept, &filters)? else {
                        return Ok(None);
                    };
                    std::fs::read_to_string(&path)
                        .map(|text| Some(CsvOutcome::Import(text)))
                        .map_err(|e| e.to_string())
                } else {
                    let Some((path, _)) =
                        crate::portal::save_file(title, accept, file_name, &filters)?
                    else {
                        return Ok(None);
                    };
                    settings::write_atomic(&path, snapshot.as_bytes())
                        .map(|()| Some(CsvOutcome::Exported))
                        .map_err(|e| e.to_string())
                }
            })();
            let _ = slint::invoke_from_event_loop(move || {
                with(|s| match result {
                    Ok(Some(CsvOutcome::Import(text))) => s.import_text(&text, bookmarks),
                    Ok(Some(CsvOutcome::Exported)) => s.message(String::new(), false),
                    Ok(None) => {}
                    Err(e) => s.message(fill(csv_failure(import, bookmarks), &[&e]), true),
                });
            });
        });
    }

    fn import_text(&self, text: &str, bookmarks: bool) {
        let mut report = None;
        let changed = self.change_with_error(csv_failure(true, bookmarks), |l| {
            let incoming = if bookmarks {
                crate::saved_csv::decode_bookmarks(text, l)?
            } else {
                crate::saved_csv::decode_filters(text)?
            };
            report = Some(commands::with_current(|k| {
                crate::saved_csv::import(l, incoming, k)
            })?);
            Ok(())
        });
        if changed && let Some(report) = report {
            self.message(report.message(), false);
        }
    }

    fn change(&self, change: impl FnOnce(&mut Library) -> io::Result<()>) -> bool {
        self.change_with_error(tr("Cannot save searches: {}"), change)
    }

    fn change_with_error(
        &self,
        failure: &str,
        change: impl FnOnce(&mut Library) -> io::Result<()>,
    ) -> bool {
        let Some(path) = &self.path else {
            self.message(
                fill(failure, &[&tr("The configuration folder is unavailable.")]),
                true,
            );
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
                self.message(fill(failure, &[&e]), true);
                false
            }
        }
    }
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    #[test]
    fn csv_failures_name_the_operation_and_the_list() {
        for (import, bookmarks, expected) in [
            (true, true, "Cannot import bookmarks: bad CSV"),
            (true, false, "Cannot import filters: bad CSV"),
            (false, true, "Cannot export bookmarks: bad CSV"),
            (false, false, "Cannot export filters: bad CSV"),
        ] {
            assert_eq!(
                fill(csv_failure(import, bookmarks), &[&"bad CSV"]),
                expected
            );
        }
    }

    #[test]
    fn bookmark_quick_lists_bound_every_level_and_keep_original_indices() {
        use slint::Model;
        let limit = crate::menus::SAVED;
        let mut library = Library::default();
        for i in 0..limit + 2 {
            let mut folder = Bookmark::group(Kind::Folder, &format!("Folder {i}"), 0);
            folder.id = i as u64 + 1;
            library.bookmarks.push(folder);
        }
        for i in 0..limit + 3 {
            library
                .bookmarks
                .push(Bookmark::group(Kind::Search, &format!("Child {i}"), 1));
            library
                .bookmarks
                .push(Bookmark::group(Kind::Search, &format!("Top {i}"), 0));
        }
        let (folders, top, more) = bookmark_menu(&library);
        assert!(more);
        assert_eq!(folders.len(), limit);
        assert_eq!(top.len(), limit);
        assert!(folders[0].more);
        assert_eq!(folders[0].items.row_count(), limit);
        assert_eq!(
            folders[0].items.row_data(0).unwrap().index as usize,
            limit + 2
        );
        assert_eq!(top[0].index as usize, limit + 3);
        assert!(!folders[1].more);
        // An overflow in a folder alone must also expose the full manager.
        library
            .bookmarks
            .retain(|b| b.kind != Kind::Folder || b.id == 1);
        library
            .bookmarks
            .retain(|b| b.folder != 0 || b.kind == Kind::Folder);
        let (folders, top, more) = bookmark_menu(&library);
        assert!(more && folders[0].more && top.is_empty());
        library.bookmarks.truncate(1);
        let (folders, _, more) = bookmark_menu(&library);
        assert!(!more && !folders[0].more);
    }

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
            layout: Layout::default(),
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

fn bookmark_matches(name: &str, search: &str, needle: &str) -> bool {
    let fold = |text: &str| {
        everygrep_core::fold::strip_diacritics(&everygrep_core::fold::fold(text.as_bytes()))
    };
    let needle = fold(needle.trim());
    needle.is_empty()
        || [name, search]
            .iter()
            .any(|s| fold(s).windows(needle.len()).any(|w| w == needle))
}

#[cfg(test)]
mod find_tests {
    #[test]
    fn bookmarks_match_name_and_search_without_case_or_diacritics() {
        assert!(super::bookmark_matches("Şəkillər", "ext:png", "SEKILLER"));
        assert!(super::bookmark_matches("Photos", "ext:png", "PNG"));
        assert!(super::bookmark_matches("Photos", "ext:png", ""));
        assert!(!super::bookmark_matches("Photos", "ext:png", "pdf"));
    }
}

/// The bookmarks whose name or search holds `needle` (all for an empty
/// one), in their order.
/// The searches of the bookmark folder at `index`, to open them in tabs.
pub fn folder_bookmarks(index: i32) -> Vec<Bookmark> {
    let Ok(index) = usize::try_from(index) else {
        return Vec::new();
    };
    with(|s| {
        s.library
            .borrow()
            .folder_searches(index)
            .into_iter()
            .cloned()
            .collect()
    })
    .unwrap_or_default()
}

pub fn bookmarks_matching(needle: &str) -> Vec<Bookmark> {
    with(|s| {
        s.library
            .borrow()
            .bookmarks
            .iter()
            .filter(|b| b.is_search() && bookmark_matches(&b.name, &b.search, needle))
            .cloned()
            .collect()
    })
    .unwrap_or_default()
}

pub fn imported(library: Library) {
    with(|s| {
        s.replace(library);
        s.fill();
    });
}
