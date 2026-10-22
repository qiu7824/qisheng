//! Real encoded representations remain separate from each fresh media approval.
#[path = "support/encoded_reuse.rs"]
mod fixture;

use fixture::*;
use lx_gpui_player::{
    audio::{
        AudioOutputMode, AudioPlayer, PlaybackSnapshot, PlaybackStatus,
        cache::CacheIoErrorCode,
        transport::{ApprovedAudioRequest, AudioTransport, CacheIdentity},
    },
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope, ReceiptState,
    },
    domain::lists::{AddLocation, ListAction},
    playback::queue::PlayMode,
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};
use symphonia::core::io::MediaSource;

static CASES: Mutex<()> = Mutex::new(());
const CHILD_MODE: &str = "LX_ENCODED_REUSE_CHILD_MODE";
const CHILD_ROOT: &str = "LX_ENCODED_REUSE_CHILD_ROOT";
const CHILD_URL: &str = "LX_ENCODED_REUSE_CHILD_URL";
const CHILD_DIGEST: &str = "LX_ENCODED_REUSE_CHILD_DIGEST";

fn transport(root: &Path) -> Arc<AudioTransport> {
    let transport = AudioTransport::new(options(root)).unwrap();
    transport
        .cache()
        .wait_ready(Duration::from_secs(8))
        .unwrap();
    transport
}
fn wait_player(
    player: &AudioPlayer,
    condition: impl Fn(&PlaybackSnapshot) -> bool,
) -> PlaybackSnapshot {
    let started = Instant::now();
    loop {
        let snapshot = player.snapshot();
        if condition(&snapshot) {
            return snapshot;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "{snapshot:?}");
        thread::sleep(Duration::from_millis(3));
    }
}
fn assert_revalidation(records: &[HttpRecord]) {
    assert_eq!(
        records.len(),
        1,
        "fully resident media should require one validation GET: {records:?}"
    );
    assert_eq!(records[0].range, Some((0, Some(0))));
    assert_eq!(records[0].target, "/original.wav");
    assert_eq!(records[0].if_range.as_deref(), Some("\"original-v1\""));
    assert_eq!(records[0].status, 206);
    assert_eq!(records[0].body_bytes, 1);
}

#[test]
fn fresh_approval_epoch_and_stream_serial_reuse_same_exact_representation() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let root = directory.path().join("cache");
    let server = Server::new();
    let context = Context::default();
    let transport = transport(&root);
    let expected = server.representation.lock().unwrap().body.clone();
    assert_eq!(read_all(&transport, &context, &server.url, 1), *expected);
    let block_count = blocks(&root).len();
    assert!(block_count > 0);
    let before = server.count();
    assert_eq!(read_all(&transport, &context, &server.url, 2), *expected);
    assert_revalidation(&server.records()[before..]);
    assert_eq!(
        blocks(&root).len(),
        block_count,
        "aliases must not duplicate physical blocks"
    );
    assert!(transport.cache().stats().disk_bytes <= options(&root).cache.disk_bytes);
    assert_eq!(transport.cache().stats().disk_bytes, physical_bytes(&root));
    assert!(transport.shutdown(Duration::from_secs(8)));
}

#[test]
fn ignored_range_strong_200_reuses_complete_bytes_with_bounded_client_validation_and_exact_seek() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let root = directory.path().join("cache");
    let server = Server::new();
    server.representation.lock().unwrap().ignore_range = true;
    let context = Context::default();
    let owner = transport(&root);
    let expected = server.representation.lock().unwrap().body.clone();
    let mut first = open(&owner, &context, &server.url, 1);
    let mut actual = Vec::new();
    first.reader.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, *expected);
    assert!(!first.control.status().range_supported);
    assert_eq!(
        first.control.status().encoded_downloaded_bytes,
        expected.len() as u64
    );
    first.control.cancel();
    assert!(first.control.join(Duration::from_secs(5)));
    drop(first);
    let before = server.count();
    let mut second = open(&owner, &context, &server.url, 2);
    second.control.wait_ready(Duration::from_secs(8)).unwrap();
    let status = second.control.status();
    assert!(!status.range_supported);
    assert!(status.cache_random_access);
    assert!(second.reader.is_seekable());
    assert!(
        (1..=8192).contains(&status.encoded_downloaded_bytes),
        "validation consumed a full sequential transfer: {status:?}"
    );
    let mut sample = [0; 128];
    second.reader.seek(SeekFrom::Start(44)).unwrap();
    second.reader.read_exact(&mut sample).unwrap();
    assert_eq!(&sample, &expected[44..172]);
    second.reader.seek(SeekFrom::End(-128)).unwrap();
    second.reader.read_exact(&mut sample).unwrap();
    assert_eq!(&sample, &expected[expected.len() - 128..]);
    second.reader.seek(SeekFrom::Start(0)).unwrap();
    let mut actual = Vec::new();
    second.reader.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, *expected);
    let final_status = second.control.status();
    assert_eq!(
        final_status.encoded_downloaded_bytes,
        status.encoded_downloaded_bytes
    );
    server.wait_records(before + 1);
    let records = server.records();
    let validation = &records[before..];
    assert_eq!(validation.len(), 1);
    assert_eq!(validation[0].status, 200);
    assert_eq!(validation[0].range, Some((0, Some(0))));
    assert_eq!(validation[0].if_range.as_deref(), Some("\"original-v1\""));
    second.control.cancel();
    assert!(second.control.join(Duration::from_secs(5)));
    drop(second);
    assert!(owner.shutdown(Duration::from_secs(8)));
}

#[test]
fn ignored_range_200_changed_tag_length_and_weak_validators_download_current_bytes() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    for change in ["etag", "length", "weak", "missing"] {
        let directory = directory();
        let root = directory.path().join("cache");
        let server = Server::new();
        server.representation.lock().unwrap().ignore_range = true;
        let context = Context::default();
        let owner = transport(&root);
        let expected = server.representation.lock().unwrap().body.clone();
        assert_eq!(read_all(&owner, &context, &server.url, 1), *expected);
        {
            let mut current = server.representation.lock().unwrap();
            current.body = Arc::new(wave(if change == "length" { 2.5 } else { 2.0 }, 880.0));
            current.etag = match change {
                "etag" => Some("\"original-v2\"".into()),
                "weak" => Some("W/\"original-v1\"".into()),
                "missing" => None,
                _ => current.etag.clone(),
            };
        }
        let expected = server.representation.lock().unwrap().body.clone();
        let mut lease = open(&owner, &context, &server.url, 2);
        let serial = lease.control.status().source_serial;
        let mut actual = Vec::new();
        lease.reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, *expected, "{change} supplied stale cached bytes");
        assert!(lease.control.join(Duration::from_secs(5)));
        let status = lease.control.status();
        assert!(!status.range_supported);
        assert!(status.cache_random_access);
        assert!(lease.reader.is_seekable());
        assert!(
            status.encoded_downloaded_bytes >= expected.len() as u64,
            "{change} did not consume the current full body: {status:?}"
        );
        let before_seek = server.count();
        lease.reader.seek(SeekFrom::Start(44)).unwrap();
        let mut current = [0_u8; 128];
        lease.reader.read_exact(&mut current).unwrap();
        assert_eq!(
            &current,
            &expected[44..172],
            "{change} backwards seek used old bytes"
        );
        lease.reader.seek(SeekFrom::End(-128)).unwrap();
        lease.reader.read_exact(&mut current).unwrap();
        assert_eq!(&current, &expected[expected.len() - 128..]);
        let sought = lease.control.status();
        assert_eq!(sought.source_serial, serial);
        assert_eq!(
            sought.encoded_downloaded_bytes,
            status.encoded_downloaded_bytes
        );
        assert_eq!(
            server.count(),
            before_seek,
            "current protected seek must not fetch another response"
        );
        if matches!(change, "weak" | "missing") {
            assert!(!status.reusable_registered);
        }
        lease.control.cancel();
        assert!(lease.control.join(Duration::from_secs(5)));
        drop(lease);
        if matches!(change, "weak" | "missing") {
            let mut fresh = open(&owner, &context, &server.url, 3);
            assert_ne!(fresh.control.status().source_serial, serial);
            let mut body = Vec::new();
            fresh.reader.read_to_end(&mut body).unwrap();
            assert_eq!(body, *expected);
            assert!(fresh.control.join(Duration::from_secs(5)));
            let status = fresh.control.status();
            assert!(!status.reusable_registered);
            assert!(
                status.encoded_downloaded_bytes >= expected.len() as u64,
                "weak or missing validators must fetch the complete body for a fresh approval"
            );
            fresh.control.cancel();
            assert!(fresh.control.join(Duration::from_secs(5)));
            drop(fresh);
        }
        assert!(owner.shutdown(Duration::from_secs(8)));
    }
}

#[test]
fn changed_validator_length_or_context_never_uses_old_bytes() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    for dimension in [
        "etag", "length", "weak", "missing", "source", "policy", "dto", "quality", "url",
    ] {
        let directory = directory();
        let root = directory.path().join("cache");
        let server = Server::new();
        let baseline = Context::default();
        let transport = transport(&root);
        let original = server.representation.lock().unwrap().body.clone();
        assert_eq!(read_all(&transport, &baseline, &server.url, 1), *original);
        let mut next = baseline.clone();
        let mut url = server.url.clone();
        match dimension {
            "etag" => {
                let mut data = server.representation.lock().unwrap();
                data.etag = Some("\"original-v2\"".into());
                data.body = Arc::new(wave(2.0, 880.0));
            }
            "length" => server.representation.lock().unwrap().body = Arc::new(wave(2.5, 660.0)),
            "weak" => {
                let mut data = server.representation.lock().unwrap();
                data.etag = Some("W/\"original-v1\"".into());
                data.body = Arc::new(wave(2.0, 660.0));
            }
            "missing" => {
                let mut data = server.representation.lock().unwrap();
                data.etag = None;
                data.body = Arc::new(wave(2.0, 660.0));
            }
            "source" => {
                next.source_sha256 = digest(b"Original approved encoded source version two")
            }
            "policy" => next.policy += 1,
            "dto" => next.record.album = "Updated original recording metadata".into(),
            "quality" => next.quality = "320k".into(),
            "url" => url.push_str("?different-resource=1"),
            _ => unreachable!(),
        }
        let before = server.count();
        let expected = server.representation.lock().unwrap().body.clone();
        assert_eq!(
            read_all(&transport, &next, &url, 2),
            *expected,
            "{dimension} exposed stale bytes"
        );
        assert!(
            server.bytes_since(before) >= expected.len(),
            "{dimension} unexpectedly reused old media: {:?}",
            &server.records()[before..]
        );
        assert!(transport.shutdown(Duration::from_secs(8)));
    }
}

#[test]
fn legacy_requests_without_a_media_context_never_reuse_encoded_blocks() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let root = directory.path().join("cache");
    let server = Server::new();
    let transport = transport(&root);
    let expected = server.representation.lock().unwrap().body.clone();
    for attempt in 0..2 {
        let before = server.count();
        let mut lease = transport
            .open(
                ApprovedAudioRequest::new(
                    &server.url,
                    CacheIdentity {
                        track_id: "legacy-original".into(),
                        approval_epoch: attempt,
                        ..Default::default()
                    },
                )
                .unwrap(),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let mut actual = Vec::new();
        lease.reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, *expected);
        lease.control.cancel();
        assert!(lease.control.join(Duration::from_secs(5)));
        drop(lease);
        assert!(server.bytes_since(before) >= expected.len());
    }
    assert!(transport.shutdown(Duration::from_secs(8)));
}

#[test]
fn rejected_revalidation_never_plays_cached_pcm() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    for status in [401_u16, 403, 404, 410] {
        let directory = directory();
        let root = directory.path().join("cache");
        let server = Server::new();
        let context = Context::default();
        let owner = transport(&root);
        assert_eq!(
            read_all(&owner, &context, &server.url, 1),
            *server.representation.lock().unwrap().body
        );
        assert!(owner.shutdown(Duration::from_secs(8)));
        drop(owner);
        server.representation.lock().unwrap().status = status;
        let before = server.count();
        let player = AudioPlayer::with_transport_options(AudioOutputMode::Memory, options(&root));
        let request = context.request(&server.url, 2);
        let url_hash = request.identity.url_sha256.clone();
        player.load_approved(request, true);
        let failed = wait_player(&player, |state| state.status == PlaybackStatus::Error);
        let failure = failed.failure.as_ref().unwrap();
        assert_eq!(failure.media.as_ref().unwrap().http_status, Some(status));
        assert_eq!(
            failure.request_url_sha256.as_deref(),
            Some(url_hash.as_str())
        );
        assert!(failure.requires_url_refresh());
        let pcm = player.rendered_pcm().unwrap();
        assert_eq!(
            pcm.batches, 0,
            "cached PCM was rendered before HTTP validation rejected it"
        );
        assert_eq!(pcm.peak, 0.0);
        assert_eq!(server.bytes_since(before), 0);
        player.shutdown(Duration::from_secs(8)).unwrap();
    }
}

struct ColdChild(Child);
impl ColdChild {
    fn warm(root: &Path, server: &Server) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "encoded_reuse_cold_child_fixture", "--nocapture"])
            .env(CHILD_MODE, "warm")
            .env(CHILD_ROOT, root)
            .env(CHILD_URL, &server.url)
            .env(
                CHILD_DIGEST,
                digest(&server.representation.lock().unwrap().body),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        Self(command.spawn().unwrap())
    }
    fn success(&mut self) {
        let started = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "real encoded child failed: {status}");
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(25),
                "real encoded child deadline"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for ColdChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn encoded_reuse_cold_child_fixture() {
    if std::env::var(CHILD_MODE).ok().as_deref() != Some("warm") {
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).unwrap());
    let url = std::env::var(CHILD_URL).unwrap();
    let expected = std::env::var(CHILD_DIGEST).unwrap();
    let owner = transport(&root);
    let actual = read_all(&owner, &Context::default(), &url, 1);
    assert_eq!(digest(&actual), expected);
    assert!(owner.shutdown(Duration::from_secs(8)));
    drop(owner);
}

#[test]
fn cold_process_reopen_revalidates_and_reuses_owned_blocks() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let root = directory.path().join("cache");
    let server = Server::new();
    ColdChild::warm(&root, &server).success();
    assert!(!blocks(&root).is_empty());
    let before = server.count();
    let owner = transport(&root);
    assert_eq!(
        server.count(),
        before,
        "cold metadata admission must not perform media GET without approval"
    );
    let actual = read_all(&owner, &Context::default(), &server.url, 30);
    assert_eq!(actual, *server.representation.lock().unwrap().body);
    assert_revalidation(&server.records()[before..]);
    assert!(owner.shutdown(Duration::from_secs(8)));
}

#[test]
fn cold_body_tampering_cannot_supply_reused_bytes() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let root = directory.path().join("cache");
    let server = Server::new();
    ColdChild::warm(&root, &server).success();
    let file = blocks(&root)
        .into_iter()
        .find(|file| fs::metadata(file).unwrap().len() > HEADER_BYTES + 150)
        .unwrap();
    let mut body = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    body.seek(SeekFrom::Start(HEADER_BYTES + 100)).unwrap();
    let mut original = [0];
    body.read_exact(&mut original).unwrap();
    body.seek(SeekFrom::Start(HEADER_BYTES + 100)).unwrap();
    body.write_all(&[original[0] ^ 0x5a]).unwrap();
    body.sync_all().unwrap();
    drop(body);
    let before = server.count();
    let owner = transport(&root);
    assert_eq!(
        read_all(&owner, &Context::default(), &server.url, 30),
        *server.representation.lock().unwrap().body
    );
    assert!(
        server.bytes_since(before) > 1,
        "corrupt bytes passed immutable payload proof"
    );
    assert!(owner.shutdown(Duration::from_secs(8)));
}

#[test]
fn cold_newest_manifest_checksum_failure_never_admits_unverified_blocks() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let root = directory.path().join("cache");
    let server = Server::new();
    ColdChild::warm(&root, &server).success();
    let mut slots: Vec<_> = files(&root)
        .into_iter()
        .filter(|file| {
            file.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("manifest.")
        })
        .map(|file| {
            let bytes = fs::read(&file).unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&bytes[HEADER_BYTES as usize..]).unwrap();
            (value["sequence"].as_u64().unwrap(), file, bytes, value)
        })
        .collect();
    slots.sort_by_key(|slot| slot.0);
    let (_, file, bytes, mut value) = slots.pop().unwrap();
    value["checksum"] = serde_json::Value::String("f".repeat(64));
    let mut corrupted = bytes[..HEADER_BYTES as usize].to_vec();
    corrupted.extend(serde_json::to_vec(&value).unwrap());
    fs::write(&file, corrupted).unwrap();
    let before = server.count();
    let owner = AudioTransport::new(options(&root)).unwrap();
    if owner.cache().wait_ready(Duration::from_secs(8)).is_ok() {
        assert_eq!(
            read_all(&owner, &Context::default(), &server.url, 40),
            *server.representation.lock().unwrap().body
        );
    } else {
        let state = owner.cache().stats();
        assert_eq!(state.last_io_error, CacheIoErrorCode::Initialization);
        assert!(state.cannot_clear);
        assert_eq!(server.count(), before);
    }
    assert!(owner.shutdown(Duration::from_secs(8)));
}

#[test]
fn multiple_reader_clear_preserves_immutable_views_and_unique_physical_accounting() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let root = directory.path().join("cache");
    let server = Server::new();
    let context = Context::default();
    let owner = transport(&root);
    assert_eq!(
        read_all(&owner, &context, &server.url, 1),
        *server.representation.lock().unwrap().body
    );
    let count = blocks(&root).len();
    let mut first = open(&owner, &context, &server.url, 2);
    let mut second = open(&owner, &context, &server.url, 3);
    first.control.wait_ready(Duration::from_secs(8)).unwrap();
    second.control.wait_ready(Duration::from_secs(8)).unwrap();
    let mut bytes = [0; 64];
    first.reader.read_exact(&mut bytes).unwrap();
    second.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(blocks(&root).len(), count);
    assert_eq!(owner.cache().stats().disk_bytes, physical_bytes(&root));
    let cleared = owner
        .cache()
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(8))
        .unwrap();
    assert!(cleared.deferred_blocks > 0);
    assert!(cleared.disk_bytes >= server.representation.lock().unwrap().body.len() as u64);
    first.reader.seek(SeekFrom::Start(44)).unwrap();
    second.reader.seek(SeekFrom::Start(44)).unwrap();
    let expected = server.representation.lock().unwrap().body.clone();
    first.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, &expected[44..108]);
    second.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, &expected[44..108]);
    first.control.cancel();
    second.control.cancel();
    assert!(first.control.join(Duration::from_secs(5)));
    assert!(second.control.join(Duration::from_secs(5)));
    drop(first);
    drop(second);
    let cleared = owner
        .cache()
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(8))
        .unwrap();
    assert_eq!(cleared.encoded_bytes, 0);
    assert_eq!(cleared.retiring_blocks, 0);
    assert_eq!(cleared.disk_bytes, physical_bytes(&root));
    assert!(cleared.disk_bytes <= options(&root).cache.disk_bytes);
    let before = server.count();
    assert_eq!(read_all(&owner, &context, &server.url, 4), *expected);
    assert!(
        server.bytes_since(before) >= expected.len(),
        "clear resurrected a persistent representation"
    );
    assert!(owner.shutdown(Duration::from_secs(8)));
}

fn command(backend: &BackendHandle, id: &str, command: BackendCommand) {
    let before = backend.event_cursor();
    let receipt = backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "encoded-reuse-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap();
    if receipt.state != ReceiptState::Accepted {
        return;
    }
    let started = Instant::now();
    loop {
        for event in backend.events_after(before).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == receipt.operation_id
            {
                assert!(success, "{id}: {error:?}");
                return;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{id} operation deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn wait_backend(
    backend: &BackendHandle,
    condition: impl Fn(&BackendSnapshot) -> bool,
) -> BackendSnapshot {
    let started = Instant::now();
    loop {
        let state = backend.snapshot();
        if condition(&state) {
            return state;
        }
        assert!(started.elapsed() < Duration::from_secs(15), "{state:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn real_memory_playback_waits_for_each_backend_url_approval_before_reuse() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = directory();
    let server = Server::new();
    let record = record();
    let backend =
        BackendHandle::start(BackendConfig::headless(directory.path().join("profile"))).unwrap();
    command(
        &backend,
        "list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![record.clone()],
                location: AddLocation::Bottom,
            }),
        },
    );
    command(
        &backend,
        "mode",
        BackendCommand::SetPlayMode {
            mode: PlayMode::None,
        },
    );
    command(
        &backend,
        "skip-off",
        BackendCommand::SetAutoSkipOnError { enabled: false },
    );
    let script = format!(
        r#"/**
 * @name Encoded reuse source
 * @version 1.0
 */
lx.on('request',async({{source,action,info}})=>{{
 if(source!=='kw'||action!=='musicUrl'||info.type!=='128k'||String(info.musicInfo.songmid)!=='original-encoded')throw Error('incorrect original record');
 return {url};
}});
lx.send('inited',{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});
"#,
        url = serde_json::to_string(&server.url).unwrap()
    );
    command(
        &backend,
        "import",
        BackendCommand::ImportSourceText { script },
    );
    let source = backend
        .snapshot()
        .sources
        .iter()
        .find(|source| source.name == "Encoded reuse source")
        .unwrap()
        .id
        .clone();
    command(
        &backend,
        "approve-source",
        BackendCommand::ApproveSource { id: source },
    );
    let mut prior_identity: Option<CacheIdentity> = None;
    for attempt in 1..=2 {
        let before = server.count();
        let before_bytes = server.bytes_since(0);
        command(
            &backend,
            &format!("play-{attempt}"),
            BackendCommand::PlayListTrack {
                list_id: "default".into(),
                music_id: record.id.clone(),
                quality: Some("128k".into()),
            },
        );
        let pending = wait_backend(&backend, |state| state.pending_playback_url.is_some());
        assert_eq!(
            pending.pending_playback_url.as_deref(),
            Some(server.url.as_str())
        );
        assert_ne!(pending.playback.status, PlaybackStatus::Playing);
        assert_eq!(
            server.count(),
            before,
            "cached permission caused media GET before new approval"
        );
        if let BackendQueryResult::RenderedPcm(Some(pcm)) =
            backend.query(BackendQuery::RenderedPcm).unwrap()
            && attempt == 1
        {
            assert_eq!(pcm.batches, 0);
        }
        command(
            &backend,
            &format!("approve-url-{attempt}"),
            BackendCommand::ApprovePlaybackUrl,
        );
        let playing = wait_backend(&backend, |state| {
            state.playback.status == PlaybackStatus::Playing
        });
        let identity = playing.approved_audio_identity.unwrap();
        assert!(!identity.media_context_sha256.is_empty());
        if let Some(previous) = &prior_identity {
            assert_ne!(previous.approval_epoch, identity.approval_epoch);
            assert_ne!(previous.approval_id, identity.approval_id);
        }
        prior_identity = Some(identity);
        server.wait_records(before + 1);
        let started = Instant::now();
        loop {
            if let BackendQueryResult::AudioCacheStats(Some(stats)) =
                backend.query(BackendQuery::AudioCacheStats).unwrap()
                && stats.pending_publish_bytes == 0
                && stats.encoded_bytes >= server.representation.lock().unwrap().body.len() as u64
            {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(8));
            thread::sleep(Duration::from_millis(5));
        }
        if attempt == 1 {
            assert!(
                server.bytes_since(0) - before_bytes
                    >= server.representation.lock().unwrap().body.len()
            );
        } else {
            assert_revalidation(&server.records()[before..]);
        }
        command(&backend, &format!("stop-{attempt}"), BackendCommand::Stop);
        wait_backend(&backend, |state| {
            state.playback.status == PlaybackStatus::Idle && !state.busy
        });
        let started = Instant::now();
        loop {
            if let BackendQueryResult::AudioCacheStats(Some(stats)) =
                backend.query(BackendQuery::AudioCacheStats).unwrap()
                && stats.active_leases == 0
            {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(8));
            thread::sleep(Duration::from_millis(5));
        }
    }
    backend.shutdown(Duration::from_secs(10)).unwrap();
}
