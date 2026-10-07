//! Update checks, and installing one over this copy where the platform
//! allows it. Ported from VisualHub's updater (itself from Schist's).
//!
//! The check is the settings window's Check for Updates (or the app
//! menu's), or the daily background check the "Check for updates
//! automatically" setting governs, and it asks GitHub for one JSON document
//! and nothing else. A download only starts once the user presses Update.
//!
//! Installing happens on macOS only: the release's `OpenComputerUse.zip` is
//! unpacked next to the running bundle and swapped in with a rename, after
//! its signature is checked against this copy's, and the relauncher opens
//! the new bundle once this process exits. MCP servers already running from
//! the old bundle keep going, and reconnect to the new agent on their next
//! request. Elsewhere the MCP server is a bare binary a client starts, so
//! the update check only points at the release.
use anyhow::Context as _;
use serde_json::Value;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Where releases are published.
const RELEASES_API: &str = "https://api.github.com/repos/IAmJSD/OpenComputerUse/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/IAmJSD/OpenComputerUse/releases";

/// This build's version.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A newer release than this build.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub version: String,
    /// The release's own page — "what's new", and where someone whose
    /// copy we must not touch goes to get it.
    pub page: String,
    /// The asset this copy can install over itself, when there is one.
    /// `None` on Linux, and on any build that isn't where its installer
    /// would have put it.
    pub install: Option<Installer>,
}

/// The release asset that updates this platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Installer {
    pub url: String,
    pub file_name: String,
    /// What the release says the download weighs, for the progress bar
    /// and as the cap on what we will read.
    pub size: u64,
    /// Lower-case hex, when the release recorded one.
    pub sha256: Option<String>,
}

/// The outcome of an update check.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    UpToDate,
    Available(Update),
    Failed(String),
}

/// Where an update in progress has got to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Progress {
    Downloading { received: u64, total: u64 },
    Installing,
}

/// Compare two `x.y.z` version strings. Returns true when `candidate` is
/// newer than `current`; anything unparseable compares as "not newer" so a
/// malformed tag never nags the user.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    fn parts(v: &str) -> Option<(u32, u32, u32)> {
        let v = v.trim().trim_start_matches('v');
        let mut it = v.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().ok()?;
        // Trailing pre-release suffixes ("1.2.3-beta") are ignored.
        let patch = it
            .next()
            .unwrap_or("0")
            .split(['-', '+'])
            .next()?
            .parse()
            .ok()?;
        Some((major, minor, patch))
    }
    match (parts(current), parts(candidate)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

/// Where to ask; `OCU_RELEASES_API` points it elsewhere (a local stand-in
/// for GitHub, when testing an update end to end).
fn releases_api() -> String {
    std::env::var("OCU_RELEASES_API").unwrap_or_else(|_| RELEASES_API.to_string())
}

/// Ask GitHub for the latest release. Blocking — call it off the UI thread.
pub fn check() -> UpdateStatus {
    let response = ureq::get(&releases_api())
        .header("User-Agent", "opencomputeruse-update-check")
        .header("Accept", "application/vnd.github+json")
        .call();
    let release: Value = match response {
        Ok(mut r) => match r
            .body_mut()
            .read_to_string()
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str(&text)?))
        {
            Ok(v) => v,
            Err(err) => {
                return UpdateStatus::Failed(format!("GitHub's answer was unreadable: {err}"))
            }
        },
        // A repository with no releases yet (or not public yet) answers 404.
        Err(ureq::Error::StatusCode(404)) => {
            return UpdateStatus::Failed("no releases are published yet".into())
        }
        Err(err) => return UpdateStatus::Failed(format!("{err}")),
    };
    let tag = s(&release, "tag_name");
    if !is_newer(current_version(), &tag) {
        return UpdateStatus::UpToDate;
    }
    let page = s(&release, "html_url");
    UpdateStatus::Available(Update {
        version: tag.trim_start_matches('v').to_string(),
        page: if page.is_empty() {
            RELEASES_PAGE.to_string()
        } else {
            page
        },
        // Offer to install only when this copy is one we can replace;
        // otherwise the dialog is a pointer at the release page.
        install: if self_installable() {
            installer_for(&release)
        } else {
            None
        },
    })
}

/// The release asset that would update this platform, ignoring whether
/// this particular copy is one we may replace.
fn installer_for(release: &Value) -> Option<Installer> {
    let asset = release
        .get("assets")
        .and_then(Value::as_array)?
        .iter()
        .find(|a| is_platform_asset(&s(a, "name")))?;
    let url = s(asset, "browser_download_url");
    if url.is_empty() {
        return None;
    }
    Some(Installer {
        url,
        file_name: s(asset, "name"),
        size: asset.get("size").and_then(Value::as_u64).unwrap_or(0),
        sha256: sha256_from_digest(&s(asset, "digest")),
    })
}

/// A string field, or "" when it is missing.
fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Whether a release asset is the one that installs this platform.
///
/// The name comes from `scripts/bundle-macos.sh`; changing it there
/// without changing it here silently ends self-updating.
fn is_platform_asset(name: &str) -> bool {
    cfg!(target_os = "macos") && name == "OpenComputerUse.zip"
}

/// The hex digest out of GitHub's `sha256:…` field, if it holds one.
/// Absent on older releases, so it is checked when present rather than
/// required.
fn sha256_from_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

/// Whether this copy is one we can replace in place: an
/// application bundle we can write next to on macOS.

pub fn self_installable() -> bool {
    #[cfg(target_os = "macos")]
    {
        bundle_path().is_some_and(|app| app.parent().is_some_and(is_writable))
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Fetch the installer, counting bytes into `received` as they land.
/// Blocking; the caller runs it on a background thread.
pub fn download(installer: &Installer, received: &AtomicU64) -> anyhow::Result<PathBuf> {
    use sha2::Digest as _;

    let dir = download_dir();
    // A previous attempt's half-file must not be mistaken for this one.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let name = Path::new(&installer.file_name)
        .file_name()
        .context("the release's file name has a path in it")?;
    let path = dir.join(name);

    let mut response = ureq::get(&installer.url)
        .header("User-Agent", "opencomputeruse-update")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = std::fs::File::create(&path)?;
    // A guard against a redirect to something enormous. The release
    // tells us the size, so this is only a fallback for one that didn't.
    let cap = if installer.size > 0 {
        installer.size
    } else {
        1 << 30
    };
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        anyhow::ensure!(
            total <= cap,
            "the download is larger than the release says it is"
        );
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        received.store(total, Ordering::Relaxed);
    }
    file.sync_all()?;
    drop(file);

    if installer.size > 0 {
        anyhow::ensure!(
            total == installer.size,
            "the download stopped at {total} of {} bytes",
            installer.size
        );
    }
    if let Some(want) = &installer.sha256 {
        let got = format!("{:x}", hasher.finalize());
        anyhow::ensure!(
            &got == want,
            "the download's SHA-256 is {got}, not the {want} the release lists"
        );
    }
    Ok(path)
}

/// Where the installer is downloaded to. Not the install location: on
/// macOS the bundle is unpacked next to the one it replaces, since a
/// rename across volumes is not one.
fn download_dir() -> PathBuf {
    std::env::temp_dir().join("opencomputeruse-update")
}

/// Throw away whatever [`download`] left behind.
pub fn clean_downloads() {
    let _ = std::fs::remove_dir_all(download_dir());
}

// ---------------------------------------------------------------- macOS

/// The application bundle this build is running out of.
#[cfg(target_os = "macos")]
fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // …/OpenComputerUse.app/Contents/MacOS/opencomputeruse
    let app = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))?;
    Some(app.to_path_buf())
}

/// Whether we may create things in `dir`. `Permissions::readonly` reads
/// the mode bits rather than what this user can actually do with them,
/// so ask the filesystem instead.
#[cfg(target_os = "macos")]
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".opencomputeruse-write-probe-{}", std::process::id()));
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Unpack `zip` over the running bundle and arrange for the new one to
/// start once this process exits.
#[cfg(target_os = "macos")]
pub fn install_and_restart(zip: &Path) -> anyhow::Result<()> {
    let app = bundle_path().context("OpenComputerUse isn't running from an application bundle")?;
    let parent = app
        .parent()
        .context("the application bundle has no folder")?;
    // Staged beside the bundle, not in the temporary directory: the swap
    // below is a rename, and a rename cannot cross volumes.
    let stage = parent.join(format!(".opencomputeruse-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir(&stage).with_context(|| format!("can't write to {}", parent.display()))?;

    let unpacked = unpack(zip, &stage).and_then(|new_app| {
        verify_signature(&app, &new_app)?;
        Ok(new_app)
    });
    let new_app = match unpacked {
        Ok(app) => app,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(err);
        }
    };

    // The running bundle steps aside rather than being deleted, so a
    // failed swap can be undone.
    let backup = parent.join(format!(".OpenComputerUse.app.old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&app, &backup)
        .with_context(|| format!("can't move {} aside", app.display()))?;
    if let Err(err) = std::fs::rename(&new_app, &app) {
        let _ = std::fs::rename(&backup, &app);
        let _ = std::fs::remove_dir_all(&stage);
        return Err(anyhow::Error::new(err).context("can't move the new version into place"));
    }
    let _ = std::fs::remove_dir_all(&backup);
    let _ = std::fs::remove_dir_all(&stage);
    clean_downloads();

    relaunch(&app)
}

/// Extract the release zip into `stage`, returning the bundle inside it.
#[cfg(target_os = "macos")]
fn unpack(zip: &Path, stage: &Path) -> anyhow::Result<PathBuf> {
    // ditto, not unzip: it is what wrote the archive, and it is the only
    // one of the two that keeps the symlinks and modes a signature is
    // taken over.
    let out = std::process::Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(zip)
        .arg(stage)
        .output()
        .context("can't run ditto")?;
    anyhow::ensure!(
        out.status.success(),
        "unpacking the update failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let app = stage.join("OpenComputerUse.app");
    anyhow::ensure!(
        app.join("Contents/MacOS/opencomputeruse").is_file(),
        "the download has no OpenComputerUse.app in it"
    );
    Ok(app)
}

/// A bundle's signing team: `None` when it carries no signature at all,
/// `Some(None)` when it is signed without one (an ad-hoc or local build).
#[cfg(target_os = "macos")]
fn signing_team(app: &Path) -> Option<Option<String>> {
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=4"])
        .arg(app)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // codesign prints the description on stderr.
    let text = String::from_utf8_lossy(&out.stderr);
    let team = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|t| *t != "not set")
        .map(str::to_string);
    Some(team)
}

/// Refuse a download that is signed worse than what it would replace.
///
/// This is the check that makes the swap safe: a signed copy only ever
/// takes an update signed by the same team, so a hijacked download
/// cannot install itself over a release build.
#[cfg(target_os = "macos")]
fn verify_signature(app: &Path, new_app: &Path) -> anyhow::Result<()> {
    let theirs = signing_team(new_app);
    if theirs.is_some() {
        let out = std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(new_app)
            .output()
            .context("can't run codesign")?;
        anyhow::ensure!(
            out.status.success(),
            "the update's signature is invalid: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let Some(ours) = signing_team(app) else {
        // An unsigned build — a local one, or a fork's. It has no
        // identity to hold the download to.
        return Ok(());
    };
    let Some(theirs) = theirs else {
        anyhow::bail!("the update isn't signed, and this copy is");
    };
    anyhow::ensure!(
        ours == theirs,
        "the update is signed by a different developer"
    );
    Ok(())
}

/// Quits the running agent app, so that the relauncher's `open` starts the
/// new bundle rather than bringing the old process forward. For updates
/// made from the command line; the app quits itself when it installs one.
#[cfg(target_os = "macos")]
pub fn quit_running_app() {
    let _ = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", "quit app id \"com.infrawrench.opencomputeruse\""])
        .output();
    for _ in 0..50 {
        let running = std::process::Command::new("/usr/bin/pgrep")
            .args(["-f", "OpenComputerUse.app/Contents/MacOS/opencomputeruse agent"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !running {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Start the new bundle once this process is gone.
#[cfg(target_os = "macos")]
fn relaunch(app: &Path) -> anyhow::Result<()> {
    let script = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open {app}",
        // Opened normally, so the window comes back showing the new version.
        pid = std::process::id(),
        app = sh_quote(&app.to_string_lossy()),
    );
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .spawn()
        .context("can't start the relauncher")?;
    Ok(())
}

/// `s` as one single-quoted `sh` word.
#[cfg(target_os = "macos")]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ------------------------------------------------------ everywhere else

/// [`self_installable`] is false off macOS, so this is unreachable; it
/// exists so the rest compiles.
#[cfg(not(target_os = "macos"))]
pub fn install_and_restart(_file: &Path) -> anyhow::Result<()> {
    anyhow::bail!("OpenComputerUse can't update itself on this platform")
}

// ---------------------------------------------------- automatic checking

/// How long an automatic check waits after the last one.
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether an automatic check is due: once a day.
pub fn check_due() -> bool {
    let dir = crate::config::config_dir();
    match std::fs::read_to_string(dir.join("last-update-check"))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
    {
        Some(then) => now_secs().saturating_sub(then) >= CHECK_INTERVAL_SECS,
        None => true,
    }
}

/// Record that a check just happened.
pub fn mark_checked() {
    let dir = crate::config::config_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("last-update-check"), now_secs().to_string());
    }
}

/// Whether to check automatically. On unless turned off in the settings.
pub fn check_automatically() -> bool {
    !crate::config::config_dir().join("no-update-check").exists()
}

pub fn set_check_automatically(on: bool) {
    let dir = crate::config::config_dir();
    let flag = dir.join("no-update-check");
    if on {
        let _ = std::fs::remove_file(flag);
    } else if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(flag, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("1.9.0", "1.10.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        // Pre-release suffixes compare on the numeric part.
        assert!(is_newer("0.1.0", "0.1.1-beta"));
    }

    #[test]
    fn malformed_versions_never_claim_an_update() {
        assert!(!is_newer("0.1.0", "not-a-version"));
        assert!(!is_newer("", "1.0.0"));
        assert!(!is_newer("0.1.0", ""));
    }

    /// One release with everything the workflow attaches to it.
    fn release() -> Value {
        serde_json::from_str(
            r#"{
            "tag_name": "v0.7.0",
            "html_url": "https://example.com/r",
            "assets": [
                {"name": "opencomputeruse-linux-x86_64", "browser_download_url": "https://e/l", "size": 1},
                {"name": "opencomputeruse-windows-x86_64.exe", "browser_download_url": "https://e/w", "size": 2},
                {"name": "OpenComputerUse.dmg", "browser_download_url": "https://e/d", "size": 3},
                {"name": "OpenComputerUse.zip", "browser_download_url": "https://e/a", "size": 4,
                 "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000001"}
            ]
        }"#,
        )
        .expect("the release JSON parses")
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_app_zip_is_the_macos_asset() {
        let installer = installer_for(&release()).expect("macOS has an installable asset");
        assert_eq!(installer.file_name, "OpenComputerUse.zip");
        assert_eq!(installer.size, 4);
        assert_eq!(
            installer.sha256.as_deref(),
            Some("0000000000000000000000000000000000000000000000000000000000000001")
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn other_platforms_install_nothing_themselves() {
        assert_eq!(installer_for(&release()), None);
        assert!(!self_installable());
    }

    #[test]
    fn digests_are_taken_only_when_they_are_sha256() {
        let sha = "a".repeat(64);
        assert_eq!(sha256_from_digest(&format!("sha256:{sha}")), Some(sha));
        // Upper case is the same digest.
        assert_eq!(
            sha256_from_digest(&format!("sha256:{}", "AB".repeat(32))),
            Some("ab".repeat(32))
        );
        assert_eq!(sha256_from_digest("sha512:beef"), None);
        assert_eq!(sha256_from_digest("sha256:beef"), None);
        assert_eq!(sha256_from_digest(""), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn paths_survive_the_shell() {
        assert_eq!(
            sh_quote("/Applications/OpenComputerUse.app"),
            "'/Applications/OpenComputerUse.app'"
        );
        // A quote in the path must not end the word.
        assert_eq!(sh_quote("/tmp/it's here"), r"'/tmp/it'\''s here'");
    }
}
