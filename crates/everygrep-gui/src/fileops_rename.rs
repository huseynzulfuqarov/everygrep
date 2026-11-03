//! The optional rename dialog holds a path, never a changing result row.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::ComponentHandle;

use crate::{AppWindow, actions, prefs};

type RenameDone = Box<dyn FnOnce(&AppWindow, Result<Option<String>, String>)>;
thread_local! { static DONE: RefCell<Option<RenameDone>> = RefCell::new(None); }

#[derive(Default)]
pub struct Dialog {
    path: RefCell<Option<PathBuf>>,
}

impl Dialog {
    pub fn connect(w: &AppWindow) -> Rc<Self> {
        let dialog = Rc::new(Self::default());
        w.on_fileops_rename_edited({
            let (dialog, weak) = (dialog.clone(), w.as_weak());
            move |name| {
                if let (Some(w), Some(_path)) = (weak.upgrade(), dialog.path.borrow().as_ref()) {
                    let error = actions::validate_name(&name)
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_default();
                    w.set_fileops_rename_error(error.into());
                }
            }
        });
        w.on_fileops_rename_submit({
            let (dialog, weak) = (dialog.clone(), w.as_weak());
            move || {
                let Some(w) = weak.upgrade() else { return };
                if w.get_fileops_rename_busy() {
                    return;
                }
                let Some(path) = dialog.path.borrow().clone() else {
                    return;
                };
                w.set_fileops_rename_busy(true);
                let (weak, name, dialog) =
                    (w.as_weak(), w.get_fileops_rename_name(), dialog.clone());
                // Rc stays on the UI thread; the callback is stored there.
                DONE.with(|d| {
                    *d.borrow_mut() = Some(Box::new(move |w, result| {
                        w.set_fileops_rename_busy(false);
                        match result {
                            Err(e) => w.set_fileops_rename_error(e.into()),
                            Ok(note) => {
                                if let Some(note) = note {
                                    crate::info(w, note);
                                }
                                dialog.close(w);
                                crate::fileops_undo_ui::refresh_open();
                            }
                        }
                    }))
                });
                std::thread::spawn(move || {
                    let result =
                        crate::fileops_undo::rename(&path, &name).map_err(|e| e.to_string());
                    let _ = weak.upgrade_in_event_loop(move |w| {
                        DONE.with(|d| {
                            if let Some(done) = d.borrow_mut().take() {
                                done(&w, result);
                            }
                        })
                    });
                });
            }
        });
        w.on_fileops_rename_cancel({
            let (dialog, weak) = (dialog.clone(), w.as_weak());
            move || {
                if let Some(w) = weak.upgrade() {
                    dialog.close(&w);
                }
            }
        });
        dialog
    }

    pub fn start(&self, w: &AppWindow, path: PathBuf, name: &str, is_dir: bool) -> bool {
        let mut enabled = false;
        let mut name_only = true;
        prefs::with(|p| {
            enabled = p.fileops_settings().rename_dialog;
            name_only = p.rename_name_only();
        });
        if !enabled {
            return false;
        }
        w.set_fileops_rename_location(
            crate::format::visible_name(&path.parent().unwrap_or(&path).to_string_lossy())
                .as_ref()
                .into(),
        );
        w.set_fileops_rename_name(name.into());
        w.set_fileops_rename_stem(actions::rename_selection(name, is_dir, name_only) as i32);
        w.set_fileops_rename_error("".into());
        *self.path.borrow_mut() = Some(path);
        w.set_fileops_rename_open(true);
        true
    }

    fn close(&self, w: &AppWindow) {
        if w.get_fileops_rename_busy() {
            return;
        }
        self.path.borrow_mut().take();
        w.set_fileops_rename_open(false);
        w.invoke_focus_list();
    }
}
