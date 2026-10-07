#!/usr/bin/env bash
# Builds dist/OpenComputerUse.app as one universal binary (Apple Silicon and
# Intel, compiled separately and joined with lipo), then the two things a
# release ships: dist/OpenComputerUse.zip, which the app's updater downloads,
# and dist/OpenComputerUse.dmg, for first installs.
#
# The bundle gives the agent its own identity, which is what macOS attaches
# the Accessibility and Screen Recording permissions to.
#
# Signing: CODESIGN_IDENTITY (or MACOS_CERT_NAME, as CI sets it, with
# MACOS_KEYCHAIN) names a "Developer ID Application" identity. Without one the
# bundle is signed ad hoc, and macOS asks for the permissions again after
# every rebuild.
#
# Notarizing, when signed: MACOS_NOTARY_PROFILE (a `notarytool
# store-credentials` profile), or APPLE_ID, APPLE_TEAM_ID and
# APPLE_APP_SPECIFIC_PASSWORD.
#
# ARCHS="aarch64-apple-darwin" builds one slice only, for quicker local builds.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
profile=${PROFILE:-release}
archs=${ARCHS:-"aarch64-apple-darwin x86_64-apple-darwin"}
identity=${CODESIGN_IDENTITY:-${MACOS_CERT_NAME:-}}
# ScreenCaptureKit's window screenshots need macOS 14; Info.plist agrees.
export MACOSX_DEPLOYMENT_TARGET=14.0

app=dist/OpenComputerUse.app
zip=dist/OpenComputerUse.zip
dmg=dist/OpenComputerUse.dmg

target_dir=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
installed=$(rustup target list --installed 2>/dev/null || true)
slices=()
for target in $archs; do
    if ! printf '%s\n' "$installed" | grep -qx "$target"; then
        rustup target add "$target"
    fi
    echo "building for $target"
    cargo build --profile "$profile" --target "$target"
    slices+=("$target_dir/$target/$profile/opencomputeruse")
done
universal="$target_dir/universal/$profile/opencomputeruse"
mkdir -p "$(dirname "$universal")"
lipo -create "${slices[@]}" -output "$universal"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$universal" "$app/Contents/MacOS/opencomputeruse"
# Debug symbols stay in the build tree, not the shipping copy; before
# signing, which a later change would void.
if [ "$profile" != "debug" ] && [ "$profile" != "dev" ]; then
    xcrun strip -S -x "$app/Contents/MacOS/opencomputeruse"
fi
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp packaging/macos/OpenComputerUse.icns "$app/Contents/Resources/"
cp src/agent/ui/LICENSE-SCHIST "$app/Contents/Resources/"
plutil -lint "$app/Contents/Info.plist" >/dev/null

keychain=()
if [ -n "${MACOS_KEYCHAIN:-}" ]; then
    keychain=(--keychain "$MACOS_KEYCHAIN")
fi
signed=false
if [ -n "$identity" ]; then
    echo "signing with $identity"
    codesign --force --options runtime --timestamp \
        ${keychain[@]+"${keychain[@]}"} --sign "$identity" "$app"
    signed=true
else
    echo "no signing identity: signing ad hoc"
    codesign --force --options runtime --timestamp=none --sign - "$app"
fi
codesign --verify --strict --verbose=2 "$app"

notary=()
if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
    notary=(--keychain-profile "$MACOS_NOTARY_PROFILE")
    if [ -n "${MACOS_KEYCHAIN:-}" ]; then
        notary+=(--keychain "$MACOS_KEYCHAIN")
    fi
elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
    notary=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID"
            --password "${APPLE_APP_SPECIFIC_PASSWORD:-}")
fi

if [ "$signed" = true ] && [ ${#notary[@]} -gt 0 ]; then
    echo "notarizing the app"
    # Notarization takes a zip, but the ticket is stapled to the bundle, so
    # this upload copy is scratch; the shippable zip is made afterwards.
    ditto -c -k --keepParent "$app" dist/upload.zip
    xcrun notarytool submit dist/upload.zip "${notary[@]}" --wait
    rm -f dist/upload.zip
    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
    # What Gatekeeper will say on a machine that has never seen the app.
    spctl --assess --type exec --verbose=2 "$app"
elif [ "$signed" = true ]; then
    echo "no notarization credentials: signed but not notarized"
fi

# What the updater downloads. ditto, not zip: it keeps the symlinks and
# extended attributes a signature is taken over.
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"

# The disk image, built from the finished bundle so any stapled ticket is
# inside it, with the drag-to-install link beside the app.
stage=$(mktemp -d)
ditto "$app" "$stage/OpenComputerUse.app"
ln -s /Applications "$stage/Applications"
rm -f "$dmg"
hdiutil create -quiet -volname OpenComputerUse -srcfolder "$stage" -ov -format UDZO "$dmg"
rm -rf "$stage"
# Gatekeeper assesses the image itself the moment it is opened.
if [ "$signed" = true ]; then
    codesign --force --timestamp ${keychain[@]+"${keychain[@]}"} --sign "$identity" "$dmg"
    if [ ${#notary[@]} -gt 0 ]; then
        echo "notarizing the disk image"
        xcrun notarytool submit "$dmg" "${notary[@]}" --wait
        xcrun stapler staple "$dmg"
    fi
fi

echo "Built $app ($version, $(lipo -archs "$app/Contents/MacOS/opencomputeruse"))"
echo "Built $zip and $dmg"
echo "Install it with: cp -R $app /Applications/ && open /Applications/OpenComputerUse.app"
