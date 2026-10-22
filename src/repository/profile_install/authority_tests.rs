use super::*;
use crate::{
    domain::{library::LibraryPreparedAsset, lists as native, music::LxMusicDtoVersion},
    library::{
        authority::RootAuthority, metadata::read_local_metadata_from_open_file, scan::ScanContext,
    },
    repository::snapshot_sanitize,
    sync::{
        Dataset, PeerCapabilities, SharedKey,
        network::{PeerRecord, SessionTicket, SyncChanges},
    },
};
use serde_json::json;
use std::{fs, path::Path};

fn folder() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn wav(path: &Path) {
    let mut body = b"WAVEfmt ".to_vec();
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
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    fs::write(path, bytes).unwrap();
}
fn selected_file(path: &Path) -> LibraryPreparedAsset {
    let root = RootAuthority::open(
        path.parent().unwrap(),
        false,
        &ScanContext::new(Duration::from_secs(20)),
    )
    .unwrap();
    let file = root.open_file(path).unwrap();
    let identity = file.identity().clone();
    let bytes = file.metadata().unwrap().len();
    let metadata = read_local_metadata_from_open_file(file, path.to_path_buf(), bytes).unwrap();
    let (mut track, asset) = crate::repository::local_track_from_path(path).unwrap();
    track.duration_ms = Some(metadata.duration_ms);
    track.metadata.insert("ext".into(), json!("wav"));
    let barrier = root.retirement_barrier();
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    LibraryPreparedAsset {
        track,
        asset,
        file_identity: Some(identity),
    }
}
fn selected_root(path: &Path) -> crate::domain::library::LibraryRootDraft {
    let root =
        RootAuthority::open(path, false, &ScanContext::new(Duration::from_secs(20))).unwrap();
    let draft = root.draft();
    let barrier = root.retirement_barrier();
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    draft
}
fn track(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"id":format!("kw_{id}"),"name":id,"singer":"Fixture","source":"kw","interval":"00:01","meta":{"songId":id,"albumName":"","qualitys":[],"_qualitys":{}}}), LxMusicDtoVersion::Modern).unwrap()
}
fn add(repo: &Repository, id: &str) {
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
fn peer(id: &str, key: u8) -> PeerRecord {
    PeerRecord {
        peer_id: id.into(),
        client_id: format!("client-{id}"),
        key: SharedKey::from_bytes([key; 16]),
        device_name: "Authority fixture".into(),
        is_mobile: false,
        auth_epoch: 7,
        capabilities: PeerCapabilities {
            list: Some(false),
            dislike: Some(false),
        },
    }
}
fn pair(repo: &Repository, key: u8, epoch: u64) -> SessionTicket {
    repo.sync_configure_pairing(1, 7, true).unwrap();
    repo.sync_store_peer(7, &peer("peer", key)).unwrap();
    let ticket = SessionTicket::new("peer".into(), 7, epoch);
    repo.sync_begin_session(&ticket).unwrap();
    ticket
}
fn copy_into(
    current: &Repository,
    candidate: &Repository,
) -> (CurrentAuthority, PhysicalRestoreAuthorityReport) {
    let current = current.connection.lock().unwrap();
    let mut candidate = candidate.connection.lock().unwrap();
    CurrentAuthority::copy_into(&current, &mut candidate, &|| Ok(())).unwrap()
}
fn sanitize(candidate: &Repository) {
    snapshot_sanitize::purge_snapshot_authority(&mut candidate.connection.lock().unwrap(), &|| {
        Ok(())
    })
    .unwrap();
}
fn document(repo: &Repository, table: &str, key: &str, id: &str) -> String {
    repo.connection
        .lock()
        .unwrap()
        .query_row(
            &format!("SELECT document FROM {table} WHERE {key}=?1"),
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn current_actual_track_asset_and_locator_override_forged_same_id_incoming_claims() {
    let folder = folder();
    let selected = folder.path().join("selected.wav");
    let foreign = folder.path().join("foreign.wav");
    wav(&selected);
    wav(&foreign);
    let current = Repository::open(folder.path().join("current")).unwrap();
    let candidate = Repository::open(folder.path().join("candidate")).unwrap();
    let selected_asset = selected_file(&selected);
    let foreign_asset = selected_file(&foreign);
    current
        .upsert_explicit_library_tracks(std::slice::from_ref(&selected_asset), &|| Ok(()))
        .unwrap();
    candidate
        .upsert_explicit_library_tracks(
            &[selected_asset.clone(), foreign_asset.clone()],
            &|| Ok(()),
        )
        .unwrap();
    candidate
        .apply_list_action(
            &native::ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![selected_asset.track.clone()],
                location: native::AddLocation::Bottom,
            },
            None,
        )
        .unwrap();
    let current_track = document(&current, "tracks", "id", &selected_asset.track.id);
    let current_asset = document(
        &current,
        "local_assets",
        "identity_key",
        &selected_asset.asset.id,
    );
    let current_binding = current
        .library_asset_binding(&selected_asset.asset.id)
        .unwrap()
        .unwrap();
    let mut forged_track = selected_asset.track.clone();
    forged_track.platform.song_id = foreign.to_str().unwrap().into();
    forged_track
        .metadata
        .insert("filePath".into(), json!(foreign));
    let mut forged_asset = selected_asset.asset.clone();
    forged_asset.path = foreign.clone();
    {
        let sql = candidate.connection.lock().unwrap();
        let track = serde_json::to_string(&forged_track).unwrap();
        sql.execute(
            "UPDATE tracks SET document=?2 WHERE id=?1",
            params![forged_track.id, track],
        )
        .unwrap();
        sql.execute(
            "UPDATE music_list_items SET document=?2 WHERE track_id=?1",
            params![forged_track.id, track],
        )
        .unwrap();
        sql.execute(
            "UPDATE local_assets SET document=?2 WHERE identity_key=?1",
            params![
                forged_asset.id,
                serde_json::to_string(&forged_asset).unwrap()
            ],
        )
        .unwrap();
    }
    sanitize(&candidate);
    let (authority, report) = copy_into(&current, &candidate);
    assert_eq!(
        CurrentAuthority::digest_current(&current.connection.lock().unwrap(), &|| Ok(())).unwrap(),
        authority.digest()
    );
    authority
        .verify_current(&current.connection.lock().unwrap(), &|| Ok(()))
        .unwrap();
    assert_eq!(report.matched_assets, 1);
    assert!(
        document(&candidate, "tracks", "id", &selected_asset.track.id) == current_track,
        "Current trusted track was not retained"
    );
    assert!(
        document(
            &candidate,
            "local_assets",
            "identity_key",
            &selected_asset.asset.id
        ) == current_asset,
        "Current trusted asset was not retained"
    );
    assert_eq!(
        candidate
            .library_asset_binding(&selected_asset.asset.id)
            .unwrap()
            .unwrap(),
        current_binding
    );
    let item = &candidate.list_tracks("default").unwrap()[0];
    assert_eq!(item.platform.song_id, selected_asset.track.platform.song_id);
    assert_eq!(
        item.metadata["filePath"],
        selected_asset.track.metadata["filePath"]
    );
    assert!(
        candidate
            .local_asset_authority(&selected_asset.asset.id)
            .unwrap()
            .is_some()
    );
    assert!(
        candidate
            .local_asset_authority(&foreign_asset.asset.id)
            .unwrap()
            .is_none()
    );
    assert!(
        document(&current, "tracks", "id", &selected_asset.track.id) == current_track,
        "Current source track changed"
    );
    assert_eq!(fs::read(&selected).unwrap(), fs::read(&foreign).unwrap());
    candidate.integrity_check().unwrap();
}

#[test]
fn current_actual_root_is_preserved_without_an_incoming_root_match() {
    let folder = folder();
    let root_path = folder.path().join("selected-root");
    let foreign_path = folder.path().join("foreign-root");
    fs::create_dir(&root_path).unwrap();
    fs::create_dir(&foreign_path).unwrap();
    let current = Repository::open(folder.path().join("current")).unwrap();
    let candidate = Repository::open(folder.path().join("candidate")).unwrap();
    let (root, _) = current
        .add_library_root(&selected_root(&root_path))
        .unwrap();
    let (foreign, _) = candidate
        .add_library_root(&selected_root(&foreign_path))
        .unwrap();
    sanitize(&candidate);
    assert!(candidate.library_root(&root.root_id).unwrap().is_none());
    let (_authority, report) = copy_into(&current, &candidate);
    assert_eq!(report.preserved_roots, 1);
    assert_eq!(
        candidate.library_root(&root.root_id).unwrap().unwrap(),
        root
    );
    assert!(candidate.library_root(&foreign.root_id).unwrap().is_none());
    assert_eq!(
        candidate.library_watch(&root.root_id).unwrap(),
        current.library_watch(&root.root_id).unwrap()
    );
    assert_eq!(
        candidate.library_dirty(&root.root_id).unwrap(),
        current.library_dirty(&root.root_id).unwrap()
    );
    candidate.integrity_check().unwrap();
}

#[test]
fn current_peer_reauthenticates_and_confirms_fresh_full_cut_after_foreign_cursor_reset() {
    let folder = folder();
    let current = Repository::open(folder.path().join("current")).unwrap();
    let candidate = Repository::open(folder.path().join("candidate")).unwrap();
    let old_ticket = pair(&current, 7, 11);
    add(&current, "old-current");
    let old = current.sync_snapshot(&old_ticket).unwrap();
    let old_data = old.data(Dataset::List);
    current
        .sync_confirm_baseline(
            &old_ticket,
            &old_data,
            old.repository_revision,
            old.sequence,
            &old_data.digest().unwrap(),
        )
        .unwrap();
    let foreign_ticket = pair(&candidate, 9, 12);
    add(&candidate, "imported");
    candidate.sync_snapshot(&foreign_ticket).unwrap();
    candidate
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE sync_profile_state SET sequence=999,journal_floor=500",
            [],
        )
        .unwrap();
    sanitize(&candidate);
    let (_authority, report) = copy_into(&current, &candidate);
    assert_eq!(report.sync_sequence, old.sequence + 1);
    assert_eq!(
        candidate.sync_profile_state().unwrap().journal_floor,
        report.sync_sequence
    );
    assert!(candidate.sync_snapshot(&foreign_ticket).is_err());
    assert!(candidate.sync_snapshot(&old_ticket).is_err());
    let saved = candidate.sync_peers().unwrap();
    assert_eq!(saved.len(), 1);
    assert!(
        saved[0].key.expose_base64() == peer("peer", 7).key.expose_base64(),
        "Current private credential was not retained"
    );
    let fresh = SessionTicket::new("peer".into(), 7, 13);
    candidate.sync_begin_session(&fresh).unwrap();
    assert!(
        candidate
            .sync_baseline(&fresh, Dataset::List)
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        candidate
            .sync_changes_after(&fresh, old.sequence, 100)
            .unwrap(),
        SyncChanges::NeedsSnapshot
    ));
    assert!(
        candidate
            .sync_confirm_baseline(
                &fresh,
                &old_data,
                old.repository_revision,
                old.sequence,
                &old_data.digest().unwrap()
            )
            .is_err()
    );
    let restored = candidate.sync_snapshot(&fresh).unwrap();
    assert_eq!(restored.lists.default_list.len(), 1);
    assert_eq!(restored.lists.default_list[0].id(), "kw_imported");
    let data = restored.data(Dataset::List);
    candidate
        .sync_confirm_baseline(
            &fresh,
            &data,
            restored.repository_revision,
            restored.sequence,
            &data.digest().unwrap(),
        )
        .unwrap();
    add(&candidate, "after-full-cut");
    let SyncChanges::Delta { records, snapshot } = candidate
        .sync_changes_after(&fresh, restored.sequence, 100)
        .unwrap()
    else {
        panic!("Fresh full cut must allow actual incremental synchronization");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(snapshot.sequence, restored.sequence + 1);
    candidate.integrity_check().unwrap();
}

#[test]
fn nonrevision_peer_changes_reject_installation_and_capture_reads_one_actual_wal_snapshot() {
    let folder = folder();
    let current = Repository::open(folder.path().join("current")).unwrap();
    let candidate = Repository::open(folder.path().join("candidate")).unwrap();
    pair(&current, 7, 21);
    let writer = Connection::open(current.path()).unwrap();
    let old_revision = current.revision().unwrap();
    let prior =
        CurrentAuthority::digest_current(&current.connection.lock().unwrap(), &|| Ok(())).unwrap();
    let changed = std::cell::Cell::new(false);
    let current_sql = current.connection.lock().unwrap();
    let mut candidate_sql = candidate.connection.lock().unwrap();
    let (authority, _report) =
        CurrentAuthority::copy_into(&current_sql, &mut candidate_sql, &|| {
            if !changed.replace(true) {
                writer.execute(
                    "UPDATE sync_peers SET revoked=1,private_key=NULL WHERE peer_id='peer'",
                    [],
                )?;
            }
            Ok(())
        })
        .unwrap();
    assert!(changed.get());
    assert_eq!(authority.digest(), prior);
    assert!(authority.verify_current(&current_sql, &|| Ok(())).is_err());
    drop(current_sql);
    drop(candidate_sql);
    assert_eq!(current.revision().unwrap(), old_revision);
    assert!(current.sync_peers().unwrap().is_empty());
}

#[test]
fn physical_discard_admission_rejects_active_work_but_close_fingerprint_remains_available() {
    let folder = folder();
    let path = folder.path().join("root");
    fs::create_dir(&path).unwrap();
    let current = Repository::open(folder.path().join("current")).unwrap();
    let candidate = Repository::open(folder.path().join("candidate")).unwrap();
    let (root, _) = current.add_library_root(&selected_root(&path)).unwrap();
    current
        .begin_library_scan(&root.root_id, "retained-scan")
        .unwrap();
    let sql = current.connection.lock().unwrap();
    let mut incoming = candidate.connection.lock().unwrap();
    let rejected = CurrentAuthority::copy_into(&sql, &mut incoming, &|| Ok(()));
    assert!(rejected.is_err());
    assert!(CurrentAuthority::digest_current(&sql, &|| Ok(())).is_ok());
    assert!(sql.is_autocommit());
    assert_eq!(
        sql.query_row(
            "SELECT state FROM library_scan_runs WHERE operation_id='retained-scan'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "running"
    );
}

#[path = "authority_network_tests.rs"]
mod network_tests;

#[cfg(windows)]
#[path = "authority_owned_tests.rs"]
mod owned_tests;

#[test]
fn actual_exclusive_preclose_cut_blocks_nonrevision_sql_writes_until_connection_close() {
    let folder = folder();
    let current = Repository::open(folder.path().join("current")).unwrap();
    pair(&current, 7, 41);
    let writer = Connection::open(current.path()).unwrap();
    writer.busy_timeout(Duration::ZERO).unwrap();
    let continuity = current.profile_guard.continuity_token().unwrap();
    let (cut, identity) = {
        let sql = current.connection.lock().unwrap();
        sql.pragma_update(None, "locking_mode", "EXCLUSIVE")
            .unwrap();
        sql.execute_batch(
            "BEGIN EXCLUSIVE; UPDATE repository_state SET revision=revision; COMMIT;",
        )
        .unwrap();
        let (busy, pages, checkpointed): (i64, i64, i64) = sql
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 0);
        assert_eq!(pages, checkpointed);
        let proof =
            unsafe { SourceFileProof::from_sqlite_main(sql.handle(), current.path()) }.unwrap();
        let identity = proof.metadata().unwrap().identity.clone();
        let cut = CurrentAuthority::capture_close_cut(&sql, &|| Ok(())).unwrap();
        let refused = writer
            .execute("UPDATE sync_profile_state SET pairing_enabled=0", [])
            .unwrap_err();
        assert!(matches!(
            refused.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ));
        proof.verify().unwrap();
        (cut, identity)
    };
    let guard = Arc::new(current).close_into_guard().unwrap();
    guard.verify_continuity(&continuity).unwrap();
    assert_eq!(guard.closed_revision(), Some(cut.revision));
    assert_eq!(guard.closed_main_identity(), Some(&identity));
    assert_eq!(cut.digest.len(), 64);
    assert!(
        writer
            .execute("UPDATE sync_profile_state SET pairing_enabled=0", [])
            .is_ok()
    );
}

#[test]
fn cancelled_overlay_rolls_back_candidate_authority_and_current_clock_transition() {
    let folder = folder();
    let path = folder.path().join("incoming-root");
    fs::create_dir(&path).unwrap();
    let current = Repository::open(folder.path().join("current")).unwrap();
    let candidate = Repository::open(folder.path().join("candidate")).unwrap();
    pair(&current, 7, 31);
    candidate.add_library_root(&selected_root(&path)).unwrap();
    sanitize(&candidate);
    let mut sql = candidate.connection.lock().unwrap();
    let before = CurrentAuthority::digest_current(&sql, &|| Ok(())).unwrap();
    let calls = std::cell::Cell::new(0usize);
    let current_sql = current.connection.lock().unwrap();
    let result = CurrentAuthority::copy_into(&current_sql, &mut sql, &|| {
        calls.set(calls.get() + 1);
        ensure!(calls.get() < 20, "Actual overlay cancelled");
        Ok(())
    });
    assert!(result.is_err());
    assert!(calls.get() >= 20);
    assert!(sql.is_autocommit());
    assert_eq!(
        CurrentAuthority::digest_current(&sql, &|| Ok(())).unwrap(),
        before
    );
    schema::integrity_check(&sql).unwrap();
}

#[test]
fn oversized_actual_current_row_is_rejected_before_copy_and_candidate_transaction_rolls_back() {
    let folder = folder();
    let selected = folder.path().join("s.wav");
    wav(&selected);
    let current = Repository::open(folder.path().join("current")).unwrap();
    let candidate = Repository::open(folder.path().join("candidate")).unwrap();
    let item = selected_file(&selected);
    current
        .upsert_explicit_library_tracks(&[item], &|| Ok(()))
        .unwrap();
    current
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE tracks SET document=json_set(document,'$.metadata.oversized',?1)",
            ["x".repeat(MAX_AUTHORITY_ROW_BYTES)],
        )
        .unwrap();
    sanitize(&candidate);
    let current_sql = current.connection.lock().unwrap();
    let mut candidate_sql = candidate.connection.lock().unwrap();
    let before = CurrentAuthority::digest_current(&candidate_sql, &|| Ok(())).unwrap();
    let error = CurrentAuthority::copy_into(&current_sql, &mut candidate_sql, &|| Ok(()))
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("row exceeds its streaming bound")
    );
    assert!(candidate_sql.is_autocommit());
    assert!(current_sql.is_autocommit());
    assert_eq!(
        CurrentAuthority::digest_current(&candidate_sql, &|| Ok(())).unwrap(),
        before
    );
    schema::integrity_check(&candidate_sql).unwrap();
}

#[test]
fn streaming_input_accounting_retains_total_row_byte_and_capture_quotas() {
    let row = [ValueRef::Text(b"actual-cell"), ValueRef::Integer(1)];
    let mut boundary = InputBudget::default();
    boundary.row(&row).unwrap();
    let row_bytes = boundary.bytes;
    boundary.bytes = MAX_AUTHORITY_BYTES - row_bytes;
    boundary.rows = MAX_AUTHORITY_ROWS - 1;
    boundary.row(&row).unwrap();
    assert_eq!(
        (boundary.bytes, boundary.rows),
        (MAX_AUTHORITY_BYTES, MAX_AUTHORITY_ROWS)
    );
    assert!(boundary.row(&row).is_err());
    let mut byte_overflow = InputBudget {
        bytes: MAX_AUTHORITY_BYTES,
        rows: 0,
    };
    assert!(byte_overflow.row(&row).is_err());
    let captures = (0..MAX_AUTHORITY_CAPTURES)
        .map(|_| AuthorityReservation::acquire().unwrap())
        .collect::<Vec<_>>();
    assert!(AuthorityReservation::acquire().is_err());
    drop(captures);
    assert!(AuthorityReservation::acquire().is_ok());
}
