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
- **Keyboard first.** Every action has a key; open, open the containing
  folder, copy names or paths, rename in place, trash or delete.
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
| `*.pdf`, `img_??.png` | wildcards, matched against the whole name |
| `case:Readme` | match case for this term |
| `wholeword:art` | whole words only: `pop-art.png`, not `party.png` |
| `diacritics:café` | match diacritics for this term |
| `path:src/main`, `docs/report` | match the full path (any term with `/` does) |
| `file:`, `folder:` | only files / only folders |
| `ext:pdf;jpg` | extensions |
| `size:>10mb`, `size:1kb..2mb`, `size:0` | size (1 kb = 1024 bytes); folders match by their total size |
| `modified:<7d`, `modified:>30d` | changed within / not within a time (`s`, `m`, `h`, `d`, `w`) |
| `modified:2026-10-25`, `modified:2026-10`, `modified:today` | changed on that local day, month or year |
| `modified:>2026-10-25`, `modified:2026-10-01..2026-10-25` | after a day; ranges are inclusive |

The full syntax, with every modifier and edge case, is in
[docs/query-language.md](docs/query-language.md).

## The window

```sh
everygrep-gui [--root <folder>]... [--index-file <file>] [--filter <name>]
              [--case] [--whole-words] [--match-path] [--diacritics]
              [--columns type,extension] [--new-instance]
              [--settings[=indexing|search|appearance|window|shortcut]] [query words...]
```

Words after the options become the first query; it runs as soon as the index is
loaded. `everygrep-gui --help` lists the options; a mistyped option or value
stops with an error instead of opening the window. Type to search; `↓` moves into the results. The selector next to the
search field limits results to one kind (folders, documents, pictures, …); the
toggles inside the field switch match case, whole words, full path and
diacritics, and the modes that are on show in the status line. Right-click the
column headers to show the Type and Extension columns.

| Keys | Action |
|---|---|
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | move and select |
| `Shift` + movement, `Shift`+click | extend the selection |
| `Ctrl+Space`, `Ctrl`+click | add or remove one row |
| `Ctrl+A` | select all |
| `Enter`, double-click | open |
| `Ctrl+Enter` | open the containing folder |
| `Ctrl+C` / `Ctrl+Shift+C` | copy the name / the full path (one per line) |
| `F2` | rename in place (`Enter` renames, `Esc` cancels) |
| `Delete` | move to the trash (`gio trash`) |
| `Shift+Delete` | delete permanently, after a confirmation |
| `Esc` | clear the selection, then back to the search |
| `Ctrl+F`, `Ctrl+L` | focus the search field |
| `Alt+←` / `Alt+→` | back / forward through earlier searches |
| `Alt+C`, `Alt+W`, `Alt+P`, `Alt+D` | match case, whole words, full path, diacritics |
| `Alt+↑` / `Alt+↓` | previous / next filter |
| `Ctrl+N` | new window over the same index |
| `Ctrl+,` | settings |
| `Ctrl+Q` | quit (also when closing only hides the window) |

Right-click a row, or press `Menu` or `Shift+F10`, for a context menu with the
same actions. The index follows these changes by itself.

The menu bar (File, Edit, View, Search, Help) holds the same actions, the
appearance choices (optional columns, folder size, density, theme, selection
details, the menu bar itself), the matching modes with their `Alt` keys, the
filters and the search history. Help → Keyboard shortcuts lists every key;
Help → About shows the version and the license. The Edit actions show no
key in the menu, because menu keys would take `Ctrl+C`, `Delete` or `F2` away
from the search field; their keys are in the table above. With the menu bar
hidden, the `⋯` button next to the filter selector holds settings, keyboard
shortcuts, about and quit.

### Settings

Open the settings with `Ctrl+,`, File → Settings (or the `⋯` menu when the
menu bar is hidden), or with `--settings` (`--settings=search` opens that page; given to
a later launch, it opens the settings in the running window). Changes apply at
once; each page has a "Reset to defaults" button. Without a settings file
the program behaves as before, except that folders show their total size.

| Page | Setting | Default |
|---|---|---|
| Indexing | folders to index (added through the desktop's folder chooser) | the folders of the saved index, or the home folder |
| | excluded folders: listed, but not searched inside | none |
| | "Rebuild index now" | — |
| Search | filter and matching modes when EveryGrep starts | all, every mode off |
| | searches kept for `Alt+←`/`Alt+→` (0 turns the history off) | 200 |
| | keep the history after EveryGrep closes | off |
| Appearance | language: same as the system, English, Azərbaycanca or Türkçe | same as the system |
| | theme: same as the desktop, light or dark | same as the desktop |
| | density of the result list: comfortable (28 px rows) or compact (22 px) | comfortable |
| | details of the selected items in the status line | on |
| | show the menu bar | on |
| Window | closing the window: quit, or hide it and keep running | quit |
| | remember the shown columns and their widths | on |
| | what the Size column shows for folders: total size, number of items, or nothing | total size |
| Keyboard shortcut | how to open EveryGrep with a key (see below) | — |

"Same as the desktop" follows the desktop's light or dark style (the
`color-scheme` of the XDG settings portal) and recolors the window when it
changes. With one item selected, the status line shows its size (for a folder
as the folder size setting says), modification time and full path; with
several, their number and the total size of the files among them. The menu
bar is new and on by default; turning it off brings back the earlier look.

Changing the folders rebuilds the index in the background while the window
stays usable; the status line shows the progress, and the new index is saved
as soon as it is complete. An excluded folder is matched exactly, and its own
entry stays in the results. Hidden files are always indexed.

A folder's total size is the sum of the sizes of all files below it, at any
depth, and is kept up to date as files change. Each file counts with its own
size: a file with several hard links counts once per link, a symbolic link
counts with the length of its target and is never followed, and excluded
folders count as empty. Sorting by Size puts folders among the files by their
total; with "number of items" folders sort by that number, before the files
(after them when the order is reversed); with "nothing" they sort as empty,
as before. Folder sizes are the one setting whose default changes the
earlier look: until they existed the Size of a folder was empty, which
"nothing" brings back.

When closing only hides the window, EveryGrep keeps the index in memory and
keeps watching; starting `everygrep-gui` again (for example with its
shortcut) shows the window at once through the hand-over described below. No
tray icon is needed. `Ctrl+Q` or Quit in the `⋯` menu ends the program.

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

`root` and `exclude` repeat for each folder. Other keys: `whole-words`,
`match-path`, `match-diacritics`, `folder-size` (`total`, `items`, `none`),
`theme` (`system`, `light`, `dark`), `density` (`comfortable`, `compact`),
`language` (`system`, `en`, `az`, `tr`),
`status-details`, `menu-bar`, `remember-columns`,
`show-extension-column`, `name-width`, `modified-width`, `type-width`,
`extension-width`. Values use backslash escapes (`\\`, `\n`, `\t`, `\s` for a
space at either end, `\xHH` for a byte that is not UTF-8). Unknown keys are kept,
lines that cannot be read are ignored (the settings window says how many),
and the file is replaced atomically. The search history, when kept, is in
`history` in the same folder, one query per line.

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
The windows find each other on the D-Bus session bus (name
`az.huseynz.EveryGrep`); without a session bus every launch opens a window.

To open the window with a key, add a GNOME shortcut: Settings → Keyboard →
View and Customize Shortcuts → Custom Shortcuts → `+`, name `EveryGrep`,
command `everygrep-gui`, then press the keys you want. The Keyboard shortcut
page of the settings shows the same steps and copies the command.

## Command line

```text
everygrep index [options]            scan and save the index
everygrep search [options] <query>   print matching paths
everygrep shell [options]            read one query per line
```

`search` and `shell` load the saved index; they scan and save it first when
there is none, when `--root` differs from the saved roots, or with
`--rescan`. Paths are printed in name order, one per line, at most `--limit`
(50 by default, `0` for all); counts and timings go to standard error.
`shell --watch` applies file changes before every query and saves the index
on exit. Other options: `--root`, `--index-file`, `--exclude`, `--case`,
`--whole-words`, `--match-path`, `--diacritics`, `--filter`, `--threads`,
`--no-meta`; `everygrep` without arguments prints them all. The command line
is English only.

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

- the index, in `~/.cache/everygrep/index.bin` (names, sizes and modification
  times of the indexed files, never their contents);
- the settings, in `~/.config/everygrep/settings`;
- the search history, in `~/.config/everygrep/history`, only when "Keep the
  history after EveryGrep closes" is on (it is off by default).

It talks to other programs only on this computer: the session bus (single
window, desktop portals for the folder chooser and the light or dark style,
the file manager), `xdg-open` to open files and `gio trash` to move them to
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
