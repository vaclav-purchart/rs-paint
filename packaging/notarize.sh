#!/usr/bin/env bash
# Notarize and staple rs-paint.app so it is trusted on any Mac.
#
# Prerequisites (one-time, requires an Apple Developer account — $99/yr):
#   1. A "Developer ID Application" certificate in your keychain
#      (Xcode ▸ Settings ▸ Accounts ▸ Manage Certificates, or developer.apple.com).
#   2. A stored notarization credential profile:
#        xcrun notarytool store-credentials rspaint-notary \
#          --apple-id "you@example.com" --team-id "TEAMID" \
#          --password "app-specific-password"      # from appleid.apple.com
#
# Usage:
#   ./packaging/notarize.sh                  # build, sign, notarize, staple
#   KEYCHAIN_PROFILE=myprofile ./packaging/notarize.sh
#   CODESIGN_ID="Developer ID Application: Name (TEAMID)" ./packaging/notarize.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP="rs-paint.app"
ZIP="rs-paint.zip"
PROFILE="${KEYCHAIN_PROFILE:-rspaint-notary}"

# --- preflight: make sure we can actually notarize, with helpful errors -------
fail() {
	echo "ERROR: $1" >&2
	exit 1
}

SIGN_ID="${CODESIGN_ID:-}"
if [ -z "$SIGN_ID" ]; then
	SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
		| grep -o 'Developer ID Application: [^"]*' | head -1 || true)"
fi
if [ -z "$SIGN_ID" ]; then
	fail "No 'Developer ID Application' certificate found in the keychain.
       Enroll in the Apple Developer Program and create one (see the header of
       this script). Ad-hoc-signed apps cannot be notarized."
fi

if ! xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1; then
	fail "No notarization credentials for profile '$PROFILE'.
       Create them once with:
         xcrun notarytool store-credentials $PROFILE \\
           --apple-id <email> --team-id <TEAMID> --password <app-specific-pw>"
fi

# --- build + sign with the Developer ID (hardened runtime) --------------------
echo "==> Building & signing with Developer ID"
CODESIGN_ID="$SIGN_ID" ./packaging/bundle.sh

# --- notarize -----------------------------------------------------------------
echo "==> Zipping for submission"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"

echo "==> Submitting to Apple notary service (this can take a few minutes)"
xcrun notarytool submit "$ZIP" --keychain-profile "$PROFILE" --wait

echo "==> Stapling the notarization ticket"
xcrun stapler staple "$APP"
rm -f "$ZIP"

echo "==> Verifying Gatekeeper acceptance"
spctl -a -vvv -t exec "$APP" || true
xcrun stapler validate "$APP"

echo "==> Done: $ROOT/$APP is signed, notarized, and stapled."
