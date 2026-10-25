//! `everygrep` command line tool.

use std::io::{self, BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use everygrep_core::index::Index;
use everygrep_core::matcher::{MatchOptions, Matcher};
use everygrep_core::scan::{ScanOptions, scan};
use everygrep_core::search::search;
use everygrep_core::store::NodeStore;

const USAGE: &str = "\
usage:
  everygrep search [options] <query>   scan, then print matching paths
  everygrep shell [options]            scan once, then read one query per line
  everygrep --version

options:
  --root <path>      folder to index (repeatable, default: $HOME)
  --exclude <path>   folder not to enter (repeatable)
  --case             match case exactly
  --threads <n>      search threads (default: number of CPUs)
  --limit <n>        print at most n results (default: 50, 0 = all)
  --no-meta          do not read size and modification time";

struct Args {
    command: String,
    query: String,
    roots: Vec<PathBuf>,
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
    if args.roots.is_empty() {
        args.roots.push(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or("HOME is not set")?,
        );
    }
    Ok(args)
}

fn parse_num(s: &str) -> Result<usize, String> {
    s.parse().map_err(|_| format!("not a number: {s}"))
}

fn build_index(args: &Args) -> Index {
    let started = Instant::now();
    let mut store = NodeStore::new();
    let stats = scan(&mut store, &args.roots, &args.scan);
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
        "search" => {
            let index = build_index(&args);
            run_query(&index, &args, &args.query, &mut out)
        }
        "shell" => {
            let index = build_index(&args);
            let mut status = Ok(());
            for line in io::stdin().lock().lines() {
                match line {
                    Ok(q) => status = run_query(&index, &args, &q, &mut out),
                    Err(e) => status = Err(e),
                }
                if status.is_err() {
                    break;
                }
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
