//! A live view for backends with no display of their own to watch (Windows,
//! macOS): the session's own windows and screenshots, polled, and a
//! watcher's input turned into the session's actions. Slower than a display
//! server's frames, but it works wherever a session can be screenshotted and
//! driven.

use std::io::Cursor;
use std::sync::{Arc, Mutex, Weak};

use anyhow::{anyhow, bail, Result};

use crate::backend::{Session, Viewer};
use crate::image::Order;
use crate::types::{Action, Frame, MouseButton, ViewInput, WindowInfo};

type Shared = Mutex<Box<dyn Session>>;

/// How far the pointer may move between a press and its release and still
/// be a click rather than a drag.
const CLICK_SLOP: f64 = 3.0;

pub struct ScreenshotViewer {
    /// Weak, so the view doesn't keep a session going after it ends.
    session: Weak<Shared>,
    state: Mutex<State>,
}

impl ScreenshotViewer {
    pub fn watching(session: &Arc<Shared>) -> Arc<dyn Viewer> {
        Arc::new(Self {
            session: Arc::downgrade(session),
            state: Mutex::new(State::default()),
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut dyn Session) -> Result<R>) -> Result<R> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| anyhow!("the session has ended"))?;
        let mut session = session.lock().unwrap();
        f(&mut **session)
    }
}

impl Viewer for ScreenshotViewer {
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        self.with(|s| s.windows())
    }

    fn frame(&self, window: Option<u64>) -> Result<Frame> {
        let shot = self.with(|s| s.screenshot(window))?;
        let (width, height, data) = rgba(&shot.png)?;
        Ok(Frame {
            window_id: shot.window_id.or(window).unwrap_or(0),
            width,
            height,
            stride: width as usize * 4,
            data,
            order: Order::Rgba,
        })
    }

    fn input(&self, window: Option<u64>, input: &ViewInput) -> Result<()> {
        let actions = self.state.lock().unwrap().actions(input);
        for action in actions {
            self.with(|s| s.perform(window, &action))?;
        }
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.with(|s| Ok(s.is_alive())).unwrap_or(false)
    }
}

/// A screenshot's PNG as 8-bit RGBA, whatever it was saved as.
fn rgba(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let pixels = &buf[..info.buffer_size()];
    let data = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        other => bail!("a screenshot came as {other:?}, which the view can't show"),
    };
    Ok((info.width, info.height, data))
}

/// What a watcher is holding down, between their inputs.
#[derive(Default)]
struct State {
    /// Where a button went down, and which, until it comes up.
    pressed: Option<(f64, f64, MouseButton)>,
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
}

impl State {
    /// The session's actions for one input: a press and its release make a
    /// click (or a drag, if the pointer went far), and a key is pressed with
    /// whatever modifiers are held.
    fn actions(&mut self, input: &ViewInput) -> Vec<Action> {
        match input {
            // Mid-drag, the release says where it ends.
            ViewInput::Move { .. } if self.pressed.is_some() => vec![],
            ViewInput::Move { x, y } => vec![Action::MoveMouse { x: *x, y: *y }],
            ViewInput::Button {
                x,
                y,
                button,
                down: true,
            } => {
                self.pressed = Some((*x, *y, *button));
                vec![]
            }
            ViewInput::Button {
                x, y, down: false, ..
            } => {
                let Some((from_x, from_y, button)) = self.pressed.take() else {
                    return vec![];
                };
                let modifiers = self.modifiers();
                if (x - from_x).hypot(y - from_y) <= CLICK_SLOP {
                    vec![Action::Click {
                        x: from_x,
                        y: from_y,
                        button,
                        count: 1,
                        modifiers,
                    }]
                } else {
                    vec![Action::Drag {
                        from_x,
                        from_y,
                        to_x: *x,
                        to_y: *y,
                        button,
                        modifiers,
                    }]
                }
            }
            ViewInput::Wheel { x, y, dx, dy } => vec![Action::Scroll {
                x: *x,
                y: *y,
                dx: *dx,
                dy: *dy,
            }],
            ViewInput::Key { code, down } => {
                if let Some(held) = self.modifier(code) {
                    *held = *down;
                    return vec![];
                }
                match (down, key_name(code)) {
                    (true, Some(name)) => {
                        let keys = match self.modifiers() {
                            Some(m) => format!("{m}+{name}"),
                            None => name.to_string(),
                        };
                        vec![Action::PressKey { keys }]
                    }
                    _ => vec![],
                }
            }
            ViewInput::Text { text } => vec![Action::TypeText { text: text.clone() }],
        }
    }

    fn modifier(&mut self, code: &str) -> Option<&mut bool> {
        match code {
            "ShiftLeft" | "ShiftRight" => Some(&mut self.shift),
            "ControlLeft" | "ControlRight" => Some(&mut self.ctrl),
            "AltLeft" | "AltRight" => Some(&mut self.alt),
            "MetaLeft" | "MetaRight" | "OSLeft" | "OSRight" => Some(&mut self.meta),
            _ => None,
        }
    }

    /// What's held, as `press_key` and clicks write it: "ctrl+shift".
    fn modifiers(&self) -> Option<String> {
        let held: Vec<&str> = [
            (self.ctrl, "ctrl"),
            (self.alt, "alt"),
            (self.shift, "shift"),
            (self.meta, "cmd"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        (!held.is_empty()).then(|| held.join("+"))
    }
}

/// A key's name in a `press_key` chord, from its DOM `KeyboardEvent.code`.
fn key_name(code: &str) -> Option<String> {
    if let Some(letter) = code.strip_prefix("Key").filter(|l| l.len() == 1) {
        return Some(letter.to_ascii_lowercase());
    }
    if let Some(digit) = code
        .strip_prefix("Digit")
        .or_else(|| code.strip_prefix("Numpad"))
        .filter(|d| d.len() == 1 && d.chars().all(|c| c.is_ascii_digit()))
    {
        return Some(digit.to_string());
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=24).contains(&n).then(|| format!("f{n}"));
    }
    let name = match code {
        "Enter" | "NumpadEnter" => "enter",
        "Tab" => "tab",
        "Escape" => "escape",
        "Backspace" => "backspace",
        "Delete" => "delete",
        "Space" => "space",
        "ArrowUp" => "up",
        "ArrowDown" => "down",
        "ArrowLeft" => "left",
        "ArrowRight" => "right",
        "Home" => "home",
        "End" => "end",
        "PageUp" => "pageup",
        "PageDown" => "pagedown",
        "Insert" => "insert",
        "Minus" | "NumpadSubtract" => "-",
        "Equal" => "=",
        "NumpadAdd" => "+",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Backslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "Comma" => ",",
        "Period" | "NumpadDecimal" => ".",
        "Slash" | "NumpadDivide" => "/",
        "NumpadMultiply" => "*",
        "Backquote" => "`",
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(state: &mut State, x: f64, y: f64, down: bool) -> Vec<Action> {
        state.actions(&ViewInput::Button {
            x,
            y,
            button: MouseButton::Left,
            down,
        })
    }

    #[test]
    fn a_press_and_release_in_place_is_a_click() {
        let mut state = State::default();
        assert!(press(&mut state, 10.0, 20.0, true).is_empty());
        let actions = press(&mut state, 11.0, 21.0, false);
        assert!(matches!(
            actions.as_slice(),
            [Action::Click { x, y, count: 1, modifiers: None, .. }] if *x == 10.0 && *y == 20.0
        ));
    }

    #[test]
    fn a_press_and_release_far_apart_is_a_drag() {
        let mut state = State::default();
        press(&mut state, 10.0, 20.0, true);
        assert!(state
            .actions(&ViewInput::Move { x: 50.0, y: 60.0 })
            .is_empty());
        let actions = press(&mut state, 100.0, 120.0, false);
        assert!(matches!(
            actions.as_slice(),
            [Action::Drag { from_x, to_x, to_y, .. }] if *from_x == 10.0 && *to_x == 100.0 && *to_y == 120.0
        ));
    }

    #[test]
    fn keys_go_with_the_modifiers_held() {
        let mut state = State::default();
        let key = |code: &str, down| ViewInput::Key {
            code: code.into(),
            down,
        };
        assert!(state.actions(&key("ControlLeft", true)).is_empty());
        assert!(state.actions(&key("ShiftRight", true)).is_empty());
        let actions = state.actions(&key("KeyT", true));
        assert!(
            matches!(actions.as_slice(), [Action::PressKey { keys }] if keys == "ctrl+shift+t")
        );
        // Released, they're held no longer; a key coming up does nothing.
        state.actions(&key("ControlLeft", false));
        state.actions(&key("ShiftRight", false));
        assert!(state.actions(&key("KeyT", false)).is_empty());
        let actions = state.actions(&key("Enter", true));
        assert!(matches!(actions.as_slice(), [Action::PressKey { keys }] if keys == "enter"));
    }

    #[test]
    fn names_the_keys_a_watcher_presses() {
        assert_eq!(key_name("KeyA").as_deref(), Some("a"));
        assert_eq!(key_name("Digit7").as_deref(), Some("7"));
        assert_eq!(key_name("Numpad3").as_deref(), Some("3"));
        assert_eq!(key_name("F12").as_deref(), Some("f12"));
        assert_eq!(key_name("ArrowLeft").as_deref(), Some("left"));
        assert_eq!(key_name("Backquote").as_deref(), Some("`"));
        assert_eq!(key_name("NumpadEnter").as_deref(), Some("enter"));
        assert_eq!(key_name("ContextMenu"), None);
    }

    #[test]
    fn reads_a_screenshot_as_rgba() {
        let rgb = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let png = crate::image::encode_png(
            2,
            2,
            2 * 4,
            &[
                rgb[0], rgb[1], rgb[2], 0, rgb[3], rgb[4], rgb[5], 0, rgb[6], rgb[7], rgb[8], 0,
                rgb[9], rgb[10], rgb[11], 0,
            ],
            Order::Rgba,
        )
        .unwrap();
        let (w, h, data) = rgba(&png).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&data[..8], &[1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
