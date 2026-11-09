//! Help → Debug log: a window over [`crate::debug_log`] with Refresh, Copy
//! and Save.

use std::cell::RefCell;

use slint::{ComponentHandle, Weak};

use crate::i18n::{fill, tr};
use crate::{AppWindow, DebugLogWindow};

thread_local! {
    static WINDOW: RefCell<Option<DebugLogWindow>> = const { RefCell::new(None) };
}

/// Opens the window, or brings it to the front with the lines read again.
pub fn open(main: Weak<AppWindow>) {
    WINDOW.with(|slot| {
        if slot.borrow().is_none() {
            let w = match DebugLogWindow::new() {
                Ok(w) => w,
                Err(e) => {
                    crate::debug_log::note!("cannot open the debug log window: {e}");
                    return;
                }
            };
            let weak = w.as_weak();
            w.on_refresh(move || {
                if let Some(w) = weak.upgrade() {
                    fill_window(&w);
                }
            });
            let (weak, main) = (w.as_weak(), main.clone());
            w.on_copy(move || {
                if let (Some(w), Some(m)) = (weak.upgrade(), main.upgrade()) {
                    fill_window(&w);
                    m.invoke_copy_text(w.get_log_text());
                    w.set_notice(tr("Copied.").into());
                }
            });
            let weak = w.as_weak();
            w.on_save(move || {
                let Some(w) = weak.upgrade() else { return };
                let parent = crate::wayland_parent::PortalParent::of(w.window());
                let weak = weak.clone();
                let text = crate::debug_log::text();
                std::thread::spawn(move || {
                    let result = save(parent.handle(), &text);
                    let _ = weak.upgrade_in_event_loop(move |w| match result {
                        Ok(Some(path)) => {
                            w.set_notice(fill(tr("Saved to {}."), &[&path.display()]).into())
                        }
                        Ok(None) => {}
                        Err(e) => w.set_notice(fill(tr("Cannot save: {}"), &[&e]).into()),
                    });
                });
            });
            let weak = w.as_weak();
            w.on_close_window(move || {
                if let Some(w) = weak.upgrade() {
                    let _ = w.hide();
                }
            });
            *slot.borrow_mut() = Some(w);
        }
        if let Some(w) = slot.borrow().as_ref() {
            crate::prefs::with(|p| {
                w.invoke_set_color_scheme(
                    p.color_scheme(),
                    p.dark_style(),
                    p.light_style(),
                    p.accent_colors(),
                )
            });
            fill_window(w);
            w.set_notice("".into());
            // Shown again for a new surface, which GNOME brings to the front.
            let _ = w.hide();
            let _ = w.show();
            if let Some(main) = main.upgrade() {
                crate::wayland_parent::keep_above(w, &main);
            }
        }
    });
}

fn fill_window(w: &DebugLogWindow) {
    w.set_log_text(crate::debug_log::text().into());
    w.set_where(where_text().into());
}

/// Whether the lines are also written to a file, and where.
fn where_text() -> String {
    match crate::debug_log::writing_to() {
        Some(path) => fill(
            tr("The last {} lines are kept in memory and also written to {}."),
            &[&crate::debug_log::CAPACITY, &path.display()],
        ),
        None => fill(
            tr(
                "The last {} lines are kept in memory only. Start EveryGrep with --debug-log to write them to a file as well.",
            ),
            &[&crate::debug_log::CAPACITY],
        ),
    }
}

fn save(parent: &str, text: &str) -> Result<Option<std::path::PathBuf>, String> {
    let Some((path, _)) = crate::portal::save_file(
        parent,
        tr("Save the debug log"),
        tr("Save"),
        "everygrep-debug.log",
        &[crate::portal::SaveFilter {
            name: tr("Log files"),
            pattern: "*.log",
        }],
    )?
    else {
        return Ok(None);
    };
    crate::settings::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

pub fn apply_scheme(scheme: i32, style: i32, light_style: i32, accent: crate::AccentColors) {
    WINDOW.with(|slot| {
        if let Some(w) = slot.borrow().as_ref() {
            w.invoke_set_color_scheme(scheme, style, light_style, accent);
        }
    });
}
