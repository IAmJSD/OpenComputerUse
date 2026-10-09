//! The app's one window: whether computer use has the permissions it needs,
//! how to add it to an MCP client, which sessions are running, and the
//! settings for recipes and the overlay.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, rgb, App, AppContext as _, ClipboardItem, Context, ElementId, FocusHandle,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, PromptButton,
    PromptLevel, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
};

use ocu_core::{Permission, Service, SessionInfo};

use super::ui::{
    icon, palette, Badge, Button, Checkbox, Chip, Divider, Heading, LineEdit, LineEditKey, Modal,
    TextInput, TextInputColors, TextPress,
};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::clients::{self, Client};
use crate::config::{Config, Provider};
use crate::remote::{self, devices::Device, devices::Devices, skill};
use crate::update::{self, Installer, Progress, UpdateStatus};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    TypesafeKey,
    TypesafeModel,
    CloudflareAccount,
    CloudflareToken,
    CloudflareModel,
    MinConfidence,
    HttpPort,
    ListenOn,
    DeviceName,
    DeviceUrl,
}

impl Field {
    /// Every field, in tab order.
    const ALL: [Field; 10] = [
        Field::TypesafeKey,
        Field::TypesafeModel,
        Field::CloudflareAccount,
        Field::CloudflareToken,
        Field::CloudflareModel,
        Field::MinConfidence,
        Field::HttpPort,
        Field::ListenOn,
        Field::DeviceName,
        Field::DeviceUrl,
    ];

    /// The fields Save writes to the recipe settings.
    const RECIPE: [Field; 6] = [
        Field::TypesafeKey,
        Field::TypesafeModel,
        Field::CloudflareAccount,
        Field::CloudflareToken,
        Field::CloudflareModel,
        Field::MinConfidence,
    ];

    fn secret(self) -> bool {
        matches!(self, Field::TypesafeKey | Field::CloudflareToken)
    }

    fn id(self) -> &'static str {
        match self {
            Field::TypesafeKey => "typesafe-key",
            Field::TypesafeModel => "typesafe-model",
            Field::CloudflareAccount => "cf-account",
            Field::CloudflareToken => "cf-token",
            Field::CloudflareModel => "cf-model",
            Field::MinConfidence => "min-confidence",
            Field::HttpPort => "http-port",
            Field::ListenOn => "listen-on",
            Field::DeviceName => "device-name",
            Field::DeviceUrl => "device-url",
        }
    }
}

pub struct Status {
    service: Arc<Service>,
    focus: FocusHandle,
    permissions: Vec<Permission>,
    sessions: Vec<SessionInfo>,
    config: Config,
    fields: Vec<(Field, LineEdit)>,
    saved: bool,
    error: Option<String>,
    clients: [ClientState; Client::ALL.len()],
    busy: Option<Client>,
    client_message: Option<String>,
    updates: Updates,
    remote: Remote,
    /// "Work while the Mac is locked": whether the privileged pieces are in
    /// place, whether a change is running, and the last message.
    #[cfg(target_os = "macos")]
    lock_installed: bool,
    #[cfg(target_os = "macos")]
    lock_busy: bool,
    #[cfg(target_os = "macos")]
    lock_message: Option<String>,
}

/// The "Other devices" section's state.
#[derive(Default)]
struct Remote {
    devices: Vec<Device>,
    /// The Generate Skill form, or the skill it (or Regenerate Key) made.
    panel: Option<Panel>,
    /// A device whose Remove was pressed once and awaits the second press.
    confirm_remove: Option<String>,
    url_placeholder: String,
    error: Option<String>,
    /// A failure of the Generate Skill form, shown with it.
    skill_error: Option<String>,
    /// What the last Deploy button did: where the skill went, or why not.
    deploy_note: Option<String>,
    /// "Every network adapter to every host" is unticked, but nothing is
    /// chosen yet, so the settings still say every adapter.
    limiting: bool,
    /// The server's state as last drawn, to redraw when it changes.
    server_seen: (Option<String>, Option<String>),
}

enum Panel {
    Form,
    /// The key Generate Skill or Regenerate Key just made, in a modal.
    Issued {
        device: String,
        /// This computer's hosts-file entry, which holds the key.
        entry: String,
        /// The generic skill, the same for every computer.
        skill: String,
        /// Both inside a message asking the device's agent to set them up.
        prompt: String,
        tab: IssuedTab,
        note: Option<String>,
        /// The block whose text is being selected, and its selection.
        selecting: Option<(Block, LineEdit)>,
    },
}

/// The modal's blocks of text, which can be selected and copied.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Prompt,
    Entry,
    Skill,
}

impl Block {
    fn id(self) -> &'static str {
        match self {
            Block::Prompt => "issued-prompt",
            Block::Entry => "issued-entry",
            Block::Skill => "issued-skill",
        }
    }
}

/// How the issued key goes to the device.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IssuedTab {
    /// A prompt for the device's agent, which sets itself up.
    Prompt,
    /// The hosts-file entry and the skill, to put in place by hand.
    Skill,
}

/// The Updates section's state.
#[derive(Default)]
struct Updates {
    checking: bool,
    status: Option<UpdateStatus>,
    /// Bytes received of an update being downloaded, shared with the
    /// download thread.
    received: Arc<AtomicU64>,
    progress: Option<Progress>,
    error: Option<String>,
}

#[derive(Clone, PartialEq)]
struct ClientState {
    found: bool,
    registration: clients::Registration,
}

const GOOD: u32 = 0x2E7D4F;

/// Opens a web page with the system's handler.
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("/usr/bin/open");
    #[cfg(windows)]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "linux")]
    let mut cmd = std::process::Command::new("xdg-open");
    let _ = cmd.arg(url).spawn();
}

/// Shows a saved file in the file manager.
fn reveal(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(target_os = "linux")]
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// The .app this is running from, if it is.
#[cfg(target_os = "macos")]
fn bundle() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Into::into)
}

#[cfg(target_os = "macos")]
fn reveal_app() {
    if let Some(b) = bundle() {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(b)
            .spawn();
    }
}

/// Runs `install-lock` / `uninstall-lock` as root behind the macOS admin
/// prompt, passing the owner uid and the bundled plugin.
#[cfg(target_os = "macos")]
fn run_privileged_lock(
    enable: bool,
    exe: &std::path::Path,
    plugin: Option<&std::path::Path>,
    uid: u32,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, bail};
    // Values go in as argv and `quoted form of` shell-quotes them, so no path
    // can break out of the root command line.
    let mut cmd = std::process::Command::new("/usr/bin/osascript");
    cmd.args(["-e", "on run argv"]);
    if enable {
        let plugin = plugin.ok_or_else(|| anyhow!("the plugin is missing from the app bundle"))?;
        cmd.args([
            "-e",
            "do shell script \"OCU_OWNER_UID=\" & quoted form of item 1 of argv & \
             \" OCU_LOCK_PLUGIN=\" & quoted form of item 2 of argv & \
             \" \" & quoted form of item 3 of argv & \" install-lock\" \
             with administrator privileges",
            "-e",
            "end run",
        ]);
        cmd.arg(uid.to_string()).arg(plugin).arg(exe);
    } else {
        cmd.args([
            "-e",
            "do shell script quoted form of item 1 of argv & \" uninstall-lock\" \
             with administrator privileges",
            "-e",
            "end run",
        ]);
        cmd.arg(exe);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // -128 is the user cancelling the prompt.
        if err.contains("-128") {
            bail!("Cancelled.");
        }
        // osascript wraps the command's stderr as "7:217: execution error: ... (1)".
        let err = err.trim();
        let err = err.split_once("execution error: ").map_or(err, |(_, e)| e);
        let err = err.rsplit_once(" (").map_or(err, |(e, _)| e);
        bail!("{err}");
    }
    Ok(())
}

/// Quits and opens again, which is when macOS applies a new Screen
/// Recording grant. Sessions end with the quit.
#[cfg(target_os = "macos")]
fn reopen(cx: &mut App) {
    if let Some(b) = bundle() {
        let script = format!("sleep 1; /usr/bin/open {:?}", b.display().to_string());
        let _ = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .spawn();
    }
    cx.quit();
}

/// What a permission's row offers: its state, or a way to grant it. Off a
/// Mac nothing grants one from here; its help says what to install.
fn grant(i: usize, perm: &Permission) -> gpui::AnyElement {
    if perm.granted {
        return status(if perm.optional { "On" } else { "Granted" }, true).into_any_element();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = i;
        status("Missing", false).into_any_element()
    }
    #[cfg(target_os = "macos")]
    {
        let name = perm.name.clone();
        if perm.optional {
            // Only the Settings pane: nothing asks for it unless the user
            // goes there and turns it on.
            Button::new(("grant", i), "Turn On…")
                .flex_none()
                .on_click(move |_, _, _| ocu_macos::open_settings(&name))
                .into_any_element()
        } else {
            Button::new(("grant", i), "Grant…")
                .primary()
                .flex_none()
                .on_click(move |_, _, _| {
                    ocu_macos::request_permissions();
                    ocu_macos::open_settings(&name);
                })
                .into_any_element()
        }
    }
}

/// A status as a coloured dot and a word, which reads the same at any
/// width (a filled badge beside an outlined one did not).
fn status(label: &str, good: bool) -> gpui::Div {
    let p = palette();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .child(
            div()
                .size(px(7.0))
                .flex_none()
                .rounded_full()
                .bg(rgb(if good { 0x3FB26B } else { p.text_faint })),
        )
        .child(
            div()
                .whitespace_nowrap()
                .text_color(rgb(if good { p.text } else { p.text_dim }))
                .child(label.to_string()),
        )
}

fn client_states() -> [ClientState; Client::ALL.len()] {
    Client::ALL.map(|c| ClientState {
        found: clients::find(c).is_some(),
        registration: clients::registration(c),
    })
}

impl Status {
    pub fn new(service: Arc<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let config = Config::load();
        let r = &config.recipe;
        let fields = Field::ALL
            .iter()
            .map(|&f| {
                let text = match f {
                    Field::TypesafeKey => r.typesafe_api_key.clone(),
                    Field::TypesafeModel => r.typesafe_model.clone(),
                    Field::CloudflareAccount => r.cloudflare_account_id.clone(),
                    Field::CloudflareToken => r.cloudflare_api_token.clone(),
                    Field::CloudflareModel => r.cloudflare_model.clone(),
                    Field::MinConfidence => format!("{}", r.min_confidence),
                    Field::HttpPort => config.http.port.to_string(),
                    Field::ListenOn | Field::DeviceName | Field::DeviceUrl => String::new(),
                };
                let mut edit = LineEdit::default();
                edit.set_text(text);
                (f, edit)
            })
            .collect();
        // Permissions change in System Settings, behind our back.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if this.update(cx, |s, cx| s.refresh(cx)).is_err() {
                break;
            }
        })
        .detach();
        let mut s = Self {
            permissions: service.platform().permissions(),
            sessions: service.sessions(),
            service,
            focus,
            config,
            fields,
            saved: false,
            error: None,
            clients: client_states(),
            updates: Updates::default(),
            remote: Remote {
                devices: Devices::load().devices,
                ..Default::default()
            },
            busy: None,
            client_message: None,
            #[cfg(target_os = "macos")]
            lock_installed: ocu_macos::lock::installed(),
            #[cfg(target_os = "macos")]
            lock_busy: false,
            #[cfg(target_os = "macos")]
            lock_message: None,
        };
        s.saved = true;
        s
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let permissions = self.service.platform().permissions();
        let sessions = self.service.sessions();
        let http = super::http::state();
        let server_changed =
            (http.listening.clone(), http.error.clone()) != self.remote.server_seen;
        self.remote.server_seen = (http.listening, http.error);
        let devices = Devices::load().devices;
        let devices_changed = devices.iter().map(|d| (&d.id, &d.key_hash)).ne(self
            .remote
            .devices
            .iter()
            .map(|d| (&d.id, &d.key_hash)));
        self.remote.devices = devices;
        let clients = client_states();
        let changed = devices_changed
            || server_changed
            || permissions
                .iter()
                .map(|p| p.granted)
                .ne(self.permissions.iter().map(|p| p.granted))
            || sessions
                .iter()
                .map(|s| &s.id)
                .ne(self.sessions.iter().map(|s| &s.id))
            || clients != self.clients;
        self.permissions = permissions;
        self.sessions = sessions;
        self.clients = clients;
        if changed {
            cx.notify();
        }
    }

    fn set_http(&mut self, on: bool) {
        match remote::set_server(Some(on), None, None) {
            Ok(config) => {
                self.config.http = config.http;
                self.remote.error = None;
            }
            Err(e) => self.remote.error = Some(format!("{e:#}")),
        }
    }

    fn apply_port(&mut self) {
        match self.text(Field::HttpPort).parse::<u16>() {
            Ok(port) => match remote::set_server(None, Some(port), None) {
                Ok(config) => {
                    self.config.http = config.http;
                    self.remote.error = None;
                    self.field(Field::HttpPort).active = false;
                }
                Err(e) => self.remote.error = Some(format!("{e:#}")),
            },
            Err(_) => self.remote.error = Some("The port is a number, like 8642.".into()),
        }
    }

    fn set_listen_on(&mut self, listen_on: Vec<String>) {
        match remote::set_server(None, None, Some(&listen_on)) {
            Ok(config) => {
                self.config.http = config.http;
                self.remote.error = None;
            }
            Err(e) => self.remote.error = Some(format!("{e:#}")),
        }
    }

    /// Ticks or unticks one adapter, address or range. The last one stays
    /// ticked: none would mean every adapter again, which has its own box.
    fn toggle_listen_entry(&mut self, entry: &str, on: bool) {
        let mut list = self.config.http.listen_on.clone();
        if on {
            list.push(entry.to_string());
        } else {
            list.retain(|e| e != entry);
            if list.is_empty() {
                self.remote.error = Some(
                    "Tick something else first, or tick Every network adapter to every host."
                        .into(),
                );
                return;
            }
        }
        self.set_listen_on(list);
    }

    /// Adds the address or range typed in the box.
    fn add_listen_entry(&mut self) {
        let entry = self.text(Field::ListenOn);
        if entry.is_empty() {
            return;
        }
        if let Err(e) = remote::interfaces::Filter::parse(&entry) {
            self.remote.error = Some(format!("{e:#}"));
            return;
        }
        let mut list = self.config.http.listen_on.clone();
        list.push(entry);
        self.set_listen_on(list);
        if self.remote.error.is_none() {
            self.field(Field::ListenOn).set_text(String::new());
            self.field(Field::ListenOn).active = false;
        }
    }

    /// "Every network adapter to every host", or the adapters, addresses and
    /// ranges to listen on instead.
    fn listen_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let listen_on = &self.config.http.listen_on;
        let every = listen_on.is_empty() && !self.remote.limiting;
        let mut out = div().flex().flex_col().gap_2().child(
            Checkbox::new("listen-every", "Every network adapter to every host", every).on_change(
                cx.listener(|s, on: &bool, _, cx| {
                    if *on {
                        s.set_listen_on(Vec::new());
                    }
                    s.remote.limiting = !*on;
                    cx.notify();
                }),
            ),
        );
        if every {
            return out;
        }
        // The choices: Tailscale (even while it is not connected), each adapter,
        // then anything chosen that is neither (addresses, ranges, adapters
        // that are gone for now).
        let adapters = remote::interfaces::adapters();
        let addrs = |ips: &[std::net::IpAddr]| remote::interfaces::summary(ips, 2);
        let mut choices: Vec<(String, String)> = Vec::new();
        let tailscale: Vec<std::net::IpAddr> = adapters
            .iter()
            .flat_map(|a| a.addrs.iter().copied())
            .filter(remote::interfaces::is_tailscale)
            .collect();
        let label = if tailscale.is_empty() {
            "Tailscale (not connected now)".to_string()
        } else {
            format!("Tailscale: {}", addrs(&tailscale))
        };
        choices.push((remote::interfaces::TAILSCALE.to_string(), label));
        for a in &adapters {
            choices.push((a.name.clone(), format!("{}: {}", a.name, addrs(&a.addrs))));
        }
        for e in listen_on {
            if !choices.iter().any(|(c, _)| c == e) {
                choices.push((e.clone(), e.clone()));
            }
        }
        let mut list = div().flex().flex_col().gap_1p5().pl_6();
        for (i, (entry, label)) in choices.into_iter().enumerate() {
            let on = listen_on.contains(&entry);
            list = list.child(
                Checkbox::new(
                    ElementId::Name(SharedString::from(format!("listen-{i}"))),
                    label,
                    on,
                )
                .on_change(cx.listener(move |s, on: &bool, _, cx| {
                    s.toggle_listen_entry(&entry, *on);
                    cx.notify();
                })),
            );
        }
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(self.input(
                    Field::ListenOn,
                    "Address or range, like 192.168.1.0/24",
                    cx,
                )))
                .child(
                    Button::new("listen-add", "Add")
                        .flex_none()
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.add_listen_entry();
                            cx.notify();
                        })),
                ),
        );
        out = out
            .child(
                div()
                    .pl_6()
                    .text_color(rgb(p.text_dim))
                    .child("Only on these, following their addresses as they change:"),
            )
            .child(list);
        out
    }

    fn open_form(&mut self) {
        self.remote.panel = Some(Panel::Form);
        self.remote.error = None;
        self.remote.skill_error = None;
        // A Tailscale lookup is a subprocess; once per form is plenty.
        self.remote.url_placeholder = remote::devices::suggested_url(self.config.http.port);
        self.activate(Field::DeviceName);
        self.field(Field::DeviceName).set_text(String::new());
        self.field(Field::DeviceUrl).set_text(String::new());
        self.field(Field::DeviceName).focus();
    }

    fn generate_skill(&mut self) {
        let name = self.text(Field::DeviceName);
        let url = Some(self.text(Field::DeviceUrl))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| self.remote.url_placeholder.clone());
        match remote::generate(&name, &url) {
            Ok(issued) => self.show_issued(issued),
            Err(e) => self.remote.skill_error = Some(format!("{e:#}")),
        }
    }

    /// Installs the generic skill (it holds no key) for one agent harness.
    fn deploy_skill(&mut self, client: Client) {
        let skill = skill::render_generic(&crate::tools::catalog());
        self.remote.deploy_note = Some(match remote::deploy_skill(client, &skill) {
            Ok(path) => format!(
                "Installed for {}: {}. Start a new session to use it.",
                client.label(),
                path.display()
            ),
            Err(e) => format!("Couldn't install for {}: {e:#}", client.label()),
        });
    }

    fn regenerate(&mut self, id: &str) {
        match remote::regenerate(id, None) {
            Ok(issued) => self.show_issued(issued),
            Err(e) => self.remote.error = Some(format!("{e:#}")),
        }
    }

    fn show_issued(&mut self, issued: remote::Issued) {
        self.remote.panel = Some(Panel::Issued {
            device: issued.device.name.clone(),
            prompt: issued.prompt,
            entry: issued.host_entry,
            skill: issued.skill,
            tab: IssuedTab::Prompt,
            note: None,
            selecting: None,
        });
        self.remote.error = None;
        self.remote.skill_error = None;
        self.remote.devices = Devices::load().devices;
        for (_, e) in &mut self.fields {
            e.active = false;
        }
    }

    fn remove_device(&mut self, id: &str) {
        if self.remote.confirm_remove.as_deref() != Some(id) {
            self.remote.confirm_remove = Some(id.to_string());
            return;
        }
        self.remote.confirm_remove = None;
        match Devices::load().remove(id) {
            Ok(_) => self.remote.devices = Devices::load().devices,
            Err(e) => self.remote.error = Some(format!("{e:#}")),
        }
    }

    /// The key just issued, in a modal: as a prompt that has the device's
    /// agent set itself up, or as the hosts-file entry and the skill.
    fn issued_modal(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let p = palette();
        let dim = |t: String| {
            div()
                .text_color(rgb(p.text_dim))
                .text_size(px(11.5))
                .child(t)
        };
        let Some(Panel::Issued {
            device,
            entry,
            skill,
            prompt,
            tab,
            note,
            selecting,
        }) = &self.remote.panel
        else {
            return None;
        };
        let tab = *tab;
        let note_msg = |s: &mut Self, msg: String| {
            if let Some(Panel::Issued { note, .. }) = &mut s.remote.panel {
                *note = Some(msg);
            }
        };
        let set_tab = |to: IssuedTab| {
            cx.listener(move |s: &mut Self, _: &gpui::ClickEvent, _, cx| {
                if let Some(Panel::Issued {
                    tab,
                    note,
                    selecting,
                    ..
                }) = &mut s.remote.panel
                {
                    *tab = to;
                    *note = None;
                    *selecting = None;
                }
                cx.notify();
            })
        };
        let copy_button =
            |id: &'static str, label: &'static str, text: String, msg: &'static str| {
                Button::new(id, label).on_click(cx.listener(move |s, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    note_msg(s, msg.into());
                    cx.notify();
                }))
            };
        // A block of text to read, select and copy, that scrolls on its
        // own: the wheel stops here, as gpui otherwise scrolls every scroll
        // area under the pointer at once. Its own scroll id per block keeps
        // each one's place.
        let mono = |block: Block, text: String| {
            let input = match selecting {
                Some((b, edit)) if *b == block => TextInput::edit(block.id(), edit),
                _ => TextInput::new(block.id(), text.clone()).multiline(),
            };
            let pressed = text.clone();
            div()
                .id(ElementId::Name(SharedString::from(format!(
                    "{}-scroll",
                    block.id()
                ))))
                .overflow_y_scroll()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .rounded(px(6.0))
                .bg(rgb(p.deep_bg))
                .child(
                    input
                        .caret_on(false)
                        .colors(TextInputColors {
                            bg: p.deep_bg,
                            border: None,
                            focus_border: p.deep_bg,
                            ..Default::default()
                        })
                        .w_full()
                        .p_2()
                        .rounded(px(6.0))
                        .font_family("Menlo")
                        .text_size(px(11.0))
                        .on_focus(cx.listener(move |s, press: &TextPress, _, cx| {
                            // The settings' fields give up the keyboard, so
                            // ⌘C copies from here.
                            for (_, e) in &mut s.fields {
                                e.active = false;
                            }
                            if let Some(Panel::Issued { selecting, .. }) = &mut s.remote.panel {
                                if !matches!(selecting, Some((b, _)) if *b == block) {
                                    let mut edit = LineEdit::multiline();
                                    edit.text = pressed.clone();
                                    *selecting = Some((block, edit));
                                }
                                if let Some((_, edit)) = selecting {
                                    edit.press(press);
                                }
                            }
                            cx.notify();
                        }))
                        .on_select_to(cx.listener(move |s, at: &usize, _, cx| {
                            if let Some(Panel::Issued {
                                selecting: Some((b, edit)),
                                ..
                            }) = &mut s.remote.panel
                            {
                                if *b == block {
                                    edit.extend_to(*at);
                                    cx.notify();
                                }
                            }
                        })),
                )
        };
        let tabs = div()
            .flex()
            .gap_1()
            .pb_1()
            .child(
                Chip::new("tab-prompt", "Prompt for an agent")
                    .selected(tab == IssuedTab::Prompt)
                    .on_click(set_tab(IssuedTab::Prompt)),
            )
            .child(
                Chip::new("tab-skill", "Skill file")
                    .selected(tab == IssuedTab::Skill)
                    .on_click(set_tab(IssuedTab::Skill)),
            );
        let done = Button::new("done-skill", "Done")
            .ghost()
            .on_click(cx.listener(|s, _, _, cx| {
                s.remote.panel = None;
                cx.notify();
            }));
        let modal = Modal::new(format!("Key for {device}"))
            .width(600.0)
            .fill()
            .child(tabs);
        let modal = match tab {
            IssuedTab::Prompt => modal
                .child(dim(format!(
                    "Paste this into an agent on {device}, such as Claude Code or Codex. It has the skill and this computer's entry for {}, holding the key, and asks the agent to set both up. The key isn't shown anywhere else.",
                    skill::HOSTS_FILE
                )))
                .child(mono(Block::Prompt, prompt.clone()).flex_1().min_h(px(120.0)))
                .action(done)
                .action(
                    copy_button("copy-prompt", "Copy Prompt", prompt.clone(), "Copied. It holds the key, so clear the clipboard once it's pasted.")
                        .primary(),
                ),
            IssuedTab::Skill => {
                let save = skill.clone();
                let save_entry = entry.clone();
                let download_entry = entry.clone();
                modal
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child(format!("1. The entry for {}", skill::HOSTS_FILE)))
                    .child(dim(format!(
                        "On {device}, add this under the file's `hosts:` line (a new file starts with `hosts:`). It holds the key, which isn't shown anywhere else."
                    )))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(mono(Block::Entry, entry.trim_end().to_string()).flex_1().min_w_0())
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .flex_none()
                                    .child(copy_button("copy-entry", "Copy Entry", entry.clone(), "Copied the entry. It holds the key, so clear the clipboard once it's pasted."))
                                    .child(Button::new("save-entry", "Save to hosts file").on_click(cx.listener(move |s, _, _, cx| {
                                        let msg = match remote::save_host_entry(&save_entry) {
                                            Ok(path) => format!("Saved to {}.", path.display()),
                                            Err(e) => format!("Couldn't save: {e:#}"),
                                        };
                                        note_msg(s, msg);
                                        cx.notify();
                                    })))
                                    .child(Button::new("download-entry", "Download Entry").on_click(cx.listener(move |s, _, _, cx| {
                                        let msg = match remote::save_download(skill::GENERIC_SKILL_NAME, "hosts-entry.yaml", &download_entry) {
                                            Ok(path) => {
                                                let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn();
                                                format!("Saved {}. It holds the key; delete it once it's in place.", path.display())
                                            }
                                            Err(e) => format!("Couldn't save: {e:#}"),
                                        };
                                        note_msg(s, msg);
                                        cx.notify();
                                    }))),
                            ),
                    )
                    .child(div().pt_2().font_weight(gpui::FontWeight::MEDIUM).child("2. The skill"))
                    .child(dim(format!(
                        "You only need this once on {device}: the same skill drives every computer in its hosts file. Put it at ~/.claude/skills/{}/SKILL.md, or wherever its agent keeps skills.",
                        skill::GENERIC_SKILL_NAME
                    )))
                    .child(mono(Block::Skill, skill.clone()).flex_1().min_h(px(120.0)))
                    .action(done)
                    .action(Button::new("save-skill", "Save to Downloads").on_click(cx.listener(move |s, _, _, cx| {
                        let msg = match remote::save_download(skill::GENERIC_SKILL_NAME, "SKILL.md", &save) {
                            Ok(path) => {
                                reveal(&path);
                                format!("Saved {}.", path.display())
                            }
                            Err(e) => format!("Couldn't save: {e:#}"),
                        };
                        note_msg(s, msg);
                        cx.notify();
                    })))
                    .action(copy_button("copy-skill", "Copy Skill", skill.clone(), "Copied the skill.").primary())
            }
        };
        Some(modal.when_some(note.clone(), |d, n| d.child(dim(n))).when(
            !self.config.http.enabled,
            |d| {
                d.child(div().text_color(rgb(p.warning)).text_size(px(11.5)).child(
                    "The HTTP server is off; turn on Serve over HTTP before the device connects.",
                ))
            },
        ))
    }

    /// Generate Skill, in Other devices: the form, and the key it issues.
    fn skill_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let dim = |t: String| div().text_color(rgb(p.text_dim)).child(t);
        let section = div().flex().flex_col().gap_2();
        match &self.remote.panel {
            None | Some(Panel::Issued { .. }) => section
                .child(
                    div().flex().child(
                        Button::new("generate-skill", "Generate Skill…")
                            .primary()
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.open_form();
                                cx.notify();
                            })),
                    ),
                )
                .child(dim(
                    "Or install the skill, which holds no key, for an agent on this computer to drive the computers in its hosts file:".into(),
                ).text_size(px(11.5)))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(Button::new("deploy-claude-code", "Claude Code").on_click(cx.listener(|s, _, _, cx| {
                            s.deploy_skill(Client::ClaudeCode);
                            cx.notify();
                        })))
                        .child(Button::new("deploy-codex", "Codex").on_click(cx.listener(|s, _, _, cx| {
                            s.deploy_skill(Client::Codex);
                            cx.notify();
                        })))
                        .child(Button::new("deploy-opencode", "OpenCode").on_click(cx.listener(|s, _, _, cx| {
                            s.deploy_skill(Client::OpenCode);
                            cx.notify();
                        }))),
                )
                .when_some(self.remote.deploy_note.clone(), |d, n| {
                    d.child(dim(n).text_size(px(11.5)))
                }),
            Some(Panel::Form) => section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(p.edge))
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child("New device"))
                    .child(Self::row("Device name", self.input(Field::DeviceName, "Work laptop", cx)))
                    .child(Self::row("URL", self.input(Field::DeviceUrl, &self.remote.url_placeholder, cx)))
                    .child(dim("The device that connects in, and the address it reaches this computer at.".into()).text_size(px(11.5)))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("do-generate", "Generate Skill").primary().on_click(cx.listener(|s, _, _, cx| {
                                s.generate_skill();
                                cx.notify();
                            })))
                            .child(Button::new("cancel-generate", "Cancel").ghost().on_click(cx.listener(|s, _, _, cx| {
                                s.remote.panel = None;
                                s.remote.skill_error = None;
                                cx.notify();
                            }))),
                    ),
            ),
        }
        .when_some(self.remote.skill_error.clone(), |d, e| {
            d.child(div().text_color(rgb(p.warning)).child(e))
        })
    }

    fn remote_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Only the Mac app stays running with its window closed.
        #[cfg(target_os = "macos")]
        let listening_line = |addr: &str| {
            if remote::autostart::is_set() {
                format!("Listening on {addr}; starts at login")
            } else {
                format!("Listening on {addr}")
            }
        };
        #[cfg(not(target_os = "macos"))]
        let listening_line = |addr: &str| {
            format!(
                "Listening on {addr} while this window is open; \
                 `opencomputeruse serve --install` serves at login instead"
            )
        };
        let p = palette();
        let http = &self.config.http;
        let state = super::http::state();
        let dim = |t: String| div().text_color(rgb(p.text_dim)).child(t);
        let status_line = match (http.enabled, &state.listening, &state.error) {
            (false, _, _) => status("Off", false),
            (true, _, Some(e)) => div().text_color(rgb(p.warning)).child(e.clone()),
            (true, Some(addr), _) => status(&listening_line(addr), true),
            (true, None, None) => status("Starting…", false),
        };
        let mut section = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(dim(
                "Let other devices, on your network or over Tailscale, drive apps on this computer through an HTTP API. Each device gets its own key, inside a skill you install on it.".into(),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div().flex_1().min_w_0().child(
                            Checkbox::new("http-on", "Serve over HTTP", http.enabled)
                                .on_change(cx.listener(|s, on: &bool, _, cx| {
                                    s.set_http(*on);
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(div().text_color(rgb(p.text_dim)).child("Port"))
                    .child(div().w(px(80.0)).flex_none().child(self.input(Field::HttpPort, "8642", cx))),
            )
            .child(self.listen_picker(cx))
            .child(status_line);

        // The devices.
        for d in &self.remote.devices {
            let (rid, xid) = (d.id.clone(), d.id.clone());
            let confirming = self.remote.confirm_remove.as_deref() == Some(d.id.as_str());
            section = section.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_2()
                    .rounded(px(6.0))
                    .bg(rgb(p.deep_bg))
                    .child(div().flex_none().child(icon("computer", 18.0, p.text_dim)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(d.name.clone()),
                            )
                            .child(
                                div()
                                    .text_color(rgb(p.text_dim))
                                    .text_size(px(11.0))
                                    .child(d.url.clone()),
                            ),
                    )
                    .child(
                        Button::new(
                            ElementId::Name(SharedString::from(format!("regen-{}", d.id))),
                            "Regenerate Key",
                        )
                        .flex_none()
                        .on_click(cx.listener(move |s, _, _, cx| {
                            s.regenerate(&rid);
                            cx.notify();
                        })),
                    )
                    .child(
                        Button::new(
                            ElementId::Name(SharedString::from(format!("remove-{}", d.id))),
                            if confirming {
                                "Confirm Remove"
                            } else {
                                "Remove"
                            },
                        )
                        .flex_none()
                        .when(!confirming, |b| b.ghost())
                        .on_click(cx.listener(move |s, _, _, cx| {
                            s.remove_device(&xid);
                            cx.notify();
                        })),
                    ),
            );
        }

        section
            .when_some(self.remote.error.clone(), |d, e| {
                d.child(div().text_color(rgb(p.warning)).child(e))
            })
            .child(self.skill_section(cx))
    }

    /// Asks GitHub for the latest release, off the main thread. `announce`,
    /// for the menu's check, also says what it found in an alert, since the
    /// Updates section is likely scrolled out of sight.
    pub fn check_for_updates(
        &mut self,
        announce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.updates.checking || self.updates.progress.is_some() {
            return;
        }
        self.updates.checking = true;
        self.updates.error = None;
        cx.notify();
        let task = cx.background_executor().spawn(async { update::check() });
        // Long enough to see "Checking…", so a click visibly does something.
        let shown = cx.background_executor().timer(Duration::from_millis(600));
        cx.spawn_in(window, async move |this, cx| {
            let status = task.await;
            shown.await;
            update::mark_checked();
            let _ = this.update_in(cx, |s, window, cx| {
                if announce {
                    s.announce_update(&status, window, cx);
                }
                s.show_update(status, cx);
            });
        })
        .detach();
    }

    /// An alert with a check's result, offering the update if there is one.
    fn announce_update(&self, status: &UpdateStatus, window: &mut Window, cx: &mut Context<Self>) {
        let current = update::current_version();
        let answer = match status {
            UpdateStatus::UpToDate => window.prompt(
                PromptLevel::Info,
                "You're up to date.",
                Some(&format!("OpenComputerUse {current} is the latest version.")),
                &[PromptButton::Ok("OK".into())],
                cx,
            ),
            UpdateStatus::Failed(e) => window.prompt(
                PromptLevel::Warning,
                "Couldn't check for updates.",
                Some(e),
                &[PromptButton::Ok("OK".into())],
                cx,
            ),
            UpdateStatus::Available(up) => {
                let mut detail = format!("You have {current}.");
                if up.install.is_some() && !self.sessions.is_empty() {
                    detail.push_str(
                        " Updating restarts the app, which ends the sessions running now.",
                    );
                }
                let action = if up.install.is_some() {
                    "Update"
                } else {
                    "Open Release Page"
                };
                let answer = window.prompt(
                    PromptLevel::Info,
                    &format!("OpenComputerUse {} is available.", up.version),
                    Some(&detail),
                    &[action, "Later"],
                    cx,
                );
                let up = up.clone();
                cx.spawn(async move |this, cx| {
                    if answer.await != Ok(0) {
                        return;
                    }
                    match up.install {
                        Some(installer) => {
                            let _ = this.update(cx, |s, cx| s.install_update(installer, cx));
                        }
                        None => open_url(&up.page),
                    }
                })
                .detach();
                return;
            }
        };
        drop(answer);
    }

    /// Shows the result of a check, made here or by the background check.
    pub fn show_update(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        self.updates.checking = false;
        self.updates.status = Some(status);
        cx.notify();
    }

    /// Downloads the update, then swaps it in and relaunches.
    fn install_update(&mut self, installer: Installer, cx: &mut Context<Self>) {
        let received = self.updates.received.clone();
        received.store(0, Ordering::Relaxed);
        self.updates.progress = Some(Progress::Downloading {
            received: 0,
            total: installer.size,
        });
        self.updates.error = None;
        cx.notify();
        let total = installer.size;
        let task = cx.background_executor().spawn(async move {
            let file = update::download(&installer, &received)?;
            update::install_and_restart(&file)
        });
        // Progress, while the download runs.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let alive = this.update(cx, |s, cx| {
                if let Some(Progress::Downloading { .. }) = s.updates.progress {
                    let got = s.updates.received.load(Ordering::Relaxed);
                    s.updates.progress = Some(if got >= total && total > 0 {
                        Progress::Installing
                    } else {
                        Progress::Downloading {
                            received: got,
                            total,
                        }
                    });
                    cx.notify();
                    true
                } else {
                    false
                }
            });
            if !matches!(alive, Ok(true)) {
                break;
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| match result {
                // The relauncher opens the new copy once this one is gone.
                Ok(()) => cx.quit(),
                Err(e) => {
                    update::clean_downloads();
                    s.updates.progress = None;
                    s.updates.error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn updates_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let u = &self.updates;
        let line = |text: String| div().text_color(rgb(p.text_dim)).child(text);
        let mut row = div().flex().items_center().gap_3();
        row = row.child(div().flex_1().min_w_0().child(
            match (&u.progress, &u.status, u.checking) {
                (Some(Progress::Downloading { received, total }), _, _) if *total > 0 => {
                    line(format!("Downloading… {}%", received * 100 / total))
                }
                (Some(Progress::Downloading { .. }), _, _) => line("Downloading…".into()),
                (Some(Progress::Installing), _, _) => {
                    line("Installing; OpenComputerUse will reopen.".into())
                }
                (_, _, true) => line("Checking…".into()),
                (_, Some(UpdateStatus::UpToDate), _) => line(format!(
                    "OpenComputerUse {} is the latest version.",
                    update::current_version()
                )),
                (_, Some(UpdateStatus::Available(up)), _) => line(format!(
                    "Version {} is available (you have {}).",
                    up.version,
                    update::current_version()
                )),
                (_, Some(UpdateStatus::Failed(e)), _) => line(format!("Couldn't check: {e}")),
                (_, None, _) => line(format!("Version {}", update::current_version())),
            },
        ));
        let busy = u.checking || u.progress.is_some();
        row = match &u.status {
            Some(UpdateStatus::Available(up)) if u.progress.is_none() => match up.install.clone() {
                Some(installer) => row.child(
                    Button::new("install-update", format!("Update to {}", up.version))
                        .primary()
                        .flex_none()
                        .on_click(
                            cx.listener(move |s, _, _, cx| s.install_update(installer.clone(), cx)),
                        ),
                ),
                None => {
                    let page = up.page.clone();
                    row.child(
                        Button::new("release-page", "Open Release Page")
                            .flex_none()
                            .on_click(move |_, _, _| open_url(&page)),
                    )
                }
            },
            _ => row.child(
                Button::new("check-updates", "Check for Updates")
                    .flex_none()
                    .disabled(busy)
                    .on_click(
                        cx.listener(|s, _, window, cx| s.check_for_updates(false, window, cx)),
                    ),
            ),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(row)
            .when(
                !self.sessions.is_empty() && matches!(u.status, Some(UpdateStatus::Available(_))),
                |d| {
                    d.child(
                        line(
                            "Updating restarts the app, which ends the sessions running now."
                                .into(),
                        )
                        .text_size(px(11.5)),
                    )
                },
            )
            .when_some(u.error.clone(), |d, e| {
                d.child(div().text_color(rgb(p.warning)).child(e))
            })
            .child(
                Checkbox::new(
                    "auto-update",
                    "Check for updates automatically",
                    update::check_automatically(),
                )
                .on_change(cx.listener(|_, on: &bool, _, cx| {
                    update::set_check_automatically(*on);
                    cx.notify();
                })),
            )
    }

    /// Turns "work while locked" on or off. Both need root, so this runs the
    /// CLI behind the admin prompt, then flips the config flag.
    #[cfg(target_os = "macos")]
    fn set_locked_use(&mut self, enable: bool, cx: &mut Context<Self>) {
        self.lock_busy = true;
        self.lock_message = None;
        cx.notify();
        let exe = std::env::current_exe().unwrap_or_default();
        let plugin =
            bundle().map(|b| b.join("Contents/Resources/OcuLockAuthorizationPlugin.bundle"));
        let uid = unsafe { libc::getuid() };
        let task = cx
            .background_executor()
            .spawn(async move { run_privileged_lock(enable, &exe, plugin.as_deref(), uid) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.lock_busy = false;
                match result {
                    Ok(()) => {
                        s.config.allow_unlock = enable;
                        let _ = s.config.save();
                        s.lock_installed = ocu_macos::lock::installed();
                        s.lock_message = Some(if enable {
                            "On. Agents can now unlock the Mac to keep working.".into()
                        } else {
                            "Off. Unlocking needs your password again.".into()
                        });
                    }
                    Err(e) => s.lock_message = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Installs into (or removes from) a client off the main thread: the
    /// CLIs take a moment.
    fn install(&mut self, client: Client, install: bool, cx: &mut Context<Self>) {
        self.busy = Some(client);
        self.client_message = None;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            if install {
                clients::install(client)
            } else {
                clients::uninstall(client)
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.busy = None;
                s.client_message = Some(match result {
                    Ok(()) if install => {
                        format!("Installed in {}. {}", client.label(), client.next_step())
                    }
                    Ok(()) => format!("Removed from {}.", client.label()),
                    Err(e) => format!("{e:#}"),
                });
                s.clients = client_states();
                cx.notify();
            });
        })
        .detach();
    }

    fn field(&mut self, f: Field) -> &mut LineEdit {
        &mut self.fields.iter_mut().find(|(k, _)| *k == f).unwrap().1
    }

    fn text(&self, f: Field) -> String {
        self.fields
            .iter()
            .find(|(k, _)| *k == f)
            .unwrap()
            .1
            .text
            .trim()
            .to_string()
    }

    fn activate(&mut self, f: Field) {
        for (k, e) in &mut self.fields {
            e.active = *k == f;
        }
    }

    fn active(&self) -> Option<Field> {
        self.fields.iter().find(|(_, e)| e.active).map(|(k, _)| *k)
    }

    fn save(&mut self) {
        let [key, model, account, token, cf_model, confidence] =
            Field::RECIPE.map(|f| self.text(f));
        let r = &mut self.config.recipe;
        r.typesafe_api_key = key;
        r.typesafe_model = model;
        r.cloudflare_account_id = account;
        r.cloudflare_api_token = token;
        r.cloudflare_model = cf_model;
        match confidence.parse::<f64>() {
            Ok(v) if (0.0..=1.0).contains(&v) => r.min_confidence = v,
            _ => {
                self.error = Some("Minimum confidence is a number from 0 to 1.".into());
                return;
            }
        }
        match self.config.save() {
            Ok(()) => {
                self.saved = true;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not save: {e:#}")),
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // The modal's text is read-only: ⌘A and ⌘C, nothing that edits.
        if let Some(Panel::Issued {
            selecting: Some((_, edit)),
            ..
        }) = &mut self.remote.panel
        {
            let m = ev.keystroke.modifiers;
            if (m.platform || m.control) && matches!(ev.keystroke.key.as_str(), "a" | "c") {
                edit.key(ev, cx);
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }
        if ev.keystroke.key == "escape" && matches!(self.remote.panel, Some(Panel::Issued { .. })) {
            self.remote.panel = None;
            cx.notify();
            return;
        }
        let Some(f) = self.active() else { return };
        let key = ev.keystroke.key.as_str();
        if key == "escape" {
            self.field(f).active = false;
            cx.notify();
            return;
        }
        if key == "tab" {
            let i = Field::ALL.iter().position(|x| *x == f).unwrap();
            let n = Field::ALL.len();
            let next = Field::ALL[if ev.keystroke.modifiers.shift {
                (i + n - 1) % n
            } else {
                (i + 1) % n
            }];
            self.activate(next);
            self.field(next).select_all();
            cx.notify();
            cx.stop_propagation();
            return;
        }
        match self.field(f).key(ev, cx) {
            LineEditKey::Ignored => {}
            LineEditKey::Submitted => {
                match f {
                    Field::HttpPort => self.apply_port(),
                    Field::ListenOn => self.add_listen_entry(),
                    Field::DeviceName | Field::DeviceUrl => self.generate_skill(),
                    _ => self.save(),
                }
                cx.notify();
                cx.stop_propagation();
            }
            LineEditKey::Changed => {
                if Field::RECIPE.contains(&f) {
                    self.saved = false;
                }
                cx.notify();
                cx.stop_propagation();
            }
            LineEditKey::Moved => {
                cx.notify();
                cx.stop_propagation();
            }
        }
    }

    fn input(&self, f: Field, placeholder: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let edit = &self.fields.iter().find(|(k, _)| *k == f).unwrap().1;
        let input = if f.secret() && !edit.active {
            let masked: String = "•".repeat(edit.text.chars().count().min(32));
            TextInput::new(ElementId::Name(f.id().into()), masked)
        } else {
            TextInput::edit(ElementId::Name(f.id().into()), edit)
        };
        input
            .placeholder(placeholder.to_string())
            .text_size(px(12.5))
            .px_2()
            .h(px(28.0))
            .w_full()
            .on_focus(cx.listener(move |s, press: &TextPress, _, cx| {
                s.activate(f);
                if !f.secret() {
                    s.field(f).press(press);
                } else {
                    s.field(f).focus();
                }
                cx.notify();
            }))
    }

    /// The settings for how sessions work: on a Mac the overlay and working
    /// while locked, elsewhere how apps' file dialogs are answered.
    #[cfg(target_os = "macos")]
    fn while_working(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                Checkbox::new(
                    "overlay",
                    "Show a halo and cursor over windows being driven",
                    self.config.show_overlay(),
                )
                .on_change(cx.listener(|s, checked: &bool, _, cx| {
                    s.config.show_overlay = Some(*checked);
                    s.save();
                    cx.notify();
                })),
            )
            // Work while locked.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child("Work while the Mac is locked"),
                            )
                            .child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(
                                "Lets agents unlock the Mac to keep working. Asks for \
                                         your password to set up. A key press or click relocks it.",
                            )),
                    )
                    .child(if self.lock_busy {
                        status("Working…", false).into_any_element()
                    } else {
                        Checkbox::new(
                            "locked-use",
                            "",
                            self.config.allow_unlock && self.lock_installed,
                        )
                        .on_change(cx.listener(|s, on: &bool, _, cx| s.set_locked_use(*on, cx)))
                        .into_any_element()
                    }),
            )
            .when_some(self.lock_message.clone(), |d, m| {
                d.child(
                    div()
                        .text_color(rgb(p.text_dim))
                        .text_size(px(11.5))
                        .child(m),
                )
            })
    }

    #[cfg(not(target_os = "macos"))]
    fn while_working(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        #[cfg(windows)]
        let (label, help, on) = (
            "Hand open and save dialogs to agents",
            "Loads a small hook into the apps sessions start, so their file dialogs never show. \
             Endpoint protection may object. Off, dialogs are answered on screen instead.",
            self.config.windows_panel_hook,
        );
        #[cfg(target_os = "linux")]
        let (label, help, on) = (
            "Answer open and save dialogs through a portal",
            "Starts each app on a private session bus whose file chooser is the agent, so its \
             dialogs never show. Everything else is forwarded to your own bus. Needs dbus-daemon.",
            self.config.linux_file_portal,
        );
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                Checkbox::new("file-dialogs", label, on).on_change(cx.listener(
                    |s, on: &bool, _, cx| {
                        #[cfg(windows)]
                        {
                            s.config.windows_panel_hook = *on;
                        }
                        #[cfg(target_os = "linux")]
                        {
                            s.config.linux_file_portal = *on;
                        }
                        s.save();
                        cx.notify();
                    },
                )),
            )
            .child(
                div()
                    .text_color(rgb(p.text_dim))
                    .text_size(px(11.5))
                    .child(format!("{help} Applies to apps started from now on.")),
            )
    }

    fn row(label: &str, child: impl IntoElement) -> impl IntoElement {
        let p = palette();
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w(px(130.0))
                    .flex_none()
                    .text_color(rgb(p.text_dim))
                    .child(label.to_string()),
            )
            .child(div().flex_1().min_w_0().child(child))
    }

    fn section(title: &str) -> impl IntoElement {
        div()
            .pt_2()
            .child(Heading::new(title.to_string()).uppercase())
    }
}

impl Render for Status {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let all_granted = self.permissions.iter().all(|p| p.granted || p.optional);
        let provider = self.config.recipe.provider;

        let modal = self.issued_modal(cx).map(IntoElement::into_any_element);
        let page = div()
            .id("status")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(p.window_bg))
            .text_color(rgb(p.text))
            .text_size(px(12.5))
            .child(
                div()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    // Header.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().text_size(px(18.0)).font_weight(gpui::FontWeight::SEMIBOLD).child("OpenComputerUse"))
                                    .child(div().text_color(rgb(p.text_dim)).child("Background computer use for agents, over MCP.")),
                            )
                            .child(div().flex_none().child(if all_granted {
                                Badge::new("Ready").colors(GOOD, 0xFFFFFF)
                            } else {
                                Badge::new("Needs permissions").colors(p.warning, 0xFFFFFF)
                            })),
                    )
                    .child(Divider::horizontal())
                    // Permissions.
                    .child(Self::section("Permissions"))
                    // Off a Mac the optional ones are settings, offered below.
                    .children(self.permissions.iter().enumerate().filter(|(_, perm)| cfg!(target_os = "macos") || !perm.optional).map(|(i, perm)| {
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(div().font_weight(gpui::FontWeight::MEDIUM).child(perm.name.clone()))
                                            .when(perm.optional, |d| d.child(Badge::new("Optional").colors(p.text_faint, 0xFFFFFF))),
                                    )
                                    .child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(perm.help.clone())),
                            )
                            .child(grant(i, perm))
                    }))
                    .when(cfg!(target_os = "macos") && !all_granted, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().flex_1().min_w_0().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(
                                    "If OpenComputerUse is missing from the list in System Settings, press + there and add it (Show in Finder finds it). macOS applies Screen Recording after the app reopens.",
                                ))
                                .child(Button::new("reveal", "Show in Finder").flex_none().on_click(|_, _, _| {
                                    #[cfg(target_os = "macos")]
                                    reveal_app();
                                }))
                                .child(Button::new("reopen", "Reopen").flex_none().on_click(|_, _, _cx| {
                                    #[cfg(target_os = "macos")]
                                    reopen(_cx);
                                })),
                        )
                    })
                    // MCP setup.
                    .child(Self::section("Add to an MCP client"))
                    .child(div().text_color(rgb(p.text_dim)).child(
                        "Clients start this app's MCP server, which talks to the app, so the permissions above cover them all.",
                    ))
                    .children(Client::ALL.iter().map(|&client| {
                        let state = &self.clients[client as usize];
                        let busy = self.busy == Some(client);
                        let installed = state.registration != clients::Registration::Absent;
                        let elsewhere = match &state.registration {
                            clients::Registration::Elsewhere(path) => Some(path.clone()),
                            _ => None,
                        };
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(div().flex_1().min_w_0().font_weight(gpui::FontWeight::MEDIUM).child(client.label()))
                                    .child(div().w(px(105.0)).flex_none().child(match (state.found, &state.registration, busy) {
                                        (_, _, true) => status("Working…", false),
                                        (false, _, _) => status("Not found", false),
                                        (true, clients::Registration::Current, _) => status("Installed", true),
                                        (true, clients::Registration::Elsewhere(_), _) => status("Points elsewhere", false),
                                        (true, clients::Registration::Absent, _) => status("Not installed", false),
                                    }))
                                    .child(div().flex_none().flex().justify_end().gap_2().when(state.found, |d| {
                                        d.child(
                                            Button::new(("install", client as usize), if installed { "Reinstall" } else { "Install" })
                                                // A stale entry needs fixing as much as a missing one.
                                                .when(state.registration != clients::Registration::Current, |b| b.primary())
                                                .disabled(self.busy.is_some())
                                                .on_click(cx.listener(move |s, _, _, cx| s.install(client, true, cx))),
                                        )
                                        .when(installed, |d| {
                                            d.child(
                                                Button::new(("remove", client as usize), "Remove")
                                                    .ghost()
                                                    .disabled(self.busy.is_some())
                                                    .on_click(cx.listener(move |s, _, _, cx| s.install(client, false, cx))),
                                            )
                                        })
                                    }))
                            )
                            .when_some(elsewhere, |d, path| {
                                d.child(
                                    div()
                                        .text_color(rgb(p.warning))
                                        .text_size(px(11.0))
                                        .child(format!("It runs {path}, not this app; Reinstall points it here.")),
                                )
                            })
                    }))
                    .when_some(self.client_message.clone(), |d, m| {
                        d.child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(m))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w_0().text_color(rgb(p.text_dim)).child("Other clients (Cursor, Windsurf, …)"))
                            .child(Button::new("copy-json", "Copy JSON config").flex_none().on_click(cx.listener(move |s, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(clients::json_snippet()));
                                s.client_message = Some("Copied the JSON config.".into());
                                cx.notify();
                            }))),
                    )
                    // Sessions.
                    .child(Self::section("Sessions"))
                    .when(self.sessions.is_empty(), |d| {
                        d.child(div().text_color(rgb(p.text_dim)).child("No sessions running."))
                    })
                    .children(self.sessions.iter().map(|s| {
                        let id = s.id.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child(s.app.clone()))
                                    .child(
                                        div()
                                            .text_color(rgb(p.text_dim))
                                            .text_size(px(11.0))
                                            .child(match s.backend.as_str() {
                                                "macos" => format!("{} · pid {}", s.id, s.pid.unwrap_or(0)),
                                                // A phone, simulator or emulator: say which.
                                                backend => {
                                                    let on = ["simulator", "device", "serial"]
                                                        .iter()
                                                        .find_map(|k| s.details.get(*k))
                                                        .map(String::as_str)
                                                        .unwrap_or(backend);
                                                    format!("{} · on {on}", s.id)
                                                }
                                            }),
                                    ),
                            )
                            .child(Button::new(ElementId::Name(SharedString::from(format!("end-{}", s.id))), "End").on_click(
                                cx.listener(move |st, _, _, cx| {
                                    let _ = st.service.end(&id);
                                    st.refresh(cx);
                                    cx.notify();
                                }),
                            ))
                    }))
                    // Mobile devices.
                    .child(Self::section("Phones and simulators"))
                    .child(
                        Checkbox::new("mobile", "Phones, simulators and emulators", self.config.mobile)
                            .on_change(cx.listener(|s, checked: &bool, _, cx| {
                                s.config.mobile = *checked;
                                s.save();
                                cx.notify();
                            })),
                    )
                    .child(
                        div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(
                            "Lets agents start apps on connected phones and tablets, iOS \
                             simulators and Android emulators. While it is off, their tools \
                             aren't offered.",
                        ),
                    )
                    .child(Self::section("While working"))
                    .child(self.while_working(cx))
                    // Other devices.
                    .child(Self::section("Other devices"))
                    .child(self.remote_section(cx))
                    // Updates.
                    .child(Self::section("Updates"))
                    .child(self.updates_section(cx))
                    // Recipes.
                    .child(Self::section("Recipes"))
                    .child(div().text_color(rgb(p.text_dim)).child(
                        "run_recipe matches each step's target to the accessibility tree with a decision model, then acts on it.",
                    ))
                    .child(Self::row(
                        "Decision model",
                        div().flex().flex_wrap().gap_2().children(Provider::ALL.iter().map(|&prov| {
                            Button::new(ElementId::Name(SharedString::from(format!("{prov:?}"))), prov.label())
                                .active(provider == prov)
                                .when(provider == prov, |b| b.primary())
                                .on_click(cx.listener(move |s, _, _, cx| {
                                    s.config.recipe.provider = prov;
                                    s.saved = false;
                                    cx.notify();
                                }))
                        })),
                    ))
                    .when(provider == Provider::Typesafe, |d| {
                        d.child(Self::row("API key", self.input(Field::TypesafeKey, "From typesafe.ai", cx)))
                            .child(Self::row("Model", self.input(Field::TypesafeModel, "jev-latest", cx)))
                    })
                    .when(provider == Provider::Cloudflare, |d| {
                        d.child(Self::row("Account ID", self.input(Field::CloudflareAccount, "Cloudflare account id", cx)))
                            .child(Self::row("API token", self.input(Field::CloudflareToken, "Token with Workers AI access", cx)))
                            .child(Self::row("Model", self.input(Field::CloudflareModel, "@cf/cloudflare/clef-flash", cx)))
                            .child(Self::row(
                                "",
                                Checkbox::new("cf-shots", "Send a screenshot with each step", self.config.recipe.cloudflare_screenshots)
                                    .on_change(cx.listener(|s, checked: &bool, _, cx| {
                                        s.config.recipe.cloudflare_screenshots = *checked;
                                        s.saved = false;
                                        cx.notify();
                                    })),
                            ))
                    })
                    .child(Self::row("Minimum confidence", self.input(Field::MinConfidence, "0.6", cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Button::new("save", "Save")
                                    .primary()
                                    .disabled(self.saved)
                                    .on_click(cx.listener(|s, _, _, cx| {
                                        s.save();
                                        cx.notify();
                                    })),
                            )
                            .when(self.saved && self.error.is_none(), |d| {
                                d.child(div().text_color(rgb(p.text_dim)).child("Saved. Recipes pick changes up on their next run."))
                            })
                            .when_some(self.error.clone(), |d, e| d.child(div().text_color(rgb(p.warning)).child(e))),
                    ),
            );
        // The modal covers the window, not the page that scrolls under it.
        div().size_full().relative().child(page).children(modal)
    }
}

pub fn open(service: Arc<Service>, cx: &mut App) -> Option<gpui::WindowHandle<Status>> {
    use gpui::{size, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
    let bounds = Bounds::centered(None, size(px(640.0), px(720.0)), cx);
    #[cfg(target_os = "macos")]
    super::native::set_regular(true);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("OpenComputerUse".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(460.0), px(420.0))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Status::new(service, window, cx)),
        )
        .map_err(|e| log::error!("opening the status window: {e:#}"))
        .ok()?;
    cx.activate(true);
    Some(handle)
}
