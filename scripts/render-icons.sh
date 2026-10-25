#!/bin/sh
# Render the app icon PNGs and the review sheets in docs/images.
# Needs rsvg-convert (librsvg) and magick (ImageMagick 7).
set -eu
cd "$(dirname "$0")/.."

APP_ID=az.huseynz.EveryGrep
TYPES=crates/everygrep-gui/ui/icons/types
HICOLOR=data/icons/hicolor
BIG=$HICOLOR/scalable/apps/$APP_ID.svg
SMALL=data/icons/src/$APP_ID-small.svg
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# App icon PNGs. 16, 24 and 32 px use the simplified small drawing.
for n in 16 24 32 48 64 128 256 512; do
    src=$BIG
    [ "$n" -le 32 ] && src=$SMALL
    mkdir -p "$HICOLOR/${n}x${n}/apps"
    rsvg-convert -w "$n" -h "$n" "$src" -o "$HICOLOR/${n}x${n}/apps/$APP_ID.png"
    echo "app icon ${n}px"
done

# File-type sheet: one column per icon, rows 16 px and 32 px on light and dark.
mkdir -p docs/images
cols=""
for f in "$TYPES"/*.svg; do
    name=$(basename "$f" .svg)
    rsvg-convert -w 16 -h 16 "$f" -o "$TMP/$name-16.png"
    rsvg-convert -w 32 -h 32 "$f" -o "$TMP/$name-32.png"
    for bg in '#ffffff' '#1c1d20'; do
        tag=$(echo "$bg" | tr -d '#')
        fg='#16171a'; [ "$tag" = 1c1d20 ] && fg='#ecedf0'
        magick -size 84x36 "xc:$bg" "$TMP/$name-16.png" -gravity center -composite \
            "$TMP/$name-16-$tag.png"
        magick -size 84x52 "xc:$bg" "$TMP/$name-32.png" -gravity center -composite \
            "$TMP/$name-32-$tag.png"
        magick -size 84x22 "xc:$bg" -fill "$fg" -font DejaVu-Sans -pointsize 10 \
            -gravity center -annotate +0+0 "$name" "$TMP/$name-label-$tag.png"
    done
    magick "$TMP/$name-label-ffffff.png" "$TMP/$name-16-ffffff.png" "$TMP/$name-32-ffffff.png" \
        "$TMP/$name-label-1c1d20.png" "$TMP/$name-16-1c1d20.png" "$TMP/$name-32-1c1d20.png" \
        -append "$TMP/col-$name.png"
    cols="$cols $TMP/col-$name.png"
    echo "type icon $name"
done
# shellcheck disable=SC2086
magick $cols +append docs/images/icon-sheet.png

# App icon preview on light and dark.
for bg in '#ffffff' '#1c1d20'; do
    tag=$(echo "$bg" | tr -d '#')
    set --
    for n in 16 24 32 48 64 128 256; do
        magick -size 280x280 "xc:$bg" "$HICOLOR/${n}x${n}/apps/$APP_ID.png" \
            -gravity center -composite -trim -bordercolor "$bg" -border 12 "$TMP/app-$n-$tag.png"
        set -- "$@" "$TMP/app-$n-$tag.png"
    done
    magick -background "$bg" "$@" -gravity center +append "$TMP/app-row-$tag.png"
done
magick -background '#ffffff' "$TMP/app-row-ffffff.png" "$TMP/app-row-1c1d20.png" \
    -gravity center -append docs/images/app-icon-preview.png
echo "sheets written to docs/images"
