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
│ [ search field ........................... ] [filter ▾] [⋯]  │  top bar 48 px
├──────────────────────────────────────────────────────────────┤
│ Name ▲               │ Folder                │ Size │ Modified│  header 30 px
│ ▢ report-2026.pdf    │ ~/Documents/work      │ 3 MB │ 25 Oct  │  rows 28 px
│ ▢ report-draft.pdf   │ ~/Documents           │ 10 B │ 15 Sep  │
│ …                                                            │
├──────────────────────────────────────────────────────────────┤
│ 2 153 results · 617 178 indexed · watching       0.9 ms      │  status 26 px
└──────────────────────────────────────────────────────────────┘
```

- **Top bar.** The search field takes all the remaining width. A filter
  selector sits next to it: all, folders, documents, pictures, audio, video,
  archives, code, executables. The overflow menu holds settings, help and about.
- **Result table.** Default columns are Name, Folder, Size and Modified. More
  are available (Type, Created, Extension, Full path). Columns can be resized,
  reordered, hidden and sorted by clicking the header. The match is highlighted
  inside the name.
- **Status line.** Result count, index size, index state and query time.
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
| Focus the search field | `Ctrl+F`, `Ctrl+L`, or start typing | click |
| Move in results | `↑` `↓` `PgUp` `PgDn` `Home` `End` | scroll |
| Extend the selection | `Shift` + movement | `Shift`/`Ctrl` + click |
| Open | `Enter` | double-click |
| Open the containing folder | `Ctrl+Enter` | context menu |
| Copy the full path / the name | `Ctrl+Shift+C` / `Ctrl+C` | context menu |
| Rename | `F2` | context menu |
| Move to trash | `Delete` | context menu |
| Context menu | `Menu`, `Shift+F10` | right-click |
| Clear the search / hide the window | `Esc` | — |

Destructive actions (trash, rename over an existing file) ask for confirmation
only when they cannot be undone.

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
