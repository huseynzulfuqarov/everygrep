//! Details that the index does not keep: creation time, access time and
//! attributes. They are read from the file system when asked for, so only
//! the rows on screen (or the results being sorted by them) pay for them,
//! and the index stays as small as it is.
//!
//! - **Created** is the birth time (`statx` with `STATX_BTIME`). File systems
//!   that do not record it (and older kernels) give none, and the column stays
//!   empty instead of showing a made-up date.
//! - **Accessed** is the access time. Most systems mount with `relatime`, which
//!   updates it at most once a day, or only after a change, so it is coarse.
//! - **Attributes** are letters for properties of the entry, see
//!   [`Attributes`].
//!
//! Links are never followed: the details are those of the link itself.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Attribute bits of an entry, shown as letters (see [`letters`](Self::letters)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Attributes(pub u8);

impl Attributes {
    /// `R`: read-only, the owner has no write permission.
    pub const READ_ONLY: u8 = 1 << 4;
    /// `H`: hidden, the name starts with a dot.
    pub const HIDDEN: u8 = 1 << 3;
    /// `D`: a directory.
    pub const DIRECTORY: u8 = 1 << 2;
    /// `L`: a symbolic link.
    pub const LINK: u8 = 1 << 1;
    /// `X`: a regular file that someone may execute.
    pub const EXECUTABLE: u8 = 1 << 0;

    /// The letters, in this order, with the bit each stands for.
    const LETTERS: [(u8, char); 5] = [
        (Self::READ_ONLY, 'R'),
        (Self::HIDDEN, 'H'),
        (Self::DIRECTORY, 'D'),
        (Self::LINK, 'L'),
        (Self::EXECUTABLE, 'X'),
    ];

    /// Attributes of an entry named `name` with `lstat` metadata `meta`.
    pub fn from_metadata(name: &[u8], meta: &fs::Metadata) -> Self {
        Self::from_mode(name, meta.file_type(), meta.permissions().mode())
    }

    fn from_mode(name: &[u8], kind: fs::FileType, mode: u32) -> Self {
        let mut bits = 0;
        // A link's own permission bits mean nothing on Linux.
        if !kind.is_symlink() && mode & 0o200 == 0 {
            bits |= Self::READ_ONLY;
        }
        if name.first() == Some(&b'.') {
            bits |= Self::HIDDEN;
        }
        if kind.is_dir() {
            bits |= Self::DIRECTORY;
        }
        if kind.is_symlink() {
            bits |= Self::LINK;
        }
        if kind.is_file() && mode & 0o111 != 0 {
            bits |= Self::EXECUTABLE;
        }
        Self(bits)
    }

    /// Whether every bit of `bits` is set.
    pub fn has(self, bits: u8) -> bool {
        self.0 & bits == bits
    }

    /// Parses attribute letters, ignoring order and ASCII case. Unknown letters fail.
    pub fn parse_letters(text: &str) -> Option<Self> {
        let mut bits = 0;
        for letter in text.bytes() {
            let (bit, _) = Self::LETTERS
                .iter()
                .find(|(_, c)| *c as u8 == letter.to_ascii_uppercase())?;
            bits |= bit;
        }
        Some(Self(bits))
    }

    /// The letters of the bits that are set, in the order `RHDLX`.
    pub fn letters(self) -> String {
        Self::LETTERS
            .iter()
            .filter(|(bit, _)| self.has(*bit))
            .map(|(_, c)| *c)
            .collect()
    }
}

/// Details of one entry. Each part is `None` when it could not be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Details {
    /// Birth time in nanoseconds since the Unix epoch; `None` where the file
    /// system does not record it.
    pub created_ns: Option<i64>,
    /// Access time in nanoseconds since the Unix epoch.
    pub accessed_ns: Option<i64>,
    pub attributes: Option<Attributes>,
}

impl Details {
    /// Reads the details of `path` (whose last component is `name`) without
    /// following a link. An entry that cannot be read (gone, no permission)
    /// has no details.
    pub fn read(path: &Path, name: &[u8]) -> Self {
        match fs::symlink_metadata(path) {
            Ok(meta) => Self::from_metadata(name, &meta),
            Err(_) => Self::default(),
        }
    }

    /// Details from `lstat` metadata. The standard library asks `statx` for
    /// the birth time and reports an error when the file system has none.
    pub fn from_metadata(name: &[u8], meta: &fs::Metadata) -> Self {
        Self {
            created_ns: meta.created().ok().and_then(system_time_ns),
            accessed_ns: Some(nanos(meta.atime(), meta.atime_nsec())),
            attributes: Some(Attributes::from_metadata(name, meta)),
        }
    }
}

fn nanos(secs: i64, nsec: i64) -> i64 {
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

/// Nanoseconds since the Unix epoch (negative before it). `None` for the
/// value 0 some file systems report when they have no birth time.
fn system_time_ns(t: std::time::SystemTime) -> Option<i64> {
    let ns = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    };
    (ns != 0).then_some(ns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "everygrep-details-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn attrs(path: &Path) -> String {
        let name = path.file_name().unwrap().as_encoded_bytes();
        Details::read(path, name).attributes.unwrap().letters()
    }

    #[test]
    fn attribute_letters_follow_the_mode_name_and_kind() {
        let dir = temp_dir("attrs");
        let plain = dir.join("plain.txt");
        fs::write(&plain, b"x").unwrap();
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(attrs(&plain), "");

        let ro = dir.join("ro.txt");
        fs::write(&ro, b"x").unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(attrs(&ro), "R");

        let script = dir.join(".run.sh");
        fs::write(&script, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(attrs(&script), "HX");

        // Only the owner's write bit counts for read-only; any x bit for
        // executable, but never for a folder.
        let group_x = dir.join("gx");
        fs::write(&group_x, b"").unwrap();
        fs::set_permissions(&group_x, fs::Permissions::from_mode(0o610)).unwrap();
        assert_eq!(attrs(&group_x), "X");

        let sub = dir.join(".hidden-dir");
        fs::create_dir(&sub).unwrap();
        assert_eq!(attrs(&sub), "HD");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(attrs(&sub), "RHD");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();

        // A link is not followed: a link to a read-only file is just a link.
        let link = dir.join("to-ro");
        symlink(&ro, &link).unwrap();
        assert_eq!(attrs(&link), "L");

        fs::set_permissions(&ro, fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn attribute_bits_order_by_letter_weight() {
        let r = Attributes(Attributes::READ_ONLY);
        let hdx = Attributes(Attributes::HIDDEN | Attributes::DIRECTORY | Attributes::EXECUTABLE);
        assert!(r > hdx);
        assert_eq!(Attributes::default().letters(), "");
        assert_eq!(Attributes(0x1f).letters(), "RHDLX");
    }

    #[test]
    fn missing_entries_have_no_details() {
        let d = Details::read(Path::new("/nonexistent/everygrep/details"), b"details");
        assert_eq!(d, Details::default());
        assert_eq!(d.created_ns, None);
        assert_eq!(d.attributes, None);
    }

    #[test]
    fn times_are_read_and_a_missing_birth_time_stays_unknown() {
        let dir = temp_dir("times");
        let f = dir.join("f");
        fs::write(&f, b"x").unwrap();
        let d = Details::read(&f, b"f");
        let now = system_time_ns(std::time::SystemTime::now()).unwrap();
        let accessed = d.accessed_ns.unwrap();
        assert!(
            (now - accessed).abs() < 3_600_000_000_000,
            "{accessed} vs {now}"
        );
        // Not every file system records a birth time; when it does, it is
        // recent, and when it does not, there is none (never 0 or 1970).
        if let Some(created) = d.created_ns {
            assert!((now - created).abs() < 3_600_000_000_000);
        }
        assert_eq!(system_time_ns(std::time::UNIX_EPOCH), None);
        assert_eq!(
            system_time_ns(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)),
            Some(-1_000_000_000)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_system_without_birth_times_gives_none() {
        // procfs records no birth time; the access time is still there.
        let d = Details::read(Path::new("/proc/self/stat"), b"stat");
        assert_eq!(d.created_ns, None);
        assert!(d.accessed_ns.is_some());
        assert!(d.attributes.is_some());
    }
}
