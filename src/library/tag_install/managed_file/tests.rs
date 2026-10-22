use super::*;
use crate::download::{
    CollisionPolicy, DestinationGrant, DownloadContext, DownloadPlan,
    artifact::{OwnedArtifactIntent, OwnedArtifactKind},
    prepare_retry_native_file, run_download_producer,
};
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation};

struct Coordinator {
    plan: DownloadPlan,
    directory: PathBuf,
    nonce: String,
    maximum: u64,
    allocated: usize,
    acknowledged_handle: usize,
    reject_ack: usize,
    attempted_handles: Vec<usize>,
}
impl TagInstallCoordinator for Coordinator {
    fn prepare_managed_artifact(
        &mut self,
        _: &TagWriteSelection,
        role: TagInstallArtifactRole,
        _: &Path,
        maximum: u64,
    ) -> Result<Option<TagManagedArtifact>> {
        ensure!(role == TagInstallArtifactRole::Candidate && maximum == self.maximum);
        Ok(Some(TagManagedArtifact::from_download(
            prepare_retry_native_file(
                self.plan.clone(),
                self.nonce.clone(),
                OwnedArtifactKind::TagCopy,
                DownloadContext {
                    profile_id: "managed-m06-profile".into(),
                    task_id: "managed-m06-task".into(),
                    operation_epoch: 1,
                    source_id: "managed-m06-source".into(),
                    source_content_sha256: "ab".repeat(32),
                    policy_epoch: 1,
                },
                maximum,
            )?,
        )))
    }
    fn before_managed_artifact_allocation(
        &mut self,
        _: &TagWriteSelection,
        _: TagInstallArtifactRole,
        intent: &OwnedArtifactIntent,
    ) -> Result<()> {
        intent.validate()?;
        ensure!(!self.directory.join(&intent.basename).exists());
        let mut journal = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.directory.join("allocation-intent.json"))?;
        journal.write_all(&serde_json::to_vec(intent)?)?;
        journal.sync_all()?;
        Ok(())
    }
    fn artifact_allocated(
        &mut self,
        _: &TagWriteSelection,
        allocation: &TagArtifactAllocation<'_>,
    ) -> Result<()> {
        allocation.check_empty()?;
        let intent: OwnedArtifactIntent = serde_json::from_slice(&std::fs::read(
            self.directory.join("allocation-intent.json"),
        )?)?;
        ensure!(
            intent.basename == allocation.facts.path.file_name().unwrap().to_str().unwrap()
                && self.allocated == 0
        );
        self.attempted_handles
            .push(allocation.file.as_raw_handle() as usize);
        if self.reject_ack > 0 {
            self.reject_ack -= 1;
            anyhow::bail!("Actual zero-file acknowledgement rejected before durable ACK")
        }
        let mut journal = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.directory.join("allocation-ack.json"))?;
        journal.write_all(&serde_json::to_vec(&serde_json::json!({
            "identity": &allocation.facts.identity,
            "directory_identity": &allocation.facts.directory_identity,
            "path": &allocation.facts.path,
            "bytes": allocation.file.metadata()?.len(),
        }))?)?;
        journal.sync_all()?;
        self.allocated += 1;
        self.acknowledged_handle = allocation.file.as_raw_handle() as usize;
        Ok(())
    }
    fn artifact_limit(&self, _: TagInstallArtifactRole) -> u64 {
        self.maximum
    }
    fn authorize_explicit_selection(&self, _: &TagWriteSelection) -> Result<()> {
        Ok(())
    }
    fn check_current_intent(&self, _: &TagWriteSelection) -> Result<()> {
        Ok(())
    }
    fn journal_directory(&self) -> Result<PathBuf> {
        Ok(self.directory.clone())
    }
    fn commit_installed(
        &mut self,
        _: &TagWriteSelection,
        _: &TagInstallationReceipt,
    ) -> Result<u64> {
        anyhow::bail!("allocation fixture has no installed original")
    }
    fn publish_installed(
        &mut self,
        _: &TagWriteSelection,
        _: &TagInstallationReceipt,
    ) -> Result<()> {
        anyhow::bail!("allocation fixture has no installed original")
    }
    fn check_recovery(
        &self,
        _: &TagWriteSelection,
        _: TagInstallPhase,
        _: Option<&TagInstallationReceipt>,
    ) -> Result<TagRecoveryDisposition> {
        anyhow::bail!("allocation fixture has no cold recovery")
    }
}

#[test]
fn managed_m06_allocation_uses_same_empty_handle_and_preserves_failed_close_owner() {
    let temporary_root = std::env::temp_dir().join("lx-native-fixtures");
    let root = temporary_root.as_path();
    std::fs::create_dir_all(root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("m06-managed-")
        .tempdir_in(root)
        .unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let plan = grant.plan("managed.wav", CollisionPolicy::Skip).unwrap();
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).unwrap();
    let nonce: String = random.iter().map(|value| format!("{value:02x}")).collect();
    let path = directory.path().join(format!(".lx-download-{nonce}.part"));
    let worker_directory = directory.path().to_owned();
    let worker_path = path.clone();
    let (mut file, identity, native_handle) = run_download_producer(move || {
        let pins = pin_directory(&worker_directory)?;
        let parent = pins.last().unwrap();
        let selection = TagWriteSelection {
            operation_id: "managed-m06-operation".into(),
            asset_id: "managed-m06-asset".into(),
            path: worker_directory.join("managed.wav"),
            expected_identity: actual_identity(parent)?,
            root_id: None,
            grant_epoch: 1,
            expected_binding_revision: 1,
            expected_body_sha256: None,
        };
        let mut coordinator = Coordinator {
            plan,
            directory: worker_directory,
            nonce,
            maximum: 16,
            allocated: 0,
            acknowledged_handle: 0,
            reject_ack: 0,
            attempted_handles: Vec::new(),
        };
        let mut slot = None;
        allocate_artifact(
            &mut slot,
            &selection,
            TagInstallArtifactRole::Candidate,
            &worker_path,
            parent,
            ArtifactAllocator::Managed,
            &mut coordinator,
        )?;
        let mut file = slot.unwrap();
        let native_handle = file.inspect_native(|native| Ok(native.as_raw_handle() as usize))?;
        ensure!(native_handle == coordinator.acknowledged_handle && coordinator.allocated == 1);
        let identity = actual_identity(&file)?;
        file.write_all(b"actual-body")?;
        file.sync_all()?;
        file.rewind()?;
        let mut body = Vec::new();
        file.read_to_end(&mut body)?;
        ensure!(body == b"actual-body");
        ensure!(file.set_len(17).is_err() && file.metadata()?.len() == 11);
        ensure!(file.managed_borrow().unwrap().close_after_join().is_err());
        ensure!(actual_identity(&file)? == identity);
        Ok((file, identity, native_handle))
    })
    .unwrap();
    assert!(file.write_all(b"untracked").is_err());
    assert_eq!(file.metadata().unwrap().len(), 11);
    let managed = file.managed_borrow().unwrap();
    let mut slot = Some(file);
    assert_ne!(
        unsafe {
            SetHandleInformation(
                native_handle as _,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        },
        0
    );
    assert!(TagNativeFile::close(&mut slot).is_err());
    assert_eq!(actual_identity(slot.as_ref().unwrap()).unwrap(), identity);
    assert_eq!(
        slot.as_ref()
            .unwrap()
            .inspect_native(|native| Ok(native.as_raw_handle() as usize))
            .unwrap(),
        native_handle
    );
    assert_ne!(
        unsafe { SetHandleInformation(native_handle as _, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    slot.as_ref().unwrap().request_removal().unwrap();
    TagNativeFile::close(&mut slot).unwrap();
    assert!(slot.is_none() && !path.exists());
    let released = managed.release_retired().unwrap();
    assert_eq!(released.identity.object, identity.file_id);
    assert_eq!(
        released.identity.device,
        u64::from_str_radix(&identity.volume, 16).unwrap()
    );
}

#[test]
fn managed_m06_failed_zero_ack_retries_same_retained_handle_after_real_join() {
    let temporary_root = std::env::temp_dir().join("lx-native-fixtures");
    let root = temporary_root.as_path();
    std::fs::create_dir_all(root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("m06-ack-retry-")
        .tempdir_in(root)
        .unwrap();
    let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
    let plan = grant.plan("managed.wav", CollisionPolicy::Skip).unwrap();
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).unwrap();
    let nonce: String = random.iter().map(|value| format!("{value:02x}")).collect();
    let path = directory.path().join(format!(".lx-download-{nonce}.part"));
    let worker_directory = directory.path().to_owned();
    let worker_path = path.clone();
    let (mut file, mut coordinator, selection, pins, identity, handle) =
        run_download_producer(move || {
            let pins = pin_directory(&worker_directory)?;
            let parent = pins.last().unwrap();
            let selection = TagWriteSelection {
                operation_id: "managed-m06-retry-operation".into(),
                asset_id: "managed-m06-retry-asset".into(),
                path: worker_directory.join("managed.wav"),
                expected_identity: actual_identity(parent)?,
                root_id: None,
                grant_epoch: 1,
                expected_binding_revision: 1,
                expected_body_sha256: None,
            };
            let mut coordinator = Coordinator {
                plan,
                directory: worker_directory,
                nonce,
                maximum: 16,
                allocated: 0,
                acknowledged_handle: 0,
                reject_ack: 1,
                attempted_handles: Vec::new(),
            };
            let mut slot = None;
            ensure!(
                allocate_artifact(
                    &mut slot,
                    &selection,
                    TagInstallArtifactRole::Candidate,
                    &worker_path,
                    parent,
                    ArtifactAllocator::Managed,
                    &mut coordinator
                )
                .is_err()
            );
            let mut file = slot.context("Failed ACK lost its actual pre-admitted owner")?;
            let identity = actual_identity(&file)?;
            let handle = file.inspect_native(|native| Ok(native.as_raw_handle() as usize))?;
            let managed = file.managed_borrow().unwrap();
            ensure!(
                !managed.creation_acknowledged()?
                    && coordinator.allocated == 0
                    && coordinator.attempted_handles == [handle]
                    && file.metadata()?.len() == 0
            );
            ensure!(file.write_all(b"before-ack").is_err() && file.metadata()?.len() == 0);
            let TagNativeFile::Download(native) = &mut file else {
                unreachable!()
            };
            ensure!(
                native
                    .acknowledge_existing_with_native_ack(|_, _, _| anyhow::bail!(
                        "Unjoined retry must not reach its callback"
                    ))
                    .is_err()
            );
            ensure!(actual_identity(&file)? == identity);
            Ok((file, coordinator, selection, pins, identity, handle))
        })
        .unwrap();
    assert!(!directory.path().join("allocation-ack.json").exists());
    coordinator.reject_ack = 1;
    let acknowledgement = ManagedAllocationAck {
        selection: &selection,
        role: TagInstallArtifactRole::Candidate,
        path: &path,
        directory: pins.last().unwrap(),
        maximum: 16,
    };
    let TagNativeFile::Download(native) = &mut file else {
        unreachable!()
    };
    assert!(
        native
            .acknowledge_existing_with_native_ack(|file, parent, allocation| acknowledgement
                .acknowledge(file, parent, allocation, &mut coordinator))
            .is_err()
    );
    assert!(!native.creation_acknowledged().unwrap());
    assert_eq!(native.metadata().unwrap().len(), 0);
    native
        .acknowledge_existing_with_native_ack(|file, parent, allocation| {
            acknowledgement.acknowledge(file, parent, allocation, &mut coordinator)
        })
        .unwrap();
    assert!(native.creation_acknowledged().unwrap());
    assert_eq!(coordinator.allocated, 1);
    assert_eq!(coordinator.attempted_handles, [handle, handle, handle]);
    let mut callback_repeated = false;
    assert!(
        native
            .acknowledge_existing_with_native_ack(|_, _, _| {
                callback_repeated = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!callback_repeated);
    assert_eq!(actual_identity(&file).unwrap(), identity);
    assert_eq!(
        file.inspect_native(|native| Ok(native.as_raw_handle() as usize))
            .unwrap(),
        handle
    );
    let ack: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("allocation-ack.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ack["bytes"], 0);
    assert_eq!(ack["identity"]["file_id"], identity.file_id);
    let file = run_download_producer(move || {
        let mut file = file;
        file.write_all(b"actual-body")?;
        file.sync_all()?;
        file.rewind()?;
        let mut body = Vec::new();
        file.read_to_end(&mut body)?;
        ensure!(body == b"actual-body");
        Ok(file)
    })
    .unwrap();
    let managed = file.managed_borrow().unwrap();
    file.request_removal().unwrap();
    let mut slot = Some(file);
    TagNativeFile::close(&mut slot).unwrap();
    assert!(slot.is_none() && !path.exists());
    let released = managed.release_retired().unwrap();
    assert_eq!(released.identity.object, identity.file_id);
}
