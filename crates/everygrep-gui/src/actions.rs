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

/// Paths passed to one `gio trash` call, to stay well below the argument
/// size limit.
const TRASH_BATCH: usize = 256;

/// Longest file name Linux file systems accept, in bytes.
const NAME_MAX: usize = 255;

/// Full paths, one per line, for the clipboard.
pub fn paths_text(paths: &[PathBuf]) -> String {
    join_lines(paths.iter().map(|p| p.as_os_str()))
}

/// File names, one per line, for the clipboard.
pub fn names_text(paths: &[PathBuf]) -> String {
    join_lines(paths.iter().map(|p| p.file_name().unwrap_or(p.as_os_str())))
}

/// Joins with `\n`. Bytes that are not UTF-8 become U+FFFD; the clipboard
/// takes text only.
fn join_lines<'a>(items: impl Iterator<Item = &'a OsStr>) -> String {
    let mut out = String::new();
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&String::from_utf8_lossy(item.as_bytes()));
    }
    out
}

/// Length in bytes of the part of a name before its extension, so a rename
/// field can preselect it. Folders and names without an extension are
/// selected whole.
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
    Reserved,
    TooLong,
    Exists(String),
    Io(io::Error),
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenameError::Empty => write!(f, "A name cannot be empty."),
            RenameError::Slash => write!(f, "A name cannot contain “/”."),
            RenameError::Reserved => write!(f, "“.” and “..” cannot be used as names."),
            RenameError::TooLong => write!(f, "The name is too long."),
            RenameError::Exists(n) => write!(f, "“{n}” already exists in this folder."),
            RenameError::Io(e) => write!(f, "Cannot rename: {e}."),
        }
    }
}

/// Checks a new name for `old` and returns the new path. `Ok(None)` means
/// the name did not change.
pub fn rename_target(old: &Path, new_name: &str) -> Result<Option<PathBuf>, RenameError> {
    if new_name.is_empty() {
        return Err(RenameError::Empty);
    }
    if new_name.contains('/') || new_name.contains('\0') {
        return Err(RenameError::Slash);
    }
    if new_name == "." || new_name == ".." {
        return Err(RenameError::Reserved);
    }
    if new_name.len() > NAME_MAX {
        return Err(RenameError::TooLong);
    }
    if old.file_name() == Some(OsStr::new(new_name)) {
        return Ok(None);
    }
    let target = old.with_file_name(new_name);
    if let Ok(existing) = std::fs::symlink_metadata(&target) {
        // On case-insensitive file systems "a.txt" -> "A.txt" finds the file
        // itself; that is a rename, not a clash.
        let same = std::fs::symlink_metadata(old)
            .is_ok_and(|m| m.dev() == existing.dev() && m.ino() == existing.ino());
        if !same {
            return Err(RenameError::Exists(new_name.to_owned()));
        }
    }
    Ok(Some(target))
}

/// Renames `old` to `new_name` in the same folder. Returns the new path, or
/// `None` if the name did not change.
///
/// The check for an existing target and the rename are two steps, so a file
/// created in between would be replaced.
pub fn rename(old: &Path, new_name: &str) -> Result<Option<PathBuf>, RenameError> {
    let Some(target) = rename_target(old, new_name)? else {
        return Ok(None);
    };
    std::fs::rename(old, &target).map_err(RenameError::Io)?;
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
            .map_err(|e| format!("Cannot run gio: {e}."))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err.lines().next().unwrap_or("").trim();
            return Err(if first.is_empty() {
                "Cannot move to the trash.".to_owned()
            } else {
                format!("Cannot move to the trash: {first}")
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
        many => format!("{} items", crate::format::count(many.len())),
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
        assert_eq!(paths_text(&paths), "/a/b c.txt\n/d/e");
        assert_eq!(names_text(&paths), "b c.txt\ne");
        assert_eq!(paths_text(&paths[..1]), "/a/b c.txt");
        assert_eq!(paths_text(&[]), "");
        let odd = [PathBuf::from(OsStr::from_bytes(b"/x/\xffy"))];
        assert_eq!(names_text(&odd), "\u{fffd}y");
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
    fn invalid_names_are_refused() {
        let old = Path::new("/nonexistent-dir/x.txt");
        assert!(matches!(rename_target(old, ""), Err(RenameError::Empty)));
        assert!(matches!(rename_target(old, "a/b"), Err(RenameError::Slash)));
        assert!(matches!(
            rename_target(old, "a\0b"),
            Err(RenameError::Slash)
        ));
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
