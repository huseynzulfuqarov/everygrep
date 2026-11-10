# EveryGrep

Instant file name search for Linux.

EveryGrep keeps a compact in-memory index of every file and folder name below
the folders you choose and finds matches anywhere in a name (not only at word
starts) as you type. A desktop window and a command line program share the
same engine.

It is inspired by [Everything](https://www.voidtools.com) for Windows.

## Status

EveryGrep is in early development. The version is 0.1.0, with no release and
no packages yet.

- **Wayland only.** The window has no X11 backend.
- **Tested on one machine**, with GNOME 51 on CachyOS (Arch-based) and
  btrfs. Other desktops, distributions and file systems are untested.
- **It changes files when you ask it to.** Besides searching, the window can
  rename, move, copy, overwrite, trash and delete. That code is covered by
  tests, but it has had one user so far. Keep backups of anything you would
  miss.
- Most of the code was written by AI models; see
  [How it was built](#how-it-was-built).

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
- **File lists.** Save the entries of a disk to a text file and search them
  while the disk is not connected.
- **Contents and duplicates.** `content:` searches inside files; `duplicates:`
  finds files with the same contents, name or size.
- **Keyboard first.** Every action can have a key, and the keys can be changed.
- **File operations with undo.** Open, rename, copy, move, trash and delete
  from the result list; rename several items with a preview; undo and redo
  for the session, trash included.
- **More than names.** Optional columns from the file system and from file
  metadata (owner, dimensions, duration, tags, camera, page and word
  counts, hashes), a preview panel, a properties dialog and a folder panel.
- **Saved searches.** Tabs, filters, bookmarks and search history.
- **Fits the desktop.** Light and dark themes that follow GNOME, accent
  colors, one window per index that a desktop shortcut brings forward.
- **Translated.** English, Azerbaijani and Turkish.
- **Private.** All data stays on your computer (see [Privacy](#privacy)).

## Installing

Build from source. This needs Rust 1.92 or newer (the minimum of Slint 1.18)
and, at run time, the usual libraries of a Wayland desktop (fontconfig,
freetype, libxkbcommon, Wayland and EGL through libglvnd).

```sh
cargo build --release --locked
```

### Into `~/.local`, by hand

```sh
install -Dm755 target/release/everygrep target/release/everygrep-gui -t ~/.local/bin/
install -Dm644 data/az.huseynz.EveryGrep.desktop -t ~/.local/share/applications/
install -Dm644 data/az.huseynz.EveryGrep.metainfo.xml -t ~/.local/share/metainfo/
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
It installs both programs to `/usr/bin`, with the desktop entry, the
AppStream metadata, the icons, the licenses and the documentation.

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
| `regex:^img_\d+\.jpe?g$` | a regular expression |
| `path:src/main`, `docs/report` | match the full path (any term with `/` does) |
| `file:`, `folder:`, `image:`, `video:`, `document:` | only that kind |
| `ext:pdf;jpg` | extensions |
| `size:>10mb`, `size:1kb..2mb` | size; folders match by their total size |
| `modified:<7d`, `modified:2026-10`, `modified:today` | changed within a time, or in that month or day |
| `content:invoice` | files whose contents hold the text (read from disk while searching) |
| `duplicates:`, `duplicates:name;size` | duplicates among the results |

The full syntax, with every modifier and edge case, is in
[docs/query-language.md](docs/query-language.md).

## The window

Type to search; `↓` moves into the results. The selector next to the search
field limits results to one kind (folders, documents, pictures, …), and the
toggles inside the field switch match case, whole words, full path,
diacritics and regular expressions. Right-click a row for its actions, and
the column headers for more columns.

| Keys | Action |
|---|---|
| `Enter`, double-click | open |
| `Ctrl+Enter` | open the containing folder |
| `Shift+Enter` | open with another application |
| `Ctrl+Shift+C` | copy the full path |
| `Ctrl+X` / `Ctrl+C` / `Ctrl+V` | cut, copy, paste into the selected folder |
| `F2` | rename; with several items selected, rename them all with a preview |
| `Delete` / `Shift+Delete` | move to the trash / delete permanently, after a confirmation |
| `Ctrl+Z` / `Ctrl+Shift+Z` | undo / redo the last file operation |
| `Ctrl+T`, `Ctrl+W` | new tab, close tab |
| `Ctrl+D` | save the search as a bookmark |
| `Ctrl+S` | export the results (CSV, TSV, text, M3U8, JSON, HTML) |
| `Ctrl+,` | settings |
| `F1` | help: search syntax, regular expressions, command line options |

Every key, menu, setting and file operation is described in
[docs/manual.md](docs/manual.md).

## Command line

```text
everygrep index [options]            scan and save the index
everygrep search [options] <query>   print matching paths
everygrep shell [options]            read one query per line
```

`search` and `shell` load the saved index and scan first when there is none.
The command line keeps its own index, apart from the window's, so neither
replaces the other's. Paths are printed in name order, one per line, at most
`--limit` (50 by default, `0` for all). `everygrep` without arguments lists
the options; they are described in
[docs/manual.md](docs/manual.md#command-line).

## Languages

The window is available in English, Azerbaijani (Azərbaycanca) and Turkish
(Türkçe). It follows the session's language, or a choice in Settings →
Appearance → Language. The command line and its messages stay in English.
Adding a language is described in
[CONTRIBUTING.md](CONTRIBUTING.md#translations).

## Privacy

All data stays on your computer. EveryGrep makes no network connections and
collects nothing. It stores:

- the index in `~/.cache/everygrep/`, which holds names, sizes and
  modification times of the indexed files and never their contents;
- next to it, a cache of file hashes from duplicate searches and a cache of
  file metadata read for columns, both with a size limit and a button that
  empties them;
- thumbnails for the preview panel, in the shared `~/.cache/thumbnails`;
- settings, filters and bookmarks in `~/.config/everygrep/`;
- the search history, only when you turn on keeping it after the program
  closes (off by default), and a debug log, only with `--debug-log`.

The only programs it talks to are on this computer. They are the session bus
and the desktop portals, `xdg-open`, a few helpers (`gio`, `pdftotext`, `pdfinfo`,
`getent`) and the system's thumbnail helpers, which run only in a `bwrap`
sandbox. The exact files and helpers are listed in
[docs/manual.md](docs/manual.md#privacy).

## How it was built

I am a software engineer. I built EveryGrep because I wanted a fast file name
search on my own Linux desktop. I also wanted to find out how far I could
take a desktop application, which is outside my usual backend work, by
directing AI coding agents.

The code was written by Claude Opus 5.5 in the Claude Code CLI and by
GPT-6.1 Sol in the Codex CLI. My part was to decide what to build and
how it should behave, to test the window by hand, and to accept or reject
what the agents produced. The first commit is from 25 October 2026; by 10
November 2026 the repository held about 120,000 lines of Rust and Slint,
tests included.

A few things keep that amount of generated code in check.

- Every commit passes `scripts/check.sh`, which runs `cargo fmt --check`,
  clippy with warnings as errors, and the whole test suite (about 1,300
  tests).
- The models reviewed each other's work, and the file operations (copy,
  move, overwrite, delete, undo) had a review pass of their own.
- I tested the window by hand, and the bugs I found went back to the agents.
- Claude Fable 5.2 had a brief look over the project before it went public.

None of this means that a person has read all of the code line by line.
Treat it as you would any young project with one user. Bug reports are
welcome.

## Documentation

- [docs/manual.md](docs/manual.md): the window, the settings, the command
  line
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
