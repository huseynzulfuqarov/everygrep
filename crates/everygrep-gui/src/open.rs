//! Handing files to the desktop: opening them and showing them in the file
//! manager. Both run detached so the window never waits for another program.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Report = Box<dyn Fn(String) + Send>;
/// Where a failure to open is shown (the window's notice line).
static REPORT: Mutex<Option<Report>> = Mutex::new(None);

/// Sets where failures to open a file are shown.
pub fn on_failure(report: impl Fn(String) + Send + 'static) {
    if let Ok(mut r) = REPORT.lock() {
        *r = Some(Box::new(report));
    }
}

/// Opens a file or folder with its default application. When `xdg-open`
/// is missing or fails, the reason is shown (see [`on_failure`]).
pub fn open(path: &Path) {
    open_with(Path::new("xdg-open"), path, |text| {
        if let Ok(r) = REPORT.lock()
            && let Some(report) = r.as_ref()
        {
            report(text);
        }
    });
}

/// A failing exit within this time is the opener's own (no application for
/// the type, or it could not start one). Some openers wait for the
/// application instead, whose exit status says nothing about the opening.
const OPENER_FAILS_WITHIN: Duration = Duration::from_secs(10);

/// [`open`] with the opener program at `opener`; `report` gets the reason
/// when it cannot be run or fails.
fn open_with(opener: &Path, path: &Path, report: impl FnOnce(String) + Send + 'static) {
    let started = Instant::now();
    let child = Command::new(opener)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let path = path.to_path_buf();
    match child {
        Err(e) => report(crate::labels::cannot_run_opener(&path, &e)),
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Ok(status) = child.wait()
                    && let Some(code) = status.code()
                    && code != 0
                    && started.elapsed() < OPENER_FAILS_WITHIN
                {
                    report(crate::labels::opener_failed(&path, code));
                }
            });
        }
    }
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

    /// Writes an executable shell script with `body` at `path`, from a shell
    /// of its own. Written by this process, the file would be open for
    /// writing while another test starts a program, whose new process keeps
    /// a copy of it until that program runs; running the script then fails
    /// with "Text file busy".
    fn write_script(path: &Path, body: &str) {
        let written = Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"",
                "sh",
            ])
            .arg(format!("#!/bin/sh\n{body}"))
            .arg(path)
            .status()
            .unwrap();
        assert!(written.success(), "{}", path.display());
    }

    #[test]
    fn showing_items_does_not_wait_for_the_file_manager() {
        let dir = std::env::temp_dir().join(format!("everygrep-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A file manager that takes long to answer, then says it showed them.
        let gdbus = dir.join("gdbus");
        let done = dir.join("answered");
        write_script(&gdbus, &format!("sleep 1\ntouch '{}'\n", done.display()));
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
    fn a_missing_or_failing_opener_is_reported() {
        use std::sync::mpsc;
        let dir = std::env::temp_dir().join(format!("everygrep-opener-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("some file.txt");
        let opener = |name: &str, code: i32| {
            let p = dir.join(name);
            write_script(&p, &format!("exit {code}\n"));
            p
        };
        // Each run reports at most once; the channel closes when it is done.
        let run = |program: &Path| {
            let (send, receive) = mpsc::channel();
            open_with(program, &target, move |text| {
                let _ = send.send(text);
            });
            let first = receive.recv_timeout(Duration::from_secs(10)).ok();
            assert!(receive.recv_timeout(Duration::from_secs(10)).is_err());
            first
        };
        let missing = run(&dir.join("no-such-opener")).expect("missing opener reported");
        assert!(missing.contains("some file.txt"), "{missing}");
        let failed = run(&opener("fails", 4)).expect("failing opener reported");
        assert!(
            failed.contains("some file.txt") && failed.contains('4'),
            "{failed}"
        );
        assert_eq!(run(&opener("works", 0)), None);
        let _ = std::fs::remove_dir_all(&dir);
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
