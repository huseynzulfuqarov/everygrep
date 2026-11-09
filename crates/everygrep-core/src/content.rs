//! Searching the contents of files, for the `content:` terms of a query.
//!
//! The index keeps names only, so contents are read from disk when a query
//! asks for them. A [`ContentTerm`] is a text or regular expression and the
//! encoding to read files in; [`search_file`] tells whether one file holds
//! it. Files are read in buffers of 64 KiB with the line searcher of ripgrep
//! (`grep-searcher`), which stops at the first match, so a match near the
//! start of a large file costs one buffer (a search over line ends reads
//! the whole file, see below).
//!
//! What is read:
//!
//! - regular files only: links are not followed, and FIFOs, sockets and
//!   devices are never opened for reading;
//! - files up to [`ContentOptions::max_size`] (0 for any size);
//! - not binary files: a NUL byte (after decoding) ends the search of a file
//!   without a match, unless a match came before it;
//! - in the encoding of the term. [`ContentEncoding::Auto`] reads UTF-8,
//!   UTF-16 when the file starts with its byte order mark, and a file that
//!   is not valid UTF-8 again in the legacy code page of the options
//!   ([`CodePage`], Windows-1254 by default);
//! - with [`ContentOptions::pdf`], a file named `*.pdf` as the text that
//!   `pdftotext` (poppler) prints for it, in a child process that is killed
//!   on cancel or after [`PDF_TIMEOUT`], so a broken file cannot take the
//!   program down or hang the search.
//!
//! A match may run over line ends ([`ContentOptions::multi_line`], on by
//! default): a space in a text also matches a line break with the spaces
//! around it, and `\n`, `\s` and classes in a regular expression match line
//! breaks too (`.` does not). Files larger than [`MULTI_LINE_MAX`], and every
//! file with the option off, are searched line by line, where a match ends
//! at the end of its line. `^` and `$` match at the start and end of every
//! line either way. Case is ignored unless the term asks for it (Unicode
//! simple case folding).
//! Diacritics are matched as written: ignoring them is a mode for names only.
//!
//! The access time of a file is not changed when the file belongs to the
//! user (`O_NOATIME`).
//!
//! A search runs in two stages. The index decides what it can
//! ([`crate::search::search_staged`]); for the rest it gives the paths and a
//! table of how the answers of the content terms decide
//! ([`crate::query::Stage`]). [`decide_all`] then reads those files on
//! worker threads, in name order, each file only for the terms that still
//! matter, and records every answer in [`Decisions`] as it comes, so the
//! matches found so far can be shown while it runs. A [`ContentCache`]
//! keeps the answers for the next search of the same terms.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Encoding, Searcher, SearcherBuilder, Sink, SinkMatch};

use crate::progress::{Cancelled, Progress};

/// The largest file read by default: 100 MiB.
pub const DEFAULT_MAX_SIZE: u64 = 100 * 1024 * 1024;

/// Longest time `pdftotext` may take for one file.
pub const PDF_TIMEOUT: Duration = Duration::from_secs(30);

/// The program that prints the text of a PDF file.
pub const PDF_READER: &str = "pdftotext";

/// Longest line kept in memory while searching a file (a minified file can
/// be one line); a file with a longer line is not searched further.
const MAX_LINE: usize = 64 * 1024 * 1024;

/// Largest file searched with matches over line ends: such a search holds
/// the whole file (as UTF-8, which UTF-16 text can grow to 1.5 times) in
/// memory. Larger files are searched line by line.
pub const MULTI_LINE_MAX: u64 = 16 * 1024 * 1024;

/// The encoding a term reads files in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ContentEncoding {
    /// UTF-8; UTF-16 with a byte order mark; else the legacy code page.
    #[default]
    Auto,
    Utf8,
    Utf16Le,
    Utf16Be,
    /// The legacy code page of the options.
    Ansi,
}

/// The legacy (Windows) code page for files that are not UTF-8.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CodePage {
    /// Central European.
    Windows1250,
    /// Cyrillic.
    Windows1251,
    /// Western European.
    Windows1252,
    /// Greek.
    Windows1253,
    /// Turkish, also used for Azerbaijani Latin.
    #[default]
    Windows1254,
    /// Baltic.
    Windows1257,
}

impl CodePage {
    pub const ALL: [Self; 6] = [
        Self::Windows1250,
        Self::Windows1251,
        Self::Windows1252,
        Self::Windows1253,
        Self::Windows1254,
        Self::Windows1257,
    ];

    /// The WHATWG label, as the settings store it.
    pub fn id(self) -> &'static str {
        match self {
            Self::Windows1250 => "windows-1250",
            Self::Windows1251 => "windows-1251",
            Self::Windows1252 => "windows-1252",
            Self::Windows1253 => "windows-1253",
            Self::Windows1254 => "windows-1254",
            Self::Windows1257 => "windows-1257",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.id() == id)
    }

    pub(crate) fn encoding(self) -> &'static encoding_rs::Encoding {
        match self {
            Self::Windows1250 => encoding_rs::WINDOWS_1250,
            Self::Windows1251 => encoding_rs::WINDOWS_1251,
            Self::Windows1252 => encoding_rs::WINDOWS_1252,
            Self::Windows1253 => encoding_rs::WINDOWS_1253,
            Self::Windows1254 => encoding_rs::WINDOWS_1254,
            Self::Windows1257 => encoding_rs::WINDOWS_1257,
        }
    }
}

/// How files are read for content terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentOptions {
    /// Larger files are not read; 0 reads files of any size.
    pub max_size: u64,
    /// The code page of files that are not UTF-8.
    pub code_page: CodePage,
    /// Read the text of PDF files with [`PDF_READER`].
    pub pdf: bool,
    /// Extract text from ZIP-based Office and EPUB documents.
    pub documents: bool,
    /// Expanded archive and extracted text limit, capped by the extractor.
    pub document_max_bytes: u64,
    /// A match may run over line ends (in files up to [`MULTI_LINE_MAX`]).
    pub multi_line: bool,
}

impl Default for ContentOptions {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_SIZE,
            code_page: CodePage::default(),
            pdf: false,
            documents: true,
            document_max_bytes: crate::document::DEFAULT_MAX_BYTES,
            multi_line: true,
        }
    }
}

/// A text looked for in the contents of files.
#[derive(Clone)]
pub struct ContentTerm {
    /// The matcher line by line. For an expression that names a line break
    /// (which no line holds) it is the one for matches over line ends, run
    /// on each line without its end, so its other parts still match there.
    matcher: RegexMatcher,
    /// The matcher for matches over line ends, when the term can match a
    /// line break there.
    multi_line: Option<RegexMatcher>,
    encoding: ContentEncoding,
    /// A literal text of ASCII characters only: the legacy code pages are
    /// ASCII supersets, so a file that did not match as UTF-8 cannot match
    /// in one.
    ascii_literal: bool,
    key: String,
    digest: Option<(crate::hash::Algorithm, Vec<u8>)>,
}

impl fmt::Debug for ContentTerm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContentTerm")
            .field("key", &self.key)
            .finish()
    }
}

impl ContentTerm {
    /// A term for `text`: a literal text, or with `regex` a regular
    /// expression. Case is ignored unless `case` is set; `whole_word` matches
    /// only whole words. Fails with the reason when the expression is not
    /// valid.
    pub fn new(
        text: &str,
        regex: bool,
        case: bool,
        whole_word: bool,
        encoding: ContentEncoding,
    ) -> Result<Self, String> {
        let pattern = if regex {
            text.to_owned()
        } else {
            regex::escape(text)
        };
        let build = |pattern: &str, line_terminator| {
            RegexMatcherBuilder::new()
                .case_insensitive(!case)
                .multi_line(true)
                .line_terminator(line_terminator)
                .word(whole_word)
                .build(pattern)
                .map_err(|e| e.to_string())
        };
        // A text crosses a line end only at its spaces; an expression may
        // anywhere.
        let (matcher, multi_line) = if regex {
            let multi_line = build(&pattern, None)?;
            let line = build(&pattern, Some(b'\n')).unwrap_or_else(|_| multi_line.clone());
            (line, Some(multi_line))
        } else if text.contains(char::is_whitespace) {
            let multi_line = build(&spaces_over_lines(text), None)?;
            let line = build(&pattern, Some(b'\n')).unwrap_or_else(|_| multi_line.clone());
            (line, Some(multi_line))
        } else {
            (build(&pattern, Some(b'\n'))?, None)
        };
        Ok(Self {
            matcher,
            multi_line,
            encoding,
            ascii_literal: !regex && text.is_ascii(),
            digest: None,
            key: format!(
                "{encoding:?}/{}/{}/{}/{text}",
                u8::from(regex),
                u8::from(case),
                u8::from(whole_word)
            ),
        })
    }

    /// An exact digest comparison, evaluated by the same cancellable workers
    /// as text contents, with the hash cache shared by columns and sorting.
    pub fn hash(algorithm: crate::hash::Algorithm, value: &str) -> Result<Self, String> {
        let expected = algorithm.parse_hex(value)?;
        let mut term = Self::new("", false, false, false, ContentEncoding::Auto)?;
        term.key = format!(
            "hash/{}/{}",
            algorithm.keyword(),
            crate::hash::hex(&expected)
        );
        term.digest = Some((algorithm, expected));
        Ok(term)
    }

    pub fn hash_algorithm(&self) -> Option<crate::hash::Algorithm> {
        self.digest.as_ref().map(|(algorithm, _)| *algorithm)
    }

    /// Identifies what the term matches: equal keys match the same files.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn encoding(&self) -> ContentEncoding {
        self.encoding
    }

    /// The matcher to search a file of `size` bytes with.
    fn matcher(&self, options: &ContentOptions, size: u64) -> Matcher<'_> {
        match &self.multi_line {
            Some(m) if options.multi_line && size <= MULTI_LINE_MAX => Matcher {
                regex: m,
                multi_line: true,
            },
            _ => Matcher {
                regex: &self.matcher,
                multi_line: false,
            },
        }
    }
}

/// A matcher and whether it searches over line ends.
#[derive(Clone, Copy)]
struct Matcher<'a> {
    regex: &'a RegexMatcher,
    multi_line: bool,
}

/// An expression for the literal `text` in which every run of spaces also
/// matches a line break with the spaces and tabs around it.
fn spaces_over_lines(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
        out.push_str(&regex::escape(&rest[..word]));
        rest = &rest[word..];
        let space = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        if space > 0 {
            out.push_str(&format!(
                "(?:{}|[ \t]*\r?\n[ \t]*)",
                regex::escape(&rest[..space])
            ));
            rest = &rest[space..];
        }
    }
    out
}

/// What reading one file for a term gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Match,
    NoMatch,
    /// A NUL byte came before any match.
    Binary,
    /// Larger than the limit; not read.
    TooLarge,
    /// A link, folder, FIFO, socket or device; not read.
    NotAFile,
    /// It could not be opened or read (permissions, gone, a line longer
    /// than 64 MiB; for a PDF file: no [`PDF_READER`], or it failed or took
    /// longer than [`PDF_TIMEOUT`]).
    Unreadable,
}

impl Outcome {
    pub fn is_match(self) -> bool {
        self == Self::Match
    }
}

/// Whether the file at `path` holds `term`. Stops with [`Cancelled`] soon
/// after `progress` is cancelled: before each buffer read.
pub fn search_file(
    path: &Path,
    term: &ContentTerm,
    options: &ContentOptions,
    progress: &Progress,
) -> Result<Outcome, Cancelled> {
    search_file_with_hashes(path, term, options, None, progress)
}

fn search_file_with_hashes(
    path: &Path,
    term: &ContentTerm,
    options: &ContentOptions,
    hashes: Option<&Mutex<crate::hashcache::HashCache>>,
    progress: &Progress,
) -> Result<Outcome, Cancelled> {
    progress.check()?;
    if let Some((algorithm, expected)) = &term.digest {
        return match crate::hash::read_cached(path, algorithm.wanted(), hashes, progress) {
            Ok(found) => Ok(
                if algorithm.bytes(&found).as_deref() == Some(expected.as_slice()) {
                    Outcome::Match
                } else {
                    Outcome::NoMatch
                },
            ),
            Err(crate::hash::HashError::Cancelled) => Err(Cancelled),
            Err(crate::hash::HashError::NotAFile) => Ok(Outcome::NotAFile),
            Err(crate::hash::HashError::Unreadable) => Ok(Outcome::Unreadable),
        };
    }
    let mut file = match open_regular(path) {
        Ok(file) => file,
        Err(outcome) => return Ok(outcome),
    };
    let size = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => return Ok(Outcome::Unreadable),
    };
    if options.max_size != 0 && size > options.max_size {
        return Ok(Outcome::TooLarge);
    }
    if let Some(kind) = crate::document::Kind::of(path) {
        if !options.documents {
            return Ok(Outcome::NoMatch);
        }
        return match crate::document::extract(file, kind, options.document_max_bytes, progress) {
            Ok(text) => search_reader(
                text.as_bytes(),
                term.matcher(options, text.len() as u64),
                Some(encoding_rs::UTF_8),
                progress,
            )
            .map(|(outcome, _)| outcome),
            Err(crate::document::Error::Cancelled) => Err(Cancelled),
            Err(crate::document::Error::Limit) => Ok(Outcome::TooLarge),
            Err(crate::document::Error::Invalid) => Ok(Outcome::Unreadable),
        };
    }
    let matcher = term.matcher(options, size);
    if options.pdf && is_pdf_name(path) {
        return search_pdf(path, term, options, size, progress);
    }
    let forced = match term.encoding {
        ContentEncoding::Auto => None,
        ContentEncoding::Utf8 => Some(encoding_rs::UTF_8),
        ContentEncoding::Utf16Le => Some(encoding_rs::UTF_16LE),
        ContentEncoding::Utf16Be => Some(encoding_rs::UTF_16BE),
        ContentEncoding::Ansi => Some(options.code_page.encoding()),
    };
    if forced.is_some() {
        return search_reader(&mut file, matcher, forced, progress).map(|(outcome, _)| outcome);
    }
    let (outcome, seen) = search_reader(&mut file, matcher, None, progress)?;
    if outcome != Outcome::NoMatch || seen.bom || seen.utf8.is_valid() || term.ascii_literal {
        return Ok(outcome);
    }
    // Not UTF-8: read it again in the legacy code page.
    if file.rewind().is_err() {
        return Ok(Outcome::Unreadable);
    }
    search_reader(
        &mut file,
        matcher,
        Some(options.code_page.encoding()),
        progress,
    )
    .map(|(outcome, _)| outcome)
}

/// An entry whose contents decide whether it matches, taken from the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub path: Vec<u8>,
    /// Indexed size and modification time. Cached answers validate live
    /// metadata because these may be omitted or out of date.
    pub size: u64,
    pub mtime_ns: i64,
    /// See [`crate::query::Stage::Contents`].
    pub table: u64,
}

/// The answers for a list of [`Pending`] entries, filled in by the workers
/// of [`decide_all`] and read by anyone meanwhile.
#[derive(Debug)]
pub struct Decisions {
    /// 0: not known yet, 1: no, 2: yes.
    states: Vec<AtomicU8>,
    matched: AtomicUsize,
    decided: AtomicUsize,
}

impl Decisions {
    pub fn new(len: usize) -> Self {
        Self {
            states: (0..len).map(|_| AtomicU8::new(0)).collect(),
            matched: AtomicUsize::new(0),
            decided: AtomicUsize::new(0),
        }
    }

    /// The answer for entry `i`, once it is known.
    pub fn get(&self, i: usize) -> Option<bool> {
        match self.states[i].load(Ordering::Acquire) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }

    /// How many entries matched so far.
    pub fn matched(&self) -> usize {
        self.matched.load(Ordering::Acquire)
    }

    /// How many entries are decided so far.
    pub fn decided(&self) -> usize {
        self.decided.load(Ordering::Acquire)
    }

    fn set(&self, i: usize, matched: bool) {
        if matched {
            self.matched.fetch_add(1, Ordering::AcqRel);
        }
        self.states[i].store(if matched { 2 } else { 1 }, Ordering::Release);
        self.decided.fetch_add(1, Ordering::AcqRel);
    }
}

/// Whether an entry matches, from its `table` over `terms` content terms,
/// asking `answer` for the answer of a term only while it still matters: a
/// term matters when some answers still possible for the others make the
/// result depend on it.
pub fn decide(
    table: u64,
    terms: usize,
    mut answer: impl FnMut(usize) -> Result<bool, Cancelled>,
) -> Result<bool, Cancelled> {
    let (mut known, mut bits) = (0u64, 0u64);
    loop {
        let open = || (0..1u64 << terms).filter(move |a| a & known == bits);
        if open().all(|a| table >> a & 1 == 1) {
            return Ok(true);
        }
        if open().all(|a| table >> a & 1 == 0) {
            return Ok(false);
        }
        let Some(term) = (0..terms).find(|&t| {
            let flip = 1u64 << t;
            known & flip == 0 && open().any(|a| table >> a & 1 != table >> (a ^ flip) & 1)
        }) else {
            // Unreachable: an undecided table depends on some open term.
            return Ok(false);
        };
        if answer(term)? {
            bits |= 1 << term;
        }
        known |= 1 << term;
    }
}

/// How often [`decide_all`] calls its `tick`.
pub const TICK_EVERY: Duration = Duration::from_millis(100);

/// Decides every entry of `pending` by reading its file for the content
/// `terms`, on `threads` threads in the order of the list, and records the
/// answers in `decisions` (made for this list) as they come. `progress`
/// counts the entries done; a cancel is seen before each buffer read. The
/// calling thread waits and calls `tick` about every [`TICK_EVERY`] meanwhile
/// (to show the matches so far). Answers are taken from `cache` and kept
/// there when one is given.
#[allow(clippy::too_many_arguments)]
pub fn decide_all(
    pending: &[Pending],
    terms: &[Arc<ContentTerm>],
    options: &ContentOptions,
    cache: Option<&Mutex<ContentCache>>,
    threads: usize,
    progress: &Progress,
    decisions: &Decisions,
    tick: impl FnMut(),
) -> Result<(), Cancelled> {
    decide_all_with_hashes(
        pending, terms, options, cache, None, threads, progress, decisions, tick,
    )
}

/// Like [`decide_all`], sharing digests with the table's optional hash columns.
#[allow(clippy::too_many_arguments)]
pub fn decide_all_with_hashes(
    pending: &[Pending],
    terms: &[Arc<ContentTerm>],
    options: &ContentOptions,
    cache: Option<&Mutex<ContentCache>>,
    hashes: Option<&Mutex<crate::hashcache::HashCache>>,
    threads: usize,
    progress: &Progress,
    decisions: &Decisions,
    mut tick: impl FnMut(),
) -> Result<(), Cancelled> {
    if let Some(cache) = cache {
        cache.lock().expect("content cache").use_options(options);
    }
    let next = AtomicUsize::new(0);
    let threads = threads.clamp(1, pending.len().max(1));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let done_tx = done_tx.clone();
            let next = &next;
            scope.spawn(move || {
                // Says it is done also when a read panics.
                let _done = Defer(Some(move || {
                    let _ = done_tx.send(());
                }));
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(entry) = pending.get(i) else { break };
                    if progress.is_cancelled() {
                        break;
                    }
                    let answer =
                        |t: usize| answer(entry, &terms[t], options, cache, hashes, progress);
                    match decide(entry.table, terms.len(), answer) {
                        Ok(matched) => decisions.set(i, matched),
                        Err(Cancelled) => break,
                    }
                    progress.add(1);
                }
            });
        }
        drop(done_tx);
        let mut running = threads;
        while running > 0 {
            match done_rx.recv_timeout(TICK_EVERY) {
                Ok(()) => running -= 1,
                Err(mpsc::RecvTimeoutError::Timeout) => tick(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    progress.check()
}

/// Runs a closure when dropped.
struct Defer<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// The answer of one term for one entry, from the cache or the file.
fn answer(
    entry: &Pending,
    term: &ContentTerm,
    options: &ContentOptions,
    cache: Option<&Mutex<ContentCache>>,
    hashes: Option<&Mutex<crate::hashcache::HashCache>>,
    progress: &Progress,
) -> Result<bool, Cancelled> {
    progress.check()?;
    let path = Path::new(std::ffi::OsStr::from_bytes(&entry.path));
    let key = live_key(path);
    if let (Some(cache), Some(key)) = (cache, key) {
        let found = cache.lock().expect("content cache").get(term, entry, key);
        if let Some(found) = found {
            progress.check()?;
            return Ok(found);
        }
    }
    let outcome = search_file_with_hashes(path, term, options, hashes, progress)?;
    // Unreadable or changing files must be tried again. Metadata comes
    // from disk even when details were omitted from the index or watching
    // is paused; replacement at the same path also invalidates an answer.
    match (cache, key) {
        (Some(cache), Some(key))
            if outcome != Outcome::Unreadable && live_key(path) == Some(key) =>
        {
            cache
                .lock()
                .expect("content cache")
                .put(term, entry, key, outcome.is_match());
        }
        _ => {}
    }
    Ok(outcome.is_match())
}

/// Maximum number of file answers a [`ContentCache`] keeps across all terms.
pub const CACHE_ENTRIES: usize = 200_000;
/// Most terms a [`ContentCache`] keeps answers for.
const CACHE_TERMS: usize = 8;

/// Answers of content terms for files, by the term's key and the file's
/// path and live file identity, size, modification and change times, so a search of the same
/// terms again (another order, file changes elsewhere) does not read the
/// files again. Answers for other [`ContentOptions`] are dropped. The terms
/// used longest ago are dropped first. At the shared [`CACHE_ENTRIES`] limit,
/// the oldest term is dropped.
/// A single term at the limit starts a fresh set of answers.
#[derive(Debug, Default)]
pub struct ContentCache {
    options: Option<ContentOptions>,
    /// Per term key, most recently used last.
    terms: VecDeque<(String, Answers)>,
}

/// File identity and content metadata, including change time for edits
/// that preserve the modification time.
type ContentKey = (crate::hash::FileKey, i64, i64);
/// Path → (live metadata, answer).
type Answers = HashMap<Vec<u8>, (ContentKey, bool)>;

fn live_key(path: &Path) -> Option<ContentKey> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.is_file().then(|| {
        (
            crate::hash::FileKey::of(&meta),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    })
}

impl ContentCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets every answer.
    pub fn clear(&mut self) {
        self.terms.clear();
    }

    /// Number of answers kept.
    pub fn len(&self) -> usize {
        self.terms.iter().map(|(_, m)| m.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn use_options(&mut self, options: &ContentOptions) {
        if self.options != Some(*options) {
            self.clear();
            self.options = Some(*options);
        }
    }

    fn get(&mut self, term: &ContentTerm, entry: &Pending, key: ContentKey) -> Option<bool> {
        let at = self.terms.iter().position(|(k, _)| k == term.key())?;
        let found = self.terms[at]
            .1
            .get(entry.path.as_slice())
            .and_then(|&(saved, matched)| (saved == key).then_some(matched));
        if at + 1 != self.terms.len() {
            let used = self.terms.remove(at)?;
            self.terms.push_back(used);
        }
        found
    }

    fn put(&mut self, term: &ContentTerm, entry: &Pending, key: ContentKey, matched: bool) {
        // Updating a live answer costs no extra entry and must not evict it.
        if let Some(answer) = self
            .terms
            .iter_mut()
            .find(|(k, _)| k == term.key())
            .and_then(|(_, answers)| answers.get_mut(entry.path.as_slice()))
        {
            *answer = (key, matched);
            return;
        }
        // The budget is shared by all terms: paths and metadata are copied
        // for each answer, so a limit per term would multiply memory use by
        // the number of terms.
        if self.len() >= CACHE_ENTRIES {
            self.terms.pop_front();
        }
        let at = match self.terms.iter().position(|(k, _)| k == term.key()) {
            Some(at) => at,
            None => {
                if self.terms.len() == CACHE_TERMS {
                    self.terms.pop_front();
                }
                self.terms
                    .push_back((term.key().to_owned(), HashMap::new()));
                self.terms.len() - 1
            }
        };
        let answers = &mut self.terms[at].1;
        answers.insert(entry.path.clone(), (key, matched));
    }
}

/// Whether the name of `path` ends with `.pdf`, in any case.
fn is_pdf_name(path: &Path) -> bool {
    let name = path.file_name().map_or(&b""[..], |n| n.as_bytes());
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".pdf")
}

/// Whether [`PDF_READER`] is an executable file in a folder of `PATH`.
pub fn pdf_reader_available() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join(PDF_READER))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Searches the text `pdftotext` prints for the PDF file at `path` of
/// `size` bytes. Over line ends the whole text is held at once, up to
/// [`MAX_LINE`] (a small PDF file can print much more): a longer text is
/// read again line by line, within the same [`PDF_TIMEOUT`].
fn search_pdf(
    path: &Path,
    term: &ContentTerm,
    options: &ContentOptions,
    size: u64,
    progress: &Progress,
) -> Result<Outcome, Cancelled> {
    let deadline = std::time::Instant::now() + PDF_TIMEOUT;
    let matcher = term.matcher(options, size);
    match read_pdf(path, matcher, deadline, progress)? {
        None if matcher.multi_line => {
            let lines = ContentOptions {
                multi_line: false,
                ..*options
            };
            let lines = term.matcher(&lines, size);
            Ok(read_pdf(path, lines, deadline, progress)?.unwrap_or(Outcome::Unreadable))
        }
        outcome => Ok(outcome.unwrap_or(Outcome::Unreadable)),
    }
}

/// Searches the text `pdftotext` prints for the PDF file at `path` with
/// `matcher`; `None` when the search failed but the reading did not (the
/// text was more than the search can hold). The child is killed when the
/// search ends early (a match, a cancel, the deadline) and always waited
/// for.
fn read_pdf(
    path: &Path,
    matcher: Matcher<'_>,
    deadline: std::time::Instant,
    progress: &Progress,
) -> Result<Option<Outcome>, Cancelled> {
    use std::process::Stdio;
    let child = pdf_reader(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return Ok(Some(Outcome::Unreadable));
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(Some(Outcome::Unreadable));
    };
    let nonblocking = rustix::fs::fcntl_getfl(&stdout)
        .and_then(|flags| rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK));
    let result = if nonblocking.is_ok() {
        let mut reader = Piped {
            inner: stdout,
            progress,
            deadline,
            timed_out: false,
        };
        let mut sink = Found::default();
        let read = searcher(None, matcher).search_reader(matcher.regex, &mut reader, &mut sink);
        Some((sink.matched, read.is_ok(), reader.timed_out))
    } else {
        None
    };
    let _ = child.kill();
    let status = child.wait();
    progress.check()?;
    Ok(match result {
        Some((true, _, _)) => Some(Outcome::Match),
        Some((false, true, false)) if status.is_ok_and(|s| s.success()) => Some(Outcome::NoMatch),
        Some((false, false, false)) => None,
        _ => Some(Outcome::Unreadable),
    })
}

/// The command that prints the text of the PDF file at `path`.
fn pdf_reader(path: &Path) -> std::process::Command {
    #[cfg(test)]
    if let Some(script) = tests::FAKE_PDF_READER.with(|f| f.borrow().clone()) {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(script);
        return command;
    }
    let mut command = std::process::Command::new(PDF_READER);
    command.args(["-q", "-enc", "UTF-8"]).arg(path).arg("-");
    command
}

/// The output of a child, read without blocking so a cancel or the
/// deadline is seen within a few milliseconds.
struct Piped<'a> {
    inner: std::process::ChildStdout,
    progress: &'a Progress,
    deadline: std::time::Instant,
    timed_out: bool,
}

impl Read for Piped<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.progress.is_cancelled() {
                return Err(io::Error::other("cancelled"));
            }
            if std::time::Instant::now() >= self.deadline {
                self.timed_out = true;
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            match self.inner.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                other => return other,
            }
        }
    }
}

/// Opens `path` for reading when it is a regular file, without following a
/// link and without waiting on a FIFO, and without changing its access time
/// when that is allowed (the file belongs to the user).
pub(crate) fn open_regular(path: &Path) -> Result<File, Outcome> {
    use rustix::fs::{Mode, OFlags};
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(Outcome::NotAFile),
        Err(_) => return Err(Outcome::Unreadable),
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::open(path, flags | OFlags::NOATIME, Mode::empty()) {
        Err(rustix::io::Errno::PERM) => rustix::fs::open(path, flags, Mode::empty()),
        other => other,
    };
    let file = match fd {
        Ok(fd) => File::from(fd),
        // Replaced by a link meanwhile.
        Err(rustix::io::Errno::LOOP) => return Err(Outcome::NotAFile),
        Err(_) => return Err(Outcome::Unreadable),
    };
    // It may have been replaced by something else meanwhile, too.
    match file.metadata() {
        Ok(meta) if meta.is_file() => Ok(file),
        Ok(_) => Err(Outcome::NotAFile),
        Err(_) => Err(Outcome::Unreadable),
    }
}

/// What a reader saw of the raw bytes.
#[derive(Default)]
struct Seen {
    /// It starts with a UTF-8 or UTF-16 byte order mark.
    bom: bool,
    utf8: Utf8Check,
}

/// Searches `reader` with `term` in `encoding` (`None`: raw bytes as UTF-8,
/// with a byte order mark deciding).
fn search_reader<R: Read>(
    reader: R,
    matcher: Matcher<'_>,
    encoding: Option<&'static encoding_rs::Encoding>,
    progress: &Progress,
) -> Result<(Outcome, Seen), Cancelled> {
    let mut searcher = searcher(encoding, matcher);
    let mut watched = Watched {
        inner: reader,
        progress,
        seen: Seen::default(),
        first: true,
    };
    let mut sink = Found::default();
    let result = searcher.search_reader(matcher.regex, &mut watched, &mut sink);
    progress.check()?;
    let outcome = if sink.matched {
        Outcome::Match
    } else if result.is_err() {
        Outcome::Unreadable
    } else if sink.binary {
        Outcome::Binary
    } else {
        Outcome::NoMatch
    };
    Ok((outcome, watched.seen))
}

/// A searcher for `matcher`, over line ends when it is built for that.
fn searcher(encoding: Option<&'static encoding_rs::Encoding>, matcher: Matcher<'_>) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .multi_line(matcher.multi_line)
        .line_number(false)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .heap_limit(Some(MAX_LINE))
        .bom_sniffing(encoding.is_none());
    if let Some(encoding) = encoding {
        // The labels of encoding_rs are the ones the searcher looks up.
        builder.encoding(Encoding::new(encoding.name()).ok());
    }
    builder.build()
}

/// Remembers the first match and stops there.
#[derive(Default)]
struct Found {
    matched: bool,
    binary: bool,
}

impl Sink for Found {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.matched = true;
        Ok(false)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

/// A reader that stops when the search is cancelled and looks at the raw
/// bytes on the way: a byte order mark, and whether they are UTF-8.
struct Watched<'a, R> {
    inner: R,
    progress: &'a Progress,
    seen: Seen,
    first: bool,
}

impl<R: Read> Read for Watched<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.progress.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        let bytes = &buf[..n];
        if std::mem::take(&mut self.first) {
            self.seen.bom = bytes.starts_with(b"\xEF\xBB\xBF")
                || bytes.starts_with(b"\xFF\xFE")
                || bytes.starts_with(b"\xFE\xFF");
        }
        self.seen.utf8.feed(bytes);
        Ok(n)
    }
}

/// Checks bytes that come in pieces for being UTF-8.
#[derive(Default)]
struct Utf8Check {
    invalid: bool,
    /// The start of a character cut at the end of the last piece.
    carry: [u8; 4],
    carry_len: usize,
}

impl Utf8Check {
    fn feed(&mut self, mut bytes: &[u8]) {
        if self.invalid {
            return;
        }
        if self.carry_len > 0 {
            let width = match self.carry[0] {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            let take = (width - self.carry_len).min(bytes.len());
            self.carry[self.carry_len..self.carry_len + take].copy_from_slice(&bytes[..take]);
            self.carry_len += take;
            bytes = &bytes[take..];
            match std::str::from_utf8(&self.carry[..self.carry_len]) {
                Ok(_) => self.carry_len = 0,
                Err(e) if e.error_len().is_some() => {
                    self.invalid = true;
                    return;
                }
                // Still cut short: the piece ended again.
                Err(_) => return,
            }
        }
        if let Err(e) = std::str::from_utf8(bytes) {
            if e.error_len().is_some() {
                self.invalid = true;
            } else {
                let rest = &bytes[e.valid_up_to()..];
                self.carry[..rest.len()].copy_from_slice(rest);
                self.carry_len = rest.len();
            }
        }
    }

    /// Whether all the bytes fed were UTF-8, ending on a whole character.
    fn is_valid(&self) -> bool {
        !self.invalid && self.carry_len == 0
    }
}

#[cfg(test)]
pub(crate) use tests::tiny_pdf as tests_pdf;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("everygrep-content-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn term(text: &str) -> ContentTerm {
        ContentTerm::new(text, false, false, false, ContentEncoding::Auto).unwrap()
    }

    fn outcome(path: &Path, term: &ContentTerm) -> Outcome {
        search_file(path, term, &ContentOptions::default(), &Progress::new()).unwrap()
    }

    fn utf16(text: &str, big_endian: bool, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let units = bom.then_some(0xFEFF).into_iter().chain(text.encode_utf16());
        for unit in units {
            let bytes = if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            };
            out.extend_from_slice(&bytes);
        }
        out
    }

    #[test]
    fn digest_terms_compare_raw_bytes_for_every_algorithm_and_share_hashes() {
        use crate::hash::Algorithm;
        use crate::hashcache::HashCache;
        let dir = Dir::new("digest-terms");
        // Its extension must not invoke document extraction for a digest query.
        let path = dir.file("raw.docx", b"abc");
        let other = dir.file("other.bin", b"abc\0");
        let cache = Mutex::new(HashCache::default());
        let options = ContentOptions {
            max_size: 1,
            documents: false,
            ..ContentOptions::default()
        };
        let vectors = [
            (Algorithm::Md5, "900150983cd24fb0d6963f7d28e17f72"),
            (Algorithm::Sha1, "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                Algorithm::Sha256,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                Algorithm::Sha384,
                "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
            ),
            (
                Algorithm::Sha512,
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            ),
            (Algorithm::Crc32, "352441c2"),
            (Algorithm::Xxh3, "06b05ab6733a618578af5f94892f3950"),
        ];
        for (algorithm, digest) in vectors {
            let term = ContentTerm::hash(algorithm, &digest.to_ascii_uppercase()).unwrap();
            assert_eq!(term.hash_algorithm(), Some(algorithm));
            assert_eq!(
                search_file_with_hashes(&path, &term, &options, Some(&cache), &Progress::new())
                    .unwrap(),
                Outcome::Match
            );
            assert_eq!(
                search_file(&other, &term, &options, &Progress::new()).unwrap(),
                Outcome::NoMatch
            );
            let known = cache
                .lock()
                .unwrap()
                .get(&crate::hash::open(&path).unwrap().key)
                .unwrap();
            assert_eq!(crate::hash::hex(&algorithm.bytes(&known).unwrap()), digest);
        }
        assert_eq!(cache.lock().unwrap().len(), 1);
        let term = ContentTerm::hash(Algorithm::Sha1, vectors[1].1).unwrap();
        let cancelled = Progress::new();
        cancelled.cancel();
        assert_eq!(
            search_file_with_hashes(&path, &term, &options, Some(&cache), &cancelled),
            Err(Cancelled)
        );
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(
            search_file(&link, &term, &options, &Progress::new()).unwrap(),
            Outcome::NotAFile
        );
        assert_eq!(
            search_file(&dir.0, &term, &options, &Progress::new()).unwrap(),
            Outcome::NotAFile
        );
    }

    #[test]
    fn digest_terms_take_cached_values_invalidate_changed_files_and_work_without_cache() {
        use crate::hash::Algorithm;
        use crate::hashcache::{Cached, HashCache};
        let dir = Dir::new("digest-cache");
        let path = dir.file("abc", b"abc");
        let cache = Mutex::new(HashCache::default());
        // A distinct sentinel proves a cache hit: reading abc again would differ.
        cache.lock().unwrap().put(
            crate::hash::open(&path).unwrap().key,
            &Cached {
                sha1: Some([0; 20]),
                ..Cached::default()
            },
        );
        let sentinel = ContentTerm::hash(Algorithm::Sha1, &"0".repeat(40)).unwrap();
        assert_eq!(
            search_file_with_hashes(
                &path,
                &sentinel,
                &ContentOptions::default(),
                Some(&cache),
                &Progress::new()
            )
            .unwrap(),
            Outcome::Match
        );
        std::fs::write(&path, b"changed").unwrap();
        assert_eq!(
            search_file_with_hashes(
                &path,
                &sentinel,
                &ContentOptions::default(),
                Some(&cache),
                &Progress::new()
            )
            .unwrap(),
            Outcome::NoMatch
        );
        let path = dir.file("uncached", b"abc");
        let disabled = Mutex::new(HashCache::new(0));
        let abc =
            ContentTerm::hash(Algorithm::Sha1, "a9993e364706816aba3e25717850c26c9cd0d89d").unwrap();
        assert_eq!(
            search_file_with_hashes(
                &path,
                &abc,
                &ContentOptions::default(),
                Some(&disabled),
                &Progress::new()
            )
            .unwrap(),
            Outcome::Match
        );
        assert!(disabled.lock().unwrap().is_empty());
    }

    #[test]
    fn digest_queries_compose_with_text_boolean_logic_and_skip_offline_entries() {
        use crate::index::Index;
        use crate::query::{EvalScratch, Query, QueryOptions};
        use crate::store::{Meta, NodeStore};
        let dir = Dir::new("digest-query");
        for (name, bytes) in [
            ("a.txt", b"abc".as_slice()),
            ("b.txt", b"hay"),
            ("c.txt", b"other"),
            ("abc.docx", b"abc"),
        ] {
            dir.file(name, bytes);
        }
        let mut store = NodeStore::new();
        let root = store.add(None, dir.0.as_os_str().as_bytes(), true, Meta::default());
        for name in [b"a.txt".as_slice(), b"b.txt", b"c.txt", b"abc.docx"] {
            store.add(Some(root), name, false, Meta::default());
        }
        let index = Index::new(store);
        let query = Query::parse(
            "file: <sha1:a9993e364706816aba3e25717850c26c9cd0d89d | content:hay>",
            QueryOptions::default(),
        )
        .unwrap();
        assert!(query.reads_files());
        let mut names: Vec<_> = crate::search::search(&index, &query, 2)
            .iter()
            .map(|&id| String::from_utf8_lossy(index.store().name(id)).into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["a.txt", "abc.docx", "b.txt"]);
        let mut scratch = EvalScratch::with_offline_roots(&[root]);
        for id in index.order().iter() {
            assert!(!query.matches(index.store(), id, &mut scratch));
        }
        let literal = Query::parse("content:sha1:abc", QueryOptions::default()).unwrap();
        assert!(literal.content_terms()[0].hash_algorithm().is_none());
    }

    #[test]
    fn type_and_attribute_queries_use_categories_and_live_link_metadata() {
        use crate::query::{EvalScratch, Query, QueryOptions};
        use crate::store::{Meta, NodeStore};
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("value-metadata");
        let hidden = dir.file(".hidden", b"x");
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o400)).unwrap();
        let plain = dir.file("plain.txt", b"x");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&hidden, dir.0.join("link")).unwrap();
        let mut store = NodeStore::new();
        let root = store.add(None, dir.0.as_os_str().as_bytes(), true, Meta::default());
        let hidden_id = store.add(Some(root), b".hidden", false, Meta::default());
        let plain_id = store.add(Some(root), b"plain.txt", false, Meta::default());
        let link = store.add(Some(root), b"link", false, Meta::default());
        let source = store.add(Some(root), b"CMakeLists.txt", false, Meta::default());
        let matches = |text: &str, id| {
            Query::parse(text, QueryOptions::default())
                .unwrap()
                .matches(&store, id, &mut EvalScratch::default())
        };
        assert!(matches("attributes:=RH", hidden_id));
        assert!(matches("attrib:r", hidden_id));
        assert!(!matches("attributes:=R", hidden_id));
        assert!(matches("attributes:=", plain_id));
        assert!(matches("attributes:=L", link));
        assert!(
            !matches("attributes:R", link),
            "link metadata does not follow the target"
        );
        assert!(matches("attributes:D", root));
        assert!(matches("type:text ext:txt", plain_id));
        assert!(!matches("type:text ext:txt", source));
        assert!(matches("type:code ext:txt", source));
        let query = Query::parse("attributes:=RH", QueryOptions::default()).unwrap();
        assert!(!query.matches(
            &store,
            hidden_id,
            &mut EvalScratch::with_offline_roots(&[root])
        ));
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(matches("attributes:=R", plain_id));
    }

    #[test]
    fn plain_text_matches_ignoring_case_and_by_word_and_expression() {
        let dir = Dir::new("plain");
        let notes = dir.file(
            "notes.txt",
            "first line\nThe Quick brown fox\nlast\n".as_bytes(),
        );
        assert_eq!(outcome(&notes, &term("quick BROWN")), Outcome::Match);
        assert_eq!(outcome(&notes, &term("slow")), Outcome::NoMatch);
        let case = ContentTerm::new("quick", false, true, false, ContentEncoding::Auto).unwrap();
        assert_eq!(outcome(&notes, &case), Outcome::NoMatch);
        let word = ContentTerm::new("bro", false, false, true, ContentEncoding::Auto).unwrap();
        assert_eq!(outcome(&notes, &word), Outcome::NoMatch);
        let word = ContentTerm::new("brown", false, false, true, ContentEncoding::Auto).unwrap();
        assert_eq!(outcome(&notes, &word), Outcome::Match);
        // `^` matches at the start of every line; literal text is not an
        // expression.
        let re = ContentTerm::new("^the q", true, false, false, ContentEncoding::Auto).unwrap();
        assert_eq!(outcome(&notes, &re), Outcome::Match);
        assert_eq!(outcome(&notes, &term("^the")), Outcome::NoMatch);
        assert!(ContentTerm::new("(", true, false, false, ContentEncoding::Auto).is_err());
        // Diacritics are matched as written.
        let az = dir.file("az.txt", "Şəhər mərkəzi\n".as_bytes());
        assert_eq!(outcome(&az, &term("şəhər")), Outcome::Match);
        assert_eq!(outcome(&az, &term("seher")), Outcome::NoMatch);
    }

    #[test]
    fn matches_run_over_line_ends_unless_turned_off() {
        let dir = Dir::new("lines");
        let text = "first line\r\n   The Quick\tbrown fox\nlast\n\nend\n";
        let notes = dir.file("notes.txt", text.as_bytes());
        let lines = ContentOptions {
            multi_line: false,
            ..ContentOptions::default()
        };
        let read = |t: &ContentTerm, o: &ContentOptions| {
            search_file(&notes, t, o, &Progress::new()).unwrap()
        };
        let regex =
            |p: &str| ContentTerm::new(p, true, false, false, ContentEncoding::Auto).unwrap();
        let word =
            |p: &str| ContentTerm::new(p, false, false, true, ContentEncoding::Auto).unwrap();
        let both = |t: &ContentTerm| (read(t, &ContentOptions::default()), read(t, &lines));
        use Outcome::{Match, NoMatch};
        // A space of the text matches a line end with the spaces around it.
        assert_eq!(both(&term("line the quick")), (Match, NoMatch));
        assert_eq!(both(&term("fox last")), (Match, NoMatch));
        assert_eq!(both(&word("line the")), (Match, NoMatch));
        // The spaces written still match as they are, and nothing else.
        assert_eq!(both(&term("quick\tbrown")), (Match, Match));
        assert_eq!(both(&term("quick brown")), (NoMatch, NoMatch));
        // An empty line between is a paragraph, not a line end.
        assert_eq!(both(&term("last end")), (NoMatch, NoMatch));
        // Expressions: `\n` and `\s` match line ends, `.` does not, and
        // `^`/`$` stay per line.
        assert_eq!(both(&regex(r"line\s+the")), (Match, NoMatch));
        assert_eq!(both(&regex(r"fox\nlast")), (Match, NoMatch));
        // Line by line, the parts of an expression without a line break
        // still match; the ones with it cannot.
        assert_eq!(both(&regex(r"brown|fox\nlast")), (Match, Match));
        assert_eq!(both(&regex(r"nothing|fox\nlast")), (Match, NoMatch));
        assert_eq!(both(&regex(r"fox(\nx)?")), (Match, Match));
        assert_eq!(both(&regex(r"last\n?$")), (Match, Match));
        assert_eq!(both(&regex("fox.last")), (NoMatch, NoMatch));
        assert_eq!(both(&regex("^last$")), (Match, Match));
        assert_eq!(both(&regex(r"^\s*the quick")), (Match, Match));
        // Words without a space are found the same way either way.
        assert_eq!(both(&term("quick")), (Match, Match));
        // A file over the limit is searched line by line.
        let mut big = vec![b'x'; usize::try_from(MULTI_LINE_MAX).unwrap()];
        big.extend_from_slice(b"\nalpha\nbeta\n");
        let big = dir.file("big.txt", &big);
        let either = regex(r"alpha|alpha\nbeta");
        assert_eq!(
            search_file(&big, &either, &ContentOptions::default(), &Progress::new()).unwrap(),
            Match
        );
        let t = term("alpha beta");
        let read_big = |o: &ContentOptions| search_file(&big, &t, o, &Progress::new()).unwrap();
        assert_eq!(read_big(&ContentOptions::default()), NoMatch);
        assert_eq!(
            read_big(&ContentOptions {
                max_size: 0,
                ..lines
            }),
            NoMatch
        );
        let small = dir.file("small.txt", b"alpha\nbeta\n");
        assert_eq!(
            search_file(&small, &t, &ContentOptions::default(), &Progress::new()).unwrap(),
            Match
        );
    }

    #[test]
    fn quoted_literal_line_breaks_match_only_with_multi_line_enabled() {
        use crate::query::{Query, QueryOptions};
        let dir = Dir::new("quoted-line-breaks");
        for (i, line_break) in ["\n", "\r\n"].into_iter().enumerate() {
            let literal = format!("foo{line_break}bar");
            let query =
                Query::parse(&format!("content:\"{literal}\""), QueryOptions::default()).unwrap();
            let term = &query.content_terms()[0];
            let path = dir.file(&format!("notes-{i}.txt"), literal.as_bytes());
            assert_eq!(
                search_file(&path, term, &ContentOptions::default(), &Progress::new()).unwrap(),
                Outcome::Match
            );
            let lines = ContentOptions {
                multi_line: false,
                ..ContentOptions::default()
            };
            assert_eq!(
                search_file(&path, term, &lines, &Progress::new()).unwrap(),
                Outcome::NoMatch
            );
        }
    }

    #[test]
    fn encodings_by_byte_order_mark_and_legacy_code_page() {
        let dir = Dir::new("encodings");
        let le = dir.file("le.txt", &utf16("Bakı şəhəri\n", false, true));
        let be = dir.file("be.txt", &utf16("Bakı şəhəri\n", true, true));
        let bare = dir.file("bare.txt", &utf16("Bakı şəhəri\n", false, false));
        let t = term("şəhəri");
        assert_eq!(outcome(&le, &t), Outcome::Match);
        assert_eq!(outcome(&be, &t), Outcome::Match);
        // Without a mark, UTF-16 is binary unless asked for.
        assert_eq!(outcome(&bare, &t), Outcome::Binary);
        let forced =
            ContentTerm::new("şəhəri", false, false, false, ContentEncoding::Utf16Le).unwrap();
        assert_eq!(outcome(&bare, &forced), Outcome::Match);
        let wrong =
            ContentTerm::new("şəhəri", false, false, false, ContentEncoding::Utf16Be).unwrap();
        assert_eq!(outcome(&bare, &wrong), Outcome::NoMatch);

        // "ışık üzüm" in Windows-1254: not UTF-8, read in the code page.
        let turkish: &[u8] = b"\xFD\xFE\xFDk \xFCz\xFCm\n";
        let legacy = dir.file("legacy.txt", turkish);
        assert_eq!(outcome(&legacy, &term("ışık")), Outcome::Match);
        assert_eq!(outcome(&legacy, &term("üzüm")), Outcome::Match);
        let ansi = ContentTerm::new("üzüm", false, false, false, ContentEncoding::Ansi).unwrap();
        assert_eq!(outcome(&legacy, &ansi), Outcome::Match);
        let utf8 = ContentTerm::new("üzüm", false, false, false, ContentEncoding::Utf8).unwrap();
        assert_eq!(outcome(&legacy, &utf8), Outcome::NoMatch);
        // Another code page reads the same bytes as other letters.
        let western = ContentOptions {
            code_page: CodePage::Windows1252,
            ..ContentOptions::default()
        };
        let other = search_file(&legacy, &term("ışık"), &western, &Progress::new()).unwrap();
        assert_eq!(other, Outcome::NoMatch);
        let thorn = search_file(&legacy, &term("ýþýk"), &western, &Progress::new()).unwrap();
        assert_eq!(thorn, Outcome::Match);
        // A valid UTF-8 file is not read again in the code page.
        let utf8_file = dir.file("utf8.txt", "\u{fe}\u{fd}\n".as_bytes());
        assert_eq!(outcome(&utf8_file, &term("şı")), Outcome::NoMatch);
    }

    #[test]
    fn binary_large_and_special_files_are_skipped() {
        let dir = Dir::new("skipped");
        let mut binary = b"\x7fELF\0\0\0".to_vec();
        binary.extend_from_slice(b"needle\n");
        let binary = dir.file("tool", &binary);
        assert_eq!(outcome(&binary, &term("needle")), Outcome::Binary);
        let big = dir.file(
            "big.log",
            &[b"x".repeat(2000), b"\nneedle\n".to_vec()].concat(),
        );
        let small = ContentOptions {
            max_size: 1000,
            ..ContentOptions::default()
        };
        let read = |options: &ContentOptions| {
            search_file(&big, &term("needle"), options, &Progress::new()).unwrap()
        };
        assert_eq!(read(&small), Outcome::TooLarge);
        let unlimited = ContentOptions {
            max_size: 0,
            ..small
        };
        assert_eq!(read(&unlimited), Outcome::Match);

        let target = dir.file("target.txt", b"needle\n");
        let link = dir.0.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(outcome(&link, &term("needle")), Outcome::NotAFile);
        assert_eq!(outcome(&dir.0, &term("needle")), Outcome::NotAFile);
        let fifo = dir.0.join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            // Opening a FIFO for reading would wait for a writer.
            assert_eq!(outcome(&fifo, &term("needle")), Outcome::NotAFile);
        }
        assert_eq!(
            outcome(&dir.0.join("gone.txt"), &term("needle")),
            Outcome::Unreadable
        );
    }

    #[test]
    fn a_cancelled_search_stops_before_reading_on() {
        let dir = Dir::new("cancel");
        let file = dir.file("a.txt", b"needle\n");
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(
            search_file(
                &file,
                &term("needle"),
                &ContentOptions::default(),
                &progress
            ),
            Err(Cancelled)
        );
        // A reader that is cancelled while it reads stops at its next read.
        let progress = Progress::new();
        struct Endless<'a>(&'a Progress, usize);
        impl Read for Endless<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.1 += 1;
                if self.1 == 3 {
                    self.0.cancel();
                }
                buf.fill(b'x');
                buf[buf.len() - 1] = b'\n';
                Ok(buf.len())
            }
        }
        let mut endless = Endless(&progress, 0);
        let needle = term("needle");
        let matcher = needle.matcher(&ContentOptions::default(), 0);
        let result = search_reader(&mut endless, matcher, None, &progress);
        assert_eq!(result.map(|(o, _)| o), Err(Cancelled));
        assert_eq!(endless.1, 3);
    }

    #[test]
    fn utf8_check_follows_characters_cut_between_pieces() {
        let text = "aş€😀b".as_bytes();
        for cut in 0..text.len() {
            for second in cut..text.len() {
                let mut check = Utf8Check::default();
                check.feed(&text[..cut]);
                check.feed(&text[cut..second]);
                check.feed(&text[second..]);
                assert!(check.is_valid(), "cut at {cut} and {second}");
            }
        }
        let mut check = Utf8Check::default();
        check.feed(b"a\xC5");
        assert!(!check.is_valid(), "ends inside a character");
        check.feed(b"\x9F");
        assert!(check.is_valid());
        check.feed(b"\xFE");
        assert!(!check.is_valid());
        let mut check = Utf8Check::default();
        check.feed(b"\xE2\x82");
        check.feed(b"x");
        assert!(!check.is_valid(), "a cut character that does not go on");
    }

    #[test]
    fn code_pages_have_stable_ids() {
        for page in CodePage::ALL {
            assert_eq!(CodePage::from_id(page.id()), Some(page));
            assert!(Encoding::new(page.encoding().name()).is_ok());
        }
        assert_eq!(CodePage::default().id(), "windows-1254");
        assert_eq!(CodePage::from_id("utf-8"), None);
    }

    #[test]
    fn decide_asks_only_for_terms_that_still_matter() {
        // a | b: table over (a, b) bits is true unless both are false.
        let or = 0b1110;
        let mut asked = Vec::new();
        let yes = decide(or, 2, |t| {
            asked.push(t);
            Ok(true)
        });
        assert_eq!((yes, asked.as_slice()), (Ok(true), &[0][..]));
        let mut asked = Vec::new();
        let no = decide(or, 2, |t| {
            asked.push(t);
            Ok(false)
        });
        assert_eq!((no, asked.as_slice()), (Ok(false), &[0, 1][..]));
        // A table that does not depend on term 0 (name decided its branch)
        // never asks for it: here the answer is term 1.
        let only_b = 0b1100;
        let mut asked = Vec::new();
        assert_eq!(
            decide(only_b, 2, |t| {
                asked.push(t);
                Ok(t == 1)
            }),
            Ok(true)
        );
        assert_eq!(asked, [1]);
        // !a
        assert_eq!(decide(0b01, 1, |_| Ok(false)), Ok(true));
        assert_eq!(decide(0b01, 1, |_| Err(Cancelled)), Err(Cancelled));
    }

    #[test]
    fn the_cache_budget_is_shared_by_terms_and_updates_keep_answers() {
        let dir = Dir::new("cache-budget");
        let path = dir.file("a.txt", b"text");
        let key = live_key(&path).unwrap();
        let mut cache = ContentCache::new();
        let terms = [term("one"), term("two"), term("three")];
        let mut entry = Pending {
            path: Vec::new(),
            size: 4,
            mtime_ns: 0,
            table: 0b10,
        };
        for term in &terms {
            for i in 0..=CACHE_ENTRIES / 2 {
                entry.path = format!("/scratch/{i}").into_bytes();
                cache.put(term, &entry, key, true);
            }
            assert!(cache.len() <= CACHE_ENTRIES);
        }
        assert_eq!(cache.len(), CACHE_ENTRIES / 2 + 1);
        assert!(cache.get(&terms[0], &entry, key).is_none());
        assert!(cache.get(&terms[1], &entry, key).is_none());
        assert_eq!(cache.get(&terms[2], &entry, key), Some(true));
        let kept = cache.len();
        cache.put(&terms[2], &entry, key, false);
        assert_eq!(cache.len(), kept);
        assert_eq!(cache.get(&terms[2], &entry, key), Some(false));
        // Small terms still share the cache; the oldest of nine is evicted.
        cache.clear();
        for i in 0..=CACHE_TERMS {
            cache.put(&term(&i.to_string()), &entry, key, true);
        }
        assert_eq!(cache.len(), CACHE_TERMS);
        assert_eq!(cache.get(&term("0"), &entry, key), None);
        assert_eq!(cache.get(&term("8"), &entry, key), Some(true));
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn the_cache_answers_again_until_the_file_or_the_options_change() {
        let dir = Dir::new("cache");
        let path = dir.file("a.txt", b"needle\n");
        let entry = Pending {
            path: path.as_os_str().as_bytes().to_vec(),
            size: 7,
            mtime_ns: 1,
            table: 0b10,
        };
        let terms = [Arc::new(term("needle"))];
        let cache = Mutex::new(ContentCache::new());
        let run = |entry: &Pending, options: &ContentOptions| {
            let decisions = Decisions::new(1);
            decide_all(
                std::slice::from_ref(entry),
                &terms,
                options,
                Some(&cache),
                2,
                &Progress::new(),
                &decisions,
                || {},
            )
            .unwrap();
            decisions.get(0).unwrap()
        };
        let options = ContentOptions::default();
        assert!(run(&entry, &options));
        assert_eq!(cache.lock().unwrap().len(), 1);
        // Disk changes invalidate the answer even before the index sees them.
        std::fs::write(&path, b"other\n").unwrap();
        assert!(!run(&entry, &options));
        // Once the index has the new time, the file is read again.
        let changed = Pending {
            mtime_ns: 2,
            ..entry.clone()
        };
        assert!(!run(&changed, &options));
        assert_eq!(cache.lock().unwrap().len(), 1);
        // Other options read every file again.
        std::fs::write(&path, b"needle\n").unwrap();
        let other = ContentOptions {
            max_size: 0,
            ..options
        };
        assert!(run(&changed, &other));
        // Files that cannot be read are not kept.
        let gone = Pending {
            path: dir.0.join("gone").as_os_str().as_bytes().to_vec(),
            ..entry.clone()
        };
        cache.lock().unwrap().clear();
        assert!(!run(&gone, &other));
        assert!(cache.lock().unwrap().is_empty());
    }

    #[test]
    fn decide_all_calls_tick_while_it_waits_and_records_every_answer() {
        let dir = Dir::new("decide-all");
        let pending: Vec<Pending> = (0..20)
            .map(|i| {
                let text = if i % 3 == 0 { "needle\n" } else { "hay\n" };
                let path = dir.file(&format!("{i}.txt"), text.as_bytes());
                Pending {
                    path: path.as_os_str().as_bytes().to_vec(),
                    size: 0,
                    mtime_ns: 0,
                    table: 0b10,
                }
            })
            .collect();
        let decisions = Decisions::new(pending.len());
        let progress = Progress::new();
        decide_all(
            &pending,
            &[Arc::new(term("needle"))],
            &ContentOptions::default(),
            None,
            3,
            &progress,
            &decisions,
            || {},
        )
        .unwrap();
        assert_eq!(decisions.decided(), 20);
        assert_eq!(decisions.matched(), 7);
        assert_eq!(progress.done(), 20);
        assert!((0..20).all(|i| decisions.get(i) == Some(i % 3 == 0)));
        // Cancelled before it starts: nothing is decided.
        let progress = Progress::new();
        progress.cancel();
        let decisions = Decisions::new(pending.len());
        let result = decide_all(
            &pending,
            &[Arc::new(term("needle"))],
            &ContentOptions::default(),
            None,
            3,
            &progress,
            &decisions,
            || {},
        );
        assert_eq!((result, decisions.decided()), (Err(Cancelled), 0));
    }

    /// A one-page PDF file with `text` in Helvetica, written as a hex
    /// string, so the raw file does not hold the text.
    pub(crate) fn tiny_pdf(text: &str) -> Vec<u8> {
        let hex: String = text.bytes().map(|b| format!("{b:02X}")).collect();
        let stream = format!("BT /F1 18 Tf 20 100 Td <{hex}> Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 144] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut out = "%PDF-1.4\n".to_owned();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
        }
        let xref = out.len();
        out.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for offset in offsets {
            out.push_str(&format!("{offset:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));
        out.into_bytes()
    }

    #[test]
    fn a_pdf_pipe_times_out_with_continuous_output_as_well_as_silence() {
        use std::process::{Command, Stdio};
        for continuous in [true, false] {
            let mut command = Command::new(if continuous { "yes" } else { "sleep" });
            if !continuous {
                command.arg("5");
            }
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdout = child.stdout.take().unwrap();
            // Ensure output is already available before setting an expired deadline.
            if continuous {
                stdout.read_exact(&mut [0; 1]).unwrap();
            }
            let flags = rustix::fs::fcntl_getfl(&stdout).unwrap();
            rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
            let progress = Progress::new();
            let mut reader = Piped {
                inner: stdout,
                progress: &progress,
                deadline: std::time::Instant::now(),
                timed_out: false,
            };
            let read = reader.read(&mut [0; 4096]);
            let _ = child.kill();
            child.wait().unwrap();
            assert_eq!(
                read.unwrap_err().kind(),
                io::ErrorKind::TimedOut,
                "continuous={continuous}"
            );
            assert!(reader.timed_out);
        }
    }

    #[test]
    fn pdf_files_are_read_through_the_reader_only_when_asked() {
        if !pdf_reader_available() {
            eprintln!("{PDF_READER} is not installed; skipped");
            return;
        }
        let dir = Dir::new("pdf");
        let pdf = dir.file("Report.PDF", &tiny_pdf("Quarterly needle report"));
        let with_pdf = ContentOptions {
            pdf: true,
            ..ContentOptions::default()
        };
        let read = |options: &ContentOptions, text: &str| {
            search_file(&pdf, &term(text), options, &Progress::new()).unwrap()
        };
        assert_eq!(read(&with_pdf, "needle report"), Outcome::Match);
        assert_eq!(read(&with_pdf, "missing"), Outcome::NoMatch);
        // Off: the raw file, where the text is not written as such.
        assert_eq!(
            read(&ContentOptions::default(), "needle report"),
            Outcome::NoMatch
        );
        // Not a PDF inside: the reader fails.
        let broken = dir.file("broken.pdf", b"not a pdf at all\n");
        let outcome = search_file(&broken, &term("pdf"), &with_pdf, &Progress::new()).unwrap();
        assert_eq!(outcome, Outcome::Unreadable);
        // Cancelled: the child is stopped.
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(
            search_file(&pdf, &term("needle"), &with_pdf, &progress),
            Err(Cancelled)
        );
    }

    thread_local! {
        /// A shell script run instead of [`PDF_READER`] in this thread.
        pub(super) static FAKE_PDF_READER: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn a_pdf_text_too_long_to_hold_is_searched_again_line_by_line() {
        let dir = Dir::new("pdf-long");
        let pdf = dir.file("small.pdf", b"%PDF-1.4\n");
        let text = format!(
            "printf 'needle phrase\\n'; yes filler | head -c {}",
            MAX_LINE + 4096
        );
        FAKE_PDF_READER.with(|f| *f.borrow_mut() = Some(text));
        let with_pdf = ContentOptions {
            pdf: true,
            ..ContentOptions::default()
        };
        let read = |t: &str| search_file(&pdf, &term(t), &with_pdf, &Progress::new()).unwrap();
        assert_eq!(read("needle phrase"), Outcome::Match);
        FAKE_PDF_READER.with(|f| *f.borrow_mut() = None);
    }

    #[test]
    fn pdf_files_are_known_by_their_name() {
        assert!(is_pdf_name(Path::new("/a/b.pdf")));
        assert!(is_pdf_name(Path::new("/a/B.Pdf")));
        assert!(!is_pdf_name(Path::new("/a/.pdf")));
        assert!(!is_pdf_name(Path::new("/a/pdf")));
        assert!(!is_pdf_name(Path::new("/a/b.pdf.txt")));
    }
}
