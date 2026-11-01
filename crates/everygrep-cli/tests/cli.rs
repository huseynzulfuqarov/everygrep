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

#[test]
fn matching_mode_flags() {
    let dir = temp_dir("modes");
    let tree = dir.join("tree");
    fs::write(tree.join("my notes.txt"), b"m").unwrap();
    let index = dir.join("index.bin");
    let (tree_s, index_s) = (tree.to_str().unwrap(), index.to_str().unwrap());
    let found = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["search", "--root", tree_s, "--index-file", index_s];
        args.extend_from_slice(extra);
        let out = run(&args);
        assert!(out.status.success(), "{extra:?}");
        let mut names: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| l.rsplit('/').next().unwrap().to_owned())
            .collect();
        names.sort();
        names
    };
    let prefix = found(&["--prefix", "no"]);
    let suffix = found(&["--suffix", "txt"]);
    let punct = found(&["--ignore-punctuation", "holidayphoto"]);
    let space = found(&["--ignore-whitespace", "mynotes"]);
    let regex = found(&["--regex", r"^(my|no)\w* ?notes"]);
    let bad = run(&["search", "--index-file", index_s, "--regex", "("]);
    fs::remove_dir_all(&dir).unwrap();

    assert_eq!(prefix, ["notes.txt"]);
    assert_eq!(suffix, ["my notes.txt", "notes.txt"]);
    assert_eq!(punct, ["Holiday-Photo.JPG"]);
    assert_eq!(space, ["my notes.txt"]);
    assert_eq!(regex, ["my notes.txt"]);
    // As for other bad queries: a message, no results.
    assert!(bad.stdout.is_empty());
    let stderr = String::from_utf8(bad.stderr).unwrap();
    assert!(stderr.contains("bad regular expression"), "{stderr}");
}

#[test]
fn a_root_missing_at_scan_is_found_when_it_returns() {
    let dir = temp_dir("returning-root");
    let tree = dir.join("tree");
    let missing = dir.join("missing");
    let index = dir.join("index.bin");
    assert!(
        run(&[
            "index",
            "--root",
            tree.to_str().unwrap(),
            "--root",
            missing.to_str().unwrap(),
            "--index-file",
            index.to_str().unwrap()
        ])
        .status
        .success()
    );
    // It may remain absent across several sessions.
    assert!(
        run(&["search", "--index-file", index.to_str().unwrap(), "secret"])
            .status
            .success()
    );
    fs::create_dir(&missing).unwrap();
    fs::write(missing.join("secret.txt"), b"secret").unwrap();
    let found = run(&["search", "--index-file", index.to_str().unwrap(), "secret"]);
    assert!(found.status.success());
    assert_eq!(
        String::from_utf8(found.stdout).unwrap().trim(),
        missing.join("secret.txt").to_str().unwrap()
    );
    assert!(String::from_utf8(found.stderr).unwrap().contains("loaded"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn index_returns_failure_and_cleans_up_when_saving_fails() {
    let dir = temp_dir("failed-save");
    let destination = dir.join("is-a-directory");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), b"keep").unwrap();
    let output = run(&[
        "index",
        "--root",
        dir.join("tree").to_str().unwrap(),
        "--index-file",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("could not save index")
    );
    assert_eq!(fs::read(destination.join("keep")).unwrap(), b"keep");
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        2,
        "temporary file leaked"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn loading_a_no_meta_index_does_not_populate_file_metadata() {
    let dir = temp_dir("no-meta");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    assert!(
        run(&[
            "index",
            "--no-meta",
            "--root",
            tree.to_str().unwrap(),
            "--index-file",
            index.to_str().unwrap()
        ])
        .status
        .success()
    );
    let found = run(&[
        "search",
        "--no-meta",
        "--index-file",
        index.to_str().unwrap(),
        "file: size:>0",
    ]);
    assert!(found.status.success());
    assert!(found.stdout.is_empty());
    let stderr = String::from_utf8(found.stderr).unwrap();
    assert!(stderr.contains("re-read 0"), "{stderr}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cached_rules_are_rebuilt_when_tightened_or_relaxed() {
    let dir = temp_dir("changed-rules");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    let search = |rules: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_everygrep"))
            .env("HOME", dir.join("unused-home"))
            .args(["search", "--index-file", index.to_str().unwrap()])
            .args(rules)
            .arg("Holiday")
            .output()
            .unwrap()
    };
    for rules in [
        ["--exclude-files", "*.JPG"],
        ["--include-files", "*.txt"],
        ["--exclude", "sub"],
    ] {
        assert!(
            run(&[
                "index",
                "--root",
                tree.to_str().unwrap(),
                "--index-file",
                index.to_str().unwrap()
            ])
            .status
            .success()
        );
        let tightened = search(&rules);
        assert!(tightened.status.success());
        assert!(tightened.stdout.is_empty(), "rules {rules:?} were ignored");
        assert!(
            String::from_utf8(tightened.stderr)
                .unwrap()
                .contains("indexed")
        );
        // The same rules now reuse the saved cache.
        let unchanged = search(&rules);
        assert!(unchanged.status.success());
        assert!(
            String::from_utf8(unchanged.stderr)
                .unwrap()
                .contains("loaded")
        );
        let relaxed = search(&[]);
        assert!(relaxed.status.success());
        assert!(
            String::from_utf8(relaxed.stdout)
                .unwrap()
                .contains("Holiday-Photo.JPG"),
            "rules {rules:?} were not relaxed"
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn legacy_cache_with_unknown_rules_is_rebuilt_using_its_saved_roots() {
    let dir = temp_dir("legacy-rules");
    let tree = dir.join("tree");
    let index = dir.join("index.bin");
    assert!(
        run(&[
            "index",
            "--root",
            tree.to_str().unwrap(),
            "--index-file",
            index.to_str().unwrap()
        ])
        .status
        .success()
    );
    let saved = everygrep_core::persist::load(&index).unwrap();
    let order: Vec<_> = saved.index.order().iter().collect();
    everygrep_core::persist::rewrite_order(&index, 5, &order).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_everygrep"))
        .env("HOME", dir.join("unused-home"))
        .args(["search", "--index-file", index.to_str().unwrap(), "Holiday"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Holiday-Photo.JPG")
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("indexed")
    );
    assert!(
        everygrep_core::persist::load(&index)
            .unwrap()
            .rules_hash
            .is_some()
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn non_utf8_root_index_and_excluded_paths_are_preserved() {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let dir = temp_dir("raw-paths");
    let tree = dir.join(OsString::from_vec(b"root-\xff".to_vec()));
    let excluded = tree.join(OsString::from_vec(b"excluded-\xfe".to_vec()));
    let index = dir.join(OsString::from_vec(b"index-\xfd.bin".to_vec()));
    fs::create_dir_all(&excluded).unwrap();
    fs::write(tree.join("notes.txt"), b"notes").unwrap();
    fs::write(excluded.join("hidden.txt"), b"hidden").unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_everygrep"))
        .args(["index", "--root"])
        .arg(&tree)
        .arg("--exclude")
        .arg(&excluded)
        .arg("--index-file")
        .arg(&index)
        .output()
        .unwrap();
    assert!(built.status.success(), "{:?}", built.stderr);
    let search = |query| {
        Command::new(env!("CARGO_BIN_EXE_everygrep"))
            .args(["search", "--index-file"])
            .arg(&index)
            .arg("--exclude")
            .arg(&excluded)
            .arg(query)
            .output()
            .unwrap()
    };
    let found = search("notes");
    assert!(found.status.success());
    let mut expected = tree.join("notes.txt").as_os_str().as_bytes().to_vec();
    expected.push(b'\n');
    assert_eq!(found.stdout, expected);
    assert!(String::from_utf8(found.stderr).unwrap().contains("loaded"));
    let hidden = search("hidden");
    assert!(hidden.status.success());
    assert!(hidden.stdout.is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn non_utf8_text_arguments_return_usage_errors_without_panicking() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let invalid = OsString::from_vec(vec![0xff]);
    for prefix in [
        &[][..],
        &["search"][..],
        &["search", "--include-files"][..],
        &["search", "--exclude-files"][..],
        &["search", "--filter"][..],
        &["search", "--threads"][..],
        &["search", "--limit"][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_everygrep"))
            .args(prefix)
            .arg(&invalid)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{prefix:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("valid UTF-8")
        );
    }
}
