use anyhow::{Context, Result, bail, ensure};
use lx_gpui_player::{
    backend::{BackendHandle, BackendSyncNetwork},
    sync::{
        Dataset, PeerCapabilities, SyncMode,
        network::{ClientConfig, NetworkState, PairingCode, PeerEndpoint, ServerConfig},
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerOptions {
    #[serde(default)]
    port: u16,
    #[serde(default)]
    bind_lan: bool,
    #[serde(default = "loopback")]
    address: IpAddr,
    server_id: String,
    server_name: String,
    #[serde(default)]
    pairing_code: Option<String>,
    pairing_epoch: u64,
    #[serde(default = "enabled")]
    list: bool,
    #[serde(default = "enabled")]
    dislike: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientOptions {
    url: String,
    pinned_address: SocketAddr,
    device_name: String,
    #[serde(default)]
    pairing_code: Option<String>,
    #[serde(default)]
    server_id: Option<String>,
    pairing_epoch: u64,
    #[serde(default = "enabled")]
    list: bool,
    #[serde(default = "enabled")]
    dislike: bool,
    #[serde(default)]
    reconnect_limit: u32,
}
fn enabled() -> bool {
    true
}
fn loopback() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModeOptions {
    owner_epoch: u64,
    peer_id: String,
    auth_epoch: u64,
    session_epoch: u64,
    dataset: Dataset,
    mode: SyncMode,
}
pub(super) struct SyncServices<'a> {
    backend: &'a BackendHandle,
    network: Option<BackendSyncNetwork>,
}
impl<'a> SyncServices<'a> {
    pub(super) fn new(backend: &'a BackendHandle) -> Self {
        Self {
            backend,
            network: None,
        }
    }
    pub(super) fn dispatch(&mut self, value: &Value, interactive: bool) -> Result<Option<Value>> {
        match value.get("query").and_then(Value::as_str) {
            Some("sync") => return Ok(Some(self.view())),
            Some("sync_peers") => {
                let peers=self.backend.sync_peers()?.into_iter().map(|peer|json!({"peer_id":peer.peer_id,"client_id":peer.client_id,"device_name":peer.device_name,"is_mobile":peer.is_mobile,"auth_epoch":peer.auth_epoch,"owner_epoch":peer.owner_epoch,"session_epoch":peer.session_epoch,"capabilities":{"list":peer.capabilities.list,"dislike":peer.capabilities.dislike},"list_mode":peer.list_mode,"dislike_mode":peer.dislike_mode})).collect::<Vec<_>>();
                return Ok(Some(Value::Array(peers)));
            }
            _ => {}
        }
        let Some(service) = value.get("service").and_then(Value::as_str) else {
            return Ok(None);
        };
        if !service.starts_with("sync_") {
            return Ok(None);
        }
        ensure!(interactive, "Sync services require --json-lines");
        match service {
            "sync_server_start" => {
                ensure!(
                    self.network.is_none(),
                    "Stop the active sync endpoint first"
                );
                let options: ServerOptions = serde_json::from_value(
                    value
                        .get("config")
                        .cloned()
                        .context("Sync server config missing")?,
                )?;
                ensure!(
                    options.bind_lan || options.address.is_loopback(),
                    "LAN sync requires explicit enable"
                );
                let generated = options.pairing_code.is_none();
                let code = options
                    .pairing_code
                    .as_deref()
                    .map(PairingCode::new)
                    .transpose()?
                    .map_or_else(PairingCode::generate, Ok)?;
                let exposed = generated.then(|| code.expose().to_owned());
                let mut config = ServerConfig::loopback(
                    options.server_id,
                    options.server_name,
                    code,
                    options.pairing_epoch,
                );
                config.listen_address = SocketAddr::new(options.address, options.port);
                config.bind_lan = options.bind_lan;
                config.capabilities = PeerCapabilities {
                    list: options.list.then_some(false),
                    dislike: options.dislike.then_some(false),
                };
                self.network = Some(self.backend.start_sync_server(config)?);
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let view = self.network.as_ref().unwrap().view();
                    if view.state == NetworkState::Listening {
                        let mut result = self.view();
                        if let Some(code) = exposed {
                            result["pairing_code"] = code.into();
                        }
                        return Ok(Some(result));
                    }
                    if let Some(error) = view.last_error {
                        self.shutdown(Duration::from_secs(5))?;
                        bail!("{}", error.message);
                    }
                    if Instant::now() >= deadline {
                        self.shutdown(Duration::from_secs(5))?;
                        bail!("Sync listener startup deadline exceeded");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            "sync_client_start" => {
                ensure!(
                    self.network.is_none(),
                    "Stop the active sync endpoint first"
                );
                let options: ClientOptions = serde_json::from_value(
                    value
                        .get("config")
                        .cloned()
                        .context("Sync client config missing")?,
                )?;
                ensure!(
                    options.pairing_code.is_some() != options.server_id.is_some(),
                    "Choose an initial pairing code or a saved server id"
                );
                let config = ClientConfig {
                    endpoint: PeerEndpoint {
                        url: options.url,
                        pinned_address: options.pinned_address,
                        tls: None,
                    },
                    device_name: options.device_name,
                    credentials: None,
                    pairing_code: options
                        .pairing_code
                        .as_deref()
                        .map(PairingCode::new)
                        .transpose()?,
                    pairing_epoch: options.pairing_epoch,
                    capabilities: PeerCapabilities {
                        list: options.list.then_some(false),
                        dislike: options.dislike.then_some(false),
                    },
                    reconnect_limit: options.reconnect_limit,
                };
                self.network = Some(if let Some(server_id) = options.server_id {
                    self.backend.start_sync_client_saved(config, server_id)?
                } else {
                    self.backend.start_sync_client(config)?
                });
                Ok(Some(self.view()))
            }
            "sync_stop" => {
                self.shutdown(Duration::from_secs(5))?;
                Ok(Some(self.view()))
            }
            "sync_revoke_peer" => {
                let id = value
                    .get("peer_id")
                    .and_then(Value::as_str)
                    .context("Peer id missing")?;
                self.network
                    .as_ref()
                    .context("Sync endpoint is disabled")?
                    .revoke(id)?;
                Ok(Some(self.view()))
            }
            "sync_choose_mode" => {
                let options: ModeOptions = serde_json::from_value(
                    value
                        .get("config")
                        .cloned()
                        .context("Sync mode config missing")?,
                )?;
                self.backend.choose_sync_mode(
                    options.owner_epoch,
                    options.peer_id,
                    options.auth_epoch,
                    options.session_epoch,
                    options.dataset,
                    options.mode,
                )?;
                Ok(Some(json!({"committed":true})))
            }
            _ => bail!("Unknown sync service"),
        }
    }
    pub(super) fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        if let Some(network) = &mut self.network {
            ensure!(
                network.shutdown(timeout)?,
                "Sync endpoint workers have not joined"
            );
        }
        self.network = None;
        Ok(())
    }
    fn view(&self) -> Value {
        self.network.as_ref().map(|network|{let view=network.view();json!({"state":format!("{:?}",view.state).to_lowercase(),"address":view.address.map(|address|address.to_string()),"owner_epoch":network.epoch(),"connections":view.connections,"peers":view.peers,"pending_rpc":view.pending_rpc,"reserved_frame_bytes":view.reserved_frame_bytes,"workspace_bytes":view.workspace_bytes,"error":view.last_error.map(|error|error.message)})}).unwrap_or_else(||json!({"state":"disabled","address":null,"owner_epoch":null,"connections":0,"peers":0,"pending_rpc":0,"reserved_frame_bytes":0,"workspace_bytes":0,"error":null}))
    }
}
