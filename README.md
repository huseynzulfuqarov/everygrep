# EveryGrep

Instant file name search for Linux.

EveryGrep keeps a compact in-memory index of every file and folder name and finds
matches anywhere in a name (not only at word starts) as you type.

**Status:** early development, no release yet.

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
| `diacritics:café` | match diacritics for this term (by default `cafe` finds `café` and `sekil` finds `Şəkil`) |
| `path:src/main`, `docs/report` | match the full path (any term with `/` does) |
| `file:`, `folder:` | only files / only folders |
| `ext:pdf;jpg` | extensions |
| `size:>10mb`, `size:1kb..2mb`, `size:0` | file size (1 kb = 1024 bytes) |
| `modified:<7d`, `modified:>30d` | changed within / not within a time (`s`, `m`, `h`, `d`, `w`) |
| `modified:2026-10-25`, `modified:2026-10`, `modified:today`, `modified:yesterday` | changed on that local day, month or year |
| `modified:>2026-10-25`, `modified:<=2026-10`, `modified:2026-10-01..2026-10-25` | after / up to a whole day, month or year; `..` is inclusive |

Case and diacritics are ignored unless `case:`/`--case` or
`diacritics:`/`--diacritics` is used. `nocase:`, `nowholeword:`, `nodiacritics:` and
`nopath:` turn a mode off for one term. The command line also takes
`--whole-words`, `--match-path` and `--filter <name>` (`audio`, `archive`,
`code`, `document`, `executable`, `folder`, `image`, `video`).

Ignoring diacritics folds letters with marks to their base letters (Latin-1,
Latin Extended-A, and Azerbaijani/Turkish/Romanian letters: `ç ş ğ ı ö ü ə` →
`c s g i o u e`, `ß` → `ss`) and drops combining marks; the table is in
`crates/everygrep-core/src/fold.rs`. Whole words are separated by any
character that is not a letter or digit.

## Using the window

```sh
everygrep-gui [--root <folder>]... [--index-file <file>] [--filter <name>]
              [--case] [--whole-words] [--match-path] [--diacritics]
              [--columns type,extension] [--new-instance]
              [--settings[=indexing|search|window|shortcut]] [query words...]
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
| `Ctrl+,` | settings |
| `Ctrl+Q` | quit (also when closing only hides the window) |

Right-click a row, or press `Menu` or `Shift+F10`, for a context menu with the
same actions. The index follows these changes by itself.

### Settings

Open the settings with `Ctrl+,`, from the `⋯` menu next to the filter
selector, or with `--settings` (`--settings=search` opens that page; given to
a later launch, it opens the settings in the running window). Changes apply at
once; each page has a "Reset to defaults" button. Without a settings file
the program behaves as before.

| Page | Setting | Default |
|---|---|---|
| Indexing | folders to index (added through the desktop's folder chooser) | the folders of the saved index, or the home folder |
| | excluded folders: listed, but not searched inside | none |
| | "Rebuild index now" | — |
| Search | filter and matching modes when EveryGrep starts | all, every mode off |
| | searches kept for `Alt+←`/`Alt+→` (0 turns the history off) | 200 |
| | keep the history after EveryGrep closes | off |
| Window | closing the window: quit, or hide it and keep running | quit |
| | remember the shown columns and their widths | on |
| Keyboard shortcut | how to open EveryGrep with a key (see below) | — |

Changing the folders rebuilds the index in the background while the window
stays usable; the status line shows the progress, and the new index is saved
as soon as it is complete. An excluded folder is matched exactly, and its own
entry stays in the results. Hidden files are always indexed.

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
`match-path`, `match-diacritics`, `remember-columns`,
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

## Building

```sh
cargo build --release
./target/release/everygrep --version
```

Requires Rust 1.85 or newer.

## Development

Run `scripts/check.sh` before every commit. It checks formatting, runs clippy with
warnings as errors, and runs the test suite.

The single-window test needs a private session bus and is skipped by default:

```sh
EVERYGREP_PRIVATE_BUS=1 dbus-run-session -- cargo test -p everygrep-gui -- --ignored
```

## License

EveryGrep is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
