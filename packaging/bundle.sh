#!/usr/bin/env bash
# Build rs-paint and assemble a macOS .app bundle with an icon.
# Usage: ./packaging/bundle.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP="rs-paint.app"
BIN="rs-paint"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
ARCH="$(uname -m)" # arm64 or x86_64

# Generate the icon first: it is embedded into the binary, so it must exist
# (and be current) before the release build.
echo "==> Generating icon artwork"
cargo run --release --example gen_icon

echo "==> Building release binary"
cargo build --release

echo "==> Building AppIcon.icns"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
SRC="assets/icon_1024.png"
for size in 16 32 128 256 512; do
	sips -z "$size" "$size" "$SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
	d=$((size * 2))
	sips -z "$d" "$d" "$SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
mkdir -p assets
iconutil -c icns "$ICONSET" -o assets/AppIcon.icns

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp packaging/Info.plist "$APP/Contents/Info.plist"
cp "target/release/$BIN" "$APP/Contents/MacOS/$BIN"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
chmod +x "$APP/Contents/MacOS/$BIN"

# Stamp the version from Cargo.toml into the bundle (single source of truth).
plutil -replace CFBundleShortVersionString -string "$VERSION" "$APP/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$VERSION" "$APP/Contents/Info.plist"

# Sign the bundle. If a Developer ID identity is available (CODESIGN_ID set, or
# one is found in the keychain) use it with the hardened runtime; otherwise fall
# back to an ad-hoc signature so the app at least runs cleanly on this machine.
SIGN_ID="${CODESIGN_ID:-}"
if [ -z "$SIGN_ID" ]; then
	SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
		| grep -o 'Developer ID Application: [^"]*' | head -1 || true)"
fi
if [ -n "$SIGN_ID" ]; then
	echo "==> Codesigning with: $SIGN_ID (hardened runtime)"
	codesign --force --deep --options runtime --timestamp --sign "$SIGN_ID" "$APP"
else
	echo "==> No Developer ID found; applying ad-hoc signature (local use only)"
	codesign --force --deep --sign - "$APP"
fi
codesign --verify --strict --verbose=2 "$APP" || true

# Refresh Finder's icon cache for the new bundle.
touch "$APP"

# Distribution zip: the .app + install.sh, in bundles/.
echo "==> Packaging distribution zip"
mkdir -p bundles
STAGE="$(mktemp -d)/rs-paint"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
cp install.sh "$STAGE/"
ZIP="bundles/rs-paint-v${VERSION}-macos-${ARCH}.zip"
rm -f "$ZIP"
# No --keepParent: place rs-paint.app + install.sh at the zip root.
ditto -c -k "$STAGE" "$ZIP"
rm -rf "$(dirname "$STAGE")"

echo "==> Done: $ROOT/$APP"
echo "==> Bundle: $ROOT/$ZIP"
