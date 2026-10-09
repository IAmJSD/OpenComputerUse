//! The session registry behind every front end. The MCP server talks to it
//! directly on Linux and Windows; on macOS the agent app owns it and the MCP
//! server reaches it over a socket. Either way each client (connection)
//! owns the sessions it starts, and they end when it goes away.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::backend::{pick_window, Observer, Platform, Session};
use crate::types::*;

type SharedSession = Arc<Mutex<Box<dyn Session>>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Permissions,
    StartSession(LaunchSpec),
    EndSession {
        session: String,
    },
    ListSessions,
    ListWindows {
        session: String,
    },
    Screenshot {
        session: String,
        window: Option<u64>,
    },
    UiTree {
        session: String,
        window: Option<u64>,
        #[serde(default)]
        options: TreeOptions,
    },
    Perform {
        session: String,
        window: Option<u64>,
        action: Action,
        #[serde(default)]
        observe: Observe,
    },
    /// Unlock the Mac if it is locked, so sessions can be driven.
    Unlock,
    /// Simulators, emulators and phones, and the apps on them.
    Devices(DeviceQuery),
}

/// What to look at after an action.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observe {
    #[serde(default = "yes")]
    pub screenshot: bool,
    #[serde(default)]
    pub ui_tree: bool,
    /// How long to let the app react before looking.
    #[serde(default = "settle")]
    pub settle_ms: u64,
}

fn yes() -> bool {
    true
}
fn settle() -> u64 {
    350
}

impl Default for Observe {
    fn default() -> Self {
        Self {
            screenshot: true,
            ui_tree: false,
            settle_ms: settle(),
        }
    }
}

impl Observe {
    pub fn nothing() -> Self {
        Self {
            screenshot: false,
            ui_tree: false,
            settle_ms: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Permissions(Vec<Permission>),
    Session {
        info: SessionInfo,
        windows: Vec<WindowInfo>,
    },
    Sessions(Vec<SessionInfo>),
    Windows(Vec<WindowInfo>),
    Screenshot(Screenshot),
    UiTree(UiNode),
    Performed {
        screenshot: Option<Screenshot>,
        ui_tree: Option<UiNode>,
        /// Something the caller should know about the app's state, such as
        /// a file panel waiting for an answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<String>,
    },
    /// Whether the screen ended up unlocked.
    Unlocked(bool),
    /// The answer to a [`Request::Devices`].
    Devices(serde_json::Value),
}

/// Anything that answers requests: the in-process service, or the socket
/// client that forwards to the macOS agent.
pub trait Handler: Send {
    fn handle(&mut self, req: Request) -> Result<Response>;
}

struct Entry {
    info: SessionInfo,
    owner: u64,
    session: SharedSession,
    /// On this computer's screen, where the observer can draw over it; not
    /// for a phone or simulator.
    desktop: bool,
}

pub struct Service {
    platform: Arc<dyn Platform>,
    /// Phones, tablets, simulators and emulators: what starts a session
    /// whose [`LaunchSpec`] names a device.
    devices: Option<Arc<dyn Platform>>,
    /// The last picture of each (session, window), to notice an action that
    /// changed nothing on screen.
    last_shots: Mutex<HashMap<(String, u64), Vec<u8>>>,
    observer: Option<Arc<dyn Observer>>,
    sessions: Mutex<BTreeMap<String, Entry>>,
    next_client: AtomicU64,
    ids: RandomState,
    next_session: AtomicU64,
}

impl Service {
    pub fn new(platform: Arc<dyn Platform>, observer: Option<Arc<dyn Observer>>) -> Arc<Self> {
        Self::with_devices(platform, None, observer)
    }

    /// A service that can also start sessions on mobile devices.
    pub fn with_devices(
        platform: Arc<dyn Platform>,
        devices: Option<Arc<dyn Platform>>,
        observer: Option<Arc<dyn Observer>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            platform,
            devices,
            last_shots: Mutex::new(HashMap::new()),
            observer,
            sessions: Mutex::new(BTreeMap::new()),
            next_client: AtomicU64::new(1),
            ids: RandomState::new(),
            next_session: AtomicU64::new(1),
        })
    }

    pub fn platform(&self) -> &dyn Platform {
        &*self.platform
    }

    /// A new client. Its sessions end when the [`Client`] is dropped.
    pub fn client(self: &Arc<Self>) -> Client {
        Client {
            service: self.clone(),
            id: self.next_client.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|e| e.info.clone())
            .collect()
    }

    /// Ends a session whoever owns it (the UI's "End" button).
    pub fn end(&self, id: &str) -> Result<()> {
        let entry = self
            .sessions
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| anyhow!("no session {id}"))?;
        self.close(id, entry);
        Ok(())
    }

    pub fn end_all(&self) {
        let all = std::mem::take(&mut *self.sessions.lock().unwrap());
        for (id, entry) in all {
            self.close(&id, entry);
        }
    }

    fn close(&self, id: &str, entry: Entry) {
        log::info!("ending session {id} ({})", entry.info.app);
        self.last_shots.lock().unwrap().retain(|(s, _), _| s != id);
        entry.session.lock().unwrap().close();
        if let Some(o) = &self.observer {
            o.session_ended(id);
        }
    }

    fn new_id(&self) -> String {
        let n = self.next_session.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        // Unguessable, because knowing an id is enough to attach to it.
        let a = self.ids.hash_one((n, nanos));
        let b = self.ids.hash_one((a, n));
        format!("s{n}-{a:016x}{b:08x}").chars().take(28).collect()
    }

    fn session(&self, client: &Client, id: &str) -> Result<(SharedSession, bool)> {
        let sessions = self.sessions.lock().unwrap();
        let entry = sessions.get(id).ok_or_else(|| {
            anyhow!("no session {id}; it may have ended (list_sessions shows the live ones)")
        })?;
        if entry.owner != client.id {
            bail!("session {id} belongs to another client");
        }
        Ok((entry.session.clone(), entry.desktop))
    }

    fn handle(&self, client: &Client, req: Request) -> Result<Response> {
        match req {
            Request::Permissions => Ok(Response::Permissions(self.platform.permissions())),
            Request::Unlock => Ok(Response::Unlocked(self.platform.unlock()?)),
            Request::Devices(query) => match &self.devices {
                Some(d) => Ok(Response::Devices(d.query_devices(&query)?)),
                None => bail!("this server cannot drive mobile devices"),
            },
            Request::StartSession(spec) => {
                let platform = match (&spec.device, &self.devices) {
                    (None, _) => &self.platform,
                    (Some(_), Some(devices)) => devices,
                    (Some(_), None) => bail!("this server cannot drive mobile devices"),
                };
                let desktop = spec.device.is_none();
                let mut session = platform.launch(&spec)?;
                let d = session.describe();
                // Driving itself, it answers its own accessibility requests
                // off the main thread, which AppKit aborts on.
                if desktop && d.pid == Some(std::process::id()) {
                    bail!("{} can't drive itself", d.app);
                }
                let windows = session.windows().unwrap_or_default();
                let info = SessionInfo {
                    id: self.new_id(),
                    app: d.app,
                    pid: d.pid,
                    backend: d.backend.unwrap_or_else(|| platform.name().to_string()),
                    details: d.details,
                };
                log::info!(
                    "started session {} ({}, pid {:?})",
                    info.id,
                    info.app,
                    info.pid
                );
                self.sessions.lock().unwrap().insert(
                    info.id.clone(),
                    Entry {
                        info: info.clone(),
                        owner: client.id,
                        session: Arc::new(Mutex::new(session)),
                        desktop,
                    },
                );
                if let Some(o) = &self.observer {
                    o.session_started(&info);
                }
                Ok(Response::Session { info, windows })
            }
            Request::EndSession { session } => {
                self.session(client, &session)?;
                self.end(&session)?;
                Ok(Response::Ok)
            }
            Request::ListSessions => {
                let sessions = self.sessions.lock().unwrap();
                Ok(Response::Sessions(
                    sessions
                        .iter()
                        .filter(|(_, e)| e.owner == client.id)
                        .map(|(_, e)| e.info.clone())
                        .collect(),
                ))
            }
            Request::ListWindows { session } => {
                let (s, _) = self.session(client, &session)?;
                let windows = s.lock().unwrap().windows()?;
                Ok(Response::Windows(windows))
            }
            Request::Screenshot { session, window } => {
                let (s, _) = self.session(client, &session)?;
                let shot = s.lock().unwrap().screenshot(window)?;
                self.remember(&session, &shot);
                Ok(Response::Screenshot(shot))
            }
            Request::UiTree {
                session,
                window,
                options,
            } => {
                let (s, _) = self.session(client, &session)?;
                let tree = s.lock().unwrap().ui_tree(window, &options)?;
                Ok(Response::UiTree(tree))
            }
            Request::Perform {
                session,
                window,
                action,
                observe,
            } => {
                let (s, desktop) = self.session(client, &session)?;
                let mut s = s.lock().unwrap();
                let (window, action) = match action {
                    Action::ClickElement {
                        element,
                        button,
                        count,
                        modifiers,
                    } => {
                        let (window, x, y) = s.element_point(window, &element)?;
                        (
                            window,
                            Action::Click {
                                x,
                                y,
                                button,
                                count,
                                modifiers,
                            },
                        )
                    }
                    action => (window, action),
                };
                let target = s
                    .windows()
                    .ok()
                    .and_then(|ws| pick_window(&ws, window).ok().cloned());
                // A picture from before, to compare with the one after.
                if observe.screenshot {
                    if let Some(w) = &target {
                        if !self
                            .last_shots
                            .lock()
                            .unwrap()
                            .contains_key(&(session.clone(), w.id))
                        {
                            if let Ok(shot) = s.screenshot(Some(w.id)) {
                                self.remember(&session, &shot);
                            }
                        }
                    }
                }
                if let Action::Wait { ms } = action {
                    std::thread::sleep(Duration::from_millis(ms.min(60_000)));
                } else {
                    let observer = self.observer.as_ref().filter(|_| desktop);
                    if let (Some(o), Some(w)) = (observer, &target) {
                        match action.pointer() {
                            Some((x, y)) => {
                                o.pointer(&session, w, x, y, matches!(action, Action::Click { .. }))
                            }
                            None => o.acted(&session, w),
                        }
                    }
                    s.perform(window, &action)?;
                }
                if !s.is_alive() {
                    return Ok(Response::Performed {
                        screenshot: None,
                        ui_tree: None,
                        notice: None,
                    });
                }
                if observe.screenshot || observe.ui_tree {
                    std::thread::sleep(Duration::from_millis(observe.settle_ms.min(10_000)));
                }
                // Looking is best effort: the action happened either way.
                let mut screenshot = observe
                    .screenshot
                    .then(|| {
                        s.screenshot(window)
                            .map_err(|e| log::warn!("screenshot after action: {e:#}"))
                            .ok()
                    })
                    .flatten();
                // Pixel-identical after an action usually means a window that
                // stopped drawing while covered: look again, uncovered.
                if let Some(shot) = &screenshot {
                    let unchanged = shot.window_id.is_some_and(|id| {
                        self.last_shots.lock().unwrap().get(&(session.clone(), id))
                            == Some(&shot.png)
                    });
                    if unchanged {
                        log::info!(
                            "screenshot unchanged after {:?}; capturing it uncovered",
                            action
                        );
                        match s.screenshot_uncovered(window) {
                            Ok(fresh) => screenshot = Some(fresh),
                            Err(e) => log::warn!("uncovered screenshot: {e:#}"),
                        }
                    }
                }
                if let Some(shot) = &screenshot {
                    self.remember(&session, shot);
                }
                let ui_tree = observe
                    .ui_tree
                    .then(|| {
                        s.ui_tree(window, &TreeOptions::default())
                            .map_err(|e| log::warn!("tree after action: {e:#}"))
                            .ok()
                    })
                    .flatten();
                Ok(Response::Performed {
                    screenshot,
                    ui_tree,
                    notice: s.notice(),
                })
            }
        }
    }

    fn remember(&self, session: &str, shot: &Screenshot) {
        if let Some(id) = shot.window_id {
            self.last_shots
                .lock()
                .unwrap()
                .insert((session.to_string(), id), shot.png.clone());
        }
    }

    /// Drops sessions whose app has exited on its own.
    pub fn reap(&self) {
        let dead: Vec<String> = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .iter()
                .filter(|(_, e)| {
                    e.session
                        .try_lock()
                        .map(|mut s| !s.is_alive())
                        .unwrap_or(false)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in dead {
            let _ = self.end(&id);
        }
    }
}

/// One connection's view of the service.
pub struct Client {
    service: Arc<Service>,
    id: u64,
}

impl Client {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Handler for Client {
    fn handle(&mut self, req: Request) -> Result<Response> {
        let service = self.service.clone();
        service.handle(self, req)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let owned: Vec<(String, Entry)> = {
            let mut sessions = self.service.sessions.lock().unwrap();
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, e)| e.owner == self.id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| sessions.remove(&id).map(|e| (id, e)))
                .collect()
        };
        for (id, entry) in owned {
            self.service.close(&id, entry);
        }
    }
}
