#![cfg(windows)]
use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    repository::{
        Repository,
        snapshot_export::{
            PhysicalSnapshotCleanupFact, PhysicalSnapshotContext, PhysicalSnapshotFailureKind,
            PhysicalSnapshotLimits, PhysicalSnapshotReceipt, PhysicalSnapshotRecoveryAction,
            PhysicalSnapshotRecoveryJournal, PhysicalSnapshotRecoveryLocator,
            PhysicalSnapshotRecoveryRecord, PhysicalSnapshotRequest, PhysicalSnapshotSelection,
        },
    },
    sync::{PeerCapabilities, SharedKey, network::PeerRecord},
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
fn assert_checkpoint_released(repo: &Repository) {
    let connection = Connection::open(repo.path()).unwrap();
    connection.busy_timeout(Duration::from_secs(2)).unwrap();
    let result: (i64, i64, i64) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        result,
        (0, 0, 0),
        "A cancelled snapshot retained a genuine WAL read mark"
    );
}
#[derive(Default)]
struct JournalState {
    locator: Option<PhysicalSnapshotRecoveryLocator>,
    publication: Option<PhysicalSnapshotReceipt>,
    cleanup: BTreeMap<String, PhysicalSnapshotCleanupFact>,
    finished: bool,
    records: usize,
}
struct Journal {
    path: PathBuf,
    state: Mutex<JournalState>,
    fail_finished: AtomicBool,
}
impl Journal {
    fn new(root: &Path) -> Self {
        Self {
            path: root.join("snapshot-recovery-journal"),
            state: Mutex::new(JournalState::default()),
            fail_finished: AtomicBool::new(false),
        }
    }
    fn append(&self, state: &mut JournalState, value: serde_json::Value) -> anyhow::Result<()> {
        anyhow::ensure!(state.records < 64, "Fixture recovery journal is full");
        let bytes = serde_json::to_vec(&value)?;
        anyhow::ensure!(
            bytes.len() <= 16 * 1024,
            "Fixture record exceeds its byte bound"
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)?;
        anyhow::ensure!(
            file.metadata()?.len() + bytes.len() as u64 + 36 <= 1024 * 1024,
            "Fixture journal exceeds 1 MiB"
        );
        file.write_all(&(bytes.len() as u32).to_le_bytes())?;
        file.write_all(&Sha256::digest(&bytes))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        state.records += 1;
        Ok(())
    }
    fn register(&self, locator: &PhysicalSnapshotRecoveryLocator) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        self.append(&mut state, serde_json::json!({"register":locator}))?;
        state.cleanup.clear();
        state.publication = None;
        state.finished = false;
        state.locator = Some(locator.clone());
        Ok(())
    }
    fn commit(&self, receipt: &PhysicalSnapshotReceipt) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(old) = &state.publication {
            anyhow::ensure!(
                serde_json::to_value(old)? == serde_json::to_value(receipt)?,
                "Receipt changed"
            );
            return Ok(());
        }
        self.append(&mut state, serde_json::json!({"publication":receipt}))?;
        state.publication = Some(receipt.clone());
        Ok(())
    }
    fn record(&self) -> PhysicalSnapshotRecoveryRecord {
        let state = self.state.lock().unwrap();
        PhysicalSnapshotRecoveryRecord {
            locator: state.locator.clone().unwrap(),
            publication: state.publication.clone(),
            cleanup: state.cleanup.values().cloned().collect(),
        }
    }
    fn restart(&self) -> Self {
        let journal = Self::new(self.path.parent().unwrap());
        let mut bytes = Vec::new();
        fs::File::open(&self.path)
            .unwrap()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= 1024 * 1024);
        let mut at = 0;
        let mut state = journal.state.lock().unwrap();
        while at < bytes.len() {
            assert!(bytes.len() - at >= 36);
            let length = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let body = &bytes[at + 36..at + 36 + length];
            assert_eq!(Sha256::digest(body).as_slice(), &bytes[at + 4..at + 36]);
            let value: serde_json::Value = serde_json::from_slice(body).unwrap();
            if let Some(value) = value.get("register") {
                state.locator = Some(serde_json::from_value(value.clone()).unwrap());
                state.publication = None;
                state.cleanup.clear();
                state.finished = false;
            }
            if let Some(value) = value.get("publication") {
                state.publication = Some(serde_json::from_value(value.clone()).unwrap());
            }
            if let Some(value) = value.get("cleanup") {
                let fact: PhysicalSnapshotCleanupFact =
                    serde_json::from_value(value.clone()).unwrap();
                state.cleanup.insert(fact.nonce().into(), fact);
            }
            if value.get("finished").is_some() {
                state.finished = true;
            }
            state.records += 1;
            at += 36 + length;
        }
        drop(state);
        journal
    }
}
impl PhysicalSnapshotRecoveryJournal for Journal {
    fn record_cleanup(
        &self,
        locator: &PhysicalSnapshotRecoveryLocator,
        fact: &PhysicalSnapshotCleanupFact,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(
            state.locator.as_ref().unwrap().request_digest == locator.request_digest,
            "Wrong operation"
        );
        if let Some(old) = state.cleanup.get(fact.nonce())
            && serde_json::to_value(old)? == serde_json::to_value(fact)?
        {
            return Ok(());
        }
        anyhow::ensure!(
            state.cleanup.contains_key(fact.nonce()) || state.cleanup.len() < 16,
            "Too many cleanup objects"
        );
        self.append(&mut state, serde_json::json!({"cleanup":fact}))?;
        state.cleanup.insert(fact.nonce().into(), fact.clone());
        Ok(())
    }
    fn finished(&self, locator: &PhysicalSnapshotRecoveryLocator) -> anyhow::Result<()> {
        if self.fail_finished.load(Ordering::Acquire) {
            anyhow::bail!("Injected final operation transaction failure");
        }
        let mut state = self.state.lock().unwrap();
        if state.finished {
            return Ok(());
        }
        self.append(
            &mut state,
            serde_json::json!({"finished":locator.operation_id}),
        )?;
        state.finished = true;
        Ok(())
    }
}
fn context() -> PhysicalSnapshotContext {
    PhysicalSnapshotContext::new(Duration::from_secs(120)).unwrap()
}
fn limits() -> PhysicalSnapshotLimits {
    PhysicalSnapshotLimits::new(4 * 1024 * 1024 * 1024).unwrap()
}
fn request(repo: &Repository, id: &str, basename: &str, maximum: u64) -> PhysicalSnapshotRequest {
    PhysicalSnapshotRequest {
        operation_id: id.into(),
        expected_revision: repo.revision().unwrap(),
        maximum_image_bytes: maximum,
        basename: basename.into(),
    }
}
fn track(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(serde_json::json!({"id":id,"source":"kw","name":"保留曲目","meta":{"songId":id,"unknown":{"keep":true}}}),LxMusicDtoVersion::Modern).unwrap()
}
fn profile() -> (tempfile::TempDir, Repository) {
    let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let repo = Repository::open(folder.path().join("profile")).unwrap();
    (folder, repo)
}
#[test]
fn actual_owned_sqlite_snapshot_removes_secret_bytes_and_keeps_live_profile_unchanged() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let music = track("kw_17");
    repo.upsert_track(&music).unwrap();
    let state = repo.sync_profile_state().unwrap();
    repo.sync_configure_pairing(state.profile_epoch, state.pairing_epoch, true)
        .unwrap();
    let key = SharedKey::from_bytes([0x9d; 16]);
    let secret = key.expose_base64();
    repo.sync_store_peer(
        state.pairing_epoch,
        &PeerRecord {
            peer_id: "snapshot-peer".into(),
            client_id: "snapshot-client".into(),
            key,
            device_name: "Metadata fixture".into(),
            is_mobile: false,
            auth_epoch: 1,
            capabilities: PeerCapabilities {
                list: Some(false),
                dislike: Some(false),
            },
        },
    )
    .unwrap();
    let revision = repo.revision().unwrap();
    let selected_path = folder.path().join("exports");
    fs::create_dir(&selected_path).unwrap();
    let selection = PhysicalSnapshotSelection::open_selected(&selected_path, 1).unwrap();
    let limits = limits();
    let prepared = repo
        .prepare_physical_snapshot(
            request(&repo, "physical-safe", "safe.sqlite3", 16 * 1024 * 1024),
            &selection,
            &context(),
            &limits,
            &|locator| journal.register(locator),
        )
        .unwrap();
    assert!(!selected_path.join("safe.sqlite3").exists());
    assert!(fs::read_dir(&selected_path).unwrap().next().is_none());
    let published = prepared.publish(&context(), &journal).unwrap();
    let receipt = published.receipt().clone();
    let committed = AtomicBool::new(false);
    let receipt = published
        .acknowledge(
            &context(),
            &|record| {
                assert_eq!(record.sha256, receipt.sha256);
                committed.store(true, Ordering::Release);
                Ok(())
            },
            &journal,
        )
        .unwrap();
    assert!(committed.load(Ordering::Acquire));
    let bytes = fs::read(&receipt.path).unwrap();
    assert_eq!(receipt.sha256, format!("{:x}", Sha256::digest(&bytes)));
    assert!(
        !bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    assert_eq!(receipt.snapshot_revision, revision);
    assert_eq!(
        receipt.schema_version,
        lx_gpui_player::repository::SCHEMA_VERSION
    );
    let image = Connection::open(&receipt.path).unwrap();
    assert_eq!(
        image
            .query_row(
                "SELECT count(*) FROM sync_peers WHERE private_key IS NOT NULL OR revoked=0",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        image
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let copied: String = image
        .query_row(
            "SELECT document FROM tracks WHERE id=?1",
            [&music.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(serde_json::from_str::<TrackRecord>(&copied).unwrap(), music);
    assert_eq!(repo.sync_peers().unwrap()[0].key.expose_base64(), secret);
    assert!(repo.sync_profile_state().unwrap().pairing_enabled);
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn physical_image_above_logical_16mib_is_streamed_without_changing_logical_policy() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    for index in 0..40 {
        let mut music = track(&format!("kw_large_{index}"));
        music.metadata.insert(
            "retainedOpaque".into(),
            serde_json::Value::String("x".repeat(512 * 1024)),
        );
        repo.upsert_track(&music).unwrap();
    }
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selection = PhysicalSnapshotSelection::open_selected(&directory, 2).unwrap();
    let limits = limits();
    let prepared = repo
        .prepare_physical_snapshot(
            request(&repo, "large-physical", "large.sqlite3", 64 * 1024 * 1024),
            &selection,
            &context(),
            &limits,
            &|locator| journal.register(locator),
        )
        .unwrap();
    let published = prepared.publish(&context(), &journal).unwrap();
    assert!(published.receipt().bytes > 16 * 1024 * 1024);
    let receipt = published
        .acknowledge(&context(), &|receipt| journal.commit(receipt), &journal)
        .unwrap();
    let image = Connection::open(&receipt.path).unwrap();
    assert_eq!(
        image
            .query_row("SELECT count(*) FROM tracks", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        40
    );
    assert_eq!(fs::metadata(&receipt.path).unwrap().len(), receipt.bytes);
    assert_eq!(
        lx_gpui_player::backup::file_export::MAX_EXPORT_BYTES,
        16 * 1024 * 1024
    );
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn stale_cut_cancel_and_existing_foreign_target_leave_explicit_retained_owners() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    repo.upsert_track(&track("kw_one")).unwrap();
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selection = PhysicalSnapshotSelection::open_selected(&directory, 3).unwrap();
    let limits = limits();
    let mut stale = request(&repo, "stale", "stale.sqlite3", 16 * 1024 * 1024);
    stale.expected_revision = 0;
    let failed = repo
        .prepare_physical_snapshot(stale, &selection, &context(), &limits, &|locator| {
            journal.register(locator)
        })
        .unwrap_err();
    assert!(failed.owner.is_some());
    failed.owner.unwrap().cleanup(&context(), &journal).unwrap();
    assert!(!directory.join("stale.sqlite3").exists());
    let cancelled = context();
    cancelled.cancel();
    let failed = repo
        .prepare_physical_snapshot(
            request(&repo, "cancel", "cancel.sqlite3", 16 * 1024 * 1024),
            &selection,
            &cancelled,
            &limits,
            &|locator| journal.register(locator),
        )
        .unwrap_err();
    assert!(failed.owner.is_none());
    fs::write(directory.join("existing.sqlite3"), b"foreign document").unwrap();
    let prepared = repo
        .prepare_physical_snapshot(
            request(&repo, "conflict", "existing.sqlite3", 16 * 1024 * 1024),
            &selection,
            &context(),
            &limits,
            &|locator| journal.register(locator),
        )
        .unwrap();
    let failed = prepared.publish(&context(), &journal).unwrap_err();
    assert!(failed.owner.is_some());
    assert_eq!(
        fs::read(directory.join("existing.sqlite3")).unwrap(),
        b"foreign document"
    );
    failed.owner.unwrap().cleanup(&context(), &journal).unwrap();
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn receipt_commit_failure_keeps_published_descriptor_and_retry_does_not_republish() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    repo.upsert_track(&track("kw_ack")).unwrap();
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selection = PhysicalSnapshotSelection::open_selected(&directory, 4).unwrap();
    let limits = limits();
    let prepared = repo
        .prepare_physical_snapshot(
            request(&repo, "ack-fault", "ack.sqlite3", 16 * 1024 * 1024),
            &selection,
            &context(),
            &limits,
            &|locator| journal.register(locator),
        )
        .unwrap();
    let published = prepared.publish(&context(), &journal).unwrap();
    let initial = published.receipt().clone();
    let failed = published
        .acknowledge(
            &context(),
            &|_| anyhow::bail!("injected durable receipt failure"),
            &journal,
        )
        .unwrap_err();
    let owner = failed.owner.unwrap();
    assert_eq!(
        owner.published_receipt().unwrap().file_object,
        initial.file_object
    );
    assert_eq!(limits.stats().0, 1);
    let receipt = owner
        .acknowledge_published(
            &context(),
            &|record| {
                assert_eq!(record.file_object, initial.file_object);
                Ok(())
            },
            &journal,
        )
        .unwrap();
    assert_eq!(receipt.sha256, initial.sha256);
    assert_eq!(receipt.file_object, initial.file_object);
    assert_eq!(limits.stats(), (0, 0));
}
fn manifest_path(locator: &PhysicalSnapshotRecoveryLocator) -> PathBuf {
    let value = serde_json::to_value(locator).unwrap();
    PathBuf::from(value["manifest"]["directory"].as_str().unwrap())
        .join(value["manifest"]["temporary_name"].as_str().unwrap())
}
fn prepare_small(
    repo: &Repository,
    selection: &PhysicalSnapshotSelection,
    limits: &PhysicalSnapshotLimits,
    journal: &Journal,
    id: &str,
    name: &str,
) -> lx_gpui_player::repository::snapshot_export::PreparedPhysicalSnapshot {
    repo.prepare_physical_snapshot(
        request(repo, id, name, 1024 * 1024),
        selection,
        &context(),
        limits,
        &|locator| journal.register(locator),
    )
    .unwrap()
}
#[test]
fn cold_compact_recovery_truncates_only_torn_tail_and_preserves_pending_reservation() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    repo.upsert_track(&track("cold-one")).unwrap();
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 11).unwrap();
    let limits = limits();
    let prepared = prepare_small(
        &repo,
        &selected,
        &limits,
        &journal,
        "cold-compact",
        "cold.sqlite3",
    );
    let locator = prepared.release_handles_for_recovery().unwrap();
    assert_eq!(limits.stats().0, 1);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(manifest_path(&locator))
        .unwrap();
    file.write_all(&[8, 0, 0, 0, 1, 2, 3]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let journal = journal.restart();
    let preview = repo
        .inspect_physical_snapshot_recovery(journal.record(), &selected, &context())
        .unwrap();
    assert_eq!(preview.phase, "CompactVerified");
    assert!(preview.artifact_count > 0);
    let recovered = repo
        .recover_physical_snapshot(preview, &context(), &limits, &|preview| {
            assert_eq!(
                preview.request_digest,
                journal.record().locator.request_digest
            );
            Ok(PhysicalSnapshotRecoveryAction::ResumePublication)
        })
        .unwrap();
    let published = recovered.publish(&context(), &journal).unwrap();
    let receipt = published
        .acknowledge(&context(), &|receipt| journal.commit(receipt), &journal)
        .unwrap();
    assert_eq!(
        Sha256::digest(fs::read(&receipt.path).unwrap()).as_slice(),
        hex_digest(&receipt.sha256).as_slice()
    );
    assert_eq!(
        fs::read_dir(folder.path().join("profile/.lx-snapshot-staging"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(limits.stats(), (0, 0));
}
fn hex_digest(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
#[test]
fn cold_publish_intent_resumes_same_final_oid_without_second_rename_or_overwrite() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    repo.upsert_track(&track("intent-one")).unwrap();
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 12).unwrap();
    let limits = limits();
    let published = prepare_small(
        &repo,
        &selected,
        &limits,
        &journal,
        "cold-intent",
        "intent.sqlite3",
    )
    .publish(&context(), &journal)
    .unwrap();
    let initial = published.receipt().clone();
    let failed = published
        .acknowledge(
            &context(),
            &|_| anyhow::bail!("Process boundary before application receipt"),
            &journal,
        )
        .unwrap_err();
    let locator = failed
        .owner
        .unwrap()
        .release_handles_for_recovery()
        .unwrap();
    let path = manifest_path(&locator);
    let bytes = fs::read(&path).unwrap();
    let mut at = 0;
    let mut saw_compact_fact = false;
    while at < bytes.len() {
        let length = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if bytes[at + 36] == 4 {
            let compact: serde_json::Value =
                serde_json::from_slice(&bytes[at + 37..at + 36 + length]).unwrap();
            assert_eq!(compact["main"]["selection_epoch"], 12);
            saw_compact_fact = true;
        }
        if bytes[at + 36] == 6 {
            break;
        }
        at += 36 + length;
    }
    assert!(at < bytes.len());
    assert!(saw_compact_fact);
    let immutable_prefix = bytes[..at].to_vec();
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(at as u64).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let fresh = PhysicalSnapshotSelection::open_selected(&directory, 13).unwrap();
    let journal = journal.restart();
    let preview = repo
        .inspect_physical_snapshot_recovery(journal.record(), &fresh, &context())
        .unwrap();
    assert_eq!(preview.phase, "PublishIntent");
    let recovered = repo
        .recover_physical_snapshot(preview, &context(), &limits, &|_| {
            Ok(PhysicalSnapshotRecoveryAction::ResumePublication)
        })
        .unwrap();
    let published = recovered.publish(&context(), &journal).unwrap();
    assert!(
        fs::read(&path).unwrap().starts_with(&immutable_prefix),
        "Fresh recovery authorization must preserve the original immutable phase records"
    );
    assert_eq!(published.receipt().file_object, initial.file_object);
    assert_eq!(published.receipt().sha256, initial.sha256);
    assert_eq!(published.receipt().selection_epoch, initial.selection_epoch);
    published
        .acknowledge(&context(), &|receipt| journal.commit(receipt), &journal)
        .unwrap();
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn foreign_reused_manifest_is_rejected_before_writer_callback_and_remains_unchanged() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 14).unwrap();
    let limits = limits();
    let locator = prepare_small(
        &repo,
        &selected,
        &limits,
        &journal,
        "foreign-manifest",
        "never.sqlite3",
    )
    .release_handles_for_recovery()
    .unwrap();
    let path = manifest_path(&locator);
    let bytes = fs::read(&path).unwrap();
    fs::rename(&path, path.with_extension("preserved-original")).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert!(
        repo.inspect_physical_snapshot_recovery(journal.record(), &selected, &context())
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(limits.stats().0, 1);
    assert!(!directory.join("never.sqlite3").exists());
}
#[test]
fn closed_pending_owners_still_charge_four_job_quota_until_actual_recovery_cleanup() {
    let (folder, repo) = profile();
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 15).unwrap();
    let limits = limits();
    let mut journals = Vec::new();
    for index in 0..4 {
        let root = folder.path().join(format!("journal-{index}"));
        fs::create_dir(&root).unwrap();
        let journal = Journal::new(&root);
        prepare_small(
            &repo,
            &selected,
            &limits,
            &journal,
            &format!("pending-{index}"),
            &format!("pending-{index}.sqlite3"),
        )
        .release_handles_for_recovery()
        .unwrap();
        journals.push(journal);
    }
    assert_eq!(limits.stats().0, 4);
    let denied = repo
        .prepare_physical_snapshot(
            request(&repo, "fifth", "fifth.sqlite3", 1024 * 1024),
            &selected,
            &context(),
            &limits,
            &|_| Ok(()),
        )
        .unwrap_err();
    assert!(denied.owner.is_none());
    let locators = journals
        .iter()
        .map(|j| j.record().locator)
        .collect::<Vec<_>>();
    let restarted_limits = PhysicalSnapshotLimits::new(4 * 1024 * 1024 * 1024).unwrap();
    repo.restore_pending_physical_snapshot_reservations(&locators, &restarted_limits, &|_| Ok(()))
        .unwrap();
    assert_eq!(restarted_limits.stats().0, 4);
    for journal in journals {
        let journal = journal.restart();
        let preview = repo
            .inspect_physical_snapshot_recovery(journal.record(), &selected, &context())
            .unwrap();
        repo.recover_physical_snapshot(preview, &context(), &limits, &|_| {
            Ok(PhysicalSnapshotRecoveryAction::CleanupPrivate)
        })
        .unwrap()
        .cleanup(&context(), &journal)
        .unwrap();
    }
    assert_eq!(limits.stats(), (0, 0));
    assert_eq!(
        fs::read_dir(folder.path().join("profile/.lx-snapshot-staging"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn receipt_committed_manifest_deleted_then_finished_fault_recovers_without_old_revision_cas() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 16).unwrap();
    let limits = limits();
    let published = prepare_small(
        &repo,
        &selected,
        &limits,
        &journal,
        "finish-fault",
        "finished.sqlite3",
    )
    .publish(&context(), &journal)
    .unwrap();
    let initial = published.receipt().clone();
    journal.fail_finished.store(true, Ordering::Release);
    let failed = published
        .acknowledge(&context(), &|receipt| journal.commit(receipt), &journal)
        .unwrap_err();
    let locator = failed
        .owner
        .unwrap()
        .release_handles_for_recovery()
        .unwrap();
    assert!(!manifest_path(&locator).exists());
    assert_eq!(limits.stats().0, 1);
    repo.upsert_track(&track("after-published-revision"))
        .unwrap();
    assert!(repo.revision().unwrap() > initial.snapshot_revision);
    let journal = journal.restart();
    let preview = repo
        .inspect_physical_snapshot_recovery(journal.record(), &selected, &context())
        .unwrap();
    let owner = repo
        .recover_physical_snapshot(preview, &context(), &limits, &|preview| {
            assert_eq!(
                preview.recorded_receipt.as_ref().unwrap().file_object,
                initial.file_object
            );
            Ok(PhysicalSnapshotRecoveryAction::ResumePublication)
        })
        .unwrap();
    let receipt = owner
        .acknowledge_published(&context(), &|receipt| journal.commit(receipt), &journal)
        .unwrap();
    assert_eq!(receipt.file_object, initial.file_object);
    assert_eq!(receipt.sha256, initial.sha256);
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn denied_recovery_authorization_does_not_open_writers_or_change_manifest() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 17).unwrap();
    let limits = limits();
    let locator = prepare_small(
        &repo,
        &selected,
        &limits,
        &journal,
        "denied-recovery",
        "denied.sqlite3",
    )
    .release_handles_for_recovery()
    .unwrap();
    let path = manifest_path(&locator);
    let before = fs::read(&path).unwrap();
    let preview = repo
        .inspect_physical_snapshot_recovery(journal.record(), &selected, &context())
        .unwrap();
    let failure = repo
        .recover_physical_snapshot(preview, &context(), &limits, &|_| {
            anyhow::bail!("Durable operation epoch/CAS denied")
        })
        .unwrap_err();
    assert!(failure.owner.is_none());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(limits.stats().0, 1);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
}
#[test]
fn real_cleanup_journal_disk_failure_retains_owner_and_can_retry_without_republishing() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 18).unwrap();
    let limits = limits();
    let published = prepare_small(
        &repo,
        &selected,
        &limits,
        &journal,
        "disk-cleanup-fault",
        "disk.sqlite3",
    )
    .publish(&context(), &journal)
    .unwrap();
    let initial = published.receipt().clone();
    let failure = published
        .acknowledge(
            &context(),
            &|receipt| {
                journal.commit(receipt)?;
                fs::rename(&journal.path, journal.path.with_extension("saved"))?;
                fs::create_dir(&journal.path)?;
                Ok(())
            },
            &journal,
        )
        .unwrap_err();
    assert!(failure.owner.is_some());
    assert_eq!(limits.stats().0, 1);
    assert_eq!(
        Sha256::digest(fs::read(&initial.path).unwrap()).as_slice(),
        hex_digest(&initial.sha256).as_slice()
    );
    fs::remove_dir(&journal.path).unwrap();
    fs::rename(journal.path.with_extension("saved"), &journal.path).unwrap();
    let receipt = failure
        .owner
        .unwrap()
        .acknowledge_published(&context(), &|receipt| journal.commit(receipt), &journal)
        .unwrap();
    assert_eq!(receipt.file_object, initial.file_object);
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn concurrent_commit_keeps_original_snapshot_cut_and_live_new_record() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 21).unwrap();
    let limits = limits();
    let context = context();
    let mut old = track("read-cut");
    old.metadata.insert("cutValue".into(), "old".into());
    repo.upsert_track(&old).unwrap();
    let request = request(&repo, "read-cut", "cut.sqlite3", 1024 * 1024);
    let cut_revision = request.expected_revision;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let prepared = std::thread::scope(|scope| {
        let (worker_repo, worker_selected, worker_context, worker_limits, worker_journal) =
            (&repo, &selected, &context, &limits, &journal);
        let worker = scope.spawn(move || {
            worker_repo.prepare_physical_snapshot_observed(
                request,
                worker_selected,
                worker_context,
                worker_limits,
                &|loc| worker_journal.register(loc),
                &|revision| {
                    ready_tx.send(revision)?;
                    resume_rx.recv_timeout(Duration::from_secs(30))?;
                    Ok(())
                },
            )
        });
        assert_eq!(
            ready_rx.recv_timeout(Duration::from_secs(30)).unwrap(),
            cut_revision
        );
        let mut new = old.clone();
        new.metadata.insert("cutValue".into(), "new".into());
        repo.upsert_track(&new).unwrap();
        assert!(repo.revision().unwrap() > cut_revision);
        resume_tx.send(()).unwrap();
        worker.join().unwrap().unwrap()
    });
    let published = prepared.publish(&context, &journal).unwrap();
    let receipt = published
        .acknowledge(&context, &|r| journal.commit(r), &journal)
        .unwrap();
    let image = Connection::open(&receipt.path).unwrap();
    let copied: String = image
        .query_row(
            "SELECT document FROM tracks WHERE id=?1",
            [&old.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<TrackRecord>(&copied)
            .unwrap()
            .metadata["cutValue"],
        "old"
    );
    let live = Connection::open(repo.path()).unwrap();
    let current: String = live
        .query_row(
            "SELECT document FROM tracks WHERE id=?1",
            [&old.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<TrackRecord>(&current)
            .unwrap()
            .metadata["cutValue"],
        "new"
    );
    assert_eq!(receipt.snapshot_revision, cut_revision);
    assert_checkpoint_released(&repo);
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn cancelled_fixed_read_cut_releases_real_wal_locks_and_preserves_live_commit() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 22).unwrap();
    let limits = limits();
    let context = context();
    let request = request(&repo, "cancel-cut", "cancel-cut.sqlite3", 1024 * 1024);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let failure = std::thread::scope(|scope| {
        let (worker_repo, worker_selected, worker_context, worker_limits, worker_journal) =
            (&repo, &selected, &context, &limits, &journal);
        let worker = scope.spawn(move || {
            worker_repo.prepare_physical_snapshot_observed(
                request,
                worker_selected,
                worker_context,
                worker_limits,
                &|loc| worker_journal.register(loc),
                &|_| {
                    ready_tx.send(())?;
                    resume_rx.recv_timeout(Duration::from_secs(30))?;
                    Ok(())
                },
            )
        });
        ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        repo.upsert_track(&track("committed-during-cut")).unwrap();
        context.cancel();
        resume_tx.send(()).unwrap();
        worker.join().unwrap().unwrap_err()
    });
    assert_eq!(failure.kind, PhysicalSnapshotFailureKind::Cancelled);
    assert_checkpoint_released(&repo);
    failure
        .owner
        .unwrap()
        .cleanup(
            &PhysicalSnapshotContext::new(Duration::from_secs(120)).unwrap(),
            &journal,
        )
        .unwrap();
    assert_eq!(limits.stats(), (0, 0));
    assert!(!directory.join("cancel-cut.sqlite3").exists());
}
#[test]
fn wal_growth_above_64mib_aborts_with_typed_quota_then_releases_read_mark() {
    let (folder, repo) = profile();
    let journal = Journal::new(folder.path());
    let directory = folder.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let selected = PhysicalSnapshotSelection::open_selected(&directory, 23).unwrap();
    let limits = limits();
    let context = context();
    let request = request(&repo, "wal-budget", "wal-budget.sqlite3", 1024 * 1024);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let wal = repo.path().with_file_name(format!(
        "{}-wal",
        repo.path().file_name().unwrap().to_string_lossy()
    ));
    let failure = std::thread::scope(|scope| {
        let (worker_repo, worker_selected, worker_context, worker_limits, worker_journal) =
            (&repo, &selected, &context, &limits, &journal);
        let worker = scope.spawn(move || {
            worker_repo.prepare_physical_snapshot_observed(
                request,
                worker_selected,
                worker_context,
                worker_limits,
                &|loc| worker_journal.register(loc),
                &|_| {
                    ready_tx.send(())?;
                    resume_rx.recv_timeout(Duration::from_secs(30))?;
                    Ok(())
                },
            )
        });
        ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let baseline = fs::metadata(&wal).unwrap().len();
        for index in 0..80 {
            let mut record = track(&format!("wal-{index}"));
            record.metadata.insert(
                "payload".into(),
                serde_json::Value::String("x".repeat(1024 * 1024)),
            );
            repo.upsert_track(&record).unwrap();
            if fs::metadata(&wal).unwrap().len() > baseline + 64 * 1024 * 1024 {
                break;
            }
        }
        assert!(fs::metadata(&wal).unwrap().len() > baseline + 64 * 1024 * 1024);
        resume_tx.send(()).unwrap();
        worker.join().unwrap().unwrap_err()
    });
    assert_eq!(failure.kind, PhysicalSnapshotFailureKind::Quota);
    assert!(failure.message.contains("WAL growth"));
    assert_checkpoint_released(&repo);
    failure
        .owner
        .unwrap()
        .cleanup(
            &PhysicalSnapshotContext::new(Duration::from_secs(120)).unwrap(),
            &journal,
        )
        .unwrap();
    assert!(!directory.join("wal-budget.sqlite3").exists());
    assert_eq!(limits.stats(), (0, 0));
}
