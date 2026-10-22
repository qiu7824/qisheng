use super::*;
use crate::audio::worker_owner::{WorkerKind, WorkerPermit};
use std::os::windows::fs::OpenOptionsExt;
use std::{cell::RefCell, os::windows::io::AsRawHandle, sync::mpsc, thread};
use windows_sys::Win32::{
    Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation},
    Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE},
};

static CASES: Mutex<()> = Mutex::new(());

fn native_worker(
    operation: impl FnOnce() + Send + 'static,
) -> crate::audio::worker_owner::TrackedWorker {
    let permit = WorkerPermit::reserve(WorkerKind::Download).unwrap();
    let ticket = permit.tracker();
    let (start, ready) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        ready.recv().unwrap();
        let _scope = ProducerWorkerScope::enter(ticket).unwrap();
        operation();
    });
    let worker = permit.attach(handle);
    start.send(()).unwrap();
    worker
}

struct Fixture {
    directory: Option<tempfile::TempDir>,
    file: Option<ProducerFile>,
    plan: Option<DownloadPlan>,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::bounded(4096)
    }
    fn bounded(maximum_bytes: u64) -> Self {
        let root = Path::new(r"E:\codex操作目录\tasks\lx-gpui-player\fixtures");
        std::fs::create_dir_all(root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("producer-")
            .tempdir_in(root)
            .unwrap();
        let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
        let plan = grant.plan("authored.wav", CollisionPolicy::Skip).unwrap();
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).unwrap();
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = directory.path().join(format!(".lx-download-{nonce}.part"));
        let context = crate::download::DownloadContext {
            profile_id: "authored-producer-profile".into(),
            task_id: "authored-producer-task".into(),
            operation_epoch: 1,
            source_id: "authored-producer-source".into(),
            source_content_sha256: "ab".repeat(32),
            policy_epoch: 1,
        };
        let worker_plan = plan.clone();
        let file = crate::download::run_download_producer(move || {
            ProducerFile::reserve(
                worker_plan,
                nonce,
                crate::download::artifact::OwnedArtifactKind::Transfer,
                Some(context),
                maximum_bytes,
            )
        })
        .unwrap();
        Self {
            directory: Some(directory),
            file: Some(file),
            plan: Some(plan),
            path,
        }
    }
    fn file(&self) -> &ProducerFile {
        self.file.as_ref().unwrap()
    }
    fn create(&self) {
        let file = self.file().shared_borrow();
        let directory = self.directory.as_ref().unwrap().path().to_owned();
        let expected_maximum = self
            .file()
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .maximum_bytes;
        crate::download::run_download_producer(move || {
            file.create(|_, basename, _, maximum| {
                assert_eq!(maximum, expected_maximum);
                assert!(!directory.join(basename).exists());
                Ok(())
            })
        })
        .unwrap();
        let record = self
            .file()
            .inspect(|file| {
                let plan = self.plan.as_ref().unwrap();
                Ok(OwnedPartRecord {
                    basename: self.path.file_name().unwrap().to_str().unwrap().into(),
                    nonce: self
                        .path
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .strip_prefix(".lx-download-")
                        .unwrap()
                        .strip_suffix(".part")
                        .unwrap()
                        .into(),
                    identity: platform::identity(file)?,
                    destination_identity: plan.root_identity.clone(),
                    destination_epoch: plan.epoch,
                    final_basename: plan.basename.clone(),
                })
            })
            .unwrap();
        self.file().remember_record(record).unwrap();
    }
    fn acknowledge(&self) -> OwnedPartRecord {
        self.file()
            .acknowledge_created(|file, plan, basename, nonce| {
                platform::validate(file, false)?;
                ensure!(
                    file.metadata()?.len() == 0 && platform::link_count(file)? == 1,
                    "Fixture requires the actual zero file"
                );
                let identity = platform::identity(file)?;
                platform::verify_file_name(plan.grant.parent(), basename, &identity)?;
                Ok(OwnedPartRecord {
                    basename: basename.into(),
                    nonce: nonce.into(),
                    identity,
                    destination_identity: plan.root_identity.clone(),
                    destination_epoch: plan.epoch,
                    final_basename: plan.basename.clone(),
                })
            })
            .unwrap()
    }
    fn raw(&self) -> usize {
        self.file()
            .inspect(|file| Ok(file.as_raw_handle() as usize))
            .unwrap()
    }
    fn remove_actual(&self) {
        let record = self
            .file()
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .record
            .clone()
            .unwrap();
        self.file()
            .inspect(|file| {
                platform::remove_owned(
                    file,
                    self.plan.as_ref().unwrap().grant.parent(),
                    &record.basename,
                    &record.identity,
                )
            })
            .unwrap();
    }
    fn assert_fenced(&self) {
        let error = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&self.path)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert!(std::fs::remove_file(&self.path).is_err());
        let directory = self.directory.as_ref().unwrap().path();
        assert!(std::fs::rename(directory, directory.with_extension("moved")).is_err());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let workers = {
                let kept = producers()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                kept[file.index].as_ref().map_or_else(Vec::new, |entry| {
                    entry
                        .workers
                        .iter()
                        .map(|bound| bound.worker.clone())
                        .collect()
                })
            };
            for worker in workers {
                assert!(worker.wait(Duration::from_secs(2)));
            }
            if file.metadata().is_ok() {
                let record = file
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .record
                    .clone()
                    .unwrap();
                let grant = &self.plan.as_ref().unwrap().grant;
                file.inspect(|native| {
                    platform::remove_owned(
                        native,
                        grant.parent(),
                        &record.basename,
                        &record.identity,
                    )
                })
                .unwrap();
            }
            file.close_after_join().unwrap();
            drop(self.plan.take());
            file.release_removed().unwrap();
            drop(file);
        }
        drop(self.plan.take());
        drop(self.directory.take());
    }
}

struct ReleaseOnDrop(mpsc::SyncSender<()>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}
struct UnprotectOnDrop {
    file: ProducerFile,
    directory: bool,
}
impl Drop for UnprotectOnDrop {
    fn drop(&mut self) {
        if self.directory {
            let _ = self.file.inspect_namespace(|grant| {
                unsafe {
                    SetHandleInformation(
                        grant.parent().as_raw_handle(),
                        HANDLE_FLAG_PROTECT_FROM_CLOSE,
                        0,
                    );
                }
                Ok(())
            });
        } else {
            let _ = self.file.inspect(|native| {
                unsafe {
                    SetHandleInformation(native.as_raw_handle(), HANDLE_FLAG_PROTECT_FROM_CLOSE, 0);
                }
                Ok(())
            });
        }
    }
}

#[test]
fn actual_native_capture_survives_validation_and_ack_failures_before_first_write() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    let raw = fixture.raw();
    assert!(
        fixture
            .file()
            .acknowledge_created(|file, _, _, _| {
                assert_eq!(file.as_raw_handle() as usize, raw);
                bail!("Actual post-create validation refused")
            })
            .is_err()
    );
    assert_eq!(fixture.raw(), raw);
    fixture.assert_fenced();
    let mut borrowed = fixture.file().shared_borrow();
    assert_eq!(
        crate::download::run_download_producer(move || {
            Ok(borrowed.write(b"unacknowledged").unwrap_err().kind())
        })
        .unwrap(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(fixture.file().metadata().unwrap().len(), 0);
    assert!(
        fixture
            .file()
            .acknowledge_created(|file, _, _, _| {
                assert_eq!(file.as_raw_handle() as usize, raw);
                bail!("Actual creation transaction refused")
            })
            .is_err()
    );
    assert_eq!(fixture.raw(), raw);
    fixture.assert_fenced();
}

#[test]
fn untracked_native_read_seek_flush_and_sync_are_rejected_without_touching_the_body() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    fixture.acknowledge();
    let mut writer = fixture.file().shared_borrow();
    crate::download::run_download_producer(move || {
        writer.write_all(b"actual read body")?;
        writer.sync_all()?;
        Ok(())
    })
    .unwrap();
    let raw = fixture.raw();
    let mut loan = fixture.file().shared_borrow();
    let mut bytes = [0xabu8; 32];
    assert_eq!(
        loan.read(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(bytes, [0xab; 32]);
    assert_eq!(
        loan.seek(SeekFrom::Start(0)).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        loan.flush().unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        loan.sync_all().unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(fixture.raw(), raw);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), b"actual read body");
    assert!(fixture.file().producers_joined().unwrap());
    fixture.assert_fenced();
}

#[test]
fn actual_ack_panic_keeps_the_original_zero_file_and_selected_directory() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    let raw = fixture.raw();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = fixture.file().acknowledge_created(|file, _, _, _| {
            assert_eq!(file.as_raw_handle() as usize, raw);
            panic!("Actual creation ACK panic");
        });
    }));
    assert!(result.is_err());
    assert_eq!(fixture.raw(), raw);
    assert_eq!(fixture.file().metadata().unwrap().len(), 0);
    fixture.assert_fenced();
}

#[test]
fn actual_file_callback_rejects_same_and_cross_slot_io_before_any_lock_reentry() {
    let _case = CASES.lock().unwrap();
    let left = Fixture::new();
    let right = Fixture::new();
    left.create();
    right.create();
    let left_raw = left.raw();
    let right_raw = right.raw();
    left.file()
        .inspect(|native| {
            assert_eq!(native.as_raw_handle() as usize, left_raw);
            assert!(left.file().metadata().is_err());
            assert!(right.file().metadata().is_err());
            assert!(right.file().reserve_length(1).is_err());
            assert!(
                ProducerFile::reserve(
                    right.plan.as_ref().unwrap().clone(),
                    "cd".repeat(16),
                    crate::download::artifact::OwnedArtifactKind::Transfer,
                    None,
                    4096
                )
                .is_err()
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(left.raw(), left_raw);
    assert_eq!(right.raw(), right_raw);
    left.assert_fenced();
    right.assert_fenced();
}

#[test]
fn real_callback_panic_resets_reentry_guard_without_discarding_original_file() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    let raw = fixture.raw();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = fixture
                .file()
                .inspect(|_| panic!("Actual file callback panic"));
        }))
        .is_err()
    );
    assert_eq!(fixture.raw(), raw);
    fixture.assert_fenced();
}

#[test]
fn real_pre_attach_worker_cannot_enter_native_scope_or_claim_a_join() {
    let _case = CASES.lock().unwrap();
    let permit = WorkerPermit::reserve(WorkerKind::Download).unwrap();
    let ticket = permit.tracker();
    assert!(!ticket.wait(Duration::ZERO));
    let child_ticket = ticket.clone();
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let (start, ready) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        observed_tx
            .send(ProducerWorkerScope::enter(child_ticket.clone()).is_err())
            .unwrap();
        ready.recv().unwrap();
        let _scope = ProducerWorkerScope::enter(child_ticket).unwrap();
    });
    assert!(observed_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(!ticket.wait(Duration::ZERO));
    let worker = permit.attach(handle);
    start.send(()).unwrap();
    assert!(worker.wait(Duration::from_secs(2)));
    assert!(ticket.wait(Duration::ZERO));
}

#[test]
fn prebound_current_read_preparation_excludes_only_its_exact_ticket_and_cannot_close() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    fixture.acknowledge();
    let raw = fixture.raw();
    let (other_release_tx, other_release_rx) = mpsc::sync_channel(1);
    let _other_release = ReleaseOnDrop(other_release_tx.clone());
    let other = native_worker(move || {
        other_release_rx.recv().unwrap();
    });
    fixture.file().attach_worker(other.clone()).unwrap();
    let permit = WorkerPermit::reserve(WorkerKind::Download).unwrap();
    let ticket = permit.tracker();
    let (start, ready) = mpsc::sync_channel(1);
    let (phase_tx, phase_rx) = mpsc::sync_channel(1);
    let (retry_tx, retry_rx) = mpsc::sync_channel(1);
    let _retry_release = ReleaseOnDrop(retry_tx.clone());
    let file = fixture.file().shared_borrow();
    let handle = thread::spawn(move || {
        ready.recv().unwrap();
        let _scope = ProducerWorkerScope::enter(ticket).unwrap();
        assert!(!file.previous_producers_joined_for_current().unwrap());
        phase_tx.send(()).unwrap();
        retry_rx.recv().unwrap();
        assert!(file.previous_producers_joined_for_current().unwrap());
        assert!(!file.producers_joined().unwrap());
        assert!(file.close_after_join().is_err());
        assert_eq!(
            file.inspect(|native| Ok(native.as_raw_handle() as usize))
                .unwrap(),
            raw
        );
        phase_tx.send(()).unwrap();
    });
    let current = permit.attach(handle);
    fixture.file().attach_worker(current.clone()).unwrap();
    start.send(()).unwrap();
    phase_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!other.wait(Duration::ZERO));
    other_release_tx.send(()).unwrap();
    assert!(other.wait(Duration::from_secs(2)));
    retry_tx.send(()).unwrap();
    phase_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(current.wait(Duration::from_secs(2)));
    assert!(fixture.file().producers_joined().unwrap());
    assert_eq!(fixture.raw(), raw);
    fixture.assert_fenced();
}

#[test]
fn actual_body_write_then_worker_panic_keeps_same_object_and_byte_reservation() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    let record = fixture.acknowledge();
    let raw = fixture.raw();
    let mut borrowed = fixture.file().shared_borrow();
    let tracked = native_worker(move || {
        borrowed.write_all(b"authored-body").unwrap();
        borrowed.sync_all().unwrap();
        panic!("Actual producer panic after first body write");
    });
    fixture.file().attach_worker(tracked.clone()).unwrap();
    assert!(tracked.wait(Duration::from_secs(2)));
    assert!(tracked.panicked());
    assert_eq!(fixture.raw(), raw);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), b"authored-body");
    fixture.assert_fenced();
    let retained = ProducerFile::retain_original(fixture.plan.as_ref().unwrap(), &record)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&retained.state, &fixture.file().state));
    assert_eq!(
        retained
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .maximum_bytes,
        4096
    );
    drop(retained);
}

struct TeardownGate {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for TeardownGate {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv();
    }
}
thread_local! {
    static TEARDOWN: RefCell<Option<TeardownGate>> = const { RefCell::new(None) };
}

#[test]
fn actual_notification_and_tls_teardown_do_not_authorize_native_close_before_join() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    fixture.acknowledge();
    let raw = fixture.raw();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let _release = ReleaseOnDrop(release_tx.clone());
    let (notified_tx, notified_rx) = mpsc::sync_channel(1);
    let permit = WorkerPermit::reserve(WorkerKind::Download).unwrap();
    let tracked = permit.attach(thread::spawn(move || {
        TEARDOWN.with(|gate| {
            *gate.borrow_mut() = Some(TeardownGate {
                entered: entered_tx,
                release: release_rx,
            })
        });
        notified_tx.send(()).unwrap();
    }));
    fixture.file().attach_worker(tracked.clone()).unwrap();
    notified_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!tracked.wait(Duration::ZERO));
    assert!(!fixture.file().producers_joined().unwrap());
    assert!(fixture.file().close_after_join().is_err());
    assert_eq!(fixture.raw(), raw);
    fixture.assert_fenced();
    release_tx.send(()).unwrap();
    assert!(tracked.wait(Duration::from_secs(2)));
    fixture.remove_actual();
    fixture.file().close_after_join().unwrap();
    assert_eq!(
        fixture
            .file()
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .maximum_bytes,
        4096
    );
}

#[test]
fn failed_actual_close_restores_the_same_native_handle_and_original_pins() {
    let _case = CASES.lock().unwrap();
    let fixture = Fixture::new();
    fixture.create();
    let raw = fixture.raw();
    fixture
        .file()
        .inspect(|file| {
            ensure!(
                unsafe {
                    SetHandleInformation(
                        file.as_raw_handle(),
                        HANDLE_FLAG_PROTECT_FROM_CLOSE,
                        HANDLE_FLAG_PROTECT_FROM_CLOSE,
                    )
                } != 0,
                "Cannot protect actual fixture handle"
            );
            Ok(())
        })
        .unwrap();
    let protection = UnprotectOnDrop {
        file: fixture.file().shared_borrow(),
        directory: false,
    };
    assert!(fixture.file().close_after_join().is_err());
    assert_eq!(fixture.raw(), raw);
    fixture.assert_fenced();
    fixture
        .file()
        .inspect(|file| {
            ensure!(
                unsafe {
                    SetHandleInformation(file.as_raw_handle(), HANDLE_FLAG_PROTECT_FROM_CLOSE, 0)
                } != 0,
                "Cannot unprotect actual fixture handle"
            );
            Ok(())
        })
        .unwrap();
    drop(protection);
    fixture.remove_actual();
    fixture.file().close_after_join().unwrap();
    assert!(fixture.file().inspect(|_| Ok(())).is_err());
    assert_eq!(
        fixture
            .file()
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .maximum_bytes,
        4096
    );
}

#[test]
fn failed_actual_directory_close_retains_same_pin_and_charge_after_file_absence() {
    let _case = CASES.lock().unwrap();
    let mut fixture = Fixture::new();
    fixture.create();
    fixture.acknowledge();
    let mut writer = fixture.file().shared_borrow();
    crate::download::run_download_producer(move || {
        writer.write_all(b"actual-charge")?;
        Ok(())
    })
    .unwrap();
    let pin = fixture
        .file()
        .inspect_namespace(|grant| {
            let raw = grant.parent().as_raw_handle();
            ensure!(
                unsafe {
                    SetHandleInformation(
                        raw,
                        HANDLE_FLAG_PROTECT_FROM_CLOSE,
                        HANDLE_FLAG_PROTECT_FROM_CLOSE,
                    )
                } != 0,
                "Cannot protect actual directory pin"
            );
            Ok(raw as usize)
        })
        .unwrap();
    let protection = UnprotectOnDrop {
        file: fixture.file().shared_borrow(),
        directory: true,
    };
    fixture.remove_actual();
    fixture.file().close_after_join().unwrap();
    drop(fixture.plan.take());
    assert!(fixture.file().release_removed().is_err());
    fixture
        .file()
        .inspect_namespace(|grant| {
            assert_eq!(grant.parent().as_raw_handle() as usize, pin);
            ensure!(
                unsafe {
                    SetHandleInformation(
                        grant.parent().as_raw_handle(),
                        HANDLE_FLAG_PROTECT_FROM_CLOSE,
                        0,
                    )
                } != 0,
                "Cannot unprotect actual directory pin"
            );
            Ok(())
        })
        .unwrap();
    drop(protection);
    let state = fixture
        .file()
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(state.phase, ProducerPhase::NamespaceRemoved);
    assert_eq!(state.charged_bytes, b"actual-charge".len() as u64);
}

#[test]
fn zero_length_native_objects_exhaust_the_real_104_slots_before_the_next_create() {
    let _case = CASES.lock().unwrap();
    let occupied = producers()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .filter(|entry| entry.is_some())
        .count();
    let mut fixtures = Vec::new();
    for _ in occupied..CAPACITY {
        let fixture = Fixture::new();
        fixture.create();
        fixture.acknowledge();
        assert_eq!(fixture.file().metadata().unwrap().len(), 0);
        fixture.assert_fenced();
        fixtures.push(fixture);
    }
    assert_eq!(
        producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|entry| entry.is_some())
            .count(),
        104
    );
    let plan = fixtures
        .last()
        .expect("Native capacity fixture requires a free slot")
        .plan
        .as_ref()
        .unwrap()
        .clone();
    let forbidden = plan
        .grant
        .path()
        .join(format!(".lx-download-{}.part", "67".repeat(16)));
    let refused = crate::download::run_download_producer(move || {
        ProducerFile::reserve(
            plan,
            "67".repeat(16),
            crate::download::artifact::OwnedArtifactKind::Transfer,
            None,
            1,
        )
    });
    assert!(refused.err().unwrap().to_string().contains("capacity"));
    assert!(!forbidden.exists());
    drop(fixtures);
    assert_eq!(
        producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|entry| entry.is_some())
            .count(),
        occupied
    );
}

#[test]
fn real_sparse_file_lengths_share_four_gib_and_closed_handles_do_not_refund_bytes() {
    let _case = CASES.lock().unwrap();
    assert_eq!(
        producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .flatten()
            .map(|entry| entry.charge.load(Ordering::Acquire))
            .sum::<u64>(),
        0
    );
    let mut fixtures = Vec::new();
    for _ in 0..4 {
        let fixture = Fixture::bounded(MAX_DOWNLOAD_BYTES);
        fixture.create();
        fixture.acknowledge();
        let file = fixture.file().shared_borrow();
        crate::download::run_download_producer(move || {
            file.inspect(|native| {
                let mut returned = 0u32;
                // CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 49, METHOD_BUFFERED, FILE_ANY_ACCESS).
                let set_sparse = (9 << 16) | (49 << 2);
                ensure!(
                    unsafe {
                        windows_sys::Win32::System::IO::DeviceIoControl(
                            native.as_raw_handle(),
                            set_sparse,
                            std::ptr::null(),
                            0,
                            std::ptr::null_mut(),
                            0,
                            &mut returned,
                            std::ptr::null_mut(),
                        )
                    } != 0,
                    "Cannot mark the actual authored native file sparse: {}",
                    io::Error::last_os_error()
                );
                Ok(())
            })?;
            file.set_len(MAX_DOWNLOAD_BYTES)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fixture.file().metadata().unwrap().len(), MAX_DOWNLOAD_BYTES);
        fixtures.push(fixture);
    }
    assert_eq!(
        producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .flatten()
            .map(|entry| entry.charge.load(Ordering::Acquire))
            .sum::<u64>(),
        crate::download::MAX_OWNED_PART_BYTES
    );
    let fifth = Fixture::bounded(MAX_DOWNLOAD_BYTES);
    fifth.create();
    fifth.acknowledge();
    let fifth_file = fifth.file().shared_borrow();
    assert!(
        crate::download::run_download_producer(move || {
            fifth_file.set_len(1)?;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(fifth.file().metadata().unwrap().len(), 0);
    let first = &fixtures[0];
    first.file().close_after_join().unwrap();
    assert_eq!(
        std::fs::metadata(&first.path).unwrap().len(),
        MAX_DOWNLOAD_BYTES
    );
    let fifth_file = fifth.file().shared_borrow();
    assert!(
        crate::download::run_download_producer(move || {
            fifth_file.set_len(1)?;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(fifth.file().metadata().unwrap().len(), 0);
    let first_file = first.file().shared_borrow();
    let plan = first.plan.as_ref().unwrap().clone();
    let record = first_file.record_binding().unwrap();
    crate::download::run_download_producer(move || {
        first_file.reopen_closed(&plan, &record)?;
        first_file.acknowledge_created(|_, _, _, _| Ok(record.clone()))?;
        Ok(())
    })
    .unwrap();
    drop(fifth);
    drop(fixtures);
    assert_eq!(
        producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .flatten()
            .map(|entry| entry.charge.load(Ordering::Acquire))
            .sum::<u64>(),
        0
    );
}
