//! Sessions on a private X server (Xvfb) each: the app is started on it
//! through `DISPLAY`, so nothing it does touches the user's desktop. Input
//! goes in through XTEST and pictures come out of the server's framebuffer.
//!
//! Xvfb is looked for in `$OCU_XVFB`, next to this executable (a bundled
//! copy), in `../lib/opencomputeruse/`, then on `PATH`.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::fd::FromRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, ImageFormat, InputFocus, MapState, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use ocu_core::image::{encode_png, Order};
use ocu_core::keys::{parse_chord, parse_chords, Chord, Key, Modifiers, NamedKey};
use ocu_core::{
    pick_window, Action, Description, Frame, LaunchSpec, MouseButton, Rect, Screenshot, Session,
    Size, TreeOptions, UiNode, ViewInput, Viewer, WindowInfo,
};

use crate::atspi::A11y;
use crate::{die_with_parent, hex, keys, random_bytes};

/// An Xauthority file with one cookie for any display, so only processes
/// given the file can connect to the session's server.
fn write_xauthority(path: &Path, cookie: &[u8]) -> Result<()> {
    fn field(out: &mut Vec<u8>, data: &[u8]) {
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0xffffu16.to_be_bytes()); // FamilyWild
    field(&mut out, b"");
    field(&mut out, b""); // any display number
    field(&mut out, b"MIT-MAGIC-COOKIE-1");
    field(&mut out, cookie);
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut f, &out)?;
    Ok(())
}

struct Display {
    number: u32,
    server: Child,
    dir: PathBuf,
}

impl Display {
    fn start(xvfb: &Path, size: Size) -> Result<(Self, Vec<u8>)> {
        let dir = std::env::temp_dir().join(format!(
            "ocu-{}-{}",
            std::process::id(),
            hex(&random_bytes(4)?)
        ));
        std::fs::create_dir(&dir)?;
        let cookie = random_bytes(16)?;
        let auth = dir.join("Xauthority");
        write_xauthority(&auth, &cookie)?;

        // Xvfb picks a free display itself and writes its number to the fd.
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            bail!("pipe: {}", std::io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        let mut cmd = Command::new(xvfb);
        cmd.arg("-displayfd")
            .arg(write_fd.to_string())
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}x24", size.width, size.height))
            .args(["-nolisten", "tcp", "-noreset", "-auth"])
            .arg(&auth)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        die_with_parent(&mut cmd);
        let spawned = cmd.spawn();
        unsafe { libc::close(write_fd) };
        let mut server = match spawned {
            Ok(c) => c,
            Err(e) => {
                unsafe { libc::close(read_fd) };
                return Err(e).context("starting Xvfb");
            }
        };
        let mut pipe = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut out = String::new();
        // Xvfb closes the pipe once the number is written (or on failure).
        let read = pipe.read_to_string(&mut out);
        let number: u32 = match read.ok().and_then(|_| out.trim().parse().ok()) {
            Some(n) => n,
            None => {
                let _ = server.kill();
                bail!("Xvfb did not start (it reported \"{}\")", out.trim());
            }
        };
        Ok((
            Self {
                number,
                server,
                dir,
            },
            cookie,
        ))
    }

    fn name(&self) -> String {
        format!(":{}", self.number)
    }

    fn auth_file(&self) -> PathBuf {
        self.dir.join("Xauthority")
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn connect(number: u32, cookie: &[u8]) -> Result<(RustConnection, Window)> {
    let path = format!("/tmp/.X11-unix/X{number}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let unix = loop {
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)),
            Err(e) => return Err(e).with_context(|| format!("connecting to {path}")),
        }
    };
    let (stream, _) = DefaultStream::from_unix_stream(unix)?;
    let conn = RustConnection::connect_to_stream_with_auth_info(
        stream,
        0,
        b"MIT-MAGIC-COOKIE-1".to_vec(),
        cookie.to_vec(),
    )
    .context("X handshake")?;
    let root = conn.setup().roots[0].root;
    conn.xtest_get_version(2, 2)?
        .reply()
        .context("the X server has no XTEST")?;
    Ok((conn, root))
}

/// The keymap, shared by the session and its viewer: both lend spare
/// keycodes to characters the layout lacks.
type SharedKeymap = Arc<Mutex<keys::Keymap>>;

pub struct LinuxSession {
    app: Child,
    name: String,
    conn: RustConnection,
    root: Window,
    keymap: SharedKeymap,
    cookie: Vec<u8>,
    /// The session's private bus and the file chooser portal on it. Dropping
    /// the bus ends its dbus-daemon, and with it the portal's bridge.
    portal: crate::Portal,
    a11y: Option<A11y>,
    viewer: Option<Arc<XView>>,
    alive: Arc<AtomicBool>,
    // Dropped last: the display outlives the connection and the app.
    display: Display,
    closed: bool,
}

impl LinuxSession {
    pub(crate) fn start(xvfb: &Path, spec: &LaunchSpec, portal: bool) -> Result<Self> {
        let size = spec.display_size.unwrap_or(Size {
            width: 1440,
            height: 900,
        });
        let (display, cookie) = Display::start(xvfb, size)?;
        let (conn, root) = connect(display.number, &cookie)?;
        let keymap = Arc::new(Mutex::new(keys::Keymap::read(&conn)?));

        // Started before the app so it finds the portal. Off, or without
        // dbus-daemon, the app keeps the user's bus and draws its own dialogs.
        let portal = if portal { crate::start_portal() } else { None };
        let a11y = crate::start_a11y();

        let mut cmd = Command::new(&spec.app);
        let mut args = spec.args.clone();
        crate::use_a11y(&mut cmd, &a11y, &spec.app, &mut args);
        cmd.args(&args)
            .env("DISPLAY", display.name())
            .env("XAUTHORITY", display.auth_file())
            // Toolkits that prefer Wayland must use the X display instead.
            .env_remove("WAYLAND_DISPLAY")
            .env("GDK_BACKEND", "x11")
            .env("QT_QPA_PLATFORM", "xcb")
            .env("SDL_VIDEODRIVER", "x11")
            .env("ELECTRON_OZONE_PLATFORM_HINT", "x11");
        crate::use_portal(&mut cmd, &portal);
        cmd.envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        die_with_parent(&mut cmd);
        let app = cmd
            .spawn()
            .with_context(|| format!("starting {}", spec.app))?;
        let mut session = Self {
            app,
            name: crate::app_name(&spec.app),
            conn,
            root,
            keymap,
            cookie,
            portal,
            a11y,
            viewer: None,
            alive: Arc::new(AtomicBool::new(true)),
            display,
            closed: false,
        };

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && session.is_alive() {
            if session.windows().map(|w| !w.is_empty()).unwrap_or(false) {
                break;
            }
            sleep(Duration::from_millis(150));
        }
        sleep(Duration::from_millis(300));
        Ok(session)
    }

    fn window(&mut self, window: Option<u64>) -> Result<WindowInfo> {
        let windows = self.windows()?;
        pick_window(&windows, window).cloned()
    }

    fn move_to(&self, w: &WindowInfo, x: f64, y: f64) -> Result<()> {
        move_to(&self.conn, self.root, w, x, y)
    }

    fn button(&self, b: u8, down: bool) -> Result<()> {
        button(&self.conn, self.root, b, down)
    }

    fn key(&self, code: u8, down: bool) -> Result<()> {
        key(&self.conn, self.root, code, down)
    }

    fn modifier_codes(&self, mods: Modifiers) -> Result<Vec<u8>> {
        let mut keymap = self.keymap.lock().unwrap();
        keys::modifier_keysyms(mods)
            .into_iter()
            .map(|ks| keymap.code_for(&self.conn, ks).map(|(c, _)| c))
            .collect()
    }
}

fn title(conn: &RustConnection, w: Window) -> String {
    let atom = |name: &[u8]| {
        conn.intern_atom(false, name)
            .ok()?
            .reply()
            .ok()
            .map(|r| r.atom)
    };
    if let (Some(net_name), Some(utf8)) = (atom(b"_NET_WM_NAME"), atom(b"UTF8_STRING")) {
        if let Ok(Ok(r)) = conn
            .get_property(false, w, net_name, utf8, 0, 1024)
            .map(|c| c.reply())
        {
            if !r.value.is_empty() {
                return String::from_utf8_lossy(&r.value).into_owned();
            }
        }
    }
    conn.get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
        .unwrap_or_default()
}

/// Mapped top-level windows, topmost first; popups and menus
/// (override-redirect) after real windows.
fn list(conn: &RustConnection, root: Window) -> Result<Vec<WindowInfo>> {
    let tree = conn.query_tree(root)?.reply()?;
    let mut out = Vec::new();
    for &w in tree.children.iter().rev() {
        let Ok(attrs) = conn.get_window_attributes(w)?.reply() else {
            continue;
        };
        if attrs.map_state != MapState::VIEWABLE || attrs.class == xproto::WindowClass::INPUT_ONLY {
            continue;
        }
        let Ok(g) = conn.get_geometry(w)?.reply() else {
            continue;
        };
        if g.width < 2 || g.height < 2 {
            continue;
        }
        let info = WindowInfo {
            id: w as u64,
            title: title(conn, w),
            frame: Rect {
                x: g.x as f64,
                y: g.y as f64,
                width: g.width as f64,
                height: g.height as f64,
            },
            on_screen: true,
        };
        out.push((info, attrs.override_redirect));
    }
    out.sort_by_key(|(_, popup)| *popup);
    Ok(out.into_iter().map(|(w, _)| w).collect())
}

/// A window's part of the framebuffer, as (width, height, stride, BGRA).
/// Read from the root, not the window, so popups and menus over it are in
/// the picture. Clipped to the screen.
fn grab(conn: &RustConnection, root: Window, w: &WindowInfo) -> Result<(u32, u32, Vec<u8>)> {
    let screen = &conn.setup().roots[0];
    let x = w.frame.x.max(0.0) as i16;
    let y = w.frame.y.max(0.0) as i16;
    let width = (w.frame.width as u16).min(screen.width_in_pixels.saturating_sub(x as u16));
    let height = (w.frame.height as u16).min(screen.height_in_pixels.saturating_sub(y as u16));
    let img = conn
        .get_image(ImageFormat::Z_PIXMAP, root, x, y, width, height, !0)?
        .reply()
        .context("reading the framebuffer")?;
    if img.depth != 24 && img.depth != 32 {
        bail!("unsupported X depth {}", img.depth);
    }
    Ok((width as u32, height as u32, img.data))
}

fn fake(conn: &RustConnection, root: Window, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
    conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, root, x, y, 0)?;
    Ok(())
}

fn move_to(conn: &RustConnection, root: Window, w: &WindowInfo, x: f64, y: f64) -> Result<()> {
    let (gx, gy) = (
        (w.frame.x + x).round() as i16,
        (w.frame.y + y).round() as i16,
    );
    fake(conn, root, xproto::MOTION_NOTIFY_EVENT, 0, gx, gy)?;
    conn.flush()?;
    Ok(())
}

fn button(conn: &RustConnection, root: Window, b: u8, down: bool) -> Result<()> {
    let kind = if down {
        xproto::BUTTON_PRESS_EVENT
    } else {
        xproto::BUTTON_RELEASE_EVENT
    };
    fake(conn, root, kind, b, 0, 0)
}

fn key(conn: &RustConnection, root: Window, code: u8, down: bool) -> Result<()> {
    let kind = if down {
        xproto::KEY_PRESS_EVENT
    } else {
        xproto::KEY_RELEASE_EVENT
    };
    fake(conn, root, kind, code, 0, 0)
}

/// Gives `w` the keyboard. There is no window manager to do it.
fn focus(conn: &RustConnection, w: &WindowInfo) -> Result<()> {
    conn.set_input_focus(
        InputFocus::POINTER_ROOT,
        w.id as Window,
        x11rb::CURRENT_TIME,
    )?;
    Ok(())
}

/// Wheel buttons: 4 up, 5 down, 6 left, 7 right; ~40px a notch.
fn wheel(conn: &RustConnection, root: Window, dx: f64, dy: f64) -> Result<()> {
    let notches = |d: f64| ((d.abs() / 40.0).round() as u32).max(if d != 0.0 { 1 } else { 0 });
    let (vb, hb) = (if dy > 0.0 { 5 } else { 4 }, if dx > 0.0 { 7 } else { 6 });
    for _ in 0..notches(dy) {
        button(conn, root, vb, true)?;
        button(conn, root, vb, false)?;
    }
    for _ in 0..notches(dx) {
        button(conn, root, hb, true)?;
        button(conn, root, hb, false)?;
    }
    conn.flush()?;
    Ok(())
}

fn press_chord(
    conn: &RustConnection,
    root: Window,
    keymap: &SharedKeymap,
    chord: &Chord,
) -> Result<()> {
    let mut keymap = keymap.lock().unwrap();
    let mut mods = chord.modifiers;
    let code = match chord.key {
        None => None,
        Some(k) => {
            let (code, shift) = keymap.code_for(conn, keys::keysym(k))?;
            mods.shift |= shift;
            Some(code)
        }
    };
    let mod_codes: Vec<u8> = keys::modifier_keysyms(mods)
        .into_iter()
        .map(|ks| keymap.code_for(conn, ks).map(|(c, _)| c))
        .collect::<Result<_>>()?;
    drop(keymap);
    for &m in &mod_codes {
        key(conn, root, m, true)?;
    }
    if let Some(code) = code {
        key(conn, root, code, true)?;
        key(conn, root, code, false)?;
    }
    for &m in mod_codes.iter().rev() {
        key(conn, root, m, false)?;
    }
    conn.flush()?;
    Ok(())
}

fn type_text(conn: &RustConnection, root: Window, keymap: &SharedKeymap, text: &str) -> Result<()> {
    for c in text.chars() {
        let k = match c {
            '\n' | '\r' => Key::Named(NamedKey::Enter),
            '\t' => Key::Named(NamedKey::Tab),
            c => Key::Char(c),
        };
        // Characters keep their case: keysym() maps 'A' to its own keysym,
        // which the keymap finds on the shifted level.
        press_chord(
            conn,
            root,
            keymap,
            &Chord {
                modifiers: Modifiers::default(),
                key: Some(k),
            },
        )?;
        sleep(Duration::from_millis(3));
    }
    Ok(())
}

fn x_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    }
}

impl Session for LinuxSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        details.insert("display".into(), self.display.name());
        details.insert(
            "xauthority".into(),
            self.display.auth_file().display().to_string(),
        );
        details.insert("files".into(), crate::portal_detail(&self.portal));
        details.insert("accessibility".into(), crate::a11y_detail(&self.a11y));
        Description {
            app: self.name.clone(),
            pid: Some(self.app.id()),
            details,
            ..Default::default()
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        list(&self.conn, self.root)
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        let (width, height, data) = grab(&self.conn, self.root, &w)?;
        let stride = data.len() / height.max(1) as usize;
        let png = encode_png(width, height, stride, &data, Order::Bgra)?;
        Ok(Screenshot {
            window_id: Some(w.id),
            width,
            height,
            png,
        })
    }

    fn ui_tree(&mut self, window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        let w = self.window(window)?;
        crate::a11y(&mut self.a11y)?.tree(&w, opts)
    }

    fn element_point(
        &mut self,
        _window: Option<u64>,
        element: &str,
    ) -> Result<(Option<u64>, f64, f64)> {
        let (w, x, y) = crate::a11y(&mut self.a11y)?.center(element)?;
        Ok((Some(w), x, y))
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                return Ok(());
            }
            Action::ElementAction { .. } | Action::SetValue { .. } | Action::Focus { .. } => {
                if let Some(then) = crate::a11y(&mut self.a11y)?.perform(action)? {
                    for a in &then.actions {
                        self.perform(Some(then.window), a)?;
                        sleep(Duration::from_millis(40));
                    }
                }
                return Ok(());
            }
            Action::ChooseFile { paths } => return crate::choose_file(&self.portal, paths),
            _ => {}
        }
        let w = self.window(window)?;
        match action {
            Action::Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => {
                let mods = match modifiers.as_deref().filter(|s| !s.is_empty()) {
                    Some(s) => parse_chord(s)?.modifiers,
                    None => Modifiers::default(),
                };
                self.move_to(&w, *x, *y)?;
                let mod_codes = self.modifier_codes(mods)?;
                for &m in &mod_codes {
                    self.key(m, true)?;
                }
                for _ in 0..(*count).max(1) {
                    self.button(x_button(*button), true)?;
                    self.button(x_button(*button), false)?;
                    self.conn.flush()?;
                    sleep(Duration::from_millis(40));
                }
                for &m in mod_codes.iter().rev() {
                    self.key(m, false)?;
                }
                self.conn.flush()?;
            }
            Action::MoveMouse { x, y } => self.move_to(&w, *x, *y)?,
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                modifiers,
            } => {
                let mods = match modifiers.as_deref().filter(|s| !s.is_empty()) {
                    Some(s) => parse_chord(s)?.modifiers,
                    None => Modifiers::default(),
                };
                let mod_codes = self.modifier_codes(mods)?;
                self.move_to(&w, *from_x, *from_y)?;
                for &m in &mod_codes {
                    self.key(m, true)?;
                }
                self.button(x_button(*button), true)?;
                self.conn.flush()?;
                for i in 1..=12 {
                    let t = i as f64 / 12.0;
                    sleep(Duration::from_millis(12));
                    self.move_to(
                        &w,
                        from_x + (to_x - from_x) * t,
                        from_y + (to_y - from_y) * t,
                    )?;
                }
                self.button(x_button(*button), false)?;
                for &m in mod_codes.iter().rev() {
                    self.key(m, false)?;
                }
                self.conn.flush()?;
            }
            Action::Scroll { x, y, dx, dy } => {
                self.move_to(&w, *x, *y)?;
                wheel(&self.conn, self.root, *dx, *dy)?;
            }
            Action::TypeText { text } => {
                focus(&self.conn, &w)?;
                type_text(&self.conn, self.root, &self.keymap, text)?;
            }
            Action::PressKey { keys } => {
                focus(&self.conn, &w)?;
                for chord in parse_chords(keys)? {
                    press_chord(&self.conn, self.root, &self.keymap, &chord)?;
                    sleep(Duration::from_millis(20));
                }
            }
            Action::ElementAction { .. }
            | Action::ClickElement { .. }
            | Action::SetValue { .. }
            | Action::Focus { .. }
            | Action::Wait { .. }
            | Action::ChooseFile { .. } => {
                unreachable!()
            }
        }
        Ok(())
    }

    fn notice(&mut self) -> Option<String> {
        crate::portal_notice(&self.portal)
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.app.try_wait(), Ok(None))
    }

    fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        self.alive.store(false, Ordering::Relaxed);
        crate::end_app(&mut self.app);
        // The display and the bus go when the session is dropped.
    }

    fn viewer(&mut self) -> Option<Arc<dyn Viewer>> {
        if self.viewer.is_none() {
            // A connection of its own, so watching never waits on the
            // session's lock.
            match connect(self.display.number, &self.cookie) {
                Ok((conn, root)) => {
                    self.viewer = Some(Arc::new(XView {
                        conn,
                        root,
                        keymap: self.keymap.clone(),
                        alive: self.alive.clone(),
                    }))
                }
                Err(e) => log::warn!("no live view of {}: {e:#}", self.name),
            }
        }
        self.viewer.clone().map(|v| v as Arc<dyn Viewer>)
    }
}

impl Drop for LinuxSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// A person's view into an X session, on its own connection.
struct XView {
    conn: RustConnection,
    root: Window,
    keymap: SharedKeymap,
    alive: Arc<AtomicBool>,
}

impl XView {
    fn window(&self, window: Option<u64>) -> Result<WindowInfo> {
        pick_window(&list(&self.conn, self.root)?, window).cloned()
    }
}

impl Viewer for XView {
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        list(&self.conn, self.root)
    }

    fn frame(&self, window: Option<u64>) -> Result<Frame> {
        let w = self.window(window)?;
        let (width, height, data) = grab(&self.conn, self.root, &w)?;
        Ok(Frame {
            window_id: w.id,
            width,
            height,
            stride: data.len() / height.max(1) as usize,
            data,
            order: Order::Bgra,
        })
    }

    fn input(&self, window: Option<u64>, input: &ViewInput) -> Result<()> {
        let (conn, root) = (&self.conn, self.root);
        let w = self.window(window)?;
        match input {
            ViewInput::Move { x, y } => move_to(conn, root, &w, *x, *y)?,
            ViewInput::Button {
                x,
                y,
                button: b,
                down,
            } => {
                move_to(conn, root, &w, *x, *y)?;
                if *down {
                    focus(conn, &w)?;
                }
                button(conn, root, x_button(*b), *down)?;
            }
            ViewInput::Wheel { x, y, dx, dy } => {
                move_to(conn, root, &w, *x, *y)?;
                wheel(conn, root, *dx, *dy)?;
            }
            ViewInput::Key { code, down } => {
                let Some(k) = ocu_core::keys::dom_code(code) else {
                    bail!("unknown key code \"{code}\"");
                };
                let (code, _) = self.keymap.lock().unwrap().code_for(conn, k.keysym)?;
                key(conn, root, code, *down)?;
            }
            ViewInput::Text { text } => type_text(conn, root, &self.keymap, text)?,
        }
        conn.flush()?;
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}
