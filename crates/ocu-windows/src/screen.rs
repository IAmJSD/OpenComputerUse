//! A session on the whole desktop: each display is a window, pictures are
//! the screen as it shows, and input is real (`SendInput`), as from the
//! user's own mouse and keyboard.

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

use ocu_core::keys::{parse_chord, parse_chords, parse_held, Modifiers};
use ocu_core::{
    pick_window, Action, CursorPosition, Description, Rect, Screenshot, Session, WindowInfo,
};

use crate::{capture, real};

/// `MONITORINFOF_PRIMARY`.
const PRIMARY: u32 = 1;

/// The displays, the primary one first, as windows in screen pixels.
fn displays() -> Vec<WindowInfo> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<(WindowInfo, bool)>);
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if GetMonitorInfoW(m, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let name = String::from_utf16_lossy(&info.szDevice);
            let primary = info.monitorInfo.dwFlags & PRIMARY != 0;
            out.push((
                WindowInfo {
                    id: m.0 as usize as u64,
                    title: format!(
                        "{}{}",
                        name.trim_end_matches('\0').trim_start_matches(r"\\.\"),
                        if primary { " (primary)" } else { "" }
                    ),
                    frame: Rect {
                        x: r.left as f64,
                        y: r.top as f64,
                        width: (r.right - r.left) as f64,
                        height: (r.bottom - r.top) as f64,
                    },
                    on_screen: true,
                },
                primary,
            ));
        }
        BOOL(1)
    }
    let mut out: Vec<(WindowInfo, bool)> = Vec::new();
    let _ =
        unsafe { EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut out as *mut _ as isize)) };
    out.sort_by_key(|(_, primary)| !primary);
    out.into_iter().map(|(w, _)| w).collect()
}

pub struct ScreenSession;

impl ScreenSession {
    fn window(&self, window: Option<u64>) -> Result<WindowInfo> {
        pick_window(&displays(), window).cloned()
    }
}

impl Session for ScreenSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        details.insert(
            "screen".into(),
            "the whole desktop, one window per display, with real input: it moves your pointer \
             and types into what has focus"
                .into(),
        );
        Description {
            backend: Some("windows-screen".into()),
            app: "Screen".into(),
            pid: None,
            details,
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(displays())
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        capture::capture_screen(&self.window(window)?)
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        let w = self.window(window)?;
        let at = |x: f64, y: f64| POINT {
            x: (w.frame.x + x).round() as i32,
            y: (w.frame.y + y).round() as i32,
        };
        let mods = |m: &Option<String>| -> Result<Modifiers> {
            Ok(match m.as_deref().filter(|s| !s.is_empty()) {
                Some(s) => parse_chord(s)?.modifiers,
                None => Modifiers::default(),
            })
        };
        match action {
            Action::Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => real::click(at(*x, *y), *button, *count, mods(modifiers)?),
            Action::MoveMouse { x, y } => real::move_to(at(*x, *y)),
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                modifiers,
            } => real::drag(
                at(*from_x, *from_y),
                at(*to_x, *to_y),
                *button,
                mods(modifiers)?,
            ),
            Action::Scroll { x, y, dx, dy } => real::scroll(at(*x, *y), *dx, *dy),
            Action::TypeText { text } => real::type_text(text),
            Action::PressKey { keys } => {
                for chord in parse_chords(keys)? {
                    real::press(&chord)?;
                    sleep(Duration::from_millis(30));
                }
                Ok(())
            }
            Action::MouseDown { x, y, button } => real::button(at(*x, *y), *button, true),
            Action::MouseUp { x, y, button } => real::button(at(*x, *y), *button, false),
            Action::KeyDown { key } => real::hold(&parse_held(key)?, true),
            Action::KeyUp { key } => real::hold(&parse_held(key)?, false),
            Action::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                Ok(())
            }
            Action::ElementAction { .. }
            | Action::ClickElement { .. }
            | Action::SetValue { .. }
            | Action::Focus { .. } => bail!(
                "a screen session has no accessibility tree; use coordinates, or start a session \
                 on the app"
            ),
            Action::ChooseFile { .. } => {
                bail!("a screen session can't answer file dialogs; click through them")
            }
        }
    }

    fn cursor(&mut self, window: Option<u64>) -> Result<Option<CursorPosition>> {
        let w = self.window(window)?;
        let p = real::cursor()?;
        Ok(Some(CursorPosition {
            window_id: w.id,
            x: p.x as f64 - w.frame.x,
            y: p.y as f64 - w.frame.y,
            live: true,
        }))
    }

    fn is_alive(&mut self) -> bool {
        true
    }

    fn close(&mut self) {}
}
