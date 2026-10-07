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

/// One line per parameter: `name` (type, required) — description.
fn params(schema: &Value) -> String {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else { return String::new() };
    let mut out = String::new();
    for (name, p) in props {
        let ty = p.get("type").and_then(Value::as_str).unwrap_or("any");
        let req = if required.contains(&name.as_str()) { ", required" } else { "" };
        let _ = write!(out, "  - `{name}` ({ty}{req})");
        if let Some(d) = p.get("description").and_then(Value::as_str) {
            let _ = write!(out, ": {d}");
        }
        if let Some(e) = p.get("enum").and_then(Value::as_array) {
            let vals: Vec<String> = e.iter().filter_map(Value::as_str).map(|v| format!("`{v}`")).collect();
            let _ = write!(out, " One of {}.", vals.join(", "));
        }
        out.push('\n');
    }
    out
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

## How to work

1. `start_session` with the app (a name like "Safari", a bundle id, or a path) and keep the `session_id`.
2. Look with `screenshot` or `get_ui_tree`. Coordinates are points from the window's top-left, on the screenshot's pixel grid.
3. Act with `click`, `type_text`, `press_key`, `scroll` and the rest. Each action returns a fresh screenshot unless you pass `"screenshot": false`. Prefer element ids from the tree (`{{"element": "e12"}}`) over coordinates.
4. `end_session` when finished. Sessions also end if this device's key is regenerated or removed.

## As an MCP server instead

Clients that speak MCP over HTTP can use the same key:

```sh
claude mcp add --transport http {name} {url_q}/mcp --header "Authorization: Bearer $OCU_KEY"
```

## Tools

"#,
        name = name,
        computer = computer,
        device_name = device.name,
        url_q = shell_quote(url),
        key_q = shell_quote(key),
    );
    for t in tools {
        let tname = t.get("name").and_then(Value::as_str).unwrap_or_default();
        let desc = t.get("description").and_then(Value::as_str).unwrap_or_default();
        let _ = write!(s, "### `{tname}`\n\n{desc}\n\n");
        let p = params(t.get("inputSchema").unwrap_or(&Value::Null));
        if !p.is_empty() {
            let _ = write!(s, "{p}\n");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_carries_url_key_and_tools() {
        let device = Device { id: "a".into(), name: "Laptop".into(), url: "http://x.ts.net:8642".into(), key_hash: String::new(), created: 0 };
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
}
