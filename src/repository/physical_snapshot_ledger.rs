//! Durable metadata coordination for actual native snapshot owners.
//! Stored locators and cleanup facts never constitute filesystem capabilities.

use super::{
    Repository, schema,
    snapshot_export::{
        PhysicalSnapshotCleanupFact, PhysicalSnapshotReceipt, PhysicalSnapshotRecoveryAction,
        PhysicalSnapshotRecoveryJournal, PhysicalSnapshotRecoveryLocator,
        PhysicalSnapshotRecoveryPreview, PhysicalSnapshotRecoveryRecord, PhysicalSnapshotRequest,
    },
    snapshot_native::{ActualFileMetadata, FileIdentity},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

const MAX_DOCUMENT: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: i64 = 16 * 1024 * 1024;
type SnapshotLedgerRow = (
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    String,
    i64,
    String,
    String,
    i64,
);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalSnapshotLedgerContext {
    pub operation_id: String,
    pub profile_id: String,
    pub profile_epoch: u64,
    pub operation_epoch: u64,
    pub selection_epoch: u64,
    pub expected_snapshot_revision: u64,
    pub maximum_image_bytes: u64,
    pub basename: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalSnapshotLedgerState {
    Registered,
    ReceiptCommitted,
    Finished,
}
impl PhysicalSnapshotLedgerState {
    fn name(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::ReceiptCommitted => "receipt_committed",
            Self::Finished => "finished",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalSnapshotLedgerRecord {
    pub context: PhysicalSnapshotLedgerContext,
    pub ledger_revision: u64,
    pub state: PhysicalSnapshotLedgerState,
    pub recovery: PhysicalSnapshotRecoveryRecord,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocatorProjection {
    operation_id: String,
    request_digest: String,
    request: PhysicalSnapshotRequest,
    manifest: ActualFileMetadata,
    selected_identity: FileIdentity,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupEnvelope {
    native: CleanupProjection,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupProjection {
    file: ActualFileMetadata,
    stamp: serde_json::Value,
    sha256: String,
    pending_zero_links: bool,
    removed: bool,
}
impl CleanupProjection {
    fn stage(&self) -> u8 {
        if self.removed {
            2
        } else {
            u8::from(self.pending_zero_links)
        }
    }
}
fn projection(locator: &PhysicalSnapshotRecoveryLocator) -> Result<LocatorProjection> {
    let bytes = serde_json::to_vec(locator)?;
    ensure!(
        bytes.len() <= MAX_DOCUMENT,
        "Snapshot locator exceeds its budget"
    );
    serde_json::from_slice(&bytes).context("Snapshot locator shape changed")
}
fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn integer(value: u64) -> Result<i64> {
    i64::try_from(value).context("Snapshot ledger integer exceeds SQLite range")
}
fn validate_context(context: &PhysicalSnapshotLedgerContext) -> Result<()> {
    ensure!(
        text(&context.operation_id)
            && context.profile_id.len() == 32
            && context.profile_id.bytes().all(|b| b.is_ascii_hexdigit())
            && context.profile_epoch > 0
            && context.operation_epoch > 0
            && context.selection_epoch > 0
            && context.maximum_image_bytes > 0
            && context.maximum_image_bytes <= super::snapshot_export::MAX_PHYSICAL_SNAPSHOT_BYTES,
        "Invalid physical snapshot operation context"
    );
    for value in [
        context.profile_epoch,
        context.operation_epoch,
        context.selection_epoch,
        context.expected_snapshot_revision,
        context.maximum_image_bytes,
    ] {
        integer(value)?;
    }
    super::snapshot_native::validate_basename(&context.basename)
}
fn validate_locator(
    context: &PhysicalSnapshotLedgerContext,
    locator: &PhysicalSnapshotRecoveryLocator,
) -> Result<LocatorProjection> {
    validate_context(context)?;
    let p = projection(locator)?;
    ensure!(
        p.operation_id == context.operation_id
            && p.request.operation_id == context.operation_id
            && p.request.expected_revision == context.expected_snapshot_revision
            && p.request.maximum_image_bytes == context.maximum_image_bytes
            && p.request.basename == context.basename
            && p.manifest.operation_id == context.operation_id
            && p.manifest.selection_epoch == context.selection_epoch
            && p.manifest.identity.is_some()
            && p.manifest.nonce.len() == 32
            && p.manifest.nonce.bytes().all(|b| b.is_ascii_hexdigit())
            && p.manifest.temporary_name.as_deref()
                == Some(format!(".lx-snapshot-{}.manifest", p.manifest.nonce).as_str())
            && !p.manifest.published
            && p.manifest.final_basename.is_none()
            && p.request_digest == format!("{:x}", Sha256::digest(serde_json::to_vec(&p.request)?)),
        "Snapshot locator differs from its immutable context"
    );
    Ok(p)
}
fn profile_state(connection: &Connection) -> Result<(String, u64, u64, u64)> {
    let (id,profile,operation,selection):(String,i64,i64,i64)=connection.query_row("SELECT profile_id,profile_epoch,operation_epoch,selection_epoch FROM physical_snapshot_state WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(
        id.len() == 32
            && id.bytes().all(|b| b.is_ascii_hexdigit())
            && profile > 0
            && operation >= 0
            && selection >= 0,
        "Invalid snapshot profile state"
    );
    Ok((
        id,
        u64::try_from(profile)?,
        u64::try_from(operation)?,
        u64::try_from(selection)?,
    ))
}
fn current_context(connection: &Connection, context: &PhysicalSnapshotLedgerContext) -> Result<()> {
    validate_context(context)?;
    let (id, epoch, operation, selection) = profile_state(connection)?;
    ensure!(
        id == context.profile_id
            && epoch == context.profile_epoch
            && context.operation_epoch <= operation
            && context.selection_epoch <= selection,
        "Snapshot operation belongs to a retired or unallocated profile context"
    );
    Ok(())
}
fn validate_receipt(
    record: &PhysicalSnapshotLedgerRecord,
    receipt: &PhysicalSnapshotReceipt,
) -> Result<()> {
    let p = validate_locator(&record.context, &record.recovery.locator)?;
    ensure!(
        receipt.operation_id == record.context.operation_id
            && receipt.snapshot_revision == record.context.expected_snapshot_revision
            && receipt.schema_version == super::SCHEMA_VERSION
            && receipt.selection_epoch == record.context.selection_epoch
            && receipt.bytes > 0
            && receipt.bytes <= record.context.maximum_image_bytes
            && receipt.sha256.len() == 64
            && receipt.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            && receipt.root_device == p.selected_identity.device
            && receipt.root_object == p.selected_identity.object
            && receipt.file_device == receipt.root_device
            && receipt.file_object.len() == 32
            && receipt.file_object.bytes().all(|b| b.is_ascii_hexdigit())
            && receipt.path.is_absolute()
            && receipt.path.components().all(|c| !matches!(
                c,
                std::path::Component::CurDir | std::path::Component::ParentDir
            ))
            && receipt
                .path
                .file_name()
                .is_some_and(|n| n == std::ffi::OsStr::new(&record.context.basename)),
        "Snapshot publication receipt differs from its operation"
    );
    Ok(())
}
fn validate_record(record: &PhysicalSnapshotLedgerRecord) -> Result<()> {
    validate_locator(&record.context, &record.recovery.locator)?;
    ensure!(
        record.ledger_revision > 0,
        "Invalid snapshot ledger revision"
    );
    integer(record.ledger_revision)?;
    ensure!(
        record.recovery.cleanup.len() <= 16
            && serde_json::to_vec(&record.recovery.cleanup)?.len() <= MAX_DOCUMENT,
        "Snapshot cleanup fact quota exceeded"
    );
    let mut nonces = std::collections::BTreeSet::new();
    for fact in &record.recovery.cleanup {
        ensure!(
            nonces.insert(fact.nonce()),
            "Snapshot cleanup nonce is duplicated"
        );
        validate_cleanup(record, fact)?;
    }
    if let Some(receipt) = &record.recovery.publication {
        validate_receipt(record, receipt)?;
    }
    ensure!(
        record.state != PhysicalSnapshotLedgerState::ReceiptCommitted
            || record.recovery.publication.is_some(),
        "Committed snapshot is missing its receipt"
    );
    ensure!(
        record.state != PhysicalSnapshotLedgerState::Finished || record.recovery.cleanup.is_empty(),
        "Finished snapshot retains cleanup claims"
    );
    Ok(())
}
fn validate_cleanup(
    record: &PhysicalSnapshotLedgerRecord,
    fact: &PhysicalSnapshotCleanupFact,
) -> Result<CleanupProjection> {
    let bytes = serde_json::to_vec(fact)?;
    ensure!(
        bytes.len() <= MAX_DOCUMENT,
        "Snapshot cleanup fact exceeds its budget"
    );
    let p: CleanupEnvelope = serde_json::from_slice(&bytes)?;
    let p = p.native;
    let locator = projection(&record.recovery.locator)?;
    ensure!(
        p.file.operation_id == record.context.operation_id
            && p.file.identity.is_some()
            && !p.file.published
            && p.file.nonce.len() == 32
            && p.file.nonce.bytes().all(|b| b.is_ascii_hexdigit())
            && p.sha256.len() == 64
            && p.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            && (!p.removed || p.pending_zero_links)
            && p.file.final_basename.is_none()
            && p.file
                .identity
                .as_ref()
                .is_some_and(|id| id.object.len() == 32
                    && id.object.bytes().all(|b| b.is_ascii_hexdigit())
                    && id.device == p.file.directory_identity.device)
            && p.file
                .temporary_name
                .as_deref()
                .is_some_and(|name| name == format!(".lx-snapshot-{}.part", p.file.nonce)
                    || name == format!(".lx-snapshot-{}.manifest", p.file.nonce))
            && p.stamp.as_object().is_some_and(|stamp| stamp.len() == 3
                && stamp.get("length").and_then(serde_json::Value::as_u64) == Some(p.file.bytes)
                && stamp
                    .get("changed")
                    .is_some_and(|v| v.as_i64().is_some() || v.as_u64().is_some())
                && stamp
                    .get("written")
                    .is_some_and(|v| v.as_i64().is_some() || v.as_u64().is_some()))
            && p.file.selection_epoch >= record.context.selection_epoch
            && (p.file.directory_identity == locator.manifest.directory_identity
                || p.file.directory_identity == locator.selected_identity),
        "Cleanup fact is outside its immutable snapshot operation"
    );
    if let Some(receipt) = &record.recovery.publication {
        ensure!(
            p.file.identity.as_ref().is_none_or(
                |id| id.device != receipt.file_device || id.object != receipt.file_object
            ),
            "Published final file cannot be acknowledged as temporary cleanup"
        );
    }
    Ok(p)
}
fn read_record(connection: &Connection, id: &str) -> Result<Option<PhysicalSnapshotLedgerRecord>> {
    let row: Option<SnapshotLedgerRow> = connection.query_row("SELECT operation_id,profile_id,profile_epoch,operation_epoch,selection_epoch,expected_snapshot_revision,maximum_image_bytes,request_digest,ledger_revision,state,document,byte_size FROM physical_snapshot_operations WHERE operation_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?))).optional()?;
    let Some((
        operation,
        profile,
        profile_epoch,
        operation_epoch,
        selection,
        expected,
        maximum,
        digest,
        revision,
        state,
        document,
        bytes,
    )) = row
    else {
        return Ok(None);
    };
    ensure!(
        bytes > 0 && bytes == i64::try_from(document.len())? && document.len() <= MAX_DOCUMENT,
        "Snapshot ledger document projection is invalid"
    );
    let record: PhysicalSnapshotLedgerRecord = serde_json::from_str(&document)?;
    validate_record(&record)?;
    ensure!(
        record.context.operation_id == operation
            && record.context.profile_id == profile
            && integer(record.context.profile_epoch)? == profile_epoch
            && integer(record.context.operation_epoch)? == operation_epoch
            && integer(record.context.selection_epoch)? == selection
            && integer(record.context.expected_snapshot_revision)? == expected
            && integer(record.context.maximum_image_bytes)? == maximum
            && record.recovery.locator.request_digest == digest
            && integer(record.ledger_revision)? == revision
            && record.state.name() == state,
        "Snapshot ledger SQL/document bindings differ"
    );
    Ok(Some(record))
}
fn storage_budget(connection: &Connection) -> Result<()> {
    let (pending,history,bytes):(i64,i64,i64)=connection.query_row("SELECT coalesce(sum(state!='finished'),0),coalesce(sum(state='finished'),0),coalesce(sum(byte_size),0) FROM physical_snapshot_operations",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    ensure!(
        pending <= 4 && history <= 64 && bytes <= MAX_TOTAL_BYTES,
        "Snapshot ledger storage quota exceeded"
    );
    Ok(())
}
fn save_record(
    connection: &Connection,
    record: &PhysicalSnapshotLedgerRecord,
    previous: Option<u64>,
) -> Result<()> {
    validate_record(record)?;
    let document = serde_json::to_string(record)?;
    ensure!(
        document.len() <= MAX_DOCUMENT,
        "Snapshot ledger document exceeds 1 MiB"
    );
    let c = &record.context;
    if let Some(previous) = previous {
        let count=connection.execute("UPDATE physical_snapshot_operations SET ledger_revision=?2,state=?3,document=?4,byte_size=?5 WHERE operation_id=?1 AND ledger_revision=?6",params![c.operation_id,integer(record.ledger_revision)?,record.state.name(),document,i64::try_from(document.len())?,integer(previous)?])?;
        ensure!(count == 1, "Snapshot ledger CAS changed");
    } else {
        connection.execute("INSERT INTO physical_snapshot_operations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![c.operation_id,c.profile_id,integer(c.profile_epoch)?,integer(c.operation_epoch)?,integer(c.selection_epoch)?,integer(c.expected_snapshot_revision)?,integer(c.maximum_image_bytes)?,record.recovery.locator.request_digest,integer(record.ledger_revision)?,record.state.name(),document,i64::try_from(document.len())?])?;
    }
    if record.state == PhysicalSnapshotLedgerState::Finished {
        connection.execute("DELETE FROM physical_snapshot_operations WHERE state='finished' AND operation_id!=?1 AND operation_id IN(SELECT operation_id FROM physical_snapshot_operations WHERE state='finished' AND operation_id!=?1 ORDER BY operation_epoch DESC LIMIT -1 OFFSET 63)",[&record.context.operation_id])?;
    }
    storage_budget(connection)
}

impl Repository {
    pub fn physical_snapshot_profile_scope(&self) -> Result<(String, u64)> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let (id, epoch, _, _) = profile_state(&connection)?;
        Ok((id, epoch))
    }
    fn snapshot_next_epoch(&self, selection: bool) -> Result<u64> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, _, operation, selected) = profile_state(&tx)?;
        let next = if selection { selected } else { operation }
            .checked_add(1)
            .context("Snapshot epoch overflow")?;
        integer(next)?;
        tx.execute(
            if selection {
                "UPDATE physical_snapshot_state SET selection_epoch=?1 WHERE singleton=1"
            } else {
                "UPDATE physical_snapshot_state SET operation_epoch=?1 WHERE singleton=1"
            },
            [integer(next)?],
        )?;
        tx.commit()?;
        Ok(next)
    }
    pub fn physical_snapshot_next_operation_epoch(&self) -> Result<u64> {
        self.snapshot_next_epoch(false)
    }
    pub fn physical_snapshot_next_selection_epoch(&self) -> Result<u64> {
        self.snapshot_next_epoch(true)
    }
    pub fn physical_snapshot_record(
        &self,
        id: &str,
    ) -> Result<Option<PhysicalSnapshotLedgerRecord>> {
        ensure!(text(id), "Invalid snapshot operation ID");
        let c = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&c)?;
        read_record(&c, id)
    }
    pub fn physical_snapshot_records(&self) -> Result<Vec<PhysicalSnapshotLedgerRecord>> {
        let c = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&c)?;
        storage_budget(&c)?;
        let mut statement = c.prepare(
            "SELECT operation_id FROM physical_snapshot_operations ORDER BY operation_epoch",
        )?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            records.push(read_record(&c, &id)?.context("Snapshot ledger row disappeared")?);
        }
        Ok(records)
    }
    pub fn physical_snapshot_pending_records(&self) -> Result<Vec<PhysicalSnapshotLedgerRecord>> {
        Ok(self
            .physical_snapshot_records()?
            .into_iter()
            .filter(|r| r.state != PhysicalSnapshotLedgerState::Finished)
            .collect())
    }
}

pub struct PhysicalSnapshotLedgerJournal {
    repository: Arc<Repository>,
    context: PhysicalSnapshotLedgerContext,
    record: Mutex<Option<PhysicalSnapshotLedgerRecord>>,
}
impl PhysicalSnapshotLedgerJournal {
    pub fn new(
        repository: Arc<Repository>,
        context: PhysicalSnapshotLedgerContext,
    ) -> Result<Self> {
        validate_context(&context)?;
        let record = repository.physical_snapshot_record(&context.operation_id)?;
        {
            let c = repository
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
            current_context(&c, &context)?;
        }
        if let Some(record) = &record {
            ensure!(record.context == context, "Snapshot context changed");
        }
        Ok(Self {
            repository,
            context,
            record: Mutex::new(record),
        })
    }
    pub fn begin_registration(&self, locator: &PhysicalSnapshotRecoveryLocator) -> Result<()> {
        validate_locator(&self.context, locator)?;
        let mut local = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot journal poisoned"))?;
        let mut connection = self
            .repository
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        current_context(&tx, &self.context)?;
        if let Some(existing) = read_record(&tx, &self.context.operation_id)? {
            ensure!(
                existing.context == self.context
                    && serde_json::to_vec(&existing.recovery.locator)?
                        == serde_json::to_vec(locator)?,
                "Snapshot operation ID was reused with another allocation"
            );
            if let Some(local) = local.as_ref() {
                ensure!(
                    local.ledger_revision == existing.ledger_revision,
                    "Snapshot journal CAS changed"
                );
            }
            tx.commit()?;
            *local = Some(existing);
            return Ok(());
        }
        ensure!(local.is_none(), "Snapshot ledger registration disappeared");
        let record = PhysicalSnapshotLedgerRecord {
            context: self.context.clone(),
            ledger_revision: 1,
            state: PhysicalSnapshotLedgerState::Registered,
            recovery: PhysicalSnapshotRecoveryRecord {
                locator: locator.clone(),
                publication: None,
                cleanup: Vec::new(),
            },
        };
        save_record(&tx, &record, None)?;
        tx.commit()?;
        *local = Some(record);
        Ok(())
    }
    fn update(
        &self,
        locator: &PhysicalSnapshotRecoveryLocator,
        change: impl FnOnce(&mut PhysicalSnapshotLedgerRecord) -> Result<bool>,
    ) -> Result<()> {
        let mut local = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot journal poisoned"))?;
        let saved = local
            .as_ref()
            .context("Snapshot operation is not registered")?;
        let mut connection = self
            .repository
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        current_context(&tx, &self.context)?;
        let mut current = read_record(&tx, &self.context.operation_id)?
            .context("Snapshot ledger operation disappeared")?;
        ensure!(
            current.context == self.context
                && current.ledger_revision == saved.ledger_revision
                && serde_json::to_vec(&current.recovery.locator)? == serde_json::to_vec(locator)?,
            "Snapshot journal context/locator/CAS changed"
        );
        let old = current.ledger_revision;
        if change(&mut current)? {
            current.ledger_revision = old
                .checked_add(1)
                .context("Snapshot ledger revision overflow")?;
            save_record(&tx, &current, Some(old))?;
        }
        tx.commit()?;
        *local = Some(current);
        Ok(())
    }
    pub fn authorize_locator(&self, locator: &PhysicalSnapshotRecoveryLocator) -> Result<()> {
        validate_locator(&self.context, locator)?;
        let local = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot journal poisoned"))?;
        let saved = local
            .as_ref()
            .context("Snapshot operation is not registered")?;
        let c = self
            .repository
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&c)?;
        current_context(&c, &self.context)?;
        let current = read_record(&c, &self.context.operation_id)?
            .context("Snapshot operation disappeared")?;
        ensure!(
            current.context == self.context
                && current.ledger_revision == saved.ledger_revision
                && current.state != PhysicalSnapshotLedgerState::Finished
                && serde_json::to_vec(&current.recovery.locator)? == serde_json::to_vec(locator)?,
            "Snapshot recovery context/CAS is stale"
        );
        Ok(())
    }
    pub fn authorize_live_locator(
        &self,
        locator: &PhysicalSnapshotRecoveryLocator,
        action: PhysicalSnapshotRecoveryAction,
    ) -> Result<()> {
        self.authorize_locator(locator)?;
        let record = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot journal poisoned"))?;
        let record = record.as_ref().unwrap();
        if action == PhysicalSnapshotRecoveryAction::CleanupPrivate {
            ensure!(
                record.recovery.publication.is_none(),
                "Committed snapshot requires receipt recovery, not private cleanup"
            );
        }
        Ok(())
    }
    pub fn authorize(
        &self,
        preview: &PhysicalSnapshotRecoveryPreview,
        action: PhysicalSnapshotRecoveryAction,
    ) -> Result<PhysicalSnapshotRecoveryAction> {
        let locator = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot journal poisoned"))?
            .as_ref()
            .context("Snapshot operation is not registered")?
            .recovery
            .locator
            .clone();
        self.authorize_live_locator(&locator, action)?;
        ensure!(
            preview.operation_id == self.context.operation_id
                && preview.request_digest == locator.request_digest
                && preview.snapshot_revision == self.context.expected_snapshot_revision,
            "Snapshot recovery preview differs from its operation"
        );
        let record = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot journal poisoned"))?;
        let current = record.as_ref().unwrap();
        if let Some(committed) = &current.recovery.publication {
            ensure!(
                serde_json::to_vec(&preview.recorded_receipt)?
                    == serde_json::to_vec(&Some(committed))?,
                "Snapshot manifest/application receipts disagree"
            );
        } else if let Some(receipt) = &preview.recorded_receipt {
            validate_receipt(current, receipt)?;
        }
        Ok(action)
    }
    pub fn commit_receipt(&self, receipt: &PhysicalSnapshotReceipt) -> Result<()> {
        let locator = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot journal poisoned"))?
            .as_ref()
            .context("Snapshot operation is not registered")?
            .recovery
            .locator
            .clone();
        self.update(&locator, |record| {
            validate_receipt(record, receipt)?;
            if let Some(previous) = &record.recovery.publication {
                ensure!(
                    serde_json::to_vec(previous)? == serde_json::to_vec(receipt)?,
                    "Snapshot publication receipt changed"
                );
                return Ok(false);
            }
            ensure!(
                record.state == PhysicalSnapshotLedgerState::Registered,
                "Snapshot receipt state is out of order"
            );
            record.recovery.publication = Some(receipt.clone());
            record.state = PhysicalSnapshotLedgerState::ReceiptCommitted;
            Ok(true)
        })
    }
}
impl PhysicalSnapshotRecoveryJournal for PhysicalSnapshotLedgerJournal {
    fn record_cleanup(
        &self,
        locator: &PhysicalSnapshotRecoveryLocator,
        fact: &PhysicalSnapshotCleanupFact,
    ) -> Result<()> {
        self.update(locator, |record| {
            ensure!(
                record.state != PhysicalSnapshotLedgerState::Finished,
                "Finished snapshot cannot regain cleanup ownership"
            );
            let new = validate_cleanup(record, fact)?;
            if let Some(index) = record
                .recovery
                .cleanup
                .iter()
                .position(|old| old.nonce() == fact.nonce())
            {
                let old = validate_cleanup(record, &record.recovery.cleanup[index])?;
                ensure!(
                    old.file.operation_id == new.file.operation_id
                        && old.file.identity == new.file.identity
                        && old.file.directory_identity == new.file.directory_identity
                        && old.file.bytes == new.file.bytes
                        && old.sha256 == new.sha256
                        && old.stamp == new.stamp
                        && new.stage() >= old.stage(),
                    "Snapshot cleanup fact changed identity/body or regressed"
                );
                if serde_json::to_vec(&record.recovery.cleanup[index])? == serde_json::to_vec(fact)?
                {
                    return Ok(false);
                }
                record.recovery.cleanup[index] = fact.clone();
            } else {
                ensure!(
                    record.recovery.cleanup.len() < 16,
                    "Snapshot cleanup fact count exceeded"
                );
                record.recovery.cleanup.push(fact.clone());
            }
            Ok(true)
        })
    }
    fn finished(&self, locator: &PhysicalSnapshotRecoveryLocator) -> Result<()> {
        self.update(locator, |record| {
            if record.state == PhysicalSnapshotLedgerState::Finished {
                return Ok(false);
            }
            ensure!(
                !record.recovery.cleanup.is_empty()
                    && record
                        .recovery
                        .cleanup
                        .iter()
                        .all(PhysicalSnapshotCleanupFact::removed),
                "Snapshot cleanup is not durably confirmed"
            );
            ensure!(
                record.recovery.cleanup.iter().any(|f| f.nonce()
                    == projection(locator)
                        .map(|p| p.manifest.nonce)
                        .unwrap_or_default()),
                "Snapshot manifest cleanup is not confirmed"
            );
            record.state = PhysicalSnapshotLedgerState::Finished;
            record.recovery.cleanup.clear();
            Ok(true)
        })?;
        Ok(())
    }
}
