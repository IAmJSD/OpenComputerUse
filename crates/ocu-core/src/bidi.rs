//! Firefox's WebDriver BiDi, for answering a page's file chooser without
//! the open panel showing. Firefox started with `--remote-debugging-port=0`
//! picks a port and writes it to `WebDriverBiDiServer.json` in its profile.
//! A session that dismisses file prompts keeps a page's file picker from
//! showing: Firefox sends `input.fileDialogOpened`, and `input.setFiles`
//! answers it.
//!
//! Firefox takes one WebDriver session at a time, and ours is made as the
//! browser starts, so no other process on the machine can drive it through
//! the port after that.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};
use tungstenite::Message;

use crate::pages::PageFiles;

const SERVER_FILE: &str = "WebDriverBiDiServer.json";

struct Chooser {
    context: String,
    element: Value,
    multiple: bool,
}

struct Inner {
    outgoing: Mutex<mpsc::Sender<String>>,
    next: AtomicU64,
    replies: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    chooser: Mutex<Option<Chooser>>,
    alive: AtomicBool,
}

#[derive(Clone)]
pub struct Bidi(Arc<Inner>);

impl Bidi {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.0.replies.lock().unwrap().insert(id, tx);
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        self.0
            .outgoing
            .lock()
            .unwrap()
            .send(msg)
            .map_err(|_| anyhow!("the link to Firefox is closed"))?;
        let reply = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow!("Firefox did not answer {method}"))?;
        if reply["type"] == "error" {
            bail!(
                "{method}: {}",
                reply["message"].as_str().unwrap_or("failed")
            );
        }
        Ok(reply["result"].clone())
    }

    fn handle(inner: &Inner, msg: Value) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if let Some(tx) = inner.replies.lock().unwrap().remove(&id) {
                let _ = tx.send(msg);
            }
            return;
        }
        let params = &msg["params"];
        match msg["method"].as_str().unwrap_or_default() {
            "input.fileDialogOpened" => {
                let (Some(context), Some(element)) =
                    (params["context"].as_str(), params.get("element"))
                else {
                    // A picker without an input element (showPicker on
                    // nothing we can name) cannot be answered.
                    return;
                };
                log::info!("a Firefox page opened a file chooser");
                *inner.chooser.lock().unwrap() = Some(Chooser {
                    context: context.to_string(),
                    element: element.clone(),
                    multiple: params["multiple"].as_bool().unwrap_or(false),
                });
            }
            "browsingContext.contextDestroyed" => {
                let gone = params["context"].as_str();
                let mut chooser = inner.chooser.lock().unwrap();
                if chooser.as_ref().map(|c| c.context.as_str()) == gone {
                    *chooser = None;
                }
            }
            _ => {}
        }
    }
}

impl PageFiles for Bidi {
    fn is_alive(&self) -> bool {
        self.0.alive.load(Ordering::Relaxed)
    }

    fn waiting(&self) -> Option<bool> {
        self.0.chooser.lock().unwrap().as_ref().map(|c| c.multiple)
    }

    fn answer(&self, paths: &[PathBuf]) -> Result<()> {
        let (context, element, multiple) = {
            let chooser = self.0.chooser.lock().unwrap();
            let c = chooser
                .as_ref()
                .ok_or_else(|| anyhow!("no page is asking for a file"))?;
            (c.context.clone(), c.element.clone(), c.multiple)
        };
        crate::pages::check(paths, multiple)?;
        let files: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into()).collect();
        self.call(
            "input.setFiles",
            json!({ "context": context, "element": element, "files": files }),
        )?;
        let mut chooser = self.0.chooser.lock().unwrap();
        if chooser.as_ref().is_some_and(|c| c.element == element) {
            *chooser = None;
        }
        Ok(())
    }
}

/// The profile folder named in Firefox's arguments (`-profile <dir>`).
pub fn profile_dir(args: &[String]) -> Option<PathBuf> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let bare = a.trim_start_matches('-');
        if bare == "profile" {
            return it.next().map(PathBuf::from);
        }
        if let Some(dir) = bare.strip_prefix("profile=") {
            return Some(PathBuf::from(dir));
        }
    }
    None
}

/// The argument that makes Firefox listen for BiDi, on a port it picks.
pub const ARG: &str = "--remote-debugging-port=0";

/// Forgets the port of an earlier run, before Firefox starts.
pub fn clear(profile: &Path) {
    let _ = std::fs::remove_file(profile.join(SERVER_FILE));
}

/// Connects to the Firefox using `profile`, once it says where it listens,
/// and opens the session that takes its pages' file choosers.
pub fn connect(profile: &Path, wait: Duration) -> Result<Bidi> {
    let deadline = Instant::now() + wait;
    let port = loop {
        let port = std::fs::read(profile.join(SERVER_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["ws_port"].as_u64());
        if let Some(p) = port {
            break p;
        }
        if Instant::now() > deadline {
            bail!("Firefox did not start its WebDriver BiDi server");
        }
        sleep(Duration::from_millis(150));
    };
    let stream = TcpStream::connect(("127.0.0.1", port as u16))
        .with_context(|| format!("connecting to Firefox on port {port}"))?;
    let (mut ws, _) = tungstenite::client(format!("ws://127.0.0.1:{port}/session"), stream)
        .map_err(|e| anyhow!("Firefox refused the WebDriver BiDi connection: {e}"))?;
    // Short reads, so one thread both sends and receives.
    ws.get_ref()
        .set_read_timeout(Some(Duration::from_millis(30)))?;
    let (tx, rx) = mpsc::channel::<String>();
    let inner = Arc::new(Inner {
        outgoing: Mutex::new(tx),
        next: AtomicU64::new(1),
        replies: Mutex::new(HashMap::new()),
        chooser: Mutex::new(None),
        alive: AtomicBool::new(true),
    });
    let reader = inner.clone();
    std::thread::Builder::new()
        .name("bidi".into())
        .spawn(move || {
            'run: loop {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => {
                            if ws.send(Message::text(msg)).is_err() {
                                break 'run;
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => break 'run,
                    }
                }
                match ws.read() {
                    Ok(Message::Text(t)) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            Bidi::handle(&reader, v);
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
            reader.alive.store(false, Ordering::Relaxed);
            reader.replies.lock().unwrap().clear();
        })?;
    let bidi = Bidi(inner);
    // Prompts the page raises (alerts, confirms) are left as they are, for
    // the caller to see. File pickers are dismissed rather than shown: the
    // page still waits on its input, which `input.setFiles` fills (after a
    // `cancel` event on it).
    bidi.call(
        "session.new",
        json!({ "capabilities": { "alwaysMatch": {
            "unhandledPromptBehavior": { "default": "ignore", "file": "dismiss" }
        } } }),
    )
    .context("opening a WebDriver BiDi session")?;
    bidi.call(
        "session.subscribe",
        json!({ "events": ["input.fileDialogOpened", "browsingContext.contextDestroyed"] }),
    )?;
    Ok(bidi)
}
