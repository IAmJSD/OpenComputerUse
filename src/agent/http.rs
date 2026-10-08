//! Keeps the HTTP server in step with the settings: started, stopped or
//! moved when they change, whether from the window or the MCP server's
//! http_server tool, and following the chosen adapters' addresses as they
//! change; and the login item that brings it back after a reboot put back
//! if it has gone missing.

use std::sync::{Arc, Mutex};

use ocu_core::Service;

use crate::config::Config;
use crate::remote::interfaces;
use crate::remote::server::HttpServer;

/// What the window shows about the server.
#[derive(Clone, Default)]
pub struct HttpState {
    pub listening: Option<String>,
    pub error: Option<String>,
}

pub static STATE: Mutex<HttpState> = Mutex::new(HttpState {
    listening: None,
    error: None,
});

pub fn state() -> HttpState {
    STATE.lock().unwrap().clone()
}

#[derive(Default)]
pub struct HttpHost {
    server: Option<HttpServer>,
}

impl HttpHost {
    pub fn sync(&mut self, service: &Arc<Service>) {
        let http = Config::load().http;
        if !http.enabled {
            if self.server.take().is_some() {
                *STATE.lock().unwrap() = HttpState::default();
            }
            return;
        }
        let server = self.server.get_or_insert_with(|| {
            #[cfg(target_os = "macos")]
            if !crate::remote::autostart::is_set() {
                if let Err(e) = crate::remote::autostart::set(true) {
                    log::warn!("start at login: {e:#}");
                }
            }
            HttpServer::new(service.clone())
        });
        let (addrs, mut errors) = match interfaces::listen_addrs(&http) {
            Ok(addrs) => (addrs, Vec::new()),
            Err(e) => (Vec::new(), vec![format!("{e:#}")]),
        };
        errors.extend(server.listen_on(&addrs));
        let listening = server.addrs();
        let mut state = HttpState {
            listening: (!listening.is_empty()).then(|| interfaces::summary(&listening, 1)),
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        };
        if listening.is_empty() && errors.is_empty() {
            state.error = Some(format!(
                "Nothing in {} has an address now; it listens once something does.",
                http.listen_on.join(", ")
            ));
        }
        let mut current = STATE.lock().unwrap();
        if (&current.listening, &current.error) != (&state.listening, &state.error) {
            if let Some(e) = &state.error {
                log::warn!("HTTP server: {e}");
            }
            *current = state;
        }
    }
}
