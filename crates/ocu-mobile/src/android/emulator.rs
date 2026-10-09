//! Android emulators: the AVDs there are, which are running, and booting
//! one headless for a session (shut down again when the last session on
//! it ends, if a session booted it).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::{adb_devices, emulator_path, Adb};
use crate::lease::Cleanup;
use crate::proc::{self, run, run_ok};

pub fn avds() -> Result<Vec<String>> {
    let (exe, sdk) = emulator_path().ok_or_else(not_installed)?;
    let mut c = Command::new(exe);
    c.arg("-list-avds");
    if let Some(sdk) = sdk {
        c.env("ANDROID_SDK_ROOT", sdk);
    }
    let out = run_ok(c, Duration::from_secs(20))?.text();
    // Newer emulators mix in log lines ("INFO | …").
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(" | ") && !l.contains(' '))
        .map(str::to_string)
        .collect())
}

fn not_installed() -> anyhow::Error {
    anyhow!(
        "the Android emulator is not installed: add it and a system image with Android Studio's \
         SDK Manager (or `sdkmanager emulator \"system-images;android-35;google_apis;arm64-v8a\"`), \
         then create a virtual device"
    )
}

/// Running emulators: serial and AVD name.
pub fn running() -> Result<Vec<(String, String)>> {
    if super::adb_path().is_none() {
        return Ok(Vec::new());
    }
    Ok(adb_devices()?
        .into_iter()
        .filter(|d| d.is_emulator() && d.state == "device")
        .map(|d| {
            let name = avd_name(&d.serial).unwrap_or_default();
            (d.serial, name)
        })
        .collect())
}

fn avd_name(serial: &str) -> Option<String> {
    let adb = Adb::new(serial).ok()?;
    let mut c = adb.command();
    c.args(["emu", "avd", "name"]);
    let out = run(c, Duration::from_secs(5)).ok()?.text();
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "OK")
        .map(str::to_string)
        .or_else(|| {
            // Older emulators: ask the system property instead.
            Some(adb.getprop("ro.boot.qemu.avd_name")).filter(|s| !s.is_empty())
        })
}

/// `android_emulator_list`: every AVD, and the serial of the running ones.
pub fn list() -> Result<Vec<Value>> {
    let running = running()?;
    let mut out: Vec<Value> = avds()?
        .into_iter()
        .map(|name| {
            let serial = running
                .iter()
                .find(|(_, n)| *n == name)
                .map(|(s, _)| s.clone());
            json!({
                "name": name,
                "state": if serial.is_some() { "running" } else { "stopped" },
                "serial": serial,
            })
        })
        .collect();
    // Running emulators of AVDs we couldn't name.
    for (serial, name) in running {
        if !out.iter().any(|v| v["serial"] == json!(serial)) {
            out.push(json!({ "name": name, "state": "running", "serial": serial }));
        }
    }
    Ok(out)
}

/// What a session asked for: an AVD name or an emulator serial.
pub enum Want<'a> {
    Any,
    Named(&'a str),
}

/// A running emulator's serial for `want`, booting the AVD when it isn't
/// running. Returns the serial and, when this booted it, the cleanup that
/// shuts it down.
pub fn ensure_running(
    want: Want,
    show_window: bool,
    logs: &Path,
) -> Result<(String, Option<Cleanup>)> {
    let running = running()?;
    let name = match want {
        Want::Named(id) => {
            if let Some((serial, _)) = running.iter().find(|(s, n)| s == id || n == id) {
                return Ok((serial.clone(), None));
            }
            if id.starts_with("emulator-") {
                bail!("no emulator {id} is running; android_emulator_list shows them");
            }
            id.to_string()
        }
        Want::Any => {
            if let Some((serial, _)) = running.first() {
                return Ok((serial.clone(), None));
            }
            let avds = avds()?;
            match avds.as_slice() {
                [] => bail!("there are no Android virtual devices; create one with Android Studio's Device Manager (or avdmanager)"),
                [one] => one.clone(),
                many => bail!(
                    "no emulator is running; say which to start: {}",
                    many.join(", ")
                ),
            }
        }
    };
    if !avds()?.contains(&name) {
        bail!("there is no Android virtual device \"{name}\"; android_emulator_list shows them");
    }
    let serial = boot(&name, show_window, logs)?;
    let kill_serial = serial.clone();
    Ok((
        serial,
        Some(Box::new(move || {
            log::info!("shutting down emulator {kill_serial}");
            if let Ok(adb) = Adb::new(&kill_serial) {
                let mut c = adb.command();
                c.args(["emu", "kill"]);
                let _ = run(c, Duration::from_secs(30));
            }
        })),
    ))
}

fn boot(name: &str, show_window: bool, logs: &Path) -> Result<String> {
    let (exe, sdk) = emulator_path().ok_or_else(not_installed)?;
    let port = free_emulator_port()?;
    let serial = format!("emulator-{port}");
    std::fs::create_dir_all(logs)?;
    let log = logs.join(format!("emulator-{name}.log"));
    let mut c = Command::new(&exe);
    c.args([
        "-avd",
        name,
        "-port",
        &port.to_string(),
        "-no-audio",
        "-no-boot-anim",
    ]);
    if !show_window {
        c.arg("-no-window");
    }
    if let Some(sdk) = &sdk {
        c.env("ANDROID_SDK_ROOT", sdk);
    }
    log::info!("booting emulator {name} as {serial}");
    let mut child = proc::spawn_detached(c, &log)?;
    let adb = Adb::new(&serial)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(status) = child.try_wait()? {
            bail!(
                "the emulator for {name} exited ({status}) while booting:\n{}",
                proc::log_tail(&log, 1500)
            );
        }
        if adb.is_online() && adb.getprop("sys.boot_completed") == "1" {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!(
                "the emulator for {name} didn't finish booting in 5 minutes:\n{}",
                proc::log_tail(&log, 1500)
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    // The launcher draws a little after boot completes.
    std::thread::sleep(Duration::from_secs(2));
    Ok(serial)
}

/// An even port from 5554 whose console and adb ports are both free.
fn free_emulator_port() -> Result<u16> {
    let taken: Vec<String> = adb_devices()
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.serial)
        .collect();
    (5554..5682)
        .step_by(2)
        .find(|p| {
            !taken.contains(&format!("emulator-{p}"))
                && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()
                && std::net::TcpListener::bind(("127.0.0.1", p + 1)).is_ok()
        })
        .ok_or_else(|| anyhow!("no free emulator port between 5554 and 5682"))
}
