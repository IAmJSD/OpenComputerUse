/*
 * A test app for tests/live_view.rs: a window filled with red, which turns
 * blue when clicked and red again when clicked again.
 *
 * Usage: swatch.exe
 */
#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif
#include <windows.h>

static BOOL blue = FALSE;

static LRESULT CALLBACK proc(HWND w, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg) {
    case WM_LBUTTONDOWN:
        blue = !blue;
        InvalidateRect(w, NULL, FALSE);
        return 0;
    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(w, &ps);
        HBRUSH brush = CreateSolidBrush(blue ? RGB(0, 0, 255) : RGB(255, 0, 0));
        RECT r;
        GetClientRect(w, &r);
        FillRect(dc, &r, brush);
        DeleteObject(brush);
        EndPaint(w, &ps);
        return 0;
    }
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(w, msg, wp, lp);
}

int WINAPI wWinMain(HINSTANCE inst, HINSTANCE prev, PWSTR cmd, int show)
{
    (void)prev;
    (void)cmd;
    WNDCLASSW wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = inst;
    wc.hCursor = LoadCursor(NULL, IDC_ARROW);
    wc.lpszClassName = L"OcuSwatchTest";
    RegisterClassW(&wc);
    HWND w = CreateWindowW(L"OcuSwatchTest", L"Swatch", WS_OVERLAPPEDWINDOW, CW_USEDEFAULT,
                           CW_USEDEFAULT, 360, 280, NULL, NULL, inst, NULL);
    ShowWindow(w, show);
    UpdateWindow(w);
    MSG m;
    while (GetMessageW(&m, NULL, 0, 0) > 0) {
        TranslateMessage(&m);
        DispatchMessageW(&m);
    }
    return 0;
}
