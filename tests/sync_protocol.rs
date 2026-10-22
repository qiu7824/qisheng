//! Pure LX v4 protocol/crypto fixtures; no sync sockets, external peer, or grants.
use lx_gpui_player::sync::*;
use serde_json::{Value, json};

fn music(id: &str, name: &str) -> Value {
    json!({"id":id,"name":name,"singer":"Artist","source":"kw","interval":null,"meta":{"songId":id,"albumName":"Album","qualitys":[],"_qualitys":{}}})
}
fn data(ids: &[&str]) -> ListData {
    ListData::from_wire(&json!({"defaultList":ids.iter().map(|id|music(id,id)).collect::<Vec<_>>(),"loveList":[],"userList":[]})).unwrap()
}
fn user(id: &str, name: &str, time: Option<u64>, songs: &[&str]) -> UserList {
    let mut value =
        json!({"id":id,"name":name,"list":songs.iter().map(|id|music(id,id)).collect::<Vec<_>>()});
    if let Some(time) = time {
        value["locationUpdateTime"] = Value::from(time);
    }
    UserList::from_wire(&value).unwrap()
}
fn song_ids(data: &ListData) -> Vec<&str> {
    data.default_list.iter().map(MusicInfo::id).collect()
}

#[test]
fn actual_three_way_metadata_matches_original_javascript_numeric_equality() {
    for (baseline, local, remote_selected) in [
        (json!(7), json!("07"), true),
        (json!(7), json!("7e0"), true),
        (json!(7), json!(" 7 "), true),
        (json!(7), json!("0x7"), true),
        (json!(7), json!("0b111"), true),
        (json!(7), json!("0o7"), true),
        (json!(0), json!(""), true),
        (json!(0), json!("\u{feff}\u{00a0}"), true),
        (json!(0), json!("-0"), true),
        (json!(7), json!("NaN"), false),
        (json!(7), json!("7x"), false),
        (json!("07"), json!("7"), false),
        (json!("07"), json!(7), true),
    ] {
        let mut base = data(&[]);
        let mut original = user("u", "baseline", None, &[]);
        original.source_list_id = Some(baseline);
        base.user_list.push(original);
        let mut left = base.clone();
        left.user_list[0].name = json!("local name");
        left.user_list[0].source_list_id = Some(local.clone());
        let mut right = base.clone();
        right.user_list[0].source_list_id = Some(json!("9"));
        let merged = merge_lists_three_way(&left, &right, &base, AddLocation::Top).unwrap();
        assert_eq!(merged.user_list[0].name, json!("local name"));
        assert_eq!(
            merged.user_list[0].source_list_id,
            Some(if remote_selected { json!("9") } else { local })
        );
    }
}

#[test]
fn newly_created_shared_list_treats_missing_and_null_as_equal_metadata_only() {
    let base = data(&[]);
    for null in [false, true] {
        let mut left = data(&[]);
        let mut local = user("g", "native G", None, &["local-song"]);
        if null {
            local.source = Some(Value::Null);
            local.source_list_id = Some(Value::Null);
        }
        left.user_list.push(local);
        let mut right = data(&[]);
        let mut remote = user("g", "remote G", None, &["remote-song"]);
        remote.source = Some(json!("kw"));
        remote.source_list_id = Some(json!("17"));
        right.user_list.push(remote);
        let merged = merge_lists_three_way(&left, &right, &base, AddLocation::Bottom).unwrap();
        assert_eq!(merged.user_list[0].name, json!("native G"));
        assert_eq!(merged.user_list[0].source, Some(json!("kw")));
        assert_eq!(merged.user_list[0].source_list_id, Some(json!("17")));
        assert_eq!(
            merged.user_list[0]
                .list
                .iter()
                .map(MusicInfo::id)
                .collect::<Vec<_>>(),
            vec!["local-song", "remote-song"]
        );
    }
    let mut left = data(&[]);
    left.user_list.push(user("07", "left", None, &[]));
    let mut right = data(&[]);
    right.user_list.push(user("7", "right", None, &[]));
    let merged = merge_lists_three_way(&left, &right, &base, AddLocation::Bottom).unwrap();
    assert_eq!(
        merged
            .user_list
            .iter()
            .map(|list| list.id.as_str())
            .collect::<Vec<_>>(),
        vec!["07", "7"]
    );
}

#[test]
fn exact_rpc_request_reply_void_and_no_queue_group_wire() {
    let request = RpcFrame::Request {
        name: "getEnabledFeatures__125".into(),
        path: vec!["getEnabledFeatures".into()],
        data: vec![json!("desktop-app"), json!({"list":1,"dislike":1})],
    };
    assert_eq!(
        serialize_frame(&request).unwrap(),
        r#"{"name":"getEnabledFeatures__125","path":["getEnabledFeatures"],"data":["desktop-app",{"list":1,"dislike":1}]}"#
    );
    assert_eq!(
        parse_frame(&serialize_frame(&request).unwrap()).unwrap(),
        request
    );
    for (number, canonical) in [
        ("1.0", "1"),
        ("1e3", "1000"),
        ("9007199254740993", "9007199254740992"),
        ("1e21", "1e+21"),
        ("-0", "0"),
    ] {
        let parsed = parse_frame(&format!(
            "{{\"name\":\"numeric\",\"error\":null,\"data\":{number}}}"
        ))
        .unwrap();
        assert_eq!(
            serialize_frame(&parsed).unwrap(),
            format!("{{\"name\":\"numeric\",\"error\":null,\"data\":{canonical}}}")
        );
    }
    let reply = RpcFrame::Reply {
        name: "onListSyncAction__125".into(),
        error: None,
        data: None,
    };
    assert_eq!(
        serialize_frame(&reply).unwrap(),
        r#"{"name":"onListSyncAction__125","error":null}"#
    );
    assert_eq!(
        parse_frame(r#"{"name":"x","error":null,"data":null}"#).unwrap(),
        RpcFrame::Reply {
            name: "x".into(),
            error: None,
            data: Some(Value::Null)
        }
    );
    assert_eq!(
        parse_frame(r#"{"name":"x","error":"readonly"}"#).unwrap(),
        RpcFrame::Reply {
            name: "x".into(),
            error: Some("readonly".into()),
            data: None
        }
    );
}
#[test]
fn rpc_rejects_proto_methods_bad_parameters_duplicate_fields_and_unknown_fields() {
    for wire in [
        r#"{"name":"x","path":["constructor"],"data":[]}"#,
        r#"{"name":"x","path":["__proto__","constructor"],"data":[]}"#,
        r#"{"name":"x","path":["finished"],"data":[1]}"#,
        r#"{"name":"x","name":"y","error":null}"#,
        r#"{"name":"x","error":null,"sourcePermissions":{}}"#,
        r#"{"name":"x","path":["onListSyncAction"],"data":[1]}"#,
    ] {
        assert!(parse_frame(wire).is_err(), "{wire}");
    }
}
#[test]
fn gzip_threshold_counts_utf16_not_utf8_and_roundtrips() {
    for (value, compressed) in [
        ("x".repeat(1024), false),
        ("x".repeat(1025), true),
        ("中".repeat(1024), false),
        ("😀".repeat(512), false),
        ("😀".repeat(513), true),
    ] {
        let encoded = encode_message(&value).unwrap();
        assert_eq!(encoded.starts_with("cg_"), compressed);
        assert_eq!(decode_message(&encoded).unwrap(), value);
    }
}
#[test]
fn gzip_crc_bomb_json_depth_and_actual_frame_budget_are_bounded() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&vec![b'x'; MAX_JSON_BYTES + 1]).unwrap();
    let bytes = gzip.finish().unwrap();
    assert!(decode_message(&format!("cg_{}", STANDARD.encode(bytes))).is_err());
    assert!(decode_message("cg_not-base64").is_err());
    assert!(decode_message(&"x".repeat(MAX_WIRE_BYTES + 1)).is_err());
    let deep = format!(
        "{{\"name\":\"x\",\"error\":null,\"data\":{}0{}}}",
        "[".repeat(34),
        "]".repeat(34)
    );
    assert!(parse_frame(&deep).is_err());
    let mut encoded = STANDARD
        .decode(
            encode_message(&"x".repeat(1025))
                .unwrap()
                .trim_start_matches("cg_"),
        )
        .unwrap();
    *encoded.last_mut().unwrap() ^= 1;
    assert!(decode_message(&format!("cg_{}", STANDARD.encode(encoded))).is_err());
    let gzip = |text: &[u8]| {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(text).unwrap();
        gzip.finish().unwrap()
    };
    let mut joined = gzip(b"first");
    joined.extend(gzip(b"second"));
    assert_eq!(
        decode_message(&format!("cg_{}", STANDARD.encode(&joined))).unwrap(),
        "firstsecond"
    );
    joined.extend(b"garbage");
    assert!(decode_message(&format!("cg_{}", STANDARD.encode(joined))).is_err());
    let mut bomb = gzip(&vec![b'a'; MAX_JSON_BYTES]);
    bomb.extend(gzip(b"b"));
    assert!(decode_message(&format!("cg_{}", STANDARD.encode(bomb))).is_err());
}
#[test]
fn list_snapshot_digest_order_and_absent_null_metadata_are_preserved() {
    let mut snapshot = data(&["a"]);
    snapshot.user_list.push(user("u", "Name", None, &[]));
    let wire = snapshot.wire();
    assert!(wire["userList"][0].get("source").is_none());
    let mut nullable = wire.clone();
    nullable["userList"][0]["source"] = Value::Null;
    let other = ListData::from_wire(&nullable).unwrap();
    assert_ne!(snapshot.digest().unwrap(), other.digest().unwrap());
    let text = serialize_frame(&RpcFrame::Reply {
        name: "x".into(),
        error: None,
        data: Some(snapshot.wire()),
    })
    .unwrap();
    assert!(text.contains("\"defaultList\""));
    assert!(text.find("\"defaultList\"").unwrap() < text.find("\"loveList\"").unwrap());
    assert_eq!(ListData::from_wire(&snapshot.wire()).unwrap(), snapshot);
}
#[test]
fn all_twelve_list_and_three_dislike_actions_validate_actual_wire_shapes() {
    let song = music("m", "Music");
    let actions = vec![
        json!({"action":"list_data_overwrite","data":data(&[]).wire()}),
        json!({"action":"list_create","data":{"position":0,"listInfos":[{"id":"u","name":"User"}]}}),
        json!({"action":"list_remove","data":["u"]}),
        json!({"action":"list_update","data":[{"id":"u","name":"Changed"}]}),
        json!({"action":"list_update_position","data":{"position":1,"ids":["u"]}}),
        json!({"action":"list_music_add","data":{"id":"love","musicInfos":[song.clone()],"addMusicLocationType":"top"}}),
        json!({"action":"list_music_move","data":{"fromId":"default","toId":"love","musicInfos":[song.clone()],"addMusicLocationType":"bottom"}}),
        json!({"action":"list_music_remove","data":{"listId":"love","ids":["m"]}}),
        json!({"action":"list_music_update","data":[{"id":"love","musicInfo":song.clone()}]}),
        json!({"action":"list_music_update_position","data":{"listId":"love","position":0,"ids":["m"]}}),
        json!({"action":"list_music_overwrite","data":{"listId":"love","musicInfos":[song]}}),
        json!({"action":"list_music_clear","data":["default","love"]}),
    ];
    assert_eq!(actions.len(), 12);
    for action in actions {
        assert!(ListAction::from_wire(&action).is_ok(), "{action}");
        let frame = RpcFrame::Request {
            name: "action__125".into(),
            path: vec!["onListSyncAction".into()],
            data: vec![action],
        };
        assert!(serialize_frame(&frame).is_ok());
    }
    for action in [
        json!({"action":"dislike_data_overwrite","data":"music@artist"}),
        json!({"action":"dislike_music_add","data":[{"name":"Music","singer":"Artist"}]}),
        json!({"action":"dislike_music_clear"}),
    ] {
        assert!(DislikeAction::from_wire(&action).is_ok());
    }
}
#[test]
fn capabilities_and_datasets_never_admit_source_file_lyrics_or_temp_permissions() {
    assert_eq!(
        PeerCapabilities::from_wire(&json!({"list":{"skipSnapshot":false},"dislike":false}))
            .unwrap(),
        PeerCapabilities {
            list: Some(false),
            dislike: None
        }
    );
    for feature in ["sourcePermissions", "lyrics", "playbackClock", "files"] {
        assert!(PeerCapabilities::from_wire(&json!({feature:{}})).is_err());
    }
    let mut local = music("m", "Name");
    local["source"] = json!("local");
    local["meta"]["filePath"] = json!("C:/private.wav");
    assert!(MusicInfo::from_wire(local).is_err());
    for field in ["permissions", "approvedUrl", "lyric", "headers", "cookie"] {
        let mut song = music("m", "Name");
        song["meta"][field] = json!("private");
        assert!(MusicInfo::from_wire(song).is_err());
    }
    assert!(
        ListData::from_wire(&json!({"defaultList":[],"loveList":[],"userList":[],"tempList":[]}))
            .is_err()
    );
    assert!(ListAction::from_wire(&json!({"action":"list_music_add","data":{"id":"temp","musicInfos":[],"addMusicLocationType":"top"}})).is_err());
}

#[test]
fn local_references_preserve_exact_metadata_ids_without_grants_or_filesystem_access() {
    let wire = json!({"id":"original-local-ID","name":"Local","singer":"Artist","source":"local","interval":null,"meta":{"songId":"V:\\opaque\\not-opened.wav","albumName":"Album","filePath":"V:\\opaque\\not-opened.wav","ext":"wav"}});
    let info = MusicInfo::from_wire(wire.clone()).unwrap();
    assert_eq!(info.kind(), MusicKind::LocalReference);
    assert!(info.is_local_reference());
    assert_eq!(info.id(), "original-local-ID");
    assert_eq!(info.wire(), &wire);
    let snapshot =
        ListData::from_wire(&json!({"defaultList":[wire.clone()],"loveList":[],"userList":[]}))
            .unwrap();
    assert_eq!(snapshot.wire()["defaultList"][0], wire);
    for field in [
        "local_asset_id",
        "grant",
        "sourcePermissions",
        "fileProof",
        "rootGrant",
    ] {
        let mut forbidden = wire.clone();
        forbidden["meta"][field] = json!({"approved":true});
        assert!(MusicInfo::from_wire(forbidden).is_err());
    }
}

#[test]
fn music_identifiers_named_like_private_lists_are_valid_but_list_selectors_are_rejected() {
    for action in [
        json!({"action":"list_music_remove","data":{"listId":"love","ids":["temp","download"]}}),
        json!({"action":"list_music_update_position","data":{"listId":"love","ids":["temp","download"],"position":0}}),
    ] {
        assert!(ListAction::from_wire(&action).is_ok());
    }
    for action in [
        json!({"action":"list_remove","data":["temp"]}),
        json!({"action":"list_update_position","data":{"ids":["download"],"position":0}}),
        json!({"action":"list_music_clear","data":["temp"]}),
        json!({"action":"list_music_remove","data":{"listId":"temp","ids":["normal-song"]}}),
    ] {
        assert!(ListAction::from_wire(&action).is_err());
    }
}
#[test]
fn first_sync_all_seven_modes_empty_sides_and_cancel_follow_exact_rules() {
    let local = data(&["a", "b"]);
    let remote = data(&["b", "c"]);
    assert_eq!(
        song_ids(
            &first_sync_lists(
                &local,
                &remote,
                SyncMode::MergeLocalRemote,
                AddLocation::Top
            )
            .unwrap()
            .data
        ),
        vec!["c", "a", "b"]
    );
    assert_eq!(
        song_ids(
            &first_sync_lists(
                &local,
                &remote,
                SyncMode::MergeLocalRemote,
                AddLocation::Bottom
            )
            .unwrap()
            .data
        ),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        song_ids(
            &first_sync_lists(
                &local,
                &remote,
                SyncMode::MergeRemoteLocal,
                AddLocation::Top
            )
            .unwrap()
            .data
        ),
        vec!["a", "b", "c"]
    );
    for mode in [
        SyncMode::OverwriteLocalRemote,
        SyncMode::OverwriteRemoteLocal,
        SyncMode::OverwriteLocalRemoteFull,
        SyncMode::OverwriteRemoteLocalFull,
    ] {
        assert!(first_sync_lists(&local, &remote, mode, AddLocation::Top).is_ok());
    }
    assert!(first_sync_lists(&local, &remote, SyncMode::Cancel, AddLocation::Top).is_err());
    let empty = ListData::default();
    let plan = first_sync_lists(&empty, &remote, SyncMode::Cancel, AddLocation::Top).unwrap();
    assert!(plan.update_local && !plan.update_remote);
    assert_eq!(plan.data, remote);
}
#[test]
fn ordinary_overwrite_preserves_target_only_lists_full_overwrite_removes_them() {
    let mut local = data(&["a"]);
    local.user_list.push(user("local", "Local", None, &[]));
    let mut remote = data(&["b"]);
    remote.user_list.push(user("remote", "Remote", None, &[]));
    let ordinary = first_sync_lists(
        &local,
        &remote,
        SyncMode::OverwriteLocalRemote,
        AddLocation::Top,
    )
    .unwrap();
    assert_eq!(ordinary.data.user_list.len(), 2);
    let full = first_sync_lists(
        &local,
        &remote,
        SyncMode::OverwriteLocalRemoteFull,
        AddLocation::Top,
    )
    .unwrap();
    assert_eq!(full.data.user_list.len(), 1);
    assert!(!full.update_local && full.update_remote);
}
#[test]
fn three_way_deletions_win_without_erasing_new_songs_and_lists() {
    let mut base = data(&["a", "b"]);
    base.user_list.push(user("old", "Old", None, &["a"]));
    let mut local = data(&["a", "b", "local-new"]);
    local.user_list.push(user("old", "Old", None, &["a"]));
    local.user_list.push(user("new-local", "New", None, &[]));
    let mut remote = data(&["a", "remote-new"]);
    remote.user_list.push(user("new-remote", "New", None, &[]));
    let merged = merge_lists_three_way(&local, &remote, &base, AddLocation::Bottom).unwrap();
    assert_eq!(song_ids(&merged), vec!["a", "local-new", "remote-new"]);
    assert_eq!(
        merged
            .user_list
            .iter()
            .map(|list| list.id.as_str())
            .collect::<Vec<_>>(),
        vec!["new-local", "new-remote"]
    );
}
#[test]
fn three_way_attribute_conflicts_and_actual_location_timestamp_quirks() {
    let mut base = data(&[]);
    base.user_list = vec![
        user("a", "Base", Some(10), &[]),
        user("b", "B", Some(10), &[]),
    ];
    let mut local = base.clone();
    local.user_list[0].name = json!("Local");
    let mut remote = base.clone();
    remote.user_list[0].name = json!("Remote");
    remote.user_list[0].location_update_time = Some(json!(30));
    remote.user_list.swap(0, 1);
    let merged = merge_lists_three_way(&local, &remote, &base, AddLocation::Top).unwrap();
    assert_eq!(merged.user_list[1].id, "a");
    assert_eq!(merged.user_list[1].name, json!("Local"));
    assert_eq!(
        merged.user_list[1]
            .location_update_time
            .as_ref()
            .and_then(Value::as_f64),
        Some(10.0)
    );
    local.user_list[0].name = json!("Base");
    let merged = merge_lists_three_way(&local, &remote, &base, AddLocation::Top).unwrap();
    assert_eq!(merged.user_list[1].name, json!("Remote"));
    remote.user_list[1].location_update_time = Some(json!(5));
    let first = first_sync_lists(
        &local,
        &remote,
        SyncMode::MergeLocalRemote,
        AddLocation::Top,
    )
    .unwrap();
    assert_eq!(first.data.user_list[1].id, "a");
    assert_eq!(
        first.data.user_list[1].location_update_time,
        Some(json!(5.0))
    );
}
#[test]
fn dislike_modes_trim_unicode_dedup_three_way_and_exact_digest() {
    assert_eq!(
        first_sync_dislike("Song@Singer", "Other", SyncMode::MergeLocalRemote)
            .unwrap()
            .data,
        "song@singer\nother"
    );
    for mode in [
        SyncMode::MergeRemoteLocal,
        SyncMode::OverwriteLocalRemote,
        SyncMode::OverwriteRemoteLocal,
    ] {
        assert!(first_sync_dislike("a", "b", mode).is_ok());
    }
    assert!(first_sync_dislike("a", "b", SyncMode::Cancel).is_err());
    assert!(first_sync_dislike("a", "b", SyncMode::OverwriteLocalRemoteFull).is_err());
    assert_eq!(
        merge_dislike_three_way("a\nb\nc", "a\nd", "a\nb").unwrap(),
        "a\nc\nd"
    );
    assert_eq!(
        first_sync_dislike("ΟΣ@İ", "x", SyncMode::MergeLocalRemote)
            .unwrap()
            .data,
        "ος@i\u{307}\nx"
    );
    assert_eq!(
        dislike_digest("\u{feff} a \u{a0}").unwrap(),
        "0cc175b9c0f1b6a831c399e269772661"
    );
}
#[test]
fn aes_auth_pairing_key_is_md5_hex_ascii_and_actual_pkcs7_roundtrips() {
    let key = code_key("123456").unwrap();
    assert_eq!(key.expose_base64(), "ZTEwYWRjMzk0OWJhNTlhYg==");
    assert!(!format!("{key:?}").contains(&key.expose_base64()));
    for plaintext in [
        b"".as_slice(),
        b"lx-music connect",
        b"0123456789abcdef",
        "设备\nUTF8".as_bytes(),
    ] {
        let ciphertext = aes_encrypt(plaintext, &key).unwrap();
        assert_eq!(aes_decrypt(&ciphertext, &key).unwrap(), plaintext);
    }
    let ciphertext = aes_encrypt(b"lx-music connect", &key).unwrap();
    assert_eq!(ciphertext, "OvX/VAIBCxyDPnHYidKNClrzvTbSVMKgGkP8aDfGQKQ=");
    assert!(aes_decrypt(&ciphertext, &SharedKey::from_bytes([0; 16])).is_err());
    assert!(SharedKey::from_base64("AAAA").is_err());
    assert!(SharedKey::from_base64(&"A".repeat(100000)).is_err());
    assert_eq!(
        SharedKey::from_base64("AAECAwQFBgcICQoLDA0ODw")
            .unwrap()
            .expose_base64(),
        "AAECAwQFBgcICQoLDA0ODw=="
    );
    assert_eq!(
        SharedKey::from_base64(" AAECAwQFBgcICQoLDA0ODw==\n")
            .unwrap()
            .expose_base64(),
        "AAECAwQFBgcICQoLDA0ODw=="
    );
    assert_eq!(
        SharedKey::from_base64("_____________________w")
            .unwrap()
            .expose_base64(),
        "/////////////////////w=="
    );
    assert!(aes_decrypt("bad-ciphertext", &key).is_err());
}
#[test]
fn actual_rsa_2048_oaep_sha1_auth_roundtrip_and_wrong_key_rejection() {
    let key = generate_pairing_key_pair().unwrap();
    assert!(key.public_pem.starts_with("-----BEGIN PUBLIC KEY-----"));
    assert!(key.private_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    assert!(!format!("{key:?}").contains(&key.private_pem));
    let payload =
        br#"{"clientId":"synthetic","key":"AAECAwQFBgcICQoLDA0ODw==","serverName":"Fixture"}"#;
    let encrypted = rsa_encrypt(payload, &key.public_pem).unwrap();
    assert_eq!(rsa_decrypt(&encrypted, &key.private_pem).unwrap(), payload);
    assert!(rsa_encrypt(&vec![0; 215], &key.public_pem).is_err());
    assert!(rsa_decrypt("AA==", &key.private_pem).is_err());
}
