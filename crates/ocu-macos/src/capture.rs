//! Window lists from the window server, and window pictures from
//! ScreenCaptureKit, which reads a window's own backing store so covered
//! and background windows capture exactly as they look.

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AllocAnyThread as _;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGImageAlphaInfo, CGWindowListCopyWindowInfo, CGWindowListOption,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};

use ocu_core::image::{encode_png, Order};
use ocu_core::{Rect, Screenshot, WindowInfo};

fn get<'a>(dict: &'a CFDictionary, key: &str) -> Option<&'a CFType> {
    let key = CFString::from_str(key);
    let v = unsafe { dict.value((&*key as *const CFString).cast::<c_void>()) };
    (!v.is_null()).then(|| unsafe { &*(v as *const CFType) })
}

fn num(dict: &CFDictionary, key: &str) -> Option<f64> {
    let n = get(dict, key)?.downcast_ref::<CFNumber>()?;
    n.as_f64().or_else(|| n.as_i64().map(|i| i as f64))
}

/// One window's current frame and visibility, whoever owns it.
pub fn window(id: u64) -> Option<WindowInfo> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, id as u32)?;
    let list: &CFArray<CFDictionary> = unsafe { list.cast_unchecked() };
    list.iter().find_map(|d| parse(&d)).filter(|w| w.id == id)
}

/// The app's normal-layer windows, front to back, on-screen ones first.
pub fn windows(pid: i32) -> Vec<WindowInfo> {
    let opts = CGWindowListOption(
        CGWindowListOption::OptionAll.0 | CGWindowListOption::ExcludeDesktopElements.0,
    );
    let Some(list) = CGWindowListCopyWindowInfo(opts, 0) else {
        return Vec::new();
    };
    let list: &CFArray<CFDictionary> = unsafe { list.cast_unchecked() };
    let mut out = Vec::new();
    for dict in list.iter() {
        if num(&dict, "kCGWindowOwnerPID") != Some(pid as f64)
            || num(&dict, "kCGWindowLayer") != Some(0.0)
        {
            continue;
        }
        if num(&dict, "kCGWindowAlpha").unwrap_or(1.0) <= 0.0 {
            continue;
        }
        // Zero-size and sliver windows are helpers, never content.
        match parse(&dict) {
            Some(w) if w.frame.width >= 20.0 && w.frame.height >= 20.0 => out.push(w),
            _ => {}
        }
    }
    out.sort_by_key(|w| !w.on_screen);
    out
}

/// The topmost normal window on screen whose owner is not in `skip`, and
/// its owner.
pub fn front_window(skip: &[i32]) -> Option<(i32, WindowInfo)> {
    let opts = CGWindowListOption(
        CGWindowListOption::OptionOnScreenOnly.0 | CGWindowListOption::ExcludeDesktopElements.0,
    );
    let list = CGWindowListCopyWindowInfo(opts, 0)?;
    let list: &CFArray<CFDictionary> = unsafe { list.cast_unchecked() };
    // Front to back, so the first that qualifies is the one in front.
    list.iter().find_map(|dict| {
        let pid = num(&dict, "kCGWindowOwnerPID")? as i32;
        if skip.contains(&pid)
            || num(&dict, "kCGWindowLayer") != Some(0.0)
            || num(&dict, "kCGWindowAlpha").unwrap_or(1.0) <= 0.0
        {
            return None;
        }
        let w = parse(&dict)?;
        (w.frame.width >= 100.0 && w.frame.height >= 60.0).then_some((pid, w))
    })
}

fn parse(dict: &CFDictionary) -> Option<WindowInfo> {
    let bounds = get(dict, "kCGWindowBounds")?.downcast_ref::<CFDictionary>()?;
    let frame = Rect {
        x: num(bounds, "X").unwrap_or(0.0),
        y: num(bounds, "Y").unwrap_or(0.0),
        width: num(bounds, "Width").unwrap_or(0.0),
        height: num(bounds, "Height").unwrap_or(0.0),
    };
    let on_screen = get(dict, "kCGWindowIsOnscreen")
        .and_then(|b| b.downcast_ref::<CFBoolean>())
        .map(|b| b.as_bool())
        .unwrap_or(false);
    Some(WindowInfo {
        id: num(dict, "kCGWindowNumber")? as u64,
        title: get(dict, "kCGWindowName")
            .and_then(|s| s.downcast_ref::<CFString>())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        frame,
        on_screen,
    })
}

/// Retained ObjC objects handed from a completion queue to the caller.
struct Sendable<T>(T);
unsafe impl<T> Send for Sendable<T> {}

pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = match unsafe { Retained::retain(content) } {
                Some(c) => Ok(Sendable(c)),
                None => Err(unsafe { error.as_ref() }
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "no content".into())),
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, false, &block,
        )
    };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => bail!("ScreenCaptureKit refused to list windows: {e} (is Screen Recording permission granted?)"),
        Err(_) => bail!("ScreenCaptureKit did not answer"),
    }
}

/// Captures one window at its size in points, so screenshot pixels and
/// action coordinates are the same grid.
pub fn capture(window: &WindowInfo) -> Result<Screenshot> {
    let content = shareable_content()?;
    let windows = unsafe { content.windows() };
    let sc_window: Retained<SCWindow> = windows
        .iter()
        .find(|w| unsafe { w.windowID() } as u64 == window.id)
        .ok_or_else(|| anyhow!("ScreenCaptureKit cannot see window {}", window.id))?;
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &sc_window)
    };
    let config = unsafe { SCStreamConfiguration::new() };
    let (w, h) = (
        window.frame.width.round().max(1.0) as usize,
        window.frame.height.round().max(1.0) as usize,
    );
    unsafe {
        config.setWidth(w);
        config.setHeight(h);
        config.setShowsCursor(false);
        config.setIgnoreShadowsSingleWindow(true);
    }

    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = if image.is_null() {
            Err(unsafe { error.as_ref() }
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "no image".into()))
        } else {
            // Retain across the queue hop; the handler's reference is borrowed.
            Ok(Sendable(unsafe {
                CFRetained::retain(std::ptr::NonNull::new_unchecked(image))
            }))
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&block),
        )
    };
    let image = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(i)) => i.0,
        Ok(Err(e)) => bail!("capturing window {} failed: {e}", window.id),
        Err(_) => bail!("ScreenCaptureKit did not return a picture"),
    };
    let png = image_png(&image).context("encoding the capture")?;
    Ok(Screenshot {
        window_id: Some(window.id),
        width: CGImage::width(Some(&image)) as u32,
        height: CGImage::height(Some(&image)) as u32,
        png,
    })
}

fn image_png(image: &CGImage) -> Result<Vec<u8>> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    let stride = CGImage::bytes_per_row(Some(image));
    anyhow::ensure!(
        CGImage::bits_per_pixel(Some(image)) == 32,
        "unexpected pixel size"
    );
    let provider =
        CGImage::data_provider(Some(image)).ok_or_else(|| anyhow!("image has no data"))?;
    let data =
        CGDataProvider::data(Some(&provider)).ok_or_else(|| anyhow!("image data unreadable"))?;
    let bytes = data.to_vec();
    let info = CGImage::bitmap_info(Some(image));
    // kCGBitmapByteOrderMask and kCGBitmapByteOrder32Little.
    let little = info.0 & 0x7000 == 0x2000;
    let alpha = CGImage::alpha_info(Some(image));
    let alpha_first = [
        CGImageAlphaInfo::PremultipliedFirst,
        CGImageAlphaInfo::First,
        CGImageAlphaInfo::NoneSkipFirst,
    ]
    .contains(&alpha);
    let order = match (little, alpha_first) {
        // ARGB stored little-endian is BGRA in memory.
        (true, true) => Order::Bgra,
        (false, false) => Order::Rgba,
        _ => bail!("unsupported pixel layout (bitmap info {:#x})", info.0),
    };
    encode_png(width as u32, height as u32, stride, &bytes, order)
}
