//! The Linux backend. Every session gets a display server of its own, so
//! nothing the app does touches the user's desktop: a private Xvfb driven
//! through XTEST (`xvfb`), or a headless Wayland compositor running inside
//! this process (`wayland`).
//!
//! Both get the accessibility tree through AT-SPI (`atspi`): each session's
//! app exports it to a private bus on which this process is the registry.
//!
//! Everything started here dies with the MCP server: displays and apps get
//! `PR_SET_PDEATHSIG`, and an app that outlives its group loses its display.

mod atspi;
mod launch;

#[cfg(target_os = "linux")]
mod keys;
#[cfg(target_os = "linux")]
mod screen;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod xvfb;

#[cfg(target_os = "linux")]
pub use platform::LinuxPlatform;

#[cfg(target_os = "linux")]
mod platform {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Result};

    use ocu_core::{LaunchSpec, LinuxDisplay, Platform, Session};

    use crate::atspi::A11y;
    use crate::launch::EnvChange;

    pub struct LinuxPlatform {
        xvfb: Option<PathBuf>,
        /// Whether sessions get a private bus with a file chooser portal.
        /// Asked at each launch, so a changed setting applies to the next app.
        portal: fn() -> bool,
        /// The display a session gets when it doesn't ask for one.
        display: fn() -> LinuxDisplay,
    }

    impl LinuxPlatform {
        pub fn new() -> Self {
            Self::with_portal(|| false)
        }

        /// A platform whose sessions answer file choosers through a portal
        /// whenever `portal` says so, on X11 unless they ask otherwise.
        pub fn with_portal(portal: fn() -> bool) -> Self {
            Self::with_settings(portal, LinuxDisplay::default)
        }

        /// A platform whose sessions answer file choosers through a portal
        /// whenever `portal` says so, on `display` unless they ask otherwise.
        pub fn with_settings(portal: fn() -> bool, display: fn() -> LinuxDisplay) -> Self {
            Self {
                xvfb: find_xvfb(),
                portal,
                display,
            }
        }
    }

    impl Default for LinuxPlatform {
        fn default() -> Self {
            Self::new()
        }
    }

    fn find_xvfb() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("OCU_XVFB") {
            return Some(PathBuf::from(p));
        }
        let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let mut candidates = vec![
            exe_dir.join("Xvfb"),
            exe_dir.join("../lib/opencomputeruse/Xvfb"),
        ];
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path).map(|d| d.join("Xvfb")));
        }
        candidates.into_iter().find(|p| p.is_file())
    }

    impl Platform for LinuxPlatform {
        fn name(&self) -> &'static str {
            "linux-xvfb"
        }

        fn permissions(&self) -> Vec<ocu_core::Permission> {
            let wayland = (self.display)() == LinuxDisplay::Wayland;
            vec![
                ocu_core::Permission {
                    name: "Xvfb".into(),
                    granted: self.xvfb.is_some(),
                    help: "X11 sessions run on a private virtual X display. Install Xvfb (Debian/Ubuntu: apt install xvfb; Fedora: dnf install xorg-x11-server-Xvfb) or set OCU_XVFB.".into(),
                    // Wayland sessions need nothing installed.
                    optional: wayland,
                },
                ocu_core::Permission {
                    name: "Accessibility tree (AT-SPI)".into(),
                    granted: on_path("dbus-daemon"),
                    help: "Element ids, set_value and recipes read each app's tree over AT-SPI, on a \
                           private bus per session. Install dbus-daemon (Debian/Ubuntu: apt install \
                           dbus; Fedora: dnf install dbus-daemon). Without it, use screenshots and \
                           coordinates."
                        .into(),
                    optional: true,
                },
                ocu_core::Permission {
                    name: "File chooser portal".into(),
                    granted: (self.portal)(),
                    help: "Answers apps' open and save dialogs without showing them. Each app runs on \
                           a private session bus whose file chooser is the agent; everything else is \
                           forwarded to the user's own bus, so the keyring and notifications still work. \
                           Turn on with linux_file_portal in the config or OCU_LINUX_FILE_PORTAL=1."
                        .into(),
                    optional: true,
                },
            ]
        }

        fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn Session>> {
            if spec.screen {
                return Ok(Box::new(crate::screen::ScreenSession::start()?));
            }
            if spec.active_window {
                bail!("each Linux session has its own virtual display, with no window in front to attach to; start the app instead");
            }
            let portal = (self.portal)();
            match spec.linux_display.unwrap_or_else(self.display) {
                LinuxDisplay::Wayland => Ok(Box::new(crate::wayland::WaylandSession::start(
                    spec, portal,
                )?)),
                LinuxDisplay::X11 => {
                    let xvfb = self.xvfb.as_ref().ok_or_else(|| {
                        anyhow!(
                            "Xvfb is not installed; install it (apt install xvfb), set OCU_XVFB to its path, or use display \"wayland\""
                        )
                    })?;
                    Ok(Box::new(crate::xvfb::LinuxSession::start(
                        xvfb, spec, portal,
                    )?))
                }
            }
        }
    }

    /// Dies with the process that started it, even on SIGKILL.
    pub(crate) fn die_with_parent(cmd: &mut Command) {
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                // The parent may already be gone by the time the flag is set.
                if libc::getppid() == 1 {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }

    fn on_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
    }

    /// The session's accessibility bus and registry. Without `dbus-daemon`
    /// the session works without a tree.
    pub(crate) fn start_a11y() -> Option<A11y> {
        A11y::start()
            .inspect_err(|e| log::warn!("no accessibility tree: {e:#}"))
            .ok()
    }

    /// Has the app export its tree to the session's registry: the
    /// environment every toolkit needs, and a Chromium-based browser's
    /// switch. Comes before the session's own environment and arguments,
    /// which may undo it.
    pub(crate) fn use_a11y(
        cmd: &mut Command,
        a11y: &Option<A11y>,
        app: &str,
        args: &mut Vec<String>,
    ) {
        let Some(a11y) = a11y else { return };
        for change in a11y.env() {
            match change {
                EnvChange::Set(k, v) => cmd.env(k, v),
                EnvChange::Remove(k) => cmd.env_remove(k),
            };
        }
        if crate::launch::is_chromium(app) {
            args.splice(0..0, crate::launch::a11y_args(args));
        }
    }

    pub(crate) fn a11y_detail(a11y: &Option<A11y>) -> String {
        match a11y {
            Some(a) => format!("AT-SPI, on a private bus at {}", a.address()),
            None => "none (dbus-daemon is missing)".into(),
        }
    }

    pub(crate) fn a11y(a11y: &mut Option<A11y>) -> Result<&mut A11y> {
        a11y.as_mut().ok_or_else(|| {
            anyhow!("this session has no accessibility tree (dbus-daemon is missing); use screenshots and coordinates")
        })
    }

    pub(crate) fn random_bytes(n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub(crate) fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// A private directory for a session's sockets and files, readable by
    /// this user alone.
    pub(crate) fn private_dir(kind: &str) -> Result<PathBuf> {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = std::env::temp_dir().join(format!(
            "ocu-{kind}{}-{}",
            std::process::id(),
            hex(&random_bytes(4)?)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(dir)
    }

    pub(crate) type Portal = Option<(Arc<ocu_core::portal::Bus>, Arc<ocu_core::portal::Portal>)>;

    /// A private session bus with the file chooser portal on it. The portal
    /// forwards everything else to the user's bus, so the app keeps the
    /// keyring, notifications and the rest.
    pub(crate) fn start_portal() -> Portal {
        let bus = ocu_core::portal::Bus::start()
            .inspect_err(|e| log::warn!("no private session bus: {e}"))
            .ok()?;
        let upstream = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        let portal = ocu_core::portal::Portal::start(bus.address(), upstream)
            .inspect_err(|e| log::warn!("the file chooser portal is not answering: {e}"))
            .ok()?;
        Some((bus, portal))
    }

    /// Points the app at the session's portal, when it has one.
    pub(crate) fn use_portal(cmd: &mut Command, portal: &Portal) {
        if let Some((bus, _)) = portal {
            // Steers GTK (Firefox included) and Qt to ask the portal rather
            // than draw a dialog.
            cmd.env("DBUS_SESSION_BUS_ADDRESS", bus.address())
                .env("GTK_USE_PORTAL", "1")
                .env("QT_QPA_PLATFORMTHEME", "xdgdesktopportal");
        }
    }

    /// How the session's file dialogs are answered, for its details.
    pub(crate) fn portal_detail(portal: &Portal) -> String {
        match portal {
            Some((bus, _)) => format!(
                "answered through a portal on a private bus at {} (other services forwarded \
                 to the user's bus); nothing is shown",
                bus.address()
            ),
            None => "the app draws its own dialogs".into(),
        }
    }

    /// Answers the file chooser the app is waiting on with `paths`; none
    /// cancels it.
    pub(crate) fn choose_file(portal: &Portal, paths: &[String]) -> Result<()> {
        let paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| ocu_core::paths::absolute(p))
            .collect::<Result<_>>()?;
        let Some((_, portal)) = portal else {
            bail!("this session has no file chooser portal (it is off, or dbus-daemon is missing); drive the app's own dialog with clicks and keys")
        };
        // A menu item's chooser comes a moment after the click.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(waiting) = portal.waiting() {
                return portal.answer(&waiting.token, &paths);
            }
            if Instant::now() > deadline {
                bail!(
                    "the app is not asking for a file: do what asks for one first (an upload \
                     button, File → Open), or use clicks and keys"
                );
            }
            sleep(Duration::from_millis(100));
        }
    }

    pub(crate) fn portal_notice(portal: &Portal) -> Option<String> {
        let waiting = portal.as_ref()?.1.waiting()?;
        Some(format!(
            "The app is asking for {} through the file chooser portal (nothing is shown): answer \
             it with choose_file, giving the {}, or no paths to cancel.",
            match (waiting.save, waiting.folders, waiting.multiple) {
                (true, true, _) => "a folder to save into",
                (true, false, _) => "a place to save",
                (false, true, true) => "folders",
                (false, true, false) => "a folder",
                (false, false, true) => "files",
                (false, false, false) => "a file",
            },
            match (waiting.save, waiting.folders) {
                (true, true) => "folder",
                (true, false) => "path to save to",
                _ => "paths",
            },
        ))
    }

    /// Ends a session's app: its whole process group, politely and then not.
    pub(crate) fn end_app(app: &mut std::process::Child) {
        let pgid = app.id() as i32;
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && matches!(app.try_wait(), Ok(None)) {
            sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = app.wait();
    }

    /// The app's name for the session list: its executable's file name.
    pub(crate) fn app_name(app: &str) -> String {
        std::path::Path::new(app)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[cfg(target_os = "linux")]
pub(crate) use platform::*;
