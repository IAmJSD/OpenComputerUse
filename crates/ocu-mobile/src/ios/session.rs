//! One iOS app under control through WebDriverAgent, on a simulator or a
//! device. Coordinates are points, WebDriverAgent's own grid; screenshots
//! are scaled down from pixels to match.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use base64::Engine as _;
use serde_json::{json, Value};

use ocu_core::keys::{Chord, Key, NamedKey};
use ocu_core::{
    Action, Description, LaunchSpec, MouseButton, Rect, Screenshot, Session, TreeOptions, UiNode,
    WindowInfo,
};

use super::tree;
use super::wda::Wda;
use crate::elements::Elements;
use crate::keys::{self, Button, Press};
use crate::lease::Lease;
use crate::picture;

pub struct IosSession {
    wda: Wda,
    bundle: String,
    /// The app wasn't running before, so it is quit at the end.
    started: bool,
    /// Pixels per point.
    scale: f64,
    size: (f64, f64),
    elements: Elements,
    backend: &'static str,
    notes: BTreeMap<String, String>,
    closed: bool,
    _lease: Lease<String>,
}

impl IosSession {
    pub fn start(
        lease: Lease<String>,
        spec: &LaunchSpec,
        backend: &'static str,
        mut notes: BTreeMap<String, String>,
    ) -> Result<Self> {
        if spec.active_window {
            bail!("on iOS, start an app by its bundle id (com.apple.Preferences) rather than attaching to the window in front");
        }
        let bundle = spec.app.trim().to_string();
        if bundle.is_empty() || bundle.contains('/') {
            bail!("name the app by its bundle id, such as com.apple.mobilesafari (the *_apps tools list them)");
        }
        let mut wda = Wda::new(&lease.value);
        let screen = wda.call("/wda/screen", None)?;
        let scale = screen["scale"].as_f64().unwrap_or(1.0).max(1.0);
        let size = wda.call("/window/size", None)?;
        let size = (
            size["width"].as_f64().unwrap_or(0.0),
            size["height"].as_f64().unwrap_or(0.0),
        );
        // 1 not running, 2 suspended, 3 in the background, 4 in front;
        // 0 when iOS has never heard of it.
        let state = wda
            .call("/wda/apps/state", Some(json!({ "bundleId": bundle })))?
            .as_u64()
            .unwrap_or(0);
        let started = state <= 1;
        if started {
            wda.call(
                "/wda/apps/launch",
                Some(json!({
                    "bundleId": bundle,
                    "arguments": spec.args,
                    "environment": spec.env,
                })),
            )
            .with_context(|| {
                format!("launching {bundle} (is it installed? the *_apps tools list what is)")
            })?;
        } else {
            wda.call("/wda/apps/activate", Some(json!({ "bundleId": bundle })))?;
            notes.insert(
                "attached".into(),
                "already running; it is left running when the session ends".into(),
            );
        }
        notes.insert("webdriveragent".into(), lease.value.clone());
        Ok(Self {
            wda,
            bundle,
            started,
            scale,
            size,
            elements: Elements::default(),
            backend,
            notes,
            closed: false,
            _lease: lease,
        })
    }

    fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        self.wda.call("/wda/tap", Some(json!({ "x": x, "y": y })))?;
        Ok(())
    }

    /// A finger from `from` to `to`, resting `hold_ms` first (a long hold
    /// picks things up to drag; a short one scrolls).
    fn swipe(&mut self, from: (f64, f64), to: (f64, f64), hold_ms: u64, ms: u64) -> Result<()> {
        let actions = json!({ "actions": [ {
            "type": "pointer",
            "id": "finger",
            "parameters": { "pointerType": "touch" },
            "actions": [
                { "type": "pointerMove", "duration": 0, "x": from.0, "y": from.1 },
                { "type": "pointerDown", "button": 0 },
                { "type": "pause", "duration": hold_ms },
                { "type": "pointerMove", "duration": ms, "x": to.0, "y": to.1 },
                { "type": "pointerUp", "button": 0 },
            ],
        } ] });
        self.wda.call("/actions", Some(actions))?;
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        if !text.is_empty() {
            self.wda
                .call("/wda/keys", Some(json!({ "value": [text] })))?;
        }
        Ok(())
    }

    fn press(&mut self, keys: &str) -> Result<()> {
        for p in keys::parse(keys)? {
            match p {
                Press::Button(Button::Home) => {
                    self.wda.post("/wda/homescreen", json!({}))?;
                }
                Press::Button(Button::VolumeUp) => {
                    self.wda
                        .call("/wda/pressButton", Some(json!({ "name": "volumeUp" })))?;
                }
                Press::Button(Button::VolumeDown) => {
                    self.wda
                        .call("/wda/pressButton", Some(json!({ "name": "volumeDown" })))?;
                }
                Press::Button(Button::Power) => {
                    self.wda.call("/wda/lock", Some(json!({})))?;
                }
                Press::Button(b) => bail!(
                    "iOS has no {b:?} button; swipe from the left edge to go back, or press home"
                ),
                Press::Chord(c) => {
                    let s = chord_text(&c)?;
                    self.type_text(&s)?;
                }
            }
        }
        Ok(())
    }

    /// WebDriverAgent's id for an element of the last tree.
    fn find(&mut self, element: &str) -> Result<String> {
        let index = *self
            .elements
            .get(element)?
            .path
            .first()
            .context("that element can't be looked up; read the tree again")?;
        let v = self.wda.call(
            "/element",
            Some(json!({ "using": "xpath", "value": format!("(//*)[{index}]") })),
        )?;
        v.get("ELEMENT")
            .or_else(|| v.get("element-6066-11e4-a52e-4f735466cecf"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("WebDriverAgent didn't find the element; read the tree again")
    }

    fn element_action(&mut self, element: &str, name: Option<&str>) -> Result<()> {
        let el = self.elements.get(element)?.clone();
        let (cx, cy) = el.frame.center();
        match name.unwrap_or("press").to_ascii_lowercase().as_str() {
            "press" | "click" | "tap" | "focus" | "pick" | "confirm" => {
                // Its own tap when it can be found; the spot otherwise.
                let tapped = self.find(element).and_then(|id| {
                    self.wda
                        .call(&format!("/element/{id}/click"), Some(json!({})))
                });
                if tapped.is_err() {
                    self.tap(cx, cy)?;
                }
                Ok(())
            }
            "longpress" | "long_press" | "showmenu" => {
                self.wda.call(
                    "/wda/touchAndHold",
                    Some(json!({ "x": cx, "y": cy, "duration": 1.0 })),
                )?;
                Ok(())
            }
            dir @ ("scrollforward" | "scroll_forward" | "scrollbackward" | "scroll_backward") => {
                let f = el.frame;
                let (top, bottom) = (f.y + f.height * 0.25, f.y + f.height * 0.75);
                if dir.contains("forward") {
                    self.swipe((cx, bottom), (cx, top), 50, 300)
                } else {
                    self.swipe((cx, top), (cx, bottom), 50, 300)
                }
            }
            dir @ ("increment" | "decrement") => {
                let id = self.find(element)?;
                let direction = if dir == "increment" { "right" } else { "left" };
                self.wda.call(
                    &format!("/wda/element/{id}/swipe"),
                    Some(json!({ "direction": direction })),
                )?;
                Ok(())
            }
            other => bail!(
                "unknown action \"{other}\"; iOS elements take press, focus, longpress, \
                 scrollforward, scrollbackward, increment, decrement"
            ),
        }
    }

    fn set_value(&mut self, element: &str, value: &str) -> Result<()> {
        let el = self.elements.get(element)?.clone();
        let direct = self.find(element).and_then(|id| {
            self.wda
                .call(&format!("/element/{id}/clear"), Some(json!({})))?;
            self.wda.call(
                &format!("/element/{id}/value"),
                Some(json!({ "value": [value] })),
            )
        });
        if direct.is_ok() {
            return Ok(());
        }
        // Some fields turn into another on focus (Safari's address bar):
        // tap it, delete what was there, and type.
        let (cx, cy) = el.frame.center();
        self.tap(cx, cy)?;
        std::thread::sleep(Duration::from_millis(400));
        if el.value_len > 0 {
            self.type_text(&"\u{8}".repeat(el.value_len))?;
        }
        self.type_text(value)
    }
}

fn chord_text(c: &Chord) -> Result<String> {
    if c.modifiers.ctrl || c.modifiers.alt || c.modifiers.meta {
        bail!("iOS takes no keyboard shortcuts; use the app's buttons or element_action");
    }
    let Some(key) = c.key else {
        bail!("a modifier alone does nothing on iOS");
    };
    Ok(match key {
        Key::Named(n) => match n {
            NamedKey::Enter => "\n".into(),
            NamedKey::Tab => "\t".into(),
            NamedKey::Backspace => "\u{8}".into(),
            NamedKey::Delete => "\u{7f}".into(),
            NamedKey::Space => " ".into(),
            NamedKey::Escape => "\u{1b}".into(),
            other => bail!("iOS's keyboard has no {other:?} key"),
        },
        Key::Char(ch) if c.modifiers.shift => ch.to_uppercase().collect(),
        Key::Char(ch) => ch.to_string(),
    })
}

impl Session for IosSession {
    fn describe(&self) -> Description {
        Description {
            backend: Some(self.backend.to_string()),
            app: self.bundle.clone(),
            pid: None,
            details: self.notes.clone(),
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(vec![WindowInfo {
            id: 0,
            title: self.bundle.clone(),
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: self.size.0,
                height: self.size.1,
            },
            on_screen: false,
        }])
    }

    fn screenshot(&mut self, _window: Option<u64>) -> Result<Screenshot> {
        let v = self.wda.get("/screenshot")?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(v.as_str().context("WebDriverAgent sent no picture")?)?;
        let (png, width, height) = picture::to_points(&png, self.scale)?;
        Ok(Screenshot {
            window_id: Some(0),
            width,
            height,
            png,
        })
    }

    fn ui_tree(&mut self, _window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        let source = self.wda.call(
            "/source?format=json&excluded_attributes=visible,accessible",
            None,
        )?;
        self.elements = Elements::default();
        Ok(tree::build(
            &source,
            &self.bundle,
            self.size,
            opts,
            &mut self.elements,
        ))
    }

    fn perform(&mut self, _window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Click {
                x,
                y,
                button,
                count,
                ..
            } => match (button, count) {
                (MouseButton::Left, 2) => {
                    self.wda
                        .call("/wda/doubleTap", Some(json!({ "x": x, "y": y })))?;
                    Ok(())
                }
                (MouseButton::Left, n) => {
                    for _ in 0..(*n).max(1) {
                        self.tap(*x, *y)?;
                    }
                    Ok(())
                }
                (MouseButton::Right, _) => {
                    self.wda.call(
                        "/wda/touchAndHold",
                        Some(json!({ "x": x, "y": y, "duration": 1.0 })),
                    )?;
                    Ok(())
                }
                (MouseButton::Middle, _) => bail!("touch screens have no middle button"),
            },
            Action::MoveMouse { .. } => {
                bail!("touch screens have no pointer to hover with; click or drag instead")
            }
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                ..
            } => self.swipe((*from_x, *from_y), (*to_x, *to_y), 600, 500),
            Action::Scroll { x, y, dx, dy } => {
                let (w, h) = self.size;
                let clamp = |v: f64, max: f64| v.clamp(1.0, (max - 1.0).max(1.0));
                self.swipe((*x, *y), (clamp(x - dx, w), clamp(y - dy, h)), 50, 300)
            }
            Action::TypeText { text } => self.type_text(text),
            Action::PressKey { keys } => self.press(keys),
            Action::ElementAction { element, name } => {
                self.element_action(element, name.as_deref())
            }
            Action::SetValue { element, value } => self.set_value(element, value),
            Action::Focus { element } => self.element_action(element, Some("focus")),
            Action::Wait { ms } => {
                std::thread::sleep(Duration::from_millis(*ms));
                Ok(())
            }
            Action::ChooseFile { .. } => bail!("a phone has no file panels to answer"),
        }
    }

    fn is_alive(&mut self) -> bool {
        !self.closed && self.wda.is_ready()
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if self.started {
            let _ = self.wda.call(
                "/wda/apps/terminate",
                Some(json!({ "bundleId": self.bundle })),
            );
        }
        self.wda.end_session();
    }
}

impl Drop for IosSession {
    fn drop(&mut self) {
        self.close();
    }
}
