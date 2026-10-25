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
fn search_uses_saved_index_when_no_root_is_given() {
    let dir = temp_dir("saved");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    let (tree_s, index_s) = (tree.to_str().unwrap(), index.to_str().unwrap());

    let built = run(&["index", "--root", tree_s, "--index-file", index_s]);
    assert!(built.status.success());
    assert!(index.exists());

    // Remove a file after indexing: a search that rescans would not find it.
    fs::remove_file(tree.join("sub/Holiday-Photo.JPG")).unwrap();
    let found = run(&["search", "--index-file", index_s, "photo"]);
    let stdout = String::from_utf8(found.stdout).unwrap();
    let stderr = String::from_utf8(found.stderr).unwrap();
    fs::remove_dir_all(&dir).unwrap();

    assert!(found.status.success());
    assert!(stderr.contains("loaded"), "stderr: {stderr}");
    assert_eq!(
        stdout.trim(),
        tree.join("sub/Holiday-Photo.JPG").to_str().unwrap()
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
fn unknown_command_fails_with_usage() {
    let out = run(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("usage:"));
}
