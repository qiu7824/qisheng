#[path = "support/library_fixture.rs"]
mod fixture;
use fixture::{Running, assets, wait, wave};
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{BackendCommand, BackendQuery, BackendQueryResult},
    open_api::{ControlAction, ExternalControlBackend, ServerConfig, ServerState},
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    time::Duration,
};

fn request(address: SocketAddr, target: &str) -> String {
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    write!(socket, "GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    response
}
fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}
fn ok(address: SocketAddr, target: &str) {
    let response = request(address, target);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[test]
fn actual_http_control_uses_current_actor_music_and_preserves_paused_play_identity() {
    let running = Running::new();
    let path = running.folder("api-selected").join("real-997.wav");
    wave(&path, 997.0, 36.0);
    running.import_explicit("api-import", vec![path]);
    let track = assets(&running.backend, None).pop().unwrap().asset.track_id;
    let playing = running.play("api-play", &track, 997.0);
    let mut server = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    wait(&running.backend, 3.0, |_| {
        server.view().state == ServerState::Listening
    });
    let address = server.view().address.unwrap();
    ok(address, "/pause");
    wait(&running.backend, 5.0, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    ok(address, "/play");
    let resumed = wait(&running.backend, 5.0, |state| {
        state.playback.status == PlaybackStatus::Playing
    });
    assert_eq!(resumed.playback.generation, playing.playback.generation);
    ok(address, "/pause");
    ok(address, "/seek?offset=4.23456");
    wait(&running.backend, 5.0, |state| {
        (state.playback.position_seconds - 4.235).abs() < 0.03
    });
    assert!(request(address, "/seek?offset=36.0001").starts_with("HTTP/1.1 400"));
    ok(address, "/volume?volume=29");
    ok(address, "/mute?mute=true");
    let response = request(address, "/status?filter=volume,mute,status,name,progress");
    let status: serde_json::Value = serde_json::from_str(body(&response)).unwrap();
    assert_eq!(status["volume"], 28);
    assert_eq!(status["mute"], true);
    assert_eq!(status["status"], "paused");
    assert_eq!(status["name"], "real-997");
    ok(address, "/collect?collect=true");
    ok(address, "/collect?collect=true");
    let BackendQueryResult::ListTracks(rows) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "love".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("Expected love list");
    };
    assert_eq!(rows.items.len(), 1);
    assert_eq!(rows.items[0].id, track);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body(&request(
            address,
            "/status?filter=collect"
        )))
        .unwrap()["collect"],
        true
    );
    ok(address, "/uncollect");
    running.execute("api-stop-after-http", BackendCommand::Stop);
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
    assert_eq!(server.view().connections, 0);
    let listener = TcpListener::bind(address).unwrap();
    drop(listener);
}

#[test]
fn retired_and_replaced_actor_sessions_cannot_mutate_player_or_saved_settings() {
    let running = Running::new();
    let old = running.backend.external_control_session().unwrap();
    let epoch = old.epoch();
    old.control(epoch, ControlAction::SetVolume { percent: 58 })
        .unwrap();
    assert_eq!(old.snapshot().unwrap().status.volume, 57);
    assert!(running.backend.external_control_session().is_err());
    old.retire();
    let current = running.backend.external_control_session().unwrap();
    assert_ne!(current.epoch(), epoch);
    assert!(
        old.control(epoch, ControlAction::SetVolume { percent: 1 })
            .is_err()
    );
    assert_eq!(current.snapshot().unwrap().status.volume, 57);
    assert!(
        current
            .control(epoch, ControlAction::SetVolume { percent: 1 })
            .is_err()
    );
    current.retire();
    assert!(
        current
            .control(current.epoch(), ControlAction::ToggleMute)
            .is_err()
    );
    assert!(!running.backend.snapshot().audio_output.preferences.muted);
}

#[test]
fn current_collection_and_status_cursor_are_coherent_across_real_track_changes() {
    let running = Running::new();
    let session = running.backend.external_control_session().unwrap();
    let before = session.snapshot().unwrap();
    assert!(!before.status.collect);
    session
        .control(
            session.epoch(),
            ControlAction::CollectCurrent { collected: true },
        )
        .unwrap();
    running.execute(
        "api-current-tone",
        BackendCommand::Play {
            id: "tone-1".into(),
        },
    );
    wait(&running.backend, 5.0, |state| {
        state.playback.status == PlaybackStatus::Playing
    });
    session
        .control(
            session.epoch(),
            ControlAction::CollectCurrent { collected: true },
        )
        .unwrap();
    let collected = session.snapshot().unwrap();
    assert!(collected.status.collect);
    running.execute(
        "api-change-tone",
        BackendCommand::Play {
            id: "tone-2".into(),
        },
    );
    let changed = session.snapshot().unwrap();
    assert_eq!(changed.status.name, "Violet Hour");
    assert!(!changed.status.collect);
    let updates = session.updates_after(collected.cursor).unwrap();
    assert_eq!(updates.last().unwrap().status.name, "Violet Hour");
    assert!(session.updates_after(u64::MAX).is_err());
    session
        .control(
            session.epoch(),
            ControlAction::CollectCurrent { collected: true },
        )
        .unwrap();
    let BackendQueryResult::ListTracks(rows) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "love".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("Expected love list");
    };
    assert_eq!(rows.items[0].id, "tone-2");
    assert_eq!(rows.items[1].id, "tone-1");
}

#[test]
fn actual_sse_reports_same_generation_effective_lyrics_and_closes_on_retirement() {
    let running = Running::new();
    running.execute("api-lyric-save", BackendCommand::SaveLyrics { id: "tone-1".into(),
        bundle: serde_json::from_value(serde_json::json!({"lyric":"[00:00.00]原创行\n[00:10.00]后续行", "tlyric":"[00:00.00]translation", "rlyric":"[00:00.00]yuan chuang"})).unwrap() });
    running.play("api-lyric-play", "tone-1", 261.63);
    wait(&running.backend, 5.0, |state| {
        state.lyrics.is_some() && state.lyric_frame.is_some()
    });
    let mut server = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    wait(&running.backend, 3.0, |_| {
        server.view().state == ServerState::Listening
    });
    let address = server.view().address.unwrap();
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(socket, "GET /subscribe-player-status?filter=name,status,lyric,lyricLineText,lyricLineAllText HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"));
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    let mut data = String::new();
    for _ in 0..5 {
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\n" {
                break;
            }
            data.push_str(&line);
        }
    }
    assert!(data.contains("原创行"));
    assert!(data.contains("First Light"));
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).unwrap();
    assert_eq!(server.view().subscriptions, 0);
}

#[test]
fn external_play_cannot_approve_or_fetch_a_source_returned_audio_url() {
    use lx_gpui_player::domain::{
        lists::{AddLocation, ListAction},
        music::{LxMusicDtoVersion, TrackRecord},
    };
    let running = Running::new();
    let forbidden_audio = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    forbidden_audio.set_nonblocking(true).unwrap();
    let url = format!(
        "http://{}/must-remain-unapproved.wav",
        forbidden_audio.local_addr().unwrap()
    );
    let script = format!(
        r#"/** @name Original API consent fixture */
lx.on('request',async({{action}})=>{{if(action!=='musicUrl')throw Error('unexpected');return {};}});
lx.send('inited',{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});"#,
        serde_json::to_string(&url).unwrap()
    );
    let source_id = lx_gpui_player::source::script_hash(&script);
    for (id, command) in [
        (
            "api-source-import",
            BackendCommand::ImportSourceText { script },
        ),
        (
            "api-source-approved",
            BackendCommand::ApproveSource {
                id: source_id.clone(),
            },
        ),
        (
            "api-source-enabled",
            BackendCommand::EnableSource { id: source_id },
        ),
    ] {
        let cursor = running.backend.event_cursor();
        running.execute(id, command);
        assert!(fixture::operation_finished(&running.backend, cursor, id).0);
    }
    let track = TrackRecord::from_lx_dto(serde_json::json!({"source":"kw","songmid":"unapproved-api-song",
        "name":"Unapproved audio","singer":"Original artist","albumName":"Original album","interval":"00:36",
        "types":[{"type":"128k"}],"_types":{"128k":{}}}), LxMusicDtoVersion::Legacy).unwrap();
    running.execute(
        "api-remote-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![track.clone()],
                location: AddLocation::Top,
            }),
        },
    );
    running.execute(
        "api-remote-resolve",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: track.id,
            quality: Some("128k".into()),
        },
    );
    wait(&running.backend, 10.0, |state| {
        state.pending_playback_url.as_deref() == Some(url.as_str())
    });
    let mut server = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    wait(&running.backend, 3.0, |_| {
        server.view().state == ServerState::Listening
    });
    ok(server.view().address.unwrap(), "/play");
    let state = running.backend.snapshot();
    assert_eq!(state.pending_playback_url.as_deref(), Some(url.as_str()));
    assert_ne!(state.playback.status, PlaybackStatus::Playing);
    assert!(forbidden_audio.accept().is_err());
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
}

#[test]
fn backend_shutdown_retires_endpoint_and_joins_its_real_workers_before_profile_release() {
    let running = Running::new();
    let server = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    wait(&running.backend, 3.0, |_| {
        server.view().state == ServerState::Listening
    });
    let address = server.view().address.unwrap();
    let _socket = TcpStream::connect(address).unwrap();
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(server.view().state, ServerState::Disabled);
    assert_eq!(server.view().connections, 0);
    let listener = TcpListener::bind(address).unwrap();
    drop(listener);
    let repository = lx_gpui_player::repository::Repository::open(running.profile()).unwrap();
    repository.integrity_check().unwrap();
}

fn import_artwork_track(running: &Running) -> (String, Vec<u8>) {
    use image::{ExtendedColorType, ImageEncoder};
    use lofty::{
        config::WriteOptions,
        picture::{MimeType, Picture, PictureType},
        tag::{Tag, TagExt, TagType},
    };
    let path = running.folder("api-artwork").join("approved-embedded.wav");
    wave(&path, 440.0, 36.0);
    let mut picture = Vec::new();
    image::codecs::png::PngEncoder::new(&mut picture)
        .write_image(
            &[
                255, 0, 40, 255, 10, 240, 20, 255, 20, 40, 240, 255, 250, 220, 40, 255,
            ],
            2,
            2,
            ExtendedColorType::Rgba8,
        )
        .unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.push_picture(
        Picture::unchecked(picture.clone())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    tag.save_to_path(&path, WriteOptions::default()).unwrap();
    running.import_explicit("api-artwork-import", vec![path]);
    let track = assets(&running.backend, None).pop().unwrap().asset.track_id;
    (track, picture)
}
fn resolve_artwork(
    running: &Running,
    track: &str,
    request: &str,
) -> lx_gpui_player::domain::artwork::ArtworkInfo {
    use lx_gpui_player::domain::artwork::PictureState;
    running.execute(
        request,
        BackendCommand::ResolvePicture {
            list_id: "catalog".into(),
            music_id: track.into(),
        },
    );
    let mut actual = None;
    wait(&running.backend, 10.0, |_| {
        let BackendQueryResult::PictureOperations(views) = running
            .backend
            .query(BackendQuery::PictureOperations {
                offset: 0,
                limit: 100,
            })
            .unwrap()
        else {
            panic!("Expected picture operations");
        };
        if let Some(view) = views
            .into_iter()
            .find(|view| view.operation_id == request && view.state == PictureState::Ready)
        {
            actual = view.info;
        }
        actual.is_some()
    });
    actual.unwrap()
}
fn current_picture(address: SocketAddr) -> String {
    serde_json::from_str::<serde_json::Value>(body(&request(address, "/status?filter=picUrl")))
        .unwrap()["picUrl"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn backend_image_request(address: SocketAddr, method: &str, target: &str) -> (String, Vec<u8>) {
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    write!(
        socket,
        "{method} {target} HTTP/1.1\r\nHost: {address}\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).unwrap();
    let end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    (
        String::from_utf8(response[..end].to_vec()).unwrap(),
        response[end..].to_vec(),
    )
}
fn picture_reference(path: &str) -> lx_gpui_player::open_api::ArtworkReference {
    let lx_gpui_player::open_api::Route::Artwork(reference) =
        lx_gpui_player::open_api::parse_route(path).unwrap()
    else {
        panic!("Expected artwork bridge path");
    };
    reference
}

#[test]
fn actual_backend_http_and_sse_publish_only_verified_current_artwork_and_revoke_previous_scopes() {
    let running = Running::new();
    let (track, picture) = import_artwork_track(&running);
    running.play("api-artwork-play", &track, 440.0);
    let mut server = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    wait(&running.backend, 3.0, |_| {
        server.view().state == ServerState::Listening
    });
    let address = server.view().address.unwrap();
    assert!(current_picture(address).is_empty());
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        socket,
        "GET /subscribe-player-status?filter=picUrl HTTP/1.1\r\nHost: {address}\r\n\r\n"
    )
    .unwrap();
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"));
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    let read_frame = |reader: &mut BufReader<TcpStream>| {
        let mut result = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            result.push_str(&line);
            if line == "\n" {
                return result;
            }
        }
    };
    assert_eq!(read_frame(&mut reader), "event: picUrl\ndata: \"\"\n\n");
    resolve_artwork(&running, &track, "api-artwork-resolve");
    let frame = read_frame(&mut reader);
    let url = current_picture(address);
    assert!(url.starts_with("/artwork/v1/"));
    assert_eq!(
        frame,
        format!(
            "event: picUrl\ndata: {}\n\n",
            serde_json::to_string(&url).unwrap()
        )
    );
    assert!(!url.contains("approved-embedded"));
    let (header, bytes) = backend_image_request(address, "GET", &url);
    assert!(header.starts_with("HTTP/1.1 200"));
    assert_eq!(bytes, picture);
    let (head, bytes) = backend_image_request(address, "HEAD", &url);
    assert!(head.contains(&format!("Content-Length: {}\r\n", picture.len())));
    assert!(bytes.is_empty());
    running.play("api-artwork-other", "tone-2", 220.0);
    assert!(current_picture(address).is_empty());
    assert!(
        backend_image_request(address, "GET", &url)
            .0
            .starts_with("HTTP/1.1 503")
    );
    running.play("api-artwork-replay", &track, 440.0);
    let replay = current_picture(address);
    assert_ne!(replay, url);
    assert_ne!(
        picture_reference(&replay).generation,
        picture_reference(&url).generation
    );
    running.execute("api-artwork-clear", BackendCommand::ClearImageCache);
    assert!(current_picture(address).is_empty());
    assert!(
        backend_image_request(address, "GET", &replay)
            .0
            .starts_with("HTTP/1.1 503")
    );
    resolve_artwork(&running, &track, "api-artwork-after-clear");
    let after_clear = current_picture(address);
    assert_ne!(after_clear, replay);
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).unwrap();
    let mut replacement = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    wait(&running.backend, 3.0, |_| {
        replacement.view().state == ServerState::Listening
    });
    let new_address = replacement.view().address.unwrap();
    assert!(
        backend_image_request(new_address, "GET", &after_clear)
            .0
            .starts_with("HTTP/1.1 401")
    );
    let fresh = current_picture(new_address);
    assert_ne!(fresh, after_clear);
    assert_eq!(backend_image_request(new_address, "GET", &fresh).1, picture);
    assert!(replacement.shutdown(Duration::from_secs(5)).unwrap());
}

#[test]
fn actual_actor_artwork_leases_retire_on_cache_clear_ttl_expiry_and_endpoint_retirement() {
    use std::sync::atomic::Ordering;
    let running = Running::new();
    let (track, picture) = import_artwork_track(&running);
    running.play("api-lease-play", &track, 440.0);
    resolve_artwork(&running, &track, "api-lease-resolve");
    let session = running.backend.external_control_session().unwrap();
    let first = session.snapshot().unwrap().status.pic_url;
    let lease = session.artwork(picture_reference(&first)).unwrap();
    assert_eq!(lease.bytes.as_ref(), picture.as_slice());
    running.execute("api-lease-cache-clear", BackendCommand::ClearImageCache);
    assert!(session.snapshot().unwrap().status.pic_url.is_empty());
    assert!(lease.retired.load(Ordering::Acquire));
    assert!(session.artwork(picture_reference(&first)).is_err());
    let mut info = resolve_artwork(&running, &track, "api-lease-resolve-again");
    let current = session.snapshot().unwrap().status.pic_url;
    let current_lease = session.artwork(picture_reference(&current)).unwrap();
    info.resolved_at_ms = 0;
    info.expire_at_ms = 1;
    info.validate().unwrap();
    let document = serde_json::to_string(&info).unwrap();
    let connection = rusqlite::Connection::open(
        running
            .profile()
            .join(lx_gpui_player::repository::DATABASE_NAME),
    )
    .unwrap();
    connection.execute("UPDATE artwork_info SET resolved_at_ms=0,expire_at_ms=1,last_access_ms=0,byte_size=?1,document=?2 WHERE cache_key=?3",
        rusqlite::params![document.len() as i64, document, info.key.fingerprint().unwrap()]).unwrap();
    drop(connection);
    assert!(session.artwork(picture_reference(&current)).is_err());
    assert!(current_lease.retired.load(Ordering::Acquire));
    assert!(session.snapshot().unwrap().status.pic_url.is_empty());
    resolve_artwork(&running, &track, "api-lease-post-expiry");
    let final_url = session.snapshot().unwrap().status.pic_url;
    let final_lease = session.artwork(picture_reference(&final_url)).unwrap();
    session.retire();
    assert!(final_lease.retired.load(Ordering::Acquire));
    assert!(session.artwork(picture_reference(&final_url)).is_err());
}

#[test]
fn actual_open_api_cannot_fetch_or_publish_a_source_picture_awaiting_url_approval() {
    use lx_gpui_player::domain::artwork::PictureState;
    let running = Running::new();
    let (track, _) = import_artwork_track(&running);
    running.play("api-picture-consent-play", &track, 440.0);
    resolve_artwork(&running, &track, "api-picture-consent-embedded");
    let mut endpoint = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    wait(&running.backend, 3.0, |_| {
        endpoint.view().state == ServerState::Listening
    });
    let address = endpoint.view().address.unwrap();
    let approved_embedded = current_picture(address);
    assert!(!approved_embedded.is_empty());
    let forbidden_image = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    forbidden_image.set_nonblocking(true).unwrap();
    let url = format!(
        "http://{}/must-remain-unapproved.png?private=source-signature",
        forbidden_image.local_addr().unwrap()
    );
    let script = format!(
        r#"/** @name Actual API artwork consent fixture */
lx.on('request',async({{action}})=>{{if(action!=='pic')throw Error('unexpected action');return {};}});
lx.send('inited',{{sources:{{local:{{name:'Local picture fixture',type:'music',actions:['pic','musicUrl'],qualitys:[]}}}}}});"#,
        serde_json::to_string(&url).unwrap()
    );
    let source = lx_gpui_player::source::script_hash(&script);
    for (id, command) in [
        (
            "api-picture-source-import",
            BackendCommand::ImportSourceText { script },
        ),
        (
            "api-picture-source-approve",
            BackendCommand::ApproveSource { id: source.clone() },
        ),
        (
            "api-picture-source-enable",
            BackendCommand::EnableSource { id: source },
        ),
    ] {
        let cursor = running.backend.event_cursor();
        running.execute(id, command);
        assert!(fixture::operation_finished(&running.backend, cursor, id).0);
    }
    assert!(current_picture(address).is_empty());
    assert!(
        backend_image_request(address, "GET", &approved_embedded)
            .0
            .starts_with("HTTP/1.1 503")
    );
    running.execute(
        "api-picture-unapproved-resolve",
        BackendCommand::ResolvePicture {
            list_id: "catalog".into(),
            music_id: track,
        },
    );
    wait(&running.backend, 10.0, |_| {
        let BackendQueryResult::PictureOperations(views) = running
            .backend
            .query(BackendQuery::PictureOperations {
                offset: 0,
                limit: 100,
            })
            .unwrap()
        else {
            panic!("Expected picture operations");
        };
        views.iter().any(|view| {
            view.operation_id == "api-picture-unapproved-resolve"
                && view.state == PictureState::AwaitingApproval
        })
    });
    let status = request(address, "/status?filter=picUrl,name");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body(&status)).unwrap()["picUrl"],
        ""
    );
    assert!(!status.contains(&url));
    assert!(!status.contains("source-signature"));
    assert!(request(address, "/approve-picture-url?url=anything").starts_with("HTTP/1.1 401"));
    assert!(forbidden_image.accept().is_err());
    assert!(endpoint.shutdown(Duration::from_secs(5)).unwrap());
}
