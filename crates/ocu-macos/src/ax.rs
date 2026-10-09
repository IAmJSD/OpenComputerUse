//! The Accessibility API: reading an app's element tree, pressing and
//! setting elements, and finding which element sits under a point. All of
//! it works on apps in the background.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{anyhow, bail, Result};
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};

use ocu_core::{Rect, TreeOptions, UiNode};

/// An element handle. AX handles are plain mach-port references, safe to
/// use from any thread.
#[derive(Clone)]
pub struct Element(pub CFRetained<AXUIElement>);

unsafe impl Send for Element {}

extern "C" {
    // Private but long-stable (yabai, Hammerspoon and friends rely on it):
    // the CGWindowID behind an AXWindow, the only link between the two.
    fn _AXUIElementGetWindow(element: &AXUIElement, out: *mut u32) -> AXError;
}

fn cfstr(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

fn check(err: AXError, what: &str) -> Result<()> {
    if err == AXError::Success {
        Ok(())
    } else {
        bail!("{what} failed (AXError {})", err.0)
    }
}

impl Element {
    pub fn application(pid: i32) -> Self {
        let el = unsafe { AXUIElement::new_application(pid) };
        // A hung app must not hang us.
        unsafe { el.set_messaging_timeout(2.0) };
        Self(el)
    }

    pub fn attr(&self, name: &str) -> Option<CFRetained<CFType>> {
        let mut out: *const CFType = std::ptr::null();
        let err = unsafe {
            self.0
                .copy_attribute_value(&cfstr(name), NonNull::from(&mut out))
        };
        if err != AXError::Success || out.is_null() {
            return None;
        }
        Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out as *mut CFType)) })
    }

    pub fn string(&self, name: &str) -> Option<String> {
        self.attr(name).and_then(|v| describe_value(&v))
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        self.attr(name)
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(|b| b.as_bool()))
    }

    pub fn elements(&self, name: &str) -> Vec<Element> {
        let Some(v) = self.attr(name) else {
            return Vec::new();
        };
        if let Some(arr) = v.downcast_ref::<CFArray>() {
            let arr: &CFArray<CFType> = unsafe { arr.cast_unchecked() };
            arr.iter()
                .filter_map(|v| v.downcast::<AXUIElement>().ok().map(Element))
                .collect()
        } else {
            v.downcast::<AXUIElement>()
                .ok()
                .map(Element)
                .into_iter()
                .collect()
        }
    }

    pub fn element(&self, name: &str) -> Option<Element> {
        self.attr(name)?.downcast::<AXUIElement>().ok().map(Element)
    }

    /// The element's frame in screen points (top-left origin).
    pub fn frame(&self) -> Option<CGRect> {
        let pos: CGPoint = ax_value(&*self.attr("AXPosition")?, AXValueType::CGPoint)?;
        let size: CGSize = ax_value(&*self.attr("AXSize")?, AXValueType::CGSize)?;
        Some(CGRect { origin: pos, size })
    }

    pub fn actions(&self) -> Vec<String> {
        let mut names: *const CFArray = std::ptr::null();
        let err = unsafe { self.0.copy_action_names(NonNull::from(&mut names)) };
        if err != AXError::Success || names.is_null() {
            return Vec::new();
        }
        let names: CFRetained<CFArray> =
            unsafe { CFRetained::from_raw(NonNull::new_unchecked(names as *mut _)) };
        let names: &CFArray<CFString> = unsafe { names.cast_unchecked() };
        names.iter().map(|s| s.to_string()).collect()
    }

    pub fn perform(&self, action: &str) -> Result<()> {
        check(unsafe { self.0.perform_action(&cfstr(action)) }, action)
    }

    pub fn set(&self, name: &str, value: &CFType) -> Result<()> {
        check(
            unsafe { self.0.set_attribute_value(&cfstr(name), value) },
            &format!("setting {name}"),
        )
    }

    pub fn settable(&self, name: &str) -> bool {
        let mut out: u8 = 0;
        let err = unsafe {
            self.0
                .is_attribute_settable(&cfstr(name), NonNull::from(&mut out).cast())
        };
        err == AXError::Success && out != 0
    }

    pub fn window_id(&self) -> Option<u32> {
        let mut id = 0u32;
        let err = unsafe { _AXUIElementGetWindow(&self.0, &mut id) };
        (err == AXError::Success && id != 0).then_some(id)
    }
}

fn ax_value<T: Default>(v: &CFType, ty: AXValueType) -> Option<T> {
    let v = v.downcast_ref::<AXValue>()?;
    let mut out = T::default();
    let ok = unsafe { v.value(ty, NonNull::from(&mut out).cast::<c_void>()) };
    ok.then_some(out)
}

/// A value attribute as text: strings as they are, numbers and booleans
/// spelled out. Other types (ranges, elements) are not worth showing.
fn describe_value(v: &CFType) -> Option<String> {
    if let Some(s) = v.downcast_ref::<CFString>() {
        return Some(s.to_string());
    }
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool().to_string());
    }
    if let Some(n) = v.downcast_ref::<CFNumber>() {
        if let Some(i) = n.as_i64() {
            return Some(i.to_string());
        }
        return n.as_f64().map(|f| format!("{f}"));
    }
    None
}

/// Finds the AX window behind a CGWindowID, or the focused/main window.
pub fn window_element(pid: i32, window_id: Option<u32>) -> Result<Element> {
    let app = Element::application(pid);
    let windows = app.elements("AXWindows");
    if let Some(id) = window_id {
        if let Some(w) = windows.iter().find(|w| w.window_id() == Some(id)) {
            return Ok(w.clone());
        }
        bail!("no accessible window {id} (is Accessibility permission granted?)");
    }
    app.element("AXFocusedWindow")
        .or_else(|| app.element("AXMainWindow"))
        .or_else(|| windows.into_iter().next())
        .ok_or_else(|| {
            anyhow!("the app exposes no accessible windows (is Accessibility permission granted?)")
        })
}

/// The deepest element of app `pid` under a screen point.
pub fn element_at(pid: i32, at: CGPoint) -> Option<Element> {
    let app = Element::application(pid);
    let mut out: *const AXUIElement = std::ptr::null();
    let err = unsafe {
        app.0
            .copy_element_at_position(at.x as f32, at.y as f32, NonNull::from(&mut out))
    };
    if err != AXError::Success || out.is_null() {
        return None;
    }
    Some(Element(unsafe {
        CFRetained::from_raw(NonNull::new_unchecked(out as *mut AXUIElement))
    }))
}

/// Whether `inner` is `outer` or sits inside it.
pub fn is_within(inner: &Element, outer: &Element) -> bool {
    let outer: &CFType = &outer.0;
    let mut cur = inner.clone();
    for _ in 0..64 {
        if <CFType as PartialEq>::eq(&cur.0, outer) {
            return true;
        }
        match cur.element("AXParent") {
            Some(parent) => cur = parent,
            None => return false,
        }
    }
    false
}

/// Whether the element sits in a web page (an AXWebArea above it), as
/// opposed to the browser's own toolbar, tabs and address bar.
pub fn in_web_area(el: &Element) -> bool {
    let mut cur = el.clone();
    for _ in 0..64 {
        if cur.string("AXRole").as_deref() == Some("AXWebArea") {
            return true;
        }
        match cur.element("AXParent") {
            Some(parent) => cur = parent,
            None => return false,
        }
    }
    false
}

/// Element ids handed out by the last tree read.
#[derive(Default)]
pub struct ElementTable {
    elements: Vec<Element>,
}

impl ElementTable {
    pub fn get(&self, id: &str) -> Result<&Element> {
        let n: usize = id
            .trim_start_matches('e')
            .parse()
            .map_err(|_| anyhow!("\"{id}\" is not an element id (they look like e12)"))?;
        self.elements
            .get(n)
            .ok_or_else(|| anyhow!("no element {id}; read the tree again (ids reset each read)"))
    }
}

/// Roles that only group others; unnamed ones are flattened away.
const GROUPING: &[&str] = &["AXGroup", "AXUnknown", "AXSplitGroup", "AXLayoutArea"];

pub fn read_tree(
    root: &Element,
    origin: CGPoint,
    opts: &TreeOptions,
    table: &mut ElementTable,
) -> UiNode {
    table.elements.clear();
    let mut budget = opts.max_nodes;
    let mut nodes = walk(root, origin, 0, opts, &mut budget, table);
    match nodes.len() {
        1 => nodes.pop().unwrap(),
        _ => UiNode {
            id: "root".into(),
            role: "AXWindow".into(),
            children: nodes,
            enabled: true,
            ..Default::default()
        },
    }
}

fn walk(
    el: &Element,
    origin: CGPoint,
    depth: usize,
    opts: &TreeOptions,
    budget: &mut usize,
    table: &mut ElementTable,
) -> Vec<UiNode> {
    if *budget == 0 {
        return Vec::new();
    }
    *budget -= 1;
    let role = el.string("AXRole").unwrap_or_else(|| "AXUnknown".into());
    let name = el.string("AXTitle").filter(|s| !s.is_empty());
    let description = el.string("AXDescription").filter(|s| !s.is_empty());
    let label = el.string("AXLabel").filter(|s| !s.is_empty());
    let value = if role == "AXStaticText" && name.is_none() {
        None
    } else {
        el.string("AXValue").filter(|v| !v.is_empty())
    };
    // Static text carries its words in AXValue; show them as the name.
    let (name, value) = match (name, value) {
        (None, v) if role == "AXStaticText" => (el.string("AXValue").or(v), None),
        (n, v) => (n, v),
    };
    // Window buttons and the like have only a subrole to go by.
    // A field's placeholder ("Departing from") is often all that names it.
    let name = name
        .or(label)
        .or_else(|| el.string("AXPlaceholderValue").filter(|s| !s.is_empty()))
        .or_else(|| {
            (description.is_none())
                .then(|| el.string("AXSubrole"))
                .flatten()
                .filter(|s| s.ends_with("Button"))
                .map(|s| s.trim_start_matches("AX").to_string())
        });
    let flatten = GROUPING.contains(&role.as_str())
        && name.is_none()
        && description.is_none()
        && value.is_none();
    let children_els = if depth < opts.max_depth {
        el.elements("AXChildren")
    } else {
        Vec::new()
    };
    if flatten {
        let mut children = Vec::new();
        for c in &children_els {
            children.extend(walk(c, origin, depth + 1, opts, budget, table));
        }
        return children;
    }
    // Ids in reading order: a parent before its children.
    let id = format!("e{}", table.elements.len());
    table.elements.push(el.clone());
    let mut children = Vec::new();
    for c in &children_els {
        children.extend(walk(c, origin, depth + 1, opts, budget, table));
    }
    let frame = el.frame().map(|f| Rect {
        x: f.origin.x - origin.x,
        y: f.origin.y - origin.y,
        width: f.size.width,
        height: f.size.height,
    });
    let actions = el
        .actions()
        .into_iter()
        .filter(|a| a != "AXScrollToVisible" && a != "AXShowDefaultUI" && a != "AXShowAlternateUI")
        .map(|a| a.trim_start_matches("AX").to_lowercase())
        .collect();
    vec![UiNode {
        id,
        role: role.trim_start_matches("AX").to_string(),
        name,
        value,
        description,
        frame,
        actions,
        enabled: el.bool("AXEnabled").unwrap_or(true),
        focused: el.bool("AXFocused").unwrap_or(false),
        children,
    }]
}

/// "press", "showmenu", "AXPress" → "AXPress", "AXShowMenu".
pub fn action_name(name: &str) -> String {
    if name.starts_with("AX") {
        return name.to_string();
    }
    let known = [
        "AXPress",
        "AXShowMenu",
        "AXConfirm",
        "AXCancel",
        "AXIncrement",
        "AXDecrement",
        "AXRaise",
        "AXPick",
        "AXOpen",
        "AXScrollToVisible",
    ];
    let want = format!("ax{}", name.to_lowercase().replace(['_', ' ', '-'], ""));
    known
        .iter()
        .find(|k| k.to_lowercase() == want)
        .map(|k| k.to_string())
        .unwrap_or_else(|| format!("AX{}{}", name[..1].to_uppercase(), &name[1..]))
}

pub fn cf_string(s: &str) -> CFRetained<CFString> {
    cfstr(s)
}
