# Changelog

All notable changes to EveryGrep are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

First version, 0.1.0, not released yet.

### Index and engine

- In-memory index of file and folder names in packed columns, with names
  stored once and a case-folded copy for matching.
- Parallel search over the name-sorted order on all CPU cores. Wildcard
  terms against full paths are compiled once per search and matched in
  reused buffers, skipping parts of the path no match can reach.
- Directory scanner that never follows symbolic links and does not enter
  virtual file systems.
- Index rules: excluded folders by exact path or pattern, "include only" and
  "exclude" file patterns (wildcards with `*`, `**`, `?`, or `regex:`),
  matched against the name or, with a `/`, the full path; compiled once and
  applied the same way by scans, catch-ups and live updates.
- Live updates with inotify: creations, deletions, renames and moves keep the
  index current; a queue overflow rebuilds it. A file created just before
  its folder is renamed is kept; a rename whose two halves are read in two
  batches stays a rename; a folder that shows up twice (a bind mount inside
  the indexed folders) is kept current at both places; a file system
  unmounted inside the indexed folders is read again at its mount point.
  A folder's time is saved only once the events before it were applied, so
  a catch-up after an exit finds what was not applied yet.
- Folders that could not be watched (watch limit reached) are tried again
  and checked for changes every 10 seconds; the status line counts them.
- Catching up on start with changes made while nothing was watching, by
  re-reading only folders whose modification time changed. Watching
  command line shells and the window also catch up after fresh scans and
  rescans, with changes made before their watches were installed.
- Failed directory listings are retried after access returns; saved roots
  that were absent during scanning are revisited on load. A folder that can
  be listed but not searched still gets its added and removed entries by
  name.
- In-memory compaction during long watch sessions.
- Saved index (format version 6) with full validation and a checksum,
  decoded while it is read. It is written to an exclusively created
  temporary file with owner-only permissions (removed when the save fails),
  synced and renamed over the old file, and then its folder is synced;
  folder sync failures are reported, except on file systems that cannot
  sync a folder (`EINVAL`, `ENOTSUP`). A name order sorted by older rules is
  sorted again instead of being scanned anew.
- Saved indexes record a fingerprint of the indexing rules. The command line
  and the window build the index again when the include/exclude rules
  changed or the file has none (versions 4 and 5); the window saves its
  index and each drive's with their rules, and reads a drive again when its
  file has other rules.
- Total size and item count of every folder, kept current on every change.
- Sorting by name, folder, size, modification time, type or extension.
  Names sort in natural order: numbers by value (`file2` before `file10`),
  case and diacritics ignored (`Çay` next to `cay`, `Ölçü` among the o's
  instead of after z), `ə` as a letter after `e`, hyphens and apostrophes
  skipped.
  The same name in several folders sorts by folder path (also as the tie
  order of the other columns), not by when the folders were indexed; so do
  names that sort as equal (`Çay` and `cay`, `a-b` and `ab`), then by their
  exact spelling. Folder paths sort naturally too (`dir2` before `dir10`).
- Creation time (birth time, where the file system keeps it), access time and
  attribute letters (`R` read-only, `H` hidden, `D` directory, `L` link,
  `X` executable) read from the file system on demand, so the index stays
  the same size; sorting by them reads them for every result on several
  threads, with progress and cancel. While file changes are watched, the
  creation times and attributes read for a sort are kept for the refreshes
  that follow, and only entries that changed are read again (about 5 ms
  instead of 20 ms, and a tenth of the CPU time, for 100 000 results);
  access times are always read again, since reading a file changes them.
  A permission change of a folder, or a move of a folder into another one,
  makes all entries below it read again.
- Progress counters and cooperative cancelling for scans, searches, sorts
  and catch-ups, and an estimate of a scan's size from the used inodes of
  the file systems it reads.
- A new query or result order stops the search or sort still running for
  the previous one at once, instead of waiting for it to finish; this is not
  reported as a stop.

### Query language

- AND (space), OR (`|`), NOT (`!`), grouping (`< >`) and quoted literals.
  Nesting deeper than 100 levels is a query error instead of a crash.
- Settings → Search can enable standalone uppercase `AND`, `OR`, `NOT`
  operators. The default keeps these words literal; quoted words and
  modifier values stay literal when the setting is enabled.
- Settings → Search can evaluate AND before OR. The default retains
  OR-before-AND behavior; explicit groups and negation keep their meaning.
- Wildcards `*` and `?` matched against the whole name; quotes make only
  the `*` and `?` inside them literal (`*"draft report"*`), where one quoted
  part used to turn off every wildcard of the term. **Behaviour
  change:** against the full path `*` and `?` no longer cross a `/`; `**`
  does (`path:/home/me/*.txt` no longer finds files in subfolders), as in the
  exclude patterns. A byte that is not valid UTF-8 is one character for
  `?` and `*`; a broken sequence no longer swallows the `/` after it.
- Wildcard scope is configurable in Settings → Search, with per-term
  `wholename:` / `nowholename:` overrides; whole-name matching
  remains the default. `wholename:` on text
  without wildcards matches the whole name exactly.
- Case, whole-word, full-path and diacritic modes, per term (`case:`, `wholeword:`,
  `path:`, `diacritics:` and their `no…:` forms) or for the whole query.
- `file:`, `folder:`, `ext:`, `size:` (folders by total size) and
  `modified:` with ages, local days, months, years and inclusive ranges.
  Modifiers before `file:`, `folder:` or a filter word apply to its term
  (`case:file:README`, `prefix:folder:src`); they used to be dropped.
- Filters for audio, archives, code, documents, executables, folders,
  images and videos, from the name. The kinds know more extensions (for
  example `.au`, `.chm`, `.mht`, `.vsd`, `.jxr`, `.wmf`, `.m2t`, `.vob`).
  **Behaviour change:** `.desktop`, `.bat`, `.cmd` and `.ps1` are now
  executables instead of code.
- Prefix and suffix modes (`prefix:`, `suffix:`) that anchor a term at the
  start or end of the name, also for wildcard patterns.
- Modes that ignore punctuation (`nopunct:`) or whitespace (`nospace:`) in
  names and terms.
- Dates: `tomorrow` and this, last and next week (Monday to Sunday), month
  and year (`modified:lastweek`, `modified:>=thismonth`); `created:` and
  `accessed:` with the same values, read from disk while searching.
- Dates that end or start now: `last3days`, `next2hours`, `next-1-day`, in
  years, months, weeks, days, hours, minutes and seconds. Their start or end keeps
  the wall clock time also on a day with a daylight saving change (it used
  to be off by an hour there).
- Weekday and month names in dates (`modified:monday`, `modified:fri`, `modified:october`,
  `modified:dec`: the latest such day or month).
- An age without a comparison, `modified:7d`, means within the last 7 days, like
  `modified:<7d`; it used to be an error. Ages end now: a time in the future no
  longer matches `modified:7d` or `modified:<7d`, as with `modified:last7days`.
- `limit:100` keeps only the first 100 results, after sorting in the window
  and in name order on the command line.
- Filter words in the query, the filter names with a colon: `audio:`,
  `archive:`, `code:`, `document:`, `executable:`, `image:` and `video:`,
  `video:` and `code:`, sharing the extension lists of the filter selector
  and combining with other terms (`image: beach`, `!video:`,
  `<audio:|video:>`).
- The executable filter (and `executable:`) also finds files of no known type that
  have an execute bit, looked up on disk while searching. Entries of offline
  drives are not looked up for it or for `created:` / `accessed:`, so a
  different drive mounted at the same folder cannot answer for them.
- `ext:` lists separated by `;` or `|` (`ext:jpg|png` is one list, no
  longer `ext:jpg` OR the text `png`), with wildcard entries (`ext:jp*`).
  `ext:` follows `case:` and the match case mode (`case:ext:JPG` finds
  only upper-case `.JPG`); by default it still ignores case.
- Size grammar: units `kib`, `mib`, … next to `kb`, `mb`, …; `==`, `!=`,
  `<>`, `=>`, `=<`, `!`; ranges written either way round; size words
  `empty`, `tiny`, `small`, `medium`, `large`, `huge`, `gigantic`.
  **Behaviour change:** a size with a unit now stands for a range up to the
  next step of its last digit (`size:1mb` = 1 MiB to 2 MiB − 1, which used to
  match only files of exactly 1 048 576 bytes); `size:=1mb` is exact.
  Sizes above the largest file size (2^64 − 1 bytes) are a query error.
- Regular expressions with `regex:` or for the whole query in regex mode,
  compiled once per query; case is ignored unless matched, invalid
  expressions are reported in one line.

### Command line

- `everygrep index`, `search` and `shell` (with `--watch`), reusing the saved
  index and rescanning when the roots change.
- Its own index file, `cli-index.bin`, next to the window's `index.bin`, so
  the two programs never replace each other's index.
- `--exclude` takes a path or a pattern; `--include-files` and
  `--exclude-files` take file pattern lists.
- `--no-meta` stays in effect during catch-up and live updates, renames and
  moves included; folder times are kept, so unchanged folders are skipped.
- `everygrep index` fails when the index cannot be saved; `search` and
  `shell` print a warning and still answer.
- Root, index and excluded-folder paths that are not UTF-8 are kept as they
  are; text arguments that are not UTF-8 are usage errors instead of a
  panic.

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
- Active matching-mode badges can be shown or hidden in Settings → Search.
- Previous/next filter navigation can wrap or stop at the ends of the list
  (Settings → Search); a boundary with wrapping off leaves the search alone.
- Hidden-file results can be shown, hidden or shown alone, independently of
  file kind, from Search or Settings (`Ctrl+H` cycles). The saved choice
  includes dot-prefixed folders and their contents, without changing the index.
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
- The status line keeps its note readable: the selection details shrink
  first, a long path is shortened to its start and last components
  (`~/…/Photos/2024/img.jpg`), and the note is shown in full. Hints such as
  "Select one folder to paste into." are no longer drawn in red; only
  failures are.
- Menu bar with keyboard shortcuts and about dialogs; `⋯` menu when it is
  hidden.
- One table of the window's commands drives its keys and the keyboard
  shortcuts dialog. `Ctrl+N` opens a new window with the menu bar hidden
  too, and the global keys also work while the filter selector has the
  focus.
- Menus built from the same table: File → Close window (`Ctrl+W`, the
  close button's action), View → Sort by (column and direction), Search →
  Previous filter / Next filter (`Alt+↑`/`Alt+↓`), and a Tools menu with
  Settings… (moved from File, still `Ctrl+,`) and Customize shortcuts…. The
  `⋯` menu has the same entries and names their keys for screen readers.
- Changed keys are saved in the settings file, one `shortcut` line per
  command whose keys differ from the defaults.
- The settings page "Keyboard shortcut" is now "Keyboard"
  (`--settings=keyboard`; `shortcut` still works). It lists the window's
  commands with their keys and where each key works, filtered by "Show
  commands containing:", and removes keys or resets all of them to the
  defaults.
- Add… and Edit… on the Keyboard page: a dialog that records the next key
  pressed, offers where it works (global, search field, result list),
  lists the commands that use it already and takes it from them on OK.
  Keys that would type text, `Tab`, `Esc`, list movement keys and the
  search field's editing keys are refused with a note.
- Removable drives mounted by the desktop are indexed automatically, each
  into its own file and recognised by file system UUID. A removed drive's
  entries stay searchable, dimmed and with file actions refused, or are
  forgotten, as a setting says; plugged in again they are caught up and
  watched. Settings → Indexing lists the known drives with Forget and
  Don't index this drive. Mounts are noticed through `POLLPRI` on the mount
  table at no idle cost; fstab mounts are never indexed this way. A drive
  hidden by another mount, on its mount point or on a folder above it, is
  not indexed (found by mount IDs). A cancelled
  or interrupted first reading goes on where it stopped the next time, also
  when the program quit while it was read (quitting waits at most 5 seconds
  for that save).
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
- A click into the search field while it has no focus selects all of its
  text, so typing replaces the search; a second click places the cursor
  ("Select the search text on a click into the field", Search page, on).
- "Sort dates newest first" and "Sort sizes largest first" (Window page,
  off): the first click on the header of Modified, Created or Accessed, or
  of Size, sorts descending.
- Alternate row color (Appearance page, on): every second row of the result
  list has a slightly different background, keeping the text contrast in
  both themes; the offline text color is a little darker so it stays
  readable on it.
- A double-click in the Folder column opens the containing folder, as
  `Ctrl+Enter` does (Window page, on; off opens the item as before).
- Size format (automatic, bytes, KB, MB, GB) and size units (1 KB =
  1024 bytes, 1 KiB = 1024 bytes, 1 KB = 1000 bytes) on the Appearance
  page, for the Size column and the status line; the defaults keep the
  earlier output.
- View → Refresh (`F5`, changeable on the Keyboard page): runs the
  search again and reads the creation times, access times and attributes
  of the shown rows, and those a sort needs, afresh.
- Copy path with quotes and Copy full path with quotes (context menu, Edit
  menu, no key by default): one path per line, each in double quotes. "Copy
  full path adds quotes" (Window page, off) makes Copy full path quote too.
- Opening more than 32 items at once asks first ("Open 40 items?") instead
  of refusing more than 20; the number is a setting on the Window page,
  and 0 never asks.
- "Clear the selection when the search changes" (Search page, on as
  before): off, a new search keeps the selected items it lists.
- Size columns to fit the rows on screen: double-click a header border,
  or View → Size all columns to fit / Fit columns to window (also
  in the header's context menu, with keys that can be set).

### Project

- Licensed under MIT OR Apache-2.0; original icons and app icon.
- UI design guidelines, architecture, index format and query language
  documentation.
- Generated list of third-party crates and their licenses.
- Arch Linux PKGBUILD.

