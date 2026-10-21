//! Real input, posted where the mouse and keyboard post theirs (the HID
//! event tap), for a session on the whole screen: it moves the user's
//! pointer and goes to whatever is under it or has focus, as a person's
//! input does.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, Result};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
};

use ocu_core::keys::{Chord, Key, Modifiers, NamedKey};
use ocu_core::MouseButton;

use crate::input::{button_types, char_code, chord_codes, flags, key_event, source};

fn post(e: &CGEvent) {
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(e));
}

fn mouse(
    ty: CGEventType,
    at: CGPoint,
    b: CGMouseButton,
    m: Modifiers,
) -> Result<CFRetained<CGEvent>> {
    let e = CGEvent::new_mouse_event(source().as_deref(), ty, at, b)
        .ok_or_else(|| anyhow!("could not make a mouse event"))?;
    CGEvent::set_flags(Some(&e), flags(m));
    Ok(e)
}

/// Where the pointer is, in screen points from the top-left of the main
/// display.
pub fn cursor() -> Result<CGPoint> {
    let e = CGEvent::new(None).ok_or_else(|| anyhow!("could not read the pointer"))?;
    Ok(CGEvent::location(Some(&e)))
}

/// A move to `at`; a drag when `held` is pressed.
pub fn move_to(at: CGPoint, held: Option<MouseButton>, m: Modifiers) -> Result<()> {
    let (ty, b) = match held {
        Some(button) => {
            let (_, _, dragged, b) = button_types(button);
            (dragged, b)
        }
        None => (CGEventType::MouseMoved, CGMouseButton::Left),
    };
    post(&*mouse(ty, at, b, m)?);
    Ok(())
}

/// Presses (`down`) or releases `button` at `at`; `clicks` is the click
/// count it belongs to (2 for the second press of a double click).
pub fn button(
    at: CGPoint,
    button: MouseButton,
    down: bool,
    clicks: i64,
    m: Modifiers,
) -> Result<()> {
    let (press, release, _, b) = button_types(button);
    let e = mouse(if down { press } else { release }, at, b, m)?;
    CGEvent::set_integer_value_field(Some(&e), CGEventField::MouseEventClickState, clicks);
    post(&e);
    Ok(())
}

pub fn click(at: CGPoint, b: MouseButton, count: u32, m: Modifiers) -> Result<()> {
    move_to(at, None, m)?;
    sleep(Duration::from_millis(12));
    for n in 1..=count.max(1) as i64 {
        button(at, b, true, n, m)?;
        sleep(Duration::from_millis(28));
        button(at, b, false, n, m)?;
        sleep(Duration::from_millis(60));
    }
    Ok(())
}

pub fn drag(from: CGPoint, to: CGPoint, b: MouseButton, m: Modifiers) -> Result<()> {
    move_to(from, None, m)?;
    sleep(Duration::from_millis(12));
    button(from, b, true, 1, m)?;
    for i in 1..=12 {
        let t = i as f64 / 12.0;
        sleep(Duration::from_millis(12));
        let p = CGPoint {
            x: from.x + (to.x - from.x) * t,
            y: from.y + (to.y - from.y) * t,
        };
        move_to(p, Some(b), m)?;
    }
    sleep(Duration::from_millis(30));
    button(to, b, false, 1, m)
}

/// About 40 points a line, sent as one wheel event per few lines.
pub fn scroll(at: CGPoint, dx: f64, dy: f64) -> Result<()> {
    move_to(at, None, Modifiers::default())?;
    sleep(Duration::from_millis(12));
    let lines = |d: f64| (d / 40.0).round() as i32;
    let (mut ly, mut lx) = (lines(dy), lines(dx));
    while ly != 0 || lx != 0 {
        let (sy, sx) = (ly.clamp(-3, 3), lx.clamp(-3, 3));
        // Positive dy scrolls content down; wheel deltas point the other way.
        let e = CGEvent::new_scroll_wheel_event2(
            source().as_deref(),
            CGScrollEventUnit::Line,
            2,
            -sy,
            -sx,
            0,
        )
        .ok_or_else(|| anyhow!("could not make a scroll event"))?;
        post(&e);
        ly -= sy;
        lx -= sx;
        sleep(Duration::from_millis(30));
    }
    Ok(())
}

/// Presses (`down`) or releases a chord: modifiers first on the way down,
/// last on the way up. Events carry their flags, so `held` are the
/// modifiers down already, which they keep.
pub fn hold(chord: &Chord, down: bool, held: Modifiers) -> Result<()> {
    let (codes, m, text) = chord_codes(chord);
    let m = m.with(held);
    let (mods, key) = match codes.split_last() {
        Some((k, mods)) if chord.key.is_some() => (mods, Some(*k)),
        _ => (&codes[..], None),
    };
    if down {
        for &c in mods {
            post(&*key_event(c, true, m, None)?);
        }
        if let Some(k) = key {
            post(&*key_event(k, true, m, text)?);
        }
    } else {
        if let Some(k) = key {
            post(&*key_event(k, false, m, text)?);
        }
        for &c in mods.iter().rev() {
            post(&*key_event(c, false, held, None)?);
        }
    }
    Ok(())
}

pub fn press(chord: &Chord, held: Modifiers) -> Result<()> {
    hold(chord, true, held)?;
    sleep(Duration::from_millis(10));
    hold(chord, false, held)
}

/// Text through the keys that make it where a US keyboard has them, with the
/// character itself as each event's text.
pub fn type_text(text: &str) -> Result<()> {
    for c in text.chars() {
        match c {
            '\n' | '\r' => press(
                &Chord {
                    modifiers: Modifiers::default(),
                    key: Some(Key::Named(NamedKey::Enter)),
                },
                Modifiers::default(),
            )?,
            '\t' => press(
                &Chord {
                    modifiers: Modifiers::default(),
                    key: Some(Key::Named(NamedKey::Tab)),
                },
                Modifiers::default(),
            )?,
            _ => {
                let (code, shift) = char_code(c.to_ascii_lowercase())
                    .filter(|_| c.is_ascii())
                    .map(|(code, s)| (code, s || c.is_ascii_uppercase()))
                    .unwrap_or((0, false));
                let m = Modifiers {
                    shift,
                    ..Default::default()
                };
                post(&*key_event(code, true, m, Some(c))?);
                sleep(Duration::from_millis(4));
                post(&*key_event(code, false, m, Some(c))?);
            }
        }
        sleep(Duration::from_millis(4));
    }
    Ok(())
}
