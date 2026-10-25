//! Compact storage for file and folder nodes.
//!
//! Nodes live in parallel columns indexed by [`NodeId`]. Names are stored once in a
//! shared byte pool, together with a folded copy used for matching, so a search
//! never has to allocate or fold per comparison.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::fold::fold_into;

/// Identifier of a node inside a [`NodeStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    /// Index of the node in the store columns.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Basic metadata kept for every node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// Size in bytes; for folders this is 0 until folder sizes are computed.
    pub size: u64,
    /// Modification time in seconds since the Unix epoch.
    pub mtime: i64,
}

/// Marks a folder in the packed parent column.
const DIR_BIT: u32 = 1 << 31;
/// Marks a folded name that is identical to the raw name and therefore not stored.
const SAME_AS_RAW: u32 = u32::MAX;

/// Column storage of all nodes.
///
/// Per node it keeps a packed parent/folder word, the position and length of the
/// raw name, the position and length of the folded name (shared with the raw name
/// when folding changes nothing) and the metadata.
#[derive(Default)]
pub struct NodeStore {
    /// Bit 31: folder flag. Bits 0..31: parent id + 1, or 0 for a root.
    parent_kind: Vec<u32>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    folded_start: Vec<u32>,
    folded_len: Vec<u16>,
    meta: Vec<Meta>,
    name_pool: Vec<u8>,
    folded_pool: Vec<u8>,
    scratch: Vec<u8>,
}

impl NodeStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.parent_kind.len()
    }

    /// Whether the store has no nodes.
    pub fn is_empty(&self) -> bool {
        self.parent_kind.is_empty()
    }

    /// Adds a node and returns its id.
    ///
    /// A root node has no parent and stores its full path as its name.
    ///
    /// # Panics
    ///
    /// Panics if the store would exceed 2^31 - 1 nodes, 4 GiB of names, or a name
    /// longer than 65 535 bytes.
    pub fn add(&mut self, parent: Option<NodeId>, name: &[u8], is_dir: bool, meta: Meta) -> NodeId {
        let id = u32::try_from(self.len())
            .ok()
            .filter(|&i| i < DIR_BIT - 1)
            .expect("too many nodes");
        let parent_word = parent.map_or(0, |p| p.0 + 1) | if is_dir { DIR_BIT } else { 0 };

        let (name_start, name_len) = push(&mut self.name_pool, name);
        self.scratch.clear();
        fold_into(name, &mut self.scratch);
        let (folded_start, folded_len) = if self.scratch == name {
            (SAME_AS_RAW, name_len)
        } else {
            push(&mut self.folded_pool, &self.scratch)
        };

        self.parent_kind.push(parent_word);
        self.name_start.push(name_start);
        self.name_len.push(name_len);
        self.folded_start.push(folded_start);
        self.folded_len.push(folded_len);
        self.meta.push(meta);
        NodeId(id)
    }

    /// Releases spare capacity after a bulk build.
    pub fn shrink_to_fit(&mut self) {
        self.parent_kind.shrink_to_fit();
        self.name_start.shrink_to_fit();
        self.name_len.shrink_to_fit();
        self.folded_start.shrink_to_fit();
        self.folded_len.shrink_to_fit();
        self.meta.shrink_to_fit();
        self.name_pool.shrink_to_fit();
        self.folded_pool.shrink_to_fit();
        self.scratch = Vec::new();
    }

    /// Parent of a node, `None` for roots.
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        match self.parent_kind[id.index()] & !DIR_BIT {
            0 => None,
            p => Some(NodeId(p - 1)),
        }
    }

    /// Whether the node is a folder.
    pub fn is_dir(&self, id: NodeId) -> bool {
        self.parent_kind[id.index()] & DIR_BIT != 0
    }

    /// Raw name bytes of a node.
    pub fn name(&self, id: NodeId) -> &[u8] {
        let i = id.index();
        slice(&self.name_pool, self.name_start[i], self.name_len[i])
    }

    /// Folded name bytes of a node, used for case-insensitive matching.
    pub fn folded_name(&self, id: NodeId) -> &[u8] {
        let i = id.index();
        match self.folded_start[i] {
            SAME_AS_RAW => self.name(id),
            start => slice(&self.folded_pool, start, self.folded_len[i]),
        }
    }

    /// Metadata of a node.
    pub fn meta(&self, id: NodeId) -> Meta {
        self.meta[id.index()]
    }

    /// Full path of a node, built by walking up to its root.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts = vec![id];
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            parts.push(p);
            cur = p;
        }
        let mut path = PathBuf::new();
        for part in parts.iter().rev() {
            path.push(OsStr::from_bytes(self.name(*part)));
        }
        path
    }

    /// Approximate heap memory used by the store, in bytes.
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        (self.parent_kind.capacity() + self.name_start.capacity() + self.folded_start.capacity())
            * size_of::<u32>()
            + (self.name_len.capacity() + self.folded_len.capacity()) * size_of::<u16>()
            + self.meta.capacity() * size_of::<Meta>()
            + self.name_pool.capacity()
            + self.folded_pool.capacity()
            + self.scratch.capacity()
    }
}

fn push(pool: &mut Vec<u8>, bytes: &[u8]) -> (u32, u16) {
    let start = u32::try_from(pool.len()).expect("name pool exceeds 4 GiB");
    let len = u16::try_from(bytes.len()).expect("name longer than 65535 bytes");
    pool.extend_from_slice(bytes);
    (start, len)
}

fn slice(pool: &[u8], start: u32, len: u16) -> &[u8] {
    let start = start as usize;
    &pool[start..start + len as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (NodeStore, NodeId) {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/data", true, Meta::default());
        let docs = s.add(Some(root), b"Docs", true, Meta::default());
        let file = s.add(
            Some(docs),
            b"Report.PDF",
            false,
            Meta { size: 42, mtime: 7 },
        );
        (s, file)
    }

    #[test]
    fn stores_names_and_metadata() {
        let (s, file) = sample();
        assert_eq!(s.len(), 3);
        assert_eq!(s.name(file), b"Report.PDF");
        assert_eq!(s.folded_name(file), b"report.pdf");
        assert_eq!(s.meta(file), Meta { size: 42, mtime: 7 });
        assert!(!s.is_dir(file));
    }

    #[test]
    fn builds_full_path() {
        let (s, file) = sample();
        assert_eq!(s.path(file), PathBuf::from("/data/Docs/Report.PDF"));
    }

    #[test]
    fn folded_copy_is_shared_when_unchanged() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/", true, Meta::default());
        let lower = s.add(Some(root), b"readme.txt", false, Meta::default());
        let mixed = s.add(Some(root), b"ReadMe.txt", false, Meta::default());
        assert_eq!(s.folded_name(lower), b"readme.txt");
        assert_eq!(s.folded_name(mixed), b"readme.txt");
        assert_eq!(s.folded_pool.len(), b"readme.txt".len());
        assert_eq!(s.parent(lower), Some(root));
        assert!(s.is_dir(root) && !s.is_dir(lower));
    }

    #[test]
    fn keeps_non_utf8_names() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/", true, Meta::default());
        let odd = s.add(Some(root), b"bad\xffname", false, Meta::default());
        assert_eq!(s.name(odd), b"bad\xffname");
        assert_eq!(s.path(odd).as_os_str().as_bytes(), b"/bad\xffname");
    }
}
