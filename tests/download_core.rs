use lx_gpui_player::download::{
    ApprovedDownloadUrl, CollisionPolicy, DestinationGrant, DownloadContext, DownloadErrorKind,
    DownloadExecutionLease, DownloadHandle, DownloadPool, DownloadState, OwnedPart, StopReason,
    SymphoniaVerifier, TransferCheckpoint, TransferJob, TransferOptions, TransferOutcome,
    validate_basename,
};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
#[path = "support/encoded_audio_fixture.rs"]
mod encoded_audio_fixture;
#[path = "support/native_producer_helpers.rs"]
#[allow(
    dead_code,
    reason = "Shared native fixtures expose constructors used by sibling integration targets"
)]
mod native_producer_helpers;

fn wav() -> Vec<u8> {
    let samples: Vec<i16> = (0..8192)
        .map(|index| ((index as f32 * 0.17).sin() * 12000.) as i16)
        .collect();
    let bytes = samples.len() as u32 * 2;
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + bytes).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&8000u32.to_le_bytes());
    data.extend_from_slice(&16000u32.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&bytes.to_le_bytes());
    for sample in samples {
        data.extend_from_slice(&sample.to_le_bytes());
    }
    data
}
fn context() -> DownloadContext {
    DownloadContext {
        profile_id: "fixture-profile".into(),
        task_id: "fixture-task".into(),
        operation_epoch: 1,
        source_id: "fixture-source".into(),
        source_content_sha256: "11".repeat(32),
        policy_epoch: 1,
    }
}
fn headers(socket: &mut TcpStream) -> String {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut data = Vec::new();
    let mut byte = [0];
    while !data.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        data.push(byte[0]);
        assert!(data.len() <= 16384);
    }
    String::from_utf8(data).unwrap()
}
type Handler = Box<dyn FnOnce(&mut TcpStream, &str) + Send>;
fn server(handlers: Vec<Handler>) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://{}/audio?secret=fixture",
        listener.local_addr().unwrap()
    );
    let (sender, requests) = mpsc::channel();
    let thread = thread::spawn(move || {
        for handler in handlers {
            let started = Instant::now();
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            started.elapsed() < Duration::from_secs(6),
                            "Fixture expected an actual HTTP request"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let request = headers(&mut socket);
            let _ = sender.send(request.clone());
            handler(&mut socket, &request);
        }
    });
    (url, requests, thread)
}
fn response(body: Vec<u8>, etag: &'static str) -> Handler {
    Box::new(move |socket, _| {
        let _ = write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(&body);
    })
}
fn truncated(body: Vec<u8>, prefix: usize, etag: &'static str) -> Handler {
    Box::new(move |socket, _| {
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        socket.write_all(&body[..prefix]).unwrap();
        socket.flush().unwrap();
        socket.shutdown(Shutdown::Both).unwrap();
    })
}
fn options() -> TransferOptions {
    TransferOptions {
        connect_timeout: Duration::from_secs(1),
        header_timeout: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(2),
        retry_delay: Duration::from_millis(20),
        automatic_retries: 0,
        ..Default::default()
    }
}
fn job(
    url: &str,
    part: OwnedPart,
    checkpoint: Option<TransferCheckpoint>,
    lease: DownloadExecutionLease,
) -> TransferJob {
    let context = context();
    let approved_url =
        ApprovedDownloadUrl::from_exact_approval(url, url, &context, &context).unwrap();
    native_producer_helpers::bind_url(part.record(), url).unwrap();
    let quality = match part.record().final_basename.rsplit('.').next().unwrap() {
        "mp3" => lx_gpui_player::download::planning::DownloadQuality::K128,
        "flac" => lx_gpui_player::download::planning::DownloadQuality::Flac,
        "wav" => lx_gpui_player::download::planning::DownloadQuality::Wav,
        "ape" => lx_gpui_player::download::planning::DownloadQuality::Ape,
        extension => panic!("Unsupported fixture extension {extension}"),
    };
    let representation =
        lx_gpui_player::download::DownloadRepresentationIdentity::from_approved_url(
            &approved_url,
            "22".repeat(32),
            quality,
        )
        .unwrap();
    TransferJob {
        approved_url,
        representation,
        execution_lease: lease,
        part,
        checkpoint,
        extension: quality.extension().into(),
        redirect_count: 0,
        options: options(),
        verifier: Arc::new(SymphoniaVerifier),
    }
}
fn wait(handle: &DownloadHandle) -> TransferOutcome {
    let started = Instant::now();
    loop {
        if let Some(outcome) = handle.take_outcome() {
            return outcome;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "Download operation did not close its actual worker job"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn partial(
    outcome: TransferOutcome,
) -> (
    lx_gpui_player::download::OwnedPartRecord,
    TransferCheckpoint,
) {
    match outcome {
        TransferOutcome::Failed {
            error,
            part,
            checkpoint: Some(checkpoint),
        } => {
            assert_eq!(error.kind, DownloadErrorKind::Network);
            (part, checkpoint)
        }
        _ => panic!("Expected a durable network-interrupted owned partial"),
    }
}

#[test]
fn full_http_audio_is_decoded_hashed_and_atomically_committed_without_overwrite() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let plan = grant.plan("歌曲.wav", CollisionPolicy::Skip).unwrap();
    let body = wav();
    let expected_sha = format!("{:x}", Sha256::digest(&body));
    let (url, requests, server) = server(vec![response(body.clone(), "\"v1\"")]);
    let pool = DownloadPool::with_defaults().unwrap();
    assert_eq!(pool.stats().configured_workers, 3);
    assert_eq!(pool.stats().workers, 0);
    let part = native_producer_helpers::create(plan, &context()).unwrap();
    let handle = pool
        .submit(job(
            &url,
            part,
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let audio = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Actual WAV must verify"),
    };
    assert_eq!(audio.checkpoint().sha256, expected_sha);
    assert_eq!(audio.verification().decoded_frames, 8192);
    assert_eq!(audio.verification().sample_rate, 8000);
    assert_eq!(audio.verification().channels, 1);
    let committed = native_producer_helpers::commit(audio).unwrap();
    // Publication alone keeps the charge until native retirement or Library adoption.
    assert_eq!(pool.stats().known_partial_bytes, body.len() as u64);
    assert_eq!(std::fs::read(&committed.path).unwrap(), body);
    let request = requests.recv().unwrap().to_lowercase();
    assert!(request.contains("accept-encoding: identity"));
    assert!(!request.contains("range:"));
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
    assert_eq!(pool.stats().known_partial_bytes, 0);
    drop(pool);
}

#[test]
fn strong_validator_resume_compares_overlap_split_into_one_byte_http_chunks() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let plan = grant.plan("resumed.wav", CollisionPolicy::Skip).unwrap();
    let body = wav();
    let resumed = body.clone();
    let (url, requests, server) = server(vec![
        truncated(body.clone(), 4107, "\"same\""),
        Box::new(move |socket, request| {
            let request = request.to_lowercase();
            assert!(request.contains("range: bytes=4097-"));
            assert!(request.contains("if-range: \"same\""));
            write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4097-{}/{}\r\nETag: \"same\"\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n", resumed.len()-1, resumed.len()).unwrap();
            for byte in &resumed[4097..4108] {
                socket
                    .write_all(&[b'1', b'\r', b'\n', *byte, b'\r', b'\n'])
                    .unwrap();
                socket.flush().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            write!(socket, "{:x}\r\n", resumed.len() - 4108).unwrap();
            socket.write_all(&resumed[4108..]).unwrap();
            socket.write_all(b"\r\n0\r\n\r\n").unwrap();
        }),
    ]);
    let pool = DownloadPool::new(1).unwrap();
    let first = pool
        .submit(job(
            &url,
            native_producer_helpers::create(plan.clone(), &context()).unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let (record, checkpoint) = partial(wait(&first));
    assert_eq!(checkpoint.bytes, 4107);
    assert!(!checkpoint.completed_body);
    assert_eq!(checkpoint.strong_etag.as_deref(), Some("\"same\""));
    let resumed = pool
        .submit(job(
            &url,
            native_producer_helpers::reopen(plan, record, &context()).unwrap(),
            Some(checkpoint),
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let audio = match wait(&resumed) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Resumed WAV must verify"),
    };
    let committed = native_producer_helpers::commit(audio).unwrap();
    assert_eq!(std::fs::read(committed.path).unwrap(), body);
    assert_eq!(requests.try_iter().count(), 2);
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(first);
    drop(resumed);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn ignored_resume_range_restarts_only_owned_partial_and_never_appends_a_full_response() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let plan = grant.plan("fresh.wav", CollisionPolicy::Skip).unwrap();
    let body = wav();
    let (url, requests, server) = server(vec![
        truncated(body.clone(), 2048, "\"same\""),
        response(body.clone(), "\"same\""),
        response(body.clone(), "\"same\""),
    ]);
    let pool = DownloadPool::new(1).unwrap();
    let first = pool
        .submit(job(
            &url,
            native_producer_helpers::create(plan.clone(), &context()).unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let (record, checkpoint) = partial(wait(&first));
    let resumed = pool
        .submit(job(
            &url,
            native_producer_helpers::reopen(plan, record, &context()).unwrap(),
            Some(checkpoint),
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let audio = match wait(&resumed) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Fresh replacement representation must verify"),
    };
    assert_eq!(audio.checkpoint().bytes, body.len() as u64);
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(audio).unwrap().path).unwrap(),
        body
    );
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].to_lowercase().contains("range:"));
    assert!(!requests[2].to_lowercase().contains("range:"));
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(first);
    drop(resumed);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn redirect_requires_exact_new_approval_and_never_connects_to_redirect_target() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let destination = format!("http://{}/new?private=target", target.local_addr().unwrap());
    let expected = destination.clone();
    let (url, _, server) = server(vec![Box::new(move |socket, _| {
        write!(socket, "HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    })]);
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("redirect.wav", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    match wait(&handle) {
        TransferOutcome::AwaitingRedirectApproval {
            exact_url,
            redirect_count,
            checkpoint,
            ..
        } => {
            assert_eq!(exact_url, expected);
            assert_eq!(redirect_count, 1);
            assert_eq!(checkpoint.bytes, 0);
        }
        _ => panic!("Redirect requires a new explicit approval"),
    }
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(handle.progress().state, DownloadState::AwaitingUrlApproval);
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn pause_interrupts_idle_body_and_shutdown_joins_actual_workers() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let (release, hold) = mpsc::channel();
    let (url, requests, server) = server(vec![Box::new(move |socket, _| {
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nETag: \"held\"\r\nConnection: close\r\n\r\n").unwrap();
        socket.flush().unwrap();
        hold.recv_timeout(Duration::from_secs(3)).unwrap();
    })]);
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("paused.wav", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    handle.stop(StopReason::Pause);
    match wait(&handle) {
        TransferOutcome::Stopped {
            reason,
            checkpoint: Some(checkpoint),
            ..
        } => {
            assert_eq!(reason, StopReason::Pause);
            assert_eq!(checkpoint.bytes, 0);
        }
        _ => panic!("Pause must close the request and persist its own partial"),
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    pool.shutdown(Duration::from_secs(1)).unwrap();
    assert_eq!(pool.stats().active, 0);
    assert_eq!(pool.stats().queued, 0);
    assert_eq!(pool.stats().workers, 0);
    release.send(()).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn approval_and_destination_revocation_reject_before_network_or_mutation() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/never", listener.local_addr().unwrap());
    let mut wrong = context();
    wrong.operation_epoch += 1;
    assert!(ApprovedDownloadUrl::from_exact_approval(&url, &url, &context(), &wrong).is_err());
    assert!(
        ApprovedDownloadUrl::from_exact_approval(
            &url,
            "http://127.0.0.1:1/other",
            &context(),
            &context()
        )
        .is_err()
    );
    let part = native_producer_helpers::create(
        grant.plan("revoked.wav", CollisionPolicy::Skip).unwrap(),
        &context(),
    )
    .unwrap();
    let record = part.record().clone();
    let lease = DownloadExecutionLease::from_approved_context(context()).unwrap();
    lease.revoke();
    let pool = DownloadPool::new(1).unwrap();
    assert!(pool.submit(job(&url, part, None, lease)).is_err());
    grant.revoke();
    assert!(grant.plan("other.wav", CollisionPolicy::Skip).is_err());
    assert!(
        std::fs::metadata(directory.path().join(&record.basename))
            .unwrap()
            .len()
            == 0
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    pool.shutdown(Duration::from_secs(1)).unwrap();
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn existing_empty_destination_and_foreign_partial_identity_are_conflicts() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    std::fs::write(directory.path().join("exists.wav"), []).unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    assert!(grant.plan("exists.wav", CollisionPolicy::Skip).is_err());
    let plan = grant.plan("identity.wav", CollisionPolicy::Skip).unwrap();
    let part = native_producer_helpers::create(plan.clone(), &context()).unwrap();
    let record = part.record().clone();
    let path = part.path();
    drop(part);
    OwnedPart::close_retained(&plan, &record).unwrap();
    std::fs::rename(&path, directory.path().join("saved-original.part")).unwrap();
    std::fs::write(&path, b"foreign-owned-by-user").unwrap();
    assert!(native_producer_helpers::reopen(plan.clone(), record.clone(), &context()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"foreign-owned-by-user");
    OwnedPart::close_retained(&plan, &record).unwrap();
    // Restore the actual owned namespace before fixture retirement; the
    // conflict check intentionally leaves the original FileID at its renamed
    // path and the foreign nonce occupant untouched.
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(directory.path().join("saved-original.part"), &path).unwrap();
    drop(native_producer_helpers::reopen(plan, record, &context()).unwrap());
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn naming_and_worker_bounds_reject_unsafe_components_without_write_grants() {
    for value in [
        "",
        ".",
        "..",
        "CON",
        "nul.mp3",
        "LPT9.flac",
        "../escape.wav",
        "folder/name.wav",
        "trailing. ",
        "bad#name.wav",
        "bad\u{0000}.wav",
    ] {
        assert!(
            validate_basename(value).is_err(),
            "Unsafe basename {value:?}"
        );
    }
    assert!(validate_basename("中文 🎵.wav").is_ok());
    assert!(DownloadPool::new(0).is_err());
    assert!(DownloadPool::new(5).is_err());
    for state in [
        DownloadState::Waiting,
        DownloadState::Resolving,
        DownloadState::Transferring,
        DownloadState::PostProcessing,
        DownloadState::Cancelling,
    ] {
        assert_eq!(state.after_restart(), DownloadState::Paused);
    }
    assert_eq!(
        DownloadState::Completed.after_restart(),
        DownloadState::Completed
    );
}

#[test]
fn short_embedded_wav_payload_is_not_accepted_when_http_body_itself_is_complete() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let mut body = wav();
    body.truncate(44 + 1024 * 2);
    let (url, _, server) = server(vec![response(body, "\"short-valid-http\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("invalid.wav", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    match wait(&handle) {
        TransferOutcome::Failed {
            error,
            checkpoint: Some(checkpoint),
            ..
        } => {
            assert_eq!(error.kind, DownloadErrorKind::Verification);
            assert!(checkpoint.completed_body);
        }
        _ => panic!("Embedded WAV truncation must fail complete audio verification"),
    }
    assert!(!directory.path().join("invalid.wav").exists());
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn source_policy_revocation_prevents_a_retained_verified_audio_from_committing() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = wav();
    let (url, _, server) = server(vec![response(body.clone(), "\"verified-held\"")]);
    let lease = DownloadExecutionLease::from_approved_context(context()).unwrap();
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant
                    .plan("revoked-commit.wav", CollisionPolicy::Skip)
                    .unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            lease.clone(),
        ))
        .unwrap();
    let audio = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Valid WAV must reach a verified owned result"),
    };
    let part_basename = audio.owned_record().basename.clone();
    lease.revoke();
    assert!(native_producer_helpers::commit(audio).is_err());
    assert!(!directory.path().join("revoked-commit.wav").exists());
    assert_eq!(
        std::fs::read(directory.path().join(part_basename)).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn self_authored_flac_passes_full_stream_md5_and_decoded_audio_verification() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = encoded_audio_fixture::mono_verbatim_flac();
    let (url, _, server) = server(vec![response(body.clone(), "\"flac-original\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let mut request = job(
        &url,
        native_producer_helpers::create(
            grant.plan("authored.flac", CollisionPolicy::Skip).unwrap(),
            &context(),
        )
        .unwrap(),
        None,
        DownloadExecutionLease::from_approved_context(context()).unwrap(),
    );
    request.extension = "flac".into();
    let handle = pool.submit(request).unwrap();
    let audio = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Self-authored FLAC must pass its actual codec checks"),
    };
    assert_eq!(audio.verification().decoded_frames, 512);
    assert_eq!(audio.verification().sample_rate, 8000);
    assert_eq!(audio.verification().codec_identifier, "0x2000");
    assert_eq!(
        audio.verification().pcm_digest_encoding,
        lx_gpui_player::download::PcmDigestEncoding::F64LeInterleavedV1
    );
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(audio).unwrap().path).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn self_authored_mpeg_layer3_frames_pass_real_download_and_full_decode() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = encoded_audio_fixture::silent_mpeg_layer3();
    let (url, _, server) = server(vec![response(body.clone(), "\"mpeg-original\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let mut request = job(
        &url,
        native_producer_helpers::create(
            grant.plan("authored.mp3", CollisionPolicy::Skip).unwrap(),
            &context(),
        )
        .unwrap(),
        None,
        DownloadExecutionLease::from_approved_context(context()).unwrap(),
    );
    request.extension = "mp3".into();
    let handle = pool.submit(request).unwrap();
    let audio = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Self-authored MPEG frames must pass actual MP3 decoding"),
    };
    assert_eq!(audio.verification().decoded_frames, 12 * 1152);
    assert_eq!(audio.verification().sample_rate, 44100);
    assert_eq!(audio.verification().codec_identifier, "0x1003");
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(audio).unwrap().path).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn canonical_f64_pcm_digest_preserves_the_lowest_bit_of_32bit_integer_audio() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let a = vec![1_073_741_824i32; 512];
    let mut b = a.clone();
    b[271] += 1;
    assert_eq!(a[271] as f32, b[271] as f32);
    let first = encoded_audio_fixture::mono_pcm32_wav(&a);
    let second = encoded_audio_fixture::mono_pcm32_wav(&b);
    let (url, _, server) = server(vec![
        response(first, "\"pcm32-a\""),
        response(second, "\"pcm32-b\""),
    ]);
    let pool = DownloadPool::new(1).unwrap();
    let handle_a = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("first.wav", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let audio_a = match wait(&handle_a) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("First PCM32 fixture must decode"),
    };
    let digest_a = audio_a.verification().decoded_pcm_sha256.clone();
    assert_eq!(audio_a.verification().source_bits_per_sample, Some(32));
    native_producer_helpers::commit(audio_a).unwrap();
    let handle_b = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("second.wav", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let audio_b = match wait(&handle_b) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Second PCM32 fixture must decode"),
    };
    assert_ne!(digest_a, audio_b.verification().decoded_pcm_sha256);
    native_producer_helpers::commit(audio_b).unwrap();
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle_a);
    drop(handle_b);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn tag_rewrite_uses_a_distinct_owned_copy_and_preserves_original_and_canonical_audio() {
    use lx_gpui_player::{
        download::{AudioVerifier, DownloadCancel},
        library::{
            scan::ScanContext,
            tag_write::{LocalTagPatch, TagField, write_local_tags},
        },
    };
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let flac = encoded_audio_fixture::mono_verbatim_flac();
    let mp3 = encoded_audio_fixture::silent_mpeg_layer3();
    let (url, _, server) = server(vec![
        response(flac.clone(), "\"flac-copy\""),
        response(mp3.clone(), "\"mp3-copy\""),
    ]);
    let pool = DownloadPool::new(1).unwrap();
    for (extension, body) in [("flac", flac), ("mp3", mp3)] {
        let mut request = job(
            &url,
            native_producer_helpers::create(
                grant
                    .plan(&format!("original.{extension}"), CollisionPolicy::Skip)
                    .unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        );
        request.extension = extension.into();
        let handle = pool.submit(request).unwrap();
        let original = match wait(&handle) {
            TransferOutcome::Verified(audio) => audio,
            _ => panic!("Original audio must verify before copying"),
        };
        let original_record = original.owned_record().clone();
        let original_verification = original.verification().clone();
        let original_checkpoint = original.checkpoint().clone();
        let cancel = DownloadCancel::default();
        let (original, copy) = native_producer_helpers::copy(original, cancel.clone()).unwrap();
        let mut copy = copy.unwrap();
        assert_ne!(copy.record().identity, original_record.identity);
        let patch = LocalTagPatch {
            title: TagField::Set("文件标题".into()),
            artists: TagField::Set(vec!["甲".into(), "乙".into()]),
            album: TagField::Set("文件专辑".into()),
            lyrics: TagField::Set("[00:00.00]文件歌词".into()),
            cover: TagField::Keep,
        };
        let work_cancel = cancel.clone();
        let (copy, rewritten, verification, output_digest) =
            lx_gpui_player::download::run_download_producer(move || {
                let rewritten = {
                    let mut io = copy.rewrite_io(work_cancel.clone())?;
                    write_local_tags(&mut io, &patch, &ScanContext::new(Duration::from_secs(5)))?
                };
                let verification = SymphoniaVerifier.verify(&copy, extension, &work_cancel)?;
                let output_digest = copy.checkpoint(&work_cancel, None, None, true)?;
                Ok((copy, rewritten, verification, output_digest))
            })
            .unwrap();
        assert_eq!(verification, original_verification);
        assert_eq!(
            std::fs::read(grant.path().join(&original_record.basename)).unwrap(),
            body
        );
        assert_eq!(original.checkpoint(), &original_checkpoint);
        assert_ne!(rewritten.body_sha256, original_checkpoint.sha256);
        assert_eq!(output_digest.sha256, rewritten.body_sha256);
        assert_eq!(
            std::fs::read(native_producer_helpers::commit(original).unwrap().path).unwrap(),
            body
        );
        drop(copy);
        drop(handle);
    }
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn stopped_metadata_copy_writer_cannot_change_the_original_or_its_shadow() {
    use lx_gpui_player::{
        download::DownloadCancel,
        library::{
            scan::ScanContext,
            tag_write::{LocalTagPatch, TagField, write_local_tags},
        },
    };
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = encoded_audio_fixture::silent_mpeg_layer3();
    let (url, _, server) = server(vec![response(body.clone(), "\"stopped-copy\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let mut request = job(
        &url,
        native_producer_helpers::create(
            grant
                .plan("stopped-tags.mp3", CollisionPolicy::Skip)
                .unwrap(),
            &context(),
        )
        .unwrap(),
        None,
        DownloadExecutionLease::from_approved_context(context()).unwrap(),
    );
    request.extension = "mp3".into();
    let handle = pool.submit(request).unwrap();
    let original = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Original audio must verify"),
    };
    let original_record = original.owned_record().clone();
    let cancel = DownloadCancel::default();
    let (original, copy) = native_producer_helpers::copy(original, cancel.clone()).unwrap();
    let mut copy = copy.unwrap();
    let copy_path = copy.path();
    let copy = lx_gpui_player::download::run_download_producer(move || {
        {
            let mut io = copy.rewrite_io(cancel.clone())?;
            cancel.stop(StopReason::Pause);
            let patch = LocalTagPatch {
                title: TagField::Set("禁止写入".into()),
                ..Default::default()
            };
            assert!(
                write_local_tags(&mut io, &patch, &ScanContext::new(Duration::from_secs(5)))
                    .is_err()
            );
        }
        Ok(copy)
    })
    .unwrap();
    assert_eq!(std::fs::read(copy_path).unwrap(), body);
    assert_eq!(
        std::fs::read(grant.path().join(original_record.basename)).unwrap(),
        body
    );
    native_producer_helpers::commit(original).unwrap();
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(copy);
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn cancellation_after_an_actual_tag_copy_write_preserves_the_good_original() {
    use lx_gpui_player::{
        download::{DownloadCancel, OwnedFileIo},
        library::{
            scan::ScanContext,
            tag_write::{EditableTagFile, LocalTagPatch, TagField, write_local_tags},
        },
    };
    struct StopAfterWrite<'a> {
        file: OwnedFileIo<'a>,
        cancel: DownloadCancel,
        wrote: bool,
    }
    impl Read for StopAfterWrite<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.file.read(bytes)
        }
    }
    impl std::io::Seek for StopAfterWrite<'_> {
        fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
            std::io::Seek::seek(&mut self.file, from)
        }
    }
    impl Write for StopAfterWrite<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let written = self.file.write(bytes)?;
            if written > 0 {
                self.wrote = true;
                self.cancel.stop(StopReason::Pause);
            }
            Ok(written)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }
    impl lofty::io::Length for StopAfterWrite<'_> {
        fn len(&self) -> std::io::Result<u64> {
            lofty::io::Length::len(&self.file)
        }
    }
    impl lofty::io::Truncate for StopAfterWrite<'_> {
        fn truncate(&mut self, bytes: u64) -> std::io::Result<()> {
            lofty::io::Truncate::truncate(&mut self.file, bytes)
        }
    }
    impl EditableTagFile for StopAfterWrite<'_> {
        fn check_write_authority(&self) -> std::io::Result<()> {
            self.file.check_write_authority()
        }
        fn sync_data(&mut self) -> std::io::Result<()> {
            self.file.sync_data()
        }
    }
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = encoded_audio_fixture::silent_mpeg_layer3();
    let (url, _, server) = server(vec![response(body.clone(), "\"partial-tag-copy\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let mut request = job(
        &url,
        native_producer_helpers::create(
            grant
                .plan("preserved-good.mp3", CollisionPolicy::Skip)
                .unwrap(),
            &context(),
        )
        .unwrap(),
        None,
        DownloadExecutionLease::from_approved_context(context()).unwrap(),
    );
    request.extension = "mp3".into();
    let handle = pool.submit(request).unwrap();
    let original = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Original audio must verify"),
    };
    let original_record = original.owned_record().clone();
    let checkpoint = original.checkpoint().clone();
    let cancel = DownloadCancel::default();
    let (original, copy) = native_producer_helpers::copy(original, cancel.clone()).unwrap();
    let mut copy = copy.unwrap();
    let copy = lx_gpui_player::download::run_download_producer(move || {
        {
            let mut io = StopAfterWrite {
                file: copy.rewrite_io(cancel.clone())?,
                cancel,
                wrote: false,
            };
            let patch = LocalTagPatch {
                title: TagField::Set("中断标签".repeat(2000)),
                ..Default::default()
            };
            assert!(
                write_local_tags(&mut io, &patch, &ScanContext::new(Duration::from_secs(5)))
                    .is_err()
            );
            assert!(
                io.wrote,
                "Failure must occur after an actual owned-copy write"
            );
        }
        Ok(copy)
    })
    .unwrap();
    assert_eq!(
        std::fs::read(grant.path().join(original_record.basename)).unwrap(),
        body
    );
    assert_eq!(original.checkpoint(), &checkpoint);
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(original).unwrap().path).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(copy);
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

fn full_or_continued(body: Vec<u8>, etag: &'static str, resume_start: usize) -> Handler {
    Box::new(move |socket, request| {
        if request.to_lowercase().contains("range:") {
            write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {resume_start}-{}/{}\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n", body.len()-1, body.len(), body.len()-resume_start).unwrap();
            socket.write_all(&body[resume_start..]).unwrap();
        } else {
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(&body).unwrap();
        }
    })
}

#[test]
fn newly_approved_other_url_with_same_etag_length_and_tail_resets_before_http() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let plan = grant
        .plan("origin-bound.wav", CollisionPolicy::Skip)
        .unwrap();
    let a = wav();
    let mut b = a.clone();
    b[100] ^= 0x20;
    assert_eq!(&a[4097..4107], &b[4097..4107]);
    assert_ne!(&a[..4107], &b[..4107]);
    let (url_a, requests_a, server_a) = server(vec![truncated(a, 4107, "\"same-across-origins\"")]);
    let (url_b, requests_b, server_b) = server(vec![full_or_continued(
        b.clone(),
        "\"same-across-origins\"",
        4097,
    )]);
    let pool = DownloadPool::new(1).unwrap();
    let first = pool
        .submit(job(
            &url_a,
            native_producer_helpers::create(plan.clone(), &context()).unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let (record, checkpoint) = partial(wait(&first));
    assert!(checkpoint.representation.is_some());
    let resumed = pool
        .submit(job(
            &url_b,
            native_producer_helpers::reopen(plan, record, &context()).unwrap(),
            Some(checkpoint),
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let audio = match wait(&resumed) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("New URL must fetch a whole independently approved representation"),
    };
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(audio).unwrap().path).unwrap(),
        b
    );
    assert!(!requests_b.recv().unwrap().to_lowercase().contains("range:"));
    assert_eq!(requests_a.try_iter().count(), 1);
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server_a.join().unwrap();
    server_b.join().unwrap();
    drop(first);
    drop(resumed);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn a_restored_checkpoint_without_representation_identity_restarts_its_owned_prefix() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let plan = grant
        .plan("legacy-identity.wav", CollisionPolicy::Skip)
        .unwrap();
    let body = wav();
    let (url, requests, server) = server(vec![
        truncated(body.clone(), 4107, "\"legacy\""),
        full_or_continued(body.clone(), "\"legacy\"", 4097),
    ]);
    let pool = DownloadPool::new(1).unwrap();
    let first = pool
        .submit(job(
            &url,
            native_producer_helpers::create(plan.clone(), &context()).unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let (record, checkpoint) = partial(wait(&first));
    let mut document = serde_json::to_value(checkpoint).unwrap();
    document.as_object_mut().unwrap().remove("representation");
    let restored: TransferCheckpoint = serde_json::from_value(document).unwrap();
    assert!(restored.representation.is_none());
    let resumed = pool
        .submit(job(
            &url,
            native_producer_helpers::reopen(plan, record, &context()).unwrap(),
            Some(restored),
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let audio = match wait(&resumed) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Legacy prefix without identity must restart safely"),
    };
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(audio).unwrap().path).unwrap(),
        body
    );
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].to_lowercase().contains("range:"));
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(first);
    drop(resumed);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn representation_serde_keeps_provenance_but_does_not_bind_to_an_execution_epoch() {
    use lx_gpui_player::download::{DownloadRepresentationIdentity, planning::DownloadQuality};
    let original = context();
    let url = "http://127.0.0.1:1/never?private=bound";
    let approved =
        ApprovedDownloadUrl::from_exact_approval(url, url, &original, &original).unwrap();
    let identity = DownloadRepresentationIdentity::from_approved_url(
        &approved,
        "22".repeat(32),
        DownloadQuality::Wav,
    )
    .unwrap();
    let restored: DownloadRepresentationIdentity =
        serde_json::from_str(&serde_json::to_string(&identity).unwrap()).unwrap();
    assert_eq!(restored, identity);
    restored.validate().unwrap();
    let mut resumed = original.clone();
    resumed.operation_epoch += 1;
    let reapproved =
        ApprovedDownloadUrl::from_exact_approval(url, url, &resumed, &resumed).unwrap();
    assert_eq!(
        DownloadRepresentationIdentity::from_approved_url(
            &reapproved,
            "22".repeat(32),
            DownloadQuality::Wav
        )
        .unwrap(),
        identity
    );
    resumed.policy_epoch += 1;
    let changed_policy =
        ApprovedDownloadUrl::from_exact_approval(url, url, &resumed, &resumed).unwrap();
    assert_ne!(
        DownloadRepresentationIdentity::from_approved_url(
            &changed_policy,
            "22".repeat(32),
            DownloadQuality::Wav
        )
        .unwrap(),
        identity
    );
}

#[test]
fn prepared_tagged_output_commits_a_distinct_verified_body_with_immutable_http_origin() {
    use lx_gpui_player::{
        download::DownloadCancel,
        library::tag_write::{LocalTagPatch, TagField},
    };
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = encoded_audio_fixture::mono_verbatim_flac();
    let (url, _, server) = server(vec![response(body.clone(), "\"sealed-flac\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("sealed.flac", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let original = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Original FLAC must verify"),
    };
    let checkpoint = original.checkpoint().clone();
    let original_id = original.owned_record().identity.clone();
    let (original, prepared) = native_producer_helpers::tagged(
        original,
        LocalTagPatch {
            title: TagField::Set("真实标题".into()),
            ..Default::default()
        },
        DownloadCancel::default(),
        Duration::from_secs(5),
    )
    .unwrap();
    let prepared = prepared.unwrap();
    assert_eq!(prepared.original_transfer(), &checkpoint);
    assert_eq!(prepared.verification(), original.verification());
    assert_ne!(prepared.owned_record().identity, original_id);
    assert_eq!(prepared.tags().body_sha256, prepared.output_digest().sha256);
    let committed =
        lx_gpui_player::download::run_download_producer(move || prepared.commit()).unwrap();
    assert_eq!(committed.original_transfer, checkpoint);
    assert_ne!(committed.output_digest.sha256, checkpoint.sha256);
    assert!(committed.output.checkpoint.representation.is_none());
    assert_eq!(
        std::fs::read(grant.path().join(&original.owned_record().basename)).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(original);
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn lyric_stage_writes_only_the_planned_stem_sidecar_without_changing_audio_or_metadata() {
    use lx_gpui_player::{
        domain::lyric::{LyricBundle, LyricField, LyricOrigin},
        download::{
            DownloadCancel,
            lyrics::{DownloadLyricOptions, LyricArtifactEncoding, encode_lyric_artifact},
        },
    };
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = wav();
    let (url, _, server) = server(vec![response(body.clone(), "\"sidecar-audio\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("有词歌曲.wav", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let original = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Original WAV must verify"),
    };
    let checkpoint = original.checkpoint().clone();
    let bundle = LyricBundle {
        lyric: LyricField::Text("[00:00.10]中文歌词".into()),
        ..Default::default()
    };
    let artifact = encode_lyric_artifact(
        &bundle,
        DownloadLyricOptions::default(),
        LyricArtifactEncoding::Utf8,
        LyricOrigin::Edited,
        7,
    )
    .unwrap();
    let (original, prepared) = native_producer_helpers::sidecar(
        original,
        artifact.clone(),
        CollisionPolicy::Skip,
        DownloadCancel::default(),
    )
    .unwrap();
    let prepared = prepared.unwrap();
    assert_eq!(prepared.owned_record().final_basename, "有词歌曲.lrc");
    let digest = prepared.output_digest().clone();
    let committed =
        lx_gpui_player::download::run_download_producer(move || prepared.commit()).unwrap();
    assert_eq!(committed.sha256, digest.sha256);
    assert_eq!(std::fs::read(committed.path).unwrap(), artifact.bytes);
    assert_eq!(original.checkpoint(), &checkpoint);
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(original).unwrap().path).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn group_derivation_keeps_selected_root_identity_and_inherits_its_revocation() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    // A standalone fixture repository admits a newly selected destination at
    // epoch one; later epochs require a persisted authority transition.
    let selected = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let group = selected.derive_group("正式歌单").unwrap();
    assert_eq!(group.identity(), selected.identity());
    assert_ne!(
        group.working_directory_identity(),
        selected.working_directory_identity()
    );
    assert_eq!(group.epoch(), 1);
    assert!(group.path().is_dir());
    assert!(group.derive_group("nested").is_err());
    assert!(selected.derive_group("../outside").is_err());
    let plan = group.plan("group.wav", CollisionPolicy::Skip).unwrap();
    let part = native_producer_helpers::create(plan, &context()).unwrap();
    assert_eq!(&part.record().destination_identity, selected.identity());
    assert_eq!(part.path().parent(), Some(group.path()));
    selected.revoke();
    assert!(group.verify().is_err());
    assert!(part.verify().is_err());
    assert_eq!(std::fs::metadata(part.path()).unwrap().len(), 0);
    drop(part);
    drop(selected);
    native_producer_helpers::finish_directory(directory.path(), group).unwrap();
}

#[test]
fn retained_metadata_and_sidecar_candidates_cannot_publish_after_stage_cancel_or_collision() {
    use lx_gpui_player::{
        domain::lyric::{LyricBundle, LyricField, LyricOrigin},
        download::{
            DownloadCancel,
            lyrics::{DownloadLyricOptions, LyricArtifactEncoding, encode_lyric_artifact},
        },
        library::tag_write::{LocalTagPatch, TagField},
    };
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = encoded_audio_fixture::silent_mpeg_layer3();
    let (url, _, server) = server(vec![response(body.clone(), "\"retained-stage\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grant.plan("retained.mp3", CollisionPolicy::Skip).unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let original = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Original MP3 must verify"),
    };
    let stage_cancel = DownloadCancel::default();
    let (original, prepared) = native_producer_helpers::tagged(
        original,
        LocalTagPatch {
            title: TagField::Set("标签".into()),
            ..Default::default()
        },
        stage_cancel.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    let prepared = prepared.unwrap();
    stage_cancel.stop(StopReason::Pause);
    assert!(lx_gpui_player::download::run_download_producer(move || prepared.commit()).is_err());
    assert!(!grant.path().join("retained.mp3").exists());
    let artifact = encode_lyric_artifact(
        &LyricBundle {
            lyric: LyricField::Text("[00:00.10]词".into()),
            ..Default::default()
        },
        DownloadLyricOptions::default(),
        LyricArtifactEncoding::Utf8,
        LyricOrigin::Raw,
        1,
    )
    .unwrap();
    let sidecar_cancel = DownloadCancel::default();
    let (original, sidecar) = native_producer_helpers::sidecar(
        original,
        artifact.clone(),
        CollisionPolicy::Skip,
        sidecar_cancel.clone(),
    )
    .unwrap();
    let sidecar = sidecar.unwrap();
    std::fs::write(grant.path().join("retained.lrc"), b"existing-user-sidecar").unwrap();
    assert!(lx_gpui_player::download::run_download_producer(move || sidecar.commit()).is_err());
    assert_eq!(
        std::fs::read(grant.path().join("retained.lrc")).unwrap(),
        b"existing-user-sidecar"
    );
    let (original, failed_sidecar) = native_producer_helpers::sidecar(
        original,
        artifact,
        CollisionPolicy::Skip,
        DownloadCancel::default(),
    )
    .unwrap();
    assert!(failed_sidecar.is_err());
    assert_eq!(
        std::fs::read(native_producer_helpers::commit(original).unwrap().path).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn scoped_small_field_reads_keep_native_proofs_within_the_tag_fixture_deadline() {
    use lx_gpui_player::download::DownloadCancel;
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let mut part = native_producer_helpers::create(
        grant
            .plan("field-read-benchmark.mp3", CollisionPolicy::Skip)
            .unwrap(),
        &context(),
    )
    .unwrap();
    let cancel = DownloadCancel::default();
    let started = Instant::now();
    let part = lx_gpui_player::download::run_download_producer(move || {
        {
            let mut io = part.rewrite_io(cancel)?;
            io.write_all(&[0x5a; 16 * 1024])?;
            std::io::Seek::seek(&mut io, std::io::SeekFrom::Start(0))?;
            let mut byte = [0u8; 1];
            for _ in 0..16 * 1024 {
                io.read_exact(&mut byte)?;
                assert_eq!(byte, [0x5a]);
            }
        }
        Ok(part)
    })
    .unwrap();
    part.verify().unwrap();
    eprintln!(
        "guarded single-byte reads=16384 elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(part);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn artifact_ownership_acknowledges_actual_zero_files_before_tag_copy_and_lyric_writes() {
    use lx_gpui_player::{
        domain::lyric::{LyricBundle, LyricField, LyricOrigin},
        download::{
            DownloadCancel,
            artifact::{
                OwnedArtifactAllocation, OwnedArtifactIntent, OwnedArtifactJournal,
                OwnedArtifactKind,
            },
            lyrics::{DownloadLyricOptions, LyricArtifactEncoding, encode_lyric_artifact},
            postprocess::{prepare_lyric_sidecar_journaled, prepare_tagged_output_journaled},
        },
        library::tag_write::{LocalTagPatch, TagField},
    };
    struct Journal {
        base: Arc<native_producer_helpers::RepositoryJournal>,
        rows: std::sync::Mutex<Vec<OwnedArtifactAllocation>>,
    }
    impl OwnedArtifactJournal for Journal {
        fn record_intent(&self, intent: &OwnedArtifactIntent) -> anyhow::Result<()> {
            self.base.record_intent(intent)
        }
        fn record_reopened(&self, allocation: &OwnedArtifactAllocation) -> anyhow::Result<()> {
            self.base.record_reopened(allocation)
        }
        fn record_created(&self, allocation: &OwnedArtifactAllocation) -> anyhow::Result<()> {
            allocation.validate()?;
            assert_eq!(
                std::fs::metadata(
                    allocation
                        .working_directory_path
                        .join(&allocation.record.basename)
                )?
                .len(),
                0
            );
            self.base.record_created(allocation)?;
            self.rows.lock().unwrap().push(allocation.clone());
            Ok(())
        }
    }
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let grouped = grant.derive_group("归档歌单").unwrap();
    let body = encoded_audio_fixture::mono_verbatim_flac();
    let (url, _, server) = server(vec![response(body.clone(), "\"journal-before-write\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(
                grouped
                    .plan("journaled.flac", CollisionPolicy::Skip)
                    .unwrap(),
                &context(),
            )
            .unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let original = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Original FLAC must verify"),
    };
    let journal = Arc::new(Journal {
        base: native_producer_helpers::journal(original.owned_record()).unwrap(),
        rows: std::sync::Mutex::new(Vec::new()),
    });
    let cancel = DownloadCancel::default();
    let tag_journal = journal.clone();
    let tag_cancel = cancel.clone();
    let (original, tagged) = lx_gpui_player::download::run_download_producer(move || {
        let tagged = prepare_tagged_output_journaled(
            &original,
            &LocalTagPatch {
                title: TagField::Set("有日志的标题".into()),
                ..Default::default()
            },
            &tag_cancel,
            Duration::from_secs(5),
            tag_journal.as_ref(),
        )?;
        Ok((original, tagged))
    })
    .unwrap();
    let artifact = encode_lyric_artifact(
        &LyricBundle {
            lyric: LyricField::Text("[00:00.10]歌词".into()),
            ..Default::default()
        },
        DownloadLyricOptions::default(),
        LyricArtifactEncoding::Utf8,
        LyricOrigin::Raw,
        1,
    )
    .unwrap();
    let sidecar_journal = journal.clone();
    let work_artifact = artifact.clone();
    let (original, sidecar) = lx_gpui_player::download::run_download_producer(move || {
        let sidecar = prepare_lyric_sidecar_journaled(
            &original,
            &work_artifact,
            CollisionPolicy::Skip,
            &cancel,
            sidecar_journal.as_ref(),
        )?;
        Ok((original, sidecar))
    })
    .unwrap();
    let rows = journal.rows.lock().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, OwnedArtifactKind::TagCopy);
    assert_eq!(rows[1].kind, OwnedArtifactKind::LyricSidecar);
    assert_eq!(rows[0].relative_group.as_deref(), Some("归档歌单"));
    assert_eq!(rows[0].selected_root_identity, *grant.identity());
    assert_eq!(
        rows[0].working_directory_identity,
        *grouped.working_directory_identity()
    );
    drop(rows);
    lx_gpui_player::download::run_download_producer(move || tagged.commit()).unwrap();
    assert_eq!(
        std::fs::read(
            lx_gpui_player::download::run_download_producer(move || sidecar.commit())
                .unwrap()
                .path
        )
        .unwrap(),
        artifact.bytes
    );
    assert_eq!(
        std::fs::read(grouped.path().join(&original.owned_record().basename)).unwrap(),
        body
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(original);
    drop(handle);
    drop(pool);
    drop(grant);
    native_producer_helpers::finish_directory(directory.path(), grouped).unwrap();
}

#[test]
fn rejected_creation_journal_cannot_write_a_shadow_and_restart_clears_only_owned_prefix() {
    use lx_gpui_player::{
        download::{
            DownloadCancel,
            artifact::{OwnedArtifactAllocation, OwnedArtifactIntent, OwnedArtifactJournal},
        },
        library::tag_write::{LocalTagPatch, TagField},
    };
    struct Rejected {
        base: Arc<native_producer_helpers::RepositoryJournal>,
        observed: std::sync::Mutex<Option<OwnedArtifactAllocation>>,
    }
    impl OwnedArtifactJournal for Rejected {
        fn record_intent(&self, intent: &OwnedArtifactIntent) -> anyhow::Result<()> {
            self.base.record_intent(intent)
        }
        fn record_reopened(&self, allocation: &OwnedArtifactAllocation) -> anyhow::Result<()> {
            self.base.record_reopened(allocation)
        }
        fn record_created(&self, allocation: &OwnedArtifactAllocation) -> anyhow::Result<()> {
            self.observed.lock().unwrap().replace(allocation.clone());
            self.base.record_created(allocation)
        }
    }
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = encoded_audio_fixture::mono_verbatim_flac();
    let (url, _, server) = server(vec![response(body.clone(), "\"journal-reject\"")]);
    let pool = DownloadPool::new(1).unwrap();
    let plan = grant
        .plan("preserved-restart.flac", CollisionPolicy::Skip)
        .unwrap();
    let handle = pool
        .submit(job(
            &url,
            native_producer_helpers::create(plan.clone(), &context()).unwrap(),
            None,
            DownloadExecutionLease::from_approved_context(context()).unwrap(),
        ))
        .unwrap();
    let original = match wait(&handle) {
        TransferOutcome::Verified(audio) => audio,
        _ => panic!("Original FLAC must verify"),
    };
    let base = native_producer_helpers::journal(original.owned_record()).unwrap();
    let database = rusqlite::Connection::open(base.repository.path()).unwrap();
    database.execute_batch("CREATE TRIGGER reject_fixture_creation BEFORE INSERT ON download_owned_artifacts BEGIN SELECT RAISE(ABORT,'creation journal rejected'); END;").unwrap();
    let journal = Arc::new(Rejected {
        base,
        observed: std::sync::Mutex::new(None),
    });
    let write_journal = journal.clone();
    let (original, failed) = lx_gpui_player::download::run_download_producer(move || {
        let prepared = lx_gpui_player::download::postprocess::prepare_tagged_output_journaled(
            &original,
            &LocalTagPatch {
                title: TagField::Set("禁止写入".into()),
                ..Default::default()
            },
            &DownloadCancel::default(),
            Duration::from_secs(5),
            write_journal.as_ref(),
        );
        Ok((original, prepared))
    })
    .unwrap();
    assert!(failed.is_err());
    database
        .execute_batch("DROP TRIGGER reject_fixture_creation;")
        .unwrap();
    let shadow = journal.observed.lock().unwrap().clone().unwrap();
    assert_eq!(
        std::fs::metadata(shadow.working_directory_path.join(&shadow.record.basename))
            .unwrap()
            .len(),
        0
    );
    let record = original.owned_record().clone();
    assert_eq!(
        std::fs::read(grant.path().join(&record.basename)).unwrap(),
        body
    );
    drop(original);
    std::fs::write(grant.path().join("unrelated-user.flac"), b"user-owned-file").unwrap();
    let mut part = native_producer_helpers::reopen(plan, record, &context()).unwrap();
    let (part, checkpoint) = lx_gpui_player::download::run_download_producer(move || {
        let checkpoint = part.restart_for_representation(&DownloadCancel::default())?;
        Ok((part, checkpoint))
    })
    .unwrap();
    assert_eq!(checkpoint.bytes, 0);
    assert!(checkpoint.strong_etag.is_none());
    assert!(checkpoint.remote_total.is_none());
    assert!(checkpoint.representation.is_none());
    assert!(!checkpoint.completed_body);
    assert_eq!(
        std::fs::read(grant.path().join("unrelated-user.flac")).unwrap(),
        b"user-owned-file"
    );
    pool.shutdown(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    drop(part);
    drop(handle);
    drop(pool);
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn readonly_recovery_verifies_only_exact_selected_artifact_identity_length_and_sha() {
    use lx_gpui_player::download::DownloadCancel;
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let body = wav();
    // A closed file models persisted output after process exit. Live download
    // writers are deliberately retained through repository ACK and adoption.
    let output_path = grant.path().join("recover.wav");
    std::fs::write(&output_path, &body).unwrap();
    let identity = native_producer_helpers::file_identity(&output_path);
    let expected_bytes = body.len() as u64;
    let expected_sha256 = format!("{:x}", Sha256::digest(&body));
    let proof = grant
        .verify_owned_artifact(
            "recover.wav",
            &identity,
            expected_bytes,
            &expected_sha256,
            &DownloadCancel::default(),
        )
        .unwrap();
    assert_eq!(proof.basename(), "recover.wav");
    assert_eq!(proof.identity(), &identity);
    assert_eq!(proof.bytes(), body.len() as u64);
    assert_eq!(proof.sha256(), expected_sha256);
    let mut wrong_identity = identity.clone();
    wrong_identity.object = "ff".repeat(16);
    assert!(
        grant
            .verify_owned_artifact(
                "recover.wav",
                &wrong_identity,
                expected_bytes,
                &expected_sha256,
                &DownloadCancel::default()
            )
            .is_err()
    );
    assert!(
        grant
            .verify_owned_artifact(
                "recover.wav",
                &identity,
                expected_bytes + 1,
                &expected_sha256,
                &DownloadCancel::default()
            )
            .is_err()
    );
    assert!(
        grant
            .verify_owned_artifact(
                "recover.wav",
                &identity,
                expected_bytes,
                &"ff".repeat(32),
                &DownloadCancel::default()
            )
            .is_err()
    );
    let cancel = DownloadCancel::default();
    cancel.stop(StopReason::Cancel);
    assert!(
        grant
            .verify_owned_artifact(
                "recover.wav",
                &identity,
                expected_bytes,
                &expected_sha256,
                &cancel
            )
            .is_err()
    );
    assert!(
        grant
            .verify_owned_artifact(
                "../recover.wav",
                &identity,
                expected_bytes,
                &expected_sha256,
                &DownloadCancel::default()
            )
            .is_err()
    );
    assert_eq!(std::fs::read(&output_path).unwrap(), body);
    grant.revoke();
    assert!(
        grant
            .verify_owned_artifact(
                "recover.wav",
                &identity,
                expected_bytes,
                &expected_sha256,
                &DownloadCancel::default()
            )
            .is_err()
    );
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
#[cfg(windows)]
fn removal_receipt_follows_actual_original_handle_close_and_keeps_user_files() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    std::fs::write(grant.path().join("user-owned.mp3"), b"preserve-user-file").unwrap();
    let part = native_producer_helpers::create(
        grant
            .plan("not-published.mp3", CollisionPolicy::Skip)
            .unwrap(),
        &context(),
    )
    .unwrap();
    let record = part.record().clone();
    let path = part.path();
    let receipt = native_producer_helpers::remove(part).unwrap();
    assert_eq!(receipt, record);
    assert!(!path.exists());
    assert_eq!(
        std::fs::read(grant.path().join("user-owned.mp3")).unwrap(),
        b"preserve-user-file"
    );
    let part = native_producer_helpers::create(
        grant
            .plan("revoked-removal.mp3", CollisionPolicy::Skip)
            .unwrap(),
        &context(),
    )
    .unwrap();
    let path = part.path();
    grant.revoke();
    assert!(native_producer_helpers::remove(part).is_err());
    assert!(path.exists());
    native_producer_helpers::finish_directory(directory.path(), grant).unwrap();
}

#[test]
fn readonly_group_recovery_does_not_create_a_missing_directory() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let missing = grant.path().join("missing-recovery-group");
    assert!(!missing.exists());
    assert!(grant.open_existing_group("missing-recovery-group").is_err());
    assert!(!missing.exists());
    std::fs::create_dir(grant.path().join("existing-recovery-group")).unwrap();
    let group = grant
        .open_existing_group("existing-recovery-group")
        .unwrap();
    assert_eq!(group.identity(), grant.identity());
    assert_ne!(
        group.working_directory_identity(),
        grant.working_directory_identity()
    );
    grant.revoke();
    assert!(group.verify().is_err());
    drop(grant);
    native_producer_helpers::finish_directory(directory.path(), group).unwrap();
}
