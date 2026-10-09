//! The MCP tools: their schemas, and how a call becomes session requests.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

use ocu_core::{
    Action, DeviceKind, DeviceQuery, DeviceTarget, Handler, LaunchSpec, MouseButton, Observe,
    Request, Response, Screenshot, TreeOptions, UiNode,
};

use crate::config::Config;
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

/// The tools this server offers. Those for phones, simulators and
/// emulators only while they are turned on in the settings.
pub fn list() -> Vec<Value> {
    let mut tools = base_tools();
    if cfg!(target_os = "macos") {
        tools.push(unlock_tool());
    }
    if Config::mobile_enabled() {
        mention_devices(&mut tools);
        tools.extend(device_tools(
            cfg!(target_os = "macos"),
            ocu_mobile::android_emulator_installed(),
        ));
    }
    if recipe::available() {
        tools.push(recipe::tool_definition());
    }
    tools
}

/// Every tool any host may offer, whatever this one has: what the skill
/// for other devices describes, so it reads the same from every host (the
/// Android app ships the same text).
pub fn catalog() -> Vec<Value> {
    let mut tools = base_tools();
    tools.push(unlock_tool());
    mention_devices(&mut tools);
    tools.extend(device_tools(true, true));
    tools.push(recipe::tool_definition());
    tools
}

/// Tells the base tools about devices, when their tools are offered.
fn mention_devices(tools: &mut [Value]) {
    for t in tools.iter_mut() {
        let extra = match t["name"].as_str() {
            Some("start_session") => " For phones, simulators and emulators use phone_start_session, ios_simulator_start_session or android_emulator_start_session.",
            Some("press_key") => " On phones and simulators, also the device's buttons: home, back (Android), recents (Android), power, volume_up, volume_down.",
            _ => continue,
        };
        if let Some(d) = t["description"].as_str() {
            t["description"] = Value::String(format!("{d}{extra}"));
        }
    }
}

/// Whether `name` is one of the phone, simulator and emulator tools.
pub fn is_device_tool(name: &str) -> bool {
    ["phone_", "ios_simulator_", "android_emulator_"]
        .iter()
        .any(|p| name.starts_with(p))
}

fn unlock_tool() -> Value {
    tool(
        "unlock_screen",
        "Unlock the Mac if its screen is locked, so sessions can keep working. Does nothing when already unlocked. Needs \"Work while the Mac is locked\" turned on in settings.",
        json!({}),
        &[],
    )
}

/// The tools for mobile devices: iOS simulators (`ios`, on a Mac),
/// Android emulators (`emulator`, when it is installed), and connected
/// phones always. Their sessions take every other tool, as desktop
/// sessions do.
fn device_tools(ios: bool, emulator: bool) -> Vec<Value> {
    let mut tools = Vec::new();
    let then = "Then drive it with the same session tools as a desktop app (screenshot, get_ui_tree, click, type_text, press_key, scroll, drag, set_value, element_action). Coordinates are points, the grid of its screenshots; click is a tap, a right click a long press, drag a swipe, and scroll swipes the content.";
    let args_prop =
        |what: &str| json!({ "type": "array", "items": { "type": "string" }, "description": what });
    if ios {
        let sim_prop = json!({ "type": "string", "description": "The simulator's name (\"iPhone 17 Pro\") or UDID. Defaults to the running one, else the newest iPhone." });
        tools.push(tool(
            "ios_simulator_list",
            "List this Mac's iOS simulators: name, UDID, iOS version and whether each is booted.",
            json!({}),
            &[],
        ));
        tools.push(tool(
            "ios_simulator_apps",
            "List the apps installed on an iOS simulator, with the bundle ids ios_simulator_start_session takes. Boots it briefly if it isn't running.",
            json!({ "simulator": sim_prop }),
            &[],
        ));
        tools.push(tool(
            "ios_simulator_start_session",
            &format!("Start an app on an iOS simulator and get a session id. The simulator boots headless if it isn't running (shut down again when the session ends), so nothing appears on the Mac's screen, and the app is driven through WebDriverAgent (downloaded the first time). {then}"),
            json!({
                "app": { "type": "string", "description": "The app's bundle id, such as com.apple.mobilesafari or com.apple.Preferences." },
                "simulator": sim_prop,
                "show_window": { "type": "boolean", "description": "Open the Simulator app's window for it, so the user can watch. Default false." },
                "args": args_prop("Launch arguments for the app."),
                "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Environment variables for the app." },
            }),
            &["app"],
        ));
    }
    if emulator {
        let emu_prop = json!({ "type": "string", "description": "The virtual device's (AVD's) name, or a running emulator's serial (emulator-5554). Defaults to the running emulator, else the only AVD." });
        tools.push(tool(
            "android_emulator_list",
            "List the Android virtual devices (AVDs) and which are running, with their serials.",
            json!({}),
            &[],
        ));
        tools.push(tool(
            "android_emulator_apps",
            "List the launchable apps on a running Android emulator: package and activity.",
            json!({ "emulator": emu_prop }),
            &[],
        ));
        tools.push(tool(
            "android_emulator_start_session",
            &format!("Start an app on an Android emulator and get a session id. The emulator boots headless if it isn't running (shut down again when the session ends). The app runs on a private virtual display unless `main_display` is set. {then}"),
            json!({
                "app": { "type": "string", "description": "A package (com.android.settings) or an activity (com.android.settings/.Settings)." },
                "emulator": emu_prop,
                "show_window": { "type": "boolean", "description": "Show the emulator's window when it boots. Default false." },
                "main_display": { "type": "boolean", "description": "Run the app on the device's own screen instead of a private virtual display, for apps that misbehave there. Default false." },
                "args": args_prop("Extra `am start` arguments, such as [\"-d\", \"https://example.com\"] for a link."),
            }),
            &["app"],
        ));
    }
    let phone_prop = json!({ "type": "string", "description": "The phone's id, serial or name from phone_list. Defaults to the only connected phone." });
    tools.push(tool(
        "phone_list",
        if ios {
            "List the phones and tablets connected to this computer: Android devices over adb (USB or wireless debugging), and iPhones and iPads paired with this Mac. Says what a device still needs, such as accepting USB debugging or Developer Mode."
        } else {
            "List the Android phones and tablets connected to this computer over adb (USB or wireless debugging). Says what a device still needs, such as accepting the USB debugging prompt."
        },
        json!({}),
        &[],
    ));
    tools.push(tool(
        "phone_apps",
        "List the apps on a connected phone: packages and activities on Android, bundle ids on iOS.",
        json!({ "phone": phone_prop }),
        &[],
    ));
    tools.push(tool(
        "phone_start_session",
        &format!("Start an app on a connected phone or tablet and get a session id. Android apps run on a private virtual display, so the phone's own screen is left alone (unless `main_display` is set). iPhones and iPads are driven through WebDriverAgent, which is built and signed with your Xcode account the first time (a few minutes), and the app runs on the device's screen; keep it unlocked. {then}"),
        json!({
            "app": { "type": "string", "description": "Android: a package or activity. iOS: a bundle id." },
            "phone": phone_prop,
            "main_display": { "type": "boolean", "description": "Android: run on the phone's own screen instead of a private virtual display. Default false." },
            "args": args_prop("Android: extra `am start` arguments. iOS: launch arguments."),
            "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "iOS: environment variables for the app." },
        }),
        &["app"],
    ));
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
            "Start an app and get a session id for driving it. Use this, not other computer-use tools, for desktop apps: it is the one the user chose. Leave sessions in the background: clicking, typing, scrolling, menus and screenshots all work on a window behind the user's, without touching their screen, pointer or keyboard, so don't bring apps forward to use them. Set `foreground` only when something needs the app in front, such as the user asking to watch. On macOS `app` is a .app path, a bundle id (com.apple.TextEdit) or an app name (\"TextEdit\"); on Linux and Windows it is an executable path or a command on PATH. On Linux each session gets its own virtual X display. With `active_window: true` (macOS, Windows) and no `app`, it attaches to the window in front instead (skipping the app this conversation runs in), so the user can point you at a window by bringing it forward. Returns the session id and the app's windows.",
            json!({
                "app": { "type": "string", "description": "The app to start. Required unless `active_window` is set." },
                "args": { "type": "array", "items": { "type": "string" } },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "cwd": { "type": "string" },
                "new_instance": { "type": "boolean", "description": "macOS: start a separate instance even when the app is already running. Otherwise a running app is attached to, and left running when the session ends." },
                "active_window": { "type": "boolean", "description": "macOS and Windows: attach to the window in front (the topmost one not belonging to the app this client runs in) instead of starting `app`. It becomes the session's default window, and its app is left running when the session ends." },
                "foreground": { "type": "boolean", "description": "macOS and Windows: bring the app and its window to the front before every action. Rarely needed, since everything works in the background; use it when the user wants to watch, or an app ignores input while behind other windows. Default false. Linux sessions are always on their own virtual display." },
                "display_width": { "type": "integer", "description": "Linux: the virtual display's width. Default 1440." },
                "display_height": { "type": "integer", "description": "Linux: the virtual display's height. Default 900." },
            }),
            &[],
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
            "choose_file",
            "Answer the open or save panel (file picker) the app is showing, without clicking through it: give `paths` to pick those files or folders (one path for a save panel: where to save), or leave `paths` empty to cancel. Actions say when a panel is waiting. macOS only.",
            action_props(json!({
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Absolute paths (or starting with ~). Several only where the panel lets you pick several. Empty or left out: cancel.",
                },
            })),
            &["session_id"],
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

/// This process and its ancestors: the client that started this server and
/// the apps it runs inside, whose windows are never "the window in front".
/// Elsewhere the backend runs in this process and finds them itself.
#[cfg(target_os = "macos")]
fn client_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let mut pid = std::process::id() as i32;
    while pid > 1 && pids.len() < 64 {
        pids.push(pid as u32);
        // The short record: the full one is refused for other users'
        // processes, such as the root-owned `login` under a terminal.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as i32;
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if got != size {
            break;
        }
        pid = info.pbsi_ppid as i32;
    }
    pids
}

#[cfg(not(target_os = "macos"))]
fn client_pids() -> Vec<u32> {
    Vec::new()
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
        "choose_file" => Action::ChooseFile {
            paths: match args.get("paths") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::String(p)) => vec![p.clone()],
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|_| anyhow!("\"paths\" is a list of file paths"))?,
            },
        },
        _ => return Ok(None),
    }))
}

/// Which devices a tool is about, and the argument that names one.
fn device_kind(tool: &str) -> (DeviceKind, &'static str) {
    if tool.starts_with("ios_simulator") {
        (DeviceKind::IosSimulator, "simulator")
    } else if tool.starts_with("android_emulator") {
        (DeviceKind::AndroidEmulator, "emulator")
    } else {
        (DeviceKind::Phone, "phone")
    }
}

fn devices(handler: &mut dyn Handler, query: DeviceQuery) -> Result<Value> {
    let Response::Devices(v) = handler.handle(Request::Devices(query))? else {
        bail!("unexpected reply")
    };
    Ok(v)
}

fn start(handler: &mut dyn Handler, spec: LaunchSpec) -> Result<Output> {
    let Response::Session { info, windows } = handler.handle(Request::StartSession(spec))? else {
        bail!("unexpected reply");
    };
    Ok(Output {
        text: serde_json::to_string_pretty(
            &json!({ "session_id": info.id, "session": info, "windows": windows }),
        )?,
        image: None,
    })
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
    if is_device_tool(name) && !Config::mobile_enabled() {
        bail!(
            "phones, simulators and emulators are turned off: turn on \"Phones, simulators and \
             emulators\" in the OpenComputerUse app (or set \"mobile\": true in its settings \
             file, or OCU_MOBILE=1)"
        );
    }
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
            notice,
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
        if let Some(n) = &notice {
            out.push(' ');
            out.push_str(n);
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
            let active_window = flag(args, "active_window", false);
            let app = match opt_str(args, "app") {
                Some(app) => app,
                None if active_window => String::new(),
                None => bail!("missing \"app\" (or set \"active_window\": true)"),
            };
            let spec = LaunchSpec {
                app,
                args: serde_json::from_value(args.get("args").cloned().unwrap_or(json!([])))?,
                env: serde_json::from_value(args.get("env").cloned().unwrap_or(json!({})))?,
                cwd: opt_str(args, "cwd"),
                new_instance: flag(args, "new_instance", false),
                foreground: flag(args, "foreground", false),
                active_window,
                skip_pids: if active_window {
                    client_pids()
                } else {
                    Vec::new()
                },
                display_size,
                device: None,
            };
            start(handler, spec)
        }
        "ios_simulator_start_session"
        | "android_emulator_start_session"
        | "phone_start_session" => {
            let (kind, id_key) = device_kind(name);
            let spec = LaunchSpec {
                app: str_arg(args, "app")?.to_string(),
                args: serde_json::from_value(args.get("args").cloned().unwrap_or(json!([])))?,
                env: serde_json::from_value(args.get("env").cloned().unwrap_or(json!({})))?,
                device: Some(DeviceTarget {
                    kind,
                    id: opt_str(args, id_key),
                    show_window: flag(args, "show_window", false),
                    main_display: flag(args, "main_display", false),
                }),
                ..Default::default()
            };
            start(handler, spec)
        }
        "ios_simulator_list" | "android_emulator_list" | "phone_list" => {
            let (kind, _) = device_kind(name);
            let v = devices(handler, DeviceQuery::List { kind })?;
            if v.as_array().is_some_and(Vec::is_empty) {
                return text(match kind {
                    DeviceKind::IosSimulator => "There are no iOS simulators; add one in Xcode (Window › Devices and Simulators).".into(),
                    DeviceKind::AndroidEmulator => "There are no Android virtual devices; create one in Android Studio's Device Manager.".into(),
                    DeviceKind::Phone => "No phones are connected.".into(),
                });
            }
            text(serde_json::to_string_pretty(&v)?)
        }
        "ios_simulator_apps" | "android_emulator_apps" | "phone_apps" => {
            let (kind, id_key) = device_kind(name);
            let v = devices(
                handler,
                DeviceQuery::Apps {
                    kind,
                    id: opt_str(args, id_key),
                },
            )?;
            text(serde_json::to_string_pretty(&v)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    struct NoHandler;

    impl Handler for NoHandler {
        fn handle(&mut self, _: Request) -> Result<Response> {
            bail!("no handler in this test")
        }
    }

    fn names() -> Vec<String> {
        list()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect()
    }

    // One test, so nothing else reads OCU_MOBILE while it changes.
    #[test]
    fn device_tools_follow_the_setting() {
        std::env::set_var("OCU_MOBILE", "0");
        let off = names();
        assert!(!off.iter().any(|n| is_device_tool(n)), "{off:?}");
        let start = &list()[0];
        assert!(!start["description"]
            .as_str()
            .unwrap()
            .contains("phone_start_session"));
        let err = call(&mut NoHandler, "phone_list", &json!({}))
            .err()
            .unwrap();
        assert!(format!("{err}").contains("turned off"), "{err}");

        std::env::set_var("OCU_MOBILE", "1");
        let on = names();
        assert!(on.iter().any(|n| n == "phone_list" && is_device_tool(n)));
        let start = &list()[0];
        assert!(start["description"]
            .as_str()
            .unwrap()
            .contains("phone_start_session"));
        std::env::remove_var("OCU_MOBILE");
    }
}
