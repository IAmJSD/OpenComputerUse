#!/bin/sh
# Builds dist/OcuPanelHook.dll with mingw-w64 for local testing (macOS:
# brew install mingw-w64). Releases build with MSVC in
# .github/workflows/release.yml.
#
# The hook runs inside other apps, so it must call nothing but Win32. This
# fails if the object leaves any other symbol undefined.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out="$root/dist/OcuPanelHook.dll"
cc=${CC:-x86_64-w64-mingw32-gcc}
nm=${NM:-x86_64-w64-mingw32-nm}
flags="-Wall -Wextra -Werror -std=c11 -DUNICODE -D_UNICODE -D_WIN32_WINNT=0x0601"

mkdir -p "$(dirname "$out")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# shellcheck disable=SC2086
"$cc" -c -o "$tmp/hook.o" "$here/hook.c" $flags

# Allowed: Win32 imports, uuid.lib GUIDs and the compiler's own helpers.
stray=$("$nm" -u "$tmp/hook.o" \
    | sed -e 's/^ *U *//' \
    | grep -vE '^__imp_|^IID_|^CLSID_|^FOLDERID_|^GUID_|^LIBID_|^IIDIsEqualTo|^__chkstk_ms|^___chkstk_ms$|^__mingw_|^_CRT_|^__gcc_' \
    || true)
if [ -n "$stray" ]; then
    echo "the hook calls these, which are not Win32:" >&2
    echo "$stray" >&2
    exit 1
fi

# shellcheck disable=SC2086
"$cc" -shared -o "$out" "$here/hook.c" $flags \
    -lole32 -lshell32 -lshlwapi -luuid -lcomdlg32

echo "built $out"
