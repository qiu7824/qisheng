//! Recovery of the surviving prepare manifest after group-journal retirement.
use super::*;
use crate::repository::snapshot_native::{
    ColdCleanup, MAX_MANIFEST_BYTES, PhaseFact, parse_phase_facts,
};

struct PreparationFile {
    metadata: ActualFileMetadata,
    inspection: Option<File>,
    sha256: String,
    cleanup: Option<DurableCleanupFact>,
    confirmation: Option<ColdCleanup>,
    confirmation_acknowledged: bool,
    owned_index: Option<usize>,
    transferred: bool,
}

/// Fresh selection retains the actual manifest and every inspected preparation
/// file. Its retained charge is reconstructed before approval or cleanup.
pub struct PhysicalProfileTerminalSelection {
    continuity: ProfileContinuityToken,
    group: PhysicalProfileRecoverySelection,
    manifest: Option<File>,
    manifest_metadata: Option<ActualFileMetadata>,
    preparations: Vec<PreparationFile>,
    journal_cleanup: Option<DurableCleanupFact>,
    permit: Option<SnapshotPermit>,
    limits: PhysicalRestoreLimits,
}
pub struct PhysicalProfileTerminalSelectionFailure {
    pub error: anyhow::Error,
    pub owner: Option<Box<PhysicalProfileTerminalSelection>>,
}
impl std::fmt::Debug for PhysicalProfileTerminalSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileTerminalSelection")
            .field("preview", &self.group.preview)
            .field("retained_preparation_files", &self.preparations.len())
            .field("charged", &self.permit.is_some())
            .finish()
    }
}
impl std::fmt::Debug for PhysicalProfileTerminalSelectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileTerminalSelectionFailure")
            .field("error", &self.error)
            .field("owner", &self.owner)
            .finish()
    }
}
impl std::fmt::Display for PhysicalProfileTerminalSelectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for PhysicalProfileTerminalSelectionFailure {}

fn manifest_nonce(name: &str) -> Result<&str> {
    let nonce = name
        .strip_prefix(".lx-snapshot-")
        .and_then(|name| name.strip_suffix(".manifest"))
        .context("Terminal recovery requires an explicitly selected preparation manifest")?;
    ensure!(
        nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Selected preparation manifest nonce is invalid"
    );
    Ok(nonce)
}
fn validate_preparation(
    metadata: &ActualFileMetadata,
    request: &PhysicalRestoreRequest,
    private: &SnapshotDirectory,
) -> Result<()> {
    ensure!(
        metadata.operation_id == request.operation_id
            && !metadata.published
            && metadata.final_basename.is_none()
            && metadata.directory == private.path()
            && metadata.directory_identity == *private.identity()
            && metadata.selection_epoch > 0
            && metadata.nonce.len() == 32
            && metadata.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
            && metadata.temporary_name.as_deref()
                == Some(format!(".lx-snapshot-{}.part", metadata.nonce).as_str())
            && metadata.bytes <= request.maximum_image_bytes,
        "Preparation allocation leaves its admitted operation or actual namespace"
    );
    Ok(())
}
fn final_record(facts: &[PhaseFact], group_name: &str) -> Result<GroupRecord> {
    let mut latest: Option<GroupRecord> = None;
    for fact in facts.iter().filter(|fact| fact.checkpoint) {
        let body: serde_json::Value = serde_json::from_slice(&fact.body)?;
        if body.get("kind").and_then(|kind| kind.as_str()) == Some("profile_group_final") {
            let record: GroupRecord = serde_json::from_value(
                body.get("record")
                    .cloned()
                    .context("Parent manifest final group record is absent")?,
            )?;
            ensure!(
                record.phase == GroupPhase::Finalizing && record.terminal_installed.is_some(),
                "Parent manifest does not record a fixed terminal direction"
            );
            if let Some(previous) = &latest {
                ensure!(
                    record.operation_id == previous.operation_id
                        && record.target == previous.target
                        && record.journal == previous.journal
                        && record.terminal_installed == previous.terminal_installed
                        && serde_json::to_vec(&record.request)?
                            == serde_json::to_vec(&previous.request)?,
                    "Repeated parent final record changed actual ownership or terminal direction"
                );
            }
            // Reuse the bounded role, namespace and preparation-binding validator.
            // The durable parent record is a terminal snapshot, not a new chain.
            let mut validated = record.clone();
            validated.sequence = 1;
            let bytes = serde_json::to_vec(&validated)?;
            let mut envelope = Vec::with_capacity(36 + bytes.len());
            envelope.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            envelope.extend_from_slice(&Sha256::digest(&bytes));
            envelope.extend_from_slice(&bytes);
            parse_records(&envelope, group_name)?;
            ensure!(
                record.sequence > 0 && record.sequence <= 64,
                "Parent final group sequence is invalid"
            );
            latest = Some(record);
        }
    }
    latest.context("Selected prepare manifest has no durable final group record")
}
fn latest_cleanups(facts: &[PhaseFact]) -> Result<BTreeMap<String, DurableCleanupFact>> {
    let mut cleanups: BTreeMap<String, DurableCleanupFact> = BTreeMap::new();
    for fact in facts.iter().filter(|fact| fact.checkpoint) {
        let body: serde_json::Value = serde_json::from_slice(&fact.body)?;
        if matches!(
            body.get("kind").and_then(|kind| kind.as_str()),
            Some("cleanup" | "profile_group_journal_cleanup" | "profile_group_terminal_cleanup")
        ) {
            let cleanup: DurableCleanupFact = serde_json::from_value(
                body.get("fact")
                    .cloned()
                    .context("Parent cleanup checkpoint has no fact")?,
            )?;
            ensure!(
                !cleanup.removed || cleanup.pending_zero_links(),
                "A removed parent cleanup lacks its physical zero-link disposition witness"
            );
            let name = cleanup
                .file
                .temporary_name
                .clone()
                .context("Parent cleanup fact has no actual leaf")?;
            if let Some(previous) = cleanups.get(&name) {
                ensure!(
                    cleanup.file.identity == previous.file.identity
                        && cleanup.file.operation_id == previous.file.operation_id
                        && cleanup.file.directory_identity == previous.file.directory_identity
                        && (!previous.pending_zero_links() || cleanup.pending_zero_links())
                        && (!previous.removed || cleanup.removed),
                    "Parent cleanup changed its actual object or physical facts"
                );
            }
            cleanups.insert(name, cleanup);
        }
    }
    Ok(cleanups)
}
fn validate_group_creation(
    facts: &[PhaseFact],
    group_name: &str,
    record: &GroupRecord,
) -> Result<()> {
    let mut intent = false;
    let mut allocated = false;
    let mut finalized = false;
    for fact in facts.iter().filter(|fact| fact.checkpoint) {
        let body: serde_json::Value = serde_json::from_slice(&fact.body)?;
        match body.get("kind").and_then(|kind| kind.as_str()) {
            Some("profile_group_allocation_intent") => {
                let target: FileIdentity = serde_json::from_value(
                    body.get("target")
                        .cloned()
                        .context("Group creation intent has no actual target")?,
                )?;
                ensure!(
                    fact.phase == SnapshotPhase::CompactVerified
                        && !intent
                        && !allocated
                        && !finalized
                        && body.get("name").and_then(|name| name.as_str()) == Some(group_name)
                        && target == record.target,
                    "Group creation intent differs from the terminal actual target or journal"
                );
                intent = true;
            }
            Some("profile_group_allocated") => {
                let identity: FileIdentity = serde_json::from_value(
                    body.get("identity")
                        .cloned()
                        .context("Group creation allocation has no actual FID")?,
                )?;
                ensure!(
                    fact.phase == SnapshotPhase::CompactVerified
                        && intent
                        && !allocated
                        && !finalized
                        && body.get("name").and_then(|name| name.as_str()) == Some(group_name)
                        && identity == record.journal,
                    "Group creation allocation differs from the terminal actual journal FID"
                );
                allocated = true;
            }
            Some("profile_group_final") => {
                ensure!(
                    intent && allocated,
                    "Terminal final record precedes its durable group creation witnesses"
                );
                finalized = true;
            }
            _ => (),
        }
    }
    ensure!(
        intent && allocated && finalized,
        "Terminal manifest lacks its durable group creation witnesses"
    );
    Ok(())
}
impl PhysicalProfileTerminalSelection {
    pub fn preview(&self) -> Option<&PhysicalProfileRecoveryPreview> {
        self.group.preview()
    }
    pub fn approve_current_content(&mut self, fingerprint: &str) -> Result<()> {
        self.group.approve_current_content(fingerprint)
    }
    pub fn open_selected(
        guard: &ProfileGuard,
        manifest_path: impl AsRef<Path>,
        selection_epoch: u64,
        limits: &PhysicalRestoreLimits,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<Self, PhysicalProfileTerminalSelectionFailure> {
        let preliminary = (|| -> Result<(SnapshotDirectory, SnapshotDirectory, String, ProfileContinuityToken)> {
            guard.verify()?;
            let target = SnapshotDirectory::pin_profile(guard.directory(), selection_epoch)?;
            let private = SnapshotDirectory::open_selected(
                guard.directory().join(".lx-snapshot-staging"),
                selection_epoch,
            )?;
            ensure!(
                manifest_path.as_ref().parent() == Some(private.path()),
                "Selected prepare manifest is outside this actual profile"
            );
            let name = manifest_path
                .as_ref()
                .file_name()
                .and_then(|name| name.to_str())
                .context("Selected manifest leaf is invalid")?
                .to_owned();
            manifest_nonce(&name)?;
            Ok((target, private, name, guard.continuity_token()?))
        })();
        let (target, private, manifest_name, continuity) = preliminary
            .map_err(|error| PhysicalProfileTerminalSelectionFailure { error, owner: None })?;
        let mut owner = Self {
            continuity,
            group: PhysicalProfileRecoverySelection {
                target,
                private,
                journal: None,
                journal_name: String::new(),
                record: None,
                valid_bytes: 0,
                journal_bytes: 0,
                originals: Vec::new(),
                replacements: Vec::new(),
                obstructions: Vec::new(),
                preview: None,
                accepted_current_content: false,
            },
            manifest: None,
            manifest_metadata: None,
            preparations: Vec::new(),
            journal_cleanup: None,
            permit: None,
            limits: limits.clone(),
        };
        let result = (|| -> Result<()> {
            context.inner.check()?;
            owner.manifest = Some(owner.group.private.open_group_file(&manifest_name, false)?);
            let file = owner.manifest.as_mut().unwrap();
            let manifest_identity = owner.group.private.group_file_identity(file)?;
            owner
                .group
                .private
                .verify_group_name(&manifest_name, &manifest_identity)?;
            let (manifest_bytes, manifest_sha256) =
                hash_file(file, MAX_MANIFEST_BYTES, &context.inner)?;
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = vec![0; manifest_bytes as usize];
            file.read_exact(&mut bytes)?;
            let facts = parse_phase_facts(&bytes)?;
            let created = facts
                .first()
                .context("Selected manifest has no durable Created fact")?;
            ensure!(
                !created.checkpoint && created.phase == SnapshotPhase::Created,
                "Selected manifest has no initial restore request"
            );
            let created: serde_json::Value = serde_json::from_slice(&created.body)?;
            let request: PhysicalRestoreRequest = serde_json::from_value(
                created
                    .get("restore_request")
                    .cloned()
                    .context("Selected manifest is not a physical restore")?,
            )?;
            let locator: PhysicalRestoreLocator = serde_json::from_value(
                created
                    .get("locator")
                    .cloned()
                    .context("Created restore locator is absent")?,
            )?;
            ensure!(
                !request.operation_id.is_empty()
                    && request.operation_id.len() <= 128
                    && !request.operation_id.chars().any(char::is_control)
                    && request.selection_epoch > 0
                    && (1..=MAX_SNAPSHOT_BYTES).contains(&request.maximum_image_bytes)
                    && locator.operation_id == request.operation_id
                    && locator.journal_path == owner.group.private.path().join(&manifest_name)
                    && locator.journal_device == manifest_identity.device
                    && locator.journal_object == manifest_identity.object
                    && locator.target_device == owner.group.target.identity().device
                    && locator.target_object == owner.group.target.identity().object
                    && locator.request_sha256
                        == format!("{:x}", Sha256::digest(serde_json::to_vec(&request)?)),
                "Created request does not bind this actual manifest and profile root"
            );
            owner.permit = Some(
                owner
                    .limits
                    .reconstruct_retained(&request, owner.group.target.identity())?,
            );
            for fact in facts.iter().filter(|fact| fact.checkpoint) {
                let body: serde_json::Value = serde_json::from_slice(&fact.body)?;
                ensure!(
                    matches!(
                        body.get("kind").and_then(|kind| kind.as_str()),
                        Some(
                            "allocation"
                                | "cleanup"
                                | "profile_group_allocation_intent"
                                | "profile_group_allocated"
                                | "profile_group_final"
                                | "profile_group_journal_cleanup"
                                | "profile_group_terminal_cleanup"
                        )
                    ),
                    "Unknown parent checkpoint retains its operation charge and actual manifest"
                );
            }
            let cleanups = latest_cleanups(&facts)?;
            let journal = cleanups
                .values()
                .find(|fact| {
                    fact.file
                        .temporary_name
                        .as_deref()
                        .is_some_and(|name| journal_nonce(name).is_ok())
                })
                .context("Parent manifest has no group-journal physical disposition witness")?
                .clone();
            let group_name = journal.file.temporary_name.clone().unwrap();
            let record = final_record(&facts, &group_name)?;
            ensure!(
                record.target == *owner.group.target.identity()
                    && serde_json::to_vec(&record.request)? == serde_json::to_vec(&request)?
                    && record.manifest.identity.as_ref() == Some(&manifest_identity)
                    && record.manifest.directory == owner.group.private.path()
                    && record.manifest.directory_identity == *owner.group.private.identity()
                    && record.manifest.temporary_name.as_deref() == Some(manifest_name.as_str())
                    && journal.file.identity.as_ref() == Some(&record.journal)
                    && journal.file.operation_id == request.operation_id
                    && journal.file.directory == owner.group.private.path()
                    && journal.file.directory_identity == *owner.group.private.identity()
                    && journal.pending_zero_links(),
                "Parent final record or group retirement does not bind the selected actual objects"
            );
            validate_group_creation(&facts, &group_name, &record)?;
            let image_fact = facts
                .iter()
                .find(|fact| !fact.checkpoint && fact.phase == SnapshotPhase::CompactVerified)
                .context("Terminal parent manifest has no compact image proof")?;
            let image: PhysicalRestoreImageView = serde_json::from_slice(&image_fact.body)?;
            ensure!(
                image.operation_id == request.operation_id
                    && serde_json::to_vec(&record.image)? == serde_json::to_vec(&Some(image))?,
                "Terminal group compact image proof differs from the preparation manifest"
            );
            // Even a foreign occupant at the old journal leaf is retained; its
            // path is never taken as proof that the original FID was retired.
            match owner.group.private.open_group_file(&group_name, false) {
                Ok(file) => {
                    owner.group.obstructions.push(file);
                    anyhow::bail!("Group-journal leaf still has an actual occupant");
                }
                Err(error) if not_found(&error) => (),
                Err(error) => return Err(error),
            }
            owner.group.journal_name = group_name;
            owner.journal_cleanup = Some(journal);
            let installed = record.terminal_installed.unwrap();
            for (original, files) in [(true, &record.originals), (false, &record.replacements)] {
                for file in files {
                    if original == installed {
                        let cleanup = record
                            .cleanup
                            .iter()
                            .find(|fact| fact.file.identity.as_ref() == Some(&file.identity))
                            .context("Finalizing group has an unproven retired object")?;
                        ensure!(
                            file.private
                                && cleanup.pending_zero_links()
                                && cleanup.removed
                                && cleanup.file.operation_id == request.operation_id
                                && cleanup.file.directory_identity
                                    == *owner.group.private.identity()
                                && cleanup.file.directory == owner.group.private.path()
                                && cleanup.file.temporary_name.as_deref()
                                    == Some(file.private_name.as_str()),
                            "Finalizing group retirement is outside the exact private object"
                        );
                        let body = serde_json::to_value(cleanup)?;
                        ensure!(
                            cleanup.file.bytes == file.bytes
                                && body.get("sha256").and_then(|value| value.as_str())
                                    == Some(file.sha256.as_str()),
                            "Finalizing retirement witness differs from the recorded whole body"
                        );
                    } else {
                        ensure!(!file.private && !record.cleanup.iter().any(|fact| fact.file.identity.as_ref() == Some(&file.identity)),
                            "Terminal current file is private or recorded as retired");
                    }
                }
            }
            ensure!(
                record.allocations.iter().all(|intent| record
                    .replacements
                    .iter()
                    .any(|file| file.private_name == intent.private_name)),
                "Terminal group allocation has no durable actual FID; charge remains retained"
            );
            owner.manifest_metadata = Some(ActualFileMetadata {
                bytes: manifest_bytes,
                ..record.manifest.clone()
            });
            // A reopened SQLite writer may have legitimately changed the same
            // main body. Fresh explicit approval binds its present complete SHA.
            let mut capture = record.clone();
            capture.phase = if installed {
                GroupPhase::Reopened
            } else {
                GroupPhase::OriginalReopened
            };
            owner.group.record = Some(capture);
            owner.group.capture_files(&context.inner)?;
            owner.group.record = Some(record.clone());
            let mut allocations: BTreeMap<String, ActualFileMetadata> = BTreeMap::new();
            for fact in facts.iter().filter(|fact| fact.checkpoint) {
                let body: serde_json::Value = serde_json::from_slice(&fact.body)?;
                if body.get("kind").and_then(|kind| kind.as_str()) == Some("allocation") {
                    let metadata: ActualFileMetadata = serde_json::from_value(
                        body.get("file")
                            .cloned()
                            .context("Preparation allocation has no metadata")?,
                    )?;
                    validate_preparation(&metadata, &request, &owner.group.private)?;
                    ensure!(
                        matches!(
                            body.get("role").and_then(|role| role.as_str()),
                            Some("restore-main" | "main" | "auxiliary")
                        ),
                        "Unknown preparation allocation role retains its charge"
                    );
                    if let Some(previous) = allocations.get(&metadata.nonce) {
                        ensure!(previous.identity.as_ref().is_none_or(|identity| metadata.identity.as_ref() == Some(identity)),
                            "Preparation allocation replaced its recorded actual FID");
                    }
                    allocations.insert(metadata.nonce.clone(), metadata);
                    ensure!(
                        allocations.len() <= 14,
                        "Preparation allocation count exceeds its admitted bound"
                    );
                }
            }
            for metadata in allocations.into_values() {
                ensure!(
                    metadata.identity.is_some(),
                    "Preparation allocation lacks a durable actual FID; charge and actual selection remain retained"
                );
                let name = metadata.temporary_name.as_ref().unwrap().clone();
                let cleanup = cleanups.get(&name).cloned();
                if let Some(cleanup) = &cleanup {
                    ensure!(
                        cleanup.file.identity == metadata.identity
                            && cleanup.file.operation_id == request.operation_id
                            && cleanup.file.directory_identity == *owner.group.private.identity()
                            && cleanup.file.directory == metadata.directory
                            && cleanup.file.nonce == metadata.nonce
                            && cleanup.file.temporary_name == metadata.temporary_name
                            && !cleanup.file.published
                            && cleanup.file.final_basename.is_none()
                            && cleanup.file.bytes <= request.maximum_image_bytes,
                        "Preparation cleanup identifies another actual allocation"
                    );
                }
                owner.preparations.push(PreparationFile {
                    metadata,
                    inspection: None,
                    sha256: String::new(),
                    cleanup,
                    confirmation: None,
                    confirmation_acknowledged: false,
                    owned_index: None,
                    transferred: false,
                });
                let preparation = owner.preparations.last_mut().unwrap();
                if preparation
                    .cleanup
                    .as_ref()
                    .is_some_and(|fact| fact.pending_zero_links())
                {
                    match owner.group.private.open_group_file(&name, false) {
                        Ok(file) => {
                            owner.group.obstructions.push(file);
                            anyhow::bail!(
                                "Retired preparation leaf has an actual occupant; charge remains retained"
                            );
                        }
                        Err(error) if not_found(&error) => (),
                        Err(error) => return Err(error),
                    }
                    continue;
                }
                preparation.inspection = Some(owner.group.private.open_group_file(&name, false)?);
                let file = preparation.inspection.as_mut().unwrap();
                ensure!(
                    owner.group.private.group_file_identity(file)?
                        == *preparation.metadata.identity.as_ref().unwrap(),
                    "Preparation leaf has a foreign or aliased actual object"
                );
                owner
                    .group
                    .private
                    .verify_group_name(&name, preparation.metadata.identity.as_ref().unwrap())?;
                let (bytes, sha256) = hash_file(file, request.maximum_image_bytes, &context.inner)?;
                preparation.metadata.bytes = bytes;
                preparation.sha256 = sha256;
            }
            if let Some(main) = &record.prepared_main {
                ensure!(
                    owner
                        .preparations
                        .iter()
                        .any(|file| file.metadata.identity == main.identity),
                    "Final group prepared-main allocation is unregistered"
                );
            }
            let mut current_files = owner
                .group
                .originals
                .iter()
                .chain(&owner.group.replacements)
                .filter(|file| file.file.is_some())
                .filter_map(|file| file.fact.as_ref())
                .map(|fact| PhysicalProfileRecoveryFile {
                    role: fact.role.clone(),
                    path: owner.group.target.path().join(&fact.live_name),
                    identity: describe_identity(&fact.identity),
                    bytes: fact.bytes,
                    sha256: fact.sha256.clone(),
                    recorded_sha256: record
                        .originals
                        .iter()
                        .chain(&record.replacements)
                        .find(|recorded| recorded.identity == fact.identity)
                        .unwrap()
                        .sha256
                        .clone(),
                })
                .collect::<Vec<_>>();
            current_files.extend(
                owner
                    .preparations
                    .iter()
                    .filter(|file| file.inspection.is_some())
                    .map(|file| PhysicalProfileRecoveryFile {
                        role: "prepare-private".to_owned(),
                        path: owner
                            .group
                            .private
                            .path()
                            .join(file.metadata.temporary_name.as_ref().unwrap()),
                        identity: describe_identity(file.metadata.identity.as_ref().unwrap()),
                        bytes: file.metadata.bytes,
                        sha256: file.sha256.clone(),
                        recorded_sha256: String::new(),
                    }),
            );
            let mut preview = PhysicalProfileRecoveryPreview {
                operation_id: request.operation_id,
                journal_path: owner.group.private.path().join(&manifest_name),
                journal_identity: describe_identity(&manifest_identity),
                journal_sha256: manifest_sha256,
                selection_epoch,
                phase: format!("Finalizing({installed})"),
                current_files,
                approval_sha256: String::new(),
            };
            preview.approval_sha256 =
                format!("{:x}", Sha256::digest(serde_json::to_vec(&preview)?));
            owner.group.preview = Some(preview);
            guard.verify()?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(PhysicalProfileTerminalSelectionFailure {
                error,
                owner: Some(Box::new(owner)),
            }),
        }
    }
}

pub struct PhysicalProfileTerminalAdmission {
    guard: Option<ProfileGuard>,
    selection: Option<PhysicalProfileTerminalSelection>,
}
pub struct PhysicalProfileTerminalRecovery {
    installation: PhysicalProfileInstallation,
    manifest_inspection: Option<File>,
    manifest_metadata: ActualFileMetadata,
    manifest_sha256: String,
    manifest_pending: Option<ActualFile>,
    manifest_recovered: bool,
    preparations: Vec<PreparationFile>,
    group_confirmations: Vec<ColdCleanup>,
    group_acknowledged: Vec<bool>,
    journal_confirmation: Option<ColdCleanup>,
    journal_acknowledged: bool,
    confirmations_acknowledged: bool,
    preparations_adopted: bool,
}
impl std::fmt::Debug for PhysicalProfileTerminalRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileTerminalRecovery")
            .field("installation", &self.installation)
            .field("retained_preparation_files", &self.preparations.len())
            .field("confirmed_retirements", &self.confirmations_acknowledged)
            .finish()
    }
}
pub enum PhysicalProfileTerminalRecoveryOwner {
    Admission(Box<PhysicalProfileTerminalAdmission>),
    Recovery(Box<PhysicalProfileTerminalRecovery>),
}

pub struct PhysicalProfileTerminalRecoveryFailure {
    pub error: anyhow::Error,
    pub owner: PhysicalProfileTerminalRecoveryOwner,
}
impl std::fmt::Debug for PhysicalProfileTerminalRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileTerminalRecoveryFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for PhysicalProfileTerminalRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for PhysicalProfileTerminalRecoveryFailure {}
impl PhysicalProfileTerminalAdmission {
    pub fn preview(&self) -> Option<&PhysicalProfileRecoveryPreview> {
        self.selection
            .as_ref()
            .and_then(PhysicalProfileTerminalSelection::preview)
    }
    pub fn approve_current_content(&mut self, fingerprint: &str) -> Result<()> {
        self.selection
            .as_mut()
            .context("Terminal admission lost the actual selection")?
            .approve_current_content(fingerprint)
    }
    pub fn recover(
        mut self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileTerminalRecovery, PhysicalProfileTerminalRecoveryFailure>
    {
        let preliminary = (|| -> Result<()> {
            let guard = self
                .guard
                .as_ref()
                .context("Terminal admission lost the actual guard")?;
            let selection = self
                .selection
                .as_ref()
                .context("Terminal admission lost the actual selection")?;
            guard.verify()?;
            guard.verify_continuity(&selection.continuity)?;
            selection.group.target.verify()?;
            selection.group.private.verify()?;
            ensure!(
                SnapshotDirectory::pin_profile(guard.directory(), selection.group.target.epoch())?
                    .identity()
                    == selection.group.target.identity(),
                "Terminal admission identifies another actual profile root"
            );
            ensure!(
                selection.group.accepted_current_content,
                "Terminal recovery requires fresh approval of the actual present content"
            );
            let record = selection
                .group
                .record
                .as_ref()
                .context("Terminal selection has no fixed final record")?;
            selection
                .limits
                .verify(&record.request, selection.group.target.identity())?;
            ensure!(
                selection.permit.is_some(),
                "Terminal selection did not reconstruct its retained charge"
            );
            Ok(())
        })();
        match preliminary {
            Ok(()) => (),
            Err(error) => {
                return Err(PhysicalProfileTerminalRecoveryFailure {
                    error,
                    owner: PhysicalProfileTerminalRecoveryOwner::Admission(Box::new(self)),
                });
            }
        }
        let mut selection = self.selection.take().unwrap();
        let record = selection.group.record.take().unwrap();
        let prepared = PreparedPhysicalRestore {
            request: record.request.clone(),
            continuity: selection.continuity,
            target: selection.group.target.clone(),
            private: selection.group.private.clone(),
            source: None,
            manifest: None,
            sqlite: None,
            registry: None,
            main: None,
            auxiliary: Vec::new(),
            permit: selection.permit.take().unwrap(),
            limits: selection.limits,
            locator: None,
            view: record.image.clone(),
            authority: None,
        };
        let installation = PhysicalProfileInstallation {
            prepared,
            guard: self.guard.take(),
            repository: None,
            opening: None,
            target: selection.group.target,
            private: selection.group.private,
            originals: selection.group.originals,
            replacements: selection.group.replacements,
            allocations: record.allocations.clone(),
            obstructions: selection.group.obstructions,
            journal: None,
            journal_name: selection.group.journal_name,
            journal_bytes: 0,
            journal_dirty: false,
            sequence: record.sequence,
            phase: GroupPhase::Finalizing,
            journal_retirement: None,
            journal_cleanup: selection.journal_cleanup,
            terminal_installed: record.terminal_installed,
            last_record: Some(record),
        };
        let manifest_sha256 = selection
            .group
            .preview
            .as_ref()
            .unwrap()
            .journal_sha256
            .clone();
        let mut owner = PhysicalProfileTerminalRecovery {
            installation,
            manifest_inspection: selection.manifest,
            manifest_metadata: selection.manifest_metadata.unwrap(),
            manifest_sha256,
            manifest_pending: None,
            manifest_recovered: false,
            preparations: selection.preparations,
            group_confirmations: Vec::new(),
            group_acknowledged: Vec::new(),
            journal_confirmation: None,
            journal_acknowledged: false,
            confirmations_acknowledged: false,
            preparations_adopted: false,
        };
        match owner.recover_manifest(context) {
            Ok(()) => Ok(owner),
            Err(error) => Err(owner.fail(error)),
        }
    }
}
impl PhysicalProfileTerminalRecovery {
    pub fn release_obstructions(&mut self) -> Result<()> {
        self.installation.release_obstructions()
    }
    pub fn recover_selected(
        guard: ProfileGuard,
        selection: PhysicalProfileTerminalSelection,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<Self, PhysicalProfileTerminalRecoveryFailure> {
        PhysicalProfileTerminalAdmission {
            guard: Some(guard),
            selection: Some(selection),
        }
        .recover(context)
    }
    fn fail(self, error: anyhow::Error) -> PhysicalProfileTerminalRecoveryFailure {
        PhysicalProfileTerminalRecoveryFailure {
            error,
            owner: PhysicalProfileTerminalRecoveryOwner::Recovery(Box::new(self)),
        }
    }
    fn recover_manifest(&mut self, context: &PhysicalRestoreContext) -> Result<()> {
        if self.manifest_recovered {
            return Ok(());
        }
        if let Some(manifest) = self.installation.prepared.manifest.as_ref() {
            manifest
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .reconcile(&context.inner)?;
            self.manifest_recovered = true;
            return Ok(());
        }
        close_owned(&mut self.manifest_inspection)?;
        if self.manifest_pending.is_none() {
            let probe = ActualFile::inspect_recovery(
                &self.installation.private,
                &self.manifest_metadata,
                MAX_MANIFEST_BYTES,
                &context.inner,
            )?;
            match ActualFile::reopen_recovery(
                probe,
                &context.inner,
                &self.installation.prepared.permit,
            ) {
                Ok(file) => self.manifest_pending = Some(file),
                Err(mut failure) => {
                    self.manifest_pending = failure.file.take().map(|file| *file);
                    anyhow::bail!("{}", failure.message);
                }
            }
        }
        ensure!(
            self.manifest_pending
                .as_mut()
                .unwrap()
                .seal_sha256(&context.inner)?
                == self.manifest_sha256,
            "Selected parent manifest changed after fresh content approval"
        );
        let file = self.manifest_pending.take().unwrap();
        match DurableManifest::from_recovered(file, &context.inner) {
            Ok(manifest) => {
                self.installation.prepared.manifest = Some(Arc::new(Mutex::new(manifest)))
            }
            Err(mut failure) => {
                if let Some(manifest) = failure.manifest.take() {
                    self.installation.prepared.manifest = Some(Arc::new(Mutex::new(*manifest)));
                }
                self.manifest_pending = failure.file.take().map(|file| *file);
                anyhow::bail!("{}", failure.message);
            }
        }
        self.manifest_recovered = true;
        Ok(())
    }
    fn append_cleanup(
        &self,
        kind: &str,
        fact: &DurableCleanupFact,
        context: &SnapshotContext,
    ) -> Result<()> {
        let manifest = self
            .installation
            .prepared
            .manifest
            .as_ref()
            .context("Terminal parent manifest is absent")?;
        let body = serde_json::to_vec(&serde_json::json!({"kind":kind,"fact":fact}))?;
        let mut manifest = manifest.lock().unwrap_or_else(|error| error.into_inner());
        manifest.reconcile(context)?;
        if manifest
            .read_facts(context)?
            .iter()
            .any(|record| record.checkpoint && record.body == body)
        {
            return Ok(());
        }
        manifest.append_checkpoint(&body, context)
    }
    fn confirm_retired(&mut self, context: &PhysicalRestoreContext) -> Result<()> {
        if self.confirmations_acknowledged {
            return Ok(());
        }
        let private = self.installation.private.clone();
        let installed = self.installation.terminal_installed.unwrap();
        let facts = if installed {
            &self.installation.originals
        } else {
            &self.installation.replacements
        };
        if self.group_confirmations.len() < facts.len() {
            for file in &facts[self.group_confirmations.len()..] {
                self.group_confirmations.push(ColdCleanup::prepare(
                    private.clone(),
                    file.cleanup
                        .clone()
                        .context("Terminal retired file has no durable witness")?,
                )?);
                self.group_acknowledged.push(false);
            }
        }
        for index in 0..self.group_confirmations.len() {
            if self.group_acknowledged[index] {
                continue;
            }
            let fact = self.group_confirmations[index].confirm(&context.inner)?;
            self.append_cleanup("profile_group_terminal_cleanup", &fact, &context.inner)?;
            self.group_acknowledged[index] = true;
        }
        if self.journal_confirmation.is_none() {
            self.journal_confirmation = Some(ColdCleanup::prepare(
                private.clone(),
                self.installation
                    .journal_cleanup
                    .clone()
                    .context("Terminal group journal has no durable retirement witness")?,
            )?);
        }
        if !self.journal_acknowledged {
            let fact = self
                .journal_confirmation
                .as_mut()
                .unwrap()
                .confirm(&context.inner)?;
            self.append_cleanup("profile_group_journal_cleanup", &fact, &context.inner)?;
            self.installation.journal_cleanup = Some(fact);
            self.journal_acknowledged = true;
        }
        for index in 0..self.preparations.len() {
            if self.preparations[index].confirmation_acknowledged {
                continue;
            }
            if self.preparations[index]
                .cleanup
                .as_ref()
                .is_some_and(|fact| fact.pending_zero_links())
            {
                if self.preparations[index].confirmation.is_none() {
                    self.preparations[index].confirmation = Some(ColdCleanup::prepare(
                        private.clone(),
                        self.preparations[index].cleanup.clone().unwrap(),
                    )?);
                }
                let fact = self.preparations[index]
                    .confirmation
                    .as_mut()
                    .unwrap()
                    .confirm(&context.inner)?;
                self.append_cleanup("cleanup", &fact, &context.inner)?;
                self.preparations[index].cleanup = Some(fact);
                self.preparations[index].confirmation_acknowledged = true;
            }
        }
        self.confirmations_acknowledged = true;
        Ok(())
    }
    fn reopen_current(&mut self, context: &PhysicalRestoreContext) -> Result<()> {
        if self.installation.opening.is_none()
            && let Some(repository) = self.installation.repository.as_ref()
        {
            repository.integrity_check()?;
            self.installation.check(&context.inner)?;
            return Ok(());
        }
        self.installation.close_repository()?;
        self.installation.check(&context.inner)?;
        self.installation.require_empty_sidecars()?;
        let current = if self.installation.terminal_installed.unwrap() {
            &mut self.installation.replacements
        } else {
            &mut self.installation.originals
        };
        for file in current.iter_mut() {
            let expected = file
                .fact
                .as_ref()
                .context("Terminal current file has no approved actual proof")?
                .clone();
            if file.file.is_none() {
                file.file = Some(
                    self.installation
                        .target
                        .open_group_file(&file.live_name, false)?,
                );
            }
            let native = file.file.as_mut().unwrap();
            ensure!(
                self.installation.target.group_file_identity(native)? == expected.identity,
                "Terminal current file was replaced after approval"
            );
            self.installation
                .target
                .verify_group_name(&file.live_name, &expected.identity)?;
            let maximum = if file.role == "main" {
                self.installation.prepared.request.maximum_image_bytes
            } else {
                64 * 1024 * 1024
            };
            let (bytes, sha256) = hash_file(native, maximum, &context.inner)?;
            ensure!(
                bytes == expected.bytes && sha256 == expected.sha256,
                "Terminal current file body changed after approval; a new actual selection is required"
            );
        }
        let identity = current
            .iter()
            .find(|file| file.role == "main")
            .and_then(|file| file.fact.as_ref())
            .context("Terminal profile has no actual current main")?
            .identity
            .clone();
        for file in current {
            close_owned(&mut file.file)?;
        }
        let guard = self
            .installation
            .guard
            .take()
            .context("Terminal reopen lost its continuously held guard")?;
        match Repository::open_installed_with_guard(guard, identity) {
            Ok(repository) => self.installation.repository = Some(Arc::new(repository)),
            Err(failure) => {
                self.installation.opening = Some(failure.owner);
                return Err(failure.error);
            }
        }
        self.installation
            .repository
            .as_ref()
            .unwrap()
            .integrity_check()?;
        Ok(())
    }
    fn recover_preparations(&mut self, context: &PhysicalRestoreContext) -> Result<()> {
        if self.preparations_adopted {
            return Ok(());
        }
        for index in 0..self.preparations.len() {
            let file = &mut self.preparations[index];
            if file.transferred || file.cleanup.as_ref().is_some_and(|fact| fact.removed) {
                continue;
            }
            if file.owned_index.is_none() {
                close_owned(&mut file.inspection)?;
                let probe = ActualFile::inspect_recovery(
                    &self.installation.private,
                    &file.metadata,
                    self.installation.prepared.request.maximum_image_bytes,
                    &context.inner,
                )?;
                let actual = match ActualFile::reopen_recovery(
                    probe,
                    &context.inner,
                    &self.installation.prepared.permit,
                ) {
                    Ok(file) => file,
                    Err(mut failure) => {
                        if let Some(actual) = failure.file.take() {
                            file.owned_index = Some(self.installation.prepared.auxiliary.len());
                            self.installation.prepared.auxiliary.push(*actual);
                        }
                        anyhow::bail!("{}", failure.message);
                    }
                };
                file.owned_index = Some(self.installation.prepared.auxiliary.len());
                self.installation.prepared.auxiliary.push(actual);
            }
            self.installation.prepared.auxiliary[file.owned_index.unwrap()]
                .restore_sealed(&file.sha256, &context.inner)?;
            // Once handed over, retry uses the retained ActualFile; no path is
            // opened again if cleanup or digest verification subsequently fails.
            file.transferred = true;
        }
        self.preparations_adopted = true;
        Ok(())
    }
    /// Continues the recorded terminal direction with the same current guard.
    pub fn finish(
        mut self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileTerminal, PhysicalProfileTerminalRecoveryFailure> {
        let result = (|| -> Result<()> {
            self.recover_manifest(context)?;
            if self.installation.opening.is_some() {
                self.installation.close_repository()?;
            }
            self.installation.check(&context.inner)?;
            self.confirm_retired(context)?;
            self.reopen_current(context)?;
            self.recover_preparations(context)?;
            self.installation.release_obstructions()?;
            self.installation.prepared.cleanup_private(context)
        })();
        match result {
            Ok(()) => Ok(PhysicalProfileTerminal {
                repository: self.installation.repository.take().unwrap(),
                operation_id: self.installation.prepared.request.operation_id.clone(),
                installed: self.installation.terminal_installed.unwrap(),
            }),
            Err(error) => Err(self.fail(error)),
        }
    }
}

#[cfg(test)]
#[path = "terminal/tests.rs"]
mod tests;
