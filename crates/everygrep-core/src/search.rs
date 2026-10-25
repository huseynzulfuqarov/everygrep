//! Parallel search over the name order of an [`Index`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::index::Index;
use crate::query::{EvalScratch, Query};
use crate::store::NodeId;

/// Number of name-order blocks handed to a worker at a time.
const BLOCKS_PER_TASK: usize = 16;

/// Returns the ids of all live nodes whose name matches, sorted by name.
///
/// The name order is split into groups of blocks that worker threads take one by
/// one, so fast and slow groups balance out. Group results are joined in group
/// order, which keeps the name order of the index.
pub fn search(index: &Index, query: &Query, threads: usize) -> Vec<NodeId> {
    let order = index.order();
    if query.matches_all() {
        return order.iter().collect();
    }
    let groups: Vec<&[Vec<NodeId>]> = order.blocks().chunks(BLOCKS_PER_TASK).collect();
    let threads = threads.clamp(1, groups.len().max(1));
    if threads == 1 {
        return groups.iter().flat_map(|g| scan(index, query, g)).collect();
    }

    let next = AtomicUsize::new(0);
    let parts: Mutex<Vec<(usize, Vec<NodeId>)>> = Mutex::new(Vec::with_capacity(groups.len()));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let Some(group) = groups.get(n) else { break };
                    local.push((n, scan(index, query, group)));
                }
                parts.lock().expect("search worker panicked").extend(local);
            });
        }
    });

    let mut parts = parts.into_inner().expect("search worker panicked");
    parts.sort_unstable_by_key(|(n, _)| *n);
    let total = parts.iter().map(|(_, v)| v.len()).sum();
    let mut out = Vec::with_capacity(total);
    for (_, part) in parts {
        out.extend(part);
    }
    out
}

fn scan(index: &Index, query: &Query, blocks: &[Vec<NodeId>]) -> Vec<NodeId> {
    let store = index.store();
    let mut scratch = EvalScratch::default();
    blocks
        .iter()
        .flatten()
        .copied()
        .filter(|&id| query.matches(store, id, &mut scratch))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryOptions;
    use crate::store::{Meta, NodeStore};

    fn big_index(n: usize) -> Index {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        for i in 0..n {
            let name = format!(
                "file-{i:06}-{}.txt",
                if i % 7 == 0 { "Match" } else { "other" }
            );
            s.add(Some(root), name.as_bytes(), false, Meta::default());
        }
        Index::new(s)
    }

    #[test]
    fn parallel_and_serial_results_are_equal_and_sorted() {
        let index = big_index(50_000);
        let m = Query::parse("match", QueryOptions::default()).unwrap();
        let serial = search(&index, &m, 1);
        let parallel = search(&index, &m, 8);
        assert_eq!(serial.len(), 50_000_usize.div_ceil(7));
        assert_eq!(serial, parallel);
        let names: Vec<&[u8]> = parallel
            .iter()
            .map(|&id| index.store().folded_name(id))
            .collect();
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn empty_query_returns_all_nodes() {
        let index = big_index(10);
        let m = Query::parse("", QueryOptions::default()).unwrap();
        assert_eq!(search(&index, &m, 4).len(), 11);
    }
}
