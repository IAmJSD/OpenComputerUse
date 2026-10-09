//! Settings shared by the app's settings window and the MCP server: a JSON
//! file in the user's config directory, read fresh whenever it is needed so
//! changes apply without restarting anything.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// TypeSafe's Jev, from TypeSafe's own API.
    #[default]
    Typesafe,
    /// Cloudflare's Clef decision models on Workers AI.
    Cloudflare,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Typesafe, Provider::Cloudflare];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Typesafe => "TypeSafe (Jev)",
            Provider::Cloudflare => "Cloudflare Workers AI (Clef)",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RecipeConfig {
    pub provider: Provider,
    pub typesafe_api_key: String,
    pub typesafe_model: String,
    pub cloudflare_account_id: String,
    pub cloudflare_api_token: String,
    pub cloudflare_model: String,
    /// Send the window's screenshot with each decision (Clef only; it is
    /// always sent when a window has no accessibility tree).
    pub cloudflare_screenshots: bool,
    /// The least confidence at which a step acts on the element it picked.
    pub min_confidence: f64,
}

impl Default for RecipeConfig {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            typesafe_api_key: String::new(),
            typesafe_model: "jev-latest".into(),
            cloudflare_account_id: String::new(),
            cloudflare_api_token: String::new(),
            cloudflare_model: "@cf/cloudflare/clef-flash".into(),
            cloudflare_screenshots: true,
            min_confidence: 0.6,
        }
    }
}

impl RecipeConfig {
    /// Whether the chosen provider has what it needs to be called.
    pub fn is_configured(&self) -> bool {
        match self.provider {
            Provider::Typesafe => !self.typesafe_api_key.trim().is_empty(),
            Provider::Cloudflare => {
                !self.cloudflare_account_id.trim().is_empty()
                    && !self.cloudflare_api_token.trim().is_empty()
            }
        }
    }
}

/// The optional HTTP server for other devices. Off unless turned on.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub enabled: bool,
    pub port: u16,
    /// Every interface by default, so devices on the network (or the
    /// tailnet) can reach it; every request still needs a device's key.
    /// Used while `listen_on` is empty.
    pub bind: String,
    /// Limits the server to some of this computer's addresses, following
    /// them as they change. Each entry is a network adapter (`en0`, or
    /// `tailscale` for whichever adapter has this computer's Tailscale
    /// address), an address (`192.168.1.5`) or a range (`192.168.1.0/24`).
    /// Empty listens on `bind`.
    pub listen_on: Vec<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8642,
            bind: "0.0.0.0".into(),
            listen_on: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub recipe: RecipeConfig,
    pub http: HttpConfig,
    /// Show the cursor and halo over windows being driven (macOS).
    pub show_overlay: Option<bool>,
    /// Allow `unlock_screen` to unlock the Mac while it is locked (macOS).
    /// Off unless turned on; also needs the one-time `install-lock` setup.
    #[serde(default)]
    pub allow_unlock: bool,
    /// Drive phones, iOS simulators and Android emulators: their tools are
    /// offered only while this is on. Off unless turned on.
    #[serde(default)]
    pub mobile: bool,
    /// Load the panel hook into the apps a Windows session starts, so their
    /// open and save dialogs never show. Off unless turned on: it loads code
    /// into other processes, which endpoint protection may object to.
    #[serde(default)]
    pub windows_panel_hook: bool,
    /// Give the apps a Linux session starts a private session bus with a
    /// file chooser portal, so their dialogs never show. Calls the portal
    /// does not answer are forwarded to the user's own bus, so the keyring
    /// and notifications still work. Off unless turned on.
    #[serde(default)]
    pub linux_file_portal: bool,
}

impl Config {
    /// Whether to load the panel hook into the apps Windows sessions start:
    /// the setting, or `OCU_WINDOWS_PANEL_HOOK` (1 or 0).
    #[cfg(windows)]
    pub fn windows_panel_hook() -> bool {
        env_flag("OCU_WINDOWS_PANEL_HOOK").unwrap_or_else(|| Self::load().windows_panel_hook)
    }

    /// Whether Linux sessions answer file choosers through a portal: the
    /// setting, or `OCU_LINUX_FILE_PORTAL` (1 or 0).
    #[cfg(target_os = "linux")]
    pub fn linux_file_portal() -> bool {
        env_flag("OCU_LINUX_FILE_PORTAL").unwrap_or_else(|| Self::load().linux_file_portal)
    }

    /// Whether the phone, simulator and emulator tools are on: the
    /// setting, or `OCU_MOBILE` (1 or 0) where there is no app to set it.
    pub fn mobile_enabled() -> bool {
        env_flag("OCU_MOBILE").unwrap_or_else(|| Self::load().mobile)
    }

    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("OCU_CONFIG") {
            return PathBuf::from(p);
        }
        config_dir().join("config.json")
    }

    /// The saved settings, then environment overrides (handy where there is
    /// no settings window: Linux, Windows, CI).
    pub fn load() -> Self {
        let mut config: Config = std::fs::read(Self::path())
            .ok()
            .and_then(|b| {
                serde_json::from_slice(&b)
                    .map_err(|e| log::warn!("ignoring a malformed config: {e}"))
                    .ok()
            })
            .unwrap_or_default();
        let r = &mut config.recipe;
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("OCU_RECIPE_PROVIDER") {
            r.provider = if v.eq_ignore_ascii_case("cloudflare") {
                Provider::Cloudflare
            } else {
                Provider::Typesafe
            };
        }
        if let Some(v) = env("TYPESAFE_API_KEY") {
            r.typesafe_api_key = v;
        }
        if let Some(v) = env("CLOUDFLARE_ACCOUNT_ID") {
            r.cloudflare_account_id = v;
        }
        if let Some(v) = env("CLOUDFLARE_API_TOKEN") {
            r.cloudflare_api_token = v;
        }
        config
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        // It holds API keys: readable by the user alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("saving {}", path.display()))
    }

    #[cfg(target_os = "macos")]
    pub fn show_overlay(&self) -> bool {
        self.show_overlay.unwrap_or(true)
    }
}

pub fn config_dir() -> PathBuf {
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support/OpenComputerUse")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("OpenComputerUse")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("opencomputeruse")
    }
}

/// An on/off environment override, or `None` when unset or unreadable.
fn env_flag(var: &str) -> Option<bool> {
    match std::env::var(var).ok().as_deref().map(str::trim) {
        Some("1" | "true" | "yes" | "on") => Some(true),
        Some("0" | "false" | "no" | "off") => Some(false),
        _ => None,
    }
}
