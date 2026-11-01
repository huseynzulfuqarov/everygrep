//! Context searches use literal names/paths and the existing query engine.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use everygrep_core::query::QueryOptions;
use slint::ComponentHandle;

use crate::i18n::{fill, tr};
use crate::{AppWindow, engine::Engine, prefs, results::ResultsModel};

/// Escape through the regex syntax, since query quotes cannot escape quotes
/// inside a filename. Valid UTF-8 keeps Unicode case matching; invalid UTF-8
/// is represented as raw bytes. Neither form can inject query operators.
fn literal(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if let Ok(text) = std::str::from_utf8(bytes) {
        for c in text.chars() {
            if c == '"' || c.is_control() {
                write!(out, "\\x{{{:x}}}", c as u32).expect("write to string");
            } else {
                if ".+*?()|[]{}^$\\".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    } else {
        out.push_str("(?-u:");
        for b in bytes {
            write!(out, "\\x{b:02x}").expect("write to string");
        }
        out.push(')');
    }
    out
}

/// Search in a directory, or for a file's name. Path searches always use
/// the containing directory. Directory boundaries prevent sibling matches.
fn query(
    path: &Path,
    is_dir: bool,
    containing: bool,
    recursive: bool,
    exact: bool,
    case: bool,
) -> Option<String> {
    if containing || is_dir {
        let folder = if containing { path.parent()? } else { path };
        let mut bytes = folder.as_os_str().as_bytes().to_vec();
        if !bytes.ends_with(b"/") {
            bytes.push(b'/');
        }
        let pattern = literal(&bytes);
        let end = if recursive {
            "(?s-u:.+)"
        } else {
            "(?-u:[^/]+)$"
        };
        // Filesystem locations are case-sensitive, independent of name search.
        Some(format!("case:path:regex:\"^{pattern}{end}\""))
    } else {
        let pattern = literal(path.file_name()?.as_bytes());
        let pattern = if exact {
            format!("^{pattern}$")
        } else {
            pattern
        };
        Some(format!(
            "{}:nopath:regex:\"{pattern}\"",
            if case { "case" } else { "nocase" }
        ))
    }
}

fn window_args(base: &[OsString], query: &str, options: QueryOptions) -> Vec<OsString> {
    let mut args = base.to_vec();
    // Giving a filter supplies explicit options, overriding a saved global
    // regex default that would otherwise treat the generated query as one regex.
    args.extend([OsString::from("--filter"), options.filter.id().into()]);
    for (on, flag) in [
        (options.case_sensitive, "--case"),
        (options.whole_words, "--whole-words"),
        (options.match_path, "--match-path"),
        (options.match_diacritics, "--diacritics"),
        (options.prefix, "--prefix"),
        (options.suffix, "--suffix"),
        (options.ignore_punctuation, "--ignore-punctuation"),
        (options.ignore_whitespace, "--ignore-whitespace"),
    ] {
        if on {
            args.push(flag.into());
        }
    }
    args.extend([OsString::from("--"), query.into()]);
    args
}

pub fn connect(
    w: &AppWindow,
    model: &Rc<ResultsModel>,
    engine: &Arc<Engine>,
    base_args: Vec<OsString>,
) {
    w.on_fileops_search({
        let (weak, model, engine) = (w.as_weak(), model.clone(), engine.clone());
        move |row, containing| {
            let (Some(w), Ok(row)) = (weak.upgrade(), usize::try_from(row)) else {
                return;
            };
            let (Some(path), Some((_, is_dir))) = (model.path(row), model.name(row)) else {
                return;
            };
            let mut settings = crate::fileops_settings::Settings::default();
            prefs::with(|p| settings = p.fileops_settings());
            let mut options = crate::options(&w);
            options.regex = false;
            let Some(query) = query(
                &path,
                is_dir,
                containing,
                settings.search_recursive,
                settings.search_exact_name,
                options.case_sensitive,
            ) else {
                return;
            };
            if settings.search_new_window {
                let result = std::env::current_exe().and_then(|exe| {
                    std::process::Command::new(exe)
                        .args(window_args(&base_args, &query, options))
                        .spawn()
                });
                if let Err(e) = result {
                    crate::error(&w, fill(tr("Cannot open a search window: {}"), &[&e]));
                }
            } else {
                // Cancel any delayed history record before replacing the query.
                w.invoke_history_record();
                w.set_query(query.as_str().into());
                crate::set_options(&w, options);
                crate::typing::with(|t| t.searched());
                crate::search_now(&w, &engine);
                w.invoke_focus_search();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::query::{EvalScratch, Query};
    use everygrep_core::store::{Meta, NodeStore};
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn context_queries_keep_special_names_literal_and_respect_directory_boundaries() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/a \"|<>*?", true, Meta::default());
        let file = store.add(Some(root), b"a \"|<>*?.txt", false, Meta::default());
        let newline = store.add(Some(root), b"\n", false, Meta::default());
        let raw = store.add(Some(root), b"\xff", false, Meta::default());
        let sub = store.add(Some(root), b"sub", true, Meta::default());
        let nested = store.add(Some(sub), b"a \"|<>*?.txt", false, Meta::default());
        let sibling = store.add(None, b"/tmp/a \"|<>*?other", true, Meta::default());
        let other = store.add(Some(sibling), b"other.txt", false, Meta::default());
        let check = |text: String, id| {
            Query::parse(&text, QueryOptions::default())
                .unwrap()
                .matches(&store, id, &mut EvalScratch::default())
        };
        for recursive in [false, true] {
            let q = query(&store.path(root), true, false, recursive, true, false).unwrap();
            assert!(check(q.clone(), file));
            assert!(check(q.clone(), newline));
            assert!(check(q.clone(), raw));
            assert!(!check(q.clone(), other));
            assert_eq!(check(q, nested), recursive);
        }
        let q = query(&store.path(file), false, false, true, true, false).unwrap();
        assert!(check(q.clone(), file));
        assert!(check(q, nested));
        let q = query(&store.path(file), false, true, false, true, false).unwrap();
        assert!(check(q, file));
        assert!(query(Path::new("/"), true, true, true, true, false).is_none());
    }

    #[test]
    fn filename_queries_handle_non_utf8_quotes_and_case_matching() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp", true, Meta::default());
        let names: &[&[u8]] = &[b"Ab\"*.txt", b"ab\"*.txt", b"Ab\"*.txt.more", b"a\xff\"|b"];
        let ids: Vec<_> = names
            .iter()
            .map(|n| store.add(Some(root), n, false, Meta::default()))
            .collect();
        for (case, exact, expected) in [
            (true, true, vec![true, false, false]),
            (false, true, vec![true, true, false]),
            (true, false, vec![true, false, true]),
        ] {
            let text = query(&store.path(ids[0]), false, false, true, exact, case).unwrap();
            let q = Query::parse(&text, QueryOptions::default()).unwrap();
            let actual: Vec<_> = ids[..3]
                .iter()
                .map(|&id| q.matches(&store, id, &mut EvalScratch::default()))
                .collect();
            assert_eq!(actual, expected);
        }
        let path = Path::new("/tmp").join(OsString::from_vec(names[3].to_vec()));
        let q = Query::parse(
            &query(&path, false, false, true, true, true).unwrap(),
            QueryOptions::default(),
        )
        .unwrap();
        assert!(q.matches(&store, ids[3], &mut EvalScratch::default()));
    }

    #[test]
    fn new_search_window_keeps_index_filter_and_query_as_separate_arguments() {
        let base: Vec<OsString> = ["--new-instance", "--root", "/a b", "--index-file", "/x y"]
            .into_iter()
            .map(Into::into)
            .collect();
        let query = "--case | \"name\"";
        let args = window_args(&base, query, QueryOptions::default());
        assert_eq!(&args[..base.len()], base);
        assert_eq!(args[base.len()], "--filter");
        assert_eq!(
            &args[args.len() - 2..],
            [OsString::from("--"), OsString::from(query)]
        );
        assert!(!args.iter().any(|a| a == "--regex"));
    }
}
