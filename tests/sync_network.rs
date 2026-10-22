//! Real isolated loopback TCP/WebSocket fixtures with a protocol-only store.
//! SQLite durability and fixed upstream peers are separate integration checks.
use lx_gpui_player::sync::network::*;
use lx_gpui_player::sync::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

static CASES: Mutex<()> = Mutex::new(());
fn song(id: &str) -> MusicInfo {
    MusicInfo::from_wire(json!({"id":id,"name":id,"singer":"Fixture","source":"kw","interval":null,"meta":{"songId":id,"albumName":"","qualitys":[],"_qualitys":{}}})).unwrap()
}
fn lists(ids: &[&str]) -> ListData {
    ListData {
        default_list: ids.iter().map(|id| song(id)).collect(),
        love_list: Vec::new(),
        user_list: Vec::new(),
    }
}
#[derive(Clone)]
struct Entry {
    delta: SyncDelta,
    origin: Option<String>,
}
struct State {
    lists: ListData,
    dislike: String,
    sequence: u64,
    revision: u64,
    peers: HashMap<String, PeerRecord>,
    tickets: HashMap<String, SessionTicket>,
    baselines: HashMap<(String, u8), SyncBaseline>,
    journal: Vec<Entry>,
}
struct Store {
    state: Mutex<State>,
    gate: Mutex<Option<Arc<Gate>>>,
}
struct Gate {
    entered: AtomicBool,
    release: Mutex<bool>,
    wake: Condvar,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicBool::new(false),
            release: Mutex::new(false),
            wake: Condvar::new(),
        })
    }
    fn open(&self) {
        *self.release.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.open();
    }
}
impl Store {
    fn new(ids: &[&str], dislike: &str) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                lists: lists(ids),
                dislike: dislike.into(),
                sequence: 0,
                revision: 0,
                peers: HashMap::new(),
                tickets: HashMap::new(),
                baselines: HashMap::new(),
                journal: Vec::new(),
            }),
            gate: Mutex::new(None),
        })
    }
    fn check(state: &State, ticket: &SessionTicket) -> SyncResult<()> {
        if !ticket.is_active()
            || state
                .peers
                .get(&ticket.peer_id)
                .is_none_or(|peer| peer.auth_epoch != ticket.auth_epoch)
            || state
                .tickets
                .get(&ticket.peer_id)
                .is_none_or(|active| active.session_epoch != ticket.session_epoch)
        {
            return Err(SyncError::new(
                SyncErrorKind::Cancelled,
                "Retired fixture ticket",
            ));
        }
        Ok(())
    }
    fn snapshot_of(state: &State) -> SyncSnapshot {
        SyncSnapshot {
            lists: state.lists.clone(),
            dislike: state.dislike.clone(),
            repository_revision: state.revision,
            sequence: state.sequence,
        }
    }
    fn has(&self, id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .lists
            .default_list
            .iter()
            .any(|music| music.id() == id)
    }
    fn local_add(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        let music = song(id);
        if state
            .lists
            .default_list
            .iter()
            .any(|music| music.id() == id)
        {
            return;
        }
        state.lists.default_list.insert(0, music.clone());
        state.sequence += 1;
        state.revision += 1;
        let sequence = state.sequence;
        state.journal.push(Entry{origin:None,delta:SyncDelta{sequence,dataset:Dataset::List,action:json!({"action":"list_music_add","data":{"id":"default","musicInfos":[music.wire()],"addMusicLocationType":"top"}})}});
    }
    fn local_dislike(&self, rules: &str) {
        let mut state = self.state.lock().unwrap();
        state.dislike = rules.into();
        state.sequence += 1;
        state.revision += 1;
        let sequence = state.sequence;
        state.journal.push(Entry {
            origin: None,
            delta: SyncDelta {
                sequence,
                dataset: Dataset::Dislike,
                action: json!({"action":"dislike_data_overwrite","data":rules}),
            },
        });
    }
    fn credentials(&self) -> ClientCredentials {
        let state = self.state.lock().unwrap();
        let peer = state.peers.values().next().unwrap();
        ClientCredentials {
            server_id: peer.peer_id.clone(),
            client_id: peer.client_id.clone(),
            key: peer.key.clone(),
            server_name: peer.device_name.clone(),
            auth_epoch: peer.auth_epoch,
        }
    }
}
fn dataset_id(dataset: Dataset) -> u8 {
    if dataset == Dataset::List { 0 } else { 1 }
}
impl SyncBackend for Store {
    fn peers(&self) -> SyncResult<Vec<PeerRecord>> {
        Ok(self.state.lock().unwrap().peers.values().cloned().collect())
    }
    fn store_peer(&self, epoch: u64, peer: PeerRecord) -> SyncResult<()> {
        if epoch != 7 {
            return Err(SyncError::new(
                SyncErrorKind::Cancelled,
                "Wrong pairing epoch",
            ));
        }
        self.state
            .lock()
            .unwrap()
            .peers
            .insert(peer.peer_id.clone(), peer);
        Ok(())
    }
    fn begin_session(&self, ticket: &SessionTicket) -> SyncResult<()> {
        let mut state = self.state.lock().unwrap();
        if !ticket.is_active()
            || state
                .peers
                .get(&ticket.peer_id)
                .is_none_or(|peer| peer.auth_epoch != ticket.auth_epoch)
        {
            return Err(SyncError::new(
                SyncErrorKind::Authentication,
                "Unknown peer",
            ));
        }
        state.tickets.insert(ticket.peer_id.clone(), ticket.clone());
        Ok(())
    }
    fn retire_session(&self, ticket: &SessionTicket) {
        ticket.retire();
        let mut state = self.state.lock().unwrap();
        if state
            .tickets
            .get(&ticket.peer_id)
            .is_some_and(|active| active.session_epoch == ticket.session_epoch)
        {
            state.tickets.remove(&ticket.peer_id);
        }
    }
    fn snapshot(&self, ticket: &SessionTicket) -> SyncResult<SyncSnapshot> {
        let state = self.state.lock().unwrap();
        Self::check(&state, ticket)?;
        Ok(Self::snapshot_of(&state))
    }
    fn baseline(
        &self,
        ticket: &SessionTicket,
        dataset: Dataset,
    ) -> SyncResult<Option<SyncBaseline>> {
        let state = self.state.lock().unwrap();
        Self::check(&state, ticket)?;
        Ok(state
            .baselines
            .get(&(ticket.peer_id.clone(), dataset_id(dataset)))
            .cloned())
    }
    fn apply_remote(
        &self,
        ticket: &SessionTicket,
        expected: Option<u64>,
        operation: SyncOperation,
    ) -> SyncResult<SyncReceipt> {
        let gate = { self.gate.lock().unwrap().clone() };
        if let Some(gate) = gate {
            gate.entered.store(true, Ordering::Release);
            let mut release = gate.release.lock().unwrap();
            while !*release {
                release = gate.wake.wait(release).unwrap();
            }
        }
        let mut state = self.state.lock().unwrap();
        Self::check(&state, ticket)?;
        if expected.is_some_and(|expected| expected != state.sequence) {
            return Err(SyncError::new(
                SyncErrorKind::Cancelled,
                "Fixture sync sequence conflict",
            ));
        }
        let (dataset, action) = match operation {
            SyncOperation::Replace(SyncData::List(data)) => {
                state.lists = data;
                (
                    Dataset::List,
                    json!({"action":"list_data_overwrite","data":state.lists.wire()}),
                )
            }
            SyncOperation::Replace(SyncData::Dislike(rules))
            | SyncOperation::Dislike(DislikeAction::DataOverwrite(rules)) => {
                state.dislike = rules;
                (
                    Dataset::Dislike,
                    json!({"action":"dislike_data_overwrite","data":state.dislike}),
                )
            }
            SyncOperation::List(ListAction::MusicAdd {
                id,
                music,
                location,
            }) if id == "default" => {
                let values = music
                    .iter()
                    .map(MusicInfo::wire)
                    .cloned()
                    .collect::<Vec<_>>();
                for item in music {
                    if state
                        .lists
                        .default_list
                        .iter()
                        .any(|existing| existing.id() == item.id())
                    {
                        continue;
                    }
                    match location {
                        AddLocation::Top => state.lists.default_list.insert(0, item),
                        AddLocation::Bottom => state.lists.default_list.push(item),
                    }
                }
                (
                    Dataset::List,
                    json!({"action":"list_music_add","data":{"id":id,"musicInfos":values,"addMusicLocationType":location}}),
                )
            }
            _ => {
                return Err(SyncError::new(
                    SyncErrorKind::Unsupported,
                    "Fixture action is not implemented",
                ));
            }
        };
        state.sequence += 1;
        state.revision += 1;
        let sequence = state.sequence;
        state.journal.push(Entry {
            origin: Some(ticket.peer_id.clone()),
            delta: SyncDelta {
                sequence,
                dataset,
                action,
            },
        });
        let digest = Self::snapshot_of(&state).data(dataset).digest()?;
        Ok(SyncReceipt {
            repository_revision: state.revision,
            sequence,
            digest,
        })
    }
    fn confirm_baseline(
        &self,
        ticket: &SessionTicket,
        data: SyncData,
        revision: u64,
        sequence: u64,
        digest: &str,
    ) -> SyncResult<()> {
        let mut state = self.state.lock().unwrap();
        Self::check(&state, ticket)?;
        if sequence > state.sequence || data.digest()? != digest {
            return Err(SyncError::new(
                SyncErrorKind::InvalidData,
                "Invalid fixture ACK cut",
            ));
        }
        let dataset = data.dataset();
        state.baselines.insert(
            (ticket.peer_id.clone(), dataset_id(dataset)),
            SyncBaseline {
                data,
                repository_revision: revision,
                sequence,
                digest: digest.into(),
            },
        );
        Ok(())
    }
    fn changes_after(
        &self,
        ticket: &SessionTicket,
        sequence: u64,
        limit: usize,
    ) -> SyncResult<SyncChanges> {
        let state = self.state.lock().unwrap();
        Self::check(&state, ticket)?;
        let records = state
            .journal
            .iter()
            .filter(|entry| {
                entry.delta.sequence > sequence && entry.origin.as_ref() != Some(&ticket.peer_id)
            })
            .map(|entry| entry.delta.clone())
            .collect::<Vec<_>>();
        if records.len() > limit {
            return Ok(SyncChanges::NeedsSnapshot);
        }
        Ok(SyncChanges::Delta {
            records,
            snapshot: Self::snapshot_of(&state),
        })
    }
    fn select_mode(&self, ticket: &SessionTicket, _: Dataset) -> SyncResult<Option<SyncMode>> {
        Self::check(&self.state.lock().unwrap(), ticket)?;
        Ok(Some(SyncMode::MergeLocalRemote))
    }
    fn revoke_peer(&self, id: &str) -> SyncResult<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(ticket) = state.tickets.remove(id) {
            ticket.retire();
        }
        state.peers.remove(id);
        Ok(())
    }
}
fn wait(timeout: Duration, predicate: impl Fn() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(start.elapsed() < timeout, "network fixture deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn server_config() -> ServerConfig {
    let mut config = ServerConfig::loopback(
        "fixture-server".into(),
        "Fixture".into(),
        PairingCode::new("123456").unwrap(),
        7,
    );
    config.listen_address = SocketAddr::from(([127, 0, 0, 1], 0));
    config
}
fn start_server(store: Arc<Store>) -> SyncNetwork {
    let server = SyncNetwork::start_server(server_config(), store).unwrap();
    wait(Duration::from_secs(3), || {
        server.view().state == NetworkState::Listening
    });
    server
}
fn client_config(address: SocketAddr) -> ClientConfig {
    ClientConfig {
        endpoint: PeerEndpoint {
            url: format!("http://{address}"),
            pinned_address: address,
            tls: None,
        },
        device_name: "NativeFixture".into(),
        credentials: None,
        pairing_code: Some(PairingCode::new("123456").unwrap()),
        pairing_epoch: 7,
        capabilities: PeerCapabilities {
            list: Some(false),
            dislike: Some(false),
        },
        reconnect_limit: 2,
    }
}
fn stop(network: &mut SyncNetwork) {
    let address = network.view().address;
    assert!(network.shutdown(Duration::from_secs(5)).unwrap());
    let view = network.view();
    assert_eq!(view.connections, 0);
    assert_eq!(view.peers, 0);
    assert_eq!(view.pending_rpc, 0);
    assert_eq!(view.reserved_frame_bytes, 0);
    assert_eq!(view.workspace_bytes, 0);
    assert_eq!(view.state, NetworkState::Disabled);
    if let Some(address) = address {
        drop(TcpListener::bind(address).unwrap());
    }
}
fn connected(server: &SyncNetwork, client: &SyncNetwork) {
    let start = Instant::now();
    let mut server_events = std::collections::VecDeque::new();
    let mut client_events = std::collections::VecDeque::new();
    loop {
        for (network, events) in [(server, &mut server_events), (client, &mut client_events)] {
            while let Some(event) = network.try_recv() {
                if events.len() == 64 {
                    events.pop_front();
                }
                events.push_back(event);
            }
        }
        let server_view = server.view();
        let client_view = client.view();
        if server_view.state == NetworkState::Synced && client_view.state == NetworkState::Synced {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(25),
            "network fixture deadline; server={server_view:?}; client={client_view:?}; server_events={server_events:?}; client_events={client_events:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn two_actual_native_peers_pair_merge_and_exchange_incremental_datasets() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Store::new(&["server"], "left");
    let right = Store::new(&["client"], "right");
    let mut server = start_server(left.clone());
    let mut client =
        SyncNetwork::start_client(client_config(server.view().address.unwrap()), right.clone())
            .unwrap();
    connected(&server, &client);
    assert!(left.has("client") && right.has("server"));
    assert_eq!(left.state.lock().unwrap().dislike, "left\nright");
    assert_eq!(right.state.lock().unwrap().dislike, "left\nright");
    left.local_add("later-server");
    right.local_add("later-client");
    wait(Duration::from_secs(6), || {
        left.has("later-client") && right.has("later-server")
    });
    left.local_dislike("new-rule");
    wait(Duration::from_secs(6), || {
        right.state.lock().unwrap().dislike == "new-rule"
    });
    stop(&mut client);
    stop(&mut server);
}
#[test]
fn saved_key_authenticates_and_bad_key_has_no_dataset_effect() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Store::new(&["server"], "");
    let right = Store::new(&[], "");
    let mut server = start_server(left);
    let address = server.view().address.unwrap();
    let mut client = SyncNetwork::start_client(client_config(address), right.clone()).unwrap();
    connected(&server, &client);
    let credentials = right.credentials();
    stop(&mut client);
    let mut config = client_config(address);
    config.credentials = Some(credentials.clone());
    config.pairing_code = None;
    let mut reconnect = SyncNetwork::start_client(config.clone(), right.clone()).unwrap();
    connected(&server, &reconnect);
    stop(&mut reconnect);
    config.credentials.as_mut().unwrap().key = SharedKey::from_bytes([0; 16]);
    let mut bad = SyncNetwork::start_client(config, right.clone()).unwrap();
    wait(Duration::from_secs(6), || {
        bad.view().state == NetworkState::Degraded
    });
    assert_eq!(right.state.lock().unwrap().lists.default_list.len(), 1);
    stop(&mut bad);
    stop(&mut server);
}
#[test]
fn local_metadata_reference_crosses_real_socket_without_constructing_a_grant() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Store::new(&[], "");
    let wire = json!({"id":"opaque-id","name":"Opaque","singer":"","source":"local","interval":null,"meta":{"songId":"D:/opaque/not-opened.wav","albumName":"","filePath":"D:/opaque/not-opened.wav","ext":"wav"}});
    left.state
        .lock()
        .unwrap()
        .lists
        .default_list
        .push(MusicInfo::from_wire(wire.clone()).unwrap());
    let right = Store::new(&[], "");
    let mut server = start_server(left);
    let mut client =
        SyncNetwork::start_client(client_config(server.view().address.unwrap()), right.clone())
            .unwrap();
    connected(&server, &client);
    assert_eq!(
        right.state.lock().unwrap().lists.default_list[0].wire(),
        &wire
    );
    assert!(right.state.lock().unwrap().lists.default_list[0].is_local_reference());
    stop(&mut client);
    stop(&mut server);
}
#[test]
fn malformed_auth_browser_headers_and_duplicate_fields_do_not_pair() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let store = Store::new(&[], "");
    let mut server = start_server(store.clone());
    let address = server.view().address.unwrap();
    for request in [
        format!("GET /ah HTTP/1.1\r\nHost: {address}\r\nm: invalid\r\n\r\n"),
        format!("GET /ah HTTP/1.1\r\nHost: {address}\r\nm: invalid\r\nm: duplicate\r\n\r\n"),
        format!("GET /ah HTTP/1.1\r\nHost: {address}\r\nOrigin: https://evil.example\r\n\r\n"),
    ] {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        assert!(!String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 200"));
    }
    assert!(store.state.lock().unwrap().peers.is_empty());
    stop(&mut server);
}
#[test]
fn socket_admission_includes_listener_and_stop_actually_releases_every_handle() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let mut server = start_server(Store::new(&[], ""));
    let address = server.view().address.unwrap();
    let streams = (0..15)
        .map(|_| TcpStream::connect(address).unwrap())
        .collect::<Vec<_>>();
    wait(Duration::from_secs(2), || server.view().connections == 16);
    assert!(server.view().peers <= 8);
    stop(&mut server);
    for mut stream in streams {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0; 1];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    }
}
#[test]
fn blocked_real_backend_job_keeps_shutdown_owned_and_late_apply_is_cancelled() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Store::new(&["source"], "");
    let right = Store::new(&[], "");
    let gate = Gate::new();
    let _release = Release(gate.clone());
    *right.gate.lock().unwrap() = Some(gate.clone());
    let mut server = start_server(left);
    let mut client =
        SyncNetwork::start_client(client_config(server.view().address.unwrap()), right.clone())
            .unwrap();
    wait(Duration::from_secs(25), || {
        gate.entered.load(Ordering::Acquire)
    });
    assert!(!client.shutdown(Duration::from_millis(20)).unwrap());
    assert!(!client.is_finished());
    assert_eq!(client.view().state, NetworkState::Retiring);
    gate.open();
    assert!(client.shutdown(Duration::from_secs(5)).unwrap());
    assert!(!right.has("source"));
    stop(&mut server);
}
#[test]
fn actual_peer_revoke_closes_its_connection_and_rejects_late_saved_key() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Store::new(&["a"], "");
    let right = Store::new(&[], "");
    let mut server = start_server(left.clone());
    let address = server.view().address.unwrap();
    let mut client = SyncNetwork::start_client(client_config(address), right.clone()).unwrap();
    connected(&server, &client);
    let peer = left
        .state
        .lock()
        .unwrap()
        .peers
        .values()
        .next()
        .unwrap()
        .peer_id
        .clone();
    let credentials = right.credentials();
    server.revoke(&peer).unwrap();
    wait(Duration::from_secs(3), || server.view().peers == 0);
    stop(&mut client);
    let mut config = client_config(address);
    config.pairing_code = None;
    config.credentials = Some(credentials);
    let mut rejected = SyncNetwork::start_client(config, right).unwrap();
    wait(Duration::from_secs(6), || {
        rejected.view().state == NetworkState::Degraded
    });
    stop(&mut rejected);
    stop(&mut server);
}
#[test]
fn strict_tls_uses_pinned_tcp_but_preserves_hostname_validation() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = certified.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let server_tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key.into())
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate).unwrap();
    let client_tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut cfg = server_config();
    cfg.tls = Some(Arc::new(server_tls));
    let mut server = SyncNetwork::start_server(cfg, Store::new(&["tls"], "")).unwrap();
    wait(Duration::from_secs(3), || {
        server.view().state == NetworkState::Listening
    });
    let address = server.view().address.unwrap();
    let right = Store::new(&[], "");
    let mut config = client_config(address);
    config.endpoint.url = format!("https://localhost:{}", address.port());
    config.endpoint.tls = Some(client_tls.clone());
    let mut client = SyncNetwork::start_client(config.clone(), right.clone()).unwrap();
    connected(&server, &client);
    assert!(right.has("tls"));
    stop(&mut client);
    config.endpoint.url = format!("https://wrong-name.example:{}", address.port());
    let mut wrong = SyncNetwork::start_client(config, Store::new(&[], "")).unwrap();
    wait(Duration::from_secs(6), || {
        wrong.view().state == NetworkState::Degraded
    });
    stop(&mut wrong);
    stop(&mut server);
}

#[test]
fn full_replace_cas_does_not_erase_a_local_change_made_during_network_wait() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Store::new(&["remote-source"], "");
    let right = Store::new(&[], "");
    let gate = Gate::new();
    let _release = Release(gate.clone());
    *right.gate.lock().unwrap() = Some(gate.clone());
    let mut server = start_server(left);
    let mut client =
        SyncNetwork::start_client(client_config(server.view().address.unwrap()), right.clone())
            .unwrap();
    wait(Duration::from_secs(25), || {
        gate.entered.load(Ordering::Acquire)
    });
    right.local_add("local-s-plus-one");
    gate.open();
    wait(Duration::from_secs(6), || {
        client.is_finished() || client.view().state == NetworkState::Degraded
    });
    assert!(right.has("local-s-plus-one"));
    assert!(!right.has("remote-source"));
    stop(&mut client);
    stop(&mut server);
}

#[test]
fn real_authenticated_slow_peer_keeps_bounded_frames_and_is_physically_closed() {
    use tokio_tungstenite::tungstenite::{self, Message};
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let store = Store::new(&[], "");
    let key = SharedKey::from_bytes([7; 16]);
    let peer = PeerRecord {
        peer_id: "slow-peer".into(),
        client_id: "slow-peer".into(),
        key: key.clone(),
        device_name: "SlowFixture".into(),
        is_mobile: false,
        auth_epoch: 7,
        capabilities: PeerCapabilities {
            list: Some(false),
            dislike: None,
        },
    };
    store
        .state
        .lock()
        .unwrap()
        .peers
        .insert(peer.peer_id.clone(), peer);
    let mut seed = 0x12345678u32;
    for id in 0..200 {
        let mut wire = song(&format!("large-{id}")).wire().clone();
        let title = (0..16000)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                char::from(33 + (seed >> 24) as u8 % 90)
            })
            .collect::<String>();
        wire["name"] = Value::String(title);
        store
            .state
            .lock()
            .unwrap()
            .lists
            .default_list
            .push(MusicInfo::from_wire(wire).unwrap());
    }
    let mut server = start_server(store);
    let address = server.view().address.unwrap();
    let stream = TcpStream::connect(address).unwrap();
    small_receive_window(&stream);
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut url = reqwest::Url::parse(&format!("ws://{address}/socket")).unwrap();
    url.query_pairs_mut()
        .append_pair("i", "slow-peer")
        .append_pair("t", &aes_encrypt(CONNECT_MESSAGE.as_bytes(), &key).unwrap());
    let (mut socket, _) = tungstenite::client(url.as_str(), stream).unwrap();
    let mut reads = 0;
    loop {
        let Message::Text(text) = socket.read().unwrap() else {
            continue;
        };
        let RpcFrame::Request { name, path, .. } = parse_frame(&text).unwrap() else {
            panic!()
        };
        match path[0].as_str() {
            "getEnabledFeatures" => socket
                .send(Message::Text(
                    serialize_frame(&RpcFrame::Reply {
                        name,
                        error: None,
                        data: Some(json!({"list":{"skipSnapshot":false}})),
                    })
                    .unwrap()
                    .into(),
                ))
                .unwrap(),
            "list_sync_get_list_data" => {
                socket
                    .send(Message::Text(
                        serialize_frame(&RpcFrame::Reply {
                            name,
                            error: None,
                            data: Some(ListData::default().wire()),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .unwrap();
                break;
            }
            other => panic!("unexpected first sync method {other}"),
        };
        reads += 1;
        assert!(reads < 4);
    }
    slow_wait(
        &server,
        socket.get_ref(),
        Duration::from_secs(3),
        "large frame reservation",
        |view| view.reserved_frame_bytes > 1024 * 1024 || view.peers == 0,
    );
    assert!(server.view().reserved_frame_bytes <= 64 * 1024 * 1024);
    slow_wait(
        &server,
        socket.get_ref(),
        Duration::from_secs(8),
        "physical slow peer close",
        |view| view.peers == 0,
    );
    assert_eq!(server.view().connections, 1);
    #[cfg(debug_assertions)]
    {
        let stats = server.debug_transport_stats();
        assert!(
            stats.7 > 0 && stats.8 <= 16 * 1024,
            "actual TCP writes must remain bounded: {stats:?}"
        );
        assert!(
            !stats.5 && stats.9 < stats.3 as u64,
            "large frame must stall before full OS acceptance: {stats:?}"
        );
    }
    drop(socket);
    stop(&mut server);
}
fn small_receive_window(socket: &TcpStream) {
    let bytes = 2048i32;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn setsockopt(
                socket: usize,
                level: i32,
                name: i32,
                value: *const u8,
                length: i32,
            ) -> i32;
        }
        assert_eq!(
            unsafe {
                setsockopt(
                    socket.as_raw_socket() as usize,
                    0xffff,
                    0x1002,
                    (&bytes as *const i32).cast(),
                    4,
                )
            },
            0
        );
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&bytes as *const i32).cast(),
                    std::mem::size_of_val(&bytes) as libc::socklen_t,
                )
            },
            0
        );
    }
}

fn slow_wait(
    network: &SyncNetwork,
    socket: &TcpStream,
    timeout: Duration,
    stage: &str,
    predicate: impl Fn(&NetworkView) -> bool,
) {
    let started = Instant::now();
    let mut events = std::collections::VecDeque::new();
    let mut peak_frames = 0;
    let mut peak_workspace = 0;
    let mut peak_pending = 0;
    let mut peak_os_unread = 0;
    loop {
        while let Some(event) = network.try_recv() {
            if events.len() == 64 {
                events.pop_front();
            }
            events.push_back(event);
        }
        let view = network.view();
        peak_frames = peak_frames.max(view.reserved_frame_bytes);
        peak_workspace = peak_workspace.max(view.workspace_bytes);
        peak_pending = peak_pending.max(view.pending_rpc);
        peak_os_unread = peak_os_unread.max(os_unread_bytes(socket).unwrap_or(0));
        if predicate(&view) {
            return;
        }
        #[cfg(debug_assertions)]
        let transport = network.debug_transport_stats();
        #[cfg(not(debug_assertions))]
        let transport = ();
        assert!(
            started.elapsed() < timeout,
            "slow fixture stage={stage}; view={view:?}; peak_frames={peak_frames}; peak_workspace={peak_workspace}; peak_pending={peak_pending}; peer_OS_unread={peak_os_unread}; transport(encoded_bytes,encode_ms,encoding,write_bytes,write_ms,write_done,SNDBUF,TCPpolls,TCPmax,TCPaccepted)={transport:?}; events={events:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn os_unread_bytes(socket: &TcpStream) -> Option<usize> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn ioctlsocket(socket: usize, command: i32, argument: *mut u32) -> i32;
        }
        let mut bytes = 0u32;
        let result =
            unsafe { ioctlsocket(socket.as_raw_socket() as usize, 0x4004667f, &mut bytes) };
        (result == 0).then_some(bytes as usize)
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut bytes = 0i32;
        let result = unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut bytes) };
        (result == 0 && bytes >= 0).then_some(bytes as usize)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = socket;
        None
    }
}
