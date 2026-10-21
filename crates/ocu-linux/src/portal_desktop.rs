//! The whole screen of a Wayland desktop, through the desktop portal: the
//! one way a Wayland compositor (GNOME, KDE Plasma) lets a program see the
//! screen and send input to other apps. The user agrees once, in a dialog
//! the desktop shows; a restore token kept in the state directory lets later
//! sessions start without asking again where the desktop allows it.
//!
//! Input goes through a RemoteDesktop session. It is started together with
//! a ScreenCast of every monitor because pointer moves are absolute to a
//! monitor's stream, and the streams say where each monitor sits. Pictures
//! come from the Screenshot portal, which needs no stream to read.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use futures_util::StreamExt as _;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use ocu_core::MouseButton;

use crate::atspi::runtime;

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE: &str = "org.freedesktop.portal.RemoteDesktop";
const CAST: &str = "org.freedesktop.portal.ScreenCast";
const SHOT: &str = "org.freedesktop.portal.Screenshot";

/// Devices to control: keyboard and pointer.
const KEYBOARD: u32 = 1;
const POINTER: u32 = 2;
/// Monitors, not windows, for the streams.
const MONITOR: u32 = 1;
/// Keep the permission until it is revoked.
const PERSIST: u32 = 2;

/// How long a call may take, and how long the user has to answer the
/// desktop's dialog.
const CALL: Duration = Duration::from_secs(10);
const ANSWER: Duration = Duration::from_secs(180);

/// A monitor the session can reach, in the desktop's logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    /// Its stream, which absolute pointer moves are made in.
    pub stream: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct PortalDesktop {
    conn: Connection,
    session: OwnedObjectPath,
    monitors: Vec<Monitor>,
}

type Options<'a> = HashMap<&'a str, Value<'a>>;
type Results = HashMap<String, OwnedValue>;

fn token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "ocu{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Where the restore token lives between sessions.
fn token_file() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("opencomputeruse/remote-desktop-token"))
}

fn saved_token(path: &std::path::Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    Some(t.trim().to_string()).filter(|t| !t.is_empty())
}

fn save_token(path: &std::path::Path, t: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, t) {
        log::warn!("saving the remote desktop restore token: {e}");
    }
}

/// Options for a portal request, carrying its token.
fn request(token: &str) -> Options<'static> {
    let mut options = Options::new();
    options.insert("handle_token", Value::from(token.to_string()));
    options
}

/// Calls a portal method that answers through a Request object (the one
/// `token` names): waits for its Response signal and returns the results,
/// or fails when the user cancels.
async fn ask<B>(
    conn: &Connection,
    iface: &str,
    method: &str,
    token: &str,
    body: &B,
) -> Result<Results>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let sender = conn
        .unique_name()
        .ok_or_else(|| anyhow!("no name on the session bus"))?
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("{PATH}/request/{sender}/{token}");
    // Listening before asking, so a quick answer isn't missed.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")?
        .member("Response")?
        .path(path.as_str())?
        .build();
    let mut responses = MessageStream::for_match_rule(rule, conn, None).await?;
    tokio::time::timeout(
        CALL,
        conn.call_method(Some(DEST), PATH, Some(iface), method, body),
    )
    .await
    .map_err(|_| anyhow!("the desktop portal did not answer {method}"))?
    .with_context(|| format!("{iface}.{method}"))?;
    let msg = tokio::time::timeout(ANSWER, responses.next())
        .await
        .map_err(|_| anyhow!("no answer to {method}: was the desktop's dialog left open?"))?
        .ok_or_else(|| anyhow!("the desktop portal went away"))??;
    let (code, results): (u32, Results) = msg.body().deserialize()?;
    match code {
        0 => Ok(results),
        1 => bail!("the request to {method} was turned down"),
        _ => bail!("the desktop portal failed {method}"),
    }
}

/// The monitors a started session's streams show.
fn monitors(results: &Results) -> Vec<Monitor> {
    let Some(Value::Array(streams)) = results.get("streams").map(|v| &**v) else {
        return Vec::new();
    };
    let pair = |v: Option<&Value>| match v {
        Some(Value::Structure(s)) => match s.fields() {
            [Value::I32(a), Value::I32(b)] => Some((*a, *b)),
            _ => None,
        },
        _ => None,
    };
    streams
        .inner()
        .iter()
        .filter_map(|stream| {
            let Value::Structure(s) = stream else {
                return None;
            };
            let [Value::U32(node), Value::Dict(props)] = s.fields() else {
                return None;
            };
            let prop = |name: &str| {
                props
                    .iter()
                    .find(|(k, _)| matches!(k, Value::Str(s) if s.as_str() == name))
                    .map(|(_, v)| match v {
                        Value::Value(inner) => &**inner,
                        v => v,
                    })
            };
            let (x, y) = pair(prop("position")).unwrap_or((0, 0));
            let (width, height) = pair(prop("size"))?;
            Some(Monitor {
                stream: *node,
                x,
                y,
                width,
                height,
            })
        })
        .collect()
}

impl PortalDesktop {
    /// Starts a remote desktop session on the session bus, asking the user
    /// unless a saved permission still holds.
    pub fn start() -> Result<Self> {
        runtime().block_on(async {
            let conn = Connection::session()
                .await
                .context("the session bus, where the desktop portal is")?;
            Self::start_on(conn, token_file()).await
        })
    }

    /// Starts a session on `conn`'s portal, keeping the restore token in
    /// `tokens`.
    async fn start_on(conn: Connection, tokens: Option<PathBuf>) -> Result<Self> {
        let restore = tokens.as_deref().and_then(saved_token);
        let t = token();
        let mut options = request(&t);
        options.insert("session_handle_token", Value::from(token()));
        let created = ask(&conn, REMOTE, "CreateSession", &t, &(options,)).await?;
        let session = match created.get("session_handle").map(|v| &**v) {
            Some(Value::Str(s)) => OwnedObjectPath::try_from(s.as_str())?,
            Some(Value::ObjectPath(p)) => p.clone().into(),
            _ => bail!("the desktop portal made no session"),
        };
        let path = ObjectPath::from(&session);

        let t = token();
        let mut options = request(&t);
        options.insert("types", Value::U32(KEYBOARD | POINTER));
        options.insert("persist_mode", Value::U32(PERSIST));
        if let Some(r) = &restore {
            options.insert("restore_token", Value::from(r.clone()));
        }
        ask(&conn, REMOTE, "SelectDevices", &t, &(&path, options)).await?;

        let t = token();
        let mut options = request(&t);
        options.insert("types", Value::U32(MONITOR));
        options.insert("multiple", Value::Bool(true));
        ask(&conn, CAST, "SelectSources", &t, &(&path, options)).await?;

        let t = token();
        let started = ask(&conn, REMOTE, "Start", &t, &(&path, "", request(&t))).await?;
        if let Some(Value::Str(t)) = started.get("restore_token").map(|v| &**v) {
            if let Some(path) = &tokens {
                save_token(path, t.as_str());
            }
        }
        let devices = match started.get("devices").map(|v| &**v) {
            Some(Value::U32(d)) => *d,
            _ => 0,
        };
        if devices & POINTER == 0 || devices & KEYBOARD == 0 {
            bail!("the desktop allowed no pointer and keyboard to be driven");
        }
        let monitors = monitors(&started);
        if monitors.is_empty() {
            bail!("the desktop shared no monitors");
        }
        Ok(Self {
            conn,
            session,
            monitors,
        })
    }

    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    /// The session and empty options: what every Notify method starts with.
    fn head(&self) -> (ObjectPath<'_>, Options<'static>) {
        (ObjectPath::from(&self.session), Options::new())
    }

    /// Calls one of RemoteDesktop's Notify methods.
    fn notify<B>(&self, method: &str, body: B) -> Result<()>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        runtime().block_on(async {
            tokio::time::timeout(
                CALL,
                self.conn
                    .call_method(Some(DEST), PATH, Some(REMOTE), method, &body),
            )
            .await
            .map_err(|_| anyhow!("the desktop portal did not answer {method}"))?
            .with_context(|| format!("RemoteDesktop.{method}"))?;
            Ok(())
        })
    }

    /// Moves the pointer to `x`, `y` on `monitor`.
    pub fn motion(&self, monitor: &Monitor, x: f64, y: f64) -> Result<()> {
        let (s, o) = self.head();
        self.notify("NotifyPointerMotionAbsolute", (s, o, monitor.stream, x, y))
    }

    pub fn button(&self, b: MouseButton, down: bool) -> Result<()> {
        // Linux's button codes: BTN_LEFT, BTN_RIGHT, BTN_MIDDLE.
        let code: i32 = match b {
            MouseButton::Left => 0x110,
            MouseButton::Right => 0x111,
            MouseButton::Middle => 0x112,
        };
        let (s, o) = self.head();
        self.notify("NotifyPointerButton", (s, o, code, down as u32))
    }

    /// Scrolls by wheel notches of ~40 pixels; positive `dy` scrolls the
    /// content down.
    pub fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        let notches = |d: f64| {
            let n = (d / 40.0).round() as i32;
            if n == 0 && d != 0.0 {
                d.signum() as i32
            } else {
                n
            }
        };
        // Axis 0 is vertical, 1 horizontal.
        for (axis, d) in [(0u32, dy), (1u32, dx)] {
            let n = notches(d);
            if n != 0 {
                let (s, o) = self.head();
                self.notify("NotifyPointerAxisDiscrete", (s, o, axis, n))?;
            }
        }
        Ok(())
    }

    /// Presses or releases a key by its evdev code.
    pub fn keycode(&self, evdev: u16, down: bool) -> Result<()> {
        let (s, o) = self.head();
        self.notify("NotifyKeyboardKeycode", (s, o, evdev as i32, down as u32))
    }

    /// Presses or releases whichever key makes `keysym` on the user's
    /// layout, with the level it needs.
    pub fn keysym(&self, keysym: u32, down: bool) -> Result<()> {
        let (s, o) = self.head();
        self.notify("NotifyKeyboardKeysym", (s, o, keysym as i32, down as u32))
    }

    /// The whole desktop as a PNG, in the screen's own pixels.
    pub fn screenshot(&self) -> Result<Vec<u8>> {
        let conn = self.conn.clone();
        runtime().block_on(async move {
            let t = token();
            let mut options = request(&t);
            options.insert("interactive", Value::Bool(false));
            let results = ask(&conn, SHOT, "Screenshot", &t, &("", options)).await?;
            let uri = match results.get("uri").map(|v| &**v) {
                Some(Value::Str(s)) => s.to_string(),
                _ => bail!("the screenshot portal gave no picture"),
            };
            let path = file_path(&uri)
                .ok_or_else(|| anyhow!("the screenshot portal gave {uri}, not a file"))?;
            let png =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            // The portal saves each one for us; nobody else wants it.
            let _ = std::fs::remove_file(&path);
            Ok(png)
        })
    }

    pub fn close(&self) {
        let conn = self.conn.clone();
        let session = self.session.clone();
        runtime().block_on(async move {
            let _ = tokio::time::timeout(
                CALL,
                conn.call_method(
                    Some(DEST),
                    session.as_str(),
                    Some("org.freedesktop.portal.Session"),
                    "Close",
                    &(),
                ),
            )
            .await;
        });
    }
}

impl Drop for PortalDesktop {
    fn drop(&mut self) {
        self.close();
    }
}

/// The path a `file://` URI names, percent escapes decoded.
fn file_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt as _;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
pub(crate) mod fake {
    //! A desktop portal for tests, on a private bus.

    use super::*;
    use std::sync::{Arc, Mutex};
    use zbus::message::Header;

    /// A desktop portal that grants everything and writes down what it is
    /// asked, for the client's calls and their D-Bus signatures.
    #[derive(Clone, Default)]
    pub(crate) struct Fake {
        pub log: Arc<Mutex<Vec<String>>>,
        shot: Arc<Mutex<Option<PathBuf>>>,
    }

    type Opts = HashMap<String, OwnedValue>;

    impl Fake {
        fn note(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }

        /// Answers the request `options` names, after the method returns.
        fn respond(&self, conn: &Connection, hdr: &Header<'_>, options: &Opts, results: Opts) {
            let token = match options.get("handle_token").map(|v| &**v) {
                Some(Value::Str(t)) => t.to_string(),
                _ => panic!("a request without a handle_token"),
            };
            let sender = hdr.sender().unwrap().to_string();
            let path = format!(
                "{PATH}/request/{}/{token}",
                sender.trim_start_matches(':').replace('.', "_")
            );
            let conn = conn.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                conn.emit_signal(
                    Some(sender.as_str()),
                    path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(0u32, results),
                )
                .await
                .unwrap();
            });
        }
    }

    fn owned(v: Value<'_>) -> OwnedValue {
        v.try_to_owned().unwrap()
    }

    fn handle() -> OwnedObjectPath {
        OwnedObjectPath::try_from("/r").unwrap()
    }

    #[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
    impl Fake {
        async fn create_session(
            &self,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] hdr: Header<'_>,
            options: Opts,
        ) -> OwnedObjectPath {
            assert!(options.contains_key("session_handle_token"));
            self.note("CreateSession".into());
            let mut r = Opts::new();
            r.insert(
                "session_handle".into(),
                owned(Value::from("/org/freedesktop/portal/desktop/session/1/s")),
            );
            self.respond(conn, &hdr, &options, r);
            handle()
        }

        async fn select_devices(
            &self,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] hdr: Header<'_>,
            session: OwnedObjectPath,
            options: Opts,
        ) -> OwnedObjectPath {
            let types = u32::try_from(&*options["types"]).unwrap();
            let persist = u32::try_from(&*options["persist_mode"]).unwrap();
            self.note(format!(
                "SelectDevices {} types={types} persist={persist}",
                session.as_str()
            ));
            self.respond(conn, &hdr, &options, Opts::new());
            handle()
        }

        async fn start(
            &self,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] hdr: Header<'_>,
            session: OwnedObjectPath,
            parent: String,
            options: Opts,
        ) -> OwnedObjectPath {
            self.note(format!("Start {} parent={parent:?}", session.as_str()));
            let props = |x: i32, w: i32| {
                let mut p: HashMap<&str, Value> = HashMap::new();
                p.insert("position", Value::from((x, 0i32)));
                p.insert("size", Value::from((w, 1080i32)));
                p
            };
            let streams: Vec<(u32, HashMap<&str, Value>)> =
                vec![(41, props(0, 1920)), (42, props(1920, 1280))];
            let mut r = Opts::new();
            r.insert("devices".into(), owned(Value::U32(3)));
            r.insert("streams".into(), owned(Value::from(streams)));
            r.insert("restore_token".into(), owned(Value::from("kept")));
            self.respond(conn, &hdr, &options, r);
            handle()
        }

        async fn notify_pointer_motion_absolute(
            &self,
            session: OwnedObjectPath,
            _options: Opts,
            stream: u32,
            x: f64,
            y: f64,
        ) {
            let _ = session;
            self.note(format!("motion {stream} {x} {y}"));
        }

        async fn notify_pointer_button(
            &self,
            _s: OwnedObjectPath,
            _o: Opts,
            button: i32,
            state: u32,
        ) {
            self.note(format!("button {button:#x} {state}"));
        }

        async fn notify_pointer_axis_discrete(
            &self,
            _s: OwnedObjectPath,
            _o: Opts,
            axis: u32,
            steps: i32,
        ) {
            self.note(format!("axis {axis} {steps}"));
        }

        async fn notify_keyboard_keycode(
            &self,
            _s: OwnedObjectPath,
            _o: Opts,
            code: i32,
            state: u32,
        ) {
            self.note(format!("keycode {code} {state}"));
        }

        async fn notify_keyboard_keysym(
            &self,
            _s: OwnedObjectPath,
            _o: Opts,
            sym: i32,
            state: u32,
        ) {
            self.note(format!("keysym {sym:#x} {state}"));
        }
    }

    /// The same fake, as the ScreenCast and Screenshot portals.
    struct Cast(Fake);

    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl Cast {
        async fn select_sources(
            &self,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] hdr: Header<'_>,
            session: OwnedObjectPath,
            options: Opts,
        ) -> OwnedObjectPath {
            let types = u32::try_from(&*options["types"]).unwrap();
            self.0
                .note(format!("SelectSources {} types={types}", session.as_str()));
            self.0.respond(conn, &hdr, &options, Opts::new());
            handle()
        }
    }

    struct Shot(Fake);

    #[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
    impl Shot {
        async fn screenshot(
            &self,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] hdr: Header<'_>,
            parent: String,
            options: Opts,
        ) -> OwnedObjectPath {
            let interactive = bool::try_from(&*options["interactive"]).unwrap();
            self.0.note(format!(
                "Screenshot parent={parent:?} interactive={interactive}"
            ));
            let path = self.0.shot.lock().unwrap().clone().unwrap();
            std::fs::write(&path, b"png").unwrap();
            let mut r = Opts::new();
            r.insert(
                "uri".into(),
                owned(Value::from(format!("file://{}", path.display()))),
            );
            self.0.respond(conn, &hdr, &options, r);
            handle()
        }
    }

    /// A running fake portal: drop it last.
    pub(crate) struct Running {
        pub fake: Fake,
        /// Holds the token file and the screenshots.
        pub dir: PathBuf,
        _service: Connection,
        _bus: Arc<ocu_core::portal::Bus>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A remote desktop session on a fake portal, or `None` without a
    /// dbus-daemon to run one on.
    pub(crate) fn start() -> Option<(Running, PortalDesktop)> {
        static N: AtomicU64 = AtomicU64::new(0);
        let Ok(bus) = ocu_core::portal::Bus::start() else {
            eprintln!("no dbus-daemon here; skipping");
            return None;
        };
        let dir = std::env::temp_dir().join(format!(
            "ocu-portal-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = Fake::default();
        *fake.shot.lock().unwrap() = Some(dir.join("Screenshot from now.png"));
        let address = bus.address().to_string();
        let served = fake.clone();
        let (service, client) = runtime().block_on(async move {
            let service = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .name(DEST)
                .unwrap()
                .serve_at(PATH, served.clone())
                .unwrap()
                .serve_at(PATH, Cast(served.clone()))
                .unwrap()
                .serve_at(PATH, Shot(served))
                .unwrap()
                .build()
                .await
                .unwrap();
            let client = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            (service, client)
        });
        let desktop = runtime()
            .block_on(PortalDesktop::start_on(client, Some(dir.join("token"))))
            .expect("a remote desktop session");
        Some((
            Running {
                fake,
                dir,
                _service: service,
                _bus: bus,
            },
            desktop,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_become_paths() {
        assert_eq!(
            file_path("file:///home/a/Pictures/Screenshot%20from%202026.png"),
            Some(PathBuf::from("/home/a/Pictures/Screenshot from 2026.png"))
        );
        assert_eq!(
            file_path("file://localhost/tmp/a.png"),
            Some("/tmp/a.png".into())
        );
        assert_eq!(file_path("https://example.com/a.png"), None);
        // An escape in the last three bytes is decoded too.
        assert_eq!(file_path("file:///tmp/shot%20"), Some("/tmp/shot ".into()));
        // A cut-off escape is kept as it is, never read past the end.
        assert_eq!(file_path("file:///tmp/shot%2"), Some("/tmp/shot%2".into()));
    }

    #[test]
    fn drives_a_desktop_through_the_portal() {
        let Some((running, desktop)) = fake::start() else {
            return;
        };
        let (fake, dir) = (&running.fake, &running.dir);
        assert_eq!(
            desktop.monitors(),
            &[
                Monitor {
                    stream: 41,
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080
                },
                Monitor {
                    stream: 42,
                    x: 1920,
                    y: 0,
                    width: 1280,
                    height: 1080
                },
            ]
        );
        assert_eq!(std::fs::read_to_string(dir.join("token")).unwrap(), "kept");
        let second = desktop.monitors()[1].clone();
        desktop.motion(&second, 10.5, 20.0).unwrap();
        desktop.button(MouseButton::Right, true).unwrap();
        desktop.scroll(0.0, 80.0).unwrap();
        desktop.keycode(42, true).unwrap();
        desktop.keysym(0x61, false).unwrap();
        assert_eq!(desktop.screenshot().unwrap(), b"png");
        assert!(
            !dir.join("Screenshot from now.png").exists(),
            "the portal's file is removed"
        );
        let log = fake.log.lock().unwrap().clone();
        let session = "/org/freedesktop/portal/desktop/session/1/s";
        assert_eq!(
            log,
            vec![
                "CreateSession".to_string(),
                format!("SelectDevices {session} types=3 persist=2"),
                format!("SelectSources {session} types=1"),
                format!("Start {session} parent=\"\""),
                "motion 42 10.5 20".into(),
                "button 0x111 1".into(),
                "axis 0 2".into(),
                "keycode 42 1".into(),
                "keysym 0x61 0".into(),
                "Screenshot parent=\"\" interactive=false".into(),
            ]
        );
        drop(desktop);
    }
}
