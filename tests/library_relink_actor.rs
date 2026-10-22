#![cfg(any(windows, target_os = "linux"))]

use lx_gpui_player::{
    catalog::Catalog,
    domain::{
        library::*,
        library_watch::{
            LibraryAssetBinding, LibraryWatchOptions, LibraryWatchToken, RelinkEvidence,
        },
        music::{TrackRecord, normalize_local_path},
    },
    library::{
        actor::{LibraryActor, LibraryAutoRelinkOutcome, LibraryCompletion},
        authority::{AuthorityRegistry, AuthorizedFile, RootAuthority},
        scan::ScanContext,
    },
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::Path,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

static CASES: Mutex<()> = Mutex::new(());
fn assert_path_eq(actual: &Path, expected: &Path) {
    assert_eq!(normalize_local_path(actual), normalize_local_path(expected));
}
fn context() -> ScanContext {
    ScanContext::new(Duration::from_secs(20))
}
fn wave(path: &Path) {
    let samples = 2205u32;
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + samples * 2).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&22050u32.to_le_bytes());
    data.extend_from_slice(&44100u32.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&(samples * 2).to_le_bytes());
    for sample in 0..samples {
        data.extend_from_slice(&(sample as i16).to_le_bytes());
    }
    fs::write(path, data).unwrap();
}
fn root(path: &Path) -> LibraryRoot {
    let selected = RootAuthority::open(path, true, &context()).unwrap();
    let draft = selected.draft();
    let barrier = selected.retirement_barrier();
    drop(selected);
    assert!(barrier.wait(Duration::from_secs(3)));
    LibraryRoot {
        root_id: "root:actor-relink".into(),
        path: draft.path,
        recursive: true,
        grant_epoch: 1,
        anchor: draft.anchor,
        path_policy: draft.path_policy,
        watch_enabled: true,
        generation: 1,
        last_complete_generation: 0,
    }
}
fn watch(root: &LibraryRoot) -> LibraryWatchToken {
    LibraryWatchToken {
        root_id: root.root_id.clone(),
        grant_epoch: root.grant_epoch,
        watch_generation: 1,
        enabled: true,
        options: LibraryWatchOptions {
            auto_relink: true,
            ..Default::default()
        },
    }
}
fn next(actor: &mut LibraryActor) -> LibraryCompletion {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(completion) = actor.try_recv() {
            return completion;
        }
        assert!(
            Instant::now() < deadline,
            "Library actor terminal completion timed out"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn scan_preparations(
    actor: &mut LibraryActor,
    selected: &LibraryRoot,
    operation: &str,
) -> Vec<(
    lx_gpui_player::catalog::PreparedLocalImport,
    LibraryPreparedAsset,
)> {
    actor
        .submit_scan(
            LibraryScanToken {
                root_id: selected.root_id.clone(),
                grant_epoch: selected.grant_epoch,
                generation: selected.generation,
                operation_id: operation.into(),
            },
            selected.clone(),
            LibraryScanOptions::default(),
            context(),
        )
        .unwrap();
    let mut items = Vec::new();
    loop {
        match next(actor) {
            LibraryCompletion::Batch { prepared, ack, .. } => {
                items.extend(prepared);
                assert!(ack.complete(Ok(())));
            }
            LibraryCompletion::Finished {
                outcome,
                discovery_complete,
                ..
            } => {
                assert!(discovery_complete);
                assert_eq!(outcome.state, LibraryOperationState::Completed);
                return items;
            }
            other => panic!("Unexpected metadata preparation completion: {other:?}"),
        }
    }
}
fn original(
    catalog: &Catalog,
    registry: &AuthorityRegistry,
    selected: &LibraryRoot,
    path: &Path,
) -> (
    TrackRecord,
    LibraryAssetBinding,
    LocalAssetAuthority,
    AuthorizedFile,
) {
    let intent = LocalAssetAuthority {
        asset_id: "asset:stable".into(),
        track_id: "track:stable".into(),
        path: fs::canonicalize(path).unwrap(),
        explicit_file_epoch: None,
        roots: vec![selected.clone()],
    };
    let opened = registry.install(&intent, None).unwrap().open().unwrap();
    let local = catalog
        .prepare_authorized(opened.try_clone().unwrap())
        .unwrap();
    let physical = local.library_asset();
    let mut record = physical.track;
    record.id = intent.track_id.clone();
    record.lx_id = "local:stable-wire".into();
    record.local_asset_id = Some(intent.asset_id.clone());
    let binding = LibraryAssetBinding {
        asset_id: intent.asset_id.clone(),
        path: intent.path.clone(),
        binding_revision: 7,
        file_identity: Some(opened.identity().clone()),
        content_sha256: Some(format!("{:x}", Sha256::digest(fs::read(path).unwrap()))),
        hash_size_bytes: Some(physical.asset.size_bytes),
        hash_modified_ms: physical.asset.modified_ms,
    };
    registry
        .install(&intent, binding.file_identity.clone())
        .unwrap();
    (record, binding, intent, opened)
}

#[test]
fn scan_content_evidence_owns_actual_source_proof_until_ack_and_retained_evidence_has_no_fh() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("evidence.wav");
    wave(&path);
    let selected = root(temp.path());
    let registry = AuthorityRegistry::default();
    let mut actor =
        LibraryActor::new_with_authority(Catalog::new().unwrap(), registry.clone()).unwrap();
    actor
        .submit_scan_with_content_evidence(
            LibraryScanToken {
                root_id: selected.root_id.clone(),
                grant_epoch: 1,
                generation: 1,
                operation_id: "scan:evidence".into(),
            },
            selected,
            LibraryScanOptions::default(),
            context(),
        )
        .unwrap();
    let LibraryCompletion::Batch {
        ack,
        content_evidence,
        ..
    } = next(&mut actor)
    else {
        panic!("Expected discovery");
    };
    assert!(content_evidence.is_empty());
    assert!(ack.complete(Ok(())));
    let LibraryCompletion::Batch {
        prepared,
        content_evidence,
        ack,
        _storage,
        ..
    } = next(&mut actor)
    else {
        panic!("Expected content evidence");
    };
    assert_eq!(prepared.len(), 1);
    assert_eq!(content_evidence.len(), 1);
    let evidence = &content_evidence[0];
    assert_path_eq(&evidence.path, &fs::canonicalize(&path).unwrap());
    assert_eq!(
        evidence.file_identity,
        prepared[0].1.file_identity.clone().unwrap()
    );
    assert_eq!(evidence.size_bytes, fs::metadata(&path).unwrap().len());
    assert_eq!(
        evidence.content_sha256,
        format!("{:x}", Sha256::digest(fs::read(&path).unwrap()))
    );
    evidence.check().unwrap();
    assert!(ack.complete(Ok(())));
    assert!(evidence.check().is_err());
    let LibraryCompletion::Finished {
        outcome,
        discovery_complete,
        ..
    } = next(&mut actor)
    else {
        panic!("Expected terminal scan");
    };
    assert!(discovery_complete);
    assert_eq!(outcome.state, LibraryOperationState::Completed);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(registry.retirement_pending(), 0);
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
    assert_eq!(prepared[0].0.track().title, "evidence");
    drop(_storage);
    drop(content_evidence);
    drop(prepared);
}

#[cfg(target_os = "linux")]
#[test]
fn delivered_content_evidence_rejects_a_body_write_even_when_original_mtime_is_restored() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("changing.wav");
    wave(&path);
    let selected = root(temp.path());
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    actor
        .submit_scan_with_content_evidence(
            LibraryScanToken {
                root_id: selected.root_id.clone(),
                grant_epoch: 1,
                generation: 1,
                operation_id: "scan:changing".into(),
            },
            selected,
            LibraryScanOptions::default(),
            context(),
        )
        .unwrap();
    let LibraryCompletion::Batch { ack, .. } = next(&mut actor) else {
        panic!("Expected discovery");
    };
    assert!(ack.complete(Ok(())));
    let LibraryCompletion::Batch {
        content_evidence,
        ack,
        ..
    } = next(&mut actor)
    else {
        panic!("Expected prepared evidence");
    };
    let evidence = &content_evidence[0];
    evidence.check().unwrap();
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let mut changed = fs::read(&path).unwrap();
    changed[48] ^= 0x7f;
    fs::write(&path, changed).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(original_mtime))
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        original_mtime
    );
    assert!(
        evidence.check().is_err(),
        "Whole-body evidence must detect high-precision ctime changes before actual commit"
    );
    assert!(ack.complete(Err("Changed source representation".into())));
    let LibraryCompletion::Finished {
        discovery_complete, ..
    } = next(&mut actor)
    else {
        panic!("Expected rejected scan terminal");
    };
    assert!(!discovery_complete);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}

#[test]
fn actual_live_rename_hints_and_preparation_preserve_stable_ids_and_ack_releases_ticket() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("before.wav");
    let new = temp.path().join("after.wav");
    wave(&old);
    let selected = root(temp.path());
    let registry = AuthorityRegistry::default();
    let catalog = Catalog::new().unwrap();
    let (record, binding, mut target, mut held) = original(&catalog, &registry, &selected, &old);
    let mut marker = [0u8; 4];
    held.read_exact(&mut marker).unwrap();
    assert_eq!(&marker, b"RIFF");
    let mut actor = LibraryActor::new_with_authority(catalog, registry.clone()).unwrap();
    fs::rename(&old, &new).unwrap();
    fs::write(&old, b"replacement object").unwrap();
    target.path = fs::canonicalize(&new).unwrap();
    actor
        .submit_live_relocation_hints(
            "hints:live".into(),
            selected.clone(),
            watch(&selected),
            context(),
        )
        .unwrap();
    let LibraryCompletion::LiveRelocationHints { result, .. } = next(&mut actor) else {
        panic!("Expected actual live hints");
    };
    let hints = result.unwrap();
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].asset_id, binding.asset_id);
    assert_path_eq(&hints[0].old_path, &binding.path);
    assert_path_eq(&hints[0].current_path, &target.path);
    assert_eq!(Some(hints[0].file_identity.clone()), binding.file_identity);
    assert!(!hints[0].requires_selection);
    // Scan bookkeeping can advance while a relocation envelope is queued;
    // the admitted physical root grant remains exactly the same scope.
    target.roots[0].generation += 1;
    target.roots[0].last_complete_generation = 1;
    target.roots[0].watch_enabled = false;
    actor
        .submit_auto_relink(
            "auto:live".into(),
            target.path.clone(),
            record.clone(),
            binding.clone(),
            target.clone(),
            selected.clone(),
            watch(&selected),
            context(),
        )
        .unwrap();
    let LibraryCompletion::AutoRelinkPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Expected actual preparation");
    };
    let LibraryAutoRelinkOutcome::Prepared(prepared) = result.unwrap() else {
        panic!("Actual live object must be admitted");
    };
    assert_eq!(prepared.prepared.asset.id, binding.asset_id);
    assert_eq!(prepared.prepared.track.id, record.id);
    assert_eq!(prepared.prepared.track.lx_id, record.lx_id);
    assert_path_eq(&prepared.prepared.asset.path, &target.path);
    assert_eq!(prepared.expected_binding_revision, binding.binding_revision);
    #[cfg(windows)]
    assert_eq!(prepared.evidence, RelinkEvidence::SameOpenObject);
    #[cfg(target_os = "linux")]
    assert_eq!(prepared.evidence, RelinkEvidence::SameFileAndContent);
    prepared.check().unwrap();
    held.read_exact(&mut marker).unwrap();
    assert_ne!(&marker, b"repl");
    assert!(ack.complete(Err("Controller rejected candidate".into())));
    assert!(prepared.check().is_err());
    drop(held);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(
        registry.retirement_pending(),
        0,
        "Caller-retained preparation must not retain private actual FH"
    );
    assert!(fs::OpenOptions::new().write(true).open(&new).is_ok());
    drop(prepared);
}

#[test]
fn hardlink_alias_is_typed_selection_and_never_automatically_chooses_a_locator() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("owned.wav");
    let alias = temp.path().join("alias.wav");
    wave(&old);
    let selected = root(temp.path());
    let registry = AuthorityRegistry::default();
    let catalog = Catalog::new().unwrap();
    let (record, binding, mut target, held) = original(&catalog, &registry, &selected, &old);
    fs::hard_link(&old, &alias).unwrap();
    target.path = fs::canonicalize(&alias).unwrap();
    let mut actor = LibraryActor::new_with_authority(catalog, registry).unwrap();
    actor
        .submit_auto_relink(
            "auto:alias".into(),
            target.path.clone(),
            record,
            binding.clone(),
            target,
            selected.clone(),
            watch(&selected),
            context(),
        )
        .unwrap();
    let LibraryCompletion::AutoRelinkPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Expected terminal alias result");
    };
    let LibraryAutoRelinkOutcome::NeedsSelection(selection) = result.unwrap() else {
        panic!("Hardlink must require selection");
    };
    assert_eq!(selection.asset_id, binding.asset_id);
    assert_eq!(
        selection.expected_binding_revision,
        binding.binding_revision
    );
    assert_eq!(Some(selection.new_identity), binding.file_identity);
    assert!(ack.complete(Ok(())));
    drop(held);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}

#[test]
fn expired_auto_request_still_delivers_terminal_result_and_removes_job() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("original.wav");
    wave(&old);
    let selected = root(temp.path());
    let registry = AuthorityRegistry::default();
    let catalog = Catalog::new().unwrap();
    let (record, binding, target, held) = original(&catalog, &registry, &selected, &old);
    let mut actor = LibraryActor::new_with_authority(catalog, registry).unwrap();
    actor
        .submit_auto_relink(
            "auto:expired".into(),
            target.path.clone(),
            record,
            binding,
            target,
            selected.clone(),
            watch(&selected),
            ScanContext::new(Duration::ZERO),
        )
        .unwrap();
    let LibraryCompletion::AutoRelinkPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Expired request lost its terminal result");
    };
    assert!(result.is_err());
    assert_eq!(actor.active_jobs(), 0);
    drop(ack);
    drop(held);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}

#[test]
fn fresh_root_path_reopens_the_observed_object_and_ack_retires_private_scope_with_retained_factory()
{
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("fresh.wav");
    wave(&path);
    let selected = root(temp.path());
    let actual = RootAuthority::restore(&selected, &context()).unwrap();
    let opened = actual.open_file(&path).unwrap();
    let observed = opened.identity().clone();
    let barrier = actual.retirement_barrier();
    drop(opened);
    drop(actual);
    assert!(barrier.wait(Duration::from_secs(3)));
    let registry = AuthorityRegistry::default();
    let mut actor =
        LibraryActor::new_with_authority(Catalog::new().unwrap(), registry.clone()).unwrap();
    actor
        .submit_refresh_root_path(
            "refresh:fresh".into(),
            path.clone(),
            selected.clone(),
            watch(&selected),
            observed.clone(),
            context(),
        )
        .unwrap();
    let LibraryCompletion::RootPathPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Fresh root path lost its terminal result");
    };
    let prepared = result.unwrap();
    assert_eq!(prepared.prepared.file_identity.as_ref(), Some(&observed));
    let evidence = prepared.content_evidence.as_ref().unwrap();
    assert_eq!(
        evidence.content_sha256,
        format!("{:x}", Sha256::digest(fs::read(&path).unwrap()))
    );
    prepared.check().unwrap();
    evidence.check().unwrap();
    #[cfg(windows)]
    assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
    assert!(ack.complete(Ok(())));
    assert!(prepared.check().is_err());
    assert!(evidence.check().is_err());
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(registry.retirement_pending(), 0);
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
    assert_eq!(prepared.local.track().title, "fresh");
    drop(prepared);
}

#[test]
fn fresh_root_path_rejects_a_different_actual_file_identity_and_expired_jobs_are_terminal() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("slot.wav");
    let moved = temp.path().join("previous.wav");
    wave(&path);
    let selected = root(temp.path());
    let actual = RootAuthority::restore(&selected, &context()).unwrap();
    let opened = actual.open_file(&path).unwrap();
    let observed = opened.identity().clone();
    let barrier = actual.retirement_barrier();
    drop(opened);
    drop(actual);
    assert!(barrier.wait(Duration::from_secs(3)));
    fs::rename(&path, &moved).unwrap();
    wave(&path);
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    actor
        .submit_refresh_root_path(
            "refresh:changed-fid".into(),
            path.clone(),
            selected.clone(),
            watch(&selected),
            observed.clone(),
            context(),
        )
        .unwrap();
    let LibraryCompletion::RootPathPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Changed object request did not finish");
    };
    assert!(result.is_err());
    assert_eq!(actor.active_jobs(), 0);
    drop(ack);
    actor
        .submit_refresh_root_path(
            "refresh:expired".into(),
            path,
            selected.clone(),
            watch(&selected),
            observed,
            ScanContext::new(Duration::ZERO),
        )
        .unwrap();
    let LibraryCompletion::RootPathPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Expired root path request lost its terminal result");
    };
    assert!(result.is_err());
    assert_eq!(actor.active_jobs(), 0);
    drop(ack);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}

#[test]
fn verified_metadata_cache_hits_use_fresh_actual_proof_and_reject_restored_mtime_body_writes() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("cacheable.wav");
    wave(&path);
    let selected = root(temp.path());
    let registry = AuthorityRegistry::default();
    let mut actor =
        LibraryActor::new_with_authority(Catalog::new().unwrap(), registry.clone()).unwrap();
    let first = scan_preparations(&mut actor, &selected, "cache:first");
    assert_eq!(first.len(), 1);
    assert_eq!(actor.metadata_cache_stats().hits, 0);
    assert_eq!(actor.metadata_cache_stats().misses, 1);
    assert_eq!(actor.metadata_cache_stats().entries, 1);
    let second = scan_preparations(&mut actor, &selected, "cache:second");
    assert_eq!(second[0].1.file_identity, first[0].1.file_identity);
    assert_eq!(actor.metadata_cache_stats().hits, 1);
    assert!(actor.metadata_cache_stats().owned_bytes <= 8 * 1024 * 1024);
    // Both callers retain metadata/factories; a cache entry owns no data FH.
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let mut changed = fs::read(&path).unwrap();
    changed[48] ^= 0x31;
    fs::write(&path, changed).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    let third = scan_preparations(&mut actor, &selected, "cache:restored-mtime");
    assert_eq!(third[0].1.file_identity, first[0].1.file_identity);
    assert_eq!(third[0].1.asset.size_bytes, first[0].1.asset.size_bytes);
    assert_eq!(third[0].1.asset.modified_ms, first[0].1.asset.modified_ms);
    assert_eq!(
        actor.metadata_cache_stats().hits,
        1,
        "A changed actual kernel change-stamp must miss despite restored mtime/FID/length"
    );
    assert_eq!(actor.metadata_cache_stats().misses, 2);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(registry.retirement_pending(), 0);
    assert_eq!(actor.metadata_cache_stats().entries, 0);
    assert_eq!(actor.metadata_cache_stats().owned_bytes, 0);
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
    assert_eq!(first[0].0.track().title, "cacheable");
    drop((first, second, third));
}

#[test]
fn verified_metadata_cache_cannot_reuse_a_previous_root_grant_epoch() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("epoch.wav");
    wave(&path);
    let selected = root(temp.path());
    let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
    let retained = scan_preparations(&mut actor, &selected, "cache:old-epoch");
    let mut renewed = selected.clone();
    renewed.grant_epoch += 1;
    actor.submit_verify(renewed.clone(), context()).unwrap();
    let LibraryCompletion::RootVerified {
        error, grant_epoch, ..
    } = next(&mut actor)
    else {
        panic!("Expected actual renewed root proof");
    };
    assert_eq!(grant_epoch, renewed.grant_epoch);
    assert!(error.is_none());
    let fresh = scan_preparations(&mut actor, &renewed, "cache:new-epoch");
    assert_eq!(fresh[0].1.file_identity, retained[0].1.file_identity);
    assert_eq!(actor.metadata_cache_stats().hits, 0);
    assert_eq!(actor.metadata_cache_stats().misses, 2);
    assert_eq!(actor.metadata_cache_stats().entries, 1);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
    drop((retained, fresh));
}

#[cfg(windows)]
#[test]
fn no_live_windows_original_requires_trusted_hash_and_keeps_ticket_actual_until_rejection() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("expired-original.wav");
    let new = temp.path().join("renamed.wav");
    wave(&old);
    let selected = root(temp.path());
    let registry = AuthorityRegistry::default();
    let catalog = Catalog::new().unwrap();
    let (record, binding, mut target, held) = original(&catalog, &registry, &selected, &old);
    drop(held);
    let mut actor = LibraryActor::new_with_authority(catalog, registry.clone()).unwrap();
    fs::rename(&old, &new).unwrap();
    target.path = fs::canonicalize(&new).unwrap();
    actor
        .submit_auto_relink(
            "auto:no-live".into(),
            target.path.clone(),
            record.clone(),
            binding.clone(),
            target.clone(),
            selected.clone(),
            watch(&selected),
            context(),
        )
        .unwrap();
    let LibraryCompletion::AutoRelinkPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Expected no-live preparation");
    };
    let LibraryAutoRelinkOutcome::Prepared(prepared) = result.unwrap() else {
        panic!("Verified Windows file-ID and content must admit no-live rename");
    };
    assert_eq!(prepared.evidence, RelinkEvidence::SameFileAndContent);
    assert_eq!(prepared.content_sha256, binding.content_sha256);
    prepared.check().unwrap();
    assert!(fs::OpenOptions::new().write(true).open(&new).is_err());
    assert!(ack.complete(Err("Controller rejection".into())));
    assert!(prepared.check().is_err());
    assert!(fs::OpenOptions::new().write(true).open(&new).is_ok());
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    assert_eq!(registry.retirement_pending(), 0);
    drop(prepared);
}

#[cfg(target_os = "linux")]
#[test]
fn unknown_linux_original_content_is_typed_selection_with_actual_target_identity() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("unknown.wav");
    let new = temp.path().join("relocated.wav");
    wave(&old);
    let selected = root(temp.path());
    let registry = AuthorityRegistry::default();
    let catalog = Catalog::new().unwrap();
    let (record, mut binding, mut target, held) = original(&catalog, &registry, &selected, &old);
    binding.content_sha256 = None;
    fs::rename(&old, &new).unwrap();
    target.path = fs::canonicalize(&new).unwrap();
    let mut actor = LibraryActor::new_with_authority(catalog, registry).unwrap();
    actor
        .submit_auto_relink(
            "auto:linux-unknown".into(),
            target.path.clone(),
            record,
            binding.clone(),
            target,
            selected.clone(),
            watch(&selected),
            context(),
        )
        .unwrap();
    let LibraryCompletion::AutoRelinkPrepared { result, ack, .. } = next(&mut actor) else {
        panic!("Expected unknown-proof result");
    };
    let LibraryAutoRelinkOutcome::NeedsSelection(selection) = result.unwrap() else {
        panic!("Linux inode alone cannot prove original representation");
    };
    assert_eq!(Some(selection.new_identity), binding.file_identity);
    assert!(ack.complete(Ok(())));
    drop(held);
    assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
}
