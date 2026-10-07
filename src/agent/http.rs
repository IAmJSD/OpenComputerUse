//! Keeps the HTTP server in step with the settings: started, stopped or
//! moved when they change, whether from the window or the MCP server's
//! http_server tool, and the login item that brings it back after a reboot
//! put back if it has gone missing.

use std::sync::{Arc, Mutex};

use ocu_core::Service;

use crate::config::Config;
use crate::remote::server::HttpServer;

/// What the window shows about the server.
#[derive(Clone, Default)]
pub struct HttpState {
    pub listening: Option<String>,
    pub error: Option<String>,
}

pub static STATE: Mutex<HttpState> = Mutex::new(HttpState { listening: None, error: None });

pub fn state() -> HttpState {
    STATE.lock().unwrap().clone()
}

#[derive(Default)]
pub struct HttpHost {
    server: Option<HttpServer>,
    /// What the running (or failed) server was started with.
    applied: Option<(String, u16)>,
}

impl HttpHost {
    pub fn sync(&mut self, service: &Arc<Service>) {
        let http = Config::load().http;
        let want = http.enabled.then(|| (http.bind.clone(), http.port));
        if want == self.applied {
            return;
        }
        // Stopping first frees the port for a restart on the same one.
        self.server = None;
        self.applied = want.clone();
        let mut state = STATE.lock().unwrap();
        *state = HttpState::default();
        let Some((bind, port)) = want else { return };
        #[cfg(target_os = "macos")]
        if !crate::remote::autostart::is_set() {
            if let Err(e) = crate::remote::autostart::set(true) {
                log::warn!("start at login: {e:#}");
            }
        }
        match HttpServer::start(service.clone(), &bind, port) {
            Ok(server) => {
                state.listening = Some(server.addr.clone());
                self.server = Some(server);
            }
            Err(e) => state.error = Some(format!("{e:#}")),
        }
    }
}
