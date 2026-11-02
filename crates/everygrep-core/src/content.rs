//! Searching the contents of files, for the `content:` terms of a query.
//!
//! The index keeps names only, so contents are read from disk when a query
//! asks for them. A [`ContentTerm`] is a text or regular expression and the
//! encoding to read files in; [`search_file`] tells whether one file holds
//! it. Files are read in buffers of 64 KiB with the line searcher of ripgrep
//! (`grep-searcher`), which stops at the first match, so a match near the
//! start of a large file costs one buffer.
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
//!   ([`CodePage`], Windows-1254 by default).
//!
//! Matching works line by line, so a phrase does not match across a line
//! break; `^` and `$` match at the start and end of every line. Case is
//! ignored unless the term asks for it (Unicode simple case folding).
//! Diacritics are matched as written: ignoring them is a mode for names only.
//!
//! The access time of a file is not changed when the file belongs to the
//! user (`O_NOATIME`).

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::Path;

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Encoding, Searcher, SearcherBuilder, Sink, SinkMatch};

use crate::progress::{Cancelled, Progress};

/// The largest file read by default: 100 MiB.
pub const DEFAULT_MAX_SIZE: u64 = 100 * 1024 * 1024;

/// Longest line kept in memory while searching a file (a minified file can
/// be one line); a file with a longer line is not searched further.
const MAX_LINE: usize = 64 * 1024 * 1024;

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

    fn encoding(self) -> &'static encoding_rs::Encoding {
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
}

impl Default for ContentOptions {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_SIZE,
            code_page: CodePage::default(),
        }
    }
}

/// A text looked for in the contents of files.
#[derive(Clone)]
pub struct ContentTerm {
    matcher: RegexMatcher,
    encoding: ContentEncoding,
    /// A literal text of ASCII characters only: the legacy code pages are
    /// ASCII supersets, so a file that did not match as UTF-8 cannot match
    /// in one.
    ascii_literal: bool,
    key: String,
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
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(!case)
            .multi_line(true)
            .line_terminator(Some(b'\n'))
            .word(whole_word)
            .build(&pattern)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            matcher,
            encoding,
            ascii_literal: !regex && text.is_ascii(),
            key: format!(
                "{encoding:?}/{}/{}/{}/{text}",
                u8::from(regex),
                u8::from(case),
                u8::from(whole_word)
            ),
        })
    }

    /// Identifies what the term matches: equal keys match the same files.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn encoding(&self) -> ContentEncoding {
        self.encoding
    }
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
    /// than 64 MiB).
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
    progress.check()?;
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
    let forced = match term.encoding {
        ContentEncoding::Auto => None,
        ContentEncoding::Utf8 => Some(encoding_rs::UTF_8),
        ContentEncoding::Utf16Le => Some(encoding_rs::UTF_16LE),
        ContentEncoding::Utf16Be => Some(encoding_rs::UTF_16BE),
        ContentEncoding::Ansi => Some(options.code_page.encoding()),
    };
    if forced.is_some() {
        return search_reader(&mut file, term, forced, progress).map(|(outcome, _)| outcome);
    }
    let (outcome, seen) = search_reader(&mut file, term, None, progress)?;
    if outcome != Outcome::NoMatch || seen.bom || seen.utf8.is_valid() || term.ascii_literal {
        return Ok(outcome);
    }
    // Not UTF-8: read it again in the legacy code page.
    if file.rewind().is_err() {
        return Ok(Outcome::Unreadable);
    }
    search_reader(
        &mut file,
        term,
        Some(options.code_page.encoding()),
        progress,
    )
    .map(|(outcome, _)| outcome)
}

/// Opens `path` for reading when it is a regular file, without following a
/// link and without waiting on a FIFO, and without changing its access time
/// when that is allowed (the file belongs to the user).
fn open_regular(path: &Path) -> Result<File, Outcome> {
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
    term: &ContentTerm,
    encoding: Option<&'static encoding_rs::Encoding>,
    progress: &Progress,
) -> Result<(Outcome, Seen), Cancelled> {
    let mut searcher = searcher(encoding);
    let mut watched = Watched {
        inner: reader,
        progress,
        seen: Seen::default(),
        first: true,
    };
    let mut sink = Found::default();
    let result = searcher.search_reader(&term.matcher, &mut watched, &mut sink);
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

fn searcher(encoding: Option<&'static encoding_rs::Encoding>) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
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
    fn plain_text_matches_ignoring_case_and_by_word_and_expression() {
        let dir = Dir::new("plain");
        let notes = dir.file(
            "notes.txt",
            "first line\nThe Quick brown fox\nlast\n".as_bytes(),
        );
        assert_eq!(outcome(&notes, &term("quick BROWN")), Outcome::Match);
        assert_eq!(outcome(&notes, &term("slow")), Outcome::NoMatch);
        // A phrase does not match across a line break.
        assert_eq!(outcome(&notes, &term("line the")), Outcome::NoMatch);
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
        let result = search_reader(&mut endless, &term("needle"), None, &progress);
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
}
