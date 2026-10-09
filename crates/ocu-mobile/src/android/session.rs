//! One Android app under control. With the shell helper (OpenComputerUse's
//! APK) the app runs on a private virtual display, so the phone's own
//! screen is left alone and the app keeps working while the phone is
//! locked; without it, or with `main_display`, it runs on the phone's
//! screen and is driven with adb's `input`, `screencap` and `uiautomator`.
//!
//! Coordinates are points (density-independent pixels), the grid of the
//! session's screenshots, whatever the device's pixel density.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use serde_json::json;

use ocu_core::keys::{Chord, Key, NamedKey};
use ocu_core::{
    Action, Description, LaunchSpec, MouseButton, Rect, Screenshot, Session, TreeOptions, UiNode,
    WindowInfo,
};

use super::helper::Helper;
use super::tree;
use super::Adb;
use crate::elements::Elements;
use crate::keys::{self, Button, Press};
use crate::lease::Lease;
use crate::picture;
use crate::proc::sh_quote;

pub struct AndroidSession {
    adb: Adb,
    package: String,
    activity: String,
    /// The app wasn't running before, so it is stopped at the end.
    started: bool,
    /// 0 for the phone's screen, else the helper's virtual display.
    display: u32,
    helper: Option<Helper>,
    /// Device pixels per point.
    scale: f64,
    size_px: (u32, u32),
    elements: Elements,
    backend: &'static str,
    notes: BTreeMap<String, String>,
    closed: bool,
    _lease: Lease<()>,
}

pub struct Start<'a> {
    pub adb: Adb,
    pub spec: &'a LaunchSpec,
    pub lease: Lease<()>,
    /// OpenComputerUse's APK, for the helper; `None` runs on the phone's
    /// own screen.
    pub apk: Option<PathBuf>,
    pub backend: &'static str,
    /// Why there is no APK, to say so in the session's details.
    pub apk_missing: Option<String>,
}

impl AndroidSession {
    pub fn start(s: Start) -> Result<Self> {
        let Start {
            adb,
            spec,
            lease,
            apk,
            backend,
            apk_missing,
        } = s;
        if spec.active_window {
            bail!("on Android, start an app (a package such as com.android.settings) rather than attaching to the window in front");
        }
        let app = spec.app.trim();
        if app.is_empty() {
            bail!("name the app to start: a package (com.android.settings) or an activity (com.android.settings/.Settings)");
        }
        let activity = if app.contains('/') {
            app.to_string()
        } else {
            adb.launcher_activity(app)?
        };
        let package = activity.split('/').next().unwrap_or(app).to_string();
        let (w, h, dpi) = adb.display_metrics()?;
        let scale = (dpi as f64 / 160.0).max(0.5);
        let started = adb.pid_of(&package).is_none();
        let mut notes = BTreeMap::new();
        notes.insert("serial".into(), adb.serial.clone());
        notes.insert("package".into(), package.clone());

        // The helper reads trees and types any text; with it the app also
        // gets a private display, unless the phone's screen was asked for.
        let mut helper = None;
        let mut display = 0;
        let want_virtual = !spec.device.as_ref().is_some_and(|d| d.main_display);
        match (&apk, adb.sdk_level()) {
            (None, _) => {
                if let Some(why) = apk_missing {
                    notes.insert("screen_note".into(), why);
                }
            }
            (Some(_), level) if level < 30 => {
                notes.insert(
                    "screen_note".into(),
                    format!("Android {level} is too old for the helper (API 30+); using adb on the device's screen"),
                );
            }
            (Some(apk), _) => match Helper::start(&adb, apk) {
                Ok(mut hp) => {
                    if want_virtual {
                        let made = hp
                            .call(
                                "create_display",
                                json!({ "width": w, "height": h, "dpi": dpi }),
                            )
                            .and_then(|r| {
                                r["display"].as_u64().context("the helper made no display")
                            });
                        match made {
                            Ok(id) => display = id as u32,
                            Err(e) => {
                                log::warn!("no virtual display on {}: {e:#}", adb.serial);
                                notes.insert(
                                    "screen_note".into(),
                                    format!(
                                        "no private display ({e:#}); using the device's screen"
                                    ),
                                );
                            }
                        }
                    }
                    helper = Some(hp);
                }
                Err(e) => {
                    log::warn!("no helper on {}: {e:#}", adb.serial);
                    notes.insert(
                        "screen_note".into(),
                        format!(
                            "the helper didn't start ({e:#}); using adb on the device's screen"
                        ),
                    );
                }
            },
        }

        let mut line = String::from("am start -W");
        if display != 0 {
            // A fresh task there, rather than the one on the phone's screen.
            line.push_str(&format!(" --display {display} -f 0x10008000"));
        }
        line.push_str(&format!(" -n {}", sh_quote(&activity)));
        for a in &spec.args {
            line.push(' ');
            line.push_str(&sh_quote(a));
        }
        let out = adb.shell_timeout(&line, Duration::from_secs(60))?;
        if out.contains("Error:") || out.contains("Exception") {
            bail!("starting {activity}: {}", out.trim());
        }
        notes.insert(
            "screen".into(),
            if display == 0 {
                "the device's own screen".into()
            } else {
                format!("private virtual display {display}, off the device's screen")
            },
        );
        if !started {
            notes.insert(
                "attached".into(),
                "already running; it is left running when the session ends".into(),
            );
        }
        let session = Self {
            adb,
            package,
            activity,
            started,
            display,
            helper,
            scale,
            size_px: (w, h),
            elements: Elements::default(),
            backend,
            notes,
            closed: false,
            _lease: lease,
        };
        // Let the first frame arrive before anyone looks.
        std::thread::sleep(Duration::from_millis(600));
        Ok(session)
    }

    fn px(&self, v: f64) -> i64 {
        (v * self.scale).round() as i64
    }

    fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        let (x, y) = (self.px(x), self.px(y));
        match &mut self.helper {
            Some(h) => {
                h.call("tap", json!({ "display": self.display, "x": x, "y": y }))?;
            }
            None => {
                self.adb.shell(&format!("input tap {x} {y}"))?;
            }
        }
        Ok(())
    }

    fn long_press(&mut self, x: f64, y: f64) -> Result<()> {
        let (x, y) = (self.px(x), self.px(y));
        match &mut self.helper {
            Some(h) => {
                h.call(
                    "long_press",
                    json!({ "display": self.display, "x": x, "y": y, "duration_ms": 800 }),
                )?;
            }
            None => {
                self.adb
                    .shell(&format!("input swipe {x} {y} {x} {y} 800"))?;
            }
        }
        Ok(())
    }

    fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        let (x1, y1, x2, y2) = (
            self.px(from.0),
            self.px(from.1),
            self.px(to.0),
            self.px(to.1),
        );
        match &mut self.helper {
            Some(h) => {
                h.call(
                    "swipe",
                    json!({ "display": self.display, "x1": x1, "y1": y1, "x2": x2, "y2": y2, "duration_ms": ms }),
                )?;
            }
            None => {
                self.adb
                    .shell(&format!("input swipe {x1} {y1} {x2} {y2} {ms}"))?;
            }
        }
        Ok(())
    }

    /// Key codes pressed together: modifiers first, the key last.
    fn keys(&mut self, codes: &[u32]) -> Result<()> {
        match &mut self.helper {
            Some(h) => {
                let (key, mods) = codes.split_last().context("no key")?;
                let meta = mods.iter().fold(0u32, |m, c| m | meta_state(*c));
                h.call(
                    "key",
                    json!({ "display": self.display, "keycode": key, "meta": meta }),
                )?;
            }
            None => {
                let list: Vec<String> = codes.iter().map(u32::to_string).collect();
                let line = if codes.len() == 1 {
                    format!("input keyevent {}", list[0])
                } else {
                    format!("input keycombination {}", list.join(" "))
                };
                self.adb.shell(&line)?;
            }
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                self.keys(&[KEYCODE_ENTER])?;
            }
            if line.is_empty() {
                continue;
            }
            match &mut self.helper {
                Some(h) => {
                    h.call("text", json!({ "display": self.display, "text": line }))
                        .context("typing (click the text field first, then give it a moment)")?;
                }
                None => {
                    if !line.chars().all(|c| c.is_ascii() && !c.is_ascii_control()) {
                        bail!(
                            "adb can only type plain ASCII on the device's own screen; \
                             set_value on the field can enter other text"
                        );
                    }
                    // `input text` reads %s as a space; spaces split words.
                    let escaped = line.replace('%', "\\%").replace(' ', "%s");
                    self.adb
                        .shell(&format!("input text {}", sh_quote(&escaped)))?;
                }
            }
        }
        Ok(())
    }

    fn press(&mut self, keys: &str) -> Result<()> {
        for p in keys::parse(keys)? {
            let codes = match p {
                // These would reach the device's own screen, not the app's.
                Press::Button(b @ (Button::Home | Button::Recents)) if self.display != 0 => {
                    bail!(
                        "the app runs on a private display with no {} screen; press back, or end \
                         the session and start another app (main_display: true runs on the \
                         device's own screen)",
                        if b == Button::Home { "home" } else { "recents" }
                    )
                }
                Press::Button(b) => vec![button_code(b)],
                Press::Chord(c) => chord_codes(&c)?,
            };
            self.keys(&codes)?;
        }
        Ok(())
    }

    /// An accessibility action through the helper, when it knows the node.
    fn node_action(&mut self, element: &str, action: &str, text: Option<&str>) -> Result<bool> {
        let path = self.elements.get(element)?.path.clone();
        let Some(h) = &mut self.helper else {
            return Ok(false);
        };
        if path.is_empty() {
            return Ok(false);
        }
        let mut args = json!({ "display": self.display, "path": path, "action": action });
        if let Some(t) = text {
            args["text"] = json!(t);
        }
        let r = h.call("action", args)?;
        Ok(r["performed"].as_bool().unwrap_or(false))
    }

    fn element_action(&mut self, element: &str, name: Option<&str>) -> Result<()> {
        let el = self.elements.get(element)?.clone();
        let (cx, cy) = el.frame.center();
        let name = name.unwrap_or("press").to_ascii_lowercase();
        let name = name.as_str();
        let helper_action = match name {
            "press" | "click" | "tap" | "pick" | "confirm" => "click",
            "longpress" | "long_press" | "showmenu" => "long_click",
            "focus" => "focus",
            "scrollforward" | "scroll_forward" | "increment" => "scroll_forward",
            "scrollbackward" | "scroll_backward" | "decrement" => "scroll_backward",
            "expand" => "expand",
            "collapse" => "collapse",
            "dismiss" | "cancel" => "dismiss",
            "select" => "select",
            other => bail!(
                "unknown action \"{other}\"; Android elements take press, longpress, focus, \
                 scrollforward, scrollbackward, expand, collapse, select, dismiss"
            ),
        };
        // A tap where the element shows is what the app expects; some
        // views report an accessibility click done and do nothing. The node
        // action is for what is out of sight, and what has no tap.
        let (w, h) = self.size_points();
        let in_sight = cx > 0.0 && cy > 0.0 && cx < w && cy < h;
        let touch_first = matches!(helper_action, "click" | "focus" | "select" | "long_click");
        if !(touch_first && in_sight) && self.node_action(element, helper_action, None)? {
            return Ok(());
        }
        match helper_action {
            "click" | "focus" | "select" => self.tap(cx, cy),
            "long_click" => self.long_press(cx, cy),
            "scroll_forward" | "scroll_backward" => {
                let f = el.frame;
                let (top, bottom) = (f.y + f.height * 0.25, f.y + f.height * 0.75);
                if helper_action == "scroll_forward" {
                    self.swipe((cx, bottom), (cx, top), 400)
                } else {
                    self.swipe((cx, top), (cx, bottom), 400)
                }
            }
            other => bail!("\"{other}\" needs the OpenComputerUse helper on the device"),
        }
    }

    fn set_value(&mut self, element: &str, value: &str) -> Result<()> {
        if self.node_action(element, "set_text", Some(value))? {
            return Ok(());
        }
        let (cx, cy) = self.elements.get(element)?.frame.center();
        self.tap(cx, cy)?;
        std::thread::sleep(Duration::from_millis(300));
        // Select everything and delete it, then type.
        self.keys(&[KEYCODE_CTRL_LEFT, KEYCODE_A])?;
        self.keys(&[KEYCODE_DEL])?;
        self.type_text(value)
    }

    fn scroll(&mut self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        // Content moves the opposite way to the finger.
        let (w, h) = self.size_points();
        let clamp = |v: f64, max: f64| v.clamp(1.0, max - 1.0);
        let to = (clamp(x - dx, w), clamp(y - dy, h));
        self.swipe((x, y), to, 350)
    }

    fn size_points(&self) -> (f64, f64) {
        (
            self.size_px.0 as f64 / self.scale,
            self.size_px.1 as f64 / self.scale,
        )
    }

    fn raw_tree(&mut self) -> Result<Vec<tree::RawNode>> {
        if let Some(h) = &mut self.helper {
            let r = h.call("tree", json!({ "display": self.display }))?;
            return tree::parse_helper(&r);
        }
        let mut last = None;
        for _ in 0..3 {
            let out = self
                .adb
                .exec_out("uiautomator dump /dev/tty", Duration::from_secs(30))?;
            let text = String::from_utf8_lossy(&out);
            if text.contains("<hierarchy") {
                return tree::parse_uiautomator(&text);
            }
            // "could not get idle state" while something animates.
            last = Some(text.trim().to_string());
            std::thread::sleep(Duration::from_millis(400));
        }
        bail!(
            "uiautomator couldn't read the screen: {}",
            last.unwrap_or_default()
        )
    }
}

const KEYCODE_ENTER: u32 = 66;
const KEYCODE_DEL: u32 = 67;
const KEYCODE_A: u32 = 29;
const KEYCODE_CTRL_LEFT: u32 = 113;
const KEYCODE_SHIFT_LEFT: u32 = 59;
const KEYCODE_ALT_LEFT: u32 = 57;
const KEYCODE_META_LEFT: u32 = 117;

fn meta_state(code: u32) -> u32 {
    match code {
        KEYCODE_SHIFT_LEFT => 0x1 | 0x40,
        KEYCODE_ALT_LEFT => 0x2 | 0x10,
        KEYCODE_CTRL_LEFT => 0x1000 | 0x2000,
        KEYCODE_META_LEFT => 0x10000 | 0x20000,
        _ => 0,
    }
}

fn button_code(b: Button) -> u32 {
    match b {
        Button::Home => 3,
        Button::Back => 4,
        Button::Recents => 187,
        Button::Power => 26,
        Button::VolumeUp => 24,
        Button::VolumeDown => 25,
        Button::Menu => 82,
    }
}

fn chord_codes(c: &Chord) -> Result<Vec<u32>> {
    let mut codes = Vec::new();
    if c.modifiers.ctrl {
        codes.push(KEYCODE_CTRL_LEFT);
    }
    if c.modifiers.shift {
        codes.push(KEYCODE_SHIFT_LEFT);
    }
    if c.modifiers.alt {
        codes.push(KEYCODE_ALT_LEFT);
    }
    if c.modifiers.meta {
        codes.push(KEYCODE_META_LEFT);
    }
    let Some(key) = c.key else {
        bail!("a modifier alone does nothing on Android");
    };
    codes.push(key_code(key)?);
    Ok(codes)
}

fn key_code(key: Key) -> Result<u32> {
    Ok(match key {
        Key::Named(n) => match n {
            NamedKey::Enter => KEYCODE_ENTER,
            NamedKey::Tab => 61,
            NamedKey::Escape => 111,
            NamedKey::Backspace => KEYCODE_DEL,
            NamedKey::Delete => 112,
            NamedKey::Space => 62,
            NamedKey::Up => 19,
            NamedKey::Down => 20,
            NamedKey::Left => 21,
            NamedKey::Right => 22,
            // "home" is the Home button on phones; this is the editing key.
            NamedKey::Home => 122,
            NamedKey::End => 123,
            NamedKey::PageUp => 92,
            NamedKey::PageDown => 93,
            NamedKey::Insert => 124,
            NamedKey::CapsLock => 115,
            NamedKey::F(n) if (1..=12).contains(&n) => 131 + n as u32 - 1,
            NamedKey::F(n) => bail!("Android has no F{n} key"),
        },
        Key::Char(c) => match c {
            'a'..='z' => 29 + (c as u32 - 'a' as u32),
            '0'..='9' => 7 + (c as u32 - '0' as u32),
            ' ' => 62,
            ',' => 55,
            '.' => 56,
            '`' => 68,
            '-' => 69,
            '=' => 70,
            '[' => 71,
            ']' => 72,
            '\\' => 73,
            ';' => 74,
            '\'' => 75,
            '/' => 76,
            '@' => 77,
            '+' => 81,
            '*' => 17,
            '#' => 18,
            other => bail!("no Android key for \"{other}\"; type_text types it"),
        },
    })
}

impl Session for AndroidSession {
    fn describe(&self) -> Description {
        Description {
            backend: Some(self.backend.to_string()),
            app: self.package.clone(),
            pid: self.adb.pid_of(&self.package),
            details: self.notes.clone(),
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        let (w, h) = self.size_points();
        Ok(vec![WindowInfo {
            id: self.display as u64,
            title: self.package.clone(),
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: w,
                height: h,
            },
            on_screen: self.display == 0,
        }])
    }

    fn screenshot(&mut self, _window: Option<u64>) -> Result<Screenshot> {
        // The helper pictures only its own displays; screencap is quicker
        // for the device's screen.
        if let Some(h) = self.helper.as_mut().filter(|_| self.display != 0) {
            // The helper scales before encoding, which is much quicker on
            // the phone than a full-size picture.
            let r = h.call(
                "screenshot",
                json!({ "display": self.display, "scale": 1.0 / self.scale }),
            )?;
            use base64::Engine as _;
            let png = base64::engine::general_purpose::STANDARD
                .decode(r["png"].as_str().context("the helper sent no picture")?)?;
            let size = |k: &str| r[k].as_u64().unwrap_or(0) as u32;
            return Ok(Screenshot {
                window_id: Some(self.display as u64),
                width: size("width"),
                height: size("height"),
                png,
            });
        }
        let png = self.adb.exec_out("screencap -p", Duration::from_secs(20))?;
        if png.len() < 8 || &png[1..4] != b"PNG" {
            bail!(
                "the device sent no picture: {}",
                crate::proc::clip(String::from_utf8_lossy(&png).trim(), 300)
            );
        }
        let (png, width, height) = picture::to_points(&png, self.scale)?;
        Ok(Screenshot {
            window_id: Some(self.display as u64),
            width,
            height,
            png,
        })
    }

    fn ui_tree(&mut self, _window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        let roots = self.raw_tree()?;
        self.elements = Elements::default();
        Ok(tree::build(
            &roots,
            &self.package,
            self.scale,
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
            } => match button {
                MouseButton::Left => {
                    for _ in 0..(*count).max(1) {
                        self.tap(*x, *y)?;
                    }
                    Ok(())
                }
                // A touch screen's secondary click is a long press.
                MouseButton::Right => self.long_press(*x, *y),
                MouseButton::Middle => bail!("touch screens have no middle button"),
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
            } => self.swipe((*from_x, *from_y), (*to_x, *to_y), 600),
            Action::Scroll { x, y, dx, dy } => self.scroll(*x, *y, *dx, *dy),
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
        }
    }

    fn is_alive(&mut self) -> bool {
        !self.closed && self.adb.is_online()
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if self.started {
            let _ = self
                .adb
                .shell(&format!("am force-stop {}", sh_quote(&self.package)));
        }
        if let Some(mut h) = self.helper.take().filter(|_| self.display != 0) {
            let _ = h.call_timeout(
                "release_display",
                json!({ "display": self.display }),
                Duration::from_secs(5),
            );
        }
        log::info!("ended {} on {}", self.activity, self.adb.serial);
    }
}

impl Drop for AndroidSession {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocu_core::keys::parse_chord;

    #[test]
    fn chords_map_to_keycodes() {
        assert_eq!(
            chord_codes(&parse_chord("ctrl+a").unwrap()).unwrap(),
            vec![113, 29]
        );
        assert_eq!(
            chord_codes(&parse_chord("enter").unwrap()).unwrap(),
            vec![66]
        );
        assert_eq!(chord_codes(&parse_chord("f5").unwrap()).unwrap(), vec![135]);
        assert!(chord_codes(&parse_chord("shift").unwrap()).is_err());
    }
}
