use super::*;
use rusqlite::Connection;
use std::{fs, os::windows::io::AsRawHandle};
use windows_sys::Win32::Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation};

struct RecordingJournal {
    actual: Arc<NativeSidecarJournal>,
    creations: Mutex<Vec<WriterSidecarCreationFact>>,
    retirements: Mutex<Vec<DurableCleanupFact>>,
}
impl RecordingJournal {
    fn new(actual: Arc<NativeSidecarJournal>) -> Self {
        Self {
            actual,
            creations: Mutex::new(Vec::new()),
            retirements: Mutex::new(Vec::new()),
        }
    }
}
impl WriterSidecarJournal for RecordingJournal {
    fn creation(&self, fact: &WriterSidecarCreationFact) -> Result<()> {
        self.actual.creation(fact)?;
        self.creations.lock().unwrap().push(fact.clone());
        Ok(())
    }
    fn retirement(&self, fact: &DurableCleanupFact) -> Result<()> {
        self.actual.retirement(fact)?;
        self.retirements.lock().unwrap().push(fact.clone());
        Ok(())
    }
    fn finish(&self, context: &SnapshotContext) -> Result<()> {
        self.actual.finish(context)
    }
}

fn fixture() -> (
    tempfile::TempDir,
    SnapshotDirectory,
    FileIdentity,
    SnapshotPermit,
    Arc<NativeSidecarJournal>,
) {
    let directory =
        tempfile::tempdir_in(r"E:\codex操作目录\tasks\lx-gpui-player\test-temp").unwrap();
    let database = directory.path().join("native-library.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE payload(value TEXT);")
        .unwrap();
    let identity = unsafe { SourceFileProof::from_sqlite_main(connection.handle(), &database) }
        .unwrap()
        .metadata()
        .unwrap()
        .identity
        .clone();
    connection.close().unwrap();
    let root = SnapshotDirectory::pin_profile(directory.path(), 1).unwrap();
    let private = SnapshotDirectory::open_private(root.path(), 1).unwrap();
    let limiter = SnapshotLimiter::new(128 * 1024 * 1024).unwrap();
    let permit = limiter
        .reserve_job(8 * 1024 * 1024, 3 * 8 * 1024 * 1024 + 65 * 1024 * 1024)
        .unwrap();
    let journal = Arc::new(
        NativeSidecarJournal::create(
            &private,
            "actual-writer-fixture",
            root.identity(),
            &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
            &permit,
        )
        .unwrap(),
    );
    (directory, root, identity, permit, journal)
}
#[test]
fn real_default_wal_and_shm_delegate_retains_creator_pins_and_retires_both_actual_fids() {
    let (directory, root, identity, permit, journal) = fixture();
    let mut owner = SidecarWriter::open(
        root,
        Some(identity),
        "actual-writer-fixture".into(),
        8 * 1024 * 1024,
        permit,
        journal,
    )
    .unwrap();
    owner.connection().unwrap().execute_batch("PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=0; INSERT INTO payload VALUES('owned-actual-default-wal');").unwrap();
    assert!(
        directory
            .path()
            .join(WriterSidecarRole::Wal.leaf())
            .is_file()
    );
    assert!(
        directory
            .path()
            .join(WriterSidecarRole::Shm.leaf())
            .is_file()
    );
    let facts = {
        let state = owner.sidecars_for_test();
        let sidecars = state.as_ref().unwrap();
        for role in [WriterSidecarRole::Wal, WriterSidecarRole::Shm] {
            sidecars.check(role).unwrap();
            assert!(sidecars.entries[&role].creation_acknowledged);
        }
        sidecars
            .entries
            .values()
            .map(|entry| entry.fact.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(facts.len(), 2);
    assert!(
        owner.actual_mapped_views_for_test() > 0,
        "Actual default SHM never produced a retained native view"
    );
    assert!(facts.iter().all(|fact| fact.identity.is_some()));
    assert!(
        fs::rename(
            directory.path().join(WriterSidecarRole::Shm.leaf()),
            directory.path().join("foreign-shm")
        )
        .is_err()
    );
    let count: i64 = owner
        .connection()
        .unwrap()
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    owner
        .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
        .unwrap();
    assert!(
        !directory
            .path()
            .join(WriterSidecarRole::Wal.leaf())
            .exists()
    );
    assert!(
        !directory
            .path()
            .join(WriterSidecarRole::Shm.leaf())
            .exists()
    );
    assert_eq!(
        fs::read_dir(directory.path().join(".lx-snapshot-staging"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn real_creator_pin_close_failure_keeps_original_raw_handle_and_reader_for_retry() {
    let (_, root, identity, permit, journal) = fixture();
    let mut owner = SidecarWriter::open(
        root,
        Some(identity),
        "actual-writer-fixture".into(),
        8 * 1024 * 1024,
        permit,
        journal,
    )
    .unwrap();
    owner
        .connection()
        .unwrap()
        .execute("INSERT INTO payload VALUES('protected-actual-creator')", [])
        .unwrap();
    let raw = {
        let mut state = owner.sidecars_for_test();
        let entry = state
            .as_mut()
            .unwrap()
            .entries
            .get_mut(&WriterSidecarRole::Wal)
            .unwrap();
        entry.creator_pin.as_ref().unwrap().as_raw_handle()
    };
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
    assert!(
        owner
            .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
            .is_err()
    );
    assert!(owner.connection().is_err());
    {
        let state = owner.sidecars_for_test();
        let entry = &state.as_ref().unwrap().entries[&WriterSidecarRole::Wal];
        assert_eq!(entry.creator_pin.as_ref().unwrap().as_raw_handle(), raw);
        assert!(entry.reader.is_some());
        assert!(entry.retirement.is_none());
        assert!(!entry.cleanup_acknowledged);
    }
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    owner
        .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
        .unwrap();
}
#[test]
fn foreign_preexisting_shm_is_never_adopted_as_actual_creator() {
    let (directory, root, identity, permit, journal) = fixture();
    let foreign = directory.path().join(WriterSidecarRole::Shm.leaf());
    let content = b"foreign actual SHM occupant";
    fs::write(&foreign, content).unwrap();
    let opened = SidecarWriter::open(
        root,
        Some(identity),
        "actual-writer-fixture".into(),
        8 * 1024 * 1024,
        permit,
        journal,
    );
    match opened {
        Ok(mut owner) => {
            assert!(
                owner
                    .connection()
                    .unwrap()
                    .execute("INSERT INTO payload VALUES('refused')", [])
                    .is_err()
            );
            owner
                .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
                .unwrap();
        }
        Err(mut failure) => {
            failure
                .owner
                .as_mut()
                .unwrap()
                .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
                .unwrap();
        }
    }
    assert!(
        fs::read(&foreign).unwrap() == content,
        "Unknown actual SHM was altered or retired"
    );
}

#[test]
fn actual_default_delete_journal_has_zero_object_ack_and_same_fid_retirement() {
    let (directory, root, identity, permit, journal) = fixture();
    let baseline = Connection::open(directory.path().join("native-library.sqlite3")).unwrap();
    baseline
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    baseline.close().unwrap();
    let observed = Arc::new(RecordingJournal::new(journal));
    let mut owner = SidecarWriter::open(
        root,
        Some(identity),
        "actual-writer-fixture".into(),
        8 * 1024 * 1024,
        permit,
        observed.clone(),
    )
    .unwrap();
    owner
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO payload VALUES('owned-default-rollback-journal')",
            [],
        )
        .unwrap();
    assert!(
        !directory
            .path()
            .join(WriterSidecarRole::Journal.leaf())
            .exists()
    );
    let creation = observed
        .creations
        .lock()
        .unwrap()
        .iter()
        .find(|fact| {
            fact.role == WriterSidecarRole::Journal
                && fact.origin == WriterSidecarOrigin::Created
                && fact.identity.is_some()
        })
        .unwrap()
        .clone();
    assert_eq!(creation.admitted_bytes, 0);
    assert!(creation.admitted_sha256.is_none());
    let retired = observed
        .retirements
        .lock()
        .unwrap()
        .iter()
        .find(|fact| fact.file.nonce == creation.nonce && fact.removed)
        .unwrap()
        .clone();
    assert_eq!(retired.file.identity, creation.identity);
    assert_eq!(
        owner
            .connection()
            .unwrap()
            .query_row("SELECT value FROM payload", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "owned-default-rollback-journal"
    );
    owner
        .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
        .unwrap();
}

#[test]
fn default_accessor_close_failure_keeps_original_handle_and_never_repeats_win_close() {
    let (_directory, root, identity, permit, journal) = fixture();
    let mut owner = SidecarWriter::open(
        root,
        Some(identity),
        "actual-writer-fixture".into(),
        8 * 1024 * 1024,
        permit,
        journal,
    )
    .unwrap();
    owner
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO payload VALUES('actual-protected-default-accessor')",
            [],
        )
        .unwrap();
    let raw = owner
        .actual_native_handle_for_test(WriterSidecarRole::Wal)
        .unwrap();
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
    assert!(
        owner
            .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
            .is_err()
    );
    assert!(owner.connection().is_err());
    assert!(owner.retains_native_close_for_test(WriterSidecarRole::Wal, raw));
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    owner
        .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
        .unwrap();
}
#[test]
fn default_shm_ignored_close_failure_keeps_real_shared_handle_after_node_was_freed() {
    let (_directory, root, identity, permit, journal) = fixture();
    let mut owner = SidecarWriter::open(
        root,
        Some(identity),
        "actual-writer-fixture".into(),
        8 * 1024 * 1024,
        permit,
        journal,
    )
    .unwrap();
    owner
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO payload VALUES('actual-protected-shared-file')",
            [],
        )
        .unwrap();
    assert!(owner.actual_mapped_views_for_test() > 0);
    let raw = owner.actual_shared_handle_for_test().unwrap();
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
    assert!(
        owner
            .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
            .is_err()
    );
    assert!(owner.connection().is_err());
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0,
        "Default VFS discarded the original shared HANDLE instead of preserving its actual owner"
    );
    owner
        .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
        .unwrap();
}

#[test]
fn legacy_cold_crash_child() {
    let Some(profile) = std::env::var_os("LX_NATIVE_TEST_SQLITE_COLD_PROFILE") else {
        return;
    };
    let profile = std::path::PathBuf::from(profile);
    assert!(profile.starts_with(r"E:\codex操作目录\tasks\lx-gpui-player\test-temp"));
    let connection = Connection::open(profile.join("native-library.sqlite3")).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=0; CREATE TABLE payload(value TEXT); INSERT INTO payload VALUES('committed-before-real-process-exit');").unwrap();
    // The OS terminates actual default SQLite handles without its normal close
    // checkpoint. The parent must recover the genuine committed WAL on cold open.
    std::process::exit(0);
}

#[test]
fn locked_legacy_cold_open_replays_committed_wal_then_retires_original_sidecar_fids() {
    let directory =
        tempfile::tempdir_in(r"E:\codex操作目录\tasks\lx-gpui-player\test-temp").unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repository::snapshot_native::writer_sidecars::tests::legacy_cold_crash_child",
            "--nocapture",
        ])
        .env("LX_NATIVE_TEST_SQLITE_COLD_PROFILE", directory.path())
        .status()
        .unwrap();
    assert!(status.success());
    let wal_path = directory.path().join(WriterSidecarRole::Wal.leaf());
    assert!(fs::metadata(&wal_path).unwrap().len() > 32);
    let guard = crate::repository::ProfileGuard::acquire(directory.path()).unwrap();
    let root = SnapshotDirectory::pin_profile(directory.path(), 1).unwrap();
    let proof = SourceFileProof::pin(directory.path().join("native-library.sqlite3")).unwrap();
    let identity = proof.metadata().unwrap().identity.clone();
    proof.close().unwrap();
    let private = SnapshotDirectory::open_private(directory.path(), 1).unwrap();
    let permit = SnapshotLimiter::new(128 * 1024 * 1024)
        .unwrap()
        .reserve_job(8 * 1024 * 1024, 3 * 8 * 1024 * 1024 + 65 * 1024 * 1024)
        .unwrap();
    let journal = Arc::new(
        NativeSidecarJournal::create(
            &private,
            "actual-cold-writer",
            root.identity(),
            &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
            &permit,
        )
        .unwrap(),
    );
    let observed = Arc::new(RecordingJournal::new(journal));
    let mut owner = SidecarWriter::open_guarded(
        root,
        Some(identity),
        "actual-cold-writer".into(),
        8 * 1024 * 1024,
        permit,
        observed.clone(),
        guard.writer_admission().unwrap(),
    )
    .unwrap();
    assert_eq!(
        owner
            .connection()
            .unwrap()
            .query_row("SELECT value FROM payload", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "committed-before-real-process-exit"
    );
    let admitted = observed
        .creations
        .lock()
        .unwrap()
        .iter()
        .filter(|fact| {
            fact.origin == WriterSidecarOrigin::ExistingLockedProfile && fact.identity.is_some()
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        admitted
            .iter()
            .any(|fact| fact.role == WriterSidecarRole::Wal
                && fact.admitted_bytes > 32
                && fact.admitted_sha256.is_some())
    );
    owner
        .close(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
        .unwrap();
    for fact in admitted {
        assert!(
            observed
                .retirements
                .lock()
                .unwrap()
                .iter()
                .any(|retired| retired.removed
                    && retired.file.nonce == fact.nonce
                    && retired.file.identity == fact.identity)
        );
    }
    assert!(!wal_path.exists());
    assert!(
        !directory
            .path()
            .join(WriterSidecarRole::Shm.leaf())
            .exists()
    );
    guard.verify().unwrap();
}
