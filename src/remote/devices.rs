//! The devices allowed to drive this computer over HTTP, each with a key.
//! Kept in `devices.json` beside the config, readable by the user alone.
//! Only a hash of each key is stored: a key exists in full once, in the
//! skill made when it is generated.

use std::path::PathBuf;

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    /// What the device connecting in is called: "Work laptop".
    pub name: String,
    /// How that device reaches this computer: "http://my-mac.tail1234.ts.net:8642".
    pub url: String,
    /// SHA-256 of the key, hex.
    pub key_hash: String,
    /// Unix seconds.
    pub created: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Devices {
    pub devices: Vec<Device>,
}

pub fn path() -> PathBuf {
    crate::config::config_dir().join("devices.json")
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("no randomness: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn hash_key(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Devices {
    pub fn load() -> Self {
        std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| log::warn!("ignoring a malformed devices.json: {e}")).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("saving {}", path.display()))
    }

    /// The device a key belongs to.
    pub fn authenticate(&self, key: &str) -> Option<&Device> {
        let hash = hash_key(key);
        self.devices.iter().find(|d| d.key_hash == hash)
    }

    fn find(&mut self, id: &str) -> Result<&mut Device> {
        self.devices
            .iter_mut()
            .find(|d| d.id == id || d.name.eq_ignore_ascii_case(id))
            .ok_or_else(|| anyhow!("no device \"{id}\" (list_devices shows them)"))
    }

    /// Adds a device and returns it with its new key.
    pub fn add(&mut self, name: &str, url: &str) -> Result<(Device, String)> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "name the device that will connect");
        let url = normalize_url(url)?;
        let key = format!("ocu_{}", random_hex(24)?);
        let device = Device { id: random_hex(4)?, name: name.into(), url, key_hash: hash_key(&key), created: now() };
        self.devices.push(device.clone());
        self.save()?;
        Ok((device, key))
    }

    /// A new key for a device, retiring its old one. `url` replaces the
    /// stored one when given.
    pub fn regenerate(&mut self, id: &str, url: Option<&str>) -> Result<(Device, String)> {
        let key = format!("ocu_{}", random_hex(24)?);
        let url = url.filter(|u| !u.trim().is_empty()).map(normalize_url).transpose()?;
        let device = self.find(id)?;
        device.key_hash = hash_key(&key);
        if let Some(url) = url {
            device.url = url;
        }
        let device = device.clone();
        self.save()?;
        Ok((device, key))
    }

    pub fn remove(&mut self, id: &str) -> Result<Device> {
        let device = self.find(id)?.clone();
        self.devices.retain(|d| d.id != device.id);
        self.save()?;
        Ok(device)
    }
}

/// "my-mac.ts.net:8642" → "http://my-mac.ts.net:8642", without a trailing slash.
fn normalize_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    anyhow::ensure!(!url.is_empty(), "give the URL the device will reach this computer at");
    Ok(if url.contains("://") { url.to_string() } else { format!("http://{url}") })
}

/// This computer's MagicDNS name, when Tailscale runs here.
pub fn tailscale_name() -> Option<String> {
    let candidates = [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/usr/bin/tailscale",
        "tailscale",
    ];
    for cli in candidates {
        let Ok(out) = std::process::Command::new(cli).args(["status", "--json"]).output() else { continue };
        if !out.status.success() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let name = v.get("Self")?.get("DNSName")?.as_str()?.trim_end_matches('.').to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// The URL to suggest: this computer on Tailscale, or an example of one.
pub fn suggested_url(port: u16) -> String {
    match tailscale_name() {
        Some(name) => format!("http://{name}:{port}"),
        None => format!("http://{}.your-tailnet.ts.net:{port}", host_slug()),
    }
}

/// What this computer is called: "Astrid's MacBook Pro".
pub fn computer_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).output() {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !name.is_empty() {
            return name;
        }
    }
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this computer".into())
}

/// `computer_name` as a lowercase slug: "astrids-macbook-pro".
pub fn host_slug() -> String {
    slug(&computer_name())
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "computer".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_slugs() {
        assert_eq!(normalize_url("my-mac.ts.net:8642/").unwrap(), "http://my-mac.ts.net:8642");
        assert_eq!(normalize_url("https://x").unwrap(), "https://x");
        assert!(normalize_url("  ").is_err());
        assert_eq!(slug("Astrid's MacBook Pro"), "astrid-s-macbook-pro");
        assert_eq!(slug("!!!"), "computer");
    }

    #[test]
    fn keys_authenticate_by_hash() {
        let mut d = Devices::default();
        d.devices.push(Device { id: "a".into(), name: "n".into(), url: "u".into(), key_hash: hash_key("ocu_k"), created: 0 });
        assert!(d.authenticate("ocu_k").is_some());
        assert!(d.authenticate("ocu_x").is_none());
    }
}
