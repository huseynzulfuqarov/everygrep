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
//! exclude = node_modules
//! exclude-files = *.tmp;~*
//! filter = document
//! history-size = 50
//! on-close = hide
//! folder-size = items
//! theme = light
//! density = compact
//! status-details = false
//! menu-bar = false
//! language = az
//! shortcut = edit.copy list Ctrl+C; global Ctrl+Insert
//! shortcut = file.export-results
//! ```
//!
//! A `shortcut` line replaces all default keys of one command (its id, see
//! [`crate::commands::COMMANDS`]) by the listed ones, `<scope> <keys>`
//! separated by `;`; without keys the command has none. Only commands
//! whose keys differ from the defaults are written.
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
use everygrep_core::query::{HiddenFiles, QueryOptions};
use everygrep_core::rules::{IndexRules, RuleSpec};
use everygrep_core::sort::FolderSize;

use crate::commands::{self, Chord, Scope};
use crate::engine::DriveSettings;
use crate::format::{SizeStandard, SizeUnit};
use crate::i18n::{self, Lang};

/// Search history entries kept by default (the number before settings
/// existed).
pub const DEFAULT_HISTORY: usize = 200;
/// Opening more items than this at once asks first, by default.
pub const DEFAULT_OPEN_CONFIRM: usize = 32;
/// The largest number the open question can be set to.
pub const MAX_OPEN_CONFIRM: usize = 10_000;
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

/// The window's colors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    /// Follow the desktop's light or dark setting, also when it changes.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    fn id(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }
}

/// How tightly the result list is packed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    /// 28 px rows (the look before the setting existed).
    #[default]
    Comfortable,
    /// 22 px rows with smaller icons and text, for many rows at once.
    Compact,
}

impl Density {
    pub const ALL: [Density; 2] = [Density::Comfortable, Density::Compact];

    fn id(self) -> &'static str {
        match self {
            Density::Comfortable => "comfortable",
            Density::Compact => "compact",
        }
    }
}

/// The language of the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LanguageChoice {
    /// The language of the desktop session (`LANGUAGE`, `LC_ALL`,
    /// `LC_MESSAGES`, `LANG`), English when it has no translation.
    #[default]
    System,
    Fixed(Lang),
}

impl LanguageChoice {
    pub const ALL: [LanguageChoice; 4] = [
        LanguageChoice::System,
        LanguageChoice::Fixed(Lang::En),
        LanguageChoice::Fixed(Lang::Az),
        LanguageChoice::Fixed(Lang::Tr),
    ];

    fn id(self) -> &'static str {
        match self {
            LanguageChoice::System => "system",
            LanguageChoice::Fixed(lang) => lang.code(),
        }
    }

    /// The language the window shows for this choice.
    pub fn resolve(self) -> Lang {
        match self {
            LanguageChoice::System => i18n::system_language_from_env(),
            LanguageChoice::Fixed(lang) => lang,
        }
    }
}

/// Optional columns and column widths as the window last showed them. A width
/// of `None` means the window's default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Columns {
    pub show_type: bool,
    pub show_ext: bool,
    pub show_created: bool,
    pub show_accessed: bool,
    pub show_attrs: bool,
    pub name: Option<f32>,
    /// Folder's own width, kept only while the columns are wider than the
    /// window (Folder otherwise takes the room the others leave).
    pub folder: Option<f32>,
    pub size: Option<f32>,
    pub date: Option<f32>,
    pub type_: Option<f32>,
    pub ext: Option<f32>,
    pub created: Option<f32>,
    pub accessed: Option<f32>,
    pub attrs: Option<f32>,
}

/// Every setting the window has. `Default` gives the program's behaviour
/// without a settings file.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Folders to index; empty means the folders of the saved index, or the
    /// home folder for a first start.
    pub roots: Vec<PathBuf>,
    /// Folders left out of the index: absolute paths or patterns (see
    /// [`everygrep_core::rules`]).
    pub excludes: Vec<PathBuf>,
    /// Whether the excluded folders and the file filters are used.
    pub use_excludes: bool,
    /// Patterns of the only files to index, `;`-separated; empty for all.
    pub include_files: String,
    /// Patterns of files left out of the index, `;`-separated.
    pub exclude_files: String,
    /// Whether file changes are applied to the index as they happen.
    pub watch_changes: bool,
    /// Filter and matching modes for a new start.
    pub options: QueryOptions,
    /// Whether the results follow every change of the search; off searches
    /// when Enter is pressed.
    pub as_you_type: bool,
    /// Show the active matching modes in the status line.
    pub show_mode_badges: bool,
    /// Whether moving to the previous or next filter wraps at the ends.
    pub filter_wrap: bool,
    /// Whether a click into the search field, while it does not have the
    /// focus, selects all of its text; off places the caret.
    pub select_search_on_click: bool,
    /// Whether a new search clears the selection; off keeps the selected
    /// items that the new result lists.
    pub clear_selection_on_search: bool,
    /// Searches kept for Back and Forward; 0 turns the history off.
    pub history_size: usize,
    /// Whether the search history is saved between sessions.
    pub history_on_disk: bool,
    pub on_close: OnClose,
    /// What the Size column shows for folders, and sorts them by. The one
    /// default that differs from the behaviour before settings existed
    /// (folders had an empty size then).
    pub folder_size: FolderSize,
    /// Whether the optional columns and the column widths are kept between
    /// sessions.
    pub remember_columns: bool,
    pub columns: Columns,
    /// Whether renaming preselects the name without its extension; off
    /// selects the whole name.
    pub rename_name_only: bool,
    pub fileops: crate::fileops_settings::Settings,
    /// Whether a double-click on the Folder cell of a row opens the
    /// containing folder instead of the item.
    pub path_click_opens_folder: bool,
    /// Whether Copy full path puts each path in double quotes, as Copy
    /// full path with quotes does.
    pub copy_full_path_quotes: bool,
    /// Opening more items than this at once asks first; 0 never asks.
    pub open_confirm_over: usize,
    /// Whether showing the window (at a later launch, or with its shortcut)
    /// moves the keyboard focus to the search field and selects its text;
    /// off keeps the focus where it was.
    pub focus_on_show: bool,
    /// Whether the first click on a date column header sorts the newest
    /// first; off sorts the oldest first, as for any other column.
    pub dates_newest_first: bool,
    /// Whether the first click on the Size header sorts the largest first.
    pub sizes_largest_first: bool,
    pub theme: ThemeChoice,
    pub density: Density,
    /// Whether every second row of the result list has a slightly
    /// different background.
    pub alternate_rows: bool,
    /// The unit of the sizes in the Size column and the status line.
    pub size_unit: SizeUnit,
    /// How big a kilobyte is, and whether units are written KB or KiB.
    pub size_standard: SizeStandard,
    /// Whether the status line shows the size, date and path of the selected
    /// item (and the number and size of several).
    pub status_details: bool,
    /// Whether the window has a menu bar; without it the ⋯ button holds the
    /// program's menu.
    pub menu_bar: bool,
    pub language: LanguageChoice,
    /// Removable drives: indexed automatically, kept when removed, and the
    /// drives never indexed.
    pub drives: DriveSettings,
    /// Commands whose keys differ from the defaults: the command's id and
    /// its keys, which replace the defaults (an empty list removes them).
    pub shortcuts: Vec<(String, Vec<(Scope, Chord)>)>,
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
            use_excludes: true,
            include_files: String::new(),
            exclude_files: String::new(),
            watch_changes: true,
            options: QueryOptions::default(),
            as_you_type: true,
            show_mode_badges: true,
            filter_wrap: true,
            select_search_on_click: true,
            clear_selection_on_search: true,
            history_size: DEFAULT_HISTORY,
            history_on_disk: false,
            on_close: OnClose::Quit,
            folder_size: FolderSize::Total,
            remember_columns: true,
            columns: Columns::default(),
            rename_name_only: true,
            fileops: crate::fileops_settings::Settings::default(),
            path_click_opens_folder: true,
            copy_full_path_quotes: false,
            open_confirm_over: DEFAULT_OPEN_CONFIRM,
            focus_on_show: true,
            dates_newest_first: false,
            sizes_largest_first: false,
            theme: ThemeChoice::System,
            density: Density::Comfortable,
            alternate_rows: true,
            size_unit: SizeUnit::Auto,
            size_standard: SizeStandard::Jedec,
            status_details: true,
            menu_bar: true,
            language: LanguageChoice::System,
            drives: DriveSettings::default(),
            shortcuts: Vec::new(),
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
    Appearance,
    Window,
}

impl Settings {
    /// The rules of the excluded folders and file filters.
    pub fn rule_spec(&self) -> RuleSpec {
        RuleSpec {
            enabled: self.use_excludes,
            folders: self.excludes.clone(),
            include_files: self.include_files.clone(),
            exclude_files: self.exclude_files.clone(),
        }
    }

    /// Takes the rules of the excluded folders and file filters.
    pub fn set_rule_spec(&mut self, spec: &RuleSpec) {
        self.use_excludes = spec.enabled;
        self.excludes = spec.folders.clone();
        self.include_files = spec.include_files.clone();
        self.exclude_files = spec.exclude_files.clone();
    }

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
            if key == "shortcut" {
                // A command this version does not know, or keys it cannot
                // read, are kept for a newer version and reported.
                match parse_shortcut(value) {
                    Some((id, keys)) => s.set_keys(id, keys),
                    None => {
                        s.problems.push((i + 1, line.to_owned()));
                        s.unknown.push(trimmed.to_owned());
                    }
                }
                continue;
            }
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
        // A pattern that does not compile is not taken.
        let valid = |spec: RuleSpec| IndexRules::new(spec).is_ok();
        match key {
            "root" | "exclude" => {
                let Some(path) = unescape(value).filter(|p| !p.is_empty()) else {
                    return false;
                };
                let path = PathBuf::from(OsString::from_vec(path));
                if key == "exclude"
                    && !valid(RuleSpec {
                        folders: vec![path.clone()],
                        ..RuleSpec::default()
                    })
                {
                    return false;
                }
                let list = if key == "root" {
                    &mut self.roots
                } else {
                    &mut self.excludes
                };
                list.push(path);
                true
            }
            "use-exclude-list" => put_flag(&mut self.use_excludes),
            "watch-changes" => put_flag(&mut self.watch_changes),
            "include-only-files" | "exclude-files" => {
                let Some(text) = unescape(value).and_then(|b| String::from_utf8(b).ok()) else {
                    return false;
                };
                let spec = if key == "include-only-files" {
                    RuleSpec {
                        include_files: text.clone(),
                        ..RuleSpec::default()
                    }
                } else {
                    RuleSpec {
                        exclude_files: text.clone(),
                        ..RuleSpec::default()
                    }
                };
                if !valid(spec) {
                    return false;
                }
                *if key == "include-only-files" {
                    &mut self.include_files
                } else {
                    &mut self.exclude_files
                } = text;
                true
            }
            "filter" => Filter::from_id(value)
                .map(|f| self.options.filter = f)
                .is_some(),
            "match-case" => put_flag(&mut self.options.case_sensitive),
            "whole-words" => put_flag(&mut self.options.whole_words),
            "match-path" => put_flag(&mut self.options.match_path),
            "match-diacritics" => put_flag(&mut self.options.match_diacritics),
            "match-prefix" => put_flag(&mut self.options.prefix),
            "match-suffix" => put_flag(&mut self.options.suffix),
            "ignore-punctuation" => put_flag(&mut self.options.ignore_punctuation),
            "ignore-whitespace" => put_flag(&mut self.options.ignore_whitespace),
            "wildcards-anywhere" => put_flag(&mut self.options.wildcards_anywhere),
            "literal-operators" => put_flag(&mut self.options.literal_operators),
            "and-before-or" => put_flag(&mut self.options.and_before_or),
            "hidden-files" => HiddenFiles::from_id(value)
                .map(|h| self.options.hidden_files = h)
                .is_some(),
            "regex" => put_flag(&mut self.options.regex),
            "filter-wrap" => put_flag(&mut self.filter_wrap),
            "show-mode-badges" => put_flag(&mut self.show_mode_badges),
            "search-as-you-type" => put_flag(&mut self.as_you_type),
            "select-search-on-click" => put_flag(&mut self.select_search_on_click),
            "deselect-on-new-search" => put_flag(&mut self.clear_selection_on_search),
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
            "folder-size" => FolderSize::from_id(value)
                .map(|f| self.folder_size = f)
                .is_some(),
            "remember-columns" => put_flag(&mut self.remember_columns),
            "rename-select-name-only" => put_flag(&mut self.rename_name_only),
            "open-folder-on-path-double-click" => put_flag(&mut self.path_click_opens_folder),
            "copy-full-path-quotes" => put_flag(&mut self.copy_full_path_quotes),
            "confirm-open-over" => value
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= MAX_OPEN_CONFIRM)
                .map(|n| self.open_confirm_over = n)
                .is_some(),
            "focus-search-on-show" => put_flag(&mut self.focus_on_show),
            "sort-dates-newest-first" => put_flag(&mut self.dates_newest_first),
            "sort-sizes-largest-first" => put_flag(&mut self.sizes_largest_first),
            "theme" => ThemeChoice::ALL
                .into_iter()
                .find(|t| t.id() == value)
                .map(|t| self.theme = t)
                .is_some(),
            "status-details" => put_flag(&mut self.status_details),
            "alternate-row-color" => put_flag(&mut self.alternate_rows),
            "size-format" => SizeUnit::from_id(value)
                .map(|u| self.size_unit = u)
                .is_some(),
            "size-units" => SizeStandard::from_id(value)
                .map(|s| self.size_standard = s)
                .is_some(),
            "menu-bar" => put_flag(&mut self.menu_bar),
            "language" => LanguageChoice::ALL
                .into_iter()
                .find(|l| l.id() == value)
                .map(|l| self.language = l)
                .is_some(),
            "density" => Density::ALL
                .into_iter()
                .find(|d| d.id() == value)
                .map(|d| self.density = d)
                .is_some(),
            "index-removable-drives" => put_flag(&mut self.drives.auto),
            "removed-drives" => match value {
                "keep" => {
                    self.drives.keep_offline = true;
                    true
                }
                "forget" => {
                    self.drives.keep_offline = false;
                    true
                }
                _ => false,
            },
            "ignore-drive" => {
                if !everygrep_core::drives::is_safe_uuid(value) {
                    return false;
                }
                if !self.drives.ignored.iter().any(|u| u == value) {
                    self.drives.ignored.push(value.to_owned());
                }
                true
            }
            "show-type-column" => put_flag(&mut self.columns.show_type),
            "show-extension-column" => put_flag(&mut self.columns.show_ext),
            "show-created-column" => put_flag(&mut self.columns.show_created),
            "show-accessed-column" => put_flag(&mut self.columns.show_accessed),
            "show-attributes-column" => put_flag(&mut self.columns.show_attrs),
            "name-width" | "folder-width" | "size-width" | "modified-width" | "type-width"
            | "extension-width" | "created-width" | "accessed-width" | "attributes-width" => {
                let Some(w) = width(value) else { return false };
                let c = &mut self.columns;
                *match key {
                    "name-width" => &mut c.name,
                    "folder-width" => &mut c.folder,
                    "size-width" => &mut c.size,
                    "modified-width" => &mut c.date,
                    "type-width" => &mut c.type_,
                    "extension-width" => &mut c.ext,
                    "created-width" => &mut c.created,
                    "accessed-width" => &mut c.accessed,
                    _ => &mut c.attrs,
                } = Some(w);
                true
            }
            _ => self.fileops.set(key, value),
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
        flag(
            &mut out,
            "watch-changes",
            self.watch_changes,
            d.watch_changes,
        );
        flag(
            &mut out,
            "use-exclude-list",
            self.use_excludes,
            d.use_excludes,
        );
        if !self.include_files.is_empty() {
            line(
                &mut out,
                "include-only-files",
                &escape(self.include_files.as_bytes()),
            );
        }
        if !self.exclude_files.is_empty() {
            line(
                &mut out,
                "exclude-files",
                &escape(self.exclude_files.as_bytes()),
            );
        }
        flag(
            &mut out,
            "index-removable-drives",
            self.drives.auto,
            d.drives.auto,
        );
        if self.drives.keep_offline != d.drives.keep_offline {
            line(
                &mut out,
                "removed-drives",
                if self.drives.keep_offline {
                    "keep"
                } else {
                    "forget"
                },
            );
        }
        for uuid in &self.drives.ignored {
            line(&mut out, "ignore-drive", uuid);
        }
        flag(&mut out, "filter-wrap", self.filter_wrap, d.filter_wrap);
        let (o, od) = (&self.options, &d.options);
        if o.hidden_files != od.hidden_files {
            line(&mut out, "hidden-files", o.hidden_files.id());
        }
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
        flag(&mut out, "match-prefix", o.prefix, od.prefix);
        flag(&mut out, "match-suffix", o.suffix, od.suffix);
        flag(
            &mut out,
            "ignore-punctuation",
            o.ignore_punctuation,
            od.ignore_punctuation,
        );
        flag(
            &mut out,
            "ignore-whitespace",
            o.ignore_whitespace,
            od.ignore_whitespace,
        );
        flag(&mut out, "regex", o.regex, od.regex);
        flag(&mut out, "and-before-or", o.and_before_or, od.and_before_or);
        flag(
            &mut out,
            "literal-operators",
            o.literal_operators,
            od.literal_operators,
        );
        flag(
            &mut out,
            "wildcards-anywhere",
            o.wildcards_anywhere,
            od.wildcards_anywhere,
        );
        flag(
            &mut out,
            "show-mode-badges",
            self.show_mode_badges,
            d.show_mode_badges,
        );
        flag(
            &mut out,
            "search-as-you-type",
            self.as_you_type,
            d.as_you_type,
        );
        flag(
            &mut out,
            "select-search-on-click",
            self.select_search_on_click,
            d.select_search_on_click,
        );
        flag(
            &mut out,
            "deselect-on-new-search",
            self.clear_selection_on_search,
            d.clear_selection_on_search,
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
        self.fileops.write(&mut out);
        if self.on_close != d.on_close {
            line(&mut out, "on-close", self.on_close.id());
        }
        if self.folder_size != d.folder_size {
            line(&mut out, "folder-size", self.folder_size.id());
        }
        flag(
            &mut out,
            "remember-columns",
            self.remember_columns,
            d.remember_columns,
        );
        flag(
            &mut out,
            "rename-select-name-only",
            self.rename_name_only,
            d.rename_name_only,
        );
        flag(
            &mut out,
            "open-folder-on-path-double-click",
            self.path_click_opens_folder,
            d.path_click_opens_folder,
        );
        flag(
            &mut out,
            "copy-full-path-quotes",
            self.copy_full_path_quotes,
            d.copy_full_path_quotes,
        );
        if self.open_confirm_over != d.open_confirm_over {
            line(
                &mut out,
                "confirm-open-over",
                &self.open_confirm_over.to_string(),
            );
        }
        flag(
            &mut out,
            "focus-search-on-show",
            self.focus_on_show,
            d.focus_on_show,
        );
        flag(
            &mut out,
            "sort-dates-newest-first",
            self.dates_newest_first,
            d.dates_newest_first,
        );
        flag(
            &mut out,
            "sort-sizes-largest-first",
            self.sizes_largest_first,
            d.sizes_largest_first,
        );
        if self.theme != d.theme {
            line(&mut out, "theme", self.theme.id());
        }
        if self.density != d.density {
            line(&mut out, "density", self.density.id());
        }
        flag(
            &mut out,
            "status-details",
            self.status_details,
            d.status_details,
        );
        flag(&mut out, "menu-bar", self.menu_bar, d.menu_bar);
        flag(
            &mut out,
            "alternate-row-color",
            self.alternate_rows,
            d.alternate_rows,
        );
        if self.size_unit != d.size_unit {
            line(&mut out, "size-format", self.size_unit.id());
        }
        if self.size_standard != d.size_standard {
            line(&mut out, "size-units", self.size_standard.id());
        }
        if self.language != d.language {
            line(&mut out, "language", self.language.id());
        }
        let c = &self.columns;
        flag(&mut out, "show-type-column", c.show_type, false);
        flag(&mut out, "show-extension-column", c.show_ext, false);
        flag(&mut out, "show-created-column", c.show_created, false);
        flag(&mut out, "show-accessed-column", c.show_accessed, false);
        flag(&mut out, "show-attributes-column", c.show_attrs, false);
        let widths = [
            ("name-width", c.name),
            ("folder-width", c.folder),
            ("size-width", c.size),
            ("modified-width", c.date),
            ("type-width", c.type_),
            ("extension-width", c.ext),
            ("created-width", c.created),
            ("accessed-width", c.accessed),
            ("attributes-width", c.attrs),
        ];
        for (key, width) in widths {
            if let Some(w) = width {
                line(&mut out, key, &format!("{}", w.round()));
            }
        }
        for (id, keys) in &self.shortcuts {
            line(&mut out, "shortcut", &shortcut_value(id, keys));
        }
        for unknown in &self.unknown {
            out.push_str(unknown);
            out.push('\n');
        }
        out
    }

    /// Sets the keys of the command `id`, in place of its defaults.
    fn set_keys(&mut self, id: String, keys: Vec<(Scope, Chord)>) {
        match self.shortcuts.iter_mut().find(|(i, _)| *i == id) {
            Some(entry) => entry.1 = keys,
            None => self.shortcuts.push((id, keys)),
        }
    }

    /// Takes the commands whose keys differ from the defaults (see
    /// [`commands::Keymap::overrides`]). A kept line that could not be read
    /// for one of them is dropped, since the new keys replace it.
    pub fn set_shortcuts(&mut self, shortcuts: Vec<(String, Vec<(Scope, Chord)>)>) {
        self.unknown.retain(|line| {
            shortcut_line_id(line).is_none_or(|id| !shortcuts.iter().any(|(i, _)| i == id))
        });
        self.shortcuts = shortcuts;
    }

    /// Puts every command back to its default keys: all `shortcut` lines
    /// go, also those that could not be read.
    pub fn reset_shortcuts(&mut self) {
        self.shortcuts.clear();
        self.unknown.retain(|line| shortcut_line_id(line).is_none());
    }

    /// Puts the settings of one page of the settings window back to their
    /// defaults.
    pub fn reset(&mut self, page: Page) {
        let d = Settings::default();
        match page {
            Page::Indexing => {
                self.roots = d.roots;
                self.excludes = d.excludes;
                self.use_excludes = d.use_excludes;
                self.include_files = d.include_files;
                self.exclude_files = d.exclude_files;
                self.watch_changes = d.watch_changes;
                self.drives = d.drives;
            }
            Page::Search => {
                self.options = d.options;
                self.as_you_type = d.as_you_type;
                self.show_mode_badges = d.show_mode_badges;
                self.filter_wrap = d.filter_wrap;
                self.select_search_on_click = d.select_search_on_click;
                self.clear_selection_on_search = d.clear_selection_on_search;
                self.history_size = d.history_size;
                self.history_on_disk = d.history_on_disk;
            }
            Page::Appearance => {
                self.theme = d.theme;
                self.density = d.density;
                self.alternate_rows = d.alternate_rows;
                self.size_unit = d.size_unit;
                self.size_standard = d.size_standard;
                self.status_details = d.status_details;
                self.menu_bar = d.menu_bar;
                self.language = d.language;
            }
            Page::Window => {
                self.on_close = d.on_close;
                self.folder_size = d.folder_size;
                self.remember_columns = d.remember_columns;
                self.columns = d.columns;
                self.rename_name_only = d.rename_name_only;
                self.fileops = d.fileops;
                self.path_click_opens_folder = d.path_click_opens_folder;
                self.copy_full_path_quotes = d.copy_full_path_quotes;
                self.open_confirm_over = d.open_confirm_over;
                self.focus_on_show = d.focus_on_show;
                self.dates_newest_first = d.dates_newest_first;
                self.sizes_largest_first = d.sizes_largest_first;
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
/// Reading first keeps changes another window made meanwhile; the lock file
/// `<name>.lock` next to it is held from the read to the write, so two
/// windows saving at once do not lose either change. A file that exists but
/// cannot be read is left alone and the error returned.
pub fn update(path: &Path, change: impl FnOnce(&mut Settings)) -> io::Result<Settings> {
    let _lock = lock(path)?;
    let mut s = Settings::load(path)?;
    change(&mut s);
    s.save(path)?;
    Ok(s)
}

unsafe extern "C" {
    fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
}

/// `LOCK_EX` from `<sys/file.h>`.
const LOCK_EX: std::ffi::c_int = 2;

/// Takes the exclusive lock on `<path>.lock` (creating it and its folder),
/// waiting for another holder. The lock goes with the returned file.
fn lock(path: &Path) -> io::Result<fs::File> {
    use std::os::fd::AsRawFd;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    let lock_path = path.with_file_name(name);
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    loop {
        // SAFETY: the descriptor is open for the whole call.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX) } == 0 {
            return Ok(file);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// The value of a `shortcut` line: `<id>[ <scope> <keys>[; <scope> <keys>]…]`.
/// `None` for an unknown command or keys that cannot be read.
fn parse_shortcut(value: &str) -> Option<(String, Vec<(Scope, Chord)>)> {
    let (id, rest) = match value.split_once(' ') {
        Some((id, rest)) => (id, rest.trim_ascii()),
        None => (value, ""),
    };
    commands::by_id(id)?;
    let mut keys = Vec::new();
    for part in rest
        .split(';')
        .map(str::trim_ascii)
        .filter(|p| !p.is_empty())
    {
        let (scope, chord) = part.split_once(' ')?;
        let key = (Scope::from_id(scope)?, Chord::parse(chord)?);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Some((id.to_owned(), keys))
}

fn shortcut_value(id: &str, keys: &[(Scope, Chord)]) -> String {
    let keys: Vec<String> = keys
        .iter()
        .map(|(scope, chord)| format!("{} {chord}", scope.id()))
        .collect();
    if keys.is_empty() {
        id.to_owned()
    } else {
        format!("{id} {}", keys.join("; "))
    }
}

/// The command id of a `shortcut` line kept as it was read.
fn shortcut_line_id(line: &str) -> Option<&str> {
    let (key, value) = line.split_once('=')?;
    (key.trim_ascii() == "shortcut").then(|| value.trim_ascii().split(' ').next().unwrap_or(""))
}

fn is_known(key: &str) -> bool {
    let mut probe = Settings::default();
    // Every known key accepts at least one of these values.
    [
        "true", "all", "quit", "total", "system", "compact", "1", "/", "keep", "A-1", "auto",
        "jedec", "show", "symlink", "number",
    ]
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
    let (mut file, tmp) = create_temp(path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// How many temporary names [`create_temp`] tries.
const TEMP_TRIES: u32 = 100;

/// Creates a new temporary file next to `target`, for writing it whole and
/// renaming it over `target`: `.<name>.tmp-<pid>-<n>` in the same folder,
/// `n` from a counter mixed with the clock. The file is always a new one
/// (`O_EXCL`): an entry under that name, such as a planted link, is never
/// followed or replaced; the next name is tried.
pub fn create_temp(target: &Path) -> io::Result<(fs::File, PathBuf)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let base = target.file_name().unwrap_or_default();
    let names = std::iter::repeat_with(|| {
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let n = NEXT
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9)
            ^ u64::from(clock);
        let mut name = std::ffi::OsString::from(".");
        name.push(base);
        name.push(format!(".tmp-{}-{n:x}", std::process::id()));
        name
    });
    create_temp_named(dir, names)
}

/// [`create_temp`] with the names to try given, for the tests.
fn create_temp_named(
    dir: &Path,
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> io::Result<(fs::File, PathBuf)> {
    let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
    for name in names.into_iter().take(TEMP_TRIES as usize) {
        let tmp = dir.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
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
    fn saving_never_writes_through_a_planted_link() {
        let dir = Scratch::new("settings-link");
        let victim = dir.0.join("victim.txt");
        fs::write(&victim, "keep").unwrap();
        let path = dir.0.join("settings");
        // The temporary name a save of this process used to take.
        let planted = dir.0.join(format!("settings.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn two_writers_at_once_keep_both_changes() {
        use std::sync::mpsc;
        let dir = Scratch::new("settings-two-writers");
        let path = dir.0.join("settings");
        Settings::default().save(&path).unwrap();
        let (loaded, wait) = mpsc::channel();
        let first = {
            let path = path.clone();
            std::thread::spawn(move || {
                update(&path, |s| {
                    s.history_size = 7;
                    loaded.send(()).unwrap();
                    // The other writer starts while this one has read the
                    // file but not written it yet.
                    std::thread::sleep(std::time::Duration::from_millis(300));
                })
                .unwrap();
            })
        };
        wait.recv().unwrap();
        update(&path, |s| s.as_you_type = false).unwrap();
        first.join().unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.history_size, 7);
        assert!(!s.as_you_type, "the second change was lost");
    }

    #[test]
    fn a_temporary_file_is_always_a_new_one() {
        let dir = Scratch::new("temp-new");
        let victim = dir.0.join("victim");
        fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.0.join("taken-link")).unwrap();
        fs::write(dir.0.join("taken-file"), "old").unwrap();
        let names = ["taken-link", "taken-file", "free"].map(std::ffi::OsString::from);
        let (mut file, tmp) = create_temp_named(&dir.0, names).unwrap();
        assert_eq!(tmp, dir.0.join("free"));
        file.write_all(b"x").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(fs::read_to_string(dir.0.join("taken-file")).unwrap(), "old");
        let e = create_temp_named(&dir.0, [std::ffi::OsString::from("free")]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        // The real names are hidden, per target, and differ every time.
        let (_, a) = create_temp(&dir.0.join("settings")).unwrap();
        let (_, b) = create_temp(&dir.0.join("settings")).unwrap();
        assert_ne!(a, b);
        let a = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(a.starts_with(".settings.tmp-"), "{a}");
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
            excludes: vec!["/home/u/.cache".into(), "regex:^te?mp$".into()],
            use_excludes: false,
            watch_changes: false,
            include_files: "*.mp3; \"a;b\"".into(),
            exclude_files: "*.tmp;~*".into(),
            options: QueryOptions {
                case_sensitive: true,
                whole_words: true,
                match_path: true,
                match_diacritics: true,
                prefix: true,
                suffix: true,
                ignore_punctuation: true,
                ignore_whitespace: true,
                hidden_files: HiddenFiles::Only,
                regex: true,
                and_before_or: true,
                literal_operators: true,
                wildcards_anywhere: true,
                filter: Filter::Picture,
            },
            as_you_type: false,
            show_mode_badges: false,
            filter_wrap: false,
            select_search_on_click: false,
            clear_selection_on_search: false,
            history_size: 0,
            history_on_disk: true,
            on_close: OnClose::Hide,
            folder_size: FolderSize::Items,
            remember_columns: false,
            rename_name_only: false,
            path_click_opens_folder: false,
            copy_full_path_quotes: true,
            open_confirm_over: 0,
            focus_on_show: false,
            dates_newest_first: true,
            sizes_largest_first: true,
            columns: Columns {
                show_type: true,
                show_ext: true,
                show_created: true,
                show_accessed: false,
                show_attrs: true,
                name: Some(300.0),
                folder: Some(410.0),
                size: Some(90.0),
                date: Some(140.0),
                type_: Some(120.0),
                ext: Some(60.0),
                created: Some(110.0),
                accessed: None,
                attrs: Some(50.0),
            },
            theme: ThemeChoice::Dark,
            density: Density::Compact,
            alternate_rows: false,
            size_unit: SizeUnit::Mega,
            size_standard: SizeStandard::Iec,
            status_details: false,
            menu_bar: false,
            drives: DriveSettings {
                auto: false,
                keep_offline: false,
                ignored: vec!["1234-ABCD".into(), "c0ffee".into()],
            },
            ..Settings::default()
        };
        let text = s.to_text();
        assert_eq!(Settings::parse(&text), s, "{text}");
        assert!(text.contains("index-removable-drives = false\n"));
        assert!(text.contains("removed-drives = forget\n"));
        assert!(text.contains("ignore-drive = 1234-ABCD\nignore-drive = c0ffee\n"));
        // A UUID that could not be a file name is not taken.
        assert!(
            Settings::parse("ignore-drive = ../x\n")
                .drives
                .ignored
                .is_empty()
        );
        assert!(text.contains("folder-width = 410\n"));
        assert!(text.contains("theme = dark\n"));
        assert!(text.contains("density = compact\n"));
        assert!(text.contains("status-details = false\n"));
        assert!(text.contains("menu-bar = false\n"));
        assert!(text.contains("alternate-row-color = false\n"));
        assert!(text.contains("size-format = mb\n"));
        assert!(text.contains("size-units = iec\n"));
        assert!(text.contains("root = /srv/media files\n"));
        assert!(text.contains("on-close = hide\n"));
        assert!(text.contains("folder-size = items\n"));
        assert!(text.contains("use-exclude-list = false\n"));
        assert!(text.contains("watch-changes = false\n"));
        assert!(text.contains("search-as-you-type = false\n"));
        assert!(text.contains("select-search-on-click = false\n"));
        assert!(text.contains("deselect-on-new-search = false\n"));
        assert!(text.contains("rename-select-name-only = false\n"));
        assert!(text.contains("open-folder-on-path-double-click = false\n"));
        assert!(text.contains("copy-full-path-quotes = true\n"));
        assert!(text.contains("confirm-open-over = 0\n"));
        assert_eq!(
            Settings::parse("confirm-open-over = 99999\n").open_confirm_over,
            DEFAULT_OPEN_CONFIRM,
            "too large a number is not taken"
        );
        assert!(text.contains("focus-search-on-show = false\n"));
        assert!(text.contains("sort-dates-newest-first = true\n"));
        assert!(text.contains("sort-sizes-largest-first = true\n"));
        assert!(text.contains("exclude = regex:^te?mp$\n"));
        assert!(text.contains("exclude-files = *.tmp;~*\n"));
    }

    #[test]
    fn hidden_file_choices_round_trip_validate_and_reset() {
        for mode in HiddenFiles::ALL {
            let mut s = Settings::parse(&format!("hidden-files = {}\n", mode.id()));
            assert!(s.problems.is_empty());
            assert_eq!(s.options.hidden_files, mode);
            assert_eq!(Settings::parse(&s.to_text()).options.hidden_files, mode);
            s.reset(Page::Search);
            assert_eq!(s.options.hidden_files, HiddenFiles::Show);
        }
        let s = Settings::parse("hidden-files = invalid\n");
        assert_eq!(s.problems.len(), 1);
        assert_eq!(s.options.hidden_files, HiddenFiles::Show);
    }

    #[test]
    fn broken_patterns_are_not_taken() {
        let s = Settings::parse(
            "exclude = regex:(open\nexclude = /ok\ninclude-only-files = regex:[z-a]\nexclude-files = *.bak\nexclude-files = regex:\n",
        );
        assert_eq!(s.excludes, [PathBuf::from("/ok")]);
        assert!(s.include_files.is_empty());
        assert_eq!(s.exclude_files, "*.bak");
        let lines: Vec<usize> = s.problems.iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, [1, 3, 5]);
        assert!(IndexRules::new(s.rule_spec()).is_ok());
        let d = Settings::default();
        assert!(d.use_excludes, "the exclude list is used by default");
        assert!(d.watch_changes, "changes are watched by default");
        assert!(
            d.select_search_on_click,
            "a click into the search field selects its text by default"
        );
        assert!(
            d.rename_name_only,
            "the extension is not selected by default"
        );
        assert!(d.focus_on_show, "showing the window focuses the search");
        assert_eq!(d.rule_spec(), RuleSpec::default());
    }

    #[test]
    fn folder_size_defaults_to_total_and_reads_every_choice() {
        assert_eq!(Settings::default().folder_size, FolderSize::Total);
        for f in FolderSize::ALL {
            let s = Settings {
                folder_size: f,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&s.to_text()).folder_size, f);
        }
        assert!(!Settings::default().to_text().contains("folder-size"));
        let s = Settings::parse("folder-size = none\nfolder-size = bytes\n");
        assert_eq!(s.folder_size, FolderSize::None);
        assert_eq!(s.problems.len(), 1, "a bad value is reported, not kept");
        assert!(!s.to_text().contains("bytes"));
    }

    #[test]
    fn theme_defaults_to_the_system_and_reads_every_choice() {
        assert_eq!(Settings::default().theme, ThemeChoice::System);
        for t in ThemeChoice::ALL {
            let s = Settings {
                theme: t,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&s.to_text()).theme, t);
        }
        assert!(!Settings::default().to_text().contains("theme"));
        let s = Settings::parse("theme = light\ntheme = blue\n");
        assert_eq!(s.theme, ThemeChoice::Light);
        assert_eq!(s.problems.len(), 1);
        let mut s = Settings::parse("theme = dark\ndensity = compact\nfilter = code\n");
        s.reset(Page::Appearance);
        assert_eq!(s.theme, ThemeChoice::System);
        assert_eq!(s.density, Density::Comfortable);
        assert_eq!(s.options.filter, Filter::Code, "other pages stay");
    }

    #[test]
    fn menu_bar_and_details_default_to_on() {
        let d = Settings::default();
        assert!(d.menu_bar && d.status_details);
        let text = d.to_text();
        assert!(!text.contains("menu-bar") && !text.contains("status-details"));
        let s = Settings::parse("menu-bar = off\nstatus-details = no\n");
        assert!(!s.menu_bar && !s.status_details);
        let mut s = Settings::parse("menu-bar = false\non-close = hide\n");
        s.reset(Page::Appearance);
        assert!(s.menu_bar);
        assert_eq!(s.on_close, OnClose::Hide, "the Window page stays");
    }

    #[test]
    fn language_defaults_to_the_system_and_reads_every_choice() {
        assert_eq!(Settings::default().language, LanguageChoice::System);
        assert!(!Settings::default().to_text().contains("language"));
        for l in LanguageChoice::ALL {
            let s = Settings {
                language: l,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&s.to_text()).language, l);
        }
        let s = Settings::parse("language = az\nlanguage = de\n");
        assert_eq!(s.language, LanguageChoice::Fixed(Lang::Az));
        assert_eq!(s.problems.len(), 1, "an unknown language is reported");
        assert!(
            Settings::parse("language = tr\n")
                .to_text()
                .contains("language = tr\n")
        );
        let mut s = Settings::parse("language = en\nfilter = code\n");
        s.reset(Page::Appearance);
        assert_eq!(s.language, LanguageChoice::System);
        assert_eq!(s.options.filter, Filter::Code);
        assert_eq!(LanguageChoice::Fixed(Lang::Tr).resolve(), Lang::Tr);
    }

    #[test]
    fn density_defaults_to_comfortable() {
        assert_eq!(Settings::default().density, Density::Comfortable);
        assert!(!Settings::default().to_text().contains("density"));
        for d in Density::ALL {
            let s = Settings {
                density: d,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&s.to_text()).density, d);
        }
        assert_eq!(Settings::parse("density = tiny\n").problems.len(), 1);
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
    fn shortcuts_round_trip_and_only_changes_are_written() {
        let text = "\
shortcut = edit.copy list Ctrl+C; global Ctrl+Insert
shortcut = file.export-results
shortcut = search.previous search Alt+Comma
";
        let s = Settings::parse(text);
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        let key = |scope, text| (scope, Chord::parse(text).unwrap());
        assert_eq!(
            s.shortcuts,
            [
                (
                    "edit.copy".to_owned(),
                    vec![
                        key(Scope::List, "Ctrl+C"),
                        key(Scope::Global, "Ctrl+Insert")
                    ]
                ),
                ("file.export-results".to_owned(), vec![]),
                (
                    "search.previous".to_owned(),
                    vec![key(Scope::Search, "Alt+Comma")]
                ),
            ]
        );
        let out = s.to_text();
        for line in text.lines() {
            assert!(out.contains(&format!("{line}\n")), "{line} in {out}");
        }
        assert_eq!(Settings::parse(&out), s);
        // A later line for the same command wins.
        let s = Settings::parse("shortcut = edit.copy\nshortcut = edit.copy list F9\n");
        assert_eq!(
            s.shortcuts,
            [("edit.copy".to_owned(), vec![key(Scope::List, "F9")])]
        );
        // The defaults write nothing.
        let mut s = Settings::default();
        s.set_shortcuts(commands::Keymap::default().overrides());
        assert!(s.shortcuts.is_empty());
        assert!(s.to_text().lines().all(|l| l.starts_with('#')));
    }

    #[test]
    fn unreadable_shortcuts_are_kept_and_reported() {
        let text = "\
shortcut = edit.nope list Ctrl+C
shortcut = edit.copy list Ctrl+Hyper
shortcut = edit.copy nowhere Ctrl+C
shortcut = edit.cut list F9
";
        let s = Settings::parse(text);
        let lines: Vec<usize> = s.problems.iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, [1, 2, 3]);
        assert_eq!(s.shortcuts.len(), 1);
        let out = s.to_text();
        assert!(out.contains("shortcut = edit.nope list Ctrl+C\n"));
        assert!(out.contains("shortcut = edit.copy list Ctrl+Hyper\n"));
        // New keys for a command drop its unreadable lines, not others.
        let mut s = s;
        s.set_shortcuts(vec![("edit.copy".to_owned(), vec![])]);
        let out = s.to_text();
        assert!(!out.contains("Ctrl+Hyper") && !out.contains("nowhere"));
        assert!(out.contains("edit.nope") && out.contains("shortcut = edit.copy\n"));
        assert!(!out.contains("edit.cut"));
        // Restoring the defaults removes every shortcut line.
        s.reset_shortcuts();
        assert!(!s.to_text().contains("shortcut"));
    }

    #[test]
    fn reset_touches_one_page_only() {
        let mut s = Settings::parse(
            "root = /a\nexclude = /a/b\nfilter = code\nhistory-size = 5\non-close = hide\nfolder-size = none\nsearch-as-you-type = no\n",
        );
        assert!(!s.as_you_type);
        s.reset(Page::Search);
        assert!(s.as_you_type);
        assert_eq!(s.options, QueryOptions::default());
        assert_eq!(s.history_size, DEFAULT_HISTORY);
        assert_eq!(s.roots, [PathBuf::from("/a")]);
        assert_eq!(s.on_close, OnClose::Hide);
        assert_eq!(s.folder_size, FolderSize::None);
        s.use_excludes = false;
        s.exclude_files = "*.tmp".into();
        s.watch_changes = false;
        s.reset(Page::Indexing);
        assert!(s.roots.is_empty() && s.excludes.is_empty());
        assert!(s.use_excludes && s.exclude_files.is_empty() && s.watch_changes);
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
        // No temporary file is left behind; the lock file stays.
        let mut names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["settings", "settings.lock"]);
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
