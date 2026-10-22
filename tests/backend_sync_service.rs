use lx_gpui_player::{
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSyncNetwork, CommandEnvelope,
    },
    domain::{
        lists as native,
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::DATABASE_NAME,
    sync::{
        Dataset, SyncMode,
        network::{ClientConfig, NetworkState, PairingCode, PeerEndpoint, ServerConfig},
    },
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    net::{SocketAddr, TcpListener},
    sync::Mutex,
    time::{Duration, Instant},
};
static CASES: Mutex<()> = Mutex::new(());
struct Player {
    directory: tempfile::TempDir,
    backend: BackendHandle,
}
impl Player {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("actual-backend-sync-")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        Self { directory, backend }
    }
    fn sql(&self) -> Connection {
        Connection::open(self.directory.path().join("profile").join(DATABASE_NAME)).unwrap()
    }
    fn execute(&self, id: &str, command: BackendCommand) {
        self.backend
            .execute(CommandEnvelope {
                request_id: id.into(),
                origin: "sync-native-fixture".into(),
                expected_revision: None,
                command,
            })
            .unwrap();
    }
    fn add(&self, id: &str) {
        self.execute(
            id,
            BackendCommand::ListAction {
                action: Box::new(native::ListAction::MusicAdd {
                    list_id: "default".into(),
                    tracks: vec![track(id)],
                    location: native::AddLocation::Top,
                }),
            },
        );
    }
    fn ids(&self) -> Vec<String> {
        let BackendQueryResult::ListTracks(page) = self
            .backend
            .query(BackendQuery::ListTracks {
                list_id: "default".into(),
                offset: 0,
                limit: 100,
            })
            .unwrap()
        else {
            panic!("Expected SQL list page")
        };
        page.items.into_iter().map(|track| track.lx_id).collect()
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.backend.shutdown(Duration::from_secs(5));
    }
}
fn music(id: &str) -> Value {
    json!({"id":id,"name":id,"singer":"Native peer","source":"kw","interval":null,"meta":{"songId":id,"albumName":"","qualitys":[],"_qualitys":{}}})
}
fn track(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(music(id), LxMusicDtoVersion::Modern).unwrap()
}
fn wait(timeout: Duration, predicate: impl Fn() -> bool) {
    let started = Instant::now();
    while !predicate() {
        assert!(started.elapsed() < timeout, "Actual backend sync deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn server(player: &Player) -> BackendSyncNetwork {
    let mut config = ServerConfig::loopback(
        "actual-sql-server".into(),
        "SQL Fixture".into(),
        PairingCode::new("123456").unwrap(),
        7,
    );
    config.listen_address = SocketAddr::from(([127, 0, 0, 1], 0));
    let server = player.backend.start_sync_server(config).unwrap();
    wait(Duration::from_secs(3), || {
        server.view().state == NetworkState::Listening
    });
    server
}
fn client(player: &Player, address: SocketAddr) -> BackendSyncNetwork {
    player
        .backend
        .start_sync_client(ClientConfig {
            endpoint: PeerEndpoint {
                url: format!("http://{address}"),
                pinned_address: address,
                tls: None,
            },
            device_name: "Actual SQLite peer".into(),
            credentials: None,
            pairing_code: Some(PairingCode::new("123456").unwrap()),
            pairing_epoch: 7,
            capabilities: lx_gpui_player::sync::PeerCapabilities {
                list: Some(false),
                dislike: Some(false),
            },
            reconnect_limit: 2,
        })
        .unwrap()
}
fn choose(player: &Player) {
    for peer in player.backend.sync_peers().unwrap() {
        if let (Some(owner), Some(session)) = (peer.owner_epoch, peer.session_epoch) {
            for (dataset, mode, enabled) in [
                (
                    Dataset::List,
                    peer.list_mode,
                    peer.capabilities.list.is_some(),
                ),
                (
                    Dataset::Dislike,
                    peer.dislike_mode,
                    peer.capabilities.dislike.is_some(),
                ),
            ] {
                if enabled && mode.is_none() {
                    player
                        .backend
                        .choose_sync_mode(
                            owner,
                            peer.peer_id.clone(),
                            peer.auth_epoch,
                            session,
                            dataset,
                            SyncMode::MergeLocalRemote,
                        )
                        .unwrap();
                }
            }
        }
    }
}
fn connected(
    left: &Player,
    right: &Player,
    server: &BackendSyncNetwork,
    client: &BackendSyncNetwork,
) {
    wait_data_stage(
        Duration::from_secs(25),
        "first negotiated merge",
        left,
        right,
        server,
        client,
        || {
            choose(left);
            choose(right);
            server.view().state == NetworkState::Synced
                && client.view().state == NetworkState::Synced
        },
    );
}
fn wait_data_stage(
    timeout: Duration,
    label: &str,
    left: &Player,
    right: &Player,
    server: &BackendSyncNetwork,
    client: &BackendSyncNetwork,
    predicate: impl Fn() -> bool,
) {
    let started = Instant::now();
    let mut server_events = std::collections::VecDeque::new();
    let mut client_events = std::collections::VecDeque::new();
    while !predicate() {
        for (network, events) in [(server, &mut server_events), (client, &mut client_events)] {
            while let Some(event) = network.try_recv() {
                if events.len() == 64 {
                    events.pop_front();
                }
                events.push_back(event);
            }
        }
        let left_rules = left.sql().query_row(
            "SELECT rules FROM dislike_state WHERE singleton=1",
            [],
            |row| row.get::<_, String>(0),
        );
        let right_rules = right.sql().query_row(
            "SELECT rules FROM dislike_state WHERE singleton=1",
            [],
            |row| row.get::<_, String>(0),
        );
        assert!(
            started.elapsed() < timeout,
            "actual backend stage={label}; server={:?}; client={:?}; leftS={:?}; rightS={:?}; leftRules={left_rules:?}; rightRules={right_rules:?}; server_events={server_events:?}; client_events={client_events:?}",
            server.view(),
            client.view(),
            left.sql().query_row(
                "SELECT sequence FROM sync_profile_state WHERE singleton=1",
                [],
                |row| row.get::<_, i64>(0)
            ),
            right.sql().query_row(
                "SELECT sequence FROM sync_profile_state WHERE singleton=1",
                [],
                |row| row.get::<_, i64>(0)
            )
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn two_real_sqlite_backends_pair_commit_merge_and_incremental_lists_dislike() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Player::new();
    let right = Player::new();
    left.add("left");
    right.add("right");
    let mut server = server(&left);
    let mut client = client(&right, server.view().address.unwrap());
    connected(&left, &right, &server, &client);
    assert!(left.ids().contains(&"right".into()) && right.ids().contains(&"left".into()));
    let cursor = right.backend.event_cursor();
    left.add("actual-delta");
    wait_data_stage(
        Duration::from_secs(12),
        "list incremental",
        &left,
        &right,
        &server,
        &client,
        || right.ids().contains(&"actual-delta".into()),
    );
    assert!(
        right
            .backend
            .events_after(cursor)
            .unwrap()
            .iter()
            .any(|event| matches!(&event.payload, BackendEvent::ListsChanged { .. }))
    );
    left.execute(
        "actual-dislike",
        BackendCommand::DislikeAction {
            action: native::DislikeAction::Overwrite {
                rules: "Actual@Artist".into(),
            },
        },
    );
    wait_data_stage(
        Duration::from_secs(12),
        "dislike incremental",
        &left,
        &right,
        &server,
        &client,
        || {
            right
                .sql()
                .query_row(
                    "SELECT rules FROM dislike_state WHERE singleton=1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
                == "Actual@Artist"
        },
    );
    for player in [&left, &right] {
        let count: i64 = player
            .sql()
            .query_row("SELECT count(*) FROM sync_baselines", [], |row| row.get(0))
            .unwrap();
        assert!(count >= 2);
        let outbox: i64 = player
            .sql()
            .query_row("SELECT count(*) FROM change_outbox", [], |row| row.get(0))
            .unwrap();
        assert!(outbox > 0);
    }
    assert!(client.shutdown(Duration::from_secs(5)).unwrap());
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
}

#[test]
fn actual_mode_is_pending_until_local_confirmation_and_rejects_stale_owner_session() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Player::new();
    let right = Player::new();
    left.add("left");
    right.add("right");
    let mut server = server(&left);
    let mut client = client(&right, server.view().address.unwrap());
    wait(Duration::from_secs(10), || {
        left.backend
            .sync_peers()
            .unwrap()
            .iter()
            .any(|peer| peer.session_epoch.is_some())
    });
    let peer = left.backend.sync_peers().unwrap().pop().unwrap();
    assert!(peer.list_mode.is_none());
    assert_eq!(left.ids(), ["left"]);
    assert!(
        left.backend
            .choose_sync_mode(
                peer.owner_epoch.unwrap() + 1,
                peer.peer_id.clone(),
                peer.auth_epoch,
                peer.session_epoch.unwrap(),
                Dataset::List,
                SyncMode::OverwriteRemoteLocalFull
            )
            .is_err()
    );
    assert!(
        left.backend
            .choose_sync_mode(
                peer.owner_epoch.unwrap(),
                peer.peer_id.clone(),
                peer.auth_epoch,
                peer.session_epoch.unwrap() + 1,
                Dataset::List,
                SyncMode::MergeLocalRemote
            )
            .is_err()
    );
    assert!(
        left.backend
            .choose_sync_mode(
                peer.owner_epoch.unwrap(),
                peer.peer_id.clone(),
                peer.auth_epoch,
                peer.session_epoch.unwrap(),
                Dataset::Dislike,
                SyncMode::OverwriteRemoteLocalFull
            )
            .is_err()
    );
    connected(&left, &right, &server, &client);
    assert!(right.ids().contains(&"left".into()));
    assert!(client.shutdown(Duration::from_secs(5)).unwrap());
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
    let rows: i64 = left
        .sql()
        .query_row("SELECT count(*) FROM sync_mode_preferences", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(rows > 0);
}

#[test]
fn managed_sync_profile_shutdown_actually_joins_and_releases_port_and_sqlite_writer() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let player = Player::new();
    assert!(player.backend.sync_peers().unwrap().is_empty());
    let mut service = server(&player);
    let address = service.view().address.unwrap();
    player.backend.shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(service.view().connections, 0);
    assert!(service.shutdown(Duration::ZERO).unwrap());
    drop(TcpListener::bind(address).unwrap());
    let reopened =
        lx_gpui_player::repository::Repository::open(player.directory.path().join("profile"))
            .unwrap();
    assert!(!reopened.sync_profile_state().unwrap().pairing_enabled);
    reopened.integrity_check().unwrap();
}

#[test]
fn actual_network_local_reference_has_no_fresh_asset_file_or_source_permission() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Player::new();
    let right = Player::new();
    let path = left.directory.path().join("never-approved-or-created.wav");
    let value = json!({"id":"opaque-local-exact","name":"Opaque","singer":"","source":"local","interval":null,"meta":{"songId":"opaque","albumName":"","filePath":path.to_string_lossy(),"ext":"wav"}});
    let local = TrackRecord::from_lx_dto(value, LxMusicDtoVersion::Modern).unwrap();
    left.execute(
        "opaque-metadata",
        BackendCommand::ListAction {
            action: Box::new(native::ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![local],
                location: native::AddLocation::Top,
            }),
        },
    );
    let mut server = server(&left);
    let mut client = client(&right, server.view().address.unwrap());
    connected(&left, &right, &server, &client);
    assert_eq!(right.ids(), ["opaque-local-exact"]);
    assert!(!path.exists());
    for table in ["local_assets", "local_file_grants", "library_roots"] {
        let count: i64 = right
            .sql()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
    assert!(right.backend.snapshot().sources.is_empty());
    let data = lx_gpui_player::store::ProjectStore::open(right.directory.path().join("profile"))
        .unwrap()
        .load()
        .unwrap();
    assert!(data.sources.is_empty());
    assert!(client.shutdown(Duration::from_secs(5)).unwrap());
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
}

#[test]
fn real_remote_sql_fault_has_no_fake_ack_partial_rows_or_baseline_advance() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let left = Player::new();
    let right = Player::new();
    left.add("left");
    right.add("right");
    let mut server = server(&left);
    let mut client = client(&right, server.view().address.unwrap());
    connected(&left, &right, &server, &client);
    let before: i64 = right
        .sql()
        .query_row(
            "SELECT sequence FROM sync_profile_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    right.sql().execute_batch("CREATE TRIGGER fail_remote_sync BEFORE INSERT ON sync_journal WHEN NEW.origin_peer IS NOT NULL BEGIN SELECT RAISE(ABORT,'actual injected remote SQL failure'); END;").unwrap();
    left.add("must-roll-back");
    wait_data_stage(
        Duration::from_secs(12),
        "remote SQL rejection",
        &left,
        &right,
        &server,
        &client,
        || matches!(client.view().state, NetworkState::Degraded),
    );
    assert!(
        client
            .view()
            .last_error
            .as_ref()
            .is_some_and(|error| error.message.contains("actual injected remote SQL failure"))
    );
    assert!(!right.ids().contains(&"must-roll-back".into()));
    let after: i64 = right
        .sql()
        .query_row(
            "SELECT sequence FROM sync_profile_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    right
        .sql()
        .execute_batch("DROP TRIGGER fail_remote_sync;")
        .unwrap();
    assert!(client.shutdown(Duration::from_secs(5)).unwrap());
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
}

#[test]
fn explicitly_selected_saved_server_reconnects_after_actual_profile_reopen_without_key_json() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let mut left = Player::new();
    let mut right = Player::new();
    left.add("left");
    right.add("right");
    let mut first_server = server(&left);
    let mut first_client = client(&right, first_server.view().address.unwrap());
    connected(&left, &right, &first_server, &first_client);
    assert!(first_client.shutdown(Duration::from_secs(5)).unwrap());
    assert!(first_server.shutdown(Duration::from_secs(5)).unwrap());
    drop(first_client);
    drop(first_server);
    left.backend.shutdown(Duration::from_secs(5)).unwrap();
    right.backend.shutdown(Duration::from_secs(5)).unwrap();
    left.backend = BackendHandle::start(BackendConfig::headless(
        left.directory.path().join("profile"),
    ))
    .unwrap();
    right.backend = BackendHandle::start(BackendConfig::headless(
        right.directory.path().join("profile"),
    ))
    .unwrap();
    let mut saved_server = server(&left);
    let address = saved_server.view().address.unwrap();
    let config = ClientConfig {
        endpoint: PeerEndpoint {
            url: format!("http://{address}"),
            pinned_address: address,
            tls: None,
        },
        device_name: "Actual saved SQLite peer".into(),
        credentials: None,
        pairing_code: None,
        pairing_epoch: 7,
        capabilities: lx_gpui_player::sync::PeerCapabilities {
            list: Some(false),
            dislike: Some(false),
        },
        reconnect_limit: 2,
    };
    let mut saved_client = right
        .backend
        .start_sync_client_saved(config, "actual-sql-server".into())
        .unwrap();
    connected(&left, &right, &saved_server, &saved_client);
    let client_rows: i64 = right
        .sql()
        .query_row(
            "SELECT count(*) FROM sync_peers WHERE json_extract(document,'$.role')='client'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(client_rows, 1);
    assert!(saved_client.shutdown(Duration::from_secs(5)).unwrap());
    assert!(saved_server.shutdown(Duration::from_secs(5)).unwrap());
}

#[test]
fn unknown_saved_server_never_connects_or_selects_a_key_from_peer_supplied_identity() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let player = Player::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let config = ClientConfig {
        endpoint: PeerEndpoint {
            url: format!("http://{address}"),
            pinned_address: address,
            tls: None,
        },
        device_name: "Unknown saved peer".into(),
        credentials: None,
        pairing_code: None,
        pairing_epoch: 7,
        capabilities: lx_gpui_player::sync::PeerCapabilities {
            list: Some(false),
            dislike: Some(false),
        },
        reconnect_limit: 0,
    };
    assert!(
        player
            .backend
            .start_sync_client_saved(config, "never-paired-id".into())
            .is_err()
    );
    assert!(matches!(listener.accept(),Err(error)if error.kind()==std::io::ErrorKind::WouldBlock));
    assert!(player.backend.sync_peers().unwrap().is_empty());
    assert!(
        !player
            .sql()
            .query_row(
                "SELECT pairing_enabled FROM sync_profile_state WHERE singleton=1",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
    );
}
