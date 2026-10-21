//! Screen sessions on an X11 desktop, end to end: a private Xvfb stands in
//! for the user's display, and each case checks what the session leaves on
//! it, read back from the X server itself.
//!
//! - Keys and buttons held with `key_down` and `mouse_down` are let go when
//!   the session ends, so nothing stays pressed on the user's machine.
//! - Typing a character the layout lacks leaves the keyboard map as it was.
//! - Two sessions share the desktop's accessibility switch: it goes back off
//!   only with the last one (needs `at-spi-bus-launcher` and `dbus-daemon`).
//! - The focused window's tree is placed on the screen without a window
//!   manager to say which window is in front, and its elements can be set,
//!   pressed and clicked (also needs Chrome or Chromium:
//!   `OCU_SMOKE_BROWSER`, else one on `PATH`).
//!
//! Each case skips itself, saying so, when what it needs is missing.
//! `DISPLAY` and the session bus are process-wide, so the cases run one
//! after another (`harness = false`).

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("screen: X11 screen sessions are Linux's; skipping");
}

#[cfg(target_os = "linux")]
fn main() {
    let cases: [(&str, Case); 4] = [
        (
            "held keys and buttons are let go at the end",
            linux::held_input_is_let_go,
        ),
        (
            "the keyboard map is left as it was",
            linux::keymap_is_restored,
        ),
        (
            "sessions share the accessibility switch",
            linux::accessibility_switch_is_shared,
        ),
        (
            "the focused window's tree is placed without a window manager",
            linux::tree_is_placed,
        ),
    ];
    let mut desktop = match Desktop::start() {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprintln!("screen: Xvfb is not installed; skipping");
            return;
        }
        Err(e) => {
            println!("FAILED  starting a desktop: {e:#}");
            std::process::exit(1);
        }
    };
    let mut failed = 0;
    for (name, case) in cases {
        match case(&mut desktop) {
            Ok(()) => println!("ok      {name}"),
            Err(e) => {
                failed += 1;
                println!("FAILED  {name}: {e:#}");
            }
        }
    }
    drop(desktop);
    if failed > 0 {
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
use linux::Desktop;

/// One check, on the desktop the cases share.
#[cfg(target_os = "linux")]
type Case = fn(&mut Desktop) -> anyhow::Result<()>;

#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::process::CommandExt as _;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, ensure, Context as _, Result};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, KeyButMask};
    use x11rb::rust_connection::RustConnection;

    use ocu_core::{Action, LaunchSpec, MouseButton, Platform as _, Session, TreeOptions};
    use ocu_linux::LinuxPlatform;

    const DISPLAY: &str = ":97";

    /// The stand-in for the user's desktop: an X server, and the processes
    /// started on it, ended when it is dropped.
    pub struct Desktop {
        children: Vec<Child>,
        conn: RustConnection,
    }

    impl Drop for Desktop {
        /// Ends each child's whole process group: the accessibility bus
        /// launcher's bus and Chrome's helpers with them.
        fn drop(&mut self) {
            for c in self.children.iter_mut().rev() {
                unsafe { libc::kill(-(c.id() as i32), libc::SIGKILL) };
                let _ = c.wait();
            }
        }
    }

    /// A command started in a process group of its own, so the desktop can
    /// end it with everything it started.
    fn grouped(program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.process_group(0);
        cmd
    }

    fn on_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
    }

    impl Desktop {
        pub fn start() -> Result<Option<Self>> {
            if !on_path("Xvfb") {
                return Ok(None);
            }
            let xvfb = grouped("Xvfb")
                .args([DISPLAY, "-screen", "0", "1280x800x24", "-nolisten", "tcp"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("starting Xvfb")?;
            let mut desktop = Self {
                children: vec![xvfb],
                conn: connect_retrying()?,
            };
            // The session finds the user's display, and is told it is X11.
            std::env::set_var("DISPLAY", DISPLAY);
            std::env::set_var("XDG_SESSION_TYPE", "x11");
            std::env::remove_var("WAYLAND_DISPLAY");
            // A session bus with the desktop's accessibility bus on it, where
            // the tools for one are here.
            if on_path("dbus-daemon") && launcher().is_some() {
                desktop.start_accessibility()?;
            }
            Ok(Some(desktop))
        }

        fn start_accessibility(&mut self) -> Result<()> {
            let mut bus = grouped("dbus-daemon")
                .args(["--session", "--nofork", "--print-address"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("starting a session bus")?;
            let mut address = String::new();
            std::io::BufRead::read_line(
                &mut std::io::BufReader::new(bus.stdout.take().unwrap()),
                &mut address,
            )?;
            self.children.push(bus);
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address.trim());
            std::env::remove_var("NO_AT_BRIDGE");
            let launcher = grouped(launcher().unwrap())
                .arg("--launch-immediately")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("starting at-spi-bus-launcher")?;
            self.children.push(launcher);
            let deadline = Instant::now() + Duration::from_secs(10);
            while a11y_enabled().is_none() {
                ensure!(
                    Instant::now() < deadline,
                    "the accessibility bus never came up"
                );
                sleep(Duration::from_millis(100));
            }
            Ok(())
        }

        fn has_accessibility(&self) -> bool {
            std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() && a11y_enabled().is_some()
        }

        fn spawn(&mut self, cmd: &mut Command) -> Result<()> {
            let child = cmd
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("starting {cmd:?}"))?;
            self.children.push(child);
            Ok(())
        }
    }

    fn connect_retrying() -> Result<RustConnection> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match x11rb::connect(Some(DISPLAY)) {
                Ok((conn, _)) => return Ok(conn),
                Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(100)),
                Err(e) => return Err(e).context("connecting to Xvfb"),
            }
        }
    }

    fn launcher() -> Option<PathBuf> {
        [
            "/usr/libexec/at-spi-bus-launcher",
            "/usr/lib/at-spi2-core/at-spi-bus-launcher",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    }

    /// `org.a11y.Status.IsEnabled` on the session bus, or `None` when there
    /// is no accessibility bus to ask.
    fn a11y_enabled() -> Option<bool> {
        let out = Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.a11y.Bus",
                "--print-reply",
                "/org/a11y/bus",
                "org.freedesktop.DBus.Properties.Get",
                "string:org.a11y.Status",
                "string:IsEnabled",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        match text.split_whitespace().last()? {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    fn set_a11y(on: bool) -> Result<()> {
        let status = Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.a11y.Bus",
                "--print-reply",
                "/org/a11y/bus",
                "org.freedesktop.DBus.Properties.Set",
                "string:org.a11y.Status",
                "string:IsEnabled",
                &format!("variant:boolean:{on}"),
            ])
            .stdout(Stdio::null())
            .status()?;
        ensure!(status.success(), "dbus-send failed");
        Ok(())
    }

    fn screen_session() -> Result<Box<dyn Session>> {
        LinuxPlatform::new().launch(&LaunchSpec {
            screen: true,
            ..Default::default()
        })
    }

    /// The keycodes whose keys are down on the server, and the pointer's
    /// held buttons.
    fn pressed(conn: &RustConnection) -> Result<(Vec<u8>, KeyButMask)> {
        let map = conn.query_keymap()?.reply()?.keys;
        let keys = (0..=255u8)
            .filter(|&k| map[k as usize / 8] & (1 << (k % 8)) != 0)
            .collect();
        let root = conn.setup().roots[0].root;
        let mask = conn.query_pointer(root)?.reply()?.mask;
        Ok((keys, mask))
    }

    /// What is pressed once `done` holds, or after a second: the session's
    /// input comes over its own connection, so the server may not have
    /// handled it when this one asks.
    fn settled(
        conn: &RustConnection,
        done: impl Fn(&(Vec<u8>, KeyButMask)) -> bool,
    ) -> Result<(Vec<u8>, KeyButMask)> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let state = pressed(conn)?;
            if done(&state) || Instant::now() > deadline {
                return Ok(state);
            }
            sleep(Duration::from_millis(20));
        }
    }

    /// The keycodes that make `sym` in the server's keyboard map.
    fn codes_for(conn: &RustConnection, sym: u32) -> Result<Vec<u8>> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = reply.keysyms_per_keycode as usize;
        Ok(reply
            .keysyms
            .chunks(per)
            .enumerate()
            .filter(|(_, row)| row.contains(&sym))
            .map(|(i, _)| min + i as u8)
            .collect())
    }

    pub fn held_input_is_let_go(d: &mut Desktop) -> Result<()> {
        let shift = codes_for(&d.conn, 0xffe1)?;
        let w = codes_for(&d.conn, 'w' as u32)?;
        let mut session = screen_session()?;
        session.perform(
            None,
            &Action::KeyDown {
                key: "shift".into(),
            },
        )?;
        session.perform(None, &Action::KeyDown { key: "w".into() })?;
        session.perform(
            None,
            &Action::MouseDown {
                x: 30.0,
                y: 40.0,
                button: MouseButton::Left,
            },
        )?;
        let down = |(keys, mask): &(Vec<u8>, KeyButMask)| {
            shift.iter().any(|k| keys.contains(k))
                && w.iter().any(|k| keys.contains(k))
                && mask.contains(KeyButMask::BUTTON1)
        };
        let state = settled(&d.conn, down)?;
        ensure!(
            down(&state),
            "shift, w and the left button are down while held (keys down: {:?}, buttons: {:?})",
            state.0,
            state.1
        );
        session.close();
        let up = |(keys, mask): &(Vec<u8>, KeyButMask)| {
            keys.is_empty() && !mask.contains(KeyButMask::BUTTON1)
        };
        let state = settled(&d.conn, up)?;
        ensure!(
            up(&state),
            "still down after the session ended: keys {:?}, buttons {:?}",
            state.0,
            state.1
        );
        Ok(())
    }

    pub fn keymap_is_restored(d: &mut Desktop) -> Result<()> {
        // é: not on the US keymap Xvfb starts with, so a spare key is lent.
        let eacute = 0xe9;
        ensure!(
            codes_for(&d.conn, eacute)?.is_empty(),
            "the keymap already has é"
        );
        let mut session = screen_session()?;
        session.perform(None, &Action::TypeText { text: "é".into() })?;
        ensure!(
            !codes_for(&d.conn, eacute)?.is_empty(),
            "typing é lent it a key while the session runs"
        );
        drop(session);
        let left = codes_for(&d.conn, eacute)?;
        ensure!(
            left.is_empty(),
            "é is still on keycodes {left:?} after the session ended"
        );
        Ok(())
    }

    pub fn accessibility_switch_is_shared(d: &mut Desktop) -> Result<()> {
        if !d.has_accessibility() {
            println!("skip    no at-spi-bus-launcher or dbus-daemon");
            return Ok(());
        }
        set_a11y(false)?;
        // Reading a tree joins the desktop's accessibility, whether or not
        // any app has one to read.
        let tree = |s: &mut Box<dyn Session>| {
            let _ = s.ui_tree(None, &TreeOptions::default());
        };
        let mut first = screen_session()?;
        tree(&mut first);
        ensure!(
            a11y_enabled() == Some(true),
            "the first session turned accessibility on"
        );
        let mut second = screen_session()?;
        tree(&mut second);
        drop(first);
        ensure!(
            a11y_enabled() == Some(true),
            "accessibility stays on while the second session reads trees"
        );
        drop(second);
        ensure!(
            a11y_enabled() == Some(false),
            "accessibility went back off with the last session"
        );
        Ok(())
    }

    fn browser() -> Option<String> {
        if let Ok(b) = std::env::var("OCU_SMOKE_BROWSER") {
            return Some(b);
        }
        ["chromium", "chromium-browser", "google-chrome"]
            .into_iter()
            .find(|b| on_path(b))
            .map(str::to_string)
    }

    /// An element's id and frame from a rendered tree line, by its role and
    /// name: `[e2] TextField "Query" @(240,245 185x21)`.
    fn find(tree: &str, role: &str, name: &str) -> Option<(String, Option<(f64, f64)>)> {
        let line = tree
            .lines()
            .find(|l| l.contains(&format!("] {role} \"{name}\"")))?;
        let id = line
            .trim()
            .split(']')
            .next()?
            .trim_start_matches('[')
            .to_string();
        let at = line.split("@(").nth(1).and_then(|f| {
            let mut xy = f.split_whitespace().next()?.split(',');
            Some((xy.next()?.parse().ok()?, xy.next()?.parse().ok()?))
        });
        Some((id, at))
    }

    pub fn tree_is_placed(d: &mut Desktop) -> Result<()> {
        if !d.has_accessibility() {
            println!("skip    no at-spi-bus-launcher or dbus-daemon");
            return Ok(());
        }
        let Some(browser) = browser() else {
            println!("skip    no Chrome or Chromium (set OCU_SMOKE_BROWSER)");
            return Ok(());
        };
        // Chromium exports its tree only when started with accessibility on.
        set_a11y(true)?;
        let profile = std::env::temp_dir().join(format!("ocu-screen-test-{}", std::process::id()));
        let page = "data:text/html,<title>Form</title><body style='margin:40px'>\
                    <input aria-label=Query id=q><button onclick=\"document.title=\
                    'pressed '+q.value\">Go</button></body>";
        let mut cmd = grouped(&browser);
        cmd.args([
            "--disable-gpu",
            "--force-renderer-accessibility",
            "--no-first-run",
            "--window-position=200,100",
            "--window-size=700,400",
            &format!("--user-data-dir={}", profile.display()),
            &format!("--app={page}"),
        ]);
        // Chrome's sandbox refuses to run as root, as in most containers.
        if unsafe { libc::geteuid() } == 0 {
            cmd.arg("--no-sandbox");
        }
        // The desktop owns it from here, so it ends with the test.
        d.spawn(&mut cmd)?;

        let mut session = screen_session()?;
        // No window manager here, so no _NET_ACTIVE_WINDOW: the window is
        // found by its title instead.
        let deadline = Instant::now() + Duration::from_secs(30);
        let (tree, field, go) = loop {
            let tree = session
                .ui_tree(None, &TreeOptions::default())
                .map(|t| t.render())
                .unwrap_or_default();
            if let (Some(field), Some(go)) = (
                find(&tree, "TextField", "Query"),
                find(&tree, "Button", "Go"),
            ) {
                break (tree, field, go);
            }
            if Instant::now() > deadline {
                bail!("the page's field and button never showed in the tree:\n{tree}");
            }
            sleep(Duration::from_millis(500));
        };
        let (fx, fy) = field.1.ok_or_else(|| {
            anyhow!("the field has no frame, so the window was not placed:\n{tree}")
        })?;
        // The page's 40px margin inside the window at (200, 100), below
        // whatever bars Chrome shows at its top.
        ensure!(
            (fx - 240.0).abs() <= 2.0 && (140.0..500.0).contains(&fy),
            "the field is at ({fx}, {fy}), not inside the window at (240, ≥140):\n{tree}"
        );

        session.perform(
            None,
            &Action::SetValue {
                element: field.0.clone(),
                value: "cats".into(),
            },
        )?;
        session.perform(
            None,
            &Action::ElementAction {
                element: go.0.clone(),
                name: None,
            },
        )?;
        title_becomes(&mut session, "pressed cats")?;

        // Ids last until the next read, so they are read again.
        let tree = session.ui_tree(None, &TreeOptions::default())?.render();
        let (field, go) = find(&tree, "TextField", "Query")
            .zip(find(&tree, "Button", "Go"))
            .ok_or_else(|| anyhow!("the field and button left the tree:\n{tree}"))?;
        // A real click where the tree says the field is, then typing.
        let (_, x, y) = session.element_point(None, &field.0)?;
        ensure!(
            (x - (fx + 92.0)).abs() < 100.0 && (y - fy).abs() < 30.0,
            "the field's centre ({x}, {y}) is not where its frame is"
        );
        session.perform(
            None,
            &Action::Click {
                x,
                y,
                button: MouseButton::Left,
                count: 3,
                modifiers: None,
            },
        )?;
        session.perform(
            None,
            &Action::TypeText {
                text: "dogs".into(),
            },
        )?;
        session.perform(
            None,
            &Action::ElementAction {
                element: go.0,
                name: None,
            },
        )?;
        title_becomes(&mut session, "pressed dogs")?;
        drop(session);
        let _ = std::fs::remove_dir_all(&profile);
        Ok(())
    }

    fn title_becomes(session: &mut Box<dyn Session>, title: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let tree = session
                .ui_tree(
                    None,
                    &TreeOptions {
                        max_depth: 1,
                        max_nodes: 5,
                    },
                )?
                .render();
            if tree.contains(&format!("\"{title}\"")) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("the page's title never became \"{title}\":\n{tree}");
            }
            sleep(Duration::from_millis(250));
        }
    }
}
