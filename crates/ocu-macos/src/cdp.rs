//! Chromium's DevTools protocol, for answering a page's file chooser
//! without the open panel ever showing. The browser is started with
//! `--remote-debugging-pipe`: it reads commands on fd 3 and writes replies
//! and events on fd 4, NUL-terminated JSON. A pipe rather than a port, so
//! only we can drive the browser, not any other process on the machine.
//!
//! Every page (and out-of-process frame) is asked to intercept its file
//! choosers: an `<input type=file>` then emits `Page.fileChooserOpened`
//! instead of opening the panel, and `DOM.setFileInputFiles` answers it.
//! Pickers this does not cover (`showOpenFilePicker` and kin) still open
//! the panel, which [`crate::panel`] answers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};

use ocu_core::pages::PageFiles;

/// What a page calls (through [`PICKERS`]) before and after a File System
/// Access picker.
const BINDING: &str = "__ocuFilePicker";

/// Wraps the File System Access pickers, which Chrome aborts while file
/// choosers are intercepted: each call first has us stop intercepting for
/// the page, and waits for that, so the picker opens its panel (answered
/// through [`crate::panel`]); we intercept again once it settles. A second
/// timer lets the picker go ahead should we not answer.
const PICKERS: &str = r#"(() => {
  if (window.__ocuResume !== undefined || typeof __ocuFilePicker !== 'function') return;
  const waiting = [];
  Object.defineProperty(window, '__ocuResume', { value: () => waiting.splice(0).forEach(r => r()) });
  for (const name of ['showOpenFilePicker', 'showSaveFilePicker', 'showDirectoryPicker']) {
    const original = window[name];
    if (typeof original !== 'function') continue;
    window[name] = async function (...args) {
      await new Promise(resolve => {
        waiting.push(resolve);
        setTimeout(resolve, 1000);
        __ocuFilePicker('open');
      });
      try { return await original.apply(this, args); }
      finally { __ocuFilePicker('done'); }
    };
  }
})()"#;

/// A file chooser a page opened and is waiting on.
#[derive(Clone, Debug)]
struct Chooser {
    session: String,
    node: i64,
    multiple: bool,
}

struct Inner {
    writer: Mutex<File>,
    next: AtomicU64,
    replies: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    chooser: Mutex<Option<Chooser>>,
    alive: AtomicBool,
}

impl Inner {
    /// Sends a command; its reply goes to `reply` when given.
    fn send_to(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        reply: Option<mpsc::Sender<Value>>,
    ) -> Result<()> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if let Some(tx) = reply {
            self.replies.lock().unwrap().insert(id, tx);
        }
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(0);
        self.writer
            .lock()
            .unwrap()
            .write_all(&bytes)
            .context("writing to the browser's debugging pipe")
    }

    fn send(&self, method: &str, params: Value, session: Option<&str>) -> Result<()> {
        self.send_to(method, params, session, None)
    }

    /// Readies a page or frame: file choosers come to us, and frames in
    /// other processes are attached as they appear.
    fn prepare(&self, session: &str) {
        let s = Some(session);
        let _ = self.send("Page.enable", json!({}), s);
        let _ = self.send("Runtime.enable", json!({}), s);
        let _ = self.send("Runtime.addBinding", json!({ "name": BINDING }), s);
        let _ = self.send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": PICKERS }),
            s,
        );
        let _ = self.send("Runtime.evaluate", json!({ "expression": PICKERS }), s);
        self.intercept(session, true);
        let _ = self.send(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
            Some(session),
        );
    }

    fn intercept(&self, session: &str, enabled: bool) {
        let _ = self.send(
            "Page.setInterceptFileChooserDialog",
            json!({ "enabled": enabled }),
            Some(session),
        );
    }

    fn handle(&self, msg: Value) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if let Some(tx) = self.replies.lock().unwrap().remove(&id) {
                let _ = tx.send(msg);
            }
            return;
        }
        let params = &msg["params"];
        match msg["method"].as_str().unwrap_or_default() {
            "Target.targetCreated" => {
                let info = &params["targetInfo"];
                if info["type"] == "page" {
                    let _ = self.send(
                        "Target.attachToTarget",
                        json!({ "targetId": info["targetId"], "flatten": true }),
                        None,
                    );
                }
            }
            "Target.attachedToTarget" => {
                if let Some(session) = params["sessionId"].as_str() {
                    self.prepare(session);
                }
            }
            "Page.fileChooserOpened" => {
                let (Some(session), Some(node)) =
                    (msg["sessionId"].as_str(), params["backendNodeId"].as_i64())
                else {
                    return;
                };
                log::info!("a page opened a file chooser ({})", params["mode"]);
                *self.chooser.lock().unwrap() = Some(Chooser {
                    session: session.to_string(),
                    node,
                    multiple: params["mode"] == "selectMultiple",
                });
            }
            "Runtime.bindingCalled" if params["name"] == BINDING => {
                let Some(session) = msg["sessionId"].as_str() else {
                    return;
                };
                if params["payload"] == "open" {
                    // In order on the session: interception is off before
                    // the page is told to go ahead.
                    self.intercept(session, false);
                    let _ = self.send(
                        "Runtime.evaluate",
                        json!({
                            "expression": "window.__ocuResume && window.__ocuResume()",
                            "contextId": params["executionContextId"],
                        }),
                        Some(session),
                    );
                } else {
                    self.intercept(session, true);
                }
            }
            "Target.detachedFromTarget" => {
                let gone = params["sessionId"].as_str();
                let mut chooser = self.chooser.lock().unwrap();
                if chooser.as_ref().map(|c| c.session.as_str()) == gone {
                    *chooser = None;
                }
            }
            _ => {}
        }
    }
}

/// The link to one browser.
#[derive(Clone)]
pub struct Cdp(Arc<Inner>);

impl Cdp {
    /// Sends a command and waits for its result.
    fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.0.send_to(method, params, session, Some(tx))?;
        let reply = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow!("the browser did not answer {method}"))?;
        if let Some(e) = reply.get("error") {
            bail!("{method}: {}", e["message"].as_str().unwrap_or("failed"));
        }
        Ok(reply["result"].clone())
    }
}

impl PageFiles for Cdp {
    fn is_alive(&self) -> bool {
        self.0.alive.load(Ordering::Relaxed)
    }

    fn waiting(&self) -> Option<bool> {
        self.0.chooser.lock().unwrap().as_ref().map(|c| c.multiple)
    }

    fn answer(&self, paths: &[PathBuf]) -> Result<()> {
        let chooser = self
            .0
            .chooser
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("no page is asking for a file"))?;
        ocu_core::pages::check(paths, chooser.multiple)?;
        let files: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into()).collect();
        self.call(
            "DOM.setFileInputFiles",
            json!({ "files": files, "backendNodeId": chooser.node }),
            Some(&chooser.session),
        )?;
        let mut pending = self.0.chooser.lock().unwrap();
        if pending.as_ref().map(|c| c.node) == Some(chooser.node) {
            *pending = None;
        }
        Ok(())
    }
}

/// A pipe's two ends, closed on exec unless handed to the child.
fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!("pipe: {}", std::io::Error::last_os_error());
    }
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Starts a Chromium browser's executable with a debugging pipe and
/// readies every page it opens. Returns once the browser has answered.
pub fn spawn(exe: &Path, spec: &ocu_core::LaunchSpec) -> Result<(Child, Cdp)> {
    use std::os::fd::AsRawFd as _;
    let (to_read, to_write) = pipe()?; // the browser reads commands here
    let (from_read, from_write) = pipe()?; // and writes replies here
    let (child_in, child_out) = (to_read.as_raw_fd(), from_write.as_raw_fd());
    let mut cmd = Command::new(exe);
    cmd.arg("--remote-debugging-pipe")
        .args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    unsafe {
        cmd.pre_exec(move || {
            // Duplicated first, so ends already numbered 3 or 4 survive;
            // dup2 leaves the new descriptors open across exec.
            let (a, b) = (libc::dup(child_in), libc::dup(child_out));
            if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    drop((to_read, from_write));
    let inner = Arc::new(Inner {
        writer: Mutex::new(File::from(to_write)),
        next: AtomicU64::new(1),
        replies: Mutex::new(HashMap::new()),
        chooser: Mutex::new(None),
        alive: AtomicBool::new(true),
    });
    let reader = inner.clone();
    std::thread::Builder::new()
        .name("cdp".into())
        .spawn(move || {
            let mut file = File::from(from_read);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let n = match file.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                while let Some(end) = buf.iter().position(|&b| b == 0) {
                    let msg: Vec<u8> = buf.drain(..=end).collect();
                    if let Ok(v) = serde_json::from_slice::<Value>(&msg[..msg.len() - 1]) {
                        reader.handle(v);
                    }
                }
            }
            reader.alive.store(false, Ordering::Relaxed);
            reader.replies.lock().unwrap().clear();
        })?;
    let cdp = Cdp(inner);
    cdp.call("Browser.getVersion", json!({}), None)
        .context("the browser did not open its debugging pipe")?;
    cdp.call(
        "Target.setDiscoverTargets",
        json!({ "discover": true }),
        None,
    )?;
    Ok((child, cdp))
}
