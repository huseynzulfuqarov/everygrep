//! Saving an [`Index`] to disk and loading it back.
//!
//! File layout (all integers little-endian):
//!
//! ```text
//! magic    8 bytes  "EGRPIDX\0"
//! version  u32
//! nodes    u64      number of nodes
//! names    u64      bytes in the raw name pool
//! roots    u32      number of root paths, then for each: u32 length + bytes
//! rules    u32 known flag + u64 rules fingerprint (version 6+)
//! columns  parent_kind[u32], name_start[u32], name_len[u16], size[u64],
//!          mtime_ns[i64], name pool, name order[u32]
//! checksum u64      over all preceding bytes
//! ```
//!
//! The size column holds each folder's total as it was when saved, but folder
//! totals and item counts are computed again on load (a few milliseconds for
//! 600 000 nodes) and the saved values are not used, so files written before
//! folder sizes existed (with 0 for folders) load unchanged.
//!
//! Files are written to a temporary name, synced and then renamed, so a crash
//! leaves either the old or the new file, never a half-written one.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::rules::RuleSpec;
use crate::store::{Columns, Meta, NodeId, NodeStore};

const MAGIC: &[u8; 8] = b"EGRPIDX\0";
const VERSION: u32 = 6;

/// The oldest version that still loads. Versions 4 and 5 omit the rules
/// fingerprint; older name orders are sorted again on load.
const OLDEST_VERSION: u32 = 4;

/// An index loaded from disk together with the roots it was built from.
pub struct Saved {
    /// The loaded index.
    pub index: Index,
    /// Folders that were scanned to build the index.
    pub roots: Vec<PathBuf>,
    /// Fingerprint of the indexing rules, or unknown for older files and
    /// callers of `save` that do not supply their rules.
    pub rules_hash: Option<u64>,
    /// The saved name order was not in the current order (a file of an older
    /// version) and was sorted again; saving the index stores the new order,
    /// so the next load does not have to sort it.
    pub reordered: bool,
}

/// Default location of the index file: `$XDG_CACHE_HOME/everygrep/index.bin`,
/// falling back to `~/.cache/everygrep/index.bin`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("everygrep").join("index.bin"))
}

/// Writes `index` and its `roots` to `path` atomically.
pub fn save(index: &Index, roots: &[PathBuf], path: &Path) -> io::Result<()> {
    save_impl(index, roots, None, path)
}

/// Saves an index with the rules used to build it, so callers can reject a
/// cache made with different include/exclude lists.
pub fn save_with_rules(
    index: &Index,
    roots: &[PathBuf],
    rules: &RuleSpec,
    path: &Path,
) -> io::Result<()> {
    save_impl(index, roots, Some(rules_fingerprint(rules)), path)
}

/// Stable cache identity of all rule fields, including raw folder path bytes.
/// This is a cache fingerprint, not a cryptographic integrity check.
pub fn rules_fingerprint(rules: &RuleSpec) -> u64 {
    let mut buf = Vec::new();
    put_u32(&mut buf, u32::from(rules.enabled));
    put_u64(&mut buf, rules.folders.len() as u64);
    for bytes in rules
        .folders
        .iter()
        .map(|p| p.as_os_str().as_bytes())
        .chain([
            rules.include_files.as_bytes(),
            rules.exclude_files.as_bytes(),
        ])
    {
        put_u64(&mut buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }
    checksum(&buf)
}

fn save_impl(
    index: &Index,
    roots: &[PathBuf],
    rules_hash: Option<u64>,
    path: &Path,
) -> io::Result<()> {
    let compacted;
    let index = if index.needs_compaction() {
        compacted = index.compacted().0;
        &compacted
    } else {
        index
    };
    let c = index.store().columns();
    let mut buf = Vec::with_capacity(index.heap_bytes() + 64);
    buf.extend_from_slice(MAGIC);
    put_u32(&mut buf, VERSION);
    put_u64(&mut buf, c.parent_kind.len() as u64);
    put_u64(&mut buf, c.name_pool.len() as u64);
    put_u32(
        &mut buf,
        u32::try_from(roots.len()).map_err(|_| invalid("too many roots"))?,
    );
    for root in roots {
        let bytes = root.as_os_str().as_bytes();
        put_u32(
            &mut buf,
            u32::try_from(bytes.len()).map_err(|_| invalid("root too long"))?,
        );
        buf.extend_from_slice(bytes);
    }
    put_u32(&mut buf, u32::from(rules_hash.is_some()));
    put_u64(&mut buf, rules_hash.unwrap_or_default());
    c.parent_kind.iter().for_each(|&v| put_u32(&mut buf, v));
    c.name_start.iter().for_each(|&v| put_u32(&mut buf, v));
    c.name_len
        .iter()
        .for_each(|&v| buf.extend_from_slice(&v.to_le_bytes()));
    c.meta.iter().for_each(|m| put_u64(&mut buf, m.size));
    c.meta
        .iter()
        .for_each(|m| buf.extend_from_slice(&m.mtime_ns.to_le_bytes()));
    buf.extend_from_slice(c.name_pool);
    index.order().iter().for_each(|id| put_u32(&mut buf, id.0));
    let sum = checksum(&buf);
    put_u64(&mut buf, sum);

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = TempFile::create(path)?;
    tmp.file.write_all(&buf)?;
    tmp.file.sync_all()?;
    fs::rename(&tmp.path, path)?;
    tmp.renamed = true;
    Ok(())
}

/// Owns only an exclusively created file; all failure paths remove it.
struct TempFile {
    path: PathBuf,
    file: fs::File,
    renamed: bool,
}

impl TempFile {
    fn create(destination: &Path) -> io::Result<Self> {
        let mut random = fs::File::open("/dev/urandom")?;
        for _ in 0..128 {
            let mut suffix = [0u8; 16];
            random.read_exact(&mut suffix)?;
            let mut name = destination.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".tmp-{:032x}", u128::from_ne_bytes(suffix)));
            let path = destination.with_file_name(name);
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        renamed: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique index temporary file",
        ))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Rewrites the index file at `path` as `version`, with the name order
/// `order` (one id per node) and a fresh checksum. Tests use it to make files
/// of an older version, or with an order that is not sorted.
#[doc(hidden)]
pub fn rewrite_order(path: &Path, version: u32, order: &[NodeId]) -> io::Result<()> {
    let mut data = fs::read(path)?;
    if data.len() < 12 + 8 + 4 * order.len() {
        return Err(invalid("file too short"));
    }
    let current = u32::from_le_bytes(data[8..12].try_into().expect("4 bytes"));
    if (current >= 6) != (version >= 6) {
        let read_u32 = |at: usize| -> io::Result<usize> {
            let bytes = data
                .get(at..at + 4)
                .ok_or_else(|| invalid("file too short"))?;
            Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")) as usize)
        };
        let count = read_u32(28)?;
        let mut at = 32usize;
        for _ in 0..count {
            at = at
                .checked_add(4 + read_u32(at)?)
                .ok_or_else(|| invalid("root too long"))?;
        }
        if current >= 6 {
            if at.checked_add(12).is_none_or(|end| end > data.len() - 8) {
                return Err(invalid("file too short"));
            }
            data.drain(at..at + 12);
        } else {
            if at > data.len() - 8 {
                return Err(invalid("file too short"));
            }
            data.splice(at..at, [0; 12]);
        }
    }
    data[8..12].copy_from_slice(&version.to_le_bytes());
    let body = data.len() - 8;
    let start = body - 4 * order.len();
    for (slot, id) in data[start..body].chunks_exact_mut(4).zip(order) {
        slot.copy_from_slice(&id.0.to_le_bytes());
    }
    let sum = checksum(&data[..body]);
    data[body..].copy_from_slice(&sum.to_le_bytes());
    fs::write(path, &data)
}

/// Reads an index file written by [`save`].
///
/// The file is decoded while it is read, so memory use stays close to the size
/// of the index itself. The sizes in the header are checked against the file
/// size before anything is allocated.
///
/// A name order saved by an older version, or sorted by older rules, is
/// sorted again (see [`Saved::reordered`]).
///
/// Returns an error of kind `InvalidData` if the file is damaged, from an
/// unsupported version, or inconsistent in any way.
pub fn load(path: &Path) -> io::Result<Saved> {
    let file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let body_len = file_len
        .checked_sub(8)
        .filter(|&l| l >= (MAGIC.len() + 4 + 8 * 2 + 4) as u64)
        .ok_or_else(|| invalid("not an index file"))?;
    let mut r = StreamReader {
        inner: io::BufReader::with_capacity(1 << 20, file),
        left: body_len,
        hasher: Hasher::new(body_len),
        scratch: Vec::new(),
    };
    if r.bytes(MAGIC.len())? != MAGIC {
        return Err(invalid("not an index file"));
    }
    let version = r.u32()?;
    if !(OLDEST_VERSION..=VERSION).contains(&version) {
        return Err(invalid("unsupported version"));
    }
    let nodes = r.len()?;
    let names = r.len()?;
    let root_count = r.u32()? as usize;
    let mut roots = Vec::new();
    for _ in 0..root_count {
        let len = r.u32()? as usize;
        roots.push(PathBuf::from(std::ffi::OsStr::from_bytes(&r.bytes(len)?)));
    }
    let rules_hash = if version >= 6 {
        let known = r.u32()?;
        r.fill(8)?;
        let hash = u64::from_le_bytes(r.scratch[..8].try_into().expect("8 bytes"));
        match known {
            0 => None,
            1 => Some(hash),
            _ => return Err(invalid("invalid rules fingerprint flag")),
        }
    } else {
        None
    };
    // 4+4+2+8+8 bytes of columns and 4 of name order per node.
    let expected = (nodes as u64)
        .checked_mul(30)
        .and_then(|v| v.checked_add(names as u64));
    if expected != Some(r.left) {
        return Err(invalid("sizes in header do not match file size"));
    }

    let parent_kind = r.vec(nodes, u32::from_le_bytes)?;
    let name_start = r.vec(nodes, u32::from_le_bytes)?;
    let name_len = r.vec(nodes, u16::from_le_bytes)?;
    let mut meta: Vec<Meta> = r
        .vec(nodes, u64::from_le_bytes)?
        .into_iter()
        .map(|size| Meta { size, mtime_ns: 0 })
        .collect();
    r.each(nodes, i64::from_le_bytes, |i, t| meta[i].mtime_ns = t)?;
    let name_pool = r.bytes(names)?;
    let by_name = r.vec(nodes, |b: [u8; 4]| NodeId(u32::from_le_bytes(b)))?;

    let mut tail = [0u8; 8];
    r.inner.read_exact(&mut tail)?;
    if r.hasher.finish() != u64::from_le_bytes(tail) {
        return Err(invalid("checksum mismatch"));
    }
    let store = NodeStore::from_columns(Columns {
        parent_kind,
        name_start,
        name_len,
        meta,
        name_pool,
    })
    .map_err(invalid)?;
    let (mut index, reordered) = Index::from_parts(store, by_name).map_err(invalid)?;
    // A root that was absent during scanning has no node. Give catch-up a
    // directory to revisit when it returns, including in older saved indexes.
    let indexed_roots: std::collections::HashSet<PathBuf> = index
        .roots()
        .into_iter()
        .map(|id| index.store().path(id))
        .collect();
    for root in &roots {
        if !indexed_roots.contains(root) {
            index.insert(None, root.as_os_str().as_bytes(), true, Meta::default());
        }
    }
    Ok(Saved {
        index,
        roots,
        rules_hash,
        reordered,
    })
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Word-at-a-time 64-bit hash, used only to detect damaged files.
fn checksum(data: &[u8]) -> u64 {
    let mut h = Hasher::new(data.len() as u64);
    h.update(data);
    h.finish()
}

/// Incremental form of [`checksum`]: feeding the same bytes in any number of
/// pieces gives the same result.
struct Hasher {
    h: u64,
    pending: [u8; 8],
    pending_len: usize,
}

impl Hasher {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;

    fn new(total_len: u64) -> Self {
        Self {
            h: 0x5EED ^ total_len,
            pending: [0; 8],
            pending_len: 0,
        }
    }

    fn mix(&mut self, v: u64) {
        self.h = (self.h ^ v).wrapping_mul(Self::K).rotate_left(29);
    }

    fn update(&mut self, mut data: &[u8]) {
        if self.pending_len > 0 {
            let take = (8 - self.pending_len).min(data.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&data[..take]);
            self.pending_len += take;
            data = &data[take..];
            if self.pending_len < 8 {
                return;
            }
            self.mix(u64::from_le_bytes(self.pending));
            self.pending_len = 0;
        }
        let mut words = data.chunks_exact(8);
        for w in &mut words {
            self.mix(u64::from_le_bytes(w.try_into().expect("8 bytes")));
        }
        let rest = words.remainder();
        self.pending[..rest.len()].copy_from_slice(rest);
        self.pending_len = rest.len();
    }

    fn finish(mut self) -> u64 {
        for i in 0..self.pending_len {
            let b = self.pending[i];
            self.mix(u64::from(b));
        }
        self.h ^ (self.h >> 31)
    }
}

/// Reads the body of an index file, hashing every byte it hands out and never
/// reading past the body.
struct StreamReader {
    inner: io::BufReader<fs::File>,
    left: u64,
    hasher: Hasher,
    scratch: Vec<u8>,
}

impl StreamReader {
    fn fill(&mut self, n: usize) -> io::Result<()> {
        if (n as u64) > self.left {
            return Err(invalid("file too short"));
        }
        self.scratch.resize(n, 0);
        self.inner.read_exact(&mut self.scratch)?;
        self.hasher.update(&self.scratch);
        self.left -= n as u64;
        Ok(())
    }

    fn bytes(&mut self, n: usize) -> io::Result<Vec<u8>> {
        if (n as u64) > self.left {
            return Err(invalid("file too short"));
        }
        let mut out = vec![0u8; n];
        self.inner.read_exact(&mut out)?;
        self.hasher.update(&out);
        self.left -= n as u64;
        Ok(out)
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.fill(4)?;
        Ok(u32::from_le_bytes(
            self.scratch[..4].try_into().expect("4 bytes"),
        ))
    }

    fn len(&mut self) -> io::Result<usize> {
        self.fill(8)?;
        let v = u64::from_le_bytes(self.scratch[..8].try_into().expect("8 bytes"));
        usize::try_from(v).map_err(|_| invalid("length too large"))
    }

    /// Decodes `n` fixed-size values, passing each to `f` with its index.
    fn each<const W: usize, T>(
        &mut self,
        n: usize,
        decode: fn([u8; W]) -> T,
        mut f: impl FnMut(usize, T),
    ) -> io::Result<()> {
        const CHUNK: usize = 1 << 16;
        let mut i = 0;
        while i < n {
            let count = (n - i).min(CHUNK);
            self.fill(count * W)?;
            for c in self.scratch.chunks_exact(W) {
                f(i, decode(c.try_into().expect("chunk of W bytes")));
                i += 1;
            }
        }
        Ok(())
    }

    fn vec<const W: usize, T>(&mut self, n: usize, decode: fn([u8; W]) -> T) -> io::Result<Vec<T>> {
        let mut out = Vec::with_capacity(n);
        self.each(n, decode, |_, v| out.push(v))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Query, QueryOptions};
    use crate::search::search;

    fn sample() -> Index {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/data", true, Meta::default());
        let docs = s.add(
            Some(root),
            b"Docs",
            true,
            Meta {
                size: 0,
                mtime_ns: -5,
            },
        );
        s.add(
            Some(docs),
            b"Report.PDF",
            false,
            Meta {
                size: 42,
                mtime_ns: 1_700_000_000,
            },
        );
        s.add(
            Some(docs),
            b"notes.txt",
            false,
            Meta {
                size: 7,
                mtime_ns: 3,
            },
        );
        Index::new(s)
    }

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "everygrep-persist-{tag}-{}.bin",
            std::process::id()
        ))
    }

    #[test]
    fn failed_rename_removes_the_temporary_index() {
        let dir = temp_file("failed-rename");
        fs::create_dir_all(dir.join("index.bin")).unwrap();
        fs::write(dir.join("index.bin/keep"), b"keep").unwrap();
        assert!(save(&sample(), &[], &dir.join("index.bin")).is_err());
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("index.bin")]);
        assert_eq!(fs::read(dir.join("index.bin/keep")).unwrap(), b"keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_preexisting_temporary_symlink_is_never_followed() {
        let dir = temp_file("temp-symlink");
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("index.bin");
        let victim = dir.join("keep");
        fs::write(&victim, b"keep").unwrap();
        let old_tmp = dir.join(format!("index.bin.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &old_tmp).unwrap();
        save(&sample(), &[], &destination).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(old_tmp.is_symlink());
        assert!(load(&destination).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn round_trip_keeps_index_searchable() {
        let path = temp_file("roundtrip");
        let index = sample();
        save(&index, &[PathBuf::from("/data")], &path).unwrap();
        let saved = load(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(saved.roots, [PathBuf::from("/data")]);
        let loaded = saved.index;
        assert_eq!(loaded.len(), index.len());
        assert!(loaded.order().iter().eq(index.order().iter()));
        let m = Query::parse("pdf", QueryOptions::default()).unwrap();
        let hits = search(&loaded, &m, 2);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            loaded.store().path(hits[0]),
            PathBuf::from("/data/Docs/Report.PDF")
        );
        assert_eq!(
            loaded.store().meta(hits[0]),
            Meta {
                size: 42,
                mtime_ns: 1_700_000_000
            }
        );
        let docs = loaded.store().parent(hits[0]).unwrap();
        assert_eq!(loaded.store().meta(docs).mtime_ns, -5);
    }

    #[test]
    fn rules_fingerprint_round_trips_and_legacy_files_have_unknown_rules() {
        let path = temp_file("rules");
        let index = sample();
        let rules = RuleSpec {
            folders: vec!["cache".into()],
            include_files: "*.txt".into(),
            exclude_files: "old*".into(),
            ..RuleSpec::default()
        };
        save_with_rules(&index, &["/data".into()], &rules, &path).unwrap();
        assert_eq!(
            load(&path).unwrap().rules_hash,
            Some(rules_fingerprint(&rules))
        );
        let order: Vec<_> = index.order().iter().collect();
        for version in [4, 5] {
            save_with_rules(&index, &["/data".into()], &rules, &path).unwrap();
            rewrite_order(&path, version, &order).unwrap();
            assert_eq!(load(&path).unwrap().rules_hash, None);
        }
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn saving_after_changes_writes_a_compacted_index() {
        let path = temp_file("compact");
        let mut index = sample();
        let pdf = index
            .order()
            .iter()
            .find(|&id| index.store().name(id) == b"Report.PDF")
            .unwrap();
        let docs = index.store().parent(pdf).unwrap();
        index.remove(pdf);
        index.insert(Some(docs), b"added.md", false, Meta::default());
        assert!(index.needs_compaction());
        save(&index, &[], &path).unwrap();
        let loaded = load(&path).unwrap().index;
        fs::remove_file(&path).unwrap();
        assert_eq!(loaded.len(), index.len());
        assert_eq!(loaded.store().len(), loaded.len());
        let names: Vec<&[u8]> = loaded
            .order()
            .iter()
            .map(|id| loaded.store().name(id))
            .collect();
        assert_eq!(names, [&b"/data"[..], b"added.md", b"Docs", b"notes.txt"]);
    }

    /// Saves `index`, then rewrites the file as `version` with the name order
    /// `order` and a fresh checksum.
    fn save_with_order(index: &Index, path: &Path, version: u32, order: &[NodeId]) {
        save(index, &[PathBuf::from("/data")], path).unwrap();
        rewrite_order(path, version, order).unwrap();
    }

    fn natural_sample() -> Index {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/data", true, Meta::default());
        for name in ["file10", "file2", "Zoo.txt", "Çay.txt", "a-b", "ab", "Əli"] {
            s.add(Some(root), name.as_bytes(), false, Meta::default());
        }
        Index::new(s)
    }

    #[test]
    fn version_4_file_in_the_old_order_loads_sorted_again() {
        let path = temp_file("v4");
        let index = natural_sample();
        let store = index.store();
        // Version 4 sorted by folded bytes, then raw bytes.
        let mut old: Vec<NodeId> = index.order().iter().collect();
        old.sort_by(|&a, &b| {
            (store.folded_name(a), store.name(a)).cmp(&(store.folded_name(b), store.name(b)))
        });
        assert!(!old.iter().copied().eq(index.order().iter()));
        save_with_order(&index, &path, 4, &old);
        let saved = load(&path).unwrap();
        assert!(saved.reordered);
        assert!(saved.index.order().iter().eq(index.order().iter()));
        // Saved again, it is the current version and loads without sorting.
        save(&saved.index, &saved.roots, &path).unwrap();
        assert_eq!(fs::read(&path).unwrap()[8..12], VERSION.to_le_bytes());
        let again = load(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!again.reordered);
        assert!(again.index.order().iter().eq(index.order().iter()));
    }

    #[test]
    fn unsorted_order_is_sorted_and_other_versions_are_rejected() {
        let path = temp_file("order");
        let index = natural_sample();
        let mut backwards: Vec<NodeId> = index.order().iter().collect();
        backwards.reverse();
        save_with_order(&index, &path, 5, &backwards);
        let saved = load(&path).unwrap();
        assert!(saved.reordered);
        assert!(saved.index.order().iter().eq(index.order().iter()));

        let mut twice = backwards.clone();
        twice[0] = twice[1];
        save_with_order(&index, &path, 5, &twice);
        let err = load(&path).err().expect("not a permutation");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        for version in [3, 7] {
            save_with_order(&index, &path, version, &backwards);
            let err = load(&path).err().expect("unsupported version");
            assert_eq!(err.to_string(), "unsupported version");
        }
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn incremental_hash_matches_one_shot_hash() {
        let data: Vec<u8> = (0..1000u32)
            .map(|i| (i.wrapping_mul(31) >> 3) as u8)
            .collect();
        for split in [0, 1, 3, 7, 8, 9, 500, 999, 1000] {
            let mut h = Hasher::new(data.len() as u64);
            h.update(&data[..split]);
            for piece in data[split..].chunks(5) {
                h.update(piece);
            }
            assert_eq!(h.finish(), checksum(&data), "split at {split}");
        }
    }

    #[test]
    fn header_claiming_huge_sizes_is_rejected_before_allocating() {
        let path = temp_file("huge");
        save(&sample(), &[], &path).unwrap();
        let mut data = fs::read(&path).unwrap();
        // The node count follows the 8-byte magic and the 4-byte version.
        data[12..20].copy_from_slice(&(u64::MAX / 64).to_le_bytes());
        fs::write(&path, &data).unwrap();
        let err = load(&path).err().expect("must not load");
        fs::remove_file(&path).unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn damaged_file_is_rejected() {
        let path = temp_file("damaged");
        save(&sample(), &[], &path).unwrap();
        let mut data = fs::read(&path).unwrap();
        let mid = data.len() / 2;
        data[mid] ^= 0x40;
        fs::write(&path, &data).unwrap();
        let err = load(&path).err().expect("damaged file must not load");
        fs::remove_file(&path).unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_file_is_rejected() {
        let path = temp_file("short");
        save(&sample(), &[], &path).unwrap();
        let data = fs::read(&path).unwrap();
        fs::write(&path, &data[..data.len() / 3]).unwrap();
        let err = load(&path).err().expect("truncated file must not load");
        fs::remove_file(&path).unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A tree of about `n` entries shaped like a home folder: folders hold up
    /// to 19 files and up to 3 subfolders, at most 12 levels deep. Nodes are
    /// added breadth first, so parents come before their children.
    fn synthetic_tree(n: usize) -> NodeStore {
        let mut s = NodeStore::new();
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let root = s.add(None, b"/home/me", true, Meta::default());
        let mut queue = std::collections::VecDeque::from([(root, 0)]);
        let mut count = 1;
        while count < n {
            let (dir, depth) = queue.pop_front().unwrap_or((root, 0));
            for f in 0..next() % 20 {
                let h = next();
                let meta = Meta {
                    size: h % (1 << (h % 28)),
                    mtime_ns: (h >> 20) as i64,
                };
                s.add(
                    Some(dir),
                    format!("file-{count}-{f}-{h:x}.dat").as_bytes(),
                    false,
                    meta,
                );
                count += 1;
            }
            let subdirs = if depth < 12 { next() % 4 } else { 0 };
            for d in 0..subdirs {
                let id = s.add(
                    Some(dir),
                    format!("dir-{count}-{d}").as_bytes(),
                    true,
                    Meta::default(),
                );
                queue.push_back((id, depth + 1));
                count += 1;
            }
        }
        s
    }

    /// Prints build, save and load times and memory for a synthetic index of
    /// 617 000 entries. Run with `cargo test --release -p everygrep-core
    /// load_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing only"]
    fn load_timing() {
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3;
        let mut builds = Vec::new();
        let mut store = NodeStore::new();
        for _ in 0..5 {
            let started = std::time::Instant::now();
            store = synthetic_tree(617_000);
            builds.push(ms(started));
        }
        let index = Index::new(store);
        let path = temp_file("timing");
        save(&index, &[], &path).unwrap();
        let mut loads = Vec::new();
        for _ in 0..7 {
            let started = std::time::Instant::now();
            let loaded = load(&path).unwrap();
            loads.push(ms(started));
            assert_eq!(loaded.index.len(), index.len());
        }
        // A folder created last with an early file moved into it: folder
        // sizes are then computed without relying on id order.
        let heap = index.heap_bytes();
        let mut moved = index;
        let first = moved.order().iter().find(|&id| !moved.store().is_dir(id));
        let root = NodeId(0);
        let late = moved.insert(Some(root), b"late", true, Meta::default());
        moved.rename(first.unwrap(), Some(late), b"moved").unwrap();
        save(&moved, &[], &path).unwrap();
        let mut moved_loads = Vec::new();
        for _ in 0..7 {
            let started = std::time::Instant::now();
            load(&path).unwrap();
            moved_loads.push(ms(started));
        }
        fs::remove_file(&path).unwrap();
        builds.sort_by(f64::total_cmp);
        loads.sort_by(f64::total_cmp);
        moved_loads.sort_by(f64::total_cmp);
        eprintln!(
            "{} nodes: build median {:.1} ms, load median {:.1} ms (min {:.1}), \
             load after a move {:.1} ms, heap {:.1} MiB",
            moved.len(),
            builds[builds.len() / 2],
            loads[loads.len() / 2],
            loads[0],
            moved_loads[moved_loads.len() / 2],
            heap as f64 / (1 << 20) as f64
        );
    }
}
