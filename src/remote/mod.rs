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

/// A new or regenerated device key, rendered as the skill that carries it.
pub struct Issued {
    pub device: devices::Device,
    pub skill: String,
}

pub fn generate(name: &str, url: &str) -> Result<Issued> {
    let mut d = Devices::load();
    let (device, key) = d.add(name, url)?;
    Ok(Issued {
        skill: skill::render(&device, &key, &crate::tools::list()),
        device,
    })
}

pub fn regenerate(id: &str, url: Option<&str>) -> Result<Issued> {
    let mut d = Devices::load();
    let (device, key) = d.regenerate(id, url)?;
    Ok(Issued {
        skill: skill::render(&device, &key, &crate::tools::list()),
        device,
    })
}

/// Where a skill is saved: `~/Downloads/<skill name>/SKILL.md`, ready to
/// copy into a device's skills folder.
pub fn save_skill(skill: &str) -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    let dir = std::path::Path::new(&home)
        .join("Downloads")
        .join(skill::skill_name());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, skill)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// A listen address from the user: an IP address, so a typo fails here
/// rather than when the server tries to listen.
pub fn parse_bind(bind: &str) -> Result<String> {
    let ip: std::net::IpAddr = bind.trim().parse().map_err(|_| {
        anyhow::anyhow!("the listen address must be an IP address, like 127.0.0.1 or 0.0.0.0")
    })?;
    Ok(ip.to_string())
}

/// Turns the server on or off (and moves it), and has it start at login
/// while it is on.
pub fn set_server(
    enabled: Option<bool>,
    port: Option<u16>,
    bind: Option<&str>,
) -> Result<Config> {
    let mut config = Config::load();
    if let Some(on) = enabled {
        config.http.enabled = on;
    }
    if let Some(port) = port {
        anyhow::ensure!(port >= 1024, "use a port from 1024 up");
        config.http.port = port;
    }
    if let Some(bind) = bind {
        config.http.bind = parse_bind(bind)?;
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
    let (port, bind) = (http.port, http.bind);
    vec![
        json!({
            "name": "http_server",
            "description": "Show, or turn on and off, the HTTP server that lets other devices drive this computer with a key (off by default). While on, it starts again after a reboot. It listens on 127.0.0.1 (this computer only) unless `bind` names another address, which other devices need before they can reach it; any non-loopback address, such as 0.0.0.0, exposes control of this computer to the network. Devices reach it at the URL in their skill, usually over Tailscale.",
            "inputSchema": { "type": "object", "properties": {
                "enabled": { "type": "boolean", "description": "Turn the server on or off. Leave out to only show its state." },
                "port": { "type": "integer", "description": format!("The port to listen on. Currently {port}.") },
                "bind": { "type": "string", "description": format!("The IP address to listen on. Currently {bind}. 127.0.0.1 reaches this computer only; this computer's Tailscale or LAN address, or 0.0.0.0 for every interface, exposes control of it to the network.") },
            } },
        }),
        json!({
            "name": "list_devices",
            "description": "List the devices allowed to drive this computer over HTTP.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "generate_skill",
            "description": "Allow a device to drive this computer over HTTP: makes it a key and returns a SKILL.md, containing the key, to install on that device (in its skills folder, e.g. ~/.claude/skills/<name>/SKILL.md). The key is shown only in this skill.",
            "inputSchema": { "type": "object", "properties": {
                "device_name": { "type": "string", "description": "What the device that will connect is called, e.g. \"Work laptop\"." },
                "url": { "type": "string", "description": format!("How that device reaches this computer. Defaults to {}.", devices::suggested_url(port)) },
            }, "required": ["device_name"] },
        }),
        json!({
            "name": "regenerate_key",
            "description": "Give a device a new key, retiring the old one and ending its sessions, and return the new skill to install on it.",
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
    format!(
        "The HTTP server is on, listening on {}; {how}.",
        server::listen_addr(&h.bind, h.port)
    )
}

pub fn call(name: &str, args: &Value) -> Result<Output> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    match name {
        "http_server" => {
            let enabled = args.get("enabled").and_then(Value::as_bool);
            let port = args.get("port").and_then(Value::as_u64).map(|p| p as u16);
            let bind = s("bind");
            let config = if enabled.is_some() || port.is_some() || bind.is_some() {
                set_server(enabled, port, bind.as_deref())?
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
        "Skill for {} ({}). Install it on that device as ~/.claude/skills/{}/SKILL.md. It contains the device's key, which is not shown anywhere else.",
        issued.device.name,
        issued.device.id,
        skill::skill_name()
    );
    if !Config::load().http.enabled {
        note.push_str(" The HTTP server is off: turn it on (http_server with enabled: true) before the device connects.");
    }
    format!("{note}\n\n{}", issued.skill)
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

#[cfg(test)]
mod tests {
    use super::parse_bind;

    #[test]
    fn bind_takes_ip_addresses_only() {
        assert_eq!(parse_bind("127.0.0.1").unwrap(), "127.0.0.1");
        assert_eq!(parse_bind(" 0.0.0.0 ").unwrap(), "0.0.0.0");
        assert_eq!(parse_bind("::1").unwrap(), "::1");
        assert!(parse_bind("").is_err());
        assert!(parse_bind("localhost").is_err());
        assert!(parse_bind("127.0.0.1:8642").is_err());
    }
}
