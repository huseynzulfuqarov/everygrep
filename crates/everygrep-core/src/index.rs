//! The searchable index: node storage plus a name-sorted order of all nodes.

use crate::store::{NodeId, NodeStore};

/// Node storage together with an order of node ids sorted by name.
///
/// Searching walks this order, so results come out already sorted by name
/// without a separate sort step.
pub struct Index {
    store: NodeStore,
    by_name: Vec<NodeId>,
}

impl Index {
    /// Builds the index and its name order from a filled store.
    pub fn new(mut store: NodeStore) -> Self {
        store.shrink_to_fit();
        let mut by_name: Vec<NodeId> = (0..store.len())
            .map(|i| NodeId(u32::try_from(i).expect("store ids fit in u32")))
            .collect();
        by_name.sort_unstable_by(|&a, &b| {
            store
                .folded_name(a)
                .cmp(store.folded_name(b))
                .then_with(|| store.name(a).cmp(store.name(b)))
                .then_with(|| a.cmp(&b))
        });
        Self { store, by_name }
    }

    /// The underlying node storage.
    pub fn store(&self) -> &NodeStore {
        &self.store
    }

    /// All node ids sorted by name.
    pub fn by_name(&self) -> &[NodeId] {
        &self.by_name
    }

    /// Number of indexed nodes.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Approximate heap memory used by the index, in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.store.heap_bytes() + self.by_name.capacity() * std::mem::size_of::<NodeId>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Meta;

    #[test]
    fn order_is_case_insensitive_by_name() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        for name in [&b"beta"[..], b"Alpha", b"alpha", b"Gamma"] {
            s.add(Some(root), name, false, Meta::default());
        }
        let index = Index::new(s);
        let names: Vec<&[u8]> = index
            .by_name()
            .iter()
            .map(|&id| index.store().name(id))
            .collect();
        assert_eq!(names, [&b"/r"[..], b"Alpha", b"alpha", b"beta", b"Gamma"]);
    }
}
