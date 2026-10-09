#!/bin/sh
# Builds the panel hook for local testing (arm64 only; scripts/bundle-macos.sh
# builds the shipping one). Output: dist/OcuPanelHook.dylib
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out="$root/dist/OcuPanelHook.dylib"
sign_id=${OCU_SIGN_ID:-"Developer ID Application: Astrid Gealer (CS54L4CF2Z)"}

mkdir -p "$(dirname "$out")"
clang -dynamiclib -arch arm64 \
    -framework AppKit -fobjc-arc -Wall \
    -mmacosx-version-min=14.0 \
    -o "$out" "$here/hook.m"
codesign --force --options runtime --timestamp=none --sign "$sign_id" "$out"
echo "built $out"
