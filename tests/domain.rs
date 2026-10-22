use lx_gpui_player::domain::{
    lyric::{LyricBundle, LyricField, LyricKind, MAX_LYRIC_BYTES},
    music::{LocalAsset, LxMusicDtoVersion, TrackRecord, local_track_id},
};
use serde_json::json;

#[test]
fn relocated_local_export_projects_current_locator_without_changing_archived_wire() {
    let old_path = std::env::temp_dir().join("lx-wire-old.wav");
    let new_path = std::env::temp_dir().join("lx-wire-new.flac");
    for version in [LxMusicDtoVersion::Legacy, LxMusicDtoVersion::Modern] {
        let original = match version {
            LxMusicDtoVersion::Legacy => json!({"source":"local","name":"Original","singer":null,
                "songmid":old_path,"filePath":old_path,"ext":"wav","future":{"retained":true}}),
            LxMusicDtoVersion::Modern => {
                json!({"id":"stable-lx-id","source":"local","name":"Original","singer":null,
                "future":{"retained":true},"meta":{"songId":old_path,"filePath":old_path,"ext":"wav","privateField":null}})
            }
        };
        let mut record = TrackRecord::from_lx_dto(original.clone(), version).unwrap();
        let identity = (
            record.id.clone(),
            record.lx_id.clone(),
            record.local_asset_id.clone(),
        );
        record.platform.song_id = json!(new_path);
        record.metadata.insert("filePath".into(), json!(new_path));
        record.metadata.insert("ext".into(), json!("flac"));
        let exported = record.to_lx_dto(version);
        let physical = match version {
            LxMusicDtoVersion::Legacy => &exported,
            LxMusicDtoVersion::Modern => &exported["meta"],
        };
        assert_eq!(physical["filePath"], json!(new_path));
        assert_eq!(physical["ext"], "flac");
        assert_eq!(exported["future"], original["future"]);
        assert_eq!(record.wire.as_ref().unwrap().original, original);
        assert_eq!(
            (
                record.id.clone(),
                record.lx_id.clone(),
                record.local_asset_id.clone()
            ),
            identity
        );
    }
}

#[test]
fn lyric_channels_preserve_missing_null_empty_and_unicode_through_storage() {
    let input = json!({"lyric":"[00:01.000]你好🎵", "tlyric":null, "rlyric":""});
    let bundle = LyricBundle::from_value(&input).unwrap();
    assert_eq!(bundle.tlyric, LyricField::Null);
    assert_eq!(bundle.rlyric, LyricField::Text(String::new()));
    assert_eq!(bundle.lxlyric, LyricField::Missing);
    let reopened: LyricBundle =
        serde_json::from_str(&serde_json::to_string(&bundle).unwrap()).unwrap();
    assert_eq!(reopened, bundle);
    assert_eq!(reopened.to_value(), input);
    let mut changed = reopened;
    changed.set_field(LyricKind::Lxlyric, LyricField::Null);
    assert!(changed.to_value().get("lxlyric").unwrap().is_null());
}

#[test]
fn lyric_payload_rejects_wrong_types_unknown_channels_and_oversized_text() {
    for value in [
        json!([]),
        json!({"lyric":false}),
        json!({"lyric":["x"]}),
        json!({"lyric":"ok", "unexpected":"x"}),
        json!({"lyric":"x\0y"}),
    ] {
        assert!(LyricBundle::from_value(&value).is_err(), "{value}");
    }
    assert!(LyricBundle::from_value(&json!({"lyric":"x".repeat(MAX_LYRIC_BYTES + 1)})).is_err());
}

#[test]
fn modern_music_roundtrip_preserves_numeric_ids_meta_nulls_and_unknown_fields() {
    let value = json!({"id":"wy_9007199254740993", "source":"wy", "name":"歌", "singer":"甲、乙",
        "interval":null,"futureRoot":{"k":true}, "meta":{"songId":9007199254740993u64,
        "albumName":"专辑", "picUrl":null,"albumId":2,"qualitys":[{"type":"flac","size":null}],
        "_qualitys":{"flac":{"size":null}},"futurePlatform":{"lossless":true}}});
    let track = TrackRecord::from_lx_dto(value.clone(), LxMusicDtoVersion::Modern).unwrap();
    assert_eq!(track.platform.song_id, json!(9007199254740993u64));
    assert_eq!(track.platform.qualities, vec!["flac"]);
    assert_eq!(track.to_lx_dto(LxMusicDtoVersion::Modern), value);
    let stored: TrackRecord =
        serde_json::from_str(&serde_json::to_string(&track).unwrap()).unwrap();
    assert_eq!(stored, track);
    let legacy = track.to_lx_dto(LxMusicDtoVersion::Legacy);
    assert_eq!(legacy["songmid"], value["meta"]["songId"]);
    assert_eq!(legacy["futurePlatform"], value["meta"]["futurePlatform"]);
    assert_eq!(legacy["futureRoot"], value["futureRoot"]);
}

#[test]
fn legacy_platform_mappings_and_flac_migration_keep_original_export() {
    let cases = [
        ("kg", json!({"hash":"hash-320","albumAudioId":"a"})),
        (
            "tx",
            json!({"songId":42,"strMediaMid":"media","albumMid":"album"}),
        ),
        (
            "mg",
            json!({"copyrightId":"cr","lrcUrl":"l","mrcUrl":"m","trcUrl":"t"}),
        ),
    ];
    for (source, extra) in cases {
        let mut value = json!({"source":source,"songmid":"00042","name":"歌曲","singer":"歌手",
            "interval":"03:55","albumName":"","img":null,"albumId":null,
            "types":[{"type":"flac32bit","hash":"quality-hash","size":null}],
            "_types":{"flac32bit":{"size":null,"hash":"quality-hash"}},"future":"kept"});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let track = TrackRecord::from_lx_dto(value.clone(), LxMusicDtoVersion::Legacy).unwrap();
        assert_eq!(track.duration_ms, Some(235000));
        assert_eq!(track.platform.song_id, json!("00042"));
        assert_eq!(track.platform.qualities, vec!["flac24bit"]);
        assert_eq!(track.to_lx_dto(LxMusicDtoVersion::Legacy), value);
        let modern = track.to_lx_dto(LxMusicDtoVersion::Modern);
        assert_eq!(
            modern["meta"]["_qualitys"]["flac24bit"]["hash"],
            "quality-hash"
        );
        match source {
            "kg" => {
                assert_eq!(track.lx_id, "00042_hash-320");
                assert_eq!(modern["meta"]["albumAudioId"], "a");
            }
            "tx" => {
                assert_eq!(modern["meta"]["id"], 42);
                assert_eq!(modern["meta"]["strMediaMid"], "media");
            }
            "mg" => {
                assert_eq!(modern["meta"]["copyrightId"], "cr");
                assert_eq!(modern["meta"]["mrcUrl"], "m");
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn local_asset_identity_is_independent_of_import_order_and_survives_serialization() {
    let first = LocalAsset::new(
        "track",
        std::env::temp_dir()
            .join("music")
            .join(".")
            .join("song.flac"),
        100,
    );
    let second = LocalAsset::new(
        "another",
        std::env::temp_dir().join("music").join("song.flac"),
        200,
    );
    assert_eq!(first.id, second.id);
    assert_eq!(first.extension, "flac");
    let stored: LocalAsset = serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
    assert_eq!(stored, first);
    assert_ne!(
        first.id,
        LocalAsset::new("track", first.path.with_file_name("other.flac"), 100).id
    );
}

#[test]
fn domain_edits_export_without_replacing_unrelated_platform_fields() {
    let value = json!({"id":"tx_4", "source":"tx", "name":"旧", "singer":"甲", "interval":"1:00",
        "meta":{"songId":"4","albumName":"原专辑","strMediaMid":"opaque","id":44,"qualitys":[],"_qualitys":{}}});
    let mut track = TrackRecord::from_lx_dto(value, LxMusicDtoVersion::Modern).unwrap();
    track.title = "新".into();
    track.album.clear();
    track.duration_ms = Some(90000);
    let exported = track.to_lx_dto(LxMusicDtoVersion::Modern);
    assert_eq!(exported["name"], "新");
    assert_eq!(exported["meta"]["albumName"], "");
    assert_eq!(exported["meta"]["strMediaMid"], "opaque");
    assert_eq!(exported["interval"], "1:30");
    assert_eq!(track.to_lx_dto(LxMusicDtoVersion::Legacy)["songId"], 44);
}

#[test]
fn resolver_payload_uses_current_metadata_while_original_wire_roundtrip_keeps_absence_and_legacy_quality()
 {
    let original = json!({"source":"kg","songmid":"7","name":"Original","singer":null,"interval":null,"albumName":"Album","hash":"old-hash","albumAudioId":1,"strMediaMid":"old-media","future":{"version":"old"},"removeMe":"old","types":[{"type":"flac32bit","size":null,"hash":"old-quality"}],"_types":{"flac32bit":{"size":null,"hash":"old-quality"}}});
    let mut track = TrackRecord::from_lx_dto(original.clone(), LxMusicDtoVersion::Legacy).unwrap();
    assert_eq!(track.to_lx_dto(LxMusicDtoVersion::Legacy), original);
    let normalized = track.to_source_music_info();
    assert_eq!(normalized["types"][0]["type"], "flac24bit");
    assert_eq!(normalized["_types"]["flac24bit"]["hash"], "old-quality");
    assert!(normalized["_types"].get("flac32bit").is_none());
    assert_eq!(track.to_lx_dto(LxMusicDtoVersion::Legacy), original);
    track.metadata.insert("hash".into(), json!("new-hash"));
    track.metadata.insert("albumAudioId".into(), json!(2));
    track
        .metadata
        .insert("strMediaMid".into(), json!("new-media"));
    track
        .metadata
        .insert("future".into(), json!({"version":"new"}));
    track.metadata.remove("removeMe");
    track.metadata["_qualitys"]["flac24bit"]["hash"] = json!("new-quality");
    let stored: TrackRecord =
        serde_json::from_value(serde_json::to_value(&track).unwrap()).unwrap();
    let current = stored.to_source_music_info();
    assert_eq!(current["hash"], "new-hash");
    assert_eq!(current["albumAudioId"], 2);
    assert_eq!(current["strMediaMid"], "new-media");
    assert_eq!(current["future"]["version"], "new");
    assert!(current.get("removeMe").is_none());
    assert_eq!(current["_types"]["flac24bit"]["hash"], "new-quality");
    assert_eq!(stored.wire.as_ref().unwrap().original, original);
    let original_modern = json!({"id":"tx_7","source":"tx","name":"TX","singer":"Singer","interval":"00:01","futureRoot":{"retained":true},"meta":{"songId":"7","albumName":"Album","id":70,"strMediaMid":"old-mid","future":{"old":true},"qualitys":[],"_qualitys":{}}});
    let mut modern =
        TrackRecord::from_lx_dto(original_modern.clone(), LxMusicDtoVersion::Modern).unwrap();
    assert_eq!(modern.to_lx_dto(LxMusicDtoVersion::Modern), original_modern);
    modern.metadata.insert("id".into(), json!(71));
    modern
        .metadata
        .insert("strMediaMid".into(), json!("current-mid"));
    modern
        .metadata
        .insert("future".into(), json!({"current":true}));
    let current = modern.to_source_music_info();
    assert_eq!(current["songId"], 71);
    assert_eq!(current["strMediaMid"], "current-mid");
    assert_eq!(current["future"]["current"], true);
    assert_eq!(current["futureRoot"]["retained"], true);
    assert_eq!(current["songmid"], "7");
}

#[test]
fn malformed_music_dto_is_rejected_and_unknown_sources_remain_importable() {
    for input in [
        json!([]),
        json!({"source":"wy","name":"x","meta":{}}),
        json!({"source":"wy","name":"x","id":"x","meta":{"songId":[]}}),
    ] {
        assert!(TrackRecord::from_lx_dto(input, LxMusicDtoVersion::Modern).is_err());
    }
    let old = json!({"source":"closed-future","songmid":123,"name":"x","singer":null});
    let track = TrackRecord::from_lx_dto(old.clone(), LxMusicDtoVersion::Legacy).unwrap();
    assert_eq!(track.platform.source, "closed-future");
    assert_eq!(track.to_lx_dto(LxMusicDtoVersion::Legacy), old);
}

#[cfg(windows)]
#[test]
fn windows_local_identity_normalizes_verbatim_prefix_separator_and_case() {
    use std::path::Path;
    assert_eq!(
        local_track_id(Path::new(r"C:\Music\Song.FLAC")),
        local_track_id(Path::new(r"\\?\C:\music\song.flac"))
    );
    assert_eq!(
        local_track_id(Path::new(r"C:\Music\Song.FLAC")),
        local_track_id(Path::new("c:/music/song.flac"))
    );
    assert_eq!(
        local_track_id(Path::new(r"\\server\Share\Song.flac")),
        local_track_id(Path::new(r"\\?\UNC\server\share\song.flac"))
    );
}
