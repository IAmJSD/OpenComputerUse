//! Recipes: a fixed list of steps ("click the search box", "type 'cats'
//! into the search field", "press enter") run without the calling agent in
//! the loop. Nothing is generated. For each step a decision model (TypeSafe's
//! Jev, or Cloudflare's Clef) picks which element of the accessibility tree
//! the step means, as a choice with a probability per element, and the step
//! acts on it when the model is confident enough. No branching: a step it
//! cannot place stops the recipe, and the caller gets the state to take over.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ocu_core::{Action, Handler, MouseButton, Observe, Request, Response, TreeOptions, UiNode};

use crate::config::{Config, Provider, RecipeConfig};
use crate::tools::Output;

/// Whether recipes can run here: a decision model is set up.
pub fn available() -> bool {
    Config::load().recipe.is_configured()
}

pub fn tool_definition() -> Value {
    json!({
        "name": "run_recipe",
        "description": "Run a fixed sequence of UI steps in a session without you in the loop. Each step names its target in plain words; a fast decision model (TypeSafe Jev or Cloudflare Clef, chosen in the OpenComputerUse settings) matches that description to an element of the accessibility tree and the step acts on it. For straight-line chores with no decisions: 'click the address bar', 'type \"example.com\" into the address bar', 'press enter'. Stops at the first step it cannot place confidently and reports where it got to, with a screenshot. Steps are strings (\"click <target>\", \"double click <target>\", \"right click <target>\", \"hover <target>\", \"type \\\"<text>\\\" into <target>\", \"type \\\"<text>\\\"\", \"set <target> to \\\"<value>\\\"\", \"press <keys>\", \"scroll down|up [in <target>]\", \"wait <ms>\") or objects ({\"click\": target}, {\"type\": text, \"into\": target}, {\"press\": keys}, {\"set_value\": value, \"on\": target}, {\"scroll\": \"down\", \"in\": target}, {\"wait\": ms}).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "window_id": { "type": "integer" },
                "steps": {
                    "type": "array",
                    "items": { "anyOf": [{ "type": "string" }, { "type": "object" }] },
                },
                "min_confidence": { "type": "number", "description": "Override the configured confidence threshold (0-1)." },
            },
            "required": ["session_id", "steps"],
        },
    })
}

#[derive(Debug, Clone)]
enum Step {
    Click {
        target: String,
        button: MouseButton,
        count: u32,
    },
    Hover {
        target: String,
    },
    Type {
        text: String,
        into: Option<String>,
    },
    SetValue {
        value: String,
        on: String,
    },
    Press {
        keys: String,
    },
    Scroll {
        dy: f64,
        within: Option<String>,
    },
    Wait {
        ms: u64,
    },
}

impl Step {
    fn target(&self) -> Option<&str> {
        match self {
            Step::Click { target, .. } | Step::Hover { target } => Some(target),
            Step::Type { into, .. } => into.as_deref(),
            Step::SetValue { on, .. } => Some(on),
            Step::Scroll { within, .. } => within.as_deref(),
            Step::Press { .. } | Step::Wait { .. } => None,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StepObject {
    click: Option<String>,
    double_click: Option<String>,
    right_click: Option<String>,
    hover: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    into: Option<String>,
    set_value: Option<String>,
    on: Option<String>,
    press: Option<String>,
    scroll: Option<String>,
    #[serde(rename = "in")]
    in_: Option<String>,
    wait: Option<u64>,
}

fn quoted(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let q = s
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\'' | '“' | '‘'))?;
    let close = match q {
        '“' => '”',
        '‘' => '’',
        c => c,
    };
    let body = &s[q.len_utf8()..];
    let end = body.find(close)?;
    Some((body[..end].to_string(), &body[end + close.len_utf8()..]))
}

fn strip_prefix_ci<'a>(s: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let lower = s.to_lowercase();
    prefixes
        .iter()
        .find(|p| lower.starts_with(*p))
        .map(|p| s[p.len()..].trim())
}

fn scroll_amount(dir: &str) -> Result<f64> {
    match dir.trim().to_lowercase().as_str() {
        "down" => Ok(400.0),
        "up" => Ok(-400.0),
        other => bail!("scroll goes \"up\" or \"down\", not \"{other}\""),
    }
}

fn parse_text(s: &str) -> Result<Step> {
    let s = s.trim();
    if let Some(rest) = strip_prefix_ci(s, &["double click ", "double-click ", "doubleclick "]) {
        return Ok(Step::Click {
            target: rest.into(),
            button: MouseButton::Left,
            count: 2,
        });
    }
    if let Some(rest) = strip_prefix_ci(s, &["right click ", "right-click ", "rightclick "]) {
        return Ok(Step::Click {
            target: rest.into(),
            button: MouseButton::Right,
            count: 1,
        });
    }
    if let Some(rest) = strip_prefix_ci(s, &["click on ", "click ", "tap on ", "tap "]) {
        return Ok(Step::Click {
            target: rest.into(),
            button: MouseButton::Left,
            count: 1,
        });
    }
    if let Some(rest) = strip_prefix_ci(s, &["hover over ", "hover "]) {
        return Ok(Step::Hover {
            target: rest.into(),
        });
    }
    if let Some(rest) = strip_prefix_ci(s, &["type ", "enter "]) {
        let (text, after) =
            quoted(rest).ok_or_else(|| anyhow!("put the text to type in quotes: {s}"))?;
        let into = strip_prefix_ci(after.trim(), &["into ", "in "]).map(str::to_string);
        return Ok(Step::Type { text, into });
    }
    if let Some(rest) = strip_prefix_ci(s, &["set "]) {
        let lower = rest.to_lowercase();
        let at = lower
            .rfind(" to ")
            .ok_or_else(|| anyhow!("write \"set <target> to \\\"<value>\\\"\": {s}"))?;
        let (value, _) =
            quoted(&rest[at + 4..]).ok_or_else(|| anyhow!("put the value in quotes: {s}"))?;
        return Ok(Step::SetValue {
            value,
            on: rest[..at].trim().into(),
        });
    }
    if let Some(rest) = strip_prefix_ci(s, &["press "]) {
        return Ok(Step::Press { keys: rest.into() });
    }
    if let Some(rest) = strip_prefix_ci(s, &["scroll "]) {
        let (dir, within) = match rest.split_once(' ') {
            Some((d, w)) => (
                d,
                strip_prefix_ci(w, &["in ", "within "]).map(str::to_string),
            ),
            None => (rest, None),
        };
        return Ok(Step::Scroll {
            dy: scroll_amount(dir)?,
            within,
        });
    }
    if let Some(rest) = strip_prefix_ci(s, &["wait "]) {
        let rest = rest.trim().to_lowercase();
        let ms = if let Some(secs) = rest.strip_suffix('s').filter(|r| !r.ends_with('m')) {
            (secs.trim().parse::<f64>()? * 1000.0) as u64
        } else {
            rest.trim_end_matches("ms").trim().parse()?
        };
        return Ok(Step::Wait { ms });
    }
    bail!("cannot read step \"{s}\"; start it with click, double click, right click, hover, type, set, press, scroll or wait")
}

fn parse_step(v: &Value) -> Result<Step> {
    if let Some(s) = v.as_str() {
        return parse_text(s);
    }
    let o: StepObject =
        serde_json::from_value(v.clone()).with_context(|| format!("reading step {v}"))?;
    Ok(if let Some(t) = o.click {
        Step::Click {
            target: t,
            button: MouseButton::Left,
            count: 1,
        }
    } else if let Some(t) = o.double_click {
        Step::Click {
            target: t,
            button: MouseButton::Left,
            count: 2,
        }
    } else if let Some(t) = o.right_click {
        Step::Click {
            target: t,
            button: MouseButton::Right,
            count: 1,
        }
    } else if let Some(t) = o.hover {
        Step::Hover { target: t }
    } else if let Some(text) = o.type_ {
        Step::Type { text, into: o.into }
    } else if let Some(value) = o.set_value {
        Step::SetValue {
            value,
            on: o
                .on
                .ok_or_else(|| anyhow!("set_value needs \"on\": the target"))?,
        }
    } else if let Some(keys) = o.press {
        Step::Press { keys }
    } else if let Some(dir) = o.scroll {
        Step::Scroll {
            dy: scroll_amount(&dir)?,
            within: o.in_,
        }
    } else if let Some(ms) = o.wait {
        Step::Wait { ms }
    } else {
        bail!("step {v} names no action")
    })
}

/// An element a step could mean, as the decision model sees it.
struct Candidate<'a> {
    node: &'a UiNode,
    description: String,
}

/// Roles worth targeting even without an action listed.
const TARGETABLE: &[&str] = &[
    "TextField",
    "TextArea",
    "SearchField",
    "ComboBox",
    "Link",
    "Button",
    "CheckBox",
    "RadioButton",
    "PopUpButton",
    "MenuItem",
    "MenuButton",
    "Tab",
    "TabItem",
    "Row",
    "Cell",
    "Slider",
    "Edit",
    "Hyperlink",
    "ListItem",
    "TreeItem",
    "Incrementor",
    "DisclosureTriangle",
    "ColorWell",
    "SplitButton",
    "DataItem",
    "ToggleButton",
    "SpinButton",
    "Switch",
    "entry",
    "push button",
    "link",
];

/// Roles that are never what a step means, whatever actions they claim:
/// web content offers "press" on every node, text and containers included,
/// and offering hundreds of them spreads the model's probability so thin a
/// right answer looks unsure.
const PASSIVE: &[&str] = &[
    "StaticText",
    "Text",
    "Heading",
    "Group",
    "Image",
    "ListMarker",
    "Splitter",
    "ScrollArea",
    "ScrollBar",
    "WebArea",
    "Toolbar",
    "TabGroup",
    "Window",
    "RadioGroup",
    "List",
    "Pane",
    "Document",
    "Separator",
    "Unknown",
    "LayoutArea",
    "LayoutItem",
    "Ruler",
    "RulerMarker",
    "Application",
    "Matte",
    "ValueIndicator",
    "TitleBar",
];

/// Whether an element has a real size. Off-screen ones (further down a
/// page) still count: they can be pressed through accessibility. Chrome
/// parks some it has not laid out at one pixel high, which do not.
fn visible(f: &ocu_core::Rect, _window: Option<&ocu_core::Rect>) -> bool {
    f.width > 2.0 && f.height > 2.0
}

/// The window's own buttons: close, minimise, zoom, full screen. macOS
/// names them only by subrole ("CloseButton"); Windows puts them in the
/// title bar.
fn window_control(node: &UiNode, parent_role: &str) -> bool {
    matches!(
        node.name.as_deref(),
        Some("CloseButton" | "MinimizeButton" | "ZoomButton" | "FullScreenButton")
    ) || (parent_role == "TitleBar" && node.role == "Button")
}

/// Whether a step is about the window itself, and so may mean its buttons.
fn names_window(target: &str) -> bool {
    target.to_lowercase().contains("window")
}

/// `title` is the window's, so containers named after it can be left out
/// of candidates' paths. The window's own buttons are offered only when
/// `controls`: a step about the page never means them, and a wrong pick
/// closes the window (Clef took "CloseButton" for a cookie banner's
/// "Reject non-essential cookies" at 0.71).
fn collect<'a>(
    node: &'a UiNode,
    window: Option<&ocu_core::Rect>,
    title: Option<&str>,
    controls: bool,
    parent_role: &str,
    path: &mut Vec<String>,
    out: &mut Vec<Candidate<'a>>,
) {
    let label = node.name.clone().or_else(|| node.description.clone());
    // Chrome reports a listbox's options (autocomplete suggestions, say) as
    // plain text inside a list; there they are choices, not prose.
    let option = matches!(parent_role, "List" | "ListBox" | "Menu") && label.is_some();
    let passive = PASSIVE.iter().any(|r| r.eq_ignore_ascii_case(&node.role)) && !option;
    let targetable = TARGETABLE
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&node.role))
        || (!node.actions.is_empty() && !passive);
    let excluded = !controls && window_control(node, parent_role);
    if targetable && !excluded && node.frame.is_some_and(|f| visible(&f, window)) && node.enabled {
        let mut d = if option && node.role == "StaticText" {
            "Option".to_string()
        } else {
            node.role.clone()
        };
        if let Some(l) = &label {
            d.push_str(&format!(" \"{l}\""));
        }
        if let Some(desc) = node
            .description
            .as_ref()
            .filter(|s| Some(*s) != label.as_ref())
        {
            d.push_str(&format!(" ({desc})"));
        }
        // A toggle's value is its state, not text it contains.
        let toggle = matches!(
            node.role.as_str(),
            "RadioButton" | "CheckBox" | "Switch" | "ToggleButton"
        );
        match node.value.as_deref().filter(|v| !v.is_empty()) {
            Some("1" | "true") if toggle => d.push_str(", selected"),
            Some(_) if toggle => {}
            Some(v) => {
                let v: String = v.chars().take(60).collect();
                d.push_str(&format!(" containing \"{v}\""));
            }
            None => {}
        }
        if node.focused {
            d.push_str(", focused");
        }
        if !path.is_empty() {
            d.push_str(&format!(", inside {}", path.join(" › ")));
        }
        out.push(Candidate {
            node,
            description: d,
        });
    }
    // Only containers that tell candidates apart go in the path: the window,
    // the page and the groups titled like them are the same for everything.
    let distinctive = !matches!(
        node.role.as_str(),
        "Window" | "WebArea" | "ScrollArea" | "Application"
    ) && label
        .as_deref()
        .is_some_and(|l| !title.is_some_and(|t| t.starts_with(l.trim_end_matches('…'))));
    let pushed = label.as_ref().filter(|_| distinctive).map(|l| {
        path.push(format!(
            "{} \"{}\"",
            node.role,
            l.chars().take(40).collect::<String>()
        ));
    });
    for c in &node.children {
        collect(c, window, title, controls, &node.role, path, out);
    }
    if pushed.is_some() {
        path.pop();
    }
}

/// Largest image sent to Clef. Workers AI estimates a request's tokens from
/// its size before decoding the images, so a full-size screenshot (half a
/// megabyte of base64) is refused as over the context window though the
/// model would see it as a few hundred tokens.
const MAX_IMAGE_BYTES: usize = 100 * 1024;

/// A PNG small enough to send, shrinking it until it is.
fn fit_image(png: &[u8]) -> Vec<u8> {
    if png.len() <= MAX_IMAGE_BYTES {
        return png.to_vec();
    }
    let Ok(img) = crate::vision::Image::decode(png) else {
        return png.to_vec();
    };
    let mut side = img.width.max(img.height).min(1024);
    loop {
        side = side * 3 / 4;
        let Ok(small) = img.shrink(side).encode() else {
            return png.to_vec();
        };
        if small.len() <= MAX_IMAGE_BYTES || side <= 256 {
            return small;
        }
    }
}

/// Images as Workers AI takes them: base64 data URIs.
fn encode_images(images: &[Vec<u8>]) -> Vec<Value> {
    use base64::Engine as _;
    images
        .iter()
        .map(|png| {
            let b64 = base64::engine::general_purpose::STANDARD.encode(fit_image(png));
            Value::String(format!("data:image/png;base64,{b64}"))
        })
        .collect()
}

/// Jev's bounds on each side of an image. It refuses images outside them
/// rather than resizing. The least side is 280, not the 256 TypeSafe also
/// states, so a padded image also has the 78,400 pixels it needs.
const JEV_MAX_SIDE: usize = 1568;
const JEV_MIN_SIDE: usize = 280;

/// A PNG within Jev's size bounds: shrunk until its longer side fits, padded
/// at the bottom and right until its shorter one does. Returns it with the
/// factor from its pixels back to the original's.
fn fit_for_jev(png: &[u8]) -> Result<(Vec<u8>, f64, crate::vision::Image)> {
    let img = crate::vision::Image::decode(png)?;
    let fits = |i: &crate::vision::Image| {
        i.width.max(i.height) <= JEV_MAX_SIDE && i.width.min(i.height) >= JEV_MIN_SIDE
    };
    if fits(&img) {
        return Ok((png.to_vec(), 1.0, img));
    }
    let small = img.shrink(JEV_MAX_SIDE);
    let scale = img.width as f64 / small.width as f64;
    let sent = small.pad(JEV_MIN_SIDE);
    Ok((sent.encode()?, scale, sent))
}

/// A reference for an image in a Jev request: TypeSafe finds and replaces
/// it in every string, so it must not occur in anything else.
fn image_ref() -> String {
    let mut buf = [0u8; 8];
    let _ = getrandom::fill(&mut buf);
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("ocu-image-{hex}")
}

/// Images as Jev takes them: each one an asset under a unique reference,
/// with the references put in the state where the image belongs.
fn attach_jev_images(body: &mut Value, images: &[Vec<u8>]) -> Result<()> {
    use base64::Engine as _;
    let mut assets = Map::new();
    let mut refs = Vec::new();
    for png in images {
        let (png, _, _) = fit_for_jev(png)?;
        let r = image_ref();
        assets.insert(
            r.clone(),
            json!({
                "type": "image",
                "format": "png",
                "data_base64": base64::engine::general_purpose::STANDARD.encode(png),
            }),
        );
        refs.push(Value::String(r));
    }
    let state = &mut body["state"];
    if !state.is_object() {
        *state = json!({ "context": state.take() });
    }
    if refs.len() == 1 {
        state["picture"] = refs.remove(0);
    } else {
        state["pictures"] = Value::Array(refs);
    }
    body["assets"] = Value::Object(assets);
    Ok(())
}

/// A request too long for the model's context window.
#[derive(Debug)]
struct ContextOverflow;

impl std::fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the request is longer than the decision model's context window")
    }
}

impl std::error::Error for ContextOverflow {}

fn overflowed(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ContextOverflow>().is_some()
}

/// [`ask_once`], fitted to the context window: a request too long for it is
/// split into halves, each asked on its own (the questions are independent,
/// so this changes nothing but the number of calls). A single question too
/// long for Jev is asked again with TypeSafe's long context, which is slower
/// and less accurate, so only then. One too long for Clef fails with
/// [`ContextOverflow`] for the caller to shorten.
fn ask(
    config: &RecipeConfig,
    state: &Value,
    questions: &Map<String, Value>,
    images: &[Vec<u8>],
) -> Result<Map<String, Value>> {
    match ask_once(config, state, questions, images, false) {
        Err(e) if overflowed(&e) && questions.len() > 1 => {
            let half = questions.len() / 2;
            let first: Map<String, Value> = questions
                .iter()
                .take(half)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let second: Map<String, Value> = questions
                .iter()
                .skip(half)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            log::info!(
                "recipe: {} questions overflow the context window; asking them in two halves",
                questions.len()
            );
            let mut answers = ask(config, state, &first, images)?;
            answers.extend(ask(config, state, &second, images)?);
            Ok(answers)
        }
        Err(e) if overflowed(&e) && config.provider == Provider::Typesafe => {
            log::info!(
                "recipe: a question overflows Jev's context window; asking with long context"
            );
            ask_once(config, state, questions, images, true)
        }
        r => r,
    }
}

/// One decision-model call: questions in, `answers` out, with `images`
/// (PNG) for the model to look at. `long_context` asks Jev for its 262k
/// window rather than the 64k (32k a question) one.
fn ask_once(
    config: &RecipeConfig,
    state: &Value,
    questions: &Map<String, Value>,
    images: &[Vec<u8>],
    long_context: bool,
) -> Result<Map<String, Value>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let (url, token, body) = match config.provider {
        Provider::Typesafe => {
            if config.typesafe_api_key.is_empty() {
                bail!("no TypeSafe API key; add one in the OpenComputerUse settings (or set TYPESAFE_API_KEY)");
            }
            let mut body =
                json!({ "model": config.typesafe_model, "state": state, "questions": questions });
            if !images.is_empty() {
                attach_jev_images(&mut body, images)?;
            }
            (
                "https://api.typesafe.ai/v1/systemone".to_string(),
                &config.typesafe_api_key,
                body,
            )
        }
        Provider::Cloudflare => {
            if config.cloudflare_account_id.is_empty() || config.cloudflare_api_token.is_empty() {
                bail!("no Cloudflare account id or API token; add them in the OpenComputerUse settings");
            }
            let model = config.cloudflare_model.trim_start_matches('/');
            // The body names the model by its short name: "clef-flash".
            let short = model.rsplit('/').next().unwrap_or(model);
            let mut body = json!({ "model": short, "state": state, "questions": questions });
            if !images.is_empty() {
                body["images"] = Value::Array(encode_images(&images[..images.len().min(4)]));
            }
            (
                format!(
                    "https://api.cloudflare.com/client/v4/accounts/{}/ai/run/{model}",
                    config.cloudflare_account_id
                ),
                &config.cloudflare_api_token,
                body,
            )
        }
    };
    let mut req = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {token}"));
    // Images and box questions are still in TypeSafe's "danger zone": off
    // unless a request asks for them.
    if config.provider == Provider::Typesafe && (body.get("assets").is_some() || has_box(questions))
    {
        req = req.header("TypeSafe-Danger-Zone-Images", "true");
    }
    if config.provider == Provider::Typesafe && long_context {
        req = req.header("TypeSafe-Danger-Zone-Long-Context", "true");
    }
    let mut resp = req
        .send_json(&body)
        .with_context(|| format!("calling {}", config.provider.label()))?;
    let status = resp.status();
    let reply: Value = resp
        .body_mut()
        .read_json()
        .context("reading the decision model's reply")?;
    if !status.is_success() {
        // Jev says so in `detail`. Workers AI uses error 3030 once it has
        // counted the tokens and 5021 when its estimate is already too many;
        // Clef-omni says so in a general validation error (5012).
        let jev = reply.pointer("/detail/error_type").and_then(Value::as_str)
            == Some("max_tokens_exceeded");
        let clef = reply
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|e| {
                e.iter().any(|e| {
                    matches!(e.get("code").and_then(Value::as_u64), Some(3030 | 5021))
                        || e.get("message")
                            .and_then(Value::as_str)
                            .is_some_and(|m| m.contains("exceeds the Clef token limit"))
                })
            });
        if jev || clef {
            return Err(anyhow::Error::new(ContextOverflow).context(format!(
                "{} answered {status}: {reply}",
                config.provider.label()
            )));
        }
        bail!("{} answered {status}: {reply}", config.provider.label());
    }
    // Workers AI wraps answers in "result".
    let answers = reply
        .get("result")
        .and_then(|r| r.get("answers"))
        .or_else(|| reply.get("answers"))
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("the decision model's reply has no answers: {reply}"))?;
    Ok(answers.clone())
}

fn has_box(questions: &Map<String, Value>) -> bool {
    questions
        .values()
        .any(|q| q.get("type").and_then(Value::as_str) == Some("box"))
}

/// Most options a Choice question takes, per both APIs.
const MAX_OPTIONS: usize = 255;
/// Most questions in one request (Clef's limit).
const MAX_QUESTIONS: usize = 64;

struct Pick {
    index: usize,
    probability: f64,
    confidence: f64,
    /// The next most likely option's probability.
    runner_up: f64,
}

impl Pick {
    /// Sure enough to act: likely enough outright, or a clear winner (at
    /// least even odds and twice anything else), which a model spread over
    /// many similar options often is without passing a fixed bar. The floor
    /// matters: asked for a train no longer listed, Clef gave the nearest
    /// one 0.39 against 0.14, and acting on that would book the wrong train.
    fn sure(&self, min: f64) -> bool {
        self.probability >= min
            || (self.probability >= 0.5 && self.probability >= 2.0 * self.runner_up)
    }
}

/// The chosen option, its probability, Clef's confidence, and the runner-up's
/// probability.
fn choice(answer: &Value) -> Option<(String, f64, f64, f64)> {
    let chosen = answer.get("choice")?.as_str()?.to_string();
    let probs = answer.get("probabilities").and_then(Value::as_object);
    let p = probs
        .and_then(|m| m.get(&chosen))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let runner_up = probs
        .map(|m| {
            m.iter()
                .filter(|(k, _)| **k != chosen)
                .filter_map(|(_, v)| v.as_f64())
                .fold(0.0, f64::max)
        })
        .unwrap_or(0.0);
    let c = answer
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(p);
    Some((chosen, p, c, runner_up))
}

/// Asks which candidate a step means. Past 255 candidates they are asked in
/// groups, then the groups' winners against each other.
fn pick(
    config: &RecipeConfig,
    app: &str,
    instruction: &str,
    candidates: &[Candidate],
    shot: Option<&Vec<u8>>,
) -> Result<Pick> {
    let question = |options: &[(usize, &Candidate)]| {
        let criteria: Map<String, Value> = options
            .iter()
            .map(|(i, c)| (format!("e{i}"), Value::String(c.description.clone())))
            .collect();
        json!({
            "type": "choice",
            "instructions": format!("Which interface element is the one this step refers to: \"{instruction}\"?"),
            "criteria": criteria,
        })
    };
    let mut state = json!({ "application": app, "step": instruction });
    if shot.is_some() {
        state["image"] =
            json!("The image is the app's window. Element positions are in its pixels.");
    }
    let images: Vec<Vec<u8>> = shot.cloned().into_iter().collect();
    let indexed: Vec<(usize, &Candidate)> = candidates.iter().enumerate().collect();
    let mut round: Vec<(usize, &Candidate)> = indexed;
    // Fewer options a question when one is too long for the model (Clef-flash
    // has 24k tokens; Jev falls back to its long context first).
    let mut per_group = MAX_OPTIONS;
    'rounds: loop {
        if round.len() == 1 {
            return Ok(Pick {
                index: round[0].0,
                probability: 1.0,
                confidence: 1.0,
                runner_up: 0.0,
            });
        }
        let groups: Vec<&[(usize, &Candidate)]> = round.chunks(per_group).collect();
        let mut winners: Vec<(usize, &Candidate, f64, f64, f64)> = Vec::new();
        for batch in groups.chunks(MAX_QUESTIONS) {
            let questions: Map<String, Value> = batch
                .iter()
                .enumerate()
                .map(|(g, opts)| (format!("g{g}"), question(opts)))
                .collect();
            let answers = match ask(config, &state, &questions, &images) {
                Err(e) if overflowed(&e) && per_group > 8 => {
                    per_group /= 2;
                    log::info!("recipe: a choice overflows the context window; {per_group} options a question");
                    continue 'rounds;
                }
                r => r?,
            };
            for (g, _) in batch.iter().enumerate() {
                let Some((chosen, p, c, r)) = answers.get(&format!("g{g}")).and_then(choice) else {
                    bail!("the decision model gave no choice");
                };
                let i: usize = chosen
                    .trim_start_matches('e')
                    .parse()
                    .context("the model chose an unknown option")?;
                winners.push((i, &candidates[i], p, c, r));
            }
        }
        if winners.len() == 1 || groups.len() == 1 {
            let (index, _, probability, confidence, runner_up) = winners[0];
            return Ok(Pick {
                index,
                probability,
                confidence,
                runner_up,
            });
        }
        round = winners.into_iter().map(|(i, c, _, _, _)| (i, c)).collect();
    }
}

/// How sure the yes/no check must be that an element is exactly the one.
const VERIFY_MIN: f64 = 0.6;

/// Asks whether one element is exactly what a step means, details and all.
/// Plain text and no screenshot: with the picture and a structured state,
/// Clef answered 0.11 for a right element it rates 0.97 asked this way.
fn verify(
    config: &RecipeConfig,
    _app: &str,
    step: &str,
    element: &str,
    _shot: Option<&Vec<u8>>,
) -> Result<f64> {
    let state = Value::String(format!("Instruction: {step}\nCandidate element: {element}"));
    let mut questions = Map::new();
    questions.insert(
        "exact".into(),
        json!({
            "type": "noul",
            "instructions": "Is the candidate element the one the instruction describes, with the same details (times, names, numbers)?",
        }),
    );
    let answers = ask(config, &state, &questions, &[])?;
    answers
        .get("exact")
        .and_then(|a| a.get("noul").and_then(Value::as_f64).or_else(|| a.as_f64()))
        .ok_or_else(|| anyhow!("the decision model gave no yes/no answer"))
}

fn perform(
    handler: &mut dyn Handler,
    session: &str,
    window: Option<u64>,
    action: Action,
) -> Result<()> {
    handler.handle(Request::Perform {
        session: session.into(),
        window,
        action,
        observe: Observe::nothing(),
    })?;
    Ok(())
}

fn tree(handler: &mut dyn Handler, session: &str, window: Option<u64>) -> Result<UiNode> {
    match handler.handle(Request::UiTree {
        session: session.into(),
        window,
        options: TreeOptions::default(),
    })? {
        Response::UiTree(t) => Ok(t),
        _ => bail!("unexpected reply"),
    }
}

fn app_name(handler: &mut dyn Handler, session: &str) -> String {
    match handler.handle(Request::ListSessions) {
        Ok(Response::Sessions(s)) => s
            .into_iter()
            .find(|s| s.id == session)
            .map(|s| s.app)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn screenshot(handler: &mut dyn Handler, session: &str, window: Option<u64>) -> Option<Vec<u8>> {
    match handler.handle(Request::Screenshot {
        session: session.into(),
        window,
    }) {
        Ok(Response::Screenshot(s)) => Some(s.png),
        _ => None,
    }
}

/// The id given to a place found by looking rather than in the tree.
const SEEN: &str = "seen";

/// Finds a step's target, waiting a little for it to appear if the model is
/// unsure (the app may still be loading). From the accessibility tree when
/// there is one; otherwise from the screenshot alone.
fn locate(
    handler: &mut dyn Handler,
    config: &RecipeConfig,
    session: &str,
    window: Option<u64>,
    app: &str,
    target: &str,
    min_confidence: f64,
) -> Result<(UiNode, f64, String)> {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        // A window without a tree (an app whose toolkit has no AT-SPI, say)
        // leaves only the picture.
        let root = tree(handler, session, window).ok();
        let mut candidates = Vec::new();
        if let Some(root) = &root {
            collect(
                root,
                root.frame.as_ref(),
                root.name.as_deref(),
                names_window(target),
                "",
                &mut Vec::new(),
                &mut candidates,
            );
        }
        let shot = (config.screenshots || candidates.is_empty())
            .then(|| screenshot(handler, session, window))
            .flatten();
        let (node, sure, ok, how) = if !candidates.is_empty() {
            let pick = pick(config, app, target, &candidates, shot.as_ref())?;
            let c = &candidates[pick.index];
            log::info!(
                "recipe: \"{target}\" → {} (p {:.2}, confidence {:.2}) of {} candidates",
                c.description,
                pick.probability,
                pick.confidence,
                candidates.len()
            );
            // The chosen option's probability is the direct "is it this one";
            // Clef's confidence runs lower and stopped right picks.
            // A pick that is not plainly right gets a yes/no check: among
            // many near-identical rows (trains a few minutes apart) the
            // choice's odds are the same for "this is it" and "this is the
            // closest there is", but asking about the one element is not.
            let mut verified = None;
            let ok = pick.sure(min_confidence) || {
                let v = verify(config, app, target, &c.description, shot.as_ref())?;
                verified = Some(v);
                pick.probability >= 0.25 && v >= VERIFY_MIN
            };
            (
                c.node.clone(),
                verified.map_or(pick.probability, |v| v.max(pick.probability)),
                ok,
                format!(
                    "{} (choice {:.2}, next {:.2}{})",
                    c.description,
                    pick.probability,
                    pick.runner_up,
                    verified
                        .map(|v| format!(", exact match {v:.2}"))
                        .unwrap_or_default()
                ),
            )
        } else if let Some(png) = &shot {
            // Jev before 1.14 has no box questions; the grid works with any
            // model that can look.
            let (x, y, sure) = match config.provider {
                Provider::Typesafe => point_with_jev(config, app, target, png)?,
                Provider::Cloudflare => look(config, app, target, png)?,
            };
            let node = UiNode {
                id: SEEN.into(),
                role: "Point".into(),
                frame: Some(ocu_core::Rect {
                    x,
                    y,
                    width: 0.0,
                    height: 0.0,
                }),
                enabled: true,
                ..Default::default()
            };
            (
                node,
                sure,
                sure >= min_confidence,
                format!("the point ({x:.0}, {y:.0}) in the screenshot"),
            )
        } else if Instant::now() > deadline {
            bail!("the window has no accessibility tree to match \"{target}\" against, and no screenshot");
        } else {
            std::thread::sleep(Duration::from_millis(700));
            continue;
        };
        if ok {
            return Ok((node, sure, how));
        }
        if Instant::now() > deadline {
            bail!("not confident where \"{target}\" is (best: {how}, confidence {sure:.2} < {min_confidence:.2})");
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// A box Jev put a target in, in the pixels of the image it was shown.
struct Boxed {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    confidence: f64,
    /// How likely the target is there at all, when that was asked.
    there: Option<f64>,
}

/// Where Jev puts `target` in `img`, asking too whether it is there at all
/// when `present` is set.
fn ask_box(
    config: &RecipeConfig,
    app: &str,
    target: &str,
    img: &crate::vision::Image,
    about: &str,
    present: bool,
) -> Result<Boxed> {
    let (png, scale, sent) = fit_for_jev(&img.encode()?)?;
    let state = json!({ "application": app, "step": target, "image": about });
    let mut questions = Map::new();
    questions.insert(
        "where".into(),
        json!({
            "type": "box",
            "instructions": format!("The part of the interface this step means, in the picture: \"{target}\""),
        }),
    );
    // A box answer always has a box, so whether there is anything to point
    // at is its own question.
    if present {
        questions.insert(
            "there".into(),
            json!({
                "type": "noul",
                "instructions": format!("Is the part of the interface this step means visible in the picture: \"{target}\"?"),
            }),
        );
    }
    let answers = ask(config, &state, &questions, &[png])?;
    log::debug!(
        "recipe: box for \"{target}\" in {}x{}: {answers:?}",
        sent.width,
        sent.height
    );
    let a = answers
        .get("where")
        .ok_or_else(|| anyhow!("the decision model gave no box"))?;
    let edge = |k: &str| {
        a.get("box")
            .and_then(|b| b.get(k))
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("the decision model's box has no {k}"))
    };
    // Fractions of the image sent, which only differs from `img` by a scale
    // and padding at the bottom and right.
    let (w, h) = (sent.width as f64 * scale, sent.height as f64 * scale);
    let (l, r, t, b) = (
        edge("left")? * w,
        edge("right")? * w,
        edge("top")? * h,
        edge("bottom")? * h,
    );
    let confidence = a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
    let there = answers
        .get("there")
        .and_then(|a| a.get("noul").and_then(Value::as_f64).or_else(|| a.as_f64()));
    Ok(Boxed {
        x: (l + r) / 2.0,
        y: (t + b) / 2.0,
        width: r - l,
        height: b - t,
        confidence,
        there,
    })
}

/// Points at `target` in a screenshot with a box question, asked of
/// `boxes` (a Jev from 1.14). A small box is asked again on an enlarged crop
/// around it, which places it more exactly. Returns window coordinates and
/// how sure the model is, shown a dot where the click would land, that it
/// hits the target: a box's own confidence is about its edges, and stays low
/// for a button boxed exactly in a large window, while a box straddling two
/// small icons can be confident and click between them.
fn point(boxes: &RecipeConfig, app: &str, target: &str, png: &[u8]) -> Result<(f64, f64, f64)> {
    use crate::vision::{Cell, Image};
    let full = Image::decode(png)?;
    let Boxed {
        mut x,
        mut y,
        width: w,
        height: h,
        confidence,
        there,
    } = ask_box(
        boxes,
        app,
        target,
        &full,
        "The picture is the app's window.",
        true,
    )?;
    log::info!("recipe: \"{target}\" boxed at ({x:.0}, {y:.0}) {w:.0}×{h:.0}, box confidence {confidence:.2}, there {there:.2?}");
    // A square of `side` around (x, y), inside the window.
    let around = |x: f64, y: f64, side: usize| Cell {
        x: (x as usize)
            .saturating_sub(side / 2)
            .min(full.width.saturating_sub(side)),
        y: (y as usize)
            .saturating_sub(side / 2)
            .min(full.height.saturating_sub(side)),
        width: side.min(full.width),
        height: side.min(full.height),
    };
    let longest = full.width.max(full.height);
    if w.max(h) * 8.0 < longest as f64 && longest > 640 {
        let side = (w.max(h) * 4.0).max(320.0) as usize;
        let region = around(x, y, side);
        let scale = (640 / side.max(1)).clamp(1, 4);
        let closer = ask_box(
            boxes,
            app,
            target,
            &full.crop(region, scale),
            "The picture is an enlarged part of the app's window.",
            false,
        )?;
        (x, y) = (
            region.x as f64 + closer.x / scale as f64,
            region.y as f64 + closer.y / scale as f64,
        );
    }
    let mut marked = full.clone();
    marked.dot(x, y, 5.0, [255, 40, 160]);
    let side = (full.width.min(full.height) / 3).max(80);
    let scale = (640 / side).clamp(1, 4);
    let state = json!({
        "application": app,
        "step": target,
        "image": "The picture is an enlarged part of the app's window, with a pink dot where the step would click.",
    });
    let mut questions = Map::new();
    questions.insert(
        "hit".into(),
        json!({
            "type": "noul",
            "instructions": format!("Would clicking at the pink dot hit the part of the interface this step means: \"{target}\"?"),
        }),
    );
    let answers = ask(
        boxes,
        &state,
        &questions,
        &[marked.crop(around(x, y, side), scale).encode()?],
    )?;
    let hit = answers
        .get("hit")
        .and_then(|a| a.get("noul").and_then(Value::as_f64).or_else(|| a.as_f64()))
        .ok_or_else(|| anyhow!("the decision model gave no yes/no answer"))?;
    log::info!("recipe: clicking ({x:.0}, {y:.0}) hits \"{target}\": {hit:.2}");
    Ok((x, y, hit.min(there.unwrap_or(1.0))))
}

/// The Jev that answers box questions while `jev-latest` is still 1.13.
const JEV_BOX_MODEL: &str = "jev-preview";

/// Points with a box question: from the configured Jev, or from
/// [`JEV_BOX_MODEL`] when that one has none, or else by narrowing a grid.
fn point_with_jev(
    config: &RecipeConfig,
    app: &str,
    target: &str,
    png: &[u8],
) -> Result<(f64, f64, f64)> {
    let e = match point(config, app, target, png) {
        Ok(p) => return Ok(p),
        Err(e) => e,
    };
    if config.typesafe_model != JEV_BOX_MODEL {
        log::info!(
            "recipe: no box from {} ({e:#}); asking {JEV_BOX_MODEL}",
            config.typesafe_model
        );
        let preview = RecipeConfig {
            typesafe_model: JEV_BOX_MODEL.into(),
            ..config.clone()
        };
        match point(&preview, app, target, png) {
            Ok(p) => return Ok(p),
            Err(e) => log::info!("recipe: no box from {JEV_BOX_MODEL} ({e:#})"),
        }
    }
    log::info!("recipe: narrowing a grid instead");
    look(config, app, target, png)
}

/// Points at `target` in a screenshot by elimination: a numbered 4×4 grid
/// over the window, then the chosen cell enlarged with its own grid, until
/// the cell is small enough to click. Returns window coordinates.
fn look(config: &RecipeConfig, app: &str, target: &str, png: &[u8]) -> Result<(f64, f64, f64)> {
    use crate::vision::{cells, draw_grid, Cell, Image};
    let full = Image::decode(png)?;
    let mut region = Cell {
        x: 0,
        y: 0,
        width: full.width,
        height: full.height,
    };
    let mut sure = 1.0f64;
    for level in 0..4 {
        if region.width <= 40 && region.height <= 40 {
            break;
        }
        // Enlarge small regions so the model sees them at a useful size.
        let scale = (640 / region.width.max(region.height).max(1)).clamp(1, 6);
        let mut view = full.crop(region, scale);
        let grid = cells(view.width, view.height, 4, 4);
        draw_grid(&mut view, &grid);
        let mut images = Vec::new();
        let mut state = json!({ "application": app, "step": target });
        if level == 0 {
            state["image"] =
                json!("The image is the app's window, divided into 16 numbered cells.");
        } else {
            let mut context = full.clone();
            context.outline(region, 3, [255, 40, 160]);
            images.push(context.encode()?);
            state["images"] = json!("The first image is the whole window with a region outlined; the second is that region enlarged and divided into 16 numbered cells.");
        }
        images.push(view.encode()?);
        let criteria: Map<String, Value> = (1..=16)
            .map(|n| (format!("c{n}"), Value::String(format!("Cell number {n}"))))
            .collect();
        let mut questions = Map::new();
        questions.insert(
            "cell".into(),
            json!({
                "type": "choice",
                "instructions": format!("Which numbered cell contains the part of the interface this step means: \"{target}\"?"),
                "criteria": criteria,
            }),
        );
        let answers = ask(config, &state, &questions, &images)?;
        log::debug!(
            "recipe: grid level {level} for \"{target}\": {:?}",
            answers.get("cell")
        );
        let (chosen, p, c, _) = answers
            .get("cell")
            .and_then(choice)
            .ok_or_else(|| anyhow!("the decision model gave no cell"))?;
        let n: usize = chosen
            .trim_start_matches('c')
            .parse()
            .context("the model chose an unknown cell")?;
        let cell = grid
            .get(n.wrapping_sub(1))
            .ok_or_else(|| anyhow!("the model chose cell {n}"))?;
        sure = sure.min(c.min(p));
        // Back from the enlarged view to the window.
        region = Cell {
            x: region.x + cell.x / scale,
            y: region.y + cell.y / scale,
            width: (cell.width / scale).max(1),
            height: (cell.height / scale).max(1),
        };
    }
    let (x, y) = region.center();
    Ok((x, y, sure))
}

fn center(node: &UiNode) -> Result<(f64, f64)> {
    node.frame
        .map(|f| f.center())
        .ok_or_else(|| anyhow!("element [{}] has no position", node.id))
}

fn run_step(
    handler: &mut dyn Handler,
    config: &RecipeConfig,
    session: &str,
    window: Option<u64>,
    app: &str,
    step: &Step,
    min_confidence: f64,
) -> Result<String> {
    let found = match step.target() {
        Some(t) => Some(locate(
            handler,
            config,
            session,
            window,
            app,
            t,
            min_confidence,
        )?),
        None => None,
    };
    let el =
        |found: &Option<(UiNode, f64, String)>| found.as_ref().map(|(n, _, _)| n.clone()).unwrap();
    let note = found
        .as_ref()
        .map(|(n, c, how)| {
            if n.id == SEEN {
                format!(" → {how} ({:.0}%)", c * 100.0)
            } else {
                format!(" → [{}] {how} ({:.0}%)", n.id, c * 100.0)
            }
        })
        .unwrap_or_default();
    match step {
        Step::Click { button, count, .. } => {
            let node = el(&found);
            let press = node.actions.iter().any(|a| a == "press");
            let menu = node.actions.iter().any(|a| a == "showmenu");
            let action = match (button, count) {
                (MouseButton::Left, 1) if press => Action::ElementAction {
                    element: node.id.clone(),
                    name: None,
                },
                (MouseButton::Right, 1) if menu => Action::ElementAction {
                    element: node.id.clone(),
                    name: Some("showmenu".into()),
                },
                _ => {
                    let (x, y) = center(&node)?;
                    Action::Click {
                        x,
                        y,
                        button: *button,
                        count: *count,
                        modifiers: None,
                    }
                }
            };
            perform(handler, session, window, action)?;
        }
        Step::Hover { .. } => {
            let (x, y) = center(&el(&found))?;
            perform(handler, session, window, Action::MoveMouse { x, y })?;
        }
        Step::Type { text, .. } => {
            match &found {
                Some((node, _, _)) if node.id == SEEN => {
                    let (x, y) = center(node)?;
                    perform(
                        handler,
                        session,
                        window,
                        Action::Click {
                            x,
                            y,
                            button: MouseButton::Left,
                            count: 1,
                            modifiers: None,
                        },
                    )?;
                }
                Some((node, _, _)) => perform(
                    handler,
                    session,
                    window,
                    Action::Focus {
                        element: node.id.clone(),
                    },
                )?,
                None => {}
            }
            // "Type X into Y" means Y should say X: clear what a field
            // already holds (a remembered search, say) rather than add to it.
            // End and backspaces, not select-all: ⌘A is a menu shortcut some
            // apps only honour while active.
            if let Some(len) = found
                .as_ref()
                .and_then(|(n, _, _)| n.value.as_ref())
                .map(|v| v.chars().count())
            {
                if len > 0 {
                    let keys = std::iter::once("end")
                        .chain(std::iter::repeat_n("backspace", len))
                        .collect::<Vec<_>>()
                        .join(" ");
                    perform(handler, session, window, Action::PressKey { keys })?;
                }
            }
            perform(
                handler,
                session,
                window,
                Action::TypeText { text: text.clone() },
            )?;
        }
        Step::SetValue { value, .. } => {
            if el(&found).id == SEEN {
                bail!("setting a value needs an element from the accessibility tree; use a type step instead");
            }
            perform(
                handler,
                session,
                window,
                Action::SetValue {
                    element: el(&found).id,
                    value: value.clone(),
                },
            )?;
        }
        Step::Press { keys } => perform(
            handler,
            session,
            window,
            Action::PressKey { keys: keys.clone() },
        )?,
        Step::Scroll { dy, .. } => {
            let (x, y) = match &found {
                Some((node, _, _)) => center(node)?,
                None => tree(handler, session, window)?
                    .frame
                    .map(|f| (f.width / 2.0, f.height / 2.0))
                    .unwrap_or((200.0, 200.0)),
            };
            perform(
                handler,
                session,
                window,
                Action::Scroll {
                    x,
                    y,
                    dx: 0.0,
                    dy: *dy,
                },
            )?;
        }
        Step::Wait { ms } => perform(handler, session, window, Action::Wait { ms: *ms })?,
    }
    // Give the app a moment before the next step reads the tree.
    std::thread::sleep(Duration::from_millis(250));
    Ok(note)
}

pub fn run(handler: &mut dyn Handler, args: &Value) -> Result<Output> {
    let session = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing \"session_id\""))?;
    let window = args.get("window_id").and_then(Value::as_u64);
    let steps: Vec<Step> = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing \"steps\""))?
        .iter()
        .map(parse_step)
        .collect::<Result<_>>()?;
    let config = Config::load().recipe;
    if !available() {
        bail!("recipes are not set up: choose a decision model and add its API key in the OpenComputerUse settings");
    }
    let min_confidence = args
        .get("min_confidence")
        .and_then(Value::as_f64)
        .unwrap_or(config.min_confidence);
    let app = app_name(handler, session);

    let mut log = Vec::new();
    let mut failed = None;
    for (i, step) in steps.iter().enumerate() {
        match run_step(
            handler,
            &config,
            session,
            window,
            &app,
            step,
            min_confidence,
        ) {
            Ok(note) => log.push(format!("{}. ✓ {step:?}{note}", i + 1)),
            Err(e) => {
                log.push(format!("{}. ✗ {step:?}: {e:#}", i + 1));
                failed = Some(i);
                break;
            }
        }
    }
    let mut text = match failed {
        None => format!(
            "Recipe finished: {} of {} steps.\n",
            steps.len(),
            steps.len()
        ),
        Some(i) => format!(
            "Recipe stopped at step {} of {}; the steps after it did not run.\n",
            i + 1,
            steps.len()
        ),
    };
    text.push_str(&log.join("\n"));
    let image = match handler.handle(Request::Screenshot {
        session: session.into(),
        window,
    }) {
        Ok(Response::Screenshot(s)) => Some(s),
        _ => None,
    };
    if failed.is_some() {
        if let Ok(t) = tree(handler, session, window) {
            text.push_str("\n\nAccessibility tree now:\n");
            text.push_str(&t.render());
        }
    }
    Ok(Output { text, image })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steps() {
        assert!(matches!(
            parse_text("click the search box").unwrap(),
            Step::Click { count: 1, .. }
        ));
        assert!(matches!(
            parse_text("Double-click the file").unwrap(),
            Step::Click { count: 2, .. }
        ));
        match parse_text("type \"hello world\" into the search field").unwrap() {
            Step::Type { text, into } => {
                assert_eq!(text, "hello world");
                assert_eq!(into.as_deref(), Some("the search field"));
            }
            s => panic!("{s:?}"),
        }
        assert!(matches!(
            parse_text("type 'x'").unwrap(),
            Step::Type { into: None, .. }
        ));
        assert!(matches!(
            parse_text("set the volume slider to \"40\"").unwrap(),
            Step::SetValue { .. }
        ));
        assert!(matches!(
            parse_text("wait 2s").unwrap(),
            Step::Wait { ms: 2000 }
        ));
        assert!(matches!(
            parse_text("wait 300ms").unwrap(),
            Step::Wait { ms: 300 }
        ));
        assert!(matches!(
            parse_text("scroll down in the results list").unwrap(),
            Step::Scroll {
                within: Some(_),
                ..
            }
        ));
        assert!(parse_text("think about it").is_err());
        assert!(matches!(
            parse_step(&json!({"type": "a", "into": "b"})).unwrap(),
            Step::Type { .. }
        ));
    }

    #[test]
    fn window_buttons_only_for_window_steps() {
        let button = |name: &str| UiNode {
            role: "Button".into(),
            name: Some(name.into()),
            frame: Some(ocu_core::Rect {
                x: 0.0,
                y: 0.0,
                width: 14.0,
                height: 14.0,
            }),
            actions: vec!["press".into()],
            enabled: true,
            ..Default::default()
        };
        let root = UiNode {
            role: "Window".into(),
            name: Some("Mozilla Firefox".into()),
            children: vec![
                button("CloseButton"),
                button("Reject non-essential cookies"),
            ],
            ..Default::default()
        };
        let names = |target: &str| {
            let mut out = Vec::new();
            collect(
                &root,
                None,
                root.name.as_deref(),
                names_window(target),
                "",
                &mut Vec::new(),
                &mut out,
            );
            out.iter()
                .map(|c| c.node.name.clone().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names("Reject non-essential cookies button"),
            ["Reject non-essential cookies"]
        );
        assert_eq!(
            names("the window's close button"),
            ["CloseButton", "Reject non-essential cookies"]
        );
        let title_bar = UiNode {
            role: "TitleBar".into(),
            children: vec![button("Close")],
            ..Default::default()
        };
        assert!(window_control(&title_bar.children[0], "TitleBar"));
    }

    fn blank(width: usize, height: usize) -> crate::vision::Image {
        crate::vision::Image {
            width,
            height,
            rgb: vec![245; width * height * 3],
        }
    }

    #[test]
    fn images_fit_jev_bounds() {
        // A wide, short window: shrunk to 1568 across, then padded to 280 high.
        let (png, scale, sent) = fit_for_jev(&blank(3136, 200).encode().unwrap()).unwrap();
        assert_eq!((sent.width, sent.height), (1568, 280));
        assert_eq!(scale, 2.0);
        let back = crate::vision::Image::decode(&png).unwrap();
        assert_eq!((back.width, back.height), (1568, 280));
        // One already in bounds goes as it is.
        let ok = blank(800, 600).encode().unwrap();
        let (png, scale, _) = fit_for_jev(&ok).unwrap();
        assert_eq!((png, scale), (ok, 1.0));
    }

    #[test]
    fn jev_images_are_referenced_from_the_state() {
        let mut body = json!({ "model": "jev-latest", "state": "plain", "questions": {} });
        attach_jev_images(&mut body, &[blank(300, 300).encode().unwrap()]).unwrap();
        let assets = body["assets"].as_object().unwrap();
        assert_eq!(assets.len(), 1);
        let (r, asset) = assets.iter().next().unwrap();
        assert_eq!(body["state"]["picture"], json!(r));
        assert_eq!(body["state"]["context"], json!("plain"));
        assert_eq!(asset["format"], "png");
        assert!(!asset["data_base64"].as_str().unwrap().starts_with("data:"));
    }

    /// Points at a red button drawn on a blank window, through TypeSafe's
    /// API. Needs `TYPESAFE_API_KEY`; run with `--ignored`.
    #[test]
    #[ignore]
    fn jev_points_at_a_button() {
        let Ok(key) = std::env::var("TYPESAFE_API_KEY") else {
            return;
        };
        let mut img = blank(1400, 900);
        for y in 600..640 {
            for x in 1100..1220 {
                let i = (y * img.width + x) * 3;
                img.rgb[i..i + 3].copy_from_slice(&[210, 30, 30]);
            }
        }
        let png = img.encode().unwrap();
        for model in ["jev-preview", "jev-latest"] {
            let config = RecipeConfig {
                typesafe_api_key: key.clone(),
                typesafe_model: model.into(),
                ..Default::default()
            };
            let (x, y, sure) = point_with_jev(&config, "Test", "the red button", &png).unwrap();
            eprintln!("{model}: ({x:.0}, {y:.0}) sure {sure:.2}");
            assert!(
                (1100.0..1220.0).contains(&x) && (600.0..640.0).contains(&y),
                "{model}: ({x}, {y})"
            );
        }
    }

    /// Rows with long descriptions, `target` among them at `at`.
    fn long_rows(count: usize, words: usize, at: usize) -> Vec<String> {
        (0..count)
            .map(|i| {
                let filler: Vec<String> =
                    (0..words).map(|w| format!("note{}x{w}", i % 7)).collect();
                let name = if i == at {
                    "Invoice 4471 from Larkspur Ltd".to_string()
                } else {
                    format!("Invoice {} from Supplier {i}", 1000 + i)
                };
                format!("Row \"{name}\" ({})", filler.join(" "))
            })
            .collect()
    }

    /// Picks a row from candidate lists too long for the default context:
    /// one question past Jev's 32k (long context), several questions past
    /// its 64k (split), and, with Clef-flash's 24k, groups made smaller.
    /// Needs `TYPESAFE_API_KEY` and, for Clef, Cloudflare credentials in the
    /// settings; run with `--ignored`.
    #[test]
    #[ignore]
    fn long_choices_fit_the_context() {
        let _ = env_logger::builder().is_test(true).try_init();
        let node = UiNode::default();
        let mut configs = Vec::new();
        if let Ok(key) = std::env::var("TYPESAFE_API_KEY") {
            configs.push(RecipeConfig {
                provider: Provider::Typesafe,
                typesafe_api_key: key,
                ..Default::default()
            });
        }
        let saved = Config::load().recipe;
        if !saved.cloudflare_api_token.is_empty() {
            configs.push(RecipeConfig {
                provider: Provider::Cloudflare,
                ..saved
            });
        }
        // ~45k tokens in one question; then three groups of ~25k each.
        for (count, words, at) in [(250, 60, 173), (700, 13, 612)] {
            let rows = long_rows(count, words, at);
            let candidates: Vec<Candidate> = rows
                .iter()
                .map(|d| Candidate {
                    node: &node,
                    description: d.clone(),
                })
                .collect();
            for config in &configs {
                let p = pick(
                    config,
                    "Ledger",
                    "click the Larkspur invoice",
                    &candidates,
                    None,
                )
                .unwrap_or_else(|e| panic!("{:?} {count}: {e:#}", config.provider));
                eprintln!(
                    "{:?} {count}×{words}: e{} p {:.2}",
                    config.provider, p.index, p.probability
                );
                assert_eq!(p.index, at, "{:?} {count}", config.provider);
            }
        }
    }

    /// The red button again, found by Clef through the grid. Needs
    /// Cloudflare credentials in the settings; run with `--ignored`.
    /// (Clef-omni can't read the grid's numbers, and fails this.)
    #[test]
    #[ignore]
    fn clef_points_at_a_button() {
        let _ = env_logger::builder().is_test(true).try_init();
        let saved = Config::load().recipe;
        if saved.cloudflare_api_token.is_empty() {
            return;
        }
        let config = RecipeConfig {
            provider: Provider::Cloudflare,
            cloudflare_model: "@cf/cloudflare/clef-flash".into(),
            ..saved
        };
        let mut img = blank(1400, 900);
        for y in 600..640 {
            for x in 1100..1220 {
                let i = (y * img.width + x) * 3;
                img.rgb[i..i + 3].copy_from_slice(&[210, 30, 30]);
            }
        }
        let (x, y, sure) = look(&config, "Test", "the red button", &img.encode().unwrap()).unwrap();
        eprintln!("clef-flash: ({x:.0}, {y:.0}) sure {sure:.2}");
        assert!(
            (1100.0..1220.0).contains(&x) && (600.0..640.0).contains(&y),
            "({x}, {y})"
        );
    }
}
