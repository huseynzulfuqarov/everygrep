//! A sorted sequence of node ids that supports cheap inserts and removals.
//!
//! The ids are kept in a list of sorted blocks. Finding a position is a binary
//! search over the blocks followed by one inside a block; inserting or removing
//! only shifts the ids of one block. Blocks are split when they grow too large and
//! dropped when they become empty.

use std::cmp::Ordering;

use crate::store::NodeId;

/// Block size used when building from an already sorted list.
const BUILD_BLOCK: usize = 512;
/// A block that grows beyond this size is split in two.
const MAX_BLOCK: usize = 1024;

/// Position of an id inside a [`SortedIds`]: block number and offset in the block.
pub type Position = (usize, usize);

/// Node ids in sorted order, split into blocks.
#[derive(Default)]
pub struct SortedIds {
    blocks: Vec<Vec<NodeId>>,
    len: usize,
}

impl SortedIds {
    /// Builds from ids that are already in order.
    pub fn from_sorted(ids: Vec<NodeId>) -> Self {
        let len = ids.len();
        let blocks = ids.chunks(BUILD_BLOCK).map(<[NodeId]>::to_vec).collect();
        Self { blocks, len }
    }

    /// Number of ids.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no ids.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The blocks in order; each block is sorted.
    pub fn blocks(&self) -> &[Vec<NodeId>] {
        &self.blocks
    }

    /// All ids in order.
    pub fn iter(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.blocks.iter().flatten().copied()
    }

    /// Finds an id for which `cmp` returns `Equal`, or the position where such an
    /// id would be inserted. `cmp` compares a stored id with the wanted key.
    pub fn search_by(&self, mut cmp: impl FnMut(NodeId) -> Ordering) -> Result<Position, Position> {
        let Some(last_block) = self.blocks.len().checked_sub(1) else {
            return Err((0, 0));
        };
        let b = self.blocks.partition_point(|block| {
            cmp(*block.last().expect("blocks are never empty")) == Ordering::Less
        });
        if b > last_block {
            return Err((last_block, self.blocks[last_block].len()));
        }
        match self.blocks[b].binary_search_by(|&id| cmp(id)) {
            Ok(i) => Ok((b, i)),
            Err(i) => Err((b, i)),
        }
    }

    /// Inserts `id` at a position returned by [`search_by`](Self::search_by).
    pub fn insert_at(&mut self, (b, i): Position, id: NodeId) {
        if self.blocks.is_empty() {
            self.blocks.push(vec![id]);
        } else {
            let block = &mut self.blocks[b];
            block.insert(i, id);
            if block.len() > MAX_BLOCK {
                let tail = block.split_off(block.len() / 2);
                self.blocks.insert(b + 1, tail);
            }
        }
        self.len += 1;
    }

    /// Removes and returns the id at `pos`.
    pub fn remove_at(&mut self, (b, i): Position) -> NodeId {
        let id = self.blocks[b].remove(i);
        if self.blocks[b].is_empty() {
            self.blocks.remove(b);
        }
        self.len -= 1;
        id
    }

    /// Approximate heap memory used, in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.blocks.capacity() * std::mem::size_of::<Vec<NodeId>>()
            + self
                .blocks
                .iter()
                .map(|b| b.capacity() * std::mem::size_of::<NodeId>())
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(s: &mut SortedIds, v: u32) {
        let pos = match s.search_by(|id| id.0.cmp(&v)) {
            Ok(p) | Err(p) => p,
        };
        s.insert_at(pos, NodeId(v));
    }

    #[test]
    fn stays_sorted_through_inserts_splits_and_removals() {
        let mut s = SortedIds::default();
        // Pseudo-random insertion order covering block splits.
        let values: Vec<u32> = (0..5000u32)
            .map(|i| i.wrapping_mul(2_654_435_761) % 100_000)
            .collect();
        for &v in &values {
            insert(&mut s, v);
        }
        assert!(s.blocks().len() > 5);
        let mut expected = values.clone();
        expected.sort_unstable();
        assert_eq!(s.iter().map(|id| id.0).collect::<Vec<_>>(), expected);

        for &v in values.iter().step_by(2) {
            let pos = s.search_by(|id| id.0.cmp(&v)).expect("present");
            assert_eq!(s.remove_at(pos), NodeId(v));
        }
        let mut rest: Vec<u32> = values.iter().copied().skip(1).step_by(2).collect();
        rest.sort_unstable();
        assert_eq!(s.iter().map(|id| id.0).collect::<Vec<_>>(), rest);
        assert_eq!(s.len(), rest.len());
        assert!(s.blocks().iter().all(|b| !b.is_empty()));
    }

    #[test]
    fn missing_key_gives_insert_position() {
        let s = SortedIds::from_sorted(vec![NodeId(1), NodeId(3)]);
        assert_eq!(s.search_by(|id| id.0.cmp(&2)), Err((0, 1)));
        assert_eq!(s.search_by(|id| id.0.cmp(&9)), Err((0, 2)));
        assert_eq!(
            SortedIds::default().search_by(|id| id.0.cmp(&9)),
            Err((0, 0))
        );
    }
}
