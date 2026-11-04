//! CSV import and export of filters and bookmarks.
//!
//! The first row names the columns; an import finds its columns by these
//! names (in any order, any letter case), so files edited in a spreadsheet
//! or written by another program work as long as `Name` and `Search` are
//! there. Unknown columns are ignored. Switches are `0`/`1` (`true`/`false`
//! and `yes`/`no` are accepted). Texts starting with a spreadsheet formula
//! sign get a leading apostrophe, removed again on import.
//!
//! An import never replaces an entry: an entry equal to an existing one is
//! skipped, an entry whose name is taken by a different entry is added as
//! "Name (2)" (or the next free number). Folders of the same name merge.
//! A keyboard shortcut that is not free is left out and counted.

use std::collections::HashMap;
use std::io;

use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, QueryOptions};

use crate::commands::{Chord, Keymap};
use crate::csv;
use crate::i18n::{fill, tr};
use crate::saved_searches::{self, Bookmark, Kind, Layout, Library, NamedFilter, Target};

const TOGGLES: [&str; 9] = [
    "Case",
    "Whole Word",
    "Path",
    "Diacritics",
    "Prefix",
    "Suffix",
    "Ignore Punctuation",
    "Ignore Whitespace",
    "Regex",
];

const FILTER_HEADER: [&str; 22] = [
    "Name",
    "Case",
    "Whole Word",
    "Path",
    "Diacritics",
    "Prefix",
    "Suffix",
    "Ignore Punctuation",
    "Ignore Whitespace",
    "Regex",
    "Search",
    "Columns",
    "Sort",
    "Descending",
    "View",
    "Macro",
    "Key",
    "Type",
    "Hidden Files",
    "And Before Or",
    "Literal Operators",
    "Wildcards Anywhere",
];

const BOOKMARK_HEADER: [&str; 28] = [
    "Name",
    "Case",
    "Whole Word",
    "Path",
    "Diacritics",
    "Prefix",
    "Suffix",
    "Ignore Punctuation",
    "Ignore Whitespace",
    "Regex",
    "Search",
    "Filter",
    "Sort",
    "Descending",
    "Columns",
    "View",
    "Macro",
    "Key",
    "Kind",
    "Folder",
    "Hidden Files",
    "And Before Or",
    "Literal Operators",
    "Wildcards Anywhere",
    "Filter Search",
    "Filter Type",
    "Filter Hidden Files",
    "Filter Options",
];

/// Sort columns 0–8 by name; others by number.
const SORT_NAMES: [&str; 9] = [
    "name",
    "folder",
    "size",
    "modified",
    "type",
    "extension",
    "created",
    "accessed",
    "attributes",
];

fn bit(on: bool) -> &'static str {
    if on { "1" } else { "0" }
}

fn toggles(o: &QueryOptions) -> [bool; 9] {
    [
        o.case_sensitive,
        o.whole_words,
        o.match_path,
        o.match_diacritics,
        o.prefix,
        o.suffix,
        o.ignore_punctuation,
        o.ignore_whitespace,
        o.regex,
    ]
}

fn sort_text(column: i32) -> String {
    usize::try_from(column)
        .ok()
        .and_then(|c| SORT_NAMES.get(c))
        .map_or_else(|| column.to_string(), |s| (*s).into())
}

fn columns_text(columns: Option<&[String]>) -> String {
    match columns {
        None => String::new(),
        Some([]) => "standard".into(),
        Some(names) => names.join(";"),
    }
}

pub fn encode_filters(library: &Library) -> String {
    let mut out = String::new();
    csv::record(&mut out, FILTER_HEADER);
    for f in &library.filters {
        let key = library
            .shortcut(Target::Filter(f.id))
            .map(|c| c.to_string())
            .unwrap_or_default();
        let (sort, descending) = match f.layout.sort {
            Some((c, d)) => (sort_text(c), bit(d)),
            None => (String::new(), "0"),
        };
        let name = csv::protect(&f.title());
        let search = csv::protect(&f.search);
        let columns = columns_text(f.layout.columns.as_deref());
        let o = &f.options;
        let mut fields: Vec<&str> = vec![&name];
        fields.extend(toggles(o).map(bit));
        fields.extend([
            search.as_str(),
            &columns,
            &sort,
            descending,
            "",
            "",
            &key,
            o.filter.id(),
            o.hidden_files.id(),
            bit(o.and_before_or),
            bit(o.literal_operators),
            bit(o.wildcards_anywhere),
        ]);
        csv::record(&mut out, fields);
    }
    out
}

pub fn encode_bookmarks(library: &Library) -> String {
    let mut out = String::new();
    csv::record(&mut out, BOOKMARK_HEADER);
    for (i, _) in library.bookmark_tree() {
        let b = &library.bookmarks[i];
        let folder = library
            .bookmarks
            .iter()
            .find(|f| f.id == b.folder)
            .map(|f| csv::protect(&f.name))
            .unwrap_or_default();
        let name = csv::protect(&b.name);
        match b.kind {
            Kind::Folder | Kind::Separator => {
                let kind = if b.kind == Kind::Folder {
                    "folder"
                } else {
                    "separator"
                };
                let mut fields = vec![""; BOOKMARK_HEADER.len()];
                fields[0] = &name;
                fields[18] = kind;
                fields[19] = &folder;
                csv::record(&mut out, fields);
                continue;
            }
            Kind::Search => {}
        }
        let key = library
            .shortcut(Target::Bookmark(b.id))
            .map(|c| c.to_string())
            .unwrap_or_default();
        let search = csv::protect(&b.search);
        let filter = csv::protect(&b.filter.title());
        let filter_search = csv::protect(&b.filter.search);
        let sort = sort_text(b.sort_column);
        let columns = columns_text(b.columns.as_deref());
        let encoded = saved_searches::encode_options(b.filter.options);
        let filter_bits = encoded.split('\t').nth(1).unwrap_or("0").to_string();
        let o = &b.options;
        let mut fields: Vec<&str> = vec![&name];
        fields.extend(toggles(o).map(bit));
        fields.extend([
            search.as_str(),
            &filter,
            &sort,
            bit(b.descending),
            &columns,
            "",
            "",
            &key,
            "bookmark",
            &folder,
            o.hidden_files.id(),
            bit(o.and_before_or),
            bit(o.literal_operators),
            bit(o.wildcards_anywhere),
            &filter_search,
            b.filter.options.filter.id(),
            b.filter.options.hidden_files.id(),
            &filter_bits,
        ]);
        csv::record(&mut out, fields);
    }
    out
}

/// Rows of a CSV file by column name.
struct Table {
    columns: HashMap<String, usize>,
    rows: Vec<(usize, Vec<String>)>,
}

impl Table {
    fn parse(text: &str, required: &[&str]) -> io::Result<Self> {
        let rows = csv::rows(text).map_err(|e| {
            invalid(fill(
                tr("Malformed CSV at line {} (check the quotes)."),
                &[&e.line],
            ))
        })?;
        let mut rows = rows.into_iter();
        let header = rows
            .next()
            .ok_or_else(|| invalid(tr("The CSV file is empty.").into()))?;
        let mut columns = HashMap::new();
        for (i, name) in header.iter().enumerate() {
            let key = name.trim().to_lowercase();
            if columns.insert(key, i).is_some() {
                return Err(invalid(fill(
                    tr("Column “{}” appears twice in the CSV header."),
                    &[&name.trim()],
                )));
            }
        }
        for name in required {
            if !columns.contains_key(&name.to_lowercase()) {
                return Err(invalid(fill(
                    tr("The CSV header has no “{}” column."),
                    &[name],
                )));
            }
        }
        let rows = rows
            .enumerate()
            .map(|(i, r)| (i + 2, r))
            .filter(|(_, r)| !(r.len() == 1 && r[0].is_empty()))
            .collect::<Vec<_>>();
        if let Some((line, _)) = rows.iter().find(|(_, r)| r.len() != header.len()) {
            return Err(invalid(fill(
                tr("Row {} of the CSV file has a different number of fields than the header."),
                &[line],
            )));
        }
        Ok(Self { columns, rows })
    }
    fn get<'a>(&self, row: &'a [String], name: &str) -> &'a str {
        self.columns
            .get(&name.to_lowercase())
            .map_or("", |i| row[*i].as_str())
    }
}

fn row_error(line: usize, what: &str) -> io::Error {
    invalid(fill(tr("Row {}: {}"), &[&line, &what]))
}

fn flag(text: &str, line: usize, column: &str) -> io::Result<bool> {
    match text.trim().to_lowercase().as_str() {
        "" | "0" | "false" | "no" => Ok(false),
        "1" | "true" | "yes" => Ok(true),
        _ => Err(row_error(
            line,
            &fill(tr("“{}” must be 0 or 1."), &[&column]),
        )),
    }
}

fn options(t: &Table, row: &[String], line: usize) -> io::Result<QueryOptions> {
    let mut on = [false; 9];
    for (i, name) in TOGGLES.iter().enumerate() {
        on[i] = flag(t.get(row, name), line, name)?;
    }
    let hidden = t.get(row, "Hidden Files").trim();
    Ok(QueryOptions {
        case_sensitive: on[0],
        whole_words: on[1],
        match_path: on[2],
        match_diacritics: on[3],
        prefix: on[4],
        suffix: on[5],
        ignore_punctuation: on[6],
        ignore_whitespace: on[7],
        regex: on[8],
        and_before_or: flag(t.get(row, "And Before Or"), line, "And Before Or")?,
        literal_operators: flag(t.get(row, "Literal Operators"), line, "Literal Operators")?,
        wildcards_anywhere: flag(t.get(row, "Wildcards Anywhere"), line, "Wildcards Anywhere")?,
        hidden_files: if hidden.is_empty() {
            HiddenFiles::Show
        } else {
            HiddenFiles::from_id(hidden)
                .ok_or_else(|| row_error(line, tr("Unknown hidden files mode.")))?
        },
        filter: Filter::All,
    })
}

fn sort(text: &str, line: usize) -> io::Result<Option<i32>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let column = SORT_NAMES
        .iter()
        .position(|n| n.eq_ignore_ascii_case(text))
        .map(|c| c as i32)
        .or_else(|| text.parse().ok())
        .filter(|c| crate::is_sort_column(*c))
        .ok_or_else(|| row_error(line, tr("Unknown sort column.")))?;
    Ok(Some(column))
}

fn columns(text: &str, line: usize) -> io::Result<Option<Vec<String>>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.eq_ignore_ascii_case("standard") {
        return Ok(Some(Vec::new()));
    }
    let names: Vec<String> = text
        .split(';')
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .collect();
    saved_searches::check_columns(Some(&names)).map_err(|e| row_error(line, &e.to_string()))?;
    Ok(Some(names))
}

fn view(text: &str, line: usize) -> io::Result<()> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("details") {
        Ok(())
    } else {
        Err(row_error(line, tr("Only the Details view is available.")))
    }
}

fn key(text: &str, line: usize) -> io::Result<Option<Chord>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    Chord::parse(text)
        .map(Some)
        .ok_or_else(|| row_error(line, tr("Invalid keyboard shortcut.")))
}

fn query_check(search: &str, options: QueryOptions, line: usize) -> io::Result<()> {
    everygrep_core::query::Query::parse(search, options)
        .map(|_| ())
        .map_err(|e| row_error(line, &crate::search_errors::message(&e.to_string())))
}

/// Entries read from a CSV file, not yet in the library.
#[derive(Debug, Default)]
pub struct Incoming {
    filters: Vec<(NamedFilter, Option<Chord>)>,
    bookmarks: Vec<(Bookmark, Option<Chord>, String)>,
}

impl Incoming {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.filters.len() + self.bookmarks.len()
    }
}

pub fn decode_filters(text: &str) -> io::Result<Incoming> {
    let t = Table::parse(text, &["Name", "Search"])?;
    let mut incoming = Incoming::default();
    for (line, row) in &t.rows {
        let line = *line;
        let name = csv::unprotect(t.get(row, "Name")).trim().to_string();
        if name.is_empty() {
            return Err(row_error(line, tr("Enter a name.")));
        }
        let search = csv::unprotect(t.get(row, "Search")).to_string();
        let mut options = options(&t, row, line)?;
        let kind = t.get(row, "Type").trim();
        if !kind.is_empty() {
            options.filter =
                Filter::from_id(kind).ok_or_else(|| row_error(line, tr("Invalid filter kind.")))?;
        }
        query_check(&search, options, line)?;
        view(t.get(row, "View"), line)?;
        let layout = Layout {
            columns: columns(t.get(row, "Columns"), line)?,
            sort: sort(t.get(row, "Sort"), line)?
                .map(|c| flag(t.get(row, "Descending"), line, "Descending").map(|d| (c, d)))
                .transpose()?,
        };
        incoming.filters.push((
            NamedFilter {
                id: 0,
                name,
                search,
                options,
                layout,
            },
            key(t.get(row, "Key"), line)?,
        ));
    }
    Ok(incoming)
}

/// `library` gives the frozen filter of a bookmark whose CSV row names a
/// filter but does not spell it out.
pub fn decode_bookmarks(text: &str, library: &Library) -> io::Result<Incoming> {
    let t = Table::parse(text, &["Name", "Search"])?;
    let mut incoming = Incoming::default();
    for (line, row) in &t.rows {
        let line = *line;
        let name = csv::unprotect(t.get(row, "Name")).trim().to_string();
        let folder = csv::unprotect(t.get(row, "Folder")).trim().to_string();
        let kind = match t.get(row, "Kind").trim().to_lowercase().as_str() {
            "" | "bookmark" => Kind::Search,
            "folder" => Kind::Folder,
            "separator" => Kind::Separator,
            _ => return Err(row_error(line, tr("Unknown row kind."))),
        };
        if kind != Kind::Search {
            if kind == Kind::Folder && (name.is_empty() || !folder.is_empty()) {
                return Err(row_error(line, tr("Invalid bookmark folder.")));
            }
            incoming
                .bookmarks
                .push((Bookmark::group(kind, &name, 0), None, folder));
            continue;
        }
        if name.is_empty() {
            return Err(row_error(line, tr("Enter a name.")));
        }
        let search = csv::unprotect(t.get(row, "Search")).to_string();
        let options = options(&t, row, line)?;
        view(t.get(row, "View"), line)?;
        let filter_name = csv::unprotect(t.get(row, "Filter")).trim().to_string();
        let filter_search = csv::unprotect(t.get(row, "Filter Search")).to_string();
        let filter_type = t.get(row, "Filter Type").trim();
        let filter = if filter_type.is_empty() && filter_search.is_empty() {
            // Only a name: the library's filter of that name, else All.
            library
                .filters
                .iter()
                .find(|f| !filter_name.is_empty() && f.title() == filter_name)
                .cloned()
                .unwrap_or_else(|| Library::default().filters.remove(0))
        } else {
            let bits = t.get(row, "Filter Options").trim();
            let hidden = t.get(row, "Filter Hidden Files").trim();
            let options = saved_searches::decode_options(
                if filter_type.is_empty() {
                    "all"
                } else {
                    filter_type
                },
                if bits.is_empty() { "0" } else { bits },
                if hidden.is_empty() { "show" } else { hidden },
            )
            .map_err(|e| row_error(line, &e.to_string()))?;
            NamedFilter {
                id: 0,
                name: filter_name,
                search: filter_search,
                options,
                layout: Layout::default(),
            }
        };
        let mut options = options;
        options.filter = filter.options.filter;
        query_check(&search, options, line)?;
        query_check(&filter.search, filter.options, line)?;
        let descending = flag(t.get(row, "Descending"), line, "Descending")?;
        let bookmark = Bookmark {
            id: 0,
            name,
            search,
            options,
            filter,
            sort_column: sort(t.get(row, "Sort"), line)?.unwrap_or(0),
            descending,
            columns: columns(t.get(row, "Columns"), line)?,
            kind: Kind::Search,
            folder: 0,
        };
        incoming
            .bookmarks
            .push((bookmark, key(t.get(row, "Key"), line)?, folder));
    }
    Ok(incoming)
}

/// What an import did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub added: usize,
    pub renamed: usize,
    pub skipped: usize,
    pub keys_left_out: usize,
}

impl Report {
    pub fn message(&self) -> String {
        let mut text = fill(
            tr("Imported {} entries ({} renamed, {} already present)."),
            &[&self.added, &self.renamed, &self.skipped],
        );
        if self.keys_left_out > 0 {
            text.push(' ');
            text.push_str(&fill(
                tr("{} keyboard shortcuts were left out because they are taken."),
                &[&self.keys_left_out],
            ));
        }
        text
    }
}

fn same_filter(a: &NamedFilter, b: &NamedFilter) -> bool {
    a.search == b.search && a.options == b.options && a.layout == b.layout
}

fn same_frozen(a: &NamedFilter, b: &NamedFilter) -> bool {
    a.title() == b.title() && a.search == b.search && a.options == b.options
}

fn same_bookmark(a: &Bookmark, b: &Bookmark) -> bool {
    a.search == b.search
        && a.options == b.options
        && same_frozen(&a.filter, &b.filter)
        && a.sort_column == b.sort_column
        && a.descending == b.descending
        && a.columns == b.columns
}

/// Whether `title` is `name` or a renamed copy of it (`name (3)`).
fn named_like(title: &str, name: &str) -> bool {
    title == name
        || title
            .strip_prefix(name)
            .and_then(|r| r.strip_prefix(" ("))
            .and_then(|r| r.strip_suffix(')'))
            .is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n >= 2))
}

/// The first of `name`, `name (2)`, `name (3)` … not in `taken`.
fn free_name(name: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.into();
    }
    (2..)
        .map(|n| format!("{name} ({n})"))
        .find(|n| !taken(n))
        .expect("a free name")
}

/// Adds `incoming` to `library` (see the module comment). Either all of
/// it goes in or, on an error, nothing.
pub fn import(library: &mut Library, incoming: Incoming, keys: &Keymap) -> io::Result<Report> {
    let mut l = library.clone();
    let mut report = Report::default();
    let mut wanted_keys = Vec::new();
    for (filter, key) in incoming.filters {
        if l.filters
            .iter()
            .any(|f| named_like(&f.title(), &filter.name) && same_filter(f, &filter))
        {
            report.skipped += 1;
            continue;
        }
        let name = free_name(&filter.name, &|n| l.filters.iter().any(|f| f.title() == n));
        report.renamed += usize::from(name != filter.name);
        let id = l.put_filter(None, &name, &filter.search, filter.options, filter.layout)?;
        report.added += 1;
        wanted_keys.extend(key.map(|k| (Target::Filter(id), k)));
    }
    // Folders first, so their rows can go in wherever they come.
    let mut folders: HashMap<String, u64> = l
        .bookmarks
        .iter()
        .filter(|b| b.kind == Kind::Folder)
        .map(|b| (b.name.clone(), b.id))
        .collect();
    let mut folder_of = |l: &mut Library, name: &str| -> io::Result<u64> {
        if name.is_empty() {
            return Ok(0);
        }
        if let Some(id) = folders.get(name) {
            return Ok(*id);
        }
        let id = l.put_bookmark(None, Bookmark::group(Kind::Folder, name, 0))?;
        folders.insert(name.into(), id);
        Ok(id)
    };
    let mut new_in: Vec<u64> = Vec::new();
    let mut separators = Vec::new();
    for (mut b, key, folder) in incoming.bookmarks {
        match b.kind {
            Kind::Folder => {
                folder_of(&mut l, &b.name)?;
                continue;
            }
            Kind::Separator => {
                separators.push(folder);
                continue;
            }
            Kind::Search => {}
        }
        let folder = folder_of(&mut l, &folder)?;
        if l.bookmarks.iter().any(|e| {
            e.is_search()
                && named_like(&e.name, &b.name)
                && e.folder == folder
                && same_bookmark(e, &b)
        }) {
            report.skipped += 1;
            continue;
        }
        let name = free_name(&b.name, &|n| {
            l.bookmarks.iter().any(|e| e.is_search() && e.name == n)
        });
        report.renamed += usize::from(name != b.name);
        b.name = name;
        b.folder = folder;
        let id = l.put_bookmark(None, b)?;
        report.added += 1;
        new_in.push(folder);
        wanted_keys.extend(key.map(|k| (Target::Bookmark(id), k)));
    }
    // Separators only go where something new came in, so importing the
    // same file again adds nothing.
    for folder in separators {
        let folder = folder_of(&mut l, &folder)?;
        if new_in.contains(&folder) {
            l.put_bookmark(None, Bookmark::group(Kind::Separator, "", folder))?;
        }
    }
    for (target, key) in wanted_keys {
        if l.set_shortcut(target, Some(key), keys).is_err() {
            report.keys_left_out += 1;
        }
    }
    *library = l;
    Ok(report)
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Library {
        let mut l = Library::default();
        let keys = Keymap::default();
        let id = l
            .put_filter(
                None,
                "Code, \"mine\"",
                "-ext:tmp src\\",
                QueryOptions {
                    case_sensitive: true,
                    regex: false,
                    filter: Filter::Code,
                    hidden_files: HiddenFiles::Hide,
                    ..QueryOptions::default()
                },
                Layout {
                    columns: Some(vec!["type".into(), "md5".into()]),
                    sort: Some((2, true)),
                },
            )
            .unwrap();
        l.set_shortcut(Target::Filter(id), Chord::parse("Ctrl+Shift+5"), &keys)
            .unwrap();
        let folder = l
            .put_bookmark(None, Bookmark::group(Kind::Folder, "Work", 0))
            .unwrap();
        let mut b = Bookmark::group(Kind::Search, "Reports\nQ1", folder);
        b.search = "=report ext:pdf".into();
        b.filter = l.filters[4].clone();
        b.sort_column = 3;
        b.descending = true;
        b.columns = Some(Vec::new());
        b.options.filter = b.filter.options.filter;
        let id = l.put_bookmark(None, b).unwrap();
        l.set_shortcut(Target::Bookmark(id), Chord::parse("Ctrl+Shift+6"), &keys)
            .unwrap();
        l.put_bookmark(None, Bookmark::group(Kind::Separator, "", folder))
            .unwrap();
        let mut top = Bookmark::group(Kind::Search, "Top", 0);
        top.search = "size:>1gb".into();
        l.put_bookmark(None, top).unwrap();
        l
    }

    #[test]
    fn filters_round_trip_through_csv_into_an_empty_library() {
        let source = sample();
        let text = encode_filters(&source);
        assert!(text.starts_with("Name,Case,Whole Word,Path,Diacritics,Prefix,Suffix,"));
        assert!(text.contains("\"Code, \"\"mine\"\"\""));
        assert!(text.contains("'-ext:tmp"));
        let mut dest = Library::default();
        let report = import(
            &mut dest,
            decode_filters(&text).unwrap(),
            &Keymap::default(),
        )
        .unwrap();
        // The nine built-ins are already there.
        assert_eq!(report.added, 1);
        assert_eq!(report.skipped, 9);
        let mine = dest.filters.last().unwrap();
        let orig = source.filters.last().unwrap();
        assert_eq!(
            (&mine.name, &mine.search, mine.options, &mine.layout),
            (&orig.name, &orig.search, orig.options, &orig.layout)
        );
        assert_eq!(
            dest.shortcut(Target::Filter(mine.id)),
            Chord::parse("Ctrl+Shift+5")
        );
        // Importing again changes nothing.
        let before = dest.clone();
        let report = import(
            &mut dest,
            decode_filters(&text).unwrap(),
            &Keymap::default(),
        )
        .unwrap();
        assert_eq!(report.added, 0);
        assert_eq!(dest, before);
    }

    #[test]
    fn bookmarks_round_trip_with_folders_separators_and_keys() {
        let source = sample();
        let text = encode_bookmarks(&source);
        let mut dest = Library::default();
        let incoming = decode_bookmarks(&text, &dest).unwrap();
        let report = import(&mut dest, incoming, &Keymap::default()).unwrap();
        assert_eq!(report.added, 2);
        assert_eq!(encode_bookmarks(&dest), text);
        let again = decode_bookmarks(&text, &dest).unwrap();
        let before = dest.clone();
        let report = import(&mut dest, again, &Keymap::default()).unwrap();
        assert_eq!((report.added, report.skipped), (0, 2));
        assert_eq!(dest, before);
    }

    #[test]
    fn import_never_overwrites_a_different_entry_of_the_same_name() {
        let mut dest = sample();
        let before_filters = dest.filters.clone();
        let text = "Name,Search,Key\r\n\"Code, \"\"mine\"\"\",ext:rs,Ctrl+Shift+5\r\n";
        let report = import(&mut dest, decode_filters(text).unwrap(), &Keymap::default()).unwrap();
        assert_eq!(report.renamed, 1);
        assert_eq!(report.keys_left_out, 1);
        assert_eq!(dest.filters[..before_filters.len()], before_filters[..]);
        assert_eq!(dest.filters.last().unwrap().name, "Code, \"mine\" (2)");
        let report = import(&mut dest, decode_filters(text).unwrap(), &Keymap::default()).unwrap();
        assert_eq!((report.added, report.skipped), (0, 1));
        let text = "Name,Search\r\nTop,ext:pdf\r\n";
        let incoming = decode_bookmarks(text, &dest).unwrap();
        import(&mut dest, incoming, &Keymap::default()).unwrap();
        let names: Vec<_> = dest
            .bookmarks
            .iter()
            .filter(|b| b.is_search())
            .map(|b| b.name.as_str())
            .collect();
        assert!(names.contains(&"Top") && names.contains(&"Top (2)"));
    }

    #[test]
    fn malformed_or_invalid_csv_fails_with_a_clear_reason_and_changes_nothing() {
        let cases = [
            ("", "empty"),
            ("Name,Search\r\n\"open,x\r\n", "line 2"),
            ("Name\r\nx\r\n", "Search"),
            ("Name,Search,Name\r\na,b,c\r\n", "twice"),
            ("Name,Search\r\na,b,c\r\n", "Row 2"),
            ("Name,Search,Case\r\na,b,maybe\r\n", "Row 2"),
            ("Name,Search\r\nok,ext:md\r\n,ext:md\r\n", "Row 3"),
            ("Name,Search\r\nbad,regex:(\r\n", "Row 2"),
            ("Name,Search,View\r\na,b,Thumbnails\r\n", "Details"),
            ("Name,Search,Columns\r\na,b,nosuch\r\n", "Row 2"),
            ("Name,Search,Sort\r\na,b,sideways\r\n", "Row 2"),
            ("Name,Search,Key\r\na,b,Ctrl+Nope\r\n", "Row 2"),
        ];
        for (text, needle) in cases {
            let error = decode_filters(text).unwrap_err().to_string();
            assert!(error.contains(needle), "{text:?}: {error}");
        }
        let error = decode_bookmarks("Name,Search,Kind\r\na,b,shelf\r\n", &Library::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Row 2"), "{error}");
    }

    #[test]
    fn foreign_columns_in_any_order_are_read_by_name() {
        let text = "search,REGEX,name,Extra\nfoo.*,1,Regex search,ignored\n";
        let incoming = decode_filters(text).unwrap();
        assert_eq!(incoming.len(), 1);
        let mut dest = Library::default();
        import(&mut dest, incoming, &Keymap::default()).unwrap();
        let f = dest.filters.last().unwrap();
        assert!(f.options.regex);
        assert_eq!(f.search, "foo.*");
        assert_eq!(f.layout, Layout::default());
    }
}
