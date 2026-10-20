//! Key chords as models write them ("cmd+shift+t", "enter", "ctrl+alt+f4"),
//! parsed once so every backend maps the same names.

use anyhow::{bail, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Command on macOS, the Windows key, Super on Linux.
    pub meta: bool,
}

impl Modifiers {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Both sets held at once.
    pub fn with(self, other: Self) -> Self {
        Self {
            shift: self.shift || other.shift,
            ctrl: self.ctrl || other.ctrl,
            alt: self.alt || other.alt,
            meta: self.meta || other.meta,
        }
    }

    /// These without `other`'s.
    pub fn without(self, other: Self) -> Self {
        Self {
            shift: self.shift && !other.shift,
            ctrl: self.ctrl && !other.ctrl,
            alt: self.alt && !other.alt,
            meta: self.meta && !other.meta,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    CapsLock,
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Named(NamedKey),
    /// A printable character, lowercased for letters; shift is a modifier.
    Char(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Modifiers,
    /// None when the chord is modifiers alone, such as a bare "shift".
    pub key: Option<Key>,
}

/// Parses whitespace-separated chords: "cmd+a cmd+c".
pub fn parse_chords(s: &str) -> Result<Vec<Chord>> {
    let chords: Vec<Chord> = s
        .split_whitespace()
        .map(parse_chord)
        .collect::<Result<_>>()?;
    if chords.is_empty() {
        bail!("no keys given");
    }
    Ok(chords)
}

/// One chord to hold down, as `key_down` takes it: "shift", "w", "ctrl+c".
pub fn parse_held(s: &str) -> Result<Chord> {
    let s = s.trim();
    if s.is_empty() {
        bail!("no key given");
    }
    if s.split_whitespace().nth(1).is_some() {
        bail!("\"{s}\" is several chords; hold one at a time (\"ctrl+shift\", not \"ctrl shift\")");
    }
    parse_chord(s)
}

pub fn parse_chord(s: &str) -> Result<Chord> {
    let mut modifiers = Modifiers::default();
    let mut key = None;
    // "+" alone or as the last part is the plus key: "cmd++".
    let parts: Vec<&str> = if s == "+" {
        vec!["+"]
    } else if let Some(head) = s.strip_suffix("++") {
        head.split('+').chain(std::iter::once("+")).collect()
    } else {
        s.split('+').collect()
    };
    for part in parts {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "shift" => modifiers.shift = true,
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "option" | "opt" => modifiers.alt = true,
            "cmd" | "command" | "meta" | "super" | "win" | "windows" => modifiers.meta = true,
            _ => {
                if key.is_some() {
                    bail!("\"{s}\" names more than one key");
                }
                key = Some(parse_key(part)?);
            }
        }
    }
    Ok(Chord { modifiers, key })
}

fn parse_key(part: &str) -> Result<Key> {
    let lower = part.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" | "ret" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "esc" | "escape" => NamedKey::Escape,
        "backspace" | "back_space" => NamedKey::Backspace,
        "delete" | "del" | "forwarddelete" => NamedKey::Delete,
        "space" | "spacebar" => NamedKey::Space,
        "up" | "arrowup" | "uparrow" => NamedKey::Up,
        "down" | "arrowdown" | "downarrow" => NamedKey::Down,
        "left" | "arrowleft" | "leftarrow" => NamedKey::Left,
        "right" | "arrowright" | "rightarrow" => NamedKey::Right,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" | "page_up" | "pgup" | "prior" => NamedKey::PageUp,
        "pagedown" | "page_down" | "pgdn" | "next" => NamedKey::PageDown,
        "insert" | "ins" => NamedKey::Insert,
        "capslock" | "caps_lock" => NamedKey::CapsLock,
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                if (1..=24).contains(&n) {
                    return Ok(Key::Named(NamedKey::F(n)));
                }
            }
            let mut chars = part.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(Key::Char(c.to_ascii_lowercase())),
                _ => bail!("unknown key \"{part}\""),
            };
        }
    };
    Ok(Key::Named(named))
}

/// A key by where it sits on the keyboard: its evdev code (what Wayland
/// sends, and X keycodes less 8) and the keysym it gives on a US layout with
/// no modifiers held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalKey {
    pub evdev: u16,
    pub keysym: u32,
}

/// The physical key a DOM `KeyboardEvent.code` names, such as "KeyA",
/// "ShiftLeft" or "Numpad7".
pub fn dom_code(code: &str) -> Option<PhysicalKey> {
    let k = |evdev, keysym| Some(PhysicalKey { evdev, keysym });
    if let Some(c) = code.strip_prefix("Key") {
        let mut chars = c.chars();
        if let (Some(c @ 'A'..='Z'), None) = (chars.next(), chars.next()) {
            let (evdev, _) = us_key(c.to_ascii_lowercase())?;
            return k(evdev, c.to_ascii_lowercase() as u32);
        }
        return None;
    }
    if let Some(d) = code.strip_prefix("Digit") {
        let mut chars = d.chars();
        if let (Some(c @ '0'..='9'), None) = (chars.next(), chars.next()) {
            let (evdev, _) = us_key(c)?;
            return k(evdev, c as u32);
        }
        return None;
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
        return match n {
            1..=10 => k(58 + n, 0xffbd + n as u32),
            11 | 12 => k(76 + n, 0xffbd + n as u32),
            13..=24 => k(170 + n, 0xffbd + n as u32),
            _ => None,
        };
    }
    if let Some(d) = code.strip_prefix("Numpad") {
        if let Ok(n @ 0..=9) = d.parse::<u16>() {
            const EVDEV: [u16; 10] = [82, 79, 80, 81, 75, 76, 77, 71, 72, 73];
            return k(EVDEV[n as usize], 0xffb0 + n as u32);
        }
    }
    match code {
        "Escape" => k(1, 0xff1b),
        "Minus" => k(12, '-' as u32),
        "Equal" => k(13, '=' as u32),
        "Backspace" => k(14, 0xff08),
        "Tab" => k(15, 0xff09),
        "BracketLeft" => k(26, '[' as u32),
        "BracketRight" => k(27, ']' as u32),
        "Enter" => k(28, 0xff0d),
        "ControlLeft" => k(29, 0xffe3),
        "Semicolon" => k(39, ';' as u32),
        "Quote" => k(40, '\'' as u32),
        "Backquote" => k(41, '`' as u32),
        "ShiftLeft" => k(42, 0xffe1),
        "Backslash" => k(43, '\\' as u32),
        "Comma" => k(51, ',' as u32),
        "Period" => k(52, '.' as u32),
        "Slash" => k(53, '/' as u32),
        "ShiftRight" => k(54, 0xffe2),
        "NumpadMultiply" => k(55, 0xffaa),
        "AltLeft" => k(56, 0xffe9),
        "Space" => k(57, ' ' as u32),
        "CapsLock" => k(58, 0xffe5),
        "NumLock" => k(69, 0xff7f),
        "ScrollLock" => k(70, 0xff14),
        "NumpadSubtract" => k(74, 0xffad),
        "NumpadAdd" => k(78, 0xffab),
        "NumpadDecimal" => k(83, 0xffae),
        "IntlBackslash" => k(86, '<' as u32),
        "NumpadEnter" => k(96, 0xff8d),
        "ControlRight" => k(97, 0xffe4),
        "NumpadDivide" => k(98, 0xffaf),
        "PrintScreen" => k(99, 0xff61),
        "AltRight" => k(100, 0xffea),
        "Home" => k(102, 0xff50),
        "ArrowUp" => k(103, 0xff52),
        "PageUp" => k(104, 0xff55),
        "ArrowLeft" => k(105, 0xff51),
        "ArrowRight" => k(106, 0xff53),
        "End" => k(107, 0xff57),
        "ArrowDown" => k(108, 0xff54),
        "PageDown" => k(109, 0xff56),
        "Insert" => k(110, 0xff63),
        "Delete" => k(111, 0xffff),
        "NumpadEqual" => k(117, 0xffbd),
        "Pause" => k(119, 0xff13),
        "MetaLeft" | "OSLeft" => k(125, 0xffeb),
        "MetaRight" | "OSRight" => k(126, 0xffec),
        "ContextMenu" => k(127, 0xff67),
        _ => None,
    }
}

/// Where a character is on a US keyboard: its evdev code and whether it
/// needs shift. Newlines and tabs are Return and Tab.
pub fn us_key(c: char) -> Option<(u16, bool)> {
    const ROWS: [(&str, &str, u16); 4] = [
        ("`1234567890-=", "~!@#$%^&*()_+", 0),
        ("qwertyuiop[]", "QWERTYUIOP{}", 16),
        ("asdfghjkl;'", "ASDFGHJKL:\"", 30),
        ("zxcvbnm,./", "ZXCVBNM<>?", 44),
    ];
    match c {
        ' ' => return Some((57, false)),
        '\n' | '\r' => return Some((28, false)),
        '\t' => return Some((15, false)),
        '\\' => return Some((43, false)),
        '|' => return Some((43, true)),
        _ => {}
    }
    for (plain, shifted, first) in ROWS {
        for (shift, row) in [(false, plain), (true, shifted)] {
            if let Some(i) = row.chars().position(|r| r == c) {
                // The top row starts at the backquote, evdev 41, then 2-13.
                let code = match (first, i) {
                    (0, 0) => 41,
                    (0, i) => 1 + i as u16,
                    (first, i) => first + i as u16,
                };
                return Some((code, shift));
            }
        }
    }
    None
}

/// A named key's evdev code.
pub fn named_evdev(k: NamedKey) -> u16 {
    match k {
        NamedKey::Enter => 28,
        NamedKey::Tab => 15,
        NamedKey::Escape => 1,
        NamedKey::Backspace => 14,
        NamedKey::Delete => 111,
        NamedKey::Space => 57,
        NamedKey::Up => 103,
        NamedKey::Down => 108,
        NamedKey::Left => 105,
        NamedKey::Right => 106,
        NamedKey::Home => 102,
        NamedKey::End => 107,
        NamedKey::PageUp => 104,
        NamedKey::PageDown => 109,
        NamedKey::Insert => 110,
        NamedKey::CapsLock => 58,
        NamedKey::F(n @ 1..=10) => 58 + n as u16,
        NamedKey::F(n @ 11..=12) => 76 + n as u16,
        NamedKey::F(n) => 170 + n.clamp(13, 24) as u16,
    }
}

/// The evdev codes of held modifiers, in pressing order.
pub fn modifier_evdev(m: Modifiers) -> Vec<u16> {
    let mut v = Vec::new();
    if m.ctrl {
        v.push(29);
    }
    if m.alt {
        v.push(56);
    }
    if m.meta {
        v.push(125);
    }
    if m.shift {
        v.push(42);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_chords_are_one_at_a_time() {
        let c = parse_held(" ctrl+c ").unwrap();
        assert!(c.modifiers.ctrl && c.key == Some(Key::Char('c')));
        assert_eq!(parse_held("shift").unwrap().key, None);
        assert!(parse_held("ctrl shift").is_err());
        assert!(parse_held("").is_err());
    }

    #[test]
    fn modifiers_combine_and_release() {
        let shift = parse_chord("shift").unwrap().modifiers;
        let ctrl_shift = parse_chord("ctrl+shift").unwrap().modifiers;
        assert_eq!(shift.with(ctrl_shift), ctrl_shift);
        assert_eq!(
            ctrl_shift.without(shift),
            parse_chord("ctrl").unwrap().modifiers
        );
        assert!(shift.without(ctrl_shift).is_empty());
    }

    #[test]
    fn chords() {
        let c = parse_chord("cmd+Shift+T").unwrap();
        assert!(c.modifiers.meta && c.modifiers.shift);
        assert_eq!(c.key, Some(Key::Char('t')));
        assert_eq!(parse_chord("cmd++").unwrap().key, Some(Key::Char('+')));
        assert_eq!(
            parse_chord("F12").unwrap().key,
            Some(Key::Named(NamedKey::F(12)))
        );
        assert_eq!(parse_chords("cmd+a cmd+c").unwrap().len(), 2);
        assert!(parse_chord("ctrl+a+b").is_err());
    }

    #[test]
    fn physical_keys() {
        assert_eq!(dom_code("KeyA").unwrap().evdev, 30);
        assert_eq!(dom_code("KeyA").unwrap().keysym, 'a' as u32);
        assert_eq!(dom_code("Digit1").unwrap().evdev, 2);
        assert_eq!(dom_code("Digit0").unwrap().evdev, 11);
        assert_eq!(dom_code("F1").unwrap().evdev, 59);
        assert_eq!(
            dom_code("F12").unwrap(),
            PhysicalKey {
                evdev: 88,
                keysym: 0xffc9
            }
        );
        assert_eq!(dom_code("F13").unwrap().evdev, 183);
        assert_eq!(dom_code("Numpad7").unwrap().evdev, 71);
        assert_eq!(dom_code("ArrowLeft").unwrap().evdev, 105);
        assert!(dom_code("KeyAA").is_none() && dom_code("Fn").is_none());
    }

    #[test]
    fn us_keys() {
        assert_eq!(us_key('q'), Some((16, false)));
        assert_eq!(us_key('Q'), Some((16, true)));
        assert_eq!(us_key('`'), Some((41, false)));
        assert_eq!(us_key('~'), Some((41, true)));
        assert_eq!(us_key('1'), Some((2, false)));
        assert_eq!(us_key('='), Some((13, false)));
        assert_eq!(us_key('"'), Some((40, true)));
        assert_eq!(us_key('m'), Some((50, false)));
        assert_eq!(us_key('?'), Some((53, true)));
        assert_eq!(us_key('é'), None);
        // Every printable ASCII character is on the keyboard, and the DOM
        // table agrees with it on where.
        for c in ' '..='~' {
            assert!(us_key(c).is_some(), "{c}");
        }
        assert_eq!(us_key('z').unwrap().0, dom_code("KeyZ").unwrap().evdev);
        assert_eq!(named_evdev(NamedKey::F(11)), 87);
        assert_eq!(named_evdev(NamedKey::F(24)), 194);
    }
}
