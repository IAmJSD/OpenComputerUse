#!/bin/sh
# Builds and signs the lock-screen authorization plugin for local testing
# (arm64 only; scripts/bundle-macos.sh builds the shipping one).
# Output: dist/OcuLockAuthorizationPlugin.bundle
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out="$root/dist/OcuLockAuthorizationPlugin.bundle"
name=OcuLockAuthorizationPlugin
sign_id=${OCU_SIGN_ID:-"Developer ID Application: Astrid Gealer (CS54L4CF2Z)"}

rm -rf "$out"
mkdir -p "$out/Contents/MacOS"
cp "$here/Info.plist" "$out/Contents/Info.plist"

clang -bundle -arch arm64 \
    -framework Foundation -framework Security -fobjc-arc \
    -mmacosx-version-min=14.0 \
    -o "$out/Contents/MacOS/$name" "$here/plugin.m"

codesign --force --options runtime --timestamp \
    --sign "$sign_id" "$out"
codesign -dv "$out" 2>&1 | sed -n '1,4p'
echo "built $out"
