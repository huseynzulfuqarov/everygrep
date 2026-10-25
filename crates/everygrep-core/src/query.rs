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
//! modified:2026-10-25 changed on that local day (also 2026-10, 2026,
//!                     today, yesterday); >, >=, <, <= compare whole days,
//!                     months or years; 2026-10-01..2026-10-25 is inclusive
//! ```
//!
//! Dates use the local time zone (see [`crate::localtime`]); a day starts at
//! local midnight, so days with a daylight saving change are 23 or 25 hours long.
//!
//! Terms without wildcards match anywhere in the name. Matching ignores case
//! unless `case:` or the case-sensitive option is used.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fold::fold;
use crate::localtime::{Zone, civil_from_days, days_from_civil, days_in_month};
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
        Self::parse_with(text, options, now_ns(), None)
    }

    /// Parses `text` at the time `now_ns`. Without a `zone`, the local zone is
    /// loaded once, when the first date term needs it.
    fn parse_with(
        text: &str,
        options: QueryOptions,
        now_ns: i64,
        zone: Option<Zone>,
    ) -> Result<Self, ParseError> {
        let tokens = lex(text)?;
        let mut p = Parser {
            tokens,
            pos: 0,
            options,
            uses_path: false,
            now_ns,
            zone,
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
    find(haystack, needle).is_some()
}

/// Returns the start of the first occurrence of `needle` in `haystack`.
pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some((&first, rest)) = needle.split_first() else {
        return Some(0);
    };
    if haystack.len() < needle.len() {
        return None;
    }
    let last_start = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last_start {
        let at = i + haystack[i..=last_start].iter().position(|&b| b == first)?;
        if haystack[at + 1..at + needle.len()] == *rest {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// A plain text term that a matching name contains, used to highlight the
/// match. Only terms that are not negated, have no wildcards and match the name
/// (not the path) count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameTerm {
    /// The text, folded unless `case` is set.
    pub pattern: Vec<u8>,
    /// Whether the term matches case exactly.
    pub case: bool,
}

impl Query {
    /// The plain text terms that are not negated, in query order.
    pub fn name_terms(&self) -> Vec<NameTerm> {
        let mut out = Vec::new();
        collect_name_terms(&self.root, &mut out);
        out
    }
}

fn collect_name_terms(node: &Node, out: &mut Vec<NameTerm>) {
    match node {
        Node::And(items) | Node::Or(items) => {
            for item in items {
                collect_name_terms(item, out);
            }
        }
        Node::Leaf(Pred::Text(t)) if !t.glob && !t.on_path => out.push(NameTerm {
            pattern: t.pattern.clone(),
            case: t.case,
        }),
        Node::All | Node::Not(_) | Node::Leaf(_) => {}
    }
}

/// Byte range in `name` of the first occurrence of the first term that occurs
/// in it. Ignoring case, offsets in the folded name are used for `name`, so if
/// folding changes the byte length of any character of `name` (such as `İ`)
/// there is no highlight rather than a wrong one.
pub fn match_span(name: &str, terms: &[NameTerm]) -> Option<std::ops::Range<usize>> {
    let mut folded: Option<Option<Vec<u8>>> = None;
    for term in terms {
        let subject: &[u8] = if term.case {
            name.as_bytes()
        } else {
            match folded.get_or_insert_with(|| fold_same_length(name)) {
                Some(f) => f,
                None => continue,
            }
        };
        if let Some(start) = find(subject, &term.pattern) {
            return Some(start..start + term.pattern.len());
        }
    }
    None
}

/// The folded form of `name` if every character keeps its byte length.
fn fold_same_length(name: &str) -> Option<Vec<u8>> {
    if name.is_ascii() {
        return Some(name.as_bytes().to_ascii_lowercase());
    }
    let mut out = Vec::with_capacity(name.len());
    let mut buf = [0u8; 4];
    for c in name.chars() {
        let before = out.len();
        crate::fold::fold_into(c.encode_utf8(&mut buf).as_bytes(), &mut out);
        if out.len() - before != c.len_utf8() {
            return None;
        }
    }
    Some(out)
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
    zone: Option<Zone>,
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
                    return self.parse_modified(&value).map(|p| Some(Node::Leaf(p)));
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

    /// `modified:<7d` (an age) or `modified:2026-10-25` (calendar dates).
    fn parse_modified(&mut self, spec: &str) -> Result<Pred, ParseError> {
        if let Some(pred) = parse_age(spec, self.now_ns) {
            return Ok(pred);
        }
        let now_ns = self.now_ns;
        let zone = self.zone.get_or_insert_with(Zone::local);
        parse_dates(spec, now_ns, zone)
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
/// Returns `None` when `spec` is not an age.
fn parse_age(spec: &str, now_ns: i64) -> Option<Pred> {
    let spec = spec.trim();
    let (newer, rest) = if let Some(r) = spec.strip_prefix("<=").or_else(|| spec.strip_prefix('<'))
    {
        (true, r)
    } else {
        (
            false,
            spec.strip_prefix(">=").or_else(|| spec.strip_prefix('>'))?,
        )
    };
    let age = parse_age_ns(rest)?;
    let edge = now_ns.saturating_sub(age);
    Some(if newer {
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

/// A calendar day, month or year in the local zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Period {
    year: i64,
    month: u32,
    day: u32,
    unit: Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Day,
    Month,
    Year,
}

impl Period {
    /// First day of the following period.
    fn next(self) -> Self {
        let (year, month, day) = match self.unit {
            Unit::Day => civil_from_days(days_from_civil(self.year, self.month, self.day) + 1),
            Unit::Month if self.month == 12 => (self.year + 1, 1, 1),
            Unit::Month => (self.year, self.month + 1, 1),
            Unit::Year => (self.year + 1, 1, 1),
        };
        Self {
            year,
            month,
            day,
            ..self
        }
    }

    /// Local midnight at the start of the period, in nanoseconds.
    fn start_ns(self, zone: &Zone) -> i64 {
        let secs = zone.local_midnight_to_unix(self.year, self.month, self.day);
        secs.saturating_mul(1_000_000_000)
    }

    /// Last nanosecond of the period.
    fn end_ns(self, zone: &Zone) -> i64 {
        self.next().start_ns(zone).saturating_sub(1)
    }
}

/// `2026-10-25`, `2026-10`, `2026`, `today`, `yesterday`, with `>`, `>=`, `<`,
/// `<=` or as an inclusive range `a..b`.
fn parse_dates(spec: &str, now_ns: i64, zone: &Zone) -> Result<Pred, ParseError> {
    let spec = spec.trim();
    let today = zone
        .to_local(now_ns.div_euclid(1_000_000_000))
        .div_euclid(86_400);
    let period = |s: &str| parse_period(s, today, spec);
    let (min, max) = if let Some((a, b)) = spec.split_once("..") {
        (period(a)?.start_ns(zone), period(b)?.end_ns(zone))
    } else if let Some(v) = spec.strip_prefix(">=") {
        (period(v)?.start_ns(zone), i64::MAX)
    } else if let Some(v) = spec.strip_prefix("<=") {
        (i64::MIN, period(v)?.end_ns(zone))
    } else if let Some(v) = spec.strip_prefix('>') {
        (period(v)?.next().start_ns(zone), i64::MAX)
    } else if let Some(v) = spec.strip_prefix('<') {
        // An empty range is (MAX, MIN), which matches nothing.
        match period(v)?.start_ns(zone).checked_sub(1) {
            Some(max) => (i64::MIN, max),
            None => (i64::MAX, i64::MIN),
        }
    } else {
        let p = period(spec.strip_prefix('=').unwrap_or(spec))?;
        (p.start_ns(zone), p.end_ns(zone))
    };
    Ok(Pred::Modified { min, max })
}

/// One date of a `modified:` value; `today` is the current local day number.
fn parse_period(s: &str, today: i64, spec: &str) -> Result<Period, ParseError> {
    let s = s.trim().to_ascii_lowercase();
    let day = |days: i64| {
        let (year, month, day) = civil_from_days(days);
        Period {
            year,
            month,
            day,
            unit: Unit::Day,
        }
    };
    match s.as_str() {
        "today" => return Ok(day(today)),
        "yesterday" => return Ok(day(today - 1)),
        _ => {}
    }
    let looks_like_date = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-');
    if !looks_like_date {
        return Err(ParseError(format!(
            "bad time: {spec} (use for example <7d, >2h, 2026-10-25, 2026-10, today)"
        )));
    }
    let bad = || ParseError(format!("bad date: {s} (use YYYY, YYYY-MM or YYYY-MM-DD)"));
    let parts: Vec<&str> = s.split('-').collect();
    let num = |i: usize, len: std::ops::RangeInclusive<usize>| -> Result<u32, ParseError> {
        let p = parts[i];
        if !len.contains(&p.len()) {
            return Err(bad());
        }
        p.parse().map_err(|_| bad())
    };
    let year = i64::from(num(0, 4..=4)?);
    let (month, day, unit) = match parts.len() {
        1 => (1, 1, Unit::Year),
        2 => (num(1, 1..=2)?, 1, Unit::Month),
        3 => (num(1, 1..=2)?, num(2, 1..=2)?, Unit::Day),
        _ => return Err(bad()),
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(bad());
    }
    Ok(Period {
        year,
        month,
        day,
        unit,
    })
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
        assert!(Query::parse("modified:soon", QueryOptions::default()).is_err());
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

    /// Nanoseconds of a local time with a known UTC offset in hours.
    fn at(y: i64, m: u32, d: u32, h: i64, min: i64, offset_h: i64) -> i64 {
        let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60 - offset_h * 3600;
        secs * 1_000_000_000
    }

    /// A Berlin-like zone and files around its 2026 daylight saving changes;
    /// "now" is 2026-10-25 12:00 local, after the autumn change.
    fn dated() -> (Tree, i64, Zone) {
        let zone = Zone::from_posix_tz("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        let mut store = NodeStore::new();
        let root = store.add(None, b"/d", true, Meta::default());
        for (name, mtime_ns) in [
            ("a-dst-day", at(2026, 3, 29, 23, 30, 2)),
            ("b-after-dst", at(2026, 3, 30, 0, 10, 2)),
            ("c-oct-first", at(2026, 10, 1, 0, 0, 2)),
            ("d-sep-last", at(2026, 10, 1, 0, 0, 2) - 1),
            ("e-today", at(2026, 10, 25, 9, 0, 1)),
            ("f-yesterday", at(2026, 10, 24, 23, 59, 2)),
            ("g-2025", at(2025, 12, 31, 23, 59, 1)),
            ("h-1960", at(1960, 1, 15, 12, 0, 1)),
        ] {
            store.add(
                Some(root),
                name.as_bytes(),
                false,
                Meta { size: 1, mtime_ns },
            );
        }
        (Tree { store }, at(2026, 10, 25, 12, 0, 1), zone)
    }

    fn find_dated(q: &str) -> Vec<String> {
        let (t, now, zone) = dated();
        let query = Query::parse_with(q, QueryOptions::default(), now, Some(zone)).unwrap();
        let mut scratch = EvalScratch::default();
        let mut out: Vec<String> = (1..t.store.len())
            .map(|i| NodeId(i as u32))
            .filter(|&id| query.matches(&t.store, id, &mut scratch))
            .map(|id| String::from_utf8_lossy(t.store.name(id)).into_owned())
            .collect();
        out.sort();
        out
    }

    fn dated_error(q: &str) -> bool {
        let (_, now, zone) = dated();
        Query::parse_with(q, QueryOptions::default(), now, Some(zone)).is_err()
    }

    #[test]
    fn absolute_days_months_and_years() {
        assert_eq!(find_dated("modified:2026-03-29"), ["a-dst-day"]);
        assert_eq!(find_dated("modified:2026-3-30"), ["b-after-dst"]);
        assert_eq!(find_dated("modified:2026-03"), ["a-dst-day", "b-after-dst"]);
        assert_eq!(
            find_dated("modified:2026-10"),
            ["c-oct-first", "e-today", "f-yesterday"]
        );
        assert_eq!(find_dated("modified:2026-09-30"), ["d-sep-last"]);
        assert_eq!(find_dated("modified:=2025"), ["g-2025"]);
        assert_eq!(find_dated("modified:1960-01-15"), ["h-1960"]);
        assert_eq!(find_dated("modified:2026").len(), 6);
    }

    #[test]
    fn date_comparisons_use_whole_units() {
        assert_eq!(find_dated("modified:>2026-10-24"), ["e-today"]);
        assert_eq!(
            find_dated("modified:>=2026-10-24"),
            ["e-today", "f-yesterday"]
        );
        let before_october = ["a-dst-day", "b-after-dst", "d-sep-last", "g-2025", "h-1960"];
        assert_eq!(find_dated("modified:<2026-10-01"), before_october);
        assert_eq!(find_dated("modified:<=2026-09-30"), before_october);
        assert_eq!(find_dated("modified:<=2026-09"), before_october);
        assert_eq!(find_dated("modified:>2025"), find_dated("modified:2026"));
        assert_eq!(find_dated("modified:<1970"), ["h-1960"]);
        assert_eq!(
            find_dated("modified:2026-09-30..2026-10-01"),
            ["c-oct-first", "d-sep-last"]
        );
        assert_eq!(
            find_dated("modified:2025..2026-03"),
            ["a-dst-day", "b-after-dst", "g-2025"]
        );
        assert!(find_dated("modified:2026-10-02..2026-10-01").is_empty());
    }

    #[test]
    fn today_yesterday_and_relative_ages() {
        assert_eq!(find_dated("modified:today"), ["e-today"]);
        // The real local zone loads without a fixed one.
        assert!(Query::parse("modified:today", QueryOptions::default()).is_ok());
        assert_eq!(find_dated("modified:Yesterday"), ["f-yesterday"]);
        assert_eq!(
            find_dated("modified:>=yesterday"),
            ["e-today", "f-yesterday"]
        );
        assert_eq!(find_dated("modified:<1d"), ["e-today", "f-yesterday"]);
        assert_eq!(
            find_dated("modified:>30d"),
            ["a-dst-day", "b-after-dst", "g-2025", "h-1960"]
        );
    }

    #[test]
    fn invalid_dates_are_errors() {
        for q in [
            "modified:2026-13-01",
            "modified:2026-02-30",
            "modified:2026-02-29",
            "modified:2026-00",
            "modified:26-10-01",
            "modified:2026-10-01-02",
            "modified:2026-10-001",
            "modified:2026-10-01..",
            "modified:..2026",
            "modified:>",
            "modified:tomorrow",
            "modified:7d..3d",
        ] {
            assert!(dated_error(q), "{q}");
        }
        assert!(!dated_error("modified:2024-02-29"));
    }

    #[test]
    fn glob_handles_multibyte_characters() {
        assert!(glob_match("ş?kil".as_bytes(), "şəkil".as_bytes()));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"a*b*c", b"axxbyyc"));
        assert!(!glob_match(b"a*b", b"ac"));
    }

    fn span(name: &str, q: &str) -> Option<(usize, usize)> {
        let query = Query::parse(q, QueryOptions::default()).unwrap();
        match_span(name, &query.name_terms()).map(|r| (r.start, r.end))
    }

    #[test]
    fn name_terms_skip_negated_glob_and_path_terms() {
        let q = Query::parse(
            "Rep !draft *.pdf path:src a/b case:Main <x|y> ext:pdf",
            QueryOptions::default(),
        )
        .unwrap();
        let terms: Vec<(String, bool)> = q
            .name_terms()
            .into_iter()
            .map(|t| (String::from_utf8(t.pattern).unwrap(), t.case))
            .collect();
        assert_eq!(
            terms,
            [
                ("rep".into(), false),
                ("Main".into(), true),
                ("x".into(), false),
                ("y".into(), false)
            ]
        );
    }

    #[test]
    fn match_span_finds_the_first_matching_term() {
        assert_eq!(span("Report-2026.PDF", "pdf"), Some((12, 15)));
        assert_eq!(span("Report-2026.PDF", "zzz rep"), Some((0, 3)));
        assert_eq!(span("a-b-a", "a"), Some((0, 1)));
        assert_eq!(span("readme", "case:READ"), None);
        assert_eq!(span("README", "case:READ"), Some((0, 4)));
        assert_eq!(span("draft", "!draft"), None);
        assert_eq!(span("x.pdf", "*.pdf"), None);
        assert_eq!(span("x.pdf", ""), None);
    }

    #[test]
    fn match_span_handles_multibyte_names() {
        // "Ş" and "Ə" fold to "ş" and "ə" with the same byte length; "İ"
        // anywhere in the name turns highlighting off for the whole name.
        assert_eq!(span("ŞƏKİL.png", "png"), None);
        assert_eq!(span("ŞƏKIL.png", "əkil"), Some((2, 7)));
        // "İ" folds to three bytes: no highlight instead of shifted offsets.
        assert_eq!(span("İzmir", "zmir"), None);
        // A case-sensitive term does not need folding.
        assert_eq!(span("İzmir", "case:zmir"), Some((2, 6)));
    }
}
