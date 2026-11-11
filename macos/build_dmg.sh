#!/bin/sh
set -eu

# Usage:
#   macos/build_dmg.sh [arm64|universal|x86_64]
# Default is arm64 (Apple Silicon)

ARCH="${1:-arm64}"
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)

cd "$ROOT"

if command -v jq >/dev/null 2>&1; then
    VERSION=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
else
    VERSION=$(grep -m 1 '^version' Cargo.toml | tr -d ' "' | cut -d= -f2)
fi

echo "==> Building BitChord v$VERSION for $ARCH..."

BIN=""
case "$ARCH" in
    arm64|aarch64)
        ARCH_TAG="aarch64"
        cargo build --release --target aarch64-apple-darwin
        BIN="target/aarch64-apple-darwin/release/bitchord"
        ;;
    x86_64|intel)
        ARCH_TAG="x86_64"
        cargo build --release --target x86_64-apple-darwin
        BIN="target/x86_64-apple-darwin/release/bitchord"
        ;;
    universal)
        ARCH_TAG="universal"
        echo "--> Building aarch64 slice..."
        cargo build --release --target aarch64-apple-darwin
        echo "--> Building x86_64 slice..."
        cargo build --release --target x86_64-apple-darwin
        mkdir -p target/universal/release
        lipo -create -output target/universal/release/bitchord \
            target/aarch64-apple-darwin/release/bitchord \
            target/x86_64-apple-darwin/release/bitchord
        BIN="target/universal/release/bitchord"
        ;;
    *)
        echo "Unknown architecture: $ARCH (expected arm64, x86_64, or universal)" >&2
        exit 2
        ;;
esac

echo "==> Creating macOS app bundle..."
mkdir -p installed
"$HERE/bundle.sh" "$BIN" "$VERSION" installed

# Ensure /Applications symlink exists inside installed directory
ln -sf /Applications installed/Applications

mkdir -p dist
DMG_NAME="bitchord-setup-$VERSION-macos-$ARCH_TAG.dmg"
DMG_PATH="dist/$DMG_NAME"

echo "==> Creating disk image ($DMG_PATH)..."
hdiutil create -ov -fs HFS+ -volname BitChord -srcfolder installed "$DMG_PATH"

# Update friendly symlinks and bundle in dist/
cd dist
ln -sf "$DMG_NAME" BitChord.dmg
rm -rf ./bitchord.app
cp -R ../installed/bitchord.app ./bitchord.app

echo "==> Done! DMG created at: $DMG_PATH"
echo "==> Symlink updated: dist/BitChord.dmg"

