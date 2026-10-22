use super::*;
use crate::domain::library::{LibraryOperationState, LibraryOperationView, LibrarySeenFile};
use crate::repository::DATABASE_NAME;
use std::{
    sync::atomic::{AtomicBool, AtomicUsize},
    thread,
};

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    _cb: u32,
    _page_faults: u32,
    _peak_working_set: usize,
    working_set: usize,
    _peak_paged_pool: usize,
    _paged_pool: usize,
    _peak_nonpaged_pool: usize,
    _nonpaged_pool: usize,
    _pagefile_usage: usize,
    _peak_pagefile_usage: usize,
    private_usage: usize,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn K32GetProcessMemoryInfo(
        process: windows_sys::Win32::Foundation::HANDLE,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}
fn memory() -> (usize, usize) {
    let mut counters = ProcessMemoryCounters {
        _cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<ProcessMemoryCounters>() as u32,
        )
    };
    assert_ne!(ok, 0, "Actual Windows process RAM measurement failed");
    (counters.working_set, counters.private_usage)
}
struct MemorySampler {
    baseline: (usize, usize),
    peak: Arc<(AtomicUsize, AtomicUsize)>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl MemorySampler {
    fn start() -> Self {
        let baseline = memory();
        let peak = Arc::new((AtomicUsize::new(baseline.0), AtomicUsize::new(baseline.1)));
        let stop = Arc::new(AtomicBool::new(false));
        let observed = peak.clone();
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let sample = memory();
                observed.0.fetch_max(sample.0, Ordering::AcqRel);
                observed.1.fetch_max(sample.1, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            baseline,
            peak,
            stop,
            thread: Some(thread),
        }
    }
    fn finish(mut self) -> ((usize, usize), (usize, usize)) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
        (
            self.baseline,
            (
                self.peak.0.load(Ordering::Acquire),
                self.peak.1.load(Ordering::Acquire),
            ),
        )
    }
}
impl Drop for MemorySampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn empty_image(folder: &Path) -> std::path::PathBuf {
    let source = Repository::open(folder.join("s")).unwrap();
    let path = folder.join("selected.sqlite3");
    source
        .connection
        .lock()
        .unwrap()
        .backup("main", &path, None)
        .unwrap();
    let standalone = Connection::open(&path).unwrap();
    standalone
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    standalone.execute_batch("VACUUM").unwrap();
    drop(standalone);
    path
}
fn prepared_file(root: &RootAuthority, path: &Path) -> LibraryPreparedAsset {
    let file = root.open_file(path).unwrap();
    let identity = file.identity().clone();
    let bytes = file.metadata().unwrap().len();
    let metadata = read_local_metadata_from_open_file(file, path.to_path_buf(), bytes)
        .unwrap_or_else(|error| {
            panic!(
                "Actual fixture metadata failed: {error:#}; root grant: {:?}",
                root.check()
            )
        });
    let (mut track, asset) = crate::repository::local_track_from_path(path).unwrap();
    track.duration_ms = Some(metadata.duration_ms);
    track.metadata.insert("ext".into(), json!("wav"));
    LibraryPreparedAsset {
        track,
        asset,
        file_identity: Some(identity),
    }
}
fn populate_actual_library(repo: &Repository, folder: &Path, count: usize) -> LibraryPreparedAsset {
    let started = std::time::Instant::now();
    let directory = folder.join("m");
    fs::create_dir(&directory).unwrap();
    let root = RootAuthority::open(
        &directory,
        false,
        &ScanContext::new(Duration::from_secs(3600)),
    )
    .unwrap();
    let mut batch = Vec::with_capacity(1000);
    let mut first = None;
    for index in 0..count {
        let path = directory.join(format!("{index:05}.wav"));
        wav(&path);
        let item = prepared_file(&root, &path);
        if first.is_none() {
            first = Some(item.clone());
        }
        batch.push(item);
        if batch.len() == 1000 || index + 1 == count {
            repo.upsert_explicit_library_tracks(&batch, &|| Ok(()))
                .unwrap();
            batch.clear();
            if (index + 1) % 10_000 == 0 {
                eprintln!(
                    "Actual WAV fixture preparation: {}/{count}, elapsed={:.2}s",
                    index + 1,
                    started.elapsed().as_secs_f64()
                );
            }
        }
    }
    let barrier = root.retirement_barrier();
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    eprintln!(
        "Actual WAV fixture preparation complete: {count}, elapsed={:.2}s",
        started.elapsed().as_secs_f64()
    );
    first.unwrap()
}
fn authority_counts(connection: &Connection) -> (i64, i64, i64, i64) {
    connection.query_row(
        "SELECT (SELECT count(*) FROM tracks WHERE id IN (SELECT track_id FROM local_assets)),(SELECT count(*) FROM local_assets),(SELECT count(*) FROM local_asset_locations),(SELECT count(*) FROM local_file_grants)",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap()
}
#[test]
fn actual_owned_streaming_overlay_late_cancel_rolls_back_spilled_candidate_rows() {
    let folder = folder();
    let current = Repository::open(folder.path().join("t")).unwrap();
    populate_actual_library(&current, folder.path(), 2000);
    current
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE tracks SET document=json_set(document,'$.metadata.padding',?1)",
            ["a".repeat(8192)],
        )
        .unwrap();
    let directory = SnapshotDirectory::open_private(folder.path(), 81).unwrap();
    let limits = SnapshotLimiter::new(512 * 1024 * 1024).unwrap();
    let maximum = 32 * 1024 * 1024;
    let permit = limits
        .reserve_job(maximum, 3 * maximum + 65 * 1024 * 1024)
        .unwrap();
    let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
    let mut manifest =
        DurableManifest::create(&directory, "late-overlay-cancel", &context, &permit).unwrap();
    manifest
        .append_once(SnapshotPhase::Created, b"{}", &context)
        .unwrap();
    let manifest = Arc::new(Mutex::new(manifest));
    let mut owned = OwnedSqlite::allocate(
        directory,
        "late-overlay-cancel",
        maximum,
        context.clone(),
        permit,
        manifest.clone(),
    )
    .unwrap();
    owned.open().unwrap();
    {
        let sql = current.connection.lock().unwrap();
        let backup = rusqlite::backup::Backup::new(&sql, owned.connection_mut().unwrap()).unwrap();
        assert_eq!(backup.step(-1).unwrap(), rusqlite::backup::StepResult::Done);
    }
    owned.normalize_copied_journal_mode().unwrap();
    snapshot_sanitize::purge_snapshot_authority(owned.connection_mut().unwrap(), &|| Ok(()))
        .unwrap();
    owned.enable_restore_rollback_journal().unwrap();
    let before = CurrentAuthority::digest_current(owned.connection().unwrap(), &|| Ok(())).unwrap();
    let before_counts = authority_counts(owned.connection().unwrap());
    current
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE tracks SET document=json_set(document,'$.metadata.padding',?1)",
            ["b".repeat(8192)],
        )
        .unwrap();
    let sql = current.connection.lock().unwrap();
    let current_digest = CurrentAuthority::digest_current(&sql, &|| Ok(())).unwrap();
    let checks = std::cell::Cell::new(0usize);
    let result = CurrentAuthority::copy_into(&sql, owned.connection_mut().unwrap(), &|| {
        checks.set(checks.get() + 1);
        ensure!(
            checks.get() < 1000,
            "Actual late streaming overlay cancelled"
        );
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(checks.get(), 1000);
    assert!(owned.connection().unwrap().is_autocommit());
    assert_eq!(
        CurrentAuthority::digest_current(owned.connection().unwrap(), &|| Ok(())).unwrap(),
        before
    );
    assert_eq!(
        CurrentAuthority::digest_current(&sql, &|| Ok(())).unwrap(),
        current_digest
    );
    schema::integrity_check(owned.connection().unwrap()).unwrap();
    assert_eq!(authority_counts(owned.connection().unwrap()), before_counts);
    let checks = std::cell::Cell::new(0usize);
    let cancelled = CurrentAuthority::copy_into(&sql, owned.connection_mut().unwrap(), &|| {
        checks.set(checks.get() + 1);
        if checks.get() == 1000 {
            context.cancel();
        }
        context.check()
    });
    assert!(cancelled.is_err());
    assert_eq!(checks.get(), 1000);
    assert_eq!(limits.stats().active, 1);
    assert_eq!(
        CurrentAuthority::digest_current(&sql, &|| Ok(())).unwrap(),
        current_digest
    );
    owned.close().unwrap();
    let registry = owned.files();
    drop(owned);
    let fresh = SnapshotContext::new(Duration::from_secs(120)).unwrap();
    let mut files = registry.lock().unwrap();
    files.take_main().unwrap().cleanup(&fresh).unwrap();
    let remaining = files.take_remaining().unwrap();
    assert!(
        remaining
            .iter()
            .any(|file| file.metadata().bytes > IO_CHUNK as u64),
        "Late streaming cancellation never exercised a spilled owned rollback journal"
    );
    for file in remaining {
        file.cleanup(&fresh).unwrap();
    }
    drop(files);
    drop(registry);
    Arc::try_unwrap(manifest)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap()
        .cleanup(&fresh, &|_| Ok(()))
        .unwrap();
    assert_eq!(limits.stats().active, 0);
}
fn physical_fixture_digest(directory: &Path) -> String {
    assert_eq!(fs::read_dir(directory).unwrap().count(), 50_000);
    let mut hash = Sha256::new();
    for index in 0..50_000 {
        let name = format!("{index:05}.wav");
        let path = directory.join(&name);
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_file());
        let bytes = fs::read(path).unwrap();
        assert_eq!(bytes.len(), 60, "Actual fixture WAV changed");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        hash.update(name.as_bytes());
        hash.update(&bytes);
    }
    format!("{:x}", hash.finalize())
}

// A failing assertion must preserve both the real operation owner and its
// private directory. TempDir cleanup is not evidence of native retirement.
struct RestoreFixtureOwners {
    directory: Option<tempfile::TempDir>,
    prepared: Option<PreparedPhysicalRestore>,
    installed: Option<PhysicalProfileInstallation>,
}
impl RestoreFixtureOwners {
    fn new() -> Self {
        Self {
            directory: Some(folder()),
            prepared: None,
            installed: None,
        }
    }
    fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
}
impl Drop for RestoreFixtureOwners {
    fn drop(&mut self) {
        if self.prepared.is_some() || self.installed.is_some() {
            let path = self.directory.take().unwrap().keep();
            if let Some(owner) = self.prepared.take() {
                std::mem::forget(owner);
            }
            if let Some(owner) = self.installed.take() {
                std::mem::forget(owner);
            }
            eprintln!("Actual restore owner and private directory retained at {path:?}");
        }
    }
}

#[test]
fn actual_owned_50k_library_restore_streams_current_rows_with_bounded_ram_and_actual_image_quota() {
    let mut folder = RestoreFixtureOwners::new();
    let source = empty_image(folder.path());
    let source_sha = format!("{:x}", Sha256::digest(fs::read(&source).unwrap()));
    let fixture = std::env::var_os("LX_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| folder.path().join("fixtures"))
        .join(format!("authority-50k-schema-{SCHEMA_VERSION}"));
    {
        use std::os::windows::fs::MetadataExt;
        for ancestor in fixture.ancestors().filter(|path| path.exists()) {
            assert_eq!(
                fs::symlink_metadata(ancestor).unwrap().file_attributes() & 0x400,
                0,
                "Fixture ancestor must not be a reparse point"
            );
        }
    }
    fs::create_dir_all(&fixture).unwrap();
    assert!(
        !fs::symlink_metadata(&fixture)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    {
        use std::os::windows::fs::MetadataExt;
        assert_eq!(
            fs::symlink_metadata(&fixture).unwrap().file_attributes() & 0x400,
            0,
            "Fixture directory must not be a reparse point"
        );
    }
    let seal_path = fixture.join("fixture-seal.json");
    let database = fixture.join("baseline.sqlite3");
    let reused = seal_path.exists();
    if !seal_path.exists() {
        assert!(
            fs::read_dir(&fixture).unwrap().next().is_none(),
            "Unsealed fixture requires explicit review; never adopt partially generated data"
        );
        let baseline = Arc::new(Repository::open(fixture.join("profile")).unwrap());
        let first = populate_actual_library(&baseline, &fixture, 50_000);
        assert_eq!(
            authority_counts(&baseline.connection.lock().unwrap()),
            (50_000, 50_000, 50_000, 50_000)
        );
        let digest =
            CurrentAuthority::digest_current(&baseline.connection.lock().unwrap(), &|| Ok(()))
                .unwrap();
        baseline.backup_to(&database).unwrap();
        let actual = Connection::open(&database).unwrap();
        actual
            .pragma_update(None, "journal_mode", "DELETE")
            .unwrap();
        actual.execute_batch("VACUUM").unwrap();
        schema::integrity_check(&actual).unwrap();
        drop(actual);
        let guard = baseline.close_into_guard().unwrap();
        drop(guard);
        let database_sha = format!("{:x}", Sha256::digest(fs::read(&database).unwrap()));
        let wav_sha = physical_fixture_digest(&fixture.join("m"));
        fs::write(&seal_path, serde_json::to_vec(&json!({"fixture_version":1,"schema":SCHEMA_VERSION,"count":50000,"database_sha256":database_sha,"authority_sha256":digest,"wav_inventory_sha256":wav_sha,"first_asset_id":first.asset.id,"first_track_id":first.track.id})).unwrap()).unwrap();
    }
    {
        use std::os::windows::fs::MetadataExt;
        for path in [&seal_path, &database] {
            let metadata = fs::symlink_metadata(path).unwrap();
            assert!(metadata.file_type().is_file());
            assert_eq!(
                metadata.file_attributes() & 0x400,
                0,
                "Cached file must not be a reparse point"
            );
        }
        let directory = fs::symlink_metadata(fixture.join("m")).unwrap();
        assert!(directory.file_type().is_dir());
        assert_eq!(
            directory.file_attributes() & 0x400,
            0,
            "Cached WAV directory must not be a reparse point"
        );
    }
    let seal: serde_json::Value = serde_json::from_slice(&fs::read(&seal_path).unwrap()).unwrap();
    assert_eq!(seal["fixture_version"], 1);
    assert_eq!(seal["schema"], SCHEMA_VERSION);
    assert_eq!(seal["count"], 50_000);
    assert_eq!(
        seal["database_sha256"],
        format!("{:x}", Sha256::digest(fs::read(&database).unwrap()))
    );
    assert_eq!(
        seal["wav_inventory_sha256"],
        physical_fixture_digest(&fixture.join("m"))
    );
    fs::create_dir(folder.path().join("t")).unwrap();
    fs::copy(&database, folder.path().join("t").join(DATABASE_NAME)).unwrap();
    let current = Arc::new(Repository::open(folder.path().join("t")).unwrap());
    assert_eq!(
        authority_counts(&current.connection.lock().unwrap()),
        (50_000, 50_000, 50_000, 50_000)
    );
    assert_eq!(
        seal["authority_sha256"],
        CurrentAuthority::digest_current(&current.connection.lock().unwrap(), &|| Ok(())).unwrap()
    );
    let actual_root = RootAuthority::open(
        &fixture.join("m"),
        false,
        &ScanContext::new(Duration::from_secs(3600)),
    )
    .unwrap();
    if reused {
        let connection = current.connection.lock().unwrap();
        let mut statement = connection
            .prepare("SELECT document FROM local_asset_locations ORDER BY asset_id")
            .unwrap();
        let mut rows = statement.query([]).unwrap();
        let mut checked = 0usize;
        while let Some(row) = rows.next().unwrap() {
            let location: crate::repository::library::AssetLocation =
                serde_json::from_str(row.get_ref(0).unwrap().as_str().unwrap()).unwrap();
            let file = actual_root.open_file(&location.path).unwrap();
            assert_eq!(location.file_identity.as_ref(), Some(file.identity()));
            assert_eq!(file.metadata().unwrap().len(), 60);
            checked += 1;
        }
        assert_eq!(checked, 50_000);
        eprintln!("Actual cached fixture identities revalidated: {checked}");
    }
    let first = prepared_file(&actual_root, &fixture.join("m").join("00000.wav"));
    assert_eq!(seal["first_asset_id"], first.asset.id);
    assert_eq!(seal["first_track_id"], first.track.id);
    let barrier = actual_root.retirement_barrier();
    drop(actual_root);
    assert!(barrier.wait(Duration::from_secs(3)));
    let root_path = first.asset.path.parent().unwrap();
    let (root, _) = current.add_library_root(&selected_root(root_path)).unwrap();
    let (token, _) = current
        .begin_library_scan(&root.root_id, "finished-inventory")
        .unwrap();
    current
        .apply_library_scan_batch(
            &token,
            &[LibrarySeenFile {
                path: first.asset.path.clone(),
                asset_id: Some(first.asset.id.clone()),
            }],
            &[],
            &|| Ok(()),
        )
        .unwrap();
    let mut finished = LibraryOperationView::running(&token);
    finished.state = LibraryOperationState::Completed;
    finished.entries_examined = 1;
    finished.discovered_files = 1;
    current
        .finish_library_scan(&token, &finished, true, &|| Ok(()))
        .unwrap();
    let expected_root = current.library_root(&root.root_id).unwrap().unwrap();
    let expected_finished = current
        .library_operation("finished-inventory")
        .unwrap()
        .unwrap();
    let expected_binding = current
        .library_asset_binding(&first.asset.id)
        .unwrap()
        .unwrap();
    let expected_document = document(&current, "tracks", "id", &first.track.id);
    let expected_watch = current.library_watch(&root.root_id).unwrap();
    let expected_dirty = current.library_dirty(&root.root_id).unwrap();
    assert_eq!(
        authority_counts(&current.connection.lock().unwrap()),
        (50_000, 50_000, 50_000, 50_000)
    );
    let expected_digest =
        CurrentAuthority::digest_current(&current.connection.lock().unwrap(), &|| Ok(())).unwrap();
    let limits = PhysicalRestoreLimits::new(4 * 1024 * 1024 * 1024).unwrap();
    let restore_started = std::time::Instant::now();
    let context = PhysicalRestoreContext::new(Duration::from_secs(120)).unwrap();
    let request = PhysicalRestoreRequest {
        operation_id: "owned-50k-stream".into(),
        expected_revision: current.revision().unwrap(),
        maximum_image_bytes: 256 * 1024 * 1024,
        selection_epoch: 71,
    };
    let sampler = MemorySampler::start();
    folder.prepared = Some(
        match current.prepare_physical_restore(
            request.clone(),
            PhysicalRestoreSelection::open_selected(&source).unwrap(),
            &context,
            &limits,
            &|_| Ok(()),
        ) {
            Ok(owner) => owner,
            Err(mut failure) => {
                let message = failure.message.clone();
                if let Some(owner) = failure.owner.take() {
                    if let Err(mut retained) = (*owner)
                        .abort(&PhysicalRestoreContext::new(Duration::from_secs(120)).unwrap())
                    {
                        folder.prepared = retained.owner.take().map(|owner| *owner);
                        panic!(
                            "50k prepare failed and actual cleanup owner remains retained: {message}"
                        );
                    }
                }
                panic!("50k actual preparation failed: {message}");
            }
        },
    );
    let (baseline, peak) = sampler.finish();
    let growth = (
        peak.0.saturating_sub(baseline.0),
        peak.1.saturating_sub(baseline.1),
    );
    let view = folder.prepared.as_ref().unwrap().view().unwrap();
    let report = view.authority.as_ref().unwrap();
    eprintln!(
        "50k authority RAM: working-set baseline={} peak={} growth={}; private baseline={} peak={} growth={}; actual image={} quota={}; rows={}; preparation_seconds={:.3}",
        baseline.0,
        peak.0,
        growth.0,
        baseline.1,
        peak.1,
        growth.1,
        view.image_bytes,
        request.maximum_image_bytes,
        report.preserved_rows,
        restore_started.elapsed().as_secs_f64()
    );
    assert!(
        growth.0 <= 64 * 1024 * 1024 && growth.1 <= 64 * 1024 * 1024,
        "Actual 50k restore RAM growth exceeded 64 MiB: {growth:?}"
    );
    assert_eq!(report.source_sha256, expected_digest);
    assert_eq!(report.matched_assets, 50_000);
    assert_eq!(report.preserved_roots, 1);
    assert!(report.preserved_rows >= 200_000);
    assert!(view.image_bytes > 32 * 1024 * 1024 && view.image_bytes <= request.maximum_image_bytes);
    assert_eq!(
        limits.stats(),
        (1, request.maximum_image_bytes * 3 + 65 * 1024 * 1024)
    );
    let guard = current.close_into_guard().unwrap();
    eprintln!(
        "50k authority closed cut: {:.3}s",
        restore_started.elapsed().as_secs_f64()
    );
    folder.installed = Some(
        match folder
            .prepared
            .take()
            .unwrap()
            .install_files(guard, &context)
        {
            Ok(owner) => owner,
            Err(failure) => {
                let message = failure.message.clone();
                folder.installed = Some(*failure.owner);
                panic!("50k install retained its actual owner: {message}");
            }
        },
    );
    eprintln!(
        "50k authority installed cut: {:.3}s",
        restore_started.elapsed().as_secs_f64()
    );
    folder.installed = Some(match folder.installed.take().unwrap().reopen(&context) {
        Ok(owner) => owner,
        Err(failure) => {
            let message = failure.message.clone();
            folder.installed = Some(*failure.owner);
            panic!("50k reopen retained its actual owner: {message}");
        }
    });
    eprintln!(
        "50k authority reopened cut: {:.3}s",
        restore_started.elapsed().as_secs_f64()
    );
    let restored = folder.installed.as_ref().unwrap().repository().unwrap();
    assert_eq!(
        authority_counts(&restored.connection.lock().unwrap()),
        (50_000, 50_000, 50_000, 50_000)
    );
    assert_eq!(
        restored.library_root(&root.root_id).unwrap().unwrap(),
        expected_root
    );
    assert_eq!(
        restored
            .library_operation("finished-inventory")
            .unwrap()
            .unwrap(),
        expected_finished
    );
    assert_eq!(
        restored.library_watch(&root.root_id).unwrap(),
        expected_watch
    );
    assert_eq!(
        restored.library_dirty(&root.root_id).unwrap(),
        expected_dirty
    );
    assert_eq!(
        restored
            .library_asset_binding(&first.asset.id)
            .unwrap()
            .unwrap(),
        expected_binding
    );
    assert_eq!(
        document(restored, "tracks", "id", &first.track.id),
        expected_document
    );
    restored.integrity_check().unwrap();
    let terminal = match folder.installed.take().unwrap().commit(&context) {
        Ok(terminal) => terminal,
        Err(failure) => {
            let message = failure.message.clone();
            folder.installed = Some(*failure.owner);
            panic!("50k commit retained its actual owner: {message}");
        }
    };
    eprintln!(
        "50k authority committed: {:.3}s",
        restore_started.elapsed().as_secs_f64()
    );
    assert_eq!(limits.stats(), (0, 0));
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&source).unwrap())),
        source_sha
    );

    // The large current model is an input stream, never an authorization to
    // exceed a smaller candidate's actual main/journal byte bounds.
    let current = terminal.into_repository();
    let target_digest =
        CurrentAuthority::digest_current(&current.connection.lock().unwrap(), &|| Ok(())).unwrap();
    let small_request = PhysicalRestoreRequest {
        operation_id: "owned-50k-small-image".into(),
        expected_revision: current.revision().unwrap(),
        maximum_image_bytes: 8 * 1024 * 1024,
        selection_epoch: 72,
    };
    let small_context = PhysicalRestoreContext::new(Duration::from_secs(120)).unwrap();
    let failure = current
        .prepare_physical_restore(
            small_request.clone(),
            PhysicalRestoreSelection::open_selected(&source).unwrap(),
            &small_context,
            &limits,
            &|_| Ok(()),
        )
        .unwrap_err();
    folder.prepared = failure.owner.map(|owner| *owner);
    let owner = folder.prepared.as_ref().unwrap();
    assert!(owner.view().is_none());
    assert!(owner.sqlite.is_some());
    assert!(
        small_context.inner.check().is_ok(),
        "Small image rejection must be quota enforcement rather than timeout"
    );
    let quota_error = owner
        .sqlite
        .as_ref()
        .unwrap()
        .files()
        .lock()
        .unwrap()
        .last_error()
        .map(str::to_owned)
        .unwrap_or_default();
    assert!(
        quota_error.contains("Physical snapshot operation: Quota"),
        "Actual 8 MiB VFS quota was not the rejection: {quota_error}"
    );
    assert_eq!(
        limits.stats(),
        (1, small_request.maximum_image_bytes * 3 + 65 * 1024 * 1024)
    );
    assert_eq!(
        CurrentAuthority::digest_current(&current.connection.lock().unwrap(), &|| Ok(())).unwrap(),
        target_digest
    );
    if let Err(mut failure) = folder
        .prepared
        .take()
        .unwrap()
        .abort(&PhysicalRestoreContext::new(Duration::from_secs(120)).unwrap())
    {
        folder.prepared = failure.owner.take().map(|owner| *owner);
        panic!(
            "Small image cleanup retained its actual owner: {}",
            failure.message
        );
    }
    assert_eq!(limits.stats(), (0, 0));
    current.integrity_check().unwrap();
}
