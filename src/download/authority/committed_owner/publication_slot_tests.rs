use super::*;
use crate::download::{AudioVerifier, DownloadContext, DownloadExecutionLease, SymphoniaVerifier};
use std::{cell::RefCell, os::windows::fs::OpenOptionsExt, sync::mpsc, time::Instant};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

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
        profile_id: "slot-profile".into(),
        task_id: "slot-task".into(),
        operation_epoch: 1,
        source_id: "authored-source".into(),
        source_content_sha256: "cd".repeat(32),
        policy_epoch: 1,
    };
    let lease = DownloadExecutionLease::from_approved_context(context.clone()).unwrap();
    let cancel = DownloadCancel::default();
    let mut body = Vec::new();
    body.extend_from_slice(b"RIFF");
    body.extend_from_slice(&1636u32.to_le_bytes());
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&8000u32.to_le_bytes());
    body.extend_from_slice(&16000u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&1600u32.to_le_bytes());
    for index in 0..800 {
        body.extend_from_slice(&if index % 16 < 8 { 8192i16 } else { -8192i16 }.to_le_bytes());
    }
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
            Some("\"slot-wave\"".into()),
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
fn wait_joined(slot: &DownloadPublicationSlot) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !slot.is_joined().unwrap() {
        assert!(
            Instant::now() < deadline,
            "Actual publication worker did not join"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn assert_fence(path: &Path, directory: &Path) {
    let error = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(32));
    assert!(std::fs::rename(directory, directory.with_extension("moved")).is_err());
}

#[test]
fn actual_worker_panic_after_rename_preserves_original_slot_before_explicit_same_file_recovery() {
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
    let mut slot =
        DownloadPublicationSlot::admit(audio).unwrap_or_else(|failure| panic!("{}", failure.cause));
    slot.start_guarded(
        Box::new(|| Ok(())),
        Box::new(|| panic!("After actual native rename")),
        |_| {},
    )
    .unwrap();
    wait_joined(&slot);
    assert!(slot.worker_panicked().unwrap());
    assert!(!destination.join(&record.basename).exists());
    let final_path = destination.join(&record.final_basename);
    assert_eq!(std::fs::read(&final_path).unwrap(), body);
    assert_fence(&final_path, &destination);
    assert_eq!(quota.bytes(), body.len() as u64);
    assert!(
        slot.acknowledge_repository_after_join(|_, _| Ok(()))
            .is_err()
    );
    lease.revoke();
    slot.recover_published_after_join().unwrap();
    slot.acknowledge_repository_after_join(|key, actual| {
        assert_eq!(key.candidate_nonce, record.nonce);
        assert_eq!(actual.identity, record.identity);
        assert_eq!(
            actual.checkpoint.sha256,
            format!("{:x}", Sha256::digest(&body))
        );
        Ok(())
    })
    .unwrap();
    let published = slot.handoff_acknowledged().unwrap();
    let anchor = published.into_read_anchor().unwrap();
    anchor.verify().unwrap();
    let mut retirement = anchor.into_retirement();
    drop(grant);
    retirement.try_close().unwrap();
    assert_eq!(quota.bytes(), 0);
}

struct TeardownGate {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for TeardownGate {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
    }
}
thread_local! { static TEARDOWN: RefCell<Option<TeardownGate>> = const { RefCell::new(None) }; }

#[test]
fn completion_notification_does_not_allow_ack_before_actual_tls_teardown_and_join() {
    let Case {
        _directory,
        destination,
        grant,
        body,
        audio,
        lease: _,
        quota,
        record,
    } = case();
    let mut slot =
        DownloadPublicationSlot::admit(audio).unwrap_or_else(|failure| panic!("{}", failure.cause));
    let (entered, tearing_down) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let (notified, notification) = mpsc::sync_channel(1);
    slot.start_guarded(
        Box::new(move || {
            TEARDOWN.with(|gate| {
                *gate.borrow_mut() = Some(TeardownGate {
                    entered,
                    release: released,
                })
            });
            Ok(())
        }),
        Box::new(|| Ok(())),
        move |key| {
            notified.send(key).unwrap();
        },
    )
    .unwrap();
    let actual_key = notification.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(actual_key, *slot.key());
    tearing_down.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(!slot.is_joined().unwrap());
    assert!(
        slot.acknowledge_repository_after_join(|_, _| Ok(()))
            .is_err()
    );
    assert_eq!(quota.bytes(), body.len() as u64);
    assert_fence(&destination.join(&record.final_basename), &destination);
    release.send(()).unwrap();
    wait_joined(&slot);
    slot.acknowledge_repository_after_join(|_, _| Ok(()))
        .unwrap();
    drop(grant);
    slot.retire_after_join().unwrap();
}

#[test]
fn actor_ack_error_and_panic_keep_same_actual_owner_and_quota_for_retry() {
    let Case {
        _directory,
        destination,
        grant,
        body,
        audio,
        lease: _,
        quota,
        record,
    } = case();
    let mut slot =
        DownloadPublicationSlot::admit(audio).unwrap_or_else(|failure| panic!("{}", failure.cause));
    slot.start(|_| {}).unwrap();
    wait_joined(&slot);
    assert!(
        slot.acknowledge_repository_after_join(|_, _| anyhow::bail!("Task/artifact ACK rejected"))
            .is_err()
    );
    assert_eq!(quota.bytes(), body.len() as u64);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ =
            slot.acknowledge_repository_after_join(|_, _| panic!("Actor interrupted during ACK"));
    }));
    assert!(panic.is_err());
    assert_eq!(quota.bytes(), body.len() as u64);
    assert_fence(&destination.join(&record.final_basename), &destination);
    slot.acknowledge_repository_after_join(|_, actual| {
        assert_eq!(actual.identity, record.identity);
        Ok(())
    })
    .unwrap();
    drop(grant);
    slot.retire_after_join().unwrap();
}

#[test]
fn losing_the_controller_keeps_actual_native_ownership_in_the_bounded_keeper() {
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
    let mut slot =
        DownloadPublicationSlot::admit(audio).unwrap_or_else(|failure| panic!("{}", failure.cause));
    slot.start(|_| {}).unwrap();
    wait_joined(&slot);
    let weak = Arc::downgrade(&slot.state);
    let key = slot.key.clone();
    drop(slot);
    assert_fence(&destination.join(&record.final_basename), &destination);
    // The test recovers the exact kept Arc; no stored path or identity creates it.
    let mut resumed = DownloadPublicationSlot::recover_retained(&key.profile_id)
        .into_iter()
        .find(|retained| retained.key == key)
        .expect("Actual kept owner survives");
    assert!(Arc::ptr_eq(
        &resumed.state,
        &weak.upgrade().expect("Original actual Arc")
    ));
    assert!(resumed.is_joined().unwrap());
    drop(grant);
    resumed.retire_after_join().unwrap();
    assert!(
        !DownloadPublicationSlot::recover_retained(&key.profile_id)
            .iter()
            .any(|retained| retained.key == key)
    );
}

#[test]
fn prepublication_source_cancel_keeps_the_original_partial_until_joined_native_retirement() {
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
    let mut slot =
        DownloadPublicationSlot::admit(audio).unwrap_or_else(|failure| panic!("{}", failure.cause));
    slot.start_guarded(
        Box::new(move || {
            lease.revoke();
            Ok(())
        }),
        Box::new(|| Ok(())),
        |_| {},
    )
    .unwrap();
    wait_joined(&slot);
    assert!(slot.error().is_some());
    assert!(!slot.worker_panicked().unwrap());
    assert!(!destination.join(&record.final_basename).exists());
    assert_eq!(
        std::fs::read(destination.join(&record.basename)).unwrap(),
        body
    );
    assert_fence(&destination.join(&record.basename), &destination);
    assert!(
        slot.acknowledge_repository_after_join(|_, _| Ok(()))
            .is_err()
    );
    drop(grant);
    slot.retire_after_join().unwrap();
    assert_eq!(quota.bytes(), body.len() as u64);
    assert_eq!(
        std::fs::read(destination.join(&record.basename)).unwrap(),
        body
    );
}
