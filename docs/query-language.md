# Query language

The window and the command line share one query language, implemented in
`crates/everygrep-core/src/query.rs` (the examples below come from its tests).
A query is a list of terms; a result must match all of them.

## Terms and operators

| Query | Meaning |
|---|---|
| `report pdf` | both terms (a space is AND) |
| `jpg\|png` | either term; `\|` binds tighter than a space, so `report jpg\|png` is `report AND (jpg OR png)` |
| `!draft` | not: names without `draft`; `!` must start the word (`a!b` is the literal text `a!b`) |
| `<draft\|final> pdf` | grouping with `<` and `>`; groups nest and can be negated (`!<a b>`) |
| `"two words"` | quotes make spaces, `\|`, `!`, `<`, `>`, `*` and `?` literal; `""` is an empty literal |
| (empty query) | every entry |

A missing closing quote, a missing `>` or an unexpected `>` is an error; the
window shows it under the search field, the command line prints `bad query:`.
Empty alternatives (`a\|`, `\|b`) are ignored.

## Plain text

A term without wildcards matches anywhere in the name, not only at the start
of a word: `port` finds `report.pdf`. By default matching ignores

- **case**, with full Unicode lowercasing for valid UTF-8 (bytes that are not
  UTF-8 are compared with ASCII lowercasing only), and
- **diacritics**: letters with marks fold to their base letters (Latin-1,
  Latin Extended-A, and the Azerbaijani, Turkish and Romanian letters:
  `ç ş ğ ı ö ü ə` → `c s g i o u e`, `ß` → `ss`), and combining marks are
  dropped. So `cafe` finds `café` and `sekil` finds `Şəkil`. The table is in
  `crates/everygrep-core/src/fold.rs`.

## Wildcards

`*` (any run of characters) and `?` (one character) make a term a wildcard
pattern that must match the **whole** name: `*.pdf` finds names ending in
`.pdf`, `img_??.png` finds `img_01.png` but not `img_001.png`. Inside quotes
`*` and `?` are literal.

## Modifiers

A modifier is written before a term, `name:term`, and can be stacked
(`case:path:Src/Main`). Modifier names ignore case. A word whose prefix is not
a known modifier is plain text (`http://x` searches for `http://x`).

| Modifier | Effect |
|---|---|
| `case:` / `nocase:` | match case for this term / ignore it |
| `wholeword:` / `nowholeword:` | whole words only: `wholeword:art` finds `pop-art.png` but not `party.png` |
| `diacritics:` / `nodiacritics:` | match diacritics: `diacritics:café` does not find `cafe` |
| `path:` / `nopath:` | match the full path instead of the name |
| `file:`, `files:` | only files; `file:report` = files whose name contains `report` |
| `folder:`, `folders:`, `dir:` | only folders; `folder:src` likewise |
| `ext:` | extensions, see below |
| `size:` | size, see below |
| `modified:` | modification time, see below |

The `no…:` forms turn off a mode that the window's toggles or the command
line options turned on, for one term.

A term that contains `/` always matches the full path: `docs/report` finds
`/home/me/docs/report.pdf`.

Whole words: a word character is any Unicode letter or digit; the match must
not have a word character right before or after it. A term that itself
starts or ends with a non-word character (`wholeword:-v`) is only checked on the
side where it has a word character.

## Extensions

`ext:pdf;jpg` matches **files** whose name ends in `.pdf` or `.jpg` (case and
a leading dot are ignored: `ext:.PDF` works too). `ext:` on its own matches
files without an extension. A dot at the very start of a name does not begin
an extension: `.bashrc` has none. Folders never match `ext:`.

## Size

| Query | Meaning |
|---|---|
| `size:0` | exactly 0 bytes (`size:=0` is the same) |
| `size:>10mb`, `size:>=10mb` | larger than / at least 10 MB |
| `size:<1kb`, `size:<=1kb` | smaller than / at most 1 KB |
| `size:1kb..2mb` | between, inclusive |

Units: none or `b` (bytes), `k`/`kb`, `m`/`mb`, `g`/`gb`, `t`/`tb`, in steps
of 1024. Decimals work: `size:>1.5gb`. A folder matches by the total size of
all files below it, at any depth; add `file:` to match files only. Symbolic
links count with the length of their target path.

## Modification time

Ages, relative to now:

| Query | Meaning |
|---|---|
| `modified:<7d` | changed within the last 7 days (`<=` is the same) |
| `modified:>30d` | not changed for 30 days (`>=` is the same) |

Units: `s`, `m` or `min`, `h`, `d`, `w`.

Calendar dates, in the local time zone:

| Query | Meaning |
|---|---|
| `modified:2026-10-25` | changed on that local day |
| `modified:2026-10`, `modified:2026` | in that month, that year |
| `modified:today`, `modified:yesterday` | today, yesterday |
| `modified:>2026-10-25` | after that day (from the next midnight) |
| `modified:>=2026-10` | from the start of October 2026 |
| `modified:<2026` | before 2026 |
| `modified:<=2026-10` | up to the end of October 2026 |
| `modified:2026-10-01..2026-10-25` | from the start of the first to the end of the last, inclusive |

Months and days may have one or two digits (`2026-1-5`); years have four. A
day starts at local midnight, so days with a daylight saving change are 23 or
25 hours long. The zone comes from `$TZ` or `/etc/localtime` (see
`crates/everygrep-core/src/localtime.rs`). 
Invalid dates (`2026-02-30`) are errors.

## Filters and options outside the query

The window's filter selector and the command line's `--filter <name>` limit
results to one kind, combined with the query (both must match):

| Filter | Accepts |
|---|---|
| `all` | every entry |
| `audio` | audio |
| `archive` | archives (zip, 7z, tar and its compressed forms, packages) |
| `code` | source code, markup, config and build files |
| `document` | text, documents, PDF, spreadsheets, presentations |
| `executable` | programs and scripts by extension (sh, AppImage, exe, …) |
| `folder` | folders |
| `image` | images, including camera raw and editor formats |
| `video` | video |

Kinds are decided from the name only (`crates/everygrep-core/src/kind.rs`);
the index keeps no permission bits, so a Linux program without an extension
is not found by `executable`.

The matching modes can be turned on for the whole query: in the window with
the toggles in the search field (`Alt+C`, `Alt+W`, `Alt+P`, `Alt+D`), on the
command line with `--case`, `--whole-words`, `--match-path` and
`--diacritics`. Modifiers in the query then override them per term.
