use anyhow::Result;
use lx_gpui_player::{
    download::{
        DownloadCancel, DownloadContext, DownloadPlan, OwnedPart, OwnedPartRecord,
        artifact::{
            OwnedArtifactAllocation, OwnedArtifactIntent, OwnedArtifactJournal, OwnedArtifactKind,
        },
        run_download_producer,
    },
    repository::Repository,
};
use std::{
    collections::BTreeMap,
    fs,
    ops::Deref,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};

pub struct FixtureDirectory(Option<tempfile::TempDir>);
fn within_fixture(path: &Path, directory: &Path) -> bool {
    // Verbatim and ordinary Windows paths describe the same fixture scope.
    // Actual retirement still requires the retained native owner and proofs.
    lx_gpui_player::domain::music::normalize_local_path(path).starts_with(
        lx_gpui_player::domain::music::normalize_local_path(directory),
    )
}
impl FixtureDirectory {
    pub fn preserve(&mut self) {
        if let Some(directory) = self.0.take() {
            let _ = directory.keep();
        }
    }
}
impl Deref for FixtureDirectory {
    type Target = tempfile::TempDir;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let Some(directory) = self.0.take() else {
            return;
        };
        let preserve = std::thread::panicking()
            || FAILED_GRANTS.get().is_some_and(|grants| {
                grants.try_lock().map_or(true, |grants| {
                    grants
                        .iter()
                        .any(|grant| within_fixture(grant.path(), directory.path()))
                })
            })
            || FIXTURES.get().is_some_and(|registry| {
                registry.try_lock().map_or(true, |fixtures| {
                    fixtures.values().any(|fixture| {
                        within_fixture(fixture.plan.grant().path(), directory.path())
                    })
                })
            });
        if preserve {
            let _ = directory.keep();
        }
    }
}
pub fn fixture_directory() -> Result<FixtureDirectory> {
    let root = Path::new(r"E:\codex操作目录\tasks\lx-gpui-player\test-fixtures");
    fs::create_dir_all(root)?;
    Ok(FixtureDirectory(Some(
        tempfile::Builder::new()
            .prefix("native-producer-")
            .tempdir_in(root)?,
    )))
}

/// SQLite controller facts are not native file or deletion authority.
pub fn record_metadata_allocation(
    repository: &Repository,
    allocation: &OwnedArtifactAllocation,
    guard: &dyn Fn() -> Result<()>,
) -> Result<()> {
    repository
        .record_download_allocation_intent(&OwnedArtifactIntent::from_created(allocation), guard)?;
    repository.record_download_artifact(allocation, guard)
}

pub struct RepositoryJournal {
    pub repository: Arc<Repository>,
    observed: Mutex<Vec<OwnedArtifactAllocation>>,
    retirements: Mutex<BTreeMap<String, (OwnedPartRecord, bool)>>,
    plans: Mutex<Vec<DownloadPlan>>,
}
impl RepositoryJournal {
    pub fn new(repository: Arc<Repository>) -> Self {
        Self {
            repository,
            observed: Mutex::new(Vec::new()),
            retirements: Mutex::new(BTreeMap::new()),
            plans: Mutex::new(Vec::new()),
        }
    }
    pub fn observed_allocations(&self) -> Vec<OwnedArtifactAllocation> {
        self.observed.lock().unwrap().clone()
    }
    fn remember_plan(&self, plan: DownloadPlan) {
        let mut plans = self.plans.lock().unwrap();
        if !plans.iter().any(|old| {
            old.basename() == plan.basename()
                && old.grant().identity() == plan.grant().identity()
                && old.grant().working_directory_identity()
                    == plan.grant().working_directory_identity()
                && old.grant().epoch() == plan.grant().epoch()
        }) {
            plans.push(plan);
        }
    }
}

struct RegisteredFixture {
    journal: Arc<RepositoryJournal>,
    plan: DownloadPlan,
    context: DownloadContext,
    _directory: FixtureDirectory,
}
static FIXTURES: OnceLock<Mutex<BTreeMap<String, Arc<RegisteredFixture>>>> = OnceLock::new();
static FAILED_GRANTS: OnceLock<Mutex<Vec<lx_gpui_player::download::DestinationGrant>>> =
    OnceLock::new();
fn fixtures() -> &'static Mutex<BTreeMap<String, Arc<RegisteredFixture>>> {
    FIXTURES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn standalone_fixture(
    plan: &DownloadPlan,
    context: &DownloadContext,
) -> Result<Arc<RegisteredFixture>> {
    use lx_gpui_player::{
        domain::music::{LxMusicDtoVersion, TrackRecord},
        download::{CollisionPolicy, DownloadState, planning::DownloadQuality, task_model::*},
    };
    let directory = fixture_directory()?;
    let repository = Arc::new(Repository::open(directory.path().join("profile"))?);
    let grouped = plan.grant().identity() != plan.grant().working_directory_identity();
    let (root, relative_components) = if grouped {
        let root = plan
            .grant()
            .path()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Fixture group lacks its selected parent"))?;
        let name = plan
            .grant()
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("Fixture group has no basename"))?;
        (
            root.to_owned(),
            vec![name.to_owned(), plan.basename().to_owned()],
        )
    } else {
        (
            plan.grant().path().to_owned(),
            vec![plan.basename().to_owned()],
        )
    };
    let quality = match plan.basename().rsplit('.').next() {
        Some("mp3") => DownloadQuality::K128,
        Some("flac") => DownloadQuality::Flac,
        Some("wav") => DownloadQuality::Wav,
        Some("ape") => DownloadQuality::Ape,
        _ => DownloadQuality::K128,
    };
    repository.put_download_destination(
        &StoredDestinationGrant {
            id: "native-fixture-destination".into(),
            profile_id: context.profile_id.clone(),
            path: root,
            identity: plan.grant().identity().clone(),
            epoch: plan.grant().epoch(),
            revision: 1,
            revoked: false,
        },
        None,
        None,
        &|| Ok(()),
    )?;
    let settings = DownloadSettings {
        embed_cover: false,
        download_lyrics: true,
        ..Default::default()
    };
    let mut task = DownloadTask {
        id: context.task_id.clone(),
        profile_id: context.profile_id.clone(),
        original_record: TrackRecord::from_lx_dto(
            serde_json::json!({"id":"kw_native_fixture","source":"kw","name":"Native producer fixture","meta":{"songId":"native-fixture"}}),
            LxMusicDtoVersion::Modern,
        )?,
        resolved_record: None,
        requested_quality: quality,
        actual_quality: quality,
        list_id: None,
        list_name_snapshot: None,
        order: 0,
        revision: 1,
        operation_epoch: 1,
        settings: settings.clone(),
        destination: DownloadDestinationBinding {
            grant_id: "native-fixture-destination".into(),
            grant_epoch: plan.grant().epoch(),
            root_identity: plan.grant().identity().clone(),
            relative_components,
            collision: CollisionPolicy::Skip,
        },
        state: DownloadState::Waiting,
        pause_reason: None,
        owned_part: None,
        original_transfer: None,
        original_representation: None,
        url_bookmark: None,
        transfer_attempts: 0,
        retry_at_ms: None,
        error: None,
        output_audio: None,
        audio_available: false,
        stages: DownloadStageRecords::for_settings(&settings, quality),
        commit_journal: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    repository.insert_download_tasks(&[task.clone()], None, &|| Ok(()))?;
    task.operation_epoch = context.operation_epoch;
    task.revision = 2;
    task.state = DownloadState::Transferring;
    task.url_bookmark = Some(PrivateUrlBookmark::new(
        "https://fixture.invalid/approved-native-body".into(),
        context.clone(),
    )?);
    repository.update_download_task(&task, 1, 1, None, &|| Ok(()))?;
    let journal = Arc::new(RepositoryJournal::new(repository));
    journal.remember_plan(plan.clone());
    Ok(Arc::new(RegisteredFixture {
        journal,
        plan: plan.clone(),
        context: context.clone(),
        _directory: directory,
    }))
}

pub fn create(plan: DownloadPlan, context: &DownloadContext) -> Result<OwnedPart> {
    let fixture = standalone_fixture(&plan, context)?;
    let part = fixture.journal.create(
        plan,
        OwnedArtifactKind::Transfer,
        context.clone(),
        lx_gpui_player::download::MAX_DOWNLOAD_BYTES,
        DownloadCancel::default(),
    )?;
    fixtures()
        .lock()
        .unwrap()
        .insert(part.record().nonce.clone(), fixture);
    Ok(part)
}

pub fn journal_for_plan(
    plan: &DownloadPlan,
    context: &DownloadContext,
) -> Result<Arc<RepositoryJournal>> {
    let fixture = standalone_fixture(plan, context)?;
    let key = format!(
        "pending-plan:{}:{}:{}:{}",
        context.profile_id,
        context.task_id,
        plan.grant().working_directory_identity().object,
        plan.basename()
    );
    let journal = fixture.journal.clone();
    fixtures().lock().unwrap().insert(key, fixture);
    Ok(journal)
}

pub fn reopen(
    plan: DownloadPlan,
    record: OwnedPartRecord,
    context: &DownloadContext,
) -> Result<OwnedPart> {
    let fixture = fixtures()
        .lock()
        .unwrap()
        .get(&record.nonce)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Fixture has no actual journaled creation"))?;
    anyhow::ensure!(
        fixture.plan.path() == plan.path()
            && fixture.plan.grant().identity() == plan.grant().identity()
            && fixture.context.profile_id == context.profile_id
            && fixture.context.task_id == context.task_id,
        "Fixture reopen changed its original namespace or task"
    );
    let mut task = fixture
        .journal
        .repository
        .download_task(&context.task_id)?
        .ok_or_else(|| anyhow::anyhow!("Fixture task disappeared"))?;
    let old_revision = task.revision;
    let old_epoch = task.operation_epoch;
    task.revision += 1;
    task.operation_epoch = context.operation_epoch;
    task.owned_part = Some(record.clone());
    fixture.journal.repository.update_download_task(
        &task,
        old_revision,
        old_epoch,
        None,
        &|| Ok(()),
    )?;
    fixture.journal.reopen(
        plan,
        record,
        context.clone(),
        lx_gpui_player::download::MAX_DOWNLOAD_BYTES,
        DownloadCancel::default(),
    )
}

pub fn bind_url(record: &OwnedPartRecord, url: &str) -> Result<()> {
    use lx_gpui_player::download::task_model::PrivateUrlBookmark;
    let fixture = fixtures()
        .lock()
        .unwrap()
        .get(&record.nonce)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Approved fixture URL has no journaled native owner"))?;
    let mut task = fixture
        .journal
        .repository
        .download_task(&fixture.context.task_id)?
        .ok_or_else(|| anyhow::anyhow!("Fixture task disappeared"))?;
    let old_revision = task.revision;
    let epoch = task.operation_epoch;
    task.revision += 1;
    task.owned_part = Some(record.clone());
    task.url_bookmark = Some(PrivateUrlBookmark::new(
        url.to_owned(),
        fixture.context.clone(),
    )?);
    fixture
        .journal
        .repository
        .update_download_task(&task, old_revision, epoch, None, &|| Ok(()))
        .map(|_| ())
}

pub fn journal(record: &OwnedPartRecord) -> Result<Arc<RepositoryJournal>> {
    fixtures()
        .lock()
        .unwrap()
        .get(&record.nonce)
        .map(|fixture| fixture.journal.clone())
        .ok_or_else(|| anyhow::anyhow!("Fixture original has no actual creation journal"))
}

pub fn commit(
    audio: Box<lx_gpui_player::download::VerifiedOwnedAudio>,
) -> Result<lx_gpui_player::download::CommittedAudio> {
    run_download_producer(move || audio.commit())
}

pub fn copy(
    original: Box<lx_gpui_player::download::VerifiedOwnedAudio>,
    cancel: DownloadCancel,
) -> Result<(
    Box<lx_gpui_player::download::VerifiedOwnedAudio>,
    Result<OwnedPart>,
)> {
    let journal = journal(original.owned_record())?;
    run_download_producer(move || {
        let copy = original.copy_for_postprocess_journaled(&cancel, journal.as_ref());
        Ok((original, copy))
    })
}

pub fn tagged(
    original: Box<lx_gpui_player::download::VerifiedOwnedAudio>,
    patch: lx_gpui_player::library::tag_write::LocalTagPatch,
    cancel: DownloadCancel,
    timeout: std::time::Duration,
) -> Result<(
    Box<lx_gpui_player::download::VerifiedOwnedAudio>,
    Result<lx_gpui_player::download::postprocess::PreparedTaggedOutput>,
)> {
    let journal = journal(original.owned_record())?;
    run_download_producer(move || {
        let prepared = lx_gpui_player::download::postprocess::prepare_tagged_output_journaled(
            &original,
            &patch,
            &cancel,
            timeout,
            journal.as_ref(),
        );
        Ok((original, prepared))
    })
}

pub fn sidecar(
    original: Box<lx_gpui_player::download::VerifiedOwnedAudio>,
    artifact: lx_gpui_player::download::lyrics::LyricArtifact,
    collision: lx_gpui_player::download::CollisionPolicy,
    cancel: DownloadCancel,
) -> Result<(
    Box<lx_gpui_player::download::VerifiedOwnedAudio>,
    Result<lx_gpui_player::download::postprocess::PreparedLyricSidecar>,
)> {
    let journal = journal(original.owned_record())?;
    run_download_producer(move || {
        let prepared = lx_gpui_player::download::postprocess::prepare_lyric_sidecar_journaled(
            &original,
            &artifact,
            collision,
            &cancel,
            journal.as_ref(),
        );
        Ok((original, prepared))
    })
}
impl OwnedArtifactJournal for RepositoryJournal {
    fn record_intent(&self, intent: &OwnedArtifactIntent) -> Result<()> {
        // Capture a plan before creation, while a sidecar destination is still
        // absent. Retirement must not ask for a new create plan after publication.
        let mut plans = self.plans.lock().unwrap();
        let matches_scope = |plan: &DownloadPlan| {
            plan.grant().identity() == &intent.selected_root_identity
                && plan.grant().working_directory_identity() == &intent.working_directory_identity
                && plan.grant().epoch() == intent.destination_epoch
        };
        if !plans
            .iter()
            .any(|plan| matches_scope(plan) && plan.basename() == intent.final_basename)
            && let Some(grant) = plans
                .iter()
                .find(|plan| matches_scope(plan))
                .map(DownloadPlan::grant)
        {
            let derived = grant.plan(
                &intent.final_basename,
                lx_gpui_player::download::CollisionPolicy::Skip,
            )?;
            plans.push(derived);
        }
        drop(plans);
        self.repository
            .record_download_allocation_intent(intent, &|| Ok(()))
    }
    fn record_created(&self, allocation: &OwnedArtifactAllocation) -> Result<()> {
        let mut observed = self.observed.lock().unwrap();
        if !observed
            .iter()
            .any(|old| old.record.nonce == allocation.record.nonce)
        {
            observed.push(allocation.clone());
        }
        drop(observed);
        self.repository
            .record_download_artifact(allocation, &|| Ok(()))
    }
    fn record_reopened(&self, allocation: &OwnedArtifactAllocation) -> Result<()> {
        self.repository
            .acknowledge_download_artifact_reopen(allocation, &|| Ok(()))
    }
}

pub fn retire_fixture(record: &OwnedPartRecord) -> Result<OwnedPartRecord> {
    let fixture = fixtures()
        .lock()
        .unwrap()
        .values()
        .find(|fixture| {
            fixture
                .journal
                .observed_allocations()
                .iter()
                .any(|allocation| allocation.record == *record)
        })
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("Fixture retirement lacks an actual creation observation")
        })?;
    let previous = fixture
        .journal
        .retirements
        .lock()
        .unwrap()
        .get(&record.nonce)
        .cloned();
    let receipt = if let Some((receipt, acknowledged)) = previous {
        anyhow::ensure!(
            receipt == *record,
            "Retained actual fixture removal receipt changed"
        );
        if acknowledged {
            return Ok(receipt);
        }
        receipt
    } else {
        let fresh_selection = fixture.plan.grant().is_revoked();
        let mut grant = if fresh_selection {
            let allocation = fixture
                .journal
                .observed_allocations()
                .into_iter()
                .find(|allocation| allocation.record == *record)
                .ok_or_else(|| anyhow::anyhow!("Fixture recovery lost its actual capture"))?;
            let root = if allocation.relative_group.is_some() {
                allocation.working_directory_path.parent().unwrap()
            } else {
                allocation.working_directory_path.as_path()
            };
            anyhow::ensure!(
                within_fixture(
                    root,
                    Path::new(r"E:\codex操作目录\tasks\lx-gpui-player\test-fixtures")
                ),
                "Fixture recovery left its owned task arena"
            );
            let selected = lx_gpui_player::download::DestinationGrant::open_selected(
                root,
                record.destination_epoch,
            )?;
            anyhow::ensure!(
                selected.identity() == &record.destination_identity,
                "Fresh fixture selection has a foreign root FID"
            );
            let selected = if let Some(group) = allocation.relative_group {
                selected.open_existing_group(&group)?
            } else {
                selected
            };
            anyhow::ensure!(
                selected.working_directory_identity() == &allocation.working_directory_identity,
                "Fresh fixture selection has a foreign working-directory FID"
            );
            selected
        } else {
            fixture.plan.grant().clone()
        };
        let plan = if fresh_selection {
            grant.plan(
                &record.final_basename,
                lx_gpui_player::download::CollisionPolicy::Skip,
            )?
        } else {
            fixture
                .journal
                .plans
                .lock()
                .unwrap()
                .iter()
                .find(|plan| {
                    plan.basename() == record.final_basename
                        && plan.grant().identity() == &record.destination_identity
                        && plan.grant().epoch() == record.destination_epoch
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Fixture retirement lost its pre-creation plan"))?
        };
        let receipt = OwnedPart::retire_retained(plan, record.clone())?;
        fixture
            .journal
            .retirements
            .lock()
            .unwrap()
            .insert(record.nonce.clone(), (receipt.clone(), false));
        if fresh_selection {
            if let Err(error) = grant.close_retained_unique() {
                FAILED_GRANTS
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                    .unwrap()
                    .push(grant);
                return Err(error);
            }
        }
        receipt
    };
    anyhow::ensure!(
        receipt == *record,
        "Actual fixture retirement changed its original identity"
    );
    let connection = rusqlite::Connection::open(fixture.journal.repository.path())?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS test_native_retirement_receipts(nonce TEXT PRIMARY KEY, record TEXT NOT NULL);")?;
    connection.execute(
        "INSERT INTO test_native_retirement_receipts(nonce,record) VALUES(?1,?2)",
        rusqlite::params![receipt.nonce, serde_json::to_string(&receipt)?],
    )?;
    fixture
        .journal
        .retirements
        .lock()
        .unwrap()
        .get_mut(&record.nonce)
        .unwrap()
        .1 = true;
    Ok(receipt)
}

pub fn remove(part: OwnedPart) -> Result<OwnedPartRecord> {
    let record = part.record().clone();
    let fixture = fixtures()
        .lock()
        .unwrap()
        .values()
        .find(|fixture| {
            fixture
                .journal
                .observed_allocations()
                .iter()
                .any(|allocation| allocation.record == record)
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Fixture removal lacks its actual creation observation"))?;
    let receipt = part.remove_owned_after_task_commit()?;
    anyhow::ensure!(
        receipt == record,
        "Actual fixture removal changed its original identity"
    );
    fixture
        .journal
        .retirements
        .lock()
        .unwrap()
        .insert(record.nonce.clone(), (receipt, false));
    retire_fixture(&record)
}

pub fn finish_directory(
    directory: &Path,
    mut grant: lx_gpui_player::download::DestinationGrant,
) -> Result<()> {
    let result = finish_registry(directory).and_then(|()| grant.close_retained_unique());
    if let Err(error) = result {
        FAILED_GRANTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push(grant);
        return Err(error);
    }
    anyhow::ensure!(
        grant.verify().is_err(),
        "Fixture grant still has complete native pins after checked close"
    );
    Ok(())
}

fn finish_registry(directory: &Path) -> Result<()> {
    let registered = fixtures()
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, fixture)| within_fixture(fixture.plan.grant().path(), directory))
        .map(|(key, fixture)| (key.clone(), fixture.clone()))
        .collect::<Vec<_>>();
    for (_, fixture) in &registered {
        for allocation in fixture.journal.observed_allocations() {
            let removed = fixture
                .journal
                .retirements
                .lock()
                .unwrap()
                .get(&allocation.record.nonce)
                .is_some_and(|(_, acknowledged)| *acknowledged);
            if !removed {
                retire_fixture(&allocation.record)?;
            }
        }
        fixture.journal.plans.lock().unwrap().clear();
    }
    let removed = {
        let mut registry = fixtures().lock().unwrap();
        registered
            .into_iter()
            .filter_map(|(key, _)| registry.remove(&key))
            .collect::<Vec<_>>()
    };
    drop(removed);
    Ok(())
}

pub fn retain_failed_grant(grant: lx_gpui_player::download::DestinationGrant) {
    FAILED_GRANTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .push(grant);
}

pub fn retry_failed_grants() -> Result<()> {
    let mut pending = std::mem::take(
        &mut *FAILED_GRANTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap(),
    );
    let mut failure = None;
    for mut grant in pending.drain(..) {
        if let Err(error) = grant.close_retained_unique() {
            retain_failed_grant(grant);
            failure = Some(error);
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
impl RepositoryJournal {
    pub fn create(
        self: &Arc<Self>,
        plan: DownloadPlan,
        kind: OwnedArtifactKind,
        context: DownloadContext,
        maximum_bytes: u64,
        cancel: DownloadCancel,
    ) -> Result<OwnedPart> {
        self.remember_plan(plan.clone());
        let journal = self.clone();
        run_download_producer(move || {
            OwnedPart::create_journaled(
                plan,
                kind,
                &context,
                maximum_bytes,
                journal.as_ref(),
                &cancel,
            )
        })
    }
    pub fn reopen(
        self: &Arc<Self>,
        plan: DownloadPlan,
        record: OwnedPartRecord,
        context: DownloadContext,
        maximum_bytes: u64,
        cancel: DownloadCancel,
    ) -> Result<OwnedPart> {
        self.remember_plan(plan.clone());
        let journal = self.clone();
        run_download_producer(move || {
            OwnedPart::reopen_journaled(
                plan,
                record,
                &context,
                maximum_bytes,
                journal.as_ref(),
                &cancel,
            )
        })
    }
}

/// Captures the real identity of a closed fixture file for read-only recovery.
pub fn file_identity(path: &Path) -> lx_gpui_player::download::FileIdentity {
    let file = std::fs::File::open(path).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
        };
        let mut info = FILE_ID_INFO::default();
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            },
            0
        );
        lx_gpui_player::download::FileIdentity {
            device: info.VolumeSerialNumber,
            object: info
                .FileId
                .Identifier
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().unwrap();
        lx_gpui_player::download::FileIdentity {
            device: metadata.dev(),
            object: metadata.ino().to_string(),
        }
    }
}
