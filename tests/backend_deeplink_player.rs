#[path = "support/library_fixture.rs"]
mod fixture;
use fixture::Running;
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{BackendCommand, BackendQuery, BackendQueryResult},
    open_api::{ServerConfig, ServerState},
};
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

#[test]
fn actual_player_deeplinks_pause_resume_collect_and_keep_the_http_session_active() {
    let running = Running::new();
    let path = running.folder("deeplink-selected").join("original.wav");
    fixture::wave(&path, 440.0, 12.0);
    running.import_explicit("deeplink-import", vec![path]);
    let page = running.backend.tracks("original", 0, 10).unwrap();
    let track = page.items.first().unwrap();
    let before = running.play("deeplink-play-local", &track.id, 440.0);
    let mut server = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while server.view().state != ServerState::Listening {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let epoch = server.epoch();
    let address = server.view().address.unwrap();
    running.execute(
        "deeplink-pause",
        BackendCommand::DispatchDeepLink {
            url: "lxmusic://player/pause".into(),
        },
    );
    let paused = fixture::wait(&running.backend, 3.0, |snapshot| {
        snapshot.playback.status == PlaybackStatus::Paused
    });
    assert_eq!(paused.playback.generation, before.playback.generation);
    running.execute(
        "deeplink-resume",
        BackendCommand::DispatchDeepLink {
            url: "lxmusic://player/play".into(),
        },
    );
    let resumed = fixture::wait(&running.backend, 3.0, |snapshot| {
        snapshot.playback.status == PlaybackStatus::Playing
    });
    assert_eq!(resumed.playback.generation, before.playback.generation);
    for id in ["deeplink-collect", "deeplink-collect-again"] {
        running.execute(
            id,
            BackendCommand::DispatchDeepLink {
                url: "lxmusic://player/collect".into(),
            },
        );
    }
    let BackendQueryResult::ListTracks(love) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "love".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("Love")
    };
    assert_eq!(love.items.len(), 1);
    assert_eq!(love.items[0].id, track.id);
    assert_eq!(server.epoch(), epoch);
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        socket,
        "GET /status?filter=collect HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(response.split_once("\r\n\r\n").unwrap().1)
            .unwrap()["collect"],
        true
    );
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
}

#[test]
fn remote_music_deeplink_imports_only_metadata_without_file_or_source_approval() {
    let running = Running::new();
    let media = running.folder("deeplink-private").join("unselected.wav");
    fixture::wave(&media, 550.0, 1.0);
    let payload = serde_json::json!({"source":"kw","songmid":"metadata-only","name":"深链曲目","singer":"作者",
        "albumName":"专辑","interval":"00:01","img":"https://example.invalid/image.jpg","filePath":media.to_str().unwrap(),
        "approvedUrl":"https://example.invalid/audio.mp3","sourceApproval":true,"types":[{"type":"128k"}],"_types":{"128k":{}}});
    let mut link = reqwest::Url::parse("lxmusic://music/play").unwrap();
    link.query_pairs_mut()
        .append_pair("data", &payload.to_string());
    let _ = running.backend.execute(fixture::envelope(
        "deeplink-remote",
        BackendCommand::DispatchDeepLink {
            url: link.to_string(),
        },
    ));
    let BackendQueryResult::ListTracks(list) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "temp".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("Temp")
    };
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].title, "深链曲目");
    assert!(list.items[0].local_asset_id.is_none());
    assert!(!list.items[0].metadata.contains_key("filePath"));
    assert!(!list.items[0].metadata.contains_key("approvedUrl"));
    assert!(running.backend.snapshot().sources.is_empty());
    assert!(running.backend.snapshot().pending_playback_url.is_none());
    let BackendQueryResult::LibraryAssets(assets) = running
        .backend
        .query(BackendQuery::LibraryAssets {
            root_id: None,
            after_asset_id: None,
            limit: 10,
            expected_revision: None,
        })
        .unwrap()
    else {
        panic!("Assets")
    };
    assert!(assets.items.is_empty());
}
