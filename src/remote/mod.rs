//! Letting other devices drive this computer over HTTP: the device keys,
//! the skills that carry them, the server, and starting it at login so it
//! survives reboots. Managed only from this computer, through the app or the
//! local MCP server's device tools.

pub mod devices;
pub mod interfaces;
pub mod server;
pub mod skill;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::config::Config;
use crate::tools::Output;
use devices::Devices;

/// A new or regenerated device key, as what the device sets up with it.
pub struct Issued {
    pub device: devices::Device,
    /// What the device adds under `hosts:` in its hosts file: this
    /// computer's name, URL and the key.
    pub host_entry: String,
    /// The generic skill, the same for every computer (no key).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub skill: String,
    /// Both, inside a message asking the device's agent to set them up.
    pub prompt: String,
}

impl Issued {
    fn new(device: devices::Device, key: &str) -> Self {
        let host_entry = skill::host_entry(&device, key);
        let skill = skill::render_generic(&crate::tools::list());
        Issued {
            prompt: skill::agent_prompt(&skill, &host_entry),
            device,
            host_entry,
            skill,
        }
    }
}

pub fn generate(name: &str, url: &str) -> Result<Issued> {
    let mut d = Devices::load();
    let (device, key) = d.add(name, url)?;
    Ok(Issued::new(device, &key))
}

pub fn regenerate(id: &str, url: Option<&str>) -> Result<Issued> {
    let mut d = Devices::load();
    let (device, key) = d.regenerate(id, url)?;
    Ok(Issued::new(device, &key))
}

/// Saves a file to `~/Downloads/<folder>/<file>`, created readable by the
/// user alone (it may hold a key), and returns where it went. A link at the
/// folder or file is never written through; an existing regular file is
/// replaced.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn save_download(folder: &str, file: &str, text: &str) -> Result<std::path::PathBuf> {
    use std::io::Write as _;

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    let dir = std::path::Path::new(&home).join("Downloads").join(folder);
    let path = dir.join(file);
    for p in [&dir, &path] {
        anyhow::ensure!(
            !std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()),
            "{} is a symlink; not writing through it",
            p.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(text.as_bytes())?;
        // A file that was already there keeps its old mode otherwise.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, text)?;
    }
    Ok(path)
}

/// Turns the server on or off (and moves it, or limits where it listens),
/// and has it start at login while it is on.
pub fn set_server(
    enabled: Option<bool>,
    port: Option<u16>,
    listen_on: Option<&[String]>,
) -> Result<Config> {
    let mut config = Config::load();
    if let Some(on) = enabled {
        config.http.enabled = on;
    }
    if let Some(port) = port {
        anyhow::ensure!(port >= 1024, "use a port from 1024 up");
        config.http.port = port;
    }
    if let Some(listen_on) = listen_on {
        config.http.listen_on = interfaces::clean(listen_on)?;
    }
    config.save()?;
    if cfg!(target_os = "macos") {
        if let Err(e) = autostart::set(config.http.enabled) {
            log::warn!("start at login: {e:#}");
        }
    }
    Ok(config)
}

// ------------------------------------------------------------- MCP tools

const TOOLS: &[&str] = &[
    "list_devices",
    "generate_skill",
    "regenerate_key",
    "remove_device",
    "http_server",
];

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

pub fn tool_definitions() -> Vec<Value> {
    let http = Config::load().http;
    let port = http.port;
    vec![
        json!({
            "name": "http_server",
            "description": "Show, or turn on and off, the HTTP server that lets other devices drive this computer with a key (off by default). While on, it starts again after a reboot. Devices reach it at the URL in their skill, usually over Tailscale. It listens on every network adapter unless listen_on limits it, for example to Tailscale only.",
            "inputSchema": { "type": "object", "properties": {
                "enabled": { "type": "boolean", "description": "Turn the server on or off. Leave out to only show its state." },
                "port": { "type": "integer", "description": format!("The port to listen on. Currently {port}.") },
                "listen_on": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": format!(
                        "Limit the server to some of this computer's addresses, following them as they change. Each entry is a network adapter (\"en0\", or \"tailscale\" for whichever adapter has the Tailscale address), an address (\"192.168.1.5\") or a range (\"192.168.1.0/24\"). An empty list listens on every adapter again. Currently {}. This computer's adapters: {}.",
                        listen_on_text(&http.listen_on),
                        adapters_text(),
                    ),
                },
            } },
        }),
        json!({
            "name": "list_devices",
            "description": "List the devices allowed to drive this computer over HTTP.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "generate_skill",
            "description": "Allow a device to drive this computer over HTTP: makes it a key and returns a prompt to paste into an agent on that device. The prompt holds the generic opencomputeruse-remote skill and this computer's entry for the device's ~/.config/opencomputeruse/hosts.yaml, which holds the key, and asks the agent to set both up. The key is shown only here, so pass the prompt on as it is.",
            "inputSchema": { "type": "object", "properties": {
                "device_name": { "type": "string", "description": "What the device that will connect is called, e.g. \"Work laptop\"." },
                "url": { "type": "string", "description": format!("How that device reaches this computer. Defaults to {}.", devices::suggested_url(port)) },
            }, "required": ["device_name"] },
        }),
        json!({
            "name": "regenerate_key",
            "description": "Give a device a new key, retiring the old one and ending its sessions, and return a prompt for an agent on that device that sets the new key up (as generate_skill does).",
            "inputSchema": { "type": "object", "properties": {
                "device": { "type": "string", "description": "The device's id or name, from list_devices." },
                "url": { "type": "string", "description": "A new URL for it to reach this computer at. Leave out to keep the current one." },
            }, "required": ["device"] },
        }),
        json!({
            "name": "remove_device",
            "description": "Stop a device driving this computer: deletes its key and ends its sessions.",
            "inputSchema": { "type": "object", "properties": {
                "device": { "type": "string", "description": "The device's id or name, from list_devices." },
            }, "required": ["device"] },
        }),
    ]
}

fn text(t: String) -> Result<Output> {
    Ok(Output {
        text: t,
        image: None,
    })
}

fn server_state(config: &Config) -> String {
    let h = &config.http;
    if !h.enabled {
        return format!("The HTTP server is off (port {}).", h.port);
    }
    let how = if cfg!(target_os = "macos") {
        "the OpenComputerUse app serves it, and starts at login to keep serving after a reboot"
    } else {
        "`opencomputeruse serve` serves it; `opencomputeruse serve --install` keeps it running after a reboot"
    };
    if h.listen_on.is_empty() {
        return format!(
            "The HTTP server is on, listening on {}:{}; {how}.",
            h.bind, h.port
        );
    }
    let addrs: Vec<String> = interfaces::listen_addrs(h)
        .unwrap_or_default()
        .iter()
        .map(ToString::to_string)
        .collect();
    let now = if addrs.is_empty() {
        "none of them has an address now".to_string()
    } else {
        format!("now {}", addrs.join(", "))
    };
    format!(
        "The HTTP server is on, listening on {} only ({now}); {how}.",
        listen_on_text(&h.listen_on)
    )
}

fn listen_on_text(listen_on: &[String]) -> String {
    if listen_on.is_empty() {
        "every adapter".into()
    } else {
        listen_on.join(", ")
    }
}

/// This computer's adapters and their addresses, for the tool description.
fn adapters_text() -> String {
    interfaces::adapters()
        .iter()
        .map(|a| {
            let addrs: Vec<String> = a.addrs.iter().map(ToString::to_string).collect();
            format!("{} ({})", a.name, addrs.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn call(name: &str, args: &Value) -> Result<Output> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    match name {
        "http_server" => {
            let enabled = args.get("enabled").and_then(Value::as_bool);
            let port = args.get("port").and_then(Value::as_u64).map(|p| p as u16);
            let listen_on = match args.get("listen_on") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .map(|v| v.as_str().map(str::to_string))
                        .collect::<Option<Vec<String>>>()
                        .ok_or_else(|| anyhow::anyhow!("\"listen_on\" is a list of strings"))?,
                ),
                Some(_) => bail!("\"listen_on\" is a list of strings"),
            };
            let config = if enabled.is_some() || port.is_some() || listen_on.is_some() {
                set_server(enabled, port, listen_on.as_deref())?
            } else {
                Config::load()
            };
            text(server_state(&config))
        }
        "list_devices" => {
            let d = Devices::load();
            if d.devices.is_empty() {
                return text("No devices yet; generate_skill adds one.".into());
            }
            let list: Vec<Value> = d
                .devices
                .iter()
                .map(|d| json!({ "id": d.id, "name": d.name, "url": d.url, "created": d.created }))
                .collect();
            text(serde_json::to_string_pretty(&list)?)
        }
        "generate_skill" => {
            let Some(device_name) = s("device_name") else {
                bail!("missing \"device_name\"")
            };
            let url = s("url")
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| devices::suggested_url(Config::load().http.port));
            let issued = generate(&device_name, &url)?;
            text(issued_text(&issued))
        }
        "regenerate_key" => {
            let Some(device) = s("device") else {
                bail!("missing \"device\"")
            };
            let issued = regenerate(&device, s("url").as_deref())?;
            text(issued_text(&issued))
        }
        "remove_device" => {
            let Some(device) = s("device") else {
                bail!("missing \"device\"")
            };
            let d = Devices::load().remove(&device)?;
            text(format!(
                "Removed {} ({}); its key no longer works.",
                d.name, d.id
            ))
        }
        _ => bail!("unknown tool \"{name}\""),
    }
}

fn issued_text(issued: &Issued) -> String {
    let mut note = format!(
        "Key for {} ({}). Paste everything below the line into an agent on that device (Claude Code, Codex, OpenCode, …): it adds this computer to the device's {} and installs the skill. The key is not shown anywhere else.",
        issued.device.name,
        issued.device.id,
        skill::HOSTS_FILE
    );
    if !Config::load().http.enabled {
        note.push_str(" The HTTP server is off: turn it on (http_server with enabled: true) before the device connects.");
    }
    format!("{note}\n\n---\n\n{}", issued.prompt)
}

// ------------------------------------------------------- start at login

pub mod autostart {
    //! Starting the server after a reboot. macOS: a LaunchAgent that opens
    //! the app in the background at login (through LaunchServices, so it
    //! keeps its own permissions). Linux: a systemd user service. Windows:
    //! a Run key. The last two are for `opencomputeruse serve`.

    use anyhow::{Context as _, Result};
    #[cfg(not(target_os = "macos"))]
    use std::path::PathBuf;

    const LABEL: &str = "com.infrawrench.opencomputeruse";

    #[cfg(target_os = "macos")]
    fn plist_path() -> std::path::PathBuf {
        std::path::Path::new(&std::env::var_os("HOME").unwrap_or_default())
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    /// Installs or removes the login item that brings the app (and so the
    /// server) back after a reboot.
    #[cfg(target_os = "macos")]
    pub fn set(on: bool) -> Result<()> {
        let path = plist_path();
        if !on {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        let Some(bundle) = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
        else {
            // A development build has no bundle to open at login.
            anyhow::bail!(
                "not running from OpenComputerUse.app, so there is nothing to start at login"
            );
        };
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<!-- Opens OpenComputerUse in the background at login, so its HTTP server
	     comes back after a reboot. Removed when the server is turned off. -->
	<key>ProgramArguments</key>
	<array>
		<string>/usr/bin/open</string>
		<string>-g</string>
		<string>-a</string>
		<string>{bundle}</string>
		<string>--args</string>
		<string>agent</string>
		<string>--background</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#,
            bundle = xml_escape(&bundle.to_string_lossy())
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))
    }

    #[cfg(target_os = "macos")]
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    #[cfg(target_os = "macos")]
    pub fn is_set() -> bool {
        plist_path().exists()
    }

    #[cfg(target_os = "linux")]
    fn unit_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            });
        base.join("systemd/user/opencomputeruse.service")
    }

    /// `serve --install` / `--uninstall`: keep the server running across
    /// reboots, started from this binary.
    #[cfg(target_os = "linux")]
    pub fn set(on: bool) -> Result<()> {
        let path = unit_path();
        let systemctl = |args: &[&str]| {
            std::process::Command::new("systemctl")
                .arg("--user")
                .args(args)
                .status()
        };
        if !on {
            let _ = systemctl(&["disable", "--now", "opencomputeruse.service"]);
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        let unit = format!(
            "[Unit]\nDescription=OpenComputerUse HTTP server\n\n[Service]\nExecStart={} serve\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
            exe.display()
        );
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, unit)?;
        systemctl(&["daemon-reload"])?;
        let status = systemctl(&["enable", "--now", "opencomputeruse.service"])?;
        anyhow::ensure!(status.success(), "systemctl --user enable failed");
        Ok(())
    }

    #[cfg(windows)]
    pub fn set(on: bool) -> Result<()> {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let status = if on {
            let exe = std::env::current_exe()?;
            std::process::Command::new("reg")
                .args([
                    "add",
                    key,
                    "/v",
                    "OpenComputerUse",
                    "/t",
                    "REG_SZ",
                    "/f",
                    "/d",
                ])
                .arg(format!("\"{}\" serve", exe.display()))
                .status()?
        } else {
            std::process::Command::new("reg")
                .args(["delete", key, "/v", "OpenComputerUse", "/f"])
                .status()?
        };
        anyhow::ensure!(status.success() || !on, "reg failed");
        Ok(())
    }
}
