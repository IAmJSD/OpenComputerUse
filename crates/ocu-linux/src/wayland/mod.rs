//! Sessions on a headless Wayland compositor of their own, which runs on a
//! thread of this process: no X server, no GPU and nothing to install
//! besides the app. The app finds the compositor through `WAYLAND_DISPLAY`,
//! an absolute path in a private directory.
//!
//! Everything that touches the compositor runs on its thread; the session
//! and its viewer send it jobs over a channel and wait for the answer, so
//! a person watching never waits on the agent's lock.

mod compositor;

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{sleep, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use smithay::reexports::calloop::channel::{self, Event};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::ListeningSocket;

use ocu_core::image::encode_png;
use ocu_core::keys::{
    dom_code, modifier_evdev, named_evdev, parse_chord, parse_chords, us_key, Key, Modifiers,
};
use ocu_core::{
    pick_window, Action, Description, Frame, LaunchSpec, MouseButton, Screenshot, Session, Size,
    TreeOptions, UiNode, ViewInput, Viewer, WindowInfo,
};

use crate::atspi::A11y;
use crate::launch::{self, EnvChange};
use compositor::{ClientState, Compositor};

/// How often apps may draw: frame callbacks go out at this pace.
const FRAME: Duration = Duration::from_millis(33);

/// What the session and its viewer share with the compositor thread.
pub(crate) struct Shared {
    /// Bumped on every commit: the picture may have changed.
    generation: AtomicU64,
    /// The compositor is running.
    alive: AtomicBool,
}

type Job = Box<dyn FnOnce(&mut Compositor) + Send>;

/// The way to the compositor thread.
#[derive(Clone)]
struct Link {
    tx: channel::Sender<Job>,
    shared: Arc<Shared>,
}

impl Link {
    /// Runs `f` on the compositor thread and returns what it gave.
    fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Compositor) -> R + Send + 'static,
    ) -> Result<R> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| anyhow!("the session's compositor has stopped"))?;
        rx.recv_timeout(Duration::from_secs(15))
            .map_err(|_| anyhow!("the session's compositor did not answer"))
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        self.call(|c| c.state.windows().iter().map(|w| c.state.info(w)).collect())
    }

    /// Moves the pointer to `x`, `y` in `window`.
    fn pointer(&self, window: Option<u64>, x: f64, y: f64) -> Result<()> {
        self.call(move |c| {
            let (_, at) = c.point(window, x, y)?;
            c.state.pointer_to(at);
            Ok(())
        })?
    }

    fn button(&self, b: MouseButton, down: bool) -> Result<()> {
        self.call(move |c| c.state.button(b, down))
    }

    fn keys(&self, codes: Vec<u16>, down: bool) -> Result<()> {
        self.call(move |c| {
            for code in codes {
                c.state.key(code, down);
            }
        })
    }

    /// Gives `window` the keyboard and runs `f` with it.
    fn with_focus<R: Send + 'static>(
        &self,
        window: Option<u64>,
        f: impl FnOnce(&mut Compositor) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        self.call(move |c| {
            let w = c.state.window(window)?;
            c.state.focus(&w);
            f(c)
        })?
    }

    fn type_text(&self, window: Option<u64>, text: String) -> Result<()> {
        self.with_focus(window, move |c| {
            let base = c.base_keymap.clone();
            c.state.type_text(&text, &base)
        })
    }
}

/// Starts the compositor's thread, listening at `socket`.
fn start_compositor(socket: PathBuf, size: Size) -> Result<(Link, JoinHandle<()>)> {
    let shared = Arc::new(Shared {
        generation: AtomicU64::new(0),
        alive: AtomicBool::new(true),
    });
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread_shared = shared.clone();
    let thread = std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || {
            let shared = thread_shared;
            if let Err(e) = run(socket, size, shared.clone(), &ready_tx) {
                log::warn!("the Wayland compositor stopped: {e:#}");
                let _ = ready_tx.send(Err(e));
            }
            shared.alive.store(false, Ordering::Relaxed);
        })?;
    let tx = ready_rx
        .recv()
        .map_err(|_| anyhow!("the Wayland compositor did not start"))??;
    Ok((Link { tx, shared }, thread))
}

fn run(
    socket: PathBuf,
    size: Size,
    shared: Arc<Shared>,
    ready: &mpsc::SyncSender<Result<channel::Sender<Job>>>,
) -> Result<()> {
    let mut event_loop: EventLoop<Compositor> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    let mut data = Compositor::new((size.width as i32, size.height as i32).into(), shared)?;

    let listener = ListeningSocket::bind_absolute(socket.clone())
        .with_context(|| format!("listening at {}", socket.display()))?;
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, c: &mut Compositor| {
                while let Some(stream) = listener.accept()? {
                    if let Err(e) = c
                        .display
                        .handle()
                        .insert_client(stream, Arc::new(ClientState::default()))
                    {
                        log::warn!("refusing a Wayland client: {e}");
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("watching the socket: {e}"))?;

    let poll_fd = data.display.backend().poll_fd().try_clone_to_owned()?;
    handle
        .insert_source(
            Generic::new(poll_fd, Interest::READ, Mode::Level),
            |_, _, c: &mut Compositor| {
                c.display
                    .dispatch_clients(&mut c.state)
                    .map_err(std::io::Error::other)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("watching the clients: {e}"))?;

    let (tx, rx) = channel::channel::<Job>();
    handle
        .insert_source(rx, |event, _, c: &mut Compositor| match event {
            Event::Msg(job) => job(c),
            // Every sender is gone: the session and its viewers.
            Event::Closed => c.stop = true,
        })
        .map_err(|e| anyhow!("watching for jobs: {e}"))?;

    handle
        .insert_source(Timer::from_duration(FRAME), |_, _, c: &mut Compositor| {
            c.state.send_frames();
            TimeoutAction::ToDuration(FRAME)
        })
        .map_err(|e| anyhow!("starting the frame clock: {e}"))?;

    let _ = ready.send(Ok(tx));
    while !data.stop {
        event_loop.dispatch(Some(FRAME), &mut data)?;
        data.state.settle_parents();
        data.state.space.refresh();
        data.state.popups.cleanup();
        data.display.flush_clients()?;
    }
    Ok(())
}

pub(crate) struct WaylandSession {
    app: Child,
    name: String,
    link: Link,
    thread: Option<JoinHandle<()>>,
    portal: crate::Portal,
    a11y: Option<A11y>,
    socket: PathBuf,
    /// Holds the socket; removed when the session ends.
    dir: PathBuf,
    viewer: Arc<WaylandView>,
    closed: bool,
}

impl WaylandSession {
    pub(crate) fn start(spec: &LaunchSpec, portal: bool) -> Result<Self> {
        let size = spec.display_size.unwrap_or(Size {
            width: 1440,
            height: 900,
        });
        let dir = crate::private_dir("wl")?;
        let socket = dir.join("wayland-0");
        let (link, thread) = match start_compositor(socket.clone(), size) {
            Ok(started) => started,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let portal = if portal { crate::start_portal() } else { None };
        let a11y = crate::start_a11y();

        let mut args = Vec::new();
        if launch::is_chromium(&spec.app) {
            args = launch::chromium_args(&spec.args, unsafe { libc::geteuid() } == 0);
        }
        args.extend(spec.args.iter().cloned());
        let mut cmd = Command::new(&spec.app);
        crate::use_a11y(&mut cmd, &a11y, &spec.app, &mut args);
        cmd.args(&args);
        let has_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| !d.is_empty());
        for change in launch::wayland_env(&socket, &dir, has_runtime_dir) {
            match change {
                EnvChange::Set(k, v) => cmd.env(k, v),
                EnvChange::Remove(k) => cmd.env_remove(k),
            };
        }
        crate::use_portal(&mut cmd, &portal);
        cmd.envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        crate::die_with_parent(&mut cmd);
        let app = match cmd.spawn() {
            Ok(app) => app,
            Err(e) => {
                drop(link);
                let _ = thread.join();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e).with_context(|| format!("starting {}", spec.app));
            }
        };
        let mut session = Self {
            app,
            name: crate::app_name(&spec.app),
            viewer: Arc::new(WaylandView { link: link.clone() }),
            link,
            thread: Some(thread),
            portal,
            a11y,
            socket,
            dir,
            closed: false,
        };

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && session.is_alive() {
            if session.windows().map(|w| !w.is_empty()).unwrap_or(false) {
                break;
            }
            sleep(Duration::from_millis(150));
        }
        sleep(Duration::from_millis(300));
        Ok(session)
    }

    fn modifiers(modifiers: &Option<String>) -> Result<Vec<u16>> {
        Ok(match modifiers.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => modifier_evdev(parse_chord(s)?.modifiers),
            None => Vec::new(),
        })
    }

    fn press_keys(&self, window: Option<u64>, keys: &str) -> Result<()> {
        for chord in parse_chords(keys)? {
            let mut mods = chord.modifiers;
            let key = match chord.key {
                None => None,
                Some(Key::Named(k)) => Some(named_evdev(k)),
                Some(Key::Char(c)) => match us_key(c) {
                    Some((code, shift)) => {
                        mods.shift |= shift;
                        Some(code)
                    }
                    // A character the keyboard lacks, on its own, is typed.
                    None if mods == Modifiers::default() => {
                        self.link.type_text(window, c.to_string())?;
                        continue;
                    }
                    None => bail!("a US keyboard has no key for \"{c}\" to press with modifiers"),
                },
            };
            let mods = modifier_evdev(mods);
            self.link.with_focus(window, move |c| {
                for &m in &mods {
                    c.state.key(m, true);
                }
                if let Some(k) = key {
                    c.state.tap(k);
                }
                for &m in mods.iter().rev() {
                    c.state.key(m, false);
                }
                Ok(())
            })?;
            sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

impl Session for WaylandSession {
    fn describe(&self) -> Description {
        let mut details = BTreeMap::new();
        details.insert("display".into(), "wayland".into());
        details.insert("wayland_display".into(), self.socket.display().to_string());
        details.insert("files".into(), crate::portal_detail(&self.portal));
        details.insert("accessibility".into(), crate::a11y_detail(&self.a11y));
        Description {
            backend: Some("linux-wayland".into()),
            app: self.name.clone(),
            pid: Some(self.app.id()),
            details,
        }
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        self.link.windows()
    }

    fn screenshot(&mut self, window: Option<u64>) -> Result<Screenshot> {
        let f = self.link.call(move |c| c.frame(window))??;
        Ok(Screenshot {
            window_id: Some(f.window_id),
            width: f.width,
            height: f.height,
            png: encode_png(f.width, f.height, f.stride, &f.data, f.order)?,
        })
    }

    fn ui_tree(&mut self, window: Option<u64>, opts: &TreeOptions) -> Result<UiNode> {
        let w = pick_window(&self.link.windows()?, window)?.clone();
        crate::a11y(&mut self.a11y)?.tree(&w, opts)
    }

    fn element_point(
        &mut self,
        _window: Option<u64>,
        element: &str,
    ) -> Result<(Option<u64>, f64, f64)> {
        let (w, x, y) = crate::a11y(&mut self.a11y)?.center(element)?;
        Ok((Some(w), x, y))
    }

    fn perform(&mut self, window: Option<u64>, action: &Action) -> Result<()> {
        let link = &self.link;
        match action {
            Action::Wait { ms } => sleep(Duration::from_millis(*ms)),
            Action::ElementAction { .. } | Action::SetValue { .. } | Action::Focus { .. } => {
                if let Some(then) = crate::a11y(&mut self.a11y)?.perform(action)? {
                    for a in &then.actions {
                        self.perform(Some(then.window), a)?;
                        sleep(Duration::from_millis(40));
                    }
                }
            }
            Action::ClickElement { .. } => bail!("click elements by their point"),
            Action::ChooseFile { paths } => return crate::choose_file(&self.portal, paths),
            Action::Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => {
                let mods = Self::modifiers(modifiers)?;
                link.pointer(window, *x, *y)?;
                link.keys(mods.clone(), true)?;
                for _ in 0..(*count).max(1) {
                    link.button(*button, true)?;
                    link.button(*button, false)?;
                    sleep(Duration::from_millis(40));
                }
                link.keys(mods.into_iter().rev().collect(), false)?;
            }
            Action::MoveMouse { x, y } => link.pointer(window, *x, *y)?,
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                modifiers,
            } => {
                let mods = Self::modifiers(modifiers)?;
                link.pointer(window, *from_x, *from_y)?;
                link.keys(mods.clone(), true)?;
                link.button(*button, true)?;
                for i in 1..=12 {
                    let t = i as f64 / 12.0;
                    sleep(Duration::from_millis(12));
                    link.pointer(
                        window,
                        from_x + (to_x - from_x) * t,
                        from_y + (to_y - from_y) * t,
                    )?;
                }
                link.button(*button, false)?;
                link.keys(mods.into_iter().rev().collect(), false)?;
            }
            Action::Scroll { x, y, dx, dy } => {
                link.pointer(window, *x, *y)?;
                let (dx, dy) = (*dx, *dy);
                link.call(move |c| c.state.scroll(dx, dy))?;
            }
            Action::TypeText { text } => link.type_text(window, text.clone())?,
            Action::PressKey { keys } => self.press_keys(window, keys)?,
        }
        Ok(())
    }

    fn notice(&mut self) -> Option<String> {
        crate::portal_notice(&self.portal)
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.app.try_wait(), Ok(None)) && self.link.shared.alive.load(Ordering::Relaxed)
    }

    fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        crate::end_app(&mut self.app);
        // A compositor that doesn't answer is left to die with the process
        // rather than waited for.
        if self.link.call(|c| c.stop = true).is_ok() {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    fn viewer(&mut self) -> Option<Arc<dyn Viewer>> {
        Some(self.viewer.clone())
    }
}

impl Drop for WaylandSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// A person's view into a Wayland session.
struct WaylandView {
    link: Link,
}

impl Viewer for WaylandView {
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        self.link.windows()
    }

    fn frame(&self, window: Option<u64>) -> Result<Frame> {
        self.link.call(move |c| c.frame(window))?
    }

    fn input(&self, window: Option<u64>, input: &ViewInput) -> Result<()> {
        match input.clone() {
            ViewInput::Move { x, y } => self.link.pointer(window, x, y),
            ViewInput::Button { x, y, button, down } => {
                self.link.pointer(window, x, y)?;
                self.link.button(button, down)
            }
            ViewInput::Wheel { x, y, dx, dy } => {
                self.link.pointer(window, x, y)?;
                self.link.call(move |c| c.state.scroll(dx, dy))
            }
            ViewInput::Key { code, down } => {
                let k = dom_code(&code).ok_or_else(|| anyhow!("unknown key code \"{code}\""))?;
                self.link.with_focus(window, move |c| {
                    c.state.key(k.evdev, down);
                    Ok(())
                })
            }
            ViewInput::Text { text } => self.link.type_text(window, text),
        }
    }

    fn resize(&self, size: Size) -> Result<()> {
        anyhow::ensure!(
            (64..=8192).contains(&size.width) && (64..=8192).contains(&size.height),
            "sizes run from 64 to 8192"
        );
        self.link.call(move |c| {
            c.state
                .set_output_size((size.width as i32, size.height as i32).into())
        })
    }

    fn generation(&self) -> Option<u64> {
        Some(self.link.shared.generation.load(Ordering::Relaxed))
    }

    fn is_alive(&self) -> bool {
        self.link.shared.alive.load(Ordering::Relaxed)
    }
}
