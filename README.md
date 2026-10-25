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
| `path:src/main`, `docs/report` | match the full path (any term with `/` does) |
| `file:`, `folder:` | only files / only folders |
| `ext:pdf;jpg` | extensions |
| `size:>10mb`, `size:1kb..2mb`, `size:0` | file size (1 kb = 1024 bytes) |
| `modified:<7d`, `modified:>30d` | changed within / not within a time (`s`, `m`, `h`, `d`, `w`) |
| `modified:2026-10-25`, `modified:2026-10`, `modified:today`, `modified:yesterday` | changed on that local day, month or year |
| `modified:>2026-10-25`, `modified:<=2026-10`, `modified:2026-10-01..2026-10-25` | after / up to a whole day, month or year; `..` is inclusive |

Case is ignored unless `case:` or `--case` is used.

## Using the window

```sh
everygrep-gui [--root <folder>]... [--index-file <file>] [query words...]
```

Words after the options become the first query; it runs as soon as the index is
loaded. Type to search; `↓` moves into the results.

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
