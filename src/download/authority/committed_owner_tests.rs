use super::*;
use crate::download::{AudioVerifier, DownloadContext, DownloadExecutionLease, SymphoniaVerifier};
use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
use windows_sys::Win32::{
    Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation},
    Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE},
};

fn authored_wave() -> Vec<u8> {
    let samples = 800u32;
    let mut body = Vec::new();
    body.extend_from_slice(b"RIFF");
    body.extend_from_slice(&(36 + samples * 2).to_le_bytes());
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&8000u32.to_le_bytes());
    body.extend_from_slice(&16000u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(samples * 2).to_le_bytes());
    for index in 0..samples {
        body.extend_from_slice(&if index % 16 < 8 { 8192i16 } else { -8192i16 }.to_le_bytes());
    }
    body
}
struct Case {
    _directory: tempfile::TempDir,
    destination: PathBuf,
    grant: DestinationGrant,
    body: Vec<u8>,
    audio: VerifiedOwnedAudio,
    lease: DownloadExecutionLease,
    quota: crate::download::quota::PartialQuota,
    record: OwnedPartRecord,
}
fn case() -> Case {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("output");
    std::fs::create_dir(&destination).unwrap();
    let grant = DestinationGrant::open_selected(&destination, 1).unwrap();
    let context = DownloadContext {
        profile_id: "owner-profile".into(),
        task_id: "owner-task".into(),
        operation_epoch: 1,
        source_id: "authored-source".into(),
        source_content_sha256: "ab".repeat(32),
        policy_epoch: 1,
    };
    let lease = DownloadExecutionLease::from_approved_context(context.clone()).unwrap();
    let cancel = DownloadCancel::default();
    let body = authored_wave();
    let mut part =
        OwnedPart::create(grant.plan("authored.wav", CollisionPolicy::Skip).unwrap()).unwrap();
    let quota = crate::download::quota::PartialQuota::default();
    part.attach_quota(quota.clone()).unwrap();
    part.bind_execution(lease.clone(), context.clone(), cancel.clone());
    part.append(&body, &cancel, MAX_DOWNLOAD_BYTES).unwrap();
    let verification = SymphoniaVerifier.verify(&part, "wav", &cancel).unwrap();
    assert_eq!(verification.decoded_frames, 800);
    let checkpoint = part
        .checkpoint(
            &cancel,
            Some("\"authored\"".into()),
            Some(body.len() as u64),
            true,
        )
        .unwrap();
    let record = part.record().clone();
    let audio = part
        .into_verified(checkpoint, verification, lease.clone(), context, cancel)
        .unwrap();
    Case {
        _directory: directory,
        destination,
        grant,
        body,
        audio,
        lease,
        quota,
        record,
    }
}
fn assert_actual_fences(path: &Path, directory: &Path) {
    let error = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(32));
    assert!(std::fs::remove_file(path).is_err());
    assert!(std::fs::rename(directory, directory.with_extension("moved")).is_err());
}

#[test]
fn actual_published_object_keeps_original_file_and_directory_until_repository_ack() {
    let Case {
        _directory,
        destination,
        grant,
        body,
        audio,
        lease,
        quota,
        record,
    } = case();
    let mut published = audio.commit_owned().unwrap();
    let facts = published.facts();
    assert_eq!(facts.identity, record.identity);
    assert_eq!(std::fs::read(&facts.path).unwrap(), body);
    assert!(!destination.join(&record.basename).exists());
    assert_eq!(quota.bytes(), body.len() as u64);
    assert_actual_fences(&facts.path, &destination);
    assert!(
        published
            .acknowledge_repository_commit(|_| anyhow::bail!("ACK failure"))
            .is_err()
    );
    assert_eq!(quota.bytes(), body.len() as u64);
    assert_actual_fences(&facts.path, &destination);
    published
        .acknowledge_repository_commit(|observed| {
            assert_eq!(observed.identity, facts.identity);
            assert_eq!(
                observed.checkpoint.sha256,
                format!("{:x}", Sha256::digest(&body))
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(quota.bytes(), 0);
    lease.revoke();
    let anchor = published.into_read_anchor().unwrap();
    anchor.verify().unwrap();
    let mut reader = anchor.try_clone_reader().unwrap();
    reader.rewind().unwrap();
    let mut observed = Vec::new();
    reader.read_to_end(&mut observed).unwrap();
    assert_eq!(observed, body);
    assert_eq!(reader.write(b"x").unwrap_err().raw_os_error(), Some(5));
    assert_eq!(reader.set_len(1).unwrap_err().raw_os_error(), Some(5));
    drop(reader);
    assert_actual_fences(&facts.path, &destination);
    let mut retiring = anchor.into_retirement();
    drop(grant);
    retiring.try_close().unwrap();
    assert!(retiring.is_closed());
    std::fs::OpenOptions::new()
        .write(true)
        .open(&facts.path)
        .unwrap();
    std::fs::rename(&destination, destination.with_extension("moved")).unwrap();
}

#[test]
fn source_revocation_before_native_publication_returns_the_original_partial_owner() {
    let Case {
        _directory,
        destination,
        grant: _,
        body,
        audio,
        lease,
        quota,
        record,
    } = case();
    lease.revoke();
    let failure = audio.commit_owned().unwrap_err();
    assert_eq!(failure.phase(), DownloadPublicationPhase::OwnedPartial);
    assert_eq!(failure.record(), &record);
    assert!(failure.cause().to_string().contains("revoked"));
    assert_eq!(
        std::fs::read(destination.join(&record.basename)).unwrap(),
        body
    );
    assert!(!destination.join(&record.final_basename).exists());
    assert_eq!(quota.bytes(), body.len() as u64);
    assert_actual_fences(&destination.join(&record.basename), &destination);
    assert!(failure.recover_partial().is_err());
}

#[test]
fn failure_after_actual_rename_keeps_the_same_file_for_recovery_and_local_read() {
    let Case {
        _directory,
        destination,
        grant,
        body,
        audio,
        lease,
        quota,
        record,
    } = case();
    let failure = audio
        .commit_owned_guarded(&|| {
            lease.revoke();
            anyhow::bail!("Failure after actual rename")
        })
        .unwrap_err();
    assert_eq!(
        failure.phase(),
        DownloadPublicationPhase::PublicationAttempted
    );
    assert!(!destination.join(&record.basename).exists());
    let output = destination.join(&record.final_basename);
    assert_eq!(std::fs::read(&output).unwrap(), body);
    assert_actual_fences(&output, &destination);
    assert_eq!(quota.bytes(), body.len() as u64);
    let mut recovered = failure.recover_published().unwrap();
    assert_eq!(recovered.record().identity, record.identity);
    recovered.acknowledge_repository_commit(|_| Ok(())).unwrap();
    let anchor = recovered.into_read_anchor().unwrap();
    anchor.verify().unwrap();
    assert_eq!(quota.bytes(), 0);
    let mut retirement = anchor.into_retirement();
    drop(grant);
    retirement.try_close().unwrap();
}

#[test]
fn a_real_final_name_collision_keeps_the_foreign_body_and_original_partial() {
    let Case {
        _directory,
        destination,
        grant: _,
        body,
        audio,
        lease: _,
        quota,
        record,
    } = case();
    std::fs::write(destination.join(&record.final_basename), b"foreign").unwrap();
    let failure = audio.commit_owned().unwrap_err();
    assert_eq!(
        failure.phase(),
        DownloadPublicationPhase::PublicationAttempted
    );
    assert_eq!(
        std::fs::read(destination.join(&record.final_basename)).unwrap(),
        b"foreign"
    );
    assert_eq!(
        std::fs::read(destination.join(&record.basename)).unwrap(),
        body
    );
    assert_eq!(quota.bytes(), body.len() as u64);
    let audio = failure.recover_partial().unwrap();
    assert_eq!(audio.owned_record(), &record);
    assert_actual_fences(&destination.join(&record.basename), &destination);
}

#[test]
fn unacknowledged_publication_cannot_create_a_read_grant_and_retains_the_native_file() {
    let Case {
        _directory,
        destination,
        grant: _,
        body: _,
        audio,
        lease: _,
        quota,
        record,
    } = case();
    let published = audio.commit_owned().unwrap();
    let failure = published.into_read_anchor().err().expect("ACK required");
    assert_eq!(failure.phase(), DownloadPublicationPhase::Published);
    assert!(quota.bytes() > 0);
    assert_actual_fences(&destination.join(&record.final_basename), &destination);
    assert!(failure.recover_published().is_ok());
}

#[test]
fn destination_revocation_denies_read_adoption_without_losing_the_original_owner() {
    let Case {
        _directory,
        destination,
        grant,
        body: _,
        audio,
        lease: _,
        quota: _,
        record,
    } = case();
    let mut published = audio.commit_owned().unwrap();
    published.acknowledge_repository_commit(|_| Ok(())).unwrap();
    grant.revoke();
    let failure = published
        .into_read_anchor()
        .err()
        .expect("Revoked destination");
    assert_eq!(
        failure.phase(),
        DownloadPublicationPhase::RepositoryAcknowledged
    );
    assert_actual_fences(&destination.join(&record.final_basename), &destination);
    assert!(failure.recover_published().is_err());
}

#[test]
fn actual_close_failure_retains_that_handle_and_directory_until_the_retry_succeeds() {
    let Case {
        _directory,
        destination,
        grant,
        body: _,
        audio,
        lease: _,
        quota: _,
        record,
    } = case();
    let mut published = audio.commit_owned().unwrap();
    published.acknowledge_repository_commit(|_| Ok(())).unwrap();
    let mut retirement = published.into_read_anchor().unwrap().into_retirement();
    let raw = retirement.files.last().unwrap().as_raw_handle();
    assert_ne!(
        unsafe {
            SetHandleInformation(
                raw,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        },
        0
    );
    assert!(retirement.try_close().is_err());
    assert!(!retirement.is_closed());
    assert_eq!(retirement.files.last().unwrap().as_raw_handle(), raw);
    assert_actual_fences(&destination.join(&record.final_basename), &destination);
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    drop(grant);
    retirement.try_close().unwrap();
    assert!(retirement.is_closed());
    std::fs::rename(&destination, destination.with_extension("moved")).unwrap();
}

#[test]
fn shared_actual_destination_keeps_retirement_pending_until_its_last_user_releases_it() {
    let Case {
        _directory,
        destination,
        grant,
        body: _,
        audio,
        lease: _,
        quota: _,
        record: _,
    } = case();
    let mut published = audio.commit_owned().unwrap();
    published.acknowledge_repository_commit(|_| Ok(())).unwrap();
    let mut retirement = published.into_read_anchor().unwrap().into_retirement();
    assert!(retirement.try_close().is_err());
    assert!(retirement.files.is_empty());
    assert!(!retirement.is_closed());
    assert!(std::fs::rename(&destination, destination.with_extension("moved")).is_err());
    drop(grant);
    retirement.try_close().unwrap();
    assert!(retirement.is_closed());
    std::fs::rename(&destination, destination.with_extension("moved")).unwrap();
}

#[test]
fn actual_destination_close_failure_retains_the_same_pin_until_native_retry() {
    let Case {
        _directory,
        destination,
        grant,
        body: _,
        audio,
        lease: _,
        quota: _,
        record: _,
    } = case();
    let mut published = audio.commit_owned().unwrap();
    published.acknowledge_repository_commit(|_| Ok(())).unwrap();
    let mut retirement = published.into_read_anchor().unwrap().into_retirement();
    drop(grant);
    let raw = retirement
        .grant
        .as_ref()
        .unwrap()
        .0
        .pins
        .last()
        .unwrap()
        .as_raw_handle();
    assert_ne!(
        unsafe {
            SetHandleInformation(
                raw,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        },
        0
    );
    assert!(retirement.try_close().is_err());
    assert!(!retirement.is_closed());
    assert_eq!(
        retirement
            .grant
            .as_ref()
            .unwrap()
            .0
            .pins
            .last()
            .unwrap()
            .as_raw_handle(),
        raw
    );
    assert!(std::fs::rename(&destination, destination.with_extension("moved")).is_err());
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    retirement.try_close().unwrap();
    assert!(retirement.is_closed());
    std::fs::rename(&destination, destination.with_extension("moved")).unwrap();
}

#[test]
fn failed_publication_shutdown_closes_actual_handles_without_deleting_or_releasing_artifact_bytes()
{
    let Case {
        _directory,
        destination,
        grant,
        body,
        audio,
        lease,
        quota,
        record,
    } = case();
    lease.revoke();
    let failure = audio.commit_owned().unwrap_err();
    let mut retirement = failure.into_retirement();
    drop(grant);
    retirement.try_close().unwrap();
    assert!(retirement.is_closed());
    assert_eq!(
        std::fs::read(destination.join(&record.basename)).unwrap(),
        body
    );
    assert!(!destination.join(&record.final_basename).exists());
    assert_eq!(quota.bytes(), body.len() as u64);
    std::fs::OpenOptions::new()
        .write(true)
        .open(destination.join(&record.basename))
        .unwrap();
    std::fs::rename(&destination, destination.with_extension("moved")).unwrap();
}
