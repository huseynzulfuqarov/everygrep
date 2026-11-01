//! Sorting search results by a column.
//!
//! [`search`](crate::search::search) returns ids in name order. [`sort`] reorders
//! such a list by another key. Each key is computed once per node, then the keys
//! are sorted; equal keys keep the name order, so the result is
//! deterministic. Folders are not forced before files.
//!
//! The name order of the index breaks ties between equal names by the parent's
//! node id, which is fast to keep but means nothing to a reader. [`sort`]
//! orders such runs of equal names by their folder path instead (the order of
//! [`SortKey::Path`]), for every key, so the same name in many folders comes
//! out by folder and stays so after refreshes, inserts and renames.
//!
//! By size, folders count with what [`FolderSize`] says: their total size
//! (sorted together with files), their number of items, or nothing.
//!
//! Creation time, access time and attributes are not in the index; sorting
//! by them reads them from the file system for every result (see
//! [`crate::details`]), on several threads. Entries without the value (no
//! birth time on that file system, gone, on a drive that is not connected)
//! sort before all others, after them when descending.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::details::Details;
use crate::index::Index;
use crate::kind::{FileKind, classify};
use crate::progress::{Cancelled, Progress};
use crate::store::{NodeId, NodeStore};

/// Column to sort results by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SortKey {
    /// Name; the same name in several folders by folder path.
    #[default]
    Name,
    /// Containing folder, then name.
    Path,
    /// Size in bytes; folders as chosen by [`Sort::folders`].
    Size,
    /// Modification time.
    Modified,
    /// Folded extension; names without one sort as an empty extension.
    Extension,
    /// File kind (in [`FileKind`] order, folders first), then extension.
    Type,
    /// Creation (birth) time, read from the file system.
    Created,
    /// Access time, read from the file system.
    Accessed,
    /// Attribute bits (see [`crate::details::Attributes`]), read from the
    /// file system.
    Attributes,
}

/// What a folder's size is when sorting by [`SortKey::Size`] (and what the
/// window shows in its Size column).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FolderSize {
    /// The total size of all files below the folder, at any depth. Folders
    /// and files sort together by bytes.
    #[default]
    Total,
    /// The number of direct children. Folders sort by it among themselves,
    /// below all files (above them when descending), since a count and a size
    /// cannot be compared.
    Items,
    /// Nothing: folders count as 0 bytes, as before folder sizes existed.
    None,
}

impl FolderSize {
    /// Every choice, in the order the settings show them.
    pub const ALL: [FolderSize; 3] = [FolderSize::Total, FolderSize::Items, FolderSize::None];

    /// Name used in settings files.
    pub fn id(self) -> &'static str {
        match self {
            FolderSize::Total => "total",
            FolderSize::Items => "items",
            FolderSize::None => "none",
        }
    }

    /// The choice named `id` (see [`id`](Self::id)).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.id() == id)
    }
}

/// A sort key and a direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Sort {
    /// Column to sort by.
    pub key: SortKey,
    /// Largest first instead of smallest first.
    pub descending: bool,
    /// What a folder's size is for [`SortKey::Size`].
    pub folders: FolderSize,
}

/// Lists shorter than this are sorted on the calling thread.
const PARALLEL_MIN: usize = 64 * 1024;

/// Reorders `ids`, which must be in name order as returned by
/// [`search`](crate::search::search), by `sort`.
///
/// A descending sort reverses the key only: nodes with equal keys stay in
/// ascending name order (equal names by ascending folder path). For [`SortKey::Name`] and [`SortKey::Path`] the name is
/// part of the key, so their descending order is the exact reverse.
/// Up to `threads` threads sort large lists.
pub fn sort(index: &Index, ids: &mut [NodeId], sort: Sort, threads: usize) {
    // Never cancelled, so it always sorts.
    let _ = sort_with_progress(index, ids, sort, threads, &Progress::new());
}

/// Ids whose keys are computed between two looks at the progress.
const KEY_BATCH: usize = 16 * 1024;

/// Like [`sort`], reporting to `progress` and stopping when it is
/// cancelled. Its total is twice the number of ids: computing the keys
/// counts one per id, sorting and placing them the other. A cancelled sort
/// leaves `ids` exactly as they were.
pub fn sort_with_progress(
    index: &Index,
    ids: &mut [NodeId],
    sort: Sort,
    threads: usize,
    progress: &Progress,
) -> Result<(), Cancelled> {
    progress.set_total(Some(2 * ids.len() as u64));
    let store = index.store();
    // Path sorts by folder first, and names within one folder are unique, so
    // it needs no name order.
    let runs = if sort.key == SortKey::Path {
        Vec::new()
    } else {
        order_equal_names(store, ids, threads, progress)?
    };
    let sorted = sort_ordered(index, ids, sort, threads, progress);
    if sorted.is_err() {
        // In the input, equal names are in parent id order.
        for &(from, to) in &runs {
            ids[from..to].sort_unstable_by_key(|&id| store.parent_key(id));
        }
    }
    sorted
}

/// [`sort_with_progress`] on ids in name order whose equal names are already
/// in folder order.
fn sort_ordered(
    index: &Index,
    ids: &mut [NodeId],
    sort: Sort,
    threads: usize,
    progress: &Progress,
) -> Result<(), Cancelled> {
    let n = ids.len() as u64;
    let store = index.store();
    let desc = sort.descending;
    match sort.key {
        SortKey::Name => {
            if desc {
                ids.reverse();
            }
            progress.add(2 * n);
        }
        SortKey::Path => {
            let ranks = folder_ranks(store, ids);
            progress.check()?;
            let keys = keys_of(ids, progress, |_, id| {
                store.parent(id).map_or(0, |p| ranks[p.index()])
            })?;
            sort_by_keys(ids, threads, keys, progress)?;
            if desc {
                ids.reverse();
            }
        }
        SortKey::Size => match sort.folders {
            FolderSize::Total | FolderSize::None => {
                let none = sort.folders == FolderSize::None;
                let k = keys_of(ids, progress, |_, id| {
                    if none && store.is_dir(id) {
                        0
                    } else {
                        store.meta(id).size
                    }
                })?;
                sort_directed(ids, threads, desc, k, progress)?;
            }
            FolderSize::Items => {
                let k = keys_of(ids, progress, |_, id| {
                    if store.is_dir(id) {
                        (false, u64::from(store.items(id)))
                    } else {
                        (true, store.meta(id).size)
                    }
                })?;
                sort_directed(ids, threads, desc, k, progress)?;
            }
        },
        SortKey::Modified => {
            let k = keys_of(ids, progress, |_, id| store.meta(id).mtime_ns)?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::Extension => {
            // Comparing small ranks is much faster than comparing name bytes
            // spread over memory; there are few distinct extensions.
            let (exts, ranks) = extension_ranks(store, ids);
            let k = keys_of(ids, progress, |i, _| ranks[exts[i] as usize])?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::Type => {
            let (exts, ranks) = extension_ranks(store, ids);
            let k = keys_of(ids, progress, |i, id| {
                match classify(store.name(id), store.is_dir(id)) {
                    // A dot in a folder name does not make a type.
                    FileKind::Folder => (FileKind::Folder, 0),
                    kind => (kind, ranks[exts[i] as usize]),
                }
            })?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::Created => {
            let k = detail_keys(index, ids, threads, progress, |d| d.created_ns)?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::Accessed => {
            let k = detail_keys(index, ids, threads, progress, |d| d.accessed_ns)?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::Attributes => {
            let k = detail_keys(index, ids, threads, progress, |d| d.attributes)?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
    }
    Ok(())
}

/// Ids whose details are read between two looks at the progress.
const DETAIL_BATCH: usize = 1024;

/// `key(details)` for every id, reading the details from the file system on
/// up to `threads` threads. Entries on a drive that is not connected are not
/// read and have no details.
fn detail_keys<K: Send>(
    index: &Index,
    ids: &[NodeId],
    threads: usize,
    progress: &Progress,
    key: impl Fn(&Details) -> K + Sync,
) -> Result<Vec<K>, Cancelled> {
    let store = index.store();
    let read = |id: NodeId| {
        if index.is_offline(id) {
            Details::default()
        } else {
            Details::read(&store.path(id), store.name(id))
        }
    };
    let part = ids.len().div_ceil(threads.max(1)).max(DETAIL_BATCH);
    let parts: Vec<Result<Vec<K>, Cancelled>> = std::thread::scope(|scope| {
        let workers: Vec<_> = ids
            .chunks(part)
            .map(|chunk| {
                let (read, key) = (&read, &key);
                scope.spawn(move || {
                    let mut keys = Vec::with_capacity(chunk.len());
                    for batch in chunk.chunks(DETAIL_BATCH) {
                        progress.check()?;
                        keys.extend(batch.iter().map(|&id| key(&read(id))));
                        progress.add(batch.len() as u64);
                    }
                    Ok(keys)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("detail reader"))
            .collect()
    });
    let mut keys = Vec::with_capacity(ids.len());
    for part in parts {
        keys.extend(part?);
    }
    Ok(keys)
}

/// `key(position, id)` for every id, in batches with a look at the progress
/// before each.
fn keys_of<K>(
    ids: &[NodeId],
    progress: &Progress,
    mut key: impl FnMut(usize, NodeId) -> K,
) -> Result<Vec<K>, Cancelled> {
    let mut keys = Vec::with_capacity(ids.len());
    for (c, chunk) in ids.chunks(KEY_BATCH).enumerate() {
        progress.check()?;
        let start = c * KEY_BATCH;
        keys.extend(chunk.iter().enumerate().map(|(i, &id)| key(start + i, id)));
        progress.add(chunk.len() as u64);
    }
    Ok(keys)
}

/// Folded extension of a node's name, without the dot. A dot at the very start
/// (`.bashrc`) does not begin an extension.
pub fn extension(store: &NodeStore, id: NodeId) -> &[u8] {
    let name = store.folded_name(id);
    match name.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => &[],
    }
}

/// Sorts by `keys` (one per id) in the given direction, ties in input order.
fn sort_directed<K: Ord + Send + Copy>(
    ids: &mut [NodeId],
    threads: usize,
    desc: bool,
    keys: Vec<K>,
    progress: &Progress,
) -> Result<(), Cancelled> {
    if desc {
        sort_by_keys(
            ids,
            threads,
            keys.into_iter().map(Reverse).collect(),
            progress,
        )
    } else {
        sort_by_keys(ids, threads, keys, progress)
    }
}

/// Sorts `ids` by `keys` (one per id); equal keys keep their input order.
/// When cancelled, `ids` are left as they were.
fn sort_by_keys<K: Ord + Send + Copy>(
    ids: &mut [NodeId],
    threads: usize,
    keys: Vec<K>,
    progress: &Progress,
) -> Result<(), Cancelled> {
    progress.check()?;
    let mut keyed: Vec<(K, u32)> = keys
        .into_iter()
        .zip(0..u32::try_from(ids.len()).expect("result count fits in u32"))
        .collect();
    // The input position makes every pair unique, so an unstable sort is
    // deterministic and keeps ties in input order.
    par_sort_unstable(&mut keyed, threads);
    progress.check()?;
    let before = ids.to_vec();
    for (slot, (_, i)) in ids.iter_mut().zip(&keyed) {
        *slot = before[*i as usize];
    }
    progress.add(ids.len() as u64);
    Ok(())
}

/// Puts the runs of equal names in `ids` (which is in the index's name order)
/// in folder path order, in place, looking for them on up to `threads`
/// threads. Returns the runs as `start..end` ranges. When cancelled, `ids` are
/// left as they were.
///
/// Equal raw names have equal folded names, so they are next to each other in
/// the name order and one look at each neighbour finds them; only folders that
/// hold a repeated name get a path key.
fn order_equal_names(
    store: &NodeStore,
    ids: &mut [NodeId],
    threads: usize,
    progress: &Progress,
) -> Result<Vec<(usize, usize)>, Cancelled> {
    let same = same_as_previous(store, ids, threads, progress)?;
    // An id equal to its previous one continues the run that one belongs to.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &i in &same {
        let i = i as usize;
        match runs.last_mut() {
            Some((_, end)) if *end == i => *end = i + 1,
            _ => runs.push((i - 1, i + 1)),
        }
    }
    if runs.is_empty() {
        return Ok(runs);
    }
    let tied: Vec<NodeId> = runs
        .iter()
        .flat_map(|&(from, to)| ids[from..to].iter().copied())
        .collect();
    let ranks = folder_ranks(store, &tied);
    // Sorts the runs of one group in its part of `ids`, which starts at
    // position `base`. Names in one folder are unique, so the folder rank
    // alone decides.
    let sort_runs = |runs: &[(usize, usize)], part: &mut [NodeId], base: usize| {
        let mut keyed: Vec<(u32, NodeId)> = Vec::new();
        for &(from, to) in runs {
            let run = &mut part[from - base..to - base];
            keyed.clear();
            keyed.extend(
                run.iter()
                    .map(|&id| (store.parent(id).map_or(0, |p| ranks[p.index()]), id)),
            );
            keyed.sort_unstable();
            for (slot, &(_, id)) in run.iter_mut().zip(&keyed) {
                *slot = id;
            }
        }
    };
    let threads = threads.max(1);
    if threads == 1 || tied.len() < PARALLEL_MIN {
        sort_runs(&runs, ids, 0);
        return Ok(runs);
    }
    // Groups of whole runs with about the same number of ids each, one per
    // thread; their parts of `ids` do not overlap.
    let per = tied.len().div_ceil(threads);
    let mut groups: Vec<&[(usize, usize)]> = Vec::new();
    let (mut first, mut count) = (0, 0);
    for (r, &(from, to)) in runs.iter().enumerate() {
        count += to - from;
        if count >= per || r + 1 == runs.len() {
            groups.push(&runs[first..=r]);
            (first, count) = (r + 1, 0);
        }
    }
    std::thread::scope(|scope| {
        let (mut rest, mut base) = (&mut *ids, 0);
        for group in groups {
            let end = group.last().expect("groups are not empty").1;
            let (part, tail) = std::mem::take(&mut rest).split_at_mut(end - base);
            let sort_runs = &sort_runs;
            scope.spawn(move || sort_runs(group, part, base));
            (rest, base) = (tail, end);
        }
    });
    Ok(runs)
}

/// Positions `i` (in increasing order) where `ids[i]` has the same raw name as
/// `ids[i - 1]`, found on up to `threads` threads.
fn same_as_previous(
    store: &NodeStore,
    ids: &[NodeId],
    threads: usize,
    progress: &Progress,
) -> Result<Vec<u32>, Cancelled> {
    let scan = |from: usize, to: usize| -> Result<Vec<u32>, Cancelled> {
        let mut out = Vec::new();
        for start in (from.max(1)..to).step_by(KEY_BATCH) {
            progress.check()?;
            for i in start..to.min(start + KEY_BATCH) {
                if store.name(ids[i]) == store.name(ids[i - 1]) {
                    out.push(u32::try_from(i).expect("result count fits in u32"));
                }
            }
        }
        Ok(out)
    };
    let threads = threads.max(1);
    if threads == 1 || ids.len() < PARALLEL_MIN {
        return scan(0, ids.len());
    }
    let part = ids.len().div_ceil(threads);
    let parts: Vec<Result<Vec<u32>, Cancelled>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..ids.len())
            .step_by(part)
            .map(|from| {
                let scan = &scan;
                scope.spawn(move || scan(from, (from + part).min(ids.len())))
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("name comparer"))
            .collect()
    });
    let mut same = Vec::new();
    for part in parts {
        same.extend(part?);
    }
    Ok(same)
}

/// Sorts `v`, splitting large slices into one run per thread that are sorted in
/// parallel and then merged pairwise, also in parallel.
fn par_sort_unstable<T: Ord + Send + Copy>(v: &mut [T], threads: usize) {
    let threads = threads.max(1);
    if threads == 1 || v.len() < PARALLEL_MIN {
        v.sort_unstable();
        return;
    }
    let run = v.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for chunk in v.chunks_mut(run) {
            scope.spawn(|| chunk.sort_unstable());
        }
    });
    let mut width = run;
    while width < v.len() {
        std::thread::scope(|scope| {
            for pair in v.chunks_mut(2 * width) {
                if pair.len() > width {
                    scope.spawn(move || merge_halves(pair, width));
                }
            }
        });
        width *= 2;
    }
}

/// Merges the sorted runs `v[..mid]` and `v[mid..]` in place, using a copy of
/// the left run.
fn merge_halves<T: Ord + Copy>(v: &mut [T], mid: usize) {
    if v[mid - 1] <= v[mid] {
        return;
    }
    let left = v[..mid].to_vec();
    let (mut i, mut j, mut k) = (0, mid, 0);
    // `k == i + (j - mid)`, so writing at `k` never overwrites an unread
    // element of the right run.
    while i < left.len() {
        if j < v.len() && v[j] < left[i] {
            v[k] = v[j];
            j += 1;
        } else {
            v[k] = left[i];
            i += 1;
        }
        k += 1;
    }
}

/// Ranks the parents of `ids` by their path, compared component by component on
/// folded names, then on raw names. Returns a table indexed by node id; ranks
/// start at 1, so 0 is left for roots, which have no parent.
fn folder_ranks(store: &NodeStore, ids: &[NodeId]) -> Vec<u32> {
    let mut ranks = vec![0; store.len()];
    // Collect each parent once, marking it in the rank table meanwhile.
    let mut folders: Vec<NodeId> = Vec::new();
    for p in ids.iter().filter_map(|&id| store.parent(id)) {
        if ranks[p.index()] == 0 {
            ranks[p.index()] = 1;
            folders.push(p);
        }
    }
    let mut keyed: Vec<(PathKey, NodeId)> = folders
        .into_iter()
        .map(|f| (path_key(store, f), f))
        .collect();
    keyed.sort_unstable();
    for (rank, (_, folder)) in (1..).zip(&keyed) {
        ranks[folder.index()] = rank;
    }
    ranks
}

/// Interns the extensions of `ids`. Returns, per id in order, the number of its
/// distinct extension, and the rank of each distinct extension in byte order.
fn extension_ranks(store: &NodeStore, ids: &[NodeId]) -> (Vec<u32>, Vec<u32>) {
    let mut seen: HashMap<&[u8], u32> = HashMap::new();
    let mut distinct: Vec<&[u8]> = Vec::new();
    let exts = ids
        .iter()
        .map(|&id| {
            let ext = extension(store, id);
            *seen.entry(ext).or_insert_with(|| {
                distinct.push(ext);
                u32::try_from(distinct.len() - 1).expect("fits")
            })
        })
        .collect();
    let mut order: Vec<u32> = (0..u32::try_from(distinct.len()).expect("fits")).collect();
    order.sort_unstable_by_key(|&i| distinct[i as usize]);
    let mut ranks = vec![0; distinct.len()];
    for (rank, &i) in (0..).zip(&order) {
        ranks[i as usize] = rank;
    }
    (exts, ranks)
}

/// Folded and raw path of a folder, see [`path_key`].
type PathKey = (Vec<u8>, Vec<u8>);

/// Folded and raw path of a folder with components joined by a zero byte, which
/// never occurs in names, so `a/b` sorts before `a-c` as it does component-wise.
fn path_key(store: &NodeStore, folder: NodeId) -> PathKey {
    let mut chain = vec![folder];
    let mut cur = folder;
    while let Some(p) = store.parent(cur) {
        chain.push(p);
        cur = p;
    }
    let (mut folded, mut raw) = (Vec::new(), Vec::new());
    for (n, &part) in chain.iter().rev().enumerate() {
        if n > 0 {
            folded.push(0);
            raw.push(0);
        }
        folded.extend_from_slice(store.folded_name(part));
        raw.extend_from_slice(store.name(part));
    }
    (folded, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Query, QueryOptions};
    use crate::search::search;
    use crate::store::Meta;

    /// `/r/{b,A,a-c}/...` with files of chosen sizes and times.
    fn tree() -> Index {
        let mut s = NodeStore::new();
        let meta = |size, mtime_ns| Meta { size, mtime_ns };
        let root = s.add(None, b"/r", true, meta(0, 5));
        let b = s.add(Some(root), b"b", true, meta(0, 1));
        let a = s.add(Some(root), b"A", true, meta(0, 2));
        let ac = s.add(Some(root), b"a-c", true, meta(0, 3));
        s.add(Some(b), b"zeta.TXT", false, meta(30, 10));
        s.add(Some(b), b"alpha.pdf", false, meta(10, 40));
        s.add(Some(a), b"mid.txt", false, meta(20, 30));
        s.add(Some(a), b".bashrc", false, meta(10, 20));
        s.add(Some(ac), b"Beta.Pdf", false, meta(10, 50));
        Index::new(s)
    }

    fn sorted(index: &Index, key: SortKey, descending: bool, threads: usize) -> Vec<String> {
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let mut ids = search(index, &all, 1);
        sort(
            index,
            &mut ids,
            Sort {
                key,
                descending,
                folders: FolderSize::None,
            },
            threads,
        );
        ids.iter()
            .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
            .collect()
    }

    fn check(key: SortKey, ascending: &[&str], descending: &[&str]) {
        let index = tree();
        assert_eq!(
            sorted(&index, key, false, 1),
            ascending,
            "{key:?} ascending"
        );
        assert_eq!(
            sorted(&index, key, true, 1),
            descending,
            "{key:?} descending"
        );
    }

    #[test]
    fn name_is_search_order_and_its_reverse() {
        let asc = [
            ".bashrc",
            "/r",
            "A",
            "a-c",
            "alpha.pdf",
            "b",
            "Beta.Pdf",
            "mid.txt",
            "zeta.TXT",
        ];
        let mut desc = asc;
        desc.reverse();
        check(SortKey::Name, &asc, &desc);
    }

    /// Full paths of all nodes named `name`, sorted by `key`.
    fn paths_named(index: &Index, name: &str, key: SortKey, descending: bool) -> Vec<String> {
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let mut ids = search(index, &all, 1);
        let s = Sort {
            key,
            descending,
            folders: FolderSize::None,
        };
        sort(index, &mut ids, s, 1);
        ids.iter()
            .filter(|&&id| index.store().name(id) == name.as_bytes())
            .map(|&id| index.store().path(id).to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn equal_names_sort_by_folder_path() {
        // Folders made in an order unlike their path order, so ids (and the
        // order of the index) disagree with the folder paths.
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let z = index.insert(Some(root), b"z", true, Meta::default());
        let m = index.insert(Some(root), b"M", true, Meta::default());
        let a = index.insert(Some(root), b"a", true, Meta::default());
        let deep = index.insert(Some(z), b"a", true, Meta::default());
        for folder in [z, deep, m, a, root] {
            index.insert(Some(folder), b"readme.md", false, Meta::default());
        }
        let asc = [
            "/r/readme.md",
            "/r/a/readme.md",
            "/r/M/readme.md",
            "/r/z/readme.md",
            "/r/z/a/readme.md",
        ];
        let desc: Vec<&str> = asc.iter().rev().copied().collect();
        assert_eq!(paths_named(&index, "readme.md", SortKey::Name, false), asc);
        assert_eq!(paths_named(&index, "readme.md", SortKey::Name, true), desc);
        // Ties of other keys keep that name order, in both directions.
        for key in [SortKey::Size, SortKey::Modified, SortKey::Type] {
            for descending in [false, true] {
                assert_eq!(
                    paths_named(&index, "readme.md", key, descending),
                    asc,
                    "{key:?} {descending}"
                );
            }
        }

        // A renamed folder moves its file to the new folder path, and a new
        // file lands at its folder's place.
        index.rename(a, Some(root), b"zz").unwrap();
        let b = index.insert(Some(root), b"b", true, Meta::default());
        index.insert(Some(b), b"readme.md", false, Meta::default());
        assert_eq!(
            paths_named(&index, "readme.md", SortKey::Name, false),
            [
                "/r/readme.md",
                "/r/b/readme.md",
                "/r/M/readme.md",
                "/r/z/readme.md",
                "/r/z/a/readme.md",
                "/r/zz/readme.md",
            ]
        );
    }

    #[test]
    fn path_sorts_by_folder_component_wise_then_name() {
        // Roots first, then /r, then /r/A before /r/a-c (component order), /r/b.
        let asc = [
            "/r",
            "A",
            "a-c",
            "b",
            ".bashrc",
            "mid.txt",
            "Beta.Pdf",
            "alpha.pdf",
            "zeta.TXT",
        ];
        let mut desc = asc;
        desc.reverse();
        check(SortKey::Path, &asc, &desc);
    }

    #[test]
    fn size_ties_keep_name_order() {
        check(
            SortKey::Size,
            &[
                "/r",
                "A",
                "a-c",
                "b",
                ".bashrc",
                "alpha.pdf",
                "Beta.Pdf",
                "mid.txt",
                "zeta.TXT",
            ],
            &[
                "zeta.TXT",
                "mid.txt",
                ".bashrc",
                "alpha.pdf",
                "Beta.Pdf",
                "/r",
                "A",
                "a-c",
                "b",
            ],
        );
    }

    fn sorted_by_size(index: &Index, folders: FolderSize, descending: bool) -> Vec<String> {
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let mut ids = search(index, &all, 1);
        let s = Sort {
            key: SortKey::Size,
            descending,
            folders,
        };
        sort(index, &mut ids, s, 1);
        ids.iter()
            .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
            .collect()
    }

    #[test]
    fn folders_sort_by_total_size_together_with_files() {
        // Totals: /r 80, b 40, A 30, a-c 10.
        let index = tree();
        assert_eq!(
            sorted_by_size(&index, FolderSize::Total, false),
            [
                ".bashrc",
                "a-c",
                "alpha.pdf",
                "Beta.Pdf",
                "mid.txt",
                "A",
                "zeta.TXT",
                "b",
                "/r"
            ]
        );
        assert_eq!(
            sorted_by_size(&index, FolderSize::Total, true),
            [
                "/r",
                "b",
                "A",
                "zeta.TXT",
                "mid.txt",
                ".bashrc",
                "a-c",
                "alpha.pdf",
                "Beta.Pdf"
            ]
        );
    }

    #[test]
    fn folders_sort_by_items_apart_from_files() {
        // Items: /r 3, b 2, A 2, a-c 1.
        let index = tree();
        assert_eq!(
            sorted_by_size(&index, FolderSize::Items, false),
            [
                "a-c",
                "A",
                "b",
                "/r",
                ".bashrc",
                "alpha.pdf",
                "Beta.Pdf",
                "mid.txt",
                "zeta.TXT"
            ]
        );
        assert_eq!(
            sorted_by_size(&index, FolderSize::Items, true),
            [
                "zeta.TXT",
                "mid.txt",
                ".bashrc",
                "alpha.pdf",
                "Beta.Pdf",
                "/r",
                "A",
                "b",
                "a-c"
            ]
        );
    }

    #[test]
    fn folder_size_ids_round_trip() {
        for f in FolderSize::ALL {
            assert_eq!(FolderSize::from_id(f.id()), Some(f));
        }
        assert_eq!(FolderSize::from_id("bytes"), None);
        assert_eq!(FolderSize::default(), FolderSize::Total);
    }

    #[test]
    fn modified_sorts_by_time() {
        check(
            SortKey::Modified,
            &[
                "b",
                "A",
                "a-c",
                "/r",
                "zeta.TXT",
                ".bashrc",
                "mid.txt",
                "alpha.pdf",
                "Beta.Pdf",
            ],
            &[
                "Beta.Pdf",
                "alpha.pdf",
                "mid.txt",
                ".bashrc",
                "zeta.TXT",
                "/r",
                "a-c",
                "A",
                "b",
            ],
        );
    }

    #[test]
    fn extension_is_folded_and_ignores_leading_dot() {
        check(
            SortKey::Extension,
            &[
                ".bashrc",
                "/r",
                "A",
                "a-c",
                "b",
                "alpha.pdf",
                "Beta.Pdf",
                "mid.txt",
                "zeta.TXT",
            ],
            &[
                "mid.txt",
                "zeta.TXT",
                "alpha.pdf",
                "Beta.Pdf",
                ".bashrc",
                "/r",
                "A",
                "a-c",
                "b",
            ],
        );
    }

    #[test]
    fn type_groups_by_kind_then_extension() {
        check(
            SortKey::Type,
            &[
                "/r",
                "A",
                "a-c",
                "b",
                "mid.txt",
                "zeta.TXT",
                "alpha.pdf",
                "Beta.Pdf",
                ".bashrc",
            ],
            &[
                ".bashrc",
                "alpha.pdf",
                "Beta.Pdf",
                "mid.txt",
                "zeta.TXT",
                "/r",
                "A",
                "a-c",
                "b",
            ],
        );
    }

    /// Sorted names of a scanned temporary folder.
    fn sorted_on_disk(index: &Index, key: SortKey, descending: bool) -> Vec<String> {
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let mut ids = search(index, &all, 1);
        let progress = Progress::new();
        let s = Sort {
            key,
            descending,
            ..Sort::default()
        };
        sort_with_progress(index, &mut ids, s, 3, &progress).unwrap();
        assert_eq!(progress.done(), 2 * ids.len() as u64);
        ids.iter()
            .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
            .filter(|n| n.starts_with('f'))
            .collect()
    }

    #[test]
    fn details_are_read_from_disk_for_sorting() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, UNIX_EPOCH};
        let dir =
            std::env::temp_dir().join(format!("everygrep-sort-details-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Access times 3, 1, 2 days after the epoch; modes: executable,
        // read-only, plain.
        for (name, day, mode) in [("f1", 3, 0o755), ("f2", 1, 0o444), ("f3", 2, 0o644)] {
            let path = dir.join(name);
            fs::write(&path, b"x").unwrap();
            let times =
                fs::FileTimes::new().set_accessed(UNIX_EPOCH + Duration::from_secs(day * 86_400));
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(times)
                .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
        let mut store = NodeStore::new();
        crate::scan::scan(&mut store, std::slice::from_ref(&dir), &Default::default());
        let index = Index::new(store);
        assert_eq!(
            sorted_on_disk(&index, SortKey::Accessed, false),
            ["f2", "f3", "f1"]
        );
        assert_eq!(
            sorted_on_disk(&index, SortKey::Accessed, true),
            ["f1", "f3", "f2"]
        );
        // Plain (no letters) < X < R.
        assert_eq!(
            sorted_on_disk(&index, SortKey::Attributes, false),
            ["f3", "f1", "f2"]
        );
        // Every entry gets a key; ties (all created now, or none) keep name order.
        assert_eq!(sorted_on_disk(&index, SortKey::Created, false).len(), 3);

        // Cancelled before it starts: the order stays.
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let ids = search(&index, &all, 1);
        let mut cancelled = ids.clone();
        let progress = Progress::new();
        progress.cancel();
        let s = Sort {
            key: SortKey::Created,
            ..Sort::default()
        };
        assert!(sort_with_progress(&index, &mut cancelled, s, 2, &progress).is_err());
        assert_eq!(cancelled, ids);

        fs::set_permissions(dir.join("f2"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Many nodes in many folders with pseudo-random sizes and times.
    fn big_index(n: usize) -> Index {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        let dirs: Vec<NodeId> = (0..n / 100)
            .map(|d| {
                let name = format!("dir-{:05}", (d * 7919) % (n / 100));
                s.add(Some(root), name.as_bytes(), true, Meta::default())
            })
            .collect();
        let exts = ["txt", "pdf", "rs", "jpg", "tar.gz", "md"];
        for i in 0..n {
            let h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let name = format!("file-{:08x}.{}", h >> 32, exts[i % exts.len()]);
            let meta = Meta {
                size: h % 4096,
                mtime_ns: i64::try_from(h.rotate_left(17) >> 24).unwrap(),
            };
            s.add(Some(dirs[i % dirs.len()]), name.as_bytes(), false, meta);
        }
        Index::new(s)
    }

    const KEYS: [SortKey; 6] = [
        SortKey::Name,
        SortKey::Path,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Extension,
        SortKey::Type,
    ];

    #[test]
    fn progress_reaches_its_total_and_cancel_keeps_the_input_order() {
        let index = big_index(50_000);
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let ids = search(&index, &all, 1);
        for key in KEYS {
            for descending in [false, true] {
                let s = Sort {
                    key,
                    descending,
                    ..Sort::default()
                };
                let mut plain = ids.clone();
                sort(&index, &mut plain, s, 4);
                let progress = Progress::new();
                let mut counted = ids.clone();
                sort_with_progress(&index, &mut counted, s, 4, &progress).unwrap();
                assert_eq!(counted, plain, "{key:?}");
                assert_eq!(progress.total(), Some(2 * ids.len() as u64), "{key:?}");
                assert_eq!(progress.done(), 2 * ids.len() as u64, "{key:?}");

                if key == SortKey::Name {
                    continue; // Instant, nothing to cancel.
                }
                let cancelled = Progress::new();
                cancelled.cancel();
                let mut kept = ids.clone();
                let result = sort_with_progress(&index, &mut kept, s, 4, &cancelled);
                assert_eq!(result, Err(Cancelled), "{key:?}");
                assert_eq!(kept, ids, "{key:?} {descending}");
            }
        }
    }

    #[test]
    fn parallel_sort_equals_serial_sort() {
        let index = big_index(200_000);
        for key in KEYS {
            for descending in [false, true] {
                assert_eq!(
                    sorted(&index, key, descending, 1),
                    sorted(&index, key, descending, 7),
                    "{key:?} {descending}"
                );
            }
        }
    }

    /// `n` files with only `n / 300` distinct names spread over 3 000 folders
    /// made in an order unlike their path order: every name occurs about 300
    /// times.
    fn dup_index(n: usize) -> Index {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        let dirs: Vec<NodeId> = (0..3000)
            .map(|d| {
                let name = format!("dir-{:05}", (d * 7919) % 3000);
                s.add(Some(root), name.as_bytes(), true, Meta::default())
            })
            .collect();
        for i in 0..n {
            // Within one folder (`i % 3000`) the names differ.
            let name = format!("name-{:06}.js", (i / 3000 + (i % 3000) * 7) % (n / 300));
            let meta = Meta {
                size: (i as u64 * 2_654_435_761) % 4096,
                mtime_ns: 0,
            };
            s.add(Some(dirs[i % dirs.len()]), name.as_bytes(), false, meta);
        }
        Index::new(s)
    }

    #[test]
    fn many_equal_names_sort_by_folder_on_any_thread_count() {
        let index = dup_index(120_000);
        let store = index.store();
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let found = search(&index, &all, 1);
        for key in KEYS {
            let s = Sort {
                key,
                ..Sort::default()
            };
            let mut serial = found.clone();
            sort(&index, &mut serial, s, 1);
            let mut parallel = found.clone();
            sort(&index, &mut parallel, s, 7);
            assert_eq!(serial, parallel, "{key:?}");
        }
        let mut ids = found;
        sort(&index, &mut ids, Sort::default(), 7);
        let folder = |id| path_key(store, store.parent(id).unwrap());
        for w in ids.windows(2) {
            if store.name(w[0]) == store.name(w[1]) {
                assert!(folder(w[0]) < folder(w[1]));
            }
        }
    }

    #[test]
    fn cancel_after_ordering_equal_names_restores_the_input() {
        let index = dup_index(120_000);
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let found = search(&index, &all, 1);
        let s = Sort {
            key: SortKey::Size,
            ..Sort::default()
        };
        let mut cancelled_once = false;
        for _ in 0..20 {
            let progress = Progress::new();
            let mut ids = found.clone();
            let result = std::thread::scope(|scope| {
                // Cancels once the keys are being computed, which is after
                // the equal names were put in folder order.
                scope.spawn(|| {
                    while progress.done() == 0 {
                        std::hint::spin_loop();
                    }
                    progress.cancel();
                });
                let result = sort_with_progress(&index, &mut ids, s, 1, &progress);
                progress.add(1); // Lets the canceller end if the sort did not count.
                result
            });
            if result.is_err() {
                assert_eq!(ids, found);
                cancelled_once = true;
                break;
            }
        }
        assert!(cancelled_once);
    }

    /// Prints sort times for 600 000 nodes. Run with
    /// `cargo test --release -p everygrep-core sort_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing only"]
    fn sort_timing() {
        for (label, index) in [
            ("unique names", big_index(600_000)),
            ("~300 of each name", dup_index(600_000)),
        ] {
            let all = Query::parse("", QueryOptions::default()).unwrap();
            let ids = search(&index, &all, 1);
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
            let cases = KEYS.iter().map(|&key| (key, FolderSize::Total)).chain([
                (SortKey::Size, FolderSize::Items),
                (SortKey::Size, FolderSize::None),
            ]);
            for (key, folders) in cases {
                for t in [1, threads] {
                    let mut v = ids.clone();
                    let started = std::time::Instant::now();
                    let s = Sort {
                        key,
                        descending: false,
                        folders,
                    };
                    sort(&index, &mut v, s, t);
                    eprintln!(
                        "{label}: {key:?} ({folders:?}) {} nodes, {t} threads: {:.1} ms",
                        v.len(),
                        started.elapsed().as_secs_f64() * 1e3
                    );
                }
            }
        }
    }
}
