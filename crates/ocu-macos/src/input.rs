//! Mouse and keyboard events delivered straight to one process with
//! `CGEventPostToPid`: the app handles them while the user's cursor,
//! keyboard focus and frontmost app stay where they are.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, Result};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventType, CGMouseButton, CGScrollEventUnit,
};

use ocu_core::keys::{Chord, Key, Modifiers, NamedKey};
use ocu_core::MouseButton;

use crate::sky;

pub struct Target {
    pub pid: i32,
    pub window_id: u32,
    /// The window's top-left in screen points.
    pub origin: CGPoint,
}

impl Target {
    /// Readies the app to take input in its window: key window, and active
    /// as far as the app can tell, while staying behind the user's apps.
    /// Focus goes back to `previous` (the user's app and its key window)
    /// when the lease drops.
    pub fn prepare(&self, previous: Option<(i32, u32)>) -> Option<sky::FocusLease> {
        sky::focus_without_raise(self.pid, self.window_id, previous)
    }
}

fn post(target: &Target, event: &CGEvent) {
    sky::set_field(event, 40, target.pid as i64);
    sky::post(target.pid, event);
}

/// One gesture id shared by every event of a click, so double clicks
/// coalesce.
fn gesture_id() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(1)
}

fn flags(m: Modifiers) -> CGEventFlags {
    let mut f = 0;
    if m.shift {
        f |= CGEventFlags::MaskShift.0;
    }
    if m.ctrl {
        f |= CGEventFlags::MaskControl.0;
    }
    if m.alt {
        f |= CGEventFlags::MaskAlternate.0;
    }
    if m.meta {
        f |= CGEventFlags::MaskCommand.0;
    }
    CGEventFlags(f)
}

fn source() -> Option<CFRetained<CGEventSource>> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
}

fn mouse_event(target: &Target, ty: CGEventType, at: CGPoint, button: CGMouseButton) -> Result<CFRetained<CGEvent>> {
    mouse_event_in(target, ty, at, button, 0, 0)
}

/// A mouse event addressed the way the window server addresses real ones:
/// window-local location, window number, and the routing fields that pick
/// the window under the pointer. Without them a background app drops the
/// event or hands it to whichever window it thinks is key.
fn mouse_event_in(
    target: &Target,
    ty: CGEventType,
    at: CGPoint,
    button: CGMouseButton,
    click_state: i64,
    gesture: i64,
) -> Result<CFRetained<CGEvent>> {
    let e = CGEvent::new_mouse_event(source().as_deref(), ty, at, button)
        .ok_or_else(|| anyhow!("could not make a mouse event"))?;
    sky::set_window_location(&e, CGPoint { x: at.x - target.origin.x, y: at.y - target.origin.y });
    let wid = target.window_id as i64;
    sky::set_field(&e, 1, click_state); // click state
    sky::set_field(&e, 3, button.0 as i64); // button number
    sky::set_field(&e, 7, 3); // subtype: as from a trackpad tap
    sky::set_field(&e, 51, wid); // window number
    if gesture != 0 {
        sky::set_field(&e, 58, gesture);
    }
    sky::set_field(&e, CGEventField::MouseEventWindowUnderMousePointer.0, wid);
    sky::set_field(&e, CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent.0, wid);
    Ok(e)
}

fn button_types(button: MouseButton) -> (CGEventType, CGEventType, CGEventType, CGMouseButton) {
    match button {
        MouseButton::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGEventType::LeftMouseDragged, CGMouseButton::Left),
        MouseButton::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGEventType::RightMouseDragged, CGMouseButton::Right),
        MouseButton::Middle => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGEventType::OtherMouseDragged, CGMouseButton::Center),
    }
}

pub fn move_to(target: &Target, at: CGPoint) -> Result<()> {
    let e = mouse_event(target, CGEventType::MouseMoved, at, CGMouseButton::Left)?;
    post(target, &e);
    Ok(())
}

pub fn click(target: &Target, at: CGPoint, button: MouseButton, count: u32, modifiers: Modifiers) -> Result<()> {
    let (down, up, _, b) = button_types(button);
    let gesture = gesture_id();
    // A move first: AppKit hit-tests a click against where it last saw the
    // pointer, and a background window has seen nothing.
    post(target, &*mouse_event_in(target, CGEventType::MouseMoved, at, CGMouseButton::Left, 0, gesture)?);
    sleep(Duration::from_millis(12));
    for n in 1..=count.max(1) {
        for (i, ty) in [down, up].into_iter().enumerate() {
            let e = mouse_event_in(target, ty, at, b, n as i64, gesture)?;
            CGEvent::set_flags(Some(&e), flags(modifiers));
            post(target, &e);
            // A button's tracking loop polls for the up; too quick and it
            // misses it.
            sleep(Duration::from_millis(if i == 0 { 28 } else { 0 }));
        }
        if count > 1 {
            sleep(Duration::from_millis(80));
        }
    }
    Ok(())
}

/// A left click the way Chromium (Chrome, Electron apps) accepts one in a
/// background window, after trycua/cua's driver. Firefox takes this one too,
/// and ignores the AppKit-style click altogether: a move to the target, a
/// primer press at (-1, -1) outside every window to satisfy Chromium's
/// user-activation gate, then the real press. Every event carries a gesture
/// phase in field 0, and its "window location" is the screen point, which
/// is how Chromium reads it on this path.
pub fn click_chromium(target: &Target, at: CGPoint, count: u32, modifiers: Modifiers) -> Result<()> {
    let gesture = gesture_id();
    let off = CGPoint { x: -1.0, y: -1.0 };
    let wid = target.window_id as i64;
    let step = |ty: CGEventType, p: CGPoint, click_state: i64, phase: i64| -> Result<()> {
        let e = CGEvent::new_mouse_event(source().as_deref(), ty, p, CGMouseButton::Left)
            .ok_or_else(|| anyhow!("could not make a mouse event"))?;
        sky::set_field(&e, 0, phase);
        sky::set_field(&e, 1, click_state);
        sky::set_field(&e, 3, 0);
        sky::set_field(&e, 7, 3);
        sky::set_field(&e, 51, wid);
        sky::set_field(&e, CGEventField::MouseEventWindowUnderMousePointer.0, wid);
        sky::set_field(&e, CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent.0, wid);
        sky::set_field(&e, 58, gesture);
        sky::set_window_location(&e, p);
        if !modifiers.is_empty() {
            CGEvent::set_flags(Some(&e), flags(modifiers));
        }
        post(target, &e);
        Ok(())
    };
    step(CGEventType::MouseMoved, at, 0, 2)?;
    sleep(Duration::from_millis(15));
    step(CGEventType::LeftMouseDown, off, 1, 1)?;
    sleep(Duration::from_millis(1));
    step(CGEventType::LeftMouseUp, off, 1, 2)?;
    sleep(Duration::from_millis(100));
    let pairs = count.clamp(1, 2);
    for n in 1..=pairs {
        step(CGEventType::LeftMouseDown, at, n as i64, 3)?;
        sleep(Duration::from_millis(1));
        step(CGEventType::LeftMouseUp, at, n as i64, 3)?;
        if n < pairs {
            sleep(Duration::from_millis(80));
        }
    }
    Ok(())
}

pub fn drag(target: &Target, from: CGPoint, to: CGPoint, button: MouseButton) -> Result<()> {
    let (down, up, dragged, b) = button_types(button);
    move_to(target, from)?;
    let e = mouse_event(target, down, from, b)?;
    CGEvent::set_integer_value_field(Some(&e), CGEventField::MouseEventClickState, 1);
    post(target, &e);
    // Intermediate points so apps that track drag distance see a drag.
    const STEPS: u32 = 12;
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        let p = CGPoint { x: from.x + (to.x - from.x) * t, y: from.y + (to.y - from.y) * t };
        sleep(Duration::from_millis(12));
        post(target, &*mouse_event(target, dragged, p, b)?);
    }
    sleep(Duration::from_millis(30));
    let e = mouse_event(target, up, to, b)?;
    CGEvent::set_integer_value_field(Some(&e), CGEventField::MouseEventClickState, 1);
    post(target, &e);
    Ok(())
}

/// Scrolls as a run of wheel notches (line units), the way a real wheel
/// arrives: a single large pixel delta is ignored by some background windows.
pub fn scroll(target: &Target, at: CGPoint, dx: f64, dy: f64) -> Result<()> {
    // A move first, so the window hit-tests the wheel at the right place.
    post(target, &*mouse_event_in(target, CGEventType::MouseMoved, at, CGMouseButton::Left, 0, gesture_id())?);
    sleep(Duration::from_millis(12));
    // About 40 points a line and 3 lines a notch.
    const LINES_PER_NOTCH: f64 = 3.0;
    let lines = |d: f64| (d / 40.0).round();
    let (ly, lx) = (lines(dy), lines(dx));
    let notches = (ly.abs().max(lx.abs()) / LINES_PER_NOTCH).ceil().max(1.0) as i32;
    let wid = target.window_id as i64;
    for i in 0..notches {
        // Spread the lines over the notches; positive dy means "scroll down",
        // and wheel deltas point the other way.
        let share = |l: f64| ((l * (i + 1) as f64 / notches as f64).round() - (l * i as f64 / notches as f64).round()) as i32;
        let (wy, wx) = (-share(ly), -share(lx));
        if wy == 0 && wx == 0 {
            continue;
        }
        let e = CGEvent::new_scroll_wheel_event2(source().as_deref(), CGScrollEventUnit::Line, 2, wy, wx, 0)
            .ok_or_else(|| anyhow!("could not make a scroll event"))?;
        CGEvent::set_location(Some(&e), at);
        sky::set_window_location(&e, CGPoint { x: at.x - target.origin.x, y: at.y - target.origin.y });
        sky::set_field(&e, 51, wid);
        sky::set_field(&e, CGEventField::MouseEventWindowUnderMousePointer.0, wid);
        sky::set_field(&e, CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent.0, wid);
        // Both routes: SkyLight reaches background Chromium-style apps, the
        // public one AppKit views that ignore the other.
        post(target, &e);
        CGEvent::post_to_pid(target.pid, Some(&e));
        sleep(Duration::from_millis(30));
    }
    Ok(())
}

fn key_event(code: u16, down: bool, m: Modifiers, text: Option<char>) -> Result<CFRetained<CGEvent>> {
    let e = CGEvent::new_keyboard_event(None, code, down).ok_or_else(|| anyhow!("could not make a key event"))?;
    CGEvent::set_flags(Some(&e), flags(m));
    if let Some(c) = text {
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        unsafe { CGEvent::keyboard_set_unicode_string(Some(&e), units.len() as _, units.as_ptr()) };
    }
    Ok(e)
}

pub fn press(target: &Target, chord: &Chord) -> Result<()> {
    let (code, shifted, text) = match chord.key {
        None => (None, false, None),
        Some(Key::Named(k)) => (Some(named_code(k)), false, None),
        Some(Key::Char(c)) => match char_code(c) {
            Some((code, shift)) => (Some(code), shift, None),
            // Not on a US keyboard: send it as text on a neutral key.
            None => (Some(0), false, Some(c)),
        },
    };
    let mut m = chord.modifiers;
    m.shift |= shifted;
    let mods = modifier_codes(m);
    for &mc in &mods {
        post(target, &*key_event(mc, true, m, None)?);
    }
    if let Some(code) = code {
        post(target, &*key_event(code, true, m, text)?);
        sleep(Duration::from_millis(10));
        post(target, &*key_event(code, false, m, text)?);
    }
    for &mc in mods.iter().rev() {
        post(target, &*key_event(mc, false, Modifiers::default(), None)?);
    }
    Ok(())
}

pub fn type_text(target: &Target, text: &str) -> Result<()> {
    for c in text.chars() {
        match c {
            '\n' | '\r' => press(target, &Chord { modifiers: Modifiers::default(), key: Some(Key::Named(NamedKey::Enter)) })?,
            '\t' => press(target, &Chord { modifiers: Modifiers::default(), key: Some(Key::Named(NamedKey::Tab)) })?,
            _ => {
                // The real key code where there is one (apps that read key
                // codes, like games and terminals, see a real key), and the
                // character itself as the event's text either way.
                let (code, shift) = char_code(c.to_ascii_lowercase())
                    .filter(|_| c.is_ascii())
                    .map(|(code, s)| (code, s || c.is_ascii_uppercase()))
                    .unwrap_or((0, false));
                let m = Modifiers { shift, ..Default::default() };
                post(target, &*key_event(code, true, m, Some(c))?);
                post(target, &*key_event(code, false, m, Some(c))?);
            }
        }
        sleep(Duration::from_millis(4));
    }
    Ok(())
}

fn modifier_codes(m: Modifiers) -> Vec<u16> {
    let mut v = Vec::new();
    if m.meta {
        v.push(55);
    }
    if m.ctrl {
        v.push(59);
    }
    if m.alt {
        v.push(58);
    }
    if m.shift {
        v.push(56);
    }
    v
}

fn named_code(k: NamedKey) -> u16 {
    match k {
        NamedKey::Enter => 36,
        NamedKey::Tab => 48,
        NamedKey::Space => 49,
        NamedKey::Backspace => 51,
        NamedKey::Escape => 53,
        NamedKey::Delete => 117,
        NamedKey::Home => 115,
        NamedKey::End => 119,
        NamedKey::PageUp => 116,
        NamedKey::PageDown => 121,
        NamedKey::Left => 123,
        NamedKey::Right => 124,
        NamedKey::Down => 125,
        NamedKey::Up => 126,
        NamedKey::CapsLock => 57,
        NamedKey::Insert => 114,
        NamedKey::F(n) => {
            const F: [u16; 20] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90];
            F.get(n as usize - 1).copied().unwrap_or(122)
        }
    }
}

/// The US-layout key code for a character, and whether it needs shift.
fn char_code(c: char) -> Option<(u16, bool)> {
    const PLAIN: &[(char, u16)] = &[
        ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7), ('c', 8), ('v', 9),
        ('b', 11), ('q', 12), ('w', 13), ('e', 14), ('r', 15), ('y', 16), ('t', 17), ('1', 18), ('2', 19),
        ('3', 20), ('4', 21), ('6', 22), ('5', 23), ('=', 24), ('9', 25), ('7', 26), ('-', 27), ('8', 28),
        ('0', 29), (']', 30), ('o', 31), ('u', 32), ('[', 33), ('i', 34), ('p', 35), ('l', 37), ('j', 38),
        ('\'', 39), ('k', 40), (';', 41), ('\\', 42), (',', 43), ('/', 44), ('n', 45), ('m', 46), ('.', 47),
        ('`', 50), (' ', 49),
    ];
    const SHIFTED: &[(char, char)] = &[
        ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'), ('^', '6'), ('&', '7'), ('*', '8'),
        ('(', '9'), (')', '0'), ('_', '-'), ('+', '='), ('{', '['), ('}', ']'), ('|', '\\'), (':', ';'),
        ('"', '\''), ('<', ','), ('>', '.'), ('?', '/'), ('~', '`'),
    ];
    if let Some(&(_, code)) = PLAIN.iter().find(|(p, _)| *p == c) {
        return Some((code, false));
    }
    let base = SHIFTED.iter().find(|(s, _)| *s == c)?.1;
    PLAIN.iter().find(|(p, _)| *p == base).map(|&(_, code)| (code, true))
}
