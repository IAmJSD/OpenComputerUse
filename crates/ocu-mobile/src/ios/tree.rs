//! WebDriverAgent's `/source?format=json` as a [`UiNode`] tree. Its frames
//! are already in points. Each element remembers its place in document
//! order, which is how WebDriverAgent finds it again: `(//*)[n]`.
//!
//! The source is read without WebDriverAgent's visibility check, which
//! triples the time it takes; what lies off screen is left out by its
//! frame instead.

use serde_json::Value;

use ocu_core::{Rect, TreeOptions, UiNode};

use crate::elements::{Element, Elements};

fn flag(v: &Value, key: &str) -> Option<bool> {
    match v.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => Some(s == "1" || s == "true"),
        Value::Number(n) => Some(n.as_i64() != Some(0)),
        _ => None,
    }
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn actions_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "Button" | "Cell" | "Link" | "Switch" | "Toggle" | "Tab" | "Key" | "MenuItem"
        | "SegmentedControl" | "RadioButton" | "CheckBox" | "Icon" | "PopUpButton" => &["press"],
        "TextField" | "SecureTextField" | "SearchField" | "TextView" => &["press", "focus"],
        "ScrollView" | "Table" | "CollectionView" | "WebView" | "PickerWheel" => {
            &["scrollforward", "scrollbackward"]
        }
        "Slider" | "Stepper" => &["increment", "decrement"],
        _ => &[],
    }
}

pub fn build(
    root: &Value,
    title: &str,
    screen: (f64, f64),
    opts: &TreeOptions,
    elements: &mut Elements,
) -> UiNode {
    let mut budget = opts.max_nodes.max(1);
    let mut index = 0usize;
    let mut top = UiNode {
        id: elements.add(Element::default()),
        role: "Screen".into(),
        name: Some(title.to_string()),
        enabled: true,
        ..Default::default()
    };
    budget -= 1;
    let mut walk = Walk {
        screen,
        max_depth: opts.max_depth,
        budget: &mut budget,
        index: &mut index,
        elements,
    };
    top.children = convert(root, 1, &mut walk);
    top
}

struct Walk<'a> {
    screen: (f64, f64),
    max_depth: usize,
    budget: &'a mut usize,
    index: &'a mut usize,
    elements: &'a mut Elements,
}

fn convert(v: &Value, depth: usize, walk: &mut Walk) -> Vec<UiNode> {
    let (screen, max_depth) = (walk.screen, walk.max_depth);
    // Document order counts every element, shown or not.
    *walk.index += 1;
    let my_index = *walk.index;
    let children: &[Value] = v
        .get("children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let r = v.get("rect");
    let num = |k: &str| {
        r.and_then(|r| r.get(k))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    let frame = Rect {
        x: num("x"),
        y: num("y"),
        width: num("width"),
        height: num("height"),
    };
    // Wholly off screen, it and what it holds are left out; a zero-size
    // wrapper gives way to its children.
    let on_screen = frame.x < screen.0
        && frame.y < screen.1
        && frame.x + frame.width > 0.0
        && frame.y + frame.height > 0.0;
    let empty = frame.width <= 0.0 || frame.height <= 0.0;
    let visible = flag(v, "isVisible").unwrap_or(true) && (on_screen || empty);
    let kind = text(v, "type");
    let label = text(v, "label");
    let identifier = text(v, "name");
    let value = text(v, "value");
    let focused = flag(v, "isFocused").unwrap_or(false);
    let actions = actions_for(&kind);
    let hollow = actions.is_empty()
        && label.is_empty()
        && value.is_empty()
        && !focused
        && matches!(
            kind.as_str(),
            "Other" | "Group" | "Window" | "Application" | "LayoutArea" | ""
        );
    let skip = !visible || hollow || empty || *walk.budget == 0 || depth > max_depth;
    if skip {
        // Still walk the children so later indexes stay right.
        let mut out = Vec::new();
        for c in children {
            let mut got = convert(c, depth, walk);
            if visible {
                out.append(&mut got);
            }
        }
        return out;
    }
    *walk.budget -= 1;
    let editable = matches!(
        kind.as_str(),
        "TextField" | "SecureTextField" | "SearchField" | "TextView"
    );
    let id = walk.elements.add(Element {
        frame,
        path: vec![my_index],
        editable,
        scrollable: actions.contains(&"scrollforward"),
        value_len: value.chars().count(),
    });
    let name = [&label, &identifier]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned();
    // A label's text repeated as its value says nothing new.
    let value = Some(value).filter(|s| !s.is_empty() && Some(s) != name.as_ref());
    let description = (!label.is_empty() && !identifier.is_empty() && identifier != label)
        .then(|| identifier.clone())
        .filter(|id| !id.contains(' ') || id.len() < 60);
    let mut node = UiNode {
        id,
        role: if kind.is_empty() {
            "Other".into()
        } else {
            kind
        },
        name,
        value,
        description,
        frame: Some(frame),
        actions: actions.iter().map(|s| s.to_string()).collect(),
        enabled: flag(v, "isEnabled").unwrap_or(true),
        focused,
        children: Vec::new(),
    };
    for c in children {
        node.children.extend(convert(c, depth + 1, walk));
    }
    vec![node]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn indexes_follow_document_order() {
        let src = json!({
            "type": "Application", "label": "Settings", "isVisible": "1",
            "rect": { "x": 0, "y": 0, "width": 402, "height": 874 },
            "children": [
                { "type": "Other", "isVisible": "1", "children": [
                    { "type": "StaticText", "label": "Hidden", "isVisible": "0" },
                    { "type": "Button", "label": "General", "name": "com.apple.settings.general", "isVisible": "1", "isEnabled": "1",
                      "rect": { "x": 16, "y": 293, "width": 370, "height": 53 } }
                ] }
            ]
        });
        let mut els = Elements::default();
        let tree = build(
            &src,
            "Settings",
            (402.0, 874.0),
            &TreeOptions::default(),
            &mut els,
        );
        let text = tree.render();
        assert!(!text.contains("Hidden"), "{text}");
        assert!(text.contains("Button \"General\" desc=\"com.apple.settings.general\" @(16,293 370x53) actions=press"), "{text}");
        // Application (1), Other (2), hidden StaticText (3), Button (4).
        let button = &tree.children[0].children[0];
        assert_eq!(els.get(&button.id).unwrap().path, vec![4]);
    }
}
