//! The live view: an HTTP server, off by default, through which a person
//! watches a session's window and takes over from the agent, and an app
//! can list, start and end sessions. Every request needs the view token.
//! The README's "Live view" section is the protocol's reference.
//!
//! Each WebSocket gets a thread of its own, which sends pictures when they
//! change and passes input on. Sessions started here run on one long-lived
//! worker thread, since on Linux an app dies with the thread that started
//! it.

pub mod protocol;

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

use ocu_core::image::encode_png;
use ocu_core::{Client, Frame, Handler, Request, Response, Service, SessionInfo, Size, Viewer};

use protocol::{changed, encode, ClientMessage, Format, Keys};

/// Set once the view server is listening, for `start_session`'s answer.
static RUNNING: OnceLock<()> = OnceLock::new();

/// Where a session's live view is, when the view server runs and the
/// session's backend can be watched.
pub fn path_for(info: &SessionInfo) -> Option<String> {
    (RUNNING.get().is_some() && info.backend.starts_with("linux"))
        .then(|| format!("/sessions/{}/view", info.id))
}

/// Largest request head and body read.
const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 1 << 20;

/// The view server's settings: `--view-listen` and `--view-token`, or
/// `OCU_VIEW_LISTEN` and `OCU_VIEW_TOKEN`.
#[derive(Debug, Default, PartialEq)]
pub struct ViewFlags {
    pub listen: Option<String>,
    pub token: Option<String>,
}

impl ViewFlags {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut flags = Self::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let (name, inline) = match a.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (a.as_str(), None),
            };
            let slot = match name {
                "--view-listen" => &mut flags.listen,
                "--view-token" => &mut flags.token,
                _ => continue,
            };
            let value = inline
                .or_else(|| it.next().cloned())
                .filter(|v| !v.is_empty() && !v.starts_with("--"))
                .ok_or_else(|| anyhow!("{name} takes a value"))?;
            *slot = Some(value);
        }
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        flags.listen = flags.listen.or_else(|| env("OCU_VIEW_LISTEN"));
        flags.token = flags.token.or_else(|| env("OCU_VIEW_TOKEN"));
        Ok(flags)
    }

    /// The address to listen on and the token, when the server is wanted.
    pub fn resolve(self, fallback_token: Option<&str>) -> Result<Option<(SocketAddr, String)>> {
        let Some(listen) = self.listen else {
            return Ok(None);
        };
        let addr: SocketAddr = listen
            .parse()
            .map_err(|_| anyhow!("--view-listen takes an address and port, like 0.0.0.0:7900"))?;
        let token = self
            .token
            .or_else(|| fallback_token.map(str::to_string))
            .ok_or_else(|| {
                anyhow!("the view server needs a token: --view-token or OCU_VIEW_TOKEN")
            })?;
        check_token(&token)?;
        Ok(Some((addr, token)))
    }
}

/// Tokens are at least 16 characters a client can send in a header or a
/// query string unescaped.
pub fn check_token(token: &str) -> Result<()> {
    if token.len() < 16
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
    {
        bail!("tokens are at least 16 letters, digits or -._~");
    }
    Ok(())
}

/// Compares without leaking where two strings first differ.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

type Job = Box<dyn FnOnce(&mut Client) + Send>;

struct Server {
    service: Arc<Service>,
    token: String,
    /// Starts and ends sessions, on a thread that outlives requests.
    worker: std::sync::Mutex<mpsc::Sender<Job>>,
}

impl Server {
    fn on_worker<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Client) -> R + Send + 'static,
    ) -> Result<R> {
        let (tx, rx) = mpsc::channel();
        self.worker
            .lock()
            .unwrap()
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| anyhow!("the view server's worker stopped"))?;
        rx.recv()
            .map_err(|_| anyhow!("the view server's worker stopped"))
    }
}

/// Starts the view server on `addr`, sharing `service`'s sessions with the
/// other clients (see [`Service::share_sessions`]).
pub fn start(service: Arc<Service>, addr: SocketAddr, token: String) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("can't listen on {addr}"))?;
    service.share_sessions();
    let (tx, rx) = mpsc::channel::<Job>();
    let worker_service = service.clone();
    std::thread::Builder::new()
        .name("view-worker".into())
        .spawn(move || {
            let mut client = worker_service.client();
            while let Ok(job) = rx.recv() {
                job(&mut client);
            }
        })?;
    let server = Arc::new(Server {
        service,
        token,
        worker: std::sync::Mutex::new(tx),
    });
    std::thread::Builder::new()
        .name("view".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let server = server.clone();
                let _ = std::thread::Builder::new()
                    .name("view-conn".into())
                    .spawn(move || {
                        if let Err(e) = connection(&server, stream) {
                            log::debug!("view connection: {e:#}");
                        }
                    });
            }
        })?;
    let _ = RUNNING.set(());
    log::warn!("live view listening on {addr}");
    Ok(())
}

/// One request, as much as a route needs.
struct Head {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// Reads a request's head, returning it and any bytes read past it.
fn read_head(stream: &mut TcpStream) -> Result<(Head, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            bail!("closed before a request");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(&buf[..end])?;
    let target = req.path.unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let head = Head {
        method: req.method.unwrap_or("GET").to_string(),
        path: path.to_string(),
        query: query
            .split('&')
            .filter_map(|kv| kv.split_once('=').or(Some((kv, ""))))
            .filter(|(k, _)| !k.is_empty())
            .map(|(k, v)| (k.to_string(), percent_decode(v)))
            .collect(),
        headers: req
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_ascii_lowercase(),
                    String::from_utf8_lossy(h.value).into_owned(),
                )
            })
            .collect(),
    };
    Ok((head, buf[end..].to_vec()))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(a), Some(b)) => {
                    out.push((a * 16 + b) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\n\
         Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nConnection: close\r\n",
        body.len()
    );
    if status == 401 {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn respond_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<()> {
    respond(
        stream,
        status,
        "application/json",
        &serde_json::to_vec(body).unwrap_or_default(),
    )
}

fn connection(server: &Server, mut stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let (head, mut rest) = read_head(&mut stream)?;
    if head.method == "OPTIONS" {
        return respond(&mut stream, 204, "text/plain", b"");
    }
    if head.path == "/health" {
        return respond_json(&mut stream, 200, &json!({ "ok": true }));
    }
    let token = head
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .or(head.query.get("token").map(String::as_str));
    if !token.is_some_and(|t| same(t, &server.token)) {
        return respond_json(
            &mut stream,
            401,
            &json!({ "error": "missing or wrong token; send Authorization: Bearer <token> or ?token=<token>" }),
        );
    }
    let parts: Vec<&str> = head.path.trim_matches('/').split('/').collect();
    let result = match (head.method.as_str(), parts.as_slice()) {
        ("GET", ["sessions", id, "view"]) => {
            let viewer = server.service.viewer(id);
            let info = server.service.session_info(id);
            match (viewer, info) {
                (Ok(viewer), Some(info)) => {
                    return watch(server, stream, &head, rest, viewer, info)
                }
                (Err(e), _) => Err((404, format!("{e:#}"))),
                (_, None) => Err((404, format!("no session {id}"))),
            }
        }
        ("GET", ["sessions"]) => Ok((200, json!({ "sessions": sessions(server) }))),
        ("POST", ["sessions"]) => {
            let length: usize = head
                .header("content-length")
                .and_then(|l| l.parse().ok())
                .unwrap_or(0);
            if length > MAX_BODY {
                return respond_json(&mut stream, 400, &json!({ "error": "body too large" }));
            }
            while rest.len() < length {
                let mut chunk = vec![0u8; length - rest.len()];
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                rest.extend_from_slice(&chunk[..n]);
            }
            start_session(server, &rest[..length.min(rest.len())])
        }
        ("GET", ["sessions", id]) => match server.service.session_info(id) {
            Some(info) => Ok((200, describe(server, info))),
            None => Err((404, format!("no session {id}"))),
        },
        ("DELETE", ["sessions", id]) => {
            let id = id.to_string();
            let service = server.service.clone();
            match server.on_worker(move |_| service.end(&id)) {
                Ok(Ok(())) => Ok((200, json!({ "ok": true }))),
                Ok(Err(e)) => Err((404, format!("{e:#}"))),
                Err(e) => Err((500, format!("{e:#}"))),
            }
        }
        ("GET", ["sessions", id, "screenshot"]) => {
            let window = head.query.get("window").and_then(|w| w.parse().ok());
            let picture = server
                .service
                .viewer(id)
                .and_then(|v| v.frame(window))
                .and_then(|f| encode_png(f.width, f.height, f.stride, &f.data, f.order));
            return match picture {
                Ok(png) => respond(&mut stream, 200, "image/png", &png),
                Err(e) => respond_json(&mut stream, 404, &json!({ "error": format!("{e:#}") })),
            };
        }
        _ => Err((404, format!("no route {} {}", head.method, head.path))),
    };
    match result {
        Ok((status, body)) => respond_json(&mut stream, status, &body),
        Err((status, message)) => respond_json(&mut stream, status, &json!({ "error": message })),
    }
}

/// A session as the API shows it: its details, windows and view path.
fn describe(server: &Server, info: SessionInfo) -> Value {
    let windows = server
        .service
        .viewer(&info.id)
        .and_then(|v| v.windows())
        .unwrap_or_default();
    let view = path_for(&info);
    let mut v = json!({ "session_id": info.id, "session": info, "windows": windows });
    if let Some(path) = view {
        v["view_path"] = json!(path);
    }
    v
}

fn sessions(server: &Server) -> Vec<Value> {
    server
        .service
        .sessions()
        .into_iter()
        .map(|info| describe(server, info))
        .collect()
}

fn start_session(server: &Server, body: &[u8]) -> Result<(u16, Value), (u16, String)> {
    let args: Value =
        serde_json::from_slice(body).map_err(|e| (400, format!("the body is not JSON: {e}")))?;
    let spec = crate::tools::launch_spec(&args).map_err(|e| (400, format!("{e:#}")))?;
    if spec.active_window {
        return Err((400, "active_window is for the local MCP server".into()));
    }
    let reply = server
        .on_worker(move |client| client.handle(Request::StartSession(spec)))
        .map_err(|e| (500, format!("{e:#}")))?;
    match reply {
        Ok(Response::Session { info, .. }) => Ok((201, describe(server, info))),
        Ok(_) => Err((500, "unexpected reply".into())),
        Err(e) => Err((500, format!("{e:#}"))),
    }
}

/// A WebSocket watching one session: pictures out, input in, until either
/// side goes.
fn watch(
    server: &Server,
    mut stream: TcpStream,
    head: &Head,
    rest: Vec<u8>,
    viewer: Arc<dyn Viewer>,
    info: SessionInfo,
) -> Result<()> {
    let Some(key) = head.header("sec-websocket-key").filter(|_| {
        head.header("upgrade")
            .is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
    }) else {
        return respond_json(
            &mut stream,
            400,
            &json!({ "error": "this is a WebSocket; connect with Upgrade: websocket" }),
        );
    };
    let format = match head.query.get("format") {
        Some(f) => match Format::parse(f) {
            Some(f) => f,
            None => {
                return respond_json(
                    &mut stream,
                    400,
                    &json!({ "error": "format is jpeg or png" }),
                )
            }
        },
        None => Format::Jpeg,
    };
    let quality: u8 = head
        .query
        .get("quality")
        .and_then(|q| q.parse().ok())
        .unwrap_or(75)
        .clamp(1, 100);
    let mut window: Option<u64> = head.query.get("window").and_then(|w| w.parse().ok());
    // Echo our subprotocol when the browser offers it; it never has to.
    let protocol = head
        .header("sec-websocket-protocol")
        .filter(|p| p.split(',').any(|p| p.trim() == "ocu-view"))
        .map(|_| "Sec-WebSocket-Protocol: ocu-view\r\n")
        .unwrap_or("");
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n{protocol}\r\n",
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    )?;
    stream.flush()?;
    // Short reads, so one thread can both listen and send.
    stream.set_read_timeout(Some(Duration::from_millis(10)))?;
    stream.set_nodelay(true)?;
    let mut ws = WebSocket::from_partially_read(stream, rest, Role::Server, None);

    let send_json = |ws: &mut WebSocket<TcpStream>, v: Value| -> Result<()> {
        ws.send(Message::text(v.to_string()))?;
        Ok(())
    };
    let windows = viewer.windows().unwrap_or_default();
    if window.is_none_or(|id| !windows.iter().any(|w| w.id == id)) {
        window = windows.first().map(|w| w.id);
    }
    send_json(
        &mut ws,
        json!({ "type": "hello", "session": info, "windows": windows, "window": window, "format": format.name() }),
    )?;

    let mut keys = Keys::default();
    let mut last: Option<Frame> = None;
    let mut seen: Option<u64> = None;
    let mut shown = windows;
    let mut checked = Instant::now();
    let mut sent = Instant::now() - Duration::from_secs(1);
    // Wayland says when the picture changed; X is looked at ten times a
    // second.
    let interval = if viewer.generation().is_some() {
        Duration::from_millis(33)
    } else {
        Duration::from_millis(100)
    };
    let result = loop {
        // Input first: a person's click should not wait behind a picture.
        match ws.read() {
            Ok(Message::Text(text)) => {
                let reply = match serde_json::from_str::<ClientMessage>(text.as_str()) {
                    Ok(msg) => handle(&*viewer, &mut window, &mut keys, &mut last, msg),
                    Err(e) => Err(anyhow!("bad message: {e}")),
                };
                if let Err(e) = reply {
                    send_json(
                        &mut ws,
                        json!({ "type": "error", "message": format!("{e:#}") }),
                    )?;
                }
                continue;
            }
            Ok(Message::Close(_)) => break Ok(()),
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                break Ok(())
            }
            Err(e) => break Err(e.into()),
        }
        if !viewer.is_alive() || server.service.session_info(&info.id).is_none() {
            let _ = send_json(&mut ws, json!({ "type": "ended" }));
            let _ = ws.close(None);
            let _ = ws.flush();
            break Ok(());
        }
        if checked.elapsed() > Duration::from_millis(500) {
            checked = Instant::now();
            let now = viewer.windows().unwrap_or_default();
            let summary = |ws: &[ocu_core::WindowInfo]| -> Vec<(u64, String, i64, i64, i64, i64)> {
                ws.iter()
                    .map(|w| {
                        let f = w.frame;
                        (
                            w.id,
                            w.title.clone(),
                            f.x as i64,
                            f.y as i64,
                            f.width as i64,
                            f.height as i64,
                        )
                    })
                    .collect()
            };
            if summary(&now) != summary(&shown) {
                if window.is_none_or(|id| !now.iter().any(|w| w.id == id)) {
                    window = now.first().map(|w| w.id);
                    last = None;
                }
                send_json(
                    &mut ws,
                    json!({ "type": "windows", "windows": now, "window": window }),
                )?;
                shown = now;
            }
        }
        if sent.elapsed() < interval {
            continue;
        }
        let generation = viewer.generation();
        if generation.is_some() && generation == seen && last.is_some() {
            continue;
        }
        sent = Instant::now();
        let Ok(frame) = viewer.frame(window) else {
            continue;
        };
        seen = generation;
        if let Some(region) = changed(last.as_ref(), &frame) {
            let msg = encode(&frame, region, format, quality)?;
            ws.send(Message::binary(msg))?;
        }
        last = Some(frame);
    };
    for input in keys.release_all() {
        let _ = viewer.input(window, &input);
    }
    result
}

fn handle(
    viewer: &dyn Viewer,
    window: &mut Option<u64>,
    keys: &mut Keys,
    last: &mut Option<Frame>,
    msg: ClientMessage,
) -> Result<()> {
    match msg {
        ClientMessage::Pointer {
            action,
            x,
            y,
            button,
        } => viewer.input(*window, &protocol::pointer_input(action, x, y, button)?),
        ClientMessage::Wheel { x, y, dx, dy } => {
            viewer.input(*window, &ocu_core::ViewInput::Wheel { x, y, dx, dy })
        }
        ClientMessage::Key { action, code, key } => {
            for input in keys.event(action, &code, &key) {
                viewer.input(*window, &input)?;
            }
            Ok(())
        }
        ClientMessage::Text { text } => viewer.input(*window, &ocu_core::ViewInput::Text { text }),
        ClientMessage::Resize { width, height } => {
            *last = None;
            viewer.resize(Size { width, height })
        }
        ClientMessage::Window { id } => {
            if !viewer.windows()?.iter().any(|w| w.id == id) {
                bail!("the session has no window {id}");
            }
            *window = Some(id);
            *last = None;
            Ok(())
        }
        ClientMessage::Refresh => {
            *last = None;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags() {
        let f = ViewFlags::parse(&args(&[
            "mcp",
            "--view-listen",
            "0.0.0.0:7900",
            "--view-token=abcdefghijklmnop",
        ]))
        .unwrap();
        assert_eq!(f.listen.as_deref(), Some("0.0.0.0:7900"));
        let (addr, token) = f.resolve(None).unwrap().unwrap();
        assert_eq!(addr.port(), 7900);
        assert_eq!(token, "abcdefghijklmnop");
        assert!(ViewFlags::parse(&args(&["--view-listen"])).is_err());
        let no_token = ViewFlags {
            listen: Some("127.0.0.1:1".into()),
            token: None,
        };
        assert!(no_token.resolve(None).is_err());
        let fallback = ViewFlags {
            listen: Some("127.0.0.1:1".into()),
            token: None,
        };
        assert_eq!(
            fallback
                .resolve(Some("0123456789abcdef"))
                .unwrap()
                .unwrap()
                .1,
            "0123456789abcdef"
        );
        assert!(ViewFlags::default().resolve(None).unwrap().is_none());
        assert!(check_token("short").is_err());
        assert!(check_token("has spaces in it ok").is_err());
    }

    #[test]
    fn tokens_compare() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert_eq!(percent_decode("a%2Bb+c%zz"), "a+b c%zz");
    }
}
