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
    /// Days of the Created and Accessed columns, read from the disk.
    Created(i64, u32, u32),
    Accessed(i64, u32, u32),
    Extension(&'a [u8]),
    Type(everygrep_core::kind::FileKind, &'a [u8]),
    Attributes(everygrep_core::details::Attributes),
    Hash(everygrep_core::hash::Algorithm, &'a [u8]),
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

/// A one-day search of a date field; years the query cannot write give none.
fn day(field: &str, y: i64, m: u32, d: u32) -> Option<String> {
    (1..=9999)
        .contains(&y)
        .then(|| format!("{field}:{y:04}-{m:02}-{d:02}"))
}

pub fn build(value: Value<'_>, case: bool) -> Option<String> {
    Some(match value {
        Value::Name(bytes) => filename(bytes, true, case),
        Value::Folder(path) => query(path, true, false, false, true, true)?,
        Value::Size(bytes) => format!("size:={bytes}"),
        Value::Day(y, m, d) => day("modified", y, m, d)?,
        Value::Created(y, m, d) => day("created", y, m, d)?,
        Value::Accessed(y, m, d) => day("accessed", y, m, d)?,
        Value::Type(kind, ext) => {
            if kind == everygrep_core::kind::FileKind::Folder {
                "type:folder".into()
            } else {
                format!(
                    "<type:{} {} >",
                    kind.id(),
                    build(Value::Extension(ext), false)?
                )
            }
        }
        Value::Attributes(attributes) => format!("attributes:={}", attributes.letters()),
        Value::Hash(algorithm, bytes) => {
            let value = everygrep_core::hash::hex(bytes);
            algorithm.parse_hex(&value).ok()?;
            format!("{}:{value}", algorithm.keyword())
        }
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
    fn type_values_match_the_displayed_category_and_extension() {
        use everygrep_core::kind::{FileKind, classify};
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp", true, Meta::default());
        let files: Vec<_> = [
            b"CMakeLists.txt".as_slice(),
            b"notes.txt",
            b"NOTES.TXT",
            b"Makefile",
            b"README",
            b"plain",
            b"archive.tar.gz",
            b"archive.gz",
            b"a.weird;*?",
            b"b.weird;*?",
        ]
        .into_iter()
        .map(|name| store.add(Some(root), name, false, Meta::default()))
        .collect();
        for &id in &files {
            let name = store.name(id);
            let text = String::from_utf8_lossy(name);
            let extension = crate::labels::extension(&text, false);
            let kind = classify(name, false);
            let displayed = crate::labels::type_name(kind, extension);
            let query = build(Value::Type(kind, extension.as_bytes()), true).unwrap();
            for &candidate in &files {
                let name = String::from_utf8_lossy(store.name(candidate));
                let other = crate::labels::type_name(
                    classify(store.name(candidate), false),
                    crate::labels::extension(&name, false),
                );
                assert_eq!(
                    matches(&query, &store, candidate),
                    displayed == other,
                    "{query}: {name}"
                );
            }
            assert!(!matches(&query, &store, root));
        }
        let folder = build(Value::Type(FileKind::Folder, b""), false).unwrap();
        assert!(matches(&folder, &store, root));
        assert!(!matches(&folder, &store, files[0]));
    }

    #[test]
    fn attribute_and_hash_values_build_exact_queries_without_injection() {
        use everygrep_core::details::Attributes;
        use everygrep_core::hash::Algorithm;
        assert_eq!(
            build(Value::Attributes(Attributes::default()), true).unwrap(),
            "attributes:="
        );
        assert_eq!(
            build(
                Value::Attributes(Attributes(Attributes::HIDDEN | Attributes::READ_ONLY)),
                false
            )
            .unwrap(),
            "attributes:=RH"
        );
        for a in Algorithm::ALL {
            let len = match a {
                Algorithm::Md5 | Algorithm::Xxh3 => 16,
                Algorithm::Sha1 => 20,
                Algorithm::Sha256 => 32,
                Algorithm::Sha384 => 48,
                Algorithm::Sha512 => 64,
                Algorithm::Crc32 => 4,
            };
            let q = build(Value::Hash(a, &vec![0xab; len]), true).unwrap();
            assert!(q.starts_with(a.keyword()));
            assert!(Query::text_reads_files(&q, QueryOptions::default()));
            assert!(build(Value::Hash(a, b"|<>"), false).is_none());
        }
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

    #[test]
    fn created_and_accessed_days_search_their_own_date_field() {
        for (value, text) in [
            (Value::Created(2026, 1, 9), "created:2026-01-09"),
            (Value::Accessed(2026, 11, 3), "accessed:2026-11-03"),
        ] {
            let q = build(value, false).unwrap();
            assert_eq!(q, text);
            assert!(Query::parse(&q, QueryOptions::default()).is_ok(), "{q}");
        }
        assert!(build(Value::Created(0, 1, 1), false).is_none());
        assert!(build(Value::Accessed(10000, 1, 1), false).is_none());
    }
}
