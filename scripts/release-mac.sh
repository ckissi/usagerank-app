#!/usr/bin/env bash
# Builds a signed, notarized and stapled UsageRank.app + .dmg (universal: Apple Silicon + Intel).
#
# Credentials come from .env.signing (gitignored) or the environment:
#   APPLE_SIGNING_IDENTITY  "Developer ID Application: Your Name (TEAMID)"
# and either an App Store Connect API key (recommended):
#   APPLE_API_KEY           key ID, e.g. ABC123DEFG
#   APPLE_API_ISSUER        issuer UUID
#   APPLE_API_KEY_PATH      path to AuthKey_<KEYID>.p8
# or an Apple ID with an app-specific password:
#   APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
#
# Usage: scripts/release-mac.sh [--native]   (--native builds only for this Mac's arch)
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ -f .env.signing ]]; then
  set -a; source .env.signing; set +a
fi

: "${APPLE_SIGNING_IDENTITY:?set APPLE_SIGNING_IDENTITY (see scripts/release-mac.sh header)}"

if [[ -n "${APPLE_API_KEY:-}" ]]; then
  : "${APPLE_API_ISSUER:?set APPLE_API_ISSUER}"
  : "${APPLE_API_KEY_PATH:?set APPLE_API_KEY_PATH}"
  notary_auth=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER")
elif [[ -n "${APPLE_ID:-}" ]]; then
  : "${APPLE_PASSWORD:?set APPLE_PASSWORD (app-specific password)}"
  : "${APPLE_TEAM_ID:?set APPLE_TEAM_ID}"
  notary_auth=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
else
  echo "error: no notarization credentials (APPLE_API_KEY... or APPLE_ID...)" >&2
  exit 1
fi

if ! security find-identity -v -p codesigning | grep -qF "$APPLE_SIGNING_IDENTITY"; then
  echo "error: signing identity not in keychain: $APPLE_SIGNING_IDENTITY" >&2
  security find-identity -v -p codesigning >&2
  exit 1
fi

if [[ "${1:-}" == "--native" ]]; then
  target_args=()
  bundle_dir=src-tauri/target/release/bundle
else
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  done
  target_args=(--target universal-apple-darwin)
  bundle_dir=src-tauri/target/universal-apple-darwin/release/bundle
fi

# Tauri signs the .app with the hardened runtime, notarizes it and staples the ticket.
pnpm tauri build ${target_args[@]+"${target_args[@]}"}  # bash 3.2 + set -u rejects an empty "${arr[@]}"

app="$bundle_dir/macos/UsageRank.app"
dmg=$(ls -t "$bundle_dir"/dmg/*.dmg | head -1)

# The .dmg wrapper isn't notarized by Tauri, so sign, notarize and staple it too.
codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$dmg"
xcrun notarytool submit "$dmg" "${notary_auth[@]}" --wait
xcrun stapler staple "$dmg"

echo
echo "== verify =="
codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=4 "$app"
spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg"
xcrun stapler validate "$dmg"
echo
echo "ready: $dmg"
