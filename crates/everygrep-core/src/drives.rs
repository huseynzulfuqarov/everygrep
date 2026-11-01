//! Removable drives: finding them in the mount table, naming them by file
//! system UUID, noticing when the mount table changes, and the small files
//! that remember them.
//!
//! A drive counts as removable only when the desktop's disk service
//! (udisks) mounted it: its mount point lies **directly** in one of the
//! media folders, `/run/media/$USER` or `/media/$USER`. Any other mount in
//! the mount table, such as `/`, `/home` or file systems mounted from
//! `/etc/fstab` under `/mnt`, is never a drive here, so it is never indexed
//! because it appeared.
//!
//! A drive is identified by the UUID of its file system, found in
//! `/dev/disk/by-uuid`: the link whose device is the mount's source (the
//! same path, or the same device numbers). So the same disk is recognised
//! at another mount point. Its label comes from `/dev/disk/by-label`.
//!
//! Every path used here can be replaced for tests and demonstrations (see
//! [`DriveEnv::from_env`]), so drives can be simulated with plain folders.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::scan::VIRTUAL_FS_TYPES;

/// Where the drive code looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveEnv {
    /// The mount table (`/proc/self/mountinfo`).
    pub mountinfo: PathBuf,
    /// Folders whose direct subfolders are drive mount points.
    pub media_dirs: Vec<PathBuf>,
    /// Links from file system UUIDs to devices (`/dev/disk/by-uuid`).
    pub by_uuid: PathBuf,
    /// Links from labels to devices (`/dev/disk/by-label`).
    pub by_label: PathBuf,
    /// Where the drives' indexes and notes are kept
    /// (`$XDG_CACHE_HOME/everygrep/volumes`).
    pub volumes_dir: PathBuf,
}

impl DriveEnv {
    /// The real places, each replaceable by an environment variable:
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `EVERYGREP_MOUNTINFO` | `/proc/self/mountinfo` |
    /// | `EVERYGREP_MEDIA_DIRS` (`:`-separated) | `/run/media/$USER:/media/$USER` |
    /// | `EVERYGREP_DISK_BY_UUID` | `/dev/disk/by-uuid` |
    /// | `EVERYGREP_DISK_BY_LABEL` | `/dev/disk/by-label` |
    /// | `EVERYGREP_VOLUMES` | `$XDG_CACHE_HOME/everygrep/volumes` |
    ///
    /// They exist for tests and screenshots with simulated drives. `None`
    /// when there is no cache folder or no user name.
    pub fn from_env() -> Option<Self> {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let user = var("USER").or_else(|| var("LOGNAME")).or_else(|| {
            var("HOME").and_then(|h| PathBuf::from(h).file_name().map(|n| n.to_owned()))
        });
        let media_dirs = match var("EVERYGREP_MEDIA_DIRS") {
            Some(list) => std::env::split_paths(&list)
                .filter(|p| p.is_absolute())
                .collect(),
            None => {
                let user = user?;
                vec![
                    Path::new("/run/media").join(&user),
                    Path::new("/media").join(&user),
                ]
            }
        };
        let volumes_dir = match var("EVERYGREP_VOLUMES") {
            Some(dir) => PathBuf::from(dir),
            None => crate::persist::default_path()?.with_file_name("volumes"),
        };
        Some(Self {
            mountinfo: var("EVERYGREP_MOUNTINFO")
                .map_or_else(|| PathBuf::from("/proc/self/mountinfo"), PathBuf::from),
            media_dirs,
            by_uuid: var("EVERYGREP_DISK_BY_UUID")
                .map_or_else(|| PathBuf::from("/dev/disk/by-uuid"), PathBuf::from),
            by_label: var("EVERYGREP_DISK_BY_LABEL")
                .map_or_else(|| PathBuf::from("/dev/disk/by-label"), PathBuf::from),
            volumes_dir,
        })
    }

    /// The saved index of a drive.
    pub fn index_path(&self, uuid: &str) -> PathBuf {
        self.volumes_dir.join(format!("{uuid}.bin"))
    }

    /// The notes about a drive (label, last mount point, last seen).
    pub fn info_path(&self, uuid: &str) -> PathBuf {
        self.volumes_dir.join(format!("{uuid}.info"))
    }
}

/// One line of the mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// Device numbers of the mounted file system (major, minor).
    pub dev: (u32, u32),
    pub mount_point: PathBuf,
    pub fs_type: String,
    /// What was mounted, usually a device path such as `/dev/sdb1`.
    pub source: PathBuf,
}

/// Reads the lines of a `mountinfo` file (see proc(5)). Lines that do not
/// have the expected fields are skipped.
pub fn parse_mountinfo(text: &[u8]) -> Vec<Mount> {
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        // id parent major:minor root mount-point options [optional...] - type source super
        let Some(dash) = fields.iter().position(|f| *f == b"-") else {
            continue;
        };
        if dash < 6 || fields.len() < dash + 3 {
            continue;
        }
        let Some(dev) = std::str::from_utf8(fields[2])
            .ok()
            .and_then(|d| d.split_once(':'))
            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        else {
            continue;
        };
        out.push(Mount {
            dev,
            mount_point: PathBuf::from(OsString::from_vec(unescape_octal(fields[4]))),
            fs_type: String::from_utf8_lossy(fields[dash + 1]).into_owned(),
            source: PathBuf::from(OsString::from_vec(unescape_octal(fields[dash + 2]))),
        });
    }
    out
}

/// Undoes the `\ooo` escapes the kernel writes for space, tab, newline and
/// backslash in mount table fields.
fn unescape_octal(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && i + 4 <= field.len()
            && field[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let v = field[i + 1..i + 4]
                .iter()
                .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            if let Ok(byte) = u8::try_from(v) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(field[i]);
        i += 1;
    }
    out
}

/// A removable drive that is mounted now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// File system UUID (only letters, digits and `-`).
    pub uuid: String,
    /// File system label; empty when it has none.
    pub label: String,
    pub mount_point: PathBuf,
}

/// The removable drives in the mount table: mounts directly inside one of
/// the media folders, of a file system that holds files, whose device has a
/// UUID. A drive mounted twice is listed once (its first mount).
pub fn removable_drives(env: &DriveEnv) -> io::Result<Vec<Drive>> {
    let table = fs::read(&env.mountinfo)?;
    let uuids = links(&env.by_uuid);
    let labels = links(&env.by_label);
    let mut out: Vec<Drive> = Vec::new();
    for mount in parse_mountinfo(&table) {
        if !is_media_mount(&mount, &env.media_dirs) {
            continue;
        }
        let Some(uuid) = device_name(&uuids, &mount).filter(|u| is_safe_uuid(u)) else {
            continue;
        };
        if out.iter().any(|d| d.uuid == uuid) {
            continue;
        }
        let label = device_name(&labels, &mount).unwrap_or_default();
        out.push(Drive {
            uuid,
            label,
            mount_point: mount.mount_point,
        });
    }
    Ok(out)
}

/// Whether a mount is a drive mounted by the desktop: its mount point is a
/// direct subfolder of a media folder, and its file system is not a virtual
/// or in-memory one.
pub fn is_media_mount(mount: &Mount, media_dirs: &[PathBuf]) -> bool {
    let point = &mount.mount_point;
    let plain = point.is_absolute()
        && point
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    plain
        && point
            .parent()
            .is_some_and(|parent| media_dirs.iter().any(|m| m == parent))
        && !VIRTUAL_FS_TYPES.contains(&mount.fs_type.as_str())
        && !MEMORY_FS_TYPES.contains(&mount.fs_type.as_str())
}

/// File systems in memory, never a drive.
const MEMORY_FS_TYPES: &[&str] = &["tmpfs", "ramfs"];

/// UUIDs and labels become file names; only these characters are accepted.
pub fn is_safe_uuid(uuid: &str) -> bool {
    !uuid.is_empty()
        && uuid.len() <= 64
        && uuid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A link folder such as `/dev/disk/by-uuid`: (name, link target as
/// written, device numbers of the target if it is a device).
type Links = Vec<(String, PathBuf, Option<(u32, u32)>)>;

fn links(dir: &Path) -> Links {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let target = fs::read_link(&path).ok()?;
            let target = lexical_join(dir, &target);
            // Follows the link; only a block device has numbers.
            let dev = fs::metadata(&path)
                .ok()
                .filter(|m| m.rdev() != 0)
                .map(|m| dev_numbers(m.rdev()));
            let name = unescape_udev(&e.file_name().to_string_lossy());
            Some((name, target, dev))
        })
        .collect()
}

/// The name of the link whose device is the mount's source: the same path,
/// or (for a real device) the same device numbers.
fn device_name(links: &Links, mount: &Mount) -> Option<String> {
    let source = lexical_join(Path::new("/"), &mount.source);
    let source_dev = fs::metadata(&mount.source)
        .ok()
        .filter(|m| m.rdev() != 0)
        .map(|m| dev_numbers(m.rdev()));
    links
        .iter()
        .find(|(_, target, dev)| {
            *target == source || dev.is_some_and(|d| d == mount.dev || Some(d) == source_dev)
        })
        .map(|(name, _, _)| name.clone())
}

/// `base` joined with `rel`, with `.` and `..` resolved by text (the
/// targets of device links are relative, such as `../../sdb1`).
fn lexical_join(base: &Path, rel: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in base.join(rel).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Major and minor numbers of a `dev_t` as glibc encodes it.
fn dev_numbers(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// udev writes some characters of labels as `\xHH` (a space is `\x20`).
fn unescape_udev(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'x') {
            if let Some(v) = bytes
                .get(i + 2..i + 4)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Tells when the mount table changes.
///
/// The kernel marks `/proc/self/mountinfo` with `POLLPRI` when a file system
/// is mounted or unmounted, so waiting costs nothing until then. A file
/// that is not under `/proc` (a simulated table) cannot do that; it is read
/// again once per `timeout` instead and compared with the last contents.
pub struct MountWatch {
    file: fs::File,
    path: PathBuf,
    pollable: bool,
    last: Vec<u8>,
}

impl MountWatch {
    pub fn new(path: &Path) -> io::Result<Self> {
        let mut file = fs::File::open(path)?;
        let mut last = Vec::new();
        file.read_to_end(&mut last)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            pollable: path.starts_with("/proc"),
            last,
        })
    }

    /// Waits up to `timeout`; returns whether the table changed meanwhile.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.pollable {
            let changed = crate::inotify::wait_priority(&self.file, timeout)?;
            if changed {
                // Reading keeps the table's own buffer from going stale.
                self.file.rewind()?;
                self.last.clear();
                self.file.read_to_end(&mut self.last)?;
            }
            return Ok(changed);
        }
        std::thread::sleep(timeout);
        let now = fs::read(&self.path).unwrap_or_default();
        if now != self.last {
            self.last = now;
            return Ok(true);
        }
        Ok(false)
    }
}

/// What is remembered about a drive between runs, in
/// `<volumes>/<uuid>.info`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriveInfo {
    pub uuid: String,
    pub label: String,
    /// Where it was mounted last.
    pub path: PathBuf,
    /// When it was last connected, in seconds since 1970; 0 when unknown.
    pub last_seen: i64,
    /// Entries in its saved index (0 when it has none).
    pub entries: u64,
}

impl DriveInfo {
    /// The text of an info file: `key = value` lines; `\` and line breaks
    /// in values are written as `\\`, `\n`; bytes that are not UTF-8 as
    /// `\xHH`.
    pub fn to_text(&self) -> String {
        format!(
            "# EveryGrep: a removable drive\nuuid = {}\nlabel = {}\npath = {}\nlast-seen = {}\nentries = {}\n",
            self.uuid,
            escape(self.label.as_bytes()),
            escape(self.path.as_os_str().as_bytes()),
            self.last_seen,
            self.entries
        )
    }

    /// Reads an info file's text; `None` without a usable UUID.
    pub fn parse(text: &str) -> Option<Self> {
        let mut info = DriveInfo::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "uuid" => info.uuid = value.to_owned(),
                "label" => info.label = String::from_utf8_lossy(&unescape(value)).into_owned(),
                "path" => info.path = PathBuf::from(OsString::from_vec(unescape(value))),
                "last-seen" => info.last_seen = value.parse().unwrap_or(0),
                "entries" => info.entries = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        is_safe_uuid(&info.uuid).then_some(info)
    }

    /// Writes the info file atomically.
    pub fn save(&self, env: &DriveEnv) -> io::Result<()> {
        write_atomic(&env.info_path(&self.uuid), self.to_text().as_bytes())
    }
}

/// Every drive with an info file in the volumes folder, by UUID.
pub fn known_drives(env: &DriveEnv) -> Vec<DriveInfo> {
    let Ok(entries) = fs::read_dir(&env.volumes_dir) else {
        return Vec::new();
    };
    let mut out: Vec<DriveInfo> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "info"))
        .filter_map(|e| DriveInfo::parse(&fs::read_to_string(e.path()).ok()?))
        .filter(|i| env.info_path(&i.uuid).exists())
        .collect();
    out.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    out.dedup_by(|a, b| a.uuid == b.uuid);
    out
}

/// Removes what is kept about a drive: its saved index and, with
/// `info`, its notes too.
pub fn forget(env: &DriveEnv, uuid: &str, info: bool) -> io::Result<()> {
    let mut result = Ok(());
    let mut paths = vec![env.index_path(uuid)];
    if info {
        paths.push(env.info_path(uuid));
    }
    for path in paths {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => result = Err(e),
            _ => {}
        }
    }
    result
}

fn escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    out
}

fn unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            match bytes.get(i + 1) {
                Some(b'\\') => {
                    out.push(b'\\');
                    i += 2;
                    continue;
                }
                Some(b'n') => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                Some(b'x') => {
                    if let Some(v) = bytes
                        .get(i + 2..i + 4)
                        .and_then(|h| std::str::from_utf8(h).ok())
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                    {
                        out.push(v);
                        i += 4;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Writes through a temporary file in the same folder, synced and renamed.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Map from UUID to the drive, for quick lookups.
pub fn by_uuid(drives: &[Drive]) -> HashMap<&str, &Drive> {
    drives.iter().map(|d| (d.uuid.as_str(), d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("everygrep-drives-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A fake system: devices, links and a mount table, all under `dir`.
    fn fake_env(dir: &Path, table: &str) -> DriveEnv {
        let dev = dir.join("dev");
        fs::create_dir_all(dev.join("disk/by-uuid")).unwrap();
        fs::create_dir_all(dev.join("disk/by-label")).unwrap();
        for (uuid, device) in [
            ("1234-ABCD", "sdz1"),
            ("0123456789ABCDEF", "sdx5"),
            ("aaaa-bbbb", "sdz2"),
            ("c0ffee00-1111-2222-3333-444455556666", "sdy1"),
        ] {
            symlink(
                format!("../../{device}"),
                dev.join("disk/by-uuid").join(uuid),
            )
            .unwrap();
        }
        symlink("../../sdz1", dev.join("disk/by-label/My\\x20Disk")).unwrap();
        let table = table.replace("$DEV", &dev.to_string_lossy());
        fs::write(dir.join("mountinfo"), table).unwrap();
        DriveEnv {
            mountinfo: dir.join("mountinfo"),
            media_dirs: vec![PathBuf::from("/run/media/u"), PathBuf::from("/media/u")],
            by_uuid: dev.join("disk/by-uuid"),
            by_label: dev.join("disk/by-label"),
            volumes_dir: dir.join("volumes"),
        }
    }

    const TABLE: &str = "\
22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/sda2 rw
23 22 0:22 / /proc rw shared:2 - proc proc rw
24 22 0:21 /@home /home rw,relatime shared:3 - btrfs /dev/sda2 rw
40 22 8:85 / /mnt/data ro,relatime shared:20 - ext4 $DEV/sdx5 ro
41 22 8:83 / /mnt/backup rw shared:21 - xfs /dev/sdx3 rw
50 22 8:1 / /run/media/u/My\\040Disk rw,nosuid shared:30 - exfat $DEV/sdz1 rw
51 22 8:2 / /run/media/u/sub/nested rw shared:31 - ext4 $DEV/sdz2 rw
52 22 8:2 / /media/u/CARD rw shared:32 - vfat $DEV/sdz2 rw
53 22 0:60 / /run/media/u/tmp rw shared:33 - tmpfs tmpfs rw
54 22 0:61 / /run/media/u/proc rw shared:34 - proc proc rw
55 22 8:17 / /run/media/other/USB rw shared:35 - vfat $DEV/sdy1 rw
56 22 8:1 / /run/media/u/Again rw shared:36 - exfat $DEV/sdz1 rw
garbage line
";

    #[test]
    fn only_desktop_mounted_drives_with_a_uuid_count() {
        let s = Scratch::new("table");
        let env = fake_env(&s.0, TABLE);
        let drives = removable_drives(&env).unwrap();
        assert_eq!(
            drives,
            [
                Drive {
                    uuid: "1234-ABCD".into(),
                    label: "My Disk".into(),
                    mount_point: "/run/media/u/My Disk".into(),
                },
                Drive {
                    uuid: "aaaa-bbbb".into(),
                    label: String::new(),
                    mount_point: "/media/u/CARD".into(),
                },
            ]
        );
    }

    /// The hard rule: fixed disks, fstab mounts under /mnt (other internal
    /// disks), the root and home file systems and anything outside the
    /// media folders are never drives, whatever their file system.
    #[test]
    fn mounts_outside_the_media_folders_are_never_drives() {
        let s = Scratch::new("rule");
        let env = fake_env(&s.0, TABLE);
        let drives = removable_drives(&env).unwrap();
        for d in &drives {
            assert!(
                d.mount_point.starts_with("/run/media/u") || d.mount_point.starts_with("/media/u"),
                "{d:?}"
            );
            assert!(!d.mount_point.starts_with("/mnt"), "{d:?}");
        }
        // The /mnt/data device has a UUID link, and still is not a drive.
        assert!(drives.iter().all(|d| d.uuid != "0123456789ABCDEF"));
        for m in parse_mountinfo(TABLE.as_bytes()) {
            let media = is_media_mount(&m, &env.media_dirs);
            let expected = [
                "/run/media/u/My Disk",
                "/media/u/CARD",
                "/run/media/u/Again",
            ]
            .contains(&m.mount_point.to_str().unwrap());
            assert_eq!(media, expected, "{m:?}");
        }
        // Tricks with dots do not get out of the media folders.
        let sneaky = Mount {
            dev: (8, 1),
            mount_point: "/run/media/u/../../../mnt/data".into(),
            fs_type: "ext4".into(),
            source: "/dev/sdz1".into(),
        };
        assert!(!is_media_mount(&sneaky, &env.media_dirs));
    }

    #[test]
    fn mount_table_fields_are_unescaped() {
        let m = parse_mountinfo(b"1 0 8:1 / /a\\040b\\011c\\134d rw - ext4 /dev/x\\040y rw\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].mount_point, Path::new("/a b\tc\\d"));
        assert_eq!(m[0].source, Path::new("/dev/x y"));
        assert_eq!(m[0].dev, (8, 1));
        assert_eq!(m[0].fs_type, "ext4");
        assert_eq!(unescape_udev("A\\x20B\\x2fC\\xZZ"), "A B/C\\xZZ");
        assert_eq!(dev_numbers(0x0801), (8, 1));
        assert_eq!(dev_numbers((259 << 8) | 5), (259, 5));
    }

    #[test]
    fn info_files_round_trip_and_are_listed() {
        let s = Scratch::new("info");
        let env = fake_env(&s.0, TABLE);
        let info = DriveInfo {
            uuid: "1234-ABCD".into(),
            label: "My \\ Disk\nTwo".into(),
            path: PathBuf::from(OsString::from_vec(b"/run/media/u/x\xffy".to_vec())),
            last_seen: 1_761_431_667,
            entries: 42,
        };
        info.save(&env).unwrap();
        assert_eq!(known_drives(&env), [info]);
        fs::write(env.index_path("1234-ABCD"), b"idx").unwrap();
        // Files that are no drive notes are ignored.
        fs::write(env.volumes_dir.join("x.info"), "uuid = ../../etc\n").unwrap();
        fs::write(env.volumes_dir.join("y.txt"), "uuid = 1\n").unwrap();
        assert_eq!(known_drives(&env).len(), 1);
        forget(&env, "1234-ABCD", false).unwrap();
        assert!(!env.index_path("1234-ABCD").exists());
        assert_eq!(known_drives(&env).len(), 1);
        forget(&env, "1234-ABCD", true).unwrap();
        assert!(known_drives(&env).is_empty());
        assert!(!is_safe_uuid("a/b") && !is_safe_uuid("") && is_safe_uuid("AB-12"));
    }

    #[test]
    fn a_simulated_table_is_compared_when_it_changes() {
        let s = Scratch::new("watch");
        let env = fake_env(&s.0, TABLE);
        let mut w = MountWatch::new(&env.mountinfo).unwrap();
        assert!(!w.wait(Duration::from_millis(10)).unwrap());
        fs::write(&env.mountinfo, "").unwrap();
        assert!(w.wait(Duration::from_millis(10)).unwrap());
        assert!(!w.wait(Duration::from_millis(10)).unwrap());
    }

    #[test]
    fn the_environment_can_be_replaced() {
        // Read only what from_env would build from these values, without
        // changing this process's environment (tests run in parallel).
        let env = DriveEnv {
            mountinfo: "/proc/self/mountinfo".into(),
            media_dirs: vec!["/run/media/me".into(), "/media/me".into()],
            by_uuid: "/dev/disk/by-uuid".into(),
            by_label: "/dev/disk/by-label".into(),
            volumes_dir: "/home/me/.cache/everygrep/volumes".into(),
        };
        assert_eq!(
            env.index_path("AB-12"),
            Path::new("/home/me/.cache/everygrep/volumes/AB-12.bin")
        );
        assert_eq!(
            env.info_path("AB-12"),
            Path::new("/home/me/.cache/everygrep/volumes/AB-12.info")
        );
    }
}
