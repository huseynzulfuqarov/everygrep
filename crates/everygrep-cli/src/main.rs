//! `everygrep` command line tool.

use std::io::{self, BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use everygrep_core::index::Index;
use everygrep_core::matcher::{MatchOptions, Matcher};
use everygrep_core::persist;
use everygrep_core::scan::{ScanOptions, scan};
use everygrep_core::search::search;
use everygrep_core::store::NodeStore;
use everygrep_core::watch::Watcher;

const USAGE: &str = "\
usage:
  everygrep index [options]            scan and save the index
  everygrep search [options] <query>   print matching paths
  everygrep shell [options]            read one query per line
  everygrep --version

search and shell load the saved index; they scan and save it first when there
is none, when --root differs from the saved roots, or with --rescan.

options:
  --root <path>      folder to index (repeatable, default: saved roots or $HOME)
  --index-file <p>   index location (default: ~/.cache/everygrep/index.bin)
  --rescan           scan even if a saved index exists
  --watch            shell only: apply file changes before every query and save
                     the index on exit
  --exclude <path>   folder not to enter (repeatable)
  --case             match case exactly
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
    matching: MatchOptions,
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
        matching: MatchOptions::default(),
        threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
        limit: 50,
    };
    let mut query = Vec::new();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--root" => args.roots.push(PathBuf::from(value("--root")?)),
            "--index-file" => args.index_file = Some(PathBuf::from(value("--index-file")?)),
            "--rescan" => args.rescan = true,
            "--watch" => args.watch = true,
            "--exclude" => args.scan.exclude.push(PathBuf::from(value("--exclude")?)),
            "--case" => args.matching.case_sensitive = true,
            "--threads" => args.threads = parse_num(&value("--threads")?)?,
            "--limit" => args.limit = parse_num(&value("--limit")?)?,
            "--no-meta" => args.scan.with_metadata = false,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => query.push(a),
        }
    }
    args.query = query.join(" ");
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

/// Loads the saved index if it fits the request, otherwise scans and saves.
/// Returns the index and the roots it covers.
fn open_index(args: &Args, force_scan: bool) -> (Index, Vec<PathBuf>) {
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
                    return (saved.index, saved.roots);
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
    (index, roots)
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

fn start_watcher(index: &Index) -> Option<Watcher> {
    let started = Instant::now();
    match Watcher::new(index) {
        Ok(w) => {
            eprintln!(
                "watching {} folders ({} could not be watched) after {:.0} ms",
                w.watch_count(),
                w.failed_watches,
                started.elapsed().as_secs_f64() * 1e3
            );
            Some(w)
        }
        Err(e) => {
            eprintln!("cannot watch for changes: {e}");
            None
        }
    }
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
    let matcher = Matcher::new(query, args.matching);
    let hits = search(index, &matcher, args.threads);
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
            let (index, _) = open_index(&args, false);
            run_query(&index, &args, &args.query, &mut out)
        }
        "shell" => {
            let (mut index, roots) = open_index(&args, false);
            let mut watcher = if args.watch {
                start_watcher(&index)
            } else {
                None
            };
            let mut changed = false;
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
                            *w = match Watcher::new(&index) {
                                Ok(nw) => nw,
                                Err(e) => {
                                    eprintln!("cannot watch for changes: {e}");
                                    break;
                                }
                            };
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
