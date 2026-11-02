//! Exporting the result list to a file.
//!
//! Two formats:
//!
//! - **CSV** (RFC 4180, UTF-8, `\r\n` line ends): a header line
//!   `Name,Folder,Size,Modified,Offline`, then one line per result: the name,
//!   the full path of its folder, the size in bytes (for a folder the total
//!   size of the files below it), the local modification time as
//!   `YYYY-MM-DD HH:MM:SS` (empty when unknown) and `yes` for an entry of a
//!   drive that is not connected. Fields with a comma, a quote or a line
//!   break are quoted, quotes doubled.
//! - **Text**: one full path per line (`\n`); an entry of a drive that is not
//!   connected is followed by a tab and `(offline)`. A backslash, line feed,
//!   carriage return or tab in a path is written as `\\`, `\n`, `\r` or
//!   `\t`.
//!
//! Bytes of names that are not UTF-8 become U+FFFD. The file is written to a
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
    Text,
}

impl Format {
    pub const ALL: [Format; 2] = [Format::Csv, Format::Text];

    /// The extension a file of this format gets, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Text => "txt",
        }
    }

    /// The format a file name asks for by its extension (`.csv`, `.txt`,
    /// any case).
    pub fn of_path(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Format::ALL.into_iter().find(|f| f.extension() == ext)
    }
}

/// The file to write and its format: the extension of the chosen name
/// decides; a name without a known extension gets the one of the chosen
/// file type (`chosen`, CSV when unknown), unless a file of that name exists
/// already (the save dialog only asked about the name as typed).
pub fn target(path: PathBuf, chosen: Option<Format>) -> (PathBuf, Format) {
    if let Some(format) = Format::of_path(&path) {
        return (path, format);
    }
    let format = chosen.unwrap_or(Format::Csv);
    let mut with_ext = path.clone().into_os_string();
    with_ext.push(".");
    with_ext.push(format.extension());
    let with_ext = PathBuf::from(with_ext);
    if fs::symlink_metadata(&with_ext).is_ok() {
        (path, format)
    } else {
        (with_ext, format)
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
}

impl Writer {
    /// Starts an export to `target`: creates the temporary file next to it
    /// and writes the CSV header.
    pub fn create(target: &Path, format: Format, zone: Zone) -> io::Result<Self> {
        let (file, tmp) = crate::settings::create_temp(target)?;
        let mut w = Self {
            out: BufWriter::with_capacity(1 << 16, file),
            tmp,
            target: target.to_path_buf(),
            format,
            zone,
            rows: 0,
        };
        if format == Format::Csv {
            if let Err(e) = w.out.write_all(b"Name,Folder,Size,Modified,Offline\r\n") {
                w.abandon();
                return Err(e);
            }
        }
        Ok(w)
    }

    /// Appends one result.
    pub fn row(&mut self, row: &Row) -> io::Result<()> {
        self.rows += 1;
        let out = &mut self.out;
        match self.format {
            Format::Csv => {
                csv_field(out, &String::from_utf8_lossy(row.name))?;
                out.write_all(b",")?;
                csv_field(out, &String::from_utf8_lossy(row.folder))?;
                write!(
                    out,
                    ",{},{},{}\r\n",
                    row.size,
                    local_time(row.mtime_ns, &self.zone),
                    if row.offline { "yes" } else { "" }
                )
            }
            Format::Text => {
                let mut path = row.folder.to_vec();
                if !path.is_empty() && !path.ends_with(b"/") {
                    path.push(b'/');
                }
                path.extend_from_slice(row.name);
                out.write_all(text_line(&String::from_utf8_lossy(&path)).as_bytes())?;
                if row.offline {
                    out.write_all(b"\t(offline)")?;
                }
                out.write_all(b"\n")
            }
        }
    }

    /// Writes the rest, syncs and puts the file in place. Returns the number
    /// of rows.
    pub fn finish(mut self) -> io::Result<usize> {
        let result = (|| {
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

/// A path for the text format: `\`, line feed, carriage return and tab
/// written as `\\`, `\n`, `\r` and `\t`, so it stays on its line and the
/// tab before `(offline)` is the only one.
fn text_line(path: &str) -> std::borrow::Cow<'_, str> {
    if !path.contains(['\\', '\n', '\r', '\t']) {
        return path.into();
    }
    let mut out = String::with_capacity(path.len() + 8);
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.into()
}

/// One CSV field, quoted when it has a comma, a quote or a line break.
fn csv_field(out: &mut impl Write, text: &str) -> io::Result<()> {
    if text.contains([',', '"', '\n', '\r']) {
        write!(out, "\"{}\"", text.replace('"', "\"\""))
    } else {
        out.write_all(text.as_bytes())
    }
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
        let mut w = Writer::create(&csv, Format::Csv, utc()).unwrap();
        for r in rows() {
            w.row(&r).unwrap();
        }
        assert_eq!(w.finish().unwrap(), 3);
        assert_eq!(
            fs::read_to_string(&csv).unwrap(),
            "Name,Folder,Size,Modified,Offline\r\n\
             \"report, final.pdf\",/home/u/Docs,3145728,2025-10-25 22:34:27,\r\n\
             \"say \"\"hi\"\"\n.txt\",/run/media/u/USB,0,,yes\r\n\
             bad\u{fffd}byte,/,7,1970-01-01 00:00:00,\r\n"
        );
        let txt = dir.0.join("out.txt");
        let mut w = Writer::create(&txt, Format::Text, utc()).unwrap();
        for r in rows() {
            w.row(&r).unwrap();
        }
        w.finish().unwrap();
        assert_eq!(
            fs::read_to_string(&txt).unwrap(),
            "/home/u/Docs/report, final.pdf\n\
             /run/media/u/USB/say \"hi\"\\n.txt\t(offline)\n\
             /bad\u{fffd}byte\n"
        );
        // A line break, tab or backslash in a name is written as an escape,
        // so every result stays one line.
        let mut w = Writer::create(&txt, Format::Text, utc()).unwrap();
        w.row(&Row {
            name: b"a\\b\tc\rd\ne",
            folder: b"/x",
            size: 0,
            mtime_ns: 0,
            offline: false,
        })
        .unwrap();
        w.finish().unwrap();
        assert_eq!(fs::read_to_string(&txt).unwrap(), "/x/a\\\\b\\tc\\rd\\ne\n");
        // No temporary files are left.
        let left: Vec<_> = fs::read_dir(&dir.0).unwrap().collect();
        assert_eq!(left.len(), 2);
    }

    #[test]
    fn a_failed_export_leaves_the_old_file() {
        let dir = Scratch::new("export-keep");
        let target = dir.0.join("keep.csv");
        fs::write(&target, "old").unwrap();
        let mut w = Writer::create(&target, Format::Csv, utc()).unwrap();
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
        // The temporary name an export of this process used to take.
        let planted = dir.0.join(format!(".out.csv.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let mut w = Writer::create(&target, Format::Text, utc()).unwrap();
        w.row(&rows()[0]).unwrap();
        assert_eq!(w.finish().unwrap(), 1);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(fs::symlink_metadata(&target).unwrap().is_file());
        assert!(fs::symlink_metadata(&planted).unwrap().is_symlink());
    }

    #[test]
    fn the_name_decides_the_format() {
        let dir = Scratch::new("export-name");
        let p = |n: &str| dir.0.join(n);
        assert_eq!(
            target(p("a.CSV"), Some(Format::Text)),
            (p("a.CSV"), Format::Csv)
        );
        assert_eq!(target(p("a.txt"), None), (p("a.txt"), Format::Text));
        assert_eq!(
            target(p("a"), Some(Format::Text)),
            (p("a.txt"), Format::Text)
        );
        assert_eq!(target(p("a.list"), None), (p("a.list.csv"), Format::Csv));
        // An existing file with the extension is not replaced unasked.
        fs::write(p("b.csv"), "x").unwrap();
        assert_eq!(target(p("b"), None), (p("b"), Format::Csv));
    }
}
