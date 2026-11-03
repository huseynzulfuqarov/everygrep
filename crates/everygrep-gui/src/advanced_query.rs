//! Build explicit query-language terms; never interpolate unquoted operators.
use crate::i18n::tr;
use everygrep_core::localtime::days_in_month;
use everygrep_core::query::{Query, QueryOptions};
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct Fields {
    pub all: String,
    pub phrase: String,
    pub any: String,
    pub none: String,
    pub in_path: bool,
    pub location: String,
    pub recursive: bool,
    // 0 all, 1 files, 2 folders, 3 files without an extension.
    pub kind: i32,
    pub extensions: String,
    pub min_size: String,
    pub max_size: String,
    // bytes, KiB, MiB, GiB.
    pub size_unit: i32,
    pub dates: [(String, String); 3],
    /// Text to find inside the files; empty adds no content term.
    pub content: String,
    pub content_case: bool,
    pub content_whole_words: bool,
    /// The content text is a regular expression.
    pub content_regex: bool,
    // 0 automatic, 1 the code page of Settings, 2 UTF-8, 3 UTF-16 LE,
    // 4 UTF-16 BE.
    pub content_encoding: i32,
}

/// The query function that reads contents in each encoding of the window.
const CONTENT_FUNCTIONS: [&str; 5] = [
    "content",
    "content-codepage",
    "content-utf8",
    "content-utf16le",
    "content-utf16be",
];

/// A regular expression the user wrote, made fit for query quotes: a quote
/// or a control character (escaped or not) becomes its `\x{..}` form, which
/// means the same character, inside a class too.
fn quotable_regex(pattern: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        let c = if c == '\\' {
            match chars.next() {
                Some(next) if next != '"' && !next.is_control() => {
                    out.push('\\');
                    out.push(next);
                    continue;
                }
                Some(next) => next,
                None => {
                    out.push('\\');
                    break;
                }
            }
        } else {
            c
        };
        if c == '"' || c.is_control() {
            write!(out, "\\x{{{:x}}}", c as u32).expect("write to string");
        } else {
            out.push(c);
        }
    }
    out
}

/// Uses the query engine's own quoted literals and wildcard rules. Regex
/// handles literal quotes/control characters, which query quotes cannot hold.
fn term(text: &str, path: bool, wildcard: bool) -> String {
    let scope = if path { "path" } else { "nopath" };
    if text.contains('"') || text.chars().any(char::is_control) || (!path && text.contains('/')) {
        let mut pattern = String::new();
        let mut chars = text.chars().peekable();
        let glob = wildcard && text.contains(['*', '?']);
        while let Some(c) = chars.next() {
            match c {
                '*' if glob => {
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        pattern.push_str("(?s:.*)");
                    } else {
                        pattern.push_str("[^/]*");
                    }
                }
                '?' if glob => pattern.push_str("[^/]"),
                c => pattern.push_str(&crate::fileops_search::literal(c.to_string().as_bytes())),
            }
        }
        if glob {
            pattern = format!("^{pattern}$");
        }
        return format!("{scope}:regex:\"{pattern}\"");
    }
    let mut quoted = String::from("\"");
    for c in text.chars() {
        if wildcard && matches!(c, '*' | '?') {
            quoted.push('"');
            quoted.push(c);
            quoted.push('"');
        } else {
            quoted.push(c);
        }
    }
    quoted.push('"');
    format!("{scope}:noregex:{quoted}")
}

fn size(text: &str, unit: i32) -> Option<u64> {
    let unit = [1u128, 1 << 10, 1 << 20, 1 << 30]
        .get(usize::try_from(unit).ok()?)
        .copied()?;
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 18
    {
        return None;
    }
    let mantissa = format!("{whole}{fraction}").parse::<u128>().ok()?;
    let scale = 10u128.pow(fraction.len() as u32);
    u64::try_from(mantissa.checked_mul(unit)?.div_ceil(scale)).ok()
}

fn valid_date(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return false;
    }
    let year = text[..4].parse::<i64>().unwrap_or(0);
    let month = text[5..7].parse::<u32>().unwrap_or(0);
    let day = text[8..].parse::<u32>().unwrap_or(0);
    year > 0 && day > 0 && day <= days_in_month(year, month)
}

impl Fields {
    /// One `content:` term for the content fields: its own case, whole-word
    /// and regex settings (the window's apply to names), the text quoted so
    /// no operator or modifier inside it takes effect.
    fn content_term(&self) -> Result<Option<String>, String> {
        let function = usize::try_from(self.content_encoding)
            .ok()
            .and_then(|i| CONTENT_FUNCTIONS.get(i))
            .ok_or_else(|| tr("Choose a supported text encoding.").to_owned())?;
        if self.content.is_empty() {
            return Ok(None);
        }
        if self.content.contains(['\n', '\r']) {
            return Err(tr(
                "Remove the line break: a space in the text also matches a line end when matches may run over line ends (Settings).",
            )
            .into());
        }
        let case = if self.content_case { "case" } else { "nocase" };
        let whole = if self.content_whole_words {
            "wholeword"
        } else {
            "nowholeword"
        };
        let text = &self.content;
        let (regex, quoted) = if self.content_regex {
            (true, quotable_regex(text))
        } else if text.contains('"') || text.chars().any(char::is_control) {
            (true, crate::fileops_search::literal(text.as_bytes()))
        } else {
            (false, text.clone())
        };
        let regex = if regex { "regex" } else { "noregex" };
        Ok(Some(format!(
            "{case}:{whole}:{regex}:{function}:\"{quoted}\""
        )))
    }

    pub fn build(&self, options: QueryOptions) -> Result<String, String> {
        if [
            &self.all,
            &self.phrase,
            &self.any,
            &self.none,
            &self.location,
            &self.content,
        ]
        .iter()
        .any(|s| s.contains('\0'))
        {
            return Err(tr("Search fields cannot contain NUL characters.").into());
        }
        let mut parts: Vec<String> = self
            .all
            .split_whitespace()
            .map(|s| term(s, self.in_path, true))
            .collect();
        if !self.phrase.is_empty() {
            parts.push(term(&self.phrase, self.in_path, false));
        }
        let any: Vec<_> = self
            .any
            .split_whitespace()
            .map(|s| term(s, self.in_path, true))
            .collect();
        if !any.is_empty() {
            parts.push(format!("<{}>", any.join(" | ")));
        }
        parts.extend(
            self.none
                .split_whitespace()
                .map(|s| format!("!{}", term(s, self.in_path, true))),
        );
        if !self.location.trim().is_empty() {
            let path = Path::new(self.location.trim());
            if !path.is_absolute() {
                return Err(tr("Enter an absolute folder path.").into());
            }
            if let Some(query) =
                crate::fileops_search::query(path, true, false, self.recursive, false, true)
            {
                parts.push(query);
            }
        }
        match self.kind {
            0 => {}
            1 => parts.push("file:".into()),
            2 => parts.push("folder:".into()),
            3 => parts.push("file: ext:".into()),
            _ => return Err(tr("Choose a supported file type.").into()),
        }
        if !self.extensions.trim().is_empty() {
            if matches!(self.kind, 2 | 3) {
                return Err(
                    tr("Choose Files or Files and folders to use an extension list.").into(),
                );
            }
            let extensions: Vec<_> = self
                .extensions
                .split(';')
                .map(|s| s.trim().trim_start_matches('.'))
                .collect();
            if extensions.iter().any(|s| {
                s.is_empty()
                    || !s
                        .chars()
                        .all(|c| c.is_alphanumeric() || "_-+.*?".contains(c))
            }) {
                return Err(tr("Separate extensions with semicolons, for example pdf;txt.").into());
            }
            parts.push(format!("ext:{}", extensions.join(";")));
        }
        let parse_size = |text: &str| -> Result<Option<u64>, String> {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                size(text.trim(), self.size_unit)
                    .map(Some)
                    .ok_or_else(|| tr("Enter a valid size range.").into())
            }
        };
        let (min, max) = (parse_size(&self.min_size)?, parse_size(&self.max_size)?);
        if min.zip(max).is_some_and(|(a, b)| a > b) {
            return Err(tr("The minimum must not exceed the maximum.").into());
        }
        if let Some(n) = min {
            parts.push(format!("size:>={n}"));
        }
        if let Some(n) = max {
            parts.push(format!("size:<={n}"));
        }
        for (field, (from, to)) in ["modified", "created", "accessed"]
            .into_iter()
            .zip(&self.dates)
        {
            let (from, to) = (from.trim(), to.trim());
            if [from, to].iter().any(|s| !s.is_empty() && !valid_date(s)) {
                return Err(tr("Use valid dates in YYYY-MM-DD format.").into());
            }
            if !from.is_empty() && !to.is_empty() && from > to {
                return Err(tr("The start date must not follow the end date.").into());
            }
            if !from.is_empty() {
                parts.push(format!("{field}:>={from}"));
            }
            if !to.is_empty() {
                parts.push(format!("{field}:<={to}"));
            }
        }
        if let Some(content) = self.content_term()? {
            parts.push(content);
        }
        let query = parts.join(" ");
        Query::parse(
            &query,
            QueryOptions {
                regex: false,
                ..options
            },
        )
        .map_err(|e| crate::search_errors::message(&e.to_string()))?;
        Ok(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::query::EvalScratch;
    use everygrep_core::store::{Meta, NodeStore};

    fn matches(f: &Fields, name: &[u8], directory: bool) -> bool {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/project", true, Meta::default());
        let id = store.add(Some(root), name, directory, Meta::default());
        for precedence in [false, true] {
            let options = QueryOptions {
                and_before_or: precedence,
                literal_operators: true,
                ..QueryOptions::default()
            };
            let text = f.build(options).unwrap();
            let q = Query::parse(&text, options).unwrap();
            let found = q.matches(&store, id, &mut EvalScratch::default());
            if precedence {
                return found;
            }
            let other = Query::parse(
                &text,
                QueryOptions {
                    and_before_or: true,
                    ..options
                },
            )
            .unwrap();
            assert_eq!(
                found,
                other.matches(&store, id, &mut EvalScratch::default())
            );
        }
        unreachable!()
    }

    #[test]
    fn invalid_content_regex_uses_the_gui_diagnostic() {
        let fields = Fields {
            content: "(".into(),
            content_regex: true,
            ..Fields::default()
        };
        assert_eq!(
            fields.build(QueryOptions::default()).unwrap_err(),
            crate::search_errors::message("bad regular expression: unclosed group")
        );
    }

    #[test]
    fn extension_lists_reject_incompatible_file_kinds() {
        for kind in [2, 3] {
            let mut fields = Fields {
                kind,
                extensions: "pdf;txt".into(),
                ..Fields::default()
            };
            assert!(fields.build(QueryOptions::default()).is_err());
            fields.extensions.clear();
            assert!(fields.build(QueryOptions::default()).is_ok());
        }
        for kind in [0, 1] {
            let fields = Fields {
                kind,
                extensions: "pdf;txt".into(),
                ..Fields::default()
            };
            assert!(fields.build(QueryOptions::default()).is_ok());
        }
    }

    #[test]
    fn words_phrase_alternatives_and_negation_use_explicit_groups() {
        let f = Fields {
            all: "report 2026".into(),
            phrase: "final draft".into(),
            any: "pdf txt".into(),
            none: "old backup".into(),
            ..Fields::default()
        };
        assert!(matches(&f, b"report 2026 final draft.txt", false));
        for name in [
            b"report 2026 final draft.old.txt".as_slice(),
            b"report final draft.txt",
            b"report 2026 draft final.txt",
            b"report 2026 final draft.rs",
        ] {
            assert!(!matches(&f, name, false));
        }
    }

    #[test]
    fn operators_quotes_unicode_and_phrase_wildcards_are_literal() {
        for value in [
            "OR",
            "file:",
            "!name",
            "<x|y>",
            "a \"quote\"",
            "*?",
            "Şəki",
            "a\nb",
        ] {
            let f = Fields {
                phrase: value.into(),
                ..Fields::default()
            };
            assert!(
                matches(&f, format!("prefix {value} suffix").as_bytes(), false),
                "{value}"
            );
            assert!(!matches(&f, b"unrelated", false), "{value}");
        }
        let f = Fields {
            all: "*.txt".into(),
            kind: 1,
            ..Fields::default()
        };
        assert!(matches(&f, b"notes.txt", false));
        assert!(!matches(&f, b"notes.txt.bak", false));
        assert!(!matches(&f, b"notes.txt", true));
        let mut f = f;
        f.phrase = "\0".into();
        assert!(f.build(QueryOptions::default()).is_err());
    }

    #[test]
    fn name_path_and_location_are_distinct_and_do_not_include_siblings() {
        let mut f = Fields {
            all: "project".into(),
            ..Fields::default()
        };
        assert!(!matches(&f, b"file.txt", false));
        f.in_path = true;
        assert!(matches(&f, b"file.txt", false));
        f.all = "project/file".into();
        f.in_path = false;
        assert!(!matches(&f, b"file.txt", false));
        f.all.clear();
        f.location = "/tmp/project".into();
        assert!(matches(&f, b"file.txt", false));
        f.location = "/tmp/pro".into();
        assert!(!matches(&f, b"file.txt", false));
        f.location = "relative".into();
        assert!(f.build(QueryOptions::default()).is_err());
    }

    #[test]
    fn file_kinds_extensions_and_size_ranges_are_validated() {
        let mut f = Fields {
            kind: 3,
            ..Fields::default()
        };
        assert!(matches(&f, b"README", false));
        assert!(!matches(&f, b"readme.txt", false));
        assert!(!matches(&f, b"README", true));
        f.kind = 2;
        assert!(matches(&f, b"folder", true));
        assert!(!matches(&f, b"folder", false));
        f.kind = 1;
        f.extensions = ".rs; txt".into();
        assert!(matches(&f, b"a.rs", false));
        assert!(!matches(&f, b"a.jpg", false));
        f.extensions = "txt | file:".into();
        assert!(f.build(QueryOptions::default()).is_err());
        f.extensions.clear();
        f.min_size = "1.5".into();
        f.max_size = "2".into();
        f.size_unit = 1;
        let query = f.build(QueryOptions::default()).unwrap();
        assert!(query.contains("size:>=1536 size:<=2048"));
        f.max_size = "1".into();
        assert!(f.build(QueryOptions::default()).is_err());
        for bad in ["-1", "1e5", "1|folder:", "18446744073709551616", "1.1.1"] {
            f.min_size = bad.into();
            assert!(f.build(QueryOptions::default()).is_err(), "{bad}");
        }
        assert_eq!(size("0", 0), Some(0));
        assert_eq!(size("0.5", 0), Some(1));
    }

    #[test]
    fn location_recursion_and_numeric_boundaries_match_real_nodes() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/tmp/project", true, Meta::default());
        let sub = store.add(Some(root), b"sub", true, Meta::default());
        let zone = everygrep_core::localtime::Zone::local();
        let start = zone.local_midnight_to_unix(2024, 2, 29) * 1_000_000_000;
        let end = zone.local_midnight_to_unix(2024, 3, 2) * 1_000_000_000 - 1;
        let ids = [
            store.add(
                Some(root),
                b"first",
                false,
                Meta {
                    size: 1024,
                    mtime_ns: start,
                },
            ),
            store.add(
                Some(root),
                b"last",
                false,
                Meta {
                    size: 2048,
                    mtime_ns: end,
                },
            ),
            store.add(
                Some(root),
                b"small",
                false,
                Meta {
                    size: 1023,
                    mtime_ns: start,
                },
            ),
            store.add(
                Some(root),
                b"large",
                false,
                Meta {
                    size: 2049,
                    mtime_ns: start,
                },
            ),
            store.add(
                Some(root),
                b"early",
                false,
                Meta {
                    size: 1024,
                    mtime_ns: start - 1,
                },
            ),
            store.add(
                Some(root),
                b"late",
                false,
                Meta {
                    size: 1024,
                    mtime_ns: end + 1,
                },
            ),
            store.add(
                Some(sub),
                b"nested",
                false,
                Meta {
                    size: 1024,
                    mtime_ns: start,
                },
            ),
        ];
        let mut f = Fields {
            location: "/tmp/project".into(),
            kind: 1,
            min_size: "1".into(),
            max_size: "2".into(),
            size_unit: 1,
            ..Fields::default()
        };
        f.dates[0] = ("2024-02-29".into(), "2024-03-01".into());
        for recursive in [false, true] {
            f.recursive = recursive;
            let q = Query::parse(
                &f.build(QueryOptions::default()).unwrap(),
                QueryOptions::default(),
            )
            .unwrap();
            let matched: Vec<_> = ids
                .iter()
                .map(|id| q.matches(&store, *id, &mut EvalScratch::default()))
                .collect();
            assert_eq!(matched, [true, true, false, false, false, false, recursive]);
        }
    }

    #[test]
    fn date_ranges_are_inclusive_and_allow_either_boundary() {
        let mut f = Fields::default();
        f.dates[0] = ("2024-02-29".into(), "2024-03-01".into());
        f.dates[1].0 = "2020-01-01".into();
        f.dates[2].1 = "2026-11-02".into();
        let text = f.build(QueryOptions::default()).unwrap();
        assert!(text.contains("modified:>=2024-02-29 modified:<=2024-03-01"));
        assert!(text.contains("created:>=2020-01-01 accessed:<=2026-11-02"));
        for bad in [
            "2023-02-29",
            "2024-13-01",
            "2024-00-10",
            "2024-01-32",
            "today",
            "2024-01-01 | x",
        ] {
            f.dates[0].0 = bad.into();
            assert!(f.build(QueryOptions::default()).is_err());
        }
        f.dates[0] = ("2025-01-01".into(), "2024-01-01".into());
        assert!(f.build(QueryOptions::default()).is_err());
        assert_eq!(
            Fields::default().build(QueryOptions::default()).unwrap(),
            ""
        );
    }

    /// Whether each file (name, bytes), written to a new folder, matches the
    /// query of `f`, for both operator precedences (which must agree).
    fn content_matches(f: &Fields, files: &[(&str, &[u8])]) -> Vec<bool> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "everygrep-advanced-content-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = NodeStore::new();
        let root = store.add(
            None,
            dir.as_os_str().as_encoded_bytes(),
            true,
            Meta::default(),
        );
        let ids: Vec<_> = files
            .iter()
            .map(|(name, bytes)| {
                std::fs::write(dir.join(name), bytes).unwrap();
                store.add(
                    Some(root),
                    name.as_bytes(),
                    false,
                    Meta {
                        size: bytes.len() as u64,
                        // 2023-11-14, a day in every time zone.
                        mtime_ns: 1_700_000_000_000_000_000,
                    },
                )
            })
            .collect();
        let mut answers = Vec::new();
        for and_before_or in [false, true] {
            let options = QueryOptions {
                and_before_or,
                literal_operators: true,
                ..QueryOptions::default()
            };
            let q = Query::parse(&f.build(options).unwrap(), options).unwrap();
            assert_eq!(q.content_terms().len(), 1);
            answers.push(
                ids.iter()
                    .map(|id| q.matches(&store, *id, &mut EvalScratch::default()))
                    .collect::<Vec<_>>(),
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(answers[0], answers[1]);
        answers.pop().unwrap()
    }

    fn content(text: &str) -> Fields {
        Fields {
            content: text.into(),
            ..Fields::default()
        }
    }

    #[test]
    fn content_text_is_one_literal_phrase_whatever_it_holds() {
        for text in [
            "two words",
            "a|b <c> !d",
            "OR NOT",
            "name:x size:1 ext:txt",
            "content:inner",
            "regex:^x",
            "*.txt ?",
            "http://example.org/a?b",
            "say \"hi\" now",
            "tab\there",
            "back\\slash \\d+",
            "  spaced  ",
        ] {
            let f = content(text);
            let hit = format!("before {text} after\n");
            assert_eq!(
                content_matches(
                    &f,
                    &[
                        ("hit.txt", hit.as_bytes()),
                        ("miss.txt", b"words\ntwo a b c d say hi now tab here\n"),
                    ]
                ),
                [true, false],
                "{text:?}"
            );
        }
        assert_eq!(
            Fields::default().build(QueryOptions::default()).unwrap(),
            ""
        );
    }

    #[test]
    fn content_case_and_whole_words_are_its_own_settings() {
        let files: &[(&str, &[u8])] = &[
            ("exact.txt", "Gəncə şəhəri\n".as_bytes()),
            ("upper.txt", "GƏNCƏ ŞƏHƏRİ\n".as_bytes()),
            ("part.txt", "Gəncəli\n".as_bytes()),
        ];
        let mut f = content("Gəncə");
        assert_eq!(content_matches(&f, files), [true, true, true]);
        f.content_whole_words = true;
        assert_eq!(content_matches(&f, files), [true, true, false]);
        f.content = "şəhər".into();
        f.content_whole_words = false;
        assert_eq!(content_matches(&f, files), [true, true, false]);
        f.content_case = true;
        assert_eq!(content_matches(&f, files), [true, false, false]);
        // The window's Match case and Whole words do not change the term.
        f.content_case = false;
        let text = f
            .build(QueryOptions {
                case_sensitive: true,
                whole_words: true,
                ..QueryOptions::default()
            })
            .unwrap();
        assert_eq!(text, "nocase:nowholeword:noregex:content:\"şəhər\"");
        let q = Query::parse(
            &text,
            QueryOptions {
                case_sensitive: true,
                whole_words: true,
                ..QueryOptions::default()
            },
        )
        .unwrap();
        assert_eq!(q.content_terms()[0].key(), "Auto/0/0/0/şəhər");
    }

    #[test]
    fn content_regex_keeps_its_syntax_and_may_hold_quotes() {
        let files: &[(&str, &[u8])] = &[
            ("main.rs", b"// x\nfn main() {\n"),
            ("quote.txt", b"say \"yo\" twice\n"),
            ("plain.txt", b"function main and say yo\n"),
        ];
        let mut f = Fields {
            content: r"^fn \w+\(".into(),
            content_regex: true,
            ..Fields::default()
        };
        assert_eq!(content_matches(&f, files), [true, false, false]);
        for pattern in [r#"say "(hi|yo)""#, r#"say \"y[^"]\""#, r"say \x22yo"] {
            f.content = pattern.into();
            assert_eq!(
                content_matches(&f, files),
                [false, true, false],
                "{pattern}"
            );
        }
        f.content = "a|b<c>".into();
        assert!(
            f.build(QueryOptions::default())
                .unwrap()
                .contains("\"a|b<c>\"")
        );
        assert_eq!(quotable_regex("a\"\\\"\t\\"), "a\\x{22}\\x{22}\\x{9}\\");
        for bad in ["(unclosed", "a\nb", "a\rb", "\0"] {
            f.content = bad.into();
            assert!(f.build(QueryOptions::default()).is_err(), "{bad:?}");
            f.content_regex = false;
            if bad.contains(['\n', '\r', '\0']) {
                assert!(f.build(QueryOptions::default()).is_err(), "{bad:?}");
            }
            f.content_regex = true;
        }
    }

    #[test]
    fn content_encodings_read_the_chosen_form() {
        fn utf16(text: &str, big_endian: bool) -> Vec<u8> {
            text.encode_utf16()
                .flat_map(|u| {
                    if big_endian {
                        u.to_be_bytes()
                    } else {
                        u.to_le_bytes()
                    }
                })
                .collect()
        }
        let le = utf16("qovluq şəkil\n", false);
        let be = utf16("qovluq şəkil\n", true);
        // "şık" in Windows-1254, the default code page of Settings.
        let ansi: &[u8] = b"\xfe\xfdk\n";
        let files: &[(&str, &[u8])] = &[
            ("utf8.txt", "qovluq şəkil şık\n".as_bytes()),
            ("le.txt", &le),
            ("be.txt", &be),
            ("ansi.txt", ansi),
        ];
        let mut f = content("şəkil");
        let expected = [
            [true, false, false, false],
            [false, false, false, false],
            [true, false, false, false],
            [false, true, false, false],
            [false, false, true, false],
        ];
        for (encoding, expected) in expected.iter().enumerate() {
            f.content_encoding = encoding as i32;
            assert_eq!(&content_matches(&f, files), expected, "{encoding}");
        }
        f.content = "şık".into();
        let expected = [
            [true, false, false, true],
            [false, false, false, true],
            [true, false, false, false],
        ];
        for (encoding, expected) in expected.iter().enumerate() {
            f.content_encoding = encoding as i32;
            assert_eq!(&content_matches(&f, files), expected, "{encoding}");
        }
        for bad in [-1, 5] {
            f.content_encoding = bad;
            assert!(f.build(QueryOptions::default()).is_err());
            f.content.clear();
            assert!(f.build(QueryOptions::default()).is_err());
            f.content = "x".into();
        }
    }

    #[test]
    fn content_term_combines_with_name_path_size_and_date_fields() {
        let mut f = Fields {
            all: "report".into(),
            any: "2025 2026".into(),
            none: "old".into(),
            kind: 1,
            extensions: "txt;md".into(),
            min_size: "11".into(),
            content: "total: 42".into(),
            ..Fields::default()
        };
        f.dates[0] = ("1970-01-01".into(), String::new());
        let body: &[u8] = b"header\ntotal: 42 AZN\n";
        let files: &[(&str, &[u8])] = &[
            ("report 2026.txt", body),
            ("report 2026.md", b"total: 41\n"),
            ("report 2024.txt", body),
            ("report 2026 old.txt", body),
            ("report 2025.pdf", body),
            ("notes 2026.txt", body),
            ("report 2025.md", b"total: 42\n"),
            ("report 2025.txt", b"total: 42"),
        ];
        assert_eq!(
            content_matches(&f, files),
            [true, false, false, false, false, false, false, false]
        );
        f.min_size = String::new();
        assert_eq!(
            content_matches(&f, files),
            [true, false, false, false, false, false, true, true]
        );
        let text = f.build(QueryOptions::default()).unwrap();
        assert!(
            text.ends_with(
                " modified:>=1970-01-01 nocase:nowholeword:noregex:content:\"total: 42\""
            ),
            "{text}"
        );
        f.in_path = true;
        f.all = "everygrep-advanced-content".into();
        f.any.clear();
        assert_eq!(
            content_matches(&f, files),
            [true, false, true, false, false, true, true, true]
        );
    }

    #[test]
    fn the_content_term_leaves_room_for_five_more_in_a_filter() {
        let f = content("one");
        let text = f.build(QueryOptions::default()).unwrap();
        let filter = |n: usize| {
            let words: Vec<_> = (0..n).map(|i| format!("w{i}")).collect();
            Query::parse(
                &format!("content:<{}>", words.join(" ")),
                QueryOptions::default(),
            )
            .unwrap()
        };
        let query = || Query::parse(&text, QueryOptions::default()).unwrap();
        let joined = query().and(filter(5)).unwrap();
        assert_eq!(joined.content_terms().len(), 6);
        assert!(query().and(filter(6)).is_err());
    }
}
