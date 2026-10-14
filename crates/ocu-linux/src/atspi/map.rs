//! What AT-SPI says about an element, turned into OCU's tree: the same role
//! and action names the other backends use, frames in window coordinates,
//! and anonymous containers folded away. Kept apart from the bus so it
//! builds and tests anywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use ocu_core::{Rect, UiNode};

/// AT-SPI's state bits (`AtspiStateType`), the ones read here.
pub mod state {
    pub const ACTIVE: u32 = 1;
    pub const CHECKED: u32 = 4;
    pub const DEFUNCT: u32 = 6;
    pub const EDITABLE: u32 = 7;
    pub const ENABLED: u32 = 8;
    pub const FOCUSED: u32 = 12;
    pub const MULTI_LINE: u32 = 17;
    pub const SENSITIVE: u32 = 24;
    pub const VISIBLE: u32 = 30;
    pub const CHECKABLE: u32 = 41;
}

/// An element's states, as the two words `GetState` returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct States(pub u64);

impl States {
    pub fn from_words(words: &[u32]) -> Self {
        let low = words.first().copied().unwrap_or(0) as u64;
        let high = words.get(1).copied().unwrap_or(0) as u64;
        Self(low | high << 32)
    }

    pub fn has(self, bit: u32) -> bool {
        self.0 & (1 << bit) != 0
    }

    /// Hidden or gone: not worth walking into. A toolkit that reports no
    /// states at all is given the benefit of the doubt.
    pub fn hidden(self) -> bool {
        self.has(state::DEFUNCT) || (self.0 != 0 && !self.has(state::VISIBLE))
    }

    pub fn enabled(self) -> bool {
        self.0 == 0 || self.has(state::ENABLED) || self.has(state::SENSITIVE)
    }
}

/// The interfaces an element offers, of the ones acted through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interfaces {
    pub action: bool,
    pub component: bool,
    pub text: bool,
    pub editable_text: bool,
    pub value: bool,
}

impl Interfaces {
    pub fn parse(names: &[String]) -> Self {
        let has = |n: &str| names.iter().any(|i| i == &format!("org.a11y.atspi.{n}"));
        Self {
            action: has("Action"),
            component: has("Component"),
            text: has("Text"),
            editable_text: has("EditableText"),
            value: has("Value"),
        }
    }
}

/// One element as read off the bus, before mapping.
#[derive(Clone, Debug, Default)]
pub struct Raw {
    /// AT-SPI's role name: "push button", "entry".
    pub role: String,
    pub name: String,
    pub description: String,
    pub states: States,
    pub interfaces: Interfaces,
    /// x, y, width, height, in the coordinates the walk asked for.
    pub extents: Option<(i32, i32, i32, i32)>,
    /// Action names, in the order `DoAction` numbers them.
    pub actions: Vec<String>,
    /// What a text field holds.
    pub text: Option<String>,
    /// A slider's or spin button's number.
    pub value: Option<f64>,
    /// Indices into the walk's list of elements.
    pub children: Vec<usize>,
}

/// OCU's name for an AT-SPI role, in the vocabulary the macOS backend
/// uses, so recipes and callers see one set of roles everywhere.
pub fn role(raw: &Raw) -> String {
    let editable = raw.states.has(state::EDITABLE);
    let multi_line = raw.states.has(state::MULTI_LINE);
    let named = match raw.role.as_str() {
        "push button" => "Button",
        "toggle button" => "ToggleButton",
        "push button menu" => "MenuButton",
        "check box" => "CheckBox",
        "radio button" => "RadioButton",
        "switch" => "Switch",
        "menu item" | "check menu item" | "radio menu item" | "tearoff menu item" => "MenuItem",
        "combo box" => "ComboBox",
        "entry" | "text box" | "password text" | "editbar" if multi_line => "TextArea",
        "entry" | "text box" | "password text" | "editbar" => "TextField",
        "text" if editable && multi_line => "TextArea",
        "text" if editable => "TextField",
        "text" | "label" | "static" | "caption" | "accelerator label" => "StaticText",
        "spin button" => "SpinButton",
        "slider" | "dial" => "Slider",
        "scroll bar" => "ScrollBar",
        "progress bar" => "ProgressIndicator",
        "level bar" => "LevelIndicator",
        "heading" => "Heading",
        "link" => "Link",
        "image" | "icon" | "image map" | "desktop icon" => "Image",
        "list" | "list box" | "description list" => "List",
        "list item" | "description term" | "description value" => "ListItem",
        "menu" | "popup menu" => "Menu",
        "menu bar" => "MenuBar",
        "page tab" => "Tab",
        "page tab list" => "TabGroup",
        "table" | "tree table" => "Table",
        "table row" => "Row",
        "table cell" => "Cell",
        "column header" | "table column header" => "ColumnHeader",
        "row header" | "table row header" => "RowHeader",
        "tree" => "Tree",
        "tree item" => "TreeItem",
        "tool bar" => "Toolbar",
        "status bar" => "StatusBar",
        "tool tip" => "ToolTip",
        "frame" | "window" => "Window",
        "dialog" | "file chooser" | "color chooser" | "font chooser" => "Dialog",
        "alert" | "notification" => "Alert",
        "document web" => "WebArea",
        "document frame"
        | "document text"
        | "document email"
        | "document presentation"
        | "document spreadsheet" => "Document",
        "application" => "Application",
        "scroll pane" => "ScrollArea",
        "split pane" => "SplitGroup",
        "separator" => "Separator",
        "drawing area" | "canvas" => "Canvas",
        "panel" | "filler" | "section" | "grouping" | "form" | "article" | "landmark"
        | "block quote" | "paragraph" | "unknown" | "redundant object" | "layered pane"
        | "root pane" | "glass pane" | "html container" | "option pane" | "internal frame"
        | "embedded" | "viewport" | "header" | "footer" | "info bar" | "generic" | "invalid"
        | "" => "Group",
        other => return camel(other),
    };
    named.to_string()
}

/// "date editor" → "DateEditor", for roles without a name of their own.
fn camel(s: &str) -> String {
    s.split([' ', '-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect()
}

/// OCU's name for an AT-SPI action: "press" for whatever activates the
/// element, "showmenu" for whatever opens it, the action's own name
/// otherwise.
pub fn action(name: &str) -> String {
    let n = name.trim().to_lowercase();
    match n.as_str() {
        "click" | "press" | "activate" | "jump" | "toggle" | "open" | "select" | "invoke" => {
            "press".into()
        }
        "expand or contract" | "expand" | "showmenu" | "show menu" | "showcontextmenu"
        | "popup" | "menu" => "showmenu".into(),
        _ => n.replace([' ', '-'], "_"),
    }
}

/// The index of the AT-SPI action an OCU action name means: by the name
/// OCU shows, or the toolkit's own.
pub fn find_action(actions: &[String], wanted: &str) -> Option<usize> {
    let wanted = wanted.trim().to_lowercase();
    let wanted = wanted.trim_start_matches("ax");
    actions
        .iter()
        .position(|a| action(a) == wanted)
        .or_else(|| actions.iter().position(|a| a.eq_ignore_ascii_case(wanted)))
        .or_else(|| {
            // "click" and "activate" mean "press" too.
            (action(wanted) == "press")
                .then(|| actions.iter().position(|a| action(a) == "press"))
                .flatten()
        })
}

/// Roles whose value is typed, so a value that can't be set directly can
/// still be typed in.
pub fn takes_text(role: &str) -> bool {
    matches!(role, "TextField" | "TextArea" | "ComboBox" | "SpinButton")
}

fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Actions Chromium gives every element.
const GENERIC: &[&str] = &["doDefault", "showContextMenu"];

/// One mapped element, which the session keeps to act on by id.
#[derive(Clone, Debug)]
pub struct Mapped {
    /// Index into the walk's list of elements.
    pub index: usize,
    pub role: String,
    pub frame: Option<Rect>,
}

/// Builds OCU's tree from the walk's elements, starting at `root`.
/// `origin` is subtracted from every frame, putting them in the window's
/// coordinates. Ids number elements in reading order, a parent before its
/// children; `elements` gets what each id stands for.
pub fn build(raws: &[Raw], root: usize, origin: (f64, f64), elements: &mut Vec<Mapped>) -> UiNode {
    elements.clear();
    let mut nodes = node(raws, root, origin, 0, None, elements);
    match nodes.len() {
        1 => nodes.pop().unwrap(),
        _ => UiNode {
            id: "root".into(),
            role: "Window".into(),
            enabled: true,
            children: nodes,
            ..Default::default()
        },
    }
}

fn node(
    raws: &[Raw],
    i: usize,
    origin: (f64, f64),
    depth: usize,
    parent: Option<&str>,
    elements: &mut Vec<Mapped>,
) -> Vec<UiNode> {
    let raw = &raws[i];
    let role = role(raw);
    let actions: Vec<String> = {
        // GTK 4 lists its widgets' own actions too ("clipboard.copy"), and
        // Chromium gives every element "doDefault" and "showContextMenu",
        // which only mean something next to a real action. Hidden ones can
        // still be asked for.
        let shown = |a: &&String| !a.contains('.') && !GENERIC.contains(&a.as_str());
        let real = raw.actions.iter().any(|a| shown(&a));
        let mut a: Vec<String> = raw
            .actions
            .iter()
            .filter(|a| shown(a) || (real && a.as_str() == "showContextMenu"))
            .map(|a| action(a))
            .collect();
        if raw.interfaces.value && !raw.interfaces.editable_text {
            a.extend(["increment".into(), "decrement".into()]);
        }
        let mut seen = std::collections::HashSet::new();
        a.retain(|x| !x.is_empty() && seen.insert(x.clone()));
        a
    };
    let checkable = matches!(
        role.as_str(),
        "CheckBox" | "RadioButton" | "Switch" | "ToggleButton"
    ) || raw.states.has(state::CHECKABLE);
    let mut name = Some(raw.name.clone()).filter(|s| !s.is_empty());
    let mut value = if checkable {
        Some(
            if raw.states.has(state::CHECKED) {
                "1"
            } else {
                "0"
            }
            .to_string(),
        )
    } else if let Some(v) = raw.value {
        Some(number(v))
    } else {
        raw.text.clone().filter(|t| !t.is_empty())
    };
    // A label with no name carries its words as text.
    if role == "StaticText" && name.is_none() {
        name = value.take();
    }
    let description = Some(raw.description.clone()).filter(|s| !s.is_empty());

    // A button's own label says nothing its button doesn't.
    if role == "StaticText"
        && raw.children.is_empty()
        && name.is_some()
        && name.as_deref() == parent
    {
        return Vec::new();
    }
    let flatten = depth > 0
        && role == "Group"
        && name.is_none()
        && description.is_none()
        && value.is_none()
        && actions.is_empty();
    if flatten {
        return raw
            .children
            .iter()
            .flat_map(|&c| node(raws, c, origin, depth + 1, parent, elements))
            .collect();
    }
    let frame = raw
        .extents
        .filter(|&(x, y, w, h)| w >= 0 && h >= 0 && x != i32::MIN && y != i32::MIN)
        .map(|(x, y, w, h)| Rect {
            x: x as f64 - origin.0,
            y: y as f64 - origin.1,
            width: w as f64,
            height: h as f64,
        });
    let id = format!("e{}", elements.len());
    elements.push(Mapped {
        index: i,
        role: role.clone(),
        frame,
    });
    let children = raw
        .children
        .iter()
        .flat_map(|&c| node(raws, c, origin, depth + 1, name.as_deref(), elements))
        .collect();
    vec![UiNode {
        id,
        role,
        name,
        value,
        description,
        frame,
        actions,
        enabled: raw.states.enabled(),
        focused: raw.states.has(state::FOCUSED),
        children,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(bits: &[u32]) -> States {
        States(bits.iter().fold(0, |s, b| s | 1 << b))
    }

    fn raw(role: &str, name: &str) -> Raw {
        Raw {
            role: role.into(),
            name: name.into(),
            states: states(&[state::VISIBLE, state::ENABLED]),
            ..Default::default()
        }
    }

    #[test]
    fn roles_use_the_shared_vocabulary() {
        assert_eq!(role(&raw("push button", "")), "Button");
        assert_eq!(role(&raw("entry", "")), "TextField");
        assert_eq!(role(&raw("text box", "")), "TextField");
        let mut area = raw("entry", "");
        area.states = states(&[state::MULTI_LINE, state::EDITABLE]);
        assert_eq!(role(&area), "TextArea");
        assert_eq!(role(&raw("text", "")), "StaticText");
        let mut field = raw("text", "");
        field.states = states(&[state::EDITABLE]);
        assert_eq!(role(&field), "TextField");
        assert_eq!(role(&raw("document web", "")), "WebArea");
        assert_eq!(role(&raw("frame", "")), "Window");
        assert_eq!(role(&raw("section", "")), "Group");
        assert_eq!(role(&raw("generic", "")), "Group");
        assert_eq!(role(&raw("date editor", "")), "DateEditor");
    }

    #[test]
    fn actions_use_the_shared_vocabulary() {
        assert_eq!(action("click"), "press");
        assert_eq!(action("Activate"), "press");
        assert_eq!(action("jump"), "press");
        assert_eq!(action("expand or contract"), "showmenu");
        assert_eq!(action("showContextMenu"), "showmenu");
        assert_eq!(action("customAction one"), "customaction_one");
        let acts = vec!["expand or contract".to_string(), "activate".to_string()];
        assert_eq!(find_action(&acts, "press"), Some(1));
        assert_eq!(find_action(&acts, "AXPress"), Some(1));
        assert_eq!(find_action(&acts, "click"), Some(1));
        assert_eq!(find_action(&acts, "showmenu"), Some(0));
        assert_eq!(find_action(&acts, "Expand or Contract"), Some(0));
        assert_eq!(find_action(&acts, "increment"), None);
    }

    #[test]
    fn states_read_both_words() {
        let s = States::from_words(&[1 << state::VISIBLE, 1 << (state::CHECKABLE - 32)]);
        assert!(s.has(state::VISIBLE) && s.has(state::CHECKABLE));
        assert!(!s.hidden());
        assert!(States::from_words(&[1 << state::FOCUSED]).hidden());
        assert!(!States::default().hidden());
        assert!(!States::from_words(&[1 << state::VISIBLE]).enabled());
    }

    #[test]
    fn builds_the_tree_ocu_shows() {
        let mut window = raw("frame", "Demo");
        window.extents = Some((100, 50, 400, 300));
        window.children = vec![1];
        let mut panel = raw("panel", "");
        panel.children = vec![2, 3, 4];
        let mut button = raw("push button", "Go");
        button.extents = Some((110, 60, 80, 30));
        button.actions = vec!["click".into()];
        button.interfaces.action = true;
        let mut field = raw("entry", "Query");
        field.text = Some("cats".into());
        field.states = states(&[state::VISIBLE, state::ENABLED, state::FOCUSED]);
        field.actions = vec!["activate".into()];
        let mut check = raw("check box", "Safe");
        check.states = states(&[state::VISIBLE, state::CHECKED]);
        let raws = vec![window, panel, button, field, check];

        let mut elements = Vec::new();
        let tree = build(&raws, 0, (100.0, 50.0), &mut elements);
        assert_eq!(
            tree.render(),
            "[e0] Window \"Demo\" @(0,0 400x300)\n\
             \x20 [e1] Button \"Go\" @(10,10 80x30) actions=press\n\
             \x20 [e2] TextField \"Query\" value=\"cats\" actions=press focused\n\
             \x20 [e3] CheckBox \"Safe\" value=\"1\" disabled\n"
        );
        let indices: Vec<usize> = elements.iter().map(|e| e.index).collect();
        assert_eq!(indices, [0, 2, 3, 4]);
        assert_eq!(elements[1].role, "Button");
    }

    #[test]
    fn toolkit_noise_is_left_out() {
        let mut button = raw("push button", "Greet");
        button.actions = vec!["click".into()];
        button.children = vec![1];
        let mut label = raw("label", "Greet");
        label.actions = vec!["clipboard.copy".into(), "menu.popup".into()];
        let raws = vec![button, label];
        let mut elements = Vec::new();
        let tree = build(&raws, 0, (0.0, 0.0), &mut elements);
        assert_eq!(tree.render(), "[e0] Button \"Greet\" actions=press\n");
        // Hidden, but still there to ask for by the toolkit's name.
        assert_eq!(find_action(&raws[1].actions, "clipboard.copy"), Some(0));

        let mut page = raw("document web", "Form");
        page.actions = vec!["doDefault".into(), "showContextMenu".into()];
        page.children = vec![1];
        let mut group = raw("section", "");
        group.actions = page.actions.clone();
        group.children = vec![2];
        let mut go = raw("push button", "Go");
        go.actions = vec!["press".into(), "showContextMenu".into()];
        let raws = vec![page, group, go];
        let tree = build(&raws, 0, (0.0, 0.0), &mut elements);
        assert_eq!(
            tree.render(),
            "[e0] WebArea \"Form\"\n  [e1] Button \"Go\" actions=press,showmenu\n"
        );
        assert_eq!(find_action(&raws[0].actions, "showmenu"), Some(1));
    }

    #[test]
    fn values_and_labels() {
        let mut slider = raw("slider", "Volume");
        slider.value = Some(40.0);
        slider.interfaces.value = true;
        let mut label = raw("label", "");
        label.text = Some("Hello".into());
        let raws = vec![slider, label];
        let mut elements = Vec::new();
        let s = build(&raws, 0, (0.0, 0.0), &mut elements);
        assert_eq!(s.value.as_deref(), Some("40"));
        assert_eq!(s.actions, ["increment", "decrement"]);
        let l = build(&raws, 1, (0.0, 0.0), &mut elements);
        assert_eq!(
            (l.name.as_deref(), l.value.as_deref()),
            (Some("Hello"), None)
        );
    }
}
