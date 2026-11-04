//! Bounded filename lists: exact lookup or a shared substring automaton.
use std::collections::{HashSet, VecDeque};
use std::fmt;

use crate::fold::fold;

pub const MAX_NAMES: usize = 10_000;
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub case_sensitive: bool,
    pub ignore_extension: bool,
    pub contains: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Empty,
    TooMany,
    TooLarge,
    InvalidName,
    InvalidEscape,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "enter at least one name",
            Self::TooMany => "at most 10000 names are allowed",
            Self::TooLarge => "the name list exceeds 2 MiB",
            Self::InvalidName => "use file names without paths or NUL characters",
            Self::InvalidEscape => "invalid name list escape; use \\xHH for escaped bytes",
        })
    }
}

impl std::error::Error for Error {}

/// Empty lines are ignored; surrounding spaces are part of a filename.
/// A UTF-8 BOM and CRLF line endings are accepted.
fn lines(text: &str) -> Result<Vec<&str>, Error> {
    if text.len() > MAX_BYTES {
        return Err(Error::TooLarge);
    }
    let mut names = Vec::new();
    for name in text.strip_prefix('\u{feff}').unwrap_or(text).lines() {
        if name.is_empty() {
            continue;
        }
        if names.len() == MAX_NAMES {
            return Err(Error::TooMany);
        }
        if name.contains(['/', '\0', '\r']) {
            return Err(Error::InvalidName);
        }
        names.push(name);
    }
    if names.is_empty() {
        return Err(Error::Empty);
    }
    Ok(names)
}

/// A query that round-trips arbitrary filename punctuation through query
/// quotes. Only backslashes, quotes and control bytes need escaping.
pub fn query(text: &str, options: Options) -> Result<String, Error> {
    lines(text)?;
    use std::fmt::Write;
    let mut out = format!(
        "{}:{}:\"",
        if options.case_sensitive {
            "case"
        } else {
            "nocase"
        },
        match (options.ignore_extension, options.contains) {
            (false, false) => "names",
            (true, false) => "stemnames",
            (false, true) => "containsnames",
            (true, true) => "containsstemnames",
        }
    );
    for c in text.chars() {
        if c == '\\' || c == '"' || c.is_ascii_control() {
            write!(out, "\\x{:02x}", c as u32).expect("write to string");
        } else {
            out.push(c);
        }
    }
    out.push('"');
    Ok(out)
}

pub(crate) fn decode(value: &str) -> Result<String, Error> {
    // Each encoded byte uses at most four bytes; bound before allocating.
    if value.len() > MAX_BYTES * 4 {
        return Err(Error::TooLarge);
    }
    let mut out = Vec::with_capacity(value.len().min(MAX_BYTES));
    let mut bytes = value.bytes();
    while let Some(b) = bytes.next() {
        if b == b'\\' {
            if bytes.next() != Some(b'x') {
                return Err(Error::InvalidEscape);
            }
            let digit = |b: Option<u8>| b.and_then(|b| char::from(b).to_digit(16));
            let a = digit(bytes.next()).ok_or(Error::InvalidEscape)?;
            let b = digit(bytes.next()).ok_or(Error::InvalidEscape)?;
            out.push((a * 16 + b) as u8);
        } else {
            out.push(b);
        }
        if out.len() > MAX_BYTES {
            return Err(Error::TooLarge);
        }
    }
    String::from_utf8(out).map_err(|_| Error::InvalidEscape)
}

pub(crate) fn validate(text: &str) -> Result<(), Error> {
    lines(text).map(|_| ())
}

/// A final dot starts an extension only after a nonempty name. Dotfiles
/// such as .profile therefore retain their full name.
fn stem(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

#[derive(Debug)]
pub(crate) struct NameList {
    options: Options,
    matcher: Matcher,
}

#[derive(Debug)]
enum Matcher {
    Exact(HashSet<Vec<u8>>),
    Contains(Automaton),
}

impl NameList {
    pub(crate) fn new(text: &str, options: Options) -> Result<Self, Error> {
        let names = lines(text)?;
        let mut set = HashSet::with_capacity(names.len());
        for name in names {
            let name = if options.ignore_extension {
                stem(name.as_bytes())
            } else {
                name.as_bytes()
            };
            set.insert(if options.case_sensitive {
                name.to_vec()
            } else {
                fold(name)
            });
        }
        let matcher = if options.contains {
            Matcher::Contains(Automaton::new(set))
        } else {
            Matcher::Exact(set)
        };
        Ok(Self { options, matcher })
    }

    pub(crate) fn case_sensitive(&self) -> bool {
        self.options.case_sensitive
    }

    /// `name` is already case-folded when case matching is off. No allocation
    /// is needed per index entry, including when ignoring extensions.
    pub(crate) fn matches(&self, name: &[u8]) -> bool {
        let name = if self.options.ignore_extension {
            stem(name)
        } else {
            name
        };
        match &self.matcher {
            Matcher::Exact(set) => set.contains(name),
            Matcher::Contains(automaton) => automaton.matches(name),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    edges: Vec<(u8, usize)>,
    failure: usize,
    matched: bool,
}

/// Aho-Corasick with sparse, sorted byte transitions. Matching is linear
/// in the filename length instead of comparing every pasted name in turn.
#[derive(Debug)]
struct Automaton(Vec<State>);

impl Automaton {
    fn next(&self, state: usize, b: u8) -> Option<usize> {
        let edges = &self.0[state].edges;
        edges
            .binary_search_by_key(&b, |&(b, _)| b)
            .ok()
            .map(|at| edges[at].1)
    }

    fn new(patterns: HashSet<Vec<u8>>) -> Self {
        let mut out = Self(vec![State::default()]);
        for pattern in patterns {
            let mut state = 0;
            for b in pattern {
                state = match out.next(state, b) {
                    Some(next) => next,
                    None => {
                        let next = out.0.len();
                        out.0.push(State::default());
                        let edges = &mut out.0[state].edges;
                        let at = edges.binary_search_by_key(&b, |&(b, _)| b).unwrap_err();
                        edges.insert(at, (b, next));
                        next
                    }
                };
            }
            out.0[state].matched = true;
        }
        let mut queue: VecDeque<_> = out.0[0].edges.iter().map(|&(_, next)| next).collect();
        while let Some(state) = queue.pop_front() {
            for (b, next) in out.0[state].edges.clone() {
                let mut failure = out.0[state].failure;
                while failure != 0 && out.next(failure, b).is_none() {
                    failure = out.0[failure].failure;
                }
                let failure = out.next(failure, b).unwrap_or(0);
                out.0[next].failure = failure;
                out.0[next].matched |= out.0[failure].matched;
                queue.push_back(next);
            }
        }
        out
    }

    fn matches(&self, name: &[u8]) -> bool {
        let mut state = 0;
        for &b in name {
            while state != 0 && self.next(state, b).is_none() {
                state = self.0[state].failure;
            }
            state = self.next(state, b).unwrap_or(0);
            if self.0[state].matched {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use crate::progress::{Cancelled, Progress};
    use crate::query::{EvalScratch, Query, QueryOptions};
    use crate::store::{Meta, NodeStore};

    fn parsed(text: &str, options: Options) -> Query {
        Query::parse(&query(text, options).unwrap(), QueryOptions::default()).unwrap()
    }

    #[test]
    fn exact_lists_keep_punctuation_spaces_unicode_and_literal_operators() {
        let names = [
            "*.txt",
            "a|b",
            "!x <y>",
            "OR",
            "name:foo",
            "a\"quote",
            "back\\slash",
            " Şəkil ",
        ];
        let text = format!("\u{feff}{}\r\n\r\n", names.join("\r\n"));
        let q = parsed(&text, Options::default());
        let mut s = NodeStore::new();
        let root = s.add(None, b"/missing", true, Meta::default());
        let mut scratch = EvalScratch::default();
        for name in names {
            let id = s.add(Some(root), name.as_bytes(), false, Meta::default());
            assert!(q.matches(&s, id, &mut scratch), "{name}");
        }
        for name in ["report.txt", "x", "prefixOR", "Şəkil", "a quote", " "] {
            let id = s.add(Some(root), name.as_bytes(), false, Meta::default());
            assert!(!q.matches(&s, id, &mut scratch), "{name}");
        }
        assert!(!q.reads_files());
        assert!(q.highlight_terms().is_empty());
    }

    #[test]
    fn case_extension_and_contains_options_are_independent() {
        let text = "Report.txt\n.profile\nŞəkil.png\nREADME\narchive.tar.gz";
        let names = [
            "Report.txt",
            "report.TXT",
            "Report.md",
            "MyReport.txt",
            ".profile",
            ".profile.bak",
            "Şəkil.jpg",
            "ŞƏKİL.PNG",
            "Sekil.png",
            "README",
            "README.bak",
            "archive.tar.xz",
        ];
        for case_sensitive in [false, true] {
            for ignore_extension in [false, true] {
                for contains in [false, true] {
                    let options = Options {
                        case_sensitive,
                        ignore_extension,
                        contains,
                    };
                    let q = parsed(text, options);
                    let mut s = NodeStore::new();
                    let root = s.add(None, b"/missing", true, Meta::default());
                    for name in names {
                        let normalize = |name: &str| {
                            let name = if ignore_extension {
                                stem(name.as_bytes())
                            } else {
                                name.as_bytes()
                            };
                            if case_sensitive {
                                name.to_vec()
                            } else {
                                fold(name)
                            }
                        };
                        let subject = normalize(name);
                        let expected = text.lines().any(|line| {
                            let pattern = normalize(line);
                            if contains {
                                subject.windows(pattern.len()).any(|part| part == pattern)
                            } else {
                                subject == pattern
                            }
                        });
                        let id = s.add(Some(root), name.as_bytes(), false, Meta::default());
                        assert_eq!(
                            q.matches(&s, id, &mut EvalScratch::default()),
                            expected,
                            "{options:?}: {name}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn ten_thousand_names_work_in_exact_and_contains_searches() {
        let text = (0..MAX_NAMES)
            .map(|i| format!("file-{i:05}.txt"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut s = NodeStore::new();
        let root = s.add(None, b"/missing", true, Meta::default());
        for i in 0..MAX_NAMES + 3 {
            s.add(
                Some(root),
                format!("file-{i:05}.txt").as_bytes(),
                false,
                Meta::default(),
            );
        }
        let index = Index::new(s);
        for contains in [false, true] {
            let q = parsed(
                &text,
                Options {
                    contains,
                    ..Options::default()
                },
            );
            for threads in [1, 4] {
                assert_eq!(crate::search::search(&index, &q, threads).len(), MAX_NAMES);
                let progress = Progress::new();
                progress.cancel();
                assert_eq!(
                    crate::search::search_with_progress(&index, &q, threads, &progress),
                    Err(Cancelled)
                );
            }
        }
    }

    #[test]
    fn substring_failures_and_suffix_outputs_agree_with_naive_matching() {
        let patterns: HashSet<Vec<u8>> =
            [b"he".as_slice(), b"she", b"hers", b"his", b"aab", b"abac"]
                .into_iter()
                .map(Vec::from)
                .collect();
        let automaton = Automaton::new(patterns.clone());
        for name in [
            b"ushers".as_slice(),
            b"hishers",
            b"aaaaab",
            b"ababc",
            b"abac",
            b"sh",
            b"hhh",
            b"",
        ] {
            let expected = patterns
                .iter()
                .any(|p| name.windows(p.len()).any(|s| s == p));
            assert_eq!(automaton.matches(name), expected, "{name:?}");
        }
        // Every short word over a small alphabet exercises overlapping failures.
        let patterns: HashSet<Vec<u8>> = [b"aab".as_slice(), b"bcc", b"cabac", b"bbca"]
            .into_iter()
            .map(Vec::from)
            .collect();
        let automaton = Automaton::new(patterns.clone());
        for n in 0..3usize.pow(8) {
            let mut n = n;
            let mut name = [0; 8];
            for b in &mut name {
                *b = b'a' + (n % 3) as u8;
                n /= 3;
            }
            let expected = patterns
                .iter()
                .any(|p| name.windows(p.len()).any(|s| s == p));
            assert_eq!(automaton.matches(&name), expected, "{name:?}");
        }
    }

    #[test]
    fn limits_and_malformed_encoded_input_never_become_an_unrestricted_query() {
        for text in ["", "\n\r\n", "a/b", "a\0b", "a\rb"] {
            assert!(query(text, Options::default()).is_err(), "{text:?}");
        }
        assert_eq!(
            query(&"x\n".repeat(MAX_NAMES + 1), Options::default()),
            Err(Error::TooMany)
        );
        assert_eq!(
            query(&"x".repeat(MAX_BYTES + 1), Options::default()),
            Err(Error::TooLarge)
        );
        for text in [
            "names:",
            "names:\"\"",
            "names:\"\\x\"",
            "names:\"\\x0z\"",
            "names:\"\\n\"",
            "names:\"\\xff\"",
            "names:\"\\x00\"",
            "names:\"a/b\"",
        ] {
            let error = Query::parse(text, QueryOptions::default()).unwrap_err();
            assert!(error.0.starts_with("name list:"), "{text}: {error}");
        }
    }

    #[test]
    fn lists_combine_with_modifiers_groups_limits_and_index_filters() {
        let names = query("Report.txt", Options::default()).unwrap();
        let mut s = NodeStore::new();
        let root = s.add(None, b"/missing", true, Meta::default());
        let file = s.add(Some(root), b"Report.txt", false, Meta::default());
        let folder = s.add(Some(root), b"report.txt", true, Meta::default());
        let miss = s.add(Some(root), b"other", false, Meta::default());
        let q = Query::parse(
            &format!("<{names} | wholename:other> !folder: parent:missing limit:2"),
            QueryOptions::default(),
        )
        .unwrap();
        assert_eq!(q.limit(), Some(2));
        assert!(q.matches(&s, file, &mut EvalScratch::default()));
        assert!(!q.matches(&s, folder, &mut EvalScratch::default()));
        assert!(q.matches(&s, miss, &mut EvalScratch::default()));
        let other = parsed("other", Options::default());
        assert!(
            !q.and(other)
                .unwrap()
                .matches(&s, file, &mut EvalScratch::default())
        );
    }
}
