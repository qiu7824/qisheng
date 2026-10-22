//! Fresh actual-object selection for closed-profile group recovery.
use super::*;

#[path = "terminal.rs"]
mod terminal;
pub use terminal::{
    PhysicalProfileTerminalAdmission, PhysicalProfileTerminalRecovery,
    PhysicalProfileTerminalRecoveryFailure, PhysicalProfileTerminalRecoveryOwner,
    PhysicalProfileTerminalSelection, PhysicalProfileTerminalSelectionFailure,
};
#[cfg(test)]
#[path = "cold_repeat_tests.rs"]
mod repeat_tests;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PhysicalProfileFileIdentity {
    pub device: u64,
    pub object: String,
}
fn describe_identity(identity: &FileIdentity) -> PhysicalProfileFileIdentity {
    PhysicalProfileFileIdentity {
        device: identity.device,
        object: identity.object.clone(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PhysicalProfileRecoveryPreview {
    pub operation_id: String,
    pub journal_path: PathBuf,
    pub journal_identity: PhysicalProfileFileIdentity,
    pub journal_sha256: String,
    pub selection_epoch: u64,
    pub phase: String,
    pub current_files: Vec<PhysicalProfileRecoveryFile>,
    pub approval_sha256: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct PhysicalProfileRecoveryFile {
    pub role: String,
    pub path: PathBuf,
    pub identity: PhysicalProfileFileIdentity,
    pub bytes: u64,
    pub sha256: String,
    pub recorded_sha256: String,
}

/// This type owns fresh native pins and file descriptors. Preview JSON cannot
/// construct it and a persisted journal grants no namespace mutation itself.
pub struct PhysicalProfileRecoverySelection {
    target: SnapshotDirectory,
    private: SnapshotDirectory,
    journal: Option<File>,
    journal_name: String,
    record: Option<GroupRecord>,
    valid_bytes: u64,
    journal_bytes: u64,
    originals: Vec<OwnedGroupFile>,
    replacements: Vec<OwnedGroupFile>,
    obstructions: Vec<File>,
    preview: Option<PhysicalProfileRecoveryPreview>,
    accepted_current_content: bool,
}
pub struct PhysicalProfileRecoverySelectionFailure {
    pub error: anyhow::Error,
    pub owner: Option<Box<PhysicalProfileRecoverySelection>>,
}
impl std::fmt::Debug for PhysicalProfileRecoverySelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileRecoverySelection")
            .field("preview", &self.preview)
            .field("retained_obstructions", &self.obstructions.len())
            .finish()
    }
}
impl std::fmt::Debug for PhysicalProfileRecoverySelectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileRecoverySelectionFailure")
            .field("error", &self.error)
            .field("owner", &self.owner)
            .finish()
    }
}
impl std::fmt::Display for PhysicalProfileRecoverySelectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for PhysicalProfileRecoverySelectionFailure {}

fn journal_nonce(name: &str) -> Result<&str> {
    let nonce = name
        .strip_prefix(".lx-profile-")
        .and_then(|name| name.strip_suffix(".journal"))
        .context("Recovery requires an explicitly selected profile group journal")?;
    ensure!(
        nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Recovery group journal nonce is invalid"
    );
    Ok(nonce)
}
fn validate_file_fact(fact: &FileFact, nonce: &str, original: bool, maximum: u64) -> Result<()> {
    let expected = match fact.role.as_str() {
        "main" => DATABASE_NAME,
        "store" => STORE_NAME,
        "wal" if original => "native-library.sqlite3-wal",
        "shm" if original => "native-library.sqlite3-shm",
        "rollback-journal" if original => "native-library.sqlite3-journal",
        _ => anyhow::bail!("Recovery group file role is invalid"),
    };
    ensure!(
        fact.live_name == expected
            && fact.private_name
                == format!(
                    ".lx-profile-{nonce}-{}.{}",
                    fact.role,
                    if original { "old" } else { "new" }
                )
            && fact.bytes
                <= if fact.role == "main" {
                    maximum
                } else {
                    64 * 1024 * 1024
                }
            && fact.sha256.len() == 64
            && fact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            && fact.identity.object.len() == 32
            && fact
                .identity
                .object
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Recovery group file facts exceed their admitted namespace or bound"
    );
    Ok(())
}
fn parse_records(bytes: &[u8], name: &str) -> Result<(GroupRecord, u64)> {
    ensure!(
        bytes.len() as u64 <= MAX_GROUP_JOURNAL,
        "Recovery group journal exceeds 1 MiB"
    );
    let nonce = journal_nonce(name)?;
    let mut at = 0usize;
    let mut latest: Option<GroupRecord> = None;
    while at < bytes.len() {
        if bytes.len() - at < 36 {
            break;
        }
        let length = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        ensure!(
            (1..=64 * 1024).contains(&length),
            "Recovery group record length is invalid"
        );
        let end = at
            .checked_add(36)
            .and_then(|at| at.checked_add(length))
            .context("Recovery record length overflow")?;
        if end > bytes.len() {
            break;
        }
        let body = &bytes[at + 36..end];
        ensure!(
            Sha256::digest(body)[..] == bytes[at + 4..at + 36],
            "Recovery group record checksum is invalid"
        );
        let record: GroupRecord = serde_json::from_slice(body)?;
        ensure!(
            record.version == 1
                && record.sequence > 0
                && record.sequence <= 64
                && record.operation_id == record.request.operation_id
                && !record.operation_id.is_empty()
                && record.operation_id.len() <= 128
                && !record.operation_id.chars().any(char::is_control)
                && record.request.selection_epoch > 0
                && (1..=MAX_SNAPSHOT_BYTES).contains(&record.request.maximum_image_bytes)
                && record.originals.len() <= 5
                && record.replacements.len() <= 2
                && record.allocations.len() <= 2
                && record.cleanup.len() <= 8,
            "Recovery group record identity or bounds are invalid"
        );
        if let Some(previous) = &latest {
            ensure!(
                record.sequence == previous.sequence + 1
                    && record.operation_id == previous.operation_id
                    && record.target == previous.target
                    && record.journal == previous.journal
                    && serde_json::to_vec(&record.request)?
                        == serde_json::to_vec(&previous.request)?
                    && record.manifest.identity == previous.manifest.identity,
                "Recovery group record chain changed ownership"
            );
            ensure!(
                phase_follows(previous.phase, record.phase),
                "Recovery group phase transition is invalid"
            );
            ensure!(
                previous
                    .terminal_installed
                    .is_none_or(|mode| record.terminal_installed == Some(mode)),
                "Recovery group terminal disposition changed"
            );
        } else {
            ensure!(
                record.sequence == 1,
                "Recovery journal does not start with its first durable record"
            );
        }
        for (original, files) in [(true, &record.originals), (false, &record.replacements)] {
            let mut roles = std::collections::BTreeSet::new();
            for fact in files {
                validate_file_fact(fact, nonce, original, record.request.maximum_image_bytes)?;
                ensure!(
                    roles.insert(&fact.role),
                    "Recovery group repeats a file role"
                );
            }
        }
        for intent in &record.allocations {
            ensure!(
                matches!(intent.role.as_str(), "main" | "store")
                    && intent.private_name == format!(".lx-profile-{nonce}-{}.new", intent.role)
                    && intent.live_name
                        == if intent.role == "main" {
                            DATABASE_NAME
                        } else {
                            STORE_NAME
                        },
                "Recovery allocation intent leaves its admitted namespace"
            );
        }
        ensure!(
            record.manifest.operation_id == record.operation_id
                && record.manifest.identity.is_some()
                && !record.manifest.published
                && record.manifest.nonce.len() == 32
                && record
                    .manifest
                    .nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && record.manifest.temporary_name.as_deref()
                    == Some(format!(".lx-snapshot-{}.manifest", record.manifest.nonce).as_str()),
            "Recovery group preparation manifest binding is invalid"
        );
        if let Some(main) = &record.prepared_main {
            ensure!(
                main.operation_id == record.operation_id
                    && main.identity.is_some()
                    && !main.published
                    && main.nonce.len() == 32
                    && main.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && main.temporary_name.as_deref()
                        == Some(format!(".lx-snapshot-{}.part", main.nonce).as_str())
                    && main.directory_identity == record.manifest.directory_identity,
                "Recovery group preparation image binding is invalid"
            );
        }
        latest = Some(record);
        at = end;
    }
    Ok((
        latest.context("Recovery journal has no complete durable group record")?,
        at as u64,
    ))
}
fn phase_follows(previous: GroupPhase, next: GroupPhase) -> bool {
    use GroupPhase::*;
    previous == next
        || matches!(
            (previous, next),
            (Capturing, Captured)
                | (Captured, Staging)
                | (Staging, Installing)
                | (Installing, Installed)
                | (Installed, Reopening)
                | (Reopening, Reopened)
                | (RolledBack, OriginalOpening)
                | (OriginalOpening, OriginalReopened)
                | (
                    Capturing
                        | Captured
                        | Staging
                        | Installing
                        | Installed
                        | Reopening
                        | Reopened
                        | OriginalOpening
                        | OriginalReopened,
                    RollingBack
                )
                | (RollingBack, RolledBack)
                | (Reopened, Committing)
                | (OriginalReopened, RestoringCleanup)
                | (Committing | RestoringCleanup, Finalizing)
        )
}

impl PhysicalProfileRecoverySelection {
    pub fn preview(&self) -> Option<&PhysicalProfileRecoveryPreview> {
        self.preview.as_ref()
    }
    pub fn approve_current_content(&mut self, approval_sha256: &str) -> Result<()> {
        let preview = self
            .preview
            .as_ref()
            .context("Recovery selection has no verified preview")?;
        ensure!(
            preview.approval_sha256 == approval_sha256,
            "Recovery approval identifies a different actual file selection"
        );
        self.accepted_current_content = true;
        Ok(())
    }
    /// The caller explicitly selects this current journal while owning a closed
    /// profile guard. Every write-capable FH is retained before its inspection.
    pub fn open_selected(
        guard: &ProfileGuard,
        journal_path: impl AsRef<Path>,
        selection_epoch: u64,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<Self, PhysicalProfileRecoverySelectionFailure> {
        let preliminary = (|| -> Result<(SnapshotDirectory, SnapshotDirectory, String)> {
            guard.verify()?;
            let target = SnapshotDirectory::pin_profile(guard.directory(), selection_epoch)?;
            let private = SnapshotDirectory::open_selected(
                guard.directory().join(".lx-snapshot-staging"),
                selection_epoch,
            )?;
            let path = journal_path.as_ref();
            ensure!(
                path.parent() == Some(private.path()),
                "Selected recovery journal is outside this actual profile staging directory"
            );
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("Selected journal leaf is invalid")?
                .to_owned();
            journal_nonce(&name)?;
            Ok((target, private, name))
        })();
        let (target, private, journal_name) = preliminary
            .map_err(|error| PhysicalProfileRecoverySelectionFailure { error, owner: None })?;
        let mut owner = Self {
            target,
            private,
            journal: None,
            journal_name,
            record: None,
            valid_bytes: 0,
            journal_bytes: 0,
            originals: Vec::new(),
            replacements: Vec::new(),
            obstructions: Vec::new(),
            preview: None,
            accepted_current_content: false,
        };
        let result = (|| -> Result<()> {
            context.inner.check()?;
            owner.journal = Some(owner.private.open_group_file(&owner.journal_name, false)?);
            let file = owner.journal.as_mut().unwrap();
            let (length, journal_sha256) = hash_file(file, MAX_GROUP_JOURNAL, &context.inner)?;
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = vec![0; length as usize];
            file.read_exact(&mut bytes)?;
            let (record, valid_bytes) = parse_records(&bytes, &owner.journal_name)?;
            let journal_identity = owner.private.group_file_identity(file)?;
            ensure!(
                record.journal == journal_identity && record.target == *owner.target.identity(),
                "Selected recovery journal belongs to a different actual object or profile"
            );
            ensure!(
                record.manifest.directory_identity == *owner.private.identity()
                    && record.manifest.directory == owner.private.path()
                    && record
                        .prepared_main
                        .as_ref()
                        .is_none_or(|main| main.directory == owner.private.path()),
                "Selected group references another actual preparation namespace"
            );
            owner.valid_bytes = valid_bytes;
            owner.journal_bytes = length;
            owner.record = Some(record);
            owner.capture_files(&context.inner)?;
            let record = owner.record.as_ref().unwrap();
            for allocation in &record.allocations {
                ensure!(
                    record
                        .replacements
                        .iter()
                        .any(|file| file.private_name == allocation.private_name),
                    "Recovery allocation has no durable actual FID; quota and selection remain retained"
                );
            }
            let current_files = owner
                .originals
                .iter()
                .chain(&owner.replacements)
                .filter_map(|file| file.fact.as_ref())
                .map(|fact| PhysicalProfileRecoveryFile {
                    role: fact.role.clone(),
                    path: if fact.private {
                        owner.private.path().join(&fact.private_name)
                    } else {
                        owner.target.path().join(&fact.live_name)
                    },
                    identity: describe_identity(&fact.identity),
                    bytes: fact.bytes,
                    sha256: fact.sha256.clone(),
                    recorded_sha256: record
                        .originals
                        .iter()
                        .chain(&record.replacements)
                        .find(|old| old.identity == fact.identity)
                        .unwrap()
                        .sha256
                        .clone(),
                })
                .collect();
            let mut preview = PhysicalProfileRecoveryPreview {
                operation_id: record.operation_id.clone(),
                journal_path: owner.private.path().join(&owner.journal_name),
                journal_identity: describe_identity(&journal_identity),
                journal_sha256,
                selection_epoch,
                phase: format!("{:?}", record.phase),
                current_files,
                approval_sha256: String::new(),
            };
            preview.approval_sha256 =
                format!("{:x}", Sha256::digest(serde_json::to_vec(&preview)?));
            owner.preview = Some(preview);
            guard.verify()?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(PhysicalProfileRecoverySelectionFailure {
                error,
                owner: Some(Box::new(owner)),
            }),
        }
    }
    fn capture_files(&mut self, context: &SnapshotContext) -> Result<()> {
        let record = self.record.as_ref().unwrap().clone();
        let known_identities = record
            .originals
            .iter()
            .chain(&record.replacements)
            .map(|fact| fact.identity.clone())
            .collect::<Vec<_>>();
        for (original, facts) in [(true, record.originals), (false, record.replacements)] {
            for fact in facts {
                let cleanup = record
                    .cleanup
                    .iter()
                    .find(|cleanup| cleanup.file.identity.as_ref() == Some(&fact.identity))
                    .cloned();
                if cleanup
                    .as_ref()
                    .is_some_and(|cleanup| cleanup.removed || cleanup.pending_zero_links())
                {
                    let file = OwnedGroupFile {
                        file: None,
                        retirement: None,
                        cleanup,
                        cleanup_acknowledged: false,
                        cold_cleanup: None,
                        role: fact.role.clone(),
                        live_name: fact.live_name.clone(),
                        private_name: fact.private_name.clone(),
                        private: fact.private,
                        fact: Some(fact),
                    };
                    if original {
                        self.originals.push(file)
                    } else {
                        self.replacements.push(file)
                    };
                    continue;
                }
                let mut found = false;
                for private in [fact.private, !fact.private] {
                    let directory = if private { &self.private } else { &self.target };
                    let name = if private {
                        &fact.private_name
                    } else {
                        &fact.live_name
                    };
                    match directory.open_group_file(name, false) {
                        Ok(file) => {
                            self.obstructions.push(file);
                            let file = self.obstructions.last().unwrap();
                            let identity = directory.group_file_identity(file)?;
                            if identity != fact.identity {
                                if known_identities.contains(&identity) {
                                    let mut inspection = self.obstructions.pop();
                                    if let Err(error) = close_owned(&mut inspection) {
                                        self.obstructions.push(inspection.unwrap());
                                        return Err(error);
                                    }
                                }
                                continue;
                            }
                            let file = self.obstructions.pop().unwrap();
                            let actual = OwnedGroupFile {
                                file: Some(file),
                                retirement: None,
                                cleanup: cleanup.clone(),
                                cleanup_acknowledged: false,
                                cold_cleanup: None,
                                fact: Some(fact.clone()),
                                role: fact.role.clone(),
                                live_name: fact.live_name.clone(),
                                private_name: fact.private_name.clone(),
                                private,
                            };
                            let files = if original {
                                &mut self.originals
                            } else {
                                &mut self.replacements
                            };
                            files.push(actual);
                            let actual = files.last_mut().unwrap();
                            let maximum = if fact.role == "main" {
                                record.request.maximum_image_bytes
                            } else {
                                64 * 1024 * 1024
                            };
                            actual.attest(directory, maximum, context)?;
                            ensure!(
                                actual.fact.as_ref().unwrap().sha256 == fact.sha256
                                    || (!original
                                        && matches!(
                                            record.phase,
                                            GroupPhase::Reopening | GroupPhase::Reopened
                                        ))
                                    || (original
                                        && !private
                                        && matches!(
                                            record.phase,
                                            GroupPhase::OriginalOpening
                                                | GroupPhase::OriginalReopened
                                        )),
                                "Recovery original/replacement body differs from its durable same-FID attestation"
                            );
                            found = true;
                            break;
                        }
                        Err(error) if not_found(&error) => {}
                        Err(error) => return Err(error),
                    }
                }
                ensure!(
                    found,
                    "Recovery group object is missing or foreign; no path-based ownership is inferred"
                );
            }
        }
        Ok(())
    }
}

pub struct PhysicalProfileRecoveryAdmission {
    guard: Option<ProfileGuard>,
    selection: Option<PhysicalProfileRecoverySelection>,
    limits: PhysicalRestoreLimits,
}
pub enum PhysicalProfileRecoveryOwner {
    Admission(Box<PhysicalProfileRecoveryAdmission>),
    Installation(Box<PhysicalProfileInstallation>),
}
pub struct PhysicalProfileRecoveryFailure {
    pub error: anyhow::Error,
    pub owner: PhysicalProfileRecoveryOwner,
}
impl std::fmt::Debug for PhysicalProfileRecoveryAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileRecoveryAdmission")
            .field("guard", &self.guard.is_some())
            .field("selection", &self.selection)
            .finish()
    }
}
impl std::fmt::Debug for PhysicalProfileRecoveryOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(owner) => owner.fmt(f),
            Self::Installation(owner) => owner.fmt(f),
        }
    }
}
impl std::fmt::Debug for PhysicalProfileRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileRecoveryFailure")
            .field("error", &self.error)
            .field("owner", &self.owner)
            .finish()
    }
}
impl std::fmt::Display for PhysicalProfileRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for PhysicalProfileRecoveryFailure {}
impl PhysicalProfileRecoveryAdmission {
    pub fn retry(
        self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileInstallation, PhysicalProfileRecoveryFailure> {
        self.recover(context)
    }
    fn recover(
        mut self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileInstallation, PhysicalProfileRecoveryFailure> {
        let preliminary = (|| -> Result<(ProfileContinuityToken, SnapshotPermit)> {
            let guard = self
                .guard
                .as_ref()
                .context("Recovery admission lost its actual guard")?;
            let selection = self
                .selection
                .as_ref()
                .context("Recovery admission lost its actual selection")?;
            guard.verify()?;
            selection.target.verify()?;
            selection.private.verify()?;
            let current_root =
                SnapshotDirectory::pin_profile(guard.directory(), selection.target.epoch())?;
            ensure!(
                current_root.identity() == selection.target.identity(),
                "Recovery guard identifies another actual profile root"
            );
            ensure!(
                selection.accepted_current_content,
                "Recovery requires explicit approval of this current actual selection"
            );
            let record = selection
                .record
                .as_ref()
                .context("Recovery has no validated group record")?;
            let continuity = guard.continuity_token()?;
            let permit = self
                .limits
                .reconstruct_retained(&record.request, current_root.identity())?;
            Ok((continuity, permit))
        })();
        let (continuity, permit) = match preliminary {
            Ok(value) => value,
            Err(error) => {
                return Err(PhysicalProfileRecoveryFailure {
                    error,
                    owner: PhysicalProfileRecoveryOwner::Admission(Box::new(self)),
                });
            }
        };
        let mut selection = self.selection.take().unwrap();
        let record = selection.record.take().unwrap();
        let manifest_identity = record.manifest.identity.as_ref().unwrap();
        let locator = PhysicalRestoreLocator {
            operation_id: record.operation_id.clone(),
            journal_path: record
                .manifest
                .directory
                .join(record.manifest.temporary_name.as_ref().unwrap()),
            journal_device: manifest_identity.device,
            journal_object: manifest_identity.object.clone(),
            target_device: record.target.device,
            target_object: record.target.object.clone(),
            request_sha256: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&record.request).unwrap())
            ),
        };
        let prepared = PreparedPhysicalRestore {
            request: record.request.clone(),
            continuity,
            target: selection.target.clone(),
            private: selection.private.clone(),
            source: None,
            manifest: None,
            sqlite: None,
            registry: None,
            main: None,
            auxiliary: Vec::new(),
            permit,
            limits: self.limits.clone(),
            locator: Some(locator),
            view: record.image.clone(),
            authority: None,
        };
        let mut owner = PhysicalProfileInstallation {
            prepared,
            guard: self.guard.take(),
            repository: None,
            opening: None,
            target: selection.target,
            private: selection.private,
            originals: selection.originals,
            replacements: selection.replacements,
            allocations: record.allocations.clone(),
            obstructions: selection.obstructions,
            journal: selection.journal,
            journal_name: selection.journal_name,
            journal_bytes: selection.valid_bytes,
            journal_dirty: selection.valid_bytes != selection.journal_bytes,
            sequence: record.sequence,
            phase: record.phase,
            journal_retirement: None,
            journal_cleanup: None,
            terminal_installed: record.terminal_installed,
            last_record: Some(record.clone()),
        };
        let result = (|| -> Result<()> {
            owner.check(&context.inner)?;
            let probe = ActualFile::inspect_recovery(
                &owner.private,
                &record.manifest,
                crate::repository::snapshot_native::MAX_MANIFEST_BYTES,
                &context.inner,
            )?;
            let manifest =
                match ActualFile::reopen_recovery(probe, &context.inner, &owner.prepared.permit) {
                    Ok(file) => match DurableManifest::from_recovered(file, &context.inner) {
                        Ok(manifest) => manifest,
                        Err(mut failure) => {
                            if let Some(manifest) = failure.manifest.take() {
                                owner.prepared.manifest = Some(Arc::new(Mutex::new(*manifest)));
                            }
                            if let Some(file) = failure.file.take() {
                                owner.prepared.auxiliary.push(*file);
                            }
                            anyhow::bail!("{}", failure.message);
                        }
                    },
                    Err(mut failure) => {
                        if let Some(file) = failure.file.take() {
                            owner.prepared.auxiliary.push(*file);
                        }
                        anyhow::bail!("{}", failure.message);
                    }
                };
            owner.prepared.manifest = Some(Arc::new(Mutex::new(manifest)));
            if let Some(main) = &record.prepared_main {
                let probe = ActualFile::inspect_recovery(
                    &owner.private,
                    main,
                    record.request.maximum_image_bytes,
                    &context.inner,
                )?;
                let file = match ActualFile::reopen_recovery(
                    probe,
                    &context.inner,
                    &owner.prepared.permit,
                ) {
                    Ok(file) => file,
                    Err(mut failure) => {
                        if let Some(file) = failure.file.take() {
                            owner.prepared.auxiliary.push(*file);
                        }
                        anyhow::bail!("{}", failure.message);
                    }
                };
                owner.prepared.main = Some(file);
                owner.prepared.main.as_mut().unwrap().restore_sealed(
                    &record
                        .image
                        .as_ref()
                        .context("Recovery image proof is absent")?
                        .image_sha256,
                    &context.inner,
                )?;
            }
            owner.checkpoint(&context.inner)?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(PhysicalProfileRecoveryFailure {
                error,
                owner: PhysicalProfileRecoveryOwner::Installation(Box::new(owner)),
            }),
        }
    }
}
impl PhysicalProfileInstallation {
    pub fn recover_selected(
        guard: ProfileGuard,
        selection: PhysicalProfileRecoverySelection,
        limits: &PhysicalRestoreLimits,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<Self, PhysicalProfileRecoveryFailure> {
        PhysicalProfileRecoveryAdmission {
            guard: Some(guard),
            selection: Some(selection),
            limits: limits.clone(),
        }
        .recover(context)
    }
    pub fn resume_installation(
        mut self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<Self, PhysicalProfileInstallFailure> {
        let result = (|| -> Result<()> {
            self.check(&context.inner)?;
            ensure!(
                matches!(
                    self.phase,
                    GroupPhase::Captured
                        | GroupPhase::Staging
                        | GroupPhase::Installing
                        | GroupPhase::Installed
                ),
                "Recovery cannot resume an incomplete capture or an already opened profile"
            );
            ensure!(
                self.originals.iter().any(|file| file.role == "main")
                    && self.replacements.iter().any(|file| file.role == "main"),
                "Recovery lacks a sealed complete main group"
            );
            if self.phase == GroupPhase::Installed {
                for (original, files) in
                    [(true, &mut self.originals), (false, &mut self.replacements)]
                {
                    for file in files {
                        ensure!(
                            file.private == original && file.cleanup.is_none(),
                            "Already installed recovery group has an unproven location or retirement"
                        );
                        let expected = file
                            .fact
                            .as_ref()
                            .context("Installed recovery group has no approved actual proof")?
                            .clone();
                        file.attest(
                            if original {
                                &self.private
                            } else {
                                &self.target
                            },
                            if file.role == "main" {
                                self.prepared.request.maximum_image_bytes
                            } else {
                                64 * 1024 * 1024
                            },
                            &context.inner,
                        )?;
                        let actual = file.fact.as_ref().unwrap();
                        let unchanged = actual.identity == expected.identity
                            && actual.bytes == expected.bytes
                            && actual.sha256 == expected.sha256;
                        file.fact = Some(expected);
                        ensure!(
                            unchanged,
                            "Already installed actual group changed after fresh approval"
                        );
                    }
                }
                return self.check(&context.inner);
            }
            if self.phase == GroupPhase::Captured {
                self.phase = GroupPhase::Staging;
                self.checkpoint(&context.inner)?;
            }
            if self.phase == GroupPhase::Staging {
                for index in 0..self.originals.len() {
                    if !self.originals[index].private {
                        self.originals[index].move_to(&self.private, true)?;
                        self.checkpoint(&context.inner)?;
                    }
                }
                self.phase = GroupPhase::Installing;
                self.checkpoint(&context.inner)?;
            }
            ensure!(
                self.originals.iter().all(|file| file.private),
                "Installing recovery group still contains a live original"
            );
            for index in 0..self.replacements.len() {
                if self.replacements[index].private {
                    self.replacements[index].move_to(&self.target, false)?;
                    self.checkpoint(&context.inner)?;
                }
            }
            self.phase = GroupPhase::Installed;
            self.checkpoint(&context.inner)
        })();
        match result {
            Ok(()) => Ok(self),
            Err(error) => Err(self.fail(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn group_journal_leaf_cannot_choose_other_profile_or_role_names() {
        assert!(journal_nonce("native-library.sqlite3").is_err());
        assert!(journal_nonce(".lx-profile-../main.journal").is_err());
        assert!(journal_nonce(".lx-profile-00000000000000000000000000000000.journal").is_ok());
    }
    #[test]
    #[ignore = "Isolated process-crash helper, invoked by the actual crash matrix"]
    fn crash_child() {
        let folder = PathBuf::from(std::env::var_os("LX_PROFILE_GROUP_CHILD_DIR").unwrap());
        let point = std::env::var("LX_PROFILE_GROUP_CHILD_POINT").unwrap();
        super::super::CRASH_POINT.with(|selected| *selected.borrow_mut() = Some(point));
        let (source, _) = super::super::super::tests::image(&folder);
        let target = Arc::new(Repository::open(folder.join("target")).unwrap());
        target.record_profile_change(&"f".repeat(64)).unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = super::super::super::tests::context();
        let prepared = target
            .prepare_physical_restore(
                super::super::super::tests::request(&target),
                PhysicalRestoreSelection::open_selected(source).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        let guard = target.close_into_guard().unwrap();
        let _owner = prepared.install_files(guard, &context).unwrap();
        panic!("Actual requested crash point was not reached");
    }
    fn run_crash(point: &str) -> tempfile::TempDir {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "repository::profile_install::group::cold::tests::crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("LX_PROFILE_GROUP_CHILD_DIR", folder.path())
            .env("LX_PROFILE_GROUP_CHILD_POINT", point)
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(77),
            "Actual child did not terminate at {point}"
        );
        folder
    }
    fn select(folder: &Path) -> (ProfileGuard, PhysicalProfileRecoverySelection) {
        let guard = ProfileGuard::acquire(folder.join("target")).unwrap();
        let staging = folder.join("target").join(".lx-snapshot-staging");
        let journal = std::fs::read_dir(&staging)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".lx-profile-")
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "journal")
            })
            .unwrap();
        let context = super::super::super::tests::context();
        let mut selection =
            PhysicalProfileRecoverySelection::open_selected(&guard, &journal, 99, &context)
                .unwrap();
        let fingerprint = selection.preview().unwrap().approval_sha256.clone();
        selection.approve_current_content(&fingerprint).unwrap();
        (guard, selection)
    }
    #[test]
    fn actual_process_crash_matrix_recovers_rename_gaps_with_fresh_same_object_selection() {
        for point in [
            "captured",
            "original-main-moved",
            "replacement-main-moved",
            "installed",
        ] {
            let folder = run_crash(point);
            let (guard, selection) = select(folder.path());
            let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
            let context = super::super::super::tests::context();
            let recovered =
                PhysicalProfileInstallation::recover_selected(guard, selection, &limits, &context)
                    .unwrap();
            assert!(ProfileGuard::acquire(folder.path().join("target")).is_err());
            let terminal = recovered
                .resume_installation(&context)
                .unwrap()
                .reopen(&context)
                .unwrap()
                .commit(&context)
                .unwrap();
            terminal.into_repository().integrity_check().unwrap();
            assert_eq!(
                limits.stats(),
                (0, 0),
                "Crash point {point} leaked actual retained quota"
            );
            assert_eq!(
                std::fs::read_dir(folder.path().join("target").join(".lx-snapshot-staging"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }
    #[test]
    fn cold_rollback_keeps_original_revision_and_retires_new_group_after_actual_child_exit() {
        let folder = run_crash("replacement-main-moved");
        let (guard, selection) = select(folder.path());
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = super::super::super::tests::context();
        let recovered =
            PhysicalProfileInstallation::recover_selected(guard, selection, &limits, &context)
                .unwrap();
        let terminal = recovered
            .rollback(&context)
            .unwrap()
            .reopen(&context)
            .unwrap()
            .finish_rollback(&context)
            .unwrap();
        assert_eq!(terminal.into_repository().revision().unwrap(), 1);
        assert_eq!(limits.stats(), (0, 0));
    }
}
