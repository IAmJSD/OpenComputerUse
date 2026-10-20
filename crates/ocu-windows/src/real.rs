//! Real input through `SendInput`, for sessions that are allowed to use the
//! user's pointer and keyboard: foreground sessions and the whole screen.
//! It reaches apps the way a mouse and keyboard do, raw input included,
//! which games read instead of window messages. Keys go as scan codes,
//! which is what DirectInput reads.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU,
    VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use ocu_core::keys::{Chord, Key, Modifiers, NamedKey};
use ocu_core::MouseButton;

use crate::input::named_vk;

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // UIPI: input to an app running as administrator from one that isn't.
        bail!(
            "Windows took {sent} of {} input events; an app running as administrator only takes \
             input from another that is",
            inputs.len()
        );
    }
    Ok(())
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A move to `p` on the virtual desktop, in the 0..65535 units absolute
/// moves take.
fn move_input(p: POINT) -> INPUT {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let scale = |v: i32, origin: i32, size: i32| {
        (((v - origin) as f64 * 65535.0) / (size - 1) as f64).round() as i32
    };
    mouse(
        scale(p.x, vx, vw),
        scale(p.y, vy, vh),
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

fn button_flags(b: MouseButton, down: bool) -> MOUSE_EVENT_FLAGS {
    match (b, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    }
}

/// Where the real pointer is, in screen pixels.
pub fn cursor() -> Result<POINT> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p)? };
    Ok(p)
}

pub fn move_to(p: POINT) -> Result<()> {
    send(&[move_input(p)])
}

/// Moves to `p` and presses or releases `b`.
pub fn button(p: POINT, b: MouseButton, down: bool) -> Result<()> {
    send(&[move_input(p), mouse(0, 0, 0, button_flags(b, down))])
}

pub fn click(p: POINT, b: MouseButton, count: u32, m: Modifiers) -> Result<()> {
    with_modifiers(m, || {
        move_to(p)?;
        sleep(Duration::from_millis(12));
        for n in 0..count.max(1) {
            if n > 0 {
                sleep(Duration::from_millis(30));
            }
            send(&[mouse(0, 0, 0, button_flags(b, true))])?;
            sleep(Duration::from_millis(20));
            send(&[mouse(0, 0, 0, button_flags(b, false))])?;
        }
        Ok(())
    })
}

pub fn drag(from: POINT, to: POINT, b: MouseButton, m: Modifiers) -> Result<()> {
    with_modifiers(m, || {
        button(from, b, true)?;
        for i in 1..=12 {
            sleep(Duration::from_millis(12));
            move_to(POINT {
                x: from.x + (to.x - from.x) * i / 12,
                y: from.y + (to.y - from.y) * i / 12,
            })?;
        }
        sleep(Duration::from_millis(20));
        button(to, b, false)
    })
}

/// 120 is one notch, ~40px.
pub fn scroll(p: POINT, dx: f64, dy: f64) -> Result<()> {
    move_to(p)?;
    if dy != 0.0 {
        send(&[mouse(
            0,
            0,
            (-dy / 40.0 * 120.0).round() as i32,
            MOUSEEVENTF_WHEEL,
        )])?;
    }
    if dx != 0.0 {
        send(&[mouse(
            0,
            0,
            (dx / 40.0 * 120.0).round() as i32,
            MOUSEEVENTF_HWHEEL,
        )])?;
    }
    Ok(())
}

/// A key by its scan code, as a keyboard sends it.
fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC_EX) };
    let mut flags = KEYEVENTF_SCANCODE;
    // Arrows, Home, Insert and the like sit on the 0xE0 page.
    if scan & 0xff00 == 0xe000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    keyboard(VIRTUAL_KEY(0), scan as u16 & 0xff, flags)
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A UTF-16 unit as typed text: for characters no key on the layout makes.
fn unicode(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    keyboard(VIRTUAL_KEY(0), unit, flags)
}

fn modifier_vks(m: Modifiers) -> Vec<VIRTUAL_KEY> {
    [
        (m.ctrl, VK_CONTROL),
        (m.alt, VK_MENU),
        (m.shift, VK_SHIFT),
        (m.meta, VK_LWIN),
    ]
    .into_iter()
    .filter_map(|(on, vk)| on.then_some(vk))
    .collect()
}

fn with_modifiers(m: Modifiers, f: impl FnOnce() -> Result<()>) -> Result<()> {
    let vks = modifier_vks(m);
    let downs: Vec<INPUT> = vks.iter().map(|&vk| key_input(vk, false)).collect();
    send(&downs)?;
    let result = f();
    let ups: Vec<INPUT> = vks.iter().rev().map(|&vk| key_input(vk, true)).collect();
    send(&ups)?;
    result
}

/// What a chord presses: its virtual keys, modifiers first, or a character
/// the layout has no key for.
enum Keys {
    Vks(Vec<VIRTUAL_KEY>),
    Text(char),
}

fn chord_keys(chord: &Chord) -> Result<Keys> {
    let mut m = chord.modifiers;
    let key = match chord.key {
        None => None,
        Some(Key::Named(k)) => Some(named_vk(k)),
        Some(Key::Char(c)) => {
            let scan = unsafe { VkKeyScanW(c as u16) };
            if scan == -1 {
                if m.is_empty() {
                    return Ok(Keys::Text(c));
                }
                bail!("no key on this keyboard layout makes \"{c}\" to press with modifiers");
            } else {
                if scan & 0x100 != 0 {
                    m.shift = true;
                }
                Some(VIRTUAL_KEY((scan & 0xff) as u16))
            }
        }
    };
    let mut vks = modifier_vks(m);
    vks.extend(key);
    Ok(Keys::Vks(vks))
}

/// Presses (`down`) or releases a chord: modifiers first on the way down,
/// last on the way up.
pub fn hold(chord: &Chord, down: bool) -> Result<()> {
    match chord_keys(chord)? {
        Keys::Vks(mut vks) => {
            if !down {
                vks.reverse();
            }
            let inputs: Vec<INPUT> = vks.iter().map(|&vk| key_input(vk, !down)).collect();
            send(&inputs)
        }
        Keys::Text(c) => {
            let mut buf = [0u16; 2];
            let inputs: Vec<INPUT> = c
                .encode_utf16(&mut buf)
                .iter()
                .map(|&u| unicode(u, !down))
                .collect();
            send(&inputs)
        }
    }
}

pub fn press(chord: &Chord) -> Result<()> {
    hold(chord, true)?;
    sleep(Duration::from_millis(15));
    hold(chord, false)
}

/// Text through the keys that make it where the layout has them (apps that
/// read keys, such as games, see real presses), as typed text otherwise.
pub fn type_text(text: &str) -> Result<()> {
    for c in text.chars() {
        let chord = |key| Chord {
            modifiers: Modifiers::default(),
            key: Some(key),
        };
        match c {
            '\n' | '\r' => press(&chord(Key::Named(NamedKey::Enter)))?,
            '\t' => {
                send(&[key_input(VK_TAB, false), key_input(VK_TAB, true)])?;
            }
            c => press(&chord(Key::Char(c)))?,
        }
        sleep(Duration::from_millis(4));
    }
    Ok(())
}
