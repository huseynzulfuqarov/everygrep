//! Words the window shows for core concepts (filter names, file types and
//! matching modes) and the texts of the settings window that the program
//! fills in. They are kept together here so they can be translated.

use everygrep_core::filter::Filter;
use everygrep_core::kind::FileKind;
use everygrep_core::query::QueryOptions;
use everygrep_core::sort::FolderSize;

use crate::engine::IndexState;
use crate::format;
use crate::settings::{Density, OnClose, ThemeChoice};

/// Name of a filter in the filter selector.
pub fn filter(f: Filter) -> &'static str {
    match f {
        Filter::All => "All",
        Filter::Audio => "Audio",
        Filter::Archive => "Archives",
        Filter::Code => "Code",
        Filter::Document => "Documents",
        Filter::Executable => "Executables",
        Filter::Folder => "Folders",
        Filter::Picture => "Pictures",
        Filter::Video => "Videos",
    }
}

/// The noun for a kind: after an extension (`PNG image`) and on its own.
fn kind_nouns(kind: FileKind) -> (&'static str, &'static str) {
    match kind {
        FileKind::Folder => ("folder", "Folder"),
        FileKind::File => ("file", "File"),
        FileKind::Text => ("text", "Text"),
        FileKind::Document | FileKind::Pdf => ("document", "Document"),
        FileKind::Spreadsheet => ("spreadsheet", "Spreadsheet"),
        FileKind::Presentation => ("presentation", "Presentation"),
        FileKind::Image => ("image", "Image"),
        FileKind::Audio => ("audio", "Audio"),
        FileKind::Video => ("video", "Video"),
        FileKind::Archive => ("archive", "Archive"),
        FileKind::Code => ("source", "Source"),
        FileKind::Executable => ("program", "Program"),
        FileKind::Font => ("font", "Font"),
        FileKind::DiskImage => ("disk image", "Disk image"),
        FileKind::Link => ("link", "Link"),
    }
}

/// Text of the Type column: `Folder`, `PNG image`, `TAR.GZ archive` would be
/// `GZ archive` (the last extension), `Source` for a `Makefile`.
pub fn type_name(kind: FileKind, extension: &str) -> String {
    let (after, alone) = kind_nouns(kind);
    if kind == FileKind::Folder || extension.is_empty() {
        alone.to_owned()
    } else {
        format!("{} {after}", extension.to_uppercase())
    }
}

/// The extension of a name as written, without the dot; empty for folders and
/// for names without one. A dot at the very start (`.bashrc`) does not begin
/// an extension.
pub fn extension(name: &str, is_dir: bool) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 && !is_dir => &name[dot + 1..],
        _ => "",
    }
}

/// Short words for the matching modes that are on, for the status line.
pub fn mode_badges(o: &QueryOptions) -> Vec<&'static str> {
    [
        (o.case_sensitive, "Case"),
        (o.whole_words, "Whole words"),
        (o.match_path, "Path"),
        (o.match_diacritics, "Diacritics"),
    ]
    .into_iter()
    .filter_map(|(on, word)| on.then_some(word))
    .collect()
}

/// `1 result`, `12 results`.
pub fn results(n: usize) -> String {
    format!(
        "{} {}",
        format::count(n),
        if n == 1 { "result" } else { "results" }
    )
}

/// Separator between the parts of the status line.
pub const STATUS_GAP: &str = "  ·  ";

/// Details of one selected item for the status line: its size (empty for a
/// folder whose size is not shown), modification time and full path.
pub fn item_details(size: &str, modified: &str, path: &str) -> String {
    let modified = (!modified.is_empty()).then(|| format!("Modified {modified}"));
    [Some(size.to_owned()), modified, Some(path.to_owned())]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(STATUS_GAP)
}

/// Several selected items: their number and, when known, the total size of
/// the files among them (`None` when it was not counted).
pub fn selection_summary(n: usize, files: Option<(usize, u64)>) -> String {
    let mut out = format!("{} items selected", format::count(n));
    match files {
        Some((0, _)) | None => {}
        Some((f, bytes)) if f == n => {
            out.push_str(STATUS_GAP);
            out.push_str(&format::size(bytes));
        }
        Some((f, bytes)) => {
            out.push_str(STATUS_GAP);
            out.push_str(&format!(
                "{} in {} {}",
                format::size(bytes),
                format::count(f),
                if f == 1 { "file" } else { "files" }
            ));
        }
    }
    out
}

/// `1 folder`, `3 folders`.
pub fn folder_count(n: usize) -> String {
    if n == 1 {
        "1 folder".to_owned()
    } else {
        format!("{} folders", format::count(n))
    }
}

/// `1 item`, `12 items`: a folder's size when it shows the number of items.
pub fn items(n: u32) -> String {
    if n == 1 {
        "1 item".to_owned()
    } else {
        format!("{} items", format::count(n as usize))
    }
}

/// Choices for what a folder's Size shows.
pub fn folder_size(f: FolderSize) -> &'static str {
    match f {
        FolderSize::Total => "Total size",
        FolderSize::Items => "Number of items",
        FolderSize::None => "Nothing",
    }
}

/// What the chosen folder size means.
pub fn folder_size_note(f: FolderSize) -> &'static str {
    match f {
        FolderSize::Total => {
            "The size of all files inside the folder and its subfolders. Folders sort by it together with files."
        }
        FolderSize::Items => {
            "How many files and folders are directly inside. Sorting by Size puts folders apart from files, by this number."
        }
        FolderSize::None => "Folders sort by Size as if they were empty.",
    }
}

/// Choices for what closing the window does.
pub fn on_close(o: OnClose) -> &'static str {
    match o {
        OnClose::Quit => "Quit EveryGrep",
        OnClose::Hide => "Hide the window and keep running",
    }
}

/// What the chosen close behaviour means.
pub fn on_close_note(o: OnClose) -> &'static str {
    match o {
        OnClose::Quit => {
            "The index is saved and EveryGrep ends. The next start loads it again, which takes a moment for large indexes."
        }
        OnClose::Hide => {
            "EveryGrep keeps the index in memory and keeps watching for changes. Starting EveryGrep again (for example with its shortcut) shows the window at once. Ctrl+Q or Quit in the ⋯ menu ends it."
        }
    }
}

/// Choices for the window's colors.
pub fn theme(t: ThemeChoice) -> &'static str {
    match t {
        ThemeChoice::System => "Same as the desktop",
        ThemeChoice::Light => "Light",
        ThemeChoice::Dark => "Dark",
    }
}

/// Choices for how tightly the result list is packed.
pub fn density(d: Density) -> &'static str {
    match d {
        Density::Comfortable => "Comfortable",
        Density::Compact => "Compact",
    }
}

/// The state of the index for the settings window.
pub fn index_status(state: IndexState, indexed: usize) -> String {
    let entries = format::count(indexed);
    match state {
        IndexState::Loading => "Loading the index…".to_owned(),
        IndexState::Scanning => "Indexing…".to_owned(),
        IndexState::CatchingUp => format!("{entries} entries · catching up"),
        IndexState::Watching => format!("{entries} entries · watching for changes"),
        IndexState::Static => format!("{entries} entries · not watching changes"),
    }
}

/// Shown on the Indexing page when the window was started with its own
/// folders or index file.
pub const INDEX_FROM_COMMAND_LINE: &str = "This window was started with --root or --index-file. Changes here apply to this window only and are not saved.";

/// Titles and button of the folder chooser.
pub const CHOOSE_ROOTS: &str = "Add folders to index";
pub const CHOOSE_EXCLUDES: &str = "Add folders to exclude";
pub const CHOOSE_ACCEPT: &str = "Add";

pub fn chooser_failed(e: &str) -> String {
    format!("Cannot open the folder chooser: {e}")
}

pub fn cannot_save(e: &std::io::Error) -> String {
    format!("Cannot save the settings: {e}")
}

pub fn cannot_read(path: &std::path::Path, e: &std::io::Error) -> String {
    format!(
        "Cannot read {}: {e}. Changes are not saved until it can be read.",
        path.display()
    )
}

pub fn bad_lines(n: usize) -> String {
    if n == 1 {
        "One line of the settings file was not understood and is ignored.".to_owned()
    } else {
        format!(
            "{} lines of the settings file were not understood and are ignored.",
            format::count(n)
        )
    }
}

pub const NO_SETTINGS_FOLDER: &str =
    "Neither XDG_CONFIG_HOME nor HOME is set, so settings cannot be saved.";

/// How to add a GNOME shortcut that opens the window.
pub const SHORTCUT_STEPS: &str = "In GNOME: open Settings → Keyboard → View and Customize Shortcuts → Custom Shortcuts, press +, enter the name EveryGrep and the command below, then press the keys you want (for example Super+F).";

pub const COMMAND_COPIED: &str = "Copied the command.";

/// The keyboard shortcuts dialog: keys and what they do.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("↑ ↓ PgUp PgDn Home End", "Move in the results and select"),
    ("Shift + movement", "Extend the selection"),
    ("Ctrl + movement", "Move without changing the selection"),
    ("Ctrl+Space", "Add or remove one row"),
    ("Ctrl+A", "Select all"),
    ("Enter", "Open"),
    ("Ctrl+Enter", "Open the containing folder"),
    ("Ctrl+C", "Copy the name"),
    ("Ctrl+Shift+C", "Copy the full path"),
    ("F2", "Rename"),
    ("Delete", "Move to the trash"),
    ("Shift+Delete", "Delete permanently"),
    ("Menu, Shift+F10", "Context menu"),
    ("Esc", "Clear the selection, then the search"),
    ("Ctrl+F, Ctrl+L", "Focus the search field"),
    ("Alt+← / Alt+→", "Previous / next search"),
    ("Alt+C", "Match case"),
    ("Alt+W", "Match whole words"),
    ("Alt+P", "Match the full path"),
    ("Alt+D", "Match diacritics"),
    ("Alt+↑ / Alt+↓", "Previous / next filter"),
    ("Ctrl+N", "New window"),
    ("Ctrl+,", "Settings"),
    ("Ctrl+Q", "Quit"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::kind::classify;

    fn type_of(name: &str, is_dir: bool) -> String {
        type_name(classify(name.as_bytes(), is_dir), extension(name, is_dir))
    }

    #[test]
    fn type_names() {
        assert_eq!(type_of("photos.jpg", true), "Folder");
        assert_eq!(type_of("a.png", false), "PNG image");
        assert_eq!(type_of("Report.Pdf", false), "PDF document");
        assert_eq!(type_of("linux.tar.gz", false), "GZ archive");
        assert_eq!(type_of("Makefile", false), "Source");
        assert_eq!(type_of("notes", false), "File");
        assert_eq!(type_of("a.xyz", false), "XYZ file");
        assert_eq!(type_of(".bashrc", false), "Source");
    }

    #[test]
    fn extensions_keep_their_case() {
        assert_eq!(extension("IMG.JPG", false), "JPG");
        assert_eq!(extension("a.b.c", false), "c");
        assert_eq!(extension(".bashrc", false), "");
        assert_eq!(extension("trailing.", false), "");
        assert_eq!(extension("dir.d", true), "");
    }

    #[test]
    fn every_filter_has_a_distinct_name() {
        let mut names: Vec<&str> = Filter::ALL.into_iter().map(filter).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Filter::ALL.len());
    }

    #[test]
    fn counts_and_states() {
        assert_eq!(folder_count(1), "1 folder");
        assert_eq!(folder_count(0), "0 folders");
        assert_eq!(
            index_status(IndexState::Watching, 1234),
            format!("{} entries · watching for changes", format::count(1234))
        );
        assert_ne!(on_close(OnClose::Quit), on_close(OnClose::Hide));
        assert_eq!(bad_lines(1).split(' ').next(), Some("One"));
    }

    #[test]
    fn status_details() {
        assert_eq!(results(1), "1 result");
        assert_eq!(results(0), "0 results");
        assert_eq!(
            item_details("210 KB", "2026-10-25 22:41", "/home/u/a.pdf"),
            "210 KB  ·  Modified 2026-10-25 22:41  ·  /home/u/a.pdf"
        );
        // A folder without a shown size, an unknown time.
        assert_eq!(item_details("", "", "/srv"), "/srv");
        assert_eq!(selection_summary(3, None), "3 items selected");
        assert_eq!(selection_summary(3, Some((0, 0))), "3 items selected");
        assert_eq!(
            selection_summary(2, Some((2, 3 << 20))),
            "2 items selected  ·  3.0 MB"
        );
        assert_eq!(
            selection_summary(3, Some((1, 512))),
            "3 items selected  ·  512 B in 1 file"
        );
        assert_eq!(
            selection_summary(1500, Some((1200, 2048))),
            format!(
                "{} items selected  ·  2.0 KB in {} files",
                format::count(1500),
                format::count(1200)
            )
        );
    }

    #[test]
    fn badges_follow_the_options() {
        assert!(mode_badges(&QueryOptions::default()).is_empty());
        let o = QueryOptions {
            case_sensitive: true,
            match_diacritics: true,
            ..QueryOptions::default()
        };
        assert_eq!(mode_badges(&o), ["Case", "Diacritics"]);
    }
}
