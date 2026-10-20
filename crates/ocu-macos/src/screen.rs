//! A session on the whole screen: each display is a window, pictures are
//! the display as it shows, and input is real, posted where the mouse and
//! keyboard post theirs.

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};
use objc2_core_foundation::CGPoint;

use ocu_core::keys::{parse_chord, parse_chords, parse_held, Modifiers};
use ocu_core::{
    pick_window, Action, CursorPosition, Description, MouseButton, Screenshot, Session, WindowInfo,
};

use crate::{capture, real};

#[derive(Default)]
pub struct ScreenSession {
    /// Modifiers held by `key_down`. Every event carries its own flags, so
    /// they go on each one until released.
    held: Modifiers,
    /// The button `mouse_down` pressed, which makes moves drags.
    pressed: Option<MouseButton>,
}

impl ScreenSession {
    fn window(&self, window: Option<u64>) -> Result<WindowInfo> {
        pick_window(&capture::displays()?, window).cloned()
    }
}

impl Session for ScreenSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        details.insert(
            "screen".into(),
            "the whole screen, one window per display, with real input: it moves your pointer \
             and types into what has focus"
                .into(),
        );
        Description {
            backend: Some("macos-screen".into()),
            app: "Screen".into(),
            pid: None,
            details,
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        capture::displays()
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        capture::capture_display(&self.window(window)?)
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                return Ok(());
            }
            Action::ElementAction { .. }
            | Action::ClickElement { .. }
            | Action::SetValue { .. }
            | Action::Focus { .. } => bail!(
                "a screen session has no accessibility tree; use coordinates, or start a session \
                 on the app"
            ),
            Action::ChooseFile { .. } => {
                bail!("a screen session can't answer file panels; click through them")
            }
            _ => {}
        }
        let w = self.window(window)?;
        let at = |x: f64, y: f64| CGPoint {
            x: w.frame.x + x,
            y: w.frame.y + y,
        };
        let held = self.held;
        let mods = |m: &Option<String>| -> Result<Modifiers> {
            Ok(match m.as_deref().filter(|s| !s.is_empty()) {
                Some(s) => parse_chord(s)?.modifiers.with(held),
                None => held,
            })
        };
        match action {
            Action::Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => real::click(at(*x, *y), *button, *count, mods(modifiers)?),
            Action::MoveMouse { x, y } => real::move_to(at(*x, *y), self.pressed, held),
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                modifiers,
            } => real::drag(
                at(*from_x, *from_y),
                at(*to_x, *to_y),
                *button,
                mods(modifiers)?,
            ),
            Action::Scroll { x, y, dx, dy } => real::scroll(at(*x, *y), *dx, *dy),
            Action::TypeText { text } => real::type_text(text),
            Action::PressKey { keys } => {
                for chord in parse_chords(keys)? {
                    real::press(&chord, held)?;
                    sleep(Duration::from_millis(30));
                }
                Ok(())
            }
            Action::MouseDown { x, y, button } => {
                real::move_to(at(*x, *y), self.pressed, held)?;
                real::button(at(*x, *y), *button, true, 1, held)?;
                self.pressed = Some(*button);
                Ok(())
            }
            Action::MouseUp { x, y, button } => {
                real::move_to(at(*x, *y), self.pressed, held)?;
                real::button(at(*x, *y), *button, false, 1, held)?;
                if self.pressed == Some(*button) {
                    self.pressed = None;
                }
                Ok(())
            }
            Action::KeyDown { key } => {
                let chord = parse_held(key)?;
                real::hold(&chord, true, held)?;
                self.held = held.with(chord.modifiers);
                Ok(())
            }
            Action::KeyUp { key } => {
                let chord = parse_held(key)?;
                self.held = held.without(chord.modifiers);
                real::hold(&chord, false, self.held)
            }
            Action::Wait { .. }
            | Action::ElementAction { .. }
            | Action::ClickElement { .. }
            | Action::SetValue { .. }
            | Action::Focus { .. }
            | Action::ChooseFile { .. } => unreachable!(),
        }
    }

    fn cursor(&mut self, window: Option<u64>) -> Result<Option<CursorPosition>> {
        let w = self.window(window)?;
        let p = real::cursor()?;
        Ok(Some(CursorPosition {
            window_id: w.id,
            x: p.x - w.frame.x,
            y: p.y - w.frame.y,
            live: true,
        }))
    }

    fn is_alive(&mut self) -> bool {
        true
    }

    fn close(&mut self) {
        // Nothing stays pressed after the session.
        if let Some(b) = self.pressed.take() {
            if let Ok(p) = real::cursor() {
                let _ = real::button(p, b, false, 1, Modifiers::default());
            }
        }
        if !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            let _ = real::hold(
                &ocu_core::keys::Chord {
                    modifiers: held,
                    key: None,
                },
                false,
                Modifiers::default(),
            );
        }
    }
}
