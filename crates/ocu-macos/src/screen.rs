//! A session on the whole screen: each display is a window, pictures are
//! the display as it shows, and input is real, posted where the mouse and
//! keyboard post theirs. The tree is the frontmost app's focused window's,
//! in the display's coordinates.

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use objc2_core_foundation::{CFBoolean, CGPoint};

use ocu_core::keys::{parse_chord, parse_chords, parse_held, Held, Modifiers};
use ocu_core::{
    pick_window, Action, CursorPosition, Description, Screenshot, Session, TreeOptions, UiNode,
    WindowInfo,
};

use crate::ax::{self, Element, ElementTable};
use crate::session::{choose_native, frontmost_pid};
use crate::{capture, real};

#[derive(Default)]
pub struct ScreenSession {
    /// What `key_down` and `mouse_down` hold on the user's real keyboard and
    /// mouse, let go when the session ends. Every event carries its own
    /// flags, so the held modifiers go on each one, and a held button makes
    /// moves drags.
    held: Held,
    /// The elements of the last tree read.
    elements: ElementTable,
    /// Apps already asked to build their whole tree (Chromium and Electron
    /// build their web content's only once asked).
    asked: Vec<i32>,
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

    /// The frontmost app's focused window's tree, in `window`'s (a
    /// display's) coordinates.
    fn ui_tree(&mut self, window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        let w = self.window(window)?;
        let pid = frontmost_pid().ok_or_else(|| anyhow!("no app is in front"))?;
        let app = Element::application(pid);
        if !self.asked.contains(&pid) {
            self.asked.push(pid);
            // Errors even when it works, and apps that don't know it ignore it.
            let _ = app.set("AXManualAccessibility", CFBoolean::new(true));
        }
        let root = match app.element("AXFocusedWindow") {
            Some(focused) => focused,
            None => ax::window_element(pid, None)?,
        };
        let origin = CGPoint {
            x: w.frame.x,
            y: w.frame.y,
        };
        Ok(ax::read_tree(&root, origin, opts, &mut self.elements))
    }

    fn element_point(
        &mut self,
        window: Option<u64>,
        element: &str,
    ) -> Result<(Option<u64>, f64, f64)> {
        let w = self.window(window)?;
        let f = self
            .elements
            .get(element)?
            .frame()
            .ok_or_else(|| anyhow!("element {element} has no position"))?;
        Ok((
            Some(w.id),
            f.origin.x + f.size.width / 2.0 - w.frame.x,
            f.origin.y + f.size.height / 2.0 - w.frame.y,
        ))
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                return Ok(());
            }
            Action::ElementAction { element, name } => {
                let el = self.elements.get(element)?;
                return el.perform(&ax::action_name(name.as_deref().unwrap_or("press")));
            }
            Action::SetValue { element, value } => {
                let el = self.elements.get(element)?;
                if el.string("AXRole").as_deref() == Some("AXPopUpButton") {
                    return choose_native(el, value);
                }
                if el.settable("AXFocused") {
                    let _ = el.set("AXFocused", CFBoolean::new(true));
                }
                return el.set("AXValue", &ax::cf_string(value));
            }
            Action::Focus { element } => {
                return self
                    .elements
                    .get(element)?
                    .set("AXFocused", CFBoolean::new(true));
            }
            Action::ClickElement { .. } => bail!("click elements by their point"),
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
        let held = self.held.modifiers;
        let pressed = self.held.buttons.last().copied();
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
            Action::MoveMouse { x, y } => real::move_to(at(*x, *y), pressed, held),
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
                real::move_to(at(*x, *y), pressed, held)?;
                real::button(at(*x, *y), *button, true, 1, held)?;
                self.held.button(*button, true);
                Ok(())
            }
            Action::MouseUp { x, y, button } => {
                real::move_to(at(*x, *y), pressed, held)?;
                real::button(at(*x, *y), *button, false, 1, held)?;
                self.held.button(*button, false);
                Ok(())
            }
            Action::KeyDown { key } => {
                let chord = parse_held(key)?;
                real::hold(&chord, true, held)?;
                self.held.key_down(&chord);
                Ok(())
            }
            Action::KeyUp { key } => {
                let chord = parse_held(key)?;
                self.held.key_up(&chord);
                real::hold(&chord, false, self.held.modifiers)
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

    /// Lets go of whatever is still held: keys, then modifiers, then
    /// buttons where the pointer is.
    fn close(&mut self) {
        for chord in self.held.take_keys() {
            if let Err(e) = real::hold(&chord, false, Modifiers::default()) {
                log::warn!("letting go of a held key: {e:#}");
            }
        }
        let buttons = std::mem::take(&mut self.held.buttons);
        if let (false, Ok(p)) = (buttons.is_empty(), real::cursor()) {
            for b in buttons {
                let _ = real::button(p, b, false, 1, Modifiers::default());
            }
        }
    }
}

impl Drop for ScreenSession {
    fn drop(&mut self) {
        self.close();
    }
}
