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
        let job = engine.begin_job(TaskKind::ReversingFiles);
        let (text, failed) = fileops_undo::run(request, job.progress());
        drop(job);
        let _ = weak.upgrade_in_event_loop(move |w| {
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
                    details.push(slint::SharedString::from(format!(
                        "{}: {}\n{}: {}",
                        tr("Source path"),
                        r.cells[1],
                        tr("Destination"),
                        r.cells[2]
                    )));
                    let cells: Vec<_> = r
                        .cells
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
    let rows = fileops_undo::view(0).rows;
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
