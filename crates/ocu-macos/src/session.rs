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
use objc2_app_kit::{
    NSApplicationActivationOptions, NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_core_foundation::{CFBoolean, CGPoint};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSString, NSURL};

use ocu_core::keys::{parse_chord, parse_chords};
use ocu_core::{
    pick_window, Action, Description, LaunchSpec, MouseButton, Screenshot, Session, TreeOptions,
    UiNode, WindowInfo,
};

use crate::ax::{self, Element, ElementTable};
use crate::bidi;
use crate::capture;
use crate::cdp::{self, Cdp};
use crate::input::{self, Target};
use crate::pages::PageFiles;
use crate::panel;

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
        if path.extension().is_some_and(|e| e == "app") || path.join("Contents/Info.plist").exists()
        {
            return Ok(Resolved::Bundle(path.to_path_buf()));
        }
        return Ok(Resolved::Binary(path.to_path_buf()));
    }
    let workspace = NSWorkspace::sharedWorkspace();
    // A bundle id: com.apple.TextEdit.
    if app.contains('.') && !app.contains(' ') && !app.contains('/') {
        if let Some(url) = workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(app))
        {
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
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
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

pub(crate) fn bundle_executable(bundle: &Path) -> Option<PathBuf> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    let b = objc2_foundation::NSBundle::bundleWithURL(&url)?;
    b.executableURL()?
        .path()
        .map(|p| PathBuf::from(p.to_string()))
}

/// Starts a Chromium browser with a DevTools pipe, so a page's file
/// chooser is answered without the open panel showing. Only for a profile
/// of its own: Chrome refuses remote debugging of the user's default
/// profile, and with that profile already open the new process would hand
/// over to the running one. `None` when it does not apply or fails.
fn spawn_with_devtools(bundle: &Path, spec: &LaunchSpec, running: bool) -> Option<(Child, Cdp)> {
    let own_profile = spec.args.iter().any(|a| a.starts_with("--user-data-dir"));
    if !own_profile || (running && !spec.new_instance) {
        return None;
    }
    let exe = bundle_executable(bundle)?;
    match cdp::spawn(&exe, spec) {
        Ok(started) => Some(started),
        Err(e) => {
            log::warn!("starting {} with DevTools: {e:#}", bundle.display());
            None
        }
    }
}

fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|a| a.processIdentifier())
}

/// How long a browser window stays focused after a click or press in its
/// page. A page only gets the clipboard while its document has focus, and a
/// Copy button often writes it after an await; give the focus back at once
/// and the write fails with "Document is not focused".
const PAGE_HANDLER_GRACE: Duration = Duration::from_millis(250);

const CHROMIUM_FLAGS: &[&str] = &[
    "--force-renderer-accessibility",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-background-timer-throttling",
];

/// Whether an app is built on Chromium: its bundle carries a Chromium-based
/// framework (Google Chrome Framework, Electron Framework, …).
fn is_chromium(pid: i32) -> bool {
    let Some(bundle) = running(pid)
        .and_then(|a| a.bundleURL())
        .and_then(|u| u.path())
    else {
        return false;
    };
    is_chromium_bundle(Path::new(&bundle.to_string()))
}

/// Whether an app is built on Gecko (Firefox, and kin like LibreWolf, Zen
/// and Thunderbird): its bundle carries Gecko's XUL library.
fn is_gecko_bundle(bundle: &Path) -> bool {
    bundle.join("Contents/MacOS/XUL").exists()
}

fn is_chromium_bundle(bundle: &Path) -> bool {
    let frameworks = bundle.join("Contents/Frameworks");
    let marks = [
        "Chrome", "Chromium", "Electron", "Edge", "Brave", "Vivaldi", "Opera", "Arc", "Helium",
    ];
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
    let window = Element::application(pid)
        .element("AXFocusedWindow")?
        .window_id()?;
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
    config.setActivates(spec.foreground);
    config.setAddsToRecentItems(false);
    config.setCreatesNewApplicationInstance(spec.new_instance);
    if !spec.args.is_empty() {
        let args: Vec<Retained<NSString>> =
            spec.args.iter().map(|a| NSString::from_str(a)).collect();
        config.setArguments(&NSArray::from_retained_slice(&args));
    }
    if !spec.env.is_empty() {
        let keys: Vec<Retained<NSString>> =
            spec.env.keys().map(|k| NSString::from_str(k)).collect();
        let values: Vec<Retained<NSString>> =
            spec.env.values().map(|v| NSString::from_str(v)).collect();
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
    let mut successor_of: Option<(String, Vec<i32>)> = None;
    let mut gecko = false;
    let mut pages: Option<Box<dyn PageFiles>> = None;
    let mut bidi_profile = None;
    let (pid, launched, child, name) = match resolved {
        Resolved::Bundle(bundle) => {
            let already: Vec<i32> = bundle_id(&bundle)
                .map(|id| {
                    NSRunningApplication::runningApplicationsWithBundleIdentifier(
                        &NSString::from_str(&id),
                    )
                    .iter()
                    .map(|a| a.processIdentifier())
                    .collect()
                })
                .unwrap_or_default();
            // Chromium builds its web pages' accessibility trees only for an
            // assistive app it recognises, and stops rendering (and slows the
            // timers of) pages in windows it thinks are hidden, which ours are
            // as far as it can tell. These switches undo both.
            let mut spec = spec.clone();
            if is_chromium_bundle(&bundle) {
                for flag in CHROMIUM_FLAGS.iter().rev() {
                    if !spec.args.iter().any(|a| a == flag) {
                        spec.args.insert(0, flag.to_string());
                    }
                }
            }
            // Known from the bundle rather than the process: Firefox
            // relaunches itself, so the process we start may be gone already.
            gecko = is_gecko_bundle(&bundle);
            let name = bundle
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let piped = is_chromium_bundle(&bundle)
                .then(|| spawn_with_devtools(&bundle, &spec, !already.is_empty()))
                .flatten();
            // Firefox with a profile of its own takes WebDriver BiDi, which
            // hands us its pages' file choosers; it is connected to once it
            // has started.
            if gecko && (already.is_empty() || spec.new_instance) {
                if let Some(profile) = bidi::profile_dir(&spec.args) {
                    bidi::clear(&profile);
                    spec.args.push(bidi::ARG.to_string());
                    bidi_profile = Some(profile);
                }
            }
            if let Some((child, c)) = piped {
                pages = Some(Box::new(c));
                (child.id() as i32, true, Some(child), name)
            } else {
                let pid = open_bundle(&bundle, &spec)?;
                if let Some(id) = bundle_id(&bundle) {
                    let mut known = already.clone();
                    known.push(pid);
                    successor_of = Some((id, known));
                }
                (pid, !already.contains(&pid), None, name)
            }
        }
        Resolved::Binary(path) => {
            let mut cmd = Command::new(&path);
            cmd.args(&spec.args).envs(&spec.env);
            if let Some(cwd) = &spec.cwd {
                cmd.current_dir(cwd);
            }
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
            let child = cmd
                .spawn()
                .with_context(|| format!("starting {}", path.display()))?;
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
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
        gecko,
        asked_for_tree: false,
        successor_of,
        launched_at: Instant::now(),
        foreground: spec.foreground,
        pinned: None,
        pages,
    };
    session.chromium = is_chromium(session.pid);
    // Wait for a window, putting the user's app back in front if this one
    // grabbed focus while starting.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if !session.foreground {
            session.restore_front();
        }
        // A relaunching app is briefly without a process; give its
        // successor a moment to appear before calling it gone.
        let gone = !session.is_alive() && session.launched_at.elapsed() > Duration::from_secs(8);
        if !app_windows(session.pid).is_empty() || gone || Instant::now() > deadline {
            break;
        }
        sleep(Duration::from_millis(150));
    }
    sleep(Duration::from_millis(300));
    if let Some(profile) = bidi_profile {
        match bidi::connect(&profile, Duration::from_secs(10)) {
            Ok(b) => session.pages = Some(Box::new(b)),
            Err(e) => log::warn!("Firefox's WebDriver BiDi: {e:#}"),
        }
    }
    if session.foreground {
        let first = app_windows(session.pid).into_iter().next();
        session.bring_forward(first.as_ref());
    } else if let Some(before) = session.front_before {
        session.raise_user_app(before);
    }
    Ok(session)
}

/// A session on the window in front, skipping the client's own apps.
pub fn attach_active(spec: &LaunchSpec) -> Result<MacSession> {
    let mut skip: Vec<i32> = spec.skip_pids.iter().map(|&p| p as i32).collect();
    skip.push(std::process::id() as i32);
    let Some((pid, window)) = capture::front_window(&skip) else {
        bail!("no window is in front, other than the client's own");
    };
    let name = running(pid)
        .and_then(|a| a.localizedName())
        .map(|n| n.to_string())
        .unwrap_or_default();
    let session = MacSession {
        pid,
        name,
        launched: false,
        child: None,
        front_before: frontmost_pid(),
        elements: ElementTable::default(),
        closed: false,
        chromium: is_chromium(pid),
        gecko: running(pid)
            .and_then(|a| a.bundleURL())
            .and_then(|u| u.path())
            .is_some_and(|p| is_gecko_bundle(Path::new(&p.to_string()))),
        asked_for_tree: false,
        successor_of: None,
        launched_at: Instant::now(),
        foreground: spec.foreground,
        pinned: Some(window.id),
        pages: None,
    };
    if session.foreground {
        session.bring_forward(Some(&window));
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
        } else if matches!(
            subrole.as_str(),
            "AXStandardWindow" | "AXDialog" | "AXSystemDialog" | "AXFloatingWindow"
        ) {
            2
        } else {
            3
        };
        ax_rank.insert(id as u64, rank);
        if let Some(w) = windows
            .iter_mut()
            .find(|w| w.id == id as u64 && w.title.is_empty())
        {
            w.title = el.string("AXTitle").unwrap_or_default();
        }
    }
    if !ax_rank.is_empty() {
        windows.retain(|w| {
            ax_rank.contains_key(&w.id)
                || (w.on_screen && w.frame.width >= 100.0 && w.frame.height >= 60.0)
        });
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
    /// Built on Gecko (Firefox and kin), which builds no accessibility tree
    /// at all until asked, and drops a background click unless it comes the
    /// way Chromium wants one.
    gecko: bool,
    /// Whether that request has been made.
    asked_for_tree: bool,
    /// The bundle id, and the processes of it that are not ours, for
    /// following an app that relaunches itself as it starts (Firefox does
    /// with a new profile): the process we started exits, and the one that
    /// replaces it is the app.
    successor_of: Option<(String, Vec<i32>)>,
    launched_at: Instant,
    /// Started with [`LaunchSpec::foreground`]: the app is brought to the
    /// front before every action and left there.
    foreground: bool,
    /// The window the session was attached to, which comes first in its
    /// window list while it exists.
    pinned: Option<u64>,
    /// A browser's link that takes its pages' file choosers: the DevTools
    /// pipe of a Chromium browser, or Firefox's WebDriver BiDi.
    pages: Option<Box<dyn PageFiles>>,
}

impl MacSession {
    /// Makes the app frontmost and `window` its main, topmost window.
    fn bring_forward(&self, window: Option<&WindowInfo>) {
        if frontmost_pid() != Some(self.pid) && !crate::sky::bring_to_front(self.pid) {
            if let Some(app) = running(self.pid) {
                #[allow(deprecated)]
                app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
            }
        }
        if let Some(w) = window {
            if let Ok(el) = ax::window_element(self.pid, Some(w.id as u32)) {
                let _ = el.perform("AXRaise");
                let _ = el.set("AXMain", CFBoolean::new(true));
            }
        }
        // Long enough for the activation to reach the app before input does.
        sleep(Duration::from_millis(100));
    }

    /// Gives focus back to whatever had it before the launch, so the session
    /// stays in the background.
    fn restore_front(&self) {
        let (Some(before), Some(now)) = (self.front_before, frontmost_pid()) else {
            return;
        };
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

    /// Follows an app that replaced its first process early on: adopts a
    /// process of the same app that appeared after our launch.
    fn adopt_successor(&mut self) -> bool {
        let Some((id, known)) = &self.successor_of else {
            return false;
        };
        if self.launched_at.elapsed() > Duration::from_secs(60) {
            return false;
        }
        let next =
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(id))
                .iter()
                .map(|a| a.processIdentifier())
                .find(|p| !known.contains(p));
        match next {
            Some(pid) => {
                log::info!(
                    "{} relaunched itself: following pid {} to {pid}",
                    self.name,
                    self.pid
                );
                self.pid = pid;
                if let Some((_, known)) = &mut self.successor_of {
                    known.push(pid);
                }
                true
            }
            None => false,
        }
    }

    /// The app's windows, best first, with an open or save panel leading
    /// (it holds up the window it belongs to), then the pinned one.
    fn listed(&self) -> Vec<WindowInfo> {
        let mut windows = app_windows(self.pid);
        if let Some(i) = windows.iter().position(|w| Some(w.id) == self.pinned) {
            let w = windows.remove(i);
            windows.insert(0, w);
        }
        let panels = panel::remotes(&windows);
        if let Some(i) = windows.iter().position(|w| panels.contains_key(&w.id)) {
            let w = windows.remove(i);
            windows.insert(0, w);
        }
        windows
    }

    fn window(&mut self, window: Option<u64>) -> Result<WindowInfo> {
        let windows = self.listed();
        pick_window(&windows, window).cloned()
    }

    /// Where input for `w` goes: the app, or the panel service when `w`
    /// shows an open or save panel.
    fn target(&self, w: &WindowInfo) -> Target {
        let (pid, window_id) = match panel::remotes(std::slice::from_ref(w)).get(&w.id) {
            Some(r) => (r.pid, r.window),
            None => (self.pid, w.id as u32),
        };
        Target {
            pid,
            window_id,
            paced: pid != self.pid,
            origin: CGPoint {
                x: w.frame.x,
                y: w.frame.y,
            },
        }
    }

    fn point(w: &WindowInfo, x: f64, y: f64) -> CGPoint {
        CGPoint {
            x: w.frame.x + x,
            y: w.frame.y + y,
        }
    }

    /// Presses an element in a browser's page. A bare AXPress leaves the
    /// browser's keyboard focus where it was (typing after pressing a field
    /// goes on into the address bar) and never gives the document focus (a
    /// Copy button fails with "Document is not focused"). When the element
    /// is the one under its own centre this clicks there instead, which does
    /// both; when it is scrolled away or covered, it moves keyboard focus
    /// onto the element and presses it.
    fn press_in_page(&mut self, el: &Element) -> Result<()> {
        let window = el.element("AXWindow").and_then(|w| w.window_id());
        if let (Some(id), Some(f)) = (window, el.frame()) {
            let at = CGPoint {
                x: f.origin.x + f.size.width / 2.0,
                y: f.origin.y + f.size.height / 2.0,
            };
            let visible = f.size.width >= 1.0 && f.size.height >= 1.0;
            if visible && ax::element_at(self.pid, at).is_some_and(|hit| ax::is_within(&hit, el)) {
                let w = self.window(Some(id as u64))?;
                return self.perform(
                    Some(id as u64),
                    &Action::Click {
                        x: at.x - w.frame.x,
                        y: at.y - w.frame.y,
                        button: MouseButton::Left,
                        count: 1,
                        modifiers: None,
                    },
                );
            }
        }
        if el.settable("AXFocused") {
            let _ = el.set("AXFocused", CFBoolean::new(true));
        }
        el.perform("AXPress")
    }

    /// The window showing an open or save panel: `window` when it does,
    /// else the first that does.
    fn panel_window(&self, window: Option<u64>) -> Option<WindowInfo> {
        let windows = self.listed();
        let panels = panel::remotes(&windows);
        let mut shown: Vec<WindowInfo> = windows
            .into_iter()
            .filter(|w| panels.contains_key(&w.id))
            .collect();
        let i = shown.iter().position(|w| Some(w.id) == window).unwrap_or(0);
        (!shown.is_empty()).then(|| shown.swap_remove(i))
    }

    /// The browser link with a page's file chooser waiting on it, and
    /// whether it takes several files.
    fn page_chooser(&self) -> Option<(&dyn PageFiles, bool)> {
        let p = self.pages.as_deref().filter(|p| p.is_alive())?;
        Some((p, p.waiting()?))
    }

    /// Answers whatever is asking for files: a page's chooser held by the
    /// browser link, or a panel on screen.
    fn choose_file(&mut self, window: Option<u64>, paths: &[String]) -> Result<()> {
        let paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| panel::absolute(p))
            .collect::<Result<_>>()?;
        // What asked may still be on its way: a click's chooser or panel
        // comes a moment after the click.
        let deadline = Instant::now() + Duration::from_secs(2);
        let w = loop {
            if let Some((pages, _)) = self.page_chooser() {
                return pages.answer(&paths);
            }
            if let Some(w) = self.panel_window(window) {
                break w;
            }
            if Instant::now() > deadline {
                bail!("the app is not showing an open or save panel, and no page is asking for a file: do what opens one first (an upload button, File → Open)");
            }
            sleep(Duration::from_millis(100));
        };
        let t = self.target(&w);
        let _focus = t.prepare(user_focus());
        panel::answer(self.pid, &w, &t, &paths)
    }

    /// Picks an option of a pop-up button (a web page's <select>) by its
    /// text. Firefox says its AXValue was set and ignores it, and neither
    /// it nor Chrome opens the menu in the background, so the option is
    /// typed to the focused button, which selects the first option starting
    /// with what was typed. Focus comes through AXFocused, not a click: a
    /// background click marks the menu open without showing it, and the
    /// hidden menu then swallows every key until the next click.
    fn choose(&mut self, window: Option<u64>, el: &Element, value: &str) -> Result<()> {
        let matches = |el: &Element| {
            el.string("AXValue")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case(value.trim()))
        };
        if matches(el) {
            return Ok(());
        }
        let w = self.window(window)?;
        let t = self.target(&w);
        let _focus = t.prepare(user_focus());
        let _ = el.set("AXFocused", CFBoolean::new(true));
        sleep(Duration::from_millis(100));
        for attempt in 0..2 {
            if attempt > 0 {
                // What was typed within the last second still counts, so a
                // second pick soon after the first searches for both: wait
                // that out and type again.
                sleep(Duration::from_millis(1100));
            }
            input::type_text(&t, value)?;
            sleep(Duration::from_millis(150));
            if matches(el) {
                return Ok(());
            }
        }
        bail!(
            "{value:?} did not select: the pop-up button shows {:?}. Pass an option's text exactly as the page shows it",
            el.string("AXValue").unwrap_or_default()
        )
    }
}

/// Picks an option of an AppKit pop-up button by its text. Its AXValue
/// says it was set and changes nothing, and it lists its items only while
/// its menu is open: so the menu is opened, the item pressed, and the menu
/// is gone again a moment later.
fn choose_native(el: &Element, value: &str) -> Result<()> {
    let is = |s: &str| s.trim().eq_ignore_ascii_case(value.trim());
    if el.string("AXValue").is_some_and(|v| is(&v)) {
        return Ok(());
    }
    let menu_items = || -> Vec<Element> {
        el.elements("AXChildren")
            .into_iter()
            .filter(|c| c.string("AXRole").as_deref() == Some("AXMenu"))
            .flat_map(|m| m.elements("AXChildren"))
            .filter(|i| i.string("AXRole").as_deref() == Some("AXMenuItem"))
            .collect()
    };
    let close = || {
        for m in el.elements("AXChildren") {
            let _ = m.perform("AXCancel");
        }
    };
    let _ = el.perform("AXPress");
    let Some(items) = panel::wait_for(Duration::from_secs(1), || {
        Some(menu_items()).filter(|i| !i.is_empty())
    }) else {
        bail!("the pop-up button did not open its menu");
    };
    let Some(item) = items
        .iter()
        .find(|i| i.string("AXTitle").is_some_and(|t| is(&t)))
    else {
        let options: Vec<String> = items.iter().filter_map(|i| i.string("AXTitle")).collect();
        close();
        bail!("{value:?} is not one of the options: {options:?}");
    };
    item.perform("AXPress")?;
    if panel::wait_for(Duration::from_secs(1), || {
        el.string("AXValue").filter(|v| is(v))
    })
    .is_none()
    {
        close();
        bail!(
            "{value:?} did not select: the pop-up button shows {:?}",
            el.string("AXValue").unwrap_or_default()
        );
    }
    // The button acts on the choice once its menu has closed.
    let _ = panel::wait_for(Duration::from_secs(1), || {
        menu_items().is_empty().then_some(())
    });
    Ok(())
}

impl Session for MacSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        if self.foreground {
            details.insert(
                "foreground".into(),
                "brought to the front before every action".into(),
            );
        }
        if self.pinned.is_some() {
            details.insert(
                "active_window".into(),
                "attached to the window that was in front; it comes first".into(),
            );
        }
        if !self.launched {
            details.insert(
                "attached".into(),
                "already running; it is left open when the session ends".into(),
            );
        }
        if let Some(app) = running(self.pid) {
            if let Some(id) = app.bundleIdentifier() {
                details.insert("bundle_id".into(), id.to_string());
            }
        }
        Description {
            app: self.name.clone(),
            pid: Some(self.pid as u32),
            details,
            ..Default::default()
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(self.listed())
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        // The app's window around a panel only hosts the panel service's
        // drawing, which a capture of either window alone misses.
        let windows = self.listed();
        let panels = panel::remotes(&windows);
        if panels.contains_key(&w.id) {
            let with: Vec<u64> = panels.keys().copied().filter(|&id| id != w.id).collect();
            return capture::capture_composed(&w, &with);
        }
        capture::capture(&w)
    }

    /// Raises the window over everything for a moment so an app that stopped
    /// drawing it while covered draws it again, captures it, then puts the
    /// user's app back on top.
    fn screenshot_uncovered(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        if self.foreground {
            self.bring_forward(Some(&w));
            sleep(Duration::from_millis(80));
            return capture::capture(&w);
        }
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
        let mut first_read = false;
        if !self.asked_for_tree {
            self.asked_for_tree = true;
            // Electron honours AXManualAccessibility; a Chromium browser
            // we did not start (so without the flag) sometimes answers to
            // AXEnhancedUserInterface, and Firefox only to that one: without
            // it its windows show nothing but their close and zoom buttons.
            // Both report errors even when they work.
            let app = Element::application(self.pid);
            let _ = app.set("AXManualAccessibility", CFBoolean::new(true));
            if self.chromium || self.gecko {
                let _ = app.set("AXEnhancedUserInterface", CFBoolean::new(true));
                sleep(Duration::from_millis(500));
                first_read = self.gecko;
            }
        }
        let w = self.window(window)?;
        let root = ax::window_element(self.pid, Some(w.id as u32))
            .or_else(|_| ax::window_element(self.pid, None))?;
        let origin = CGPoint {
            x: w.frame.x,
            y: w.frame.y,
        };
        if first_read {
            // Firefox's first read of a page builds its tree, and gives some
            // elements frames from before layout (a button at -43,-75 that
            // sits at 100,185): clicked there, they miss. A read after that
            // one has them right.
            let _ = ax::read_tree(&root, origin, opts, &mut self.elements);
            sleep(Duration::from_millis(250));
        }
        Ok(ax::read_tree(&root, origin, opts, &mut self.elements))
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        if self.foreground && !matches!(action, Action::Wait { .. }) {
            let w = self.window(window).ok();
            self.bring_forward(w.as_ref());
        }
        match action {
            Action::ElementAction { element, name } => {
                let el = self.elements.get(element)?.clone();
                let action = ax::action_name(name.as_deref().unwrap_or("press"));
                if (self.chromium || self.gecko) && action == "AXPress" && ax::in_web_area(&el) {
                    return self.press_in_page(&el);
                }
                return el.perform(&action);
            }
            Action::SetValue { element, value } => {
                let el = self.elements.get(element)?.clone();
                if el.string("AXRole").as_deref() == Some("AXPopUpButton") {
                    if (self.chromium || self.gecko) && ax::in_web_area(&el) {
                        return self.choose(window, &el, value);
                    }
                    return choose_native(&el, value);
                }
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
            Action::ChooseFile { paths } => return self.choose_file(window, paths),
            _ => {}
        }
        let w = self.window(window)?;
        let t = self.target(&w);
        // In front already, the app takes input without borrowing focus.
        let _focus = if self.foreground {
            None
        } else {
            t.prepare(user_focus())
        };
        match action {
            Action::Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => {
                let m = match modifiers.as_deref().filter(|s| !s.is_empty()) {
                    Some(s) => parse_chord(s)?.modifiers,
                    None => Default::default(),
                };
                // A panel is AppKit's own, whatever the app is built on.
                let in_app = t.pid == self.pid;
                if (self.chromium || self.gecko) && in_app && *button == MouseButton::Left {
                    let clicked = input::click_chromium(&t, Self::point(&w, *x, *y), *count, m);
                    // Keep the window focused while the page handles it.
                    sleep(PAGE_HANDLER_GRACE);
                    clicked
                } else {
                    input::click(&t, Self::point(&w, *x, *y), *button, *count, m)
                }
            }
            Action::MoveMouse { x, y } => input::move_to(&t, Self::point(&w, *x, *y)),
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
            } => input::drag(
                &t,
                Self::point(&w, *from_x, *from_y),
                Self::point(&w, *to_x, *to_y),
                *button,
            ),
            Action::Scroll { x, y, dx, dy } => input::scroll(&t, Self::point(&w, *x, *y), *dx, *dy),
            Action::TypeText { text } => input::type_text(&t, text),
            Action::PressKey { keys } => {
                for chord in parse_chords(keys)? {
                    input::press(&t, &chord)?;
                    sleep(Duration::from_millis(30));
                }
                Ok(())
            }
            Action::ElementAction { .. }
            | Action::SetValue { .. }
            | Action::Focus { .. }
            | Action::Wait { .. }
            | Action::ChooseFile { .. } => {
                unreachable!()
            }
        }
    }

    fn notice(&mut self) -> Option<String> {
        if let Some((_, multiple)) = self.page_chooser() {
            return Some(format!(
                "The page is asking for {} (a file input; no panel is shown): answer it with choose_file, giving the paths, or no paths to cancel.",
                if multiple { "files" } else { "a file" }
            ));
        }
        let w = self.panel_window(None)?;
        let kind = panel::element(self.pid, &w)
            .map(|el| panel::kind(&el))
            .ok()?;
        Some(match kind {
            panel::Kind::Open => format!("An open panel is showing (window {}): answer it with choose_file, giving the paths to pick, or no paths to cancel.", w.id),
            panel::Kind::Save => format!("A save panel is showing (window {}): answer it with choose_file, giving the path to save to, or no paths to cancel.", w.id),
        })
    }

    fn is_alive(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            return matches!(child.try_wait(), Ok(None));
        }
        if running(self.pid).is_some_and(|a| !a.isTerminated()) {
            return true;
        }
        self.adopt_successor()
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
