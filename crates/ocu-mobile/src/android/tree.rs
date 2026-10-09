//! Android's view hierarchy as a [`UiNode`] tree: from `uiautomator dump`'s
//! XML, or from the shell helper's JSON (which also says how to find each
//! node again, for accessibility actions).

use anyhow::{bail, Context as _, Result};
use quick_xml::events::Event;
use serde_json::Value;

use ocu_core::{Rect, TreeOptions, UiNode};

use crate::elements::{Element, Elements};

#[derive(Clone, Debug, Default)]
pub struct RawNode {
    pub class: String,
    pub text: String,
    pub desc: String,
    pub hint: String,
    pub res: String,
    /// left, top, right, bottom in device pixels.
    pub bounds: [f64; 4],
    pub clickable: bool,
    pub long_clickable: bool,
    pub scrollable: bool,
    pub editable: bool,
    pub checkable: bool,
    pub checked: bool,
    pub enabled: bool,
    pub focused: bool,
    pub focusable: bool,
    pub selected: bool,
    pub password: bool,
    /// Shown to the user: on screen and not covered.
    pub visible: bool,
    /// The helper's path to the node: window index, then child indexes.
    pub path: Vec<usize>,
    pub children: Vec<RawNode>,
}

/// `uiautomator dump`'s XML. Its nodes carry no path; actions on them
/// fall back to touching their frame.
pub fn parse_uiautomator(xml: &str) -> Result<Vec<RawNode>> {
    let start = xml
        .find("<hierarchy")
        .context("uiautomator printed no hierarchy")?;
    let end = xml
        .rfind("</hierarchy>")
        .map(|i| i + "</hierarchy>".len())
        .unwrap_or(xml.len());
    let mut reader = quick_xml::Reader::from_str(&xml[start..end]);
    let mut stack: Vec<RawNode> = Vec::new();
    let mut roots = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.name().as_ref() == b"node" => stack.push(xml_node(&e)?),
            Event::Empty(e) if e.name().as_ref() == b"node" => {
                let n = xml_node(&e)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(n),
                    None => roots.push(n),
                }
            }
            Event::End(e) if e.name().as_ref() == b"node" => {
                let n = stack.pop().context("unbalanced uiautomator XML")?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(n),
                    None => roots.push(n),
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(roots)
}

fn xml_node(e: &quick_xml::events::BytesStart) -> Result<RawNode> {
    let mut n = RawNode {
        enabled: true,
        // uiautomator lists only what is on screen.
        visible: true,
        ..Default::default()
    };
    for attr in e.attributes() {
        let attr = attr?;
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
            .into_owned();
        let yes = value == "true";
        match attr.key.as_ref() {
            b"class" => n.class = value,
            b"text" => n.text = value,
            b"content-desc" => n.desc = value,
            b"hint" => n.hint = value,
            b"resource-id" => n.res = value,
            b"bounds" => n.bounds = parse_bounds(&value).unwrap_or_default(),
            b"clickable" => n.clickable = yes,
            b"long-clickable" => n.long_clickable = yes,
            b"scrollable" => n.scrollable = yes,
            b"checkable" => n.checkable = yes,
            b"checked" => n.checked = yes,
            b"enabled" => n.enabled = yes,
            b"focused" => n.focused = yes,
            b"focusable" => n.focusable = yes,
            b"selected" => n.selected = yes,
            b"password" => n.password = yes,
            _ => {}
        }
    }
    n.editable = n.class.ends_with("EditText") || n.class.ends_with("AutoCompleteTextView");
    Ok(n)
}

/// "[0,63][1080,210]" → [0, 63, 1080, 210].
fn parse_bounds(s: &str) -> Option<[f64; 4]> {
    let nums: Vec<f64> = s
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    match nums[..] {
        [l, t, r, b] => Some([l, t, r, b]),
        _ => None,
    }
}

/// The helper's `tree` reply: one root per window, top window first.
pub fn parse_helper(reply: &Value) -> Result<Vec<RawNode>> {
    let Some(windows) = reply.get("windows").and_then(Value::as_array) else {
        bail!("the helper's tree has no windows");
    };
    Ok(windows
        .iter()
        .enumerate()
        .filter_map(|(i, w)| w.get("root").map(|r| json_node(r, vec![i])))
        .collect())
}

fn json_node(v: &Value, path: Vec<usize>) -> RawNode {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let bounds = v
        .get("bounds")
        .and_then(Value::as_array)
        .map(|a| {
            let n = |i: usize| a.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            [n(0), n(1), n(2), n(3)]
        })
        .unwrap_or_default();
    let children = v
        .get("children")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut p = path.clone();
                    p.push(i);
                    json_node(c, p)
                })
                .collect()
        })
        .unwrap_or_default();
    RawNode {
        class: s("class"),
        text: s("text"),
        desc: s("desc"),
        hint: s("hint"),
        res: s("res"),
        bounds,
        clickable: b("clickable"),
        long_clickable: b("longClickable"),
        scrollable: b("scrollable"),
        editable: b("editable"),
        checkable: b("checkable"),
        checked: b("checked"),
        // The helper leaves out what is false.
        enabled: b("enabled"),
        focused: b("focused"),
        focusable: b("focusable"),
        selected: b("selected"),
        password: b("password"),
        visible: b("visible"),
        path,
        children,
    }
}

impl RawNode {
    fn interactive(&self) -> bool {
        self.clickable || self.long_clickable || self.scrollable || self.editable || self.checkable
    }

    /// A layout wrapper with nothing to say or do.
    fn hollow(&self) -> bool {
        !self.interactive()
            && self.text.is_empty()
            && self.desc.is_empty()
            && self.hint.is_empty()
            && !self.focused
    }

    fn role(&self) -> String {
        let short = self.class.rsplit(['.', '$']).next().unwrap_or("View");
        if short.is_empty() {
            "View".into()
        } else {
            short.to_string()
        }
    }
}

/// Builds the session's tree: `title` names the root (the app), `scale`
/// is device pixels per point.
pub fn build(
    roots: &[RawNode],
    title: &str,
    scale: f64,
    opts: &TreeOptions,
    elements: &mut Elements,
) -> UiNode {
    let mut budget = opts.max_nodes.max(1);
    let mut root = UiNode {
        id: elements.add(Element::default()),
        role: "Screen".into(),
        name: Some(title.to_string()),
        enabled: true,
        ..Default::default()
    };
    budget -= 1;
    for r in roots {
        root.children
            .extend(convert(r, scale, 1, opts.max_depth, &mut budget, elements));
    }
    root
}

fn convert(
    n: &RawNode,
    scale: f64,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
    elements: &mut Elements,
) -> Vec<UiNode> {
    // Off screen (scrolled away, or covered): neither it nor what it holds.
    if !n.visible {
        return Vec::new();
    }
    // Hollow wrappers give way to their children.
    if n.hollow() {
        return n
            .children
            .iter()
            .flat_map(|c| convert(c, scale, depth, max_depth, budget, elements))
            .collect();
    }
    if *budget == 0 || depth > max_depth {
        return Vec::new();
    }
    *budget -= 1;
    let [l, t, r, b] = n.bounds;
    let frame = Rect {
        x: l / scale,
        y: t / scale,
        width: ((r - l) / scale).max(0.0),
        height: ((b - t) / scale).max(0.0),
    };
    let id = elements.add(Element {
        frame,
        path: n.path.clone(),
        editable: n.editable,
        scrollable: n.scrollable,
        value_len: if n.editable {
            n.text.chars().count()
        } else {
            0
        },
    });
    let mut actions = Vec::new();
    if n.clickable || n.checkable {
        actions.push("press");
    }
    if n.long_clickable {
        actions.push("longpress");
    }
    if n.editable || (n.focusable && !n.clickable) {
        actions.push("focus");
    }
    if n.scrollable {
        actions.extend(["scrollforward", "scrollbackward"]);
    }
    // A field's text is its value; its name is what it is for.
    let (name, value) = if n.editable {
        let label = [&n.desc, &n.hint, &short_res(&n.res)]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned();
        let value = if n.password && !n.text.is_empty() {
            Some("•".repeat(n.text.chars().count().min(12)))
        } else {
            Some(n.text.clone()).filter(|t| !t.is_empty())
        };
        (label, value)
    } else if n.checkable {
        let label = [&n.text, &n.desc]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned();
        (
            label,
            Some(if n.checked { "on" } else { "off" }.to_string()),
        )
    } else {
        let label = [&n.text, &n.desc]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned();
        (label, None)
    };
    let description = if !n.text.is_empty() && !n.desc.is_empty() && !n.editable {
        Some(n.desc.clone())
    } else if name.is_none() && !n.res.is_empty() {
        Some(format!("id:{}", short_res(&n.res)))
    } else {
        None
    };
    let mut node = UiNode {
        id,
        role: n.role(),
        name,
        value,
        description,
        frame: Some(frame),
        actions: actions.into_iter().map(String::from).collect(),
        enabled: n.enabled,
        focused: n.focused,
        children: Vec::new(),
    };
    for c in &n.children {
        node.children
            .extend(convert(c, scale, depth + 1, max_depth, budget, elements));
    }
    vec![node]
}

/// "com.android.settings:id/search_bar" → "search_bar".
fn short_res(res: &str) -> String {
    res.rsplit('/').next().unwrap_or(res).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"UI hierchary dumped to: /dev/tty<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="p" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][900,1600]"><node index="0" text="" class="android.widget.LinearLayout" bounds="[0,0][900,1600]"><node index="0" text="Network &amp; internet" resource-id="android:id/title" class="android.widget.TextView" content-desc="" clickable="true" enabled="true" bounds="[100,200][500,260]" /><node text="" resource-id="com.x:id/search" class="android.widget.EditText" content-desc="Search settings" clickable="true" focusable="true" focused="true" bounds="[0,10][900,90]" /></node></node></hierarchy>UI hierchary dumped to: /dev/tty"#;

    #[test]
    fn uiautomator_tree() {
        let roots = parse_uiautomator(XML).unwrap();
        assert_eq!(roots.len(), 1);
        let mut els = Elements::default();
        let tree = build(&roots, "Settings", 2.0, &TreeOptions::default(), &mut els);
        let text = tree.render();
        assert!(
            text.contains("TextView \"Network & internet\" @(50,100 200x30) actions=press"),
            "{text}"
        );
        assert!(text.contains("EditText \"Search settings\""), "{text}");
        assert!(text.contains("focused"), "{text}");
        // The hollow LinearLayout gave way to its children.
        assert!(!text.contains("LinearLayout"), "{text}");
        let e = els.get("e2").unwrap();
        assert!(e.editable);
        assert_eq!(e.frame.width, 450.0);
    }

    #[test]
    fn helper_tree_keeps_paths() {
        let v = serde_json::json!({ "windows": [ { "root": { "class": "android.widget.FrameLayout", "bounds": [0,0,100,100], "enabled": true, "visible": true, "children": [
            { "class": "android.widget.Button", "text": "OK", "clickable": true, "enabled": true, "visible": true, "bounds": [10,10,50,30] },
            { "class": "android.widget.Button", "text": "Below", "clickable": true, "enabled": true, "bounds": [10,200,50,230] },
            { "class": "android.widget.Button", "text": "Off", "clickable": true, "visible": true, "bounds": [60,10,90,30] }
        ] } } ] });
        let roots = parse_helper(&v).unwrap();
        let mut els = Elements::default();
        let tree = build(&roots, "App", 1.0, &TreeOptions::default(), &mut els);
        let button = &tree.children[0];
        assert_eq!(button.role, "Button");
        assert_eq!(els.get(&button.id).unwrap().path, vec![0, 0]);
        let text = tree.render();
        assert!(!text.contains("Below"), "{text}");
        assert!(
            text.contains("Button \"Off\" @(60,10 30x20) actions=press disabled"),
            "{text}"
        );
    }
}
