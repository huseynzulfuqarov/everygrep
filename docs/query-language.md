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
| `prefix:` / `noprefix:` | the term must match at the start of the name: `prefix:rep` finds `report.pdf`, not `draft report.pdf` |
| `suffix:` / `nosuffix:` | the term must match at the end of the name: `suffix:.tar.gz` |
| `nopunct:` / `punct:` | ignore punctuation / match it: `nopunct:foo-bar` finds `foo_bar.txt`, `foo-bar.txt` and `foobar.txt` |
| `nospace:` / `space:` | ignore whitespace / match it: `nospace:myfile` finds `my file.txt` |
| `regex:` / `noregex:` | a regular expression, see below |
| `file:`, `files:` | only files; `file:report` = files whose name contains `report` |
| `folder:`, `folders:`, `dir:` | only folders; `folder:src` likewise |
| `ext:` | extensions, see below |
| `size:` | size, see below |
| `modified:` | modification time, see below |

The `no…:` forms (and `punct:`, `space:`) turn off a mode that the window's
toggles or the command line options turned on, for one term.

### Prefix and suffix

`prefix:` anchors the term at the start of the name, `suffix:` at its end;
both together make the term equal the whole name (as it is compared: case,
diacritics and the ignore modes still apply). With the full path they anchor
at the start or end of the path. With whole words only the open side is
checked: `prefix:wholeword:art` finds `art.png`, not `artist.png`. A wildcard
pattern always matches the whole name; with `prefix:` it no longer has to
reach the end (`prefix:img_??` finds `img_01.png`), with `suffix:` it no
longer has to start at the beginning (`suffix:?.png`).

### Ignoring punctuation and whitespace

`nopunct:` leaves punctuation out of the name and the term before comparing:
ASCII punctuation (``!"#$%&'()*+,-./:;<=>?@[\]^_`{|}~``) and the common
punctuation of Latin-1 (`« » ¡ ¿ · § ¶`), the General Punctuation block
(dashes, quotation marks, `…`, `‰`, …) and CJK texts. `nospace:` leaves out
whitespace (Unicode `White_Space`). In a wildcard pattern `*` and `?` keep
their meaning. Word boundaries for `wholeword:` and the start and end for
`prefix:`/`suffix:` are those of the name after the characters were left
out, so punctuation no longer separates words: `nopunct:wholeword:art` does not
find `pop-art` (compared as `popart`). A `/` in the term still
switches it to the full path, whose `/` are then left out too. A term with
nothing left (`nopunct:-`) is no condition.

### Regular expressions

`regex:` makes the rest of the word a regular expression in the syntax of
the Rust [`regex`](https://docs.rs/regex/1.13/regex/#syntax) crate. The
expression runs to the next space: `|`, `<` and `>` in it belong to it
(`regex:(jpg|png)$`); quote it to include spaces (`regex:"^my file"`), and
put a space before a closing `>` of a group. It matches anywhere in the
name unless anchored with `^` and `$`.

- It is matched against the name as it is stored (not folded), or against
  the full path with `path:` or the full path mode. A `/` in the expression
  does not switch to the full path.
- Case is ignored (Unicode case folding: `regex:^şə` finds `Şəkil.png`)
  unless `case:` or the match case mode is on.
- Diacritics are not folded: `regex:^se` does not find `Şəkil.png`; write
  the letters (`[sş]`) or a class.
- Whole words, prefix, suffix and the ignore modes do not apply; use `\b`,
  `^`, `$` instead.
- A name that is not valid UTF-8 is matched byte by byte; `.` does not
  match a byte that is not part of a valid character.
- An invalid expression is an error (`bad regular expression: unclosed
  group`), shown under the search field.

The expression is compiled once per query and is as fast as plain text on
typical patterns (see the timing test in `crates/everygrep-core/src/search.rs`).

In **regex mode** (`Alt+R` in the window, `--regex` on the command line) the
whole query is one regular expression: blanks at both ends are dropped,
spaces, `|`, `!`, `<`, `>` and quotes are part of it, and modifiers are not
read. The filter, match case and full path still apply.

A term that contains `/` always matches the full path: `docs/report` finds
`/home/me/docs/report.pdf`.

Whole words: a word character is any Unicode letter or digit; the match must
not have a word character right before or after it. A term that itself
starts or ends with a non-word character (`wholeword:-v`) is only checked on the
side where it has a word character.

## Extensions

`ext:pdf;jpg` matches **files** whose name ends in `.pdf` or `.jpg` (case and
a leading dot are ignored: `ext:.PDF` works too). `|` separates the list like
`;`: `ext:jpg|png` is one list, not `ext:jpg` OR the text `png` (a `|` with
spaces around it, `ext:jpg | png`, is still an OR). `ext:` on its own matches
files without an extension. A dot at the very start of a name does not begin
an extension: `.bashrc` has none. Folders never match `ext:`.

An entry with `*` or `?` is a wildcard pattern for the extension: `ext:jp*`
finds `.jpg` and `.jpeg`, `ext:jp?` only `.jpg`, `ext:*` any file that has an
extension. A pattern with dots is matched against as many parts of the name
as it has: `ext:t*.gz` finds `x.tar.gz`, `ext:*.*` any name with two
extensions.

## Size

| Query | Meaning |
|---|---|
| `size:0`, `size:1024` | exactly 0 / 1024 bytes (a number without a unit is exact) |
| `size:1mb` | 1 MB up to just under 2 MB (1 048 576 to 2 097 151 bytes) |
| `size:1.5kb` | 1.5 KB up to just under 1.6 KB (1536 to 1638 bytes) |
| `size:=1mb`, `size:==1mb` | exactly 1 MB |
| `size:!1mb` | not 1 MB up to just under 2 MB |
| `size:!=1mb`, `size:<>1mb` | not exactly 1 MB |
| `size:>10mb`, `size:>=10mb` (also `=>`) | larger than / at least 10 MB |
| `size:<1kb`, `size:<=1kb` (also `=<`) | smaller than / at most 1 KB |
| `size:1kb..2mb` | between, inclusive; `size:2mb..1kb` is the same |
| `size:large` | a size word, see below |

A value with a unit stands for all sizes up to the next step of its last
digit: `size:1mb` finds files of 1 MB and of 1.4 MB, `size:1.5kb` steps by
0.1 KB. The comparisons, `=`, `!=` and the ends of a range use the value
itself.

Units: none or `b` (bytes), `k`, `kb`, `ki`, `kib`, and the same with `m`,
`g`, `t` and `p`, all in steps of 1024 (`kb` and `kib` are the same).
Decimals work: `size:>1.5gb`.

Size words, with comparisons too (`size:>large` is larger than 16 MB):

| Word | Size |
|---|---|
| `empty` | 0 bytes |
| `tiny` | over 0 bytes, up to 10 KB |
| `small` | over 10 KB, up to 100 KB |
| `medium` | over 100 KB, up to 1 MB |
| `large` | over 1 MB, up to 16 MB |
| `huge` | over 16 MB, up to 128 MB |
| `gigantic` | over 128 MB |

A folder matches by the total size of all files below it, at any depth; add
`file:` to match files only. Symbolic links count with the length of their
target path.

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
the toggles in the search field (`Alt+C`, `Alt+W`, `Alt+P`, `Alt+D`) and the
Search menu (`Alt+B` prefix, `Alt+E` suffix, `Alt+U` ignore punctuation,
`Alt+S` ignore whitespace, `Alt+R` regex), on the command line with
`--case`, `--whole-words`, `--match-path`, `--diacritics`, `--prefix`,
`--suffix`, `--ignore-punctuation`, `--ignore-whitespace` and `--regex`.
Modifiers in the query then override them per term (except in regex mode,
where the query is not split into terms).
