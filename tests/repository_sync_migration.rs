use lx_gpui_player::{
    domain::{
        lists::{ListAction, ListInfo},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::{Repository, SCHEMA_VERSION},
};
use rusqlite::Connection;
use serde_json::json;

#[test]
fn schema_eleven_migration_preserves_user_data_and_does_not_enable_pairing_or_open_media() {
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let repo = Repository::open(directory.path()).unwrap();
    let track = TrackRecord::from_lx_dto(
        json!({"source":"kw","songmid":"migration-kept","name":"保留曲目","singer":"作者",
        "albumName":"专辑","interval":"00:36","types":[{"type":"128k"}]}),
        LxMusicDtoVersion::Legacy,
    )
    .unwrap();
    repo.apply_list_action(
        &ListAction::Create {
            position: 0,
            lists: vec![ListInfo::user("migration-kept-list", "保留列表")],
        },
        None,
    )
    .unwrap();
    repo.apply_list_action(
        &ListAction::MusicOverwrite {
            list_id: "migration-kept-list".into(),
            tracks: vec![track],
        },
        None,
    )
    .unwrap();
    let original = repo.list_data().unwrap();
    let revision = repo.revision().unwrap();
    let path = repo.path().to_path_buf();
    drop(repo);
    let database = Connection::open(&path).unwrap();
    database.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; PRAGMA user_version=11;").unwrap();
    drop(database);
    let before_names = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(SCHEMA_VERSION, 17);
    assert_eq!(repo.list_data().unwrap(), original);
    assert_eq!(repo.revision().unwrap(), revision);
    let state = repo.sync_profile_state().unwrap();
    assert!(!state.pairing_enabled);
    assert_eq!(state.sequence, 0);
    assert!(repo.sync_peers().unwrap().is_empty());
    repo.integrity_check().unwrap();
    assert!(!directory.path().join("audio-cache").exists());
    assert!(!directory.path().join("download-cleanup").exists());
    assert!(
        before_names
            .iter()
            .all(|name| directory.path().join(name).exists())
    );
    let database = Connection::open(repo.path()).unwrap();
    for table in [
        "sync_journal",
        "sync_baselines",
        "sync_peers",
        "sync_capture_override",
    ] {
        assert_eq!(
            database
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn schema_twelve_migration_preserves_peer_keys_baselines_sequences_and_creates_no_mode_authority() {
    use lx_gpui_player::sync::{
        Dataset, PeerCapabilities, SharedKey,
        network::{PeerRecord, SessionTicket},
    };
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let repo = Repository::open(directory.path()).unwrap();
    repo.sync_configure_pairing(1, 9, true).unwrap();
    let peer = PeerRecord {
        peer_id: "schema12-peer".into(),
        client_id: "schema12-client".into(),
        key: SharedKey::from_bytes([0x3d; 16]),
        device_name: "Preserved device".into(),
        is_mobile: false,
        auth_epoch: 3,
        capabilities: PeerCapabilities {
            list: Some(false),
            dislike: Some(false),
        },
    };
    repo.sync_store_peer(9, &peer).unwrap();
    let ticket = SessionTicket::new(peer.peer_id.clone(), 3, 8);
    repo.sync_begin_session(&ticket).unwrap();
    let snapshot = repo.sync_snapshot(&ticket).unwrap();
    let data = snapshot.data(Dataset::List);
    let digest = data.digest().unwrap();
    repo.sync_confirm_baseline(
        &ticket,
        &data,
        snapshot.repository_revision,
        snapshot.sequence,
        &digest,
    )
    .unwrap();
    let state = repo.sync_profile_state().unwrap();
    let revision = repo.revision().unwrap();
    let baseline = repo.sync_baseline(&ticket, Dataset::List).unwrap().unwrap();
    let path = repo.path().to_path_buf();
    ticket.retire();
    drop(repo);
    let database = Connection::open(&path).unwrap();
    database
        .execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; PRAGMA user_version=12;")
        .unwrap();
    drop(database);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(SCHEMA_VERSION, 17);
    assert_eq!(repo.revision().unwrap(), revision);
    let migrated = repo.sync_profile_state().unwrap();
    assert_eq!(
        (
            migrated.profile_epoch,
            migrated.pairing_epoch,
            migrated.pairing_enabled,
            migrated.sequence
        ),
        (
            state.profile_epoch,
            state.pairing_epoch,
            state.pairing_enabled,
            state.sequence
        )
    );
    let peers = repo.sync_peers().unwrap();
    let preserved = &peers[0];
    assert_eq!(preserved.peer_id, peer.peer_id);
    assert_eq!(preserved.key.expose_base64(), peer.key.expose_base64());
    let current = SessionTicket::new(peer.peer_id, 3, 9);
    repo.sync_begin_session(&current).unwrap();
    let after = repo
        .sync_baseline(&current, Dataset::List)
        .unwrap()
        .unwrap();
    assert_eq!(
        (after.digest, after.sequence, after.repository_revision),
        (
            baseline.digest,
            baseline.sequence,
            baseline.repository_revision
        )
    );
    let database = Connection::open(repo.path()).unwrap();
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM sync_mode_preferences", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    database.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        database
            .execute(
                "INSERT INTO sync_mode_preferences VALUES('nonexistent','list',3,'\"merge\"')",
                []
            )
            .is_err()
    );
    repo.integrity_check().unwrap();
    assert!(std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("schema-12-")
    }));
}
