//! File actions on result rows: text for the clipboard, rename, trash and
//! permanent delete. The index is not touched here; the watcher sees the
//! changes on disk and updates it.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::i18n::{fill, tr};

/// Paths passed to one `gio trash` call, to stay well below the argument
/// size limit.
const TRASH_BATCH: usize = 256;

/// Longest file name Linux file systems accept, in bytes.
const NAME_MAX: usize = 255;

/// Full paths, one per line, for the clipboard; with `quote` each one
/// quoted as [`quote`] does.
pub fn paths_text(paths: &[PathBuf], quote: bool) -> String {
    join_lines(paths.iter().map(|p| p.as_os_str()), quote)
}

/// Whether opening `n` items at once asks first: more than `over` of them,
/// unless `over` is 0 (never ask).
pub fn open_needs_confirm(n: usize, over: usize) -> bool {
    over > 0 && n > over
}

/// Characters that a shell still reads inside double quotes: `$` and
/// backticks run commands, `\` and `"` escape, `!` is history expansion in
/// bash and zsh. A line break would split one item over two lines.
const DOUBLE_QUOTE_SPECIAL: &[char] = &['$', '`', '"', '\\', '!', '\n'];

/// One item quoted for pasting into a shell (sh, bash, zsh, fish) or a
/// file dialog. Usually in double quotes, `"/home/me/a b.txt"`; an item with
/// a character a shell reads inside double quotes goes in single quotes
/// instead, with `'` written as `'\''` and `\` as `'\\'`, so the shell takes
/// it as one literal word and never runs a part of it.
pub fn quote(item: &str) -> String {
    if !item.contains(DOUBLE_QUOTE_SPECIAL) {
        return format!("\"{item}\"");
    }
    let mut out = String::with_capacity(item.len() + 2);
    out.push('\'');
    for c in item.chars() {
        match c {
            '\'' => out.push_str("'\\''"),
            // fish reads `\\` and `\'` inside single quotes, so a backslash
            // goes outside them.
            '\\' => out.push_str("'\\\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// File names, one per line, for the clipboard.
pub fn names_text(paths: &[PathBuf]) -> String {
    join_lines(
        paths.iter().map(|p| p.file_name().unwrap_or(p.as_os_str())),
        false,
    )
}

/// The containing folders of `paths`, each once, in the order first met.
pub fn folders(paths: &[PathBuf]) -> Vec<&Path> {
    let mut seen = std::collections::HashSet::new();
    paths
        .iter()
        .filter_map(|p| p.parent())
        .filter(|f| !f.as_os_str().is_empty())
        .filter(|f| seen.insert(*f))
        .collect()
}

/// The containing folders, one per line, for the clipboard. A folder is
/// given once even when several items are in it, in the order first met;
/// with `quote` each one quoted as [`quote`] does.
pub fn folders_text(paths: &[PathBuf], quote: bool) -> String {
    join_lines(folders(paths).into_iter().map(|f| f.as_os_str()), quote)
}

/// Joins with `\n`, each item quoted with `quote`. Bytes that are not UTF-8
/// become U+FFFD; the clipboard takes text only.
fn join_lines<'a>(items: impl Iterator<Item = &'a OsStr>, quote: bool) -> String {
    let mut out = String::new();
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let text = String::from_utf8_lossy(item.as_bytes());
        if quote {
            out.push_str(&self::quote(&text));
        } else {
            out.push_str(&text);
        }
    }
    out
}

/// Length in bytes of the part of a name a rename field preselects: the
/// part before the extension with `name_only`, otherwise the whole name.
pub fn rename_selection(name: &str, is_dir: bool, name_only: bool) -> usize {
    if name_only {
        stem_len(name, is_dir)
    } else {
        name.len()
    }
}

/// Length in bytes of the part of a name before its extension. Folders and
/// names without an extension are taken whole.
pub fn stem_len(name: &str, is_dir: bool) -> usize {
    if is_dir {
        return name.len();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => dot,
        _ => name.len(),
    }
}

/// Why a rename was refused or failed.
#[derive(Debug)]
pub enum RenameError {
    Empty,
    Slash,
    Nul,
    Reserved,
    TooLong,
    Exists(String),
    Io(io::Error),
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenameError::Empty => f.write_str(tr("A name cannot be empty.")),
            RenameError::Slash => f.write_str(tr("A name cannot contain “/”.")),
            RenameError::Nul => f.write_str(tr("A name cannot contain a null character.")),
            RenameError::Reserved => f.write_str(tr("“.” and “..” cannot be used as names.")),
            RenameError::TooLong => f.write_str(tr("The name is too long.")),
            RenameError::Exists(n) => {
                f.write_str(&fill(tr("“{}” already exists in this folder."), &[n]))
            }
            RenameError::Io(e) => f.write_str(&fill(tr("Cannot rename: {}."), &[e])),
        }
    }
}

/// Checks a new name for `old` and returns the new path. `Ok(None)` means
/// the name did not change: also when `old`'s name is not UTF-8 and
/// `new_name` is the name as shown, with U+FFFD for those bytes (renaming
/// to it would replace the bytes nobody edited).
pub fn rename_target(old: &Path, new_name: &str) -> Result<Option<PathBuf>, RenameError> {
    // First: a lossy name can be longer than the limit.
    if let Some(name) = old.file_name() {
        let name = name.as_bytes();
        if name == new_name.as_bytes()
            || (std::str::from_utf8(name).is_err() && String::from_utf8_lossy(name) == new_name)
        {
            return Ok(None);
        }
    }
    if new_name.is_empty() {
        return Err(RenameError::Empty);
    }
    if new_name.contains('/') {
        return Err(RenameError::Slash);
    }
    if new_name.contains('\0') {
        return Err(RenameError::Nul);
    }
    if new_name == "." || new_name == ".." {
        return Err(RenameError::Reserved);
    }
    if new_name.len() > NAME_MAX {
        return Err(RenameError::TooLong);
    }
    let target = old.with_file_name(new_name);
    if std::fs::symlink_metadata(&target).is_ok() && !case_only(old, &target) {
        return Err(RenameError::Exists(new_name.to_owned()));
    }
    Ok(Some(target))
}

/// Whether renaming `old` to `target` only changes the case of its name on a
/// case-insensitive file system: `target` finds the item itself, and the
/// folder has no entry of exactly that name. A hard link under another
/// name, also one that differs only in case, is the same item too, but a
/// clash.
fn case_only(old: &Path, target: &Path) -> bool {
    let (Some(a), Some(b)) = (old.file_name(), target.file_name()) else {
        return false;
    };
    a != b
        && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        && std::fs::symlink_metadata(old)
            .and_then(|m| Ok((m, std::fs::symlink_metadata(target)?)))
            .is_ok_and(|(m, t)| m.dev() == t.dev() && m.ino() == t.ino())
        && !has_entry(target)
}

/// Whether the folder of `path` lists an entry named exactly (byte for
/// byte) like it. An unreadable folder counts as having it.
fn has_entry(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return true;
    };
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|e| e.file_name() == name),
        Err(_) => true,
    }
}

/// Renames `old` to `new_name` in the same folder. Returns the new path, or
/// `None` if the name did not change.
///
/// The rename never replaces a target that appears after the check, where
/// the file system can refuse that atomically or a file can get a hard
/// link; elsewhere (a folder on NFS, SMB, some FUSE file systems) the check
/// and the rename are two steps. A change of
/// case on a case-insensitive file system is a plain rename, since the
/// target found there is the item itself.
pub fn rename(old: &Path, new_name: &str) -> Result<Option<PathBuf>, RenameError> {
    let Some(target) = rename_target(old, new_name)? else {
        return Ok(None);
    };
    let result = if case_only(old, &target) {
        std::fs::rename(old, &target)
    } else {
        crate::transfer::rename_noreplace(old, &target)
    };
    result.map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            RenameError::Exists(new_name.to_owned())
        } else if e.raw_os_error() == Some(36) {
            RenameError::TooLong
        } else {
            RenameError::Io(e)
        }
    })?;
    Ok(Some(target))
}

/// The `gio trash` commands for `paths`, in batches.
pub fn trash_commands(paths: &[PathBuf]) -> Vec<Command> {
    paths
        .chunks(TRASH_BATCH)
        .map(|batch| {
            let mut cmd = Command::new("gio");
            // After `--` every argument is a location, even one starting with `-`.
            cmd.arg("trash").arg("--").args(batch);
            cmd
        })
        .collect()
}

/// Moves `paths` to the trash. Returns an error message for the user.
pub fn trash(paths: &[PathBuf]) -> Result<(), String> {
    for mut cmd in trash_commands(paths) {
        let out = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| fill(tr("Cannot run gio: {}."), &[&e]))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err.lines().next().unwrap_or("").trim();
            return Err(if first.is_empty() {
                tr("Cannot move to the trash.").to_owned()
            } else {
                fill(tr("Cannot move to the trash: {}"), &[&first])
            });
        }
    }
    Ok(())
}

/// Deletes `paths` permanently: folders with all their contents, links as
/// links (never what they point to). Returns the paths that failed.
pub fn delete_permanently(paths: &[PathBuf]) -> Vec<(PathBuf, io::Error)> {
    let mut failed = Vec::new();
    for path in paths {
        let result = match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            failed.push((path.clone(), e));
        }
    }
    failed
}

/// "“name”" for one item, "N items" for more.
pub fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!(
            "“{}”",
            String::from_utf8_lossy(one.file_name().unwrap_or(one.as_os_str()).as_bytes())
        ),
        many => crate::labels::items(many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh empty folder under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("everygrep-gui-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn clipboard_text_is_one_item_per_line() {
        let paths = [PathBuf::from("/a/b c.txt"), PathBuf::from("/d/e")];
        assert_eq!(paths_text(&paths, false), "/a/b c.txt\n/d/e");
        assert_eq!(names_text(&paths), "b c.txt\ne");
        assert_eq!(paths_text(&paths[..1], false), "/a/b c.txt");
        assert_eq!(paths_text(&[], false), "");
        let odd = [PathBuf::from(OsStr::from_bytes(b"/x/\xffy"))];
        assert_eq!(names_text(&odd), "\u{fffd}y");
    }

    #[test]
    fn copy_path_gives_each_containing_folder_once() {
        let paths = [
            PathBuf::from("/a/b c/x.txt"),
            PathBuf::from("/d/e"),
            PathBuf::from("/a/b c/y"),
            PathBuf::from("/"),
        ];
        assert_eq!(folders_text(&paths, false), "/a/b c\n/d");
        assert_eq!(folders_text(&paths[1..2], false), "/d");
        assert_eq!(folders_text(&[PathBuf::from("/top")], false), "/");
        assert_eq!(folders_text(&[], false), "");
        assert_eq!(folders(&paths).len(), 2);
    }

    #[test]
    fn stem_excludes_the_extension() {
        assert_eq!(stem_len("report.pdf", false), 6);
        assert_eq!(stem_len("a.tar.gz", false), 5);
        assert_eq!(stem_len(".bashrc", false), 7);
        assert_eq!(stem_len("notes", false), 5);
        assert_eq!(stem_len("photos.2026", true), 11);
        assert_eq!(stem_len("şəkil.png", false), "şəkil".len());
    }

    #[test]
    fn opening_many_items_asks_above_the_threshold() {
        assert!(!open_needs_confirm(1, 32));
        assert!(!open_needs_confirm(32, 32));
        assert!(open_needs_confirm(33, 32));
        assert!(open_needs_confirm(2, 1));
        assert!(!open_needs_confirm(10_000, 0), "0 never asks");
    }

    #[test]
    fn quoting_wraps_every_item() {
        assert_eq!(quote("/a b/c.txt"), "\"/a b/c.txt\"");
        assert_eq!(quote("/a/it's"), "\"/a/it's\"");
        assert_eq!(quote("/a/$x"), "'/a/$x'");
        assert_eq!(quote("/a/it's $x"), "'/a/it'\\''s $x'");
        assert_eq!(quote("/a\\b"), "'/a'\\\\'b'");
        assert_eq!(quote("/a\nb"), "'/a\nb'");
        let paths = [
            PathBuf::from("/x/one.txt"),
            PathBuf::from("/y/two words.pdf"),
        ];
        assert_eq!(
            paths_text(&paths, true),
            "\"/x/one.txt\"\n\"/y/two words.pdf\""
        );
        assert_eq!(folders_text(&paths, true), "\"/x\"\n\"/y\"");
    }

    /// What `sh` (and bash, zsh, fish when installed) make of one quoted
    /// path given to `printf`: the words it gets, NUL-separated.
    fn shell_words(quoted: &str) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for shell in ["sh", "bash", "zsh", "fish"] {
            let script = format!("printf '%s\\0' {quoted}");
            let run = Command::new(shell)
                .arg("-c")
                .arg(&script)
                .env("HOME", "/nonexistent")
                .stdin(Stdio::null())
                .output();
            if let Ok(o) = run {
                out.push((shell.to_owned(), o.stdout));
            }
        }
        out
    }

    #[test]
    fn quoted_copies_are_taken_literally_by_shells() {
        let names = [
            "/x/$(echo PWNED)",
            "/x/`echo PWNED`",
            "/x/$HOME",
            "/x/a\"b",
            "/x/it's",
            "/x/back\\slash\\",
            "/x/a\\'b",
            "/x/!!",
            "/x/two\nlines",
            "/x/plain name.txt",
        ];
        for name in names {
            let text = paths_text(&[PathBuf::from(name)], true);
            let got = shell_words(&text);
            assert!(got.iter().any(|(s, _)| s == "sh"), "sh runs");
            for (shell, words) in got {
                assert_eq!(
                    String::from_utf8_lossy(&words),
                    format!("{name}\0"),
                    "{shell} given {text}"
                );
            }
        }
    }

    #[test]
    fn rename_selects_the_name_or_all_of_it() {
        assert_eq!(rename_selection("report.pdf", false, true), 6);
        assert_eq!(rename_selection("report.pdf", false, false), 10);
        assert_eq!(rename_selection("photos.2026", true, true), 11);
        assert_eq!(rename_selection(".bashrc", false, false), 7);
    }

    #[test]
    fn invalid_names_are_refused() {
        let old = Path::new("/nonexistent-dir/x.txt");
        assert!(matches!(rename_target(old, ""), Err(RenameError::Empty)));
        assert!(matches!(rename_target(old, "a/b"), Err(RenameError::Slash)));
        assert!(matches!(rename_target(old, "a\0b"), Err(RenameError::Nul)));
        assert!(matches!(
            rename_target(old, ".."),
            Err(RenameError::Reserved)
        ));
        let long = "x".repeat(256);
        assert!(matches!(
            rename_target(old, &long),
            Err(RenameError::TooLong)
        ));
        assert!(matches!(rename_target(old, "x.txt"), Ok(None)));
        assert_eq!(
            rename_target(old, "y.txt").unwrap(),
            Some(PathBuf::from("/nonexistent-dir/y.txt"))
        );
    }

    #[test]
    fn rename_refuses_to_replace_and_renames_otherwise() {
        let dir = TempDir::new("rename");
        let a = dir.0.join("a.txt");
        let b = dir.0.join("b.txt");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let err = rename(&a, "b.txt").unwrap_err();
        assert!(matches!(err, RenameError::Exists(_)));
        assert_eq!(err.to_string(), "“b.txt” already exists in this folder.");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b");

        let c = rename(&a, "c.txt").unwrap().unwrap();
        assert_eq!(c, dir.0.join("c.txt"));
        assert!(!a.exists());
        assert_eq!(fs::read_to_string(&c).unwrap(), "a");
        assert!(matches!(rename(&a, "d.txt"), Err(RenameError::Io(_))));
    }

    #[test]
    fn accepting_the_shown_name_of_a_name_that_is_not_utf8_changes_nothing() {
        let dir = TempDir::new("rename-lossy");
        let old = dir.0.join(OsStr::from_bytes(b"a\xff.txt"));
        fs::write(&old, "x").unwrap();
        // The editors show (and give back) the name with U+FFFD.
        let shown = String::from_utf8_lossy(old.file_name().unwrap().as_bytes()).into_owned();
        assert_eq!(shown, "a\u{fffd}.txt");
        assert!(matches!(rename_target(&old, &shown), Ok(None)));
        assert!(matches!(rename(&old, &shown), Ok(None)));
        assert_eq!(fs::read_to_string(&old).unwrap(), "x");
        assert!(!dir.0.join(&shown).exists());
        // Also when the shown name is longer than the limit in bytes.
        let long = dir.0.join(OsStr::from_bytes(&[0xff; 200]));
        fs::write(&long, "y").unwrap();
        let shown = "\u{fffd}".repeat(200);
        assert!(matches!(rename(&long, &shown), Ok(None)));
        assert_eq!(fs::read_to_string(&long).unwrap(), "y");
        // A real new name still renames it.
        let new = rename(&old, "b.txt").unwrap().unwrap();
        assert_eq!(fs::read_to_string(new).unwrap(), "x");
    }

    #[test]
    fn another_link_to_the_file_clashes_even_under_another_case() {
        let dir = TempDir::new("rename-case");
        let (x, upper) = (dir.0.join("x.txt"), dir.0.join("X.txt"));
        fs::write(&x, "x").unwrap();
        // A hard link under the other case: on a case-sensitive file system
        // "X.txt" is an entry of its own, not "x.txt" found again, so it is
        // taken (a rename onto it would change nothing and say "renamed").
        fs::hard_link(&x, &upper).unwrap();
        assert!(matches!(
            rename_target(&x, "X.txt"),
            Err(RenameError::Exists(_))
        ));
        assert!(matches!(rename(&x, "X.txt"), Err(RenameError::Exists(_))));
        assert!(x.exists() && upper.exists());
        assert!(has_entry(&upper) && !has_entry(&dir.0.join("x.TXT")));
        // The same file under an unrelated name is a different entry.
        let other = dir.0.join("other.txt");
        fs::hard_link(&upper, &other).unwrap();
        assert!(matches!(
            rename_target(&upper, "other.txt"),
            Err(RenameError::Exists(_))
        ));
        assert!(matches!(
            rename(&upper, "other.txt"),
            Err(RenameError::Exists(_))
        ));
        assert!(upper.exists() && other.exists());
    }

    #[test]
    fn rename_counts_utf8_bytes_and_preserves_a_dangling_target() {
        let dir = TempDir::new("rename-unicode");
        let old = dir.0.join("old");
        fs::write(&old, "keep").unwrap();
        assert!(matches!(
            rename(&old, &"ə".repeat(128)),
            Err(RenameError::TooLong)
        ));
        assert_eq!(fs::read_to_string(&old).unwrap(), "keep");
        let link = dir.0.join("taken");
        std::os::unix::fs::symlink(dir.0.join("missing"), &link).unwrap();
        assert!(matches!(rename(&old, "taken"), Err(RenameError::Exists(_))));
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        let renamed = rename(&old, &"ə".repeat(127)).unwrap().unwrap();
        assert_eq!(fs::read_to_string(renamed).unwrap(), "keep");
    }

    #[test]
    fn trash_runs_gio_in_batches_with_an_option_terminator() {
        let paths: Vec<PathBuf> = (0..TRASH_BATCH + 3)
            .map(|i| PathBuf::from(format!("/t/-{i}")))
            .collect();
        let cmds = trash_commands(&paths);
        assert_eq!(cmds.len(), 2);
        let args: Vec<&OsStr> = cmds[1].get_args().collect();
        assert_eq!(cmds[1].get_program(), "gio");
        assert_eq!(&args[..2], [OsStr::new("trash"), OsStr::new("--")]);
        assert_eq!(args.len(), 2 + 3);
        assert_eq!(cmds[0].get_args().count(), 2 + TRASH_BATCH);
    }

    #[test]
    fn delete_removes_files_and_folders_but_not_link_targets() {
        let dir = TempDir::new("delete");
        let keep = dir.0.join("keep");
        fs::create_dir(&keep).unwrap();
        fs::write(keep.join("inside"), "x").unwrap();
        let file = dir.0.join("f.txt");
        fs::write(&file, "x").unwrap();
        let tree = dir.0.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/g"), "x").unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&keep, &link).unwrap();

        let missing = dir.0.join("missing");
        let failed =
            delete_permanently(&[file.clone(), tree.clone(), link.clone(), missing.clone()]);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, missing);
        assert!(!file.exists() && !tree.exists());
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(keep.join("inside").is_file());
    }

    #[test]
    fn descriptions_name_one_item_or_count_many() {
        assert_eq!(describe(&[PathBuf::from("/a/b.txt")]), "“b.txt”");
        assert_eq!(
            describe(&[PathBuf::from("/a"), PathBuf::from("/b")]),
            "2 items"
        );
    }
}
