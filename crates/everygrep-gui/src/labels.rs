//! Words the window shows for core concepts (filter names, file types and
//! matching modes) and the texts of the settings window that the program
//! fills in. They are kept together here so they can be translated.

use everygrep_core::filter::Filter;
use everygrep_core::kind::FileKind;
use everygrep_core::query::QueryOptions;
use everygrep_core::sort::FolderSize;

use crate::engine::IndexState;
use crate::format;
use crate::i18n::{fill, tr, trn};
use crate::settings::{Density, LanguageChoice, OnClose, ThemeChoice};

/// Name of a filter in the filter selector.
pub fn filter(f: Filter) -> &'static str {
    match f {
        Filter::All => tr("All"),
        Filter::Audio => tr("Audio"),
        Filter::Archive => tr("Archives"),
        Filter::Code => tr("Code"),
        Filter::Document => tr("Documents"),
        Filter::Executable => tr("Executables"),
        Filter::Folder => tr("Folders"),
        Filter::Picture => tr("Pictures"),
        Filter::Video => tr("Videos"),
    }
}

/// The words for a kind: after an extension (`{}` stands for it, as in
/// `PNG image`) and on its own.
fn kind_nouns(kind: FileKind) -> (&'static str, &'static str) {
    match kind {
        FileKind::Folder => ("", tr("Folder")),
        FileKind::File => (tr("{} file"), tr("File")),
        FileKind::Text => (tr("{} text"), tr("Text")),
        FileKind::Document | FileKind::Pdf => (tr("{} document"), tr("Document")),
        FileKind::Spreadsheet => (tr("{} spreadsheet"), tr("Spreadsheet")),
        FileKind::Presentation => (tr("{} presentation"), tr("Presentation")),
        FileKind::Image => (tr("{} image"), tr("Image")),
        FileKind::Audio => (tr("{} audio"), tr("Audio")),
        FileKind::Video => (tr("{} video"), tr("Video")),
        FileKind::Archive => (tr("{} archive"), tr("Archive")),
        FileKind::Code => (tr("{} source"), tr("Source")),
        FileKind::Executable => (tr("{} program"), tr("Program")),
        FileKind::Font => (tr("{} font"), tr("Font")),
        FileKind::DiskImage => (tr("{} disk image"), tr("Disk image")),
        FileKind::Link => (tr("{} link"), tr("Link")),
    }
}

/// Text of the Type column: `Folder`, `PNG image`, `TAR.GZ archive` would be
/// `GZ archive` (the last extension), `Source` for a `Makefile`.
pub fn type_name(kind: FileKind, extension: &str) -> String {
    let (after, alone) = kind_nouns(kind);
    if kind == FileKind::Folder || extension.is_empty() {
        alone.to_owned()
    } else {
        fill(after, &[&extension.to_uppercase()])
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
        (o.case_sensitive, tr("Case")),
        (o.whole_words, tr("Whole words")),
        (o.match_path, tr("Path")),
        (o.match_diacritics, tr("Diacritics")),
        (o.regex, tr("Regex")),
        (o.prefix, tr("Prefix")),
        (o.suffix, tr("Suffix")),
        (o.ignore_punctuation, tr("No punctuation")),
        (o.ignore_whitespace, tr("No whitespace")),
    ]
    .into_iter()
    .filter_map(|(on, word)| on.then_some(word))
    .collect()
}

/// `1 result`, `12 results`.
pub fn results(n: usize) -> String {
    fill(trn("{} result", "{} results", n), &[&format::count(n)])
}

/// Separator between the parts of the status line.
pub const STATUS_GAP: &str = "  ·  ";

/// The state of the index for the status line.
pub fn state_word(state: IndexState) -> &'static str {
    match state {
        IndexState::Loading => tr("loading"),
        IndexState::Scanning => tr("indexing"),
        IndexState::CatchingUp => tr("catching up"),
        IndexState::Watching => tr("watching for changes"),
        IndexState::Static => tr("not watching changes"),
    }
}

/// `3 selected` in the status line, without details of the selection.
pub fn selected(n: usize) -> String {
    fill(tr("{} selected"), &[&format::count(n)])
}

/// `617 178 indexed` in the status line.
pub fn indexed(n: usize) -> String {
    fill(tr("{} indexed"), &[&format::count(n)])
}

/// Details of one selected item for the status line: its size (empty for a
/// folder whose size is not shown), modification time and full path.
pub fn item_details(size: &str, modified: &str, path: &str) -> String {
    let modified = (!modified.is_empty()).then(|| fill(tr("Modified {}"), &[&modified]));
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
    let mut out = fill(
        trn("{} item selected", "{} items selected", n),
        &[&format::count(n)],
    );
    match files {
        Some((0, _)) | None => {}
        Some((f, bytes)) if f == n => {
            out.push_str(STATUS_GAP);
            out.push_str(&format::size(bytes));
        }
        Some((f, bytes)) => {
            out.push_str(STATUS_GAP);
            out.push_str(&fill(
                trn("{} in {} file", "{} in {} files", f),
                &[&format::size(bytes), &format::count(f)],
            ));
        }
    }
    out
}

/// `1 folder`, `3 folders`.
pub fn folder_count(n: usize) -> String {
    fill(trn("{} folder", "{} folders", n), &[&format::count(n)])
}

/// `1 item`, `12 items`: a folder's size when it shows the number of items,
/// and several items in messages.
pub fn items(n: usize) -> String {
    fill(trn("{} item", "{} items", n), &[&format::count(n)])
}

/// Choices for what a folder's Size shows.
pub fn folder_size(f: FolderSize) -> &'static str {
    match f {
        FolderSize::Total => tr("Total size"),
        FolderSize::Items => tr("Number of items"),
        FolderSize::None => tr("Nothing"),
    }
}

/// What the chosen folder size means.
pub fn folder_size_note(f: FolderSize) -> &'static str {
    match f {
        FolderSize::Total => tr(
            "The size of all files inside the folder and its subfolders. Folders sort by it together with files.",
        ),
        FolderSize::Items => tr(
            "How many files and folders are directly inside. Sorting by Size puts folders apart from files, by this number.",
        ),
        FolderSize::None => tr("Folders sort by Size as if they were empty."),
    }
}

/// Choices for what closing the window does.
pub fn on_close(o: OnClose) -> &'static str {
    match o {
        OnClose::Quit => tr("Quit EveryGrep"),
        OnClose::Hide => tr("Hide the window and keep running"),
    }
}

/// What the chosen close behaviour means.
pub fn on_close_note(o: OnClose) -> &'static str {
    match o {
        OnClose::Quit => tr(
            "The index is saved and EveryGrep ends. The next start loads it again, which takes a moment for large indexes.",
        ),
        OnClose::Hide => tr(
            "EveryGrep keeps the index in memory and keeps watching for changes. Starting EveryGrep again (for example with its shortcut) shows the window at once. Ctrl+Q or Quit in the ⋯ menu ends it.",
        ),
    }
}

/// Choices for the window's colors.
pub fn theme(t: ThemeChoice) -> &'static str {
    match t {
        ThemeChoice::System => tr("Same as the desktop"),
        ThemeChoice::Light => tr("Light"),
        ThemeChoice::Dark => tr("Dark"),
    }
}

/// Choices for how tightly the result list is packed.
pub fn density(d: Density) -> &'static str {
    match d {
        Density::Comfortable => tr("Comfortable"),
        Density::Compact => tr("Compact"),
    }
}

/// Choices for the language. Languages are named in their own language, so
/// they can be found whatever the window shows.
pub fn language(l: LanguageChoice) -> &'static str {
    match l {
        LanguageChoice::System => tr("Same as the system"),
        LanguageChoice::Fixed(lang) => lang.native_name(),
    }
}

/// The state of the index for the settings window.
pub fn index_status(state: IndexState, indexed: usize) -> String {
    let entries = format::count(indexed);
    let text = match state {
        IndexState::Loading => return tr("Loading the index…").to_owned(),
        IndexState::Scanning => return tr("Indexing…").to_owned(),
        IndexState::CatchingUp => trn(
            "{} entry · catching up",
            "{} entries · catching up",
            indexed,
        ),
        IndexState::Watching => trn(
            "{} entry · watching for changes",
            "{} entries · watching for changes",
            indexed,
        ),
        IndexState::Static => trn(
            "{} entry · not watching changes",
            "{} entries · not watching changes",
            indexed,
        ),
    };
    fill(text, &[&entries])
}

/// The message over the empty result list while the index is not ready.
pub fn loading_message(state: IndexState) -> Option<&'static str> {
    match state {
        IndexState::Loading => Some(tr("Loading the index…")),
        IndexState::Scanning => Some(tr(
            "Indexing your files. This happens once; later starts are instant.",
        )),
        _ => None,
    }
}

/// Shown on the Indexing page when the window was started with its own
/// folders or index file.
pub fn index_from_command_line() -> &'static str {
    tr(
        "This window was started with --root or --index-file. Changes here apply to this window only and are not saved.",
    )
}

/// Titles and button of the folder chooser.
pub fn choose_roots() -> &'static str {
    tr("Add folders to index")
}

pub fn choose_excludes() -> &'static str {
    tr("Add folders to exclude")
}

pub fn choose_accept() -> &'static str {
    tr("Add")
}

pub fn chooser_failed(e: &str) -> String {
    fill(tr("Cannot open the folder chooser: {}"), &[&e])
}

pub fn cannot_save(e: &std::io::Error) -> String {
    fill(tr("Cannot save the settings: {}"), &[e])
}

pub fn cannot_read(path: &std::path::Path, e: &std::io::Error) -> String {
    fill(
        tr("Cannot read {}: {}. Changes are not saved until it can be read."),
        &[&path.display(), e],
    )
}

pub fn bad_lines(n: usize) -> String {
    fill(
        trn(
            "One line of the settings file was not understood and is ignored.",
            "{} lines of the settings file were not understood and are ignored.",
            n,
        ),
        &[&format::count(n)],
    )
}

pub fn no_settings_folder() -> &'static str {
    tr("Neither XDG_CONFIG_HOME nor HOME is set, so settings cannot be saved.")
}

/// How to add a GNOME shortcut that opens the window.
pub fn shortcut_steps() -> &'static str {
    tr(
        "In GNOME: open Settings → Keyboard → View and Customize Shortcuts → Custom Shortcuts, press +, enter the name EveryGrep and the command below, then press the keys you want (for example Super+F).",
    )
}

pub fn command_copied() -> &'static str {
    tr("Copied the command.")
}

/// Copied names or paths, for the status line.
pub fn copied_names(n: usize) -> String {
    fill(
        trn("Copied the name.", "Copied {} names.", n),
        &[&format::count(n)],
    )
}

pub fn copied_paths(n: usize) -> String {
    fill(
        trn("Copied the path.", "Copied {} paths.", n),
        &[&format::count(n)],
    )
}

/// The keyboard shortcuts dialog: keys and what they do.
pub fn shortcuts() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "↑ ↓ PgUp PgDn Home End",
            tr("Move in the results and select"),
        ),
        (tr("Shift + movement"), tr("Extend the selection")),
        (
            tr("Ctrl + movement"),
            tr("Move without changing the selection"),
        ),
        ("Ctrl+Space", tr("Add or remove one row")),
        ("Ctrl+A", tr("Select all")),
        ("Ctrl+I", tr("Invert selection")),
        ("Enter", tr("Open")),
        ("Ctrl+Enter", tr("Open the containing folder")),
        ("Ctrl+C", tr("Copy the name")),
        ("Ctrl+Shift+C", tr("Copy the full path")),
        ("F2", tr("Rename")),
        ("Delete", tr("Move to the trash")),
        ("Shift+Delete", tr("Delete permanently")),
        ("Menu, Shift+F10", tr("Context menu")),
        ("Esc", tr("Clear the selection, then the search")),
        ("Ctrl+F, Ctrl+L", tr("Focus the search field")),
        ("Alt+← / Alt+→", tr("Previous / next search")),
        ("Alt+C", tr("Match case")),
        ("Alt+W", tr("Match whole words")),
        ("Alt+P", tr("Match the full path")),
        ("Alt+D", tr("Match diacritics")),
        ("Alt+R", tr("Regular expression")),
        ("Alt+B", tr("Match prefix")),
        ("Alt+E", tr("Match suffix")),
        ("Alt+U", tr("Ignore punctuation")),
        ("Alt+S", tr("Ignore whitespace")),
        ("Alt+↑ / Alt+↓", tr("Previous / next filter")),
        ("Ctrl+S", tr("Export results")),
        ("Ctrl+N", tr("New window")),
        ("Ctrl+,", tr("Settings")),
        ("Ctrl+Q", tr("Quit")),
    ]
}

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
        let o = QueryOptions {
            regex: true,
            suffix: true,
            ignore_whitespace: true,
            ..QueryOptions::default()
        };
        assert_eq!(mode_badges(&o), ["Regex", "Suffix", "No whitespace"]);
    }
}
