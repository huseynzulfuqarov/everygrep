//! Creating links in a folder picked by the user, without replacing anything.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::ComponentHandle;

use crate::i18n::{fill, tr};
use crate::{AppWindow, clipboard, portal, prefs, results::ResultsModel};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Symlink,
    Desktop,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Collision {
    #[default]
    Number,
    Skip,
}

/// Desktop entry strings use their own escaping, not shell quoting.
fn desktop_text(source: &Path) -> String {
    let name = source.file_name().unwrap_or_default().to_string_lossy();
    let mut escaped = String::new();
    for c in name.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ' ' => escaped.push_str("\\s"),
            _ => escaped.push(c),
        }
    }
    let uri = String::from_utf8(clipboard::uri_list(&[source.to_path_buf()]))
        .expect("file URIs are ASCII");
    format!(
        "[Desktop Entry]\nType=Link\nName={escaped}\nURL={}\n",
        uri.trim_end()
    )
}

fn candidate(source: &Path, folder: &Path, kind: Kind, number: u64) -> io::Result<PathBuf> {
    let name = source
        .file_name()
        .ok_or(io::ErrorKind::InvalidInput)?
        .as_bytes();
    let suffix = format!(
        "{}{}",
        if number == 1 {
            String::new()
        } else {
            format!(" ({number})")
        },
        if kind == Kind::Desktop {
            ".desktop"
        } else {
            ""
        }
    );
    // A directory entry is limited in bytes, including our suffix.
    let mut end = name.len().min(255 - suffix.len());
    if let Ok(text) = std::str::from_utf8(name) {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    let mut bytes = name[..end].to_vec();
    bytes.extend_from_slice(suffix.as_bytes());
    Ok(folder.join(std::ffi::OsString::from_vec(bytes)))
}

/// Creates a link, or returns None for an existing name with Skip selected.
/// The target path is absolute and symlinks at the source are not resolved.
pub fn create(
    source: &Path,
    folder: &Path,
    kind: Kind,
    collision: Collision,
) -> io::Result<Option<PathBuf>> {
    if !source.is_absolute() || !folder.is_absolute() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    fs::symlink_metadata(source)?;
    for number in 1.. {
        let target = candidate(source, folder, kind, number)?;
        let result = match kind {
            Kind::Symlink => std::os::unix::fs::symlink(source, &target),
            Kind::Desktop => {
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                {
                    Ok(mut file) => {
                        let result = file
                            .write_all(desktop_text(source).as_bytes())
                            .and_then(|()| file.sync_all());
                        if result.is_err() {
                            let _ = fs::remove_file(&target);
                        }
                        result
                    }
                    Err(e) => Err(e),
                }
            }
        };
        match result {
            Ok(()) => return Ok(Some(target)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if collision == Collision::Skip {
                    return Ok(None);
                }
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

pub fn connect(w: &AppWindow, model: &Rc<ResultsModel>) {
    w.on_fileops_shortcuts({
        let (weak, model) = (w.as_weak(), model.clone());
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        move |row| {
            let row = usize::try_from(row).ok();
            if crate::refuse_offline(&weak, &model, row) {
                return;
            }
            let sources = model.targets(row);
            let Some(w) = weak.upgrade() else { return };
            if sources.is_empty() || busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let mut options = crate::fileops_settings::Settings::default();
            prefs::with(|p| options = p.fileops_settings());
            let (weak, busy) = (w.as_weak(), busy.clone());
            std::thread::spawn(move || {
                let result =
                    portal::choose_folder(tr("Create shortcuts"), tr("Create")).map(|folder| {
                        folder.map(|folder| {
                            let mut made = 0;
                            let mut skipped = 0;
                            let mut failures = Vec::new();
                            for source in sources {
                                match create(
                                    &source,
                                    &folder,
                                    options.shortcut_kind,
                                    options.shortcut_collision,
                                ) {
                                    Ok(Some(_)) => made += 1,
                                    Ok(None) => skipped += 1,
                                    Err(e) => failures.push(format!("{}: {e}", source.display())),
                                }
                            }
                            (made, skipped, failures)
                        })
                    });
                let _ = weak.upgrade_in_event_loop(move |w| {
                    busy.store(false, std::sync::atomic::Ordering::SeqCst);
                    match result {
                        Ok(Some((made, skipped, failures))) => {
                            let text = fill(
                                tr("Shortcuts created: {}; skipped: {}."),
                                &[&made, &skipped],
                            );
                            if let Some(first) = failures.first() {
                                crate::error(
                                    &w,
                                    fill(
                                        tr("{} Failed: {}. First error: {}"),
                                        &[&text, &failures.len(), first],
                                    ),
                                );
                            } else {
                                crate::info(&w, text);
                            }
                        }
                        Ok(None) => {}
                        Err(e) => crate::error(&w, fill(tr("Cannot create shortcuts: {}"), &[&e])),
                    }
                });
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_preserve_sources_and_never_replace_files_or_dangling_links() {
        let dir = std::env::temp_dir().join(format!("everygrep-shortcuts-{}", std::process::id()));
        fs::create_dir_all(dir.join("out")).unwrap();
        let source = dir.join("a b\n[Entry].txt");
        fs::write(&source, "keep").unwrap();
        let folder = dir.join("out");
        let first = create(&source, &folder, Kind::Symlink, Collision::Number)
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_link(&first).unwrap(), source);
        assert_eq!(
            create(&source, &folder, Kind::Symlink, Collision::Skip).unwrap(),
            None
        );
        fs::remove_file(&first).unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), &first).unwrap();
        let next = create(&source, &folder, Kind::Symlink, Collision::Number)
            .unwrap()
            .unwrap();
        assert_ne!(next, first);
        assert_eq!(fs::read_link(&first).unwrap(), dir.join("missing"));
        let desktop = create(&source, &folder, Kind::Desktop, Collision::Number)
            .unwrap()
            .unwrap();
        let text = fs::read_to_string(&desktop).unwrap();
        assert_eq!(text.lines().count(), 4);
        assert!(text.contains("Name=a\\sb\\n[Entry].txt\n"));
        assert!(text.contains("%20b%0A%5BEntry%5D.txt"));
        assert!(!text.contains("Exec="));
        assert_eq!(fs::read_to_string(&source).unwrap(), "keep");
        let tree = dir.join("tree");
        fs::create_dir(&tree).unwrap();
        let link = create(&tree, &folder, Kind::Symlink, Collision::Number)
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_link(link).unwrap(), tree);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn suffixes_fit_long_names_and_missing_sources_fail() {
        let folder = Path::new("/tmp");
        let source = Path::new("/tmp").join("x".repeat(255));
        for kind in [Kind::Symlink, Kind::Desktop] {
            assert_eq!(
                candidate(&source, folder, kind, 2)
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .as_bytes()
                    .len(),
                255
            );
        }
        assert!(
            create(
                Path::new("relative"),
                folder,
                Kind::Symlink,
                Collision::Number
            )
            .is_err()
        );
        assert!(
            create(
                Path::new("/nonexistent-shortcut-source"),
                folder,
                Kind::Symlink,
                Collision::Number
            )
            .is_err()
        );
    }
}
