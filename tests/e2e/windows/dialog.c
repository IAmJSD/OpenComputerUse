/*
 * A test app for tests/file_dialogs.rs: opens a window, then a moment later
 * a file dialog, and writes what came back to a file. Its dialog calls are
 * its own imports, which is what the panel hook patches.
 *
 * Usage: dialog.exe <open|save|legacy-open> <result file>
 */
#define COBJMACROS
#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif
#include <windows.h>
#include <shobjidl.h>
#include <commdlg.h>

static const wchar_t *g_mode = L"open";
static const wchar_t *g_out = L"NUL";

static void write_result(const wchar_t *text)
{
    char buf[4 * MAX_PATH];
    int n = WideCharToMultiByte(CP_UTF8, 0, text, -1, buf, sizeof(buf), NULL, NULL);
    HANDLE f = CreateFileW(g_out, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) return;
    DWORD written;
    WriteFile(f, buf, n > 0 ? (DWORD)(n - 1) : 0, &written, NULL);
    CloseHandle(f);
}

/* IFileOpenDialog / IFileSaveDialog, through CoCreateInstance. */
static void modern(HWND owner, BOOL save)
{
    IFileDialog *d = NULL;
    HRESULT hr = CoCreateInstance(save ? &CLSID_FileSaveDialog : &CLSID_FileOpenDialog, NULL,
                                  CLSCTX_INPROC_SERVER, &IID_IFileDialog, (void **)&d);
    if (FAILED(hr)) {
        write_result(L"ERROR");
        return;
    }
    if (save) IFileDialog_SetFileName(d, L"untitled.txt");
    hr = IFileDialog_Show(d, owner);
    if (hr == HRESULT_FROM_WIN32(ERROR_CANCELLED)) {
        write_result(L"CANCELLED");
    } else if (SUCCEEDED(hr)) {
        IShellItem *item = NULL;
        PWSTR path = NULL;
        if (SUCCEEDED(IFileDialog_GetResult(d, &item)) &&
            SUCCEEDED(IShellItem_GetDisplayName(item, SIGDN_FILESYSPATH, &path))) {
            write_result(path);
            CoTaskMemFree(path);
        } else {
            write_result(L"ERROR");
        }
        if (item) IShellItem_Release(item);
    } else {
        write_result(L"ERROR");
    }
    IFileDialog_Release(d);
}

/* GetOpenFileNameW, the older dialog. */
static void legacy(HWND owner)
{
    wchar_t file[MAX_PATH] = L"";
    OPENFILENAMEW o = {0};
    o.lStructSize = sizeof(o);
    o.hwndOwner = owner;
    o.lpstrFile = file;
    o.nMaxFile = MAX_PATH;
    o.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST;
    if (GetOpenFileNameW(&o))
        write_result(file);
    else
        write_result(CommDlgExtendedError() ? L"ERROR" : L"CANCELLED");
}

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg) {
    case WM_TIMER:
        KillTimer(hwnd, 1);
        if (lstrcmpW(g_mode, L"legacy-open") == 0)
            legacy(hwnd);
        else
            modern(hwnd, lstrcmpW(g_mode, L"save") == 0);
        DestroyWindow(hwnd);
        return 0;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(hwnd, msg, wp, lp);
}

int WINAPI wWinMain(HINSTANCE inst, HINSTANCE prev, PWSTR cmd, int show)
{
    (void)prev;
    (void)cmd;
    int argc = 0;
    wchar_t **argv = CommandLineToArgvW(GetCommandLineW(), &argc);
    if (argv && argc > 2) {
        g_mode = argv[1];
        g_out = argv[2];
    }
    CoInitializeEx(NULL, COINIT_APARTMENTTHREADED);

    WNDCLASSW wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = inst;
    wc.lpszClassName = L"OcuDialogTest";
    wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    RegisterClassW(&wc);
    HWND hwnd = CreateWindowW(L"OcuDialogTest", L"Dialog test", WS_OVERLAPPEDWINDOW, 200, 200,
                              360, 220, NULL, NULL, inst, NULL);
    ShowWindow(hwnd, show);
    SetTimer(hwnd, 1, 1000, NULL);

    MSG m;
    while (GetMessageW(&m, NULL, 0, 0) > 0) {
        TranslateMessage(&m);
        DispatchMessageW(&m);
    }
    CoUninitialize();
    return 0;
}
