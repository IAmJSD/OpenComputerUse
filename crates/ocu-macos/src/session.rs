//! Starting apps without bringing them forward, and the session that drives
//! one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
use objc2_core_foundation::{CFBoolean, CGPoint};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSString, NSURL};

use ocu_core::keys::{parse_chord, parse_chords};
use ocu_core::{pick_window, Action, MouseButton, Description, LaunchSpec, Screenshot, Session, TreeOptions, UiNode, WindowInfo};

use crate::ax::{self, Element, ElementTable};
use crate::capture;
use crate::input::{self, Target};

enum Resolved {
    Bundle(PathBuf),
    Binary(PathBuf),
}

const APP_DIRS: &[&str] = &[
    "/Applications",
    "/Applications/Utilities",
    "/System/Applications",
    "/System/Applications/Utilities",
    "/System/Library/CoreServices",
];

fn resolve(app: &str) -> Result<Resolved> {
    let path = Path::new(app);
    if path.exists() {
        if path.extension().is_some_and(|e| e == "app") || path.join("Contents/Info.plist").exists() {
            return Ok(Resolved::Bundle(path.to_path_buf()));
        }
        return Ok(Resolved::Binary(path.to_path_buf()));
    }
    let workspace = NSWorkspace::sharedWorkspace();
    // A bundle id: com.apple.TextEdit.
    if app.contains('.') && !app.contains(' ') && !app.contains('/') {
        if let Some(url) = workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(app)) {
            if let Some(p) = url.path() {
                return Ok(Resolved::Bundle(PathBuf::from(p.to_string())));
            }
        }
    }
    // An app name: "TextEdit", "Visual Studio Code".
    let name = app.trim_end_matches(".app");
    let mut dirs: Vec<PathBuf> = APP_DIRS.iter().map(PathBuf::from).collect();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.insert(0, Path::new(&home).join("Applications"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if let Some(stem) = file.strip_suffix(".app") {
                if stem.eq_ignore_ascii_case(name) {
                    return Ok(Resolved::Bundle(entry.path()));
                }
            }
        }
    }
    // A command on PATH.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(app);
            if candidate.is_file() {
                return Ok(Resolved::Binary(candidate));
            }
        }
    }
    bail!("cannot find an app called \"{app}\" (give a .app path, a bundle id like com.apple.TextEdit, or an app name)")
}

fn bundle_id(bundle: &Path) -> Option<String> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    let b = objc2_foundation::NSBundle::bundleWithURL(&url)?;
    b.bundleIdentifier().map(|s| s.to_string())
}

fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier())
}

/// Whether an app is built on Chromium: its bundle carries a Chromium-based
/// framework (Google Chrome Framework, Electron Framework, …).
fn is_chromium(pid: i32) -> bool {
    let Some(bundle) = running(pid).and_then(|a| a.bundleURL()).and_then(|u| u.path()) else { return false };
    is_chromium_bundle(Path::new(&bundle.to_string()))
}

fn is_chromium_bundle(bundle: &Path) -> bool {
    let frameworks = bundle.join("Contents/Frameworks");
    let marks = ["Chrome", "Chromium", "Electron", "Edge", "Brave", "Vivaldi", "Opera", "Arc", "Helium"];
    std::fs::read_dir(&frameworks)
        .map(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(" Framework.framework") && marks.iter().any(|m| name.contains(m))
            })
        })
        .unwrap_or(false)
}

/// The user's frontmost app and its key window.
fn user_focus() -> Option<(i32, u32)> {
    let pid = frontmost_pid()?;
    let window = Element::application(pid).element("AXFocusedWindow")?.window_id()?;
    Some((pid, window))
}

fn running(pid: i32) -> Option<Retained<NSRunningApplication>> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
}

struct Sendable<T>(T);
unsafe impl<T> Send for Sendable<T> {}

fn open_bundle(bundle: &Path, spec: &LaunchSpec) -> Result<i32> {
    let workspace = NSWorkspace::sharedWorkspace();
    let url = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    let config = NSWorkspaceOpenConfiguration::configuration();
    config.setActivates(false);
    config.setAddsToRecentItems(false);
    config.setCreatesNewApplicationInstance(spec.new_instance);
    if !spec.args.is_empty() {
        let args: Vec<Retained<NSString>> = spec.args.iter().map(|a| NSString::from_str(a)).collect();
        config.setArguments(&NSArray::from_retained_slice(&args));
    }
    if !spec.env.is_empty() {
        let keys: Vec<Retained<NSString>> = spec.env.keys().map(|k| NSString::from_str(k)).collect();
        let values: Vec<Retained<NSString>> = spec.env.values().map(|v| NSString::from_str(v)).collect();
        let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        config.setEnvironment(&NSDictionary::from_retained_objects(&keys, &values));
    }
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |app: *mut NSRunningApplication, error: *mut NSError| {
        let result = match unsafe { app.as_ref() } {
            Some(app) => Ok(app.processIdentifier()),
            None => Err(unsafe { error.as_ref() }
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "unknown error".into())),
        };
        let _ = tx.send(Sendable(result));
    });
    workspace.openApplicationAtURL_configuration_completionHandler(&url, &config, Some(&block));
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Sendable(Ok(pid))) => Ok(pid),
        Ok(Sendable(Err(e))) => bail!("could not open {}: {e}", bundle.display()),
        Err(_) => bail!("{} did not finish launching", bundle.display()),
    }
}

pub fn launch(spec: &LaunchSpec) -> Result<MacSession> {
    let front_before = frontmost_pid();
    let resolved = resolve(&spec.app)?;
    let (pid, launched, child, name) = match resolved {
        Resolved::Bundle(bundle) => {
            let already: Vec<i32> = bundle_id(&bundle)
                .map(|id| {
                    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(&id))
                        .iter()
                        .map(|a| a.processIdentifier())
                        .collect()
                })
                .unwrap_or_default();
            // Chromium builds its web pages' accessibility trees only for an
            // assistive app it recognises; this flag makes it always do so.
            let mut spec = spec.clone();
            if is_chromium_bundle(&bundle) && !spec.args.iter().any(|a| a == "--force-renderer-accessibility") {
                spec.args.insert(0, "--force-renderer-accessibility".into());
            }
            let pid = open_bundle(&bundle, &spec)?;
            let name = bundle.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            (pid, !already.contains(&pid), None, name)
        }
        Resolved::Binary(path) => {
            let mut cmd = Command::new(&path);
            cmd.args(&spec.args).envs(&spec.env);
            if let Some(cwd) = &spec.cwd {
                cmd.current_dir(cwd);
            }
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
            let child = cmd.spawn().with_context(|| format!("starting {}", path.display()))?;
            let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            (child.id() as i32, true, Some(child), name)
        }
    };
    let mut session = MacSession {
        pid,
        name,
        launched,
        child,
        front_before,
        elements: ElementTable::default(),
        closed: false,
        chromium: false,
        asked_for_tree: false,
    };
    session.chromium = is_chromium(session.pid);
    // Wait for a window, putting the user's app back in front if this one
    // grabbed focus while starting.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        session.restore_front();
        if !app_windows(pid).is_empty() || !session.is_alive() || Instant::now() > deadline {
            break;
        }
        sleep(Duration::from_millis(150));
    }
    sleep(Duration::from_millis(300));
    if let Some(before) = session.front_before {
        session.raise_user_app(before);
    }
    Ok(session)
}

/// The app's windows, best first: its main and focused windows, then other
/// real windows, then anything else big enough to matter. Accessibility
/// tells real windows from the helpers apps keep around (Writing Tools
/// buttons, menu bar shadows), and fills in titles when Screen Recording
/// is missing.
fn app_windows(pid: i32) -> Vec<WindowInfo> {
    let mut windows = capture::windows(pid);
    let app = Element::application(pid);
    let main = app.element("AXMainWindow").and_then(|w| w.window_id());
    let focused = app.element("AXFocusedWindow").and_then(|w| w.window_id());
    let mut ax_rank = std::collections::HashMap::new();
    for el in app.elements("AXWindows") {
        let Some(id) = el.window_id() else { continue };
        let subrole = el.string("AXSubrole").unwrap_or_default();
        let rank = if Some(id) == main {
            0
        } else if Some(id) == focused {
            1
        } else if matches!(subrole.as_str(), "AXStandardWindow" | "AXDialog" | "AXSystemDialog" | "AXFloatingWindow") {
            2
        } else {
            3
        };
        ax_rank.insert(id as u64, rank);
        if let Some(w) = windows.iter_mut().find(|w| w.id == id as u64 && w.title.is_empty()) {
            w.title = el.string("AXTitle").unwrap_or_default();
        }
    }
    if !ax_rank.is_empty() {
        windows.retain(|w| ax_rank.contains_key(&w.id) || (w.on_screen && w.frame.width >= 100.0 && w.frame.height >= 60.0));
    }
    windows.sort_by_key(|w| (ax_rank.get(&w.id).copied().unwrap_or(4), !w.on_screen));
    windows
}

pub struct MacSession {
    pid: i32,
    name: String,
    launched: bool,
    child: Option<Child>,
    front_before: Option<i32>,
    elements: ElementTable,
    closed: bool,
    /// Built on Chromium (Chrome, Electron and kin), which needs its own
    /// click sequence and has to be asked to build its web accessibility
    /// tree.
    chromium: bool,
    /// Whether that request has been made.
    asked_for_tree: bool,
}

impl MacSession {
    /// Gives focus back to whatever had it before the launch, so the session
    /// stays in the background.
    fn restore_front(&self) {
        let (Some(before), Some(now)) = (self.front_before, frontmost_pid()) else { return };
        if now == self.pid && before != self.pid {
            self.raise_user_app(before);
        }
    }

    /// Puts the user's app's windows back above the session's. Opening
    /// without activating still stacks an app's new windows on top.
    fn raise_user_app(&self, pid: i32) {
        if pid == self.pid {
            return;
        }
        if !crate::sky::bring_to_front(pid) {
            if let Some(app) = running(pid) {
                #[allow(deprecated)]
                app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
            }
        }
    }

    fn window(&mut self, window: Option<u64>) -> Result<WindowInfo> {
        let windows = app_windows(self.pid);
        pick_window(&windows, window).cloned()
    }

    fn target(&self, w: &WindowInfo) -> Target {
        Target { pid: self.pid, window_id: w.id as u32, origin: CGPoint { x: w.frame.x, y: w.frame.y } }
    }

    fn point(w: &WindowInfo, x: f64, y: f64) -> CGPoint {
        CGPoint { x: w.frame.x + x, y: w.frame.y + y }
    }
}

impl Session for MacSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        if !self.launched {
            details.insert("attached".into(), "already running; it is left open when the session ends".into());
        }
        if let Some(app) = running(self.pid) {
            if let Some(id) = app.bundleIdentifier() {
                details.insert("bundle_id".into(), id.to_string());
            }
        }
        Description { app: self.name.clone(), pid: Some(self.pid as u32), details }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(app_windows(self.pid))
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        capture::capture(&w)
    }

    /// Raises the window over everything for a moment so an app that stopped
    /// drawing it while covered draws it again, captures it, then puts the
    /// user's app back on top.
    fn screenshot_uncovered(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        let front = frontmost_pid();
        let el = ax::window_element(self.pid, Some(w.id as u32))?;
        el.perform("AXRaise")?;
        // Long enough for the occlusion change to reach the app and a frame
        // or two to be drawn.
        sleep(Duration::from_millis(180));
        let shot = capture::capture(&w);
        if let Some(front) = front {
            self.raise_user_app(front);
        }
        shot
    }

    fn ui_tree(&mut self, window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        // Chromium and Electron build their web content's accessibility tree
        // only once an assistive app asks; other apps ignore the attribute.
        if !self.asked_for_tree {
            self.asked_for_tree = true;
            // Electron honours AXManualAccessibility; a Chromium browser
            // we did not start (so without the flag) sometimes answers to
            // AXEnhancedUserInterface. Both report errors even when they work.
            let app = Element::application(self.pid);
            let _ = app.set("AXManualAccessibility", CFBoolean::new(true));
            if self.chromium {
                let _ = app.set("AXEnhancedUserInterface", CFBoolean::new(true));
                sleep(Duration::from_millis(500));
            }
        }
        let w = self.window(window)?;
        let root = ax::window_element(self.pid, Some(w.id as u32))
            .or_else(|_| ax::window_element(self.pid, None))?;
        let origin = CGPoint { x: w.frame.x, y: w.frame.y };
        Ok(ax::read_tree(&root, origin, opts, &mut self.elements))
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::ElementAction { element, name } => {
                let el = self.elements.get(element)?;
                let action = ax::action_name(name.as_deref().unwrap_or("press"));
                return el.perform(&action);
            }
            Action::SetValue { element, value } => {
                let el = self.elements.get(element)?;
                if el.settable("AXFocused") {
                    let _ = el.set("AXFocused", CFBoolean::new(true));
                }
                return el.set("AXValue", &ax::cf_string(value));
            }
            Action::Focus { element } => {
                let el = self.elements.get(element)?;
                el.set("AXFocused", CFBoolean::new(true))?;
                // Bring its window's focus with it, still without raising.
                if let Some(w) = el.element("AXWindow").and_then(|w| w.window_id()) {
                    let _ = crate::sky::focus_without_raise(self.pid, w, None);
                }
                return Ok(());
            }
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                return Ok(());
            }
            _ => {}
        }
        let w = self.window(window)?;
        let t = self.target(&w);
        let _focus = t.prepare(user_focus());
        match action {
            Action::Click { x, y, button, count, modifiers } => {
                let m = match modifiers.as_deref().filter(|s| !s.is_empty()) {
                    Some(s) => parse_chord(s)?.modifiers,
                    None => Default::default(),
                };
                if self.chromium && *button == MouseButton::Left {
                    input::click_chromium(&t, Self::point(&w, *x, *y), *count, m)
                } else {
                    input::click(&t, Self::point(&w, *x, *y), *button, *count, m)
                }
            }
            Action::MoveMouse { x, y } => input::move_to(&t, Self::point(&w, *x, *y)),
            Action::Drag { from_x, from_y, to_x, to_y, button } => {
                input::drag(&t, Self::point(&w, *from_x, *from_y), Self::point(&w, *to_x, *to_y), *button)
            }
            Action::Scroll { x, y, dx, dy } => input::scroll(&t, Self::point(&w, *x, *y), *dx, *dy),
            Action::TypeText { text } => input::type_text(&t, text),
            Action::PressKey { keys } => {
                for chord in parse_chords(keys)? {
                    input::press(&t, &chord)?;
                    sleep(Duration::from_millis(30));
                }
                Ok(())
            }
            Action::ElementAction { .. } | Action::SetValue { .. } | Action::Focus { .. } | Action::Wait { .. } => {
                unreachable!()
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            return matches!(child.try_wait(), Ok(None));
        }
        running(self.pid).is_some_and(|a| !a.isTerminated())
    }

    fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) || !self.launched {
            return;
        }
        if let Some(mut child) = self.child.take() {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                sleep(Duration::from_millis(50));
            }
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            return;
        }
        let Some(app) = running(self.pid) else { return };
        app.terminate();
        // An unsaved-changes prompt would hold it open forever.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !app.isTerminated() {
            sleep(Duration::from_millis(50));
        }
        if !app.isTerminated() {
            app.forceTerminate();
        }
    }
}

impl Drop for MacSession {
    fn drop(&mut self) {
        self.close();
    }
}
