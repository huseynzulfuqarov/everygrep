//! The optional rename dialog holds a path, never a changing result row.
//! The same dialog names a new folder.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::ComponentHandle;

use crate::i18n::{fill, tr};
use crate::{AppWindow, actions, prefs};

type RenameDone = Box<dyn FnOnce(&AppWindow, Result<Option<String>, String>)>;
thread_local! { static DONE: RefCell<Option<RenameDone>> = RefCell::new(None); }

/// What the dialog's name is for.
#[derive(Clone)]
enum Mode {
    /// A new name for the item at this path, as it was when the dialog
    /// opened.
    Rename(PathBuf, actions::Chosen),
    /// The name of a new folder in this folder.
    NewFolder(PathBuf),
}

#[derive(Default)]
pub struct Dialog {
    path: RefCell<Option<Mode>>,
}

/// The error shown for `name` while it is typed: syntax, and for a new
/// folder a name that is taken in its folder.
pub fn name_problem(name: &str, new_in: Option<&std::path::Path>) -> String {
    if let Err(e) = actions::validate_name(name) {
        return e.to_string();
    }
    match new_in {
        Some(folder) if std::fs::symlink_metadata(folder.join(name)).is_ok() => {
            actions::RenameError::Exists(name.to_owned()).to_string()
        }
        _ => String::new(),
    }
}

/// Where New folder puts the folder for the item at `path`: inside it when
/// it is a folder and `inside` is set, else in the item's own folder.
pub fn new_folder_place(path: &std::path::Path, is_dir: bool, inside: bool) -> Option<PathBuf> {
    if is_dir && inside {
        Some(path.to_owned())
    } else {
        path.parent().map(std::path::Path::to_owned)
    }
}

/// The name a new folder in `folder` is offered: "New Folder", or the first
/// free "New Folder (2)", "New Folder (3)", …
pub fn free_folder_name(folder: &std::path::Path) -> String {
    let base = tr("New Folder");
    crate::transfer::first_free(folder, std::ffi::OsStr::new(base), true)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| base.to_owned())
}

impl Dialog {
    pub fn connect(w: &AppWindow) -> Rc<Self> {
        let dialog = Rc::new(Self::default());
        w.on_fileops_rename_edited({
            let (dialog, weak) = (dialog.clone(), w.as_weak());
            move |name| {
                if let (Some(w), Some(mode)) = (weak.upgrade(), dialog.path.borrow().as_ref()) {
                    let folder = match mode {
                        Mode::NewFolder(folder) => Some(folder.as_path()),
                        Mode::Rename(..) => None,
                    };
                    w.set_fileops_rename_error(name_problem(&name, folder).into());
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
                let Some(mode) = dialog.path.borrow().clone() else {
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
                    let result = crate::fileops_undo::catch_worker(|| match &mode {
                        Mode::Rename(path, chosen) => {
                            crate::fileops_undo::rename_chosen(path, &name, chosen)
                                .map_err(|e| e.to_string())
                        }
                        Mode::NewFolder(folder) => crate::fileops_undo::new_folder(folder, &name)
                            .map(|(_, note)| {
                                let done = fill(
                                    tr("Created the folder “{0}” in “{1}”."),
                                    &[&name.as_str(), &folder.display()],
                                );
                                Some(match note {
                                    Some(note) => format!("{done} {note}"),
                                    None => done,
                                })
                            })
                            .map_err(|e| e.to_string()),
                    })
                    .unwrap_or_else(Err);
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
        w.set_fileops_rename_title(tr("Rename").into());
        w.set_fileops_rename_action(tr("Rename").into());
        w.set_fileops_rename_label(tr("New name").into());
        w.set_fileops_rename_name(name.into());
        w.set_fileops_rename_stem(actions::rename_selection(name, is_dir, name_only) as i32);
        w.set_fileops_rename_error("".into());
        let chosen = actions::Chosen::of(std::slice::from_ref(&path));
        *self.path.borrow_mut() = Some(Mode::Rename(path, chosen));
        w.set_fileops_rename_open(true);
        true
    }

    /// Asks for the name of a new folder in `folder`, offering a free one.
    pub fn start_new_folder(&self, w: &AppWindow, folder: PathBuf) {
        let name = free_folder_name(&folder);
        w.set_fileops_rename_location(
            crate::format::visible_name(&folder.to_string_lossy())
                .as_ref()
                .into(),
        );
        w.set_fileops_rename_title(tr("New folder").into());
        w.set_fileops_rename_action(tr("Create").into());
        w.set_fileops_rename_label(tr("Folder name").into());
        w.set_fileops_rename_stem(name.len() as i32);
        w.set_fileops_rename_error(name_problem(&name, Some(&folder)).into());
        w.set_fileops_rename_name(name.into());
        *self.path.borrow_mut() = Some(Mode::NewFolder(folder));
        w.set_fileops_rename_open(true);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_folder_names_skip_taken_names_and_typed_names_are_checked() {
        let dir = std::env::temp_dir().join(format!("everygrep-newname-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_folder_name(&dir), "New Folder");
        std::fs::create_dir(dir.join("New Folder")).unwrap();
        // A dangling link takes a name too.
        std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("New Folder (2)")).unwrap();
        assert_eq!(free_folder_name(&dir), "New Folder (3)");
        assert!(name_problem("New Folder", Some(&dir)).contains("already exists"));
        assert!(name_problem("New Folder (2)", Some(&dir)).contains("already exists"));
        assert_eq!(name_problem("New Folder (3)", Some(&dir)), "");
        assert_eq!(name_problem("New Folder", None), "");
        for bad in ["", "a/b", ".", ".."] {
            assert!(!name_problem(bad, Some(&dir)).is_empty(), "{bad:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn new_folder_place_follows_the_setting_for_folders_only() {
        let p = std::path::Path::new("/a/b");
        assert_eq!(new_folder_place(p, true, true).unwrap(), p);
        assert_eq!(
            new_folder_place(p, true, false).unwrap(),
            std::path::Path::new("/a")
        );
        assert_eq!(
            new_folder_place(p, false, true).unwrap(),
            std::path::Path::new("/a")
        );
        assert!(new_folder_place(std::path::Path::new("/"), false, true).is_none());
    }
}
