//! Named filters and their private, locked, atomic storage.
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::commands::{self, Chord, Keymap, Scope};
use crate::i18n::{fill, tr};
use crate::{labels, settings};
use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, Query, QueryOptions};

const HEADER: &str = "EveryGrep saved searches 2";
/// The first format: no layouts, folders or separators. Still read.
const HEADER_V1: &str = "EveryGrep saved searches 1";

/// How a filter shapes the result list when it is chosen. `None` keeps
/// what the window shows, so a filter changes nothing unless asked to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// The optional columns to show, by name (see `session::column_names`).
    pub columns: Option<Vec<String>>,
    /// Sort column and direction.
    pub sort: Option<(i32, bool)>,
}

/// What a row of the bookmark list is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Search,
    /// A folder groups the bookmarks whose `folder` is its id; one level.
    Folder,
    Separator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedFilter {
    pub id: u64,
    // Empty names on built-ins follow the interface language.
    pub name: String,
    pub search: String,
    pub options: QueryOptions,
    pub layout: Layout,
}

impl NamedFilter {
    pub fn manager_title(&self) -> String {
        if Query::parse(&self.search, self.options).is_err() {
            fill(tr("{} (invalid query)"), &[&self.title()])
        } else {
            self.title()
        }
    }
    pub fn title(&self) -> String {
        if self.name.is_empty() {
            labels::filter(self.options.filter).into()
        } else {
            self.name.clone()
        }
    }
}

/// A bookmark freezes both expressions, so editing or deleting a filter
/// later cannot silently change the saved search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub id: u64,
    pub name: String,
    pub search: String,
    pub options: QueryOptions,
    pub filter: NamedFilter,
    pub sort_column: i32,
    pub descending: bool,
    /// Optional columns to show; `None` keeps the window's columns.
    pub columns: Option<Vec<String>>,
    pub kind: Kind,
    /// Id of the folder holding this entry, 0 for the top level.
    pub folder: u64,
}

impl Bookmark {
    /// A folder or separator row; it has no search of its own.
    pub fn group(kind: Kind, name: &str, folder: u64) -> Self {
        Self {
            id: 0,
            name: name.into(),
            search: String::new(),
            options: QueryOptions::default(),
            filter: defaults().remove(0),
            sort_column: 0,
            descending: false,
            columns: None,
            kind,
            folder,
        }
    }
    pub fn is_search(&self) -> bool {
        self.kind == Kind::Search
    }
    pub fn manager_title(&self) -> String {
        match self.kind {
            Kind::Folder => return fill(tr("{} (folder)"), &[&self.name]),
            Kind::Separator => return tr("Separator").into(),
            Kind::Search => {}
        }
        if Query::parse(&self.search, self.options).is_err()
            || Query::parse(&self.filter.search, self.filter.options).is_err()
        {
            fill(tr("{} (invalid query)"), &[&self.name])
        } else {
            self.name.clone()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Filter(u64),
    Bookmark(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    pub target: Target,
    pub chord: Chord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    pub filters: Vec<NamedFilter>,
    pub bookmarks: Vec<Bookmark>,
    pub shortcuts: Vec<Shortcut>,
    next_id: u64,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            filters: defaults(),
            bookmarks: Vec::new(),
            shortcuts: Vec::new(),
            next_id: 10,
        }
    }
}

fn defaults() -> Vec<NamedFilter> {
    Filter::ALL
        .into_iter()
        .enumerate()
        .map(|(i, filter)| NamedFilter {
            id: i as u64 + 1,
            name: String::new(),
            search: String::new(),
            options: QueryOptions {
                filter,
                ..QueryOptions::default()
            },
            layout: Layout::default(),
        })
        .collect()
}

impl Library {
    /// Built-in requests retain their original meaning even after the user
    /// edits or deletes the corresponding library entry.
    pub fn builtin_selection(&self, filter: Filter) -> (NamedFilter, Option<usize>) {
        let original = defaults()
            .into_iter()
            .find(|f| f.options.filter == filter)
            .expect("built-in filter");
        let index = self.filters.iter().position(|f| *f == original);
        (original, index)
    }

    /// Restoring replaces the built-ins and their order, keeping user entries.
    pub fn restore_filters(&mut self) {
        self.shortcuts
            .retain(|s| !matches!(s.target, Target::Filter(1..=9)));
        self.filters.retain(|f| f.id > 9);
        self.filters.splice(..0, defaults());
    }

    /// None adds a fresh identity. Editing a missing entry never recreates it.
    pub fn put_filter(
        &mut self,
        id: Option<u64>,
        name: &str,
        search: &str,
        options: QueryOptions,
        layout: Layout,
    ) -> io::Result<u64> {
        if name.trim().is_empty() {
            return Err(invalid(tr("Enter a name.")));
        }
        Query::parse(search, options)
            .map_err(|e| invalid(&crate::search_errors::message(&e.to_string())))?;
        check_layout(&layout)?;
        let id = match id {
            Some(id) => {
                if !self.filters.iter().any(|f| f.id == id) {
                    return Err(missing());
                }
                id
            }
            None => {
                let id = self.next_id;
                self.next_id = id
                    .checked_add(1)
                    .ok_or_else(|| invalid(tr("Too many saved searches.")))?;
                id
            }
        };
        let entry = NamedFilter {
            id,
            name: name.trim().into(),
            search: search.into(),
            options,
            layout,
        };
        if let Some(old) = self.filters.iter_mut().find(|f| f.id == id) {
            *old = entry;
        } else {
            self.filters.push(entry);
        }
        Ok(id)
    }

    pub fn remove_filter(&mut self, id: u64) -> io::Result<()> {
        let i = self
            .filters
            .iter()
            .position(|f| f.id == id)
            .ok_or_else(missing)?;
        self.shortcuts.retain(|s| s.target != Target::Filter(id));
        self.filters.remove(i);
        Ok(())
    }

    pub fn move_filter(&mut self, id: u64, step: i32) -> io::Result<()> {
        move_entry(&mut self.filters, |f| f.id == id, step)
    }

    pub fn put_bookmark(&mut self, id: Option<u64>, mut entry: Bookmark) -> io::Result<u64> {
        if entry.kind == Kind::Separator {
            entry.name.clear();
        } else if entry.name.trim().is_empty() {
            return Err(invalid(tr("Enter a name.")));
        }
        if entry.kind == Kind::Search {
            if !crate::is_sort_column(entry.sort_column) {
                return Err(invalid(tr("Invalid bookmark.")));
            }
            Query::parse(&entry.search, entry.options)
                .map_err(|e| invalid(&crate::search_errors::message(&e.to_string())))?;
            Query::parse(&entry.filter.search, entry.filter.options)
                .map_err(|e| invalid(&crate::search_errors::message(&e.to_string())))?;
            check_columns(entry.columns.as_deref())?;
        }
        // Folders hold one level only.
        if entry.folder != 0
            && (entry.kind == Kind::Folder
                || !self
                    .bookmarks
                    .iter()
                    .any(|b| b.id == entry.folder && b.kind == Kind::Folder))
        {
            return Err(invalid(tr("Invalid bookmark folder.")));
        }
        entry.name = entry.name.trim().into();
        entry.id = if let Some(id) = id {
            let Some(old) = self.bookmarks.iter().find(|b| b.id == id) else {
                return Err(missing());
            };
            if old.kind != entry.kind {
                return Err(invalid(tr("Invalid bookmark.")));
            }
            id
        } else {
            let id = self.next_id;
            self.next_id = id
                .checked_add(1)
                .ok_or_else(|| invalid(tr("Too many saved searches.")))?;
            id
        };
        let id = entry.id;
        if let Some(old) = self.bookmarks.iter_mut().find(|b| b.id == id) {
            *old = entry;
        } else {
            self.bookmarks.push(entry);
        }
        Ok(id)
    }

    /// Removing a folder keeps its bookmarks: they move to the top level,
    /// in the folder's place.
    pub fn remove_bookmark(&mut self, id: u64) -> io::Result<()> {
        let i = self
            .bookmarks
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(missing)?;
        self.shortcuts.retain(|s| s.target != Target::Bookmark(id));
        let removed = self.bookmarks.remove(i);
        if removed.kind == Kind::Folder {
            let (children, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.bookmarks)
                .into_iter()
                .partition(|b| b.folder == id);
            self.bookmarks = rest;
            let at = i.min(self.bookmarks.len());
            self.bookmarks.splice(
                at..at,
                children.into_iter().map(|mut b| {
                    b.folder = 0;
                    b
                }),
            );
        }
        Ok(())
    }

    /// Moves an entry past its previous or next sibling (same folder).
    pub fn move_bookmark(&mut self, id: u64, step: i32) -> io::Result<()> {
        let i = self
            .bookmarks
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(missing)?;
        let folder = self.bookmarks[i].folder;
        let sibling = if step == -1 {
            self.bookmarks[..i].iter().rposition(|b| b.folder == folder)
        } else if step == 1 {
            self.bookmarks[i + 1..]
                .iter()
                .position(|b| b.folder == folder)
                .map(|n| i + 1 + n)
        } else {
            None
        };
        if let Some(j) = sibling {
            self.bookmarks.swap(i, j);
        }
        Ok(())
    }

    /// The bookmark list as the manager and the menu show it: top-level
    /// rows in order, each folder followed by its own rows (depth 1).
    pub fn bookmark_tree(&self) -> Vec<(usize, u8)> {
        let mut rows = Vec::new();
        for (i, b) in self.bookmarks.iter().enumerate() {
            if b.folder != 0 {
                continue;
            }
            rows.push((i, 0));
            if b.kind == Kind::Folder {
                rows.extend(
                    self.bookmarks
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.folder == b.id)
                        .map(|(j, _)| (j, 1)),
                );
            }
        }
        rows
    }

    /// The folders, for choosing where a bookmark goes.
    /// The searches inside the folder at `index` of the list, in their
    /// order; none when `index` is not a folder.
    pub fn folder_searches(&self, index: usize) -> Vec<&Bookmark> {
        let Some(folder) = self.bookmarks.get(index).filter(|b| b.kind == Kind::Folder) else {
            return Vec::new();
        };
        self.bookmarks
            .iter()
            .filter(|b| b.is_search() && b.folder == folder.id)
            .collect()
    }

    pub fn bookmark_folders(&self) -> Vec<&Bookmark> {
        self.bookmarks
            .iter()
            .filter(|b| b.kind == Kind::Folder)
            .collect()
    }

    pub fn shortcut(&self, target: Target) -> Option<Chord> {
        self.shortcuts
            .iter()
            .find(|s| s.target == target)
            .map(|s| s.chord)
    }

    pub fn shortcut_owner(&self, except: Option<Target>, chord: Chord) -> Option<String> {
        self.shortcuts
            .iter()
            .find(|s| Some(s.target) != except && s.chord == chord)
            .and_then(|s| self.target_name(s.target))
    }

    pub fn target_name(&self, target: Target) -> Option<String> {
        match target {
            Target::Filter(id) => self
                .filters
                .iter()
                .find(|f| f.id == id)
                .map(NamedFilter::title),
            Target::Bookmark(id) => self
                .bookmarks
                .iter()
                .find(|b| b.id == id && b.is_search())
                .map(|b| b.name.clone()),
        }
    }

    /// Global item keys never steal an active or default command key, in any
    /// scope. Reserving defaults also makes Restore keyboard defaults safe.
    pub fn set_shortcut(
        &mut self,
        target: Target,
        chord: Option<Chord>,
        keymap: &Keymap,
    ) -> io::Result<()> {
        if self.target_name(target).is_none() {
            return Err(missing());
        }
        if let Some(chord) = chord {
            if let Some(reason) = commands::refusal(Scope::Global, chord) {
                return Err(invalid(reason));
            }
            let defaults = Keymap::default();
            if let Some(binding) = keymap
                .bindings()
                .iter()
                .chain(defaults.bindings())
                .find(|b| b.chord == chord)
            {
                let owner = commands::spec(binding.cmd).map_or("", |s| (s.label)());
                return Err(invalid(&fill(
                    tr("Shortcut already used by {}."),
                    &[&owner],
                )));
            }
            if let Some(owner) = self.shortcut_owner(Some(target), chord) {
                return Err(invalid(&fill(
                    tr("Shortcut already used by {}."),
                    &[&owner],
                )));
            }
        }
        self.shortcuts.retain(|s| s.target != target);
        if let Some(chord) = chord {
            self.shortcuts.push(Shortcut { target, chord });
        }
        Ok(())
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> io::Result<Self> {
        let mut lines = text.lines();
        let v2 = match lines.next() {
            Some(HEADER) => true,
            Some(HEADER_V1) => false,
            _ => return Err(invalid(tr("Invalid saved searches header."))),
        };
        let next_id = lines
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n >= 10)
            .ok_or_else(|| invalid(tr("Invalid saved search identity.")))?;
        let mut library = Self {
            filters: Vec::new(),
            bookmarks: Vec::new(),
            shortcuts: Vec::new(),
            next_id,
        };
        let fresh = |library: &Self, id: &str, min: u64| -> io::Result<u64> {
            let id = id
                .parse::<u64>()
                .ok()
                .filter(|id| *id >= min && *id < next_id)
                .ok_or_else(|| invalid(tr("Invalid saved search identity.")))?;
            if library.bookmarks.iter().any(|b| b.id == id)
                || library.filters.iter().any(|f| f.id == id)
            {
                return Err(invalid(tr("Invalid saved search identity.")));
            }
            Ok(id)
        };
        for line in lines {
            let parts: Vec<_> = line.split('\t').collect();
            match parts.first().copied() {
                Some("K") => {
                    if parts.len() != 4 {
                        return Err(invalid(tr("Invalid saved shortcut.")));
                    }
                    let id = parts[2]
                        .parse()
                        .map_err(|_| invalid(tr("Invalid saved search identity.")))?;
                    let target = match parts[1] {
                        "F" => Target::Filter(id),
                        "B" => Target::Bookmark(id),
                        _ => return Err(invalid(tr("Invalid saved shortcut."))),
                    };
                    let chord = Chord::parse(parts[3])
                        .ok_or_else(|| invalid(tr("Invalid saved shortcut.")))?;
                    if commands::refusal(Scope::Global, chord).is_some()
                        || library
                            .shortcuts
                            .iter()
                            .any(|s| s.target == target || s.chord == chord)
                    {
                        return Err(invalid(tr("Invalid saved shortcut.")));
                    }
                    library.shortcuts.push(Shortcut { target, chord });
                }
                Some("G") if v2 => {
                    if parts.len() != 3 {
                        return Err(invalid(tr("Invalid saved search record.")));
                    }
                    let id = fresh(&library, parts[1], 10)?;
                    let name = decode(parts[2])?;
                    if name.trim().is_empty() {
                        return Err(invalid(tr("Enter a name.")));
                    }
                    let mut entry = Bookmark::group(Kind::Folder, &name, 0);
                    entry.id = id;
                    library.bookmarks.push(entry);
                }
                Some("S") if v2 => {
                    if parts.len() != 3 {
                        return Err(invalid(tr("Invalid saved search record.")));
                    }
                    let id = fresh(&library, parts[1], 10)?;
                    let folder = parts[2]
                        .parse()
                        .map_err(|_| invalid(tr("Invalid bookmark folder.")))?;
                    let mut entry = Bookmark::group(Kind::Separator, "", folder);
                    entry.id = id;
                    library.bookmarks.push(entry);
                }
                Some("B") => {
                    if parts.len() != if v2 { 17 } else { 15 } {
                        return Err(invalid(tr("Invalid saved search record.")));
                    }
                    let id = fresh(&library, parts[1], 10)?;
                    let filter = NamedFilter {
                        id: parts[9]
                            .parse()
                            .map_err(|_| invalid(tr("Invalid saved search identity.")))?,
                        name: decode(parts[10])?,
                        search: decode(parts[11])?,
                        options: decode_options(parts[12], parts[13], parts[14])?,
                        layout: Layout::default(),
                    };
                    let (columns, folder) = if v2 {
                        (
                            decode_columns(parts[15])?,
                            parts[16]
                                .parse()
                                .map_err(|_| invalid(tr("Invalid bookmark folder.")))?,
                        )
                    } else {
                        (None, 0)
                    };
                    let entry = Bookmark {
                        id,
                        name: decode(parts[2])?,
                        search: decode(parts[3])?,
                        options: decode_options(parts[4], parts[5], parts[6])?,
                        sort_column: parts[7]
                            .parse()
                            .map_err(|_| invalid(tr("Invalid saved search sort order.")))?,
                        descending: decode_flag(parts[8])?,
                        filter,
                        columns,
                        kind: Kind::Search,
                        folder,
                    };
                    // Keep structurally sound records editable across parser changes.
                    if entry.name.trim().is_empty() || !crate::is_sort_column(entry.sort_column) {
                        return Err(invalid(tr("Invalid bookmark.")));
                    }
                    library.bookmarks.push(entry);
                }
                Some("F") if parts.len() == if v2 { 10 } else { 7 } => {
                    let id = fresh(&library, parts[1], 1)?;
                    let name = decode(parts[2])?;
                    if name.trim().is_empty() && id > 9 {
                        return Err(invalid(tr("Enter a name.")));
                    }
                    let search = decode(parts[3])?;
                    let options = decode_options(parts[4], parts[5], parts[6])?;
                    let layout = if v2 {
                        Layout {
                            columns: decode_columns(parts[7])?,
                            sort: decode_sort(parts[8], parts[9])?,
                        }
                    } else {
                        Layout::default()
                    };
                    library.filters.push(NamedFilter {
                        id,
                        name,
                        search,
                        options,
                        layout,
                    });
                }
                _ => return Err(invalid(tr("Invalid saved search record."))),
            }
        }
        // Folders hold bookmarks and separators, one level deep.
        if library.bookmarks.iter().any(|b| {
            b.folder != 0
                && (b.kind == Kind::Folder
                    || !library
                        .bookmarks
                        .iter()
                        .any(|f| f.id == b.folder && f.kind == Kind::Folder))
        }) {
            return Err(invalid(tr("Invalid bookmark folder.")));
        }
        if library
            .shortcuts
            .iter()
            .any(|s| library.target_name(s.target).is_none())
        {
            return Err(invalid(tr("Invalid saved shortcut.")));
        }
        Ok(library)
    }

    /// Merge portable records with fresh identities; snapshots stay self-contained.
    /// Existing equivalent entries are reused, making repeated imports harmless.
    pub fn merge_import(&mut self, incoming: &Self) -> io::Result<()> {
        let mut merged = self.clone();
        let mut ids = std::collections::HashMap::new();
        for filter in &incoming.filters {
            let mut filter = filter.clone();
            filter.name = filter.name.trim().into();
            Query::parse(&filter.search, filter.options)
                .map_err(|e| invalid(&crate::search_errors::message(&e.to_string())))?;
            let id = if let Some(same) = merged.filters.iter().find(|f| {
                f.name == filter.name
                    && f.search == filter.search
                    && f.options == filter.options
                    && f.layout == filter.layout
            }) {
                same.id
            } else if filter.id <= 9 {
                merged.filters.retain(|f| f.id != filter.id);
                merged.filters.push(filter.clone());
                filter.id
            } else {
                merged.put_filter(
                    None,
                    &filter.name,
                    &filter.search,
                    filter.options,
                    filter.layout.clone(),
                )?
            };
            ids.insert(filter.id, id);
        }
        let mut folders = std::collections::HashMap::new();
        for bookmark in &incoming.bookmarks {
            let mut bookmark = bookmark.clone();
            bookmark.name = bookmark.name.trim().into();
            if let Some(id) = ids.get(&bookmark.filter.id) {
                bookmark.filter.id = *id;
            }
            bookmark.folder = if bookmark.folder == 0 {
                0
            } else {
                *folders
                    .get(&bookmark.folder)
                    .ok_or_else(|| invalid(tr("Invalid bookmark folder.")))?
            };
            if bookmark.kind == Kind::Folder {
                // Folders of the same name merge, so their bookmarks meet.
                let id = match merged
                    .bookmarks
                    .iter()
                    .find(|b| b.kind == Kind::Folder && b.name == bookmark.name)
                {
                    Some(same) => same.id,
                    None => merged.put_bookmark(None, bookmark.clone())?,
                };
                folders.insert(bookmark.id, id);
                continue;
            }
            let equivalent = bookmark.kind == Kind::Search
                && merged.bookmarks.iter().any(|b| {
                    b.kind == Kind::Search
                        && b.name == bookmark.name
                        && b.search == bookmark.search
                        && b.options == bookmark.options
                        && b.filter == bookmark.filter
                        && b.sort_column == bookmark.sort_column
                        && b.descending == bookmark.descending
                        && b.columns == bookmark.columns
                });
            if !equivalent {
                merged.put_bookmark(None, bookmark)?;
            }
        }
        *self = merged;
        Ok(())
    }
    pub fn portable(&self, filters: bool, bookmarks: bool) -> Self {
        Self {
            filters: if filters {
                self.filters.clone()
            } else {
                Vec::new()
            },
            bookmarks: if bookmarks {
                self.bookmarks.clone()
            } else {
                Vec::new()
            },
            shortcuts: Vec::new(),
            next_id: self.next_id,
        }
    }
    pub fn text(&self) -> String {
        let mut text = format!("{HEADER}\n{}\n", self.next_id);
        for f in &self.filters {
            let (sort, descending) = match f.layout.sort {
                Some((column, descending)) => (column.to_string(), descending),
                None => ("-".to_string(), false),
            };
            text.push_str(&format!(
                "F\t{}\t{}\t{}\t{}\t{}\t{sort}\t{}\n",
                f.id,
                settings::escape(f.name.as_bytes()),
                settings::escape(f.search.as_bytes()),
                encode_options(f.options),
                encode_columns(f.layout.columns.as_deref()),
                u8::from(descending),
            ));
        }
        for b in &self.bookmarks {
            match b.kind {
                Kind::Folder => {
                    text.push_str(&format!(
                        "G\t{}\t{}\n",
                        b.id,
                        settings::escape(b.name.as_bytes())
                    ));
                    continue;
                }
                Kind::Separator => {
                    text.push_str(&format!("S\t{}\t{}\n", b.id, b.folder));
                    continue;
                }
                Kind::Search => {}
            }
            let f = &b.filter;
            text.push_str(&format!(
                "B\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                b.id,
                settings::escape(b.name.as_bytes()),
                settings::escape(b.search.as_bytes()),
                encode_options(b.options),
                b.sort_column,
                u8::from(b.descending),
                f.id,
                settings::escape(f.name.as_bytes()),
                settings::escape(f.search.as_bytes()),
                encode_options(f.options),
                encode_columns(b.columns.as_deref()),
                b.folder,
            ));
        }
        for s in &self.shortcuts {
            let (kind, id) = match s.target {
                Target::Filter(id) => ("F", id),
                Target::Bookmark(id) => ("B", id),
            };
            text.push_str(&format!("K\t{kind}\t{id}\t{}\n", s.chord));
        }
        text
    }
}

/// `-` keeps the window's columns; otherwise the names, comma-separated
/// (empty for no optional column).
fn encode_columns(columns: Option<&[String]>) -> String {
    columns.map_or_else(|| "-".into(), |c| c.join(","))
}

pub(crate) fn decode_columns(text: &str) -> io::Result<Option<Vec<String>>> {
    if text == "-" {
        return Ok(None);
    }
    let names: Vec<String> = text
        .split(',')
        .filter(|n| !n.is_empty())
        .map(String::from)
        .collect();
    check_columns(Some(&names))?;
    Ok(Some(names))
}

pub(crate) fn check_columns(columns: Option<&[String]>) -> io::Result<()> {
    let known = crate::session::column_names();
    match columns {
        Some(names) if names.iter().any(|n| !known.contains(&n.as_str())) => {
            Err(invalid(tr("Invalid column name.")))
        }
        _ => Ok(()),
    }
}

fn check_layout(layout: &Layout) -> io::Result<()> {
    check_columns(layout.columns.as_deref())?;
    if layout.sort.is_some_and(|(c, _)| !crate::is_sort_column(c)) {
        return Err(invalid(tr("Invalid saved search sort order.")));
    }
    Ok(())
}

fn decode_flag(text: &str) -> io::Result<bool> {
    match text {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(invalid(tr("Invalid saved search sort order."))),
    }
}

fn decode_sort(column: &str, descending: &str) -> io::Result<Option<(i32, bool)>> {
    let descending = decode_flag(descending)?;
    if column == "-" {
        return Ok(None);
    }
    let column = column
        .parse::<i32>()
        .ok()
        .filter(|c| crate::is_sort_column(*c))
        .ok_or_else(|| invalid(tr("Invalid saved search sort order.")))?;
    Ok(Some((column, descending)))
}

/// The download folder of the desktop (`XDG_DOWNLOAD_DIR` in
/// `user-dirs.dirs`), else `~/Downloads`.
pub fn download_dir(config: Option<&Path>, home: &Path) -> std::path::PathBuf {
    let from_file = config
        .and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok())
        .and_then(|text| {
            text.lines().find_map(|line| {
                let value = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=")?;
                let value = value.strip_prefix('"')?.strip_suffix('"')?;
                let path = match value.strip_prefix("$HOME") {
                    Some(rest) => home.join(rest.trim_start_matches('/')),
                    None => std::path::PathBuf::from(value),
                };
                (path.is_absolute() && path != home).then_some(path)
            })
        });
    from_file.unwrap_or_else(|| home.join("Downloads"))
}

/// The ready-made searches offered in the Bookmarks menu, by name. The
/// Downloads search is left out when the folder's path cannot be quoted.
pub fn starter_pack(downloads: &Path) -> Vec<Bookmark> {
    let mut entries = vec![
        (
            tr("Files over 1 GB"),
            "file: size:>1gb".to_string(),
            2,
            true,
        ),
        (tr("Changed today"), "modified:today".to_string(), 3, true),
    ];
    let dl = downloads.to_string_lossy();
    if !dl.contains('"') && downloads.to_str().is_some() {
        entries.push((
            tr("Downloads of the last 7 days"),
            format!("path:\"{}/\" modified:last7days", dl.trim_end_matches('/')),
            3,
            true,
        ));
    }
    entries.push((tr("Empty folders"), "folder: children:0".into(), 0, false));
    entries.push((tr("Duplicate photos"), "image: duplicates:".into(), 2, true));
    entries
        .into_iter()
        .map(|(name, search, sort, descending)| {
            let mut b = Bookmark::group(Kind::Search, name, 0);
            b.search = search;
            b.sort_column = sort;
            b.descending = descending;
            b
        })
        .collect()
}

impl Library {
    /// Adds the ready-made searches to their own folder; the ones already
    /// there (same search) are skipped. Returns how many were added.
    pub fn add_starter_pack(&mut self, downloads: &Path) -> io::Result<usize> {
        let name = tr("Ready-made searches");
        let folder = match self
            .bookmarks
            .iter()
            .find(|b| b.kind == Kind::Folder && b.name == name)
        {
            Some(f) => f.id,
            None => self.put_bookmark(None, Bookmark::group(Kind::Folder, name, 0))?,
        };
        let mut added = 0;
        for mut b in starter_pack(downloads) {
            if self
                .bookmarks
                .iter()
                .any(|e| e.is_search() && e.folder == folder && e.search == b.search)
            {
                continue;
            }
            b.folder = folder;
            self.put_bookmark(None, b)?;
            added += 1;
        }
        Ok(added)
    }
}

fn move_entry<T>(entries: &mut [T], matches: impl Fn(&T) -> bool, step: i32) -> io::Result<()> {
    let i = entries.iter().position(matches).ok_or_else(missing)?;
    let next = if step == -1 {
        i.checked_sub(1)
    } else if step == 1 {
        i.checked_add(1)
    } else {
        None
    };
    if let Some(next) = next.filter(|n| *n < entries.len()) {
        entries.swap(i, next);
    }
    Ok(())
}

pub(crate) fn encode_options(o: QueryOptions) -> String {
    let bits = [
        o.case_sensitive,
        o.whole_words,
        o.match_path,
        o.match_diacritics,
        o.prefix,
        o.suffix,
        o.ignore_punctuation,
        o.ignore_whitespace,
        o.regex,
        o.and_before_or,
        o.literal_operators,
        o.wildcards_anywhere,
    ]
    .into_iter()
    .enumerate()
    .fold(0u16, |bits, (i, on)| bits | (u16::from(on) << i));
    format!("{}\t{bits}\t{}", o.filter.id(), o.hidden_files.id())
}

pub(crate) fn decode_options(filter: &str, bits: &str, hidden: &str) -> io::Result<QueryOptions> {
    let filter = Filter::from_id(filter).ok_or_else(|| invalid(tr("Invalid filter kind.")))?;
    let bits = bits
        .parse::<u16>()
        .ok()
        .filter(|b| *b < 4096)
        .ok_or_else(|| invalid(tr("Invalid saved search options.")))?;
    let bit = |i: u32| bits & (1u16 << i) != 0u16;
    Ok(QueryOptions {
        filter,
        hidden_files: HiddenFiles::from_id(hidden)
            .ok_or_else(|| invalid(tr("Invalid saved search options.")))?,
        case_sensitive: bit(0),
        whole_words: bit(1),
        match_path: bit(2),
        match_diacritics: bit(3),
        prefix: bit(4),
        suffix: bit(5),
        ignore_punctuation: bit(6),
        ignore_whitespace: bit(7),
        regex: bit(8),
        and_before_or: bit(9),
        literal_operators: bit(10),
        wildcards_anywhere: bit(11),
    })
}

fn decode(s: &str) -> io::Result<String> {
    settings::unescape(s)
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| invalid(tr("Invalid saved search text.")))
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn missing() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        tr("The entry was removed in another window."),
    )
}

/// All edits reload under one lock, including the identity allocation.
/// Malformed files are never overwritten, even by Restore defaults.
/// The search text with `extra` (a bookmark's search) added as one more
/// condition. Space joins the two as AND; when AND binds tighter than OR
/// (`and_before_or`), a side with `|` is grouped in `<` `>` so the OR stays
/// inside it. An empty side is left out.
pub fn append_search(current: &str, extra: &str, and_before_or: bool) -> String {
    let (current, extra) = (current.trim(), extra.trim());
    if current.is_empty() {
        return extra.to_owned();
    }
    if extra.is_empty() {
        return current.to_owned();
    }
    let side = |text: &str| {
        if and_before_or && text.contains('|') {
            format!("<{text}>")
        } else {
            text.to_owned()
        }
    };
    format!("{} {}", side(current), side(extra))
}

pub fn update<R>(
    path: &Path,
    change: impl FnOnce(&mut Library) -> io::Result<R>,
) -> io::Result<(Library, R)> {
    let _lock = settings::lock(path)?;
    let mut library = Library::load(path)?;
    let result = change(&mut library)?;
    write(path, &library)?;
    Ok((library, result))
}

fn write(path: &Path, library: &Library) -> io::Result<()> {
    let (mut file, tmp) = settings::create_temp(path)?;
    let written = (|| {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(library.text().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    Ok(())
}

/// Explicit recovery only: a private, non-overwriting copy is durable before
/// replacing the unreadable library. A stale dialog cannot reset a repaired file.
pub fn recover(path: &Path) -> io::Result<(Library, std::path::PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;
    let _lock = settings::lock(path)?;
    if Library::load(path).is_ok() {
        return Err(invalid(tr("Saved searches changed. Reopen the manager.")));
    }
    let bytes = std::fs::read(path)?;
    for attempt in 0..100 {
        let backup = path.with_file_name(format!(
            "saved-searches.backup-{}-{}-{attempt}",
            crate::search_history::now(),
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backup);
        let mut file = match file {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        file.write_all(&bytes)?;
        file.sync_all()?;
        let library = Library::default();
        write(path, &library)?;
        return Ok((library, backup));
    }
    Err(invalid(tr("Could not create a saved searches backup.")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_a_search_adds_it_as_one_more_condition() {
        assert_eq!(append_search("report", "ext:pdf", false), "report ext:pdf");
        assert_eq!(append_search("", "ext:pdf", false), "ext:pdf");
        assert_eq!(append_search("  report ", "  ", false), "report");
        // OR binds tighter than AND by default: no group needed.
        assert_eq!(append_search("a|b", "c|d", false), "a|b c|d");
        // With AND first, each side keeps its OR inside a group.
        assert_eq!(append_search("a|b", "c|d", true), "<a|b> <c|d>");
        assert_eq!(append_search("a b", "c|d", true), "a b <c|d>");
        for and_before_or in [false, true] {
            let options = QueryOptions {
                and_before_or,
                ..QueryOptions::default()
            };
            let text = append_search("a|b", "c|d", and_before_or);
            assert!(Query::parse(&text, options).is_ok(), "{text}");
        }
    }
    use crate::settings::tests::Scratch;

    fn bookmark(l: &Library, name: &str) -> Bookmark {
        Bookmark {
            id: 0,
            name: name.into(),
            search: "Report.*".into(),
            options: QueryOptions {
                regex: true,
                case_sensitive: true,
                ..QueryOptions::default()
            },
            filter: l.filters[3].clone(),
            sort_column: 8,
            descending: true,
            columns: None,
            kind: Kind::Search,
            folder: 0,
        }
    }

    #[test]
    fn version_one_files_are_read_and_written_back_as_version_two() {
        let mut l = Library::default();
        l.put_bookmark(None, bookmark(&l, "old")).unwrap();
        let v1 = format!(
            "{HEADER_V1}\n{}\n{}",
            l.next_id,
            l.text()
                .lines()
                .skip(2)
                .map(|line| {
                    let parts: Vec<_> = line.split('\t').collect();
                    match parts[0] {
                        "F" => parts[..7].join("\t"),
                        "B" => parts[..15].join("\t"),
                        _ => line.to_string(),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        );
        let read = Library::parse(&v1).unwrap();
        assert_eq!(read, l);
        assert!(read.text().starts_with(HEADER));
        assert!(Library::parse(&v1.replace(HEADER_V1, "EveryGrep saved searches 3")).is_err());
        // Version 1 had no folders or separators.
        assert!(Library::parse(&format!("{HEADER_V1}\n20\nG\t11\tx\n")).is_err());
    }

    #[test]
    fn filter_layouts_round_trip_and_are_validated() {
        let d = Scratch::new("filter-layout");
        let path = d.0.join("saved-searches");
        let layout = Layout {
            columns: Some(vec!["type".into(), "md5".into()]),
            sort: Some((3, true)),
        };
        let (l, id) = update(&path, |l| {
            l.put_filter(
                None,
                "Docs",
                "ext:pdf",
                QueryOptions::default(),
                layout.clone(),
            )
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap(), l);
        assert_eq!(
            l.filters.iter().find(|f| f.id == id).unwrap().layout,
            layout
        );
        let empty = Layout {
            columns: Some(Vec::new()),
            sort: None,
        };
        let (l, _) = update(&path, |l| {
            l.put_filter(
                Some(id),
                "Docs",
                "ext:pdf",
                QueryOptions::default(),
                empty.clone(),
            )
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap(), l);
        let mut library = l.clone();
        for bad in [
            Layout {
                columns: Some(vec!["nosuch".into()]),
                sort: None,
            },
            Layout {
                columns: None,
                sort: Some((10_000, false)),
            },
        ] {
            assert!(
                library
                    .put_filter(None, "Bad", "", QueryOptions::default(), bad)
                    .is_err()
            );
        }
        let text = l.text().replace("\t\t-\t0\n", "\tnosuch\t-\t0\n");
        assert!(Library::parse(&text).is_err());
    }

    #[test]
    fn folders_and_separators_round_trip_move_and_keep_children_on_delete() {
        let d = Scratch::new("bookmark-folders");
        let path = d.0.join("saved-searches");
        let (l, (folder, inner, sep, top)) = update(&path, |l| {
            let folder = l.put_bookmark(None, Bookmark::group(Kind::Folder, "Work", 0))?;
            let mut b = bookmark(l, "inner");
            b.folder = folder;
            b.columns = Some(vec!["ext".into()]);
            let inner = l.put_bookmark(None, b)?;
            let sep = l.put_bookmark(None, Bookmark::group(Kind::Separator, "x", folder))?;
            let top = l.put_bookmark(None, bookmark(l, "top"))?;
            Ok((folder, inner, sep, top))
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap(), l);
        let ids = |l: &Library| -> Vec<(u64, u8)> {
            l.bookmark_tree()
                .into_iter()
                .map(|(i, d)| (l.bookmarks[i].id, d))
                .collect()
        };
        assert_eq!(ids(&l), [(folder, 0), (inner, 1), (sep, 1), (top, 0)]);
        assert!(
            l.bookmarks
                .iter()
                .find(|b| b.id == sep)
                .unwrap()
                .name
                .is_empty()
        );
        let mut l = l;
        // Moving stays among siblings.
        l.move_bookmark(sep, -1).unwrap();
        assert_eq!(ids(&l)[1..3], [(sep, 1), (inner, 1)]);
        l.move_bookmark(sep, -1).unwrap();
        assert_eq!(ids(&l)[1], (sep, 1));
        l.move_bookmark(top, -1).unwrap();
        assert_eq!(ids(&l)[0], (top, 0));
        // Folders, separators and nesting rules.
        let mut nested = Bookmark::group(Kind::Folder, "Nested", folder);
        assert!(l.put_bookmark(None, nested.clone()).is_err());
        nested.folder = 0;
        nested.name = " ".into();
        assert!(l.put_bookmark(None, nested).is_err());
        let mut lost = bookmark(&l, "lost");
        lost.folder = inner;
        assert!(l.put_bookmark(None, lost).is_err());
        assert!(
            l.set_shortcut(
                Target::Bookmark(folder),
                Chord::parse("Ctrl+Shift+7"),
                &Keymap::default()
            )
            .is_err()
        );
        let mut changed = l.bookmarks.iter().find(|b| b.id == inner).unwrap().clone();
        changed.kind = Kind::Folder;
        assert!(l.put_bookmark(Some(inner), changed).is_err());
        // Deleting the folder keeps its bookmarks at the top level.
        l.remove_bookmark(folder).unwrap();
        assert!(l.bookmarks.iter().all(|b| b.folder == 0));
        assert_eq!(l.bookmarks.len(), 3);
        assert_eq!(Library::parse(&l.text()).unwrap(), l);
        let dangling = format!("{HEADER}\n{}\nS\t11\t99\n", l.next_id);
        assert!(Library::parse(&dangling).is_err());
    }

    #[test]
    fn a_folder_lists_only_its_own_searches() {
        let d = Scratch::new("folder-searches");
        let mut l = Library::default();
        assert_eq!(l.add_starter_pack(&d.0).unwrap(), 5);
        let folder = l
            .bookmarks
            .iter()
            .position(|b| b.kind == Kind::Folder)
            .unwrap();
        let id = l.bookmarks[folder].id;
        l.put_bookmark(None, Bookmark::group(Kind::Separator, "", id))
            .unwrap();
        let mut outside = Bookmark::group(Kind::Search, "Outside", 0);
        outside.search = "x".into();
        l.put_bookmark(None, outside).unwrap();
        let inside = l.folder_searches(folder);
        assert_eq!(inside.len(), 5);
        assert!(inside.iter().all(|b| b.is_search() && b.folder == id));
        // Not a folder, or past the end: nothing.
        let search = l
            .bookmarks
            .iter()
            .position(|b| b.name == "Outside")
            .unwrap();
        assert!(l.folder_searches(search).is_empty());
        assert!(l.folder_searches(l.bookmarks.len()).is_empty());
    }

    #[test]
    fn starter_pack_parses_is_idempotent_and_finds_the_download_folder() {
        let d = Scratch::new("starter");
        let home = d.0.join("home");
        let config = d.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        assert_eq!(download_dir(Some(&config), &home), home.join("Downloads"));
        std::fs::write(
            config.join("user-dirs.dirs"),
            "# x\nXDG_DOWNLOAD_DIR=\"$HOME/Yükləmələr\"\n",
        )
        .unwrap();
        let dl = download_dir(Some(&config), &home);
        assert_eq!(dl, home.join("Yükləmələr"));
        std::fs::write(
            config.join("user-dirs.dirs"),
            "XDG_DOWNLOAD_DIR=\"$HOME/\"\n",
        )
        .unwrap();
        assert_eq!(download_dir(Some(&config), &home), home.join("Downloads"));
        let pack = starter_pack(Path::new("/home/me/My Downloads"));
        assert_eq!(pack.len(), 5);
        for b in &pack {
            Query::parse(&b.search, b.options).unwrap_or_else(|e| panic!("{}: {e}", b.search));
            assert!(crate::is_sort_column(b.sort_column));
        }
        assert!(
            pack[2]
                .search
                .starts_with("path:\"/home/me/My Downloads/\"")
        );
        // Empty = no entry inside in the index, not only 0 bytes of files.
        assert!(pack.iter().any(|b| b.search == "folder: children:0"));
        assert_eq!(starter_pack(Path::new("/home/me/a\"b")).len(), 4);
        let mut l = Library::default();
        assert_eq!(l.add_starter_pack(&dl).unwrap(), 5);
        assert_eq!(l.add_starter_pack(&dl).unwrap(), 0);
        assert_eq!(l.bookmarks.len(), 6);
        assert_eq!(Library::parse(&l.text()).unwrap(), l);
        // A removed entry comes back; the others stay single.
        let id = l.bookmarks[3].id;
        l.remove_bookmark(id).unwrap();
        assert_eq!(l.add_starter_pack(&dl).unwrap(), 1);
        assert_eq!(l.bookmarks.len(), 6);
    }

    #[test]
    fn filter_validation_keeps_the_translated_parser_reason() {
        let mut library = Library::default();
        let error = library
            .put_filter(
                None,
                "Broken",
                "regex:(",
                QueryOptions::default(),
                Layout::default(),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            crate::search_errors::message("bad regular expression: unclosed group")
        );
    }

    #[test]
    fn invalid_bookmark_expressions_do_not_block_other_edits() {
        let dir = Scratch::new("bookmark-query-recovery");
        let path = dir.0.join("saved-searches");
        update(&path, |l| {
            let mut b = bookmark(l, "Repair me");
            b.search = "ext:md".into();
            b.options = QueryOptions::default();
            b.filter.search = "ext:rs".into();
            b.filter.options = QueryOptions::default();
            l.put_bookmark(None, b)
        })
        .unwrap();
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("ext:md", "size:invalid")
            .replace("ext:rs", "modified:invalid");
        std::fs::write(&path, text).unwrap();
        let l = Library::load(&path).unwrap();
        assert_eq!(l.bookmarks.len(), 1);
        assert_ne!(l.bookmarks[0].manager_title(), l.bookmarks[0].name);
        update(&path, |l| {
            l.put_filter(
                Some(1),
                "All",
                "",
                QueryOptions::default(),
                Layout::default(),
            )
        })
        .unwrap();
        let mut b = Library::load(&path).unwrap().bookmarks.remove(0);
        b.search = "ext:md".into();
        b.filter.search = "ext:rs".into();
        update(&path, |l| l.put_bookmark(Some(b.id), b)).unwrap();
        assert_eq!(Library::load(&path).unwrap().bookmarks[0].search, "ext:md");
    }

    #[test]
    fn item_shortcuts_reject_commands_scopes_defaults_and_duplicates() {
        let mut l = Library::default();
        let map = Keymap::with_overrides(&[(
            "file.new-window".into(),
            vec![(Scope::List, Chord::parse("Ctrl+Shift+9").unwrap())],
        )]);
        let target = Target::Filter(1);
        for text in [
            "Ctrl+N",
            "Ctrl+Shift+9",
            "Ctrl+C",
            "F2",
            "Alt+Up",
            "A",
            "Tab",
            "Esc",
        ] {
            assert!(
                l.set_shortcut(target, Chord::parse(text), &map).is_err(),
                "{text}"
            );
        }
        let key = Chord::parse("Ctrl+Shift+1").unwrap();
        l.set_shortcut(target, Some(key), &map).unwrap();
        l.set_shortcut(target, Some(key), &map).unwrap();
        assert_eq!(l.shortcuts.len(), 1);
        assert_eq!(l.shortcut_owner(None, key), Some(l.filters[0].title()));
        assert!(l.set_shortcut(Target::Filter(2), Some(key), &map).is_err());
        let id = l.put_bookmark(None, bookmark(&l, "work")).unwrap();
        assert!(
            l.set_shortcut(Target::Bookmark(id), Some(key), &map)
                .is_err()
        );
        l.set_shortcut(target, None, &map).unwrap();
        l.set_shortcut(Target::Bookmark(id), Some(key), &map)
            .unwrap();
        assert!(l.shortcut_owner(None, key).is_some());
        l.remove_bookmark(id).unwrap();
        assert!(l.shortcut_owner(None, key).is_none());
    }

    #[test]
    fn shortcut_storage_round_trip_and_restore_cleanup() {
        let d = Scratch::new("shortcut-roundtrip");
        let path = d.0.join("saved-searches");
        let map = Keymap::default();
        let (l, ()) = update(&path, |l| {
            let id = l.put_filter(
                None,
                "custom",
                "",
                QueryOptions::default(),
                Layout::default(),
            )?;
            l.set_shortcut(Target::Filter(1), Chord::parse("Ctrl+Shift+1"), &map)?;
            l.set_shortcut(Target::Filter(id), Chord::parse("Ctrl+Shift+2"), &map)?;
            let id = l.put_bookmark(None, bookmark(l, "saved"))?;
            l.set_shortcut(Target::Bookmark(id), Chord::parse("Ctrl+Shift+3"), &map)
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap(), l);
        let (mut l, ()) = update(&path, |l| {
            l.restore_filters();
            Ok(())
        })
        .unwrap();
        assert_eq!(l.shortcuts.len(), 2);
        let id = l.filters.last().unwrap().id;
        l.remove_filter(id).unwrap();
        assert_eq!(l.shortcuts.len(), 1);
        let before = std::fs::read(&path).unwrap();
        assert!(
            update(&path, |l| l.set_shortcut(
                Target::Filter(2),
                Chord::parse("Ctrl+Shift+2"),
                &map
            ))
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn malformed_or_dangling_shortcuts_are_preserved_on_failure() {
        let d = Scratch::new("shortcut-invalid");
        let path = d.0.join("saved-searches");
        let l = Library::default();
        for suffix in [
            "K\tF\t1\tNope\n",
            "K\tF\t99\tCtrl+Shift+1\n",
            "K\tF\t1\tCtrl+Shift+1\nK\tF\t2\tCtrl+Shift+1\n",
            "K\tF\t1\tTab\n",
        ] {
            let text = format!("{}{suffix}", l.text());
            std::fs::write(&path, &text).unwrap();
            assert!(update(&path, |_| Ok(())).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn bookmarks_freeze_filters_and_round_trip_all_search_state() {
        let d = Scratch::new("bookmark-state");
        let path = d.0.join("saved-searches");
        let (mut l, id) =
            update(&path, |l| l.put_bookmark(None, bookmark(l, "Work\t\n\\"))).unwrap();
        let saved = l.bookmarks[0].clone();
        l.put_filter(
            Some(4),
            "Changed",
            "other",
            QueryOptions::default(),
            Layout::default(),
        )
        .unwrap();
        assert_eq!(l.bookmarks[0], saved);
        l.remove_filter(4).unwrap();
        assert_eq!(l.bookmarks[0], saved);
        update(&path, |stored| {
            *stored = l.clone();
            Ok(())
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap(), l);
        assert_eq!(l.bookmarks[0].id, id);
        assert!(l.bookmarks[0].options.regex && l.bookmarks[0].descending);
        assert_eq!(l.bookmarks[0].sort_column, 8);
    }

    #[test]
    fn bookmarks_rename_delete_move_and_reject_stale_edits() {
        let mut l = Library::default();
        let first = l.put_bookmark(None, bookmark(&l, "first")).unwrap();
        let second = l.put_bookmark(None, bookmark(&l, "second")).unwrap();
        l.move_bookmark(second, -1).unwrap();
        assert_eq!(l.bookmarks[0].id, second);
        let mut b = l.bookmarks[0].clone();
        b.name = "renamed".into();
        l.put_bookmark(Some(second), b.clone()).unwrap();
        l.move_bookmark(second, 1).unwrap();
        l.move_bookmark(first, -1).unwrap();
        assert_eq!(l.bookmarks[1].name, "renamed");
        l.remove_bookmark(second).unwrap();
        assert!(l.put_bookmark(Some(second), b.clone()).is_err());
        assert!(l.move_bookmark(second, 1).is_err());
        b.name = " ".into();
        assert!(l.put_bookmark(None, b.clone()).is_err());
        b.name = "valid".into();
        b.sort_column = i32::MAX;
        assert!(l.put_bookmark(None, b.clone()).is_err());
        l.restore_filters();
        assert_eq!(l.bookmarks.len(), 1);
    }

    #[test]
    fn bookmarks_keep_every_sort_column_of_the_table() {
        let dir = Scratch::new("bookmark-sorts");
        let path = dir.0.join("saved-searches");
        let mut l = Library::default();
        for column in (-1..crate::columns::COLUMNS as i32 + 4).filter(|c| crate::is_sort_column(*c))
        {
            let mut b = bookmark(&l, &format!("sort {column}"));
            b.sort_column = column;
            l.put_bookmark(None, b).unwrap();
        }
        // The frequency columns (12-14) sort too.
        let sorts: Vec<i32> = l.bookmarks.iter().map(|b| b.sort_column).collect();
        assert_eq!(
            sorts,
            (0..crate::columns::COLUMNS as i32).collect::<Vec<_>>()
        );
        update(&path, |stored| {
            *stored = l.clone();
            Ok(())
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap(), l);
        for column in [i32::MAX, crate::columns::COLUMNS as i32, -1] {
            let mut b = bookmark(&l, "unknown column");
            b.sort_column = column;
            assert!(l.put_bookmark(None, b).is_err(), "{column}");
        }
    }

    #[test]
    fn builtin_requests_reject_modified_or_deleted_library_entries() {
        let mut library = Library::default();
        let (original, index) = library.builtin_selection(Filter::Audio);
        assert_eq!(index, Some(1));
        library.filters[1].options.filter = Filter::Video;
        assert_eq!(
            library.builtin_selection(Filter::Audio),
            (original.clone(), None)
        );
        library.filters.remove(1);
        assert_eq!(library.builtin_selection(Filter::Audio), (original, None));
    }

    #[test]
    fn invalid_queries_remain_editable_without_hiding_other_filters() {
        let dir = Scratch::new("saved-query-recovery");
        let path = dir.0.join("saved-searches");
        update(&path, |l| {
            l.put_filter(
                None,
                "Broken",
                "ext:md",
                QueryOptions::default(),
                Layout::default(),
            )
        })
        .unwrap();
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("ext:md", "size:invalid");
        std::fs::write(&path, text).unwrap();
        let library = Library::load(&path).unwrap();
        assert_eq!(library.filters.len(), 10);
        let broken = &library.filters[9];
        assert!(Query::parse(&broken.search, broken.options).is_err());
        update(&path, |l| {
            l.put_filter(
                Some(broken.id),
                "Repaired",
                "ext:txt",
                broken.options,
                Layout::default(),
            )
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap().filters[9].search, "ext:txt");
    }

    #[test]
    fn explicit_recovery_saves_a_private_copy_and_refuses_stale_dialogs() {
        let dir = Scratch::new("saved-recovery");
        let path = dir.0.join("saved-searches");
        std::fs::write(&path, "malformed\noriginal\n").unwrap();
        let (library, backup) = recover(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "malformed\noriginal\n"
        );
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Library::load(&path).unwrap(), library);
        assert!(recover(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "malformed\noriginal\n"
        );
    }

    #[test]
    fn filter_edit_move_remove_and_restore_keep_custom_entries() {
        let mut l = Library::default();
        let id = l
            .put_filter(
                None,
                "  Notes  ",
                "ext:md",
                QueryOptions::default(),
                Layout::default(),
            )
            .unwrap();
        l.move_filter(id, -1).unwrap();
        assert_eq!(l.filters[8].id, id);
        l.put_filter(
            Some(id),
            "Work",
            "ext:rs",
            QueryOptions::default(),
            Layout::default(),
        )
        .unwrap();
        assert_eq!(l.filters[8].name, "Work");
        l.remove_filter(1).unwrap();
        l.restore_filters();
        assert_eq!(l.filters.len(), 10);
        assert_eq!(l.filters[9].id, id);
        l.move_filter(1, -1).unwrap();
        l.move_filter(id, 1).unwrap();
        assert_eq!(l.filters[9].id, id);
        l.remove_filter(id).unwrap();
        assert!(
            l.put_filter(
                Some(id),
                "missing",
                "",
                QueryOptions::default(),
                Layout::default()
            )
            .is_err()
        );
        assert!(l.move_filter(id, -1).is_err());
        assert!(
            l.put_filter(None, " ", "", QueryOptions::default(), Layout::default())
                .is_err()
        );
        assert!(
            l.put_filter(
                None,
                "bad",
                "size:invalid",
                QueryOptions::default(),
                Layout::default()
            )
            .is_err()
        );
    }

    #[test]
    fn storage_round_trip_options_escapes_and_permissions() {
        let d = Scratch::new("saved-roundtrip");
        let path = d.0.join("saved-searches");
        let options = QueryOptions {
            case_sensitive: true,
            whole_words: true,
            match_path: true,
            match_diacritics: true,
            prefix: true,
            suffix: true,
            ignore_punctuation: true,
            ignore_whitespace: true,
            regex: true,
            and_before_or: true,
            literal_operators: true,
            wildcards_anywhere: true,
            filter: Filter::Code,
            hidden_files: HiddenFiles::Only,
        };
        let (l, _) = update(&path, |l| {
            l.put_filter(None, "Ad\t\n\\", "foo.*", options, Layout::default())
        })
        .unwrap();
        assert_eq!(Library::load(&path).unwrap(), l);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(&d.0).unwrap().count(), 2);
        assert!(decode_options("all", "4096", "show").is_err());
    }

    #[test]
    fn concurrent_edits_reload_and_allocate_unique_ids() {
        let d = Scratch::new("saved-lock");
        let path = d.0.join("saved-searches");
        let jobs: Vec<_> = (0..8)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    update(&p, |l| {
                        l.put_filter(
                            None,
                            &i.to_string(),
                            "",
                            QueryOptions::default(),
                            Layout::default(),
                        )
                    })
                    .unwrap()
                })
            })
            .collect();
        for job in jobs {
            job.join().unwrap();
        }
        let l = Library::load(&path).unwrap();
        assert_eq!(l.filters.len(), 17);
        for (i, f) in l.filters.iter().enumerate() {
            assert!(!l.filters[i + 1..].iter().any(|g| g.id == f.id));
        }
    }

    #[test]
    fn failed_reads_and_failed_edits_preserve_file() {
        let d = Scratch::new("saved-invalid");
        let path = d.0.join("saved-searches");
        for bad in [
            "wrong".into(),
            format!("{HEADER}\n10\nF\t1\tbad\t\\q\tall\t0\tshow\n"),
            format!("{HEADER}\n10\nF\t1\tbad\t\tall\t8192\tshow\n"),
        ] {
            std::fs::write(&path, &bad).unwrap();
            assert!(
                update(&path, |l| {
                    l.restore_filters();
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        }
        std::fs::remove_file(&path).unwrap();
        update(&path, |_| Ok(())).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(update(&path, |l| l.remove_filter(99)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
