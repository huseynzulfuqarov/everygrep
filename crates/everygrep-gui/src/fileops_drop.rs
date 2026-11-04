//! Dragging result rows onto a folder row: the dragged items are copied or
//! moved into that folder with the same transfer as Paste.
//!
//! The drag stays inside the window. What is dragged is taken when the
//! drag starts (the selection, or the pressed row alone); the folder is
//! checked while the pointer is over it, and once more when the items are
//! dropped. Ctrl while dropping copies; otherwise the setting decides:
//! move on the same drive and copy to another (as file managers do),
//! always copy, always move, or ask with a menu.

use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use slint::ComponentHandle;

use crate::engine::Engine;
use crate::results::ResultsModel;
use crate::{AppWindow, clipboard::Operation, prefs};

/// What a drop does without Ctrl.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DropAction {
    /// Move when every item is on the folder's drive, else copy.
    #[default]
    Auto,
    Copy,
    Move,
    Ask,
}

impl DropAction {
    const ALL: [DropAction; 4] = [Self::Auto, Self::Copy, Self::Move, Self::Ask];

    /// The word in the settings file.
    pub fn word(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Copy => "copy",
            Self::Move => "move",
            Self::Ask => "ask",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.word() == word)
    }

    /// The position in the settings list.
    pub fn index(self) -> i32 {
        Self::ALL.iter().position(|&a| a == self).unwrap_or(0) as i32
    }

    pub fn from_index(i: i32) -> Self {
        usize::try_from(i)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
            .unwrap_or_default()
    }
}

/// What to do with a drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Do(Operation),
    Ask,
}

/// The drop of `sources` onto `dest`: Ctrl (`copy`) copies; otherwise
/// `action` decides. When a drive cannot be read, the automatic rule
/// copies, which leaves the items where they are.
pub fn choose(action: DropAction, copy: bool, sources: &[PathBuf], dest: &Path) -> Choice {
    if copy {
        return Choice::Do(Operation::Copy);
    }
    match action {
        DropAction::Copy => Choice::Do(Operation::Copy),
        DropAction::Move => Choice::Do(Operation::Cut),
        DropAction::Ask => Choice::Ask,
        DropAction::Auto => {
            let same = std::fs::metadata(dest).ok().is_some_and(|d| {
                sources.iter().all(|s| {
                    std::fs::symlink_metadata(s)
                        .ok()
                        .is_some_and(|m| m.dev() == d.dev())
                })
            });
            Choice::Do(if same {
                Operation::Cut
            } else {
                Operation::Copy
            })
        }
    }
}

/// Whether `sources` may be dropped onto the folder `dest`: there is
/// something to drop, and the folder is not one of them or inside one.
pub fn can_drop(sources: &[PathBuf], dest: &Path) -> bool {
    !sources.is_empty()
        && !sources.iter().any(|s| s == dest)
        && crate::transfer::refusal(sources, dest).is_none()
}

/// A marker for drags of result rows; the rows are kept here, not in it.
struct RowDrag;

#[derive(Default)]
struct State {
    /// The items being dragged.
    sources: Option<Vec<PathBuf>>,
    /// The last folder row checked, and the answer.
    checked: Option<(usize, bool)>,
    /// A drop waiting for the answer of the menu: items and folder.
    asked: Option<(Vec<PathBuf>, PathBuf)>,
}

pub fn connect(w: &AppWindow, model: &Rc<ResultsModel>, engine: &Arc<Engine>) {
    let mut payload = slint::DataTransfer::default();
    payload.set_user_data(Rc::new(RowDrag));
    w.set_drag_payload(payload);
    let state = Rc::new(RefCell::new(State::default()));
    w.on_rows_drag_start({
        let (weak, model, state) = (w.as_weak(), model.clone(), state.clone());
        move |row| {
            let mut s = state.borrow_mut();
            s.checked = None;
            s.sources = crate::online_targets(&weak, &model, usize::try_from(row).ok())
                .map(|t| t.paths)
                .filter(|p| !p.is_empty());
        }
    });
    w.on_rows_drag_end({
        let state = state.clone();
        move || {
            let mut s = state.borrow_mut();
            s.sources = None;
            s.checked = None;
        }
    });
    w.on_drop_allowed({
        let (model, state) = (model.clone(), state.clone());
        move |row| {
            let Ok(row) = usize::try_from(row) else {
                return false;
            };
            let mut s = state.borrow_mut();
            if let Some((r, ok)) = s.checked
                && r == row
            {
                return ok;
            }
            let ok = match (&s.sources, model.path(row)) {
                (Some(sources), Some(dest)) => can_drop(sources, &dest),
                _ => false,
            };
            s.checked = Some((row, ok));
            ok
        }
    });
    w.on_drop_on({
        let (weak, model, engine, state) =
            (w.as_weak(), model.clone(), engine.clone(), state.clone());
        move |row, copy| {
            let Some(w) = weak.upgrade() else {
                return false;
            };
            let sources = state.borrow_mut().sources.take();
            let (Some(sources), Some(dest)) = (
                sources,
                usize::try_from(row).ok().and_then(|r| model.path(r)),
            ) else {
                return false;
            };
            if model.targets_offline(usize::try_from(row).ok()) {
                crate::info(&w, crate::labels::offline_refused().into());
                return false;
            }
            let mut action = DropAction::default();
            prefs::with(|p| action = p.fileops_settings().drop_action);
            match choose(action, copy, &sources, &dest) {
                Choice::Do(op) => {
                    crate::start_checked_transfer(&w, &engine, op, sources, dest);
                    false
                }
                Choice::Ask => {
                    state.borrow_mut().asked = Some((sources, dest));
                    true
                }
            }
        }
    });
    w.on_drop_answer({
        let (weak, engine) = (w.as_weak(), engine.clone());
        move |answer| {
            let Some((sources, dest)) = state.borrow_mut().asked.take() else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            let op = match answer {
                1 => Operation::Copy,
                2 => Operation::Cut,
                _ => return,
            };
            crate::start_checked_transfer(&w, &engine, op, sources, dest);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("everygrep-drop-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn ctrl_copies_and_the_setting_decides_otherwise() {
        let t = Temp::new("choose");
        let file = t.0.join("f");
        fs::write(&file, "x").unwrap();
        let dest = t.0.join("d");
        fs::create_dir(&dest).unwrap();
        let sources = [file];
        for action in DropAction::ALL {
            assert_eq!(
                choose(action, true, &sources, &dest),
                Choice::Do(Operation::Copy)
            );
        }
        assert_eq!(
            choose(DropAction::Copy, false, &sources, &dest),
            Choice::Do(Operation::Copy)
        );
        assert_eq!(
            choose(DropAction::Move, false, &sources, &dest),
            Choice::Do(Operation::Cut)
        );
        assert_eq!(choose(DropAction::Ask, false, &sources, &dest), Choice::Ask);
        // Same drive: the automatic rule moves.
        assert_eq!(
            choose(DropAction::Auto, false, &sources, &dest),
            Choice::Do(Operation::Cut)
        );
    }

    #[test]
    fn the_automatic_rule_copies_when_a_drive_cannot_be_read() {
        let t = Temp::new("auto-missing");
        let dest = t.0.join("d");
        fs::create_dir(&dest).unwrap();
        let gone = [t.0.join("gone")];
        assert_eq!(
            choose(DropAction::Auto, false, &gone, &dest),
            Choice::Do(Operation::Copy)
        );
        let file = [t.0.join("d")];
        assert_eq!(
            choose(DropAction::Auto, false, &file, &t.0.join("missing")),
            Choice::Do(Operation::Copy)
        );
    }

    #[test]
    fn a_folder_is_never_a_drop_target_for_itself_or_its_parents() {
        let t = Temp::new("targets");
        let a = t.0.join("a");
        let inner = a.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let b = t.0.join("b");
        fs::create_dir(&b).unwrap();
        let file = t.0.join("file");
        fs::write(&file, "x").unwrap();
        assert!(!can_drop(std::slice::from_ref(&a), &a));
        assert!(!can_drop(std::slice::from_ref(&a), &inner));
        assert!(!can_drop(&[b.clone(), a.clone()], &inner));
        assert!(!can_drop(&[], &b));
        assert!(!can_drop(std::slice::from_ref(&b), &file));
        assert!(can_drop(&[a.clone(), file.clone()], &b));
        // Into a folder's own parent is allowed: a move leaves it there,
        // a copy gets a free name.
        assert!(can_drop(std::slice::from_ref(&inner), &a));
    }

    #[test]
    fn drop_actions_have_stable_words_and_positions() {
        for (i, action) in DropAction::ALL.into_iter().enumerate() {
            assert_eq!(DropAction::from_word(action.word()), Some(action));
            assert_eq!(action.index(), i as i32);
        }
        assert_eq!(DropAction::from_index(-1), DropAction::Auto);
        assert_eq!(DropAction::from_index(9), DropAction::Auto);
        assert_eq!(DropAction::from_word("Copy"), None);
    }
}
