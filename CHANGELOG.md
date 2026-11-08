# Changelog

All notable changes to EveryGrep are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

First version, 0.1.0, not released yet.

### File operations

- Search typed values from the clicked Name, Folder, Size, Modified and
  Extension cells, with literal escaping and a stable query while the menu is
  open. Add explicit filename and name-without-extension search commands,
  sharing current-window, new-window and new-tab routing.

- Session file undo/redo (on by default, 100 steps) for renames, new
  folders, copies, same-filesystem moves and moves to the trash, with
  bounded history, filtering, undo through selection, retry of skipped
  steps and CSV export. Changed items and occupied targets are protected;
  undoing a copy uses the trash, undoing a trash step restores the item
  from it.
- New folder (`Ctrl+Shift+N`), inside the selected folder or next to it.
- Copy to folder… and Move to folder… through the desktop's folder chooser,
  and dragging result rows onto a folder row (`Ctrl` copies; a setting
  chooses move on the same drive and copy to another, always copy, always
  move, or ask), all with the same transfer, questions and undo.
- Rename several items at once with a preview: text before or after the
  name, replace text, a regular expression, the modification date or a
  counter; a batch with any problem row is refused as a whole.
- Clean up duplicates…: keep the newest, the oldest or the copies in a
  chosen folder, preview what goes, compare the bytes again just before
  each copy goes to the trash; one undo step.
- Moving to the trash is done in-process, following the freedesktop.org
  Trash specification, instead of running `gio trash`; every trashed item
  of a batch is recorded for undo and every failure is reported.
- A copy whose source changes while it is read is refused; a move across
  file systems keeps the whole source when file times or extended
  attributes cannot be kept, and is refused for files with several hard
  links. Partial copies are created with private permissions.
- Rename validation stays local while typing; rename submission runs on a
  helper thread and keeps dialog errors next to the editor.

- Paste conflict questions (on by default) with skip, skip all, keep both,
  retry and atomic replacement of regular files, whose old version goes to
  the trash; off, a taken name gets the first free numbered name.

### Tabs

- Opt-in last-window session restore, with private, locked atomic snapshots,
  coalesced autosave and preservation of malformed or newer session files.

- Custom overflow-aware tab strip and rebindable tab commands; independent
  search, filter, matching, sort, columns, selection and navigation state.
- Close selected, left, right or other tabs; reopen the last ten individually
  or together. Background searches refresh only when shown. Only the 10
  background tabs shown last keep their results in memory (a setting);
  the others search again when shown.

- Window settings for searches in tabs and immediate activation (both on by
  default) and restoring the last window's tabs (off by default).

### Saved searches

- Advanced Search builds safe queries for words, paths, kinds, extensions and
  inclusive size/date ranges, with validation before replacing the search field.
- Advanced Search has a File contents section: a word or phrase to find inside
  the files, its own match case, whole words and regular expression switches,
  and the text encoding (automatic, the Settings code page, UTF-8, UTF-16 LE or
  BE). It adds one quoted `content:` term to the query.

- Optional per-filter and per-bookmark shortcuts reject conflicts with saved
  items and command keys; keyboard settings checks saved keys too.

- Save complete search bookmarks, open them from a Bookmarks menu and manage
  their names, search options, sort and order. Saved filters remain reproducible
  after editing or removing the original.

- Manage named filters with matching modes in the existing selector and Search
  menu; restore built-ins, reorder with Alt+Up/Down and save edits atomically.
- Filters can bring their own columns and sort order, and bookmarks keep
  their columns. Bookmarks can be grouped in one level of folders with
  separators (submenus of the Bookmarks menu). Both managers import and
  export CSV; an import never replaces an entry. The saved searches file
  is version 2 and still reads version 1.
- Bookmarks → Open all bookmarks, "Open all in tabs" for one bookmark
  folder, and Append to search; Search → Find Filter… and Bookmarks → Find
  Bookmark… open the managers with their find field.
- Five ready-made bookmark searches: files over 1 GB, changed today,
  downloads of the last 7 days, empty folders and duplicate photos.

### Search history

- Organize search history in a filterable table, delete individual searches or
  all entries, and import/export UTF-8 CSV with counts and UTC dates. Imports
  validate all rows and merge idempotently before applying retention.

- A search-field dropdown (F4) sorts history by date, count or text; optional
  prefix suggestions support arrow keys, Enter and Esc. A History menu holds
  Back and Forward.

- Record distinct searches with visit counts and last dates; retain them for
  90 days or a chosen duration (0 keeps them forever), disable recording, or
  clear them in Settings. Persistent history uses locked atomic updates.

### File actions

- Search selected folders or file names, or their containing folder, from
  the result context menu; settings choose window, depth and name matching.

- Create shortcuts to selected items in a chosen folder; settings choose
  symbolic or desktop links and numbered names or skipping existing names.

- Optional rename dialog with location, name validation and errors kept next
  to the editor; the Window settings keep in-place rename as the default.
  Renaming refuses to replace a target atomically where the file system
  supports it; elsewhere a file is moved by a hard link (which never
  replaces either), and only a folder falls back to a check and a plain
  rename.

### Index and engine

- A folder to index can leave out the contents of its subfolders (Settings
  → Indexing, per folder).
- The change event buffer is settable (16 KiB to 4 MiB), and lost events
  can lead to a catch-up by folder times instead of indexing again.
- Index → Index changes…: the last 10 000 changes applied from file
  events, with filters, Open path, Copy full path and CSV export; off by
  default and kept in memory only.
- Index → File list editor… makes, opens and saves file lists; a file list
  whose file changes on disk is read again.
- File lists: the entries of a folder, normally a disk, saved to a text file
  (`.eglist`, names kept byte for byte) and searched while it is not
  connected. Index → Create file list…, and Settings → Indexing → File lists
  to add, turn on or off, remove and read them again. Their entries are shown
  as offline and refused for file actions; the live index wins, so nothing
  is listed twice. Index → Hide offline entries leaves offline entries out
  of the results.
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
  applied the same way by scans, catch-ups and live updates. Case is ignored
  the same way for both (`*.k` also leaves out a name ending in the Kelvin
  sign, as `/x/*.k` does).
- Live updates with inotify: creations, deletions, renames and moves keep the
  index current; a queue overflow rebuilds it. A file created just before
  its folder is renamed is kept, also when the rename is read in a later
  batch; a rename whose two halves are read in two
  batches stays a rename; a folder that shows up twice (a bind mount inside
  the indexed folders) is kept current at both places; a file system
  unmounted inside the indexed folders is read again at its mount point.
  A folder's time is saved only once the events before it were applied, so
  a catch-up after an exit finds what was not applied yet.
- When no inotify instance can be made (for example for the index built
  again after a queue overflow, while other programs hold every instance),
  the index is shown as not watching changes and a watcher is tried again
  every 10 seconds; once one is made, what changed meanwhile is caught up.
- A catch-up (at start, after watching was off, for a drive connected again)
  locks the index for short batches of folders instead of for the whole
  walk, so the window keeps drawing and searching while it runs, and
  quitting no longer waits for it to finish.
- Folders that could not be watched (watch limit reached) are tried again
  and checked for changes every 10 seconds; the status line counts them.
- Catching up on start with changes made while nothing was watching, by
  re-reading only folders whose modification time changed. Watching
  command line shells and the window also catch up after fresh scans and
  rescans, with changes made before their watches were installed.
- Failed directory listings are retried after access returns; saved roots
  that were absent during scanning are revisited on load. A folder that can
  be listed but not searched still gets its added and removed entries by
  name, also while it is watched; it is listed again when it gets an event
  (such as the permission change that makes it searchable) until all its
  entries could be looked at, also when the scan or a catch-up before the
  watch left it so.
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
  file has other rules or the rules change, and leaves out a drive that a
  new folder to index holds.
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

- `type:` (the kind shown in the Type column), `attributes:` (attribute
  letters, `=` for an exact set) and exact digests (`md5:`, `sha1:`,
  `sha256:`, `sha384:`, `sha512:`, `crc32:`, `xxhash:`), read from disk
  while searching.
- Conditions on the index tree: `parent:`, `child:`, `depth:`, `len:`,
  `children:`, `childfiles:` and `childfolders:`, also in
  Advanced Search and the search field's Insert menu.
- Lists of names (`names:`, `stemnames:`, `containsnames:`,
  `containsstemnames:`) from Search → Search a list of names…, up to
  10 000 names.
- `content:` also searches Office and EPUB documents (DOCX, XLSX, PPTX,
  ODT, ODS, ODP, EPUB; on by default, with a limit for their expanded size).
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
  An unknown modification time (shown empty) matches no date; it used to
  count as 1 January 1970 (`modified:<2000` found folders that could not be
  listed and files indexed without details).
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
- `content:` finds files whose contents hold a text (`content:invoice`,
  `content:"two words"`, `content:<a b>` for both, `regex:content:^foo`,
  `case:` and `wholeword:` too). Contents are read from disk after the other terms
  narrowed the list: regular files up to 100 MB, not binary files, not
  links; UTF-8, UTF-16 with a byte order mark, else Windows-1254.
  `content-utf8:`, `content-utf16le:`, `content-utf16be:` and `content-codepage:` read
  in one encoding. Diacritics are matched as written in contents. A match
  may run over line ends (a space also matches a line break; `\n` and `\s`
  in expressions), for files up to 16 MB; Settings → Search → File
  contents turns it off.

- `duplicates:` lists files with the same contents (size first, then a
  hash of the first and last 16 KiB, then of the whole file; hard links
  count as one file), `duplicates:name`, `duplicates:stem`,
  `duplicates:ext`, `duplicates:size`, `duplicates:modified` and
  combinations (`duplicates:name;size`) compare those; `extra-copies:`
  lists every copy but the first of each group, `distinct:` one of each,
  `unique:` the results without a duplicate. Groups stay together in the
  order shown.  Empty files are left out unless
  Settings → Search → Duplicates counts them; the same page can have files
  with equal hashes compared byte by byte as well. With alternate row
  colors on, the window shades every second group of copies.

### Command line

- `everygrep index`, `search` and `shell` (with `--watch`), reusing the saved
  index and rescanning when the roots change.
- Its own index file, `cli-index.bin`, next to the window's `index.bin`, so
  the two programs never replace each other's index.
- `--exclude` takes a path or a pattern; `--include-files` and
  `--exclude-files` take file pattern lists.
- `--no-meta` stays in effect during catch-up and live updates, renames and
  moves included; folder times are kept, so unchanged folders are skipped.
  A saved index records whether it has sizes and times: one built with
  `--no-meta` is scanned again when used without it, and the other way
  round.
- `everygrep index` fails when the index cannot be saved; `search` and
  `shell` print a warning and still answer.
- Root, index and excluded-folder paths that are not UTF-8 are kept as they
  are; text arguments that are not UTF-8 are usage errors instead of a
  panic.

### Window

- Optional columns from the index, the file system and file metadata (paths,
  lengths, counts, size on disk, owner, hard links, shortcut target,
  dimensions, duration, audio tags, rating, tags, page and word counts,
  camera and GPS data), more hashes (SHA-1, SHA-384, SHA-512, xxHash) and
  a sortable duplicate group column; metadata is kept in a property cache
  (`properties.bin`, 256 MB by default), and chosen properties can be
  indexed in the background.
- Preview panel with the shared thumbnail cache, PNG/JPEG images, text
  files and sandboxed system thumbnail helpers, each source switchable; a
  Properties dialog (`Alt+Enter`); a Folders panel; a Largest folders view.
- Optional toolbar (Back, Forward, New tab, Rescan, Filters, Preview,
  Settings; each button and the Back/Forward labels can be turned off).
- Accent colors (system, blue, green, teal, purple, orange, pink; system
  follows the desktop accent, adjusted for contrast) and four surface
  styles each for the dark and the light theme.
- Zoom of the result list from 50 to 300 % (`Ctrl+=`, `Ctrl+-`,
  `Ctrl+Shift+0`); the status line can be hidden (messages then appear in a
  small box); the window title names the search and the other tabs.
- A search scope selector next to the search field (all locations, the
  home folder or one indexed folder or drive), kept per tab.
- Search page: a search delay while typing (0 ms by default) and hiding
  the results while the search field is empty (off by default).
- The search field's context menu has Insert menus for operators,
  modifiers and functions.
- Copy column value and Copy all properties (TSV), with the formula guard
  of the export setting.
- File → Load file details reads sizes and dates for an index of
  names only, and a Window page switch reads them for the rows on screen.
- Shortcuts with `Ctrl`, `Alt` or `Super` are matched by key position,
  whatever the keyboard layout (Keyboard page, on by default).
- Status badges for a duplicate search and paused index updates.

- Search window with results on every key press, matched terms in bold
  (every occurrence of every term, wildcards, regular expressions, path terms
  also in the Folder column),
  file-type icons and sortable, resizable columns (Name, Folder, Size,
  Modified, and optional Type, Extension, Created, Accessed and
  Attributes). Created, Accessed and Attributes are read on a helper
  thread, so a slow or hanging file system never holds up the window.
- `content:` searches read files without holding the index, show the
  matches found so far about once a second with "Searching file contents:
  42%" in the status line, and can be cancelled; a cancel keeps the
  matches found so far. File changes during the reading search again
  afterwards, reading only the files that changed.
- Advanced Search has a Duplicates section: what to list and what the
  copies share, written as one duplicate term.
- A right-click on the empty area below the results opens a menu: select
  all, invert selection, export, refresh and the column commands.
- While searching as you type, searches that read files (`content:`,
  duplicates by contents) wait for Enter, so typing does not read the disk
  at every key; Settings → Search → File contents turns this off.
- Duplicate searches compare files without holding the index too
  ("Comparing files: 42%", cancellable; a cancel keeps the previous
  results), with the hashes kept in `hashes.bin` next to the index so a
  second search reads only changed files. Search → Find duplicates puts
  `duplicates:`, `duplicates:name` or `duplicates:size` in the query or
  takes it out.
- MD5, SHA-256 and CRC-32 columns (View menu, header menu): hashes of the
  files on screen, read on a helper thread, smallest files first, kept in
  the hash cache; a new result list gives up the file being read.
- Same name, Same extension and Same size columns, sortable: how many entries
  of the whole index share the name (by kind, ignoring case), the
  extension (files) or the size; counted after a search while one is shown,
  again only when the index changed.
- Settings → Search → Hash cache: its largest size (512 MB by default, 0
  keeps none), how much is in use and "Clear hash cache".
- Settings → Search → File contents: the largest file to search (100 MB;
  0 for any size), the code page of files that are not UTF-8
  (Windows-1250 to 1257, Windows-1254 by default) and "Search inside PDF
  files" (off; reads them with `pdftotext` in a separate process, and says
  how to install it when it is missing).
- Settings and the search history are saved on a thread of their own, in
  the order they changed, instead of on the window's thread; quitting waits
  a few seconds at most for the last saves.
- Help window (`F1`, Help → Search syntax, Regular expressions, Command
  line options): a short reference with examples, translated.
- Typing in the result list jumps to the next name that starts with the
  typed text, shown as "Jump to: …" in the status line (Settings → Window,
  on by default). A pause of a second or a move with the arrow, page, Home
  or End keys starts a new text.
- Tools → Open the settings file, in the desktop's text editor.
- Help → Copy troubleshooting information: version, system, desktop session,
  language and index numbers on the clipboard, without paths or names.
- Help → Debug log: the last 1000 notes of the program, kept in memory, with
  Copy and Save; `--debug-log` also writes them to
  `~/.cache/everygrep/debug.log`. Notes that went only to the terminal go
  there too.
- Edit → Invert selection (`Ctrl+I`), which costs the same for any number
  of results.
- File → Export results… (`Ctrl+S`): the results as CSV (name, folder,
  size, modified, offline) or as a list of full paths (one per line, a
  backslash, line break or tab in a path escaped), through the desktop's
  save dialog, written atomically. Also as TSV, an M3U8 playlist, JSON or
  an HTML table, and of the selected results only (a check box of the save
  dialog).
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
- Menus built from the same table: File → Close window (`Ctrl+Shift+W`, the

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
  forgotten, as a setting says (also those in a rename that was half read
  when the drive went away); plugged in again they are caught up and
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
  Neither cut and copy nor paste wait for the display on the window's
  thread: a paste goes on once the other program answered, and when the
  clipboard cannot be taken with the files, the paths are put on it as text
  afterwards, unless something else was copied meanwhile.
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
  previous index and folders (the settings follow), a cancelled sort keeps
  the previous order; a newer change of the folders or rules replaces the
  running rescan.
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
  menu, no key by default): one path per line, each in double quotes, or in
  single quotes when it has `$`, a backtick, `"`, `\`, `!` or a line break,
  so a shell never runs a part of a name. "Copy full path adds quotes"
  (Window page, off) makes Copy full path quote too.
- Opening more than 32 items at once asks first ("Open 40 items?") instead
  of refusing more than 20; the number is a setting on the Window page,
  and 0 never asks.
- "Clear the selection when the search changes" (Search page, on as
  before): off, a new search keeps the selected items it lists.
- Size columns to fit the rows on screen: double-click a header border,
  or View → Size all columns to fit / Fit columns to window (also
  in the header's context menu, with keys that can be set).
- Columns can be wider than the window: the list and its headers then scroll
  sideways together, so every shown column can be reached in a narrow
  window; Left and Right in the list scroll it too (Ctrl to the edges). A dragged header border stays under the pointer; past the
  minimum of the column on its right, or at the new border after the last
  column, the columns get wider than the window, and dragging back narrows
  them again before the right column gets room. Folder still takes the
  spare room; the width it keeps while the columns are too wide is saved as
  `folder-width`.
- "Index file sizes and modification times" (Indexing page, on): off reads
  names only, which indexes faster; the Size column is then empty for files
  and folder totals, and an index of either kind is never loaded for the
  other (drives included), so changing it indexes again.
- A saved index that is damaged, of another version or cannot be read is
  built again with a note in the status line saying why (a missing one, at
  the first start, is not mentioned).
- Rescan schedule (Indexing page): never (the default), every N hours or
  every day at a time; it scans the folders to index again from scratch,
  which also finds changes no event reported, and waits while index updates
  are paused.
- Index details on the Indexing page: files, folders, total size, folders
  and drives, memory, the index file and its size, the last full scan and
  folders not watched, counted off the window's thread.
- Index file location (Indexing page): choose the folder of the window's
  index file; the index and its hash cache move there at once, and the old
  file is removed once the new one is written.
- Advanced settings page: export the settings file, import one in place of
  every setting, or reset every setting, each after a question; the window
  applies the new settings at once.
- Settings changed outside the window (by hand, by another program or by
  another window) are applied while it runs: the saving thread looks at the
  file every 2 seconds after the window's own saves, and a new version that
  differs is applied like an import, after a question (Apply or Cancel;
  a change turned down is not asked about again until the file changes in
  another way). Switches on the Advanced page turn the question or the
  whole check off.
- All settings table on the Advanced page: every key with its value and
  default, a filter, "only changed", and editing, toggling, resetting or
  copying one value, applied and saved at once.
- Index menu: Rebuild index now (as the button on the Indexing page) and
  Pause index updates, the "Watch for changes" setting the other way round
  (resuming catches up with what changed meanwhile); both commands can get
  keys.

### Project

- Licensed under MIT OR Apache-2.0; original icons and app icon.
- UI design guidelines, architecture, index format and query language
  documentation.
- Generated list of third-party crates and their licenses.
- Arch Linux PKGBUILD.
- AppStream metainfo (`data/az.huseynz.EveryGrep.metainfo.xml`), installed by
  the PKGBUILD.

