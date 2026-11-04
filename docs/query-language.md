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
So is nesting deeper than 100 levels of groups, `!` and `file:`/`folder:`
(`!!!…a` with more than 100 `!`).
Empty alternatives (`a\|`, `\|b`) are ignored.

Settings → Search → Allow literal AND / OR / NOT operators (off by
default) recognizes standalone, unquoted uppercase `AND`, `OR`, `NOT` as
operators alongside space, `|`, `!`. They use the same precedence and grouping.
Lowercase `and` / `or` / `not`, quoted words (`"OR"`), substrings (`candy`)
and modifier values (`case:OR`, `regex:OR`) remain text. The whole-query regex
mode bypasses operator parsing. Changing the setting reruns the current query.

Settings → Search → Evaluate AND before OR changes precedence to
`AND > OR`. It is off by default, preserving `OR > AND`: `a b|c` means
`a AND (b OR c)`. With the setting on it means `(a AND b) OR c`.
Negation still binds first; explicit `<...>` groups work in both modes,
including nested groups. Word and symbolic operators use the same setting.
The whole-query regex mode is unaffected.

The window has a separate Hidden files result filter (Search menu and
Search settings): Show (default), Hide, Only. `Ctrl+H` cycles it. It combines
with the kind filter and the query, including regex mode and empty queries.
A dot-prefixed entry or any descendant of a dot-prefixed folder is hidden;
components in an indexed root path count too. This checks indexed names,
with no disk access, rescanning or removal from the index. The Attributes
column retains its own-name `H` meaning.

## Parents, children and numeric properties

These conditions read the index tree only, including saved offline entries.

| Query | Meaning |
|---|---|
| `parent:docs` | items directly inside a folder whose name contains docs |
| `parent:wholename:docs` | items whose immediate parent has exactly that name |
| `parent:/home/me/docs` | match the immediate parent's full path (a slash selects path mode) |
| `child:*.txt` | folders with a matching direct child, including files and folders |
| `child:<file: ext:txt !draft>` | folders with a direct child satisfying all conditions |
| `case:child:<Readme.md\|LICENSE>` | modifiers before the function apply to every term in the group |
| `depth:3`, `depth:<=2`, `depth:2..4` | absolute path depth, for files and folders; `/` is 0, `/home` is 1, `/home/me/a.txt` is 3 |
| `len:5..20` | name length in Unicode scalar values; each invalid UTF-8 byte counts once |
| `children:0`, `childfiles:>10`, `childfolders:1..3` | direct child counts, matching folders only |

Integer conditions accept `=`, `<`, `<=`, `>` and `>=` or an inclusive
`min..max` range. Empty, negative, overflowing and reversed ranges are errors.
`<0` and `>18446744073709551615` are valid empty ranges.
Depth includes components above an indexed root: `/home/me` has depth 2 even
when it is indexed on its own. For folders alone, combine with `folder:`.
Counts include only indexed direct children; excluded or unindexed items
cannot contribute. Symlinks follow the index's file/folder classification.
Renames, moves and removals take effect through the current index tree.

Parent and child conditions can be negated, grouped, nested and combined
with other query terms. `parent:` has no match for an indexed root, whose
parent is outside the index. When a relative is an indexed root, name
conditions see its final path component; `path:` sees its full path.
A function needs a non-empty term or group. Inside these functions only
indexed conditions are allowed: content, disk times, executable checks,
result limits and duplicate searches are rejected with a diagnostic.
Those functions can still be used alongside the parent/child condition.
Relative names are not highlighted in the result's own name.
Tree walks check the search cancellation flag, and cancelled searches
return no partial results.

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
`.pdf`, `img_??.png` finds `img_01.png` but not `img_001.png`. A `?` is one
character of the name as it is compared, after case and diacritics are
folded: with diacritics ignored (the default) `ß` and `Æ` become `ss` and
`ae`, so a name `ß` needs `??`; with `diacritics:` a capital `İ` lowercases
to `i` and a combining dot, two characters. With `case:diacritics:` each
`?` is one character of the name as written. Inside quotes
`*` and `?` are literal, and only there: `*"draft report"*` finds names
containing `draft report`, `"a*"*` names starting with `a*`. In the window, Settings → Search → Match wildcards
anywhere in the filename opens both ends instead (off by default).
`wholename:ain*` forces whole-name wildcard matching
and `nowholename:ain*` allows a match inside `main.rs`, regardless of that
setting. On text without wildcards, `wholename:` requires the whole name
(for a path term, the whole path) to be that text: `wholename:main.rs` finds
`main.rs` but not `main.rs.bak`, with case and diacritics matched as for any
other term (`case:wholename:Main.rs`). Without the modifier, ordinary text keeps its
substring behavior. Explicit `prefix:` / `suffix:` still anchor their side of
a wildcard.
In path mode the open ends can cross folders, while `*` / `?` inside the
original pattern keep their separator rules.

Against the full path (`path:`, a term with `/`, or the full path mode), `*`
and `?` do not cross a `/`, and `**` matches any run including `/`, the same
rule as the index's exclude patterns: `path:/home/me/*.txt` finds
`/home/me/a.txt` but not `/home/me/docs/a.txt`, `path:/home/me/**.txt` finds
both, `path:/home/me/**/a.txt` the second. In a name, which has no `/`, `**`
is the same as `*`. In names that are not valid UTF-8, each invalid byte
counts as one character for `?` and `*`, so they still stop at a `/`.
The open side of `prefix:` or `suffix:` crosses folders:
`prefix:path:/home/me/d*` also finds all entries below `/home/me/docs`.

## Modifiers

A modifier is written before a term, `name:term`, and can be stacked
(`case:path:Src/Main`). Modifiers stacked before `file:`, `folder:` or a
filter word apply to its term too: `case:file:README` is the same as
`file:case:README`. Modifier names ignore case. A word whose prefix is not
a known modifier is plain text (`http://x` searches for `http://x`).

| Modifier | Effect |
|---|---|
| `case:` / `nocase:` | match case for this term / ignore it |
| `wholeword:` / `nowholeword:` | whole words only: `wholeword:art` finds `pop-art.png` but not `party.png` |
| `diacritics:` / `nodiacritics:` | match diacritics: `diacritics:café` does not find `cafe` |
| `path:` / `nopath:` | match the full path instead of the name |
| `wholename:` / `nowholename:` | match a wildcard, or plain text exactly, against the whole name / a wildcard anywhere in it: `wholename:main.rs` does not find `main.rs.bak` |
| `prefix:` / `noprefix:` | the term must match at the start of the name: `prefix:rep` finds `report.pdf`, not `draft report.pdf` |
| `suffix:` / `nosuffix:` | the term must match at the end of the name: `suffix:.tar.gz` |
| `nopunct:` / `punct:` | ignore punctuation / match it: `nopunct:foo-bar` finds `foo_bar.txt`, `foo-bar.txt` and `foobar.txt` |
| `nospace:` / `space:` | ignore whitespace / match it: `nospace:myfile` finds `my file.txt` |
| `regex:` / `noregex:` | a regular expression, see below |
| `file:`, `files:` | only files; `file:report` = files whose name contains `report` |
| `folder:`, `folders:`, `dir:` | only folders; `folder:src` likewise |
| `ext:` | extensions, see below |
| `audio:`, `archive:`, `code:`, `document:`, `executable:`, `image:`, `video:` | filter words, see "Filter words" below |
| `size:` | size, see below |
| `modified:` | modification time, see below |
| `created:` / `accessed:` | creation / access time, see below |
| `limit:` | the number of results, see below |
| `duplicates:`, `extra-copies:`, `distinct:`, `unique:` | duplicates, see below |
| `content:`, `content-utf8:`, `content-utf16le:`, `content-utf16be:`, `content-codepage:` | file contents, see below |

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

`ext:pdf;jpg` matches **files** whose name ends in `.pdf` or `.jpg` (a
leading dot is ignored, and so is case: `ext:.PDF` works too). With `case:`
or the match case mode the case counts: `case:ext:JPG` finds `a.JPG` but not
`a.jpg`. `|` separates the list like
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
Decimals work: `size:>1.5gb`. A value above the largest possible size,
2^64 − 1 bytes (16 777 216 TB − 1 byte), is an error.

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
| `modified:<7d`, `modified:7d` | changed within the last 7 days, up to now (`<=` is the same); times in the future do not match |
| `modified:>30d` | not changed for 30 days (`>=` is the same) |

Units: `s`, `m` or `min`, `h`, `d`, `w`. An age without a comparison means
"within", like `<`: an age is a span that ends now, so `modified:7d` is the
same as `modified:last7days` (apart from a day with a daylight saving
change, which `7d` counts as 24 hours). `=` and `!` do not take an age.

Calendar dates, in the local time zone:

| Query | Meaning |
|---|---|
| `modified:2026-10-25` | changed on that local day |
| `modified:2026-10`, `modified:2026` | in that month, that year |
| `modified:today`, `modified:yesterday`, `modified:tomorrow` | today, yesterday, tomorrow |
| `modified:thisweek`, `modified:lastweek`, `modified:nextweek` | this, last, next calendar week (Monday to Sunday) |
| `modified:thismonth`, `modified:lastmonth`, `modified:nextmonth` | this, last, next calendar month |
| `modified:thisyear`, `modified:lastyear`, `modified:nextyear` | this, last, next calendar year |
| `modified:monday` … `modified:sunday`, `modified:mon` … `modified:sun` | the latest such day, today included: on a Wednesday `monday` is two days ago and `wednesday` is today |
| `modified:january` … `modified:december`, `modified:jan` … `modified:dec` | the latest such month, the current one included: in October 2026 `oct` is October 2026 and `dec` is December 2025 |
| `modified:>2026-10-25` | after that day (from the next midnight) |
| `modified:>=2026-10` | from the start of October 2026 |
| `modified:<2026` | before 2026 |
| `modified:<=2026-10` | up to the end of October 2026 |
| `modified:2026-10-01..2026-10-25` | from the start of the first to the end of the last, inclusive |

The words may be written with `-` or `_` (`last-week`); they
work with the comparisons and ranges like dates (`modified:>=lastmonth`,
`modified:lastweek..thisweek`). A week starts on Monday.

Spans that end or start now, with a count `n` (1 to 1 000 000):

| Query | Meaning |
|---|---|
| `modified:last3days` | from 3 days before now up to now |
| `modified:next2hours` | from now up to 2 hours ahead |

Units are `years`, `months`, `weeks`, `days`, `hours`, `minutes` (`mins`)
and `seconds` (`secs`), singular or plural. `lastweek` without a count is the previous calendar week, while
`last1week` is the 7 days up to now. Days, weeks, months and years keep the
local wall clock time, so `last1day` on the day the clocks go back is 25
hours long, also when the change falls on the day the span starts, and a day
that does not exist in the month is the month's last day (`last1month` on 31
March starts on the last day of February at the same time). A time that the
clocks skip on that day is moved to the first moment after the gap; a time
they repeat is its earlier occurrence. These words may be written with `-` or `_` too (`last-3-days`) and
compare by their whole span: `modified:<last3days` is older than 3 days,
`modified:>last3days` is in the future.

Months and days may have one or two digits (`2026-1-5`); years have four. A
day starts at local midnight, so days with a daylight saving change are 23 or
25 hours long. The zone comes from `$TZ` or `/etc/localtime` (see
`crates/everygrep-core/src/localtime.rs`). 
Invalid dates (`2026-02-30`) are errors.
Entries whose modification time is not known (shown empty: indexed without
file details, not readable, or a folder that could not be listed and is
read again later) never match `modified:`.

## Creation and access time

`created:` and `accessed:` take the same values as
`modified:`: `created:today`, `accessed:>30d`, `created:2026-10`, `created:lastweek`. The index
does not keep these times, so they are read from disk while searching
(`statx`, links are not followed), one lookup per entry that the other
terms of the query let through; they are checked last. Combine them with
other terms on large indexes (`created:today ext:pdf` is cheaper than `created:today`).

- The creation time is the birth time; entries on file systems that do not
  record it (and entries that cannot be read: removed, on a drive that is not
  plugged in) never match `created:`. Entries of an offline drive are not
  looked up, so they match neither `created:` nor `accessed:`, even when
  another drive is mounted at its folder.
- The access time is coarse on most systems (`relatime` updates it at most
  once a day); entries that cannot be read never match `accessed:`.

## File contents

`content:invoice` finds files whose contents hold `invoice`. The index keeps
names only, so contents are read from disk while searching, and only for the
entries that the other terms let through: the content terms are checked last.
Combine them with other terms (`content:invoice ext:pdf;txt modified:thisyear`);
a content term alone reads every file of the index.

- The text is literal: `*`, `?` and words with a colon (`content:http://`)
  are text to look for. Quote spaces and operators: `content:"due date"`,
  `content:"a<b"`.
- `content:<a b>` holds both texts, `content:<a|b>` either, `content:<a !b>`
  `a` without `b`. Outside a group `|` and `!` work as usual:
  `content:invoice|bill` is a content term or a name term.
- `case:` and `wholeword:` (and the window's Match case and Match whole words) apply,
  `regex:content:^From:` is a regular expression (to the next space). Other
  modifiers (`path:`, `prefix:`, the ignore modes, `diacritics:`) do not
  apply: diacritics are matched as written in contents, `ş` does not find
  `s`.
- A match may run over line ends: a space in the text also matches a line
  break with the spaces and tabs around it, so `content:"salam dünya"`
  finds "salam" at the end of one line and "dünya" at the start of the
  next (not across an empty line). In a regular expression `\n`, `\s` and
  classes such as `[^a]` match line breaks too, `.` does not. Settings →
  Search → File contents → "Let a match run over line ends" turns this off
  (then a match ends at the end of its line); files over 16 MB are always
  searched line by line, since a match over lines holds the whole file in
  memory. `^` and `$` match at the start and end of every line either way.
- Folders never match, and neither do links (the target is not read),
  FIFOs, sockets, devices, files that cannot be read, and entries of an
  offline drive.
- Files over 100 MB are not read (Settings → Search → File contents; 0
  reads files of any size), and neither are binary files: a NUL byte
  before the first match ends the search of a file.
- PDF files are binary for a content term unless Settings → Search →
  Search inside PDF files is on: a file named `*.pdf` is then read as the
  text that `pdftotext` (from the poppler package) prints for it, in a
  separate process that is stopped after 30 seconds. A PDF that it cannot
  read does not match. Office documents are not read yet.
- Encoding: UTF-8; UTF-16 (little or big endian) when the file starts with a
  byte order mark; a file that is not valid UTF-8 is read again in the code
  page of Settings → Search → File contents (Windows-1254, Turkish and
  Azerbaijani Latin, by default). `content-utf8:`, `content-utf16le:` (little
  endian), `content-utf16be:` and `content-codepage:` (that code page) read every
  file in that one encoding, without looking for a byte order mark.
- A query can have at most 6 content terms. A named filter chosen in the
  window is searched with the query, so its content terms count too; only one
  of the two may have a duplicate term (`duplicates:` and its relatives).

## Duplicates

`duplicates:` lists the files whose contents another result has too: copies
of the same file. The results are compared among themselves, so combine it with
other terms to say where to look (`duplicates: ~/Pictures ext:jpg;png`). Each
group of equal files stays together, at the place of its first member in the
order shown, and its members follow in that order.

- Contents are compared as fast as possible: files of a size no other file
  has are not read; files of the same size are compared by their first and
  last 16 KiB, and only files that still agree are read in full (XXH3-128,
  a 128-bit hash). The window keeps the hashes in a cache (Settings →
  Search → Hash cache), so a second search reads only files that changed;
  the command line reads them every time. With Settings → Search →
  Duplicates → "Compare duplicates byte by byte" (off by default) files
  with equal hashes are also read side by side and compared byte by byte,
  so even files crafted to share a hash are not shown as copies; it reads
  every file of a group once more.
- Folders, empty files, links, FIFOs, devices and files that cannot be read
  are left out. Empty files count (as one group of copies) when Settings →
  Search → Duplicates → "Count empty files as duplicates" is on (off by
  default; the command line leaves them out). Hard links are one file: of several paths to the same file
  only the first is listed, and it is not its own duplicate.
- Other properties are named after the colon, separated by `;` or `,`:
  `duplicates:name` (the same name in different folders), `duplicates:stem`
  (the name without its extension), `duplicates:ext`, `duplicates:size`,
  `duplicates:modified` (`dm`) and `duplicates:content` (the default). With
  several, all must be equal: `duplicates:name;size`. Only `content` reads
  files. Size, extension and contents apply to files only; for the others, files and folders form groups of
  their own. Names, stems and extensions ignore case unless `case:` is set
  (`case:duplicates:name`) or Match case is on.
- What is listed depends on the word, like `DISTINCT` in SQL and the
  `uniq` tool:

  | Word | Lists |
  |---|---|
  | `duplicates:` | every member of every group: all copies |
  | `extra-copies:` | every member but the first of each group (as sorted): the copies you could remove and still keep one of each |
  | `distinct:` | one of each: the first of each group and every result without a duplicate |
  | `unique:` | only the results without a duplicate |

  In the window, with Settings → Appearance → alternate row color on,
  every second group of a `duplicates:` or `extra-copies:` list is shaded
  (instead of every second row), so the groups stand apart.

  `duplicates:` and `extra-copies:` are short names of `duplicates:` and
  `extra-copies:`.
- Like `limit:`, the term applies to the whole query wherever it is written,
  and `limit:` applies after it. A query can have one.

## Number of results

`limit:100` keeps only the first 100 results, in the order they are shown:
the window sorts all results by the chosen column first, so `limit:10` with
the Size column sorted largest first gives the ten largest. The command line
keeps them in name order (its `--limit` then shortens what is printed).
`limit:` is not a condition but a setting for the whole query: the other
terms decide what matches, and it counts the same wherever it is written,
also inside a group or after `!`. With several, the smallest wins;
`limit:0` gives no results. A value that is not a whole number
(`limit:x`, `limit:-1`, `limit:1.5`) is an error. In regex mode it is part
of the expression, like any other text.

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
| `executable` | programs, scripts and launchers by extension (sh, AppImage, desktop, exe, bat, …), and files of no known type with an execute bit |
| `folder` | folders |
| `image` | images, including camera raw and editor formats |
| `video` | video |

Kinds are decided from the name (`crates/everygrep-core/src/kind.rs`), with
one exception: the index keeps no permission bits, so for `executable` a file
whose name gives no known type (`firefox`, `build.real`) is looked up on disk
while searching (`lstat`, links are not followed) and passes when it is a
regular file with an execute bit. Files with a known type do not pass by
their bits (`notes.txt` with an execute bit is a text file), so drives that
mark every file executable (FAT, NTFS) do not fill the results with
documents. Files that cannot be read then (removed, or on a drive that is
not plugged in) do not pass, and files of a drive the window shows as
offline are not looked up at all, even when another drive is mounted at its
folder. This check costs a file system lookup per such file, so it runs
after the other terms of the query.

### Filter words

The same filters can be written in the query, as a word with a colon:

| Word | Filter |
|---|---|
| `audio:` | audio |
| `archive:` | archive |
| `document:` | document |
| `executable:` | executable |
| `image:` | image |
| `video:` | video |
| `code:` | code |

They combine with other terms like any term: `image: beach`, `!video:`,
`<audio:|video:> 2026`, and like `file:` they take a term after the colon:
`image:beach` = `image: beach`. They use the same extension lists as the
filter selector. `folder:` is described with the modifiers above.

The matching modes can be turned on for the whole query: in the window with
the toggles in the search field (`Alt+C`, `Alt+W`, `Alt+P`, `Alt+D`) and the
Search menu (`Alt+B` prefix, `Alt+E` suffix, `Alt+U` ignore punctuation,
`Alt+S` ignore whitespace, `Alt+R` regex), on the command line with
`--case`, `--whole-words`, `--match-path`, `--diacritics`, `--prefix`,
`--suffix`, `--ignore-punctuation`, `--ignore-whitespace` and `--regex`.
Modifiers in the query then override them per term (except in regex mode,
where the query is not split into terms).
