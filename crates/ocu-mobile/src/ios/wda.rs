//! WebDriverAgent, Appium's XCUITest server: the only way to drive iOS
//! apps from outside, on simulators and devices alike. It runs on the
//! device as a UI-test runner app and serves HTTP.
//!
//! Simulators use the runner Appium publishes prebuilt, downloaded once.
//! Devices need a runner signed for them, so it is built from WDA's source
//! with the user's Apple development team, once per team.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};

use crate::proc::{self, run, run_ok};

/// The WebDriverAgent release used, unless `OCU_WDA_VERSION` says another.
pub const VERSION: &str = "16.13.0";

pub const SIM_BUNDLE: &str = "com.facebook.WebDriverAgentRunner.xctrunner";

fn version() -> String {
    std::env::var("OCU_WDA_VERSION").unwrap_or_else(|_| VERSION.to_string())
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(90)))
        .build()
        .into()
}

fn download(url: &str, to: &Path) -> Result<()> {
    log::info!("downloading {url}");
    let mut resp = ureq::get(url)
        .header("User-Agent", "opencomputeruse")
        .call()
        .map_err(|e| anyhow!("downloading {url}: {e}"))?;
    let tmp = to.with_extension("part");
    {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::copy(&mut resp.body_mut().as_reader(), &mut file)?;
    }
    std::fs::rename(&tmp, to)?;
    Ok(())
}

/// The prebuilt simulator runner: `OCU_WDA_RUNNER`, else the app's own
/// (an xz tarball scripts/wda-sim-runner.sh puts in Contents/Resources),
/// unpacked into `cache` once, else downloaded into `cache` once.
pub fn simulator_runner(cache: &Path) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("OCU_WDA_RUNNER") {
        return Ok(PathBuf::from(p));
    }
    // The bundled one is VERSION; another version is asked for by name.
    if version() == VERSION {
        if let Some(tarball) = std::env::current_exe().ok().and_then(|exe| {
            let t = exe
                .parent()?
                .join("../Resources/WebDriverAgentRunner-Runner.tar.xz");
            t.is_file().then_some(t)
        }) {
            return unpack_bundled(&tarball, cache);
        }
    }
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    let v = version();
    let dir = cache.join("wda").join(&v).join(format!("sim-{arch}"));
    let app = dir.join("WebDriverAgentRunner-Runner.app");
    if app.join("Info.plist").is_file() {
        return Ok(app);
    }
    std::fs::create_dir_all(&dir)?;
    let zip = dir.join("runner.zip");
    download(
        &format!("https://github.com/appium/WebDriverAgent/releases/download/v{v}/WebDriverAgentRunner-Build-Sim-{arch}.zip"),
        &zip,
    )
    .context("fetching WebDriverAgent for the simulator")?;
    let mut c = Command::new("/usr/bin/ditto");
    c.args(["-x", "-k"]).arg(&zip).arg(&dir);
    run_ok(c, Duration::from_secs(120))?;
    let _ = std::fs::remove_file(&zip);
    if !app.join("Info.plist").is_file() {
        bail!("WebDriverAgent's download had no WebDriverAgentRunner-Runner.app");
    }
    Ok(app)
}

/// Unpacks the app's runner tarball, once per tarball: the folder is named
/// for its hash, so an update that ships another runner gets a fresh copy.
fn unpack_bundled(tarball: &Path, cache: &Path) -> Result<PathBuf> {
    use sha2::Digest as _;
    let data = std::fs::read(tarball)?;
    let hash: String = sha2::Sha256::digest(&data)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let dir = cache
        .join("wda")
        .join(VERSION)
        .join(format!("bundled-{hash}"));
    let app = dir.join("WebDriverAgentRunner-Runner.app");
    if app.join("Info.plist").is_file() {
        return Ok(app);
    }
    // Into a scratch folder first, so a half-unpacked runner is never used.
    let part = dir.with_extension("part");
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part)?;
    let mut c = Command::new("/usr/bin/tar");
    c.arg("-xJf").arg(tarball).arg("-C").arg(&part);
    run_ok(c, Duration::from_secs(120)).context("unpacking the bundled WebDriverAgent")?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&part, &dir)?;
    if !app.join("Info.plist").is_file() {
        bail!("the bundled WebDriverAgent had no WebDriverAgentRunner-Runner.app");
    }
    Ok(app)
}

/// WebDriverAgent's source, for building a device runner.
fn source(cache: &Path) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("OCU_WDA_SOURCE") {
        return Ok(PathBuf::from(p));
    }
    let v = version();
    let dir = cache.join("wda").join(&v).join("src");
    let project = dir.join(format!("WebDriverAgent-{v}"));
    if project.join("WebDriverAgent.xcodeproj").is_dir() {
        return Ok(project);
    }
    std::fs::create_dir_all(&dir)?;
    let tgz = dir.join("source.tar.gz");
    download(
        &format!("https://github.com/appium/WebDriverAgent/archive/refs/tags/v{v}.tar.gz"),
        &tgz,
    )
    .context("fetching WebDriverAgent's source")?;
    let mut c = Command::new("/usr/bin/tar");
    c.arg("-xzf").arg(&tgz).arg("-C").arg(&dir);
    run_ok(c, Duration::from_secs(120))?;
    let _ = std::fs::remove_file(&tgz);
    if !project.join("WebDriverAgent.xcodeproj").is_dir() {
        bail!("WebDriverAgent's source had no WebDriverAgent.xcodeproj");
    }
    Ok(project)
}

/// The bundle id a team's device runner gets: WDA's own is taken.
pub fn device_bundle_base(team: &str) -> String {
    format!(
        "com.infrawrench.opencomputeruse.wda.{}",
        team.to_ascii_lowercase()
    )
}

/// Builds (once per team) and returns the device runner .app, signed by
/// `team`. Xcode must be signed in to an account in that team; it
/// registers the device and makes the provisioning profile itself.
pub fn device_runner(cache: &Path, team: &str, device_udid: &str) -> Result<PathBuf> {
    let derived = device_build_dir(cache, team);
    let app = derived.join("Build/Products/Debug-iphoneos/WebDriverAgentRunner-Runner.app");
    if app.join("Info.plist").is_file() {
        // A profile lasts a week on a free account, and names the devices
        // it was made for: a new one is made by building again.
        match stale_profile(&app, device_udid, std::time::SystemTime::now()) {
            None => return Ok(app),
            Some(why) => {
                log::info!("rebuilding WebDriverAgent: {why}");
                forget_device_runner(cache, team);
            }
        }
    }
    let project = source(cache)?;
    log::info!("building WebDriverAgent for team {team}; this takes a few minutes once");
    let log = derived.with_extension("build.log");
    std::fs::create_dir_all(&derived)?;
    let mut c = Command::new("/usr/bin/xcrun");
    c.arg("xcodebuild")
        .arg("build-for-testing")
        .arg("-project")
        .arg(project.join("WebDriverAgent.xcodeproj"))
        .args(["-scheme", "WebDriverAgentRunner"])
        .arg("-destination")
        .arg(format!("id={device_udid}"))
        .arg("-derivedDataPath")
        .arg(&derived)
        .arg("-allowProvisioningUpdates")
        .arg("-allowProvisioningDeviceRegistration")
        .arg(format!("DEVELOPMENT_TEAM={team}"))
        .arg("CODE_SIGN_STYLE=Automatic")
        .arg("CODE_SIGN_IDENTITY=Apple Development")
        .arg(format!(
            "PRODUCT_BUNDLE_IDENTIFIER={}",
            device_bundle_base(team)
        ));
    let out = run(c, Duration::from_secs(1200))?;
    let _ = std::fs::write(&log, [&out.stdout[..], &out.stderr[..]].concat());
    if !out.status.success() || !app.join("Info.plist").is_file() {
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let errors: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("error:"))
            .take(6)
            .collect();
        bail!(
            "building WebDriverAgent for the device failed (full log: {}):\n{}\n\
             Xcode needs to be signed in to an account in team {team} (Xcode › Settings › Accounts), \
             and the device unlocked, trusted and in Developer Mode.",
            log.display(),
            errors.join("\n")
        );
    }
    Ok(app)
}

fn device_build_dir(cache: &Path, team: &str) -> PathBuf {
    cache
        .join("wda")
        .join(version())
        .join(format!("device-{team}"))
}

/// Drops a team's device runner, so the next session builds it again.
pub fn forget_device_runner(cache: &Path, team: &str) {
    let _ = std::fs::remove_dir_all(device_build_dir(cache, team));
}

/// Why the runner's provisioning profile won't do for `udid` at `now`:
/// expired (or about to be), or made before this device was registered.
fn stale_profile(app: &Path, udid: &str, now: std::time::SystemTime) -> Option<String> {
    let mut c = Command::new("/usr/bin/security");
    c.args(["cms", "-D", "-i"])
        .arg(app.join("embedded.mobileprovision"));
    let xml = match run_ok(c, Duration::from_secs(10)) {
        Ok(out) => out.text(),
        Err(e) => return Some(format!("its provisioning profile is unreadable ({e:#})")),
    };
    profile_problem(&xml, udid, now)
}

fn profile_problem(xml: &str, udid: &str, now: std::time::SystemTime) -> Option<String> {
    let after = |key: &str| xml.split(&format!("<key>{key}</key>")).nth(1);
    let expires = after("ExpirationDate")
        .and_then(|rest| rest.split("<date>").nth(1))
        .and_then(|rest| rest.split("</date>").next())
        .and_then(parse_iso8601);
    let Some(expires) = expires else {
        return Some("its provisioning profile has no expiry date".into());
    };
    // An hour's margin, so it doesn't lapse mid-session.
    if expires <= now + Duration::from_secs(3600) {
        return Some("its provisioning profile has expired".into());
    }
    // Enterprise profiles provision every device and list none.
    if after("ProvisionsAllDevices").is_some_and(|r| r.trim_start().starts_with("<true/>")) {
        return None;
    }
    let devices = after("ProvisionedDevices")
        .and_then(|rest| rest.split("</array>").next())
        .unwrap_or_default();
    if !devices.contains(&format!("<string>{udid}</string>")) {
        return Some(format!(
            "its provisioning profile doesn't include device {udid}"
        ));
    }
    None
}

/// "2027-09-13T17:06:05Z", as plists write dates.
fn parse_iso8601(s: &str) -> Option<std::time::SystemTime> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).ok()?))
}

/// The development team to sign device runners with: `OCU_IOS_TEAM`, or
/// the team of the first "Apple Development" certificate in the keychain.
pub fn team() -> Result<String> {
    if let Ok(t) = std::env::var("OCU_IOS_TEAM") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    let mut c = Command::new("/usr/bin/security");
    c.args(["find-certificate", "-c", "Apple Development", "-p"]);
    let pem = run_ok(c, Duration::from_secs(10))
        .map(|o| o.stdout)
        .unwrap_or_default();
    if !pem.is_empty() {
        let dir = std::env::temp_dir().join(format!("ocu-cert-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("cert.pem");
        std::fs::write(&file, &pem)?;
        let mut c = Command::new("/usr/bin/openssl");
        c.args(["x509", "-noout", "-subject", "-in"]).arg(&file);
        let subject = run_ok(c, Duration::from_secs(10)).map(|o| o.text());
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(team) = subject.ok().as_deref().and_then(organizational_unit) {
            return Ok(team);
        }
    }
    bail!(
        "no Apple development team found to sign WebDriverAgent with: sign in to Xcode with your \
         Apple account (Xcode › Settings › Accounts) or set OCU_IOS_TEAM to your team id"
    )
}

/// The OU of a certificate subject, in either openssl's style.
fn organizational_unit(subject: &str) -> Option<String> {
    let i = subject.find("OU")?;
    let rest = subject[i + 2..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let end = rest.find([',', '/', '\n']).unwrap_or(rest.len());
    let ou = rest[..end].trim();
    (!ou.is_empty()).then(|| ou.to_string())
}

/// A WebDriverAgent server and the session this client holds on it.
pub struct Wda {
    pub base: String,
    agent: ureq::Agent,
    session: Option<String>,
}

impl Wda {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            agent: http(),
            session: None,
        }
    }

    /// Waits until the server answers /status as ready.
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let quick: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(3)))
            .build()
            .into();
        loop {
            let ready = quick
                .get(&format!("{}/status", self.base))
                .call()
                .ok()
                .and_then(|mut r| r.body_mut().read_json::<Value>().ok())
                .is_some_and(|v| v["value"]["ready"].as_bool() == Some(true));
            if ready {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "WebDriverAgent at {} didn't become ready in {}s",
                    self.base,
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    pub fn is_ready(&self) -> bool {
        self.wait_ready(Duration::from_millis(1)).is_ok()
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let resp = match (method, body) {
            ("GET", _) => self.agent.get(&url).call(),
            ("DELETE", _) => self.agent.delete(&url).call(),
            (_, b) => self.agent.post(&url).send_json(b.unwrap_or(&json!({}))),
        };
        let mut resp = resp.map_err(|e| anyhow!("WebDriverAgent ({path}): {e}"))?;
        let status = resp.status();
        let v: Value = resp
            .body_mut()
            .read_json()
            .with_context(|| format!("WebDriverAgent's answer to {path}"))?;
        let value = v.get("value").cloned().unwrap_or(Value::Null);
        if let Some(err) = value.get("error").and_then(Value::as_str) {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Its messages can carry a whole stack trace.
            let message = message.lines().next().unwrap_or_default();
            bail!(WdaError {
                kind: err.to_string(),
                message: proc::clip(message, 400),
            });
        }
        if !status.is_success() {
            bail!("WebDriverAgent answered {status} to {path}");
        }
        Ok(value)
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        self.request("GET", path, None)
    }

    pub fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request("POST", path, Some(&body))
    }

    /// The session-scoped path, starting a session when there is none (or
    /// another client's session replaced ours).
    fn session_path(&mut self, path: &str) -> Result<String> {
        if self.session.is_none() {
            let v = self.post(
                "/session",
                json!({ "capabilities": { "alwaysMatch": { "shouldWaitForQuiescence": false } } }),
            )?;
            let id = v
                .get("sessionId")
                .and_then(Value::as_str)
                .context("WebDriverAgent started no session")?;
            self.session = Some(id.to_string());
        }
        Ok(format!("/session/{}{path}", self.session.as_ref().unwrap()))
    }

    /// A call within the session, retried once on a fresh session.
    pub fn call(&mut self, path: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..2 {
            let full = self.session_path(path)?;
            let result = match &body {
                Some(b) => self.post(&full, b.clone()),
                None => self.get(&full),
            };
            match result {
                Err(e)
                    if attempt == 0
                        && e.downcast_ref::<WdaError>()
                            .is_some_and(|w| w.kind == "invalid session id") =>
                {
                    self.session = None;
                }
                other => return other,
            }
        }
        unreachable!()
    }

    pub fn end_session(&mut self) {
        if let Some(id) = self.session.take() {
            let _ = self.request("DELETE", &format!("/session/{id}"), None);
        }
    }
}

#[derive(Debug)]
pub struct WdaError {
    pub kind: String,
    pub message: String,
}

impl std::fmt::Display for WdaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "WebDriverAgent: {}", self.kind)
        } else {
            write!(f, "WebDriverAgent: {}: {}", self.kind, self.message)
        }
    }
}

impl std::error::Error for WdaError {}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = "<plist><dict><key>ExpirationDate</key>\n\t<date>2027-09-13T17:06:05Z</date>\n<key>ProvisionedDevices</key>\n<array>\n\t<string>00008110-001A655A2E13601E</string>\n</array></dict></plist>";

    #[test]
    fn profiles_go_stale() {
        let t = |s| parse_iso8601(s).unwrap();
        assert_eq!(
            t("1970-01-02T00:00:01Z"),
            std::time::UNIX_EPOCH + Duration::from_secs(86401)
        );
        // 2027-09-13T17:06:05Z is 1820855165 seconds after the epoch.
        assert_eq!(
            t("2027-09-13T17:06:05Z"),
            std::time::UNIX_EPOCH + Duration::from_secs(1_820_855_165)
        );
        let before = t("2026-10-09T00:00:00Z");
        assert_eq!(
            profile_problem(PROFILE, "00008110-001A655A2E13601E", before),
            None
        );
        assert!(
            profile_problem(PROFILE, "00008030-000000000000002E", before)
                .unwrap()
                .contains("doesn't include")
        );
        let after = t("2027-09-13T16:30:00Z");
        assert!(profile_problem(PROFILE, "00008110-001A655A2E13601E", after)
            .unwrap()
            .contains("expired"));
        let everyone = PROFILE.replace(
            "<key>ProvisionedDevices</key>",
            "<key>ProvisionsAllDevices</key><true/><key>x</key>",
        );
        assert_eq!(profile_problem(&everyone, "any", before), None);
    }

    #[test]
    fn team_from_subject() {
        assert_eq!(
            organizational_unit(
                "subject=UID=X, CN=Apple Development: A (Y), OU=CS54L4CF2Z, O=A, C=GB"
            )
            .as_deref(),
            Some("CS54L4CF2Z")
        );
        assert_eq!(
            organizational_unit("subject= /UID=X/CN=Apple Development/OU=ABCDE12345/O=A")
                .as_deref(),
            Some("ABCDE12345")
        );
        assert_eq!(organizational_unit("subject=CN=x"), None);
    }
}
