#!/bin/bash
# Builds the Automator application "Send to RUGM0Q3RJ0.app".
# Usage: build.sh [destination.app]

set -euo pipefail

readonly APP_NAME='Send to RUGM0Q3RJ0'
readonly STUB='/System/Library/CoreServices/Automator Application Stub.app'
readonly SOURCE_DIR="$(cd "$(dirname "$0")" && pwd)"
readonly ICON_PNG="$SOURCE_DIR/../dropzone/$APP_NAME.dzbundle/icon.png"
readonly DESTINATION="${1:-$HOME/Applications/$APP_NAME.app}"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/localsend-automator.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT
app="$work_dir/$APP_NAME.app"
contents="$app/Contents"

cp -R "$STUB" "$app"

cp "$SOURCE_DIR/document.wflow" "$contents/document.wflow"
plutil -replace actions.0.action.ActionParameters.source \
  -string "$(cat "$SOURCE_DIR/send.applescript")" "$contents/document.wflow"

iconset="$work_dir/AppIcon.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_PNG" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  if ((double <= 512)); then
    sips -z "$double" "$double" "$ICON_PNG" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
  fi
done
iconutil -c icns "$iconset" -o "$contents/Resources/AppIcon.icns"

info="$contents/Info.plist"
plutil -replace CFBundleIdentifier -string 'com.github.localsend.send-to-RUGM0Q3RJ0' "$info"
plutil -replace CFBundleName -string "$APP_NAME" "$info"
plutil -replace CFBundleIconFile -string 'AppIcon' "$info"
plutil -remove CFBundleIconName "$info"
plutil -replace CFBundleDocumentTypes -json \
  '[{"CFBundleTypeName":"File","CFBundleTypeRole":"Viewer","LSItemContentTypes":["public.data"]}]' \
  "$info"

codesign --force --deep --sign - "$app"

if [[ -e "$DESTINATION" ]]; then
  if [[ ! -f "$DESTINATION/Contents/document.wflow" ]]; then
    echo "Не перезаписываю $DESTINATION: это не приложение Automator" >&2
    exit 1
  fi
  rm -rf "$DESTINATION"
fi
mkdir -p "$(dirname "$DESTINATION")"
mv "$app" "$DESTINATION"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$DESTINATION"

echo "Готово: $DESTINATION"
