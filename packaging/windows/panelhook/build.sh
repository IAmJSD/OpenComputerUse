#!/bin/sh
# Builds the panel hook for local testing on Windows. The release build is in
# .github/workflows/release.yml, which uses MSVC. Output:
# dist/OcuPanelHook.dll
#
# The hook is loaded into apps this agent starts, so it deliberately needs no
# C runtime of its own: everything it calls is a Win32 call. That is checked
# here, by looking at what the compiled object actually leaves undefined. If a
# libc function ever appears, write it out rather than pulling a runtime into
# someone else's process.
#
# Needs mingw-w64 (macOS: brew install mingw-w64).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out="$root/dist/OcuPanelHook.dll"
cc=${CC:-x86_64-w64-mingw32-gcc}
nm=${NM:-x86_64-w64-mingw32-nm}

mkdir -p "$(dirname "$out")"
obj=$(mktemp -d)/hook.o

"$cc" -c -o "$obj" "$here/hook.c" \
    -Wall -Wextra -Werror -std=c11 \
    -DUNICODE -D_UNICODE -D_WIN32_WINNT=0x0601

# What is left undefined should be only Windows imports (__imp_*, and the
# GUIDs that come from uuid.lib) and the compiler's own stack probe. Anything
# else is a runtime call, and a reason to write the function out instead.
stray=$("$nm" -u "$obj" \
    | sed -e 's/^ *U *//' \
    | grep -vE '^__imp_|^IID_|^CLSID_|^FOLDERID_|^GUID_|^LIBID_|^IIDIsEqualTo|^__chkstk_ms|^___chkstk_ms$|^__mingw_|^_CRT_|^__gcc_' \
    || true)
if [ -n "$stray" ]; then
    echo "the hook wants these, which are not Windows:" >&2
    echo "$stray" >&2
    echo "write them out rather than pulling a runtime into the host app" >&2
    exit 1
fi

# The shipping one. A mingw build carries mingw's runtime, which a release
# does not: releases build with MSVC (/MD) and check the same way there.
"$cc" -shared -o "$out" "$here/hook.c" \
    -Wall -Wextra -Werror -std=c11 \
    -DUNICODE -D_UNICODE -D_WIN32_WINNT=0x0601 \
    -lole32 -lshell32 -lshlwapi -luuid -lcomdlg32

echo "built $out"
echo "note: a mingw build. Releases use MSVC; see .github/workflows/release.yml."