//! The app and its window.
//!
//! On macOS it owns the sessions, because the Accessibility and Screen
//! Recording permissions belong to it, and serves the MCP servers that
//! clients start over a Unix socket. It has no Dock icon unless its window
//! is open; while sessions work it draws their overlays.
//!
//! On Windows and Linux, where each MCP server drives apps itself, it is the
//! settings window, hosting the HTTP server while open. Closing it quits.

mod assets;
mod http;
mod menu;
#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
mod overlay;
mod status;
pub mod ui;

#[cfg(target_os = "macos")]
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use anyhow::Result;
#[cfg(target_os = "macos")]
use gpui::{
    point, px, size, Bounds, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
};
use gpui::{App, AppContext as _, Application, Context, Entity, WindowHandle};

#[cfg(target_os = "macos")]
use ocu_core::WindowInfo;
use ocu_core::{Observer, Service, SessionInfo};

#[cfg(target_os = "macos")]
use crate::config::Config;
#[cfg(target_os = "macos")]
use overlay::{Overlay, MARGIN};

/// Where the agent listens. `OCU_SOCKET` moves it, so a development build
/// can run beside an installed app.
#[cfg(target_os = "macos")]
pub fn socket_path() -> std::path::PathBuf {
    std::env::var_os("OCU_SOCKET")
        .map(Into::into)
        .unwrap_or_else(|| crate::config::config_dir().join("agent.sock"))
}

enum UiEvent {
    Started,
    Ended(String),
    #[cfg(target_os = "macos")]
    Pointer {
        session: String,
        window: WindowInfo,
        x: f64,
        y: f64,
        click: bool,
    },
    #[cfg(target_os = "macos")]
    Acted {
        session: String,
        window: WindowInfo,
    },
    /// The background check found a release it hasn't shown yet.
    UpdateAvailable(crate::update::UpdateStatus),
}

/// Carries session events from request threads to the UI thread.
struct Bus(async_channel::Sender<UiEvent>);

impl Observer for Bus {
    fn session_started(&self, info: &SessionInfo) {
        let _ = (info, self.0.try_send(UiEvent::Started));
    }
    fn session_ended(&self, id: &str) {
        let _ = self.0.try_send(UiEvent::Ended(id.to_string()));
    }
    #[cfg(target_os = "macos")]
    fn pointer(&self, session: &str, window: &WindowInfo, x: f64, y: f64, click: bool) {
        let _ = self.0.try_send(UiEvent::Pointer {
            session: session.into(),
            window: window.clone(),
            x,
            y,
            click,
        });
        // Let the cursor arrive before the click lands, so what the user
        // sees matches what happens.
        std::thread::sleep(Duration::from_millis(if click { 260 } else { 120 }));
    }
    #[cfg(target_os = "macos")]
    fn acted(&self, session: &str, window: &WindowInfo) {
        let _ = self.0.try_send(UiEvent::Acted {
            session: session.into(),
            window: window.clone(),
        });
    }
}

#[cfg(target_os = "macos")]
struct OverlayEntry {
    handle: WindowHandle<Overlay>,
    target: u64,
    last_active: Instant,
    shown: bool,
}

/// How long an idle session keeps its cursor on screen.
#[cfg(target_os = "macos")]
const OVERLAY_IDLE: Duration = Duration::from_secs(30);

struct Agent {
    service: Arc<Service>,
    #[cfg(target_os = "macos")]
    overlays: HashMap<String, OverlayEntry>,
    status: Option<WindowHandle<status::Status>>,
    http: http::HttpHost,
}

impl Agent {
    fn show_status(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.status {
            if h.update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                cx.activate(true);
                return;
            }
        }
        self.status = status::open(self.service.clone(), cx);
    }

    fn on_event(&mut self, ev: UiEvent, cx: &mut Context<Self>) {
        match ev {
            UiEvent::Started => self.refresh_status(cx),
            UiEvent::Ended(id) => {
                #[cfg(target_os = "macos")]
                if let Some(entry) = self.overlays.remove(&id) {
                    let _ = entry
                        .handle
                        .update(cx, |_, window, _| window.remove_window());
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id; // No overlay to put away off macOS.
                self.refresh_status(cx);
            }
            #[cfg(target_os = "macos")]
            UiEvent::Pointer {
                session,
                window,
                x,
                y,
                click,
            } => {
                if let Some(h) = self.overlay(&session, &window, cx) {
                    let _ = h.update(cx, |o, _, cx| {
                        o.point_at(x, y, click);
                        cx.notify();
                    });
                }
            }
            UiEvent::UpdateAvailable(status) => {
                self.show_status(cx);
                if let Some(h) = self.status {
                    let _ = h.update(cx, |s, _, cx| s.show_update(status, cx));
                }
            }
            #[cfg(target_os = "macos")]
            UiEvent::Acted { session, window } => {
                if let Some(h) = self.overlay(&session, &window, cx) {
                    let _ = h.update(cx, |o, _, cx| {
                        o.touch();
                        cx.notify();
                    });
                }
            }
        }
    }

    /// The menu's Check for Updates: the window, checking, then an alert.
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.show_status(cx);
        if let Some(h) = self.status {
            let _ = h.update(cx, |s, window, cx| s.check_for_updates(true, window, cx));
        }
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.status {
            let _ = h.update(cx, |s, _, cx| s.refresh(cx));
        }
    }

    /// The session's overlay over `window`, made or moved as needed.
    #[cfg(target_os = "macos")]
    fn overlay(
        &mut self,
        session: &str,
        window: &WindowInfo,
        cx: &mut Context<Self>,
    ) -> Option<WindowHandle<Overlay>> {
        if !Config::load().show_overlay() {
            return None;
        }
        if let Some(entry) = self.overlays.get_mut(session) {
            entry.last_active = Instant::now();
            if entry.target != window.id {
                entry.target = window.id;
            }
            let (handle, target) = (entry.handle, entry.target);
            entry.shown = true;
            let frame = window.frame;
            let _ = handle.update(cx, |_, w, _| {
                if let Some(ns) = native::ns_window(w) {
                    native::cover(&ns, frame, target, MARGIN);
                }
            });
            return Some(handle);
        }
        let f = window.frame;
        let bounds = Bounds {
            origin: point(px((f.x - MARGIN) as f32), px((f.y - MARGIN) as f32)),
            size: size(
                px((f.width + MARGIN * 2.0) as f32),
                px((f.height + MARGIN * 2.0) as f32),
            ),
        };
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    focus: false,
                    show: true,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    window_background: WindowBackgroundAppearance::Transparent,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Overlay::new()),
            )
            .map_err(|e| log::warn!("opening an overlay: {e:#}"))
            .ok()?;
        let target = window.id;
        let _ = handle.update(cx, |_, w, _| {
            if let Some(ns) = native::ns_window(w) {
                native::make_overlay(&ns);
                native::cover(&ns, f, target, MARGIN);
            }
        });
        self.overlays.insert(
            session.to_string(),
            OverlayEntry {
                handle,
                target,
                last_active: Instant::now(),
                shown: true,
            },
        );
        Some(handle)
    }

    /// Keeps overlays glued to their windows as they move, resize, hide and
    /// get covered, and puts idle ones away.
    #[cfg(target_os = "macos")]
    fn track(&mut self, cx: &mut Context<Self>) {
        for entry in self.overlays.values_mut() {
            let info = ocu_macos::window_info(entry.target);
            let visible = entry.last_active.elapsed() < OVERLAY_IDLE
                && info.as_ref().is_some_and(|w| w.on_screen);
            let target = entry.target;
            if !visible && !entry.shown {
                continue;
            }
            entry.shown = visible;
            let _ = entry.handle.update(cx, |_, w, _| {
                let Some(ns) = native::ns_window(w) else {
                    return;
                };
                match (&info, visible) {
                    (Some(info), true) => native::cover(&ns, info.frame, target, MARGIN),
                    _ => native::hide(&ns),
                }
            });
        }
    }
}

/// Checks for a release once a day while the agent runs, when the setting
/// allows. A release is announced (the window opens on it) once; after
/// that it waits in the window's Updates section.
fn background_update_checks(tx: async_channel::Sender<UiEvent>) {
    use crate::update;
    let announced = crate::config::config_dir().join("update-announced");
    // Not in the way of whatever launched the agent.
    std::thread::sleep(Duration::from_secs(30));
    loop {
        if update::check_automatically() && update::check_due() {
            let status = update::check();
            update::mark_checked();
            match &status {
                update::UpdateStatus::Available(up) => {
                    let seen = std::fs::read_to_string(&announced).unwrap_or_default();
                    if seen.trim() != up.version {
                        let _ = std::fs::write(&announced, &up.version);
                        if tx
                            .send_blocking(UiEvent::UpdateAvailable(status.clone()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                update::UpdateStatus::Failed(e) => log::info!("update check: {e}"),
                update::UpdateStatus::UpToDate => {}
            }
        }
        std::thread::sleep(Duration::from_secs(60 * 60));
    }
}

fn log_to_file() {
    let path = crate::config::config_dir().join("agent.log");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let target = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Ok(file) = target {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    let _ = builder.try_init();
}

/// Runs the agent. `show` opens the window (a launch from Finder); on macOS
/// a launch by an MCP server stays out of sight unless permissions are
/// missing. Elsewhere the window always opens, since it is the whole app.
pub fn run(show: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        anyhow::bail!(
            "no display to open the window on; run `opencomputeruse serve` or `opencomputeruse mcp`"
        );
    }
    #[cfg(windows)]
    detach_console();
    log_to_file();
    let (tx, rx) = async_channel::unbounded();
    let updates_tx = tx.clone();
    let service = Service::with_devices(
        crate::platform(),
        Some(crate::mobile_platform()),
        Some(Arc::new(Bus(tx))),
    );
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = crate::ipc::listen(service.clone(), &socket_path()) {
            // Another copy already serves; it will show itself if reopened.
            log::info!("not starting a second agent: {e:#}");
            return Ok(());
        }
        log::info!("agent listening on {}", socket_path().display());
    }
    {
        let service = service.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            service.reap();
        });
    }

    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || background_update_checks(updates_tx))?;

    let app = Application::new().with_assets(assets::Assets);
    let agent_slot: std::rc::Rc<std::cell::RefCell<Option<Entity<Agent>>>> = Default::default();
    {
        let slot = agent_slot.clone();
        app.on_reopen(move |cx| {
            if let Some(agent) = slot.borrow().clone() {
                agent.update(cx, |a, cx| a.show_status(cx));
            }
        });
    }
    app.run(move |cx: &mut App| {
        menu::install(cx);
        #[cfg(target_os = "macos")]
        native::set_regular(false);
        ui::set_light(matches!(
            cx.window_appearance(),
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
        ));
        let missing = service
            .platform()
            .permissions()
            .iter()
            .any(|p| !p.granted && !p.optional);
        let agent = cx.new(|_| Agent {
            service: service.clone(),
            #[cfg(target_os = "macos")]
            overlays: HashMap::new(),
            status: None,
            http: Default::default(),
        });
        *agent_slot.borrow_mut() = Some(agent.clone());
        if show || missing || cfg!(not(target_os = "macos")) {
            agent.update(cx, |a, cx| a.show_status(cx));
        }
        {
            let agent = agent.clone();
            // With the window focused, the menu dispatches through it, so it
            // can't be reached until that's over: without waiting, it'd look
            // closed and a second one would open.
            cx.on_action(move |_: &menu::CheckForUpdates, cx| {
                let agent = agent.clone();
                cx.defer(move |cx| agent.update(cx, |a, cx| a.check_for_updates(cx)));
            });
        }
        cx.on_window_closed({
            let agent = agent.clone();
            // Closing an overlay happens inside an update of the agent, so
            // look at it once that update is over.
            move |cx| {
                let agent = agent.clone();
                cx.defer(move |cx| {
                    let open = agent
                        .read(cx)
                        .status
                        .is_some_and(|h| h.update(cx, |_, _, _| ()).is_ok());
                    if !open {
                        agent.update(cx, |a, _| a.status = None);
                        // Without a Dock or menu bar there is no way back
                        // to a hidden app, so closing the window quits.
                        #[cfg(target_os = "macos")]
                        native::set_regular(false);
                        #[cfg(not(target_os = "macos"))]
                        cx.quit();
                    }
                });
            }
        })
        .detach();
        let events = agent.clone();
        cx.spawn(async move |cx| {
            while let Ok(ev) = rx.recv().await {
                if events.update(cx, |a, cx| a.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // The HTTP server follows the settings, wherever they are changed.
        let http = agent.clone();
        cx.spawn(async move |cx| loop {
            if http.update(cx, |a, _| a.http.sync(&a.service)).is_err() {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
        })
        .detach();
        #[cfg(target_os = "macos")]
        let tracker = agent.clone();
        #[cfg(target_os = "macos")]
        cx.spawn(async move |cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            if tracker.update(cx, |a, cx| a.track(cx)).is_err() {
                break;
            }
        })
        .detach();
        cx.on_app_quit(move |_| {
            let service = service.clone();
            async move { service.end_all() }
        })
        .detach();
    });
    Ok(())
}

/// Lets go of the console Windows gives a console program started from
/// Explorer, so a double-click shows the window alone. A console the user
/// started us from is shared with their shell, and kept.
#[cfg(windows)]
fn detach_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 2];
    if unsafe { GetConsoleProcessList(&mut pids) } == 1 {
        let _ = unsafe { FreeConsole() };
    }
}
