//! Registering this server with the MCP clients that have a CLI for it,
//! through their own `mcp add` commands so their config stays theirs.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Result};

/// The name the server is registered under. Not "computer-use": Claude Code
/// reserves that, and Codex ships its own server by that name.
pub const SERVER_NAME: &str = "opencomputeruse";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    ClaudeCode,
    Codex,
}

impl Client {
    pub const ALL: [Client; 2] = [Client::ClaudeCode, Client::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::Codex => "Codex",
        }
    }

    fn binary(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude",
            Client::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Ok(Client::ClaudeCode),
            "codex" => Ok(Client::Codex),
            _ => bail!("unknown client \"{s}\" (claude or codex)"),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Finds a client's CLI. An app opened from Finder has a bare PATH, so the
/// usual install locations are searched too.
pub fn find(client: Client) -> Option<PathBuf> {
    let name = client.binary();
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let h = home();
    for d in [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin", ".volta/bin", ".cargo/bin"] {
        dirs.push(h.join(d));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            dirs.push(Path::new(&appdata).join("npm"));
        }
    }
    dirs.into_iter()
        .flat_map(|d| [d.join(&exe), d.join(format!("{name}.cmd"))])
        .find(|p| p.is_file())
}

/// Whether the client's user-level config has this server registered.
/// The entry must run an `opencomputeruse` binary, so a same-named entry
/// someone else made is never mistaken for ours.
pub fn installed(client: Client) -> bool {
    let ours = |command: &str| command.contains("opencomputeruse");
    match client {
        Client::ClaudeCode => std::fs::read(home().join(".claude.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("mcpServers")?.get(SERVER_NAME)?.get("command")?.as_str().map(ours))
            .unwrap_or(false),
        Client::Codex => {
            let Ok(text) = std::fs::read_to_string(home().join(".codex/config.toml")) else { return false };
            let header = [format!("[mcp_servers.{SERVER_NAME}]"), format!("[mcp_servers.\"{SERVER_NAME}\"]")];
            let mut lines = text.lines().map(str::trim);
            lines.by_ref().find(|l| header.iter().any(|h| h == l)).is_some()
                && lines
                    .take_while(|l| !l.starts_with('['))
                    .any(|l| l.starts_with("command") && ours(l))
        }
    }
}

/// The command a client should run to start this server.
pub fn server_command() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

fn run(cli: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(cli);
    cmd.args(args);
    // Both CLIs are often scripts that want their own directory on PATH.
    if let Some(dir) = cli.parent() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths: Vec<PathBuf> = vec![dir.to_path_buf(), "/opt/homebrew/bin".into(), "/usr/local/bin".into()];
        paths.extend(std::env::split_paths(&path));
        cmd.env("PATH", std::env::join_paths(paths)?);
    }
    let out = cmd.output()?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        bail!("{} {} failed: {}", cli.display(), args.join(" "), text.trim());
    }
    Ok(text)
}

/// Registers (or re-registers, pointing at this build) the server for the
/// user across all their projects.
pub fn install(client: Client) -> Result<()> {
    let cli = find(client).ok_or_else(|| anyhow!("{} is not installed (no `{}` command found)", client.label(), client.binary()))?;
    let exe = server_command()?;
    let exe = exe.to_string_lossy();
    match client {
        Client::ClaudeCode => {
            if installed(client) {
                run(&cli, &["mcp", "remove", "--scope", "user", SERVER_NAME])?;
            }
            run(&cli, &["mcp", "add", "--scope", "user", SERVER_NAME, "--", &exe, "mcp"])?;
        }
        Client::Codex => {
            if installed(client) {
                run(&cli, &["mcp", "remove", SERVER_NAME])?;
            }
            run(&cli, &["mcp", "add", SERVER_NAME, "--", &exe, "mcp"])?;
        }
    }
    Ok(())
}

pub fn uninstall(client: Client) -> Result<()> {
    if !installed(client) {
        bail!("{} has no {SERVER_NAME} server of ours to remove", client.label());
    }
    let cli = find(client).ok_or_else(|| anyhow!("{} is not installed", client.label()))?;
    match client {
        Client::ClaudeCode => run(&cli, &["mcp", "remove", "--scope", "user", SERVER_NAME])?,
        Client::Codex => run(&cli, &["mcp", "remove", SERVER_NAME])?,
    };
    Ok(())
}

/// A JSON snippet for clients without a CLI (Claude Desktop, Cursor, …).
pub fn json_snippet() -> String {
    let exe = server_command().map(|p| p.display().to_string()).unwrap_or_else(|_| "opencomputeruse".into());
    serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": { SERVER_NAME: { "command": exe, "args": ["mcp"] } }
    }))
    .unwrap_or_default()
}
