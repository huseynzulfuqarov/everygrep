//! Command-line arguments of the window program. Only `--help` and wrong
//! arguments are tried: both end before a window is made.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

fn run(args: &[&OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_everygrep-gui"))
        .args(args)
        .output()
        .expect("run everygrep-gui")
}

#[test]
fn a_path_that_is_not_utf8_is_taken_as_it_is() {
    let path = OsStr::from_bytes(b"/tmp/no-such-\xff-folder");
    let out = run(&[OsStr::new("--root"), path, OsStr::new("--help")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{:?}: {stderr}", out.status);
    assert!(String::from_utf8_lossy(&out.stdout).contains("--root"));
}

#[test]
fn other_values_that_are_not_utf8_are_refused() {
    let out = run(&[OsStr::new("--filter"), OsStr::from_bytes(b"a\xff")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("not valid UTF-8"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
