//! Hashes of file contents: the digests the window shows in its hash
//! columns (MD5, SHA-1/256/384/512, CRC-32, XXH3-128) and a fast 128-bit hash (XXH3-128) that
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

    pub(crate) fn of(meta: &std::fs::Metadata) -> Self {
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
    ctime: (i64, i64),
}

impl OpenFile {
    /// Whether the opened file still has the key and change time it had
    /// when it was opened.
    pub fn unchanged(&self) -> bool {
        self.file.metadata().is_ok_and(|meta| {
            FileKey::of(&meta) == self.key && (meta.ctime(), meta.ctime_nsec()) == self.ctime
        })
    }
}

/// Opens `path` for hashing (see the module notes).
pub fn open(path: &Path) -> Result<OpenFile, HashError> {
    let file = open_regular(path)?;
    let meta = file.metadata().map_err(|_| HashError::Unreadable)?;
    Ok(OpenFile {
        key: FileKey::of(&meta),
        ctime: (meta.ctime(), meta.ctime_nsec()),
        file,
    })
}

/// SHA-256 over bytes written by a transfer, independently of path names.
#[derive(Default)]
pub struct Sha256Stream(sha2::Sha256);
impl Sha256Stream {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

/// Hashes a held regular-file descriptor with positional reads. Its identity,
/// length, mode and change times must stay stable throughout verification.
pub fn sha256_open_file(file: &File, progress: &Progress) -> Result<[u8; 32], HashError> {
    fn stamp(m: &std::fs::Metadata) -> (u64, u64, u64, u32, i64, i64, i64, i64) {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mode(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }
    let before = file.metadata().map_err(|_| HashError::Unreadable)?;
    if !before.is_file() {
        return Err(HashError::NotAFile);
    }
    let mut digest = Sha256Stream::default();
    let mut buf = vec![0; BUFFER];
    let mut offset = 0;
    loop {
        if progress.is_cancelled() {
            return Err(HashError::Cancelled);
        }
        let n = match file.read_at(&mut buf, offset) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(HashError::Unreadable),
        };
        digest.update(&buf[..n]);
        offset += n as u64;
    }
    let after = file.metadata().map_err(|_| HashError::Unreadable)?;
    if offset != before.len() || stamp(&before) != stamp(&after) {
        return Err(HashError::Unreadable);
    }
    Ok(digest.finish())
}

/// The hashes to compute in one reading of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    pub md5: bool,
    pub sha256: bool,
    pub sha1: bool,
    pub sha384: bool,
    pub sha512: bool,
    pub crc32: bool,
    /// XXH3-128 of the whole file, to compare contents.
    pub full: bool,
}

impl Wanted {
    pub fn any(self) -> bool {
        self.md5
            || self.sha256
            || self.sha1
            || self.sha384
            || self.sha512
            || self.crc32
            || self.full
    }
}

/// Hashes of a whole file; each is there when it was asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hashes {
    pub md5: Option<[u8; 16]>,
    pub sha256: Option<[u8; 32]>,
    pub sha1: Option<[u8; 20]>,
    pub sha384: Option<[u8; 48]>,
    pub sha512: Option<[u8; 64]>,
    pub crc32: Option<u32>,
    pub full: Option<u128>,
}

/// Reads `file` from its start to its end once and computes the `wanted`
/// hashes. A cancel of `progress` is seen before each buffer; a file that is not as long as when it was opened is
/// [`HashError::Unreadable`] (it changed meanwhile).
pub fn hash_file(
    file: &mut OpenFile,
    wanted: Wanted,
    progress: &Progress,
) -> Result<Hashes, HashError> {
    let mut md5 = wanted.md5.then(md5::Md5::new);
    let mut sha256 = wanted.sha256.then(sha2::Sha256::new);
    let mut sha1 = wanted.sha1.then(sha1::Sha1::new);
    let mut sha384 = wanted.sha384.then(sha2::Sha384::new);
    let mut sha512 = wanted.sha512.then(sha2::Sha512::new);
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
        if let Some(h) = &mut sha1 {
            h.update(bytes);
        }
        if let Some(h) = &mut sha384 {
            h.update(bytes);
        }
        if let Some(h) = &mut sha512 {
            h.update(bytes);
        }
        if let Some(h) = &mut crc32 {
            h.update(bytes);
        }
        if let Some(h) = &mut full {
            h.write(bytes);
        }
        read += n as u64;
    }
    if read != file.key.size {
        return Err(HashError::Unreadable);
    }
    Ok(Hashes {
        md5: md5.map(|h| h.finalize().into()),
        sha256: sha256.map(|h| h.finalize().into()),
        sha1: sha1.map(|h| h.finalize().into()),
        sha384: sha384.map(|h| h.finalize().into()),
        sha512: sha512.map(|h| h.finalize().into()),
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

/// Why two opened files could not be compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareError {
    Cancelled,
    /// The descriptors that failed a read or changed while being compared.
    Unreadable {
        first: bool,
        second: bool,
    },
}

/// Whether `a` and `b` hold the same bytes, read side by side in pieces of
/// [`BUFFER`]; files of different sizes do not. A cancel of `progress` is
/// seen before each piece. A file that is no longer the one opened (its
/// size, modification time or change time changed, by the end of the reading) is
/// [`CompareError::Unreadable`]: equal pieces of it say nothing.
pub fn same_contents(
    a: &OpenFile,
    b: &OpenFile,
    progress: &Progress,
) -> Result<bool, CompareError> {
    same_contents_with_read(a, b, progress, read_exact_at)
}

pub(crate) fn same_contents_with_read(
    a: &OpenFile,
    b: &OpenFile,
    progress: &Progress,
    mut read: impl FnMut(&File, &mut [u8], u64) -> Result<(), HashError>,
) -> Result<bool, CompareError> {
    let size = a.key.size;
    let mut equal = size == b.key.size;
    let piece = (BUFFER as u64).min(size) as usize;
    let (mut x, mut y) = (vec![0u8; piece], vec![0u8; piece]);
    let mut at = 0u64;
    while equal && at < size {
        if progress.is_cancelled() {
            return Err(CompareError::Cancelled);
        }
        let n = (size - at).min(BUFFER as u64) as usize;
        read(&a.file, &mut x[..n], at).map_err(|_| CompareError::Unreadable {
            first: true,
            second: false,
        })?;
        read(&b.file, &mut y[..n], at).map_err(|_| CompareError::Unreadable {
            first: false,
            second: true,
        })?;
        equal = x[..n] == y[..n];
        at += n as u64;
    }
    let (first, second) = (!a.unchanged(), !b.unchanged());
    if first || second {
        return Err(CompareError::Unreadable { first, second });
    }
    Ok(equal)
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

/// A digest column, with stable column numbers shared by the table and sorts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Md5,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
    Crc32,
    Xxh3,
}

impl Algorithm {
    pub const ALL: [Self; 7] = [
        Self::Md5,
        Self::Sha1,
        Self::Sha256,
        Self::Sha384,
        Self::Sha512,
        Self::Crc32,
        Self::Xxh3,
    ];
    /// Query keyword, independent of display titles.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
            Self::Crc32 => "crc32",
            Self::Xxh3 => "xxhash",
        }
    }
    pub fn from_keyword(word: &str) -> Option<Self> {
        let word = word.to_ascii_lowercase();
        match word.as_str() {
            "sha-1" => Some(Self::Sha1),
            "sha-256" => Some(Self::Sha256),
            "sha-384" => Some(Self::Sha384),
            "sha-512" => Some(Self::Sha512),
            "crc-32" => Some(Self::Crc32),
            "xxh3" | "xxh3-128" => Some(Self::Xxh3),
            _ => Self::ALL.into_iter().find(|a| a.keyword() == word),
        }
    }
    /// Parses an exact digest; no truncation, whitespace or query syntax.
    pub fn parse_hex(self, value: &str) -> Result<Vec<u8>, String> {
        let bytes = match self {
            Self::Md5 | Self::Xxh3 => 16,
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
            Self::Crc32 => 4,
        };
        if value.len() != 2 * bytes || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "{} needs {} hexadecimal digits",
                self.keyword(),
                2 * bytes
            ));
        }
        let digit = |b: u8| {
            if b.is_ascii_digit() {
                b - b'0'
            } else {
                b.to_ascii_lowercase() - b'a' + 10
            }
        };
        Ok(value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| digit(c[0]) * 16 + digit(c[1]))
            .collect())
    }

    pub fn column(self) -> i32 {
        match self {
            Self::Md5 => 9,
            Self::Sha256 => 10,
            Self::Crc32 => 11,
            Self::Sha1 => 52,
            Self::Sha384 => 53,
            Self::Sha512 => 54,
            Self::Xxh3 => 55,
        }
    }
    pub fn from_column(column: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.column() == column)
    }
    pub fn wanted(self) -> Wanted {
        let mut w = Wanted::default();
        match self {
            Self::Md5 => w.md5 = true,
            Self::Sha1 => w.sha1 = true,
            Self::Sha256 => w.sha256 = true,
            Self::Sha384 => w.sha384 = true,
            Self::Sha512 => w.sha512 = true,
            Self::Crc32 => w.crc32 = true,
            Self::Xxh3 => w.full = true,
        }
        w
    }
    /// Canonical bytes; lexicographic order is the order of displayed hex digits.
    pub fn bytes(self, h: &crate::hashcache::Cached) -> Option<Vec<u8>> {
        match self {
            Self::Md5 => h.md5.map(|v| v.to_vec()),
            Self::Sha1 => h.sha1.map(|v| v.to_vec()),
            Self::Sha256 => h.sha256.map(|v| v.to_vec()),
            Self::Sha384 => h.sha384.map(|v| v.to_vec()),
            Self::Sha512 => h.sha512.map(|v| v.to_vec()),
            Self::Crc32 => h.crc32.map(|v| v.to_be_bytes().to_vec()),
            Self::Xxh3 => h.full.map(|v| v.to_be_bytes().to_vec()),
        }
    }
}

/// Reads only missing digests, once, and merges them into the shared cache.
/// A zero-sized cache still returns the computed values.
pub fn read_cached(
    path: &Path,
    wanted: Wanted,
    cache: Option<&std::sync::Mutex<crate::hashcache::HashCache>>,
    progress: &Progress,
) -> Result<crate::hashcache::Cached, HashError> {
    use crate::hashcache::Cached;
    if progress.is_cancelled() {
        return Err(HashError::Cancelled);
    }
    let mut file = open(path)?;
    let known = cache
        .and_then(|c| c.lock().expect("hash cache").get(&file.key))
        .unwrap_or_default();
    let missing = Wanted {
        md5: wanted.md5 && known.md5.is_none(),
        sha1: wanted.sha1 && known.sha1.is_none(),
        sha256: wanted.sha256 && known.sha256.is_none(),
        sha384: wanted.sha384 && known.sha384.is_none(),
        sha512: wanted.sha512 && known.sha512.is_none(),
        crc32: wanted.crc32 && known.crc32.is_none(),
        full: wanted.full && known.full.is_none(),
    };
    if !missing.any() {
        return Ok(known);
    }
    let read = hash_file(&mut file, missing, progress)?;
    let hashes = Cached {
        md5: read.md5.or(known.md5),
        sha1: read.sha1.or(known.sha1),
        sha256: read.sha256.or(known.sha256),
        sha384: read.sha384.or(known.sha384),
        sha512: read.sha512.or(known.sha512),
        crc32: read.crc32.or(known.crc32),
        full: read.full.or(known.full),
        ..known
    };
    if let Some(cache) = cache {
        cache.lock().expect("hash cache").put(file.key, &hashes);
    }
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn copied_byte_stream_and_held_descriptor_match_the_standard_sha256_vector() {
        let mut digest = Sha256Stream::default();
        digest.update(b"a");
        digest.update(b"bc");
        let expected = digest.finish();
        let hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let t = Dir::new("copied-digest");
        let path = t.0.join("held");
        std::fs::write(&path, b"abc").unwrap();
        let file = File::open(path).unwrap();
        assert_eq!(sha256_open_file(&file, &Progress::new()).unwrap(), expected);
    }

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
        sha1: true,
        sha384: true,
        sha512: true,
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
    fn additional_digests_match_known_vectors_and_are_optional() {
        let dir = Dir::new("additional-vectors");
        let abc = hashes(&dir.file("abc", b"abc"), ALL).unwrap();
        assert_eq!(
            hex(&abc.sha1.unwrap()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&abc.sha384.unwrap()),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            hex(&abc.sha512.unwrap()),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(abc.full, Some(0x06b05ab6733a618578af5f94892f3950));
        let empty = hashes(&dir.file("empty", b""), ALL).unwrap();
        assert_eq!(
            hex(&empty.sha1.unwrap()),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(&empty.sha384.unwrap()),
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b"
        );
        assert_eq!(
            hex(&empty.sha512.unwrap()),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(empty.full, Some(0x99aa06d3014798d86001c324468d497f));
        let big: Vec<u8> = (0..BUFFER * 2 + 9).map(|i| (i % 251) as u8).collect();
        let file = dir.file("big", &big);
        let h = hashes(&file, ALL).unwrap();
        assert_eq!(h.sha1, Some(sha1::Sha1::digest(&big).into()));
        assert_eq!(h.sha384, Some(sha2::Sha384::digest(&big).into()));
        assert_eq!(h.sha512, Some(sha2::Sha512::digest(&big).into()));
        for a in Algorithm::ALL {
            assert_eq!(Algorithm::from_column(a.column()), Some(a));
            let only = read_cached(&file, a.wanted(), None, &Progress::new()).unwrap();
            assert!(a.bytes(&only).is_some());
            for other in Algorithm::ALL.into_iter().filter(|other| *other != a) {
                assert_eq!(other.bytes(&only), None);
            }
        }
    }

    #[test]
    fn additional_hashes_merge_in_cache_and_cancellation_keeps_it_intact() {
        let dir = Dir::new("cached-additional");
        let path = dir.file("abc", b"abc");
        let cache = std::sync::Mutex::new(crate::hashcache::HashCache::default());
        let first = read_cached(
            &path,
            Algorithm::Sha1.wanted(),
            Some(&cache),
            &Progress::new(),
        )
        .unwrap();
        let second = read_cached(
            &path,
            Algorithm::Sha512.wanted(),
            Some(&cache),
            &Progress::new(),
        )
        .unwrap();
        assert_eq!(first.sha1, second.sha1);
        assert!(second.sha512.is_some());
        let p = Progress::new();
        p.cancel();
        assert_eq!(
            read_cached(&path, ALL, Some(&cache), &p),
            Err(HashError::Cancelled)
        );
        assert_eq!(
            cache.lock().unwrap().get(&open(&path).unwrap().key),
            Some(second)
        );
        std::fs::write(&path, b"changed").unwrap();
        let changed = read_cached(
            &path,
            Algorithm::Sha1.wanted(),
            Some(&cache),
            &Progress::new(),
        )
        .unwrap();
        assert_ne!(changed.sha1, first.sha1);
        assert_eq!(changed.sha512, None);
        let disabled = std::sync::Mutex::new(crate::hashcache::HashCache::new(0));
        assert!(
            read_cached(&path, ALL, Some(&disabled), &Progress::new())
                .unwrap()
                .sha384
                .is_some()
        );
        assert!(disabled.lock().unwrap().is_empty());
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
    fn a_file_that_grows_or_changes_while_compared_is_not_the_same() {
        let dir = Dir::new("grow");
        let bytes = vec![5u8; BUFFER + 10];
        let (a, b) = (dir.file("a", &bytes), dir.file("b", &bytes));
        let progress = Progress::new();
        assert_eq!(
            same_contents(&open(&a).unwrap(), &open(&b).unwrap(), &progress),
            Ok(true)
        );
        // Bytes added after it was opened: its first bytes are still equal.
        let (x, y) = (open(&a).unwrap(), open(&b).unwrap());
        use std::io::Write;
        let mut append = std::fs::OpenOptions::new().append(true).open(&b).unwrap();
        append.write_all(b"more").unwrap();
        assert_eq!(
            same_contents(&x, &y, &progress),
            Err(CompareError::Unreadable {
                first: false,
                second: true
            })
        );
        // Empty files are compared without reading too.
        let (e, f) = (dir.file("e", b""), dir.file("f", b""));
        let (x, y) = (open(&e).unwrap(), open(&f).unwrap());
        std::fs::write(&f, b"now full").unwrap();
        assert_eq!(
            same_contents(&x, &y, &progress),
            Err(CompareError::Unreadable {
                first: false,
                second: true
            })
        );
    }

    #[test]
    fn restored_mtime_does_not_hide_a_write_to_compared_bytes() {
        let dir = Dir::new("restored-mtime");
        let bytes = vec![5u8; BUFFER + 10];
        let (a, b) = (dir.file("a", &bytes), dir.file("b", &bytes));
        let (x, y) = (open(&a).unwrap(), open(&b).unwrap());
        let modified = y.file.metadata().unwrap().modified().unwrap();
        let writer = std::fs::OpenOptions::new().write(true).open(&b).unwrap();
        let mut reads = 0;
        let result = same_contents_with_read(&x, &y, &Progress::new(), |file, buf, at| {
            read_exact_at(file, buf, at)?;
            reads += 1;
            if reads == 2 {
                // Both first buffers have been read. Change those bytes only,
                // keeping the remaining bytes, size and mtime unchanged.
                writer.write_all_at(b"changed", 0).unwrap();
                writer.set_modified(modified).unwrap();
                assert_eq!(FileKey::of(&writer.metadata().unwrap()), y.key);
            }
            Ok(())
        });
        assert_eq!(reads, 4);
        assert_eq!(
            result,
            Err(CompareError::Unreadable {
                first: false,
                second: true
            })
        );
    }

    #[test]
    fn a_byte_difference_still_checks_both_file_snapshots() {
        let dir = Dir::new("changed-difference");
        let (a, b) = (dir.file("a", b"same"), dir.file("b", b"same"));
        let (x, y) = (open(&a).unwrap(), open(&b).unwrap());
        let writer = std::fs::OpenOptions::new().write(true).open(&b).unwrap();
        let mut reads = 0;
        let result = same_contents_with_read(&x, &y, &Progress::new(), |file, buf, at| {
            read_exact_at(file, buf, at)?;
            reads += 1;
            if reads == 1 {
                writer.write_all_at(b"diff", 0).unwrap();
                writer
                    .set_modified(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap();
            }
            Ok(())
        });
        assert_eq!(
            result,
            Err(CompareError::Unreadable {
                first: false,
                second: true
            })
        );
        // Stable, genuinely different files remain an ordinary mismatch.
        assert_eq!(
            same_contents(&open(&a).unwrap(), &open(&b).unwrap(), &Progress::new()),
            Ok(false)
        );
    }

    #[test]
    fn hex_is_lowercase_two_digits_per_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex(&[]), "");
    }
}
