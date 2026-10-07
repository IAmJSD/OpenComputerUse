//! The accessibility tree from UI Automation, and acting on its elements
//! through their patterns (Invoke, Toggle, Value, …), which work without
//! the window being in front.

use anyhow::{anyhow, bail, Result};
use windows::core::{Interface as _, BSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::*;

use ocu_core::{Rect, TreeOptions, UiNode};

use crate::capture::hwnd;

/// UI Automation objects live in the multithreaded apartment, where any
/// thread may use them.
pub struct Uia {
    automation: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    elements: Vec<IUIAutomationElement>,
}

unsafe impl Send for Uia {}

pub fn com_init() {
    // Already initialised (S_FALSE) or initialised differently is fine.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

#[allow(non_upper_case_globals)]
fn role(t: UIA_CONTROLTYPE_ID) -> &'static str {
    match t {
        UIA_ButtonControlTypeId => "Button",
        UIA_CalendarControlTypeId => "Calendar",
        UIA_CheckBoxControlTypeId => "CheckBox",
        UIA_ComboBoxControlTypeId => "ComboBox",
        UIA_EditControlTypeId => "Edit",
        UIA_HyperlinkControlTypeId => "Hyperlink",
        UIA_ImageControlTypeId => "Image",
        UIA_ListItemControlTypeId => "ListItem",
        UIA_ListControlTypeId => "List",
        UIA_MenuControlTypeId => "Menu",
        UIA_MenuBarControlTypeId => "MenuBar",
        UIA_MenuItemControlTypeId => "MenuItem",
        UIA_ProgressBarControlTypeId => "ProgressBar",
        UIA_RadioButtonControlTypeId => "RadioButton",
        UIA_ScrollBarControlTypeId => "ScrollBar",
        UIA_SliderControlTypeId => "Slider",
        UIA_SpinnerControlTypeId => "Spinner",
        UIA_StatusBarControlTypeId => "StatusBar",
        UIA_TabControlTypeId => "Tab",
        UIA_TabItemControlTypeId => "TabItem",
        UIA_TextControlTypeId => "Text",
        UIA_ToolBarControlTypeId => "ToolBar",
        UIA_ToolTipControlTypeId => "ToolTip",
        UIA_TreeControlTypeId => "Tree",
        UIA_TreeItemControlTypeId => "TreeItem",
        UIA_GroupControlTypeId => "Group",
        UIA_ThumbControlTypeId => "Thumb",
        UIA_DataGridControlTypeId => "DataGrid",
        UIA_DataItemControlTypeId => "DataItem",
        UIA_DocumentControlTypeId => "Document",
        UIA_SplitButtonControlTypeId => "SplitButton",
        UIA_WindowControlTypeId => "Window",
        UIA_PaneControlTypeId => "Pane",
        UIA_HeaderControlTypeId => "Header",
        UIA_HeaderItemControlTypeId => "HeaderItem",
        UIA_TableControlTypeId => "Table",
        UIA_TitleBarControlTypeId => "TitleBar",
        UIA_SeparatorControlTypeId => "Separator",
        _ => "Custom",
    }
}

impl Uia {
    pub fn new() -> Result<Self> {
        com_init();
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
        let walker = unsafe { automation.ControlViewWalker()? };
        Ok(Self {
            automation,
            walker,
            elements: Vec::new(),
        })
    }

    pub fn tree(&mut self, window: u64, origin: (f64, f64), opts: &TreeOptions) -> Result<UiNode> {
        self.elements.clear();
        let root = unsafe { self.automation.ElementFromHandle(hwnd(window))? };
        let mut budget = opts.max_nodes;
        let mut nodes = self.walk(&root, origin, 0, opts, &mut budget);
        Ok(match nodes.len() {
            1 => nodes.pop().unwrap(),
            _ => UiNode {
                id: "root".into(),
                role: "Window".into(),
                enabled: true,
                children: nodes,
                ..Default::default()
            },
        })
    }

    fn walk(
        &mut self,
        el: &IUIAutomationElement,
        origin: (f64, f64),
        depth: usize,
        opts: &TreeOptions,
        budget: &mut usize,
    ) -> Vec<UiNode> {
        if *budget == 0 {
            return Vec::new();
        }
        *budget -= 1;
        let ty = unsafe { el.CurrentControlType() }.unwrap_or(UIA_CustomControlTypeId);
        let name = unsafe { el.CurrentName() }
            .ok()
            .map(|b| b.to_string())
            .filter(|s| !s.is_empty());
        let value = self.value(el);
        let mut children_els = Vec::new();
        if depth < opts.max_depth {
            let mut next = unsafe { self.walker.GetFirstChildElement(el) }.ok();
            while let Some(c) = next {
                next = unsafe { self.walker.GetNextSiblingElement(&c) }.ok();
                children_els.push(c);
            }
        }
        #[allow(non_upper_case_globals)]
        let flatten = matches!(
            ty,
            UIA_PaneControlTypeId | UIA_GroupControlTypeId | UIA_CustomControlTypeId
        ) && name.is_none()
            && value.is_none()
            && depth > 0;
        if flatten {
            return children_els
                .iter()
                .flat_map(|c| self.walk(c, origin, depth + 1, opts, budget))
                .collect();
        }
        let id = format!("e{}", self.elements.len());
        self.elements.push(el.clone());
        let children = children_els
            .iter()
            .flat_map(|c| self.walk(c, origin, depth + 1, opts, budget))
            .collect();
        let frame = unsafe { el.CurrentBoundingRectangle() }.ok().map(|r| Rect {
            x: r.left as f64 - origin.0,
            y: r.top as f64 - origin.1,
            width: (r.right - r.left) as f64,
            height: (r.bottom - r.top) as f64,
        });
        vec![UiNode {
            id,
            role: role(ty).into(),
            name,
            value,
            description: unsafe { el.CurrentHelpText() }
                .ok()
                .map(|b| b.to_string())
                .filter(|s| !s.is_empty()),
            frame,
            actions: self.actions(el),
            enabled: unsafe { el.CurrentIsEnabled() }
                .map(|b| b.as_bool())
                .unwrap_or(true),
            focused: unsafe { el.CurrentHasKeyboardFocus() }
                .map(|b| b.as_bool())
                .unwrap_or(false),
            children,
        }]
    }

    fn value(&self, el: &IUIAutomationElement) -> Option<String> {
        let p: IUIAutomationValuePattern =
            unsafe { el.GetCurrentPatternAs(UIA_ValuePatternId) }.ok()?;
        unsafe { p.CurrentValue() }
            .ok()
            .map(|b| b.to_string())
            .filter(|s| !s.is_empty())
    }

    fn has(&self, el: &IUIAutomationElement, pattern: UIA_PATTERN_ID) -> bool {
        unsafe { el.GetCurrentPattern(pattern) }.is_ok_and(|p| !p.as_raw().is_null())
    }

    fn actions(&self, el: &IUIAutomationElement) -> Vec<String> {
        let mut a = Vec::new();
        if self.has(el, UIA_InvokePatternId)
            || self.has(el, UIA_TogglePatternId)
            || self.has(el, UIA_SelectionItemPatternId)
        {
            a.push("press".into());
        }
        if self.has(el, UIA_ExpandCollapsePatternId) {
            a.push("showmenu".into());
        }
        if self.has(el, UIA_RangeValuePatternId) {
            a.extend(["increment".into(), "decrement".into()]);
        }
        a
    }

    pub fn element(&self, id: &str) -> Result<&IUIAutomationElement> {
        let n: usize = id
            .trim_start_matches('e')
            .parse()
            .map_err(|_| anyhow!("\"{id}\" is not an element id"))?;
        self.elements
            .get(n)
            .ok_or_else(|| anyhow!("no element {id}; read the tree again"))
    }

    pub fn perform(&self, id: &str, action: &str) -> Result<()> {
        let el = self.element(id)?;
        unsafe {
            match action.to_lowercase().trim_start_matches("ax") {
                "press" | "invoke" | "click" => {
                    if let Ok(p) =
                        el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    {
                        return Ok(p.Invoke()?);
                    }
                    if let Ok(p) =
                        el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    {
                        return Ok(p.Toggle()?);
                    }
                    if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    ) {
                        return Ok(p.Select()?);
                    }
                    bail!("element {id} cannot be pressed; click its position instead")
                }
                "showmenu" | "expand" => {
                    let p: IUIAutomationExpandCollapsePattern =
                        el.GetCurrentPatternAs(UIA_ExpandCollapsePatternId)?;
                    Ok(p.Expand()?)
                }
                "collapse" | "cancel" => {
                    let p: IUIAutomationExpandCollapsePattern =
                        el.GetCurrentPatternAs(UIA_ExpandCollapsePatternId)?;
                    Ok(p.Collapse()?)
                }
                "increment" | "decrement" => {
                    let p: IUIAutomationRangeValuePattern =
                        el.GetCurrentPatternAs(UIA_RangeValuePatternId)?;
                    let step = p.CurrentSmallChange()?.max(1.0);
                    let v = p.CurrentValue()?;
                    Ok(p.SetValue(if action.contains("incr") {
                        v + step
                    } else {
                        v - step
                    })?)
                }
                other => bail!(
                    "unknown action \"{other}\" (press, showmenu, collapse, increment, decrement)"
                ),
            }
        }
    }

    pub fn set_value(&self, id: &str, value: &str) -> Result<()> {
        let el = self.element(id)?;
        unsafe {
            if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                return Ok(p.SetValue(&BSTR::from(value))?);
            }
            if let Ok(p) =
                el.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
            {
                let v: f64 = value
                    .trim()
                    .parse()
                    .map_err(|_| anyhow!("this element takes a number"))?;
                return Ok(p.SetValue(v)?);
            }
        }
        bail!("element {id} has no settable value")
    }

    /// The element's centre in window coordinates, for clicking it.
    pub fn center(&self, id: &str, origin: (f64, f64)) -> Result<(f64, f64)> {
        let r = unsafe { self.element(id)?.CurrentBoundingRectangle()? };
        Ok((
            (r.left + r.right) as f64 / 2.0 - origin.0,
            (r.top + r.bottom) as f64 / 2.0 - origin.1,
        ))
    }
}
