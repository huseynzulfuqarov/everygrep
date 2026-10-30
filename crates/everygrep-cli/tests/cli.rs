//! End-to-end tests that run the `everygrep` binary.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("everygrep-cli-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("tree/sub")).unwrap();
    fs::write(dir.join("tree/sub/Holiday-Photo.JPG"), b"x").unwrap();
    fs::write(dir.join("tree/notes.txt"), b"y").unwrap();
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_everygrep"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn search_loads_saved_index_and_catches_up_without_full_scan() {
    let dir = temp_dir("saved");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    let (tree_s, index_s) = (tree.to_str().unwrap(), index.to_str().unwrap());

    let built = run(&["index", "--root", tree_s, "--index-file", index_s]);
    assert!(built.status.success());
    assert!(index.exists());

    // Changed while nothing was running.
    fs::remove_file(tree.join("sub/Holiday-Photo.JPG")).unwrap();
    fs::write(tree.join("sub/holiday-video.mp4"), b"v").unwrap();
    let found = run(&["search", "--index-file", index_s, "holiday"]);
    let stdout = String::from_utf8(found.stdout).unwrap();
    let stderr = String::from_utf8(found.stderr).unwrap();
    fs::remove_dir_all(&dir).unwrap();

    assert!(found.status.success());
    assert!(stderr.contains("loaded"), "stderr: {stderr}");
    assert!(stderr.contains("caught up"), "stderr: {stderr}");
    assert!(
        !stderr.contains("indexed"),
        "must not rescan fully, stderr: {stderr}"
    );
    assert_eq!(
        stdout.trim(),
        tree.join("sub/holiday-video.mp4").to_str().unwrap()
    );
}

#[test]
fn rescan_reflects_changes() {
    let dir = temp_dir("rescan");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    let (tree_s, index_s) = (tree.to_str().unwrap(), index.to_str().unwrap());

    assert!(
        run(&["index", "--root", tree_s, "--index-file", index_s])
            .status
            .success()
    );
    fs::write(tree.join("sub/new-file.txt"), b"z").unwrap();
    let out = run(&[
        "search",
        "--root",
        tree_s,
        "--index-file",
        index_s,
        "--rescan",
        "new-file",
    ]);
    fs::remove_dir_all(&dir).unwrap();

    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("new-file.txt")
    );
}

#[test]
fn filter_limits_results_to_a_kind() {
    let dir = temp_dir("filter");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    let (tree_s, index_s) = (tree.to_str().unwrap(), index.to_str().unwrap());
    let search = |filter: &str| {
        run(&[
            "search",
            "--root",
            tree_s,
            "--index-file",
            index_s,
            "--filter",
            filter,
        ])
    };
    let pictures = search("image");
    let folders = search("folder");
    let bad = search("images");
    fs::remove_dir_all(&dir).unwrap();

    assert!(pictures.status.success());
    let stdout = String::from_utf8(pictures.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.contains("Holiday-Photo.JPG"));
    let stdout = String::from_utf8(folders.stdout).unwrap();
    assert!(stdout.lines().all(|l| !l.contains('.')), "{stdout}");
    assert!(stdout.contains("sub"));
    assert!(!bad.status.success());
}

#[test]
fn unknown_command_fails_with_usage() {
    let out = run(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("usage:"));
}

#[test]
fn watching_shell_sees_new_files_and_saves_them() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = temp_dir("watch");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    let (tree_s, index_s) = (tree.to_str().unwrap(), index.to_str().unwrap());

    let mut child = Command::new(env!("CARGO_BIN_EXE_everygrep"))
        .args([
            "shell",
            "--watch",
            "--root",
            tree_s,
            "--index-file",
            index_s,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "notes").unwrap();
    stdin.flush().unwrap();
    // Give the shell time to scan and set up its watches before the change.
    std::thread::sleep(std::time::Duration::from_millis(500));
    fs::write(tree.join("sub/fresh-report.md"), b"new").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    writeln!(stdin, "fresh").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();

    let saved = run(&["search", "--index-file", index_s, "fresh"]);
    fs::remove_dir_all(&dir).unwrap();

    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("notes.txt"), "stdout: {stdout}");
    assert!(
        stdout.contains("fresh-report.md"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stderr.contains("applied changes"), "stderr: {stderr}");
    assert!(
        String::from_utf8(saved.stdout)
            .unwrap()
            .contains("fresh-report.md")
    );
}

#[test]
fn duplicate_and_relative_roots_still_load() {
    let dir = temp_dir("roots");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    let (tree_s, index_s) = (tree.to_str().unwrap(), index.to_str().unwrap());

    assert!(
        run(&[
            "index",
            "--root",
            tree_s,
            "--root",
            tree_s,
            "--index-file",
            index_s
        ])
        .status
        .success()
    );
    // The same folder given as a relative path from its parent.
    let out = Command::new(env!("CARGO_BIN_EXE_everygrep"))
        .current_dir(&dir)
        .args([
            "search",
            "--root",
            "tree",
            "--root",
            "./tree/sub",
            "--index-file",
            index_s,
            "notes",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert!(stderr.contains("loaded"), "stderr: {stderr}");
    assert_eq!(stdout.trim(), tree.join("notes.txt").to_str().unwrap());
}
