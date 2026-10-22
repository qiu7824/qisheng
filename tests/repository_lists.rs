use lx_gpui_player::{
    domain::{
        lists::*,
        lyric::{LyricBundle, LyricField, LyricOrigin},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::{ChangeKind, DATABASE_NAME, Repository, SCHEMA_VERSION},
};
use rusqlite::Connection;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

fn profile() -> TempDir {
    let base = std::env::temp_dir().join("lx-native-list-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("repository-")
        .tempdir_in(base)
        .unwrap()
}
fn track(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"id":format!("kw_{id}"),"name":format!("Song {id}"),"singer":"Artist","source":"kw","interval":"00:30","meta":{"songId":id,"albumName":"Album","qualitys":[{"type":"128k"}],"extra":{"opaque":true}}}),LxMusicDtoVersion::Modern).unwrap()
}
fn ids(tracks: Vec<TrackRecord>) -> Vec<String> {
    tracks.into_iter().map(|t| t.lx_id).collect()
}
fn apply(repo: &Repository, action: ListAction) -> u64 {
    let rev = repo.revision().unwrap();
    let change = repo.apply_list_action(&action, Some(rev)).unwrap();
    assert_eq!(change.revision, rev + 1);
    assert_eq!(change.kind, ChangeKind::ListsChanged);
    change.revision
}
fn user_infos(repo: &Repository) -> Vec<ListInfo> {
    repo.list_infos()
        .unwrap()
        .into_iter()
        .filter(|i| !i.is_builtin())
        .collect()
}

#[test]
fn all_twelve_actions_persist_with_upstream_order_and_membership_rules() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(
        repo.list_infos()
            .unwrap()
            .iter()
            .map(|l| l.id.as_str())
            .collect::<Vec<_>>(),
        ["default", "love", "temp"]
    );
    let (a, b, c) = (
        ListInfo::user("a", "A"),
        ListInfo::user("b", "B"),
        ListInfo::user("c", "C"),
    );
    let (t1, t2, t3, t4) = (track("1"), track("2"), track("3"), track("4"));
    apply(
        &repo,
        ListAction::Create {
            position: 0,
            lists: vec![a.clone(), b.clone(), c],
        },
    );
    apply(
        &repo,
        ListAction::MusicOverwrite {
            list_id: "default".into(),
            tracks: vec![t1.clone(), t2.clone()],
        },
    );
    apply(
        &repo,
        ListAction::MusicAdd {
            list_id: "default".into(),
            tracks: vec![t3.clone(), t2.clone(), t3.clone()],
            location: AddLocation::Top,
        },
    );
    assert_eq!(
        ids(repo.list_tracks("default").unwrap()),
        ["kw_3", "kw_1", "kw_2"]
    );
    apply(
        &repo,
        ListAction::MusicAdd {
            list_id: "love".into(),
            tracks: vec![t1.clone()],
            location: AddLocation::Bottom,
        },
    );
    apply(
        &repo,
        ListAction::MusicMove {
            from_id: "default".into(),
            to_id: "love".into(),
            tracks: vec![t1.clone(), t3.clone()],
            location: AddLocation::Top,
        },
    );
    assert_eq!(ids(repo.list_tracks("default").unwrap()), ["kw_2"]);
    assert_eq!(ids(repo.list_tracks("love").unwrap()), ["kw_3", "kw_1"]);
    let mut updated = t3.clone();
    updated.title = "Updated song".into();
    apply(
        &repo,
        ListAction::MusicUpdate {
            updates: vec![ListMusicUpdate {
                list_id: "love".into(),
                track: updated.clone(),
            }],
        },
    );
    assert_eq!(repo.list_tracks("love").unwrap()[0].title, "Updated song");
    apply(
        &repo,
        ListAction::MusicUpdatePosition {
            list_id: "love".into(),
            ids: vec![t1.id.clone(), t3.lx_id.clone()],
            position: 0,
        },
    );
    assert_eq!(ids(repo.list_tracks("love").unwrap()), ["kw_1", "kw_3"]);
    apply(
        &repo,
        ListAction::MusicRemove {
            list_id: "love".into(),
            ids: vec!["kw_1".into()],
        },
    );
    assert!(!repo.list_contains("love", &t1.id).unwrap());
    assert_eq!(repo.music_list_ids(&t3.id).unwrap(), ["love"]);
    let mut renamed = b.clone();
    renamed.name = "Renamed".into();
    renamed.source = Some("wy".into());
    renamed.source_list_id = Some("19723756".into());
    apply(
        &repo,
        ListAction::Update {
            lists: vec![renamed.clone()],
        },
    );
    apply(
        &repo,
        ListAction::UpdatePosition {
            ids: vec!["a".into(), "b".into()],
            position: 1,
        },
    );
    assert_eq!(
        user_infos(&repo)
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["c", "b", "a"]
    );
    assert!(user_infos(&repo)[1].location_update_time.unwrap() > 0);
    apply(
        &repo,
        ListAction::Remove {
            ids: vec!["c".into()],
        },
    );
    apply(
        &repo,
        ListAction::MusicClear {
            list_ids: vec!["love".into()],
        },
    );
    apply(
        &repo,
        ListAction::MusicOverwrite {
            list_id: "temp".into(),
            tracks: vec![t4.clone()],
        },
    );
    let mut temp = ListInfo::user("temp", "ignored builtin rename");
    temp.meta = Some(json!({"id":"remote-playlist"}));
    apply(&repo, ListAction::Update { lists: vec![temp] });
    apply(
        &repo,
        ListAction::DataOverwrite {
            data: ListData {
                default_list: vec![t2.clone()],
                love_list: vec![],
                user_list: vec![UserList {
                    info: a,
                    tracks: vec![updated.clone()],
                }],
                temp_list: None,
            },
        },
    );
    assert_eq!(repo.list_tracks("temp").unwrap(), vec![t4]);
    assert_eq!(
        repo.list_infos()
            .unwrap()
            .iter()
            .find(|i| i.id == "temp")
            .unwrap()
            .meta,
        Some(json!({"id":"remote-playlist"}))
    );
    let snapshot = repo.list_data().unwrap();
    let revision = repo.revision().unwrap();
    let outbox = repo.changes_since(0, 100).unwrap();
    assert_eq!(outbox.len() as u64, revision);
    repo.integrity_check().unwrap();
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(repo.list_data().unwrap(), snapshot);
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.changes_since(0, 100).unwrap(), outbox);
}

#[test]
fn cross_list_destination_failure_rolls_back_source_target_tracks_revision_and_outbox() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = track("atomic");
    apply(
        &repo,
        ListAction::Create {
            position: -1,
            lists: vec![ListInfo::user("dest", "Destination")],
        },
    );
    apply(
        &repo,
        ListAction::MusicAdd {
            list_id: "default".into(),
            tracks: vec![t.clone()],
            location: AddLocation::Bottom,
        },
    );
    let external = Connection::open(repo.path()).unwrap();
    external.execute_batch("CREATE TRIGGER block_destination BEFORE INSERT ON music_list_items WHEN NEW.list_id='dest' BEGIN SELECT RAISE(ABORT,'destination failure'); END;").unwrap();
    let before = repo.list_data().unwrap();
    let revision = repo.revision().unwrap();
    let outbox = repo.changes_since(0, 100).unwrap();
    assert!(
        repo.apply_list_action(
            &ListAction::MusicMove {
                from_id: "default".into(),
                to_id: "dest".into(),
                tracks: vec![t.clone()],
                location: AddLocation::Top
            },
            Some(revision)
        )
        .is_err()
    );
    assert_eq!(repo.list_data().unwrap(), before);
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.changes_since(0, 100).unwrap(), outbox);
    assert_eq!(repo.track(&t.id).unwrap(), Some(t));
    external
        .execute_batch("DROP TRIGGER block_destination")
        .unwrap();
    drop(external);
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(repo.list_data().unwrap(), before);
}

#[test]
fn conflicts_invalid_second_list_and_duplicate_overwrites_publish_nothing() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    apply(
        &repo,
        ListAction::Create {
            position: 0,
            lists: vec![ListInfo::user("existing", "Existing")],
        },
    );
    let rev = repo.revision().unwrap();
    assert!(
        repo.apply_list_action(
            &ListAction::Create {
                position: 0,
                lists: vec![
                    ListInfo::user("new", "New"),
                    ListInfo::user("existing", "Duplicate")
                ]
            },
            Some(rev)
        )
        .is_err()
    );
    assert_eq!(user_infos(&repo).len(), 1);
    assert!(
        repo.apply_list_action(
            &ListAction::MusicAdd {
                list_id: "love".into(),
                tracks: vec![track("x")],
                location: AddLocation::Bottom
            },
            Some(0)
        )
        .is_err()
    );
    assert!(repo.list_tracks("love").unwrap().is_empty());
    assert!(
        repo.apply_list_action(
            &ListAction::Remove {
                ids: vec!["existing".into(), "default".into()]
            },
            Some(rev)
        )
        .is_err()
    );
    assert_eq!(user_infos(&repo).len(), 1);
    assert!(
        repo.apply_list_action(
            &ListAction::DataOverwrite {
                data: ListData {
                    user_list: vec![
                        UserList {
                            info: ListInfo::user("same", "1"),
                            tracks: vec![]
                        },
                        UserList {
                            info: ListInfo::user("same", "2"),
                            tracks: vec![]
                        }
                    ],
                    ..Default::default()
                }
            },
            Some(rev)
        )
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), rev);
    let duplicate = track("duplicate");
    assert!(
        repo.apply_list_action(
            &ListAction::MusicOverwrite {
                list_id: "love".into(),
                tracks: vec![duplicate.clone(), duplicate.clone()]
            },
            Some(rev)
        )
        .is_err()
    );
    assert!(repo.list_tracks("love").unwrap().is_empty());
    assert!(repo.track(&duplicate.id).unwrap().is_none());
    assert_eq!(repo.revision().unwrap(), rev);
    assert_eq!(repo.changes_since(0, 100).unwrap().len(), 1);
}

#[test]
fn scroll_source_auto_update_time_and_temp_reference_survive_replacement_and_restart() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let mut info = ListInfo::user("remote", "Remote");
    info.source = Some("mg".into());
    info.source_list_id = Some("221573417".into());
    info.location_update_time = Some(123);
    apply(
        &repo,
        ListAction::Create {
            position: -1,
            lists: vec![info.clone()],
        },
    );
    repo.set_list_position("remote", 125.25, None).unwrap();
    let update = ListUpdateInfo {
        update_time: 7654321,
        is_auto_update: true,
    };
    repo.set_list_update_info("remote", &update, None).unwrap();
    let mut catalog_track = track("r");
    catalog_track.title = "Current catalog metadata".into();
    repo.upsert_track(&catalog_track).unwrap();
    apply(
        &repo,
        ListAction::MusicOverwrite {
            list_id: "remote".into(),
            tracks: vec![track("r")],
        },
    );
    assert_eq!(repo.track(&catalog_track.id).unwrap(), Some(catalog_track));
    assert_eq!(repo.list_update_infos().unwrap()["remote"], update);
    apply(
        &repo,
        ListAction::DataOverwrite {
            data: ListData {
                user_list: vec![UserList {
                    info: info.clone(),
                    tracks: vec![track("r2")],
                }],
                ..Default::default()
            },
        },
    );
    assert_eq!(repo.list_positions().unwrap()["remote"], 125.25);
    assert_eq!(repo.list_update_infos().unwrap()["remote"], update);
    let rev = repo.revision().unwrap();
    assert!(
        repo.set_list_position("remote", f64::NAN, Some(rev))
            .is_err()
    );
    assert!(
        repo.set_list_update_info("missing", &update, Some(rev))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), rev);
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(repo.list_positions().unwrap()["remote"], 125.25);
    assert_eq!(repo.list_update_infos().unwrap()["remote"], update);
    assert_eq!(user_infos(&repo), vec![info]);
    apply(
        &repo,
        ListAction::Remove {
            ids: vec!["remote".into()],
        },
    );
    assert!(!repo.list_positions().unwrap().contains_key("remote"));
    assert!(!repo.list_update_infos().unwrap().contains_key("remote"));
}

#[test]
fn dislike_text_semantics_aliases_unicode_and_add_clear_are_durable() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    repo.apply_dislike_action(&DislikeAction::Overwrite{rules:" SONG \n@ ARTIST \n Song @ Artist \nSong@Artist\nA#B@Singer\nÉCHO\nfoo@bar@ignored\n  ".into()},Some(0)).unwrap();
    let rules = repo.dislike_rules().unwrap();
    assert_eq!(
        rules.rules,
        "song\n@artist\nsong@artist\na#b@singer\nécho\nfoo@bar"
    );
    assert!(rules.matches(" Song ", "other"));
    assert!(rules.matches("other", " Artist "));
    assert!(rules.matches("A@B", "Singer"));
    assert!(rules.matches("ÉCHO", "else"));
    assert!(!rules.matches("other", "else"));
    repo.apply_dislike_action(
        &DislikeAction::Add {
            infos: vec![DislikeMusicInfo {
                name: "New Song".into(),
                singer: "New Singer".into(),
            }],
        },
        None,
    )
    .unwrap();
    let rule = repo.dislike_rules().unwrap();
    assert!(rule.matches("new song", "new singer"));
    let rev = repo.revision().unwrap();
    assert!(
        repo.apply_dislike_action(&DislikeAction::Clear, Some(0))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), rev);
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(repo.dislike_rules().unwrap(), rule);
    repo.apply_dislike_action(&DislikeAction::Clear, None)
        .unwrap();
    assert_eq!(repo.dislike_rules().unwrap(), DislikeRules::default());
}

#[test]
fn history_keeps_fifteen_mru_entries_exact_case_and_noop_revision() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    for index in 0..18 {
        repo.record_search(&format!("word{index}"), None).unwrap();
    }
    let h = repo.search_history().unwrap();
    assert_eq!(h.len(), 15);
    assert_eq!(h.first().unwrap(), "word17");
    assert_eq!(h.last().unwrap(), "word3");
    let rev = repo.revision().unwrap();
    assert!(repo.record_search("word17", Some(rev)).unwrap().is_none());
    assert_eq!(repo.revision().unwrap(), rev);
    assert!(repo.record_search("word17", Some(0)).is_err());
    repo.record_search("word5", None).unwrap();
    repo.record_search("WORD5", None).unwrap();
    assert_eq!(&repo.search_history().unwrap()[..2], ["WORD5", "word5"]);
    repo.remove_search_history("word5", None).unwrap();
    let expected = repo.search_history().unwrap();
    drop(repo);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(repo.search_history().unwrap(), expected);
    repo.clear_search_history(None).unwrap();
    assert!(repo.search_history().unwrap().is_empty());
}

fn schema_three_profile() -> (TempDir, TrackRecord) {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let t = track("migrate");
    repo.upsert_track(&t).unwrap();
    repo.replace_lyrics(
        &t.id,
        LyricOrigin::Edited,
        &LyricBundle {
            lyric: LyricField::Text("[00:01.000]kept".into()),
            ..Default::default()
        },
    )
    .unwrap();
    repo.record_profile_change(&"a".repeat(64)).unwrap();
    let path = repo.path().to_path_buf();
    drop(repo);
    let connection = Connection::open(path).unwrap();
    connection.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_scan_runs; DROP TABLE library_membership; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; DROP TABLE artwork_info; DROP TABLE artwork_blobs; DROP TABLE resolved_media; DROP TABLE source_matches; DROP TABLE playback_state; DROP TABLE music_list_items; DROP TABLE music_lists; DROP TABLE dislike_state; DROP TABLE search_history; PRAGMA user_version=3;").unwrap();
    drop(connection);
    (p, t)
}
#[test]
fn schema_three_to_four_preserves_old_track_lyrics_profile_and_backup() {
    let (p, t) = schema_three_profile();
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(
        rusqlite::Connection::open(repo.path())
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(repo.track(&t.id).unwrap(), Some(t.clone()));
    assert_eq!(
        repo.lyrics(&t.id, LyricOrigin::Edited)
            .unwrap()
            .unwrap()
            .lyric
            .text(),
        Some("[00:01.000]kept")
    );
    assert_eq!(repo.revision().unwrap(), 3);
    assert_eq!(
        repo.record_profile_change(&"a".repeat(64))
            .unwrap()
            .revision,
        3
    );
    assert_eq!(repo.list_infos().unwrap().len(), 3);
    repo.integrity_check().unwrap();
    drop(repo);
    let backups: Vec<_> = fs::read_dir(p.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "bak"))
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = Connection::open(&backups[0]).unwrap();
    assert_eq!(
        backup
            .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        backup
            .query_row("SELECT count(*) FROM tracks", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(backup);
    let repo = Repository::open(p.path()).unwrap();
    assert_eq!(repo.revision().unwrap(), 3);
    assert_eq!(
        fs::read_dir(p.path())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "bak"))
            .count(),
        1
    );
}
#[test]
fn schema_four_migration_failure_rolls_back_ddl_and_keeps_original_and_backup() {
    let (p, t) = schema_three_profile();
    let path = p.path().join(DATABASE_NAME);
    let c = Connection::open(&path).unwrap();
    c.execute_batch("CREATE TABLE music_list_items(sentinel TEXT); INSERT INTO music_list_items VALUES('preserve');").unwrap();
    drop(c);
    assert!(Repository::open(p.path()).is_err());
    let c = Connection::open(&path).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='music_lists'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        c.query_row("SELECT sentinel FROM music_list_items", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "preserve"
    );
    assert_eq!(
        c.query_row("SELECT id FROM tracks", [], |r| r.get::<_, String>(0))
            .unwrap(),
        t.id
    );
    assert!(
        fs::read_dir(p.path()).unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|e| e == "bak"))
    );
}
#[test]
fn unknown_schema_blocks_all_new_writes_and_is_not_overwritten_on_reopen() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    let path = repo.path().to_path_buf();
    let c = Connection::open(&path).unwrap();
    c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(c);
    assert!(
        repo.apply_list_action(
            &ListAction::MusicClear {
                list_ids: vec!["default".into()]
            },
            None
        )
        .is_err()
    );
    assert!(
        repo.apply_dislike_action(&DislikeAction::Clear, None)
            .is_err()
    );
    assert!(repo.record_search("word", None).is_err());
    drop(repo);
    let bytes = fs::read(&path).unwrap();
    assert!(Repository::open(p.path()).is_err());
    assert_eq!(fs::read(path).unwrap(), bytes);
}
