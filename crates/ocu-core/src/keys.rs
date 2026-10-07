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
    let chords: Vec<Chord> = s.split_whitespace().map(parse_chord).collect::<Result<_>>()?;
    if chords.is_empty() {
        bail!("no keys given");
    }
    Ok(chords)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords() {
        let c = parse_chord("cmd+Shift+T").unwrap();
        assert!(c.modifiers.meta && c.modifiers.shift);
        assert_eq!(c.key, Some(Key::Char('t')));
        assert_eq!(parse_chord("cmd++").unwrap().key, Some(Key::Char('+')));
        assert_eq!(parse_chord("F12").unwrap().key, Some(Key::Named(NamedKey::F(12))));
        assert_eq!(parse_chords("cmd+a cmd+c").unwrap().len(), 2);
        assert!(parse_chord("ctrl+a+b").is_err());
    }
}
