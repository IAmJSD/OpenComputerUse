//! The Linux backend. Every session gets a private X server (Xvfb) of its
//! own and the app is started on it through `DISPLAY`, so nothing it does
//! touches the user's desktop. Input goes in through XTEST and pictures
//! come out of the server's framebuffer.
//!
//! Xvfb is looked for in `$OCU_XVFB`, next to this executable (a bundled
//! copy), in `../lib/opencomputeruse/`, then on `PATH`.
//!
//! Everything started here dies with the MCP server: Xvfb and the app get
//! `PR_SET_PDEATHSIG`, and an app that outlives its group loses its display.
#![cfg(target_os = "linux")]

mod keys;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::fd::FromRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, ImageFormat, InputFocus, MapState, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use ocu_core::image::{encode_png, Order};
use ocu_core::keys::{parse_chord, parse_chords, Chord, Key, Modifiers, NamedKey};
use ocu_core::{
    pick_window, Action, Description, LaunchSpec, MouseButton, Platform, Rect, Screenshot, Session,
    Size, WindowInfo,
};

pub struct LinuxPlatform {
    xvfb: Option<PathBuf>,
    /// Whether sessions get a private bus with a file chooser portal.
    portal: bool,
}

impl LinuxPlatform {
    pub fn new() -> Self {
        Self::with_portal(false)
    }

    /// A platform whose sessions answer file choosers through a portal when
    /// `portal` is set. The agent passes the user's setting.
    pub fn with_portal(portal: bool) -> Self {
        Self {
            xvfb: find_xvfb(),
            portal,
        }
    }
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

fn find_xvfb() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCU_XVFB") {
        return Some(PathBuf::from(p));
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let mut candidates = vec![
        exe_dir.join("Xvfb"),
        exe_dir.join("../lib/opencomputeruse/Xvfb"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join("Xvfb")));
    }
    candidates.into_iter().find(|p| p.is_file())
}

impl Platform for LinuxPlatform {
    fn name(&self) -> &'static str {
        "linux-xvfb"
    }

    fn permissions(&self) -> Vec<ocu_core::Permission> {
        vec![
            ocu_core::Permission {
                name: "Xvfb".into(),
                granted: self.xvfb.is_some(),
                help: "Sessions run on a private virtual X display. Install Xvfb (Debian/Ubuntu: apt install xvfb; Fedora: dnf install xorg-x11-server-Xvfb) or set OCU_XVFB.".into(),
                optional: false,
            },
            ocu_core::Permission {
                name: "File chooser portal".into(),
                granted: self.portal,
                help: "Answers apps' open and save dialogs without showing them. Each app runs on \
                       a private session bus whose file chooser is the agent; everything else is \
                       forwarded to the user's own bus, so the keyring and notifications still work. \
                       Turn on with linux_file_portal in the config or OCU_LINUX_FILE_PORTAL=1."
                    .into(),
                optional: true,
            },
        ]
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn Session>> {
        if spec.active_window {
            bail!("each Linux session has its own virtual display, with no window in front to attach to; start the app instead");
        }
        let xvfb = self.xvfb.as_ref().ok_or_else(|| {
            anyhow!(
                "Xvfb is not installed; install it (apt install xvfb) or set OCU_XVFB to its path"
            )
        })?;
        Ok(Box::new(LinuxSession::start(xvfb, spec, self.portal)?))
    }
}

/// Dies with the process that started it, even on SIGKILL.
fn die_with_parent(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            // The parent may already be gone by the time the flag is set.
            if libc::getppid() == 1 {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn connect(display: &Display, cookie: &[u8]) -> Result<(RustConnection, Window)> {
    let path = format!("/tmp/.X11-unix/X{}", display.number);
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

/// A private session bus with the file chooser portal on it. The portal
/// forwards everything else to the user's bus, so the app keeps the keyring,
/// notifications and the rest.
fn start_portal() -> Option<(Arc<ocu_core::portal::Bus>, Arc<ocu_core::portal::Portal>)> {
    let bus = ocu_core::portal::Bus::start()
        .inspect_err(|e| log::warn!("no private session bus: {e}"))
        .ok()?;
    let upstream = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
    let portal = ocu_core::portal::Portal::start(bus.address(), upstream)
        .inspect_err(|e| log::warn!("the file chooser portal is not answering: {e}"))
        .ok()?;
    Some((bus, portal))
}

pub struct LinuxSession {
    app: Child,
    name: String,
    conn: RustConnection,
    root: Window,
    keymap: keys::Keymap,
    /// The session's private bus and the file chooser portal on it. Dropping
    /// the bus ends its dbus-daemon, and with it the portal's bridge.
    portal: Option<(Arc<ocu_core::portal::Bus>, Arc<ocu_core::portal::Portal>)>,
    // Dropped last: the display outlives the connection and the app.
    display: Display,
    closed: bool,
}

impl LinuxSession {
    fn start(xvfb: &Path, spec: &LaunchSpec, portal: bool) -> Result<Self> {
        let size = spec.display_size.unwrap_or(Size {
            width: 1440,
            height: 900,
        });
        let (display, cookie) = Display::start(xvfb, size)?;
        let (conn, root) = connect(&display, &cookie)?;
        let keymap = keys::Keymap::read(&conn)?;

        // Started before the app so it finds the portal. Off, or without
        // dbus-daemon, the app keeps the user's bus and draws its own dialogs.
        let portal = if portal { start_portal() } else { None };

        let mut cmd = Command::new(&spec.app);
        cmd.args(&spec.args)
            .env("DISPLAY", display.name())
            .env("XAUTHORITY", display.auth_file())
            // Toolkits that prefer Wayland must use the X display instead.
            .env_remove("WAYLAND_DISPLAY")
            .env("GDK_BACKEND", "x11")
            .env("QT_QPA_PLATFORM", "xcb")
            .env("SDL_VIDEODRIVER", "x11")
            .env("ELECTRON_OZONE_PLATFORM_HINT", "x11");
        if let Some((bus, _)) = &portal {
            // Steers GTK (Firefox included) and Qt to ask the portal rather
            // than draw a dialog.
            cmd.env("DBUS_SESSION_BUS_ADDRESS", bus.address())
                .env("GTK_USE_PORTAL", "1")
                .env("QT_QPA_PLATFORMTHEME", "xdgdesktopportal");
        }
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
        let name = Path::new(&spec.app)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut session = Self {
            app,
            name,
            conn,
            root,
            keymap,
            portal,
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

    /// Answers the file chooser the app is waiting on with `paths`; none
    /// cancels it.
    fn choose_file(&mut self, paths: &[String]) -> Result<()> {
        let paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| ocu_core::paths::absolute(p))
            .collect::<Result<_>>()?;
        let Some((_, portal)) = &self.portal else {
            bail!("this session has no file chooser portal (it is off, or dbus-daemon is missing); drive the app's own dialog with clicks and keys")
        };
        // A menu item's chooser comes a moment after the click.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(waiting) = portal.waiting() {
                return portal.answer(&waiting.token, &paths);
            }
            if Instant::now() > deadline {
                bail!(
                    "the app is not asking for a file: do what asks for one first (an upload \
                     button, File → Open), or use clicks and keys"
                );
            }
            sleep(Duration::from_millis(100));
        }
    }

    fn title(&self, w: Window) -> String {
        let atom = |name: &[u8]| {
            self.conn
                .intern_atom(false, name)
                .ok()?
                .reply()
                .ok()
                .map(|r| r.atom)
        };
        if let (Some(net_name), Some(utf8)) = (atom(b"_NET_WM_NAME"), atom(b"UTF8_STRING")) {
            if let Ok(Ok(r)) = self
                .conn
                .get_property(false, w, net_name, utf8, 0, 1024)
                .map(|c| c.reply())
            {
                if !r.value.is_empty() {
                    return String::from_utf8_lossy(&r.value).into_owned();
                }
            }
        }
        self.conn
            .get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.value).into_owned())
            .unwrap_or_default()
    }

    /// Mapped top-level windows, topmost first; popups and menus
    /// (override-redirect) after real windows.
    fn list(&self) -> Result<Vec<(WindowInfo, bool)>> {
        let tree = self.conn.query_tree(self.root)?.reply()?;
        let mut out = Vec::new();
        for &w in tree.children.iter().rev() {
            let Ok(attrs) = self.conn.get_window_attributes(w)?.reply() else {
                continue;
            };
            if attrs.map_state != MapState::VIEWABLE
                || attrs.class == xproto::WindowClass::INPUT_ONLY
            {
                continue;
            }
            let Ok(g) = self.conn.get_geometry(w)?.reply() else {
                continue;
            };
            if g.width < 2 || g.height < 2 {
                continue;
            }
            let info = WindowInfo {
                id: w as u64,
                title: self.title(w),
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
        Ok(out)
    }

    fn window(&mut self, window: Option<u64>) -> Result<WindowInfo> {
        let windows = self.windows()?;
        pick_window(&windows, window).cloned()
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        Ok(())
    }

    fn move_to(&self, w: &WindowInfo, x: f64, y: f64) -> Result<()> {
        let (gx, gy) = (
            (w.frame.x + x).round() as i16,
            (w.frame.y + y).round() as i16,
        );
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, gx, gy)?;
        self.conn.flush()?;
        Ok(())
    }

    fn button(&self, b: u8, down: bool) -> Result<()> {
        let kind = if down {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(kind, b, 0, 0)
    }

    fn key(&self, code: u8, down: bool) -> Result<()> {
        let kind = if down {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(kind, code, 0, 0)
    }

    /// Gives `w` the keyboard. There is no window manager to do it.
    fn focus(&self, w: &WindowInfo) -> Result<()> {
        self.conn.set_input_focus(
            InputFocus::POINTER_ROOT,
            w.id as Window,
            x11rb::CURRENT_TIME,
        )?;
        Ok(())
    }

    fn press_chord(&mut self, chord: &Chord) -> Result<()> {
        let mut mods = chord.modifiers;
        let code = match chord.key {
            None => None,
            Some(key) => {
                let (code, shift) = self.keymap.code_for(&self.conn, keys::keysym(key))?;
                mods.shift |= shift;
                Some(code)
            }
        };
        let mod_codes: Vec<u8> = keys::modifier_keysyms(mods)
            .into_iter()
            .map(|ks| self.keymap.code_for(&self.conn, ks).map(|(c, _)| c))
            .collect::<Result<_>>()?;
        for &m in &mod_codes {
            self.key(m, true)?;
        }
        if let Some(code) = code {
            self.key(code, true)?;
            self.key(code, false)?;
        }
        for &m in mod_codes.iter().rev() {
            self.key(m, false)?;
        }
        self.conn.flush()?;
        Ok(())
    }
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
        details.insert(
            "files".into(),
            match &self.portal {
                Some((bus, _)) => format!(
                    "answered through a portal on a private bus at {} (other services forwarded \
                     to the user's bus); nothing is shown",
                    bus.address()
                ),
                None => "the app draws its own dialogs".into(),
            },
        );
        Description {
            app: self.name.clone(),
            pid: Some(self.app.id()),
            details,
            ..Default::default()
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(self.list()?.into_iter().map(|(w, _)| w).collect())
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        let screen = &self.conn.setup().roots[0];
        // Read from the root, not the window, so popups and menus over it
        // are in the picture. Clip to the screen.
        let x = w.frame.x.max(0.0) as i16;
        let y = w.frame.y.max(0.0) as i16;
        let width = (w.frame.width as u16).min(screen.width_in_pixels.saturating_sub(x as u16));
        let height = (w.frame.height as u16).min(screen.height_in_pixels.saturating_sub(y as u16));
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, x, y, width, height, !0)?
            .reply()
            .context("reading the framebuffer")?;
        if img.depth != 24 && img.depth != 32 {
            bail!("unsupported X depth {}", img.depth);
        }
        let stride = img.data.len() / height.max(1) as usize;
        let png = encode_png(width as u32, height as u32, stride, &img.data, Order::Bgra)?;
        Ok(Screenshot {
            window_id: Some(w.id),
            width: width as u32,
            height: height as u32,
            png,
        })
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                return Ok(());
            }
            Action::ElementAction { .. } | Action::SetValue { .. } | Action::Focus { .. } => {
                bail!("element actions need the accessibility tree, which the Linux backend does not read yet; use coordinates")
            }
            Action::ChooseFile { paths } => return self.choose_file(paths),
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
                let mod_codes: Vec<u8> = keys::modifier_keysyms(mods)
                    .into_iter()
                    .map(|ks| self.keymap.code_for(&self.conn, ks).map(|(c, _)| c))
                    .collect::<Result<_>>()?;
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
            } => {
                self.move_to(&w, *from_x, *from_y)?;
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
                self.conn.flush()?;
            }
            Action::Scroll { x, y, dx, dy } => {
                self.move_to(&w, *x, *y)?;
                // Wheel buttons: 4 up, 5 down, 6 left, 7 right; ~40px a notch.
                let notches =
                    |d: f64| ((d.abs() / 40.0).round() as u32).max(if d != 0.0 { 1 } else { 0 });
                let (vb, hb) = (if *dy > 0.0 { 5 } else { 4 }, if *dx > 0.0 { 7 } else { 6 });
                for _ in 0..notches(*dy) {
                    self.button(vb, true)?;
                    self.button(vb, false)?;
                }
                for _ in 0..notches(*dx) {
                    self.button(hb, true)?;
                    self.button(hb, false)?;
                }
                self.conn.flush()?;
            }
            Action::TypeText { text } => {
                self.focus(&w)?;
                for c in text.chars() {
                    let key = match c {
                        '\n' | '\r' => Key::Named(NamedKey::Enter),
                        '\t' => Key::Named(NamedKey::Tab),
                        c => Key::Char(c),
                    };
                    // Characters keep their case: keysym() maps 'A' to its
                    // own keysym, which the keymap finds on the shifted level.
                    self.press_chord(&Chord {
                        modifiers: Modifiers::default(),
                        key: Some(key),
                    })?;
                    sleep(Duration::from_millis(3));
                }
            }
            Action::PressKey { keys } => {
                self.focus(&w)?;
                for chord in parse_chords(keys)? {
                    self.press_chord(&chord)?;
                    sleep(Duration::from_millis(20));
                }
            }
            Action::ElementAction { .. }
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
        let waiting = self.portal.as_ref()?.1.waiting()?;
        Some(format!(
            "The app is asking for {} through the file chooser portal (nothing is shown): answer \
             it with choose_file, giving the {}, or no paths to cancel.",
            match (waiting.save, waiting.folders, waiting.multiple) {
                (true, true, _) => "a folder to save into",
                (true, false, _) => "a place to save",
                (false, true, true) => "folders",
                (false, true, false) => "a folder",
                (false, false, true) => "files",
                (false, false, false) => "a file",
            },
            match (waiting.save, waiting.folders) {
                (true, true) => "folder",
                (true, false) => "path to save to",
                _ => "paths",
            },
        ))
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.app.try_wait(), Ok(None))
    }

    fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        let pgid = self.app.id() as i32;
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && matches!(self.app.try_wait(), Ok(None)) {
            sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.app.wait();
        // The display and the bus go when the session is dropped.
    }
}

impl Drop for LinuxSession {
    fn drop(&mut self) {
        self.close();
    }
}
