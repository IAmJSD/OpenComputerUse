//! The panel hook: a DLL loaded into the apps we start, which hands us their
//! open and save dialogs instead of showing them.
//!
//! Each hooked app connects once to our pipe (named in `OCU_PANEL_PIPE`) and
//! keeps the connection. Per dialog it sends one JSON object,
//! `{"pid":…,"kind":"open"|"save","multiple":bool,"folders":bool}`, and waits
//! for a `{"paths":[…]}` line back. This is the macOS hook's protocol.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

/// The DLL loaded into the apps we start.
pub const DLL: &str = "OcuPanelHook.dll";

/// The environment variable the hook finds its pipe name in.
pub const PIPE_VAR: &str = "OCU_PANEL_PIPE";

/// How long the hook waits for an answer (`AGENT_TIMEOUT_MS` in hook.c), less
/// a margin, so we never answer a dialog it has already given up on.
const ANSWER_WINDOW: Duration = Duration::from_secs(25);

/// A dialog a hooked app is waiting on.
#[derive(Clone, Debug)]
pub struct Request {
    pub save: bool,
    pub multiple: bool,
    /// An open dialog for folders only.
    pub folders: bool,
}

/// The reply line, and where to report whether it was written.
type Reply = (String, mpsc::Sender<windows::core::Result<()>>);

struct Pending {
    request: Request,
    /// To the thread holding the app's connection, which writes the reply.
    reply: mpsc::Sender<Reply>,
    /// Tells this request apart from the app's next one.
    id: u64,
}

fn pending() -> &'static Mutex<HashMap<u32, Pending>> {
    static P: OnceLock<Mutex<HashMap<u32, Pending>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// A pipe instance, closed on drop.
struct Pipe(HANDLE);

// Each instance is used by one thread at a time.
unsafe impl Send for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn create(name: &[u16], first: bool) -> Option<Pipe> {
    let mut mode = PIPE_ACCESS_DUPLEX;
    if first {
        // Fails if someone else made the name first, so we never serve
        // through a pipe they own.
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            mode,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            None,
        )
    };
    (!pipe.is_invalid()).then_some(Pipe(pipe))
}

/// `\\.\pipe\ocu-panel-<pid>`, where hooked apps reach us. Listening from the
/// first call on; `None` if the pipe could not be made.
pub fn pipe() -> Option<&'static str> {
    static S: OnceLock<Option<String>> = OnceLock::new();
    S.get_or_init(|| {
        let name = format!(r"\\.\pipe\ocu-panel-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let first = create(&wide, true)?;
        std::thread::Builder::new()
            .name("panel-hook".into())
            .spawn(move || serve(&wide, first))
            .ok()?;
        Some(name)
    })
    .as_deref()
}

/// Accepts hooks for good. Each connection gets its own thread, and a new
/// instance is made as soon as one is taken, so the next app can connect.
fn serve(name: &[u16], mut listening: Pipe) {
    loop {
        // A client that connected before this call is ERROR_PIPE_CONNECTED,
        // which is still a connection.
        let connected = match unsafe { ConnectNamedPipe(listening.0, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        let next = loop {
            match create(name, false) {
                Some(p) => break p,
                // Out of handles: back off rather than spin.
                None => std::thread::sleep(Duration::from_secs(1)),
            }
        };
        let conn = std::mem::replace(&mut listening, next);
        if connected {
            let _ = std::thread::Builder::new()
                .name("panel-hook-app".into())
                .spawn(move || converse(conn));
        }
    }
}

/// Serves one hooked app until it goes: a request, its answer, the next.
fn converse(pipe: Pipe) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // The pid comes from the kernel, not the request, so no other process
    // can ask in an app's name.
    let mut pid = 0;
    if unsafe { GetNamedPipeClientProcessId(pipe.0, &mut pid) }.is_err() {
        return;
    }
    log::info!("the panel hook in pid {pid} connected");
    let mut buf = Vec::new();
    while let Some(v) = read_request(&pipe, &mut buf) {
        let request = Request {
            save: v["kind"] == "save",
            multiple: v["multiple"].as_bool().unwrap_or(false),
            folders: v["folders"].as_bool().unwrap_or(false),
        };
        log::info!("pid {pid} asked for a file: {v}");
        let (tx, rx) = mpsc::channel();
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        pending().lock().unwrap().insert(
            pid,
            Pending {
                request,
                reply: tx,
                id,
            },
        );
        let (line, done) = match rx.recv_timeout(ANSWER_WINDOW) {
            Ok(reply) => reply,
            // Forgotten: closing the pipe tells the app.
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                // The hook has shown the real dialog. Withdraw the request,
                // unless an answer is already on its way.
                let mut open = pending().lock().unwrap();
                if open.get(&pid).is_some_and(|p| p.id == id) {
                    open.remove(&pid);
                    continue;
                }
                drop(open);
                match rx.recv() {
                    Ok(reply) => reply,
                    Err(_) => return,
                }
            }
        };
        let written = unsafe { WriteFile(pipe.0, Some(line.as_bytes()), None, None) };
        let failed = written.is_err();
        let _ = done.send(written);
        if failed {
            return;
        }
    }
}

/// The next request: one JSON object, with or without a newline after it.
/// `None` once the app has gone or sent something else.
fn read_request(pipe: &Pipe, buf: &mut Vec<u8>) -> Option<Value> {
    loop {
        let parsed = {
            let mut it = serde_json::Deserializer::from_slice(buf).into_iter::<Value>();
            it.next().map(|r| r.map(|v| (v, it.byte_offset())))
        };
        match parsed {
            Some(Ok((v, used))) => {
                buf.drain(..used);
                return Some(v);
            }
            Some(Err(e)) if !e.is_eof() => {
                log::warn!("the panel hook sent something that is not JSON: {e}");
                return None;
            }
            _ => {}
        }
        if buf.len() > 1 << 20 {
            return None;
        }
        let mut chunk = [0u8; 4096];
        let mut n = 0u32;
        let ok = unsafe { ReadFile(pipe.0, Some(&mut chunk), Some(&mut n), None) }.is_ok();
        if !ok || n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
}

/// Drops app `pid`'s waiting dialog as its session ends.
pub fn forget(pid: u32) {
    pending().lock().unwrap().remove(&pid);
}

/// The dialog app `pid` is waiting on, if any.
pub fn waiting(pid: u32) -> Option<Request> {
    pending()
        .lock()
        .unwrap()
        .get(&pid)
        .map(|p| p.request.clone())
}

/// Answers app `pid`'s dialog with `paths`; none cancels it.
pub fn answer(pid: u32, paths: &[PathBuf]) -> Result<()> {
    let p = {
        let mut open = pending().lock().unwrap();
        let p = open
            .get(&pid)
            .ok_or_else(|| anyhow!("the app is not asking for a file"))?;
        ocu_core::paths::check_answer(paths, p.request.save, p.request.multiple)?;
        open.remove(&pid).expect("just seen")
    };
    let paths: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into()).collect();
    let mut line = json!({ "paths": paths }).to_string();
    line.push('\n');
    let (done, written) = mpsc::channel();
    p.reply
        .send((line, done))
        .map_err(|_| anyhow!("the app stopped asking for a file"))?;
    written
        .recv()
        .map_err(|_| anyhow!("the app stopped waiting for its file"))?
        .map_err(|e| anyhow!("the app stopped waiting for its file: {e}"))
}

/// The hook DLL to load: `OCU_PANEL_HOOK`, else the one built into this
/// executable, else `panelhook\` beside it (a local build).
pub fn dll() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCU_PANEL_HOOK") {
        return Some(PathBuf::from(p)).filter(|p| p.is_file());
    }
    #[cfg(ocu_panel_hook)]
    match unpack() {
        Ok(p) => return Some(p),
        Err(e) => log::warn!("unpacking the panel hook: {e}"),
    }
    let path = std::env::current_exe()
        .ok()?
        .parent()?
        .join("panelhook")
        .join(DLL);
    path.is_file().then_some(path)
}

/// Writes the embedded hook to `%LOCALAPPDATA%\opencomputeruse\panelhook\<version>`.
/// Versioned, because a running app keeps the old one loaded and locked.
#[cfg(ocu_panel_hook)]
fn unpack() -> Result<PathBuf> {
    static BYTES: &[u8] = include_bytes!(env!("OCU_PANEL_HOOK_DLL"));
    let base =
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow!("LOCALAPPDATA is not set"))?;
    let dir = PathBuf::from(base)
        .join("opencomputeruse")
        .join("panelhook")
        .join(env!("CARGO_PKG_VERSION"));
    let path = dir.join(DLL);
    if std::fs::read(&path).is_ok_and(|b| b == BYTES) {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{DLL}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, BYTES)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(path)
}
