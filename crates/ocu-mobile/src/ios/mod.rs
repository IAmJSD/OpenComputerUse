//! iOS: simulators through `simctl`, iPhones and iPads through `devicectl`,
//! and both driven through WebDriverAgent. Needs Xcode, so macOS only.

pub mod session;
pub mod tree;
pub mod wda;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};

use crate::lease::Leases;
use crate::proc::{self, run, run_ok};

fn xcrun() -> Command {
    Command::new("/usr/bin/xcrun")
}

/// Whether Xcode (not just the command-line tools) is installed: simctl
/// and devicectl come with it.
pub fn xcode_installed() -> bool {
    let mut c = xcrun();
    c.args(["--find", "simctl"]);
    run(c, Duration::from_secs(10)).is_ok_and(|o| o.status.success())
}

fn need_xcode() -> Result<()> {
    if !xcode_installed() {
        bail!("iOS needs Xcode: install it from the App Store, open it once, then run `sudo xcode-select -s /Applications/Xcode.app`");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Simulator {
    pub udid: String,
    pub name: String,
    /// "iOS 26.3"
    pub runtime: String,
    /// "Booted", "Shutdown", …
    pub state: String,
}

impl Simulator {
    pub fn booted(&self) -> bool {
        self.state == "Booted"
    }
}

pub fn simulators() -> Result<Vec<Simulator>> {
    need_xcode()?;
    let mut c = xcrun();
    c.args(["simctl", "list", "devices", "available", "-j"]);
    let v: Value = serde_json::from_slice(&run_ok(c, Duration::from_secs(30))?.stdout)?;
    let mut sims = Vec::new();
    if let Some(runtimes) = v["devices"].as_object() {
        for (runtime, list) in runtimes {
            // "com.apple.CoreSimulator.SimRuntime.iOS-26-3" → "iOS 26.3"
            let pretty = runtime
                .rsplit('.')
                .next()
                .unwrap_or(runtime)
                .replacen('-', " ", 1)
                .replace('-', ".");
            for d in list.as_array().into_iter().flatten() {
                sims.push(Simulator {
                    udid: d["udid"].as_str().unwrap_or_default().into(),
                    name: d["name"].as_str().unwrap_or_default().into(),
                    runtime: pretty.clone(),
                    state: d["state"].as_str().unwrap_or_default().into(),
                });
            }
        }
    }
    // Newest runtime first, so "the first iPhone" is a current one.
    sims.sort_by_key(|s| std::cmp::Reverse(version_key(&s.runtime)));
    Ok(sims)
}

fn version_key(runtime: &str) -> (bool, Vec<u32>) {
    let is_ios = runtime.starts_with("iOS");
    let nums = runtime
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse().ok())
        .collect();
    (is_ios, nums)
}

pub fn list_simulators() -> Result<Vec<Value>> {
    Ok(simulators()?
        .into_iter()
        .map(|s| json!({ "udid": s.udid, "name": s.name, "runtime": s.runtime, "state": s.state }))
        .collect())
}

/// The simulator `id` names (UDID or name), or the booted one, or the
/// newest iPhone.
pub fn pick_simulator(id: Option<&str>) -> Result<Simulator> {
    let sims = simulators()?;
    if sims.is_empty() {
        bail!("there are no iOS simulators; add one in Xcode (Window › Devices and Simulators)");
    }
    if let Some(id) = id {
        let id = id.trim();
        if let Some(s) = sims.iter().find(|s| s.udid.eq_ignore_ascii_case(id)) {
            return Ok(s.clone());
        }
        let named: Vec<&Simulator> = sims
            .iter()
            .filter(|s| s.name.eq_ignore_ascii_case(id))
            .collect();
        return named
            .iter()
            .find(|s| s.booted())
            .or(named.first())
            .map(|s| (*s).clone())
            .ok_or_else(|| anyhow!("no simulator \"{id}\"; ios_simulator_list shows them"));
    }
    let booted: Vec<&Simulator> = sims.iter().filter(|s| s.booted()).collect();
    if booted.len() == 1 {
        return Ok(booted[0].clone());
    }
    if booted.len() > 1 {
        bail!(
            "{} simulators are running ({}); say which",
            booted.len(),
            booted
                .iter()
                .map(|s| format!("{} {}", s.name, s.udid))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    sims.iter()
        .find(|s| s.name.starts_with("iPhone"))
        .or(sims.first())
        .cloned()
        .ok_or_else(|| anyhow!("no simulator"))
}

/// Boots `sim` if it isn't running; returns whether this booted it.
pub fn boot(sim: &Simulator, show_window: bool) -> Result<bool> {
    let booted_here = !sim.booted();
    if booted_here {
        log::info!("booting simulator {} ({})", sim.name, sim.udid);
        let mut c = xcrun();
        c.args(["simctl", "boot", &sim.udid]);
        let out = run(c, Duration::from_secs(120))?;
        let err = String::from_utf8_lossy(&out.stderr);
        // It may have booted since we looked.
        if !out.status.success() && !err.contains("current state: Booted") {
            bail!("booting {}: {}", sim.name, err.trim());
        }
    }
    let mut c = xcrun();
    c.args(["simctl", "bootstatus", &sim.udid, "-b"]);
    run_ok(c, Duration::from_secs(300)).context("waiting for the simulator to boot")?;
    if show_window {
        let mut c = Command::new("/usr/bin/open");
        c.args(["-a", "Simulator", "--args", "-CurrentDeviceUDID", &sim.udid]);
        let _ = run(c, Duration::from_secs(20));
    }
    Ok(booted_here)
}

pub fn shutdown(udid: &str) {
    log::info!("shutting down simulator {udid}");
    let mut c = xcrun();
    c.args(["simctl", "shutdown", udid]);
    let _ = run(c, Duration::from_secs(60));
}

/// The apps installed on a simulator: bundle id and name. A simulator
/// that isn't running is booted for the look and shut down again.
pub fn simulator_apps(id: Option<&str>) -> Result<Vec<Value>> {
    let sim = pick_simulator(id)?;
    let booted_here = boot(&sim, false)?;
    let result = (|| {
        let mut c = xcrun();
        c.args(["simctl", "listapps", &sim.udid]);
        let plist = run_ok(c, Duration::from_secs(60))?.stdout;
        let v = plist_to_json(&plist)?;
        let mut apps: Vec<Value> = v
            .as_object()
            .into_iter()
            .flatten()
            .map(|(id, info)| {
                let name = info["CFBundleDisplayName"]
                    .as_str()
                    .or(info["CFBundleName"].as_str())
                    .unwrap_or_default();
                json!({
                    "bundle_id": id,
                    "name": name,
                    "kind": info["ApplicationType"].as_str().unwrap_or_default().to_ascii_lowercase(),
                })
            })
            .filter(|a| a["bundle_id"] != json!(wda::SIM_BUNDLE))
            .collect();
        apps.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(apps)
    })();
    if booted_here {
        shutdown(&sim.udid);
    }
    result
}

/// simctl prints old-style (OpenStep) plists; plutil reads those.
fn plist_to_json(plist: &[u8]) -> Result<Value> {
    let dir = std::env::temp_dir().join(format!("ocu-plist-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("apps.plist");
    std::fs::write(&file, plist)?;
    let mut c = Command::new("/usr/bin/plutil");
    c.args(["-convert", "json", "-r", "-o", "-"]).arg(&file);
    let out = run_ok(c, Duration::from_secs(30));
    let _ = std::fs::remove_dir_all(&dir);
    Ok(serde_json::from_slice(&out?.stdout)?)
}

/// Runs a devicectl command that writes JSON and returns its "result".
fn devicectl(args: &[&str], timeout: Duration) -> Result<Value> {
    let dir = std::env::temp_dir().join(format!(
        "ocu-devicectl-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("out.json");
    let mut c = xcrun();
    c.arg("devicectl")
        .args(args)
        .arg("--json-output")
        .arg(&file);
    let out = run(c, timeout);
    let json = std::fs::read(&file);
    let _ = std::fs::remove_dir_all(&dir);
    let out = out?;
    let v: Value = json
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if !out.status.success() {
        let msg = v["error"]["userInfo"]["NSLocalizedDescription"]["string"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| String::from_utf8_lossy(&out.stderr).trim().to_string());
        bail!(
            "devicectl {}: {}",
            args.first().unwrap_or(&""),
            proc::clip(&msg, 500)
        );
    }
    Ok(v["result"].clone())
}

#[derive(Clone, Debug)]
pub struct Phone {
    /// devicectl's identifier.
    pub id: String,
    /// The hardware UDID xcodebuild wants.
    pub udid: String,
    pub name: String,
    pub model: String,
    pub os_version: String,
    pub paired: bool,
    pub developer_mode: bool,
    pub transport: String,
    pub tunnel: String,
}

pub fn phones() -> Result<Vec<Phone>> {
    if !xcode_installed() {
        return Ok(Vec::new());
    }
    let v = devicectl(&["list", "devices"], Duration::from_secs(30))?;
    Ok(v["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| {
            matches!(
                d["hardwareProperties"]["platform"].as_str(),
                Some("iOS") | Some("iPadOS")
            )
        })
        .map(|d| {
            let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
            Phone {
                id: s(&d["identifier"]),
                udid: s(&d["hardwareProperties"]["udid"]),
                name: s(&d["deviceProperties"]["name"]),
                model: s(&d["hardwareProperties"]["marketingName"]),
                os_version: s(&d["deviceProperties"]["osVersionNumber"]),
                paired: d["connectionProperties"]["pairingState"] == json!("paired"),
                developer_mode: d["deviceProperties"]["developerModeStatus"] == json!("enabled"),
                transport: s(&d["connectionProperties"]["transportType"]),
                tunnel: s(&d["connectionProperties"]["tunnelState"]),
            }
        })
        .collect())
}

pub fn list_phones() -> Result<Vec<Value>> {
    Ok(phones()?
        .into_iter()
        .map(|p| {
            let mut v = json!({
                "id": p.id,
                "platform": "ios",
                "name": p.name,
                "model": p.model,
                "os_version": p.os_version,
                "connection": if p.transport == "wired" { "usb" } else { "network" },
                "state": if p.paired && p.developer_mode { "ready" } else { "needs setup" },
            });
            if !p.paired {
                v["help"] = json!("Connect it by USB, unlock it and tap Trust.");
            } else if !p.developer_mode {
                v["help"] = json!("Turn on Developer Mode: Settings › Privacy & Security › Developer Mode, then restart it.");
            }
            v
        })
        .collect())
}

pub fn pick_phone(id: Option<&str>) -> Result<Option<Phone>> {
    let phones = phones()?;
    Ok(match id {
        Some(id) => phones.into_iter().find(|p| {
            p.id.eq_ignore_ascii_case(id)
                || p.udid.eq_ignore_ascii_case(id)
                || p.name.eq_ignore_ascii_case(id)
                // Curly and straight apostrophes in "Astrid's iPhone".
                || p.name.replace('’', "'").eq_ignore_ascii_case(&id.replace('’', "'"))
        }),
        None => match phones.len() {
            0 => None,
            1 => phones.into_iter().next(),
            _ => bail!(
                "{} iPhones and iPads are known ({}); say which",
                phones.len(),
                phones
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    })
}

pub fn phone_apps(phone: &Phone) -> Result<Vec<Value>> {
    let v = devicectl(
        &["device", "info", "apps", "--device", &phone.id],
        Duration::from_secs(60),
    )?;
    let mut apps: Vec<Value> = v["apps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            json!({
                "bundle_id": a["bundleIdentifier"],
                "name": a["name"],
            })
        })
        .filter(|a| {
            !a["bundle_id"]
                .as_str()
                .unwrap_or_default()
                .ends_with(".xctrunner")
        })
        .collect();
    apps.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(apps)
}

/// Holds on simulators and devices, valued by their WebDriverAgent URL.
pub type Runners = Leases<String>;

/// Boots the simulator (if needed) and starts WebDriverAgent on it.
pub fn acquire_simulator(
    runners: &Runners,
    cache: &Path,
    sim: &Simulator,
    show_window: bool,
) -> Result<crate::lease::Lease<String>> {
    runners.acquire(&sim.udid, || {
        let booted_here = boot(sim, show_window)?;
        let started = (|| {
            let runner = wda::simulator_runner(cache)?;
            let mut c = xcrun();
            c.args(["simctl", "install", &sim.udid]).arg(&runner);
            run_ok(c, Duration::from_secs(120)).context("installing WebDriverAgent")?;
            let port = proc::free_port()?;
            let mut c = xcrun();
            c.args([
                "simctl",
                "launch",
                "--terminate-running-process",
                &sim.udid,
                wda::SIM_BUNDLE,
            ])
            .env("SIMCTL_CHILD_USE_PORT", port.to_string());
            run_ok(c, Duration::from_secs(60)).context("starting WebDriverAgent")?;
            let base = format!("http://127.0.0.1:{port}");
            wda::Wda::new(&base).wait_ready(Duration::from_secs(90))?;
            Ok::<_, anyhow::Error>(base)
        })();
        let base = match started {
            Ok(b) => b,
            Err(e) => {
                if booted_here {
                    shutdown(&sim.udid);
                }
                return Err(e);
            }
        };
        let udid = sim.udid.clone();
        Ok((
            base,
            Some(Box::new(move || {
                let mut c = xcrun();
                c.args(["simctl", "terminate", &udid, wda::SIM_BUNDLE]);
                let _ = run(c, Duration::from_secs(20));
                if booted_here {
                    shutdown(&udid);
                }
            }) as Box<dyn FnOnce() + Send>),
        ))
    })
}

/// Builds WebDriverAgent (once) and runs it on a phone. A device runner
/// lives only inside an XCTest session, so `xcodebuild test-without-building`
/// installs it and keeps it running; WebDriverAgent prints the URL it
/// serves at (the device's network address) to that run's log.
pub fn acquire_phone(
    runners: &Runners,
    cache: &Path,
    phone: &Phone,
) -> Result<crate::lease::Lease<String>> {
    if !phone.paired {
        bail!(
            "{} isn't paired with this Mac: connect it by USB, unlock it and tap Trust",
            phone.name
        );
    }
    if !phone.developer_mode {
        bail!(
            "{} needs Developer Mode: Settings › Privacy & Security › Developer Mode",
            phone.name
        );
    }
    runners.acquire(&phone.id, || {
        let team = wda::team()?;
        match start_device_runner(cache, &team, phone) {
            // A signing problem the profile check didn't see (revoked, or
            // the device's own record of it): build afresh, once.
            Err(e) if is_signing_failure(&format!("{e:#}")) => {
                log::info!("WebDriverAgent didn't launch ({e:#}); rebuilding it");
                wda::forget_device_runner(cache, &team);
                start_device_runner(cache, &team, phone)
            }
            other => other,
        }
    })
}

/// Whether `xcodebuild` refused the runner for its signing.
fn is_signing_failure(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "provisioning profile",
        "0xe80080",
        "code signature",
        "certificate has expired",
        "has expired",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// Builds (or reuses) the runner and starts it on the phone.
fn start_device_runner(
    cache: &Path,
    team: &str,
    phone: &Phone,
) -> Result<(String, Option<crate::lease::Cleanup>)> {
    let runner = wda::device_runner(cache, team, &phone.udid)?;
    let products = runner
        .parent()
        .and_then(Path::parent)
        .context("WebDriverAgent's build has no products folder")?;
    let xctestrun = std::fs::read_dir(products)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "xctestrun"))
        .context("WebDriverAgent's build has no .xctestrun file")?;
    let logs = cache.join("logs");
    std::fs::create_dir_all(&logs)?;
    let log = logs.join(format!("wda-{}.log", phone.udid));
    let mut c = xcrun();
    c.args(["xcodebuild", "test-without-building", "-xctestrun"])
        .arg(&xctestrun)
        .arg("-destination")
        .arg(format!("id={}", phone.udid))
        // Passed to the runner as USE_PORT.
        .env("TEST_RUNNER_USE_PORT", "8100");
    log::info!("starting WebDriverAgent on {}", phone.name);
    let mut child = proc::spawn_detached(c, &log)?;
    let base = match wait_for_device_runner(&mut child, &log, phone) {
        Ok(base) => base,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    Ok((
        base,
        Some(Box::new(move || {
            // Ending the test run stops WebDriverAgent with it.
            let _ = child.kill();
            let _ = child.wait();
        }) as crate::lease::Cleanup),
    ))
}

/// Waits for the device runner to say where it serves, and answers there
/// or at the CoreDevice tunnel.
fn wait_for_device_runner(
    child: &mut std::process::Child,
    log: &Path,
    phone: &Phone,
) -> Result<String> {
    let start = std::time::Instant::now();
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if let Some(url) = text
            .split("ServerURLHere->")
            .nth(1)
            .and_then(|rest| rest.split("<-ServerURLHere").next())
        {
            let url = url.trim().trim_end_matches('/').to_string();
            if wda::Wda::new(&url)
                .wait_ready(Duration::from_secs(15))
                .is_ok()
            {
                return Ok(url);
            }
            // Not reachable on the network (a USB-only connection): the
            // tunnel reaches any port on the device.
            let tunnel = format!("http://{}:8100", tunnel_address(phone)?);
            wda::Wda::new(&tunnel).wait_ready(Duration::from_secs(15))?;
            return Ok(tunnel);
        }
        if let Some(status) = child.try_wait()? {
            if text.contains("Failed to initialize for UI testing") {
                bail!(
                    "{} didn't allow UI automation: unlock it, start the session again, and \
                     approve the prompt to allow it (Face ID, Touch ID or the passcode)",
                    phone.name
                );
            }
            bail!(
                "WebDriverAgent stopped on {} ({status}):\n{}",
                phone.name,
                proc::log_tail(log, 1500)
            );
        }
        let elapsed = start.elapsed();
        if text.contains("is locked") && elapsed > Duration::from_secs(90) {
            bail!(
                "unlock {} and try again: Xcode can't start WebDriverAgent while it is locked",
                phone.name
            );
        }
        if elapsed > Duration::from_secs(240) {
            bail!(
                "WebDriverAgent didn't start on {} in 4 minutes (log: {}):\n{}",
                phone.name,
                log.display(),
                proc::log_tail(log, 1000)
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Where this Mac reaches the device: CoreDevice's tunnel address, which
/// any port on the device answers at.
fn tunnel_address(phone: &Phone) -> Result<String> {
    let v = devicectl(
        &["device", "info", "details", "--device", &phone.id],
        Duration::from_secs(60),
    )?;
    let ip = v["connectionProperties"]["tunnelIPAddress"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{} has no network tunnel to this Mac; reconnect it",
                phone.name
            )
        })?;
    Ok(if ip.contains(':') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    })
}

/// Where downloads and builds are kept.
pub fn cache_dir(base: &Path) -> PathBuf {
    base.join("ios")
}
