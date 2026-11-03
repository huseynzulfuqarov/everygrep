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

thread_local! { static ACTION_TIMER: slint::Timer = slint::Timer::default(); }
thread_local! { static WINDOW: RefCell<Option<FileUndoWindow>> = const { RefCell::new(None) }; }
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn connect(w: &AppWindow, engine: &Arc<Engine>) {
    ACTION_TIMER.with(|timer| {
        let weak = w.as_weak();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                if let Some(w) = weak.upgrade() {
                    let (undo, redo) = fileops_undo::availability();
                    w.set_file_can_undo(undo);
                    w.set_file_can_redo(redo);
                }
            },
        );
    });
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
    w.set_operation_ids(ModelRc::default());
    std::thread::spawn(move || {
        let view = fileops_undo::view(filter);
        let _ = weak.upgrade_in_event_loop(move |w| {
            if w.get_revision() != revision {
                return;
            }
            let mut ids = Vec::new();
            let rows: Vec<ModelRc<StandardListViewItem>> = view
                .rows
                .into_iter()
                .map(|r| {
                    ids.push(slint::SharedString::from(r.id.to_string()));
                    let cells: Vec<_> = r
                        .cells
                        .into_iter()
                        .map(|text| StandardListViewItem::from(slint::SharedString::from(text)))
                        .collect();
                    ModelRc::new(VecModel::from(cells))
                })
                .collect();
            w.set_operation_ids(ModelRc::new(VecModel::from(ids)));
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
