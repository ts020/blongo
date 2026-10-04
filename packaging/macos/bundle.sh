#!/usr/bin/env bash
# Build Blongo.app and a drag-to-Applications .dmg from release binaries.
#
#   packaging/macos/bundle.sh VERSION BIN_DIR OUT_DMG
#
# BIN_DIR holds blongo and blongo-serve (target/release). With these set,
# the app and the dmg are signed with a Developer ID and notarized:
#   MACOS_CERTIFICATE_P12_BASE64, MACOS_CERTIFICATE_PASSWORD  (the
#     "Developer ID Application" certificate with its key, base64 .p12)
#   APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD  (notarization; an
#     app-specific password from appleid.apple.com)
# Without them everything is ad-hoc signed: it runs, but Gatekeeper asks
# the user to confirm on first launch.
set -euo pipefail

version=$1 bin_dir=$2 out_dmg=$3
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"; [[ -n "${keychain:-}" ]] && security delete-keychain "$keychain" || true' EXIT

app="$work/Blongo.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin_dir/blongo" "$bin_dir/blongo-serve" "$app/Contents/MacOS/"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
cp "$root/LICENSE" "$root/THIRD_PARTY_LICENSES.txt" "$root/THIRD_PARTY_NOTICES.md" \
  "$app/Contents/Resources/"

# The icon: every size iconutil expects, from the 1024 px source.
iconset="$work/Blongo.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size "$here/icon.png" --out "$iconset/icon_${size}x${size}.png" > /dev/null
  double=$((size * 2))
  sips -z $double $double "$here/icon.png" --out "$iconset/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Blongo.icns"

identity=-
if [[ -n "${MACOS_CERTIFICATE_P12_BASE64:-}" ]]; then
  keychain="$work/signing.keychain-db"
  keychain_password=$(uuidgen)
  security create-keychain -p "$keychain_password" "$keychain"
  security set-keychain-settings -lut 3600 "$keychain"
  security unlock-keychain -p "$keychain_password" "$keychain"
  echo "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$work/cert.p12"
  security import "$work/cert.p12" -k "$keychain" -P "$MACOS_CERTIFICATE_PASSWORD" \
    -T /usr/bin/codesign
  rm "$work/cert.p12"
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" \
    > /dev/null
  security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
  identity=$(security find-identity -v -p codesigning "$keychain" \
    | awk -F'"' '/Developer ID Application/ { print $2; exit }')
  [[ -n "$identity" ]] || { echo "no Developer ID Application identity in the certificate" >&2; exit 1; }
  echo "signing with: $identity"
fi

sign() {
  if [[ "$identity" == - ]]; then
    codesign --force --sign - "$@"
  else
    codesign --force --sign "$identity" --options runtime --timestamp "$@"
  fi
}
# Inner executables first, then the bundle that seals them.
sign "$app/Contents/MacOS/blongo-serve"
sign "$app"
codesign --verify --strict --verbose=2 "$app"

stage="$work/dmg"
mkdir "$stage"
mv "$app" "$stage/"
ln -s /Applications "$stage/Applications"
rm -f "$out_dmg"
hdiutil create -volname "Blongo $version" -srcfolder "$stage" -fs HFS+ -format UDZO \
  -ov "$out_dmg" > /dev/null

if [[ "$identity" != - ]]; then
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
