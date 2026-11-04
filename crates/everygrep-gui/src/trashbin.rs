//! Finding an item this program sent to the desktop trash and putting it
//! back, for undo.
//!
//! The trash follows the freedesktop.org Trash specification: a trashed
//! item is renamed into `files/` of a trash folder and described by
//! `info/<name>.trashinfo` (its original `Path=`). The home trash
//! (`$XDG_DATA_HOME/Trash`) holds items of the file system it is on; items
//! of other file systems go to `.Trash/<uid>` or `.Trash-<uid>` at the top
//! folder of their file system. Trashing is a rename, so the item keeps its
//! device and inode: an item is found again only when its identity (device,
//! inode, size, modification time) and its recorded original path both
//! match. Restoring never replaces anything and never recreates a missing
//! folder.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::i18n::tr;

/// What identifies a trashed file: it survives the rename into the trash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime: (i64, i64),
}

impl Identity {
    pub fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    }
}

/// A trashed item: its entry under `files/` and its description.
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    pub file: PathBuf,
    pub info: PathBuf,
}

#[cfg(test)]
thread_local! {
    /// The home trash folder in tests, so they never look at the real one.
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn set_test_home(dir: Option<PathBuf>) {
    TEST_HOME.with(|h| *h.borrow_mut() = dir);
}

fn home_trash() -> Option<PathBuf> {
    #[cfg(test)]
    {
        TEST_HOME.with(|h| h.borrow().clone())
    }
    #[cfg(not(test))]
    {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Some(data.join("Trash"))
    }
}

/// The top folder of the file system holding `dir`: the last ancestor on
/// the same device.
fn top_dir(dir: &Path) -> Option<PathBuf> {
    let dev = fs::metadata(dir).ok()?.dev();
    let mut top = dir.to_owned();
    while let Some(parent) = top.parent() {
        match fs::metadata(parent) {
            Ok(m) if m.dev() == dev => top = parent.to_owned(),
            _ => break,
        }
    }
    Some(top)
}

/// The trash folders an item that was at `original` can be in, each with
/// the folder its relative `Path=` values start from.
fn candidates(original: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut out = Vec::new();
    if let Some(home) = home_trash() {
        out.push((home, None));
    }
    // Tests never look into a real trash folder at the top of /tmp.
    if cfg!(test) {
        return out;
    }
    let Some(parent) = original.parent() else {
        return out;
    };
    if let Some(top) = top_dir(parent) {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { getuid() };
        out.push((top.join(".Trash").join(uid.to_string()), Some(top.clone())));
        out.push((top.join(format!(".Trash-{uid}")), Some(top)));
    }
    out
}

unsafe extern "C" {
    fn getuid() -> u32;
}

/// Finds the trashed item that was at `original` and is `id`.
pub fn find(original: &Path, id: Identity) -> Option<Found> {
    for (trash, top) in candidates(original) {
        let Ok(entries) = fs::read_dir(trash.join("files")) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.path();
            let Ok(meta) = fs::symlink_metadata(&file) else {
                continue;
            };
            if Identity::of(&meta) != id {
                continue;
            }
            let mut info_name = entry.file_name();
            info_name.push(".trashinfo");
            let info = trash.join("info").join(info_name);
            if recorded_path(&info, top.as_deref()).as_deref() == Some(original) {
                return Some(Found { file, info });
            }
        }
    }
    None
}

/// The original path written in a `.trashinfo` file.
fn recorded_path(info: &Path, top: Option<&Path>) -> Option<PathBuf> {
    let text = fs::read(info).ok()?;
    let mut in_group = false;
    for line in text.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.starts_with(b"[") {
            in_group = line == b"[Trash Info]";
            continue;
        }
        if in_group && let Some(value) = line.strip_prefix(b"Path=") {
            let path = PathBuf::from(OsString::from_vec(percent_decode(value)?));
            return if path.is_absolute() {
                Some(path)
            } else {
                top.map(|t| t.join(path))
            };
        }
    }
    None
}

fn percent_decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Puts the trashed item `id` back at `original`. Refuses when something
/// has that name now, when its folder is gone, or when the item is not in
/// the trash any more (emptied, restored or changed).
pub fn restore(original: &Path, id: Identity) -> Result<(), String> {
    let parent = original
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput).to_string())?;
    if !fs::symlink_metadata(parent).is_ok_and(|m| m.is_dir()) {
        return Err(tr("The original folder no longer exists.").into());
    }
    match fs::symlink_metadata(original) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err(tr("The destination already exists.").into()),
        Err(e) => return Err(e.to_string()),
    }
    let found =
        find(original, id).ok_or_else(|| tr("The item is no longer in the trash.").to_owned())?;
    crate::transfer::rename_strict(&found.file, original).map_err(|e| e.to_string())?;
    // The item is back; a description left behind only shows a stale entry.
    let _ = fs::remove_file(&found.info);
    Ok(())
}

/// The `Path=` value for `path`, percent-encoded as the specification asks.
#[cfg(test)]
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Trashes `path` into the test home trash like a desktop does.
#[cfg(test)]
pub fn test_trash(path: &Path) -> Result<(), String> {
    let home = home_trash().ok_or("no test trash")?;
    fs::create_dir_all(home.join("files")).map_err(|e| e.to_string())?;
    fs::create_dir_all(home.join("info")).map_err(|e| e.to_string())?;
    let name = path.file_name().ok_or("no name")?;
    let mut n = 1;
    let mut target = OsString::from(name);
    while home.join("files").join(&target).exists() {
        n += 1;
        target = OsString::from(format!("{}.{n}", name.to_string_lossy()));
    }
    let mut info_name = target.clone();
    info_name.push(".trashinfo");
    fs::write(
        home.join("info").join(info_name),
        format!(
            "[Trash Info]\nPath={}\nDeletionDate=2026-11-04T20:00:00\n",
            encode(path)
        ),
    )
    .map_err(|e| e.to_string())?;
    crate::transfer::rename_strict(path, &home.join("files").join(target))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct Bin(pub PathBuf);
    impl Bin {
        pub fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "everygrep-trashbin-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(dir.join("work")).unwrap();
            set_test_home(Some(dir.join("Trash")));
            Self(dir)
        }
    }
    impl Drop for Bin {
        fn drop(&mut self) {
            set_test_home(None);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn id(path: &Path) -> Identity {
        Identity::of(&fs::symlink_metadata(path).unwrap())
    }

    #[test]
    fn restores_the_matching_item_and_removes_its_description() {
        let bin = Bin::new();
        let file = bin.0.join("work/a b%.txt");
        fs::write(&file, "data").unwrap();
        let before = id(&file);
        test_trash(&file).unwrap();
        assert!(!file.exists());
        restore(&file, before).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"data");
        assert_eq!(fs::read_dir(bin.0.join("Trash/info")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(bin.0.join("Trash/files")).unwrap().count(), 0);
    }

    #[test]
    fn same_name_trashed_twice_restores_only_the_recorded_one() {
        let bin = Bin::new();
        let file = bin.0.join("work/x");
        fs::write(&file, "first").unwrap();
        let first = id(&file);
        test_trash(&file).unwrap();
        fs::write(&file, "second, longer").unwrap();
        let second = id(&file);
        test_trash(&file).unwrap();
        restore(&file, first).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"first");
        // The other one stays in the trash; its slot is taken now.
        assert!(restore(&file, second).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"first");
        fs::remove_file(&file).unwrap();
        restore(&file, second).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"second, longer");
    }

    #[test]
    fn refuses_occupied_names_missing_folders_and_items_no_longer_trashed() {
        let bin = Bin::new();
        let dir = bin.0.join("work/sub");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("f");
        fs::write(&file, "data").unwrap();
        let before = id(&file);
        test_trash(&file).unwrap();
        fs::write(&file, "new occupant").unwrap();
        assert!(
            restore(&file, before)
                .unwrap_err()
                .contains("already exists")
        );
        assert_eq!(fs::read(&file).unwrap(), b"new occupant");
        fs::remove_file(&file).unwrap();
        fs::remove_dir(&dir).unwrap();
        assert!(restore(&file, before).unwrap_err().contains("folder"));
        fs::create_dir(&dir).unwrap();
        // Emptied trash: nothing to restore.
        fs::remove_dir_all(bin.0.join("Trash/files")).unwrap();
        fs::create_dir(bin.0.join("Trash/files")).unwrap();
        assert!(restore(&file, before).unwrap_err().contains("no longer"));
        assert!(!file.exists());
    }

    #[test]
    fn an_item_whose_description_names_another_path_is_not_taken() {
        let bin = Bin::new();
        let file = bin.0.join("work/f");
        fs::write(&file, "data").unwrap();
        let before = id(&file);
        test_trash(&file).unwrap();
        let other = bin.0.join("work/g");
        assert!(find(&other, before).is_none());
        assert!(find(&file, before).is_some());
        // A changed item (same inode, other size) is not taken either.
        let trashed = bin.0.join("Trash/files/f");
        fs::write(&trashed, "changed in the trash").unwrap();
        assert!(restore(&file, before).is_err());
        assert!(!file.exists());
    }

    #[test]
    fn percent_decoding_reads_the_specification_form() {
        assert_eq!(
            percent_decode(b"/a%20b/%C9%99").unwrap(),
            "/a b/ə".as_bytes()
        );
        assert!(percent_decode(b"/bad%2").is_none());
        assert!(percent_decode(b"/bad%zz").is_none());
    }
}
