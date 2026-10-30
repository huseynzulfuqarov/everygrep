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
              [--columns type,extension] [query words...]
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

Right-click a row, or press `Menu` or `Shift+F10`, for a context menu with the
same actions. The index follows these changes by itself.

## Building

```sh
cargo build --release
./target/release/everygrep --version
```

Requires Rust 1.85 or newer.

## Development

Run `scripts/check.sh` before every commit. It checks formatting, runs clippy with
warnings as errors, and runs the test suite.

## License

EveryGrep is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
