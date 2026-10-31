# UI design guidelines

EveryGrep is a tool you open, type into, and leave within seconds. The interface
exists to show results fast and stay out of the way. These rules apply to every
screen; when a change conflicts with them, the rules win or are changed first.

## Principles

1. **Speed is the design.** The first frame appears in under 200 ms. Results
   update on every key press, within one frame for typical queries. Scrolling
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
│ File  Edit  View  Search  Help                               │  menu bar 40 px
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
  settings. File (new window, export results, settings, quit), Edit (cut, copy,
  paste into folder, copy name, path and full path, select all, rename,
  trash, delete), View (optional columns, folder size,
  density, theme, selection details, menu bar), Search (matching modes,
  including prefix, suffix, ignore punctuation and ignore whitespace,
  filters, previous and next search), Help (keyboard shortcuts, about).
  Edit also has "Invert selection" (`Ctrl+I`). Every entry runs the same
  action as its key or the context menu. Slint
  1.18 draws the bar inside the window on Linux (a native menu bar exists
  only on macOS and Windows), and its item shortcuts are matched before the
  focused widget sees the key. So only keys that mean the same everywhere
  are given to menu items (`Ctrl+N`, `Ctrl+S`, `Ctrl+,`, `Ctrl+Q`, `Ctrl+I`,
  `Alt+C/W/P/D/R/B/E/U/S`, `Alt+←/→`); `Ctrl+X`, `Ctrl+C`, `Ctrl+V`, `Ctrl+A`, `Delete` and `F2` stay with the search
  field when it has focus, and the Edit entries show no key. Help →
  Keyboard shortcuts lists them all. The bar cannot be opened from the
  keyboard yet (no `F10`), which is fine because every entry has a key or
  is in the settings.
- **Top bar.** The search field takes all the remaining width. Five small
  toggles at its right end switch the matching modes: match case, whole words,
  full path, diacritics and regular expression; each shows its name and key
  in a tip on hover. The rarer modes (prefix, suffix, ignore punctuation,
  ignore whitespace) are in the Search menu only. A
  filter selector sits next to the field: all, audio, archives, code,
  documents, executables, folders, pictures, videos. When the menu bar is
  hidden, an overflow button (`⋯`) after the filter holds settings,
  keyboard shortcuts, about, "show the menu bar" and quit; with the menu
  bar shown it is left out, so no entry has two homes on screen.
- **Result table.** Default columns are Name, Folder, Size and Modified. Type,
  Extension, Created, Accessed and Attributes can be shown from the header's
  context menu and the View menu; Full path is planned. Created, Accessed and
  Attributes are not in the index, which keeps it small: they are read from
  the file system (`statx`) for the rows on screen while their column is
  shown, and for every result when sorting by one of them, with progress and
  Cancel. Created is the birth time and stays empty where the file system
  has none (never 0 or 1970); Accessed follows `relatime`, so it is coarse.
  Attributes are letters in the order `RHDLX`: `R` read-only (no write
  permission for the owner; not shown for links, whose mode means nothing),
  `H` hidden (the name starts with a dot), `D` folder, `L` symbolic link
  (never followed), `X` regular file with any execute bit.
  Columns can be resized, hidden and sorted by clicking the header; reordering
  is planned. Sorting by Type groups by kind, then extension. Matched search
  terms are bold: every occurrence of every term that is not negated (`!`),
  matched as the search matches it (case, diacritics, whole words, prefix,
  suffix and the ignore modes). Wildcard patterns mark the characters their
  literal parts matched, regular expressions every match. Name terms are
  marked in the name; path terms (`path:`, a term with `/`, or the full path
  mode) in the folder and the name; a run inside the home folder marks the
  `~` that stands for it. A cell shows up to four bold runs; later ones
  stay plain. The runs are worked out for the rows on screen only. Entries of a removable drive that is not
  connected are drawn in the `text-offline` color (dimmer than muted text,
  still at least 3.9:1), their names slanted and their icons faded, so the
  state is not shown by color alone; file actions on them are refused with a
  note in the status line. A folder's Size is the total size of all
  files below it, at any depth (a setting can show the number of items or
  nothing instead); sorting by Size mixes folders and files by that total,
  as every other column does, since folders are never forced first.
- **Status line.** Result count, index size, index state, small badges for the
  matching modes that are on, and query time. With one item selected the
  details of that item follow the count: size (for a folder as the folder
  size setting says), modification time and full path; with several, their
  number and the total size of the files among them (summed up to 100 000
  selected items). A setting turns the details off. The left part is elided
  first, so badges and time stay visible. A long operation that has run for
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
`accent-text`, `selection`, `selection-text`, `hover`, `danger`) for each
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
hover `#eef1f5`, danger `#b42323`. The search field is white like the
surface, so it gets a visible `#c5cad2` border (`field-line`).

Contrast: body text at least 7:1 against its background, muted text at least
4.5:1.

## Icons

Simple monochrome line icons, drawn for this project and licensed with it:
folder, generic file, document, picture, audio, video, archive, code,
executable, link. Each is 16 px in rows and 20 px in the top bar. Icons are
never the only carrier of meaning; the Type column and tooltips say the same in
words.

## Interaction

| Action | Keys | Mouse |
|---|---|---|
| Focus the search field | `Ctrl+F`, `Ctrl+L`, `↑` on the first row | click |
| Move in results (selects the row) | `↓` from the search field, `↑` `↓` `PgUp` `PgDn` `Home` `End` | scroll, click |
| Move without changing the selection | `Ctrl` + movement | — |
| Add or remove one row | `Ctrl+Space` | `Ctrl` + click |
| Extend the selection | `Shift` + movement | `Shift` + click |
| Select all | `Ctrl+A` | — |
| Invert the selection | `Ctrl+I` | Edit menu |
| Open | `Enter` | double-click, context menu |
| Open with another application | `Shift+Enter` | context menu |
| Open the containing folder | `Ctrl+Enter` | context menu |
| Cut / copy the items (as files) | `Ctrl+X` / `Ctrl+C` | context menu |
| Paste into the selected folder | `Ctrl+V` | context menu |
| Copy the full path / the name / the containing folder | `Ctrl+Shift+C` / `Ctrl+Alt+C` / `Ctrl+Alt+Shift+C` | context menu |
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
| Export the results (CSV or text) | `Ctrl+S` | File menu |

Actions apply to the selected rows when the row they start from is selected,
otherwise to that row alone. Several selected items give one path or name per
line on the clipboard. Opening is limited to 20 items at once. The selection
follows the files, not the row numbers: it stays when the list is sorted again
or refreshed after file changes, and a new query clears it.

A search is kept in the history when typing pauses for a second, on `Enter`
in the search field, or when a result is opened; a repeat of the current entry
is skipped, and searching after going back drops the entries ahead, as in a web
browser. The history lives in memory for the session (up to 200 entries by
default); its size, and whether it is kept on disk, are settings.

### One window

EveryGrep is opened many times a day, usually from a key. Opening it again
never stacks windows: the open window comes to the front with the search field
focused and its text selected, so typing starts a new search. Query words and
options passed to the new launch replace the search and the matching modes in
that window, as if it had just been started with them, with the caret after
the new query; a bare launch keeps the search. Closing the window quits the
program by default. A setting makes closing hide the window instead: the
program keeps the index loaded and watching, and the next launch shows the
window again through this hand-over, so no tray icon is needed. `Ctrl+Q`
always quits.

A window belongs to one index (its roots and index file). A launch for another
index opens its own window, and `--new-instance` opens one on purpose.

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
the status line. Moving to the trash asks first only for more than 50 items;
deleting permanently always asks, because it cannot be undone. Hiding the
window with `Esc` is planned.

### Settings

Settings open in a separate window (`Ctrl+,`, File → Settings, the overflow
menu, or `--settings`): a list of pages on the left (Indexing, Search,
Appearance, Window, Keyboard shortcut), the page on the right. Appearance
holds the language, the theme, the density, the selection details and the
menu bar. `Ctrl+PgUp`/`Ctrl+PgDn` switch
pages and `Esc` closes the window.

- Changes apply at once; there is no OK or Apply. Each page has "Reset to
  defaults", which resets that page only.
- Defaults reproduce the behaviour from before settings existed, so a missing
  settings file changes nothing. One exception was chosen on purpose: folder
  sizes (Window page) default to the total size, where folders used to show
  an empty Size; "nothing" restores the old look. The menu bar and the
  selection details (Appearance page) are also on by default; turning them
  off restores the earlier look.
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
- Problems with the settings file are shown at the top of the settings window
  in plain words; a file that cannot be read is never overwritten.

## Writing

- UI text is short, in sentence case, and never blames the user.
- Numbers use thin-space grouping (`617 178`). Sizes use KB/MB/GB with 1024
  steps. Dates show `25 Oct` within the current year, otherwise `25 Oct 2025`;
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
