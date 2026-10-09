//! Mobile devices for opencomputeruse: iOS simulators (macOS), Android
//! emulators, and physical phones and tablets (Android anywhere adb runs;
//! iPhones and iPads on macOS). One [`MobilePlatform`] starts sessions on
//! all of them; [`LaunchSpec::device`] says which.
//!
//! Sessions drive apps through the same [`Session`] trait as the desktop
//! backends, in points (iOS) or density-independent pixels (Android), with
//! screenshots scaled to match.

pub mod android;
mod elements;
#[cfg(target_os = "macos")]
pub mod ios;
mod keys;
mod lease;
mod picture;
mod proc;

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use ocu_core::{DeviceKind, DeviceQuery, LaunchSpec, Platform, Session};

pub use keys::HELP as KEYS_HELP;

pub struct MobileConfig {
    /// Where downloads (WebDriverAgent, the Android helper) and emulator
    /// logs go.
    pub cache: PathBuf,
    /// This build's version, to fetch the matching Android helper.
    pub version: String,
    /// The Android helper's APK when it isn't the release's: a local build.
    pub android_apk: Option<PathBuf>,
}

pub struct MobilePlatform {
    config: MobileConfig,
    android: lease::Leases<()>,
    #[cfg(target_os = "macos")]
    runners: ios::Runners,
}

impl MobilePlatform {
    pub fn new(config: MobileConfig) -> Self {
        Self {
            config,
            android: Default::default(),
            #[cfg(target_os = "macos")]
            runners: Default::default(),
        }
    }

    /// The Android helper: `OCU_ANDROID_APK`, the configured build, a copy
    /// next to this executable, or the release's, downloaded once.
    fn android_apk(&self) -> std::result::Result<PathBuf, String> {
        if let Some(p) = std::env::var_os("OCU_ANDROID_APK") {
            return Ok(PathBuf::from(p));
        }
        if let Some(p) = self.config.android_apk.as_ref().filter(|p| p.is_file()) {
            return Ok(p.clone());
        }
        if let Some(p) = std::env::current_exe().ok().and_then(|exe| {
            let dir = exe.parent()?.to_path_buf();
            [
                dir.join("OpenComputerUse.apk"),
                dir.join("../Resources/OpenComputerUse.apk"),
            ]
            .into_iter()
            .find(|p| p.is_file())
        }) {
            return Ok(p);
        }
        let dir = self.config.cache.join("android").join(&self.config.version);
        let apk = dir.join("OpenComputerUse.apk");
        if apk.is_file() {
            return Ok(apk);
        }
        let url = format!(
            "https://github.com/IAmJSD/OpenComputerUse/releases/download/v{}/OpenComputerUse.apk",
            self.config.version
        );
        let fetched = (|| -> Result<()> {
            std::fs::create_dir_all(&dir)?;
            let mut resp = ureq::get(&url)
                .header("User-Agent", "opencomputeruse")
                .call()
                .map_err(|e| anyhow!("{e}"))?;
            let part = apk.with_extension("part");
            std::io::copy(
                &mut resp.body_mut().as_reader(),
                &mut std::fs::File::create(&part)?,
            )?;
            std::fs::rename(&part, &apk)?;
            Ok(())
        })();
        match fetched {
            Ok(()) => Ok(apk),
            Err(e) => Err(format!(
                "the OpenComputerUse helper for Android couldn't be fetched ({e:#}), so the app \
                 runs on the device's own screen"
            )),
        }
    }

    fn launch_android(
        &self,
        spec: &LaunchSpec,
        serial: String,
        cleanup: Option<lease::Cleanup>,
        backend: &'static str,
    ) -> Result<Box<dyn Session>> {
        let lease = self.android.acquire(&serial, || Ok(((), cleanup)))?;
        let adb = android::Adb::new(&serial)?;
        let (apk, apk_missing) = match self.android_apk() {
            Ok(p) => (Some(p), None),
            Err(why) => (None, Some(why)),
        };
        Ok(Box::new(android::session::AndroidSession::start(
            android::session::Start {
                adb,
                spec,
                lease,
                apk,
                backend,
                apk_missing,
            },
        )?))
    }

    #[cfg(target_os = "macos")]
    fn launch_ios_simulator(
        &self,
        spec: &LaunchSpec,
        id: Option<&str>,
        show: bool,
    ) -> Result<Box<dyn Session>> {
        let sim = ios::pick_simulator(id)?;
        let lease = ios::acquire_simulator(
            &self.runners,
            &ios::cache_dir(&self.config.cache),
            &sim,
            show,
        )?;
        let mut notes = std::collections::BTreeMap::new();
        notes.insert(
            "simulator".into(),
            format!("{} ({})", sim.name, sim.runtime),
        );
        notes.insert("udid".into(), sim.udid.clone());
        Ok(Box::new(ios::session::IosSession::start(
            lease,
            spec,
            "ios-simulator",
            notes,
        )?))
    }

    #[cfg(target_os = "macos")]
    fn launch_iphone(&self, spec: &LaunchSpec, phone: ios::Phone) -> Result<Box<dyn Session>> {
        let lease = ios::acquire_phone(&self.runners, &ios::cache_dir(&self.config.cache), &phone)?;
        let mut notes = std::collections::BTreeMap::new();
        notes.insert("device".into(), phone.name.clone());
        notes.insert("id".into(), phone.id.clone());
        Ok(Box::new(ios::session::IosSession::start(
            lease, spec, "ios", notes,
        )?))
    }

    fn launch_phone(&self, spec: &LaunchSpec, id: Option<&str>) -> Result<Box<dyn Session>> {
        let androids: Vec<android::AdbDevice> = if android::adb_path().is_some() {
            android::adb_devices()?
                .into_iter()
                .filter(|d| !d.is_emulator())
                .collect()
        } else {
            Vec::new()
        };
        #[cfg(target_os = "macos")]
        let iphones = ios::phones().unwrap_or_default();
        if let Some(id) = id {
            if androids
                .iter()
                .any(|d| d.serial == id || d.model.eq_ignore_ascii_case(id))
            {
                let d = android::pick_device(Some(id), false)?;
                return self.launch_android(spec, d.serial, None, "android");
            }
            #[cfg(target_os = "macos")]
            if let Some(p) = ios::pick_phone(Some(id))? {
                return self.launch_iphone(spec, p);
            }
            bail!("no phone \"{id}\" is connected; phone_list shows them");
        }
        let ready_androids = androids.iter().filter(|d| d.state == "device").count();
        #[cfg(target_os = "macos")]
        let ios_count = iphones.len();
        #[cfg(not(target_os = "macos"))]
        let ios_count = 0;
        match (ready_androids, ios_count) {
            (0, 0) => bail!(no_phone_help()),
            (1, 0) => {
                let d = android::pick_device(None, false)?;
                self.launch_android(spec, d.serial, None, "android")
            }
            #[cfg(target_os = "macos")]
            (0, 1) => self.launch_iphone(spec, iphones.into_iter().next().unwrap()),
            _ => bail!("several phones are connected; say which (phone_list shows them)"),
        }
    }
}

fn no_phone_help() -> String {
    let mut s = String::from(
        "no phone is connected. Android: turn on USB debugging (Settings › About phone › tap \
         Build number seven times, then Developer options › USB debugging), connect it and \
         accept the prompt.",
    );
    if cfg!(target_os = "macos") {
        s.push_str(
            " iPhone or iPad: connect it, tap Trust, and turn on Developer Mode \
             (Settings › Privacy & Security).",
        );
    }
    s
}

#[cfg(not(target_os = "macos"))]
fn macos_only() -> anyhow::Error {
    anyhow!("iOS simulators need a Mac with Xcode")
}

impl Platform for MobilePlatform {
    fn name(&self) -> &'static str {
        "mobile"
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn Session>> {
        let device = spec
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("no device named"))?;
        let id = device
            .id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match device.kind {
            DeviceKind::IosSimulator => {
                #[cfg(target_os = "macos")]
                return self.launch_ios_simulator(spec, id, device.show_window);
                #[cfg(not(target_os = "macos"))]
                Err(macos_only())
            }
            DeviceKind::AndroidEmulator => {
                let want = match id {
                    Some(id) => android::emulator::Want::Named(id),
                    None => android::emulator::Want::Any,
                };
                let (serial, cleanup) = android::emulator::ensure_running(
                    want,
                    device.show_window,
                    &self.config.cache.join("logs"),
                )?;
                self.launch_android(spec, serial, cleanup, "android-emulator")
            }
            DeviceKind::Phone => self.launch_phone(spec, id),
        }
    }

    fn query_devices(&self, query: &DeviceQuery) -> Result<Value> {
        match query {
            DeviceQuery::List { kind } => list(*kind),
            DeviceQuery::Apps { kind, id } => {
                let id = id.as_deref().map(str::trim).filter(|s| !s.is_empty());
                apps(*kind, id)
            }
        }
    }
}

fn list(kind: DeviceKind) -> Result<Value> {
    Ok(match kind {
        DeviceKind::IosSimulator => {
            #[cfg(target_os = "macos")]
            {
                json!(ios::list_simulators()?)
            }
            #[cfg(not(target_os = "macos"))]
            return Err(macos_only());
        }
        DeviceKind::AndroidEmulator => json!(android::emulator::list()?),
        DeviceKind::Phone => {
            let mut all = android::phones()?;
            #[cfg(target_os = "macos")]
            all.extend(ios::list_phones()?);
            json!(all)
        }
    })
}

fn apps(kind: DeviceKind, id: Option<&str>) -> Result<Value> {
    Ok(match kind {
        DeviceKind::IosSimulator => {
            #[cfg(target_os = "macos")]
            {
                json!(ios::simulator_apps(id)?)
            }
            #[cfg(not(target_os = "macos"))]
            return Err(macos_only());
        }
        DeviceKind::AndroidEmulator => {
            let running = android::emulator::running()?;
            let serial = match id {
                Some(id) => running
                    .iter()
                    .find(|(s, n)| s == id || n == id)
                    .map(|(s, _)| s.clone())
                    .ok_or_else(|| {
                        anyhow!("emulator \"{id}\" isn't running; android_emulator_start_session boots it")
                    })?,
                None => match running.as_slice() {
                    [(s, _)] => s.clone(),
                    [] => bail!("no emulator is running; android_emulator_start_session boots one"),
                    _ => bail!("several emulators are running; say which"),
                },
            };
            json!(android::apps(&android::Adb::new(&serial)?)?)
        }
        DeviceKind::Phone => {
            #[cfg(target_os = "macos")]
            {
                let is_android = android::adb_path().is_some()
                    && android::adb_devices()?.iter().any(|d| {
                        !d.is_emulator()
                            && id
                                .is_none_or(|id| d.serial == id || d.model.eq_ignore_ascii_case(id))
                    });
                if !is_android {
                    if let Some(p) = ios::pick_phone(id)? {
                        return Ok(json!(ios::phone_apps(&p)?));
                    }
                }
            }
            let d = android::pick_device(id, false)?;
            json!(android::apps(&android::Adb::new(&d.serial)?)?)
        }
    })
}

/// Whether there is an Android emulator to offer tools for.
pub fn android_emulator_installed() -> bool {
    android::emulator_installed()
}
