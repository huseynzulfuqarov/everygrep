# Changelog

All notable changes to EveryGrep are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

First version, 0.1.0, not released yet.

### Index and engine

- In-memory index of file and folder names in packed columns, with names
  stored once and a case-folded copy for matching.
- Parallel search over the name-sorted order on all CPU cores.
- Directory scanner that never follows symbolic links and does not enter
  virtual file systems.
- Index rules: excluded folders by exact path or pattern, "include only" and
  "exclude" file patterns (wildcards with `*`, `**`, `?`, or `regex:`),
  matched against the name or, with a `/`, the full path; compiled once and
  applied the same way by scans, catch-ups and live updates.
- Live updates with inotify: creations, deletions, renames and moves keep the
  index current; a queue overflow rebuilds it.
- Catching up on start with changes made while nothing was watching, by
  re-reading only folders whose modification time changed.
- In-memory compaction during long watch sessions.
- Saved index (format version 4) with full validation and a checksum,
  written atomically, decoded while it is read.
- Total size and item count of every folder, kept current on every change.
- Sorting by name, folder, size, modification time, type or extension.
- Creation time (birth time, where the file system keeps it), access time and
  attribute letters (`R` read-only, `H` hidden, `D` directory, `L` link,
  `X` executable) read from the file system on demand, so the index stays
  the same size; sorting by them reads them for every result on several
  threads, with progress and cancel.
- Progress counters and cooperative cancelling for scans, searches, sorts
  and catch-ups, and an estimate of a scan's size from the used inodes of
  the file systems it reads.

### Query language

- AND (space), OR (`|`), NOT (`!`), grouping (`< >`) and quoted literals.
- Wildcards `*` and `?` matched against the whole name. **Behaviour
  change:** against the full path `*` and `?` no longer cross a `/`; `**`
  does (`path:/home/me/*.txt` no longer finds files in subfolders), as in the
  exclude patterns.
- Case, whole-word, full-path and diacritic modes, per term (`case:`, `wholeword:`,
  `path:`, `diacritics:` and their `no…:` forms) or for the whole query.
- `file:`, `folder:`, `ext:`, `size:` (folders by total size) and
  `modified:` with ages, local days, months, years and inclusive ranges.
- Filters for audio, archives, code, documents, executables, folders,
  images and videos, from the name. The kinds know more extensions (for
  example `.au`, `.chm`, `.mht`, `.vsd`, `.jxr`, `.wmf`, `.m2t`, `.vob`).
  **Behaviour change:** `.desktop`, `.bat`, `.cmd` and `.ps1` are now
  executables instead of code.
- Prefix and suffix modes (`prefix:`, `suffix:`) that anchor a term at the
  start or end of the name, also for wildcard patterns.
- Modes that ignore punctuation (`nopunct:`) or whitespace (`nospace:`) in
  names and terms.
- Filter words in the query, the filter names with a colon: `audio:`,
  `archive:`, `code:`, `document:`, `executable:`, `image:` and `video:`,
  `video:` and `code:`, sharing the extension lists of the filter selector
  and combining with other terms (`image: beach`, `!video:`,
  `<audio:|video:>`).
- The executable filter (and `executable:`) also finds files of no known type that
  have an execute bit, looked up on disk while searching.
- `ext:` lists separated by `;` or `|` (`ext:jpg|png` is one list, no
  longer `ext:jpg` OR the text `png`), with wildcard entries (`ext:jp*`).
- Size grammar: units `kib`, `mib`, … next to `kb`, `mb`, …; `==`, `!=`,
  `<>`, `=>`, `=<`, `!`; ranges written either way round; size words
  `empty`, `tiny`, `small`, `medium`, `large`, `huge`, `gigantic`.
  **Behaviour change:** a size with a unit now stands for a range up to the
  next step of its last digit (`size:1mb` = 1 MiB to 2 MiB − 1, which used to
  match only files of exactly 1 048 576 bytes); `size:=1mb` is exact.
- Regular expressions with `regex:` or for the whole query in regex mode,
  compiled once per query; case is ignored unless matched, invalid
  expressions are reported in one line.

### Command line

- `everygrep index`, `search` and `shell` (with `--watch`), reusing the saved
  index and rescanning when the roots change.
- `--exclude` takes a path or a pattern; `--include-files` and
  `--exclude-files` take file pattern lists.

### Window

- Search window with results on every key press, matched terms in bold
  (every occurrence of every term, wildcards, regular expressions, path terms
  also in the Folder column),
  file-type icons and sortable, resizable columns (Name, Folder, Size,
  Modified, and optional Type, Extension, Created, Accessed and
  Attributes).
- Edit → Invert selection (`Ctrl+I`), which costs the same for any number
  of results.
- File → Export results… (`Ctrl+S`): the results as CSV (name, folder,
  size, modified, offline) or as a list of full paths, through the
  desktop's save dialog, written atomically.
- Selection with the mouse and keyboard; open, open the containing folder,
  copy names or paths, rename in place, move to the trash and delete
  permanently, from keys, a context menu and the menu bar.
- Filter selector and matching mode toggles, search history with
  `Alt+←`/`Alt+→`, initial query and options from the command line.
- Regular expression toggle (`Alt+R`) and Search menu entries with check
  marks for match prefix (`Alt+B`), match suffix (`Alt+E`), ignore
  punctuation (`Alt+U`) and ignore whitespace (`Alt+S`), with status
  badges, start defaults in the settings and command line flags.
- One window per index: a later launch hands its query to the open window
  over the session bus; GNOME keyboard shortcut instructions.
- Settings window: folders to index and exclude, start options, history,
  closing the window (quit or hide), remembered columns, folder size
  (total, item count or nothing), theme, density, selection details, menu bar
  and language; saved in a small text file.
- Indexing settings: a switch for the exclude list, folder patterns next to
  the folder chooser, and "include only files" / "exclude files" pattern
  lists. A pattern that does not compile is refused with a message and not
  saved; a change indexes again with progress and Cancel.
- "Watch for changes" setting: off removes every inotify watch and the
  status line says changes are not watched; on again catches up with what
  changed meanwhile and watches again.
- "Search as you type" setting: off, changes of the search text and the
  matching options wait for Enter (the status line says so), and Enter
  opens the current row once the results are up to date.
- Setting to select the whole name, extension included, when renaming
  (by default only the part before the extension is selected).
- "Start EveryGrep when I log in" setting: an XDG autostart entry that
  runs the program with `--background`, which keeps the index loaded
  without a window when closing hides it (the window opens otherwise).
- A launch that finds the open window still shutting down or not
  answering no longer opens a second window or waits silently: it shows a
  dialog that offers to terminate the existing instance and continues by
  itself once the old window has ended.
- `--toggle` (show the window, or close it when it is in front) and
  `--new-window` (another window over the same index) for desktop
  shortcuts; the Keyboard shortcut page lists the three commands with Copy
  buttons.
- Setting to keep the focus where it was when a later launch shows the
  window (by default the search field is focused and its text selected).
- Light and dark themes following the desktop, compact density, details of
  the selection in the status line.
- Menu bar with keyboard shortcuts and about dialogs; `⋯` menu when it is
  hidden.
- Removable drives mounted by the desktop are indexed automatically, each
  into its own file and recognised by file system UUID. A removed drive's
  entries stay searchable, dimmed and with file actions refused, or are
  forgotten, as a setting says; plugged in again they are caught up and
  watched. Settings → Indexing lists the known drives with Forget and
  Don't index this drive. Mounts are noticed through `POLLPRI` on the mount
  table at no idle cost; fstab mounts are never indexed this way.
- Translations into Azerbaijani and Turkish, with a language setting that
  applies at once; translated desktop entry.
- Cut, copy and paste of files (`Ctrl+X`, `Ctrl+C`, `Ctrl+V`) into a
  folder of the results: never replaces (a taken name becomes
  `name (2).ext`), copies through a temporary name, moves across file
  systems by copying and then removing only what was copied, with progress
  and Cancel in the status line. Other programs get the files through the
  XWayland clipboard (`x-special/gnome-copied-files`, `text/uri-list` and
  the paths as text), and files cut or copied in a file manager paste here;
  without XWayland, the paths as text. The name moved to `Ctrl+Alt+C`.
- Copy path: the containing folders (`Ctrl+Alt+Shift+C`), next to copy
  name and copy full path.
- Open with… (`Shift+Enter`): the desktop's application chooser, through
  the OpenURI portal.
- Progress of long operations in the status line ("Indexing: 42%",
  rescanning, reading a drive, catching up, searching, sorting), shown
  after 300 ms and updated ten times a second; a count instead of a
  percentage when the file systems give no estimate (btrfs). A Cancel button
  stops indexing, rescans, a drive's first reading, searches and sorts; a
  cancelled first index is never saved, a cancelled rescan keeps the
  previous index and folders, a cancelled sort keeps the previous order.

### Project

- Licensed under MIT OR Apache-2.0; original icons and app icon.
- UI design guidelines, architecture, index format and query language
  documentation.
- Generated list of third-party crates and their licenses.
- Arch Linux PKGBUILD.

