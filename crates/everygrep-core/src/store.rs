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

/// Byte range inside one of the name pools.
#[derive(Clone, Copy, Debug)]
struct Span {
    start: u32,
    len: u32,
}

impl Span {
    fn range(self) -> std::ops::Range<usize> {
        let start = self.start as usize;
        start..start + self.len as usize
    }
}

/// Column storage of all nodes.
#[derive(Default)]
pub struct NodeStore {
    parent: Vec<Option<NodeId>>,
    is_dir: Vec<bool>,
    name: Vec<Span>,
    folded: Vec<Span>,
    meta: Vec<Meta>,
    name_pool: Vec<u8>,
    folded_pool: Vec<u8>,
}

impl NodeStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    /// Whether the store has no nodes.
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Adds a node and returns its id.
    ///
    /// A root node has no parent and stores its full path as its name.
    ///
    /// # Panics
    ///
    /// Panics if the store would exceed `u32::MAX` nodes or 4 GiB of names.
    pub fn add(&mut self, parent: Option<NodeId>, name: &[u8], is_dir: bool, meta: Meta) -> NodeId {
        let id = NodeId(u32::try_from(self.len()).expect("too many nodes"));
        let name_span = push_span(&mut self.name_pool, |pool| pool.extend_from_slice(name));
        let folded_span = push_span(&mut self.folded_pool, |pool| fold_into(name, pool));
        self.parent.push(parent);
        self.is_dir.push(is_dir);
        self.name.push(name_span);
        self.folded.push(folded_span);
        self.meta.push(meta);
        id
    }

    /// Parent of a node, `None` for roots.
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.parent[id.index()]
    }

    /// Whether the node is a folder.
    pub fn is_dir(&self, id: NodeId) -> bool {
        self.is_dir[id.index()]
    }

    /// Raw name bytes of a node.
    pub fn name(&self, id: NodeId) -> &[u8] {
        &self.name_pool[self.name[id.index()].range()]
    }

    /// Folded name bytes of a node, used for case-insensitive matching.
    pub fn folded_name(&self, id: NodeId) -> &[u8] {
        &self.folded_pool[self.folded[id.index()].range()]
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
        self.parent.capacity() * size_of::<Option<NodeId>>()
            + self.is_dir.capacity()
            + (self.name.capacity() + self.folded.capacity()) * size_of::<Span>()
            + self.meta.capacity() * size_of::<Meta>()
            + self.name_pool.capacity()
            + self.folded_pool.capacity()
    }
}

fn push_span(pool: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>)) -> Span {
    let start = pool.len();
    write(pool);
    let len = pool.len() - start;
    Span {
        start: u32::try_from(start).expect("name pool exceeds 4 GiB"),
        len: u32::try_from(len).expect("name too long"),
    }
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
    fn keeps_non_utf8_names() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/", true, Meta::default());
        let odd = s.add(Some(root), b"bad\xffname", false, Meta::default());
        assert_eq!(s.name(odd), b"bad\xffname");
        assert_eq!(s.path(odd).as_os_str().as_bytes(), b"/bad\xffname");
    }
}
