//! The shell helper: a small Java program from OpenComputerUse's APK, run
//! on the device as adb's shell user (the way scrcpy runs its server). It
//! gives a session a private virtual display, so the app runs off the
//! phone's real screen, and reads and acts on that display's accessibility
//! tree and input, which plain adb can't do for a virtual display.
//!
//! The APK is pushed to /data/local/tmp, not installed. Requests are JSON
//! lines on the helper's stdin; replies are JSON lines on its stdout that
//! start with "OCU " (anything else is app_process chatter).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};

use super::Adb;
use crate::proc::{run_ok, sh_quote};

pub const REMOTE_APK: &str = "/data/local/tmp/ocu-android.apk";
const CLASS: &str = "com.infrawrench.opencomputeruse.shell.Helper";

pub struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next: u64,
}

impl Helper {
    /// Pushes `apk` (when the device's copy differs) and starts the helper.
    pub fn start(adb: &Adb, apk: &Path) -> Result<Self> {
        push_if_changed(adb, apk)?;
        let mut c = adb.command();
        c.args(["shell", "-T"])
            .arg(format!(
                "CLASSPATH={} app_process / {CLASS}",
                sh_quote(REMOTE_APK)
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().context("starting the Android helper")?;
        let stdin = child.stdin.take().unwrap();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("android-helper".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some(reply) = line.trim_start().strip_prefix("OCU ") {
                        if tx.send(reply.to_string()).is_err() {
                            break;
                        }
                    } else if !line.trim().is_empty() {
                        log::debug!("android helper: {line}");
                    }
                }
            })?;
        let mut helper = Self {
            child,
            stdin: Some(stdin),
            lines: rx,
            next: 1,
        };
        let hello = helper
            .call_timeout("hello", json!({}), Duration::from_secs(45))
            .context("the Android helper didn't start")?;
        log::info!("android helper on {}: {hello}", adb.serial);
        Ok(helper)
    }

    pub fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
        // A busy phone can take many seconds to accept an event.
        self.call_timeout(cmd, args, Duration::from_secs(45))
    }

    pub fn call_timeout(&mut self, cmd: &str, args: Value, timeout: Duration) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let mut req = match args {
            Value::Object(m) => m,
            _ => Default::default(),
        };
        req.insert("id".into(), json!(id));
        req.insert("cmd".into(), json!(cmd));
        let mut line = serde_json::to_vec(&Value::Object(req))?;
        line.push(b'\n');
        let stdin = self
            .stdin
            .as_mut()
            .context("the Android helper is closed")?;
        stdin
            .write_all(&line)
            .and_then(|_| stdin.flush())
            .map_err(|_| {
                anyhow!("the Android helper has stopped (was the device disconnected?)")
            })?;
        loop {
            let reply = self.lines.recv_timeout(timeout).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => {
                    anyhow!(
                        "the Android helper didn't answer \"{cmd}\" in {}s",
                        timeout.as_secs()
                    )
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    anyhow!("the Android helper has stopped (was the device disconnected?)")
                }
            })?;
            let v: Value = serde_json::from_str(&reply)
                .with_context(|| format!("the helper's reply: {reply}"))?;
            // A reply to an earlier request that timed out.
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if v.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(v);
            }
            bail!(
                "{}",
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the Android helper failed")
            );
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Closing stdin asks it to release its displays and exit.
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pushes the APK unless the device already has this exact copy.
fn push_if_changed(adb: &Adb, apk: &Path) -> Result<()> {
    let local = std::fs::metadata(apk)
        .with_context(|| format!("reading {}", apk.display()))?
        .len();
    let remote = adb
        .shell(&format!("stat -c %s {REMOTE_APK} 2>/dev/null"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let same = remote == Some(local) && {
        let theirs = adb
            .shell(&format!("sha256sum {REMOTE_APK}"))
            .ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_string));
        theirs.is_some() && theirs == sha256_hex(apk)
    };
    if !same {
        let mut c = adb.command();
        c.arg("push").arg(apk).arg(REMOTE_APK);
        run_ok(c, Duration::from_secs(60))?;
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Option<String> {
    use sha2::Digest as _;
    let data = std::fs::read(path).ok()?;
    Some(
        sha2::Sha256::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}
