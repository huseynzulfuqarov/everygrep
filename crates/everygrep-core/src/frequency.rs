//! How often the name, the extension and the size of each entry occur in
//! the whole index (the Same name, Same extension and Same size columns).
//!
//! - Name: entries of the same kind (file or folder) with the same name,
//!   ignoring case (the folded name; diacritics count).
//! - Extension: files with the same extension, ignoring case; files without
//!   one count together. Folders have none (0).
//! - Size: files of the same size; folders with the same total size, among
//!   folders.
//!
//! The counts cover every entry of the index, not just the results of a
//! query, so `report.pdf` shows how many `report.pdf` there are anywhere.
//! [`Frequencies::build`] walks the whole index once; [`Index::frequencies`]
//! keeps the last ones built until the index changes.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::index::Index;
use crate::sort::extension;
use crate::store::NodeId;

/// The three counts of every node of one index, by node id (0 for removed
/// nodes and for an extension of a folder).
#[derive(Debug)]
pub struct Frequencies {
    stamp: u64,
    version: u64,
    name: Vec<u32>,
    ext: Vec<u32>,
    size: Vec<u32>,
}

impl Frequencies {
    /// Counts the names, extensions and sizes of every live node of `index`.
    pub fn build(index: &Index) -> Self {
        let store = index.store();
        let n = store.len();
        let ids = || index.order().iter();
        let mut names: HashMap<(bool, &[u8]), u32, Fnv> = HashMap::default();
        let mut exts: HashMap<&[u8], u32, Fnv> = HashMap::default();
        let mut sizes: HashMap<(bool, u64), u32, Fnv> = HashMap::default();
        for id in ids() {
            let is_dir = store.is_dir(id);
            *names.entry((is_dir, store.folded_name(id))).or_default() += 1;
            if !is_dir {
                *exts.entry(extension(store, id)).or_default() += 1;
            }
            *sizes.entry((is_dir, store.meta(id).size)).or_default() += 1;
        }
        let (mut name, mut ext, mut size) = (vec![0; n], vec![0; n], vec![0; n]);
        for id in ids() {
            let is_dir = store.is_dir(id);
            let i = id.index();
            name[i] = names[&(is_dir, store.folded_name(id))];
            if !is_dir {
                ext[i] = exts[extension(store, id)];
            }
            size[i] = sizes[&(is_dir, store.meta(id).size)];
        }
        Self {
            stamp: index.stamp(),
            version: index.version(),
            name,
            ext,
            size,
        }
    }

    /// Whether these are the counts of `index` as it is now.
    pub fn is_for(&self, index: &Index) -> bool {
        self.stamp == index.stamp() && self.version == index.version()
    }

    pub(crate) fn stamp(&self) -> u64 {
        self.stamp
    }

    /// Number of node ids covered.
    pub fn len(&self) -> usize {
        self.name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// Entries of the same kind with the same name.
    pub fn name(&self, id: NodeId) -> u32 {
        self.name.get(id.index()).copied().unwrap_or(0)
    }

    /// Files with the same extension; 0 for a folder.
    pub fn extension(&self, id: NodeId) -> u32 {
        self.ext.get(id.index()).copied().unwrap_or(0)
    }

    /// Files (or folders) of the same size.
    pub fn size(&self, id: NodeId) -> u32 {
        self.size.get(id.index()).copied().unwrap_or(0)
    }
}

/// FNV-1a: much faster than the default hasher for the short keys here, and
/// the keys come from file names on this computer, not from a network.
#[derive(Clone, Copy)]
struct FnvHasher(u64);

impl Default for FnvHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for FnvHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type Fnv = BuildHasherDefault<FnvHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Meta, NodeStore};
    use std::sync::Arc;

    fn meta(size: u64) -> Meta {
        Meta {
            size,
            ..Meta::default()
        }
    }

    #[test]
    fn names_extensions_and_sizes_are_counted_over_the_whole_index() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        let a = s.add(Some(root), b"a", true, Meta::default());
        let b = s.add(Some(root), b"b", true, Meta::default());
        let r1 = s.add(Some(a), b"Report.PDF", false, meta(10));
        let r2 = s.add(Some(b), b"report.pdf", false, meta(10));
        let n = s.add(Some(b), b"notes.txt", false, meta(3));
        let x = s.add(Some(root), b"Makefile", false, meta(10));
        // A folder named like a file is counted apart.
        let d = s.add(Some(a), b"notes.txt", true, Meta::default());
        let mut index = Index::new(s);
        let f = index.frequencies();
        assert!(f.is_for(&index));
        assert_eq!((f.name(r1), f.name(r2), f.name(n), f.name(d)), (2, 2, 1, 1));
        assert_eq!((f.extension(r1), f.extension(n), f.extension(x)), (2, 1, 1));
        assert_eq!(f.extension(d), 0);
        assert_eq!((f.size(r1), f.size(x), f.size(n)), (3, 3, 1));
        // The same counts come back until the index changes.
        assert!(Arc::ptr_eq(&f, &index.frequencies()));
        let more = index.insert(Some(root), b"REPORT.pdf", false, meta(3));
        assert!(!f.is_for(&index));
        assert_eq!(index.last_frequencies().unwrap().name(r1), 2);
        let g = index.frequencies();
        assert_eq!(
            (g.name(r1), g.name(more), g.size(n), g.extension(r2)),
            (3, 3, 2, 3)
        );
        index.remove(r2);
        let h = index.frequencies();
        assert_eq!((h.name(r1), h.name(r2)), (2, 0));
        index.rename(r1, Some(a), b"other.pdf").unwrap();
        assert_eq!(index.frequencies().name(r1), 1);
        index.set_meta(n, meta(10));
        assert_eq!(index.frequencies().size(x), 3);
    }

    #[test]
    fn results_sort_by_a_frequency_with_ties_in_name_order() {
        use crate::sort::{Sort, SortKey, sort};
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        let a = s.add(Some(root), b"a", true, Meta::default());
        for (parent, name, size) in [
            (root, &b"x.jpg"[..], 1),
            (a, b"x.jpg", 2),
            (a, b"y.jpg", 2),
            (root, b"z.txt", 2),
        ] {
            s.add(Some(parent), name, false, meta(size));
        }
        let index = Index::new(s);
        let files: Vec<NodeId> = index
            .order()
            .iter()
            .filter(|&id| !index.store().is_dir(id))
            .collect();
        let names = |key, descending| {
            let mut ids = files.clone();
            sort(
                &index,
                &mut ids,
                Sort {
                    key,
                    descending,
                    ..Sort::default()
                },
                2,
            );
            ids.iter()
                .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(SortKey::NameFrequency, true),
            ["x.jpg", "x.jpg", "y.jpg", "z.txt"]
        );
        assert_eq!(
            names(SortKey::ExtensionFrequency, false),
            ["z.txt", "x.jpg", "x.jpg", "y.jpg"]
        );
        assert_eq!(names(SortKey::SizeFrequency, true)[3], "x.jpg");
    }
}
