#[path = "support/native_producer_helpers.rs"]
#[allow(
    dead_code,
    reason = "Shared native fixtures expose constructors used by sibling integration targets"
)]
mod native_producer_helpers;

#[cfg(not(windows))]
#[test]
fn unsupported_native_cleanup_does_not_write_profile_journals() {
    let profile = native_producer_helpers::fixture_directory().unwrap();
    assert!(lx_gpui_player::download::cleanup::CleanupJournal::open(profile.path()).is_err());
    assert_eq!(std::fs::read_dir(profile.path()).unwrap().count(), 0);
}

#[cfg(windows)]
#[allow(
    dead_code,
    reason = "Shared encoded fixtures expose other format constructors to sibling tests"
)]
#[path = "support/encoded_audio_fixture.rs"]
mod encoded;

#[cfg(windows)]
mod windows {
    use super::{encoded, native_producer_helpers};
    use anyhow::{Context, Result, ensure};
    use lx_gpui_player::{
        domain::{
            media::track_record_fingerprint,
            music::{LxMusicDtoVersion, TrackRecord},
        },
        download::{
            AudioVerifier, CollisionPolicy, DestinationGrant, DownloadCancel, DownloadContext,
            DownloadPlan, DownloadRepresentationIdentity, DownloadState, FileIdentity, OwnedPart,
            SymphoniaVerifier,
            artifact::{OwnedArtifactAllocation, OwnedArtifactKind},
            cleanup::{
                CleanupBoundary, CleanupCoordinator, CleanupIntent, CleanupJournal, CleanupOwner,
                CleanupPhase, NativeRemovedReceipt,
            },
            planning::DownloadQuality,
            task_model::*,
        },
        repository::{DownloadOwnedArtifact, Repository},
    };
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FILE_SHARE_READ, FileIdInfo, GetFileInformationByHandleEx,
    };

    fn identity(path: &Path) -> FileIdentity {
        let file = File::open(path).unwrap();
        let mut info = FILE_ID_INFO::default();
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            },
            0
        );
        FileIdentity {
            device: info.VolumeSerialNumber,
            object: info
                .FileId
                .Identifier
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }

    fn context() -> DownloadContext {
        DownloadContext {
            profile_id: "cleanup-profile".into(),
            task_id: "cleanup-task".into(),
            operation_epoch: 2,
            source_id: "inert-original-source".into(),
            source_content_sha256: "11".repeat(32),
            policy_epoch: 1,
        }
    }

    fn save(repo: &Repository, task: &mut DownloadTask, previous_epoch: u64) {
        let revision = task.revision;
        task.revision += 1;
        task.updated_at_ms += 1;
        repo.update_download_task(task, revision, previous_epoch, None, &|| Ok(()))
            .unwrap();
    }

    struct Coordinator {
        repository: Arc<Repository>,
        stop_before_remove: AtomicBool,
        ack_calls: AtomicUsize,
    }

    impl CleanupCoordinator for Coordinator {
        fn validate(&self, intent: &CleanupIntent, boundary: CleanupBoundary) -> Result<()> {
            let task = self
                .repository
                .download_task(&intent.allocation.context.task_id)?
                .context("Cleanup task missing")?;
            let allocation = self
                .repository
                .download_owned_artifacts(Some(&task.id))?
                .into_iter()
                .find(|artifact| artifact.allocation.record.nonce == intent.allocation.record.nonce)
                .context("Cleanup original allocation missing")?;
            ensure!(
                allocation.allocation == intent.allocation
                    && task.profile_id == intent.allocation.context.profile_id
                    && task.revision >= intent.task_revision
                    && task.audio_available
                    && task.output_audio.as_ref() == Some(&intent.final_output)
                    && task.owned_part.is_none()
                    && task.commit_journal.is_none(),
                "Current durable cleanup task or allocation differs"
            );
            ensure!(
                allocation.phase == "verified"
                    || (boundary == CleanupBoundary::Acknowledge && allocation.phase == "removed"),
                "Cleanup original is not in an admissible accounting phase"
            );
            if allocation.phase == "verified" {
                ensure!(
                    allocation.actual_bytes == intent.raw_bytes
                        && allocation.sha256.as_deref() == Some(intent.raw_sha256.as_str()),
                    "Cleanup durable original digest differs"
                );
            }
            let grant = self
                .repository
                .download_destinations()?
                .into_iter()
                .find(|grant| grant.id == task.destination.grant_id)
                .context("Cleanup destination missing")?;
            task.require_current_destination(&grant)?;
            if boundary == CleanupBoundary::Remove {
                ensure!(
                    !self.stop_before_remove.load(Ordering::SeqCst),
                    "Paused before actual native removal"
                );
            }
            Ok(())
        }

        fn acknowledge(&self, receipt: &NativeRemovedReceipt) -> Result<()> {
            self.ack_calls.fetch_add(1, Ordering::SeqCst);
            self.validate(receipt.intent(), CleanupBoundary::Acknowledge)?;
            self.repository.acknowledge_removed_download_original(
                &receipt.intent().allocation.context.task_id,
                receipt.intent().allocation.context.operation_epoch,
                receipt.removed_record(),
            )
        }
    }

    struct Case {
        profile: PathBuf,
        grant: DestinationGrant,
        plan: Option<DownloadPlan>,
        raw_path: PathBuf,
        final_path: PathBuf,
        body: Vec<u8>,
        intent: CleanupIntent,
        coordinator: Coordinator,
        _directory: native_producer_helpers::FixtureDirectory,
    }
    impl Drop for Case {
        fn drop(&mut self) {
            drop(self.plan.take());
            if std::thread::panicking() || self.raw_path.exists() {
                self._directory.preserve();
                native_producer_helpers::retain_failed_grant(self.grant.clone());
                return;
            }
            if self.grant.close_retained_unique().is_err() {
                self._directory.preserve();
                native_producer_helpers::retain_failed_grant(self.grant.clone());
            }
        }
    }

    impl Case {
        fn new() -> Self {
            let directory = native_producer_helpers::fixture_directory().unwrap();
            let profile = directory.path().join("profile");
            let media = directory.path().join("media");
            fs::create_dir(&media).unwrap();
            let repository = Arc::new(Repository::open(&profile).unwrap());
            let grant = DestinationGrant::open_selected(&media, 1).unwrap();
            repository
                .put_download_destination(
                    &StoredDestinationGrant {
                        id: "cleanup-destination".into(),
                        profile_id: context().profile_id,
                        path: grant.path().to_owned(),
                        identity: grant.identity().clone(),
                        epoch: 1,
                        revision: 1,
                        revoked: false,
                    },
                    None,
                    None,
                    &|| Ok(()),
                )
                .unwrap();
            let settings = DownloadSettings {
                embed_cover: false,
                ..Default::default()
            };
            let mut task = DownloadTask {
                id: context().task_id,
                profile_id: context().profile_id,
                original_record: TrackRecord::from_lx_dto(
                    serde_json::json!({"id":"kw_cleanup",
                    "source":"kw","name":"Native disposal fixture","meta":{"songId":"cleanup"}}),
                    LxMusicDtoVersion::Modern,
                )
                .unwrap(),
                resolved_record: None,
                requested_quality: DownloadQuality::Flac,
                actual_quality: DownloadQuality::Flac,
                list_id: None,
                list_name_snapshot: None,
                order: 0,
                revision: 1,
                operation_epoch: 1,
                settings: settings.clone(),
                destination: DownloadDestinationBinding {
                    grant_id: "cleanup-destination".into(),
                    grant_epoch: 1,
                    root_identity: grant.identity().clone(),
                    relative_components: vec!["published.flac".into()],
                    collision: CollisionPolicy::Skip,
                },
                state: DownloadState::Waiting,
                pause_reason: None,
                owned_part: None,
                original_transfer: None,
                original_representation: None,
                url_bookmark: None,
                transfer_attempts: 0,
                retry_at_ms: None,
                error: None,
                output_audio: None,
                audio_available: false,
                stages: DownloadStageRecords::for_settings(&settings, DownloadQuality::Flac),
                commit_journal: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            };
            repository
                .insert_download_tasks(&[task.clone()], None, &|| Ok(()))
                .unwrap();
            task.operation_epoch = 2;
            task.state = DownloadState::Paused;
            task.url_bookmark = Some(
                PrivateUrlBookmark::new("https://fixture.invalid/original.flac".into(), context())
                    .unwrap(),
            );
            save(&repository, &mut task, 1);
            let plan = grant.plan("published.flac", CollisionPolicy::Skip).unwrap();
            let journal = Arc::new(native_producer_helpers::RepositoryJournal::new(
                repository.clone(),
            ));
            let mut part = journal
                .create(
                    plan.clone(),
                    OwnedArtifactKind::Transfer,
                    context(),
                    65_536,
                    DownloadCancel::default(),
                )
                .unwrap();
            let allocation = OwnedArtifactAllocation {
                kind: OwnedArtifactKind::Transfer,
                record: part.record().clone(),
                working_directory_path: grant.path().to_owned(),
                working_directory_identity: grant.working_directory_identity().clone(),
                selected_root_identity: grant.identity().clone(),
                relative_group: None,
                reserved_max_bytes: 65_536,
                context: context(),
            };
            let body = encoded::mono_verbatim_flac();
            let cancel = DownloadCancel::default();
            let work_body = body.clone();
            let work_cancel = cancel.clone();
            let (returned, mut checkpoint) =
                lx_gpui_player::download::run_download_producer(move || {
                    part.rewrite_io(work_cancel.clone())?
                        .write_all(&work_body)?;
                    SymphoniaVerifier.verify(&part, "flac", &work_cancel)?;
                    let checkpoint =
                        part.checkpoint(&work_cancel, None, Some(work_body.len() as u64), true)?;
                    Ok((part, checkpoint))
                })
                .unwrap();
            part = returned;
            checkpoint.representation = Some(DownloadRepresentationIdentity {
                profile_id: context().profile_id,
                task_id: context().task_id,
                source_id: context().source_id,
                source_content_sha256: context().source_content_sha256,
                policy_epoch: context().policy_epoch,
                exact_url_sha256: task.url_bookmark.as_ref().unwrap().sha256.clone(),
                resolved_track_sha256: track_record_fingerprint(&task.original_record).unwrap(),
                actual_quality: DownloadQuality::Flac,
            });
            repository
                .verify_download_artifact(
                    &allocation.record.nonce,
                    2,
                    checkpoint.bytes,
                    &checkpoint.sha256,
                    &|| Ok(()),
                )
                .unwrap();
            let raw_path = part.path();
            OwnedPart::close_retained(&plan, part.record()).unwrap();
            drop(part);
            let final_path = grant.path().join("published.flac");
            fs::write(&final_path, &body).unwrap();
            let final_output = DownloadArtifactProof {
                basename: "published.flac".into(),
                identity: identity(&final_path),
                bytes: body.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&body)),
            };
            assert_ne!(final_output.identity, allocation.record.identity);
            grant
                .verify_owned_artifact(
                    &final_output.basename,
                    &final_output.identity,
                    final_output.bytes,
                    &final_output.sha256,
                    &cancel,
                )
                .unwrap();
            task.original_representation = checkpoint.representation.clone();
            task.original_transfer = Some(checkpoint.clone());
            task.audio_available = true;
            task.output_audio = Some(final_output.clone());
            save(&repository, &mut task, 2);
            let intent = CleanupIntent {
                allocation,
                raw_bytes: checkpoint.bytes,
                raw_sha256: checkpoint.sha256,
                final_output,
                task_revision: task.revision,
            };
            let coordinator = Coordinator {
                repository,
                stop_before_remove: AtomicBool::new(false),
                ack_calls: AtomicUsize::new(0),
            };
            Self {
                _directory: directory,
                profile,
                grant,
                plan: Some(plan),
                raw_path,
                final_path,
                body,
                intent,
                coordinator,
            }
        }

        fn journal_path(&self) -> PathBuf {
            self.profile.join(format!(
                ".lx-cleanup-{}.journal",
                self.intent.allocation.record.nonce
            ))
        }
        fn journal(&self) -> CleanupJournal {
            CleanupJournal::open(&self.profile).unwrap()
        }
        fn owner(&self) -> CleanupOwner {
            CleanupOwner::recover(self.journal(), self.intent.clone(), self.grant.clone())
        }
        fn row(&self) -> DownloadOwnedArtifact {
            self.coordinator
                .repository
                .download_owned_artifacts(Some(&context().task_id))
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
        }
        fn prepared(&self) {
            self.coordinator
                .stop_before_remove
                .store(true, Ordering::SeqCst);
            let mut owner = self.owner();
            assert!(owner.advance(&self.coordinator).is_err());
            assert_eq!(owner.phase(), Some(CleanupPhase::Prepared));
            assert!(!owner.physical_removal_proven());
            assert!(self.raw_path.exists());
            drop(owner);
            // A cold gap requires an actual joined close, not merely dropping
            // a borrow of the process-kept original descriptor.
            OwnedPart::close_retained(self.plan.as_ref().unwrap(), &self.intent.allocation.record)
                .unwrap();
            self.coordinator
                .stop_before_remove
                .store(false, Ordering::SeqCst);
        }
    }

    #[test]
    fn actual_original_removal_is_journaled_then_acked_without_source_activation_or_user_revision()
    {
        let case = Case::new();
        let revision = case.coordinator.repository.revision().unwrap();
        let mut owner = case.owner();
        assert_eq!(
            owner.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert!(owner.physical_removal_proven());
        assert!(!case.raw_path.exists());
        assert_eq!(fs::read(&case.final_path).unwrap(), case.body);
        assert_eq!(case.row().phase, "removed");
        assert_eq!(case.row().reserved_bytes, 0);
        assert_eq!(case.coordinator.repository.revision().unwrap(), revision);
        assert!(
            case.coordinator
                .repository
                .changes_since(revision, 10)
                .unwrap()
                .is_empty()
        );
        drop(owner);
        let snapshot = case
            .journal()
            .load(&case.intent.allocation.record.nonce)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.phase, CleanupPhase::Acknowledged);
        let bytes = fs::read(case.journal_path()).unwrap();
        let mut offset = 8;
        let mut phases = Vec::new();
        while offset < bytes.len() {
            let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let record: serde_json::Value =
                serde_json::from_slice(&bytes[offset + 36..offset + 36 + length]).unwrap();
            assert_eq!(record["version"], 2);
            assert_eq!(record["prepared_evidence"]["filesystem"], "NTFS");
            phases.push(record["phase"].as_str().unwrap().to_owned());
            offset += 36 + length;
        }
        assert_eq!(phases, ["prepared", "prepared", "removed", "acknowledged"]);
        let calls = case.coordinator.ack_calls.load(Ordering::SeqCst);
        let mut replay = case.owner();
        assert_eq!(
            replay.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert_eq!(case.coordinator.ack_calls.load(Ordering::SeqCst), calls);
    }

    #[test]
    fn failure_before_native_delete_retains_the_original_and_final_fences_for_retry() {
        let case = Case::new();
        case.coordinator
            .stop_before_remove
            .store(true, Ordering::SeqCst);
        let mut owner = case.owner();
        assert!(owner.advance(&case.coordinator).is_err());
        assert_eq!(owner.phase(), Some(CleanupPhase::Prepared));
        assert!(fs::rename(&case.raw_path, case.raw_path.with_extension("moved")).is_err());
        assert!(fs::write(&case.final_path, b"foreign replacement").is_err());
        assert_eq!(case.row().phase, "verified");
        case.coordinator
            .stop_before_remove
            .store(false, Ordering::SeqCst);
        assert_eq!(
            owner.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert_eq!(fs::read(&case.final_path).unwrap(), case.body);
    }

    #[test]
    fn actual_whole_digest_failures_preserve_files_and_never_prepare_a_delete() {
        for change_final in [false, true] {
            let case = Case::new();
            let path = if change_final {
                &case.final_path
            } else {
                &case.raw_path
            };
            let mut changed = case.body.clone();
            let middle = changed.len() / 2;
            changed[middle] ^= 1;
            fs::write(path, &changed).unwrap();
            let mut owner = case.owner();
            assert!(owner.advance(&case.coordinator).is_err());
            assert!(owner.phase().is_none());
            assert!(case.raw_path.exists());
            assert_eq!(case.row().phase, "verified");
            assert_eq!(fs::read(path).unwrap(), changed);
            assert!(
                case.journal()
                    .load(&case.intent.allocation.record.nonce)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn missing_nonce_name_cannot_hide_the_same_fileid_renamed_inside_or_outside_its_namespace() {
        for outside in [false, true] {
            let case = Case::new();
            case.prepared();
            let moved = if outside {
                case._directory.path().join("outside-original.flac")
            } else {
                case.grant.path().join("renamed-original.flac")
            };
            fs::rename(&case.raw_path, &moved).unwrap();
            assert_eq!(identity(&moved), case.intent.allocation.record.identity);
            let mut owner = case.owner();
            let error = owner.advance(&case.coordinator).unwrap_err();
            assert!(
                format!("{error:#}").contains("FileID still exists"),
                "{error:#}"
            );
            assert_eq!(owner.phase(), Some(CleanupPhase::Prepared));
            assert!(!owner.physical_removal_proven());
            assert_eq!(fs::read(&moved).unwrap(), case.body);
            assert_eq!(case.row().phase, "verified");
        }
    }

    #[test]
    fn prepared_gap_uses_actual_absent_fileid_and_keeps_a_foreign_reused_nonce_name() {
        let case = Case::new();
        case.prepared();
        fs::remove_file(&case.raw_path).unwrap();
        let foreign = b"independent foreign object at a reused nonce name";
        fs::write(&case.raw_path, foreign).unwrap();
        let foreign_id = identity(&case.raw_path);
        assert_ne!(foreign_id, case.intent.allocation.record.identity);
        let mut owner = case.owner();
        assert_eq!(
            owner.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert_eq!(fs::read(&case.raw_path).unwrap(), foreign);
        assert_eq!(identity(&case.raw_path), foreign_id);
        assert_eq!(case.row().reserved_bytes, 0);
        assert_eq!(fs::read(&case.final_path).unwrap(), case.body);
    }

    #[test]
    fn unapproved_original_hardlink_is_retained_without_releasing_accounting() {
        let case = Case::new();
        case.prepared();
        let alias = case.grant.path().join("original-alias.flac");
        fs::hard_link(&case.raw_path, &alias).unwrap();
        let mut owner = case.owner();
        assert!(owner.advance(&case.coordinator).is_err());
        assert_eq!(owner.phase(), Some(CleanupPhase::Prepared));
        assert_eq!(case.row().phase, "verified");
        assert_eq!(fs::read(&case.raw_path).unwrap(), case.body);
        assert_eq!(fs::read(&alias).unwrap(), case.body);
    }

    #[test]
    fn occupied_original_reader_blocks_exclusive_recovery_and_retries_after_real_close() {
        let case = Case::new();
        case.prepared();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&case.raw_path)
            .unwrap();
        let mut owner = case.owner();
        assert!(owner.advance(&case.coordinator).is_err());
        assert_eq!(case.row().phase, "verified");
        assert!(!owner.physical_removal_proven());
        drop(reader);
        assert_eq!(
            owner.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert!(!case.raw_path.exists());
    }

    #[test]
    fn actual_readonly_original_delete_failure_keeps_prepared_ownership_until_retry() {
        let case = Case::new();
        case.coordinator
            .stop_before_remove
            .store(true, Ordering::SeqCst);
        let mut owner = case.owner();
        assert!(owner.advance(&case.coordinator).is_err());
        let original_permissions = fs::metadata(&case.raw_path).unwrap().permissions();
        let mut permissions = original_permissions.clone();
        permissions.set_readonly(true);
        fs::set_permissions(&case.raw_path, permissions).unwrap();
        case.coordinator
            .stop_before_remove
            .store(false, Ordering::SeqCst);
        assert!(owner.advance(&case.coordinator).is_err());
        assert_eq!(owner.phase(), Some(CleanupPhase::Prepared));
        assert!(!owner.physical_removal_proven());
        assert!(case.raw_path.exists());
        assert_eq!(case.row().phase, "verified");
        fs::set_permissions(&case.raw_path, original_permissions).unwrap();
        assert_eq!(
            owner.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert_eq!(fs::read(&case.final_path).unwrap(), case.body);
    }

    #[test]
    fn sql_ack_failure_keeps_durable_removed_receipt_and_restart_replays_without_another_delete() {
        let case = Case::new();
        let db = rusqlite::Connection::open(case.coordinator.repository.path()).unwrap();
        db.execute_batch("CREATE TRIGGER fail_native_cleanup_ack BEFORE UPDATE ON download_owned_artifacts WHEN NEW.phase='removed' BEGIN SELECT RAISE(ABORT,'injected native cleanup ACK failure'); END;").unwrap();
        let mut owner = case.owner();
        assert!(owner.advance(&case.coordinator).is_err());
        assert!(owner.physical_removal_proven());
        assert_eq!(owner.phase(), Some(CleanupPhase::Removed));
        assert!(!case.raw_path.exists());
        assert_eq!(case.row().phase, "verified");
        drop(owner);
        assert_eq!(
            case.journal()
                .load(&case.intent.allocation.record.nonce)
                .unwrap()
                .unwrap()
                .phase,
            CleanupPhase::Removed
        );
        db.execute_batch("DROP TRIGGER fail_native_cleanup_ack;")
            .unwrap();
        let mut restarted = case.owner();
        assert_eq!(
            restarted.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert_eq!(case.row().phase, "removed");
        assert_eq!(case.row().reserved_bytes, 0);
        assert_eq!(fs::read(&case.final_path).unwrap(), case.body);
    }

    #[test]
    fn a_truncated_last_record_recovers_the_last_complete_prepared_phase() {
        let case = Case::new();
        case.prepared();
        let mut file = OpenOptions::new()
            .append(true)
            .open(case.journal_path())
            .unwrap();
        file.write_all(&100u32.to_le_bytes()).unwrap();
        file.write_all(&[0; 32]).unwrap();
        file.write_all(b"{}").unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert_eq!(
            case.journal()
                .load(&case.intent.allocation.record.nonce)
                .unwrap()
                .unwrap()
                .phase,
            CleanupPhase::Prepared
        );
        let mut owner = case.owner();
        assert_eq!(
            owner.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert!(!case.raw_path.exists());
        assert_eq!(case.row().phase, "removed");
    }

    #[test]
    fn foreign_copied_or_corrupt_journals_never_overwrite_or_authorize_removal() {
        let case = Case::new();
        case.prepared();
        let original = fs::read(case.journal_path()).unwrap();
        let copied_profile = case._directory.path().join("copied-profile");
        fs::create_dir(&copied_profile).unwrap();
        let copy = copied_profile.join(case.journal_path().file_name().unwrap());
        fs::write(&copy, &original).unwrap();
        let journal = CleanupJournal::open(&copied_profile).unwrap();
        assert!(journal.load(&case.intent.allocation.record.nonce).is_err());
        let mut owner = CleanupOwner::recover(journal, case.intent.clone(), case.grant.clone());
        assert!(owner.advance(&case.coordinator).is_err());
        assert_eq!(fs::read(&copy).unwrap(), original);
        assert!(case.raw_path.exists());
        drop(owner);
        let mut corrupted = original.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        fs::write(case.journal_path(), &corrupted).unwrap();
        let mut owner = case.owner();
        assert!(owner.advance(&case.coordinator).is_err());
        assert_eq!(fs::read(case.journal_path()).unwrap(), corrupted);
        assert_eq!(case.row().phase, "verified");
    }

    #[test]
    fn profile_journal_count_and_actual_bytes_are_bounded_before_any_new_prepare() {
        for count_bound in [true, false] {
            let case = Case::new();
            let count = if count_bound { 400 } else { 33 };
            let body = vec![0; if count_bound { 1 } else { 512 * 1024 }];
            for index in 0..count {
                fs::write(
                    case.profile
                        .join(format!(".lx-cleanup-{index:032x}.journal")),
                    &body,
                )
                .unwrap();
            }
            let mut owner = case.owner();
            assert!(owner.advance(&case.coordinator).is_err());
            assert!(owner.phase().is_none());
            assert!(case.raw_path.exists());
            assert_eq!(case.row().phase, "verified");
            assert!(!case.journal_path().exists());
        }
    }

    #[test]
    fn live_verified_original_survives_source_lease_revocation_until_durable_cleanup_ack() {
        use lx_gpui_player::download::{
            ApprovedDownloadUrl, DownloadExecutionLease, DownloadPool, TransferJob,
            TransferOptions, TransferOutcome,
        };
        let case = Case::new();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/original.flac", listener.local_addr().unwrap());
        let body = case.body.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut headers = Vec::new();
            let mut byte = [0];
            while !headers.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
                assert!(headers.len() <= 16_384);
            }
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
        });
        let mut task = case
            .coordinator
            .repository
            .download_task(&context().task_id)
            .unwrap()
            .unwrap();
        task.operation_epoch = 3;
        task.original_transfer = None;
        task.original_representation = None;
        task.output_audio = None;
        task.audio_available = false;
        *task.destination.relative_components.last_mut().unwrap() = "live-original.flac".into();
        let mut current = context();
        current.operation_epoch = 3;
        task.url_bookmark = Some(PrivateUrlBookmark::new(url.clone(), current.clone()).unwrap());
        save(&case.coordinator.repository, &mut task, 2);
        let journal = Arc::new(native_producer_helpers::RepositoryJournal::new(
            case.coordinator.repository.clone(),
        ));
        let part = journal
            .create(
                case.grant
                    .plan("live-original.flac", CollisionPolicy::Skip)
                    .unwrap(),
                OwnedArtifactKind::Transfer,
                current.clone(),
                65_536,
                DownloadCancel::default(),
            )
            .unwrap();
        let mut allocation = case.intent.allocation.clone();
        allocation.record = part.record().clone();
        allocation.context = current.clone();
        let approved =
            ApprovedDownloadUrl::from_exact_approval(&url, &url, &current, &current).unwrap();
        let lease = DownloadExecutionLease::from_approved_context(current).unwrap();
        let representation = DownloadRepresentationIdentity::from_approved_url(
            &approved,
            track_record_fingerprint(&task.original_record).unwrap(),
            DownloadQuality::Flac,
        )
        .unwrap();
        let pool = DownloadPool::new(1).unwrap();
        let handle = pool
            .submit(TransferJob {
                approved_url: approved,
                representation,
                execution_lease: lease.clone(),
                part,
                checkpoint: None,
                extension: "flac".into(),
                redirect_count: 0,
                options: TransferOptions {
                    automatic_retries: 0,
                    connect_timeout: Duration::from_secs(2),
                    header_timeout: Duration::from_secs(3),
                    ..Default::default()
                },
                verifier: Arc::new(SymphoniaVerifier),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let audio = loop {
            if let Some(outcome) = handle.take_outcome() {
                match outcome {
                    TransferOutcome::Verified(audio) => break audio,
                    _ => panic!("Actual live original did not verify"),
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        };
        server.join().unwrap();
        let raw_path = case.grant.path().join(&audio.owned_record().basename);
        let final_path = case.grant.path().join("live-original.flac");
        fs::write(&final_path, &case.body).unwrap();
        case.coordinator
            .repository
            .verify_download_artifact(
                &allocation.record.nonce,
                3,
                audio.checkpoint().bytes,
                &audio.checkpoint().sha256,
                &|| Ok(()),
            )
            .unwrap();
        let mut intent = case.intent.clone();
        intent.allocation = allocation;
        intent.raw_bytes = audio.checkpoint().bytes;
        intent.raw_sha256 = audio.checkpoint().sha256.clone();
        intent.final_output = DownloadArtifactProof {
            basename: "live-original.flac".into(),
            identity: identity(&final_path),
            bytes: case.body.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&case.body)),
        };
        task.original_transfer = Some(audio.checkpoint().clone());
        task.original_representation = audio.checkpoint().representation.clone();
        task.output_audio = Some(intent.final_output.clone());
        task.audio_available = true;
        save(&case.coordinator.repository, &mut task, 3);
        intent.task_revision = task.revision;
        lease.revoke();
        let mut owner = CleanupOwner::from_live(case.journal(), intent.clone(), *audio);
        assert_eq!(
            owner.advance(&case.coordinator).unwrap(),
            CleanupPhase::Acknowledged
        );
        assert_eq!(case.coordinator.ack_calls.load(Ordering::SeqCst), 1);
        assert!(!raw_path.exists());
        assert_eq!(
            case.coordinator
                .repository
                .download_owned_artifacts(Some(&context().task_id))
                .unwrap()
                .into_iter()
                .find(|artifact| artifact.allocation.record.nonce == intent.allocation.record.nonce)
                .unwrap()
                .phase,
            "removed"
        );
        assert_eq!(fs::read(final_path).unwrap(), case.body);
        pool.shutdown(Duration::from_secs(3)).unwrap();
    }
}
