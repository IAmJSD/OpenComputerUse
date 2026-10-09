//! The values that cross every boundary: MCP tool arguments, the agent's
//! socket, and the platform backends.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// What to start a session with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// An app bundle path, bundle id or app name on macOS; an executable
    /// path or a name on `PATH` elsewhere.
    pub app: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Start a fresh instance even when the app is already running
    /// (macOS; elsewhere every launch is a fresh process).
    #[serde(default)]
    pub new_instance: bool,
    /// Bring the app to the front when it starts and before every action,
    /// instead of keeping it behind the user's windows (macOS, Windows).
    #[serde(default)]
    pub foreground: bool,
    /// Attach to the window in front instead of starting `app`: the topmost
    /// normal window not owned by one of `skip_pids` (macOS, Windows). Its
    /// app is left running when the session ends.
    #[serde(default)]
    pub active_window: bool,
    /// Processes whose windows never count as the one in front: the client
    /// asking, and the apps it runs inside (a terminal, an editor).
    #[serde(default)]
    pub skip_pids: Vec<u32>,
    /// The virtual display's size, where the backend makes one (Linux).
    #[serde(default)]
    pub display_size: Option<Size>,
    /// Start the app on a phone, tablet, simulator or emulator instead of
    /// this computer. `app` is then a bundle id (iOS) or a package or
    /// activity (Android).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceTarget>,
}

/// Which mobile device a session runs on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceTarget {
    pub kind: DeviceKind,
    /// A simulator's UDID or name, an AVD name or adb serial, or a phone's
    /// id, serial or name. `None` picks the only (or the first running) one.
    #[serde(default)]
    pub id: Option<String>,
    /// Show the simulator's or emulator's window when the session boots it.
    /// By default it boots headless, so nothing appears on screen.
    #[serde(default)]
    pub show_window: bool,
    /// Android: run the app on the device's own screen instead of a private
    /// virtual display. Needed for apps that refuse secondary displays.
    #[serde(default)]
    pub main_display: bool,
}

/// A question about the mobile devices a server can drive.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DeviceQuery {
    /// The simulators, emulators or phones there are, and their state.
    List { kind: DeviceKind },
    /// The apps on one of them.
    Apps {
        kind: DeviceKind,
        #[serde(default)]
        id: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    #[default]
    IosSimulator,
    AndroidEmulator,
    /// A physical phone or tablet: Android over adb, or an iPhone or iPad
    /// through WebDriverAgent (macOS).
    Phone,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// A rectangle in points. Window frames are in screen space; element frames
/// are relative to their window's top-left, the same space as screenshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub app: String,
    pub pid: Option<u32>,
    /// Which backend runs it: "macos", "linux-xvfb", "windows".
    pub backend: String,
    /// Backend-specific facts worth showing, such as the X display.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub frame: Rect,
    #[serde(default)]
    pub on_screen: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Screenshot {
    pub window_id: Option<u64>,
    pub width: u32,
    pub height: u32,
    /// PNG bytes; base64 on the wire.
    #[serde(with = "b64")]
    pub png: Vec<u8>,
}

mod b64 {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

impl Screenshot {
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.png)
    }
}

/// One node of an accessibility tree.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UiNode {
    /// Stable until the next tree is read; pass it to element actions.
    pub id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Rect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<UiNode>,
}

fn yes() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}

impl UiNode {
    /// The tree as indented lines, one element each: the compact form models
    /// read best.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out, 0);
        out
    }

    fn render_into(&self, out: &mut String, depth: usize) {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "{:indent$}[{}] {}",
            "",
            self.id,
            self.role,
            indent = depth * 2
        );
        if let Some(name) = self.name.as_deref().filter(|s| !s.is_empty()) {
            let _ = write!(out, " \"{}\"", clip(name, 80));
        }
        if let Some(value) = self.value.as_deref().filter(|s| !s.is_empty()) {
            let _ = write!(out, " value=\"{}\"", clip(value, 120));
        }
        if let Some(desc) = self.description.as_deref().filter(|s| !s.is_empty()) {
            if Some(desc) != self.name.as_deref() {
                let _ = write!(out, " desc=\"{}\"", clip(desc, 80));
            }
        }
        if let Some(f) = self.frame {
            let _ = write!(
                out,
                " @({:.0},{:.0} {:.0}x{:.0})",
                f.x, f.y, f.width, f.height
            );
        }
        if !self.actions.is_empty() {
            let _ = write!(out, " actions={}", self.actions.join(","));
        }
        if !self.enabled {
            out.push_str(" disabled");
        }
        if self.focused {
            out.push_str(" focused");
        }
        out.push('\n');
        for child in &self.children {
            child.render_into(out, depth + 1);
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    let s = s.replace('\n', "\\n").replace('"', "\\\"");
    if s.chars().count() <= max {
        s
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeOptions {
    #[serde(default = "default_depth")]
    pub max_depth: usize,
    #[serde(default = "default_nodes")]
    pub max_nodes: usize,
}

fn default_depth() -> usize {
    25
}
fn default_nodes() -> usize {
    1500
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self {
            max_depth: default_depth(),
            max_nodes: default_nodes(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

/// Something to do to a session's window. Coordinates are points relative
/// to the window's top-left: the pixel grid of its screenshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        button: MouseButton,
        #[serde(default = "one")]
        count: u32,
        /// Held while clicking, as in `press_key`: "cmd", "shift+alt".
        #[serde(default)]
        modifiers: Option<String>,
    },
    MoveMouse {
        x: f64,
        y: f64,
    },
    Drag {
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        #[serde(default)]
        button: MouseButton,
    },
    Scroll {
        x: f64,
        y: f64,
        /// Positive scrolls content right.
        #[serde(default)]
        dx: f64,
        /// Positive scrolls content down.
        #[serde(default)]
        dy: f64,
    },
    TypeText {
        text: String,
    },
    /// Key chords separated by spaces: "cmd+s", "ctrl+shift+tab enter".
    PressKey {
        keys: String,
    },
    /// An accessibility action on an element from the last tree read;
    /// "press" when none is named.
    ElementAction {
        element: String,
        #[serde(default)]
        name: Option<String>,
    },
    SetValue {
        element: String,
        value: String,
    },
    /// Gives an element keyboard focus, ready for typing.
    Focus {
        element: String,
    },
    Wait {
        ms: u64,
    },
}

fn one() -> u32 {
    1
}

impl Action {
    /// Where the pointer ends up, for the cursor overlay.
    pub fn pointer(&self) -> Option<(f64, f64)> {
        match *self {
            Action::Click { x, y, .. }
            | Action::MoveMouse { x, y }
            | Action::Scroll { x, y, .. } => Some((x, y)),
            Action::Drag { to_x, to_y, .. } => Some((to_x, to_y)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Permission {
    pub name: String,
    pub granted: bool,
    pub help: String,
}
