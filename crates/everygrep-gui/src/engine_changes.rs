//! Whether file changes seen by the watcher can change the results shown.
//!
//! A busy folder (a home folder with browser caches, say) changes many
//! times a second. Most changes are entries the search does not list, or a
//! new time or size the search and its order do not look at. Those need no
//! new search: the window only draws its rows again, which shows the new
//! sizes and times of the entries it lists. Any change that can add an
//! entry, take one away or move one in the order searches again, in full;
//! the results are never patched in place.

use std::collections::HashSet;
use std::sync::Arc;

use everygrep_core::index::Index;
use everygrep_core::query::EvalScratch;
use everygrep_core::sort::{Sort, SortKey};
use everygrep_core::store::NodeId;
use everygrep_core::watch::{TOUCHED_MAX, Touched};

use super::QueryState;

/// The complete results last handed to the window: the request they answer
/// and the epoch of their ids.
#[derive(Clone)]
pub(super) struct Shown {
    pub generation: u64,
    pub epoch: u64,
    pub ids: Arc<Vec<NodeId>>,
}

/// What an order looks at besides names and paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortReads {
    /// Names, paths and types only.
    Names,
    /// The size, a time, attributes or contents of each entry.
    Details,
    /// Counts over the whole index (entries with the same name, extension
    /// or size), which any added or removed entry changes.
    WholeIndex,
}

fn sort_reads(sort: &Sort) -> SortReads {
    let mut reads = SortReads::Names;
    for (key, _) in sort.keys() {
        match key {
            SortKey::Name | SortKey::Path | SortKey::Extension | SortKey::Type => {}
            SortKey::NameFrequency | SortKey::ExtensionFrequency | SortKey::SizeFrequency => {
                return SortReads::WholeIndex;
            }
            _ => reads = SortReads::Details,
        }
    }
    reads
}

/// Whether the changes in `touched`, already applied to `index`, can change
/// what a search for `state` lists or the order it lists it in. `shown` are
/// the ids that search listed before them, when known. When in doubt the
/// answer is yes.
pub(super) fn search_again(
    index: &Index,
    state: &QueryState,
    shown: Option<&[NodeId]>,
    touched: &Touched,
) -> bool {
    if touched.is_empty() {
        return false;
    }
    if touched.many {
        return true;
    }
    let query = match state.parse() {
        Ok(Some(query)) => query,
        // Nothing is listed: an empty search that lists nothing, or a
        // query with an error.
        Ok(None) | Err(_) => return false,
    };
    // Contents, duplicates and relatives are not cheap to test one entry
    // at a time.
    if query.reads_files() || query.dupes().is_some() || query.reads_relatives() {
        return true;
    }
    let order = sort_reads(&state.sort);
    if order == SortReads::WholeIndex {
        return true;
    }
    let details = query.reads_metadata();
    let order_details = order == SortReads::Details;
    // Permissions of a folder can change what is read below it.
    if touched.subtree && (details || order_details) {
        return true;
    }
    let store = index.store();
    let live = |id: NodeId| id.index() < store.len() && store.is_live(id);
    let mut scratch = EvalScratch::with_offline_roots(index.offline_roots());
    let mut accepts = |id: NodeId| live(id) && query.matches(store, id, &mut scratch);

    // A new entry, or a new name, that the query accepts is a new row or a
    // row at a new place.
    let mut stack = touched.added.clone();
    let mut looked = 0;
    while let Some(id) = stack.pop() {
        looked += 1;
        if looked > 4 * TOUCHED_MAX {
            return true;
        }
        if accepts(id) {
            return true;
        }
        if live(id) && store.is_dir(id) {
            stack.extend(store.children(id));
        }
    }
    if touched.renamed.iter().any(|&id| accepts(id)) {
        return true;
    }

    // Entries that must not be listed any more: removed, or renamed to a
    // name the query does not accept.
    let mut leave: HashSet<NodeId> = touched
        .removed
        .iter()
        .chain(&touched.renamed)
        .copied()
        .collect();
    // Entries whose details changed, with the folders above them (their
    // total size, time and item count changed too), when the query or the
    // order looks at details.
    let mut join: HashSet<NodeId> = HashSet::new();
    let mut moves: HashSet<NodeId> = HashSet::new();
    if details || order_details {
        let mut changed: HashSet<NodeId> = HashSet::new();
        for &id in touched
            .updated
            .iter()
            .chain(&touched.added)
            .chain(&touched.renamed)
        {
            let mut at = Some(id);
            while let Some(node) = at.filter(|&n| live(n)) {
                if !changed.insert(node) {
                    break;
                }
                at = store.parent(node);
            }
            if changed.len() > 4 * TOUCHED_MAX {
                return true;
            }
        }
        if details {
            for &id in &changed {
                if accepts(id) {
                    join.insert(id);
                } else {
                    leave.insert(id);
                }
            }
        }
        if order_details {
            moves = changed;
        }
    }
    if leave.is_empty() && join.is_empty() && moves.is_empty() {
        return false;
    }
    let Some(shown) = shown else {
        return true;
    };
    let mut joined = 0;
    for id in shown {
        if leave.contains(id) || moves.contains(id) {
            return true;
        }
        if join.contains(id) {
            joined += 1;
        }
    }
    // An entry the query accepts now that is not listed yet.
    joined < join.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::query::QueryOptions;
    use everygrep_core::store::{Meta, NodeStore};

    struct Tree {
        index: Index,
        docs: NodeId,
        report: NodeId,
        notes: NodeId,
        big: NodeId,
    }

    fn tree() -> Tree {
        let mut store = NodeStore::new();
        let file = |size| Meta {
            size,
            ..Meta::default()
        };
        let root = store.add(None, b"/home", true, Meta::default());
        let docs = store.add(Some(root), b"docs", true, Meta::default());
        let report = store.add(Some(docs), b"report.txt", false, file(10));
        let notes = store.add(Some(docs), b"notes.md", false, file(20));
        let big = store.add(Some(root), b"big.iso", false, file(5_000_000));
        Tree {
            index: Index::new(store),
            docs,
            report,
            notes,
            big,
        }
    }

    fn state(text: &str) -> QueryState {
        QueryState {
            text: text.into(),
            options: QueryOptions::default(),
            ..QueryState::default()
        }
    }

    fn sorted_by(text: &str, key: SortKey) -> QueryState {
        QueryState {
            sort: Sort {
                key,
                ..Sort::default()
            },
            ..state(text)
        }
    }

    fn updated(ids: &[NodeId]) -> Touched {
        Touched {
            updated: ids.to_vec(),
            ..Touched::default()
        }
    }

    #[test]
    fn a_new_entry_counts_only_when_the_query_accepts_it() {
        let mut t = tree();
        let new = t
            .index
            .insert(Some(t.docs), b"cache.tmp", false, Meta::default());
        let added = Touched {
            added: vec![new],
            updated: vec![t.docs],
            ..Touched::default()
        };
        let shown = [t.report];
        assert!(!search_again(
            &t.index,
            &state("report"),
            Some(&shown),
            &added
        ));
        assert!(search_again(&t.index, &state("tmp"), Some(&shown), &added));
        // An empty search lists all entries.
        assert!(search_again(&t.index, &state(""), Some(&shown), &added));
        // Below a new folder too.
        let dir = t.index.insert(Some(t.docs), b"new", true, Meta::default());
        t.index
            .insert(Some(dir), b"report-2.txt", false, Meta::default());
        let folder = Touched {
            added: vec![dir],
            ..Touched::default()
        };
        assert!(search_again(
            &t.index,
            &state("report"),
            Some(&shown),
            &folder
        ));
    }

    #[test]
    fn a_removed_or_renamed_entry_counts_when_it_was_listed() {
        let mut t = tree();
        let shown = [t.report];
        t.index.remove(t.notes);
        let removed = Touched {
            removed: vec![t.notes],
            ..Touched::default()
        };
        assert!(!search_again(
            &t.index,
            &state("report"),
            Some(&shown),
            &removed
        ));
        assert!(search_again(&t.index, &state("report"), None, &removed));
        t.index.remove(t.report);
        let removed = Touched {
            removed: vec![t.report],
            ..Touched::default()
        };
        assert!(search_again(
            &t.index,
            &state("report"),
            Some(&shown),
            &removed
        ));

        let t = tree();
        let renamed = Touched {
            renamed: vec![t.notes],
            ..Touched::default()
        };
        assert!(!search_again(
            &t.index,
            &state("report"),
            Some(&shown),
            &renamed
        ));
        assert!(search_again(
            &t.index,
            &state("notes"),
            Some(&[t.notes]),
            &renamed
        ));
    }

    #[test]
    fn new_details_count_only_when_the_query_or_the_order_reads_them() {
        let t = tree();
        let home = t.index.roots()[0];
        // "e": /home, report.txt and notes.md.
        let shown = [home, t.notes, t.report];
        let touched = updated(&[t.report]);
        assert!(!search_again(&t.index, &state("e"), Some(&shown), &touched));
        assert!(!search_again(
            &t.index,
            &sorted_by("e", SortKey::Extension),
            Some(&shown),
            &touched
        ));
        // Sorted by size, a listed entry (or a folder above it) may move.
        assert!(search_again(
            &t.index,
            &sorted_by("e", SortKey::Size),
            Some(&shown),
            &touched
        ));
        assert!(!search_again(
            &t.index,
            &sorted_by("iso", SortKey::Size),
            Some(&[t.big]),
            &touched
        ));
        // A size term: what matches is listed, what does not is not.
        let small = state("size:<1mb");
        let listed = [t.docs, t.report, t.notes];
        assert!(!search_again(&t.index, &small, Some(&listed), &touched));
        // A folder above that matches now and was not listed joins.
        assert!(search_again(
            &t.index,
            &small,
            Some(&[t.report, t.notes]),
            &touched
        ));
        // One listed that does not match any more leaves.
        assert!(search_again(
            &t.index,
            &state("size:>1mb"),
            Some(&[home, t.report]),
            &touched
        ));
        assert!(!search_again(
            &t.index,
            &state("size:>1mb"),
            Some(&[home, t.big]),
            &touched
        ));
    }

    #[test]
    fn some_changes_always_search_again() {
        let t = tree();
        let shown = [t.report];
        let many = Touched {
            many: true,
            ..Touched::default()
        };
        assert!(search_again(
            &t.index,
            &state("report"),
            Some(&shown),
            &many
        ));
        let touched = updated(&[t.notes]);
        assert!(search_again(
            &t.index,
            &state("parent:docs"),
            Some(&shown),
            &touched
        ));
        assert!(search_again(
            &t.index,
            &sorted_by("report", SortKey::NameFrequency),
            Some(&shown),
            &touched
        ));
        assert!(!search_again(
            &t.index,
            &state("report"),
            Some(&shown),
            &Touched::default()
        ));
    }
}
