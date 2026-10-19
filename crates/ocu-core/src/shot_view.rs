//! A live view for backends with no display of their own to watch (Windows,
//! macOS): the session's own windows, pictures streamed by the backend's
//! [`Capturer`] where it has one and its screenshots, polled, where not, and
//! a watcher's input turned into the session's actions.
//!
//! A streamed picture never takes the session's lock, and nor do the
//! window list and liveness while an action holds it, so a person watching
//! neither waits for the agent nor slows it down.

use std::any::Any;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::backend::{pick_window, Session, Viewer};
use crate::image::Order;
use crate::types::{Action, Frame, MouseButton, ViewInput, WindowInfo};

type Shared = Mutex<Box<dyn Session>>;

/// How far the pointer may move between a press and its release and still
/// be a click rather than a drag.
const CLICK_SLOP: f64 = 3.0;

/// How long a new stream gets to deliver its first picture before a
/// screenshot stands in for it.
const FIRST_FRAME: Duration = Duration::from_millis(500);

/// How long after a window's stream would not start before trying again.
const RETRY: Duration = Duration::from_secs(10);

/// How long a window's stream runs with no one looking at it.
const UNWATCHED: Duration = Duration::from_secs(10);

/// How often a polled view looks again.
const POLL: Duration = Duration::from_millis(100);

/// Streams windows' pictures as they change: ScreenCaptureKit's `SCStream`
/// on macOS, `Windows.Graphics.Capture` on Windows.
pub trait Capturer: Send + Sync {
    /// Starts capturing `window` at its size, delivering into the returned
    /// stream's [`Latest`] until it is dropped.
    fn stream(&self, window: &WindowInfo) -> Result<Stream>;
}

/// One window's running capture.
pub struct Stream {
    frames: Arc<Latest>,
    /// Whatever keeps the capture going; dropping it stops the capture.
    _running: Box<dyn Any + Send + Sync>,
}

impl Stream {
    pub fn new(frames: Arc<Latest>, running: impl Any + Send + Sync) -> Self {
        Self {
            frames,
            _running: Box::new(running),
        }
    }
}

/// The newest picture a capture has delivered, and how many it has.
#[derive(Default)]
pub struct Latest {
    frame: Mutex<Option<Frame>>,
    arrived: Condvar,
    count: AtomicU64,
    stopped: AtomicBool,
}

impl Latest {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// A new picture, from the capture's own thread. One the same as the
    /// last is dropped: captures may deliver a still window over and over,
    /// and the view sends whenever the count goes up.
    pub fn push(&self, frame: Frame) {
        let mut last = self.frame.lock().unwrap();
        let same = last.as_ref().is_some_and(|l| {
            (l.window_id, l.width, l.height, l.stride, l.order)
                == (
                    frame.window_id,
                    frame.width,
                    frame.height,
                    frame.stride,
                    frame.order,
                )
                && l.data == frame.data
        });
        if same {
            return;
        }
        *last = Some(frame);
        self.count.fetch_add(1, Ordering::Release);
        self.arrived.notify_all();
    }

    /// The capture ended (its window closed, or it failed): the view goes
    /// back to screenshots.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.arrived.notify_all();
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    pub fn is_running(&self) -> bool {
        !self.stopped.load(Ordering::Acquire)
    }

    /// The newest picture, waiting up to `wait` for a first one.
    fn latest(&self, wait: Duration) -> Option<Frame> {
        let deadline = Instant::now() + wait;
        let mut frame = self.frame.lock().unwrap();
        while frame.is_none() && self.is_running() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            frame = self.arrived.wait_timeout(frame, left).unwrap().0;
        }
        frame.clone()
    }
}

/// A window's stream, what it was started for, and when it was last looked
/// at.
struct Watched {
    window: WindowInfo,
    stream: Stream,
    used: Instant,
}

/// The streams of the windows being watched.
#[derive(Default)]
struct Live {
    streams: HashMap<u64, Watched>,
    /// Goes up with each new stream, so generations never repeat.
    epoch: u64,
    /// Windows whose streams would not start, and when.
    failed: HashMap<u64, Instant>,
    /// When a picture last had to be a screenshot.
    polled: Option<Instant>,
}

/// What the session last said, for when an action holds its lock.
struct Seen {
    windows: Vec<WindowInfo>,
    alive: bool,
}

pub struct ScreenshotViewer {
    /// Weak, so the view doesn't keep a session going after it ends.
    session: Weak<Shared>,
    capturer: Option<Arc<dyn Capturer>>,
    live: Mutex<Live>,
    seen: Mutex<Seen>,
    state: Mutex<State>,
    started: Instant,
}

impl ScreenshotViewer {
    /// Watches `session`, streaming through `capturer` when there is one.
    pub fn watching(session: &Arc<Shared>, capturer: Option<Arc<dyn Capturer>>) -> Arc<dyn Viewer> {
        Arc::new(Self {
            session: Arc::downgrade(session),
            capturer,
            live: Mutex::default(),
            seen: Mutex::new(Seen {
                windows: Vec::new(),
                alive: true,
            }),
            state: Mutex::new(State::default()),
            started: Instant::now(),
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut dyn Session) -> Result<R>) -> Result<R> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| anyhow!("the session has ended"))?;
        let mut session = session.lock().unwrap();
        f(&mut **session)
    }

    /// `f` on the session if no one else holds it, else `None`.
    fn try_with<R>(&self, f: impl FnOnce(&mut dyn Session) -> Result<R>) -> Option<Result<R>> {
        let Some(session) = self.session.upgrade() else {
            return Some(Err(anyhow!("the session has ended")));
        };
        let mut session = match session.try_lock() {
            Ok(s) => s,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(_)) => {
                return Some(Err(anyhow!("the session failed mid-action")))
            }
        };
        Some(f(&mut **session))
    }

    fn list(&self) -> Result<Vec<WindowInfo>> {
        let windows = self.with(|s| s.windows())?;
        self.seen.lock().unwrap().windows = windows.clone();
        Ok(windows)
    }

    /// The window a picture of `window` is of, as last listed.
    fn target(&self, window: Option<u64>) -> Result<WindowInfo> {
        let known = pick_window(&self.seen.lock().unwrap().windows, window)
            .ok()
            .cloned();
        match known {
            Some(w) => Ok(w),
            None => pick_window(&self.list()?, window).cloned(),
        }
    }

    /// The newest streamed picture of `target`, starting its stream if it
    /// has none (or one at another size); `None` to take a screenshot.
    fn streamed(&self, target: &WindowInfo) -> Option<Frame> {
        let capturer = self.capturer.as_ref()?;
        let mut live = self.live.lock().unwrap();
        live.streams
            .retain(|&id, w| id == target.id || w.used.elapsed() < UNWATCHED);
        let current = live
            .streams
            .get(&target.id)
            .is_some_and(|w| same_size(&w.window, target) && w.stream.frames.is_running());
        if !current {
            if live
                .failed
                .get(&target.id)
                .is_some_and(|at| at.elapsed() < RETRY)
            {
                return None;
            }
            // The old capture stops before the new one starts.
            live.streams.remove(&target.id);
            match capturer.stream(target) {
                Ok(stream) => {
                    live.streams.insert(
                        target.id,
                        Watched {
                            window: target.clone(),
                            stream,
                            used: Instant::now(),
                        },
                    );
                    live.epoch += 1;
                    live.failed.remove(&target.id);
                }
                Err(e) => {
                    log::warn!(
                        "can't stream window {}; polling its screenshots: {e:#}",
                        target.id
                    );
                    live.failed.insert(target.id, Instant::now());
                    return None;
                }
            }
        }
        let watched = live.streams.get_mut(&target.id)?;
        watched.used = Instant::now();
        let frames = watched.stream.frames.clone();
        drop(live);
        frames.latest(FIRST_FRAME)
    }

    fn screenshot(&self, window: Option<u64>) -> Result<Frame> {
        let shot = self.with(|s| s.screenshot(window))?;
        let (width, height, data) = rgba(&shot.png)?;
        Ok(Frame {
            window_id: shot.window_id.or(window).unwrap_or(0),
            width,
            height,
            stride: width as usize * 4,
            data,
            order: Order::Rgba,
        })
    }
}

/// Whether two listings of a window are the same size, to the pixel.
fn same_size(a: &WindowInfo, b: &WindowInfo) -> bool {
    a.frame.width.round() == b.frame.width.round()
        && a.frame.height.round() == b.frame.height.round()
}

impl Viewer for ScreenshotViewer {
    /// Mid-action, the windows as they were before it.
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        let busy_with = {
            let seen = self.seen.lock().unwrap();
            (!seen.windows.is_empty()).then(|| seen.windows.clone())
        };
        let Some(before) = busy_with else {
            return self.list();
        };
        match self.try_with(|s| s.windows()) {
            Some(windows) => {
                let windows = windows?;
                self.seen.lock().unwrap().windows = windows.clone();
                Ok(windows)
            }
            None => Ok(before),
        }
    }

    fn frame(&self, window: Option<u64>) -> Result<Frame> {
        if self.capturer.is_some() {
            if let Some(frame) = self.streamed(&self.target(window)?) {
                return Ok(frame);
            }
            self.live.lock().unwrap().polled = Some(Instant::now());
        }
        self.screenshot(window)
    }

    fn input(&self, window: Option<u64>, input: &ViewInput) -> Result<()> {
        let actions = self.state.lock().unwrap().actions(input);
        for action in actions {
            self.with(|s| s.perform(window, &action))?;
        }
        Ok(())
    }

    /// With a capturer, the streams' pictures counted, plus a tick every
    /// [`POLL`] while some picture had to be a screenshot, so a view polls
    /// only while it has to.
    fn generation(&self) -> Option<u64> {
        self.capturer.as_ref()?;
        let live = self.live.lock().unwrap();
        let pictures: u64 = live.streams.values().map(|w| w.stream.frames.count()).sum();
        let ticks = match live.polled {
            Some(at) if at.elapsed() < Duration::from_secs(1) => {
                1 << 31 | (self.started.elapsed().as_millis() / POLL.as_millis()) as u64
            }
            _ => 0,
        };
        Some((live.epoch << 32) + (pictures & 0x7fff_ffff) + ticks)
    }

    /// Mid-action, alive: a session's actions end with it.
    fn is_alive(&self) -> bool {
        match self.try_with(|s| Ok(s.is_alive())) {
            Some(alive) => {
                let alive = alive.unwrap_or(false);
                self.seen.lock().unwrap().alive = alive;
                alive
            }
            None => self.session.strong_count() > 0 && self.seen.lock().unwrap().alive,
        }
    }
}

/// A screenshot's PNG as 8-bit RGBA, whatever it was saved as.
fn rgba(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let pixels = &buf[..info.buffer_size()];
    let data = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        other => bail!("a screenshot came as {other:?}, which the view can't show"),
    };
    Ok((info.width, info.height, data))
}

/// What a watcher is holding down, between their inputs.
#[derive(Default)]
struct State {
    /// Where a button went down, and which, until it comes up.
    pressed: Option<(f64, f64, MouseButton)>,
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
}

impl State {
    /// The session's actions for one input: a press and its release make a
    /// click (or a drag, if the pointer went far), and a key is pressed with
    /// whatever modifiers are held.
    fn actions(&mut self, input: &ViewInput) -> Vec<Action> {
        match input {
            // Mid-drag, the release says where it ends.
            ViewInput::Move { .. } if self.pressed.is_some() => vec![],
            ViewInput::Move { x, y } => vec![Action::MoveMouse { x: *x, y: *y }],
            ViewInput::Button {
                x,
                y,
                button,
                down: true,
            } => {
                self.pressed = Some((*x, *y, *button));
                vec![]
            }
            ViewInput::Button {
                x, y, down: false, ..
            } => {
                let Some((from_x, from_y, button)) = self.pressed.take() else {
                    return vec![];
                };
                let modifiers = self.modifiers();
                if (x - from_x).hypot(y - from_y) <= CLICK_SLOP {
                    vec![Action::Click {
                        x: from_x,
                        y: from_y,
                        button,
                        count: 1,
                        modifiers,
                    }]
                } else {
                    vec![Action::Drag {
                        from_x,
                        from_y,
                        to_x: *x,
                        to_y: *y,
                        button,
                        modifiers,
                    }]
                }
            }
            ViewInput::Wheel { x, y, dx, dy } => vec![Action::Scroll {
                x: *x,
                y: *y,
                dx: *dx,
                dy: *dy,
            }],
            ViewInput::Key { code, down } => {
                if let Some(held) = self.modifier(code) {
                    *held = *down;
                    return vec![];
                }
                match (down, key_name(code)) {
                    (true, Some(name)) => {
                        let keys = match self.modifiers() {
                            Some(m) => format!("{m}+{name}"),
                            None => name.to_string(),
                        };
                        vec![Action::PressKey { keys }]
                    }
                    _ => vec![],
                }
            }
            ViewInput::Text { text } => vec![Action::TypeText { text: text.clone() }],
        }
    }

    fn modifier(&mut self, code: &str) -> Option<&mut bool> {
        match code {
            "ShiftLeft" | "ShiftRight" => Some(&mut self.shift),
            "ControlLeft" | "ControlRight" => Some(&mut self.ctrl),
            "AltLeft" | "AltRight" => Some(&mut self.alt),
            "MetaLeft" | "MetaRight" | "OSLeft" | "OSRight" => Some(&mut self.meta),
            _ => None,
        }
    }

    /// What's held, as `press_key` and clicks write it: "ctrl+shift".
    fn modifiers(&self) -> Option<String> {
        let held: Vec<&str> = [
            (self.ctrl, "ctrl"),
            (self.alt, "alt"),
            (self.shift, "shift"),
            (self.meta, "cmd"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        (!held.is_empty()).then(|| held.join("+"))
    }
}

/// A key's name in a `press_key` chord, from its DOM `KeyboardEvent.code`.
fn key_name(code: &str) -> Option<String> {
    if let Some(letter) = code.strip_prefix("Key").filter(|l| l.len() == 1) {
        return Some(letter.to_ascii_lowercase());
    }
    if let Some(digit) = code
        .strip_prefix("Digit")
        .or_else(|| code.strip_prefix("Numpad"))
        .filter(|d| d.len() == 1 && d.chars().all(|c| c.is_ascii_digit()))
    {
        return Some(digit.to_string());
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=24).contains(&n).then(|| format!("f{n}"));
    }
    let name = match code {
        "Enter" | "NumpadEnter" => "enter",
        "Tab" => "tab",
        "Escape" => "escape",
        "Backspace" => "backspace",
        "Delete" => "delete",
        "Space" => "space",
        "ArrowUp" => "up",
        "ArrowDown" => "down",
        "ArrowLeft" => "left",
        "ArrowRight" => "right",
        "Home" => "home",
        "End" => "end",
        "PageUp" => "pageup",
        "PageDown" => "pagedown",
        "Insert" => "insert",
        "Minus" | "NumpadSubtract" => "-",
        "Equal" => "=",
        "NumpadAdd" => "+",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Backslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "Comma" => ",",
        "Period" | "NumpadDecimal" => ".",
        "Slash" | "NumpadDivide" => "/",
        "NumpadMultiply" => "*",
        "Backquote" => "`",
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(state: &mut State, x: f64, y: f64, down: bool) -> Vec<Action> {
        state.actions(&ViewInput::Button {
            x,
            y,
            button: MouseButton::Left,
            down,
        })
    }

    #[test]
    fn a_press_and_release_in_place_is_a_click() {
        let mut state = State::default();
        assert!(press(&mut state, 10.0, 20.0, true).is_empty());
        let actions = press(&mut state, 11.0, 21.0, false);
        assert!(matches!(
            actions.as_slice(),
            [Action::Click { x, y, count: 1, modifiers: None, .. }] if *x == 10.0 && *y == 20.0
        ));
    }

    #[test]
    fn a_press_and_release_far_apart_is_a_drag() {
        let mut state = State::default();
        press(&mut state, 10.0, 20.0, true);
        assert!(state
            .actions(&ViewInput::Move { x: 50.0, y: 60.0 })
            .is_empty());
        let actions = press(&mut state, 100.0, 120.0, false);
        assert!(matches!(
            actions.as_slice(),
            [Action::Drag { from_x, to_x, to_y, .. }] if *from_x == 10.0 && *to_x == 100.0 && *to_y == 120.0
        ));
    }

    #[test]
    fn keys_go_with_the_modifiers_held() {
        let mut state = State::default();
        let key = |code: &str, down| ViewInput::Key {
            code: code.into(),
            down,
        };
        assert!(state.actions(&key("ControlLeft", true)).is_empty());
        assert!(state.actions(&key("ShiftRight", true)).is_empty());
        let actions = state.actions(&key("KeyT", true));
        assert!(
            matches!(actions.as_slice(), [Action::PressKey { keys }] if keys == "ctrl+shift+t")
        );
        // Released, they're held no longer; a key coming up does nothing.
        state.actions(&key("ControlLeft", false));
        state.actions(&key("ShiftRight", false));
        assert!(state.actions(&key("KeyT", false)).is_empty());
        let actions = state.actions(&key("Enter", true));
        assert!(matches!(actions.as_slice(), [Action::PressKey { keys }] if keys == "enter"));
    }

    #[test]
    fn names_the_keys_a_watcher_presses() {
        assert_eq!(key_name("KeyA").as_deref(), Some("a"));
        assert_eq!(key_name("Digit7").as_deref(), Some("7"));
        assert_eq!(key_name("Numpad3").as_deref(), Some("3"));
        assert_eq!(key_name("F12").as_deref(), Some("f12"));
        assert_eq!(key_name("ArrowLeft").as_deref(), Some("left"));
        assert_eq!(key_name("Backquote").as_deref(), Some("`"));
        assert_eq!(key_name("NumpadEnter").as_deref(), Some("enter"));
        assert_eq!(key_name("ContextMenu"), None);
    }

    #[test]
    fn reads_a_screenshot_as_rgba() {
        let rgb = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let png = crate::image::encode_png(
            2,
            2,
            2 * 4,
            &[
                rgb[0], rgb[1], rgb[2], 0, rgb[3], rgb[4], rgb[5], 0, rgb[6], rgb[7], rgb[8], 0,
                rgb[9], rgb[10], rgb[11], 0,
            ],
            Order::Rgba,
        )
        .unwrap();
        let (w, h, data) = rgba(&png).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&data[..8], &[1, 2, 3, 255, 4, 5, 6, 255]);
    }

    mod streaming {
        use super::super::*;
        use crate::backend::Description;
        use crate::types::{Rect, Screenshot};
        use std::sync::mpsc;

        fn window(id: u64, width: f64) -> WindowInfo {
            WindowInfo {
                id,
                title: format!("window {id}"),
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width,
                    height: 20.0,
                },
                on_screen: true,
            }
        }

        /// A session whose windows the test sets, counting its screenshots.
        struct Fake {
            windows: Arc<Mutex<Vec<WindowInfo>>>,
            shots: Arc<AtomicU64>,
        }

        impl Session for Fake {
            fn describe(&self) -> Description {
                Description::default()
            }
            fn windows(&mut self) -> Result<Vec<WindowInfo>> {
                Ok(self.windows.lock().unwrap().clone())
            }
            fn screenshot(&mut self, window: Option<u64>) -> Result<crate::types::Screenshot> {
                self.shots.fetch_add(1, Ordering::Relaxed);
                let png = crate::image::encode_png(1, 1, 4, &[9, 9, 9, 0], Order::Rgba)?;
                Ok(Screenshot {
                    window_id: window,
                    width: 1,
                    height: 1,
                    png,
                })
            }
            fn perform(&mut self, _: Option<u64>, _: &Action) -> Result<()> {
                Ok(())
            }
            fn is_alive(&mut self) -> bool {
                true
            }
            fn close(&mut self) {}
        }

        /// Hands out streams the test pushes pictures into, or refuses.
        #[derive(Default)]
        struct Streams {
            started: Mutex<Vec<(WindowInfo, Arc<Latest>)>>,
            refuse: AtomicBool,
        }

        impl Capturer for Streams {
            fn stream(&self, window: &WindowInfo) -> Result<Stream> {
                if self.refuse.load(Ordering::Relaxed) {
                    bail!("no capture here");
                }
                let latest = Latest::new();
                self.started
                    .lock()
                    .unwrap()
                    .push((window.clone(), latest.clone()));
                Ok(Stream::new(latest, ()))
            }
        }

        impl Streams {
            fn latest(&self) -> Arc<Latest> {
                self.started.lock().unwrap().last().unwrap().1.clone()
            }
            fn count(&self) -> usize {
                self.started.lock().unwrap().len()
            }
        }

        fn picture(window_id: u64, shade: u8) -> Frame {
            Frame {
                window_id,
                width: 1,
                height: 1,
                stride: 4,
                data: vec![shade, shade, shade, 255],
                order: Order::Bgra,
            }
        }

        struct Rig {
            session: Arc<Shared>,
            windows: Arc<Mutex<Vec<WindowInfo>>>,
            shots: Arc<AtomicU64>,
            streams: Arc<Streams>,
            viewer: Arc<dyn Viewer>,
        }

        fn rig(capture: bool) -> Rig {
            let windows = Arc::new(Mutex::new(vec![window(1, 30.0), window(2, 40.0)]));
            let shots = Arc::new(AtomicU64::new(0));
            let session: Arc<Shared> = Arc::new(Mutex::new(Box::new(Fake {
                windows: windows.clone(),
                shots: shots.clone(),
            })));
            let streams = Arc::new(Streams::default());
            let capturer = capture.then(|| streams.clone() as Arc<dyn Capturer>);
            let viewer = ScreenshotViewer::watching(&session, capturer);
            Rig {
                session,
                windows,
                shots,
                streams,
                viewer,
            }
        }

        /// Runs `f` on another thread, failing if it takes over two seconds
        /// (it is waiting on a lock it should not need).
        fn promptly<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(f());
            });
            rx.recv_timeout(Duration::from_secs(2))
                .expect("waited on the session's lock")
        }

        #[test]
        fn without_a_capturer_the_view_polls() {
            let rig = rig(false);
            assert_eq!(rig.viewer.generation(), None);
            let frame = rig.viewer.frame(None).unwrap();
            assert_eq!(
                (frame.order, &frame.data[..]),
                (Order::Rgba, &[9, 9, 9, 255][..])
            );
            assert_eq!(rig.shots.load(Ordering::Relaxed), 1);
        }

        #[test]
        fn streamed_pictures_count_up_the_generation() {
            let rig = rig(true);
            let streams = rig.streams.clone();
            std::thread::spawn(move || {
                // The first picture comes a moment after the stream starts.
                while streams.count() == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(50));
                streams.latest().push(picture(1, 7));
            });
            let frame = rig.viewer.frame(None).unwrap();
            assert_eq!((frame.order, frame.data[0]), (Order::Bgra, 7));
            assert_eq!(rig.shots.load(Ordering::Relaxed), 0, "no screenshot taken");
            let started = rig.viewer.generation().unwrap();
            // Nothing new, nothing to send.
            std::thread::sleep(POLL * 2);
            assert_eq!(rig.viewer.generation(), Some(started));
            // The same picture again is nothing new either.
            rig.streams.latest().push(picture(1, 7));
            assert_eq!(rig.viewer.generation(), Some(started));
            rig.streams.latest().push(picture(1, 8));
            assert_eq!(rig.viewer.generation(), Some(started + 1));
            assert_eq!(rig.viewer.frame(None).unwrap().data[0], 8);
            assert_eq!(rig.streams.count(), 1, "one stream, kept");
        }

        #[test]
        fn the_view_does_not_wait_for_an_action() {
            let rig = rig(true);
            rig.viewer.windows().unwrap();
            rig.viewer.frame(Some(2)).unwrap();
            rig.streams.latest().push(picture(2, 3));
            // An action holds the session from here on.
            let _busy = rig.session.lock().unwrap();
            let viewer = rig.viewer.clone();
            let streams = rig.streams.clone();
            let (windows, alive, before, frame, after) = promptly(move || {
                let windows = viewer.windows().unwrap();
                let alive = viewer.is_alive();
                let before = viewer.generation().unwrap();
                streams.latest().push(picture(2, 4));
                let frame = viewer.frame(Some(2)).unwrap();
                (windows, alive, before, frame, viewer.generation().unwrap())
            });
            assert_eq!(windows.len(), 2);
            assert!(alive);
            assert_eq!(frame.data[0], 4);
            assert_eq!(after, before + 1);
        }

        #[test]
        fn each_window_and_size_has_a_stream_of_its_own() {
            let rig = rig(true);
            rig.viewer.windows().unwrap();
            rig.viewer.frame(Some(1)).unwrap();
            let first = rig.viewer.generation().unwrap();
            rig.viewer.frame(Some(2)).unwrap();
            assert_eq!(rig.streams.count(), 2);
            let second = rig.viewer.generation().unwrap();
            assert_ne!(first, second);
            // Two people watching two windows each keep their stream.
            rig.viewer.frame(Some(1)).unwrap();
            assert_eq!(rig.streams.count(), 2);
            rig.windows.lock().unwrap()[1] = window(2, 64.0);
            rig.viewer.windows().unwrap();
            rig.viewer.frame(Some(2)).unwrap();
            assert_eq!(rig.streams.count(), 3);
            let started = rig.streams.started.lock().unwrap();
            assert_eq!(started[2].0.frame.width, 64.0);
            // Restarted streams never repeat a generation.
            assert!(![first, second].contains(&rig.viewer.generation().unwrap()));
        }

        #[test]
        fn without_a_stream_the_view_falls_back_to_screenshots() {
            let rig = rig(true);
            rig.streams.refuse.store(true, Ordering::Relaxed);
            let frame = rig.viewer.frame(None).unwrap();
            assert_eq!(frame.order, Order::Rgba);
            // Polled, so the generation ticks without pictures.
            let before = rig.viewer.generation().unwrap();
            std::thread::sleep(POLL + Duration::from_millis(20));
            assert_ne!(rig.viewer.generation().unwrap(), before);
            // Not asked again straight away.
            rig.streams.refuse.store(false, Ordering::Relaxed);
            rig.viewer.frame(None).unwrap();
            assert_eq!(rig.streams.count(), 0);
            assert_eq!(rig.shots.load(Ordering::Relaxed), 2);
        }

        #[test]
        fn a_stopped_stream_is_replaced() {
            let rig = rig(true);
            let streams = rig.streams.clone();
            std::thread::spawn(move || {
                while streams.count() == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                streams.latest().push(picture(1, 1));
            });
            rig.viewer.frame(None).unwrap();
            // Its window closed, say.
            rig.streams.latest().stop();
            rig.viewer.frame(None).unwrap();
            assert_eq!(rig.streams.count(), 2);
        }
    }
}
