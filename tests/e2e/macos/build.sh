#!/bin/sh
# Builds the macOS test app as an ad hoc signed bundle (no hardened runtime,
# so the panel hook may load into it). Usage: build.sh <out dir>
set -eu
here=$(cd "$(dirname "$0")" && pwd)
app="$1/Dialog.app"
mkdir -p "$app/Contents/MacOS"
swiftc -O -o "$app/Contents/MacOS/Dialog" "$here/Dialog.swift"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.infrawrench.ocu-e2e-dialog</string>
  <key>CFBundleExecutable</key><string>Dialog</string>
  <key>CFBundleName</key><string>Dialog</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$app"
echo "$app"
