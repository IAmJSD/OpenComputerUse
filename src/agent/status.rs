//! The app's one window: whether computer use has the permissions it needs,
//! how to add it to an MCP client, which sessions are running, and the
//! settings for recipes and the overlay.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, rgb, App, AppContext as _, ClipboardItem, Context, ElementId, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
};

use ocu_core::{Permission, Service, SessionInfo};

use super::ui::{palette, Badge, Button, Checkbox, Divider, Heading, LineEdit, LineEditKey, TextInput, TextPress};
use crate::clients::{self, Client};
use crate::config::{Config, Provider};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    TypesafeKey,
    TypesafeModel,
    CloudflareAccount,
    CloudflareToken,
    CloudflareModel,
    MinConfidence,
}

impl Field {
    const ALL: [Field; 6] = [
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
    clients: [ClientState; 2],
    busy: Option<Client>,
    client_message: Option<String>,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct ClientState {
    found: bool,
    installed: bool,
}

const GOOD: u32 = 0x2E7D4F;

/// The .app this is running from, if it is.
fn bundle() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Into::into)
}

fn reveal_app() {
    if let Some(b) = bundle() {
        let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(b).spawn();
    }
}

/// Quits and opens again, which is when macOS applies a new Screen
/// Recording grant. Sessions end with the quit.
fn reopen(cx: &mut App) {
    if let Some(b) = bundle() {
        let script = format!("sleep 1; /usr/bin/open {:?}", b.display().to_string());
        let _ = std::process::Command::new("/bin/sh").args(["-c", &script]).spawn();
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
        .child(div().size(px(7.0)).flex_none().rounded_full().bg(rgb(if good { 0x3FB26B } else { p.text_faint })))
        .child(
            div()
                .whitespace_nowrap()
                .text_color(rgb(if good { p.text } else { p.text_dim }))
                .child(label.to_string()),
        )
}

fn client_states() -> [ClientState; 2] {
    Client::ALL.map(|c| ClientState { found: clients::find(c).is_some(), installed: clients::installed(c) })
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
            busy: None,
            client_message: None,
        };
        s.saved = true;
        s
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let permissions = self.service.platform().permissions();
        let sessions = self.service.sessions();
        let clients = client_states();
        let changed = permissions.iter().map(|p| p.granted).ne(self.permissions.iter().map(|p| p.granted))
            || sessions.iter().map(|s| &s.id).ne(self.sessions.iter().map(|s| &s.id))
            || clients != self.clients;
        self.permissions = permissions;
        self.sessions = sessions;
        self.clients = clients;
        if changed {
            cx.notify();
        }
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
                    Ok(()) if install => format!("Installed in {}. Start a new {} session to use it.", client.label(), client.label()),
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
        self.fields.iter().find(|(k, _)| *k == f).unwrap().1.text.trim().to_string()
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
        let [key, model, account, token, cf_model, confidence] = Field::ALL.map(|f| self.text(f));
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
            let next = Field::ALL[if ev.keystroke.modifiers.shift { (i + n - 1) % n } else { (i + 1) % n }];
            self.activate(next);
            self.field(next).select_all();
            cx.notify();
            cx.stop_propagation();
            return;
        }
        match self.field(f).key(ev, cx) {
            LineEditKey::Ignored => {}
            LineEditKey::Submitted => {
                self.save();
                cx.notify();
                cx.stop_propagation();
            }
            LineEditKey::Changed => {
                self.saved = false;
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
            .child(div().w(px(130.0)).flex_none().text_color(rgb(p.text_dim)).child(label.to_string()))
            .child(div().flex_1().min_w_0().child(child))
    }

    fn section(title: &str) -> impl IntoElement {
        div().pt_2().child(Heading::new(title.to_string()).uppercase())
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
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().flex_1().min_w_0().font_weight(gpui::FontWeight::MEDIUM).child(client.label()))
                            .child(div().w(px(110.0)).flex_none().child(match (state.found, state.installed, busy) {
                                (_, _, true) => status("Working…", false),
                                (false, _, _) => status("Not found", false),
                                (true, true, _) => status("Installed", true),
                                (true, false, _) => status("Not installed", false),
                            }))
                            .child(div().w(px(180.0)).flex_none().flex().justify_end().gap_2().when(state.found, |d| {
                                d.child(
                                    Button::new(("install", client as usize), if state.installed { "Reinstall" } else { "Install" })
                                        .when(!state.installed, |b| b.primary())
                                        .disabled(self.busy.is_some())
                                        .on_click(cx.listener(move |s, _, _, cx| s.install(client, true, cx))),
                                )
                                .when(state.installed, |d| {
                                    d.child(
                                        Button::new(("remove", client as usize), "Remove")
                                            .ghost()
                                            .disabled(self.busy.is_some())
                                            .on_click(cx.listener(move |s, _, _, cx| s.install(client, false, cx))),
                                    )
                                })
                            }))
                    }))
                    .when_some(self.client_message.clone(), |d, m| {
                        d.child(div().text_color(rgb(p.text_dim)).text_size(px(11.5)).child(m))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w_0().text_color(rgb(p.text_dim)).child("Other clients (Claude Desktop, Cursor, …)"))
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
    let bounds = Bounds::centered(None, size(px(560.0), px(720.0)), cx);
    super::native::set_regular(true);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("OpenComputerUse".into()), ..Default::default() }),
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
