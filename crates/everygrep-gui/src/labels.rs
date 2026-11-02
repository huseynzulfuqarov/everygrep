//! Words the window shows for core concepts (filter names, file types and
//! matching modes) and the texts of the settings window that the program
//! fills in. They are kept together here so they can be translated.

use everygrep_core::filter::Filter;
use everygrep_core::kind::FileKind;
use everygrep_core::query::{HiddenFiles, QueryOptions};
use everygrep_core::rules::{RuleError, RuleList};
use everygrep_core::sort::FolderSize;

use crate::clipboard::Operation;
use crate::engine::{Busy, IndexState, Notice, Stopped, TaskKind, Unwatched};
use crate::format::{self, SizeStandard, SizeUnit};
use crate::i18n::{fill, tr, trn};
use crate::settings::{Density, LanguageChoice, OnClose, ThemeChoice};
use crate::transfer::Outcome;

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

pub fn hidden_files(mode: HiddenFiles) -> &'static str {
    match mode {
        HiddenFiles::Show => tr("Show hidden files"),
        HiddenFiles::Hide => tr("Hide hidden files"),
        HiddenFiles::Only => tr("Only hidden files"),
    }
}

/// Short words for the matching modes that are on, for the status line.
pub fn mode_badges(o: &QueryOptions) -> Vec<&'static str> {
    [
        (o.hidden_files == HiddenFiles::Hide, tr("Hidden excluded")),
        (o.hidden_files == HiddenFiles::Only, tr("Hidden only")),
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
        IndexState::Unwatched => tr("changes not watched"),
    }
}

/// Indexed folders whose changes are not watched, for the status line.
pub fn unwatched(u: Unwatched) -> String {
    let text = if u.limit_reached {
        trn(
            "{} folder not watched: watch limit reached",
            "{} folders not watched: watch limit reached",
            u.folders,
        )
    } else {
        trn("{} folder not watched", "{} folders not watched", u.folders)
    };
    fill(text, &[&format::count(u.folders)])
}

/// `3 selected` in the status line, without details of the selection.
pub fn selected(n: usize) -> String {
    fill(tr("{} selected"), &[&format::count(n)])
}

/// `617 178 indexed` in the status line.
pub fn indexed(n: usize) -> String {
    fill(tr("{} indexed"), &[&format::count(n)])
}

/// Details of one selected item for the status line: whether its drive is
/// not connected, its size (empty for a folder whose size is not shown),
/// modification time and full path.
pub fn item_details(size: &str, modified: &str, path: &str, offline: bool) -> String {
    let modified = (!modified.is_empty()).then(|| fill(tr("Modified {}"), &[&modified]));
    let offline = offline.then(|| tr("Offline: the drive is not connected").to_owned());
    [
        offline,
        Some(size.to_owned()),
        modified,
        Some(path.to_owned()),
    ]
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

/// `2 offline` after the number of selected items.
pub fn offline_count(n: usize) -> String {
    fill(tr("{} offline"), &[&format::count(n)])
}

/// Why a file action was refused.
pub fn offline_refused() -> &'static str {
    tr(
        "Items on a drive that is not connected cannot be opened, renamed or deleted. Connect the drive first.",
    )
}

/// Choices for what happens to a removed drive's entries.
pub fn removed_drives(keep: bool) -> &'static str {
    if keep {
        tr("Keep its entries, shown dimmed")
    } else {
        tr("Forget its entries")
    }
}

/// A drive's name in the Drives list: its label, or its UUID.
pub fn drive_title(label: &str, uuid: &str) -> String {
    if label.is_empty() {
        fill(tr("Drive {}"), &[&uuid])
    } else {
        label.to_owned()
    }
}

/// The second line of a drive in the Drives list.
pub fn drive_details(
    d: &crate::engine::DriveStatus,
    zone: &everygrep_core::localtime::Zone,
) -> String {
    let state = if d.online {
        tr("Connected")
    } else {
        tr("Not connected")
    };
    let mut parts = vec![fill(tr("UUID {}"), &[&d.uuid]), state.to_owned()];
    if d.ignored {
        parts.push(tr("Not indexed").to_owned());
    } else if d.scanning {
        parts.push(tr("Indexing…").to_owned());
    } else if d.indexed {
        parts.push(fill(
            trn("{} entry", "{} entries", d.entries as usize),
            &[&format::count(d.entries as usize)],
        ));
    } else {
        parts.push(tr("Not indexed").to_owned());
    }
    if !d.online && d.last_seen > 0 {
        let when = format::timestamp(d.last_seen.saturating_mul(1_000_000_000), zone);
        parts.push(fill(tr("Last seen {}"), &[&when]));
    }
    parts.push(d.path.display().to_string());
    parts.join(STATUS_GAP)
}

/// The button that stops or starts indexing a drive.
pub fn drive_action(index_it: bool) -> &'static str {
    if index_it {
        tr("Index this drive")
    } else {
        tr("Don't index this drive")
    }
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

/// What starting on login does with the chosen close behaviour.
pub fn start_on_login_note(o: OnClose) -> &'static str {
    match o {
        OnClose::Quit => tr(
            "The window opens when you log in. To start without a window, choose \"Hide the window and keep running\" above.",
        ),
        OnClose::Hide => tr(
            "EveryGrep starts without a window, loads the index and watches for changes; starting it again shows the window at once.",
        ),
    }
}

pub fn cannot_change_start_on_login(e: &std::io::Error) -> String {
    fill(tr("Cannot change starting on login: {}"), &[e])
}

/// Heading and message of the dialog a launch shows when the open window
/// cannot take its request.
pub fn stalled(stall: crate::instance::Stall) -> (&'static str, &'static str) {
    match stall {
        crate::instance::Stall::ShuttingDown => (
            tr("The previous EveryGrep window is still closing"),
            tr(
                "This usually takes a few seconds. EveryGrep opens once it has closed; you can also terminate the existing instance.",
            ),
        ),
        crate::instance::Stall::NoAnswer => (
            tr("The open EveryGrep window does not answer"),
            tr("Terminate the existing instance? EveryGrep opens as soon as it has ended."),
        ),
    }
}

pub fn stalled_terminating() -> &'static str {
    tr("Waiting for EveryGrep to close…")
}

pub fn stalled_timed_out() -> (&'static str, &'static str) {
    (
        tr("The previous EveryGrep window did not close in time"),
        tr("Try again in a moment, or end it in System Monitor."),
    )
}

pub fn cannot_terminate(e: &std::io::Error) -> String {
    fill(tr("Cannot terminate the existing instance: {}"), &[e])
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
/// Choices for the size format.
pub fn size_unit(u: SizeUnit) -> &'static str {
    match u {
        SizeUnit::Auto => tr("Automatic"),
        SizeUnit::Bytes => tr("Bytes"),
        SizeUnit::Kilo => "KB",
        SizeUnit::Mega => "MB",
        SizeUnit::Giga => "GB",
    }
}

/// Choices for the size units.
pub fn size_standard(s: SizeStandard) -> &'static str {
    match s {
        SizeStandard::Jedec => tr("1 KB = 1024 bytes"),
        SizeStandard::Iec => tr("1 KiB = 1024 bytes"),
        SizeStandard::Metric => tr("1 KB = 1000 bytes"),
    }
}

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
        IndexState::Unwatched => trn(
            "{} entry · watching for changes is off",
            "{} entries · watching for changes is off",
            indexed,
        ),
    };
    fill(text, &[&entries])
}

/// The long operation in the status line: `Indexing: 42%`, or what has
/// been done so far when the amount of work is not known.
pub fn busy(b: &Busy) -> String {
    let n = usize::try_from(b.done).unwrap_or(usize::MAX);
    let done = format::count(n);
    match (&b.kind, b.percent) {
        (TaskKind::Indexing, Some(p)) => fill(tr("Indexing: {}%"), &[&p]),
        (TaskKind::Indexing, None) => {
            fill(trn("Indexing: {} item", "Indexing: {} items", n), &[&done])
        }
        (TaskKind::Rescanning, Some(p)) => fill(tr("Rescanning: {}%"), &[&p]),
        (TaskKind::Rescanning, None) => fill(
            trn("Rescanning: {} item", "Rescanning: {} items", n),
            &[&done],
        ),
        (TaskKind::Drive(name), Some(p)) => fill(tr("Indexing {0}: {1}%"), &[name, &p]),
        (TaskKind::Drive(name), None) => fill(
            trn("Indexing {0}: {1} item", "Indexing {0}: {1} items", n),
            &[name, &done],
        ),
        (TaskKind::Updating, Some(p)) => fill(tr("Updating: {}%"), &[&p]),
        (TaskKind::Updating, None) => fill(
            trn("Updating: {} folder", "Updating: {} folders", n),
            &[&done],
        ),
        (TaskKind::Searching, p) => fill(tr("Searching: {}%"), &[&p.unwrap_or(0)]),
        (TaskKind::SearchingContent, p) => {
            fill(tr("Searching file contents: {}%"), &[&p.unwrap_or(0)])
        }
        (TaskKind::ComparingFiles, p) => fill(tr("Comparing files: {}%"), &[&p.unwrap_or(0)]),
        (TaskKind::Sorting, p) => fill(tr("Sorting: {}%"), &[&p.unwrap_or(0)]),
        (TaskKind::Copying, Some(p)) => fill(tr("Copying: {}%"), &[&p]),
        (TaskKind::Copying, None) => tr("Copying…").to_owned(),
        (TaskKind::Moving, Some(p)) => fill(tr("Moving: {}%"), &[&p]),
        (TaskKind::Moving, None) => tr("Moving…").to_owned(),
    }
}

/// The note in the status line after a long operation was cancelled.
pub fn stopped(s: &Stopped) -> String {
    match s {
        Stopped::Index => tr(
            "Indexing cancelled. Nothing was saved; use Rebuild index now in the settings to index again.",
        )
        .to_owned(),
        Stopped::Rescan => tr("Rescan cancelled. The previous index is kept.").to_owned(),
        Stopped::Drive(name) => fill(
            tr("Indexing {} cancelled. It goes on where it stopped when it is connected again."),
            &[name],
        ),
        Stopped::Search => tr("Search cancelled. The list shows the previous results.").to_owned(),
        Stopped::ContentSearch => {
            tr("Content search cancelled. The list shows the matches found so far.").to_owned()
        }
        Stopped::Sort(_) => tr("Sort cancelled. The previous order is kept.").to_owned(),
    }
}

/// A note of the engine in the status line.
pub fn notice(n: &Notice) -> String {
    match n {
        Notice::IndexRebuilt { damaged: true, .. } => tr(
            "The saved index is damaged or from another version, so it is built again. Nothing else is lost.",
        )
        .to_owned(),
        Notice::IndexRebuilt { error, .. } => fill(
            tr("The saved index could not be read ({}), so it is built again."),
            &[error],
        ),
    }
}

/// The choices of the Indexing page's rescan, in the order of
/// [`crate::schedule::Schedule::KINDS`].
pub fn rescan_kinds() -> [&'static str; 3] {
    [tr("Never"), tr("Every N hours"), tr("Every day at")]
}

/// Why a typed rescan time was not taken.
pub fn bad_rescan_time() -> &'static str {
    tr("Type the time as HH:MM on the 24-hour clock, for example 03:00. The previous time is kept.")
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

/// Why a change of the excluded folders or file filters was refused.
pub fn bad_rule(e: &RuleError) -> String {
    let list = match e.list {
        RuleList::Folders => tr("Excluded folders"),
        RuleList::IncludeFiles => tr("Include only files"),
        RuleList::ExcludeFiles => tr("Exclude files"),
    };
    if e.reason.is_empty() {
        fill(
            tr("{0}: “{1}” has an empty regular expression. Nothing was changed."),
            &[&list, &e.item],
        )
    } else {
        fill(
            tr("{0}: “{1}” is not a valid pattern ({2}). Nothing was changed."),
            &[&list, &e.item, &e.reason],
        )
    }
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
        "In GNOME: open Settings → Keyboard → View and Customize Shortcuts → Custom Shortcuts, press +, enter a name (for example EveryGrep) and one of the commands below, then press the keys you want (for example Super+F). Add one shortcut for each command you want.",
    )
}

/// Title and note of the commands on the Keyboard shortcut page: show,
/// toggle, new window.
pub fn shortcut_commands() -> [(&'static str, &'static str); 3] {
    [
        (
            tr("Show the window"),
            tr("Starts EveryGrep, or brings its window to the front with the search field ready."),
        ),
        (
            tr("Show or hide the window"),
            tr(
                "Like Show, but when the window is already in front it closes it, as the close button does (it hides or quits, as set on the Window page).",
            ),
        ),
        (
            tr("Open a new window"),
            tr("Opens another window over the same index, even when one is open."),
        ),
    ]
}

/// Titles of the Keyboard page's dialog.
pub fn add_shortcut() -> &'static str {
    tr("Add shortcut")
}

pub fn edit_shortcut() -> &'static str {
    tr("Edit shortcut")
}

/// The global scope in the dialog's "Use shortcut in" choice.
pub fn scope_global_choice() -> &'static str {
    tr("Global (any focus)")
}

/// A key of another command in an overlapping scope; both keep it.
pub fn also_used(scope: &str, command: &str) -> String {
    fill(tr("Also used in {0} by {1}."), &[&scope, &command])
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

pub fn copied_folders(n: usize) -> String {
    fill(
        trn("Copied the folder path.", "Copied {} folder paths.", n),
        &[&format::count(n)],
    )
}

/// Cut or Copy put `n` items on the clipboard.
pub fn on_clipboard(op: Operation, n: usize) -> String {
    let count = format::count(n);
    match op {
        Operation::Copy => fill(
            trn(
                "Copied {} item. Paste it into a folder to copy it there.",
                "Copied {} items. Paste them into a folder to copy them there.",
                n,
            ),
            &[&count],
        ),
        Operation::Cut => fill(
            trn(
                "Cut {} item. Paste it into a folder to move it there.",
                "Cut {} items. Paste them into a folder to move them there.",
                n,
            ),
            &[&count],
        ),
    }
}

/// The note after a paste into `folder` (its name).
pub fn pasted(op: Operation, out: &Outcome, folder: &str) -> (String, bool) {
    let n = out.done.len();
    let tried = n + out.failed.len() + out.unchanged;
    let mut parts = Vec::new();
    if out.cancelled {
        parts.push(fill(
            trn(
                "Paste cancelled after {0} item into {1}.",
                "Paste cancelled after {0} items into {1}.",
                n,
            ),
            &[&format::count(n), &folder],
        ));
    } else if n > 0 || out.failed.is_empty() {
        let text = match op {
            Operation::Copy => trn("Copied {0} item into {1}.", "Copied {0} items into {1}.", n),
            Operation::Cut => trn("Moved {0} item into {1}.", "Moved {0} items into {1}.", n),
        };
        parts.push(fill(text, &[&format::count(n), &folder]));
    }
    if out.renamed > 0 {
        parts.push(fill(
            trn(
                "{} got a new name because the name was taken.",
                "{} got new names because the names were taken.",
                out.renamed,
            ),
            &[&items(out.renamed)],
        ));
    }
    if out.unchanged > 0 {
        parts.push(fill(
            trn(
                "{} was already in this folder.",
                "{} were already in this folder.",
                out.unchanged,
            ),
            &[&items(out.unchanged)],
        ));
    }
    if out.skipped_special > 0 {
        let k = usize::try_from(out.skipped_special).unwrap_or(usize::MAX);
        parts.push(fill(
            trn(
                "{} special file (pipe, socket or device) was left out.",
                "{} special files (pipes, sockets or devices) were left out.",
                k,
            ),
            &[&format::count(k)],
        ));
    }
    if let Some((path, why)) = out.failed.first() {
        parts.push(fill(
            tr("Could not paste {0} of {1}: {2}: {3}."),
            &[
                &format::count(out.failed.len()),
                &format::count(tried),
                &path.display(),
                why,
            ],
        ));
    }
    let error = !out.failed.is_empty() || out.skipped_special > 0;
    (parts.join(" "), error)
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
    fn unwatched_folders_say_why_when_the_limit_is_the_cause() {
        let mut u = Unwatched {
            folders: 1,
            limit_reached: false,
        };
        assert_eq!(unwatched(u), "1 folder not watched");
        u.folders = 3;
        u.limit_reached = true;
        assert_eq!(unwatched(u), "3 folders not watched: watch limit reached");
    }

    #[test]
    fn progress_texts() {
        let busy_with = |kind: TaskKind, done: u64, percent: Option<u8>| {
            busy(&Busy {
                id: 1,
                kind,
                done,
                percent,
                cancellable: true,
            })
        };
        assert_eq!(busy_with(TaskKind::Indexing, 5, Some(42)), "Indexing: 42%");
        // No estimate (btrfs): the count instead.
        assert_eq!(
            busy_with(TaskKind::Indexing, 123_456, None),
            format!("Indexing: {} items", format::count(123_456))
        );
        assert_eq!(busy_with(TaskKind::Indexing, 1, None), "Indexing: 1 item");
        assert_eq!(
            busy_with(TaskKind::Rescanning, 9, Some(99)),
            "Rescanning: 99%"
        );
        assert_eq!(
            busy_with(TaskKind::Drive("USB".into()), 3, Some(7)),
            "Indexing USB: 7%"
        );
        assert_eq!(
            busy_with(TaskKind::Drive("USB".into()), 3, None),
            "Indexing USB: 3 items"
        );
        assert_eq!(
            busy_with(TaskKind::Updating, 2, None),
            "Updating: 2 folders"
        );
        assert_eq!(busy_with(TaskKind::Sorting, 0, Some(50)), "Sorting: 50%");
        assert_eq!(busy_with(TaskKind::Searching, 0, None), "Searching: 0%");
        assert_eq!(
            busy_with(TaskKind::SearchingContent, 30, Some(12)),
            "Searching file contents: 12%"
        );
        assert!(stopped(&Stopped::Drive("USB".into())).contains("USB"));
    }

    #[test]
    fn status_details() {
        assert_eq!(results(1), "1 result");
        assert_eq!(results(0), "0 results");
        assert_eq!(
            item_details("210 KB", "2026-10-25 22:41", "/home/u/a.pdf", false),
            "210 KB  ·  Modified 2026-10-25 22:41  ·  /home/u/a.pdf"
        );
        // A folder without a shown size, an unknown time.
        assert_eq!(item_details("", "", "/srv", false), "/srv");
        assert_eq!(
            item_details("1 KB", "", "/run/media/u/USB/a", true),
            "Offline: the drive is not connected  ·  1 KB  ·  /run/media/u/USB/a"
        );
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
