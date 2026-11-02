# Architecture

EveryGrep keeps the name of every file and folder below a few root folders in
memory, searches them on all CPU cores as you type, and keeps them current
with inotify. Nothing leaves the computer: EveryGrep makes no network
connections, has no telemetry and runs no service besides the window itself.

## Crates

| Crate | Kind | Role |
|---|---|---|
| `everygrep-core` | library | index, scanner, watcher, query language, search, sorting, saved index, file contents |
| `everygrep-cli` | binary `everygrep` | `index`, `search` and `shell` commands over the core |
| `everygrep-gui` | binary `everygrep-gui` | the window: Slint UI, settings, single instance, file actions |

The core uses the standard library, the `regex` crate for regular expressions
in queries, `rustix` for opening files and folders without following links,
the line searcher and matcher of ripgrep (`grep-searcher`, `grep-regex`) with
`encoding_rs` for reading file contents (`content.rs`), `md-5`, `sha2`,
`crc32fast` and `twox-hash` (XXH3-128) for hashes of file contents
(`hash.rs`), and hand-written
bindings for the few Linux calls it needs (`inotify.rs`). The window adds Slint (UI toolkit) and
zbus (session bus); see `THIRD-PARTY-LICENSES.md` for the full list.

## Core

### Node store (`store.rs`)

Nodes live in parallel columns indexed by a `NodeId` (u32):

- `parent_kind`: the parent id + 1 (0 for a root), a folder bit and a
  removed bit, packed in one u32;
- the name as a range of a shared byte pool, and a case-folded copy of it
  for matching, stored only when it differs from the raw name (diacritics
  are not removed in it; a term that ignores them strips them from the
  name while matching, which costs nothing for ASCII names);
- size and modification time (nanoseconds);
- first-child and sibling links, so a subtree can be removed in time
  proportional to its size;
- for folders, the total size of all files below and the number of direct
  children, updated on every change by walking up to the root.

A root's name is its whole path; other names are single components. Removed
nodes keep their slot (marked removed) until the store is compacted.

An index can hold roots of different origins. `Index::graft` adds another
index (a drive's saved index) by appending its columns and merging the two
name orders in one pass; `Index::extract` copies chosen roots with their
subtrees into an index of their own (to save a drive apart from the rest).
A root can be marked **offline** (`Index::set_offline`): its nodes stay
searchable, but catch-ups, watchers (`Watcher::new`, `catch_up`) and
`Index::is_offline` treat it as unreachable, so nothing below it is read,
watched or removed. `Watcher::watch_root`, `unwatch_root` and
`catch_up_roots` bring one root online or take it offline while watching.

### Index and search (`index.rs`, `order.rs`, `search.rs`)

The index is the store plus all live node ids sorted by folded name
(`order.rs`): a list of sorted blocks (built with 512 ids, split above 1024),
so inserts and removals while watching only shift one block. Names are
compared by `collate.rs` in natural order: digit runs by value, diacritics
ignored (letters weigh as their base letters, `ə` as a letter after `e`),
hyphens and apostrophes skipped; it works on the stored case-folded names
directly, so it needs no key per node. Building the order after a scan or
sorting a loaded one again runs on one thread per CPU (sorted runs, then
merged in pairs). Equal results break ties by raw name,
then parent. Search keeps its own folding: with diacritics ignored, `ə`
matches `e`.

`search()` splits the blocks into groups of 16 and lets worker threads (one
per CPU by default) take groups one by one; each evaluates the parsed query
against every node and the results are concatenated in name order. Text
matching works on the folded names, so it never allocates or folds per
comparison. `sort.rs` reorders results by another column (path, size,
modified, type, extension, created, accessed, attributes) with a stable sort,
so equal keys keep name order. The index's own name order breaks ties between
names that collate equal by raw name and parent id; `sort.rs` first puts each
run of such names in folder path order, then raw name order (one look at each
neighbour finds the runs, and only their folders get a path key), so that
order means something to the reader. Folder paths compare component by
component in the same natural order (`dir2` before `dir10`). Creation time, access time and attributes are
not kept in the index; `details.rs` reads them with `lstat` (`statx` with the
birth time) for the rows on screen, on a thread of the window's own
(`DetailReader`: the rows are shown at once and updated when the values
come), and the sort reads them for every result on several threads.

Contents (`content.rs`) are not indexed. A query with `content:` terms is
searched in two stages: `search_staged` evaluates every node with the
content terms unknown (three-valued: an AND with a false term is false, an
OR with a true one true) and keeps the matches and the nodes the contents
decide, with their path, size and time and a truth table of the query over
the answers of its content terms (at most 6, so 64 bits). `decide_all`
then reads those files on worker threads in name order, without the index,
asking for the answer of a term only while it can still change the result,
and records each decision as it comes. `search_file` opens a regular file
without following links (`O_NOFOLLOW`, and `O_NONBLOCK` so a FIFO swapped in
never blocks), reads it with `grep-searcher` in 64 KiB buffers and stops at
the first match, a NUL byte (binary) or a cancel. A `ContentCache` keeps the
answers by term, path, size and time.

`frequency.rs` counts, over the whole index, how many entries share each
name (by kind, ignoring case), how many files each extension and how many
files (or folders) each size, in one pass with three hash maps that borrow
the names; the counts are kept by node id (12 bytes per node).
`Index::frequencies` builds them when the index changed since the last
ones (`Index::version` grows with every change of names, sizes and times)
and keeps them; `sort.rs` sorts by them.

A query with a duplicate term (`dupe.rs`) groups its matches: `group`
sorts them by the values of the properties and compares neighbours, so it
costs one sort. For contents it gives files of the same size, with their
paths, as candidates; `compare_contents` then works without the index, on
worker threads: it looks at each candidate on disk (one path per device
and inode), splits the groups by real size, by a hash of the first and last
16 KiB and, for files over 32 KiB, by a hash of the whole file. `Dupes`
keeps the groups; `arrange` puts the members of a group together where its
first member is in the order shown.

`hash.rs` hashes file contents: MD5, SHA-256 and CRC-32 for the hash
columns, and XXH3-128 to compare contents. A file is opened like a content
search opens it and read once for every hash asked for, in 256 KiB buffers
with a cancel check before each. A file is known by its `FileKey` (device,
inode, size, modification time): hard links share it, and a file that was
written to or replaced gets another. `HashCache` (`hashcache.rs`) keeps
hashes by that key in memory and in `$XDG_CACHE_HOME/everygrep/hashes.bin`
(no paths), up to a size limit (512 MiB by default, counted as the entries
take in the file); when it is reached, the entries used longest ago go first.

A `DetailCache` keeps the creation times or attributes one sort read, by
node id, for the next sort of the same results (the refresh after file
changes). The index logs every node whose details may have changed: new
metadata (`set_meta`, an `insert` of an existing name), renames and moves,
removals, and `note_changed` for an attribute change of a root, which only
its own watch reports. The log has sequence numbers; at the start of each
sort the cache drops the logged nodes it has values for. It forgets
all values when the log was reset (more than 65 536 entries, or a drive went
offline or came back, since a drive can change elsewhere) or when the index
is another one (`Index::stamp`, new after a compaction or a scan). The engine
uses the cache only while its watch loop applies changes, and also forgets it
on every id epoch, at the start of every watch session (a catch-up finds new
and removed entries, not changed attributes) and when events were lost.
Access times are never cached: reading a file changes them, and inotify's
`IN_ACCESS` is not watched. Entries on an offline drive are never read or
served, and entries that cannot be read are not kept, because a permission
change of a folder above makes them readable without an event of their own.
Values are kept for the current sort key only, and for at most twice as
many nodes as the last result (then only those of the listed nodes stay);
a creation time takes about 29 to 57 bytes, an attribute 10 to 21.

The values are only as current as the events applied: a change no event
reports stays unseen until the node changes again or the cache is dropped.
That is the case below a folder whose watch could not be added (the inotify
watch limit), for a change made through a hard link in another folder, and
for changes made by another machine on a network file system. The window's
own cache of the details of the visible rows is not affected: it reads them
again for every new result.

### Query language (`query.rs`, `fold.rs`, `filter.rs`, `kind.rs`)

`Query::parse` turns the text into a tree of AND, OR and NOT nodes over
predicates (text, wildcard, regular expression, extension, size, time,
file/folder, kind filter). A text predicate carries its prepared pattern and
its modes (case, diacritics, whole word, prefix, suffix, characters to leave
out); regular expressions are compiled once, when the query is parsed, and
shared by the search threads.
AND terms are ordered by a rough cost so cheap tests run first. The syntax is
described in [query-language.md](query-language.md). Dates use the local
time zone read by `localtime.rs` (TZif files and POSIX TZ strings).

### Scanning (`scan.rs`)

A scan walks each root with `readdir` and `lstat`. Symbolic links are indexed
but never followed. Mount points of virtual file systems (`proc`, `sysfs`,
`cgroup2`, `tracefs`, … read from `/proc/self/mountinfo`) are indexed but
not entered. The rules of `rules.rs` (excluded folders by path or pattern,
include-only and exclude file patterns) are compiled once into
`IndexRules` and asked about every entry before it is added, by the scan,
the catch-up and the watcher alike; an excluded folder gets no entry and is
not read. Name patterns compare bytes (`*.ext` is a suffix test); only
patterns that need it build the full path or run a regular expression.

### Watching (`watch.rs`, `inotify.rs`, `sync.rs`)

`Watcher::new` puts an inotify watch on every indexed folder. Events are read
in batches and applied to the index: creations add nodes, deletions remove
them, and a `MOVED_FROM`/`MOVED_TO` pair with the same cookie becomes a
rename that keeps the node id, so paths below a renamed folder stay correct.
A new folder is watched before it is read, so nothing created meanwhile is
lost. If the kernel queue overflows, the index is rebuilt by a scan and
gets a new watcher. When no watcher can be made (no inotify instance left),
the window's index thread shows the index as not watched and tries again
every 10 seconds, then catches up in full.

On start, `catch_up` compares the modification time of every indexed folder
with the saved one and re-reads only the folders that changed; folders that
vanished are removed with their subtree, new ones are scanned. A file whose
contents changed without touching its folder keeps its old size and time
until the next full scan. In a folder that can be listed but not searched
(no `x` permission), entries are added and removed by the names and types of
the listing, with unknown size and time, and the folder keeps its saved time
so the next catch-up reads it again.

The window's catch-ups (at start, when watching is turned on again, after a
cancelled rebuild, and for a drive that is connected again) use
`Watcher::catch_up_shared`: it holds the index's write lock for batches of
about 15 ms (`CATCH_UP_BATCH`) and lets readers in between, so drawing rows,
searching and saving on quit wait at most one batch. The folders still to
check are ids of the index, so only the index thread changes it meanwhile.
Quitting (or a new rebuild) ends a catch-up after the current batch; folders
not checked keep their saved times and are checked by the next catch-up.

While watching, removed slots and unused name bytes accumulate; when they
make up more than a quarter of the store, it is compacted in place and the
ids are renumbered (callers get the id map).

### Removable drives (`drives.rs`)

A drive counts as removable only when its mount point lies directly in
`/run/media/$USER` or `/media/$USER`, where the desktop's disk service
(udisks) mounts what is plugged in; in-memory and virtual file systems never
count. Any other mount in the mount table, including file systems mounted
from `/etc/fstab` under `/mnt`, is never treated as a drive (a test holds
this). A drive is named by the UUID of its file system: the link in
`/dev/disk/by-uuid` that points to the mount's source (compared as a path,
or by device numbers), and labelled from `/dev/disk/by-label`. So the same
disk is recognised wherever it is mounted.

`MountWatch` notices mount table changes: the kernel marks
`/proc/self/mountinfo` with `POLLPRI` on every mount and unmount, so a thread
can sleep in `poll` at no cost until then (it wakes once a second only to
check whether the program is ending). A table that is a plain file (a
simulated one) is read again once a second and compared.

What is known about a drive lives in the volumes folder
(`$XDG_CACHE_HOME/everygrep/volumes`): `<uuid>.bin`, its saved index in the
normal index format with the mount point as its root, and `<uuid>.info`, a
`key = value` text with its UUID, label, last mount point, the time it was
last connected and its number of entries.

Every place can be replaced for tests and demonstrations with environment
variables: `EVERYGREP_MOUNTINFO`, `EVERYGREP_MEDIA_DIRS` (`:`-separated),
`EVERYGREP_DISK_BY_UUID`, `EVERYGREP_DISK_BY_LABEL` and `EVERYGREP_VOLUMES`.

### Saved index (`persist.rs`)

See [index-format.md](index-format.md).

## Command line (`everygrep-cli`)

`everygrep index` scans and saves. `everygrep search <query>` loads the saved
index (scanning first when there is none, when `--root` differs from the saved
roots, or with `--rescan`) and prints matching paths in name order, at most
`--limit` (50 by default). `everygrep shell` reads one query per line from
standard input; with `--watch` it applies file changes before each query and
saves the index on exit. Timings and counts go to standard error. The
command line is English only.

## Window (`everygrep-gui`)

### Threads

| Thread | Work |
|---|---|
| UI (Slint event loop) | the windows, the result model, settings, file actions started by the user |
| search (`engine.rs`) | parses and runs the newest query, sorts it, hands the ids to the UI; for `content:` terms it waits for up to 8 file readers (scoped threads) |
| index (`engine.rs`) | loads or scans the index, catches up, then watches and applies changes |
| zbus | the session bus connection for the single instance |
| short-lived helpers | trash and permanent delete, the folder chooser, the export, the desktop color-scheme watcher |

The index sits behind a read-write lock. Searches and the result table take
short read locks; the index thread takes the write lock only while it
applies a batch of changes, never while it waits for events. Every query
gets a generation number; the search thread skips to the newest request and
results of older generations are dropped. An epoch number changes whenever
node ids change meaning (a compaction or a new scan), and ids from an older
epoch are never used.

The index thread also scans the folders to index again by itself when the
rescan schedule (`schedule.rs`: every N hours since the last full scan, or
when the local clock passes a time of day) says so; it looks at it at most
once a second while it watches, and not while watching is off.

The search thread is woken again when the index finishes loading and at most
every 500 ms while changes arrive, so the shown results stay current. The
index is saved at most every five minutes while it changes, after a rebuild
and when the program ends.

A query with `content:` terms takes the read lock only for its index stage
(`search_staged`), which copies the paths of the files to read. The files
are read with the lock released, so the watcher and the result table never
wait for a disk; 500 ms after the reading began and then at most once a
second (when new matches came) the search thread takes the lock again
briefly, sorts the matches so far and hands them to the window (the status
line shows "Searching file contents: 42%"). If node ids changed meaning
meanwhile, the query runs again. File changes do not interrupt the
reading: the search runs again once it is done, and the engine's
`ContentCache` (answers by term, path, size and time; dropped when the
content settings change) means only changed files are read again; a new
order is searched the same way. A cancel keeps the matches shown so far,
and file changes then leave them alone until the next search.

A query with a duplicate term runs the same way: its matches are found
(with the reading above for `content:` terms), grouped under the read
lock, and when contents are compared, the files are read with the lock
released ("Comparing files: 42%"), the hashes kept in the engine's
`HashStore` (`hashes.rs`). That store reads `hashes.bin` (next to the
index file) only when a search first needs it, and is saved with the
index and when the program ends. The matches are then sorted in the
window's order and `Dupes::arrange` keeps each group together.

### Result model (`results.rs`)

The hash columns (`hash_columns.rs`) work like Created and Accessed: a row
on screen asks a `HashReader` thread for the hashes of its file and shows
the cells empty until they come. The thread takes the smallest file asked
for first, reads it once for every hash column shown (or takes the hashes
from the engine's `HashStore`, which keeps them) and hands them over in
batches; a new list cancels the file being read. The frequency columns
read `Index::last_frequencies`; while one is shown, the engine has the
index count them (`Index::frequencies`, only when it changed) before each
sort, under the read lock it holds anyway.

The table holds only the list of matching ids. Rows (name split around the
match, folder, size, date, type) are built on demand when the list view asks
for them, so a million results cost no more than ten. The selection is kept
by node id (`selection.rs`), so it survives re-sorting and live updates.

### UI

The UI is written in Slint (`ui/app.slint`, `ui/settings.slint`,
`ui/theme.slint`) and compiled into Rust by `build.rs`, with the Fluent style
and the femtovg (OpenGL) renderer on the winit Wayland backend. Colors are
tokens in `theme.slint`; `look.rs` resolves the theme setting, and
`portal.rs` reads the desktop's light or dark preference
(`org.freedesktop.portal.Settings`) and opens the folder chooser and the
save dialog of the export (`org.freedesktop.portal.FileChooser`, `OpenFile`
and `SaveFile`). `export.rs` writes the export (CSV, TSV, text, M3U8, JSON, HTML) on a helper
thread, reading the index in chunks of 20 000 results under short read
locks; if the ids change meaning meanwhile (a compaction), it stops and
asks to export again.

### Commands and keys (`commands.rs`, `ui/commands.slint`)

Every command of the main window is one value of the `Command` enum in
`ui/commands.slint`, shared by Slint and Rust. `commands.rs` gives each a
stable id, a label and its default keys, and builds the `Keymap` in use. A
key is bound in a scope: anywhere in the window, the search field or the
result list. The search field, the list and a focus scope around the rest
of the window ask Rust which command a key runs (`resolve-key`); a binding
of the focused control's scope wins over a global one, and the search field
keeps the keys that edit text. The window's `can-run` and `run` functions
hold the conditions and the actions of every command in one place. The
help dialog lists the keys from the keymap. Movement keys, `Tab` and `Esc`
are fixed.

### Settings (`settings.rs`, `prefs.rs`)

Settings are a `key = value` text file in `$XDG_CONFIG_HOME/everygrep/settings`
that lists only values that differ from the defaults and keeps unknown keys.
A save reads the file, applies the change and writes it back while holding
an `flock` on `settings.lock` next to it, so two windows saving at once keep
both changes.
`prefs.rs` owns the running settings, applies each change at once and fills
the settings window. The file (and the search history) is written by a
`settings::Writer` thread, one save after another in the order asked, so a
slow disk or another window holding the lock never holds up the window; a
failed save is shown in the settings window when it comes, and quitting
waits up to 5 seconds for the saves still running. Every 2 seconds the same
thread looks at the file's inode, size and time; a version it has not seen
is read and handed to the window, which applies it (as an import, without
saving it again) when it differs from the running settings and no save was
asked for since it was read. Choosing other folders
or rules to index is saved at once, as before.

### Search history (`history.rs`, `search_history.rs`)

`history.rs` owns the bounded session navigation trail. `search_history.rs`
keeps distinct searches, saturating visit counts and UTC epoch timestamps.
The configuration-folder `search-history` file imports the earlier `history`
file once, without modifying it during normal updates. Explicit clear/disable
also removes that older copy. Previously unknown dates use the migration date.
Loading checks duplicates in linear time; at most 10,000 distinct searches survive.
Its disk operations run on the settings saving thread (`settings::Writer`), in the
order asked together with every settings save (not counted as settings saves, so
they never hold back applying a settings file changed outside the window), and the
last ones are waited for at quit; results return to the event loop with epoch and revision guards. Memory updates
immediately; stale disk snapshots cannot replace later local changes. No-op
updates skip writes.
Retention drafts apply only after the explicit Apply action. Every mutation reloads
the latest file under `search-history.lock`, prunes expired entries and replaces it via
`settings::create_temp`. The data file is private (0600). An unreadable or
malformed file is preserved and reported in Settings. An explicit Clear now
can replace a malformed file with an empty history.

`history_csv.rs` validates UTF-8 CSV with quoted multiline fields before a
merge. Its fixed header is `query,times_used,last_used_utc`; UTC dates use
`YYYY-MM-DDTHH:MM:SSZ`. A merge is idempotent (maximum count and last date).
The organizer uses a standard Slint table; its file chooser and CSV file I/O
run on a helper thread. A cancelled chooser changes nothing.

### Translations (`i18n.rs`, `translations/`)

English is the source language. Texts in the `.slint` files are marked with
`@tr()` and compiled in from gettext catalogs
(`translations/<lang>/LC_MESSAGES/everygrep-gui.po`) with Slint's bundled
translations; texts made in Rust use `tr()`/`trn()` and read the same
catalogs, embedded at build time. Switching the language selects the bundled
translation and sets the Rust-made texts again, so it applies at once.

### Single instance (`instance.rs`)

The first window for an index owns a name on the session bus:
`az.huseynz.EveryGrep` for the default roots and index file, or
`az.huseynz.EveryGrep.Index.x<hash>` for other roots or index files. A later
launch calls `Present` on interface `az.huseynz.EveryGrep.Window1` at
`/az/huseynz/EveryGrep` with its arguments and the activation token, then
exits. The running window applies the query, filter, modes and columns and
shows itself again.

When the call fails, the launch decides by the error (`next_step`, unit
tested):

| Answer | Next |
|---|---|
| reply | exit (handed over) |
| `az.huseynz.EveryGrep.Error.ShuttingDown`: the window's event loop has ended and it is saving the index (`set_closing`); the name is released when the process exits | wait up to 1.5 s for the name to be released and claim it; still held, show the dialog |
| `ServiceUnknown` / `NameHasNoOwner`: the owner went away meanwhile | claim the name again, at most 3 times, then run without a name |
| no reply within 5 seconds (or the bus's `NoReply`) | show the dialog |
| anything else | run without a name, as before |

The dialog (`stalled.rs`, `ui/stalled.slint`) runs before the main window,
in its own pass of the event loop. It says the window is still shutting
down or does not answer, polls `NameHasOwner` every 200 ms and closes by
itself once the name is free, so the launch claims it and opens its
window. "Terminate" sends `SIGTERM` to the owner's process
(`GetConnectionUnixProcessID`), only when `/proc/<pid>/comm` equals this
program's and the process is not this one; after 5 seconds without an end
the dialog says it timed out and offers only Close. Cancel or Close ends
the launch. A launch never opens a second window next to one that is
closing. `--new-instance` skips the bus; without a session bus every
launch opens its own window. A `--toggle` launch adds `toggle = true` to
the platform data; the running window then closes itself when it is in
front, and an older window that does not know the key simply shows itself.
A `--new-window` launch that finds the name taken runs without a name
instead of calling `Present`. A `--background` launch (from the autostart
entry, see `autostart.rs`) that finds the name taken exits without calling
`Present`; owning the name, it skips showing the window when closing only
hides it.

### Starting on login (`autostart.rs`)

The setting is the file `$XDG_CONFIG_HOME/autostart/az.huseynz.EveryGrep.desktop`
(a relative `XDG_CONFIG_HOME` is ignored, as the base directory
specification says), written atomically with
`Exec=<current executable> --background` and
`X-GNOME-Autostart-enabled=true`. The path is quoted by the Desktop Entry
rules: an argument with a reserved character goes in double quotes with
`"`, `` ` ``, `$` and `\` escaped, `%` is doubled, and the string escapes
(`\\`, `\n`, `\t`) are applied on top. A path that is not UTF-8 is refused.
Nothing else is stored, so the switch is the file's existence, unless its
`[Desktop Entry]` group says `Hidden=true` or `X-GNOME-Autostart-enabled=false`
(another tool turned it off); turning the switch on writes the file again.

### Removable drives in the window (`engine.rs`)

Drives are roots of the one index, so searching, sorting and the result
table need nothing special. The engine keeps a list of known drives (from
the `.info` files) with the name of each drive's root in the index and
whether it is mounted.

- **Start.** After the index file is loaded (or scanned), every known
  drive's saved index is grafted in and marked offline. A root name that is
  taken already gets ` (<uuid>)` added.
- **Mount changes.** A `mounts` thread sleeps in `MountWatch` and sets a
  flag; the index thread looks at it every 250 ms and brings the drives in
  line with the mount table: a drive that went away is unwatched and marked
  offline, and its entries are saved to its own file (or, with "forget",
  removed and its file deleted); a known drive that is back is renamed to
  its new mount point if it moved, marked online, watched and caught up; a
  new one is read on a helper thread and grafted when it is complete, then
  caught up for the changes made meanwhile.
- **Saving.** The index file gets only the folders to index
  (`Index::extract`), each connected drive its own file; a drive that is
  not connected was saved when it went away.
- **Rebuilds.** A new scan of the folders to index (changed folders, a
  queue overflow) carries the drives over from the old index.

Requests from the settings window (forget a drive, stop or start indexing
it) are queued and carried out on the index thread. File actions are
refused for rows on an offline root (`Index::is_offline`): its paths may
now belong to another disk mounted at the same place, or to nothing.

### File actions (`actions.rs`, `open.rs`)

Opening runs `xdg-open`. The containing folder is shown with the items
selected through the file manager's `org.freedesktop.FileManager1.ShowItems`
(called with `gdbus`), falling back to opening the parent folder. Move to trash runs `gio trash`; permanent
delete removes files and folders directly (links as links). Renames are
checked first (empty names, `/`, `.` and `..`, too long, existing names).
The index is not touched by these actions: the watcher sees the change on
disk and updates it like any other change.

### Named filters (`saved_searches.rs`, `saved_ui.rs`)

The main selector and Search menu use one ordered collection of named filters.
Each retains a core `Filter` kind and a query with independent `QueryOptions`.
The engine intersects the two parsed expressions with `Query::and`, preserving
path requirements, highlighting and the smaller result limit. The versioned
`saved-searches` file lives beside settings; every mutation reloads it under
its sibling lock and uses `settings::create_temp`, mode 0600, sync and rename.
Stable numeric identities prevent stale edits from recreating removed items.
Restore replaces built-ins in their original order and retains user entries.
The editor exposes hidden visibility and all parse flags. Built-in requests check
against the original record, using a temporary snapshot if it was edited/deleted.
Query validation is lenient on load and strict on save; manager rows mark invalid
queries. Structural corruption requires explicit recovery: a unique mode-0600
backup is synced before an atomic reset. The latest file is checked under the
lock so a stale recovery dialog cannot reset an already repaired library.

Bookmarks share the same file and identity allocator. Each stores field text,
complete query options, a filter snapshot and sort column/direction. A snapshot
that differs from its live filter becomes a transient entry in the existing
selector; the manager continues to edit only the persisted filter collection.
Reloading or editing the library preserves the selected identity and respects
search-as-you-type when its effective filter changes.

Optional item shortcuts are separate records in the same library transaction.
They are global, checked against every scope of the active keymap and all
default command keys, and cannot use typing/navigation keys. Keyboard settings
checks library ownership before applying a command binding. Commands retain
priority if an external settings edit introduces a collision. Deleted entries
lose their keys; restoring filters clears built-in keys and keeps custom keys.

### Advanced Search (`advanced.rs`, `advanced_query.rs`)

The session form produces existing syntax only. Word alternatives are explicitly
grouped, operators in fields are quoted, and quotes/control characters use the
same regex escaping as context searches. Those regex terms follow the engine's
regex matching rules. Location queries share its boundary-safe implementation.
Size units become exact byte comparisons with checked arithmetic; dates validate
calendar days before emitting inclusive dm/dc/da comparisons. A full engine
parse validates the output before changing the field. Filter choices are held
by value, so a concurrent organizer edit cannot make an index select another
filter. No form data is persisted and no content query is generated.

### Search tabs (`tabs.rs`, `tabs_ui.rs`, `ui/tabs.slint`)

Rust owns the tab list and ten closed search records. One `ResultsModel` and
one engine search serve the active tab. Switching freezes Arc-backed ids,
highlight terms and selection; inactive tabs do not submit work. Restoring a
snapshot checks the engine's id epoch before showing it, then submits a fresh
search. Old queued responses still go through the generation check. Query,
filter references, matching toggles, sort, column layout (hash and frequency
columns and Folder's own width too), navigation trail, cursor and both scroll
positions belong to the tab; the sideways one is kept within the table's room
when the tab is shown. A content or duplicate search the user stopped stays stopped
when its tab is shown again (`Engine::show_stopped`): its matches so far come back
from the tab and no file is read again, until the user asks for a search, the
order changes or the index's ids change. A tab whose complete results were shown keeps the index
version they were found in (`Engine::settled_search`); shown again with the same
request while the index has that version and the same ids, its frozen results are
the answer and no search runs (`Engine::show_settled`), so a duplicate or content
search and the frequency counts are not done again for nothing. The check never
waits for the index lock: while the index is being changed the tab searches again.
A frequency column the tab shows and the one before did not also searches again,
to count the index. Enter-pending edits keep their
last submitted search separately. Hidden-file mode and the three parse policies
follow Settings across all tabs. Filters resolve by live identity, falling back
to their snapshot only after deletion; bookmarks retain their explicit snapshots.
Closed tabs release result arrays. No-op navigation does not reinstall a search.

A custom horizontal strip appears for two or more tabs; the list dialog and
Tabs menu make every overflowed tab reachable. A 250 ms observer sees changes
made by history, bookmarks and settings without additional search callbacks or
copying result arrays. It updates titles only when search/layout state changed.

### Tab sessions (`session.rs`)

`session` lives beside settings/history, using escaped `key = value` fields
and a version header. It stores a single window's search states and active tab,
plus up to ten closed searches; result ids and selection never go to disk.
Columns are named, not positional: `columns = type md5 …` lists the optional
columns shown and `widths = name:350 folder:420 …` the widths set (hash and
frequency columns and Folder's own width included); names a later version adds
are skipped. `sort` accepts every column the table sorts by (0–8, 12–14).
Index identity gates restoration. Explicit searches and explicit extra windows
skip restoring. Restore, history recording and history disk storage must all be
enabled for reading/writing. Disabling deletes the session. Clear/delete also
invalidate the writer cache and disk file; runtime tombstones redact those
queries from later captures without destroying visible open-tab work.
The window only captures the tabs; the saves and deletions run on the settings
saving thread (`session::Saver`), in order with the settings and history saves, a
newer snapshot replacing one still waiting, and are waited for at quit.
Each save holds `session.lock`, validates existing data, creates a unique file
with `settings::create_temp`, sets 0600, syncs and renames it. Malformed/newer
files are preserved. Readers are bounded to 8 MiB / 1,000 records, skip unknown
keys and use the last duplicate field. Active indexes are clamped and closed
records pruned. Each window writes a complete snapshot; the last one to exit
wins, without merging independent windows.

The title observer captures cheap search states for persistence. `Writer` uses
a fixed 15-second save window: a leading write, then one trailing write if
changed; edits never restart it. Normal exit captures and writes once more.
Restore submits only the selected tab, leaving background searches dormant.

### Transfer questions (`fileops_conflict.rs`)

The transfer resolver posts a question to the UI event loop, then waits on a
channel with cancellation checks every 100 ms. Only the worker waits. The UI
answers through a modal, and disconnect or cancel stops the paste. The resolver
is optional; default transfers still select a free numbered name. Overwrites
copy to a private temporary name and recheck destination device, inode, size,
mtime and ctime before moving the old file to the desktop trash and publishing
with a strict non-replacing rename. Trash capability is queried on the worker;
without it Overwrite is disabled. A trash failure preserves the destination.
Questions show both sizes and modification times; results count replacements.

### File undo (`fileops_undo.rs`, `fileops_undo_ui.rs`)

An opt-in, bounded session journal records successful transfer pairs and
renames. One paste is one step, containing its supported successful items;
undo visits the members in reverse order and redo in source order. The history
limit counts whole operations. If a new group would exceed the tree snapshot
budget, none of that group is recorded and older applied history is kept;
redo is invalidated and the result warns about omitted history. Each item carries a sorted, non-following tree snapshot (relative
paths, device, inode, byte size, mode and nanosecond modification time). Undo
checks the snapshot and destination vacancy on a worker, then uses strict
`RENAME_NOREPLACE`; unsupported filesystems fail safely. Copy undo uses the
desktop trash, while redo creates an exact-path copy from a verified source.
It never relies on discovering a trash identifier. Cross-filesystem moves,
overwrites and destructive deletion are excluded. No history goes to disk.
The settings writer persists only the enable choice and history limit.

The journal and this program's destructive operations are serialized on
workers. History snapshots, CSV export, rename submission and all tree
traversal run off the UI thread. Existing result helpers also take brief index
read locks on the UI thread when preparing result context menus.
History rows carry stable operation IDs; filter refreshes have a revision guard
so stale responses cannot retarget a selected step. Recreating a copy updates
the recorded identity of a later pending rename of that same copy. An identity
check cannot eliminate changes made by other processes between the check and
the filesystem syscall; strict destination creation still never overwrites.

### Typed context searches (`fileops_value.rs`)

A row's local x coordinate identifies the clicked column after resizing or
horizontal scrolling. `ResultsModel::value_query` takes one index read and
builds from raw bytes, indexed sizes and the model's cached local zone. It
performs no filesystem reads and never parses translated or rounded cell text.
The UI captures the query and matching/filter options before showing the menu.
Unsupported or unavailable values omit that menu entry. Date values explicitly
search a local day; folder values use exact direct-child path boundaries.

Filename and stem commands use byte-safe literal regexes. Stem expressions
separate files with a last extension, files without one, and folders; a word
occurring only in the extension never satisfies a stem search. All context
commands share one routing function for history, tabs and new search windows.
