use flate2::{Compression, write::GzEncoder};
use lx_gpui_player::{
    backup::{
        BackupEncoding, ConflictPolicy, LxBackupKind, MAX_BACKUP_BYTES, NativeBackup,
        NativeBackupOptions,
        csv::{CsvMapping, decode_csv, export_music_csv},
        decode_lx, decode_native, encode_lx, encode_native,
        lx::{ListImportDisposition, MAX_IMPORT_TRACKS},
        native::{NativeDownloadPlan, NativeEditedLyrics},
        settings::LxSettings,
    },
    domain::{
        lists::{ListData, ListInfo, UserList},
        lyric::{LyricBundle, LyricField},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    download::{planning::DownloadQuality, task_model::DownloadSettings},
    source::import_script,
    store::StoreData,
};
use serde_json::{Value, json};
use std::io::Write;

fn modern(id: &str) -> Value {
    json!({"id":id,"name":"星河,\"夜\"\n序曲","singer":"甲、乙","source":"kw","interval":"03:02","meta":{"songId":17,"albumName":"秋日","qualitys":[{"type":"128k"}],"_qualitys":{"128k":{}},"privateVendorField":{"unchanged":true}},"unmappedTop":[null,"保留"]})
}
fn list(id: &str, song: &str) -> Value {
    json!({"id":id,"name":format!("名单-{id}"),"source":"kw","sourceListId":"源名单","locationUpdateTime":null,"autoUpdateExtension":{"enabled":true},"list":[modern(song)]})
}
fn track(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(modern(id), LxMusicDtoVersion::Modern).unwrap()
}
fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut writer = GzEncoder::new(Vec::new(), Compression::default());
    writer.write_all(bytes).unwrap();
    writer.finish().unwrap()
}

#[test]
fn fixed_upstream_four_wire_shapes_roundtrip_json_gzip_and_unicode() {
    // Exact fields of pinned LX 2.12.6 SettingBackup.vue/config_files.d.ts.
    let fixtures = [
        (
            LxBackupKind::AllData,
            json!({"type":"allData_v2","setting":{"version":"2.1.0","player.volume":0.75,"futureVisual":{"accent":"紫"}},"playList":[list("default","kw_17"),list("love","kw_18"),list("user-a","kw_19")],"archiveExtension":"保留"}),
        ),
        (
            LxBackupKind::Settings,
            json!({"type":"setting_v2","data":{"version":"2.1.0","player.volume":0.75,"unknownNullable":null},"extension":{"version":9}}),
        ),
        (
            LxBackupKind::PlayList,
            json!({"type":"playList_v2","data":[list("default","kw_17"),list("love","kw_18"),list("user-a","kw_19")]}),
        ),
        (
            LxBackupKind::PlayListPart,
            json!({"type":"playListPart_v2","data":list("user-a","kw_17")}),
        ),
    ];
    for (kind, fixture) in fixtures {
        for encoding in [BackupEncoding::Json, BackupEncoding::Gzip] {
            let input = if encoding == BackupEncoding::Gzip {
                gzip(&bytes(&fixture))
            } else {
                bytes(&fixture)
            };
            let parsed = decode_lx(&input, BackupEncoding::Auto).unwrap();
            assert_eq!(parsed.kind, kind);
            assert!(!parsed.migrated_legacy);
            assert_eq!(parsed.to_value().unwrap(), fixture);
            let exported = encode_lx(&parsed, encoding).unwrap();
            assert_eq!(
                decode_lx(&exported, BackupEncoding::Auto)
                    .unwrap()
                    .to_value()
                    .unwrap(),
                fixture
            );
        }
    }
}

#[test]
fn absent_null_list_fields_and_builtin_location_extensions_are_preserved() {
    let fixture = json!({"type":"playList_v2","data":[{"id":"default","name":"list__name_default","locationUpdateTime":1234,"meta":{"scroll":42},"list":[modern("kw_17")]},{"id":"custom","name":"自建","source":null,"list":[]}]});
    let parsed = decode_lx(&bytes(&fixture), BackupEncoding::Json).unwrap();
    assert_eq!(parsed.to_value().unwrap(), fixture);
    let plan = parsed
        .preview(&ListData::default(), &ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert_eq!(plan.lists.user_list[0].info.source, None);
    assert_eq!(parsed.lists[0].info.location_update_time, Some(1234));
    assert_eq!(
        plan.archived_list_metadata[0].wire["meta"],
        json!({"scroll":42})
    );
}

#[test]
fn legacy_typo_music_quality_settings_and_double_serialization_migrate() {
    let legacy_song = json!({"source":"kg","name":"旧曲","singer":null,"songmid":23,"hash":"ABC","albumName":"旧专辑","types":[{"type":"flac32bit"}],"_types":{"flac32bit":{"hash":"x"}},"extension":{"keep":true}});
    let typo = json!({"type":"defautlList","data":{"list":[legacy_song.clone()]}});
    let double = serde_json::to_vec(&serde_json::to_string(&typo).unwrap()).unwrap();
    let parsed = decode_lx(&gzip(&double), BackupEncoding::Auto).unwrap();
    assert!(parsed.migrated_legacy);
    assert_eq!(parsed.lists[0].info.id, "default");
    assert_eq!(parsed.lists[0].tracks[0].lx_id, "23_ABC");
    assert_eq!(parsed.lists[0].tracks[0].platform.qualities, ["flac24bit"]);
    let migrated = parsed.to_value().unwrap();
    assert_eq!(migrated["type"], "playListPart_v2");
    assert_eq!(
        migrated["data"]["list"][0]["meta"]["extension"],
        json!({"keep":true})
    );
    let old_settings = json!({"type":"setting","data":{"version":"1.8.2","player":{"volume":0.4,"isShowLyricTransition":true},"desktopLyric":{"style":{"fontSize":125}},"sync":{"port":"23332","enable":true},"themeId":10,"unknownOld":{"keep":7}}});
    let settings = decode_lx(&bytes(&old_settings), BackupEncoding::Json)
        .unwrap()
        .settings
        .unwrap();
    assert_eq!(settings.values["version"], "2.1.0");
    assert_eq!(settings.values["player.isShowLyricTranslation"], true);
    assert_eq!(settings.values["desktopLyric.style.fontSize"], 20.0);
    assert_eq!(settings.values["sync.erver.port"], "23332");
    assert_eq!(settings.values["theme.id"], "pink");
    assert_eq!(settings.values["unknownOld"], json!({"keep":7}));
    let old_all = json!({"type":"allData","setting":{"version":"2.0.0"},"defaultList":{"list":[legacy_song.clone()]}});
    assert_eq!(
        decode_lx(&bytes(&old_all), BackupEncoding::Json)
            .unwrap()
            .lists
            .len(),
        1
    );
    let old_lists =
        json!({"type":"playList","data":[{"id":"old","name":"旧表","list":[legacy_song]}]});
    assert_eq!(
        decode_lx(&bytes(&old_lists), BackupEncoding::Json)
            .unwrap()
            .lists[0]
            .tracks[0]
            .title,
        "旧曲"
    );
}

#[test]
fn preview_merges_by_id_retains_local_name_order_and_untouched_lists() {
    let current = ListData {
        default_list: vec![track("kw_old")],
        love_list: vec![],
        user_list: vec![
            UserList {
                info: ListInfo::user("user-a", "本地名称"),
                tracks: vec![track("kw_old2")],
            },
            UserList {
                info: ListInfo::user("untouched", "不在备份中"),
                tracks: vec![track("kw_keep")],
            },
        ],
        temp_list: Some(vec![track("kw_temp")]),
    };
    let original = current.clone();
    let backup = decode_lx(
        &bytes(
            &json!({"type":"playList_v2","data":[list("user-a","kw_new"),list("new","kw_added")]}),
        ),
        BackupEncoding::Json,
    )
    .unwrap();
    let plan = backup
        .preview(&current, &ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert_eq!(current, original);
    assert_eq!(plan.lists.default_list, original.default_list);
    assert_eq!(plan.lists.temp_list, original.temp_list);
    assert_eq!(
        plan.lists
            .user_list
            .iter()
            .map(|list| list.info.id.as_str())
            .collect::<Vec<_>>(),
        ["user-a", "untouched", "new"]
    );
    assert_eq!(plan.lists.user_list[0].info.name, "本地名称");
    assert_eq!(plan.lists.user_list[0].tracks[0].lx_id, "kw_new");
    assert_eq!(plan.lists.user_list[1], original.user_list[1]);
    assert_eq!(
        plan.list_changes[0].disposition,
        ListImportDisposition::ReplacedTracks
    );
    let kept = backup
        .preview(&current, &ConflictPolicy::KeepExisting)
        .unwrap();
    assert_eq!(kept.lists.user_list[0], current.user_list[0]);
    let single = decode_lx(
        &bytes(&json!({"type":"playListPart_v2","data":list("user-a","kw_new")})),
        BackupEncoding::Json,
    )
    .unwrap();
    assert!(
        single
            .preview(
                &current,
                &ConflictPolicy::Copy {
                    new_list_id: "user-a".into()
                }
            )
            .is_err()
    );
    let copied = single
        .preview(
            &current,
            &ConflictPolicy::Copy {
                new_list_id: "new-copy".into(),
            },
        )
        .unwrap();
    assert_eq!(copied.lists.user_list[0], current.user_list[0]);
    assert_eq!(copied.lists.user_list.last().unwrap().info.id, "new-copy");
    assert_eq!(
        copied.list_changes[0].disposition,
        ListImportDisposition::Copied
    );
}

#[test]
fn malformed_duplicate_future_or_overbudget_data_never_returns_partial_plan() {
    for fixture in [
        json!({"type":"unknown_v3","data":[]}),
        json!({"type":"setting_v2","data":{"version":"9.0.0"}}),
        json!({"type":"setting_v2","data":{"player.volume":2}}),
        json!({"type":"setting_v2","data":{"sync.enable":"yes"}}),
        json!({"type":"playList_v2","data":[list("duplicate","kw_1"),list("duplicate","kw_2")]}),
        json!({"type":"playList_v2","data":[list("good","kw_1"),{"id":"broken","name":"坏","list":[{}]}]}),
    ] {
        assert!(decode_lx(&bytes(&fixture), BackupEncoding::Json).is_err());
    }
    let mut backup = decode_lx(
        &bytes(&json!({"type":"playListPart_v2","data":list("large","kw_1")})),
        BackupEncoding::Json,
    )
    .unwrap();
    backup.lists[0].tracks = (0..=MAX_IMPORT_TRACKS)
        .map(|i| {
            TrackRecord::from_lx_dto(
                json!({"id":format!("kw_{i}"),"name":"短曲","source":"kw","meta":{"songId":i}}),
                LxMusicDtoVersion::Modern,
            )
            .unwrap()
        })
        .collect();
    assert!(
        backup
            .preview(&ListData::default(), &ConflictPolicy::ReplaceTracks)
            .is_err()
    );
    assert_eq!(backup.lists[0].tracks.len(), MAX_IMPORT_TRACKS + 1);
}

#[test]
fn gzip_crc_truncation_concatenation_and_expansion_limits_are_checked() {
    let fixture = json!({"type":"setting_v2","data":{"version":"2.1.0"}});
    let compressed = gzip(&bytes(&fixture));
    assert!(decode_lx(&compressed[..compressed.len() - 3], BackupEncoding::Gzip).is_err());
    let mut corrupt = compressed.clone();
    let index = corrupt.len() - 8;
    corrupt[index] ^= 1;
    assert!(decode_lx(&corrupt, BackupEncoding::Gzip).is_err());
    let document = bytes(&fixture);
    let halfway = document.len() / 2;
    let mut concatenated = gzip(&document[..halfway]);
    concatenated.extend(gzip(&document[halfway..]));
    assert_eq!(
        decode_lx(&concatenated, BackupEncoding::Auto)
            .unwrap()
            .to_value()
            .unwrap(),
        fixture
    );
    let bomb = gzip(&vec![b' '; MAX_BACKUP_BYTES + 1]);
    assert!(bomb.len() < 64 * 1024);
    assert!(decode_lx(&bomb, BackupEncoding::Gzip).is_err());
    assert!(decode_lx(&vec![b' '; MAX_BACKUP_BYTES + 1], BackupEncoding::Json).is_err());
    assert!(
        encode_lx(
            &decode_lx(&document, BackupEncoding::Json).unwrap(),
            BackupEncoding::Auto
        )
        .is_err()
    );
}

#[test]
fn settings_archive_is_preserved_but_execution_preferences_are_reset() {
    let fixture = json!({"version":"2.1.0","common.isAgreePact":true,"sync.enable":true,"openAPI.enable":true,"openAPI.bindLan":true,"download.enable":true,"player.startupAutoPlay":true,"sourceApprovals":{"a":true},"urlApprovals":["https://example.test"],"localFileGrants":["C:/private.flac"],"futureSetting":{"opaque":1},"player.volume":0.25,"player.playbackRate":1.5,"player.preservesPitch":false,"player.soundEffect.biquadFilter.hz31":3.0});
    let settings = LxSettings::from_value(&fixture, false).unwrap();
    let preview = settings.preview().unwrap();
    assert_eq!(Value::Object(preview.archived.clone()), fixture);
    for key in [
        "common.isAgreePact",
        "sync.enable",
        "openAPI.enable",
        "openAPI.bindLan",
        "download.enable",
        "player.startupAutoPlay",
    ] {
        assert_eq!(preview.preferences[key], false);
    }
    for key in ["sourceApprovals", "urlApprovals", "localFileGrants"] {
        assert!(!preview.preferences.contains_key(key));
    }
    assert_eq!(preview.preferences["futureSetting"], json!({"opaque":1}));
    let store = preview.project_store(&StoreData::default()).unwrap();
    assert_eq!(store.volume, 0.25);
    assert_eq!(store.playback_processing.playback_rate, 1.5);
    assert!(!store.playback_processing.preserves_pitch);
    assert_eq!(store.equalizer.gains_db[0], 3.0);
}

#[test]
fn csv_quotes_newlines_unicode_unknown_columns_and_explicit_mapping_roundtrip() {
    let mut music = track("kw_1");
    music.title = "曲,\"名\"\r\n第二行".into();
    music.album = "专辑\n续行".into();
    let output = export_music_csv(
        &[music.clone()],
        ["歌名".into(), "歌手".into(), "专辑".into()],
    )
    .unwrap();
    assert!(output.starts_with(&[0xef, 0xbb, 0xbf]));
    let table = decode_csv(&output).unwrap();
    let mapped = table
        .map_music(&CsvMapping {
            name: 0,
            singer: Some(1),
            album: Some(2),
        })
        .unwrap();
    assert_eq!(mapped[0].name, music.title);
    assert_eq!(mapped[0].album, music.album);
    assert_eq!(mapped[0].singer, "甲、乙");
    let unknown =
        decode_csv("外部编号,专辑,歌名,歌手,扩展\r\n17,秋,夜,乙,保留\r\n".as_bytes()).unwrap();
    let rows = unknown
        .map_music(&CsvMapping {
            name: 2,
            singer: Some(3),
            album: Some(1),
        })
        .unwrap();
    assert_eq!(rows[0].original_columns[4], "保留");
    assert_eq!(decode_csv(&unknown.encode().unwrap()).unwrap(), unknown);
    for malformed in [
        "a,b\n\"unfinished,b",
        "a,b\nx\"y,z",
        "a,b\n\"x\"junk,y",
        "a,b\nx,y,z",
    ] {
        assert!(decode_csv(malformed.as_bytes()).is_err());
    }
    assert!(
        table
            .map_music(&CsvMapping {
                name: 7,
                singer: None,
                album: None
            })
            .is_err()
    );
    assert!(
        table
            .map_music(&CsvMapping {
                name: 0,
                singer: Some(0),
                album: None
            })
            .is_err()
    );
}

#[test]
fn csv_streaming_export_checks_expanded_quotes_at_exact_byte_budget() {
    let header = ["歌名".into(), "歌手".into(), "专辑".into()];
    let mut music = track("kw_budget");
    music.title = "\"".repeat(64 * 1024);
    music.artists.clear();
    music.album.clear();
    let mut tracks = vec![music; 128];
    // RFC quoting doubles each literal quote. Empty singer/album plus commas and
    // CRLF add six bytes per quoted row; the final album adds one ASCII byte.
    let prefix_bytes = "\u{feff}歌名,歌手,专辑\r\n".len();
    let remaining = MAX_BACKUP_BYTES - prefix_bytes - 127 * (2 * 64 * 1024 + 6);
    let final_quotes = (remaining - 7) / 2;
    tracks[127].title = "\"".repeat(final_quotes);
    tracks[127].album = "x".into();
    let output = export_music_csv(&tracks, header.clone()).unwrap();
    assert_eq!(output.len(), MAX_BACKUP_BYTES);
    let parsed = decode_csv(&output).unwrap();
    assert_eq!(parsed.rows.len(), 128);
    for (row, track) in parsed.rows.iter().zip(&tracks) {
        assert_eq!(row[0], track.title);
        assert_eq!(row[1], "");
        assert_eq!(row[2], track.album);
    }
    tracks[127].title.push('"');
    assert!(export_music_csv(&tracks, header).is_err());
    let mut artists = track("kw_artists");
    artists.title = "合唱".into();
    artists.artists = vec!["甲,乙".into(), "丙\"丁".into(), "戊\n己".into()];
    let exported =
        export_music_csv(&[artists.clone()], ["n".into(), "s".into(), "a".into()]).unwrap();
    assert_eq!(
        decode_csv(&exported).unwrap().rows[0][1],
        artists.artists.join("、")
    );
    artists.artists = vec!["甲".repeat(180_000), "乙".repeat(180_000)];
    assert!(export_music_csv(&[artists], ["n".into(), "s".into(), "a".into()]).is_err());
}

#[test]
fn native_backup_separates_optional_lyrics_sources_dislike_and_download_plans() {
    let mut source = import_script("/* @name 合法样例 */\nconst sample = 1;").unwrap();
    source.approve().unwrap();
    let store = StoreData {
        sources: vec![source.clone()],
        selected: Some(source.sha256.clone()),
        local_files: vec!["C:/unverified/song.flac".into()],
        ..StoreData::default()
    };
    let lists = ListData {
        default_list: vec![track("kw_1")],
        ..ListData::default()
    };
    let lyrics = vec![NativeEditedLyrics {
        track_id: lists.default_list[0].id.clone(),
        bundle: LyricBundle {
            lyric: LyricField::Text("[00:01]自行编辑".into()),
            tlyric: LyricField::Null,
            ..LyricBundle::default()
        },
    }];
    let default = NativeBackup::from_snapshot(
        &store,
        &lists,
        "歌曲@歌手",
        &[],
        &lyrics,
        NativeBackupOptions::default(),
    )
    .unwrap();
    assert!(
        default.sources.is_empty()
            && default.downloads.is_none()
            && default.edited_lyrics.is_none()
    );
    let mut included = NativeBackup::from_snapshot(
        &store,
        &lists,
        "歌曲@歌手",
        &[],
        &lyrics,
        NativeBackupOptions {
            include_sources: true,
            include_downloads: true,
            include_edited_lyrics: true,
        },
    )
    .unwrap();
    included.downloads = Some(vec![NativeDownloadPlan {
        record: track("kw_2"),
        requested_quality: DownloadQuality::K128,
        list_id: Some("default".into()),
        list_name_snapshot: Some("试听".into()),
        order: 0,
        settings: DownloadSettings::default(),
    }]);
    included
        .extensions
        .insert("futureData".into(), json!({"keep":true}));
    let encoded = encode_native(&included, BackupEncoding::Gzip).unwrap();
    let imported = decode_native(&encoded, BackupEncoding::Auto).unwrap();
    let plan = imported
        .preview(&ListData::default(), &ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert_eq!(plan.unapproved_sources.len(), 1);
    assert!(!plan.unapproved_sources[0].approved);
    assert_eq!(plan.unapproved_sources[0].approval_policy_version, 0);
    assert!(plan.unapproved_sources[0].require_approval().is_err());
    assert!(
        plan.store_preferences.sources.is_empty()
            && plan.store_preferences.local_files.is_empty()
            && plan.store_preferences.selected.is_none()
    );
    assert_eq!(imported.declared_local_paths, ["C:/unverified/song.flac"]);
    assert_eq!(plan.edited_lyrics.unwrap()[0].bundle, lyrics[0].bundle);
    assert!(plan.dislike.matches("歌曲", "歌手"));
    let download = &plan.download_plans.unwrap()[0];
    assert!(!download.settings.enabled);
    assert!(download.settings.destination_grant_id.is_none());
    let raw: Value =
        serde_json::from_slice(&encode_native(&imported, BackupEncoding::Json).unwrap()).unwrap();
    assert_eq!(raw["type"], "lx_native_backup");
    assert_eq!(raw["version"], 1);
    assert_eq!(raw["sources"][0].as_object().unwrap().len(), 1);
    for field in [
        "url_bookmark",
        "original_transfer",
        "owned_part",
        "destination",
        "commit_journal",
        "output_audio",
    ] {
        assert!(raw["downloads"][0].get(field).is_none());
    }
}

#[test]
fn uploaded_native_permissions_cannot_restore_and_unknown_versions_fail() {
    let snapshot = NativeBackup::from_snapshot(
        &StoreData::default(),
        &ListData::default(),
        "",
        &[],
        &[],
        NativeBackupOptions::default(),
    )
    .unwrap();
    let mut value: Value =
        serde_json::from_slice(&encode_native(&snapshot, BackupEncoding::Json).unwrap()).unwrap();
    let mut source = import_script("/* @name 伪权限 */\nconst x = 1;").unwrap();
    source.approve().unwrap();
    value["store"]["sources"] = serde_json::to_value(vec![source]).unwrap();
    value["store"]["selected"] = "uploaded".into();
    value["store"]["local_files"] = json!(["C:/private.flac"]);
    let restored = decode_native(&bytes(&value), BackupEncoding::Json).unwrap();
    assert!(
        restored.store.sources.is_empty()
            && restored.store.local_files.is_empty()
            && restored.store.selected.is_none()
    );
    assert_eq!(restored.declared_local_paths, ["C:/private.flac"]);
    value["version"] = 2.into();
    assert!(decode_native(&bytes(&value), BackupEncoding::Json).is_err());
    assert!(
        decode_lx(
            &encode_native(&snapshot, BackupEncoding::Json).unwrap(),
            BackupEncoding::Json
        )
        .is_err()
    );
    value["version"] = 1.into();
    value["destinationGrants"] = json!([{}]);
    assert!(decode_native(&bytes(&value), BackupEncoding::Json).is_err());
}

#[test]
fn imported_local_paths_are_unverified_metadata_without_filesystem_access() {
    let local = json!({"id":"local_external","name":"未定位本地曲","singer":"","source":"local","meta":{"songId":"Z:/never-created.flac","filePath":"Z:/never-created.flac","ext":"flac"}});
    let backup = decode_lx(&bytes(&json!({"type":"playListPart_v2","data":{"id":"local-list","name":"本地引用","list":[local]}})), BackupEncoding::Json).unwrap();
    let plan = backup
        .preview(&ListData::default(), &ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert_eq!(plan.local_references.len(), 1);
    assert_eq!(
        plan.local_references[0].declared_path,
        "Z:/never-created.flac"
    );
    assert_eq!(plan.lists.user_list[0].tracks[0].platform.source, "local");
}
