//! The panel hook (`packaging/macos/panelhook`): a dylib that apps we start
//! load through `DYLD_INSERT_LIBRARIES`, which hands us their open and save
//! panels instead of showing them. Each panel arrives on our socket as one
//! JSON line; the panel waits until [`answer`] writes the reply.
//!
//! Only apps whose signature lets the variable through get it: dyld ignores
//! it for most apps, which is harmless, but where it is honoured a dylib
//! that fails to load (a missing file, a signature the app refuses, an
//! architecture it lacks) stops the app before it starts.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub const DYLIB: &str = "OcuPanelHook.dylib";

/// A panel a hooked app is waiting on.
#[derive(Clone, Debug)]
pub struct Request {
    pub save: bool,
    pub multiple: bool,
    /// An open panel for folders only.
    pub folders: bool,
}

struct Pending {
    request: Request,
    stream: UnixStream,
}

fn pending() -> &'static Mutex<HashMap<i32, Pending>> {
    static P: OnceLock<Mutex<HashMap<i32, Pending>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// Where hooked apps reach us, listening from the first call on.
pub fn socket() -> Option<&'static Path> {
    static S: OnceLock<Option<PathBuf>> = OnceLock::new();
    S.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("ocu-panel-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .map_err(|e| log::warn!("panel hook socket {}: {e}", path.display()))
            .ok()?;
        std::thread::Builder::new()
            .name("panel-hook".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || receive(stream));
                }
            })
            .ok()?;
        Some(path)
    })
    .as_deref()
}

fn receive(stream: UnixStream) {
    let mut line = String::new();
    let Ok(read) = stream.try_clone() else { return };
    if BufReader::new(read).read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let Some(pid) = v["pid"].as_i64() else { return };
    let request = Request {
        save: v["kind"] == "save",
        multiple: v["multiple"].as_bool().unwrap_or(false),
        folders: v["directories"] == true && v["files"] == false,
    };
    log::info!("pid {pid} asked for a file: {line}");
    // A newer panel replaces an older one; dropping the older stream
    // cancels it.
    pending()
        .lock()
        .unwrap()
        .insert(pid as i32, Pending { request, stream });
}

/// The panel app `pid` is waiting on, if any.
pub fn waiting(pid: i32) -> Option<Request> {
    pending()
        .lock()
        .unwrap()
        .get(&pid)
        .map(|p| p.request.clone())
}

/// Answers app `pid`'s panel with `paths`; none cancels it.
pub fn answer(pid: i32, paths: &[PathBuf]) -> Result<()> {
    let request = waiting(pid).ok_or_else(|| anyhow!("the app is not asking for a file"))?;
    if request.save {
        match paths {
            [] => {}
            [p] => {
                if !p.parent().is_some_and(Path::is_dir) {
                    bail!("there is no folder to save {} in", p.display());
                }
            }
            _ => bail!("a save panel saves to one path"),
        }
    } else {
        crate::pages::check(paths, request.multiple)?;
    }
    let Some(mut p) = pending().lock().unwrap().remove(&pid) else {
        bail!("the app stopped asking for a file");
    };
    let paths: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into()).collect();
    let mut line = json!({ "paths": paths }).to_string();
    line.push('\n');
    p.stream
        .write_all(line.as_bytes())
        .map_err(|e| anyhow!("the app stopped waiting for its file: {e}"))
}

/// The hook dylib shipped beside this executable, or `OCU_PANEL_HOOK`.
pub fn dylib() -> Option<PathBuf> {
    let path = match std::env::var_os("OCU_PANEL_HOOK") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .join("Frameworks")
            .join(DYLIB),
    };
    path.is_file().then_some(path)
}

/// Whether an entitlements plist sets `key` to true.
fn entitled(plist: &str, key: &str) -> bool {
    let Some(at) = plist.find(&format!("<key>{key}</key>")) else {
        return false;
    };
    plist[at..]
        .split_once("</key>")
        .is_some_and(|(_, rest)| rest.trim_start().starts_with("<true/>"))
}

/// Whether dyld honours `DYLD_INSERT_LIBRARIES` for `bundle` and will load
/// our dylib into it: an app without the hardened runtime or library
/// validation, or a hardened one entitled both to the variable and to
/// libraries from other teams. Not a sandboxed app, which could not reach
/// the socket nor open what it was handed, nor one whose executable lacks
/// this machine's architecture.
pub fn injectable(bundle: &Path) -> bool {
    let Ok(out) = Command::new("/usr/bin/codesign")
        .args(["-d", "-v", "--entitlements", "-", "--xml"])
        .arg(bundle)
        .output()
    else {
        return false;
    };
    let info = String::from_utf8_lossy(&out.stderr);
    let plist = String::from_utf8_lossy(&out.stdout);
    let flags = info
        .lines()
        .find_map(|l| l.split_once("flags=").map(|(_, f)| f.to_string()))
        .unwrap_or_default();
    let unsigned = info.contains("not signed at all");
    if !unsigned && !out.status.success() {
        return false;
    }
    if flags.contains("restrict") || entitled(&plist, "com.apple.security.app-sandbox") {
        return false;
    }
    let hardened = flags.contains("runtime");
    let allowed = if hardened {
        entitled(
            &plist,
            "com.apple.security.cs.allow-dyld-environment-variables",
        ) && entitled(&plist, "com.apple.security.cs.disable-library-validation")
    } else {
        !flags.contains("library-validation")
    };
    allowed && has_our_arch(bundle)
}

fn has_our_arch(bundle: &Path) -> bool {
    let Some(exe) = crate::session::bundle_executable(bundle) else {
        return false;
    };
    let Ok(out) = Command::new("/usr/bin/lipo")
        .arg("-archs")
        .arg(&exe)
        .output()
    else {
        return false;
    };
    let archs = String::from_utf8_lossy(&out.stdout);
    let want = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    // An arm64e-only app would refuse a plain arm64 library.
    archs.split_whitespace().any(|a| a == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_entitlements() {
        let plist = "<plist><dict><key>com.apple.security.cs.allow-dyld-environment-variables</key>\n\t<true/><key>com.apple.security.app-sandbox</key><false/></dict></plist>";
        assert!(entitled(
            plist,
            "com.apple.security.cs.allow-dyld-environment-variables"
        ));
        assert!(!entitled(plist, "com.apple.security.app-sandbox"));
        assert!(!entitled(
            plist,
            "com.apple.security.cs.disable-library-validation"
        ));
    }

    #[test]
    fn leaves_protected_apps_alone() {
        // Apple's apps are sandboxed or validate their libraries; dyld
        // would refuse the hook in them.
        for app in [
            "/System/Applications/TextEdit.app",
            "/Applications/Safari.app",
            "/System/Applications/Calculator.app",
        ] {
            if Path::new(app).exists() {
                assert!(!injectable(Path::new(app)), "{app}");
            }
        }
    }
}
