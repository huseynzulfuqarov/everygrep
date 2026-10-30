# Architecture

EveryGrep keeps the name of every file and folder below a few root folders in
memory, searches them on all CPU cores as you type, and keeps them current
with inotify. Nothing leaves the computer: EveryGrep makes no network
connections, has no telemetry and runs no service besides the window itself.

## Crates

| Crate | Kind | Role |
|---|---|---|
| `everygrep-core` | library, no dependencies | index, scanner, watcher, query language, search, sorting, saved index |
| `everygrep-cli` | binary `everygrep` | `index`, `search` and `shell` commands over the core |
| `everygrep-gui` | binary `everygrep-gui` | the window: Slint UI, settings, single instance, file actions |

The core uses only the standard library and hand-written bindings for the few
Linux calls it needs (`inotify.rs`). The window adds Slint (UI toolkit) and
zbus (session bus); see `THIRD-PARTY-LICENSES.md` for the full list.

## Core

### Node store (`store.rs`)

Nodes live in parallel columns indexed by a `NodeId` (u32):

- `parent_kind`: the parent id + 1 (0 for a root), a folder bit and a
  removed bit, packed in one u32;
- the name as a range of a shared byte pool, and a case- and
  diacritic-folded copy of it for matching, stored only when it differs
  from the raw name;
- size and modification time (nanoseconds);
- first-child and sibling links, so a subtree can be removed in time
  proportional to its size;
- for folders, the total size of all files below and the number of direct
  children, updated on every change by walking up to the root.

A root's name is its whole path; other names are single components. Removed
nodes keep their slot (marked removed) until the store is compacted.

### Index and search (`index.rs`, `order.rs`, `search.rs`)

The index is the store plus all live node ids sorted by folded name
(`order.rs`): a list of sorted blocks (built with 512 ids, split above 1024),
so inserts and removals while watching only shift one block.

`search()` splits the blocks into groups of 16 and lets worker threads (one
per CPU by default) take groups one by one; each evaluates the parsed query
against every node and the results are concatenated in name order. Text
matching works on the folded names, so it never allocates or folds per
comparison. `sort.rs` reorders results by another column (path, size,
modified, type, extension) with a stable sort, so equal keys keep name order.

### Query language (`query.rs`, `fold.rs`, `filter.rs`, `kind.rs`)

`Query::parse` turns the text into a tree of AND, OR and NOT nodes over
predicates (text, wildcard, extension, size, time, file/folder, kind filter).
AND terms are ordered by a rough cost so cheap tests run first. The syntax is
described in [query-language.md](query-language.md). Dates use the local
time zone read by `localtime.rs` (TZif files and POSIX TZ strings).

### Scanning (`scan.rs`)

A scan walks each root with `readdir` and `lstat`. Symbolic links are indexed
but never followed. Mount points of virtual file systems (`proc`, `sysfs`,
`cgroup2`, `tracefs`, … read from `/proc/self/mountinfo`) and excluded
folders are indexed but not entered.

### Watching (`watch.rs`, `inotify.rs`, `sync.rs`)

`Watcher::new` puts an inotify watch on every indexed folder. Events are read
in batches and applied to the index: creations add nodes, deletions remove
them, and a `MOVED_FROM`/`MOVED_TO` pair with the same cookie becomes a
rename that keeps the node id, so paths below a renamed folder stay correct.
A new folder is watched before it is read, so nothing created meanwhile is
lost. If the kernel queue overflows, the index is rebuilt by a scan.

On start, `catch_up` compares the modification time of every indexed folder
with the saved one and re-reads only the folders that changed; folders that
vanished are removed with their subtree, new ones are scanned. A file whose
contents changed without touching its folder keeps its old size and time
until the next full scan.

While watching, removed slots and unused name bytes accumulate; when they
make up more than a quarter of the store, it is compacted in place and the
ids are renumbered (callers get the id map).

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
| search (`engine.rs`) | parses and runs the newest query, sorts it, hands the ids to the UI |
| index (`engine.rs`) | loads or scans the index, catches up, then watches and applies changes |
| zbus | the session bus connection for the single instance |
| short-lived helpers | trash and permanent delete, the folder chooser, the desktop color-scheme watcher |

The index sits behind a read-write lock. Searches and the result table take
short read locks; the index thread takes the write lock only while it
applies a batch of changes, never while it waits for events. Every query
gets a generation number; the search thread skips to the newest request and
results of older generations are dropped. An epoch number changes whenever
node ids change meaning (a compaction or a new scan), and ids from an older
epoch are never used.

The search thread is woken again when the index finishes loading and at most
every 500 ms while changes arrive, so the shown results stay current. The
index is saved at most every five minutes while it changes, after a rebuild
and when the program ends.

### Result model (`results.rs`)

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
(`org.freedesktop.portal.Settings`) and opens the folder chooser
(`org.freedesktop.portal.FileChooser`).

### Settings (`settings.rs`, `prefs.rs`)

Settings are a `key = value` text file in `$XDG_CONFIG_HOME/everygrep/settings`
that lists only values that differ from the defaults and keeps unknown keys.
`prefs.rs` owns the running settings, applies each change at once and fills
the settings window.

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
shows itself again. A window that does not answer within 5 seconds is
treated as gone. `--new-instance` skips the bus; without a session bus every
launch opens its own window.

### File actions (`actions.rs`, `open.rs`)

Opening runs `xdg-open`. The containing folder is shown with the items
selected through the file manager's `org.freedesktop.FileManager1.ShowItems`
(called with `gdbus`), falling back to opening the parent folder. Move to trash runs `gio trash`; permanent
delete removes files and folders directly (links as links). Renames are
checked first (empty names, `/`, `.` and `..`, too long, existing names).
The index is not touched by these actions: the watcher sees the change on
disk and updates it like any other change.
