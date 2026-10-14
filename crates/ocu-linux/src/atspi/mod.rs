//! The accessibility tree on Linux, through AT-SPI: the same on X11 and
//! Wayland, since it runs over D-Bus rather than the display.

pub mod map;

#[cfg(target_os = "linux")]
mod bus;

#[cfg(target_os = "linux")]
pub use bus::A11y;
