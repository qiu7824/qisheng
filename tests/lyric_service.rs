use lx_gpui_player::{
    domain::{
        lyric::{LyricBundle, LyricOrigin},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    lyrics::{
        local::read_local_lyrics,
        parser::ParserMode,
        service::{
            LyricExportFormat, LyricProvenance, LyricRequest, LyricService, LyricServiceError,
            export_bundle,
        },
    },
    repository::{Repository, local_track_from_path},
    source::{SourceRequest, SourceResult},
    worker::SourceWorker,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn wave(path: &Path) {
    let data_size = 160u32;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_size.to_le_bytes());
    bytes.resize(44 + data_size as usize, 0);
    std::fs::write(path, bytes).unwrap();
}

fn setup(directory: &Path) -> (Arc<Repository>, LyricService, TrackRecord, PathBuf) {
    let path = directory.join("original.wav");
    wave(&path);
    let (track, asset) = local_track_from_path(&path).unwrap();
    let repository = Arc::new(Repository::open(directory.join("profile")).unwrap());
    repository.upsert_local_track(&track, &asset).unwrap();
    let service = LyricService::new(repository.clone());
    (repository, service, track, path)
}

fn bundle(text: &str) -> LyricBundle {
    LyricBundle::from_value(
        &json!({"lyric":format!("[00:01.000]{text}"),"tlyric":null,"rlyric":""}),
    )
    .unwrap()
}

#[test]
fn edited_whole_bundle_wins_then_restore_reads_sidecar_before_cache_and_resolver() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, path) = setup(directory.path());
    repository
        .replace_lyrics(&track.id, LyricOrigin::Raw, &bundle("cached"))
        .unwrap();
    service
        .save_edited(&track.id, &bundle("edited"), None)
        .unwrap();
    std::fs::write(path.with_extension("lrc"), "[00:01.000]file").unwrap();
    let mut resolve = |_: &TrackRecord, _: &LyricRequest| -> anyhow::Result<LyricBundle> {
        panic!("Resolver must not run")
    };
    let prepared = service
        .prepare(LyricRequest::new(&track.id, 1), Some(&mut resolve))
        .unwrap();
    assert_eq!(prepared.provenance, LyricProvenance::Edited);
    assert_eq!(prepared.bundle, bundle("edited"));
    assert!(
        service
            .commit(prepared, &track.id, 1)
            .unwrap()
            .change
            .is_none()
    );
    service.restore_raw(&track.id).unwrap();
    let prepared = service
        .prepare(LyricRequest::new(&track.id, 1), Some(&mut resolve))
        .unwrap();
    assert_eq!(prepared.provenance, LyricProvenance::LocalSidecar);
    assert_eq!(prepared.document.lines[0].text, "file");
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Raw).unwrap(),
        Some(bundle("cached"))
    );
    let committed = service.commit(prepared, &track.id, 1).unwrap();
    assert!(committed.change.is_some());
    assert_eq!(
        repository
            .lyrics(&track.id, LyricOrigin::Raw)
            .unwrap()
            .unwrap()
            .lyric
            .text(),
        Some("[00:01.000]file")
    );
    let unchanged = service
        .prepare(LyricRequest::new(&track.id, 1), None)
        .unwrap();
    assert!(
        service
            .commit(unchanged, &track.id, 1)
            .unwrap()
            .change
            .is_none()
    );
}

#[test]
fn raw_cache_and_fetched_empty_avoid_unnecessary_source_execution() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, _) = setup(directory.path());
    for raw in [
        bundle("cached"),
        LyricBundle::from_value(&json!({"lyric":"","tlyric":null})).unwrap(),
    ] {
        repository
            .replace_lyrics(&track.id, LyricOrigin::Raw, &raw)
            .unwrap();
        let mut resolver = |_: &TrackRecord, _: &LyricRequest| -> anyhow::Result<LyricBundle> {
            panic!("Cache hit must not execute a source")
        };
        let prepared = service
            .prepare(LyricRequest::new(&track.id, 1), Some(&mut resolver))
            .unwrap();
        assert_eq!(prepared.provenance, LyricProvenance::RawCache);
        assert_eq!(prepared.bundle, raw);
    }
}

#[test]
fn stale_track_generation_and_cancelled_fetch_do_not_write_raw_data() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, path) = setup(directory.path());
    std::fs::write(path.with_extension("lrc"), "[00:01.000]file").unwrap();
    for (current_track, generation, expected) in [
        (track.id.as_str(), 2, LyricServiceError::StaleGeneration),
        ("different", 1, LyricServiceError::ChangedTrack),
    ] {
        let prepared = service
            .prepare(LyricRequest::new(&track.id, 1), None)
            .unwrap();
        let error = service
            .commit(prepared, current_track, generation)
            .err()
            .unwrap();
        assert_eq!(error.downcast_ref::<LyricServiceError>(), Some(&expected));
        assert!(
            repository
                .lyrics(&track.id, LyricOrigin::Raw)
                .unwrap()
                .is_none()
        );
    }
    let request = LyricRequest::new(&track.id, 1);
    let cancel = request.cancel.clone();
    let prepared = service.prepare(request, None).unwrap();
    cancel.store(true, Ordering::Release);
    let error = service.commit(prepared, &track.id, 1).err().unwrap();
    assert_eq!(
        error.downcast_ref::<LyricServiceError>(),
        Some(&LyricServiceError::Cancelled)
    );
    assert!(
        repository
            .lyrics(&track.id, LyricOrigin::Raw)
            .unwrap()
            .is_none()
    );
}

#[test]
fn edit_committed_during_fetch_prevents_old_raw_result_from_publishing() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, _) = setup(directory.path());
    let mut resolve = |_: &TrackRecord, _: &LyricRequest| Ok(bundle("fetched"));
    let prepared = service
        .prepare(LyricRequest::new(&track.id, 1), Some(&mut resolve))
        .unwrap();
    service
        .save_edited(&track.id, &bundle("new edit"), None)
        .unwrap();
    let error = service.commit(prepared, &track.id, 1).err().unwrap();
    assert_eq!(
        error.downcast_ref::<LyricServiceError>(),
        Some(&LyricServiceError::RevisionConflict)
    );
    assert!(
        repository
            .lyrics(&track.id, LyricOrigin::Raw)
            .unwrap()
            .is_none()
    );
    let next = service
        .prepare(LyricRequest::new(&track.id, 1), None)
        .unwrap();
    assert_eq!(next.document.lines[0].text, "new edit");
}

#[test]
fn callback_cancellation_and_missing_resolver_are_explicit_and_never_approve_sources() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, _) = setup(directory.path());
    let mut resolve = |_: &TrackRecord, request: &LyricRequest| {
        request.cancel.store(true, Ordering::Release);
        Ok(bundle("too late"))
    };
    let error = service
        .prepare(LyricRequest::new(&track.id, 1), Some(&mut resolve))
        .err()
        .unwrap();
    assert_eq!(
        error.downcast_ref::<LyricServiceError>(),
        Some(&LyricServiceError::Cancelled)
    );
    assert!(
        repository
            .lyrics(&track.id, LyricOrigin::Raw)
            .unwrap()
            .is_none()
    );
    let error = service
        .prepare(LyricRequest::new(&track.id, 1), None)
        .err()
        .unwrap();
    assert_eq!(
        error.downcast_ref::<LyricServiceError>(),
        Some(&LyricServiceError::Unavailable)
    );
    let cancelled = LyricRequest {
        cancel: Arc::new(AtomicBool::new(true)),
        ..LyricRequest::new(&track.id, 1)
    };
    assert!(service.prepare(cancelled, None).is_err());
}

#[test]
fn original_source_worker_callback_completes_prepare_commit_and_reopen_chain() {
    let directory = tempfile::tempdir().unwrap();
    let track_id;
    {
        let (repository, service, track, _) = setup(directory.path());
        track_id = track.id.clone();
        let script = "lx.on('request',async({action,info})=>{if(action!=='lyric'||info.musicInfo.source!=='local')throw Error('contract');return {lyric:'[00:01.000]source',tlyric:null,rlyric:'',lxlyric:'[00:01.000]<0,1000>source'};});lx.send('inited',{sources:{local:{type:'music',actions:['lyric'],qualitys:[]}}});";
        let mut worker = SourceWorker::start(script).unwrap();
        let mut resolve =
            |track: &TrackRecord, request: &LyricRequest| -> anyhow::Result<LyricBundle> {
                request.check_cancelled()?;
                let response = worker.request_result(&SourceRequest {
                    source: "local".into(),
                    action: "lyric".into(),
                    info: json!({"musicInfo":track.to_lx_dto(LxMusicDtoVersion::Legacy)}),
                })?;
                match response {
                    SourceResult::Lyrics(bundle) => Ok(bundle),
                    _ => anyhow::bail!("Wrong result type"),
                }
            };
        let prepared = service
            .prepare(LyricRequest::new(&track.id, 5), Some(&mut resolve))
            .unwrap();
        assert_eq!(prepared.provenance, LyricProvenance::AuthorizedSource);
        assert!(
            repository
                .lyrics(&track.id, LyricOrigin::Raw)
                .unwrap()
                .is_none()
        );
        let committed = service.commit(prepared, &track.id, 5).unwrap();
        assert_eq!(committed.document.lines[0].words[0].text, "source");
        assert!(committed.change.is_some());
    }
    let repository = Arc::new(Repository::open(directory.path().join("profile")).unwrap());
    let service = LyricService::new(repository);
    let restored = service
        .prepare(LyricRequest::new(&track_id, 6), None)
        .unwrap();
    assert_eq!(restored.provenance, LyricProvenance::RawCache);
    assert_eq!(restored.bundle.rlyric.text(), Some(""));
    assert!(restored.bundle.tlyric.text().is_none());
}

#[test]
fn restore_and_clear_raw_keep_edits_separate_and_validate_before_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, _) = setup(directory.path());
    repository
        .replace_lyrics(&track.id, LyricOrigin::Raw, &bundle("raw"))
        .unwrap();
    service
        .save_edited(&track.id, &bundle("edited"), None)
        .unwrap();
    service.clear_raw(&track.id).unwrap();
    assert!(
        repository
            .lyrics(&track.id, LyricOrigin::Raw)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        service
            .prepare(LyricRequest::new(&track.id, 1), None)
            .unwrap()
            .bundle,
        bundle("edited")
    );
    let revision = repository.revision().unwrap();
    let invalid = LyricBundle::from_value(&json!({"lyric":null})).unwrap();
    assert!(
        service
            .save_edited(&track.id, &invalid, Some(revision))
            .is_err()
    );
    assert_eq!(repository.revision().unwrap(), revision);
    assert!(
        service
            .save_edited(&track.id, &bundle("stale edit"), Some(revision - 1))
            .is_err()
    );
    service.restore_raw(&track.id).unwrap();
    assert!(
        repository
            .lyrics(&track.id, LyricOrigin::Edited)
            .unwrap()
            .is_none()
    );
}

#[test]
fn json_export_preserves_states_and_awlrc_container_roundtrips_available_four_tracks() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, path) = setup(directory.path());
    let states = bundle("原文🎵");
    repository
        .replace_lyrics(&track.id, LyricOrigin::Raw, &states)
        .unwrap();
    let json = service
        .export_cached(&track.id, LyricExportFormat::BundleJson)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<LyricBundle>(&json.text).unwrap(),
        states
    );
    let four = LyricBundle::from_value(
        &json!({"lyric":"[00:01.000]原文", "tlyric":"[00:01.000]译文",
        "rlyric":"[00:01.000]roman", "lxlyric":"[00:01.000]<0,1000>原文"}),
    )
    .unwrap();
    let exported = export_bundle(&four, LyricExportFormat::Awlrc).unwrap();
    std::fs::write(path.with_extension("lrc"), exported.text).unwrap();
    assert_eq!(read_local_lyrics(&path).unwrap().unwrap().bundle, four);
    assert_eq!(
        export_bundle(&four, LyricExportFormat::Lrc).unwrap().text,
        "[00:01.000]原文"
    );
}

#[test]
fn explicit_refresh_uses_resolver_without_deleting_existing_edited_data() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, service, track, path) = setup(directory.path());
    service
        .save_edited(&track.id, &bundle("edited"), None)
        .unwrap();
    std::fs::write(path.with_extension("lrc"), "[00:01.000]file").unwrap();
    let mut request = LyricRequest::new(&track.id, 1);
    request.force_refresh = true;
    request.parser_mode = ParserMode::LxCompatible;
    let mut resolver = |_: &TrackRecord, _: &LyricRequest| Ok(bundle("refreshed"));
    let prepared = service.prepare(request, Some(&mut resolver)).unwrap();
    assert_eq!(prepared.provenance, LyricProvenance::AuthorizedSource);
    service.commit(prepared, &track.id, 1).unwrap();
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Edited).unwrap(),
        Some(bundle("edited"))
    );
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Raw).unwrap(),
        Some(bundle("refreshed"))
    );
}

#[test]
fn empty_selected_sidecar_does_not_execute_source_and_untimed_edits_remain_exportable() {
    let directory = tempfile::tempdir().unwrap();
    let (_, service, track, path) = setup(directory.path());
    std::fs::write(path.with_extension("lrc"), "").unwrap();
    let mut resolver = |_: &TrackRecord, _: &LyricRequest| -> anyhow::Result<LyricBundle> {
        panic!("Empty sidecar is available")
    };
    let prepared = service
        .prepare(LyricRequest::new(&track.id, 1), Some(&mut resolver))
        .unwrap();
    assert_eq!(prepared.provenance, LyricProvenance::LocalSidecar);
    assert!(prepared.document.lines.is_empty());
    let edited = LyricBundle::from_value(&json!({"lyric":"Plain unsynchronized verse"})).unwrap();
    service.save_edited(&track.id, &edited, None).unwrap();
    assert_eq!(
        service
            .export_cached(&track.id, LyricExportFormat::Lrc)
            .unwrap()
            .text,
        "Plain unsynchronized verse"
    );
}
