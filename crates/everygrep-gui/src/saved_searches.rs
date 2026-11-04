//! Named filters and their private, locked, atomic storage.
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::commands::{self, Chord, Keymap, Scope};
use crate::i18n::{fill, tr};
use crate::{labels, settings};
use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, Query, QueryOptions};

const HEADER: &str = "EveryGrep saved searches 1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedFilter {
    pub id: u64,
    // Empty names on built-ins follow the interface language.
    pub name: String,
    pub search: String,
    pub options: QueryOptions,
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
}

impl Bookmark {
    pub fn manager_title(&self) -> String {
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
    ) -> io::Result<u64> {
        if name.trim().is_empty() {
            return Err(invalid(tr("Enter a name.")));
        }
        Query::parse(search, options)
            .map_err(|e| invalid(&crate::search_errors::message(&e.to_string())))?;
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
        if entry.name.trim().is_empty() || !crate::is_sort_column(entry.sort_column) {
            return Err(invalid(tr("Invalid bookmark.")));
        }
        Query::parse(&entry.search, entry.options)
            .map_err(|e| invalid(&crate::search_errors::message(&e.to_string())))?;
        Query::parse(&entry.filter.search, entry.filter.options)
            .map_err(|e| invalid(&crate::search_errors::message(&e.to_string())))?;
        entry.name = entry.name.trim().into();
        entry.id = if let Some(id) = id {
            if !self.bookmarks.iter().any(|b| b.id == id) {
                return Err(missing());
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

    pub fn remove_bookmark(&mut self, id: u64) -> io::Result<()> {
        let i = self
            .bookmarks
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(missing)?;
        self.shortcuts.retain(|s| s.target != Target::Bookmark(id));
        self.bookmarks.remove(i);
        Ok(())
    }

    pub fn move_bookmark(&mut self, id: u64, step: i32) -> io::Result<()> {
        move_entry(&mut self.bookmarks, |b| b.id == id, step)
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
                .find(|b| b.id == id)
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
        if lines.next() != Some(HEADER) {
            return Err(invalid(tr("Invalid saved searches header.")));
        }
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
        for line in lines {
            let parts: Vec<_> = line.split('\t').collect();
            if parts.first() == Some(&"K") {
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
                let chord =
                    Chord::parse(parts[3]).ok_or_else(|| invalid(tr("Invalid saved shortcut.")))?;
                if commands::refusal(Scope::Global, chord).is_some()
                    || library
                        .shortcuts
                        .iter()
                        .any(|s| s.target == target || s.chord == chord)
                {
                    return Err(invalid(tr("Invalid saved shortcut.")));
                }
                library.shortcuts.push(Shortcut { target, chord });
                continue;
            }
            if parts.first() == Some(&"B") {
                if parts.len() != 15 {
                    return Err(invalid(tr("Invalid saved search record.")));
                }
                let id = parts[1]
                    .parse::<u64>()
                    .ok()
                    .filter(|id| *id >= 10 && *id < next_id)
                    .ok_or_else(|| invalid(tr("Invalid saved search identity.")))?;
                if library.bookmarks.iter().any(|b| b.id == id)
                    || library.filters.iter().any(|f| f.id == id)
                {
                    return Err(invalid(tr("Invalid saved search identity.")));
                }
                let filter = NamedFilter {
                    id: parts[9]
                        .parse()
                        .map_err(|_| invalid(tr("Invalid saved search identity.")))?,
                    name: decode(parts[10])?,
                    search: decode(parts[11])?,
                    options: decode_options(parts[12], parts[13], parts[14])?,
                };
                let entry = Bookmark {
                    id,
                    name: decode(parts[2])?,
                    search: decode(parts[3])?,
                    options: decode_options(parts[4], parts[5], parts[6])?,
                    sort_column: parts[7]
                        .parse()
                        .map_err(|_| invalid(tr("Invalid saved search sort order.")))?,
                    descending: match parts[8] {
                        "0" => false,
                        "1" => true,
                        _ => return Err(invalid(tr("Invalid saved search sort order."))),
                    },
                    filter,
                };
                // Keep structurally sound records editable across parser changes.
                if entry.name.trim().is_empty() || !crate::is_sort_column(entry.sort_column) {
                    return Err(invalid(tr("Invalid bookmark.")));
                }
                library.bookmarks.push(entry);
                continue;
            }
            if parts.len() != 7 || parts[0] != "F" {
                return Err(invalid(tr("Invalid saved search record.")));
            }
            let id = parts[1]
                .parse::<u64>()
                .ok()
                .filter(|id| *id > 0 && *id < next_id)
                .ok_or_else(|| invalid(tr("Invalid saved search identity.")))?;
            if library.filters.iter().any(|f| f.id == id)
                || library.bookmarks.iter().any(|b| b.id == id)
            {
                return Err(invalid(tr("Invalid saved search identity.")));
            }
            let name = decode(parts[2])?;
            if name.trim().is_empty() && id > 9 {
                return Err(invalid(tr("Enter a name.")));
            }
            let search = decode(parts[3])?;
            let options = decode_options(parts[4], parts[5], parts[6])?;
            library.filters.push(NamedFilter {
                id,
                name,
                search,
                options,
            });
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
                f.name == filter.name && f.search == filter.search && f.options == filter.options
            }) {
                same.id
            } else if filter.id <= 9 {
                merged.filters.retain(|f| f.id != filter.id);
                merged.filters.push(filter.clone());
                filter.id
            } else {
                merged.put_filter(None, &filter.name, &filter.search, filter.options)?
            };
            ids.insert(filter.id, id);
        }
        for bookmark in &incoming.bookmarks {
            let mut bookmark = bookmark.clone();
            bookmark.name = bookmark.name.trim().into();
            if let Some(id) = ids.get(&bookmark.filter.id) {
                bookmark.filter.id = *id;
            }
            let equivalent = merged.bookmarks.iter().any(|b| {
                b.name == bookmark.name
                    && b.search == bookmark.search
                    && b.options == bookmark.options
                    && b.filter == bookmark.filter
                    && b.sort_column == bookmark.sort_column
                    && b.descending == bookmark.descending
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
            text.push_str(&format!(
                "F\t{}\t{}\t{}\t{}\n",
                f.id,
                settings::escape(f.name.as_bytes()),
                settings::escape(f.search.as_bytes()),
                encode_options(f.options)
            ));
        }
        for b in &self.bookmarks {
            let f = &b.filter;
            text.push_str(&format!(
                "B\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                b.id,
                settings::escape(b.name.as_bytes()),
                settings::escape(b.search.as_bytes()),
                encode_options(b.options),
                b.sort_column,
                u8::from(b.descending),
                f.id,
                settings::escape(f.name.as_bytes()),
                settings::escape(f.search.as_bytes()),
                encode_options(f.options)
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
        }
    }

    #[test]
    fn filter_validation_keeps_the_translated_parser_reason() {
        let mut library = Library::default();
        let error = library
            .put_filter(None, "Broken", "regex:(", QueryOptions::default())
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
            l.put_filter(Some(1), "All", "", QueryOptions::default())
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
            let id = l.put_filter(None, "custom", "", QueryOptions::default())?;
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
        l.put_filter(Some(4), "Changed", "other", QueryOptions::default())
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
            l.put_filter(None, "Broken", "ext:md", QueryOptions::default())
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
            l.put_filter(Some(broken.id), "Repaired", "ext:txt", broken.options)
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
            .put_filter(None, "  Notes  ", "ext:md", QueryOptions::default())
            .unwrap();
        l.move_filter(id, -1).unwrap();
        assert_eq!(l.filters[8].id, id);
        l.put_filter(Some(id), "Work", "ext:rs", QueryOptions::default())
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
            l.put_filter(Some(id), "missing", "", QueryOptions::default())
                .is_err()
        );
        assert!(l.move_filter(id, -1).is_err());
        assert!(
            l.put_filter(None, " ", "", QueryOptions::default())
                .is_err()
        );
        assert!(
            l.put_filter(None, "bad", "size:invalid", QueryOptions::default())
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
        let (l, _) = update(&path, |l| l.put_filter(None, "Ad\t\n\\", "foo.*", options)).unwrap();
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
                        l.put_filter(None, &i.to_string(), "", QueryOptions::default())
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
