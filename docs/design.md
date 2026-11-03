# UI design guidelines

EveryGrep is a tool you open, type into, and leave within seconds. The interface
exists to show results fast and stay out of the way. These rules apply to every
screen; when a change conflicts with them, the rules win or are changed first.

## Principles

1. **Speed is the design.** The first frame appears in under 200 ms. Results
   update on every key press, within one frame for typical queries (a
   setting can make the search wait for Enter instead). Scrolling
   keeps up with a 120 Hz display. Nothing animates that the user has to wait
   for.
2. **Content first.** Most of the window shows file names. The chrome around
   them is a menu bar, a search field, column headers and one status line;
   the menu bar can be turned off.
3. **Quiet surfaces.** Flat neutral backgrounds and one accent color. No
   gradients, glows or decorative illustrations. Shadows appear only under
   floating elements such as menus and popups.
4. **All actions reachable by keyboard.** Every action has a key, and the mouse
   is optional. Focus is always visible.
5. **Honest feedback.** The status line says what the index is doing (loading,
   catching up, watching, number of entries). Errors appear where they happen,
   in plain words.
6. **Our own design.** The look, names and icons are our own.

## Layout

```
┌──────────────────────────────────────────────────────────────┐
│ File  Edit  View  Search  Tools  Help                        │  menu bar 40 px
│ [ search field ............ Aa ab a/b é .* ] [filter ▾]      │  top bar 48 px
├──────────────────────────────────────────────────────────────┤
│ Name ▲               │ Folder                │ Size │ Modified│  header 30 px
│ ▢ report-2026.pdf    │ ~/Documents/work      │ 3 MB │ 25 Oct  │  rows 28 px
│ ▢ report-draft.pdf   │ ~/Documents           │ 10 B │ 15 Sep  │
│ …                                                            │
├──────────────────────────────────────────────────────────────┤
│ 2 153 results · 617 178 indexed · watching       0.9 ms      │  status 26 px
└──────────────────────────────────────────────────────────────┘
```

- **Menu bar.** On by default, switchable in the View menu and the
  settings. File (new window, close window, export results, quit), Edit
  (cut, copy, paste into folder, copy name, path and full path, select all,
  invert selection, rename, trash, delete, focus the search field), View
  (refresh, optional columns, size columns to fit, sort by column and direction, folder size, density,
  theme, selection details, menu bar), Search (matching modes, including
  prefix, suffix, ignore punctuation and ignore whitespace, filters,
  previous and next filter), History (back, forward, search history), Tools (settings,
  keyboard shortcuts settings), Help (search syntax, regular expressions, command line options, copy troubleshooting information, keyboard shortcuts, about). Only
  features that exist get an entry. Every entry runs the same command as its
  key and the context menu (one table, `commands.rs`). Slint 1.18 draws the
  bar inside the window on Linux, and its item shortcuts are matched before
  the focused widget sees the key. So a menu item shows a key only when it
  is a global key that edits no text and that the search field or the list
  does not bind otherwise (`Ctrl+N`, `Ctrl+W`, `Ctrl+S`, `Ctrl+,`, `Ctrl+Q`,
  `Ctrl+I`, `F5`, `Alt+C/W/P/D/R/B/E/U/S`, `Alt+↑/↓`, `Alt+←/→` by default);
  `Ctrl+X`, `Ctrl+C`, `Ctrl+V`, `Ctrl+A`, `Delete` and `F2` are result list
  keys, stay with the search field when it has focus, and the Edit entries
  show no key. Help → Keyboard shortcuts lists them all. The bar cannot be
  opened from the keyboard yet (no `F10`), which is fine because every entry
  has a key or can get one in the settings.
- **Top bar.** The search field takes all the remaining width. Five small
  toggles at its right end switch the matching modes: match case, whole words,
  full path, diacritics and regular expression; each shows its name and key
  in a tip on hover. The rarer modes (prefix, suffix, ignore punctuation,
  ignore whitespace) are in the Search menu only. A
  filter selector sits next to the field: all, audio, archives, code,
  documents, executables, folders, pictures, videos. When the menu bar is
  hidden, an overflow button (`⋯`) after the filter holds settings,
  keyboard shortcuts settings, the keyboard shortcuts dialog, about, "show
  the menu bar" and quit; with the menu
  bar shown it is left out, so no entry has two homes on screen.
- **Result table.** Default columns are Name, Folder, Size and Modified. Type,
  Extension, Created, Accessed and Attributes can be shown from the header's
  context menu and the View menu; Full path is planned. Created, Accessed and
  Attributes are not in the index, which keeps it small: they are read from
  the file system (`statx`) for the rows on screen while their column is
  shown, off the window's thread (the cells fill in once read), and for every result when sorting by one of them, with progress and
  Cancel. Created is the birth time and stays empty where the file system
  has none (never 0 or 1970); Accessed follows `relatime`, so it is coarse.
  Attributes are letters in the order `RHDLX`: `R` read-only (no write
  permission for the owner; not shown for links, whose mode means nothing),
  `H` hidden (the name starts with a dot), `D` folder, `L` symbolic link
  (never followed), `X` regular file with any execute bit.
  Columns can be resized, hidden and sorted by clicking the header; reordering
  is planned. A dragged border stays under the pointer: the column on its
  left changes, the one on its right gives or takes the room while the
  columns fit the window, and past its minimum (or at the border after the
  last column) the columns get wider than the window. Then the rows and the
  headers scroll sideways together; dragging back narrows the columns again
  first. Folder takes the spare room and keeps a width of its own only while
  the columns are too wide (saved as `folder-width`). A double-click on a header border sizes the column left of it
  to fit; "Size all columns to fit" and "Fit columns to window"
  (View menu, header context menu, keys can be set) size every shown
  column. Fitting measures the cells of the rows on screen and the header
  label. Since Folder always takes the room the other columns leave, it is
  fitted by giving Name the rest, and "to fit" leaves the spare room to
  Folder, while "to fit window" widens or narrows every column by the same
  factor so that together they fill the window. The first click on a header sorts ascending; two settings
  (Window page, off by default) make it sort the date columns newest first
  and Size largest first. A second click reverses the order. Sorting by Type groups by kind, then extension. Matched search
  terms are bold: every occurrence of every term that is not negated (`!`),
  matched as the search matches it (case, diacritics, whole words, prefix,
  suffix and the ignore modes). Wildcard patterns mark the characters their
  literal parts matched, regular expressions every match. Name terms are
  marked in the name; path terms (`path:`, a term with `/`, or the full path
  mode) in the folder and the name; a run inside the home folder marks the
  `~` that stands for it. A cell shows up to four bold runs; later ones
  stay plain. The runs are worked out for the rows on screen only. Entries of a removable drive that is not
  connected are drawn in the `text-offline` color (dimmer than muted text,
  still at least 3.9:1, also on alternate rows), their names slanted and their icons faded, so the
  state is not shown by color alone; file actions on them are refused with a
  note in the status line. A folder's Size is the total size of all
  files below it, at any depth (a setting can show the number of items or
  nothing instead); sorting by Size mixes folders and files by that total,
  as every other column does, since folders are never forced first.
- **Hidden files.** Search menu and Search settings share a saved Show/Hide/Only
  result filter, independent of the kind selector. Ctrl+H cycles it; all three
  choices can have custom keys. Hidden folders include their descendants.
  The default shows all entries and changing visibility never rebuilds the index.
- **Status line.** Result count, index size, index state, small badges for the
  matching modes that are on (switchable on the Search settings page), and query time. With one item selected the
  details of that item follow the count: size (for a folder as the folder
  size setting says), modification time and path (the home folder as `~`;
  a path longer than 60 characters keeps its start and its last components,
  `~/…/Photos/2024/img.jpg`, while Copy full path still copies all of it);
  with several, their number and the total size of the files among them
  (summed up to 100 000 selected items). A setting turns the details off.
  The details are elided first, so the count, the index state, the badges,
  the note and the time stay visible. The note (for example "Copied 3
  paths.") is shown in full up to 360 px; hints such as "Select one folder
  to paste into." use the muted text color, and only failures (an I/O
  error, a failed paste, trash or export) are drawn in the danger color. A long operation that has run for
  300 ms shows its progress after the left part ("Indexing: 42%", or a count
  when there is no estimate), updated at most ten times a second, with a
  small Cancel button when it can be cancelled. Cancel keeps the earlier
  state and says so in the note. Its key is not decided yet.
- **Optional panes,** hidden by default: a preview pane on the right and a
  sidebar of saved searches on the left.

## Visual system

| Token | Value |
|---|---|
| Spacing scale | 4, 8, 12, 16, 24 px |
| Corner radius | 6 px for fields and buttons, 8 px for popups, 0 for table rows |
| Font | System UI font. Body 13 px, header 12 px semibold, status 12 px |
| Numbers | Tabular figures. Sizes are right-aligned |
| Row height | Comfortable 28 px; compact 22 px with 14 px icons, 12 px row text, 24 px header, 40 px top bar, 22 px status line |
| Accent | One color, used for focus rings, the selection and the active sort column |
| Lines | 1 px hairlines in a low-contrast neutral color, no heavy borders |
| Themes | Light and dark, following the desktop setting by default |

Colors are defined once as named tokens (`surface`, `surface-alt`, `field`,
`field-line`, `text`, `text-muted`, `text-offline`, `line`, `accent`,
`accent-text`, `selection`, `selection-text`, `hover`, `row-alt`, `danger`) for each
theme. Components
use only the tokens, never raw colors.

The theme setting is "same as the desktop", light or dark. "Same as the
desktop" reads `color-scheme` from the XDG settings portal
(`org.freedesktop.portal.Settings`, `ReadOne`, falling back to `Read`) and
follows its `SettingChanged` signal, so the window recolors live; "no
preference" counts as light. Until the portal answers, the toolkit's own
reading of the same setting is used, so the first frame never flashes. The
chosen scheme is also given to the standard widgets (`Palette.color-scheme`),
so menus, combo boxes and buttons match. The window frame drawn by the
toolkit keeps the desktop's scheme.

Light palette: white surface, `#f5f6f8` for the header and status line, text
`#16171a` (17.9:1), muted text `#545862` (7.1:1 on white, 6.3:1 on hover),
accent `#1f5fd6` (5.7:1), selection `#d3e2fd` with `#0b1b3d` text (13:1),
hover `#eef1f5`, danger `#b42323`. With "Alternate row color" (Appearance
page, on by default) every second row of the list is `#f8f9fb` (`row-alt`;
dark `#212226`): text keeps 17:1 on it, muted text 6.8:1 and offline text
3.9:1. The search field is white like the
surface, so it gets a visible `#c5cad2` border (`field-line`).

Contrast: body text at least 7:1 against its background, muted text at least
4.5:1, in both themes and on every row background (a test checks the
tokens).

## Icons

Simple monochrome line icons, drawn for this project and licensed with it:
folder, generic file, document, picture, audio, video, archive, code,
executable, link. Each is 16 px in rows and 20 px in the top bar. Icons are
never the only carrier of meaning; the Type column and tooltips say the same in
words.

## Interaction

| Action | Keys | Mouse |
|---|---|---|
| Focus the search field | `Ctrl+F`, `Ctrl+L`, `↑` on the first row | click (selects the text; a second click places the cursor) |
| Move in results (selects the row) | `↓` from the search field, `↑` `↓` `PgUp` `PgDn` `Home` `End` | scroll, click |
| Move without changing the selection | `Ctrl` + movement | — |
| Add or remove one row | `Ctrl+Space` | `Ctrl` + click |
| Extend the selection | `Shift` + movement | `Shift` + click |
| Select all | `Ctrl+A` | — |
| Invert the selection | `Ctrl+I` | Edit menu |
| Open | `Enter` | double-click (not in the Folder column), context menu |
| Open with another application | `Shift+Enter` | context menu |
| Open the containing folder | `Ctrl+Enter` | double-click in the Folder column (a setting, on by default), context menu |
| Cut / copy the items (as files) | `Ctrl+X` / `Ctrl+C` | context menu |
| Paste into the selected folder | `Ctrl+V` | context menu |
| Copy the full path / the name / the containing folder | `Ctrl+Shift+C` / `Ctrl+Alt+C` / `Ctrl+Alt+Shift+C` | context menu |
| Copy the full path / the containing folder in double quotes | none by default | context menu, Edit menu |
| Rename | `F2`; `Enter` renames, `Esc` or a click elsewhere cancels | context menu |
| Move to trash | `Delete` | context menu |
| Delete permanently | `Shift+Delete` | context menu |
| Context menu | `Menu`, `Shift+F10` | right-click |
| Clear the selection, then back to the search | `Esc` in the list | — |
| Clear the search | `Esc` in the search field | — |
| Back / forward through earlier searches | `Alt+←` / `Alt+→` | — |
| Match case, whole words, full path, diacritics | `Alt+C`, `Alt+W`, `Alt+P`, `Alt+D` | toggles in the search field |
| Regular expression | `Alt+R` | toggle in the search field, Search menu |
| Match prefix, match suffix | `Alt+B`, `Alt+E` | Search menu |
| Ignore punctuation, ignore whitespace | `Alt+U`, `Alt+S` | Search menu |
| Previous / next filter | `Alt+↑` / `Alt+↓` | filter selector, Search menu |
| Show or hide the Type and Extension columns | — | header context menu, View menu |
| New window over the same index | `Ctrl+N` | File menu |
| Export the results (CSV, TSV, text, M3U8, JSON or HTML) | `Ctrl+S` | File menu |
| Help: search syntax, regular expressions, command line options | `F1` | Help menu |
| Refresh: run the search again and read the details of the shown rows afresh | `F5` | View menu |

Actions apply to the selected rows when the row they start from is selected,
otherwise to that row alone. Several selected items give one path or name per
line on the clipboard; the quoted copies put each path in double quotes (in
single quotes when it has `$`, a backtick, `"`, `\`, `!` or a line break, so
that a shell takes it literally), and
"Copy full path adds quotes" (Window page, off by default) makes Copy full
path quote too. Opening more than 32 items at once asks first ("Open 40
items?", Open or Cancel); the number is a setting (Window page, 0 never
asks). Open containing folder shows at most 20 items in the file
manager. The selection
follows the files, not the row numbers: it stays when the list is sorted again
or refreshed after file changes, and a new query text clears it ("Clear the
selection when the search changes", Search page, on by default). Off, a new
query keeps the selected items it lists, and the cursor moves to the first
of them. Changing only a matching mode or the filter keeps the selection on
the items still listed either way.

A search is kept in the history when typing pauses for a second, on `Enter`
in the search field, or when a result is opened; a repeat of the current entry
is skipped, and searching after going back drops the entries ahead, as in a web
browser. The navigation trail lives in memory (up to 200 entries by default). Search
statistics keep distinct queries, their visit counts and last search dates,
independent of the navigation trail. Recording is enabled by default; disk
persistence remains opt-in. Entries expire after 90 days by default; 0 days
keeps them indefinitely, subject to the 10,000 distinct-search cap. Changing the
days field takes effect only on Apply. Search settings can disable recording or
clear both lists.
The search field’s ▼ button (F4) shows all saved searches, ordered by newest
date, highest count or ascending text, with deterministic text ties. Optional
suggestions match the typed prefix without case sensitivity and show at most
12 entries. They always appear when search as you type is off; with it on,
"Suggest earlier searches while typing" enables them. Up/Down selects a suggestion,
Enter runs it, and Esc dismisses the list.
History → Organize search history opens a separate window with a text filter,
a keyboard-accessible table (search, count, local last date), deletion and CSV
import/export through the desktop portal. Delete all asks for confirmation.
Export includes the whole history. Import validates the whole CSV first and
merges each exact search with the greater count and later date; repeat imports
do not inflate counts. Dates in CSV are UTC, independent of the UI language.

### One window

EveryGrep is opened many times a day, usually from a key. Opening it again
never stacks windows: the open window comes to the front with the search field
focused and its text selected, so typing starts a new search ("Focus the
search field when the window is shown", Window page, on by default; off
keeps the focus where it was). The first show always starts in the search
field. Switching to the window with `Alt+Tab` does not move the focus. Query words and
options passed to the new launch replace the search and the matching modes in
that window, as if it had just been started with them, with the caret after
the new query; a bare launch keeps the search. Closing the window quits the
program by default. A setting makes closing hide the window instead: the
program keeps the index loaded and watching, and the next launch shows the
window again through this hand-over, so no tray icon is needed. `Ctrl+Q`
always quits.

"Start EveryGrep when I log in" (Window page, off by default) writes an XDG
autostart entry and removes it when turned off; the switch reads the entry's
existence, so it stays true when other tools remove it. The entry starts the
program with `--background`. With closing set to hide, that start loads the
index and keeps watching without a window, ready for the next launch. With
closing set to quit, a hidden window could never come back without a tray
icon, so the window simply opens on login. A background start when the
program runs already does nothing. Resetting the Window page turns the
login start off.

A launch that finds the open window closing (still saving the index) or
not answering for 5 seconds does not open a second window next to it and
does not wait silently. A window that is closing usually ends within a
moment, and the launch goes on by itself. Otherwise a small dialog says
"The previous EveryGrep window is still closing" or "The open EveryGrep window does not
answer", with "Terminate" and "Cancel"; it closes by itself and the window
opens as soon as the old one has ended. If a terminated window has not
ended after 5 seconds, the dialog says so and offers only Close.

A window belongs to one index (its roots and index file). A launch for another
index opens its own window, and `--new-instance` opens one on purpose.

Keys that open the window are desktop shortcuts that run a command; a
program cannot grab keys on GNOME without the GlobalShortcuts portal, and
the program may not be running when the key is pressed. The Keyboard
shortcut page lists three commands with Copy buttons and the steps for
Settings → Keyboard → Custom Shortcuts; EveryGrep never writes the
desktop's settings itself.

| Shortcut | Command | Does |
|---|---|---|
| Show | `everygrep-gui` | starts, or brings the window forward |
| Show or hide | `everygrep-gui --toggle` | the same, but a window in front is closed as its close button closes it: hidden when closing hides, otherwise the program quits |
| New window | `everygrep-gui --new-window` | opens another window over the same index (another process, like `Ctrl+N`); it takes the bus name when none is held |

"In front" means the search field or the list has the keyboard focus: Slint
1.18 does not say whether a window is active, but the focused item loses
the focus when another window becomes active. While a rename, a dialog or a
menu holds the focus, a toggle brings the window forward instead of
closing it.

On Wayland a program cannot raise its own window without an activation token,
and the toolkit cannot pass one to an open window. The window is therefore
shown again as a new surface, which GNOME focuses; the window keeps its size
and contents.

`Ctrl+C` copies the items as files, as in a file manager; their names and
paths have keys of their own. Paste needs one folder selected in the
results and pastes into it. It never replaces anything: a taken name gets
the first free `name (N).ext`, from 2. Copies are written under a temporary
name and renamed into place when complete; a move across file systems
copies first and then removes only what was copied. A cut is pasted once.
Slint 1.18 puts only text on the clipboard, so files are offered through
the X11 clipboard of XWayland, which GNOME passes on to Wayland programs;
without XWayland other programs get the full paths as text. Slint cannot
start a drag to other programs yet, so dragging results out is planned. "Open with" shows the desktop's own application
chooser (the OpenURI portal with `ask`), never a list of ours.

Rename refuses a name that already exists in the folder and shows the reason in
the status line, or inside the rename dialog when Settings → Window opens one. Moving to the trash asks first only for more than 50 items;
deleting permanently always asks, because it cannot be undone. Hiding the
window with `Esc` is planned.

### Settings

Settings open in a separate window (`Ctrl+,`, Tools → Settings…, the overflow
menu, or `--settings`): a list of pages on the left (Indexing, Search,
Appearance, Window, Keyboard), the page on the right. Appearance
holds the language, the theme, the density, the alternate row color, the
selection details and the menu bar. `Ctrl+PgUp`/`Ctrl+PgDn` switch
pages and `Esc` closes the window.

- Changes apply at once; there is no OK or Apply. Each page has "Reset to
  defaults", which resets that page only.
- Defaults reproduce the behaviour from before settings existed, so a missing
  settings file changes nothing. One exception was chosen on purpose: folder
  sizes (Window page) default to the total size, where folders used to show
  an empty Size; "nothing" restores the old look. The menu bar, the
  selection details and the alternate row color (Appearance page) and the
  selection of the search text on a click (Search page) and the opening of
  the containing folder by a double-click in the Folder column (Window
  page) are also on by default; turning them off restores the earlier
  behaviour.
- Choices that also appear in a menu (View and Search) and in the settings
  are the same setting: changing one place changes the other at once.
- Every behaviour choice is a setting; nothing is fixed in code that a user
  might want otherwise.
- Folders are added through the desktop's own folder chooser (the XDG
  portal), never through a dialog of ours.
- The Indexing page also holds the removable drives: index them
  automatically (on), what happens when one is removed (keep its entries,
  shown dimmed, or forget them) and the list of known drives, each with its
  state and the buttons "Forget" and "Don't index this drive" / "Index this
  drive".
- Changing what is indexed rebuilds the index in the background; the window
  stays usable and the status line reports the progress.
- The pattern lists (Indexing page) are the one exception to "changes apply
  at once": a typed list applies on Enter or with "Apply filters", because
  every change indexes again. A pattern that does not compile is shown next
  to the lists in plain words and nothing is saved.
- Problems with the settings file are shown at the top of the settings window
  in plain words; a file that cannot be read is never overwritten.

### Behaviour choices for indexing and searching

The behaviour chosen for indexing and searching.

| Topic | Behaviour |
|---|---|
| Search as you type | On by default (today's behaviour). Off, text, mode and filter changes wait; the status line says "Press Enter to search." |
| Enter with as-you-type off | Enter runs a waiting search; with the results already current it opens the current row as before. Back/Forward still search at once. |
| Rename selection | On by default (today's behaviour): the name before the last dot; folders and dot-files whole. |
| Monitoring changes | One global switch, on by default (per-folder: no; our folder list has no per-folder options yet). Off drops every inotify watch; on again catches up and watches. A loaded index is not caught up while off. |
| Exclude list switch | One global switch, on by default; it turns all three lists on or off. |
| Lists | Three lists: excluded folders (paths and folder patterns), include-only files and excluded files. Folder list items: an absolute path matched exactly, or a pattern. |
| Item syntax | Items separated by `;` (`,` and double quotes also work); a `regex:` prefix; `*` stays within one name, `**` crosses `/`; `?` = one character. |
| Name or path | The whole name is matched; an item with a `/` (or `**`) matches the full path, and a path item that does not start with `/` matches the end of the path after a `/`; `regex:` with a `/` matches the path. |
| Case | Wildcards ignore case (Unicode); `regex:` as written, `(?i)` to ignore. Exact folder paths compare bytes. |
| Precedence | A file is kept when the include list is empty or one include matches, and no exclude matches; the order of the items does not matter. Folders are never tested against the file lists. |
| Excluded folder entry | The folder's own entry is not created, so nothing below it is read. (Before this change the folder's own entry was kept.) |
| Invalid regex | Refused with a message in Settings and not saved; a bad line in the settings file is reported and ignored. |
| After a change | Every change of the lists or the switch indexes again with progress and Cancel. A saved index built with other rules is not detected at start (the catch-up removes excluded folders it checks). |
| Live changes | The watcher and the catch-up use the same compiled rules as the scan; a moved folder is read again when a rule looks at paths. |
| Roots | The folders to index are never excluded by the rules. |

## Writing

- UI text is short, in sentence case, and never blames the user.
- Numbers use thin-space grouping (`617 178`). Sizes use KB/MB/GB with 1024
  steps by default, the unit chosen by the size (bytes below 1 KB). Two
  settings (Appearance page) change that for the Size column and the
  status line: the size format (automatic, or always bytes, KB, MB or GB;
  a fixed unit groups its digits and shows one decimal below 10) and the
  size units (1 KB = 1024 bytes, 1 KiB = 1024 bytes written `KiB`,
  `MiB`, or 1 KB = 1000 bytes). Size searches keep 1024 steps. Dates show `25 Oct` within the current year, otherwise `25 Oct 2025`;
  the full timestamp is in a tooltip.
- All strings live in one catalog per language so they can be translated:
  gettext files in `crates/everygrep-gui/translations/`, used by both the
  `.slint` files (`@tr`) and the Rust code (`i18n::tr`). English is the
  source; Azerbaijani and Turkish are complete. The window follows the
  session's language unless the Language setting picks one, and switching
  applies at once. Month names in dates are translated (`25 okt`, `25 Eki`);
  number and size formatting stays the same in every language.
- Translations use the desktop's own terms for that language (az: Qovluq,
  Zibil qutusu, Axtarış; tr: Klasör, Çöp kutusu, Arama) and must fit the
  same space as the English text; key names (`Ctrl`, `Shift`, `Enter`,
  `Delete`) are not translated.

## Performance budget

| What | Budget |
|---|---|
| Cold start to first frame, index loaded | < 200 ms |
| Key press to updated results (typical query) | < 16 ms |
| Scrolling | no dropped frames at 120 Hz |
| Idle CPU while watching | ≈ 0 % |
| Memory, 600 k entries | < 120 MB in total |

These budgets are measured before each release and the numbers are recorded in
the changelog.

### Filters and bookmarks

Search → Filter holds Save as filter, Manage filters and Restore default
filters above the existing choices. The organizer uses a table and an editor
with a name, query, kind and matching modes. New starts from the current field.
Alt+Up/Down moves the selected item when the editor is closed. Restore asks
first and keeps custom entries. Bookmarks has its own menu between Search and
History, with Save bookmark (Ctrl+D), Manage bookmarks and the saved names.
Its organizer also edits the filter and sort. Opening restores the text, all
matching modes, frozen filter and sort, revealing the sort column if needed.
A frozen filter that differs from the live one is available in the same
selector; choosing another filter leaves the snapshot unused.

Advanced Search (Search menu, Ctrl+Shift+F) is a separate window with a session
form. Words may use `*` and `?`; the exact phrase is literal. Name/path, location
and subfolder scope, the existing filter choices, type, extension and inclusive
size/date ranges build the query. Dates are local YYYY-MM-DD. Empty bounds are
unrestricted; invalid or reversed ranges leave the main field unchanged.
Use query replaces the field, applies the form's matching modes and follows
Search as you type. The form never tries to reverse-parse an existing query.

### Search tabs and session

A flat 34 px tab strip sits above the search field only with two or more tabs.
Each tab shows its query (or New tab) and a close button; middle click closes.
The active tab uses the selection colors and an accent border. Horizontal
scrolling and an all-tabs button handle overflow. Tabs have accessible labels
and selection state. Commands in the Tabs menu and keyboard settings cover
creation, duplication, navigation, direct selection, bulk closure and reopening.
The list dialog supports keyboard focus on every tab; multiple closure uses
checkboxes. Closing the last tab leaves a blank search.

Each tab retains its search, filter, matching modes, sort, columns, selection,
scroll positions (down and sideways) and navigation trail in memory. Background
results freeze;
activation restores the snapshot and updates the visible search. Unsubmitted
text still waits for Enter. The last ten closed searches can be reopened from
the Tabs menu. Searches requested in new windows can instead open background
tabs; immediate activation is a separate option. Both settings are off by
default, in the Tabs menu and Window settings.

Session restore is separately opt-in. It restores only the last window's
searches for the same index, selected tab and closed searches; an explicit
startup query takes precedence. Selection, raw row ids and navigation trails
stay in memory. Saving is coalesced, and a malformed session is preserved with
a status message.
