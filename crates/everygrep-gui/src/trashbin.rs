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

/// A backend receives the isolated entry, its public original path and the
/// exact post-isolation identity; no path lookup may authorize another inode.
pub(crate) type Backend<'a> =
    dyn Fn(&Path, &Path, crate::transfer::FileStamp) -> Result<(), String> + 'a;

fn private_dir(path: &Path) -> io::Result<std::sync::Arc<std::fs::File>> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::symlink_metadata(path) {
        Ok(m) if !m.is_dir() => return Err(io::ErrorKind::NotADirectory.into()),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        Err(e) => return Err(e),
    }
    let dir = std::sync::Arc::new(crate::transfer::open_dir(path)?);
    crate::transfer::require_private(&dir)?;
    Ok(dir)
}
fn deletion_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let zone = everygrep_core::localtime::Zone::local();
    let (y, m, d) = zone.local_date(secs);
    let day = zone.to_local(secs).rem_euclid(86400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}
fn remove_own_info(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    expected: &std::fs::File,
) -> io::Result<()> {
    let current = fs::symlink_metadata(crate::transfer::fd_path(parent).join(name))?;
    let own = expected.metadata()?;
    if (current.dev(), current.ino()) != (own.dev(), own.ino()) {
        return Err(io::Error::other(tr(
            "The item changed since the recorded operation.",
        )));
    }
    crate::transfer::unlink_at(parent, name, false)
}

/// Freedesktop Trash publication through held directory descriptors. Metadata
/// is reserved without replacement before moving the already-isolated inode.
pub(crate) fn put_from(
    actual: &Path,
    original: &Path,
    expected: crate::transfer::FileStamp,
) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let perform = || -> io::Result<()> {
        let parent =
            crate::transfer::open_dir(actual.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let name = actual.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        let source = crate::transfer::pin_path(&crate::transfer::fd_path(&parent).join(name))?;
        if crate::transfer::FileStamp::of(&source.metadata()?) != expected {
            return Err(io::Error::other(tr(
                "The item changed since the recorded operation.",
            )));
        }
        let base = original.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        let mut last = io::Error::new(io::ErrorKind::Unsupported, tr("Cannot move to the trash."));
        for (root, top) in candidates(original) {
            if top.is_some()
                && root.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".Trash"))
            {
                let Some(shared) = root.parent() else {
                    continue;
                };
                if !fs::symlink_metadata(shared).is_ok_and(|m| m.is_dir() && m.mode() & 0o1000 != 0)
                {
                    continue;
                }
            }
            if source.metadata()?.is_dir() && root.starts_with(original) {
                return Err(io::Error::other(tr(
                    "A folder cannot be trashed into itself.",
                )));
            }
            let dirs = (|| -> io::Result<_> {
                let root = private_dir(&root)?;
                if root.metadata()?.dev() != expected.dev {
                    return Err(io::ErrorKind::CrossesDevices.into());
                }
                let files = private_dir(&crate::transfer::fd_path(&root).join("files"))?;
                let info = private_dir(&crate::transfer::fd_path(&root).join("info"))?;
                Ok((root, files, info))
            })();
            let (root, files, info) = match dirs {
                Ok(d) => d,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            for n in 0..1000 {
                let suffix = if n == 0 {
                    String::new()
                } else {
                    format!(".{n}")
                };
                let mut bytes = base.as_bytes().to_vec();
                bytes.truncate(245usize.saturating_sub(suffix.len()));
                bytes.extend(suffix.as_bytes());
                let trash_name = OsString::from_vec(bytes);
                let mut info_name = trash_name.clone();
                info_name.push(".trashinfo");
                let info_path = crate::transfer::fd_path(&info).join(&info_name);
                let mut record = match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&info_path)
                {
                    Ok(f) => f,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => {
                        last = e;
                        break;
                    }
                };
                let relative = match &top {
                    Some(top) => original
                        .strip_prefix(top)
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
                    None => original,
                };
                let prepared = (|| -> io::Result<()> {
                    crate::transfer::require_private(&record)?;
                    record.write_all(
                        format!(
                            "[Trash Info]\nPath={}\nDeletionDate={}\n",
                            encode(relative),
                            deletion_date()
                        )
                        .as_bytes(),
                    )?;
                    record.sync_all()?;
                    info.sync_all()?;
                    root.sync_all()?;
                    if crate::transfer::FileStamp::read(
                        &crate::transfer::fd_path(&parent).join(name),
                    )? != expected
                    {
                        return Err(io::Error::other(tr(
                            "The item changed since the recorded operation.",
                        )));
                    }
                    Ok(())
                })();
                if let Err(e) = prepared {
                    let _ = remove_own_info(&info, &info_name, &record);
                    return Err(e);
                }
                #[cfg(test)]
                crate::fileops_test::at("trash-publish-checked", original);
                match crate::transfer::rename_at(&parent, name, &files, &trash_name) {
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        remove_own_info(&info, &info_name, &record)?;
                        continue;
                    }
                    Err(e) => {
                        let _ = remove_own_info(&info, &info_name, &record);
                        return Err(e);
                    }
                    Ok(()) => {}
                }
                let published = (|| -> io::Result<()> {
                    let target = crate::transfer::fd_path(&files).join(&trash_name);
                    if !expected.same_except_ctime(&crate::transfer::FileStamp::read(&target)?) {
                        return Err(io::Error::other(tr(
                            "The item changed since the recorded operation.",
                        )));
                    }
                    files.sync_all()?;
                    parent.sync_all()?;
                    info.sync_all()?;
                    root.sync_all()?;
                    Ok(())
                })();
                if let Err(e) = published {
                    // Keep even an unexpected inode; roll it back without
                    // replacing anything. A collision leaves it in the Trash.
                    if let Err(restore) =
                        crate::transfer::rename_at(&files, &trash_name, &parent, name)
                    {
                        return Err(io::Error::other(crate::i18n::fill(
                            tr("Could not restore the source; it remains at {0}: {1}"),
                            &[
                                &fs::read_link(format!(
                                    "/proc/self/fd/{}",
                                    std::os::fd::AsRawFd::as_raw_fd(&*files)
                                ))
                                .unwrap_or_else(|_| crate::transfer::fd_path(&files))
                                .join(&trash_name)
                                .display(),
                                &restore,
                            ],
                        )));
                    }
                    let _ = remove_own_info(&info, &info_name, &record);
                    return Err(e);
                }
                return Ok(());
            }
        }
        Err(last)
    };
    perform().map_err(|e| crate::i18n::fill(tr("Cannot move to the trash: {}"), &[&e]))
}

/// Puts the trashed item `id` back at `original`. Refuses when something
/// has that name now, when its folder is gone, or when the item is not in
/// the trash any more (emptied, restored or changed).
#[cfg(test)]
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
    let expected = crate::transfer::FileStamp::read(&found.file).map_err(|e| e.to_string())?;
    if (expected.dev, expected.ino, expected.len, expected.mtime)
        != (id.dev, id.ino, id.len, id.mtime)
    {
        return Err(tr("The item changed since the recorded operation.").into());
    }
    let info = crate::transfer::pin_path(&found.info).ok();
    crate::transfer::rename_expected(&found.file, original, expected).map_err(|e| e.to_string())?;
    if let Some(info) = info {
        let _ = remove_description(&found.info, &info);
    }
    Ok(())
}

pub(crate) fn remove_description(path: &Path, expected: &fs::File) -> io::Result<()> {
    let parent = crate::transfer::open_dir(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    remove_own_info(
        &parent,
        path.file_name().ok_or(io::ErrorKind::InvalidInput)?,
        expected,
    )
}

/// The `Path=` value for `path`, percent-encoded as the specification asks.
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
    let stamp = crate::transfer::FileStamp::read(path).map_err(|e| e.to_string())?;
    put_from(path, path, stamp)
}
#[cfg(test)]
pub(crate) fn test_send(
    actual: &Path,
    original: &Path,
    stamp: crate::transfer::FileStamp,
) -> Result<(), String> {
    put_from(actual, original, stamp)
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
    fn restore_rejects_a_replacement_in_the_last_rename_lookup_gap() {
        let t = Bin::new();
        let file = t.0.join("work/file");
        let saved = t.0.join("saved");
        fs::write(&file, "selected bytes").unwrap();
        let id = Identity::of(&fs::symlink_metadata(&file).unwrap());
        test_trash(&file).unwrap();
        let found = find(&file, id).unwrap();
        let target = found.file.clone();
        crate::fileops_test::arm_path("rename-checked", &target, move |path| {
            fs::rename(path, saved).unwrap();
            fs::write(path, "foreign unique bytes").unwrap();
        });
        assert!(restore(&file, id).is_err());
        crate::fileops_test::fired();
        assert!(!file.exists());
        assert_eq!(fs::read(&target).unwrap(), b"foreign unique bytes");
    }

    #[test]
    fn private_trash_directories_never_follow_a_files_symlink() {
        let t = Bin::new();
        let file = t.0.join("work/file");
        let outside = t.0.join("outside");
        fs::write(&file, "selected bytes").unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir_all(t.0.join("Trash")).unwrap();
        fs::set_permissions(
            t.0.join("Trash"),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, t.0.join("Trash/files")).unwrap();
        assert!(test_trash(&file).is_err());
        assert_eq!(fs::read(file).unwrap(), b"selected bytes");
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn a_trash_inside_the_selected_directory_keeps_the_source() {
        let bin = Bin::new();
        let folder = bin.0.join("work/folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("unique"), "selected bytes").unwrap();
        set_test_home(Some(folder.join("Trash")));
        assert!(test_trash(&folder).unwrap_err().contains("itself"));
        assert_eq!(fs::read(folder.join("unique")).unwrap(), b"selected bytes");
        assert!(!folder.join("Trash").exists());
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
