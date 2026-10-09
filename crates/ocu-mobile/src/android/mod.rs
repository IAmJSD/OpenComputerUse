//! Android over adb: phones, tablets and emulators, from any desktop.
//!
//! The SDK is looked for in `$ANDROID_HOME`, `$ANDROID_SDK_ROOT`, the
//! places Android Studio and Homebrew put it, then on `PATH`. Only
//! platform-tools (adb) is needed for phones; emulators also need the
//! emulator package and an AVD.

pub mod emulator;
pub mod helper;
pub mod session;
pub mod tree;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::proc::{self, run, run_ok, sh_quote};

/// Where an Android SDK may be, best first.
pub fn sdk_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["OCU_ANDROID_SDK", "ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if cfg!(target_os = "macos") {
        dirs.push(home().join("Library/Android/sdk"));
        dirs.push("/opt/homebrew/share/android-commandlinetools".into());
        dirs.push("/usr/local/share/android-commandlinetools".into());
        dirs.push("/opt/homebrew/share/android-sdk".into());
    } else if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Android\\Sdk"));
        }
    } else {
        dirs.push(home().join("Android/Sdk"));
        dirs.push("/usr/lib/android-sdk".into());
        dirs.push("/opt/android-sdk".into());
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

pub fn adb_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCU_ADB") {
        return Some(PathBuf::from(p));
    }
    let candidates: Vec<PathBuf> = sdk_dirs()
        .into_iter()
        .map(|d| d.join("platform-tools").join(exe("adb")))
        .collect();
    proc::find_tool("adb", &candidates)
}

/// The emulator, and the SDK it belongs to (which it needs to be told).
pub fn emulator_path() -> Option<(PathBuf, Option<PathBuf>)> {
    for sdk in sdk_dirs() {
        let p = sdk.join("emulator").join(exe("emulator"));
        if p.is_file() {
            return Some((p, Some(sdk)));
        }
    }
    proc::find_tool("emulator", &[]).map(|p| {
        // <sdk>/emulator/emulator
        let sdk = p.parent().and_then(|d| d.parent()).map(PathBuf::from);
        (p, sdk)
    })
}

/// Whether the emulator is installed with at least one virtual device.
pub fn emulator_installed() -> bool {
    emulator_path().is_some()
}

fn need_adb() -> Result<PathBuf> {
    adb_path().ok_or_else(|| {
        anyhow!(
            "adb is not installed. Install Android's platform-tools (Android Studio's SDK Manager, \
             `brew install --cask android-platform-tools`, or `apt install adb`), or set \
             ANDROID_HOME to the SDK or OCU_ADB to adb"
        )
    })
}

/// One device, as adb reaches it.
#[derive(Clone, Debug)]
pub struct Adb {
    pub exe: PathBuf,
    pub serial: String,
}

const QUICK: Duration = Duration::from_secs(20);

impl Adb {
    pub fn new(serial: &str) -> Result<Self> {
        Ok(Self {
            exe: need_adb()?,
            serial: serial.to_string(),
        })
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.arg("-s").arg(&self.serial);
        c
    }

    /// Runs a shell command line on the device and returns its output.
    /// adb's own exit status follows the command's on any recent device.
    pub fn shell(&self, line: &str) -> Result<String> {
        self.shell_timeout(line, QUICK)
    }

    pub fn shell_timeout(&self, line: &str, timeout: Duration) -> Result<String> {
        let mut c = self.command();
        c.arg("shell").arg(line);
        Ok(run_ok(c, timeout)?.text())
    }

    /// Like `shell`, with stdout as raw bytes (no newline translation).
    pub fn exec_out(&self, line: &str, timeout: Duration) -> Result<Vec<u8>> {
        let mut c = self.command();
        c.arg("exec-out").arg(line);
        Ok(run_ok(c, timeout)?.stdout)
    }

    pub fn getprop(&self, name: &str) -> String {
        self.shell(&format!("getprop {name}"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    pub fn sdk_level(&self) -> u32 {
        self.getprop("ro.build.version.sdk").parse().unwrap_or(0)
    }

    /// The main display's size in pixels (the override, when set) and its
    /// density in dpi.
    pub fn display_metrics(&self) -> Result<(u32, u32, u32)> {
        // "Physical size: 1080x2400", then "Override size: …" when set,
        // which is what apps get.
        let current = |text: &str| -> Option<String> {
            let value = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(|v| v.trim_start_matches([' ', ':']).trim().to_string())
            };
            value("Override").or_else(|| value("Physical"))
        };
        let size_text = self.shell("wm size")?;
        let size = current(&size_text.replace(" size", ""))
            .ok_or_else(|| anyhow!("`wm size` said: {size_text}"))?;
        let (w, h) = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .ok_or_else(|| anyhow!("unreadable display size {size}"))?;
        let dpi = current(&self.shell("wm density")?.replace(" density", ""))
            .and_then(|d| d.parse().ok())
            .unwrap_or(160);
        Ok((w, h, dpi))
    }

    /// The launcher activity of `package`, as `package/class`.
    pub fn launcher_activity(&self, package: &str) -> Result<String> {
        let out = self.shell(&format!(
            "cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER {}",
            sh_quote(package)
        ))?;
        out.lines()
            .map(str::trim)
            .rfind(|l| l.contains('/'))
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow!(
                    "{package} has no launcher activity on {} (is it installed? *_apps lists them)",
                    self.serial
                )
            })
    }

    pub fn pid_of(&self, package: &str) -> Option<u32> {
        self.shell(&format!("pidof {}", sh_quote(package)))
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    pub fn is_online(&self) -> bool {
        let mut c = self.command();
        c.arg("get-state");
        run(c, Duration::from_secs(5))
            .map(|o| o.status.success() && o.text().trim() == "device")
            .unwrap_or(false)
    }
}

#[derive(Clone, Debug)]
pub struct AdbDevice {
    pub serial: String,
    /// "device", "unauthorized", "offline", …
    pub state: String,
    pub model: String,
    pub product: String,
}

impl AdbDevice {
    pub fn is_emulator(&self) -> bool {
        self.serial.starts_with("emulator-")
    }
}

/// Everything `adb devices -l` lists.
pub fn adb_devices() -> Result<Vec<AdbDevice>> {
    let mut c = Command::new(need_adb()?);
    c.args(["devices", "-l"]);
    let out = run_ok(c, QUICK)?.text();
    Ok(out
        .lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let mut d = AdbDevice {
                serial,
                state,
                model: String::new(),
                product: String::new(),
            };
            for p in parts {
                if let Some(m) = p.strip_prefix("model:") {
                    d.model = m.replace('_', " ");
                } else if let Some(m) = p.strip_prefix("product:") {
                    d.product = m.to_string();
                }
            }
            Some(d)
        })
        .collect())
}

/// Physical Android devices, for `phone_list`.
pub fn phones() -> Result<Vec<Value>> {
    if adb_path().is_none() {
        return Ok(Vec::new());
    }
    Ok(adb_devices()?
        .into_iter()
        .filter(|d| !d.is_emulator())
        .map(|d| {
            let (release, manufacturer) = if d.state == "device" {
                let adb = Adb::new(&d.serial).ok();
                (
                    adb.as_ref()
                        .map(|a| a.getprop("ro.build.version.release"))
                        .unwrap_or_default(),
                    adb.as_ref()
                        .map(|a| a.getprop("ro.product.manufacturer"))
                        .unwrap_or_default(),
                )
            } else {
                Default::default()
            };
            let mut v = json!({
                "id": d.serial,
                "platform": "android",
                "name": format!("{manufacturer} {}", d.model).trim().to_string(),
                "os_version": release,
                "state": state_note(&d.state),
            });
            if d.state != "device" {
                v["help"] = json!(help_for_state(&d.state));
            }
            v
        })
        .collect())
}

fn state_note(state: &str) -> &str {
    match state {
        "device" => "ready",
        other => other,
    }
}

fn help_for_state(state: &str) -> &'static str {
    match state {
        "unauthorized" => "Unlock the phone and accept the \"Allow USB debugging?\" prompt.",
        "offline" => "Reconnect the phone, or run `adb kill-server`.",
        _ => "Turn on USB debugging in Developer options.",
    }
}

/// Picks the device a phone or emulator session means: by serial, by
/// model name, or the only ready one.
pub fn pick_device(id: Option<&str>, emulators: bool) -> Result<AdbDevice> {
    let all: Vec<AdbDevice> = adb_devices()?
        .into_iter()
        .filter(|d| d.is_emulator() == emulators)
        .collect();
    let what = if emulators {
        "emulator"
    } else {
        "Android phone"
    };
    let found = match id {
        Some(id) => all
            .iter()
            .find(|d| d.serial == id)
            .or_else(|| all.iter().find(|d| d.model.eq_ignore_ascii_case(id)))
            .cloned(),
        None => {
            let ready: Vec<&AdbDevice> = all.iter().filter(|d| d.state == "device").collect();
            match ready.len() {
                0 => None,
                1 => Some(ready[0].clone()),
                _ => bail!(
                    "{} {what}s are connected ({}); say which",
                    ready.len(),
                    ready
                        .iter()
                        .map(|d| d.serial.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    let Some(d) = found else {
        bail!(match id {
            Some(id) => format!("no {what} \"{id}\" is connected"),
            None => format!("no {what} is connected"),
        });
    };
    if d.state != "device" {
        bail!("{} is {}: {}", d.serial, d.state, help_for_state(&d.state));
    }
    Ok(d)
}

/// The apps with a launcher icon: package and activity.
pub fn apps(adb: &Adb) -> Result<Vec<Value>> {
    let out = adb.shell(
        "cmd package query-activities --components -a android.intent.action.MAIN -c android.intent.category.LAUNCHER",
    )?;
    let mut seen = std::collections::BTreeSet::new();
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('/') && !l.contains(' '))
        .filter_map(|l| {
            let (package, _) = l.split_once('/')?;
            seen.insert(package.to_string())
                .then(|| json!({ "package": package, "activity": l }))
        })
        .collect())
}
