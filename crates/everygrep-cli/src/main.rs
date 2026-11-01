//! `everygrep` command line tool.

use std::io::{self, BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use everygrep_core::filter::Filter;
use everygrep_core::index::Index;
use everygrep_core::persist;
use everygrep_core::query::{Query, QueryOptions};
use everygrep_core::rules::{IndexRules, RuleSpec};
use everygrep_core::scan::{ScanOptions, normalize_roots, scan};
use everygrep_core::search::search;
use everygrep_core::store::NodeStore;
use everygrep_core::sync::{CatchUpStats, catch_up};
use everygrep_core::watch::Watcher;

const USAGE: &str = "\
usage:
  everygrep index [options]            scan and save the index
  everygrep search [options] <query>   print matching paths
  everygrep shell [options]            read one query per line
  everygrep --version

query syntax: space = and, a|b = or, !a = not, <a b> = group, \"a b\" = literal,
*.pdf = wildcard, case: wholeword: diacritics: path: file: folder: ext:pdf;jpg|png
audio: archive: document: executable: image: video: code: size:>10mb size:1mb size:large
modified:<7d modified:2026-10-25 modified:>=2026-10 modified:lastweek
modified:2026-10-01..2026-10-25 modified:today created:today accessed:>30d prefix:
suffix: nopunct: nospace: regex:^img_\\d+ modified:last3days modified:monday limit:100

search and shell load the saved index; they scan and save it first when there
is none, when --root differs from the saved roots, or with --rescan.

options:
  --root <path>      folder to index (repeatable, default: saved roots or $HOME)
  --index-file <p>   index location (default: ~/.cache/everygrep/index.bin)
  --rescan           scan even if a saved index exists
  --watch            shell only: apply file changes before every query and save
                     the index on exit
  --exclude <path>   folder to leave out, by path or pattern (repeatable)
  --include-files <list>   index only files matching one of these patterns
  --exclude-files <list>   leave out files matching one of these patterns
                     (lists: *.mp3;*.jpg, regex:<expression>; see README)
  --case             match case exactly
  --whole-words      match whole words only
  --match-path       match the full path instead of the name
  --diacritics       match diacritics exactly (by default e also finds é)
  --prefix           terms must match at the start of the name
  --suffix           terms must match at the end of the name
  --ignore-punctuation   leave punctuation out of names and terms
  --ignore-whitespace    leave whitespace out of names and terms
  --regex            the whole query is one regular expression
  --filter <name>    only all, audio, archive, code, document, executable,
                     folder, picture or video entries
  --threads <n>      search threads (default: number of CPUs)
  --limit <n>        print at most n results (default: 50, 0 = all)
  --no-meta          do not read size and modification time";

struct Args {
    command: String,
    query: String,
    roots: Vec<PathBuf>,
    index_file: Option<PathBuf>,
    rescan: bool,
    watch: bool,
    scan: ScanOptions,
    matching: QueryOptions,
    threads: usize,
    limit: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let command = it.next().ok_or("missing command")?;
    let mut args = Args {
        command,
        query: String::new(),
        roots: Vec::new(),
        index_file: persist::default_path(),
        rescan: false,
        watch: false,
        scan: ScanOptions::default(),
        matching: QueryOptions::default(),
        threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
        limit: 50,
    };
    let mut query = Vec::new();
    let mut rules = RuleSpec::default();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--root" => args.roots.push(PathBuf::from(value("--root")?)),
            "--index-file" => args.index_file = Some(PathBuf::from(value("--index-file")?)),
            "--rescan" => args.rescan = true,
            "--watch" => args.watch = true,
            "--exclude" => rules.folders.push(PathBuf::from(value("--exclude")?)),
            "--include-files" => rules.include_files = value("--include-files")?,
            "--exclude-files" => rules.exclude_files = value("--exclude-files")?,
            "--case" => args.matching.case_sensitive = true,
            "--whole-words" => args.matching.whole_words = true,
            "--match-path" => args.matching.match_path = true,
            "--diacritics" => args.matching.match_diacritics = true,
            "--prefix" => args.matching.prefix = true,
            "--suffix" => args.matching.suffix = true,
            "--ignore-punctuation" => args.matching.ignore_punctuation = true,
            "--ignore-whitespace" => args.matching.ignore_whitespace = true,
            "--regex" => args.matching.regex = true,
            "--filter" => {
                let name = value("--filter")?;
                args.matching.filter =
                    Filter::from_id(&name).ok_or(format!("unknown filter {name}"))?;
            }
            "--threads" => args.threads = parse_num(&value("--threads")?)?,
            "--limit" => args.limit = parse_num(&value("--limit")?)?,
            "--no-meta" => args.scan.with_metadata = false,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => query.push(a),
        }
    }
    args.query = query.join(" ");
    args.roots = normalize_roots(&args.roots);
    args.scan.rules = IndexRules::new(rules).map_err(|e| e.to_string())?;
    Ok(args)
}

fn parse_num(s: &str) -> Result<usize, String> {
    s.parse().map_err(|_| format!("not a number: {s}"))
}

fn home_root() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

/// How an index was obtained.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Loaded,
    Scanned,
}

/// Loads the saved index if it fits the request, otherwise scans and saves.
/// Returns the index, the roots it covers and where it came from.
fn open_index(args: &Args, force_scan: bool) -> (Index, Vec<PathBuf>, Source) {
    if !force_scan && !args.rescan {
        if let Some(path) = &args.index_file {
            let started = Instant::now();
            match persist::load(path) {
                Ok(saved) if args.roots.is_empty() || saved.roots == args.roots => {
                    eprintln!(
                        "loaded {} entries from {} in {:.0} ms, memory {:.1} MiB",
                        saved.index.len(),
                        path.display(),
                        started.elapsed().as_secs_f64() * 1e3,
                        saved.index.heap_bytes() as f64 / 1048576.0,
                    );
                    if saved.reordered {
                        // An older file: store the new name order once.
                        save_index(args, &saved.index, &saved.roots);
                    }
                    return (saved.index, saved.roots, Source::Loaded);
                }
                Ok(_) => eprintln!("saved index has other roots, scanning"),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("cannot use saved index ({e}), scanning"),
            }
        }
    }
    let roots = if args.roots.is_empty() {
        home_root()
    } else {
        args.roots.clone()
    };
    let index = scan_roots(&roots, &args.scan);
    save_index(args, &index, &roots);
    (index, roots, Source::Scanned)
}

fn report_catch_up(stats: &CatchUpStats, took: std::time::Duration) -> bool {
    eprintln!(
        "caught up in {:.0} ms: checked {} folders, re-read {}, +{} -{} updated {}",
        took.as_secs_f64() * 1e3,
        stats.dirs_checked,
        stats.dirs_rescanned,
        stats.added,
        stats.removed,
        stats.updated
    );
    stats.added + stats.removed + stats.updated > 0
}

fn save_index(args: &Args, index: &Index, roots: &[PathBuf]) {
    if let Some(path) = &args.index_file {
        let started = Instant::now();
        match persist::save(index, roots, path) {
            Ok(()) => eprintln!(
                "saved index to {} in {:.0} ms",
                path.display(),
                started.elapsed().as_secs_f64() * 1e3
            ),
            Err(e) => eprintln!("could not save index to {}: {e}", path.display()),
        }
    }
}

/// Starts watching, then catches up with changes made before watches were
/// installed, including during a fresh scan. Returns whether catch-up changed it.
fn start_watcher(index: &mut Index, options: &ScanOptions) -> (Option<Watcher>, bool) {
    let started = Instant::now();
    let mut w = match Watcher::new(index, options) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("cannot watch for changes: {e}");
            return (None, false);
        }
    };
    eprintln!(
        "watching {} folders ({} could not be watched) after {:.0} ms",
        w.watch_count(),
        w.failed_watches(),
        started.elapsed().as_secs_f64() * 1e3
    );
    let started = Instant::now();
    let stats = w.catch_up(index);
    let changed = report_catch_up(&stats, started.elapsed());
    (Some(w), changed)
}

fn scan_roots(roots: &[PathBuf], options: &ScanOptions) -> Index {
    let started = Instant::now();
    let mut store = NodeStore::new();
    let stats = scan(&mut store, roots, options);
    let scanned = started.elapsed();
    let index = Index::new(store);
    let sorted = started.elapsed() - scanned;
    eprintln!(
        "indexed {} folders, {} files ({} errors) in {:.0} ms (scan {:.0} ms, sort {:.0} ms), memory {:.1} MiB",
        stats.dirs,
        stats.files,
        stats.errors,
        started.elapsed().as_secs_f64() * 1e3,
        scanned.as_secs_f64() * 1e3,
        sorted.as_secs_f64() * 1e3,
        index.heap_bytes() as f64 / 1048576.0,
    );
    index
}

fn run_query(index: &Index, args: &Args, query: &str, out: &mut impl Write) -> io::Result<()> {
    let started = Instant::now();
    let parsed = match Query::parse(query, args.matching) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("bad query: {e}");
            return Ok(());
        }
    };
    let mut hits = search(index, &parsed, args.threads);
    // `limit:` keeps the first results in name order.
    parsed.apply_limit(&mut hits);
    let took = started.elapsed();
    let shown = if args.limit == 0 {
        hits.len()
    } else {
        hits.len().min(args.limit)
    };
    for &id in &hits[..shown] {
        out.write_all(index.store().path(id).as_os_str().as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    eprintln!(
        "{} results in {:.2} ms (showing {shown})",
        hits.len(),
        took.as_secs_f64() * 1e3
    );
    Ok(())
}

fn main() -> ExitCode {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("everygrep {}", everygrep_core::VERSION);
        return ExitCode::SUCCESS;
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("everygrep: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let result = match args.command.as_str() {
        "index" => {
            open_index(&args, true);
            Ok(())
        }
        "search" => {
            let (mut index, roots, source) = open_index(&args, false);
            if source == Source::Loaded {
                let started = Instant::now();
                let stats = catch_up(&mut index, &args.scan);
                if report_catch_up(&stats, started.elapsed()) {
                    save_index(&args, &index, &roots);
                }
            }
            run_query(&index, &args, &args.query, &mut out)
        }
        "shell" => {
            let (mut index, roots, _) = open_index(&args, false);
            let (mut watcher, mut changed) = if args.watch {
                start_watcher(&mut index, &args.scan)
            } else {
                (None, false)
            };
            let mut status = Ok(());
            for line in io::stdin().lock().lines() {
                let q = match line {
                    Ok(q) => q,
                    Err(e) => {
                        status = Err(e);
                        break;
                    }
                };
                if let Some(w) = watcher.as_mut() {
                    match w.poll(&mut index, Some(std::time::Duration::ZERO)) {
                        Ok(st) if st.overflow => {
                            eprintln!("change queue overflowed, rescanning");
                            index = scan_roots(&roots, &args.scan);
                            *w = match Watcher::new(&index, &args.scan) {
                                Ok(nw) => nw,
                                Err(e) => {
                                    eprintln!("cannot watch for changes: {e}");
                                    break;
                                }
                            };
                            // Changes made while rescanning were not watched yet.
                            let started = Instant::now();
                            let stats = w.catch_up(&mut index);
                            report_catch_up(&stats, started.elapsed());
                            changed = true;
                        }
                        Ok(st) if st != Default::default() => {
                            eprintln!(
                                "applied changes: +{} -{} moved {} updated {}",
                                st.added, st.removed, st.moved, st.updated
                            );
                            changed = true;
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("reading changes failed: {e}"),
                    }
                }
                status = run_query(&index, &args, &q, &mut out);
                if status.is_err() {
                    break;
                }
            }
            if changed {
                save_index(&args, &index, &roots);
            }
            status
        }
        other => {
            eprintln!("everygrep: unknown command {other}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("everygrep: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn fresh_shell_catches_changes_between_scan_and_watch_installation() {
        let base =
            std::env::temp_dir().join(format!("everygrep-cli-watch-gap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let options = ScanOptions::default();
        let mut index = scan_roots(std::slice::from_ref(&base), &options);
        // Scan completion is the barrier: this event cannot be in the scan or
        // the watch queue. It must be found by startup catch-up.
        let missed = base.join("during-scan.txt");
        fs::write(&missed, b"new").unwrap();
        let (watcher, changed) = start_watcher(&mut index, &options);
        assert!(watcher.is_some());
        assert!(changed);
        assert!(
            index
                .order()
                .iter()
                .any(|id| index.store().path(id) == missed)
        );
        drop(watcher);
        fs::remove_dir_all(&base).unwrap();
    }
}
