use lx_gpui_player::{
    audio::{
        PlaybackStatus,
        cache::{CacheOptions, CacheStore},
    },
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope,
    },
};
use std::{
    fs,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
#[path = "support/cache_lock.rs"]
mod cache_lock;
use cache_lock::{
    CASES, RegistryLock, assert_p95, fresh_directory, logical_bytes, measured,
    measured_with_watchdog, seed, timed, wait_initialized, wait_registry_wait, wait_worker_count,
};

#[test]
fn cache_registry_lock_child_fixture() {
    cache_lock::fixture_child();
}

fn execute(
    backend: &BackendHandle,
    id: &str,
    command: BackendCommand,
) -> lx_gpui_player::backend::CommandReceipt {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "cache-latency-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap()
}
fn stats(backend: &BackendHandle) -> Option<lx_gpui_player::audio::cache::CacheStats> {
    match backend.query(BackendQuery::AudioCacheStats).unwrap() {
        BackendQueryResult::AudioCacheStats(stats) => stats,
        _ => unreachable!(),
    }
}

#[test]
fn backend_startup_and_metadata_queries_stay_responsive_during_real_cache_bootstrap_wait() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    wait_worker_count(0);
    let directory = fresh_directory("backend-init-");
    let profile = directory.path().join("profile");
    let root = profile.join("audio-cache");
    seed(&root);
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let configured = BackendConfig::headless(profile.clone());
    let (backend, startup) = measured_with_watchdog(&mut lock, Duration::from_secs(5), move || {
        BackendHandle::start(configured).unwrap()
    });
    assert!(
        startup < Duration::from_secs(5),
        "backend initialization exceeded its deadline while the registry remained locked: {startup:?}"
    );
    lock.assert_held();
    eprintln!(
        "backend cold-start elapsed: {:.3} ms; registry OS lock remains held",
        startup.as_secs_f64() * 1000.0
    );
    wait_registry_wait(|| stats(&backend));
    execute(
        &backend,
        "history",
        BackendCommand::RecordSearch {
            keyword: "preserved latency query".into(),
        },
    );
    let mut snapshots = Vec::new();
    let mut cache_queries = Vec::new();
    let mut metadata_queries = Vec::new();
    let mut cursor_queries = Vec::new();
    for _ in 0..20 {
        let owned = backend.clone();
        let (snapshot, elapsed) = measured(&mut lock, move || owned.snapshot());
        snapshots.push(elapsed);
        assert!(!snapshot.stopped);
        let owned = backend.clone();
        let (cached, elapsed) = measured(&mut lock, move || stats(&owned));
        cache_queries.push(elapsed);
        assert!(cached.unwrap().io_waiting_for_registry);
        let owned = backend.clone();
        let (history, elapsed) = measured(&mut lock, move || {
            owned.query(BackendQuery::SearchHistory).unwrap()
        });
        metadata_queries.push(elapsed);
        let BackendQueryResult::SearchHistory(history) = history else {
            panic!();
        };
        assert_eq!(history, ["preserved latency query"]);
        let owned = backend.clone();
        let ((snapshot, cursor), elapsed) =
            measured(&mut lock, move || owned.snapshot_with_cursor().unwrap());
        cursor_queries.push(elapsed);
        assert!(!snapshot.stopped);
        assert!(backend.events_after(cursor).is_ok());
    }
    assert_p95(snapshots, "Backend snapshot during cache bootstrap");
    assert_p95(cache_queries, "Backend cache query during cache bootstrap");
    assert_p95(
        metadata_queries,
        "Backend metadata query during cache bootstrap",
    );
    assert_p95(
        cursor_queries,
        "Backend atomic snapshot cursor during cache bootstrap",
    );
    lock.assert_held();
    lock.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !stats(&backend).is_some_and(|stats| stats.initialized && !stats.initializing) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let (result, elapsed) = timed(|| backend.shutdown(Duration::from_secs(5)));
    result.unwrap();
    assert!(elapsed < Duration::from_secs(5));
    let observer = CacheStore::new(CacheOptions {
        root: root.clone(),
        ..Default::default()
    })
    .unwrap();
    wait_initialized(&observer);
    let audit = observer
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(audit.blocks, 0);
    assert_eq!(audit.encoded_bytes, 0);
    assert_eq!(audit.disk_bytes, logical_bytes(&root));
    assert!(observer.shutdown(Duration::from_secs(5)));
    wait_worker_count(0);
}

#[test]
fn backend_seek_stop_and_snapshot_services_do_not_wait_for_an_accepted_cache_clear_io_job() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    wait_worker_count(0);
    let directory = fresh_directory("backend-clear-");
    let profile = directory.path().join("profile");
    let root = profile.join("audio-cache");
    let backend = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    let preserved = profile.join("original-user.lrc");
    fs::write(&preserved, b"[00:00.000]Original preserved lyric").unwrap();
    execute(
        &backend,
        "history",
        BackendCommand::RecordSearch {
            keyword: "preserved latency query".into(),
        },
    );
    let track = backend.snapshot().tracks[0].id.clone();
    execute(&backend, "play", BackendCommand::Play { id: track });
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.snapshot().playback.status != PlaybackStatus::Playing
        || !stats(&backend).is_some_and(|stats| stats.blocks > 0)
    {
        assert!(
            Instant::now() < deadline,
            "Playback and durable cache fixture did not become ready"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(stats(&backend).unwrap().blocks > 0);
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let cursor = backend.event_cursor();
    let receipt = execute(&backend, "clear", BackendCommand::ClearAudioCache);
    assert_eq!(
        receipt.state,
        lx_gpui_player::backend::ReceiptState::Accepted
    );
    wait_registry_wait(|| stats(&backend));
    let mut seeks = Vec::new();
    let mut snapshots = Vec::new();
    let mut metadata = Vec::new();
    let mut cache_queries = Vec::new();
    for index in 0..20 {
        let owned = backend.clone();
        let (_, elapsed) = measured(&mut lock, move || {
            execute(
                &owned,
                &format!("seek-{index}"),
                BackendCommand::Seek {
                    seconds: 0.1 + index as f64 * 0.01,
                },
            )
        });
        seeks.push(elapsed);
        let owned = backend.clone();
        let (_, elapsed) = measured(&mut lock, move || owned.snapshot());
        snapshots.push(elapsed);
        let owned = backend.clone();
        let (lists, elapsed) = measured(&mut lock, move || {
            owned.query(BackendQuery::ListInfos).unwrap()
        });
        metadata.push(elapsed);
        assert!(matches!(lists, BackendQueryResult::ListInfos(_)));
        let owned = backend.clone();
        let (cached, elapsed) = measured(&mut lock, move || stats(&owned));
        cache_queries.push(elapsed);
        assert!(cached.unwrap().io_waiting_for_registry);
    }
    assert_p95(seeks, "Backend Seek acknowledgment");
    assert_p95(snapshots, "Backend snapshot during clear");
    assert_p95(metadata, "Backend list queries during clear");
    assert_p95(cache_queries, "Backend cache statistics during clear");
    assert!(!backend.events_after(cursor).unwrap().iter().any(|event| matches!(&event.payload, BackendEvent::OperationFinished { operation_id, .. } if operation_id == &receipt.operation_id)), "a cache clear must not report a terminal audit while its registry is still locked");
    let owned = backend.clone();
    let (_, elapsed) = measured(&mut lock, move || {
        execute(&owned, "stop", BackendCommand::Stop)
    });
    assert!(elapsed < Duration::from_millis(100));
    let applied = Instant::now();
    while backend.snapshot().playback.status != PlaybackStatus::Idle {
        assert!(
            applied.elapsed() < Duration::from_millis(100),
            "Stop did not reach the audio engine within the control deadline"
        );
        thread::sleep(Duration::from_millis(1));
    }
    lock.assert_held();
    lock.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let terminal = backend
            .events_after(cursor)
            .unwrap()
            .into_iter()
            .find_map(|event| {
                if let BackendEvent::OperationFinished {
                    operation_id,
                    success,
                    error,
                } = event.payload
                    && operation_id == receipt.operation_id
                {
                    Some((success, error))
                } else {
                    None
                }
            });
        if let Some((success, error)) = terminal {
            assert!(success, "ownership audit failed after release: {error:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "accepted clear did not finish after the real filesystem lock was released"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fs::read(preserved).unwrap(),
        b"[00:00.000]Original preserved lyric"
    );
    let BackendQueryResult::SearchHistory(history) =
        backend.query(BackendQuery::SearchHistory).unwrap()
    else {
        panic!();
    };
    assert_eq!(history, ["preserved latency query"]);
    let (outcome, elapsed) = timed(|| backend.shutdown(Duration::from_secs(5)));
    outcome.unwrap();
    assert!(elapsed < Duration::from_secs(5));
    let observer: Arc<CacheStore> = CacheStore::new(CacheOptions {
        root: root.clone(),
        ..Default::default()
    })
    .unwrap();
    wait_initialized(&observer);
    let audit = observer
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(audit.blocks, 0);
    assert_eq!(audit.encoded_bytes, 0);
    assert_eq!(audit.retiring_blocks, 0);
    assert!(!audit.cannot_clear);
    assert_eq!(audit.disk_bytes, logical_bytes(&root));
    assert!(audit.disk_bytes <= 512 * 1024 * 1024);
    assert!(observer.shutdown(Duration::from_secs(5)));
    wait_worker_count(0);
}
