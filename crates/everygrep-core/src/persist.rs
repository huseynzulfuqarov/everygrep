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
//!          folded_len[u16], size[u64], mtime[i64], name pool, folded pool,
//!          name order[u32]
//! checksum u64      over all preceding bytes
//! ```
//!
//! Files are written to a temporary name, synced and then renamed, so a crash
//! leaves either the old or the new file, never a half-written one.

use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::store::{Columns, Meta, NodeId, NodeStore};

const MAGIC: &[u8; 8] = b"EGRPIDX\0";
const VERSION: u32 = 2;

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
        .for_each(|m| buf.extend_from_slice(&m.mtime.to_le_bytes()));
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
/// Returns an error of kind `InvalidData` if the file is damaged, from another
/// version, or inconsistent in any way.
pub fn load(path: &Path) -> io::Result<Saved> {
    let data = fs::read(path)?;
    if data.len() < MAGIC.len() + 8 || &data[..MAGIC.len()] != MAGIC {
        return Err(invalid("not an index file"));
    }
    let (body, tail) = data.split_at(data.len() - 8);
    if checksum(body) != u64::from_le_bytes(tail.try_into().expect("8 bytes")) {
        return Err(invalid("checksum mismatch"));
    }
    let mut r = Reader {
        data: body,
        pos: MAGIC.len(),
    };
    if r.u32()? != VERSION {
        return Err(invalid("unsupported version"));
    }
    let nodes = r.len()?;
    let names = r.len()?;
    let folded = r.len()?;
    let root_count = r.u32()? as usize;
    let mut roots = Vec::with_capacity(root_count.min(1024));
    for _ in 0..root_count {
        let len = r.u32()? as usize;
        roots.push(PathBuf::from(std::ffi::OsStr::from_bytes(r.bytes(len)?)));
    }
    let parent_kind = r.vec_u32(nodes)?;
    let name_start = r.vec_u32(nodes)?;
    let name_len = r.vec_u16(nodes)?;
    let folded_start = r.vec_u32(nodes)?;
    let folded_len = r.vec_u16(nodes)?;
    let sizes = r.vec_u64(nodes)?;
    let mtimes = r.vec_u64(nodes)?;
    let meta = sizes
        .into_iter()
        .zip(mtimes)
        .map(|(size, mtime)| Meta {
            size,
            mtime: mtime as i64,
        })
        .collect();
    let name_pool = r.bytes(names)?.to_vec();
    let folded_pool = r.bytes(folded)?.to_vec();
    let by_name = r.vec_u32(nodes)?.into_iter().map(NodeId).collect();
    if r.pos != body.len() {
        return Err(invalid("trailing data"));
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
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h: u64 = 0x5EED ^ data.len() as u64;
    let mut words = data.chunks_exact(8);
    for w in &mut words {
        let v = u64::from_le_bytes(w.try_into().expect("8 bytes"));
        h = (h ^ v).wrapping_mul(K).rotate_left(29);
    }
    for &b in words.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(K).rotate_left(29);
    }
    h ^ (h >> 31)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| invalid("file too short"))?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn len(&mut self) -> io::Result<usize> {
        let v = u64::from_le_bytes(self.bytes(8)?.try_into().expect("8 bytes"));
        usize::try_from(v).map_err(|_| invalid("length too large"))
    }

    fn sized(&mut self, n: usize, width: usize) -> io::Result<&'a [u8]> {
        let total = n
            .checked_mul(width)
            .ok_or_else(|| invalid("length too large"))?;
        self.bytes(total)
    }

    fn vec_u16(&mut self, n: usize) -> io::Result<Vec<u16>> {
        let raw = self.sized(n, 2)?;
        Ok(raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect())
    }

    fn vec_u32(&mut self, n: usize) -> io::Result<Vec<u32>> {
        let raw = self.sized(n, 4)?;
        Ok(raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
            .collect())
    }

    fn vec_u64(&mut self, n: usize) -> io::Result<Vec<u64>> {
        let raw = self.sized(n, 8)?;
        Ok(raw
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
            .collect())
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
        let docs = s.add(Some(root), b"Docs", true, Meta { size: 0, mtime: -5 });
        s.add(
            Some(docs),
            b"Report.PDF",
            false,
            Meta {
                size: 42,
                mtime: 1_700_000_000,
            },
        );
        s.add(Some(docs), b"notes.txt", false, Meta { size: 7, mtime: 3 });
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
                mtime: 1_700_000_000
            }
        );
        let docs = loaded.store().parent(hits[0]).unwrap();
        assert_eq!(loaded.store().meta(docs).mtime, -5);
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
