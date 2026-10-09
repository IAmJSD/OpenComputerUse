//! opencomputeruse: background computer use for agents, as an MCP server.
//!
//! `opencomputeruse mcp` is what MCP clients run. On Linux and Windows it
//! drives apps itself; on macOS it forwards to the OpenComputerUse agent
//! app (`opencomputeruse agent`), which holds the Accessibility and Screen
//! Recording permissions and draws the overlays. Run with no arguments, it
//! opens the app's window on every platform.

mod clients;
mod config;
mod mcp;
mod recipe;
mod remote;
mod tools;
mod update;
mod vision;

mod agent;
#[cfg(target_os = "macos")]
mod ipc;

use anyhow::{bail, Result};

const USAGE: &str = "\
opencomputeruse: background computer use for agents, over MCP

USAGE:
    opencomputeruse mcp                   Run the MCP server on stdio (what clients launch)
    opencomputeruse install <client>      Register the MCP server with claude, claude-desktop, codex, opencode or kimi
    opencomputeruse uninstall <client>    Remove it again
    opencomputeruse clients               Show which clients run this copy
    opencomputeruse serve [--port N] [--listen-on LIST]
                                          Run the HTTP server for other devices (the app does this on macOS).
                                          LIST limits it to some network adapters, addresses or ranges,
                                          like en0,tailscale,192.168.1.0/24
    opencomputeruse serve --install       Run it at login from now on (--uninstall stops that; Linux, Windows)
    opencomputeruse update [--check]      Check for a new release, and on macOS install it
    opencomputeruse skill                 Print the skill other devices use to drive a host
    opencomputeruse [agent]               Open the app's window (on macOS, --background starts it hidden)
    sudo opencomputeruse install-lock     Enable working while the Mac is locked (macOS; one-time)
    sudo opencomputeruse uninstall-lock   Undo install-lock, restoring the normal unlock

Settings live in the config file printed by `opencomputeruse config-path`.";

fn main() {
    if let Err(e) = run() {
        eprintln!("opencomputeruse: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(String::as_str);
    match first {
        Some("mcp") => run_mcp(),
        Some("install") | Some("uninstall") => {
            let Some(client) = args.get(1) else {
                bail!("name a client: claude, claude-desktop, codex, opencode or kimi")
            };
            let client = clients::Client::parse(client)?;
            if first == Some("install") {
                clients::install(client)?;
                println!(
                    "Installed as \"{}\" in {}. {}",
                    clients::SERVER_NAME,
                    client.label(),
                    client.next_step()
                );
            } else {
                clients::uninstall(client)?;
                println!("Removed from {}.", client.label());
            }
            Ok(())
        }
        Some("serve") => run_serve(&args[1..]),
        #[cfg(target_os = "macos")]
        Some("install-lock") => run_install_lock(&args[1..]),
        #[cfg(target_os = "macos")]
        Some("uninstall-lock") => {
            ocu_macos::lock::uninstall()?;
            println!("Lock-screen support removed. Unlocking needs your password again.");
            Ok(())
        }
        Some("update") => run_update(args.iter().any(|a| a == "--check")),
        Some("clients") => {
            for client in clients::Client::ALL {
                let found = clients::find(client).is_some();
                let state = match (found, clients::registration(client)) {
                    (false, _) => "not found".to_string(),
                    (true, clients::Registration::Current) => "installed".to_string(),
                    (true, clients::Registration::Elsewhere(path)) => {
                        format!("points elsewhere ({path}); `install` fixes it")
                    }
                    (true, clients::Registration::Absent) => "not installed".to_string(),
                };
                println!("{:<15} {state}", client.label());
            }
            Ok(())
        }
        // The skill other devices get, as android/res/raw/skill.md holds it.
        Some("skill") => {
            print!("{}", remote::skill::render_generic(&tools::catalog()));
            Ok(())
        }
        Some("config-path") => {
            println!("{}", config::Config::path().display());
            Ok(())
        }
        Some("-h") | Some("--help") | Some("help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V") | Some("--version") => {
            println!("opencomputeruse {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("agent") => agent::run(!args.iter().any(|a| a == "--background")),
        // Opened from Finder or Explorer: no arguments, or on macOS a legacy
        // process serial number.
        None => agent::run(true),
        #[cfg(target_os = "macos")]
        Some(a) if a.starts_with("-psn_") => agent::run(true),
        Some(other) => bail!("unknown command \"{other}\"\n\n{USAGE}"),
    }
}

/// Enables working while the Mac is locked: installs the authorization
/// plugin, socket directory and unlock rule. Must run as root.
#[cfg(target_os = "macos")]
fn run_install_lock(args: &[String]) -> Result<()> {
    use std::path::{Path, PathBuf};
    // The user to own the socket directory: `--uid N` or `OCU_OWNER_UID` (the
    // app's admin prompt, where root has no SUDO_UID), else sudo's SUDO_UID.
    let uid_arg = args
        .iter()
        .position(|a| a == "--uid")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u32>().ok());
    let owner_uid: u32 = uid_arg
        .or_else(|| {
            std::env::var("OCU_OWNER_UID")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .or_else(|| std::env::var("SUDO_UID").ok().and_then(|v| v.parse().ok()))
        .unwrap_or_else(|| unsafe { libc::getuid() });
    if owner_uid == 0 {
        bail!("run this with sudo from your own account, not as root, so you own the socket");
    }
    // Find the signed plugin bundle: an override, then next to the app or exe,
    // then the build output.
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(Path::new("."));
    let bundle = "OcuLockAuthorizationPlugin.bundle";
    let candidates = [
        std::env::var_os("OCU_LOCK_PLUGIN").map(PathBuf::from),
        Some(dir.join(format!("../Resources/{bundle}"))), // inside the .app
        Some(dir.join(bundle)),
        Some(PathBuf::from("dist").join(bundle)),
    ];
    let plugin = candidates
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
        .ok_or_else(|| anyhow::anyhow!(
            "cannot find {bundle}; build it with packaging/macos/lockplugin/build.sh or set OCU_LOCK_PLUGIN"
        ))?;
    ocu_macos::lock::install(&plugin, owner_uid)?;
    println!(
        "Lock-screen support installed. Turn it on in the app's settings to let \
         `unlock_screen` work. Undo with `sudo opencomputeruse uninstall-lock`."
    );
    Ok(())
}

/// The HTTP server without the app: Linux and Windows, or a headless Mac.
fn run_serve(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--install" || a == "--uninstall") {
        #[cfg(target_os = "macos")]
        bail!("on macOS the app serves HTTP and starts at login itself: turn the server on in its settings");
        #[cfg(not(target_os = "macos"))]
        {
            let on = args.iter().any(|a| a == "--install");
            remote::autostart::set(on)?;
            println!(
                "{}",
                if on {
                    "The HTTP server now starts at login."
                } else {
                    "The HTTP server no longer starts at login."
                }
            );
            return Ok(());
        }
    }
    init_stderr_log();
    let flags = ServeFlags::parse(args)?;
    let service = ocu_core::Service::with_devices(platform(), Some(mobile_platform()), None);
    let mut server = remote::server::HttpServer::new(service);
    let mut shown: Option<(Vec<_>, Vec<String>)> = None;
    // Follow the settings and the network: adapters gain and lose addresses.
    loop {
        let http = flags.apply(config::Config::load().http);
        let (addrs, mut errors) = match remote::interfaces::listen_addrs(&http) {
            Ok(addrs) => (addrs, Vec::new()),
            Err(e) => (Vec::new(), vec![format!("{e:#}")]),
        };
        errors.extend(server.listen_on(&addrs));
        let listening = server.addrs();
        // Nothing to wait for without listen_on: a bad bind or a port in use
        // fails now, as it always has.
        if shown.is_none() && listening.is_empty() && http.listen_on.is_empty() {
            bail!("{}", errors.join("; "));
        }
        if shown.as_ref() != Some(&(listening.clone(), errors.clone())) {
            for e in &errors {
                eprintln!("{e}");
            }
            if listening.is_empty() && errors.is_empty() {
                eprintln!(
                    "Waiting: nothing in {} has an address now.",
                    http.listen_on.join(", ")
                );
            } else if !listening.is_empty() {
                let at: Vec<String> = listening.iter().map(ToString::to_string).collect();
                eprintln!(
                    "Serving on {} for the devices in {}.",
                    at.join(", "),
                    remote::devices::path().display()
                );
            }
            shown = Some((listening, errors));
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// `serve`'s `--port` and `--listen-on`, which win over the settings file.
#[derive(Debug, Default, PartialEq)]
struct ServeFlags {
    port: Option<u16>,
    listen_on: Option<Vec<String>>,
}

impl ServeFlags {
    fn parse(args: &[String]) -> Result<Self> {
        let mut flags = Self::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--port" => {
                    flags.port = Some(
                        it.next()
                            .and_then(|p| p.parse().ok())
                            .ok_or_else(|| anyhow::anyhow!("--port takes a number"))?,
                    )
                }
                "--listen-on" => {
                    let list = it.next().filter(|l| !l.starts_with("--")).ok_or_else(|| {
                        anyhow::anyhow!(
                            "--listen-on takes adapters, addresses or ranges, like en0,tailscale"
                        )
                    })?;
                    let entries: Vec<String> = list.split(',').map(str::to_string).collect();
                    flags
                        .listen_on
                        .get_or_insert_with(Vec::new)
                        .extend(remote::interfaces::clean(&entries)?);
                }
                _ => {}
            }
        }
        Ok(flags)
    }

    fn apply(&self, mut http: config::HttpConfig) -> config::HttpConfig {
        if let Some(port) = self.port {
            http.port = port;
        }
        if let Some(l) = &self.listen_on {
            http.listen_on = l.clone();
        }
        http
    }
}

/// The command-line update: report, and on macOS install over this bundle.
fn run_update(check_only: bool) -> Result<()> {
    use std::sync::atomic::AtomicU64;
    match update::check() {
        update::UpdateStatus::UpToDate => println!(
            "opencomputeruse {} is the latest version.",
            update::current_version()
        ),
        update::UpdateStatus::Failed(e) => bail!("couldn't check for updates: {e}"),
        update::UpdateStatus::Available(up) => {
            println!(
                "Version {} is available (this is {}): {}",
                up.version,
                update::current_version(),
                up.page
            );
            if check_only {
                return Ok(());
            }
            let Some(installer) = up.install else {
                println!("Download it from the release page.");
                return Ok(());
            };
            println!(
                "Downloading {} ({} bytes)…",
                installer.file_name, installer.size
            );
            let file = update::download(&installer, &AtomicU64::new(0))?;
            #[cfg(target_os = "macos")]
            update::quit_running_app();
            update::install_and_restart(&file)?;
            println!("Installed {}; OpenComputerUse is reopening.", up.version);
        }
    }
    update::mark_checked();
    Ok(())
}

/// This computer's apps, through its own backend.
pub fn platform() -> std::sync::Arc<dyn ocu_core::Platform> {
    #[cfg(target_os = "macos")]
    return std::sync::Arc::new(ocu_macos::MacPlatform);
    #[cfg(target_os = "linux")]
    return std::sync::Arc::new(ocu_linux::LinuxPlatform::with_portal(
        config::Config::linux_file_portal,
    ));
    #[cfg(windows)]
    return std::sync::Arc::new(ocu_windows::WindowsPlatform::with_hook(
        config::Config::windows_panel_hook,
    ));
}

/// Phones, tablets, simulators and emulators, beside this computer's apps.
pub fn mobile_platform() -> std::sync::Arc<ocu_mobile::MobilePlatform> {
    // A development build uses the Android helper built in this checkout
    // (android/build.sh); a release fetches its own version's.
    let android_apk = cfg!(debug_assertions)
        .then(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/OpenComputerUse.apk"));
    std::sync::Arc::new(ocu_mobile::MobilePlatform::new(ocu_mobile::MobileConfig {
        cache: config::config_dir().join("mobile"),
        version: env!("CARGO_PKG_VERSION").to_string(),
        android_apk,
    }))
}

fn init_stderr_log() {
    // Stdout belongs to the protocol.
    let _ = env_logger::Builder::from_env(env_logger::Env::new().filter_or("OCU_LOG", "warn"))
        .target(env_logger::Target::Stderr)
        .try_init();
}

#[cfg(target_os = "macos")]
fn run_mcp() -> Result<()> {
    init_stderr_log();
    mcp::serve(Box::new(macos::AgentLink::default()))
}

#[cfg(not(target_os = "macos"))]
fn run_mcp() -> Result<()> {
    init_stderr_log();
    let service = ocu_core::Service::with_devices(platform(), Some(mobile_platform()), None);
    let client = service.client();
    let result = mcp::serve(Box::new(client));
    // The client is gone with `serve`'s box, ending its sessions; make sure.
    service.end_all();
    result
}

#[cfg(target_os = "macos")]
mod macos {
    //! The MCP server's link to the agent app, made on first use and
    //! started if the app is not running.

    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context as _, Result};

    use ocu_core::{Handler, Request, Response};

    use crate::agent::socket_path;
    use crate::ipc::{Disconnected, Remote};

    #[derive(Default)]
    pub struct AgentLink {
        remote: Option<Remote>,
    }

    /// Starts the agent through LaunchServices, so macOS treats it as its
    /// own app (with its own permissions) rather than part of whichever
    /// client started this server.
    fn launch_agent() -> Result<()> {
        let exe = std::env::current_exe()?;
        let exe = exe.canonicalize().unwrap_or(exe);
        if let Some(bundle) = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
        {
            let status = Command::new("/usr/bin/open")
                .arg("-g")
                .arg("-a")
                .arg(bundle)
                .args(["--args", "agent", "--background"])
                .status()
                .context("running open")?;
            if !status.success() {
                bail!("could not open {}", bundle.display());
            }
        } else {
            // A development build outside a bundle: permissions then come
            // from the terminal it runs under.
            use std::os::unix::process::CommandExt as _;
            Command::new(&exe)
                .args(["agent", "--background"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .context("starting the agent")?;
        }
        Ok(())
    }

    fn connect() -> Result<Remote> {
        let path = socket_path();
        if let Ok(r) = Remote::connect(&path) {
            return Ok(r);
        }
        log::info!("starting the OpenComputerUse agent");
        launch_agent()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            std::thread::sleep(Duration::from_millis(150));
            if let Ok(r) = Remote::connect(&path) {
                return Ok(r);
            }
            if Instant::now() > deadline {
                bail!(
                    "the OpenComputerUse app did not start; open it once to finish setting it up"
                );
            }
        }
    }

    impl Handler for AgentLink {
        fn handle(&mut self, req: Request) -> Result<Response> {
            // Once for an agent that was already gone (the app was updated or
            // restarted since the last request), once more on a fresh one.
            for attempt in 0..2 {
                if self.remote.is_none() {
                    self.remote = Some(connect()?);
                }
                let result = self.remote.as_mut().unwrap().handle(req.clone());
                match result
                    .as_ref()
                    .err()
                    .and_then(|e| e.downcast_ref::<Disconnected>())
                {
                    None => return result,
                    Some(d) => {
                        self.remote = None;
                        if !d.before_sending || attempt == 1 {
                            return result;
                        }
                        log::info!("the agent went away; reconnecting");
                    }
                }
            }
            unreachable!()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn serve_flags_override_the_settings() {
        let flags = ServeFlags::parse(&args(&[
            "serve",
            "--listen-on",
            "en0, tailscale",
            "--port",
            "9000",
            "--listen-on",
            "10.0.0.0/8",
        ]))
        .unwrap();
        let http = flags.apply(config::HttpConfig::default());
        assert_eq!(http.port, 9000);
        assert_eq!(http.listen_on, ["en0", "tailscale", "10.0.0.0/8"]);
        let none = ServeFlags::parse(&args(&["serve"])).unwrap();
        let saved = config::HttpConfig {
            listen_on: vec!["en1".into()],
            ..Default::default()
        };
        assert_eq!(none.apply(saved).listen_on, ["en1"]);
    }

    #[test]
    fn serve_flags_need_values() {
        assert!(ServeFlags::parse(&args(&["serve", "--listen-on"])).is_err());
        assert!(ServeFlags::parse(&args(&["serve", "--listen-on", "--port", "9000"])).is_err());
        assert!(ServeFlags::parse(&args(&["serve", "--listen-on", "10.0.0.0/40"])).is_err());
        assert!(ServeFlags::parse(&args(&["serve", "--port", "x"])).is_err());
    }
}
