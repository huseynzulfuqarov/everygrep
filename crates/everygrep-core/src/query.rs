//! Query language.
//!
//! ```text
//! report pdf          both terms (space = AND)
//! jpg|png             either term (OR binds tighter than AND)
//! !draft              not
//! <a b>|c             grouping
//! "two words"         quotes make spaces and operators literal
//! *.pdf  img_??.png   wildcards match the whole name
//! case:Readme         match case for this term
//! path:src/main       match against the full path instead of the name
//! file:  folder:      only files / only folders
//! ext:pdf;jpg         extension list (files only)
//! size:>10mb  size:1kb..2mb  size:0
//! modified:<7d        changed within the last 7 days (also h, m, s);
//!                     modified:>30d = not changed for 30 days
//! ```
//!
//! Terms without wildcards match anywhere in the name. Matching ignores case
//! unless `case:` or the case-sensitive option is used.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fold::fold;
use crate::store::{NodeId, NodeStore};

/// Options that change how a query matches names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions {
    /// Compare text exactly instead of case-insensitively.
    pub case_sensitive: bool,
}

/// Why a query could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// A parsed query.
#[derive(Clone, Debug)]
pub struct Query {
    root: Node,
    uses_path: bool,
}

#[derive(Clone, Debug)]
enum Node {
    All,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Pred),
}

#[derive(Clone, Debug)]
enum Pred {
    Text(TextPred),
    IsFile,
    IsDir,
    /// Folded extensions without the leading dot (may contain dots, such as
    /// `tar.gz`); `none` also accepts files that have no extension.
    Ext {
        exts: Vec<Vec<u8>>,
        none: bool,
    },
    Size {
        min: u64,
        max: u64,
    },
    /// Inclusive range of modification times in nanoseconds.
    Modified {
        min: i64,
        max: i64,
    },
}

#[derive(Clone, Debug)]
struct TextPred {
    pattern: Vec<u8>,
    case: bool,
    on_path: bool,
    glob: bool,
}

/// Per-node scratch space for predicates that need the full path.
#[derive(Default)]
pub struct EvalScratch {
    path: Vec<u8>,
    folded_path: Vec<u8>,
    for_node: Option<NodeId>,
    folded_for_node: Option<NodeId>,
}

impl Query {
    /// Parses `text`. An empty query matches every node.
    pub fn parse(text: &str, options: QueryOptions) -> Result<Self, ParseError> {
        let tokens = lex(text)?;
        let mut p = Parser {
            tokens,
            pos: 0,
            options,
            uses_path: false,
            now_ns: now_ns(),
        };
        let root = p.parse_and(false)?.unwrap_or(Node::All);
        if p.pos < p.tokens.len() {
            return Err(ParseError("unexpected '>'".into()));
        }
        Ok(Self {
            root,
            uses_path: p.uses_path,
        })
    }

    /// Whether the query accepts every node.
    pub fn matches_all(&self) -> bool {
        matches!(self.root, Node::All)
    }

    /// Tests one node.
    pub fn matches(&self, store: &NodeStore, id: NodeId, scratch: &mut EvalScratch) -> bool {
        if self.uses_path {
            scratch.for_node = None;
            scratch.folded_for_node = None;
        }
        eval(&self.root, store, id, scratch)
    }
}

fn eval(node: &Node, store: &NodeStore, id: NodeId, scratch: &mut EvalScratch) -> bool {
    match node {
        Node::All => true,
        Node::And(items) => items.iter().all(|n| eval(n, store, id, scratch)),
        Node::Or(items) => items.iter().any(|n| eval(n, store, id, scratch)),
        Node::Not(inner) => !eval(inner, store, id, scratch),
        Node::Leaf(pred) => eval_pred(pred, store, id, scratch),
    }
}

fn eval_pred(pred: &Pred, store: &NodeStore, id: NodeId, scratch: &mut EvalScratch) -> bool {
    match pred {
        Pred::IsFile => !store.is_dir(id),
        Pred::IsDir => store.is_dir(id),
        Pred::Ext { exts, none } => {
            if store.is_dir(id) {
                return false;
            }
            let name = store.folded_name(id);
            // A dot at the very start (".bashrc") does not begin an extension.
            let has_ext = name
                .iter()
                .rposition(|&b| b == b'.')
                .is_some_and(|dot| dot > 0);
            (*none && !has_ext)
                || exts.iter().any(|e| {
                    name.len() > e.len() + 1
                        && name.ends_with(e)
                        && name[name.len() - e.len() - 1] == b'.'
                })
        }
        Pred::Size { min, max } => {
            let size = store.meta(id).size;
            !store.is_dir(id) && *min <= size && size <= *max
        }
        Pred::Modified { min, max } => {
            let t = store.meta(id).mtime_ns;
            *min <= t && t <= *max
        }
        Pred::Text(t) => {
            let subject: &[u8] = if t.on_path {
                if t.case {
                    path_bytes(store, id, scratch)
                } else {
                    folded_path_bytes(store, id, scratch)
                }
            } else if t.case {
                store.name(id)
            } else {
                store.folded_name(id)
            };
            if t.glob {
                glob_match(&t.pattern, subject)
            } else {
                contains(subject, &t.pattern)
            }
        }
    }
}

fn path_bytes<'a>(store: &NodeStore, id: NodeId, scratch: &'a mut EvalScratch) -> &'a [u8] {
    if scratch.for_node != Some(id) {
        use std::os::unix::ffi::OsStrExt;
        scratch.path.clear();
        scratch
            .path
            .extend_from_slice(store.path(id).as_os_str().as_bytes());
        scratch.for_node = Some(id);
    }
    &scratch.path
}

fn folded_path_bytes<'a>(store: &NodeStore, id: NodeId, scratch: &'a mut EvalScratch) -> &'a [u8] {
    if scratch.folded_for_node != Some(id) {
        path_bytes(store, id, scratch);
        scratch.folded_path.clear();
        crate::fold::fold_into(&scratch.path, &mut scratch.folded_path);
        scratch.folded_for_node = Some(id);
    }
    &scratch.folded_path
}

/// Returns whether `needle` occurs in `haystack`.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    if haystack.len() < needle.len() {
        return false;
    }
    let last_start = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last_start {
        match haystack[i..=last_start].iter().position(|&b| b == first) {
            None => return false,
            Some(off) => {
                let at = i + off;
                if haystack[at + 1..at + needle.len()] == *rest {
                    return true;
                }
                i = at + 1;
            }
        }
    }
    false
}

/// Matches a whole subject against a pattern where `*` is any run of characters
/// and `?` is exactly one character (one UTF-8 sequence).
pub fn glob_match(pattern: &[u8], subject: &[u8]) -> bool {
    let (mut p, mut s) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while s < subject.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, s));
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                s += char_len(subject[s]);
            }
            Some(&c) if c == subject[s] => {
                p += 1;
                s += 1;
            }
            _ => match star {
                Some((sp, ss)) => {
                    let next = ss + char_len(subject[ss]);
                    star = Some((sp, next));
                    p = sp + 1;
                    s = next;
                }
                None => return false,
            },
        }
    }
    pattern[p.min(pattern.len())..].iter().all(|&c| c == b'*')
}

/// Length of the UTF-8 sequence that starts with `b` (1 for invalid bytes).
fn char_len(b: u8) -> usize {
    match b {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Or,
    Not,
    Open,
    Close,
}

fn lex(text: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let flush = |word: &mut String, tokens: &mut Vec<Token>| {
        if !word.is_empty() {
            tokens.push(Token::Word(std::mem::take(word)));
        }
    };
    for c in text.chars() {
        if c == '"' {
            quoted = !quoted;
            // Keep a marker so `""` still produces a (literal, empty) word.
            word.push('\u{0}');
            continue;
        }
        if quoted {
            word.push(c);
            continue;
        }
        match c {
            c if c.is_whitespace() => flush(&mut word, &mut tokens),
            '|' => {
                flush(&mut word, &mut tokens);
                tokens.push(Token::Or);
            }
            '!' if word.is_empty() => tokens.push(Token::Not),
            '<' if !word_takes_comparison(&word) => {
                flush(&mut word, &mut tokens);
                tokens.push(Token::Open);
            }
            '>' if !word_takes_comparison(&word) => {
                flush(&mut word, &mut tokens);
                tokens.push(Token::Close);
            }
            c => word.push(c),
        }
    }
    if quoted {
        return Err(ParseError("missing closing quote".into()));
    }
    flush(&mut word, &mut tokens);
    Ok(tokens)
}

/// `size:>1mb` and `modified:>7d` use `<`/`>` as comparisons, not grouping.
fn word_takes_comparison(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    ["size:", "modified:"]
        .iter()
        .any(|m| lower.ends_with(m) || lower.ends_with(&format!("{m}=")))
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    options: QueryOptions,
    uses_path: bool,
    now_ns: i64,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    /// Sequence of OR-groups (implicit AND). Stops at `>` when inside a group.
    /// Returns `None` when there is nothing to match on (for example `<>`).
    fn parse_and(&mut self, in_group: bool) -> Result<Option<Node>, ParseError> {
        let mut items = Vec::new();
        while let Some(t) = self.peek() {
            match t {
                Token::Close if in_group => break,
                Token::Close => return Err(ParseError("unexpected '>'".into())),
                Token::Or => {
                    // A leading or doubled `|` is ignored.
                    self.pos += 1;
                }
                _ => items.extend(self.parse_or(in_group)?),
            }
        }
        // AND is order independent: test cheap conditions first so expensive
        // full-path matching runs only for nodes that passed them.
        items.sort_by_key(cost);
        Ok(match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Node::And(items)),
        })
    }

    /// Terms joined by `|`. Empty alternatives (`a||b`, `a|<>`, `a|!`) are
    /// dropped instead of matching all nodes.
    fn parse_or(&mut self, in_group: bool) -> Result<Option<Node>, ParseError> {
        let mut items: Vec<Node> = self.parse_unary()?.into_iter().collect();
        while self.peek() == Some(&Token::Or) {
            while self.peek() == Some(&Token::Or) {
                self.pos += 1;
            }
            match self.peek() {
                None => break,
                Some(Token::Close) if in_group => break,
                _ => items.extend(self.parse_unary()?),
            }
        }
        Ok(match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Node::Or(items)),
        })
    }

    /// One term, `!term` or `<group>`. Returns `None` for an empty term; tokens
    /// that cannot start a term (`|`, end of input) are not consumed.
    fn parse_unary(&mut self) -> Result<Option<Node>, ParseError> {
        match self.tokens.get(self.pos).cloned() {
            Some(Token::Not) => {
                self.pos += 1;
                Ok(self.parse_unary()?.map(|n| Node::Not(Box::new(n))))
            }
            Some(Token::Open) => {
                self.pos += 1;
                let inner = self.parse_and(true)?;
                match self.peek() {
                    Some(Token::Close) => self.pos += 1,
                    _ => return Err(ParseError("missing '>'".into())),
                }
                Ok(inner)
            }
            Some(Token::Word(w)) => {
                self.pos += 1;
                self.parse_word(&w)
            }
            Some(Token::Close) => Err(ParseError("unexpected '>'".into())),
            Some(Token::Or) | None => Ok(None),
        }
    }

    fn parse_word(&mut self, raw: &str) -> Result<Option<Node>, ParseError> {
        let mut case = self.options.case_sensitive;
        let mut on_path = false;
        let mut rest = raw;
        // Modifiers can be stacked: case:path:Src/Main
        while let Some((head, tail)) = split_modifier(rest) {
            // Quotes inside a value (`ext:"pdf"`) only group; drop their markers.
            let value: String = tail.chars().filter(|&c| c != '\u{0}').collect();
            match head.as_str() {
                "case" => case = true,
                "nocase" => case = false,
                "path" => on_path = true,
                "file" | "files" => return Ok(Some(self.with_rest(Pred::IsFile, tail)?)),
                "folder" | "folders" | "dir" => {
                    return Ok(Some(self.with_rest(Pred::IsDir, tail)?));
                }
                "ext" => return Ok(Some(Node::Leaf(parse_ext(&value)))),
                "size" => return parse_size(&value).map(|p| Some(Node::Leaf(p))),
                "modified" => {
                    return parse_modified(&value, self.now_ns).map(|p| Some(Node::Leaf(p)));
                }
                _ => break,
            }
            rest = tail;
        }
        let quoted = rest.contains('\u{0}');
        let text: String = rest.chars().filter(|&c| c != '\u{0}').collect();
        if text.is_empty() {
            return Ok(None);
        }
        let glob = !quoted && (text.contains('*') || text.contains('?'));
        let on_path = on_path || text.contains('/');
        self.uses_path |= on_path;
        let pattern = if case {
            text.into_bytes()
        } else {
            fold(text.as_bytes())
        };
        Ok(Some(Node::Leaf(Pred::Text(TextPred {
            pattern,
            case,
            on_path,
            glob,
        }))))
    }

    /// `file:foo` means "a file whose name contains foo".
    fn with_rest(&mut self, kind: Pred, rest: &str) -> Result<Node, ParseError> {
        Ok(match self.parse_word(rest)? {
            Some(term) => Node::And(vec![Node::Leaf(kind), term]),
            None => Node::Leaf(kind),
        })
    }
}

/// Rough evaluation cost of a node, used to order AND terms.
fn cost(node: &Node) -> u8 {
    match node {
        Node::All => 0,
        Node::Leaf(Pred::Text(t)) if t.on_path => 3,
        Node::Leaf(Pred::Text(_)) => 1,
        Node::Leaf(_) => 0,
        Node::Not(inner) => cost(inner),
        Node::And(items) | Node::Or(items) => items.iter().map(cost).max().unwrap_or(0),
    }
}

/// Splits `name:rest` when `name` is a known modifier (case-insensitive).
fn split_modifier(word: &str) -> Option<(String, &str)> {
    let (head, tail) = word.split_once(':')?;
    let head = head.to_ascii_lowercase();
    const KNOWN: &[&str] = &[
        "case", "nocase", "path", "file", "files", "folder", "folders", "dir", "ext", "size",
        "modified",
    ];
    KNOWN.contains(&head.as_str()).then_some((head, tail))
}

/// `ext:pdf;jpg`. Empty entries (as in a trailing `;`) are ignored; `ext:` on its
/// own matches files without an extension.
fn parse_ext(list: &str) -> Pred {
    let exts: Vec<Vec<u8>> = list
        .split(';')
        .map(|e| e.trim().trim_start_matches('.'))
        .filter(|e| !e.is_empty())
        .map(|e| fold(e.as_bytes()))
        .collect();
    let none = exts.is_empty();
    Pred::Ext { exts, none }
}

fn parse_size(spec: &str) -> Result<Pred, ParseError> {
    let bad = || ParseError(format!("bad size: {spec}"));
    let (min, max) = parse_range(spec, |s| parse_bytes(s).ok_or_else(bad))?;
    Ok(Pred::Size { min, max })
}

fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let digits_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let value: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = value * mult;
    (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes.round() as u64)
}

/// `>x`, `>=x`, `<x`, `<=x`, `=x`, `x`, `x..y` into an inclusive range.
fn parse_range<T: Copy + Bounded>(
    spec: &str,
    value: impl Fn(&str) -> Result<T, ParseError>,
) -> Result<(T, T), ParseError> {
    let spec = spec.trim();
    if let Some((a, b)) = spec.split_once("..") {
        return Ok((value(a)?, value(b)?));
    }
    if let Some(v) = spec.strip_prefix(">=") {
        return Ok((value(v)?, T::MAX));
    }
    if let Some(v) = spec.strip_prefix("<=") {
        return Ok((T::MIN, value(v)?));
    }
    // An empty range is returned as (MAX, MIN), which matches nothing.
    if let Some(v) = spec.strip_prefix('>') {
        return Ok(value(v)?.succ().map_or((T::MAX, T::MIN), |lo| (lo, T::MAX)));
    }
    if let Some(v) = spec.strip_prefix('<') {
        return Ok(value(v)?.pred().map_or((T::MAX, T::MIN), |hi| (T::MIN, hi)));
    }
    let v = value(spec.strip_prefix('=').unwrap_or(spec))?;
    Ok((v, v))
}

trait Bounded: Sized {
    const MIN: Self;
    const MAX: Self;
    fn succ(self) -> Option<Self>;
    fn pred(self) -> Option<Self>;
}

impl Bounded for u64 {
    const MIN: Self = 0;
    const MAX: Self = u64::MAX;
    fn succ(self) -> Option<Self> {
        self.checked_add(1)
    }
    fn pred(self) -> Option<Self> {
        self.checked_sub(1)
    }
}

/// `modified:<7d` means newer than 7 days ago; `modified:>7d` means older.
fn parse_modified(spec: &str, now_ns: i64) -> Result<Pred, ParseError> {
    let bad = || ParseError(format!("bad time: {spec} (use for example <7d, >2h, <30m)"));
    let spec = spec.trim();
    let (newer, rest) = if let Some(r) = spec.strip_prefix("<=").or_else(|| spec.strip_prefix('<'))
    {
        (true, r)
    } else if let Some(r) = spec.strip_prefix(">=").or_else(|| spec.strip_prefix('>')) {
        (false, r)
    } else {
        return Err(bad());
    };
    let age = parse_age_ns(rest).ok_or_else(bad)?;
    let edge = now_ns.saturating_sub(age);
    Ok(if newer {
        Pred::Modified {
            min: edge,
            max: i64::MAX,
        }
    } else {
        Pred::Modified {
            min: i64::MIN,
            max: edge,
        }
    })
}

fn parse_age_ns(s: &str) -> Option<i64> {
    let s = s.trim().to_ascii_lowercase();
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    };
    n.checked_mul(secs)?.checked_mul(1_000_000_000)
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Meta;

    struct Tree {
        store: NodeStore,
    }

    impl Tree {
        fn new() -> Self {
            let mut store = NodeStore::new();
            let now = now_ns();
            let day = 86_400 * 1_000_000_000i64;
            let root = store.add(None, b"/home", true, Meta::default());
            let docs = store.add(Some(root), b"Docs", true, Meta::default());
            let src = store.add(Some(root), b"src", true, Meta::default());
            store.add(
                Some(docs),
                b"Report-2026.PDF",
                false,
                Meta {
                    size: 3 << 20,
                    mtime_ns: now - day,
                },
            );
            store.add(
                Some(docs),
                b"draft report.pdf",
                false,
                Meta {
                    size: 10,
                    mtime_ns: now - 40 * day,
                },
            );
            store.add(
                Some(docs),
                b"photo.jpg",
                false,
                Meta {
                    size: 500 << 10,
                    mtime_ns: now,
                },
            );
            store.add(
                Some(docs),
                b"photo (1).png",
                false,
                Meta {
                    size: 0,
                    mtime_ns: now,
                },
            );
            store.add(
                Some(src),
                b"main.rs",
                false,
                Meta {
                    size: 2048,
                    mtime_ns: now,
                },
            );
            store.add(Some(src), b"README", false, Meta::default());
            store.add(Some(src), b"report", true, Meta::default());
            Self { store }
        }

        fn find(&self, q: &str) -> Vec<String> {
            self.find_with(q, QueryOptions::default())
        }

        fn find_with(&self, q: &str, options: QueryOptions) -> Vec<String> {
            let query = Query::parse(q, options).unwrap();
            let mut scratch = EvalScratch::default();
            let mut out: Vec<String> = (0..self.store.len())
                .map(|i| NodeId(i as u32))
                .filter(|&id| query.matches(&self.store, id, &mut scratch))
                .map(|id| String::from_utf8_lossy(self.store.name(id)).into_owned())
                .collect();
            out.sort();
            out
        }
    }

    #[test]
    fn and_or_not_and_groups() {
        let t = Tree::new();
        assert_eq!(
            t.find("report pdf"),
            ["Report-2026.PDF", "draft report.pdf"]
        );
        assert_eq!(t.find("jpg|png"), ["photo (1).png", "photo.jpg"]);
        assert_eq!(t.find("photo jpg|png"), ["photo (1).png", "photo.jpg"]);
        assert_eq!(t.find("report !draft !folder:"), ["Report-2026.PDF"]);
        assert_eq!(t.find("<draft|main> !rs"), ["draft report.pdf"]);
    }

    #[test]
    fn quotes_keep_spaces_and_operators_literal() {
        let t = Tree::new();
        assert_eq!(t.find("\"draft report\""), ["draft report.pdf"]);
        assert_eq!(t.find("\"(1)\""), ["photo (1).png"]);
        assert_eq!(t.find("(1)"), ["photo (1).png"]);
    }

    #[test]
    fn wildcards_match_whole_name() {
        let t = Tree::new();
        assert_eq!(t.find("*.pdf"), ["Report-2026.PDF", "draft report.pdf"]);
        assert_eq!(t.find("main.??"), ["main.rs"]);
        assert!(t.find("mai*").contains(&"main.rs".to_string()));
        assert!(t.find("ain*").is_empty());
    }

    #[test]
    fn case_and_path_modifiers() {
        let t = Tree::new();
        assert_eq!(t.find("case:README"), ["README"]);
        assert!(t.find("case:readme").is_empty());
        assert_eq!(
            t.find_with(
                "README",
                QueryOptions {
                    case_sensitive: true
                }
            ),
            ["README"]
        );
        assert_eq!(t.find("path:src rs"), ["main.rs"]);
        assert_eq!(t.find("docs/photo"), ["photo (1).png", "photo.jpg"]);
    }

    #[test]
    fn type_ext_size_and_time_filters() {
        let t = Tree::new();
        assert_eq!(t.find("report folder:"), ["report"]);
        assert_eq!(
            t.find("report file:"),
            ["Report-2026.PDF", "draft report.pdf"]
        );
        assert_eq!(
            t.find("ext:pdf;JPG"),
            ["Report-2026.PDF", "draft report.pdf", "photo.jpg"]
        );
        assert_eq!(t.find("size:>1mb"), ["Report-2026.PDF"]);
        assert_eq!(t.find("size:1kb..1mb"), ["main.rs", "photo.jpg"]);
        assert_eq!(
            t.find("size:<1kb file: !photo"),
            ["README", "draft report.pdf"]
        );
        assert_eq!(t.find("photo size:0"), ["photo (1).png"]);
        assert_eq!(t.find("pdf modified:<7d"), ["Report-2026.PDF"]);
        assert_eq!(t.find("pdf modified:>30d"), ["draft report.pdf"]);
    }

    #[test]
    fn empty_and_malformed_queries() {
        let t = Tree::new();
        assert!(
            Query::parse("  ", QueryOptions::default())
                .unwrap()
                .matches_all()
        );
        assert_eq!(t.find("!").len(), t.store.len());
        assert!(Query::parse("\"open", QueryOptions::default()).is_err());
        assert!(Query::parse("<a b", QueryOptions::default()).is_err());
        assert!(Query::parse("a>", QueryOptions::default()).is_err());
        assert!(Query::parse("size:>lots", QueryOptions::default()).is_err());
        assert!(Query::parse("modified:yesterday", QueryOptions::default()).is_err());
    }

    #[test]
    fn empty_alternatives_do_not_match_all_nodes() {
        let t = Tree::new();
        assert_eq!(t.find("pdf||jpg"), t.find("pdf|jpg"));
        assert_eq!(t.find("pdf | !"), t.find("pdf"));
        assert_eq!(t.find("pdf|<>"), t.find("pdf"));
        assert_eq!(t.find("<> pdf"), t.find("pdf"));
        assert_eq!(t.find("pdf|\"\""), t.find("pdf"));
    }

    #[test]
    fn modifier_aliases_and_values() {
        let t = Tree::new();
        assert_eq!(t.find("pdf modified:<7d"), ["Report-2026.PDF"]);
        assert_eq!(
            t.find("file:report"),
            ["Report-2026.PDF", "draft report.pdf"]
        );
        assert_eq!(t.find("folder:report"), ["report"]);
        assert!(t.find("size:<0").is_empty());
        assert_eq!(t.find("ext:pdf;"), t.find("ext:pdf"));
        assert_eq!(t.find("ext:\"pdf\""), t.find("ext:pdf"));
    }

    #[test]
    fn extensions_with_dots_and_dot_files() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/r", true, Meta::default());
        for name in [
            &b"archive.tar.gz"[..],
            b".bashrc",
            b"notes",
            b"x.gz",
            b".tar.gz",
        ] {
            store.add(Some(root), name, false, Meta::default());
        }
        let t = Tree { store };
        assert_eq!(t.find("ext:tar.gz"), ["archive.tar.gz"]);
        assert_eq!(t.find("ext:gz"), [".tar.gz", "archive.tar.gz", "x.gz"]);
        assert_eq!(t.find("ext:"), [".bashrc", "notes"]);
        assert!(t.find("ext:bashrc").is_empty());
    }

    #[test]
    fn glob_handles_multibyte_characters() {
        assert!(glob_match("ş?kil".as_bytes(), "şəkil".as_bytes()));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"a*b*c", b"axxbyyc"));
        assert!(!glob_match(b"a*b", b"ac"));
    }
}
