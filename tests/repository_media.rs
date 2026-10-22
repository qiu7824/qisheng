use lx_gpui_player::{
    domain::{
        media::*,
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::{
        DATABASE_NAME, MAX_MEDIA_CACHE_BYTES, MAX_MEDIA_CACHE_ROWS, Repository, SCHEMA_VERSION,
    },
};
use rusqlite::{Connection, params};
use serde_json::json;
use std::fs;

fn profile() -> tempfile::TempDir {
    let directory = std::env::temp_dir().join("lx-native-media-cache-tests");
    fs::create_dir_all(&directory).unwrap();
    tempfile::Builder::new()
        .prefix("media-")
        .tempdir_in(directory)
        .unwrap()
}
fn track(platform: &str, song: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"id":format!("{platform}_{song}"),"name":"Original Song","singer":"Artist","source":platform,"interval":"03:05","meta":{"songId":song,"albumName":"Album","hash":"exact-hash","qualitys":[{"type":"128k"},{"type":"flac"}],"opaque":{"keep":true}}}),LxMusicDtoVersion::Modern).unwrap()
}
fn resolved(original: &TrackRecord, actual: Option<TrackRecord>) -> ResolvedMedia {
    let dto = original.to_lx_dto(LxMusicDtoVersion::Legacy);
    let key = ResolvedMediaKey::new(original, "flac", &"a".repeat(64), 1, &dto).unwrap();
    ResolvedMedia::new(
        key,
        CachedMediaUrl::new("https://media.invalid/audio?signature=private-value").unwrap(),
        actual,
        1000,
        1000,
    )
    .unwrap()
}
fn matches(original: &TrackRecord) -> SourceMatches {
    SourceMatches::new(
        SourceMatchKey::new(original).unwrap(),
        vec![
            MatchCandidate {
                track: track("wy", "actual-first"),
                score: 10000,
                reasons: vec![MatchReason::ExactTitle, MatchReason::ExactArtists],
                actual_quality: None,
            },
            MatchCandidate {
                track: track("tx", "actual-second"),
                score: 9000,
                reasons: vec![MatchReason::ExactTitle, MatchReason::CompatibleArtists],
                actual_quality: Some("flac".into()),
            },
        ],
        1000,
        1000,
    )
    .unwrap()
}

#[test]
fn persistent_hits_preserve_actual_identity_and_do_not_mutate_tracks_lists_revision_or_outbox() {
    let profile = profile();
    let original = track("kg", "original");
    let actual = track("wy", "matched");
    let expected = resolved(&original, Some(actual.clone()));
    let expected_matches = matches(&original);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_track(&original).unwrap();
    let revision = repo.revision().unwrap();
    let outbox = repo.changes_since(0, 100).unwrap();
    let lists = repo.list_data().unwrap();
    repo.put_resolved_media(&expected).unwrap();
    repo.put_source_matches(&expected_matches).unwrap();
    assert_eq!(
        repo.resolved_media(&expected.key, 1001).unwrap(),
        MediaCacheLookup::Hit(expected.clone())
    );
    assert_eq!(
        repo.source_matches(&expected_matches.key, 1001).unwrap(),
        MediaCacheLookup::Hit(expected_matches.clone())
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.changes_since(0, 100).unwrap(), outbox);
    assert_eq!(repo.list_data().unwrap(), lists);
    assert_eq!(repo.track(&original.id).unwrap(), Some(original.clone()));
    assert!(repo.track(&actual.id).unwrap().is_none());
    assert!(!format!("{expected:?}").contains("private-value"));
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repo.resolved_media(&expected.key, 1002).unwrap(),
        MediaCacheLookup::Hit(expected)
    );
    assert_eq!(
        repo.source_matches(&expected_matches.key, 1002).unwrap(),
        MediaCacheLookup::Hit(expected_matches)
    );
    assert_eq!(repo.revision().unwrap(), revision);
    repo.integrity_check().unwrap();
}

#[test]
fn every_resolver_identity_dimension_and_the_actual_dto_hash_fence_reuse() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let original = track("kg", "original");
    let value = resolved(&original, None);
    repo.put_resolved_media(&value).unwrap();
    let mut variants = Vec::new();
    let mut key = value.key.clone();
    key.original_track_id.push('x');
    variants.push(key);
    let mut key = value.key.clone();
    key.original_platform = "wy".into();
    variants.push(key);
    let mut key = value.key.clone();
    key.actual_quality = "128k".into();
    variants.push(key);
    let mut key = value.key.clone();
    key.source_sha256 = "b".repeat(64);
    variants.push(key);
    let mut key = value.key.clone();
    key.source_policy_version = 2;
    variants.push(key);
    let mut dto = original.to_lx_dto(LxMusicDtoVersion::Legacy);
    dto["hash"] = "changed-platform-hash".into();
    let key = ResolvedMediaKey::new(&original, "flac", &"a".repeat(64), 1, &dto).unwrap();
    assert_ne!(key.music_info_sha256, value.key.music_info_sha256);
    variants.push(key);
    for key in variants {
        assert_eq!(
            repo.resolved_media(&key, 1001).unwrap(),
            MediaCacheLookup::Miss
        );
    }
    let first = json!({"b":{"d":4,"c":3},"a":1});
    let second = json!({"a":1,"b":{"c":3,"d":4}});
    assert_eq!(
        music_info_fingerprint(&first).unwrap(),
        music_info_fingerprint(&second).unwrap()
    );
    let mut changed = original.clone();
    changed
        .metadata
        .insert("hash".into(), "metadata-revision".into());
    assert_ne!(
        track_record_fingerprint(&original).unwrap(),
        track_record_fingerprint(&changed).unwrap()
    );
}

#[test]
fn expiry_clock_rollback_and_explicit_failure_are_persisted_without_resurrecting_hits() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let value = resolved(&track("kg", "original"), None);
    repo.put_resolved_media(&value).unwrap();
    assert!(matches!(
        repo.resolved_media(&value.key, 1500).unwrap(),
        MediaCacheLookup::Hit(_)
    ));
    assert_eq!(
        repo.resolved_media(&value.key, 1499).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::ClockRollback)
    );
    assert_eq!(
        repo.resolved_media(&value.key, 1800).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::ClockRollback)
    );
    repo.put_resolved_media(&value).unwrap();
    assert_eq!(
        repo.resolved_media(&value.key, 2000).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::Expired)
    );
    repo.put_resolved_media(&value).unwrap();
    assert!(
        repo.record_resolved_media_failure(&value.key, MediaFailureKind::HttpRejected)
            .unwrap()
    );
    assert_eq!(
        repo.resolved_media(&value.key, 1001).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::HttpRejected)
    );
    let doc: String = Connection::open(repo.path())
        .unwrap()
        .query_row("SELECT document FROM resolved_media", [], |row| row.get(0))
        .unwrap();
    let stored: ResolvedMedia = serde_json::from_str(&doc).unwrap();
    assert_eq!(stored.failure, Some(MediaFailureKind::HttpRejected));
    let match_value = matches(&track("kg", "original"));
    repo.put_source_matches(&match_value).unwrap();
    assert_eq!(
        repo.source_matches(&match_value.key, 999).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::ClockRollback)
    );
    repo.put_source_matches(&match_value).unwrap();
    assert_eq!(
        repo.source_matches(&match_value.key, 2000).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::Expired)
    );
    assert_eq!(repo.revision().unwrap(), 0);
    assert!(repo.changes_since(0, 100).unwrap().is_empty());
}

#[test]
fn source_policy_and_metadata_invalidations_are_scoped_and_candidate_order_survives_restart() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let original = track("kg", "original");
    let first = resolved(&original, None);
    let mut second = first.clone();
    second.key.source_policy_version = 2;
    let other = resolved(&track("kg", "other"), None);
    let matched = matches(&original);
    for value in [&first, &second, &other] {
        repo.put_resolved_media(value).unwrap();
    }
    repo.put_source_matches(&matched).unwrap();
    assert_eq!(
        repo.invalidate_media_for_source_policy(
            &"a".repeat(64),
            1,
            MediaInvalidationReason::SourceChanged
        )
        .unwrap(),
        2
    );
    assert_eq!(
        repo.resolved_media(&first.key, 1001).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::SourceChanged)
    );
    assert!(matches!(
        repo.resolved_media(&second.key, 1001).unwrap(),
        MediaCacheLookup::Hit(_)
    ));
    assert!(matches!(
        repo.source_matches(&matched.key, 1001).unwrap(),
        MediaCacheLookup::Hit(_)
    ));
    assert_eq!(
        repo.invalidate_media_for_track(&original.id, MediaInvalidationReason::MetadataChanged)
            .unwrap(),
        3
    );
    assert_eq!(
        repo.source_matches(&matched.key, 1001).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::MetadataChanged)
    );
    let removed = repo.clear_media_cache().unwrap();
    assert_eq!(removed.resolved_entries, 3);
    assert_eq!(removed.match_entries, 1);
    assert_eq!(repo.media_cache_stats().unwrap().resolved_entries, 0);
    assert_eq!(repo.revision().unwrap(), 0);
}

#[test]
fn malformed_urls_unbounded_times_versions_counts_scores_and_partial_writes_are_rejected() {
    for url in [
        "file:///C:/formal.wav",
        "ftp://media.invalid/a",
        "https://user:pass@media.invalid/a",
        "https://@media.invalid/a",
        "https://media.invalid/a\n",
    ] {
        assert!(CachedMediaUrl::new(url).is_err(), "{url}");
    }
    let prefix = "https://media.invalid/";
    assert!(CachedMediaUrl::new(format!("{prefix}{}", "a".repeat(2048 - prefix.len()))).is_ok());
    assert!(CachedMediaUrl::new(format!("{prefix}{}", "a".repeat(2049 - prefix.len()))).is_err());
    assert!(
        serde_json::from_str::<CachedMediaUrl>("\"https://user@media.invalid/private\"").is_err()
    );
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let original = track("kg", "original");
    let valid = resolved(&original, None);
    let mut invalid = valid.clone();
    invalid.expire_at_ms = invalid.resolved_at_ms;
    assert!(repo.put_resolved_media(&invalid).is_err());
    invalid = valid.clone();
    invalid.key.source_policy_version = 0;
    assert!(repo.put_resolved_media(&invalid).is_err());
    invalid = valid.clone();
    invalid.expire_at_ms = invalid.resolved_at_ms + MAX_RESOLVED_MEDIA_TTL_MS + 1;
    assert!(repo.put_resolved_media(&invalid).is_err());
    let valid_matches = matches(&original);
    let mut invalid_matches = valid_matches.clone();
    invalid_matches.key.algorithm_version += 1;
    assert!(repo.put_source_matches(&invalid_matches).is_err());
    invalid_matches = valid_matches.clone();
    invalid_matches.candidates[0].score = 10001;
    assert!(repo.put_source_matches(&invalid_matches).is_err());
    invalid_matches = valid_matches.clone();
    invalid_matches.candidates.reverse();
    assert!(repo.put_source_matches(&invalid_matches).is_err());
    invalid_matches = valid_matches.clone();
    invalid_matches.candidates = vec![valid_matches.candidates[0].clone(); 101];
    assert!(repo.put_source_matches(&invalid_matches).is_err());
    invalid_matches = valid_matches.clone();
    invalid_matches.candidates[0]
        .track
        .metadata
        .insert("oversize".into(), "x".repeat(MAX_MEDIA_CACHE_BYTES).into());
    assert!(repo.put_source_matches(&invalid_matches).is_err());
    assert_eq!(repo.media_cache_stats().unwrap().resolved_entries, 0);
    assert_eq!(repo.media_cache_stats().unwrap().match_entries, 0);
    assert_eq!(repo.revision().unwrap(), 0);
    let connection = Connection::open(repo.path()).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_media BEFORE INSERT ON resolved_media BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(repo.put_resolved_media(&valid).is_err());
    assert_eq!(repo.media_cache_stats().unwrap().resolved_entries, 0);
    connection
        .execute_batch("DROP TRIGGER reject_media;")
        .unwrap();
    repo.put_resolved_media(&valid).unwrap();
    assert!(matches!(
        repo.resolved_media(&valid.key, 1001).unwrap(),
        MediaCacheLookup::Hit(_)
    ));
}

#[test]
fn cache_byte_quotas_evict_the_oldest_entry_and_unknown_persisted_algorithms_expire() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let mut first = matches(&track("kg", "first"));
    first.candidates[0]
        .track
        .metadata
        .insert("payload".into(), "x".repeat(3 * 1024 * 1024).into());
    let mut second = first.clone();
    second.key = SourceMatchKey::new(&track("kg", "second")).unwrap();
    second.matched_at_ms = 1001;
    second.expire_at_ms = 2001;
    let mut third = first.clone();
    third.key = SourceMatchKey::new(&track("kg", "third")).unwrap();
    third.matched_at_ms = 1002;
    third.expire_at_ms = 2002;
    repo.put_source_matches(&first).unwrap();
    repo.put_source_matches(&second).unwrap();
    assert!(matches!(
        repo.source_matches(&first.key, 1003).unwrap(),
        MediaCacheLookup::Hit(_)
    ));
    repo.put_source_matches(&third).unwrap();
    assert_eq!(
        repo.source_matches(&second.key, 1004).unwrap(),
        MediaCacheLookup::Miss
    );
    let stats = repo.media_cache_stats().unwrap();
    assert_eq!(stats.match_entries, 2);
    assert!(stats.match_bytes <= MAX_MEDIA_CACHE_BYTES as u64);
    let connection = Connection::open(repo.path()).unwrap();
    connection
        .execute(
            "UPDATE source_matches SET algorithm_version=99 WHERE cache_key=?1",
            [first.key.fingerprint().unwrap()],
        )
        .unwrap();
    assert_eq!(
        repo.source_matches(&first.key, 1004).unwrap(),
        MediaCacheLookup::Invalidated(MediaInvalidationReason::UnsupportedVersion)
    );
    assert_eq!(
        repo.source_matches(&first.key, 1005).unwrap(),
        MediaCacheLookup::Miss
    );
    assert_eq!(repo.revision().unwrap(), 0);
}

fn downgrade_to_five(profile: &tempfile::TempDir, collision: bool) {
    let path = profile.path().join(DATABASE_NAME);
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_scan_runs; DROP TABLE library_membership; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; DROP TABLE artwork_info; DROP TABLE artwork_blobs; DROP TABLE resolved_media; DROP TABLE source_matches;")
        .unwrap();
    if collision {
        connection.execute_batch("CREATE TABLE source_matches(sentinel TEXT); INSERT INTO source_matches VALUES('preserve');").unwrap();
    }
    connection.pragma_update(None, "user_version", 5).unwrap();
}
#[test]
fn schema_five_migration_preserves_user_data_and_a_valid_backup_and_adds_indexes() {
    let profile = profile();
    let original = track("kg", "original");
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_track(&original).unwrap();
    let revision = repo.revision().unwrap();
    let lists = repo.list_data().unwrap();
    let changes = repo.changes_since(0, 100).unwrap();
    drop(repo);
    downgrade_to_five(&profile, false);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        rusqlite::Connection::open(repo.path())
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(repo.track(&original.id).unwrap(), Some(original));
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.list_data().unwrap(), lists);
    assert_eq!(repo.changes_since(0, 100).unwrap(), changes);
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
        5
    );
    assert_eq!(
        backup
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let connection = Connection::open(repo.path()).unwrap();
    let indexes:i64=connection.query_row("SELECT count(*) FROM sqlite_schema WHERE type='index' AND name IN ('resolved_media_track','resolved_media_source','resolved_media_expiry','resolved_media_lru','source_matches_track','source_matches_expiry','source_matches_lru')",[],|row|row.get(0)).unwrap();
    assert_eq!(indexes, 7);
}
#[test]
fn a_schema_six_collision_rolls_back_all_migration_ddl_and_preserves_schema_five() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let path = repo.path().to_owned();
    drop(repo);
    downgrade_to_five(&profile, true);
    assert!(Repository::open(profile.path()).is_err());
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        5
    );
    assert_eq!(
        connection
            .query_row("SELECT sentinel FROM source_matches", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "preserve"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='resolved_media'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn the_row_quota_is_enforced_even_for_small_empty_match_sets() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let mut connection = Connection::open(repo.path()).unwrap();
    let tx = connection.transaction().unwrap();
    {
        let mut insert = tx.prepare("INSERT INTO source_matches(cache_key,original_track_id,original_platform,original_record_sha256,algorithm_version,matched_at_ms,expire_at_ms,last_access_ms,byte_size,document) VALUES(?1,?2,?3,?4,?5,?6,?7,?6,?8,?9)").unwrap();
        for index in 0..MAX_MEDIA_CACHE_ROWS {
            let key = SourceMatchKey {
                original_track_id: format!("fixture-{index}"),
                original_platform: "kg".into(),
                original_record_sha256: "c".repeat(64),
                algorithm_version: MATCHING_ALGORITHM_VERSION,
            };
            let value = SourceMatches::new(key, Vec::new(), 1, 1000).unwrap();
            let document = serde_json::to_string(&value).unwrap();
            insert
                .execute(params![
                    value.key.fingerprint().unwrap(),
                    value.key.original_track_id,
                    value.key.original_platform,
                    value.key.original_record_sha256,
                    value.key.algorithm_version,
                    value.matched_at_ms as i64,
                    value.expire_at_ms as i64,
                    document.len() as i64,
                    document
                ])
                .unwrap();
        }
    }
    tx.commit().unwrap();
    let new_value = SourceMatches::new(
        SourceMatchKey::new(&track("kg", "newest")).unwrap(),
        Vec::new(),
        2,
        1000,
    )
    .unwrap();
    repo.put_source_matches(&new_value).unwrap();
    assert_eq!(
        repo.media_cache_stats().unwrap().match_entries,
        MAX_MEDIA_CACHE_ROWS
    );
    assert!(repo.media_cache_stats().unwrap().match_bytes <= MAX_MEDIA_CACHE_BYTES as u64);
    assert_eq!(
        repo.source_matches(&new_value.key, 3).unwrap(),
        MediaCacheLookup::Hit(new_value)
    );
    assert_eq!(repo.revision().unwrap(), 0);
}

#[test]
fn a_failed_bulk_invalidation_rolls_back_every_cache_document() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let original = track("kg", "original");
    let first = resolved(&original, None);
    let mut second = first.clone();
    second.key.actual_quality = "128k".into();
    repo.put_resolved_media(&first).unwrap();
    repo.put_resolved_media(&second).unwrap();
    let connection = Connection::open(repo.path()).unwrap();
    let documents = || {
        let mut query = connection
            .prepare("SELECT document FROM resolved_media ORDER BY cache_key")
            .unwrap();
        query
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = documents();
    let last: String = connection
        .query_row(
            "SELECT cache_key FROM resolved_media ORDER BY cache_key DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection.execute_batch(&format!("CREATE TRIGGER reject_second BEFORE UPDATE ON resolved_media WHEN NEW.cache_key='{last}' BEGIN SELECT RAISE(ABORT,'fixture'); END;")).unwrap();
    assert!(
        repo.invalidate_media_for_track(&original.id, MediaInvalidationReason::MetadataChanged)
            .is_err()
    );
    assert_eq!(documents(), before);
    assert_eq!(repo.revision().unwrap(), 0);
    assert!(repo.changes_since(0, 100).unwrap().is_empty());
    connection
        .execute_batch("DROP TRIGGER reject_second;")
        .unwrap();
    assert_eq!(
        repo.invalidate_media_for_track(&original.id, MediaInvalidationReason::MetadataChanged)
            .unwrap(),
        2
    );
}
