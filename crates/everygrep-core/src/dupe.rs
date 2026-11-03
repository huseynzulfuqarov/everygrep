//! Finding duplicates among the results of a query (`duplicates:` and its
//! relatives, see [`DupeSpec`]).
//!
//! Results are grouped by the values of one or more properties: files and
//! folders that agree on all of them form a group. Grouping sorts the
//! results by those values and compares neighbours, so it costs one sort.
//! Files and folders never share a group.
//!
//! The property [`DupeProperty::Contents`] (the default) compares what the
//! files hold, in stages that read as little as possible:
//!
//! 1. with the index ([`group`]): files of the same size form candidate
//!    groups; a file whose size no other file has is unique without being
//!    read. Folders are left out; zero indexed sizes are treated as unknown;
//! 2. without the index ([`compare_contents`]), on worker threads: every
//!    candidate is looked at on disk (`lstat` through
//!    [`crate::hash::open`]): what is not a readable regular file any more
//!    is left out, as are empty files, and of the paths of one file (hard links, the same device
//!    and inode) only the first counts, so a file is never its own
//!    duplicate. Groups are split again by the size on disk;
//! 3. the first and last 16 KiB of each file are hashed (all of a file of
//!    up to 32 KiB) and the groups split by that hash;
//! 4. files still in a group and larger than 32 KiB are hashed whole
//!    (XXH3-128); files with equal hashes have the same contents.
//!
//! Hashes come from a [`HashCache`] when one is given, and are kept there.
//!
//! What a query lists then depends on the [`DupeMode`]; the members of a
//! group stay together, in the order of the results ([`Dupes::arrange`]).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use crate::hash::{self, END, FileKey, HashError, Wanted};
use crate::hashcache::{Cached, HashCache};
use crate::index::Index;
use crate::progress::{Cancelled, Progress};
use crate::sort::extension;
use crate::store::{NodeId, NodeStore};

/// Most properties one duplicate term compares.
pub const MAX_PROPERTIES: usize = 8;

/// What a duplicate search lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DupeMode {
    /// Every member of every group (`duplicates:`).
    #[default]
    All,
    /// Every member but the first of each group: the extra copies
    /// (`extra-copies:`).
    Extra,
    /// One of each value: the first of each group and every entry without
    /// a duplicate (`distinct:`).
    Distinct,
    /// Only the entries without a duplicate (`unique:`).
    Unique,
}

/// A property that duplicates agree on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DupeProperty {
    /// The bytes of the file (files only).
    Contents,
    /// The whole name.
    Name,
    /// The name without its extension.
    Stem,
    /// The extension (files only).
    Extension,
    /// Size in bytes (files only).
    Size,
    /// Modification time.
    Modified,
}

impl DupeProperty {
    /// The property a word of a duplicate term names.
    pub fn from_word(word: &str) -> Option<Self> {
        Some(match word.to_ascii_lowercase().as_str() {
            "content" | "contents" => Self::Contents,
            "name" => Self::Name,
            "stem" => Self::Stem,
            "ext" | "extension" => Self::Extension,
            "size" => Self::Size,
            "modified" | "date-modified" => Self::Modified,
            _ => return None,
        })
    }

    fn files_only(self) -> bool {
        matches!(self, Self::Contents | Self::Extension | Self::Size)
    }
}

/// A duplicate term of a query: the properties to compare, how names
/// compare, and what to list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DupeSpec {
    pub mode: DupeMode,
    /// At least one, at most [`MAX_PROPERTIES`], each once.
    pub properties: Vec<DupeProperty>,
    /// Names, stems and extensions compare exactly instead of ignoring
    /// case.
    pub case: bool,
}

impl DupeSpec {
    /// Whether files are read: the contents are compared.
    pub fn reads_files(&self) -> bool {
        self.properties.contains(&DupeProperty::Contents)
    }

    /// Whether folders are left out.
    fn files_only(&self) -> bool {
        self.properties.iter().any(|p| p.files_only())
    }

    /// Orders two entries by the values of the properties (and files after
    /// folders). The contents count as the size here.
    fn compare(&self, store: &NodeStore, a: NodeId, b: NodeId) -> Ordering {
        let mut order = store.is_dir(a).cmp(&store.is_dir(b));
        for p in &self.properties {
            if order != Ordering::Equal {
                break;
            }
            order = match p {
                DupeProperty::Contents | DupeProperty::Size => {
                    store.meta(a).size.cmp(&store.meta(b).size)
                }
                DupeProperty::Modified => store.meta(a).mtime_ns.cmp(&store.meta(b).mtime_ns),
                DupeProperty::Name => self.name(store, a).cmp(self.name(store, b)),
                DupeProperty::Stem => stem(self.name(store, a)).cmp(stem(self.name(store, b))),
                DupeProperty::Extension if self.case => {
                    raw_extension(store.name(a)).cmp(raw_extension(store.name(b)))
                }
                DupeProperty::Extension => extension(store, a).cmp(extension(store, b)),
            };
        }
        order
    }

    fn name<'a>(&self, store: &'a NodeStore, id: NodeId) -> &'a [u8] {
        if self.case {
            store.name(id)
        } else {
            store.folded_name(id)
        }
    }
}

/// The name before its last dot; a dot at the very start does not count.
fn stem(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// The extension of a name as written, without the dot.
fn raw_extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => &[],
    }
}

/// The result of a duplicate search: groups of entries that agree, and
/// the entries without a duplicate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dupes {
    mode: DupeMode,
    /// Every entry that may be listed, in the order of the results given.
    listed: Vec<NodeId>,
    /// The group of each entry that has duplicates.
    group_of: HashMap<NodeId, u32>,
    groups: usize,
}

impl Dupes {
    fn new(mode: DupeMode, listed: Vec<NodeId>, groups: &[Vec<NodeId>]) -> Self {
        let mut group_of = HashMap::new();
        for (g, members) in groups.iter().enumerate() {
            for &id in members {
                group_of.insert(id, g as u32);
            }
        }
        Self {
            mode,
            listed,
            group_of,
            groups: groups.len(),
        }
    }

    /// Number of groups of duplicates.
    pub fn groups(&self) -> usize {
        self.groups
    }

    /// The entries the mode may list, in the order of the results given
    /// (name order for a search), to be sorted and then given to
    /// [`arrange`](Self::arrange).
    pub fn to_sort(&self) -> Vec<NodeId> {
        self.listed
            .iter()
            .copied()
            .filter(|id| {
                let grouped = self.group_of.contains_key(id);
                match self.mode {
                    DupeMode::All | DupeMode::Extra => grouped,
                    DupeMode::Distinct => true,
                    DupeMode::Unique => !grouped,
                }
            })
            .collect()
    }

    /// The listed entries from `sorted` (the result of
    /// [`to_sort`](Self::to_sort) in the order to show): the members of a
    /// group together, where its first member is, in their order; for
    /// [`DupeMode::Extra`] without the first, for [`DupeMode::Distinct`]
    /// only the first.
    pub fn arrange(&self, sorted: &[NodeId]) -> Vec<NodeId> {
        let mut members: Vec<Vec<NodeId>> = vec![Vec::new(); self.groups];
        for id in sorted {
            if let Some(&g) = self.group_of.get(id) {
                members[g as usize].push(*id);
            }
        }
        let mut out = Vec::with_capacity(sorted.len());
        let mut placed = vec![false; self.groups];
        for id in sorted {
            match self.group_of.get(id) {
                Some(&g) => {
                    let g = g as usize;
                    if std::mem::replace(&mut placed[g], true) {
                        continue;
                    }
                    let group = &members[g];
                    match self.mode {
                        DupeMode::All => out.extend_from_slice(group),
                        DupeMode::Extra => out.extend_from_slice(&group[1..]),
                        DupeMode::Distinct => out.push(group[0]),
                        DupeMode::Unique => {}
                    }
                }
                None if matches!(self.mode, DupeMode::Distinct | DupeMode::Unique) => out.push(*id),
                None => {}
            }
        }
        out
    }
}

/// Files of the same size whose contents decide, taken from the index so
/// they can be read without it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Candidates {
    groups: Vec<Vec<(NodeId, Vec<u8>)>>,
}

impl Candidates {
    /// Number of files to look at.
    pub fn files(&self) -> usize {
        self.groups.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// The first stage of a duplicate search, with the index (see the module
/// notes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grouped {
    /// Without contents to compare: the answer. With them: the entries
    /// without a duplicate so far, which the comparison adds to.
    pub dupes: Dupes,
    /// The files whose contents decide; empty without contents to compare.
    pub candidates: Candidates,
}

/// Groups `ids` (results in name order) of `index` by the properties of
/// `spec`. The contents are compared by [`compare_contents`] afterwards.
pub fn group(index: &Index, ids: &[NodeId], spec: &DupeSpec) -> Grouped {
    let store = index.store();
    let files_only = spec.files_only();
    let contents = spec.reads_files();
    let kept: Vec<NodeId> = ids
        .iter()
        .copied()
        .filter(|&id| !(files_only && store.is_dir(id)))
        .collect();
    // Positions keep the order of the results within a group.
    let mut order: Vec<u32> = (0..kept.len() as u32).collect();
    order.sort_unstable_by(|&a, &b| {
        spec.compare(store, kept[a as usize], kept[b as usize])
            .then(a.cmp(&b))
    });
    let mut groups: Vec<Vec<NodeId>> = Vec::new();
    let mut start = 0;
    while start < order.len() {
        let first = kept[order[start] as usize];
        let mut end = start + 1;
        while end < order.len()
            && spec.compare(store, first, kept[order[end] as usize]) == Ordering::Equal
        {
            end += 1;
        }
        // Zero may mean metadata was not indexed. Include even a singleton
        // so the file stage can distinguish a real empty file from unknown
        // size without doing disk I/O under the index lock.
        if end - start > 1 || (contents && store.meta(first).size == 0) {
            groups.push(
                order[start..end]
                    .iter()
                    .map(|&p| kept[p as usize])
                    .collect(),
            );
        }
        start = end;
    }
    if !contents {
        return Grouped {
            dupes: Dupes::new(spec.mode, kept, &groups),
            candidates: Candidates::default(),
        };
    }
    let candidates = Candidates {
        groups: groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|&id| (id, store.path(id).into_os_string().into_vec()))
                    .collect()
            })
            .collect(),
    };
    Grouped {
        // No group is known yet; the comparison makes them.
        dupes: Dupes::new(spec.mode, kept, &[]),
        candidates,
    }
}

/// A candidate file while it is compared: its place in the candidates, its
/// key on disk and the hash of the stage it is in.
#[derive(Clone, Copy)]
struct Looked {
    group: usize,
    member: usize,
    key: FileKey,
    hash: u128,
}

/// The second stage of a duplicate search, without the index: compares
/// the contents of `grouped`'s candidates on `threads` threads and returns
/// the answer. A candidate that is not a readable regular file any more,
/// changed while it was compared, or is another path of a file already
/// looked at (a hard link) is not listed; one whose contents no other has
/// is unique. `progress` gets a total of three steps per file and counts
/// them; a cancel is seen before each file and each buffer read.
pub fn compare_contents(
    grouped: Grouped,
    cache: Option<&Mutex<HashCache>>,
    threads: usize,
    progress: &Progress,
) -> Result<Dupes, Cancelled> {
    let Grouped { dupes, candidates } = grouped;
    progress.restart(Some(candidates.files() as u64 * 3));
    let id_of = |l: &Looked| candidates.groups[l.group][l.member].0;
    let path_of =
        |l: &Looked| Path::new(OsStr::from_bytes(&candidates.groups[l.group][l.member].1));
    let mut dropped: HashSet<NodeId> = HashSet::new();

    // On disk: readable regular files, one path per file, by real size.
    let all: Vec<Looked> = candidates
        .groups
        .iter()
        .enumerate()
        .flat_map(|(g, members)| {
            (0..members.len()).map(move |m| Looked {
                group: g,
                member: m,
                key: FileKey::default(),
                hash: 0,
            })
        })
        .collect();
    let opened = par_map(&all, threads, progress, |l| {
        let key = hash::open(path_of(l))?.key;
        if key.size == 0 {
            return Err(HashError::NotAFile);
        }
        Ok(Looked { key, ..*l })
    })?;
    // Per group: hard links with other names (another group) are no
    // duplicates of each other there either.
    let mut inodes: HashSet<(usize, u64, u64)> = HashSet::new();
    let mut looked = Vec::with_capacity(all.len());
    for (l, found) in all.iter().zip(opened) {
        match found {
            Some(found) if inodes.insert((l.group, found.key.dev, found.key.ino)) => {
                looked.push(found);
            }
            _ => {
                dropped.insert(id_of(l));
                progress.add(3);
            }
        }
    }
    progress.add(looked.len() as u64);
    let looked = split(looked, |l| (l.group, l.key.size), progress, 2);

    // The ends.
    let ends = par_map(&looked, threads, progress, |l| {
        let hash = match cached(cache, &l.key, |c| c.ends) {
            Some(hash) => hash,
            None => {
                let hash = hash::ends_hash(&reopen(path_of(l), &l.key)?, progress)?;
                let hashes = Cached {
                    ends: Some(hash),
                    ..Cached::default()
                };
                put(cache, l.key, &hashes);
                hash
            }
        };
        Ok(Looked { hash, ..*l })
    })?;
    let looked = settle(&looked, ends, progress, 2, |l| dropped.insert(id_of(l)));
    progress.add(looked.len() as u64);
    let looked = split(looked, |l| (l.group, l.key.size, l.hash), progress, 1);

    // The whole of larger files; for files up to twice END the ends hash
    // was of the whole file.
    let (small, large): (Vec<Looked>, Vec<Looked>) =
        looked.into_iter().partition(|l| l.key.size <= 2 * END);
    progress.add(small.len() as u64);
    let full = par_map(&large, threads, progress, |l| {
        let hash = match cached(cache, &l.key, |c| c.full) {
            Some(hash) => hash,
            None => {
                let wanted = Wanted {
                    full: true,
                    ..Wanted::default()
                };
                let mut file = reopen(path_of(l), &l.key)?;
                let hash = hash::hash_file(&mut file, wanted, progress)?
                    .full
                    .unwrap_or_default();
                let hashes = Cached {
                    full: Some(hash),
                    ..Cached::default()
                };
                put(cache, l.key, &hashes);
                hash
            }
        };
        Ok(Looked { hash, ..*l })
    })?;
    let large = settle(&large, full, progress, 1, |l| dropped.insert(id_of(l)));
    progress.add(large.len() as u64);

    let mut decided: Vec<(usize, u64, bool, u128, usize)> = small
        .iter()
        .map(|l| (l.group, l.key.size, false, l.hash, l.member))
        .chain(
            large
                .iter()
                .map(|l| (l.group, l.key.size, true, l.hash, l.member)),
        )
        .collect();
    decided.sort_unstable();
    let groups: Vec<Vec<NodeId>> = decided
        .chunk_by(|a, b| (a.0, a.1, a.2, a.3) == (b.0, b.1, b.2, b.3))
        .filter(|run| run.len() > 1)
        .map(|run| run.iter().map(|r| candidates.groups[r.0][r.4].0).collect())
        .collect();
    let listed = dupes
        .listed
        .into_iter()
        .filter(|id| !dropped.contains(id))
        .collect();
    progress.check()?;
    Ok(Dupes::new(dupes.mode, listed, &groups))
}

/// The hash `pick` takes from the cache entry of `key`.
fn cached(
    cache: Option<&Mutex<HashCache>>,
    key: &FileKey,
    pick: impl Fn(&Cached) -> Option<u128>,
) -> Option<u128> {
    let mut cache = cache?.lock().expect("hash cache");
    cache.get(key).as_ref().and_then(pick)
}

fn put(cache: Option<&Mutex<HashCache>>, key: FileKey, hashes: &Cached) {
    if let Some(cache) = cache {
        cache.lock().expect("hash cache").put(key, hashes);
    }
}

/// Opens a file again for reading, when it is still the file looked at.
fn reopen(path: &Path, key: &FileKey) -> Result<hash::OpenFile, HashError> {
    let file = hash::open(path)?;
    if file.key != *key {
        return Err(HashError::Unreadable);
    }
    Ok(file)
}

/// Splits `looked` into groups by `key` and keeps the members of groups of
/// two or more; the others are decided, and their remaining `steps` are
/// counted done.
fn split<K: Ord>(
    mut looked: Vec<Looked>,
    key: impl Fn(&Looked) -> K,
    progress: &Progress,
    steps: u64,
) -> Vec<Looked> {
    looked.sort_by(|a, b| {
        key(a)
            .cmp(&key(b))
            .then((a.group, a.member).cmp(&(b.group, b.member)))
    });
    let mut out = Vec::with_capacity(looked.len());
    for run in looked.chunk_by(|a, b| key(a) == key(b)) {
        if run.len() > 1 {
            out.extend_from_slice(run);
        } else {
            progress.add(steps);
        }
    }
    out
}

/// The files of a stage that could be hashed (`results` of `looked`); the
/// others are left out, given to `drop`, and their remaining `steps`
/// counted done.
fn settle(
    looked: &[Looked],
    results: Vec<Option<Looked>>,
    progress: &Progress,
    steps: u64,
    mut drop: impl FnMut(&Looked) -> bool,
) -> Vec<Looked> {
    let mut out = Vec::with_capacity(results.len());
    for (l, r) in looked.iter().zip(results) {
        match r {
            Some(l) => out.push(l),
            None => {
                drop(l);
                progress.add(steps);
            }
        }
    }
    out
}

/// Runs `f` for every item on `threads` threads: `None` for an item that
/// could not be read, and [`Cancelled`] when `progress` was cancelled.
fn par_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    progress: &Progress,
    f: impl Fn(&T) -> Result<R, HashError> + Sync,
) -> Result<Vec<Option<R>>, Cancelled> {
    let next = AtomicUsize::new(0);
    let threads = threads.clamp(1, items.len().max(1));
    let parts: Mutex<Vec<(usize, Option<R>)>> = Mutex::new(Vec::with_capacity(items.len()));
    let work = || {
        let mut mine = Vec::new();
        loop {
            let i = next.fetch_add(1, AtomicOrdering::Relaxed);
            let Some(item) = items.get(i) else { break };
            if progress.is_cancelled() {
                break;
            }
            match f(item) {
                Ok(r) => mine.push((i, Some(r))),
                Err(HashError::Cancelled) => break,
                Err(_) => mine.push((i, None)),
            }
        }
        parts.lock().expect("parts").extend(mine);
    };
    if threads == 1 {
        work();
    } else {
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(work);
            }
        });
    }
    progress.check()?;
    let mut parts = parts.into_inner().expect("parts");
    parts.sort_unstable_by_key(|(i, _)| *i);
    Ok(parts.into_iter().map(|(_, r)| r).collect())
}

/// Names of the duplicate terms, lowercase.
const TERMS: [&str; 4] = ["duplicates", "extra-copies", "distinct", "unique"];

/// `query` with its duplicate terms (with any modifiers stacked before
/// them, as in `case:duplicates:name`) taken out and `term` added at the end, for
/// the window's Find duplicates commands. Words in double quotes are left
/// alone, and so are all other words and the spacing, except that the
/// spaces before a removed word go with it (or after it, at the start).
pub fn replace_term(query: &str, term: Option<&str>) -> String {
    let mut out = String::with_capacity(query.len() + term.map_or(0, str::len) + 1);
    let mut rest = query;
    let mut removed = false;
    while !rest.is_empty() {
        let blank = rest.len() - rest.trim_start().len();
        let (space, tail) = rest.split_at(blank);
        let mut quoted = false;
        let end = tail
            .char_indices()
            .find(|&(_, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                c.is_whitespace() && !quoted
            })
            .map_or(tail.len(), |(i, _)| i);
        let (word, after) = tail.split_at(end);
        let mut parts: Vec<&str> = word.split(':').collect();
        parts.pop();
        let is_dupe = !word.starts_with('"')
            && parts
                .iter()
                .any(|p| TERMS.contains(&p.to_ascii_lowercase().as_str()));
        if is_dupe {
            removed = true;
        } else {
            // A word that is first now loses the spaces before it.
            if !(removed && out.is_empty()) {
                out.push_str(space);
            }
            out.push_str(word);
        }
        rest = after;
    }
    if let Some(term) = term {
        let kept = out.trim_end().len();
        out.truncate(kept);
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(term);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Query, QueryOptions};
    use crate::scan::{ScanOptions, scan};
    use crate::search::search;
    use std::path::PathBuf;

    struct Tree {
        dir: PathBuf,
        index: Index,
    }

    impl Tree {
        fn new(name: &str, files: &[(&str, &[u8])]) -> Self {
            let dir =
                std::env::temp_dir().join(format!("everygrep-dupe-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for (path, bytes) in files {
                let path = dir.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
            let mut tree = Self {
                dir,
                index: Index::new(NodeStore::new()),
            };
            tree.rescan();
            tree
        }

        fn rescan(&mut self) {
            let mut store = NodeStore::new();
            scan(
                &mut store,
                std::slice::from_ref(&self.dir),
                &ScanOptions::default(),
            );
            self.index = Index::new(store);
        }

        /// Paths below the tree, in the order of the results.
        fn paths(&self, q: &str) -> Vec<String> {
            let query = Query::parse(q, QueryOptions::default()).unwrap();
            self.show(&search(&self.index, &query, 3))
        }

        fn show(&self, ids: &[NodeId]) -> Vec<String> {
            ids.iter()
                .map(|&id| {
                    let path = self.index.store().path(id);
                    path.strip_prefix(&self.dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn spec(q: &str) -> DupeSpec {
        Query::parse(q, QueryOptions::default())
            .unwrap()
            .dupes()
            .cloned()
            .unwrap()
    }

    fn big(seed: u8, middle: u8) -> Vec<u8> {
        let mut v: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8 ^ seed).collect();
        v[50_000] = middle;
        v
    }

    #[test]
    fn content_duplicates_work_without_indexed_file_details() {
        let mut tree = Tree::new(
            "no-details",
            &[
                ("a", b"same"),
                ("b", b"same"),
                ("c", b"unique"),
                ("empty", b""),
            ],
        );
        let mut store = NodeStore::new();
        scan(
            &mut store,
            std::slice::from_ref(&tree.dir),
            &ScanOptions {
                with_metadata: false,
                ..ScanOptions::default()
            },
        );
        tree.index = Index::new(store);
        assert_eq!(tree.paths("duplicates:"), ["a", "b"]);
        assert_eq!(tree.paths("extra-copies:"), ["b"]);
        assert_eq!(tree.paths("distinct:"), ["a", "c"]);
        assert_eq!(tree.paths("unique:"), ["c"]);
        // A sole zero-sized indexed entry must still be checked for emptiness.
        let empty = tree
            .index
            .order()
            .iter()
            .filter(|&id| tree.index.store().name(id) == b"empty")
            .collect::<Vec<_>>();
        let spec = Query::parse("unique:", QueryOptions::default()).unwrap();
        let found = compare_contents(
            group(&tree.index, &empty, spec.dupes().unwrap()),
            None,
            1,
            &Progress::new(),
        )
        .unwrap();
        assert!(found.to_sort().is_empty());
    }

    #[test]
    fn a_duplicate_term_replaces_the_one_in_the_query() {
        let set = |q: &str, t: &str| replace_term(q, Some(t));
        assert_eq!(set("", "duplicates:"), "duplicates:");
        assert_eq!(set("ext:jpg", "duplicates:"), "ext:jpg duplicates:");
        assert_eq!(
            set("ext:jpg duplicates:name  ~/Pictures", "duplicates:size"),
            "ext:jpg  ~/Pictures duplicates:size"
        );
        assert_eq!(
            set("case:duplicates:name a UNIQUE: b", "duplicates:"),
            "a b duplicates:"
        );
        assert_eq!(set("distinct:size;name|x", "duplicates:"), "duplicates:");
        assert_eq!(
            set("extra-copies: report  ", "duplicates:name"),
            "report duplicates:name"
        );
        // Text in quotes and words that only look alike stay.
        assert_eq!(
            set("\"duplicates: x\" dupes", "duplicates:"),
            "\"duplicates: x\" dupes duplicates:"
        );
        assert_eq!(
            set("content:\"a duplicates:\"", "duplicates:"),
            "content:\"a duplicates:\" duplicates:"
        );
        assert_eq!(replace_term("pdf duplicates:size  ", None), "pdf  ");
        assert_eq!(replace_term("duplicates:", None), "");
    }

    #[test]
    fn terms_parse_with_their_mode_properties_and_case() {
        let s = spec("duplicates:");
        assert_eq!(
            (s.mode, s.properties.as_slice(), s.case),
            (DupeMode::All, &[DupeProperty::Contents][..], false)
        );
        let s = spec("case:extra-copies:Name;SIZE,name ext:jpg");
        assert_eq!(s.mode, DupeMode::Extra);
        assert_eq!(s.properties, [DupeProperty::Name, DupeProperty::Size]);
        assert!(s.case);
        assert_eq!(spec("distinct:stem").mode, DupeMode::Distinct);
        assert_eq!(
            spec("unique:date-modified").properties,
            [DupeProperty::Modified]
        );
        assert_eq!(
            spec("<a|duplicates:ext>").properties,
            [DupeProperty::Extension]
        );
        let err = |q: &str| Query::parse(q, QueryOptions::default()).unwrap_err().0;
        assert!(err("duplicates:colour").contains("unknown property for duplicates: colour"));
        assert!(err("duplicates: unique:").contains("only one of"));
        assert!(
            err("duplicates:name;size;ext;stem;modified;content;name;x")
                .contains("unknown property")
        );
        // Without a duplicate term there is none.
        assert!(
            Query::parse("dupes", QueryOptions::default())
                .unwrap()
                .dupes()
                .is_none()
        );
    }

    fn contents_tree(name: &str) -> Tree {
        Tree::new(
            name,
            &[
                ("a.txt", b"same"),
                ("sub/a.txt", b"same"),
                ("d.dat", b"same"),
                ("diff.txt", b"diff"),
                ("lonely.txt", b"only one of this size"),
                ("e1", b""),
                ("e2", b""),
                ("big1.bin", &big(0, 1)),
                ("big2.bin", &big(0, 1)),
                // Same size and ends as big1, another middle.
                ("big3.bin", &big(0, 2)),
            ],
        )
    }

    #[test]
    fn same_contents_by_size_ends_and_whole_file_in_every_mode() {
        let tree = contents_tree("contents");
        // Groups stay together where their first member is; empty files and
        // folders are left out.
        assert_eq!(
            tree.paths("duplicates:"),
            ["a.txt", "sub/a.txt", "d.dat", "big1.bin", "big2.bin"]
        );
        assert_eq!(
            tree.paths("extra-copies:"),
            ["sub/a.txt", "d.dat", "big2.bin"]
        );
        assert_eq!(
            tree.paths("distinct:"),
            ["a.txt", "big1.bin", "big3.bin", "diff.txt", "lonely.txt"]
        );
        assert_eq!(
            tree.paths("unique:"),
            ["big3.bin", "diff.txt", "lonely.txt"]
        );
        // With other terms: only the matches are compared.
        assert_eq!(tree.paths("duplicates: ext:txt"), ["a.txt", "sub/a.txt"]);
        assert_eq!(
            tree.paths("duplicates: !dat"),
            ["a.txt", "sub/a.txt", "big1.bin", "big2.bin"]
        );
        // Contents and name together.
        assert_eq!(
            tree.paths("duplicates:content;name"),
            ["a.txt", "sub/a.txt"]
        );
        assert!(tree.paths("duplicates: ext:bin big3").is_empty());
    }

    #[test]
    fn hard_links_are_one_file_and_unreadable_files_are_left_out() {
        let tree = Tree::new(
            "links",
            &[
                ("one", b"1234"),
                ("copy", b"1234"),
                ("solo", b"abcdefgh"),
                ("gone", b"abcdefgh"),
            ],
        );
        std::fs::hard_link(tree.dir.join("one"), tree.dir.join("one-link")).unwrap();
        std::fs::hard_link(tree.dir.join("solo"), tree.dir.join("solo-link")).unwrap();
        let mut tree = tree;
        tree.rescan();
        std::fs::remove_file(tree.dir.join("gone")).unwrap();
        // `one` and its link are one file: the first path is shown, with
        // the real copy. `solo` and its link are no duplicates.
        assert_eq!(tree.paths("duplicates:"), ["copy", "one"]);
        assert_eq!(tree.paths("unique:"), ["solo"]);
        // By name or size nothing is read: the links count.
        assert_eq!(
            tree.paths("duplicates:size"),
            ["copy", "one", "one-link", "gone", "solo", "solo-link"]
        );
    }

    #[test]
    fn properties_from_the_index_without_reading() {
        let tree = Tree::new(
            "props",
            &[
                ("Read.me", b"1"),
                ("x/read.me", b"22"),
                ("y/read.ME", b"333"),
                ("photo.JPG", b"4444"),
                ("photo.png", b"55555"),
                ("z/photo.jpg", b"666666"),
                ("x/readme", b"7"),
                ("y/x/keep", b"8"),
            ],
        );
        assert_eq!(
            tree.paths("duplicates:name file:"),
            [
                "photo.JPG",
                "z/photo.jpg",
                "Read.me",
                "y/read.ME",
                "x/read.me"
            ]
        );
        assert_eq!(
            tree.paths("case:duplicates:name file:"),
            Vec::<String>::new()
        );
        // Folders form their own groups: `x` twice.
        assert_eq!(tree.paths("duplicates:name folder:"), ["x", "y/x"]);
        assert_eq!(tree.paths("duplicates:name x"), ["x", "y/x"]);
        assert_eq!(
            tree.paths("duplicates:stem photo"),
            ["photo.JPG", "z/photo.jpg", "photo.png"]
        );
        assert_eq!(
            tree.paths("case:duplicates:ext photo"),
            Vec::<String>::new()
        );
        assert_eq!(
            tree.paths("duplicates:ext photo"),
            ["photo.JPG", "z/photo.jpg"]
        );
        assert_eq!(
            tree.paths("duplicates:stem;ext photo"),
            ["photo.JPG", "z/photo.jpg"]
        );
        // Size: files only; 1-byte files `Read.me`, `readme`, `keep`.
        assert_eq!(
            tree.paths("duplicates:size"),
            ["y/x/keep", "Read.me", "x/readme"]
        );
        // `limit:` keeps the first after grouping (callers apply it).
        let query = Query::parse("duplicates:size limit:2", QueryOptions::default()).unwrap();
        let mut ids = search(&tree.index, &query, 2);
        query.apply_limit(&mut ids);
        assert_eq!(tree.show(&ids), ["y/x/keep", "Read.me"]);
    }

    #[test]
    fn groups_follow_the_order_given_and_stay_together() {
        let ids: Vec<NodeId> = (0..6).map(NodeId).collect();
        // Groups {1, 4} and {2, 3, 5}; 0 alone.
        let groups = vec![vec![ids[1], ids[4]], vec![ids[2], ids[3], ids[5]]];
        let mut dupes = Dupes::new(DupeMode::All, ids.clone(), &groups);
        assert_eq!(dupes.groups(), 2);
        assert_eq!(dupes.to_sort(), [1, 2, 3, 4, 5].map(NodeId));
        // Another order: 5 first brings its group, in that order.
        let sorted = [5, 4, 3, 2, 1].map(NodeId);
        assert_eq!(dupes.arrange(&sorted), [5, 3, 2, 4, 1].map(NodeId));
        dupes.mode = DupeMode::Extra;
        assert_eq!(dupes.arrange(&sorted), [3, 2, 1].map(NodeId));
        dupes.mode = DupeMode::Distinct;
        assert_eq!(dupes.to_sort(), ids);
        assert_eq!(
            dupes.arrange(&[0, 5, 4, 3, 2, 1].map(NodeId)),
            [0, 5, 4].map(NodeId)
        );
        dupes.mode = DupeMode::Unique;
        assert_eq!(dupes.to_sort(), [NodeId(0)]);
        assert_eq!(dupes.arrange(&[NodeId(0)]), [NodeId(0)]);
    }

    fn compare(tree: &Tree, q: &str, cache: &Mutex<HashCache>) -> Vec<String> {
        let ids = search(
            &tree.index,
            &Query::parse("", QueryOptions::default()).unwrap(),
            2,
        );
        let grouped = group(&tree.index, &ids, &spec(q));
        let dupes = compare_contents(grouped, Some(cache), 2, &Progress::new()).unwrap();
        tree.show(&dupes.arrange(&dupes.to_sort()))
    }

    #[test]
    fn hashes_are_cached_and_a_changed_file_is_read_again() {
        let mut tree = contents_tree("cache");
        let cache = Mutex::new(HashCache::default());
        let first = compare(&tree, "duplicates:", &cache);
        assert_eq!(
            first,
            ["a.txt", "sub/a.txt", "d.dat", "big1.bin", "big2.bin"]
        );
        // Ends hashes for the 3 + 1 small files of size 4 and the three big
        // ones, full hashes for big1 to big3.
        let len = cache.lock().unwrap().len();
        assert_eq!(len, 7);
        assert_eq!(compare(&tree, "duplicates:", &cache), first);
        assert_eq!(cache.lock().unwrap().len(), len);
        // Same size, other contents, a newer time: hashed again.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(tree.dir.join("d.dat"), b"diff").unwrap();
        tree.rescan();
        assert_eq!(
            compare(&tree, "duplicates:", &cache),
            [
                "a.txt",
                "sub/a.txt",
                "big1.bin",
                "big2.bin",
                "d.dat",
                "diff.txt"
            ]
        );
    }

    #[test]
    fn progress_counts_three_steps_per_file_and_a_cancel_stops() {
        let tree = contents_tree("progress");
        let ids = search(
            &tree.index,
            &Query::parse("", QueryOptions::default()).unwrap(),
            2,
        );
        let grouped = group(&tree.index, &ids, &spec("duplicates:"));
        // Size 4: a, sub/a, d, diff; size 100 000: three big ones;
        // two indexed zero sizes need a live check before being excluded.
        assert_eq!(grouped.candidates.files(), 9);
        let progress = Progress::new();
        compare_contents(grouped.clone(), None, 2, &progress).unwrap();
        assert_eq!((progress.done(), progress.total()), (27, Some(27)));
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(
            compare_contents(grouped, None, 2, &progress),
            Err(Cancelled)
        );
    }

    /// Hash speed per algorithm, a duplicate search over a tree cold, warm
    /// and with the hash cache, how soon a cancel is seen, grouping and
    /// frequency counts over a large index, and the size of the cache file.
    /// Writes about 1.4 GB below `EVERYGREP_TIMING_DIR` (default: the temp
    /// folder; use a disk, not tmpfs) and removes it at the end.
    #[test]
    #[ignore = "timing; run with --ignored --nocapture"]
    fn dupe_timing() {
        use crate::store::Meta;
        use std::io::Write;
        use std::time::Instant;

        let base = std::env::var_os("EVERYGREP_TIMING_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("everygrep-dupe-timing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
        let uncache = |dir: &Path| {
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).unwrap().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if let Ok(f) = std::fs::File::open(&p) {
                        let _ = rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::DontNeed);
                    }
                }
            }
        };
        let noise = |seed: u64, len: usize| -> Vec<u8> {
            let mut h = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            (0..len)
                .map(|_| {
                    h ^= h << 13;
                    h ^= h >> 7;
                    h ^= h << 17;
                    h as u8
                })
                .collect()
        };

        // 1. Hash speed, one thread, warm.
        let big = base.join("big.bin");
        let chunk = noise(1, 1 << 20);
        let mut f = std::fs::File::create(&big).unwrap();
        for _ in 0..512 {
            f.write_all(&chunk).unwrap();
        }
        drop(f);
        let all = |md5, sha256, crc32, full| Wanted {
            md5,
            sha256,
            crc32,
            full,
        };
        let mb = 512.0;
        for (name, wanted) in [
            ("warm-up", all(false, false, true, false)),
            ("xxh3-128", all(false, false, false, true)),
            ("crc32", all(false, false, true, false)),
            ("md5", all(true, false, false, false)),
            ("sha256", all(false, true, false, false)),
            ("md5+sha256+crc32", all(true, true, true, false)),
        ] {
            let t = Instant::now();
            let mut file = hash::open(&big).unwrap();
            hash::hash_file(&mut file, wanted, &Progress::new()).unwrap();
            let took = ms(t);
            println!(
                "hash {name}: 512 MiB in {took:.0} ms = {:.0} MiB/s",
                mb / took * 1e3
            );
        }
        std::fs::remove_file(&big).unwrap();

        // 2. A tree: 20 000 small files, every tenth a copy of an earlier
        // one; four copies of a 64 MiB file and four 64 MiB files that
        // differ only in the middle (each read in full).
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let mut small_bytes = 0u64;
        for i in 0..20_000u64 {
            let dir = tree.join(format!("d{}", i / 1000));
            std::fs::create_dir_all(&dir).unwrap();
            let source = if i % 10 == 9 { i - 5 } else { i };
            let len = 1024 + (source * 7919 % (63 * 1024)) as usize;
            let bytes = noise(source + 100, len);
            small_bytes += bytes.len() as u64;
            std::fs::write(dir.join(format!("f{i}.dat")), bytes).unwrap();
        }
        let large = noise(7, 64 << 20);
        for i in 0..4 {
            std::fs::write(tree.join(format!("copy{i}.iso")), &large).unwrap();
        }
        let mut other = large.clone();
        for i in 0..4u8 {
            other[32 << 20] = i;
            std::fs::write(tree.join(format!("near{i}.iso")), &other).unwrap();
        }
        drop((large, other));
        let mut store = NodeStore::new();
        scan(
            &mut store,
            std::slice::from_ref(&tree),
            &ScanOptions::default(),
        );
        let index = Index::new(store);
        let ids = search(
            &index,
            &Query::parse("", QueryOptions::default()).unwrap(),
            8,
        );
        let contents = spec("duplicates:");
        let t = Instant::now();
        let grouped = group(&index, &ids, &contents);
        println!(
            "tree: {} entries, {:.1} MB small files + 512 MiB large; grouped in {:.1} ms, {} candidates",
            ids.len(),
            small_bytes as f64 / 1e6,
            ms(t),
            grouped.candidates.files()
        );
        let run = |label: &str, cache: Option<&Mutex<HashCache>>, threads: usize| {
            let t = Instant::now();
            let dupes =
                compare_contents(grouped.clone(), cache, threads, &Progress::new()).unwrap();
            println!(
                "compare {label}, {threads} threads: {:.0} ms, {} groups, {} listed",
                ms(t),
                dupes.groups(),
                dupes.to_sort().len()
            );
        };
        for threads in [1, 4, 8] {
            uncache(&tree);
            run("cold", None, threads);
        }
        for threads in [1, 8] {
            run("warm", None, threads);
        }
        let cache = Mutex::new(HashCache::default());
        uncache(&tree);
        run("cold, filling the cache", Some(&cache), 8);
        uncache(&tree);
        run("cold, from the cache", Some(&cache), 8);
        run("warm, from the cache", Some(&cache), 8);
        {
            let c = cache.lock().unwrap();
            println!(
                "cache: {} entries, {} bytes in the file",
                c.len(),
                c.bytes() + 16
            );
        }
        // Cancel 50 ms into a cold run.
        uncache(&tree);
        let progress = Progress::new();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| compare_contents(grouped.clone(), None, 8, &progress));
            std::thread::sleep(std::time::Duration::from_millis(50));
            let t = Instant::now();
            progress.cancel();
            let result = worker.join().unwrap();
            println!(
                "cancel 50 ms into a cold compare: returned {:.1} ms after it ({:?}), {} of {} steps done",
                ms(t),
                result.err(),
                progress.done(),
                progress.total().unwrap_or(0)
            );
        });

        // 3. A large index without files: grouping by size and by name,
        // and the frequency counts.
        let mut s = NodeStore::new();
        let root = s.add(None, b"/big", true, Meta::default());
        let dirs: Vec<NodeId> = (0..6_000)
            .map(|d| {
                s.add(
                    Some(root),
                    format!("dir{d}").as_bytes(),
                    true,
                    Meta::default(),
                )
            })
            .collect();
        let exts = ["jpg", "png", "txt", "rs", "pdf", "mp3", "json", "md"];
        for i in 0..600_000u64 {
            let name = format!("file{}.{}", i % 150_000, exts[(i % 8) as usize]);
            let meta = Meta {
                size: i * 2_654_435_761 % 5_000_000,
                ..Meta::default()
            };
            s.add(
                Some(dirs[(i % 6_000) as usize]),
                name.as_bytes(),
                false,
                meta,
            );
        }
        let index = Index::new(s);
        let ids: Vec<NodeId> = index.order().iter().collect();
        for q in ["duplicates:size", "duplicates:name", "duplicates:name;size"] {
            let t = Instant::now();
            let g = group(&index, &ids, &spec(q));
            println!(
                "{q} over {} entries: {:.0} ms, {} groups",
                ids.len(),
                ms(t),
                g.dupes.groups()
            );
        }
        let t = Instant::now();
        let f = crate::frequency::Frequencies::build(&index);
        println!("frequencies of {} nodes: {:.0} ms", f.len(), ms(t));

        // 4. The cache file for 100 000 files with every hash.
        let mut c = HashCache::default();
        for i in 0..100_000u64 {
            let key = FileKey {
                dev: 1,
                ino: i,
                size: i,
                mtime_ns: i as i64,
            };
            let all = Cached {
                ends: Some(i.into()),
                full: Some(i.into()),
                md5: Some([1; 16]),
                sha256: Some([2; 32]),
                crc32: Some(3),
            };
            c.put(key, &all);
        }
        let t = Instant::now();
        let bytes = c.to_bytes();
        let written = ms(t);
        let t = Instant::now();
        let back = HashCache::from_bytes(&bytes, u64::MAX).unwrap();
        println!(
            "cache file of {} entries: {} bytes ({} per entry), written in {written:.1} ms, read in {:.1} ms",
            back.len(),
            bytes.len(),
            bytes.len() / back.len(),
            ms(t)
        );
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let hwm = status
            .lines()
            .find(|l| l.starts_with("VmHWM"))
            .unwrap_or("");
        println!("peak RSS of the test process: {hwm}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
