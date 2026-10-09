/*
 * The panel hook: a DLL loaded into the apps opencomputeruse starts, which
 * hands us their open and save dialogs instead of showing them.
 *
 * The agent's pipe name arrives in OCU_PANEL_PIPE. Each dialog is sent on
 * it as one JSON line, and the hook waits for one back:
 *
 *     ->  {"pid":N,"kind":"open"|"save","multiple":bool,"folders":bool}
 *     <-  {"paths":["C:\\a.txt"]}     pick these
 *     <-  {"paths":[]}                cancelled
 *
 * Windows has two file dialogs, so there are two routes:
 *
 *   - the shell's modern one, IFileOpenDialog / IFileSaveDialog, reached
 *     through CoCreateInstance. The dialog is created as usual but never
 *     shown: Show is answered from the agent, and the shell's own GetResult
 *     and GetResults hand back what was chosen.
 *   - the older GetOpenFileNameW and GetSaveFileNameW, wrapped whole.
 *
 * Everything fails open. If the pipe is unreachable, the agent does not
 * answer, or anything at all goes wrong, the original function runs and the
 * app sees a normal dialog. An app must never break because the hook is
 * there, and must never wait on it forever: every read is time-limited.
 *
 * Only apps the agent itself starts, and only for its own sessions.
 *
 * NOT TESTED. This has never been compiled or run. Treat it as a reviewed
 * draft: the logic is meant to be right, but it needs a Windows machine,
 * an injected-app test, and a test that the app still starts when every
 * failure path here is taken.
 */

#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <objbase.h>
#include <shlobj.h>
/* The Windows SDK splits these two; mingw keeps them in one header each. */
#ifdef __MINGW32__
#include <shobjidl.h>
#else
#include <shobjidl_core.h>
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <commdlg.h>

/* How long any one exchange with the agent may take. A hook that hangs
   would hang the app, which is worse than not hooking at all. */
#define AGENT_TIMEOUT_MS 30000

/* Paths one dialog can answer with, and the longest one handled. */
#define MAX_PATHS 16
#define MAX_PATH_LEN 1024

/* Shell file dialog CLSIDs and IIDs, filled in once on first use. */
static CLSID g_clsid_open, g_clsid_save;
static IID g_iid_dialog, g_iid_open, g_iid_save;
static BOOL g_ids_ready = FALSE;

static HANDLE g_pipe = INVALID_HANDLE_VALUE;
static CRITICAL_SECTION g_lock;
static BOOL g_ready = FALSE;

static void ensure_ids(void)
{
    if (g_ids_ready) return;
    CLSIDFromString(L"{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}", &g_clsid_open);
    CLSIDFromString(L"{C0B4E2F3-BA21-4773-8DBA-335EC946EB8B}", &g_clsid_save);
    IIDFromString(L"{42f85136-db7e-439c-85f1-e4075d135fc8}", &g_iid_dialog);
    IIDFromString(L"{d57c7288-d4ad-4768-be02-9d969532d960}", &g_iid_open);
    IIDFromString(L"{84bccd23-5fde-4cdb-aea4-af64b83d78ab}", &g_iid_save);
    g_ids_ready = TRUE;
}

/* ---------------------------------------------------------------- JSON */

/* Just enough string and memory handling to read one reply, so the hook
   pulls in no runtime of its own inside a process it was put into. */
static void copy_bytes(void *dst, const void *src, size_t n)
{
    unsigned char *d = (unsigned char *)dst;
    const unsigned char *s = (const unsigned char *)src;
    for (size_t i = 0; i < n; i++) d[i] = s[i];
}

static int has_newline(const char *s, size_t n)
{
    for (size_t i = 0; i < n; i++)
        if (s[i] == '\n') return 1;
    return 0;
}

static int same_str(const char *a, const char *b)
{
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

/* Case-insensitive, for module names, which differ only in case between
   the manifest and the import table more often than one would like. */
static int same_str_nocase(const char *a, const char *b)
{
    while (*a && *b) {
        char ca = *a, cb = *b;
        if (ca >= 'A' && ca <= 'Z') ca = (char)(ca - 'A' + 'a');
        if (cb >= 'A' && cb <= 'Z') cb = (char)(cb - 'A' + 'a');
        if (ca != cb) return 0;
        a++;
        b++;
    }
    return *a == *b;
}

/* GUIDs are compared by hand because IsEqualGUID expands to a memcmp, and
   this hook wants no runtime of its own. */
static int same_guid(const GUID *a, const GUID *b)
{
    const unsigned char *x = (const unsigned char *)a, *y = (const unsigned char *)b;
    for (size_t i = 0; i < sizeof(GUID); i++)
        if (x[i] != y[i]) return 0;
    return 1;
}

static size_t wlen(const wchar_t *s)
{
    const wchar_t *p = s;
    while (*p) p++;
    return (size_t)(p - s);
}

/* The last path separator in a path, which is what splits a directory from
   the file in it. */
static const wchar_t *last_sep(const wchar_t *s)
{
    const wchar_t *last = NULL;
    for (; *s; s++)
        if (*s == L'\\') last = s;
    return last;
}

static size_t slen(const char *s)
{
    const char *p = s;
    while (*p) p++;
    return (size_t)(p - s);
}

static const char *find_char(const char *s, char c)
{
    while (*s && *s != c) s++;
    return *s ? s : NULL;
}

static const char *find_str(const char *hay, const char *needle)
{
    size_t n = slen(needle);
    if (!n) return hay;
    for (; *hay; hay++) {
        size_t i = 0;
        while (i < n && hay[i] == needle[i]) i++;
        if (i == n) return hay;
    }
    return NULL;
}

/* Decodes one UTF-8 sequence at p into *out, advancing p past it. Returns
   the number of bytes consumed. A malformed sequence is consumed as one
   replacement character rather than looped on. */
static int utf8_next(const char **p, wchar_t *out)
{
    const unsigned char *s = (const unsigned char *)*p;
    unsigned char c = s[0];
    int extra;
    unsigned long cp;

    if (c < 0x80) {
        *out = (wchar_t)c;
        *p += 1;
        return 1;
    } else if ((c & 0xe0) == 0xc0) {
        extra = 1;
        cp = c & 0x1fu;
    } else if ((c & 0xf0) == 0xe0) {
        extra = 2;
        cp = c & 0x0fu;
    } else if ((c & 0xf8) == 0xf0) {
        extra = 3;
        cp = c & 0x07u;
    } else {
        *out = 0xfffd;
        *p += 1;
        return 1;
    }
    for (int i = 1; i <= extra; i++) {
        if ((s[i] & 0xc0) != 0x80) { /* truncated: not a sequence at all */
            *out = 0xfffd;
            *p += 1;
            return 1;
        }
        cp = (cp << 6) | (s[i] & 0x3fu);
    }
    /* Surrogates would need a pair, and a path carrying one is not a path
       Windows produced. */
    if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) cp = 0xfffd;
    if (cp > 0xffff) {
        /* Outside the BMP: keep the replacement, since wchar_t here is
           16 bits and splitting it would corrupt the path. */
        *out = 0xfffd;
    } else {
        *out = (wchar_t)cp;
    }
    *p += extra + 1;
    return extra + 1;
}

/* Reads the reply's "paths" array into out[]. Returns how many paths it
   holds, or -1 if the reply has no "paths" at all, which the callers treat
   as a cancel. */
static int parse_paths(const char *json, wchar_t out[][MAX_PATH_LEN])
{
    const char *p = find_str(json, "\"paths\"");
    if (!p) return 0;
    p = find_char(p, '[');
    if (!p) return 0;
    p++;

    int n = 0;
    while (*p && *p != ']') {
        if (*p != '"') {
            p++;
            continue;
        }
        p++; /* the opening quote */
        wchar_t *w = out[n < MAX_PATHS ? n : MAX_PATHS - 1];
        int i = 0;
        while (*p && *p != '"') {
            wchar_t ch;
            if (*p == '\\') {
                p++;
                switch (*p) {
                case 'u': {
                    unsigned code = 0;
                    if (slen(p + 1) >= 4) {
                        for (int k = 1; k <= 4; k++) {
                            char c = p[k];
                            unsigned d = (c >= '0' && c <= '9')   ? c - '0'
                                         : (c >= 'a' && c <= 'f') ? c - 'a' + 10
                                         : (c >= 'A' && c <= 'F') ? c - 'A' + 10
                                                                  : 0xffff;
                            if (d == 0xffff) break;
                            code = (code << 4) | d;
                        }
                    }
                    p += 5;
                    /* A lone surrogate is dropped rather than written, so
                       a mangled path fails visibly instead of silently. */
                    ch = (code >= 0xd800 && code <= 0xdfff) ? 0xfffd : (wchar_t)code;
                    break;
                }
                case 'n': ch = L'\n'; p++; break;
                case 't': ch = L'\t'; p++; break;
                case 'r': ch = L'\r'; p++; break;
                case '\\': case '"': case '/': ch = (wchar_t)*p; p++; break;
                default: ch = (wchar_t)*p; p++; break;
                }
            } else {
                p += utf8_next(&p, &ch);
            }
            if (i < MAX_PATH_LEN - 1) w[i++] = ch;
        }
        w[i] = 0;
        if (*p == '"') p++;
        if (n < MAX_PATHS) n++;
    }
    return n;
}

/* --------------------------------------------------------------- pipe */

/* Reads one line from the pipe, giving up after AGENT_TIMEOUT_MS. Polling
   rather than a blocking read, because a blocking one has no timeout to
   give and would hang the app if the agent wedged. */
static int read_line(char *buf, DWORD cap)
{
    DWORD total = 0;
    DWORD start = GetTickCount();
    for (;;) {
        DWORD avail = 0;
        if (!PeekNamedPipe(g_pipe, NULL, 0, NULL, &avail, NULL)) return -1;
        if (avail > 0) {
            DWORD room = cap - total - 1;
            DWORD want = avail < room ? avail : room;
            DWORD got = 0;
            if (want == 0) return (int)total;
            if (!ReadFile(g_pipe, buf + total, want, &got, NULL)) return -1;
            total += got;
            buf[total] = 0;
            if (has_newline(buf, total)) return (int)total;
        } else {
            if (GetTickCount() - start > AGENT_TIMEOUT_MS) return -1;
            Sleep(20);
        }
    }
}

/* Asks the agent about a dialog. Returns how many paths it chose, or -1 if
   it could not be reached or did not answer, which means the caller must
   show the real dialog. An empty result is a cancel, not a failure. */
static int ask(const char *kind, BOOL multiple, BOOL folders, wchar_t out[][MAX_PATH_LEN])
{
    if (g_pipe == INVALID_HANDLE_VALUE) return -1;
    DWORD mode = PIPE_READMODE_BYTE;
    if (!SetNamedPipeHandleState(g_pipe, &mode, NULL, NULL)) return -1;

    /* Built by hand rather than with the CRT, so the hook needs no runtime
       of its own inside someone else's process. Every part but the pid is a
       fixed string, and the buffer is far larger than they can fill. */
    char request[128];
    const char *head = "{\"pid\":";
    const char *mid = ",\"kind\":\"";
    const char *tail1 = "\",\"multiple\":";
    const char *tail2 = ",\"folders\":";
    const char *yes = "true";
    const char *no = "false";
    const char *end = "}\n";
    size_t n = 0;

    /* The pid, digits from the end backwards, so no formatting needed. */
    char num[12];
    unsigned long pid = (unsigned long)GetCurrentProcessId();
    int numlen = 0;
    do {
        num[numlen++] = (char)('0' + (pid % 10));
        pid /= 10;
    } while (pid && numlen < (int)sizeof(num));
    char digits[12];
    for (int i = 0; i < numlen; i++) digits[i] = num[numlen - 1 - i];
    digits[numlen] = 0;

    const char *parts[] = {head, digits, mid, kind, tail1, multiple ? yes : no, tail2,
                           folders ? yes : no, end};
    for (int i = 0; i < (int)(sizeof(parts) / sizeof(parts[0])); i++) {
        const char *s = parts[i];
        while (*s && n + 1 < sizeof(request)) request[n++] = *s++;
    }
    request[n] = 0;

    DWORD written = 0;
    if (!WriteFile(g_pipe, request, (DWORD)n, &written, NULL)) return -1;

    char reply[64 * 1024];
    int got = read_line(reply, (DWORD)sizeof(reply));
    if (got <= 0) return -1;
    return parse_paths(reply, out);
}

/* --------------------------------------------------- the modern dialog */

/* Wraps a shell file dialog.
 *
 * The vtable here is IFileOpenDialogVtbl, which is exactly IFileDialogVtbl
 * plus GetResults and GetSelectedItems. That is deliberate: a save dialog
 * only has the shorter table, so only its own slots are copied and only
 * they are ever called, while an open dialog gets both tables' worth and so
 * can answer a multiple selection. */
typedef struct HookedDialog {
    /* First, because the app calls through the pointer it was handed and
       that has to land on this struct. */
    IFileOpenDialogVtbl *lpVtbl;
    IFileOpenDialogVtbl vtbl;
    LONG ref;
    IFileDialog *real; /* the shell's own object, vtable aside */
    int is_save;
    /* Set by Show once the agent has answered, read by the GetResult that
       follows on the same thread. */
    int answered;
    int npaths;
    wchar_t paths[MAX_PATHS][MAX_PATH_LEN];
} HookedDialog;

static HookedDialog *self_of(IFileOpenDialog *d)
{
    return (HookedDialog *)d;
}

static HRESULT STDMETHODCALLTYPE hook_QueryInterface(IFileOpenDialog *d, REFIID riid,
                                                    void **out)
{
    HookedDialog *h = self_of(d);
    /* Forwarded, so every interface the app asks for behaves normally. The
       shell's object comes back, which means a caller that unwraps it and
       calls Show on it sees the real dialog: a fair outcome, since that
       shows a dialog rather than hanging. */
    return IFileDialog_QueryInterface(h->real, riid, out);
}

static ULONG STDMETHODCALLTYPE hook_AddRef(IFileOpenDialog *d)
{
    HookedDialog *h = self_of(d);
    return (ULONG)InterlockedIncrement(&h->ref);
}

static ULONG STDMETHODCALLTYPE hook_Release(IFileOpenDialog *d)
{
    HookedDialog *h = self_of(d);
    LONG n = InterlockedDecrement(&h->ref);
    if (n == 0) {
        IFileDialog_Release(h->real);
        HeapFree(GetProcessHeap(), 0, h);
    }
    return (ULONG)n;
}

static HRESULT STDMETHODCALLTYPE hook_Show(IFileOpenDialog *d, HWND owner)
{
    HookedDialog *h = self_of(d);
    wchar_t picks[MAX_PATHS][MAX_PATH_LEN];
    FILEOPENDIALOGOPTIONS opts = 0;
    int n;

    EnterCriticalSection(&g_lock);
    /* GetOptions before answering: an open dialog set for folders says so
       only here. */
    IFileDialog_GetOptions(h->real, &opts);
    n = ask(h->is_save ? "save" : "open", (opts & FOS_ALLOWMULTISELECT) ? TRUE : FALSE,
            (opts & FOS_PICKFOLDERS) ? TRUE : FALSE, picks);
    if (n > 0) {
        copy_bytes(h->paths, picks, sizeof(picks));
        h->npaths = n;
        h->answered = 1;
    }
    LeaveCriticalSection(&g_lock);

    if (n < 0) {
        /* No agent, or no answer: the app gets its own dialog. */
        return IFileDialog_Show(h->real, owner);
    }
    if (n == 0) {
        /* Cancelled, and reported exactly as the shell reports a cancel. */
        return HRESULT_FROM_WIN32(ERROR_CANCELLED);
    }
    /* S_OK without having shown anything: the shell takes that as the
       dialog closed, and the app asks for its result as usual. */
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE hook_GetResult(IFileOpenDialog *d, IShellItem **out)
{
    HookedDialog *h = self_of(d);
    if (!h->answered || h->npaths < 1)
        return IFileDialog_GetResult(h->real, out);
    return SHCreateItemFromParsingName(h->paths[0], NULL, &IID_IShellItem, (void **)out);
}

static HRESULT STDMETHODCALLTYPE hook_GetResults(IFileOpenDialog *d, IShellItemArray **out)
{
    HookedDialog *h = self_of(d);
    if (!h->answered || h->npaths < 1)
        /* Not ours to answer, so the shell's own, reached through the longer
           table this one came in on. */
        return ((IFileOpenDialogVtbl *)h->real->lpVtbl)
            ->GetResults((IFileOpenDialog *)h->real, out);

    ITEMIDLIST **ids =
        (ITEMIDLIST **)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY,
                                 sizeof(ITEMIDLIST *) * h->npaths);
    if (!ids) return E_OUTOFMEMORY;
    HRESULT hr = E_OUTOFMEMORY;
    int made = 0;
    for (int i = 0; i < h->npaths; i++) {
        ids[made] = ILCreateFromPathW(h->paths[i]);
        if (ids[made]) made++;
    }
    if (made > 0)
        hr = SHCreateShellItemArrayFromIDLists((UINT)made, (const ITEMIDLIST **)ids, out);
    for (int i = 0; i < made; i++) CoTaskMemFree(ids[i]);
    HeapFree(GetProcessHeap(), 0, ids);
    return hr;
}

/* Takes over a dialog the shell just made: copies its vtable, points the
   methods that matter at ours, and leaves the object pointing here. */
static IFileDialog *wrap(IFileDialog *real, int is_save)
{
    HookedDialog *h =
        (HookedDialog *)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(HookedDialog));
    if (!h) return real;

    h->ref = 1;
    h->real = real;
    h->is_save = is_save;
    /* Only as many slots as the object's own table really has: a save
       dialog's stops at GetFilter, and reading past that would be reading
       whatever the next thing in memory happens to be. */
    copy_bytes(&h->vtbl, real->lpVtbl,
            is_save ? sizeof(IFileDialogVtbl) : sizeof(IFileOpenDialogVtbl));
    h->vtbl.QueryInterface = hook_QueryInterface;
    h->vtbl.AddRef = hook_AddRef;
    h->vtbl.Release = hook_Release;
    h->vtbl.Show = hook_Show;
    h->vtbl.GetResult = hook_GetResult;
    /* GetResults only exists on the open dialog, and only that one is ever
       asked for it. */
    if (!is_save) h->vtbl.GetResults = hook_GetResults;

    h->lpVtbl = &h->vtbl;
    IFileDialog_AddRef(real); /* held for our own lifetime */
    real->lpVtbl = (CONST_VTBL IFileDialogVtbl *)&h->vtbl;
    return (IFileDialog *)h;
}

/* ------------------------------------------------ the older functions */

/* Fills the caller's buffer for GetOpenFileNameW and GetSaveFileNameW. The
   shell writes one path, or, when OFN_ALLOWMULTISELECT is set, the folder
   first and then each name, each double-null terminated. Both are produced
   here. */
static BOOL fill_legacy(OPENFILENAMEW *ofn, wchar_t picks[][MAX_PATH_LEN], int n)
{
    wchar_t *buf = ofn->lpstrFile;
    DWORD cap = ofn->nMaxFile;
    if (!buf || cap == 0) return FALSE;

    if (n == 1) {
        if (wlen(picks[0]) + 1 > cap) return FALSE;
        for (size_t i = 0; i <= wlen(picks[0]); i++) buf[i] = picks[0][i];
        return TRUE;
    }
    if (!(ofn->Flags & OFN_ALLOWMULTISELECT)) return FALSE;

    /* The shared folder, then each name after it. */
    const wchar_t *first = picks[0];
    const wchar_t *slash = last_sep(first);
    size_t dir_len = slash ? (size_t)(slash - first) + 1 : 0;
    if (!dir_len) return FALSE;

    size_t need = dir_len + 1;
    for (int i = 0; i < n; i++) {
        const wchar_t *base = last_sep(picks[i]);
        need += (base ? wlen(base + 1) : wlen(picks[i])) + 1;
    }
    if (need + 1 > cap) return FALSE;

    wchar_t *p = buf;
    copy_bytes(p, first, dir_len * sizeof(wchar_t));
    p += dir_len;
    *p++ = 0;
    for (int i = 0; i < n; i++) {
        const wchar_t *base = last_sep(picks[i]);
        base = base ? base + 1 : picks[i];
        size_t len = wlen(base) + 1;
        copy_bytes(p, base, len * sizeof(wchar_t));
        p += len;
    }
    *p = 0; /* the extra null the shell's own multi-select format ends on */
    return TRUE;
}

static BOOL WINAPI hook_GetOpenFileNameW(OPENFILENAMEW *ofn)
{
    if (!ofn || !g_ready) return GetOpenFileNameW(ofn);
    wchar_t picks[MAX_PATHS][MAX_PATH_LEN];
    int n;
    EnterCriticalSection(&g_lock);
    n = ask("open", (ofn->Flags & OFN_ALLOWMULTISELECT) ? TRUE : FALSE, FALSE, picks);
    LeaveCriticalSection(&g_lock);
    if (n < 0) return GetOpenFileNameW(ofn);
    if (n == 0) return FALSE; /* cancelled */
    if (!fill_legacy(ofn, picks, n)) return GetOpenFileNameW(ofn);
    ofn->nFilterIndex = 1;
    return TRUE;
}

static BOOL WINAPI hook_GetSaveFileNameW(OPENFILENAMEW *ofn)
{
    if (!ofn || !g_ready) return GetSaveFileNameW(ofn);
    wchar_t picks[MAX_PATHS][MAX_PATH_LEN];
    int n;
    EnterCriticalSection(&g_lock);
    n = ask("save", FALSE, FALSE, picks);
    LeaveCriticalSection(&g_lock);
    if (n < 0) return GetSaveFileNameW(ofn);
    if (n == 0) return FALSE;
    if (!fill_legacy(ofn, picks, n)) return GetSaveFileNameW(ofn);
    ofn->nFilterIndex = 1;
    return TRUE;
}

/* ------------------------------------------------------------- CoCreate */

typedef HRESULT(WINAPI *CoCreateInstanceFn)(REFCLSID, LPUNKNOWN, DWORD, REFIID, LPVOID);
static CoCreateInstanceFn g_real_cocreate = NULL;

static HRESULT WINAPI hook_CoCreateInstance(REFCLSID rclsid, LPUNKNOWN pUnk, DWORD dwClsContext,
                                            REFIID riid, LPVOID ppv)
{
    HRESULT hr = g_real_cocreate(rclsid, pUnk, dwClsContext, riid, ppv);
    if (FAILED(hr) || !ppv || !g_ready) return hr;

    ensure_ids();
    int is_save;
    if (same_guid(rclsid, &g_clsid_open))
        is_save = 0;
    else if (same_guid(rclsid, &g_clsid_save))
        is_save = 1;
    else
        return hr; /* some other COM object entirely */

    /* Apps ask for whichever of the three dialog interfaces they need, and
       the one they get back is what they go on to call Show through. */
    if (!same_guid(riid, &g_iid_dialog) && !same_guid(riid, &g_iid_open) &&
        !same_guid(riid, &g_iid_save))
        return hr;

    IFileDialog *dlg = *(IFileDialog **)ppv;
    if (!dlg) return hr;
    *(IFileDialog **)ppv = wrap(dlg, is_save);
    return hr;
}

/* ---------------------------------------------------- import patching */

/* Replaces one entry in a module's import table. The shell's dialogs are
   reached through imports, so this is where a call the app makes directly
   can be caught. Delay-loaded imports are not covered: an app that reaches
   for a dialog through one gets its own dialog, which is the fail-open
   outcome everywhere else here. */
static void patch_imports(HMODULE mod, const char *module, const char *name, void *replacement)
{
    if (!mod) return;
    PIMAGE_DOS_HEADER dos = (PIMAGE_DOS_HEADER)mod;
    if (dos->e_magic != IMAGE_DOS_SIGNATURE) return;
    PIMAGE_NT_HEADERS nt = (PIMAGE_NT_HEADERS)((BYTE *)mod + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE) return;

    IMAGE_DATA_DIRECTORY dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!dir.VirtualAddress) return;
    PIMAGE_IMPORT_DESCRIPTOR imports =
        (PIMAGE_IMPORT_DESCRIPTOR)((BYTE *)mod + dir.VirtualAddress);

    for (PIMAGE_IMPORT_DESCRIPTOR d = imports; d->Name; d++) {
        const char *dll_name = (const char *)((BYTE *)mod + d->Name);
        if (!same_str_nocase(dll_name, module)) continue;
        if (!d->OriginalFirstThunk || !d->FirstThunk) continue;

        PIMAGE_THUNK_DATA lookup = (PIMAGE_THUNK_DATA)((BYTE *)mod + d->OriginalFirstThunk);
        PIMAGE_THUNK_DATA patch = (PIMAGE_THUNK_DATA)((BYTE *)mod + d->FirstThunk);
        for (; lookup->u1.AddressOfData; lookup++, patch++) {
            /* Ordinals have no name to match against. */
            if (IMAGE_SNAP_BY_ORDINAL(lookup->u1.Ordinal)) continue;
            PIMAGE_IMPORT_BY_NAME n =
                (PIMAGE_IMPORT_BY_NAME)((BYTE *)mod + lookup->u1.AddressOfData);
            if (!same_str((const char *)n->Name, name)) continue;
            InterlockedExchangePointer((PVOID *)&patch->u1.Function, replacement);
            return;
        }
    }
}

/* ------------------------------------------------------------- entry */

BOOL WINAPI DllMain(HINSTANCE self, DWORD reason, LPVOID reserved)
{
    (void)self;
    (void)reserved;
    if (reason != DLL_PROCESS_ATTACH) return TRUE;

    /* Off before anything the app starts can inherit it. */
    wchar_t name[256];
    DWORD got = GetEnvironmentVariableW(L"OCU_PANEL_PIPE", name, 256);
    SetEnvironmentVariableW(L"OCU_PANEL_PIPE", NULL);
    if (!got || got >= 256) return TRUE; /* not ours to do anything with */

    ensure_ids();

    /* Opened once and kept: a pipe serves one client at a time, and there
       is exactly one client here, the app itself. */
    g_pipe = CreateFileW(name, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING,
                         FILE_ATTRIBUTE_NORMAL, NULL);
    if (g_pipe == INVALID_HANDLE_VALUE) return TRUE;

    InitializeCriticalSection(&g_lock);

    HMODULE exe = GetModuleHandleA(NULL);
    HMODULE ole32 = GetModuleHandleA("ole32.dll");
    /* comdlg32 is loaded on demand, so it may not be there yet. Without it
       only the modern route works; nothing else is affected. */
    HMODULE comdlg = LoadLibraryA("comdlg32.dll");

    g_ready = TRUE;

    if (exe && ole32) {
        FARPROC p = GetProcAddress(ole32, "CoCreateInstance");
        if (p) {
            g_real_cocreate = (CoCreateInstanceFn)(void *)p;
            patch_imports(exe, "ole32.dll", "CoCreateInstance", (void *)hook_CoCreateInstance);
        }
    }
    if (exe && comdlg) {
        patch_imports(exe, "comdlg32.dll", "GetOpenFileNameW", (void *)hook_GetOpenFileNameW);
        patch_imports(exe, "comdlg32.dll", "GetSaveFileNameW", (void *)hook_GetSaveFileNameW);
    }
    return TRUE;
}
