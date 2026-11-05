//! Handing files to the desktop: opening them and showing them in the file
//! manager. Both run detached so the window never waits for another program.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Opens a file or folder with its default application.
pub fn open(path: &Path) {
    spawn(Command::new("xdg-open").arg(path));
}

/// Opens the containing folders in the file manager with the items selected,
/// using the freedesktop `FileManager1` interface. Falls back to opening the
/// parent folder of the first item. The call waits for the file manager,
/// which may have to start first (or not answer for long), so it runs on a
/// thread of its own.
pub fn show_in_folder(paths: &[PathBuf]) {
    show_in_folder_with(Path::new("gdbus"), paths);
}

/// [`show_in_folder`] with the `gdbus` program at `gdbus`.
fn show_in_folder_with(gdbus: &Path, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let (gdbus, paths) = (gdbus.to_path_buf(), paths.to_vec());
    std::thread::spawn(move || show_items(&gdbus, &paths));
}

fn show_items(gdbus: &Path, paths: &[PathBuf]) {
    let Some(first) = paths.first() else { return };
    let status = Command::new(gdbus)
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
            &uri_list(paths),
            "",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !status.is_ok_and(|s| s.success())
        && let Some(parent) = first.parent()
    {
        open(parent);
    }
}

/// A GVariant string array of `file://` URIs. URIs hold no quotes or
/// backslashes, so no escaping is needed.
fn uri_list(paths: &[PathBuf]) -> String {
    let uris: Vec<String> = paths.iter().map(|p| format!("'{}'", file_uri(p))).collect();
    format!("[{}]", uris.join(", "))
}

fn spawn(cmd: &mut Command) {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(child) = child {
        reap(child);
    }
}

/// Waits for `child` on a thread of its own, so it does not stay as a
/// zombie once it ends.
pub(crate) fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// `file://` URI with percent-encoding of every byte outside the unreserved set
/// (and `/`), as required for paths with spaces, quotes or non-ASCII bytes.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn showing_items_does_not_wait_for_the_file_manager() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("everygrep-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A file manager that takes long to answer, then says it showed them.
        let gdbus = dir.join("gdbus");
        let done = dir.join("answered");
        std::fs::write(
            &gdbus,
            format!("#!/bin/sh\nsleep 1\ntouch '{}'\n", done.display()),
        )
        .unwrap();
        std::fs::set_permissions(&gdbus, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = std::time::Instant::now();
        show_in_folder_with(&gdbus, &[dir.join("item")]);
        let took = start.elapsed();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let answered = done.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        assert!(answered);
    }

    #[test]
    fn uri_lists_are_gvariant_arrays() {
        let paths = [PathBuf::from("/a b"), PathBuf::from("/c'd")];
        assert_eq!(uri_list(&paths), "['file:///a%20b', 'file:///c%27d']");
    }

    #[test]
    fn uris_escape_spaces_quotes_and_non_ascii() {
        assert_eq!(
            file_uri(Path::new("/home/a b/it's.txt")),
            "file:///home/a%20b/it%27s.txt"
        );
        assert_eq!(
            file_uri(Path::new("/tmp/şəkil.png")),
            "file:///tmp/%C5%9F%C9%99kil.png"
        );
        assert_eq!(
            file_uri(Path::new(OsStr::from_bytes(b"/x/\xff"))),
            "file:///x/%FF"
        );
    }
}
