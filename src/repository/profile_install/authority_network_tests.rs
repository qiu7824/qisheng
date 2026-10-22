use super::*;
use crate::{
    backend::{
        BackendCommand, BackendConfig, BackendHandle, BackendQuery, BackendQueryResult,
        BackendSyncNetwork, CommandEnvelope,
    },
    sync::{
        SyncMode,
        network::{ClientConfig, NetworkState, PairingCode, PeerEndpoint, ServerConfig},
    },
};
use std::{
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::Instant,
};

struct Actor(BackendHandle);
impl Drop for Actor {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Duration::from_secs(15));
    }
}
fn actor(repository: Repository, directory: &Path) -> Actor {
    let guard = Arc::new(repository).close_into_guard().unwrap();
    Actor(
        BackendHandle::start_with_guard(BackendConfig::headless(directory.to_path_buf()), guard)
            .unwrap(),
    )
}
fn choose(actor: &Actor) {
    for peer in actor.0.sync_peers().unwrap() {
        let (Some(owner), Some(session)) = (peer.owner_epoch, peer.session_epoch) else {
            continue;
        };
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
                actor
                    .0
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
fn ids(actor: &Actor) -> Vec<String> {
    let BackendQueryResult::ListTracks(page) = actor
        .0
        .query(BackendQuery::ListTracks {
            list_id: "default".into(),
            offset: 0,
            limit: 100,
        })
        .unwrap()
    else {
        panic!("Actual list query returned another result");
    };
    page.items.into_iter().map(|item| item.lx_id).collect()
}
fn wait(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(25);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "Actual saved-credential negotiation did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn retire(network: &mut BackendSyncNetwork) {
    let address = network.view().address;
    assert!(network.shutdown(Duration::from_secs(15)).unwrap());
    assert_eq!(network.view().connections, 0);
    assert_eq!(network.view().pending_rpc, 0);
    if let Some(address) = address {
        drop(TcpListener::bind(address).unwrap());
    }
}

#[test]
fn actual_saved_current_credentials_reconnect_over_lx_v4_and_deliver_restored_full_dataset() {
    let folder = folder();
    let current_path = folder.path().join("current");
    let installed_path = folder.path().join("installed");
    let remote_path = folder.path().join("remote");
    let current = Repository::open(&current_path).unwrap();
    let candidate = Repository::open(&installed_path).unwrap();
    current.sync_configure_pairing(1, 7, true).unwrap();
    let approved = peer("approved-current-client", 7);
    current.sync_store_peer(7, &approved).unwrap();
    add(&current, "prior-private-cut");
    add(&candidate, "restored-dataset");
    sanitize(&candidate);
    let (_authority, report) = copy_into(&current, &candidate);
    let before_key = candidate.sync_peers().unwrap()[0].key.expose_base64();
    let remote = Repository::open(&remote_path).unwrap();
    remote.sync_configure_pairing(1, 7, true).unwrap();
    let saved_server = PeerRecord {
        peer_id: "authority-fixture-server".into(),
        client_id: approved.client_id.clone(),
        key: approved.key.clone(),
        device_name: "Authority fixture server".into(),
        is_mobile: false,
        auth_epoch: 7,
        capabilities: approved.capabilities,
    };
    remote
        .sync_store_client_peer_with_commit_guard(7, &saved_server, &|| Ok(()), || Ok(()))
        .unwrap();
    remote.sync_configure_pairing(1, 7, false).unwrap();
    let left = actor(candidate, &installed_path);
    let right = actor(remote, &remote_path);
    let mut config = ServerConfig::loopback(
        "authority-fixture-server".into(),
        "Authority fixture server".into(),
        PairingCode::new("123456").unwrap(),
        7,
    );
    config.listen_address = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut server = left.0.start_sync_server(config).unwrap();
    wait(|| server.view().state == NetworkState::Listening);
    let address = server.view().address.unwrap();
    let mut client = right
        .0
        .start_sync_client_saved(
            ClientConfig {
                endpoint: PeerEndpoint {
                    url: format!("http://{address}"),
                    pinned_address: address,
                    tls: None,
                },
                device_name: "Saved current client".into(),
                credentials: None,
                pairing_code: None,
                pairing_epoch: 7,
                capabilities: PeerCapabilities {
                    list: Some(false),
                    dislike: Some(false),
                },
                reconnect_limit: 1,
            },
            "authority-fixture-server".into(),
        )
        .unwrap();
    wait(|| {
        choose(&left);
        choose(&right);
        while server.try_recv().is_some() {}
        while client.try_recv().is_some() {}
        server.view().state == NetworkState::Synced && client.view().state == NetworkState::Synced
    });
    assert_eq!(ids(&left), vec!["kw_restored-dataset"]);
    assert_eq!(ids(&right), vec!["kw_restored-dataset"]);
    let sql = Connection::open(installed_path.join(crate::repository::DATABASE_NAME)).unwrap();
    assert!(
        sql.query_row(
            "SELECT private_key FROM sync_peers WHERE peer_id=?1",
            [&approved.peer_id],
            |row| row.get::<_, String>(0)
        )
        .unwrap()
            == before_key,
        "Reused authentication changed the current private credential"
    );
    let (sequence, baselines): (i64, i64) = sql.query_row("SELECT sequence,(SELECT count(*) FROM sync_baselines) FROM sync_profile_state WHERE singleton=1", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert!(sequence >= report.sync_sequence as i64);
    assert!(baselines >= 2);
    left.0
        .execute(CommandEnvelope {
            request_id: "after-restored-full-cut".into(),
            origin: "fixture".into(),
            expected_revision: None,
            command: BackendCommand::ListAction {
                action: Box::new(native::ListAction::MusicAdd {
                    list_id: "default".into(),
                    tracks: vec![track("fresh-increment")],
                    location: native::AddLocation::Bottom,
                }),
            },
        })
        .unwrap();
    wait(|| {
        while server.try_recv().is_some() {}
        while client.try_recv().is_some() {}
        ids(&right) == vec!["kw_restored-dataset", "kw_fresh-increment"]
    });
    retire(&mut client);
    retire(&mut server);
    drop(sql);
    left.0.shutdown(Duration::from_secs(15)).unwrap();
    right.0.shutdown(Duration::from_secs(15)).unwrap();
}
