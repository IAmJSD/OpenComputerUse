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
    icon, palette, Badge, Button, Checkbox, Divider, Heading, LineEdit, LineEditKey, TextInput,
    TextPress,
};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::clients::{self, Client};
use crate::config::{Config, Provider};
use crate::remote::{self, devices::Device, devices::Devices, skill};
use crate::skills;
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
    /// The generic skill's text, while it is showing in Other devices.
    skill_preview: Option<String>,
    /// What the last skill button did.
    skill_message: Option<String>,
    updates: Updates,
    remote: Remote,
    /// "Work while the Mac is locked": whether the privileged pieces are in
    /// place, whether a change is running, and the last message.
    lock_installed: bool,
    lock_busy: bool,
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
    /// "Every network adapter to every host" is unticked, but nothing is
    /// chosen yet, so the settings still say every adapter.
    limiting: bool,
    /// The server's state as last drawn, to redraw when it changes.
    server_seen: (Option<String>, Option<String>),
}

enum Panel {
    Form,
    Issued {
        device: String,
        skill: String,
        /// What to add under `hosts:` on the device.
        entry: String,
        /// Made by Regenerate Key, so shown beside the device list rather
        /// than with the Generate Skill button.
        regen: bool,
        note: Option<String>,
    },
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
    /// The client has a skills folder on this computer, and whether the
    /// generic skill is in it.
    skill_available: bool,
    skill_installed: bool,
}

const GOOD: u32 = 0x2E7D4F;

/// The .app this is running from, if it is.
fn bundle() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Into::into)
}

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
fn reopen(cx: &mut App) {
    if let Some(b) = bundle() {
        let script = format!("sleep 1; /usr/bin/open {:?}", b.display().to_string());
        let _ = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .spawn();
    }
    cx.quit();
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
    Client::ALL.map(|c| {
        let found = clients::find(c).is_some();
        ClientState {
            found,
            registration: clients::registration(c),
            skill_available: skills::available(c, found),
            skill_installed: skills::installed(c),
        }
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
            skill_preview: None,
            skill_message: None,
            lock_installed: ocu_macos::lock::installed(),
            lock_busy: false,
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
            Ok(issued) => self.show_issued(issued, false),
            Err(e) => self.remote.skill_error = Some(format!("{e:#}")),
        }
    }

    fn regenerate(&mut self, id: &str) {
        match remote::regenerate(id, None) {
            Ok(issued) => self.show_issued(issued, true),
            Err(e) => self.remote.error = Some(format!("{e:#}")),
        }
    }

    fn show_issued(&mut self, issued: remote::Issued, regen: bool) {
        self.remote.panel = Some(Panel::Issued {
            device: issued.device.name.clone(),
            skill: issued.skill,
            entry: issued.host_entry,
            regen,
            note: None,
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

    /// The key just issued: its hosts-file entry, and a skill that holds it.
    fn issued_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = palette();
        let dim = |t: String| div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(t);
        let Some(Panel::Issued { device, skill, entry, note, .. }) = &self.remote.panel else {
            return div().into_any_element();
        };
        let file = format!("hosts:\n{entry}");
        let (copy, save, copy_file, save_file) = (skill.clone(), skill.clone(), file.clone(), file.clone());
        let mono = |id: &'static str, text: String| {
            div()
                .id(id)
                .max_h(px(220.0))
                .overflow_y_scroll()
                .p_2()
                .rounded(px(6.0))
                .bg(rgb(p.deep_bg))
                .font_family("Menlo")
                .text_size(px(11.0))
                .child(text)
        };
        let note_msg = |s: &mut Self, msg: String| {
            if let Some(Panel::Issued { note, .. }) = &mut s.remote.panel {
                *note = Some(msg);
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(8.0))
            .border_1()
            .border_color(rgb(p.edge))
            .child(div().font_weight(gpui::FontWeight::MEDIUM).child(format!("Key for {device}")))
            .child(dim(format!(
                "On {device}, save this as {}. If that file exists, add the lines under its `hosts:` instead (the last entry of a name wins, so a regenerated key can go below the old one). The key isn't shown anywhere else.",
                skill::HOSTS_FILE
            )))
            .child(mono("host-entry", file.clone()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Button::new("copy-entry", "Copy hosts.yaml").primary().on_click(cx.listener(move |s, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_file.clone()));
                        note_msg(s, "Copied. It holds the key, so paste it into the file and clear the clipboard.".into());
                        cx.notify();
                    })))
                    .child(Button::new("save-entry", "Save hosts.yaml").on_click(cx.listener(move |s, _, _, cx| {
                        let msg = match remote::save_download("opencomputeruse", "hosts.yaml", &save_file) {
                            Ok(path) => {
                                let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn();
                                format!("Saved {}.", path.display())
                            }
                            Err(e) => format!("Couldn't save: {e:#}"),
                        };
                        note_msg(s, msg);
                        cx.notify();
                    }))),
            )
            .child(dim(format!(
                "Or install a skill on {device} that holds this key itself, as ~/.claude/skills/{}/SKILL.md:",
                skill::skill_name()
            )))
            .child(mono("skill-text", skill.clone()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Button::new("copy-skill", "Copy skill").on_click(cx.listener(move |s, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        note_msg(s, "Copied.".into());
                        cx.notify();
                    })))
                    .child(Button::new("save-skill", "Save to Downloads").on_click(cx.listener(move |s, _, _, cx| {
                        let msg = match remote::save_download(&skill::skill_name(), "SKILL.md", &save) {
                            Ok(path) => {
                                let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn();
                                format!("Saved {}.", path.display())
                            }
                            Err(e) => format!("Couldn't save: {e:#}"),
                        };
                        note_msg(s, msg);
                        cx.notify();
                    })))
                    .child(Button::new("done-skill", "Done").ghost().on_click(cx.listener(|s, _, _, cx| {
                        s.remote.panel = None;
                        cx.notify();
                    })))
                    .when_some(note.clone(), |d, n| d.child(dim(n))),
            )
            .when(!self.config.http.enabled, |d| {
                d.child(
                    div()
                        .text_color(rgb(p.warning))
                        .text_size(px(11.5))
                        .child("The HTTP server is off; turn on Serve over HTTP before the device connects."),
                )
            })
            .into_any_element()
    }

    /// Generate Skill, in Other devices: the form, and the key it issues.
    fn skill_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let dim = |t: String| div().text_color(rgb(p.text_dim)).child(t);
        let section = div().flex().flex_col().gap_2().child(dim(format!(
            "Generate Skill makes a key for a device and the entry that device adds to its {}.",
            skill::HOSTS_FILE
        )).text_size(px(11.5)));
        match &self.remote.panel {
            None | Some(Panel::Issued { regen: true, .. }) => section.child(
                div().flex().child(
                    Button::new("generate-skill", "Generate Skill…")
                        .primary()
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.open_form();
                            cx.notify();
                        })),
                ),
            ),
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
            Some(Panel::Issued { regen: false, .. }) => section.child(self.issued_panel(cx)),
        }
        .when_some(self.remote.skill_error.clone(), |d, e| {
            d.child(div().text_color(rgb(p.warning)).child(e))
        })
    }

    /// The skill that lets agents on this computer drive other computers:
    /// added to each agent that takes skills, or copied for any other.
    fn agent_skills(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().font_weight(gpui::FontWeight::MEDIUM).child("Drive other computers from here"))
            .child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(format!(
                "One skill lets agents here drive any computer you have a key for. It reads each computer's URL and key from {}, so one file serves several.",
                skill::HOSTS_FILE
            )))
            .children(Client::ALL.iter().filter(|&&c| self.clients[c as usize].skill_available).map(|&client| {
                let state = &self.clients[client as usize];
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(client.label()))
                    .child(div().w(px(105.0)).flex_none().child(if state.skill_installed {
                        status("Added", true)
                    } else {
                        status("Not added", false)
                    }))
                    .child(
                        Button::new(("add-skill", client as usize), if state.skill_installed { "Update skill" } else { "Add skill" })
                            .flex_none()
                            .on_click(cx.listener(move |s, _, _, cx| {
                                s.set_skill(client, true);
                                cx.notify();
                            })),
                    )
                    .when(state.skill_installed, |d| {
                        d.child(
                            Button::new(("remove-skill", client as usize), "Remove skill")
                                .flex_none()
                                .ghost()
                                .on_click(cx.listener(move |s, _, _, cx| {
                                    s.set_skill(client, false);
                                    cx.notify();
                                })),
                        )
                    })
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().text_color(rgb(p.text_dim)).child("Any other agent"))
                    .child(Button::new("preview-skill", if self.skill_preview.is_some() { "Hide preview" } else { "Preview" }).flex_none().on_click(cx.listener(|s, _, _, cx| {
                        s.skill_preview = match s.skill_preview {
                            Some(_) => None,
                            None => Some(skill::render_generic(&crate::tools::list())),
                        };
                        cx.notify();
                    })))
                    .child(Button::new("copy-generic", "Copy skill").flex_none().on_click(cx.listener(|s, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(skill::render_generic(&crate::tools::list())));
                        s.skill_message = Some("Copied the skill.".into());
                        cx.notify();
                    })))
                    .child(Button::new("save-generic", "Save skill").flex_none().on_click(cx.listener(|s, _, _, cx| {
                        let text = skill::render_generic(&crate::tools::list());
                        s.skill_message = Some(match remote::save_download(skill::GENERIC_SKILL_NAME, "SKILL.md", &text) {
                            Ok(path) => {
                                let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn();
                                format!("Saved {}.", path.display())
                            }
                            Err(e) => format!("Couldn't save: {e:#}"),
                        });
                        cx.notify();
                    }))),
            )
            .when_some(self.skill_preview.clone(), |d, text| {
                d.child(
                    div()
                        .id("generic-preview")
                        .max_h(px(260.0))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(px(6.0))
                        .bg(rgb(p.deep_bg))
                        .font_family("Menlo")
                        .text_size(px(11.0))
                        .child(text),
                )
            })
            .when_some(self.skill_message.clone(), |d, m| {
                d.child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(m))
            })
    }

    fn set_skill(&mut self, client: Client, add: bool) {
        let result = if add {
            skills::add(client).map(|_| {
                format!("Added the skill to {}. Start a new session to use it.", client.label())
            })
        } else {
            skills::remove(client).map(|()| format!("Removed the skill from {}.", client.label()))
        };
        self.skill_message = Some(result.unwrap_or_else(|e| format!("{e:#}")));
        self.clients = client_states();
    }

    fn remote_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let http = &self.config.http;
        let state = super::http::state();
        let dim = |t: String| div().text_color(rgb(p.text_dim)).child(t);
        let status_line = match (http.enabled, &state.listening, &state.error) {
            (false, _, _) => status("Off", false),
            (true, _, Some(e)) => div().text_color(rgb(p.warning)).child(e.clone()),
            (true, Some(addr), _) => status(
                &if remote::autostart::is_set() {
                    format!("Listening on {addr}; starts at login")
                } else {
                    format!("Listening on {addr}")
                },
                true,
            ),
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

        // A key Regenerate Key just made, beside the device list.
        if matches!(self.remote.panel, Some(Panel::Issued { regen: true, .. })) {
            section = section.child(self.issued_panel(cx));
        }
        section
            .when_some(self.remote.error.clone(), |d, e| {
                d.child(div().text_color(rgb(p.warning)).child(e))
            })
            .child(self.skill_section(cx))
            .child(Divider::horizontal())
            .child(self.agent_skills(cx))
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
                        None => {
                            let _ = std::process::Command::new("/usr/bin/open")
                                .arg(&up.page)
                                .spawn();
                        }
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
                            .on_click(move |_, _, _| {
                                let _ = std::process::Command::new("/usr/bin/open")
                                    .arg(&page)
                                    .spawn();
                            }),
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
        let all_granted = self.permissions.iter().all(|p| p.granted);
        let provider = self.config.recipe.provider;

        div()
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
                    .children(self.permissions.iter().enumerate().map(|(i, perm)| {
                        let name = perm.name.clone();
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
                                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child(perm.name.clone()))
                                    .child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(perm.help.clone())),
                            )
                            .child(if perm.granted {
                                status("Granted", true).into_any_element()
                            } else {
                                Button::new(("grant", i), "Grant…")
                                    .primary()
                                    .flex_none()
                                    .on_click(move |_, _, _| {
                                        ocu_macos::request_permissions();
                                        ocu_macos::open_settings(&name);
                                    })
                                    .into_any_element()
                            })
                    }))
                    .when(!all_granted, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().flex_1().min_w_0().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(
                                    "If OpenComputerUse is missing from the list in System Settings, press + there and add it (Show in Finder finds it). macOS applies Screen Recording after the app reopens.",
                                ))
                                .child(Button::new("reveal", "Show in Finder").flex_none().on_click(|_, _, _| reveal_app()))
                                .child(Button::new("reopen", "Reopen").flex_none().on_click(|_, _, cx| reopen(cx))),
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
                                            .child(format!("{} · pid {}", s.id, s.pid.unwrap_or(0))),
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
                    // Overlay.
                    .child(Self::section("While working"))
                    .child(
                        Checkbox::new("overlay", "Show a halo and cursor over windows being driven", self.config.show_overlay())
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
                                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child("Work while the Mac is locked"))
                                    .child(
                                        div()
                                            .text_color(rgb(p.text_dim))
                                            .text_size(px(11.5))
                                            .child(
                                                "Lets agents unlock the Mac to keep working. Asks for \
                                                 your password to set up. A key press or click relocks it.",
                                            ),
                                    ),
                            )
                            .child(if self.lock_busy {
                                status("Working…", false).into_any_element()
                            } else {
                                Checkbox::new("locked-use", "", self.config.allow_unlock && self.lock_installed)
                                    .on_change(cx.listener(|s, on: &bool, _, cx| s.set_locked_use(*on, cx)))
                                    .into_any_element()
                            }),
                    )
                    .when_some(self.lock_message.clone(), |d, m| {
                        d.child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(m))
                    })
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
            )
    }
}

pub fn open(service: Arc<Service>, cx: &mut App) -> Option<gpui::WindowHandle<Status>> {
    use gpui::{size, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
    let bounds = Bounds::centered(None, size(px(640.0), px(720.0)), cx);
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
