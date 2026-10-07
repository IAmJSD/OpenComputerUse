//! The Windows backend. Apps start inside a kill-on-close Job Object
//! (so they die with the MCP server), shown without activating and sent to
//! the back; they are captured with `PrintWindow`, read and pressed through
//! UI Automation, and sent input as window messages.
#![cfg(windows)]

mod capture;
mod input;
mod launch;
mod uia;

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow, SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE,
};

use ocu_core::keys::{parse_chord, parse_chords};
use ocu_core::{pick_window, Action, Description, LaunchSpec, Platform, Screenshot, Session, TreeOptions, UiNode, WindowInfo};

pub struct WindowsPlatform;

impl WindowsPlatform {
    pub fn new() -> Self {
        // Physical pixels everywhere, so screenshots and coordinates agree
        // on high-DPI screens.
        let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        uia::com_init();
        Self
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

    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn Session>> {
        let foreground = unsafe { GetForegroundWindow() };
        let job = launch::Job::spawn(spec)?;
        let name = std::path::Path::new(&spec.app)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut session = WindowsSession { job, name, uia: uia::Uia::new()?, closed: false };
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut sent_back = Vec::new();
        while Instant::now() < deadline && session.is_alive() {
            let windows = session.windows()?;
            // New windows go behind everything else, and focus goes back.
            for w in &windows {
                if !sent_back.contains(&w.id) {
                    sent_back.push(w.id);
                    let h = capture::hwnd(w.id);
                    let _ = unsafe { SetWindowPos(h, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
                }
            }
            let now = unsafe { GetForegroundWindow() };
            let mut owner = 0u32;
            unsafe { GetWindowThreadProcessId(now, Some(&mut owner)) };
            if session.job.pids().contains(&owner) && !foreground.is_invalid() {
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

pub struct WindowsSession {
    job: launch::Job,
    name: String,
    uia: uia::Uia,
    closed: bool,
}

impl WindowsSession {
    fn window(&mut self, window: Option<u64>) -> Result<WindowInfo> {
        let windows = self.windows()?;
        pick_window(&windows, window).cloned()
    }

    fn point(w: &WindowInfo, x: f64, y: f64) -> POINT {
        POINT { x: (w.frame.x + x).round() as i32, y: (w.frame.y + y).round() as i32 }
    }
}

impl Session for WindowsSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        details.insert("job".into(), "the app and its children run in a kill-on-close job".into());
        Description { app: self.name.clone(), pid: Some(self.job.pid), details }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(capture::windows(&self.job.pids()))
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
        if let Action::Wait { ms } = action {
            sleep(Duration::from_millis(*ms));
            return Ok(());
        }
        let w = self.window(window)?;
        let top = capture::hwnd(w.id);
        let origin = (w.frame.x, w.frame.y);
        match action {
            Action::ElementAction { element, name } => self.uia.perform(element, name.as_deref().unwrap_or("press"))?,
            Action::SetValue { element, value } => self.uia.set_value(element, value)?,
            Action::Focus { element } => {
                // UIA's SetFocus would bring the window forward; a posted
                // click focuses the control and leaves the window be.
                let (x, y) = self.uia.center(element, origin)?;
                input::click(top, Self::point(&w, x, y), ocu_core::MouseButton::Left, 1, Default::default());
            }
            Action::Click { x, y, button, count, modifiers } => {
                let m = match modifiers.as_deref().filter(|s| !s.is_empty()) {
                    Some(s) => parse_chord(s)?.modifiers,
                    None => Default::default(),
                };
                input::click(top, Self::point(&w, *x, *y), *button, *count, m);
            }
            Action::MoveMouse { x, y } => input::move_to(top, Self::point(&w, *x, *y)),
            Action::Drag { from_x, from_y, to_x, to_y, button } => {
                input::drag(top, Self::point(&w, *from_x, *from_y), Self::point(&w, *to_x, *to_y), *button)
            }
            Action::Scroll { x, y, dx, dy } => input::scroll(top, Self::point(&w, *x, *y), *dx, *dy),
            Action::TypeText { text } => input::type_text(top, text),
            Action::PressKey { keys } => {
                for chord in parse_chords(keys)? {
                    input::press(top, &chord);
                    sleep(Duration::from_millis(30));
                }
            }
            Action::Wait { .. } => unreachable!(),
        }
        Ok(())
    }

    fn is_alive(&mut self) -> bool {
        self.job.alive()
    }

    fn close(&mut self) {
        if !std::mem::replace(&mut self.closed, true) {
            self.job.kill();
        }
    }
}
