//! Running the tools that talk to devices (adb, xcrun, the emulator) with
//! a deadline, since a wedged device can leave them hanging forever.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};

pub struct Output {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn describe_failure(&self, what: &str) -> String {
        let err = String::from_utf8_lossy(&self.stderr);
        let out = String::from_utf8_lossy(&self.stdout);
        let detail = if err.trim().is_empty() { out } else { err };
        format!(
            "{what} failed ({}): {}",
            self.status,
            clip(detail.trim(), 600)
        )
    }
}

/// Runs `cmd` to completion, killing it after `timeout`.
pub fn run(mut cmd: Command, timeout: Duration) -> Result<Output> {
    let what = describe(&cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("starting {what}"))?;
    // Drain both pipes on threads so a chatty tool can't fill one and stall.
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{what} took longer than {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
    })
}

/// Like [`run`], but a failing exit status is an error.
pub fn run_ok(cmd: Command, timeout: Duration) -> Result<Output> {
    let what = describe(&cmd);
    let out = run(cmd, timeout)?;
    if !out.status.success() {
        bail!("{}", out.describe_failure(&what));
    }
    Ok(out)
}

fn describe(cmd: &Command) -> String {
    let program = Path::new(cmd.get_program())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parts: Vec<String> = std::iter::once(program)
        .chain(
            cmd.get_args()
                .take(4)
                .map(|a| clip(&a.to_string_lossy(), 40)),
        )
        .collect();
    format!("`{}`", parts.join(" "))
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Finds an executable: the first of `candidates` that exists, then
/// `name` on `PATH`. Apps started from the Finder get a bare `PATH`, so
/// the usual install places are always among the candidates.
pub fn find_tool(name: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut all: Vec<PathBuf> = candidates.to_vec();
    if let Some(path) = std::env::var_os("PATH") {
        all.extend(std::env::split_paths(&path).map(|d| d.join(&file)));
    }
    if cfg!(target_os = "macos") {
        all.push(PathBuf::from("/opt/homebrew/bin").join(&file));
        all.push(PathBuf::from("/usr/local/bin").join(&file));
    }
    all.into_iter().find(|p| p.is_file())
}

/// Starts a long-lived process that should outlive this request (an
/// emulator), detached from our terminal's signals, its output in `log`.
pub fn spawn_detached(mut cmd: Command, log: &Path) -> Result<std::process::Child> {
    let what = describe(&cmd);
    let file = std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    cmd.stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn().with_context(|| format!("starting {what}"))
}

/// The last `max` bytes of a log file, for an error message.
pub fn log_tail(path: &Path, max: usize) -> String {
    let text = std::fs::read(path).unwrap_or_default();
    let start = text.len().saturating_sub(max);
    String::from_utf8_lossy(&text[start..]).trim().to_string()
}

/// A TCP port nothing on this machine is listening on right now.
pub fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

/// Quotes `s` for a POSIX shell (the device side of `adb shell`).
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@%+".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("com.example/.Main"), "com.example/.Main");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote(""), "''");
    }
}
