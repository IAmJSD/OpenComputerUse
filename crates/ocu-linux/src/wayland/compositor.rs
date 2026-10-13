//! The compositor's state and the Wayland protocols it speaks: just enough
//! of a desktop for apps to map windows nobody local sees. Windows sit in a
//! Smithay [`Space`] on one virtual output; parentless ones fill it, dialogs
//! are centred over their parent, and popups (menus, tooltips) belong to
//! their window. Pictures are drawn with the pixman renderer, in software.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use smithay::backend::allocator::Fourcc;
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::backend::renderer::{Bind, Offscreen};
use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager, Space, Window,
    WindowSurfaceType,
};
use smithay::input::keyboard::{FilterResult, Keycode, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Serial, Size, Transform, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
    CompositorState,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
    ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::viewporter::ViewporterState;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_viewporter, delegate_xdg_decoration, delegate_xdg_shell,
};

use ocu_core::image::Order;
use ocu_core::keys::us_key;
use ocu_core::{Frame, MouseButton, Rect, WindowInfo};

use super::Shared;

/// Linux's button codes, which `wl_pointer` speaks.
fn button_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
    }
}

/// What OCU knows about a window beside Smithay: its id in `list_windows`,
/// and for a dialog whether it has been put over its parent yet.
struct Meta {
    id: u64,
    placed: AtomicBool,
}

fn meta(w: &Window) -> &Meta {
    w.user_data()
        .get::<Meta>()
        .expect("every window gets its meta when made")
}

pub(super) struct State {
    pub dh: DisplayHandle,
    compositor: CompositorState,
    xdg: XdgShellState,
    shm: ShmState,
    seat_state: SeatState<State>,
    data_device: DataDeviceState,
    // Held for their globals, which go when they are dropped.
    _decoration: XdgDecorationState,
    _viewporter: ViewporterState,
    _outputs: OutputManagerState,
    pub seat: Seat<State>,
    pub output: Output,
    pub space: Space<Window>,
    pub popups: PopupManager,
    /// The output's size, which parentless windows are configured to fill.
    pub size: Size<i32, Logical>,
    started: Instant,
    next_window: u64,
    shared: Arc<Shared>,
    /// Popups that asked for a grab (open menus), oldest first: a press
    /// outside them closes them, as on a desktop.
    grabs: Vec<PopupKind>,
    /// Windows that said they are dialogs, to settle after dispatching:
    /// Smithay tells us while it holds their state.
    reparented: Vec<ToplevelSurface>,
}

impl State {
    pub fn new(dh: &DisplayHandle, size: Size<i32, Logical>, shared: Arc<Shared>) -> Result<Self> {
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "ocu");
        // A rate of 0 turns off the app's own key repeat: a key the agent
        // holds for a moment must never type twice. A person's held key
        // repeats through their browser's own repeated presses instead.
        seat.add_keyboard(XkbConfig::default(), 600, 0)
            .map_err(|e| {
                anyhow!("no keyboard ({e:?}); is xkb-data (xkeyboard-config) installed?")
            })?;
        seat.add_pointer();

        let output = Output::new(
            "OCU-1".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "OpenComputerUse".into(),
                model: "Virtual".into(),
            },
        );
        let _global = output.create_global::<State>(dh);
        let mut space = Space::default();
        let mut state = Self {
            dh: dh.clone(),
            compositor: CompositorState::new::<Self>(dh),
            xdg: XdgShellState::new::<Self>(dh),
            shm: ShmState::new::<Self>(dh, vec![]),
            data_device: DataDeviceState::new::<Self>(dh),
            _decoration: XdgDecorationState::new::<Self>(dh),
            _viewporter: ViewporterState::new::<Self>(dh),
            _outputs: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat_state,
            seat,
            output: output.clone(),
            space: {
                space.map_output(&output, (0, 0));
                space
            },
            popups: PopupManager::default(),
            size,
            started: Instant::now(),
            next_window: 1,
            shared,
            grabs: Vec::new(),
            reparented: Vec::new(),
        };
        state.set_output_size(size);
        Ok(state)
    }

    pub fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn millis(&self) -> u32 {
        self.now().as_millis() as u32
    }

    /// Something on screen may have changed.
    pub fn damaged(&self) {
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_output_size(&mut self, size: Size<i32, Logical>) {
        self.size = size;
        let mode = Mode {
            size: (size.w, size.h).into(),
            refresh: 60_000,
        };
        self.output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        self.output.set_preferred(mode);
        for w in self.space.elements() {
            let Some(toplevel) = w.toplevel() else {
                continue;
            };
            if toplevel.parent().is_none() {
                toplevel.with_pending_state(|s| s.size = Some(size));
                toplevel.send_pending_configure();
            }
        }
        self.damaged();
    }

    fn window_for(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    /// Windows with something drawn, topmost first.
    pub fn windows(&self) -> Vec<Window> {
        self.space
            .elements()
            .rev()
            .filter(|w| !w.bbox().size.is_empty())
            .cloned()
            .collect()
    }

    pub fn info(&self, w: &Window) -> WindowInfo {
        let title = w
            .toplevel()
            .map(|t| {
                with_states(t.wl_surface(), |s| {
                    s.data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|d| d.lock().unwrap().title.clone())
                })
                .unwrap_or_default()
            })
            .unwrap_or_default();
        let g = self.space.element_geometry(w).unwrap_or_default();
        WindowInfo {
            id: meta(w).id,
            title,
            frame: Rect {
                x: g.loc.x as f64,
                y: g.loc.y as f64,
                width: g.size.w as f64,
                height: g.size.h as f64,
            },
            on_screen: true,
        }
    }

    /// `window`, or the topmost one.
    pub fn window(&self, window: Option<u64>) -> Result<Window> {
        let windows = self.windows();
        match window {
            Some(id) => windows
                .into_iter()
                .find(|w| meta(w).id == id)
                .ok_or_else(|| anyhow!("the session has no window {id}; call list_windows")),
            None => windows
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("the app has no windows (yet)")),
        }
    }

    /// Apps say a window is a dialog after making it: it then keeps its own
    /// size and goes over its parent.
    pub fn settle_parents(&mut self) {
        for surface in std::mem::take(&mut self.reparented) {
            let Some(w) = self.window_for(surface.wl_surface()) else {
                continue;
            };
            if surface.parent().is_some() {
                surface.with_pending_state(|s| {
                    s.states.unset(xdg_toplevel::State::Maximized);
                    s.size = None;
                });
                surface.send_pending_configure();
                meta(&w).placed.store(false, Ordering::Relaxed);
                self.place(&w);
            }
        }
    }

    /// Lets every surface draw its next frame.
    pub fn send_frames(&self) {
        let now = self.now();
        for w in self.space.elements() {
            w.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| None);
        }
    }

    /// The surface under `pos` (output coordinates) and where its origin is.
    fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (w, at) = self.space.element_under(pos)?;
        w.surface_under(pos - at.to_f64(), WindowSurfaceType::ALL)
            .map(|(s, p)| (s, (p + at).to_f64()))
    }

    /// Gives `w` the keyboard, unless it already has it, or one of its
    /// menus does.
    pub fn focus(&mut self, w: &Window) {
        let Some(root) = w.toplevel().map(|t| t.wl_surface().clone()) else {
            return;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let current = keyboard.current_focus();
        let inside = current.as_ref().is_some_and(|f| {
            *f == root
                || self.grabs.iter().any(|p| {
                    p.wl_surface() == f && find_popup_root_surface(p).ok() == Some(root.clone())
                })
        });
        if !inside {
            keyboard.set_focus(self, Some(root), SERIAL_COUNTER.next_serial());
        }
        self.space.raise_element(w, true);
    }

    pub fn pointer_to(&mut self, pos: Point<f64, Logical>) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let under = self.surface_under(pos);
        let time = self.millis();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    pub fn button(&mut self, b: MouseButton, down: bool) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if down {
            let pos = pointer.current_location();
            let under = self.surface_under(pos).map(|(s, _)| s);
            // A press outside the open menus closes them, newest first.
            if !self.grabs.is_empty()
                && !under
                    .as_ref()
                    .is_some_and(|s| self.grabs.iter().any(|p| p.wl_surface() == s))
            {
                for popup in std::mem::take(&mut self.grabs).iter().rev() {
                    if let Ok(root) = find_popup_root_surface(popup) {
                        let _ = PopupManager::dismiss_popup(&root, popup);
                    }
                }
            }
            if let Some((w, _)) = self.space.element_under(pos) {
                let w = w.clone();
                self.focus(&w);
            }
        }
        let time = self.millis();
        pointer.button(
            self,
            &ButtonEvent {
                button: button_code(b),
                state: if down {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    /// Scrolls by pixels, as wheel notches of 40px each.
    pub fn scroll(&mut self, dx: f64, dy: f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let mut frame = AxisFrame::new(self.millis()).source(AxisSource::Wheel);
        for (axis, d) in [(Axis::Horizontal, dx), (Axis::Vertical, dy)] {
            if d != 0.0 {
                let v120 = ((d / 40.0 * 120.0).round() as i32).clamp(-12_000, 12_000);
                frame = frame.value(axis, d).v120(
                    axis,
                    if v120 == 0 {
                        d.signum() as i32 * 120
                    } else {
                        v120
                    },
                );
            }
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Presses or releases a key by its evdev code.
    pub fn key(&mut self, evdev: u16, down: bool) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let time = self.millis();
        keyboard.input::<(), _>(
            self,
            // xkb numbers keys 8 above evdev, a relic of X.
            Keycode::new(evdev as u32 + 8),
            if down {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    pub fn tap(&mut self, evdev: u16) {
        self.key(evdev, true);
        self.key(evdev, false);
    }

    /// Types `text`: characters on a US keyboard with their own keys (and
    /// shift), the rest through a keymap remapped for them (see
    /// [`ocu_core::xkb`]), restoring `base` after.
    pub fn type_text(&mut self, text: &str, base: &str) -> Result<()> {
        let mut foreign = String::new();
        for c in text.chars() {
            let Some((code, shift)) = us_key(c) else {
                foreign.push(c);
                continue;
            };
            if !foreign.is_empty() {
                self.type_foreign(&std::mem::take(&mut foreign), base)?;
            }
            if shift {
                self.key(42, true);
            }
            self.tap(code);
            if shift {
                self.key(42, false);
            }
        }
        if !foreign.is_empty() {
            self.type_foreign(&foreign, base)?;
        }
        Ok(())
    }

    fn type_foreign(&mut self, text: &str, base: &str) -> Result<()> {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return Ok(());
        };
        let runs = ocu_core::xkb::remap(base, text)
            .ok_or_else(|| anyhow!("the keyboard map could not be read"))?;
        for run in runs {
            keyboard
                .set_keymap_from_string(self, run.keymap)
                .map_err(|e| anyhow!("remapping the keyboard: {e:?}"))?;
            for code in run.presses {
                self.tap(code);
            }
        }
        keyboard
            .set_keymap_from_string(self, base.to_string())
            .map_err(|e| anyhow!("restoring the keyboard: {e:?}"))?;
        Ok(())
    }

    /// Puts a dialog over the middle of its parent, once it has a size.
    fn place(&mut self, w: &Window) {
        let m = meta(w);
        if m.placed.load(Ordering::Relaxed) {
            return;
        }
        let size = w.geometry().size;
        if size.is_empty() {
            return;
        }
        let parent = w
            .toplevel()
            .and_then(|t| t.parent())
            .and_then(|p| self.window_for(&p))
            .and_then(|p| self.space.element_geometry(&p));
        let area = parent.unwrap_or(Rectangle::from_size(self.size));
        let at = (
            (area.loc.x + (area.size.w - size.w) / 2).max(0),
            (area.loc.y + (area.size.h - size.h) / 2).max(0),
        );
        m.placed.store(true, Ordering::Relaxed);
        self.space.map_element(w.clone(), at, true);
    }

    /// Keeps a popup on the output, flipping or sliding it as its
    /// positioner allows.
    fn unconstrain(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Some(w) = self.window_for(&root) else {
            return;
        };
        let Some(geometry) = self.space.element_geometry(&w) else {
            return;
        };
        let mut target = Rectangle::from_size(self.size);
        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= geometry.loc;
        popup.with_pending_state(|s| s.geometry = s.positioner.get_unconstrained_geometry(target));
    }
}

/// Draws `region` of the output (everything in it, windows and popups) in
/// software, as BGRA.
pub(super) fn capture(
    renderer: &mut PixmanRenderer,
    space: &Space<Window>,
    region: Rectangle<i32, Logical>,
    window_id: u64,
) -> Result<Frame> {
    let size = region.size;
    anyhow::ensure!(!size.is_empty(), "the window has no size yet");
    let mut image = renderer
        .create_buffer(Fourcc::Argb8888, (size.w, size.h).into())
        .context("making a picture to draw into")?;
    {
        let elements: Vec<WaylandSurfaceRenderElement<PixmanRenderer>> =
            space.render_elements_for_region(renderer, &region, 1.0, 1.0);
        let mut target = renderer.bind(&mut image).context("drawing")?;
        let mut tracker = OutputDamageTracker::new((size.w, size.h), 1.0, Transform::Normal);
        tracker
            .render_output(renderer, &mut target, 0, &elements, [0.0, 0.0, 0.0, 1.0])
            .map_err(|e| anyhow!("drawing: {e:?}"))?;
    }
    let stride = image.stride();
    let height = image.height();
    // Argb8888 is a native-endian u32 a pixel: B, G, R, A in memory here.
    let data =
        unsafe { std::slice::from_raw_parts(image.data() as *const u8, stride * height) }.to_vec();
    Ok(Frame {
        window_id,
        width: size.w as u32,
        height: size.h as u32,
        stride,
        data,
        order: Order::Bgra,
    })
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(w) = self.window_for(&root) {
                w.on_commit();
                let first =
                    !w.bbox().size.is_empty() && self.space.outputs_for_element(&w).is_empty();
                if w.toplevel().is_some_and(|t| t.parent().is_some()) {
                    self.place(&w);
                }
                if first {
                    // A surface on no output reads to some apps as hidden,
                    // and they stop drawing it.
                    self.space.refresh();
                    // A new window takes the keyboard once it shows, as on a
                    // desktop (before that, some apps crash on it).
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        keyboard.set_focus(self, Some(root.clone()), SERIAL_COUNTER.next_serial());
                    }
                }
            }
        }
        // The first commit asks for the first configure, which permits the
        // app to attach a buffer.
        if let Some(w) = self.window_for(surface) {
            let sent = with_states(surface, |s| {
                s.data_map
                    .get::<XdgToplevelSurfaceData>()
                    .is_some_and(|d| d.lock().unwrap().initial_configure_sent)
            });
            if !sent {
                if let Some(t) = w.toplevel() {
                    t.send_configure();
                }
            }
        }
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                let _ = popup.send_configure();
            }
        }
        self.damaged();
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_window;
        self.next_window += 1;
        // Parentless windows fill the output, like a maximised window with
        // no desktop around it. Dialogs pick their own size.
        let size = self.size;
        // Asked outside with_pending_state, which holds what parent() reads.
        let dialog = surface.parent().is_some();
        surface.with_pending_state(|s| {
            s.states.set(xdg_toplevel::State::Activated);
            if !dialog {
                s.states.set(xdg_toplevel::State::Maximized);
                s.size = Some(size);
            }
        });
        let w = Window::new_wayland_window(surface.clone());
        w.user_data().insert_if_missing_threadsafe(|| Meta {
            id,
            placed: AtomicBool::new(!dialog),
        });
        self.space.map_element(w, (0, 0), true);
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        self.reparented.push(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(w) = self.window_for(surface.wl_surface()) {
            self.space.unmap_elem(&w);
        }
        // The keyboard goes to whichever window is now on top.
        let next = self.windows().into_iter().next();
        if let Some(w) = next {
            if let Some(keyboard) = self.seat.get_keyboard() {
                let focus = w.toplevel().map(|t| t.wl_surface().clone());
                keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
            }
        }
        self.damaged();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|s| s.positioner = positioner);
        self.unconstrain(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // A menu wants the keyboard (its arrows, its Escape) while it is open.
        let kind = PopupKind::Xdg(surface.clone());
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                Some(surface.wl_surface().clone()),
                SERIAL_COUNTER.next_serial(),
            );
        }
        self.grabs.push(kind);
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        let gone = surface.wl_surface().clone();
        self.grabs.retain(|p| p.wl_surface() != &gone && p.alive());
        // The keyboard goes back to the menu it came from, or the window.
        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard.current_focus().as_ref() == Some(&gone) {
                let back = self
                    .grabs
                    .last()
                    .map(|p| p.wl_surface().clone())
                    .or_else(|| surface.get_parent_surface());
                keyboard.set_focus(self, back, SERIAL_COUNTER.next_serial());
            }
        }
        self.damaged();
    }
}

impl XdgDecorationHandler for State {
    // Server-side always, and the server draws none: no shadows or title
    // bars inside the window's picture.
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide)
        });
        toplevel.send_pending_configure();
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: zxdg_toplevel_decoration_v1::Mode,
    ) {
        self.new_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // The clipboard follows the keyboard.
        let client = focused.and_then(|s| self.dh.get_client(s.id()).ok());
        set_data_device_focus(&self.dh, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}
impl OutputHandler for State {}

#[derive(Default)]
pub(super) struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_output!(State);
delegate_viewporter!(State);

/// The event loop's data: the display and everything hanging off it.
pub(super) struct Compositor {
    pub display: Display<State>,
    pub state: State,
    pub renderer: PixmanRenderer,
    /// The US keymap as text, put back after typing through a remapped one.
    pub base_keymap: String,
    pub stop: bool,
}

impl Compositor {
    pub fn new(size: Size<i32, Logical>, shared: Arc<Shared>) -> Result<Self> {
        let display = Display::<State>::new().context("making the Wayland display")?;
        let state = State::new(&display.handle(), size, shared)?;
        let renderer = PixmanRenderer::new().map_err(|e| anyhow!("pixman: {e:?}"))?;
        let base_keymap = {
            use smithay::input::keyboard::xkb;
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
                .ok_or_else(|| anyhow!("no US keymap; is xkb-data (xkeyboard-config) installed?"))?
                .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        };
        Ok(Self {
            display,
            state,
            renderer,
            base_keymap,
            stop: false,
        })
    }

    pub fn frame(&mut self, window: Option<u64>) -> Result<Frame> {
        let w = self.state.window(window)?;
        let region = self
            .state
            .space
            .element_geometry(&w)
            .ok_or_else(|| anyhow!("the window is not on screen"))?;
        capture(&mut self.renderer, &self.state.space, region, meta(&w).id)
    }

    /// `x`, `y` in `window` as a point on the output.
    pub fn point(
        &self,
        window: Option<u64>,
        x: f64,
        y: f64,
    ) -> Result<(Window, Point<f64, Logical>)> {
        let w = self.state.window(window)?;
        let g = self.state.space.element_geometry(&w).unwrap_or_default();
        Ok((w, (g.loc.x as f64 + x, g.loc.y as f64 + y).into()))
    }
}
