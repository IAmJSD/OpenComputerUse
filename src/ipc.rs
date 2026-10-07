//! Newline-delimited JSON over a Unix socket between the MCP server and the
//! macOS agent app, which holds the sessions (and the permissions).

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use ocu_core::{Handler, Request, Response, Service};

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    id: u64,
    #[serde(flatten)]
    body: T,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Ok(Response),
    Error(String),
}

trait Stream: std::io::Read + Write + Send {}
impl<T: std::io::Read + Write + Send> Stream for T {}

/// The agent went away. When it was gone `before_sending`, it never saw
/// the request, which is then safe to send again to a new agent.
#[derive(Debug)]
pub struct Disconnected {
    pub before_sending: bool,
}

impl std::fmt::Display for Disconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the OpenComputerUse app quit or restarted; its sessions ended with it")
    }
}

impl std::error::Error for Disconnected {}

/// The client half: a [`Handler`] that forwards over the connection.
pub struct Remote {
    reader: BufReader<Box<dyn Stream>>,
    next: u64,
}

impl Remote {
    pub fn connect(path: &std::path::Path) -> Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        Ok(Self { reader: BufReader::new(Box::new(stream)), next: 1 })
    }
}

impl Handler for Remote {
    fn handle(&mut self, req: Request) -> Result<Response> {
        let id = self.next;
        self.next += 1;
        let mut line = serde_json::to_vec(&Envelope { id, body: req })?;
        line.push(b'\n');
        let stream = self.reader.get_mut();
        if stream.write_all(&line).and_then(|_| stream.flush()).is_err() {
            return Err(Disconnected { before_sending: true }.into());
        }
        let mut line = String::new();
        if self.reader.read_line(&mut line).unwrap_or(0) == 0 {
            return Err(Disconnected { before_sending: false }.into());
        }
        let reply: Envelope<Reply> = serde_json::from_str(&line).context("reading the agent's reply")?;
        match reply.body {
            Reply::Ok(r) => Ok(r),
            Reply::Error(e) => Err(anyhow!(e)),
        }
    }
}

/// Serves one connection until it closes. The connection's sessions end
/// with it, because its [`ocu_core::Client`] drops here.
fn serve_connection(service: Arc<Service>, stream: Box<dyn Stream>) {
    let mut client = service.client();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let (id, reply) = match serde_json::from_str::<Envelope<Request>>(&line) {
            Ok(env) => (env.id, match client.handle(env.body) {
                Ok(r) => Reply::Ok(r),
                Err(e) => Reply::Error(format!("{e:#}")),
            }),
            Err(e) => (0, Reply::Error(format!("bad request: {e}"))),
        };
        let stream = reader.get_mut();
        let ok = serde_json::to_writer(&mut *stream, &Envelope { id, body: reply }).is_ok()
            && stream.write_all(b"\n").is_ok()
            && stream.flush().is_ok();
        if !ok {
            break;
        }
    }
    log::debug!("client {} disconnected", client.id());
}

/// Listens on `path` (replacing a stale socket) and serves each connection
/// on its own thread.
pub fn listen(service: Arc<Service>, path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another agent is already listening on {}", path.display());
        }
        let _ = std::fs::remove_file(path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new().name("ipc-listener".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let service = service.clone();
            let _ = std::thread::Builder::new()
                .name("ipc-client".into())
                .spawn(move || serve_connection(service, Box::new(stream)));
        }
    })?;
    Ok(())
}
