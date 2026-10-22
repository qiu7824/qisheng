#![allow(dead_code)]

#[path = "library_fixture.rs"]
mod base;

#[allow(unused_imports)]
pub use base::{
    Running, assets, cache_epoch, corpus, envelope, library_operation, library_terminal,
    operation_finished, pcm, root, wait, wave,
};
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendEvent, BackendHandle, BackendQuery, BackendQueryResult,
        BackendSnapshot, ErrorKind, ReceiptState,
    },
    domain::{
        library::{LibraryAssetView, LibraryRoot},
        library_watch::{
            LibraryAssetBinding, LibraryRelinkResult, LibraryRootBinding, LibraryWatchOptions,
            LibraryWatchState, LibraryWatchView,
        },
    },
    source::SourceHostEvent,
};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub fn watch(backend: &BackendHandle, root_id: &str) -> Option<LibraryWatchView> {
    match backend
        .query(BackendQuery::LibraryWatch {
            root_id: root_id.into(),
        })
        .unwrap()
    {
        BackendQueryResult::LibraryWatch(view) => view,
        other => panic!("unexpected watch result: {other:?}"),
    }
}

pub fn binding(backend: &BackendHandle, asset_id: &str) -> LibraryAssetBinding {
    match backend
        .query(BackendQuery::LibraryAssetBinding {
            asset_id: asset_id.into(),
        })
        .unwrap()
    {
        BackendQueryResult::LibraryAssetBinding(Some(binding)) => binding,
        other => panic!("missing actual asset binding: {other:?}"),
    }
}

pub fn root_binding(backend: &BackendHandle, root_id: &str) -> LibraryRootBinding {
    match backend
        .query(BackendQuery::LibraryRootBinding {
            root_id: root_id.into(),
        })
        .unwrap()
    {
        BackendQueryResult::LibraryRootBinding(Some(binding)) => binding,
        other => panic!("missing actual root binding: {other:?}"),
    }
}

pub fn options(auto_relink: bool) -> LibraryWatchOptions {
    LibraryWatchOptions {
        debounce_ms: 50,
        max_wait_ms: 300,
        auto_relink,
        ..Default::default()
    }
}

pub fn set_watch(
    running: &Running,
    request: &str,
    selected: &LibraryRoot,
    enabled: bool,
    options: LibraryWatchOptions,
) {
    let cursor = running.backend.event_cursor();
    let receipt = running
        .backend
        .execute(envelope(
            request,
            BackendCommand::SetLibraryWatch {
                root_id: selected.root_id.clone(),
                enabled,
                options,
            },
        ))
        .unwrap();
    if receipt.state == ReceiptState::Accepted {
        assert_eq!(
            operation_finished(&running.backend, cursor, request),
            (true, None)
        );
    }
    wait(&running.backend, 20.0, |_| {
        watch(&running.backend, &selected.root_id).is_some_and(|view| {
            view.preference.enabled == enabled
                && if enabled {
                    view.state == LibraryWatchState::Watching
                        && view.last_scan_operation_id.is_some()
                        && view.covered_sequence >= view.received_sequence
                        && !view.full_rescan_required
                } else {
                    view.state == LibraryWatchState::Disabled
                }
        })
    });
}

pub fn settled(
    running: &Running,
    selected: &LibraryRoot,
    condition: impl Fn(&[LibraryAssetView]) -> bool,
) -> LibraryWatchView {
    wait(&running.backend, 30.0, |_| {
        let Some(view) = watch(&running.backend, &selected.root_id) else {
            return false;
        };
        view.state == LibraryWatchState::Watching
            && view.covered_sequence >= view.received_sequence
            && !view.full_rescan_required
            && condition(&assets(&running.backend, None))
    });
    watch(&running.backend, &selected.root_id).unwrap()
}

pub fn named(backend: &BackendHandle, filename: &str) -> LibraryAssetView {
    let mut rows = assets(backend, None).into_iter().filter(|item| {
        item.asset.path.file_name().and_then(|name| name.to_str()) == Some(filename)
    });
    let row = rows
        .next()
        .unwrap_or_else(|| panic!("actual file {filename} was not indexed"));
    assert!(
        rows.next().is_none(),
        "duplicate actual path filename {filename}"
    );
    row
}

pub fn file_indexed(rows: &[LibraryAssetView], path: &Path) -> bool {
    rows.iter().any(|item| {
        item.asset.path.file_name() == path.file_name() && item.asset.available && item.authorized
    })
}

pub fn content_evidence(running: &Running, asset_id: &str) -> LibraryAssetBinding {
    wait(&running.backend, 30.0, |_| {
        let value = binding(&running.backend, asset_id);
        value.file_identity.is_some()
            && value
                .content_sha256
                .as_ref()
                .is_some_and(|hash| hash.len() == 64)
    });
    binding(&running.backend, asset_id)
}

pub fn relink(
    running: &Running,
    request: &str,
    asset_id: &str,
    path: PathBuf,
    revision: u64,
    allow_content_change: bool,
) -> LibraryRelinkResult {
    let cursor = running.backend.event_cursor();
    let receipt = running
        .backend
        .execute(envelope(
            request,
            BackendCommand::RelinkLocalAsset {
                asset_id: asset_id.into(),
                path,
                expected_binding_revision: revision,
                allow_content_change,
            },
        ))
        .unwrap();
    if receipt.state == ReceiptState::Accepted {
        assert_eq!(
            operation_finished(&running.backend, cursor, request),
            (true, None)
        );
    }
    let mut result = None;
    wait(&running.backend, 30.0, |_| {
        result = match running
            .backend
            .query(BackendQuery::LibraryRelinkResult {
                operation_id: request.into(),
            })
            .unwrap()
        {
            BackendQueryResult::LibraryRelinkResult(result) => result,
            other => panic!("unexpected relink result: {other:?}"),
        };
        result.is_some()
    });
    result.unwrap()
}

pub fn relink_denied(
    running: &Running,
    request: &str,
    asset_id: &str,
    path: PathBuf,
    revision: u64,
) -> ErrorKind {
    let cursor = running.backend.event_cursor();
    match running.backend.execute(envelope(
        request,
        BackendCommand::RelinkLocalAsset {
            asset_id: asset_id.into(),
            path,
            expected_binding_revision: revision,
            allow_content_change: false,
        },
    )) {
        Err(error) => error.kind,
        Ok(receipt) => {
            assert_eq!(
                receipt.state,
                ReceiptState::Accepted,
                "failed relink was committed"
            );
            let (success, error) = operation_finished(&running.backend, cursor, request);
            assert!(
                !success,
                "unapproved representation/collision relink succeeded"
            );
            error.unwrap()
        }
    }
}

pub fn actual_frequency(backend: &BackendHandle, hz: f64) {
    let mut seen = 0;
    let mut last = 0;
    wait(backend, 12.0, |_| {
        let observed = pcm(backend);
        if observed.batches != last {
            last = observed.batches;
            if observed.peak > 0.002
                && observed
                    .frequency_hz
                    .is_some_and(|measured| (measured - hz).abs() <= 8.0)
            {
                seen += 1;
            } else {
                seen = 0;
            }
        }
        seen >= 5
    });
}

pub fn media_continues(before: &BackendSnapshot, after: &BackendSnapshot) {
    assert_eq!(after.playback.track_id, before.playback.track_id);
    assert_eq!(after.playback.generation, before.playback.generation);
    assert_eq!(after.playback.seek_seq, before.playback.seek_seq);
    assert_eq!(
        after.playback.duration_seconds,
        before.playback.duration_seconds
    );
    assert_eq!(after.pending_playback_url, before.pending_playback_url);
    assert_eq!(
        serde_json::to_value(&after.approved_audio_identity).unwrap(),
        serde_json::to_value(&before.approved_audio_identity).unwrap()
    );
    assert_eq!(
        after.playback.status,
        PlaybackStatus::Playing,
        "Playback continuity: before={before:?}, after={after:?}"
    );
    assert!(after.playback.position_seconds >= before.playback.position_seconds);
}

pub fn play_denied(running: &Running, request: &str, track_id: &str) {
    if running
        .backend
        .execute(envelope(
            request,
            BackendCommand::Play {
                id: track_id.into(),
            },
        ))
        .is_ok()
    {
        let state = wait(&running.backend, 10.0, |state| !state.busy);
        assert!(state.error.is_some());
        assert!(
            state.playback.track_id.as_deref() != Some(track_id)
                || state.playback.status != PlaybackStatus::Playing
        );
    }
}

pub fn capture_native_url(running: &Running, request: &str, track_id: &str, hz: f64) -> String {
    let script = r#"/** @name Original watch authority observer */
let alerted=false;
lx.on('request',async({action,info})=>{
  if(action!=='musicUrl')throw Error('unsupported fixture action');
  const url=info.musicInfo.filePath;
  if(typeof url!=='string'||!url.startsWith('http://127.0.0.1:'))throw Error('expected actual loopback');
  if(!alerted){lx.send('updateAlert',{log:'watch-actual-url='+url});alerted=true;}
  return url;
});
lx.send('inited',{sources:{local:{type:'music',actions:['musicUrl'],qualitys:[]}}});"#;
    for (id, command) in [
        (
            format!("{request}-import-source"),
            BackendCommand::ImportSourceText {
                script: script.into(),
            },
        ),
        (
            format!("{request}-approve-source"),
            BackendCommand::ApproveSource {
                id: lx_gpui_player::source::script_hash(script),
            },
        ),
    ] {
        let cursor = running.backend.event_cursor();
        running.execute(&id, command);
        assert_eq!(
            operation_finished(&running.backend, cursor, &id),
            (true, None)
        );
    }
    let cursor = running.backend.event_cursor();
    running.play(request, track_id, hz);
    let mut url = None;
    wait(&running.backend, 12.0, |_| {
        for event in running.backend.events_after(cursor).unwrap() {
            if let BackendEvent::SourceHost {
                payload: SourceHostEvent::UpdateAlert { log, .. },
                ..
            } = event.payload
                && let Some(value) = log.strip_prefix("watch-actual-url=")
            {
                url = Some(value.to_owned());
            }
        }
        url.is_some()
    });
    url.unwrap()
}

pub fn http_wave(url: &str, allowed: bool) {
    let parsed = reqwest::Url::parse(url).unwrap();
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-43")
        .send()
        .unwrap();
    if allowed {
        assert!(response.status().is_success(), "{response:?}");
        let bytes = response.bytes().unwrap();
        assert_eq!(bytes.len(), 44);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
    } else {
        assert!(!response.status().is_success());
        assert!(!response.bytes().unwrap().starts_with(b"RIFF"));
    }
}

pub fn stop(running: &Running, request: &str) {
    running.execute(request, BackendCommand::Stop);
    wait(&running.backend, 10.0, |state| {
        state.playback.status == PlaybackStatus::Idle
    });
}

pub fn same_contents(left: &Path, right: &Path) {
    assert_eq!(fs::read(left).unwrap(), fs::read(right).unwrap());
}

pub fn fresh(backend: &BackendHandle) -> BackendSnapshot {
    backend.snapshot_with_cursor().unwrap().0
}

pub fn bounded_watch_generation(backend: &BackendHandle, root_id: &str, expected: u64) {
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        assert_eq!(
            watch(backend, root_id).unwrap().preference.watch_generation,
            expected
        );
        thread::sleep(Duration::from_millis(10));
    }
}
