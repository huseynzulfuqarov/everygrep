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
//!    read. Folders and empty files are left out;
//! 2. without the index ([`compare_contents`]), on worker threads: every
//!    candidate is looked at on disk (`lstat` through
//!    [`crate::hash::open`]): what is not a readable regular file any more
//!    is left out, and of the paths of one file (hard links, the same device
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
        .filter(|&id| !(contents && store.meta(id).size == 0))
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
        if end - start > 1 {
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
        Ok(Looked {
            key: hash::open(path_of(l))?.key,
            ..*l
        })
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
        // Size 4: a, sub/a, d, diff; size 100 000: three big ones.
        assert_eq!(grouped.candidates.files(), 7);
        let progress = Progress::new();
        compare_contents(grouped.clone(), None, 2, &progress).unwrap();
        assert_eq!((progress.done(), progress.total()), (21, Some(21)));
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(
            compare_contents(grouped, None, 2, &progress),
            Err(Cancelled)
        );
    }
}
