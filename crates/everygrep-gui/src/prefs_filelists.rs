//! File lists in the settings window (Indexing page) and the commands that
//! make one, add one and hide offline entries.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use slint::{ModelRc, VecModel};

use super::{Prefs, with};
use crate::engine::{FileListSetting, ListState, TaskKind};
use crate::i18n::{fill, tr};
use crate::{FileListRow, SettingsWindow, labels};

impl Prefs {
    /// Hands the file lists and the hide switch to the engine and the menu.
    pub fn apply_file_lists(&self) {
        let s = self.settings.borrow();
        self.engine.set_file_lists(&s.file_lists);
        self.engine.set_hide_offline(s.hide_offline);
        if let Some(m) = self.main.upgrade() {
            m.set_hide_offline(s.hide_offline);
        }
    }

    /// Shows or hides the entries of offline drives and file lists.
    pub fn set_hide_offline(&self, hide: bool) {
        self.change(move |s| s.hide_offline = hide);
        self.apply_file_lists();
        self.refresh();
    }

    fn edit_file_lists(&self, f: impl Fn(&mut Vec<FileListSetting>) + Send + 'static) {
        self.change(move |s| f(&mut s.file_lists));
        self.apply_file_lists();
        self.refresh();
    }

    /// Adds the list at `path`, turned on, or turns it on when it is there.
    pub fn add_file_list(&self, path: PathBuf) {
        self.edit_file_lists(
            move |lists| match lists.iter_mut().find(|l| l.path == path) {
                Some(l) => l.enabled = true,
                None => lists.push(FileListSetting {
                    path: path.clone(),
                    enabled: true,
                }),
            },
        );
    }

    fn remove_file_list(&self, i: usize) {
        self.edit_file_lists(move |lists| {
            if i < lists.len() {
                lists.remove(i);
            }
        });
    }

    fn toggle_file_list(&self, i: usize) {
        self.edit_file_lists(move |lists| {
            if let Some(l) = lists.get_mut(i) {
                l.enabled = !l.enabled;
            }
        });
    }

    /// Asks for a file list on a helper thread and adds it.
    fn choose_file_list(&self) {
        if self.choosing.replace(true) {
            return;
        }
        let (title, accept) = (tr("Add a file list"), tr("Add"));
        std::thread::spawn(move || {
            let filters = [crate::portal::SaveFilter {
                name: tr("File lists"),
                pattern: "*.eglist",
            }];
            let result = crate::portal::open_file(title, accept, &filters);
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| {
                    p.choosing.set(false);
                    match result {
                        Ok(Some(path)) => p.add_file_list(path),
                        Ok(None) => {}
                        Err(e) => p.set_error(Some(labels::chooser_failed(&e))),
                    }
                });
            });
        });
    }

    /// Index → Create file list…: asks for an indexed folder and where to
    /// save its list, writes it as a task in the status line and adds it
    /// to the file lists.
    pub fn create_file_list(&self) {
        if self.choosing.replace(true) {
            return;
        }
        let engine = Arc::clone(&self.engine);
        let main = self.main.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<Option<(PathBuf, PathBuf)>, String> {
                let Some(folder) = crate::portal::choose_folder(
                    tr("Create a file list of a folder"),
                    tr("Choose"),
                )?
                else {
                    return Ok(None);
                };
                let name = folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "files".into());
                let Some((to, _)) = crate::portal::save_file(
                    tr("Save the file list"),
                    tr("Save"),
                    &format!("{name}.{}", everygrep_core::filelist::EXTENSION),
                    &[crate::portal::SaveFilter {
                        name: tr("File lists"),
                        pattern: "*.eglist",
                    }],
                )?
                else {
                    return Ok(None);
                };
                Ok(Some((folder, to)))
            })();
            let _ = slint::invoke_from_event_loop(move || {
                with(|p| p.choosing.set(false));
            });
            let (folder, to) = match result {
                Ok(Some(chosen)) => chosen,
                Ok(None) => return,
                Err(e) => {
                    let text = labels::chooser_failed(&e);
                    let _ = main.upgrade_in_event_loop(move |m| crate::error(&m, text));
                    return;
                }
            };
            let job = engine.begin_job(TaskKind::SavingFileList);
            let saved = engine.save_file_list(&folder, &to, job.progress(), true);
            drop(job);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(m) = main.upgrade() else { return };
                match saved {
                    Ok(n) => {
                        crate::info(
                            &m,
                            fill(
                                crate::i18n::trn(
                                    "Saved the file list of {0} ({1} entry) to {2}.",
                                    "Saved the file list of {0} ({1} entries) to {2}.",
                                    n as usize,
                                ),
                                &[
                                    &folder.display(),
                                    &crate::format::count(n as usize),
                                    &to.display(),
                                ],
                            ),
                        );
                        with(|p| p.add_file_list(to));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => crate::error(&m, fill(tr("Cannot save the file list: {}"), &[&e])),
                }
            });
        });
    }

    /// The File lists part of the Indexing page.
    pub(super) fn fill_file_lists(&self, w: &SettingsWindow) {
        let lists = self.engine.file_lists();
        let rows: Vec<FileListRow> = self
            .settings
            .borrow()
            .file_lists
            .iter()
            .map(|l| {
                let status = lists.iter().find(|s| s.path == l.path);
                let state = match status {
                    Some(s) if l.enabled => s.state.clone(),
                    _ if !l.enabled => ListState::Off,
                    _ => ListState::Pending,
                };
                let root = status.and_then(|s| s.root.as_deref());
                FileListRow {
                    title: l
                        .path
                        .file_name()
                        .unwrap_or(l.path.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                    details: labels::file_list_details(root, &state).into(),
                    file: l.path.display().to_string().into(),
                    enabled: l.enabled,
                    failed: matches!(state, ListState::Failed(_)),
                }
            })
            .collect();
        w.set_file_lists(ModelRc::new(VecModel::from(rows)));
        w.set_hide_offline(self.settings.borrow().hide_offline);
    }

    pub(super) fn connect_file_lists(me: &Rc<Prefs>, w: &SettingsWindow) {
        let weak = Rc::downgrade(me);
        let index = move |f: fn(&Prefs, usize)| {
            let weak = weak.clone();
            move |i: i32| {
                if let (Some(p), Ok(i)) = (weak.upgrade(), usize::try_from(i)) {
                    f(&p, i);
                }
            }
        };
        w.on_toggle_file_list(index(|p, i| p.toggle_file_list(i)));
        w.on_remove_file_list(index(|p, i| p.remove_file_list(i)));
        let weak = Rc::downgrade(me);
        w.on_add_file_list(move || {
            if let Some(p) = weak.upgrade() {
                p.choose_file_list();
            }
        });
        let weak = Rc::downgrade(me);
        w.on_create_file_list(move || {
            if let Some(p) = weak.upgrade() {
                p.create_file_list();
            }
        });
        let weak = Rc::downgrade(me);
        w.on_reload_file_lists(move || {
            if let Some(p) = weak.upgrade() {
                p.engine.reload_file_lists();
            }
        });
        let weak = Rc::downgrade(me);
        w.on_hide_offline_changed(move |hide| {
            if let Some(p) = weak.upgrade() {
                p.set_hide_offline(hide);
            }
        });
    }
}
