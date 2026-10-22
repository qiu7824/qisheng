use lx_gpui_player::{
    domain::{
        lists as native,
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::{ChangeKind, Repository},
    sync::{
        Dataset, DislikeAction, ListAction, ListData, MusicInfo, PeerCapabilities, SharedKey,
        network::{PeerRecord, SessionTicket, SyncChanges, SyncData, SyncOperation},
    },
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, path::Path, time::Duration};

fn profile() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("repository-sync-")
        .tempdir_in(std::env::temp_dir())
        .unwrap()
}
fn music(id: &str) -> Value {
    json!({"id":format!("kw_{id}"),"name":format!("Song {id}"),"singer":"Artist","source":"kw","interval":"00:30","meta":{"songId":id,"albumName":"Album","qualitys":[{"type":"128k"}],"_qualitys":{"128k":{}}}})
}
fn track(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(music(id), LxMusicDtoVersion::Modern).unwrap()
}
fn peer(id: &str) -> PeerRecord {
    PeerRecord {
        peer_id: id.into(),
        client_id: format!("client-{id}"),
        key: SharedKey::from_bytes([7; 16]),
        device_name: format!("Peer {id}"),
        is_mobile: false,
        auth_epoch: 7,
        capabilities: PeerCapabilities {
            list: Some(false),
            dislike: Some(false),
        },
    }
}
fn ticket(repo: &Repository, id: &str) -> SessionTicket {
    repo.sync_configure_pairing(1, 7, true).unwrap();
    repo.sync_store_peer(7, &peer(id)).unwrap();
    let ticket = SessionTicket::new(id.into(), 7, 11);
    repo.sync_begin_session(&ticket).unwrap();
    ticket
}
fn local_add(repo: &Repository, id: &str) {
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "default".into(),
            tracks: vec![track(id)],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
}
fn apply(repo: &Repository, ticket: &SessionTicket, value: Value) {
    let action = ListAction::from_wire(&value).unwrap();
    let expected = if matches!(action, ListAction::DataOverwrite(_)) {
        Some(repo.sync_profile_state().unwrap().sequence)
    } else {
        None
    };
    let (change, receipt) = repo
        .sync_apply_remote(ticket, expected, &SyncOperation::List(action))
        .unwrap();
    assert_eq!(change.kind, ChangeKind::ListsChanged);
    assert_eq!(change.revision, receipt.repository_revision);
}
fn ids(repo: &Repository, list: &str) -> Vec<String> {
    repo.list_tracks(list)
        .unwrap()
        .into_iter()
        .map(|track| track.lx_id)
        .collect()
}
fn sql(repo: &Repository) -> Connection {
    Connection::open(repo.path()).unwrap()
}
fn count(connection: &Connection, table: &str) -> i64 {
    assert!(matches!(
        table,
        "sync_journal"
            | "sync_current"
            | "sync_capture_override"
            | "sync_peers"
            | "local_assets"
            | "local_file_grants"
            | "library_roots"
            | "local_asset_locations"
            | "sync_baselines"
    ));
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}
fn tagged_wav(path: &Path, title: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&8000u32.to_le_bytes());
    body.extend_from_slice(&16000u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&[0; 16]);
    let mut info = b"INFO".to_vec();
    for (key, value) in [(b"INAM", title), (b"IART", "Artist"), (b"IPRD", "Album")] {
        info.extend_from_slice(key);
        let mut text = value.as_bytes().to_vec();
        text.push(0);
        info.extend_from_slice(&(text.len() as u32).to_le_bytes());
        info.extend_from_slice(&text);
        if text.len() % 2 == 1 {
            info.push(0);
        }
    }
    body.extend_from_slice(b"LIST");
    body.extend_from_slice(&(info.len() as u32).to_le_bytes());
    body.extend_from_slice(&info);
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    fs::write(path, bytes).unwrap();
}
fn approved_local(path: &Path) -> lx_gpui_player::domain::library::LibraryPreparedAsset {
    use lx_gpui_player::library::{
        authority::RootAuthority, metadata::read_local_metadata_from_open_file, scan::ScanContext,
    };
    let root = RootAuthority::open(
        path.parent().unwrap(),
        false,
        &ScanContext::new(Duration::from_secs(30)),
    )
    .unwrap();
    let file = root.open_file(path).unwrap();
    let identity = file.identity().clone();
    let length = file.metadata().unwrap().len();
    let metadata = read_local_metadata_from_open_file(file, path.to_path_buf(), length).unwrap();
    let (mut track, asset) = lx_gpui_player::repository::local_track_from_path(path).unwrap();
    if let Some(title) = metadata.title {
        track.title = title;
    }
    track.artists = metadata.artists;
    if let Some(album) = metadata.album {
        track.album = album;
    }
    track.duration_ms = Some(metadata.duration_ms);
    track.metadata.insert("ext".into(), json!("wav"));
    track
        .metadata
        .insert("albumName".into(), json!(track.album));
    let barrier = root.retirement_barrier();
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    lx_gpui_player::domain::library::LibraryPreparedAsset {
        track,
        asset,
        file_identity: Some(identity),
    }
}
fn actual_root(path: &Path) -> lx_gpui_player::domain::library::LibraryRootDraft {
    use lx_gpui_player::library::{authority::RootAuthority, scan::ScanContext};
    let root =
        RootAuthority::open(path, false, &ScanContext::new(Duration::from_secs(30))).unwrap();
    let draft = root.draft();
    let barrier = root.retirement_barrier();
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    draft
}
fn finish_scan(repo: &Repository, token: &lx_gpui_player::domain::library::LibraryScanToken) {
    use lx_gpui_player::domain::library::{LibraryOperationState, LibraryOperationView};
    let mut view = LibraryOperationView::running(token);
    view.state = LibraryOperationState::Completed;
    repo.finish_library_scan(token, &view, true, &|| Ok(()))
        .unwrap();
}

#[test]
fn coherent_cuts_independent_peer_ack_preserve_s_plus_one_and_global_outbox() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let a = ticket(&repo, "a");
    let b = ticket(&repo, "b");
    local_add(&repo, "1");
    let cut = repo.sync_snapshot(&a).unwrap();
    assert_eq!(cut.sequence, 1);
    assert_eq!(cut.lists.default_list[0].id(), "kw_1");
    local_add(&repo, "2");
    let data = cut.data(Dataset::List);
    let digest = data.digest().unwrap();
    repo.sync_confirm_baseline(&a, &data, cut.repository_revision, cut.sequence, &digest)
        .unwrap();
    let baseline = repo.sync_baseline(&a, Dataset::List).unwrap().unwrap();
    assert_eq!(baseline.sequence, 1);
    assert!(repo.sync_baseline(&b, Dataset::List).unwrap().is_none());
    assert_eq!(repo.changes_since(0, 100).unwrap().len(), 2);
    let SyncChanges::Delta { records, snapshot } =
        repo.sync_changes_after(&a, baseline.sequence, 100).unwrap()
    else {
        panic!("Expected native incremental record")
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sequence, 2);
    assert_eq!(snapshot.sequence, 2);
    assert_eq!(records[0].action["action"], "list_music_add");
    assert!(
        repo.sync_confirm_baseline(&a, &data, cut.repository_revision, 3, &digest)
            .is_err()
    );
    assert!(
        repo.sync_confirm_baseline(&b, &data, cut.repository_revision, 1, &digest)
            .is_err()
    );
    let b_cut = repo.sync_snapshot(&b).unwrap();
    let b_data = b_cut.data(Dataset::List);
    repo.sync_confirm_baseline(
        &b,
        &b_data,
        b_cut.repository_revision,
        b_cut.sequence,
        &b_data.digest().unwrap(),
    )
    .unwrap();
    assert_eq!(
        repo.sync_baseline(&a, Dataset::List)
            .unwrap()
            .unwrap()
            .sequence,
        1
    );
    repo.integrity_check().unwrap();
}

#[test]
fn all_twelve_incoming_list_actions_change_actual_sql_membership_and_order() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    apply(
        &repo,
        &t,
        json!({"action":"list_data_overwrite","data":{"defaultList":[music("1"),music("2")],"loveList":[],"userList":[]}}),
    );
    apply(
        &repo,
        &t,
        json!({"action":"list_create","data":{"position":0,"listInfos":[{"id":"a","name":"A"},{"id":"b","name":"B"}]}}),
    );
    apply(
        &repo,
        &t,
        json!({"action":"list_update","data":[{"id":"b","name":"B2","source":null,"sourceListId":123,"locationUpdateTime":null}]}),
    );
    let before_move = repo.sync_snapshot(&t).unwrap();
    let info = before_move
        .lists
        .user_list
        .iter()
        .find(|info| info.id == "b")
        .unwrap()
        .wire();
    assert_eq!(info["locationUpdateTime"], Value::Null);
    apply(
        &repo,
        &t,
        json!({"action":"list_update_position","data":{"position":0,"ids":["b"]}}),
    );
    apply(
        &repo,
        &t,
        json!({"action":"list_music_add","data":{"id":"default","musicInfos":[music("3"),music("2")],"addMusicLocationType":"top"}}),
    );
    assert_eq!(ids(&repo, "default"), ["kw_3", "kw_1", "kw_2"]);
    apply(
        &repo,
        &t,
        json!({"action":"list_music_move","data":{"fromId":"default","toId":"love","musicInfos":[music("3"),music("1")],"addMusicLocationType":"bottom"}}),
    );
    assert_eq!(ids(&repo, "default"), ["kw_2"]);
    assert_eq!(ids(&repo, "love"), ["kw_3", "kw_1"]);
    let mut updated = music("3");
    updated["name"] = json!("Changed");
    apply(
        &repo,
        &t,
        json!({"action":"list_music_update","data":[{"id":"love","musicInfo":updated}]}),
    );
    assert_eq!(repo.list_tracks("love").unwrap()[0].title, "Changed");
    apply(
        &repo,
        &t,
        json!({"action":"list_music_update_position","data":{"listId":"love","position":0,"ids":["kw_1"]}}),
    );
    assert_eq!(ids(&repo, "love"), ["kw_1", "kw_3"]);
    apply(
        &repo,
        &t,
        json!({"action":"list_music_remove","data":{"listId":"love","ids":["kw_1"]}}),
    );
    apply(
        &repo,
        &t,
        json!({"action":"list_music_overwrite","data":{"listId":"a","musicInfos":[music("4")]}}),
    );
    apply(&repo, &t, json!({"action":"list_music_clear","data":["a"]}));
    apply(&repo, &t, json!({"action":"list_remove","data":["a"]}));
    assert_eq!(ids(&repo, "love"), ["kw_3"]);
    assert!(repo.list_tracks("a").is_err());
    let snapshot = repo.sync_snapshot(&t).unwrap();
    let info = snapshot.lists.user_list[0].wire();
    assert_eq!(info["name"], "B2");
    assert_eq!(info["source"], Value::Null);
    assert_eq!(info["sourceListId"], 123);
    assert!(
        info["locationUpdateTime"]
            .as_u64()
            .is_some_and(|time| time > 0)
    );
    assert_eq!(count(&sql(&repo), "sync_capture_override"), 0);
    repo.integrity_check().unwrap();
}

#[test]
fn all_three_dislike_actions_use_actual_raw_sql_dataset() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    for value in [
        json!({"action":"dislike_data_overwrite","data":"Alpha@Artist\n\nBeta@"}),
        json!({"action":"dislike_music_add","data":[{"name":"Gamma","singer":"G"}]}),
        json!({"action":"dislike_music_clear"}),
    ] {
        let action = DislikeAction::from_wire(&value).unwrap();
        let expected = matches!(action, DislikeAction::DataOverwrite(_))
            .then(|| repo.sync_profile_state().unwrap().sequence);
        repo.sync_apply_remote(&t, expected, &SyncOperation::Dislike(action))
            .unwrap();
        if value["action"] == "dislike_data_overwrite" {
            assert_eq!(
                repo.sync_snapshot(&t).unwrap().dislike,
                "Alpha@Artist\n\nBeta@"
            );
        }
        if value["action"] == "dislike_music_add" {
            assert_eq!(
                repo.sync_snapshot(&t).unwrap().dislike,
                "Alpha@Artist\n\nBeta@\nGamma@G"
            );
        }
    }
    assert_eq!(repo.sync_snapshot(&t).unwrap().dislike, "");
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 3);
}

#[test]
fn temp_first_write_is_not_a_lazy_public_sync_change_and_replace_preserves_private_lists() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "temp".into(),
            tracks: vec![track("temp")],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    repo.apply_list_action(
        &native::ListAction::Create {
            position: 0,
            lists: vec![native::ListInfo::user("download", "Downloads")],
        },
        None,
    )
    .unwrap();
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "download".into(),
            tracks: vec![track("download")],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    assert_eq!(count(&sql(&repo), "sync_current"), 0);
    assert_eq!(count(&sql(&repo), "sync_journal"), 0);
    let t = ticket(&repo, "a");
    let data =
        ListData::from_wire(&json!({"defaultList":[music("public")],"loveList":[],"userList":[]}))
            .unwrap();
    repo.sync_apply_remote(&t, Some(0), &SyncOperation::Replace(SyncData::List(data)))
        .unwrap();
    assert_eq!(ids(&repo, "temp"), ["kw_temp"]);
    assert_eq!(ids(&repo, "download"), ["kw_download"]);
    let snapshot = repo.sync_snapshot(&t).unwrap();
    assert!(snapshot.lists.user_list.is_empty());
    assert_eq!(snapshot.lists.default_list[0].id(), "kw_public");
}

#[test]
fn full_replacement_cas_uses_sync_sequence_instead_of_unrelated_repository_revision() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    let cut = repo.sync_snapshot(&t).unwrap();
    repo.record_search("unrelated", None).unwrap();
    repo.sync_apply_remote(
        &t,
        Some(cut.sequence),
        &SyncOperation::Replace(SyncData::List(ListData::default())),
    )
    .unwrap();
    local_add(&repo, "1");
    let before = repo.revision().unwrap();
    assert!(
        repo.sync_apply_remote(
            &t,
            Some(cut.sequence),
            &SyncOperation::Replace(SyncData::List(ListData::default()))
        )
        .is_err()
    );
    assert!(
        repo.sync_apply_remote(
            &t,
            None,
            &SyncOperation::Replace(SyncData::List(ListData::default()))
        )
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
    assert_eq!(ids(&repo, "default"), ["kw_1"]);
}

#[test]
fn cancellation_after_effects_rolls_back_native_rows_revision_journal_and_override() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    let before = repo.revision().unwrap();
    let calls = AtomicUsize::new(0);
    let check = || {
        if calls.fetch_add(1, Ordering::SeqCst) >= 3 {
            anyhow::bail!("Cancelled after SQL effects");
        }
        Ok(())
    };
    let data =
        ListData::from_wire(&json!({"defaultList":[music("1")],"loveList":[],"userList":[]}))
            .unwrap();
    assert!(
        repo.sync_apply_remote_checked(
            &t,
            Some(0),
            &SyncOperation::Replace(SyncData::List(data)),
            &check
        )
        .is_err()
    );
    assert!(repo.list_tracks("default").unwrap().is_empty());
    assert_eq!(repo.revision().unwrap(), before);
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    for table in ["sync_journal", "sync_current", "sync_capture_override"] {
        assert_eq!(count(&sql(&repo), table), 0);
    }
    assert!(repo.changes_since(before, 10).unwrap().is_empty());
}

#[test]
fn sql_sync_failure_is_not_swallowed_as_unavailable_and_user_transaction_rolls_back() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let _t = ticket(&repo, "a");
    let before = repo.revision().unwrap();
    let connection = sql(&repo);
    connection.execute_batch("CREATE TRIGGER reject_sync_journal BEFORE INSERT ON sync_journal BEGIN SELECT RAISE(ABORT,'injected sync journal fault'); END;").unwrap();
    assert!(
        repo.apply_list_action(
            &native::ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![track("1")],
                location: native::AddLocation::Bottom
            },
            None
        )
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
    assert!(repo.list_tracks("default").unwrap().is_empty());
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    assert_eq!(count(&connection, "sync_current"), 0);
    connection
        .execute_batch("DROP TRIGGER reject_sync_journal;")
        .unwrap();
    local_add(&repo, "1");
    repo.integrity_check().unwrap();
}

#[test]
fn unsupported_native_metadata_commits_with_unavailable_marker_without_affecting_other_scope() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let unsupported=TrackRecord::from_lx_dto(json!({"id":"custom_1","name":"Custom","singer":"Artist","source":"custom","meta":{"songId":"1","albumName":"Album"}}),LxMusicDtoVersion::Modern).unwrap();
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "default".into(),
            tracks: vec![unsupported],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    assert_eq!(ids(&repo, "default"), ["custom_1"]);
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 1);
    let connection = sql(&repo);
    let (available, digest): (bool, Option<String>) = connection
        .query_row(
            "SELECT available,digest FROM sync_current WHERE dataset='list'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(!available);
    assert!(digest.is_none());
    let a = ticket(&repo, "a");
    assert!(repo.sync_snapshot(&a).is_err());
    let mut b = peer("b");
    b.capabilities.list = None;
    repo.sync_store_peer(7, &b).unwrap();
    let b = SessionTicket::new("b".into(), 7, 11);
    repo.sync_begin_session(&b).unwrap();
    assert_eq!(repo.sync_snapshot(&b).unwrap().dislike, "");
    assert!(
        repo.sync_apply_remote(
            &b,
            None,
            &SyncOperation::List(ListAction::MusicClear(vec!["default".into()]))
        )
        .is_err()
    );
}

#[test]
fn same_fields_cannot_forge_ticket_and_restart_does_not_restore_live_session_authority() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    let forged = SessionTicket::new("a".into(), 7, 11);
    assert!(repo.sync_snapshot(&forged).is_err());
    repo.sync_begin_session(&t).unwrap();
    assert!(t.is_active());
    repo.sync_snapshot(&t).unwrap();
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(
        repo.sync_peers().unwrap()[0].key.expose_base64(),
        peer("a").key.expose_base64()
    );
    assert!(repo.sync_snapshot(&t).is_err());
    let new = SessionTicket::new("a".into(), 7, 12);
    repo.sync_begin_session(&new).unwrap();
    repo.sync_snapshot(&new).unwrap();
    repo.sync_retire_session(&t).unwrap();
    assert!(repo.sync_snapshot(&new).is_ok());
}

#[test]
fn ticket_retirement_at_commit_check_and_auth_rotation_prevent_late_remote_effects() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    let before = repo.revision().unwrap();
    let calls = AtomicUsize::new(0);
    let check = || {
        if calls.fetch_add(1, Ordering::SeqCst) == 3 {
            t.retire();
        }
        Ok(())
    };
    assert!(
        repo.sync_apply_remote_checked(
            &t,
            None,
            &SyncOperation::List(ListAction::MusicAdd {
                id: "default".into(),
                music: vec![MusicInfo::from_wire(music("1")).unwrap()],
                location: lx_gpui_player::sync::AddLocation::Bottom
            }),
            &check
        )
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
    assert!(repo.list_tracks("default").unwrap().is_empty());
    let mut rotated = peer("a");
    rotated.auth_epoch = 8;
    repo.sync_store_peer(7, &rotated).unwrap();
    assert!(
        repo.sync_begin_session(&SessionTicket::new("a".into(), 7, 12))
            .is_err()
    );
    let fresh = SessionTicket::new("a".into(), 8, 13);
    repo.sync_begin_session(&fresh).unwrap();
    repo.sync_rotate_profile(1).unwrap();
    assert!(!fresh.is_active());
    assert!(repo.sync_snapshot(&fresh).is_err());
}

#[test]
fn revoked_peer_history_is_bounded_and_explicit_forget_is_epoch_checked() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    repo.sync_configure_pairing(1, 7, true).unwrap();
    for i in 0..8 {
        let id = format!("p{i}");
        repo.sync_store_peer(7, &peer(&id)).unwrap();
        repo.sync_revoke_peer(&id).unwrap();
    }
    assert!(repo.sync_peers().unwrap().is_empty());
    assert_eq!(count(&sql(&repo), "sync_peers"), 8);
    assert!(repo.sync_store_peer(7, &peer("ninth")).is_err());
    assert!(repo.sync_store_peer(7, &peer("p0")).is_err());
    assert!(repo.sync_forget_peer("p0", 7).is_err());
    repo.sync_forget_peer("p0", 8).unwrap();
    repo.sync_store_peer(7, &peer("ninth")).unwrap();
    assert!(repo.sync_forget_peer("ninth", 7).is_err());
    let connection = sql(&repo);
    let bytes: i64 = connection
        .query_row("SELECT sum(byte_size) FROM sync_peers", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(bytes <= 8 * 16384);
    let public: String = connection
        .query_row(
            "SELECT document FROM sync_peers WHERE peer_id='ninth'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!public.contains(&peer("ninth").key.expose_base64()));
    assert!(!format!("{:?}", peer("ninth")).contains(&peer("ninth").key.expose_base64()));
}

#[test]
fn incoming_local_references_preserve_opaque_identity_without_asset_or_read_grant() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    let path = p
        .path()
        .join("unapproved-never-created.wav")
        .to_string_lossy()
        .into_owned();
    let value = json!({"id":"local_opaque_exact","name":"Opaque","singer":"Artist","source":"local","interval":null,"meta":{"songId":"opaque","albumName":"Album","filePath":path,"ext":"wav"}});
    let data =
        ListData::from_wire(&json!({"defaultList":[value.clone()],"loveList":[],"userList":[]}))
            .unwrap();
    repo.sync_apply_remote(&t, Some(0), &SyncOperation::Replace(SyncData::List(data)))
        .unwrap();
    assert!(!p.path().join("unapproved-never-created.wav").exists());
    let tracks = repo.list_tracks("default").unwrap();
    assert_eq!(tracks[0].lx_id, "local_opaque_exact");
    assert!(tracks[0].local_asset_id.is_none());
    let snapshot = repo.sync_snapshot(&t).unwrap();
    assert_eq!(snapshot.lists.default_list[0].wire(), &value);
    for table in [
        "local_assets",
        "local_file_grants",
        "library_roots",
        "local_asset_locations",
    ] {
        assert_eq!(count(&sql(&repo), table), 0);
    }
    let mut forbidden = value;
    forbidden["meta"]["permissions"] = json!(["fs.read"]);
    assert!(MusicInfo::from_wire(forbidden).is_err());
}

#[test]
fn real_journal_retention_requires_snapshot_for_gaps_and_never_covers_unsent_record() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    for i in 0..1002 {
        repo.apply_dislike_action(
            &native::DislikeAction::Overwrite {
                rules: format!("song{i}@artist"),
            },
            None,
        )
        .unwrap();
    }
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 1002);
    assert_eq!(count(&sql(&repo), "sync_journal"), 1000);
    assert!(matches!(
        repo.sync_changes_after(&t, 0, 1000).unwrap(),
        SyncChanges::NeedsSnapshot
    ));
    assert!(matches!(
        repo.sync_changes_after(&t, 2, 1).unwrap(),
        SyncChanges::NeedsSnapshot
    ));
    let SyncChanges::Delta { records, snapshot } = repo.sync_changes_after(&t, 1001, 1).unwrap()
    else {
        panic!("Expected last exact event")
    };
    assert_eq!(records[0].sequence, 1002);
    assert_eq!(snapshot.sequence, 1002);
    assert!(repo.sync_changes_after(&t, 1003, 100).is_err());
    assert!(
        repo.sync_confirm_baseline(
            &t,
            &snapshot.data(Dataset::Dislike),
            snapshot.repository_revision,
            1001,
            &snapshot.data(Dataset::Dislike).digest().unwrap()
        )
        .is_err()
    );
    repo.integrity_check().unwrap();
}

#[test]
fn same_trim_md5_keeps_distinct_raw_snapshots_and_exact_old_ack_after_restart() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    repo.sync_apply_remote(
        &t,
        Some(0),
        &SyncOperation::Replace(SyncData::Dislike("a@b".into())),
    )
    .unwrap();
    let old = repo.sync_snapshot(&t).unwrap();
    let old_data = old.data(Dataset::Dislike);
    repo.sync_confirm_baseline(
        &t,
        &old_data,
        old.repository_revision,
        old.sequence,
        &old_data.digest().unwrap(),
    )
    .unwrap();
    repo.sync_apply_remote(
        &t,
        Some(old.sequence),
        &SyncOperation::Replace(SyncData::Dislike(" a@b ".into())),
    )
    .unwrap();
    let new = repo.sync_snapshot(&t).unwrap();
    let new_data = new.data(Dataset::Dislike);
    assert_eq!(old_data.digest().unwrap(), new_data.digest().unwrap());
    assert_ne!(old.dislike, new.dislike);
    assert_eq!(new.sequence, old.sequence + 1);
    assert!(
        repo.sync_confirm_baseline(
            &t,
            &old_data,
            new.repository_revision,
            new.sequence,
            &old_data.digest().unwrap()
        )
        .is_err()
    );
    assert_eq!(
        repo.sync_baseline(&t, Dataset::Dislike)
            .unwrap()
            .unwrap()
            .data,
        old_data
    );
    let rows: i64 = sql(&repo)
        .query_row(
            "SELECT count(*) FROM sync_snapshots WHERE dataset='dislike' AND digest=?1",
            [old_data.digest().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 2);
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    let fresh = SessionTicket::new("a".into(), 7, 12);
    repo.sync_begin_session(&fresh).unwrap();
    assert_eq!(repo.sync_snapshot(&fresh).unwrap().dislike, " a@b ");
    assert_eq!(
        repo.sync_baseline(&fresh, Dataset::Dislike)
            .unwrap()
            .unwrap()
            .data,
        SyncData::Dislike("a@b".into())
    );
    let actual = repo.sync_snapshot(&fresh).unwrap();
    let data = actual.data(Dataset::Dislike);
    repo.sync_confirm_baseline(
        &fresh,
        &data,
        actual.repository_revision,
        actual.sequence,
        &data.digest().unwrap(),
    )
    .unwrap();
    assert_eq!(
        repo.sync_baseline(&fresh, Dataset::Dislike)
            .unwrap()
            .unwrap()
            .data,
        SyncData::Dislike(" a@b ".into())
    );
    repo.integrity_check().unwrap();
}

#[test]
fn oversized_projected_dataset_retains_legitimate_native_commit_as_snapshot_unavailable() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    for batch in 0..60 {
        let tracks = (0..10)
            .map(|index| {
                let mut value = music(&format!("{batch}-{index}"));
                value["name"] = json!("N".repeat(16000));
                TrackRecord::from_lx_dto(value, LxMusicDtoVersion::Modern).unwrap()
            })
            .collect();
        repo.apply_list_action(
            &native::ListAction::MusicAdd {
                list_id: "default".into(),
                tracks,
                location: native::AddLocation::Bottom,
            },
            None,
        )
        .unwrap();
    }
    assert_eq!(repo.list_tracks_page("default", 0, 1).unwrap().total, 600);
    let connection = sql(&repo);
    let (available, digest, sha): (bool, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT available,digest,sha256 FROM sync_current WHERE dataset='list'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(!available);
    assert!(digest.is_none() && sha.is_none());
    let marker: Option<String> = connection
        .query_row(
            "SELECT action_document FROM sync_journal ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // A bounded delta may exist, but the full dataset cannot be sent until it fits.
    if let Some(marker) = marker {
        assert!(marker.len() <= 1024 * 1024);
    }
    assert_eq!(repo.revision().unwrap(), 60);
    repo.integrity_check().unwrap();
}

#[test]
fn fifty_thousand_item_full_replace_is_one_sql_commit_with_small_global_outbox() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    let music=(0..50000).map(|index|MusicInfo::from_wire(json!({"id":format!("k{index}"),"name":"","singer":"","source":"kw","meta":{"songId":index,"albumName":"","qualitys":[],"_qualitys":{}}})).unwrap()).collect();
    let data = ListData {
        default_list: music,
        love_list: Vec::new(),
        user_list: Vec::new(),
    };
    data.validate().unwrap();
    let before = repo.revision().unwrap();
    let (change, receipt) = repo
        .sync_apply_remote(
            &t,
            Some(0),
            &SyncOperation::Replace(SyncData::List(data.clone())),
        )
        .unwrap();
    assert_eq!(change.revision, before + 1);
    assert_eq!(receipt.sequence, 1);
    assert_eq!(receipt.digest, data.digest().unwrap());
    assert_eq!(repo.list_tracks_page("default", 0, 1).unwrap().total, 50000);
    let outbox = repo.changes_since(before, 10).unwrap();
    assert_eq!(outbox.len(), 1);
    assert!(serde_json::to_vec(&outbox[0]).unwrap().len() < 1024);
    let marker: Option<String> = sql(&repo)
        .query_row(
            "SELECT action_document FROM sync_journal WHERE sequence=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(marker.is_none());
    assert_eq!(
        repo.sync_snapshot(&t).unwrap().lists.default_list.len(),
        50000
    );
    repo.integrity_check().unwrap();
}

#[test]
fn fractional_lx_list_time_survives_music_changes_until_actual_native_reorder() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = ticket(&repo, "a");
    apply(
        &repo,
        &t,
        json!({"action":"list_create","data":{"position":0,"listInfos":[{"id":"fraction","name":"Fraction","locationUpdateTime":1.25}]}}),
    );
    apply(
        &repo,
        &t,
        json!({"action":"list_music_add","data":{"id":"fraction","musicInfos":[music("1")],"addMusicLocationType":"bottom"}}),
    );
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "fraction".into(),
            tracks: vec![track("2")],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let snapshot = repo.sync_snapshot(&t).unwrap();
    assert_eq!(
        snapshot.lists.user_list[0].wire()["locationUpdateTime"],
        json!(1.25)
    );
    let data = snapshot.data(Dataset::List);
    repo.sync_confirm_baseline(
        &t,
        &data,
        snapshot.repository_revision,
        snapshot.sequence,
        &data.digest().unwrap(),
    )
    .unwrap();
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    let fresh = SessionTicket::new("a".into(), 7, 12);
    repo.sync_begin_session(&fresh).unwrap();
    assert_eq!(
        repo.sync_snapshot(&fresh).unwrap().lists.user_list[0].wire()["locationUpdateTime"],
        json!(1.25)
    );
    repo.apply_list_action(
        &native::ListAction::UpdatePosition {
            ids: vec!["fraction".into()],
            position: 0,
        },
        None,
    )
    .unwrap();
    let timestamp =
        repo.sync_snapshot(&fresh).unwrap().lists.user_list[0].wire()["locationUpdateTime"]
            .as_u64()
            .unwrap();
    assert!(timestamp > 0);
    assert_eq!(
        repo.sync_baseline(&fresh, Dataset::List)
            .unwrap()
            .unwrap()
            .data,
        data
    );
}

#[test]
fn explicit_actual_tag_update_refreshes_public_sync_snapshot_and_preserves_old_cut() {
    let p = profile();
    let media = p.path().join("media");
    fs::create_dir(&media).unwrap();
    let file = media.join("tagged.wav");
    tagged_wav(&file, "Before");
    let original = approved_local(&file);
    assert_eq!(original.track.title, "Before");
    let repo = Repository::open(p.path().join("profile")).unwrap();
    let t = ticket(&repo, "a");
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "default".into(),
            tracks: vec![original.track],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let old = repo.sync_snapshot(&t).unwrap();
    let old_data = old.data(Dataset::List);
    repo.sync_confirm_baseline(
        &t,
        &old_data,
        old.repository_revision,
        old.sequence,
        &old_data.digest().unwrap(),
    )
    .unwrap();
    tagged_wav(&file, "After actual tag change");
    let updated = approved_local(&file);
    assert_eq!(updated.track.title, "After actual tag change");
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&updated), &|| Ok(()))
        .unwrap();
    let current = repo.sync_snapshot(&t).unwrap();
    assert_eq!(current.sequence, old.sequence + 1);
    assert_eq!(
        current.lists.default_list[0].wire()["name"],
        "After actual tag change"
    );
    assert_eq!(
        repo.list_tracks("default").unwrap()[0].title,
        "After actual tag change"
    );
    assert_eq!(
        repo.sync_baseline(&t, Dataset::List).unwrap().unwrap().data,
        old_data
    );
    assert!(matches!(
        repo.sync_changes_after(&t, old.sequence, 100).unwrap(),
        SyncChanges::NeedsSnapshot
    ));
}

#[test]
fn actual_root_scan_metadata_change_captures_only_public_members_and_ignores_unchanged_tag() {
    let p = profile();
    let media = p.path().join("media");
    fs::create_dir(&media).unwrap();
    let file = media.join("scan.wav");
    tagged_wav(&file, "Before scan");
    let original = approved_local(&file);
    let repo = Repository::open(p.path().join("profile")).unwrap();
    let t = ticket(&repo, "a");
    let (root, _) = repo.add_library_root(&actual_root(&media)).unwrap();
    let (initial, _) = repo.begin_library_scan(&root.root_id, "initial").unwrap();
    repo.apply_library_scan_batch(&initial, &[], std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    finish_scan(&repo, &initial);
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "love".into(),
            tracks: vec![original.track],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let before = repo.sync_snapshot(&t).unwrap();
    tagged_wav(&file, "Changed scanned title");
    let updated = approved_local(&file);
    let (scan, _) = repo.begin_library_scan(&root.root_id, "changed").unwrap();
    repo.apply_library_scan_batch(&scan, &[], std::slice::from_ref(&updated), &|| Ok(()))
        .unwrap();
    finish_scan(&repo, &scan);
    let current = repo.sync_snapshot(&t).unwrap();
    assert_eq!(current.sequence, before.sequence + 1);
    assert_eq!(
        current.lists.love_list[0].wire()["name"],
        "Changed scanned title"
    );
    let (same, _) = repo.begin_library_scan(&root.root_id, "unchanged").unwrap();
    repo.apply_library_scan_batch(&same, &[], std::slice::from_ref(&updated), &|| Ok(()))
        .unwrap();
    finish_scan(&repo, &same);
    assert_eq!(
        repo.sync_profile_state().unwrap().sequence,
        current.sequence
    );
}

#[test]
fn actual_manual_file_relocation_refreshes_opaque_sync_path_and_keeps_archive_identity() {
    use lx_gpui_player::domain::library_watch::{
        LibraryRelinkAuthorization, LibraryRelinkCommit, RelinkEvidence,
    };
    use sha2::{Digest, Sha256};
    let p = profile();
    let media = p.path().join("media");
    fs::create_dir(&media).unwrap();
    let old = media.join("old.wav");
    let new = media.join("new.wav");
    tagged_wav(&old, "Actual file");
    let original = approved_local(&old);
    let repo = Repository::open(p.path().join("profile")).unwrap();
    let t = ticket(&repo, "a");
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "default".into(),
            tracks: vec![original.track.clone()],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let before = repo.sync_snapshot(&t).unwrap();
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    let hash = format!("{:x}", Sha256::digest(fs::read(&old).unwrap()));
    repo.put_library_content_evidence(
        &original.asset.id,
        binding.binding_revision,
        original.file_identity.as_ref().unwrap(),
        original.asset.size_bytes,
        original.asset.modified_ms,
        &hash,
        &|| Ok(()),
    )
    .unwrap();
    fs::rename(&old, &new).unwrap();
    let mut prepared = approved_local(&new);
    prepared.track.id = original.track.id.clone();
    prepared.track.lx_id = original.track.lx_id.clone();
    prepared.track.local_asset_id = Some(original.asset.id.clone());
    prepared.asset.id = original.asset.id.clone();
    prepared.asset.track_id = original.track.id.clone();
    repo.commit_library_relink(
        &LibraryRelinkCommit {
            operation_id: "actual-sync-move".into(),
            expected_binding_revision: binding.binding_revision,
            prepared,
            authorization: LibraryRelinkAuthorization::Manual,
            evidence: RelinkEvidence::SameFileAndContent,
            content_sha256: Some(hash),
            allow_content_change: false,
        },
        &|| Ok(()),
    )
    .unwrap();
    let current = repo.sync_snapshot(&t).unwrap();
    assert_eq!(current.sequence, before.sequence + 1);
    let music = current.lists.default_list[0].wire();
    assert_eq!(music["id"], before.lists.default_list[0].wire()["id"]);
    assert_eq!(
        lx_gpui_player::domain::music::normalize_local_path(Path::new(
            music["meta"]["filePath"].as_str().unwrap()
        )),
        lx_gpui_player::domain::music::normalize_local_path(&fs::canonicalize(&new).unwrap())
    );
    assert!(matches!(
        repo.sync_changes_after(&t, before.sequence, 100).unwrap(),
        SyncChanges::NeedsSnapshot
    ));
    repo.integrity_check().unwrap();
}

#[test]
fn four_hundred_one_actual_library_files_without_public_members_do_not_initialize_sync_cut() {
    let p = profile();
    let media = p.path().join("media");
    fs::create_dir(&media).unwrap();
    let repo = Repository::open(p.path().join("profile")).unwrap();
    let (root, _) = repo.add_library_root(&actual_root(&media)).unwrap();
    let mut items = Vec::new();
    for index in 0..401 {
        let path = media.join(format!("{index:03}.wav"));
        tagged_wav(&path, &format!("Track {index}"));
        items.push(approved_local(&path));
    }
    let (token, _) = repo
        .begin_library_scan(&root.root_id, "401-no-public")
        .unwrap();
    for batch in items.chunks(200) {
        repo.apply_library_scan_batch(&token, &[], batch, &|| Ok(()))
            .unwrap();
    }
    finish_scan(&repo, &token);
    assert_eq!(repo.local_assets().unwrap().len(), 401);
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    assert_eq!(count(&sql(&repo), "sync_current"), 0);
    assert_eq!(count(&sql(&repo), "sync_journal"), 0);
    repo.apply_list_action(
        &native::ListAction::MusicAdd {
            list_id: "temp".into(),
            tracks: vec![items[0].track.clone()],
            location: native::AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let path = media.join("000.wav");
    tagged_wav(&path, "Only private title changes");
    let updated = approved_local(&path);
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&updated), &|| Ok(()))
        .unwrap();
    assert_eq!(
        repo.list_tracks("temp").unwrap()[0].title,
        "Only private title changes"
    );
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    assert_eq!(count(&sql(&repo), "sync_current"), 0);
}

#[test]
fn saved_credentials_require_explicit_client_role_exact_id_and_current_private_key() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    repo.sync_configure_pairing(1, 7, true).unwrap();
    repo.sync_store_peer(7, &peer("unknown")).unwrap();
    repo.sync_store_server_peer_with_commit_guard(7, &peer("incoming-client"), &|| Ok(()), || {
        Ok(())
    })
    .unwrap();
    repo.sync_store_client_peer_with_commit_guard(7, &peer("selected-server"), &|| Ok(()), || {
        Ok(())
    })
    .unwrap();
    for id in ["unknown", "incoming-client", "missing-server"] {
        let error = repo.sync_client_credentials(id).unwrap_err();
        assert!(!error.to_string().contains(&peer(id).key.expose_base64()));
    }
    let credentials = repo.sync_client_credentials("selected-server").unwrap();
    assert_eq!(credentials.server_id, "selected-server");
    assert_eq!(credentials.client_id, "client-selected-server");
    assert_eq!(
        credentials.key.expose_base64(),
        peer("selected-server").key.expose_base64()
    );
    assert!(!format!("{credentials:?}").contains(&credentials.key.expose_base64()));
    assert!(
        repo.sync_store_server_peer_with_commit_guard(
            7,
            &peer("selected-server"),
            &|| Ok(()),
            || Ok(())
        )
        .is_err()
    );
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(
        repo.sync_client_credentials("selected-server")
            .unwrap()
            .client_id,
        "client-selected-server"
    );
    repo.sync_revoke_peer("selected-server").unwrap();
    assert!(repo.sync_client_credentials("selected-server").is_err());
}
