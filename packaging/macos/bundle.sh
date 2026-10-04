#!/usr/bin/env bash
# Package Blongo for macOS in two steps:
#
#   packaging/macos/bundle.sh app VERSION BIN_DIR OUT_DIR
#     Build OUT_DIR/Blongo.app (blongo-serve inside) from release binaries,
#     ad-hoc signed. Needs no secrets.
#   packaging/macos/bundle.sh dmg APP OUT_DMG
#     Wrap APP in a drag-to-Applications .dmg. With these set, the app and
#     the dmg are signed with a Developer ID and notarized first:
#       MACOS_CERTIFICATE_P12_BASE64, MACOS_CERTIFICATE_PASSWORD  (the
#         "Developer ID Application" certificate with its key, base64 .p12)
#       APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD  (notarization; an
#         app-specific password from account.apple.com)
#     Without them the dmg holds the ad-hoc signed app: it runs, but
#     Gatekeeper asks the user to confirm on first launch.
#
# The release workflow runs `dmg` with secrets only in a job gated by the
# `release` environment (docs/macos-signing.md).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
keychain=
cleanup() {
  [[ -n "$keychain" ]] && security delete-keychain "$keychain" 2> /dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

build_app() {
  local version=$1 bin_dir=$2 out_dir=$3
  local app="$out_dir/Blongo.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$bin_dir/blongo" "$bin_dir/blongo-serve" "$app/Contents/MacOS/"
  sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
  cp "$root/LICENSE" "$root/THIRD_PARTY_LICENSES.txt" "$root/THIRD_PARTY_NOTICES.md" \
    "$app/Contents/Resources/"

  # The icon: every size iconutil expects, from the 1024 px source.
  local iconset="$work/Blongo.iconset" size double
  mkdir "$iconset"
  for size in 16 32 128 256 512; do
    sips -z $size $size "$here/icon.png" --out "$iconset/icon_${size}x${size}.png" > /dev/null
    double=$((size * 2))
    sips -z $double $double "$here/icon.png" --out "$iconset/icon_${size}x${size}@2x.png" \
      > /dev/null
  done
  iconutil -c icns "$iconset" -o "$app/Contents/Resources/Blongo.icns"

  # Inner executable first, then the bundle that seals it.
  codesign --force --sign - "$app/Contents/MacOS/blongo-serve"
  codesign --force --sign - "$app"
  codesign --verify --strict --verbose=2 "$app"
  echo "built $app"
}

import_identity() {
  keychain="$work/signing.keychain-db"
  local keychain_password
  keychain_password=$(uuidgen)
  security create-keychain -p "$keychain_password" "$keychain"
  security set-keychain-settings -lut 3600 "$keychain"
  security unlock-keychain -p "$keychain_password" "$keychain"
  echo "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$work/cert.p12"
  security import "$work/cert.p12" -k "$keychain" -P "$MACOS_CERTIFICATE_PASSWORD" \
    -T /usr/bin/codesign > /dev/null
  rm "$work/cert.p12"
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" \
    > /dev/null
  # shellcheck disable=SC2046
  security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
  identity=$(security find-identity -v -p codesigning "$keychain" \
    | awk -F'"' '/Developer ID Application/ { print $2; exit }')
  if [[ -z "$identity" ]]; then
    echo "no Developer ID Application identity in the certificate" >&2
    exit 1
  fi
  echo "signing with: $identity"
}

build_dmg() {
  local app=$1 out_dmg=$2
  local stage="$work/dmg" version
  mkdir "$stage"
  cp -R "$app" "$stage/Blongo.app"
  app="$stage/Blongo.app"
  version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
    "$app/Contents/Info.plist")

  identity=
  if [[ -n "${MACOS_CERTIFICATE_P12_BASE64:-}" ]]; then
    import_identity
    local sign=(codesign --force --sign "$identity" --options runtime --timestamp)
    "${sign[@]}" "$app/Contents/MacOS/blongo-serve"
    "${sign[@]}" "$app"
    codesign --verify --strict --verbose=2 "$app"
  fi

  ln -s /Applications "$stage/Applications"
  rm -f "$out_dmg"
  hdiutil create -volname "Blongo $version" -srcfolder "$stage" -fs HFS+ -format UDZO \
    -ov "$out_dmg" > /dev/null

  if [[ -n "$identity" ]]; then
    codesign --force --sign "$identity" --timestamp "$out_dmg"
    if [[ -n "${APPLE_ID:-}" ]]; then
      xcrun notarytool submit "$out_dmg" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
        --password "$APPLE_APP_PASSWORD" --wait
      xcrun stapler staple "$out_dmg"
    else
      echo "APPLE_ID is not set: signed but not notarized" >&2
    fi
  fi
  echo "built $out_dmg"
}

case "${1:-}" in
  app) build_app "$2" "$3" "$4" ;;
  dmg) build_dmg "$2" "$3" ;;
  *)
    echo "usage: $0 app VERSION BIN_DIR OUT_DIR | dmg APP OUT_DMG" >&2
    exit 2
    ;;
esac
