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
mod tools;
mod vision;

#[cfg(target_os = "macos")]
mod agent;
#[cfg(target_os = "macos")]
mod ipc;

use anyhow::{bail, Result};

const USAGE: &str = "\
opencomputeruse — background computer use for agents, over MCP

USAGE:
    opencomputeruse mcp                   Run the MCP server on stdio (what clients launch)
    opencomputeruse install <client>      Register the MCP server with claude or codex
    opencomputeruse uninstall <client>    Remove it again
    opencomputeruse agent [--background]  Run the macOS agent app (opening the app does this)

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
            let Some(client) = args.get(1) else { bail!("name a client: claude or codex") };
            let client = clients::Client::parse(client)?;
            if first == Some("install") {
                clients::install(client)?;
                println!("Installed as \"{}\" in {}.", clients::SERVER_NAME, client.label());
            } else {
                clients::uninstall(client)?;
                println!("Removed from {}.", client.label());
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
        if let Some(bundle) = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")) {
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
                bail!("the OpenComputerUse app did not start; open it once to finish setting it up");
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
                match result.as_ref().err().and_then(|e| e.downcast_ref::<Disconnected>()) {
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
