//! Exporting the result list to a file.
//!
//! The formats:
//!
//! - **CSV** (RFC 4180, original name bytes, `\r\n` line ends): a header line
//!   `Name,Folder,Size,Modified,Offline`, then one line per result: the name,
//!   the full path of its folder, the size in bytes (for a folder the total
//!   size of the files below it), the local modification time as
//!   `YYYY-MM-DD HH:MM:SS` (empty when unknown) and `yes` for an entry of a
//!   drive that is not connected. Fields with a comma, a quote or a line
//!   break are quoted, quotes doubled.
//! - **TSV** (original name bytes, `\n` line ends): the same columns separated by tabs. A
//!   backslash, line feed, carriage return or tab in a field is written as
//!   `\\`, `\n`, `\r` or `\t`, so every result is one line.
//! - **Text**: one full path per line (`\n`); an entry of a drive that is not
//!   connected is followed by a tab and `(offline)`. Paths are escaped as in
//!   TSV.
//! - **M3U8** (an extended playlist): `#EXTM3U`, then one full path per
//!   line, using the original file-name bytes. A playlist line cannot hold
//!   a line break, so a path with one is left out.
//! - **JSON**: an array with one object per result, `{"name", "folder",
//!   "size", "modified", "offline"}`; `modified` is the local time as above,
//!   or `null` when unknown.
//! - **HTML**: a page with a table of the CSV columns, for reading in a web
//!   browser or pasting into a spreadsheet.
//!
//! JSON and HTML replace invalid UTF-8 with U+FFFD. Other formats retain
//! original bytes, with only their documented delimiter escaping.
//! The file is written to a
//! temporary file in the same folder, synced and renamed over the target, so
//! the target holds either its old contents or the whole export.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use everygrep_core::localtime::Zone;

/// The file formats of an export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Text,
    M3u8,
    Json,
    Html,
}

impl Format {
    /// Every format, in the order of the export options.
    pub const ALL: [Format; 6] = [
        Format::Csv,
        Format::Tsv,
        Format::Text,
        Format::M3u8,
        Format::Json,
        Format::Html,
    ];

    /// The extension a file of this format gets, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Text => "txt",
            Format::M3u8 => "m3u8",
            Format::Json => "json",
            Format::Html => "html",
        }
    }

    /// The format a file name asks for by its extension (`.csv`, `.tsv`,
    /// `.txt`, `.m3u8`, `.json`, `.html`, any case).
    pub fn of_path(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Format::ALL.into_iter().find(|f| f.extension() == ext)
    }
}

/// A typed known extension chooses the format. Always write to the exact
/// path accepted by the portal, whose overwrite prompt covers that path.
pub fn saved_format(path: &Path, chosen: Format) -> Format {
    Format::of_path(path).unwrap_or(chosen)
}

/// The frozen result scope shown in the export options.
pub struct Selection<T> {
    all: std::sync::Arc<Vec<T>>,
    selected: Vec<T>,
    pub only_selected: bool,
}
impl<T> Selection<T> {
    pub fn new(all: std::sync::Arc<Vec<T>>, selected: Vec<T>) -> Self {
        Self {
            all,
            selected,
            only_selected: false,
        }
    }
    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }
    pub fn rows(self) -> std::sync::Arc<Vec<T>> {
        if self.only_selected {
            std::sync::Arc::new(self.selected)
        } else {
            self.all
        }
    }
}

/// One result line.
pub struct Row<'a> {
    pub name: &'a [u8],
    /// Full path of the folder holding the entry (the parent path of a
    /// root).
    pub folder: &'a [u8],
    pub size: u64,
    pub mtime_ns: i64,
    pub offline: bool,
}

/// Writes rows of one format. Call [`finish`](Self::finish) at the end.
pub struct Writer {
    out: BufWriter<fs::File>,
    tmp: PathBuf,
    target: PathBuf,
    format: Format,
    zone: Zone,
    rows: usize,
    protect_formulas: bool,
}

impl Writer {
    /// Starts an export to `target`: creates the temporary file next to it
    /// and writes the start of the format (a header line, the playlist tag,
    /// the opening of the array or the page).
    pub fn create(
        target: &Path,
        format: Format,
        zone: Zone,
        protect_formulas: bool,
    ) -> io::Result<Self> {
        let (file, tmp) = crate::settings::create_temp(target)?;
        let mut w = Self {
            out: BufWriter::with_capacity(1 << 16, file),
            tmp,
            target: target.to_path_buf(),
            format,
            zone,
            rows: 0,
            protect_formulas,
        };
        let head: &[u8] = match format {
            Format::Csv => b"Name,Folder,Size,Modified,Offline\r\n",
            Format::Tsv => b"Name\tFolder\tSize\tModified\tOffline\n",
            Format::Text => b"",
            Format::M3u8 => b"#EXTM3U\n",
            Format::Json => b"[",
            Format::Html => HTML_HEAD.as_bytes(),
        };
        if let Err(e) = w.out.write_all(head) {
            w.abandon();
            return Err(e);
        }
        Ok(w)
    }

    /// Appends one result.
    pub fn row(&mut self, row: &Row) -> io::Result<()> {
        let out = &mut self.out;
        match self.format {
            Format::Csv | Format::Tsv => {
                let size = row.size.to_string();
                let modified = local_time(row.mtime_ns, &self.zone);
                let offline = if row.offline { &b"yes"[..] } else { &b""[..] };
                for (i, cell) in [
                    row.name,
                    row.folder,
                    size.as_bytes(),
                    modified.as_bytes(),
                    offline,
                ]
                .into_iter()
                .enumerate()
                {
                    if i > 0 {
                        out.write_all(if self.format == Format::Csv {
                            b","
                        } else {
                            b"\t"
                        })?;
                    }
                    if self.format == Format::Csv {
                        csv_field(out, cell, self.protect_formulas)?;
                    } else {
                        out.write_all(formula_prefix(cell, self.protect_formulas))?;
                        text_line(out, cell)?;
                    }
                }
                out.write_all(if self.format == Format::Csv {
                    b"\r\n"
                } else {
                    b"\n"
                })?;
            }
            Format::M3u8 => {
                let path = full_path_bytes(row);
                if path.contains(&b'\n') || path.contains(&b'\r') {
                    return Ok(());
                }
                out.write_all(&path)?;
                out.write_all(b"\n")?;
            }
            Format::Json => {
                let name = String::from_utf8_lossy(row.name);
                let folder = String::from_utf8_lossy(row.folder);
                out.write_all(if self.rows == 0 { b"\n" } else { b",\n" })?;
                out.write_all(b"{\"name\":")?;
                json_string(out, &name)?;
                out.write_all(b",\"folder\":")?;
                json_string(out, &folder)?;
                write!(out, ",\"size\":{},\"modified\":", row.size)?;
                match row.mtime_ns {
                    0 => out.write_all(b"null")?,
                    t => write!(out, "\"{}\"", local_time(t, &self.zone))?,
                }
                write!(out, ",\"offline\":{}}}", row.offline)?;
            }
            Format::Html => {
                let name = String::from_utf8_lossy(row.name);
                let folder = String::from_utf8_lossy(row.folder);
                out.write_all(b"<tr><td>")?;
                html_text(out, &name)?;
                out.write_all(b"</td><td>")?;
                html_text(out, &folder)?;
                writeln!(
                    out,
                    "</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    row.size,
                    local_time(row.mtime_ns, &self.zone),
                    if row.offline { "yes" } else { "" }
                )?;
            }
            Format::Text => {
                text_line(out, &full_path_bytes(row))?;
                if row.offline {
                    out.write_all(b"\t(offline)")?;
                }
                out.write_all(b"\n")?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// Writes the rest, syncs and puts the file in place. Returns the number
    /// of rows.
    pub fn finish(mut self) -> io::Result<usize> {
        let tail: &[u8] = match self.format {
            Format::Json if self.rows > 0 => b"\n]\n",
            Format::Json => b"]\n",
            Format::Html => HTML_TAIL.as_bytes(),
            _ => b"",
        };
        let result = (|| {
            self.out.write_all(tail)?;
            self.out.flush()?;
            self.out.get_ref().sync_all()?;
            fs::rename(&self.tmp, &self.target)
        })();
        match result {
            Ok(()) => Ok(self.rows),
            Err(e) => {
                self.abandon();
                Err(e)
            }
        }
    }

    /// Removes the temporary file after a failure.
    pub fn abandon(&mut self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

/// The start of an HTML export, up to the first row.
const HTML_HEAD: &str = "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
<title>EveryGrep results</title>\n<style>\n\
body { font-family: sans-serif; margin: 1em; }\n\
table { border-collapse: collapse; }\n\
th, td { border: 1px solid #ccc; padding: 2px 8px; text-align: left; }\n\
td:nth-child(3) { text-align: right; }\n\
</style>\n</head>\n<body>\n<table>\n\
<thead><tr><th>Name</th><th>Folder</th><th>Size</th><th>Modified</th><th>Offline</th></tr></thead>\n\
<tbody>\n";

/// The end of an HTML export.
const HTML_TAIL: &str = "</tbody>\n</table>\n</body>\n</html>\n";

/// The full path of a row, using the bytes of the file system.
fn full_path_bytes(row: &Row) -> Vec<u8> {
    let mut path = row.folder.to_vec();
    if !path.is_empty() && !path.ends_with(b"/") {
        path.push(b'/');
    }
    path.extend_from_slice(row.name);
    path
}

/// A JSON string: quoted, with `"`, `\` and control characters escaped.
fn json_string(out: &mut impl Write, text: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    for c in text.chars() {
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => write!(out, "{c}")?,
        }
    }
    out.write_all(b"\"")
}

/// Text in an HTML element: `&`, `<`, `>` and `"` as entities.
fn html_text(out: &mut impl Write, text: &str) -> io::Result<()> {
    let mut rest = text;
    while let Some(i) = rest.find(['&', '<', '>', '"']) {
        out.write_all(&rest.as_bytes()[..i])?;
        out.write_all(match rest.as_bytes()[i] {
            b'&' => b"&amp;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            _ => b"&quot;",
        })?;
        rest = &rest[i + 1..];
    }
    out.write_all(rest.as_bytes())
}

/// A path for the text format: `\`, line feed, carriage return and tab
/// written as `\\`, `\n`, `\r` and `\t`, so it stays on its line and the
/// tab before `(offline)` is the only one.
fn text_line(out: &mut impl Write, text: &[u8]) -> io::Result<()> {
    let mut start = 0;
    for (i, b) in text.iter().enumerate() {
        let escaped: &[u8] = match b {
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            _ => continue,
        };
        out.write_all(&text[start..i])?;
        out.write_all(escaped)?;
        start = i + 1;
    }
    out.write_all(&text[start..])
}

/// One cell of tab-separated text on the clipboard: escaped as in a TSV
/// export (`\\`, `\n`, `\r`, `\t`), so each row stays one line, with a
/// `'` in front of a text a spreadsheet would read as a formula when
/// `protect` is on.
pub fn tsv_cell(text: &str, protect: bool) -> String {
    let mut out = Vec::with_capacity(text.len() + 1);
    out.extend_from_slice(formula_prefix(text.as_bytes(), protect));
    // Writing into a Vec cannot fail.
    let _ = text_line(&mut out, text.as_bytes());
    String::from_utf8(out).unwrap_or_default()
}

/// A header row and the rows below it as tab-separated text, one line per
/// row, each line ending with `\n`.
pub fn tsv_table(header: &[String], rows: &[Vec<String>], protect: bool) -> String {
    let mut out = String::new();
    for row in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        let cells: Vec<String> = row.iter().map(|c| tsv_cell(c, protect)).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

/// The values of one column for the clipboard: one value as it is, several
/// one per line, escaped as [`tsv_cell`] without the formula mark.
pub fn column_lines(values: &[String]) -> String {
    match values {
        [one] => one.clone(),
        many => many
            .iter()
            .map(|v| tsv_cell(v, false))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Spreadsheet protection is applied to original bytes before escaping.
fn formula_prefix(text: &[u8], protect: bool) -> &'static [u8] {
    if protect && text.first().is_some_and(|b| b"=+-@\t\r".contains(b)) {
        b"'"
    } else {
        b""
    }
}

/// One CSV field, quoted when it has a comma, a quote or a line break.
fn csv_field(out: &mut impl Write, text: &[u8], protect: bool) -> io::Result<()> {
    if !text.iter().any(|b| b",\"\n\r".contains(b)) {
        out.write_all(formula_prefix(text, protect))?;
        return out.write_all(text);
    }
    out.write_all(b"\"")?;
    out.write_all(formula_prefix(text, protect))?;
    let mut start = 0;
    for (i, b) in text.iter().enumerate() {
        if *b == b'"' {
            out.write_all(&text[start..i])?;
            out.write_all(b"\"\"")?;
            start = i + 1;
        }
    }
    out.write_all(&text[start..])?;
    out.write_all(b"\"")
}

/// `2026-10-25 22:41:07` in the local zone; empty for an unknown time.
fn local_time(mtime_ns: i64, zone: &Zone) -> String {
    if mtime_ns == 0 {
        return String::new();
    }
    let secs = mtime_ns.div_euclid(1_000_000_000);
    let (y, m, d) = zone.local_date(secs);
    let t = zone.to_local(secs).rem_euclid(86_400);
    format!(
        "{y}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_tsv_keeps_every_row_on_one_line() {
        let header = vec!["Name".to_owned(), "Folder".to_owned()];
        let rows = vec![
            vec!["a\tb.txt".to_owned(), "/x\\y".to_owned()],
            vec!["line\nbreak\r".to_owned(), String::new()],
            vec!["=SUM(A1)".to_owned(), "+1".to_owned()],
        ];
        assert_eq!(
            tsv_table(&header, &rows, true),
            "Name\tFolder\na\\tb.txt\t/x\\\\y\nline\\nbreak\\r\t\n'=SUM(A1)\t'+1\n"
        );
        assert_eq!(
            tsv_table(&header, &rows[2..], false),
            "Name\tFolder\n=SUM(A1)\t+1\n"
        );
        assert_eq!(tsv_table(&header, &[], true), "Name\tFolder\n");
        // A tab at the start is both marked and escaped.
        assert_eq!(tsv_cell("\tx", true), "'\\tx");
        assert_eq!(tsv_cell("Şəkil ünü.png", true), "Şəkil ünü.png");
    }

    #[test]
    fn one_column_value_is_copied_as_it_is_several_one_per_line() {
        assert_eq!(column_lines(&["a\nb".to_owned()]), "a\nb");
        assert_eq!(
            column_lines(&["a\nb".to_owned(), "c".to_owned()]),
            "a\\nb\nc"
        );
        assert_eq!(column_lines(&[]), "");
    }
    use crate::settings::tests::Scratch;

    fn utc() -> Zone {
        Zone::utc()
    }

    fn rows() -> Vec<Row<'static>> {
        vec![
            Row {
                name: b"report, final.pdf",
                folder: b"/home/u/Docs",
                size: 3 << 20,
                mtime_ns: 1_761_431_667_000_000_000,
                offline: false,
            },
            Row {
                name: b"say \"hi\"\n.txt",
                folder: b"/run/media/u/USB",
                size: 0,
                mtime_ns: 0,
                offline: true,
            },
            Row {
                name: b"bad\xffbyte",
                folder: b"/",
                size: 7,
                mtime_ns: 1,
                offline: false,
            },
        ]
    }

    #[test]
    fn csv_and_text_exports() {
        let dir = Scratch::new("export");
        let csv = dir.0.join("out.csv");
        let mut w = Writer::create(&csv, Format::Csv, utc(), true).unwrap();
        for r in rows() {
            w.row(&r).unwrap();
        }
        assert_eq!(w.finish().unwrap(), 3);
        assert_eq!(
            fs::read(&csv).unwrap(),
            b"Name,Folder,Size,Modified,Offline\r\n\
             \"report, final.pdf\",/home/u/Docs,3145728,2025-10-25 22:34:27,\r\n\
             \"say \"\"hi\"\"\n.txt\",/run/media/u/USB,0,,yes\r\n\
             bad\xffbyte,/,7,1970-01-01 00:00:00,\r\n"
        );
        let txt = dir.0.join("out.txt");
        let mut w = Writer::create(&txt, Format::Text, utc(), true).unwrap();
        for r in rows() {
            w.row(&r).unwrap();
        }
        w.finish().unwrap();
        assert_eq!(
            fs::read(&txt).unwrap(),
            b"/home/u/Docs/report, final.pdf\n\
             /run/media/u/USB/say \"hi\"\\n.txt\t(offline)\n\
             /bad\xffbyte\n"
        );
        // A line break, tab or backslash in a name is written as an escape,
        // so every result stays one line.
        let mut w = Writer::create(&txt, Format::Text, utc(), true).unwrap();
        w.row(&Row {
            name: b"a\\b\tc\rd\ne",
            folder: b"/x",
            size: 0,
            mtime_ns: 0,
            offline: false,
        })
        .unwrap();
        w.finish().unwrap();
        assert_eq!(fs::read(&txt).unwrap(), b"/x/a\\\\b\\tc\\rd\\ne\n");
        // No temporary files are left.
        let left: Vec<_> = fs::read_dir(&dir.0).unwrap().collect();
        assert_eq!(left.len(), 2);
    }

    /// The file that `format` makes of [`rows`].
    fn export(format: Format, rows: &[Row]) -> String {
        String::from_utf8(export_bytes(format, rows)).unwrap()
    }

    fn export_bytes(format: Format, rows: &[Row]) -> Vec<u8> {
        let dir = Scratch::new("export-format");
        let path = dir.0.join(format!("out.{}", format.extension()));
        let mut w = Writer::create(&path, format, utc(), true).unwrap();
        for r in rows {
            w.row(r).unwrap();
        }
        w.finish().unwrap();
        fs::read(&path).unwrap()
    }

    #[test]
    fn tsv_export_escapes_tabs_and_line_breaks() {
        assert_eq!(
            export_bytes(Format::Tsv, &rows()),
            b"Name\tFolder\tSize\tModified\tOffline\n\
             report, final.pdf\t/home/u/Docs\t3145728\t2025-10-25 22:34:27\t\n\
             say \"hi\"\\n.txt\t/run/media/u/USB\t0\t\tyes\n\
             bad\xffbyte\t/\t7\t1970-01-01 00:00:00\t\n"
        );
        let tab = Row {
            name: b"a\tb\\c",
            folder: b"/x\ty",
            size: 1,
            mtime_ns: 0,
            offline: false,
        };
        assert_eq!(
            export(Format::Tsv, &[tab]).lines().nth(1).unwrap(),
            "a\\tb\\\\c\t/x\\ty\t1\t\t"
        );
    }

    #[test]
    fn playlist_export_leaves_out_paths_with_line_breaks() {
        let dir = Scratch::new("export-m3u8");
        let path = dir.0.join("out.m3u8");
        let mut w = Writer::create(&path, Format::M3u8, utc(), true).unwrap();
        for r in rows() {
            w.row(&r).unwrap();
        }
        w.row(&Row {
            name: b"track\rhidden.mp3",
            folder: b"/music",
            size: 1,
            mtime_ns: 0,
            offline: false,
        })
        .unwrap();
        // Neither line feed nor carriage return can create another playlist line.
        assert_eq!(w.finish().unwrap(), 2);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"#EXTM3U\n/home/u/Docs/report, final.pdf\n/bad\xffbyte\n"
        );
        // The playlist keeps the original path bytes, including invalid UTF-8.
        let odd = Row {
            name: b"song\xff #1%.mp3",
            folder: b"/m\xe9/a b",
            size: 1,
            mtime_ns: 0,
            offline: false,
        };
        assert_eq!(
            export_bytes(Format::M3u8, &[odd]),
            b"#EXTM3U\n/m\xe9/a b/song\xff #1%.mp3\n"
        );
    }

    #[test]
    fn byte_escaping_keeps_invalid_bytes_next_to_delimiters() {
        let row = Row {
            name: b"a\xff,\t\"\\\n",
            folder: b"/f\x80",
            size: 1,
            mtime_ns: 0,
            offline: false,
        };
        let rows = std::slice::from_ref(&row);
        assert_eq!(
            export_bytes(Format::Csv, rows),
            b"Name,Folder,Size,Modified,Offline\r\n\"a\xff,\t\"\"\\\n\",/f\x80,1,,\r\n"
        );
        assert_eq!(
            export_bytes(Format::Tsv, rows),
            b"Name\tFolder\tSize\tModified\tOffline\na\xff,\\t\"\\\\\\n\t/f\x80\t1\t\t\n"
        );
        assert_eq!(
            export_bytes(Format::Text, rows),
            b"/f\x80/a\xff,\\t\"\\\\\\n\n"
        );
        assert_eq!(export_bytes(Format::M3u8, rows), b"#EXTM3U\n");
    }

    #[test]
    fn spreadsheet_protection_covers_each_trigger_and_can_be_disabled() {
        let cases: &[(&[u8], &[u8], &[u8])] = &[
            (b"=x\xff", b"'=x\xff", b"'=x\xff"),
            (b"+x\xff", b"'+x\xff", b"'+x\xff"),
            (b"-x\xff", b"'-x\xff", b"'-x\xff"),
            (b"@x\xff", b"'@x\xff", b"'@x\xff"),
            (b"\tx\xff", b"'\tx\xff", b"'\\tx\xff"),
            (b"\rx\xff", b"\"'\rx\xff\"", b"'\\rx\xff"),
        ];
        for &(name, csv_cell, tsv_cell) in cases {
            let row = Row {
                name,
                folder: name,
                size: 1,
                mtime_ns: 0,
                offline: false,
            };
            for format in [Format::Csv, Format::Tsv] {
                let protected = export_bytes(format, std::slice::from_ref(&row));
                let cell = if format == Format::Csv {
                    csv_cell
                } else {
                    tsv_cell
                };
                let separator = if format == Format::Csv { b',' } else { b'\t' };
                let mut expected = cell.to_vec();
                expected.push(separator);
                expected.extend_from_slice(cell);
                assert!(
                    protected.windows(expected.len()).any(|s| s == expected),
                    "{format:?}: {name:?}"
                );
                let dir = Scratch::new("export-unprotected");
                let path = dir.0.join("out");
                let mut writer = Writer::create(&path, format, utc(), false).unwrap();
                writer.row(&row).unwrap();
                writer.finish().unwrap();
                let plain = fs::read(path).unwrap();
                assert_ne!(plain, protected);
                assert!(!plain.contains(&b'\''));
            }
            let json = export(Format::Json, std::slice::from_ref(&row));
            assert!(!json.contains('\''));
        }
        for cell in [
            b"".as_slice(),
            b"normal",
            b"1+2",
            b" =x",
            b"'already",
            b"\n=x",
        ] {
            assert!(formula_prefix(cell, true).is_empty());
        }
        let mut csv = Vec::new();
        csv_field(&mut csv, b"=a,\"b", true).unwrap();
        assert_eq!(csv, b"\"'=a,\"\"b\"");
    }

    #[test]
    fn protection_does_not_change_json_html_text_or_playlists() {
        let row = Row {
            name: b"=song.mp3",
            folder: b"/music",
            size: 1,
            mtime_ns: 0,
            offline: false,
        };
        for format in [Format::Json, Format::Html, Format::Text, Format::M3u8] {
            let protected = export_bytes(format, std::slice::from_ref(&row));
            let dir = Scratch::new("export-other");
            let path = dir.0.join("out");
            let mut writer = Writer::create(&path, format, utc(), false).unwrap();
            writer.row(&row).unwrap();
            writer.finish().unwrap();
            assert_eq!(fs::read(path).unwrap(), protected, "{format:?}");
        }
        assert_eq!(
            export_bytes(Format::M3u8, &[row]),
            b"#EXTM3U\n/music/=song.mp3\n"
        );
    }

    #[test]
    fn byte_exports_preserve_invalid_utf8_in_both_name_and_folder() {
        let row = Row {
            name: b"raw\xff\xfe.txt",
            folder: b"/folder\x80",
            size: 1,
            mtime_ns: 0,
            offline: false,
        };
        for format in [Format::Csv, Format::Tsv, Format::Text, Format::M3u8] {
            let bytes = export_bytes(format, std::slice::from_ref(&row));
            assert!(
                bytes.windows(row.name.len()).any(|s| s == row.name),
                "{format:?}"
            );
            assert!(
                bytes.windows(row.folder.len()).any(|s| s == row.folder),
                "{format:?}"
            );
            assert!(
                !bytes.windows(3).any(|s| s == b"\xef\xbf\xbd"),
                "{format:?}"
            );
        }
        for format in [Format::Json, Format::Html] {
            let bytes = export_bytes(format, std::slice::from_ref(&row));
            assert!(std::str::from_utf8(&bytes).is_ok());
            assert!(bytes.windows(3).any(|s| s == b"\xef\xbf\xbd"));
        }
    }

    #[test]
    fn json_export_is_an_array_of_objects() {
        assert_eq!(
            export(Format::Json, &rows()),
            "[\n\
             {\"name\":\"report, final.pdf\",\"folder\":\"/home/u/Docs\",\"size\":3145728,\
             \"modified\":\"2025-10-25 22:34:27\",\"offline\":false},\n\
             {\"name\":\"say \\\"hi\\\"\\n.txt\",\"folder\":\"/run/media/u/USB\",\"size\":0,\
             \"modified\":null,\"offline\":true},\n\
             {\"name\":\"bad\u{fffd}byte\",\"folder\":\"/\",\"size\":7,\
             \"modified\":\"1970-01-01 00:00:00\",\"offline\":false}\n]\n"
        );
        assert_eq!(export(Format::Json, &[]), "[]\n");
        let control = Row {
            name: b"a\x01\\b\t",
            folder: b"/",
            size: 0,
            mtime_ns: 0,
            offline: false,
        };
        assert!(
            export(Format::Json, &[control]).contains(r#""name":"a\u0001\\b\t""#),
            "control characters and backslashes are escaped"
        );
    }

    #[test]
    fn html_export_escapes_markup() {
        let row = Row {
            name: b"<b>&\"x\".txt",
            folder: b"/home/u",
            size: 5,
            mtime_ns: 0,
            offline: true,
        };
        let page = export(Format::Html, &[row]);
        assert!(page.starts_with("<!DOCTYPE html>\n"));
        assert!(page.ends_with("</html>\n"));
        assert!(page.contains(
            "<tr><td>&lt;b&gt;&amp;&quot;x&quot;.txt</td><td>/home/u</td>\
             <td>5</td><td></td><td>yes</td></tr>\n"
        ));
        assert!(!page.contains("<b>"));
    }

    #[test]
    fn a_failed_export_leaves_the_old_file() {
        let dir = Scratch::new("export-keep");
        let target = dir.0.join("keep.csv");
        fs::write(&target, "old").unwrap();
        let mut w = Writer::create(&target, Format::Csv, utc(), true).unwrap();
        w.row(&rows()[0]).unwrap();
        w.abandon();
        drop(w);
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn an_export_never_writes_through_a_planted_link() {
        let dir = Scratch::new("export-link");
        let victim = dir.0.join("victim.txt");
        fs::write(&victim, "keep").unwrap();
        let target = dir.0.join("out.csv");
        // A link at a predictable temporary name (the file's name and the
        // process id).
        let planted = dir.0.join(format!(".out.csv.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let mut w = Writer::create(&target, Format::Text, utc(), true).unwrap();
        w.row(&rows()[0]).unwrap();
        assert_eq!(w.finish().unwrap(), 1);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(fs::symlink_metadata(&target).unwrap().is_file());
        assert!(fs::symlink_metadata(&planted).unwrap().is_symlink());
    }

    #[test]
    fn typed_extensions_win_without_changing_the_accepted_path() {
        for chosen in Format::ALL {
            for named in Format::ALL {
                let path = PathBuf::from(format!("şəkil.{}", named.extension().to_uppercase()));
                assert_eq!(saved_format(&path, chosen), named);
            }
            for name in ["results", "results.list"] {
                assert_eq!(saved_format(Path::new(name), chosen), chosen);
            }
        }
    }

    #[test]
    fn export_selection_is_never_automatically_enabled_and_counts_actual_rows() {
        let all = std::sync::Arc::new(vec![4, 7, 9]);
        for selected in [vec![], vec![7], vec![4, 9], vec![4, 7, 9]] {
            let selection = Selection::new(all.clone(), selected.clone());
            assert_eq!(selection.selected_count(), selected.len());
            assert!(!selection.only_selected);
            assert_eq!(*selection.rows(), *all);
            let mut selection = Selection::new(all.clone(), selected.clone());
            selection.only_selected = true;
            assert_eq!(*selection.rows(), selected);
        }
    }
}
