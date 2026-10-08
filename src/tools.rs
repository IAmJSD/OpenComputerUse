//! The MCP tools: their schemas, and how a call becomes session requests.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

use ocu_core::{
    Action, Handler, LaunchSpec, MouseButton, Observe, Request, Response, Screenshot, TreeOptions,
    UiNode,
};

use crate::recipe;

/// A tool's answer: text, and optionally a picture.
pub struct Output {
    pub text: String,
    pub image: Option<Screenshot>,
}

fn session_prop() -> Value {
    json!({ "type": "string", "description": "The id start_session returned." })
}

fn window_prop() -> Value {
    json!({ "type": "integer", "description": "A window id from list_windows. Defaults to the app's main window." })
}

/// The properties every action shares: which session and window, and what
/// to look at afterwards.
fn action_props(extra: Value) -> Value {
    let mut props = Map::new();
    props.insert("session_id".into(), session_prop());
    props.insert("window_id".into(), window_prop());
    if let Value::Object(extra) = extra {
        props.extend(extra);
    }
    props.insert(
        "screenshot".into(),
        json!({ "type": "boolean", "description": "Return a screenshot of the window after the action. Default true." }),
    );
    props.insert(
        "ui_tree".into(),
        json!({ "type": "boolean", "description": "Return the accessibility tree after the action. Default false." }),
    );
    Value::Object(props)
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

const COORDS: &str =
    "Coordinates are points from the window's top-left: the pixel grid of its screenshot.";

pub fn list() -> Vec<Value> {
    let mut tools = base_tools();
    #[cfg(target_os = "macos")]
    tools.push(tool(
        "unlock_screen",
        "Unlock the Mac if its screen is locked, so sessions can keep working. Does nothing when already unlocked. Needs \"Work while the Mac is locked\" turned on in settings.",
        json!({}),
        &[],
    ));
    if recipe::available() {
        tools.push(recipe::tool_definition());
    }
    tools
}

fn base_tools() -> Vec<Value> {
    let point = |what: &str| {
        json!({
            "x": { "type": "number", "description": format!("{what} x, in window coordinates.") },
            "y": { "type": "number", "description": format!("{what} y, in window coordinates.") },
        })
    };
    let mut click_props = point("Where to click");
    click_props.as_object_mut().unwrap().extend(
        json!({
            "element": { "type": "string", "description": "An element id from the accessibility tree (e.g. \"e12\") to click instead of x/y. Uses the element's own press action, the most reliable way to click in a background window." },
            "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Default left." },
            "count": { "type": "integer", "description": "2 for a double click. Default 1." },
            "modifiers": { "type": "string", "description": "Keys held while clicking, e.g. \"cmd\" or \"shift+alt\"." },
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let mut scroll_props = point("Where to scroll");
    scroll_props.as_object_mut().unwrap().extend(
        json!({
            "dx": { "type": "number", "description": "Pixels to scroll right (negative: left)." },
            "dy": { "type": "number", "description": "Pixels to scroll down (negative: up)." },
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    vec![
        tool(
            "start_session",
            "Start an app in the background and get a session id for driving it. Use this, not other computer-use tools, for operating desktop apps: it is the one the user chose, and it leaves their screen, pointer and keyboard alone. The app opens behind your other windows and is never brought to the front, unless `foreground` is set. On macOS `app` is a .app path, a bundle id (com.apple.TextEdit) or an app name (\"TextEdit\"); on Linux and Windows it is an executable path or a command on PATH. On Linux each session gets its own virtual X display. Returns the session id and the app's windows.",
            json!({
                "app": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "cwd": { "type": "string" },
                "new_instance": { "type": "boolean", "description": "macOS: start a separate instance even when the app is already running. Otherwise a running app is attached to, and left running when the session ends." },
                "foreground": { "type": "boolean", "description": "macOS and Windows: open the app in front and bring it and the target window to the front before every action, so the user can watch. Default false (the app stays in the background). Linux sessions are always on their own virtual display." },
                "display_width": { "type": "integer", "description": "Linux: the virtual display's width. Default 1440." },
                "display_height": { "type": "integer", "description": "Linux: the virtual display's height. Default 900." },
            }),
            &["app"],
        ),
        tool(
            "end_session",
            "End a session, quitting the app if the session started it. Sessions also end when this server exits.",
            json!({ "session_id": session_prop() }),
            &["session_id"],
        ),
        tool("list_sessions", "List this client's live sessions.", json!({}), &[]),
        tool(
            "list_windows",
            "List a session's windows, best first, with ids, titles and screen frames.",
            json!({ "session_id": session_prop() }),
            &["session_id"],
        ),
        tool(
            "screenshot",
            &format!("Capture a session's window, even when it is covered by other windows. {COORDS}"),
            json!({
                "session_id": session_prop(),
                "window_id": window_prop(),
                "ui_tree": { "type": "boolean", "description": "Also return the accessibility tree. Default false." },
            }),
            &["session_id"],
        ),
        tool(
            "get_ui_tree",
            "Read a window's accessibility tree: one line per element with an id (e12), role, name, value, frame in window coordinates and available actions. Element ids work with click, set_value and element_action until the next read.",
            json!({
                "session_id": session_prop(),
                "window_id": window_prop(),
                "max_depth": { "type": "integer", "description": "Default 25." },
                "max_nodes": { "type": "integer", "description": "Default 1500." },
            }),
            &["session_id"],
        ),
        tool(
            "click",
            &format!("Click in a session's window without moving your real pointer or focus. Give x/y, or an element id. {COORDS}"),
            action_props(click_props),
            &["session_id"],
        ),
        tool(
            "move_mouse",
            &format!("Move the session's pointer, for hover effects. {COORDS}"),
            action_props(point("Where to move")),
            &["session_id", "x", "y"],
        ),
        tool(
            "drag",
            &format!("Press, drag and release. {COORDS}"),
            action_props(json!({
                "from_x": { "type": "number" }, "from_y": { "type": "number" },
                "to_x": { "type": "number" }, "to_y": { "type": "number" },
                "button": { "type": "string", "enum": ["left", "right", "middle"] },
            })),
            &["session_id", "from_x", "from_y", "to_x", "to_y"],
        ),
        tool(
            "scroll",
            &format!("Scroll at a point. {COORDS}"),
            action_props(scroll_props),
            &["session_id", "x", "y"],
        ),
        tool(
            "type_text",
            "Type text into the window's focused field. Newlines press Return.",
            action_props(json!({ "text": { "type": "string" } })),
            &["session_id", "text"],
        ),
        tool(
            "press_key",
            "Press keys: chords joined with +, several separated by spaces. Modifiers: cmd (meta/win/super), ctrl, alt (option), shift. Named keys: enter, tab, escape, backspace, delete, space, up, down, left, right, home, end, pageup, pagedown, f1-f24. Examples: \"cmd+s\", \"ctrl+shift+tab\", \"down down enter\".",
            action_props(json!({ "keys": { "type": "string" } })),
            &["session_id", "keys"],
        ),
        tool(
            "set_value",
            "Set an element's value directly (a text field's contents, a slider's position) through accessibility. On a web page's dropdown (a PopUpButton) the value is the option's text, picked without opening the menu.",
            action_props(json!({
                "element": { "type": "string", "description": "Element id from get_ui_tree." },
                "value": { "type": "string" },
            })),
            &["session_id", "element", "value"],
        ),
        tool(
            "element_action",
            "Perform an accessibility action on an element: press, focus, showmenu, increment, decrement, confirm, cancel, raise, pick. The tree lists each element's actions.",
            action_props(json!({
                "element": { "type": "string", "description": "Element id from get_ui_tree." },
                "action": { "type": "string", "description": "Default press." },
            })),
            &["session_id", "element"],
        ),
        tool(
            "wait",
            "Wait for the app, then look again.",
            action_props(json!({ "ms": { "type": "integer", "description": "Milliseconds, at most 60000." } })),
            &["session_id", "ms"],
        ),
        tool(
            "permissions",
            "Show the OS permissions computer use needs and whether they are granted.",
            json!({}),
            &[],
        ),
    ]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing \"{key}\""))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn num(args: &Value, key: &str) -> Result<f64> {
    args.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("missing \"{key}\""))
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn flag(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn button(args: &Value) -> Result<MouseButton> {
    Ok(match args.get("button").and_then(Value::as_str) {
        None | Some("left") => MouseButton::Left,
        Some("right") => MouseButton::Right,
        Some("middle") => MouseButton::Middle,
        Some(other) => bail!("unknown button \"{other}\""),
    })
}

/// Builds the action a tool call names.
pub fn action_for(name: &str, args: &Value) -> Result<Option<Action>> {
    Ok(Some(match name {
        "click" => match opt_str(args, "element") {
            Some(element) => {
                let action = match (button(args)?, opt_u64(args, "count").unwrap_or(1)) {
                    (MouseButton::Left, 1) => None,
                    (MouseButton::Right, 1) => Some("showmenu".to_string()),
                    _ => {
                        bail!("element clicks are single left or right clicks; use x/y for others")
                    }
                };
                Action::ElementAction {
                    element,
                    name: action,
                }
            }
            None => Action::Click {
                x: num(args, "x")?,
                y: num(args, "y")?,
                button: button(args)?,
                count: opt_u64(args, "count").unwrap_or(1).clamp(1, 3) as u32,
                modifiers: opt_str(args, "modifiers"),
            },
        },
        "move_mouse" => Action::MoveMouse {
            x: num(args, "x")?,
            y: num(args, "y")?,
        },
        "drag" => Action::Drag {
            from_x: num(args, "from_x")?,
            from_y: num(args, "from_y")?,
            to_x: num(args, "to_x")?,
            to_y: num(args, "to_y")?,
            button: button(args)?,
        },
        "scroll" => Action::Scroll {
            x: num(args, "x")?,
            y: num(args, "y")?,
            dx: args.get("dx").and_then(Value::as_f64).unwrap_or(0.0),
            dy: args.get("dy").and_then(Value::as_f64).unwrap_or(0.0),
        },
        "type_text" => Action::TypeText {
            text: str_arg(args, "text")?.to_string(),
        },
        "press_key" => Action::PressKey {
            keys: str_arg(args, "keys")?.to_string(),
        },
        "set_value" => Action::SetValue {
            element: str_arg(args, "element")?.to_string(),
            value: str_arg(args, "value")?.to_string(),
        },
        "element_action" => {
            let element = str_arg(args, "element")?.to_string();
            match opt_str(args, "action") {
                Some(a) if a.eq_ignore_ascii_case("focus") => Action::Focus { element },
                name => Action::ElementAction { element, name },
            }
        }
        "wait" => Action::Wait {
            ms: opt_u64(args, "ms").unwrap_or(1000),
        },
        _ => return Ok(None),
    }))
}

fn tree_text(tree: &UiNode) -> String {
    format!("Accessibility tree:\n{}", tree.render())
}

pub fn call(handler: &mut dyn Handler, name: &str, args: &Value) -> Result<Output> {
    let text = |t: String| {
        Ok(Output {
            text: t,
            image: None,
        })
    };
    if let Some(action) = action_for(name, args)? {
        let session = str_arg(args, "session_id")?.to_string();
        let observe = Observe {
            screenshot: flag(args, "screenshot", true),
            ui_tree: flag(args, "ui_tree", false),
            ..Default::default()
        };
        let Response::Performed {
            screenshot,
            ui_tree,
        } = handler.handle(Request::Perform {
            session,
            window: opt_u64(args, "window_id"),
            action,
            observe,
        })?
        else {
            bail!("unexpected reply");
        };
        let mut out = format!("Done: {name}.");
        if let Some(s) = &screenshot {
            out.push_str(&format!(
                " Screenshot of window {} ({}x{}).",
                s.window_id.unwrap_or(0),
                s.width,
                s.height
            ));
        }
        if let Some(t) = &ui_tree {
            out.push('\n');
            out.push_str(&tree_text(t));
        }
        return Ok(Output {
            text: out,
            image: screenshot,
        });
    }
    match name {
        "start_session" => {
            let display_size = match (
                opt_u64(args, "display_width"),
                opt_u64(args, "display_height"),
            ) {
                (Some(w), Some(h)) => Some(ocu_core::Size {
                    width: w as u32,
                    height: h as u32,
                }),
                _ => None,
            };
            let spec = LaunchSpec {
                app: str_arg(args, "app")?.to_string(),
                args: serde_json::from_value(args.get("args").cloned().unwrap_or(json!([])))?,
                env: serde_json::from_value(args.get("env").cloned().unwrap_or(json!({})))?,
                cwd: opt_str(args, "cwd"),
                new_instance: flag(args, "new_instance", false),
                foreground: flag(args, "foreground", false),
                display_size,
            };
            let Response::Session { info, windows } =
                handler.handle(Request::StartSession(spec))?
            else {
                bail!("unexpected reply");
            };
            text(serde_json::to_string_pretty(
                &json!({ "session_id": info.id, "session": info, "windows": windows }),
            )?)
        }
        "end_session" => {
            handler.handle(Request::EndSession {
                session: str_arg(args, "session_id")?.to_string(),
            })?;
            text("Session ended.".into())
        }
        "list_sessions" => {
            let Response::Sessions(s) = handler.handle(Request::ListSessions)? else {
                bail!("unexpected reply")
            };
            text(serde_json::to_string_pretty(&s)?)
        }
        "list_windows" => {
            let Response::Windows(w) = handler.handle(Request::ListWindows {
                session: str_arg(args, "session_id")?.to_string(),
            })?
            else {
                bail!("unexpected reply")
            };
            text(serde_json::to_string_pretty(&w)?)
        }
        "screenshot" => {
            let session = str_arg(args, "session_id")?.to_string();
            let window = opt_u64(args, "window_id");
            let Response::Screenshot(shot) = handler.handle(Request::Screenshot {
                session: session.clone(),
                window,
            })?
            else {
                bail!("unexpected reply")
            };
            let mut out = format!(
                "Window {} ({}x{}).",
                shot.window_id.unwrap_or(0),
                shot.width,
                shot.height
            );
            if flag(args, "ui_tree", false) {
                if let Response::UiTree(t) = handler.handle(Request::UiTree {
                    session,
                    window,
                    options: TreeOptions::default(),
                })? {
                    out.push('\n');
                    out.push_str(&tree_text(&t));
                }
            }
            Ok(Output {
                text: out,
                image: Some(shot),
            })
        }
        "get_ui_tree" => {
            let mut options = TreeOptions::default();
            if let Some(d) = opt_u64(args, "max_depth") {
                options.max_depth = d as usize;
            }
            if let Some(n) = opt_u64(args, "max_nodes") {
                options.max_nodes = n as usize;
            }
            let Response::UiTree(t) = handler.handle(Request::UiTree {
                session: str_arg(args, "session_id")?.to_string(),
                window: opt_u64(args, "window_id"),
                options,
            })?
            else {
                bail!("unexpected reply")
            };
            text(t.render())
        }
        "run_recipe" => recipe::run(handler, args),
        "permissions" => {
            let Response::Permissions(p) = handler.handle(Request::Permissions)? else {
                bail!("unexpected reply")
            };
            if p.is_empty() {
                return text("This platform needs no special permissions.".into());
            }
            text(serde_json::to_string_pretty(&p)?)
        }
        "unlock_screen" => {
            if !crate::config::Config::load().allow_unlock {
                return text(
                    "Unlocking is turned off. Turn on \"Work while the Mac is locked\" in \
                     the OpenComputerUse settings."
                        .into(),
                );
            }
            let Response::Unlocked(ok) = handler.handle(Request::Unlock)? else {
                bail!("unexpected reply")
            };
            text(if ok {
                "The screen is unlocked.".into()
            } else {
                "The screen is still locked: the unlock did not complete.".into()
            })
        }
        _ => bail!("unknown tool \"{name}\""),
    }
}
