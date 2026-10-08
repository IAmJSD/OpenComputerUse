//! The macOS backend. Apps are opened without activating, captured with
//! ScreenCaptureKit, read and pressed through Accessibility, and sent input
//! with per-process events, so the user keeps their cursor and focus.
//!
//! Accessibility and Screen Recording are granted to the process that
//! calls these APIs, which is why the backend lives in the agent app rather
//! than in whatever process the MCP client spawned.
#![cfg(target_os = "macos")]

mod ax;
mod capture;
mod input;
pub mod lock;
mod session;
mod sky;

use anyhow::Result;
use objc2_application_services::AXIsProcessTrustedWithOptions;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

use ocu_core::{LaunchSpec, Permission, Platform, Session};

pub use capture::window as window_info;
pub use session::MacSession;

pub struct MacPlatform;

impl Platform for MacPlatform {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn permissions(&self) -> Vec<Permission> {
        vec![
            Permission {
                name: "Accessibility".into(),
                granted: accessibility_granted(),
                help: "Reads apps' interface elements and sends them input. System Settings → Privacy & Security → Accessibility.".into(),
            },
            Permission {
                name: "Screen Recording".into(),
                granted: CGPreflightScreenCaptureAccess(),
                help: "Captures windows for screenshots. System Settings → Privacy & Security → Screen & System Audio Recording.".into(),
            },
        ]
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn Session>> {
        if !accessibility_granted() {
            anyhow::bail!(
                "OpenComputerUse needs the Accessibility permission. Open the OpenComputerUse app to grant it, then try again."
            );
        }
        if spec.active_window {
            return Ok(Box::new(session::attach_active(spec)?));
        }
        Ok(Box::new(session::launch(spec)?))
    }

    fn unlock(&self) -> Result<bool> {
        lock::unlock(std::time::Duration::from_secs(20))
    }
}

pub fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrustedWithOptions(None) }
}

/// Shows the system prompts for whichever permissions are missing.
pub fn request_permissions() {
    if !accessibility_granted() {
        let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
        let dict =
            CFDictionary::<CFString, CFBoolean>::from_slices(&[&*key], &[CFBoolean::new(true)]);
        unsafe { AXIsProcessTrustedWithOptions(Some(dict.as_opaque())) };
    }
    if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess();
        // On recent macOS the call above can stay silent and leave the app
        // out of the Screen Recording list; asking ScreenCaptureKit for
        // content is what registers it (and shows the system prompt).
        std::thread::spawn(|| {
            let _ = capture::shareable_content();
        });
    }
}

/// Opens the System Settings pane for one permission.
pub fn open_settings(permission: &str) {
    let anchor = if permission.starts_with("Screen") {
        "Privacy_ScreenCapture"
    } else {
        "Privacy_Accessibility"
    };
    let _ = std::process::Command::new("open")
        .arg(format!(
            "x-apple.systempreferences:com.apple.preference.security?{anchor}"
        ))
        .status();
}
