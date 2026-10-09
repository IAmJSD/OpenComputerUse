//! The xdg-desktop-portal FileChooser, answered by the agent, in front of a
//! private session bus that otherwise forwards to the user's real one.
//!
//! An app told to use the portal (GTK with `GTK_USE_PORTAL=1`, Qt with
//! `QT_QPA_PLATFORMTHEME=xdgdesktopportal`, Chromium whenever a portal is on
//! the bus) calls `OpenFile`, `SaveFile` or `SaveFiles`, gets back a request
//! path, and waits for a `Response` signal on it. We answer that signal
//! ourselves, so nothing is drawn.
//!
//! The app runs on a session bus of its own ([`Bus`]), so a plain private bus
//! would cut it off from the keyring, notifications and the rest. Instead a
//! bridge owns the portal name and a short list of real services on the
//! private bus and forwards every call it does not answer itself to the
//! user's real bus, relaying replies and signals back. File-chooser calls are
//! intercepted; everything else is the real thing.
//!
//! A forwarded call that carries file descriptors is refused rather than
//! mis-forwarded: the bridge copies message bytes, not fds. The calls that
//! matter here (the keyring, text notifications, the a11y bus address, the
//! portal's own `Settings` and `OpenURI`-by-uri) carry none.
//!
//! Nothing here is Linux-specific, which lets the tests run anywhere
//! `dbus-daemon` does.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt as _;
use zbus::message::{Message, Type};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

/// The name a desktop portal answers on.
pub const DESKTOP: &str = "org.freedesktop.portal.Desktop";
/// Where it is exported.
pub const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
/// The FileChooser interface, the one we answer rather than forward.
const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
/// The interface a request is answered on.
const REQUEST: &str = "org.freedesktop.portal.Request";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// The FileChooser portal version we present. 3 is the first with directories.
const VERSION: u32 = 3;

/// Real services forwarded to the user's bus, so the app keeps them. The
/// portal name itself is forwarded too (bar FileChooser), and is not listed
/// here because it is owned for interception, not only forwarding.
const FORWARD_NAMES: &[&str] = &[
    "org.freedesktop.secrets",       // the keyring
    "org.freedesktop.Notifications", // desktop notifications
    "org.a11y.Bus",                  // where the accessibility bus lives
    "org.freedesktop.ScreenSaver",   // inhibit, which media apps ask for
];

/// The portal's response codes.
mod response {
    pub const SUCCESS: u32 = 0;
    pub const CANCELLED: u32 = 1;
}

/// A chooser an app is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The request path the app was handed, which [`Portal::answer`] takes.
    pub token: String,
    pub save: bool,
    pub multiple: bool,
    /// A folder rather than files. With `save`, the folder `SaveFiles` saves
    /// into.
    pub folders: bool,
}

struct Pending {
    request: Waiting,
    /// The file names `SaveFiles` will write into the chosen folder.
    names: Vec<PathBuf>,
    /// The app the answer is addressed to, so its `Response` reaches it even
    /// though a signal is otherwise a broadcast.
    caller: Option<String>,
}

struct State {
    pending: HashMap<String, Pending>,
    next: u64,
}

/// Answers file choosers on a bus and reports which one is waiting. The
/// bridge thread does the talking; this is the handle the session holds.
pub struct Portal {
    state: Mutex<State>,
    /// Raw messages for the bridge to send on the private bus: the `Response`
    /// signals that answer choosers.
    out: tokio::sync::mpsc::UnboundedSender<Message>,
}

impl Portal {
    /// Gives the app a private bus with a file chooser portal on it, bridged
    /// to the user's real bus at `upstream` for everything else. Without an
    /// `upstream`, the services in [`FORWARD_NAMES`] are simply absent.
    ///
    /// Returns once the portal has claimed its name, so an app started after
    /// this finds it.
    pub fn start(private: &str, upstream: Option<String>) -> Result<Arc<Self>> {
        let (out, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let portal = Arc::new(Portal {
            state: Mutex::new(State {
                pending: HashMap::new(),
                next: 0,
            }),
            out,
        });
        // A worker thread or two: a forwarded call awaits the real bus, and
        // the bridge keeps serving while it does.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| anyhow!("the portal needs an async runtime: {e}"))?;
        let bridge = Arc::clone(&portal);
        let private = private.to_string();
        let (ready, claimed) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("portal".into())
            .spawn(move || {
                runtime.block_on(async move {
                    match connect(&private, upstream.as_deref()).await {
                        Ok(buses) => {
                            let _ = ready.send(Ok(()));
                            run(buses, bridge, out_rx).await;
                        }
                        Err(e) => {
                            let _ = ready.send(Err(e));
                        }
                    }
                });
            })
            .map_err(|e| anyhow!("starting the portal's thread: {e}"))?;
        claimed
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow!("the portal did not claim {DESKTOP} in time"))??;
        Ok(portal)
    }

    /// The chooser the app is waiting on, if any. There is at most one: a
    /// newer request cancels the older.
    pub fn waiting(&self) -> Option<Waiting> {
        self.state
            .lock()
            .ok()?
            .pending
            .values()
            .next()
            .map(|p| p.request.clone())
    }

    /// Answers the chooser `token` names with `paths`; none cancels it.
    pub fn answer(&self, token: &str, paths: &[PathBuf]) -> Result<()> {
        let p = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("the portal's lock is poisoned"))?;
            let seen = state
                .pending
                .get(token)
                .ok_or_else(|| anyhow!("the app is not asking for a file"))?;
            check(&seen.request, paths)?;
            state.pending.remove(token).expect("just seen")
        };

        let uris: Vec<String> = match paths {
            // SaveFiles answers with the files it will write, not the folder.
            [dir] if p.request.save && p.request.folders => {
                p.names.iter().map(|n| file_uri(&dir.join(n))).collect()
            }
            _ => paths.iter().map(|p| file_uri(p)).collect(),
        };
        let signal = if paths.is_empty() {
            response_signal(token, p.caller.as_deref(), response::CANCELLED, &[])?
        } else {
            response_signal(token, p.caller.as_deref(), response::SUCCESS, &uris)?
        };
        self.out
            .send(signal)
            .map_err(|_| anyhow!("the portal's bus is gone"))
    }

    /// Records a file-chooser request, returning the path the app waits on
    /// and the tokens of any older request now cancelled.
    fn record(
        &self,
        caller: Option<&str>,
        token: String,
        save: bool,
        multiple: bool,
        folders: bool,
        names: Vec<PathBuf>,
    ) -> Vec<Pending> {
        let mut state = self.state.lock().expect("the portal's lock");
        let superseded: Vec<Pending> = state.pending.drain().map(|(_, p)| p).collect();
        state.pending.insert(
            token.clone(),
            Pending {
                request: Waiting {
                    token,
                    save,
                    multiple,
                    folders,
                },
                names,
                caller: caller.map(str::to_owned),
            },
        );
        superseded
    }

    fn forget(&self, token: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.remove(token);
        }
    }

    fn next_token(&self, caller: Option<&str>, handle: Option<&str>) -> String {
        let n = {
            let mut state = self.state.lock().expect("the portal's lock");
            state.next += 1;
            state.next
        };
        request_path(caller, handle, n)
    }
}

/// Checks paths against what a chooser said it would take.
fn check(request: &Waiting, paths: &[PathBuf]) -> Result<()> {
    if request.save && request.folders {
        return match paths {
            [] => Ok(()),
            [dir] if dir.is_dir() => Ok(()),
            [dir] => bail!("there is no folder {}", dir.display()),
            _ => bail!("the app asks for one folder to save into"),
        };
    }
    crate::paths::check_answer(paths, request.save, request.multiple)
}

/// `file:///a/b`, percent-encoded, the form the portal's results take.
pub fn file_uri(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec();
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let mut uri = String::from("file://");
    for byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The request path the spec has a client expect, so one that subscribes
/// before its call returns still hears the answer: `.../request/SENDER/TOKEN`,
/// with the sender's unique name stripped of `:` and its dots made `_`.
fn request_path(sender: Option<&str>, token: Option<&str>, n: u64) -> String {
    let valid =
        |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let sender = sender
        .map(|s| s.trim_start_matches(':').replace('.', "_"))
        .filter(|s| valid(&s.as_str()))
        .unwrap_or_else(|| "ocu".into());
    match token.filter(valid) {
        Some(token) => format!("{DESKTOP_PATH}/request/{sender}/{token}"),
        None => format!("{DESKTOP_PATH}/request/{sender}/ocu{n}"),
    }
}

/// The `Response` signal that answers a chooser: `(code, {"uris": [...]})` on
/// the request path, addressed to the app that asked.
fn response_signal(
    token: &str,
    caller: Option<&str>,
    code: u32,
    uris: &[String],
) -> Result<Message> {
    let mut results: HashMap<&str, Value> = HashMap::new();
    if !uris.is_empty() {
        results.insert("uris", Value::from(uris.to_vec()));
    }
    let mut builder = Message::signal(token, REQUEST, "Response")?;
    if let Some(caller) = caller {
        builder = builder.destination(caller)?;
    }
    Ok(builder.build(&(code, results))?)
}

// ------------------------------------------------------------ the bridge

/// The two buses the bridge works between.
struct Buses {
    /// The app's private bus, where we own the names.
    private: Connection,
    /// The user's real bus, or none when there was nothing to bridge to.
    upstream: Option<Connection>,
}

/// Connects both buses, claims the portal and forwarded names on the private
/// one, and subscribes to the signals worth relaying from the real one.
async fn connect(private: &str, upstream: Option<&str>) -> Result<Buses> {
    // No interface is served: with the object server idle, zbus hands every
    // incoming call to our own stream instead of answering it itself.
    let private = zbus::connection::Builder::address(private)?
        .build()
        .await
        .map_err(|e| anyhow!("connecting to the private bus: {e}"))?;
    for name in std::iter::once(DESKTOP).chain(FORWARD_NAMES.iter().copied()) {
        private
            .request_name(name)
            .await
            .map_err(|e| anyhow!("asking for {name} on the private bus: {e}"))?;
    }

    let upstream = match upstream {
        Some(address) => {
            let conn = zbus::connection::Builder::address(address)?
                .build()
                .await
                .map_err(|e| anyhow!("connecting to the user's bus at {address}: {e}"))?;
            subscribe(&conn).await?;
            Some(conn)
        }
        None => None,
    };
    log::info!("answering file choosers on {private:?}, forwarding the rest");
    Ok(Buses { private, upstream })
}

/// Asks the real bus for the signals the app will want relayed: a forwarded
/// request's `Response`, and signals from the services we forward.
async fn subscribe(upstream: &Connection) -> Result<()> {
    let dbus = zbus::fdo::DBusProxy::new(upstream)
        .await
        .map_err(|e| anyhow!("talking to the real bus: {e}"))?;
    let add = |rule: MatchRule<'static>| {
        let dbus = dbus.clone();
        async move {
            dbus.add_match_rule(rule)
                .await
                .map_err(|e| anyhow!("subscribing to a relayed signal: {e}"))
        }
    };
    add(MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(REQUEST)?
        .build())
    .await?;
    for name in FORWARD_NAMES {
        add(MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(*name)?
            .build())
        .await?;
    }
    Ok(())
}

/// The bridge loop: answer file choosers, forward the rest, relay what comes
/// back, and send out the `Response` signals that answer choosers.
async fn run(
    buses: Buses,
    portal: Arc<Portal>,
    mut out: tokio::sync::mpsc::UnboundedReceiver<Message>,
) {
    let mut from_app = MessageStream::from(&buses.private);
    let mut from_real = buses.upstream.as_ref().map(MessageStream::from);
    // Forwarded calls, by the serial they went out on, so a reply can be sent
    // back to the app that is waiting. Local to the one task that reads both
    // buses, so it needs no lock.
    let mut forwarded: HashMap<NonZeroU32, Message> = HashMap::new();

    loop {
        tokio::select! {
            Some(Ok(msg)) = from_app.next() => {
                handle_from_app(&buses, &portal, &mut forwarded, msg).await;
            }
            Some(Ok(msg)) = next_opt(&mut from_real), if from_real.is_some() => {
                relay_from_real(&buses.private, &mut forwarded, msg).await;
            }
            answer = out.recv() => {
                match answer {
                    Some(signal) => {
                        if let Err(e) = buses.private.send(&signal).await {
                            log::warn!("the app stopped waiting for its file: {e}");
                        }
                    }
                    // Every sender is gone, so the portal itself is: nothing
                    // left to answer, and the buses can be let go.
                    None => break,
                }
            }
        }
    }
}

/// Awaits the next message on an optional stream, pending forever when there
/// is none, so the bridge's `select!` can leave the real bus out cleanly.
async fn next_opt(stream: &mut Option<MessageStream>) -> Option<zbus::Result<Message>> {
    match stream {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

/// A call from the app: answered here if it is a file chooser, otherwise
/// forwarded to the real bus.
async fn handle_from_app(
    buses: &Buses,
    portal: &Arc<Portal>,
    forwarded: &mut HashMap<NonZeroU32, Message>,
    msg: Message,
) {
    // Owned up front, so the borrow of `msg` ends before it is forwarded.
    let (ty, interface, member, path, caller) = {
        let hdr = msg.header();
        (
            hdr.message_type(),
            hdr.interface().map(|i| i.to_string()),
            hdr.member().map(|m| m.to_string()),
            hdr.path().map(|p| p.to_string()),
            hdr.sender().map(|s| s.to_string()),
        )
    };
    if ty != Type::MethodCall {
        return;
    }

    let answered = match (path.as_deref(), interface.as_deref(), member.as_deref()) {
        (Some(DESKTOP_PATH), Some(FILE_CHOOSER), Some(m)) => {
            answer_chooser(&buses.private, portal, &msg, caller.as_deref(), m).await
        }
        (Some(DESKTOP_PATH), Some(PROPERTIES), Some(m @ ("Get" | "GetAll"))) => {
            answer_properties(&buses.private, &msg, m).await
        }
        (Some(p), Some(REQUEST), Some("Close")) => {
            portal.forget(p);
            reply(&buses.private, &msg, &()).await
        }
        _ => false,
    };
    if !answered {
        forward(buses, forwarded, msg).await;
    }
}

/// Handles one FileChooser call, or returns `false` for a member we do not
/// know, which is then forwarded like anything else.
async fn answer_chooser(
    private: &Connection,
    portal: &Arc<Portal>,
    msg: &Message,
    caller: Option<&str>,
    member: &str,
) -> bool {
    let options = chooser_options(msg);
    let (save, multiple, folders, names) = match member {
        "OpenFile" => (
            false,
            flag(&options, "multiple"),
            flag(&options, "directory"),
            vec![],
        ),
        "SaveFile" => (true, false, false, vec![]),
        "SaveFiles" => (true, false, true, file_names(&options)),
        _ => return false,
    };
    let handle = string(&options, "handle_token");
    let token = portal.next_token(caller, handle);
    let superseded = portal.record(caller, token.clone(), save, multiple, folders, names);
    for old in superseded {
        if let Ok(signal) = response_signal(
            &old.request.token,
            old.caller.as_deref(),
            response::CANCELLED,
            &[],
        ) {
            let _ = private.send(&signal).await;
        }
    }
    match ObjectPath::try_from(token.as_str()) {
        Ok(path) => reply(private, msg, &path).await,
        Err(e) => {
            reply_error(
                private,
                msg,
                "org.freedesktop.DBus.Error.Failed",
                &e.to_string(),
            )
            .await;
            true
        }
    }
}

/// Forwards a call to the real bus verbatim and remembers it, so its reply
/// can be sent back to the app.
async fn forward(buses: &Buses, forwarded: &mut HashMap<NonZeroU32, Message>, msg: Message) {
    let Some(upstream) = &buses.upstream else {
        return reply_error(
            &buses.private,
            &msg,
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "this session has no bus to forward to",
        )
        .await;
    };
    // The bridge copies bytes, not file descriptors, so a call that passes one
    // is refused rather than forwarded without it.
    if !msg.data().fds().is_empty() {
        return reply_error(
            &buses.private,
            &msg,
            "org.freedesktop.DBus.Error.NotSupported",
            "this call passes a file descriptor, which the portal bridge does not forward",
        )
        .await;
    }
    let call = match build_like(&msg, Forwarded::Call) {
        Ok(call) => call,
        Err(e) => {
            return reply_error(
                &buses.private,
                &msg,
                "org.freedesktop.DBus.Error.Failed",
                &format!("the call could not be forwarded: {e}"),
            )
            .await;
        }
    };
    // The serial is fixed once the message is built; it keys the reply back.
    let serial = call.primary_header().serial_num();
    match upstream.send(&call).await {
        Ok(()) => {
            forwarded.insert(serial, msg);
        }
        Err(e) => {
            reply_error(
                &buses.private,
                &msg,
                "org.freedesktop.DBus.Error.NoReply",
                &format!("the real bus did not take the call: {e}"),
            )
            .await;
        }
    }
}

/// A reply or signal from the real bus, sent on to the app: a reply to a
/// forwarded call, or a signal we subscribed to.
async fn relay_from_real(
    private: &Connection,
    forwarded: &mut HashMap<NonZeroU32, Message>,
    msg: Message,
) {
    let hdr = msg.header();
    match hdr.message_type() {
        Type::MethodReturn | Type::Error => {
            let Some(serial) = hdr.reply_serial() else {
                return;
            };
            let Some(call) = forwarded.remove(&serial) else {
                return;
            };
            if msg.data().fds().is_empty() {
                let _ = relay_reply(private, &call, &msg).await;
            } else {
                reply_error(
                    private,
                    &call,
                    "org.freedesktop.DBus.Error.NotSupported",
                    "the reply carries a file descriptor, which the portal bridge does not forward",
                )
                .await;
            }
        }
        Type::Signal if msg.data().fds().is_empty() => {
            if let Ok(signal) = build_like(&msg, Forwarded::Signal) {
                let _ = private.send(&signal).await;
            }
        }
        _ => {}
    }
}

/// Which kind of message [`build_like`] copies into.
enum Forwarded {
    Call,
    Signal,
}

/// Rebuilds a call or signal with the same path, interface, member and body
/// bytes, addressed to the same destination, for sending on the other bus.
fn build_like(msg: &Message, kind: Forwarded) -> Result<Message> {
    let hdr = msg.header();
    let path = hdr.path().ok_or_else(|| anyhow!("no object path"))?.clone();
    let member = hdr.member().ok_or_else(|| anyhow!("no member"))?.clone();
    let body = msg.body();
    let signature = body.signature().to_string();
    let bytes = body.data().bytes();

    let mut builder = match kind {
        Forwarded::Call => {
            let mut b = Message::method_call(&path, &member)?;
            if let Some(dest) = hdr.destination() {
                b = b.destination(dest.to_owned())?;
            }
            b
        }
        Forwarded::Signal => {
            let interface = hdr.interface().ok_or_else(|| anyhow!("no interface"))?;
            Message::signal(&path, interface, &member)?
        }
    };
    if let Some(interface) = hdr.interface() {
        builder = builder.interface(interface.to_owned())?;
    }
    // Safe: the bytes and signature are a body zbus itself serialized, and the
    // fd-carrying case is turned away before this is reached.
    Ok(unsafe { builder.build_raw_body(bytes, signature.as_str(), Vec::new())? })
}

/// Sends the real bus's reply on to the app it belongs to, body bytes and all.
async fn relay_reply(private: &Connection, call: &Message, reply: &Message) -> Result<()> {
    let call_hdr = call.header();
    let reply_hdr = reply.header();
    let body = reply.body();
    let signature = body.signature().to_string();
    let bytes = body.data().bytes();
    let built = match reply_hdr.message_type() {
        Type::Error => {
            let name = reply_hdr
                .error_name()
                .ok_or_else(|| anyhow!("an error with no name"))?;
            Message::error(&call_hdr, name.to_owned())?
        }
        _ => Message::method_return(&call_hdr)?,
    };
    // Safe: see `build_like`; the fd case is turned away before this.
    let built = unsafe { built.build_raw_body(bytes, signature.as_str(), Vec::new())? };
    private.send(&built).await?;
    Ok(())
}

/// Sends `body` back to the caller of `call` as a method return.
async fn reply<B>(conn: &Connection, call: &Message, body: &B) -> bool
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let sent = async {
        let built = Message::method_return(&call.header())?.build(body)?;
        conn.send(&built).await
    }
    .await;
    if let Err(e) = sent {
        log::warn!("answering the app: {e}");
    }
    true
}

async fn reply_error(conn: &Connection, call: &Message, name: &str, message: &str) {
    let sent = async {
        let built = Message::error(&call.header(), name)?.build(&(message,))?;
        conn.send(&built).await
    }
    .await;
    if let Err(e) = sent {
        log::warn!("answering the app with an error: {e}");
    }
}

/// Answers `Properties.Get`/`GetAll` for the FileChooser interface, whose one
/// property is `version`. A request about any other interface is left to be
/// forwarded (`false`), so the real portal's properties still reach the app.
async fn answer_properties(private: &Connection, msg: &Message, member: &str) -> bool {
    // The interface the properties are asked about is the first argument of
    // both Get and GetAll.
    let interface = msg
        .body()
        .deserialize::<(String,)>()
        .map(|(interface,)| interface);
    if interface.as_deref() != Ok(FILE_CHOOSER) {
        return false;
    }
    if member == "GetAll" {
        let all = HashMap::from([("version", Value::from(VERSION))]);
        return reply(private, msg, &(all,)).await;
    }
    match msg.body().deserialize::<(String, String)>() {
        Ok((_, property)) if property == "version" => {
            reply(private, msg, &(Value::from(VERSION),)).await
        }
        Ok((_, property)) => {
            reply_error(
                private,
                msg,
                "org.freedesktop.DBus.Error.UnknownProperty",
                &format!("the file chooser has no {property}"),
            )
            .await;
            true
        }
        Err(_) => false,
    }
}

/// A FileChooser call's options dictionary.
fn chooser_options(msg: &Message) -> HashMap<String, OwnedValue> {
    msg.body()
        .deserialize::<(String, String, HashMap<String, OwnedValue>)>()
        .map(|(_, _, options)| options)
        .unwrap_or_default()
}

/// A boolean option, absent meaning no.
fn flag(options: &HashMap<String, OwnedValue>, key: &str) -> bool {
    options
        .get(key)
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false)
}

fn string<'a>(options: &'a HashMap<String, OwnedValue>, key: &str) -> Option<&'a str> {
    options.get(key).and_then(|v| v.downcast_ref::<&str>().ok())
}

/// `SaveFiles`'s `files`: NUL-terminated byte strings, kept as bare names so
/// none can climb out of the chosen folder.
fn file_names(options: &HashMap<String, OwnedValue>) -> Vec<PathBuf> {
    let Some(files) = options
        .get("files")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<Vec<u8>>::try_from(v).ok())
    else {
        return vec![];
    };
    files
        .into_iter()
        .filter_map(|mut f| {
            if f.last() == Some(&0) {
                f.pop();
            }
            let name = PathBuf::from(String::from_utf8(f).ok()?);
            Some(PathBuf::from(name.file_name()?))
        })
        .collect()
}

// ------------------------------------------------------------- the bus

/// A private session bus, ended when this is dropped. Answering
/// `org.freedesktop.portal.Desktop` here cannot touch anything the user is
/// running.
pub struct Bus {
    child: std::process::Child,
    address: String,
    dir: PathBuf,
}

impl Bus {
    /// Starts `dbus-daemon` on a socket in a directory only this user can
    /// enter, and waits for it to listen.
    pub fn start() -> Result<Arc<Self>> {
        use std::io::BufRead as _;
        use std::process::{Command, Stdio};

        let dir = private_dir()?;
        let config = dir.join("bus.config");
        let started = std::fs::write(&config, bus_config(&dir))
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                Command::new("dbus-daemon")
                    .arg("--config-file")
                    .arg(&config)
                    .arg("--nofork")
                    .arg("--print-address")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| anyhow!("starting dbus-daemon: {e}"))
            });
        let mut child = match started {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let bus = |child, address| Bus {
            child,
            address,
            dir: dir.clone(),
        };

        // The address is the first line, printed once the bus is listening.
        let mut address = String::new();
        if let Some(out) = child.stdout.take() {
            let _ = std::io::BufReader::new(out).read_line(&mut address);
        }
        let address = address.trim().to_string();
        if address.is_empty() {
            drop(bus(child, address));
            bail!("dbus-daemon printed no address");
        }
        log::info!("a private session bus at {address}");
        Ok(Arc::new(bus(child, address)))
    }

    /// The address to hand an app in `DBUS_SESSION_BUS_ADDRESS`.
    pub fn address(&self) -> &str {
        &self.address
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A fresh directory, mode 0700, for the bus's config and socket. Created
/// rather than reused, so a directory someone else made first is an error,
/// not a place we write into.
fn private_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let dir = base.join(format!("ocu-bus-{}-{nanos:x}", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .map_err(|e| anyhow!("making {}: {e}", dir.display()))?;
    Ok(dir)
}

/// A session bus listening in `dir`. Permissive, since its one client is the
/// app we started.
fn bus_config(dir: &Path) -> String {
    // A D-Bus address value, percent-encoded, which also leaves nothing for
    // XML to escape.
    let mut listen = String::new();
    for byte in dir.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'/' | b'.' => {
                listen.push(byte as char)
            }
            _ => listen.push_str(&format!("%{byte:02x}")),
        }
    }
    format!(
        r#"<!DOCTYPE busconfig PUBLIC
 "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={listen}</listen>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(save: bool, multiple: bool) -> Waiting {
        Waiting {
            token: "/x".into(),
            save,
            multiple,
            folders: false,
        }
    }

    #[test]
    fn encodes_a_uri() {
        assert_eq!(
            file_uri(Path::new("/home/a/Documents/a b.txt")),
            "file:///home/a/Documents/a%20b.txt"
        );
        assert_eq!(file_uri(Path::new("/a#b?c")), "file:///a%23b%3Fc");
        assert_eq!(file_uri(Path::new("/home/a/b.txt")), "file:///home/a/b.txt");
    }

    #[test]
    fn makes_the_request_path_a_client_expects() {
        assert_eq!(
            request_path(Some(":1.42"), Some("gtk123"), 7),
            "/org/freedesktop/portal/desktop/request/1_42/gtk123"
        );
        // A token that is not a path element is not used.
        assert_eq!(
            request_path(Some(":1.42"), Some("a/b"), 7),
            "/org/freedesktop/portal/desktop/request/1_42/ocu7"
        );
    }

    #[test]
    fn refuses_more_than_a_save_takes() {
        let dir = std::env::temp_dir();
        let two = vec![dir.join("ocu-a.txt"), dir.join("ocu-b.txt")];
        assert!(
            check(&request(true, false), &two).is_err(),
            "a save takes one"
        );
        assert!(
            check(&request(false, false), &two).is_err(),
            "one file asked"
        );
        let listed = request(false, true);
        assert!(check(&listed, &two).is_err(), "and they do not exist");
        for p in &two {
            std::fs::write(p, b"x").unwrap();
        }
        assert!(check(&listed, &two).is_ok());
        assert!(check(&request(false, false), &two).is_err());
        for p in &two {
            std::fs::remove_file(p).ok();
        }
    }

    #[test]
    fn takes_no_paths_as_a_cancel() {
        assert!(check(&request(true, false), &[]).is_ok());
        assert!(check(&request(false, false), &[]).is_ok());
    }

    #[test]
    fn refuses_a_missing_file() {
        assert!(check(&request(false, false), &[PathBuf::from("/no/such/thing")]).is_err());
    }

    #[test]
    fn keeps_save_files_names_inside_the_folder() {
        let files = vec![b"a.txt\0".to_vec(), b"../../b.txt\0".to_vec()];
        let value = OwnedValue::try_from(Value::from(files)).unwrap();
        let options = HashMap::from([("files".to_string(), value)]);
        assert_eq!(
            file_names(&options),
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]
        );
    }

    /// Drives the real interface over a private bus, with a stub standing in
    /// for the user's real bus so forwarding can be exercised too. Skipped
    /// where there is no `dbus-daemon`.
    #[cfg(unix)]
    mod over_a_bus {
        use super::*;
        use zbus::zvariant::{OwnedObjectPath, Str};

        /// A FileChooser proxy on the app's bus, and the portal handle.
        async fn pair() -> Option<(Arc<Bus>, Arc<Portal>, Connection, zbus::Proxy<'static>)> {
            pair_with_upstream(None).await
        }

        async fn pair_with_upstream(
            upstream: Option<String>,
        ) -> Option<(Arc<Bus>, Arc<Portal>, Connection, zbus::Proxy<'static>)> {
            let bus = Bus::start().ok()?;
            let address = bus.address().to_string();
            let portal = tokio::task::spawn_blocking(move || Portal::start(&address, upstream))
                .await
                .unwrap()
                .expect("claiming the name");
            let client = zbus::connection::Builder::address(bus.address())
                .ok()?
                .build()
                .await
                .expect("connecting to the app bus");
            let proxy = zbus::Proxy::new(&client, DESKTOP, DESKTOP_PATH, FILE_CHOOSER)
                .await
                .expect("a client proxy");
            Some((bus, portal, client, proxy))
        }

        async fn within<F: std::future::Future>(f: F) -> F::Output {
            tokio::time::timeout(Duration::from_secs(20), f)
                .await
                .expect("the portal did not answer in time")
        }

        async fn ask(
            proxy: &zbus::Proxy<'static>,
            member: &str,
            options: &[(&'static str, bool)],
        ) -> String {
            let options: HashMap<Str, OwnedValue> = options
                .iter()
                .map(|&(k, v)| (Str::from(k), OwnedValue::from(v)))
                .collect();
            within(async move {
                let path: OwnedObjectPath = proxy
                    .call(member, &(String::new(), String::new(), options))
                    .await
                    .expect("calling the chooser");
                path.as_str().to_string()
            })
            .await
        }

        /// The Response on one request's path, matched on the Request
        /// interface it comes on rather than the FileChooser one.
        async fn responses_on(client: &Connection, path: &str) -> zbus::MessageStream {
            let rule = MatchRule::builder()
                .path(path.to_string())
                .unwrap()
                .interface(REQUEST)
                .unwrap()
                .member("Response")
                .unwrap()
                .build();
            zbus::MessageStream::for_match_rule(rule, client, Some(4))
                .await
                .expect("listening for the answer")
        }

        async fn response(stream: &mut zbus::MessageStream) -> (u32, HashMap<String, OwnedValue>) {
            let answer = within(stream.next()).await.expect("an answer");
            answer
                .expect("the signal itself")
                .body()
                .deserialize::<(u32, HashMap<String, OwnedValue>)>()
                .expect("the answer's shape")
        }

        #[tokio::test]
        async fn answers_a_chooser_over_the_bus() {
            let Some((_bus, portal, client, proxy)) = pair().await else {
                eprintln!("no dbus-daemon here; skipping");
                return;
            };
            let file = std::env::temp_dir().join("ocu-portal-test.txt");
            std::fs::write(&file, b"x").unwrap();

            let token = ask(&proxy, "OpenFile", &[]).await;
            assert_eq!(portal.waiting().expect("the app is waiting").token, token);

            let mut answers = responses_on(&client, &token).await;
            portal
                .answer(&token, std::slice::from_ref(&file))
                .expect("answering");
            let (code, results) = response(&mut answers).await;
            assert_eq!(code, 0, "a successful pick");
            let uris = Vec::<String>::try_from(results["uris"].try_clone().unwrap())
                .expect("uris is a list of strings");
            assert_eq!(uris, vec![file_uri(&file)]);
            std::fs::remove_file(&file).ok();
        }

        #[tokio::test]
        async fn a_cancel_reaches_the_app_as_a_cancelled_answer() {
            let Some((_bus, portal, client, proxy)) = pair().await else {
                eprintln!("no dbus-daemon here; skipping");
                return;
            };
            let token = ask(&proxy, "OpenFile", &[]).await;
            let mut answers = responses_on(&client, &token).await;
            portal.answer(&token, &[]).expect("cancelling");
            let (code, results) = response(&mut answers).await;
            assert_eq!(code, 1, "a cancel, which is the portal's 1");
            assert!(results.is_empty(), "and nothing picked");
            assert!(portal.waiting().is_none(), "a cancel ends the request");
        }

        #[tokio::test]
        async fn records_what_each_call_asked_for() {
            let Some((_bus, portal, _client, proxy)) = pair().await else {
                eprintln!("no dbus-daemon here; skipping");
                return;
            };
            let save = ask(&proxy, "SaveFile", &[]).await;
            assert!(portal.waiting().expect("waiting").save);
            let several = ask(&proxy, "OpenFile", &[("multiple", true)]).await;
            let waiting = portal.waiting().expect("waiting");
            assert!(!waiting.save);
            assert!(waiting.multiple);
            assert_ne!(save, several, "each request gets its own path");
            ask(&proxy, "OpenFile", &[("directory", true)]).await;
            assert!(portal.waiting().expect("waiting").folders);
            let version: u32 = proxy.get_property("version").await.expect("version");
            assert_eq!(version, 3);
        }

        #[tokio::test]
        async fn refuses_an_answer_the_chooser_did_not_ask_for() {
            let Some((_bus, portal, _client, proxy)) = pair().await else {
                eprintln!("no dbus-daemon here; skipping");
                return;
            };
            let token = ask(&proxy, "OpenFile", &[]).await;
            let two = vec![
                std::env::temp_dir().join("ocu-a.txt"),
                std::env::temp_dir().join("ocu-b.txt"),
            ];
            assert!(portal.answer(&token, &two).is_err());
            assert_eq!(portal.waiting().map(|w| w.token), Some(token));
        }

        /// A call the portal does not answer is forwarded to the user's bus
        /// and its reply comes back: here, a stub `Ping` on a forwarded name.
        #[tokio::test]
        async fn forwards_an_unknown_call_to_the_real_bus() {
            // The stub "real" bus, with a service under a forwarded name.
            let Ok(real) = Bus::start() else {
                eprintln!("no dbus-daemon here; skipping");
                return;
            };
            let service = zbus::connection::Builder::address(real.address())
                .unwrap()
                .name("org.freedesktop.Notifications")
                .unwrap()
                .serve_at("/org/freedesktop/Notifications", Stub)
                .unwrap()
                .build()
                .await
                .expect("the stub service");
            let _ = &service;

            let Some((_bus, _portal, client, _proxy)) =
                pair_with_upstream(Some(real.address().to_string())).await
            else {
                return;
            };
            let notifications = zbus::Proxy::new(
                &client,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.example.Stub",
            )
            .await
            .expect("a proxy to the forwarded name");
            let echoed: String = within(notifications.call("Echo", &("hi",)))
                .await
                .expect("the forwarded call's reply");
            assert_eq!(echoed, "hi", "the real service answered through the bridge");
        }

        /// A call with no real bus to forward to fails cleanly rather than
        /// hanging the app.
        #[tokio::test]
        async fn a_call_with_nowhere_to_forward_errors() {
            let Some((_bus, _portal, client, _proxy)) = pair().await else {
                eprintln!("no dbus-daemon here; skipping");
                return;
            };
            let stub = zbus::Proxy::new(
                &client,
                "org.freedesktop.secrets",
                "/org/freedesktop/secrets",
                "org.example.Stub",
            )
            .await
            .expect("a proxy");
            let answer: zbus::Result<String> = within(stub.call("Echo", &("hi",))).await;
            assert!(
                answer.is_err(),
                "no bus to forward to is an error, not a hang"
            );
        }

        struct Stub;

        #[zbus::interface(name = "org.example.Stub")]
        impl Stub {
            async fn echo(&self, text: String) -> String {
                text
            }
        }
    }
}
