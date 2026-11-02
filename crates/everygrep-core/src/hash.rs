//! Hashes of file contents: the digests the window shows in its hash
//! columns (MD5, SHA-256, CRC-32) and a fast 128-bit hash (XXH3-128) that
//! tells files with the same contents apart from the rest (see
//! [`crate::dupe`]).
//!
//! Files are read like `content:` reads them (see [`crate::content`]):
//! regular files only, without following a link, without waiting on a FIFO
//! swapped in, and without changing the access time when the file belongs
//! to the user. A file is read once for all the hashes asked for, in
//! buffers of [`BUFFER`] bytes; a cancel is seen before each one.
//!
//! [`FileKey`] is what the file system says about a file's contents: device,
//! inode, size and modification time. Two paths with the same device and
//! inode are one file (hard links). A [`crate::hashcache::HashCache`] keeps
//! hashes by that key, so a file that changed (another size or time) or was
//! replaced (another inode) is read again.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use md5::Digest;
use twox_hash::XxHash3_128;

use crate::content::{Outcome, open_regular};
use crate::progress::Progress;

/// Bytes read at a time.
pub const BUFFER: usize = 256 * 1024;

/// Bytes [`ends_hash`] reads at each end of a file.
pub const END: u64 = 16 * 1024;

/// What identifies the contents of a file as the file system describes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileKey {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
}

impl FileKey {
    /// Device and inode: the same for every hard link of a file.
    pub fn inode(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }

    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: meta
                .mtime()
                .saturating_mul(1_000_000_000)
                .saturating_add(meta.mtime_nsec()),
        }
    }
}

/// Why a file has no hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashError {
    /// The search was cancelled while reading.
    Cancelled,
    /// A link, folder, FIFO, socket or device: not read.
    NotAFile,
    /// It could not be opened or read, or it changed size while it was read.
    Unreadable,
}

impl From<Outcome> for HashError {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::NotAFile => Self::NotAFile,
            _ => Self::Unreadable,
        }
    }
}

/// A regular file opened for hashing, with its key as it was when opened.
pub struct OpenFile {
    file: File,
    pub key: FileKey,
}

/// Opens `path` for hashing (see the module notes).
pub fn open(path: &Path) -> Result<OpenFile, HashError> {
    let file = open_regular(path)?;
    let meta = file.metadata().map_err(|_| HashError::Unreadable)?;
    Ok(OpenFile {
        key: FileKey::of(&meta),
        file,
    })
}

/// The hashes to compute in one reading of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    pub md5: bool,
    pub sha256: bool,
    pub crc32: bool,
    /// XXH3-128 of the whole file, to compare contents.
    pub full: bool,
}

impl Wanted {
    pub fn any(self) -> bool {
        self.md5 || self.sha256 || self.crc32 || self.full
    }
}

/// Hashes of a whole file; each is there when it was asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hashes {
    pub md5: Option<[u8; 16]>,
    pub sha256: Option<[u8; 32]>,
    pub crc32: Option<u32>,
    pub full: Option<u128>,
}

/// Reads `file` from its start to its end once and computes the `wanted`
/// hashes, adding the bytes read to `progress`. A cancel is seen before each
/// buffer; a file that is not as long as when it was opened is
/// [`HashError::Unreadable`] (it changed meanwhile).
pub fn hash_file(
    file: &mut OpenFile,
    wanted: Wanted,
    progress: &Progress,
) -> Result<Hashes, HashError> {
    let mut md5 = wanted.md5.then(md5::Md5::new);
    let mut sha256 = wanted.sha256.then(sha2::Sha256::new);
    let mut crc32 = wanted.crc32.then(crc32fast::Hasher::new);
    let mut full = wanted.full.then(XxHash3_128::new);
    let mut buf = vec![0u8; BUFFER];
    let mut read = 0u64;
    loop {
        if progress.is_cancelled() {
            return Err(HashError::Cancelled);
        }
        let n = match file.file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(HashError::Unreadable),
        };
        let bytes = &buf[..n];
        if let Some(h) = &mut md5 {
            h.update(bytes);
        }
        if let Some(h) = &mut sha256 {
            h.update(bytes);
        }
        if let Some(h) = &mut crc32 {
            h.update(bytes);
        }
        if let Some(h) = &mut full {
            h.write(bytes);
        }
        read += n as u64;
        progress.add(n as u64);
    }
    if read != file.key.size {
        return Err(HashError::Unreadable);
    }
    Ok(Hashes {
        md5: md5.map(|h| h.finalize().into()),
        sha256: sha256.map(|h| h.finalize().into()),
        crc32: crc32.map(crc32fast::Hasher::finalize),
        full: full.map(|h| h.finish_128()),
    })
}

/// XXH3-128 of the first and the last [`END`] bytes of `file` (all of a
/// file up to twice as long), with its size: files whose ends differ cannot
/// have the same contents, and most files that are not copies differ there.
pub fn ends_hash(file: &OpenFile, progress: &Progress) -> Result<u128, HashError> {
    if progress.is_cancelled() {
        return Err(HashError::Cancelled);
    }
    let size = file.key.size;
    let mut h = XxHash3_128::new();
    h.write(&size.to_le_bytes());
    let mut buf = vec![0u8; (2 * END).min(size) as usize];
    if size <= 2 * END {
        read_exact_at(&file.file, &mut buf, 0)?;
    } else {
        let (head, tail) = buf.split_at_mut(END as usize);
        read_exact_at(&file.file, head, 0)?;
        read_exact_at(&file.file, tail, size - END)?;
    }
    h.write(&buf);
    Ok(h.finish_128())
}

fn read_exact_at(file: &File, buf: &mut [u8], at: u64) -> Result<(), HashError> {
    file.read_exact_at(buf, at)
        .map_err(|_| HashError::Unreadable)
}

/// Lowercase hexadecimal digits of `bytes`, as hash tools print them.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("everygrep-hash-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ALL: Wanted = Wanted {
        md5: true,
        sha256: true,
        crc32: true,
        full: true,
    };

    fn hashes(path: &Path, wanted: Wanted) -> Result<Hashes, HashError> {
        hash_file(&mut open(path)?, wanted, &Progress::new())
    }

    #[test]
    fn digests_match_the_published_test_values() {
        let dir = Dir::new("vectors");
        let abc = hashes(&dir.file("abc", b"abc"), ALL).unwrap();
        assert_eq!(hex(&abc.md5.unwrap()), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&abc.sha256.unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let check = hashes(&dir.file("check", b"123456789"), ALL).unwrap();
        assert_eq!(check.crc32, Some(0xCBF4_3926));
        let empty = hashes(&dir.file("empty", b""), ALL).unwrap();
        assert_eq!(hex(&empty.md5.unwrap()), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            hex(&empty.sha256.unwrap()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(empty.crc32, Some(0));
        // XXH3-128 with seed 0, as the crate computes it in one call.
        assert_eq!(abc.full, Some(XxHash3_128::oneshot(b"abc")));

        // Only what is asked for, and the same across buffer boundaries.
        let big: Vec<u8> = (0..BUFFER * 2 + 17).map(|i| (i * 7 % 251) as u8).collect();
        let path = dir.file("big", &big);
        let only = hashes(
            &path,
            Wanted {
                crc32: true,
                ..Wanted::default()
            },
        )
        .unwrap();
        assert_eq!((only.md5, only.sha256, only.full), (None, None, None));
        assert_eq!(only.crc32, Some(crc32fast::hash(&big)));
        let all = hashes(&path, ALL).unwrap();
        assert_eq!(all.full, Some(XxHash3_128::oneshot(&big)));
        assert_eq!(all.md5, Some(md5::Md5::digest(&big).into()));
        assert_eq!(all.sha256, Some(sha2::Sha256::digest(&big).into()));
    }

    #[test]
    fn links_folders_and_fifos_are_not_read() {
        let dir = Dir::new("kinds");
        let target = dir.file("target", b"data");
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(open(&link).err(), Some(HashError::NotAFile));
        assert_eq!(open(&dir.0).err(), Some(HashError::NotAFile));
        let fifo = dir.0.join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            assert_eq!(open(&fifo).err(), Some(HashError::NotAFile));
        }
        assert_eq!(
            open(&dir.0.join("missing")).err(),
            Some(HashError::Unreadable)
        );
    }

    #[test]
    fn keys_tell_hard_links_and_changes_apart() {
        let dir = Dir::new("keys");
        let a = dir.file("a", b"one");
        let b = dir.0.join("b");
        std::fs::hard_link(&a, &b).unwrap();
        let c = dir.file("c", b"one");
        let (ka, kb, kc) = (
            open(&a).unwrap().key,
            open(&b).unwrap().key,
            open(&c).unwrap().key,
        );
        assert_eq!(ka, kb);
        assert_eq!(ka.inode(), kb.inode());
        assert_ne!(ka.inode(), kc.inode());
        assert_eq!(ka.size, 3);
        std::fs::write(&a, b"other").unwrap();
        let changed = open(&b).unwrap().key;
        assert_eq!(changed.inode(), ka.inode());
        assert_ne!(changed, ka);
    }

    #[test]
    fn the_ends_tell_most_files_apart_and_read_little() {
        let dir = Dir::new("ends");
        let mut body = vec![b'x'; (4 * END) as usize];
        let a = dir.file("a", &body);
        body[(2 * END) as usize] = b'y';
        // Differs in the middle only: the ends cannot tell.
        let b = dir.file("b", &body);
        body[0] = b'z';
        let c = dir.file("c", &body);
        let ends = |p: &Path| ends_hash(&open(p).unwrap(), &Progress::new()).unwrap();
        assert_eq!(ends(&a), ends(&b));
        assert_ne!(ends(&a), ends(&c));
        // Small files are hashed whole, and the size is part of it.
        let s1 = dir.file("s1", b"same");
        let s2 = dir.file("s2", b"same");
        let s3 = dir.file("s3", b"same\0");
        assert_eq!(ends(&s1), ends(&s2));
        assert_ne!(ends(&s1), ends(&s3));
        let empty = dir.file("empty", b"");
        assert_ne!(ends(&empty), ends(&s1));
    }

    #[test]
    fn a_cancel_stops_before_the_next_buffer() {
        let dir = Dir::new("cancel");
        let path = dir.file("big", &vec![1u8; BUFFER * 4]);
        let progress = Progress::new();
        progress.cancel();
        let mut file = open(&path).unwrap();
        assert_eq!(
            hash_file(&mut file, ALL, &progress),
            Err(HashError::Cancelled)
        );
        assert_eq!(progress.done(), 0);
        assert_eq!(ends_hash(&file, &progress), Err(HashError::Cancelled));
        let progress = Progress::new();
        hash_file(&mut open(&path).unwrap(), ALL, &progress).unwrap();
        assert_eq!(progress.done(), (BUFFER * 4) as u64);
    }

    #[test]
    fn a_file_that_shrinks_while_read_has_no_hash() {
        let dir = Dir::new("shrink");
        let path = dir.file("f", &vec![3u8; BUFFER * 2]);
        let mut file = open(&path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(10)
            .unwrap();
        assert_eq!(
            hash_file(&mut file, ALL, &Progress::new()),
            Err(HashError::Unreadable)
        );
    }

    #[test]
    fn hex_is_lowercase_two_digits_per_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex(&[]), "");
    }
}
