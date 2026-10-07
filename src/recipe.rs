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

/// Whether recipes can run here: a decision model is set up that can find
/// targets on this platform. Without accessibility trees (Linux, for now)
/// only Clef, which reads screenshots, can.
pub fn available() -> bool {
    let r = Config::load().recipe;
    r.is_configured() && (!cfg!(target_os = "linux") || r.provider == Provider::Cloudflare)
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
    Click { target: String, button: MouseButton, count: u32 },
    Hover { target: String },
    Type { text: String, into: Option<String> },
    SetValue { value: String, on: String },
    Press { keys: String },
    Scroll { dy: f64, within: Option<String> },
    Wait { ms: u64 },
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
    let q = s.chars().next().filter(|c| matches!(c, '"' | '\'' | '“' | '‘'))?;
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
        return Ok(Step::Click { target: rest.into(), button: MouseButton::Left, count: 2 });
    }
    if let Some(rest) = strip_prefix_ci(s, &["right click ", "right-click ", "rightclick "]) {
        return Ok(Step::Click { target: rest.into(), button: MouseButton::Right, count: 1 });
    }
    if let Some(rest) = strip_prefix_ci(s, &["click on ", "click ", "tap on ", "tap "]) {
        return Ok(Step::Click { target: rest.into(), button: MouseButton::Left, count: 1 });
    }
    if let Some(rest) = strip_prefix_ci(s, &["hover over ", "hover "]) {
        return Ok(Step::Hover { target: rest.into() });
    }
    if let Some(rest) = strip_prefix_ci(s, &["type ", "enter "]) {
        let (text, after) = quoted(rest).ok_or_else(|| anyhow!("put the text to type in quotes: {s}"))?;
        let into = strip_prefix_ci(after.trim(), &["into ", "in "]).map(str::to_string);
        return Ok(Step::Type { text, into });
    }
    if let Some(rest) = strip_prefix_ci(s, &["set "]) {
        let lower = rest.to_lowercase();
        let at = lower.rfind(" to ").ok_or_else(|| anyhow!("write \"set <target> to \\\"<value>\\\"\": {s}"))?;
        let (value, _) = quoted(&rest[at + 4..]).ok_or_else(|| anyhow!("put the value in quotes: {s}"))?;
        return Ok(Step::SetValue { value, on: rest[..at].trim().into() });
    }
    if let Some(rest) = strip_prefix_ci(s, &["press "]) {
        return Ok(Step::Press { keys: rest.into() });
    }
    if let Some(rest) = strip_prefix_ci(s, &["scroll "]) {
        let (dir, within) = match rest.split_once(' ') {
            Some((d, w)) => (d, strip_prefix_ci(w, &["in ", "within "]).map(str::to_string)),
            None => (rest, None),
        };
        return Ok(Step::Scroll { dy: scroll_amount(dir)?, within });
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
    let o: StepObject = serde_json::from_value(v.clone()).with_context(|| format!("reading step {v}"))?;
    Ok(if let Some(t) = o.click {
        Step::Click { target: t, button: MouseButton::Left, count: 1 }
    } else if let Some(t) = o.double_click {
        Step::Click { target: t, button: MouseButton::Left, count: 2 }
    } else if let Some(t) = o.right_click {
        Step::Click { target: t, button: MouseButton::Right, count: 1 }
    } else if let Some(t) = o.hover {
        Step::Hover { target: t }
    } else if let Some(text) = o.type_ {
        Step::Type { text, into: o.into }
    } else if let Some(value) = o.set_value {
        Step::SetValue { value, on: o.on.ok_or_else(|| anyhow!("set_value needs \"on\": the target"))? }
    } else if let Some(keys) = o.press {
        Step::Press { keys }
    } else if let Some(dir) = o.scroll {
        Step::Scroll { dy: scroll_amount(&dir)?, within: o.in_ }
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
    "TextField", "TextArea", "SearchField", "ComboBox", "Link", "Button", "CheckBox", "RadioButton", "PopUpButton",
    "MenuItem", "MenuButton", "Tab", "TabItem", "Row", "Cell", "Slider", "Edit", "Hyperlink", "ListItem", "TreeItem",
    "Incrementor", "DisclosureTriangle", "ColorWell", "SplitButton", "DataItem",
    "entry", "push button", "link",
];

/// Roles that are never what a step means, whatever actions they claim:
/// web content offers "press" on every node, text and containers included,
/// and offering hundreds of them spreads the model's probability so thin a
/// right answer looks unsure.
const PASSIVE: &[&str] = &[
    "StaticText", "Text", "Heading", "Group", "Image", "ListMarker", "Splitter", "ScrollArea", "ScrollBar",
    "WebArea", "Toolbar", "TabGroup", "Window", "RadioGroup", "List", "Pane", "Document", "Separator", "Unknown",
    "LayoutArea", "LayoutItem", "Ruler", "RulerMarker", "Application", "Matte", "ValueIndicator", "TitleBar",
];

/// Whether a frame is on the window and big enough to click.
fn visible(f: &ocu_core::Rect, window: Option<&ocu_core::Rect>) -> bool {
    f.width > 2.0
        && f.height > 2.0
        && window.is_none_or(|w| f.x + f.width > 0.0 && f.y + f.height > 0.0 && f.x < w.width && f.y < w.height)
}

/// `title` is the window's, so containers named after it can be left out
/// of candidates' paths.
fn collect<'a>(
    node: &'a UiNode,
    window: Option<&ocu_core::Rect>,
    title: Option<&str>,
    parent_role: &str,
    path: &mut Vec<String>,
    out: &mut Vec<Candidate<'a>>,
) {
    let label = node.name.clone().or_else(|| node.description.clone());
    // Chrome reports a listbox's options (autocomplete suggestions, say) as
    // plain text inside a list; there they are choices, not prose.
    let option = matches!(parent_role, "List" | "ListBox" | "Menu") && label.is_some();
    let passive = PASSIVE.iter().any(|r| r.eq_ignore_ascii_case(&node.role)) && !option;
    let targetable = TARGETABLE.iter().any(|r| r.eq_ignore_ascii_case(&node.role)) || (!node.actions.is_empty() && !passive);
    if targetable && node.frame.is_some_and(|f| visible(&f, window)) && node.enabled {
        let mut d = if option && node.role == "StaticText" { "Option".to_string() } else { node.role.clone() };
        if let Some(l) = &label {
            d.push_str(&format!(" \"{l}\""));
        }
        if let Some(desc) = node.description.as_ref().filter(|s| Some(*s) != label.as_ref()) {
            d.push_str(&format!(" ({desc})"));
        }
        if let Some(v) = node.value.as_ref().filter(|v| !v.is_empty()) {
            let v: String = v.chars().take(60).collect();
            d.push_str(&format!(" containing \"{v}\""));
        }
        if node.focused {
            d.push_str(", focused");
        }
        if !path.is_empty() {
            d.push_str(&format!(", inside {}", path.join(" › ")));
        }
        out.push(Candidate { node, description: d });
    }
    // Only containers that tell candidates apart go in the path: the window,
    // the page and the groups titled like them are the same for everything.
    let distinctive = !matches!(node.role.as_str(), "Window" | "WebArea" | "ScrollArea" | "Application")
        && label.as_deref().is_some_and(|l| !title.is_some_and(|t| t.starts_with(l.trim_end_matches('…'))));
    let pushed = label.as_ref().filter(|_| distinctive).map(|l| {
        path.push(format!("{} \"{}\"", node.role, l.chars().take(40).collect::<String>()));
    });
    for c in &node.children {
        collect(c, window, title, &node.role, path, out);
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
    let Ok(img) = crate::vision::Image::decode(png) else { return png.to_vec() };
    let mut side = img.width.max(img.height).min(1024);
    loop {
        side = side * 3 / 4;
        let Ok(small) = img.shrink(side).encode() else { return png.to_vec() };
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

/// One decision-model call: questions in, `answers` out. `images` (PNG)
/// go to Clef, which can look; Jev reads text only and never gets them.
fn ask(config: &RecipeConfig, state: &Value, questions: &Map<String, Value>, images: &[Vec<u8>]) -> Result<Map<String, Value>> {
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
            (
                "https://api.typesafe.ai/v1/systemone".to_string(),
                &config.typesafe_api_key,
                json!({ "model": config.typesafe_model, "state": state, "questions": questions }),
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
                format!("https://api.cloudflare.com/client/v4/accounts/{}/ai/run/{model}", config.cloudflare_account_id),
                &config.cloudflare_api_token,
                body,
            )
        }
    };
    let mut resp = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(&body)
        .with_context(|| format!("calling {}", config.provider.label()))?;
    let status = resp.status();
    let reply: Value = resp.body_mut().read_json().context("reading the decision model's reply")?;
    if !status.is_success() {
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
    /// Sure enough to act: likely enough outright, or a clear winner (twice
    /// as likely as anything else), which a model spread over many similar
    /// options often is without passing a fixed bar.
    fn sure(&self, min: f64) -> bool {
        self.probability >= min || (self.probability >= 0.3 && self.probability >= 2.0 * self.runner_up)
    }
}

/// The chosen option, its probability, Clef's confidence, and the runner-up's
/// probability.
fn choice(answer: &Value) -> Option<(String, f64, f64, f64)> {
    let chosen = answer.get("choice")?.as_str()?.to_string();
    let probs = answer.get("probabilities").and_then(Value::as_object);
    let p = probs.and_then(|m| m.get(&chosen)).and_then(Value::as_f64).unwrap_or(0.0);
    let runner_up = probs
        .map(|m| m.iter().filter(|(k, _)| **k != chosen).filter_map(|(_, v)| v.as_f64()).fold(0.0, f64::max))
        .unwrap_or(0.0);
    let c = answer.get("confidence").and_then(Value::as_f64).unwrap_or(p);
    Some((chosen, p, c, runner_up))
}

/// Asks which candidate a step means. Past 255 candidates they are asked in
/// groups, then the groups' winners against each other.
fn pick(config: &RecipeConfig, app: &str, instruction: &str, candidates: &[Candidate], shot: Option<&Vec<u8>>) -> Result<Pick> {
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
        state["image"] = json!("The image is the app's window. Element positions are in its pixels.");
    }
    let images: Vec<Vec<u8>> = shot.cloned().into_iter().collect();
    let indexed: Vec<(usize, &Candidate)> = candidates.iter().enumerate().collect();
    let mut round: Vec<(usize, &Candidate)> = indexed;
    loop {
        if round.len() == 1 {
            return Ok(Pick { index: round[0].0, probability: 1.0, confidence: 1.0, runner_up: 0.0 });
        }
        let groups: Vec<&[(usize, &Candidate)]> = round.chunks(MAX_OPTIONS).collect();
        let mut winners: Vec<(usize, &Candidate, f64, f64, f64)> = Vec::new();
        for batch in groups.chunks(MAX_QUESTIONS) {
            let questions: Map<String, Value> =
                batch.iter().enumerate().map(|(g, opts)| (format!("g{g}"), question(opts))).collect();
            let answers = ask(config, &state, &questions, &images)?;
            for (g, _) in batch.iter().enumerate() {
                let Some((chosen, p, c, r)) = answers.get(&format!("g{g}")).and_then(choice) else {
                    bail!("the decision model gave no choice");
                };
                let i: usize = chosen.trim_start_matches('e').parse().context("the model chose an unknown option")?;
                winners.push((i, &candidates[i], p, c, r));
            }
        }
        if winners.len() == 1 || groups.len() == 1 {
            let (index, _, probability, confidence, runner_up) = winners[0];
            return Ok(Pick { index, probability, confidence, runner_up });
        }
        round = winners.into_iter().map(|(i, c, _, _, _)| (i, c)).collect();
    }
}

fn perform(handler: &mut dyn Handler, session: &str, window: Option<u64>, action: Action) -> Result<()> {
    handler.handle(Request::Perform { session: session.into(), window, action, observe: Observe::nothing() })?;
    Ok(())
}

fn tree(handler: &mut dyn Handler, session: &str, window: Option<u64>) -> Result<UiNode> {
    match handler.handle(Request::UiTree { session: session.into(), window, options: TreeOptions::default() })? {
        Response::UiTree(t) => Ok(t),
        _ => bail!("unexpected reply"),
    }
}

fn app_name(handler: &mut dyn Handler, session: &str) -> String {
    match handler.handle(Request::ListSessions) {
        Ok(Response::Sessions(s)) => s.into_iter().find(|s| s.id == session).map(|s| s.app).unwrap_or_default(),
        _ => String::new(),
    }
}

fn screenshot(handler: &mut dyn Handler, session: &str, window: Option<u64>) -> Option<Vec<u8>> {
    match handler.handle(Request::Screenshot { session: session.into(), window }) {
        Ok(Response::Screenshot(s)) => Some(s.png),
        _ => None,
    }
}

/// The id given to a place found by looking rather than in the tree.
const SEEN: &str = "seen";

/// Finds a step's target, waiting a little for it to appear if the model is
/// unsure (the app may still be loading). From the accessibility tree when
/// there is one; otherwise, with Clef, from the screenshot alone.
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
    let vision = config.provider == Provider::Cloudflare;
    loop {
        // Backends without a tree (Linux, for now) leave only the picture.
        let root = tree(handler, session, window).ok();
        let mut candidates = Vec::new();
        if let Some(root) = &root {
            collect(root, root.frame.as_ref(), root.name.as_deref(), "", &mut Vec::new(), &mut candidates);
        }
        let shot = (vision && (config.cloudflare_screenshots || candidates.is_empty()))
            .then(|| screenshot(handler, session, window))
            .flatten();
        let (node, sure, ok, how) = if !candidates.is_empty() {
            let pick = pick(config, app, target, &candidates, shot.as_ref())?;
            let c = &candidates[pick.index];
            log::info!(
                "recipe: \"{target}\" → {} (p {:.2}, confidence {:.2}) of {} candidates",
                c.description, pick.probability, pick.confidence, candidates.len()
            );
            // The chosen option's probability is the direct "is it this one";
            // Clef's confidence runs lower and stopped right picks.
            let ok = pick.sure(min_confidence);
            (
                c.node.clone(),
                pick.probability,
                ok,
                format!("{} (confidence {:.2}, next {:.2})", c.description, pick.confidence, pick.runner_up),
            )
        } else if let Some(png) = &shot {
            let (x, y, sure) = look(config, app, target, png)?;
            let node = UiNode {
                id: SEEN.into(),
                role: "Point".into(),
                frame: Some(ocu_core::Rect { x, y, width: 0.0, height: 0.0 }),
                enabled: true,
                ..Default::default()
            };
            (node, sure, sure >= min_confidence, format!("the point ({x:.0}, {y:.0}) in the screenshot"))
        } else if Instant::now() > deadline {
            bail!(
                "the window has no accessibility tree to match \"{target}\" against{}",
                if vision { "" } else { "; Cloudflare's Clef can work from screenshots instead (choose it in the settings)" }
            );
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

/// Points at `target` in a screenshot by elimination: a numbered 4×4 grid
/// over the window, then the chosen cell enlarged with its own grid, until
/// the cell is small enough to click. Returns window coordinates.
fn look(config: &RecipeConfig, app: &str, target: &str, png: &[u8]) -> Result<(f64, f64, f64)> {
    use crate::vision::{cells, draw_grid, Cell, Image};
    let full = Image::decode(png)?;
    let mut region = Cell { x: 0, y: 0, width: full.width, height: full.height };
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
            state["image"] = json!("The image is the app's window, divided into 16 numbered cells.");
        } else {
            let mut context = full.clone();
            context.outline(region, 3, [255, 40, 160]);
            images.push(context.encode()?);
            state["images"] = json!("The first image is the whole window with a region outlined; the second is that region enlarged and divided into 16 numbered cells.");
        }
        images.push(view.encode()?);
        let criteria: Map<String, Value> =
            (1..=16).map(|n| (format!("c{n}"), Value::String(format!("Cell number {n}")))).collect();
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
        let (chosen, p, c, _) = answers.get("cell").and_then(choice).ok_or_else(|| anyhow!("the decision model gave no cell"))?;
        let n: usize = chosen.trim_start_matches('c').parse().context("the model chose an unknown cell")?;
        let cell = grid.get(n.wrapping_sub(1)).ok_or_else(|| anyhow!("the model chose cell {n}"))?;
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
    node.frame.map(|f| f.center()).ok_or_else(|| anyhow!("element [{}] has no position", node.id))
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
        Some(t) => Some(locate(handler, config, session, window, app, t, min_confidence)?),
        None => None,
    };
    let el = |found: &Option<(UiNode, f64, String)>| found.as_ref().map(|(n, _, _)| n.clone()).unwrap();
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
                (MouseButton::Left, 1) if press => Action::ElementAction { element: node.id.clone(), name: None },
                (MouseButton::Right, 1) if menu => Action::ElementAction { element: node.id.clone(), name: Some("showmenu".into()) },
                _ => {
                    let (x, y) = center(&node)?;
                    Action::Click { x, y, button: *button, count: *count, modifiers: None }
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
                    perform(handler, session, window, Action::Click { x, y, button: MouseButton::Left, count: 1, modifiers: None })?;
                }
                Some((node, _, _)) => perform(handler, session, window, Action::Focus { element: node.id.clone() })?,
                None => {}
            }
            perform(handler, session, window, Action::TypeText { text: text.clone() })?;
        }
        Step::SetValue { value, .. } => {
            if el(&found).id == SEEN {
                bail!("setting a value needs an element from the accessibility tree; use a type step instead");
            }
            perform(handler, session, window, Action::SetValue { element: el(&found).id, value: value.clone() })?;
        }
        Step::Press { keys } => perform(handler, session, window, Action::PressKey { keys: keys.clone() })?,
        Step::Scroll { dy, .. } => {
            let (x, y) = match &found {
                Some((node, _, _)) => center(node)?,
                None => tree(handler, session, window)?.frame.map(|f| (f.width / 2.0, f.height / 2.0)).unwrap_or((200.0, 200.0)),
            };
            perform(handler, session, window, Action::Scroll { x, y, dx: 0.0, dy: *dy })?;
        }
        Step::Wait { ms } => perform(handler, session, window, Action::Wait { ms: *ms })?,
    }
    // Give the app a moment before the next step reads the tree.
    std::thread::sleep(Duration::from_millis(250));
    Ok(note)
}

pub fn run(handler: &mut dyn Handler, args: &Value) -> Result<Output> {
    let session = args.get("session_id").and_then(Value::as_str).ok_or_else(|| anyhow!("missing \"session_id\""))?;
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
    let min_confidence = args.get("min_confidence").and_then(Value::as_f64).unwrap_or(config.min_confidence);
    let app = app_name(handler, session);

    let mut log = Vec::new();
    let mut failed = None;
    for (i, step) in steps.iter().enumerate() {
        match run_step(handler, &config, session, window, &app, step, min_confidence) {
            Ok(note) => log.push(format!("{}. ✓ {step:?}{note}", i + 1)),
            Err(e) => {
                log.push(format!("{}. ✗ {step:?}: {e:#}", i + 1));
                failed = Some(i);
                break;
            }
        }
    }
    let mut text = match failed {
        None => format!("Recipe finished: {} of {} steps.\n", steps.len(), steps.len()),
        Some(i) => format!("Recipe stopped at step {} of {}; the steps after it did not run.\n", i + 1, steps.len()),
    };
    text.push_str(&log.join("\n"));
    let image = match handler.handle(Request::Screenshot { session: session.into(), window }) {
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
        assert!(matches!(parse_text("click the search box").unwrap(), Step::Click { count: 1, .. }));
        assert!(matches!(parse_text("Double-click the file").unwrap(), Step::Click { count: 2, .. }));
        match parse_text("type \"hello world\" into the search field").unwrap() {
            Step::Type { text, into } => {
                assert_eq!(text, "hello world");
                assert_eq!(into.as_deref(), Some("the search field"));
            }
            s => panic!("{s:?}"),
        }
        assert!(matches!(parse_text("type 'x'").unwrap(), Step::Type { into: None, .. }));
        assert!(matches!(parse_text("set the volume slider to \"40\"").unwrap(), Step::SetValue { .. }));
        assert!(matches!(parse_text("wait 2s").unwrap(), Step::Wait { ms: 2000 }));
        assert!(matches!(parse_text("wait 300ms").unwrap(), Step::Wait { ms: 300 }));
        assert!(matches!(parse_text("scroll down in the results list").unwrap(), Step::Scroll { within: Some(_), .. }));
        assert!(parse_text("think about it").is_err());
        assert!(matches!(parse_step(&json!({"type": "a", "into": "b"})).unwrap(), Step::Type { .. }));
    }
}
