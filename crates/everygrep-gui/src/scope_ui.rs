//! The search scope selector next to the search field: all locations, the
//! home folder, or one indexed root (a folder, a drive or a file list).
//! Only entries below the chosen folder are listed (see
//! [`Engine::set_within`]). Each tab keeps its own scope; the selector can
//! be hidden in Settings, which lists all locations again.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::AppWindow;
use crate::engine::Engine;
use crate::i18n::{fill, tr};

/// One entry of the selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    /// `None` for all locations.
    pub folder: Option<PathBuf>,
}

// Keep the distinctive folder name visible when a combo elides the end.
fn folder_label(path: &Path) -> String {
    match path.file_name() {
        Some(name) => format!("{} ({})", name.to_string_lossy(), path.display()),
        None => path.display().to_string(),
    }
}

/// The entries: all locations, the home folder when it is indexed (inside
/// or equal to a root), every root, and `current` when it is none of
/// these (a tab's scope whose root went away). Drive roots show their
/// label.
pub fn choices(
    roots: &[PathBuf],
    home: Option<&Path>,
    labels: &HashMap<Vec<u8>, String>,
    current: Option<&Path>,
) -> Vec<Choice> {
    let mut out = vec![Choice {
        label: tr("All locations").to_owned(),
        folder: None,
    }];
    let home = home.filter(|h| h.is_absolute() && roots.iter().any(|r| h.starts_with(r)));
    if let Some(home) = home {
        out.push(Choice {
            label: fill(tr("Home ({})"), &[&home.display()]),
            folder: Some(home.to_path_buf()),
        });
    }
    for root in roots {
        if Some(root.as_path()) == home {
            continue;
        }
        let label = match labels.get(root.as_os_str().as_bytes()) {
            Some(name) => format!("{name} ({})", root.display()),
            None => folder_label(root),
        };
        out.push(Choice {
            label,
            folder: Some(root.clone()),
        });
    }
    if let Some(current) = current
        && !out.iter().any(|c| c.folder.as_deref() == Some(current))
    {
        out.push(Choice {
            label: folder_label(current),
            folder: Some(current.to_path_buf()),
        });
    }
    out
}

struct State {
    engine: Arc<Engine>,
    choices: Vec<Choice>,
    current: Option<PathBuf>,
    shown: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn connect(w: &AppWindow, engine: &Arc<Engine>, shown: bool) {
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            engine: engine.clone(),
            choices: Vec::new(),
            current: None,
            shown,
        })
    });
    w.on_scope_chosen({
        let weak = w.as_weak();
        move |i| {
            let Some(w) = weak.upgrade() else { return };
            let folder = STATE.with(|s| {
                s.borrow().as_ref().and_then(|s| {
                    usize::try_from(i)
                        .ok()
                        .and_then(|i| s.choices.get(i))
                        .map(|c| c.folder.clone())
                })
            });
            if let Some(folder) = folder {
                set(&w, folder, true);
            }
        }
    });
    w.set_show_scope(shown);
    refresh(w);
}

/// Builds the entries again (roots or drives changed) and selects the
/// current one.
pub fn refresh(w: &AppWindow) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(s) = s.as_mut() else { return };
        let labels = s.engine.root_labels().unwrap_or_default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        s.choices = choices(
            &s.engine.root_paths(),
            home.as_deref(),
            &labels,
            s.current.as_deref(),
        );
        let names: Vec<SharedString> = s.choices.iter().map(|c| c.label.as_str().into()).collect();
        let index = s
            .choices
            .iter()
            .position(|c| c.folder == s.current)
            .unwrap_or(0);
        w.set_scope_names(ModelRc::new(VecModel::from(names)));
        w.set_scope_index(index as i32);
    });
}

/// The scope of the shown tab (`None` for all locations).
pub fn current() -> Option<PathBuf> {
    STATE.with(|s| s.borrow().as_ref().and_then(|s| s.current.clone()))
}

/// Makes `folder` the scope; `search` runs the search again (a tab being
/// shown searches by itself). While the selector is hidden every search
/// lists all locations, whatever a tab kept.
pub fn set(w: &AppWindow, folder: Option<PathBuf>, search: bool) {
    let engine = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let s = s.as_mut()?;
        s.current = folder.clone();
        Some((s.engine.clone(), s.shown))
    });
    let Some((engine, shown)) = engine else {
        return;
    };
    let effective = folder.filter(|_| shown);
    if search {
        engine.set_within(effective);
    } else {
        engine.put_within(effective);
    }
    refresh(w);
}

/// Shows or hides the selector (Settings).
pub fn set_shown(w: &AppWindow, shown: bool) {
    let changed = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let s = s.as_mut()?;
        let changed = s.shown != shown;
        s.shown = shown;
        Some(changed)
    });
    w.set_show_scope(shown);
    if changed == Some(true) {
        set(w, current(), true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_under_a_long_shared_parent_start_with_distinct_names() {
        let roots = [
            PathBuf::from("/home/me/a-very-long-shared-parent/alpha"),
            PathBuf::from("/home/me/a-very-long-shared-parent/beta"),
        ];
        let c = choices(&roots, None, &HashMap::new(), None);
        assert!(c[1].label.starts_with("alpha ("));
        assert!(c[2].label.starts_with("beta ("));
        for (choice, root) in c[1..].iter().zip(&roots) {
            assert!(choice.label.contains(&root.display().to_string()));
            assert_eq!(choice.folder.as_ref(), Some(root));
        }
        assert_eq!(folder_label(Path::new("/")), "/");
    }

    #[test]
    fn the_selector_lists_all_home_and_every_root() {
        let roots = [PathBuf::from("/home/me"), PathBuf::from("/media/USB")];
        let mut labels = HashMap::new();
        labels.insert(b"/media/USB".to_vec(), "Photos".to_owned());
        let c = choices(&roots, Some(Path::new("/home/me")), &labels, None);
        let folders: Vec<_> = c.iter().map(|c| c.folder.clone()).collect();
        assert_eq!(
            folders,
            [None, Some("/home/me".into()), Some("/media/USB".into())],
            "the home root is listed once, as Home"
        );
        assert_eq!(c[0].label, "All locations");
        assert_eq!(c[1].label, "Home (/home/me)");
        assert_eq!(c[2].label, "Photos (/media/USB)");
    }

    #[test]
    fn home_is_listed_only_when_it_is_indexed() {
        let roots = [PathBuf::from("/")];
        let c = choices(&roots, Some(Path::new("/home/me")), &HashMap::new(), None);
        assert_eq!(c.len(), 3, "home lies inside the root /");
        assert_eq!(c[1].folder.as_deref(), Some(Path::new("/home/me")));
        let roots = [PathBuf::from("/srv/data")];
        let c = choices(&roots, Some(Path::new("/home/me")), &HashMap::new(), None);
        assert_eq!(c.len(), 2);
        let c = choices(&roots, Some(Path::new("relative")), &HashMap::new(), None);
        assert_eq!(c.len(), 2);
        let c = choices(&roots, None, &HashMap::new(), None);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn a_kept_scope_whose_root_went_away_stays_selectable() {
        let roots = [PathBuf::from("/srv/data")];
        let gone = Path::new("/media/old");
        let c = choices(&roots, None, &HashMap::new(), Some(gone));
        assert_eq!(c.last().unwrap().folder.as_deref(), Some(gone));
        let c = choices(&roots, None, &HashMap::new(), Some(Path::new("/srv/data")));
        assert_eq!(c.len(), 2, "a listed scope is not added twice");
    }
}
