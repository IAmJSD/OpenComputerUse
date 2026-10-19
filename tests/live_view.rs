//! End to end: a person watching a Windows or macOS session live. The view
//! streams the window's pictures (ScreenCaptureKit's `SCStream`,
//! Windows.Graphics.Capture), so it says when the picture changed, sends
//! nothing while it hasn't, and never waits on the session's lock, nor makes
//! the agent wait on it.
//!
//! It needs a desktop and the test app from `tests/e2e/*/swatch*`, a window
//! that swaps between red and blue when clicked, so it only runs when
//! `OCU_E2E_SWATCH_APP` names it; `.github/workflows/e2e.yml` sets it up.
//!
//! No libtest harness (`harness = false`): AppKit wants the main thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use ocu_core::image::Order;
use ocu_core::{
    Action, Frame, Handler as _, LaunchSpec, MouseButton, Observe, Platform, Request, Response,
    Service, Viewer, WindowInfo,
};

fn main() {
    let Some(app) = std::env::var_os("OCU_E2E_SWATCH_APP") else {
        eprintln!("live_view: OCU_E2E_SWATCH_APP is not set; skipping");
        return;
    };
    let Some(platform) = platform() else {
        eprintln!("live_view: only Windows and macOS sessions are watched through a capturer");
        return;
    };
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();
    // Every wait below has its own limit, so this long is a call that never
    // returns, which CI should name rather than time out on.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(180));
        println!("FAILED  stuck for three minutes; giving up");
        std::process::exit(1);
    });
    if let Err(e) = run(platform, &app.to_string_lossy()) {
        println!("FAILED  {e:#}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn platform() -> Option<Arc<dyn Platform>> {
    Some(Arc::new(ocu_macos::MacPlatform))
}

#[cfg(windows)]
fn platform() -> Option<Arc<dyn Platform>> {
    Some(Arc::new(ocu_windows::WindowsPlatform::with_hook(|| false)))
}

/// Linux sessions have a display of their own to watch, tested by
/// `scripts/linux/wayland_smoke.py`.
#[cfg(target_os = "linux")]
fn platform() -> Option<Arc<dyn Platform>> {
    None
}

fn ok(what: &str) {
    println!("ok      {what}");
}

fn run(platform: Arc<dyn Platform>, app: &str) -> Result<()> {
    let service = Service::new(platform, None);
    // A second client acts on the session while the first watches it.
    service.share_sessions();
    let mut agent = service.client();
    let reply = agent.handle(Request::StartSession(LaunchSpec {
        app: app.into(),
        ..Default::default()
    }))?;
    let Response::Session { info, .. } = reply else {
        bail!("unexpected reply to start_session: {reply:?}");
    };
    let session = info.id.clone();
    let viewer = service.viewer(&session)?;
    let result = watch(&service, &mut agent, &session, &*viewer);
    let _ = agent.handle(Request::EndSession {
        session: session.clone(),
    });
    result?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while viewer.is_alive() {
        ensure!(
            Instant::now() < deadline,
            "the view still thinks the session is alive after it ended"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    ok("the view sees the session end");
    Ok(())
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Colour {
    Red,
    Blue,
    Other([u8; 3]),
}

/// The colour in the middle of a picture.
fn middle(frame: &Frame) -> Colour {
    let (x, y) = (frame.width as usize / 2, frame.height as usize / 2);
    let at = y * frame.stride + x * 4;
    let px = &frame.data[at..at + 4];
    let [r, g, b] = match frame.order {
        Order::Rgba => [px[0], px[1], px[2]],
        Order::Bgra => [px[2], px[1], px[0]],
    };
    match (r, g, b) {
        (r, g, b) if r > 180 && g < 80 && b < 80 => Colour::Red,
        (r, g, b) if b > 180 && r < 80 && g < 80 => Colour::Blue,
        _ => Colour::Other([r, g, b]),
    }
}

/// Waits up to `limit` for `f` to give something.
fn within<T>(limit: Duration, mut f: impl FnMut() -> Result<Option<T>>) -> Result<Option<T>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = f()? {
            return Ok(Some(v));
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn watch(
    service: &Arc<Service>,
    agent: &mut ocu_core::Client,
    session: &str,
    viewer: &dyn Viewer,
) -> Result<()> {
    let first = viewer
        .generation()
        .context("the view can't tell when the picture changes, so it polls")?;
    ok("the view says when the picture changes");

    let window: WindowInfo = within(Duration::from_secs(10), || {
        Ok(viewer.windows()?.into_iter().next())
    })?
    .context("the app showed no window")?;

    // Streamed pictures come as the capture's BGRA; screenshots, which the
    // view falls back to, as decoded PNGs' RGBA.
    let mut last = None;
    let frame = within(Duration::from_secs(10), || {
        let frame = viewer.frame(None)?;
        let red = middle(&frame) == Colour::Red;
        last = Some(frame.clone());
        Ok((frame.order == Order::Bgra && red).then_some(frame))
    })?
    .ok_or_else(|| {
        anyhow!(
            "no streamed picture of the red window; the last was {:?} in the middle, {:?}",
            last.as_ref().map(middle),
            last.as_ref().map(|f| f.order)
        )
    })?;
    ensure!(
        frame.window_id == window.id,
        "a picture of window {} for window {}",
        frame.window_id,
        window.id
    );
    let (w, h) = (window.frame.width.round(), window.frame.height.round());
    ensure!(
        (frame.width as f64 - w).abs() <= 1.0 && (frame.height as f64 - h).abs() <= 1.0,
        "a {}x{} picture of a {w}x{h} window, so clicks would land off",
        frame.width,
        frame.height
    );
    ok(&format!(
        "streamed pictures are the window's, {}x{} (generation {first} to {})",
        frame.width,
        frame.height,
        viewer.generation().unwrap_or_default()
    ));

    // A still window sends nothing, where polling would tick ten times.
    std::thread::sleep(Duration::from_millis(500));
    let before = viewer.generation().unwrap_or_default();
    std::thread::sleep(Duration::from_secs(1));
    let after = viewer.generation().unwrap_or_default();
    ensure!(
        after.wrapping_sub(before) <= 2,
        "the generation went from {before} to {after} in a second of a still window"
    );
    ok(&format!(
        "a still window brings no new pictures ({} in a second)",
        after - before
    ));

    // The agent's click shows in the view as soon as it is drawn.
    let clicked = Instant::now();
    agent.handle(Request::Perform {
        session: session.into(),
        window: Some(window.id),
        action: Action::Click {
            x: window.frame.width / 2.0,
            y: window.frame.height / 2.0,
            button: MouseButton::Left,
            count: 1,
            modifiers: None,
        },
        observe: Observe::nothing(),
    })?;
    let acted = clicked.elapsed();
    within(Duration::from_secs(3), || {
        if viewer.generation() == Some(after) {
            return Ok(None);
        }
        Ok((middle(&viewer.frame(None)?) == Colour::Blue).then_some(()))
    })?
    .context("the click turned the window blue, but no picture of it came")?;
    ok(&format!(
        "a click shows {:?} after it was sent ({acted:?} of that the click itself)",
        clicked.elapsed()
    ));

    // An action holds the session for three seconds; the view carries on.
    let waiting = {
        let service = service.clone();
        let session = session.to_string();
        std::thread::spawn(move || {
            service.client().handle(Request::Perform {
                session,
                window: None,
                action: Action::Wait { ms: 3000 },
                observe: Observe::nothing(),
            })
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let windows = viewer.windows()?;
    let alive = viewer.is_alive();
    let generation = viewer.generation();
    let frame = viewer.frame(None)?;
    let took = started.elapsed();
    ensure!(
        took < Duration::from_secs(1),
        "the view waited {took:?} on the agent's action"
    );
    ensure!(
        alive && !windows.is_empty() && generation.is_some() && middle(&frame) == Colour::Blue,
        "mid-action the view saw alive {alive}, {} windows, generation {generation:?}, {:?}",
        windows.len(),
        middle(&frame)
    );
    waiting
        .join()
        .map_err(|_| anyhow!("the waiting action panicked"))??;
    ok(&format!("the view does not wait for the agent ({took:?})"));

    // And the agent does not wait for the view: the same requests, alone
    // and then with a watcher asking as fast as the view does.
    let timed = |agent: &mut ocu_core::Client| -> Result<Duration> {
        let started = Instant::now();
        for _ in 0..5 {
            agent.handle(Request::ListWindows {
                session: session.into(),
            })?;
            agent.handle(Request::Screenshot {
                session: session.into(),
                window: None,
            })?;
        }
        Ok(started.elapsed())
    };
    let alone = timed(agent)?;
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let viewer = service.viewer(session)?;
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut listed = Instant::now();
            let mut frames = 0u32;
            while !stop.load(Ordering::Relaxed) {
                if listed.elapsed() > Duration::from_millis(500) {
                    let _ = viewer.windows();
                    listed = Instant::now();
                }
                let _ = viewer.is_alive();
                let _ = viewer.generation();
                if viewer.frame(None).is_ok() {
                    frames += 1;
                }
                std::thread::sleep(Duration::from_millis(33));
            }
            frames
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    let watched = timed(agent)?;
    stop.store(true, Ordering::Relaxed);
    let frames = watcher
        .join()
        .map_err(|_| anyhow!("the watcher panicked"))?;
    ensure!(
        watched < alone * 2 + Duration::from_millis(500),
        "the agent's requests took {watched:?} watched, against {alone:?} alone"
    );
    ok(&format!(
        "the agent does not wait for the view ({watched:?} watched, {alone:?} alone, {frames} pictures)"
    ));
    Ok(())
}
