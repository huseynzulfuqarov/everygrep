//! Sorting search results by a column.
//!
//! [`search`](crate::search::search) returns ids in name order. [`sort`] reorders
//! such a list by another key. Each key is computed once per node, then the keys
//! are sorted; equal keys keep the name order, so the result is
//! deterministic. Folders are not forced before files.
//!
//! The name order of the index is natural and ignores case and diacritics
//! (see [`crate::collate`]); names it finds equal (`Çay` and `cay`, `a-b` and
//! `ab`, the same name in many folders) are ordered by raw name and then by
//! the parent's node id, which is fast to keep but means nothing to a reader.
//! [`sort`] orders such runs of equal names by their folder path instead (the
//! order of [`SortKey::Path`]), then by raw name, for every key, so the same
//! name in many folders comes out by folder and stays so after refreshes,
//! inserts and renames.
//!
//! By size, folders count with what [`FolderSize`] says: their total size
//! (sorted together with files), their number of items, or nothing.
//!
//! Creation time, access time and attributes are not in the index; sorting
//! by them reads them from the file system for every result (see
//! [`crate::details`]), on several threads. Entries without the value (no
//! birth time on that file system, gone, on a drive that is not connected)
//! sort before all others, after them when descending.
//!
//! [`sort_cached`] keeps the creation times or attributes it read in a
//! [`DetailCache`] for the next sort of the same results, such as the
//! refresh after file changes, and reads again only the nodes the index
//! logged as changed (see [`Index::changed_since`]). Access times are always
//! read again: reading a file changes them, and no event says so.

use std::cmp::{Ordering, Reverse};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::collate::collate;
use crate::details::{Attributes, Details};
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
    /// How many entries of the index share the name (see
    /// [`crate::frequency`]).
    NameFrequency,
    /// How many files of the index share the extension.
    ExtensionFrequency,
    /// How many files (or folders) of the index share the size.
    SizeFrequency,
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

/// Creation times or attributes read by [`sort_cached`], by node id, so a
/// later sort of the same results does not read them again.
///
/// # What it trusts
///
/// A value stays until the index logs its node as changed (see
/// [`Index::changed_since`]): new metadata (an `IN_ATTRIB` event, a write),
/// a rename or move, a removal, or a node replaced by another. An
/// `IN_ATTRIB` event of a folder (such as a permission change, which can
/// make all entries below it unreadable) logs the whole subtree. The cache
/// follows the log at the start of every sort and forgets all values when
/// the log was reset (a drive went offline or came back, too many changes)
/// or when the index is another one (another [`Index::stamp`], such as after
/// a compaction). Callers forget it all with
/// [`set_generation`](Self::set_generation) for what the index cannot know:
/// their own id epochs, and anything that may have changed while file
/// changes were not watched.
///
/// So the values are only as current as the events applied to the index.
/// Changes no event reports stay unseen: below a folder whose watch could
/// not be added (for example when the inotify watch limit is reached),
/// through a hard link in another folder, or made by another machine on a
/// network file system. Use [`sort_with_progress`] where nothing watches the
/// index.
///
/// # Size
///
/// It keeps values for one sort key, the one of the last sort, and drops
/// all when the key changes. After a sort, it holds at most twice as many
/// values as that sort had results: when it holds more, only the values of
/// the listed nodes are kept. Entries that could not be read are not kept,
/// nor those on drives that are not connected. A creation time takes 24
/// bytes and an attribute 8, plus one byte of table control each, in a hash
/// table at most 7/8 full (about 29 to 57 and 10 to 21 bytes per value).
#[derive(Default)]
pub struct DetailCache {
    /// [`Index::stamp`] of the index the ids belong to.
    stamp: u64,
    /// [`Index::change_seq`] the cache has followed up to.
    seq: u64,
    /// See [`set_generation`](Self::set_generation).
    generation: u64,
    created: IdMap<Option<i64>>,
    attributes: IdMap<Option<Attributes>>,
}

impl DetailCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets every value when `generation` is not the one given last.
    pub fn set_generation(&mut self, generation: u64) {
        if generation != self.generation {
            self.clear();
            self.generation = generation;
        }
    }

    /// Forgets every value and frees the memory.
    pub fn clear(&mut self) {
        self.created = IdMap::default();
        self.attributes = IdMap::default();
    }

    /// Number of values kept.
    pub fn len(&self) -> usize {
        self.created.len() + self.attributes.len()
    }

    /// Whether no value is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate heap memory used, in bytes.
    pub fn heap_bytes(&self) -> usize {
        fn bytes<V>(map: &IdMap<V>) -> usize {
            map.capacity() * (std::mem::size_of::<(NodeId, V)>() + 1)
        }
        bytes(&self.created) + bytes(&self.attributes)
    }

    /// Drops what the index says may have changed since the last sort.
    fn follow(&mut self, index: &Index) {
        if self.stamp != index.stamp() {
            self.clear();
            self.stamp = index.stamp();
        } else {
            match index.changed_since(self.seq) {
                None => self.clear(),
                Some(changed) if !self.is_empty() => {
                    for id in changed {
                        self.created.remove(id);
                        self.attributes.remove(id);
                    }
                }
                Some(_) => {}
            }
        }
        self.seq = index.change_seq();
    }
}

/// A hash map keyed by node id.
type IdMap<V> = HashMap<NodeId, V, BuildHasherDefault<IdHasher>>;

/// Hashes node ids by one multiplication. Ids are small dense numbers; an odd
/// factor keeps them distinct in the low bits the table uses for buckets
/// and spreads them over the high bits it uses for its tags.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.0 = u64::from(n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Like [`sort_with_progress`], but creation times and attributes are taken
/// from `cache` where it has them, and those read are kept there (see
/// [`DetailCache`] for when that is right). The order is the same as
/// without the cache as long as the index has applied every change.
pub fn sort_cached(
    index: &Index,
    ids: &mut [NodeId],
    sort: Sort,
    threads: usize,
    progress: &Progress,
    cache: &mut DetailCache,
) -> Result<(), Cancelled> {
    cache.follow(index);
    sort_maybe_cached(index, ids, sort, threads, progress, Some(cache))
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
    sort_maybe_cached(index, ids, sort, threads, progress, None)
}

fn sort_maybe_cached(
    index: &Index,
    ids: &mut [NodeId],
    sort: Sort,
    threads: usize,
    progress: &Progress,
    cache: Option<&mut DetailCache>,
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
    let sorted = sort_ordered(index, ids, sort, threads, progress, cache);
    if sorted.is_err() {
        // In the input, equal names are in raw name, then parent id order.
        for &(from, to) in &runs {
            ids[from..to].sort_unstable_by(|&a, &b| {
                store
                    .name(a)
                    .cmp(store.name(b))
                    .then_with(|| store.parent_key(a).cmp(&store.parent_key(b)))
            });
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
    mut cache: Option<&mut DetailCache>,
) -> Result<(), Cancelled> {
    // Values are kept for the current key only.
    if let Some(c) = cache.as_deref_mut() {
        if sort.key != SortKey::Created {
            c.created = IdMap::default();
        }
        if sort.key != SortKey::Attributes {
            c.attributes = IdMap::default();
        }
    }
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
            let kept = cache.map(|c| &mut c.created);
            let k = detail_keys(index, ids, threads, progress, |d| d.created_ns, kept)?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::Accessed => {
            // Never cached: reads change it without an event.
            let k = detail_keys(index, ids, threads, progress, |d| d.accessed_ns, None)?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::Attributes => {
            let kept = cache.map(|c| &mut c.attributes);
            let k = detail_keys(index, ids, threads, progress, |d| d.attributes, kept)?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
        SortKey::NameFrequency | SortKey::ExtensionFrequency | SortKey::SizeFrequency => {
            let f = index.frequencies();
            progress.check()?;
            let k = keys_of(ids, progress, |_, id| match sort.key {
                SortKey::NameFrequency => f.name(id),
                SortKey::ExtensionFrequency => f.extension(id),
                _ => f.size(id),
            })?;
            sort_directed(ids, threads, desc, k, progress)?;
        }
    }
    Ok(())
}

/// Ids whose details are read between two looks at the progress.
const DETAIL_BATCH: usize = 1024;

/// `key(details)` for every id, reading the details from the file system on
/// up to `threads` threads. Entries on a drive that is not connected are not
/// read and have no details. With `kept`, keys found there are not read
/// again, and keys read are added (see [`DetailCache`]).
fn detail_keys<K: Copy + Send + Sync>(
    index: &Index,
    ids: &[NodeId],
    threads: usize,
    progress: &Progress,
    key: impl Fn(&Details) -> K + Sync,
    kept: Option<&mut IdMap<K>>,
) -> Result<Vec<K>, Cancelled> {
    let store = index.store();
    let known = kept.as_deref();
    // The key and, when it was read now, whether it can be kept.
    let look = |id: NodeId| {
        if index.is_offline(id) {
            (key(&Details::default()), None)
        } else if let Some(&k) = known.and_then(|m| m.get(&id)) {
            (k, None)
        } else {
            let d = Details::read(&store.path(id), store.name(id));
            // Not kept when unreadable: a change of a folder above (its
            // permissions) can make it readable without an event for it.
            (key(&d), Some(d.attributes.is_some()))
        }
    };
    let part = ids.len().div_ceil(threads.max(1)).max(DETAIL_BATCH);
    // Keys, keys read that can be kept, number of reads.
    type Part<K> = Result<(Vec<K>, Vec<(NodeId, K)>, u64), Cancelled>;
    let parts: Vec<Part<K>> = std::thread::scope(|scope| {
        let workers: Vec<_> = ids
            .chunks(part)
            .map(|chunk| {
                let look = &look;
                scope.spawn(move || {
                    let mut keys = Vec::with_capacity(chunk.len());
                    let mut read = Vec::new();
                    let mut reads = 0;
                    for batch in chunk.chunks(DETAIL_BATCH) {
                        progress.check()?;
                        for &id in batch {
                            let (k, fresh) = look(id);
                            keys.push(k);
                            if let Some(keep) = fresh {
                                reads += 1;
                                if keep {
                                    read.push((id, k));
                                }
                            }
                        }
                        progress.add(batch.len() as u64);
                    }
                    Ok((keys, read, reads))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("detail reader"))
            .collect()
    });
    let mut keys = Vec::with_capacity(ids.len());
    let mut read = Vec::new();
    let mut reads = 0;
    for part in parts {
        let (k, r, n) = part?;
        keys.extend(k);
        read.extend(r);
        reads += n;
    }
    count_reads(reads);
    if let Some(map) = kept {
        map.extend(read);
        if map.len() > 2 * ids.len() {
            let old = std::mem::take(map);
            map.extend(ids.iter().filter_map(|id| old.get(id).map(|&k| (*id, k))));
        }
    }
    Ok(keys)
}

#[cfg(test)]
thread_local! {
    static READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Adds `n` to [`detail_reads`] in tests.
#[cfg(test)]
fn count_reads(n: u64) {
    READS.with(|r| r.set(r.get() + n));
}

#[cfg(not(test))]
fn count_reads(_: u64) {}

/// Details read from the file system by sorts on this thread so far.
#[cfg(test)]
fn detail_reads() -> u64 {
    READS.with(std::cell::Cell::get)
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
/// in folder path order, then raw name order, in place, looking for them on up
/// to `threads` threads. Returns the runs as `start..end` ranges. When
/// cancelled, `ids` are left as they were.
///
/// Names that collate equal are next to each other in the name order, already
/// in raw name order, so one look at each neighbour finds them; only folders
/// that hold such a name get a path key.
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
    // position `base`. Within one folder the run is in raw name order
    // already, and the position keeps it.
    let sort_runs = |runs: &[(usize, usize)], part: &mut [NodeId], base: usize| {
        let mut keyed: Vec<(u32, u32, NodeId)> = Vec::new();
        for &(from, to) in runs {
            let run = &mut part[from - base..to - base];
            keyed.clear();
            keyed.extend(
                (0..)
                    .zip(run.iter())
                    .map(|(at, &id)| (store.parent(id).map_or(0, |p| ranks[p.index()]), at, id)),
            );
            keyed.sort_unstable();
            for (slot, &(_, _, id)) in run.iter_mut().zip(&keyed) {
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

/// Positions `i` (in increasing order) where the name of `ids[i]` collates
/// equal to the name of `ids[i - 1]`, found on up to `threads` threads.
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
                if same_name(store, ids[i - 1], ids[i]) {
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

/// Whether the names of `a` and `b` collate equal.
fn same_name(store: &NodeStore, a: NodeId, b: NodeId) -> bool {
    let (fa, fb) = (store.folded_name(a), store.folded_name(b));
    fa == fb || collate(fa, fb) == Ordering::Equal
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

/// Ranks the parents of `ids` by their path (see [`cmp_paths`]). Returns a table indexed by node id; ranks
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
    let mut keyed: Vec<(Vec<NodeId>, NodeId)> = folders
        .into_iter()
        .map(|f| (path_of(store, f), f))
        .collect();
    keyed.sort_unstable_by(|a, b| cmp_paths(store, &a.0, &b.0).then(a.1.cmp(&b.1)));
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

/// The folders from the root down to `folder`.
fn path_of(store: &NodeStore, folder: NodeId) -> Vec<NodeId> {
    let mut chain = vec![folder];
    let mut cur = folder;
    while let Some(p) = store.parent(cur) {
        chain.push(p);
        cur = p;
    }
    chain.reverse();
    chain
}

/// Compares two paths (from [`path_of`]) component by component, each in the
/// index's name order (natural, then raw name), so `/r/dir2` comes before
/// `/r/dir10`, `a/b` before `a-c`, and the folders below one folder stay
/// together; a path comes before the longer paths it starts.
fn cmp_paths(store: &NodeStore, a: &[NodeId], b: &[NodeId]) -> Ordering {
    for (&x, &y) in a.iter().zip(b) {
        if x == y {
            continue;
        }
        let order = collate(store.folded_name(x), store.folded_name(y))
            .then_with(|| store.name(x).cmp(store.name(y)));
        if order.is_ne() {
            return order;
        }
    }
    a.len().cmp(&b.len())
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

    /// Full paths of all nodes, sorted by `key`.
    fn all_paths(index: &Index, key: SortKey, descending: bool) -> Vec<String> {
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let mut ids = search(index, &all, 1);
        let s = Sort {
            key,
            descending,
            folders: FolderSize::None,
        };
        sort(index, &mut ids, s, 1);
        ids.iter()
            .map(|&id| index.store().path(id).to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn names_that_collate_equal_sort_by_folder_then_raw_name() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        let z = index.insert(Some(root), b"z", true, Meta::default());
        let a = index.insert(Some(root), b"a", true, Meta::default());
        for name in ["ab", "a-b"] {
            index.insert(Some(z), name.as_bytes(), false, Meta::default());
        }
        index.insert(Some(a), b"ab", false, Meta::default());
        for name in ["Çay", "cay"] {
            index.insert(Some(a), name.as_bytes(), false, Meta::default());
        }
        index.insert(Some(z), "Çay".as_bytes(), false, Meta::default());
        let asc = [
            "/r",
            "/r/a",
            "/r/a/ab",
            "/r/z/a-b",
            "/r/z/ab",
            "/r/a/cay",
            "/r/a/Çay",
            "/r/z/Çay",
            "/r/z",
        ];
        assert_eq!(all_paths(&index, SortKey::Name, false), asc);
        let desc: Vec<&str> = asc.iter().rev().copied().collect();
        assert_eq!(all_paths(&index, SortKey::Name, true), desc);
        // Other keys keep that order for ties, in both directions.
        for descending in [false, true] {
            assert_eq!(all_paths(&index, SortKey::Size, descending), asc);
        }
    }

    #[test]
    fn path_sorts_folders_naturally() {
        let mut index = Index::new(NodeStore::new());
        let root = index.insert(None, b"/r", true, Meta::default());
        for dir in ["dir10", "Dir2", "dir-1", "Ölçü", "olcu", "z"] {
            let d = index.insert(Some(root), dir.as_bytes(), true, Meta::default());
            index.insert(Some(d), b"f", false, Meta::default());
            // Folders whose names sort as equal keep their subfolders apart.
            let sub: &[u8] = if dir == "olcu" { b"x" } else { b"a" };
            if dir == "olcu" || dir == "Ölçü" {
                let s = index.insert(Some(d), sub, true, Meta::default());
                index.insert(Some(s), b"f", false, Meta::default());
            }
        }
        let files: Vec<String> = all_paths(&index, SortKey::Path, false)
            .into_iter()
            .filter(|p| p.ends_with("/f"))
            .collect();
        assert_eq!(
            files,
            [
                "/r/dir-1/f",
                "/r/Dir2/f",
                "/r/dir10/f",
                "/r/olcu/f",
                "/r/olcu/x/f",
                "/r/Ölçü/f",
                "/r/Ölçü/a/f",
                "/r/z/f"
            ]
        );
        // The same name in those folders sorts by name in that folder order.
        let by_name: Vec<String> = all_paths(&index, SortKey::Name, false)
            .into_iter()
            .filter(|p| p.ends_with("/f"))
            .collect();
        assert_eq!(by_name, files);
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
        let folder = |id| path_of(store, store.parent(id).unwrap());
        for w in ids.windows(2) {
            if store.name(w[0]) == store.name(w[1]) {
                assert_eq!(
                    cmp_paths(store, &folder(w[0]), &folder(w[1])),
                    Ordering::Less
                );
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

    /// A temporary folder `files/` with `f0` .. `f{n-1}` (every third one
    /// executable, every fifth read-only), scanned and watched.
    struct Live {
        base: std::path::PathBuf,
        index: Index,
        watcher: crate::watch::Watcher,
    }

    impl Live {
        fn new(tag: &str, n: usize) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let base = std::env::temp_dir()
                .join(format!("everygrep-sort-cache-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let dir = base.join("files");
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..n {
                let path = dir.join(format!("f{i}"));
                std::fs::write(&path, b"x").unwrap();
                let mode = match i {
                    _ if i % 5 == 0 => 0o444,
                    _ if i % 3 == 0 => 0o755,
                    _ => 0o644,
                };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            let mut store = NodeStore::new();
            crate::scan::scan(&mut store, std::slice::from_ref(&base), &Default::default());
            let index = Index::new(store);
            let watcher = crate::watch::Watcher::new(&index, &Default::default()).unwrap();
            Self {
                base,
                index,
                watcher,
            }
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.base.join("files").join(name)
        }

        fn files(&self) -> NodeId {
            let root = self.index.roots()[0];
            self.index.find_child(Some(root), b"files").unwrap()
        }

        /// Applies events until `done` holds; panics after five seconds.
        fn settle(&mut self, done: impl Fn(&Index) -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done(&self.index) {
                assert!(std::time::Instant::now() < deadline, "events did not come");
                self.watcher
                    .poll(&mut self.index, Some(std::time::Duration::from_millis(50)))
                    .unwrap();
            }
            // Anything else already queued.
            self.watcher
                .poll(&mut self.index, Some(std::time::Duration::from_millis(50)))
                .unwrap();
        }

        /// Sorts all nodes by creation time, attributes and access time,
        /// each up and down, with one cache per key (`caches`) and without;
        /// asserts the orders are the same. Returns the names by attributes.
        fn check(&self, caches: &mut [DetailCache; 3]) -> Vec<String> {
            let all = Query::parse("", QueryOptions::default()).unwrap();
            let ids = search(&self.index, &all, 1);
            let mut order = Vec::new();
            let keys = [SortKey::Created, SortKey::Attributes, SortKey::Accessed];
            for (key, cache) in keys.into_iter().zip(caches.iter_mut()) {
                for descending in [false, true] {
                    let s = Sort {
                        key,
                        descending,
                        ..Sort::default()
                    };
                    let (mut plain, mut cached) = (ids.clone(), ids.clone());
                    sort(&self.index, &mut plain, s, 2);
                    let progress = Progress::new();
                    sort_cached(&self.index, &mut cached, s, 2, &progress, cache).unwrap();
                    assert_eq!(progress.done(), 2 * ids.len() as u64);
                    let names = |v: &[NodeId]| -> Vec<String> {
                        v.iter()
                            .map(|&id| String::from_utf8_lossy(self.index.store().name(id)).into())
                            .collect()
                    };
                    assert_eq!(names(&cached), names(&plain), "{s:?}");
                    if key == SortKey::Attributes && !descending {
                        order = names(&plain);
                    }
                    // Values for one key and the listed nodes only.
                    assert!(cache.len() <= ids.len(), "{} > {}", cache.len(), ids.len());
                }
            }
            order
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            for dir in ["files", "locked"] {
                let _ = std::fs::set_permissions(
                    self.base.join(dir),
                    std::fs::Permissions::from_mode(0o755),
                );
            }
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn cached_details_sort_like_fresh_ones_after_changes() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let mut live = Live::new("changes", 12);
        let mut caches: [DetailCache; 3] = Default::default();
        let before = live.check(&mut caches);
        let [created, attributes, accessed] = &caches;
        assert!(!created.is_empty() && !attributes.is_empty() && accessed.is_empty());

        // chmod: a plain file becomes read-only.
        fs::set_permissions(live.path("f1"), fs::Permissions::from_mode(0o444)).unwrap();
        let f1 = live.index.find_child(Some(live.files()), b"f1").unwrap();
        let seq = live.index.change_seq();
        live.settle(|i| i.changed_since(seq).is_none_or(|c| c.contains(&f1)));
        let after = live.check(&mut caches);
        assert_ne!(after, before, "the change must show in the order");

        // Replaced by a new file (new inode, new birth time, executable).
        let tmp = live.base.join("files/n.tmp");
        fs::write(&tmp, b"new").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, live.path("f2")).unwrap();
        live.settle(|i| {
            i.find_child(Some(i.roots()[0]), b"files").is_some_and(|d| {
                i.find_child(Some(d), b"n.tmp").is_none() && i.find_child(Some(d), b"f2").is_some()
            })
        });
        live.check(&mut caches);

        // Renamed: the name decides the hidden attribute.
        fs::rename(live.path("f4"), live.path(".f4")).unwrap();
        let files = live.files();
        live.settle(|i| i.find_child(Some(files), b".f4").is_some());
        live.check(&mut caches);

        // Removed.
        fs::remove_file(live.path("f6")).unwrap();
        live.settle(|i| i.find_child(Some(files), b"f6").is_none());
        live.check(&mut caches);

        // New ids (compaction): the cached ids would name other nodes.
        assert!(!caches[1].is_empty());
        live.index.take_removed();
        live.index.compact_in_place();
        live.check(&mut caches);

        // A drive that is not connected is not read, cached or not.
        let root = live.index.roots()[0];
        live.index.set_offline(root, true);
        live.check(&mut caches);
        assert!(caches.iter().all(DetailCache::is_empty));
        // Changed while it was away, then back.
        fs::set_permissions(live.path("f3"), fs::Permissions::from_mode(0o444)).unwrap();
        live.index.set_offline(root, false);
        live.check(&mut caches);

        // Another generation (the engine's id epoch) forgets all values.
        assert!(!caches[1].is_empty());
        caches[1].set_generation(7);
        assert!(caches[1].is_empty());
        live.check(&mut caches);
    }

    #[test]
    fn parent_permission_change_drops_cached_details_below_it() {
        use std::os::unix::fs::PermissionsExt;
        let mut live = Live::new("parent", 12);
        let mut caches: [DetailCache; 3] = Default::default();
        live.check(&mut caches);
        assert!(!caches[1].is_empty());
        // Only the folder changes and only it has an event; the files below
        // it can no longer be read, so cached values must not be used.
        let files = live.files();
        let seq = live.index.change_seq();
        std::fs::set_permissions(
            live.base.join("files"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        live.settle(|i| i.changed_since(seq).is_none_or(|c| c.contains(&files)));
        live.check(&mut caches);
        // And back: readable again.
        let seq = live.index.change_seq();
        std::fs::set_permissions(
            live.base.join("files"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        live.settle(|i| i.changed_since(seq).is_none_or(|c| c.contains(&files)));
        live.check(&mut caches);
    }

    #[test]
    fn a_folder_moved_where_it_cannot_be_read_drops_cached_details_below_it() {
        use std::os::unix::fs::PermissionsExt;
        let mut live = Live::new("moved", 12);
        let locked = live.base.join("locked");
        std::fs::create_dir(&locked).unwrap();
        let root = live.index.roots()[0];
        live.settle(|i| i.find_child(Some(root), b"locked").is_some());
        let mut caches: [DetailCache; 3] = Default::default();
        live.check(&mut caches);
        assert!(!caches[1].is_empty());
        // Another user moves the folder into one this user cannot search.
        // Only the move reaches the index (the permission change of
        // `locked` is not applied), as when `locked` belongs to that user.
        std::fs::rename(live.base.join("files"), locked.join("files")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (files, into) = (
            live.files(),
            live.index.find_child(Some(root), b"locked").unwrap(),
        );
        live.index.rename(files, Some(into), b"files").unwrap();
        live.check(&mut caches);
    }

    #[test]
    fn repeated_sorts_read_only_what_changed() {
        let mut live = Live::new("reads", 40);
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let ids = search(&live.index, &all, 1);
        let n = ids.len() as u64;
        let mut cache = DetailCache::new();
        let reads = |index: &Index, key, ids: &[NodeId], cache: &mut DetailCache| {
            let s = Sort {
                key,
                ..Sort::default()
            };
            let before = detail_reads();
            let mut v = ids.to_vec();
            sort_cached(index, &mut v, s, 3, &Progress::new(), cache).unwrap();
            detail_reads() - before
        };
        assert_eq!(reads(&live.index, SortKey::Attributes, &ids, &mut cache), n);
        assert_eq!(reads(&live.index, SortKey::Attributes, &ids, &mut cache), 0);
        assert_eq!(cache.len() as u64, n);
        assert!(cache.heap_bytes() >= cache.len() * 9);
        // Access times change on every read without an event: always read.
        assert_eq!(reads(&live.index, SortKey::Accessed, &ids, &mut cache), n);
        assert_eq!(reads(&live.index, SortKey::Accessed, &ids, &mut cache), n);
        // Only the current key is kept.
        assert!(cache.is_empty());
        assert_eq!(reads(&live.index, SortKey::Created, &ids, &mut cache), n);
        assert_eq!(reads(&live.index, SortKey::Created, &ids, &mut cache), 0);

        // One changed node is read again, and only it.
        let files = live.files();
        let f7 = live.index.find_child(Some(files), b"f7").unwrap();
        live.index.note_changed(f7);
        assert_eq!(reads(&live.index, SortKey::Created, &ids, &mut cache), 1);

        // A much shorter result drops the values of nodes no longer listed.
        assert_eq!(reads(&live.index, SortKey::Created, &[f7], &mut cache), 0);
        assert_eq!(cache.len(), 1);
        assert_eq!(
            reads(&live.index, SortKey::Created, &ids, &mut cache),
            n - 1
        );

        // Entries that cannot be read are not kept: what keeps them from
        // being read may change without an event for them.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            live.base.join("files"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let unreadable = 40;
        live.index.note_changed(files);
        for &id in &ids {
            if live.index.store().parent(id) == Some(files) {
                live.index.note_changed(id);
            }
        }
        // `files` is read again and kept; the files below it cannot be read.
        assert_eq!(
            reads(&live.index, SortKey::Created, &ids, &mut cache),
            unreadable + 1
        );
        assert_eq!(
            reads(&live.index, SortKey::Created, &ids, &mut cache),
            unreadable
        );
        // Readable again: read once more, then kept.
        std::fs::set_permissions(
            live.base.join("files"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(
            reads(&live.index, SortKey::Created, &ids, &mut cache),
            unreadable
        );
        assert_eq!(reads(&live.index, SortKey::Created, &ids, &mut cache), 0);
    }

    /// Prints the time and file system reads of sorting 100 000 files by
    /// creation time and attributes, without and with a [`DetailCache`].
    /// The files are made in `$EVERYGREP_BENCH_DIR` (default `/var/tmp`) and
    /// removed afterwards. Run with
    /// `cargo test --release -p everygrep-core detail_cache_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing only"]
    fn detail_cache_timing() {
        let parent = std::env::var_os("EVERYGREP_BENCH_DIR")
            .map_or_else(|| std::path::PathBuf::from("/var/tmp"), Into::into);
        let base = parent.join(format!("everygrep-detail-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in 0..100 {
            let dir = base.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..1000 {
                std::fs::write(dir.join(format!("f{f}.txt")), b"").unwrap();
            }
            if d % 20 == 19 {
                eprintln!("made {} files", (d + 1) * 1000);
            }
        }
        let mut store = NodeStore::new();
        crate::scan::scan(&mut store, std::slice::from_ref(&base), &Default::default());
        let mut index = Index::new(store);
        let all = Query::parse("", QueryOptions::default()).unwrap();
        let ids = search(&index, &all, 1);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        eprintln!("{} results, {threads} threads", ids.len());
        let run = |index: &Index, key, cache: Option<&mut DetailCache>| {
            let s = Sort {
                key,
                ..Sort::default()
            };
            let mut v = ids.clone();
            let reads = detail_reads();
            let started = std::time::Instant::now();
            match cache {
                Some(c) => sort_cached(index, &mut v, s, threads, &Progress::new(), c).unwrap(),
                None => sort(index, &mut v, s, threads),
            }
            (
                started.elapsed().as_secs_f64() * 1e3,
                detail_reads() - reads,
            )
        };
        // User and system time of this process, in ms (from `/proc`, in
        // steps of 10 ms; averaged over many runs).
        let cpu_ms = || {
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let rest = &stat[stat.rfind(')').unwrap() + 2..];
            let f: Vec<u64> = rest
                .split(' ')
                .skip(11)
                .take(2)
                .map(|x| x.parse().unwrap())
                .collect();
            (f[0] + f[1]) as f64 * 10.0
        };
        const REPEATS: u32 = 20;
        for key in [SortKey::Created, SortKey::Attributes] {
            let mut caches = [None, Some(DetailCache::new())];
            for cache in &mut caches {
                let label = if cache.is_some() {
                    "with cache"
                } else {
                    "without cache"
                };
                let (ms, reads) = run(&index, key, cache.as_mut());
                eprintln!("{key:?} {label}, first: {ms:.1} ms, {reads} reads");
                let (mut total, mut all_reads, cpu) = (0.0, 0, cpu_ms());
                for _ in 0..REPEATS {
                    let (ms, reads) = run(&index, key, cache.as_mut());
                    total += ms;
                    all_reads += reads;
                }
                let n = f64::from(REPEATS);
                eprintln!(
                    "{key:?} {label}, repeated (mean of {REPEATS}): {:.1} ms, {:.0} ms CPU, {} reads",
                    total / n,
                    (cpu_ms() - cpu) / n,
                    all_reads / u64::from(REPEATS)
                );
            }
            let mut cache = caches[1].take().unwrap();
            eprintln!(
                "  cache: {} values, {} KiB",
                cache.len(),
                cache.heap_bytes() / 1024
            );
            // A refresh after 1000 changed files.
            for &id in ids.iter().step_by(ids.len() / 1000) {
                index.note_changed(id);
            }
            let (ms, reads) = run(&index, key, Some(&mut cache));
            eprintln!("{key:?} with cache, after ~1000 changes: {ms:.1} ms, {reads} reads");
        }
        std::fs::remove_dir_all(&base).unwrap();
        eprintln!("removed {}", base.display());
    }
}
