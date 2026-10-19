//! opencomputeruse: background computer use for agents, as an MCP server.
//!
//! `opencomputeruse mcp` is what MCP clients run. On Linux and Windows it
//! drives apps itself; on macOS it forwards to the OpenComputerUse agent
//! app (`opencomputeruse agent`), which holds the Accessibility and Screen
//! Recording permissions and draws the overlays. Run with no arguments, it
//! opens the app's window on every platform.

mod clients;
mod config;
mod extract;
mod mcp;
mod recipe;
mod remote;
mod tools;
mod update;
mod view;
mod vision;

mod agent;
#[cfg(target_os = "macos")]
mod ipc;

use anyhow::{bail, Result};

const USAGE: &str = "\
opencomputeruse: background computer use for agents, over MCP

USAGE:
    opencomputeruse mcp [VIEW]            Run the MCP server on stdio (what clients launch)
    opencomputeruse install <client>      Register the MCP server with claude, claude-desktop, codex, opencode or kimi
    opencomputeruse uninstall <client>    Remove it again
    opencomputeruse clients               Show which clients run this copy
    opencomputeruse serve [--port N] [--listen-on LIST] [--token TOKEN] [VIEW]
                                          Run the HTTP server for other devices (the app does this on macOS).
                                          LIST limits it to some network adapters, addresses or ranges,
                                          like en0,tailscale,192.168.1.0/24. TOKEN (or OCU_SERVE_TOKEN) is
                                          a key accepted beside the devices', for setting up without the app
    opencomputeruse serve --install       Run it at login from now on (--uninstall stops that; Linux, Windows)
    opencomputeruse serve --install [--port N] [--listen-on LIST]
                                          Same, saving those settings first (Linux, Windows)
    opencomputeruse device add --name N --url U [--replace]
                                          Allow a device to drive this computer: prints its new key.
                                          A taken name is refused; --replace gives that device a new key
    opencomputeruse device list           The devices allowed to drive this computer
    opencomputeruse device remove NAME    Remove a device by ID or name and its key
    opencomputeruse device remove --name N
                                          Remove only the device named N
    opencomputeruse ssh-setup HOST --name NAME [-i KEYFILE] [--replace]
                                          HOST is an SSH alias or user@host
                                          Install this version on a Linux host over SSH, serve it there
                                          on loopback only, and keep an SSH tunnel to it up from here.
                                          --replace gives a remote device of that name a new key
    opencomputeruse ssh-setup HOST --name NAME --use-installed [--remote-port P]
                                          Instead connect to an opencomputeruse already running there
                                          (Linux or macOS; its HTTP server on port P, default 8642)
    opencomputeruse extract FIELDS (--app APP [--wait MS] | --active-window | --state TEXT | --state-file F)
                            [--image PNG]... [--context TEXT] [--reference-date DATE] [--no-screenshot | --no-tree]
                                          Read typed values with Cloudflare's Clairic extraction model.
                                          FIELDS is --fields JSON (or @file) and/or --field NAME=TYPE[:INSTRUCTIONS],
                                          repeated; TYPE is text, number, date, time, datetime, duration or
                                          list (of text). --app starts the app, waits MS (default 3000), and reads
                                          its window's accessibility tree with a screenshot, naming the element
                                          of each text answer (--no-tree reads the screenshot alone; on macOS,
                                          OCU_IN_PROCESS=1 drives the app here rather than through the app's agent);
                                          --state reads text instead (--state-file - for stdin)
    opencomputeruse update [--check]      Check for a new release, and on macOS install it
    opencomputeruse skill                 Print the skill other devices use to drive a host
    opencomputeruse [agent]               Open the app's window (on macOS, --background starts it hidden)
    sudo opencomputeruse install-lock     Enable working while the Mac is locked (macOS; one-time)
    sudo opencomputeruse uninstall-lock   Undo install-lock, restoring the normal unlock

VIEW is the live view of sessions (Linux): --view-listen ADDR:PORT and --view-token TOKEN (or
OCU_VIEW_LISTEN and OCU_VIEW_TOKEN; serve falls back to its --token). With it, or a serve token,
sessions are shared by every client until they are ended.

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
        Some("mcp") => run_mcp(&args[1..]),
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
        Some("device") => run_device(&args[1..]),
        Some("ssh-setup") => remote::ssh::setup(&args[1..]),
        Some("extract") => run_extract(&args[1..]),
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
            if on {
                let flags = ServeFlags::parse(args)?;
                remote::update_server_config(None, flags.port, flags.listen_on.as_deref())?;
            }
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
    let view = view::ViewFlags::parse(args)?.resolve(flags.token.as_deref())?;
    let service = ocu_core::Service::with_devices(platform(), Some(mobile_platform()), None);
    if flags.token.is_some() {
        // One server for every harness: their sessions are each other's.
        service.share_sessions();
    }
    if let Some((addr, token)) = view {
        view::start(service.clone(), addr, token)?;
    }
    let mut server = remote::server::HttpServer::new(service, flags.token.clone());
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

/// `device add --name N --url U` prints only the new key on stdout, so a
/// caller can capture it; what happened goes to stderr, as an `ocu-device:`
/// line `ssh-setup` reads.
fn run_device(args: &[String]) -> Result<()> {
    use remote::devices::Devices;
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .filter(|v| !v.starts_with("--"))
    };
    match args.first().map(String::as_str) {
        Some("add") => {
            let name = value("--name").ok_or_else(|| anyhow::anyhow!("--name takes a name"))?;
            let url = value("--url").ok_or_else(|| anyhow::anyhow!("--url takes a URL"))?;
            let mut devices = Devices::load();
            let existing = devices
                .devices
                .iter()
                .find(|d| d.name.eq_ignore_ascii_case(name.trim()))
                .map(|d| d.id.clone());
            let replace = args.iter().any(|a| a == "--replace");
            let (key, what) = match existing {
                Some(id) if replace => (devices.regenerate(&id, Some(url))?.1, "regenerated"),
                Some(_) => {
                    eprintln!("ocu-device: exists");
                    bail!("a device named \"{name}\" already exists; --replace gives it a new key, and its current key stops working")
                }
                None => (devices.add(name, url)?.1, "added"),
            };
            eprintln!("ocu-device: {what}");
            println!("{key}");
            Ok(())
        }
        // Also what `ssh-setup` runs to tell a release with this command
        // from an older one.
        Some("list") => {
            for d in Devices::load().devices {
                println!("{}\t{}\t{}", d.name, d.url, d.id);
            }
            Ok(())
        }
        Some("remove") => {
            let Some(name) = args.get(1) else {
                bail!("name the device to remove")
            };
            let d = if name == "--name" {
                let name = value("--name").ok_or_else(|| anyhow::anyhow!("--name takes a name"))?;
                Devices::load().remove_named(name)?
            } else {
                Devices::load().remove(name)?
            };
            println!("Removed {} ({}); its key no longer works.", d.name, d.id);
            Ok(())
        }
        _ => bail!(
            "usage: opencomputeruse device add --name N --url U [--replace] | device list | device remove NAME"
        ),
    }
}

/// `serve`'s `--port`, `--listen-on` and `--token`, which win over the
/// settings file.
#[derive(Debug, Default, PartialEq)]
struct ServeFlags {
    port: Option<u16>,
    listen_on: Option<Vec<String>>,
    token: Option<String>,
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
                "--token" => {
                    let token = it
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--token takes a value"))?;
                    flags.token = Some(token.clone());
                }
                _ => {}
            }
        }
        if flags.token.is_none() {
            flags.token = std::env::var("OCU_SERVE_TOKEN")
                .ok()
                .filter(|t| !t.is_empty());
        }
        if let Some(t) = &flags.token {
            view::check_token(t)?;
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

/// `extract`'s arguments: the fields, and what to read them from.
#[derive(Debug, Default)]
struct ExtractFlags {
    fields: serde_json::Map<String, serde_json::Value>,
    app: Option<String>,
    active_window: bool,
    wait_ms: u64,
    state: Option<String>,
    images: Vec<std::path::PathBuf>,
    context: Option<String>,
    reference_date: Option<String>,
    screenshot: bool,
    tree: bool,
}

impl ExtractFlags {
    fn parse(args: &[String]) -> Result<Self> {
        use serde_json::{json, Value};
        let mut flags = Self {
            wait_ms: 3000,
            screenshot: true,
            tree: true,
            ..Default::default()
        };
        let mut it = args.iter();
        let value = |it: &mut std::slice::Iter<String>, flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{flag} takes a value"))
        };
        let read = |path: &str| -> Result<String> {
            use std::io::Read as _;
            let mut s = String::new();
            if path == "-" {
                std::io::stdin().read_to_string(&mut s)?;
            } else {
                s = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
            }
            Ok(s)
        };
        while let Some(a) = it.next() {
            match a.as_str() {
                "--fields" => {
                    let v = value(&mut it, a)?;
                    let text = match v.strip_prefix('@') {
                        Some(path) => read(path)?,
                        None => v,
                    };
                    let parsed: Value = serde_json::from_str(&text)
                        .map_err(|e| anyhow::anyhow!("--fields is a JSON object: {e}"))?;
                    let Value::Object(m) = parsed else {
                        bail!("--fields is a JSON object of field name to spec")
                    };
                    flags.fields.extend(m);
                }
                "--field" => {
                    let v = value(&mut it, a)?;
                    let (name, rest) = v
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("--field is NAME=TYPE[:INSTRUCTIONS]"))?;
                    let (kind, instructions) = match rest.split_once(':') {
                        Some((k, i)) => (k, Some(i)),
                        None => (rest, None),
                    };
                    let mut spec = json!({ "type": kind.trim() });
                    if kind.trim() == "list" {
                        spec["items"] = json!("text");
                    }
                    if let Some(i) = instructions.filter(|i| !i.trim().is_empty()) {
                        spec["instructions"] = json!(i.trim());
                    }
                    flags.fields.insert(name.trim().to_string(), spec);
                }
                "--app" => flags.app = Some(value(&mut it, a)?),
                "--active-window" => flags.active_window = true,
                "--wait" => {
                    flags.wait_ms = value(&mut it, a)?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--wait takes milliseconds"))?
                }
                "--state" => flags.state = Some(value(&mut it, a)?),
                "--state-file" => flags.state = Some(read(&value(&mut it, a)?)?),
                "--image" => flags.images.push(value(&mut it, a)?.into()),
                "--context" => flags.context = Some(value(&mut it, a)?),
                "--reference-date" => flags.reference_date = Some(value(&mut it, a)?),
                "--no-screenshot" => flags.screenshot = false,
                "--no-tree" => flags.tree = false,
                other => bail!("unknown option \"{other}\" for extract"),
            }
        }
        // Checks names and types, before anything is started.
        flags.fields =
            extract::fields_arg(Some(&Value::Object(std::mem::take(&mut flags.fields))))?;
        let window = flags.app.is_some() || flags.active_window;
        if window && flags.state.is_some() {
            bail!("give --app or --active-window, or --state, not both");
        }
        if window && !flags.images.is_empty() {
            bail!("--image is for --state; a window's screenshot is sent by itself");
        }
        if !flags.tree && !flags.screenshot {
            bail!("--no-tree and --no-screenshot leave nothing to read");
        }
        if !window && flags.state.is_none() && flags.images.is_empty() {
            bail!("say what to read: --app APP, --active-window, --state TEXT, --state-file FILE or --image FILE");
        }
        Ok(flags)
    }
}

/// `extract`: Clairic on an app's window, or on text and images given here.
fn run_extract(args: &[String]) -> Result<()> {
    use serde_json::{json, Value};
    let flags = ExtractFlags::parse(args)?;
    let config = config::Config::load();
    // Before starting an app only to find nothing to call.
    extract::require_credentials(&config.recipe)?;
    if flags.app.is_none() && !flags.active_window {
        let mut state = flags.state.clone().unwrap_or_default();
        if let Some(c) = &flags.context {
            state = format!("{state}\n{c}");
        }
        let images = flags
            .images
            .iter()
            .map(|p| std::fs::read(p).map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display())))
            .collect::<Result<Vec<_>>>()?;
        let input = extract::Input {
            state,
            images,
            fields: flags.fields,
            reference_date: flags.reference_date,
        };
        // The state is the caller's own, so its spans mean something to them.
        let reply = extract::call(&config.recipe, &config.extract, &input)?;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }
    init_stderr_log();
    let mut handler = cli_handler();
    let mut start = json!({ "active_window": flags.active_window });
    if let Some(app) = &flags.app {
        start["app"] = json!(app);
    }
    let started = tools::call(&mut *handler, "start_session", &start)?;
    let session = serde_json::from_str::<Value>(&started.text)?["session_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("start_session returned no session id"))?
        .to_string();
    if flags.app.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(flags.wait_ms));
    }
    let mut call = json!({
        "session_id": session,
        "fields": flags.fields,
        "screenshot": flags.screenshot,
        "ui_tree": flags.tree,
    });
    if let Some(c) = &flags.context {
        call["context"] = json!(c);
    }
    if let Some(d) = &flags.reference_date {
        call["reference_date"] = json!(d);
    }
    let result = tools::call(&mut *handler, "extract", &call);
    let _ = tools::call(
        &mut *handler,
        "end_session",
        &json!({ "session_id": session }),
    );
    println!("{}", result?.text);
    Ok(())
}

/// Sessions for a command run here: through the app on macOS, in this
/// process elsewhere (they end with it).
/// `OCU_IN_PROCESS=1` drives apps in this process on macOS too, as `serve`
/// does on a headless Mac, with this process's own permissions (CI).
fn cli_handler() -> Box<dyn ocu_core::Handler> {
    #[cfg(target_os = "macos")]
    if std::env::var("OCU_IN_PROCESS").as_deref() != Ok("1") {
        return Box::new(macos::AgentLink::default());
    }
    let service = ocu_core::Service::with_devices(platform(), None, None);
    Box::new(service.client())
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
    return std::sync::Arc::new(ocu_linux::LinuxPlatform::with_settings(
        config::Config::linux_file_portal,
        config::Config::linux_display,
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
fn run_mcp(args: &[String]) -> Result<()> {
    init_stderr_log();
    if view::ViewFlags::parse(args)?.listen.is_some() {
        bail!("on macOS the app owns these sessions; run `serve` with --view-listen to watch sessions live");
    }
    mcp::serve(Box::new(macos::AgentLink::default()))
}

#[cfg(not(target_os = "macos"))]
fn run_mcp(args: &[String]) -> Result<()> {
    init_stderr_log();
    let view = view::ViewFlags::parse(args)?.resolve(None)?;
    let service = ocu_core::Service::with_devices(platform(), Some(mobile_platform()), None);
    if let Some((addr, token)) = view {
        view::start(service.clone(), addr, token)?;
    }
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
    fn extract_flags() {
        let f = ExtractFlags::parse(&args(&[
            "--field",
            "total=number:Amount due",
            "--field",
            "names=list",
            "--fields",
            r#"{"due": {"type": "date"}}"#,
            "--state",
            "Invoice total $10",
        ]))
        .unwrap();
        assert_eq!(f.fields["total"]["instructions"], "Amount due");
        assert_eq!(f.fields["names"]["items"], "text");
        assert_eq!(f.fields["due"]["type"], "date");
        assert_eq!(f.state.as_deref(), Some("Invoice total $10"));
        // Something to read, and only one of a window or text.
        assert!(ExtractFlags::parse(&args(&["--field", "a=text"])).is_err());
        assert!(
            ExtractFlags::parse(&args(&["--field", "a=text", "--app", "X", "--state", "s"]))
                .is_err()
        );
        assert!(ExtractFlags::parse(&args(&["--field", "a=colour", "--state", "s"])).is_err());
        assert!(ExtractFlags::parse(&args(&["--state", "s"])).is_err());
        assert!(ExtractFlags::parse(&args(&["--field", "a=text", "--app", "X"])).is_ok());
    }

    #[test]
    fn serve_flags_need_values() {
        assert!(ServeFlags::parse(&args(&["serve", "--listen-on"])).is_err());
        assert!(ServeFlags::parse(&args(&["serve", "--listen-on", "--port", "9000"])).is_err());
        assert!(ServeFlags::parse(&args(&["serve", "--listen-on", "10.0.0.0/40"])).is_err());
        assert!(ServeFlags::parse(&args(&["serve", "--port", "x"])).is_err());
    }
}
