#!/bin/sh
# Builds dist/OpenComputerUse.app. The bundle gives the agent its own
# identity, which is what macOS attaches the Accessibility and Screen
# Recording permissions to.
#
# Signing: set CODESIGN_IDENTITY to a "Developer ID Application" (or Apple
# Development) identity so permissions survive rebuilds. Without one the
# bundle is signed ad hoc, and macOS asks for the permissions again after
# every rebuild.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
profile=${PROFILE:-release}
cargo build --profile "$profile"

app=dist/OpenComputerUse.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
target_dir=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
cp "$target_dir/$profile/opencomputeruse" "$app/Contents/MacOS/opencomputeruse"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp packaging/macos/OpenComputerUse.icns "$app/Contents/Resources/"

codesign --force --options runtime --timestamp=none \
    --sign "${CODESIGN_IDENTITY:--}" "$app"
codesign --verify --verbose "$app"
echo "Built $app"
echo "Install it with: cp -R $app /Applications/ && open /Applications/OpenComputerUse.app"
