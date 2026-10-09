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
 *     through CoCreateInstance. The dialog is created as usual and its
 *     vtable patched in place: Show is answered from the agent, GetResult
 *     and GetResults hand back what it chose, and every other method is the
 *     shell's own, called on the shell's own object.
 *   - the older GetOpenFileNameW and GetSaveFileNameW, wrapped whole.
 *
 * Everything fails open. If the pipe is unreachable, the agent does not
 * answer, or anything at all goes wrong, the original function runs and the
 * app sees a normal dialog. An app must never break because the hook is
 * there, and must never wait on it forever: every read is time-limited.
 *
 * Only apps the agent itself starts, and only for its own sessions.
 *
 * The open, save and cancel paths, modern and legacy, are exercised on a
 * real Windows runner by tests/file_dialogs.rs. Set OCU_PANEL_LOG to a file
 * to trace each step while bringing it up on a new machine.
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

/* Paths one dialog can answer with, and the longest one handled. A reply
   past either limit is not one the hook can follow, and shows the real
   dialog. */
#define MAX_PATHS 16
#define MAX_PATH_LEN 1024

/* The pipe, connected on the first dialog rather than at load: DllMain runs
   under the loader lock, where waiting on anything is unsafe. */
static wchar_t g_pipe_name[256];
static HANDLE g_pipe = INVALID_HANDLE_VALUE;

/* Held for each exchange with the agent. The buffers below belong to
   whoever holds it, and are static so no hook puts tens of kilobytes on an
   app thread's stack. */
static CRITICAL_SECTION g_lock;
static char g_reply[64 * 1024];
static wchar_t g_picks[MAX_PATHS][MAX_PATH_LEN];

/* --------------------------------------------------------------- debug */

/* Appends "<msg> <value>" to the file named by OCU_PANEL_LOG, when it is
   set. For bringing up the hook on a real machine: it tells which step a
   dialog reached without the hook needing to reach the agent first. Off (no
   variable) it does nothing, so it costs a shipped hook one env lookup. */
static void dbg(const char *msg, long value)
{
    static wchar_t path[260];
    static int enabled = -1;
    if (enabled < 0) {
        DWORD n = GetEnvironmentVariableW(L"OCU_PANEL_LOG", path, 260);
        enabled = (n > 0 && n < 260) ? 1 : 0;
    }
    if (enabled != 1) return;

    HANDLE f = CreateFileW(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                           OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) return;

    char line[256];
    size_t n = 0;
    for (const char *s = msg; *s && n < 200; s++) line[n++] = *s;
    line[n++] = ' ';
    char num[21];
    int ni = 0;
    unsigned long v = value < 0 ? (line[n++] = '-', (unsigned long)-value) : (unsigned long)value;
    do {
        num[ni++] = (char)('0' + v % 10);
        v /= 10;
    } while (v && ni < 20);
    while (ni) line[n++] = num[--ni];
    line[n++] = '\n';

    DWORD wrote = 0;
    WriteFile(f, line, (DWORD)n, &wrote, NULL);
    CloseHandle(f);
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

/* Where the file's name starts in a path: just past the last separator, or
   0 if there is none. */
static size_t name_offset(const wchar_t *s)
{
    size_t at = 0;
    for (size_t i = 0; s[i]; i++)
        if (s[i] == L'\\') at = i + 1;
    return at;
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

static int hex_digit(char c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

/* Decodes one UTF-8 sequence at *p into out, advancing *p past it, and
   returns how many UTF-16 units it made: two for a character outside the
   BMP, which Windows stores as a surrogate pair. A malformed sequence is
   consumed as one replacement character rather than looped on. */
static int utf8_next(const char **p, wchar_t out[2])
{
    const unsigned char *s = (const unsigned char *)*p;
    unsigned char c = s[0];
    int extra;
    unsigned long cp;

    if (c < 0x80) {
        out[0] = (wchar_t)c;
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
        out[0] = 0xfffd;
        *p += 1;
        return 1;
    }
    for (int i = 1; i <= extra; i++) {
        if ((s[i] & 0xc0) != 0x80) { /* truncated: not a sequence at all */
            out[0] = 0xfffd;
            *p += 1;
            return 1;
        }
        cp = (cp << 6) | (s[i] & 0x3fu);
    }
    *p += extra + 1;
    if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) {
        out[0] = 0xfffd;
        return 1;
    }
    if (cp > 0xffff) {
        cp -= 0x10000;
        out[0] = (wchar_t)(0xd800 + (cp >> 10));
        out[1] = (wchar_t)(0xdc00 + (cp & 0x3ff));
        return 2;
    }
    out[0] = (wchar_t)cp;
    return 1;
}

/* Reads the reply's "paths" array into out[]. Returns how many paths it
   holds, 0 for a cancel, or -1 for a reply this cannot follow in full: no
   "paths", a path or escape cut short, a path too long, or too many paths.
   Callers treat -1 as no answer and show the real dialog. */
static int parse_paths(const char *json, wchar_t out[][MAX_PATH_LEN])
{
    const char *p = find_str(json, "\"paths\"");
    if (!p) return -1;
    p = find_char(p, '[');
    if (!p) return -1;
    p++;

    int n = 0;
    while (*p && *p != ']') {
        if (*p != '"') {
            p++;
            continue;
        }
        p++; /* the opening quote */
        if (n == MAX_PATHS) return -1;
        wchar_t *w = out[n];
        int i = 0;
        while (*p && *p != '"') {
            wchar_t ch[2];
            int units = 1;
            if (*p == '\\') {
                p++;
                if (*p == 'u') {
                    unsigned code = 0;
                    /* Stops at the first non-digit, the end included, so
                       nothing past the reply is read. */
                    for (int k = 1; k <= 4; k++) {
                        int d = hex_digit(p[k]);
                        if (d < 0) return -1;
                        code = (code << 4) | (unsigned)d;
                    }
                    p += 5;
                    /* Kept as it comes, surrogates too: a path is UTF-16
                       units, and a pair arrives as two escapes. */
                    ch[0] = (wchar_t)code;
                } else {
                    switch (*p) {
                    case '\0': return -1; /* the reply ends in a backslash */
                    case 'n': ch[0] = L'\n'; break;
                    case 't': ch[0] = L'\t'; break;
                    case 'r': ch[0] = L'\r'; break;
                    case 'b': ch[0] = L'\b'; break;
                    case 'f': ch[0] = L'\f'; break;
                    default: ch[0] = (wchar_t)(unsigned char)*p; break;
                    }
                    p++;
                }
            } else {
                units = utf8_next(&p, ch);
            }
            if (i + units > MAX_PATH_LEN - 1) return -1;
            for (int k = 0; k < units; k++) w[i++] = ch[k];
        }
        if (*p != '"') return -1; /* the reply ends inside a path */
        p++;
        w[i] = 0;
        n++;
    }
    return *p == ']' ? n : -1;
}

/* --------------------------------------------------------------- pipe */

static void disconnect(void)
{
    if (g_pipe == INVALID_HANDLE_VALUE) return;
    CloseHandle(g_pipe);
    g_pipe = INVALID_HANDLE_VALUE;
}

/* Connects to the agent if not already. The agent makes a fresh pipe
   instance as soon as one is taken, but two apps connecting at once can
   find every instance busy for a moment, so that is waited out briefly. */
static BOOL connect_agent(void)
{
    if (g_pipe != INVALID_HANDLE_VALUE) return TRUE;
    for (int tries = 0; tries < 3; tries++) {
        g_pipe = CreateFileW(g_pipe_name, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING,
                             FILE_ATTRIBUTE_NORMAL, NULL);
        if (g_pipe != INVALID_HANDLE_VALUE) {
            DWORD mode = PIPE_READMODE_BYTE;
            if (SetNamedPipeHandleState(g_pipe, &mode, NULL, NULL)) return TRUE;
            disconnect();
            return FALSE;
        }
        if (GetLastError() != ERROR_PIPE_BUSY || !WaitNamedPipeW(g_pipe_name, 2000))
            return FALSE;
    }
    return FALSE;
}

/* Reads one line from the pipe into g_reply, giving up after
   AGENT_TIMEOUT_MS. Polling rather than a blocking read, because a blocking
   one has no timeout to give and would hang the app if the agent wedged.
   Returns -1 on a timeout, a broken pipe, or a line too long to hold. */
static int read_line(void)
{
    DWORD cap = (DWORD)sizeof(g_reply);
    DWORD total = 0;
    DWORD start = GetTickCount();
    for (;;) {
        DWORD avail = 0;
        if (!PeekNamedPipe(g_pipe, NULL, 0, NULL, &avail, NULL)) return -1;
        if (avail > 0) {
            DWORD room = cap - total - 1;
            DWORD want = avail < room ? avail : room;
            DWORD got = 0;
            if (want == 0) return -1;
            if (!ReadFile(g_pipe, g_reply + total, want, &got, NULL)) return -1;
            total += got;
            g_reply[total] = 0;
            if (has_newline(g_reply, total)) return (int)total;
        } else {
            if (GetTickCount() - start > AGENT_TIMEOUT_MS) return -1;
            Sleep(20);
        }
    }
}

/* Asks the agent about a dialog. Returns how many paths it chose, or -1 if
   it could not be reached or did not answer, which means the caller must
   show the real dialog. An empty result is a cancel, not a failure. The
   caller holds g_lock. */
static int ask(const char *kind, BOOL multiple, BOOL folders, wchar_t out[][MAX_PATH_LEN])
{
    if (!connect_agent()) {
        dbg("connect failed err=", (long)GetLastError());
        return -1;
    }
    dbg("connected", 0);

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
    if (!WriteFile(g_pipe, request, (DWORD)n, &written, NULL)) {
        disconnect();
        return -1;
    }
    if (read_line() < 0) {
        /* Hung up rather than kept: a reply arriving late would otherwise
           be read as the answer to the next dialog. The next dialog
           reconnects. */
        disconnect();
        return -1;
    }
    return parse_paths(g_reply, out);
}

/* --------------------------------------------------- the modern dialog */

/* A shell file dialog's patched vtable, and what the hook keeps about the
 * dialog beside it.
 *
 * The table is first, so the object's own lpVtbl leads straight back here:
 * the hooks are called with the shell's object as `this`, as every method
 * is, and find their state through it. Methods the hook leaves alone are
 * the shell's own entries, copied, so they run exactly as before.
 *
 * Each kind of dialog has its own table length: an open dialog's adds
 * GetResults and GetSelectedItems to IFileDialog's, and a save dialog's adds
 * five others. The table is copied at the length of the interface it was
 * read through, so no slot past the shell's own is ever read. */
typedef struct DialogHook {
    union {
        IFileDialogVtbl base;
        IFileOpenDialogVtbl open;
        IFileSaveDialogVtbl save;
    } vtbl;
    const IFileDialogVtbl *orig; /* the shell's own table */
    int is_save;
    /* Set by Show once the agent has answered, read by the GetResult that
       follows. Cleared each time Show is called. */
    int npaths;
    wchar_t (*paths)[MAX_PATH_LEN];
} DialogHook;

static DialogHook *hook_of(IFileDialog *d)
{
    return (DialogHook *)d->lpVtbl;
}

static void forget_answer(DialogHook *h)
{
    if (h->paths) HeapFree(GetProcessHeap(), 0, h->paths);
    h->paths = NULL;
    h->npaths = 0;
}

/* An ID list for a path. A real one where the path exists, and a simple
   one where it does not yet, which is what a save dialog's answer usually
   is: ILCreateFromPathW fails on a file that is not there. */
static PIDLIST_ABSOLUTE id_list_of(const wchar_t *path)
{
    PIDLIST_ABSOLUTE id = ILCreateFromPathW(path);
    return id ? id : SHSimpleIDListFromPath(path);
}

static ULONG STDMETHODCALLTYPE hook_Release(IFileDialog *d)
{
    DialogHook *h = hook_of(d);
    ULONG n = h->orig->Release(d);
    /* The object is gone, and nothing will look at this table again. A
       last release made through one of the object's other interfaces never
       comes here, which leaks this small block, nothing worse. */
    if (n == 0) {
        forget_answer(h);
        HeapFree(GetProcessHeap(), 0, h);
    }
    return n;
}

static HRESULT STDMETHODCALLTYPE hook_Show(IFileDialog *d, HWND owner)
{
    DialogHook *h = hook_of(d);
    FILEOPENDIALOGOPTIONS opts = 0;
    int n;

    dbg("show save=", h->is_save);
    /* A dialog shown again starts over, so a real dialog shown this time
       never hands back what the agent chose last time. */
    forget_answer(h);
    /* GetOptions before answering: an open dialog set for folders says so
       only here. */
    h->orig->GetOptions(d, &opts);

    EnterCriticalSection(&g_lock);
    n = ask(h->is_save ? "save" : "open", (opts & FOS_ALLOWMULTISELECT) ? TRUE : FALSE,
            (opts & FOS_PICKFOLDERS) ? TRUE : FALSE, g_picks);
    if (n > 0) {
        size_t size = sizeof(g_picks[0]) * (size_t)n;
        h->paths = (wchar_t(*)[MAX_PATH_LEN])HeapAlloc(GetProcessHeap(), 0, size);
        if (h->paths) {
            copy_bytes(h->paths, g_picks, size);
            h->npaths = n;
        } else {
            n = -1;
        }
    }
    LeaveCriticalSection(&g_lock);

    if (n < 0) {
        /* No agent, or no answer: the app gets its own dialog. */
        return h->orig->Show(d, owner);
    }
    if (n == 0) {
        /* Cancelled, and reported exactly as the shell reports a cancel. */
        return HRESULT_FROM_WIN32(ERROR_CANCELLED);
    }
    /* S_OK without having shown anything: the shell takes that as the
       dialog closed, and the app asks for its result as usual. */
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE hook_GetResult(IFileDialog *d, IShellItem **out)
{
    DialogHook *h = hook_of(d);
    if (h->npaths < 1) return h->orig->GetResult(d, out);

    PIDLIST_ABSOLUTE id = id_list_of(h->paths[0]);
    if (!id) return E_FAIL;
    HRESULT hr = SHCreateItemFromIDList(id, &IID_IShellItem, (void **)out);
    CoTaskMemFree(id);
    return hr;
}

static HRESULT STDMETHODCALLTYPE hook_GetResults(IFileOpenDialog *d, IShellItemArray **out)
{
    DialogHook *h = hook_of((IFileDialog *)d);
    if (h->npaths < 1)
        return ((const IFileOpenDialogVtbl *)h->orig)->GetResults(d, out);

    /* PIDLIST_ABSOLUTE, not ITEMIDLIST *: MSVC marks it __unaligned. */
    PIDLIST_ABSOLUTE *ids =
        (PIDLIST_ABSOLUTE *)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY,
                                      sizeof(PIDLIST_ABSOLUTE) * h->npaths);
    if (!ids) return E_OUTOFMEMORY;
    HRESULT hr = E_FAIL;
    int made = 0;
    for (int i = 0; i < h->npaths; i++) {
        ids[made] = id_list_of(h->paths[i]);
        if (ids[made]) made++;
    }
    if (made > 0)
        hr = SHCreateShellItemArrayFromIDLists((UINT)made, (PCIDLIST_ABSOLUTE_ARRAY)ids, out);
    for (int i = 0; i < made; i++) CoTaskMemFree(ids[i]);
    HeapFree(GetProcessHeap(), 0, ids);
    return hr;
}

/* Takes over a dialog the shell just made, through the interface pointer
   that carries its full table: copies the table, points the methods that
   matter at ours, and points the object at the copy. */
static void patch_dialog(IFileDialog *d, int is_save)
{
    if (d->lpVtbl->Show == hook_Show) return; /* already ours */
    DialogHook *h =
        (DialogHook *)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(DialogHook));
    if (!h) return;

    h->orig = d->lpVtbl;
    h->is_save = is_save;
    copy_bytes(&h->vtbl, d->lpVtbl,
               is_save ? sizeof(IFileSaveDialogVtbl) : sizeof(IFileOpenDialogVtbl));
    h->vtbl.base.Release = hook_Release;
    h->vtbl.base.Show = hook_Show;
    h->vtbl.base.GetResult = hook_GetResult;
    if (!is_save) h->vtbl.open.GetResults = hook_GetResults;
    d->lpVtbl = &h->vtbl.base;
}

/* ------------------------------------------------ the older functions */

/* Fills the caller's buffer for GetOpenFileNameW and GetSaveFileNameW the
   way the shell does: one full path, or, for an Explorer-style multiple
   selection, the folder and then each name, each null-terminated and the
   whole ending on an extra null. Also the offsets apps read the name and
   extension through. FALSE for anything it cannot produce exactly, which
   shows the real dialog. */
static BOOL fill_legacy(OPENFILENAMEW *ofn, wchar_t picks[][MAX_PATH_LEN], int n)
{
    wchar_t *buf = ofn->lpstrFile;
    DWORD cap = ofn->nMaxFile;
    if (!buf || cap == 0) return FALSE;

    if (n == 1) {
        const wchar_t *path = picks[0];
        size_t len = wlen(path);
        size_t name = name_offset(path);
        if (len + 1 > cap || len > 0xffff) return FALSE;

        /* Past the last dot in the name; the terminating null if there is
           none; zero if the name ends in one. */
        size_t ext = len;
        for (size_t i = len; i > name; i--) {
            if (path[i - 1] == L'.') {
                ext = i == len ? 0 : i;
                break;
            }
        }
        copy_bytes(buf, path, (len + 1) * sizeof(wchar_t));
        ofn->nFileOffset = (WORD)name;
        ofn->nFileExtension = (WORD)ext;
        if (ofn->lpstrFileTitle && ofn->nMaxFileTitle > len - name)
            copy_bytes(ofn->lpstrFileTitle, path + name, (len - name + 1) * sizeof(wchar_t));
        return TRUE;
    }

    /* Several only where several were asked for, and only in the Explorer
       format: the old one separates names with spaces. */
    if (!(ofn->Flags & OFN_ALLOWMULTISELECT) || !(ofn->Flags & OFN_EXPLORER)) return FALSE;

    /* One folder holds them all, or the format cannot say where each is. */
    const wchar_t *first = picks[0];
    size_t name = name_offset(first);
    if (!name) return FALSE;
    for (int i = 1; i < n; i++) {
        if (name_offset(picks[i]) != name) return FALSE;
        for (size_t k = 0; k < name; k++)
            if (picks[i][k] != first[k]) return FALSE;
    }

    /* The folder without its trailing separator, except a drive's root,
       which the shell writes as "C:\". */
    size_t dir_len = name - 1;
    if (dir_len == 2 && first[1] == L':') dir_len = 3;

    size_t need = dir_len + 1;
    for (int i = 0; i < n; i++) need += wlen(picks[i] + name) + 1;
    if (need + 1 > cap || dir_len + 1 > 0xffff) return FALSE;

    wchar_t *p = buf;
    copy_bytes(p, first, dir_len * sizeof(wchar_t));
    p += dir_len;
    *p++ = 0;
    for (int i = 0; i < n; i++) {
        size_t len = wlen(picks[i] + name) + 1;
        copy_bytes(p, picks[i] + name, len * sizeof(wchar_t));
        p += len;
    }
    *p = 0; /* the extra null the multi-select format ends on */
    ofn->nFileOffset = (WORD)(dir_len + 1);
    ofn->nFileExtension = 0;
    return TRUE;
}

static BOOL legacy_dialog(OPENFILENAMEW *ofn, BOOL save)
{
    int n = -1;
    BOOL filled = FALSE;
    if (ofn) {
        EnterCriticalSection(&g_lock);
        n = ask(save ? "save" : "open",
                (!save && (ofn->Flags & OFN_ALLOWMULTISELECT)) ? TRUE : FALSE, FALSE, g_picks);
        if (n > 0) filled = fill_legacy(ofn, g_picks, n);
        LeaveCriticalSection(&g_lock);
    }
    if (n == 0) return FALSE; /* cancelled */
    if (!filled) return save ? GetSaveFileNameW(ofn) : GetOpenFileNameW(ofn);
    ofn->nFilterIndex = 1;
    return TRUE;
}

static BOOL WINAPI hook_GetOpenFileNameW(OPENFILENAMEW *ofn)
{
    return legacy_dialog(ofn, FALSE);
}

static BOOL WINAPI hook_GetSaveFileNameW(OPENFILENAMEW *ofn)
{
    return legacy_dialog(ofn, TRUE);
}

/* ------------------------------------------------------------- CoCreate */

/* CoCreateInstance here is the hook's own import, which nothing patches, so
   it is always the real one. */
static HRESULT WINAPI hook_CoCreateInstance(REFCLSID rclsid, LPUNKNOWN pUnk, DWORD dwClsContext,
                                            REFIID riid, LPVOID ppv)
{
    HRESULT hr = CoCreateInstance(rclsid, pUnk, dwClsContext, riid, (void **)ppv);
    if (FAILED(hr) || !ppv || !*(void **)ppv) return hr;

    int is_save;
    if (same_guid(rclsid, &CLSID_FileOpenDialog))
        is_save = 0;
    else if (same_guid(rclsid, &CLSID_FileSaveDialog))
        is_save = 1;
    else
        return hr; /* some other COM object entirely */

    /* The pointer the app was handed is the one it calls Show through, so
       that is the vtable to patch. The dialog interfaces are one inheritance
       chain over a single vtable, so this pointer carries IFileDialog's
       methods whichever of them `riid` named; the CLSID, not the pointer,
       says how long the table is. Patching a pointer fetched through a fresh
       QueryInterface would miss, since COM may hand that back as a different
       one. */
    dbg("cocreate matched save=", is_save);
    patch_dialog((IFileDialog *)*(void **)ppv, is_save);
    return hr;
}

/* ---------------------------------------------------- import patching */

/* Writes one import table slot. The loader makes the table read-only once
   it has filled it, so it is made writable for the write and put back. */
static void write_slot(void **slot, void *value)
{
    MEMORY_BASIC_INFORMATION mbi;
    if (!VirtualQuery(slot, &mbi, sizeof(mbi))) return;
    /* Keep execute where the page had it: other code may share the page. */
    DWORD exec = PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    DWORD writable = (mbi.Protect & exec) ? PAGE_EXECUTE_READWRITE : PAGE_READWRITE;
    DWORD old;
    if (!VirtualProtect(slot, sizeof(*slot), writable, &old)) return;
    InterlockedExchangePointer(slot, value);
    VirtualProtect(slot, sizeof(*slot), old, &old);
}

/* Replaces one entry in a module's import table. The shell's dialogs are
   reached through imports, so this is where a call the app makes directly
   can be caught. Delay-loaded imports are not covered: an app that reaches
   for a dialog through one gets its own dialog, which is the fail-open
   outcome everywhere else here. */
static int patch_imports(HMODULE mod, const char *module, const char *name, void *replacement)
{
    if (!mod) return 0;
    PIMAGE_DOS_HEADER dos = (PIMAGE_DOS_HEADER)mod;
    if (dos->e_magic != IMAGE_DOS_SIGNATURE) return 0;
    PIMAGE_NT_HEADERS nt = (PIMAGE_NT_HEADERS)((BYTE *)mod + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE) return 0;

    IMAGE_DATA_DIRECTORY dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!dir.VirtualAddress) return 0;
    PIMAGE_IMPORT_DESCRIPTOR imports =
        (PIMAGE_IMPORT_DESCRIPTOR)((BYTE *)mod + dir.VirtualAddress);

    for (PIMAGE_IMPORT_DESCRIPTOR d = imports; d->Name; d++) {
        const char *dll_name = (const char *)((BYTE *)mod + d->Name);
        if (!same_str_nocase(dll_name, module)) continue;
        if (!d->FirstThunk) continue;

        /* Names come from the lookup table (the original first thunk); the
           loader overwrites the first thunk with addresses, so it is no use
           for names. Where there is no lookup table, the first thunk still
           held names until the loader bound it, so it is the one source
           left: read names there and patch there too. */
        DWORD names_rva = d->OriginalFirstThunk ? d->OriginalFirstThunk : d->FirstThunk;
        PIMAGE_THUNK_DATA lookup = (PIMAGE_THUNK_DATA)((BYTE *)mod + names_rva);
        PIMAGE_THUNK_DATA patch = (PIMAGE_THUNK_DATA)((BYTE *)mod + d->FirstThunk);
        for (; lookup->u1.AddressOfData; lookup++, patch++) {
            /* Ordinals have no name to match against. */
            if (IMAGE_SNAP_BY_ORDINAL(lookup->u1.Ordinal)) continue;
            PIMAGE_IMPORT_BY_NAME n =
                (PIMAGE_IMPORT_BY_NAME)((BYTE *)mod + lookup->u1.AddressOfData);
            if (!same_str((const char *)n->Name, name)) continue;
            write_slot((void **)&patch->u1.Function, replacement);
            return 1;
        }
    }
    return 0;
}

/* ------------------------------------------------------------- entry */

/* The DLLs an exe can import CoCreateInstance from: ole32 classically,
   combase or the API set on newer SDKs. */
static const char *const com_modules[] = {
    "ole32.dll",
    "combase.dll",
    "api-ms-win-core-com-l1-1-0.dll",
    "api-ms-win-core-com-l1-1-1.dll",
};

/* Runs under the loader lock, so it only reads the environment and patches
   memory: no other library is loaded and nothing is waited on. The pipe is
   connected on the first dialog. */
BOOL WINAPI DllMain(HINSTANCE self, DWORD reason, LPVOID reserved)
{
    (void)self;
    (void)reserved;
    if (reason != DLL_PROCESS_ATTACH) return TRUE;

    /* Off before anything the app starts can inherit it. */
    DWORD cap = (DWORD)(sizeof(g_pipe_name) / sizeof(g_pipe_name[0]));
    DWORD got = GetEnvironmentVariableW(L"OCU_PANEL_PIPE", g_pipe_name, cap);
    SetEnvironmentVariableW(L"OCU_PANEL_PIPE", NULL);
    if (!got || got >= cap) return TRUE; /* not ours to do anything with */

    InitializeCriticalSection(&g_lock);
    dbg("dllmain pipe-name-len", (long)got);

    HMODULE exe = GetModuleHandleW(NULL);
    int co = 0;
    for (size_t i = 0; i < sizeof(com_modules) / sizeof(com_modules[0]); i++)
        co += patch_imports(exe, com_modules[i], "CoCreateInstance", (void *)hook_CoCreateInstance);
    int open = patch_imports(exe, "comdlg32.dll", "GetOpenFileNameW", (void *)hook_GetOpenFileNameW);
    int save = patch_imports(exe, "comdlg32.dll", "GetSaveFileNameW", (void *)hook_GetSaveFileNameW);
    dbg("dllmain patched cocreate=", co);
    dbg("dllmain patched legacy-open=", open);
    dbg("dllmain patched legacy-save=", save);
    return TRUE;
}
