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
   them is a search field, column headers and one status line.
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
│ [ search field ............... Aa ab a/b é ] [filter ▾] [⋯]  │  top bar 48 px
├──────────────────────────────────────────────────────────────┤
│ Name ▲               │ Folder                │ Size │ Modified│  header 30 px
│ ▢ report-2026.pdf    │ ~/Documents/work      │ 3 MB │ 25 Oct  │  rows 28 px
│ ▢ report-draft.pdf   │ ~/Documents           │ 10 B │ 15 Sep  │
│ …                                                            │
├──────────────────────────────────────────────────────────────┤
│ 2 153 results · 617 178 indexed · watching       0.9 ms      │  status 26 px
└──────────────────────────────────────────────────────────────┘
```

- **Top bar.** The search field takes all the remaining width. Four small
  toggles at its right end switch the matching modes: match case, whole words,
  full path and diacritics; each shows its name and key in a tip on hover. A
  filter selector sits next to the field: all, audio, archives, code,
  documents, executables, folders, pictures, videos. The overflow menu holds
  settings and quit; help and about are planned.
- **Result table.** Default columns are Name, Folder, Size and Modified. Type
  and Extension can be shown from the header's context menu; Created and Full
  path are planned (the index does not keep creation or access times yet).
  Columns can be resized, hidden and sorted by clicking the header; reordering
  is planned. Sorting by Type groups by kind, then extension. The match is
  highlighted inside the name. A folder's Size is the total size of all
  files below it, at any depth (a setting can show the number of items or
  nothing instead); sorting by Size mixes folders and files by that total,
  as every other column does, since folders are never forced first.
- **Status line.** Result count, index size, index state, small badges for the
  matching modes that are on, and query time.
- **Optional panes,** hidden by default: a preview pane on the right and a
  sidebar of saved searches on the left.

## Visual system

| Token | Value |
|---|---|
| Spacing scale | 4, 8, 12, 16, 24 px |
| Corner radius | 6 px for fields and buttons, 8 px for popups, 0 for table rows |
| Font | System UI font. Body 13 px, header 12 px semibold, status 12 px |
| Numbers | Tabular figures. Sizes are right-aligned |
| Row height | 28 px (compact 24 px, comfortable 32 px) |
| Accent | One color, used for focus rings, the selection and the active sort column |
| Lines | 1 px hairlines in a low-contrast neutral color, no heavy borders |
| Themes | Light and dark, following the system setting by default |

Colors are defined once as named tokens (`surface`, `surface-alt`, `text`,
`text-muted`, `line`, `accent`, `accent-text`, `selection`, `danger`) for each
theme. Components use only the tokens, never raw colors.

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
| Open | `Enter` | double-click, context menu |
| Open the containing folder | `Ctrl+Enter` | context menu |
| Copy the full path / the name | `Ctrl+Shift+C` / `Ctrl+C` | context menu |
| Rename | `F2`; `Enter` renames, `Esc` or a click elsewhere cancels | context menu |
| Move to trash | `Delete` | context menu |
| Delete permanently | `Shift+Delete` | context menu |
| Context menu | `Menu`, `Shift+F10` | right-click |
| Clear the selection, then back to the search | `Esc` in the list | — |
| Clear the search | `Esc` in the search field | — |
| Back / forward through earlier searches | `Alt+←` / `Alt+→` | — |
| Match case, whole words, full path, diacritics | `Alt+C`, `Alt+W`, `Alt+P`, `Alt+D` | toggles in the search field |
| Previous / next filter | `Alt+↑` / `Alt+↓` | filter selector |
| Show or hide the Type and Extension columns | — | header context menu |

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

Rename refuses a name that already exists in the folder and shows the reason in
the status line. Moving to the trash asks first only for more than 50 items;
deleting permanently always asks, because it cannot be undone. Hiding the
window with `Esc` is planned.

### Settings

Settings open in a separate window (`Ctrl+,`, the overflow menu, or
`--settings`): a list of pages on the left (Indexing, Search, Window,
Keyboard shortcut), the page on the right. `Ctrl+PgUp`/`Ctrl+PgDn` switch
pages and `Esc` closes the window.

- Changes apply at once; there is no OK or Apply. Each page has "Reset to
  defaults", which resets that page only.
- Defaults reproduce the behaviour from before settings existed, so a missing
  settings file changes nothing. One exception was chosen on purpose: folder
  sizes (Window page) default to the total size, where folders used to show
  an empty Size; "nothing" restores the old look.
- Every behaviour choice is a setting; nothing is fixed in code that a user
  might want otherwise.
- Folders are added through the desktop's own folder chooser (the XDG
  portal), never through a dialog of ours.
- Changing what is indexed rebuilds the index in the background; the window
  stays usable and the status line reports the progress.
- Problems with the settings file are shown at the top of the settings window
  in plain words; a file that cannot be read is never overwritten.

## Writing

- UI text is short, in sentence case, and never blames the user.
- Numbers use thin-space grouping (`617 178`). Sizes use KB/MB/GB with 1024
  steps. Dates show `25 Oct` within the current year, otherwise `25 Oct 2025`;
  the full timestamp is in a tooltip.
- All strings live in one catalog so they can be translated. English comes
  first; Azerbaijani and Turkish are planned.

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
