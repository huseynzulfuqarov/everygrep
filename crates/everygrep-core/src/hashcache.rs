//! Hashes of files kept between searches and between runs, so a file is
//! hashed again only when it changed.
//!
//! Entries are keyed by [`FileKey`] (device, inode, size, modification
//! time): a file written to, truncated or replaced by another file gets
//! another key and is read again, and its old entry is never used again
//! (it goes when room is needed). Paths are not kept, so renaming or moving
//! a file within a file system keeps its hashes, and the file says nothing
//! about the names of the files it describes.
//!
//! The cache has a size limit in bytes, counted as the entries take in the
//! file (a key and the hashes it holds, 41 to 257 bytes); when it is
//! exceeded, the entries used longest ago are dropped until a tenth is
//! free. A limit of 0 keeps nothing.
//!
//! The file (`$XDG_CACHE_HOME/everygrep/hashes.bin` by default) starts with
//! the magic bytes `EGHASH01` and the number of entries (u64), followed by
//! the entries: device, inode, size (u64), modification time (i64), when
//! last used (u64, a counter), a byte of flags for the hashes present, then
//! those hashes in this order: the ends hash (16 bytes), the full hash (16),
//! MD5 (16), SHA-256 (32), CRC-32 (u32), SHA-1 (20), SHA-384 (48), SHA-512 (64). Numbers are little-endian. It is
//! written to a new file that then replaces the old one. A file that cannot
//! be read as this format is ignored (and replaced at the next save).

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::hash::FileKey;

/// The default size limit: 512 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;

const MAGIC: &[u8; 8] = b"EGHASH01";

/// Bytes of an entry without hashes: key, last use and flags.
const ENTRY_BASE: u64 = 8 * 5 + 1;

/// The hashes known for one file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cached {
    /// See [`crate::hash::ends_hash`].
    pub ends: Option<u128>,
    /// XXH3-128 of the whole file.
    pub full: Option<u128>,
    pub md5: Option<[u8; 16]>,
    pub sha256: Option<[u8; 32]>,
    pub sha1: Option<[u8; 20]>,
    pub sha384: Option<[u8; 48]>,
    pub sha512: Option<[u8; 64]>,
    pub crc32: Option<u32>,
}

impl Cached {
    /// Bytes the entry takes in the file.
    fn bytes(&self) -> u64 {
        ENTRY_BASE
            + u64::from(self.ends.is_some()) * 16
            + u64::from(self.full.is_some()) * 16
            + u64::from(self.md5.is_some()) * 16
            + u64::from(self.sha256.is_some()) * 32
            + u64::from(self.crc32.is_some()) * 4
            + u64::from(self.sha1.is_some()) * 20
            + u64::from(self.sha384.is_some()) * 48
            + u64::from(self.sha512.is_some()) * 64
    }

    /// Takes the hashes `other` has.
    fn merge(&mut self, other: &Cached) {
        self.ends = other.ends.or(self.ends);
        self.full = other.full.or(self.full);
        self.md5 = other.md5.or(self.md5);
        self.sha256 = other.sha256.or(self.sha256);
        self.crc32 = other.crc32.or(self.crc32);
        self.sha1 = other.sha1.or(self.sha1);
        self.sha384 = other.sha384.or(self.sha384);
        self.sha512 = other.sha512.or(self.sha512);
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    hashes: Cached,
    used: u64,
}

/// File hashes by [`FileKey`], with a size limit (see the module notes).
#[derive(Debug)]
pub struct HashCache {
    entries: HashMap<FileKey, Entry>,
    /// Sum of the entries' bytes.
    bytes: u64,
    max_bytes: u64,
    /// The next value of an entry's last use.
    clock: u64,
    /// Changed since it was loaded or saved.
    changed: bool,
}

impl Default for HashCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BYTES)
    }
}

impl HashCache {
    /// An empty cache that keeps at most `max_bytes`.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            max_bytes,
            clock: 1,
            changed: false,
        }
    }

    /// Number of files with hashes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes the entries take in the file (the file has 16 bytes more).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Whether entries were added, changed or dropped since the cache was
    /// loaded or last saved.
    pub fn is_changed(&self) -> bool {
        self.changed
    }

    /// Says whether the cache differs from its file, for a caller that
    /// writes [`to_bytes`](Self::to_bytes) itself (a failed write leaves
    /// it changed).
    pub fn set_changed(&mut self, changed: bool) {
        self.changed = changed;
    }

    /// Changes the size limit, dropping entries when it is now exceeded.
    pub fn set_max_bytes(&mut self, max_bytes: u64) {
        self.max_bytes = max_bytes;
        self.make_room();
    }

    /// The hashes known for `key`, which counts as a use.
    pub fn get(&mut self, key: &FileKey) -> Option<Cached> {
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        self.clock = self.clock.saturating_add(1);
        // A use alone is not worth writing the file again.
        Some(entry.hashes)
    }

    /// Adds the hashes in `hashes` to those known for `key`.
    pub fn put(&mut self, key: FileKey, hashes: &Cached) {
        if self.max_bytes == 0 {
            return;
        }
        let used = self.clock;
        self.clock = self.clock.saturating_add(1);
        let entry = self.entries.entry(key).or_insert(Entry {
            hashes: Cached::default(),
            used,
        });
        let before = if entry.hashes == Cached::default() {
            0
        } else {
            entry.hashes.bytes()
        };
        entry.hashes.merge(hashes);
        entry.used = used;
        self.bytes = self.bytes - before + entry.hashes.bytes();
        self.changed = true;
        self.make_room();
    }

    /// Forgets every entry.
    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.changed = true;
        }
        self.entries = HashMap::new();
        self.bytes = 0;
    }

    /// Drops the entries used longest ago while the limit is exceeded,
    /// down to nine tenths of it, so this does not run on every addition.
    fn make_room(&mut self) {
        if self.bytes <= self.max_bytes {
            return;
        }
        let goal = self.max_bytes / 10 * 9;
        let mut by_use: Vec<(u64, FileKey)> =
            self.entries.iter().map(|(k, e)| (e.used, *k)).collect();
        by_use.sort_unstable();
        for (_, key) in by_use {
            if self.bytes <= goal {
                break;
            }
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.hashes.bytes();
            }
        }
        self.changed = true;
    }

    /// The cache as the bytes of its file.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.bytes as usize);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (key, entry) in &self.entries {
            let h = &entry.hashes;
            for n in [key.dev, key.ino, key.size] {
                out.extend_from_slice(&n.to_le_bytes());
            }
            out.extend_from_slice(&key.mtime_ns.to_le_bytes());
            out.extend_from_slice(&entry.used.to_le_bytes());
            let flags = u8::from(h.ends.is_some())
                | u8::from(h.full.is_some()) << 1
                | u8::from(h.md5.is_some()) << 2
                | u8::from(h.sha256.is_some()) << 3
                | u8::from(h.crc32.is_some()) << 4
                | u8::from(h.sha1.is_some()) << 5
                | u8::from(h.sha384.is_some()) << 6
                | u8::from(h.sha512.is_some()) << 7;
            out.push(flags);
            if let Some(v) = h.ends {
                out.extend_from_slice(&v.to_le_bytes());
            }
            if let Some(v) = h.full {
                out.extend_from_slice(&v.to_le_bytes());
            }
            if let Some(v) = h.md5 {
                out.extend_from_slice(&v);
            }
            if let Some(v) = h.sha256 {
                out.extend_from_slice(&v);
            }
            if let Some(v) = h.crc32 {
                out.extend_from_slice(&v.to_le_bytes());
            }
            if let Some(v) = h.sha1 {
                out.extend_from_slice(&v);
            }
            if let Some(v) = h.sha384 {
                out.extend_from_slice(&v);
            }
            if let Some(v) = h.sha512 {
                out.extend_from_slice(&v);
            }
        }
        out
    }

    /// Reads a cache from the bytes of its file, keeping at most
    /// `max_bytes` of it (the entries used last).
    pub fn from_bytes(bytes: &[u8], max_bytes: u64) -> io::Result<Self> {
        Self::read_from(bytes, bytes.len() as u64, max_bytes)
    }

    /// Parses one entry at a time and evicts by last use as it goes. Neither
    /// the input buffer nor retained tables grow with the old file's size.
    fn read_from(mut input: impl Read, length: u64, max_bytes: u64) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "not a hash cache file");
        let mut header = [0; 16];
        input.read_exact(&mut header)?;
        if &header[..8] != MAGIC {
            return Err(bad());
        }
        let count = u64::from_le_bytes(header[8..].try_into().map_err(|_| bad())?);
        if count > length.saturating_sub(16) / ENTRY_BASE {
            return Err(bad());
        }
        let mut cache = Self::new(max_bytes);
        let mut by_use = BTreeSet::new();
        let mut bytes = [0; 257];
        for _ in 0..count {
            input.read_exact(&mut bytes[..ENTRY_BASE as usize])?;
            let flags = bytes[ENTRY_BASE as usize - 1];
            let sizes = [16, 16, 16, 32, 4, 20, 48, 64];
            let extra: usize = sizes
                .iter()
                .enumerate()
                .filter(|(bit, _)| flags & (1 << bit) != 0)
                .map(|(_, size)| size)
                .sum();
            let size = ENTRY_BASE as usize + extra;
            input.read_exact(&mut bytes[ENTRY_BASE as usize..size])?;
            let (key, entry) = Reader(&bytes[..size]).entry().ok_or_else(bad)?;
            cache.clock = cache.clock.max(entry.used.saturating_add(1));
            if let Some(old) = cache.entries.remove(&key) {
                cache.bytes -= old.hashes.bytes();
                by_use.remove(&(old.used, key));
            }
            if entry.hashes.bytes() <= max_bytes {
                cache.bytes += entry.hashes.bytes();
                by_use.insert((entry.used, key));
                cache.entries.insert(key, entry);
            }
            while cache.bytes > max_bytes {
                let (_, key) = by_use.pop_first().expect("entries exceed limit");
                if let Some(old) = cache.entries.remove(&key) {
                    cache.bytes -= old.hashes.bytes();
                }
            }
        }
        if input.read(&mut [0; 1])? != 0 {
            return Err(bad());
        }
        Ok(cache)
    }

    /// Loads with bounded buffers and retained entries. Disabled caching
    /// bypasses the file altogether; a missing file is an empty cache.
    pub fn load(path: &Path, max_bytes: u64) -> io::Result<Self> {
        if max_bytes == 0 {
            return Ok(Self::new(0));
        }
        match std::fs::File::open(path) {
            Ok(file) => {
                let length = file.metadata()?.len();
                Self::read_from(io::BufReader::new(file), length, max_bytes)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::new(max_bytes)),
            Err(e) => Err(e),
        }
    }

    /// Writes the cache to `path` (see the module notes); an empty cache
    /// removes the file.
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        if self.entries.is_empty() {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        } else {
            crate::persist::write_atomically(path, &self.to_bytes())?;
        }
        self.changed = false;
        Ok(())
    }
}

/// Default location of the cache file, next to the window's index file:
/// `$XDG_CACHE_HOME/everygrep/hashes.bin`.
pub fn default_path() -> Option<PathBuf> {
    Some(crate::persist::default_path()?.with_file_name("hashes.bin"))
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    fn entry(&mut self) -> Option<(FileKey, Entry)> {
        let key = FileKey {
            dev: self.u64()?,
            ino: self.u64()?,
            size: self.u64()?,
            mtime_ns: self.array().map(i64::from_le_bytes)?,
        };
        let used = self.u64()?;
        let flags = self.take(1)?[0];
        let has = |bit: u8| flags >> bit & 1 == 1;
        let mut hashes = Cached::default();
        if has(0) {
            hashes.ends = Some(u128::from_le_bytes(self.array()?));
        }
        if has(1) {
            hashes.full = Some(u128::from_le_bytes(self.array()?));
        }
        if has(2) {
            hashes.md5 = Some(self.array()?);
        }
        if has(3) {
            hashes.sha256 = Some(self.array()?);
        }
        if has(4) {
            hashes.crc32 = Some(u32::from_le_bytes(self.array()?));
        }
        if has(5) {
            hashes.sha1 = Some(self.array()?);
        }
        if has(6) {
            hashes.sha384 = Some(self.array()?);
        }
        if has(7) {
            hashes.sha512 = Some(self.array()?);
        }
        Some((key, Entry { hashes, used }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ino: u64) -> FileKey {
        FileKey {
            dev: 7,
            ino,
            size: 100 + ino,
            mtime_ns: 1_700_000_000_000_000_000 + ino as i64,
        }
    }

    fn ends(v: u128) -> Cached {
        Cached {
            ends: Some(v),
            ..Cached::default()
        }
    }

    #[test]
    fn a_small_load_limit_bounds_the_retained_table_allocation() {
        let mut large = HashCache::new(u64::MAX);
        for i in 0..10_000 {
            large.put(key(i), &ends(i.into()));
        }
        large.get(&key(0));
        let limit = (ENTRY_BASE + 16) * 10;
        let mut loaded = HashCache::from_bytes(&large.to_bytes(), limit).unwrap();
        assert!(
            loaded.get(&key(0)).is_some(),
            "the most recent use was lost"
        );
        assert!(loaded.bytes() <= limit);
        assert!(
            loaded.entries.capacity() <= 32,
            "retained capacity {} for {} entries",
            loaded.entries.capacity(),
            loaded.len()
        );
    }

    #[test]
    fn a_disabled_cache_load_does_not_read_or_parse_the_old_file() {
        let dir =
            std::env::temp_dir().join(format!("everygrep-cache-disabled-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hashes.bin");
        std::fs::write(&path, b"old invalid cache").unwrap();
        let loaded = HashCache::load(&path, 0);
        let _ = std::fs::remove_dir_all(dir);
        assert!(loaded.unwrap().is_empty());
    }

    #[test]
    fn hashes_are_found_by_key_and_merged() {
        let mut cache = HashCache::default();
        assert_eq!(cache.get(&key(1)), None);
        cache.put(key(1), &ends(5));
        cache.put(
            key(1),
            &Cached {
                md5: Some([1; 16]),
                crc32: Some(9),
                ..Cached::default()
            },
        );
        let got = cache.get(&key(1)).unwrap();
        assert_eq!(
            (got.ends, got.md5, got.crc32, got.full),
            (Some(5), Some([1; 16]), Some(9), None)
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), ENTRY_BASE + 16 + 16 + 4);
        // Another size, time or inode is another file.
        let mut changed = key(1);
        changed.mtime_ns += 1;
        assert_eq!(cache.get(&changed), None);
        changed = key(1);
        changed.size += 1;
        assert_eq!(cache.get(&changed), None);
        changed = key(1);
        changed.dev += 1;
        assert_eq!(cache.get(&changed), None);
    }

    #[test]
    fn the_entries_used_longest_ago_go_first_when_the_limit_is_reached() {
        let one = ENTRY_BASE + 16;
        let mut cache = HashCache::new(one * 10);
        for i in 0..10 {
            cache.put(key(i), &ends(i.into()));
        }
        assert_eq!(cache.len(), 10);
        // A use keeps entry 0.
        cache.get(&key(0)).unwrap();
        cache.put(key(10), &ends(10));
        // Over the limit: down to nine tenths (9 entries).
        assert_eq!(cache.len(), 9);
        assert!(cache.bytes() <= one * 9);
        assert!(cache.get(&key(0)).is_some());
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(2)).is_none());
        assert!(cache.get(&key(10)).is_some());
        cache.set_max_bytes(one * 2);
        assert!(cache.len() <= 2);
        assert!(cache.get(&key(10)).is_some());
        // 0 keeps nothing.
        cache.set_max_bytes(0);
        assert!(cache.is_empty());
        cache.put(key(3), &ends(3));
        assert!(cache.is_empty());
    }

    #[test]
    fn the_file_keeps_every_hash_and_the_order_of_use() {
        let mut cache = HashCache::default();
        let all = Cached {
            ends: Some(u128::MAX - 3),
            full: Some(1 << 100),
            md5: Some([0xab; 16]),
            sha256: Some([0x5c; 32]),
            crc32: Some(0xCBF4_3926),
            ..Cached::default()
        };
        cache.put(key(1), &all);
        cache.put(key(2), &ends(2));
        cache.put(key(3), &ends(3));
        cache.get(&key(1)).unwrap();
        let bytes = cache.to_bytes();
        assert_eq!(bytes.len() as u64, 16 + cache.bytes());
        let mut back = HashCache::from_bytes(&bytes, DEFAULT_MAX_BYTES).unwrap();
        assert!(!back.is_changed());
        assert_eq!(back.len(), 3);
        assert_eq!(back.bytes(), cache.bytes());
        assert_eq!(back.get(&key(1)), Some(all));
        assert_eq!(back.get(&key(2)), Some(ends(2)));
        // A smaller limit keeps what was used last: 1, then 3.
        let small = HashCache::from_bytes(&bytes, ENTRY_BASE + 124).unwrap();
        assert_eq!(small.len(), 1);
        let mut small = HashCache::from_bytes(&bytes, 210).unwrap();
        assert_eq!(small.len(), 2);
        assert!(small.get(&key(1)).is_some() && small.get(&key(3)).is_some());
    }

    #[test]
    fn extended_hashes_roundtrip_and_count_towards_the_limit() {
        let mut cache = HashCache::default();
        let all = Cached {
            ends: Some(1),
            full: Some(2),
            md5: Some([3; 16]),
            sha256: Some([4; 32]),
            crc32: Some(5),
            sha1: Some([6; 20]),
            sha384: Some([7; 48]),
            sha512: Some([8; 64]),
        };
        cache.put(key(1), &all);
        assert_eq!(cache.bytes(), 257);
        let bytes = cache.to_bytes();
        let mut back = HashCache::from_bytes(&bytes, 257).unwrap();
        assert_eq!(back.get(&key(1)), Some(all));
        assert!(HashCache::from_bytes(&bytes, 256).unwrap().is_empty());
        for cut in 16..bytes.len() {
            assert!(HashCache::from_bytes(&bytes[..cut], 300).is_err());
        }
        back.set_max_bytes(256);
        assert!(back.is_empty());
    }

    #[test]
    fn a_damaged_file_is_refused_without_a_large_allocation() {
        let mut cache = HashCache::default();
        cache.put(key(1), &ends(1));
        let bytes = cache.to_bytes();
        for cut in [0, 7, 8, 15, 16, bytes.len() - 1] {
            assert!(HashCache::from_bytes(&bytes[..cut], DEFAULT_MAX_BYTES).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(HashCache::from_bytes(&extra, DEFAULT_MAX_BYTES).is_err());
        let mut huge = bytes.clone();
        huge[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(HashCache::from_bytes(&huge, DEFAULT_MAX_BYTES).is_err());
        let mut flags = bytes;
        flags[16 + 40] = 0x80;
        assert!(HashCache::from_bytes(&flags, DEFAULT_MAX_BYTES).is_err());
        assert!(HashCache::from_bytes(b"EGHASH02\0\0\0\0\0\0\0\0", DEFAULT_MAX_BYTES).is_err());
    }

    #[test]
    fn save_and_load_go_through_a_file_and_clear_removes_it() {
        let dir = std::env::temp_dir().join(format!("everygrep-hashcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("hashes.bin");
        let mut fresh = HashCache::load(&path, DEFAULT_MAX_BYTES).unwrap();
        assert!(fresh.is_empty());
        fresh.put(key(4), &ends(4));
        assert!(fresh.is_changed());
        fresh.save(&path).unwrap();
        assert!(!fresh.is_changed());
        let mut loaded = HashCache::load(&path, DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(loaded.get(&key(4)), Some(ends(4)));
        loaded.clear();
        assert!(loaded.is_changed());
        loaded.save(&path).unwrap();
        assert!(!path.exists());
        // Saving an empty cache again with no file is fine.
        loaded.save(&path).unwrap();
        std::fs::write(&path, b"junk").unwrap();
        assert!(HashCache::load(&path, DEFAULT_MAX_BYTES).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
