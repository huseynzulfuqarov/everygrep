//! Saving an [`Index`] to disk and loading it back.
//!
//! File layout (all integers little-endian):
//!
//! ```text
//! magic    8 bytes  "EGRPIDX\0"
//! version  u32
//! nodes    u64      number of nodes
//! names    u64      bytes in the raw name pool
//! folded   u64      bytes in the folded name pool
//! roots    u32      number of root paths, then for each: u32 length + bytes
//! columns  parent_kind[u32], name_start[u32], name_len[u16], folded_start[u32],
//!          folded_len[u16], size[u64], mtime_ns[i64], name pool, folded pool,
//!          name order[u32]
//! checksum u64      over all preceding bytes
//! ```
//!
//! Files are written to a temporary name, synced and then renamed, so a crash
//! leaves either the old or the new file, never a half-written one.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::store::{Columns, Meta, NodeId, NodeStore};

const MAGIC: &[u8; 8] = b"EGRPIDX\0";
const VERSION: u32 = 3;

/// An index loaded from disk together with the roots it was built from.
pub struct Saved {
    /// The loaded index.
    pub index: Index,
    /// Folders that were scanned to build the index.
    pub roots: Vec<PathBuf>,
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
    put_u64(&mut buf, c.folded_pool.len() as u64);
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
    c.parent_kind.iter().for_each(|&v| put_u32(&mut buf, v));
    c.name_start.iter().for_each(|&v| put_u32(&mut buf, v));
    c.name_len
        .iter()
        .for_each(|&v| buf.extend_from_slice(&v.to_le_bytes()));
    c.folded_start.iter().for_each(|&v| put_u32(&mut buf, v));
    c.folded_len
        .iter()
        .for_each(|&v| buf.extend_from_slice(&v.to_le_bytes()));
    c.meta.iter().for_each(|m| put_u64(&mut buf, m.size));
    c.meta
        .iter()
        .for_each(|m| buf.extend_from_slice(&m.mtime_ns.to_le_bytes()));
    buf.extend_from_slice(c.name_pool);
    buf.extend_from_slice(c.folded_pool);
    index.order().iter().for_each(|id| put_u32(&mut buf, id.0));
    let sum = checksum(&buf);
    put_u64(&mut buf, sum);

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Reads an index file written by [`save`].
///
/// The file is decoded while it is read, so memory use stays close to the size
/// of the index itself. The sizes in the header are checked against the file
/// size before anything is allocated.
///
/// Returns an error of kind `InvalidData` if the file is damaged, from another
/// version, or inconsistent in any way.
pub fn load(path: &Path) -> io::Result<Saved> {
    let file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let body_len = file_len
        .checked_sub(8)
        .filter(|&l| l >= (MAGIC.len() + 4 + 8 * 3 + 4) as u64)
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
    if r.u32()? != VERSION {
        return Err(invalid("unsupported version"));
    }
    let nodes = r.len()?;
    let names = r.len()?;
    let folded = r.len()?;
    let root_count = r.u32()? as usize;
    let mut roots = Vec::new();
    for _ in 0..root_count {
        let len = r.u32()? as usize;
        roots.push(PathBuf::from(std::ffi::OsStr::from_bytes(&r.bytes(len)?)));
    }
    // 4+4+2+4+2+8+8 bytes of columns and 4 of name order per node.
    let expected = (nodes as u64)
        .checked_mul(36)
        .and_then(|v| v.checked_add(names as u64))
        .and_then(|v| v.checked_add(folded as u64));
    if expected != Some(r.left) {
        return Err(invalid("sizes in header do not match file size"));
    }

    let parent_kind = r.vec(nodes, u32::from_le_bytes)?;
    let name_start = r.vec(nodes, u32::from_le_bytes)?;
    let name_len = r.vec(nodes, u16::from_le_bytes)?;
    let folded_start = r.vec(nodes, u32::from_le_bytes)?;
    let folded_len = r.vec(nodes, u16::from_le_bytes)?;
    let mut meta: Vec<Meta> = r
        .vec(nodes, u64::from_le_bytes)?
        .into_iter()
        .map(|size| Meta { size, mtime_ns: 0 })
        .collect();
    r.each(nodes, i64::from_le_bytes, |i, t| meta[i].mtime_ns = t)?;
    let name_pool = r.bytes(names)?;
    let folded_pool = r.bytes(folded)?;
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
        folded_start,
        folded_len,
        meta,
        name_pool,
        folded_pool,
    })
    .map_err(invalid)?;
    let index = Index::from_parts(store, by_name).map_err(invalid)?;
    Ok(Saved { index, roots })
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
    use crate::matcher::{MatchOptions, Matcher};
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
        let m = Matcher::new("pdf", MatchOptions::default());
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
}
