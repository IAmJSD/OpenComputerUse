//! The panel hook: a DLL loaded into the apps we start, which hands us their
//! open and save dialogs instead of showing them. Each dialog arrives on our
//! named pipe as one JSON line and waits until [`answer`] writes the reply.
//!
//! The wire protocol is the macOS hook's, so the two backends read the same
//! requests and give the same answers: `{"pid":…,"kind":"open"|"save",
//! "multiple":bool,"folders":bool}` in, `{"paths":[…]}` out. The pipe name
//! reaches the app in `OCU_PANEL_PIPE`.
//!
//! The pipe carries the same shape as the hook's socket, and one difference
//! for the better: a named pipe goes away with the last handle to it, so a
//! killed or crashed agent leaves nothing behind to sweep up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::Sleep;

/// The DLL loaded into the apps we start, shipped beside this executable.
pub const DLL: &str = "OcuPanelHook.dll";

/// The environment variable the hook finds its pipe name in.
pub const PIPE_VAR: &str = "OCU_PANEL_PIPE";

/// A dialog a hooked app is waiting on.
#[derive(Clone, Debug)]
pub struct Request {
    pub save: bool,
    pub multiple: bool,
    /// An open dialog for folders only.
    pub folders: bool,
}

/// The pipe's default timeout, handed to clients that wait for an instance.
const IO_TIMEOUT_MS: u32 = 30_000;

struct Pending {
    request: Request,
    pipe: HANDLE,
}

// The pipes are handed to the app's thread, so they cross threads; only the
// hook's own thread touches one at a time, under the lock below.
unsafe impl Send for Pending {}

fn pending() -> &'static Mutex<HashMap<u32, Pending>> {
    static P: OnceLock<Mutex<HashMap<u32, Pending>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// `\\.\pipe\ocu-panel-<pid>`, where hooked apps reach us. Listening from the
/// first call on, and gone when the agent exits.
pub fn pipe() -> Option<&'static str> {
    static S: OnceLock<Option<&'static str>> = OnceLock::new();
    S.get_or_init(|| {
        let name: Vec<u16> = pipe_name()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        std::thread::Builder::new()
            .name("panel-hook".into())
            .spawn(move || serve(&name))
            .ok()?;
        Some(pipe_name())
    })
    .as_ref()
    .copied()
}

/// The pipe's name, cached so [`pipe`] can hand out a `&'static str` and the
/// serving thread the same wide string.
fn pipe_name() -> &'static str {
    static N: OnceLock<&'static str> = OnceLock::new();
    N.get_or_init(|| {
        let owned = format!(r"\\.\pipe\ocu-panel-{}", std::process::id());
        Box::leak(owned.into_boxed_str())
    })
}

/// One pipe instance per connection: a named pipe serves a single client at
/// a time, so it is made afresh for each and taken away after.
fn serve(name: &[u16]) {
    loop {
        // A raw HANDLE, not a Result: a failure is an invalid handle.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                IO_TIMEOUT_MS,
                None,
            )
        };
        if pipe.is_invalid() {
            // Out of handles or the name is taken: back off rather than spin.
            unsafe { Sleep(1000) };
            continue;
        }
        // Blocks until a hook connects. A client that gave up first shows up
        // as an error rather than a connection, and is dropped either way.
        let _ = unsafe { ConnectNamedPipe(pipe, None) };
        // `receive` keeps the pipe (storing it to answer later) or closes it:
        // either way this loop must not touch it again, or it would close a
        // handle still in the map and, once that value is recycled, a stranger.
        receive(pipe);
    }
}

/// Reads one request line from a connected hook. On success the pipe is held
/// open in [`pending`] for [`answer`]; on any failure it is closed here.
fn receive(pipe: HANDLE) {
    let Some(line) = read_line(pipe) else {
        let _ = unsafe { CloseHandle(pipe) };
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        log::warn!("panel hook sent something that is not JSON: {line:?}");
        let _ = unsafe { CloseHandle(pipe) };
        return;
    };
    let Some(pid) = v["pid"].as_u64() else {
        let _ = unsafe { CloseHandle(pipe) };
        return;
    };
    let request = Request {
        save: v["kind"] == "save",
        multiple: v["multiple"].as_bool().unwrap_or(false),
        folders: v["folders"].as_bool().unwrap_or(false),
    };
    log::info!("pid {pid} asked for a file: {}", line.trim());
    // A newer dialog replaces an older one; closing the older pipe is how
    // the app sees it cancelled.
    let mut open = pending().lock().unwrap();
    if let Some(old) = open.insert(pid as u32, Pending { request, pipe }) {
        let _ = unsafe { CloseHandle(old.pipe) };
    }
}

/// Drops app `pid`'s waiting dialog, as its session ends. The app sees it
/// cancelled.
pub fn forget(pid: u32) {
    if let Some(p) = pending().lock().unwrap().remove(&pid) {
        let _ = unsafe { CloseHandle(p.pipe) };
    }
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
    let request = waiting(pid).ok_or_else(|| anyhow!("the app is not asking for a file"))?;
    ocu_core::paths::check_answer(paths, request.save, request.multiple)?;
    let p = pending().lock().unwrap().remove(&pid);
    let Some(p) = p else {
        bail!("the app stopped asking for a file");
    };
    let paths: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into()).collect();
    let mut line = json!({ "paths": paths }).to_string();
    line.push('\n');
    // The handle is closed either way: the app is answered, or gone.
    let written = unsafe {
        WriteFile(p.pipe, Some(line.as_bytes()), None, None)
            .map_err(|e| anyhow!("the app stopped waiting for its file: {e}"))
    };
    let _ = unsafe { CloseHandle(p.pipe) };
    written
}

fn read_line(pipe: HANDLE) -> Option<String> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut n = 0u32;
        let ok = unsafe { ReadFile(pipe, Some(&mut buf), Some(&mut n), None) }.is_ok();
        if !ok || n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n as usize]);
        if got.contains(&b'\n') {
            break;
        }
        if got.len() > 1 << 20 {
            return None;
        }
    }
    (!got.is_empty()).then(|| String::from_utf8_lossy(&got).into_owned())
}

/// Whether the hook should be loaded into the apps this agent starts.
///
/// The user's setting is carried by the platform, since the backend cannot
/// read the agent's config itself. `OCU_WINDOWS_PANEL_HOOK` overrides it,
/// for a machine with no settings to change.
pub fn wanted() -> bool {
    *WANTED.get_or_init(from_env)
}

static WANTED: OnceLock<bool> = OnceLock::new();

/// Sets whether to load the hook, from the user's own settings.
pub fn set_wanted(on: bool) {
    let _ = WANTED.set(on);
}

/// The hook's setting from the environment alone.
pub fn from_env() -> bool {
    matches!(
        std::env::var("OCU_WINDOWS_PANEL_HOOK")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "yes" | "on")
    )
}

/// The hook DLL shipped beside this executable, or `OCU_PANEL_HOOK`.
pub fn dll() -> Option<PathBuf> {
    let path = match std::env::var_os("OCU_PANEL_HOOK") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe()
            .ok()?
            .parent()?
            .join("panelhook")
            .join(DLL),
    };
    path.is_file().then_some(path)
}

/// Opens the pipe by name, as the hook does. Used to check the agent is
/// really listening before an app is asked to take the hook.
pub fn reachable(name: &str) -> bool {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let pipe = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    if let Ok(pipe) = pipe {
        let _ = unsafe { CloseHandle(pipe) };
        true
    } else {
        false
    }
}
