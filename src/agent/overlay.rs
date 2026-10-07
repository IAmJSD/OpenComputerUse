//! What the user sees while a session works: a soft halo around the window
//! being driven and a cursor that glides to each place it acts, with a
//! ripple where it clicks. Drawn in a click-through window stacked just
//! above the target, so it moves, hides and reorders with it.

use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, div, hsla, point, px, Context, Hsla, IntoElement, ParentElement as _, PathBuilder,
    Pixels, Point, Render, Styled as _, Window,
};

/// Room around the target window for the halo's glow.
pub const MARGIN: f64 = 14.0;

const GLIDE: Duration = Duration::from_millis(380);
const RIPPLE: Duration = Duration::from_millis(450);
/// How long the halo stays lit after the last action.
const HALO_HOLD: Duration = Duration::from_millis(2500);
const HALO_FADE: Duration = Duration::from_millis(900);

pub struct Overlay {
    from: Point<f32>,
    to: Point<f32>,
    moved_at: Instant,
    clicked_at: Option<Instant>,
    active_at: Instant,
    has_cursor: bool,
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            from: point(0.0, 0.0),
            to: point(0.0, 0.0),
            moved_at: Instant::now() - GLIDE,
            clicked_at: None,
            active_at: Instant::now(),
            has_cursor: false,
        }
    }

    /// Glide the cursor to (`x`, `y`) in target-window points.
    pub fn point_at(&mut self, x: f64, y: f64, click: bool) {
        let to = point(x as f32 + MARGIN as f32, y as f32 + MARGIN as f32);
        self.from = if self.has_cursor {
            self.position(Instant::now())
        } else {
            to
        };
        self.to = to;
        self.moved_at = Instant::now();
        self.has_cursor = true;
        // The ripple waits for the cursor to arrive.
        self.clicked_at = click.then(|| Instant::now() + GLIDE);
        self.active_at = Instant::now();
    }

    /// Light the halo without moving the cursor (typing, element actions).
    pub fn touch(&mut self) {
        self.active_at = Instant::now();
    }

    fn position(&self, now: Instant) -> Point<f32> {
        let t =
            (now.duration_since(self.moved_at).as_secs_f32() / GLIDE.as_secs_f32()).clamp(0.0, 1.0);
        // Ease out: quick start, gentle landing.
        let e = 1.0 - (1.0 - t).powi(3);
        point(
            self.from.x + (self.to.x - self.from.x) * e,
            self.from.y + (self.to.y - self.from.y) * e,
        )
    }

    /// The halo's strength, 0 to 1, with a slow breath while lit.
    fn halo(&self, now: Instant) -> f32 {
        let idle = now.duration_since(self.active_at);
        let base = if idle < HALO_HOLD {
            1.0
        } else {
            1.0 - ((idle - HALO_HOLD).as_secs_f32() / HALO_FADE.as_secs_f32()).min(1.0)
        };
        let breath = 0.82 + 0.18 * (seconds(now) * 2.2).sin();
        base * breath
    }

    fn animating(&self, now: Instant) -> bool {
        now.duration_since(self.moved_at) < GLIDE
            || self.clicked_at.is_some_and(|c| now < c + RIPPLE)
            || now.duration_since(self.active_at) < HALO_HOLD + HALO_FADE
    }
}

/// Seconds on a clock every overlay shares, so their halos breathe together.
fn seconds(now: Instant) -> f32 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    now.duration_since(*START.get_or_init(Instant::now))
        .as_secs_f32()
}

/// Rings in the halo: the edge, then the glow fading outwards.
const GLOW_RINGS: usize = 6;

const ACCENT: (f32, f32, f32) = (0.6, 0.95, 0.66);

fn accent(alpha: f32) -> Hsla {
    hsla(ACCENT.0, ACCENT.1, ACCENT.2, alpha)
}

/// The classic arrow, tip at the origin, 1 unit ≈ 1 point.
const ARROW: &[(f32, f32)] = &[
    (0.0, 0.0),
    (0.0, 17.0),
    (4.2, 13.2),
    (7.0, 19.5),
    (9.8, 18.3),
    (7.1, 12.2),
    (12.4, 12.2),
];

fn arrow(at: Point<f32>, scale: f32, inflate: f32) -> Option<gpui::Path<Pixels>> {
    // Inflating pushes each corner away from the arrow's centre, which
    // makes the outline path that frames the fill.
    let (cx, cy) = (5.0, 10.0);
    let mut b = PathBuilder::fill();
    for (i, &(x, y)) in ARROW.iter().enumerate() {
        let (dx, dy) = (x - cx, y - cy);
        let len = (dx * dx + dy * dy).sqrt().max(0.01);
        let p = point(
            px(at.x + (x + dx / len * inflate) * scale),
            px(at.y + (y + dy / len * inflate) * scale),
        );
        if i == 0 {
            b.move_to(p);
        } else {
            b.line_to(p);
        }
    }
    b.close();
    b.build().ok()
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        if self.animating(now) {
            window.request_animation_frame();
        }
        let halo = self.halo(now);
        let pos = self.position(now);
        let ripple = self.clicked_at.and_then(|c| {
            let t = now.checked_duration_since(c)?.as_secs_f32() / RIPPLE.as_secs_f32();
            (t < 1.0).then_some(t)
        });
        let has_cursor = self.has_cursor;
        let m = px(MARGIN as f32);

        div()
            .size_full()
            .relative()
            // The halo: a crisp ring on the window's edge and fainter rings
            // outside it, which read as a glow. (A box shadow would fill the
            // whole window: GPUI draws shadows as solid blurred rectangles.)
            .children((0..GLOW_RINGS).map(|i| {
                let out = i as f32 * 2.0;
                let fade = 1.0 - i as f32 / GLOW_RINGS as f32;
                let alpha = if i == 0 { 0.9 } else { 0.32 * fade * fade };
                div()
                    .absolute()
                    .top(m - px(1.0 + out))
                    .left(m - px(1.0 + out))
                    .right(m - px(1.0 + out))
                    .bottom(m - px(1.0 + out))
                    .rounded(px(11.0 + out))
                    .border_2()
                    .border_color(accent(alpha * halo))
            }))
            .when_some(ripple, |d, t| {
                let r = 6.0 + 22.0 * t;
                d.child(
                    div()
                        .absolute()
                        .left(px(self.to.x - r))
                        .top(px(self.to.y - r))
                        .size(px(r * 2.0))
                        .rounded_full()
                        .border_2()
                        .border_color(accent(0.9 * (1.0 - t))),
                )
            })
            .when(has_cursor, |d| {
                d.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            // A soft shadow, a dark outline, then the fill.
                            if let Some(p) = arrow(point(pos.x + 1.0, pos.y + 1.5), 1.15, 1.6) {
                                window.paint_path(p, hsla(0.0, 0.0, 0.0, 0.25));
                            }
                            if let Some(p) = arrow(pos, 1.15, 1.3) {
                                window.paint_path(p, hsla(0.0, 0.0, 1.0, 1.0));
                            }
                            if let Some(p) = arrow(pos, 1.15, 0.0) {
                                window.paint_path(p, accent(1.0));
                            }
                        },
                    )
                    .absolute()
                    .size_full(),
                )
            })
    }
}
