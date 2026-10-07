//! Registering this server with MCP clients: through their own `mcp add`
//! commands where they have one, so their config stays theirs, and in
//! their config file where they do not (OpenCode, Claude Desktop).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Result};

/// The name the server is registered under. Not "computer-use": Claude Code
/// reserves that, and Codex ships its own server by that name.
pub const SERVER_NAME: &str = "opencomputeruse";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    OpenCode,
}

impl Client {
    pub const ALL: [Client; 4] = [
        Client::ClaudeCode,
        Client::ClaudeDesktop,
        Client::Codex,
        Client::OpenCode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::ClaudeDesktop => "Claude Desktop",
            Client::Codex => "Codex",
            Client::OpenCode => "OpenCode",
        }
    }

    fn binary(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude",
            Client::ClaudeDesktop => "claude-desktop",
            Client::Codex => "codex",
            Client::OpenCode => "opencode",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Ok(Client::ClaudeCode),
            "claude-desktop" | "claudedesktop" | "desktop" => Ok(Client::ClaudeDesktop),
            "codex" => Ok(Client::Codex),
            "opencode" => Ok(Client::OpenCode),
            _ => bail!("unknown client \"{s}\" (claude, claude-desktop, codex or opencode)"),
        }
    }

    /// What to do after installing for the server to show up.
    pub fn next_step(self) -> String {
        match self {
            // It reads its config once, at launch.
            Client::ClaudeDesktop => "Quit and reopen Claude Desktop to use it.".into(),
            _ => format!("Start a new {} session to use it.", self.label()),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Finds a client: its CLI (an app opened from Finder has a bare PATH, so
/// the usual install locations are searched too), or for OpenCode and
/// Claude Desktop, whose MCP servers live in their config files, any sign
/// they are installed.
pub fn find(client: Client) -> Option<PathBuf> {
    if client == Client::ClaudeDesktop {
        return desktop::find();
    }
    if client == Client::OpenCode {
        return opencode::config_dir()
            .is_dir()
            .then(opencode::config_dir)
            .or_else(|| Some(PathBuf::from("/Applications/OpenCode.app")).filter(|p| p.exists()))
            .or_else(|| find_cli("opencode"));
    }
    find_cli(client.binary())
}

fn find_cli(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let h = home();
    for d in [
        ".local/bin",
        ".claude/local",
        ".npm-global/bin",
        ".bun/bin",
        ".volta/bin",
        ".cargo/bin",
    ] {
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

/// The command a client's user-level config runs for this server's
/// name, if it has an entry.
pub fn registered_command(client: Client) -> Option<String> {
    match client {
        Client::ClaudeCode => std::fs::read(home().join(".claude.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v.get("mcpServers")?
                    .get(SERVER_NAME)?
                    .get("command")?
                    .as_str()
                    .map(str::to_string)
            }),
        Client::OpenCode => opencode::registered_command(),
        Client::ClaudeDesktop => desktop::registered_command(),
        Client::Codex => {
            let text = std::fs::read_to_string(home().join(".codex/config.toml")).ok()?;
            let header = [
                format!("[mcp_servers.{SERVER_NAME}]"),
                format!("[mcp_servers.\"{SERVER_NAME}\"]"),
            ];
            let mut lines = text.lines().map(str::trim);
            lines.by_ref().find(|l| header.iter().any(|h| h == l))?;
            let line = lines
                .take_while(|l| !l.starts_with('['))
                .find(|l| l.starts_with("command"))?;
            // command = "/path/to/opencomputeruse"
            let value = line.split_once('=')?.1.trim();
            Some(
                value
                    .trim_matches(|c| c == '"' || c == '\'')
                    .replace("\\\\", "\\"),
            )
        }
    }
}

/// Where a client's entry for this server stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registration {
    /// No entry, or an entry by this name that runs something else.
    Absent,
    /// It runs this copy of OpenComputerUse.
    Current,
    /// It runs an OpenComputerUse binary at another path (an old build, a
    /// moved app) or one that no longer exists: Reinstall points it here.
    Elsewhere(String),
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn registration(client: Client) -> Registration {
    let Some(command) = registered_command(client) else {
        return Registration::Absent;
    };
    // Someone else's server by the same name is never ours to touch.
    if !command.contains("opencomputeruse") {
        return Registration::Absent;
    }
    match server_command() {
        Ok(ours) if same_file(Path::new(&command), &ours) => Registration::Current,
        _ => Registration::Elsewhere(command),
    }
}

/// Whether the client has an entry of ours, current or not.
pub fn installed(client: Client) -> bool {
    registration(client) != Registration::Absent
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
        let mut paths: Vec<PathBuf> = vec![
            dir.to_path_buf(),
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
        ];
        paths.extend(std::env::split_paths(&path));
        cmd.env("PATH", std::env::join_paths(paths)?);
    }
    let out = cmd.output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!(
            "{} {} failed: {}",
            cli.display(),
            args.join(" "),
            text.trim()
        );
    }
    Ok(text)
}

/// Registers (or re-registers, pointing at this build) the server for the
/// user across all their projects.
pub fn install(client: Client) -> Result<()> {
    match client {
        Client::OpenCode => return opencode::set(Some(&server_command()?.to_string_lossy())),
        Client::ClaudeDesktop => return desktop::set(Some(&server_command()?.to_string_lossy())),
        _ => {}
    }
    let cli = find(client).ok_or_else(|| {
        anyhow!(
            "{} is not installed (no `{}` command found)",
            client.label(),
            client.binary()
        )
    })?;
    let exe = server_command()?;
    let exe = exe.to_string_lossy();
    match client {
        Client::ClaudeCode => {
            if installed(client) {
                run(&cli, &["mcp", "remove", "--scope", "user", SERVER_NAME])?;
            }
            run(
                &cli,
                &[
                    "mcp",
                    "add",
                    "--scope",
                    "user",
                    SERVER_NAME,
                    "--",
                    &exe,
                    "mcp",
                ],
            )?;
        }
        Client::Codex => {
            if installed(client) {
                run(&cli, &["mcp", "remove", SERVER_NAME])?;
            }
            run(&cli, &["mcp", "add", SERVER_NAME, "--", &exe, "mcp"])?;
        }
        Client::OpenCode | Client::ClaudeDesktop => unreachable!(),
    }
    Ok(())
}

pub fn uninstall(client: Client) -> Result<()> {
    if !installed(client) {
        bail!(
            "{} has no {SERVER_NAME} server of ours to remove",
            client.label()
        );
    }
    match client {
        Client::OpenCode => return opencode::set(None),
        Client::ClaudeDesktop => return desktop::set(None),
        _ => {}
    }
    let cli = find(client).ok_or_else(|| anyhow!("{} is not installed", client.label()))?;
    match client {
        Client::ClaudeCode => run(&cli, &["mcp", "remove", "--scope", "user", SERVER_NAME])?,
        Client::Codex => run(&cli, &["mcp", "remove", SERVER_NAME])?,
        Client::OpenCode | Client::ClaudeDesktop => unreachable!(),
    };
    Ok(())
}

/// A JSON snippet for clients set up by hand (Cursor, Windsurf, …).
pub fn json_snippet() -> String {
    let exe = server_command()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "opencomputeruse".into());
    serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": { SERVER_NAME: { "command": exe, "args": ["mcp"] } }
    }))
    .unwrap_or_default()
}

/// Writes a client's JSON config back: a copy of what was there beside it,
/// then the new text through a temporary file, so a failure midway never
/// leaves half a config.
fn write_config(path: &Path, original: &str, config: &serde_json::Value) -> Result<()> {
    use anyhow::Context as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !original.is_empty() {
        std::fs::write(path.with_extension("opencomputeruse-backup"), original)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(config)? + "\n")?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

mod desktop {
    //! Claude Desktop keeps MCP servers in `claude_desktop_config.json`,
    //! under `mcpServers`, beside the app's own preferences, and reads it
    //! when it starts.

    use std::path::PathBuf;

    use anyhow::{Context as _, Result};
    use serde_json::{json, Map, Value};

    use super::{home, write_config, SERVER_NAME};

    fn config_dir() -> PathBuf {
        if cfg!(target_os = "macos") {
            home().join("Library/Application Support/Claude")
        } else if cfg!(windows) {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join("AppData/Roaming"))
                .join("Claude")
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".config"))
                .join("Claude")
        }
    }

    fn path() -> PathBuf {
        config_dir().join("claude_desktop_config.json")
    }

    /// The app, or its config folder once it has run.
    pub fn find() -> Option<PathBuf> {
        let mut apps = vec![
            PathBuf::from("/Applications/Claude.app"),
            home().join("Applications/Claude.app"),
        ];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            apps.push(PathBuf::from(local).join("AnthropicClaude"));
        }
        apps.into_iter()
            .find(|p| p.exists())
            .or_else(|| Some(config_dir()).filter(|d| d.is_dir()))
    }

    fn load() -> Result<(String, Value)> {
        let text = std::fs::read_to_string(path()).unwrap_or_default();
        if text.trim().is_empty() {
            return Ok((text, json!({})));
        }
        let value =
            serde_json::from_str(&text).with_context(|| format!("reading {}", path().display()))?;
        Ok((text, value))
    }

    pub fn registered_command() -> Option<String> {
        let (_, config) = load().ok()?;
        config
            .get("mcpServers")?
            .get(SERVER_NAME)?
            .get("command")?
            .as_str()
            .map(str::to_string)
    }

    /// Adds (with `exe`) or removes the server's entry, keeping the app's
    /// preferences and other servers as they were.
    pub fn set(exe: Option<&str>) -> Result<()> {
        let (original, mut config) = load()?;
        let root = config
            .as_object_mut()
            .context("Claude Desktop's config is not a JSON object")?;
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()));
        let servers = servers
            .as_object_mut()
            .context("Claude Desktop's \"mcpServers\" setting is not an object")?;
        match exe {
            Some(exe) => {
                servers.insert(
                    SERVER_NAME.into(),
                    json!({ "command": exe, "args": ["mcp"] }),
                );
            }
            None => {
                servers.remove(SERVER_NAME);
                if servers.is_empty() {
                    root.remove("mcpServers");
                }
            }
        }
        write_config(&path(), &original, &config)
    }
}

mod opencode {
    //! OpenCode keeps MCP servers in its config file (it has no `mcp add`):
    //! `~/.config/opencode/opencode.jsonc` or `opencode.json`, under `mcp`.

    use std::path::PathBuf;

    use anyhow::{bail, Context as _, Result};
    use serde_json::{json, Map, Value};

    use super::{home, write_config, SERVER_NAME};

    pub fn config_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("opencode")
    }

    /// The config file in use, or where a new one goes.
    fn path() -> PathBuf {
        let dir = config_dir();
        ["opencode.jsonc", "opencode.json"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.exists())
            .unwrap_or_else(|| dir.join("opencode.json"))
    }

    /// Whether JSONC text has comments (outside strings), which a rewrite
    /// through a JSON parser would drop.
    fn has_comments(text: &str) -> bool {
        let (mut in_string, mut escaped, mut prev) = (false, false, '\0');
        for c in text.chars() {
            if in_string {
                match (escaped, c) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => in_string = false,
                    _ => {}
                }
            } else if c == '"' {
                in_string = true;
            } else if prev == '/' && (c == '/' || c == '*') {
                return true;
            }
            prev = if in_string { '\0' } else { c };
        }
        false
    }

    fn load() -> Result<Value> {
        match std::fs::read_to_string(path()) {
            Ok(text) if text.trim().is_empty() => {
                Ok(json!({ "$schema": "https://opencode.ai/config.json" }))
            }
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("reading {}", path().display()))
            }
            Err(_) => Ok(json!({ "$schema": "https://opencode.ai/config.json" })),
        }
    }

    pub fn registered_command() -> Option<String> {
        load().ok().and_then(|v| {
            v.get("mcp")?
                .get(SERVER_NAME)?
                .get("command")?
                .get(0)?
                .as_str()
                .map(str::to_string)
        })
    }

    /// Adds (with `exe`) or removes the server's entry, keeping the rest of
    /// the file as it was and a copy of the original beside it.
    pub fn set(exe: Option<&str>) -> Result<()> {
        let path = path();
        let original = std::fs::read_to_string(&path).unwrap_or_default();
        if has_comments(&original) {
            bail!(
                "{} has comments, which rewriting it would lose. Add this under \"mcp\" yourself: \"{SERVER_NAME}\": {{ \"type\": \"local\", \"command\": [\"{}\", \"mcp\"], \"enabled\": true }}",
                path.display(),
                exe.unwrap_or("…")
            );
        }
        let mut config = load()?;
        let root = config
            .as_object_mut()
            .context("OpenCode's config is not a JSON object")?;
        let mcp = root
            .entry("mcp")
            .or_insert_with(|| Value::Object(Map::new()));
        let mcp = mcp
            .as_object_mut()
            .context("OpenCode's \"mcp\" setting is not an object")?;
        match exe {
            Some(exe) => {
                mcp.insert(
                    SERVER_NAME.into(),
                    json!({ "type": "local", "command": [exe, "mcp"], "enabled": true }),
                );
            }
            None => {
                mcp.remove(SERVER_NAME);
                // Leave no empty "mcp" behind where there was none before.
                if mcp.is_empty() {
                    root.remove("mcp");
                }
            }
        }
        write_config(&path, &original, &config)
    }

    #[cfg(test)]
    mod tests {
        use super::has_comments;

        #[test]
        fn comments_are_found_outside_strings_only() {
            assert!(has_comments("{ // hi\n }"));
            assert!(has_comments("{ /* hi */ }"));
            assert!(!has_comments(r#"{ "url": "http://x/y" }"#));
            assert!(!has_comments(r#"{ "a": "\"//\"" }"#));
        }
    }
}
