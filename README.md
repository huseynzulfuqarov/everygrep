# EveryGrep

Instant file name search for Linux.

EveryGrep keeps a compact in-memory index of every file and folder name below
the folders you choose and finds matches anywhere in a name (not only at word
starts) as you type. A desktop window and a command line program share the
same engine and the same saved index.

**Status:** early development, no release yet (version 0.1.0).

Screenshots: to be added to `docs/images/`.

## Features

- **Instant results.** Results update on every key press; the search runs on
  all CPU cores over names kept in memory.
- **Always current.** The index follows file changes with inotify while the
  window runs, and catches up with changes made while it was closed by
  re-reading only the folders that changed.
- **A real query language.** AND, OR, NOT, grouping, quoted text, wildcards,
  whole words, full paths, extensions, size and date ranges
  ([query language](docs/query-language.md)).
- **Forgiving matching.** Case and diacritics are ignored by default, so
  `sekil` finds `Şəkil` and `cafe` finds `café`.
- **Folder sizes.** Every folder knows the total size of the files below it,
  kept current as files change; sort and filter by it.
- **Removable drives.** A USB stick or external disk is indexed when it is
  plugged in; when it is removed, its entries stay searchable, shown dimmed,
  and come back to life when it is plugged in again, wherever it is mounted.
- **Keyboard first.** Every action has a key; open, open with another
  application, open the containing folder, cut, copy and paste files, copy
  names or paths, rename in place, trash or delete.
- **Fits the desktop.** Light and dark themes that follow GNOME, one window
  per index that a desktop shortcut brings forward, a menu bar or a compact
  `⋯` menu.
- **Translated.** English, Azerbaijani and Turkish; the window follows the
  system language or a choice in the settings.
- **Private.** All data stays on this computer (see [Privacy](#privacy)).

## Installing

There are no packages yet; build from source (see [Building](#building)).

### Into `~/.local`, by hand

After `cargo build --release --locked` in the checkout:

```sh
install -Dm755 target/release/everygrep target/release/everygrep-gui -t ~/.local/bin/
install -Dm644 data/az.huseynz.EveryGrep.desktop -t ~/.local/share/applications/
for icon in data/icons/hicolor/*/apps/az.huseynz.EveryGrep.*; do
    install -Dm644 "$icon" ~/.local/share/"${icon#data/}"
done
```

`~/.local/bin` must be on `PATH` for the desktop entry's `Exec=everygrep-gui`
to work (on GNOME it usually is). To remove it again, delete those files;
the index (`~/.cache/everygrep/`) and the settings (`~/.config/everygrep/`)
are separate and can be deleted too.

### Arch Linux

`packaging/arch/PKGBUILD` builds a package from a git checkout (by default
the one it lives in, at branch `main`): `cd packaging/arch && makepkg -si`.
It installs both programs to `/usr/bin`, the desktop entry, the icons, the
licenses (including `THIRD-PARTY-LICENSES.md`) and the documentation. The
project's public URL is not set yet; the PKGBUILD has a placeholder for it.

## Quick start

```sh
everygrep-gui                 # index your home folder and open the window
everygrep-gui report pdf      # open with a query
everygrep search 'ext:pdf modified:<7d'
```

The first start scans the home folder (or the folders set in the settings)
and saves the index; later starts load it and catch up in a moment.

## Query syntax

| Query | Finds |
|---|---|
| `report pdf` | names containing both words (anywhere, not only at word starts) |
| `jpg\|png` | either word (`\|` binds tighter than a space) |
| `!draft` | names without the word |
| `<draft\|final> pdf` | grouping |
| `"two words"` | literal text, including spaces and operators |
| `*.pdf`, `img_??.png` | wildcards, matched against the whole name (in a path `*` stops at `/`, `**` does not) |
| `case:Readme` | match case for this term |
| `wholeword:art` | whole words only: `pop-art.png`, not `party.png` |
| `prefix:rep`, `suffix:.tar.gz` | the term at the start / end of the name |
| `nopunct:foo-bar`, `nospace:myfile` | ignore punctuation / whitespace: `foo_bar.txt`, `my file.txt` |
| `regex:^img_\d+\.jpe?g$` | a regular expression (to the next space; case is ignored) |
| `diacritics:café` | match diacritics for this term |
| `path:src/main`, `docs/report` | match the full path (any term with `/` does) |
| `file:`, `folder:` | only files / only folders |
| `audio:`, `archive:`, `code:`, `document:`, `executable:`, `image:`, `video:` | the filters, as query words: `image: beach`, `!video:` |
| `ext:pdf;jpg`, `ext:jpg\|png`, `ext:jp*` | extensions |
| `size:>10mb`, `size:1kb..2mb`, `size:0`, `size:large` | size (1 kb = 1 kib = 1024 bytes; `size:1mb` = 1 MB up to just under 2 MB, `size:=1mb` exactly); folders match by their total size |
| `modified:<7d`, `modified:>30d` | changed within / not within a time (`s`, `m`, `h`, `d`, `w`) |
| `modified:2026-10-25`, `modified:2026-10`, `modified:today` | changed on that local day, month or year |
| `modified:>2026-10-25`, `modified:2026-10-01..2026-10-25` | after a day; ranges are inclusive |
| `modified:lastweek`, `modified:thismonth`, `modified:tomorrow` | calendar weeks (from Monday), months, years |
| `created:today`, `accessed:>30d` | created / accessed (read from disk while searching) |
| `content:invoice`, `content:<total "due date">`, `regex:content:^From:` | files whose contents hold the text (read from disk while searching) |
| `duplicates:`, `duplicates:name`, `duplicates:name;size`, `extra-copies:`, `distinct:`, `unique:` | duplicates among the results: same contents by default (read from disk), or same name, size, … |

The full syntax, with every modifier and edge case, is in
[docs/query-language.md](docs/query-language.md).

## The window

```sh
everygrep-gui [--root <folder>]... [--index-file <file>] [--filter <name>]
              [--case] [--whole-words] [--match-path] [--diacritics]
              [--prefix] [--suffix] [--ignore-punctuation]
              [--ignore-whitespace] [--regex]
              [--columns type,extension,created,accessed,attributes]
              [--new-instance] [--new-window] [--toggle] [--background]
              [--settings[=indexing|search|appearance|window|keyboard|advanced]] [query words...]
```

Words after the options become the first query; it runs as soon as the index is
loaded. `everygrep-gui --help` lists the options; a mistyped option or value
stops with an error instead of opening the window. Type to search; `↓` moves into the results. The selector next to the
search field limits results to one kind (folders, documents, pictures, …); the
toggles inside the field switch match case, whole words, full path,
diacritics and regular expressions; the Search menu also has match prefix,
match suffix, ignore punctuation and ignore whitespace, and Find duplicates
(by contents, name or size), which puts a `duplicates:` term at the end of the
query (or takes it out again); the modes that are on
show in the status line. Results come in natural name order: numbers by value
(`file2` before `file10`), case and diacritics ignored (`Çay` next to `cay`),
`ə` after `e`, hyphens and apostrophes skipped. Right-click the
column headers to show the Type, Extension, Created, Accessed and Attributes
columns, and MD5, SHA-256 and CRC-32: hashes of the files on screen,
computed on a thread of their own (the smallest files first, each read once
for all hash columns shown) and kept in the hash cache; these three cannot
be sorted by. The Same name, Same extension and Same size columns count over the
whole index how many entries of the same kind share the name (ignoring
case), how many files the extension and how many files (or folders) the
size, so sorting by one brings the most common to the top. Drag a header border to change a column's width; past the minimum
of the column on its right, or at the border after the last column, the
columns get wider than the window, and the list scrolls sideways (with the
headers) to show the rest; Folder takes the room that is left when they fit.
Double-click a header border to size the column on its left to fit
the rows on screen; View → Size all columns to fit (the spare room goes to
Folder) and Fit columns to window (every column scaled to fill the
window) do it for all shown columns, also from the header's context menu. Created, Accessed and Attributes are not in the index: they are read
from the file system for the rows on screen while their column is shown (on
a helper thread, so their cells fill in a moment later on a slow drive), and
for every result when sorting by them (the status line shows the progress).
Created stays empty where the file system does not record a birth time;
Accessed is coarse on most systems, which update it at most once a day
(`relatime`). Attributes are letters: `R` read-only (the owner may not write),
`H` hidden (the name starts with a dot), `D` folder, `L` symbolic link, `X`
executable file.

| Keys | Action |
|---|---|
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | move and select |
| `Shift` + movement, `Shift`+click | extend the selection |
| `Ctrl+Space`, `Ctrl`+click | add or remove one row |
| `Ctrl+A` | select all |
| `Ctrl+I` | invert the selection (also from the search field) |
| `Enter`, double-click | open |
| `Ctrl+Enter`, double-click in the Folder column | open the containing folder |
| `Shift+Enter` | open with another application, chosen in the desktop's dialog |
| `Ctrl+X` / `Ctrl+C` | cut / copy the items, to paste them into a folder |
| `Ctrl+V` | paste into the selected folder |
| `Ctrl+Shift+C` | copy the full path (one per line) |
| `Ctrl+Alt+C` | copy the name |
| `Ctrl+Alt+Shift+C` | copy the path of the containing folder (each folder once) |
| — | copy path / full path with quotes: each in double quotes, or in single quotes when it has `$`, a backtick, `"`, `\`, `!` or a line break, so a shell takes it literally (context menu, Edit menu; a key can be set) |
| `F2` | rename in place or in a dialog (`Enter` renames, `Esc` cancels; Settings → Window) |
| `Delete` | move to the trash (`gio trash`) |
| `Shift+Delete` | delete permanently, after a confirmation |
| `Esc` | clear the selection, then back to the search |
| `Ctrl+F`, `Ctrl+L` | focus the search field |
| `Alt+←` / `Alt+→` | back / forward through earlier searches |
| `Alt+C`, `Alt+W`, `Alt+P`, `Alt+D` | match case, whole words, full path, diacritics |
| `Alt+R` | regular expression: the whole query is one expression |
| `Alt+B`, `Alt+E` | match prefix, match suffix |
| `Alt+U`, `Alt+S` | ignore punctuation, ignore whitespace |
| `Alt+↑` / `Alt+↓` | previous / next filter |
| `Ctrl+S` | export the results to a CSV, TSV, text, M3U8, JSON or HTML file |
| `F5` | refresh: run the search again and read the Created, Accessed and Attributes values afresh |
| `Ctrl+N` | new window over the same index (also with the menu bar hidden) |
| `Ctrl+W` | close the window, as its close button does |
| `Ctrl+,` | settings |
| `F1` | help: search syntax, regular expressions, command line options |
| `Ctrl+Q` | quit (also when closing only hides the window) |

Right-click a row, or press `Menu` or `Shift+F10`, for a context menu with the
same actions. The index follows these changes by itself.

Cut or copy items, select one folder in the results and paste (`Ctrl+V`,
or Paste into folder in the menus) to copy or move them there. By default,
when a name is taken, the item gets the first free name like
`report (2).pdf`. A copy is written under a temporary name and renamed into
place only when it is complete, so a failure or a cancel leaves nothing
half-copied. A move is a rename on the same file system; across file
systems the items are copied first and only what was copied is removed
afterwards, so a failed move never loses the original. Links are copied as
links, and special files (pipes, sockets, devices) are left out and named
in the status line. A paste that takes longer shows its progress in the
status line with a Cancel button; items already pasted stay. A folder is
never pasted into itself. Settings → Window → File operations can enable
questions for existing names: keep both, skip, skip all conflicts in this paste,
or cancel. Regular files can also be overwritten: a complete temporary copy
replaces the destination atomically after its identity is checked again. Links,
folders and hard links to the source cannot be overwritten. Errors offer retry
or skip. The worker waits for an answer while the window stays responsive.

The result context menu can search the clicked Name, Folder, Size, Modified
or Extension value. Name matches the whole literal name, Folder searches direct
children of that exact folder, Size uses the exact indexed byte count, and
Modified searches that local calendar day (the menu says "Search this day").
Size searches are offered only when byte sizes are known and shown. The menu
captures the query when it opens, so live index changes do not change its value.
`Ctrl+Alt+V` searches the current row's Name value from the result list.

Separate filename commands search a literal substring (`Ctrl+Alt+G`) or the
exact name (`Ctrl+Alt+Shift+F`); two more commands search the name before
its last extension, as a substring or as a whole. Dotfiles without another
dot and folders keep their whole name. These commands follow the existing
context-search window/tab setting, retain the current filter and case choice,
and turn global regex mode off. The name-without-extension commands can have
keys assigned in Keyboard settings and are also in the row context menu.

File undo is off by default. Settings → Window → File operations enables a
session-only history (100 steps by default, adjustable from 1 to 1000).
A typed limit takes effect only with "Apply history limit".
`Ctrl+Z` and `Ctrl+Shift+Z` in the result list undo/redo; `Ctrl+Alt+Z` opens
File undo history, with action filtering, undo through a selected step and CSV
export. That selection includes every newer step, even when filtered out.
Rename, copies and moves within one filesystem are recorded. Trash, permanent
deletion, overwrites, shortcuts and moves across filesystems are excluded.
Undo never replaces a destination. Changed or missing items are skipped with
the reason shown. Undoing a copy sends it to the desktop trash; redo copies
its original source again only if that source is unchanged. A new recorded
action discards redo. Disabling undo clears the session history. History also
has a memory bound of 100,000 recorded tree entries; the oldest whole steps
are dropped to make room when either bound is reached. Unreadable or larger
trees are not recorded; when a rename or paste that changed the disk cannot be
recorded, the notice says so and the older steps are removed too, so undo never
skips over it.

Other programs get the items as files: Cut and Copy offer them on the
clipboard of XWayland (`DISPLAY`) as `x-special/gnome-copied-files`,
`text/uri-list` and the full paths as text, which GNOME passes on to
Wayland programs, so a file manager pastes them and a text field gets the
paths. Files cut or copied in a file manager paste here the same way; a cut
there becomes a move here, once. Without XWayland the toolkit's clipboard
is used, which takes text only: other programs get the full paths as text,
and Paste here accepts text whose every line is an absolute path or a
`file://` URI, as a copy.

File → Export results… (`Ctrl+S`) saves the listed results, in the shown
order, through the desktop's save dialog. A name ending in `.csv` (or the
CSV file type) gives a CSV file with the columns `Name`, `Folder`, `Size`
(bytes; for a folder the total of the files below it), `Modified` (local
time, `YYYY-MM-DD HH:MM:SS`) and `Offline` (`yes` for an entry of a drive
that is not connected); `.txt` gives one full path per line, where an
offline entry is followed by a tab and `(offline)`, and a backslash, line
break or tab in a path is written as `\\`, `\n`, `\r` or `\t`. `.tsv` gives
the CSV columns separated by tabs, with the same escapes in the name and the
folder; `.m3u8` a UTF-8 playlist (`#EXTM3U`, then one full path per line; a
path with a line break is left out); `.json` an array of objects with
`name`, `folder`, `size`, `modified` (`null` when unknown) and `offline`;
`.html` a web page with a table of the CSV columns. When only part of the
list is selected, the save dialog has a check box "Export only the N
selected results" (not ticked at the start). The file is written to
a temporary file and renamed into place.

The menu bar (File, Edit, View, Search, Index, Tools, Help) holds the same actions,
File → Close window (`Ctrl+W`, as the close button: hide or quit as set),
the appearance choices (optional columns, sort by a column and direction,
folder size, density, theme, selection details, the menu bar itself), the
matching modes with their `Alt` keys, the filters and the search history.
Index → Rebuild index now scans the folders to index again from scratch.
Settings → Indexing has Scan now beside the rescan schedule for the same action.
Index → Pause index updates stops applying file changes (it is the "Watch for
changes" setting, the other way round) until it is turned off again, which
catches up with what changed meanwhile.
Tools → Settings… and Tools → Customize shortcuts… open the settings; Tools →
Open the settings file opens the file itself in the desktop's application
for it (a text editor), and what is saved there applies at once while
"Apply changes made to the settings file outside this window" is on. Help →
Search syntax (`F1`), Regular expressions and Command line options open the
help window, a short reference with examples on three pages; Help →
Keyboard shortcuts lists every key; Help → Copy troubleshooting information
puts a short report for a bug report on the clipboard (version, system,
desktop session, language and the numbers of the index; no paths or names); Help → About shows the version and the
license. The Edit actions show no key in the menu, because menu keys would
take `Ctrl+C`, `Delete` or `F2` away from the search field; their keys are in
the table above. With the menu bar hidden, the `⋯` button next to the filter
selector holds settings, search syntax, keyboard shortcuts, about and quit.

The status line shows badges for active matching modes and hidden-file filters.
Settings → Search → Show active matching modes in the status line turns them
off or on.
Badges describe the window toggles; per-term modifiers stay in the query.

Search → Previous filter / Next filter (`Alt+↑` / `Alt+↓`) follows the
filter selector order. Settings → Search → Wrap when moving to the next or
previous filter controls whether the ends wrap (on by default). With it off,
the selection stays at the first or last filter. Keys can be changed on the
Keyboard page.

Settings → Search → Match wildcards anywhere in the filename is off by
default, preserving whole-name matching. `wholename:` /
`nowholename:` override it for one wildcard term; `wholename:` on text without
wildcards matches the whole name exactly (`wholename:main.rs` finds `main.rs`, not
`main.rs.bak`). Changing the setting reruns the current
query; prefix/suffix anchors and quoted literal wildcards still work.

Settings → Search → Allow literal AND / OR / NOT operators enables the
standalone uppercase words as operators (off by default). Quote a word such
as `"OR"` to search for it literally; lowercase words and values such as
`case:OR` remain text. The symbolic operators keep working in both modes.

Settings → Search → Evaluate AND before OR switches to the conventional
precedence. It is off by default: `report pdf|main` keeps meaning
`report AND (pdf OR main)`. With it on, the query means
`(report AND pdf) OR main`. Explicit `<...>` groups override either order.

Search → Hidden files and Settings → Search offer Show / Hide / Only
hidden files (Show by default). `Ctrl+H` cycles these modes; each choice can
also get its own key in Keyboard settings. The choice is saved and applies
to the current search, including regex and empty queries, alongside the kind
filter. Dotfiles and all descendants of dot-prefixed folders count as hidden,
including hidden components in an indexed root path. No entries are removed
from the index and no rescan is needed.

The result context menu has Search in EveryGrep (`Ctrl+Alt+F`) and Search
path in EveryGrep (`Ctrl+Alt+P`). The first searches inside a folder or for a
file's literal name; the second searches its containing folder. Window
settings choose this window or a new window, whether folder searches include
subfolders, and filename substring or whole-name matching. The defaults use
this window, direct children and filename substrings. Name searches use the
current case setting; folder paths are case-sensitive. The current filter is
kept, and the whole-query regex mode is turned off for the generated query.
Names containing query operators, quotes or non-UTF-8 bytes stay literal.

Create shortcuts (`Ctrl+Alt+L`, or the result context menu) asks for a
folder and creates links to all selected items there. Window settings choose
symbolic links (default) or `.desktop` links, and numbered names (default) or
skipping when a name exists. Numbers go before the extension, as for pasted
copies (`report (2).pdf`); a desktop link keeps the whole name
(`report.pdf (2).desktop`). Existing files are never replaced. Desktop links
use `Type=Link` and `URL` from the [desktop entry specification](https://specifications.freedesktop.org/desktop-entry/latest/recognized-keys.html);
opening them depends on the file manager's support. No executable launcher
or shell command is generated.

The Window settings can make Rename open a dialog showing the containing
folder and the new name. Invalid names, names over 255 UTF-8 bytes and
existing targets are reported inside the dialog; a failed rename keeps it
open. The existing in-place editor remains the default. Both editors refuse
to replace a target that appears during the rename. On NFS, SMB and some
FUSE file systems, which cannot refuse that in the rename itself, a file
gets a new hard link and loses its old name instead; only a folder (or a
file where hard links are not supported) gets a check followed by a plain
rename there.

Search tabs use `Ctrl+T` (new), `Ctrl+W` (close), `Ctrl+Shift+K`
(duplicate), `Ctrl+Tab` / `Ctrl+Shift+Tab` (next/previous), and `Ctrl+1`…`Ctrl+9`
or `Ctrl+0` (tabs 1…10). `Ctrl+Shift+W` closes the window. All are rebindable.
The tab strip appears with two tabs; its × button or middle click closes a tab.
The Tabs menu and `Ctrl+Shift+L` list all tabs, including those beyond the strip.
Close multiple tabs opens a checklist. Left/right/other closure and the ten most
recently closed tabs share the menu; `Ctrl+Shift+T` reopens the latest one.
Closing the last tab clears only its query and keeps its options, sort and layout.
New tabs inherit the active search's
filter, matching options and column layout. Duplicate also keeps its search and
selection. Background tabs freeze and refresh when shown; pending text still
waits for Enter when search as you type is off.

Settings → Window includes opt-in preferences for opening searches in tabs,
switching to newly opened tabs and restoring the last window's tabs. All three
are off by default. Opening a context search in a new window follows the tab
preference, as do query-bearing launches handed to the running window. Explicit
New window commands continue to create windows.

Session restore saves only the last window's tabs, active tab and last ten
closed tabs in `session` beside `settings` (private mode 0600). Queries, filter
snapshots, tab matching toggles, sort and column layout are kept. Hidden-file
mode and the three parse policies always follow Settings. Row ids,
selection and navigation trails stay in memory. A bare startup restores only
when the index identity matches; an explicit search, `--new-window` or
`--new-instance` starts as requested. Session persistence also requires history
recording and disk storage. Turning any of these off deletes the session file;
no session is loaded or written while disabled. Clear history deletes the saved
session and recently closed tabs; Delete entry removes matching closed tabs.
Queries cleared/deleted while still visible in open tabs are redacted from later
session saves for the rest of that window's lifetime. Writes use a sibling lock and atomic replacement:
the first change is saved immediately, changes during the next 15 seconds are
coalesced, and exit saves the latest state. Malformed or newer files are
preserved and reported with their path and reset instructions in the status
line. Turning restore off and on explicitly resets the saved file. Reading is limited to 8 MiB and
1,000 tab records.

### Settings

Open the settings with `Ctrl+,`, Tools → Settings… (or the `⋯` menu when the
menu bar is hidden), or with `--settings` (`--settings=search` opens that page; given to
a later launch, it opens the settings in the running window). Changes apply at
once; each page has a "Reset to defaults" button. Without a settings file
the program behaves as before, except that folders show their total size.

| Page | Setting | Default |
|---|---|---|
| Indexing | folders to index (added through the desktop's folder chooser) | the folders of the saved index, or the home folder |
| | watch for changes (off: nothing is watched and the status line says so; on again catches up) | on |
| | rescan the folders: never, every N hours (1 to 168, counted from the last full scan, also across restarts: its time is kept in `last-full-scan` next to the index file; from the start when there is none) or every day at a time (`HH:MM`; a time passed while the computer slept runs at the next check); not while index updates are paused | never (24 hours, 03:00 when chosen) |
| | index file sizes and modification times (off: only names are read, which is faster; Size and Modified are empty for files, folders have no total size, `size:` and `modified:` find no files; changing it indexes again) | on |
| | use the exclude list (excluded folders and file filters) | on |
| | excluded folders, by path (folder chooser) or pattern; left out of the index with all they hold | none |
| | include only files / exclude files: pattern lists (see [Excluded folders and files](#excluded-folders-and-files)) | empty |
| | index removable drives automatically | on |
| | when a drive is removed: keep its entries, shown dimmed, or forget them | keep |
| | known drives, each with "Forget" and "Don't index this drive" | — |
| | "Rebuild index now" | — |
| | index file: the folder of `index.bin` (and `hashes.bin` and `last-full-scan` next to it), "Choose folder…" moves it there at once and removes the old file once the new one is written; drive indexes stay in the cache folder; not for a window started with `--root` or `--index-file` | the cache folder |
| | index details: files, folders, size of the files, folders to index, drives in the index (connected), memory, the index file and its size, the last full scan, folders not watched; counted when the settings open and with "Count again" | — |
| Search | search as you type (off: the search runs when Enter is pressed in the search field) | on |
| | a click into the search field selects its text (a second click places the cursor) | on |
| | clear the selection when the search changes (off: selected items stay selected while listed) | on |
| | filter and matching modes when EveryGrep starts | all, every mode off |
| | searches kept for `Alt+←`/`Alt+→` (0 turns the history off) | 200 |
| | record search history | on |
| | days to keep searches (0 = forever), with Clear now | 90 |
| | suggest earlier searches while typing (off: suggestions appear only when search as you type is off) | off |
| | keep the history after EveryGrep closes | off |
| | file contents (`content:`): largest file to search in MB (0 for any size) | 100 |
| | code page of files that are not UTF-8 (also for `content-codepage:`) | Windows-1254 |
| | search inside PDF files (with `pdftotext` from poppler; the page says when it is missing) | off |
| | hash cache of duplicate searches: largest size in MB (0 keeps none; applies at the next save), how much is in use, "Clear hash cache" | 512 |
| Appearance | language: same as the system, English, Azərbaycanca or Türkçe | same as the system |
| | theme: same as the desktop, light or dark | same as the desktop |
| | density of the result list: comfortable (28 px rows) or compact (22 px) | comfortable |
| | alternate row color: every second row slightly darker (lighter in the dark theme) | on |
| | size format: automatic, bytes, KB, MB or GB | automatic |
| | size units: 1 KB = 1024 bytes, 1 KiB = 1024 bytes, or 1 KB = 1000 bytes | 1 KB = 1024 bytes |
| | details of the selected items in the status line | on |
| | show the menu bar | on |
| Window | closing the window: quit, or hide it and keep running | quit |
| | start EveryGrep when I log in | off |
| | remember the shown columns and their widths | on |
| | leave the extension unselected when renaming (off: the whole name is selected) | on |
| | a double-click in the Folder column opens the containing folder (off: the item) | on |
| | copy full path adds quotes (each path in double quotes) | off |
| | ask before opening more items than this at once (0 never asks) | 32 |
| | focus the search field and select its text when the window is shown again | on |
| | sort dates newest first / sort sizes largest first: the direction of the first click on a date or Size header | off |
| | what the Size column shows for folders: total size, number of items, or nothing | total size |
| Keyboard | the keys of the window's commands (see below), and how to open EveryGrep with a key | the keys listed above |
| Advanced | the settings file: "Export settings…" writes it where you choose; "Import settings…" replaces every setting with those of a chosen file (after a question; a file that sets nothing known is refused); "Reset all settings…" puts every setting back to its default (after a question); folders, rules and file details are taken only by the window over the default index, and changing them indexes again; column widths of an import apply at the next start | — |
| | all settings: a table of every key with its value and default ("Search settings…", "Only changed settings"); choose a row to edit its value in the file's form, toggle a `true`/`false`, reset it or copy its line; a change applies and is saved at once, and under "Only changed settings" a setting changed back to its default stays listed (and chosen) until the filter changes; list keys (`root`, `exclude`, `ignore-drive`, `shortcut`) are shown but changed on their pages | — |
| | apply changes made to the settings file outside this window (looked at every 2 seconds, after the window's own saves; applied as an import, with a note in the status line) | on |

On the Keyboard page, "Show commands containing:" filters the commands by
name, menu or key (case and diacritics ignored). Choose a command to see its
keys, each with where it works: Global (anywhere in the window), Search field
or Result list. Add… and Edit… open a dialog: choose where the key works
("Use shortcut in"), press the new key, and see which commands have it
already. OK takes it from the commands with the same key in the same place;
a global key and a key of the search field or the list may share a key (the
focused control's key wins), which the dialog notes. Keys that would type
into the search field (no `Ctrl`, `Alt` or `Super`), `Tab`, `Esc`, movement
keys of the list and the search field's editing keys (`Ctrl+A`, `Ctrl+C`,
`Ctrl+V`, `Home`, `Delete`, …) are refused with a note. Remove takes one
key away; "Reset to defaults" gives every command its original keys.
Movement keys, `Tab` and `Esc` cannot be changed. `--settings=shortcut` still opens this page.

"Same as the desktop" follows the desktop's light or dark style (the
`color-scheme` of the XDG settings portal) and recolors the window when it
changes. With one item selected, the status line shows its size (for a folder
as the folder size setting says), modification time and full path; with
several, their number and the total size of the files among them. The menu
bar is new and on by default; turning it off brings back the earlier look.

Changing the folders updates the index in the background while the window
stays usable. Added coverage is scanned and merged; removed coverage is dropped,
with nested roots retained. Narrower include/exclude lists re-filter the saved
entries. Removing an exact folder exclusion reads its parent and newly admitted
tree. Broader or unproven pattern changes, changed file details, or an index
without reliable live watches require a full scan (folders that cannot be read
do not count; when folders are not watched, for example past the watch limit,
the status line says why the change scans in full). After the saved index was
loaded, file sizes and times are read again for the reused files. The status line shows progress
and Cancel; cancellation keeps the old index and settings. The replacement is
saved as soon as it is complete. Explicit and scheduled rescans still read all
folders.

### Excluded folders and files

What is left out of the index comes from three lists, used only while "Use
the exclude list" is on:

- **Excluded folders**: an absolute path, matched exactly, or a pattern
  (`node_modules`, `*.git`, `/srv/**/tmp`, `regex:^te?mp$`). An excluded
  folder is left out with its own entry and all that is below it.
- **Include only files**: when not empty, a file is indexed only if one of
  these patterns matches it (`*.mp3;*.jpg`).
- **Exclude files**: a file that matches one of these is not indexed
  (`*.tmp;~*`).

A file is indexed when the include list is empty or matches it, and the
exclude list does not; an exclude always wins. Folders are tested only
against the folder list. Items are separated by `;` or `,`; double quotes
keep a separator inside an item. In a wildcard pattern `*` matches anything
but `/`, `**` anything, and `?` one character but `/`; it must match the
whole name, ignoring case. A pattern with a `/` is matched against the full
path instead: from the start when it begins with `/`, otherwise against the
end of the path after a `/` (`docs/*.txt`). `regex:` starts a regular
expression, searched for in the name (in the full path when it contains a
`/`), with case as written (`(?i)` ignores it). A pattern that does not
compile is refused with a message and not saved. The folders to index
themselves are always indexed. Changing any list or the switch indexes
again; the same rules apply while watching for changes and when catching up.
The saved index (and each drive's) records these rules: one saved with other
rules, or by an older version without them, is built again at the next start
(a drive when it is next connected). Changing the rules also drops the
entries of every drive read with the old ones; a connected drive is read
again at once.

Long operations show their progress in the status line once they take
more than 300 ms: indexing, rescanning, reading a drive, catching up,
searching and sorting ("Indexing: 42%"). The percentage of a scan is
estimated from the used inodes of the file systems it reads, or from the
size of the previous index of the same folders; file systems that report
no inodes (btrfs, FAT) give a count instead ("Indexing: 123 456 items").
The Cancel button next to it stops the operation and keeps what was there
before: a cancelled first index is not saved (the next start, or "Rebuild
index now", indexes again), a cancelled rescan keeps the previous index,
folders and rules (the settings go back to them; a newer change of the
folders or rules cancels the older rescan without a message), a cancelled sort or search keeps the results and order shown (a content
search shows its matches while it reads files, about once a second, and a
cancel keeps the matches found so far; a duplicate search shows "Comparing
files: 42%" while it reads files and its results only at the end), and
a drive whose first reading was cancelled goes on where it stopped when it
is plugged in again (also after it was removed while being read, or the
program quit while it was read: quitting cancels the reading and waits up to
5 seconds for what was read to be saved). Catching up cannot be cancelled;
quitting during a long catch-up waits at most those 5 seconds too, and an
index it could not save then is caught up again at the next start. Cancel has no key yet. Hidden files are always indexed.

A folder's total size is the sum of the sizes of all files below it, at any
depth, and is kept up to date as files change. Each file counts with its own
size: a file with several hard links counts once per link, a symbolic link
counts with the length of its target and is never followed. Sorting by Size puts folders among the files by their
total; with "number of items" folders sort by that number, before the files
(after them when the order is reversed); with "nothing" they sort as empty,
as before. Folder sizes are the one setting whose default changes the
earlier look: until they existed the Size of a folder was empty, which
"nothing" brings back.

When closing only hides the window, EveryGrep keeps the index in memory and
keeps watching; starting `everygrep-gui` again (for example with its
shortcut) shows the window at once through the hand-over described below. No
tray icon is needed. `Ctrl+Q` or Quit in the `⋯` menu ends the program.

"Start EveryGrep when I log in" writes the autostart entry
`$XDG_CONFIG_HOME/autostart/az.huseynz.EveryGrep.desktop` (by default under
`~/.config`) and removes it when turned off; the switch shows whether the
entry exists and is not turned off inside (`Hidden=true` or
`X-GNOME-Autostart-enabled=false`, as other tools may write). The entry runs this program by its full path with
`--background`: when closing only hides the window, EveryGrep then loads the
index and watches without showing a window, and the next launch shows it at
once. When closing quits, a window without a way back would make no sense,
so the window opens on login. `--background` does nothing when EveryGrep
already runs.

Options on the command line take the place of the saved ones for that start.
The folders on the Indexing page belong to the default index: a window
started with `--root` or `--index-file` shows its own folders, and changes
made there apply to that window only and are never saved.

The settings live in `$XDG_CONFIG_HOME/everygrep/settings` (by default
`~/.config/everygrep/settings`), a text file with one `key = value` per line
and only the settings that differ from the defaults:

```text
root = /home/me
root = /srv/media
exclude = /home/me/.cache
filter = document
match-case = true
history-size = 50
history-on-disk = true
on-close = hide
show-type-column = true
size-width = 120
```

`root` and `exclude` repeat for each folder (an `exclude` may be a pattern).
`content-max-size-mb`, `content-code-page` (`windows-1250` … `windows-1257`)
and `content-pdf` set how files are read for `content:`; `hash-cache-max-mb`
the size of the hash cache.
`index-folder` holds the folder of the index file, `reload-changed-settings`
(`false`) ignores changes made to the file outside the window until the next start,
`watch-changes` turns watching off (`false`), `index-file-details` (`false`)
indexes names only, `rescan` (`never`, `hours`, `daily`) with `rescan-hours`
and `rescan-time` (`HH:MM`) scans the folders again by themselves,
`search-as-you-type` the
search at every key press, and `rename-in-dialog` chooses the rename editor.
`history-enabled` (`false`) stops recording searches, `history-days` keeps them
that many days (`0` for ever; 90 by default), `history-sort` (`date`, `count`,
`text`) orders the history list, and `always-search-suggestions` shows
suggestions while searching as you type too. `open-searches-in-tabs`,
`switch-to-new-tab` and `restore-session` hold the tab choices of the Window
page.
`context-search-new-window`, `context-search-subfolders` and
`context-search-exact-name` control context searches.
`shortcut-kind` (`symlink` or `desktop`) and `shortcut-collision` (`number`
or `skip`) control shortcuts. `rename-select-name-only` controls selection of
the name without its extension when renaming,
`open-folder-on-path-double-click` what a double-click in the Folder column
opens, `copy-full-path-quotes` the quotes around copied full paths,
`type-to-jump` typing in the result list to jump to a name (on),
`confirm-open-over` the number of items opened at once without a question, `focus-search-on-show`
whether showing the window again focuses the search field.
`use-exclude-list`, `include-only-files` and `exclude-files` hold the switch
and the file pattern lists; a pattern that does not compile is reported and
ignored. Other keys: `whole-words`,
`match-path`, `match-diacritics`, `match-prefix`, `match-suffix`,
`ignore-punctuation`, `ignore-whitespace`, `regex`, `select-search-on-click`,
`deselect-on-new-search`,
`index-removable-drives`,
`removed-drives` (`keep`, `forget`), `ignore-drive` (a file system UUID,
repeated per drive), `folder-size` (`total`, `items`, `none`),
`theme` (`system`, `light`, `dark`), `density` (`comfortable`, `compact`),
`size-format` (`auto`, `bytes`, `kb`, `mb`, `gb`), `size-units` (`jedec`
for 1 KB = 1024 bytes, `iec` for KiB, `metric` for 1 KB = 1000 bytes),
`language` (`system`, `en`, `az`, `tr`),
`status-details`, `menu-bar`, `alternate-row-color`, `remember-columns`, `sort-dates-newest-first`,
`sort-sizes-largest-first`,
`show-extension-column`, `show-created-column`, `show-accessed-column`,
`show-attributes-column`, `show-md5-column`, `show-sha256-column`,
`show-crc32-column`, `show-same-name-column`,
`show-same-extension-column`, `show-same-size-column`, `name-width`, `folder-width`, `modified-width`, `type-width`,
`extension-width`, `created-width`, `accessed-width`, `attributes-width`,
`md5-width`, `sha256-width`, `crc32-width`, `same-name-width`,
`same-extension-width`, `same-size-width`.
`shortcut` gives one command its own keys in place of the defaults, written
by the Keyboard settings page: `shortcut = edit.copy list Ctrl+C; global
Ctrl+Insert` (the scope `global`, `search` or `list`, then the keys), or
`shortcut = file.export-results` for none; a line with an unknown command or
keys that cannot be read is reported and kept as it is. Values use backslash escapes (`\\`, `\n`, `\t`, `\s` for a
space at either end, `\xHH` for a byte that is not UTF-8). Unknown keys are kept,
lines that cannot be read are ignored (the settings window says how many),
and the file is replaced atomically. The search history, when kept, is in
`search-history` in the same folder, with distinct queries, counts and last search
dates. It is updated atomically under `search-history.lock`; the earlier `history`
file is imported once and left untouched until history is explicitly cleared
or disk persistence is turned off. At most 10,000 distinct searches are
kept, dropping the oldest first. Apply confirms a changed retention period. Recording can be disabled without deleting
existing entries; Clear now clears the saved and session histories. The search
field’s ▼ button (`F4`) lists searches, sorted by date, count or text. While
typing, optional prefix suggestions show at most 12 matches; use Up/Down and
Enter to choose, or Esc to dismiss them. History → Back/Forward uses the
session navigation trail. CSV exports prefix formula starters (`=`, `+`, `-`, `@`)
with an apostrophe and double existing leading apostrophes; import reverses
this convention. Blank CSV records are ignored. History → Organize search history (`Ctrl+Shift+H`)
opens a filterable table with search text, visit count and last local date.
Delete removes the selected search; Delete all asks before clearing the full
history. Import/Export CSV uses the desktop file chooser; export includes all
entries regardless of the table filter. CSV headers are
`query,times_used,last_used_utc`, with UTC timestamps such as
`2026-11-02T12:30:00Z`. Imports merge the greater count and later date, so
repeated imports are safe; expired imported entries are removed immediately.

### Removable drives

Drives that the desktop mounts when they are plugged in (GNOME's disk
service, udisks, mounts them under `/run/media/$USER`, some systems under
`/media/$USER`) are indexed automatically, each into a file of its own. A
drive is recognised by the UUID of its file system, so it keeps its entries
when it is mounted somewhere else the next time. Nothing else is indexed
because it was mounted: disks the system mounts, such as those in
`/etc/fstab` under `/mnt`, are indexed only when you add them as folders.

When a drive is removed (unmounted), its entries stay in the results,
drawn dimmed and slanted; the status line says "Offline: the drive is not
connected" for a selected one, and opening, open with, the containing folder, cut,
copy, paste, rename, trash and delete are refused for them (copying their names and paths
works). Plugged in again, the drive is caught up by reading only the folders
whose time changed, then watched like any other folder. Settings →
Indexing can forget a drive's entries instead when it is removed, and
lists the known drives (label, UUID, connected or not, number of entries,
last seen) with "Forget" (for a drive that is not connected) and "Don't
index this drive" / "Index this drive". A drive whose mount point lies in
a folder to index, or holds one, is left to that folder.

A folder the window cannot watch, because the limit of inotify watches
(`fs.inotify.max_user_watches`, shared by all programs of the user) is
reached, is tried again every 10 seconds and in the meantime checked for
changes by its modification time; the status line then says "N folders not
watched", with "watch limit reached" when that is the cause. Raising the
limit (for example `sysctl fs.inotify.max_user_watches=1048576`) removes
it. Folders that cannot be read at all are not counted: nothing in them is
indexed. When a file system mounted inside the indexed folders (not a
removable drive) is unmounted, its folder is read again as it is now.
When no inotify instance can be made at all (other programs hold all of
them; this can happen after lost events, when the index is built again),
the status line says "not watching changes" and a watcher is tried again
every 10 seconds; what changed meanwhile is caught up once it works.

EveryGrep notices mounts and unmounts through the kernel's mount table
(`/proc/self/mountinfo`), which wakes it only when something changes; it
costs nothing while idle. The command line does not search drives yet.

### One window

Only one window runs per index. Launching `everygrep-gui` again brings the open
window to the front with the search field focused; query words, `--filter`,
the matching mode flags and `--columns` given to the new launch are applied to
that window (a launch without them leaves the search as it is), and the new
launch exits at once. Query words are kept in the search history, so
`Alt+←` returns to the previous search.

A launch with other `--root` folders or another `--index-file` than the open
window opens a window of its own, which in turn takes later launches for the
same roots and index file. `--new-instance` always opens a separate window.
`--new-window` does too, but it takes over the name when no window runs, so
later launches find it. `--toggle` shows the window like a plain launch,
except that a window already in front (the search field or the list has
the keyboard focus) is closed the way its close button closes it: hidden
when closing hides, otherwise the program quits.
The windows find each other on the D-Bus session bus (name
`az.huseynz.EveryGrep`); without a session bus every launch opens a window.
A launch that finds the open window still shutting down, or not answering
(its window does not take the request within 5 seconds), waits for it with a small dialog that offers to terminate it, and opens the
window as soon as the old one has ended.

To open the window with a key, add a GNOME shortcut: Settings → Keyboard →
View and Customize Shortcuts → Custom Shortcuts → `+`, name `EveryGrep`,
command `everygrep-gui` (show), `everygrep-gui --toggle` (show or hide) or
`everygrep-gui --new-window` (new window), then press the keys you want; one
shortcut per command. The Keyboard page of the settings shows the
same steps and copies each command. EveryGrep does not grab keys itself: the
desktop runs the command, so the keys work even when EveryGrep is not
running.

## Command line

```text
everygrep index [options]            scan and save the index
everygrep search [options] <query>   print matching paths
everygrep shell [options]            read one query per line
```

`search` and `shell` load the saved index; they scan and save it first when
there is none, when roots or include/exclude rules differ from the saved
ones, when `--no-meta` is given for an index with sizes and times or left
out for one without them, when an older cache has unknown rules, or with
`--rescan`. The command
line keeps its own index, `~/.cache/everygrep/cli-index.bin`, apart from the
window's `index.bin`, so neither replaces the other's. Paths are printed in
name order, one per line, at most `--limit` (50 by default, `0` for all);
counts and timings go to standard error. `shell --watch` applies file changes
before every query and saves the index on exit. Both the shell and the window
catch up after installing watches, including after a fresh scan or rescan.

`--exclude`, `--include-files` and `--exclude-files` take the excluded
folders and file patterns described under
[Excluded folders and files](#excluded-folders-and-files). Other options:
`--root`, `--index-file`, `--case`, `--whole-words`, `--match-path`,
`--diacritics`, `--prefix`, `--suffix`, `--ignore-punctuation`,
`--ignore-whitespace`, `--regex`, `--filter`, `--threads`, `--no-meta`;
`everygrep` without arguments prints them all. The command line is English
only. Path arguments preserve non-UTF-8 filenames; queries and textual option
values must be valid UTF-8. `--no-meta` omits file sizes and timestamps during
scans, catch-up and watching; folder timestamps are retained for catch-up.

When the index cannot be saved, `everygrep index` returns a nonzero exit
status; `search` and `shell` print a warning and still answer. A completed
save syncs the file and, after rename, its parent directory (where the file
system supports syncing a directory).

Failed or partial directory listings remain eligible for catch-up even when
permissions change without changing the directory mtime. In a folder that
can be listed but not searched, catch-up and the watcher still add and
remove entries by name, without sizes or times until they can look at them;
the watcher lists such a folder again on its next event (a permission change
included), and its time is not saved until then. Roots absent during
a scan are retried when the saved index is loaded.

## Languages

The window is available in English, Azerbaijani (Azərbaycanca) and Turkish
(Türkçe). By default it follows the session's language, read like gettext
does: `LC_ALL`, `LC_MESSAGES`, then `LANG` give the locale, and unless that is
`C` or `POSIX`, the list in `LANGUAGE` comes first; other languages fall back
to English. Settings → Appearance → Language picks one instead and applies at
once. The command line and its messages stay in English. Adding a language
is described in [CONTRIBUTING.md](CONTRIBUTING.md#translations).

## Privacy

All data stays on this computer. EveryGrep makes no network connections and
collects nothing. It stores:

- the index, in `~/.cache/everygrep/index.bin` (or the folder chosen in
  Settings → Indexing → Index file) for the window and
  `~/.cache/everygrep/cli-index.bin` for the command line (names, sizes and
  modification times of the indexed files, never their contents), and for
  each removable drive `~/.cache/everygrep/volumes/<uuid>.bin` with the same
  kind of data, `<uuid>.partial` with what a cancelled or interrupted first
  reading read,
  and `<uuid>.info` with its label, last mount point, when it was last
  connected and its number of entries;
- hashes of files that a duplicate search compared, in
  `~/.cache/everygrep/hashes.bin` next to the window's index (device and
  inode numbers, sizes, modification times and hashes, never names or
  paths), at most 512 MB (Settings → Search → Hash cache, with a button
  that empties it);
- the settings, in `~/.config/everygrep/settings` (and an empty
  `settings.lock` next to it, which windows lock while they save);
- the search history, in `~/.config/everygrep/search-history`, only when "Keep the
  history after EveryGrep closes" is on (it is off by default).

It talks to other programs only on this computer: the session bus (single
window, desktop portals for the folder chooser, the save dialog of the
export, the application chooser of "open with" and the light or dark style,
the file manager), the XWayland display for files on the clipboard, `xdg-open` to open files and `gio trash` to move them to
the trash.

## Building

```sh
cargo build --release --locked
./target/release/everygrep --version
./target/release/everygrep-gui --version
```

Requires Rust 1.85 or newer. The window needs the usual libraries of a
Wayland desktop at run time (fontconfig, freetype, libxkbcommon, Wayland and
EGL through libglvnd).

## Documentation

- [docs/query-language.md](docs/query-language.md): the query language
- [docs/architecture.md](docs/architecture.md): crates, index, watcher,
  threads, single instance
- [docs/index-format.md](docs/index-format.md): the saved index file
- [docs/design.md](docs/design.md): UI design guidelines
- [docs/icons.md](docs/icons.md): icons and the app id
- [CONTRIBUTING.md](CONTRIBUTING.md): building, checks, commits, translations
- [CHANGELOG.md](CHANGELOG.md)

## License

EveryGrep is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

The crates EveryGrep is built from, with their licenses, are listed in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md)
(regenerate it with `python3 -I scripts/third-party-licenses.py`).

The window is built with [Slint](https://slint.dev), used under its
royalty-free license, which asks for its attribution in an about dialog
reachable from the top-level menu: Help → About EveryGrep (or `⋯` → About
when the menu bar is hidden).

Search → Filter → Save as filter saves the field text and its matching modes.
Manage filters adds, edits, removes and reorders the entries used by the same
selector and menu. Alt+Up/Down moves the selected entry in the manager.
A filter is intersected with the field query using its own matching modes.
Restore default filters asks first and keeps user filters. The private
`saved-searches` file beside settings is updated atomically under a lock;
invalid files are preserved and reported. Hidden-files mode and all three parse
options are visible in the editor; new filters start with Show hidden files.
Delete asks for confirmation. Invalid queries remain visible for editing; a
structurally unreadable file offers a confirmed reset after saving a private copy.
Built-in requests use an original snapshot with a notice if that built-in was
edited or removed.

Bookmarks → Save bookmark (Ctrl+D) stores the query, matching options, filter
and sort column/direction. Manage bookmarks renames, edits, deletes and reorders
entries (Alt+Up/Down); Open restores one search. A bookmark keeps a snapshot
of its filter. If that filter was changed or removed, its saved version appears
in the existing selector for this window. Opening a bookmark reveals a hidden
sort column when necessary. After using saved query parse options, the next
manual edit restores the policy shown in Search settings.

Filters and bookmarks accept an optional global keyboard shortcut in their
editor by pressing the key combination in the active keyboard layout; Clear
shortcut removes it. Conflicts with another
saved item or any active/default command key are refused. Keyboard settings
also refuses keys owned by saved items. Item keys appear in menus and keyboard
help; the organizer's Alt+Up/Down remains local to the organizer.

Search → Advanced Search (Ctrl+Shift+F) builds the existing query syntax from
all words, an exact phrase, any words and excluded words, in names or paths.
It offers a location with optional subfolders, the current filter, files or
folders, extensions, size and modified/created/accessed date ranges. Sizes use
bytes or binary KiB/MiB/GiB; dates are local `YYYY-MM-DD` and both ends are
inclusive. Empty ends impose no limit. Use query (or Enter in a text field) replaces the field and follows
Search as you type; with that setting off, press Enter to run it. Unsupported
metadata conditions are omitted.

Its File contents section adds one `content:` term: a word or phrase found on
one line of the file, with its own Match case, Whole words and Regular
expression switches (the ones above apply to names), and the text encoding:
Automatic (`content:`), the code page from Settings (`content-codepage:`), UTF-8
(`content-utf8:`), UTF-16 LE (`content-utf16le:`) or UTF-16 BE (`content-utf16be:`).
The text is quoted, so operators and modifiers in it are text to find.

Advanced Search rejects extension lists combined with Folders or Files without
an extension. Its form and the saved-item editor scroll in short work areas.
