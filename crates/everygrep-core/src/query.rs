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
//! wholeword:art              whole words only: "art" but not "party"
//! diacritics:café     match diacritics: "café" but not "cafe"
//! path:src/main       match against the full path instead of the name
//! file:  folder:      only files / only folders
//! ext:pdf;jpg         extension list (files only); also ext:pdf|jpg, ext:jp*
//! size:>10mb  size:1kb..2mb  size:0
//!                     folders match by the total size of the files below them
//! modified:<7d        changed within the last 7 days (also h, m, s);
//!                     modified:>30d = not changed for 30 days
//! modified:2026-10-25 changed on that local day (also 2026-10, 2026,
//!                     today, yesterday); >, >=, <, <= compare whole days,
//!                     months or years; 2026-10-01..2026-10-25 is inclusive
//! prefix:rep          the name starts with the term
//! suffix:.tar.gz      the name ends with the term
//! nopunct:foo-bar     punctuation is ignored: "foo_bar", "foobar"
//! nospace:"my file"   whitespace is ignored: "myfile", "my  file"
//! regex:^img_\d+$     a regular expression (to the next space)
//! ```
//!
//! Dates use the local time zone (see [`crate::localtime`]); a day starts at
//! local midnight, so days with a daylight saving change are 23 or 25 hours long.
//!
//! Terms without wildcards match anywhere in the name. Matching ignores case
//! and diacritics (`é` = `e`, `ş` = `s`, `ə` = `e`; see
//! [`crate::fold::strip_diacritics_into`]) unless `case:`, `diacritics:` or the
//! matching options are used. `nocase:`, `nowholeword:`, `nodiacritics:` and `nopath:`
//! turn an option off for one term, as do `noprefix:`, `nosuffix:`, `punct:`,
//! `space:` and `noregex:`.
//!
//! A regular expression ([`regex`] syntax) is matched against the raw name, or
//! the raw full path in path mode, ignoring case unless case is matched.
//! Diacritics, whole words, prefix, suffix and the ignore modes do not apply
//! to it. With [`QueryOptions::regex`] the whole query is one expression.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::filter::Filter;
use crate::fold::{fold, strip_diacritics_into};
use crate::kind::classify;
use crate::localtime::{Zone, civil_from_days, days_from_civil, days_in_month};
use crate::store::{NodeId, NodeStore};

/// Options that change how a query matches names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions {
    /// Compare text exactly instead of case-insensitively.
    pub case_sensitive: bool,
    /// Text terms match whole words only (see [`find_word`]).
    pub whole_words: bool,
    /// Text terms match the full path instead of the name.
    pub match_path: bool,
    /// Letters with diacritics differ from their base letters. Off by default:
    /// `cafe` finds `café`.
    pub match_diacritics: bool,
    /// Text terms must match at the start of the name (or path). Wildcard
    /// patterns then need not reach the end of the name.
    pub prefix: bool,
    /// Text terms must match at the end of the name (or path). Wildcard
    /// patterns then need not start at the beginning of the name.
    pub suffix: bool,
    /// Punctuation in names and terms is left out before comparing, so
    /// `foo-bar` finds `foo_bar.txt` and `foobar`.
    pub ignore_punctuation: bool,
    /// Whitespace in names and terms is left out before comparing, so
    /// `myfile` finds `my file.txt`.
    pub ignore_whitespace: bool,
    /// The whole query is one regular expression; operators and modifiers
    /// are not read.
    pub regex: bool,
    /// Only entries of this kind match, in addition to the query text.
    pub filter: Filter,
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
    Regex(RegexPred),
    IsFile,
    IsDir,
    /// Entries that pass a result filter.
    Kind(Filter),
    /// Folded extensions without the leading dot (may contain dots, such as
    /// `tar.gz`), wildcard patterns for an extension (`jp*`), and `none`,
    /// which also accepts files that have no extension.
    Ext {
        exts: Vec<Vec<u8>>,
        globs: Vec<Vec<u8>>,
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
    /// The text, folded unless `case` is set, without diacritics unless
    /// `diacritics` is set.
    pattern: Vec<u8>,
    case: bool,
    on_path: bool,
    glob: bool,
    whole_word: bool,
    diacritics: bool,
    /// Anchored at the start / end of the subject. For a wildcard pattern the
    /// anchors are already in the pattern (a `*` is added on the open side).
    prefix: bool,
    suffix: bool,
    /// Characters left out of the subject before comparing (the pattern
    /// was prepared the same way).
    squeeze: Squeeze,
}

/// A compiled regular expression, matched against the raw name or path.
#[derive(Clone, Debug)]
struct RegexPred {
    re: regex::bytes::Regex,
    on_path: bool,
}

/// Which characters the ignore-punctuation and ignore-whitespace modes leave
/// out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Squeeze {
    pub punctuation: bool,
    pub whitespace: bool,
}

impl Squeeze {
    fn any(self) -> bool {
        self.punctuation || self.whitespace
    }

    /// Whether `c` is left out.
    fn drops(self, c: char) -> bool {
        (self.punctuation && is_punctuation(c)) || (self.whitespace && c.is_whitespace())
    }
}

/// Punctuation for the ignore-punctuation mode: ASCII punctuation (which
/// includes `_`, `-`, `.`, `/` and brackets) and the common punctuation of
/// Latin-1, the General Punctuation block and CJK texts.
pub fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(c,
            '\u{a1}' | '\u{a7}' | '\u{ab}' | '\u{b6}' | '\u{b7}' | '\u{bb}' | '\u{bf}'
            | '\u{2010}'..='\u{2027}'
            | '\u{2030}'..='\u{205e}'
            | '\u{3001}'..='\u{3003}'
            | '\u{3008}'..='\u{3011}'
            | '\u{ff01}'..='\u{ff0f}')
}

/// Appends `text` to `out` without the characters `squeeze` drops. With
/// `keep_wildcards`, `*` and `?` stay (for wildcard patterns). Bytes that are
/// not UTF-8 are kept.
pub fn squeeze_into(text: &[u8], squeeze: Squeeze, keep_wildcards: bool, out: &mut Vec<u8>) {
    let keep = |c: char| (keep_wildcards && (c == '*' || c == '?')) || !squeeze.drops(c);
    if text.is_ascii() {
        out.extend(text.iter().copied().filter(|&b| keep(b as char)));
        return;
    }
    let mut buf = [0u8; 4];
    for chunk in text.utf8_chunks() {
        for c in chunk.valid().chars() {
            if keep(c) {
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        out.extend_from_slice(chunk.invalid());
    }
}

/// Per-node scratch space for predicates that need the full path or a name
/// without diacritics.
#[derive(Default)]
pub struct EvalScratch {
    path: Vec<u8>,
    folded_path: Vec<u8>,
    for_node: Option<NodeId>,
    folded_for_node: Option<NodeId>,
    stripped: Vec<u8>,
    squeezed: Vec<u8>,
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
        if options.regex {
            return Self::parse_regex(text, options);
        }
        let tokens = lex(text)?;
        let mut p = Parser {
            tokens,
            pos: 0,
            options,
            uses_path: false,
            now_ns,
            zone,
        };
        let mut root = p.parse_and(false)?.unwrap_or(Node::All);
        if p.pos < p.tokens.len() {
            return Err(ParseError("unexpected '>'".into()));
        }
        if options.filter != Filter::All {
            root = and_also(root, Node::Leaf(Pred::Kind(options.filter)));
        }
        Ok(Self {
            root,
            uses_path: p.uses_path,
        })
    }

    /// The whole text as one regular expression (regex mode). Blanks at both
    /// ends are dropped; an empty text matches every node.
    fn parse_regex(text: &str, options: QueryOptions) -> Result<Self, ParseError> {
        let text = text.trim();
        let mut root = if text.is_empty() {
            Node::All
        } else {
            Node::Leaf(Pred::Regex(RegexPred {
                re: build_regex(text, options.case_sensitive)?,
                on_path: options.match_path,
            }))
        };
        let uses_path = options.match_path && !text.is_empty();
        if options.filter != Filter::All {
            root = and_also(root, Node::Leaf(Pred::Kind(options.filter)));
        }
        Ok(Self { root, uses_path })
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

/// `node` and `extra` together, keeping cheap conditions first.
fn and_also(node: Node, extra: Node) -> Node {
    let mut items = match node {
        Node::All => return extra,
        Node::And(items) => items,
        other => vec![other],
    };
    items.push(extra);
    items.sort_by_key(cost);
    Node::And(items)
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
        Pred::Kind(filter) => filter.accepts(classify(store.name(id), store.is_dir(id))),
        Pred::Ext { exts, globs, none } => {
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
                || globs.iter().any(|g| {
                    let parts = g.iter().filter(|&&b| b == b'.').count() + 1;
                    extension_parts(name, parts).is_some_and(|ext| glob_match(g, ext))
                })
        }
        Pred::Size { min, max } => {
            // A folder's size is the total of the files below it.
            let size = store.meta(id).size;
            *min <= size && size <= *max
        }
        Pred::Modified { min, max } => {
            let t = store.meta(id).mtime_ns;
            *min <= t && t <= *max
        }
        Pred::Regex(r) => {
            if r.on_path {
                r.re.is_match(path_bytes(store, id, scratch))
            } else {
                r.re.is_match(store.name(id))
            }
        }
        Pred::Text(t) => {
            let mut stripped = std::mem::take(&mut scratch.stripped);
            let mut squeezed = std::mem::take(&mut scratch.squeezed);
            let matched = {
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
                // Most names are ASCII and have no diacritics to remove.
                let subject = if t.diacritics || subject.is_ascii() {
                    subject
                } else {
                    stripped.clear();
                    strip_diacritics_into(subject, &mut stripped);
                    &stripped
                };
                let subject = if t.squeeze.any() {
                    squeezed.clear();
                    squeeze_into(subject, t.squeeze, false, &mut squeezed);
                    &squeezed
                } else {
                    subject
                };
                text_matches(t, subject)
            };
            scratch.stripped = stripped;
            scratch.squeezed = squeezed;
            matched
        }
    }
}

/// The last `parts` dot-separated parts of `name` (`tar.gz` for 2 parts of
/// `x.tar.gz`), when the name has that many after a non-empty start.
fn extension_parts(name: &[u8], parts: usize) -> Option<&[u8]> {
    let mut end = name.len();
    let mut dot = None;
    for _ in 0..parts {
        let at = name[..end].iter().rposition(|&b| b == b'.')?;
        dot = Some(at);
        end = at;
    }
    let dot = dot?;
    (dot > 0).then(|| &name[dot + 1..])
}

fn text_matches(t: &TextPred, subject: &[u8]) -> bool {
    if t.glob {
        glob_match(&t.pattern, subject)
    } else {
        find_term(subject, &t.pattern, t.whole_word, t.prefix, t.suffix).is_some()
    }
}

/// Start of the first occurrence of `needle` in `haystack` that is a whole
/// word when `whole_word` is set (see [`find_word`]), at the very start with
/// `prefix` and at the very end with `suffix` (both: the whole haystack).
pub fn find_term(
    haystack: &[u8],
    needle: &[u8],
    whole_word: bool,
    prefix: bool,
    suffix: bool,
) -> Option<usize> {
    let (check_start, check_end) = if whole_word {
        (
            first_char(needle).is_some_and(is_word_char),
            last_char(needle).is_some_and(is_word_char),
        )
    } else {
        (false, false)
    };
    match (prefix, suffix) {
        (true, true) => (haystack == needle).then_some(0),
        (true, false) => (haystack.starts_with(needle)
            && !(check_end && first_char(&haystack[needle.len()..]).is_some_and(is_word_char)))
        .then_some(0),
        (false, true) => {
            let at = haystack.len().checked_sub(needle.len())?;
            (haystack.ends_with(needle)
                && !(check_start && last_char(&haystack[..at]).is_some_and(is_word_char)))
            .then_some(at)
        }
        (false, false) if whole_word => find_word(haystack, needle),
        (false, false) => find(haystack, needle),
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

/// Returns the start of the first occurrence of `needle` in `haystack` that is
/// a whole word: where `needle` starts with a letter or digit, the character
/// before it is none; where it ends with one, the character after it is none.
/// Any character that is not a letter or digit (Unicode alphanumeric) separates
/// words, including `_`, `-`, `.` and spaces.
pub fn find_word(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let check_start = first_char(needle).is_some_and(is_word_char);
    let check_end = last_char(needle).is_some_and(is_word_char);
    let mut from = 0;
    while from <= haystack.len() {
        let at = from + find(&haystack[from..], needle)?;
        let end = at + needle.len();
        let starts = !check_start || !last_char(&haystack[..at]).is_some_and(is_word_char);
        let ends = !check_end || !first_char(&haystack[end..]).is_some_and(is_word_char);
        if starts && ends {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// The first character of `bytes`, if they start with valid UTF-8.
fn first_char(bytes: &[u8]) -> Option<char> {
    let len = char_len(*bytes.first()?).min(bytes.len());
    std::str::from_utf8(&bytes[..len]).ok()?.chars().next()
}

/// The last character of `bytes`, if they end with valid UTF-8.
fn last_char(bytes: &[u8]) -> Option<char> {
    let start = bytes
        .iter()
        .rev()
        .take(4)
        .position(|&b| b & 0xC0 != 0x80)
        .map(|back| bytes.len() - 1 - back)?;
    std::str::from_utf8(&bytes[start..]).ok()?.chars().next()
}

/// A term of the query that is highlighted in the results: every text,
/// wildcard and regular expression term that is not negated. Terms that
/// match the full path are highlighted in the path (the Folder column and the
/// name), the others in the name only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightTerm {
    /// The text, prepared as for matching: folded unless `case` is set, without
    /// diacritics unless `diacritics` is set, without the characters the
    /// ignore modes leave out. Empty for a regular expression.
    pub pattern: Vec<u8>,
    /// Whether the term matches case exactly.
    pub case: bool,
    /// Whether the term matches whole words only.
    pub whole_word: bool,
    /// Whether the term matches diacritics exactly.
    pub diacritics: bool,
    /// Whether the term must be at the start / the end of its subject.
    pub prefix: bool,
    pub suffix: bool,
    /// Characters left out before comparing.
    pub squeeze: Squeeze,
    /// `pattern` is a wildcard pattern matched against the whole subject
    /// (anchors already in it).
    pub glob: bool,
    /// The term matches the full path instead of the name.
    pub on_path: bool,
    /// A regular expression term, matched against the raw subject.
    pub regex: Option<RegexTerm>,
}

/// A compiled regular expression in a [`HighlightTerm`]. Two are equal when
/// their expressions are.
#[derive(Clone, Debug)]
pub struct RegexTerm(pub regex::bytes::Regex);

impl PartialEq for RegexTerm {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for RegexTerm {}

impl Query {
    /// The terms to highlight, in query order.
    pub fn highlight_terms(&self) -> Vec<HighlightTerm> {
        let mut out = Vec::new();
        collect_highlight_terms(&self.root, &mut out);
        out
    }
}

fn collect_highlight_terms(node: &Node, out: &mut Vec<HighlightTerm>) {
    match node {
        Node::And(items) | Node::Or(items) => {
            for item in items {
                collect_highlight_terms(item, out);
            }
        }
        Node::Leaf(Pred::Text(t)) if !t.pattern.is_empty() => out.push(HighlightTerm {
            pattern: t.pattern.clone(),
            case: t.case,
            whole_word: t.whole_word,
            diacritics: t.diacritics,
            prefix: t.prefix,
            suffix: t.suffix,
            squeeze: t.squeeze,
            glob: t.glob,
            on_path: t.on_path,
            regex: None,
        }),
        Node::Leaf(Pred::Regex(r)) => out.push(HighlightTerm {
            pattern: Vec::new(),
            case: true,
            whole_word: false,
            diacritics: true,
            prefix: false,
            suffix: false,
            squeeze: Squeeze::default(),
            glob: false,
            on_path: r.on_path,
            regex: Some(RegexTerm(r.re.clone())),
        }),
        // Negated terms are not in a matching name.
        Node::All | Node::Not(_) | Node::Leaf(_) => {}
    }
}

/// Byte ranges of `subject` to highlight: every occurrence of every term in
/// `terms` whose `on_path` equals `on_path`, matched the way the term
/// matches (`subject` is then the full path, or the name). The ranges are
/// sorted, do not overlap or touch, and cover whole characters of `subject`,
/// also when folding changes byte lengths (`İ`, `ß`, `ə`).
///
/// A wildcard pattern marks the characters its literal parts matched, not
/// what `*` and `?` stand for. Cost is linear in the subject per term, so
/// call it for the rows on screen only.
pub fn highlight_spans(
    subject: &str,
    terms: &[HighlightTerm],
    on_path: bool,
) -> Vec<std::ops::Range<usize>> {
    // One prepared form of the subject per (case, diacritics, squeeze) combination.
    type Key = (bool, bool, Squeeze);
    let mut forms: Vec<(Key, Mapped)> = Vec::new();
    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    for term in terms.iter().filter(|t| t.on_path == on_path) {
        if let Some(RegexTerm(re)) = &term.regex {
            spans.extend(
                re.find_iter(subject.as_bytes())
                    .filter(|m| {
                        !m.is_empty()
                            && subject.is_char_boundary(m.start())
                            && subject.is_char_boundary(m.end())
                    })
                    .map(|m| m.range()),
            );
            continue;
        }
        if term.pattern.is_empty() {
            continue;
        }
        let key = (term.case, term.diacritics, term.squeeze);
        let form = match forms.iter().position(|(k, _)| *k == key) {
            Some(i) => &forms[i].1,
            None => {
                forms.push((
                    key,
                    Mapped::new(subject, !term.case, !term.diacritics, term.squeeze),
                ));
                &forms[forms.len() - 1].1
            }
        };
        if form.bytes.is_empty() {
            continue;
        }
        if term.glob {
            if let Some(marks) = glob_marks(&term.pattern, &form.bytes) {
                spans.extend(marks.into_iter().map(|r| form.original(r)));
            }
            continue;
        }
        let len = term.pattern.len();
        if term.prefix || term.suffix {
            // Anchored: at most one place.
            let found = find_term(
                &form.bytes,
                &term.pattern,
                term.whole_word,
                term.prefix,
                term.suffix,
            );
            spans.extend(found.map(|at| form.original(at..at + len)));
            continue;
        }
        let mut from = 0;
        while let Some(rel) = find(&form.bytes[from..], &term.pattern) {
            let at = from + rel;
            if !term.whole_word || is_whole_word(&form.bytes, at, len) {
                spans.push(form.original(at..at + len));
            }
            // Overlapping occurrences ("aa" in "aaa") merge below.
            from = at + 1;
            if from + len > form.bytes.len() {
                break;
            }
        }
    }
    merge_spans(spans)
}

/// Whether `len` bytes at `at` of `haystack` are a whole word, as
/// [`find_word`] decides.
fn is_whole_word(haystack: &[u8], at: usize, len: usize) -> bool {
    let needle = &haystack[at..at + len];
    let check_start = first_char(needle).is_some_and(is_word_char);
    let check_end = last_char(needle).is_some_and(is_word_char);
    (!check_start || !last_char(&haystack[..at]).is_some_and(is_word_char))
        && (!check_end || !first_char(&haystack[at + len..]).is_some_and(is_word_char))
}

/// Sorts `spans` and joins the ones that overlap or touch.
pub fn merge_spans(mut spans: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
    spans.sort_unstable_by_key(|r| (r.start, r.end));
    let mut out: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
    for r in spans {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Where the literal characters of a wildcard `pattern` are in `subject`,
/// for the match [`glob_match`] finds, as byte ranges in order; `None`
/// when it does not match.
fn glob_marks(pattern: &[u8], subject: &[u8]) -> Option<Vec<std::ops::Range<usize>>> {
    let (mut p, mut s) = (0, 0);
    // Pattern and subject position after the last `*`, and the marks made
    // before it, which a retry keeps.
    let mut star: Option<(usize, usize, usize)> = None;
    let mut marks: Vec<usize> = Vec::new();
    while s < subject.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, s, marks.len()));
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                s += char_len(subject[s]);
            }
            Some(&c) if c == subject[s] => {
                marks.push(s);
                p += 1;
                s += 1;
            }
            _ => match star {
                Some((sp, ss, kept)) => {
                    let next = ss + char_len(subject[ss]);
                    star = Some((sp, next, kept));
                    marks.truncate(kept);
                    p = sp + 1;
                    s = next;
                }
                None => return None,
            },
        }
    }
    if !pattern[p.min(pattern.len())..].iter().all(|&c| c == b'*') {
        return None;
    }
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for at in marks {
        match ranges.last_mut() {
            Some(last) if last.end == at => last.end = at + 1,
            _ => ranges.push(at..at + 1),
        }
    }
    Some(ranges)
}

/// A name as it is matched (folded, without diacritics and/or without the
/// characters an ignore mode leaves out), with the byte range of the original
/// character behind each of its bytes.
struct Mapped {
    bytes: Vec<u8>,
    /// Per byte of `bytes`: start and end of its character in the name.
    spans: Vec<(usize, usize)>,
}

impl Mapped {
    fn new(name: &str, fold_case: bool, strip: bool, squeeze: Squeeze) -> Self {
        let mut bytes = Vec::with_capacity(name.len());
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(name.len());
        let mut folded = Vec::new();
        let mut buf = [0u8; 4];
        for (at, c) in name.char_indices() {
            // Left out entirely; a match around it still covers it, since
            // a range runs from its first to its last character.
            if squeeze.drops(c) {
                continue;
            }
            let len = bytes.len();
            let raw = c.encode_utf8(&mut buf).as_bytes();
            match (fold_case, strip) {
                (false, false) => bytes.extend_from_slice(raw),
                (true, false) => crate::fold::fold_into(raw, &mut bytes),
                (false, true) => strip_diacritics_into(raw, &mut bytes),
                (true, true) => {
                    folded.clear();
                    crate::fold::fold_into(raw, &mut folded);
                    strip_diacritics_into(&folded, &mut bytes);
                }
            }
            let span = (at, at + c.len_utf8());
            if bytes.len() == len {
                // A dropped combining mark belongs to the character before it.
                for s in spans.iter_mut().rev().take_while(|s| s.1 == at) {
                    s.1 = span.1;
                }
            }
            spans.resize(bytes.len(), span);
        }
        Self { bytes, spans }
    }

    /// The range of whole characters of the name behind `range` of `bytes`.
    fn original(&self, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        self.spans[range.start].0..self.spans[range.end - 1].1
    }
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
            // The rest of a `regex:` word belongs to the expression.
            '|' | '<' | '>' if word_is_regex(&word) => word.push(c),
            // `ext:jpg|png` is one list.
            '|' if word_starts_function(&word, "ext") => word.push(c),
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

/// Whether `word` starts a regular expression: it has a `regex:` modifier
/// (possibly after other modifiers), so `|`, `<` and `>` up to the next space
/// are part of the expression.
fn word_is_regex(word: &str) -> bool {
    word_starts_function(word, "regex")
}

/// Whether `word` has the modifier or function `name:` (possibly after other
/// modifiers).
fn word_starts_function(word: &str, name: &str) -> bool {
    let mut rest = word;
    while let Some((head, tail)) = rest.split_once(':') {
        let head = head.trim_start_matches('\u{0}');
        if head.eq_ignore_ascii_case(name) {
            return true;
        }
        if !KNOWN_MODIFIERS.contains(&head.to_ascii_lowercase().as_str()) {
            return false;
        }
        rest = tail;
    }
    false
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
        let mut on_path = self.options.match_path;
        let mut whole_word = self.options.whole_words;
        let mut diacritics = self.options.match_diacritics;
        let mut prefix = self.options.prefix;
        let mut suffix = self.options.suffix;
        let mut squeeze = Squeeze {
            punctuation: self.options.ignore_punctuation,
            whitespace: self.options.ignore_whitespace,
        };
        let mut regex = false;
        let mut rest = raw;
        // Modifiers can be stacked: case:path:Src/Main
        while let Some((head, tail)) = split_modifier(rest) {
            // Quotes inside a value (`ext:"pdf"`) only group; drop their markers.
            let value: String = tail.chars().filter(|&c| c != '\u{0}').collect();
            match head.as_str() {
                "case" => case = true,
                "nocase" => case = false,
                "path" => on_path = true,
                "nopath" => on_path = false,
                "wholeword" => whole_word = true,
                "nowholeword" => whole_word = false,
                "diacritics" => diacritics = true,
                "nodiacritics" => diacritics = false,
                "prefix" => prefix = true,
                "noprefix" => prefix = false,
                "suffix" => suffix = true,
                "nosuffix" => suffix = false,
                "nopunct" => squeeze.punctuation = true,
                "punct" => squeeze.punctuation = false,
                "nospace" => squeeze.whitespace = true,
                "space" => squeeze.whitespace = false,
                "regex" => regex = true,
                "noregex" => regex = false,
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
        if regex {
            // A `/` does not switch a regular expression to the full path.
            self.uses_path |= on_path;
            return Ok(Some(Node::Leaf(Pred::Regex(RegexPred {
                re: build_regex(&text, case)?,
                on_path,
            }))));
        }
        let glob = !quoted && (text.contains('*') || text.contains('?'));
        let on_path = on_path || text.contains('/');
        self.uses_path |= on_path;
        let mut pattern = if case {
            text.into_bytes()
        } else {
            fold(text.as_bytes())
        };
        if !diacritics {
            pattern = crate::fold::strip_diacritics(&pattern);
        }
        if squeeze.any() {
            let mut kept = Vec::with_capacity(pattern.len());
            squeeze_into(&pattern, squeeze, glob, &mut kept);
            // Nothing left (`nopunct:-`): no condition, like an empty term.
            if kept.is_empty() {
                return Ok(None);
            }
            pattern = kept;
        }
        if glob && prefix != suffix {
            // A pattern matches the whole name; open the side that is not
            // anchored.
            if prefix {
                pattern.push(b'*');
            } else {
                pattern.insert(0, b'*');
            }
        }
        Ok(Some(Node::Leaf(Pred::Text(TextPred {
            pattern,
            case,
            on_path,
            glob,
            whole_word,
            diacritics,
            prefix,
            suffix,
            squeeze,
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
        Node::Leaf(Pred::Regex(r)) if r.on_path => 4,
        Node::Leaf(Pred::Regex(_)) => 3,
        // Classifying looks at the name; let text terms narrow first.
        Node::Leaf(Pred::Kind(_)) => 2,
        Node::Leaf(_) => 0,
        Node::Not(inner) => cost(inner),
        Node::And(items) | Node::Or(items) => items.iter().map(cost).max().unwrap_or(0),
    }
}

/// Splits `name:rest` when `name` is a known modifier (case-insensitive).
fn split_modifier(word: &str) -> Option<(String, &str)> {
    let (head, tail) = word.split_once(':')?;
    let head = head.to_ascii_lowercase();
    KNOWN_MODIFIERS
        .contains(&head.as_str())
        .then_some((head, tail))
}

/// Compiles a regular expression; case is ignored unless `case` is set.
fn build_regex(pattern: &str, case: bool) -> Result<regex::bytes::Regex, ParseError> {
    regex::bytes::RegexBuilder::new(pattern)
        .case_insensitive(!case)
        .build()
        .map_err(|e| ParseError(format!("bad regular expression: {}", regex_error(&e))))
}

/// The one-line reason of a regular expression error. Syntax errors come as
/// several lines (the pattern, a caret and `error: ...`); only the reason is
/// kept.
fn regex_error(e: &regex::Error) -> String {
    match e {
        regex::Error::Syntax(text) => text
            .lines()
            .rev()
            .find_map(|l| l.trim().strip_prefix("error: "))
            .unwrap_or_else(|| text.lines().last().unwrap_or("syntax error"))
            .trim()
            .to_owned(),
        regex::Error::CompiledTooBig(_) => "the expression is too large".to_owned(),
        other => other.to_string(),
    }
}

/// Names of the modifiers, lowercase.
const KNOWN_MODIFIERS: &[&str] = &[
    "case",
    "nocase",
    "path",
    "nopath",
    "wholeword",
    "nowholeword",
    "diacritics",
    "nodiacritics",
    "file",
    "files",
    "folder",
    "folders",
    "dir",
    "ext",
    "size",
    "modified",
    "prefix",
    "noprefix",
    "suffix",
    "nosuffix",
    "nopunct",
    "punct",
    "nospace",
    "space",
    "regex",
    "noregex",
];

/// `ext:pdf;jpg` or `ext:pdf|jpg`; `ext:jp*` with wildcards. Empty entries (as
/// in a trailing `;`) are ignored; `ext:` on its own matches files without an
/// extension.
fn parse_ext(list: &str) -> Pred {
    let (globs, exts): (Vec<Vec<u8>>, Vec<Vec<u8>>) = list
        .split([';', '|'])
        .map(|e| e.trim().trim_start_matches('.'))
        .filter(|e| !e.is_empty())
        .map(|e| fold(e.as_bytes()))
        .partition(|e| e.contains(&b'*') || e.contains(&b'?'));
    let none = exts.is_empty() && globs.is_empty();
    Pred::Ext { exts, globs, none }
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
                    case_sensitive: true,
                    ..QueryOptions::default()
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
        assert_eq!(t.find("size:>1mb file:"), ["Report-2026.PDF"]);
        assert_eq!(t.find("size:1kb..1mb file:"), ["main.rs", "photo.jpg"]);
        // Folders match by the total size of the files below them.
        assert_eq!(t.find("size:>1mb"), ["/home", "Docs", "Report-2026.PDF"]);
        assert_eq!(t.find("size:1kb..1mb folder:"), ["src"]);
        assert_eq!(t.find("size:0 folder:"), ["report"]);
        assert_eq!(
            t.find("size:<1kb file: !photo"),
            ["README", "draft report.pdf"]
        );
        assert_eq!(t.find("photo size:0"), ["photo (1).png"]);
        assert_eq!(t.find("pdf modified:<7d"), ["Report-2026.PDF"]);
        assert_eq!(t.find("pdf modified:>30d"), ["draft report.pdf"]);
    }

    #[test]
    fn filters_combine_with_the_query() {
        let t = Tree::new();
        let with = |filter| QueryOptions {
            filter,
            ..QueryOptions::default()
        };
        assert_eq!(
            t.find_with("photo", with(Filter::Picture)),
            ["photo (1).png", "photo.jpg"]
        );
        assert_eq!(
            t.find_with("report", with(Filter::Document)),
            ["Report-2026.PDF", "draft report.pdf"]
        );
        assert_eq!(t.find_with("report", with(Filter::Folder)), ["report"]);
        assert_eq!(t.find_with("", with(Filter::Code)), ["main.rs"]);
        assert_eq!(t.find_with("report|main", with(Filter::Code)), ["main.rs"]);
        assert!(t.find_with("!photo", with(Filter::Picture)).is_empty());
        assert_eq!(t.find_with("", with(Filter::All)).len(), t.store.len());
        let q = Query::parse("", with(Filter::Video)).unwrap();
        assert!(!q.matches_all());
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

    #[test]
    fn extension_lists_take_bars_and_wildcards() {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/r", true, Meta::default());
        for name in [
            &b"a.jpg"[..],
            b"b.PNG",
            b"c.jpeg",
            b"pngcrush.c",
            b"archive.tar.gz",
            b"notes",
        ] {
            store.add(Some(root), name, false, Meta::default());
        }
        store.add(Some(root), b"png-icons", true, Meta::default());
        let t = Tree { store };
        // `|` separates extensions like `;`, it is not an OR with text.
        assert_eq!(t.find("ext:jpg|png"), ["a.jpg", "b.PNG"]);
        assert_eq!(t.find("ext:jpg;png"), ["a.jpg", "b.PNG"]);
        assert_eq!(t.find("ext:.jpg|.png;"), ["a.jpg", "b.PNG"]);
        assert_eq!(t.find("<ext:jpg|png>"), ["a.jpg", "b.PNG"]);
        assert_eq!(t.find("case:ext:jpg|png"), ["a.jpg", "b.PNG"]);
        assert_eq!(t.find("ext:jpg|png !b"), ["a.jpg"]);
        // A spaced `|` is still an OR.
        assert_eq!(t.find("ext:jpg | notes"), ["a.jpg", "notes"]);
        // Wildcards match the extension, not the whole name.
        assert_eq!(t.find("ext:jp*"), ["a.jpg", "c.jpeg"]);
        assert_eq!(t.find("ext:JP?"), ["a.jpg"]);
        assert_eq!(t.find("ext:p*|c"), ["b.PNG", "pngcrush.c"]);
        assert_eq!(t.find("ext:t*.gz"), ["archive.tar.gz"]);
        assert_eq!(t.find("ext:*.*"), ["archive.tar.gz"]);
        // `*` is any extension; files without one and folders stay out.
        assert_eq!(t.find("ext:*").len(), 5);
        assert!(t.find("ext:*").iter().all(|n| n != "notes"));
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

    /// Highlighted ranges of a name for `q`.
    fn spans(name: &str, q: &str) -> Vec<(usize, usize)> {
        spans_with(name, q, QueryOptions::default(), false)
    }

    fn spans_with(subject: &str, q: &str, o: QueryOptions, on_path: bool) -> Vec<(usize, usize)> {
        let query = Query::parse(q, o).unwrap();
        highlight_spans(subject, &query.highlight_terms(), on_path)
            .into_iter()
            .map(|r| (r.start, r.end))
            .collect()
    }

    /// Highlighted ranges of a full path for `q`.
    fn path_spans(path: &str, q: &str) -> Vec<(usize, usize)> {
        spans_with(path, q, QueryOptions::default(), true)
    }

    #[test]
    fn highlight_terms_skip_negated_terms() {
        let q = Query::parse(
            "Rep !draft *.pdf path:src a/b case:Main <x|y> ext:pdf !regex:z regex:q",
            QueryOptions::default(),
        )
        .unwrap();
        let mut terms: Vec<(String, bool, bool, bool)> = q
            .highlight_terms()
            .into_iter()
            .map(|t| {
                let text = match &t.regex {
                    Some(re) => format!("re:{}", re.0.as_str()),
                    None => String::from_utf8(t.pattern).unwrap(),
                };
                (text, t.case, t.glob, t.on_path)
            })
            .collect();
        terms.sort();
        assert_eq!(
            terms,
            [
                ("*.pdf".into(), false, true, false),
                ("Main".into(), true, false, false),
                // A `/` makes a path term.
                ("a/b".into(), false, false, true),
                ("re:q".into(), true, false, false),
                ("rep".into(), false, false, false),
                ("src".into(), false, false, true),
                ("x".into(), false, false, false),
                ("y".into(), false, false, false),
            ]
        );
    }

    #[test]
    fn every_occurrence_of_every_term_is_highlighted() {
        assert_eq!(spans("Report-2026.PDF", "pdf"), [(12, 15)]);
        assert_eq!(spans("Report-2026.PDF", "zzz rep"), [(0, 3)]);
        assert_eq!(spans("a-b-a", "a"), [(0, 1), (4, 5)]);
        assert_eq!(
            spans("report-2026-rep.pdf", "rep 2026"),
            [(0, 3), (7, 11), (12, 15)]
        );
        assert_eq!(spans("readme", "case:READ"), Vec::new());
        assert_eq!(spans("README", "case:READ"), [(0, 4)]);
        assert_eq!(spans("draft", "!draft"), Vec::new());
        assert_eq!(spans("draft-x", "x !draft"), [(6, 7)]);
        assert_eq!(spans("x.pdf", ""), Vec::new());
        // OR alternatives are highlighted wherever they occur.
        assert_eq!(spans("jpg-png", "jpg|png"), [(0, 3), (4, 7)]);
    }

    #[test]
    fn overlapping_and_touching_matches_merge() {
        assert_eq!(spans("aaaa", "aa"), [(0, 4)]);
        assert_eq!(spans("abcdef", "abc cd"), [(0, 4)]);
        assert_eq!(spans("abcdef", "abc def"), [(0, 6)]);
        assert_eq!(spans("abcdef", "bcd abcdef"), [(0, 6)]);
        assert_eq!(spans("ab-ab", "ab b"), [(0, 2), (3, 5)]);
    }

    #[test]
    fn highlight_folds_case_and_diacritics_like_the_matcher() {
        // Folding changes byte lengths ("İ" → "i", "Ə" → "e"); the ranges
        // still cover whole characters of the name.
        assert_eq!(spans("ŞƏKİL.png", "png"), [(9, 12)]);
        assert_eq!(spans("ŞƏKIL.png", "əkil"), [(2, 7)]);
        assert_eq!(spans("İzmir", "izmir"), [(0, 6)]);
        assert_eq!(spans("İzmir", "zmir"), [(2, 6)]);
        assert_eq!(spans("Straße", "sse"), [(4, 7)]);
        assert_eq!(spans("Straße", "strass"), [(0, 6)]);
        assert_eq!(spans("cafe\u{301}.txt", "cafe"), [(0, 6)]);
        assert_eq!(spans("Café-cafe-CAFÉ", "cafe"), [(0, 5), (6, 10), (11, 16)]);
        // Exact terms use the name as it is.
        assert_eq!(spans("İzmir", "case:zmir"), [(2, 6)]);
        assert_eq!(spans("Café-cafe", "case:cafe"), [(6, 10)]);
        assert_eq!(spans("café-cafe", "diacritics:café"), [(0, 5)]);
        assert_eq!(spans("şəkil", "diacritics:əkil"), [(2, 7)]);
        assert_eq!(spans("sekil", "diacritics:əkil"), Vec::new());
    }

    #[test]
    fn highlight_follows_whole_words() {
        assert_eq!(spans("party-art.png", "wholeword:art"), [(6, 9)]);
        assert_eq!(spans("art-party-art", "wholeword:art"), [(0, 3), (10, 13)]);
        assert_eq!(spans("party.png", "wholeword:art"), Vec::new());
        assert_eq!(spans("party.png", "art"), [(1, 4)]);
    }

    #[test]
    fn wildcards_highlight_their_literal_parts() {
        assert_eq!(spans("x.pdf", "*.pdf"), [(1, 5)]);
        assert_eq!(spans("Report.PDF", "rep*.pdf"), [(0, 3), (6, 10)]);
        // `?` stands for a character but is not highlighted.
        assert_eq!(spans("img_7.png", "img_?.png"), [(0, 4), (5, 9)]);
        // The match found is the one the matcher finds: `*` takes as little
        // as it can, then retries.
        assert_eq!(spans("a-b-b.txt", "a*b.txt"), [(0, 1), (4, 9)]);
        assert_eq!(spans("ŞƏKİL.png", "şəkil*"), [(0, 8)]);
        assert_eq!(spans("x.pdf", "*.doc"), Vec::new());
        assert_eq!(glob_marks(b"*", b"abc"), Some(Vec::new()));
        assert_eq!(glob_marks(b"a*c", b"abd"), None);
    }

    #[test]
    fn path_terms_are_highlighted_in_the_path() {
        let path = "/home/me/src/main-src.rs";
        assert_eq!(path_spans(path, "path:src"), [(9, 12), (18, 21)]);
        assert_eq!(path_spans(path, "me/src"), [(6, 12)]);
        // Name terms are not highlighted in the path, nor path terms in the
        // name on its own.
        assert_eq!(path_spans(path, "main"), Vec::new());
        assert_eq!(spans("main-src.rs", "path:src"), Vec::new());
        // Prefix anchors at the start of the path, wildcards match all of it.
        assert_eq!(path_spans(path, "prefix:path:/home"), [(0, 5)]);
        assert_eq!(path_spans(path, "path:/home/*.rs"), [(0, 6), (21, 24)]);
        assert_eq!(path_spans(path, r"path:regex:/s\w+"), [(8, 12)]);
        // The whole query on the path.
        let o = with(|o| o.match_path = true);
        assert_eq!(
            spans_with(path, "me src", o, true),
            [(3, 5), (6, 8), (9, 12), (18, 21)]
        );
        assert_eq!(spans_with("main-src.rs", "src", o, false), Vec::new());
    }

    fn with(f: impl FnOnce(&mut QueryOptions)) -> QueryOptions {
        let mut o = QueryOptions::default();
        f(&mut o);
        o
    }

    fn named(names: &[&str]) -> Tree {
        let mut store = NodeStore::new();
        let root = store.add(None, b"/r", true, Meta::default());
        let sub = store.add(Some(root), b"S\xc3\xbcd", true, Meta::default());
        for name in names {
            store.add(Some(sub), name.as_bytes(), false, Meta::default());
        }
        Tree { store }
    }

    #[test]
    fn whole_words() {
        let t = named(&[
            "art.png",
            "party.png",
            "pop-art_2026.jpg",
            "Art Deco.pdf",
            "arts.txt",
            "çart.txt",
            "a.pdf.bak",
        ]);
        let ww = with(|o| o.whole_words = true);
        assert_eq!(
            t.find_with("art", ww),
            ["Art Deco.pdf", "art.png", "pop-art_2026.jpg"]
        );
        assert_eq!(t.find("wholeword:art"), t.find_with("art", ww));
        assert_eq!(t.find_with("nowholeword:art", ww).len(), 6);
        // Non-word characters at the edges of the term need no boundary.
        assert_eq!(t.find("wholeword:.pdf"), ["Art Deco.pdf", "a.pdf.bak"]);
        assert_eq!(t.find("wholeword:\"art deco\""), ["Art Deco.pdf"]);
        // Wildcards already match the whole name.
        assert_eq!(
            t.find("wholeword:art*"),
            ["Art Deco.pdf", "art.png", "arts.txt"]
        );
    }

    #[test]
    fn find_word_boundaries() {
        assert_eq!(find_word(b"party art", b"art"), Some(6));
        assert_eq!(find_word(b"partyart", b"art"), None);
        assert_eq!(find_word("şart art".as_bytes(), b"art"), Some(6));
        assert_eq!(find_word(b"\xffart", b"art"), Some(1));
        assert_eq!(find_word(b"art", b""), Some(0));
        assert_eq!(find_word(b"", b"art"), None);
    }

    #[test]
    fn diacritics_are_ignored_unless_matched() {
        let t = named(&[
            "Şəkil.png",
            "sekil.png",
            "café.txt",
            "cafe\u{301}-nfd.txt",
            "Straße.md",
            "İzmir.jpg",
            "ızgara.txt",
            "Ölçü.ods",
        ]);
        assert_eq!(t.find("sekil"), ["sekil.png", "Şəkil.png"]);
        assert_eq!(t.find("şəkil"), ["sekil.png", "Şəkil.png"]);
        assert_eq!(t.find("diacritics:şəkil"), ["Şəkil.png"]);
        assert_eq!(t.find("cafe"), ["cafe\u{301}-nfd.txt", "café.txt"]);
        assert_eq!(t.find("strasse"), ["Straße.md"]);
        assert_eq!(t.find("izmir"), ["İzmir.jpg"]);
        assert_eq!(t.find("izgara"), ["ızgara.txt"]);
        assert_eq!(t.find("olcu"), ["Ölçü.ods"]);
        let exact = with(|o| o.match_diacritics = true);
        assert_eq!(t.find_with("sekil", exact), ["sekil.png"]);
        assert_eq!(t.find_with("nodiacritics:sekil", exact).len(), 2);
        // Case-sensitive and diacritic-insensitive together.
        assert_eq!(t.find("case:Sekil"), ["Şəkil.png"]);
        assert!(t.find("case:SEKIL").is_empty());
        // Wildcards and paths too ("Süd" is the folder).
        assert_eq!(t.find("s?kil.*"), ["sekil.png", "Şəkil.png"]);
        assert_eq!(t.find("sud/olcu"), ["Ölçü.ods"]);
    }

    #[test]
    fn match_path_option() {
        let t = named(&["a.txt", "sud.txt"]);
        let path = with(|o| o.match_path = true);
        // The folder "Süd" matches by its name, "a.txt" only by its path.
        assert_eq!(t.find("sud"), ["Süd", "sud.txt"]);
        assert_eq!(t.find_with("sud", path), ["Süd", "a.txt", "sud.txt"]);
        assert_eq!(t.find_with("nopath:sud", path), ["Süd", "sud.txt"]);
    }

    #[test]
    fn prefix_and_suffix_anchor_the_term() {
        let t = named(&[
            "report.pdf",
            "draft report.txt",
            "reports.tar.gz",
            "old.tar.gz.bak",
            "Rep",
        ]);
        assert_eq!(
            t.find("prefix:rep"),
            ["Rep", "report.pdf", "reports.tar.gz"]
        );
        assert_eq!(t.find("suffix:.tar.gz"), ["reports.tar.gz"]);
        assert_eq!(t.find("prefix:suffix:rep"), ["Rep"]);
        // The option, and the `no…:` forms that undo it per term.
        let pre = with(|o| o.prefix = true);
        assert_eq!(t.find_with("report", pre), ["report.pdf", "reports.tar.gz"]);
        assert_eq!(t.find_with("noprefix:report", pre).len(), 3);
        let suf = with(|o| o.suffix = true);
        assert_eq!(t.find_with("txt", suf), ["draft report.txt"]);
        assert_eq!(t.find_with("nosuffix:txt", suf), ["draft report.txt"]);
        assert_eq!(t.find_with("gz", suf), ["reports.tar.gz"]);
    }

    #[test]
    fn prefix_and_suffix_with_whole_words_and_wildcards() {
        let t = named(&["art.png", "artist.png", "pop-art", "popart", "img_01.png"]);
        // Whole words check the open side only.
        assert_eq!(t.find("prefix:wholeword:art"), ["art.png"]);
        assert_eq!(t.find("suffix:wholeword:art"), ["pop-art"]);
        assert_eq!(t.find("suffix:art"), ["pop-art", "popart"]);
        // A wildcard pattern matches the whole name; prefix or suffix opens
        // the other end.
        assert_eq!(t.find("img_??"), Vec::<String>::new());
        assert_eq!(t.find("prefix:img_??"), ["img_01.png"]);
        assert_eq!(
            t.find("suffix:*.png"),
            ["art.png", "artist.png", "img_01.png"]
        );
        assert_eq!(
            t.find("suffix:?.png"),
            ["art.png", "artist.png", "img_01.png"]
        );
        assert_eq!(t.find("prefix:suffix:art*"), ["art.png", "artist.png"]);
    }

    #[test]
    fn punctuation_and_whitespace_can_be_ignored() {
        let t = named(&[
            "foo_bar.txt",
            "foo-bar.txt",
            "foobar.txt",
            "foo bar.txt",
            "foo\u{2013}bar.txt",
            "my  file.txt",
            "Ş-ə.txt",
        ]);
        assert_eq!(t.find("foo-bar"), ["foo-bar.txt"]);
        assert_eq!(
            t.find("nopunct:foo-bar"),
            [
                "foo-bar.txt",
                "foo_bar.txt",
                "foobar.txt",
                "foo\u{2013}bar.txt"
            ]
        );
        let punct = with(|o| o.ignore_punctuation = true);
        assert_eq!(t.find_with("foobartxt", punct).len(), 4);
        assert_eq!(t.find_with("punct:foo_bar", punct), ["foo_bar.txt"]);
        assert_eq!(t.find_with("se", punct), ["Ş-ə.txt"]);
        assert_eq!(t.find("nospace:myfile"), ["my  file.txt"]);
        assert_eq!(t.find("nospace:\"foo bar\""), ["foo bar.txt", "foobar.txt"]);
        let space = with(|o| o.ignore_whitespace = true);
        assert_eq!(t.find_with("space:\"foo bar\"", space), ["foo bar.txt"]);
        // Both modes, with wildcards kept as wildcards.
        assert_eq!(
            t.find("nopunct:nospace:foo?bar*").len(),
            0,
            "the dash is gone, so ? has nothing to match"
        );
        assert_eq!(t.find("nopunct:nospace:foobar*").len(), 5);
        // Punctuation no longer separates words.
        let art = named(&["pop-art", "pop art"]);
        assert_eq!(art.find("nopunct:wholeword:art"), ["pop art"]);
        assert_eq!(art.find("wholeword:art").len(), 2);
        // A term with nothing left is no condition.
        assert_eq!(t.find("nopunct:-").len(), t.find("").len());
    }

    #[test]
    fn regular_expressions() {
        let t = named(&["IMG_0042.JPG", "img_7.png", "notes.md", "Şəkil.png", "a|b"]);
        assert_eq!(t.find(r"regex:^img_\d+\."), ["IMG_0042.JPG", "img_7.png"]);
        // Case: ignored by default, matched with case: or the option.
        assert_eq!(t.find(r"case:regex:^img_\d+"), ["img_7.png"]);
        let case = with(|o| o.case_sensitive = true);
        assert_eq!(t.find_with(r"regex:^IMG", case), ["IMG_0042.JPG"]);
        // `|`, `<` and `>` belong to the expression up to the next space.
        assert_eq!(t.find(r"regex:(jpg|md)$"), ["IMG_0042.JPG", "notes.md"]);
        assert_eq!(t.find(r"regex:^a\|b$"), ["a|b"]);
        assert_eq!(
            t.find(r"regex:md$|png$ !regex:^img"),
            ["notes.md", "Şəkil.png"]
        );
        // Unicode case folding, but no diacritic folding.
        assert_eq!(t.find("regex:^şə"), ["Şəkil.png"]);
        assert_eq!(t.find("regex:^se").len(), 0);
        // In the full path, and a `/` alone does not switch to it.
        assert_eq!(t.find("path:regex:/Süd/notes"), ["notes.md"]);
        assert_eq!(t.find("regex:/Süd/notes").len(), 0);
        // Bad expressions are errors with a one-line reason.
        let e = Query::parse("regex:(abc", QueryOptions::default()).unwrap_err();
        assert!(e.0.starts_with("bad regular expression: "), "{e}");
        assert!(!e.0.contains('\n'), "{e}");
        assert!(Query::parse("regex:a{99999999}", QueryOptions::default()).is_err());
        assert_eq!(t.find("noregex:a|b"), ["a|b"]);
    }

    #[test]
    fn regex_mode_takes_the_whole_query() {
        let t = named(&["IMG_0042.JPG", "img_7.png", "notes.md", "my report.pdf"]);
        let re = with(|o| o.regex = true);
        assert_eq!(
            t.find_with(r"(jpg|png)$", re),
            ["IMG_0042.JPG", "img_7.png"]
        );
        assert_eq!(t.find_with("y r", re), ["my report.pdf"]);
        assert_eq!(t.find_with("  ", re).len(), t.find("").len());
        let re_case = with(|o| {
            o.regex = true;
            o.case_sensitive = true;
        });
        assert_eq!(t.find_with("^img", re_case), ["img_7.png"]);
        let re_path = with(|o| {
            o.regex = true;
            o.match_path = true;
        });
        assert_eq!(t.find_with("^/r/Süd/n", re_path), ["notes.md"]);
        let re_filter = with(|o| {
            o.regex = true;
            o.filter = Filter::Picture;
        });
        assert_eq!(t.find_with("g", re_filter), ["IMG_0042.JPG", "img_7.png"]);
        assert!(Query::parse("(", re).is_err());
    }

    #[test]
    fn highlight_follows_the_new_modes() {
        assert_eq!(spans("art-art", "suffix:art"), [(4, 7)]);
        assert_eq!(spans("art-art", "prefix:art"), [(0, 3)]);
        // The dash in the middle is covered, the ones outside are not.
        assert_eq!(spans("x-foo_bar-y", "nopunct:foobar"), [(2, 9)]);
        assert_eq!(spans("my  file", "nospace:yf"), [(1, 5)]);
        assert_eq!(spans("IMG_0042.JPG", r"regex:\d+"), [(4, 8)]);
        assert_eq!(spans("a1b22c333", r"regex:\d+"), [(1, 2), (3, 5), (6, 9)]);
        assert_eq!(spans("şəkil", "regex:Ə"), [(2, 4)]);
        // An empty match is no highlight.
        assert_eq!(spans("abc", "regex:x*"), Vec::new());
        assert_eq!(
            spans_with("IMG_7.png", "7", with(|o| o.regex = true), false),
            [(4, 5)]
        );
    }
}
