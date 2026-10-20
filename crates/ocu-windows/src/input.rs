//! Input as window messages posted to the control under the point (mouse)
//! or the control with keyboard focus in the app's GUI thread (keys), so
//! the real cursor, keyboard and foreground window are left alone.
//!
//! Apps that read the global key state (Ctrl held?) get it from a brief
//! attachment to their input queue. Apps that ignore posted mouse
//! messages (some Chromium surfaces) are better driven by element ids.

use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardState, MapVirtualKeyW, SetKeyboardState, VkKeyScanW, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
    VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME,
    VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, GetGUIThreadInfo, GetWindowThreadProcessId, PostMessageW,
    CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, GUITHREADINFO, WM_CHAR, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

use ocu_core::keys::{Chord, Key, Modifiers, NamedKey};
use ocu_core::MouseButton;

const MK_LBUTTON: usize = 0x1;
const MK_RBUTTON: usize = 0x2;
const MK_SHIFT: usize = 0x4;
const MK_CONTROL: usize = 0x8;
const MK_MBUTTON: usize = 0x10;

fn lparam_xy(x: i32, y: i32) -> LPARAM {
    LPARAM(((y as u16 as u32) << 16 | (x as u16 as u32)) as isize)
}

/// The deepest visible, enabled child of `top` at screen point `p`, and
/// `p` in its client coordinates.
fn target(top: HWND, p: POINT) -> (HWND, POINT) {
    let mut hwnd = top;
    loop {
        let mut local = p;
        let _ = unsafe { ScreenToClient(hwnd, &mut local) };
        let child = unsafe {
            ChildWindowFromPointEx(
                hwnd,
                local,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        if child.is_invalid() || child == hwnd {
            return (hwnd, local);
        }
        hwnd = child;
    }
}

fn post(hwnd: HWND, msg: u32, w: usize, l: LPARAM) {
    let _ = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(w), l) };
}

fn mk(m: Modifiers) -> usize {
    (if m.shift { MK_SHIFT } else { 0 }) | (if m.ctrl { MK_CONTROL } else { 0 })
}

pub fn move_to(top: HWND, p: POINT) {
    let (h, local) = target(top, p);
    post(h, WM_MOUSEMOVE, 0, lparam_xy(local.x, local.y));
}

pub fn click(top: HWND, p: POINT, button: MouseButton, count: u32, m: Modifiers) {
    let (h, local) = target(top, p);
    let at = lparam_xy(local.x, local.y);
    let (down, up, flag) = match button {
        MouseButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON),
        MouseButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON),
        MouseButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON),
    };
    with_modifiers(top, m, || {
        post(h, WM_MOUSEMOVE, mk(m), at);
        for n in 0..count.max(1) {
            let msg = if n == 1 && button == MouseButton::Left {
                WM_LBUTTONDBLCLK
            } else {
                down
            };
            post(h, msg, flag | mk(m), at);
            sleep(Duration::from_millis(20));
            post(h, up, mk(m), at);
            sleep(Duration::from_millis(30));
        }
    });
}

fn button_messages(button: MouseButton) -> (u32, u32, usize) {
    match button {
        MouseButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON),
        MouseButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON),
        MouseButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON),
    }
}

/// Presses `button` at `p` in the control under it, and returns that
/// control: a real press captures the mouse to it, so the moves and the
/// release that follow go there too.
pub fn button_down(top: HWND, p: POINT, button: MouseButton, m: Modifiers) -> HWND {
    let (h, local) = target(top, p);
    let at = lparam_xy(local.x, local.y);
    let (down, _, flag) = button_messages(button);
    with_modifiers(top, m, || {
        post(h, WM_MOUSEMOVE, mk(m), at);
        post(h, down, flag | mk(m), at);
    });
    h
}

/// A move with `button` held, to `pressed`, the control it went down in.
pub fn move_held(top: HWND, pressed: HWND, p: POINT, button: MouseButton, m: Modifiers) {
    let mut local = p;
    let _ = unsafe { ScreenToClient(pressed, &mut local) };
    let (_, _, flag) = button_messages(button);
    with_modifiers(top, m, || {
        post(
            pressed,
            WM_MOUSEMOVE,
            flag | mk(m),
            lparam_xy(local.x, local.y),
        )
    });
}

/// Releases `button` at `p`, in `pressed` when the press is known, else in
/// the control under the point.
pub fn button_up(top: HWND, pressed: Option<HWND>, p: POINT, button: MouseButton, m: Modifiers) {
    let (h, local) = match pressed {
        Some(h) => {
            let mut local = p;
            let _ = unsafe { ScreenToClient(h, &mut local) };
            (h, local)
        }
        None => target(top, p),
    };
    let at = lparam_xy(local.x, local.y);
    let (_, up, flag) = button_messages(button);
    with_modifiers(top, m, || {
        post(h, WM_MOUSEMOVE, flag | mk(m), at);
        post(h, up, mk(m), at);
    });
}

pub fn drag(top: HWND, from: POINT, to: POINT, button: MouseButton, m: Modifiers) {
    let (h, a) = target(top, from);
    let (down, up, flag) = match button {
        MouseButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON),
        MouseButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON),
        MouseButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON),
    };
    with_modifiers(top, m, || {
        post(h, down, flag | mk(m), lparam_xy(a.x, a.y));
        // The press captures the mouse to `h`; keep sending there.
        let d = (to.x - from.x, to.y - from.y);
        for i in 1..=12 {
            sleep(Duration::from_millis(12));
            post(
                h,
                WM_MOUSEMOVE,
                flag | mk(m),
                lparam_xy(a.x + d.0 * i / 12, a.y + d.1 * i / 12),
            );
        }
        post(h, up, mk(m), lparam_xy(a.x + d.0, a.y + d.1));
    });
}

pub fn scroll(top: HWND, p: POINT, dx: f64, dy: f64) {
    let (h, _) = target(top, p);
    // Wheel messages carry screen coordinates; 120 is one notch, ~40px.
    let screen = lparam_xy(p.x, p.y);
    if dy != 0.0 {
        let delta = (-dy / 40.0 * 120.0).round() as i16;
        post(h, WM_MOUSEWHEEL, (delta as u16 as usize) << 16, screen);
    }
    if dx != 0.0 {
        let delta = (dx / 40.0 * 120.0).round() as i16;
        post(h, WM_MOUSEHWHEEL, (delta as u16 as usize) << 16, screen);
    }
}

/// Where keys go: the control with focus in the window's GUI thread,
/// which a thread keeps even while it is in the background.
pub fn focus_target(top: HWND) -> HWND {
    let thread = unsafe { GetWindowThreadProcessId(top, None) };
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok() && !info.hwndFocus.is_invalid() {
        info.hwndFocus
    } else {
        top
    }
}

/// Runs `f` with the app's thread seeing `m` held, for apps that check
/// `GetKeyState(VK_CONTROL)` rather than the message.
fn with_modifiers(top: HWND, m: Modifiers, f: impl FnOnce()) {
    if m.is_empty() {
        return f();
    }
    let theirs = unsafe { GetWindowThreadProcessId(top, None) };
    let ours = unsafe { GetCurrentThreadId() };
    let attached = unsafe { AttachThreadInput(ours, theirs, true) }.as_bool();
    let mut saved = [0u8; 256];
    if attached {
        let _ = unsafe { GetKeyboardState(&mut saved) };
        let mut state = saved;
        for (on, vk) in [
            (m.shift, VK_SHIFT),
            (m.ctrl, VK_CONTROL),
            (m.alt, VK_MENU),
            (m.meta, VK_LWIN),
        ] {
            if on {
                state[vk.0 as usize] |= 0x80;
            }
        }
        let _ = unsafe { SetKeyboardState(&state) };
    }
    f();
    if attached {
        // Posted messages are handled later; give them time to see the state.
        sleep(Duration::from_millis(60));
        let _ = unsafe { SetKeyboardState(&saved) };
        let _ = unsafe { AttachThreadInput(ours, theirs, false) };
    }
}

fn key_lparam(vk: VIRTUAL_KEY, up: bool) -> LPARAM {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as isize;
    let mut l = 1 | (scan << 16);
    if up {
        l |= (1 << 30) | (1 << 31);
    }
    LPARAM(l)
}

pub(crate) fn named_vk(k: NamedKey) -> VIRTUAL_KEY {
    match k {
        NamedKey::Enter => VK_RETURN,
        NamedKey::Tab => VK_TAB,
        NamedKey::Escape => VK_ESCAPE,
        NamedKey::Backspace => VK_BACK,
        NamedKey::Delete => VK_DELETE,
        NamedKey::Space => VK_SPACE,
        NamedKey::Up => VK_UP,
        NamedKey::Down => VK_DOWN,
        NamedKey::Left => VK_LEFT,
        NamedKey::Right => VK_RIGHT,
        NamedKey::Home => VK_HOME,
        NamedKey::End => VK_END,
        NamedKey::PageUp => VK_PRIOR,
        NamedKey::PageDown => VK_NEXT,
        NamedKey::Insert => VK_INSERT,
        NamedKey::CapsLock => VK_CAPITAL,
        NamedKey::F(n) => VIRTUAL_KEY(VK_F1.0 + n as u16 - 1),
    }
}

/// What a chord presses: its key (if the layout has one), the character
/// it types, and its modifiers with shift added where the key needs it.
fn chord_key(chord: &Chord) -> (Option<VIRTUAL_KEY>, Option<char>, Modifiers) {
    let mut m = chord.modifiers;
    let (vk, text) = match chord.key {
        None => (None, None),
        Some(Key::Named(k)) => (Some(named_vk(k)), (k == NamedKey::Space).then_some(' ')),
        Some(Key::Char(c)) => {
            let scan = unsafe { VkKeyScanW(c as u16) };
            if scan == -1 {
                (None, Some(c))
            } else {
                if scan & 0x100 != 0 {
                    m.shift = true;
                }
                (Some(VIRTUAL_KEY((scan & 0xff) as u16)), Some(c))
            }
        }
    };
    (vk, text, m)
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

/// The WM_CHAR a key press would make, which a posted key does not:
/// TranslateMessage makes it from a real key only.
fn post_char(h: HWND, vk: VIRTUAL_KEY, text: Option<char>, m: Modifiers) {
    if let Some(c) = text.filter(|_| !m.ctrl && !m.alt && !m.meta) {
        let shown = if m.shift {
            c.to_uppercase().next().unwrap_or(c)
        } else {
            c
        };
        post(h, WM_CHAR, shown as usize, key_lparam(vk, false));
    }
    if vk == VK_RETURN && !m.ctrl && !m.alt {
        post(h, WM_CHAR, '\r' as usize, key_lparam(vk, false));
    }
}

fn post_text(h: HWND, c: char) {
    let mut buf = [0u16; 2];
    for unit in c.encode_utf16(&mut buf) {
        post(h, WM_CHAR, *unit as usize, LPARAM(1));
    }
}

/// Alt combinations are system keys.
fn key_messages(m: Modifiers) -> (u32, u32) {
    if m.alt {
        (WM_SYSKEYDOWN, WM_SYSKEYUP)
    } else {
        (WM_KEYDOWN, WM_KEYUP)
    }
}

pub fn press(top: HWND, chord: &Chord, held: Modifiers) {
    let h = focus_target(top);
    let (vk, text, mut m) = chord_key(chord);
    let mods = modifier_vks(m);
    m = m.with(held);
    let (down, up) = key_messages(m);
    with_modifiers(top, m, || {
        for &mk in &mods {
            post(h, down, mk.0 as usize, key_lparam(mk, false));
        }
        if let Some(vk) = vk {
            post(h, down, vk.0 as usize, key_lparam(vk, false));
            post_char(h, vk, text, m);
            post(h, up, vk.0 as usize, key_lparam(vk, true));
        } else if let Some(c) = text {
            post_text(h, c);
        }
        for &mk in mods.iter().rev() {
            post(h, up, mk.0 as usize, key_lparam(mk, true));
        }
    });
}

/// Presses (`down`) or releases a chord's keys in the focused control:
/// modifiers first on the way down, last on the way up. `held` are the
/// modifiers already down, which the app's key state shows meanwhile.
pub fn hold(top: HWND, chord: &Chord, down: bool, held: Modifiers) {
    let h = focus_target(top);
    let (vk, text, m) = chord_key(chord);
    let mut vks = modifier_vks(m);
    vks.extend(vk);
    let all = m.with(held);
    let (key_down, key_up) = key_messages(all);
    with_modifiers(top, all, || {
        if down {
            for &k in &vks {
                post(h, key_down, k.0 as usize, key_lparam(k, false));
            }
            match vk {
                Some(vk) => post_char(h, vk, text, all),
                None => text.into_iter().for_each(|c| post_text(h, c)),
            }
        } else {
            for &k in vks.iter().rev() {
                post(h, key_up, k.0 as usize, key_lparam(k, true));
            }
        }
    });
}

/// Text as WM_CHAR messages: what a text control acts on.
pub fn type_text(top: HWND, text: &str) {
    let h = focus_target(top);
    for c in text.chars() {
        match c {
            '\n' => press(
                top,
                &Chord {
                    modifiers: Modifiers::default(),
                    key: Some(Key::Named(NamedKey::Enter)),
                },
                Modifiers::default(),
            ),
            c => post_text(h, c),
        }
        sleep(Duration::from_millis(2));
    }
}
