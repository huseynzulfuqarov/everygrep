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
              [--settings[=indexing|search|appearance|window|keyboard]] [query words...]
```

Words after the options become the first query; it runs as soon as the index is
loaded. `everygrep-gui --help` lists the options; a mistyped option or value
stops with an error instead of opening the window. Type to search; `↓` moves into the results. The selector next to the
search field limits results to one kind (folders, documents, pictures, …); the
toggles inside the field switch match case, whole words, full path,
diacritics and regular expressions; the Search menu also has match prefix,
match suffix, ignore punctuation and ignore whitespace; the modes that are on
show in the status line. Results come in natural name order: numbers by value
(`file2` before `file10`), case and diacritics ignored (`Çay` next to `cay`),
`ə` after `e`, hyphens and apostrophes skipped. Right-click the
column headers to show the Type, Extension, Created, Accessed and Attributes
columns. Double-click a header border to size the column on its left to fit
the rows on screen; View → Size all columns to fit (the spare room goes to
Folder) and Fit columns to window (every column scaled to fill the
window) do it for all shown columns, also from the header's context menu. Created, Accessed and Attributes are not in the index: they are read
from the file system for the rows on screen while their column is shown, and
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
| — | copy path / full path with quotes: each in double quotes (context menu, Edit menu; a key can be set) |
| `F2` | rename in place (`Enter` renames, `Esc` cancels) |
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
| `Ctrl+S` | export the results to a CSV or text file |
| `F5` | refresh: run the search again and read the Created, Accessed and Attributes values afresh |
| `Ctrl+N` | new window over the same index (also with the menu bar hidden) |
| `Ctrl+W` | close the window, as its close button does |
| `Ctrl+,` | settings |
| `Ctrl+Q` | quit (also when closing only hides the window) |

Right-click a row, or press `Menu` or `Shift+F10`, for a context menu with the
same actions. The index follows these changes by itself.

Cut or copy items, select one folder in the results and paste (`Ctrl+V`,
or Paste into folder in the menus) to copy or move them there. Nothing is
ever replaced: when a name is taken, the item gets the first free name like
`report (2).pdf`. A copy is written under a temporary name and renamed into
place only when it is complete, so a failure or a cancel leaves nothing
half-copied. A move is a rename on the same file system; across file
systems the items are copied first and only what was copied is removed
afterwards, so a failed move never loses the original. Links are copied as
links, and special files (pipes, sockets, devices) are left out and named
in the status line. A paste that takes longer shows its progress in the
status line with a Cancel button; items already pasted stay. A folder is
never pasted into itself.

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
offline entry is followed by a tab and `(offline)`. The file is written to
a temporary file and renamed into place.

The menu bar (File, Edit, View, Search, Tools, Help) holds the same actions,
File → Close window (`Ctrl+W`, as the close button: hide or quit as set),
the appearance choices (optional columns, sort by a column and direction,
folder size, density, theme, selection details, the menu bar itself), the
matching modes with their `Alt` keys, the filters and the search history.
Tools → Settings… and Tools → Customize shortcuts… open the settings. Help →
Keyboard shortcuts lists every key; Help → About shows the version and the
license. The Edit actions show no key in the menu, because menu keys would
take `Ctrl+C`, `Delete` or `F2` away from the search field; their keys are in
the table above. With the menu bar hidden, the `⋯` button next to the filter
selector holds settings, keyboard shortcuts, about and quit.

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
| | use the exclude list (excluded folders and file filters) | on |
| | excluded folders, by path (folder chooser) or pattern; left out of the index with all they hold | none |
| | include only files / exclude files: pattern lists (see [Excluded folders and files](#excluded-folders-and-files)) | empty |
| | index removable drives automatically | on |
| | when a drive is removed: keep its entries, shown dimmed, or forget them | keep |
| | known drives, each with "Forget" and "Don't index this drive" | — |
| | "Rebuild index now" | — |
| Search | search as you type (off: the search runs when Enter is pressed in the search field) | on |
| | a click into the search field selects its text (a second click places the cursor) | on |
| | clear the selection when the search changes (off: selected items stay selected while listed) | on |
| | filter and matching modes when EveryGrep starts | all, every mode off |
| | searches kept for `Alt+←`/`Alt+→` (0 turns the history off) | 200 |
| | keep the history after EveryGrep closes | off |
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

Changing the folders rebuilds the index in the background while the window
stays usable; the status line shows the progress, and the new index is saved
as soon as it is complete.

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
(a drive when it is next connected).

Long operations show their progress in the status line once they take
more than 300 ms: indexing, rescanning, reading a drive, catching up,
searching and sorting ("Indexing: 42%"). The percentage of a scan is
estimated from the used inodes of the file systems it reads, or from the
size of the previous index of the same folders; file systems that report
no inodes (btrfs, FAT) give a count instead ("Indexing: 123 456 items").
The Cancel button next to it stops the operation and keeps what was there
before: a cancelled first index is not saved (the next start, or "Rebuild
index now", indexes again), a cancelled rescan keeps the previous index and
folders, a cancelled sort or search keeps the results and order shown, and
a drive whose first reading was cancelled is read again when it is plugged
in again. Catching up cannot be cancelled. Cancel has no key yet. Hidden files are always indexed.

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
entry exists. The entry runs this program by its full path with
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
`watch-changes` turns watching off (`false`), `search-as-you-type` the
search at every key press, `rename-select-name-only` the selection of
the name without its extension when renaming,
`open-folder-on-path-double-click` what a double-click in the Folder column
opens, `copy-full-path-quotes` the quotes around copied full paths,
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
`show-attributes-column`, `name-width`, `modified-width`, `type-width`,
`extension-width`, `created-width`, `accessed-width`, `attributes-width`.
`shortcut` gives one command its own keys in place of the defaults, written
by the Keyboard settings page: `shortcut = edit.copy list Ctrl+C; global
Ctrl+Insert` (the scope `global`, `search` or `list`, then the keys), or
`shortcut = file.export-results` for none; a line with an unknown command or
keys that cannot be read is reported and kept as it is. Values use backslash escapes (`\\`, `\n`, `\t`, `\s` for a
space at either end, `\xHH` for a byte that is not UTF-8). Unknown keys are kept,
lines that cannot be read are ignored (the settings window says how many),
and the file is replaced atomically. The search history, when kept, is in
`history` in the same folder, one query per line.

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
A launch that finds the open window still shutting down, or not answering,
waits for it with a small dialog that offers to terminate it, and opens the
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
ones, when an older cache has unknown rules, or with `--rescan`. The command
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
can be listed but not searched, catch-up still adds and removes entries by
name, without sizes or times until it can look at them. Roots absent during
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

- the index, in `~/.cache/everygrep/index.bin` for the window and
  `~/.cache/everygrep/cli-index.bin` for the command line (names, sizes and
  modification times of the indexed files, never their contents), and for
  each removable drive `~/.cache/everygrep/volumes/<uuid>.bin` with the same
  kind of data and `<uuid>.info` with its label, last mount point, when it
  was last connected and its number of entries;
- the settings, in `~/.config/everygrep/settings`;
- the search history, in `~/.config/everygrep/history`, only when "Keep the
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
