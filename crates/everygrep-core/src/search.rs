//! Parallel search over the name order of an [`Index`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use std::os::unix::ffi::OsStringExt;

use crate::content::{ContentOptions, Decisions, Pending, decide_all};
use crate::index::Index;
use crate::progress::{Cancelled, Progress};
use crate::query::{EvalScratch, Query, Stage};
use crate::store::NodeId;

/// Number of name-order blocks handed to a worker at a time.
const BLOCKS_PER_TASK: usize = 16;

/// Returns the ids of all live nodes whose name matches, sorted by name.
///
/// The name order is split into groups of blocks that worker threads take one by
/// one, so fast and slow groups balance out. Group results are joined in group
/// order, which keeps the name order of the index. A query with `content:`
/// terms then reads the files the index cannot decide (see [`search_staged`]).
pub fn search(index: &Index, query: &Query, threads: usize) -> Vec<NodeId> {
    search_with_progress(index, query, threads, &Progress::new()).unwrap_or_default()
}

/// Like [`search`], counting the names looked at in `progress` (its total is
/// set to the size of the index) and stopping when it is cancelled. The
/// cancel is seen before each group of blocks and before returning results,
/// including when the query matches every name or the index is empty.
///
/// With `content:` terms the files are read on `threads` threads, with the
/// default [`ContentOptions`], while `index` stays borrowed; the progress
/// then starts again and counts files. The window reads them without
/// holding the index (see [`search_staged`] and
/// [`crate::content::decide_all`]).
pub fn search_with_progress(
    index: &Index,
    query: &Query,
    threads: usize,
    progress: &Progress,
) -> Result<Vec<NodeId>, Cancelled> {
    if query.content_terms().is_empty() {
        return search_names(index, query, threads, progress);
    }
    let staged = search_staged(index, query, threads, progress)?;
    progress.restart(Some(staged.pending.len() as u64));
    let decisions = Decisions::new(staged.pending.len());
    decide_all(
        &staged.pending,
        query.content_terms(),
        &ContentOptions::default(),
        None,
        threads,
        progress,
        &decisions,
        || {},
    )?;
    Ok(staged.results(&decisions))
}

fn search_names(
    index: &Index,
    query: &Query,
    threads: usize,
    progress: &Progress,
) -> Result<Vec<NodeId>, Cancelled> {
    let order = index.order();
    progress.set_total(Some(order.len() as u64));
    progress.check()?;
    if query.matches_all() {
        let mut out = Vec::with_capacity(order.len());
        for group in order.blocks().chunks(BLOCKS_PER_TASK) {
            progress.check()?;
            for block in group {
                out.extend_from_slice(block);
                progress.add(block.len() as u64);
            }
        }
        progress.check()?;
        return Ok(out);
    }
    let parts = scan_groups(index, threads, progress, |scratch, id| {
        query.matches(index.store(), id, scratch).then_some(id)
    })?;
    Ok(parts.into_iter().flatten().collect())
}

/// The first stage of a query with `content:` terms: what the index alone
/// decides, without reading any file.
#[derive(Debug, Default)]
pub struct Staged {
    /// In name order: the matches and the entries whose contents decide.
    pub ids: Vec<NodeId>,
    /// The places in `ids` of the entries whose contents decide.
    pub places: Vec<usize>,
    /// Those entries, in the same order: what to read.
    pub pending: Vec<Pending>,
}

impl Staged {
    /// The matches: the entries the index decided, and those `decisions`
    /// accepted so far, in name order.
    pub fn results(&self, decisions: &Decisions) -> Vec<NodeId> {
        let mut out = Vec::with_capacity(self.ids.len() - self.places.len() + decisions.matched());
        let mut next = self.places.iter().enumerate().peekable();
        for (at, &id) in self.ids.iter().enumerate() {
            match next.peek() {
                Some(&(i, &place)) if place == at => {
                    next.next();
                    if decisions.get(i) == Some(true) {
                        out.push(id);
                    }
                }
                _ => out.push(id),
            }
        }
        out
    }
}

/// Runs the index part of `query` (see [`Query::stage`]): entries it
/// rejects are left out, and for those whose contents decide, the path,
/// size and modification time are taken while `index` is borrowed, so the
/// files can be read after it is released. Progress counts names, as in
/// [`search_with_progress`].
pub fn search_staged(
    index: &Index,
    query: &Query,
    threads: usize,
    progress: &Progress,
) -> Result<Staged, Cancelled> {
    progress.set_total(Some(index.order().len() as u64));
    progress.check()?;
    let store = index.store();
    let parts = scan_groups(index, threads, progress, |scratch, id| {
        match query.stage(store, id, scratch) {
            Stage::No => None,
            Stage::Yes => Some((id, None)),
            Stage::Contents(table) => {
                let meta = store.meta(id);
                Some((
                    id,
                    Some(Pending {
                        path: store.path(id).into_os_string().into_vec(),
                        size: meta.size,
                        mtime_ns: meta.mtime_ns,
                        table,
                    }),
                ))
            }
        }
    })?;
    let mut staged = Staged::default();
    for (id, pending) in parts.into_iter().flatten() {
        if let Some(pending) = pending {
            staged.places.push(staged.ids.len());
            staged.pending.push(pending);
        }
        staged.ids.push(id);
    }
    Ok(staged)
}

/// Evaluates `test` for every live node on `threads` threads, a group of
/// blocks at a time, and returns what it kept per group, in name order.
fn scan_groups<T: Send>(
    index: &Index,
    threads: usize,
    progress: &Progress,
    test: impl Fn(&mut EvalScratch, NodeId) -> Option<T> + Sync,
) -> Result<Vec<Vec<T>>, Cancelled> {
    let order = index.order();
    progress.set_total(Some(order.len() as u64));
    progress.check()?;
    let groups: Vec<&[Vec<NodeId>]> = order.blocks().chunks(BLOCKS_PER_TASK).collect();
    let threads = threads.clamp(1, groups.len().max(1));
    let scan_group = |group: &[Vec<NodeId>]| -> Result<Vec<T>, Cancelled> {
        progress.check()?;
        let mut scratch = EvalScratch::with_offline_roots(index.offline_roots());
        let found = group
            .iter()
            .flatten()
            .filter_map(|&id| test(&mut scratch, id))
            .collect();
        progress.add(group.iter().map(|b| b.len() as u64).sum());
        Ok(found)
    };
    if threads == 1 {
        let mut out = Vec::with_capacity(groups.len());
        for group in &groups {
            out.push(scan_group(group)?);
        }
        progress.check()?;
        return Ok(out);
    }

    let next = AtomicUsize::new(0);
    let parts: Mutex<Vec<(usize, Vec<T>)>> = Mutex::new(Vec::with_capacity(groups.len()));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let Some(group) = groups.get(n) else { break };
                    let Ok(found) = scan_group(group) else { break };
                    local.push((n, found));
                }
                parts.lock().expect("search worker panicked").extend(local);
            });
        }
    });

    progress.check()?;
    let mut parts = parts.into_inner().expect("search worker panicked");
    parts.sort_unstable_by_key(|(n, _)| *n);
    progress.check()?;
    Ok(parts.into_iter().map(|(_, part)| part).collect())
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
        assert!(
            names
                .windows(2)
                .all(|w| crate::collate::collate(w[0], w[1]).is_le())
        );
    }

    #[test]
    fn progress_covers_the_index_and_cancel_returns_nothing() {
        let index = big_index(50_000);
        let m = Query::parse("match", QueryOptions::default()).unwrap();
        for threads in [1, 8] {
            let progress = Progress::new();
            let found = search_with_progress(&index, &m, threads, &progress).unwrap();
            assert_eq!(found, search(&index, &m, 1));
            assert_eq!(progress.total(), Some(index.len() as u64));
            assert_eq!(progress.done(), index.len() as u64);

            let cancelled = Progress::new();
            cancelled.cancel();
            assert_eq!(
                search_with_progress(&index, &m, threads, &cancelled),
                Err(Cancelled)
            );
            assert_eq!(cancelled.done(), 0);
        }
    }

    #[test]
    fn offline_roots_are_not_looked_up_on_disk() {
        use std::os::unix::fs::PermissionsExt;
        // The saved paths of an offline root now hold other files (another
        // drive mounted at the same place): they must not decide matches.
        let dir = std::env::temp_dir().join(format!("everygrep-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["runme", "notes.txt", "tool.sh"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        std::fs::set_permissions(dir.join("runme"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let mut s = NodeStore::new();
        let root = s.add(
            None,
            dir.as_os_str().as_encoded_bytes(),
            true,
            Meta::default(),
        );
        for name in ["runme", "notes.txt", "tool.sh"] {
            s.add(Some(root), name.as_bytes(), false, Meta::default());
        }
        let mut index = Index::new(s);
        let names = |index: &Index, q: &str| -> Vec<String> {
            let query = Query::parse(q, QueryOptions::default()).unwrap();
            let mut out: Vec<String> = search(index, &query, 1)
                .into_iter()
                .map(|id| String::from_utf8_lossy(index.store().name(id)).into_owned())
                .collect();
            out.sort();
            out
        };
        assert_eq!(names(&index, "executable:"), ["runme", "tool.sh"]);
        assert_eq!(
            names(&index, "accessed:today file:"),
            ["notes.txt", "runme", "tool.sh"]
        );
        index.set_offline(root, true);
        // Executables by extension and the names stay searchable.
        assert_eq!(names(&index, "executable:"), ["tool.sh"]);
        assert_eq!(names(&index, "runme"), ["runme"]);
        assert!(names(&index, "accessed:today").is_empty());
        assert!(names(&index, "created:<2100").is_empty());
        index.set_offline(root, false);
        assert_eq!(names(&index, "executable:"), ["runme", "tool.sh"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_query_returns_all_nodes() {
        let index = big_index(50_000);
        let m = Query::parse("", QueryOptions::default()).unwrap();
        for threads in [1, 8] {
            let progress = Progress::new();
            let found = search_with_progress(&index, &m, threads, &progress).unwrap();
            assert_eq!(found, index.order().iter().collect::<Vec<_>>());
            assert_eq!(progress.total(), Some(index.len() as u64));
            assert_eq!(progress.done(), index.len() as u64);
        }
    }

    #[test]
    fn cancelled_search_returns_no_results_for_any_query_or_index_size() {
        for index in [Index::new(NodeStore::new()), big_index(50_000)] {
            for text in ["", "match"] {
                let query = Query::parse(text, QueryOptions::default()).unwrap();
                for threads in [1, 8] {
                    let progress = Progress::new();
                    progress.cancel();
                    assert_eq!(
                        search_with_progress(&index, &query, threads, &progress),
                        Err(Cancelled),
                        "query {text:?}, {} nodes, {threads} threads",
                        index.len()
                    );
                    assert_eq!(progress.done(), 0);
                }
            }
        }
    }

    /// Prints search times for 600 000 names, a tenth of them not ASCII. Run
    /// with `cargo test --release -p everygrep-core search_timing -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "timing only"]
    fn search_timing() {
        use crate::filter::Filter;
        let mut s = NodeStore::new();
        let root = s.add(None, b"/r", true, Meta::default());
        let exts = ["txt", "pdf", "rs", "jpg", "mp3", "md", "png"];
        for i in 0..600_000u64 {
            let h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40;
            let word = if i % 10 == 0 {
                "şəkil-ölçü"
            } else {
                "report-draft"
            };
            let name = format!("{word}-{h:06x}.{}", exts[i as usize % exts.len()]);
            s.add(Some(root), name.as_bytes(), false, Meta::default());
        }
        let index = Index::new(s);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let cases: [(&str, QueryOptions); 12] = [
            ("draft", QueryOptions::default()),
            (
                "draft",
                QueryOptions {
                    match_diacritics: true,
                    ..QueryOptions::default()
                },
            ),
            ("sekil", QueryOptions::default()),
            (
                "draft",
                QueryOptions {
                    whole_words: true,
                    ..QueryOptions::default()
                },
            ),
            (
                "",
                QueryOptions {
                    filter: Filter::Picture,
                    ..QueryOptions::default()
                },
            ),
            (
                "draft",
                QueryOptions {
                    filter: Filter::Audio,
                    ..QueryOptions::default()
                },
            ),
            (
                "r",
                QueryOptions {
                    match_path: true,
                    ..QueryOptions::default()
                },
            ),
            (
                "report",
                QueryOptions {
                    prefix: true,
                    ..QueryOptions::default()
                },
            ),
            (
                "reportdraft",
                QueryOptions {
                    ignore_punctuation: true,
                    ..QueryOptions::default()
                },
            ),
            ("regex:draft-[0-9a-f]+\\.pdf$", QueryOptions::default()),
            ("regex:^şəkil", QueryOptions::default()),
            (
                "dr.ft",
                QueryOptions {
                    regex: true,
                    ..QueryOptions::default()
                },
            ),
        ];
        for (text, options) in cases {
            let q = Query::parse(text, options).unwrap();
            for t in [1, threads] {
                let started = std::time::Instant::now();
                let n = search(&index, &q, t).len();
                eprintln!(
                    "{text:?} {options:?}, {t} threads: {n} results, {:.1} ms",
                    started.elapsed().as_secs_f64() * 1e3
                );
            }
        }
    }

    /// Prints the best of five search times for wildcard terms matched
    /// against the full paths of 500 000 files (paths of about 100 bytes, in
    /// 2 000 folders). Run with `cargo test --release -p everygrep-core
    /// path_wildcard_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing only"]
    fn path_wildcard_timing() {
        let mut s = NodeStore::new();
        let root = s.add(None, b"/home/user", true, Meta::default());
        let exts = ["pdf", "txt", "rs", "jpg", "odt"];
        let mut folders = Vec::new();
        for p in 0..100 {
            let project = format!("Documents/projects/project-{p:03}");
            let project = s.add(Some(root), project.as_bytes(), true, Meta::default());
            for f in 0..20 {
                let sub = format!("subfolder-{f:02}");
                let sub = s.add(Some(project), sub.as_bytes(), true, Meta::default());
                folders.push(s.add(Some(sub), b"nested", true, Meta::default()));
            }
        }
        for i in 0..500_000u64 {
            let h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40;
            let name = format!("report-draft-{h:06x}.{}", exts[i as usize % exts.len()]);
            let dir = folders[i as usize % folders.len()];
            s.add(Some(dir), name.as_bytes(), false, Meta::default());
        }
        let index = Index::new(s);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        for text in [
            "path:.pdf",
            "path:*.pdf",
            "path:**.pdf",
            "/home/user/**/subfolder-*/*.pdf",
            "/home/user/**/subfolder-*/nested/*.pdf",
            "/home/user/**/subfolder-1?/nested/*draft*",
            "path:**/project-05?/**",
            "*.pdf",
        ] {
            let q = Query::parse(text, QueryOptions::default()).unwrap();
            for t in [1, threads] {
                let mut best = f64::MAX;
                let mut n = 0;
                for _ in 0..5 {
                    let started = std::time::Instant::now();
                    n = search(&index, &q, t).len();
                    best = best.min(started.elapsed().as_secs_f64() * 1e3);
                }
                eprintln!("{text:?}, {t} threads: {n} results, {best:.1} ms");
            }
        }
    }

    /// A folder with files of these names and contents, indexed with a
    /// subfolder `sub` holding `inner.txt`.
    struct Tree {
        dir: std::path::PathBuf,
        index: Index,
    }

    impl Tree {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("everygrep-search-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            let mut s = NodeStore::new();
            let root = s.add(
                None,
                dir.as_os_str().as_encoded_bytes(),
                true,
                Meta::default(),
            );
            for (name, text) in files {
                std::fs::write(dir.join(name), text).unwrap();
                s.add(Some(root), name.as_bytes(), false, Meta::default());
            }
            let sub = s.add(Some(root), b"sub", true, Meta::default());
            std::fs::write(dir.join("sub/inner.txt"), "needle in a folder\n").unwrap();
            s.add(Some(sub), b"inner.txt", false, Meta::default());
            Self {
                dir,
                index: Index::new(s),
            }
        }

        fn names(&self, q: &str) -> Vec<String> {
            let query = Query::parse(q, QueryOptions::default()).unwrap();
            let mut out: Vec<String> = search(&self.index, &query, 4)
                .into_iter()
                .map(|id| String::from_utf8_lossy(self.index.store().name(id)).into_owned())
                .collect();
            out.sort();
            out
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn content_terms_combine_with_names_and_each_other() {
        let tree = Tree::new(
            "content",
            &[
                ("a.txt", "alpha needle\n"),
                ("b.txt", "beta\nhaystack\n"),
                ("c.md", "Needle and haystack\n"),
                ("needle.log", "nothing here\n"),
            ],
        );
        assert_eq!(tree.names("content:needle"), ["a.txt", "c.md", "inner.txt"]);
        assert_eq!(tree.names("content:needle ext:txt"), ["a.txt", "inner.txt"]);
        assert_eq!(tree.names("content:<needle haystack>"), ["c.md"]);
        assert_eq!(
            tree.names("content:<needle|haystack>"),
            ["a.txt", "b.txt", "c.md", "inner.txt"]
        );
        // `|` outside a group joins the content term and a name term.
        assert_eq!(
            tree.names("content:haystack|needle"),
            ["b.txt", "c.md", "needle.log"]
        );
        assert_eq!(tree.names("!content:needle file:"), ["b.txt", "needle.log"]);
        assert_eq!(tree.names("case:content:Needle"), ["c.md"]);
        assert_eq!(tree.names("regex:content:^hay"), ["b.txt"]);
        assert_eq!(tree.names("content:\"and hay\""), ["c.md"]);
        assert!(tree.names("content:missing").is_empty());
        // Folders never match a content term.
        assert_eq!(
            tree.names("content:needle|sub"),
            ["a.txt", "c.md", "inner.txt", "sub"]
        );
    }

    #[test]
    fn staged_search_leaves_the_files_to_read_and_shows_decided_matches() {
        let tree = Tree::new("staged", &[("a.txt", "needle\n"), ("b.txt", "other\n")]);
        let query = Query::parse("content:needle | a.txt", QueryOptions::default()).unwrap();
        let staged = search_staged(&tree.index, &query, 2, &Progress::new()).unwrap();
        let name = |id: NodeId| String::from_utf8_lossy(tree.index.store().name(id)).into_owned();
        // a.txt matches by its name; b.txt and inner.txt need their contents.
        assert_eq!(
            staged.ids.iter().map(|&id| name(id)).collect::<Vec<_>>(),
            ["a.txt", "b.txt", "inner.txt"]
        );
        assert_eq!(staged.places, [1, 2]);
        assert!(staged.pending[0].path.ends_with(b"/b.txt"));
        let decisions = Decisions::new(staged.pending.len());
        // Nothing read yet: only what the index decided.
        assert_eq!(staged.results(&decisions).len(), 1);
        decide_all(
            &staged.pending,
            query.content_terms(),
            &ContentOptions::default(),
            None,
            1,
            &Progress::new(),
            &decisions,
            || {},
        )
        .unwrap();
        assert_eq!(decisions.get(0), Some(false));
        assert_eq!(decisions.get(1), Some(true));
        assert_eq!(
            staged
                .results(&decisions)
                .into_iter()
                .map(name)
                .collect::<Vec<_>>(),
            ["a.txt", "inner.txt"]
        );
    }

    #[test]
    fn content_search_reads_nothing_of_offline_drives_and_stops_when_cancelled() {
        let mut tree = Tree::new("content-offline", &[("a.txt", "needle\n")]);
        let root = tree.index.roots()[0];
        tree.index.set_offline(root, true);
        assert!(tree.names("content:needle").is_empty());
        tree.index.set_offline(root, false);
        let query = Query::parse("content:needle", QueryOptions::default()).unwrap();
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(
            search_with_progress(&tree.index, &query, 2, &progress),
            Err(Cancelled)
        );
        // The progress counts the files read in the end.
        let progress = Progress::new();
        let found = search_with_progress(&tree.index, &query, 2, &progress).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!((progress.done(), progress.total()), (2, Some(2)));
    }
}
