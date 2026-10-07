//! The widget kit: the chrome components every page builds from, and the
//! palette they draw with.
//!
//! Copied from VisualHub, which copied it from Schist's `schist-ui` crate (Infrawrench/schist, crates/ui,
//! at 362c598b), MIT licensed; see `LICENSE-SCHIST` beside this file.
//! Only the components VisualHub draws with came across: the slider,
//! number field, radio, swatch and document tab did not, and the progress
//! bar was lifted out of the slider's module into `progress.rs`. Inside
//! the copied files `crate::` became `super::`; nothing else changed.
//! Not every builder method or export is used here; both are allowed.
//!
//! GPUI ships no widget library. Every pressable component here commits
//! on *release*, over the control, as native buttons do -- that is what
//! GPUI's `on_click` implements -- never on the press alone.
//!
//! The components know nothing about the app. They take plain
//! `Fn(&ClickEvent, &mut Window, &mut App)` handlers, which is exactly
//! what `Context::listener` produces, and every component is a
//! [`gpui::RenderOnce`] value implementing [`gpui::Styled`], so a caller
//! can adjust its size or colours with the usual fluent methods; those
//! refinements win over the component's own defaults.
//!
//! The exceptions are the gestures native chrome starts on the way down:
//! a field taking the keyboard ([`TextInput::on_focus`]), a dropdown
//! opening ([`DropdownButton::on_press`]), a popup dismissing on a press
//! outside it ([`Popover::on_dismiss`]). Those take a [`PressHandler`].
//!
//! Icons are monochrome SVGs served by the app's asset source under
//! `icons/<name>.svg`; see [`icon`].

#![allow(dead_code, unused_imports)]

mod button;
mod checkbox;
mod chip;
mod icon;
mod layout;
mod line_edit;
mod link;
mod list_item;
mod popover;
mod progress;
mod spinner;
mod text_input;
mod theme;
mod tooltip;

pub use button::{Button, ButtonColors, ButtonVariant, IconButton};
pub use checkbox::Checkbox;
pub use chip::{Badge, Chip, ChipColors};
pub use icon::icon;
pub use layout::{Divider, FieldRow, Heading, Modal};
pub use line_edit::{caret_left, caret_right, word_at, LineEdit, LineEditKey};
pub use link::Link;
pub use list_item::ListItem;
pub use popover::{menu_separator, DropdownButton, MenuItem, Popover};
pub use progress::{ProgressBar, TrackColors};
pub use spinner::Spinner;
pub use text_input::{OffsetHandler, TextInput, TextInputColors, TextPress, TextPressHandler};
pub use theme::{
    is_light, metrics, palette, set_light, touch, Metrics, Palette, DARK, DESKTOP_METRICS, LIGHT,
    TOUCH_METRICS,
};
pub use tooltip::{tip, Tooltip};

/// What a component's `on_click` takes: fired on release, over the
/// control, or from the keyboard (Enter or Space while focused).
pub type ClickHandler = Box<dyn Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static>;

/// A handler told a yes-or-no: a checkbox's new state, a row's hover.
pub type BoolHandler = Box<dyn Fn(&bool, &mut gpui::Window, &mut gpui::App) + 'static>;

/// What the components that act on the *press* take: a field taking
/// the keyboard, a dropdown opening, a popup dismissing on a press
/// outside it.
///
/// On a touch screen ([`touch`]) the press is delivered when the finger
/// lifts off the element instead, so a swipe that starts on a field or a
/// dropdown never opens it.
pub type PressHandler =
    Box<dyn Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static>;

/// Bind a [`PressHandler`] to the left button the way the platform
/// expects: on the press itself, or on touch on the finger lifting.
fn on_press<E: gpui::InteractiveElement>(el: E, handler: PressHandler) -> E {
    if touch() {
        el.on_mouse_up(gpui::MouseButton::Left, move |ev, window, cx| {
            let press = gpui::MouseDownEvent {
                button: ev.button,
                position: ev.position,
                modifiers: ev.modifiers,
                click_count: ev.click_count,
                first_mouse: false,
                pressure: ev.pressure,
                tilt: ev.tilt,
            };
            handler(&press, window, cx)
        })
    } else {
        el.on_mouse_down(gpui::MouseButton::Left, handler)
    }
}
