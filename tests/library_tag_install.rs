#![cfg(windows)]
#[allow(
    dead_code,
    reason = "Shared fixtures also expose sibling format constructors"
)]
#[path = "support/encoded_audio_fixture.rs"]
mod encoded;
use anyhow::{Result, ensure};
use lx_gpui_player::{
    domain::library::FileIdentity,
    library::{
        authority::RootAuthority,
        scan::ScanContext,
        tag_install::*,
        tag_write::{LocalTagPatch, TagField},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
struct State {
    revision: u64,
    sha: String,
    published: bool,
}
struct Coordinator {
    directory: PathBuf,
    state_path: PathBuf,
    selection: TagWriteSelection,
    deny: bool,
    fail_commit: bool,
    commit_then_error: bool,
    fail_publish: bool,
    fail_install: bool,
    fail_restore: bool,
    panic_phase: Option<TagInstallPhase>,
    crash_phase: Option<TagInstallPhase>,
    crash_after_write: bool,
    artifact_bytes: u64,
    committed_facts: Option<TagInstalledFacts>,
    restored_facts: Option<TagInstalledFacts>,
    closed_original: Option<TagFileObservation>,
    unopened_count: usize,
    unallocated_roles: Vec<TagInstallArtifactRole>,
    retire_ack_fail_once: bool,
    retired_calls: Vec<(TagInstallArtifactRole, FileIdentity, u64, String)>,
}
impl Coordinator {
    fn state(&self) -> State {
        serde_json::from_slice(&fs::read(&self.state_path).unwrap()).unwrap()
    }
    fn put(&self, state: &State) -> Result<()> {
        let mut file = fs::File::create(&self.state_path)?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        Ok(())
    }
}
impl TagInstallCoordinator for Coordinator {
    fn original_closed(
        &mut self,
        _selection: &TagWriteSelection,
        witness: &TagOriginalCloseWitness,
    ) -> Result<()> {
        let metadata = fs::metadata(&witness.observation().path)?;
        let modified_ms = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        ensure!(
            Some(modified_ms) == witness.observation().modified_ms,
            "Actual writer close changed its captured modified time"
        );
        self.closed_original = Some(witness.observation().clone());
        Ok(())
    }
    fn installation_unopened(
        &mut self,
        _selection: &TagWriteSelection,
        _witness: &TagUnopenedInstallationWitness,
    ) -> Result<()> {
        self.unopened_count += 1;
        Ok(())
    }
    fn artifact_unallocated(
        &mut self,
        _selection: &TagWriteSelection,
        witness: &TagUnallocatedArtifactWitness,
    ) -> Result<()> {
        self.unallocated_roles.push(witness.role());
        Ok(())
    }
    fn artifact_retired(
        &mut self,
        _selection: &TagWriteSelection,
        witness: &TagArtifactRetirementWitness,
    ) -> Result<()> {
        self.retired_calls.push((
            witness.facts().role,
            witness.facts().identity.clone(),
            witness.size_bytes(),
            witness.sha256().to_owned(),
        ));
        if self.retire_ack_fail_once {
            self.retire_ack_fail_once = false;
            anyhow::bail!("Injected ACK failure after actual native retirement");
        }
        Ok(())
    }
    fn authorize_explicit_selection(&self, selection: &TagWriteSelection) -> Result<()> {
        ensure!(
            !self.deny
                && selection.operation_id == self.selection.operation_id
                && selection.asset_id == self.selection.asset_id
                && selection.path == self.selection.path
                && selection.expected_identity == self.selection.expected_identity,
            "Explicit selected writer authorization denied"
        );
        Ok(())
    }
    fn check_current_intent(&self, selection: &TagWriteSelection) -> Result<()> {
        let state = self.state();
        ensure!(
            selection.grant_epoch == self.selection.grant_epoch
                && state.revision == selection.expected_binding_revision,
            "Selected writer epoch or binding CAS changed"
        );
        Ok(())
    }
    fn journal_directory(&self) -> Result<PathBuf> {
        Ok(self.directory.clone())
    }
    fn commit_installed(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
    ) -> Result<u64> {
        let state = self.state();
        if state.revision == selection.expected_binding_revision + 1
            && state.sha == receipt.new_body_sha256
        {
            return Ok(state.revision);
        }
        ensure!(!self.fail_commit, "Injected repository CAS failure");
        self.check_current_intent(selection)?;
        let next = State {
            revision: state.revision + 1,
            sha: receipt.new_body_sha256.clone(),
            published: false,
        };
        self.put(&next)?;
        ensure!(!self.commit_then_error, "Injected commit-to-journal gap");
        Ok(next.revision)
    }
    fn publish_installed(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
    ) -> Result<()> {
        ensure!(!self.fail_publish, "Injected publication failure");
        let mut state = self.state();
        ensure!(
            state.revision == selection.expected_binding_revision + 1
                && state.sha == receipt.new_body_sha256,
            "Published receipt does not match the actual committed repository"
        );
        state.published = true;
        self.put(&state)
    }
    fn commit_prepared(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<u64> {
        prepared.check()?;
        ensure!(
            fs::File::open(&selection.path).is_err(),
            "Actual installed writer is not exclusively retained during commit"
        );
        let revision = self.commit_installed(selection, receipt)?;
        self.committed_facts = Some(prepared.facts().clone());
        Ok(revision)
    }
    fn restored_asset(
        &mut self,
        selection: &TagWriteSelection,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<()> {
        prepared.check()?;
        ensure!(
            fs::File::open(&selection.path).is_err(),
            "Actual restored writer is not exclusively retained during restoration publication"
        );
        self.restored_facts = Some(prepared.facts().clone());
        Ok(())
    }
    fn artifact_limit(&self, _role: TagInstallArtifactRole) -> u64 {
        self.artifact_bytes
    }
    fn check_recovery(
        &self,
        selection: &TagWriteSelection,
        _phase: TagInstallPhase,
        receipt: Option<&TagInstallationReceipt>,
    ) -> Result<TagRecoveryDisposition> {
        self.authorize_explicit_selection(selection)?;
        let state = self.state();
        if state.revision == selection.expected_binding_revision {
            return Ok(TagRecoveryDisposition::RestoreOriginal);
        }
        let receipt = receipt.ok_or_else(|| anyhow::anyhow!("Committed receipt is unavailable"))?;
        ensure!(
            state.revision == selection.expected_binding_revision + 1
                && state.sha == receipt.new_body_sha256,
            "Phase-specific committed binding or SHA does not match"
        );
        Ok(if state.published {
            TagRecoveryDisposition::Completed
        } else {
            TagRecoveryDisposition::ResumePublication
        })
    }
    fn checkpoint(&mut self, phase: TagInstallPhase) -> Result<()> {
        if self.crash_phase == Some(phase) {
            std::process::exit(91);
        }
        assert!(self.panic_phase != Some(phase), "Injected tag phase panic");
        Ok(())
    }
    fn check_install_chunk(&mut self, restoring: bool, written: u64) -> Result<()> {
        if !restoring && written > 0 && self.crash_after_write {
            std::process::exit(91);
        }
        ensure!(
            !(restoring && self.fail_restore),
            "Injected actual restore disk failure"
        );
        ensure!(
            !(!restoring && self.fail_install && written > 0),
            "Injected failure after an actual original write"
        );
        Ok(())
    }
}
fn context() -> ScanContext {
    ScanContext::new(Duration::from_secs(30))
}
fn sha(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}
fn identity(path: &Path) -> FileIdentity {
    let root = RootAuthority::open(path.parent().unwrap(), false, &context()).unwrap();
    let file = root.open_file(path).unwrap();
    let identity = file.identity().clone();
    let barrier = root.retirement_barrier();
    drop(file);
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    identity
}
fn fixture() -> (tempfile::TempDir, Coordinator) {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let original = temp.path().join("selected.flac");
    fs::write(&original, encoded::mono_verbatim_flac()).unwrap();
    let directory = temp.path().join("owned-profile-journal");
    fs::create_dir(&directory).unwrap();
    let state_path = temp.path().join("repository-state.json");
    let selection = TagWriteSelection {
        operation_id: "selected-tag-operation".into(),
        asset_id: "asset:selected".into(),
        path: original.clone(),
        expected_identity: identity(&original),
        root_id: Some("root:selected".into()),
        grant_epoch: 1,
        expected_binding_revision: 7,
        expected_body_sha256: Some(sha(&original)),
    };
    let coordinator = Coordinator {
        directory,
        state_path,
        selection,
        deny: false,
        fail_commit: false,
        commit_then_error: false,
        fail_publish: false,
        fail_install: false,
        fail_restore: false,
        panic_phase: None,
        crash_phase: None,
        crash_after_write: false,
        artifact_bytes: u64::MAX,
        committed_facts: None,
        restored_facts: None,
        closed_original: None,
        unopened_count: 0,
        unallocated_roles: Vec::new(),
        retire_ack_fail_once: false,
        retired_calls: Vec::new(),
    };
    coordinator
        .put(&State {
            revision: 7,
            sha: sha(&original),
            published: false,
        })
        .unwrap();
    (temp, coordinator)
}
fn patch() -> LocalTagPatch {
    LocalTagPatch {
        title: TagField::Set("同对象写入校验".into()),
        artists: TagField::Set(vec!["合成作者".into()]),
        ..Default::default()
    }
}
fn install(coordinator: &mut Coordinator) -> Result<TagInstallationReceipt> {
    let token = NativeWriterToken::from_explicit_selection(
        coordinator.selection.clone(),
        &context(),
        coordinator,
    )?;
    let directory = coordinator.directory.clone();
    install_tags(token, &directory, &patch(), &context(), coordinator)
}

#[test]
fn actual_selected_same_object_installation_retains_fid_and_durable_original_backup() {
    let (_temp, mut coordinator) = fixture();
    let old_sha = sha(&coordinator.selection.path);
    let old_fid = coordinator.selection.expected_identity.clone();
    let receipt = install(&mut coordinator).unwrap();
    assert_eq!(identity(&coordinator.selection.path), old_fid);
    assert_eq!(receipt.old_body_sha256, old_sha);
    assert_eq!(receipt.new_body_sha256, sha(&coordinator.selection.path));
    assert_ne!(receipt.new_body_sha256, old_sha);
    assert_eq!(receipt.committed_binding_revision, Some(8));
    assert!(coordinator.state().published);
    let facts = receipt.installed.as_ref().unwrap();
    assert_eq!(coordinator.committed_facts.as_ref(), Some(facts));
    assert_eq!(facts.before.identity, old_fid);
    assert_eq!(facts.after.identity, old_fid);
    assert_eq!(
        facts.after.size_bytes,
        fs::metadata(&coordinator.selection.path).unwrap().len()
    );
    assert_eq!(facts.metadata.title.as_deref(), Some("同对象写入校验"));
    assert_eq!(facts.metadata.artists, vec!["合成作者"]);
    assert_eq!(facts.old_body_sha256, old_sha);
    assert_eq!(facts.new_body_sha256, receipt.new_body_sha256);
    let backups = fs::read_dir(&coordinator.directory)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|ext| ext == "backup")).then_some(path)
        })
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert_eq!(sha(&backups[0]), old_sha);
}
#[test]
fn explicit_authorization_actual_active_reader_and_hardlink_ambiguity_reject_before_mutation() {
    let (temp, mut coordinator) = fixture();
    let old = sha(&coordinator.selection.path);
    coordinator.deny = true;
    assert!(
        NativeWriterToken::from_explicit_selection(
            coordinator.selection.clone(),
            &context(),
            &coordinator
        )
        .is_err()
    );
    coordinator.deny = false;
    let active = fs::File::open(&coordinator.selection.path).unwrap();
    assert!(
        NativeWriterToken::from_explicit_selection(
            coordinator.selection.clone(),
            &context(),
            &coordinator
        )
        .is_err()
    );
    drop(active);
    fs::hard_link(&coordinator.selection.path, temp.path().join("alias.flac")).unwrap();
    assert!(
        NativeWriterToken::from_explicit_selection(
            coordinator.selection.clone(),
            &context(),
            &coordinator
        )
        .is_err()
    );
    assert_eq!(sha(&coordinator.selection.path), old);
}
#[test]
fn actual_original_write_failure_and_repository_failure_restore_same_fid_and_sha() {
    for repository in [false, true] {
        let (_temp, mut coordinator) = fixture();
        let original = sha(&coordinator.selection.path);
        let fid = coordinator.selection.expected_identity.clone();
        coordinator.fail_install = !repository;
        coordinator.fail_commit = repository;
        assert!(install(&mut coordinator).is_err());
        assert_eq!(sha(&coordinator.selection.path), original);
        assert_eq!(identity(&coordinator.selection.path), fid);
        assert_eq!(coordinator.state().revision, 7);
        let restored = coordinator.restored_facts.as_ref().unwrap();
        assert_eq!(restored.before.identity, fid);
        assert_eq!(restored.after.identity, fid);
        assert_eq!(restored.old_body_sha256, original);
        assert_eq!(restored.new_body_sha256, original);
        assert_eq!(restored.metadata.title, None);
    }
}

#[test]
fn actual_mp3_prepared_evidence_is_read_from_exclusively_installed_original() {
    let (temp, mut coordinator) = fixture();
    let path = temp.path().join("selected.mp3");
    fs::write(&path, encoded::silent_mpeg_layer3()).unwrap();
    coordinator.selection.path = path.clone();
    coordinator.selection.expected_identity = identity(&path);
    coordinator.selection.expected_body_sha256 = Some(sha(&path));
    coordinator
        .put(&State {
            revision: 7,
            sha: sha(&path),
            published: false,
        })
        .unwrap();
    let receipt = install(&mut coordinator).unwrap();
    let facts = receipt.installed.as_ref().unwrap();
    assert_eq!(
        facts.metadata.format,
        lx_gpui_player::library::tag_write::TagWriteFormat::Mp3
    );
    assert_eq!(facts.metadata.title.as_deref(), Some("同对象写入校验"));
    assert_eq!(facts.metadata.artists, vec!["合成作者"]);
    assert_eq!(
        facts.audio_sha256,
        format!("{:x}", Sha256::digest(encoded::silent_mpeg_layer3()))
    );
    assert_eq!(facts.after.identity, identity(&path));
    assert_eq!(facts.new_body_sha256, sha(&path));
    assert_eq!(coordinator.committed_facts.as_ref(), Some(facts));
}

#[test]
fn actual_original_growth_over_admitted_budget_rejects_before_journal_and_retains_writer() {
    let (_temp, mut coordinator) = fixture();
    let original = fs::read(&coordinator.selection.path).unwrap();
    coordinator.artifact_bytes = original.len() as u64 + 8;
    let mut grown = original.clone();
    grown.extend_from_slice(&[0; 16]);
    fs::write(&coordinator.selection.path, &grown).unwrap();
    assert_eq!(
        identity(&coordinator.selection.path),
        coordinator.selection.expected_identity
    );
    let error = install(&mut coordinator)
        .unwrap_err()
        .downcast::<TagInstallError>()
        .unwrap();
    let TagInstallError::Rejected { owner, .. } = error else {
        panic!("Actual over-budget writer was not retained");
    };
    assert_eq!(fs::read_dir(&coordinator.directory).unwrap().count(), 0);
    assert!(fs::File::open(&coordinator.selection.path).is_err());
    drop(owner);
    assert_eq!(fs::read(&coordinator.selection.path).unwrap(), grown);
    assert_eq!(coordinator.state().revision, 7);
}

fn owned_install(coordinator: &mut Coordinator) -> Result<TagOwnedInstallation> {
    let selection = coordinator.selection.clone();
    let directory = coordinator.directory.clone();
    install_selected_tags_owned(selection, &directory, &patch(), &context(), coordinator)
}
fn owned_error(error: anyhow::Error) -> Box<TagOwnedInstallation> {
    let TagInstallError::Owned { owner, .. } = error.downcast::<TagInstallError>().unwrap() else {
        panic!("Actual tag owner was not retained");
    };
    owner
}
#[test]
fn actual_owned_success_closes_original_and_retires_journal_last_with_repeatable_ack() {
    let (_temp, mut coordinator) = fixture();
    let mut owner = owned_install(&mut coordinator).unwrap();
    let expected = owner.receipt().unwrap().installed.clone().unwrap();
    assert!(owner.has_live_handles());
    assert!(File::open(&coordinator.selection.path).is_err());
    coordinator.retire_ack_fail_once = true;
    assert!(owner.retire(&context(), &mut coordinator).is_err());
    assert!(!owner.is_retired());
    assert_eq!(coordinator.closed_original.as_ref(), Some(&expected.after));
    assert_eq!(sha(&coordinator.selection.path), expected.new_body_sha256);
    owner.retire(&context(), &mut coordinator).unwrap();
    assert!(owner.is_retired());
    assert!(!owner.has_live_handles());
    assert_eq!(coordinator.retired_calls.len(), 4);
    assert_eq!(coordinator.retired_calls[0], coordinator.retired_calls[1]);
    assert_eq!(
        coordinator.retired_calls.last().unwrap().0,
        TagInstallArtifactRole::Journal
    );
    assert_eq!(fs::read_dir(&coordinator.directory).unwrap().count(), 0);
    owner.retire(&context(), &mut coordinator).unwrap();
    assert_eq!(coordinator.retired_calls.len(), 4);
}
#[test]
fn actual_pending_publication_retries_with_same_exclusive_owner_without_reopen() {
    let (_temp, mut coordinator) = fixture();
    coordinator.fail_publish = true;
    let mut owner = owned_error(owned_install(&mut coordinator).unwrap_err());
    assert_eq!(owner.phase(), TagInstallPhase::RepoCommitted);
    assert!(File::open(&coordinator.selection.path).is_err());
    assert!(
        owner
            .retry_publish(&ScanContext::new(Duration::ZERO), &mut coordinator)
            .is_err()
    );
    assert!(File::open(&coordinator.selection.path).is_err());
    coordinator.fail_publish = false;
    owner.retry_publish(&context(), &mut coordinator).unwrap();
    assert_eq!(owner.phase(), TagInstallPhase::Published);
    let expected = owner.receipt().unwrap().new_body_sha256.clone();
    owner.retire(&context(), &mut coordinator).unwrap();
    assert!(owner.is_retired());
    assert_eq!(sha(&coordinator.selection.path), expected);
}
#[test]
fn actual_busy_admission_does_not_close_existing_reader_or_create_original_close_proof() {
    let (_temp, mut coordinator) = fixture();
    let reader = fs::File::open(&coordinator.selection.path).unwrap();
    let mut owner = owned_error(owned_install(&mut coordinator).unwrap_err());
    assert!(owner.admission_was_busy());
    assert_eq!(
        owner.failure_kind(),
        Some(lx_gpui_player::domain::library::LibraryErrorKind::Busy)
    );
    owner.retire(&context(), &mut coordinator).unwrap();
    assert!(owner.is_retired());
    assert!(!owner.has_live_handles());
    assert!(coordinator.closed_original.is_none());
    assert_eq!(coordinator.unopened_count, 1);
    assert_eq!(coordinator.unallocated_roles.len(), 3);
    assert_eq!(
        reader.metadata().unwrap().len(),
        fs::metadata(&coordinator.selection.path).unwrap().len()
    );
    assert_eq!(fs::read_dir(&coordinator.directory).unwrap().count(), 0);
    owner.retire(&context(), &mut coordinator).unwrap();
    assert_eq!(coordinator.unopened_count, 1);
}
#[test]
fn actual_owned_restoration_keeps_writer_until_close_and_retires_all_artifacts() {
    let (_temp, mut coordinator) = fixture();
    let original = sha(&coordinator.selection.path);
    coordinator.fail_commit = true;
    let mut owner = owned_error(owned_install(&mut coordinator).unwrap_err());
    assert_eq!(owner.phase(), TagInstallPhase::Restored);
    assert!(File::open(&coordinator.selection.path).is_err());
    owner
        .retire(&ScanContext::new(Duration::ZERO), &mut coordinator)
        .unwrap();
    assert!(owner.is_retired());
    assert!(!owner.has_live_handles());
    assert_eq!(sha(&coordinator.selection.path), original);
    assert_eq!(fs::read_dir(&coordinator.directory).unwrap().count(), 0);
    assert_eq!(
        coordinator.closed_original.as_ref(),
        Some(&coordinator.restored_facts.as_ref().unwrap().after)
    );
}
#[test]
fn unrestored_error_retains_exclusive_actual_owner_until_retry_restores_the_original() {
    let (_temp, mut coordinator) = fixture();
    let original = sha(&coordinator.selection.path);
    coordinator.fail_install = true;
    coordinator.fail_restore = true;
    let error = install(&mut coordinator)
        .unwrap_err()
        .downcast::<TagInstallError>()
        .unwrap();
    let TagInstallError::Unrestored { owner, .. } = error else {
        panic!("Unrestored physical owner was not retained");
    };
    assert!(
        fs::OpenOptions::new()
            .read(true)
            .open(&coordinator.selection.path)
            .is_err()
    );
    coordinator.fail_restore = false;
    let error = owner
        .retry_restore(&ScanContext::new(Duration::ZERO), &mut coordinator)
        .unwrap_err()
        .downcast::<TagInstallError>()
        .unwrap();
    let TagInstallError::Unrestored { owner, .. } = error else {
        panic!("Expired retry dropped the actual unrestored owner");
    };
    assert!(
        fs::OpenOptions::new()
            .read(true)
            .open(&coordinator.selection.path)
            .is_err()
    );
    owner.retry_restore(&context(), &mut coordinator).unwrap();
    assert_eq!(sha(&coordinator.selection.path), original);
}
#[test]
fn committed_publish_failure_and_database_journal_gap_recover_without_reverting_committed_bytes() {
    for gap in [false, true] {
        let (_temp, mut coordinator) = fixture();
        coordinator.fail_publish = !gap;
        coordinator.commit_then_error = gap;
        let error = install(&mut coordinator)
            .unwrap_err()
            .downcast::<TagInstallError>()
            .unwrap();
        let TagInstallError::PendingPublication { journal_path, .. } = error else {
            panic!("Committed data was incorrectly restored");
        };
        let committed = sha(&coordinator.selection.path);
        assert_eq!(coordinator.state().revision, 8);
        assert_eq!(coordinator.state().sha, committed);
        coordinator.fail_publish = false;
        coordinator.commit_then_error = false;
        let recovered = recover_tag_install(&journal_path, &context(), &mut coordinator)
            .unwrap()
            .unwrap();
        assert_eq!(sha(&coordinator.selection.path), committed);
        assert!(coordinator.state().published);
        assert_eq!(recovered.committed_binding_revision, Some(8));
    }
}
#[test]
fn panic_after_installing_and_foreign_reused_path_never_mutate_the_wrong_object() {
    let (temp, mut coordinator) = fixture();
    let original = sha(&coordinator.selection.path);
    coordinator.panic_phase = Some(TagInstallPhase::DiskVerified);
    assert!(install(&mut coordinator).is_err());
    assert_eq!(sha(&coordinator.selection.path), original);
    coordinator.panic_phase = None;
    let journal = journal_path(&coordinator.directory, &coordinator.selection.operation_id);
    fs::rename(
        &coordinator.selection.path,
        temp.path().join("original-moved.flac"),
    )
    .unwrap();
    fs::write(&coordinator.selection.path, b"foreign unchanged sentinel").unwrap();
    assert!(recover_tag_install(&journal, &context(), &mut coordinator).is_err());
    assert_eq!(
        fs::read(&coordinator.selection.path).unwrap(),
        b"foreign unchanged sentinel"
    );
}

#[test]
#[ignore]
fn tag_install_crash_worker() {
    let directory = PathBuf::from(std::env::var("LX_TAG_INSTALL_FIXTURE").unwrap());
    let selection: TagWriteSelection =
        serde_json::from_slice(&fs::read(directory.join("selection.json")).unwrap()).unwrap();
    let phase: TagInstallPhase =
        serde_json::from_str(&std::env::var("LX_TAG_INSTALL_CRASH_PHASE").unwrap()).unwrap();
    let mut coordinator = Coordinator {
        directory: directory.join("owned-profile-journal"),
        state_path: directory.join("repository-state.json"),
        selection,
        deny: false,
        fail_commit: false,
        commit_then_error: false,
        fail_publish: false,
        fail_install: false,
        fail_restore: false,
        panic_phase: None,
        crash_phase: Some(phase),
        crash_after_write: std::env::var_os("LX_TAG_INSTALL_CRASH_WRITE").is_some(),
        artifact_bytes: u64::MAX,
        committed_facts: None,
        restored_facts: None,
        closed_original: None,
        unopened_count: 0,
        unallocated_roles: Vec::new(),
        retire_ack_fail_once: false,
        retired_calls: Vec::new(),
    };
    install(&mut coordinator)
        .expect("Actual tag installation failed before its requested crash phase");
    panic!("Crash phase was not reached");
}
#[test]
fn actual_process_crash_at_each_durable_phase_recovers_same_object_or_confirmed_committed_output() {
    let phases = [
        TagInstallPhase::Prepared,
        TagInstallPhase::BackupSealed,
        TagInstallPhase::CandidateVerified,
        TagInstallPhase::Installing,
        TagInstallPhase::DiskVerified,
        TagInstallPhase::RepoCommitted,
        TagInstallPhase::Published,
    ];
    for phase in phases {
        let (temp, mut coordinator) = fixture();
        let original = sha(&coordinator.selection.path);
        fs::write(
            temp.path().join("selection.json"),
            serde_json::to_vec(&coordinator.selection).unwrap(),
        )
        .unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "tag_install_crash_worker",
                "--nocapture",
            ])
            .env("LX_TAG_INSTALL_FIXTURE", temp.path())
            .env(
                "LX_TAG_INSTALL_CRASH_PHASE",
                serde_json::to_string(&phase).unwrap(),
            )
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(91),
            "Actual crash phase {phase:?} was not reached"
        );
        let result = recover_tag_install(
            &journal_path(&coordinator.directory, &coordinator.selection.operation_id),
            &context(),
            &mut coordinator,
        )
        .unwrap();
        assert_eq!(
            identity(&coordinator.selection.path),
            coordinator.selection.expected_identity
        );
        if matches!(
            phase,
            TagInstallPhase::RepoCommitted | TagInstallPhase::Published
        ) {
            assert!(result.is_some());
            assert!(coordinator.state().published);
            assert_eq!(sha(&coordinator.selection.path), coordinator.state().sha);
        } else {
            assert!(result.is_none());
            assert_eq!(sha(&coordinator.selection.path), original);
            assert_eq!(coordinator.state().revision, 7);
        }
    }
    let (temp, mut coordinator) = fixture();
    let original = sha(&coordinator.selection.path);
    fs::write(
        temp.path().join("selection.json"),
        serde_json::to_vec(&coordinator.selection).unwrap(),
    )
    .unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "tag_install_crash_worker",
            "--nocapture",
        ])
        .env("LX_TAG_INSTALL_FIXTURE", temp.path())
        .env(
            "LX_TAG_INSTALL_CRASH_PHASE",
            serde_json::to_string(&TagInstallPhase::Restoring).unwrap(),
        )
        .env("LX_TAG_INSTALL_CRASH_WRITE", "1")
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(91),
        "Actual original-write crash boundary was not reached"
    );
    assert!(
        recover_tag_install(
            &journal_path(&coordinator.directory, &coordinator.selection.operation_id),
            &context(),
            &mut coordinator
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(sha(&coordinator.selection.path), original);
}
