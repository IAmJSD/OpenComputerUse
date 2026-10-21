//! A session on this computer's own desktop: the whole screen, driven as if
//! by a person at the keyboard. Unlike the other sessions it moves the
//! user's real pointer and types into whatever has focus, which is what
//! apps that read raw input (games) need.
//!
//! An X11 desktop is driven through XTEST on `DISPLAY`. A Wayland desktop
//! lets no program do that, so it goes through the desktop portal
//! ([`PortalDesktop`]), which asks the user once.
//!
//! The tree is the focused window's, read from the desktop's accessibility
//! bus. On X11 its frames are placed on the screen; Wayland never says
//! where a window is, so there they are left out.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, MapState, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use ocu_core::image::{decode_png, encode_png, resize, Order, Rgba};
use ocu_core::keys::{
    modifier_evdev, named_evdev, parse_chord, parse_chords, parse_held, Chord, Held, Key,
    Modifiers, NamedKey,
};
use ocu_core::{
    pick_window, Action, CursorPosition, Description, Rect, Screenshot, Session, TreeOptions,
    UiNode, WindowInfo,
};

use crate::atspi::A11y;
use crate::keys;
use crate::portal_desktop::{Monitor, PortalDesktop};
use crate::xvfb::{button, drive, grab, hold_chord, pointer, title, x_button, SharedKeymap};

/// Whether this is a Wayland desktop session.
pub(crate) fn on_wayland() -> bool {
    match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => true,
        Ok("x11") => false,
        _ => std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none(),
    }
}

enum Desktop {
    X11 {
        conn: Box<RustConnection>,
        root: Window,
        keymap: SharedKeymap,
        display: String,
    },
    Wayland(PortalDesktop),
}

pub struct ScreenSession {
    desktop: Desktop,
    /// The desktop's accessibility bus, joined at the first tree read.
    a11y: Option<A11y>,
    /// What `key_down` and `mouse_down` hold on the user's real keyboard and
    /// pointer, let go when the session ends.
    held: Held,
    closed: bool,
}

impl ScreenSession {
    pub(crate) fn start() -> Result<Self> {
        let desktop = if on_wayland() {
            Desktop::Wayland(PortalDesktop::start().context(
                "driving a Wayland desktop through its portal (RemoteDesktop and Screenshot, which \
                 GNOME and KDE Plasma provide)",
            )?)
        } else {
            let display = std::env::var("DISPLAY")
                .ok()
                .filter(|d| !d.is_empty())
                .context("DISPLAY is not set, so there is no screen to drive")?;
            let (conn, screen) = x11rb::connect(Some(&display))
                .with_context(|| format!("connecting to {display}"))?;
            let root = conn.setup().roots[screen].root;
            conn.xtest_get_version(2, 2)?
                .reply()
                .context("the X server has no XTEST")?;
            let keymap = Arc::new(Mutex::new(keys::Keymap::read(&conn)?));
            Desktop::X11 {
                conn: Box::new(conn),
                root,
                keymap,
                display,
            }
        };
        Ok(Self::on(desktop))
    }

    fn on(desktop: Desktop) -> Self {
        Self {
            desktop,
            a11y: None,
            held: Held::default(),
            closed: false,
        }
    }

    /// The displays, as the session's windows.
    fn screens(&self) -> Vec<WindowInfo> {
        match &self.desktop {
            Desktop::X11 {
                conn,
                root,
                display,
                ..
            } => {
                let s = conn
                    .setup()
                    .roots
                    .iter()
                    .find(|s| s.root == *root)
                    .expect("the root is one of the server's screens");
                vec![WindowInfo {
                    id: *root as u64,
                    title: format!("Screen {display}"),
                    frame: Rect {
                        x: 0.0,
                        y: 0.0,
                        width: s.width_in_pixels as f64,
                        height: s.height_in_pixels as f64,
                    },
                    on_screen: true,
                }]
            }
            Desktop::Wayland(p) => p
                .monitors()
                .iter()
                .enumerate()
                .map(|(i, m)| WindowInfo {
                    id: m.stream as u64,
                    title: format!("Monitor {}", i + 1),
                    frame: Rect {
                        x: m.x as f64,
                        y: m.y as f64,
                        width: m.width as f64,
                        height: m.height as f64,
                    },
                    on_screen: true,
                })
                .collect(),
        }
    }

    fn window(&self, window: Option<u64>) -> Result<WindowInfo> {
        pick_window(&self.screens(), window).cloned()
    }

    fn a11y(&mut self) -> Result<&mut A11y> {
        if self.a11y.is_none() {
            self.a11y = Some(A11y::desktop().context(
                "reading the desktop's accessibility tree (is the accessibility bus, at-spi2-core, \
                 running?)",
            )?);
        }
        Ok(self.a11y.as_mut().unwrap())
    }

    /// The window with focus on an X11 desktop, as the window manager says.
    fn active_x11(conn: &RustConnection, root: Window) -> Option<Window> {
        let reply = conn
            .get_property(
                false,
                root,
                atom(conn, b"_NET_ACTIVE_WINDOW")?,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        let active = reply.value32()?.next();
        active.filter(|&w| w != 0)
    }
}

fn atom(conn: &RustConnection, name: &[u8]) -> Option<u32> {
    Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
}

/// An X11 desktop's app windows: the window manager's list, or with no
/// window manager the root's shown children.
fn app_windows_x11(conn: &RustConnection, root: Window) -> Vec<Window> {
    let listed = atom(conn, b"_NET_CLIENT_LIST")
        .and_then(|a| {
            conn.get_property(false, root, a, AtomEnum::WINDOW, 0, 4096)
                .ok()?
                .reply()
                .ok()
        })
        .and_then(|r| Some(r.value32()?.collect::<Vec<_>>()))
        .filter(|l| !l.is_empty());
    if let Some(list) = listed {
        return list;
    }
    let Ok(Ok(tree)) = conn.query_tree(root).map(|c| c.reply()) else {
        return Vec::new();
    };
    tree.children
        .into_iter()
        .filter(|&w| {
            conn.get_window_attributes(w)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|a| a.map_state == MapState::VIEWABLE && !a.override_redirect)
        })
        .collect()
}

/// Where the toplevel named `name`, `size` big, has its top-left on an X11
/// screen. The tree may come from another toplevel than the active one,
/// when no title matched, so its own window is looked for: the active
/// window when its title is that, else an app window titled so, else the
/// one app window of its size. A title can lag its accessible name (Chrome
/// changes the name first), which the size covers.
fn locate_x11(
    conn: &RustConnection,
    root: Window,
    active: Option<Window>,
    name: &str,
    size: Option<(f64, f64)>,
) -> std::result::Result<(f64, f64), String> {
    let name = name.trim();
    let titled = |w: Window| !name.is_empty() && title(conn, w).trim() == name;
    let apps = || app_windows_x11(conn, root);
    let sized = |w: Window| {
        let Some((width, height)) = size else {
            return false;
        };
        conn.get_geometry(w)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|g| {
                (g.width as f64 - width).abs() <= 2.0 && (g.height as f64 - height).abs() <= 2.0
            })
    };
    let found = active
        .filter(|&w| titled(w))
        .or_else(|| apps().into_iter().find(|&w| titled(w)))
        .or_else(|| {
            let mut same: Vec<Window> = apps().into_iter().filter(|&w| sized(w)).collect();
            (same.len() == 1).then(|| same.remove(0))
        })
        .ok_or_else(|| {
            if name.is_empty() {
                "the focused window has no name, nor a size only it has, to find it on the X \
                 display by"
                    .to_string()
            } else {
                format!("no window on the X display is titled \"{name}\", nor alone its size")
            }
        })?;
    let at = conn
        .translate_coordinates(found, root, 0, 0)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    Ok((at.dst_x as f64, at.dst_y as f64))
}

/// Input on a Wayland desktop, through the portal. Coordinates are the
/// monitor's.
fn drive_portal(p: &PortalDesktop, m: &Monitor, action: &Action) -> Result<()> {
    let mods = |s: &Option<String>| -> Result<Vec<u16>> {
        Ok(match s.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => modifier_evdev(parse_chord(s)?.modifiers),
            None => Vec::new(),
        })
    };
    let hold = |codes: &[u16], down: bool| -> Result<()> {
        if down {
            codes.iter().try_for_each(|&c| p.keycode(c, true))
        } else {
            codes.iter().rev().try_for_each(|&c| p.keycode(c, false))
        }
    };
    match action {
        Action::Click {
            x,
            y,
            button,
            count,
            modifiers,
        } => {
            let mods = mods(modifiers)?;
            p.motion(m, *x, *y)?;
            hold(&mods, true)?;
            for _ in 0..(*count).max(1) {
                p.button(*button, true)?;
                p.button(*button, false)?;
                sleep(Duration::from_millis(40));
            }
            hold(&mods, false)
        }
        Action::MoveMouse { x, y } => p.motion(m, *x, *y),
        Action::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            button,
            modifiers,
        } => {
            let mods = mods(modifiers)?;
            p.motion(m, *from_x, *from_y)?;
            hold(&mods, true)?;
            p.button(*button, true)?;
            for i in 1..=12 {
                let t = i as f64 / 12.0;
                sleep(Duration::from_millis(12));
                p.motion(
                    m,
                    from_x + (to_x - from_x) * t,
                    from_y + (to_y - from_y) * t,
                )?;
            }
            p.button(*button, false)?;
            hold(&mods, false)
        }
        Action::Scroll { x, y, dx, dy } => {
            p.motion(m, *x, *y)?;
            p.scroll(*dx, *dy)
        }
        Action::TypeText { text } => {
            for c in text.chars() {
                match c {
                    '\n' | '\r' => press_portal(p, &bare(Key::Named(NamedKey::Enter)))?,
                    '\t' => press_portal(p, &bare(Key::Named(NamedKey::Tab)))?,
                    // The layout's own key for it, whatever the layout.
                    c => {
                        let sym = keys::keysym(Key::Char(c));
                        p.keysym(sym, true)?;
                        p.keysym(sym, false)?;
                    }
                }
                sleep(Duration::from_millis(4));
            }
            Ok(())
        }
        Action::PressKey { keys } => {
            for chord in parse_chords(keys)? {
                press_portal(p, &chord)?;
                sleep(Duration::from_millis(20));
            }
            Ok(())
        }
        Action::MouseDown { x, y, button } | Action::MouseUp { x, y, button } => {
            p.motion(m, *x, *y)?;
            p.button(*button, matches!(action, Action::MouseDown { .. }))
        }
        Action::KeyDown { key } => hold_portal(p, &parse_held(key)?, true),
        Action::KeyUp { key } => hold_portal(p, &parse_held(key)?, false),
        other => bail!("{other:?} is not pointer or keyboard input"),
    }
}

/// Presses (`down`) or releases a chord through the portal: modifiers by
/// their keys, named keys by theirs, characters by keysym so the user's
/// layout picks the key.
fn hold_portal(p: &PortalDesktop, chord: &Chord, down: bool) -> Result<()> {
    let mods = modifier_evdev(chord.modifiers);
    let key = |down| match chord.key {
        Some(Key::Named(k)) => p.keycode(named_evdev(k), down),
        Some(k @ Key::Char(_)) => p.keysym(keys::keysym(k), down),
        None => Ok(()),
    };
    if down {
        mods.iter().try_for_each(|&c| p.keycode(c, true))?;
        key(true)
    } else {
        key(false)?;
        mods.iter().rev().try_for_each(|&c| p.keycode(c, false))
    }
}

fn press_portal(p: &PortalDesktop, chord: &Chord) -> Result<()> {
    hold_portal(p, chord, true)?;
    hold_portal(p, chord, false)
}

/// A key on its own, as a chord.
fn bare(key: Key) -> Chord {
    Chord {
        modifiers: Modifiers::default(),
        key: Some(key),
    }
}

/// Monitor `m`'s part of a picture of the whole desktop, in the desktop's
/// logical pixels. The picture is in the screen's own pixels, which on a
/// scaled desktop are more.
fn crop_monitor(desktop: &Rgba, all: &[Monitor], m: &Monitor) -> Rgba {
    let left = all.iter().map(|m| m.x).min().unwrap_or(0);
    let top = all.iter().map(|m| m.y).min().unwrap_or(0);
    let right = all.iter().map(|m| m.x + m.width).max().unwrap_or(1);
    let bottom = all.iter().map(|m| m.y + m.height).max().unwrap_or(1);
    // Each axis on its own: the picture need not keep the layout's shape.
    let (sx, sy) = (
        desktop.width as f64 / (right - left).max(1) as f64,
        desktop.height as f64 / (bottom - top).max(1) as f64,
    );
    let px = |v: i32, origin: i32, scale: f64, size: u32| {
        (((v - origin) as f64 * scale).round().max(0.0) as u32).min(size)
    };
    let (dw, dh) = (desktop.width.max(1), desktop.height.max(1));
    // At least one pixel, inside the picture.
    let x0 = px(m.x, left, sx, dw - 1);
    let y0 = px(m.y, top, sy, dh - 1);
    let x1 = px(m.x + m.width, left, sx, dw).max(x0 + 1);
    let y1 = px(m.y + m.height, top, sy, dh).max(y0 + 1);
    let (w, h) = (x1 - x0, y1 - y0);
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for row in y0..y1 {
        let start = ((row * desktop.width + x0) * 4) as usize;
        data.extend_from_slice(&desktop.data[start..start + (w * 4) as usize]);
    }
    let cropped = Rgba {
        width: w,
        height: h,
        data,
    };
    resize(&cropped, m.width.max(1) as u32, m.height.max(1) as u32)
}

impl Session for ScreenSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        match &self.desktop {
            Desktop::X11 { display, .. } => {
                details.insert("display".into(), display.clone());
            }
            Desktop::Wayland(_) => {
                details.insert(
                    "display".into(),
                    "wayland, through the desktop portal (RemoteDesktop and Screenshot)".into(),
                );
            }
        }
        details.insert(
            "screen".into(),
            "the whole screen, with real input: it moves your pointer and types into what has focus"
                .into(),
        );
        details.insert(
            "accessibility".into(),
            match &self.desktop {
                Desktop::X11 { .. } => "the focused window's, from the desktop's AT-SPI bus",
                Desktop::Wayland(_) => {
                    "the focused window's, from the desktop's AT-SPI bus, without positions: \
                     Wayland doesn't say where windows are"
                }
            }
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
        Ok(self.screens())
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        match &self.desktop {
            Desktop::X11 { conn, root, .. } => {
                let (width, height, data) = grab(conn, *root, &w)?;
                let stride = data.len() / height.max(1) as usize;
                Ok(Screenshot {
                    window_id: Some(w.id),
                    width,
                    height,
                    png: encode_png(width, height, stride, &data, Order::Bgra)?,
                })
            }
            Desktop::Wayland(p) => {
                let desktop = decode_png(&p.screenshot()?)?;
                let m = p
                    .monitors()
                    .iter()
                    .find(|m| m.stream as u64 == w.id)
                    .ok_or_else(|| anyhow!("no monitor {}", w.id))?;
                let pic = crop_monitor(&desktop, p.monitors(), m);
                Ok(Screenshot {
                    window_id: Some(w.id),
                    width: pic.width,
                    height: pic.height,
                    png: encode_png(
                        pic.width,
                        pic.height,
                        pic.width as usize * 4,
                        &pic.data,
                        Order::Rgba,
                    )?,
                })
            }
        }
    }

    /// The focused window's tree, in `window`'s (a display's) coordinates.
    fn ui_tree(&mut self, window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        let w = self.window(window)?;
        self.a11y()?;
        // The fields apart, to read the display while the bus is in use.
        let a11y = self.a11y.as_mut().expect("joined above");
        match &self.desktop {
            Desktop::X11 { conn, root, .. } => {
                let active = Self::active_x11(conn, *root);
                let wanted = active.map(|a| title(conn, a)).unwrap_or_default();
                let origin = (w.frame.x, w.frame.y);
                a11y.tree_at(w.id, &wanted, opts, &|name, size| {
                    let (x, y) = locate_x11(conn, *root, active, name, size)?;
                    Ok((x - origin.0, y - origin.1))
                })
            }
            Desktop::Wayland(_) => a11y.tree_at(w.id, "", opts, &|_, _| {
                Err("Wayland doesn't tell anyone where a window is".into())
            }),
        }
    }

    fn element_point(
        &mut self,
        _window: Option<u64>,
        element: &str,
    ) -> Result<(Option<u64>, f64, f64)> {
        let (w, x, y) = self.a11y()?.center(element)?;
        Ok((Some(w), x, y))
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                return Ok(());
            }
            Action::ElementAction { .. } | Action::SetValue { .. } | Action::Focus { .. } => {
                if let Some(then) = self.a11y()?.perform(action)? {
                    for a in &then.actions {
                        self.perform(Some(then.window), a)?;
                        sleep(Duration::from_millis(40));
                    }
                }
                return Ok(());
            }
            Action::ClickElement { .. } => bail!("click elements by their point"),
            Action::ChooseFile { .. } => {
                bail!("a screen session can't answer file dialogs; click through them")
            }
            _ => {}
        }
        let w = self.window(window)?;
        match &self.desktop {
            // Keys go wherever the desktop has put focus, as a person's do.
            Desktop::X11 {
                conn, root, keymap, ..
            } => {
                // The user may have switched layouts since the last action.
                keymap.lock().unwrap().refresh(conn)?;
                drive(conn, *root, keymap, &w, action, false)?;
            }
            Desktop::Wayland(p) => {
                let m = p
                    .monitors()
                    .iter()
                    .find(|m| m.stream as u64 == w.id)
                    .ok_or_else(|| anyhow!("no monitor {}", w.id))?
                    .clone();
                drive_portal(p, &m, action)?;
            }
        }
        match action {
            Action::KeyDown { key } => self.held.key_down(&parse_held(key)?),
            Action::KeyUp { key } => self.held.key_up(&parse_held(key)?),
            Action::MouseDown { button, .. } => self.held.button(*button, true),
            Action::MouseUp { button, .. } => self.held.button(*button, false),
            _ => {}
        }
        Ok(())
    }

    fn cursor(&mut self, window: Option<u64>) -> Result<Option<CursorPosition>> {
        let w = self.window(window)?;
        match &self.desktop {
            Desktop::X11 { conn, root, .. } => pointer(conn, *root, &w).map(Some),
            // The portal can move the pointer but not read it.
            Desktop::Wayland(_) => Ok(None),
        }
    }

    fn is_alive(&mut self) -> bool {
        match &self.desktop {
            Desktop::X11 { conn, .. } => conn.flush().is_ok(),
            Desktop::Wayland(_) => true,
        }
    }

    /// Lets go of whatever is still held, and puts the desktop's keyboard
    /// map and accessibility back as they were.
    fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        let keys = self.held.take_keys();
        let buttons = std::mem::take(&mut self.held.buttons);
        let released = match &self.desktop {
            Desktop::X11 {
                conn, root, keymap, ..
            } => (|| -> Result<()> {
                for chord in &keys {
                    hold_chord(conn, *root, keymap, chord, false)?;
                }
                for b in buttons {
                    button(conn, *root, x_button(b), false)?;
                }
                keymap.lock().unwrap().restore(conn)?;
                conn.flush()?;
                Ok(())
            })(),
            Desktop::Wayland(p) => (|| -> Result<()> {
                for chord in &keys {
                    hold_portal(p, chord, false)?;
                }
                for b in buttons {
                    p.button(b, false)?;
                }
                Ok(())
            })(),
        };
        if let Err(e) = released {
            log::warn!("letting go of the screen session's keys: {e:#}");
        }
        self.a11y = None;
    }
}

impl Drop for ScreenSession {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocu_core::MouseButton;

    fn monitor(stream: u32, x: i32, y: i32, width: i32, height: i32) -> Monitor {
        Monitor {
            stream,
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_monitor_is_cut_from_a_scaled_desktop() {
        // Two 100x50 monitors side by side, captured at twice their size.
        let all = [monitor(1, 0, 0, 100, 50), monitor(2, 100, 0, 100, 50)];
        let (w, h) = (400u32, 100u32);
        let mut data = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                // Left monitor red, right one blue.
                data.extend_from_slice(if x < 200 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        let desktop = Rgba {
            width: w,
            height: h,
            data,
        };
        let right = crop_monitor(&desktop, &all, &all[1]);
        assert_eq!((right.width, right.height), (100, 50));
        assert!(right.data.chunks(4).all(|p| p == [0, 0, 255, 255]));
        let left = crop_monitor(&desktop, &all, &all[0]);
        assert!(left.data.chunks(4).all(|p| p == [255, 0, 0, 255]));
    }

    #[test]
    fn a_wayland_session_lets_go_of_what_it_holds_when_it_ends() {
        let Some((running, desktop)) = crate::portal_desktop::fake::start() else {
            return;
        };
        let mut session = ScreenSession::on(Desktop::Wayland(desktop));
        let at = |button| Action::MouseDown {
            x: 5.0,
            y: 6.0,
            button,
        };
        session
            .perform(
                None,
                &Action::KeyDown {
                    key: "ctrl+w".into(),
                },
            )
            .unwrap();
        session.perform(None, &at(MouseButton::Left)).unwrap();
        // Let go of one key by hand: only the rest is let go at the end.
        session
            .perform(None, &Action::KeyUp { key: "w".into() })
            .unwrap();
        running.fake.log.lock().unwrap().clear();
        drop(session);
        assert_eq!(
            *running.fake.log.lock().unwrap(),
            vec!["keycode 29 0".to_string(), "button 0x110 0".into()],
            "ctrl and the left button are let go"
        );
    }

    #[test]
    fn a_picture_narrower_than_the_layout_is_cut_without_running_off_it() {
        // A monitor whose left edge lands on the picture's right edge, and
        // one beside it: neither may read past the pixels there are.
        let all = [monitor(1, 0, 0, 100, 50), monitor(2, 100, 0, 1, 50)];
        let desktop = Rgba {
            width: 3,
            height: 1,
            data: vec![9; 3 * 4],
        };
        for m in &all {
            let pic = crop_monitor(&desktop, &all, m);
            assert_eq!((pic.width, pic.height), (m.width as u32, m.height as u32));
        }
        let empty = Rgba {
            width: 1,
            height: 1,
            data: vec![0; 4],
        };
        crop_monitor(&empty, &all, &all[1]);
    }
}
