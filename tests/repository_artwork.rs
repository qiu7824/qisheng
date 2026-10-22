use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use lx_gpui_player::{
    domain::{
        artwork::*,
        music::{LxMusicDtoVersion, TrackRecord},
    },
    library::artwork_validator::{VerifiedArtwork, validate_artwork},
    repository::{DATABASE_NAME, Repository, SCHEMA_VERSION},
};
use rusqlite::{Connection, params};
use serde_json::json;
use std::{fs, net::TcpListener};

fn profile() -> tempfile::TempDir {
    let base = std::env::temp_dir().join("lx-native-repository-artwork-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("artwork-")
        .tempdir_in(base)
        .unwrap()
}
fn track(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"id":format!("kw_{id}"),"source":"kw","name":"Original Artwork","singer":"Fixture Artist","interval":"00:30","meta":{"songId":id,"albumName":"Fixture Album","qualitys":[{"type":"128k"}]}}),LxMusicDtoVersion::Modern).unwrap()
}
fn png(id: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            &[id as u8, (id >> 8) as u8, (id >> 16) as u8, 255],
            1,
            1,
            ExtendedColorType::Rgba8,
        )
        .unwrap();
    bytes
}
fn verified(id: u32) -> VerifiedArtwork {
    validate_artwork(png(id), Some("image/png"), || Ok(())).unwrap()
}
fn remote(id: &str, url: &str, blob: Option<&VerifiedArtwork>, now: u64) -> ArtworkInfo {
    ArtworkInfo::new(
        ArtworkKey::new(&track(id), None, ArtworkAuthority::Native).unwrap(),
        ArtworkProvenance::Metadata {
            platform: "kw".into(),
        },
        Some(ArtworkUrl::new(url).unwrap()),
        blob.map(|blob| blob.metadata().clone()),
        now,
    )
    .unwrap()
}

#[test]
fn information_and_sha_deduplicated_bytes_survive_reopen_without_network_or_user_mutations() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let original = track("original");
    repo.upsert_track(&original).unwrap();
    let revision = repo.revision().unwrap();
    let changes = repo.changes_since(0, 100).unwrap();
    let lists = repo.list_data().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://{}/cover.png?private=fixture",
        listener.local_addr().unwrap()
    );
    let blob = verified(1);
    let first = remote("first", &url, Some(&blob), 1000);
    let second = remote("second", &url, Some(&blob), 1000);
    repo.put_artwork(&first, Some(&blob)).unwrap();
    repo.put_artwork(&second, Some(&blob)).unwrap();
    assert_eq!(
        repo.artwork_info(&first.key, 1001).unwrap(),
        ArtworkLookup::Hit(Box::new(first.clone()))
    );
    let stats = repo.image_cache_stats().unwrap();
    assert_eq!(stats.info_entries, 2);
    assert_eq!(stats.blob_entries, 1);
    assert_eq!(stats.blob_bytes, blob.encoded().len() as u64);
    assert!(
        stats.database_bytes.is_none()
            && stats.wal_bytes.is_none()
            && stats.shared_memory_bytes.is_none()
    );
    assert!(!format!("{first:?}").contains("private=fixture"));
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.changes_since(0, 100).unwrap(), changes);
    assert_eq!(repo.list_data().unwrap(), lists);
    assert_eq!(repo.track(&original.id).unwrap(), Some(original));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repo.artwork_info(&second.key, 1002).unwrap(),
        ArtworkLookup::Hit(Box::new(second))
    );
    let (meta, bytes) = repo
        .stored_artwork_blob(&blob.metadata().sha256, 1002)
        .unwrap()
        .unwrap();
    let decoded = validate_artwork(bytes, Some(&meta.mime_type), || Ok(())).unwrap();
    assert_eq!(decoded.metadata(), &meta);
    assert_eq!(decoded.encoded(), blob.encoded());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(repo.revision().unwrap(), revision);
    repo.integrity_check().unwrap();
}

#[test]
fn ttl_rollback_identity_permission_and_stored_blob_integrity_are_checked() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let blob = verified(2);
    let info = remote("ttl", "https://cover.invalid/a.png", Some(&blob), 1000);
    repo.put_artwork(&info, Some(&blob)).unwrap();
    assert_eq!(
        repo.artwork_info(&info.key, info.expire_at_ms).unwrap(),
        ArtworkLookup::Expired
    );
    assert_eq!(
        repo.artwork_info(&info.key, 1001).unwrap(),
        ArtworkLookup::Miss
    );
    repo.put_artwork(&info, None).unwrap();
    assert!(matches!(
        repo.artwork_info(&info.key, 1200).unwrap(),
        ArtworkLookup::Hit(_)
    ));
    assert_eq!(
        repo.artwork_info(&info.key, 1199).unwrap(),
        ArtworkLookup::ClockRollback
    );
    let mut changed = track("ttl");
    changed
        .metadata
        .insert("hash".into(), "changed-hash".into());
    let changed_key = ArtworkKey::new(&changed, None, ArtworkAuthority::Native).unwrap();
    assert_eq!(
        repo.artwork_info(&changed_key, 1300).unwrap(),
        ArtworkLookup::Miss
    );
    assert!(
        ArtworkInfo::new(
            info.key.clone(),
            ArtworkProvenance::Source {
                source_sha256: "a".repeat(64),
                policy_version: 1
            },
            info.url.clone(),
            None,
            1000
        )
        .is_err()
    );
    let connection = Connection::open(repo.path()).unwrap();
    let mut bytes = blob.encoded().to_vec();
    bytes[40] ^= 1;
    connection
        .execute(
            "UPDATE artwork_blobs SET data=?1 WHERE sha256=?2",
            params![bytes, blob.metadata().sha256],
        )
        .unwrap();
    let (meta, corrupt) = repo
        .stored_artwork_blob(&blob.metadata().sha256, 1300)
        .unwrap()
        .unwrap();
    assert_eq!(meta, blob.metadata().clone());
    assert!(validate_artwork(corrupt, Some(&meta.mime_type), || Ok(())).is_err());
    assert_eq!(repo.revision().unwrap(), 0);
    assert!(repo.changes_since(0, 100).unwrap().is_empty());
}

#[test]
fn byte_information_mismatch_and_second_table_failure_leave_no_partial_cache_rows() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let first = verified(3);
    let second = verified(4);
    let info = remote("atomic", "https://cover.invalid/a.png", Some(&first), 1000);
    assert!(repo.put_artwork(&info, Some(&second)).is_err());
    assert!(repo.put_artwork(&info, None).is_err());
    assert_eq!(repo.image_cache_stats().unwrap().info_entries, 0);
    assert_eq!(repo.image_cache_stats().unwrap().blob_entries, 0);
    let connection = Connection::open(repo.path()).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_artwork BEFORE INSERT ON artwork_info BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(repo.put_artwork(&info, Some(&first)).is_err());
    assert_eq!(repo.image_cache_stats().unwrap().info_entries, 0);
    assert_eq!(repo.image_cache_stats().unwrap().blob_entries, 0);
    connection
        .execute_batch("DROP TRIGGER reject_artwork;")
        .unwrap();
    repo.put_artwork(&info, Some(&first)).unwrap();
    assert_eq!(repo.image_cache_stats().unwrap().blob_entries, 1);
    assert_eq!(repo.revision().unwrap(), 0);
}

#[test]
fn image_cache_clear_preserves_foreign_tables_tracks_lists_and_outbox() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let track = track("formal");
    repo.upsert_track(&track).unwrap();
    let revision = repo.revision().unwrap();
    let changes = repo.changes_since(0, 100).unwrap();
    let lists = repo.list_data().unwrap();
    let connection = Connection::open(repo.path()).unwrap();
    connection.execute_batch("CREATE TABLE formal_project_notes(id INTEGER PRIMARY KEY,document TEXT); INSERT INTO formal_project_notes VALUES(1,'retain professional material');").unwrap();
    let blob = verified(5);
    repo.put_artwork(
        &remote("formal", "https://cover.invalid/a.png", Some(&blob), 1000),
        Some(&blob),
    )
    .unwrap();
    let after = repo.clear_image_cache().unwrap();
    assert_eq!(after.info_entries, 0);
    assert_eq!(after.blob_entries, 0);
    assert_eq!(after.blob_bytes, 0);
    assert_eq!(repo.image_cache_stats().unwrap(), after);
    assert_eq!(
        connection
            .query_row(
                "SELECT document FROM formal_project_notes WHERE id=1",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "retain professional material"
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.changes_since(0, 100).unwrap(), changes);
    assert_eq!(repo.list_data().unwrap(), lists);
    assert_eq!(repo.track(&track.id).unwrap(), Some(track));
}

#[test]
fn embedded_artwork_is_bound_to_the_exact_local_asset_permission_stamp() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let mut original = track("local");
    original.platform.source = "local".into();
    let mut asset = lx_gpui_player::domain::music::LocalAsset::new(
        original.id.clone(),
        profile.path().join("selected.flac"),
        1024,
    );
    asset.modified_ms = Some(1000);
    asset.fingerprint = Some("a".repeat(64));
    original.local_asset_id = Some(asset.id.clone());
    let permission = local_artwork_permission(&asset).unwrap();
    let blob = verified(21);
    let key = ArtworkKey::new(
        &original,
        None,
        ArtworkAuthority::LocalAsset {
            asset_id: asset.id.clone(),
            permission_sha256: permission.clone(),
        },
    )
    .unwrap();
    let info = ArtworkInfo::new(
        key.clone(),
        ArtworkProvenance::Embedded {
            asset_id: asset.id.clone(),
            permission_sha256: permission.clone(),
        },
        None,
        Some(blob.metadata().clone()),
        1000,
    )
    .unwrap();
    repo.put_artwork(&info, Some(&blob)).unwrap();
    assert_eq!(
        repo.artwork_info(&key, 1001).unwrap(),
        ArtworkLookup::Hit(Box::new(info))
    );
    asset.modified_ms = Some(1001);
    let changed = local_artwork_permission(&asset).unwrap();
    assert_ne!(changed, permission);
    let changed_key = ArtworkKey::new(
        &original,
        None,
        ArtworkAuthority::LocalAsset {
            asset_id: asset.id.clone(),
            permission_sha256: changed.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        repo.artwork_info(&changed_key, 1002).unwrap(),
        ArtworkLookup::Miss
    );
    assert!(
        ArtworkInfo::new(
            key,
            ArtworkProvenance::Embedded {
                asset_id: asset.id.clone(),
                permission_sha256: changed
            },
            None,
            Some(blob.metadata().clone()),
            1000
        )
        .is_err()
    );
    asset.available = false;
    assert!(local_artwork_permission(&asset).is_err());
    assert_eq!(repo.revision().unwrap(), 0);
}

fn insert_info(tx: &rusqlite::Transaction<'_>, info: &ArtworkInfo) {
    let doc = serde_json::to_string(info).unwrap();
    tx.execute("INSERT INTO artwork_info(cache_key,original_track_id,original_record_sha256,resolved_at_ms,expire_at_ms,last_access_ms,byte_size,blob_sha256,document) VALUES(?1,?2,?3,?4,?5,?4,?6,NULL,?7)",params![info.key.fingerprint().unwrap(),info.key.original_track_id,info.key.original_record_sha256,info.resolved_at_ms as i64,info.expire_at_ms as i64,doc.len()as i64,doc]).unwrap();
}
fn quota_info(index: usize, long: bool) -> ArtworkInfo {
    let mut id = format!("quota-{index:08}");
    if long {
        id.push_str(&"x".repeat(1024 - id.len()));
    }
    let mut url = format!("https://cover.invalid/{index:08}.png?");
    if long {
        url.push_str(&"x".repeat(2048 - url.len()));
    }
    ArtworkInfo::new(
        ArtworkKey {
            version: ARTWORK_VERSION,
            original_track_id: id,
            original_platform: "kw".into(),
            original_record_sha256: "c".repeat(64),
            matched_record_sha256: None,
            authority: ArtworkAuthority::Native,
        },
        ArtworkProvenance::Metadata {
            platform: "kw".into(),
        },
        Some(ArtworkUrl::new(url).unwrap()),
        None,
        1,
    )
    .unwrap()
}

#[test]
fn information_row_and_byte_quotas_remain_logical_and_pages_are_bounded() {
    for long in [false, true] {
        let profile = profile();
        let repo = Repository::open(profile.path()).unwrap();
        let mut connection = Connection::open(repo.path()).unwrap();
        let tx = connection.transaction().unwrap();
        let count = if long { 2600 } else { MAX_ARTWORK_INFO_ROWS };
        for index in 0..count {
            insert_info(&tx, &quota_info(index, long));
        }
        tx.commit().unwrap();
        let mut newest = quota_info(count, long);
        newest.resolved_at_ms = 2;
        newest.expire_at_ms += 1;
        repo.put_artwork(&newest, None).unwrap();
        let stats = repo.image_cache_stats().unwrap();
        assert!(stats.info_entries <= MAX_ARTWORK_INFO_ROWS);
        assert!(stats.info_bytes <= MAX_ARTWORK_INFO_BYTES as u64);
        if long {
            assert!(stats.info_entries < count);
        } else {
            assert_eq!(stats.info_entries, MAX_ARTWORK_INFO_ROWS);
        }
        assert_eq!(
            repo.artwork_info(&newest.key, 3).unwrap(),
            ArtworkLookup::Hit(Box::new(newest))
        );
        assert!(repo.artwork_info_page(0, 101).is_err());
        assert!(
            repo.artwork_info_page(MAX_ARTWORK_INFO_ROWS + 1, 1)
                .is_err()
        );
        assert!(repo.artwork_info_page(0, 100).unwrap().len() <= 100);
        assert_eq!(repo.revision().unwrap(), 0);
    }
}

#[test]
fn blob_row_quota_evicts_only_bytes_and_preserves_remote_url_information() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let mut oldest = None;
    for index in 0..=MAX_ARTWORK_BLOB_ROWS {
        let blob = verified(index as u32);
        let info = remote(
            &format!("blob-{index}"),
            "https://cover.invalid/p.png",
            Some(&blob),
            1000 + index as u64,
        );
        if index == 0 {
            oldest = Some((info.clone(), blob.metadata().sha256.clone()));
        }
        repo.put_artwork(&info, Some(&blob)).unwrap();
    }
    let stats = repo.image_cache_stats().unwrap();
    assert_eq!(stats.blob_entries, MAX_ARTWORK_BLOB_ROWS);
    assert_eq!(stats.info_entries, MAX_ARTWORK_BLOB_ROWS + 1);
    assert!(stats.blob_bytes <= MAX_ARTWORK_BLOB_BYTES as u64);
    let (info, sha) = oldest.unwrap();
    assert!(repo.stored_artwork_blob(&sha, 3000).unwrap().is_none());
    let ArtworkLookup::Hit(retained) = repo.artwork_info(&info.key, 3000).unwrap() else {
        panic!()
    };
    assert!(retained.url.is_some());
    assert!(retained.blob.is_none());
    assert_eq!(repo.revision().unwrap(), 0);
}

fn crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb88320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
fn large_verified(index: u32) -> VerifiedArtwork {
    let mut encoded = png(index);
    let at = encoded.len() - 12;
    let mut data = format!("cache-fixture-{index}\0").into_bytes();
    data.resize(6 * 1024 * 1024, b'x');
    let mut chunk = Vec::new();
    chunk.extend((data.len() as u32).to_be_bytes());
    chunk.extend(b"tEXt");
    chunk.extend(&data);
    let checksum = crc(&chunk[4..]);
    chunk.extend(checksum.to_be_bytes());
    encoded.splice(at..at, chunk);
    validate_artwork(encoded, Some("image/png"), || Ok(())).unwrap()
}
#[test]
fn encoded_byte_quota_is_enforced_with_full_valid_images_instead_of_truncating_artwork() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let mut oldest = None;
    for index in 0..11 {
        let blob = large_verified(index);
        assert!(blob.encoded().len() > 6 * 1024 * 1024);
        let info = remote(
            &format!("large-{index}"),
            "https://cover.invalid/large.png",
            Some(&blob),
            1000 + index as u64,
        );
        if index == 0 {
            oldest = Some((info.key.clone(), blob.metadata().sha256.clone()));
        }
        repo.put_artwork(&info, Some(&blob)).unwrap();
    }
    let stats = repo.image_cache_stats().unwrap();
    assert_eq!(stats.info_entries, 11);
    assert_eq!(stats.blob_entries, 10);
    assert!(stats.blob_bytes <= MAX_ARTWORK_BLOB_BYTES as u64);
    let (key, sha) = oldest.unwrap();
    assert!(repo.stored_artwork_blob(&sha, 2000).unwrap().is_none());
    let ArtworkLookup::Hit(info) = repo.artwork_info(&key, 2000).unwrap() else {
        panic!()
    };
    assert!(info.blob.is_none() && info.url.is_some());
    assert_eq!(repo.revision().unwrap(), 0);
}

fn schema_six(profile: &tempfile::TempDir, collision: bool) {
    let connection = Connection::open(profile.path().join(DATABASE_NAME)).unwrap();
    connection
        .execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_scan_runs; DROP TABLE library_membership; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; DROP TABLE artwork_info; DROP TABLE artwork_blobs;")
        .unwrap();
    if collision {
        connection.execute_batch("CREATE TABLE artwork_info(sentinel TEXT); INSERT INTO artwork_info VALUES('preserve');").unwrap();
    }
    connection.pragma_update(None, "user_version", 6).unwrap();
}
#[test]
fn schema_six_migration_creates_a_valid_backup_and_preserves_user_and_media_cache_data() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let original = track("migration");
    repo.upsert_track(&original).unwrap();
    let media = lx_gpui_player::domain::media::ResolvedMedia::new(
        lx_gpui_player::domain::media::ResolvedMediaKey::new(
            &original,
            "128k",
            &"a".repeat(64),
            1,
            &original.to_lx_dto(LxMusicDtoVersion::Legacy),
        )
        .unwrap(),
        lx_gpui_player::domain::media::CachedMediaUrl::new("https://media.invalid/keep.wav")
            .unwrap(),
        None,
        1000,
        1000,
    )
    .unwrap();
    repo.put_resolved_media(&media).unwrap();
    let revision = repo.revision().unwrap();
    let changes = repo.changes_since(0, 100).unwrap();
    let path = repo.path().to_owned();
    drop(repo);
    schema_six(&profile, false);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        Connection::open(repo.path())
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(repo.track(&original.id).unwrap(), Some(original));
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.changes_since(0, 100).unwrap(), changes);
    assert_eq!(
        repo.resolved_media(&media.key, 1001).unwrap(),
        lx_gpui_player::domain::media::MediaCacheLookup::Hit(media)
    );
    repo.integrity_check().unwrap();
    let backups: Vec<_> = fs::read_dir(profile.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "bak"))
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = Connection::open(&backups[0]).unwrap();
    assert_eq!(
        backup
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        backup
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let current = Connection::open(path).unwrap();
    assert_eq!(current.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('artwork_info_lru','artwork_info_track','artwork_blobs_lru')",[],|row|row.get::<_,u32>(0)).unwrap(),3);
}
#[test]
fn schema_seven_collision_rolls_back_blob_table_creation_and_keeps_the_schema_six_backup() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let path = repo.path().to_owned();
    drop(repo);
    schema_six(&profile, true);
    assert!(Repository::open(profile.path()).is_err());
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        connection
            .query_row("SELECT sentinel FROM artwork_info", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "preserve"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='artwork_blobs'",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}
