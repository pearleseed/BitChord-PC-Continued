#!/bin/sh
# macos/bundle.sh target/release/bitchord 2.0.0 out [portable]
set -eu

if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
    echo "usage: $0 <binary> <version> <out dir> [portable]" >&2
    exit 2
fi
bin=$1
version=$2
out=$3
kind=${4:-}
case "$kind" in
    ""|portable) ;;
    *) echo "unknown bundle kind: $kind" >&2; exit 2 ;;
esac
if ! printf '%s\n' "$version" | LC_ALL=C grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "expected a three-component numeric bundle version" >&2
    exit 2
fi
[ -f "$bin" ] || { echo "binary not found: $bin" >&2; exit 1; }

here=$(cd "$(dirname "$0")" && pwd)
icon="$here/../assets/icon/macos.png"
app="$out/bitchord.app"

mkdir -p "$out"
stage=$(mktemp -d "$out/.bitchord-stage.XXXXXX")
# A failed build leaves the old bundle in place.
trap 'rm -rf "$stage"' EXIT
bundle="$stage/bitchord.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$bin" "$bundle/Contents/MacOS/bitchord"
chmod 755 "$bundle/Contents/MacOS/bitchord"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist" >/dev/null

iconset="$stage/bitchord.iconset"
mkdir "$iconset"
for pixels in 16 32 64 128 256 512 1024; do
    sips -z "$pixels" "$pixels" "$icon" --out "$stage/$pixels.png" >/dev/null
done
for spec in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x 128:128x128 256:128x128@2x 256:256x256 512:256x256@2x 512:512x512 1024:512x512@2x; do
    cp "$stage/${spec%%:*}.png" "$iconset/icon_${spec#*:}.png"
done
iconutil -c icns "$iconset" -o "$bundle/Contents/Resources/bitchord.icns"

if [ "$kind" = portable ]; then
    cp "$here/portable.txt" "$bundle/Contents/Resources/portable.txt"
fi
if [ -n "${BITCHORD_SIGN_IDENTITY:-}" ]; then
    codesign --force --options runtime --timestamp --sign "$BITCHORD_SIGN_IDENTITY" "$bundle"
else
    codesign --force --sign - "$bundle"
fi
codesign --verify --strict "$bundle"

# Replace only after the entire new bundle has been validated.
if [ -e "$app" ]; then
    mv "$app" "$stage/previous.app"
fi
if ! mv "$bundle" "$app"; then
    if [ -e "$stage/previous.app" ]; then
        mv "$stage/previous.app" "$app" || {
            trap - EXIT
            echo "previous bundle preserved at $stage/previous.app" >&2
        }
    fi
    exit 1
fi
echo "Made $app"
