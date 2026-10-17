//! `opencomputeruse ssh-setup HOST --name NAME`: installs this version
//! on a Linux host over SSH, has it serve on that host's loopback only,
//! makes a device key there, and keeps an SSH tunnel to it up from this
//! computer. With `--use-installed` it instead connects to an
//! opencomputeruse already running there (Linux or macOS) and changes
//! nothing on the host but the new device. The hosts file then holds a plain `http://127.0.0.1:PORT` URL,
//! so the generic skill needs nothing new.
//!
//! The builders are pure; `setup` is the runner. Host, name and key path are
//! validated first, ssh gets them as separate arguments after `--`, and the
//! remote script is a constant that reaches the host on stdin (its few
//! values are validated, then quoted).

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, ensure, Context as _, Result};

use super::skill;

/// The port the remote server listens on, on its loopback.
const REMOTE_PORT: u16 = 8642;
/// The first local port tried for a tunnel.
const FIRST_LOCAL_PORT: u16 = 18642;
const UNIT_PREFIX: &str = "ocu-tunnel-";
const LABEL_PREFIX: &str = "com.infrawrench.opencomputeruse.tunnel.";
const RELEASES_DOWNLOAD: &str = "https://github.com/IAmJSD/OpenComputerUse/releases/download";

// ------------------------------------------------------------ validation

fn plain(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !s.starts_with('-')
}

/// An SSH alias or `user@host`, with plain parts that cannot be SSH options.
pub fn validate_host(host: &str) -> Result<()> {
    let ok = host
        .split_once('@')
        .map_or_else(|| plain(host), |(user, host)| plain(user) && plain(host));
    ensure!(
        ok,
        "\"{host}\" is not an SSH alias or user@host (letters, digits and . _ - only, no leading dash)"
    );
    Ok(())
}

/// The name of the host in the hosts file and of the tunnel and device made
/// for it.
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        plain(name) && !name.starts_with('.') && name.len() <= 40,
        "\"{name}\" is not a usable name (letters, digits and . _ -, up to 40, no leading dot or dash)"
    );
    Ok(())
}

/// A key or program path that is safe to put in a unit file, a plist and an
/// ssh argument: absolute, with no spaces, `%`, `$`, quotes, backslashes,
/// angle brackets, `&` or control characters.
pub fn validate_path(path: &str) -> Result<()> {
    let bad = |c: char| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '%' | '$' | '"' | '\'' | '`' | '\\' | '<' | '>' | '&')
    };
    ensure!(
        path.starts_with('/') && !path.chars().any(bad),
        "\"{path}\" must be an absolute path without spaces, %, $, quotes or backslashes"
    );
    Ok(())
}

/// What a remote host reports for `uname -sm`. Installing needs a release
/// binary, so only Linux on x86_64 or aarch64; connecting to an install that
/// is already there (`connect`) also takes macOS.
pub fn parse_probe(out: &str, connect: bool) -> Result<String> {
    let out = last_line(out);
    let mut it = out.split_whitespace();
    let (os, arch) = (it.next().unwrap_or_default(), it.next().unwrap_or_default());
    match (os, arch) {
        ("Linux", "x86_64" | "amd64" | "aarch64" | "arm64") => Ok(format!("{os} {arch}")),
        ("Linux" | "Darwin", _) if connect && !arch.is_empty() => Ok(format!("{os} {arch}")),
        ("Darwin", _) => bail!(
            "macOS remotes are not installed over ssh yet; with opencomputeruse already there, use --use-installed"
        ),
        _ if connect => bail!(
            "unsupported remote \"{}\"; --use-installed needs Linux or macOS",
            out.trim()
        ),
        _ => bail!(
            "unsupported remote \"{}\"; only Linux x86_64 and aarch64 have a release",
            out.trim()
        ),
    }
}

/// The remote opencomputeruse path the check script reports: absolute, one
/// line. It is not `validate_path`d: `$HOME` may hold spaces. It only ever
/// reaches the remote as a quoted value.
pub fn validate_remote_bin(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/') && !path.chars().any(char::is_control),
        "the remote reported \"{}\" as opencomputeruse, which is not an absolute path",
        path.escape_debug()
    );
    Ok(())
}

/// The last non-empty line: login banners come before a command's output.
fn last_line(s: &str) -> &str {
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

// ------------------------------------------------------------ ssh argv

#[derive(Debug, Clone)]
pub struct Target {
    host: String,
    key: Option<String>,
}

impl Target {
    pub fn new(host: &str, key: Option<&str>) -> Result<Self> {
        validate_host(host)?;
        if let Some(k) = key {
            validate_path(k)?;
        }
        Ok(Self {
            host: host.into(),
            key: key.map(Into::into),
        })
    }

    fn key_opts(&self) -> Vec<String> {
        match &self.key {
            Some(k) => vec![
                "-o".into(),
                "IdentitiesOnly=yes".into(),
                "-i".into(),
                k.clone(),
            ],
            None => Vec::new(),
        }
    }
}

/// Whether ssh may ask the user anything. The first connection is
/// interactive so the host key can be accepted into known_hosts; every
/// later one must work unattended, as the tunnel will.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Interactive,
    Batch,
}

/// Arguments for `ssh` to run `remote` (words) on the target.
pub fn ssh_args(t: &Target, mode: Mode, remote: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = vec!["-o".into(), "ConnectTimeout=15".into()];
    if mode == Mode::Batch {
        a.extend(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"].map(String::from));
    }
    a.extend(t.key_opts());
    a.push("--".into());
    a.push(t.host.clone());
    a.extend(remote.iter().map(|s| s.to_string()));
    a
}

/// Arguments for the long-running tunnel from `127.0.0.1:local` to the
/// remote loopback server. Never asks, never accepts an unknown host key.
pub fn tunnel_args(t: &Target, local: u16, remote_port: u16) -> Vec<String> {
    let mut a: Vec<String> = [
        "-N",
        "-L",
        &format!("127.0.0.1:{local}:127.0.0.1:{remote_port}"),
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "StrictHostKeyChecking=yes",
    ]
    .map(String::from)
    .to_vec();
    a.extend(t.key_opts());
    a.push("--".into());
    a.push(t.host.clone());
    a
}

// ------------------------------------------------------- remote script

/// `bin` is the remote opencomputeruse the check found; `None` is the one
/// the install put in `~/.local/bin`.
pub enum RemoteJob<'a> {
    Install {
        version: &'a str,
        port: u16,
    },
    Check {
        port: u16,
    },
    /// `replace` gives a device of the same name a new key; without it a
    /// taken name fails.
    DeviceAdd {
        bin: Option<&'a str>,
        name: &'a str,
        url: &'a str,
        replace: bool,
    },
    DeviceRemove {
        bin: Option<&'a str>,
        name: &'a str,
    },
}

/// `s` as one shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The awk program that turns /proc/net/tcp{,6} lines into `addr:port`
/// for listeners on the port: loopback (v4 and v4-mapped) as 127.0.0.1, v6
/// loopback as `[::1]` (a tunnel to 127.0.0.1 cannot reach it), all-zero as
/// 0.0.0.0, anything else as `other`.
macro_rules! proc_awk {
    () => {
        r#"
      $4 == "0A" {
        split($2, a, ":")
        if (a[2] != hex) next
        ip = a[1]
        if (ip == "0100007F" || ip == "0000000000000000FFFF00000100007F") print "127.0.0.1:" port
        else if (ip == "00000000000000000000000001000000") print "[::1]:" port
        else if (ip ~ /^0+$/) print "0.0.0.0:" port
        else print "other:" port
      }"#
    };
}

/// The shell function `listeners`, which prints the `addr:port` of every
/// listener, from ss or /proc/net/tcp; it fails with neither. `$OCU_PORT`
/// must be set.
macro_rules! listeners_fn {
    () => {
        concat!(
            r#"listeners() {
  if command -v ss >/dev/null 2>&1; then
    ss -ltn | awk 'NR > 1 { print $4 }'
  elif [ -r /proc/net/tcp ]; then
    hex=$(printf '%04X' "$OCU_PORT")
    cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk -v hex="$hex" -v port="$OCU_PORT" '"#,
            proc_awk!(),
            r#"'
  else
    return 1
  fi
}
"#
        )
    };
}

/// `device list` is the probe: releases before the device commands fail it
/// as an unknown command.
macro_rules! device_cli_fn {
    () => {
        r#"require_device_cli() {
  "$1" device list >/dev/null 2>&1 || fail "remote opencomputeruse is too old, update it"
}
"#
    };
}

const INSTALL_BODY: &str = concat!(
    r#"set -eu
fail() { echo "ocu-setup: $*" >&2; exit 1; }
"#,
    device_cli_fn!(),
    r#"
[ "$(uname -s)" = Linux ] || fail "only Linux remotes are supported"
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) fail "no release binary for $(uname -m)" ;;
esac
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$1" "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$1" "$2"; }
else
  fail "curl or wget is needed to download opencomputeruse"
fi
bin="$HOME/.local/bin/opencomputeruse"
mkdir -p "$HOME/.local/bin"
if [ "$("$bin" --version 2>/dev/null || true)" != "opencomputeruse $OCU_VERSION" ]; then
  tmp="$bin.new.$$"
  fetch "$tmp" "$OCU_RELEASES/v$OCU_VERSION/opencomputeruse-linux-$arch" \
    || { rm -f "$tmp"; fail "could not download release v$OCU_VERSION for $arch"; }
  chmod 755 "$tmp"
  "$tmp" --version >/dev/null || fail "the downloaded binary does not run here"
  require_device_cli "$tmp"
  mv -f "$tmp" "$bin"
fi
"$bin" --version >/dev/null || fail "the downloaded binary does not run here"
require_device_cli "$bin"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
systemctl --user show-environment >/dev/null 2>&1 \
  || fail "no systemd user manager for $(id -un) (is this a login session?)"
loginctl enable-linger "$(id -un)" 2>/dev/null \
  || echo "ocu-setup: warning: could not enable linger; the server stops at logout and starts only at login" >&2

"$bin" serve --install --port "$OCU_PORT" --listen-on 127.0.0.1

"#,
    listeners_fn!(),
    r#"i=0
while :; do
  out=$(listeners) || fail "need ss or /proc/net/tcp to check what the server listens on"
  printf '%s\n' "$out" | grep -qx "127.0.0.1:$OCU_PORT" && break
  i=$((i + 1))
  [ "$i" -lt 10 ] || fail "the server is not listening on 127.0.0.1:$OCU_PORT"
  sleep 1
done
bad=$(printf '%s\n' "$out" | grep -E ":$OCU_PORT\$" | grep -vx "127.0.0.1:$OCU_PORT" || true)
[ -z "$bad" ] || fail "the server also listens on $bad; refusing to leave it exposed"
echo "ocu-setup: serving on 127.0.0.1:$OCU_PORT" >&2
"#
);

/// Finds an existing install and checks that it serves on `$OCU_PORT` where a
/// tunnel to 127.0.0.1 gets through. Changes nothing on the host. stdout is
/// the binary's path; the version and warnings go to stderr.
const CHECK_BODY: &str = concat!(
    r#"set -eu
fail() { echo "ocu-setup: $*" >&2; exit 1; }
"#,
    device_cli_fn!(),
    r#"
bin=
c=$(command -v opencomputeruse 2>/dev/null || true)
case "$c" in /*) [ -x "$c" ] && bin=$c ;; esac
for c in /Applications/OpenComputerUse.app/Contents/MacOS/opencomputeruse \
  "$HOME/Applications/OpenComputerUse.app/Contents/MacOS/opencomputeruse" \
  "$HOME/.local/bin/opencomputeruse"; do
  if [ -z "$bin" ] && [ -x "$c" ]; then bin=$c; fi
done
[ -n "$bin" ] || fail "opencomputeruse is not installed on this host (looked on PATH, in /Applications, ~/Applications and ~/.local/bin); install it there, or run without --use-installed"
ver=$("$bin" --version 2>/dev/null) || fail "$bin does not run here"
require_device_cli "$bin"
echo "ocu-version: $ver" >&2

"#,
    listeners_fn!(),
    r#"
if [ "$(uname -s)" = Darwin ]; then
  command -v lsof >/dev/null 2>&1 || fail "need lsof to check what the server listens on"
  out=$(lsof -nP -iTCP:"$OCU_PORT" -sTCP:LISTEN -Fn | sed -n 's/^n//p')
  none="cannot verify that anything listens on port $OCU_PORT (lsof shows only this user's processes); turn the HTTP server on first if it is off"
else
  out=$(listeners) || fail "need ss or /proc/net/tcp to check what the server listens on"
  none="nothing listens on port $OCU_PORT; turn the HTTP server on first"
fi
mine=$(printf '%s\n' "$out" | grep -E ":$OCU_PORT\$" || true)
[ -n "$mine" ] || fail "$none"
reach=$(printf '%s\n' "$mine" | grep -xE '(127\.0\.0\.1|0\.0\.0\.0|\*|\[::\]|\[::ffff:127\.0\.0\.1\]|\[::ffff:0\.0\.0\.0\]):'"$OCU_PORT" || true)
[ -n "$reach" ] || fail "the server listens only on $(printf '%s' "$mine" | tr '\n' ' '), which a tunnel to 127.0.0.1 cannot reach; set listen_on to 127.0.0.1 or all"
open=$(printf '%s\n' "$mine" | grep -vxE '(127\.0\.0\.1|\[::1\]|\[::ffff:127\.0\.0\.1\]):'"$OCU_PORT" || true)
[ -z "$open" ] || echo "ocu-setup: warning: the server also listens on $(printf '%s' "$open" | tr '\n' ' '), not just loopback; limit it with listen_on if that is unintended" >&2
printf '%s\n' "$bin"
"#
);

const DEVICE_ADD_BODY: &str = r#"set -eu
exec "${OCU_BIN:-$HOME/.local/bin/opencomputeruse}" device add --name "$OCU_NAME" --url "$OCU_URL" ${OCU_REPLACE:+--replace}
"#;

const DEVICE_REMOVE_BODY: &str = r#"set -eu
exec "${OCU_BIN:-$HOME/.local/bin/opencomputeruse}" device remove --name "$OCU_NAME"
"#;

/// The script to feed `sh -s` on the remote: a few quoted assignments, then
/// a body that is the same every time.
pub fn remote_script(job: &RemoteJob) -> Result<String> {
    let mut s = String::new();
    let mut set = |k: &str, v: &str| s.push_str(&format!("{k}={}\n", sh_quote(v)));
    let body = match job {
        RemoteJob::Install { version, port } => {
            ensure!(
                !version.is_empty()
                    && version
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-')),
                "odd version \"{version}\""
            );
            set("OCU_VERSION", version);
            set("OCU_PORT", &port.to_string());
            set("OCU_RELEASES", RELEASES_DOWNLOAD);
            INSTALL_BODY
        }
        RemoteJob::Check { port } => {
            set("OCU_PORT", &port.to_string());
            CHECK_BODY
        }
        RemoteJob::DeviceAdd {
            bin,
            name,
            url,
            replace,
        } => {
            validate_name(name)?;
            ensure!(
                url.starts_with("http://127.0.0.1:")
                    && url
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.')),
                "odd url \"{url}\""
            );
            if let Some(b) = bin {
                validate_remote_bin(b)?;
                set("OCU_BIN", b);
            }
            set("OCU_NAME", name);
            set("OCU_URL", url);
            set("OCU_REPLACE", if *replace { "1" } else { "" });
            DEVICE_ADD_BODY
        }
        RemoteJob::DeviceRemove { bin, name } => {
            validate_name(name)?;
            if let Some(b) = bin {
                validate_remote_bin(b)?;
                set("OCU_BIN", b);
            }
            set("OCU_NAME", name);
            DEVICE_REMOVE_BODY
        }
    };
    s.push_str(body);
    Ok(s)
}

// -------------------------------------------------------------- tunnel

/// systemd's quoting for one `ExecStart=` word.
pub fn systemd_quote(s: &str) -> String {
    let body = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{body}\"")
}

pub fn unit_file_name(name: &str) -> String {
    format!("{UNIT_PREFIX}{name}.service")
}

pub fn launchd_label(name: &str) -> String {
    format!("{LABEL_PREFIX}{name}")
}

/// A systemd user unit that keeps the tunnel up, retrying forever.
pub fn tunnel_unit(name: &str, host: &str, ssh: &str, args: &[String]) -> Result<String> {
    validate_name(name)?;
    validate_path(ssh)?;
    let exec: Vec<String> = std::iter::once(ssh)
        .chain(args.iter().map(String::as_str))
        .map(systemd_quote)
        .collect();
    Ok(format!(
        "[Unit]\nDescription=OpenComputerUse SSH tunnel to {host} ({name})\nStartLimitIntervalSec=0\n\n[Service]\nExecStart={}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        exec.join(" ")
    ))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A LaunchAgent that keeps the tunnel up from login on.
pub fn tunnel_plist(name: &str, ssh: &str, args: &[String]) -> Result<String> {
    validate_name(name)?;
    validate_path(ssh)?;
    let words: String = std::iter::once(ssh)
        .chain(args.iter().map(String::as_str))
        .map(|w| format!("\t\t<string>{}</string>\n", xml_escape(w)))
        .collect();
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{}</string>
	<key>ProgramArguments</key>
	<array>
{words}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>10</integer>
</dict>
</plist>
"#,
        xml_escape(&launchd_label(name))
    ))
}

/// The local port a tunnel unit or plist forwards from.
pub fn tunnel_port(text: &str) -> Option<u16> {
    let head = &text[..text.find(":127.0.0.1:")?];
    let digits: String = head
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

/// The tunnels already set up in `dir`, as (name, local port), from files
/// named `prefix` + name + `suffix`.
fn existing_tunnels(dir: &Path, prefix: &str, suffix: &str) -> Vec<(String, u16)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let name = file.strip_prefix(prefix)?.strip_suffix(suffix)?.to_string();
            let port = tunnel_port(&std::fs::read_to_string(e.path()).ok()?)?;
            Some((name, port))
        })
        .collect()
}

/// The local port for `name`'s tunnel: the one its unit already forwards
/// from, else the first free one from 18642 that no other tunnel uses.
pub fn pick_port(
    name: &str,
    existing: &[(String, u16)],
    is_free: impl Fn(u16) -> bool,
) -> Result<u16> {
    if let Some((_, port)) = existing.iter().find(|(n, _)| n == name) {
        return Ok(*port);
    }
    let taken: HashSet<u16> = existing.iter().map(|(_, p)| *p).collect();
    (FIRST_LOCAL_PORT..FIRST_LOCAL_PORT + 200)
        .find(|p| !taken.contains(p) && is_free(*p))
        .context("no free local port from 18642 up")
}

// -------------------------------------------------------------- runner

/// What a setup needs, from `ssh-setup`'s arguments or the app's form.
#[derive(Debug, Clone)]
pub struct Options {
    pub host: String,
    pub name: String,
    pub key: Option<String>,
    /// Connect to an opencomputeruse already on the remote instead of
    /// installing one.
    pub use_installed: bool,
    pub remote_port: u16,
    /// Give a remote device of the same name a new key.
    pub replace: bool,
}

impl Options {
    /// Checks the values, with a leading `~/` in the key path expanded.
    pub fn new(
        host: &str,
        name: &str,
        key: Option<&str>,
        use_installed: bool,
        remote_port: Option<u16>,
        replace: bool,
    ) -> Result<Self> {
        ensure!(
            use_installed || remote_port.is_none(),
            "--remote-port only goes with --use-installed"
        );
        let key = key.map(|k| match k.strip_prefix("~/") {
            Some(rest) => crate::clients::home()
                .join(rest)
                .to_string_lossy()
                .into_owned(),
            None => k.to_string(),
        });
        validate_host(host)?;
        validate_name(name)?;
        if let Some(k) = &key {
            validate_path(k)?;
            ensure!(Path::new(k).is_file(), "no key file at {k}");
        }
        Ok(Self {
            host: host.into(),
            name: name.into(),
            key,
            use_installed,
            remote_port: remote_port.unwrap_or(REMOTE_PORT),
            replace,
        })
    }
}

fn parse_args(args: &[String]) -> Result<Options> {
    let (mut host, mut name, mut key, mut remote_port) = (None, None, None, None);
    let (mut use_installed, mut replace) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next().cloned(),
            "-i" | "--identity" => key = it.next().cloned(),
            "--use-installed" => use_installed = true,
            "--replace" => replace = true,
            "--remote-port" => {
                let v = it.next().map(String::as_str).unwrap_or_default();
                remote_port = Some(
                    v.parse::<u16>()
                        .ok()
                        .filter(|p| *p != 0)
                        .with_context(|| format!("--remote-port takes 1 to 65535, not \"{v}\""))?,
                );
            }
            _ if a.starts_with('-') => bail!("unknown option \"{a}\" for ssh-setup"),
            _ if host.is_none() => host = Some(a.clone()),
            _ => bail!("unexpected \"{a}\""),
        }
    }
    let usage = "usage: opencomputeruse ssh-setup HOST --name NAME [-i KEYFILE] [--replace] [--use-installed [--remote-port P]] (HOST is an SSH alias or user@host)";
    let (host, name) = (host.context(usage)?, name.context(usage)?);
    Options::new(
        &host,
        &name,
        key.as_deref(),
        use_installed,
        remote_port,
        replace,
    )
}

/// What the remote check reported.
#[derive(Debug, PartialEq)]
struct Checked {
    bin: String,
    warnings: Vec<String>,
}

/// Reads the check script's output: the binary on the last stdout line, its
/// version and the script's warnings on stderr. A different version from
/// `local_version` is a warning, not an error.
fn parse_check(stdout: &str, stderr: &str, local_version: &str) -> Result<Checked> {
    let bin = last_line(stdout);
    validate_remote_bin(bin)?;
    let mut warnings: Vec<String> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("ocu-setup: warning: "))
        .map(String::from)
        .collect();
    let remote = stderr
        .lines()
        .find_map(|l| l.strip_prefix("ocu-version: opencomputeruse "))
        .map(str::trim);
    match remote {
        Some(v) if v == local_version => {}
        Some(v) => warnings.push(format!(
            "the remote runs opencomputeruse {v}, this computer {local_version}; they may not match"
        )),
        None => warnings.push("the remote did not report its version".into()),
    }
    Ok(Checked {
        bin: bin.into(),
        warnings,
    })
}

/// The first `ssh` on PATH, which must be a path that is safe in a unit or
/// plist; with none on PATH, `/usr/bin/ssh`.
fn ssh_from_path(path: &std::ffi::OsStr) -> Result<String> {
    let Some(found) = std::env::split_paths(path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join("ssh"))
        .find(|p| p.is_file())
    else {
        return Ok("/usr/bin/ssh".into());
    };
    let found = found.to_string_lossy().into_owned();
    validate_path(&found).with_context(|| format!("the ssh on PATH, {found}, cannot be used"))?;
    Ok(found)
}

fn find_ssh() -> Result<String> {
    ssh_from_path(&std::env::var_os("PATH").unwrap_or_default())
}

struct Out {
    stdout: String,
    stderr: String,
}

fn run_checked(
    argv: &[String],
    script: Option<&str>,
    tty: bool,
    capture_stderr: bool,
    what: &str,
) -> Result<Out> {
    let (ok, out) = run_ssh_status(argv, script, tty, capture_stderr)?;
    ensure!(ok, "{what} failed{}", detail(&out.stderr));
    Ok(out)
}

/// ": " and ssh's complaint, if it made one.
fn detail(stderr: &str) -> String {
    match stderr.trim() {
        "" => String::new(),
        e => format!(": {e}"),
    }
}

/// Runs ssh with `script` on stdin if given. stdout is captured. With a
/// terminal (`tty`) stderr is captured only when asked, else passed through,
/// and without a script ssh may read the terminal; without one, stderr is
/// always captured and there is nothing to read.
fn run_ssh_status(
    argv: &[String],
    script: Option<&str>,
    tty: bool,
    capture_stderr: bool,
) -> Result<(bool, Out)> {
    let mut cmd = Command::new(find_ssh()?);
    cmd.args(argv)
        .stdin(match (script, tty) {
            (Some(_), _) => Stdio::piped(),
            (None, true) => Stdio::inherit(),
            (None, false) => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(if capture_stderr || !tty {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    let mut child = cmd.spawn().context("running ssh")?;
    // ssh may exit before reading the script; its stderr says why.
    let written = match script {
        Some(script) => child
            .stdin
            .take()
            .context("no stdin for ssh")?
            .write_all(script.as_bytes()),
        None => Ok(()),
    };
    let out = child.wait_with_output()?;
    if let Err(e) = written {
        ensure!(
            e.kind() == std::io::ErrorKind::BrokenPipe,
            "writing to ssh: {e}"
        );
    }
    Ok((
        out.status.success(),
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
    ))
}

fn uid() -> u32 {
    #[cfg(unix)]
    return unsafe { libc::getuid() };
    #[cfg(not(unix))]
    0
}

fn unit_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        crate::clients::home().join("Library/LaunchAgents")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::clients::home().join(".config"))
            .join("systemd/user")
    }
}

fn tunnel_file(name: &str) -> PathBuf {
    if cfg!(target_os = "macos") {
        unit_dir().join(format!("{}.plist", launchd_label(name)))
    } else {
        unit_dir().join(unit_file_name(name))
    }
}

fn run(program: &str, a: &[&str]) -> Result<bool> {
    Ok(Command::new(program)
        .args(a)
        .status()
        .with_context(|| format!("running {program}"))?
        .success())
}

/// Loads the unit or plist at `tunnel_file(name)` and (re)starts it; it then
/// restarts on its own whenever ssh exits.
fn start_tunnel(name: &str) -> Result<()> {
    let path = tunnel_file(name);
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid());
        let _ = run(
            "launchctl",
            &["bootout", &format!("{domain}/{}", launchd_label(name))],
        );
        // bootout returns before the label is gone, and bootstrap then fails.
        for attempt in 0..10 {
            if run(
                "launchctl",
                &["bootstrap", &domain, &path.to_string_lossy()],
            )? {
                return Ok(());
            }
            if attempt < 9 {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        bail!("launchctl bootstrap failed");
    }
    let unit = unit_file_name(name);
    ensure!(
        run("systemctl", &["--user", "daemon-reload"])?,
        "systemctl --user daemon-reload failed"
    );
    ensure!(
        run("systemctl", &["--user", "enable", &unit])?,
        "systemctl --user enable failed"
    );
    ensure!(
        run("systemctl", &["--user", "restart", &unit])?,
        "systemctl --user restart failed"
    );
    Ok(())
}

/// Stops the tunnel and deletes its unit or plist; best effort.
fn remove_tunnel(name: &str) {
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid());
        let _ = run(
            "launchctl",
            &["bootout", &format!("{domain}/{}", launchd_label(name))],
        );
    } else {
        let _ = run(
            "systemctl",
            &["--user", "disable", "--now", &unit_file_name(name)],
        );
    }
    let _ = std::fs::remove_file(tunnel_file(name));
    if !cfg!(target_os = "macos") {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
}

fn install_tunnel(t: &Target, name: &str, local: u16, remote_port: u16) -> Result<()> {
    let ssh = find_ssh()?;
    let args = tunnel_args(t, local, remote_port);
    std::fs::create_dir_all(unit_dir())?;
    let text = if cfg!(target_os = "macos") {
        tunnel_plist(name, &ssh, &args)?
    } else {
        tunnel_unit(name, &t.host, &ssh, &args)?
    };
    std::fs::write(tunnel_file(name), text)?;
    start_tunnel(name)
}

/// After a failed setup: a tunnel made for this run goes away; one that was
/// replaced gets its old file and a restart.
fn undo_tunnel(name: &str, previous: Option<&str>) {
    match previous {
        None => remove_tunnel(name),
        Some(text) => {
            if std::fs::write(tunnel_file(name), text).is_err() || start_tunnel(name).is_err() {
                eprintln!(
                    "Could not restore the previous tunnel \"{name}\"; check {}",
                    tunnel_file(name).display()
                );
            }
        }
    }
}

/// Takes the lock that keeps two setups from picking the same port; held
/// until the process exits.
#[cfg(unix)]
fn lock_setup() -> Result<std::fs::File> {
    use std::os::fd::AsRawFd as _;
    let dir = crate::config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ssh-setup.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            bail!("another ssh-setup is running");
        }
        return Err(e).context("locking ssh-setup.lock");
    }
    Ok(file)
}

#[cfg(not(unix))]
fn lock_setup() -> Result<std::fs::File> {
    bail!("ssh-setup needs macOS or Linux on this computer")
}

/// Whether every process listening, in `lsof -F c` (macOS) or `ss -ltnp`
/// (Linux) output, is ssh.
pub fn listeners_are_ssh(out: &str) -> bool {
    let names: Vec<&str> = if out.lines().any(|l| l.starts_with('c')) && !out.contains("users:") {
        out.lines().filter_map(|l| l.strip_prefix('c')).collect()
    } else {
        out.split("(\"")
            .skip(1)
            .filter_map(|r| r.split('"').next())
            .collect()
    };
    !names.is_empty() && names.iter().all(|n| *n == "ssh")
}

/// Refuses to go on unless an ssh process is what listens on the port, so
/// the key never goes to anything else.
fn verify_listener_is_ssh(local: u16) -> Result<()> {
    let (program, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        (
            "lsof",
            vec![
                "-nP".into(),
                format!("-iTCP:{local}"),
                "-sTCP:LISTEN".into(),
                "-Fc".into(),
            ],
        )
    } else {
        ("ss", vec!["-ltnp".into(), format!("sport = :{local}")])
    };
    let out = Command::new(program)
        .args(&args)
        .output()
        .with_context(|| format!("running {program} to check who listens on 127.0.0.1:{local}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    ensure!(
        listeners_are_ssh(&text),
        "could not confirm that ssh is what listens on 127.0.0.1:{local}; not sending the key"
    );
    Ok(())
}

/// Asks the server through the tunnel with the new key, retrying while the
/// tunnel comes up or the remote server finishes starting.
fn check_tunnel(local: u16, key: &str) -> Result<()> {
    let mut verified = false;
    for _ in 0..20 {
        if verify_listener_is_ssh(local).is_ok() {
            verified = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    if !verified {
        verify_listener_is_ssh(local)?;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    let mut last = String::new();
    for _ in 0..20 {
        match agent
            .get(&format!("http://127.0.0.1:{local}/v1/tools"))
            .header("Authorization", &format!("Bearer {key}"))
            .call()
        {
            Ok(_) => return Ok(()),
            Err(ureq::Error::StatusCode(c @ (401 | 403))) => {
                bail!("the server through the tunnel rejected the new key ({c})")
            }
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    bail!("the server did not answer through the tunnel on 127.0.0.1:{local}: {last}")
}

fn is_key(s: &str) -> bool {
    s.strip_prefix("ocu_")
        .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The SSH_AUTH_SOCK the tunnel's ssh gets from the service manager that
/// runs it, which need not be this process's.
fn service_agent_socket() -> Result<Option<String>> {
    if cfg!(target_os = "macos") {
        let out = Command::new("launchctl")
            .args(["getenv", "SSH_AUTH_SOCK"])
            .output()
            .context("running launchctl")?;
        let sock = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Ok(Some(sock).filter(|s| !s.is_empty()));
    }
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .context("running systemctl")?;
    ensure!(
        out.status.success(),
        "no systemd user manager here, and the tunnel runs as a user service"
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("SSH_AUTH_SOCK="))
        .map(String::from))
}

/// Logs in the way the tunnel will: unattended, with the service manager's
/// agent. A key that only this terminal can use then fails here, before the
/// remote changes, rather than leaving a tunnel that never comes up.
fn check_unattended_login(t: &Target) -> Result<()> {
    let sock = service_agent_socket()?;
    let mut cmd = Command::new(find_ssh()?);
    cmd.args(ssh_args(t, Mode::Batch, &["true"]))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    match &sock {
        Some(s) => cmd.env("SSH_AUTH_SOCK", s),
        None => cmd.env_remove("SSH_AUTH_SOCK"),
    };
    let out = cmd.output().context("running ssh")?;
    let (runner, fix) = if cfg!(target_os = "macos") {
        ("a login item", "one loaded in the agent or keychain")
    } else {
        (
            "a systemd user service",
            "an agent made visible to services with `systemctl --user import-environment SSH_AUTH_SOCK`",
        )
    };
    ensure!(
        out.status.success(),
        "the tunnel runs as {runner}, which could not log in to {} on its own{}. Use a key that needs no prompt: one without a passphrase (-i KEYFILE), or {fix}",
        t.host,
        detail(&String::from_utf8_lossy(&out.stderr))
    );
    Ok(())
}

/// What to do about a first login the app could not make, having no
/// terminal for ssh's questions.
fn login_hint(host: &str, stderr: &str) -> Option<String> {
    if stderr.contains("Host key verification failed") || stderr.contains("host key is known") {
        Some(format!(
            "This computer does not know {host}'s host key yet. Run `ssh {host}` in a terminal once to check and accept it, then try again."
        ))
    } else if stderr.contains("Permission denied") {
        Some("The key must log in without a prompt: use one without a passphrase, or one loaded in the agent or keychain.".into())
    } else {
        None
    }
}

/// What a running setup reports as it goes.
#[derive(Debug, Clone)]
pub enum Note {
    /// The step it is starting.
    Step(String),
    /// Something it found out.
    Info(String),
    Warning(String),
}

/// A finished setup: the hosts file at `hosts` reaches `name` at `url`.
#[derive(Debug, Clone)]
pub struct Done {
    pub name: String,
    pub url: String,
    pub hosts: PathBuf,
}

/// The remote already has a device of the setup's name, and replacing it
/// was not asked for.
#[derive(Debug)]
pub struct NameTaken(pub String);

impl std::fmt::Display for NameTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the remote already has a device named \"{}\". Replacing it gives it a new key, and whatever uses its current key stops working",
            self.0
        )
    }
}

impl std::error::Error for NameTaken {}

/// `opencomputeruse ssh-setup`.
pub fn setup(args: &[String]) -> Result<()> {
    ensure!(
        cfg!(unix),
        "ssh-setup needs macOS or Linux on this computer"
    );
    let o = parse_args(args)?;
    let done = connect(&o, true, &mut |note| match note {
        Note::Step(s) => println!("{s}..."),
        Note::Info(s) => println!("{s}"),
        Note::Warning(w) => eprintln!("Warning: {w}"),
    })
    .map_err(|e| match e.downcast_ref::<NameTaken>() {
        Some(taken) => anyhow::anyhow!("{taken}. Pick another --name, or pass --replace"),
        None => e,
    })?;
    println!(
        "Done. {} is in {} at {}, through a tunnel that stays up ({}).",
        done.name,
        done.hosts.display(),
        done.url,
        if cfg!(target_os = "macos") {
            "a LaunchAgent: it starts at login, not at boot"
        } else {
            "a systemd user unit; run `loginctl enable-linger` here to keep it across logouts"
        }
    );
    println!("After the remote reboots, calls fail with resets until its server is back up (a minute or so).");
    Ok(())
}

/// Sets up `o`. With `interactive`, the first connection may ask on this
/// terminal (a new host key, a passphrase); without, as from the app,
/// nothing may ask. Steps and warnings go to `note` as they happen.
pub fn connect(o: &Options, interactive: bool, note: &mut dyn FnMut(Note)) -> Result<Done> {
    ensure!(
        cfg!(unix),
        "ssh-setup needs macOS or Linux on this computer"
    );
    let _lock = lock_setup()?;
    let target = Target::new(&o.host, o.key.as_deref())?;

    note(Note::Step(if interactive {
        format!("Connecting to {} (answer any prompts here)", o.host)
    } else {
        format!("Connecting to {}", o.host)
    }));
    let first = if interactive {
        Mode::Interactive
    } else {
        Mode::Batch
    };
    let (ok, probe) = run_ssh_status(
        &ssh_args(&target, first, &["uname", "-sm"]),
        None,
        interactive,
        false,
    )?;
    if !ok {
        match login_hint(&o.host, &probe.stderr) {
            Some(hint) => bail!("connecting failed{}\n{hint}", detail(&probe.stderr)),
            None => bail!("connecting failed{}", detail(&probe.stderr)),
        }
    }
    note(Note::Info(format!(
        "Remote: {}.",
        parse_probe(&probe.stdout, o.use_installed)?
    )));

    note(Note::Step(
        "Checking that the tunnel can log in on its own".into(),
    ));
    check_unattended_login(&target)?;

    let dir = unit_dir();
    let existing = if cfg!(target_os = "macos") {
        existing_tunnels(&dir, LABEL_PREFIX, ".plist")
    } else {
        existing_tunnels(&dir, UNIT_PREFIX, ".service")
    };
    let local = pick_port(&o.name, &existing, |port| {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    })?;
    let url = format!("http://127.0.0.1:{local}");

    let batch = |remote: &[&str]| ssh_args(&target, Mode::Batch, remote);
    let checked = if o.use_installed {
        note(Note::Step(format!(
            "Checking the opencomputeruse already on the remote (port {})",
            o.remote_port
        )));
        let check = remote_script(&RemoteJob::Check {
            port: o.remote_port,
        })?;
        let out = run_checked(
            &batch(&["sh", "-s"]),
            Some(&check),
            interactive,
            true,
            "the remote check",
        )?;
        let c = parse_check(&out.stdout, &out.stderr, env!("CARGO_PKG_VERSION"))?;
        for w in &c.warnings {
            note(Note::Warning(w.clone()));
        }
        Some(c)
    } else {
        note(Note::Step(format!(
            "Installing opencomputeruse {} on the remote",
            env!("CARGO_PKG_VERSION")
        )));
        let install = remote_script(&RemoteJob::Install {
            version: env!("CARGO_PKG_VERSION"),
            port: REMOTE_PORT,
        })?;
        run_checked(
            &batch(&["sh", "-s"]),
            Some(&install),
            interactive,
            false,
            "the remote install",
        )?;
        None
    };
    let bin = checked.as_ref().map(|c| c.bin.as_str());

    note(Note::Step("Creating the device key".into()));
    let add = remote_script(&RemoteJob::DeviceAdd {
        bin,
        name: &o.name,
        url: &url,
        replace: o.replace,
    })?;
    let (ok, added) = run_ssh_status(&batch(&["sh", "-s"]), Some(&add), interactive, true)?;
    if !ok && added.stderr.contains("ocu-device: exists") {
        return Err(NameTaken(o.name.clone()).into());
    }
    ensure!(
        ok,
        "creating the device key failed{}",
        detail(&added.stderr)
    );
    let key = last_line(&added.stdout).to_string();
    let regenerated = added.stderr.contains("ocu-device: regenerated");
    if regenerated {
        note(Note::Info(format!(
            "The remote device \"{}\" has a new key; its old one no longer works.",
            o.name
        )));
    }
    let previous_tunnel = std::fs::read_to_string(tunnel_file(&o.name)).ok();

    note(Note::Step("Starting the tunnel".into()));
    let finish = || -> Result<PathBuf> {
        ensure!(is_key(&key), "the remote did not return a key");
        install_tunnel(&target, &o.name, local, o.remote_port)?;
        check_tunnel(local, &key)?;
        super::save_host_entry(&skill::named_host_entry(&o.name, &url, &key))
    };
    let hosts = match finish() {
        Ok(path) => path,
        Err(e) => {
            undo_tunnel(&o.name, previous_tunnel.as_deref());
            if regenerated {
                note(Note::Warning(format!(
                    "the remote device \"{}\" got a new key, so the old one in the hosts file no longer works",
                    o.name
                )));
            } else {
                let rm = remote_script(&RemoteJob::DeviceRemove { bin, name: &o.name })?;
                let undone = run_checked(
                    &batch(&["sh", "-s"]),
                    Some(&rm),
                    interactive,
                    true,
                    "removing the new device",
                );
                if let Err(e) = undone {
                    note(Note::Warning(format!(
                        "could not remove the new device \"{}\" on the remote: {e:#}",
                        o.name
                    )));
                }
            }
            return Err(e.context("setup did not finish"));
        }
    };
    Ok(Done {
        name: o.name.clone(),
        url,
        hosts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(key: Option<&str>) -> Target {
        Target::new("me@box.example", key).unwrap()
    }

    #[test]
    fn hosts_are_aliases_or_user_at_host_and_never_options() {
        assert!(validate_host("oma").is_ok());
        assert!(validate_host("box.example").is_ok());
        assert!(validate_host("me@box.example").is_ok());
        assert!(validate_host("me@10.0.0.2").is_ok());
        for bad in [
            "user@-oProxyCommand=x",
            "-oProxyCommand=x@host",
            "-me@host",
            "-oma",
            "oma extra",
            "oma\n",
            "oma;id",
            "oma$(id)",
            "me@host extra",
            "me@ho;st",
            "me@host$(id)",
            "@host",
            "me@",
            "me@a@b",
            "",
        ] {
            assert!(validate_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_are_plain() {
        assert!(validate_name("work-box_1.lan").is_ok());
        for bad in ["", "-x", ".x", "a b", "a/b", "a'b", &"x".repeat(41)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_paths_reject_what_breaks_units_and_plists() {
        assert!(validate_path("/home/me/.ssh/id_ed25519").is_ok());
        for bad in [
            "id_ed25519",
            "/home/me/my key",
            "/home/me/100%",
            "/home/me/$HOME",
            "/home/me/a\"b",
            "/home/me/a'b",
            "/home/me/a\\b",
            "/home/me/a`b",
            "/home/me/<k>",
            "/home/me/a&b",
            "/home/me/a\nb",
            "-i/etc",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?}");
        }
        assert!(Target::new("me@box", Some("/tmp/a b")).is_err());
    }

    #[test]
    fn probe_output() {
        assert!(parse_probe("Linux x86_64\n", false).is_ok());
        assert!(parse_probe("Linux aarch64\n", false).is_ok());
        let mac = parse_probe("Darwin arm64\n", false)
            .unwrap_err()
            .to_string();
        assert!(mac.contains("macOS") && mac.contains("--use-installed"));
        assert!(parse_probe("Linux riscv64\n", false).is_err());
        assert!(parse_probe("", false).is_err());
        assert!(parse_probe("", true).is_err());
    }

    #[test]
    fn connecting_accepts_macos_and_any_linux() {
        assert_eq!(parse_probe("Darwin arm64\n", true).unwrap(), "Darwin arm64");
        assert_eq!(
            parse_probe("Linux riscv64\n", true).unwrap(),
            "Linux riscv64"
        );
        assert!(parse_probe("FreeBSD amd64\n", true).is_err());
    }

    #[test]
    fn ssh_argv_puts_the_host_after_double_dash() {
        let a = ssh_args(&target(Some("/k/id")), Mode::Batch, &["sh", "-s"]);
        assert_eq!(
            a,
            [
                "-o",
                "ConnectTimeout=15",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-i",
                "/k/id",
                "--",
                "me@box.example",
                "sh",
                "-s"
            ]
        );
        let i = ssh_args(&target(None), Mode::Interactive, &["uname", "-sm"]);
        assert!(!i
            .iter()
            .any(|w| w.contains("BatchMode") || w.contains("accept-new")));
        assert_eq!(&i[i.len() - 4..], ["--", "me@box.example", "uname", "-sm"]);
        let alias = Target::new("oma", None).unwrap();
        let a = ssh_args(&alias, Mode::Batch, &["sh", "-s"]);
        assert_eq!(&a[a.len() - 4..], ["--", "oma", "sh", "-s"]);
        let tunnel = tunnel_args(&alias, 18642, 8642);
        assert_eq!(&tunnel[tunnel.len() - 2..], ["--", "oma"]);
    }

    #[test]
    fn tunnel_argv_never_weakens_host_checking() {
        let a = tunnel_args(&target(Some("/k/id")), 18650, 8642);
        let joined = a.join(" ");
        assert!(joined.contains("-L 127.0.0.1:18650:127.0.0.1:8642"));
        for want in [
            "BatchMode=yes",
            "ExitOnForwardFailure=yes",
            "ServerAliveInterval=30",
            "StrictHostKeyChecking=yes",
            "-i /k/id",
        ] {
            assert!(joined.contains(want), "{want}");
        }
        assert!(!joined.contains("StrictHostKeyChecking=no") && !joined.contains("accept-new"));
        assert_eq!(&a[a.len() - 2..], ["--", "me@box.example"]);
    }

    fn sh_syntax_ok(script: &str) -> bool {
        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait().unwrap().success()
    }

    #[test]
    fn remote_scripts_parse_as_sh() {
        let jobs = [
            RemoteJob::Install {
                version: "0.6.0",
                port: 8642,
            },
            RemoteJob::Check { port: 8642 },
            RemoteJob::DeviceAdd {
                bin: None,
                name: "box-1",
                url: "http://127.0.0.1:18642",
                replace: false,
            },
            RemoteJob::DeviceAdd {
                bin: Some("/Users/me/My Apps/it's/opencomputeruse"),
                name: "box-1",
                url: "http://127.0.0.1:18642",
                replace: false,
            },
            RemoteJob::DeviceRemove {
                bin: Some("/opt/ocu"),
                name: "box-1",
            },
        ];
        for job in &jobs {
            let s = remote_script(job).unwrap();
            assert!(sh_syntax_ok(&s), "{s}");
        }
    }

    #[test]
    fn script_values_are_quoted_and_validated() {
        let s = remote_script(&RemoteJob::DeviceAdd {
            bin: None,
            name: "n",
            url: "http://127.0.0.1:1",
            replace: false,
        })
        .unwrap();
        assert!(s.starts_with("OCU_NAME='n'\nOCU_URL='http://127.0.0.1:1'\nOCU_REPLACE=''\n"));
        assert!(remote_script(&RemoteJob::DeviceAdd {
            bin: None,
            name: "n; id",
            url: "http://127.0.0.1:1",
            replace: false,
        })
        .is_err());
        assert!(remote_script(&RemoteJob::DeviceAdd {
            bin: None,
            name: "n",
            url: "http://evil:1",
            replace: false,
        })
        .is_err());
        assert!(remote_script(&RemoteJob::DeviceAdd {
            bin: None,
            name: "n",
            url: "http://127.0.0.1:1';id",
            replace: false,
        })
        .is_err());
        assert!(remote_script(&RemoteJob::Install {
            version: "1;id",
            port: 1
        })
        .is_err());
        assert!(remote_script(&RemoteJob::DeviceRemove {
            bin: None,
            name: "$(id)"
        })
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sh_quote_round_trips() {
        for v in ["plain", "a b", "it's", "$HOME `id` \"q\" \\ ;"] {
            let out = Command::new("sh")
                .args(["-c", &format!("printf %s {}", sh_quote(v))])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), v);
        }
    }

    #[test]
    fn systemd_unit_text() {
        let args = tunnel_args(&target(Some("/k/id")), 18650, 8642);
        let unit = tunnel_unit("box", "me@box.example", "/usr/bin/ssh", &args).unwrap();
        for want in [
            "Restart=always\n",
            "RestartSec=5\n",
            "StartLimitIntervalSec=0\n",
            "WantedBy=default.target\n",
            "ExecStart=\"/usr/bin/ssh\" \"-N\" \"-L\" \"127.0.0.1:18650:127.0.0.1:8642\"",
            "\"--\" \"me@box.example\"\n",
        ] {
            assert!(unit.contains(want), "{want}\n{unit}");
        }
        assert!(!unit.contains("network-online"));
        assert_eq!(systemd_quote(r#"a"b\c%d$e"#), r#""a\"b\\c%%d$$e""#);
        assert!(tunnel_unit("bad name", "h", "/usr/bin/ssh", &[]).is_err());
        assert!(tunnel_unit("n", "h", "ssh", &[]).is_err());
    }

    #[test]
    fn plist_text() {
        let args = tunnel_args(&target(Some("/k/id")), 18650, 8642);
        let plist = tunnel_plist("box", "/usr/bin/ssh", &args).unwrap();
        for want in [
            "<string>com.infrawrench.opencomputeruse.tunnel.box</string>",
            "<string>/usr/bin/ssh</string>",
            "<string>127.0.0.1:18650:127.0.0.1:8642</string>",
            "<key>KeepAlive</key>\n\t<true/>",
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>ThrottleInterval</key>\n\t<integer>10</integer>",
        ] {
            assert!(plist.contains(want), "{want}\n{plist}");
        }
        let escaped = tunnel_plist("box", "/usr/bin/ssh", &["a&b<c>".to_string()]).unwrap();
        assert!(escaped.contains("<string>a&amp;b&lt;c&gt;</string>"));
    }

    #[test]
    fn ports_are_read_back_from_units_and_plists() {
        let args = tunnel_args(&target(None), 18661, 8642);
        let unit = tunnel_unit("a", "me@box.example", "/usr/bin/ssh", &args).unwrap();
        let plist = tunnel_plist("a", "/usr/bin/ssh", &args).unwrap();
        assert_eq!(tunnel_port(&unit), Some(18661));
        assert_eq!(tunnel_port(&plist), Some(18661));
        assert_eq!(tunnel_port("nothing here"), None);
    }

    #[test]
    fn existing_tunnels_are_found_by_file_name() {
        let dir = std::env::temp_dir().join(format!("ocu-tunnels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let args = tunnel_args(&target(None), 18643, 8642);
        std::fs::write(
            dir.join("ocu-tunnel-one.service"),
            tunnel_unit("one", "me@h", "/usr/bin/ssh", &args).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("other.service"), "x").unwrap();
        assert_eq!(
            existing_tunnels(&dir, UNIT_PREFIX, ".service"),
            [("one".to_string(), 18643)]
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(existing_tunnels(&dir, UNIT_PREFIX, ".service").is_empty());
    }

    #[test]
    fn port_choice() {
        let used = [("a".to_string(), 18642), ("b".to_string(), 18643)];
        assert_eq!(pick_port("new", &[], |_| true).unwrap(), 18642);
        assert_eq!(pick_port("new", &used, |_| true).unwrap(), 18644);
        assert_eq!(pick_port("new", &used, |p| p != 18644).unwrap(), 18645);
        assert_eq!(pick_port("b", &used, |_| false).unwrap(), 18643);
        assert!(pick_port("new", &[], |_| false).is_err());
    }

    #[test]
    fn arguments() {
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let p = parse_args(&s(&["me@box", "--name", "box"])).unwrap();
        assert_eq!(
            (p.host.as_str(), p.name.as_str(), p.key),
            ("me@box", "box", None)
        );
        let p = parse_args(&s(&["oma", "--name", "box"])).unwrap();
        assert_eq!(p.host, "oma");
        assert!(!p.replace);
        assert!(
            parse_args(&s(&["oma", "--name", "box", "--replace"]))
                .unwrap()
                .replace
        );
        assert!(parse_args(&s(&["me@box"])).is_err());
        assert!(parse_args(&s(&["--name", "x"])).is_err());
        assert!(parse_args(&s(&["-oProxyCommand=x", "--name", "x"])).is_err());
        assert!(parse_args(&s(&["user@-oProxyCommand=x", "--name", "x"])).is_err());
        assert!(parse_args(&s(&["me@box", "--name", "x", "-i", "/no/such key"])).is_err());
    }

    #[test]
    fn banners_before_output_are_skipped() {
        assert_eq!(last_line("Welcome\n\nocu_ab12\n\n"), "ocu_ab12");
        assert_eq!(last_line(""), "");
        assert!(parse_probe("Last login: x\nLinux x86_64\n", false).is_ok());
    }

    #[test]
    fn only_ssh_may_own_the_tunnel_port() {
        assert!(listeners_are_ssh("p42\ncssh\n"));
        assert!(!listeners_are_ssh("p42\ncnode\n"));
        assert!(!listeners_are_ssh("p42\ncssh\np43\ncnode\n"));
        assert!(!listeners_are_ssh(""));
        let ss = "State Recv-Q Send-Q Local Address:Port Peer\nLISTEN 0 128 127.0.0.1:18642 0.0.0.0:* users:((\"ssh\",pid=9,fd=5))\n";
        assert!(listeners_are_ssh(ss));
        assert!(!listeners_are_ssh(&ss.replace("\"ssh\"", "\"python3\"")));
        assert!(!listeners_are_ssh(
            "LISTEN 0 128 127.0.0.1:18642 0.0.0.0:*\n"
        ));
        let shared = "LISTEN 0 128 127.0.0.1:18642 0.0.0.0:* users:((\"ssh\",pid=1,fd=3),(\"python3\",pid=2,fd=3))\n";
        assert!(!listeners_are_ssh(shared));
    }

    #[test]
    #[cfg(unix)]
    fn an_unusable_ssh_on_path_is_an_error() {
        let base = std::env::temp_dir().join(format!("ocu-path-{}", std::process::id()));
        let (good, spaced) = (base.join("bin"), base.join("a b"));
        for d in [&good, &spaced] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("ssh"), "").unwrap();
        }
        assert!(ssh_from_path(spaced.as_os_str())
            .unwrap_err()
            .to_string()
            .contains("a b"));
        assert_eq!(
            ssh_from_path(good.as_os_str()).unwrap(),
            good.join("ssh").to_string_lossy()
        );
        assert_eq!(
            ssh_from_path(std::ffi::OsStr::new("/nonexistent")).unwrap(),
            "/usr/bin/ssh"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn proc_net_tcp_addresses_are_classified() {
        let line =
            |ip: &str, st: &str, port: &str| format!("0: {ip}:{port} 00000000:0000 {st} 0:0\n");
        let input = [
            line("0100007F", "0A", "21C2"),
            line("00000000000000000000000001000000", "0A", "21C2"),
            line("0000000000000000FFFF00000100007F", "0A", "21C2"),
            line("00000000", "0A", "21C2"),
            line("00000000000000000000000000000000", "0A", "21C2"),
            line("0500000A", "0A", "21C2"),
            line("0000000000000000FFFF000000000000", "0A", "21C2"),
            line("0500000A", "01", "21C2"),
            line("0500000A", "0A", "0050"),
        ]
        .concat();
        let out = Command::new("awk")
            .args(["-v", "hex=21C2", "-v", "port=8642", proc_awk!()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                c.stdin.take().unwrap().write_all(input.as_bytes())?;
                c.wait_with_output()
            })
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "127.0.0.1:8642\n[::1]:8642\n127.0.0.1:8642\n0.0.0.0:8642\n0.0.0.0:8642\nother:8642\nother:8642\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn exposure_check_flags_any_other_listener() {
        let script = remote_script(&RemoteJob::Install {
            version: "0.6.0",
            port: 8642,
        })
        .unwrap();
        let check = script.lines().find(|l| l.starts_with("bad=")).unwrap();
        for (listeners, exposed) in [
            ("127.0.0.1:8642", false),
            ("127.0.0.1:8642\n10.0.0.5:8642", true),
            ("127.0.0.1:8642\n[::ffff:0.0.0.0]:8642", true),
            ("127.0.0.1:8642\n*:8642", true),
            ("127.0.0.1:8642\n0.0.0.0:80", false),
            ("127.0.0.1:8642\n[::1]:8642", true),
        ] {
            let out = Command::new("sh")
                .args([
                    "-c",
                    &format!("OCU_PORT=8642; out=$1; {check}; printf %s \"$bad\""),
                ])
                .arg("sh")
                .arg(listeners)
                .output()
                .unwrap();
            assert_eq!(!out.stdout.is_empty(), exposed, "{listeners}");
        }
    }

    #[test]
    fn connect_flags() {
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let base = ["me@box", "--name", "box"];
        let with = |extra: &[&str]| s(&[&base[..], extra].concat());
        let p = parse_args(&with(&[])).unwrap();
        assert_eq!((p.use_installed, p.remote_port), (false, REMOTE_PORT));
        let p = parse_args(&with(&["--use-installed"])).unwrap();
        assert_eq!((p.use_installed, p.remote_port), (true, REMOTE_PORT));
        let p = parse_args(&with(&["--use-installed", "--remote-port", "9000"])).unwrap();
        assert_eq!(p.remote_port, 9000);
        let p = parse_args(&with(&["--remote-port", "65535", "--use-installed"])).unwrap();
        assert_eq!(p.remote_port, 65535);
        for bad in ["0", "65536", "-1", "x", ""] {
            assert!(
                parse_args(&with(&["--use-installed", "--remote-port", bad])).is_err(),
                "{bad}"
            );
        }
        assert!(parse_args(&with(&["--use-installed", "--remote-port"])).is_err());
        assert!(parse_args(&with(&["--remote-port", "9000"])).is_err());
    }

    #[test]
    fn tunnel_forwards_to_the_remote_port() {
        let a = tunnel_args(&target(None), 18650, 9000);
        assert!(a.contains(&"127.0.0.1:18650:127.0.0.1:9000".to_string()));
        let unit = tunnel_unit("box", "me@box.example", "/usr/bin/ssh", &a).unwrap();
        assert!(unit.contains("\"127.0.0.1:18650:127.0.0.1:9000\""));
        assert_eq!(tunnel_port(&unit), Some(18650));
    }

    #[test]
    fn remote_bin_must_be_one_absolute_line() {
        assert!(validate_remote_bin("/Users/me/My Apps/opencomputeruse").is_ok());
        for bad in ["", "opencomputeruse", "./x", "/a\nb", "/a\rb", "/a\u{0}b"] {
            assert!(validate_remote_bin(bad).is_err(), "{bad:?}");
            assert!(
                remote_script(&RemoteJob::DeviceAdd {
                    bin: Some(bad),
                    name: "n",
                    url: "http://127.0.0.1:1",
                    replace: false,
                })
                .is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn check_output_is_read_from_both_streams() {
        let err = "ocu-version: opencomputeruse 1.2.3\nocu-setup: warning: exposed\n";
        let c = parse_check("banner\n/opt/ocu\n", err, "1.2.3").unwrap();
        assert_eq!(
            c,
            Checked {
                bin: "/opt/ocu".into(),
                warnings: vec!["exposed".into()]
            }
        );
        let c = parse_check("/opt/ocu\n", err, "1.2.4").unwrap();
        assert_eq!(c.warnings.len(), 2);
        assert!(c.warnings[1].contains("1.2.3") && c.warnings[1].contains("1.2.4"));
        assert!(parse_check("relative\n", err, "1.2.3").is_err());
        assert!(parse_check("", err, "1.2.3").is_err());
    }

    /// A temp directory laid out like a remote host: a fake HOME and a first
    /// PATH entry holding fake commands, so nothing real is consulted.
    #[cfg(unix)]
    struct Fake {
        dir: PathBuf,
        home: PathBuf,
        bins: PathBuf,
    }

    #[cfg(unix)]
    impl Fake {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ocu-fake-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self {
                home: dir.join("home"),
                bins: dir.join("bin"),
                dir,
            }
        }

        fn exe(&self, path: &Path, body: &str) {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn bin(&self, name: &str) -> PathBuf {
            self.bins.join(name)
        }

        /// A fake opencomputeruse on PATH whose `device` runs `device`.
        fn ocu(&self, device: &str) {
            self.exe(
                &self.bin("opencomputeruse"),
                &format!(
                    "case \"$1\" in --version) echo 'opencomputeruse 9.9.9' ;; device) {device} ;; esac"
                ),
            );
        }

        /// Fake `uname -s` and `ss -ltn`, whose rows are `listeners`.
        fn linux(&self, listeners: &[&str]) {
            self.exe(&self.bin("uname"), "echo Linux");
            let rows: String = listeners
                .iter()
                .map(|a| format!("LISTEN 0 128 {a} 0.0.0.0:*\n"))
                .collect();
            std::fs::write(
                self.dir.join("ss.txt"),
                format!("State Recv-Q Send-Q Local Address:Port Peer Address:Port\n{rows}"),
            )
            .unwrap();
            self.exe(
                &self.bin("ss"),
                &format!("cat '{}'", self.dir.join("ss.txt").display()),
            );
        }

        /// Fake `uname -s` and `lsof -Fn`, which lists `names`.
        fn mac(&self, names: &[&str]) {
            self.exe(&self.bin("uname"), "echo Darwin");
            let rows: String = names.iter().map(|n| format!("p1\\nn{n}\\n")).collect();
            self.exe(&self.bin("lsof"), &format!("printf '{rows}'"));
        }

        /// Runs `job` as the remote would, as `sh -s` with the fake HOME and
        /// PATH; (success, stdout, stderr).
        fn run(&self, job: &RemoteJob) -> (bool, String, String) {
            let mut child = Command::new("sh")
                .arg("-s")
                .env_clear()
                .env("HOME", &self.home)
                .env("PATH", format!("{}:/usr/bin:/bin", self.bins.display()))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(remote_script(job).unwrap().as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }

        fn check(&self) -> (bool, String, String) {
            self.run(&RemoteJob::Check { port: 8642 })
        }
    }

    #[cfg(unix)]
    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// What `device list` does in a release without the device commands,
    /// and in one with them.
    #[cfg(unix)]
    const OLD: &str = "echo 'opencomputeruse: unknown command \"device\"' >&2; exit 1";
    #[cfg(unix)]
    const NEW: &str = "[ \"$2\" = list ]";

    #[test]
    #[cfg(unix)]
    fn installer_checks_device_support_before_replacing_or_starting() {
        for downloaded in [false, true] {
            for compatible in [false, true] {
                let f = Fake::new("install-capability");
                f.linux(&["127.0.0.1:8642"]);
                f.exe(
                    &f.bin("uname"),
                    "case \"$1\" in -s) echo Linux ;; -m) echo aarch64 ;; esac",
                );
                let effects = f.dir.join("effects");
                let binary = |version: &str, device: &str| {
                    format!(
                        "case \"$1\" in --version) echo 'opencomputeruse {version}' ;; device) {device} ;; serve) echo serve >> '{}' ;; esac",
                        effects.display()
                    )
                };
                let usage = if compatible { NEW } else { OLD };
                let installed = f.home.join(".local/bin/opencomputeruse");
                f.exe(
                    &installed,
                    &binary(if downloaded { "old" } else { "9.9.9" }, usage),
                );
                let original = std::fs::read(&installed).unwrap();
                let release = f.dir.join("release");
                f.exe(&release, &binary("9.9.9", usage));
                f.exe(
                    &f.bin("curl"),
                    &format!("cp '{}' \"$5\"", release.display()),
                );
                for cli in ["systemctl", "loginctl"] {
                    f.exe(
                        &f.bin(cli),
                        &format!("echo {cli} >> '{}'", effects.display()),
                    );
                }
                let (ok, _, err) = f.run(&RemoteJob::Install {
                    version: "9.9.9",
                    port: 8642,
                });
                if compatible {
                    assert!(ok, "{err}");
                    let effects = std::fs::read_to_string(&effects).unwrap();
                    assert_eq!(effects, "systemctl\nloginctl\nserve\n");
                    assert_eq!(
                        std::fs::read(&installed).unwrap(),
                        std::fs::read(&release).unwrap()
                    );
                } else {
                    assert!(!ok && err.contains("too old"), "{err}");
                    assert_eq!(std::fs::read(&installed).unwrap(), original);
                    assert!(!effects.exists());
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn check_finds_a_loopback_listener() {
        for listeners in [
            &["127.0.0.1:8642"][..],
            &["127.0.0.1:8642", "[::1]:8642"],
            &["[::ffff:127.0.0.1]:8642", "0.0.0.0:22"],
        ] {
            let f = Fake::new("found");
            f.ocu(NEW);
            f.linux(listeners);
            let (ok, out, err) = f.check();
            assert!(ok, "{listeners:?}: {err}");
            assert_eq!(out, format!("{}\n", f.bin("opencomputeruse").display()));
            assert!(err.contains("ocu-version: opencomputeruse 9.9.9"), "{err}");
            assert!(!err.contains("warning"), "{err}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn check_reports_a_binary_with_spaces_in_its_path() {
        let mut f = Fake::new("spaces");
        f.bins = f.dir.join("my bin");
        f.ocu(NEW);
        f.linux(&["127.0.0.1:8642"]);
        let (ok, out, err) = f.check();
        assert!(ok, "{err}");
        assert_eq!(out, format!("{}\n", f.bin("opencomputeruse").display()));
    }

    #[test]
    #[cfg(unix)]
    fn check_fails_when_nothing_is_installed() {
        let apps = [
            "/Applications/OpenComputerUse.app/Contents/MacOS/opencomputeruse",
            "/usr/bin/opencomputeruse",
        ];
        if apps.iter().any(|a| Path::new(a).exists()) {
            return; // this machine has a real install the script would find
        }
        let f = Fake::new("none");
        f.linux(&["127.0.0.1:8642"]);
        let (ok, out, err) = f.check();
        assert!(!ok && out.is_empty());
        assert!(err.contains("not installed"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn check_fails_on_a_binary_without_the_device_commands() {
        let f = Fake::new("old");
        f.ocu(OLD);
        f.linux(&["127.0.0.1:8642"]);
        let (ok, _, err) = f.check();
        assert!(!ok && err.contains("too old"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn check_fails_when_nothing_listens() {
        let f = Fake::new("quiet");
        f.ocu(NEW);
        f.linux(&["127.0.0.1:22", "0.0.0.0:80"]);
        let (ok, _, err) = f.check();
        assert!(!ok && err.contains("nothing listens on port 8642"), "{err}");
        assert!(err.contains("turn the HTTP server on"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn check_fails_when_the_listener_is_not_reachable_through_127_0_0_1() {
        for (listeners, shown) in [
            (&["10.0.0.5:8642"][..], "10.0.0.5:8642"),
            (&["[::1]:8642"], "[::1]:8642"),
            (&["10.0.0.5:8642", "[::1]:8642"], "10.0.0.5:8642"),
        ] {
            let f = Fake::new("elsewhere");
            f.ocu(NEW);
            f.linux(listeners);
            let (ok, out, err) = f.check();
            assert!(!ok && out.is_empty(), "{listeners:?}");
            assert!(
                err.contains("listens only on") && err.contains(shown),
                "{err}"
            );
            assert!(err.contains("listen_on"), "{err}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn check_warns_about_wildcard_and_extra_listeners_but_passes() {
        for (listeners, shown) in [
            (&["*:8642"][..], "*:8642"),
            (&["0.0.0.0:8642"], "0.0.0.0:8642"),
            (&["[::]:8642"], "[::]:8642"),
            (&["127.0.0.1:8642", "10.0.0.5:8642"], "10.0.0.5:8642"),
        ] {
            let f = Fake::new("wild");
            f.ocu(NEW);
            f.linux(listeners);
            let (ok, out, err) = f.check();
            assert!(ok && !out.is_empty(), "{listeners:?}: {err}");
            let warning = err
                .lines()
                .find(|l| l.starts_with("ocu-setup: warning:"))
                .unwrap_or_else(|| panic!("no warning for {listeners:?}: {err}"));
            assert!(warning.contains(shown), "{warning}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn check_on_macos_uses_lsof() {
        let f = Fake::new("mac");
        f.ocu(NEW);
        f.mac(&["127.0.0.1:8642"]);
        let (ok, _, err) = f.check();
        assert!(ok && !err.contains("warning"), "{err}");

        f.mac(&["*:8642"]);
        let (ok, _, err) = f.check();
        assert!(ok && err.contains("warning"), "{err}");

        f.mac(&["[::1]:8642"]);
        let (ok, _, err) = f.check();
        assert!(!ok && err.contains("[::1]:8642"), "{err}");

        f.mac(&[]);
        let (ok, _, err) = f.check();
        assert!(!ok && err.contains("cannot verify"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn device_jobs_run_the_located_binary_with_a_quoted_path() {
        let f = Fake::new("device");
        let tricky = f.dir.join("My Apps").join("it's");
        let bin = tricky.join("opencomputeruse");
        f.exe(&bin, "echo \"$@\"");
        let bin = bin.to_string_lossy().into_owned();
        let add = f.run(&RemoteJob::DeviceAdd {
            bin: Some(&bin),
            name: "box",
            url: "http://127.0.0.1:18642",
            replace: false,
        });
        assert_eq!(
            add,
            (
                true,
                "device add --name box --url http://127.0.0.1:18642\n".into(),
                String::new()
            )
        );
        let rm = f.run(&RemoteJob::DeviceRemove {
            bin: Some(&bin),
            name: "box",
        });
        assert_eq!(
            rm,
            (true, "device remove --name box\n".into(), String::new())
        );

        f.exe(
            &f.dir.join("home/.local/bin/opencomputeruse"),
            "echo installed \"$1\"",
        );
        let add = f.run(&RemoteJob::DeviceAdd {
            bin: None,
            name: "box",
            url: "http://127.0.0.1:18642",
            replace: false,
        });
        assert_eq!(add.1, "installed device\n");

        let add = f.run(&RemoteJob::DeviceAdd {
            bin: Some(&bin),
            name: "box",
            url: "http://127.0.0.1:18642",
            replace: true,
        });
        assert_eq!(
            add.1,
            "device add --name box --url http://127.0.0.1:18642 --replace\n"
        );
    }

    #[test]
    fn the_app_gets_a_hint_for_logins_it_cannot_make() {
        let unknown = "No ED25519 host key is known for box and you have requested strict checking.\r\nHost key verification failed.";
        assert!(login_hint("box", unknown).unwrap().contains("`ssh box`"));
        assert!(login_hint("box", "me@box: Permission denied (publickey).")
            .unwrap()
            .contains("without a prompt"));
        assert_eq!(login_hint("box", "Connection refused"), None);
    }

    #[test]
    fn keys_have_a_fixed_shape() {
        assert!(is_key("ocu_0123abcd"));
        assert!(!is_key("ocu_"));
        assert!(!is_key("Warning: something"));
    }
}
