//! The window's settings and the small text file that keeps them.
//!
//! The file is `$XDG_CONFIG_HOME/everygrep/settings` (by default
//! `~/.config/everygrep/settings`). Each line is `key = value`; `#` starts a
//! comment line. List settings repeat their key, one value per line:
//!
//! ```text
//! root = /home/me
//! root = /srv/media
//! exclude = /home/me/.cache
//! filter = document
//! history-size = 50
//! on-close = hide
//! ```
//!
//! Only settings that differ from their defaults are written, so a file
//! without a key follows the program's default. Values are written with
//! backslash escapes (`\\`, `\n`, `\t`, `\s` for a space at either end,
//! `\xHH` for a byte that is not UTF-8), so any path fits on one line.
//!
//! Reading never fails on content: lines with an unknown key are kept and
//! written back unchanged (a newer version may have written them); lines that
//! cannot be understood are skipped and reported in [`Settings::problems`].
//! The file is replaced atomically (written to a temporary file, synced and
//! renamed), so a crash leaves the old or the new file, never a mix.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use everygrep_core::filter::Filter;
use everygrep_core::query::QueryOptions;

/// Search history entries kept by default (the number before settings
/// existed).
pub const DEFAULT_HISTORY: usize = 200;
/// Most search history entries that can be kept.
pub const MAX_HISTORY: usize = 1000;
/// Widths outside this range in pixels are ignored when read.
const WIDTH_RANGE: std::ops::RangeInclusive<f32> = 1.0..=10_000.0;

/// What closing the window does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnClose {
    /// End the program (the behaviour before settings existed).
    #[default]
    Quit,
    /// Hide the window and keep the index loaded; the next launch shows it
    /// again through the single-instance hand-over.
    Hide,
}

impl OnClose {
    pub const ALL: [OnClose; 2] = [OnClose::Quit, OnClose::Hide];

    fn id(self) -> &'static str {
        match self {
            OnClose::Quit => "quit",
            OnClose::Hide => "hide",
        }
    }
}

/// Optional columns and column widths as the window last showed them. A width
/// of `None` means the window's default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Columns {
    pub show_type: bool,
    pub show_ext: bool,
    pub name: Option<f32>,
    pub size: Option<f32>,
    pub date: Option<f32>,
    pub type_: Option<f32>,
    pub ext: Option<f32>,
}

/// Every setting the window has. `Default` gives the program's behaviour
/// without a settings file.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Folders to index; empty means the folders of the saved index, or the
    /// home folder for a first start.
    pub roots: Vec<PathBuf>,
    /// Folders that are not entered while indexing.
    pub excludes: Vec<PathBuf>,
    /// Filter and matching modes for a new start.
    pub options: QueryOptions,
    /// Searches kept for Back and Forward; 0 turns the history off.
    pub history_size: usize,
    /// Whether the search history is saved between sessions.
    pub history_on_disk: bool,
    pub on_close: OnClose,
    /// Whether the optional columns and the column widths are kept between
    /// sessions.
    pub remember_columns: bool,
    pub columns: Columns,
    /// Lines with keys this version does not know, kept as they were.
    unknown: Vec<String>,
    /// Lines that could not be understood: (line number, text).
    pub problems: Vec<(usize, String)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            excludes: Vec::new(),
            options: QueryOptions::default(),
            history_size: DEFAULT_HISTORY,
            history_on_disk: false,
            on_close: OnClose::Quit,
            remember_columns: true,
            columns: Columns::default(),
            unknown: Vec::new(),
            problems: Vec::new(),
        }
    }
}

/// The settings that one page of the settings window resets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Indexing,
    Search,
    Window,
}

impl Settings {
    /// Reads settings from the text of a settings file.
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for (i, line) in text.lines().enumerate() {
            // Only ASCII blanks are trimmed: a path may end in any other
            // character, and spaces at the ends are escaped.
            let trimmed = line.trim_ascii();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                s.problems.push((i + 1, line.to_owned()));
                continue;
            };
            let (key, value) = (key.trim_ascii(), value.trim_ascii());
            if !s.set(key, value) {
                if is_known(key) {
                    s.problems.push((i + 1, line.to_owned()));
                } else if !key.is_empty() && !key.contains(char::is_whitespace) {
                    s.unknown.push(trimmed.to_owned());
                } else {
                    s.problems.push((i + 1, line.to_owned()));
                }
            }
        }
        s
    }

    /// Applies one `key = value` line. Returns false when the key is unknown
    /// or the value does not fit it.
    fn set(&mut self, key: &str, value: &str) -> bool {
        let flag = |v: &str| match v {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        };
        let width = |v: &str| v.parse::<f32>().ok().filter(|w| WIDTH_RANGE.contains(w));
        let put_flag = |target: &mut bool| flag(value).map(|v| *target = v).is_some();
        match key {
            "root" | "exclude" => {
                let Some(path) = unescape(value).filter(|p| !p.is_empty()) else {
                    return false;
                };
                let list = if key == "root" {
                    &mut self.roots
                } else {
                    &mut self.excludes
                };
                list.push(PathBuf::from(OsString::from_vec(path)));
                true
            }
            "filter" => Filter::from_id(value)
                .map(|f| self.options.filter = f)
                .is_some(),
            "match-case" => put_flag(&mut self.options.case_sensitive),
            "whole-words" => put_flag(&mut self.options.whole_words),
            "match-path" => put_flag(&mut self.options.match_path),
            "match-diacritics" => put_flag(&mut self.options.match_diacritics),
            "history-size" => value
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= MAX_HISTORY)
                .map(|n| self.history_size = n)
                .is_some(),
            "history-on-disk" => put_flag(&mut self.history_on_disk),
            "on-close" => OnClose::ALL
                .into_iter()
                .find(|o| o.id() == value)
                .map(|o| self.on_close = o)
                .is_some(),
            "remember-columns" => put_flag(&mut self.remember_columns),
            "show-type-column" => put_flag(&mut self.columns.show_type),
            "show-extension-column" => put_flag(&mut self.columns.show_ext),
            "name-width" | "size-width" | "modified-width" | "type-width" | "extension-width" => {
                let Some(w) = width(value) else { return false };
                let c = &mut self.columns;
                *match key {
                    "name-width" => &mut c.name,
                    "size-width" => &mut c.size,
                    "modified-width" => &mut c.date,
                    "type-width" => &mut c.type_,
                    _ => &mut c.ext,
                } = Some(w);
                true
            }
            _ => false,
        }
    }

    /// The text of a settings file: settings that differ from their
    /// defaults, then the unknown lines that were read.
    pub fn to_text(&self) -> String {
        let d = Settings::default();
        let mut out = String::from(
            "# EveryGrep settings. Only settings that differ from the defaults are listed.\n",
        );
        let line = |out: &mut String, key: &str, value: &str| {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(value);
            out.push('\n');
        };
        let flag = |out: &mut String, key: &str, value: bool, default: bool| {
            if value != default {
                line(out, key, if value { "true" } else { "false" });
            }
        };
        for root in &self.roots {
            line(&mut out, "root", &escape(root.as_os_str().as_bytes()));
        }
        for exclude in &self.excludes {
            line(&mut out, "exclude", &escape(exclude.as_os_str().as_bytes()));
        }
        let (o, od) = (&self.options, &d.options);
        if o.filter != od.filter {
            line(&mut out, "filter", o.filter.id());
        }
        flag(&mut out, "match-case", o.case_sensitive, od.case_sensitive);
        flag(&mut out, "whole-words", o.whole_words, od.whole_words);
        flag(&mut out, "match-path", o.match_path, od.match_path);
        flag(
            &mut out,
            "match-diacritics",
            o.match_diacritics,
            od.match_diacritics,
        );
        if self.history_size != d.history_size {
            line(&mut out, "history-size", &self.history_size.to_string());
        }
        flag(
            &mut out,
            "history-on-disk",
            self.history_on_disk,
            d.history_on_disk,
        );
        if self.on_close != d.on_close {
            line(&mut out, "on-close", self.on_close.id());
        }
        flag(
            &mut out,
            "remember-columns",
            self.remember_columns,
            d.remember_columns,
        );
        let c = &self.columns;
        flag(&mut out, "show-type-column", c.show_type, false);
        flag(&mut out, "show-extension-column", c.show_ext, false);
        let widths = [
            ("name-width", c.name),
            ("size-width", c.size),
            ("modified-width", c.date),
            ("type-width", c.type_),
            ("extension-width", c.ext),
        ];
        for (key, width) in widths {
            if let Some(w) = width {
                line(&mut out, key, &format!("{}", w.round()));
            }
        }
        for unknown in &self.unknown {
            out.push_str(unknown);
            out.push('\n');
        }
        out
    }

    /// Puts the settings of one page of the settings window back to their
    /// defaults.
    pub fn reset(&mut self, page: Page) {
        let d = Settings::default();
        match page {
            Page::Indexing => {
                self.roots = d.roots;
                self.excludes = d.excludes;
            }
            Page::Search => {
                self.options = d.options;
                self.history_size = d.history_size;
                self.history_on_disk = d.history_on_disk;
            }
            Page::Window => {
                self.on_close = d.on_close;
                self.remember_columns = d.remember_columns;
                self.columns = d.columns;
            }
        }
    }

    /// Reads the settings file at `path`; a missing file gives the defaults.
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => Ok(Self::parse(&String::from_utf8_lossy(&bytes))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Writes the settings to `path` atomically, creating its folder.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, self.to_text().as_bytes())
    }
}

/// Reads the file at `path`, changes it with `change` and writes it back.
/// Reading first keeps changes another window made meanwhile. A file that
/// exists but cannot be read is left alone and the error returned.
pub fn update(path: &Path, change: impl FnOnce(&mut Settings)) -> io::Result<Settings> {
    let mut s = Settings::load(path)?;
    change(&mut s);
    s.save(path)?;
    Ok(s)
}

fn is_known(key: &str) -> bool {
    let mut probe = Settings::default();
    // Every known key accepts at least one of these values.
    ["true", "all", "quit", "1", "/"]
        .iter()
        .any(|v| probe.set(key, v))
}

/// Folder of the settings: `$XDG_CONFIG_HOME/everygrep`, falling back to
/// `~/.config/everygrep`.
pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("everygrep"))
}

/// Location of the settings file.
pub fn path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings"))
}

/// Location of the saved search history (used when it is kept on disk).
pub fn history_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("history"))
}

/// Writes `bytes` to `path` through a temporary file in the same folder, so
/// readers see the old or the new content, never a mix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent folder"))?;
    fs::create_dir_all(dir)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(name);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// One value for a settings line: backslash escapes for `\`, line breaks,
/// tabs, other control characters and bytes that are not UTF-8, and `\s` for
/// spaces at either end (they would be trimmed otherwise).
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => {
                    let mut buf = [0; 4];
                    for b in c.encode_utf8(&mut buf).bytes() {
                        out.push_str(&format!("\\x{b:02x}"));
                    }
                }
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    // Spaces at the ends would be trimmed when read.
    let lead = out.len() - out.trim_start_matches(' ').len();
    let trail = out.len() - out.trim_end_matches(' ').len();
    if lead + trail > 0 {
        if lead == out.len() {
            return "\\s".repeat(lead);
        }
        let middle = &out[lead..out.len() - trail];
        return format!("{}{middle}{}", "\\s".repeat(lead), "\\s".repeat(trail));
    }
    out
}

/// The bytes of an escaped value; `None` for a broken escape.
pub fn unescape(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next()? {
            b'\\' => out.push(b'\\'),
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b's' => out.push(b' '),
            b'x' => {
                let hex = [bytes.next()?, bytes.next()?];
                let hex = std::str::from_utf8(&hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
            }
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh folder under the system temp folder, removed on drop.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "everygrep-gui-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_write_an_empty_file_and_read_back() {
        let d = Settings::default();
        let text = d.to_text();
        assert!(text.lines().all(|l| l.starts_with('#')), "{text}");
        assert_eq!(Settings::parse(&text), d);
        assert_eq!(Settings::parse(""), d);
    }

    #[test]
    fn every_setting_round_trips() {
        let s = Settings {
            roots: vec!["/home/u".into(), "/srv/media files".into()],
            excludes: vec!["/home/u/.cache".into()],
            options: QueryOptions {
                case_sensitive: true,
                whole_words: true,
                match_path: true,
                match_diacritics: true,
                filter: Filter::Picture,
            },
            history_size: 0,
            history_on_disk: true,
            on_close: OnClose::Hide,
            remember_columns: false,
            columns: Columns {
                show_type: true,
                show_ext: true,
                name: Some(300.0),
                size: Some(90.0),
                date: Some(140.0),
                type_: Some(120.0),
                ext: Some(60.0),
            },
            ..Settings::default()
        };
        let text = s.to_text();
        assert_eq!(Settings::parse(&text), s, "{text}");
        assert!(text.contains("root = /srv/media files\n"));
        assert!(text.contains("on-close = hide\n"));
    }

    #[test]
    fn odd_paths_survive_the_trip() {
        let odd: Vec<PathBuf> = [
            &b" lead and trail "[..],
            b"back\\slash",
            b"line\nbreak\ttab",
            b"bad\xffutf8",
            b"x = y # not a comment",
            b"   ",
            "Şəkillər".as_bytes(),
            "nbsp\u{a0}".as_bytes(),
        ]
        .iter()
        .map(|b| PathBuf::from(OsString::from_vec(b.to_vec())))
        .collect();
        let s = Settings {
            roots: odd.clone(),
            ..Settings::default()
        };
        let text = s.to_text();
        assert_eq!(text.lines().count(), 1 + odd.len());
        assert_eq!(Settings::parse(&text).roots, odd);
    }

    #[test]
    fn unknown_keys_are_kept_and_bad_lines_skipped() {
        let text = "\
# comment
future-key = some value
not a setting line
filter = nonsense
history-size = 99999
match-case = yes
[section]
name-width = -5
   = empty key
root = broken\\q
";
        let s = Settings::parse(text);
        assert!(s.options.case_sensitive);
        assert_eq!(s.options.filter, Filter::All);
        assert_eq!(s.history_size, DEFAULT_HISTORY);
        assert_eq!(s.columns.name, None);
        assert!(s.roots.is_empty());
        let lines: Vec<usize> = s.problems.iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, [3, 4, 5, 7, 8, 9, 10]);
        let out = s.to_text();
        assert!(out.contains("future-key = some value\n"));
        assert!(out.contains("match-case = true\n"));
        assert!(!out.contains("nonsense"));
        // Writing again changes nothing.
        assert_eq!(Settings::parse(&out).to_text(), out);
    }

    #[test]
    fn reset_touches_one_page_only() {
        let mut s = Settings::parse(
            "root = /a\nexclude = /a/b\nfilter = code\nhistory-size = 5\non-close = hide\n",
        );
        s.reset(Page::Search);
        assert_eq!(s.options, QueryOptions::default());
        assert_eq!(s.history_size, DEFAULT_HISTORY);
        assert_eq!(s.roots, [PathBuf::from("/a")]);
        assert_eq!(s.on_close, OnClose::Hide);
        s.reset(Page::Indexing);
        assert!(s.roots.is_empty() && s.excludes.is_empty());
        s.reset(Page::Window);
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn save_is_atomic_and_update_keeps_other_changes() {
        let dir = Scratch::new("settings");
        let path = dir.0.join("everygrep").join("settings");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        update(&path, |s| s.on_close = OnClose::Hide).unwrap();
        // Another window changes another setting in between.
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("filter = video\nfuture = 1\n");
        fs::write(&path, text).unwrap();
        let s = update(&path, |s| s.history_size = 7).unwrap();
        assert_eq!(s.on_close, OnClose::Hide);
        assert_eq!(s.options.filter, Filter::Video);
        assert_eq!(Settings::load(&path).unwrap(), s);
        assert!(fs::read_to_string(&path).unwrap().contains("future = 1"));
        // No temporary file is left behind.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings"]);
    }

    #[test]
    fn an_unreadable_file_is_not_overwritten() {
        let dir = Scratch::new("unreadable");
        // A folder where the file should be cannot be read as a file.
        let path = dir.0.join("settings");
        fs::create_dir(&path).unwrap();
        assert!(update(&path, |s| s.history_size = 1).is_err());
        assert!(path.is_dir());
    }

    #[test]
    fn escapes() {
        assert_eq!(escape(b"a b"), "a b");
        assert_eq!(escape(b" a "), "\\sa\\s");
        assert_eq!(escape(b"\x01"), "\\x01");
        assert_eq!(unescape("\\x4"), None);
        assert_eq!(unescape("a\\"), None);
        assert_eq!(unescape("\\xzz"), None);
        assert_eq!(unescape("\\s\\\\").unwrap(), b" \\");
    }
}
