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
# and the Tauri updater key that signs the in-app update:
#   TAURI_SIGNING_PRIVATE_KEY_PATH, TAURI_SIGNING_PRIVATE_KEY_PASSWORD
#
# Usage:
#   scripts/release-mac.sh 1.2.3 [--native] [--notes "text"]   release 1.2.3
#   scripts/release-mac.sh [--native]                          test build of the current version, nothing published
#
# A release must run on main, clean and up to date with origin. It sets the version in
# src-tauri/tauri.conf.json, src-tauri/Cargo.toml and package.json, builds, then commits "Release 1.2.3",
# tags v1.2.3, pushes both and creates the GitHub release with the .dmg, the update bundle and
# latest.json. Installed apps find it through latest.json (endpoint in tauri.conf.json).
# If the build fails, the version files are put back.
#   --native   build only for this Mac's arch
#   --notes    release notes, shown in the app's "What's new" tooltip and on GitHub
set -euo pipefail

native=0 notes="" new_version=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --native) native=1 ;;
    --notes) notes="${2:?--notes needs text}"; shift ;;
    [0-9]*.[0-9]*.[0-9]*)
      [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: version must look like 1.2.3, got $1" >&2; exit 1; }
      new_version="$1" ;;
    *) echo "error: unknown option $1" >&2; exit 1 ;;
  esac
  shift
done
repo=ckissi/usagerank-app
version_files=(src-tauri/tauri.conf.json src-tauri/Cargo.toml package.json)

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

: "${TAURI_SIGNING_PRIVATE_KEY_PATH:?set TAURI_SIGNING_PRIVATE_KEY_PATH (updater key, see .env.signing)}"
: "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD?set TAURI_SIGNING_PRIVATE_KEY_PASSWORD}"
TAURI_SIGNING_PRIVATE_KEY=$(cat "$TAURI_SIGNING_PRIVATE_KEY_PATH")
export TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD

# The updater compares tauri.conf.json's version; the popover shows Cargo's. Keep all three in step.
version=$(node -p 'require("./src-tauri/tauri.conf.json").version')
cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' src-tauri/Cargo.toml | head -1)
npm_version=$(node -p 'require("./package.json").version')
if [[ "$cargo_version" != "$version" || "$npm_version" != "$version" ]]; then
  echo "error: versions differ: tauri.conf.json $version, Cargo.toml $cargo_version, package.json $npm_version" >&2
  exit 1
fi

if [[ -n "$new_version" ]]; then
  branch=$(git branch --show-current)
  [[ "$branch" == main ]] || { echo "error: release from main, not ${branch:-a detached HEAD}" >&2; exit 1; }
  [[ -z "$(git status --porcelain --untracked-files=no)" ]] || { echo "error: commit or stash your changes first" >&2; git status --short >&2; exit 1; }
  git fetch -q origin main --tags
  git merge-base --is-ancestor origin/main HEAD || { echo "error: main is behind origin/main; pull first" >&2; exit 1; }
  if [[ "$new_version" == "$version" || "$(printf '%s\n' "$version" "$new_version" | sort -V | tail -1)" != "$new_version" ]]; then
    echo "error: $new_version is not newer than the current $version" >&2
    exit 1
  fi
  if git rev-parse -q --verify "refs/tags/v$new_version" >/dev/null || gh release view "v$new_version" -R "$repo" >/dev/null 2>&1; then
    echo "error: v$new_version already exists (tag or GitHub release)" >&2
    exit 1
  fi

  # Until the release commit exists, a failed run puts the version files (and Cargo.lock) back.
  committed=0 old_version="$version"
  restore_versions() {
    if [[ $committed == 0 ]]; then
      git checkout -- "${version_files[@]}" src-tauri/Cargo.lock
      echo "version files restored to $old_version" >&2
    fi
  }
  trap restore_versions EXIT
  sed -i '' "s/\"version\": \"$version\"/\"version\": \"$new_version\"/" src-tauri/tauri.conf.json package.json
  sed -i '' "s/^version = \"$version\"/version = \"$new_version\"/" src-tauri/Cargo.toml
  echo "== release $version -> $new_version =="
  version="$new_version"
fi

if [[ $native == 1 ]]; then
  target_args=()
  bundle_dir=src-tauri/target/release/bundle
  # arch_label matches the .dmg name Tauri picks (UsageRank_<version>_aarch64.dmg / _x64.dmg).
  case "$(uname -m)" in
    arm64) platforms=(darwin-aarch64) arch_label=aarch64 ;;
    *) platforms=(darwin-x86_64) arch_label=x64 ;;
  esac
else
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  done
  target_args=(--target universal-apple-darwin)
  bundle_dir=src-tauri/target/universal-apple-darwin/release/bundle
  platforms=(darwin-aarch64 darwin-x86_64)
  arch_label=universal
fi

# Tauri signs the .app with the hardened runtime, notarizes it and staples the ticket, then packs the
# stapled app into UsageRank.app.tar.gz + .sig for the in-app updater.
pnpm tauri build ${target_args[@]+"${target_args[@]}"} --config '{"bundle":{"createUpdaterArtifacts":true}}'  # bash 3.2 + set -u rejects an empty "${arr[@]}"

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

# The update bundle and the latest.json manifest the app polls.
out="$bundle_dir/release"
rm -rf "$out" && mkdir -p "$out"
update_name="UsageRank_${version}_${arch_label}.app.tar.gz"
cp "$bundle_dir/macos/UsageRank.app.tar.gz" "$out/$update_name"
cp "$dmg" "$out/"
VERSION="$version" NOTES="$notes" URL="https://github.com/$repo/releases/download/v$version/$update_name" \
SIGNATURE="$(cat "$bundle_dir/macos/UsageRank.app.tar.gz.sig")" PLATFORMS="${platforms[*]}" \
  node -e '
    const e = process.env;
    const platforms = Object.fromEntries(e.PLATFORMS.split(" ").map((p) => [p, { signature: e.SIGNATURE, url: e.URL }]));
    const manifest = { version: e.VERSION, notes: e.NOTES, pub_date: new Date().toISOString(), platforms };
    require("fs").writeFileSync(process.argv[1], JSON.stringify(manifest, null, 2) + "\n");
  ' "$out/latest.json"

echo
if [[ -z "$new_version" ]]; then
  echo "test build ready: $out (nothing published; release with: scripts/release-mac.sh <new version>)"
  exit 0
fi

# Commit and tag locally, then push both, so the tag points at the release commit.
git add "${version_files[@]}" src-tauri/Cargo.lock
git commit -q -m "Release $version"
committed=1
git tag -a "v$version" -m "UsageRank $version"
git push -q --atomic origin main "v$version"

assets=("$out/$(basename "$dmg")" "$out/$update_name" "$out/latest.json")
if ! gh release create "v$version" -R "$repo" --verify-tag --title "UsageRank $version" --notes "${notes:-UsageRank $version}" "${assets[@]}"; then
  echo "error: the tag v$version is pushed but the GitHub release failed. Retry with:" >&2
  printf '  gh release create v%s -R %s --verify-tag --title "UsageRank %s" --notes %q' "$version" "$repo" "$version" "${notes:-UsageRank $version}" >&2
  printf ' %q' "${assets[@]}" >&2
  echo >&2
  exit 1
fi
echo "published: https://github.com/$repo/releases/tag/v$version"
