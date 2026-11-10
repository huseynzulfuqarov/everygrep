//! Index → Index changes: a window over the engine's in-memory journal of
//! the changes applied from file events (see `Engine::journal`). Nothing is
//! written to disk except by Export.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use everygrep_core::watch::ChangeKind;
use slint::{ComponentHandle, ModelRc, StandardListViewItem, Timer, TimerMode, VecModel};

use crate::engine::{Engine, JournalRow};
use crate::i18n::{fill, tr};
use crate::{AppWindow, JournalWindow};

/// The action filter's choices, after "All".
const ACTIONS: [ChangeKind; 5] = [
    ChangeKind::Created,
    ChangeKind::Deleted,
    ChangeKind::Renamed,
    ChangeKind::Moved,
    ChangeKind::Modified,
];

struct State {
    window: JournalWindow,
    engine: Arc<Engine>,
    // The rows shown, newest first, for the buttons.
    shown: Vec<JournalRow>,
    timer: Timer,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn action_label(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Created => tr("Added"),
        ChangeKind::Deleted => tr("Deleted"),
        ChangeKind::Renamed => tr("Renamed"),
        ChangeKind::Moved => tr("Moved"),
        ChangeKind::Modified => tr("Changed"),
    }
}

fn action_id(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Created => "created",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Moved => "moved",
        ChangeKind::Modified => "modified",
    }
}

/// The rows that pass the filters, newest first. `action` 0 is all, then
/// [`ACTIONS`]; `kind` 0 both, 1 folders, 2 files.
pub fn filtered(rows: &[JournalRow], action: i32, kind: i32) -> Vec<JournalRow> {
    let want = match action {
        0 => None,
        n => match usize::try_from(n - 1).ok().and_then(|i| ACTIONS.get(i)) {
            Some(k) => Some(*k),
            None => return Vec::new(),
        },
    };
    rows.iter()
        .rev()
        .filter(|r| want.is_none_or(|k| r.change.kind == k))
        .filter(|r| match kind {
            1 => r.change.is_dir,
            2 => !r.change.is_dir,
            _ => true,
        })
        .cloned()
        .collect()
}

/// The CSV of `rows` (UTC times, fixed English column names).
pub fn csv_text(rows: &[JournalRow]) -> String {
    let mut out = String::new();
    crate::csv::record(&mut out, ["Time", "Action", "Type", "Path", "Old Path"]);
    for r in rows {
        let path = crate::csv::protect(&r.change.path.to_string_lossy());
        let old = r
            .change
            .old_path
            .as_ref()
            .map(|p| crate::csv::protect(&p.to_string_lossy()))
            .unwrap_or_default();
        let time = crate::history_csv::utc_date(r.time.max(0) as u64);
        crate::csv::record(
            &mut out,
            [
                time.as_str(),
                action_id(r.change.kind),
                if r.change.is_dir { "folder" } else { "file" },
                &path,
                &old,
            ],
        );
    }
    out
}

pub fn open(main: slint::Weak<AppWindow>, engine: &Arc<Engine>) {
    let exists = STATE.with(|s| s.borrow().is_some());
    if !exists {
        let w = match JournalWindow::new() {
            Ok(w) => w,
            Err(e) => {
                crate::debug_log::note!("cannot open the index changes window: {e}");
                return;
            }
        };
        w.set_actions(ModelRc::new(VecModel::from(
            std::iter::once(tr("All actions"))
                .chain(ACTIONS.iter().map(|k| action_label(*k)))
                .map(Into::into)
                .collect::<Vec<slint::SharedString>>(),
        )));
        w.set_kinds(ModelRc::new(VecModel::from(
            [tr("Folders and files"), tr("Folders"), tr("Files")]
                .into_iter()
                .map(Into::into)
                .collect::<Vec<slint::SharedString>>(),
        )));
        w.on_filter_changed(fill_window);
        w.on_enabled_changed(|on| {
            crate::prefs::with(|p| p.set_index_journal(on));
            fill_window();
        });
        w.on_open_path(|i| {
            if let Some(path) = row_path(i) {
                if path.symlink_metadata().is_ok() {
                    crate::open::show_in_folder(&[path]);
                } else {
                    notice(tr("This entry is no longer there."));
                }
            }
        });
        let copy_main = main.clone();
        w.on_copy_path(move |i| {
            if let (Some(path), Some(m)) = (row_path(i), copy_main.upgrade()) {
                m.invoke_copy_text(path.to_string_lossy().as_ref().into());
                notice(tr("Copied."));
            }
        });
        w.on_export(export);
        w.on_clear(|| {
            STATE.with(|s| {
                if let Some(s) = s.borrow().as_ref() {
                    s.engine.clear_journal();
                }
            });
            fill_window();
        });
        w.on_close_window(|| {
            STATE.with(|s| {
                if let Some(s) = s.borrow().as_ref() {
                    s.timer.stop();
                    let _ = s.window.hide();
                }
            });
        });
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                window: w,
                engine: engine.clone(),
                shown: Vec::new(),
                timer: Timer::default(),
            })
        });
    }
    STATE.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            crate::prefs::with(|p| {
                s.window.invoke_set_color_scheme(
                    p.color_scheme(),
                    p.dark_style(),
                    p.light_style(),
                    p.accent_colors(),
                )
            });
            s.window.set_notice("".into());
            s.timer.start(
                TimerMode::Repeated,
                std::time::Duration::from_secs(2),
                fill_window,
            );
            let _ = s.window.hide();
            let _ = s.window.show();
            if let Some(main) = main.upgrade() {
                crate::wayland_parent::keep_above(&s.window, &main);
            }
        }
    });
    fill_window();
}

pub fn apply_scheme(scheme: i32, style: i32, light_style: i32, accent: crate::AccentColors) {
    STATE.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            s.window
                .invoke_set_color_scheme(scheme, style, light_style, accent);
        }
    });
}

fn notice(text: &str) {
    STATE.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            s.window.set_notice(text.into());
        }
    });
}

fn row_path(i: i32) -> Option<PathBuf> {
    STATE.with(|s| {
        let s = s.borrow();
        let s = s.as_ref()?;
        s.shown
            .get(usize::try_from(i).ok()?)
            .map(|r| r.change.path.clone())
    })
}

/// Put the identifying name first so the table's trailing ellipsis only
/// hides the containing folder. Copy, open and export keep the full path.
fn display_path(path: &Path) -> String {
    match (path.file_name(), path.parent()) {
        (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
            format!("{} — {}", name.to_string_lossy(), parent.display())
        }
        _ => path.to_string_lossy().into_owned(),
    }
}

fn fill_window() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(s) = s.as_mut() else { return };
        let w = &s.window;
        let all = s.engine.journal();
        let shown = filtered(&all, w.get_action(), w.get_kind());
        // Keep the selected row selected while new changes come in.
        let selected = usize::try_from(w.get_current())
            .ok()
            .and_then(|i| s.shown.get(i))
            .cloned();
        let zone = everygrep_core::localtime::Zone::local();
        let rows: Vec<ModelRc<StandardListViewItem>> = shown
            .iter()
            .map(|r| {
                let items = vec![
                    StandardListViewItem::from(
                        crate::format::timestamp(r.time.saturating_mul(1_000_000_000), &zone)
                            .as_str(),
                    ),
                    StandardListViewItem::from(action_label(r.change.kind)),
                    StandardListViewItem::from(if r.change.is_dir {
                        tr("Folder")
                    } else {
                        tr("File")
                    }),
                    StandardListViewItem::from(display_path(&r.change.path).as_str()),
                    StandardListViewItem::from(
                        r.change
                            .old_path
                            .as_ref()
                            .map(|p| display_path(p))
                            .unwrap_or_default()
                            .as_str(),
                    ),
                ];
                ModelRc::new(VecModel::from(items))
            })
            .collect();
        w.set_rows(ModelRc::new(VecModel::from(rows)));
        w.set_current(
            selected
                .and_then(|sel| shown.iter().position(|r| *r == sel))
                .map_or(-1, |i| i as i32),
        );
        w.set_count(
            fill(
                tr("{} of {} changes"),
                &[
                    &crate::format::count(shown.len()),
                    &crate::format::count(all.len()),
                ],
            )
            .into(),
        );
        w.set_enabled(s.engine.journal_on());
        s.shown = shown;
    });
}

fn export() {
    let shown = STATE.with(|s| {
        s.borrow().as_ref().map(|s| {
            (
                s.shown.clone(),
                crate::wayland_parent::PortalParent::of(s.window.window()),
            )
        })
    });
    let Some((rows, parent)) = shown else { return };
    let text = csv_text(&rows);
    std::thread::spawn(move || {
        let result = (|| -> Result<Option<PathBuf>, String> {
            let Some((path, _)) = crate::portal::save_file(
                parent.handle(),
                tr("Export index changes"),
                tr("Export"),
                "index-changes.csv",
                &[crate::portal::SaveFilter {
                    name: tr("CSV files"),
                    pattern: "*.csv",
                }],
            )?
            else {
                return Ok(None);
            };
            crate::settings::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
            Ok(Some(path))
        })();
        let _ = slint::invoke_from_event_loop(move || match result {
            Ok(Some(path)) => notice(&fill(tr("Saved to {}."), &[&path.display()])),
            Ok(None) => {}
            Err(e) => notice(&fill(tr("Cannot save: {}"), &[&e])),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::watch::Change;

    fn row(time: i64, kind: ChangeKind, is_dir: bool, path: &str) -> JournalRow {
        JournalRow {
            time,
            change: Change {
                kind,
                is_dir,
                path: path.into(),
                old_path: (kind == ChangeKind::Renamed).then(|| PathBuf::from("/a/old,\"x\"")),
            },
        }
    }

    #[test]
    fn journal_paths_start_with_the_name_and_keep_the_folder() {
        assert_eq!(
            display_path(Path::new("/a/long/folder/report.txt")),
            "report.txt — /a/long/folder"
        );
        assert_eq!(display_path(Path::new("/")), "/");
        assert_eq!(display_path(Path::new("report.txt")), "report.txt");
        assert_eq!(display_path(Path::new("")), "");
    }

    #[test]
    fn filters_pick_actions_and_kinds_newest_first() {
        let rows = vec![
            row(1, ChangeKind::Created, false, "/a/1"),
            row(2, ChangeKind::Deleted, true, "/a/2"),
            row(3, ChangeKind::Created, true, "/a/3"),
            row(4, ChangeKind::Renamed, false, "/a/4"),
        ];
        let paths = |v: Vec<JournalRow>| -> Vec<String> {
            v.into_iter()
                .map(|r| r.change.path.display().to_string())
                .collect()
        };
        assert_eq!(
            paths(filtered(&rows, 0, 0)),
            ["/a/4", "/a/3", "/a/2", "/a/1"]
        );
        assert_eq!(paths(filtered(&rows, 1, 0)), ["/a/3", "/a/1"]);
        assert_eq!(paths(filtered(&rows, 1, 1)), ["/a/3"]);
        assert_eq!(paths(filtered(&rows, 0, 2)), ["/a/4", "/a/1"]);
        assert_eq!(paths(filtered(&rows, 3, 0)), ["/a/4"]);
        assert!(filtered(&rows, 99, 0).is_empty());
    }

    #[test]
    fn csv_export_quotes_paths_and_reads_back() {
        let rows = vec![
            row(86_400, ChangeKind::Renamed, false, "/a/new\nline"),
            row(0, ChangeKind::Deleted, true, "/-dash"),
        ];
        let text = csv_text(&rows);
        let back = crate::csv::rows(&text).unwrap();
        assert_eq!(back[0], ["Time", "Action", "Type", "Path", "Old Path"]);
        assert_eq!(
            back[1],
            [
                "1970-01-02T00:00:00Z",
                "renamed",
                "file",
                "/a/new\nline",
                "/a/old,\"x\""
            ]
        );
        assert_eq!(back[2][..3], ["1970-01-01T00:00:00Z", "deleted", "folder"]);
        assert_eq!(back[2][3], "/-dash");
    }
}
