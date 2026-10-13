//! How an app is started on a Wayland session: its environment and, for
//! Chromium-based browsers, the switches that put it on Wayland without a
//! GPU. Kept apart from the compositor so it builds and tests anywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::Path;

/// What to do to one environment variable.
#[derive(Debug, PartialEq, Eq)]
pub enum EnvChange {
    Set(&'static str, String),
    Remove(&'static str),
}

/// The environment for an app on the compositor listening at `socket`.
/// `runtime_dir` stands in for `XDG_RUNTIME_DIR` when this process has
/// none (common in containers); an inherited one is kept, so the app still
/// finds the user's session bus and sound server.
pub fn wayland_env(socket: &Path, runtime_dir: &Path, has_runtime_dir: bool) -> Vec<EnvChange> {
    use EnvChange::*;
    let mut env = vec![
        // An absolute WAYLAND_DISPLAY needs no runtime dir to resolve.
        Set("WAYLAND_DISPLAY", socket.display().to_string()),
        Remove("WAYLAND_SOCKET"),
        Remove("DISPLAY"),
        Remove("XAUTHORITY"),
        Set("XDG_SESSION_TYPE", "wayland".into()),
        // Toolkits that would pick X11, or give up once DISPLAY is gone.
        Set("GDK_BACKEND", "wayland".into()),
        Set("QT_QPA_PLATFORM", "wayland".into()),
        Set("SDL_VIDEODRIVER", "wayland".into()),
        Set("CLUTTER_BACKEND", "wayland".into()),
        Set("MOZ_ENABLE_WAYLAND", "1".into()),
        Set("ELECTRON_OZONE_PLATFORM_HINT", "wayland".into()),
        // Software rendering throughout: there is no GPU behind this
        // compositor, and often no Mesa either. GTK 4 picks a GL renderer
        // by default and fails outright without one.
        Set("GSK_RENDERER", "cairo".into()),
        Set("LIBGL_ALWAYS_SOFTWARE", "1".into()),
        Set("QT_QUICK_BACKEND", "software".into()),
        Set("WEBKIT_DISABLE_COMPOSITING_MODE", "1".into()),
        // Scaling is the compositor's to say (it says 1).
        Remove("GDK_SCALE"),
        Remove("GDK_DPI_SCALE"),
        Remove("QT_SCALE_FACTOR"),
    ];
    if !has_runtime_dir {
        env.push(Set("XDG_RUNTIME_DIR", runtime_dir.display().to_string()));
    }
    env
}

/// Whether `app` is a Chromium-based browser, which needs switches (not
/// just the environment) to run on Wayland.
pub fn is_chromium(app: &str) -> bool {
    let name = Path::new(app)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    [
        "chromium",
        "chrome",
        "google-chrome",
        "microsoft-edge",
        "msedge",
        "brave",
        "vivaldi",
    ]
    .iter()
    .any(|b| name == *b || name.starts_with(&format!("{b}-")) || name.starts_with(&format!("{b}.")))
}

/// Switches a Chromium-based browser gets before the session's own, unless
/// the session already gives them: Wayland through Ozone, software
/// compositing (there is no GPU), and no sandbox as root, where Chromium
/// otherwise refuses to start.
pub fn chromium_args(args: &[String], root: bool) -> Vec<String> {
    let has = |flag: &str| {
        args.iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    let mut out = Vec::new();
    for (flag, value) in [
        ("--ozone-platform", Some("wayland")),
        ("--disable-gpu", None),
        ("--disable-dev-shm-usage", None),
        ("--no-sandbox", None),
    ] {
        if flag == "--no-sandbox" && !root {
            continue;
        }
        if !has(flag) {
            out.push(match value {
                Some(v) => format!("{flag}={v}"),
                None => flag.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_is_recognised() {
        assert!(is_chromium("/usr/bin/chromium"));
        assert!(is_chromium("chromium-browser"));
        assert!(is_chromium("google-chrome-stable"));
        assert!(is_chromium("/opt/brave.com/brave/brave-browser"));
        assert!(!is_chromium("firefox"));
        assert!(!is_chromium("foot"));
    }

    #[test]
    fn chromium_switches_leave_given_ones_alone() {
        let none: Vec<String> = Vec::new();
        assert_eq!(
            chromium_args(&none, false),
            [
                "--ozone-platform=wayland",
                "--disable-gpu",
                "--disable-dev-shm-usage"
            ]
        );
        assert!(chromium_args(&none, true).contains(&"--no-sandbox".to_string()));
        let given = vec!["--ozone-platform=x11".to_string(), "--disable-gpu".into()];
        assert_eq!(chromium_args(&given, false), ["--disable-dev-shm-usage"]);
    }

    #[test]
    fn the_runtime_dir_is_kept_when_there_is_one() {
        let socket = Path::new("/tmp/ocu-wl/wayland-0");
        let kept = wayland_env(socket, Path::new("/tmp/ocu-wl"), true);
        assert!(kept.contains(&EnvChange::Set(
            "WAYLAND_DISPLAY",
            "/tmp/ocu-wl/wayland-0".into()
        )));
        assert!(kept.contains(&EnvChange::Remove("DISPLAY")));
        assert!(!kept
            .iter()
            .any(|c| matches!(c, EnvChange::Set("XDG_RUNTIME_DIR", _))));
        let made = wayland_env(socket, Path::new("/tmp/ocu-wl"), false);
        assert!(made.contains(&EnvChange::Set("XDG_RUNTIME_DIR", "/tmp/ocu-wl".into())));
    }
}
