#!/usr/bin/env bash
# Makes the universal WebDriverAgent simulator runner the app bundles, so
# iOS simulator sessions need no download: Appium's prebuilt arm64 and
# x86_64 runners for the version crates/ocu-mobile/src/ios/wda.rs pins,
# joined with lipo, in an xz tarball (3.8 MB rather than 19) that the app
# unpacks the first time a simulator session needs it.
#
#   scripts/wda-sim-runner.sh OUT_DIR [IDENTITY]
#
# Writes OUT_DIR/WebDriverAgentRunner-Runner.tar.xz and
# OUT_DIR/LICENSE-WebDriverAgent. With IDENTITY (and MACOS_KEYCHAIN), every
# piece of code in it is signed with it, inside out, with the hardened
# runtime notarization wants (and packaging/macos/wda-sim.entitlements, which
# a simulator process needs under it); without, ad hoc.
#
# Signed, and with the notary credentials bundle-macos.sh takes
# (MACOS_NOTARY_PROFILE, or APPLE_ID, APPLE_TEAM_ID and
# APPLE_APP_SPECIFIC_PASSWORD), the runner is notarized on its own before
# it is packed: the notary service doesn't look inside an xz tarball, so the
# app's notarization says nothing about it. Downloads are cached in
# target/wda-sim.
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:?usage: scripts/wda-sim-runner.sh OUT_DIR [IDENTITY]}
identity=${2:-}
version=$(sed -n 's/^pub const VERSION: &str = "\(.*\)";/\1/p' crates/ocu-mobile/src/ios/wda.rs)
[ -n "$version" ] || { echo "can't read WebDriverAgent's version from wda.rs" >&2; exit 1; }
cache=target/wda-sim/$version
app=WebDriverAgentRunner-Runner.app

mkdir -p "$cache"
for arch in arm64 x86_64; do
    zip="$cache/sim-$arch.zip"
    if [ ! -f "$zip" ]; then
        echo "downloading WebDriverAgent $version for the $arch simulator"
        curl -fsSL -o "$zip.part" \
            "https://github.com/appium/WebDriverAgent/releases/download/v$version/WebDriverAgentRunner-Build-Sim-$arch.zip"
        mv "$zip.part" "$zip"
    fi
    rm -rf "$cache/$arch"
    ditto -x -k "$zip" "$cache/$arch"
done
if [ ! -f "$cache/LICENSE" ]; then
    curl -fsSL -o "$cache/LICENSE" \
        "https://raw.githubusercontent.com/appium/WebDriverAgent/v$version/LICENSE"
fi

# The arm64 copy, with each single-arch binary joined to its x86_64 twin.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
runner="$work/$app"
ditto "$cache/arm64/$app" "$runner"
while IFS= read -r -d '' f; do
    rel=${f#"$runner/"}
    file -b "$f" | grep -q Mach-O || continue
    archs=$(lipo -archs "$f")
    case "$archs" in *x86_64*) continue ;; esac
    lipo -create "$f" "$cache/x86_64/$app/$rel" -output "$f.universal"
    mv "$f.universal" "$f"
done < <(find "$runner" -type f -print0)

# Inside out: nested code first, the app last.
keychain=()
if [ -n "${MACOS_KEYCHAIN:-}" ]; then
    keychain=(--keychain "$MACOS_KEYCHAIN")
fi
# The two that run as executables get the entitlements.
sign() {
    local extra=()
    if [ "${2:-}" = executable ]; then
        extra=(--entitlements packaging/macos/wda-sim.entitlements)
    fi
    if [ -n "$identity" ]; then
        codesign --force --options runtime --timestamp ${extra[@]+"${extra[@]}"} \
            ${keychain[@]+"${keychain[@]}"} --sign "$identity" "$1"
    else
        codesign --force --timestamp=none ${extra[@]+"${extra[@]}"} --sign - "$1"
    fi
}
find "$runner" -depth \( -name '*.dylib' -o -name '*.framework' \) -print0 |
    while IFS= read -r -d '' f; do sign "$f"; done
sign "$runner/PlugIns/WebDriverAgentRunner.xctest" executable
sign "$runner" executable
codesign --verify --deep --strict "$runner"

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
if [ -n "$identity" ] && [ ${#notary[@]} -gt 0 ]; then
    echo "notarizing the simulator runner"
    ditto -c -k --keepParent "$runner" "$work/runner.zip"
    xcrun notarytool submit "$work/runner.zip" "${notary[@]}" --wait \
        --output-format plist > "$work/notary.plist"
    status=$(/usr/libexec/PlistBuddy -c "Print :status" "$work/notary.plist")
    if [ "$status" != Accepted ]; then
        id=$(/usr/libexec/PlistBuddy -c "Print :id" "$work/notary.plist")
        echo "the notary service said $status for the simulator runner:" >&2
        xcrun notarytool log "$id" "${notary[@]}" >&2 || true
        exit 1
    fi
    # Apple tickets only the x86_64 slices of simulator code, so stapling
    # fails on Apple silicon. Nothing needs the ticket: the app unpacks the
    # runner without quarantine, so Gatekeeper never assesses it.
    xcrun stapler staple "$runner" >/dev/null 2>&1 ||
        echo "the runner is notarized; its ticket couldn't be stapled (Apple tickets only its x86_64 slices)"
elif [ -n "$identity" ]; then
    echo "no notarization credentials: the simulator runner is signed but not notarized"
fi

mkdir -p "$out"
tarball="$out/WebDriverAgentRunner-Runner.tar.xz"
tar -cJf "$tarball" -C "$work" "$app"
cp "$cache/LICENSE" "$out/LICENSE-WebDriverAgent"
echo "made $tarball (WebDriverAgent $version, $(lipo -archs "$runner/WebDriverAgentRunner-Runner"), $(stat -f %z "$tarball" | awk '{ printf "%.1f MB", $1 / 1e6 }'))"
