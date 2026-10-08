//! The skill a device installs to drive this computer: a `SKILL.md` holding
//! the URL, the device's key, and how to call every tool over HTTP. It is
//! what generating (or regenerating) a key produces, and the only place the
//! key is ever shown.

use std::fmt::Write as _;

use serde_json::Value;

use super::devices::{computer_name, host_slug, Device};

/// The skill's directory name, which is also its `name`.
pub fn skill_name() -> String {
    format!("computer-{}", host_slug())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One line per parameter: `name` (type, required): description.
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
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const HOSTS_FILE: &str = "~/.config/opencomputeruse/hosts.yaml";

/// The generic skill's directory name, which is also its `name`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const GENERIC_SKILL_NAME: &str = "opencomputeruse-remote";

/// POSIX shell for the generic skill: `ocu_hosts` lists the hosts in the
/// hosts file, `ocu HOST TOOL [JSON]` calls a tool on one. The key is read
/// inside `ocu` and handed to curl on stdin, so it never appears in a
/// command line or in output. `_ocu_entry` is internal: it prints the key.
/// The file is a deliberately small subset of YAML (see the skill text), and
/// a key or URL holding characters that could break out of curl's config
/// line is refused.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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

pub fn render(device: &Device, key: &str, tools: &[Value]) -> String {
    let computer = computer_name();
    let name = skill_name();
    let url = &device.url;
    let mut s = String::new();
    let _ = write!(
        s,
        r#"---
name: {name}
description: Operate desktop apps on {computer}, a separate computer, through its OpenComputerUse HTTP API. Starts apps there in the background, takes screenshots, reads accessibility trees, clicks, types and presses keys. Use when asked to do anything in an app on {computer}.
---

# Using {computer}

{computer} runs OpenComputerUse, which lets this device ("{device_name}") drive apps on it over HTTP. Apps run in the background on {computer}; its user keeps their own screen, pointer and keyboard.

This file contains the device's key. Keep it private. If it leaks, regenerate the key in OpenComputerUse on {computer}.

```sh
OCU_URL={url_q}
OCU_KEY={key_q}
```

## Calling a tool

Every tool is a POST with its arguments as JSON:

```sh
curl -sS -X POST "$OCU_URL/v1/tools/start_session" \
  -H "Authorization: Bearer $OCU_KEY" -H "Content-Type: application/json" \
  -d '{{"app": "TextEdit"}}'
```

The response is `{{"content": [...], "isError": false}}`. `content` holds a `text` item, and for actions and screenshots an `image` item: a base64 PNG of the app's window. To look at it, save it and open the file:

```sh
curl -sS -X POST "$OCU_URL/v1/tools/screenshot" \
  -H "Authorization: Bearer $OCU_KEY" -H "Content-Type: application/json" \
  -d '{{"session_id": "SESSION_ID"}}' \
  | tee /tmp/ocu-response.json | jq -r '.content[] | select(.type == "text") | .text'
jq -r '.content[] | select(.type == "image") | .data' /tmp/ocu-response.json | base64 --decode > /tmp/ocu-screen.png
```

`GET $OCU_URL/v1/tools` lists the tools with their JSON schemas.

{how_to_work}
## As an MCP server instead

Clients that speak MCP over HTTP can use the same key:

```sh
claude mcp add --transport http {name} {url_q}/mcp --header "Authorization: Bearer $OCU_KEY"
```

## Tools

"#,
        how_to_work = HOW_TO_WORK,
        name = name,
        computer = computer,
        device_name = device.name,
        url_q = shell_quote(url),
        key_q = shell_quote(key),
    );
    s.push_str(&tools_section(tools));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_carries_url_key_and_tools() {
        let device = Device {
            id: "a".into(),
            name: "Laptop".into(),
            url: "http://x.ts.net:8642".into(),
            key_hash: String::new(),
            created: 0,
        };
        let tools = vec![serde_json::json!({
            "name": "click", "description": "Click.",
            "inputSchema": {"type": "object", "properties": {"x": {"type": "number", "description": "Across."}}, "required": ["x"]}
        })];
        let s = render(&device, "ocu_secret", &tools);
        assert!(s.starts_with("---\nname: computer-"));
        assert!(s.contains("OCU_KEY='ocu_secret'"));
        assert!(s.contains("OCU_URL='http://x.ts.net:8642'"));
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
    /// file: (exit code, stdout, stderr).
    fn sh(name: &str, hosts: &str, script: &str) -> (i32, String, String) {
        let dir = std::env::temp_dir().join(format!("ocu-skill-test-{}-{name}", std::process::id()));
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

    #[test]
    fn the_shell_helper_reads_the_hosts_file() {
        let (code, out, _) = sh("entry", HOSTS, "_ocu_entry studio");
        assert_eq!((code, out.as_str()), (0, "https://studio.example.com\tocu_BBB-222\n"));
        // The last entry of a repeated name wins.
        let (_, out, _) = sh("dup", HOSTS, "_ocu_entry work-mac");
        assert_eq!(out, "http://work-mac.ts.net:9000\tocu_CCC333\n");
        // Windows line endings.
        let crlf = HOSTS.replace('\n', "\r\n");
        let (code, out, _) = sh("crlf", &crlf, "_ocu_entry studio");
        assert_eq!((code, out.as_str()), (0, "https://studio.example.com\tocu_BBB-222\n"));
    }

    #[test]
    fn listing_hosts_never_shows_a_key() {
        let (code, out, _) = sh("list", HOSTS, "ocu_hosts");
        assert_eq!(code, 0);
        assert_eq!(out, "work-mac\thttp://work-mac.ts.net:9000\nstudio\thttps://studio.example.com\n");
        assert!(!out.contains("ocu_"));
    }

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
}
