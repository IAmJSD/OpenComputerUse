//! The live view's wire format: what a browser sends (JSON), what it gets
//! back (a small binary header and an image, or JSON), and the bookkeeping
//! between them. The README's "Live view" section is the reference for
//! clients; this follows it.

use std::collections::{BTreeSet, HashSet};

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use ocu_core::image::{encode_png, Order};
use ocu_core::{Frame, MouseButton, ViewInput};

/// Size of a binary frame's header.
pub const HEADER: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jpeg" | "jpg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            _ => None,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Jpeg => 1,
            Self::Png => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
        }
    }
}

/// A part of a window, in its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Where `now` differs from `before`: the box around every changed pixel,
/// all of `now` when the size changed or there is no `before`, or `None`
/// when nothing did.
pub fn changed(before: Option<&Frame>, now: &Frame) -> Option<Region> {
    let whole = Region {
        x: 0,
        y: 0,
        width: now.width,
        height: now.height,
    };
    let Some(before) = before.filter(|b| (b.width, b.height) == (now.width, now.height)) else {
        return Some(whole);
    };
    let row_bytes = now.width as usize * 4;
    fn row(f: &Frame, y: usize, len: usize) -> &[u8] {
        &f.data[y * f.stride..y * f.stride + len]
    }
    let differs = |y: usize| row(before, y, row_bytes) != row(now, y, row_bytes);
    let h = now.height as usize;
    let top = (0..h).find(|&y| differs(y))?;
    let bottom = (top..h).rev().find(|&y| differs(y)).unwrap_or(top);
    let (mut left, mut right) = (usize::MAX, 0);
    for y in top..=bottom {
        let (a, b) = (row(before, y, row_bytes), row(now, y, row_bytes));
        let px = |x: usize| a[x * 4..x * 4 + 4] != b[x * 4..x * 4 + 4];
        let w = now.width as usize;
        if let Some(l) = (0..left.min(w)).find(|&x| px(x)) {
            left = l;
        }
        if let Some(r) = (right..w).rev().find(|&x| px(x)) {
            right = right.max(r);
        }
    }
    if left == usize::MAX {
        return None;
    }
    Some(Region {
        x: left as u32,
        y: top as u32,
        width: (right - left + 1) as u32,
        height: (bottom - top + 1) as u32,
    })
}

/// A binary message: the header, then `region` of `frame` as an image.
pub fn encode(frame: &Frame, region: Region, format: Format, quality: u8) -> Result<Vec<u8>> {
    let (w, h) = (region.width as usize, region.height as usize);
    let mut packed = Vec::with_capacity(w * h * 4);
    for y in region.y as usize..region.y as usize + h {
        let at = y * frame.stride + region.x as usize * 4;
        packed.extend_from_slice(&frame.data[at..at + w * 4]);
    }
    let image = match format {
        Format::Png => encode_png(region.width, region.height, w * 4, &packed, frame.order)?,
        Format::Jpeg => {
            // JPEG ignores alpha; make it opaque so no encoder reads it.
            for px in packed.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
            let mut out = Vec::new();
            let color = match frame.order {
                Order::Bgra => jpeg_encoder::ColorType::Bgra,
                Order::Rgba => jpeg_encoder::ColorType::Rgba,
            };
            jpeg_encoder::Encoder::new(&mut out, quality)
                .encode(&packed, region.width as u16, region.height as u16, color)
                .map_err(|e| anyhow!("encoding JPEG: {e}"))?;
            out
        }
    };
    let keyframe = region.x == 0
        && region.y == 0
        && (region.width, region.height) == (frame.width, frame.height);
    let mut out = Vec::with_capacity(HEADER + image.len());
    out.push(1);
    out.push(format.code());
    out.extend_from_slice(&u16::from(keyframe).to_le_bytes());
    out.extend_from_slice(&(frame.window_id as u32).to_le_bytes());
    for v in [
        region.x,
        region.y,
        region.width,
        region.height,
        frame.width,
        frame.height,
    ] {
        out.extend_from_slice(&(v.min(u16::MAX as u32) as u16).to_le_bytes());
    }
    out.extend_from_slice(&image);
    Ok(out)
}

/// A message from the browser.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Pointer {
        action: PointerAction,
        x: f64,
        y: f64,
        /// `MouseEvent.button`: 0 left, 1 middle, 2 right.
        #[serde(default)]
        button: u8,
    },
    Wheel {
        x: f64,
        y: f64,
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
    },
    Key {
        action: KeyAction,
        code: String,
        #[serde(default)]
        key: String,
    },
    Text {
        text: String,
    },
    Resize {
        width: u32,
        height: u32,
    },
    Window {
        id: u64,
    },
    Refresh,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum PointerAction {
    Move,
    Down,
    Up,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum KeyAction {
    Down,
    Up,
}

pub fn pointer_input(action: PointerAction, x: f64, y: f64, button: u8) -> Result<ViewInput> {
    Ok(match action {
        PointerAction::Move => ViewInput::Move { x, y },
        PointerAction::Down | PointerAction::Up => ViewInput::Button {
            x,
            y,
            button: match button {
                0 => MouseButton::Left,
                1 => MouseButton::Middle,
                2 => MouseButton::Right,
                b => bail!("no button {b}; buttons are 0 (left), 1 (middle) and 2 (right)"),
            },
            down: action == PointerAction::Down,
        },
    })
}

/// Turns a browser's key events into input, one connection's worth.
///
/// A key that types a character (its `key` is one printable character, and
/// no shortcut modifier is held) is sent as that text, so a person on any
/// layout types what their keyboard says; its release is dropped. Every
/// other key (Enter, arrows, shortcuts, modifiers) goes by its `code`, as
/// the key in that place on a US keyboard.
#[derive(Default)]
pub struct Keys {
    held: BTreeSet<String>,
    typed: HashSet<String>,
}

const CTRL: [&str; 2] = ["ControlLeft", "ControlRight"];
const ALT: [&str; 1] = ["AltLeft"];
const META: [&str; 4] = ["MetaLeft", "MetaRight", "OSLeft", "OSRight"];

impl Keys {
    fn holding(&self, codes: &[&str]) -> bool {
        codes.iter().any(|c| self.held.contains(*c))
    }

    pub fn event(&mut self, action: KeyAction, code: &str, key: &str) -> Vec<ViewInput> {
        let up = |code: &str| ViewInput::Key {
            code: code.into(),
            down: false,
        };
        let down = |code: &str| ViewInput::Key {
            code: code.into(),
            down: true,
        };
        match action {
            KeyAction::Down => {
                let mut chars = key.chars();
                let printable =
                    matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_control());
                // AltGr arrives as ControlLeft with AltRight on some systems:
                // that is typing, not a shortcut.
                let altgr = self.held.contains("AltRight");
                let shortcut =
                    (self.holding(&CTRL) && !altgr) || self.holding(&ALT) || self.holding(&META);
                if printable && !shortcut {
                    self.typed.insert(code.into());
                    // Modifiers held for the character (AltGr's) are let go
                    // around it, so the app sees only the character.
                    let lifted: Vec<String> = self
                        .held
                        .iter()
                        .filter(|c| !c.starts_with("Shift"))
                        .cloned()
                        .collect();
                    let mut out: Vec<ViewInput> = lifted.iter().map(|c| up(c)).collect();
                    out.push(ViewInput::Text { text: key.into() });
                    out.extend(lifted.iter().map(|c| down(c)));
                    return out;
                }
                self.held.insert(code.into());
                vec![down(code)]
            }
            KeyAction::Up => {
                if self.typed.remove(code) {
                    return Vec::new();
                }
                self.held.remove(code);
                vec![up(code)]
            }
        }
    }

    /// Releases whatever is still held, when the connection goes.
    pub fn release_all(&mut self) -> Vec<ViewInput> {
        self.typed.clear();
        std::mem::take(&mut self.held)
            .into_iter()
            .map(|code| ViewInput::Key { code, down: false })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: u8) -> Frame {
        Frame {
            window_id: 7,
            width: w,
            height: h,
            stride: w as usize * 4 + 8,
            data: vec![fill; (w as usize * 4 + 8) * h as usize],
            order: Order::Bgra,
        }
    }

    #[test]
    fn changes_are_boxed() {
        let a = frame(10, 8, 0);
        let mut b = a.clone();
        assert_eq!(changed(Some(&a), &b), None);
        let set = |f: &mut Frame, x: usize, y: usize| f.data[y * f.stride + x * 4 + 1] = 9;
        set(&mut b, 3, 2);
        set(&mut b, 6, 5);
        assert_eq!(
            changed(Some(&a), &b),
            Some(Region {
                x: 3,
                y: 2,
                width: 4,
                height: 4
            })
        );
        // Padding past the row's pixels is never compared.
        let mut c = a.clone();
        c.data[c.stride - 1] = 5;
        assert_eq!(changed(Some(&a), &c), None);
        let whole = Region {
            x: 0,
            y: 0,
            width: 10,
            height: 8,
        };
        assert_eq!(changed(None, &a), Some(whole));
        assert_eq!(changed(Some(&frame(4, 4, 0)), &a), Some(whole));
    }

    #[test]
    fn frames_carry_their_header() {
        let f = frame(10, 8, 200);
        let region = Region {
            x: 2,
            y: 3,
            width: 4,
            height: 2,
        };
        let msg = encode(&f, region, Format::Png, 80).unwrap();
        assert_eq!(&msg[..4], &[1, 2, 0, 0]);
        assert_eq!(u32::from_le_bytes(msg[4..8].try_into().unwrap()), 7);
        let u16s: Vec<u16> = msg[8..HEADER]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(u16s, [2, 3, 4, 2, 10, 8]);
        assert_eq!(&msg[HEADER..HEADER + 4], b"\x89PNG");
        let key = encode(
            &f,
            Region {
                x: 0,
                y: 0,
                width: 10,
                height: 8,
            },
            Format::Jpeg,
            80,
        )
        .unwrap();
        assert_eq!(&key[..4], &[1, 1, 1, 0]);
        assert_eq!(&key[HEADER..HEADER + 2], &[0xff, 0xd8]);
    }

    #[test]
    fn messages_parse() {
        let m: ClientMessage =
            serde_json::from_str(r#"{"type":"pointer","action":"down","x":1,"y":2,"button":2}"#)
                .unwrap();
        let ClientMessage::Pointer {
            action,
            x,
            y,
            button,
        } = m
        else {
            panic!()
        };
        assert_eq!(
            pointer_input(action, x, y, button).unwrap(),
            ViewInput::Button {
                x: 1.0,
                y: 2.0,
                button: MouseButton::Right,
                down: true
            }
        );
        assert!(pointer_input(PointerAction::Down, 0.0, 0.0, 4).is_err());
        let m: ClientMessage = serde_json::from_str(r#"{"type":"refresh"}"#).unwrap();
        assert_eq!(m, ClientMessage::Refresh);
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn printable_keys_type_and_shortcuts_press() {
        let mut k = Keys::default();
        let text = |t: &str| ViewInput::Text { text: t.into() };
        let key = |c: &str, down| ViewInput::Key {
            code: c.into(),
            down,
        };
        assert_eq!(k.event(KeyAction::Down, "KeyQ", "a"), [text("a")]);
        assert!(k.event(KeyAction::Up, "KeyQ", "a").is_empty());
        // Shift stays down for a capital: the app sees the character.
        assert_eq!(
            k.event(KeyAction::Down, "ShiftLeft", "Shift"),
            [key("ShiftLeft", true)]
        );
        assert_eq!(k.event(KeyAction::Down, "KeyA", "A"), [text("A")]);
        k.event(KeyAction::Up, "KeyA", "A");
        k.event(KeyAction::Up, "ShiftLeft", "Shift");
        // A shortcut is the key in that place.
        k.event(KeyAction::Down, "ControlLeft", "Control");
        assert_eq!(k.event(KeyAction::Down, "KeyC", "c"), [key("KeyC", true)]);
        assert_eq!(k.event(KeyAction::Up, "KeyC", "c"), [key("KeyC", false)]);
        // AltGr: Control and AltRight held, the character typed without them.
        k.event(KeyAction::Down, "AltRight", "AltGraph");
        assert_eq!(
            k.event(KeyAction::Down, "KeyE", "€"),
            [
                key("AltRight", false),
                key("ControlLeft", false),
                text("€"),
                key("AltRight", true),
                key("ControlLeft", true),
            ]
        );
        assert_eq!(
            k.event(KeyAction::Down, "Enter", "Enter"),
            [key("Enter", true)]
        );
        assert_eq!(k.release_all().len(), 3);
    }
}
