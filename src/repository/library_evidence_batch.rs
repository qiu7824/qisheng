//! Atomic content evidence for one acknowledged library scan batch.

use super::{Repository, decode_document, encode_document, library, library_binding, schema};
use crate::domain::{
    library::{
        FileIdentity, LibraryError, LibraryScanToken, MAX_LIBRARY_BATCH_BYTES,
        MAX_LIBRARY_BATCH_ITEMS, validate_library_id,
    },
    library_watch::validate_content_sha256,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::BTreeSet;

/// Metadata binds the committed representation. Its corresponding live lease
/// remains with the caller and is checked through the indexed callback.
#[derive(Clone, Debug, Serialize)]
pub struct LibraryContentEvidenceCommit {
    pub asset_id: String,
    pub expected_binding_revision: u64,
    pub file_identity: FileIdentity,
    pub size_bytes: u64,
    pub modified_ms: Option<u64>,
    pub content_sha256: String,
}

impl Repository {
    pub fn put_library_content_evidence_batch(
        &self,
        token: &LibraryScanToken,
        records: &[LibraryContentEvidenceCommit],
        check: &dyn Fn() -> Result<()>,
        check_record: &dyn Fn(usize) -> Result<()>,
    ) -> Result<()> {
        token.validate()?;
        validate_records(records)?;
        let check_all = || -> Result<()> {
            check()?;
            for index in 0..records.len() {
                check()?;
                check_record(index)?;
            }
            Ok(())
        };
        // Preserve every lease check across the lock and final commit boundary.
        check_all()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        check_all()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (root, _) = library::require_token(&transaction, token)?;
        let scope = library_binding::scope_at(&transaction, token)?;
        for (index, record) in records.iter().enumerate() {
            check()?;
            check_record(index)?;
            let binding = library_binding::binding_at(&transaction, &record.asset_id)?
                .context("Evidence asset missing")?;
            ensure!(
                scope.contains(&root, &binding.path),
                LibraryError::conflict("Content evidence lies outside its scan scope")
            );
            let covered: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_membership m JOIN library_scan_seen s ON s.root_id=m.root_id AND s.asset_id=m.asset_id AND s.generation=m.last_seen_generation WHERE m.root_id=?1 AND m.asset_id=?2 AND m.last_seen_generation=?3)",
                params![token.root_id, record.asset_id, token.generation as i64], |row| row.get(0))?;
            ensure!(
                covered,
                LibraryError::conflict(
                    "Content evidence was not covered by its committed scan batch"
                )
            );
            update_content_evidence_at(&transaction, record)?;
        }
        check_all()?;
        transaction.commit()?;
        Ok(())
    }
}

pub(super) fn update_content_evidence_at(
    transaction: &Transaction<'_>,
    record: &LibraryContentEvidenceCommit,
) -> Result<()> {
    validate_record(record)?;
    let binding = library_binding::binding_at(transaction, &record.asset_id)?
        .context("Evidence asset missing")?;
    let asset =
        library::asset_at(transaction, &record.asset_id)?.context("Evidence asset missing")?;
    ensure!(
        binding.binding_revision == record.expected_binding_revision
            && binding.file_identity.as_ref() == Some(&record.file_identity)
            && asset.size_bytes == record.size_bytes
            && asset.modified_ms == record.modified_ms,
        LibraryError::conflict("Content evidence representation changed")
    );
    let document: String = transaction.query_row(
        "SELECT document FROM local_asset_locations WHERE asset_id=?1",
        [&record.asset_id],
        |row| row.get(0),
    )?;
    let mut location: library::AssetLocation = decode_document(&document)?;
    location.content_sha256 = Some(record.content_sha256.to_ascii_lowercase());
    location.hash_size_bytes = Some(record.size_bytes);
    location.hash_modified_ms = record.modified_ms;
    ensure!(
        transaction.execute(
            "UPDATE local_asset_locations SET document=?2 WHERE asset_id=?1",
            params![record.asset_id, encode_document(&location)?]
        )? == 1,
        LibraryError::conflict("Content evidence asset disappeared")
    );
    Ok(())
}

fn validate_record(record: &LibraryContentEvidenceCommit) -> Result<()> {
    validate_library_id(&record.asset_id)?;
    ensure!(
        record.expected_binding_revision > 0 && record.expected_binding_revision <= i64::MAX as u64,
        LibraryError::invalid("Content evidence binding revision is invalid")
    );
    record.file_identity.validate()?;
    validate_content_sha256(&record.content_sha256)?;
    Ok(())
}
fn validate_records(records: &[LibraryContentEvidenceCommit]) -> Result<()> {
    ensure!(
        records.len() <= MAX_LIBRARY_BATCH_ITEMS,
        LibraryError::budget("Content evidence exceeds 200 items")
    );
    let mut ids = BTreeSet::new();
    let mut bytes = 2usize;
    for record in records {
        validate_record(record)?;
        ensure!(
            ids.insert(&record.asset_id),
            LibraryError::invalid("Content evidence repeats an asset")
        );
        bytes = bytes
            .checked_add(serde_json::to_vec(record)?.len() + 1)
            .context("Content evidence byte count overflow")?;
        ensure!(
            bytes <= MAX_LIBRARY_BATCH_BYTES,
            LibraryError::budget("Content evidence exceeds 1 MiB")
        );
    }
    Ok(())
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    use crate::{
        domain::library::{LibraryPreparedAsset, LibrarySeenFile},
        library::{
            authority::{AuthorityRegistry, AuthorizedFile, RootAuthority},
            scan::ScanContext,
        },
        repository::local_track_from_path,
    };
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    struct Fixture {
        repository: Repository,
        registry: AuthorityRegistry,
        files: Vec<AuthorizedFile>,
        token: LibraryScanToken,
        records: Vec<LibraryContentEvidenceCommit>,
        context: ScanContext,
        _profile: tempfile::TempDir,
        _media: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            let profile = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
            let media = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
            let context = ScanContext::new(Duration::from_secs(30));
            let authority = RootAuthority::open(media.path(), true, &context).unwrap();
            let repository = Repository::open(profile.path()).unwrap();
            let (root, _) = repository.add_library_root(&authority.draft()).unwrap();
            let (token, _) = repository
                .begin_library_scan(&root.root_id, "evidence-batch")
                .unwrap();
            let mut items = Vec::new();
            let mut seen = Vec::new();
            for index in 0..2 {
                let path = media.path().join(format!("actual-{index}.wav"));
                let mut body = Vec::new();
                body.extend_from_slice(b"RIFF");
                body.extend_from_slice(&52u32.to_le_bytes());
                body.extend_from_slice(b"WAVEfmt ");
                body.extend_from_slice(&16u32.to_le_bytes());
                body.extend_from_slice(&1u16.to_le_bytes());
                body.extend_from_slice(&1u16.to_le_bytes());
                body.extend_from_slice(&8000u32.to_le_bytes());
                body.extend_from_slice(&16000u32.to_le_bytes());
                body.extend_from_slice(&2u16.to_le_bytes());
                body.extend_from_slice(&16u16.to_le_bytes());
                body.extend_from_slice(b"data");
                body.extend_from_slice(&16u32.to_le_bytes());
                body.extend_from_slice(&[0u8; 16]);
                fs::write(&path, body).unwrap();
                let opened = authority.open_file(&path).unwrap();
                let (track, asset) = local_track_from_path(&path).unwrap();
                seen.push(LibrarySeenFile {
                    path: asset.path.clone(),
                    asset_id: Some(asset.id.clone()),
                });
                items.push(LibraryPreparedAsset {
                    track,
                    asset,
                    file_identity: Some(opened.identity().clone()),
                });
            }
            repository
                .apply_library_scan_batch(&token, &seen, &items, &|| context.check())
                .unwrap();
            let registry = AuthorityRegistry::default();
            let mut files = Vec::new();
            let mut records = Vec::new();
            for item in items {
                let binding = repository
                    .library_asset_binding(&item.asset.id)
                    .unwrap()
                    .unwrap();
                let permissions = repository
                    .local_asset_authority(&item.asset.id)
                    .unwrap()
                    .unwrap();
                let lease = registry
                    .install(&permissions, item.file_identity.clone())
                    .unwrap();
                let opened = lease.open().unwrap();
                records.push(LibraryContentEvidenceCommit {
                    asset_id: item.asset.id,
                    expected_binding_revision: binding.binding_revision,
                    file_identity: opened.identity().clone(),
                    size_bytes: item.asset.size_bytes,
                    modified_ms: item.asset.modified_ms,
                    content_sha256: format!(
                        "{:x}",
                        Sha256::digest(fs::read(opened.path()).unwrap())
                    ),
                });
                files.push(opened);
            }
            let retired = authority.retirement_barrier();
            drop(authority);
            assert!(retired.wait(Duration::from_secs(3)));
            Self {
                repository,
                registry,
                files,
                token,
                records,
                context,
                _profile: profile,
                _media: media,
            }
        }
        fn commit(&self, records: &[LibraryContentEvidenceCommit]) -> Result<()> {
            self.repository.put_library_content_evidence_batch(
                &self.token,
                records,
                &|| self.context.check(),
                &|index| self.files[index].check(),
            )
        }
        fn assert_no_hashes(&self) {
            for record in &self.records {
                assert!(
                    self.repository
                        .library_asset_binding(&record.asset_id)
                        .unwrap()
                        .unwrap()
                        .content_sha256
                        .is_none()
                );
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.files.clear();
            self.registry.begin_shutdown();
            if !std::thread::panicking() {
                assert!(
                    self.registry
                        .retirement_barrier()
                        .wait(Duration::from_secs(3))
                );
            }
        }
    }

    #[test]
    fn real_file_batch_persists_all_hashes_without_user_revision_or_outbox() {
        let fixture = Fixture::new();
        let revision = fixture.repository.revision().unwrap();
        fixture.commit(&fixture.records).unwrap();
        for record in &fixture.records {
            let binding = fixture
                .repository
                .library_asset_binding(&record.asset_id)
                .unwrap()
                .unwrap();
            assert_eq!(binding.binding_revision, record.expected_binding_revision);
            assert_eq!(binding.file_identity, Some(record.file_identity.clone()));
            assert_eq!(
                binding.content_sha256.as_deref(),
                Some(record.content_sha256.as_str())
            );
        }
        assert_eq!(fixture.repository.revision().unwrap(), revision);
        assert!(
            fixture
                .repository
                .changes_since(revision, 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn second_representation_cas_failure_rolls_back_first_evidence_row() {
        for field in 0..4 {
            let fixture = Fixture::new();
            let mut records = fixture.records.clone();
            match field {
                0 => records[1].expected_binding_revision += 1,
                1 => records[1].file_identity = records[0].file_identity.clone(),
                2 => records[1].size_bytes += 1,
                3 => records[1].modified_ms = Some(records[1].modified_ms.unwrap_or(0) + 1),
                _ => unreachable!(),
            }
            assert!(fixture.commit(&records).is_err());
            fixture.assert_no_hashes();
        }
    }

    #[test]
    fn second_sql_write_failure_rolls_back_first_evidence_row() {
        let fixture = Fixture::new();
        let database = Connection::open(fixture.repository.path()).unwrap();
        database.execute_batch(&format!("CREATE TRIGGER fail_second_evidence BEFORE UPDATE ON local_asset_locations WHEN NEW.asset_id='{}' BEGIN SELECT RAISE(ABORT,'injected second evidence failure'); END;", fixture.records[1].asset_id)).unwrap();
        assert!(fixture.commit(&fixture.records).is_err());
        fixture.assert_no_hashes();
    }

    #[test]
    fn actual_lease_revocation_before_commit_rolls_back_every_evidence_row() {
        let fixture = Fixture::new();
        let calls = AtomicUsize::new(0);
        let result = fixture.repository.put_library_content_evidence_batch(
            &fixture.token,
            &fixture.records,
            &|| fixture.context.check(),
            &|index| {
                if calls.fetch_add(1, Ordering::SeqCst) == 6 {
                    fixture.registry.revoke_root(&fixture.token.root_id);
                }
                fixture.files[index].check()
            },
        );
        assert!(result.is_err());
        fixture.assert_no_hashes();
    }

    #[test]
    fn precommit_scan_cancel_and_stale_or_missing_membership_reject_atomically() {
        let fixture = Fixture::new();
        let calls = AtomicUsize::new(0);
        let result = fixture.repository.put_library_content_evidence_batch(
            &fixture.token,
            &fixture.records,
            &|| fixture.context.check(),
            &|index| {
                if calls.fetch_add(1, Ordering::SeqCst) == 6 {
                    fixture.context.cancel.store(true, Ordering::Release);
                }
                fixture.files[index].check()
            },
        );
        assert!(result.is_err());
        fixture.assert_no_hashes();
        for stale in [true, false] {
            let fixture = Fixture::new();
            let database = Connection::open(fixture.repository.path()).unwrap();
            if stale {
                database
                    .execute(
                        "UPDATE library_scan_runs SET state='cancelled' WHERE operation_id=?1",
                        [&fixture.token.operation_id],
                    )
                    .unwrap();
            } else {
                database
                    .execute(
                        "DELETE FROM library_membership WHERE asset_id=?1",
                        [&fixture.records[1].asset_id],
                    )
                    .unwrap();
            }
            assert!(fixture.commit(&fixture.records).is_err());
            fixture.assert_no_hashes();
        }
    }

    #[test]
    fn batch_admission_rejects_repeated_assets_and_two_hundred_and_first_record() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .commit(&[fixture.records[0].clone(), fixture.records[0].clone()])
                .is_err()
        );
        let mut records = Vec::new();
        for index in 0..201 {
            let mut record = fixture.records[0].clone();
            record.asset_id = format!("evidence-admission-{index}");
            records.push(record);
        }
        let error = fixture.commit(&records).unwrap_err();
        assert_eq!(
            error.downcast_ref::<LibraryError>().unwrap().kind,
            crate::domain::library::LibraryErrorKind::BudgetExceeded
        );
        let records: Vec<_> = (0..200)
            .map(|index| {
                let mut record = fixture.records[0].clone();
                record.asset_id = format!("{}-{index}", "\u{0001}".repeat(1000));
                record
            })
            .collect();
        let error = fixture.commit(&records).unwrap_err();
        assert_eq!(
            error.downcast_ref::<LibraryError>().unwrap().kind,
            crate::domain::library::LibraryErrorKind::BudgetExceeded
        );
        fixture.assert_no_hashes();
    }
}
