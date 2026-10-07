//! The AppKit calls GPUI has no words for: making the overlay a click-
//! through sheet of glass that sits exactly over a window of another app,
//! and switching the app between agent (no Dock icon) and regular mode.

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSScreen, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowOrderingMode,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use ocu_core::Rect;

pub fn ns_window(window: &gpui::Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return None };
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

/// Turns a fresh window into the overlay: no shadow, no mouse, at the normal
/// level so the windows in front of its target stay in front of it too.
/// Session screenshots capture the target window alone, so the overlay
/// never shows up in the pictures the model sees.
pub fn make_overlay(window: &NSWindow) {
    window.setIgnoresMouseEvents(true);
    window.setHasShadow(false);
    window.setOpaque(false);
    window.setLevel(0);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
}

/// The height of the screen whose bottom-left is AppKit's origin; window
/// server frames measure from its top-left.
fn primary_height(mtm: MainThreadMarker) -> f64 {
    NSScreen::screens(mtm).firstObject().map(|s| s.frame().size.height).unwrap_or(0.0)
}

/// Places the overlay over `target` (a window-server frame), `margin`
/// points bigger on every side for the halo, just above `target_id`.
pub fn cover(window: &NSWindow, target: Rect, target_id: u64, margin: f64) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let h = primary_height(mtm);
    let frame = NSRect::new(
        NSPoint::new(target.x - margin, h - (target.y + target.height) - margin),
        NSSize::new(target.width + margin * 2.0, target.height + margin * 2.0),
    );
    if window.frame() != frame {
        window.setFrame_display(frame, true);
    }
    window.orderWindow_relativeTo(NSWindowOrderingMode::Above, target_id as isize);
}

pub fn hide(window: &NSWindow) {
    window.orderOut(None);
}

/// Regular while the status window is open (a Dock icon, ⌘-Tab), an agent
/// otherwise.
pub fn set_regular(regular: bool) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(if regular {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    });
}
