//! opencomputeruse: background computer use for agents, as an MCP server.
//!
//! `opencomputeruse mcp` is what MCP clients run. On Linux and Windows it
//! drives apps itself; on macOS it forwards to the OpenComputerUse agent
//! app (`opencomputeruse agent`), which holds the Accessibility and Screen
//! Recording permissions and draws the overlays.

mod clients;
mod config;
mod mcp;
mod recipe;
mod remote;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod skills;
mod tools;
mod update;
mod vision;

#[cfg(target_os = "macos")]
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
    opencomputeruse serve [--port N]      Run the HTTP server for other devices (the app does this on macOS)
    opencomputeruse serve --install       Run it at login from now on (--uninstall stops that; Linux, Windows)
    opencomputeruse update [--check]      Check for a new release, and on macOS install it
    opencomputeruse agent [--background]  Run the macOS agent app (opening the app does this)
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
        #[cfg(target_os = "macos")]
        Some("agent") => agent::run(!args.iter().any(|a| a == "--background")),
        // Opened from Finder: no arguments, or a legacy process serial number.
        #[cfg(target_os = "macos")]
        None => agent::run(true),
        #[cfg(target_os = "macos")]
        Some(a) if a.starts_with("-psn_") => agent::run(true),
        #[cfg(not(target_os = "macos"))]
        None => run_mcp(),
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
    let config = config::Config::load();
    let port = match args.iter().position(|a| a == "--port") {
        Some(i) => args
            .get(i + 1)
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("--port takes a number"))?,
        None => config.http.port,
    };
    #[cfg(target_os = "macos")]
    let platform = std::sync::Arc::new(ocu_macos::MacPlatform);
    #[cfg(target_os = "linux")]
    let platform = std::sync::Arc::new(ocu_linux::LinuxPlatform::new());
    #[cfg(windows)]
    let platform = std::sync::Arc::new(ocu_windows::WindowsPlatform::new());
    let service = ocu_core::Service::new(platform, None);
    let server = remote::server::HttpServer::start(service, &config.http.bind, port)?;
    eprintln!(
        "Serving on {} for the devices in {}.",
        server.addr,
        remote::devices::path().display()
    );
    server.wait();
    Ok(())
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
    use std::sync::Arc;
    init_stderr_log();
    #[cfg(target_os = "linux")]
    let platform = Arc::new(ocu_linux::LinuxPlatform::new());
    #[cfg(windows)]
    let platform = Arc::new(ocu_windows::WindowsPlatform::new());
    let service = ocu_core::Service::new(platform, None);
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
