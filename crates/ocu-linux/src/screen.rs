//! A session on this computer's own X display: the whole screen, driven
//! through XTEST as if by a person at the keyboard. Unlike the other
//! sessions it moves the user's real pointer and types into whatever has
//! focus, which is what apps that read raw input (games) need.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::Window;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use ocu_core::image::{encode_png, Order};
use ocu_core::{
    pick_window, Action, CursorPosition, Description, Rect, Screenshot, Session, WindowInfo,
};

use crate::keys;
use crate::xvfb::{drive, grab, pointer, SharedKeymap};

pub struct ScreenSession {
    conn: RustConnection,
    root: Window,
    keymap: SharedKeymap,
    display: String,
}

impl ScreenSession {
    pub(crate) fn start() -> Result<Self> {
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") {
            bail!(
                "this is a Wayland desktop, where one program can't read the screen or send \
                 input to the others; screen sessions need an X11 desktop. Start the app in a \
                 session of its own instead"
            );
        }
        let display = std::env::var("DISPLAY")
            .ok()
            .filter(|d| !d.is_empty())
            .context("DISPLAY is not set, so there is no screen to drive")?;
        let (conn, screen) =
            x11rb::connect(Some(&display)).with_context(|| format!("connecting to {display}"))?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?
            .reply()
            .context("the X server has no XTEST")?;
        let keymap = Arc::new(Mutex::new(keys::Keymap::read(&conn)?));
        Ok(Self {
            conn,
            root,
            keymap,
            display,
        })
    }

    /// The screen, as the session's one window.
    fn screen(&self) -> WindowInfo {
        let s = self
            .conn
            .setup()
            .roots
            .iter()
            .find(|s| s.root == self.root)
            .expect("the root is one of the server's screens");
        WindowInfo {
            id: self.root as u64,
            title: format!("Screen {}", self.display),
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: s.width_in_pixels as f64,
                height: s.height_in_pixels as f64,
            },
            on_screen: true,
        }
    }

    fn window(&self, window: Option<u64>) -> Result<WindowInfo> {
        pick_window(&[self.screen()], window).cloned()
    }
}

impl Session for ScreenSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        details.insert("display".into(), self.display.clone());
        details.insert(
            "screen".into(),
            "the whole screen, with real input: it moves your pointer and types into what has focus"
                .into(),
        );
        Description {
            backend: Some("linux-screen".into()),
            app: "Screen".into(),
            pid: None,
            details,
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(vec![self.screen()])
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        let (width, height, data) = grab(&self.conn, self.root, &w)?;
        let stride = data.len() / height.max(1) as usize;
        Ok(Screenshot {
            window_id: Some(w.id),
            width,
            height,
            png: encode_png(width, height, stride, &data, Order::Bgra)?,
        })
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                Ok(())
            }
            Action::ElementAction { .. }
            | Action::ClickElement { .. }
            | Action::SetValue { .. }
            | Action::Focus { .. } => bail!(
                "a screen session has no accessibility tree; use coordinates, or start a session \
                 on the app"
            ),
            Action::ChooseFile { .. } => {
                bail!("a screen session can't answer file dialogs; click through them")
            }
            // Keys go wherever the desktop has put focus, as a person's do.
            action => drive(
                &self.conn,
                self.root,
                &self.keymap,
                &self.window(window)?,
                action,
                false,
            ),
        }
    }

    fn cursor(&mut self, window: Option<u64>) -> Result<Option<CursorPosition>> {
        let w = self.window(window)?;
        pointer(&self.conn, self.root, &w).map(Some)
    }

    fn is_alive(&mut self) -> bool {
        self.conn.flush().is_ok()
    }

    fn close(&mut self) {}
}
