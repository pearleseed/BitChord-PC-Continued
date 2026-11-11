#!/bin/bash
# Build with Apple's system tools; no third-party packager is required.
set -euo pipefail
# An app installed into /Applications must also be readable by other Mac users.
umask 022

usage() {
    cat <<'HELP'
Usage: scripts/build-dmg.sh [native|arm64|x86_64|universal]

Build a locked release and create dist/bitchord-setup-VERSION-macos-ARCH.dmg.
The default architecture is the Mac running this script.

Optional environment:
  BITCHORD_DMG_DIR         Output directory (default: <repository>/dist)
  BITCHORD_SIGN_IDENTITY   Developer ID Application identity; unset means ad-hoc
  BITCHORD_NOTARY_PROFILE  Existing notarytool Keychain profile; requires identity

Cross builds require the corresponding Rust target to be installed.
HELP
}

fail() { echo "build-dmg: $*" >&2; exit 1; }
if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then usage; exit 0; fi
if (( $# > 1 )); then usage >&2; exit 2; fi
arch=${1:-native}
[[ "$arch" != native ]] || arch=$(uname -m)
case "$arch" in
    arm64|aarch64) targets=(aarch64-apple-darwin); tag=aarch64 ;;
    x86_64) targets=(x86_64-apple-darwin); tag=x86_64 ;;
    universal) targets=(aarch64-apple-darwin x86_64-apple-darwin); tag=universal ;;
    *) echo "build-dmg: unsupported architecture: $arch" >&2; exit 2 ;;
esac
[[ $(uname -s) == Darwin ]] || fail 'macOS is required'
identity=${BITCHORD_SIGN_IDENTITY:--}
notary=${BITCHORD_NOTARY_PROFILE:-}
if [[ -n "$notary" && "$identity" == - ]]; then
    fail 'notarization requires BITCHORD_SIGN_IDENTITY (Developer ID Application)'
fi
for tool in cargo xcrun plutil sips iconutil codesign hdiutil ditto shasum; do
    command -v "$tool" >/dev/null || fail "missing tool: $tool"
done
if [[ -n "$notary" ]]; then
    xcrun --find notarytool >/dev/null
    xcrun --find stapler >/dev/null
fi

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(printf '%s' "$metadata" | plutil -extract packages.0.version raw -o - -)
target_dir=$(printf '%s' "$metadata" | plutil -extract target_directory raw -o - -)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'bundle version must contain three numeric components'
out=${BITCHORD_DMG_DIR:-$root/dist}
mkdir -p "$out"
out=$(cd "$out" && pwd)
name="bitchord-setup-$version-macos-$tag.dmg"
[[ ! -d "$out/$name" ]] || fail "output is a directory: $out/$name"
stage=$(mktemp -d "$out/.bitchord-dmg.XXXXXX")
trap 'rm -rf "$stage"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Give both universal slices the same deployment floor. The plist below uses
# the actual linked minimum, so a dependency cannot silently understate it.
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
for target in "${targets[@]}"; do
    cargo build --release --locked --package bitchord --bin bitchord --target "$target"
done
binary="$target_dir/${targets[0]}/release/bitchord"
if [[ "$tag" == universal ]]; then
    xcrun lipo -create "$binary" "$target_dir/${targets[1]}/release/bitchord" -output "$stage/bitchord"
    binary="$stage/bitchord"
fi
minimum=$(xcrun vtool -show-build "$binary" | awk '
    $1 == "minos" {
        split($2, v, "."); key = v[1] * 1000000 + v[2] * 1000 + v[3];
        if (key > best) { best = key; version = $2 }
    }
    END { print version }
')
[[ -n "$minimum" ]] || fail 'cannot read the binary deployment target'

app="$stage/volume/bitchord.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/licenses/fonts"
ditto "$binary" "$app/Contents/MacOS/bitchord"
sed -e "s/@VERSION@/$version/g" -e "s/@MINIMUM_OS@/$minimum/g" \
    "$root/scripts/macos/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
ditto "$root/LICENSE" "$app/Contents/Resources/licenses/LICENSE"
for license in "$root"/assets/fonts/LICENSE-*.txt; do
    ditto "$license" "$app/Contents/Resources/licenses/fonts/$(basename "$license")"
done

iconset="$stage/bitchord.iconset"
mkdir "$iconset"
for spec in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x \
    128:128x128 256:128x128@2x 256:256x256 512:256x256@2x \
    512:512x512 1024:512x512@2x; do
    pixels=${spec%%:*}
    sips -z "$pixels" "$pixels" "$root/assets/icon/macos.png" \
        --out "$iconset/icon_${spec#*:}.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/bitchord.icns"
# ditto preserves source modes, including a checkout made under umask 077.
find "$app" -type d -exec chmod 755 {} +
find "$app" -type f -exec chmod 644 {} +
chmod 755 "$app/Contents/MacOS/bitchord"
if [[ "$identity" == - ]]; then
    codesign --force --sign - "$app"
    echo 'Ad-hoc build: Developer ID signing and notarization are not configured.'
else
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
fi
codesign --verify --deep --strict "$app"
ln -s /Applications "$stage/volume/Applications"

# Keep updater-compatible names and never replace an earlier DMG on failure.
dmg="$stage/$name"
hdiutil create -quiet -fs HFS+ -format UDZO -volname BitChord \
    -srcfolder "$stage/volume" "$dmg"
if [[ "$identity" != - ]]; then
    codesign --force --timestamp --identifier io.github.pearleseed.bitchord.dmg \
        --sign "$identity" "$dmg"
    codesign --verify --strict "$dmg"
fi
if [[ -n "$notary" ]]; then
    xcrun notarytool submit "$dmg" --keychain-profile "$notary" --wait \
        --output-format plist > "$stage/notarization.plist"
    status=$(plutil -extract status raw -o - "$stage/notarization.plist")
    [[ "$status" == Accepted ]] || fail "notarization status: $status"
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
fi
hdiutil verify "$dmg"
mv -f "$dmg" "$out/$name"
shasum -a 256 "$out/$name"
echo "Created $out/$name"
