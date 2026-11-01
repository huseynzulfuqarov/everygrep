# Index file format (version 4)

The saved index lets EveryGrep start without scanning the disk again. It is
written and read by `crates/everygrep-core/src/persist.rs`; this page
describes what that code does.

## Location

`$XDG_CACHE_HOME/everygrep/index.bin` (only an absolute `XDG_CACHE_HOME` is
used), otherwise `~/.cache/everygrep/index.bin`. Both programs take
`--index-file <path>` to use another file. The file is a cache: deleting it
only makes the next start scan again.

Each removable drive the window indexed has a file of the same format in
the `volumes` folder next to it, `volumes/<uuid>.bin`, with one root: the
drive's mount point when it was saved. The index file itself holds only the
folders to index. Next to each drive file, `volumes/<uuid>.info` is a small
text file of `key = value` lines (`uuid`, `label`, `path`, `last-seen` in
seconds since 1970, `entries`; `\\`, `\n` and `\xHH` escapes in values).
Deleting a drive's files makes the window forget it.

## Layout

All integers are little-endian. `n` is the number of nodes, `m` the number
of bytes in the name pool.

| Field | Size | Content |
|---|---|---|
| magic | 8 bytes | `EGRPIDX\0` |
| version | u32 | `4` |
| nodes | u64 | `n` |
| names | u64 | `m` |
| roots | u32 | number of root paths, then for each: u32 length and the path bytes |
| parent_kind | n × u32 | bit 31: folder; bit 30: removed (must be 0 in a file); bits 0–29: parent id + 1, or 0 for a root |
| name_start | n × u32 | offset of the node's name in the name pool |
| name_len | n × u16 | length of the name in bytes |
| size | n × u64 | file size in bytes (see below for folders) |
| mtime | n × i64 | modification time in nanoseconds since 1970 (0 = unknown) |
| name pool | m bytes | the names, as raw bytes (any bytes except `/` and NUL; not necessarily UTF-8) |
| name order | n × u32 | node ids sorted by name |
| checksum | u64 | over every byte before it |

Node ids are positions in the columns, starting at 0. A root's name is its
whole path (`/home/me`); every other name is one path component, and a path is
the names from the root down joined by `/`. Symbolic links are stored as
files, with the size `lstat` reports (the length of the target path).

The name order sorts by the case-folded name in natural, diacritic-folded
order, then by the raw name, then by the parent id, which is the order
searches walk through. The natural order (`collate.rs`) compares units from
the left:

- a run of ASCII digits is one unit compared by value (`file2` before
  `file10`, runs of any length); between equal values, more leading zeros
  come first (`002`, `02`, `2`);
- letters with diacritics weigh as their base letters (`ç` as `c`, `ı` and
  `İ` as `i`, `ß` as `ss`), combining marks U+0300–U+036F are skipped, and
  `ə` is a letter of its own between `e` and `f`;
- hyphens, apostrophes and control characters are skipped (`a-b` and `ab`
  are equal and then ordered by raw name);
- the classes are ordered: space, the punctuation
  ``! " # $ % & ( ) * , . / : ; ? @ [ \ ] ^ _ ` { | } ~``, then `+ < = >`,
  then numbers, then the letters `a`–`z`, then every other character by code
  point, then bytes that are not valid UTF-8 by byte value;
- a name that runs out of units first comes first.

## Folder sizes

The size column holds each folder's total (the sum of the sizes of all
files below it) as it was when saved, but the loader does not trust it:
folder totals and item counts are computed again after loading (a few
milliseconds for 600 000 nodes). Files from before folder sizes existed,
with 0 for folders, therefore load unchanged.

## Checksum

A word-at-a-time 64-bit hash, only meant to detect damaged files, not
tampering. Start with `h = 0x5EED XOR (length of the hashed bytes)`. For each
whole 8-byte little-endian word `w`: `h = rotate_left((h XOR w) ×
0x9E3779B97F4A7C15, 29)` (wrapping multiplication). Each remaining byte `b`
of a final partial word is mixed the same way as a word with the value `b`.
The result is `h XOR (h >> 31)`.

## Writing

Before saving, an index with removed slots or unused name bytes is compacted
into a copy, so files never contain removed nodes. The bytes are written to
`<file>.tmp-<pid>` in the same folder, synced, and renamed over the file, so a
crash leaves either the old or the new file. The window saves a changed index
at most every five minutes while it watches, after a rebuild, and when it
ends (also when closing only hides the window).

## Reading and validation

The file is decoded while it is read, so memory stays close to the size of
the index itself. Before anything is allocated, the sizes in the header are
checked against the file size (`30 × n + m` bytes must follow the roots).
After reading:

- the checksum must match;
- every parent link must point to an existing folder (or be 0), names must lie
  inside the pool, and no node may be marked removed;
- parent chains must end at a root without loops;
- the name order must be a permutation of the nodes; if it is not sorted by
  the current rules (a version 4 file, or rules that changed with an update
  of the program) it is sorted again, on one thread per CPU (about 0.05 s
  for 260 000 nodes on 20 threads), and the index is saved with the new order at the next save; two
  nodes with the same parent and name make the file invalid;
- the folded names used for matching are computed again, not read.

Any failure gives an `InvalidData` error ("not an index file", "unsupported
version", "checksum mismatch", …) and the programs scan again instead. A file
of version 4 loads as described above; a file of any other version is not
converted; it is replaced by the next save.

## Version history

The version number changes whenever the layout or the meaning of a field
changes; older versions are rejected, not migrated, except version 4, whose
layout is the same as version 5's and whose name order is sorted again.

| Version | Introduced by the commit |
|---|---|
| 1 | save and load the index with validation and checksum |
| 2 | keep the name order in blocks and support live index changes |
| 3 | store modification times in nanoseconds |
| 4 | recompute folded names on load (the folded name columns and pool were dropped from the file) |
| 5 | order the index by natural, diacritic-folded names |

All of them predate the first release.
