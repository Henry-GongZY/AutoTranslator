#!/usr/bin/env bash
# Build the macOS client app bundle and the headless self-test binary.
#
# Usage: scripts/build-macos-app.sh
# Output:
#   target/AutoTranslatorMac.app          (GUI: capture + subtitle overlay +
#                                          translation-asset download UI)
#   target/core-selftest                  (headless protocol/session self-test)
#
# The Metal engine package is rebuilt automatically when missing and copied
# into the bundle so the app can spawn it.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

app_dir="$repo/target/AutoTranslatorMac.app"
bin_dir="$app_dir/Contents/MacOS"
res_dir="$app_dir/Contents/Resources"
mkdir -p "$bin_dir" "$res_dir"

if [[ ! -x "$repo/engines/metal/translator-core" ]]; then
  echo ">> Metal engine missing, building it first"
  ./scripts/build-engines.sh metal
fi

cat > "$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>AutoTranslator</string>
    <key>CFBundleDisplayName</key>     <string>AutoTranslator</string>
    <key>CFBundleIdentifier</key>      <string>com.autotranslator.macos</string>
    <key>CFBundleExecutable</key>      <string>AutoTranslatorMac</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key>         <string>1</string>
    <key>LSMinimumSystemVersion</key>  <string>15.0</string>
    <key>NSHighResolutionCapable</key> <true/>
</dict>
</plist>
PLIST

echo ">> building app bundle"
swiftc -O -swift-version 5 \
  clients/macos/app/proto_codec.swift \
  clients/macos/app/core_link.swift \
  clients/macos/app/capture.swift \
  clients/macos/app/subtitle_panel.swift \
  clients/macos/app/app.swift \
  -o "$bin_dir/AutoTranslatorMac"

echo ">> building headless self-test"
swiftc -O -swift-version 5 \
  clients/macos/app/proto_codec.swift \
  clients/macos/app/core_link.swift \
  clients/macos/app/main.swift \
  -o "$repo/target/core-selftest"

cp "$repo/engines/metal/translator-core" "$res_dir/translator-core"

# Sign with a real identity so the Screen Recording TCC grant survives
# rebuilds (ad-hoc signatures change on every build and invalidate it).
identity="$(security find-identity -p codesigning 2>/dev/null | awk '/Apple Development/ {print $2; exit}')"
if [[ -n "${identity:-}" ]]; then
  codesign --force --deep --sign "$identity" "$app_dir" 2>/dev/null \
    && echo ">> signed: $identity"
fi

echo "app bundle: $app_dir"
echo "self-test:  $repo/target/core-selftest  (run it before launching the GUI)"
