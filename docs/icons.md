# Icons

All icons in this repository are original drawings made for EveryGrep. They are
licensed like the rest of the project (MIT OR Apache-2.0). Do not copy shapes
from icon themes or fonts; draw new ones by hand following the rules below.

Review sheets (regenerate with `scripts/render-icons.sh`):

- `docs/images/icon-sheet.png`: every file-type icon at 16 px and 32 px on the
  light (`#ffffff`) and dark (`#1c1d20`) surface.
- `docs/images/app-icon-preview.png`: the app icon from 16 px to 256 px on both
  surfaces.

## App id

The app id is **`az.huseynz.EveryGrep`**. It is used, unchanged, for:

- the icon file name `data/icons/hicolor/*/apps/az.huseynz.EveryGrep.{svg,png}`,
- the desktop entry `data/az.huseynz.EveryGrep.desktop` (`Icon=` and
  `StartupWMClass=`),
- the Wayland app id, which the GUI must set before the first window is shown:
  `slint::set_xdg_app_id("az.huseynz.EveryGrep")`. Without it the window does not
  match the desktop entry and the shell shows a generic icon.

`scripts/render-icons.sh` also holds it in `APP_ID`.

## File-type icons

Location: `crates/everygrep-gui/ui/icons/types/`. Pick one with
`filetype::classify(name, is_dir)` and `FileKind::icon_name()`.

### Style

The design guide asks for quiet, monochrome line icons. File-type icons bend
that rule in one controlled way: the outline stays neutral grey, and each kind
adds **one** flat tint on a small inner symbol. In a long result list the tint
lets the eye find "the PDFs" or "the folders" without reading, while the
neutral outline keeps the table calm. No gradients, no shading, no second tint,
no letters (they are unreadable at 16 px).

| Rule | Value |
|---|---|
| Grid | `viewBox="0 0 20 20"`, drawn for 20 px, also used at 16 px |
| Stroke | 1.5, round caps and joins, `fill="none"` on the root |
| Page | `x 4.75–15.25`, `y 1.75–18.25`, corner radius 1.5, folded corner 3.5 |
| Inner area | roughly `x 6.5–13.5`, `y 7.5–16`; keep 1 unit away from the outline |
| Fills | flat; small parts at least 1.5 units wide so they survive 16 px |
| White details | only on top of a tinted fill (PDF label, archive pull) |

Folders are the exception: a solid two-tone amber shape with no outline, so
they stand apart from files at a glance (common practice in file managers).
Executable and disk image use their own outline (window, disc) instead of the
page.

### Palette

Tints were chosen in OKLCH at lightness 0.60–0.66 so that every color has at
least 3:1 contrast (the WCAG level for graphics) against both theme surfaces.

| Kind | Hex | vs `#ffffff` | vs `#1c1d20` |
|---|---|---|---|
| Neutral outline (file, text, link) | `#82868e` | 3.65 | 4.61 |
| Folder front | `#d08b1d` | 2.84 | 5.93 |
| Folder back | `#b7752b` | 3.75 | 4.49 |
| Document | `#427fd8` | 3.99 | 4.22 |
| PDF | `#d74840` | 4.29 | 3.92 |
| Spreadsheet | `#399d57` | 3.42 | 4.92 |
| Presentation | `#de6e29` | 3.30 | 5.11 |
| Image | `#04a19b` | 3.19 | 5.28 |
| Audio | `#d1528b` | 3.97 | 4.25 |
| Video | `#9163d5` | 4.24 | 3.97 |
| Archive | `#9f7655` | 4.04 | 4.17 |
| Code | `#1b92c2` | 3.54 | 4.77 |
| Font | `#9c8d34` | 3.35 | 5.03 |
| Disk image | `#6d8aa3` | 3.61 | 4.67 |
| Executable | `#4b9779` | 3.50 | 4.82 |

Notes:

- The folder front is below 3:1 on white on purpose (a lighter amber looks
  like a folder; a darker one looks brown). The folder is a large filled shape
  and its darker back panel passes 3:1, so the silhouette stays clear.
- On the selected row (`selection` token: `#dbe7ff` light, `#2b4a80` dark) the
  contrast drops to about 2–3.4:1. The name text next to the icon carries the
  meaning; icons are never the only carrier (see `design.md`).
- The SVGs contain fixed colors, so do not use Slint's `colorize` on them; it
  would paint the tint and the outline the same.

### Icons

| File | Kind | Symbol |
|---|---|---|
| `folder.svg` | Folder | two-tone amber folder |
| `folder-open.svg` | Folder (optional, e.g. expanded) | folder with a tilted front |
| `file.svg` | File, hidden, unknown | empty page |
| `text.svg` | Text | page with three grey lines |
| `document.svg` | Document | blue heading block and two lines |
| `pdf.svg` | Pdf | red label overlapping the page edge |
| `spreadsheet.svg` | Spreadsheet | green 2 × 3 cells |
| `presentation.svg` | Presentation | orange pie chart with a lifted slice |
| `image.svg` | Image | teal hills and sun |
| `audio.svg` | Audio | rose eighth note |
| `video.svg` | Video | violet play triangle |
| `archive.svg` | Archive | brown zipper teeth and pull |
| `code.svg` | Code | cyan-blue angle brackets |
| `executable.svg` | Executable | window with a green prompt |
| `font.svg` | Font | olive letter A drawn from strokes |
| `disk-image.svg` | DiskImage | disc with slate hub |
| `link.svg` | Link | page with the link badge |
| `link-badge.svg` | overlay | grey disc with a white arrow, bottom-left; draw it over any other icon for a symlink that keeps its target's kind |

### Adding an icon

1. Copy `file.svg` (or `folder.svg`), keep the root attributes and the page
   paths unchanged.
2. Draw the symbol inside the inner area with one tint. Pick a hue that is not
   yet used and check contrast against both surfaces (at least 3:1).
3. Add the variant to `FileKind`, its name to `FileKind::icon_name()`, the
   extensions to `by_extension()`, and a case to the tests in
   `crates/everygrep-gui/src/filetype.rs`. The test
   `every_kind_has_an_icon_file` fails if the SVG is missing.
4. Run `scripts/render-icons.sh` and look at `docs/images/icon-sheet.png` at
   16 px on both backgrounds before committing.

## App icon

- `data/icons/hicolor/scalable/apps/az.huseynz.EveryGrep.svg` (128 grid): a
  list of result rows on a flat ink-blue plate (`#22314d`); a white lens sits
  over the rows, and the row seen through it is amber (`#ffb648`), like the
  highlighted match in the result list.
- `data/icons/src/az.huseynz.EveryGrep-small.svg` (16 grid): the same mark with
  most rows removed and a thicker ring. It is used for 16, 24 and 32 px.
- PNGs in `data/icons/hicolor/<N>x<N>/apps/` for 16, 24, 32, 48, 64, 128, 256 and
  512 px are generated by `scripts/render-icons.sh`; edit the SVGs, not the PNGs.

## Classifier notes

`classify` uses only the name. Rules worth knowing:

- Case-insensitive; compound extensions (`.tar.gz`, `.pkg.tar.zst`, `.d.ts`)
  are checked first.
- A leading dot is not an extension: `.bashrc` is matched as a known name,
  `.hidden` is a generic file, `.config.toml` is code.
- `.ts` is TypeScript (code), not an MPEG transport stream; `.sh`, `.AppImage`,
  `.run`, `.exe` are executables; `.desktop` and systemd units are code
  (configuration text).
- Symbolic links are not visible in the name; the caller sets `FileKind::Link`
  from metadata.
