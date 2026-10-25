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
