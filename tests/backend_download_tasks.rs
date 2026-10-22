#[allow(dead_code)]
#[path = "support/encoded_audio_fixture.rs"]
mod encoded;
#[path = "support/library_fixture.rs"]
mod fixture;
use fixture::Running;
use lx_gpui_player::{
    backend::{BackendCommand, BackendQuery, BackendQueryResult},
    domain::{
        lists::{AddLocation, ListAction},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    download::{DownloadState, planning::DownloadQuality, task_model::DownloadSettings},
};
struct HttpOwner {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for HttpOwner {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn destinations(
    running: &Running,
) -> Vec<lx_gpui_player::download::task_model::StoredDestinationGrant> {
    let BackendQueryResult::DownloadDestinations(values) = running
        .backend
        .query(BackendQuery::DownloadDestinations)
        .unwrap()
    else {
        panic!("Destinations")
    };
    values
}
fn rows(running: &Running) -> Vec<lx_gpui_player::download::task_model::DownloadTaskView> {
    let BackendQueryResult::DownloadTasks(values) =
        running.backend.query(BackendQuery::DownloadTasks).unwrap()
    else {
        panic!("Tasks")
    };
    values
}
#[test]
fn selected_destination_persisted_tasks_and_restart_do_not_create_files_or_source_runtime() {
    let mut running = Running::new();
    let destination = running.folder("download-explicit-selected");
    let cursor = running.backend.event_cursor();
    running.execute(
        "download-select",
        BackendCommand::AddDownloadDestination {
            path: destination.clone(),
        },
    );
    assert!(fixture::operation_finished(&running.backend, cursor, "download-select").0);
    let grant = destinations(&running).pop().unwrap();
    let settings = DownloadSettings {
        enabled: true,
        destination_grant_id: Some(grant.id.clone()),
        ..Default::default()
    };
    running.execute(
        "download-settings",
        BackendCommand::SetDownloadSettings { settings },
    );
    let track=TrackRecord::from_lx_dto(serde_json::json!({"source":"kw","songmid":"owned-online-metadata","name":"合法曲目","singer":"作者",
        "albumName":"专辑","interval":"00:36","types":[{"type":"128k"}],"_types":{"128k":{}}}),LxMusicDtoVersion::Legacy).unwrap();
    running.execute(
        "download-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![track.clone()],
                location: AddLocation::Top,
            }),
        },
    );
    running.execute(
        "download-add",
        BackendCommand::AddDownloadTasks {
            list_id: "default".into(),
            music_ids: vec![track.id.clone(), track.id.clone()],
            quality: DownloadQuality::K128,
            top: true,
        },
    );
    let before = rows(&running);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].state, DownloadState::Waiting);
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    assert!(running.backend.snapshot().sources.is_empty());
    running.restart();
    let after = rows(&running);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, before[0].id);
    assert_eq!(after[0].state, DownloadState::Paused);
    assert!(after[0].operation_epoch > before[0].operation_epoch);
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    assert!(running.backend.snapshot().sources.is_empty());
    running.execute(
        "download-revoke",
        BackendCommand::RevokeDownloadDestination {
            grant_id: grant.id,
            expected_grant_revision: grant.revision,
        },
    );
    assert!(destinations(&running)[0].revoked);
    running.execute(
        "download-remove",
        BackendCommand::RemoveDownloadTask {
            task_id: after[0].id.clone(),
            expected_task_revision: after[0].revision,
            operation_epoch: after[0].operation_epoch,
        },
    );
    assert!(rows(&running).is_empty());
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
}
#[test]
fn unselected_download_destination_and_invalid_track_batch_preserve_profile_revision() {
    let running = Running::new();
    let revision = running.backend.snapshot_with_cursor().unwrap().0.revision;
    assert!(
        running
            .backend
            .execute(fixture::envelope(
                "download-no-destination",
                BackendCommand::AddDownloadTasks {
                    list_id: "default".into(),
                    music_ids: vec!["unknown".into()],
                    quality: DownloadQuality::K128,
                    top: true
                }
            ))
            .is_err()
    );
    assert_eq!(running.backend.snapshot().revision, revision);
    assert!(rows(&running).is_empty());
    assert!(destinations(&running).is_empty());
}

#[test]
fn actual_approved_source_resolution_persists_pending_url_without_audio_fetch_or_implicit_consent()
{
    let running = Running::new();
    let destination = running.folder("source-download-selected");
    let cursor = running.backend.event_cursor();
    running.execute(
        "source-download-dest",
        BackendCommand::AddDownloadDestination {
            path: destination.clone(),
        },
    );
    assert!(fixture::operation_finished(&running.backend, cursor, "source-download-dest").0);
    let grant = destinations(&running).pop().unwrap();
    running.execute(
        "source-download-settings",
        BackendCommand::SetDownloadSettings {
            settings: DownloadSettings {
                destination_grant_id: Some(grant.id),
                ..Default::default()
            },
        },
    );
    let audio = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    audio.set_nonblocking(true).unwrap();
    let url = format!(
        "http://{}/download-original.mp3",
        audio.local_addr().unwrap()
    );
    let script = format!(
        r#"/** @name Original download resolver */
lx.on('request',async({{action,info}})=>{{if(action!=='musicUrl'||info.type!=='128k')throw Error('invalid request');return {};}});
lx.send('inited',{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});"#,
        serde_json::to_string(&url).unwrap()
    );
    let source = lx_gpui_player::source::script_hash(&script);
    for (id, command) in [
        (
            "download-source-import",
            BackendCommand::ImportSourceText { script },
        ),
        (
            "download-source-approved",
            BackendCommand::ApproveSource { id: source.clone() },
        ),
        (
            "download-source-enabled",
            BackendCommand::EnableSource { id: source },
        ),
    ] {
        let cursor = running.backend.event_cursor();
        running.execute(id, command);
        assert!(fixture::operation_finished(&running.backend, cursor, id).0);
    }
    let track=TrackRecord::from_lx_dto(serde_json::json!({"source":"kw","songmid":"real-source-download","name":"下载曲目","singer":"作者",
        "albumName":"专辑","interval":"00:36","types":[{"type":"128k"}],"_types":{"128k":{}}}),LxMusicDtoVersion::Legacy).unwrap();
    running.execute(
        "source-download-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![track.clone()],
                location: AddLocation::Top,
            }),
        },
    );
    running.execute(
        "source-download-add",
        BackendCommand::AddDownloadTasks {
            list_id: "default".into(),
            music_ids: vec![track.id],
            quality: DownloadQuality::K128,
            top: true,
        },
    );
    let task = rows(&running).pop().unwrap();
    running.execute(
        "source-download-resume",
        BackendCommand::ResumeDownloadTask {
            task_id: task.id.clone(),
            expected_task_revision: task.revision,
        },
    );
    fixture::wait(&running.backend, 10.0, |_| {
        rows(&running)
            .iter()
            .any(|task| task.state == DownloadState::AwaitingUrlApproval)
    });
    let BackendQueryResult::DownloadPendingUrl(Some(pending)) = running
        .backend
        .query(BackendQuery::DownloadPendingUrl {
            task_id: task.id.clone(),
        })
        .unwrap()
    else {
        panic!("Pending download approval")
    };
    assert_eq!(pending.url, url);
    assert!(audio.accept().is_err());
    assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
    assert!(running.backend.snapshot().pending_playback_url.is_none());
    assert!(
        running
            .backend
            .execute(fixture::envelope(
                "source-download-wrong-url",
                BackendCommand::ApproveDownloadUrl {
                    task_id: task.id,
                    expected_task_revision: pending.task_revision,
                    operation_epoch: pending.operation_epoch,
                    url: format!("{url}?other=true")
                }
            ))
            .is_err()
    );
    assert!(audio.accept().is_err());
}

#[test]
fn actual_download_approval_transfers_then_tags_flac_before_publishing_and_survives_restart() {
    complete_flac_case(CompleteCase::Normal);
}
#[test]
fn lyric_sidecar_collision_preserves_published_audio_and_the_existing_user_file() {
    complete_flac_case(CompleteCase::SidecarCollision);
}
#[test]
fn final_rename_before_task_commit_is_recovered_by_proof_without_another_download() {
    complete_flac_case(CompleteCase::MetadataCrash);
}
#[test]
fn sidecar_collision_and_task_ack_failure_recover_the_published_audio_independently() {
    complete_flac_case(CompleteCase::MetadataCrashWithSidecar);
}
#[test]
fn approved_download_publishes_four_lyric_tracks_in_sidecar_and_embedded_tags() {
    complete_flac_case(CompleteCase::FourLyrics);
}
#[test]
fn recovery_with_a_foreign_final_sidecar_keeps_verified_audio_and_reports_the_lyric_failure() {
    complete_flac_case(CompleteCase::PublishedSidecarReplaced);
}
#[test]
fn cleanup_ack_failure_survives_backend_restart_and_releases_only_original_accounting() {
    complete_flac_case(CompleteCase::CleanupAckFailure);
}
#[test]
fn published_audio_lyric_retry_after_restart_uses_no_transfer_and_preserves_completed_tags() {
    complete_flac_case(CompleteCase::RetrySidecar);
}
#[test]
fn postprocessing_retry_never_overwrites_the_foreign_sidecar_or_retags_completed_audio() {
    complete_flac_case(CompleteCase::RetryForeignSidecar);
}
#[test]
fn postprocessing_retry_rejects_a_replaced_published_audio_before_creating_artifacts() {
    complete_flac_case(CompleteCase::RetryForeignAudio);
}
#[test]
fn cached_cover_retry_updates_the_same_published_flac_without_retransferring_or_retagging_metadata()
{
    complete_flac_case(CompleteCase::RetryCover);
}
#[test]
fn failed_tag_stages_retry_on_the_real_published_file_and_preserve_the_completed_sidecar() {
    complete_flac_case(CompleteCase::RetryTags);
}
#[test]
fn completed_download_plays_actual_pcm_without_a_source_or_an_explicit_file_grant() {
    complete_flac_case(CompleteCase::LocalPlayWithoutSource);
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompleteCase {
    Normal,
    LocalPlayWithoutSource,
    SidecarCollision,
    MetadataCrash,
    MetadataCrashWithSidecar,
    FourLyrics,
    PublishedSidecarReplaced,
    CleanupAckFailure,
    RetrySidecar,
    RetryForeignSidecar,
    RetryForeignAudio,
    RetryCover,
    RetryTags,
}
fn complete_flac_case(case: CompleteCase) {
    use std::io::{Read, Write};
    use std::time::Duration;
    let sidecar_collision = matches!(
        case,
        CompleteCase::SidecarCollision
            | CompleteCase::MetadataCrashWithSidecar
            | CompleteCase::RetrySidecar
            | CompleteCase::RetryForeignSidecar
            | CompleteCase::RetryForeignAudio
    );
    let metadata_crash = matches!(
        case,
        CompleteCase::MetadataCrash | CompleteCase::MetadataCrashWithSidecar
    );
    let four_lyrics = matches!(
        case,
        CompleteCase::FourLyrics
            | CompleteCase::PublishedSidecarReplaced
            | CompleteCase::RetryTags
            | CompleteCase::LocalPlayWithoutSource
    );
    let mut running = Running::new();
    let destination = running.folder("complete-download-selected");
    let cursor = running.backend.event_cursor();
    running.execute(
        "complete-download-dest",
        BackendCommand::AddDownloadDestination {
            path: destination.clone(),
        },
    );
    assert!(fixture::operation_finished(&running.backend, cursor, "complete-download-dest").0);
    let grant = destinations(&running).pop().unwrap();
    running.execute(
        "complete-download-settings",
        BackendCommand::SetDownloadSettings {
            settings: DownloadSettings {
                destination_grant_id: Some(grant.id),
                embed_cover: case == CompleteCase::RetryCover,
                download_lyrics: sidecar_collision || four_lyrics,
                embed_lyrics: four_lyrics,
                sidecar_lyrics: lx_gpui_player::download::lyrics::DownloadLyricOptions {
                    include_awlrc: true,
                    translation: four_lyrics,
                    romanization: four_lyrics,
                },
                embedded_lyrics: lx_gpui_player::download::lyrics::DownloadLyricOptions {
                    include_awlrc: true,
                    translation: four_lyrics,
                    romanization: four_lyrics,
                },
                ..Default::default()
            },
        },
    );
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/verified.flac", listener.local_addr().unwrap());
    let original = encoded::mono_verbatim_flac();
    let expected = original.clone();
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = stopping.clone();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if stopped.load(std::sync::atomic::Ordering::Acquire)
                || std::time::Instant::now() >= deadline
            {
                return;
            }
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if socket.read(&mut byte).unwrap_or(0) == 0 {
                            return;
                        }
                        headers.extend(byte);
                    }
                    write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"original-flac\"\r\nConnection: close\r\n\r\n",original.len()).unwrap();
                    socket.write_all(&original).unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let mut owner = HttpOwner {
        stop: stopping.clone(),
        worker: Some(server),
    };
    let script = format!(
        r#"/** @name Original complete download source */
lx.on('request',async({{action,info}})=>{{if(action!=='musicUrl'||info.type!=='flac')throw Error('invalid');return {};}});
lx.send('inited',{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['flac']}}}}}});"#,
        serde_json::to_string(&url).unwrap()
    );
    let source = lx_gpui_player::source::script_hash(&script);
    for (id, command) in [
        (
            "complete-source-import",
            BackendCommand::ImportSourceText { script },
        ),
        (
            "complete-source-approve",
            BackendCommand::ApproveSource { id: source.clone() },
        ),
        (
            "complete-source-enable",
            BackendCommand::EnableSource { id: source },
        ),
    ] {
        let cursor = running.backend.event_cursor();
        running.execute(id, command);
        assert!(fixture::operation_finished(&running.backend, cursor, id).0);
    }
    let track=TrackRecord::from_lx_dto(serde_json::json!({"source":"kw","songmid":"complete-flac-source","name":"下载标题","singer":"合成作者",
        "albumName":"下载专辑","interval":"00:01","types":[{"type":"flac"}],"_types":{"flac":{}}}),LxMusicDtoVersion::Legacy).unwrap();
    running.execute(
        "complete-download-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![track.clone()],
                location: AddLocation::Top,
            }),
        },
    );
    let sidecar = destination.join("下载标题 - 合成作者.lrc");
    if sidecar_collision {
        std::fs::write(&sidecar, b"original user lyric").unwrap();
        running.execute(
            "complete-download-existing-lyric",
            BackendCommand::SaveLyrics {
                id: track.id.clone(),
                bundle: serde_json::from_value(serde_json::json!({"lyric":"[00:00.00]原创歌词"}))
                    .unwrap(),
            },
        );
    }
    let lyric_bundle:lx_gpui_player::domain::lyric::LyricBundle=serde_json::from_value(serde_json::json!({
        "lyric":"[00:00.00]原创歌词","tlyric":"[00:00.00]Translation","rlyric":"[00:00.00]Yuan chuang",
        "lxlyric":"[00:00.00]<0,500>原创<500,500>歌词"})).unwrap();
    if four_lyrics {
        running.execute(
            "complete-download-four-lyrics",
            BackendCommand::SaveLyrics {
                id: track.id.clone(),
                bundle: lyric_bundle.clone(),
            },
        );
    }
    running.execute(
        "complete-download-add",
        BackendCommand::AddDownloadTasks {
            list_id: "default".into(),
            music_ids: vec![track.id],
            quality: DownloadQuality::Flac,
            top: true,
        },
    );
    let task = rows(&running).pop().unwrap();
    running.execute(
        "complete-download-resolve",
        BackendCommand::ResumeDownloadTask {
            task_id: task.id.clone(),
            expected_task_revision: task.revision,
        },
    );
    fixture::wait(&running.backend, 10.0, |_| {
        rows(&running)
            .iter()
            .any(|task| task.state == DownloadState::AwaitingUrlApproval)
    });
    let BackendQueryResult::DownloadPendingUrl(Some(pending)) = running
        .backend
        .query(BackendQuery::DownloadPendingUrl {
            task_id: task.id.clone(),
        })
        .unwrap()
    else {
        panic!("Approval")
    };
    let fault_connection = rusqlite::Connection::open(
        running
            .profile()
            .join(lx_gpui_player::repository::DATABASE_NAME),
    )
    .unwrap();
    if metadata_crash {
        fault_connection.execute_batch("CREATE TRIGGER fail_final_task_ack BEFORE UPDATE OF document ON download_tasks WHEN json_extract(NEW.document,'$.audio_available')=1 BEGIN SELECT RAISE(ABORT,'injected final task ACK failure'); END;").unwrap();
    }
    if case == CompleteCase::CleanupAckFailure {
        fault_connection.execute_batch("CREATE TRIGGER fail_cleanup_accounting BEFORE UPDATE OF phase ON download_owned_artifacts WHEN NEW.phase='removed' BEGIN SELECT RAISE(ABORT,'injected cleanup ACK failure'); END;").unwrap();
    }
    running.execute(
        "complete-download-url-approved",
        BackendCommand::ApproveDownloadUrl {
            task_id: task.id.clone(),
            expected_task_revision: pending.task_revision,
            operation_epoch: pending.operation_epoch,
            url: pending.url,
        },
    );
    if metadata_crash {
        fixture::wait(&running.backend, 20.0, |snapshot| {
            snapshot
                .error
                .as_ref()
                .is_some_and(|error| error.contains("injected final task ACK failure"))
        });
        let published = destination.join("下载标题 - 合成作者.flac");
        let body = std::fs::read(&published).unwrap();
        assert_ne!(body, expected);
        stopping.store(true, std::sync::atomic::Ordering::Release);
        owner.worker.take().unwrap().join().unwrap();
        fault_connection
            .execute_batch("DROP TRIGGER fail_final_task_ack;")
            .unwrap();
        drop(fault_connection);
        running.restart();
        running.execute("recovery-source-disabled", BackendCommand::DisableSource);
        let interrupted = rows(&running).pop().unwrap();
        assert_eq!(interrupted.state, DownloadState::Paused);
        assert!(!interrupted.audio_available);
        let cursor = running.backend.event_cursor();
        running.execute(
            "final-rename-recovery",
            BackendCommand::RecoverDownloadCommit {
                task_id: interrupted.id.clone(),
                expected_task_revision: interrupted.revision,
            },
        );
        assert!(
            fixture::operation_finished(&running.backend, cursor, "final-rename-recovery").0,
            "recovery error={:?}; events={:?}",
            running.backend.snapshot().error,
            running.backend.events_after(cursor).unwrap()
        );
        let recovered = rows(&running).pop().unwrap();
        assert_eq!(
            recovered.state,
            if sidecar_collision {
                DownloadState::Failed
            } else {
                DownloadState::Completed
            }
        );
        assert!(recovered.audio_available);
        assert_eq!(std::fs::read(&published).unwrap(), body);
        if sidecar_collision {
            assert_eq!(std::fs::read(&sidecar).unwrap(), b"original user lyric");
        }
        running.restart();
        assert_eq!(
            rows(&running).pop().unwrap().output_audio,
            recovered.output_audio
        );
        return;
    }
    drop(fault_connection);
    let expected_state = if sidecar_collision {
        DownloadState::Failed
    } else if case == CompleteCase::RetryCover {
        DownloadState::Paused
    } else {
        DownloadState::Completed
    };
    fixture::wait(&running.backend, 20.0, |_| {
        rows(&running)
            .iter()
            .any(|row| row.id == task.id && row.state == expected_state && row.audio_available)
    });
    stopping.store(true, std::sync::atomic::Ordering::Release);
    owner.worker.take().unwrap().join().unwrap();
    let completed = rows(&running).pop().unwrap();
    assert!(completed.audio_available);
    if sidecar_collision {
        assert!(
            running
                .backend
                .execute(fixture::envelope(
                    "preserved-audio-no-transfer-retry",
                    BackendCommand::ResumeDownloadTask {
                        task_id: completed.id.clone(),
                        expected_task_revision: completed.revision
                    }
                ))
                .is_err()
        );
        assert_eq!(rows(&running).pop().unwrap().revision, completed.revision);
    }
    let output = destination.join(&completed.output_audio.as_ref().unwrap().basename);
    let published_body = std::fs::read(&output).unwrap();
    assert_ne!(published_body, expected);
    let metadata = lx_gpui_player::library::metadata::read_local_metadata(&output).unwrap();
    assert_eq!(metadata.title.as_deref(), Some("下载标题"));
    assert_eq!(metadata.album.as_deref(), Some("下载专辑"));
    if sidecar_collision {
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"original user lyric");
    }
    if four_lyrics {
        let expected_lyrics = lx_gpui_player::download::lyrics::build_download_lyrics(
            &lyric_bundle,
            lx_gpui_player::download::lyrics::DownloadLyricOptions {
                include_awlrc: true,
                translation: true,
                romanization: true,
            },
        )
        .unwrap();
        let sidecar_bytes = std::fs::read(&sidecar).unwrap();
        assert!(sidecar_bytes.starts_with(&[0xef, 0xbb, 0xbf]));
        assert_eq!(
            std::str::from_utf8(&sidecar_bytes[3..]).unwrap(),
            expected_lyrics.text
        );
        assert_eq!(metadata.embedded_lyrics.as_ref().unwrap(), &lyric_bundle);
    }
    let connection = rusqlite::Connection::open(
        running
            .profile()
            .join(lx_gpui_player::repository::DATABASE_NAME),
    )
    .unwrap();
    let mut statement = connection
        .prepare("SELECT document FROM download_owned_artifacts WHERE task_id=?1")
        .unwrap();
    let original_allocation = statement
        .query_map([&completed.id], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| {
            serde_json::from_str::<lx_gpui_player::download::artifact::OwnedArtifactAllocation>(
                &row.unwrap(),
            )
            .unwrap()
        })
        .find(|allocation| {
            allocation.kind == lx_gpui_player::download::artifact::OwnedArtifactKind::Transfer
        })
        .unwrap();
    let raw_path = original_allocation
        .working_directory_path
        .join(&original_allocation.record.basename);
    drop(statement);
    drop(connection);
    fixture::wait(&running.backend, 5.0, |_| !raw_path.exists());
    let accounting = rusqlite::Connection::open(
        running
            .profile()
            .join(lx_gpui_player::repository::DATABASE_NAME),
    )
    .unwrap();
    if case == CompleteCase::CleanupAckFailure {
        fixture::wait(&running.backend, 5.0, |snapshot| {
            snapshot
                .error
                .as_ref()
                .is_some_and(|error| error.contains("injected cleanup ACK failure"))
        });
        assert_eq!(
            accounting
                .query_row(
                    "SELECT phase FROM download_owned_artifacts WHERE nonce=?1",
                    [&original_allocation.record.nonce],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "verified"
        );
        let pending_task = rows(&running).pop().unwrap();
        running.execute(
            "cleanup-cancel-pending-owner",
            BackendCommand::CancelDownloadCleanup {
                task_id: pending_task.id.clone(),
                expected_task_revision: pending_task.revision,
            },
        );
        fixture::wait(&running.backend, 5.0, |_| {
            let BackendQueryResult::DownloadCleanup(view) = running
                .backend
                .query(BackendQuery::DownloadCleanup {
                    task_id: pending_task.id.clone(),
                })
                .unwrap()
            else {
                panic!("Cleanup view")
            };
            !view.active && !view.pending
        });
        drop(accounting);
        running.restart();
        running.execute("cleanup-recovery-source-off", BackendCommand::DisableSource);
        let repair = rusqlite::Connection::open(
            running
                .profile()
                .join(lx_gpui_player::repository::DATABASE_NAME),
        )
        .unwrap();
        repair
            .execute_batch("DROP TRIGGER fail_cleanup_accounting;")
            .unwrap();
        drop(repair);
        let row = rows(&running).pop().unwrap();
        let cursor = running.backend.event_cursor();
        running.execute(
            "cleanup-accounting-recovery",
            BackendCommand::RecoverDownloadCleanup {
                task_id: row.id.clone(),
                expected_task_revision: row.revision,
            },
        );
        assert!(
            fixture::operation_finished(&running.backend, cursor, "cleanup-accounting-recovery").0
        );
        assert!(!raw_path.exists());
        assert!(output.is_file());
        assert_eq!(std::fs::read(&output).unwrap(), published_body);
        let BackendQueryResult::DownloadCleanup(view) = running
            .backend
            .query(BackendQuery::DownloadCleanup { task_id: row.id })
            .unwrap()
        else {
            panic!("Cleanup view")
        };
        assert!(view.accounting_removed);
        assert!(!view.pending);
        assert!(!view.active);
        return;
    }
    fixture::wait(&running.backend, 5.0, |_| {
        accounting
            .query_row(
                "SELECT phase FROM download_owned_artifacts WHERE nonce=?1",
                [&original_allocation.record.nonce],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            == "removed"
    });
    drop(accounting);
    running.restart();
    let after = rows(&running).pop().unwrap();
    assert_eq!(after.state, expected_state);
    assert_eq!(after.id, completed.id);
    if matches!(case, CompleteCase::RetryCover | CompleteCase::RetryTags) {
        use lx_gpui_player::domain::{artwork::*, lyric::LyricOrigin};
        use lx_gpui_player::download::{
            DownloadError, DownloadErrorKind, task_model::DownloadStageState,
        };
        running.execute("tag-retry-source-off", BackendCommand::DisableSource);
        running.backend.shutdown(Duration::from_secs(5)).unwrap();
        let repo = lx_gpui_player::repository::Repository::open(running.profile()).unwrap();
        let mut recorded = repo.download_task(&after.id).unwrap().unwrap();
        let before_proof = recorded.output_audio.clone().unwrap();
        let before_body = std::fs::read(&output).unwrap();
        let before_stages = recorded.stages.clone();
        let before_sidecar = recorded
            .stages
            .lyrics
            .artifact
            .as_ref()
            .map(|_| std::fs::read(&sidecar).unwrap());
        let mut png = Vec::new();
        if case == CompleteCase::RetryCover {
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&[9, 87, 201, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .unwrap();
            let verified = lx_gpui_player::library::artwork_validator::validate_artwork(
                png.clone(),
                Some("image/png"),
                || Ok(()),
            )
            .unwrap();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let info = ArtworkInfo::new(
                ArtworkKey::new(&recorded.original_record, None, ArtworkAuthority::Native).unwrap(),
                ArtworkProvenance::Metadata {
                    platform: "kw".into(),
                },
                Some(ArtworkUrl::new("http://127.0.0.1:1/cached-original-fixture.png").unwrap()),
                Some(verified.metadata().clone()),
                now,
            )
            .unwrap();
            repo.put_artwork(&info, Some(&verified)).unwrap();
        } else {
            let revision = recorded.revision;
            let epoch = recorded.operation_epoch;
            let fault = DownloadError {
                kind: DownloadErrorKind::Verification,
                message: "Injected postpublication stage acknowledgement failure".into(),
                http_status: None,
            };
            recorded.stages.metadata.state = DownloadStageState::Failed;
            recorded.stages.metadata.error = Some(fault.clone());
            recorded.stages.embedded_lyrics.state = DownloadStageState::Failed;
            recorded.stages.embedded_lyrics.error = Some(fault.clone());
            recorded.state = DownloadState::Failed;
            recorded.error = Some(fault);
            recorded.revision += 1;
            recorded.updated_at_ms += 1;
            repo.update_download_task(&recorded, revision, epoch, None, &|| Ok(()))
                .unwrap();
            let replacement =
                serde_json::from_value(serde_json::json!({"lyric":"[00:00.00]重试内嵌歌词"}))
                    .unwrap();
            repo.replace_lyrics(
                &recorded.original_record.id,
                LyricOrigin::Edited,
                &replacement,
            )
            .unwrap();
        }
        drop(repo);
        running.restart();
        let cursor = running.backend.event_cursor();
        running.execute(
            "same-object-tag-retry",
            BackendCommand::RetryDownloadPostprocessing {
                task_id: after.id.clone(),
            },
        );
        let completion =
            fixture::operation_finished(&running.backend, cursor, "same-object-tag-retry");
        assert!(
            completion.0,
            "{completion:?}; error={:?}; events={:?}; tasks={:?}",
            running.backend.snapshot().error,
            running.backend.events_after(cursor).unwrap(),
            rows(&running)
        );
        let done = rows(&running).pop().unwrap();
        assert_eq!(done.state, DownloadState::Completed);
        let proof = done.output_audio.as_ref().unwrap();
        assert_eq!(proof.identity, before_proof.identity);
        assert_ne!(proof.sha256, before_proof.sha256);
        assert_ne!(std::fs::read(&output).unwrap(), before_body);
        let metadata = lx_gpui_player::library::metadata::read_local_metadata(&output).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("下载标题"));
        assert_eq!(metadata.album.as_deref(), Some("下载专辑"));
        if case == CompleteCase::RetryCover {
            assert!(metadata.pictures.iter().any(|p| p.data == png));
        } else {
            assert_eq!(
                metadata.embedded_lyrics.as_ref().unwrap().lyric.text(),
                Some("[00:00.00]重试内嵌歌词")
            );
            assert_eq!(std::fs::read(&sidecar).unwrap(), before_sidecar.unwrap());
        }
        running.backend.shutdown(Duration::from_secs(5)).unwrap();
        let repo = lx_gpui_player::repository::Repository::open(running.profile()).unwrap();
        let task = repo.download_task(&done.id).unwrap().unwrap();
        assert!(task.commit_journal.is_none());
        assert_eq!(task.stages.lyrics, before_stages.lyrics);
        if case == CompleteCase::RetryCover {
            assert_eq!(task.stages.metadata, before_stages.metadata);
            assert_eq!(task.stages.embedded_lyrics, before_stages.embedded_lyrics);
        }
        assert_eq!(
            repo.download_owned_artifacts(Some(&done.id))
                .unwrap()
                .iter()
                .map(|a| a.reserved_bytes)
                .sum::<u64>(),
            0
        );
        drop(repo);
        return;
    }
    if matches!(
        case,
        CompleteCase::RetrySidecar
            | CompleteCase::RetryForeignSidecar
            | CompleteCase::RetryForeignAudio
    ) {
        running.execute("retry-source-disabled", BackendCommand::DisableSource);
        let original_proof = after.output_audio.clone().unwrap();
        let before = std::fs::read(&output).unwrap();
        let sql = rusqlite::Connection::open(
            running
                .profile()
                .join(lx_gpui_player::repository::DATABASE_NAME),
        )
        .unwrap();
        let document: String = sql
            .query_row(
                "SELECT document FROM download_tasks WHERE id=?1",
                [&after.id],
                |r| r.get(0),
            )
            .unwrap();
        let original_task: lx_gpui_player::download::task_model::DownloadTask =
            serde_json::from_str(&document).unwrap();
        drop(sql);
        if case == CompleteCase::RetrySidecar {
            std::fs::remove_file(&sidecar).unwrap();
        }
        if case == CompleteCase::RetryForeignAudio {
            std::fs::rename(&output, destination.join("preserved-user-audio.flac")).unwrap();
            std::fs::write(&output, b"foreign audio").unwrap();
        }
        let cursor = running.backend.event_cursor();
        running.execute(
            "published-stage-retry",
            BackendCommand::RetryDownloadPostprocessing {
                task_id: after.id.clone(),
            },
        );
        let (success, _) =
            fixture::operation_finished(&running.backend, cursor, "published-stage-retry");
        assert_eq!(success, case == CompleteCase::RetrySidecar);
        let repaired = rows(&running).pop().unwrap();
        assert_eq!(repaired.output_audio, Some(original_proof));
        let sql = rusqlite::Connection::open(
            running
                .profile()
                .join(lx_gpui_player::repository::DATABASE_NAME),
        )
        .unwrap();
        let document: String = sql
            .query_row(
                "SELECT document FROM download_tasks WHERE id=?1",
                [&after.id],
                |r| r.get(0),
            )
            .unwrap();
        let repaired_task: lx_gpui_player::download::task_model::DownloadTask =
            serde_json::from_str(&document).unwrap();
        assert_eq!(repaired_task.stages.metadata, original_task.stages.metadata);
        assert_eq!(repaired_task.stages.cover, original_task.stages.cover);
        assert_eq!(
            repaired_task.stages.embedded_lyrics,
            original_task.stages.embedded_lyrics
        );
        assert_eq!(
            repaired_task.transfer_attempts,
            original_task.transfer_attempts
        );
        assert_eq!(
            repaired_task.original_transfer,
            original_task.original_transfer
        );
        assert!(repaired_task.commit_journal.is_none());
        let pending:i64=sql.query_row("SELECT coalesce(sum(reserved_bytes),0) FROM download_owned_artifacts WHERE task_id=?1",[&after.id],|r|r.get(0)).unwrap();
        assert!(pending >= 0);
        assert_eq!(pending, 0);
        drop(sql);
        if case == CompleteCase::RetrySidecar {
            assert_eq!(repaired.state, DownloadState::Completed);
            let bytes = std::fs::read(&sidecar).unwrap();
            assert!(std::str::from_utf8(&bytes).unwrap().contains("原创歌词"));
            assert_eq!(std::fs::read(&output).unwrap(), before);
        } else if case == CompleteCase::RetryForeignSidecar {
            assert_eq!(repaired.state, DownloadState::Failed);
            assert_eq!(std::fs::read(&sidecar).unwrap(), b"original user lyric");
            assert_eq!(std::fs::read(&output).unwrap(), before);
        } else {
            assert_eq!(repaired.state, DownloadState::Failed);
            assert_eq!(std::fs::read(&output).unwrap(), b"foreign audio");
            assert_eq!(
                std::fs::read(destination.join("preserved-user-audio.flac")).unwrap(),
                before
            );
        }
        running.restart();
        assert_eq!(
            rows(&running).pop().unwrap().output_audio,
            repaired.output_audio
        );
        return;
    }
    if case == CompleteCase::LocalPlayWithoutSource {
        fixture::wait(
            &running.backend,
            15.0,
            |_| matches!(running.backend.query(BackendQuery::LibraryAssets { root_id: None, after_asset_id: None, limit: 8, expected_revision: None }), Ok(BackendQueryResult::LibraryAssets(page)) if page.items.iter().any(|view| view.asset.path == output)),
        );
        running.execute(
            "download-local-disable-source",
            BackendCommand::DisableSource,
        );
        let BackendQueryResult::LibraryAssets(assets) = running
            .backend
            .query(BackendQuery::LibraryAssets {
                root_id: None,
                after_asset_id: None,
                limit: 8,
                expected_revision: None,
            })
            .unwrap()
        else {
            panic!("Downloaded asset page")
        };
        let expected_track = assets
            .items
            .iter()
            .find(|view| view.asset.path == output)
            .unwrap()
            .asset
            .track_id
            .clone();
        let before_play = running.backend.snapshot().playback.generation;
        let cursor = running.backend.event_cursor();
        running.execute(
            "download-local-play",
            BackendCommand::PlayDownloadTask {
                task_id: after.id.clone(),
            },
        );
        assert_eq!(
            fixture::operation_finished(&running.backend, cursor, "download-local-play"),
            (true, None)
        );
        fixture::wait(&running.backend, 15.0, |before| {
            let pcm = fixture::pcm(&running.backend);
            let after = running.backend.snapshot();
            before.playback.track_id.as_deref() == Some(expected_track.as_str())
                && after.playback.track_id == before.playback.track_id
                && after.playback.generation == before.playback.generation
                && after.playback.generation > before_play
                && after.playback.has_presented_media
                && after.playback.position_seconds > 0.0
                && after.playback.error.is_none()
                && after.playback.failure.is_none()
                && pcm.batches >= 2
                && pcm.peak > 0.01
                && pcm.rms > 0.001
        });
        let before_pcm = running.backend.snapshot();
        let actual_pcm = fixture::pcm(&running.backend);
        let after_pcm = running.backend.snapshot();
        assert_eq!(
            before_pcm.playback.track_id.as_deref(),
            Some(expected_track.as_str())
        );
        assert_eq!(after_pcm.playback.track_id, before_pcm.playback.track_id);
        assert_eq!(
            after_pcm.playback.generation,
            before_pcm.playback.generation
        );
        assert!(
            after_pcm.playback.has_presented_media && after_pcm.playback.generation > before_play
        );
        assert!(after_pcm.playback.error.is_none() && after_pcm.playback.failure.is_none());
        assert!(actual_pcm.batches >= 2 && actual_pcm.rms > 0.001 && actual_pcm.peak > 0.01);
        let BackendQueryResult::Queue(queue) = running
            .backend
            .query(BackendQuery::Queue {
                offset: 0,
                limit: 8,
            })
            .unwrap()
        else {
            panic!("Downloaded playback queue")
        };
        assert_eq!(queue.mode, lx_gpui_player::playback::queue::PlayMode::None);
        assert!(!queue.auto_skip_on_error && queue.temporary.is_empty() && queue.next.is_none());
        assert_eq!(
            queue.current_track.as_ref().map(|track| track.id.as_str()),
            Some(expected_track.as_str())
        );
        let sql = rusqlite::Connection::open_with_flags(
            running
                .profile()
                .join(lx_gpui_player::repository::DATABASE_NAME),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            sql.query_row("SELECT count(*) FROM local_file_grants", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            sql.query_row("SELECT count(*) FROM local_asset_locations", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            1
        );
        drop(sql);
        running.execute("download-local-stop", BackendCommand::Stop);
    }
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    let repo = lx_gpui_player::repository::Repository::open(running.profile()).unwrap();
    let artifacts = repo.download_owned_artifacts(Some(&after.id)).unwrap();
    let original = artifacts
        .iter()
        .find(|artifact| {
            artifact.allocation.kind
                == lx_gpui_player::download::artifact::OwnedArtifactKind::Transfer
        })
        .unwrap();
    assert_eq!(original.phase, "removed");
    assert_eq!(original.reserved_bytes, 0);
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact.phase == "published"
                && artifact.allocation.record.identity
                    == after.output_audio.as_ref().unwrap().identity)
    );
    assert!(output.is_file());
    if case == CompleteCase::PublishedSidecarReplaced {
        use lx_gpui_player::download::task_model::{
            DownloadArtifactProof, DownloadCommitJournal, DownloadCommitPhase, DownloadPauseReason,
        };
        let mut interrupted = repo.download_task(&after.id).unwrap().unwrap();
        let revision = interrupted.revision;
        let epoch = interrupted.operation_epoch;
        let original_part = original.allocation.record.clone();
        let candidate = artifacts
            .iter()
            .find(|artifact| {
                artifact.allocation.kind
                    == lx_gpui_player::download::artifact::OwnedArtifactKind::TagCopy
            })
            .unwrap();
        let audio_proof = interrupted.output_audio.clone().unwrap();
        let lyric_proof = interrupted.stages.lyrics.artifact.clone().unwrap();
        interrupted.commit_journal = Some(DownloadCommitJournal {
            task_revision: revision,
            operation_epoch: epoch,
            grant_epoch: interrupted.destination.grant_epoch,
            phase: DownloadCommitPhase::Verified,
            original_part: original_part.clone(),
            candidate: DownloadArtifactProof {
                basename: candidate.allocation.record.basename.clone(),
                ..audio_proof.clone()
            },
            observed_target: None,
            rollback_original: None,
            sidecar: Some(lyric_proof),
            retry: None,
        });
        interrupted.output_audio = None;
        interrupted.audio_available = false;
        interrupted.owned_part = Some(original_part);
        interrupted.stages.lyrics.artifact = None;
        interrupted.stages.lyrics.state =
            lx_gpui_player::download::task_model::DownloadStageState::Pending;
        interrupted.state = DownloadState::Paused;
        interrupted.pause_reason = Some(DownloadPauseReason::Restart);
        interrupted.revision += 1;
        interrupted.updated_at_ms += 1;
        repo.update_download_task(&interrupted, revision, epoch, None, &|| Ok(()))
            .unwrap();
        let displaced = destination.join("displaced-original-sidecar.lrc");
        std::fs::rename(&sidecar, &displaced).unwrap();
        std::fs::write(&sidecar, b"foreign user sidecar").unwrap();
        drop(repo);
        running.restart();
        running.execute(
            "foreign-sidecar-source-disabled",
            BackendCommand::DisableSource,
        );
        let row = rows(&running).pop().unwrap();
        let cursor = running.backend.event_cursor();
        running.execute(
            "foreign-sidecar-recovery",
            BackendCommand::RecoverDownloadCommit {
                task_id: row.id,
                expected_task_revision: row.revision,
            },
        );
        let completion =
            fixture::operation_finished(&running.backend, cursor, "foreign-sidecar-recovery");
        assert!(
            completion.0,
            "{completion:?}; events={:?}; tasks={:?}",
            running.backend.events_after(cursor).unwrap(),
            rows(&running)
        );
        let recovered = rows(&running).pop().unwrap();
        assert_eq!(recovered.state, DownloadState::Failed);
        assert!(recovered.audio_available);
        assert_eq!(recovered.output_audio.as_ref(), Some(&audio_proof));
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"foreign user sidecar");
        assert!(displaced.is_file());
        assert!(output.is_file());
        return;
    }
    drop(repo);
}
