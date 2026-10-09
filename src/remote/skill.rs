//! What a device needs to drive this computer: the generic skill (how to
//! call every tool over HTTP, the same for every computer), this computer's
//! entry for the device's hosts file (its URL and the device's key), and a
//! prompt holding both for the device's agent to set up. Generating (or
//! regenerating) a key produces them, and is the only time the key is shown.

use std::fmt::Write as _;

use serde_json::Value;

use super::devices::{computer_name, host_slug, Device};

fn params(schema: &Value) -> String {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let mut out = String::new();
    for (name, p) in props {
        let ty = p.get("type").and_then(Value::as_str).unwrap_or("any");
        let req = if required.contains(&name.as_str()) {
            ", required"
        } else {
            ""
        };
        let _ = write!(out, "  - `{name}` ({ty}{req})");
        if let Some(d) = p.get("description").and_then(Value::as_str) {
            let _ = write!(out, ": {d}");
        }
        if let Some(e) = p.get("enum").and_then(Value::as_array) {
            let vals: Vec<String> = e
                .iter()
                .filter_map(Value::as_str)
                .map(|v| format!("`{v}`"))
                .collect();
            let _ = write!(out, " One of {}.", vals.join(", "));
        }
        out.push('\n');
    }
    out
}

const HOW_TO_WORK: &str = "## How to work

1. `start_session` with the app (a name like \"Safari\", a bundle id, or a path) and keep the `session_id`.
2. Look with `screenshot` or `get_ui_tree`. Coordinates are points from the window's top-left, on the screenshot's pixel grid.
3. Act with `click`, `type_text`, `press_key`, `scroll` and the rest. Each action returns a fresh screenshot unless you pass `\"screenshot\": false`. Prefer element ids from the tree (`{\"element\": \"e12\"}`) over coordinates.
4. `end_session` when finished. Sessions also end if this device's key is regenerated or removed.
";

/// "### `tool`" blocks with each tool's description and parameters.
fn tools_section(tools: &[Value]) -> String {
    let mut s = String::new();
    for t in tools {
        let tname = t.get("name").and_then(Value::as_str).unwrap_or_default();
        let desc = t
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let _ = write!(s, "### `{tname}`\n\n{desc}\n\n");
        let p = params(t.get("inputSchema").unwrap_or(&Value::Null));
        if !p.is_empty() {
            let _ = writeln!(s, "{p}");
        }
    }
    s
}

/// Where the generic skill reads host names, URLs and keys from.
pub const HOSTS_FILE: &str = "~/.config/opencomputeruse/hosts.yaml";

/// The generic skill's directory name, which is also its `name`.
pub const GENERIC_SKILL_NAME: &str = "opencomputeruse-remote";

/// POSIX shell for the generic skill: `ocu_hosts` lists the hosts in the
/// hosts file, `ocu HOST TOOL [JSON]` calls a tool on one. The key is read
/// inside `ocu` and handed to curl on stdin, so it never appears in a
/// command line or in output. `_ocu_entry` is internal: it prints the key.
/// The file is a deliberately small subset of YAML (see the skill text), and
/// a key or URL holding characters that could break out of curl's config
/// line is refused.
pub const OCU_SH: &str = r##"OCU_HOSTS="${OCU_HOSTS:-$HOME/.config/opencomputeruse/hosts.yaml}"
_ocu_entry() (
  awk -v host="${1-}" -v file="$OCU_HOSTS" '
    function unq(v,   q) {
      sub(/^[ \t]+/, "", v)
      q = substr(v, 1, 1)
      if (q == "\"" || q == "\047") { v = substr(v, 2); sub(q ".*$", "", v); return v }
      sub(/[ \t]+#.*$/, "", v); sub(/[ \t]+$/, "", v); return v
    }
    { sub(/\r$/, "") }
    /^[ \t]*(#|$)/ { next }
    /^hosts:[ \t]*(#.*)?$/ { inh = 1; hi = -1; next }
    /^[^ \t]/ { inh = 0; next }
    inh {
      match($0, /^[ \t]+/); ind = RLENGTH
      line = substr($0, ind + 1)
      if (hi < 0) hi = ind
      if (ind == hi) {
        name = line; sub(/:[ \t]*(#.*)?$/, "", name); name = unq(name)
        cur = name
        if (!(cur in seen)) { order[++n] = cur; seen[cur] = 1 }
        u[cur] = ""; k[cur] = ""
      } else if (cur != "") {
        f = line; sub(/:.*$/, "", f)
        v = line; sub(/^[^:]*:/, "", v)
        if (f == "url") u[cur] = unq(v)
        if (f == "key") k[cur] = unq(v)
      }
    }
    END {
      if (host == "") { for (i = 1; i <= n; i++) print order[i] "\t" u[order[i]]; exit 0 }
      if (!(host in seen)) { print "ocu: no host \"" host "\" in " file > "/dev/stderr"; exit 1 }
      if (u[host] !~ /^https?:\/\// || u[host] ~ /[ \t"\\\047]/ || k[host] !~ /^[A-Za-z0-9._~+\/=-]+$/) {
        print "ocu: host \"" host "\" needs a plain http(s) url and a key of letters, digits and ._~+/=-" > "/dev/stderr"; exit 1
      }
      printf "%s\t%s\n", u[host], k[host]
    }' "$OCU_HOSTS"
)
ocu_hosts() ( _ocu_entry )
ocu() (
  set -eu
  : "${2:?usage: ocu HOST TOOL [JSON]}"
  entry=$(_ocu_entry "${1:?usage: ocu HOST TOOL [JSON]}")
  url=${entry%"$(printf '\t')"*}
  key=${entry#*"$(printf '\t')"}
  body=${3-}; [ -n "$body" ] || body='{}'
  printf 'header = "Authorization: Bearer %s"\n' "$key" |
    curl -sS -K - -X POST "$url/v1/tools/$2" -H 'Content-Type: application/json' --data-binary "$body"
)
"##;

/// The skill that works for every computer in the hosts file: no key in it.
/// Deployed to the user's agent harnesses by the settings window.
pub fn render_generic(tools: &[Value]) -> String {
    let mut s = format!(
        r#"---
name: {GENERIC_SKILL_NAME}
description: Operate desktop apps on another computer that runs OpenComputerUse, through its HTTP API, using the host names, URLs and keys kept in {HOSTS_FILE}. Starts apps there in the background, takes screenshots, reads accessibility trees, clicks, types and presses keys. Use when asked to do anything in an app on a remote computer.
---

# Driving a remote computer

Other computers run OpenComputerUse, which lets this device drive their apps over HTTP. Apps run in the background there; the computer's user keeps their own screen, pointer and keyboard. Each computer is a host in `{HOSTS_FILE}`, so one file holds the keys for several.

## The hosts file

This file holds keys. Never print, cat, grep or `set -x` it, and never put a key in a command line, a message or a file. Use the functions below, which read the key themselves. If it is missing, ask the user to create it (the OpenComputerUse app's Generate Skill gives them the entry):

```yaml
hosts:
  work-mac:
    url: "http://work-mac.example.ts.net:8642"
    key: "ocu_..."
  studio:
    url: "http://studio.example.ts.net:8642"
    key: "ocu_..."
```

It is a small subset of YAML: a top-level `hosts:` map; each host a name with indented `url:` and `key:` values, bare or in single or double quotes (no escape sequences); `#` comments; later entries of the same name win. Keys may hold only letters, digits and `._~+/=-`; URLs must be plain `http://` or `https://`. Create it with `umask 077; mkdir -p ~/.config/opencomputeruse` and `chmod 600` on the file. It needs a POSIX shell, awk and curl (not available on Windows).

## Setting up the shell

Each shell command starts fresh, so save this once, with the folder made as above, to `~/.config/opencomputeruse/ocu.sh` (it holds no secrets, and a private folder keeps anyone else from swapping it) and begin every command that talks to a computer with `. ~/.config/opencomputeruse/ocu.sh`:

```sh
{OCU_SH}```

- `ocu_hosts` lists the hosts with their URLs. Run it to see which ones exist, and ask the user which to use when it is not clear.
- `ocu HOST TOOL [JSON]` calls one tool on HOST and prints the response. Never call `_ocu_entry`: it prints the key.

## Calling a tool

```sh
. ~/.config/opencomputeruse/ocu.sh
ocu work-mac start_session '{{"app": "TextEdit"}}'
```

The response is `{{"content": [...], "isError": false}}`. `content` holds a `text` item, and for actions and screenshots an `image` item: a base64 PNG of the app's window. To look at it, save it and open the file:

```sh
. ~/.config/opencomputeruse/ocu.sh
ocu work-mac screenshot '{{"session_id": "SESSION_ID"}}' > /tmp/ocu-response.json
jq -r '.content[] | select(.type == "text") | .text' /tmp/ocu-response.json
jq -r '.content[] | select(.type == "image") | .data' /tmp/ocu-response.json | base64 --decode > /tmp/ocu-screen.png
```

For arguments with apostrophes or quotes, pass the JSON as `"$(cat <<'EOF'
{{"text": "it's here"}}
EOF
)"`.

{HOW_TO_WORK}
## Tools

Not every host offers every tool. A computer offers what its system has: `unlock_screen` only on a Mac; the phone, iOS simulator and Android emulator tools only when they are turned on in its settings (the simulator ones on a Mac, the emulator ones when the emulator is installed); and `run_recipe` once it is set up. A phone running the OpenComputerUse app offers only the session tools, for apps on that phone. A host answers a tool it doesn't have with an error.

"#
    );
    s.push_str(&tools_section(tools));
    s
}

/// What to add under `hosts:` in the hosts file of the device that will
/// drive this computer: the computer's name, where to reach it, and the key.
pub fn host_entry(device: &Device, key: &str) -> String {
    format!(
        "  {}:\n    url: \"{}\"\n    key: \"{key}\"\n",
        host_slug(),
        device.url
    )
}

/// The hosts file on this computer, from `HOSTS_FILE`.
pub fn hosts_file_path() -> std::path::PathBuf {
    crate::clients::home().join(HOSTS_FILE.trim_start_matches("~/"))
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty() || t.starts_with('#')
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The host a line of the hosts section names, if it is an entry's name line.
fn entry_name(line: &str) -> &str {
    let name = line.trim().split(':').next().unwrap_or_default().trim();
    name.trim_matches(|c| c == '"' || c == '\'')
}

/// Puts `entry` (from `host_entry`) into the text of a hosts file, under
/// `hosts:`, replacing an entry of the same name and leaving the rest as it
/// is. A missing `hosts:` line is added. The entry takes the indentation the
/// file's other entries use, which the shell helper's parser relies on.
pub fn merge_host_entry(existing: &str, entry: &str) -> anyhow::Result<String> {
    let name = entry_name(entry.lines().next().unwrap_or_default());
    anyhow::ensure!(!name.is_empty(), "the entry has no host name");
    let lines: Vec<&str> = existing.lines().collect();
    let is_hosts = |l: &&str| l.starts_with("hosts:");
    let Some(at) = lines.iter().position(is_hosts) else {
        let mut out = existing.trim_end().to_string();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("hosts:\n");
        out.push_str(entry);
        return Ok(out);
    };
    anyhow::ensure!(
        is_blank_or_comment(&lines[at]["hosts:".len()..]),
        "the hosts file's `hosts:` must be a block map, not an inline value"
    );
    let end = lines[at + 1..]
        .iter()
        .position(|l| !is_blank_or_comment(l) && indent(l) == 0)
        .map_or(lines.len(), |i| at + 1 + i);
    let section = &lines[at + 1..end];
    let first = section.iter().find(|l| !is_blank_or_comment(l));
    let hi = first.map_or(2, |l| indent(l));
    let pad = first.map_or("  ", |l| &l[..hi]);

    let mut kept: Vec<String> = Vec::new();
    let mut skipping = false;
    for l in section {
        // The replaced entry's own lines, and comments inside it, go with it.
        skipping = if is_blank_or_comment(l) {
            skipping && indent(l) > hi
        } else {
            indent(l) == hi && entry_name(l) == name || (skipping && indent(l) > hi)
        };
        if !skipping {
            kept.push((*l).to_string());
        }
    }
    // Blank lines and comments that follow the last entry belong to what
    // comes after the section, so the new entry goes above them.
    let mut trail = Vec::new();
    while kept
        .last()
        .is_some_and(|l| is_blank_or_comment(l) && indent(l) == 0)
    {
        trail.push(kept.pop().unwrap_or_default());
    }
    trail.reverse();
    for l in entry.lines() {
        let rest = l.strip_prefix("  ").unwrap_or(l);
        kept.push(format!("{pad}{rest}"));
    }
    kept.extend(trail);

    let mut out: Vec<String> = lines[..=at].iter().map(|l| (*l).to_string()).collect();
    out.extend(kept);
    out.extend(lines[end..].iter().map(|l| (*l).to_string()));
    Ok(out.join("\n") + "\n")
}

/// A message to paste into the device's agent, which then sets itself up
/// to drive this computer: it adds this computer's `entry`, with the key,
/// to the hosts file, and installs the generic `skill`. The entry comes
/// first, where it is seen without scrolling past the skill.
pub fn agent_prompt(skill: &str, entry: &str) -> String {
    let computer = computer_name();
    format!(
        r#"Set yourself up to drive apps on {computer}, another computer, through its OpenComputerUse HTTP API. Two steps:

1. Add the lines between BEGIN HOSTS and END HOSTS to {HOSTS_FILE}, under its `hosts:` line. If the file doesn't exist, create it with `hosts:` as its first line. If it already has an entry with the same name, replace that entry. The entry holds this computer's key, so make the file readable by this user only (chmod 600 on macOS and Linux) and don't repeat the key back to me.
2. Save everything between the BEGIN SKILL and END SKILL lines, exactly as it is, as `{GENERIC_SKILL_NAME}/SKILL.md` in your skills folder, replacing any older copy:
   - Claude Code: ~/.claude/skills/{GENERIC_SKILL_NAME}/SKILL.md
   - Codex: ~/.codex/skills/{GENERIC_SKILL_NAME}/SKILL.md
   - OpenCode: ~/.config/opencode/skills/{GENERIC_SKILL_NAME}/SKILL.md
   - Anything else: wherever you keep skills, or your instructions file if you have no skills.

Then tell me what you changed, and that a new session picks the skill up.

-----BEGIN HOSTS-----
{entry}-----END HOSTS-----

-----BEGIN SKILL-----
{skill}-----END SKILL-----
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generic_skill_lists_the_tools() {
        let tools = vec![serde_json::json!({
            "name": "click", "description": "Click.",
            "inputSchema": {"type": "object", "properties": {"x": {"type": "number", "description": "Across."}}, "required": ["x"]}
        })];
        let s = render_generic(&tools);
        assert!(s.contains("### `click`") && s.contains("`x` (number, required): Across."));
    }

    fn device(name: &str, url: &str) -> Device {
        Device {
            id: "a".into(),
            name: name.into(),
            url: url.into(),
            key_hash: String::new(),
            created: 0,
        }
    }

    /// The Android app ships the skill as a file; it must be this one.
    #[test]
    fn the_android_app_ships_this_skill() {
        let shipped = include_str!("../../android/res/raw/skill.md");
        assert!(
            shipped == render_generic(&crate::tools::catalog()),
            "android/res/raw/skill.md is out of date: regenerate it with \
             `cargo run -- skill > android/res/raw/skill.md`"
        );
    }

    #[test]
    fn the_generic_skill_has_no_key_and_names_the_hosts_file() {
        let s = render_generic(&[]);
        assert!(s.starts_with("---\nname: opencomputeruse-remote\n"));
        assert!(s.contains("~/.config/opencomputeruse/hosts.yaml"));
        assert!(s.contains("ocu_hosts()") && s.contains("curl -sS -K -"));
        assert!(!s.contains("OCU_KEY"));
        // The helper lives in the private config folder, not in /tmp.
        assert!(s.contains(". ~/.config/opencomputeruse/ocu.sh"));
        assert!(!s.contains("TMPDIR") && !s.contains("{{"));
    }

    #[test]
    fn the_agent_prompt_carries_the_skill_and_the_entry() {
        let entry = host_entry(&device("Laptop", "http://x.ts.net:8642"), "ocu_k");
        let p = agent_prompt("---\nname: x\n---\n", &entry);
        assert!(p.contains("-----BEGIN SKILL-----\n---\nname: x\n---\n-----END SKILL-----\n"));
        assert!(p.contains(&format!(
            "-----BEGIN HOSTS-----\n{entry}-----END HOSTS-----\n"
        )));
        assert!(p.contains("~/.claude/skills/opencomputeruse-remote/SKILL.md"));
        assert!(p.contains(HOSTS_FILE));
        assert!(p.find("BEGIN HOSTS") < p.find("BEGIN SKILL"));
    }

    #[test]
    fn the_host_entry_names_the_driven_computer() {
        let e = host_entry(&device("Laptop", "http://x.ts.net:8642"), "ocu_k");
        assert_eq!(
            e,
            format!(
                "  {}:\n    url: \"http://x.ts.net:8642\"\n    key: \"ocu_k\"\n",
                host_slug()
            )
        );
        assert!(!e.contains("Laptop"));
    }

    /// Runs `script` in `sh` with the helpers loaded and `hosts` as the hosts
    /// file: (exit code, stdout, stderr). Unix only: the helper is a POSIX
    /// script the agent sources in its own shell, and this drives it with a
    /// Unix path, which a Windows `sh` would not read.
    #[cfg(unix)]
    fn sh(name: &str, hosts: &str, script: &str) -> (i32, String, String) {
        let dir =
            std::env::temp_dir().join(format!("ocu-skill-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ocu.sh"), OCU_SH).unwrap();
        std::fs::write(dir.join("hosts.yaml"), hosts).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(". {}/ocu.sh; {script}", dir.display()))
            .env("OCU_HOSTS", dir.join("hosts.yaml"))
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    const HOSTS: &str = "# my computers\nhosts:\n  work-mac:   # the office one\n    url: \"http://work-mac.ts.net:8642\"\n    key: ocu_AAA111\n  \"studio\":\n    url: 'https://studio.example.com'\n    key: 'ocu_BBB-222'  # rotated\n  work-mac:\n    url: http://work-mac.ts.net:9000\n    key: ocu_CCC333\nother: 1\n";

    #[cfg(unix)]
    #[test]
    fn the_shell_helper_reads_the_hosts_file() {
        let (code, out, _) = sh("entry", HOSTS, "_ocu_entry studio");
        assert_eq!(
            (code, out.as_str()),
            (0, "https://studio.example.com\tocu_BBB-222\n")
        );
        // The last entry of a repeated name wins.
        let (_, out, _) = sh("dup", HOSTS, "_ocu_entry work-mac");
        assert_eq!(out, "http://work-mac.ts.net:9000\tocu_CCC333\n");
        // Windows line endings.
        let crlf = HOSTS.replace('\n', "\r\n");
        let (code, out, _) = sh("crlf", &crlf, "_ocu_entry studio");
        assert_eq!(
            (code, out.as_str()),
            (0, "https://studio.example.com\tocu_BBB-222\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn listing_hosts_never_shows_a_key() {
        let (code, out, _) = sh("list", HOSTS, "ocu_hosts");
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "work-mac\thttp://work-mac.ts.net:9000\nstudio\thttps://studio.example.com\n"
        );
        assert!(!out.contains("ocu_"));
    }

    #[cfg(unix)]
    #[test]
    fn the_shell_helper_refuses_unknown_and_unsafe_hosts() {
        let (code, out, err) = sh("missing", HOSTS, "_ocu_entry nowhere");
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(err.contains("no host \"nowhere\""));
        // Quotes, spaces or newlines could add options to curl's config line.
        for bad in [
            "key: 'a\"b'\n    url: http://x",
            "key: 'a b'\n    url: http://x",
            "key: ok\n    url: 'http://x\"y'",
            "key: ok\n    url: ftp://x",
            "url: http://x",
        ] {
            let hosts = format!("hosts:\n  h:\n    {bad}\n");
            let (code, out, _) = sh("bad", &hosts, "_ocu_entry h");
            assert_eq!((code, out.as_str()), (1, ""), "{bad}");
        }
    }

    #[test]
    fn merging_a_host_entry_replaces_only_that_host() {
        let new = "  work-mac:\n    url: \"http://n:1\"\n    key: \"ocu_NEW\"\n";
        let merged = merge_host_entry(HOSTS, new).unwrap();
        // The old entries of that name are gone; the others and the rest stay.
        assert_eq!(merged.matches("work-mac:").count(), 1);
        assert!(!merged.contains("ocu_AAA111") && !merged.contains("ocu_CCC333"));
        assert!(merged.contains("ocu_BBB-222") && merged.contains("other: 1"));
        assert!(merged.contains("# my computers"));
        // And the shell helper reads the result.
        #[cfg(unix)]
        {
            let (code, out, _) = sh("merged", &merged, "_ocu_entry work-mac");
            assert_eq!((code, out.as_str()), (0, "http://n:1\tocu_NEW\n"));
            let (_, out, _) = sh("merged2", &merged, "_ocu_entry studio");
            assert_eq!(out, "https://studio.example.com\tocu_BBB-222\n");
        }
    }

    #[test]
    fn merging_creates_and_follows_the_file_layout() {
        let e = "  m:\n    url: \"http://x\"\n    key: \"k\"\n";
        assert_eq!(merge_host_entry("", e).unwrap(), format!("hosts:\n{e}"));
        assert_eq!(
            merge_host_entry("other: 1\n", e).unwrap(),
            format!("other: 1\nhosts:\n{e}")
        );
        // A file indented by four gets an entry indented by four.
        let four = "hosts:\n    a:\n        url: http://a\n        key: ka\n";
        let merged = merge_host_entry(four, e).unwrap();
        assert!(merged.ends_with("    m:\n      url: \"http://x\"\n      key: \"k\"\n"));
        #[cfg(unix)]
        {
            let (code, out, _) = sh("four", &merged, "_ocu_entry m");
            assert_eq!((code, out.as_str()), (0, "http://x\tk\n"));
        }
    }

    #[test]
    fn merging_keeps_comments_and_refuses_an_inline_hosts_value() {
        let e = "  m:\n    url: \"http://x\"\n    key: \"k\"\n";
        let file = "hosts:\n  m:\n    url: http://o\n    key: OLD\n\n  # about b\n  b:\n    url: http://b\n    key: KB\n\n# settings\nother: 1\n";
        let merged = merge_host_entry(file, e).unwrap();
        assert!(merged.contains("  # about b\n  b:"));
        assert!(merged.contains("key: \"k\"\n\n# settings\nother: 1\n"));
        #[cfg(unix)]
        {
            let (_, out, _) = sh("kept", &merged, "_ocu_entry b");
            assert_eq!(out, "http://b\tKB\n");
        }
        assert!(merge_host_entry("hosts: {}\n", e).is_err());
        // A tab-indented file keeps its tabs.
        let tabbed = merge_host_entry("hosts:\n\ta:\n\t\turl: http://a\n\t\tkey: ka\n", e).unwrap();
        assert!(tabbed.contains("\n\tm:\n"));
    }
}
