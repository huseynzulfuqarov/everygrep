//! Bounded, read-only text extraction from ZIP-based documents.
//!
//! No archive path is written to disk. Every entry is counted and verified,
//! including images, before any extracted text can be searched. A malformed
//! archive or XML part invalidates the whole document, even after useful text.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::time::{Duration, Instant};

use quick_xml::Reader;
use quick_xml::events::Event;
use rawzip::{CompressionMethod, ReaderAt, ZipLocator};

use crate::progress::Progress;

pub const MAX_ENTRIES: u64 = 10_000;
pub const MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const HARD_MAX_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_RATIO: u64 = 200;
pub const MAX_XML_DEPTH: usize = 128;
pub const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Word,
    Sheet,
    Slides,
    OpenDocument,
    Epub,
}

impl Kind {
    pub fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        [
            ("docx", Self::Word),
            ("xlsx", Self::Sheet),
            ("pptx", Self::Slides),
            ("odt", Self::OpenDocument),
            ("ods", Self::OpenDocument),
            ("odp", Self::OpenDocument),
            ("epub", Self::Epub),
        ]
        .into_iter()
        .find_map(|(e, kind)| ext.eq_ignore_ascii_case(e).then_some(kind))
    }

    fn selects(self, name: &str) -> bool {
        match self {
            Self::Word => {
                matches!(
                    name,
                    "word/document.xml"
                        | "word/footnotes.xml"
                        | "word/endnotes.xml"
                        | "word/comments.xml"
                ) || numbered_part(name, "word/header", ".xml")
                    || numbered_part(name, "word/footer", ".xml")
            }
            Self::Sheet => {
                name == "xl/sharedStrings.xml" || numbered_part(name, "xl/worksheets/sheet", ".xml")
            }
            Self::Slides => {
                numbered_part(name, "ppt/slides/slide", ".xml")
                    || numbered_part(name, "ppt/notesSlides/notesSlide", ".xml")
            }
            Self::OpenDocument => matches!(name, "content.xml" | "styles.xml"),
            Self::Epub => ["xhtml", "html", "htm"].iter().any(|ext| {
                name.rsplit_once('.')
                    .is_some_and(|(_, e)| e.eq_ignore_ascii_case(ext))
            }),
        }
    }
}

fn numbered_part(name: &str, prefix: &str, suffix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(suffix))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    Limit,
    Invalid,
}

struct Budget<'a> {
    progress: &'a Progress,
    deadline: Instant,
    max_bytes: u64,
}

impl Budget<'_> {
    fn check(&self) -> Result<(), Error> {
        if self.progress.is_cancelled() {
            Err(Error::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(Error::Limit)
        } else {
            Ok(())
        }
    }

    fn append(&self, out: &mut String, text: &str) -> Result<(), Error> {
        self.check()?;
        if (out.len() as u64).saturating_add(text.len() as u64) > self.max_bytes {
            return Err(Error::Limit);
        }
        out.push_str(text);
        Ok(())
    }
}

struct WatchedFile<'a> {
    file: File,
    budget: &'a Budget<'a>,
}

impl ReaderAt for WatchedFile<'_> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.budget
            .check()
            .map_err(|_| std::io::Error::other("document read stopped"))?;
        self.file.read_at(buf, offset)
    }
}

/// `file` must already be opened as a regular file without following links.
/// Limits count both all expanded entries and the final UTF-8 text.
pub fn extract(
    file: File,
    kind: Kind,
    max_bytes: u64,
    progress: &Progress,
) -> Result<String, Error> {
    let budget = Budget {
        progress,
        deadline: Instant::now() + TIMEOUT,
        max_bytes: max_bytes.min(HARD_MAX_BYTES),
    };
    let result = extract_bounded(file, kind, &budget);
    // Preserve cancellation/deadline errors even when a ZIP read reported I/O.
    budget.check()?;
    result
}

fn extract_bounded(file: File, kind: Kind, budget: &Budget<'_>) -> Result<String, Error> {
    budget.check()?;
    let meta = file.metadata().map_err(|_| Error::Invalid)?;
    if meta.len() > HARD_MAX_BYTES || budget.max_bytes == 0 {
        return Err(Error::Limit);
    }
    let snapshot = (
        crate::hash::FileKey::of(&meta),
        meta.ctime(),
        meta.ctime_nsec(),
    );
    let mut buffer = vec![0; rawzip::RECOMMENDED_BUFFER_SIZE];
    let archive = ZipLocator::new()
        .locate_in_reader(WatchedFile { file, budget }, &mut buffer, meta.len())
        .map_err(|_| Error::Invalid)?;
    if archive.entries_hint() > MAX_ENTRIES {
        return Err(Error::Limit);
    }
    let mut entries = archive.entries(&mut buffer);
    let mut count = 0u64;
    let mut declared_total = 0u64;
    let mut actual_total = 0u64;
    let mut names = HashSet::new();
    let mut ranges = BTreeMap::new();
    let mut parts = Vec::new();
    while let Some(entry) = entries.next_entry().map_err(|_| Error::Invalid)? {
        budget.check()?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(Error::Limit);
        }
        let raw_path = entry.file_path();
        let name = std::str::from_utf8(raw_path.as_ref()).map_err(|_| Error::Invalid)?;
        if name.len() > 1024 {
            return Err(Error::Limit);
        }
        if !names.insert(name.to_owned()) || entry.flags().is_encrypted() {
            return Err(Error::Invalid);
        }
        let size = entry.uncompressed_size_hint();
        let compressed = entry.compressed_size_hint();
        if size > MAX_ENTRY_BYTES
            || compressed > MAX_ENTRY_BYTES
            || size > compressed.saturating_mul(MAX_RATIO)
        {
            return Err(Error::Limit);
        }
        declared_total = declared_total.checked_add(size).ok_or(Error::Limit)?;
        if declared_total > budget.max_bytes {
            return Err(Error::Limit);
        }
        let local = archive
            .get_entry(entry.wayfinder())
            .map_err(|_| Error::Invalid)?;
        let (start, end) = local.compressed_data_range();
        if end > archive.directory_offset()
            || end < start
            || ranges
                .range(..=start)
                .next_back()
                .is_some_and(|(_, &end)| end > start)
            || ranges
                .range(start..)
                .next()
                .is_some_and(|(&next, _)| next < end)
        {
            return Err(Error::Invalid);
        }
        ranges.insert(start, end);
        let selected = kind.selects(name);
        let xml = selected
            || [".xml", ".rels", ".opf"]
                .iter()
                .any(|ext| name.ends_with(ext));
        let mut bytes = Vec::new();
        let mut read = |reader: &mut dyn Read| -> Result<(), Error> {
            let mut expanded = 0u64;
            let mut chunk = [0; 32 * 1024];
            loop {
                budget.check()?;
                let n = reader.read(&mut chunk).map_err(|_| Error::Invalid)?;
                if n == 0 {
                    break;
                }
                expanded += n as u64;
                actual_total += n as u64;
                if expanded > MAX_ENTRY_BYTES
                    || expanded > compressed.saturating_mul(MAX_RATIO)
                    || actual_total > budget.max_bytes
                {
                    return Err(Error::Limit);
                }
                if xml {
                    bytes.extend_from_slice(&chunk[..n]);
                }
            }
            if expanded != size {
                return Err(Error::Invalid);
            }
            Ok(())
        };
        match entry.compression_method() {
            CompressionMethod::STORE => read(&mut local.verifying_reader(local.reader()))?,
            CompressionMethod::DEFLATE => {
                let mut decoder = flate2::read::DeflateDecoder::new(local.reader());
                read(&mut local.verifying_reader(&mut decoder))?;
                if decoder.total_in() != compressed {
                    return Err(Error::Invalid);
                }
            }
            _ => return Err(Error::Invalid),
        }
        if xml {
            parts.push((name.to_owned(), bytes));
        }
    }
    let main = match kind {
        Kind::Word => names.contains("word/document.xml"),
        Kind::Sheet => names
            .iter()
            .any(|n| numbered_part(n, "xl/worksheets/sheet", ".xml")),
        Kind::Slides => names
            .iter()
            .any(|n| numbered_part(n, "ppt/slides/slide", ".xml")),
        Kind::OpenDocument => names.contains("content.xml"),
        Kind::Epub => names.iter().any(|n| kind.selects(n)),
    };
    if count != archive.entries_hint() || !main {
        return Err(Error::Invalid);
    }
    let meta = archive
        .get_ref()
        .file
        .metadata()
        .map_err(|_| Error::Invalid)?;
    if snapshot
        != (
            crate::hash::FileKey::of(&meta),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    {
        return Err(Error::Invalid);
    }
    let mut out = String::new();
    let mut shared = Vec::new();
    // Shared strings are resolved at each cell's position, regardless of ZIP order.
    if kind == Kind::Sheet
        && let Some((_, bytes)) = parts.iter().find(|(n, _)| n == "xl/sharedStrings.xml")
    {
        parse_xml(bytes, Mode::Shared, &mut out, &mut shared, budget)?;
    }
    for (name, bytes) in parts {
        budget.check()?;
        let mode = if !kind.selects(&name) {
            Mode::Validate
        } else {
            match kind {
                Kind::Word | Kind::Slides => Mode::Runs,
                Kind::OpenDocument => Mode::Paragraphs,
                Kind::Epub => Mode::Html,
                Kind::Sheet if name == "xl/sharedStrings.xml" => continue,
                Kind::Sheet => Mode::Cells,
            }
        };
        parse_xml(&bytes, mode, &mut out, &mut shared, budget)?;
        budget.append(&mut out, "\n")?;
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Validate,
    Runs,
    Paragraphs,
    Html,
    Shared,
    Cells,
}

fn xml_text(bytes: &[u8]) -> Result<std::borrow::Cow<'_, str>, Error> {
    let encoding = if bytes.starts_with(&[0xff, 0xfe]) {
        Some(encoding_rs::UTF_16LE)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Some(encoding_rs::UTF_16BE)
    } else {
        None
    };
    if let Some(encoding) = encoding {
        encoding
            .decode_without_bom_handling_and_without_replacement(&bytes[2..])
            .ok_or(Error::Invalid)
    } else {
        std::str::from_utf8(bytes)
            .map(std::borrow::Cow::Borrowed)
            .map_err(|_| Error::Invalid)
    }
}

fn block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "tr"
            | "section"
            | "blockquote"
            | "br"
    )
}

fn parse_xml(
    bytes: &[u8],
    mode: Mode,
    out: &mut String,
    shared: &mut Vec<String>,
    budget: &Budget<'_>,
) -> Result<(), Error> {
    let text = xml_text(bytes)?;
    let mut reader = Reader::from_str(&text);
    reader.config_mut().check_comments = true;
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<String> = Vec::new();
    let mut roots = 0;
    let mut declaration = false;
    let mut cell_type = String::new();
    let mut cell = String::new();
    let mut shared_bytes = shared.iter().map(String::len).sum::<usize>();
    loop {
        budget.check()?;
        match reader.read_event().map_err(|_| Error::Invalid)? {
            Event::Start(start) | Event::Empty(start) => {
                // The reader expands empty elements into a start and an end.
                let name = start.local_name().as_ref().to_owned();
                if stack.is_empty() {
                    roots += 1;
                    if roots != 1 {
                        return Err(Error::Invalid);
                    }
                }
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(Error::Limit);
                }
                // Validate all attributes, even on elements whose text is ignored.
                let mut spaces = 1;
                for attr in start.attributes() {
                    let attr = attr.map_err(|_| Error::Invalid)?;
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|_| Error::Invalid)?;
                    if mode == Mode::Cells && name == "c" && attr.key.local_name().as_ref() == "t" {
                        cell_type = value.into_owned();
                    } else if mode == Mode::Paragraphs
                        && name == "s"
                        && attr.key.local_name().as_ref() == "c"
                    {
                        spaces = value.parse::<usize>().map_err(|_| Error::Invalid)?;
                    }
                }
                if (mode == Mode::Cells && name == "c") || (mode == Mode::Shared && name == "si") {
                    cell.clear();
                }
                if mode == Mode::Html && block(&name) {
                    budget.append(out, "\n")?;
                }
                let in_paragraph = stack.iter().any(|n| matches!(n.as_str(), "p" | "h"));
                if mode == Mode::Paragraphs && in_paragraph && name == "s" {
                    if spaces as u64 > budget.max_bytes {
                        return Err(Error::Limit);
                    }
                    budget.append(out, &" ".repeat(spaces))?;
                } else if (mode == Mode::Runs && name == "tab")
                    || (mode == Mode::Paragraphs && in_paragraph && name == "tab")
                {
                    budget.append(out, "\t")?;
                } else if (mode == Mode::Runs && matches!(name.as_str(), "br" | "cr"))
                    || (mode == Mode::Paragraphs && in_paragraph && name == "line-break")
                {
                    budget.append(out, "\n")?;
                }
                stack.push(name);
            }
            Event::End(end) => {
                let name = end.local_name();
                if stack.pop().as_deref() != Some(name.as_ref()) {
                    return Err(Error::Invalid);
                }
                if mode == Mode::Cells && name.as_ref() == "c" {
                    if cell_type == "s" {
                        let index = cell.trim().parse::<usize>().map_err(|_| Error::Invalid)?;
                        budget.append(out, shared.get(index).ok_or(Error::Invalid)?)?;
                    } else {
                        budget.append(out, &cell)?;
                    }
                    budget.append(out, "\n")?;
                    cell_type.clear();
                } else if mode == Mode::Shared && name.as_ref() == "si" {
                    shared_bytes = shared_bytes.checked_add(cell.len()).ok_or(Error::Limit)?;
                    if shared.len() >= 1_000_000 || shared_bytes as u64 > budget.max_bytes {
                        return Err(Error::Limit);
                    }
                    shared.push(std::mem::take(&mut cell));
                } else if (matches!(mode, Mode::Runs | Mode::Paragraphs)
                    && matches!(name.as_ref(), "p" | "h"))
                    || (mode == Mode::Html && block(name.as_ref()))
                {
                    budget.append(out, "\n")?;
                }
            }
            Event::Text(t) => {
                append_text(&t.xml10_content(), mode, &stack, out, &mut cell, budget)?
            }
            Event::CData(t) => {
                append_text(&t.xml10_content(), mode, &stack, out, &mut cell, budget)?
            }
            Event::GeneralRef(r) => {
                let value = if let Some(c) = r.resolve_char_ref().map_err(|_| Error::Invalid)? {
                    c.to_string()
                } else {
                    match r.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "apos" => "'",
                        "quot" => "\"",
                        _ => return Err(Error::Invalid),
                    }
                    .to_owned()
                };
                append_text(&value, mode, &stack, out, &mut cell, budget)?;
            }
            Event::DocType(_) => return Err(Error::Invalid),
            Event::Decl(d) => {
                if !stack.is_empty() || roots != 0 || declaration {
                    return Err(Error::Invalid);
                }
                declaration = true;
                if let Some(encoding) = d.encoding() {
                    let encoding = encoding.map_err(|_| Error::Invalid)?;
                    if !["utf-8", "utf-16", "utf-16le", "utf-16be"]
                        .iter()
                        .any(|s| encoding.eq_ignore_ascii_case(s))
                    {
                        return Err(Error::Invalid);
                    }
                }
            }
            Event::Eof => {
                return if stack.is_empty() && roots == 1 {
                    Ok(())
                } else {
                    Err(Error::Invalid)
                };
            }
            Event::Comment(_) | Event::PI(_) => {}
        }
    }
}

fn append_text(
    text: &str,
    mode: Mode,
    stack: &[String],
    out: &mut String,
    cell: &mut String,
    budget: &Budget<'_>,
) -> Result<(), Error> {
    if stack.is_empty() && !text.trim().is_empty() {
        return Err(Error::Invalid);
    }
    if text.chars().any(|c| matches!(c, '\0'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')) { return Err(Error::Invalid); }
    let has = |name: &str| stack.iter().any(|n| n == name);
    let selected = match mode {
        Mode::Validate => false,
        Mode::Runs | Mode::Shared => has("t"),
        Mode::Paragraphs => has("p") || has("h"),
        Mode::Html => has("body") && !has("script") && !has("style"),
        Mode::Cells => has("c") && (has("t") || has("v")),
    };
    if selected {
        budget.append(
            if matches!(mode, Mode::Shared | Mode::Cells) {
                cell
            } else {
                out
            },
            text,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentEncoding, ContentOptions, ContentTerm, Outcome, search_file};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "everygrep-document-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn zip(parts: &[(&str, &[u8])], deflated: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut archive = rawzip::ZipArchiveWriter::new(&mut bytes);
        for &(name, content) in parts {
            let method = if deflated && name != "mimetype" {
                CompressionMethod::DEFLATE
            } else {
                CompressionMethod::STORE
            };
            let (mut entry, config) = archive
                .new_file(name)
                .compression_method(method)
                .start()
                .unwrap();
            if method == CompressionMethod::DEFLATE {
                let encoder =
                    flate2::write::DeflateEncoder::new(&mut entry, flate2::Compression::default());
                let mut writer = config.wrap(encoder);
                writer.write_all(content).unwrap();
                let (encoder, descriptor) = writer.finish().unwrap();
                encoder.finish().unwrap();
                entry.finish(descriptor).unwrap();
            } else {
                let mut writer = config.wrap(&mut entry);
                writer.write_all(content).unwrap();
                let (_, descriptor) = writer.finish().unwrap();
                entry.finish(descriptor).unwrap();
            }
        }
        archive.finish().unwrap();
        bytes
    }

    const TYPES: &[u8] = br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#;
    const WORD: &[u8] = br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>world &amp; &#x41;</w:t></w:r><w:tab/><w:r><w:t>needle</w:t></w:r></w:p><w:p><w:r><w:t>second paragraph</w:t></w:r></w:p></w:body></w:document>"#;

    fn found(path: &Path, text: &str, options: &ContentOptions) -> bool {
        let term = ContentTerm::new(text, false, false, false, ContentEncoding::Auto).unwrap();
        search_file(path, &term, options, &Progress::new())
            .unwrap()
            .is_match()
    }

    fn extracted(path: &Path) -> Result<String, Error> {
        extract(
            crate::content::open_regular(path).unwrap(),
            Kind::of(path).unwrap(),
            DEFAULT_MAX_BYTES,
            &Progress::new(),
        )
    }

    #[test]
    fn docx_and_slides_join_runs_separate_paragraphs_and_decode_entities() {
        let dir = Dir::new();
        let options = ContentOptions::default();
        for compressed in [false, true] {
            let word = dir.write("small.docx", &zip(&[("[Content_Types].xml", TYPES), ("word/document.xml", WORD), ("word/header1.xml", br#"<w:hdr xmlns:w="urn:word"><w:p><w:r><w:t>Heading</w:t></w:r></w:p></w:hdr>"#)], compressed));
            assert!(found(&word, "Hello world & A", &options));
            assert!(found(&word, "needle second paragraph", &options));
            assert!(found(&word, "Heading", &options));
            assert!(!found(&word, "wordprocessingml", &options));
            assert!(!found(
                &word,
                "needle second paragraph",
                &ContentOptions {
                    multi_line: false,
                    ..options
                }
            ));
            let slides = dir.write("small.pptx", &zip(&[("[Content_Types].xml", TYPES), ("ppt/slides/slide1.xml", br#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><a:p><a:r><a:t>slide needle</a:t></a:r></a:p></p:cSld></p:sld>"#), ("ppt/notesSlides/notesSlide1.xml", br#"<p:notes xmlns:p="urn:p" xmlns:a="urn:a"><a:p><a:r><a:t>speaker notes</a:t></a:r></a:p></p:notes>"#)], compressed));
            assert!(found(&slides, "slide needle", &options));
            assert!(found(&slides, "speaker notes", &options));
        }
    }

    #[test]
    fn xlsx_resolves_shared_strings_at_cells_and_ignores_unused_strings() {
        let dir = Dir::new();
        let sheet = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c t="s"><v>1</v></c><c t="s"><v>0</v></c><c t="inlineStr"><is><r><t>inline </t></r><r><t>needle</t></r></is></c><c><v>12345</v></c></row></sheetData></worksheet>"#;
        let shared = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>world</t></si><si><r><t>Hello</t></r></si><si><t>unused decoy</t></si></sst>"#;
        let path = dir.write(
            "small.xlsx",
            &zip(
                &[
                    ("[Content_Types].xml", TYPES),
                    ("xl/worksheets/sheet1.xml", sheet),
                    ("xl/sharedStrings.xml", shared),
                ],
                true,
            ),
        );
        let options = ContentOptions::default();
        assert!(found(&path, "Hello world", &options));
        assert!(found(&path, "inline needle", &options));
        assert!(found(&path, "12345", &options));
        assert!(!found(&path, "unused decoy", &options));
        let bad = dir.write(
            "bad.xlsx",
            &zip(
                &[
                    (
                        "xl/worksheets/sheet1.xml",
                        br#"<worksheet><c t="s"><v>9999</v></c></worksheet>"#,
                    ),
                    ("xl/sharedStrings.xml", shared),
                ],
                false,
            ),
        );
        assert_eq!(extracted(&bad), Err(Error::Invalid));
    }

    #[test]
    fn opendocument_formats_and_epub_search_visible_unicode_text() {
        let dir = Dir::new();
        let text = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:office" xmlns:text="urn:text"><office:body><text:p>Sənəd <text:span>Türkiyə</text:span><text:s text:c="2"/>needle<text:tab/>here<text:line-break/>next</text:p></office:body></office:document-content>"#;
        for ext in ["odt", "ods", "odp"] {
            let path = dir.write(
                &format!("small.{ext}"),
                &zip(
                    &[
                        ("mimetype", b"application/vnd.oasis.opendocument.text"),
                        ("content.xml", text.as_bytes()),
                    ],
                    false,
                ),
            );
            assert!(found(&path, "Sənəd Türkiyə", &ContentOptions::default()));
            assert!(extracted(&path).unwrap().contains("needle\there\nnext"));
        }
        let book = dir.write("book.EPUB", &zip(&[("mimetype", b"application/epub+zip"), ("META-INF/container.xml", br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#), ("OEBPS/content.opf", br#"<package><manifest><item id="c" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c"/></spine></package>"#), ("OEBPS/chapter.xhtml", "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>hidden title</title><style>hidden style</style></head><body><p>Sənəd <em>Türkiyə</em> &amp; &#x41;</p><script>hidden script</script><p>next<br/>line</p></body></html>".as_bytes())], true));
        assert!(found(
            &book,
            "Sənəd Türkiyə & A",
            &ContentOptions::default()
        ));
        for hidden in ["hidden title", "hidden style", "hidden script"] {
            assert!(!found(&book, hidden, &ContentOptions::default()));
        }
    }

    #[test]
    fn utf16_xml_uses_its_own_encoding_and_off_switch_never_searches_zip_bytes() {
        let dir = Dir::new();
        let text = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><w:document xmlns:w=\"urn:word\"><w:p><w:t>Sənəd needle</w:t></w:p></w:document>";
        let bytes: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let path = dir.write("utf16.docx", &zip(&[("word/document.xml", &bytes)], false));
        assert!(found(&path, "Sənəd needle", &ContentOptions::default()));
        let path = dir.write("stored.docx", &zip(&[("word/document.xml", WORD)], false));
        assert!(!found(
            &path,
            "needle",
            &ContentOptions {
                documents: false,
                ..ContentOptions::default()
            }
        ));
        assert!(!found(
            &path,
            "needle",
            &ContentOptions {
                document_max_bytes: 10,
                ..ContentOptions::default()
            }
        ));
        assert!(!found(
            &path,
            "needle",
            &ContentOptions {
                max_size: 10,
                ..ContentOptions::default()
            }
        ));
    }

    #[test]
    fn bad_zip_crc_xml_and_entities_invalidate_even_an_earlier_match() {
        let dir = Dir::new();
        for (name, bytes) in [
            ("empty.docx", b"".as_slice()),
            ("bad.docx", b"PK\x03\x04broken"),
        ] {
            let path = dir.write(name, bytes);
            assert!(!found(&path, "needle", &ContentOptions::default()));
        }
        for xml in [
            "<document><t>needle</t>",
            "<document><t>needle</t></document>garbage",
            "<document><t>needle</t></document><second/>",
            "<document><t>needle</t><a x=\"1\" x=\"2\"/></document>",
            "<document><t>needle &unknown;</t></document>",
            "<!DOCTYPE document [<!ENTITY x SYSTEM 'file:///no-such-file'>]><document><t>needle &x;</t></document>",
            "<document><t>needle</t><!-- bad -- comment --></document>",
        ] {
            let path = dir.write(
                "bad-xml.docx",
                &zip(&[("word/document.xml", xml.as_bytes())], false),
            );
            assert!(!found(&path, "needle", &ContentOptions::default()), "{xml}");
        }
        let mut bytes = zip(&[("word/document.xml", WORD)], false);
        let offset = {
            let archive = rawzip::ZipArchive::from_slice(&bytes).unwrap();
            archive
                .entries()
                .next_entry()
                .unwrap()
                .unwrap()
                .central_directory_offset() as usize
        };
        bytes[offset + 16] ^= 1;
        let path = dir.write("bad-crc.docx", &bytes);
        assert_eq!(extracted(&path), Err(Error::Invalid));
        let path = dir.write(
            "duplicate.docx",
            &zip(
                &[("word/document.xml", WORD), ("word/document.xml", WORD)],
                false,
            ),
        );
        assert_eq!(extracted(&path), Err(Error::Invalid));
    }

    #[test]
    fn zip_bombs_and_forged_sizes_stop_before_returning_any_text() {
        let dir = Dir::new();
        let mut bomb = WORD.to_vec();
        bomb.extend(std::iter::repeat_n(b' ', 2 * 1024 * 1024));
        let mut bytes = zip(&[("word/document.xml", &bomb)], true);
        let path = dir.write("bomb.docx", &bytes);
        assert_eq!(extracted(&path), Err(Error::Limit));
        let offset = {
            let archive = rawzip::ZipArchive::from_slice(&bytes).unwrap();
            archive
                .entries()
                .next_entry()
                .unwrap()
                .unwrap()
                .central_directory_offset() as usize
        };
        bytes[offset + 24..offset + 28].copy_from_slice(&1u32.to_le_bytes());
        let path = dir.write("forged.docx", &bytes);
        assert!(extracted(&path).is_err());
        let mut bytes = zip(&[("word/document.xml", WORD)], false);
        let offset = {
            let archive = rawzip::ZipArchive::from_slice(&bytes).unwrap();
            archive
                .entries()
                .next_entry()
                .unwrap()
                .unwrap()
                .central_directory_offset() as usize
        };
        bytes[offset + 24..offset + 28]
            .copy_from_slice(&(MAX_ENTRY_BYTES as u32 + 1).to_le_bytes());
        let path = dir.write("huge-entry.docx", &bytes);
        assert_eq!(extracted(&path), Err(Error::Limit));
        let entries: Vec<_> = (0..MAX_ENTRIES + 1)
            .map(|i| (format!("entry{i}"), Vec::new()))
            .collect();
        let parts: Vec<_> = entries
            .iter()
            .map(|(n, bytes)| (n.as_str(), bytes.as_slice()))
            .collect();
        let path = dir.write("many.docx", &zip(&parts, false));
        assert_eq!(extracted(&path), Err(Error::Limit));
        let path = dir.write(
            "total.docx",
            &zip(
                &[("word/document.xml", WORD), ("image.bin", &[42; 1000])],
                false,
            ),
        );
        assert_eq!(
            extract(
                crate::content::open_regular(&path).unwrap(),
                Kind::Word,
                1100,
                &Progress::new()
            ),
            Err(Error::Limit)
        );
    }

    #[test]
    fn xml_depth_output_limits_timeout_and_cancel_are_enforced() {
        let dir = Dir::new();
        let deep = format!(
            "{}<t>needle</t>{}",
            "<node>".repeat(MAX_XML_DEPTH),
            "</node>".repeat(MAX_XML_DEPTH)
        );
        let path = dir.write(
            "deep.docx",
            &zip(&[("word/document.xml", deep.as_bytes())], false),
        );
        assert_eq!(extracted(&path), Err(Error::Limit));
        let path = dir.write(
            "spaces.odt",
            &zip(
                &[(
                    "content.xml",
                    br#"<document><p>needle<s c="999999999"/></p></document>"#,
                )],
                false,
            ),
        );
        assert_eq!(extracted(&path), Err(Error::Limit));
        let path = dir.write("normal.docx", &zip(&[("word/document.xml", WORD)], false));
        let progress = Progress::new();
        let budget = Budget {
            progress: &progress,
            deadline: Instant::now() - Duration::from_millis(1),
            max_bytes: DEFAULT_MAX_BYTES,
        };
        assert_eq!(
            extract_bounded(
                crate::content::open_regular(&path).unwrap(),
                Kind::Word,
                &budget
            ),
            Err(Error::Limit)
        );
        progress.cancel();
        assert_eq!(
            extracted_with_progress(&path, &progress),
            Err(Error::Cancelled)
        );
        let term = ContentTerm::new("needle", false, false, false, ContentEncoding::Auto).unwrap();
        assert_eq!(
            search_file(&path, &term, &ContentOptions::default(), &progress),
            Err(crate::progress::Cancelled)
        );
    }

    fn extracted_with_progress(path: &Path, progress: &Progress) -> Result<String, Error> {
        extract(
            crate::content::open_regular(path).unwrap(),
            Kind::of(path).unwrap(),
            DEFAULT_MAX_BYTES,
            progress,
        )
    }

    #[test]
    fn content_cache_changes_with_document_switch_and_expanded_limit() {
        use crate::content::{ContentCache, Decisions, Pending, decide_all};
        use std::os::unix::ffi::OsStrExt;
        use std::sync::{Arc, Mutex};
        let dir = Dir::new();
        let path = dir.write("cache.docx", &zip(&[("word/document.xml", WORD)], false));
        let entry = Pending {
            path: path.as_os_str().as_bytes().to_vec(),
            size: 0,
            mtime_ns: 0,
            table: 0b10,
        };
        let terms = [Arc::new(
            ContentTerm::new("needle", false, false, false, ContentEncoding::Auto).unwrap(),
        )];
        let cache = Mutex::new(ContentCache::new());
        let run = |options: &ContentOptions| {
            let decisions = Decisions::new(1);
            decide_all(
                std::slice::from_ref(&entry),
                &terms,
                options,
                Some(&cache),
                1,
                &Progress::new(),
                &decisions,
                || {},
            )
            .unwrap();
            decisions.get(0)
        };
        let on = ContentOptions::default();
        assert_eq!(run(&on), Some(true));
        assert_eq!(
            run(&ContentOptions {
                documents: false,
                ..on
            }),
            Some(false)
        );
        assert_eq!(run(&on), Some(true));
        assert_eq!(
            run(&ContentOptions {
                document_max_bytes: 1,
                ..on
            }),
            Some(false)
        );
        assert_eq!(run(&on), Some(true));
    }

    #[test]
    fn overlaps_encryption_unsupported_methods_and_broken_metadata_are_refused() {
        let dir = Dir::new();
        let path = dir.write(
            "metadata.docx",
            &zip(
                &[("word/document.xml", WORD), ("_rels/.rels", b"<broken>")],
                false,
            ),
        );
        assert!(!found(&path, "needle", &ContentOptions::default()));
        let bytes = zip(
            &[("word/document.xml", WORD), ("word/header1.xml", WORD)],
            false,
        );
        let offsets = {
            let archive = rawzip::ZipArchive::from_slice(&bytes).unwrap();
            let mut entries = archive.entries();
            let first = entries
                .next_entry()
                .unwrap()
                .unwrap()
                .central_directory_offset() as usize;
            let second = entries
                .next_entry()
                .unwrap()
                .unwrap()
                .central_directory_offset() as usize;
            [first, second]
        };
        let mut overlap = bytes.clone();
        overlap[offsets[1] + 42..offsets[1] + 46].copy_from_slice(&0u32.to_le_bytes());
        let path = dir.write("overlap.docx", &overlap);
        assert_eq!(extracted(&path), Err(Error::Invalid));
        let mut encrypted = bytes.clone();
        encrypted[offsets[0] + 8] |= 1;
        let path = dir.write("encrypted.docx", &encrypted);
        assert_eq!(extracted(&path), Err(Error::Invalid));
        let mut unknown = bytes;
        unknown[offsets[0] + 10..offsets[0] + 12].copy_from_slice(&99u16.to_le_bytes());
        let path = dir.write("unsupported.docx", &unknown);
        assert_eq!(extracted(&path), Err(Error::Invalid));
    }

    #[test]
    fn arbitrary_corrupt_archives_do_not_panic_or_match() {
        let dir = Dir::new();
        let valid = zip(&[("word/document.xml", WORD)], true);
        for cut in (0..valid.len()).step_by(17) {
            let path = dir.write("cut.docx", &valid[..cut]);
            assert!(!found(&path, "needle", &ContentOptions::default()));
        }
        let mut seed = 13u64;
        for len in [1, 19, 64, 1024, 65_600] {
            let bytes: Vec<_> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 32) as u8
                })
                .collect();
            let path = dir.write("random.docx", &bytes);
            assert_eq!(
                search_file(
                    &path,
                    &ContentTerm::new("needle", false, false, false, ContentEncoding::Auto)
                        .unwrap(),
                    &ContentOptions::default(),
                    &Progress::new()
                )
                .unwrap(),
                Outcome::Unreadable
            );
        }
    }
}
