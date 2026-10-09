//! The Windows backend. Apps start inside a kill-on-close Job Object
//! (so they die with the MCP server), shown without activating and sent to
//! the back (or, for a foreground session, brought to the front before
//! every action); they are captured with `PrintWindow`, read and pressed
//! through UI Automation, and sent input as window messages.
#![cfg(windows)]

mod capture;
mod dialog;
mod hook;
mod inject;
mod input;
mod launch;
mod uia;

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SetForegroundWindow,
    SetWindowPos, ShowWindow, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE,
};

use ocu_core::keys::{parse_chord, parse_chords};
use ocu_core::{
    pick_window, Action, Description, LaunchSpec, Platform, Screenshot, Session, TreeOptions,
    UiNode, WindowInfo,
};

pub struct WindowsPlatform {
    /// Whether to load the panel hook into the apps sessions start.
    hook: bool,
}

impl WindowsPlatform {
    pub fn new() -> Self {
        // Physical pixels everywhere, so screenshots and coordinates agree
        // on high-DPI screens.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        uia::com_init();
        let hook = hook::from_env();
        hook::set_wanted(hook);
        Self { hook }
    }

    /// A platform that loads the panel hook when `hook` is set. The agent
    /// passes the user's own setting; [`new`] alone takes it from the
    /// environment.
    pub fn with_hook(hook: bool) -> Self {
        hook::set_wanted(hook);
        Self { hook }
    }
}

impl Default for WindowsPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn permissions(&self) -> Vec<ocu_core::Permission> {
        vec![ocu_core::Permission {
            name: "Panel hook".into(),
            granted: self.hook,
            help: "Hands apps' open and save dialogs straight to the agent, so they never show. \
                   Loads a small library into the apps a session starts, which endpoint protection \
                   may object to; dialogs are answered on screen instead without it."
                .into(),
            optional: true,
        }]
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn Session>> {
        if spec.active_window {
            return attach_active(spec);
        }
        let foreground = unsafe { GetForegroundWindow() };
        let job = launch::Job::spawn(spec)?;
        let name = std::path::Path::new(&spec.app)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut session = WindowsSession {
            owner: Owner::Job(job),
            name,
            uia: uia::Uia::new()?,
            closed: false,
            foreground: spec.foreground,
            pinned: None,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut sent_back = Vec::new();
        while Instant::now() < deadline && session.is_alive() {
            let windows = session.windows()?;
            if session.foreground {
                if let Some(w) = windows.first() {
                    sleep(Duration::from_millis(300));
                    bring_forward(capture::hwnd(w.id));
                    break;
                }
                sleep(Duration::from_millis(150));
                continue;
            }
            // New windows go behind everything else, and focus goes back.
            for w in &windows {
                if !sent_back.contains(&w.id) {
                    sent_back.push(w.id);
                    let h = capture::hwnd(w.id);
                    let _ = unsafe {
                        SetWindowPos(
                            h,
                            Some(HWND_BOTTOM),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        )
                    };
                }
            }
            let now = unsafe { GetForegroundWindow() };
            let mut owner = 0u32;
            unsafe { GetWindowThreadProcessId(now, Some(&mut owner)) };
            if session.owner.pids().contains(&owner) && !foreground.is_invalid() {
                let _ = unsafe { SetForegroundWindow(foreground) };
            }
            if !windows.is_empty() {
                sleep(Duration::from_millis(300));
                break;
            }
            sleep(Duration::from_millis(150));
        }
        Ok(Box::new(session))
    }
}

/// A session on the window in front, skipping the client's own apps. The
/// app is not in a job: it was running before and keeps running after.
fn attach_active(spec: &LaunchSpec) -> Result<Box<dyn Session>> {
    let mut skip = launch::ancestors();
    skip.extend(&spec.skip_pids);
    let hwnd = capture::front_window(&skip)
        .ok_or_else(|| anyhow!("no window is in front, other than the client's own"))?;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let process = launch::Process::open(pid)?;
    let session = WindowsSession {
        name: process.name(),
        owner: Owner::Attached(process),
        uia: uia::Uia::new()?,
        closed: false,
        foreground: spec.foreground,
        pinned: Some(hwnd.0 as usize as u64),
    };
    if session.foreground {
        bring_forward(hwnd);
    }
    Ok(Box::new(session))
}

/// Makes `hwnd` the foreground window. Windows only lets the foreground
/// thread hand the foreground on, so this joins that thread's input queue
/// for the moment it takes.
fn bring_forward(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let now = GetForegroundWindow();
        if now == hwnd {
            return;
        }
        let ours = GetCurrentThreadId();
        let theirs = GetWindowThreadProcessId(now, None);
        let attached =
            theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(ours, theirs, false);
        }
    }
    // Long enough for the activation to reach the app before input does.
    sleep(Duration::from_millis(100));
}

/// Where a session's app came from: started in a kill-on-close job, or an
/// app that was already running, whose window the session attached to.
enum Owner {
    Job(launch::Job),
    Attached(launch::Process),
}

impl Owner {
    fn pid(&self) -> u32 {
        match self {
            Owner::Job(job) => job.pid,
            Owner::Attached(p) => p.pid,
        }
    }

    fn pids(&self) -> Vec<u32> {
        match self {
            Owner::Job(job) => job.pids(),
            Owner::Attached(p) => vec![p.pid],
        }
    }

    fn alive(&self) -> bool {
        match self {
            Owner::Job(job) => job.alive(),
            Owner::Attached(p) => p.alive(),
        }
    }

    /// Ends a started app; an attached one is left running.
    fn kill(&mut self) {
        if let Owner::Job(job) = self {
            job.kill();
        }
    }

    /// Whether the panel hook was loaded into the app before it started.
    /// An attached app never can have one.
    fn hooked(&self) -> bool {
        matches!(self, Owner::Job(job) if job.hooked)
    }
}

pub struct WindowsSession {
    owner: Owner,
    name: String,
    uia: uia::Uia,
    closed: bool,
    /// Started with [`LaunchSpec::foreground`]: the window is brought to the
    /// front before every action.
    foreground: bool,
    /// The window the session was attached to, which comes first in its
    /// window list while it exists.
    pinned: Option<u64>,
}

impl WindowsSession {
    fn window(&mut self, window: Option<u64>) -> Result<WindowInfo> {
        let windows = self.windows()?;
        pick_window(&windows, window).cloned()
    }

    /// The open or save dialog among the app's windows, if one is up.
    fn dialog(&mut self) -> Option<dialog::Dialog> {
        let windows = self.windows().ok()?;
        dialog::locate(&self.uia, &windows)
    }

    /// Answers whatever is asking for files: a dialog held back by the
    /// panel hook, or a dialog on screen answered through UI Automation.
    fn choose_file(&mut self, paths: &[String]) -> Result<()> {
        let paths: Vec<std::path::PathBuf> = paths
            .iter()
            .map(|p| ocu_core::paths::absolute(p))
            .collect::<Result<_>>()?;
        // What asked may still be on its way: a menu item's dialog comes a
        // moment after the click.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // The hook first: a hooked app's dialog never shows, so it is
            // the better answer where there is one.
            if hook::waiting(self.owner.pid()).is_some() {
                return hook::answer(self.owner.pid(), &paths);
            }
            if let Some(d) = self.dialog() {
                return d.answer(&self.uia, &paths);
            }
            if Instant::now() > deadline {
                bail!(
                    "the app is not asking for a file: do what opens a dialog first (an upload \
                     button, File → Open), or use clicks and keys"
                );
            }
            sleep(Duration::from_millis(100));
        }
    }

    fn point(w: &WindowInfo, x: f64, y: f64) -> POINT {
        POINT {
            x: (w.frame.x + x).round() as i32,
            y: (w.frame.y + y).round() as i32,
        }
    }
}

impl Session for WindowsSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        match self.owner {
            Owner::Job(_) => {
                details.insert(
                    "job".into(),
                    "the app and its children run in a kill-on-close job".into(),
                );
                // Which route `choose_file` will take, which is worth saying
                // out loud when the hook was asked for and did not arrive.
                details.insert(
                    "dialogs".into(),
                    if self.owner.hooked() {
                        "held by the panel hook; nothing shows on screen".into()
                    } else {
                        "answered on screen through UI Automation".into()
                    },
                );
            }
            Owner::Attached(_) => {
                details.insert(
                    "active_window".into(),
                    "attached to the window that was in front; it comes first".into(),
                );
                details.insert(
                    "attached".into(),
                    "already running; it is left open when the session ends".into(),
                );
            }
        }
        if self.foreground {
            details.insert(
                "foreground".into(),
                "brought to the front before every action".into(),
            );
        }
        Description {
            app: self.name.clone(),
            pid: Some(self.owner.pid()),
            details,
            ..Default::default()
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        let mut windows = capture::windows(&self.owner.pids());
        if let Some(i) = windows.iter().position(|w| Some(w.id) == self.pinned) {
            let w = windows.remove(i);
            windows.insert(0, w);
        }
        Ok(windows)
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let w = self.window(window)?;
        capture::capture(&w)
    }

    fn ui_tree(&mut self, window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        let w = self.window(window)?;
        self.uia.tree(w.id, (w.frame.x, w.frame.y), opts)
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        match action {
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                return Ok(());
            }
            Action::ChooseFile { paths } => return self.choose_file(paths),
            _ => {}
        }
        let w = self.window(window)?;
        let top = capture::hwnd(w.id);
        if self.foreground {
            bring_forward(top);
        }
        let origin = (w.frame.x, w.frame.y);
        match action {
            Action::ElementAction { element, name } => self
                .uia
                .perform(element, name.as_deref().unwrap_or("press"))?,
            Action::SetValue { element, value } => self.uia.set_value(element, value)?,
            Action::Focus { element } => {
                // UIA's SetFocus would bring the window forward; a posted
                // click focuses the control and leaves the window be.
                let (x, y) = self.uia.center(element, origin)?;
                input::click(
                    top,
                    Self::point(&w, x, y),
                    ocu_core::MouseButton::Left,
                    1,
                    Default::default(),
                );
            }
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
                input::click(top, Self::point(&w, *x, *y), *button, *count, m);
            }
            Action::MoveMouse { x, y } => input::move_to(top, Self::point(&w, *x, *y)),
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
            } => input::drag(
                top,
                Self::point(&w, *from_x, *from_y),
                Self::point(&w, *to_x, *to_y),
                *button,
            ),
            Action::Scroll { x, y, dx, dy } => {
                input::scroll(top, Self::point(&w, *x, *y), *dx, *dy)
            }
            Action::TypeText { text } => input::type_text(top, text),
            Action::PressKey { keys } => {
                for chord in parse_chords(keys)? {
                    input::press(top, &chord);
                    sleep(Duration::from_millis(30));
                }
            }
            Action::Wait { .. } | Action::ChooseFile { .. } => unreachable!(),
        }
        Ok(())
    }

    fn notice(&mut self) -> Option<String> {
        if let Some(r) = hook::waiting(self.owner.pid()) {
            return Some(format!(
                "The app is asking for {} (its {} dialog is held back, so nothing shows): answer \
                 it with choose_file, giving the {}, or no paths to cancel.",
                match (r.save, r.folders, r.multiple) {
                    (true, ..) => "a place to save",
                    (false, true, true) => "folders",
                    (false, true, false) => "a folder",
                    (false, false, true) => "files",
                    (false, false, false) => "a file",
                },
                if r.save { "save" } else { "open" },
                if r.save { "path to save to" } else { "paths" },
            ));
        }
        self.dialog().map(|d| d.notice())
    }

    fn is_alive(&mut self) -> bool {
        self.owner.alive()
    }

    fn close(&mut self) {
        if !std::mem::replace(&mut self.closed, true) {
            // A dialog the hook is holding is the app's own thread, blocked
            // on us. Dropping it here is how the app sees a cancel.
            hook::forget(self.owner.pid());
            self.owner.kill();
        }
    }
}
