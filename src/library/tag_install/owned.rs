//! Live ownership survives every file, publication and retirement boundary.
use super::*;

pub struct TagUnopenedInstallationWitness {
    selection: TagWriteSelection,
    admission_attempted: bool,
}
impl TagUnopenedInstallationWitness {
    pub fn selection(&self) -> &TagWriteSelection {
        &self.selection
    }
    pub fn admission_attempted(&self) -> bool {
        self.admission_attempted
    }
}
struct Admission {
    selection: TagWriteSelection,
    pins: Vec<Option<File>>,
    file: Option<TagNativeFile>,
    unopened: Option<TagUnopenedInstallationWitness>,
    acknowledged: bool,
    artifacts: Vec<TagUnallocatedArtifactWitness>,
    artifact_acknowledged: usize,
    recovery: bool,
    started: bool,
}

pub struct TagArtifactRetirementWitness {
    selection: TagWriteSelection,
    facts: TagInstallArtifactFacts,
    size_bytes: u64,
    sha256: String,
}
impl TagArtifactRetirementWitness {
    pub fn selection(&self) -> &TagWriteSelection {
        &self.selection
    }
    pub fn facts(&self) -> &TagInstallArtifactFacts {
        &self.facts
    }
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}
pub struct TagUnallocatedArtifactWitness {
    selection: TagWriteSelection,
    role: TagInstallArtifactRole,
    path: PathBuf,
}
impl TagUnallocatedArtifactWitness {
    pub fn selection(&self) -> &TagWriteSelection {
        &self.selection
    }
    pub fn role(&self) -> TagInstallArtifactRole {
        self.role
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
pub struct TagOriginalCloseWitness {
    selection: TagWriteSelection,
    facts: Option<TagInstalledFacts>,
    observation: TagFileObservation,
    never_modified: bool,
}
pub struct TagOriginalRetirementSeal<'a> {
    file: &'a File,
    selection: &'a TagWriteSelection,
    facts: Option<&'a TagInstalledFacts>,
    observation: &'a TagFileObservation,
    never_modified: bool,
}
impl TagOriginalRetirementSeal<'_> {
    pub fn selection(&self) -> &TagWriteSelection {
        self.selection
    }
    pub fn facts(&self) -> Option<&TagInstalledFacts> {
        self.facts
    }
    pub fn observation(&self) -> &TagFileObservation {
        self.observation
    }
    pub fn never_modified(&self) -> bool {
        self.never_modified
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            normal_file(self.file)?
                && links(self.file)? == 1
                && self.observation.identity == self.selection.expected_identity
                && normalize_local_path(&self.observation.path)
                    == normalize_local_path(&self.selection.path)
                && observe_file(self.file)? == *self.observation,
            "Actual original retirement seal changed before native close"
        );
        Ok(())
    }
}
/// Proves exclusion of the old writer through new same-object native controls;
/// it does not report that the old CloseHandle call returned successfully.
pub struct TagOriginalRetirementRecoveryWitness {
    selection: TagWriteSelection,
    intent: TagOriginalRecoveryIntent,
    body_sha256: String,
    stability: Option<File>,
}
impl TagOriginalRetirementRecoveryWitness {
    pub fn selection(&self) -> &TagWriteSelection {
        &self.selection
    }
    pub fn intent(&self) -> &TagOriginalRecoveryIntent {
        &self.intent
    }
    pub fn body_sha256(&self) -> &str {
        &self.body_sha256
    }
    pub fn check(&self) -> Result<()> {
        let file = self
            .stability
            .as_ref()
            .context("Actual original recovery stability pin is closed")?;
        ensure!(
            normal_file(file)?
                && links(file)? == 1
                && observe_file(file)? == self.intent.observation
                && self.body_sha256 == self.intent.facts.new_body_sha256,
            "Actual original recovery proof changed before acknowledgement"
        );
        Ok(())
    }
}
struct OriginalRecovery {
    intent: TagOriginalRecoveryIntent,
    calibration: retire_native::OriginalCalibration,
    witness: Option<TagOriginalRetirementRecoveryWitness>,
}
impl TagOriginalCloseWitness {
    pub fn selection(&self) -> &TagWriteSelection {
        &self.selection
    }
    pub fn facts(&self) -> Option<&TagInstalledFacts> {
        self.facts.as_ref()
    }
    pub fn observation(&self) -> &TagFileObservation {
        &self.observation
    }
    pub fn never_modified(&self) -> bool {
        self.never_modified
    }
}

pub struct TagOwnedInstallation {
    live: Option<Session>,
    opening: Option<NativeWriterToken>,
    loose: Vec<UnacknowledgedTagArtifact>,
    retirement: Option<Retirement>,
    receipt: Option<TagInstallationReceipt>,
    phase: TagInstallPhase,
    admission: Option<Admission>,
    prejournal: Option<TagNativeFile>,
    prejournal_pins: Vec<Option<File>>,
    failure_kind: Option<LibraryErrorKind>,
    cold_attempt: bool,
    attempt_retired: bool,
    cold_files: Vec<Option<TagNativeFile>>,
    already_retired_roles: Vec<TagInstallArtifactRole>,
}
impl std::fmt::Debug for TagOwnedInstallation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagOwnedInstallation")
            .field("phase", &self.phase())
            .field("retired", &self.is_retired())
            .finish()
    }
}
struct Artifact {
    role: TagInstallArtifactRole,
    path: PathBuf,
    file: Option<TagNativeFile>,
    seal: Option<(TagInstallArtifactFacts, TagFileObservation, String)>,
    begun: bool,
    pending: bool,
    zero_links: bool,
    retired: Option<TagArtifactRetirementWitness>,
    unallocated: Option<TagUnallocatedArtifactWitness>,
    acknowledged: bool,
    recovery_seal: Option<(TagInstallArtifactFacts, u64, String, bool)>,
    managed: Option<crate::download::DownloadNativeFile>,
}
struct Retirement {
    selection: TagWriteSelection,
    original: Option<TagNativeFile>,
    original_pins: Vec<Option<File>>,
    authority: Option<Box<dyn Fn() -> Result<()> + Send + Sync>>,
    facts: Option<TagInstalledFacts>,
    original_observation: TagFileObservation,
    original_witness: Option<TagOriginalCloseWitness>,
    original_acknowledged: bool,
    original_intent_recorded: bool,
    original_recovery: Option<OriginalRecovery>,
    original_prepared_hash: Option<String>,
    original_managed: Option<crate::download::DownloadNativeFile>,
    never_modified: bool,
    directory_pins: Vec<Option<File>>,
    calibration: retire_native::Calibration,
    artifacts: Vec<Artifact>,
    complete: bool,
}

fn retained(mut owner: TagOwnedInstallation, error: anyhow::Error) -> anyhow::Error {
    owner.failure_kind = Some(
        error
            .downcast_ref::<LibraryError>()
            .map_or(LibraryErrorKind::Storage, |error| error.kind),
    );
    TagInstallError::Owned {
        message: bounded(&error),
        owner: Box::new(owner),
    }
    .into()
}
pub fn install_tags_owned(
    token: NativeWriterToken,
    journal_directory: &Path,
    patch: &LocalTagPatch,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<TagOwnedInstallation> {
    let mut owner = TagOwnedInstallation {
        live: None,
        opening: Some(token),
        loose: Vec::new(),
        retirement: None,
        receipt: None,
        phase: TagInstallPhase::Prepared,
        admission: None,
        prejournal: None,
        prejournal_pins: Vec::new(),
        failure_kind: None,
        cold_attempt: false,
        attempt_retired: false,
        cold_files: Vec::new(),
        already_retired_roles: Vec::new(),
    };
    let setup = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        context.check()?;
        let token = owner.opening.as_mut().unwrap();
        token.check()?;
        let observation = observe_file(&token.file)?;
        ensure!(
            observation.size_bytes <= coordinator.artifact_limit(TagInstallArtifactRole::Backup)
                && observation.size_bytes
                    <= coordinator.artifact_limit(TagInstallArtifactRole::Candidate),
            LibraryError::budget(
                "Actual original tag body exceeds its admitted backup/candidate reservation"
            )
        );
        let metadata = read_writer_metadata(token, context)?;
        let persistence = TagPersistenceBudget::original(token, &metadata)?;
        budget::check_original(&persistence, coordinator)?;
        let mut journal = create_owned_journal(&mut owner, journal_directory, coordinator)?;
        journal.manifest.original_observation = Some(observation);
        owner.live = Some(Session {
            token: owner.opening.take().unwrap(),
            journal,
            backup: None,
            candidate: None,
            owned: true,
        });
        Ok(())
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Tag admission worker panicked")));
    if let Err(error) = setup {
        let error = capture_loose(&mut owner, error);
        return Err(retained(owner, error));
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_install(owner.live.as_mut().unwrap(), patch, context, coordinator)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Tag installation worker panicked")));
    match result {
        Ok(receipt) => {
            owner.receipt = Some(receipt);
            owner.phase = TagInstallPhase::Published;
            Ok(owner)
        }
        Err(error) => {
            let error = capture_loose(&mut owner, error);
            let phase = owner.phase();
            let recovery = catch_unwind(AssertUnwindSafe(|| {
                coordinator.check_recovery(
                    &owner.live.as_ref().unwrap().token.selection,
                    phase,
                    owner
                        .live
                        .as_ref()
                        .unwrap()
                        .journal
                        .manifest
                        .receipt
                        .as_ref(),
                )
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Tag recovery decision panicked")));
            let decision = match recovery {
                Ok(value) => value,
                Err(error) => return Err(retained(owner, error)),
            };
            let committed = matches!(
                phase,
                TagInstallPhase::RepoCommitted | TagInstallPhase::Published
            ) || matches!(
                decision,
                TagRecoveryDisposition::ResumePublication | TagRecoveryDisposition::Completed
            );
            if !committed {
                let restore_result = catch_unwind(AssertUnwindSafe(|| {
                    owner.retry_restore(&ScanContext::new(Duration::from_secs(30)), coordinator)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Tag restoration worker panicked")));
                if let Err(restore_error) = restore_result {
                    return Err(retained(
                        owner,
                        anyhow::anyhow!(
                            "{}; restore: {}",
                            bounded(&error),
                            bounded(&restore_error)
                        ),
                    ));
                }
            }
            Err(retained(owner, error))
        }
    }
}
fn capture_loose(owner: &mut TagOwnedInstallation, error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<UnacknowledgedTagArtifact>() {
        Ok(artifact) => {
            let message = artifact.message.clone();
            owner.loose.push(artifact);
            anyhow::anyhow!("{message}")
        }
        Err(error) => error,
    }
}

impl TagOwnedInstallation {
    /// Allocation retries borrow the same zero FH after its actual creator
    /// joins. Full-body preparation belongs to a later tracked worker.
    pub(crate) fn acknowledge_managed_allocations_after_join(
        &mut self,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        for producer in self.managed_producers() {
            ensure!(
                producer.producers_joined()?,
                "Managed allocation recovery retains a producer that has not actually joined"
            );
        }
        if self.retirement.is_none() {
            self.start_retirement(coordinator)?;
        }
        let state = self.retirement.as_mut().unwrap();
        coordinator.check_retirement_owner(&state.selection)?;
        let parent = state
            .directory_pins
            .last()
            .and_then(Option::as_ref)
            .context("Managed allocation retry lost its actual directory owner")?;
        for artifact in &mut state.artifacts {
            if let Some(TagNativeFile::Download(file)) = artifact.file.as_mut()
                && !file.creation_acknowledged()?
            {
                match file.status()? {
                    crate::download::DownloadNativeStatus::Reserved => continue,
                    crate::download::DownloadNativeStatus::Captured => {}
                    crate::download::DownloadNativeStatus::AttemptedNoHandle => {
                        anyhow::bail!(
                            "Managed allocation attempt retains an unresolved native return"
                        )
                    }
                    crate::download::DownloadNativeStatus::Closed => {
                        anyhow::bail!(
                            "Managed allocation closed before its actual creation acknowledgement"
                        )
                    }
                }
                let acknowledgment = ManagedAllocationAck {
                    selection: &state.selection,
                    role: artifact.role,
                    path: &artifact.path,
                    directory: parent,
                    maximum: coordinator.artifact_limit(artifact.role),
                };
                file.acknowledge_existing_with_native_ack(|native, actual_parent, allocation| {
                    acknowledgment.acknowledge(native, actual_parent, allocation, coordinator)
                })?;
                ensure!(
                    file.creation_acknowledged()?,
                    "Managed allocation retry returned without its actual creation acknowledgement"
                );
            }
        }
        Ok(())
    }
    /// Full body reads remain in an actual managed producer worker. The
    /// returned seals are rechecked against the same retained FH after join.
    pub(crate) fn prepare_managed_retirement(
        &mut self,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        if self.retirement.is_none() {
            self.start_retirement(coordinator)?;
        }
        let state = self.retirement.as_mut().unwrap();
        let context = ScanContext::new(Duration::from_secs(30));
        coordinator.check_retirement_owner(&state.selection)?;
        if let Some(file) = state.original.as_mut()
            && file.is_download()
        {
            ensure!(
                observe_file(file)? == state.original_observation,
                "Actual managed original changed before retirement preparation"
            );
            let sha = hash_body(file, &context)?;
            if let Some(facts) = &state.facts {
                ensure!(
                    sha == facts.new_body_sha256,
                    "Actual managed original body changed before retirement preparation"
                );
            }
            ensure!(
                observe_file(file)? == state.original_observation,
                "Actual managed original changed during full retirement preparation"
            );
            state.original_prepared_hash = Some(sha);
        }
        let parent = state
            .directory_pins
            .last()
            .and_then(Option::as_ref)
            .context("Managed retirement preparation lost its actual directory")?;
        for artifact in &mut state.artifacts {
            if let Some(file) = artifact.file.as_mut()
                && file.is_download()
            {
                match file.managed_status()? {
                    Some(crate::download::DownloadNativeStatus::Captured) => {}
                    Some(crate::download::DownloadNativeStatus::Reserved) => continue,
                    Some(crate::download::DownloadNativeStatus::AttemptedNoHandle) => {
                        anyhow::bail!(
                            "Managed native attempt has no captured handle; its real owner remains unresolved"
                        )
                    }
                    Some(crate::download::DownloadNativeStatus::Closed) => {
                        anyhow::bail!(
                            "Managed artifact closed before its tracked whole-body retirement preparation"
                        )
                    }
                    None => unreachable!(),
                }
                ensure!(
                    file.managed_borrow().unwrap().creation_acknowledged()?,
                    "Managed body preparation requires its joined creation acknowledgement retry"
                );
                let facts = TagInstallArtifactFacts {
                    role: artifact.role,
                    path: artifact.path.clone(),
                    identity: actual_identity(file)?,
                    directory_identity: actual_identity(parent)?,
                };
                check_artifact_owner(file, parent, &facts)?;
                let before = observe_file(file)?;
                let sha = hash_body(file, &context)?;
                ensure!(
                    observe_file(file)? == before,
                    "Actual managed artifact changed during full retirement preparation"
                );
                artifact.seal = Some((facts, before, sha));
            }
        }
        Ok(())
    }
    pub(crate) fn managed_producers(&self) -> Vec<crate::download::DownloadProducerObject> {
        let mut values = Vec::new();
        let mut add = |file: &TagNativeFile| {
            if let Some(file) = file.managed_borrow() {
                values.push(file.producer_object());
            }
        };
        if let Some(token) = &self.opening {
            add(&token.file);
        }
        if let Some(session) = &self.live {
            add(&session.token.file);
            add(&session.journal.file);
            if let Some(file) = &session.backup {
                add(file);
            }
            if let Some(file) = &session.candidate {
                add(file);
            }
        }
        if let Some(file) = &self.prejournal {
            add(file);
        }
        if let Some(state) = &self.retirement {
            if let Some(file) = &state.original {
                add(file);
            } else if let Some(file) = &state.original_managed {
                add(&TagNativeFile::Download(file.shared_borrow()));
            }
            for artifact in &state.artifacts {
                if let Some(file) = &artifact.file {
                    add(file);
                } else if let Some(file) = &artifact.managed {
                    add(&TagNativeFile::Download(file.shared_borrow()));
                }
            }
        }
        values
    }
    /// Retains the admitted obligation before coordinator setup or native IO.
    pub fn unstarted(selection: TagWriteSelection) -> Self {
        Self {
            live: None,
            opening: None,
            loose: Vec::new(),
            retirement: None,
            receipt: None,
            phase: TagInstallPhase::Prepared,
            admission: Some(Admission {
                selection,
                pins: Vec::new(),
                file: None,
                unopened: None,
                acknowledged: false,
                artifacts: Vec::new(),
                artifact_acknowledged: 0,
                recovery: false,
                started: false,
            }),
            prejournal: None,
            prejournal_pins: Vec::new(),
            failure_kind: None,
            cold_attempt: false,
            attempt_retired: false,
            cold_files: Vec::new(),
            already_retired_roles: Vec::new(),
        }
    }
    pub fn is_unstarted(&self) -> bool {
        self.admission.as_ref().is_some_and(|value| {
            !value.recovery
                && !value.started
                && value.file.is_none()
                && value.pins.is_empty()
                && value.unopened.is_none()
                && !value.acknowledged
                && value.artifacts.is_empty()
        }) && self.opening.is_none()
            && self.live.is_none()
            && self.retirement.is_none()
    }
    pub fn is_retirement_only(&self) -> bool {
        self.cold_attempt
            || self.retirement.as_ref().is_some_and(|value| {
                value.original_acknowledged || value.original_recovery.is_some()
            })
            || self.is_unstarted()
    }
    pub fn start_selected(
        mut self,
        journal_directory: &Path,
        patch: &LocalTagPatch,
        context: &ScanContext,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<Self> {
        if !self.is_unstarted() {
            return Err(retained(
                self,
                anyhow::anyhow!("Tag installation admission already started"),
            ));
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            open_owned_admission(self.admission.as_mut().unwrap(), context, coordinator)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Tag native admission worker panicked")));
        if let Err(error) = result {
            return Err(retained(self, error));
        }
        let mut admission = self.admission.take().unwrap();
        let token = NativeWriterToken {
            selection: admission.selection,
            file: admission.file.take().unwrap(),
            _parents: admission.pins.drain(..).flatten().collect(),
            authority: None,
        };
        install_tags_owned(token, journal_directory, patch, context, coordinator)
    }
    pub fn receipt(&self) -> Option<&TagInstallationReceipt> {
        self.receipt.as_ref().or_else(|| {
            self.live
                .as_ref()
                .and_then(|session| session.journal.manifest.receipt.as_ref())
        })
    }
    pub fn phase(&self) -> TagInstallPhase {
        self.live
            .as_ref()
            .map_or(self.phase, |session| session.journal.manifest.phase)
    }
    pub fn is_retired(&self) -> bool {
        self.attempt_retired
            || self.retirement.as_ref().is_some_and(|value| value.complete)
            || self
                .admission
                .as_ref()
                .is_some_and(|value| value.acknowledged && value.artifact_acknowledged == 3)
    }
    pub fn failure_kind(&self) -> Option<LibraryErrorKind> {
        self.failure_kind
    }
    pub fn admission_was_busy(&self) -> bool {
        self.failure_kind == Some(LibraryErrorKind::Busy)
    }
    pub fn has_retirement_obligation(&self) -> bool {
        !self.is_retired()
    }
    pub fn has_live_handles(&self) -> bool {
        self.live.is_some()
            || self.opening.is_some()
            || self.prejournal.is_some()
            || !self.loose.is_empty()
            || self.prejournal_pins.iter().any(Option::is_some)
            || self.cold_files.iter().any(Option::is_some)
            || self
                .admission
                .as_ref()
                .is_some_and(|value| value.file.is_some() || value.pins.iter().any(Option::is_some))
            || self.retirement.as_ref().is_some_and(|value| {
                value.original.is_some()
                    || value.original_pins.iter().any(Option::is_some)
                    || value.directory_pins.iter().any(Option::is_some)
                    || value.original_managed.is_some()
                    || value
                        .artifacts
                        .iter()
                        .any(|item| item.file.is_some() || item.managed.is_some())
                    || value.calibration.has_live_handles()
                    || value.original_recovery.as_ref().is_some_and(|value| {
                        value.calibration.has_live_handles()
                            || value
                                .witness
                                .as_ref()
                                .is_some_and(|witness| witness.stability.is_some())
                    })
            })
    }
    pub fn retry_publish(
        &mut self,
        context: &ScanContext,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        let session = self
            .live
            .as_mut()
            .context("Tag publication has no retained actual session")?;
        context.check()?;
        let disposition = coordinator.check_recovery(
            &session.token.selection,
            session.journal.manifest.phase,
            session.journal.manifest.receipt.as_ref(),
        )?;
        ensure!(
            matches!(
                disposition,
                TagRecoveryDisposition::ResumePublication | TagRecoveryDisposition::Completed
            ),
            "Uncommitted tags must restore instead of publish"
        );
        let mut receipt = session
            .journal
            .manifest
            .receipt
            .clone()
            .context("Retained tag receipt is unavailable")?;
        let facts = receipt
            .installed
            .as_ref()
            .context("Retained actual installed facts are unavailable")?;
        verify_recovered_facts(&mut session.token, facts, context)?;
        if receipt.committed_binding_revision.is_none() {
            let prepared = TagPreparedAsset {
                writer: &session.token,
                facts,
            };
            receipt.committed_binding_revision =
                Some(coordinator.commit_prepared(&session.token.selection, &receipt, &prepared)?);
            session.journal.manifest.receipt = Some(receipt.clone());
        }
        if session.journal.manifest.phase != TagInstallPhase::Published {
            phase(session, TagInstallPhase::RepoCommitted, coordinator)?;
            let prepared = TagPreparedAsset {
                writer: &session.token,
                facts: receipt.installed.as_ref().unwrap(),
            };
            coordinator.publish_prepared(&session.token.selection, &receipt, &prepared)?;
            phase(session, TagInstallPhase::Published, coordinator)?;
        }
        self.receipt = Some(receipt);
        self.phase = TagInstallPhase::Published;
        Ok(())
    }
    pub fn retry_restore(
        &mut self,
        context: &ScanContext,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        let session = self
            .live
            .as_mut()
            .context("Tag restoration has no retained actual session")?;
        ensure!(
            coordinator.check_recovery(
                &session.token.selection,
                session.journal.manifest.phase,
                session.journal.manifest.receipt.as_ref()
            )? == TagRecoveryDisposition::RestoreOriginal,
            "Committed tags must retain and publish their actual output"
        );
        if matches!(
            session.journal.manifest.phase,
            TagInstallPhase::Installing
                | TagInstallPhase::DiskVerified
                | TagInstallPhase::Restoring
        ) && session.journal.manifest.restored_facts.is_none()
        {
            restore(session, context, coordinator)?;
        } else {
            if let Some(facts) = session.journal.manifest.restored_facts.as_ref() {
                let prepared = TagPreparedAsset {
                    writer: &session.token,
                    facts,
                };
                coordinator.restored_asset(&session.token.selection, &prepared)?;
            } else {
                let before = session
                    .journal
                    .manifest
                    .original_observation
                    .clone()
                    .context("Actual original observation is unavailable")?;
                let original_sha = hash_body(&mut session.token.file, context)?;
                ensure!(
                    session
                        .token
                        .selection
                        .expected_body_sha256
                        .as_ref()
                        .is_none_or(|expected| expected.eq_ignore_ascii_case(&original_sha)),
                    "Actual untouched original no longer matches selected body"
                );
                let metadata = read_writer_metadata(&mut session.token, context)?;
                let audio_sha256 = read_writer_audio_sha256(&mut session.token, context)?;
                phase(session, TagInstallPhase::Restoring, coordinator)?;
                let facts = TagInstalledFacts {
                    before,
                    after: observe_file(&session.token.file)?,
                    format: metadata.format,
                    metadata,
                    old_body_sha256: original_sha.clone(),
                    new_body_sha256: original_sha,
                    audio_sha256,
                };
                let prepared = TagPreparedAsset {
                    writer: &session.token,
                    facts: &facts,
                };
                coordinator.restored_asset(&session.token.selection, &prepared)?;
                session.journal.manifest.restored_facts = Some(facts);
            }
            phase(session, TagInstallPhase::Restored, coordinator)?;
        }
        self.phase = TagInstallPhase::Restored;
        Ok(())
    }
    pub fn retire(
        &mut self,
        _context: &ScanContext,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        let context = ScanContext::new(Duration::from_secs(30));
        if self.cold_attempt {
            return self.close_cold_attempt(coordinator);
        }
        if let Some(admission) = self.admission.as_mut() {
            coordinator.check_retirement_owner(&admission.selection)?;
            if admission.file.is_some() {
                let token = NativeWriterToken {
                    selection: admission.selection.clone(),
                    file: admission.file.take().unwrap(),
                    _parents: admission.pins.drain(..).flatten().collect(),
                    authority: None,
                };
                self.opening = Some(token);
                self.admission = None;
            } else {
                for pin in &mut admission.pins {
                    retire_native::close(pin)?;
                }
                admission
                    .unopened
                    .get_or_insert_with(|| TagUnopenedInstallationWitness {
                        selection: admission.selection.clone(),
                        admission_attempted: admission.started,
                    });
                if !admission.acknowledged {
                    coordinator.installation_unopened(
                        &admission.selection,
                        admission.unopened.as_ref().unwrap(),
                    )?;
                    admission.acknowledged = true;
                }
                if admission.artifacts.len() < 3 {
                    let directory = coordinator.journal_directory()?;
                    for (role, extension) in [
                        (TagInstallArtifactRole::Backup, "backup"),
                        (TagInstallArtifactRole::Candidate, "edit"),
                        (TagInstallArtifactRole::Journal, "journal"),
                    ]
                    .into_iter()
                    .skip(admission.artifacts.len())
                    {
                        admission.artifacts.push(TagUnallocatedArtifactWitness {
                            selection: admission.selection.clone(),
                            role,
                            path: selected_artifact_path(
                                &directory,
                                &admission.selection.operation_id,
                                extension,
                                coordinator
                                    .artifact_basename(&admission.selection, role)?
                                    .as_deref(),
                            )?,
                        });
                    }
                }
                while admission.artifact_acknowledged < admission.artifacts.len() {
                    coordinator.artifact_unallocated(
                        &admission.selection,
                        &admission.artifacts[admission.artifact_acknowledged],
                    )?;
                    admission.artifact_acknowledged += 1;
                }
                return Ok(());
            }
        }
        if self.retirement.is_none() {
            self.start_retirement(coordinator)?;
        }
        let state = self.retirement.as_mut().unwrap();
        if state.complete {
            return Ok(());
        }
        coordinator.check_retirement_owner(&state.selection)?;
        if let Some(check) = &state.authority {
            check()?;
        }
        if let Some(recovery) = &mut state.original_recovery {
            if !state.original_acknowledged {
                coordinator.original_retirement_recovered(
                    &state.selection,
                    recovery
                        .witness
                        .as_ref()
                        .context("Actual original recovery witness is absent")?,
                )?;
                state.original_acknowledged = true;
            }
            if let Some(witness) = &mut recovery.witness {
                coordinator.check_retirement_owner(&state.selection)?;
                retire_native::close(&mut witness.stability)?;
            }
            recovery.calibration.close()?;
            for pin in &mut state.original_pins {
                retire_native::close(pin)?;
            }
        } else if !state.original_acknowledged && state.original_witness.is_none() {
            if let Some(file) = &mut state.original {
                ensure!(
                    observe_file(file)? == state.original_observation,
                    "Actual original changed before closing its writer"
                );
                if let Some(facts) = &state.facts {
                    if file.is_download() {
                        ensure!(
                            state.original_prepared_hash.as_deref()
                                == Some(facts.new_body_sha256.as_str()),
                            "Managed original lacks its actual tracked whole-body retirement preparation"
                        );
                    } else {
                        ensure!(
                            hash_body(file, &context)? == facts.new_body_sha256,
                            "Actual original body changed before native close"
                        );
                    }
                }
                if file.is_download() {
                    ensure!(
                        state.original_prepared_hash.is_some(),
                        "Managed original has not completed its tracked full-body retirement preparation"
                    );
                }
                coordinator.check_retirement_owner(&state.selection)?;
                if !state.original_intent_recorded {
                    file.inspect_native(|file| {
                        coordinator.begin_original_retirement(
                            &state.selection,
                            &TagOriginalRetirementSeal {
                                file,
                                selection: &state.selection,
                                facts: state.facts.as_ref(),
                                observation: &state.original_observation,
                                never_modified: state.never_modified,
                            },
                        )
                    })?;
                    state.original_intent_recorded = true;
                }
                TagNativeFile::close(&mut state.original)?;
            }
            for pin in &mut state.original_pins {
                retire_native::close(pin)?;
            }
            state.original_witness = Some(TagOriginalCloseWitness {
                selection: state.selection.clone(),
                facts: state.facts.clone(),
                observation: state.original_observation.clone(),
                never_modified: state.never_modified,
            });
        }
        if !state.original_acknowledged && state.original_recovery.is_none() {
            coordinator
                .original_closed(&state.selection, state.original_witness.as_ref().unwrap())?;
            state.original_acknowledged = true;
        }
        for artifact in &mut state.artifacts {
            if artifact.acknowledged {
                continue;
            }
            coordinator.check_retirement_owner(&state.selection)?;
            if let Some(file) = &artifact.file {
                match file.managed_status()? {
                    Some(crate::download::DownloadNativeStatus::Reserved) => {
                        anyhow::bail!(
                            "Managed artifact reservation requires its actual never-attempted cancellation receipt"
                        )
                    }
                    Some(crate::download::DownloadNativeStatus::AttemptedNoHandle) => {
                        anyhow::bail!(
                            "Managed native attempt remains retained without a captured native owner"
                        )
                    }
                    Some(crate::download::DownloadNativeStatus::Closed) => {
                        anyhow::bail!(
                            "Managed artifact was closed outside its retained M06 retirement owner"
                        )
                    }
                    Some(crate::download::DownloadNativeStatus::Captured) | None => {}
                }
                if let Some(managed) = file.managed_borrow() {
                    ensure!(
                        managed.creation_acknowledged()?,
                        "Managed artifact cannot retire before its actual allocation acknowledgement"
                    );
                }
            }
            if artifact.seal.is_none()
                && let Some((facts, size_bytes, sha256, retiring)) = &artifact.recovery_seal
            {
                let parent = state
                    .directory_pins
                    .last()
                    .and_then(Option::as_ref)
                    .context("Cold tag retirement actual directory pin is absent")?;
                ensure!(
                    actual_identity(parent)? == facts.directory_identity
                        && normalize_local_path(&actual_path(parent)?)
                            == facts
                                .path
                                .parent()
                                .map(normalize_local_path)
                                .context("Cold tag artifact parent is absent")?,
                    "Cold tag retirement actual directory differs from its sealed ledger"
                );
                if *retiring && state.calibration.absent(parent, &facts.identity)? {
                    coordinator.check_retirement_owner(&state.selection)?;
                    artifact.seal = Some((
                        facts.clone(),
                        TagFileObservation {
                            path: facts.path.clone(),
                            identity: facts.identity.clone(),
                            size_bytes: *size_bytes,
                            modified_ms: None,
                            modified_nanos: None,
                        },
                        sha256.clone(),
                    ));
                    artifact.pending = true;
                    artifact.zero_links = true;
                    artifact.retired = Some(TagArtifactRetirementWitness {
                        selection: state.selection.clone(),
                        facts: facts.clone(),
                        size_bytes: *size_bytes,
                        sha256: sha256.clone(),
                    });
                } else {
                    if artifact.file.is_none() {
                        artifact.file = Some(open_cold_artifact(&facts.path)?);
                    }
                    let file = artifact.file.as_mut().unwrap();
                    check_artifact_owner(file, parent, facts)?;
                    let observation = observe_file(file)?;
                    ensure!(
                        observation.size_bytes == *size_bytes
                            && hash_body(file, &context)? == *sha256,
                        "Cold tag retirement actual bytes or SHA differ from its sealed ledger"
                    );
                    artifact.seal = Some((facts.clone(), observation, sha256.clone()));
                }
                artifact.begun = *retiring;
            }
            if artifact.file.is_none() && artifact.seal.is_none() {
                artifact
                    .unallocated
                    .get_or_insert_with(|| TagUnallocatedArtifactWitness {
                        selection: state.selection.clone(),
                        role: artifact.role,
                        path: artifact.path.clone(),
                    });
                coordinator.artifact_unallocated(
                    &state.selection,
                    artifact.unallocated.as_ref().unwrap(),
                )?;
                artifact.acknowledged = true;
                continue;
            }
            let parent = state
                .directory_pins
                .last()
                .and_then(Option::as_ref)
                .context("Retained actual artifact directory pin missing")?;
            if artifact.seal.is_none() {
                let file = artifact.file.as_mut().unwrap();
                ensure!(
                    !file.is_download(),
                    "Managed artifact lacks its actual tracked full-body retirement seal"
                );
                let facts = TagInstallArtifactFacts {
                    role: artifact.role,
                    path: artifact.path.clone(),
                    identity: actual_identity(file)?,
                    directory_identity: actual_identity(parent)?,
                };
                check_artifact_owner(file, parent, &facts)?;
                let sha = hash_body(file, &context)?;
                artifact.seal = Some((facts, observe_file(file)?, sha));
            }
            let (facts, observation, sha) = artifact.seal.as_ref().unwrap();
            if !artifact.begun {
                artifact.file.as_ref().unwrap().inspect_native(|file| {
                    coordinator.begin_artifact_retirement(
                        &state.selection,
                        &TagArtifactSeal {
                            file,
                            directory: parent,
                            facts,
                            observation: observation.clone(),
                            sha256: sha.clone(),
                            phase: self.phase,
                        },
                    )
                })?;
                artifact.begun = true;
            }
            state.calibration.prepare(parent)?;
            if !artifact.pending {
                let file = artifact
                    .file
                    .as_ref()
                    .context("Actual artifact closed before disposition")?;
                check_artifact_owner(file, parent, facts)?;
                ensure!(
                    observe_file(file)? == *observation,
                    "Actual sealed artifact changed before disposition"
                );
                #[cfg(unix)]
                file.inspect_native(|native| {
                    state.calibration.capture(parent, native, &facts.identity)
                })?;
                state.calibration.present(parent, &facts.identity)?;
                coordinator.check_retirement_owner(&state.selection)?;
                file.request_removal()?;
                artifact.pending = true;
            }
            if !artifact.zero_links {
                artifact
                    .file
                    .as_ref()
                    .unwrap()
                    .inspect_native(|file| retire_native::zero_links(file, &facts.identity))?;
                state.calibration.status(parent, &facts.identity, true)?;
                artifact.zero_links = true;
            }
            if artifact.file.is_some() {
                coordinator.check_retirement_owner(&state.selection)?;
                TagNativeFile::close(&mut artifact.file)?;
            }
            if artifact.retired.is_none() {
                state
                    .calibration
                    .present(parent, &facts.directory_identity)?;
                state.calibration.status(parent, &facts.identity, false)?;
                artifact.retired = Some(TagArtifactRetirementWitness {
                    selection: state.selection.clone(),
                    facts: facts.clone(),
                    size_bytes: observation.size_bytes,
                    sha256: sha.clone(),
                });
            }
            coordinator.artifact_retired(&state.selection, artifact.retired.as_ref().unwrap())?;
            if let Some(managed) = &artifact.managed {
                let released = managed.release_retired()?;
                ensure!(
                    FileIdentity::from_native_parts(
                        released.identity.device,
                        &released.identity.object
                    )? == facts.identity,
                    "Managed retirement released another actual original descriptor"
                );
            }
            artifact.managed.take();
            artifact.acknowledged = true;
        }
        for pin in &mut state.directory_pins {
            retire_native::close(pin)?;
        }
        ensure!(
            state.original_managed.is_none(),
            "Managed original retains its actual producer reservation until close-witness release"
        );
        state.complete = true;
        Ok(())
    }
    fn close_cold_attempt(&mut self, coordinator: &dyn TagInstallCoordinator) -> Result<()> {
        if self.attempt_retired {
            return Ok(());
        }
        let selection = self
            .admission
            .as_ref()
            .map(|value| &value.selection)
            .or_else(|| self.opening.as_ref().map(|value| &value.selection))
            .or_else(|| self.live.as_ref().map(|value| &value.token.selection))
            .or_else(|| self.retirement.as_ref().map(|value| &value.selection))
            .context("Cold attempt selection missing")?;
        coordinator.check_retirement_owner(selection)?;
        if let Some(admission) = &mut self.admission {
            TagNativeFile::close(&mut admission.file)?;
            for pin in &mut admission.pins {
                retire_native::close(pin)?;
            }
        }
        if let Some(token) = self.opening.take() {
            self.admission = Some(Admission {
                selection: token.selection,
                pins: token._parents.into_iter().map(Some).collect(),
                file: Some(token.file),
                unopened: None,
                acknowledged: false,
                artifacts: Vec::new(),
                artifact_acknowledged: 0,
                recovery: true,
                started: true,
            });
            return self.close_cold_attempt(coordinator);
        }
        if let Some(session) = self.live.take() {
            self.opening = Some(session.token);
            self.prejournal = Some(session.journal.file);
            self.prejournal_pins = session.journal._pins.into_iter().map(Some).collect();
            self.cold_files.extend([session.backup, session.candidate]);
            return self.close_cold_attempt(coordinator);
        }
        TagNativeFile::close(&mut self.prejournal)?;
        for pin in &mut self.prejournal_pins {
            retire_native::close(pin)?;
        }
        for file in &mut self.cold_files {
            TagNativeFile::close(file)?;
        }
        if let Some(state) = &mut self.retirement {
            TagNativeFile::close(&mut state.original)?;
            if let Some(recovery) = &mut state.original_recovery {
                recovery.calibration.close()?;
                if let Some(witness) = &mut recovery.witness {
                    retire_native::close(&mut witness.stability)?;
                }
            }
            for pin in &mut state.original_pins {
                retire_native::close(pin)?;
            }
            for pin in &mut state.directory_pins {
                retire_native::close(pin)?;
            }
            for artifact in &mut state.artifacts {
                TagNativeFile::close(&mut artifact.file)?;
            }
        }
        while let Some(artifact) = self.loose.pop() {
            let mut file = Some(artifact.file);
            if let Err(error) = TagNativeFile::close(&mut file) {
                self.loose.push(UnacknowledgedTagArtifact {
                    file: file.unwrap(),
                    facts: artifact.facts,
                    message: artifact.message,
                });
                return Err(error);
            }
        }
        self.attempt_retired = true;
        Ok(())
    }
    fn start_retirement(&mut self, coordinator: &mut dyn TagInstallCoordinator) -> Result<()> {
        ensure!(
            self.opening.is_some()
                || matches!(
                    self.phase(),
                    TagInstallPhase::Published | TagInstallPhase::Restored
                ),
            "Live uncommitted/unrestored tags cannot retire their recovery owners"
        );
        let selection = self
            .live
            .as_ref()
            .map(|session| &session.token.selection)
            .or_else(|| self.opening.as_ref().map(|token| &token.selection))
            .context("Actual tag owner is missing")?
            .clone();
        coordinator.check_retirement_owner(&selection)?;
        let directory = normalize_local_path(&coordinator.journal_directory()?);
        let mut paths = Vec::new();
        for (role, extension) in [
            (TagInstallArtifactRole::Backup, "backup"),
            (TagInstallArtifactRole::Candidate, "edit"),
            (TagInstallArtifactRole::Journal, "journal"),
        ] {
            paths.push((
                role,
                selected_artifact_path(
                    &directory,
                    &selection.operation_id,
                    extension,
                    coordinator.artifact_basename(&selection, role)?.as_deref(),
                )?,
            ));
        }
        let token = self
            .live
            .as_ref()
            .map(|session| &session.token)
            .or(self.opening.as_ref())
            .unwrap();
        let observation = observe_file(&token.file)?;
        let expected_facts = self.live.as_ref().and_then(|session| {
            if self.phase() == TagInstallPhase::Restored {
                session.journal.manifest.restored_facts.as_ref()
            } else {
                session
                    .journal
                    .manifest
                    .receipt
                    .as_ref()
                    .and_then(|receipt| receipt.installed.as_ref())
            }
        });
        if let Some(facts) = expected_facts {
            ensure!(
                observation == facts.after,
                "Actual original stamp differs from its committed/restored facts"
            );
        }
        let (token, mut pins, mut files, facts, never_modified) =
            if let Some(session) = self.live.take() {
                self.phase = session.journal.manifest.phase;
                self.receipt = session.journal.manifest.receipt.clone();
                let facts = if self.phase == TagInstallPhase::Restored {
                    session.journal.manifest.restored_facts.clone()
                } else {
                    session
                        .journal
                        .manifest
                        .receipt
                        .as_ref()
                        .and_then(|receipt| receipt.installed.clone())
                };
                (
                    session.token,
                    session
                        .journal
                        ._pins
                        .into_iter()
                        .map(Some)
                        .collect::<Vec<_>>(),
                    vec![
                        (TagInstallArtifactRole::Backup, session.backup),
                        (TagInstallArtifactRole::Candidate, session.candidate),
                        (TagInstallArtifactRole::Journal, Some(session.journal.file)),
                    ],
                    facts,
                    false,
                )
            } else {
                (
                    self.opening.take().unwrap(),
                    std::mem::take(&mut self.prejournal_pins),
                    vec![(TagInstallArtifactRole::Journal, self.prejournal.take())],
                    None,
                    true,
                )
            };
        let NativeWriterToken {
            selection,
            file,
            _parents,
            authority,
        } = token;
        let mut artifacts = Vec::new();
        for (role, path) in paths {
            let file = files
                .iter_mut()
                .find(|(value, _)| *value == role)
                .and_then(|(_, file)| file.take());
            let managed = file.as_ref().and_then(TagNativeFile::managed_borrow);
            artifacts.push(Artifact {
                role,
                path,
                file,
                seal: None,
                begun: false,
                pending: false,
                zero_links: false,
                retired: None,
                unallocated: None,
                acknowledged: self.already_retired_roles.contains(&role),
                recovery_seal: None,
                managed,
            });
        }
        for loose in self.loose.drain(..) {
            let artifact = artifacts
                .iter_mut()
                .find(|value| value.role == loose.facts.role)
                .unwrap();
            artifact.managed = loose.file.managed_borrow();
            artifact.file = Some(loose.file);
        }
        let original_managed = file.managed_borrow();
        self.retirement = Some(Retirement {
            selection,
            original: Some(file),
            original_pins: _parents.into_iter().map(Some).collect(),
            authority,
            facts,
            original_observation: observation,
            original_witness: None,
            original_acknowledged: false,
            original_intent_recorded: false,
            original_recovery: None,
            original_prepared_hash: None,
            original_managed,
            never_modified,
            directory_pins: std::mem::take(&mut pins),
            calibration: Default::default(),
            artifacts,
            complete: false,
        });
        Ok(())
    }
}

fn create_owned_journal(
    owner: &mut TagOwnedInstallation,
    directory: &Path,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<Journal> {
    let directory = normalize_local_path(directory);
    ensure!(
        directory == normalize_local_path(&coordinator.journal_directory()?),
        "Tag journal is outside its actual profile"
    );
    retained_pins(&directory, &mut owner.prejournal_pins)?;
    let parent = owner
        .prejournal_pins
        .last()
        .and_then(Option::as_ref)
        .context("Actual owned journal directory pin missing")?;
    let identity = actual_identity(parent)?;
    let selection = &owner.opening.as_ref().unwrap().selection;
    let path = selected_artifact_path(
        &directory,
        &selection.operation_id,
        "journal",
        coordinator
            .artifact_basename(selection, TagInstallArtifactRole::Journal)?
            .as_deref(),
    )?;
    let backup_basename =
        coordinator.artifact_basename(selection, TagInstallArtifactRole::Backup)?;
    let candidate_basename =
        coordinator.artifact_basename(selection, TagInstallArtifactRole::Candidate)?;
    allocate_artifact(
        &mut owner.prejournal,
        selection,
        TagInstallArtifactRole::Journal,
        &path,
        parent,
        if owner.opening.as_ref().unwrap().file.is_download() {
            ArtifactAllocator::Managed
        } else {
            ArtifactAllocator::OwnedNative
        },
        coordinator,
    )?;
    let file = owner.prejournal.as_ref().unwrap();
    let facts = TagInstallArtifactFacts {
        role: TagInstallArtifactRole::Journal,
        path: path.clone(),
        identity: actual_identity(file)?,
        directory_identity: identity.clone(),
    };
    if !file.is_download() {
        file.inspect_native(|file| {
            coordinator.artifact_allocated(
                selection,
                &TagArtifactAllocation {
                    file,
                    directory: parent,
                    facts: &facts,
                },
            )
        })?;
    }
    Ok(Journal {
        directory,
        path,
        _pins: owner.prejournal_pins.drain(..).flatten().collect(),
        file: owner.prejournal.take().unwrap(),
        records: 0,
        maximum_bytes: coordinator.artifact_limit(TagInstallArtifactRole::Journal),
        manifest: Manifest {
            selection: selection.clone(),
            phase: TagInstallPhase::Prepared,
            journal_directory_identity: identity,
            backup_identity: None,
            candidate_identity: None,
            old_sha256: None,
            old_size: None,
            receipt: None,
            backup_basename,
            candidate_basename,
            original_observation: None,
            restored_facts: None,
        },
    })
}

#[cfg(windows)]
fn retained_pins(path: &Path, pins: &mut Vec<Option<File>>) -> Result<()> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::*;
    validate_library_path(path)?;
    let normalized = normalize_local_path(path);
    ensure!(
        normalized.components().count() <= 256,
        "Tag namespace exceeds its component bound"
    );
    let mut chain = normalized
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    chain.reverse();
    for directory in chain {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&directory)?;
        pins.push(Some(file));
        let file = pins.last().unwrap().as_ref().unwrap();
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_dir()
                && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && normalize_local_path(&actual_path(file)?) == directory,
            "Actual tag namespace changed or contains a reparse point"
        );
    }
    Ok(())
}
#[cfg(not(windows))]
fn retained_pins(_path: &Path, _pins: &mut Vec<Option<File>>) -> Result<()> {
    anyhow::bail!("UnsupportedCapability: owned tag namespace requires Windows")
}

pub fn install_selected_tags_owned(
    selection: TagWriteSelection,
    journal_directory: &Path,
    patch: &LocalTagPatch,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<TagOwnedInstallation> {
    TagOwnedInstallation::unstarted(selection).start_selected(
        journal_directory,
        patch,
        context,
        coordinator,
    )
}
#[cfg(windows)]
fn open_owned_admission(
    admission: &mut Admission,
    context: &ScanContext,
    coordinator: &dyn TagInstallCoordinator,
) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::*;
    let selection = &admission.selection;
    validate_selection(selection)?;
    context.check()?;
    if admission.recovery {
        coordinator.authorize_recovery_selection(selection)?;
    } else {
        coordinator.authorize_explicit_selection(selection)?;
        coordinator.check_current_intent(selection)?;
        coordinator.before_installation_open(selection)?;
        admission.started = true;
    }
    retained_pins(
        selection
            .path
            .parent()
            .context("Selected tag original has no parent")?,
        &mut admission.pins,
    )?;
    let name_pin = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&selection.path)?;
    admission.pins.push(Some(name_pin));
    let pin = admission.pins.last().unwrap().as_ref().unwrap();
    ensure!(
        actual_identity(pin)? == selection.expected_identity
            && normal_file(pin)?
            && links(pin)? == 1
            && normalize_local_path(&actual_path(pin)?) == normalize_local_path(&selection.path),
        "Selected tag original changed before actual writer admission"
    );
    admission.file = Some(TagNativeFile::Native(
        OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&selection.path)
            .map_err(|error| {
                if matches!(error.raw_os_error(), Some(32 | 33)) {
                    anyhow::Error::from(LibraryError::new(
                        LibraryErrorKind::Busy,
                        "Selected tag original has an active reader or competing writer",
                    ))
                } else {
                    error.into()
                }
            })?,
    ));
    let file = admission.file.as_ref().unwrap();
    file.native_metadata_io(retire_native::freeze_write_time)?;
    ensure!(
        actual_identity(file)? == selection.expected_identity
            && normal_file(file)?
            && links(file)? == 1
            && normalize_local_path(&actual_path(file)?) == normalize_local_path(&selection.path),
        "Actual original writer differs from explicit selection"
    );
    context.check()
}
#[cfg(not(windows))]
fn open_owned_admission(
    _admission: &mut Admission,
    _context: &ScanContext,
    _coordinator: &dyn TagInstallCoordinator,
) -> Result<()> {
    anyhow::bail!("UnsupportedCapability: owned tag installation requires Windows")
}

/// A cold attempt has independent current recovery authorization. Failed
/// attempts close only their own new descriptors and cannot release old quotas.
pub fn recover_selected_tags_owned(
    selection: &TagWriteSelection,
    journal_file: &Path,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<TagOwnedInstallation> {
    let mut owner = TagOwnedInstallation {
        live: None,
        opening: None,
        loose: Vec::new(),
        retirement: None,
        receipt: None,
        phase: TagInstallPhase::Prepared,
        admission: Some(Admission {
            selection: selection.clone(),
            pins: Vec::new(),
            file: None,
            unopened: None,
            acknowledged: false,
            artifacts: Vec::new(),
            artifact_acknowledged: 0,
            recovery: true,
            started: false,
        }),
        prejournal: None,
        prejournal_pins: Vec::new(),
        failure_kind: None,
        cold_attempt: true,
        attempt_retired: false,
        cold_files: Vec::new(),
        already_retired_roles: Vec::new(),
    };
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        coordinator.authorize_recovery_selection(selection)?;
        if coordinator.recovery_unopened(selection)? {
            owner = TagOwnedInstallation::unstarted(selection.clone());
            return Ok(());
        }
        if let Some(recovery) = coordinator.recovery_retirement(selection)? {
            ensure!(
                matches!(
                    recovery.phase,
                    TagInstallPhase::Published | TagInstallPhase::Restored
                ) && recovery.artifacts.len() == 3,
                "Cold tag retirement has no terminal actual original receipt"
            );
            let directory = normalize_local_path(&coordinator.journal_directory()?);
            let mut artifacts = Vec::with_capacity(3);
            for (role, extension) in [
                (TagInstallArtifactRole::Backup, "backup"),
                (TagInstallArtifactRole::Candidate, "edit"),
                (TagInstallArtifactRole::Journal, "journal"),
            ] {
                let path = selected_artifact_path(
                    &directory,
                    &selection.operation_id,
                    extension,
                    coordinator.artifact_basename(selection, role)?.as_deref(),
                )?;
                let slot = recovery
                    .artifacts
                    .iter()
                    .find(|slot| match slot {
                        TagRetirementRecoveryArtifact::Retired { role: value, .. } => {
                            *value == role
                        }
                        TagRetirementRecoveryArtifact::Owned { facts, .. } => facts.role == role,
                    })
                    .context("Cold tag retirement role is absent")?;
                let (acknowledged, recovery_seal) = match slot {
                    TagRetirementRecoveryArtifact::Retired { path: saved, .. } => {
                        ensure!(
                            normalize_local_path(saved) == path,
                            "Cold retired tag namespace differs"
                        );
                        (true, None)
                    }
                    TagRetirementRecoveryArtifact::Owned {
                        facts,
                        size_bytes,
                        sha256,
                        retiring,
                    } => {
                        ensure!(
                            normalize_local_path(&facts.path) == path
                                && *size_bytes <= coordinator.artifact_limit(role)
                                && sha256.len() == 64
                                && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
                            "Cold tag retirement seal differs from admitted namespace or budget"
                        );
                        (
                            false,
                            Some((facts.clone(), *size_bytes, sha256.clone(), *retiring)),
                        )
                    }
                };
                artifacts.push(Artifact {
                    role,
                    path,
                    file: None,
                    seal: None,
                    begun: false,
                    pending: false,
                    zero_links: false,
                    retired: None,
                    unallocated: None,
                    acknowledged,
                    recovery_seal,
                    managed: None,
                });
            }
            retained_pins(&directory, &mut owner.prejournal_pins)?;
            let parent = owner
                .prejournal_pins
                .last()
                .and_then(Option::as_ref)
                .context("Cold tag retirement actual directory pin is absent")?;
            let directory_identity = actual_identity(parent)?;
            ensure!(
                artifacts
                    .iter()
                    .filter_map(|item| item.recovery_seal.as_ref())
                    .all(|(facts, _, _, _)| facts.directory_identity == directory_identity),
                "Cold tag retirement directory differs from authoritative actual seals"
            );
            owner.admission = None;
            owner.phase = recovery.phase;
            owner.receipt = recovery.receipt;
            owner.retirement = Some(Retirement {
                selection: selection.clone(),
                original: None,
                original_pins: Vec::new(),
                authority: None,
                facts: None,
                original_observation: recovery.observation,
                original_witness: None,
                original_acknowledged: recovery.original.is_none(),
                original_intent_recorded: true,
                original_recovery: recovery.original.map(|intent| OriginalRecovery {
                    intent,
                    calibration: retire_native::OriginalCalibration::default(),
                    witness: None,
                }),
                original_prepared_hash: None,
                original_managed: None,
                never_modified: false,
                directory_pins: std::mem::take(&mut owner.prejournal_pins),
                calibration: retire_native::Calibration::default(),
                artifacts,
                complete: false,
            });
            let state = owner.retirement.as_mut().unwrap();
            if let Some(original) = &mut state.original_recovery {
                retained_pins(
                    selection
                        .path
                        .parent()
                        .context("Original recovery parent is absent")?,
                    &mut state.original_pins,
                )?;
                let (stability, body_sha256) =
                    original
                        .calibration
                        .prove(selection, &original.intent, context, &|| {
                            coordinator.check_retirement_owner(selection)
                        })?;
                original.witness = Some(TagOriginalRetirementRecoveryWitness {
                    selection: selection.clone(),
                    intent: original.intent.clone(),
                    body_sha256,
                    stability: Some(stability),
                });
            }
            owner.cold_attempt = false;
            return Ok(());
        }
        open_owned_admission(owner.admission.as_mut().unwrap(), context, coordinator)?;
        let mut admission = owner.admission.take().unwrap();
        owner.opening = Some(NativeWriterToken {
            selection: selection.clone(),
            file: admission.file.take().unwrap(),
            _parents: admission.pins.drain(..).flatten().collect(),
            authority: None,
        });
        let token = owner.opening.as_mut().unwrap();
        if let Some(expected) = coordinator.recovery_expected_body_sha256(selection)? {
            ensure!(
                hash_body(&mut token.file, context)?.eq_ignore_ascii_case(&expected),
                LibraryError::conflict(
                    "Actual recovery writer body differs from current explicit recovery approval"
                )
            );
        }
        let directory = normalize_local_path(&coordinator.journal_directory()?);
        ensure!(
            journal_file.parent().map(normalize_local_path).as_ref() == Some(&directory),
            "Cold journal is outside its owned profile directory"
        );
        retained_pins(&directory, &mut owner.prejournal_pins)?;
        let expected_journal =
            match coordinator.recovery_artifact(selection, TagInstallArtifactRole::Journal)? {
                TagRecoveryArtifactState::Allocated(identity) => identity,
                _ => anyhow::bail!("Cold journal has no authoritative actual allocation identity"),
            };
        owner.prejournal = Some(open_cold_artifact(journal_file)?);
        let file = owner.prejournal.as_mut().unwrap();
        ensure!(
            actual_identity(file)? == expected_journal
                && normal_file(file)?
                && links(file)? == 1
                && normalize_local_path(&actual_path(file)?) == normalize_local_path(journal_file),
            "Cold journal actual selected FileID differs from its ledger"
        );
        let (manifest, records, valid_length) = read_records(file)?;
        ensure!(
            serde_json::to_vec(&manifest.selection)? == serde_json::to_vec(selection)?,
            "Cold journal selection differs from its independently authorized operation"
        );
        let parent = owner
            .prejournal_pins
            .last()
            .and_then(Option::as_ref)
            .unwrap();
        ensure!(
            actual_identity(parent)? == manifest.journal_directory_identity,
            "Cold journal actual directory changed"
        );
        ensure!(
            normalize_local_path(journal_file)
                == selected_artifact_path(
                    &directory,
                    &selection.operation_id,
                    "journal",
                    coordinator
                        .artifact_basename(selection, TagInstallArtifactRole::Journal)?
                        .as_deref()
                )?,
            "Cold journal actual namespace differs from admitted role"
        );
        let disposition =
            coordinator.check_recovery(selection, manifest.phase, manifest.receipt.as_ref())?;
        file.set_len(valid_length)?;
        file.sync_all()?;
        owner.live = Some(Session {
            token: owner.opening.take().unwrap(),
            journal: Journal {
                directory: directory.clone(),
                path: journal_file.to_path_buf(),
                _pins: owner.prejournal_pins.drain(..).flatten().collect(),
                file: owner.prejournal.take().unwrap(),
                records,
                maximum_bytes: coordinator.artifact_limit(TagInstallArtifactRole::Journal),
                manifest,
            },
            backup: None,
            candidate: None,
            owned: true,
        });
        for (role, extension) in [
            (TagInstallArtifactRole::Backup, "backup"),
            (TagInstallArtifactRole::Candidate, "edit"),
        ] {
            match coordinator.recovery_artifact(selection, role)? {
                TagRecoveryArtifactState::NeverAllocated => {}
                TagRecoveryArtifactState::Retired => {
                    owner.already_retired_roles.push(role);
                }
                TagRecoveryArtifactState::Uncertain => anyhow::bail!(
                    "Cold tag artifact has an uncertain allocation and must retain its quota"
                ),
                TagRecoveryArtifactState::Allocated(expected) => {
                    let path = selected_artifact_path(
                        &directory,
                        &selection.operation_id,
                        extension,
                        coordinator.artifact_basename(selection, role)?.as_deref(),
                    )?;
                    let session = owner.live.as_mut().unwrap();
                    let slot = if role == TagInstallArtifactRole::Backup {
                        &mut session.backup
                    } else {
                        &mut session.candidate
                    };
                    *slot = Some(open_cold_artifact(&path)?);
                    let file = slot.as_ref().unwrap();
                    ensure!(
                        actual_identity(file)? == expected
                            && normal_file(file)?
                            && links(file)? == 1
                            && normalize_local_path(&actual_path(file)?)
                                == normalize_local_path(&path),
                        "Cold artifact actual selected FileID differs from its ledger"
                    );
                }
            }
        }
        match disposition {
            TagRecoveryDisposition::ResumePublication | TagRecoveryDisposition::Completed => {
                owner.retry_publish(context, coordinator)?
            }
            TagRecoveryDisposition::RestoreOriginal => owner.retry_restore(context, coordinator)?,
        }
        owner.cold_attempt = false;
        Ok(())
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Cold tag recovery worker panicked")));
    match result {
        Ok(()) => Ok(owner),
        Err(error) => Err(retained(owner, error)),
    }
}
#[cfg(windows)]
fn open_cold_artifact(path: &Path) -> Result<TagNativeFile> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{DELETE, FILE_FLAG_OPEN_REPARSE_POINT},
    };
    Ok(TagNativeFile::Native(
        OpenOptions::new()
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?,
    ))
}
#[cfg(not(windows))]
fn open_cold_artifact(_path: &Path) -> Result<TagNativeFile> {
    anyhow::bail!("UnsupportedCapability: cold tag ownership requires Windows")
}
