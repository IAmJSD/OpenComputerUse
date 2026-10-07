//! The optional HTTP server: the computer-use tools for other devices, each
//! authenticating with its own key. Two ways in:
//!
//! - `POST /v1/tools/<tool>` with the arguments as JSON, answered with the
//!   same `{content, isError}` an MCP tool call gives (`GET /v1/tools`
//!   lists them). Easy to drive with curl, which is what the skill does.
//! - `POST /mcp`: MCP's streamable HTTP transport, answered with plain JSON
//!   (no streams), for clients that add it as an MCP server.
//!
//! Device management is not here: keys are made and removed only on this
//! computer, in the app or its local MCP server.
//!
//! Each device's requests run in order on a worker thread of its own,
//! which owns the device's sessions. The thread outlives requests, so on
//! Linux the apps it starts (which die with the thread that started them)
//! last as long as the device is allowed in; removing the device or
//! regenerating its key ends the worker and its sessions.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use ocu_core::{Client, Service};

use super::devices::Devices;

/// Largest request body read: tool arguments are small.
const MAX_BODY: u64 = 1 << 20;

type Job = Box<dyn FnOnce(&mut Client) + Send>;

/// A device's worker thread, fed jobs through a channel.
struct Worker {
    tx: mpsc::Sender<Job>,
}

impl Worker {
    fn spawn(service: Arc<Service>, name: String) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let _ = std::thread::Builder::new()
            .name(format!("http-{name}"))
            .spawn(move || {
                let mut client = service.client();
                while let Ok(job) = rx.recv() {
                    job(&mut client);
                }
                // The client drops here, ending the device's sessions.
            });
        Self { tx }
    }
}

struct State {
    service: Arc<Service>,
    devices: Mutex<(Devices, Option<SystemTime>)>,
    /// Keyed by device id and key hash, so a regenerated key gets a fresh
    /// worker and the old one (and its sessions) goes.
    workers: Mutex<HashMap<(String, String), Worker>>,
}

impl State {
    /// The device a key belongs to, rereading devices.json when it changed
    /// and retiring workers whose device or key is gone.
    fn authenticate(&self, key: &str) -> Option<(String, String, String)> {
        let mtime = std::fs::metadata(super::devices::path())
            .and_then(|m| m.modified())
            .ok();
        let mut devices = self.devices.lock().unwrap();
        if devices.1 != mtime || mtime.is_none() {
            *devices = (Devices::load(), mtime);
            let valid: Vec<(String, String)> = devices
                .0
                .devices
                .iter()
                .map(|d| (d.id.clone(), d.key_hash.clone()))
                .collect();
            self.workers
                .lock()
                .unwrap()
                .retain(|k, _| valid.contains(k));
        }
        devices
            .0
            .authenticate(key)
            .map(|d| (d.id.clone(), d.key_hash.clone(), d.name.clone()))
    }

    fn worker_run<R: Send + 'static>(
        &self,
        device: (String, String, String),
        f: impl FnOnce(&mut Client) -> R + Send + 'static,
    ) -> Result<R> {
        let (id, hash, name) = device;
        let mut workers = self.workers.lock().unwrap();
        let worker = workers
            .entry((id, hash))
            .or_insert_with(|| Worker::spawn(self.service.clone(), name));
        // Send under the lock (cheap), wait outside it.
        let (tx, rx) = mpsc::channel();
        worker
            .tx
            .send(Box::new(move |c: &mut Client| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| anyhow!("the device's worker stopped"))?;
        drop(workers);
        rx.recv()
            .map_err(|_| anyhow!("the device's worker stopped"))
    }
}

pub struct HttpServer {
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
    pub addr: String,
}

impl HttpServer {
    pub fn start(service: Arc<Service>, bind: &str, port: u16) -> Result<Self> {
        let addr = format!("{bind}:{port}");
        let server =
            Arc::new(Server::http(&addr).map_err(|e| anyhow!("can't listen on {addr}: {e}"))?);
        let state = Arc::new(State {
            service,
            devices: Mutex::new((Devices::default(), None)),
            workers: Mutex::new(HashMap::new()),
        });
        let s = server.clone();
        let thread = std::thread::Builder::new()
            .name("http".into())
            .spawn(move || {
                for request in s.incoming_requests() {
                    let state = state.clone();
                    let _ = std::thread::Builder::new()
                        .name("http-request".into())
                        .spawn(move || handle(&state, request));
                }
                // Unblocked: drop the workers, ending every device's sessions.
                state.workers.lock().unwrap().clear();
            })?;
        log::info!("HTTP server listening on {addr}");
        Ok(Self {
            server,
            thread: Some(thread),
            addr,
        })
    }

    /// Serves until the process ends (`opencomputeruse serve`).
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        log::info!("HTTP server on {} stopped", self.addr);
    }
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(serde_json::to_vec(body).unwrap_or_default())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn bearer(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| {
            h.value
                .as_str()
                .strip_prefix("Bearer ")
                .map(|k| k.trim().to_string())
        })
}

fn read_json(request: &mut Request) -> Result<Value> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)?;
    anyhow::ensure!(
        body.len() as u64 <= MAX_BODY,
        "the request body is too large"
    );
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(&body).context("the request body is not JSON")
}

fn handle(state: &State, mut request: Request) {
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("/").to_string();

    if path == "/health" {
        let _ = request.respond(json_response(200, &json!({ "ok": true })));
        return;
    }
    let Some(device) = bearer(&request).and_then(|k| state.authenticate(&k)) else {
        let resp = json_response(
            401,
            &json!({ "error": "missing or unknown key; send Authorization: Bearer <key>" }),
        )
        .with_header(Header::from_bytes("WWW-Authenticate", "Bearer").unwrap());
        let _ = request.respond(resp);
        return;
    };

    let response = match (&method, path.as_str()) {
        (Method::Get, "/v1/tools") => json_response(200, &json!({ "tools": crate::tools::list() })),
        (Method::Post, p) if p.starts_with("/v1/tools/") => {
            let name = p.trim_start_matches("/v1/tools/").to_string();
            match read_json(&mut request) {
                Err(e) => json_response(400, &json!({ "error": format!("{e:#}") })),
                Ok(args) => match state.worker_run(device, move |client| {
                    crate::mcp::call_tool(client, &name, &args)
                }) {
                    Ok(result) => json_response(200, &result),
                    Err(e) => json_response(500, &json!({ "error": format!("{e:#}") })),
                },
            }
        }
        (Method::Post, "/mcp") => match read_json(&mut request) {
            Err(e) => json_response(
                400,
                &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("{e:#}") } }),
            ),
            Ok(msg) => match state.worker_run(device, move |client| {
                crate::mcp::dispatch_value(client, &msg, false)
            }) {
                Ok(Some(reply)) => json_response(200, &reply),
                // Notifications and responses get no body.
                Ok(None) => Response::from_data(Vec::new()).with_status_code(202),
                Err(e) => json_response(500, &json!({ "error": format!("{e:#}") })),
            },
        },
        (Method::Get, "/mcp") => json_response(
            405,
            &json!({ "error": "this server does not stream; POST JSON-RPC to /mcp" }),
        ),
        _ => json_response(
            404,
            &json!({ "error": format!("no route {method} {path}") }),
        ),
    };
    let _ = request.respond(response);
}
