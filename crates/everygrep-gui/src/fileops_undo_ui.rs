//! File history window; all journal reads and file operations run on workers.
use crate::{
    AppWindow, FileUndoWindow,
    engine::{Engine, TaskKind},
    fileops_undo::{self, Request},
    i18n::{fill, tr},
};
use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

thread_local! { static MAIN: RefCell<Vec<slint::Weak<AppWindow>>> = const { RefCell::new(Vec::new()) }; }
static CONNECTED: AtomicBool = AtomicBool::new(false);

pub fn request_refresh() {
    #[cfg(test)]
    refresh_tests::REQUESTS.with(|n| n.set(n.get() + 1));
    if CONNECTED.load(Ordering::Relaxed) {
        let _ = slint::invoke_from_event_loop(refresh_actions);
    }
}

pub fn refresh_actions() {
    let (undo, redo) = fileops_undo::availability();
    MAIN.with(|windows| {
        windows.borrow_mut().retain(|weak| {
            let Some(w) = weak.upgrade() else {
                return false;
            };
            w.set_file_can_undo(undo);
            w.set_file_can_redo(redo);
            true
        })
    });
}
thread_local! { static WINDOW: RefCell<Option<FileUndoWindow>> = const { RefCell::new(None) }; }
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn connect(w: &AppWindow, engine: &Arc<Engine>) {
    MAIN.with(|windows| windows.borrow_mut().push(w.as_weak()));
    CONNECTED.store(true, Ordering::Relaxed);
    refresh_actions();
    w.on_file_undo({
        let (weak, engine) = (w.as_weak(), engine.clone());
        move |redo| {
            if let Some(w) = weak.upgrade() {
                reverse(
                    &w,
                    &engine,
                    if redo { Request::Redo } else { Request::Undo },
                );
            }
        }
    });
    w.on_file_undo_history({
        let (weak, engine) = (w.as_weak(), engine.clone());
        move || {
            if let Some(w) = weak.upgrade() {
                show(&w, &engine);
            }
        }
    });
}

fn reverse(w: &AppWindow, engine: &Arc<Engine>, request: Request) {
    if BUSY.swap(true, Ordering::SeqCst) {
        crate::info(w, tr("A file operation is still running.").into());
        return;
    }
    let (weak, engine) = (w.as_weak(), engine.clone());
    WINDOW.with(|slot| {
        if let Some(w) = slot.borrow().as_ref() {
            w.set_busy(true);
        }
    });
    std::thread::spawn(move || {
        let (text, failed) = fileops_undo::catch_worker(|| {
            let job = engine.begin_job(TaskKind::ReversingFiles);
            fileops_undo::run(request, job.progress())
        })
        .unwrap_or_else(|e| (e, true));
        let queued = weak.upgrade_in_event_loop(move |w| {
            BUSY.store(false, Ordering::SeqCst);
            refresh_actions();
            if failed {
                crate::error(&w, text.clone());
            } else {
                crate::info(&w, text.clone());
            }
            WINDOW.with(|slot| {
                if let Some(h) = slot.borrow().as_ref() {
                    h.set_busy(false);
                    h.set_notice(text.into());
                    refresh(h);
                }
            });
        });
        if queued.is_err() {
            BUSY.store(false, Ordering::SeqCst);
        }
    });
}

fn refresh(w: &FileUndoWindow) {
    let weak = w.as_weak();
    let filter = w.get_action_filter();
    let revision = w.get_revision().wrapping_add(1);
    w.set_revision(revision);
    // Row IDs are cleared while rebuilding, so a stale visible row cannot
    // become the target of a different history snapshot.
    w.set_current(-1);
    w.set_rows(ModelRc::default());
    w.set_path_details(ModelRc::default());
    w.set_operation_ids(ModelRc::default());
    std::thread::spawn(move || {
        let view = fileops_undo::view(filter);
        let _ = weak.upgrade_in_event_loop(move |w| {
            refresh_actions();
            if w.get_revision() != revision {
                return;
            }
            let mut ids = Vec::new();
            let mut details = Vec::new();
            let rows: Vec<ModelRc<StandardListViewItem>> = view
                .rows
                .into_iter()
                .map(|r| {
                    ids.push(slint::SharedString::from(r.id.to_string()));
                    let cells: Vec<_> = r
                        .cells
                        .iter()
                        .map(|s| crate::format::visible_name(s).into_owned())
                        .collect();
                    details.push(slint::SharedString::from(fill(
                        tr("Source path: {0}\nDestination: {1}"),
                        &[&cells[1], &cells[2]],
                    )));
                    let cells: Vec<_> = cells
                        .into_iter()
                        .enumerate()
                        .map(|(column, text)| {
                            let text = if column == 1 || column == 2 {
                                crate::format::middle_path(&text, 28)
                            } else {
                                text
                            };
                            StandardListViewItem::from(slint::SharedString::from(text))
                        })
                        .collect();
                    ModelRc::new(VecModel::from(cells))
                })
                .collect();
            w.set_operation_ids(ModelRc::new(VecModel::from(ids)));
            w.set_path_details(ModelRc::new(VecModel::from(details)));
            w.set_rows(ModelRc::new(VecModel::from(rows)));
            w.set_can_undo(view.can_undo);
            w.set_can_redo(view.can_redo);
        });
    });
}

fn show(main: &AppWindow, engine: &Arc<Engine>) {
    WINDOW.with(|slot| {
        if slot.borrow().is_none() {
            let Ok(w) = FileUndoWindow::new() else {
                return;
            };
            crate::physical::watch(w.window());
            w.on_resolve_key(|text, ctrl, alt, shift, meta| {
                crate::physical::chords(&text, ctrl, alt, shift, meta)
                    .into_iter()
                    .map(|chord| crate::commands::with_current(|keys| history_key(keys, chord)))
                    .find(|&k| k >= 0)
                    .unwrap_or(-1)
            });
            // Stable IDs are kept apart from the visible table cells.
            w.on_reverse({
                let (weak, main, engine) = (w.as_weak(), main.as_weak(), engine.clone());
                move |action| {
                    use slint::Model;
                    let (Some(w), Some(main)) = (weak.upgrade(), main.upgrade()) else {
                        return;
                    };
                    let request = match action {
                        0 => Request::Undo,
                        1 => Request::Redo,
                        _ => {
                            let Some(index) = usize::try_from(w.get_current())
                                .ok()
                                .and_then(|i| w.get_operation_ids().row_data(i))
                                .and_then(|id| id.parse().ok())
                            else {
                                return;
                            };
                            Request::Through(index)
                        }
                    };
                    reverse(&main, &engine, request);
                }
            });
            w.on_filter_changed({
                let weak = w.as_weak();
                move || {
                    if let Some(w) = weak.upgrade() {
                        refresh(&w);
                    }
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
            w.on_export({
                let weak = w.as_weak();
                move || {
                    let Some(w) = weak.upgrade() else {
                        return;
                    };
                    if BUSY.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    w.set_busy(true);
                    let weak = weak.clone();
                    std::thread::spawn(move || {
                        let result = export();
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            BUSY.store(false, Ordering::SeqCst);
                            refresh_actions();
                            w.set_busy(false);
                            if let Err(e) = result {
                                w.set_notice(
                                    fill(tr("Cannot export file history: {}"), &[&e]).into(),
                                );
                            }
                        });
                    });
                }
            });
            *slot.borrow_mut() = Some(w);
        }
        if let Some(w) = slot.borrow().as_ref() {
            crate::prefs::with(|p| w.invoke_set_color_scheme(p.color_scheme()));
            refresh(w);
            w.set_busy(BUSY.load(Ordering::SeqCst));
            // An open window may be behind the main window; showing it again
            // gives a new surface, which GNOME brings to the front (see `raise`
            // in main.rs).
            let _ = w.hide();
            let _ = w.show();
        }
    });
}

fn export() -> Result<(), String> {
    let rows = fileops_undo::export_view().rows;
    let Some((path, _)) = crate::portal::save_file(
        tr("Export file undo history"),
        tr("Save"),
        "file-history.csv",
        &[crate::portal::SaveFilter {
            name: tr("CSV files"),
            pattern: "*.csv",
        }],
    )?
    else {
        return Ok(());
    };
    crate::settings::write_atomic(&path, fileops_undo::csv(&rows).as_bytes())
        .map_err(|e| e.to_string())
}

pub fn refresh_open() {
    refresh_actions();
    WINDOW.with(|slot| {
        if let Some(w) = slot.borrow().as_ref() {
            refresh(w);
        }
    });
}

pub fn apply_scheme(scheme: i32) {
    WINDOW.with(|slot| {
        if let Some(w) = slot.borrow().as_ref() {
            w.invoke_set_color_scheme(scheme);
        }
    });
}

fn history_key(keys: &crate::commands::Keymap, chord: crate::commands::Chord) -> i32 {
    match keys.resolve(crate::commands::Scope::List, &chord) {
        Some(crate::Command::UndoFile) => 0,
        Some(crate::Command::RedoFile) => 1,
        _ => -1,
    }
}
#[cfg(test)]
mod key_tests {
    #[test]
    fn history_obeys_custom_keys_and_releases_the_old_binding() {
        use crate::commands::{Chord, Keymap, Scope};
        let key = Chord::parse("F9").unwrap();
        let map = Keymap::with_overrides(&[("edit.undo-file".into(), vec![(Scope::List, key)])]);
        assert_eq!(super::history_key(&map, key), 0);
        assert_eq!(
            super::history_key(&map, Chord::parse("Ctrl+Z").unwrap()),
            -1
        );
        assert_eq!(
            super::history_key(&map, Chord::parse("Ctrl+Shift+Z").unwrap()),
            1
        );
    }
}
#[cfg(test)]
mod refresh_tests {
    use crate::fileops_undo;
    use std::cell::Cell;

    thread_local! {
        /// Refreshes requested on this thread; each one queues `refresh_actions`.
        pub(super) static REQUESTS: Cell<usize> = const { Cell::new(0) };
    }
    fn requests() -> usize {
        REQUESTS.with(Cell::get)
    }

    #[test]
    fn reading_actions_requests_no_refresh_but_file_work_does() {
        let _test = fileops_undo::tests_lock();
        let before = requests();
        let _ = fileops_undo::availability();
        super::refresh_actions();
        assert_eq!(requests(), before, "reading availability queued a refresh");
        let guard = fileops_undo::file_guard();
        assert_eq!(requests(), before + 1, "taking the file slot");
        assert_eq!(fileops_undo::availability(), (false, false));
        drop(guard);
        assert_eq!(requests(), before + 2, "releasing the file slot");
    }

    /// Runs the queue the way the event loop would after `connect`: one
    /// refresh, then one more for every refresh it requests.
    fn drain(mut queued: usize) -> Option<usize> {
        for ran in 0..64 {
            if queued == 0 {
                return Some(ran);
            }
            queued -= 1;
            let before = requests();
            super::refresh_actions();
            queued += requests() - before;
        }
        None
    }

    #[test]
    fn startup_refresh_queue_drains() {
        let _test = fileops_undo::tests_lock();
        assert_eq!(drain(1), Some(1), "the startup refresh queued more work");
        let before = requests();
        drop(fileops_undo::file_guard());
        let queued = requests() - before;
        assert_eq!(drain(queued), Some(2), "a file operation's refreshes");
    }
}
