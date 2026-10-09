//! The macOS backend. Apps are opened without activating, captured with
//! ScreenCaptureKit, read and pressed through Accessibility, and sent input
//! with per-process events, so the user keeps their cursor and focus.
//!
//! Accessibility and Screen Recording are granted to the process that
//! calls these APIs, which is why the backend lives in the agent app rather
//! than in whatever process the MCP client spawned.
#![cfg(target_os = "macos")]

mod ax;
mod bidi;
mod capture;
mod cdp;
mod hook;
mod input;
pub mod lock;
mod pages;
mod panel;
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
                optional: false,
            },
            Permission {
                name: "Screen Recording".into(),
                granted: CGPreflightScreenCaptureAccess(),
                help: "Captures windows for screenshots. System Settings → Privacy & Security → Screen & System Audio Recording.".into(),
                optional: false,
            },
            Permission {
                name: "App Management".into(),
                granted: app_management_granted(),
                help: "Hands file pickers straight to the agent in the few apps that allow it. System Settings → Privacy & Security → App Management.".into(),
                optional: true,
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

/// Whether the user turned on App Management for this app, which is what
/// lets sessions load the panel hook into the apps they start
/// ([`hook`]). Read through TCC's preflight, which never prompts: the
/// permission is optional, and asking for it unprompted, or loading the
/// hook without it, would look like something trying to tamper with apps.
pub fn app_management_granted() -> bool {
    type Preflight = unsafe extern "C" fn(*const std::ffi::c_void, *const std::ffi::c_void) -> i32;
    static PREFLIGHT: std::sync::OnceLock<Option<Preflight>> = std::sync::OnceLock::new();
    let Some(preflight) = *PREFLIGHT.get_or_init(|| unsafe {
        let tcc = libc::dlopen(
            c"/System/Library/PrivateFrameworks/TCC.framework/TCC".as_ptr(),
            libc::RTLD_LAZY,
        );
        if tcc.is_null() {
            return None;
        }
        let f = libc::dlsym(tcc, c"TCCAccessPreflight".as_ptr());
        (!f.is_null()).then(|| std::mem::transmute::<*mut std::ffi::c_void, Preflight>(f))
    }) else {
        return false;
    };
    let service = CFString::from_static_str("kTCCServiceSystemPolicyAppBundles");
    // 0 granted, 1 denied, 2 never decided.
    unsafe { preflight((&*service as *const CFString).cast(), std::ptr::null()) == 0 }
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
    } else if permission.starts_with("App Management") {
        "Privacy_AppBundles"
    } else {
        "Privacy_Accessibility"
    };
    let _ = std::process::Command::new("open")
        .arg(format!(
            "x-apple.systempreferences:com.apple.preference.security?{anchor}"
        ))
        .status();
}
