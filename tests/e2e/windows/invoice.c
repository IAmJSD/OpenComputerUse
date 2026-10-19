/*
 * A test app for tests/extract.rs: a window showing an invoice, whose
 * values the test asks Clairic for.
 *
 * Usage: invoice.exe
 */
#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif
#include <windows.h>

static const wchar_t *LINES[] = {
    L"Invoice number: INV-2291",
    L"Northwind Supply",
    L"Issued: 2026-10-02",
    L"Due: 2026-11-01",
    L"4 pallets at $250.00 each",
    L"Shipping: $80.00",
    L"Total due: $1,080.00",
};

static LRESULT CALLBACK proc(HWND w, UINT msg, WPARAM wp, LPARAM lp)
{
    if (msg == WM_DESTROY) {
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
    wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    wc.lpszClassName = L"OcuInvoiceTest";
    RegisterClassW(&wc);
    HWND w = CreateWindowW(L"OcuInvoiceTest", L"Invoice", WS_OVERLAPPEDWINDOW,
                           CW_USEDEFAULT, CW_USEDEFAULT, 440, 360, NULL, NULL, inst, NULL);
    HFONT font = (HFONT)GetStockObject(DEFAULT_GUI_FONT);
    int y = 16;
    for (int i = 0; i < (int)(sizeof(LINES) / sizeof(LINES[0])); i++, y += 26) {
        HWND label = CreateWindowW(L"STATIC", LINES[i], WS_CHILD | WS_VISIBLE, 16, y, 380, 22, w,
                                   NULL, inst, NULL);
        SendMessageW(label, WM_SETFONT, (WPARAM)font, TRUE);
    }
    y += 8;
    HWND save = CreateWindowW(L"BUTTON", L"Save draft", WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON, 16,
                              y, 110, 28, w, (HMENU)(INT_PTR)1, inst, NULL);
    HWND send = CreateWindowW(L"BUTTON", L"Send invoice", WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
                              136, y, 110, 28, w, (HMENU)(INT_PTR)2, inst, NULL);
    SendMessageW(save, WM_SETFONT, (WPARAM)font, TRUE);
    SendMessageW(send, WM_SETFONT, (WPARAM)font, TRUE);
    ShowWindow(w, show);
    UpdateWindow(w);
    MSG m;
    while (GetMessageW(&m, NULL, 0, 0) > 0) {
        TranslateMessage(&m);
        DispatchMessageW(&m);
    }
    return 0;
}
