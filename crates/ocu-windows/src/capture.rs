//! Window lists and window pictures. `PrintWindow` with full-content
//! rendering asks the window to draw itself into our bitmap, so covered and
//! background windows capture as they look (minimised ones do not draw).

use anyhow::{bail, Result};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, GW_OWNER,
};
use windows::core::BOOL;

use ocu_core::image::{encode_png, Order};
use ocu_core::{Rect, Screenshot, WindowInfo};

pub fn frame(hwnd: HWND) -> Rect {
    let mut r = RECT::default();
    // The visible frame, without the invisible resize borders.
    let ok = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32)
    }
    .is_ok();
    if !ok {
        let _ = unsafe { GetWindowRect(hwnd, &mut r) };
    }
    Rect { x: r.left as f64, y: r.top as f64, width: (r.right - r.left) as f64, height: (r.bottom - r.top) as f64 }
}

fn title(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n as usize])
}

fn cloaked(hwnd: HWND) -> bool {
    let mut c = 0u32;
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut c as *mut _ as *mut _, 4) }.is_ok() && c != 0
}

/// Visible top-level windows of `pids`, in z-order (topmost first), with
/// unowned (main) windows before owned ones (dialogs, tool windows).
pub fn windows(pids: &[u32]) -> Vec<WindowInfo> {
    struct Acc<'a> {
        pids: &'a [u32],
        out: Vec<(WindowInfo, bool)>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let acc = &mut *(lparam.0 as *mut Acc);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if acc.pids.contains(&pid) && IsWindowVisible(hwnd).as_bool() && !cloaked(hwnd) {
            let f = frame(hwnd);
            if f.width >= 20.0 && f.height >= 20.0 {
                let owned = GetWindow(hwnd, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false);
                let on_screen = !IsIconic(hwnd).as_bool();
                acc.out.push((WindowInfo { id: hwnd.0 as usize as u64, title: title(hwnd), frame: f, on_screen }, owned));
            }
        }
        BOOL(1)
    }
    let mut acc = Acc { pids, out: Vec::new() };
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut acc as *mut _ as isize)) };
    acc.out.sort_by_key(|(w, owned)| (*owned, !w.on_screen));
    acc.out.into_iter().map(|(w, _)| w).collect()
}

pub fn hwnd(id: u64) -> HWND {
    HWND(id as usize as *mut _)
}

pub fn capture(window: &WindowInfo) -> Result<Screenshot> {
    let h = hwnd(window.id);
    if unsafe { IsIconic(h) }.as_bool() {
        bail!("the window is minimised and has nothing to show");
    }
    // PrintWindow draws the whole window rect, borders included; crop to
    // the visible frame afterwards.
    let mut wr = RECT::default();
    unsafe { GetWindowRect(h, &mut wr)? };
    let (w, ht) = (wr.right - wr.left, wr.bottom - wr.top);
    if w <= 0 || ht <= 0 {
        bail!("the window has no size");
    }
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -ht, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old = SelectObject(dc, bmp.into());
        // PW_RENDERFULLCONTENT: include DirectComposition content.
        let ok = PrintWindow(h, dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        let data = std::slice::from_raw_parts(bits as *const u8, (w * ht * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        if !ok {
            bail!("the window would not draw itself");
        }
        let f = window.frame;
        let (ox, oy) = ((f.x as i32 - wr.left).max(0) as usize, (f.y as i32 - wr.top).max(0) as usize);
        let (cw, ch) = ((f.width as usize).min(w as usize - ox), (f.height as usize).min(ht as usize - oy));
        let stride = w as usize * 4;
        let cropped = &data[oy * stride + ox * 4..];
        let png = encode_png(cw as u32, ch as u32, stride, cropped, Order::Bgra)?;
        Ok(Screenshot { window_id: Some(window.id), width: cw as u32, height: ch as u32, png })
    }
}
