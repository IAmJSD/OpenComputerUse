#!/bin/sh
# Builds dist/OpenComputerUse.apk with the Android SDK's build-tools alone:
# no Gradle, nothing downloaded. Needs a JDK (javac, keytool) and an SDK
# with build-tools and a platform of API 30 or newer (the newest is used).
#
# The SDK is found in $ANDROID_HOME, $ANDROID_SDK_ROOT, ~/Library/Android/sdk,
# Homebrew's android-commandlinetools, or ~/Android/Sdk. Signing uses
# $OCU_ANDROID_KEYSTORE ($OCU_ANDROID_KEYSTORE_PASSWORD, $OCU_ANDROID_KEY_ALIAS)
# when set, else a debug keystore made on first build in android/.keystore.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)

# The newest android-NN platform from 30 up in an SDK directory, if any.
newest_platform() {
    ls -d "$1"/platforms/android-* 2>/dev/null | sed -n 's|.*/android-\([0-9][0-9]*\)$|\1|p' |
        sort -n | awk '$1 >= 30' | tail -1
}

sdk=""
for d in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Library/Android/sdk" \
    /opt/homebrew/share/android-commandlinetools /usr/local/share/android-commandlinetools \
    "$HOME/Android/Sdk"; do
    if [ -n "$d" ] && [ -n "$(newest_platform "$d")" ]; then
        sdk=$d
        break
    fi
done
[ -n "$sdk" ] || { echo "no Android SDK with a platform of API 30 or newer; set ANDROID_HOME" >&2; exit 1; }
platform=android-$(newest_platform "$sdk")

bt=$(ls -d "$sdk"/build-tools/* 2>/dev/null | sort -V | tail -1)
[ -n "$bt" ] && [ -x "$bt/d8" ] || { echo "no build-tools in $sdk" >&2; exit 1; }
android_jar="$sdk/platforms/$platform/android.jar"

# versionName from the workspace, versionCode as MMmmpp.
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml")
code=$(echo "$version" | awk -F. '{ printf "%d", $1 * 10000 + $2 * 100 + $3 }')

out="$root/dist"
build="$here/build"
rm -rf "$build"
mkdir -p "$build/classes" "$build/dex" "$build/gen" "$out"

# Resources first: linking them writes R.java, which the code uses.
"$bt/aapt2" compile --dir "$here/res" -o "$build/res.zip"
"$bt/aapt2" link -o "$build/base.apk" -I "$android_jar" \
    --manifest "$here/AndroidManifest.xml" "$build/res.zip" \
    --java "$build/gen" \
    --min-sdk-version 30 --target-sdk-version 35 \
    --version-code "$code" --version-name "$version"

find "$here/src" "$build/gen" -name '*.java' > "$build/sources.txt"
# Java 8 against android.jar as the boot class path, so only Android's own
# java.* is visible; d8 desugars the lambdas.
javac -source 8 -target 8 -Xlint:-options -encoding UTF-8 \
    -bootclasspath "$android_jar:$bt/core-lambda-stubs.jar" -d "$build/classes" @"$build/sources.txt"

"$bt/d8" --release --min-api 30 --lib "$android_jar" --output "$build/dex" \
    $(find "$build/classes" -name '*.class')

(cd "$build/dex" && zip -q -j "$build/base.apk" classes.dex)

"$bt/zipalign" -f -p 4 "$build/base.apk" "$build/aligned.apk"

if [ -n "${OCU_ANDROID_KEYSTORE:-}" ]; then
    ks=$OCU_ANDROID_KEYSTORE
    pass=${OCU_ANDROID_KEYSTORE_PASSWORD:?set OCU_ANDROID_KEYSTORE_PASSWORD}
    alias=${OCU_ANDROID_KEY_ALIAS:?set OCU_ANDROID_KEY_ALIAS}
else
    ks="$here/.keystore/debug.jks"
    pass=android
    alias=debug
    if [ ! -f "$ks" ]; then
        mkdir -p "$here/.keystore"
        keytool -genkeypair -keystore "$ks" -storepass "$pass" -keypass "$pass" \
            -alias "$alias" -keyalg RSA -keysize 2048 -validity 10000 \
            -dname "CN=OpenComputerUse Debug" >/dev/null 2>&1
    fi
fi
"$bt/apksigner" sign --ks "$ks" --ks-pass "pass:$pass" --ks-key-alias "$alias" \
    --key-pass "pass:$pass" --out "$out/OpenComputerUse.apk" "$build/aligned.apk"
rm -f "$out/OpenComputerUse.apk.idsig"

echo "$out/OpenComputerUse.apk ($version)"
