//! Extraction: typed values read from a window by Cloudflare's Clairic
//! extraction model. The window's accessibility tree is the state Clairic
//! reads and its screenshot an image beside it; each field asked for comes
//! back as a value of its type (text, number, date, list, estimate…) with a
//! confidence, or null when the window does not show it. Nothing is
//! generated. A text answer is a span of the tree, so it also names the
//! element whose line it came from, ready for click or set_value.

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Map, Value};

use ocu_core::{Handler, Request, Response, TreeOptions};

use crate::config::{Config, ExtractConfig, RecipeConfig};
use crate::tools::Output;

/// Clairic's field types.
pub const FIELD_TYPES: &[&str] = &[
    "text", "number", "date", "time", "datetime", "duration", "list", "estimate", "forecast",
];

/// Clairic takes at most four images.
const MAX_IMAGES: usize = 4;

/// Whether the tool is offered: turned on, with a Cloudflare account to call.
pub fn available() -> bool {
    let config = Config::load();
    config.extract.enabled && has_credentials(&config.recipe)
}

/// Fails, saying where to add them, without a Cloudflare account to call.
pub fn require_credentials(r: &RecipeConfig) -> Result<()> {
    if !has_credentials(r) {
        bail!("no Cloudflare account id or API token; add them in the OpenComputerUse settings (or set CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN)");
    }
    Ok(())
}

fn has_credentials(r: &RecipeConfig) -> bool {
    !r.cloudflare_account_id.trim().is_empty() && !r.cloudflare_api_token.trim().is_empty()
}

pub fn tool_definition() -> Value {
    json!({
        "name": "extract",
        "description": "Read typed values from a session's window in one fast call, without reading the tree or screenshot yourself. Cloudflare's Clairic extraction model reads the window's accessibility tree together with its screenshot, and returns one value per field you define: its type (text, number, date, time, datetime, duration, list, estimate, forecast), a confidence, and p_null (the chance the window doesn't show it); null when it doesn't. Use it to pull values out of a busy window (a total, a date, every row's name, how many unread items) or to find an element: a text answer is a span of the tree, so it comes with `element`, the id of the line it came from (lists come with `elements`), ready for click, set_value and element_action. Values are well formed, not necessarily right: check low-confidence ones before acting on them.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "The id start_session returned." },
                "window_id": { "type": "integer", "description": "A window id from list_windows. Defaults to the app's main window." },
                "fields": {
                    "type": "object",
                    "description": "Field name (letters, digits, _ . -) to its spec: {\"type\": ..., \"instructions\": what it should contain}. Numbers take `unit`, `integer` and `decimals`; durations a `unit` (seconds to years); lists `items` (text, number or date); estimates `min`, `max` and optionally `step` and `scale`; forecasts also `horizon` and `interval`; any field `null_threshold`. Example: {\"save_button\": {\"type\": \"text\", \"instructions\": \"The Save button's element id\"}, \"total\": {\"type\": \"number\", \"decimals\": 2, \"unit\": \"USD\"}, \"rows\": {\"type\": \"list\", \"items\": \"text\", \"instructions\": \"Each message subject\"}}.",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": FIELD_TYPES },
                            "instructions": { "type": "string" },
                        },
                        "required": ["type"],
                    },
                },
                "context": { "type": "string", "description": "More for the model to read beside the window, such as \"Today: 2026-10-19\" for fields that depend on the date." },
                "reference_date": { "type": "string", "description": "ISO date that anchors dates shown without a year." },
                "ui_tree": { "type": "boolean", "description": "Read the accessibility tree. Default true; without one, values are read from the screenshot alone." },
                "screenshot": { "type": "boolean", "description": "Send the window's screenshot with the tree. Defaults to the OpenComputerUse setting (on)." },
            },
            "required": ["session_id", "fields"],
        },
    })
}

/// What one Clairic call reads.
#[derive(Default)]
pub struct Input {
    pub state: String,
    /// PNG, JPEG or WebP bytes.
    pub images: Vec<Vec<u8>>,
    pub fields: Map<String, Value>,
    pub reference_date: Option<String>,
}

/// The `extract` tool: the window's tree and screenshot, then Clairic.
pub fn run(handler: &mut dyn Handler, args: &Value) -> Result<Output> {
    let config = Config::load();
    let session = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing \"session_id\""))?
        .to_string();
    let window = args.get("window_id").and_then(Value::as_u64);
    let flag = |k: &str, d: bool| args.get(k).and_then(Value::as_bool).unwrap_or(d);
    let fields = fields_arg(args.get("fields"))?;

    let mut state = String::new();
    if flag("ui_tree", true) {
        match handler.handle(Request::UiTree {
            session: session.clone(),
            window,
            options: TreeOptions::default(),
        }) {
            Ok(Response::UiTree(t)) => {
                let tree = t.render();
                if !tree.trim().is_empty() {
                    state = tree_state(&tree);
                }
            }
            Ok(_) => bail!("unexpected reply"),
            // Some windows have no tree; the screenshot may still do.
            Err(e) => log::info!("extract: no accessibility tree: {e:#}"),
        }
    }
    let mut images = Vec::new();
    if flag("screenshot", config.extract.screenshots) || state.is_empty() {
        match handler.handle(Request::Screenshot { session, window })? {
            Response::Screenshot(s) => images.push(s.png),
            _ => bail!("unexpected reply"),
        }
    }
    if let Some(c) = args
        .get("context")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    {
        if !state.is_empty() {
            state.push('\n');
        }
        state.push_str(c);
    }
    let input = Input {
        state,
        images,
        fields,
        reference_date: args
            .get("reference_date")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let reply = call(&config.recipe, &config.extract, &input)?;
    Ok(Output {
        text: serde_json::to_string_pretty(&summarize(&input.state, &reply))?,
        image: None,
    })
}

/// The tree as Clairic reads it: a line saying what it is, then one line
/// per element.
fn tree_state(tree: &str) -> String {
    format!(
        "Accessibility tree of an app window. One element per line: [id] role \"name\" value=\"…\" @(x,y widthxheight) in window points, actions=…; children are indented under their parent. The image is a screenshot of the same window.\n{tree}"
    )
}

/// `fields` as given: an object, or JSON text of one (some clients send
/// nested objects as strings). Each must have a known type.
pub fn fields_arg(v: Option<&Value>) -> Result<Map<String, Value>> {
    let fields = match v {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::String(s)) => match serde_json::from_str(s) {
            Ok(Value::Object(m)) => m,
            _ => bail!("\"fields\" is an object of field name to spec"),
        },
        _ => bail!("missing \"fields\": an object of field name to spec, like {{\"total\": {{\"type\": \"number\"}}}}"),
    };
    if fields.is_empty() {
        bail!("\"fields\" needs at least one field");
    }
    for (name, spec) in &fields {
        let ok_name = name.len() <= 64
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !ok_name {
            bail!("field name \"{name}\" must start with a letter or _ and hold only letters, digits, _ . and - (at most 64)");
        }
        match spec.get("type").and_then(Value::as_str) {
            Some(t) if FIELD_TYPES.contains(&t) => {}
            Some(t) => bail!(
                "field \"{name}\" has unknown type \"{t}\"; use one of {}",
                FIELD_TYPES.join(", ")
            ),
            None => bail!(
                "field \"{name}\" needs a \"type\" ({})",
                FIELD_TYPES.join(", ")
            ),
        }
    }
    Ok(fields)
}

/// One Clairic request. Returns its reply without Workers AI's `result`
/// wrapper. A request Workers AI finds too long is tried once more with
/// smaller images.
pub fn call(creds: &RecipeConfig, config: &ExtractConfig, input: &Input) -> Result<Value> {
    require_credentials(creds)?;
    if input.state.trim().is_empty() && input.images.is_empty() {
        bail!("nothing to read: the window has no accessibility tree and no screenshot was sent");
    }
    match call_once(creds, config, input, crate::recipe::MAX_IMAGE_BYTES) {
        Err(e) if e.downcast_ref::<TooLong>().is_some() && !input.images.is_empty() => {
            log::info!("extract: the request is too long; shrinking the screenshot");
            call_once(creds, config, input, crate::recipe::MAX_IMAGE_BYTES / 3)
        }
        r => r,
    }
}

#[derive(Debug)]
struct TooLong;

impl std::fmt::Display for TooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the request is longer than Clairic's context window")
    }
}

impl std::error::Error for TooLong {}

fn call_once(
    creds: &RecipeConfig,
    config: &ExtractConfig,
    input: &Input,
    max_image_bytes: usize,
) -> Result<Value> {
    let model = config.model.trim().trim_start_matches('/');
    // The body names the model by its short name: "clairic".
    let short = model.rsplit('/').next().unwrap_or(model);
    let mut body = json!({ "model": short, "state": input.state, "fields": input.fields });
    if !input.images.is_empty() {
        let images = &input.images[..input.images.len().min(MAX_IMAGES)];
        body["images"] = Value::Array(crate::recipe::encode_images_within(images, max_image_bytes));
    }
    if let Some(d) = input.reference_date.as_deref().filter(|d| !d.is_empty()) {
        body["reference_date"] = json!(d);
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/run/{model}",
        creds.cloudflare_account_id.trim()
    );
    let mut resp = agent
        .post(&url)
        .header(
            "Authorization",
            &format!("Bearer {}", creds.cloudflare_api_token.trim()),
        )
        .send_json(&body)
        .context("calling Cloudflare Workers AI (Clairic)")?;
    let status = resp.status();
    let reply: Value = resp
        .body_mut()
        .read_json()
        .context("reading Clairic's reply")?;
    if !status.is_success() {
        // As for Clef: 3030 once Workers AI has counted the tokens, 5021
        // when its estimate is already too many.
        let too_long = reply
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|e| {
                e.iter()
                    .any(|e| matches!(e.get("code").and_then(Value::as_u64), Some(3030 | 5021)))
            });
        let err = anyhow!("Clairic answered {status}: {reply}");
        return Err(if too_long { err.context(TooLong) } else { err });
    }
    let reply = reply.get("result").cloned().unwrap_or(reply);
    if !reply.get("answers").is_some_and(Value::is_object) {
        bail!("Clairic's reply has no answers: {reply}");
    }
    Ok(reply)
}

/// Clairic's reply for the caller: answers with the element each text span
/// came from in place of offsets into a state they never saw, and whether
/// the state was cut short.
pub fn summarize(state: &str, reply: &Value) -> Value {
    let mut answers = reply
        .get("answers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for answer in answers.values_mut() {
        let Some(a) = answer.as_object_mut() else {
            continue;
        };
        if let Some(span) = a.remove("span") {
            let text = a
                .get("value")
                .or_else(|| a.get("guess"))
                .and_then(Value::as_str);
            if let Some(id) = span_pair(&span).and_then(|s| element_at(state, s, text)) {
                a.insert("element".into(), json!(id));
            }
        }
        if let Some(Value::Array(spans)) = a.remove("spans") {
            let values: Vec<Option<&str>> = match a.get("value") {
                Some(Value::Array(v)) => v.iter().map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            let ids: Vec<Value> = spans
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    span_pair(s)
                        .and_then(|s| element_at(state, s, values.get(i).copied().flatten()))
                        .map_or(Value::Null, Value::String)
                })
                .collect();
            if ids.iter().any(|v| !v.is_null()) {
                a.insert("elements".into(), Value::Array(ids));
            }
        }
    }
    let mut out = json!({ "answers": answers });
    if reply.get("truncated").and_then(Value::as_bool) == Some(true) {
        out["truncated"] = json!(true);
        out["note"] = json!(
            "The window's tree was too long to read whole; elements near its end may be missing."
        );
    }
    out
}

fn span_pair(v: &Value) -> Option<(usize, usize)> {
    let a = v.as_array()?;
    Some((a.first()?.as_u64()? as usize, a.get(1)?.as_u64()? as usize))
}

/// The id of the tree line holding a span of `state`. Spans count
/// characters; when that doesn't land on `text`, byte offsets and then the
/// text itself are tried.
fn element_at(state: &str, (start, end): (usize, usize), text: Option<&str>) -> Option<String> {
    let char_byte = |n: usize| {
        state
            .char_indices()
            .map(|(i, _)| i)
            .chain([state.len()])
            .nth(n)
    };
    let by_chars = || {
        (start <= end)
            .then(|| Some((char_byte(start)?, char_byte(end)?)))
            .flatten()
    };
    let by_bytes = || {
        (start <= end
            && end <= state.len()
            && state.is_char_boundary(start)
            && state.is_char_boundary(end))
        .then_some((start, end))
    };
    let at = match text.filter(|t| !t.is_empty()) {
        Some(t) => [by_chars(), by_bytes()]
            .into_iter()
            .flatten()
            .find(|&(s, e)| &state[s..e] == t)
            .map(|(s, _)| s)
            .or_else(|| state.find(t))?,
        None => by_chars()?.0,
    };
    line_element(state, at)
}

/// The element id at the start of the line holding byte `at`.
fn line_element(state: &str, at: usize) -> Option<String> {
    let line_start = state[..at].rfind('\n').map_or(0, |i| i + 1);
    let line = state[line_start..].lines().next()?.trim_start();
    let id = line.strip_prefix('[')?.split(']').next()?;
    (!id.is_empty() && !id.contains(' ')).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "[e1] Window \"Invoice\"\n  [e2] StaticText value=\"Total: $1,080.00\"\n  [e3] Button \"Save\" @(10,20 80x24) actions=press\n";

    fn state() -> String {
        tree_state(TREE)
    }

    fn span_of(state: &str, text: &str) -> Value {
        let b = state.find(text).unwrap();
        let s = state[..b].chars().count();
        json!([s, s + text.chars().count()])
    }

    #[test]
    fn text_answers_name_their_element() {
        let state = state();
        let reply = json!({
            "answers": {
                "save": { "type": "text", "value": "Save", "span": span_of(&state, "Save\""), "confidence": 0.99 },
                "id": { "type": "text", "value": "e2", "span": span_of(&state, "e2"), "confidence": 0.9 },
                "total": { "type": "number", "value": 1080, "confidence": 0.99 },
                "names": { "type": "list", "value": ["Invoice", "Save"], "spans": [span_of(&state, "Invoice"), span_of(&state, "Save")] },
            },
            "truncated": false,
        });
        let out = summarize(&state, &reply);
        assert_eq!(out["answers"]["save"]["element"], "e3");
        assert_eq!(out["answers"]["id"]["element"], "e2");
        assert!(out["answers"]["save"].get("span").is_none());
        assert!(out["answers"]["total"].get("element").is_none());
        assert_eq!(out["answers"]["names"]["elements"], json!(["e1", "e3"]));
        assert!(out.get("truncated").is_none());
    }

    #[test]
    fn spans_past_wide_characters_still_land() {
        let tree = "[e1] Window \"Café ☕\"\n  [e2] Button \"Pay\"\n";
        let state = tree_state(tree);
        let span = span_of(&state, "Pay");
        assert_eq!(
            element_at(&state, span_pair(&span).unwrap(), Some("Pay")).as_deref(),
            Some("e2")
        );
        // Wrong offsets fall back to the text.
        assert_eq!(
            element_at(&state, (0, 3), Some("Pay")).as_deref(),
            Some("e2")
        );
        // The header line has no element.
        assert_eq!(element_at(&state, (0, 4), None), None);
    }

    #[test]
    fn fields_are_checked() {
        assert!(fields_arg(Some(&json!({ "a": { "type": "number" } }))).is_ok());
        assert!(fields_arg(Some(&json!("{\"a\": {\"type\": \"date\"}}"))).is_ok());
        assert!(fields_arg(Some(&json!({}))).is_err());
        assert!(fields_arg(Some(&json!({ "a": { "type": "colour" } }))).is_err());
        assert!(fields_arg(Some(&json!({ "a": {} }))).is_err());
        assert!(fields_arg(Some(&json!({ "1a": { "type": "text" } }))).is_err());
        assert!(fields_arg(None).is_err());
    }
}
