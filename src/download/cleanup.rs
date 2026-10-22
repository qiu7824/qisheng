//! Durable disposal of an acknowledged transfer original, with native object
//! proofs kept separate from the journal's recovery metadata.

use super::{
    DestinationGrant, DownloadCancel, FileIdentity, OwnedPartRecord, VerifiedOwnedAudio,
    artifact::{OwnedArtifactAllocation, OwnedArtifactKind},
    authority::{
        CleanupAbsentOriginal, CleanupDirectory, CleanupDispositionFacts, CleanupFinalGuard,
        CleanupJournalFile, CleanupNativeAbsence, CleanupOriginal, CleanupOriginalLookup,
        CleanupPreparedFacts, NativeDispositionWitness, NativePreparedAttestation,
        NativeVolumeCalibration,
    },
    task_model::DownloadArtifactProof,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
};

const MAGIC: &[u8; 8] = b"LXCLEAN1";
const PREFIX: &str = ".lx-cleanup-";
const SUFFIX: &str = ".journal";
const MAX_JOURNALS: usize = 400;
const MAX_PROFILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 128 * 1024;
const MAX_RECORDS: usize = 4;
const RECORD_HEADER: usize = 4 + 32;
const MAX_JOURNAL_BYTES: usize = MAGIC.len() + MAX_RECORDS * (RECORD_HEADER + MAX_RECORD_BYTES);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupIntent {
    pub allocation: OwnedArtifactAllocation,
    pub raw_bytes: u64,
    pub raw_sha256: String,
    pub final_output: DownloadArtifactProof,
    pub task_revision: u64,
}

impl CleanupIntent {
    pub fn validate(&self) -> Result<()> {
        self.allocation.validate()?;
        self.final_output.validate()?;
        ensure!(
            self.allocation.kind == OwnedArtifactKind::Transfer && self.task_revision > 0,
            "Cleanup intent requires an original transfer and durable task revision"
        );
        ensure!(
            self.raw_bytes > 0
                && self.raw_bytes <= self.allocation.reserved_max_bytes
                && valid_sha256(&self.raw_sha256),
            "Cleanup original digest or byte bound is invalid"
        );
        ensure!(
            self.final_output.identity != self.allocation.record.identity
                && self.final_output.basename == self.allocation.record.final_basename,
            "Cleanup final proof must name a different published object in its planned role"
        );
        ensure!(
            self.allocation.record.identity.object.len() == 32
                && self.final_output.identity.object.len() == 32
                && self.allocation.record.identity.device
                    == self.allocation.selected_root_identity.device
                && self.final_output.identity.device
                    == self.allocation.selected_root_identity.device,
            "Cleanup intent requires same-volume native 128-bit object identities"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPhase {
    Prepared,
    Removed,
    Acknowledged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupBoundary {
    Prepare,
    Remove,
    Acknowledge,
}

/// Implemented by the exclusive profile coordinator. Validation must check its
/// current repository output, original allocation and destination binding.
pub trait CleanupCoordinator {
    fn validate(&self, intent: &CleanupIntent, boundary: CleanupBoundary) -> Result<()>;
    fn acknowledge(&self, receipt: &NativeRemovedReceipt) -> Result<()>;
}

/// Cannot be constructed or deserialized by a caller. It is sealed only after
/// native close and a successful selected-volume lookup proving the old ID absent.
pub struct NativeRemovedReceipt {
    intent: CleanupIntent,
    record: OwnedPartRecord,
    volume_guid: String,
}

impl NativeRemovedReceipt {
    pub fn intent(&self) -> &CleanupIntent {
        &self.intent
    }
    pub fn removed_record(&self) -> &OwnedPartRecord {
        &self.record
    }
    pub fn volume_guid(&self) -> &str {
        &self.volume_guid
    }

    fn seal(intent: &CleanupIntent, absence: CleanupNativeAbsence) -> Result<Self> {
        ensure!(
            absence.record() == &intent.allocation.record,
            "Native cleanup absence differs from its original allocation"
        );
        Ok(Self {
            intent: intent.clone(),
            record: absence.record().clone(),
            volume_guid: absence.volume_guid().into(),
        })
    }

    fn stored(&self) -> StoredReceipt {
        StoredReceipt {
            record: self.record.clone(),
            volume_guid: self.volume_guid.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    record: OwnedPartRecord,
    volume_guid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    version: u32,
    sequence: usize,
    previous_sha256: Option<String>,
    profile_identity: FileIdentity,
    journal_identity: FileIdentity,
    intent: CleanupIntent,
    phase: CleanupPhase,
    receipt: Option<StoredReceipt>,
    #[serde(default)]
    prepared_evidence: Option<CleanupPreparedFacts>,
    #[serde(default)]
    disposition_evidence: Option<CleanupDispositionFacts>,
}

#[derive(Clone, Debug)]
pub struct CleanupJournalSnapshot {
    pub intent: CleanupIntent,
    pub phase: CleanupPhase,
}

struct JournalState {
    directory: CleanupDirectory,
    gate: Mutex<()>,
}
static PROFILES: OnceLock<Mutex<HashMap<String, Weak<JournalState>>>> = OnceLock::new();

#[derive(Clone)]
pub struct CleanupJournal {
    state: Arc<JournalState>,
}

impl CleanupJournal {
    /// Opens the existing application profile with native ancestor pins. This
    /// journal never creates a profile or follows a redirected profile directory.
    pub fn open(profile: &Path) -> Result<Self> {
        let directory = CleanupDirectory::open_profile(profile)?;
        let key = format!(
            "{}:{}",
            directory.identity().device,
            directory.identity().object
        );
        let mut profiles = PROFILES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup profile registry was poisoned"))?;
        profiles.retain(|_, state| state.strong_count() != 0);
        if let Some(state) = profiles.get(&key).and_then(Weak::upgrade) {
            state.directory.verify()?;
            ensure!(
                crate::domain::music::normalize_local_path(state.directory.path())
                    == crate::domain::music::normalize_local_path(directory.path()),
                "Cleanup profile object has a different pinned namespace"
            );
            return Ok(Self { state });
        }
        let state = Arc::new(JournalState {
            directory,
            gate: Mutex::new(()),
        });
        profiles.insert(key, Arc::downgrade(&state));
        Ok(Self { state })
    }

    /// Returns bounded recovery facts; an explicit current destination grant and
    /// coordinator validation are still required before any effect.
    pub fn load(&self, nonce: &str) -> Result<Option<CleanupJournalSnapshot>> {
        let name = journal_name(nonce)?;
        let _gate = self
            .state
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup journal lock was poisoned"))?;
        self.audit(None, 0)?;
        let mut file = match self.state.directory.open_journal(&name, false, false) {
            Ok(file) => file,
            Err(error) if not_found(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let parsed = parse_records(
            &file.read_bounded(MAX_JOURNAL_BYTES)?,
            self.state.directory.identity(),
            file.identity(),
        )?;
        let record = parsed
            .last
            .context("Cleanup journal has no complete prepared record")?;
        ensure!(
            record
                .intent
                .allocation
                .record
                .nonce
                .eq_ignore_ascii_case(nonce),
            "Cleanup journal nonce differs from its filename"
        );
        Ok(Some(CleanupJournalSnapshot {
            intent: record.intent,
            phase: record.phase,
        }))
    }

    pub fn snapshots(&self) -> Result<Vec<CleanupJournalSnapshot>> {
        let names = {
            let _gate = self
                .state
                .gate
                .lock()
                .map_err(|_| anyhow::anyhow!("Cleanup journal lock was poisoned"))?;
            self.audit(None, 0)?;
            self.state.directory.entries()?
        };
        let mut snapshots = Vec::new();
        for name in names {
            if let Some(nonce) = journal_nonce(&name)
                && let Some(snapshot) = self.load(&nonce)?
            {
                snapshots.push(snapshot);
            }
        }
        ensure!(
            snapshots.len() <= MAX_JOURNALS,
            "Cleanup recovery page exceeds its journal bound"
        );
        Ok(snapshots)
    }

    fn audit(&self, replacing: Option<(&str, u64)>, added: u64) -> Result<()> {
        self.state.directory.verify()?;
        let mut count = 0usize;
        let mut bytes = 0u64;
        for name in self.state.directory.entries()? {
            if journal_nonce(&name).is_none() {
                continue;
            }
            count += 1;
            let length = self.state.directory.journal_length(&name)?;
            ensure!(
                length <= MAX_JOURNAL_BYTES as u64,
                "Cleanup journal exceeds its physical bound"
            );
            let length = if let Some((target, replacement)) = replacing {
                if name.eq_ignore_ascii_case(target) {
                    replacement
                } else {
                    length
                }
            } else {
                length
            };
            bytes = bytes
                .checked_add(length)
                .context("Cleanup profile byte count overflow")?;
        }
        bytes = bytes
            .checked_add(added)
            .context("Cleanup profile byte count overflow")?;
        ensure!(
            count <= MAX_JOURNALS && bytes <= MAX_PROFILE_BYTES,
            "Cleanup profile journal quota exhausted"
        );
        Ok(())
    }

    fn bind(&self, intent: &CleanupIntent) -> Result<JournalEntry> {
        let name = journal_name(&intent.allocation.record.nonce)?;
        let _gate = self
            .state
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup journal lock was poisoned"))?;
        self.audit(None, 0)?;
        let (file, created_here) = match self.state.directory.open_journal(&name, false, true) {
            Ok(file) => (file, false),
            Err(error) if not_found(&error) => {
                let count = self
                    .state
                    .directory
                    .entries()?
                    .into_iter()
                    .filter(|name| journal_nonce(name).is_some())
                    .count();
                ensure!(count < MAX_JOURNALS, "Cleanup journal count exhausted");
                (self.state.directory.open_journal(&name, true, true)?, true)
            }
            Err(error) => return Err(error),
        };
        let mut entry = JournalEntry {
            file,
            name,
            created_here,
            ever_prepared: false,
            parsed: ParsedRecords::empty(),
        };
        if !created_here {
            entry.refresh(self, intent)?;
        }
        Ok(entry)
    }
}

struct ParsedRecords {
    last: Option<StoredRecord>,
    records: usize,
    valid_length: usize,
    last_sha256: Option<String>,
}
impl ParsedRecords {
    fn empty() -> Self {
        Self {
            last: None,
            records: 0,
            valid_length: 0,
            last_sha256: None,
        }
    }
}

struct JournalEntry {
    file: CleanupJournalFile,
    name: String,
    created_here: bool,
    ever_prepared: bool,
    parsed: ParsedRecords,
}

impl JournalEntry {
    fn phase(&self) -> Option<CleanupPhase> {
        self.parsed.last.as_ref().map(|record| record.phase)
    }

    fn refresh(&mut self, journal: &CleanupJournal, intent: &CleanupIntent) -> Result<()> {
        let bytes = self.file.read_bounded(MAX_JOURNAL_BYTES)?;
        let parsed = match parse_records(
            &bytes,
            journal.state.directory.identity(),
            self.file.identity(),
        ) {
            Ok(parsed) => parsed,
            Err(_) if self.created_here && !self.ever_prepared => {
                self.file.truncate(0)?;
                ParsedRecords::empty()
            }
            Err(error) => return Err(error),
        };
        if let Some(record) = &parsed.last {
            ensure!(
                &record.intent == intent,
                "Cleanup journal belongs to a different original or durable output"
            );
            self.ever_prepared = true;
        } else {
            ensure!(
                self.created_here && !self.ever_prepared,
                "Foreign or incomplete cleanup journal cannot be overwritten"
            );
        }
        self.parsed = parsed;
        self.file.flush()?;
        Ok(())
    }

    fn append(
        &mut self,
        journal: &CleanupJournal,
        intent: &CleanupIntent,
        phase: CleanupPhase,
        receipt: Option<StoredReceipt>,
        attestation: &NativePreparedAttestation,
        disposition: Option<&NativeDispositionWitness>,
    ) -> Result<()> {
        let _gate = journal
            .state
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup journal lock was poisoned"))?;
        self.refresh(journal, intent)?;
        let disposition_evidence =
            disposition
                .map(|witness| witness.facts().clone())
                .or_else(|| {
                    self.parsed
                        .last
                        .as_ref()
                        .and_then(|record| record.disposition_evidence.clone())
                });
        if self.phase() == Some(phase)
            && self.parsed.last.as_ref().is_some_and(|record| {
                record.version == 2 && record.disposition_evidence == disposition_evidence
            })
        {
            return Ok(());
        }
        ensure!(
            self.parsed.records < MAX_RECORDS,
            "Cleanup journal effective-record bound exhausted"
        );
        let record = StoredRecord {
            version: 2,
            sequence: self.parsed.records + 1,
            previous_sha256: self.parsed.last_sha256.clone(),
            profile_identity: journal.state.directory.identity().clone(),
            journal_identity: self.file.identity().clone(),
            intent: intent.clone(),
            phase,
            receipt,
            prepared_evidence: Some(attestation.facts().clone()),
            disposition_evidence,
        };
        validate_record(&record, self.parsed.last.as_ref())?;
        let payload = serde_json::to_vec(&record)?;
        ensure!(
            payload.len() <= MAX_RECORD_BYTES,
            "Cleanup journal record exceeds its bound"
        );
        let mut bytes = Vec::with_capacity(RECORD_HEADER + payload.len() + MAGIC.len());
        if self.parsed.records == 0 {
            bytes.extend_from_slice(MAGIC);
        }
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&Sha256::digest(&payload));
        bytes.extend_from_slice(&payload);
        let target_length = self
            .parsed
            .valid_length
            .checked_add(bytes.len())
            .context("Cleanup journal length overflow")?;
        ensure!(
            target_length <= MAX_JOURNAL_BYTES,
            "Cleanup journal physical-record bound exhausted"
        );
        journal.audit(Some((&self.name, target_length as u64)), 0)?;
        if self.file.length()? != self.parsed.valid_length as u64 {
            self.file.truncate(self.parsed.valid_length as u64)?;
        }
        self.file.append(&bytes)?;
        self.refresh(journal, intent)?;
        Ok(())
    }

    fn recorded_attestation(&self, journal: &CleanupJournal) -> Result<NativePreparedAttestation> {
        let record = self
            .parsed
            .last
            .as_ref()
            .context("Cleanup journal has no complete native preparation")?;
        ensure!(
            record.version == 2,
            "Legacy cleanup metadata cannot attest an absent original ID"
        );
        let evidence = record
            .prepared_evidence
            .as_ref()
            .context("Cleanup record has no native preparation evidence")?;
        self.file.recorded_prepare(
            evidence,
            journal.state.directory.identity(),
            self.file.identity(),
        )
    }

    fn upgrade_legacy(
        &mut self,
        journal: &CleanupJournal,
        intent: &CleanupIntent,
        attestation: &NativePreparedAttestation,
    ) -> Result<()> {
        let _gate = journal
            .state
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup journal lock was poisoned"))?;
        self.refresh(journal, intent)?;
        if self
            .parsed
            .last
            .as_ref()
            .is_some_and(|record| record.version == 1)
        {
            ensure!(
                self.phase() == Some(CleanupPhase::Prepared) && attestation.is_live(),
                "Legacy cleanup journal requires actual same-file native verification before upgrade"
            );
            self.file.truncate(0)?;
            self.parsed = ParsedRecords::empty();
            self.created_here = true;
            self.ever_prepared = false;
        }
        Ok(())
    }
}

fn parse_records(
    bytes: &[u8],
    profile: &FileIdentity,
    journal: &FileIdentity,
) -> Result<ParsedRecords> {
    ensure!(
        bytes.len() >= MAGIC.len() && &bytes[..MAGIC.len()] == MAGIC,
        "Unknown cleanup journal format"
    );
    let mut parsed = ParsedRecords::empty();
    let mut offset = MAGIC.len();
    while offset < bytes.len() {
        if bytes.len() - offset < RECORD_HEADER {
            break;
        }
        let length = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("Complete journal length"),
        ) as usize;
        ensure!(
            length > 0 && length <= MAX_RECORD_BYTES,
            "Cleanup journal declared record bound exceeded"
        );
        let end = offset
            .checked_add(RECORD_HEADER + length)
            .context("Cleanup journal length overflow")?;
        if end > bytes.len() {
            break;
        }
        ensure!(
            parsed.records < MAX_RECORDS,
            "Cleanup journal has too many effective records"
        );
        let payload = &bytes[offset + RECORD_HEADER..end];
        let digest = Sha256::digest(payload);
        ensure!(
            digest[..] == bytes[offset + 4..offset + RECORD_HEADER],
            "Cleanup journal checksum differs"
        );
        let record: StoredRecord = serde_json::from_slice(payload)?;
        record.intent.validate()?;
        ensure!(
            &record.profile_identity == profile && &record.journal_identity == journal,
            "Cleanup journal was copied or belongs to another profile object"
        );
        ensure!(
            record.sequence == parsed.records + 1 && record.previous_sha256 == parsed.last_sha256,
            "Cleanup journal hash chain or sequence differs"
        );
        validate_record(&record, parsed.last.as_ref())?;
        parsed.last_sha256 = Some(format!("{digest:x}"));
        parsed.last = Some(record);
        parsed.records += 1;
        parsed.valid_length = end;
        offset = end;
    }
    Ok(parsed)
}

fn validate_record(record: &StoredRecord, previous: Option<&StoredRecord>) -> Result<()> {
    ensure!(
        matches!(record.version, 1 | 2),
        "Unsupported cleanup journal version"
    );
    record.intent.validate()?;
    if record.version == 2 {
        let evidence = record
            .prepared_evidence
            .as_ref()
            .context("Cleanup native preparation evidence missing")?;
        ensure!(
            evidence.raw_record == record.intent.allocation.record
                && evidence.raw_bytes == record.intent.raw_bytes
                && evidence.raw_sha256 == record.intent.raw_sha256
                && evidence.final_output == record.intent.final_output
                && evidence.filesystem == "NTFS"
                && valid_volume_guid(&evidence.volume_guid),
            "Cleanup native preparation evidence differs from its exact original or final proof"
        );
        if let Some(disposition) = &record.disposition_evidence {
            ensure!(
                disposition.record == evidence.raw_record
                    && disposition.volume_guid == evidence.volume_guid
                    && disposition.delete_pending
                    && disposition.number_of_links == 0,
                "Cleanup disposition record has no matching native zero-link witness"
            );
        }
    } else {
        ensure!(
            record.prepared_evidence.is_none() && record.disposition_evidence.is_none(),
            "Legacy journal cannot contain native attestations"
        );
    }
    match previous {
        None => ensure!(
            record.phase == CleanupPhase::Prepared
                && record.sequence == 1
                && record.previous_sha256.is_none(),
            "Cleanup journal must begin with prepared intent"
        ),
        Some(previous) => {
            ensure!(
                record.intent == previous.intent
                    && record.profile_identity == previous.profile_identity
                    && record.journal_identity == previous.journal_identity,
                "Cleanup journal immutable identity changed"
            );
            ensure!(
                record.version == previous.version
                    && record.prepared_evidence == previous.prepared_evidence,
                "Cleanup journal native preparation identity changed"
            );
            if previous.disposition_evidence.is_some() {
                ensure!(
                    record.disposition_evidence == previous.disposition_evidence,
                    "Cleanup journal native disposition witness changed"
                );
            }
            let next = match previous.phase {
                CleanupPhase::Prepared => CleanupPhase::Removed,
                CleanupPhase::Removed => CleanupPhase::Acknowledged,
                CleanupPhase::Acknowledged => CleanupPhase::Acknowledged,
            };
            ensure!(
                record.phase == previous.phase || record.phase == next,
                "Cleanup journal phase moved backward or skipped an effect"
            );
            if previous.receipt.is_some() {
                ensure!(
                    record.receipt == previous.receipt,
                    "Cleanup journal native removal receipt changed"
                );
            }
        }
    }
    match record.phase {
        CleanupPhase::Prepared => ensure!(
            record.receipt.is_none(),
            "Prepared cleanup journal cannot claim native removal"
        ),
        CleanupPhase::Removed | CleanupPhase::Acknowledged => {
            let receipt = record
                .receipt
                .as_ref()
                .context("Cleanup journal phase has no native removal receipt")?;
            ensure!(
                receipt.record == record.intent.allocation.record
                    && valid_volume_guid(&receipt.volume_guid),
                "Cleanup journal removal receipt differs from its original native identity"
            );
        }
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn valid_volume_guid(value: &str) -> bool {
    value
        .strip_prefix("\\\\?\\volume{")
        .and_then(|value| value.strip_suffix("}\\"))
        .is_some_and(|guid| {
            guid.len() == 36
                && guid
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        })
}
fn not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}
fn journal_name(nonce: &str) -> Result<String> {
    ensure!(
        nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid cleanup journal nonce"
    );
    Ok(format!("{PREFIX}{nonce}{SUFFIX}"))
}
fn journal_nonce(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    let nonce = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    (nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| nonce.into())
}

/// Keeps every acquired scope, raw handle and final-file fence on an error.
/// The coordinator must retain this owner until retry or actual worker retirement.
pub struct CleanupOwner {
    journal: CleanupJournal,
    intent: CleanupIntent,
    selected: Option<DestinationGrant>,
    scope: Option<DestinationGrant>,
    original: Option<CleanupOriginal>,
    final_guard: Option<CleanupFinalGuard>,
    entry: Option<JournalEntry>,
    receipt: Option<NativeRemovedReceipt>,
    prepared: Option<NativePreparedAttestation>,
    disposition: Option<NativeDispositionWitness>,
    cancel: DownloadCancel,
    verification: Option<CleanupVerification>,
    final_authorization: Option<crate::library::authority::FileAuthorization>,
    retirement_absent: Option<CleanupAbsentOriginal>,
}
struct VerificationState {
    original: Option<CleanupOriginal>,
    absent: Option<CleanupAbsentOriginal>,
    outcome: Option<Result<()>>,
    final_guard: Option<CleanupFinalGuard>,
}
struct CleanupVerification {
    worker: crate::audio::worker_owner::TrackedWorker,
    state: Arc<Mutex<VerificationState>>,
}

impl CleanupOwner {
    pub fn from_live(
        journal: CleanupJournal,
        intent: CleanupIntent,
        audio: VerifiedOwnedAudio,
    ) -> Self {
        Self::from_live_with_cancel(journal, intent, audio, DownloadCancel::default())
    }
    pub fn from_live_with_cancel(
        journal: CleanupJournal,
        intent: CleanupIntent,
        audio: VerifiedOwnedAudio,
        cancel: DownloadCancel,
    ) -> Self {
        let original = CleanupOriginal::from_live(audio);
        let scope = original.grant().clone();
        Self {
            journal,
            intent,
            selected: None,
            scope: Some(scope),
            original: Some(original),
            final_guard: None,
            entry: None,
            receipt: None,
            prepared: None,
            disposition: None,
            cancel,
            verification: None,
            final_authorization: None,
            retirement_absent: None,
        }
    }

    pub fn recover(
        journal: CleanupJournal,
        intent: CleanupIntent,
        current_destination: DestinationGrant,
    ) -> Self {
        Self::recover_with_cancel(
            journal,
            intent,
            current_destination,
            DownloadCancel::default(),
        )
    }
    pub fn recover_with_cancel(
        journal: CleanupJournal,
        intent: CleanupIntent,
        current_destination: DestinationGrant,
        cancel: DownloadCancel,
    ) -> Self {
        Self {
            journal,
            intent,
            selected: Some(current_destination),
            scope: None,
            original: None,
            final_guard: None,
            entry: None,
            receipt: None,
            prepared: None,
            disposition: None,
            cancel,
            verification: None,
            final_authorization: None,
            retirement_absent: None,
        }
    }

    pub fn intent(&self) -> &CleanupIntent {
        &self.intent
    }
    pub fn phase(&self) -> Option<CleanupPhase> {
        self.entry.as_ref().and_then(JournalEntry::phase)
    }
    pub fn cancellation(&self) -> DownloadCancel {
        self.cancel.clone()
    }
    pub fn physical_removal_proven(&self) -> bool {
        self.receipt.is_some()
    }
    pub(crate) fn set_final_authorization(
        &mut self,
        authorization: crate::library::authority::FileAuthorization,
    ) {
        self.final_authorization = Some(authorization);
    }
    pub(crate) fn original_for_recovery_after_join(
        &mut self,
    ) -> Result<Option<crate::download::DownloadProducerObject>> {
        if let Some(verification) = &self.verification {
            ensure!(
                verification.worker.wait(std::time::Duration::ZERO),
                "Actual cleanup verification loan is still running during recovery retirement"
            );
            let mut owned = verification
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.original = owned.original.take();
            self.final_guard = owned.final_guard.take();
            self.retirement_absent = owned.absent.take();
            drop(owned);
            self.verification = None;
        }
        Ok(self
            .original
            .as_ref()
            .and_then(CleanupOriginal::producer_object)
            .or_else(|| {
                self.retirement_absent
                    .as_ref()
                    .map(CleanupAbsentOriginal::producer_object)
            }))
    }
    pub(crate) fn return_recovery_borrows(&mut self) {
        self.original.take();
        self.retirement_absent.take();
        self.final_guard.take();
        self.final_authorization.take();
        self.entry.take();
        self.scope.take();
        self.selected.take();
    }

    fn verify_original_loan(
        &mut self,
        scope: &DestinationGrant,
        calibration: Option<&NativeVolumeCalibration>,
    ) -> Result<()> {
        if self.verification.is_none() {
            use crate::audio::worker_owner::{WorkerKind, WorkerPermit};
            let permit = WorkerPermit::reserve(WorkerKind::Download)?;
            let ticket = permit.tracker();
            let state = Arc::new(Mutex::new(VerificationState {
                original: self.original.take(),
                absent: None,
                outcome: None,
                final_guard: self.final_guard.take(),
            }));
            let worker_state = state.clone();
            let grant = scope.clone();
            let record = self.intent.allocation.record.clone();
            let prepared = self.prepared.clone();
            let calibration = calibration.cloned();
            let bytes = self.intent.raw_bytes;
            let sha256 = self.intent.raw_sha256.clone();
            let cancel = self.cancel.clone();
            let final_authorization = self.final_authorization.clone();
            let final_proof = self.intent.final_output.clone();
            let (start, ready) = std::sync::mpsc::sync_channel(1);
            let spawned = std::thread::Builder::new()
                .name("download-cleanup-native-read".into())
                .spawn(move || {
                    if ready.recv().is_err() {
                        return;
                    }
                    let mut owned = worker_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let outcome =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                            let _scope = crate::download::ProducerWorkScope::enter(ticket)?;
                            if owned.final_guard.is_none() {
                                owned.final_guard =
                                    Some(if let Some(authorization) = final_authorization {
                                        grant.bind_cleanup_final_authorized(
                                            &final_proof,
                                            authorization,
                                            &cancel,
                                        ).context("Cleanup prepare: existing actual authorized final reader")?
                                    } else {
                                        grant.bind_cleanup_final(&final_proof, &cancel)
                                            .context("Cleanup prepare: native final readonly name open (no installed receiver)")?
                                    });
                            }
                            if owned.original.is_none() {
                                match CleanupOriginal::reopen(
                                    &grant,
                                    &record,
                                    prepared.as_ref(),
                                    calibration.as_ref(),
                                ).context("Cleanup prepare: retained original lookup or current native reopen")? {
                                    CleanupOriginalLookup::Present(original) => {
                                        owned.original = Some(*original)
                                    }
                                    CleanupOriginalLookup::Absent(absent) => {
                                        owned.absent = Some(absent)
                                    }
                                }
                            }
                            if let Some(original) = &mut owned.original
                                && !original.is_closed()
                                && !original.disposition_requested()
                            {
                                original.verify_raw(bytes, &sha256, &cancel)
                                    .context("Cleanup prepare: same original full-body native read")?;
                            }
                            Ok(())
                        }))
                        .unwrap_or_else(|_| {
                            Err(anyhow::anyhow!(
                                "Actual cleanup native read worker panicked"
                            ))
                        });
                    owned.outcome = Some(outcome);
                });
            let handle = match spawned {
                Ok(handle) => handle,
                Err(error) => {
                    self.original = state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .original
                        .take();
                    self.final_guard = state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .final_guard
                        .take();
                    return Err(error.into());
                }
            };
            let worker = permit.attach(handle);
            self.verification = Some(CleanupVerification { worker, state });
            start
                .send(())
                .context("Cannot start actual cleanup native read loan")?;
        }
        let verification = self
            .verification
            .as_ref()
            .expect("Admitted cleanup read loan");
        ensure!(
            verification.worker.wait(std::time::Duration::from_secs(30)),
            "Actual cleanup native read loan has not joined"
        );
        let mut owned = verification
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.original = owned.original.take();
        self.final_guard = owned.final_guard.take();
        let outcome = owned
            .outcome
            .take()
            .context("Joined cleanup read loan returned no outcome")?;
        if outcome.is_ok()
            && let Some(absent) = &mut owned.absent
        {
            match absent.finish_after_join() {
                Ok(absence) => {
                    self.receipt = Some(NativeRemovedReceipt::seal(&self.intent, absence)?)
                }
                Err(error) => {
                    owned.outcome = Some(Ok(()));
                    return Err(error);
                }
            }
        }
        drop(owned);
        self.verification = None;
        outcome
    }

    pub fn advance(&mut self, coordinator: &dyn CleanupCoordinator) -> Result<CleanupPhase> {
        self.intent.validate()?;
        self.cancel.check()?;
        self.journal.state.directory.verify()?;
        if self.scope.is_none() {
            let selected = self
                .selected
                .as_ref()
                .context("Cleanup recovery has no current selected destination")?;
            self.scope = Some(selected.cleanup_scope(&self.intent.allocation)?);
        }
        let scope = self
            .scope
            .as_ref()
            .expect("Bound cleanup namespace")
            .clone();
        scope.cleanup_matches_allocation(&self.intent.allocation)?;
        if let Some(original) = &self.original {
            ensure!(
                original.record() == &self.intent.allocation.record,
                "Cleanup live original differs from its intent"
            );
        }
        let known_phase = if let Some(entry) = &self.entry {
            entry.phase()
        } else {
            self.journal
                .load(&self.intent.allocation.record.nonce)?
                .map(|snapshot| snapshot.phase)
        };
        let boundary = if matches!(
            known_phase,
            Some(CleanupPhase::Removed | CleanupPhase::Acknowledged)
        ) {
            CleanupBoundary::Acknowledge
        } else {
            CleanupBoundary::Prepare
        };
        coordinator.validate(&self.intent, boundary)?;
        if self.entry.is_none() && known_phase.is_some() {
            self.entry = Some(self.journal.bind(&self.intent)?);
        }
        if let Some(entry) = &mut self.entry {
            let _gate = self
                .journal
                .state
                .gate
                .lock()
                .map_err(|_| anyhow::anyhow!("Cleanup journal lock was poisoned"))?;
            entry.refresh(&self.journal, &self.intent)?;
        }
        if self.prepared.is_none()
            && self
                .entry
                .as_ref()
                .and_then(|entry| entry.parsed.last.as_ref())
                .is_some_and(|record| record.version == 2)
        {
            self.prepared = Some(
                self.entry
                    .as_ref()
                    .unwrap()
                    .recorded_attestation(&self.journal)?,
            );
        }
        let mut calibration = if self.prepared.is_some() {
            Some(scope.calibrate_cleanup_volume()?)
        } else {
            None
        };
        if self.phase() == Some(CleanupPhase::Acknowledged) {
            let receipt = NativeRemovedReceipt::seal(
                &self.intent,
                scope.cleanup_attested_absence(
                    &self.intent.allocation.record,
                    self.prepared
                        .as_ref()
                        .context("Acknowledged journal has no native prepared evidence")?,
                    calibration
                        .as_ref()
                        .context("Acknowledged journal has no fresh actual calibration")?,
                )?,
            )?;
            let stored = self
                .entry
                .as_ref()
                .and_then(|entry| entry.parsed.last.as_ref())
                .and_then(|record| record.receipt.as_ref())
                .context("Acknowledged cleanup phase has no durable native receipt")?;
            ensure!(
                stored == &receipt.stored(),
                "Acknowledged cleanup receipt differs from its actual selected volume"
            );
            self.receipt = Some(receipt);
            return Ok(CleanupPhase::Acknowledged);
        }

        if self.phase() == Some(CleanupPhase::Removed) && self.receipt.is_none() {
            let absence = scope.cleanup_attested_absence(
                &self.intent.allocation.record,
                self.prepared
                    .as_ref()
                    .context("Removed journal has no native prepared evidence")?,
                calibration
                    .as_ref()
                    .context("Removed journal has no fresh actual calibration")?,
            )?;
            let receipt = NativeRemovedReceipt::seal(&self.intent, absence)?;
            let stored = self
                .entry
                .as_ref()
                .and_then(|entry| entry.parsed.last.as_ref())
                .and_then(|record| record.receipt.as_ref())
                .context("Removed cleanup phase has no durable receipt")?;
            ensure!(
                stored == &receipt.stored(),
                "Recovered cleanup receipt differs from its actual selected volume"
            );
            self.receipt = Some(receipt);
        }

        if self.receipt.is_none() {
            self.verify_original_loan(&scope, calibration.as_ref())
                .context("Cleanup tracked preparation and actual join")?;
            if self.prepared.is_none() {
                let original = self
                    .original
                    .as_ref()
                    .context("Cleanup has no actual valid original ID attestation")?;
                self.prepared = Some(original.attest_prepared(
                    self.final_guard.as_ref().unwrap(),
                    self.intent.raw_bytes,
                    &self.intent.raw_sha256,
                    &self.cancel,
                )?);
                calibration = Some(
                    scope
                        .calibrate_cleanup_volume()
                        .context("Cleanup prepare: actual native volume calibration")?,
                );
                if let Some(entry) = &mut self.entry {
                    entry.upgrade_legacy(
                        &self.journal,
                        &self.intent,
                        self.prepared.as_ref().unwrap(),
                    )?;
                }
            }
            if self.phase().is_none() {
                coordinator.validate(&self.intent, CleanupBoundary::Prepare)?;
                self.cancel.check()?;
                self.final_guard
                    .as_ref()
                    .expect("Bound cleanup final file")
                    .verify(&self.cancel)?;
                if self.entry.is_none() {
                    self.entry = Some(
                        self.journal
                            .bind(&self.intent)
                            .context("Cleanup prepare: private journal native bind")?,
                    );
                }
                self.entry.as_mut().expect("Bound cleanup journal").append(
                    &self.journal,
                    &self.intent,
                    CleanupPhase::Prepared,
                    None,
                    self.prepared.as_ref().unwrap(),
                    None,
                )?;
            }
            if self.receipt.is_none() {
                coordinator.validate(&self.intent, CleanupBoundary::Remove)?;
                self.final_guard
                    .as_ref()
                    .expect("Bound cleanup final file")
                    .verify(&self.cancel)?;
                self.journal.state.directory.verify()?;
                let original = self
                    .original
                    .as_mut()
                    .context("Cleanup original handle is unavailable")?;
                if !original.is_closed() {
                    self.disposition = Some(
                        original
                            .request_cleanup_disposition(
                                self.prepared.as_ref().unwrap(),
                                &self.cancel,
                            )
                            .context("Cleanup remove: same original native disposition")?,
                    );
                    self.entry.as_mut().unwrap().append(
                        &self.journal,
                        &self.intent,
                        CleanupPhase::Prepared,
                        None,
                        self.prepared.as_ref().unwrap(),
                        self.disposition.as_ref(),
                    )?;
                    original
                        .close_disposition(self.disposition.as_ref().unwrap())
                        .context("Cleanup remove: actual joined original handle close")?;
                }
                let absence = original.finish_calibrated_absence(
                    self.prepared.as_ref().unwrap(),
                    calibration.as_ref().unwrap(),
                )?;
                self.receipt = Some(NativeRemovedReceipt::seal(&self.intent, absence)?);
            }
        }

        if self.phase() == Some(CleanupPhase::Prepared) {
            let stored = self
                .receipt
                .as_ref()
                .context("Cleanup has no actual native removal receipt")?
                .stored();
            self.entry.as_mut().expect("Bound cleanup journal").append(
                &self.journal,
                &self.intent,
                CleanupPhase::Removed,
                Some(stored),
                self.prepared
                    .as_ref()
                    .context("Removed phase has no native prepared attestation")?,
                self.disposition.as_ref(),
            )?;
        }
        ensure!(
            self.phase() == Some(CleanupPhase::Removed),
            "Cleanup removal has not reached its durable journal phase"
        );
        self.cancel.check()?;
        scope.verify()?;
        coordinator.validate(&self.intent, CleanupBoundary::Acknowledge)?;
        let receipt = self
            .receipt
            .as_ref()
            .context("Cleanup ACK has no actual native removal receipt")?;
        coordinator.acknowledge(receipt)?;
        self.entry.as_mut().expect("Bound cleanup journal").append(
            &self.journal,
            &self.intent,
            CleanupPhase::Acknowledged,
            Some(receipt.stored()),
            self.prepared
                .as_ref()
                .context("Cleanup ACK has no native prepared attestation")?,
            self.disposition.as_ref(),
        )?;
        Ok(CleanupPhase::Acknowledged)
    }
}
