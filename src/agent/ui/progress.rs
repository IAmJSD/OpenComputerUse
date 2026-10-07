//! A progress bar. In Schist it shares a module with the slider, whose
//! track it borrows; only the bar came across.

use super::palette;
use gpui::{
    div, px, App, IntoElement, ParentElement as _, Refineable as _, RenderOnce, StyleRefinement,
    Styled, Window,
};

/// The two colours of a track: its bed, and the filled part.
#[derive(Clone, Copy, Debug)]
pub struct TrackColors {
    pub track: u32,
    pub fill: u32,
}

impl TrackColors {
    fn progress() -> Self {
        let p = palette();
        TrackColors {
            track: p.button_bg,
            fill: p.accent,
        }
    }
}

/// A bar filled to a ratio: 4 px high, as wide as its row.
#[derive(gpui::IntoElement)]
pub struct ProgressBar {
    ratio: f32,
    colors: Option<TrackColors>,
    style: StyleRefinement,
}

impl ProgressBar {
    pub fn new(ratio: f32) -> Self {
        ProgressBar {
            ratio: ratio.clamp(0.0, 1.0),
            colors: None,
            style: StyleRefinement::default(),
        }
    }

    /// Colours of the caller's own choosing.
    pub fn colors(mut self, colors: TrackColors) -> Self {
        self.colors = Some(colors);
        self
    }
}

impl Styled for ProgressBar {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for ProgressBar {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let colors = self.colors.unwrap_or_else(TrackColors::progress);
        let mut el = div()
            .w_full()
            .h(px(4.0))
            .rounded_sm()
            .bg(gpui::rgb(colors.track));
        el.style().refine(&self.style);
        el.child(
            div()
                .h_full()
                .w(gpui::relative(self.ratio))
                .rounded_sm()
                .bg(gpui::rgb(colors.fill)),
        )
    }
}
