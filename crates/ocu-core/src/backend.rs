//! The plugin surface. A platform backend implements [`Platform`] to start
//! apps and [`Session`] to drive one; everything else (session ids, owners,
//! observing after actions, the MCP and socket protocols) is shared.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::types::*;

pub trait Platform: Send + Sync {
    /// A short name for the backend: "macos", "linux-xvfb", "windows".
    fn name(&self) -> &'static str;

    /// The OS permissions the backend needs, and whether it has them.
    fn permissions(&self) -> Vec<Permission> {
        Vec::new()
    }

    /// Starts (or on macOS, possibly attaches to) an app, returning once it
    /// has a window or has had a fair chance to make one.
    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn Session>>;
}

/// What a session tells the registry about itself.
#[derive(Clone, Debug, Default)]
pub struct Description {
    pub app: String,
    pub pid: Option<u32>,
    pub details: BTreeMap<String, String>,
}

/// One app under control. Window arguments are `None` for "the app's main
/// window"; coordinates in actions are relative to the chosen window.
///
/// Dropping a session must release it as [`Session::close`] does, so a
/// registry that is dropped (its client gone) leaves nothing behind.
pub trait Session: Send {
    fn describe(&self) -> Description;

    fn windows(&mut self) -> Result<Vec<WindowInfo>>;

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot>;

    /// A screenshot that cannot be a stale frame. Some apps stop drawing a
    /// window while it is covered, so its capture shows the past; a backend
    /// may briefly uncover the window to get a fresh one. Asked for only when
    /// an action left the picture pixel-identical, since uncovering is seen.
    fn screenshot_uncovered(&mut self, window: Option<u64>) -> Result<Screenshot> {
        self.screenshot(window)
    }

    fn ui_tree(&mut self, _window: Option<u64>, _opts: &TreeOptions) -> Result<UiNode> {
        bail!("this backend has no accessibility tree")
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()>;

    fn is_alive(&mut self) -> bool;

    /// Ends the app if the session started it. Idempotent.
    fn close(&mut self);
}

/// Hooks for a UI that shows what sessions are doing. All calls come from
/// request threads, never the UI's own.
pub trait Observer: Send + Sync {
    fn session_started(&self, _info: &SessionInfo) {}
    fn session_ended(&self, _id: &str) {}
    /// The pointer moved to (`x`, `y`) in `window`; `click` when it pressed.
    fn pointer(&self, _session: &str, _window: &WindowInfo, _x: f64, _y: f64, _click: bool) {}
    /// Something without a position happened in `window` (typing, keys, an
    /// element action).
    fn acted(&self, _session: &str, _window: &WindowInfo) {}
}

/// Picks `window` from `windows`, or the first when `None`.
pub fn pick_window(windows: &[WindowInfo], window: Option<u64>) -> Result<&WindowInfo> {
    match window {
        Some(id) => windows
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| anyhow::anyhow!("the session has no window {id}; call list_windows")),
        // Backends list windows best first.
        None => windows
            .first()
            .ok_or_else(|| anyhow::anyhow!("the app has no windows (yet)")),
    }
}
