use super::*;
use crate::library::authority::AuthorityRegistry;
use std::time::Duration;

fn wave(frames: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    let data = (frames * 2) as u32;
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&32000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for index in 0..frames {
        bytes.extend_from_slice(&(if index == 0 { 16384i16 } else { 0 }).to_le_bytes());
    }
    bytes
}
fn cancelled(temp: &tempfile::TempDir, half: bool) -> (ImpulseVault, ImpulseImportFact) {
    let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
    let mut vault = ImpulseVault::create(&profile).unwrap();
    let receipt = vault.receipt().clone();
    vault.arm(&receipt).unwrap();
    let path = temp.path().join("selected.wav");
    fs::write(&path, wave(64000)).unwrap();
    let selected = AuthorityRegistry::default().open_selection(&path).unwrap();
    let context = ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap();
    if half {
        vault.cancel_after_write = Some(64 * 1024);
    }
    let facts = Mutex::new(Vec::new());
    assert!(
        vault
            .import_selected(&selected, "wav", &context, &|fact| {
                facts.lock().unwrap().push(fact.clone());
                if !half && fact.phase == ImpulseImportPhase::Writing {
                    context.cancel();
                }
                Ok(())
            })
            .is_err()
    );
    let fact = facts.into_inner().unwrap().last().unwrap().clone();
    assert_eq!(fact.phase, ImpulseImportPhase::Writing);
    let retained = vault.pending.lock().unwrap();
    let original = retained.get(&fact.token).unwrap().file.as_ref().unwrap();
    assert!(
        fact.body_identity
            .as_ref()
            .unwrap()
            .matches(&platform::identity(original).unwrap())
    );
    if half {
        assert!(original.metadata().unwrap().len() > HEADER_BYTES as u64);
        assert!(original.metadata().unwrap().len() < fact.reserved_bytes);
    } else {
        assert_eq!(original.metadata().unwrap().len(), 0);
    }
    drop(retained);
    (vault, fact)
}

#[test]
fn actual_cancelled_zero_and_half_written_producers_retire_then_release_and_reimport() {
    for half in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (mut vault, fact) = cancelled(&temp, half);
        assert!(vault.disk_usage().unwrap().1 >= fact.reserved_bytes);
        let mut owner = vault
            .prepare_import_abort(
                &fact,
                None,
                &ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap(),
                &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
            )
            .unwrap();
        let records = Mutex::new(Vec::new());
        assert!(
            owner
                .attempt(
                    &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
                    &|record| {
                        records.lock().unwrap().push(record.clone());
                        Ok(())
                    }
                )
                .unwrap()
        );
        assert!(records.lock().unwrap().last().unwrap().removed());
        assert!(
            !vault
                .directory
                .path()
                .join(impulse_basename(&fact.token))
                .exists()
        );
        // Native completion alone retains the original reservation until the
        // coordinator acknowledges removal and joins its actual worker.
        assert!(vault.disk_usage().unwrap().1 >= fact.reserved_bytes);
        vault.release_import_reservation(&fact.token).unwrap();
        assert_eq!(vault.disk_usage().unwrap(), (1, OWNER_BYTES as u64));
        vault.cancel_after_write = None;
        let path = temp.path().join("selected.wav");
        let selected = AuthorityRegistry::default().open_selection(&path).unwrap();
        assert!(
            vault
                .import_selected(
                    &selected,
                    "wav",
                    &ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap(),
                    &|_| Ok(())
                )
                .is_ok()
        );
    }
}
#[test]
fn failed_abort_ack_retains_same_native_owner_and_retry_reclaims_original() {
    let temp = tempfile::tempdir().unwrap();
    let (mut vault, fact) = cancelled(&temp, true);
    let before = fs::read(vault.directory.path().join(impulse_basename(&fact.token))).unwrap();
    let mut owner = vault
        .prepare_import_abort(
            &fact,
            None,
            &ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap(),
            &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
        )
        .unwrap();
    assert!(
        owner
            .attempt(
                &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
                &|_| anyhow::bail!("Injected actor ACK failure")
            )
            .is_err()
    );
    assert_eq!(
        fs::read(vault.directory.path().join(impulse_basename(&fact.token))).unwrap(),
        before
    );
    assert!(
        owner
            .failure
            .as_ref()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .metadata()
            .identity
            .as_ref()
            .is_some_and(|identity| fact.body_identity.as_ref().unwrap().matches(identity))
    );
    let removed = Mutex::new(false);
    assert!(
        owner
            .attempt(
                &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
                &|record| {
                    *removed.lock().unwrap() = record.removed();
                    Ok(())
                }
            )
            .unwrap()
    );
    assert!(*removed.lock().unwrap());
    vault.release_import_reservation(&fact.token).unwrap();
    assert_eq!(vault.disk_usage().unwrap(), (1, OWNER_BYTES as u64));
}
#[test]
fn cold_abort_requires_fresh_actual_selection_and_preserves_foreign_fid_body() {
    let temp = tempfile::tempdir().unwrap();
    let (vault, fact) = cancelled(&temp, true);
    let receipt = vault.receipt().clone();
    let path = vault.directory.path().join(impulse_basename(&fact.token));
    let original = fs::read(&path).unwrap();
    drop(vault);
    let profile = SnapshotDirectory::pin_profile(temp.path(), 2).unwrap();
    let mut cold = ImpulseVault::reopen(&profile, &receipt).unwrap();
    assert!(
        cold.prepare_import_abort(
            &fact,
            None,
            &ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap(),
            &SnapshotContext::new(Duration::from_secs(30)).unwrap()
        )
        .is_err()
    );
    fs::remove_file(&path).unwrap();
    fs::write(&path, &original).unwrap();
    let replacement = AuthorityRegistry::default().open_selection(&path).unwrap();
    assert!(
        cold.prepare_import_abort(
            &fact,
            Some(replacement),
            &ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap(),
            &SnapshotContext::new(Duration::from_secs(30)).unwrap()
        )
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
}
#[test]
fn cold_attested_partial_selection_can_retire_the_exact_original() {
    let temp = tempfile::tempdir().unwrap();
    let (vault, fact) = cancelled(&temp, true);
    let receipt = vault.receipt().clone();
    let path = vault.directory.path().join(impulse_basename(&fact.token));
    drop(vault);
    let profile = SnapshotDirectory::pin_profile(temp.path(), 2).unwrap();
    let mut cold = ImpulseVault::reopen(&profile, &receipt).unwrap();
    let selected = AuthorityRegistry::default().open_selection(&path).unwrap();
    let mut owner = cold
        .prepare_import_abort(
            &fact,
            Some(selected),
            &ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap(),
            &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
        )
        .unwrap();
    assert!(
        owner
            .attempt(
                &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
                &|_| Ok(())
            )
            .unwrap()
    );
    cold.release_import_reservation(&fact.token).unwrap();
    assert!(!path.exists());
}
