//! The bus half of AT-SPI. Toolkits export their trees to whoever owns
//! `org.a11y.atspi.Registry` on the accessibility bus. Each session gets a
//! private bus for that ([`ocu_core::portal::Bus`]), handed to its app as
//! `AT_SPI_BUS_ADDRESS`, and this process is the registry on it: it answers
//! each app's `Embed` and keeps the list. Reading a tree is then plain
//! method calls on the app's objects. Nothing else is on the bus, so
//! sessions never see each other's apps, and no desktop session or
//! `at-spi2-registryd` is needed.
//!
//! The registry and the calls run on a small shared async runtime, so an
//! app registering mid-walk is still answered. Calls are made a level of
//! the tree at a time, many at once, each with a timeout, and the walk
//! stops at its node budget or deadline: a busy or hung app slows a read
//! but never stalls the display or input, which never wait on it.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use futures_util::stream::{self, StreamExt as _};
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use ocu_core::{Action, MouseButton, TreeOptions, UiNode, WindowInfo};

use super::map::{self, Interfaces, Mapped, Raw, States};
use crate::launch::EnvChange;

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const NULL: &str = "/org/a11y/atspi/null";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const COMPONENT: &str = "org.a11y.atspi.Component";
const ACTION: &str = "org.a11y.atspi.Action";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// How long one call may take, and a whole read.
const CALL: Duration = Duration::from_secs(2);
const WALK: Duration = Duration::from_secs(10);
/// Elements read at once. The app answers them in turn on its main loop;
/// this only saves the round trips.
const CONCURRENT: usize = 48;
/// The longest text a field's value shows.
const MAX_TEXT: usize = 2000;

/// An object on the bus: its connection's name and its path.
type ObjRef = (String, OwnedObjectPath);

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("atspi")
            .enable_all()
            .build()
            .expect("an async runtime for AT-SPI")
    })
}

/// `ATSPI_COORD_TYPE_WINDOW`: frames within the toplevel. Wayland has no
/// screen positions, and GTK 4 reports wrong ones on X11 too.
const WINDOW_COORDS: u32 = 1;

/// Apps that have embedded themselves, newest last.
type Apps = Arc<Mutex<Vec<ObjRef>>>;

/// The registry's `Socket`, which apps call to join.
struct Socket {
    apps: Apps,
}

#[zbus::interface(name = "org.a11y.atspi.Socket")]
impl Socket {
    /// An app announces its root; the registry's root is its parent.
    async fn embed(
        &self,
        #[zbus(header)] header: Header<'_>,
        plug: ObjRef,
    ) -> ((String, OwnedObjectPath),) {
        let name = header
            .sender()
            .map(|s| s.to_string())
            .unwrap_or_else(|| plug.0.clone());
        log::info!("{name} registered its accessibility tree");
        let mut apps = self.apps.lock().unwrap();
        apps.retain(|a| a.0 != name);
        apps.push((name, plug.1));
        ((REGISTRY.into(), OwnedObjectPath::try_from(ROOT).unwrap()),)
    }

    async fn unembed(&self, plug: ObjRef) {
        self.apps.lock().unwrap().retain(|a| a.0 != plug.0);
    }
}

/// The registry's event list, which toolkits ask for so they only send
/// events someone wants. Nobody here listens to any.
struct Events;

#[zbus::interface(name = "org.a11y.atspi.Registry")]
impl Events {
    async fn get_registered_events(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// A session's accessibility: its bus, the registry on it, and the
/// elements of the last tree read.
pub struct A11y {
    bus: Arc<ocu_core::portal::Bus>,
    conn: Connection,
    apps: Apps,
    last: Option<Read>,
}

/// The last tree read, for acting on its elements by id.
struct Read {
    window: u64,
    /// Subtracted from frames to put them in the window's coordinates.
    origin: (f64, f64),
    refs: Vec<ObjRef>,
    raws: Vec<Raw>,
    elements: Vec<Mapped>,
}

/// Something a session does with its own input after an element action:
/// what AT-SPI could not do directly.
pub struct Fallback {
    pub window: u64,
    pub actions: Vec<Action>,
}

impl A11y {
    /// Starts the session's accessibility bus and becomes its registry.
    /// Needs `dbus-daemon`.
    pub fn start() -> Result<Self> {
        let bus = ocu_core::portal::Bus::start()?;
        let apps: Apps = Arc::default();
        let address = bus.address().to_string();
        let socket = Socket { apps: apps.clone() };
        let conn = runtime().block_on(async move {
            let conn = zbus::connection::Builder::address(address.as_str())?
                .serve_at(ROOT, socket)?
                .serve_at("/org/a11y/atspi/registry", Events)?
                .build()
                .await?;
            conn.request_name(REGISTRY).await?;
            zbus::Result::Ok(conn)
        });
        let conn = conn.map_err(|e| anyhow!("becoming the accessibility registry: {e}"))?;
        Ok(Self {
            bus,
            conn,
            apps,
            last: None,
        })
    }

    /// What the app needs in its environment to export its tree here.
    pub fn env(&self) -> Vec<EnvChange> {
        crate::launch::a11y_env(self.bus.address())
    }

    pub fn address(&self) -> &str {
        self.bus.address()
    }

    fn block_on<F: Future>(&self, f: F) -> F::Output {
        runtime().block_on(f)
    }

    /// Reads `window`'s tree. Ids stay good until the next read.
    pub fn tree(&mut self, window: &WindowInfo, opts: &TreeOptions) -> Result<UiNode> {
        // A freshly started app may not have registered yet.
        let deadline = Instant::now() + Duration::from_secs(5);
        let apps = loop {
            let apps = self.apps.lock().unwrap().clone();
            if !apps.is_empty() || Instant::now() > deadline {
                break apps;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if apps.is_empty() {
            bail!(
                "the app has not registered an accessibility tree: its toolkit may not support \
                 AT-SPI, or ignored AT_SPI_BUS_ADDRESS; use screenshots and coordinates"
            );
        }
        let conn = self.conn.clone();
        let (refs, raws) = self.block_on(async {
            let root = find_window(&conn, &apps, window).await?;
            anyhow::Ok(walk(&conn, root, opts).await)
        })?;
        // Less the toplevel's own position in them: a client-side shadow.
        let origin = raws[0]
            .extents
            .map_or((0.0, 0.0), |(x, y, _, _)| (x as f64, y as f64));
        let mut elements = Vec::new();
        let tree = map::build(&raws, 0, origin, &mut elements);
        self.last = Some(Read {
            window: window.id,
            origin,
            refs,
            raws,
            elements,
        });
        Ok(tree)
    }

    /// An element of the last read: its reference, what it was, and how.
    fn element(&self, id: &str) -> Result<(&Read, &ObjRef, &Raw, &Mapped)> {
        let read = self
            .last
            .as_ref()
            .ok_or_else(|| anyhow!("read the tree first (get_ui_tree)"))?;
        let n: usize = id
            .trim_start_matches('e')
            .parse()
            .map_err(|_| anyhow!("\"{id}\" is not an element id"))?;
        let mapped = read
            .elements
            .get(n)
            .ok_or_else(|| anyhow!("no element {id}; read the tree again"))?;
        Ok((
            read,
            &read.refs[mapped.index],
            &read.raws[mapped.index],
            mapped,
        ))
    }

    /// The window `id` is in, and its centre there, asked afresh so a
    /// scrolled element is found where it is now.
    pub fn center(&self, id: &str) -> Result<(u64, f64, f64)> {
        let (read, r, _, mapped) = self.element(id)?;
        let fresh = self
            .block_on(extents(&self.conn, r))
            .filter(|&(_, _, w, h)| w > 0 && h > 0)
            .map(|(x, y, w, h)| ocu_core::Rect {
                x: x as f64 - read.origin.0,
                y: y as f64 - read.origin.1,
                width: w as f64,
                height: h as f64,
            });
        let frame = fresh
            .or(mapped.frame)
            .ok_or_else(|| anyhow!("{id} has no position on screen"))?;
        let (x, y) = frame.center();
        Ok((read.window, x, y))
    }

    fn click(&self, id: &str) -> Result<Fallback> {
        let (window, x, y) = self.center(id)?;
        Ok(Fallback {
            window,
            actions: vec![Action::Click {
                x,
                y,
                button: MouseButton::Left,
                count: 1,
                modifiers: None,
            }],
        })
    }

    /// Does an element action, set_value or focus through AT-SPI, or says
    /// what input to use instead when the element can't take it that way.
    pub fn perform(&self, action: &Action) -> Result<Option<Fallback>> {
        match action {
            Action::ElementAction { element, name } => {
                let wanted = name.as_deref().unwrap_or("press");
                let (_, r, raw, _) = self.element(element)?;
                let known = map::find_action(&raw.actions, wanted);
                match (known, map::action(wanted).as_str()) {
                    (Some(i), _) => {
                        let done: bool =
                            self.block_on(call(&self.conn, r, ACTION, "DoAction", &(i as i32,)))?;
                        if !done {
                            bail!("{element} refused \"{wanted}\"");
                        }
                        Ok(None)
                    }
                    (None, "increment" | "decrement") if raw.interfaces.value => {
                        self.block_on(step(&self.conn, r, wanted.contains("incr")))?;
                        Ok(None)
                    }
                    (None, "focus") => self.focus(element),
                    // A press is a click, for elements that don't say.
                    (None, "press") => Ok(Some(self.click(element)?)),
                    (None, _) => {
                        let mut names: Vec<String> =
                            raw.actions.iter().map(|a| map::action(a)).collect();
                        if raw.interfaces.value {
                            names.extend(["increment".into(), "decrement".into()]);
                        }
                        bail!(
                            "{element} has no action \"{wanted}\" (it has: {})",
                            if names.is_empty() {
                                "none".into()
                            } else {
                                names.join(", ")
                            }
                        )
                    }
                }
            }
            Action::Focus { element } => self.focus(element),
            Action::SetValue { element, value } => {
                let (read, r, raw, mapped) = self.element(element)?;
                if raw.interfaces.editable_text {
                    let set: zbus::Result<bool> = self.block_on(call(
                        &self.conn,
                        r,
                        "org.a11y.atspi.EditableText",
                        "SetTextContents",
                        &(value.as_str(),),
                    ));
                    if set.unwrap_or(false) {
                        return Ok(None);
                    }
                } else if raw.interfaces.value {
                    let v: f64 = value
                        .trim()
                        .parse()
                        .map_err(|_| anyhow!("{element} takes a number"))?;
                    self.block_on(set_number(&self.conn, r, v))?;
                    return Ok(None);
                }
                if !map::takes_text(&mapped.role) {
                    bail!("{element} ({}) has no value to set", mapped.role);
                }
                // Typed in, as a person would: focused, emptied, filled.
                let mut then = match self.focus(element)? {
                    Some(f) => f.actions,
                    None => Vec::new(),
                };
                then.push(Action::PressKey {
                    keys: "ctrl+a".into(),
                });
                then.push(Action::TypeText {
                    text: value.clone(),
                });
                Ok(Some(Fallback {
                    window: read.window,
                    actions: then,
                }))
            }
            _ => bail!("not an element action"),
        }
    }

    /// Gives `element` keyboard focus, by clicking it when AT-SPI can't.
    fn focus(&self, element: &str) -> Result<Option<Fallback>> {
        let (_, r, raw, _) = self.element(element)?;
        if raw.interfaces.component {
            let grabbed: zbus::Result<bool> =
                self.block_on(call(&self.conn, r, COMPONENT, "GrabFocus", &()));
            if grabbed.unwrap_or(false) {
                return Ok(None);
            }
        }
        Ok(Some(self.click(element)?))
    }
}

impl Drop for A11y {
    /// Leaves the bus before ending its daemon, which zbus would otherwise
    /// log as an error.
    fn drop(&mut self) {
        let conn = self.conn.clone();
        let bus = self.bus.clone();
        runtime().spawn(async move {
            let _ = tokio::time::timeout(CALL, conn.close()).await;
            drop(bus);
        });
    }
}

/// Calls `member` on `r` and reads its reply, within [`CALL`].
async fn call<B, R>(
    conn: &Connection,
    r: &ObjRef,
    iface: &str,
    member: &str,
    body: &B,
) -> zbus::Result<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = tokio::time::timeout(
        CALL,
        conn.call_method(Some(r.0.as_str()), r.1.as_str(), Some(iface), member, body),
    )
    .await
    .map_err(|_| zbus::Error::Failure(format!("{member} timed out")))??;
    reply.body().deserialize()
}

/// A property, through `org.freedesktop.DBus.Properties.Get`.
async fn property(conn: &Connection, r: &ObjRef, iface: &str, name: &str) -> Option<OwnedValue> {
    call::<_, OwnedValue>(conn, r, PROPERTIES, "Get", &(iface, name))
        .await
        .ok()
}

async fn string_property(conn: &Connection, r: &ObjRef, name: &str) -> String {
    property(conn, r, ACCESSIBLE, name)
        .await
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default()
}

async fn number_property(conn: &Connection, r: &ObjRef, name: &str) -> Option<f64> {
    let v = property(conn, r, "org.a11y.atspi.Value", name).await?;
    match &*v {
        Value::F64(n) => Some(*n),
        Value::I32(n) => Some(*n as f64),
        Value::U32(n) => Some(*n as f64),
        Value::I64(n) => Some(*n as f64),
        _ => None,
    }
}

async fn set_number(conn: &Connection, r: &ObjRef, v: f64) -> Result<()> {
    call::<_, ()>(
        conn,
        r,
        PROPERTIES,
        "Set",
        &("org.a11y.atspi.Value", "CurrentValue", Value::F64(v)),
    )
    .await
    .map_err(|e| anyhow!("setting the value: {e}"))
}

/// Nudges a slider or spin button by its step.
async fn step(conn: &Connection, r: &ObjRef, up: bool) -> Result<()> {
    let now = number_property(conn, r, "CurrentValue")
        .await
        .ok_or_else(|| anyhow!("the element has no current value"))?;
    let by = number_property(conn, r, "MinimumIncrement")
        .await
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0);
    set_number(conn, r, if up { now + by } else { now - by }).await
}

async fn extents(conn: &Connection, r: &ObjRef) -> Option<(i32, i32, i32, i32)> {
    call(conn, r, COMPONENT, "GetExtents", &(WINDOW_COORDS,))
        .await
        .ok()
}

async fn children(conn: &Connection, r: &ObjRef) -> Vec<ObjRef> {
    call::<_, Vec<ObjRef>>(conn, r, ACCESSIBLE, "GetChildren", &())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, path)| !name.is_empty() && path.as_str() != NULL)
        .collect()
}

/// The toplevel that is `window`: by its title, else the active one, else
/// the first shown.
async fn find_window(conn: &Connection, apps: &[ObjRef], window: &WindowInfo) -> Result<ObjRef> {
    let mut best: Option<(u32, ObjRef)> = None;
    for app in apps.iter().rev() {
        for top in children(conn, app).await {
            let (name, states) = tokio::join!(
                string_property(conn, &top, "Name"),
                call::<_, Vec<u32>>(conn, &top, ACCESSIBLE, "GetState", &()),
            );
            let Ok(states) = states else { continue };
            let states = States::from_words(&states);
            if states.hidden() {
                continue;
            }
            let title = window.title.trim();
            let score = if !title.is_empty() && name.trim() == title {
                4
            } else if !title.is_empty() && !name.is_empty() && title.contains(name.trim()) {
                3
            } else if states.has(map::state::ACTIVE) {
                2
            } else {
                1
            };
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, top));
            }
        }
    }
    best.map(|(_, r)| r).ok_or_else(|| {
        anyhow!("the app has no window in its accessibility tree (yet); try again in a moment")
    })
}

/// Reads one element: what it is, and the references of its children.
/// `None` for one that is gone, hidden, or does not answer.
async fn read(conn: &Connection, r: &ObjRef) -> Option<(Raw, Vec<ObjRef>)> {
    let (role, states, interfaces, name, description, kids) = tokio::join!(
        call::<_, String>(conn, r, ACCESSIBLE, "GetRoleName", &()),
        call::<_, Vec<u32>>(conn, r, ACCESSIBLE, "GetState", &()),
        call::<_, Vec<String>>(conn, r, ACCESSIBLE, "GetInterfaces", &()),
        string_property(conn, r, "Name"),
        string_property(conn, r, "Description"),
        children(conn, r),
    );
    let role = role.ok()?;
    let states = States::from_words(&states.unwrap_or_default());
    if states.hidden() {
        return None;
    }
    let interfaces = Interfaces::parse(&interfaces.unwrap_or_default());
    // A field's text is its value; a document's text is everything in it,
    // and a password's is not for showing.
    let wants_text = interfaces.text
        && role != "password text"
        && (states.has(map::state::EDITABLE)
            || matches!(
                role.as_str(),
                "entry" | "text box" | "spin button" | "editbar"
            )
            || (name.is_empty() && matches!(role.as_str(), "label" | "static" | "text")));
    let (extents, actions, text, value) = tokio::join!(
        async {
            if interfaces.component {
                extents(conn, r).await
            } else {
                None
            }
        },
        async {
            if interfaces.action {
                let listed =
                    call::<_, Vec<(String, String, String)>>(conn, r, ACTION, "GetActions", &())
                        .await
                        .unwrap_or_default();
                // Chromium leaves the names out of the list, but gives them
                // one at a time.
                let mut names = Vec::with_capacity(listed.len());
                for (i, (name, _, _)) in listed.into_iter().enumerate() {
                    names.push(if name.is_empty() {
                        call::<_, String>(conn, r, ACTION, "GetName", &(i as i32,))
                            .await
                            .unwrap_or_default()
                    } else {
                        name
                    });
                }
                names
            } else {
                Vec::new()
            }
        },
        async {
            if wants_text {
                call::<_, String>(conn, r, "org.a11y.atspi.Text", "GetText", &(0i32, -1i32))
                    .await
                    .ok()
                    .map(|mut t| {
                        if t.len() > MAX_TEXT {
                            let mut cut = MAX_TEXT;
                            while !t.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            t.truncate(cut);
                        }
                        t
                    })
            } else {
                None
            }
        },
        async {
            if interfaces.value && !interfaces.editable_text {
                number_property(conn, r, "CurrentValue").await
            } else {
                None
            }
        },
    );
    Some((
        Raw {
            role,
            name,
            description,
            states,
            interfaces,
            extents,
            actions,
            text,
            value,
            children: Vec::new(),
        },
        kids,
    ))
}

/// Walks the tree under `root` a level at a time, within the node budget,
/// the depth limit and [`WALK`]. The root is element 0.
async fn walk(conn: &Connection, root: ObjRef, opts: &TreeOptions) -> (Vec<ObjRef>, Vec<Raw>) {
    let deadline = Instant::now() + WALK;
    let mut refs: Vec<ObjRef> = Vec::new();
    let mut raws: Vec<Raw> = Vec::new();
    // Toolkits have been known to list an element under itself.
    let mut seen: HashSet<ObjRef> = HashSet::new();
    // (reference, parent index)
    let mut level: Vec<(ObjRef, Option<usize>)> = vec![(root, None)];
    let mut depth = 0;
    while !level.is_empty() {
        let room = opts.max_nodes.max(1).saturating_sub(raws.len());
        level.truncate(room);
        level.retain(|(r, _)| seen.insert(r.clone()));
        let read: Vec<_> = stream::iter(level)
            .map(|(r, parent)| async move {
                let got = read(conn, &r).await;
                (r, parent, got)
            })
            .buffered(CONCURRENT)
            .collect()
            .await;
        let mut next = Vec::new();
        for (r, parent, got) in read {
            let Some((raw, kids)) = got else { continue };
            let i = raws.len();
            if let Some(p) = parent {
                raws[p].children.push(i);
            }
            refs.push(r);
            raws.push(raw);
            if depth < opts.max_depth {
                next.extend(kids.into_iter().map(|k| (k, Some(i))));
            }
        }
        if raws.is_empty() || Instant::now() > deadline {
            break;
        }
        level = next;
        depth += 1;
    }
    if raws.is_empty() {
        // The window answered nothing: show it as an empty window rather
        // than fail, so the caller still gets the picture.
        raws.push(Raw {
            role: "frame".into(),
            ..Default::default()
        });
        refs.push((String::new(), OwnedObjectPath::try_from(NULL).unwrap()));
    }
    (refs, raws)
}
