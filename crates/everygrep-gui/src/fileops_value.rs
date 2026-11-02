//! Typed context-search values, escaped independently of query syntax.
use crate::{
    AppWindow,
    engine::Engine,
    fileops_search::{literal, query},
    results::ResultsModel,
};
use everygrep_core::query::QueryOptions;
use slint::ComponentHandle;
use std::{cell::RefCell, ffi::OsString, path::Path, rc::Rc, sync::Arc};

pub enum Value<'a> {
    Name(&'a [u8]),
    Folder(&'a Path),
    Size(u64),
    Day(i64, u32, u32),
    Extension(&'a [u8]),
}

pub fn filename(name: &[u8], exact: bool, case: bool) -> String {
    let pattern = literal(name);
    let pattern = if exact {
        format!("^{pattern}$")
    } else {
        pattern
    };
    format!(
        "{}:nopath:regex:\"{pattern}\"",
        if case { "case" } else { "nocase" }
    )
}

/// Compare the part before a file's last non-leading dot. Folder names
/// and dotfiles without another dot keep their whole name.
pub fn stem(name: &[u8], is_dir: bool, exact: bool, case: bool) -> String {
    let stem = if is_dir {
        name
    } else {
        name.iter()
            .rposition(|&b| b == b'.')
            .filter(|&i| i > 0)
            .map_or(name, |i| &name[..i])
    };
    let text = literal(stem);
    let mode = if case { "case" } else { "nocase" };
    let folder = filename(stem, exact, case);
    let with_ext = if exact {
        format!("{text}(?-u:\\.[^.]*)")
    } else {
        format!("(?s-u:.*){text}(?s-u:.*\\.[^.]*)")
    };
    let with_ext = format!("{mode}:nopath:regex:\"(?s)^{with_ext}$\"");
    let without_ext = if exact {
        if stem.iter().enumerate().any(|(i, b)| i > 0 && *b == b'.') {
            None
        } else {
            Some(filename(stem, true, case))
        }
    } else {
        Some(format!(
            "<{mode}:nopath:regex:\"(?s-u)^(?:[^.]+|\\.[^.]*)$\" {} >",
            filename(stem, false, case)
        ))
    };
    let file = match without_ext {
        Some(q) => format!("<{with_ext} | {q} >"),
        None => with_ext,
    };
    format!("< <folder: {folder} > | <file: {file} > >")
}

pub fn build(value: Value<'_>, case: bool) -> Option<String> {
    Some(match value {
        Value::Name(bytes) => filename(bytes, true, case),
        Value::Folder(path) => query(path, true, false, false, true, true)?,
        Value::Size(bytes) => format!("size:={bytes}"),
        Value::Day(y, m, d) if (1..=9999).contains(&y) => format!("modified:{y:04}-{m:02}-{d:02}"),
        Value::Day(..) => return None,
        Value::Extension(ext) => {
            let mode = if case { "case" } else { "nocase" };
            if ext.is_empty() {
                // Empty as displayed: dotfiles, names with no dot, and a
                // trailing dot all have an empty last extension.
                format!("{mode}:file:nopath:regex:\"(?s-u)^(?:[^.]+|\\.[^.]*|.+\\.)$\"")
            } else if ext.iter().all(u8::is_ascii_alphanumeric) {
                format!("{mode}:ext:{}", std::str::from_utf8(ext).ok()?)
            } else {
                format!(
                    "{mode}:file:nopath:regex:\"(?s-u:^.+\\.){}$\"",
                    literal(ext)
                )
            }
        }
    })
}

#[derive(Clone)]
struct FrozenNames {
    queries: [String; 4],
    options: QueryOptions,
}

pub fn connect(
    w: &AppWindow,
    model: &Rc<ResultsModel>,
    engine: &Arc<Engine>,
    base_args: Vec<OsString>,
) {
    // Freeze the typed query and matching/filter choices when the menu opens.
    // A watcher update cannot change which value its menu action searches.
    let pending: Rc<RefCell<Option<(String, QueryOptions)>>> = Rc::default();
    let names: Rc<RefCell<Option<FrozenNames>>> = Rc::default();
    w.on_fileops_prepare_value({
        let (weak, model, pending, names) =
            (w.as_weak(), model.clone(), pending.clone(), names.clone());
        move |row, column| {
            pending.borrow_mut().take();
            names.borrow_mut().take();
            let (Some(w), Ok(row)) = (weak.upgrade(), usize::try_from(row)) else {
                return false;
            };
            let mut options = crate::options(&w);
            options.regex = false;
            *names.borrow_mut() = model
                .filename_queries(row, options.case_sensitive)
                .map(|queries| FrozenNames { queries, options });
            let Some(query) = model.value_query(row, column, options.case_sensitive) else {
                return false;
            };
            *pending.borrow_mut() = Some((query, options));
            true
        }
    });
    w.on_fileops_search_value({
        let (weak, engine, base_args) = (w.as_weak(), engine.clone(), base_args.clone());
        move || {
            let Some(w) = weak.upgrade() else {
                return;
            };
            if let Some((query, options)) = pending.borrow().clone() {
                crate::fileops_search::apply(&w, &engine, &base_args, &query, options);
            }
        }
    });
    w.on_fileops_search_filename({
        let (weak, model, engine) = (w.as_weak(), model.clone(), engine.clone());
        move |row, exact, stem| {
            let Some(w) = weak.upgrade() else {
                return;
            };
            let frozen = if row < 0 {
                names.borrow().clone()
            } else {
                let mut options = crate::options(&w);
                options.regex = false;
                usize::try_from(row)
                    .ok()
                    .and_then(|row| model.filename_queries(row, options.case_sensitive))
                    .map(|queries| FrozenNames { queries, options })
            };
            if let Some(frozen) = frozen {
                let query = &frozen.queries[usize::from(exact) + 2 * usize::from(stem)];
                crate::fileops_search::apply(&w, &engine, &base_args, query, frozen.options);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::{
        query::{EvalScratch, Query},
        store::{Meta, NodeStore},
    };
    fn matches(query: &str, store: &NodeStore, id: everygrep_core::store::NodeId) -> bool {
        Query::parse(query, QueryOptions::default())
            .unwrap()
            .matches(store, id, &mut EvalScratch::default())
    }
    #[test]
    fn values_use_raw_bytes_and_cannot_inject_query_operators() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp", true, Meta::default());
        for name in [b"a \"|<>*.txt".as_slice(), b"a.weird;*?\"|<>\n", b"a.\xff*"] {
            let id = store.add(Some(root), name, false, Meta::default());
            let other = store.add(Some(root), b"other.txt", false, Meta::default());
            let q = build(Value::Name(name), true).unwrap();
            assert!(matches(&q, &store, id));
            assert!(!matches(&q, &store, other));
            let dot = name.iter().rposition(|&c| c == b'.').unwrap();
            let q = build(Value::Extension(&name[dot + 1..]), true).unwrap();
            assert!(matches(&q, &store, id));
            if dot + 1 < name.len() && &name[dot + 1..] != b"txt" {
                assert!(!matches(&q, &store, other));
            }
        }
    }
    #[test]
    fn empty_extension_case_and_exact_bytes_have_property_semantics() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp", true, Meta::default());
        let names = [
            b".bashrc".as_slice(),
            b"plain",
            b"trailing.",
            b"a.TXT",
            b"a.txt",
        ];
        let ids: Vec<_> = names
            .iter()
            .map(|n| {
                store.add(
                    Some(root),
                    n,
                    false,
                    Meta {
                        size: 123,
                        mtime_ns: 0,
                    },
                )
            })
            .collect();
        let q = build(Value::Extension(b""), true).unwrap();
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(matches(&q, &store, id), i < 3);
        }
        let q = build(Value::Extension(b"TXT"), true).unwrap();
        assert!(matches(&q, &store, ids[3]));
        assert!(!matches(&q, &store, ids[4]));
        let q = build(Value::Extension(b"TXT"), false).unwrap();
        assert!(matches(&q, &store, ids[4]));
        let folder = store.add(Some(root), b"folder.TXT", true, Meta::default());
        assert!(!matches(&q, &store, folder));
        let q = build(Value::Size(123), false).unwrap();
        assert!(matches(&q, &store, ids[0]));
        assert!(!matches(&q, &store, folder));
    }
    #[test]
    fn stem_search_excludes_extension_only_matches_and_handles_dotfiles_and_folders() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp", true, Meta::default());
        let names = [
            b"a.tar".as_slice(),
            b"a.tar.gz",
            b"a.tar.x",
            b"z.a.tar.x",
            b"a.tar.more.ext",
            b"other.a.tar",
        ];
        let ids: Vec<_> = names
            .iter()
            .map(|n| store.add(Some(root), n, false, Meta::default()))
            .collect();
        let folder = store.add(Some(root), b"a.tar", true, Meta::default());
        for exact in [false, true] {
            let q = stem(b"a.tar.gz", false, exact, true);
            let actual: Vec<_> = ids.iter().map(|&id| matches(&q, &store, id)).collect();
            assert_eq!(
                actual,
                if exact {
                    vec![false, true, true, false, false, false]
                } else {
                    vec![false, true, true, true, true, false]
                }
            );
            assert!(matches(&q, &store, folder));
        }
        let dot = store.add(Some(root), b".profile", false, Meta::default());
        let dot_ext = store.add(Some(root), b".profile.txt", false, Meta::default());
        let q = stem(b".profile", false, true, true);
        assert!(matches(&q, &store, dot));
        assert!(matches(&q, &store, dot_ext));
        let q = stem(b"a.tar", true, true, true);
        assert!(matches(&q, &store, folder));
        assert!(!matches(&q, &store, ids[0]));
        assert!(matches(&q, &store, ids[1]));
    }

    #[test]
    fn byte_prefixes_and_suffixes_keep_non_utf8_names_searchable() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp", true, Meta::default());
        for name in [
            b"\xff.weird;*".as_slice(),
            b"\xff",
            b"part.\xff.txt",
            b"part.txt.\xff",
        ] {
            let id = store.add(Some(root), name, false, Meta::default());
            let ext = name
                .iter()
                .rposition(|&b| b == b'.')
                .filter(|&i| i > 0)
                .map_or(&b""[..], |i| &name[i + 1..]);
            assert!(matches(
                &build(Value::Extension(ext), true).unwrap(),
                &store,
                id
            ));
            for exact in [false, true] {
                assert!(matches(&stem(name, false, exact, true), &store, id));
            }
        }
    }

    #[test]
    fn filename_commands_and_folder_values_respect_exactness_and_boundaries() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/a \"|*", true, Meta::default());
        let a = store.add(Some(root), b"Ab.txt", false, Meta::default());
        let b = store.add(Some(root), b"ab.txt.more", false, Meta::default());
        assert!(matches(&filename(b"ab.txt", false, false), &store, a));
        assert!(matches(&filename(b"ab.txt", false, false), &store, b));
        assert!(!matches(&filename(b"ab.txt", true, true), &store, a));
        assert!(matches(&filename(b"ab.txt", true, false), &store, a));
        assert!(!matches(&filename(b"ab.txt", true, false), &store, b));
        let sub = store.add(Some(root), b"sub", true, Meta::default());
        let nested = store.add(Some(sub), b"child", false, Meta::default());
        let sibling = store.add(None, b"/tmp/a \"|*extra", true, Meta::default());
        let other = store.add(Some(sibling), b"child", false, Meta::default());
        let q = build(Value::Folder(&store.path(root)), false).unwrap();
        assert!(matches(&q, &store, a));
        assert!(!matches(&q, &store, nested));
        assert!(!matches(&q, &store, other));
        assert_eq!(
            build(Value::Day(2026, 11, 3), false).as_deref(),
            Some("modified:2026-11-03")
        );
        assert!(build(Value::Day(10000, 1, 1), false).is_none());
    }
}
