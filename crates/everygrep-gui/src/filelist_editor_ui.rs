//! Index → File list editor: the window over [`crate::filelist_editor`].
//! Its keys (Ctrl+N/O/S/Shift+S/W) are handled by the window itself, so
//! they never reach the main window, whose Ctrl+N and Ctrl+S keep their
//! own meaning.

use std::cell::RefCell;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};

use crate::FileListEditor;
use crate::filelist_editor::{EditorList, SaveError};
use crate::i18n::{fill, tr};

/// What waits for the user's answer.
#[derive(Clone)]
enum Pending {
    /// Leave the unsaved list for this.
    Discard(Then),
    /// Replace a file that changed on disk (or another existing file).
    Replace(PathBuf),
}

#[derive(Clone, Copy)]
enum Then {
    New,
    Open,
    Close,
}

#[derive(Default)]
struct State {
    window: Option<FileListEditor>,
    list: EditorList,
    sizes: Option<std::collections::HashMap<Vec<u8>, u64>>,
    pending: Option<Pending>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

fn window() -> Option<FileListEditor> {
    with(|s| s.window.as_ref().map(|w| w.clone_strong()))
}

fn notice(text: String, error: bool) {
    if let Some(w) = window() {
        w.set_notice(text.into());
        w.set_notice_error(error);
    }
}

fn busy(on: bool) {
    if let Some(w) = window() {
        w.set_busy(on);
    }
}

/// Opens the editor, or brings it to the front.
pub fn open() {
    if window().is_none() {
        let w = match FileListEditor::new() {
            Ok(w) => w,
            Err(e) => {
                crate::debug_log::note!("cannot open the file list editor: {e}");
                return;
            }
        };
        w.on_new_list(|| leave(Then::New));
        w.on_open_list(|| leave(Then::Open));
        w.on_close_window(|| leave(Then::Close));
        w.on_save_list(|| save(false));
        w.on_save_as(|| save(true));
        w.on_add_folder(add_folder);
        w.on_add_files(add_file);
        w.on_remove_current(|| {
            let Some(w) = window() else { return };
            let row = w.get_current();
            if let Ok(row) = usize::try_from(row) {
                let n = with(|s| s.list.remove(&[row]));
                refresh();
                notice(
                    fill(tr("Removed {} entries."), &[&crate::format::count(n)]),
                    false,
                );
            }
        });
        w.on_remove_duplicates(|| {
            let n = with(|s| s.list.remove_duplicates());
            refresh();
            notice(
                fill(
                    tr("Removed {} duplicate entries."),
                    &[&crate::format::count(n)],
                ),
                false,
            );
        });
        w.on_folder_sizes(|| {
            with(|s| s.sizes = Some(s.list.folder_sizes()));
            refresh();
        });
        w.on_answer(answer);
        with(|s| s.window = Some(w));
        refresh();
    }
    if let Some(w) = window() {
        crate::prefs::with(|p| {
            w.invoke_set_color_scheme(p.color_scheme(), p.dark_style(), p.accent_colors())
        });
        let _ = w.hide();
        let _ = w.show();
    }
}

pub fn apply_scheme(scheme: i32, style: i32, accent: crate::AccentColors) {
    if let Some(w) = window() {
        w.invoke_set_color_scheme(scheme, style, accent);
    }
}

fn refresh() {
    let Some(w) = window() else { return };
    let zone = everygrep_core::localtime::Zone::local();
    let rows: Vec<ModelRc<StandardListViewItem>> = with(|s| {
        s.list
            .list
            .entries
            .iter()
            .map(|e| {
                let size = if e.is_dir {
                    s.sizes
                        .as_ref()
                        .map(|m| crate::format::size(m.get(&e.path).copied().unwrap_or(0)))
                        .unwrap_or_default()
                } else {
                    crate::format::size(e.size)
                };
                let items = vec![
                    StandardListViewItem::from(String::from_utf8_lossy(&e.path).as_ref()),
                    StandardListViewItem::from(if e.is_dir { tr("Folder") } else { tr("File") }),
                    StandardListViewItem::from(size.as_str()),
                    StandardListViewItem::from(
                        crate::format::timestamp(e.modified_ns.unwrap_or(0), &zone).as_str(),
                    ),
                ];
                ModelRc::new(VecModel::from(items))
            })
            .collect()
    });
    let (name, root, dirty) = with(|s| {
        (
            s.list
                .path
                .as_deref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            s.list.list.root.clone(),
            s.list.dirty,
        )
    });
    if w.get_current() >= rows.len() as i32 {
        w.set_current(-1);
    }
    w.set_rows(ModelRc::new(VecModel::from(rows)));
    w.set_file_name(name.into());
    w.set_dirty(dirty);
    w.set_root_text(
        if root.is_empty() {
            tr("Empty list. Add a folder or files.").to_string()
        } else {
            fill(
                tr("Folder of the list: {}"),
                &[&String::from_utf8_lossy(&root)],
            )
        }
        .into(),
    );
}

fn ask(pending: Pending, question: String, kind: i32) {
    with(|s| s.pending = Some(pending));
    if let Some(w) = window() {
        w.set_question(question.into());
        w.set_asking(kind);
    }
}

fn answer(yes: bool) {
    let pending = with(|s| s.pending.take());
    if let Some(w) = window() {
        w.set_asking(0);
    }
    match (pending, yes) {
        (Some(Pending::Discard(then)), true) => go(then),
        (Some(Pending::Replace(path)), true) => write(path, true),
        _ => {}
    }
}

/// New, Open and Close ask first when the list has unsaved changes.
fn leave(then: Then) {
    if with(|s| s.list.dirty) {
        ask(
            Pending::Discard(then),
            tr("The list has unsaved changes. Discard them?").into(),
            1,
        );
    } else {
        go(then);
    }
}

fn go(then: Then) {
    match then {
        Then::New => {
            with(|s| {
                s.list = EditorList::default();
                s.sizes = None;
            });
            notice(String::new(), false);
            refresh();
        }
        Then::Open => open_file(),
        Then::Close => {
            if let Some(w) = window() {
                let _ = w.hide();
            }
        }
    }
}

fn list_filter() -> [crate::portal::SaveFilter<'static>; 1] {
    [crate::portal::SaveFilter {
        name: tr("File lists"),
        pattern: "*.eglist",
    }]
}

/// Runs `work` off the UI thread, then `done` with its result on it.
fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + Send + 'static,
) {
    busy(true);
    std::thread::spawn(move || {
        let result = work();
        let _ = slint::invoke_from_event_loop(move || {
            busy(false);
            done(result);
        });
    });
}

fn open_file() {
    background(
        || -> Result<Option<EditorList>, String> {
            let Some(path) =
                crate::portal::open_file(tr("Open a file list"), tr("Open"), &list_filter())?
            else {
                return Ok(None);
            };
            EditorList::open(&path)
                .map(Some)
                .map_err(|e| fill(tr("Cannot open the file list: {}"), &[&e]))
        },
        |result| match result {
            Ok(Some(list)) => {
                with(|s| {
                    s.list = list;
                    s.sizes = None;
                });
                notice(String::new(), false);
                refresh();
            }
            Ok(None) => {}
            Err(e) => notice(e, true),
        },
    );
}

fn add_folder() {
    let list = with(|s| s.list.clone());
    background(
        move || -> Result<Option<(EditorList, usize)>, String> {
            let Some(dir) =
                crate::portal::choose_folder(tr("Add a folder to the file list"), tr("Add"))?
            else {
                return Ok(None);
            };
            let mut list = list;
            let n = list
                .add_folder(&dir)
                .map_err(|e| fill(tr("Cannot add {}: {}"), &[&dir.display(), &e]))?;
            Ok(Some((list, n)))
        },
        added,
    );
}

fn add_file() {
    let list = with(|s| s.list.clone());
    background(
        move || -> Result<Option<(EditorList, usize)>, String> {
            let Some(file) =
                crate::portal::choose_file(tr("Add a file to the file list"), tr("Add"))?
            else {
                return Ok(None);
            };
            let mut list = list;
            let n = list
                .add_paths(std::slice::from_ref(&file))
                .map_err(|e| fill(tr("Cannot add {}: {}"), &[&file.display(), &e]))?;
            Ok(Some((list, n)))
        },
        added,
    );
}

/// The list grew off the UI thread; it replaces the shown one only when
/// that did not change meanwhile (the window is busy while it works).
fn added(result: Result<Option<(EditorList, usize)>, String>) {
    match result {
        Ok(Some((list, n))) => {
            with(|s| {
                s.list = list;
                s.sizes = None;
            });
            refresh();
            notice(
                fill(tr("Added {} entries."), &[&crate::format::count(n)]),
                false,
            );
        }
        Ok(None) => {}
        Err(e) => notice(e, true),
    }
}

fn save(as_new: bool) {
    let current = with(|s| s.list.path.clone());
    match current {
        Some(path) if !as_new => write(path, false),
        _ => {
            let name = with(|s| {
                let root = s.list.list.root.clone();
                let root = std::path::Path::new(std::ffi::OsStr::from_bytes(&root));
                root.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "files".into())
            });
            background(
                move || {
                    crate::portal::save_file(
                        tr("Save the file list"),
                        tr("Save"),
                        &format!("{name}.{}", everygrep_core::filelist::EXTENSION),
                        &list_filter(),
                    )
                },
                |chosen| match chosen {
                    // The desktop's dialog already asked before replacing.
                    Ok(Some((path, _))) => write(path, true),
                    Ok(None) => {}
                    Err(e) => notice(crate::labels::chooser_failed(&e), true),
                },
            );
        }
    }
}

fn write(path: PathBuf, replace: bool) {
    let mut list = with(|s| s.list.clone());
    background(
        move || {
            let result = list.save(&path, replace);
            (list, path, result)
        },
        |(list, path, result)| match result {
            Ok(()) => {
                with(|s| {
                    s.list.path = list.path;
                    s.list.stamp = list.stamp;
                    s.list.dirty = false;
                });
                refresh();
                notice(fill(tr("Saved to {}."), &[&path.display()]), false);
            }
            Err(SaveError::Changed) => ask(
                Pending::Replace(path.clone()),
                fill(
                    tr("{} changed on disk since it was opened. Replace it with this list?"),
                    &[&path.display()],
                ),
                2,
            ),
            Err(SaveError::Io(e)) => notice(fill(tr("Cannot save the file list: {}"), &[&e]), true),
        },
    );
}
