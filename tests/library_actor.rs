#![cfg(any(windows, target_os = "linux", target_os = "macos"))]
use lx_gpui_player::catalog::Catalog;
use lx_gpui_player::domain::library::*;
use lx_gpui_player::library::actor::{LibraryActor, LibraryCompletion};
use lx_gpui_player::library::authority::{AuthorityRegistry, RootAuthority, RootHealth};
use lx_gpui_player::library::scan::ScanContext;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

static CASES: Mutex<()> = Mutex::new(());
fn context() -> ScanContext {
    ScanContext::new(Duration::from_secs(20))
}
fn wav(path: &Path) {
    let frames = 2205u32;
    let mut bytes = Vec::with_capacity(44 + frames as usize * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + frames * 2).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&22050u32.to_le_bytes());
    bytes.extend_from_slice(&44100u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(frames * 2).to_le_bytes());
    for frame in 0..frames {
        let sample = if frame % 20 < 10 { 3000i16 } else { -3000 };
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}
fn root(path: &Path, index: usize) -> LibraryRoot {
    let authority = RootAuthority::open(path, true, &context()).unwrap();
    let barrier = authority.retirement_barrier();
    let draft = authority.draft();
    drop(authority);
    assert!(barrier.wait(Duration::from_secs(3)));
    LibraryRoot {
        root_id: format!("root:{index}"),
        path: draft.path,
        recursive: true,
        grant_epoch: 1,
        anchor: draft.anchor,
        path_policy: draft.path_policy,
        watch_enabled: false,
        generation: 1,
        last_complete_generation: 0,
    }
}
fn token(root: &LibraryRoot) -> LibraryScanToken {
    LibraryScanToken {
        root_id: root.root_id.clone(),
        grant_epoch: root.grant_epoch,
        generation: root.generation,
        operation_id: format!("scan:{}", root.root_id),
    }
}
fn completion(actor: &mut LibraryActor) -> LibraryCompletion {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(completion) = actor.try_recv() {
            return completion;
        }
        assert!(Instant::now() < deadline, "Library completion timed out");
        thread::sleep(Duration::from_millis(2));
    }
}
fn assert_no_completion(actor: &mut LibraryActor) {
    let deadline = Instant::now() + Duration::from_millis(80);
    while Instant::now() < deadline {
        assert!(
            actor.try_recv().is_none(),
            "Worker advanced without batch acknowledgement"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn seen_ack_precedes_preparation_and_prepared_guard_lives_until_ack() {
    let _case = CASES.lock().unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("source.wav");
    wav(&path);
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    let selected = root(temp.path(), 1);
    let token = token(&selected);
    actor
        .submit_scan(
            token.clone(),
            selected,
            LibraryScanOptions::default(),
            context(),
        )
        .unwrap();
    let first = completion(&mut actor);
    let LibraryCompletion::Batch {
        seen,
        prepared,
        ack,
        _storage,
        ..
    } = first
    else {
        panic!("Expected discovery batch");
    };
    assert_eq!(seen.len(), 1);
    assert!(seen[0].asset_id.is_none());
    assert!(prepared.is_empty());
    assert_no_completion(&mut actor);
    assert!(ack.complete(Ok(())));
    drop(_storage);
    let second = completion(&mut actor);
    let LibraryCompletion::Batch {
        seen,
        prepared,
        ack,
        _storage,
        ..
    } = second
    else {
        panic!("Expected prepared batch");
    };
    assert_eq!(prepared.len(), 1);
    assert_eq!(
        seen[0].asset_id.as_deref(),
        Some(prepared[0].1.asset.id.as_str())
    );
    assert!(prepared[0].1.file_identity.is_some());
    assert_eq!(prepared[0].0.track().title, "source");
    #[cfg(windows)]
    assert!(
        fs::OpenOptions::new().write(true).open(&path).is_err(),
        "Unacknowledged verified file guard must reject writes"
    );
    assert_no_completion(&mut actor);
    assert!(ack.complete(Ok(())));
    drop(_storage);
    let LibraryCompletion::Finished {
        outcome,
        discovery_complete,
        ..
    } = completion(&mut actor)
    else {
        panic!("Expected scan finish");
    };
    assert!(discovery_complete);
    assert_eq!(outcome.state, LibraryOperationState::Completed);
    assert_eq!(outcome.prepared_files, 1);
    assert_eq!(outcome.entries_examined, 1);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert!(fs::OpenOptions::new().write(true).open(path).is_ok());
}
#[test]
fn cancellation_stops_the_worker_but_delivered_proof_lives_until_controller_rejection() {
    let _case = CASES.lock().unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("source.wav");
    wav(&path);
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    let selected = root(temp.path(), 2);
    let token = token(&selected);
    actor
        .submit_scan(
            token.clone(),
            selected,
            LibraryScanOptions::default(),
            context(),
        )
        .unwrap();
    let LibraryCompletion::Batch { ack, .. } = completion(&mut actor) else {
        panic!("Expected discovery batch");
    };
    assert!(ack.complete(Ok(())));
    let pending = completion(&mut actor);
    assert!(matches!(&pending, LibraryCompletion::Batch { prepared, .. } if prepared.len() == 1));
    assert!(actor.cancel(&token.operation_id));
    let LibraryCompletion::Finished {
        outcome,
        discovery_complete,
        ..
    } = completion(&mut actor)
    else {
        panic!("Expected cancelled finish");
    };
    assert_eq!(outcome.state, LibraryOperationState::Cancelled);
    assert!(!discovery_complete);
    #[cfg(windows)]
    assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
    let LibraryCompletion::Batch { ack, _storage, .. } = pending else {
        unreachable!()
    };
    assert!(!ack.complete(Ok(())));
    assert!(fs::OpenOptions::new().write(true).open(path).is_ok());
    drop(_storage);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}
#[test]
fn retained_preparation_factories_do_not_keep_scanning_directory_cache_alive() {
    let _case = CASES.lock().unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("source.wav");
    wav(&path);
    let selected = root(temp.path(), 20);
    let registry = AuthorityRegistry::default();
    let mut actor =
        LibraryActor::new_with_authority(Catalog::new().unwrap(), registry.clone()).unwrap();
    let scan = token(&selected);
    let scan_context = context();
    actor
        .submit_scan(
            scan,
            selected,
            LibraryScanOptions::default(),
            scan_context.clone(),
        )
        .unwrap();
    let LibraryCompletion::Batch { ack, .. } = completion(&mut actor) else {
        panic!("Expected seen batch");
    };
    assert!(ack.complete(Ok(())));
    let LibraryCompletion::Batch {
        prepared,
        ack,
        _storage,
        ..
    } = completion(&mut actor)
    else {
        panic!("Expected prepared batch");
    };
    assert_eq!(prepared.len(), 1);
    assert!(ack.complete(Ok(())));
    let LibraryCompletion::Finished {
        outcome,
        discovery_complete,
        ..
    } = completion(&mut actor)
    else {
        panic!("Expected completed scan");
    };
    assert_eq!(outcome.state, LibraryOperationState::Completed);
    assert!(discovery_complete);
    assert!(
        !scan_context
            .cancel
            .load(std::sync::atomic::Ordering::Acquire),
        "Finalizer context must remain usable after worker scope retirement"
    );
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(registry.retirement_pending(), 0);
    assert_eq!(prepared[0].0.track().title, "source");
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
    // The caller still owns the actual metadata/factory objects across shutdown.
    drop(prepared);
    drop(_storage);
}
#[test]
fn two_workers_and_eight_queued_jobs_have_exact_admission_and_real_join() {
    let _case = CASES.lock().unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    let mut roots = Vec::new();
    for index in 0..11 {
        let path = temp.path().join(format!("root{index}"));
        fs::create_dir(&path).unwrap();
        wav(&path.join("source.wav"));
        roots.push(root(&path, index));
    }
    for selected in &roots[..2] {
        actor
            .submit_scan(
                token(selected),
                selected.clone(),
                LibraryScanOptions::default(),
                context(),
            )
            .unwrap();
    }
    let first = completion(&mut actor);
    let second = completion(&mut actor);
    assert!(matches!(first, LibraryCompletion::Batch { .. }));
    assert!(matches!(second, LibraryCompletion::Batch { .. }));
    for selected in &roots[2..10] {
        actor.can_submit_scan(&selected.root_id).unwrap();
        actor
            .submit_scan(
                token(selected),
                selected.clone(),
                LibraryScanOptions::default(),
                context(),
            )
            .unwrap();
    }
    assert_eq!(actor.active_jobs(), 10);
    assert!(actor.can_submit_scan(&roots[10].root_id).is_err());
    assert!(
        actor
            .submit_scan(
                token(&roots[10]),
                roots[10].clone(),
                LibraryScanOptions::default(),
                context()
            )
            .is_err()
    );
    assert_eq!(actor.active_jobs(), 10);
    // The caller retains both ACKs: cancellation must still unblock the owners.
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(actor.active_jobs(), 0);
    drop(first);
    drop(second);
}
#[test]
fn corrupt_audio_still_has_seen_path_and_scope_complete_partial_outcome() {
    let _case = CASES.lock().unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    fs::write(temp.path().join("bad.wav"), b"not a waveform").unwrap();
    wav(&temp.path().join("good.wav"));
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    let selected = root(temp.path(), 3);
    actor
        .submit_scan(
            token(&selected),
            selected,
            LibraryScanOptions::default(),
            context(),
        )
        .unwrap();
    let mut discovered = Vec::<PathBuf>::new();
    let mut prepared_count = 0;
    loop {
        match completion(&mut actor) {
            LibraryCompletion::Batch {
                seen,
                prepared,
                ack,
                _storage,
                ..
            } => {
                if prepared.is_empty() {
                    discovered.extend(seen.into_iter().map(|file| file.path));
                } else {
                    prepared_count += prepared.len();
                }
                assert!(ack.complete(Ok(())));
                drop(_storage);
            }
            LibraryCompletion::Finished {
                outcome,
                discovery_complete,
                ..
            } => {
                assert!(discovery_complete);
                assert_eq!(outcome.state, LibraryOperationState::Partial);
                assert_eq!(outcome.discovered_files, 2);
                assert_eq!(outcome.skipped_files, 1);
                assert_eq!(outcome.prepared_files, 1);
                assert!(!outcome.warnings.is_empty());
                break;
            }
            LibraryCompletion::RootValidated { .. }
            | LibraryCompletion::RootVerified { .. }
            | LibraryCompletion::RelinkPrepared { .. } => {
                panic!("Unexpected validation result")
            }
            LibraryCompletion::LiveRelocationHints { .. }
            | LibraryCompletion::RootPathPrepared { .. }
            | LibraryCompletion::AutoRelinkPrepared { .. } => {
                panic!("Unexpected validation result")
            }
        }
    }
    assert_eq!(prepared_count, 1);
    assert!(discovered.iter().any(|path| path.ends_with("bad.wav")));
    assert!(discovered.iter().any(|path| path.ends_with("good.wav")));
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}
#[test]
fn depth_limited_scope_never_claims_complete_missing_finalization() {
    let _case = CASES.lock().unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    fs::create_dir(temp.path().join("nested")).unwrap();
    wav(&temp.path().join("root.wav"));
    wav(&temp.path().join("nested/child.wav"));
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    let selected = root(temp.path(), 4);
    let options = LibraryScanOptions {
        max_depth: 0,
        ..Default::default()
    };
    actor
        .submit_scan(token(&selected), selected, options, context())
        .unwrap();
    loop {
        match completion(&mut actor) {
            LibraryCompletion::Batch { ack, .. } => {
                assert!(ack.complete(Ok(())));
            }
            LibraryCompletion::Finished {
                outcome,
                discovery_complete,
                ..
            } => {
                assert!(!discovery_complete);
                assert_eq!(outcome.state, LibraryOperationState::Partial);
                assert_eq!(outcome.discovered_files, 1);
                assert_eq!(outcome.entries_examined, 2);
                break;
            }
            LibraryCompletion::RootValidated { .. }
            | LibraryCompletion::RootVerified { .. }
            | LibraryCompletion::RelinkPrepared { .. } => {
                panic!("Unexpected validation result")
            }
            LibraryCompletion::LiveRelocationHints { .. }
            | LibraryCompletion::RootPathPrepared { .. }
            | LibraryCompletion::AutoRelinkPrepared { .. } => {
                panic!("Unexpected validation result")
            }
        }
    }
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}
#[test]
fn background_root_verification_retains_real_proof_and_shutdown_retires_it() {
    let _case = CASES.lock().unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("audio.wav");
    wav(&path);
    let selected = root(temp.path(), 12);
    let registry = AuthorityRegistry::default();
    registry.install_root(&selected).unwrap();
    let mut actor =
        LibraryActor::new_with_authority(Catalog::new().unwrap(), registry.clone()).unwrap();
    #[cfg(windows)]
    let exclusive = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap()
    };
    actor.submit_verify(selected.clone(), context()).unwrap();
    let LibraryCompletion::RootVerified {
        root_id,
        grant_epoch,
        health,
        error,
    } = completion(&mut actor)
    else {
        panic!("Expected root verification completion");
    };
    assert_eq!(root_id, selected.root_id);
    assert_eq!(grant_epoch, 1);
    assert_eq!(health, RootHealth::Verified);
    assert!(error.is_none());
    assert!(registry.retirement_pending() > 0);
    #[cfg(windows)]
    drop(exclusive);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(registry.retirement_pending(), 0);
    let moved = temp.path().with_extension("retired-library-test");
    fs::rename(temp.path(), &moved).unwrap();
    fs::rename(&moved, temp.path()).unwrap();
}
