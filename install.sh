#!/usr/bin/env bash
# Install the compiled app into /Applications and clear the macOS quarantine
# flag so it opens without a Gatekeeper prompt.
set -euo pipefail
cd "$(dirname "$0")"

[ -d rs-paint.app ] || { echo "rs-paint.app not found — run ./packaging/bundle.sh first." >&2; exit 1; }

DEST="/Applications/rs-paint.app"
rm -rf "$DEST"
cp -R rs-paint.app "$DEST"
xattr -dr com.apple.quarantine "$DEST"
echo "Installed to $DEST"
