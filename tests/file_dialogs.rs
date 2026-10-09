//! End to end: a real app opens a real file dialog and `choose_file`
//! answers it, through each platform's hook (the panel hook on macOS and
//! Windows, the portal on Linux) or without one.
//!
//! It needs a desktop and a test app from `tests/e2e`, so it only runs when
//! `OCU_E2E_APP` names one; `.github/workflows/e2e.yml` sets it up.
//! `OCU_E2E_HOOK=1` turns the hook on, and the test then insists the dialog
//! never showed, so a silent fallback cannot pass for the hook working.
//!
//! No libtest harness (`harness = false`): AppKit wants the main thread.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use ocu_core::{Action, LaunchSpec, Platform, Session};

#[derive(Clone, Copy)]
enum Answer {
    /// A file that exists, for an open dialog.
    Existing,
    /// A new file in a folder that exists, for a save dialog.
    New,
    /// No paths.
    Cancel,
}

struct Case {
    name: &'static str,
    mode: &'static str,
    answer: Answer,
}

const CASES: &[Case] = &[
    Case {
        name: "open",
        mode: "open",
        answer: Answer::Existing,
    },
    Case {
        name: "save",
        mode: "save",
        answer: Answer::New,
    },
    Case {
        name: "cancel",
        mode: "open",
        answer: Answer::Cancel,
    },
    #[cfg(windows)]
    Case {
        name: "legacy open",
        mode: "legacy-open",
        answer: Answer::Existing,
    },
];

fn main() {
    let Some(app) = std::env::var_os("OCU_E2E_APP") else {
        eprintln!("file_dialogs: OCU_E2E_APP is not set; skipping");
        return;
    };
    let app = app.to_string_lossy().into_owned();
    let hook = std::env::var("OCU_E2E_HOOK").is_ok_and(|v| v == "1");
    let platform = platform(hook);
    let mut failed = 0;
    for case in CASES {
        match run(platform.as_ref(), &app, hook, case) {
            Ok(()) => println!("ok      {} (hook {})", case.name, on(hook)),
            Err(e) => {
                println!("FAILED  {} (hook {}): {e:#}", case.name, on(hook));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

fn on(hook: bool) -> &'static str {
    if hook {
        "on"
    } else {
        "off"
    }
}

/// On macOS the hook follows `OCU_PANEL_HOOK` (and App Management), which
/// the workflow sets only with the hook on.
#[cfg(target_os = "macos")]
fn platform(_hook: bool) -> Box<dyn Platform> {
    Box::new(ocu_macos::MacPlatform)
}

#[cfg(windows)]
fn platform(hook: bool) -> Box<dyn Platform> {
    Box::new(ocu_windows::WindowsPlatform::with_hook(if hook {
        || true
    } else {
        || false
    }))
}

#[cfg(target_os = "linux")]
fn platform(hook: bool) -> Box<dyn Platform> {
    Box::new(ocu_linux::LinuxPlatform::with_portal(if hook {
        || true
    } else {
        || false
    }))
}

fn run(platform: &dyn Platform, app: &str, hook: bool, case: &Case) -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "ocu-e2e-{}-{}",
        std::process::id(),
        case.name.replace(' ', "-")
    ));
    std::fs::create_dir_all(&dir)?;
    let out = dir.join("result.txt");
    let (paths, expect) = match case.answer {
        Answer::Existing => {
            let f = dir.join("pick me.txt");
            std::fs::write(&f, "x")?;
            (vec![f.clone()], Some(f))
        }
        Answer::New => {
            let f = dir.join("saved.txt");
            (vec![f.clone()], Some(f))
        }
        Answer::Cancel => (vec![], None),
    };
    let mut session = platform.launch(&LaunchSpec {
        app: app.into(),
        args: vec![case.mode.into(), out.to_string_lossy().into_owned()],
        ..Default::default()
    })?;
    let result = answer(session.as_mut(), hook, &paths, &out, expect.as_deref());
    session.close();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn answer(
    session: &mut dyn Session,
    hook: bool,
    paths: &[PathBuf],
    out: &Path,
    expect: Option<&Path>,
) -> Result<()> {
    let choose = Action::ChooseFile {
        paths: paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    };
    // Without the portal, Linux leaves the dialog to the app and says so.
    if cfg!(target_os = "linux") && !hook {
        std::thread::sleep(Duration::from_secs(3));
        let err = session
            .perform(None, &choose)
            .err()
            .ok_or_else(|| anyhow!("choose_file answered with no portal to answer through"))?;
        ensure!(
            err.to_string().contains("no file chooser portal"),
            "unexpected error: {err:#}"
        );
        return Ok(());
    }

    let notice = wait(Duration::from_secs(30), || session.notice())
        .context("the app never asked for a file")?;
    let hidden = notice.contains("nothing shows") || notice.contains("nothing is shown");
    ensure!(
        hidden == hook,
        "the dialog was {} with the hook {}: {notice}",
        if hidden { "held back" } else { "shown" },
        on(hook)
    );
    session.perform(None, &choose)?;

    let got = wait(Duration::from_secs(20), || {
        std::fs::read_to_string(out).ok().filter(|s| !s.is_empty())
    })
    .context("the app never reported what it was given")?;
    match expect {
        Some(want) => ensure!(
            same_file(Path::new(got.trim()), want),
            "the app got {got:?}, not {}",
            want.display()
        ),
        None => ensure!(got == "CANCELLED", "the app got {got:?}, not a cancel"),
    }
    Ok(())
}

fn wait<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Whether two paths name one file, through symlinks (macOS's /var) and
/// short names (Windows' temp folder). The file itself may not exist yet.
fn same_file(a: &Path, b: &Path) -> bool {
    fn real(p: &Path) -> Result<PathBuf> {
        let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else {
            bail!("{} has no folder", p.display());
        };
        Ok(std::fs::canonicalize(dir)?.join(name))
    }
    match (real(a), real(b)) {
        (Ok(a), Ok(b)) if cfg!(windows) => {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        }
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
