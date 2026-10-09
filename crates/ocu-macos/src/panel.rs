//! Open and save panels. AppKit draws them in a process of their own,
//! `com.apple.appkit.xpc.openAndSavePanelService`, and shows that process's
//! window inside one the app keeps for it, with the same frame. The app's
//! accessibility tree reaches into the panel, but input sent to the app
//! never gets there: it goes to the service's window instead. Neither
//! window captures on its own; the app's, composed as the screen draws it,
//! does ([`crate::capture::capture_composed`]).
//!
//! [`answer`] fills the panel in without the pointer: the "Go to" sheet
//! takes the path, command-clicks add more files, and the default button
//! is pressed through accessibility.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFString, CFType, CGPoint, CGRect,
};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};

use ocu_core::keys::{parse_chords, Modifiers};
use ocu_core::{MouseButton, WindowInfo};

use crate::ax::{self, Element};
use crate::input::{self, Target};

/// The service's window showing inside one of the app's windows.
#[derive(Clone, Copy, Debug)]
pub struct Remote {
    pub pid: i32,
    pub window: u32,
}

fn get<'a>(dict: &'a CFDictionary, key: &str) -> Option<&'a CFType> {
    let key = CFString::from_str(key);
    let v = unsafe { dict.value((&*key as *const CFString).cast::<c_void>()) };
    (!v.is_null()).then(|| unsafe { &*(v as *const CFType) })
}

fn num(dict: &CFDictionary, key: &str) -> Option<f64> {
    let n = get(dict, key)?.downcast_ref::<CFNumber>()?;
    n.as_f64().or_else(|| n.as_i64().map(|i| i as f64))
}

/// Whether `pid` is a panel service, remembered per pid: a service lives
/// as long as the app it serves.
fn is_panel_service(pid: i32) -> bool {
    static KNOWN: Mutex<Option<HashMap<i32, bool>>> = Mutex::new(None);
    let mut known = KNOWN.lock().unwrap();
    let known = known.get_or_insert_with(HashMap::new);
    *known.entry(pid).or_insert_with(|| {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        n > 0 && String::from_utf8_lossy(&buf[..n as usize]).contains("openAndSavePanelService")
    })
}

/// The panel showing in each of `windows`, keyed by the app's window id:
/// a panel service window with exactly the frame of one of them.
pub fn remotes(windows: &[WindowInfo]) -> HashMap<u64, Remote> {
    let mut out = HashMap::new();
    if windows.is_empty() {
        return out;
    }
    let Some(list) = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0) else {
        return out;
    };
    let list: &CFArray<CFDictionary> = unsafe { list.cast_unchecked() };
    for dict in list.iter() {
        let (Some(pid), Some(id)) = (
            num(&dict, "kCGWindowOwnerPID"),
            num(&dict, "kCGWindowNumber"),
        ) else {
            continue;
        };
        if num(&dict, "kCGWindowLayer") != Some(0.0) {
            continue;
        }
        let Some(bounds) =
            get(&dict, "kCGWindowBounds").and_then(|b| b.downcast_ref::<CFDictionary>())
        else {
            continue;
        };
        let frame = [
            num(bounds, "X").unwrap_or(0.0),
            num(bounds, "Y").unwrap_or(0.0),
            num(bounds, "Width").unwrap_or(0.0),
            num(bounds, "Height").unwrap_or(0.0),
        ];
        if frame[2] < 100.0 || frame[3] < 100.0 {
            continue;
        }
        let host = windows.iter().find(|w| {
            [w.frame.x, w.frame.y, w.frame.width, w.frame.height] == frame
                && !out.contains_key(&w.id)
        });
        if let Some(host) = host {
            if is_panel_service(pid as i32) {
                out.insert(
                    host.id,
                    Remote {
                        pid: pid as i32,
                        window: id as u32,
                    },
                );
            }
        }
    }
    out
}

/// The panel's accessibility element in the app: by window id, or by
/// frame where the app's element does not give its id (Firefox's).
pub fn element(app_pid: i32, host: &WindowInfo) -> Result<Element> {
    if let Ok(el) = ax::window_element(app_pid, Some(host.id as u32)) {
        return Ok(el);
    }
    let same = |el: &Element| {
        el.frame().is_some_and(|f| {
            (f.size.width - host.frame.width).abs() < 1.0
                && (f.size.height - host.frame.height).abs() < 1.0
        })
    };
    // Firefox lists its panel sheet nowhere but as the focused window.
    let app = Element::application(app_pid);
    let windows: Vec<Element> = app
        .element("AXFocusedWindow")
        .into_iter()
        .chain(app.elements("AXWindows"))
        .collect();
    windows
        .iter()
        .flat_map(|w| std::iter::once(w.clone()).chain(w.elements("AXChildren")))
        .find(|el| {
            matches!(el.string("AXRole").as_deref(), Some("AXWindow" | "AXSheet")) && same(el)
        })
        .ok_or_else(|| anyhow!("cannot read the panel in window {}", host.id))
}

/// The panel's element once its contents are readable. They fill in a
/// moment after the panel appears or changes folder, and until then the
/// panel reads as empty: no buttons, no name field.
pub fn loaded(app_pid: i32, host: &WindowInfo, limit: Duration) -> Result<Element> {
    let has_buttons = |p: &Element| {
        parts(p)
            .iter()
            .any(|c| c.string("AXRole").as_deref() == Some("AXButton"))
    };
    wait_for(limit, || element(app_pid, host).ok().filter(has_buttons))
        .map_or_else(|| element(app_pid, host), Ok)
}

/// The panel's controls: its children, looking through the unnamed groups
/// that newer macOS wraps its buttons and fields in, as the tree does.
fn parts(panel: &Element) -> Vec<Element> {
    fn add(el: &Element, depth: usize, out: &mut Vec<Element>) {
        for c in el.elements("AXChildren") {
            let unnamed = |attr: &str| c.string(attr).is_none_or(|s| s.is_empty());
            let group = c
                .string("AXRole")
                .is_some_and(|r| ax::GROUPING.contains(&r.as_str()))
                && unnamed("AXTitle")
                && unnamed("AXDescription");
            if group && depth > 0 {
                add(&c, depth - 1, out);
            } else {
                out.push(c);
            }
        }
    }
    let mut out = Vec::new();
    add(panel, 4, &mut out);
    out
}

/// Which panel a window shows, from its accessibility description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Open,
    Save,
}

pub fn kind(panel: &Element) -> Kind {
    let save = panel
        .string("AXDescription")
        .is_some_and(|d| d.eq_ignore_ascii_case("save"))
        || name_field(panel).is_some();
    if save {
        Kind::Save
    } else {
        Kind::Open
    }
}

/// A save panel's "Save As" field: a text field of its own, not the tag
/// editor or the search field.
fn name_field(panel: &Element) -> Option<Element> {
    parts(panel).into_iter().find(|c| {
        c.string("AXRole").as_deref() == Some("AXTextField")
            && c.string("AXDescription").is_none_or(|d| d.is_empty())
            && c.elements("AXChildren").is_empty()
            && (c.string("AXIdentifier").as_deref() == Some("saveAsNameTextField")
                || c.bool("AXFocused") == Some(true)
                || c.string("AXValue").is_some_and(|v| !v.is_empty()))
    })
}

/// Polls `f` until it gives something or `limit` passes.
pub(crate) fn wait_for<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() > deadline {
            return None;
        }
        sleep(Duration::from_millis(60));
    }
}

/// The "Go to" sheet's path field: a focused text field holding a path,
/// somewhere in the app's windows.
fn go_to_field(app_pid: i32) -> Option<Element> {
    fn find(el: &Element, depth: usize) -> Option<Element> {
        if el.string("AXRole").as_deref() == Some("AXTextField")
            && el.bool("AXFocused") == Some(true)
            && el
                .string("AXValue")
                .is_some_and(|v| v.starts_with('/') || v.starts_with('~'))
        {
            return Some(el.clone());
        }
        if depth == 0 {
            return None;
        }
        el.elements("AXChildren")
            .iter()
            .find_map(|c| find(c, depth - 1))
    }
    let app = Element::application(app_pid);
    app.element("AXFocusedWindow")
        .into_iter()
        .chain(app.elements("AXWindows"))
        .find_map(|w| find(&w, 6))
}

fn key(target: &Target, chord: &str) -> Result<()> {
    for c in parse_chords(chord)? {
        input::press(target, &c)?;
        sleep(Duration::from_millis(30));
    }
    Ok(())
}

/// Makes the panel the service's key window, without which it takes no
/// keys: a click on an empty stretch of its bottom bar, between the
/// buttons there. A panel in a window of its own (TextEdit's) is not key
/// until clicked; a sheet in a browser window is, and the click is harmless.
fn focus_panel(panel: &Element, target: &Target) -> Result<()> {
    let parts = parts(panel);
    let buttons: Vec<CGRect> = parts
        .iter()
        .filter(|c| c.string("AXRole").as_deref() == Some("AXButton"))
        .filter_map(|c| c.frame())
        .collect();
    // The bottom bar's row: that of the lowest button (Cancel, Open).
    let Some(b) = buttons
        .iter()
        .max_by(|a, b| a.origin.y.total_cmp(&b.origin.y))
    else {
        return Ok(());
    };
    let frame = panel.frame().unwrap_or(*b);
    let row = b.origin.y + b.size.height / 2.0;
    // The bar starts where the sidebar ends.
    let left = parts
        .iter()
        .find(|c| c.string("AXRole").as_deref() == Some("AXSplitter"))
        .and_then(|s| s.frame())
        .map(|f| f.origin.x + f.size.width)
        .unwrap_or(frame.origin.x);
    let mut edges = vec![(left, left)];
    edges.extend(
        buttons
            .iter()
            .filter(|f| (f.origin.y + f.size.height / 2.0 - row).abs() < 6.0)
            .map(|f| (f.origin.x, f.origin.x + f.size.width)),
    );
    edges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let gap = edges
        .windows(2)
        .map(|w| (w[0].1, w[1].0))
        .max_by(|a, b| (a.1 - a.0).total_cmp(&(b.1 - b.0)));
    let Some((from, to)) = gap.filter(|(from, to)| to - from > 20.0) else {
        return Ok(());
    };
    let at = CGPoint {
        x: (from + to) / 2.0,
        y: row,
    };
    input::click(target, at, MouseButton::Left, 1, Modifiers::default())?;
    // Keys that come before the panel has become key are lost.
    sleep(Duration::from_millis(400));
    Ok(())
}

/// Shows `path` in the panel through its "Go to" sheet: a folder is opened,
/// a file is selected in its folder. Typing "/" opens the sheet; the path
/// goes in through accessibility, which takes it whole.
fn go_to(app_pid: i32, target: &Target, path: &Path) -> Result<()> {
    let mut field = None;
    for _ in 0..2 {
        key(target, "/")?;
        field = wait_for(Duration::from_secs(1), || go_to_field(app_pid));
        if field.is_some() {
            break;
        }
    }
    let field = field.ok_or_else(|| anyhow!("the panel's Go to sheet did not open"))?;
    field.set("AXValue", &ax::cf_string(&path.to_string_lossy()))?;
    sleep(Duration::from_millis(150));
    key(target, "enter")?;
    if wait_for(Duration::from_secs(3), || {
        go_to_field(app_pid).is_none().then_some(())
    })
    .is_none()
    {
        // Still open: it did not take the path. Close it, leave the panel.
        let _ = key(target, "escape");
        bail!("the panel would not go to {}", path.display());
    }
    // The panel lists the folder's files a moment after.
    sleep(Duration::from_millis(400));
    Ok(())
}

/// Presses the panel's default button (Open, Save) or its Cancel button,
/// once it is enabled. The button is looked up afresh each time, since the
/// panel replaces its elements as it changes folder.
///
/// A panel behind other windows is not drawn, and AppKit re-checks which
/// buttons are enabled only as it draws: Save stays disabled under a name
/// typed into it until something shows the panel. A capture does, and
/// leaves the user's screen alone.
fn press_button(app_pid: i32, host: &WindowInfo, attr: &str, fallback: &str) -> Result<()> {
    let find = || {
        let panel = element(app_pid, host).ok()?;
        panel.element(attr).or_else(|| {
            parts(&panel).into_iter().find(|c| {
                c.string("AXRole").as_deref() == Some("AXButton")
                    && c.string("AXTitle").as_deref() == Some(fallback)
            })
        })
    };
    // The button may not be readable yet (see `loaded`), or not enabled.
    let mut seen = false;
    let Some(button) = wait_for(Duration::from_secs(3), || {
        let found = find();
        seen |= found.is_some();
        let found = found.filter(|b| b.bool("AXEnabled") != Some(false));
        if found.is_none() {
            let _ = crate::capture::capture_composed(host, &[]);
        }
        found
    }) else {
        if !seen {
            bail!("the panel has no {fallback} button");
        }
        bail!("the panel's {fallback} button stays disabled: it does not accept what is selected");
    };
    // Pressing through a remote view reports an error even when it works.
    let _ = button.perform("AXPress");
    Ok(())
}

/// Adds `names` to the panel's selection by command-clicking them: the
/// panel ignores selection set through accessibility.
fn select_more(panel: &Element, target: &Target, names: &[String]) -> Result<()> {
    /// Files and folders the panel lists, with their names ("puppy.png,
    /// 1,280 × 800" is puppy.png's).
    fn items(el: &Element, depth: usize, out: &mut Vec<(String, Element)>) {
        if el.actions().iter().any(|a| a == "AXOpen") {
            if let Some(name) = el.string("AXTitle").or_else(|| el.string("AXLabel")) {
                out.push((name, el.clone()));
            }
        }
        if depth > 0 {
            for c in el.elements("AXChildren") {
                items(&c, depth - 1, out);
            }
        }
    }
    let shown = |name: &str, item: &str| {
        item == name || item.strip_prefix(name).is_some_and(|r| r.starts_with(", "))
    };
    // The listing fills in a moment after the panel arrives in a folder.
    let wanted = wait_for(Duration::from_secs(2), || {
        let mut all = Vec::new();
        items(panel, 10, &mut all);
        names
            .iter()
            .map(|n| {
                all.iter()
                    .find(|(item, _)| shown(n, item))
                    .map(|(_, el)| el.clone())
            })
            .collect::<Option<Vec<Element>>>()
    })
    .ok_or_else(|| anyhow!("the panel does not show all of {names:?} in that folder"))?;
    let cmd = Modifiers {
        meta: true,
        ..Default::default()
    };
    for el in &wanted {
        let _ = el.perform("AXScrollToVisible");
        sleep(Duration::from_millis(100));
        let f = el
            .frame()
            .ok_or_else(|| anyhow!("cannot tell where a file sits in the panel"))?;
        let at = CGPoint {
            x: f.origin.x + f.size.width / 2.0,
            y: f.origin.y + f.size.height / 2.0,
        };
        input::click(target, at, MouseButton::Left, 1, cmd)?;
        sleep(Duration::from_millis(150));
    }
    Ok(())
}

/// Answers the panel showing in `host`, through accessibility and keys
/// sent to the panel service: picks `paths`, or cancels without any.
pub fn answer(app_pid: i32, host: &WindowInfo, target: &Target, paths: &[PathBuf]) -> Result<()> {
    let panel = loaded(app_pid, host, Duration::from_secs(3))?;
    focus_panel(&panel, target)?;
    let gone = || {
        wait_for(Duration::from_secs(3), || {
            crate::capture::window(host.id)
                .is_none_or(|w| !w.on_screen)
                .then_some(())
        })
        .is_some()
    };
    if paths.is_empty() {
        if press_button(app_pid, host, "AXCancelButton", "Cancel").is_err() {
            key(target, "escape")?;
        }
        return Ok(());
    }
    match kind(&panel) {
        Kind::Save => {
            let [path] = paths else {
                bail!("a save panel saves to one path");
            };
            let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
                bail!("{} is not a path to save to", path.display());
            };
            if !dir.is_dir() {
                bail!("there is no folder {}", dir.display());
            }
            let field =
                name_field(&panel).ok_or_else(|| anyhow!("the save panel has no Save As field"))?;
            // The Go to sheet opens on "/" only away from the name field.
            if let Some(view) = parts(&panel).into_iter().find(|c| {
                matches!(
                    c.string("AXRole").as_deref(),
                    Some("AXBrowser" | "AXScrollArea" | "AXOutline" | "AXList")
                ) && c.string("AXDescription").as_deref() != Some("sidebar")
            }) {
                let _ = view.set("AXFocused", CFBoolean::new(true));
            }
            go_to(app_pid, target, dir)?;
            let _ = field.set("AXFocused", CFBoolean::new(true));
            field.set("AXValue", &ax::cf_string(&name.to_string_lossy()))?;
            sleep(Duration::from_millis(150));
            press_button(app_pid, host, "AXDefaultButton", "Save")?;
        }
        Kind::Open => {
            for p in paths {
                if !p.exists() {
                    bail!("there is no {}", p.display());
                }
            }
            let dir = paths[0].parent().unwrap_or(Path::new("/"));
            if paths.iter().any(|p| p.parent() != Some(dir)) {
                bail!("several paths have to be in one folder");
            }
            // Going to the first selects it; the rest are added to it.
            go_to(app_pid, target, &paths[0])?;
            if paths.len() > 1 {
                let names: Vec<String> = paths[1..]
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .collect();
                select_more(&panel, target, &names)?;
            }
            sleep(Duration::from_millis(150));
            press_button(app_pid, host, "AXDefaultButton", "Open")?;
        }
    }
    if !gone() {
        bail!("the panel is still open: look at it for a message (a file to replace, a type it refuses)");
    }
    Ok(())
}
