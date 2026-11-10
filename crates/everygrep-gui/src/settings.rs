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
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use everygrep_core::filter::Filter;
use everygrep_core::query::{HiddenFiles, QueryOptions};
use everygrep_core::rules::{IndexRules, RuleSpec};
use everygrep_core::sort::FolderSize;

use crate::commands::{self, Chord, Scope};
use crate::engine::{DriveSettings, FileListSetting};
use crate::format::{SizeStandard, SizeUnit};
use crate::i18n::{self, Lang};
use crate::schedule::{self, Schedule};
use crate::search_history::Order;

/// Search history entries kept by default.
pub const DEFAULT_HISTORY: usize = 200;
/// Opening more items than this at once asks first, by default.
pub const DEFAULT_OPEN_CONFIRM: usize = 32;
/// The largest number the open question can be set to.
pub const MAX_OPEN_CONFIRM: usize = 10_000;
/// Background tabs that keep their results in memory, by default and at
/// most.
pub const DEFAULT_TAB_RESULTS: usize = 10;
pub const MAX_TAB_RESULTS: usize = 1000;
/// The name of the window's index file in a chosen folder, as in the
/// cache folder.
pub const INDEX_FILE_NAME: &str = "index.bin";
/// Hours between rescans, and the time of day of a daily one (03:00), when
/// that kind is first chosen.
pub const DEFAULT_RESCAN_HOURS: u32 = 24;
pub const DEFAULT_RESCAN_TIME: u32 = 3 * 60;
/// The longest search delay in milliseconds.
pub const MAX_SEARCH_DELAY: u32 = 5000;
/// Most search history entries that can be kept.
pub const MAX_HISTORY: usize = 1000;
/// Widths outside this range in pixels are ignored when read.
const WIDTH_RANGE: std::ops::RangeInclusive<f32> = 1.0..=10_000.0;

/// What closing the window does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnClose {
    /// End the program (the default).
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

/// App-owned dark surfaces; light colors are independent of this choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DarkStyle {
    #[default]
    Slate,
    Layered,
    Graphite,
    Classic,
}

impl DarkStyle {
    pub const ALL: [Self; 4] = [Self::Slate, Self::Layered, Self::Graphite, Self::Classic];

    fn id(self) -> &'static str {
        match self {
            Self::Slate => "slate",
            Self::Layered => "layered",
            Self::Graphite => "graphite",
            Self::Classic => "classic",
        }
    }
}

/// App-owned light surfaces; dark colors are independent of this choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LightStyle {
    #[default]
    Classic,
    Cool,
    Layered,
    Neutral,
}

impl LightStyle {
    pub const ALL: [Self; 4] = [Self::Classic, Self::Cool, Self::Layered, Self::Neutral];

    fn id(self) -> &'static str {
        match self {
            Self::Cool => "cool",
            Self::Layered => "layered",
            Self::Neutral => "neutral",
            Self::Classic => "classic",
        }
    }
}

/// Accent preference; Blue is the default, also for settings files without
/// an accent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccentChoice {
    System,
    #[default]
    Blue,
    Green,
    Teal,
    Purple,
    Orange,
    Pink,
}

impl AccentChoice {
    pub const ALL: [Self; 7] = [
        Self::System,
        Self::Blue,
        Self::Green,
        Self::Teal,
        Self::Purple,
        Self::Orange,
        Self::Pink,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Teal => "teal",
            Self::Purple => "purple",
            Self::Orange => "orange",
            Self::Pink => "pink",
        }
    }
}

/// How tightly the result list is packed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    /// 28 px rows (the default).
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
    pub show_md5: bool,
    pub show_sha256: bool,
    pub show_crc32: bool,
    pub show_name_freq: bool,
    pub show_ext_freq: bool,
    pub show_size_freq: bool,
    pub show_full_path: bool,
    pub full_path: Option<f32>,
    pub show_parent_path: bool,
    pub parent_path: Option<f32>,
    pub show_stem: bool,
    pub stem: Option<f32>,
    pub show_name_length: bool,
    pub name_length: Option<f32>,
    pub show_path_length: bool,
    pub path_length: Option<f32>,
    pub show_disk_size: bool,
    pub disk_size: Option<f32>,
    pub show_total_size: bool,
    pub total_size: Option<f32>,
    pub show_child_count: bool,
    pub child_count: Option<f32>,
    pub show_child_files: bool,
    pub child_files: Option<f32>,
    pub show_child_folders: bool,
    pub child_folders: Option<f32>,
    pub show_desc_count: bool,
    pub desc_count: Option<f32>,
    pub show_desc_files: bool,
    pub desc_files: Option<f32>,
    pub show_desc_folders: bool,
    pub desc_folders: Option<f32>,
    pub show_folder_depth: bool,
    pub folder_depth: Option<f32>,
    pub show_hard_links: bool,
    pub hard_links: Option<f32>,
    pub show_shortcut_target: bool,
    pub shortcut_target: Option<f32>,
    pub show_owner: bool,
    pub owner: Option<f32>,
    pub show_width: bool,
    pub width: Option<f32>,
    pub show_height: bool,
    pub height: Option<f32>,
    pub show_dimensions: bool,
    pub dimensions: Option<f32>,
    pub show_length: bool,
    pub length: Option<f32>,
    pub show_bit_rate: bool,
    pub bit_rate: Option<f32>,
    pub show_title: bool,
    pub title: Option<f32>,
    pub show_artist: bool,
    pub artist: Option<f32>,
    pub show_album: bool,
    pub album: Option<f32>,
    pub show_year: bool,
    pub year: Option<f32>,
    pub show_track: bool,
    pub track: Option<f32>,
    pub show_genre: bool,
    pub genre: Option<f32>,
    pub show_rating: bool,
    pub rating: Option<f32>,
    pub show_tags: bool,
    pub tags: Option<f32>,
    pub show_page_count: bool,
    pub page_count: Option<f32>,
    pub show_word_count: bool,
    pub word_count: Option<f32>,
    pub show_camera_maker: bool,
    pub camera_maker: Option<f32>,
    pub show_camera_model: bool,
    pub camera_model: Option<f32>,
    pub show_date_taken: bool,
    pub date_taken: Option<f32>,
    pub show_gps_lat: bool,
    pub gps_lat: Option<f32>,
    pub show_gps_long: bool,
    pub show_sha1: bool,
    pub show_sha384: bool,
    pub show_sha512: bool,
    pub show_xxhash: bool,
    pub show_dupe_group: bool,
    pub gps_long: Option<f32>,
    pub sha1: Option<f32>,
    pub sha384: Option<f32>,
    pub sha512: Option<f32>,
    pub xxhash: Option<f32>,
    pub dupe_group: Option<f32>,
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
    pub md5: Option<f32>,
    pub sha256: Option<f32>,
    pub crc32: Option<f32>,
    pub name_freq: Option<f32>,
    pub ext_freq: Option<f32>,
    pub size_freq: Option<f32>,
}

/// Default and range of [`Settings::event_buffer_kib`].
pub const DEFAULT_EVENT_BUFFER_KIB: u32 = 64;
pub const EVENT_BUFFER_KIB: std::ops::RangeInclusive<u32> = 16..=4096;

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
    /// The folder of the index file (`index.bin`) of the window over the
    /// default index; `None` for the cache folder.
    pub index_folder: Option<PathBuf>,
    /// Whether the index holds the size and modification time of every
    /// file; off reads only names (folders keep their times for catch-up).
    pub index_file_details: bool,
    /// When the folders to index are scanned again by themselves: the kind
    /// (see [`Schedule::KINDS`]), and the hours and the time of day kept
    /// for their kinds also while another is chosen.
    pub rescan: usize,
    pub rescan_hours: u32,
    /// Minute of the day.
    pub rescan_time: u32,
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
    /// Whether the result list stays empty while the search field is
    /// empty; off lists every indexed entry (with the chosen filter).
    pub hide_results_when_empty: bool,
    /// Whether the search scope selector is shown next to the search field;
    /// hidden, every search lists all locations.
    pub scope_selector: bool,
    /// Milliseconds a search as you type waits for the next key; 0 runs it
    /// at once.
    pub search_delay_ms: u32,
    /// Searches kept for Back and Forward; 0 turns the history off.
    pub history_size: usize,
    /// Whether the search history is saved between sessions.
    pub history_on_disk: bool,
    /// Whether changes made to the settings file by another program (or
    /// another window) are applied while the window runs.
    pub reload_changed: bool,
    /// Whether such a change is applied only after the user agrees.
    pub ask_reload: bool,
    /// Recording can be disabled without discarding existing entries.
    pub history_enabled: bool,
    /// Retention in days; 0 means forever.
    pub history_days: u32,
    pub history_entries: usize,
    pub history_sort: Order,
    pub always_suggestions: bool,
    pub open_searches_in_tabs: bool,
    pub switch_to_new_tab: bool,
    pub restore_session: bool,
    pub export_filters: bool,
    pub export_bookmarks: bool,
    pub on_close: OnClose,
    /// What the Size column shows for folders, and sorts them by.
    pub folder_size: FolderSize,
    /// Whether the optional columns and the column widths are kept between
    /// sessions.
    pub remember_columns: bool,
    /// Whether saved search layouts may replace the user's column defaults.
    pub saved_columns_as_defaults: bool,
    pub columns: Columns,
    /// Whether renaming preselects the name without its extension; off
    /// selects the whole name.
    pub rename_name_only: bool,
    pub fileops: crate::fileops_settings::Settings,
    /// How files are read for `content:` terms (Search page).
    pub content: crate::content_settings::Settings,
    pub properties: crate::property_settings::Settings,
    pub preview: crate::preview::Options,
    /// Whether a double-click on the Folder cell of a row opens the
    /// containing folder instead of the item.
    pub path_click_opens_folder: bool,
    /// Whether Copy full path puts each path in double quotes, as Copy
    /// full path with quotes does.
    pub copy_full_path_quotes: bool,
    /// Whether the Name cell of a drive's root shows the drive's label,
    /// `Label (mount point)`.
    pub drive_root_labels: bool,
    /// Whether rows on screen whose size or date the index lacks read them
    /// from disk by themselves (Load file details).
    pub read_extended_auto: bool,
    /// Whether typing in the result list moves to a name that starts with
    /// the typed text (see `jump.rs`).
    pub type_to_jump: bool,
    /// Last successful result-export file type.
    pub export_format: crate::export::Format,
    /// Prefix spreadsheet formula cells in CSV and TSV exports.
    pub export_protect_formulas: bool,
    /// Opening more items than this at once asks first; 0 never asks.
    pub open_confirm_over: usize,
    /// The background tabs shown last keep their results in memory; the
    /// others search again when shown.
    pub tab_results_kept: usize,
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
    pub dark_style: DarkStyle,
    pub light_style: LightStyle,
    pub accent: AccentChoice,
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
    /// Whether the window shows the status line at its bottom; without it,
    /// progress and messages appear in a small box there while they last.
    pub status_bar: bool,
    /// Whether the window shows a toolbar below the menu bar.
    pub toolbar: bool,
    /// Back, Forward, New tab, Rescan, Filters, Preview and Settings.
    pub toolbar_buttons: [bool; 7],
    /// Show words beside the Back and Forward arrows.
    pub toolbar_navigation_labels: bool,
    /// Open the two search selector lists below their buttons.
    pub dropdown_below: bool,
    /// The size of the result list's text, icons and rows, in percent.
    pub zoom: u32,
    pub folders_visible: bool,
    pub preview_visible: bool,
    pub preview_width: f32,
    pub folders_width: f32,
    pub folders_recursive: bool,
    /// Whether folders that hold none of the current results are grey.
    pub folders_dim_empty: bool,
    pub language: LanguageChoice,
    /// Removable drives: indexed automatically, kept when removed, and the
    /// drives never indexed.
    pub drives: DriveSettings,
    /// File lists searched as offline entries, in order.
    pub file_lists: Vec<FileListSetting>,
    /// Leave entries of offline drives and file lists out of the results.
    pub hide_offline: bool,
    /// Whether a shortcut with Ctrl, Alt or Super is taken by the key's
    /// position, whatever the keyboard layout; off by the character it
    /// types.
    pub shortcuts_by_position: bool,
    /// Read a file list again when its file changes on disk.
    pub reload_changed_lists: bool,
    /// Folders to index whose subfolders' contents are left out.
    pub shallow_roots: Vec<PathBuf>,
    /// Size of the buffer change events are read into, in KiB.
    pub event_buffer_kib: u32,
    /// After lost change events (a full kernel queue): build the index
    /// again (true) or only catch up by folder times (false).
    pub rescan_after_lost_events: bool,
    /// Keep the last changes of the index in memory (Index > Index changes).
    pub index_journal: bool,
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
            index_folder: None,
            index_file_details: true,
            rescan: 0,
            rescan_hours: DEFAULT_RESCAN_HOURS,
            rescan_time: DEFAULT_RESCAN_TIME,
            options: QueryOptions::default(),
            as_you_type: true,
            show_mode_badges: true,
            filter_wrap: true,
            select_search_on_click: true,
            clear_selection_on_search: true,
            hide_results_when_empty: false,
            scope_selector: true,
            search_delay_ms: 0,
            history_size: DEFAULT_HISTORY,
            history_on_disk: false,
            reload_changed: true,
            ask_reload: true,
            history_enabled: true,
            history_days: 90,
            history_entries: crate::search_history::MAX_ENTRIES,
            history_sort: Order::Date,
            always_suggestions: true,
            open_searches_in_tabs: true,
            switch_to_new_tab: true,
            restore_session: false,
            export_filters: false,
            export_bookmarks: false,
            on_close: OnClose::Quit,
            folder_size: FolderSize::Total,
            remember_columns: true,
            saved_columns_as_defaults: false,
            columns: Columns::default(),
            rename_name_only: true,
            fileops: crate::fileops_settings::Settings::default(),
            content: crate::content_settings::Settings::default(),
            properties: crate::property_settings::Settings::default(),
            preview: crate::preview::Options::default(),
            path_click_opens_folder: true,
            copy_full_path_quotes: false,
            drive_root_labels: true,
            read_extended_auto: false,
            type_to_jump: true,
            export_format: crate::export::Format::Csv,
            export_protect_formulas: true,
            open_confirm_over: DEFAULT_OPEN_CONFIRM,
            tab_results_kept: DEFAULT_TAB_RESULTS,
            focus_on_show: true,
            dates_newest_first: false,
            sizes_largest_first: false,
            theme: ThemeChoice::System,
            dark_style: DarkStyle::Slate,
            light_style: LightStyle::Classic,
            accent: AccentChoice::Blue,
            density: Density::Comfortable,
            alternate_rows: true,
            size_unit: SizeUnit::Auto,
            size_standard: SizeStandard::Jedec,
            status_details: true,
            menu_bar: true,
            status_bar: true,
            toolbar: true,
            toolbar_buttons: [true; 7],
            toolbar_navigation_labels: false,
            dropdown_below: true,
            zoom: crate::zoom::DEFAULT,
            folders_visible: false,
            preview_visible: false,
            preview_width: 300.0,
            folders_width: 220.0,
            folders_recursive: true,
            folders_dim_empty: true,
            language: LanguageChoice::System,
            drives: DriveSettings::default(),
            file_lists: Vec::new(),
            hide_offline: false,
            shortcuts_by_position: true,
            reload_changed_lists: true,
            shallow_roots: Vec::new(),
            event_buffer_kib: DEFAULT_EVENT_BUFFER_KIB,
            rescan_after_lost_events: true,
            index_journal: false,
            shortcuts: Vec::new(),
            unknown: Vec::new(),
            problems: Vec::new(),
        }
    }
}

/// Keys that are written once per value (a folder, a drive, a command's
/// keys); the raw settings table shows them but edits them only on their
/// pages.
pub const LIST_KEYS: [&str; 7] = [
    "root",
    "exclude",
    "ignore-drive",
    "file-list",
    "file-list-off",
    "shortcut",
    "root-without-subfolders",
];

/// One line of the raw settings table: a key and its value as the file
/// writes them, its default (`None` for list keys and unknown keys), and
/// whether this version knows the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRow {
    pub key: String,
    pub value: String,
    pub default: Option<String>,
    pub known: bool,
}

impl RawRow {
    /// Whether the value differs from the default.
    pub fn modified(&self) -> bool {
        self.default.as_deref() != Some(self.value.as_str())
    }

    /// Whether it can be edited in the table.
    pub fn editable(&self) -> bool {
        self.known && !LIST_KEYS.contains(&self.key.as_str())
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

/// Where the current column layout came from. Unedited defaults never
/// overwrite newer choices made in another tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnSource {
    #[default]
    Defaults = 0,
    Saved = 1,
    Edited = 2,
}

impl ColumnSource {
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Saved,
            2 => Self::Edited,
            _ => Self::Defaults,
        }
    }
}

impl Settings {
    pub fn can_remember_view_columns(&self, source: ColumnSource) -> bool {
        self.remember_columns
            && (source == ColumnSource::Edited
                || (source == ColumnSource::Saved && self.saved_columns_as_defaults))
    }

    /// Saved layouts belong to their views unless explicitly allowed as defaults.
    pub fn remember_view_columns(&mut self, columns: Columns, source: ColumnSource) {
        if self.can_remember_view_columns(source) {
            self.columns = columns;
        }
    }

    /// The index file of the window over the default index: `index.bin`
    /// in the chosen folder, or the default file in the cache folder.
    pub fn index_file(&self) -> Option<PathBuf> {
        match &self.index_folder {
            Some(folder) => Some(folder.join(INDEX_FILE_NAME)),
            None => everygrep_core::persist::default_path(),
        }
    }

    /// When the folders to index are scanned again by themselves.
    pub fn schedule(&self) -> Schedule {
        match self.rescan {
            1 => Schedule::EveryHours(self.rescan_hours),
            2 => Schedule::Daily(self.rescan_time),
            _ => Schedule::Never,
        }
    }

    /// The rules of the excluded folders and file filters.
    pub fn rule_spec(&self) -> RuleSpec {
        RuleSpec {
            shallow: self.shallow_roots.clone(),
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
        self.shallow_roots = spec.shallow.clone();
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
            "file-list" | "file-list-off" => {
                let Some(path) = unescape(value).filter(|p| p.first() == Some(&b'/')) else {
                    return false;
                };
                let path = PathBuf::from(OsString::from_vec(path));
                if !self.file_lists.iter().any(|l| l.path == path) {
                    self.file_lists.push(FileListSetting {
                        path,
                        enabled: key == "file-list",
                    });
                }
                true
            }
            "hide-offline-entries" => put_flag(&mut self.hide_offline),
            "reload-changed-file-lists" => put_flag(&mut self.reload_changed_lists),
            "rescan-after-lost-events" => put_flag(&mut self.rescan_after_lost_events),
            "index-changes" => put_flag(&mut self.index_journal),
            "event-buffer-kib" => value
                .parse::<u32>()
                .ok()
                .filter(|n| EVENT_BUFFER_KIB.contains(n))
                .map(|n| self.event_buffer_kib = n)
                .is_some(),
            "root-without-subfolders" => {
                let Some(path) = unescape(value).filter(|p| p.first() == Some(&b'/')) else {
                    return false;
                };
                let path = PathBuf::from(OsString::from_vec(path));
                if !self.shallow_roots.contains(&path) {
                    self.shallow_roots.push(path);
                }
                true
            }
            "use-exclude-list" => put_flag(&mut self.use_excludes),
            "watch-changes" => put_flag(&mut self.watch_changes),
            "index-folder" => {
                let Some(path) = unescape(value).filter(|p| p.first() == Some(&b'/')) else {
                    return false;
                };
                self.index_folder = Some(PathBuf::from(OsString::from_vec(path)));
                true
            }
            "index-file-details" => put_flag(&mut self.index_file_details),
            "rescan" => Schedule::KINDS
                .iter()
                .position(|k| *k == value)
                .map(|k| self.rescan = k)
                .is_some(),
            "rescan-hours" => value
                .parse::<u32>()
                .ok()
                .filter(|h| (1..=schedule::MAX_HOURS).contains(h))
                .map(|h| self.rescan_hours = h)
                .is_some(),
            "rescan-time" => schedule::parse_time(value)
                .map(|m| self.rescan_time = m)
                .is_some(),
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
            "hide-results-when-empty" => put_flag(&mut self.hide_results_when_empty),
            "show-search-scope" => put_flag(&mut self.scope_selector),
            "search-delay" => value
                .parse::<u32>()
                .ok()
                .filter(|n| *n <= MAX_SEARCH_DELAY)
                .map(|n| self.search_delay_ms = n)
                .is_some(),
            "history-size" => value
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= MAX_HISTORY)
                .map(|n| self.history_size = n)
                .is_some(),
            "search-history-limit" => value
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=crate::search_history::MAX_ENTRIES).contains(n))
                .map(|n| self.history_entries = n)
                .is_some(),
            "history-on-disk" => put_flag(&mut self.history_on_disk),
            "reload-changed-settings" => put_flag(&mut self.reload_changed),
            "ask-before-reloading-settings" => put_flag(&mut self.ask_reload),
            "history-enabled" => put_flag(&mut self.history_enabled),
            "always-search-suggestions" => put_flag(&mut self.always_suggestions),
            "history-sort" => Order::ALL
                .into_iter()
                .find(|o| o.id() == value)
                .map(|o| self.history_sort = o)
                .is_some(),
            "history-days" => value
                .parse::<u32>()
                .ok()
                .filter(|n| *n <= 36500)
                .map(|n| self.history_days = n)
                .is_some(),
            "open-searches-in-tabs" => put_flag(&mut self.open_searches_in_tabs),
            "switch-to-new-tab" => put_flag(&mut self.switch_to_new_tab),
            "restore-session" => put_flag(&mut self.restore_session),
            "export-filters" => put_flag(&mut self.export_filters),
            "export-bookmarks" => put_flag(&mut self.export_bookmarks),
            "on-close" => OnClose::ALL
                .into_iter()
                .find(|o| o.id() == value)
                .map(|o| self.on_close = o)
                .is_some(),
            "folder-size" => FolderSize::from_id(value)
                .map(|f| self.folder_size = f)
                .is_some(),
            "remember-columns" => put_flag(&mut self.remember_columns),
            "saved-columns-as-defaults" => put_flag(&mut self.saved_columns_as_defaults),
            "rename-select-name-only" => put_flag(&mut self.rename_name_only),
            "open-folder-on-path-double-click" => put_flag(&mut self.path_click_opens_folder),
            "copy-full-path-quotes" => put_flag(&mut self.copy_full_path_quotes),
            "drive-root-labels" => put_flag(&mut self.drive_root_labels),
            "shortcuts-by-key-position" => put_flag(&mut self.shortcuts_by_position),
            "load-file-details" => put_flag(&mut self.read_extended_auto),
            "type-to-jump" => put_flag(&mut self.type_to_jump),
            "export-protect-formulas" => put_flag(&mut self.export_protect_formulas),
            "export-format" => crate::export::Format::ALL
                .into_iter()
                .find(|f| f.extension() == value)
                .map(|f| self.export_format = f)
                .is_some(),
            "tab-results-kept" => value
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= MAX_TAB_RESULTS)
                .map(|n| self.tab_results_kept = n)
                .is_some(),
            "confirm-open-over" => value
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= MAX_OPEN_CONFIRM)
                .map(|n| self.open_confirm_over = n)
                .is_some(),
            "focus-search-on-show" => put_flag(&mut self.focus_on_show),
            "sort-dates-newest-first" => put_flag(&mut self.dates_newest_first),
            "sort-sizes-largest-first" => put_flag(&mut self.sizes_largest_first),
            "accent-color" => AccentChoice::ALL
                .into_iter()
                .find(|a| a.id() == value)
                .map(|a| self.accent = a)
                .is_some(),
            "dark-style" => DarkStyle::ALL
                .into_iter()
                .find(|t| t.id() == value)
                .map(|t| self.dark_style = t)
                .is_some(),
            "light-style" => LightStyle::ALL
                .into_iter()
                .find(|t| t.id() == value)
                .map(|t| self.light_style = t)
                .is_some(),
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
            "preview-visible" => put_flag(&mut self.preview_visible),
            "preview-width" => width(value)
                .map(|v| self.preview_width = v.clamp(180.0, 700.0))
                .is_some(),
            "folders-visible" => put_flag(&mut self.folders_visible),
            "folders-subfolders" => put_flag(&mut self.folders_recursive),
            "folders-grey-empty" => put_flag(&mut self.folders_dim_empty),
            "folders-width" => width(value)
                .map(|v| self.folders_width = v.clamp(140.0, 600.0))
                .is_some(),
            "menu-bar" => put_flag(&mut self.menu_bar),
            "status-bar" => put_flag(&mut self.status_bar),
            "toolbar" => put_flag(&mut self.toolbar),
            "dropdown-below" => put_flag(&mut self.dropdown_below),
            "toolbar-navigation-labels" => put_flag(&mut self.toolbar_navigation_labels),
            "toolbar-button-back" => put_flag(&mut self.toolbar_buttons[0]),
            "toolbar-button-forward" => put_flag(&mut self.toolbar_buttons[1]),
            "toolbar-button-new-tab" => put_flag(&mut self.toolbar_buttons[2]),
            "toolbar-button-rescan" => put_flag(&mut self.toolbar_buttons[3]),
            "toolbar-button-filters" => put_flag(&mut self.toolbar_buttons[4]),
            "toolbar-button-preview" => put_flag(&mut self.toolbar_buttons[5]),
            "toolbar-button-settings" => put_flag(&mut self.toolbar_buttons[6]),
            "zoom" => value
                .parse::<u32>()
                .ok()
                .filter(|z| crate::zoom::valid(*z))
                .map(|z| self.zoom = z)
                .is_some(),
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
            "show-md5-column" => put_flag(&mut self.columns.show_md5),
            "show-sha256-column" => put_flag(&mut self.columns.show_sha256),
            "show-crc32-column" => put_flag(&mut self.columns.show_crc32),
            "show-same-name-column" => put_flag(&mut self.columns.show_name_freq),
            "show-same-extension-column" => put_flag(&mut self.columns.show_ext_freq),
            "show-same-size-column" => put_flag(&mut self.columns.show_size_freq),
            "show-full-path-column" => put_flag(&mut self.columns.show_full_path),
            "full-path-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.full_path = Some(w);
                true
            }
            "show-parent-path-column" => put_flag(&mut self.columns.show_parent_path),
            "parent-path-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.parent_path = Some(w);
                true
            }
            "show-stem-column" => put_flag(&mut self.columns.show_stem),
            "stem-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.stem = Some(w);
                true
            }
            "show-name-length-column" => put_flag(&mut self.columns.show_name_length),
            "name-length-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.name_length = Some(w);
                true
            }
            "show-path-length-column" => put_flag(&mut self.columns.show_path_length),
            "path-length-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.path_length = Some(w);
                true
            }
            "show-disk-size-column" => put_flag(&mut self.columns.show_disk_size),
            "disk-size-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.disk_size = Some(w);
                true
            }
            "show-total-size-column" => put_flag(&mut self.columns.show_total_size),
            "total-size-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.total_size = Some(w);
                true
            }
            "show-items-column" => put_flag(&mut self.columns.show_child_count),
            "items-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.child_count = Some(w);
                true
            }
            "show-files-inside-column" => put_flag(&mut self.columns.show_child_files),
            "files-inside-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.child_files = Some(w);
                true
            }
            "show-subfolders-column" => put_flag(&mut self.columns.show_child_folders),
            "subfolders-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.child_folders = Some(w);
                true
            }
            "show-deep-items-column" => put_flag(&mut self.columns.show_desc_count),
            "deep-items-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.desc_count = Some(w);
                true
            }
            "show-deep-files-column" => put_flag(&mut self.columns.show_desc_files),
            "deep-files-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.desc_files = Some(w);
                true
            }
            "show-deep-subfolders-column" => put_flag(&mut self.columns.show_desc_folders),
            "deep-subfolders-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.desc_folders = Some(w);
                true
            }
            "show-folder-depth-column" => put_flag(&mut self.columns.show_folder_depth),
            "folder-depth-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.folder_depth = Some(w);
                true
            }
            "show-hard-links-column" => put_flag(&mut self.columns.show_hard_links),
            "hard-links-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.hard_links = Some(w);
                true
            }
            "show-shortcut-target-column" => put_flag(&mut self.columns.show_shortcut_target),
            "shortcut-target-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.shortcut_target = Some(w);
                true
            }
            "show-owner-column" => put_flag(&mut self.columns.show_owner),
            "owner-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.owner = Some(w);
                true
            }
            "show-width-column" => put_flag(&mut self.columns.show_width),
            "width-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.width = Some(w);
                true
            }
            "show-height-column" => put_flag(&mut self.columns.show_height),
            "height-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.height = Some(w);
                true
            }
            "show-dimensions-column" => put_flag(&mut self.columns.show_dimensions),
            "dimensions-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.dimensions = Some(w);
                true
            }
            "show-length-column" => put_flag(&mut self.columns.show_length),
            "length-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.length = Some(w);
                true
            }
            "show-bit-rate-column" => put_flag(&mut self.columns.show_bit_rate),
            "bit-rate-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.bit_rate = Some(w);
                true
            }
            "show-title-column" => put_flag(&mut self.columns.show_title),
            "title-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.title = Some(w);
                true
            }
            "show-artist-column" => put_flag(&mut self.columns.show_artist),
            "artist-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.artist = Some(w);
                true
            }
            "show-album-column" => put_flag(&mut self.columns.show_album),
            "album-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.album = Some(w);
                true
            }
            "show-year-column" => put_flag(&mut self.columns.show_year),
            "year-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.year = Some(w);
                true
            }
            "show-track-column" => put_flag(&mut self.columns.show_track),
            "track-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.track = Some(w);
                true
            }
            "show-genre-column" => put_flag(&mut self.columns.show_genre),
            "genre-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.genre = Some(w);
                true
            }
            "show-rating-column" => put_flag(&mut self.columns.show_rating),
            "rating-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.rating = Some(w);
                true
            }
            "show-tags-column" => put_flag(&mut self.columns.show_tags),
            "tags-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.tags = Some(w);
                true
            }
            "show-page-count-column" => put_flag(&mut self.columns.show_page_count),
            "page-count-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.page_count = Some(w);
                true
            }
            "show-word-count-column" => put_flag(&mut self.columns.show_word_count),
            "word-count-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.word_count = Some(w);
                true
            }
            "show-camera-maker-column" => put_flag(&mut self.columns.show_camera_maker),
            "camera-maker-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.camera_maker = Some(w);
                true
            }
            "show-camera-model-column" => put_flag(&mut self.columns.show_camera_model),
            "camera-model-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.camera_model = Some(w);
                true
            }
            "show-date-taken-column" => put_flag(&mut self.columns.show_date_taken),
            "date-taken-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.date_taken = Some(w);
                true
            }
            "show-gps-lat-column" => put_flag(&mut self.columns.show_gps_lat),
            "gps-lat-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.gps_lat = Some(w);
                true
            }
            "show-gps-long-column" => put_flag(&mut self.columns.show_gps_long),
            "show-sha1-column" => put_flag(&mut self.columns.show_sha1),
            "show-sha384-column" => put_flag(&mut self.columns.show_sha384),
            "show-sha512-column" => put_flag(&mut self.columns.show_sha512),
            "show-xxhash-column" => put_flag(&mut self.columns.show_xxhash),
            "show-dupe-group-column" => put_flag(&mut self.columns.show_dupe_group),
            "gps-long-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.gps_long = Some(w);
                true
            }
            "sha1-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.sha1 = Some(w);
                true
            }
            "sha384-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.sha384 = Some(w);
                true
            }
            "sha512-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.sha512 = Some(w);
                true
            }
            "xxhash-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.xxhash = Some(w);
                true
            }
            "dupe-group-width" => {
                let Some(w) = width(value) else { return false };
                self.columns.dupe_group = Some(w);
                true
            }
            "name-width"
            | "folder-width"
            | "size-width"
            | "modified-width"
            | "type-width"
            | "extension-width"
            | "created-width"
            | "accessed-width"
            | "attributes-width"
            | "md5-width"
            | "sha256-width"
            | "crc32-width"
            | "same-name-width"
            | "same-extension-width"
            | "same-size-width" => {
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
                    "attributes-width" => &mut c.attrs,
                    "md5-width" => &mut c.md5,
                    "sha256-width" => &mut c.sha256,
                    "crc32-width" => &mut c.crc32,
                    "same-name-width" => &mut c.name_freq,
                    "same-extension-width" => &mut c.ext_freq,
                    _ => &mut c.size_freq,
                } = Some(w);
                true
            }
            _ => {
                self.fileops.set(key, value)
                    || self.content.set(key, value)
                    || self.properties.set(key, value)
                    || self.preview.set(key, value)
            }
        }
    }

    /// The text of a settings file: settings that differ from their
    /// defaults, then the unknown lines that were read.
    pub fn to_text(&self) -> String {
        self.write(false)
    }

    /// The lines of the full settings text (every setting, see
    /// [`write`](Self::write)) found in only one of `self` and `other`:
    /// what applying one over the other changes.
    pub fn differing_lines(&self, other: &Settings) -> std::collections::BTreeSet<String> {
        let mine = self.write(true);
        let theirs = other.write(true);
        let mine: std::collections::BTreeSet<&str> = mine.lines().collect();
        let theirs: std::collections::BTreeSet<&str> = theirs.lines().collect();
        mine.symmetric_difference(&theirs)
            .map(|l| (*l).to_owned())
            .collect()
    }

    /// Keeps the running values of keys whose outside changes were declined.
    /// Full text includes defaults, and repeated list keys are replaced together.
    pub fn keeping_declined(
        &self,
        current: &Settings,
        declined: &std::collections::BTreeSet<String>,
    ) -> Settings {
        let keys: std::collections::BTreeSet<&str> = declined
            .iter()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim_ascii()))
            .collect();
        let kept_key = |line: &str| {
            line.split_once('=')
                .is_some_and(|(key, _)| keys.contains(key.trim_ascii()))
        };
        let incoming = self.write(true);
        let running = current.write(true);
        let mut text: String = incoming
            .lines()
            .filter(|line| !kept_key(line))
            .map(|line| format!("{line}\n"))
            .collect();
        text.extend(
            running
                .lines()
                .filter(|line| kept_key(line))
                .map(|line| format!("{line}\n")),
        );
        let mut settings = Settings::parse(&text);
        settings.problems.clone_from(&self.problems);
        settings
    }

    /// The text of a settings file; with `all` every setting is written,
    /// those at their default too (an unset width or folder as an empty
    /// value), for the raw settings table.
    fn write(&self, all: bool) -> String {
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
            if all || value != default {
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
        if all || self.index_folder.is_some() {
            let folder = self.index_folder.as_deref().unwrap_or(Path::new(""));
            line(
                &mut out,
                "index-folder",
                &escape(folder.as_os_str().as_bytes()),
            );
        }
        flag(
            &mut out,
            "index-file-details",
            self.index_file_details,
            d.index_file_details,
        );
        if all || self.rescan != d.rescan {
            line(&mut out, "rescan", Schedule::KINDS[self.rescan]);
        }
        if all || self.rescan_hours != d.rescan_hours {
            line(&mut out, "rescan-hours", &self.rescan_hours.to_string());
        }
        if all || self.rescan_time != d.rescan_time {
            line(
                &mut out,
                "rescan-time",
                &schedule::time_text(self.rescan_time),
            );
        }
        flag(
            &mut out,
            "use-exclude-list",
            self.use_excludes,
            d.use_excludes,
        );
        if all || !self.include_files.is_empty() {
            line(
                &mut out,
                "include-only-files",
                &escape(self.include_files.as_bytes()),
            );
        }
        if all || !self.exclude_files.is_empty() {
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
        if all || self.drives.keep_offline != d.drives.keep_offline {
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
        for list in &self.file_lists {
            let key = if list.enabled {
                "file-list"
            } else {
                "file-list-off"
            };
            line(&mut out, key, &escape(list.path.as_os_str().as_bytes()));
        }
        flag(
            &mut out,
            "hide-offline-entries",
            self.hide_offline,
            d.hide_offline,
        );
        flag(
            &mut out,
            "reload-changed-file-lists",
            self.reload_changed_lists,
            d.reload_changed_lists,
        );
        for root in &self.shallow_roots {
            line(
                &mut out,
                "root-without-subfolders",
                &escape(root.as_os_str().as_bytes()),
            );
        }
        if all || self.event_buffer_kib != d.event_buffer_kib {
            line(
                &mut out,
                "event-buffer-kib",
                &self.event_buffer_kib.to_string(),
            );
        }
        flag(
            &mut out,
            "rescan-after-lost-events",
            self.rescan_after_lost_events,
            d.rescan_after_lost_events,
        );
        flag(
            &mut out,
            "index-changes",
            self.index_journal,
            d.index_journal,
        );
        flag(
            &mut out,
            "open-searches-in-tabs",
            self.open_searches_in_tabs,
            d.open_searches_in_tabs,
        );
        flag(
            &mut out,
            "switch-to-new-tab",
            self.switch_to_new_tab,
            d.switch_to_new_tab,
        );
        flag(
            &mut out,
            "restore-session",
            self.restore_session,
            d.restore_session,
        );
        flag(&mut out, "filter-wrap", self.filter_wrap, d.filter_wrap);
        let (o, od) = (&self.options, &d.options);
        if all || o.hidden_files != od.hidden_files {
            line(&mut out, "hidden-files", o.hidden_files.id());
        }
        if all || o.filter != od.filter {
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
        flag(
            &mut out,
            "hide-results-when-empty",
            self.hide_results_when_empty,
            d.hide_results_when_empty,
        );
        flag(
            &mut out,
            "show-search-scope",
            self.scope_selector,
            d.scope_selector,
        );
        if all || self.search_delay_ms != d.search_delay_ms {
            line(&mut out, "search-delay", &self.search_delay_ms.to_string());
        }
        flag(
            &mut out,
            "history-enabled",
            self.history_enabled,
            d.history_enabled,
        );
        flag(
            &mut out,
            "always-search-suggestions",
            self.always_suggestions,
            d.always_suggestions,
        );
        flag(
            &mut out,
            "export-filters",
            self.export_filters,
            d.export_filters,
        );
        flag(
            &mut out,
            "export-bookmarks",
            self.export_bookmarks,
            d.export_bookmarks,
        );
        if all || self.history_sort != d.history_sort {
            line(&mut out, "history-sort", self.history_sort.id());
        }
        if all || self.history_days != d.history_days {
            line(&mut out, "history-days", &self.history_days.to_string());
        }
        if all || self.history_entries != d.history_entries {
            line(
                &mut out,
                "search-history-limit",
                &self.history_entries.to_string(),
            );
        }
        if all || self.history_size != d.history_size {
            line(&mut out, "history-size", &self.history_size.to_string());
        }
        flag(
            &mut out,
            "history-on-disk",
            self.history_on_disk,
            d.history_on_disk,
        );
        flag(
            &mut out,
            "reload-changed-settings",
            self.reload_changed,
            d.reload_changed,
        );
        flag(
            &mut out,
            "ask-before-reloading-settings",
            self.ask_reload,
            d.ask_reload,
        );
        flag(
            &mut out,
            "preview-visible",
            self.preview_visible,
            d.preview_visible,
        );
        if all || self.preview_width != d.preview_width {
            line(&mut out, "preview-width", &self.preview_width.to_string());
        }
        self.fileops.write(&mut out, all);
        self.content.write(&mut out, all);
        self.properties.write(&mut out, all);
        self.preview.write(&mut out, all);
        if all || self.on_close != d.on_close {
            line(&mut out, "on-close", self.on_close.id());
        }
        if all || self.folder_size != d.folder_size {
            line(&mut out, "folder-size", self.folder_size.id());
        }
        flag(
            &mut out,
            "saved-columns-as-defaults",
            self.saved_columns_as_defaults,
            d.saved_columns_as_defaults,
        );
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
        flag(
            &mut out,
            "drive-root-labels",
            self.drive_root_labels,
            d.drive_root_labels,
        );
        flag(
            &mut out,
            "shortcuts-by-key-position",
            self.shortcuts_by_position,
            d.shortcuts_by_position,
        );
        flag(
            &mut out,
            "load-file-details",
            self.read_extended_auto,
            d.read_extended_auto,
        );
        flag(&mut out, "type-to-jump", self.type_to_jump, d.type_to_jump);
        flag(
            &mut out,
            "export-protect-formulas",
            self.export_protect_formulas,
            d.export_protect_formulas,
        );
        if all || self.export_format != d.export_format {
            line(&mut out, "export-format", self.export_format.extension());
        }
        if all || self.tab_results_kept != d.tab_results_kept {
            line(
                &mut out,
                "tab-results-kept",
                &self.tab_results_kept.to_string(),
            );
        }
        if all || self.open_confirm_over != d.open_confirm_over {
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
        if all || self.accent != d.accent {
            line(&mut out, "accent-color", self.accent.id());
        }
        if all || self.dark_style != d.dark_style {
            line(&mut out, "dark-style", self.dark_style.id());
        }
        if all || self.light_style != d.light_style {
            line(&mut out, "light-style", self.light_style.id());
        }
        if all || self.theme != d.theme {
            line(&mut out, "theme", self.theme.id());
        }
        if all || self.density != d.density {
            line(&mut out, "density", self.density.id());
        }
        flag(
            &mut out,
            "status-details",
            self.status_details,
            d.status_details,
        );
        flag(&mut out, "menu-bar", self.menu_bar, d.menu_bar);
        flag(&mut out, "status-bar", self.status_bar, d.status_bar);
        flag(&mut out, "toolbar", self.toolbar, d.toolbar);
        flag(
            &mut out,
            "dropdown-below",
            self.dropdown_below,
            d.dropdown_below,
        );
        flag(
            &mut out,
            "toolbar-navigation-labels",
            self.toolbar_navigation_labels,
            d.toolbar_navigation_labels,
        );
        flag(
            &mut out,
            "toolbar-button-back",
            self.toolbar_buttons[0],
            d.toolbar_buttons[0],
        );
        flag(
            &mut out,
            "toolbar-button-forward",
            self.toolbar_buttons[1],
            d.toolbar_buttons[1],
        );
        flag(
            &mut out,
            "toolbar-button-new-tab",
            self.toolbar_buttons[2],
            d.toolbar_buttons[2],
        );
        flag(
            &mut out,
            "toolbar-button-rescan",
            self.toolbar_buttons[3],
            d.toolbar_buttons[3],
        );
        flag(
            &mut out,
            "toolbar-button-filters",
            self.toolbar_buttons[4],
            d.toolbar_buttons[4],
        );
        flag(
            &mut out,
            "toolbar-button-preview",
            self.toolbar_buttons[5],
            d.toolbar_buttons[5],
        );
        flag(
            &mut out,
            "toolbar-button-settings",
            self.toolbar_buttons[6],
            d.toolbar_buttons[6],
        );
        if all || self.zoom != d.zoom {
            line(&mut out, "zoom", &self.zoom.to_string());
        }
        flag(
            &mut out,
            "folders-visible",
            self.folders_visible,
            d.folders_visible,
        );
        flag(
            &mut out,
            "folders-subfolders",
            self.folders_recursive,
            d.folders_recursive,
        );
        flag(
            &mut out,
            "folders-grey-empty",
            self.folders_dim_empty,
            d.folders_dim_empty,
        );
        if all || self.folders_width != d.folders_width {
            line(&mut out, "folders-width", &self.folders_width.to_string());
        }
        flag(
            &mut out,
            "alternate-row-color",
            self.alternate_rows,
            d.alternate_rows,
        );
        if all || self.size_unit != d.size_unit {
            line(&mut out, "size-format", self.size_unit.id());
        }
        if all || self.size_standard != d.size_standard {
            line(&mut out, "size-units", self.size_standard.id());
        }
        if all || self.language != d.language {
            line(&mut out, "language", self.language.id());
        }
        let c = &self.columns;
        flag(&mut out, "show-type-column", c.show_type, false);
        flag(&mut out, "show-extension-column", c.show_ext, false);
        flag(&mut out, "show-created-column", c.show_created, false);
        flag(&mut out, "show-accessed-column", c.show_accessed, false);
        flag(&mut out, "show-attributes-column", c.show_attrs, false);
        flag(&mut out, "show-md5-column", c.show_md5, false);
        flag(&mut out, "show-sha256-column", c.show_sha256, false);
        flag(&mut out, "show-crc32-column", c.show_crc32, false);
        flag(&mut out, "show-same-name-column", c.show_name_freq, false);
        flag(
            &mut out,
            "show-same-extension-column",
            c.show_ext_freq,
            false,
        );
        flag(&mut out, "show-same-size-column", c.show_size_freq, false);
        flag(&mut out, "show-full-path-column", c.show_full_path, false);
        flag(
            &mut out,
            "show-parent-path-column",
            c.show_parent_path,
            false,
        );
        flag(&mut out, "show-stem-column", c.show_stem, false);
        flag(
            &mut out,
            "show-name-length-column",
            c.show_name_length,
            false,
        );
        flag(
            &mut out,
            "show-path-length-column",
            c.show_path_length,
            false,
        );
        flag(&mut out, "show-disk-size-column", c.show_disk_size, false);
        flag(&mut out, "show-total-size-column", c.show_total_size, false);
        flag(&mut out, "show-items-column", c.show_child_count, false);
        flag(
            &mut out,
            "show-files-inside-column",
            c.show_child_files,
            false,
        );
        flag(
            &mut out,
            "show-subfolders-column",
            c.show_child_folders,
            false,
        );
        flag(&mut out, "show-deep-items-column", c.show_desc_count, false);
        flag(&mut out, "show-deep-files-column", c.show_desc_files, false);
        flag(
            &mut out,
            "show-deep-subfolders-column",
            c.show_desc_folders,
            false,
        );
        flag(
            &mut out,
            "show-folder-depth-column",
            c.show_folder_depth,
            false,
        );
        flag(&mut out, "show-hard-links-column", c.show_hard_links, false);
        flag(
            &mut out,
            "show-shortcut-target-column",
            c.show_shortcut_target,
            false,
        );
        flag(&mut out, "show-owner-column", c.show_owner, false);
        flag(&mut out, "show-width-column", c.show_width, false);
        flag(&mut out, "show-height-column", c.show_height, false);
        flag(&mut out, "show-dimensions-column", c.show_dimensions, false);
        flag(&mut out, "show-length-column", c.show_length, false);
        flag(&mut out, "show-bit-rate-column", c.show_bit_rate, false);
        flag(&mut out, "show-title-column", c.show_title, false);
        flag(&mut out, "show-artist-column", c.show_artist, false);
        flag(&mut out, "show-album-column", c.show_album, false);
        flag(&mut out, "show-year-column", c.show_year, false);
        flag(&mut out, "show-track-column", c.show_track, false);
        flag(&mut out, "show-genre-column", c.show_genre, false);
        flag(&mut out, "show-rating-column", c.show_rating, false);
        flag(&mut out, "show-tags-column", c.show_tags, false);
        flag(&mut out, "show-page-count-column", c.show_page_count, false);
        flag(&mut out, "show-word-count-column", c.show_word_count, false);
        flag(
            &mut out,
            "show-camera-maker-column",
            c.show_camera_maker,
            false,
        );
        flag(
            &mut out,
            "show-camera-model-column",
            c.show_camera_model,
            false,
        );
        flag(&mut out, "show-date-taken-column", c.show_date_taken, false);
        flag(&mut out, "show-gps-lat-column", c.show_gps_lat, false);
        flag(&mut out, "show-gps-long-column", c.show_gps_long, false);
        flag(&mut out, "show-sha1-column", c.show_sha1, false);
        flag(&mut out, "show-sha384-column", c.show_sha384, false);
        flag(&mut out, "show-sha512-column", c.show_sha512, false);
        flag(&mut out, "show-xxhash-column", c.show_xxhash, false);
        flag(&mut out, "show-dupe-group-column", c.show_dupe_group, false);
        let widths = [
            ("width-width", c.width),
            ("height-width", c.height),
            ("dimensions-width", c.dimensions),
            ("length-width", c.length),
            ("bit-rate-width", c.bit_rate),
            ("title-width", c.title),
            ("artist-width", c.artist),
            ("album-width", c.album),
            ("year-width", c.year),
            ("track-width", c.track),
            ("genre-width", c.genre),
            ("rating-width", c.rating),
            ("tags-width", c.tags),
            ("page-count-width", c.page_count),
            ("word-count-width", c.word_count),
            ("camera-maker-width", c.camera_maker),
            ("camera-model-width", c.camera_model),
            ("date-taken-width", c.date_taken),
            ("gps-lat-width", c.gps_lat),
            ("gps-long-width", c.gps_long),
            ("sha1-width", c.sha1),
            ("sha384-width", c.sha384),
            ("sha512-width", c.sha512),
            ("xxhash-width", c.xxhash),
            ("dupe-group-width", c.dupe_group),
            ("full-path-width", c.full_path),
            ("parent-path-width", c.parent_path),
            ("stem-width", c.stem),
            ("name-length-width", c.name_length),
            ("path-length-width", c.path_length),
            ("disk-size-width", c.disk_size),
            ("total-size-width", c.total_size),
            ("items-width", c.child_count),
            ("files-inside-width", c.child_files),
            ("subfolders-width", c.child_folders),
            ("deep-items-width", c.desc_count),
            ("deep-files-width", c.desc_files),
            ("deep-subfolders-width", c.desc_folders),
            ("folder-depth-width", c.folder_depth),
            ("hard-links-width", c.hard_links),
            ("shortcut-target-width", c.shortcut_target),
            ("owner-width", c.owner),
            ("name-width", c.name),
            ("folder-width", c.folder),
            ("size-width", c.size),
            ("modified-width", c.date),
            ("type-width", c.type_),
            ("extension-width", c.ext),
            ("created-width", c.created),
            ("accessed-width", c.accessed),
            ("attributes-width", c.attrs),
            ("md5-width", c.md5),
            ("sha256-width", c.sha256),
            ("crc32-width", c.crc32),
            ("same-name-width", c.name_freq),
            ("same-extension-width", c.ext_freq),
            ("same-size-width", c.size_freq),
        ];
        for (key, width) in widths {
            match width {
                Some(w) => line(&mut out, key, &format!("{}", w.round())),
                None if all => line(&mut out, key, ""),
                None => {}
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

    /// Every setting as a row of the raw table, in the file's order: each
    /// key with its value (defaults too), every value of a list key, then
    /// the lines with keys this version does not know.
    pub fn raw_rows(&self) -> Vec<RawRow> {
        let split = |text: String| -> Vec<(String, String)> {
            text.lines()
                .filter(|l| !l.trim_ascii().starts_with('#'))
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim_ascii().to_owned(), v.trim_ascii().to_owned()))
                .collect()
        };
        let defaults = split(Settings::default().write(true));
        split(self.write(true))
            .into_iter()
            .map(|(key, value)| {
                let list = LIST_KEYS.contains(&key.as_str());
                RawRow {
                    default: (!list)
                        .then(|| defaults.iter().find(|(k, _)| *k == key))
                        .flatten()
                        .map(|(_, v)| v.clone()),
                    known: key == "shortcut" || is_known(&key),
                    key,
                    value,
                }
            })
            .collect()
    }

    /// The settings with `key` set to `value` as the file would write it,
    /// or back to its default with `None` (an empty value does the same
    /// for keys whose default is empty). `None` when the key cannot be
    /// edited this way (a list key) or the value does not fit it.
    pub fn with_raw(&self, key: &str, value: Option<&str>) -> Option<Settings> {
        if LIST_KEYS.contains(&key) {
            return None;
        }
        let value = value.map(str::trim_ascii).filter(|v| {
            !v.is_empty()
                || !Settings::default()
                    .raw_rows()
                    .iter()
                    .any(|r| r.key == key && r.default.as_deref() == Some(""))
        });
        let mut text: String = self
            .to_text()
            .lines()
            .filter(|l| l.split_once('=').is_none_or(|(k, _)| k.trim_ascii() != key))
            .map(|l| format!("{l}\n"))
            .collect();
        if let Some(value) = value {
            text.push_str(&format!("{key} = {value}\n"));
        }
        let new = Settings::parse(&text);
        new.problems.is_empty().then_some(new)
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
                self.index_folder = d.index_folder;
                self.index_file_details = d.index_file_details;
                self.rescan = d.rescan;
                self.rescan_hours = d.rescan_hours;
                self.rescan_time = d.rescan_time;
                self.drives = d.drives;
                self.file_lists = d.file_lists;
                self.hide_offline = d.hide_offline;
                self.reload_changed_lists = d.reload_changed_lists;
                self.shallow_roots = d.shallow_roots;
                self.event_buffer_kib = d.event_buffer_kib;
                self.rescan_after_lost_events = d.rescan_after_lost_events;
                self.index_journal = d.index_journal;
            }
            Page::Search => {
                self.options = d.options;
                self.as_you_type = d.as_you_type;
                self.show_mode_badges = d.show_mode_badges;
                self.filter_wrap = d.filter_wrap;
                self.select_search_on_click = d.select_search_on_click;
                self.clear_selection_on_search = d.clear_selection_on_search;
                self.hide_results_when_empty = d.hide_results_when_empty;
                self.scope_selector = d.scope_selector;
                self.search_delay_ms = d.search_delay_ms;
                self.history_size = d.history_size;
                self.history_on_disk = d.history_on_disk;
                self.history_enabled = d.history_enabled;
                self.history_days = d.history_days;
                self.history_entries = d.history_entries;
                self.history_sort = d.history_sort;
                self.always_suggestions = d.always_suggestions;
                self.content = d.content;
                self.properties = d.properties;
            }
            Page::Appearance => {
                self.theme = d.theme;
                self.dark_style = d.dark_style;
                self.light_style = d.light_style;
                self.accent = d.accent;
                self.density = d.density;
                self.alternate_rows = d.alternate_rows;
                self.size_unit = d.size_unit;
                self.size_standard = d.size_standard;
                self.status_details = d.status_details;
                self.menu_bar = d.menu_bar;
                self.status_bar = d.status_bar;
                self.toolbar = d.toolbar;
                self.toolbar_buttons = d.toolbar_buttons;
                self.toolbar_navigation_labels = d.toolbar_navigation_labels;
                self.dropdown_below = d.dropdown_below;
                self.zoom = d.zoom;
                self.language = d.language;
            }
            Page::Window => {
                self.preview = d.preview;
                self.preview_visible = d.preview_visible;
                self.preview_width = d.preview_width;
                self.folders_visible = d.folders_visible;
                self.folders_width = d.folders_width;
                self.folders_recursive = d.folders_recursive;
                self.folders_dim_empty = d.folders_dim_empty;
                self.open_searches_in_tabs = d.open_searches_in_tabs;
                self.switch_to_new_tab = d.switch_to_new_tab;
                self.restore_session = d.restore_session;
                self.tab_results_kept = d.tab_results_kept;
                self.on_close = d.on_close;
                self.folder_size = d.folder_size;
                self.remember_columns = d.remember_columns;
                self.saved_columns_as_defaults = d.saved_columns_as_defaults;
                self.columns = d.columns;
                self.rename_name_only = d.rename_name_only;
                self.fileops = d.fileops;
                self.path_click_opens_folder = d.path_click_opens_folder;
                self.copy_full_path_quotes = d.copy_full_path_quotes;
                self.drive_root_labels = d.drive_root_labels;
                self.read_extended_auto = d.read_extended_auto;
                self.type_to_jump = d.type_to_jump;
                self.export_format = d.export_format;
                self.export_protect_formulas = d.export_protect_formulas;
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
    let text = s.to_text();
    // Without the mark another window asks about this save as if it came
    // from outside; the settings themselves are saved already.
    let mark = format!("{}{}\n", text_hash(text.as_bytes()), save_origin());
    let _ = write_atomic(&own_mark(path), mark.as_bytes());
    Ok(s)
}

/// `<name>.own` next to the settings file: a hash of the text an EveryGrep
/// window saved there last, followed by its process token. Another instance that finds the file
/// still matching it applies the change without asking; an edit made
/// outside EveryGrep no longer matches.
fn own_mark(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".own");
    path.with_file_name(name)
}

/// FNV-1a, 64 bits: the same in every build, unlike the std hasher.
fn text_hash(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}\n")
}

/// Unique to this running process, whose main window owns its settings.
fn save_origin() -> &'static str {
    static ORIGIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ORIGIN.get_or_init(|| {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{time}", std::process::id())
    })
}

/// Reads the settings file like [`Settings::load`], under its lock, and
/// tells whether an EveryGrep window or instance wrote this version, and
/// whether it was this process (see [`update`]). Older hash-only marks
/// identify another instance without claiming a local save.
pub fn load_marked(path: &Path) -> io::Result<(Settings, bool, bool)> {
    let _lock = lock(path)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((Settings::default(), false, false));
        }
        Err(e) => return Err(e),
    };
    let hash = text_hash(&bytes);
    let (own, local) = fs::read_to_string(own_mark(path))
        .map(|mark| {
            let mut lines = mark.lines();
            let own = lines.next() == Some(hash.trim_end());
            let local = own && lines.next() == Some(save_origin());
            (own, local)
        })
        .unwrap_or_default();
    Ok((
        Settings::parse(&String::from_utf8_lossy(&bytes)),
        own,
        local,
    ))
}

/// Saves files on a thread of its own, one after the other in the order
/// asked, so a slow or stalled disk (or another window holding the settings
/// lock) never holds up the window.
pub struct Writer {
    jobs: mpsc::Sender<Box<dyn FnOnce() + Send>>,
    /// Jobs asked for and not done yet.
    left: Arc<(Mutex<usize>, Condvar)>,
    /// Jobs asked for so far.
    asked: Arc<std::sync::atomic::AtomicU64>,
}

impl Writer {
    pub fn start() -> Self {
        let (jobs, asked) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let left = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let counted = Arc::clone(&left);
        let _ = std::thread::Builder::new()
            .name("save-settings".into())
            .spawn(move || {
                for job in asked {
                    job();
                    let (count, done) = &*counted;
                    let mut count = count.lock().unwrap_or_else(|e| e.into_inner());
                    *count = count.saturating_sub(1);
                    done.notify_all();
                }
            });
        Self {
            jobs,
            left,
            asked: Arc::default(),
        }
    }

    /// The number of jobs asked for so far. A job that reads `asked` (see
    /// [`asked_counter`](Self::asked_counter)) while it runs and finds the
    /// same number later knows no job was asked for after it.
    pub fn asked(&self) -> u64 {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Reads outside settings on the saving thread, with the revision
    /// that must still be current when the result is applied.
    pub fn read_at_revision<T: Send + 'static>(
        &self,
        read: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(T, u64) + Send + 'static,
    ) {
        // Capture before queuing: a local save asked while the read waits
        // behind another job must also make its answer obsolete.
        let at = self
            .asked_counter()
            .load(std::sync::atomic::Ordering::SeqCst);
        self.run_apart(move || done(read(), at));
    }

    pub fn asked_counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.asked)
    }

    /// Runs `job` after the jobs asked before it, without waiting for it.
    pub fn run(&self, job: impl FnOnce() + Send + 'static) {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.run_apart(job);
    }

    /// Like [`run`](Self::run), for a job that does not write the settings
    /// file (the search history, the tabs' session): in the same order, but
    /// not counted in [`asked`](Self::asked), so it never holds back applying
    /// a settings file changed outside the window.
    pub fn run_apart(&self, job: impl FnOnce() + Send + 'static) {
        let (count, _) = &*self.left;
        *count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        if self.jobs.send(Box::new(job)).is_err() {
            *count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        }
    }

    /// Waits at most `wait` for every job asked so far to be done; returns
    /// whether they are.
    pub fn wait_done(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let (count, done) = &*self.left;
        let mut count = count.lock().unwrap_or_else(|e| e.into_inner());
        while *count > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            count = done
                .wait_timeout(count, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

unsafe extern "C" {
    fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
}

/// `LOCK_EX` from `<sys/file.h>`.
const LOCK_EX: std::ffi::c_int = 2;

/// Takes the exclusive lock on `<path>.lock` (creating it and its folder),
/// waiting for another holder. The lock goes with the returned file.
pub(crate) fn lock(path: &Path) -> io::Result<fs::File> {
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

/// How many lines of a settings file's `text` set a setting this version
/// knows (whether or not their values can be read), to tell a settings
/// file from any other text.
pub fn known_lines(text: &str) -> usize {
    text.lines()
        .filter_map(|l| l.trim_ascii().split_once('='))
        .filter(|(k, _)| {
            let k = k.trim_ascii();
            !k.starts_with('#') && (k == "shortcut" || is_known(k))
        })
        .count()
}

fn is_known(key: &str) -> bool {
    let mut probe = Settings::default();
    // Every known key accepts at least one of these values.
    [
        "true",
        "64",
        "all",
        "quit",
        "total",
        "system",
        "compact",
        "slate",
        "classic",
        "1",
        "100",
        "/",
        "keep",
        "A-1",
        "auto",
        "jedec",
        "csv",
        "show",
        "symlink",
        "number",
        "windows-1254",
        "never",
        "03:00",
        "date",
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
    config_dir().map(|d| d.join("search-history"))
}

/// What tells one version of a file from another: its inode, size and
/// modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    ino: u64,
    len: u64,
    mtime_ns: i128,
}

/// The [`Stamp`] of the file at `path`; `None` when there is none.
pub fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path).ok()?;
    Some(Stamp {
        ino: m.ino(),
        len: m.len(),
        mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
    })
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
    fn export_protection_defaults_on_round_trips_and_resets() {
        assert!(Settings::default().export_protect_formulas);
        for protect in [false, true] {
            let mut settings = Settings {
                export_protect_formulas: protect,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&settings.to_text()), settings);
            settings.reset(Page::Window);
            assert!(settings.export_protect_formulas);
        }
        let invalid = Settings::parse("export-protect-formulas = invalid\n");
        assert!(invalid.export_protect_formulas);
        assert!(!invalid.problems.is_empty());
    }

    #[test]
    fn export_format_round_trips_and_rejects_unknown_types() {
        for format in crate::export::Format::ALL {
            let settings = Settings {
                export_format: format,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&settings.to_text()), settings);
        }
        let invalid = Settings::parse("export-format = unknown\n");
        assert_eq!(invalid.export_format, crate::export::Format::Csv);
        assert!(!invalid.problems.is_empty());
    }

    #[test]
    fn saves_run_in_order_without_holding_up_the_caller() {
        let writer = Writer::start();
        let (release, stalled) = mpsc::channel::<()>();
        let order = Arc::new(Mutex::new(Vec::new()));
        // A save that hangs, as on a stalled disk, and two after it.
        let asked = Instant::now();
        let o = Arc::clone(&order);
        writer.run(move || {
            let _ = stalled.recv();
            o.lock().unwrap().push(1);
        });
        for n in [2, 3] {
            let o = Arc::clone(&order);
            writer.run(move || o.lock().unwrap().push(n));
        }
        assert!(asked.elapsed() < Duration::from_millis(100));
        assert!(
            !writer.wait_done(Duration::from_millis(200)),
            "waited too long"
        );
        assert!(order.lock().unwrap().is_empty());
        release.send(()).unwrap();
        assert!(writer.wait_done(Duration::from_secs(5)));
        assert_eq!(*order.lock().unwrap(), [1, 2, 3]);
        // Nothing left: no wait.
        assert!(writer.wait_done(Duration::ZERO));
        assert_eq!(writer.asked(), 3);
        // A job sees how many were asked for up to then.
        let counter = writer.asked_counter();
        let (seen_tx, seen) = mpsc::channel();
        writer.run(move || {
            let _ = seen_tx.send(counter.load(std::sync::atomic::Ordering::SeqCst));
        });
        assert_eq!(seen.recv_timeout(Duration::from_secs(5)).unwrap(), 4);
        // Jobs for other files keep the order but are not counted.
        let o = Arc::clone(&order);
        writer.run_apart(move || o.lock().unwrap().push(5));
        let o = Arc::clone(&order);
        writer.run(move || o.lock().unwrap().push(6));
        assert!(writer.wait_done(Duration::from_secs(5)));
        assert_eq!(*order.lock().unwrap(), [1, 2, 3, 5, 6]);
        assert_eq!(writer.asked(), 5);
    }

    #[test]
    fn an_outside_read_drops_local_changes_asked_while_queued_or_reading() {
        for while_reading in [false, true] {
            let writer = Writer::start();
            let (release_queue, blocked_queue) = mpsc::channel();
            writer.run_apart(move || {
                blocked_queue.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            let (reading_tx, reading) = mpsc::channel();
            let (release_read, blocked_read) = mpsc::channel();
            let (done_tx, done) = mpsc::channel();
            writer.read_at_revision(
                move || {
                    reading_tx.send(()).unwrap();
                    blocked_read.recv_timeout(Duration::from_secs(5)).unwrap();
                    "old theme"
                },
                move |value, at| done_tx.send((value, at)).unwrap(),
            );
            if while_reading {
                release_queue.send(()).unwrap();
                reading.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            writer.run(|| {});
            if !while_reading {
                release_queue.send(()).unwrap();
                reading.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            release_read.send(()).unwrap();
            let (read, at) = done.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(read, "old theme");
            assert_ne!(
                at,
                writer.asked(),
                "a stale outside read would overwrite a local change"
            );
            assert!(writer.wait_done(Duration::from_secs(5)));
        }
    }

    #[test]
    fn a_stamp_tells_versions_of_a_file_apart() {
        let dir = Scratch::new("stamp");
        let path = dir.0.join("settings");
        assert_eq!(stamp(&path), None);
        write_atomic(&path, b"theme = dark\n").unwrap();
        let first = stamp(&path).unwrap();
        assert_eq!(stamp(&path), Some(first));
        // Replaced atomically: a new inode even with the same size and time.
        write_atomic(&path, b"theme = lite\n").unwrap();
        assert_ne!(stamp(&path), Some(first));
    }

    #[test]
    fn tab_preferences_defaults_round_trip_and_reset_with_window() {
        let mut s = Settings::default();
        assert!(s.open_searches_in_tabs && s.switch_to_new_tab && !s.restore_session);
        assert_eq!(s.tab_results_kept, 10);
        assert_eq!(
            Settings::parse("tab-results-kept = 0\n").tab_results_kept,
            0
        );
        let bad = Settings::parse("tab-results-kept = 1001\n");
        assert_eq!((bad.tab_results_kept, bad.problems.len()), (10, 1));
        let mut kept = Settings::parse("tab-results-kept = 3\n");
        assert!(kept.to_text().contains("tab-results-kept = 3\n"));
        kept.reset(Page::Window);
        assert_eq!(kept.tab_results_kept, 10);
        assert!(!s.to_text().contains("restore-session"));
        s = Settings::parse(
            "open-searches-in-tabs = true\nswitch-to-new-tab = true\nrestore-session = true\n",
        );
        assert!(s.open_searches_in_tabs && s.switch_to_new_tab && s.restore_session);
        assert_eq!(Settings::parse(&s.to_text()), s);
        s = Settings::parse(
            "open-searches-in-tabs = false\nswitch-to-new-tab = false\nrestore-session = true",
        );
        assert!(!s.open_searches_in_tabs && !s.switch_to_new_tab);
        assert_eq!(Settings::parse(&s.to_text()), s);
        s.reset(Page::Search);
        assert!(s.restore_session);
        s.reset(Page::Window);
        assert_eq!(s, Settings::default());
        let s = Settings::parse(
            "restore-session = invalid\nopen-searches-in-tabs = invalid\nswitch-to-new-tab = 2\n",
        );
        assert_eq!(s.problems.len(), 3);
        assert!(!s.restore_session && s.open_searches_in_tabs && s.switch_to_new_tab);
    }

    #[test]
    fn saving_never_writes_through_a_planted_link() {
        let dir = Scratch::new("settings-link");
        let victim = dir.0.join("victim.txt");
        fs::write(&victim, "keep").unwrap();
        let path = dir.0.join("settings");
        // A link at a predictable temporary name (the file's name and the
        // process id).
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
            index_folder: Some("/mnt/fast disk/eg".into()),
            index_file_details: false,
            rescan: 2,
            rescan_hours: 6,
            rescan_time: 22 * 60 + 5,
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
                filter: Filter::Image,
            },
            as_you_type: false,
            show_mode_badges: false,
            filter_wrap: false,
            select_search_on_click: false,
            clear_selection_on_search: false,
            hide_results_when_empty: true,
            scope_selector: false,
            search_delay_ms: 250,
            history_size: 0,
            history_on_disk: true,
            reload_changed: false,
            ask_reload: false,
            on_close: OnClose::Hide,
            folder_size: FolderSize::Items,
            remember_columns: false,
            rename_name_only: false,
            path_click_opens_folder: false,
            copy_full_path_quotes: true,
            drive_root_labels: false,
            read_extended_auto: true,
            shortcuts_by_position: false,
            type_to_jump: false,
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
                show_md5: true,
                show_sha256: false,
                show_crc32: true,
                md5: Some(280.0),
                sha256: None,
                crc32: Some(70.0),
                show_name_freq: true,
                show_ext_freq: false,
                show_size_freq: true,
                name_freq: None,
                ext_freq: Some(100.0),
                size_freq: Some(90.0),
                show_full_path: true,
                full_path: Some(260.0),
                show_owner: true,
                owner: Some(110.0),
                ..Columns::default()
            },
            theme: ThemeChoice::Dark,
            density: Density::Compact,
            alternate_rows: false,
            size_unit: SizeUnit::Mega,
            size_standard: SizeStandard::Iec,
            status_details: false,
            menu_bar: false,
            status_bar: false,
            toolbar: false,
            zoom: 125,
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
        assert!(text.contains("status-bar = false\n"));
        assert!(text.contains("toolbar = false\n"));
        assert!(text.contains("zoom = 125\n"));
        assert_eq!(Settings::parse("zoom = 20\n").problems.len(), 1);
        assert_eq!(Settings::parse("zoom = 301\n").problems.len(), 1);
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
        assert!(text.contains("hide-results-when-empty = true\n"));
        assert!(text.contains("show-search-scope = false\n"));
        assert!(text.contains("search-delay = 250\n"));
        assert_eq!(
            Settings::parse("search-delay = 5001\n").problems.len(),
            1,
            "too long a delay is not taken"
        );
        assert_eq!(
            Settings::parse("search-delay = 5000\n").search_delay_ms,
            5000
        );
        assert!(text.contains("rename-select-name-only = false\n"));
        assert!(text.contains("open-folder-on-path-double-click = false\n"));
        assert!(text.contains("copy-full-path-quotes = true\n"));
        assert!(text.contains("load-file-details = true\n"));
        assert!(text.contains("shortcuts-by-key-position = false\n"));
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
    fn history_recording_and_retention_settings_round_trip() {
        let mut s = Settings::parse("history-enabled = false\nhistory-days = 0\n");
        assert!(!s.history_enabled);
        assert_eq!(s.history_days, 0);
        assert_eq!(Settings::parse(&s.to_text()), s);
        assert_eq!(Settings::parse("history-days = 36501\n").problems.len(), 1);
        assert_eq!(
            Settings::parse("history-sort = invalid\n").problems.len(),
            1
        );
        for order in Order::ALL {
            s.history_sort = order;
            s.always_suggestions = true;
            assert_eq!(Settings::parse(&s.to_text()), s);
        }
        s.reset(Page::Search);
        assert_eq!(s.history_sort, Order::Date);
        assert!(s.always_suggestions);
        assert!(s.history_enabled);
        assert_eq!(s.history_days, 90);
    }

    #[test]
    fn saves_by_everygrep_are_told_from_edits_made_outside() {
        let dir = std::env::temp_dir().join(format!("everygrep-own-mark-{}", std::process::id()));
        let path = dir.join("settings");
        let _ = fs::remove_dir_all(&dir);
        assert!(!load_marked(&path).unwrap().1, "no file, nothing written");
        update(&path, |s| s.history_size = 7).unwrap();
        let (read, own, local) = load_marked(&path).unwrap();
        assert!(own && local, "this window saved it");
        assert_eq!(read.history_size, 7);
        let text = fs::read_to_string(&path).unwrap();
        for mark in [
            text_hash(text.as_bytes()),
            format!("{}other-process\n", text_hash(text.as_bytes())),
        ] {
            write_atomic(&own_mark(&path), mark.as_bytes()).unwrap();
            let (_, own, local) = load_marked(&path).unwrap();
            assert!(own && !local, "a different instance's save");
        }
        write_atomic(&path, format!("{text}history-days = 3\n").as_bytes()).unwrap();
        let (read, own, local) = load_marked(&path).unwrap();
        assert!(!own && !local, "an edit made outside");
        assert_eq!(read.history_days, 3);
        update(&path, |s| s.history_size = 8).unwrap();
        assert!(load_marked(&path).unwrap().1, "saved again by EveryGrep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn declined_values_preserve_defaults_and_complete_lists() {
        let current = Settings::parse("root = /kept\nroot = /also-kept\n");
        let outside = Settings::parse("history-size = 7\nroot = /declined\n");
        let declined = outside.differing_lines(&current);
        let mut saved = outside;
        saved.menu_bar = false;
        let applied = saved.keeping_declined(&current, &declined);
        assert_eq!(applied.history_size, current.history_size);
        assert_eq!(applied.roots, current.roots);
        assert!(!applied.menu_bar);
        // A locally emptied list must stay empty as well.
        let empty = Settings::default();
        let declined = saved.differing_lines(&empty);
        assert!(saved.keeping_declined(&empty, &declined).roots.is_empty());
    }

    #[test]
    fn outside_changes_are_asked_about_by_default_and_differ_by_line() {
        let d = Settings::default();
        assert!(d.reload_changed && d.ask_reload, "ask before applying");
        let off = Settings::parse("ask-before-reloading-settings = false\n");
        assert!(off.problems.is_empty() && !off.ask_reload);
        assert!(
            off.to_text()
                .contains("ask-before-reloading-settings = false\n")
        );
        assert!(d.differing_lines(&d).is_empty());
        let other = Settings::parse("history-size = 7\n");
        let lines = d.differing_lines(&other);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.iter().any(|l| l == "history-size = 7"), "{lines:?}");
        assert_eq!(lines, other.differing_lines(&d));
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
    fn accents_round_trip_reject_unknown_and_reset_with_appearance() {
        assert_eq!(Settings::default().accent, AccentChoice::Blue);
        for accent in AccentChoice::ALL {
            let mut s = Settings {
                accent,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&s.to_text()).accent, accent);
            s.reset(Page::Search);
            assert_eq!(s.accent, accent);
            s.reset(Page::Appearance);
            assert_eq!(s.accent, AccentChoice::Blue);
        }
        let s = Settings::parse("accent-color = green\naccent-color = unknown\n");
        assert_eq!(s.accent, AccentChoice::Green);
        assert_eq!(s.problems.len(), 1);
    }

    #[test]
    fn dark_styles_round_trip_reject_unknown_and_reset_only_appearance() {
        assert_eq!(Settings::default().dark_style, DarkStyle::Slate);
        for style in DarkStyle::ALL {
            let mut s = Settings {
                dark_style: style,
                theme: ThemeChoice::Dark,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&s.to_text()).dark_style, style);
            s.reset(Page::Search);
            assert_eq!(s.dark_style, style);
            s.reset(Page::Appearance);
            assert_eq!(s.dark_style, DarkStyle::Slate);
        }
        let s = Settings::parse("dark-style = graphite\ndark-style = unknown\n");
        assert_eq!(s.dark_style, DarkStyle::Graphite);
        assert_eq!(s.problems.len(), 1);
    }

    #[test]
    fn light_styles_round_trip_reject_unknown_and_reset_only_appearance() {
        assert_eq!(Settings::default().light_style, LightStyle::Classic);
        assert!(!Settings::default().to_text().contains("light-style"));
        assert_eq!(
            Settings::parse("light-style = unknown\n").light_style,
            LightStyle::Classic
        );
        for style in LightStyle::ALL {
            let mut s = Settings {
                light_style: style,
                theme: ThemeChoice::Light,
                ..Settings::default()
            };
            assert_eq!(Settings::parse(&s.to_text()).light_style, style);
            assert_eq!(
                Settings::default()
                    .with_raw("light-style", Some(style.id()))
                    .unwrap()
                    .light_style,
                style
            );
            assert!(s.with_raw("light-style", Some("unknown")).is_none());
            s.reset(Page::Search);
            assert_eq!(s.light_style, style);
            s.reset(Page::Appearance);
            assert_eq!(s.light_style, LightStyle::Classic);
        }
        let s = Settings::parse("light-style = neutral\nlight-style = unknown\n");
        assert_eq!(s.light_style, LightStyle::Neutral);
        assert_eq!(s.problems.len(), 1);
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
    fn dropdown_placement_defaults_below_persists_and_resets_with_appearance() {
        assert!(Settings::default().dropdown_below);
        let mut s = Settings::parse("dropdown-below = false\n");
        assert!(!s.dropdown_below);
        s = Settings::parse(&s.to_text());
        assert!(!s.dropdown_below);
        s.reset(Page::Search);
        assert!(!s.dropdown_below);
        s.reset(Page::Appearance);
        assert!(s.dropdown_below);
        let s = Settings::parse("dropdown-below = false\ndropdown-below = invalid\n");
        assert!(!s.dropdown_below);
        assert_eq!(s.problems.len(), 1);
    }

    #[test]
    fn toolbar_defaults_on_and_turning_it_off_persists() {
        assert!(Settings::default().toolbar);
        assert!(Settings::parse("").toolbar);
        let s = Settings {
            toolbar: false,
            ..Settings::default()
        };
        let text = s.to_text();
        assert!(text.contains("toolbar = false\n"));
        assert!(!Settings::parse(&text).toolbar);
        assert!(Settings::parse("toolbar = true\n").toolbar);
    }

    #[test]
    fn navigation_labels_default_off_persist_and_reset_with_appearance() {
        assert!(!Settings::default().toolbar_navigation_labels);
        let mut s = Settings::parse("toolbar-navigation-labels = true\n");
        assert!(s.toolbar_navigation_labels);
        s = Settings::parse(&s.to_text());
        assert!(s.toolbar_navigation_labels);
        s.reset(Page::Search);
        assert!(s.toolbar_navigation_labels);
        s.reset(Page::Appearance);
        assert!(!s.toolbar_navigation_labels);
        let s = Settings::parse(
            "toolbar-navigation-labels = true\ntoolbar-navigation-labels = invalid\n",
        );
        assert!(s.toolbar_navigation_labels);
        assert_eq!(s.problems.len(), 1);
    }

    #[test]
    fn toolbar_buttons_are_independent_persisted_and_reset_with_appearance() {
        assert_eq!(Settings::default().toolbar_buttons, [true; 7]);
        for hidden in 0..7 {
            let mut s = Settings::default();
            s.toolbar_buttons[hidden] = false;
            let mut loaded = Settings::parse(&s.to_text());
            assert_eq!(loaded.toolbar_buttons, s.toolbar_buttons);
            loaded.reset(Page::Search);
            assert_eq!(loaded.toolbar_buttons, s.toolbar_buttons);
            loaded.reset(Page::Appearance);
            assert_eq!(loaded.toolbar_buttons, [true; 7]);
        }
        let s = Settings::parse("toolbar-button-back = false\ntoolbar-button-back = invalid\n");
        assert!(!s.toolbar_buttons[0]);
        assert_eq!(s.problems.len(), 1);
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
    fn rescan_schedule_reads_its_three_keys_and_refuses_bad_values() {
        assert_eq!(Settings::default().schedule(), Schedule::Never);
        let s = Settings::parse("rescan = hours\nrescan-hours = 12\nrescan-time = 4:30\n");
        assert_eq!(s.schedule(), Schedule::EveryHours(12));
        assert_eq!(s.rescan_time, 270);
        let s = Settings::parse(&s.to_text().replace("rescan = hours", "rescan = daily"));
        assert_eq!(s.schedule(), Schedule::Daily(270));
        assert!(s.to_text().contains("rescan-time = 04:30\n"));
        let bad = Settings::parse(
            "rescan = weekly\nrescan-hours = 0\nrescan-hours = 169\nrescan-time = 25:00\n",
        );
        assert_eq!(bad.problems.len(), 4);
        assert_eq!(bad.schedule(), Schedule::Never);
        assert_eq!(bad.rescan_hours, DEFAULT_RESCAN_HOURS);
        assert_eq!(bad.rescan_time, DEFAULT_RESCAN_TIME);
    }

    #[test]
    fn known_lines_tell_a_settings_file_from_other_text() {
        assert_eq!(known_lines("# EveryGrep settings\n"), 0);
        assert_eq!(
            known_lines("theme = dark\nmatch-case = maybe\nfuture = 1\n"),
            2
        );
        assert_eq!(known_lines("shortcut = file.export-results\n"), 1);
        assert_eq!(known_lines("[Desktop Entry]\nName = x\nExec=y\n"), 0);
        assert_eq!(known_lines("# theme = dark\n"), 0);
    }

    #[test]
    fn the_raw_table_lists_every_setting_and_edits_one_at_a_time() {
        let d = Settings::default();
        let rows = d.raw_rows();
        assert!(rows.iter().all(|r| r.known && !r.modified()), "{rows:#?}");
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        for key in [
            "theme",
            "history-size",
            "name-width",
            "content-pdf",
            "rescan-time",
        ] {
            assert!(keys.contains(&key), "{key} missing");
        }
        // Every key accepts its own default, and the result is the default.
        for r in &rows {
            let back = d.with_raw(&r.key, Some(r.default.as_deref().unwrap()));
            assert_eq!(back.as_ref(), Some(&d), "{}", r.key);
        }
        // Every key the file can hold is in the table.
        for r in &rows {
            assert!(is_known(&r.key), "{}", r.key);
        }

        let s = d.with_raw("theme", Some("dark")).unwrap();
        assert_eq!(s.theme, ThemeChoice::Dark);
        let row = s.raw_rows().into_iter().find(|r| r.key == "theme").unwrap();
        assert!(row.modified() && row.editable());
        assert_eq!(
            (row.value.as_str(), row.default.as_deref()),
            ("dark", Some("system"))
        );
        assert_eq!(s.with_raw("theme", None).unwrap(), d);
        assert_eq!(d.with_raw("theme", Some("purple")), None);
        assert_eq!(d.with_raw("history-size", Some("-1")), None);
        // An empty width is its default.
        let wide = d.with_raw("name-width", Some("300")).unwrap();
        assert_eq!(wide.columns.name, Some(300.0));
        assert_eq!(wide.with_raw("name-width", Some(" ")).unwrap(), d);
        // List keys and unknown lines are listed, list keys not edited.
        let s = Settings::parse("root = /a\nroot = /b\nfuture-key = 1\n");
        let rows = s.raw_rows();
        let roots: Vec<&RawRow> = rows.iter().filter(|r| r.key == "root").collect();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|r| !r.editable() && r.modified()));
        let future = rows.iter().find(|r| r.key == "future-key").unwrap();
        assert!(!future.known && !future.editable());
        assert_eq!(s.with_raw("root", Some("/c")), None);
        // Other lines stay as they were.
        assert_eq!(s.with_raw("theme", Some("light")).unwrap().roots.len(), 2);
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
        // No temporary file is left behind; the lock file and the mark of
        // the last save by EveryGrep stay.
        let mut names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["settings", "settings.lock", "settings.own"]);
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
    fn file_lists_keep_their_order_switch_and_odd_paths() {
        let odd = PathBuf::from(OsString::from_vec(b"/l/a = b\n\xff.eglist".to_vec()));
        let s = Settings {
            file_lists: vec![
                FileListSetting {
                    path: odd.clone(),
                    enabled: false,
                },
                FileListSetting {
                    path: "/l/disk.eglist".into(),
                    enabled: true,
                },
            ],
            hide_offline: true,
            reload_changed_lists: false,
            ..Settings::default()
        };
        let text = s.to_text();
        assert_eq!(Settings::parse(&text), s, "{text}");
        assert!(text.contains("file-list = /l/disk.eglist\n"), "{text}");
        assert!(text.contains("hide-offline-entries = true\n"));
        assert!(text.contains("reload-changed-file-lists = false\n"));
        assert!(Settings::default().reload_changed_lists);
        let s2 = Settings {
            shallow_roots: vec!["/srv/a b".into()],
            event_buffer_kib: 512,
            rescan_after_lost_events: false,
            index_journal: true,
            ..Settings::default()
        };
        let text2 = s2.to_text();
        assert_eq!(Settings::parse(&text2), s2, "{text2}");
        assert_eq!(s2.rule_spec().shallow, s2.shallow_roots);
        for bad in [
            "event-buffer-kib = 1",
            "event-buffer-kib = 99999",
            "root-without-subfolders = rel",
        ] {
            let s = Settings::parse(bad);
            assert_eq!(s.event_buffer_kib, DEFAULT_EVENT_BUFFER_KIB, "{bad}");
            assert!(s.shallow_roots.is_empty(), "{bad}");
        }
        let back = Settings::parse(&text);
        assert_eq!(back.file_lists[0].path, odd);
        // Relative paths and repeats are not taken.
        let t = Settings::parse("file-list = x.eglist\nfile-list = /a\nfile-list-off = /a\n");
        assert_eq!(t.file_lists.len(), 1);
        assert!(t.file_lists[0].enabled);
        assert!(!Settings::default().hide_offline);
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
