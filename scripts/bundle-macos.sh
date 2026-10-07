#!/bin/sh
# Builds dist/OpenComputerUse.app as one universal binary (Apple Silicon and
# Intel, compiled separately and joined with lipo). The bundle gives the
# agent its own identity, which is what macOS attaches the Accessibility and
# Screen Recording permissions to.
#
# ARCHS="aarch64-apple-darwin" builds one slice only, for quicker local builds.
#
# Signing: set CODESIGN_IDENTITY to a "Developer ID Application" (or Apple
# Development) identity so permissions survive rebuilds. Without one the
# bundle is signed ad hoc, and macOS asks for the permissions again after
# every rebuild.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
profile=${PROFILE:-release}
archs=${ARCHS:-"aarch64-apple-darwin x86_64-apple-darwin"}
# ScreenCaptureKit's window screenshots need macOS 14; Info.plist agrees.
export MACOSX_DEPLOYMENT_TARGET=14.0

target_dir=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
installed=$(rustup target list --installed 2>/dev/null || true)
slices=""
for target in $archs; do
    if ! printf '%s\n' "$installed" | grep -qx "$target"; then
        rustup target add "$target"
    fi
    echo "building for $target"
    cargo build --profile "$profile" --target "$target"
    slices="$slices $target_dir/$target/$profile/opencomputeruse"
done
universal="$target_dir/universal/$profile/opencomputeruse"
mkdir -p "$(dirname "$universal")"
# shellcheck disable=SC2086
lipo -create $slices -output "$universal"

app=dist/OpenComputerUse.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$universal" "$app/Contents/MacOS/opencomputeruse"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp packaging/macos/OpenComputerUse.icns "$app/Contents/Resources/"

codesign --force --options runtime --timestamp=none \
    --sign "${CODESIGN_IDENTITY:--}" "$app"
codesign --verify --verbose "$app"
echo "Built $app ($(lipo -archs "$app/Contents/MacOS/opencomputeruse"))"
echo "Install it with: cp -R $app /Applications/ && open /Applications/OpenComputerUse.app"
