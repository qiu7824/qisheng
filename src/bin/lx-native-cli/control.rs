use anyhow::{Context, Result, bail, ensure};
use lx_gpui_player::{
    backend::{BackendHandle, BackendOpenApiServer},
    open_api::{Authentication, BearerDigest, ServerConfig, ServerState, ServerView},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::IpAddr,
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartOptions {
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    bind_lan: bool,
    #[serde(default)]
    legacy_loopback: bool,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default)]
    allowed_lan_hosts: Vec<IpAddr>,
}
fn default_port() -> u16 {
    23330
}

pub(super) struct Services<'a> {
    backend: &'a BackendHandle,
    interactive: bool,
    server: Option<BackendOpenApiServer>,
    sync: super::cli_sync_control::SyncServices<'a>,
}
impl<'a> Services<'a> {
    pub(super) fn new(backend: &'a BackendHandle, interactive: bool) -> Self {
        Self {
            backend,
            interactive,
            server: None,
            sync: super::cli_sync_control::SyncServices::new(backend),
        }
    }
    pub(super) fn dispatch(&mut self, value: &Value) -> Result<Option<Value>> {
        ensure!(
            !(value.get("service").is_some()
                && (value.get("query").is_some() || value.get("command").is_some())),
            "Choose one query, command or endpoint service per request"
        );
        if value.get("query").and_then(Value::as_str) == Some("open_api") {
            return Ok(Some(self.view()));
        }
        if let Some(result) = self.sync.dispatch(value, self.interactive)? {
            return Ok(Some(result));
        }
        let Some(service) = value.get("service").and_then(Value::as_str) else {
            return Ok(None);
        };
        ensure!(self.interactive, "Endpoint services require --json-lines");
        match service {
            "open_api_start" | "open_api_rotate" => {
                let options: StartOptions = serde_json::from_value(
                    value.get("config").cloned().unwrap_or_else(|| json!({})),
                )
                .context("Invalid endpoint configuration")?;
                let mut generated_token = None;
                let authentication = if options.legacy_loopback {
                    ensure!(
                        !options.bind_lan && options.bearer_token.is_none(),
                        "Legacy authentication requires loopback without a bearer token"
                    );
                    Authentication::LegacyLoopback
                } else if let Some(token) = options.bearer_token {
                    Authentication::Bearer(BearerDigest::from_token(&token)?)
                } else {
                    let (token, digest) = BearerDigest::generate()?;
                    generated_token = Some(token);
                    Authentication::Bearer(digest)
                };
                let config = ServerConfig {
                    port: options.port,
                    bind_lan: options.bind_lan,
                    authentication,
                    allowed_origins: options.allowed_origins,
                    allowed_lan_hosts: options.allowed_lan_hosts,
                    session_epoch: 1,
                };
                config.validate()?;
                if service == "open_api_rotate" {
                    self.shutdown_open_api(Duration::from_secs(5))?;
                }
                ensure!(
                    self.server.is_none(),
                    "Stop or rotate the active endpoint first"
                );
                self.server = Some(self.backend.start_open_api(config)?);
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let view = self.server.as_ref().unwrap().view();
                    match view.state {
                        ServerState::Listening => {
                            let mut response = self.view();
                            if let Some(token) = generated_token {
                                response["bearer_token"] = token.into();
                            }
                            return Ok(Some(response));
                        }
                        ServerState::Failed => {
                            let message = view
                                .last_error
                                .map(|error| error.message)
                                .unwrap_or_else(|| "Endpoint startup failed".into());
                            self.shutdown_open_api(Duration::from_secs(5))?;
                            bail!("{message}");
                        }
                        _ => {
                            if Instant::now() >= deadline {
                                self.shutdown_open_api(Duration::from_secs(5))?;
                                bail!("Endpoint startup deadline exceeded");
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            "open_api_stop" => {
                self.shutdown_open_api(Duration::from_secs(5))?;
                Ok(Some(self.view()))
            }
            _ => bail!("Unknown endpoint service"),
        }
    }
    pub(super) fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        self.sync
            .shutdown(deadline.saturating_duration_since(Instant::now()))?;
        self.shutdown_open_api(deadline.saturating_duration_since(Instant::now()))
    }
    fn shutdown_open_api(&mut self, timeout: Duration) -> Result<()> {
        if let Some(server) = &mut self.server {
            ensure!(
                server.shutdown(timeout)?,
                "Endpoint workers have not joined"
            );
        }
        self.server = None;
        Ok(())
    }
    fn view(&self) -> Value {
        self.server.as_ref().map(|server|view_value(server.view()))
            .unwrap_or_else(||json!({"state":"disabled","address":null,"session_epoch":null,"connections":0,"subscriptions":0,"error":null}))
    }
}
fn view_value(view: ServerView) -> Value {
    let state = match view.state {
        ServerState::Disabled => "disabled",
        ServerState::Starting => "starting",
        ServerState::Listening => "listening",
        ServerState::Stopping => "stopping",
        ServerState::Failed => "failed",
    };
    json!({"state":state,"address":view.address.map(|address|address.to_string()),"session_epoch":view.session_epoch,
        "connections":view.connections,"subscriptions":view.subscriptions,"error":view.last_error.map(|error|error.message)})
}
