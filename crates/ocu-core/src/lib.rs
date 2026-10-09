//! The platform-neutral core of opencomputeruse.
//!
//! Backends (`ocu-macos`, `ocu-linux`, `ocu-windows`) implement
//! [`Platform`] and [`Session`]; the [`Service`] tracks sessions per client
//! and answers [`Request`]s, whichever front end they come from.

mod backend;
pub mod bidi;
pub mod image;
pub mod keys;
pub mod pages;
pub mod paths;
#[cfg(unix)]
pub mod portal;
mod service;
mod types;

pub use backend::{pick_window, Description, Observer, Platform, Session};
pub use service::{Client, Handler, Observe, Request, Response, Service};
pub use types::*;
