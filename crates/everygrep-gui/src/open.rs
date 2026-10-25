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
/// parent folder of the first item.
pub fn show_in_folder(paths: &[PathBuf]) {
    let Some(first) = paths.first() else { return };
    let status = Command::new("gdbus")
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
    if !status.is_ok_and(|s| s.success()) {
        if let Some(parent) = first.parent() {
            open(parent);
        }
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
    if let Ok(mut child) = child {
        // Reap the process when it ends so it does not stay as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
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
