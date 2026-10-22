use super::*;
use crate::audio::{
    cache::CacheStats,
    transport::{ApprovedAudioRequest, CacheIdentity, TransportOptions},
};
use crate::platforms::{ProviderContext, ReqwestTransport, TransportConfig};
use crate::playback::preload::{
    Invalidation, PreloadAuthority, PreloadContext, PreloadFailure, PreloadService, PreloadState,
    PrepareAction, ResolveAction, TriggerTiming,
};
use crate::{
    audio::{AudioOutputMode, AudioPlayer, PlaybackStatus},
    catalog::{Catalog, PreparedLocalImport},
    source::{ImportedSource, SourceRequest, SourceResult},
    store::{ProjectStore, StoreData},
    worker::{SourceWorker, WorkerCancel},
};
use crate::{
    domain::lists::{DEFAULT_LIST, DislikeMusicInfo},
    playback::queue::{AdvanceReason, QueueState},
};
use crate::{
    domain::lyric::{LyricBundle, LyricOrigin},
    lyrics::{
        service::{LyricRequest, LyricService, LyricServiceError, PreparedLyrics},
        timeline::{AudioClockSnapshot, ClockStatus, LyricTimeline},
    },
};
use crate::{
    domain::music::{LocalAsset, PlatformTrackRef, TrackRecord},
    repository::Repository,
};
use anyhow::{Context, Result, ensure};
use pictures::PictureSourceRequest;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
pub(super) mod backup_export;
mod backups;
pub(super) mod deeplink;
pub(super) mod download_cleanup;
mod download_reads;
mod downloads;
pub(super) mod effects_control;
mod effects_service;
pub(super) mod instance_relay;
mod library;
mod library_relink;
mod library_relink_cold;
mod library_watch;
#[cfg(test)]
mod lifecycle_tests;
mod media;
pub(super) mod native_restore;
mod open_api;
pub(super) mod physical_snapshot;
mod pictures;
pub(super) mod profile_handover;
mod recovery;
pub(super) mod startup_ownership;
mod sync_service;
mod system_controls;
pub(super) mod tag_edits;
mod timeout_stop;
use crate::domain::media::{ResolvedMedia, SourceMatches};
use crate::platforms::matching::MatchingService;

pub(super) struct RuntimeParts {
    pub commands: mpsc::SyncSender<RuntimeMessage>,
    pub snapshot: Arc<Mutex<BackendSnapshot>>,
    pub events: Arc<Mutex<EventJournal>>,
    pub shutdown: Arc<AtomicBool>,
    pub handover_pending: Arc<AtomicBool>,
    pub lifecycle: startup_ownership::ActorLifecycle,
    pub shutdown_outcome: Arc<Mutex<Option<Result<(), BackendError>>>>,
}
enum Completion {
    BackupExportWake,
    PhysicalSnapshotWake,
    DeepLink {
        operation: String,
        event: deeplink::DeepLinkCompletion,
    },
    BackupPreviewPrepared {
        operation: String,
        result: Result<crate::repository::LxBackupPreview>,
    },
    NativeBackupPreviewPrepared {
        operation: String,
        result: Box<Result<crate::repository::NativeBackupPreview>>,
    },
    DownloadRecoveryVerified {
        operation: String,
        task_id: String,
        expected_task_revision: u64,
        operation_epoch: u64,
        result: Result<Box<downloads::RecoveredDownloadOutput>>,
    },
    DownloadPostprocessPrepared {
        task_id: String,
        operation_epoch: u64,
        result: Result<Box<downloads::PreparedDownloadOutput>>,
    },
    DownloadAudioCommitted {
        task_id: String,
        operation_epoch: u64,
        original: crate::download::TransferCheckpoint,
        stages: crate::download::task_model::DownloadStageRecords,
        original_audio: Option<Box<crate::download::VerifiedOwnedAudio>>,
        result: Box<
            Result<(
                crate::download::CommittedAudio,
                Option<crate::download::CommittedSidecar>,
            )>,
        >,
    },
    DownloadCleanupFinished {
        task_id: String,
        operation: Option<String>,
        work: Option<Box<download_cleanup::CleanupWork>>,
        result: Result<()>,
    },
    DownloadCleanupRetired {
        task_id: String,
        operation: Option<String>,
    },
    DownloadTransferPrepared {
        task_id: String,
        operation_epoch: u64,
        result: Result<Box<crate::download::TransferJob>>,
    },
    DownloadResolved {
        task_id: String,
        operation_epoch: u64,
        source_epoch: u64,
        source_id: String,
        policy_epoch: u64,
        worker: SourceWorker,
        result: Result<(String, crate::download::planning::DownloadQuality)>,
    },
    DownloadDestination {
        operation: String,
        result: Result<crate::download::task_model::StoredDestinationGrant>,
    },
    Picture {
        operation: String,
        result: Result<Box<crate::pictures::service::PreparedPicture>>,
    },
    SourceEvents {
        generation: u64,
        source_id: String,
        events: Vec<crate::source::SourceHostEvent>,
    },
    Provider {
        operation: String,
        result: Arc<ProviderOperationResult>,
        json_bytes: usize,
    },
    Initialized(u64, String, Result<SourceWorker>),
    Resolved {
        generation: u64,
        source_epoch: u64,
        preload_operation: Option<String>,
        worker: SourceWorker,
        result: Result<(String, CacheIdentity)>,
        original: TrackRecord,
        cache_entry: Option<Box<ResolvedMedia>>,
        matches: Option<Box<SourceMatches>>,
        cache_hit: bool,
        attempted_platforms: Vec<String>,
    },
    Matched {
        operation: String,
        list_id: String,
        original: TrackRecord,
        cache_epoch: u64,
        result: Result<SourceMatches>,
    },
    SourcePicture {
        operation: String,
        source_epoch: u64,
        worker: SourceWorker,
        result: Result<String>,
    },
    CacheCleared(String, Result<CacheStats>),
    Imported(String, Result<ImportedSource>),
    LocalPrepared {
        operation: String,
        result: Result<Vec<(PreparedLocalImport, TrackRecord, LocalAsset)>>,
        ack: mpsc::SyncSender<()>,
    },
    Lyrics {
        epoch: u64,
        source_generation: u64,
        source_epoch: u64,
        result: Result<PreparedLyrics>,
        worker: Option<SourceWorker>,
    },
}
struct PendingLyrics {
    track_id: String,
    audio_generation: u64,
    epoch: u64,
    cancel: Arc<AtomicBool>,
    operation: Option<String>,
    retries: u8,
}
struct PendingPlayback {
    url: String,
    identity: CacheIdentity,
    generation: u64,
    original: TrackRecord,
}
struct SourceFlight {
    download: bool,
    source_epoch: u64,
    cancel: Arc<AtomicBool>,
    preload_operation: Option<String>,
}
struct SourceResolveJob {
    generation: u64,
    record: TrackRecord,
    strict_quality: Option<String>,
    preload: Option<ResolveAction>,
    force_refresh: bool,
    resolved_track: Option<TrackRecord>,
    skip_primary: bool,
    excluded_platforms: Vec<String>,
}
struct PendingPromotion {
    cancel: crate::playback::preload::CancelFlag,
    operation: String,
    entry: QueueEntry,
    record: TrackRecord,
    expected_generation: u64,
    nonce: u64,
    source_epoch: u64,
    identity: CacheIdentity,
    started: Instant,
}
pub(super) struct Runtime {
    effects: effects_service::EffectsRuntime,
    instance_relay: instance_relay::InstanceRelayRuntime,
    system_control: system_controls::SystemControlRuntime,
    timeout_stop: crate::playback::timeout_stop::TimeoutStop,
    sync_service: sync_service::SyncServiceRuntime,
    backup_exports: backup_export::BackupExportRuntime,
    physical_snapshots: physical_snapshot::PhysicalSnapshotRuntime,
    backup_preview: Option<(String, Arc<crate::repository::LxBackupPreview>)>,
    native_restore: native_restore::NativeRestoreRuntime,
    deep_links: deeplink::DeepLinkRuntime,
    downloads: downloads::DownloadRuntime,
    download_reads: download_reads::DownloadReadRuntime,
    download_cleanup: download_cleanup::CleanupRuntime,
    external_control: open_api::ExternalControlRuntime,
    library: library::LibraryRuntime,
    pictures: pictures::PictureRuntime,
    catalog: Catalog,
    audio: AudioPlayer,
    store: ProjectStore,
    repository: Arc<Repository>,
    tag_edits: tag_edits::TagEditRuntime,
    data: StoreData,
    shared: Arc<Mutex<BackendSnapshot>>,
    events: Arc<Mutex<EventJournal>>,
    runtime: Option<SourceWorker>,
    source_caps: Option<crate::source::RuntimeManifest>,
    cancellation: Option<WorkerCancel>,
    generation: u64,
    source_epoch: u64,
    approval_epoch: u64,
    source_approval_epoch: u64,
    source_inflight: Option<SourceFlight>,
    source_initializing: bool,
    init_operation: Option<String>,
    pending_foreground: Option<SourceResolveJob>,
    pending_pictures: VecDeque<PictureSourceRequest>,
    pending_picture_bytes: usize,
    preload: PreloadService<crate::audio::AudioPreloadToken>,
    preload_prepare: Option<(PrepareAction, crate::audio::AudioPrepareTicket)>,
    preload_identity: Option<CacheIdentity>,
    preload_media: Option<ResolvedMedia>,
    preload_accept_operation: Option<String>,
    last_preload_signature: String,
    preload_suppressed: Option<(u64, u64, u64, String)>,
    pending_promotion: Option<PendingPromotion>,
    preload_failure_snapshot: Option<BackendPreloadSnapshot>,
    last_seek: Instant,
    busy: bool,
    play_when_ready: bool,
    statuses: HashMap<String, String>,
    pending: Option<PendingPlayback>,
    completed_tx: mpsc::SyncSender<Completion>,
    completed_rx: mpsc::Receiver<Completion>,
    previous_audio_status: PlaybackStatus,
    previous_audio_generation: u64,
    revision: u64,
    catalog_revision: u64,
    last_signature: String,
    tasks: Vec<thread::JoinHandle<()>>,
    provider_tasks: Vec<thread::JoinHandle<()>>,
    provider_registry: Arc<ProviderRegistry>,
    matching_service: MatchingService,
    provider_timeout: Duration,
    provider_jobs: HashMap<String, ProviderJob>,
    provider_results: VecDeque<(Arc<ProviderOperationResult>, usize)>,
    provider_result_bytes: usize,
    import_cancels: HashMap<String, WorkerCancel>,
    file_cancels: HashMap<String, Arc<AtomicBool>>,
    cache_jobs: HashSet<String>,
    cancelled: HashSet<String>,
    request_id: String,
    origin: String,
    operation_origins: HashMap<String, String>,
    active_operation: Option<String>,
    async_started: bool,
    receipts: VecDeque<(String, String, Result<CommandReceipt, BackendError>)>,
    lyric_service: LyricService,
    lyric_pending: Option<PendingLyrics>,
    pending_lyric: Option<(String, u64, Option<String>, u8)>,
    lyric_epoch: u64,
    lyric_key: Option<(String, u64)>,
    lyric_timeline: Option<LyricTimeline>,
    lyric_offset: i64,
    intended_track: Option<String>,
    lyric_start_after_generation: u64,
    queue: QueueState,
    current_record: Option<Arc<TrackRecord>>,
    eligibility: HashMap<(String, String), QueueEligibility>,
    local_ids: HashSet<String>,
    dislike: DislikeRules,
    preferred_quality: String,
    auto_skip_on_error: bool,
    auto_switch_source: bool,
    media_refresh_attempts: u8,
    media_rate_retries: u8,
    media_retry_pending: Option<recovery::MediaRetry>,
    media_attempted_sources: HashSet<String>,
    playback_requested_quality: Option<String>,
    approved_request: Option<ApprovedAudioRequest>,
    active_media: Option<ResolvedMedia>,
    media_cache_epoch: u64,
    restored_position_ms: u64,
    restore_pending: bool,
    seek_after_load: Option<(String, u64, u64)>,
    failed_entries: HashSet<String>,
    failure_attempts: usize,
    skip_pending: Option<(Instant, u64)>,
    last_queue_signature: String,
}
struct QueueEligibility {
    name: String,
    singer: String,
    available: bool,
    source: String,
    qualities: Vec<String>,
}
struct ProviderJob {
    platform: Platform,
    operation: String,
    cancel: Arc<AtomicBool>,
}
const PROVIDER_RESULT_CAPACITY: usize = 32;
const PROVIDER_RESULT_BYTES: usize = 64 * 1024 * 1024;
const PROVIDER_JOB_CAPACITY: usize = 4;
const MAX_FAILURE_CHAIN: usize = 256;
const ERROR_SKIP_DELAY: Duration = Duration::from_secs(5);

pub(super) fn start(config: BackendConfig) -> Result<RuntimeParts> {
    let guard =
        crate::repository::ProfileGuard::acquire(&config.data_dir).map_err(anyhow::Error::new)?;
    start_with_guard(config, guard, false)
}
pub(super) fn start_with_guard(
    mut config: BackendConfig,
    guard: crate::repository::ProfileGuard,
    relay_enabled: bool,
) -> Result<RuntimeParts> {
    #[cfg(windows)]
    let mut relay_endpoint = None;
    let prepared = (|| -> Result<(startup_ownership::StartupAdmission, Arc<ProviderRegistry>)> {
        let startup_owner = startup_ownership::StartupAdmission::reserve()?;
        guard.verify()?;
        let requested = std::fs::canonicalize(&config.data_dir)?;
        ensure!(
            crate::domain::music::normalize_local_path(&requested)
                == crate::domain::music::normalize_local_path(guard.directory()),
            "Guard belongs to another backend profile"
        );
        config.data_dir = guard.directory().to_path_buf();
        #[cfg(windows)]
        {
            relay_endpoint = if relay_enabled {
                Some(
                    crate::system_control::instance_relay::RelayEndpoint::from_active_guard(
                        &guard,
                    )?,
                )
            } else {
                None
            };
        }
        #[cfg(not(windows))]
        ensure!(
            !relay_enabled,
            "Native instance relay is unavailable on this platform"
        );
        ensure!(
            config.provider_timeout <= Duration::from_secs(120),
            "Provider deadline must not exceed two minutes"
        );
        let provider_registry = match config.provider_registry.clone() {
            Some(registry) => registry,
            None => Arc::new(ProviderRegistry::new(Arc::new(ReqwestTransport::new(
                TransportConfig::default(),
            )?))),
        };
        Ok((startup_owner, provider_registry))
    })();
    let (mut startup_owner, provider_registry) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(anyhow::Error::new(
                instance_relay::RetainedProfileGuardStartupFailure::preserve(error, guard),
            ));
        }
    };
    let repository = Arc::new(Repository::open_with_guard(guard).map_err(|failure| {
        anyhow::Error::new(instance_relay::RetainedRepositoryOpenFailure::preserve(
            failure,
        ))
    })?);
    let initialization_keeper = repository.clone();
    let initialized = (move || -> Result<RuntimeParts> {
        startup_owner.retain_repository(repository.clone(), None)?;
        let store = ProjectStore::open(config.data_dir.clone())?;
        let (mut data, load_error) = match store.load() {
            Ok(data) => (data, None),
            Err(error) => (
                StoreData::default(),
                Some(format!("无法恢复保存的数据：{error:#}")),
            ),
        };
        if load_error.is_none() {
            backups::recover_backup_preferences(&repository, &store, &mut data)
                .context("Cannot finish the pending backup preferences")?;
            native_restore::recover_native_backup_preferences(&repository, &store, &mut data)
                .context("Cannot finish the pending native backup preferences")?;
        } else {
            ensure!(
                repository.pending_lx_backup_preferences()?.is_empty()
                    && repository.pending_native_backup_preferences()?.is_empty(),
                "Repair the saved player state before recovering pending backup preferences"
            );
        }
        if load_error.is_none() {
            repository.record_profile_change(&format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&data)?)
            ))?;
        }
        if load_error.is_none()
            && repository
                .import_marker("project-store-v1-local-files")?
                .is_none()
        {
            let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&data)?));
            repository.import_legacy_local_files(&data.local_files, &digest)?;
        }
        let authority = crate::library::authority::AuthorityRegistry::default();
        startup_owner
            .retain_repository(repository.clone(), Some(authority.retirement_barrier()))?;
        let catalog = Catalog::new()?;
        for track in catalog.tracks() {
            if repository.track(&track.id)?.is_none() {
                repository.upsert_track(&TrackRecord {
                    id: track.id.clone(),
                    lx_id: track.id.clone(),
                    title: track.title.clone(),
                    artists: vec![track.artist.clone()],
                    album: track.album.clone(),
                    duration_ms: Some((track.duration_seconds * 1000.0) as u64),
                    platform: PlatformTrackRef {
                        source: "local".into(),
                        song_id: track.id.into(),
                        qualities: Vec::new(),
                    },
                    local_asset_id: None,
                    metadata: serde_json::Map::new(),
                    wire: None,
                })?;
            }
        }
        let initial_revision = repository.revision()?;
        let mut local_errors = Vec::new();
        let mut asset_cursor = None;
        loop {
            let page = repository.library_assets_page(None, asset_cursor.as_deref(), 1000, None)?;
            for view in page.items {
                if !view.authorized || !view.asset.available {
                    continue;
                }
                let restored = (|| -> Result<()> {
                    let authorization =
                        library::file_authorization(&repository, &authority, &view.asset.id)?;
                    let record = repository
                        .track(&view.asset.track_id)?
                        .context("Local track metadata is unavailable")?;
                    catalog.restore_local_record(&record, &view.asset, authorization)?;
                    Ok(())
                })();
                if let Err(error) = restored
                    && local_errors.len() < 256
                {
                    local_errors.push(format!("{}：{error:#}", view.asset.path.display()));
                }
            }
            asset_cursor = page.next_asset_id;
            if asset_cursor.is_none() {
                break;
            }
        }
        let shared = Arc::new(Mutex::new(BackendSnapshot::default()));
        let events = Arc::new(Mutex::new(EventJournal::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopping = shutdown.clone();
        let handover_pending = Arc::new(AtomicBool::new(false));
        let actor_handover_pending = handover_pending.clone();
        let shutdown_outcome = Arc::new(Mutex::new(None));
        startup_owner.attach_shutdown_outcome(shutdown_outcome.clone());
        let termination = shutdown_outcome.clone();
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (completed_tx, completed_rx) = mpsc::sync_channel(8);
        let mut transport_options = TransportOptions::default();
        transport_options.cache.root = config.data_dir.join("audio-cache");
        let physical_snapshots = physical_snapshot::PhysicalSnapshotRuntime::restore(&repository)?;
        let tag_edits = tag_edits::TagEditRuntime::restore(&repository)?;
        let pictures = pictures::PictureRuntime::new(
            repository.clone(),
            provider_registry.clone(),
            authority.clone(),
        )?;
        let audio = AudioPlayer::with_transport_options(
            match config.audio_mode {
                BackendAudioMode::Device => AudioOutputMode::Device,
                BackendAudioMode::Memory => AudioOutputMode::Memory,
            },
            transport_options,
        );
        let lyric_service = LyricService::with_authority(repository.clone(), authority.clone());
        let initial_core = Runtime {
            effects: effects_service::EffectsRuntime::new(config.data_dir.clone()),
            instance_relay: instance_relay::InstanceRelayRuntime::new(
                commands.clone(),
                shutdown.clone(),
            ),
            system_control: system_controls::SystemControlRuntime::new(
                commands.clone(),
                shutdown.clone(),
            ),
            timeout_stop: Default::default(),
            sync_service: sync_service::SyncServiceRuntime::default(),
            backup_exports: backup_export::BackupExportRuntime::default(),
            physical_snapshots,
            backup_preview: None,
            native_restore: Default::default(),
            deep_links: deeplink::DeepLinkRuntime::default(),
            downloads: downloads::DownloadRuntime::default(),
            download_reads: download_reads::DownloadReadRuntime::default(),
            download_cleanup: download_cleanup::CleanupRuntime::default(),
            external_control: open_api::ExternalControlRuntime::default(),
            library: library::LibraryRuntime::new(authority),
            pictures,
            catalog,
            audio,
            store,
            repository,
            tag_edits,
            data,
            shared: shared.clone(),
            events: events.clone(),
            runtime: None,
            source_caps: None,
            cancellation: None,
            generation: 0,
            source_epoch: 0,
            approval_epoch: 0,
            source_approval_epoch: 0,
            source_inflight: None,
            source_initializing: false,
            init_operation: None,
            pending_foreground: None,
            pending_pictures: VecDeque::new(),
            pending_picture_bytes: 0,
            preload: PreloadService::new(),
            preload_prepare: None,
            preload_identity: None,
            preload_media: None,
            preload_accept_operation: None,
            last_preload_signature: String::new(),
            preload_suppressed: None,
            pending_promotion: None,
            preload_failure_snapshot: None,
            last_seek: Instant::now(),
            busy: false,
            play_when_ready: true,
            statuses: HashMap::new(),
            pending: None,
            completed_tx,
            completed_rx,
            previous_audio_status: PlaybackStatus::Idle,
            previous_audio_generation: 0,
            revision: initial_revision,
            catalog_revision: u64::MAX,
            last_signature: String::new(),
            tasks: Vec::new(),
            provider_tasks: Vec::new(),
            matching_service: MatchingService::new(provider_registry.clone()),
            provider_registry,
            provider_timeout: config.provider_timeout,
            provider_jobs: HashMap::new(),
            provider_results: VecDeque::new(),
            provider_result_bytes: 0,
            import_cancels: HashMap::new(),
            file_cancels: HashMap::new(),
            cache_jobs: HashSet::new(),
            cancelled: HashSet::new(),
            request_id: "startup".into(),
            origin: "backend".into(),
            operation_origins: HashMap::new(),
            active_operation: None,
            async_started: false,
            receipts: VecDeque::new(),
            lyric_service,
            lyric_pending: None,
            pending_lyric: None,
            lyric_epoch: 0,
            lyric_key: None,
            lyric_timeline: None,
            lyric_offset: 0,
            intended_track: None,
            lyric_start_after_generation: 0,
            queue: QueueState::default(),
            current_record: None,
            eligibility: HashMap::new(),
            local_ids: HashSet::new(),
            dislike: DislikeRules::default(),
            preferred_quality: "128k".into(),
            auto_skip_on_error: true,
            auto_switch_source: false,
            media_refresh_attempts: 0,
            media_rate_retries: 0,
            media_retry_pending: None,
            media_attempted_sources: HashSet::new(),
            playback_requested_quality: None,
            approved_request: None,
            active_media: None,
            media_cache_epoch: 0,
            restored_position_ms: 0,
            restore_pending: false,
            seek_after_load: None,
            failed_entries: HashSet::new(),
            failure_attempts: 0,
            skip_pending: None,
            last_queue_signature: String::new(),
        };
        startup_owner.attach(initial_core);
        let mut startup_core = startup_owner.core();
        let core = startup_core
            .as_mut()
            .expect("startup owner retains constructed Runtime");
        #[cfg(test)]
        lifecycle_tests::fail_attached_startup_if_requested(
            core,
            &config.data_dir,
            startup_owner.lifecycle(),
        )?;
        core.dislike = core.repository.dislike_rules()?;
        if let Some(checkpoint) = core.repository.playback_checkpoint()? {
            core.queue = checkpoint.queue;
            core.preferred_quality = checkpoint.quality;
            core.auto_skip_on_error = checkpoint.auto_skip_on_error;
            core.auto_switch_source = checkpoint.auto_switch_source;
            core.lyric_offset = checkpoint.lyric_offset_ms;
            core.restored_position_ms = checkpoint.position_ms;
            if let Err(error) = core.reconcile_queue() {
                core.queue = QueueState::default();
                core.queue.set_list(
                    "catalog".into(),
                    core.catalog
                        .tracks()
                        .iter()
                        .map(|track| track.id.clone())
                        .collect(),
                );
                core.error(format!("Playback restore failed: {error:#}"));
            }
            core.current_record = core
                .queue
                .current
                .as_ref()
                .and_then(|entry| core.queue_record(entry).ok())
                .map(Arc::new);
            core.restore_pending = core.current_record.is_some();
            if core.current_record.is_none() {
                core.queue.clear_current();
                core.restored_position_ms = 0;
            }
        } else {
            core.queue.set_list(
                "catalog".into(),
                core.catalog
                    .tracks()
                    .iter()
                    .map(|track| track.id.clone())
                    .collect(),
            );
            core.reconcile_queue()?;
        }
        core.audio.set_volume(core.data.volume);
        core.audio
            .set_output_preferences(core.data.audio_preferences.clone())?;
        core.audio.set_equalizer(core.data.equalizer)?;
        core.audio.set_processing(core.data.playback_processing)?;
        core.audio
            .set_analysis_enabled(core.data.audio_analysis_enabled)?;
        if let Err(error) = core.initialize_effects() {
            core.effects.resource_error = Some(format!("{error:#}"));
            core.error(format!("Effect resource restore failed: {error:#}"));
        }
        core.begin_library_verification()?;
        if let Some(error) = load_error {
            core.error(error);
        }
        if !local_errors.is_empty() {
            core.error(format!("部分本地曲目无法恢复：{}", local_errors.join("；")));
        }
        core.refresh();
        #[cfg(windows)]
        core.instance_relay.prepare(relay_endpoint);
        core.start_instance_relay()?;
        drop(startup_core);
        let actor_startup = startup_owner.actor_ticket();
        let lifecycle = startup_owner.lifecycle();
        startup_owner.begin_actor_spawn();
        let worker = thread::Builder::new()
        .name("lx-backend".into())
        .spawn(move || {
            let mut core=actor_startup.take();
            let mut profile_retirement=None;
            let actor_result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Err(error)=core.start_system_controls_from_saved() {core.error(format!("System controls unavailable: {error:#}"));}
            while !stopping.load(Ordering::Acquire) || actor_handover_pending.load(Ordering::Acquire) {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(RuntimeMessage::ProfileQuiesce {request,slot,admission})=>{
                        match core.begin_profile_quiesce(&request,&slot) {
                            Ok(seed)=>{
                                actor_handover_pending.store(false,Ordering::Release);
                                profile_retirement=Some((seed,slot.clone()));
                                *termination.lock().unwrap_or_else(|e|e.into_inner())=Some(Ok(()));
                                return;
                            }
                            Err(error)=>{actor_handover_pending.store(false,Ordering::Release);let _=slot.reject_before_admission(&request.scope,backend_error(error));admission.reopen_if_running();}
                        }
                    }
                    #[cfg(test)]
                    Ok(RuntimeMessage::TestActorPanic)=>panic!("isolated actor failure fixture"),
                    #[cfg(test)]
                    Ok(RuntimeMessage::TestGatedTask{entered,release})=>{core.tasks.push(thread::spawn(move ||{let _=entered.send(());let _=release.recv();}));}
                    #[cfg(test)]
                    Ok(RuntimeMessage::TestGatedPanicTask{entered,release})=>{core.tasks.push(thread::spawn(move ||{let _=entered.send(());let _=release.recv();panic!("isolated handover worker panic");}));}
                    Ok(RuntimeMessage::OpenSyncSession {cancel,reply})=>{
                        let result=if cancel.load(Ordering::Acquire){Err(BackendError::new(ErrorKind::Cancelled,"Sync session opening cancelled"))}else{core.open_sync_session()};
                        if let Err(result)=reply.send(result) && let Ok(gate)=result.0 {gate.begin_close();}
                    }
                    Ok(RuntimeMessage::SyncRequest {gate,cancel,request,reply})=>{let result=core.sync_request(gate,cancel,request);let _=reply.send(result);}
                    Ok(RuntimeMessage::SyncManagement {request,cancel,reply})=>{let result=core.sync_management(request,cancel);let _=reply.send(result);}
                    Ok(RuntimeMessage::OpenExternalSession { cancel, reply }) => {
                        let result = if cancel.load(Ordering::Acquire) {
                            Err(BackendError::new(
                                ErrorKind::Cancelled,
                                "External session request expired",
                            ))
                        } else {
                            core.open_external_session()
                        };
                        if let Err(result) = reply.send(result)
                            && let Ok(gate) = result.0
                        {
                            gate.retire();
                        }
                    }
                    Ok(RuntimeMessage::ExternalSnapshot { gate, reply }) => {
                        let _ = reply.send(core.external_snapshot(&gate));
                    }
                    Ok(RuntimeMessage::ExternalUpdates {
                        gate,
                        cursor,
                        reply,
                    }) => {
                        let _ = reply.send(core.external_updates(&gate, cursor));
                    }
                    Ok(RuntimeMessage::ExternalArtwork { gate, reference, reply }) => {
                        let _ = reply.send(core.external_artwork(&gate, reference));
                    }
                    Ok(RuntimeMessage::ExternalCommand {
                        gate,
                        action,
                        request_id,
                        reply,
                    }) => {
                        let _ = reply.send(core.external_command(&gate, action, request_id));
                    }
                    Ok(RuntimeMessage::SystemInput { input,fence,cancel,reply })=>{
                        let _=reply.send(core.system_input(input,fence,&cancel));
                    }
                    Ok(RuntimeMessage::RelayInput { request,fence,reply })=>{
                        let result=core.relay_input(request,fence);let _=reply.send(result);
                    }
                    Ok(RuntimeMessage::Command { envelope, reply }) => {
                        let result = core.execute(*envelope);
                        let _ = reply.send(result);
                    }
                    Ok(RuntimeMessage::Wake) => {}
                    Ok(RuntimeMessage::Query { query, reply }) => {
                        let _ = reply.send(core.query(query).map_err(backend_error));
                    }
                    Ok(RuntimeMessage::QueryProviderResult {
                        operation_id,
                        reply,
                    }) => {
                        let result = core
                            .provider_results
                            .iter()
                            .find(|(result, _)| result.operation_id == operation_id)
                            .map(|(result, _)| result.clone())
                            .ok_or_else(|| {
                                BackendError::new(
                                    ErrorKind::Unavailable,
                                    "Provider operation result is unknown or evicted",
                                )
                            });
                        let _ = reply.send(result);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Ok(RuntimeMessage::QueryLyrics {
                        track_id,
                        origin,
                        reply,
                    }) => {
                        let result = (|| -> Result<Option<LyricBundle>> {
                            if let Some(origin) = origin {
                                return core.repository.lyrics(&track_id, origin);
                            }
                            let edited = core.repository.lyrics(&track_id, LyricOrigin::Edited)?;
                            if edited
                                .as_ref()
                                .is_some_and(|bundle| bundle.lyric.text().is_some())
                            {
                                Ok(edited)
                            } else {
                                core.repository.lyrics(&track_id, LyricOrigin::Raw)
                            }
                        })()
                        .map_err(lyric_backend_error);
                        let _ = reply.send(result);
                    }
                    Ok(RuntimeMessage::ExportLyrics {
                        track_id,
                        format,
                        reply,
                    }) => {
                        let _ = reply.send(
                            core.lyric_service
                                .export_cached(&track_id, format)
                                .map_err(lyric_backend_error),
                        );
                    }
                    Ok(RuntimeMessage::SnapshotWithCursor { reply }) => {
                        core.refresh();
                        let snapshot = core.shared.lock().unwrap().clone();
                        let cursor = core.events.lock().unwrap().next_id;
                        let _ = reply.send(Ok((snapshot, cursor)));
                    }
                }
                while let Ok(completion) = core.completed_rx.try_recv() {
                    core.dispatch_completion(completion);
                }
                core.poll_library();
                core.poll_library_watch();
                core.poll_downloads();
                core.poll_cleanup();
                core.poll_backup_exports();
                core.poll_physical_snapshots();
                core.poll_sync_service();
                core.poll_system_controls();
                core.poll_tag_edits();
                core.poll_effects();
                core.reap_tasks();
                if let Err(error) = core.start_pending_foreground() {
                    core.busy = false;
                    core.error(format!("{error:#}"));
                    if !core.system_control.owns_operation(core.active_operation.as_deref().unwrap_or("")) && !core.tag_edits.owns_operation(core.active_operation.as_deref().unwrap_or("")) && let Some(operation) = core.active_operation.take() {
                        core.publish_finished(operation, &Err(error));
                    }
                }
                core.refresh();
            }
            core.external_control.retire();
            core.begin_instance_relay_shutdown();
            core.begin_tag_shutdown();
            let sync_shutdown=core.retire_sync_service();
            core.deep_links.begin_shutdown();
            core.begin_effect_shutdown();
            if let Err(error)=core.begin_system_control_shutdown(){core.error(format!("System shutdown preparation failed: {error:#}"));}
            core.begin_download_shutdown();
            core.begin_cleanup_shutdown();
            if let Err(error) = core.save_checkpoint() {
                core.error(format!("Playback checkpoint failed: {error:#}"));
            }
            core.cancel_runtime();
            if std::env::var_os("LX_DEBUG").is_some() { eprintln!("Backend shutdown: draining downloads"); }
            let download_shutdown = core.finish_download_shutdown();
            if std::env::var_os("LX_DEBUG").is_some() { eprintln!("Backend shutdown: download drain returned {download_shutdown:?}"); }
            let provider_operations = core.provider_jobs.keys().cloned().collect::<Vec<_>>();
            for operation in provider_operations {
                core.cancel_provider(&operation);
            }
            for (operation, cancel) in std::mem::take(&mut core.import_cancels) {
                cancel.terminate();
                core.publish_finished(
                    operation,
                    &Err(BackendError::new(ErrorKind::Cancelled, "Backend shutting down").into()),
                );
            }
            for (operation, cancel) in std::mem::take(&mut core.file_cancels) {
                cancel.store(true, Ordering::Release);
                core.publish_finished(
                    operation,
                    &Err(BackendError::new(ErrorKind::Cancelled, "Backend shutting down").into()),
                );
            }
            core.cancel_all_picture_requests();
            core.backup_exports.begin_shutdown();
            core.physical_snapshots.begin_shutdown();
            core.pictures.begin_shutdown();
            core.shutdown_library_operations();
            let _ = core.library.shutdown(Duration::ZERO);
            let audio_shutdown = core.audio.shutdown(Duration::from_secs(3));
            let mut shutdown_task_panicked = false;
            let mut shutdown_diagnostic_at = Instant::now();
            // The public handle's deadline bounds its wait. The actor must retain ownership
            // of cancelled tasks until they actually exit, including blocked file I/O.
            loop {
                while let Ok(completion) = core.completed_rx.try_recv() {
                    if let Completion::BackupExportWake = completion {
                        core.poll_backup_exports();
                    } else if let Completion::PhysicalSnapshotWake = completion {
                        core.poll_physical_snapshots();
                    } else if let Completion::DownloadCleanupFinished {
                        task_id,
                        operation,
                        work,
                        result,
                    } = completion
                    {
                        core.finish_cleanup(task_id, operation, work, result);
                    } else if let Completion::DownloadCleanupRetired { task_id, operation } =
                        completion
                    {
                        core.finish_cleanup_retired(task_id, operation);
                    } else {
                        drop(completion);
                    }
                }
                core.poll_backup_exports();
                core.poll_physical_snapshots();
                shutdown_task_panicked |= core.reap_shutdown_tasks();
                let retiring_complete = core.retry_cleanup_retirement();
                let snapshot_retiring_complete = core.retire_physical_snapshot_owners().unwrap_or(false);
                let system_retiring_complete=core.poll_system_control_quiesce_step().unwrap_or(false);
                let relay_retiring_complete=core.poll_instance_relay_quiesce_step().unwrap_or(false);
                let tags_retiring_complete=core.poll_tag_quiesce_step().unwrap_or(false);
                let effects_retiring_complete=core.poll_effect_quiesce().unwrap_or(false);
                let download_reads_retiring_complete=core.poll_download_read_shutdown_step();
                let catalog_retiring_complete=core.catalog.shutdown(Duration::ZERO).unwrap_or(false);
                let running = core
                    .tasks
                    .iter()
                    .chain(core.provider_tasks.iter())
                    .any(|task| !task.is_finished());
                if std::env::var_os("LX_DEBUG").is_some() && shutdown_diagnostic_at.elapsed() >= Duration::from_secs(1) {
                    eprintln!("Backend shutdown pending: workers={running}, cleanup={retiring_complete}, snapshots={snapshot_retiring_complete}, system={system_retiring_complete}, relay={relay_retiring_complete}, tags={tags_retiring_complete}, effects={effects_retiring_complete}, reads={download_reads_retiring_complete}, catalog={catalog_retiring_complete}");
                    shutdown_diagnostic_at = Instant::now();
                }
                if !running && retiring_complete && snapshot_retiring_complete && system_retiring_complete && relay_retiring_complete && tags_retiring_complete && effects_retiring_complete && download_reads_retiring_complete && catalog_retiring_complete {
                    core.poll_backup_exports();
                    core.poll_physical_snapshots();
                    match core.completed_rx.try_recv() {
                        Ok(Completion::BackupExportWake) => core.poll_backup_exports(),
                        Ok(Completion::PhysicalSnapshotWake) => core.poll_physical_snapshots(),
                        Ok(Completion::DownloadCleanupFinished {
                            task_id,
                            operation,
                            work,
                            result,
                        }) => core.finish_cleanup(task_id, operation, work, result),
                        Ok(Completion::DownloadCleanupRetired { task_id, operation }) => {
                            core.finish_cleanup_retired(task_id, operation)
                        }
                        Ok(completion) => drop(completion),
                        Err(_) => break,
                    }
                    continue;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let mut task_error = download_shutdown.err().map(|error| {
                BackendError::new(
                    ErrorKind::OperationFailed,
                    format!("Download shutdown failed: {error:#}"),
                )
            });
            if shutdown_task_panicked {
                task_error = Some(BackendError::new(ErrorKind::OperationFailed,
                    "A backend task panicked during shutdown"));
            }
            if core.repository.pending_tag_edits().is_ok_and(|pending|!pending.is_empty()) {task_error=Some(BackendError::new(ErrorKind::OperationFailed,"Local tag recovery remains pending"));}
            if core.backup_exports.retained_count() > 0 {
                task_error=Some(BackendError::new(ErrorKind::OperationFailed,"Backup export parts remain unconfirmed; retained files require explicit recovery"));
            }
            if core.physical_snapshots.retained_count() > 0 {
                task_error = Some(BackendError::new(ErrorKind::OperationFailed,
                    "Physical snapshot recovery remains pending"));
            }
            if let Err(error)=sync_shutdown{task_error=Some(error);}
            for task in core.tasks.drain(..).chain(core.provider_tasks.drain(..)) {
                if task.join().is_err() {
                    task_error = Some(BackendError::new(
                        ErrorKind::OperationFailed,
                        "A backend task panicked during shutdown",
                    ));
                }
            }
            while !core.pictures.shutdown(Duration::from_millis(10)) {
                while let Ok(completion) = core.completed_rx.try_recv() {
                    drop(completion);
                }
            }
            let mut library_shutdown = core.library.shutdown(Duration::ZERO);
            while matches!(&library_shutdown, Ok(false)) {
                library_shutdown = core.library.shutdown(Duration::from_millis(10));
            }
            if let Err(error) = library_shutdown {
                task_error = Some(BackendError::new(
                    ErrorKind::OperationFailed,
                    format!("Library shutdown failed: {error:#}"),
                ));
            }
            let mut final_audio = audio_shutdown;
            while final_audio.as_ref().err().is_some_and(|error| {
                error
                    .downcast_ref::<crate::audio::AudioShutdownError>()
                    .is_some_and(|error| error.is_timeout())
            }) {
                final_audio = core.audio.shutdown(Duration::from_millis(20));
                if final_audio.is_err() {
                    thread::park_timeout(Duration::from_millis(2));
                }
            }
            let outcome = task_error.map(Err).unwrap_or_else(|| {
                final_audio.map_err(|error| {
                    BackendError::new(
                        ErrorKind::OperationFailed,
                        format!("Audio shutdown failed: {error}"),
                    )
                })
            });
            if outcome.is_ok() {
                let mut snapshot = core.shared.lock().unwrap();
                snapshot.stopped = true;
                snapshot.library.running_operations = 0;
                snapshot.library.revision = core.revision;
                snapshot.playback = core.audio.snapshot();
                snapshot.equalizer = core.audio.equalizer_snapshot();
                snapshot.playback_processing = core.audio.processing_snapshot();
                snapshot.effects = core.audio.effects_snapshot();
                let output = core.audio.output_snapshot();
                if snapshot.audio_output != output {
                    snapshot.audio_output = output.clone();
                    core.events.lock().unwrap().publish(
                        core.revision,
                        "audio",
                        None,
                        BackendEvent::AudioOutputChanged {
                            output: Box::new(output),
                        },
                    );
                }
                core.events.lock().unwrap().publish(
                    core.revision,
                    "backend",
                    None,
                    BackendEvent::Stopped,
                );
            }
            *termination.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
            }));
            let failure=match actor_result {
                Ok(())=>termination.lock().unwrap_or_else(|e|e.into_inner()).as_ref().and_then(|result|result.as_ref().err()).map(|error|error.message.clone()),
                Err(_)=>{
                    let error=BackendError::new(ErrorKind::OperationFailed,"Backend actor panicked; the whole runtime remains owned until actual retirement");
                    *termination.lock().unwrap_or_else(|e|e.into_inner())=Some(Err(error.clone()));
                    Some(error.message)
                }
            };
            if profile_retirement.is_none() && actor_handover_pending.swap(false,Ordering::AcqRel) {
                for message in receiver.try_iter() {
                    if let RuntimeMessage::ProfileQuiesce{request,slot,..}=message {
                        let _=slot.reject_before_admission(&request.scope,BackendError::new(ErrorKind::OperationFailed,"Actor stopped before profile handover admission"));
                    }
                }
            }
            stopping.store(true,Ordering::Release);
            actor_startup.retire(core,profile_retirement,failure);
        });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                startup_owner.actor_spawn_failed();
                return Err(error.into());
            }
        };
        startup_owner.attach_actor(worker);
        startup_owner.commit();
        Ok(RuntimeParts {
            commands,
            snapshot: shared,
            events,
            shutdown,
            handover_pending,
            lifecycle,
            shutdown_outcome,
        })
    })();
    match initialized {
        Ok(parts) => {
            drop(initialization_keeper);
            Ok(parts)
        }
        Err(error) => Err(anyhow::Error::new(
            instance_relay::RetainedRepositoryInitializationFailure::preserve(
                error,
                initialization_keeper,
            ),
        )),
    }
}
impl Runtime {
    fn mutation_origin(&self) -> &str {
        if self.origin.is_empty() {
            "local"
        } else {
            &self.origin
        }
    }
    fn queue_record(&self, entry: &QueueEntry) -> Result<TrackRecord> {
        if entry.list_id == "catalog" {
            return self
                .repository
                .track(&entry.track_id)?
                .context("Catalog track does not exist");
        }
        self.repository
            .list_track(&entry.list_id, &entry.track_id)?
            .context("Queue item is no longer in its list")
    }
    fn list_records(&self, list_id: &str) -> Result<Vec<TrackRecord>> {
        if list_id == "catalog" {
            return Ok(self
                .catalog
                .tracks()
                .into_iter()
                .map(|track| TrackRecord {
                    id: track.id.clone(),
                    lx_id: track.id.clone(),
                    title: track.title,
                    artists: if track.artists.is_empty() {
                        vec![track.artist]
                    } else {
                        track.artists
                    },
                    album: track.album,
                    duration_ms: Some((track.duration_seconds * 1000.0) as u64),
                    platform: PlatformTrackRef {
                        source: "local".into(),
                        song_id: serde_json::json!(track.id),
                        qualities: Vec::new(),
                    },
                    local_asset_id: None,
                    metadata: serde_json::Map::new(),
                    wire: None,
                })
                .collect());
        }
        let mut records = Vec::new();
        let mut snapshot = None;
        let mut offset = 0;
        let mut bytes = 0usize;
        loop {
            let page = self.repository.list_tracks_page(list_id, offset, 1000)?;
            if let Some((revision, total)) = snapshot {
                ensure!(
                    page.revision == revision && page.total == total,
                    BackendError::new(ErrorKind::Conflict, "List changed while being paged")
                );
            } else {
                snapshot = Some((page.revision, page.total));
            }
            ensure!(
                !page.items.is_empty() || offset as u64 >= page.total,
                "List paging made no progress"
            );
            for record in page.items {
                bytes += serde_json::to_vec(&record)?.len();
                ensure!(
                    bytes <= 64 * 1024 * 1024,
                    "Playback list metadata exceeds 64 MiB"
                );
                records.push(record);
                ensure!(records.len() <= 50_000, "Playback list exceeds 50000 items");
            }
            offset = records.len();
            if offset as u64 >= page.total {
                break;
            }
        }
        Ok(records)
    }
    fn reconcile_queue(&mut self) -> Result<()> {
        self.local_ids = self
            .catalog
            .tracks()
            .iter()
            .map(|track| track.id.clone())
            .collect();
        let records = self.list_records(&self.queue.origin_list)?;
        let mut eligibility = HashMap::new();
        let mut ids = Vec::new();
        for record in records {
            ids.push(record.id.clone());
            eligibility.insert(
                (self.queue.origin_list.clone(), record.id.clone()),
                self.eligibility_record(&record),
            );
        }
        let refs = self
            .queue
            .temporary
            .iter()
            .chain(self.queue.current.iter())
            .chain(self.queue.history().iter())
            .chain(self.queue.future().iter())
            .cloned()
            .collect::<Vec<_>>();
        for entry in refs {
            if let Ok(record) = self.queue_record(&entry) {
                eligibility.insert(
                    (entry.list_id.clone(), entry.track_id.clone()),
                    self.eligibility_record(&record),
                );
            }
        }
        self.queue.set_list(self.queue.origin_list.clone(), ids);
        self.queue.validate()?;
        self.eligibility = eligibility;
        self.refresh_source_eligibility();
        Ok(())
    }
    fn eligibility_record(&self, record: &TrackRecord) -> QueueEligibility {
        QueueEligibility {
            name: record.title.clone(),
            singer: record.artists.join("、"),
            available: record.platform.source != "local" || self.local_ids.contains(&record.id),
            source: record.platform.source.clone(),
            qualities: record.platform.qualities.clone(),
        }
    }
    fn refresh_source_eligibility(&mut self) {
        let approved = self
            .data
            .selected
            .as_ref()
            .and_then(|id| self.data.sources.iter().find(|source| &source.sha256 == id))
            .is_some_and(|source| {
                source.approved
                    && source.approval_policy_version
                        == crate::source::CURRENT_APPROVAL_POLICY_VERSION
            });
        for record in self.eligibility.values_mut() {
            if record.source != "local" {
                record.available = approved
                    && self.source_caps.as_ref().is_none_or(|manifest| {
                        manifest.sources.get(&record.source).is_some_and(|caps| {
                            caps.actions.iter().any(|action| action == "musicUrl")
                                && caps.qualitys.iter().any(|quality| {
                                    record.qualities.is_empty()
                                        || record.qualities.contains(quality)
                                })
                        })
                    });
            }
        }
    }
    fn eligible_with(
        records: &HashMap<(String, String), QueueEligibility>,
        dislike: &DislikeRules,
        failed: &HashSet<String>,
        entry: &QueueEntry,
    ) -> bool {
        !failed.contains(&entry.entry_id)
            && records
                .get(&(entry.list_id.clone(), entry.track_id.clone()))
                .is_some_and(|record| {
                    record.available && !dislike.matches(&record.name, &record.singer)
                })
    }
    fn playback_position(&self) -> u64 {
        if self.restore_pending {
            return self.restored_position_ms;
        }
        let audio = self.audio.snapshot();
        if self.busy
            || self.pending.is_some()
            || self.seek_after_load.is_some()
            || audio.track_id.as_deref()
                != self
                    .current_record
                    .as_ref()
                    .map(|record| record.id.as_str())
        {
            return 0;
        }
        (audio.position_seconds.max(0.0) * 1000.0).min((30u64 * 24 * 60 * 60 * 1000) as f64) as u64
    }
    fn checkpoint(&self, queue: QueueState, position_ms: u64) -> PlaybackCheckpoint {
        PlaybackCheckpoint {
            queue,
            position_ms,
            lyric_offset_ms: self.lyric_offset,
            quality: self.preferred_quality.clone(),
            auto_skip_on_error: self.auto_skip_on_error,
            auto_switch_source: self.auto_switch_source,
        }
    }
    fn save_checkpoint(&mut self) -> Result<()> {
        let checkpoint = self.checkpoint(self.queue.clone(), self.playback_position());
        let change = self.repository.save_playback_checkpoint(
            &checkpoint,
            Some(self.revision),
            self.mutation_origin(),
        )?;
        self.revision = change.revision;
        Ok(())
    }
    fn commit_queue(&mut self, candidate: QueueState) -> Result<()> {
        candidate.validate()?;
        let same = candidate.current.as_ref().map(|entry| &entry.entry_id)
            == self.queue.current.as_ref().map(|entry| &entry.entry_id);
        let checkpoint = self.checkpoint(
            candidate.clone(),
            if same { self.playback_position() } else { 0 },
        );
        let change = self.repository.save_playback_checkpoint(
            &checkpoint,
            Some(self.revision),
            self.mutation_origin(),
        )?;
        self.revision = change.revision;
        self.queue = candidate;
        Ok(())
    }
    fn queue_snapshot(&mut self, offset: usize, limit: usize) -> Result<QueueSnapshot> {
        ensure!(
            limit > 0 && limit <= 1000,
            "Queue page limit must be 1..=1000"
        );
        let next = self.queue.preview_next(AdvanceReason::Ended, |entry| {
            Self::eligible_with(
                &self.eligibility,
                &self.dislike,
                &self.failed_entries,
                entry,
            )
        });
        Ok(QueueSnapshot {
            revision: self.queue.revision,
            repository_revision: self.revision,
            mode: self.queue.mode,
            origin_list: self.queue.origin_list.clone(),
            current: self.queue.current.clone(),
            current_track: self
                .current_record
                .as_ref()
                .map(|record| TrackSummary::from(record.as_ref())),
            entries: self
                .queue
                .entries
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect(),
            total: self.queue.entries.len(),
            offset,
            limit,
            temporary: self.queue.temporary.iter().take(limit).cloned().collect(),
            temporary_total: self.queue.temporary.len(),
            next,
            restore_pending: self.restore_pending,
            position_ms: self.playback_position(),
            quality: self.preferred_quality.clone(),
            auto_skip_on_error: self.auto_skip_on_error,
            auto_switch_source: self.auto_switch_source,
            skip_pending: self.skip_pending.is_some(),
        })
    }
    fn query(&mut self, query: BackendQuery) -> Result<BackendQueryResult> {
        self.poll_timeout_stop();
        Ok(match query {
            BackendQuery::LocalTags { operation_id } => {
                BackendQueryResult::LocalTags(self.tag_edits.read_view(&operation_id))
            }
            BackendQuery::TagWriteApproval { approval_id } => BackendQueryResult::TagWriteApproval(
                self.repository.tag_write_approval(&approval_id)?,
            ),
            BackendQuery::TagEdit { operation_id } => {
                BackendQueryResult::TagEdit(self.tag_view(&operation_id)?.map(Box::new))
            }
            BackendQuery::TagEdits => BackendQueryResult::TagEdits(self.tag_views()?),
            BackendQuery::SystemControlSettings => {
                BackendQueryResult::SystemControlSettings(self.system_control_settings())
            }
            BackendQuery::SystemControlState => {
                BackendQueryResult::SystemControlState(self.system_control_state())
            }
            BackendQuery::TimeoutStopPreferences => {
                BackendQueryResult::TimeoutStopPreferences(self.data.timeout_stop)
            }
            BackendQuery::TimeoutStop => {
                BackendQueryResult::TimeoutStop(self.timeout_stop.snapshot(Instant::now()))
            }
            BackendQuery::DeepLinkResult {
                operation_id,
                offset,
                limit,
            } => BackendQueryResult::DeepLinkResult(Box::new(self.deeplink_result_page(
                &operation_id,
                offset,
                limit,
            )?)),
            BackendQuery::LxBackupPreview => {
                BackendQueryResult::LxBackupPreview(self.backup_preview_view())
            }
            BackendQuery::NativeBackupPreview => {
                BackendQueryResult::NativeBackupPreview(self.native_backup_preview_view())
            }
            BackendQuery::NativeBackupImportResult { operation_id } => {
                BackendQueryResult::NativeBackupImportResult(
                    self.native_backup_result(&operation_id)?,
                )
            }
            BackendQuery::NativeBackupArchive { operation_id } => {
                BackendQueryResult::NativeBackupArchive(
                    self.repository.native_backup_archive(&operation_id)?,
                )
            }
            BackendQuery::BackupExport { operation_id } => BackendQueryResult::BackupExport(
                self.backup_export_view(&operation_id).map(Box::new),
            ),
            BackendQuery::PhysicalSnapshot { operation_id } => {
                BackendQueryResult::PhysicalSnapshot(self.physical_snapshot_view(&operation_id))
            }
            BackendQuery::PhysicalSnapshots => {
                BackendQueryResult::PhysicalSnapshots(self.physical_snapshot_views())
            }
            BackendQuery::LxBackupImportResult { operation_id } => {
                BackendQueryResult::LxBackupImportResult(
                    self.repository
                        .lx_backup_result(&operation_id)?
                        .map(|result| BackupImportView {
                            operation_id: result.operation_id,
                            input_digest: result.input_digest,
                            revision: result.change.revision,
                            replayed: result.replayed,
                        }),
                )
            }
            BackendQuery::DownloadSettings => {
                BackendQueryResult::DownloadSettings(self.repository.download_settings()?)
            }
            BackendQuery::DownloadCleanup { task_id } => {
                BackendQueryResult::DownloadCleanup(self.cleanup_view(task_id)?)
            }
            BackendQuery::DownloadDestinations => {
                BackendQueryResult::DownloadDestinations(self.repository.download_destinations()?)
            }
            BackendQuery::DownloadTasks => BackendQueryResult::DownloadTasks(
                self.repository
                    .download_tasks()?
                    .iter()
                    .map(|task| task.query_view())
                    .collect(),
            ),
            BackendQuery::DownloadTask { task_id } => BackendQueryResult::DownloadTask(
                self.repository
                    .download_task(&task_id)?
                    .map(|task| task.query_view()),
            ),
            BackendQuery::DownloadPendingUrl { task_id } => {
                BackendQueryResult::DownloadPendingUrl(self.download_pending_url(&task_id)?)
            }
            BackendQuery::LibraryWatch { root_id } => {
                BackendQueryResult::LibraryWatch(self.library_watch_view(&root_id)?)
            }
            BackendQuery::LibraryWatches { offset, limit } => {
                BackendQueryResult::LibraryWatches(self.library_watch_page(offset, limit)?)
            }
            BackendQuery::LibraryAssetBinding { asset_id } => {
                BackendQueryResult::LibraryAssetBinding(
                    self.repository.library_asset_binding(&asset_id)?,
                )
            }
            BackendQuery::LibraryRootBinding { root_id } => BackendQueryResult::LibraryRootBinding(
                self.repository.library_root_binding(&root_id)?,
            ),
            BackendQuery::LibraryRelinkCandidates {
                root_id,
                after_candidate_id,
                limit,
            } => BackendQueryResult::LibraryRelinkCandidates(
                self.repository.library_relink_candidates_page(
                    root_id.as_deref(),
                    after_candidate_id.as_deref(),
                    limit,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                )?,
            ),
            BackendQuery::LibraryRelinkResult { operation_id } => {
                BackendQueryResult::LibraryRelinkResult(
                    self.repository.library_relink_result(&operation_id)?,
                )
            }
            BackendQuery::LibraryRoots { offset, limit } => {
                BackendQueryResult::LibraryRoots(self.repository.library_roots_page(offset, limit)?)
            }
            BackendQuery::LibraryRoot { root_id } => {
                BackendQueryResult::LibraryRoot(self.repository.library_root(&root_id)?)
            }
            BackendQuery::LibraryRootHealth { root_id } => BackendQueryResult::LibraryRootHealth(
                self.repository
                    .library_root(&root_id)?
                    .map(|root| LibraryRootHealthState {
                        health: self
                            .library
                            .authority
                            .root_health(&root.root_id, root.grant_epoch),
                        root_id: root.root_id,
                        grant_epoch: root.grant_epoch,
                    }),
            ),
            BackendQuery::LibraryOperations { offset, limit } => {
                BackendQueryResult::LibraryOperations(self.library_operations(offset, limit)?)
            }
            BackendQuery::LibraryOperation { operation_id } => {
                BackendQueryResult::LibraryOperation(
                    self.library_operation(&operation_id)?.map(Box::new),
                )
            }
            BackendQuery::LibraryAssets {
                root_id,
                after_asset_id,
                limit,
                expected_revision,
            } => BackendQueryResult::LibraryAssets(self.effective_library_assets(
                self.repository.library_assets_page(
                    root_id.as_deref(),
                    after_asset_id.as_deref(),
                    limit,
                    expected_revision,
                )?,
            )?),
            BackendQuery::PictureInfo { list_id, music_id } => {
                BackendQueryResult::PictureInfo(self.picture_info(&list_id, &music_id)?)
            }
            BackendQuery::PictureOperations { offset, limit } => {
                BackendQueryResult::PictureOperations(self.picture_operations(offset, limit)?)
            }
            BackendQuery::PictureCacheInfo { offset, limit } => {
                BackendQueryResult::PictureCacheInfo(
                    self.repository.artwork_info_page(offset, limit)?,
                )
            }
            BackendQuery::PictureCachedBytes {
                sha256,
                offset,
                limit,
            } => {
                BackendQueryResult::PictureCachedBytes(self.picture_bytes(&sha256, offset, limit)?)
            }
            BackendQuery::ImageCacheStats => {
                BackendQueryResult::ImageCacheStats(self.picture_stats()?)
            }
            BackendQuery::MediaCacheStats => {
                BackendQueryResult::MediaCacheStats(self.repository.media_cache_stats()?)
            }
            BackendQuery::MusicMatches { list_id, music_id } => {
                let record = self
                    .repository
                    .list_track(&list_id, &music_id)?
                    .context("Track is unavailable")?;
                BackendQueryResult::MusicMatches(self.repository.source_matches(
                    &crate::domain::media::SourceMatchKey::new(&record)?,
                    media::now_ms(),
                )?)
            }
            BackendQuery::PlaybackProcessingSettings => {
                BackendQueryResult::PlaybackProcessingSettings(self.data.playback_processing)
            }
            BackendQuery::PlaybackProcessingState => {
                BackendQueryResult::PlaybackProcessingState(self.audio.processing_snapshot())
            }
            BackendQuery::AudioAnalysis => {
                BackendQueryResult::AudioAnalysis(self.audio.analysis_snapshot().map(Box::new))
            }
            BackendQuery::AudioAnalysisEnabled => {
                BackendQueryResult::AudioAnalysisEnabled(self.data.audio_analysis_enabled)
            }
            BackendQuery::SpatialSettings => BackendQueryResult::SpatialSettings(self.data.spatial),
            BackendQuery::SpatialState => {
                BackendQueryResult::SpatialState(self.audio.spatial_snapshot())
            }
            BackendQuery::EffectsSettings => {
                BackendQueryResult::EffectsSettings(self.effects_preferences().settings)
            }
            BackendQuery::EffectsState => {
                BackendQueryResult::EffectsState(Box::new(self.effects_state()))
            }
            BackendQuery::BundledImpulsePresets => {
                BackendQueryResult::BundledImpulsePresets(crate::audio::bundled_impulses::presets())
            }
            BackendQuery::ImpulseResources => {
                BackendQueryResult::ImpulseResources(self.effects_preferences())
            }
            BackendQuery::EffectsOperations => {
                BackendQueryResult::EffectsOperations(self.effects_operations())
            }
            BackendQuery::EqualizerSettings => {
                BackendQueryResult::EqualizerSettings(self.data.equalizer)
            }
            BackendQuery::EqualizerPresets => {
                BackendQueryResult::EqualizerPresets(crate::domain::equalizer::equalizer_presets())
            }
            BackendQuery::EqualizerState => {
                BackendQueryResult::EqualizerState(self.audio.equalizer_snapshot())
            }
            BackendQuery::AudioSettings => {
                BackendQueryResult::AudioSettings(self.data.audio_preferences.clone())
            }
            BackendQuery::AudioOutputState => {
                BackendQueryResult::AudioOutputState(self.audio.output_snapshot().state)
            }
            BackendQuery::AudioOutputRenderTiming => {
                BackendQueryResult::AudioOutputRenderTiming(self.audio.output_render_timing())
            }
            BackendQuery::AudioSupply => {
                BackendQueryResult::AudioSupply(self.audio.supply_snapshot())
            }
            BackendQuery::AudioDevices => {
                BackendQueryResult::AudioDevices(self.audio.output_snapshot().devices)
            }
            BackendQuery::RenderedPcm => BackendQueryResult::RenderedPcm(self.audio.rendered_pcm()),
            BackendQuery::AudioCacheStats => {
                BackendQueryResult::AudioCacheStats(self.audio.cache_stats())
            }
            BackendQuery::Preload => BackendQueryResult::Preload(
                self.shared.lock().unwrap().preload.clone().map(Box::new),
            ),
            BackendQuery::ListInfos => BackendQueryResult::ListInfos(self.repository.list_infos()?),
            BackendQuery::ListTracks {
                list_id,
                offset,
                limit,
            } => {
                ensure!(
                    limit > 0 && limit <= 1000,
                    "List page limit must be 1..=1000"
                );
                let page = self.repository.list_tracks_page(&list_id, offset, limit)?;
                BackendQueryResult::ListTracks(ListTrackPage {
                    items: page.items,
                    total: page.total as usize,
                    offset: page.offset,
                    limit: page.limit,
                    revision: page.revision,
                })
            }
            BackendQuery::ListTrack { list_id, music_id } => BackendQueryResult::ListTrack(
                self.repository
                    .list_track(&list_id, &music_id)?
                    .map(Box::new),
            ),
            BackendQuery::ListPositions => {
                BackendQueryResult::ListPositions(self.repository.list_positions()?)
            }
            BackendQuery::ListUpdateInfos => {
                BackendQueryResult::ListUpdateInfos(self.repository.list_update_infos()?)
            }
            BackendQuery::DislikeRules => {
                BackendQueryResult::DislikeRules(self.repository.dislike_rules()?)
            }
            BackendQuery::SearchHistory => {
                BackendQueryResult::SearchHistory(self.repository.search_history()?)
            }
            BackendQuery::Queue { offset, limit } => {
                BackendQueryResult::Queue(Box::new(self.queue_snapshot(offset, limit)?))
            }
            BackendQuery::PlaybackCheckpoint => BackendQueryResult::PlaybackCheckpoint(
                self.repository.playback_checkpoint()?.map(Box::new),
            ),
        })
    }
    fn select_list(&mut self, list_id: String) -> Result<()> {
        let records = self.list_records(&list_id)?;
        let mut candidate = self.queue.clone();
        candidate.set_list(
            list_id,
            records.iter().map(|track| track.id.clone()).collect(),
        );
        self.commit_queue(candidate)?;
        self.reconcile_queue()?;
        self.queue.invalidate_plan();
        self.failed_entries.clear();
        self.skip_pending = None;
        Ok(())
    }
    fn apply_lists(&mut self, action: &ListAction) -> Result<()> {
        let previous_current = self.queue.current.clone();
        let change = self.repository.apply_list_action_with_origin(
            action,
            Some(self.revision),
            self.mutation_origin(),
        )?;
        self.revision = change.revision;
        self.deep_links.note_temp_mutation(action);
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::ListsChanged {
                revision: self.revision,
                action: action.name().into(),
            },
        );
        if let Err(error) = self.reconcile_queue() {
            self.queue.set_list(DEFAULT_LIST.into(), Vec::new());
            self.reconcile_queue().context(error.to_string())?;
        }
        self.queue.invalidate_plan();
        self.failed_entries.clear();
        self.skip_pending = None;
        self.save_checkpoint()?;
        if let Some(current) = previous_current {
            let list_exists = current.list_id == "catalog"
                || self
                    .repository
                    .list_infos()?
                    .iter()
                    .any(|list| list.id == current.list_id);
            let removed = current.list_id != "catalog"
                && (!list_exists
                    || self
                        .repository
                        .list_track(&current.list_id, &current.track_id)?
                        .is_none());
            if removed && !current.temporary {
                let active = self.play_when_ready
                    && !self.restore_pending
                    && (self.busy
                        || matches!(
                            self.audio.snapshot().status,
                            PlaybackStatus::Playing | PlaybackStatus::Buffering
                        ));
                if active {
                    self.advance_queue(AdvanceReason::Ended, false)?;
                } else {
                    self.pending_promotion = None;
                    self.cancel_playback_resolution();
                    self.cancel_lyrics(true);
                    self.generation = self.generation.wrapping_add(1);
                    self.pending = None;
                    {
                        let mut snapshot = self.shared.lock().unwrap();
                        snapshot.pending_playback_url = None;
                        snapshot.approved_audio_identity = None;
                    }
                    self.audio.stop();
                    self.queue.clear_current();
                    self.current_record = None;
                    self.restore_pending = false;
                    self.restored_position_ms = 0;
                    self.seek_after_load = None;
                    self.save_checkpoint()?;
                }
            }
        }
        Ok(())
    }
    fn apply_dislike(&mut self, action: &DislikeAction) -> Result<()> {
        let change = self.repository.apply_dislike_action_with_origin(
            action,
            Some(self.revision),
            self.mutation_origin(),
        )?;
        self.revision = change.revision;
        self.dislike = self.repository.dislike_rules()?;
        self.queue.invalidate_plan();
        self.failed_entries.clear();
        self.skip_pending = None;
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::DislikeChanged {
                revision: self.revision,
            },
        );
        self.save_checkpoint()?;
        Ok(())
    }
    fn enqueue_refs(&mut self, items: Vec<QueueItemRef>, front: bool) -> Result<()> {
        ensure!(
            items.len() <= 10_000,
            "Temporary queue exceeds 10000 entries"
        );
        let mut refs = Vec::new();
        let mut records = Vec::new();
        for item in items {
            let entry = QueueEntry {
                entry_id: String::new(),
                list_id: item.list_id,
                track_id: item.track_id,
                temporary: true,
            };
            let record = self.queue_record(&entry)?;
            refs.push((entry.list_id.clone(), record.id.clone()));
            records.push((entry.list_id, record));
        }
        let mut candidate = self.queue.clone();
        candidate.enqueue_refs(refs, front);
        self.commit_queue(candidate)?;
        for (list_id, record) in records {
            self.eligibility.insert(
                (list_id, record.id.clone()),
                self.eligibility_record(&record),
            );
        }
        self.refresh_source_eligibility();
        self.failed_entries.clear();
        self.skip_pending = None;
        Ok(())
    }
    fn play_list_track(
        &mut self,
        list_id: String,
        music_id: String,
        quality: Option<String>,
    ) -> Result<()> {
        let records = self.list_records(&list_id)?;
        let record = records
            .iter()
            .find(|record| record.id == music_id || record.lx_id == music_id)
            .context("Requested list track does not exist")?;
        let lookup = QueueEntry {
            entry_id: String::new(),
            list_id: list_id.clone(),
            track_id: record.id.clone(),
            temporary: false,
        };
        let record = self.queue_record(&lookup)?;
        if let Some(quality) = quality.as_ref() {
            ensure!(
                matches!(quality.as_str(), "128k" | "320k" | "flac" | "flac24bit"),
                "Unsupported playback quality"
            );
        }
        let mut candidate = self.queue.clone();
        candidate.set_list(
            list_id,
            records.iter().map(|record| record.id.clone()).collect(),
        );
        candidate
            .select_track(&record.id)
            .context("Requested queue track does not exist")?;
        let old = self.preferred_quality.clone();
        if let Some(quality) = quality.as_ref() {
            self.preferred_quality = quality.clone();
        }
        if let Err(error) = self.commit_queue(candidate) {
            self.preferred_quality = old;
            return Err(error);
        }
        self.reconcile_queue()?;
        self.failed_entries.clear();
        self.failure_attempts = 0;
        self.skip_pending = None;
        self.restored_position_ms = 0;
        self.restore_pending = false;
        self.play_record(record, true, quality, false)
    }
    fn source_music_info(&self, record: &TrackRecord) -> Value {
        let mut info = record.to_source_music_info();
        if record.platform.source == "local"
            && let Some(track) = self
                .catalog
                .tracks()
                .iter()
                .find(|track| track.id == record.id)
        {
            info["filePath"] = serde_json::json!(track.source_url);
            info["songmid"] = serde_json::json!(track.id);
            info["ext"] = serde_json::json!(track.extension);
        }
        info
    }
    fn schedule_failure_skip(&mut self, error: &anyhow::Error) {
        if self.timeout_stop.blocks_automatic_playback()
            || !self.auto_skip_on_error
            || !self.play_when_ready
            || self.restore_pending
            || self.skip_pending.is_some()
        {
            return;
        }
        self.failure_attempts += 1;
        if let Some(entry) = self.queue.current.as_ref() {
            self.failed_entries.insert(entry.entry_id.clone());
            self.events.lock().unwrap().publish(
                self.revision,
                "playback",
                None,
                BackendEvent::TrackSkipped {
                    entry_id: entry.entry_id.clone(),
                    reason: format!("{error:#}").chars().take(2048).collect(),
                },
            );
        }
        if self.failure_attempts >= MAX_FAILURE_CHAIN {
            self.skip_pending = None;
            self.play_when_ready = false;
            self.audio.stop();
            self.error("Playback error chain reached its 256-entry limit".into());
        } else {
            self.queue.invalidate_plan();
            self.skip_pending = Some((Instant::now() + ERROR_SKIP_DELAY, self.generation));
        }
    }
    fn begin_provider(&mut self, platform: Platform, request: ProviderRequest) -> Result<()> {
        if self.provider_jobs.len() + self.deep_links.pending_count() >= PROVIDER_JOB_CAPACITY
            || self.provider_tasks.len() >= PROVIDER_JOB_CAPACITY
        {
            return Err(
                BackendError::new(ErrorKind::Busy, "Too many pending provider operations").into(),
            );
        }
        let operation_id = self.request_id.clone();
        let operation = request.operation().to_owned();
        let context = ProviderContext::new(self.provider_timeout);
        let cancel = context.cancel.clone();
        let registry = self.provider_registry.clone();
        let tx = self.completed_tx.clone();
        let running = Arc::new(ProviderOperationResult {
            operation_id: operation_id.clone(),
            platform,
            operation: operation.clone(),
            state: ProviderOperationState::Running,
            response: None,
            error: None,
        });
        let task = thread::Builder::new()
            .name(format!("lx-provider-{}", platform.as_str()))
            .spawn(move || {
                let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    registry.execute(platform, request, &context)
                }))
                .unwrap_or_else(|_| {
                    Err(ProviderError::upstream(
                        "Provider operation failed after a worker panic",
                    ))
                });
                let response = context.check().and(response);
                let (state, response, error) = match response {
                    Ok(response) => (
                        ProviderOperationState::Completed,
                        Some(Arc::new(response)),
                        None,
                    ),
                    Err(error) => (
                        ProviderOperationState::Failed,
                        None,
                        Some(bounded_provider_error(error)),
                    ),
                };
                let mut result = Arc::new(ProviderOperationResult {
                    operation_id: operation_id.clone(),
                    platform,
                    operation,
                    state,
                    response,
                    error,
                });
                let json_bytes = match provider_json_size(&result, Some(&context)) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        result = Arc::new(ProviderOperationResult {
                            operation_id: result.operation_id.clone(),
                            platform,
                            operation: result.operation.clone(),
                            state: ProviderOperationState::Failed,
                            response: None,
                            error: Some(error),
                        });
                        provider_json_size(&result, None).expect("bounded provider error envelope")
                    }
                };
                let _ = tx.send(Completion::Provider {
                    operation: operation_id,
                    result,
                    json_bytes,
                });
            })?;
        self.provider_jobs.insert(
            running.operation_id.clone(),
            ProviderJob {
                platform,
                operation: running.operation.clone(),
                cancel,
            },
        );
        self.cache_provider(running, None);
        self.provider_tasks.push(task);
        Ok(())
    }

    fn cache_provider(
        &mut self,
        mut result: Arc<ProviderOperationResult>,
        size: Option<usize>,
    ) -> Arc<ProviderOperationResult> {
        if let Some(index) = self
            .provider_results
            .iter()
            .position(|(entry, _)| entry.operation_id == result.operation_id)
        {
            let (_, bytes) = self.provider_results.remove(index).unwrap();
            self.provider_result_bytes -= bytes;
        }
        let mut bytes = size.unwrap_or_else(|| {
            provider_json_size(&result, None).expect("bounded provider metadata")
        });
        loop {
            if self.provider_results.len() < PROVIDER_RESULT_CAPACITY
                && bytes <= PROVIDER_RESULT_BYTES.saturating_sub(self.provider_result_bytes)
            {
                break;
            }
            if let Some(index) = self
                .provider_results
                .iter()
                .position(|(entry, _)| entry.state != ProviderOperationState::Running)
            {
                let (_, evicted) = self.provider_results.remove(index).unwrap();
                self.provider_result_bytes -= evicted;
            } else {
                result = Arc::new(ProviderOperationResult {
                    operation_id: result.operation_id.clone(),
                    platform: result.platform,
                    operation: result.operation.clone(),
                    state: ProviderOperationState::Failed,
                    response: None,
                    error: Some(ProviderError::ResponseTooLarge {
                        limit: PROVIDER_RESULT_BYTES,
                    }),
                });
                bytes = provider_json_size(&result, None).expect("bounded provider capacity error");
            }
        }
        self.provider_result_bytes += bytes;
        self.provider_results.push_back((result.clone(), bytes));
        result
    }

    fn finish_provider(&mut self, result: Arc<ProviderOperationResult>, json_bytes: Option<usize>) {
        let result = self.cache_provider(result, json_bytes);
        let origin = self
            .operation_origins
            .get(&result.operation_id)
            .cloned()
            .unwrap_or_else(|| "backend".into());
        self.events.lock().unwrap().publish(
            self.revision,
            &origin,
            Some(result.operation_id.clone()),
            BackendEvent::ProviderFinished {
                operation_id: result.operation_id.clone(),
                platform: result.platform,
                state: result.state,
                error: result.error.clone(),
            },
        );
        let outcome = match &result.error {
            None => Ok(()),
            Some(error) => Err(provider_backend_error(error).into()),
        };
        self.publish_finished(result.operation_id.clone(), &outcome);
    }

    fn cancel_provider(&mut self, operation_id: &str) {
        if self.cancel_deeplink_provider(operation_id) {
            return;
        }
        if let Some(job) = self.provider_jobs.remove(operation_id) {
            job.cancel.store(true, Ordering::Release);
            self.finish_provider(
                Arc::new(ProviderOperationResult {
                    operation_id: operation_id.into(),
                    platform: job.platform,
                    operation: job.operation,
                    state: ProviderOperationState::Failed,
                    response: None,
                    error: Some(ProviderError::Cancelled),
                }),
                None,
            );
        }
    }

    fn commit_data(&mut self, mut candidate: StoreData) -> Result<()> {
        self.system_control.require_store_mutation()?;
        if let Some(effects) = &mut candidate.effects {
            effects.settings.spatial = candidate.spatial;
        }
        self.store.save(&candidate)?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&candidate)?));
        if let Err(error) = self.repository.record_profile_change(&digest) {
            self.store
                .save(&self.data)
                .context("Profile update failed and original JSON could not be restored")?;
            return Err(error);
        }
        self.data = candidate;
        self.revision = self.repository.revision()?;
        Ok(())
    }
    fn commit_equalizer(
        &mut self,
        settings: crate::domain::equalizer::EqualizerSettings,
    ) -> Result<()> {
        settings
            .validate()
            .map_err(|error| BackendError::new(ErrorKind::InvalidData, error.to_string()))?;
        self.audio
            .validate_equalizer_admission(&settings)
            .map_err(|error| BackendError::new(ErrorKind::Unavailable, error.to_string()))?;
        let mut candidate = self.data.clone();
        candidate.equalizer = settings;
        self.commit_data(candidate)?;
        self.audio.set_equalizer(settings)
    }
    fn commit_processing(
        &mut self,
        settings: crate::domain::playback_processing::PlaybackProcessingSettings,
    ) -> Result<()> {
        settings
            .validate()
            .map_err(|error| BackendError::new(ErrorKind::InvalidData, error.to_string()))?;
        self.audio
            .validate_processing_admission(&settings)
            .map_err(|error| BackendError::new(ErrorKind::Unavailable, error.to_string()))?;
        let mut candidate = self.data.clone();
        candidate.playback_processing = settings;
        self.commit_data(candidate)?;
        self.audio.set_processing(settings)
    }
    fn reap_shutdown_tasks(&mut self) -> bool {
        let mut panicked = false;
        for tasks in [&mut self.tasks, &mut self.provider_tasks] {
            let mut index = 0;
            while index < tasks.len() {
                if tasks[index].is_finished() {
                    panicked |= tasks.swap_remove(index).join().is_err();
                } else {
                    index += 1;
                }
            }
        }
        panicked
    }
    fn reap_tasks(&mut self) {
        let mut running = Vec::new();
        for task in self.tasks.drain(..) {
            if task.is_finished() {
                let _ = task.join();
            } else {
                running.push(task);
            }
        }
        self.tasks = running;
        let mut running = Vec::new();
        for task in self.provider_tasks.drain(..) {
            if task.is_finished() {
                let _ = task.join();
            } else {
                running.push(task);
            }
        }
        self.provider_tasks = running;
    }
    fn publish_finished(&mut self, operation: String, result: &Result<()>) {
        if self.active_operation.as_deref() == Some(operation.as_str()) {
            self.active_operation = None;
        }
        self.refresh();
        let origin = self
            .operation_origins
            .remove(&operation)
            .unwrap_or_else(|| "backend".into());
        let error = result.as_ref().err().map(|error| {
            if let Some(error) = library::backend_library_error_ref(error) {
                return error;
            }
            if error.is::<crate::source::SourceRequestCancelled>() {
                return BackendError::new(ErrorKind::Cancelled, "Source request cancelled");
            }
            error
                .downcast_ref::<BackendError>()
                .cloned()
                .unwrap_or_else(|| {
                    BackendError::new(ErrorKind::OperationFailed, format!("{error:#}"))
                })
        });
        if let Some((_, _, cached)) = self.receipts.iter_mut().find(|(id, _, _)| id == &operation) {
            if let Some(error) = &error {
                *cached = Err(error.clone());
            } else if let Ok(receipt) = cached {
                receipt.state = ReceiptState::Completed;
                receipt.revision = self.revision;
            }
        }
        self.events.lock().unwrap().publish(
            self.revision,
            &origin,
            Some(operation.clone()),
            BackendEvent::OperationFinished {
                operation_id: operation,
                success: result.is_ok(),
                error,
            },
        );
    }
    fn mark_source_async_started(&mut self) {
        self.async_started = true;
        if !self.request_id.is_empty() {
            self.active_operation = Some(self.request_id.clone());
        }
    }
    fn execute(&mut self, envelope: CommandEnvelope) -> Result<CommandReceipt, BackendError> {
        let fingerprint = serde_json::to_string(&envelope).unwrap_or_default();
        if let Some((_, old, result)) = self
            .receipts
            .iter()
            .find(|(id, _, _)| id == &envelope.request_id)
        {
            return if old == &fingerprint {
                result.clone()
            } else {
                Err(BackendError::new(
                    ErrorKind::Conflict,
                    "request_id was already used for different input",
                ))
            };
        }
        if envelope
            .expected_revision
            .is_some_and(|revision| revision != self.revision)
        {
            return Err(BackendError::new(
                ErrorKind::Conflict,
                "Repository revision changed",
            ));
        }
        self.request_id = envelope.request_id.clone();
        self.origin = envelope.origin;
        self.async_started = false;
        self.operation_origins
            .insert(self.request_id.clone(), self.origin.clone());
        let result = self
            .handle(envelope.command)
            .map(|mut state| {
                if self.async_started {
                    state = ReceiptState::Accepted;
                }
                CommandReceipt {
                    request_id: self.request_id.clone(),
                    operation_id: self.request_id.clone(),
                    revision: self.revision,
                    state,
                }
            })
            .map_err(|error| {
                if let Some(error) = library::backend_library_error_ref(&error) {
                    return error;
                }
                if error.is::<crate::source::SourceRequestCancelled>() {
                    return BackendError::new(ErrorKind::Cancelled, "Source request cancelled");
                }
                self.error(format!("{error:#}"));
                error
                    .downcast_ref::<BackendError>()
                    .cloned()
                    .unwrap_or_else(|| {
                        BackendError::new(ErrorKind::OperationFailed, format!("{error:#}"))
                    })
            });
        self.refresh();
        if !matches!(&result,Ok(receipt) if receipt.state==ReceiptState::Accepted) {
            self.operation_origins.remove(&self.request_id);
        }
        self.receipts
            .push_back((self.request_id.clone(), fingerprint, result.clone()));
        if self.receipts.len() > 256 {
            self.receipts.pop_front();
        }
        self.request_id.clear();
        self.origin = "backend".into();
        result
    }
    fn begin_import(&mut self, script: String) -> Result<()> {
        ensure!(
            self.tasks.len() + self.tag_edits.job_count() < 4,
            "Too many pending operations"
        );
        let mut worker = SourceWorker::prepare()?;
        let cancel = worker.cancel_handle();
        let operation = self.request_id.clone();
        self.import_cancels.insert(operation.clone(), cancel);
        let tx = self.completed_tx.clone();
        self.tasks.push(thread::spawn(move || {
            let result = worker.validate_script(&script);
            let _ = tx.send(Completion::Imported(operation, result));
        }));
        Ok(())
    }
    fn begin_import_file(&mut self, path: PathBuf) -> Result<()> {
        ensure!(
            self.tasks.len() + self.tag_edits.job_count() < 4,
            "Too many pending operations"
        );
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut worker = SourceWorker::prepare()?;
        let operation = self.request_id.clone();
        self.import_cancels
            .insert(operation.clone(), worker.cancel_handle());
        let tx = self.completed_tx.clone();
        self.tasks.push(thread::spawn(move || {
            let result = worker.validate_file(&path);
            let _ = tx.send(Completion::Imported(operation, result));
        }));
        Ok(())
    }
    fn finish_lyric_operation(&mut self, operation: String, result: &Result<()>) {
        let origin = self
            .operation_origins
            .remove(&operation)
            .unwrap_or_else(|| "backend".into());
        let error = result.as_ref().err().map(lyric_error_ref);
        if let Some((_, _, cached)) = self.receipts.iter_mut().find(|(id, _, _)| id == &operation) {
            if let Some(error) = &error {
                *cached = Err(error.clone());
            } else if let Ok(receipt) = cached {
                receipt.state = ReceiptState::Completed;
                receipt.revision = self.revision;
            }
        }
        self.events.lock().unwrap().publish(
            self.revision,
            &origin,
            Some(operation.clone()),
            BackendEvent::OperationFinished {
                operation_id: operation,
                success: result.is_ok(),
                error,
            },
        );
    }
    fn cancel_lyrics(&mut self, clear_target: bool) {
        if let Some((_, _, Some(operation), _)) = self.pending_lyric.take() {
            self.finish_lyric_operation(
                operation,
                &Err(
                    BackendError::new(ErrorKind::Cancelled, "Queued lyric operation cancelled")
                        .into(),
                ),
            );
        }
        if clear_target {
            self.intended_track = None;
        }
        self.lyric_epoch = self.lyric_epoch.wrapping_add(1);
        if let Some(pending) = self.lyric_pending.take() {
            pending.cancel.store(true, Ordering::Release);
            if let Some(operation) = pending.operation {
                self.finish_lyric_operation(
                    operation,
                    &Err(
                        BackendError::new(ErrorKind::Cancelled, "Lyric operation cancelled").into(),
                    ),
                );
            }
        }
        self.lyric_key = None;
        self.lyric_timeline = None;
        let mut snapshot = self.shared.lock().unwrap();
        snapshot.lyrics = None;
        snapshot.lyric_frame = None;
        snapshot.lyrics_error = None;
    }
    fn begin_lyrics(
        &mut self,
        id: String,
        audio_generation: u64,
        operation: Option<String>,
        retries: u8,
    ) -> Result<()> {
        ensure!(
            self.tasks.len() + self.tag_edits.job_count() < 4,
            "Too many pending operations"
        );
        let record = self
            .current_record
            .as_ref()
            .filter(|record| record.id == id)
            .cloned()
            .or(self.repository.track(&id)?.map(Arc::new))
            .context("Lyric track does not exist")?;
        let music_info = self.source_music_info(&record);
        let mut worker = None;
        let mut initialize_script = None;
        if !self.busy
            && record.platform.source == "local"
            && let Some(source_id) = self.data.selected.as_ref()
            && let Some(source) = self
                .data
                .sources
                .iter()
                .find(|source| &source.sha256 == source_id)
        {
            source.require_approval()?;
            if self.source_inflight.is_some() || self.source_initializing {
                self.invalidate_preload(Invalidation::Playback);
                self.lyric_key = Some((id.clone(), audio_generation));
                self.pending_lyric = Some((id, audio_generation, operation, retries));
                return Ok(());
            } else if self.runtime.as_ref().is_none_or(SourceWorker::is_stopped) {
                self.runtime.take();
                worker = Some(SourceWorker::prepare()?);
                self.source_epoch = self.source_epoch.wrapping_add(1);
                initialize_script = Some(source.script.clone());
            } else if self.runtime.as_ref().is_some_and(|worker| {
                worker
                    .manifest()
                    .sources
                    .get("local")
                    .is_some_and(|caps| caps.actions.iter().any(|action| action == "lyric"))
            }) {
                worker = self.runtime.take();
            }
        }
        let mut request = LyricRequest::new(&id, audio_generation);
        request.record_override = Some(record);
        request.platform_resolver = request
            .record_override
            .as_ref()
            .is_some_and(|record| record.platform.source != "local");
        let cancel = request.cancel.clone();
        if let Some(worker) = &worker {
            self.cancellation = Some(worker.cancel_handle());
            self.source_inflight = Some(SourceFlight {
                download: false,
                source_epoch: self.source_epoch,
                cancel: cancel.clone(),
                preload_operation: None,
            });
        }
        self.lyric_epoch = self.lyric_epoch.wrapping_add(1);
        let epoch = self.lyric_epoch;
        self.lyric_key = Some((id.clone(), audio_generation));
        self.lyric_pending = Some(PendingLyrics {
            track_id: id,
            audio_generation,
            epoch,
            cancel,
            operation,
            retries,
        });
        let service = self.lyric_service.clone();
        let tx = self.completed_tx.clone();
        let source_generation = self.generation;
        let source_epoch = self.source_epoch;
        let provider_registry = self.provider_registry.clone();
        self.tasks.push(thread::spawn(move || {
            let result = if let Some(active) = worker.as_mut() {
                let mut resolver =
                    |_: &TrackRecord, request: &LyricRequest| -> Result<LyricBundle> {
                        request.check_cancelled()?;
                        if !active.is_initialized() {
                            active.initialize(initialize_script.as_deref().context(
                                "Approved source script is unavailable for lyric restart",
                            )?)?;
                            request.check_cancelled()?;
                        }
                        match active.request_cancellable(
                            &SourceRequest {
                                source: "local".into(),
                                action: "lyric".into(),
                                info: serde_json::json!({"musicInfo":music_info}),
                            },
                            || request.cancel.load(Ordering::Acquire),
                        )? {
                            SourceResult::Lyrics(bundle) => Ok(bundle),
                            _ => anyhow::bail!("Lyric source returned a URL"),
                        }
                    };
                service.prepare(request, Some(&mut resolver))
            } else if request.platform_resolver {
                let mut resolver =
                    |track: &TrackRecord, request: &LyricRequest| -> Result<LyricBundle> {
                        let platform: Platform =
                            serde_json::from_value(serde_json::json!(track.platform.source))
                                .context("Lyric platform is unsupported")?;
                        let context = ProviderContext {
                            cancel: request.cancel.clone(),
                            deadline: Instant::now() + Duration::from_secs(20),
                        };
                        let response = provider_registry.execute(
                            platform,
                            ProviderRequest::Lyrics {
                                track: track.clone(),
                                word_by_word: true,
                            },
                            &context,
                        )?;
                        let ProviderResponse::Lyrics(lyrics) = response else {
                            anyhow::bail!("Lyric provider returned an incompatible response");
                        };
                        let mut data = serde_json::Map::new();
                        data.insert("lyric".into(), serde_json::json!(lyrics.lyric));
                        for (name, value) in [
                            ("tlyric", lyrics.translated),
                            ("rlyric", lyrics.romanized),
                            ("lxlyric", lyrics.word_by_word),
                        ] {
                            if let Some(value) = value {
                                data.insert(name.into(), serde_json::json!(value));
                            }
                        }
                        LyricBundle::from_value(&Value::Object(data))
                    };
                service.prepare(request, Some(&mut resolver))
            } else {
                service.prepare(request, None)
            };
            let _ = tx.send(Completion::Lyrics {
                epoch,
                source_generation,
                source_epoch,
                result,
                worker,
            });
        }));
        Ok(())
    }
    fn complete_lyrics(
        &mut self,
        epoch: u64,
        source_generation: u64,
        source_epoch: u64,
        result: Result<PreparedLyrics>,
        worker: Option<SourceWorker>,
    ) {
        if let Some(mut worker) = worker
            && source_epoch == self.source_epoch
        {
            if let Some(source_id) = self.data.selected.clone() {
                self.publish_source_events(source_id, worker.take_host_events());
            }
            self.return_source_worker(source_epoch, worker);
            let _ = self.start_pending_foreground();
        }
        if !self
            .lyric_pending
            .as_ref()
            .is_some_and(|pending| pending.epoch == epoch)
            || source_generation != self.generation
        {
            return;
        }
        let pending = self.lyric_pending.take().unwrap();
        let audio = self.audio.snapshot();
        if pending.cancel.load(Ordering::Acquire)
            || audio.generation != pending.audio_generation
            || audio.track_id.as_deref() != Some(&pending.track_id)
        {
            if let Some(operation) = pending.operation {
                self.finish_lyric_operation(
                    operation,
                    &Err(
                        BackendError::new(ErrorKind::Cancelled, "Old lyric result discarded")
                            .into(),
                    ),
                );
            }
            return;
        }
        let outcome = result.and_then(|prepared| {
            self.lyric_service
                .commit(prepared, &pending.track_id, audio.generation)
        });
        match outcome {
            Ok(committed) => {
                self.revision = committed.revision;
                let mut timeline = LyricTimeline::new(committed.document.clone(), audio.generation);
                timeline.set_offset(self.lyric_offset);
                self.lyric_timeline = Some(timeline);
                let mut snapshot = self.shared.lock().unwrap();
                snapshot.lyrics = Some(Arc::new(LyricSnapshot {
                    track_id: committed.track_id.clone(),
                    generation: audio.generation,
                    bundle: committed.bundle,
                    document: committed.document,
                    provenance: committed.provenance,
                }));
                snapshot.lyrics_error = None;
                drop(snapshot);
                self.events.lock().unwrap().publish(
                    self.revision,
                    "backend",
                    pending.operation.clone(),
                    BackendEvent::LyricChanged {
                        track_id: committed.track_id,
                        generation: audio.generation,
                    },
                );
                if let Some(operation) = pending.operation {
                    self.finish_lyric_operation(operation, &Ok(()));
                }
            }
            Err(error) => {
                let conflict = error.downcast_ref::<LyricServiceError>()
                    == Some(&LyricServiceError::RevisionConflict)
                    || error.to_string().contains("revision conflict");
                if conflict && pending.retries < 2 {
                    let operation = pending.operation.clone();
                    if self
                        .begin_lyrics(
                            pending.track_id,
                            pending.audio_generation,
                            operation.clone(),
                            pending.retries + 1,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                if !matches!(
                    error.downcast_ref::<LyricServiceError>(),
                    Some(
                        LyricServiceError::Cancelled
                            | LyricServiceError::StaleGeneration
                            | LyricServiceError::ChangedTrack
                    )
                ) {
                    self.shared.lock().unwrap().lyrics_error = Some(error.to_string());
                }
                if let Some(operation) = pending.operation {
                    self.finish_lyric_operation(operation, &Err(error));
                }
            }
        }
    }
    fn dispatch_completion(&mut self, completion: Completion) {
        self.poll_timeout_stop();
        match completion {
            Completion::BackupExportWake => self.poll_backup_exports(),
            Completion::PhysicalSnapshotWake => self.poll_physical_snapshots(),
            Completion::DeepLink { operation, event } => {
                self.complete_deeplink_provider(operation, event)
            }
            Completion::BackupPreviewPrepared { operation, result } => {
                self.finish_backup_preview(operation, result)
            }
            Completion::NativeBackupPreviewPrepared { operation, result } => {
                self.finish_native_backup_preview(operation, *result)
            }
            Completion::DownloadRecoveryVerified {
                operation,
                task_id,
                expected_task_revision,
                operation_epoch,
                result,
            } => {
                self.finish_download_recovery(
                    operation,
                    task_id,
                    expected_task_revision,
                    operation_epoch,
                    result,
                );
            }
            Completion::DownloadPostprocessPrepared {
                task_id,
                operation_epoch,
                result,
            } => {
                self.finish_download_postprocess_preparation(task_id, operation_epoch, result);
            }
            Completion::DownloadAudioCommitted {
                task_id,
                operation_epoch,
                original,
                stages,
                original_audio,
                result,
            } => {
                self.finish_download_audio_commit(
                    task_id,
                    operation_epoch,
                    original,
                    stages,
                    original_audio,
                    *result,
                );
            }
            Completion::DownloadCleanupFinished {
                task_id,
                operation,
                work,
                result,
            } => self.finish_cleanup(task_id, operation, work, result),
            Completion::DownloadCleanupRetired { task_id, operation } => {
                self.finish_cleanup_retired(task_id, operation)
            }
            Completion::DownloadTransferPrepared {
                task_id,
                operation_epoch,
                result,
            } => {
                self.finish_download_transfer_preparation(task_id, operation_epoch, result);
            }
            Completion::DownloadResolved {
                task_id,
                operation_epoch,
                source_epoch,
                source_id,
                policy_epoch,
                worker,
                result,
            } => {
                self.finish_download_resolve(
                    task_id,
                    operation_epoch,
                    source_epoch,
                    source_id,
                    policy_epoch,
                    worker,
                    result,
                );
            }
            Completion::DownloadDestination { operation, result } => {
                self.finish_download_destination(operation, result)
            }
            Completion::Picture { operation, result } => self.complete_picture(operation, result),
            Completion::CacheCleared(operation, result) => {
                self.cache_jobs.remove(&operation);
                if let Ok(stats) = &result {
                    self.events.lock().unwrap().publish(
                        self.revision,
                        self.operation_origins
                            .get(&operation)
                            .map(String::as_str)
                            .unwrap_or("backend"),
                        Some(operation.clone()),
                        BackendEvent::AudioCacheCleared {
                            operation_id: operation.clone(),
                            stats: *stats,
                        },
                    );
                }
                self.publish_finished(operation, &result.map(|_| ()));
            }
            Completion::SourceEvents {
                generation,
                source_id,
                events,
            } => {
                if generation == self.generation {
                    self.publish_source_events(source_id, events);
                }
            }
            Completion::Provider {
                operation,
                result,
                json_bytes,
            } => {
                if let Some(job) = self.provider_jobs.remove(&operation) {
                    if job.cancel.load(Ordering::Acquire) {
                        self.finish_provider(
                            Arc::new(ProviderOperationResult {
                                operation_id: operation,
                                platform: job.platform,
                                operation: job.operation,
                                state: ProviderOperationState::Failed,
                                response: None,
                                error: Some(ProviderError::Cancelled),
                            }),
                            None,
                        );
                    } else {
                        self.finish_provider(result, Some(json_bytes));
                    }
                }
            }
            Completion::Lyrics {
                epoch,
                source_generation,
                source_epoch,
                result,
                worker,
            } => self.complete_lyrics(epoch, source_generation, source_epoch, result, worker),
            Completion::SourcePicture {
                operation,
                source_epoch,
                worker,
                result,
            } => {
                let current = source_epoch == self.source_epoch;
                self.return_source_worker(source_epoch, worker);
                self.complete_picture_source(
                    operation,
                    source_epoch,
                    if current {
                        result
                    } else {
                        Err(crate::source::SourceRequestCancelled.into())
                    },
                );
                if let Err(error) = self.start_pending_foreground() {
                    self.error(format!("{error:#}"));
                }
            }
            Completion::Matched {
                operation,
                list_id,
                original,
                cache_epoch,
                result,
            } => {
                let cancelled = self
                    .file_cancels
                    .remove(&operation)
                    .is_none_or(|flag| flag.load(Ordering::Acquire))
                    || self.cancelled.remove(&operation);
                let outcome = (|| -> Result<()> {
                    ensure!(
                        !cancelled && cache_epoch == self.media_cache_epoch,
                        crate::source::SourceRequestCancelled
                    );
                    let value = result?;
                    let current = self
                        .repository
                        .list_track(&list_id, &original.id)?
                        .context("Matched track was removed")?;
                    ensure!(
                        crate::domain::media::track_record_fingerprint(&current)?
                            == value.key.original_record_sha256,
                        "Track metadata changed while matching"
                    );
                    self.repository.put_source_matches(&value)
                })();
                self.publish_finished(operation, &outcome);
            }
            Completion::Imported(operation, result) => {
                self.import_cancels.remove(&operation);
                let result = if self.cancelled.remove(&operation) {
                    Err(BackendError::new(ErrorKind::Cancelled, "Operation cancelled").into())
                } else {
                    result.and_then(|source| self.apply_import(source))
                };
                if let Err(error) = &result {
                    self.error(format!("{error:#}"));
                }
                self.publish_finished(operation, &result);
            }
            Completion::LocalPrepared {
                operation,
                result,
                ack,
            } => {
                let cancellation = self.file_cancels.remove(&operation);
                let result = if self.cancelled.remove(&operation) {
                    Err(BackendError::new(ErrorKind::Cancelled, "Operation cancelled").into())
                } else {
                    result.and_then(|mut prepared| {
                        let check = || -> Result<()> {
                            ensure!(
                                cancellation
                                    .as_ref()
                                    .is_some_and(|cancel| !cancel.load(Ordering::Acquire)),
                                crate::domain::library::LibraryError::new(
                                    crate::domain::library::LibraryErrorKind::Cancelled,
                                    "Audio import cancelled"
                                )
                            );
                            Ok(())
                        };
                        let mut records = Vec::with_capacity(prepared.len());
                        for (local, record, asset) in &mut prepared {
                            let identity = self
                                .repository
                                .prepare_library_identity(&local.library_asset(), &check)?;
                            local.bind_library_identity(&identity)?;
                            *record = identity.track.clone();
                            *asset = identity.asset.clone();
                            records.push(identity);
                        }
                        self.repository
                            .upsert_explicit_library_tracks(&records, &check)?;
                        self.revision = self.repository.revision()?;
                        for (mut item, _, asset) in prepared {
                            item.set_authorization(library::file_authorization(
                                &self.repository,
                                &self.library.authority,
                                &asset.id,
                            )?);
                            self.catalog.commit_local(item);
                        }
                        Ok(())
                    })
                };
                let _ = ack.try_send(());
                if let Err(error) = &result {
                    self.error(format!("{error:#}"));
                }
                self.publish_finished(operation, &result);
            }
            other => {
                let failed_resolution = matches!(&other, Completion::Resolved { generation, preload_operation: None, .. } if *generation == self.generation);
                let initialized = matches!(&other, Completion::Initialized(epoch, ..) if *epoch == self.source_epoch);
                let current = initialized || failed_resolution;
                let outcome = self.complete(other);
                if current {
                    let mut recovering = false;
                    if let Err(error) = &outcome {
                        if failed_resolution && !error.is::<crate::source::SourceRequestCancelled>()
                        {
                            recovering = self.recover_resolution_failure(error).unwrap_or(false);
                        }
                        if !recovering {
                            self.error(format!("{error:#}"));
                            if failed_resolution
                                && !error.is::<crate::source::SourceRequestCancelled>()
                            {
                                self.schedule_failure_skip(error);
                            }
                        }
                    }
                    let operation = if recovering {
                        None
                    } else if initialized {
                        self.init_operation.take()
                    } else {
                        self.active_operation.take()
                    };
                    if let Some(operation) = operation {
                        if initialized
                            && self.active_operation.as_deref() == Some(operation.as_str())
                        {
                            self.active_operation = None;
                        }
                        self.publish_finished(operation, &outcome);
                    }
                }
                if let Err(error) = self.start_pending_foreground() {
                    self.busy = false;
                    self.error(format!("{error:#}"));
                    if let Some(operation) = self.active_operation.take() {
                        self.publish_finished(operation, &Err(error));
                    }
                }
            }
        }
    }
}
impl Runtime {
    fn error(&self, error: String) {
        self.shared.lock().unwrap().error = Some(error);
    }
    fn notice(&self, notice: impl Into<String>) {
        self.shared.lock().unwrap().notice = Some(notice.into());
    }
    fn load_approved(&mut self, url: &str, identity: CacheIdentity, autoplay: bool) -> Result<()> {
        let request = ApprovedAudioRequest::new(url, identity)?;
        self.approved_request = Some(request.clone());
        self.shared.lock().unwrap().approved_audio_identity = Some(request.identity.clone());
        self.audio.load_approved(request, autoplay);
        Ok(())
    }
    fn begin_clear_audio_cache(&mut self) -> Result<()> {
        ensure!(
            self.cache_jobs.is_empty(),
            BackendError::new(ErrorKind::Busy, "Audio cache cleanup is already running")
        );
        ensure!(
            self.tasks.len() + self.tag_edits.job_count() < 4,
            BackendError::new(ErrorKind::Busy, "Too many pending operations")
        );
        let audio = self.audio.clone();
        let tx = self.completed_tx.clone();
        let operation = self.request_id.clone();
        let job_operation = operation.clone();
        let task = thread::Builder::new()
            .name("lx-audio-cache-clear".into())
            .spawn(move || {
                let result = audio.clear_cache().map_err(|error| {
                    BackendError::new(
                        ErrorKind::Storage,
                        format!("Audio cache cleanup failed: {error:#}"),
                    )
                    .into()
                });
                let _ = tx.send(Completion::CacheCleared(job_operation, result));
            })
            .context("Start audio cache cleanup")?;
        self.cache_jobs.insert(operation);
        self.tasks.push(task);
        Ok(())
    }
    fn cancel_runtime(&mut self) {
        self.revoke_download_execution();
        self.reset_media_recovery();
        self.cancel_picture_source_requests();
        self.pending_promotion = None;
        self.invalidate_preload(Invalidation::Source);
        self.pending_foreground = None;
        if let Some(flight) = self.source_inflight.take() {
            flight.cancel.store(true, Ordering::Release);
        }
        self.source_initializing = false;
        if let Some(operation) = self.init_operation.take() {
            if self.active_operation.as_deref() == Some(operation.as_str()) {
                self.active_operation = None;
            }
            self.publish_finished(
                operation,
                &Err(BackendError::new(ErrorKind::Cancelled, "Source session closed").into()),
            );
        }
        self.skip_pending = None;
        self.cancel_lyrics(true);
        if !self
            .system_control
            .owns_operation(self.active_operation.as_deref().unwrap_or(""))
            && let Some(operation) = self.active_operation.take()
        {
            self.publish_finished(
                operation,
                &Err(BackendError::new(
                    ErrorKind::Cancelled,
                    "Operation cancelled by source or playback change",
                )
                .into()),
            );
        }
        self.generation = self.generation.wrapping_add(1);
        self.source_epoch = self.source_epoch.wrapping_add(1);
        if let Some(cancel) = self.cancellation.take() {
            cancel.terminate();
        }
        self.runtime.take();
        self.source_caps = None;
        self.queue.invalidate_plan();
        self.refresh_source_eligibility();
        self.pending = None;
        self.busy = false;
        for (id, status) in &mut self.statuses {
            if status.starts_with("运行中") || status.starts_with("正在初始化") {
                *status = if self.data.selected.as_deref() == Some(id.as_str()) {
                    "已授权 · 待按需初始化"
                } else {
                    "已授权 · 已停用"
                }
                .into();
            }
        }
        let mut snapshot = self.shared.lock().unwrap();
        snapshot.pending_playback_url = None;
        snapshot.approved_audio_identity = None;
        snapshot.resolved_track = None;
    }

    fn cancel_playback_resolution(&mut self) {
        self.pending_foreground = None;
        if let Some(flight) = &self.source_inflight
            && !flight.download
        {
            flight.cancel.store(true, Ordering::Release);
        }
        if self.active_operation.as_ref() != self.init_operation.as_ref()
            && !self
                .system_control
                .owns_operation(self.active_operation.as_deref().unwrap_or(""))
            && let Some(operation) = self.active_operation.take()
        {
            self.publish_finished(
                operation,
                &Err(BackendError::new(ErrorKind::Cancelled, "Playback selection changed").into()),
            );
        }
        self.busy = self.source_initializing;
    }

    fn selected_quality(
        record: &TrackRecord,
        manifest: &crate::source::RuntimeManifest,
        preferred: &str,
        strict: Option<&str>,
    ) -> Result<String> {
        let capabilities =
            manifest
                .sources
                .get(&record.platform.source)
                .context(BackendError::new(
                    ErrorKind::Unavailable,
                    "Approved source does not support this platform",
                ))?;
        ensure!(
            capabilities
                .actions
                .iter()
                .any(|action| action == "musicUrl"),
            BackendError::new(
                ErrorKind::Unavailable,
                "Approved source does not provide musicUrl"
            )
        );
        if record.platform.source == "local" {
            return Ok("local".into());
        }
        let supported = capabilities
            .qualitys
            .iter()
            .filter(|quality| {
                record.platform.qualities.is_empty() || record.platform.qualities.contains(quality)
            })
            .collect::<Vec<_>>();
        let selected = if let Some(required) = strict {
            supported
                .iter()
                .find(|quality| quality.as_str() == required)
                .context(BackendError::new(
                    ErrorKind::Unavailable,
                    "Approved source or track does not support the explicitly requested quality",
                ))?
        } else {
            supported
                .iter()
                .find(|quality| quality.as_str() == preferred)
                .or_else(|| supported.iter().find(|quality| quality.as_str() == "128k"))
                .or(supported.first())
                .context(BackendError::new(
                    ErrorKind::Unavailable,
                    "Approved source and track have no compatible quality",
                ))?
        };
        Ok(selected.as_str().to_owned())
    }

    fn start_pending_foreground(&mut self) -> Result<()> {
        if self.source_initializing || self.source_inflight.is_some() || self.tasks.len() >= 4 {
            return Ok(());
        }
        if let Some(job) = self.pending_foreground.take() {
            self.start_source_job(job)?;
        } else if let Some(job) = self.pending_pictures.pop_front() {
            self.pending_picture_bytes = self
                .pending_picture_bytes
                .saturating_sub(serde_json::to_vec(&job.request).map_or(0, |bytes| bytes.len()));
            if job.cancel.load(Ordering::Acquire) || Instant::now() >= job.deadline {
                self.complete_picture_source(
                    job.operation,
                    self.source_epoch,
                    Err(crate::source::SourceRequestCancelled.into()),
                );
                return self.start_pending_foreground();
            }
            let operation = job.operation.clone();
            if let Err(error) = self.start_source_picture(job) {
                self.complete_picture_source(operation, self.source_epoch, Err(error));
            }
        } else if let Some((id, generation, operation, retries)) = self.pending_lyric.take() {
            self.begin_lyrics(id, generation, operation, retries)?;
        } else {
            self.pump_download_resolution()?;
        }
        Ok(())
    }

    fn queue_picture_source(&mut self, job: PictureSourceRequest) -> Result<()> {
        let bytes = serde_json::to_vec(&job.request)?.len();
        ensure!(
            self.pending_pictures.len() < 16
                && bytes <= 2 * 1024 * 1024
                && bytes <= (4 * 1024 * 1024usize).saturating_sub(self.pending_picture_bytes),
            BackendError::new(ErrorKind::Busy, "Picture source queue capacity reached")
        );
        self.pending_picture_bytes += bytes;
        self.pending_pictures.push_back(job);
        self.start_pending_foreground()
    }

    fn start_source_picture(&mut self, job: PictureSourceRequest) -> Result<()> {
        ensure!(
            !self.source_initializing && self.source_inflight.is_none(),
            BackendError::new(ErrorKind::Busy, "Source session is already leased")
        );
        let source_id = self
            .data
            .selected
            .clone()
            .context("An approved picture source is required")?;
        let source = self
            .data
            .sources
            .iter()
            .find(|source| source.sha256 == source_id)
            .context("Selected picture source is unavailable")?
            .clone();
        source.require_approval()?;
        let mut worker = match self.runtime.take().filter(|worker| !worker.is_stopped()) {
            Some(worker) => worker,
            None => {
                self.source_epoch = self.source_epoch.wrapping_add(1);
                SourceWorker::prepare()?
            }
        };
        let epoch = self.source_epoch;
        self.cancellation = Some(worker.cancel_handle());
        self.source_inflight = Some(SourceFlight {
            download: false,
            source_epoch: epoch,
            cancel: job.cancel.clone(),
            preload_operation: None,
        });
        let generation = self.generation;
        let tx = self.completed_tx.clone();
        let spawned = thread::Builder::new()
            .name("lx-source-picture".into())
            .spawn(move || {
                let result = (|| -> Result<String> {
                    ensure!(
                        !job.cancel.load(Ordering::Acquire) && Instant::now() < job.deadline,
                        crate::source::SourceRequestCancelled
                    );
                    if !worker.is_initialized() {
                        worker.initialize(&source.script)?;
                    }
                    worker.request_url_cancellable(&job.request, || {
                        job.cancel.load(Ordering::Acquire) || Instant::now() >= job.deadline
                    })
                })();
                let _ = tx.send(Completion::SourceEvents {
                    generation,
                    source_id,
                    events: worker.take_host_events(),
                });
                let _ = tx.send(Completion::SourcePicture {
                    operation: job.operation,
                    source_epoch: epoch,
                    worker,
                    result,
                });
            });
        match spawned {
            Ok(task) => self.tasks.push(task),
            Err(error) => {
                self.source_inflight = None;
                self.cancellation = None;
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn start_source_job(&mut self, job: SourceResolveJob) -> Result<()> {
        ensure!(
            !self.source_initializing && self.source_inflight.is_none(),
            BackendError::new(ErrorKind::Busy, "Source session is already leased")
        );
        let source_id = self
            .data
            .selected
            .clone()
            .context("An approved source is required")?;
        let source = self
            .data
            .sources
            .iter()
            .find(|source| source.sha256 == source_id)
            .context("Selected source is unavailable")?
            .clone();
        source.require_approval()?;
        let mut worker = match self.runtime.take().filter(|worker| !worker.is_stopped()) {
            Some(worker) => worker,
            None => {
                self.source_epoch = self.source_epoch.wrapping_add(1);
                SourceWorker::prepare()?
            }
        };
        let source_epoch = self.source_epoch;
        self.cancellation = Some(worker.cancel_handle());
        let cancel = Arc::new(AtomicBool::new(false));
        let preload_operation = job
            .preload
            .as_ref()
            .map(|action| action.operation_id.clone());
        self.source_inflight = Some(SourceFlight {
            download: false,
            source_epoch,
            cancel: cancel.clone(),
            preload_operation: preload_operation.clone(),
        });
        let preferred = self.preferred_quality.clone();
        let music_info = self.source_music_info(&job.record);
        let local_fallback = self
            .catalog
            .track(&job.record.id)
            .map(|track| track.source_url);
        let identity = CacheIdentity {
            track_id: job.record.id.clone(),
            platform: job.record.platform.source.clone(),
            source_sha256: source.sha256.clone(),
            source_policy_version: u64::from(source.approval_policy_version),
            source_epoch,
            approval_id: format!(
                "source:{}:{}",
                source.sha256, source.approval_policy_version
            ),
            approval_epoch: self.approval_epoch,
            ..Default::default()
        };
        let repository = self.repository.clone();
        let matching = self.matching_service.clone();
        let platforms = self.provider_registry.platforms();
        let allow_switch = self.auto_switch_source && job.preload.is_none();
        let timeout = self.provider_timeout;
        let tx = self.completed_tx.clone();
        self.tasks.push(
            thread::Builder::new()
                .name("lx-source-url".into())
                .spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(70);
                    let result = (|| -> Result<media::SourceResolution> {
                        if !worker.is_initialized() {
                            worker.initialize(&source.script)?;
                        }
                        media::resolve(
                            &mut worker,
                            &job,
                            media::ResolveInputs {
                                repository: &repository,
                                matching: &matching,
                                platforms,
                                preferred: &preferred,
                                allow_switch,
                                timeout,
                                deadline,
                                identity,
                                original_info: music_info,
                                local_fallback,
                                cancel,
                            },
                        )
                        .map_err(|error| {
                            if Instant::now() >= deadline
                                && error.is::<crate::source::SourceRequestCancelled>()
                            {
                                BackendError::new(
                                    ErrorKind::Timeout,
                                    "Source resolution exceeded its deadline",
                                )
                                .into()
                            } else {
                                error
                            }
                        })
                    })();
                    let (result, cache_entry, matches, cache_hit, attempted_platforms) =
                        match result {
                            Ok(value) => (
                                Ok((value.url, value.identity)),
                                value.cache_entry,
                                value.matches,
                                value.cache_hit,
                                value.attempted_platforms,
                            ),
                            Err(error) => (Err(error), None, None, false, Vec::new()),
                        };
                    let _ = tx.send(Completion::SourceEvents {
                        generation: job.generation,
                        source_id,
                        events: worker.take_host_events(),
                    });
                    let _ = tx.send(Completion::Resolved {
                        generation: job.generation,
                        source_epoch,
                        preload_operation,
                        worker,
                        result,
                        original: job.record,
                        cache_entry: cache_entry.map(Box::new),
                        matches: matches.map(Box::new),
                        cache_hit,
                        attempted_platforms,
                    });
                })?,
        );
        Ok(())
    }

    fn return_source_worker(&mut self, source_epoch: u64, worker: SourceWorker) {
        if source_epoch != self.source_epoch {
            return;
        }
        if self
            .source_inflight
            .as_ref()
            .is_some_and(|flight| flight.source_epoch == source_epoch)
        {
            self.source_inflight = None;
        }
        if worker.is_initialized() && !worker.is_stopped() {
            let changed = self.source_caps.as_ref() != Some(worker.manifest());
            self.cancellation = Some(worker.cancel_handle());
            self.source_caps = Some(worker.manifest().clone());
            self.runtime = Some(worker);
            if changed {
                self.invalidate_preload(Invalidation::Source);
                self.queue.invalidate_plan();
                self.refresh_source_eligibility();
            }
        } else {
            self.cancellation = None;
        }
    }

    fn enable(&mut self, id: &str) -> Result<()> {
        let source = self
            .data
            .sources
            .iter()
            .find(|source| source.sha256 == id)
            .context("没有找到这个音源")?
            .clone();
        source.require_approval()?;
        self.cancel_runtime();
        self.queue.invalidate_plan();
        self.failed_entries.clear();
        self.failure_attempts = 0;
        self.audio.stop();
        let mut candidate = self.data.clone();
        candidate.selected = Some(id.into());
        self.commit_data(candidate)?;
        let mut worker = SourceWorker::prepare()?;
        self.cancellation = Some(worker.cancel_handle());
        self.busy = true;
        self.source_initializing = true;
        self.init_operation = Some(self.request_id.clone());
        self.statuses.insert(id.into(), "正在初始化…".into());
        self.notice("正在启动音源独立进程…");
        let generation = self.generation;
        let source_epoch = self.source_epoch;
        let tx = self.completed_tx.clone();
        let id = id.to_owned();
        self.tasks.push(thread::spawn(move || {
            let initialized = worker.initialize(&source.script);
            let _ = tx.send(Completion::SourceEvents {
                generation,
                source_id: id.clone(),
                events: worker.take_host_events(),
            });
            let result = initialized.map(|()| worker);
            let _ = tx.send(Completion::Initialized(source_epoch, id, result));
        }));
        self.mark_source_async_started();
        Ok(())
    }

    fn invalidate_preload(&mut self, reason: Invalidation) {
        self.preload_failure_snapshot = None;
        let snapshot = self.preload.snapshot();
        if matches!(reason, Invalidation::ApprovalRevoked | Invalidation::Cache)
            && let Some(candidate) = snapshot.candidate.as_ref()
        {
            let audio = self.audio.snapshot();
            self.preload_suppressed = Some((
                audio.generation,
                self.queue.revision,
                audio.seek_seq,
                candidate.entry_id.clone(),
            ));
        }
        if let Some(operation) = snapshot.operation_id.as_deref() {
            if let Some(flight) = &self.source_inflight
                && flight.preload_operation.as_deref() == Some(operation)
            {
                flight.cancel.store(true, Ordering::Release);
            }
            self.audio.cancel_preload(operation);
        }
        let _ = self.preload.invalidate(reason);
        self.preload_prepare = None;
        self.preload_identity = None;
        self.preload_media = None;
        if let Some(operation) = self.preload_accept_operation.take() {
            self.finish_lyric_operation(
                operation,
                &Err(BackendError::new(ErrorKind::Cancelled, "Preload invalidated").into()),
            );
        }
    }

    fn catalog_preload(
        &self,
        record: &TrackRecord,
    ) -> Result<Option<(PreloadAuthority, String, CacheIdentity)>> {
        if record.platform.source != "local" {
            return Ok(None);
        }
        let routes_through_source = self.data.selected.is_some()
            && self.source_caps.as_ref().is_none_or(|manifest| {
                manifest
                    .sources
                    .get("local")
                    .is_some_and(|caps| caps.actions.iter().any(|action| action == "musicUrl"))
            });
        if routes_through_source {
            return Ok(None);
        }
        self.catalog_asset_authority(record)
    }

    fn catalog_asset_authority(
        &self,
        record: &TrackRecord,
    ) -> Result<Option<(PreloadAuthority, String, CacheIdentity)>> {
        if record.platform.source != "local" {
            return Ok(None);
        }
        let track = self.catalog.track(&record.id).context(BackendError::new(
            ErrorKind::Unavailable,
            "Preload catalog asset is unavailable",
        ))?;
        let asset = if let Some(asset_id) = &record.local_asset_id {
            Some(
                self.repository
                    .local_asset(asset_id)?
                    .context("Preload asset permission is missing")?,
            )
        } else {
            self.repository.local_asset_for_track(&record.id)?
        };
        let (asset_id, content_stamp) = if let Some(asset) = asset {
            if let Some(authorization) = self.catalog.download_read_authorization(&record.id) {
                let reader = authorization.open()?;
                ensure!(
                    crate::domain::music::normalize_local_path(&authorization.current_path())
                        == crate::domain::music::normalize_local_path(&asset.path)
                        && self
                            .repository
                            .local_asset_file_identity(&asset.id)?
                            .as_ref()
                            == Some(reader.identity()),
                    BackendError::new(
                        ErrorKind::Unavailable,
                        "Downloaded catalog reader differs from its current physical asset"
                    )
                );
                reader.check()?;
            } else {
                let permission = self.repository.local_asset_authority(&asset.id)?.context(
                    BackendError::new(
                        ErrorKind::Unavailable,
                        "Local asset file grant is unavailable",
                    ),
                )?;
                ensure!(
                    permission.explicit_file_epoch.is_some() || !permission.roots.is_empty(),
                    BackendError::new(
                        ErrorKind::Unavailable,
                        "Local asset file grant is unavailable"
                    )
                );
            }
            ensure!(
                asset.available && asset.track_id == record.id,
                BackendError::new(
                    ErrorKind::Unavailable,
                    "Preload asset permission is unavailable or belongs to another track"
                )
            );
            let content = self
                .repository
                .library_asset_binding(&asset.id)?
                .and_then(|binding| {
                    binding.content_sha256.filter(|_| {
                        binding.hash_size_bytes == Some(asset.size_bytes)
                            && binding.hash_modified_ms == asset.modified_ms
                    })
                });
            let stamp = if let Some(hash) = content {
                serde_json::to_vec(&(
                    "local-content-sha256-v1",
                    asset.id.as_str(),
                    asset.size_bytes,
                    hash,
                ))?
            } else {
                serde_json::to_vec(&(
                    asset.id.as_str(),
                    asset.size_bytes,
                    asset.modified_ms,
                    &asset.fingerprint,
                ))?
            };
            (asset.id, format!("{:x}", Sha256::digest(stamp)))
        } else {
            ensure!(
                track.is_demo,
                BackendError::new(
                    ErrorKind::Unavailable,
                    "Local preload requires an explicitly registered asset"
                )
            );
            let stamp = serde_json::to_vec(&(
                "original-catalog-v1",
                &track.id,
                &track.extension,
                track.duration_seconds,
            ))?;
            (track.id.clone(), format!("{:x}", Sha256::digest(stamp)))
        };
        let identity = CacheIdentity {
            track_id: record.id.clone(),
            platform: "local".into(),
            actual_quality: "local".into(),
            source_epoch: self.source_epoch,
            approval_id: format!("catalog-asset:{asset_id}"),
            media_context_sha256: content_stamp.clone(),
            ..Default::default()
        };
        Ok(Some((
            PreloadAuthority::CatalogAsset {
                asset_id,
                content_stamp,
            },
            track.source_url,
            identity,
        )))
    }

    fn preload_context(&self, candidate: &QueueEntry) -> Result<PreloadContext> {
        let record = self.queue_record(candidate)?;
        let current = self
            .queue
            .current
            .as_ref()
            .context("Current queue entry is unavailable")?;
        if let Some((authority, _, _)) = self.catalog_preload(&record)? {
            return Ok(PreloadContext {
                playback_generation: self.audio.snapshot().generation,
                current_entry_id: current.entry_id.clone(),
                queue_revision: self.queue.revision,
                authority,
                source_epoch: self.source_epoch,
                actual_quality: "local".into(),
                approval_epoch: 0,
                authority_approved: true,
            });
        }
        let source_id = self
            .data
            .selected
            .as_deref()
            .context("Preload requires an approved source")?;
        let source = self
            .data
            .sources
            .iter()
            .find(|source| source.sha256 == source_id)
            .context("Selected source is unavailable")?;
        source.require_approval()?;
        let quality = Self::selected_quality(
            &record,
            self.source_caps
                .as_ref()
                .context("Source manifest is unavailable")?,
            &self.preferred_quality,
            None,
        )?;
        Ok(PreloadContext {
            playback_generation: self.audio.snapshot().generation,
            current_entry_id: current.entry_id.clone(),
            queue_revision: self.queue.revision,
            authority: PreloadAuthority::Source {
                sha256: source.sha256.clone(),
                policy_version: source.approval_policy_version,
            },
            source_epoch: self.source_epoch,
            actual_quality: quality,
            approval_epoch: self.source_approval_epoch,
            authority_approved: true,
        })
    }

    fn complete_preload_resolution(
        &mut self,
        operation: String,
        result: Result<(String, CacheIdentity)>,
    ) {
        let snapshot = self.preload.snapshot();
        if snapshot.operation_id.as_deref() != Some(&operation) {
            return;
        }
        let Some(candidate) = snapshot.candidate else {
            return;
        };
        let Ok(context) = self.preload_context(&candidate) else {
            self.invalidate_preload(Invalidation::Source);
            return;
        };
        match result {
            Ok((url, identity)) if identity.actual_quality == context.actual_quality => {
                match self.preload.resolved(&operation, &context, url) {
                    Ok(true) => self.preload_identity = Some(identity),
                    Err(_) => {
                        let _ = self
                            .preload
                            .fail(&operation, &context, PreloadFailure::Resolution);
                    }
                    Ok(false) => {}
                }
            }
            _ => {
                let _ = self
                    .preload
                    .fail(&operation, &context, PreloadFailure::Resolution);
            }
        }
    }

    fn approve_preload(&mut self, operation: String, url: String) -> Result<()> {
        let candidate = self
            .preload
            .snapshot()
            .candidate
            .context(BackendError::new(
                ErrorKind::Unavailable,
                "Preload candidate is unavailable",
            ))?;
        let context = self.preload_context(&candidate)?;
        let action = self
            .preload
            .approve(&operation, &context, &url)
            .map_err(|error| BackendError::new(ErrorKind::Conflict, error.to_string()))?;
        let setup = (|| -> Result<(CacheIdentity, crate::audio::AudioPrepareTicket)> {
            ensure!(
                self.preload.validate_action(&action, &context),
                BackendError::new(ErrorKind::Conflict, "Preload consent is stale")
            );
            let mut identity = self
                .preload_identity
                .clone()
                .context("Resolved preload context is unavailable")?;
            self.approval_epoch = self.approval_epoch.wrapping_add(1);
            identity.approval_id = self.request_id.clone();
            identity.approval_epoch = self.approval_epoch;
            let request = ApprovedAudioRequest::new(&url, identity)?;
            let identity = request.identity.clone();
            let ticket = self.audio.prepare_approved(
                operation.clone(),
                request,
                context.playback_generation,
                action.cancel.clone(),
            )?;
            Ok((identity, ticket))
        })();
        match setup {
            Ok((identity, ticket)) => {
                self.preload_identity = Some(identity);
                self.preload_prepare = Some((action, ticket));
                self.preload_accept_operation = Some(self.request_id.clone());
                Ok(())
            }
            Err(error) => {
                let _ = self
                    .preload
                    .fail(&operation, &context, PreloadFailure::Preparation);
                self.audio.cancel_preload(&operation);
                self.preload_prepare = None;
                Err(error)
            }
        }
    }

    fn prepare_catalog_preload(
        &mut self,
        action: ResolveAction,
        context: &PreloadContext,
        url: String,
        identity: CacheIdentity,
    ) -> Result<()> {
        ensure!(
            matches!(context.authority, PreloadAuthority::CatalogAsset { .. }),
            "Catalog preparation requires asset authority"
        );
        self.complete_preload_resolution(
            action.operation_id.clone(),
            Ok((url.clone(), identity.clone())),
        );
        let preparation = self.preload.approve(&action.operation_id, context, &url)?;
        let result = (|| -> Result<()> {
            ensure!(
                self.preload.validate_action(&preparation, context),
                "Catalog preload permission became stale"
            );
            let request = ApprovedAudioRequest::new(&url, identity)?;
            self.preload_identity = Some(request.identity.clone());
            let ticket = self.audio.prepare_approved(
                action.operation_id.clone(),
                request,
                context.playback_generation,
                preparation.cancel.clone(),
            )?;
            self.preload_prepare = Some((preparation, ticket));
            Ok(())
        })();
        if result.is_err() {
            let _ = self
                .preload
                .fail(&action.operation_id, context, PreloadFailure::Preparation);
            self.audio.cancel_preload(&action.operation_id);
            self.preload_prepare = None;
        }
        result
    }

    fn poll_preload(&mut self) {
        if self.timeout_stop.blocks_automatic_playback() {
            return;
        }
        if let Some(promotion) = self.pending_promotion.take() {
            let audio = self.audio.snapshot();
            let diagnostic = self
                .audio
                .preload_snapshot(&promotion.operation)
                .filter(|state| {
                    state.nonce == promotion.nonce
                        && state.expected_generation == promotion.expected_generation
                });
            let selected = self
                .queue
                .current
                .as_ref()
                .is_some_and(|entry| entry.entry_id == promotion.entry.entry_id)
                && self.source_epoch == promotion.source_epoch;
            let attached = selected
                && audio.generation > promotion.expected_generation
                && audio.track_id.as_deref() == Some(promotion.record.id.as_str())
                && matches!(
                    audio.status,
                    PlaybackStatus::Playing
                        | PlaybackStatus::Paused
                        | PlaybackStatus::Buffering
                        | PlaybackStatus::Ended
                )
                && diagnostic
                    .as_ref()
                    .is_some_and(|state| state.state == crate::audio::AudioPreloadState::Promoted);
            let rejected = diagnostic.as_ref().is_some_and(|state| {
                matches!(
                    state.state,
                    crate::audio::AudioPreloadState::Failed
                        | crate::audio::AudioPreloadState::Cancelled
                )
            }) || audio.status == PlaybackStatus::Error
                || promotion.started.elapsed() > Duration::from_secs(2);
            if attached {
                self.events.lock().unwrap().publish(
                    self.revision,
                    "backend",
                    Some(promotion.operation.clone()),
                    BackendEvent::PreloadPromoted {
                        operation_id: promotion.operation,
                        entry_id: promotion.entry.entry_id,
                        track_id: promotion.entry.track_id,
                        previous_audio_generation: promotion.expected_generation,
                    },
                );
            } else if selected && rejected {
                let failed = BackendPreloadSnapshot {
                    state: "failed".into(),
                    operation_id: promotion.operation,
                    candidate: promotion.entry,
                    actual_quality: promotion.identity.actual_quality.clone(),
                    pending_url: None,
                    identity: Some(promotion.identity),
                    failure: Some("promotion".into()),
                };
                if let Err(error) =
                    self.play_record(promotion.record, self.play_when_ready, None, true)
                {
                    self.error(format!("{error:#}"));
                }
                self.preload_failure_snapshot = Some(failed);
            } else if selected {
                self.pending_promotion = Some(promotion);
            }
        }
        if let Some((action, mut ticket)) = self.preload_prepare.take() {
            match ticket.try_result() {
                None => {
                    self.preload_prepare = Some((action, ticket));
                }
                Some(result) => {
                    let context = self.preload_context(&action.candidate);
                    let success = match (context, result) {
                        (Ok(context), Ok(token)) => self
                            .preload
                            .ready(&action, &context, token)
                            .unwrap_or(false),
                        (Ok(context), Err(error)) => {
                            if error.requires_url_refresh()
                                && let Some(media) = &self.preload_media
                                && let Err(error) = self.repository.invalidate_resolved_media(
                                    &media.key,
                                    crate::domain::media::MediaInvalidationReason::HttpRejected,
                                )
                            {
                                self.error(format!("{error:#}"));
                            }
                            let _ = self.preload.fail(
                                &action.operation_id,
                                &context,
                                PreloadFailure::Preparation,
                            );
                            false
                        }
                        _ => false,
                    };
                    if let Some(operation) = self.preload_accept_operation.take() {
                        let result = if success {
                            Ok(())
                        } else {
                            Err(BackendError::new(
                                ErrorKind::OperationFailed,
                                "Preload preparation failed or became stale",
                            )
                            .into())
                        };
                        self.finish_lyric_operation(operation, &result);
                    }
                }
            }
        }
        let ready_snapshot = self.preload.snapshot();
        if ready_snapshot.state == PreloadState::Ready
            && let Some(operation) = ready_snapshot.operation_id.as_deref()
            && self.audio.preload_snapshot(operation).is_some_and(|state| {
                matches!(
                    state.state,
                    crate::audio::AudioPreloadState::Failed
                        | crate::audio::AudioPreloadState::Cancelled
                )
            })
            && let (Some(candidate), Some(fence)) = (ready_snapshot.candidate, ready_snapshot.fence)
        {
            if std::env::var_os("LX_DEBUG").is_some() {
                eprintln!(
                    "Preload preparation failed: {:?}",
                    self.audio.preload_snapshot(operation)
                );
            }
            if self.audio.preload_snapshot(operation).is_some_and(|state| {
                state
                    .error
                    .as_ref()
                    .is_some_and(|error| error.requires_url_refresh())
            }) && let Some(media) = &self.preload_media
                && let Err(error) = self.repository.invalidate_resolved_media(
                    &media.key,
                    crate::domain::media::MediaInvalidationReason::HttpRejected,
                )
            {
                self.error(format!("{error:#}"));
            }
            let failed = BackendPreloadSnapshot {
                state: "failed".into(),
                operation_id: operation.into(),
                candidate,
                actual_quality: fence.actual_quality,
                pending_url: None,
                identity: self.preload_identity.clone(),
                failure: Some("preparation".into()),
            };
            self.invalidate_preload(Invalidation::Cache);
            self.preload_failure_snapshot = Some(failed);
        }
        let audio = self.audio.snapshot();
        let snapshot = self.preload.snapshot();
        if let Some(candidate) = snapshot.candidate.as_ref() {
            match self.preload_context(candidate) {
                Ok(context) => {
                    if self
                        .preload
                        .synchronize(&context, Invalidation::Queue)
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        self.invalidate_preload(Invalidation::Queue);
                    }
                }
                Err(_) => {
                    if !matches!(
                        snapshot.state,
                        PreloadState::Cancelled | PreloadState::Failed
                    ) {
                        self.invalidate_preload(Invalidation::Queue);
                    }
                }
            }
        }
        if self.pending_promotion.is_some()
            || self.busy
            || self.pending_foreground.is_some()
            || self.pending_lyric.is_some()
            || self.source_initializing
            || self.source_inflight.is_some()
            || !self.play_when_ready
            || audio.status != PlaybackStatus::Playing
            || !audio.duration_known
            || audio.seek_pending
            || audio.buffering
        {
            return;
        }
        let timing = TriggerTiming {
            duration_seconds: audio.duration_seconds,
            position_seconds: audio.position_seconds,
            since_seek: self.last_seek.elapsed(),
        };
        if !timing.should_preload() {
            return;
        }
        let Some(candidate) = self.queue.preview_next(AdvanceReason::Ended, |entry| {
            Self::eligible_with(
                &self.eligibility,
                &self.dislike,
                &self.failed_entries,
                entry,
            )
        }) else {
            return;
        };
        if self.preload_suppressed.as_ref()
            == Some(&(
                audio.generation,
                self.queue.revision,
                audio.seek_seq,
                candidate.entry_id.clone(),
            ))
        {
            return;
        }
        let Ok(context) = self.preload_context(&candidate) else {
            return;
        };
        if matches!(context.authority, PreloadAuthority::Source { .. }) && self.runtime.is_none() {
            return;
        }
        let action = self
            .preload
            .begin(&mut self.queue, &context, timing, |entry| {
                Self::eligible_with(
                    &self.eligibility,
                    &self.dislike,
                    &self.failed_entries,
                    entry,
                )
            });
        if let Ok(Some(action)) = action {
            self.preload_failure_snapshot = None;
            match self.queue_record(&action.candidate) {
                Ok(record) => {
                    match self.catalog_preload(&record) {
                        Ok(Some((authority, url, identity))) if authority == context.authority => {
                            if self
                                .prepare_catalog_preload(action.clone(), &context, url, identity)
                                .is_err()
                            {
                                let _ = self.preload.fail(
                                    &action.operation_id,
                                    &context,
                                    PreloadFailure::Preparation,
                                );
                            }
                            return;
                        }
                        Err(_) => {
                            let _ = self.preload.fail(
                                &action.operation_id,
                                &context,
                                PreloadFailure::Unavailable,
                            );
                            return;
                        }
                        _ => {}
                    }
                    let job = SourceResolveJob {
                        generation: self.generation,
                        record,
                        strict_quality: Some(context.actual_quality.clone()),
                        preload: Some(action.clone()),
                        force_refresh: false,
                        resolved_track: None,
                        skip_primary: false,
                        excluded_platforms: Vec::new(),
                    };
                    if self.start_source_job(job).is_err() {
                        let _ = self.preload.fail(
                            &action.operation_id,
                            &context,
                            PreloadFailure::Resolution,
                        );
                    }
                }
                Err(_) => {
                    let _ = self.preload.fail(
                        &action.operation_id,
                        &context,
                        PreloadFailure::Unavailable,
                    );
                }
            }
        }
    }

    fn project_preload(&mut self) {
        let state = self.preload.snapshot();
        let projected = state
            .operation_id
            .and_then(|operation_id| {
                let candidate = state.candidate?;
                let fence = state.fence?;
                Some(BackendPreloadSnapshot {
                    state: match state.state {
                        PreloadState::Idle => "idle",
                        PreloadState::Resolving => "resolving",
                        PreloadState::AwaitingApproval => "awaiting_approval",
                        PreloadState::Preparing => "preparing",
                        PreloadState::Ready => "ready",
                        PreloadState::Failed => "failed",
                        PreloadState::Cancelled => "cancelled",
                    }
                    .into(),
                    operation_id,
                    candidate,
                    actual_quality: fence.actual_quality,
                    pending_url: state.pending_url,
                    identity: self.preload_identity.clone(),
                    failure: state
                        .failure
                        .map(|failure| format!("{failure:?}").to_lowercase()),
                })
            })
            .filter(|snapshot| {
                snapshot.state != "cancelled"
                    || !self
                        .preload_failure_snapshot
                        .as_ref()
                        .is_some_and(|failure| failure.operation_id == snapshot.operation_id)
            })
            .or_else(|| {
                self.pending_promotion
                    .as_ref()
                    .map(|promotion| BackendPreloadSnapshot {
                        state: "promoting".into(),
                        operation_id: promotion.operation.clone(),
                        candidate: promotion.entry.clone(),
                        actual_quality: promotion.identity.actual_quality.clone(),
                        pending_url: None,
                        identity: Some(promotion.identity.clone()),
                        failure: None,
                    })
            })
            .or_else(|| self.preload_failure_snapshot.clone());
        let signature = serde_json::to_string(&projected).unwrap_or_default();
        if signature != self.last_preload_signature {
            self.last_preload_signature = signature;
            if let Some(snapshot) = &projected {
                self.events.lock().unwrap().publish(
                    self.revision,
                    "backend",
                    Some(snapshot.operation_id.clone()),
                    BackendEvent::PreloadChanged {
                        operation_id: snapshot.operation_id.clone(),
                        state: snapshot.state.clone(),
                        candidate_entry_id: snapshot.candidate.entry_id.clone(),
                        actual_quality: snapshot.actual_quality.clone(),
                    },
                );
            }
        }
        self.shared.lock().unwrap().preload = projected;
    }

    fn apply_import(&mut self, source: ImportedSource) -> Result<()> {
        ensure!(
            !self
                .data
                .sources
                .iter()
                .any(|item| item.sha256 == source.sha256),
            "此脚本已经导入"
        );
        let name = source.metadata.name.clone();
        let id = source.sha256.clone();
        let mut candidate = self.data.clone();
        candidate.sources.push(source);
        self.commit_data(candidate)?;
        self.statuses.insert(id, "已校验 · 等待授权".into());
        self.notice(format!("已导入 {name}，脚本尚未执行；查看后授权启用。"));
        Ok(())
    }

    fn play(&mut self, id: &str) -> Result<()> {
        self.play_list_track("catalog".into(), id.into(), None)
    }

    fn play_record(
        &mut self,
        record: TrackRecord,
        desired_play: bool,
        strict_quality: Option<String>,
        automatic: bool,
    ) -> Result<()> {
        self.require_effect_resource()?;
        let local = self.catalog.track(&record.id);
        let id = record.id.clone();
        self.pending_promotion = None;
        self.invalidate_preload(Invalidation::Playback);
        self.cancel_lyrics(true);
        self.cancel_playback_resolution();
        self.generation = self.generation.wrapping_add(1);
        self.skip_pending = None;
        self.audio.stop();
        self.play_when_ready = desired_play;
        self.current_record = Some(Arc::new(record.clone()));
        self.reset_media_recovery();
        self.playback_requested_quality = strict_quality.clone();
        self.media_refresh_attempts = 0;
        self.active_media = None;
        self.restore_pending = false;
        if self.restored_position_ms > 0 {
            self.seek_after_load = Some((
                record.id.clone(),
                self.restored_position_ms,
                self.audio.snapshot().generation,
            ));
        }
        self.restored_position_ms = 0;
        self.lyric_start_after_generation = self.audio.snapshot().generation;
        self.pending = None;
        {
            let mut snapshot = self.shared.lock().unwrap();
            snapshot.pending_playback_url = None;
            snapshot.approved_audio_identity = None;
            snapshot.resolved_track = None;
            snapshot.error = None;
        }
        let native_download = self.catalog.has_download_read_binding(&record.id);
        let use_source = !native_download
            && (record.platform.source != "local"
                || self.source_caps.as_ref().is_none_or(|manifest| {
                    manifest
                        .sources
                        .get("local")
                        .is_some_and(|caps| caps.actions.iter().any(|action| action == "musicUrl"))
                }));
        if let Some(source_id) = self.data.selected.clone().filter(|_| use_source) {
            self.data
                .sources
                .iter()
                .find(|source| source.sha256 == source_id)
                .context("Current source is unavailable")?
                .require_approval()?;
            self.pending_foreground = Some(SourceResolveJob {
                generation: self.generation,
                record: record.clone(),
                strict_quality,
                preload: None,
                force_refresh: false,
                resolved_track: None,
                skip_primary: false,
                excluded_platforms: Vec::new(),
            });
            self.busy = true;
            if let Err(error) = self.start_pending_foreground() {
                self.busy = false;
                return Err(error);
            }
            self.mark_source_async_started();
            self.notice("正在通过已授权的音源解析播放地址…");
        } else {
            ensure!(
                !self.busy || native_download,
                "音源正在初始化，请稍候或停用音源"
            );
            ensure!(
                record.platform.source == "local" && local.is_some(),
                BackendError::new(
                    ErrorKind::Unavailable,
                    "Online playback requires an approved source for the track platform"
                )
            );
            let (_, url, identity) = self
                .catalog_preload(&record)?
                .context("Catalog asset permission is unavailable")?;
            if automatic {
                self.poll_timeout_stop();
            }
            let autoplay =
                desired_play && (!automatic || !self.timeout_stop.blocks_automatic_playback());
            self.play_when_ready = autoplay;
            self.load_approved(&url, identity, autoplay)?;
        }
        self.intended_track = Some(id);
        if let Some(entry) = self.queue.current.as_ref() {
            self.events.lock().unwrap().publish(
                self.revision,
                &self.origin,
                Some(self.request_id.clone()),
                BackendEvent::PlaybackSelectionChanged {
                    entry_id: entry.entry_id.clone(),
                    list_id: entry.list_id.clone(),
                    track_id: entry.track_id.clone(),
                },
            );
        }
        Ok(())
    }

    fn next(&mut self, offset: isize) -> Result<()> {
        self.failed_entries.clear();
        self.failure_attempts = 0;
        self.skip_pending = None;
        self.advance_queue(AdvanceReason::Manual, offset < 0)
    }

    fn advance_queue(&mut self, reason: AdvanceReason, previous: bool) -> Result<()> {
        if reason != AdvanceReason::Manual {
            self.poll_timeout_stop();
            if self.timeout_stop.blocks_automatic_playback() {
                return Ok(());
            }
        }
        let preload_snapshot = self.preload.snapshot();
        let promotion_context = if reason == AdvanceReason::Ended
            && !previous
            && preload_snapshot.state == PreloadState::Ready
            && self.audio.snapshot().status == PlaybackStatus::Ended
        {
            preload_snapshot
                .candidate
                .as_ref()
                .and_then(|candidate| self.preload_context(candidate).ok())
        } else {
            None
        };
        let mut candidate = self.queue.clone();
        let entry = if previous {
            candidate.previous(|entry| {
                Self::eligible_with(
                    &self.eligibility,
                    &self.dislike,
                    &self.failed_entries,
                    entry,
                )
            })
        } else {
            candidate.advance(reason, |entry| {
                Self::eligible_with(
                    &self.eligibility,
                    &self.dislike,
                    &self.failed_entries,
                    entry,
                )
            })
        };
        if let Some(entry) = entry {
            let record = self.queue_record(&entry)?;
            if reason != AdvanceReason::Manual {
                self.poll_timeout_stop();
                if self.timeout_stop.blocks_automatic_playback() {
                    return Ok(());
                }
            }
            self.commit_queue(candidate)?;
            if reason != AdvanceReason::Manual {
                self.poll_timeout_stop();
                if self.timeout_stop.blocks_automatic_playback() {
                    self.current_record = Some(Arc::new(record));
                    self.restore_pending = true;
                    self.restored_position_ms = 0;
                    self.seek_after_load = None;
                    let checkpoint = self.checkpoint(self.queue.clone(), 0);
                    self.revision = self
                        .repository
                        .save_playback_checkpoint(
                            &checkpoint,
                            Some(self.revision),
                            self.mutation_origin(),
                        )?
                        .revision;
                    return Ok(());
                }
            }
            self.restore_pending = false;
            self.restored_position_ms = 0;
            if let (Some(context), Some(operation)) =
                (promotion_context, preload_snapshot.operation_id)
                && preload_snapshot
                    .candidate
                    .as_ref()
                    .is_some_and(|candidate| candidate.entry_id == entry.entry_id)
                && let Ok(lease) = self.preload.promote(&operation, &context, &entry.entry_id)
            {
                let (token, promotion_cancel) = lease.into_parts();
                let nonce = token.nonce();
                let identity = token.identity().clone();
                self.preload_identity = None;
                self.active_media = self.preload_media.take();
                self.media_refresh_attempts = 0;
                self.preload_prepare = None;
                self.cancel_lyrics(true);
                self.generation = self.generation.wrapping_add(1);
                self.current_record = Some(Arc::new(record.clone()));
                self.reset_media_recovery();
                self.playback_requested_quality = None;
                self.intended_track = Some(record.id.clone());
                self.lyric_start_after_generation = context.playback_generation;
                self.busy = false;
                self.play_when_ready = true;
                self.seek_after_load = None;
                self.last_seek = Instant::now();
                if self
                    .audio
                    .promote_prepared(token, context.playback_generation, true)
                    .is_ok()
                {
                    let mut snapshot = self.shared.lock().unwrap();
                    snapshot.approved_audio_identity = Some(identity.clone());
                    let approved_url = self
                        .active_media
                        .as_ref()
                        .map(|media| media.url.as_str().to_owned())
                        .or_else(|| {
                            self.catalog_asset_authority(&record)
                                .ok()
                                .flatten()
                                .map(|(_, url, _)| url)
                        });
                    self.approved_request = approved_url
                        .and_then(|url| ApprovedAudioRequest::new(&url, identity.clone()).ok());
                    snapshot.resolved_track = self
                        .active_media
                        .as_ref()
                        .and_then(|media| media.matched_track.as_ref())
                        .map(TrackSummary::from);
                    snapshot.pending_playback_url = None;
                    snapshot.error = None;
                    drop(snapshot);
                    self.events.lock().unwrap().publish(
                        self.revision,
                        "backend",
                        Some(operation.clone()),
                        BackendEvent::PlaybackSelectionChanged {
                            entry_id: entry.entry_id.clone(),
                            list_id: entry.list_id.clone(),
                            track_id: entry.track_id.clone(),
                        },
                    );
                    self.pending_promotion = Some(PendingPromotion {
                        cancel: promotion_cancel,
                        operation,
                        entry,
                        record,
                        expected_generation: context.playback_generation,
                        nonce,
                        source_epoch: self.source_epoch,
                        identity,
                        started: Instant::now(),
                    });
                    return Ok(());
                }
            }
            self.play_record(record, true, None, reason != AdvanceReason::Manual)
        } else {
            self.skip_pending = None;
            self.play_when_ready = false;
            self.pending_promotion = None;
            self.invalidate_preload(Invalidation::Playback);
            self.cancel_playback_resolution();
            self.cancel_lyrics(true);
            self.generation = self.generation.wrapping_add(1);
            self.audio.stop();
            self.save_checkpoint()?;
            Ok(())
        }
    }

    fn handle(&mut self, command: BackendCommand) -> Result<ReceiptState> {
        self.poll_timeout_stop();
        let manual_playback = matches!(
            &command,
            BackendCommand::Play { .. }
                | BackendCommand::PlayListTrack { .. }
                | BackendCommand::Resume
                | BackendCommand::Toggle
                | BackendCommand::Previous
                | BackendCommand::Next
        );
        let mut state = ReceiptState::Applied;
        if manual_playback {
            self.require_effect_resource()?;
        }
        let invalidation = match &command {
            BackendCommand::ListAction { .. } | BackendCommand::SaveProviderTracks { .. } => {
                Some(Invalidation::List)
            }
            BackendCommand::DislikeAction { .. } | BackendCommand::DislikeCurrent => {
                Some(Invalidation::Dislike)
            }
            BackendCommand::SelectList { .. } | BackendCommand::RemoveQueueEntry { .. } => {
                Some(Invalidation::Queue)
            }
            BackendCommand::Enqueue { .. } | BackendCommand::ClearTemporaryQueue => {
                Some(Invalidation::TemporaryQueue)
            }
            BackendCommand::SetPlayMode { .. } => Some(Invalidation::Mode),
            BackendCommand::SetPlaybackQuality { .. } => Some(Invalidation::Quality),
            BackendCommand::ApproveSource { .. }
            | BackendCommand::EnableSource { .. }
            | BackendCommand::DeleteSource { .. }
            | BackendCommand::DisableSource => Some(Invalidation::SourceRevoked),
            BackendCommand::Seek { .. } => Some(Invalidation::Seek),
            BackendCommand::Stop => Some(Invalidation::Stop),
            BackendCommand::ClearAudioCache => Some(Invalidation::Cache),
            _ => None,
        };
        if let Some(reason) = invalidation {
            if matches!(
                reason,
                Invalidation::SourceRevoked
                    | Invalidation::Source
                    | Invalidation::Cache
                    | Invalidation::Stop
                    | Invalidation::Playback
                    | Invalidation::List
                    | Invalidation::Queue
            ) {
                self.invalidate_system_artwork()?;
            }
            self.invalidate_preload(reason);
        }
        match command {
            BackendCommand::ReadLocalTags {
                asset_id,
                tag_operation_id,
            } => {
                self.start_tag_read(asset_id, tag_operation_id)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ApproveTagWrite {
                approval_id,
                selection,
            } => {
                self.start_tag_approval(approval_id, selection, None)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ApproveTagRecovery {
                approval_id,
                tag_operation_id,
                selection,
            } => {
                self.start_tag_approval(approval_id, selection, Some(tag_operation_id))?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RevokeTagWrite {
                approval_id,
                expected_approval_epoch,
            } => {
                self.start_tag_revoke(approval_id, expected_approval_epoch)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::EditLocalTags {
                approval_id,
                expected_approval_epoch,
                budget,
                patch,
            } => {
                self.start_tag_install(approval_id, expected_approval_epoch, budget, patch)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RecoverTagEdit {
                tag_operation_id,
                action,
                approval_id,
                expected_approval_epoch,
            } => {
                self.start_tag_recovery(
                    tag_operation_id,
                    action,
                    approval_id,
                    expected_approval_epoch,
                )?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CancelTagEdit { tag_operation_id } => {
                self.tag_edits.cancel(&tag_operation_id);
            }
            BackendCommand::SetSystemControlSettings { settings } => {
                state = self.set_system_control_settings(settings)?
            }
            BackendCommand::RetrySystemControl => state = self.retry_system_controls()?,
            BackendCommand::StartTimeoutStop {
                seconds,
                wait_for_track_end,
            } => {
                let wait = wait_for_track_end.unwrap_or(self.data.timeout_stop.wait_for_track_end);
                let mut timer = self.timeout_stop.clone();
                timer.arm(seconds, wait, Instant::now())?;
                let mut data = self.data.clone();
                data.timeout_stop = crate::playback::timeout_stop::TimeoutStopPreferences {
                    wait_for_track_end: wait,
                    last_seconds: Some(seconds),
                };
                self.commit_data(data)?;
                self.timeout_stop = timer;
                self.publish_timeout_stop();
                state = ReceiptState::Committed;
            }
            BackendCommand::SetTimeoutStopPreferences { settings } => {
                settings.validate()?;
                let mut data = self.data.clone();
                data.timeout_stop = settings;
                self.commit_data(data)?;
                self.timeout_stop
                    .configure_wait(settings.wait_for_track_end);
                self.publish_timeout_stop();
                state = ReceiptState::Committed;
            }
            BackendCommand::CancelTimeoutStop => {
                self.timeout_stop.cancel()?;
                self.publish_timeout_stop();
            }
            BackendCommand::DispatchDeepLink { url } => {
                match crate::open_api::parse_deeplink(&url)?.action {
                    crate::open_api::DeepLinkAction::Player(action) => {
                        if let Some(command) = self.control_action_command(action)? {
                            return self.handle(command);
                        }
                    }
                    crate::open_api::DeepLinkAction::MusicPlay { music } => {
                        let record = TrackRecord::from_lx_dto(
                            music.wire,
                            crate::domain::music::LxMusicDtoVersion::Legacy,
                        )?;
                        let music_id = record.id.clone();
                        self.handle(BackendCommand::ListAction {
                            action: Box::new(ListAction::MusicAdd {
                                list_id: crate::domain::lists::TEMP_LIST.into(),
                                tracks: vec![record],
                                location: AddLocation::Top,
                            }),
                        })?;
                        self.handle(BackendCommand::Enqueue {
                            items: vec![QueueItemRef {
                                list_id: crate::domain::lists::TEMP_LIST.into(),
                                track_id: music_id,
                            }],
                            front: true,
                        })?;
                        return self.handle(BackendCommand::Next);
                    }
                    action => {
                        self.begin_deeplink_provider(action)?;
                        state = ReceiptState::Accepted;
                    }
                }
            }
            BackendCommand::PreviewLxBackup {
                path,
                replace_tracks,
                copy_list_id,
            } => {
                self.begin_backup_preview(path, replace_tracks, copy_list_id)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ExportPhysicalSnapshot {
                directory,
                basename,
                maximum_image_bytes,
            } => {
                self.begin_physical_snapshot(directory, basename, maximum_image_bytes)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RecoverPhysicalSnapshot {
                snapshot_operation_id,
                directory,
                action,
            } => {
                self.recover_physical_snapshot_operation(snapshot_operation_id, directory, action)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ExportBackup {
                directory,
                basename,
                format,
                encoding,
                include_sources,
                include_downloads,
                include_edited_lyrics,
            } => {
                self.begin_backup_export(
                    directory,
                    basename,
                    format,
                    encoding,
                    include_sources,
                    include_downloads,
                    include_edited_lyrics,
                )?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RetryBackupExport {
                export_operation_id,
            } => {
                self.retry_backup_export(export_operation_id, None)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CleanupBackupExport {
                export_operation_id,
                directory,
            } => {
                self.retry_backup_export(export_operation_id, Some(directory))?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CommitLxBackup {
                preview_operation_id,
            } => {
                self.commit_backup_preview(&preview_operation_id)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::PreviewNativeBackup {
                path,
                replace_tracks,
            } => {
                self.begin_native_backup_preview(path, replace_tracks)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CommitNativeBackup {
                preview_operation_id,
                input_digest,
            } => {
                self.commit_native_backup_preview(&preview_operation_id, &input_digest)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::AddDownloadDestination { path } => {
                self.select_download_destination(path)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RevokeDownloadDestination {
                grant_id,
                expected_grant_revision,
            } => {
                self.revoke_download_destination(grant_id, expected_grant_revision)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetDownloadSettings { settings } => {
                self.revision = self
                    .repository
                    .save_download_settings(&settings, Some(self.revision))?
                    .revision;
                state = ReceiptState::Committed;
            }
            BackendCommand::AddDownloadTasks {
                list_id,
                music_ids,
                quality,
                top,
            } => {
                self.add_download_tasks(list_id, music_ids, quality, top)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::RemoveDownloadTask {
                task_id,
                expected_task_revision,
                operation_epoch,
            } => {
                self.downloads.require_no_cleanup(&task_id)?;
                self.revision = self
                    .repository
                    .remove_download_task(
                        &task_id,
                        expected_task_revision,
                        operation_epoch,
                        Some(self.revision),
                    )?
                    .revision;
                self.forget_cleanup_status(&task_id);
                state = ReceiptState::Committed;
            }
            BackendCommand::ResumeDownloadTask {
                task_id,
                expected_task_revision,
            } => {
                self.resume_download(task_id, expected_task_revision)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::RetryDownloadPostprocessing { task_id } => {
                self.retry_download_postprocessing(task_id)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::PlayDownloadTask { task_id } => {
                state = if self.play_download_task(task_id)? {
                    ReceiptState::Accepted
                } else {
                    ReceiptState::Applied
                };
            }
            BackendCommand::PauseDownloadTask {
                task_id,
                expected_task_revision,
            } => {
                self.pause_download(task_id, expected_task_revision)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::RecoverDownloadCommit {
                task_id,
                expected_task_revision,
            } => {
                self.recover_download_commit(task_id, expected_task_revision)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RecoverDownloadCleanup {
                task_id,
                expected_task_revision,
            } => {
                self.recover_download_cleanup(task_id, expected_task_revision)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CancelDownloadCleanup {
                task_id,
                expected_task_revision,
            } => self.cancel_download_cleanup(task_id, expected_task_revision)?,
            BackendCommand::ApproveDownloadUrl {
                task_id,
                expected_task_revision,
                operation_epoch,
                url,
            } => {
                self.approve_download_url(task_id, expected_task_revision, operation_epoch, url)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RelinkLocalAsset {
                asset_id,
                path,
                expected_binding_revision,
                allow_content_change,
            } => {
                self.begin_library_relink(
                    asset_id,
                    path,
                    expected_binding_revision,
                    allow_content_change,
                    None,
                )?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ConfirmLibraryRelink {
                candidate_id,
                expected_binding_revision,
                allow_content_change,
            } => {
                self.confirm_library_relink(
                    &candidate_id,
                    expected_binding_revision,
                    allow_content_change,
                )?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::SetLibraryWatch {
                root_id,
                enabled,
                options,
            } => {
                self.set_library_watch(root_id, enabled, options)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RebindLibraryRoot {
                root_id,
                path,
                expected_grant_epoch,
                expected_location_revision,
            } => {
                self.rebind_library_root(
                    root_id,
                    path,
                    expected_grant_epoch,
                    expected_location_revision,
                )?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::AddLibraryRoot { path, recursive } => {
                self.add_library_root(path, recursive)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RemoveLibraryRoot { root_id } => {
                self.remove_library_root(&root_id)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::ScanLibraryRoot { root_id, options } => {
                self.scan_library_root(root_id, options)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ResolvePicture { list_id, music_id } => {
                self.begin_picture(list_id, music_id)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ApprovePictureUrl { operation_id, url } => {
                self.approve_picture(&operation_id, &url)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CancelPicture { operation_id } => {
                self.cancel_picture(&operation_id)?;
            }
            BackendCommand::ClearImageCache => {
                self.clear_pictures()?;
            }
            BackendCommand::SetAutoSwitchSource { enabled } => {
                let previous = std::mem::replace(&mut self.auto_switch_source, enabled);
                if let Err(error) = self.save_checkpoint() {
                    self.auto_switch_source = previous;
                    return Err(error);
                }
                state = ReceiptState::Committed;
            }
            BackendCommand::FindMusicMatches { list_id, music_id } => {
                self.begin_matches(list_id, music_id)?;
                state = if self.file_cancels.contains_key(&self.request_id) {
                    ReceiptState::Accepted
                } else {
                    ReceiptState::Applied
                };
            }
            BackendCommand::RefreshPlaybackUrl => self.refresh_media_url(false)?,
            BackendCommand::ClearMediaCache => {
                self.repository.clear_media_cache()?;
                self.media_cache_epoch = self.media_cache_epoch.wrapping_add(1);
                self.invalidate_preload(Invalidation::Cache);
                self.cancel_playback_resolution();
                self.generation = self.generation.wrapping_add(1);
                self.pending = None;
                self.shared.lock().unwrap().pending_playback_url = None;
            }
            BackendCommand::ListAction { action } => {
                self.apply_lists(&action)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::DislikeAction { action } => {
                self.apply_dislike(&action)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SelectList { list_id } => {
                self.select_list(list_id)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::PlayListTrack {
                list_id,
                music_id,
                quality,
            } => {
                self.play_list_track(list_id, music_id, quality)?;
                state = if self.async_started {
                    ReceiptState::Accepted
                } else {
                    ReceiptState::Committed
                };
            }
            BackendCommand::SaveProviderTracks {
                operation_id,
                list_id,
                music_ids,
                location,
            } => {
                let cached = self
                    .provider_results
                    .iter()
                    .find(|(result, _)| result.operation_id == operation_id)
                    .map(|(result, _)| result)
                    .context("Provider result is unknown or evicted")?;
                ensure!(
                    cached.state == ProviderOperationState::Completed,
                    "Provider result is not completed"
                );
                let records = match cached.response.as_deref() {
                    Some(ProviderResponse::Tracks(page)) => &page.items,
                    Some(ProviderResponse::Collection(collection)) => &collection.tracks.items,
                    _ => anyhow::bail!("Provider result does not contain tracks"),
                };
                let chosen = records
                    .iter()
                    .filter(|track| {
                        music_ids.is_empty()
                            || music_ids
                                .iter()
                                .any(|id| id == &track.id || id == &track.lx_id)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                ensure!(!chosen.is_empty(), "No selected provider tracks");
                self.apply_lists(&ListAction::MusicAdd {
                    list_id,
                    tracks: chosen,
                    location,
                })?;
                state = ReceiptState::Committed;
            }
            BackendCommand::Enqueue { items, front } => {
                self.enqueue_refs(items, front)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::RemoveQueueEntry { entry_id } => {
                if let Some(entry) = self
                    .queue
                    .entries
                    .iter()
                    .find(|entry| entry.entry_id == entry_id)
                    .cloned()
                {
                    ensure!(
                        entry.list_id != "catalog",
                        BackendError::new(
                            ErrorKind::InvalidData,
                            "Virtual catalog entries are read-only; save tracks to a playlist to remove membership"
                        )
                    );
                    self.apply_lists(&ListAction::MusicRemove {
                        list_id: entry.list_id,
                        ids: vec![entry.track_id],
                    })?;
                } else {
                    let mut candidate = self.queue.clone();
                    ensure!(candidate.remove(&entry_id), "Queue entry does not exist");
                    self.commit_queue(candidate)?;
                }
                self.skip_pending = None;
                state = ReceiptState::Committed;
            }
            BackendCommand::ClearTemporaryQueue => {
                let mut candidate = self.queue.clone();
                candidate.clear_temporary();
                self.commit_queue(candidate)?;
                self.skip_pending = None;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetPlayMode { mode } => {
                let mut candidate = self.queue.clone();
                candidate.set_mode(mode);
                self.commit_queue(candidate)?;
                self.failed_entries.clear();
                self.skip_pending = None;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetPlaybackQuality { quality } => {
                ensure!(
                    matches!(quality.as_str(), "128k" | "320k" | "flac" | "flac24bit"),
                    "Unsupported playback quality"
                );
                let old = std::mem::replace(&mut self.preferred_quality, quality);
                if let Err(error) = self.save_checkpoint() {
                    self.preferred_quality = old;
                    return Err(error);
                }
                state = ReceiptState::Committed;
            }
            BackendCommand::SetAutoSkipOnError { enabled } => {
                let old = std::mem::replace(&mut self.auto_skip_on_error, enabled);
                if let Err(error) = self.save_checkpoint() {
                    self.auto_skip_on_error = old;
                    return Err(error);
                }
                if !enabled {
                    self.skip_pending = None;
                }
                state = ReceiptState::Committed;
            }
            BackendCommand::DislikeCurrent => {
                let record = self
                    .current_record
                    .as_ref()
                    .context("There is no current track")?;
                let info = DislikeMusicInfo {
                    name: record.title.clone(),
                    singer: record.artists.join("、"),
                };
                self.apply_dislike(&DislikeAction::Add { infos: vec![info] })?;
                self.advance_queue(AdvanceReason::Ended, false)?;
                state = if self.async_started {
                    ReceiptState::Accepted
                } else {
                    ReceiptState::Committed
                };
            }
            BackendCommand::SetListPosition { list_id, position } => {
                self.revision = self
                    .repository
                    .set_list_position(&list_id, position, Some(self.revision))?
                    .revision;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetListUpdateInfo { list_id, info } => {
                self.revision = self
                    .repository
                    .set_list_update_info(&list_id, &info, Some(self.revision))?
                    .revision;
                state = ReceiptState::Committed;
            }
            BackendCommand::RecordSearch { keyword } => {
                if let Some(change) = self
                    .repository
                    .record_search(&keyword, Some(self.revision))?
                {
                    self.revision = change.revision;
                }
                state = ReceiptState::Committed;
            }
            BackendCommand::RemoveSearchHistory { keyword } => {
                self.revision = self
                    .repository
                    .remove_search_history(&keyword, Some(self.revision))?
                    .revision;
                state = ReceiptState::Committed;
            }
            BackendCommand::ClearSearchHistory => {
                self.revision = self
                    .repository
                    .clear_search_history(Some(self.revision))?
                    .revision;
                state = ReceiptState::Committed;
            }
            BackendCommand::ProviderRequest { platform, request } => {
                self.begin_provider(platform, *request)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ImportSource { path } => {
                self.begin_import_file(path)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ImportSourceText { script } => {
                self.begin_import(script)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::AddDemoSource => {
                self.begin_import(
                    include_str!("../../fixtures/legal-http-utils-source.js").into(),
                )?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ImportAudio { paths } => {
                ensure!(
                    !paths.is_empty() && paths.len() <= 1000,
                    "Choose 1 to 1000 audio files"
                );
                ensure!(
                    self.tasks.len() + self.tag_edits.job_count() < 4,
                    "Too many pending operations"
                );
                let catalog = self.catalog.clone();
                let authority = self.library.authority.clone();
                let tx = self.completed_tx.clone();
                let operation = self.request_id.clone();
                let cancelled = Arc::new(AtomicBool::new(false));
                self.file_cancels
                    .insert(operation.clone(), cancelled.clone());
                self.tasks.push(thread::spawn(move || {
                    let mut guards = Vec::new();
                    let result =
                        (|| -> Result<Vec<(PreparedLocalImport, TrackRecord, LocalAsset)>> {
                            let mut items = Vec::new();
                            let mut held_bytes = 0usize;
                            for path in paths {
                                ensure!(
                                    !cancelled.load(Ordering::Acquire),
                                    crate::domain::library::LibraryError::new(
                                        crate::domain::library::LibraryErrorKind::Cancelled,
                                        "Audio import cancelled"
                                    )
                                );
                                let selected = authority.open_selection(&path)?;
                                let file = selected
                                    .authorization()
                                    .with_cancel(cancelled.clone())?
                                    .open()?;
                                let prepared = catalog.prepare_authorized(file.try_clone()?)?;
                                let native = prepared.library_asset();
                                let encoded = serde_json::to_vec(&native)?.len();
                                held_bytes = held_bytes
                                    .checked_add(prepared.owned_metadata_bytes())
                                    .and_then(|bytes| bytes.checked_add(encoded.saturating_mul(2)))
                                    .context("Audio import preparation size overflow")?;
                                ensure!(
                                    held_bytes <= 32 * 1024 * 1024,
                                    crate::domain::library::LibraryError::budget(
                                        "Audio import preparation exceeds 32 MiB"
                                    )
                                );
                                guards.push(file);
                                items.push((prepared, native.track, native.asset));
                            }
                            Ok(items)
                        })();
                    let (ack, waiting) = mpsc::sync_channel(1);
                    if tx
                        .send(Completion::LocalPrepared {
                            operation,
                            result,
                            ack,
                        })
                        .is_ok()
                    {
                        loop {
                            match waiting.recv_timeout(Duration::from_millis(10)) {
                                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Err(mpsc::RecvTimeoutError::Timeout)
                                    if cancelled.load(Ordering::Acquire) =>
                                {
                                    break;
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                            }
                        }
                    }
                    drop(guards);
                }));
                state = ReceiptState::Accepted;
            }
            BackendCommand::ApproveSource { id } => {
                self.repository.invalidate_media_for_source(
                    &id,
                    crate::domain::media::MediaInvalidationReason::SourceChanged,
                )?;
                let mut candidate = self.data.clone();
                candidate
                    .sources
                    .iter_mut()
                    .find(|source| source.sha256 == id)
                    .context("音源不存在")?
                    .approve()?;
                self.commit_data(candidate)?;
                self.approval_epoch = self.approval_epoch.wrapping_add(1);
                self.source_approval_epoch = self.source_approval_epoch.wrapping_add(1);
                self.enable(&id)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::EnableSource { id } => {
                self.enable(&id)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::DisableSource => {
                if let Some(id) = &self.data.selected {
                    self.repository.invalidate_media_for_source(
                        id,
                        crate::domain::media::MediaInvalidationReason::SourceChanged,
                    )?;
                }
                let mut candidate = self.data.clone();
                candidate.selected = None;
                self.commit_data(candidate)?;
                self.cancel_runtime();
                self.audio.stop();
                self.notice("音源已停用；曲库可直接播放本地音乐");
                state = ReceiptState::Committed;
            }
            BackendCommand::DeleteSource { id } => {
                self.repository.invalidate_media_for_source(
                    &id,
                    crate::domain::media::MediaInvalidationReason::SourceChanged,
                )?;
                ensure!(
                    self.data.sources.iter().any(|source| source.sha256 == id),
                    "音源不存在"
                );
                let active = self.data.selected.as_deref() == Some(&id)
                    || self
                        .statuses
                        .get(&id)
                        .is_some_and(|status| status == "正在初始化…");
                let mut candidate = self.data.clone();
                candidate.sources.retain(|source| source.sha256 != id);
                if active {
                    candidate.selected = None;
                }
                self.commit_data(candidate)?;
                if active {
                    self.cancel_runtime();
                    self.audio.stop();
                }
                self.statuses.remove(&id);
                self.notice("音源已删除");
                state = ReceiptState::Committed;
            }
            BackendCommand::Play { id } => {
                self.play(&id)?;
                if self.async_started {
                    state = ReceiptState::Accepted;
                }
            }
            BackendCommand::Pause => {
                self.media_retry_pending = None;
                self.skip_pending = None;
                self.play_when_ready = false;
                self.audio.pause();
                self.save_checkpoint()?;
                state = ReceiptState::Committed;
            }
            BackendCommand::Resume => {
                self.play_when_ready = true;
                if self.restore_pending
                    || (!self.busy
                        && self.pending.is_none()
                        && self.audio.snapshot().track_id.is_none()
                        && self.current_record.is_some())
                {
                    let record = self.current_record.as_ref().unwrap().as_ref().clone();
                    self.play_record(record, true, None, false)?;
                    if self.async_started {
                        state = ReceiptState::Accepted;
                    }
                } else {
                    self.audio.play();
                }
            }
            BackendCommand::Toggle => {
                self.skip_pending = None;
                if self.restore_pending {
                    let record = self
                        .current_record
                        .as_ref()
                        .context("Restored track is missing")?
                        .as_ref()
                        .clone();
                    self.play_record(record, true, None, false)?;
                    if self.async_started {
                        state = ReceiptState::Accepted;
                    }
                    self.release_timeout_stop_latch();
                    return Ok(state);
                }
                let playback = self.audio.snapshot();
                if self.busy || self.pending.is_some() {
                    self.play_when_ready = !self.play_when_ready;
                    if self.play_when_ready {
                        self.audio.play();
                    } else {
                        self.audio.pause();
                    }
                } else if playback.track_id.is_some() {
                    self.play_when_ready = match playback.status {
                        PlaybackStatus::Playing => false,
                        PlaybackStatus::Loading | PlaybackStatus::Buffering => {
                            !self.play_when_ready
                        }
                        _ => true,
                    };
                    self.audio.toggle();
                } else if let Some(track) = self.catalog.tracks().first() {
                    self.play(&track.id)?;
                }
            }
            BackendCommand::Previous => self.next(-1)?,
            BackendCommand::Next => self.next(1)?,
            BackendCommand::Stop => {
                self.reset_media_recovery();
                self.pending_promotion = None;
                self.skip_pending = None;
                self.play_when_ready = false;
                self.cancel_playback_resolution();
                self.cancel_lyrics(true);
                self.generation = self.generation.wrapping_add(1);
                self.pending = None;
                {
                    let mut snapshot = self.shared.lock().unwrap();
                    snapshot.pending_playback_url = None;
                    snapshot.approved_audio_identity = None;
                }
                self.audio.stop();
                self.restore_pending = false;
                self.restored_position_ms = 0;
                self.seek_after_load = None;
                let checkpoint = self.checkpoint(self.queue.clone(), 0);
                self.revision = self
                    .repository
                    .save_playback_checkpoint(
                        &checkpoint,
                        Some(self.revision),
                        self.mutation_origin(),
                    )?
                    .revision;
                state = ReceiptState::Committed;
            }
            BackendCommand::Seek { seconds } => {
                self.last_seek = Instant::now();
                ensure!(
                    seconds.is_finite() && (0.0..=30.0 * 24.0 * 60.0 * 60.0).contains(&seconds),
                    "Seek must be finite and nonnegative"
                );
                self.skip_pending = None;
                if self.restore_pending {
                    self.restored_position_ms = (seconds * 1000.0) as u64;
                } else {
                    self.audio.seek(seconds);
                }
                let checkpoint = self.checkpoint(self.queue.clone(), (seconds * 1000.0) as u64);
                self.revision = self
                    .repository
                    .save_playback_checkpoint(
                        &checkpoint,
                        Some(self.revision),
                        self.mutation_origin(),
                    )?
                    .revision;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetAudioOutputDevice { selection } => {
                selection.validate().map_err(|error| {
                    BackendError::new(ErrorKind::InvalidData, error.to_string())
                })?;
                let mut candidate = self.data.clone();
                candidate.audio_preferences.selection = selection;
                self.commit_data(candidate)?;
                self.audio
                    .set_output_preferences(self.data.audio_preferences.clone())?;
                let _ = self.audio.refresh_audio_devices();
                state = ReceiptState::Committed;
            }
            BackendCommand::SetPlaybackProcessing { settings } => {
                self.commit_processing(settings)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetPlaybackRate { rate } => {
                let mut settings = self.data.playback_processing;
                settings.playback_rate = rate;
                self.commit_processing(settings)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetPreservesPitch { enabled } => {
                let mut settings = self.data.playback_processing;
                settings.preserves_pitch = enabled;
                self.commit_processing(settings)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetPitchRatio { ratio } => {
                let mut settings = self.data.playback_processing;
                settings.pitch_ratio = ratio;
                self.commit_processing(settings)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::ResetPlaybackProcessing => {
                self.commit_processing(Default::default())?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetEqualizer { settings } => {
                self.commit_equalizer(settings)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetAudioAnalysisEnabled { enabled } => {
                self.audio.validate_analysis_admission(enabled)?;
                let mut candidate = self.data.clone();
                candidate.audio_analysis_enabled = enabled;
                self.commit_data(candidate)?;
                self.audio.set_analysis_enabled(enabled)?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetEqualizerPreset { preset } => {
                self.commit_equalizer(preset.settings())?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetSpatialSettings { settings } => {
                let mut effects = self.effects_preferences().settings;
                effects.spatial = settings;
                state = if self.commit_effect_settings(effects)? {
                    ReceiptState::Committed
                } else {
                    ReceiptState::Accepted
                };
            }
            BackendCommand::SetEffects { settings } => {
                state = if self.commit_effect_settings(settings)? {
                    ReceiptState::Committed
                } else {
                    ReceiptState::Accepted
                };
            }
            BackendCommand::ResetEffects => {
                state = if self.commit_effect_settings(Default::default())? {
                    ReceiptState::Committed
                } else {
                    ReceiptState::Accepted
                };
            }
            BackendCommand::ApplyBundledImpulsePreset { id } => {
                state = if self.apply_bundled_preset(id)? {
                    ReceiptState::Committed
                } else {
                    ReceiptState::Accepted
                };
            }
            BackendCommand::ImportImpulseResource {
                path,
                file_read_approved,
                timeout_ms,
            } => {
                self.import_impulse(path, file_read_approved, timeout_ms)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RecoverImpulseImport { token, timeout_ms } => {
                self.recover_impulse_import(token, timeout_ms)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RecoverImpulseNamespace {
                selection,
                timeout_ms,
            } => {
                self.recover_impulse_namespace(selection, timeout_ms)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::AbortImpulseImport { token, selection } => {
                self.abort_impulse_import(token, selection)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RetireImpulseResource { resource_id } => {
                self.retire_impulse_resource(resource_id)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::RecoverImpulseRetirement { token } => {
                self.recover_impulse_retirement(token)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CancelEffectsOperation { operation_id } => {
                ensure!(
                    self.cancel_effect_operation(&operation_id),
                    "Effect operation is unknown or already finished"
                );
            }
            BackendCommand::ResetEqualizer => {
                self.commit_equalizer(Default::default())?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetMute { muted } => {
                let mut candidate = self.data.clone();
                candidate.audio_preferences.muted = muted;
                self.commit_data(candidate)?;
                self.audio
                    .set_output_preferences(self.data.audio_preferences.clone())?;
                state = ReceiptState::Committed;
            }
            BackendCommand::SetPauseOnDeviceChange { enabled } => {
                let mut candidate = self.data.clone();
                candidate.audio_preferences.pause_on_device_change = enabled;
                self.commit_data(candidate)?;
                self.audio
                    .set_output_preferences(self.data.audio_preferences.clone())?;
                state = ReceiptState::Committed;
            }
            BackendCommand::RefreshAudioDevices => {
                self.audio.refresh_audio_devices()?;
            }
            BackendCommand::Volume { volume } => {
                ensure!(
                    volume.is_finite() && (0.0..=1.0).contains(&volume),
                    "Volume must be between 0 and 1"
                );
                let mut candidate = self.data.clone();
                candidate.volume = volume;
                self.commit_data(candidate)?;
                self.audio.set_volume(volume);
                state = ReceiptState::Committed;
            }
            BackendCommand::ClearError => {
                if self.audio.snapshot().status == PlaybackStatus::Error {
                    self.invalidate_preload(Invalidation::Playback);
                    self.audio.stop();
                }
                self.audio.clear_error();
                self.shared.lock().unwrap().error = None;
            }
            BackendCommand::ApprovePlaybackUrl => {
                self.shared.lock().unwrap().pending_playback_url = None;
                if let Some(mut pending) = self.pending.take()
                    && pending.generation == self.generation
                {
                    ensure!(
                        self.media_original_current(&pending.original, None),
                        BackendError::new(
                            ErrorKind::Conflict,
                            "Playback URL consent refers to changed metadata"
                        )
                    );
                    let source = self
                        .data
                        .sources
                        .iter()
                        .find(|source| {
                            self.data.selected.as_deref() == Some(source.sha256.as_str())
                                && source.sha256 == pending.identity.source_sha256
                                && u64::from(source.approval_policy_version)
                                    == pending.identity.source_policy_version
                        })
                        .context("Playback URL source permission changed")?;
                    source.require_approval()?;
                    ensure!(
                        pending.identity.source_epoch == self.source_epoch,
                        BackendError::new(
                            ErrorKind::Conflict,
                            "Playback URL consent refers to an old source session"
                        )
                    );
                    self.approval_epoch = self.approval_epoch.wrapping_add(1);
                    pending.identity.approval_id = self.request_id.clone();
                    pending.identity.approval_epoch = self.approval_epoch;
                    self.load_approved(&pending.url, pending.identity, self.play_when_ready)?;
                }
            }
            BackendCommand::ClearAudioCache => {
                self.begin_clear_audio_cache()?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::ApprovePreloadUrl { operation_id, url } => {
                self.approve_preload(operation_id, url)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::CancelPreload { operation_id } => {
                ensure!(
                    self.preload.snapshot().operation_id.as_deref() == Some(operation_id.as_str()),
                    BackendError::new(ErrorKind::Conflict, "Preload operation is stale")
                );
                self.invalidate_preload(Invalidation::ApprovalRevoked);
            }
            BackendCommand::CancelPlaybackUrl => {
                self.skip_pending = None;
                self.pending = None;
                self.shared.lock().unwrap().pending_playback_url = None;
                self.notice("已取消网络音频播放");
            }
            BackendCommand::CancelOperation { operation_id } => {
                self.cancel_effect_operation(&operation_id);
                self.tag_edits.cancel(&operation_id);
                self.cancel_download_postprocessing(&operation_id);
                self.backup_exports.cancel(&operation_id);
                self.physical_snapshots.cancel(&operation_id);
                self.cancel_cleanup_operation(&operation_id);
                self.library.cancel(&operation_id);
                self.cancel_provider(&operation_id);
                if let Some(cancel) = self.import_cancels.remove(&operation_id) {
                    cancel.terminate();
                    self.cancelled.insert(operation_id.clone());
                }
                if let Some(cancel) = self.file_cancels.remove(&operation_id) {
                    cancel.store(true, Ordering::Release);
                    self.cancelled.insert(operation_id.clone());
                }
                if self.init_operation.as_deref() == Some(operation_id.as_str()) {
                    let mut candidate = self.data.clone();
                    candidate.selected = None;
                    self.commit_data(candidate)?;
                    self.cancel_runtime();
                    self.audio.stop();
                    state = ReceiptState::Committed;
                }
                if self.active_operation.as_deref() == Some(&operation_id) {
                    self.cancel_playback_resolution();
                    self.generation = self.generation.wrapping_add(1);
                }
                if self.preload_accept_operation.as_deref() == Some(operation_id.as_str())
                    || self.preload.snapshot().operation_id.as_deref()
                        == Some(operation_id.as_str())
                {
                    self.invalidate_preload(Invalidation::ApprovalRevoked);
                }
                if self
                    .lyric_pending
                    .as_ref()
                    .is_some_and(|pending| pending.operation.as_deref() == Some(&operation_id))
                    || self
                        .pending_lyric
                        .as_ref()
                        .is_some_and(|(_, _, operation, _)| {
                            operation.as_deref() == Some(operation_id.as_str())
                        })
                {
                    self.cancel_lyrics(false);
                    self.lyric_key = self
                        .audio
                        .snapshot()
                        .track_id
                        .map(|id| (id, self.audio.snapshot().generation));
                }
            }
            BackendCommand::LoadLyrics { id } => {
                let audio = self.audio.snapshot();
                ensure!(
                    audio.track_id.as_deref() == Some(&id),
                    "LoadLyrics requires the current playback track"
                );
                self.cancel_lyrics(false);
                self.begin_lyrics(id, audio.generation, Some(self.request_id.clone()), 0)?;
                state = ReceiptState::Accepted;
            }
            BackendCommand::SaveLyrics { id, bundle } => {
                let change = self.lyric_service.save_edited(&id, &bundle, None)?;
                self.revision = change.revision;
                self.lyrics_modified(&id);
                state = ReceiptState::Committed;
            }
            BackendCommand::RestoreLyrics { id } => {
                self.revision = self.lyric_service.restore_raw(&id)?.revision;
                self.lyrics_modified(&id);
                state = ReceiptState::Committed;
            }
            BackendCommand::ClearRawLyrics { id } => {
                self.revision = self.lyric_service.clear_raw(&id)?.revision;
                self.lyrics_modified(&id);
                state = ReceiptState::Committed;
            }
            BackendCommand::SetLyricOffset { milliseconds } => {
                ensure!(
                    milliseconds.unsigned_abs() <= 86_400_000,
                    "Lyric offset exceeds one day"
                );
                self.lyric_offset = milliseconds;
                if let Some(timeline) = self.lyric_timeline.as_mut() {
                    timeline.set_offset(milliseconds);
                }
                self.save_checkpoint()?;
                state = ReceiptState::Committed;
            }
        }
        if manual_playback {
            self.release_timeout_stop_latch();
        }
        Ok(state)
    }

    fn publish_source_events(
        &self,
        source_id: String,
        events: Vec<crate::source::SourceHostEvent>,
    ) {
        let mut journal = self.events.lock().unwrap();
        for payload in events {
            journal.publish(
                self.revision,
                "source",
                self.active_operation.clone(),
                BackendEvent::SourceHost {
                    source_id: source_id.clone(),
                    payload,
                },
            );
        }
    }

    fn complete(&mut self, completion: Completion) -> Result<()> {
        match completion {
            Completion::Initialized(epoch, id, result) if epoch == self.source_epoch => {
                self.source_initializing = false;
                self.busy = self.pending_foreground.is_some();
                let worker = match result {
                    Ok(worker) => worker,
                    Err(error) => {
                        self.cancellation = None;
                        self.statuses.insert(id, "初始化失败".into());
                        return Err(error);
                    }
                };
                let names = worker
                    .manifest()
                    .sources
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                self.return_source_worker(epoch, worker);
                self.statuses.insert(id, format!("运行中 · {names}"));
                self.notice("音源初始化成功；播放曲目将经过 lx.request 事件解析。");
            }
            Completion::Resolved {
                generation,
                source_epoch,
                preload_operation,
                worker,
                result,
                original,
                cache_entry,
                matches,
                cache_hit,
                attempted_platforms,
            } => {
                let cache_entry = cache_entry.map(|value| *value);
                let matches = matches.map(|value| *value);
                let source_current = source_epoch == self.source_epoch;
                let cancelled = self
                    .source_inflight
                    .as_ref()
                    .is_some_and(|flight| flight.cancel.load(Ordering::Acquire));
                self.return_source_worker(source_epoch, worker);
                if !source_current {
                    return Ok(());
                }
                if generation != self.generation {
                    return Ok(());
                }
                if cancelled
                    || !self.media_original_current(&original, preload_operation.as_deref())
                {
                    if preload_operation.is_none() {
                        self.busy = false;
                    }
                    return Err(crate::source::SourceRequestCancelled.into());
                }
                if preload_operation.is_none() {
                    self.busy = false;
                }
                if let Some(media) = &cache_entry {
                    let source = self
                        .data
                        .sources
                        .iter()
                        .find(|source| {
                            source.sha256 == media.key.source_sha256
                                && u64::from(source.approval_policy_version)
                                    == media.key.source_policy_version
                        })
                        .context("Resolved source permission changed")?;
                    source.require_approval()?;
                    if !cache_hit {
                        self.repository.put_resolved_media(media)?;
                    }
                }
                if let Some(matches) = matches
                    && self
                        .repository
                        .source_matches(&matches.key, media::now_ms())?
                        .hit()
                        .as_ref()
                        != Some(&matches)
                {
                    self.repository.put_source_matches(&matches)?;
                }
                if let Some(operation) = preload_operation {
                    self.preload_media = cache_entry;
                    self.complete_preload_resolution(operation, result);
                } else if generation == self.generation {
                    self.busy = false;
                    self.media_attempted_sources.extend(attempted_platforms);
                    self.shared.lock().unwrap().resolved_track = cache_entry
                        .as_ref()
                        .and_then(|media| media.matched_track.as_ref())
                        .map(TrackSummary::from);
                    self.active_media = cache_entry;
                    let (url, identity) = result?;
                    self.media_attempted_sources
                        .insert(identity.platform.clone());
                    if let Some((_, asset_url, asset_identity)) =
                        self.catalog_asset_authority(&original)?
                        && url == asset_url
                    {
                        self.poll_timeout_stop();
                        self.load_approved(&url, asset_identity, self.play_when_ready)?;
                    } else {
                        self.shared.lock().unwrap().pending_playback_url = Some(url.clone());
                        self.pending = Some(PendingPlayback {
                            url,
                            identity,
                            generation,
                            original,
                        });
                        self.notice("音源返回了网络地址，请确认后下载播放。");
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn refresh(&mut self) {
        self.poll_timeout_stop();
        let library_summary = self.effective_library_summary().map(|mut summary| {
            summary.running_operations = self.library.active();
            if let Some(operation) = self.library_operation_latest() {
                summary.latest_operation_id = Some(operation);
            }
            summary
        });
        if let Ok(summary) = library_summary {
            self.shared.lock().unwrap().library = summary;
        }
        if self.audio.take_device_change_pause() {
            self.play_when_ready = false;
            self.media_retry_pending = None;
            self.skip_pending = None;
            if let Err(error) = self.save_checkpoint() {
                self.error(error.to_string());
            }
        }
        self.poll_media_retry();
        self.poll_preload();
        self.project_preload();
        let audio = self.audio.snapshot();
        if audio.status == PlaybackStatus::Ended
            && !self.timeout_stop.blocks_automatic_playback()
            && (self.previous_audio_status != PlaybackStatus::Ended
                || self.previous_audio_generation != audio.generation)
            && !self.busy
            && {
                self.failed_entries.clear();
                self.failure_attempts = 0;
                true
            }
            && let Err(error) = self.advance_queue(AdvanceReason::Ended, false)
        {
            self.error(error.to_string());
        }
        if audio.status == PlaybackStatus::Error
            && !self.timeout_stop.blocks_automatic_playback()
            && (self.previous_audio_status != PlaybackStatus::Error
                || self.previous_audio_generation != audio.generation)
            && !self.busy
            && self.play_when_ready
            && audio.track_id.as_deref()
                == self
                    .current_record
                    .as_ref()
                    .map(|record| record.id.as_str())
        {
            let error = anyhow::anyhow!(
                audio
                    .error
                    .clone()
                    .unwrap_or_else(|| "Audio playback failed".into())
            );
            if !self.recover_media_failure(&audio).unwrap_or(false) {
                self.schedule_failure_skip(&error);
            }
        }
        if self
            .skip_pending
            .as_ref()
            .is_some_and(|(deadline, generation)| {
                *deadline <= Instant::now() && *generation == self.generation
            })
        {
            self.skip_pending = None;
            if self.auto_skip_on_error
                && self.play_when_ready
                && let Err(error) = self.advance_queue(AdvanceReason::Failed, false)
            {
                self.error(format!("{error:#}"));
                self.schedule_failure_skip(&error);
            }
        }
        if let Some((id, position, previous_generation)) = self.seek_after_load.as_ref()
            && audio.track_id.as_deref() == Some(id.as_str())
            && audio.generation != *previous_generation
            && matches!(
                audio.status,
                PlaybackStatus::Playing | PlaybackStatus::Paused | PlaybackStatus::Buffering
            )
        {
            let seconds = (*position as f64 / 1000.0).min(if audio.duration_seconds > 0.0 {
                audio.duration_seconds
            } else {
                *position as f64 / 1000.0
            });
            self.audio.seek(seconds);
            self.seek_after_load = None;
        }
        self.previous_audio_status = audio.status;
        self.previous_audio_generation = audio.generation;
        if !self.busy
            && audio.generation != self.lyric_start_after_generation
            && matches!(
                audio.status,
                PlaybackStatus::Loading
                    | PlaybackStatus::Buffering
                    | PlaybackStatus::Playing
                    | PlaybackStatus::Paused
            )
            && let Some(id) = audio
                .track_id
                .as_ref()
                .filter(|id| self.intended_track.as_deref() == Some(id.as_str()))
        {
            let key = (id.clone(), audio.generation);
            if self.lyric_key.as_ref() != Some(&key) {
                self.cancel_lyrics(false);
                if let Err(error) = self.begin_lyrics(id.clone(), audio.generation, None, 0) {
                    self.lyric_key = Some(key);
                    self.shared.lock().unwrap().lyrics_error = Some(error.to_string());
                }
            }
        }
        let frame = self.lyric_timeline.as_mut().and_then(|timeline| {
            timeline.update(&AudioClockSnapshot {
                generation: audio.generation,
                position_ms: (audio.position_seconds * 1000.0)
                    .max(0.0)
                    .min(i64::MAX as f64) as i64,
                status: match audio.status {
                    PlaybackStatus::Idle => ClockStatus::Idle,
                    PlaybackStatus::Loading | PlaybackStatus::Buffering => ClockStatus::Buffering,
                    PlaybackStatus::Playing => ClockStatus::Playing,
                    PlaybackStatus::Paused => ClockStatus::Paused,
                    PlaybackStatus::Ended => ClockStatus::Ended,
                    PlaybackStatus::Error => ClockStatus::Error,
                },
                rate: audio.rate,
                seek_seq: audio.seek_seq,
            })
        });
        let catalog_revision = self.catalog.revision();
        if self.catalog_revision != catalog_revision {
            self.local_ids = self
                .catalog
                .tracks()
                .iter()
                .map(|track| track.id.clone())
                .collect();
            if self.queue.origin_list == "catalog"
                && let Err(error) = self.reconcile_queue()
            {
                self.error(error.to_string());
            }
        }
        let queue_signature = format!(
            "{}:{:?}:{}:{}",
            self.queue.revision,
            self.queue.current.as_ref().map(|entry| &entry.entry_id),
            self.restore_pending,
            self.skip_pending.is_some()
        );
        if queue_signature != self.last_queue_signature {
            self.last_queue_signature = queue_signature;
            self.events.lock().unwrap().publish(
                self.revision,
                &self.origin,
                None,
                BackendEvent::QueueChanged {
                    queue_revision: self.queue.revision,
                    current_entry_id: self
                        .queue
                        .current
                        .as_ref()
                        .map(|entry| entry.entry_id.clone()),
                    mode: self.queue.mode,
                },
            );
        }
        let display_record = self
            .queue
            .current
            .as_ref()
            .and_then(|entry| self.queue_record(entry).ok())
            .map(Arc::new)
            .or_else(|| self.current_record.clone());
        let mut snapshot = self.shared.lock().unwrap();
        let processing = self.audio.processing_snapshot();
        let prior = snapshot.playback_processing;
        let processing_changed = prior.desired != processing.desired
            || prior.applied != processing.applied
            || prior.requested_sequence != processing.requested_sequence
            || prior.applied_sequence != processing.applied_sequence
            || prior.sample_rate != processing.sample_rate
            || prior.latency_frames != processing.latency_frames
            || (prior.transition_remaining_frames != 0)
                != (processing.transition_remaining_frames != 0);
        snapshot.playback_processing = processing;
        snapshot.effects = self.audio.effects_snapshot();
        if processing_changed {
            self.events.lock().unwrap().publish(
                self.revision,
                "audio",
                None,
                BackendEvent::PlaybackProcessingChanged {
                    state: Box::new(processing),
                },
            );
        }
        let equalizer = self.audio.equalizer_snapshot();
        let previous = &snapshot.equalizer;
        let changed = previous.desired != equalizer.desired
            || previous.applied_sequence != equalizer.applied_sequence
            || previous.requested_sequence != equalizer.requested_sequence
            || previous.sample_rate != equalizer.sample_rate
            || previous.active_bands != equalizer.active_bands
            || (previous.transition_remaining_frames != 0)
                != (equalizer.transition_remaining_frames != 0);
        snapshot.equalizer = equalizer;
        if changed {
            self.events.lock().unwrap().publish(
                self.revision,
                "audio",
                None,
                BackendEvent::EqualizerChanged {
                    state: Box::new(equalizer),
                },
            );
        }
        let output = self.audio.output_snapshot();
        if snapshot.audio_output != output {
            snapshot.audio_output = output.clone();
            self.events.lock().unwrap().publish(
                self.revision,
                "audio",
                None,
                BackendEvent::AudioOutputChanged {
                    output: Box::new(output),
                },
            );
        }
        if frame.as_ref() != snapshot.lyric_frame.as_deref() {
            if let Some(frame) = &frame {
                self.events.lock().unwrap().publish(
                    self.revision,
                    "backend",
                    None,
                    BackendEvent::LyricFrameChanged {
                        generation: frame.generation,
                        seek_seq: frame.seek_seq,
                        line_index: frame.line_index,
                        word_index: frame.word_index,
                        word_progress: frame.word_progress,
                    },
                );
            }
            snapshot.lyric_frame = frame.map(Arc::new);
        }
        if self.catalog_revision != catalog_revision {
            snapshot.tracks = Arc::new(
                self.catalog
                    .tracks()
                    .iter()
                    .map(TrackSummary::from)
                    .collect(),
            );
            snapshot.queue = snapshot.tracks.clone();
            self.catalog_revision = catalog_revision;
        }
        snapshot.now_playing = display_record
            .as_ref()
            .filter(|record| {
                self.restore_pending || audio.track_id.as_deref() == Some(record.id.as_str())
            })
            .map(|record| TrackSummary::from(record.as_ref()));
        snapshot.busy = self.busy
            || matches!(
                audio.status,
                PlaybackStatus::Loading | PlaybackStatus::Buffering
            )
            || audio.buffering
            || !self.import_cancels.is_empty()
            || !self.file_cancels.is_empty()
            || self.backup_exports.has_running()
            || self.physical_snapshots.has_running()
            || !self.cache_jobs.is_empty();
        if audio.error.is_some() {
            snapshot.error = audio.error.clone();
        }
        snapshot.playback = audio;
        if self.restore_pending
            && let Some(record) = self.current_record.as_ref()
        {
            snapshot.playback.status = PlaybackStatus::Paused;
            snapshot.playback.track_id = Some(record.id.clone());
            snapshot.playback.position_seconds = self.restored_position_ms as f64 / 1000.0;
            snapshot.playback.duration_seconds = record.duration_ms.unwrap_or(0) as f64 / 1000.0;
            snapshot.playback.duration_known = record.duration_ms.is_some();
            snapshot.playback.buffering = false;
        }
        snapshot.revision = self.revision;
        snapshot.sources = self
            .data
            .sources
            .iter()
            .map(|source| SourceSummary {
                id: source.sha256.clone(),
                name: source.metadata.name.clone(),
                version: source.metadata.version.clone(),
                author: source.metadata.author.clone(),
                description: source.metadata.description.clone(),
                hash: source.sha256.clone(),
                enabled: self.data.selected.as_deref() == Some(&source.sha256),
                approved: source.approved,
                status: self
                    .statuses
                    .get(&source.sha256)
                    .cloned()
                    .unwrap_or_else(|| {
                        if source.approved {
                            if self.data.selected.as_deref() == Some(source.sha256.as_str()) {
                                "已授权 · 待按需初始化"
                            } else {
                                "已授权 · 已停用"
                            }
                            .into()
                        } else {
                            "已校验 · 等待授权".into()
                        }
                    }),
            })
            .collect();
        let signature = serde_json::to_string(&serde_json::json!({"audio": snapshot.playback, "revision":self.revision, "catalog":catalog_revision, "sources":snapshot.sources, "error":snapshot.error, "notice":snapshot.notice, "pending":snapshot.pending_playback_url, "approved_audio_identity":snapshot.approved_audio_identity, "busy":snapshot.busy,"lyrics":snapshot.lyrics.as_ref().map(|lyrics|(&lyrics.track_id,lyrics.generation)),"lyrics_error":snapshot.lyrics_error})).unwrap_or_default();
        if signature != self.last_signature {
            self.last_signature = signature;
            self.events.lock().unwrap().publish(
                self.revision,
                &self.origin,
                Some(self.request_id.clone()),
                BackendEvent::StateChanged {
                    generation: snapshot.playback.generation,
                    seek_seq: snapshot.playback.seek_seq,
                },
            );
        }
        drop(snapshot);
        if let Err(error) = self.project_external_status() {
            self.error(format!("External status projection failed: {error:#}"));
        }
    }
    fn lyrics_modified(&mut self, id: &str) {
        let audio = self.audio.snapshot();
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::LyricChanged {
                track_id: id.into(),
                generation: audio.generation,
            },
        );
        if audio.track_id.as_deref() == Some(id) {
            self.cancel_lyrics(false);
        }
    }
}

fn lyric_error_ref(error: &anyhow::Error) -> BackendError {
    if error.is::<crate::source::SourceRequestCancelled>() {
        return BackendError::new(ErrorKind::Cancelled, "Source request cancelled");
    }
    if let Some(error) = error.downcast_ref::<BackendError>() {
        return error.clone();
    }
    let kind = match error.downcast_ref::<LyricServiceError>() {
        Some(
            LyricServiceError::Cancelled
            | LyricServiceError::StaleGeneration
            | LyricServiceError::ChangedTrack,
        ) => ErrorKind::Cancelled,
        Some(LyricServiceError::RevisionConflict) => ErrorKind::Conflict,
        Some(LyricServiceError::Unavailable) => ErrorKind::Unavailable,
        _ => ErrorKind::OperationFailed,
    };
    BackendError::new(kind, error.to_string())
}
fn backend_error(error: anyhow::Error) -> BackendError {
    if let Some(error) = library::backend_library_error_ref(&error) {
        return error;
    }
    if error.is::<crate::source::SourceRequestCancelled>() {
        return BackendError::new(ErrorKind::Cancelled, "Source request cancelled");
    }
    if let Some(error) = error.downcast_ref::<BackendError>() {
        return error.clone();
    }
    let message = format!("{error:#}");
    let kind = if message.to_ascii_lowercase().contains("revision conflict") {
        ErrorKind::Conflict
    } else if error.downcast_ref::<rusqlite::Error>().is_some()
        || error.downcast_ref::<std::io::Error>().is_some()
    {
        ErrorKind::Storage
    } else {
        ErrorKind::InvalidData
    };
    BackendError::new(kind, message)
}

fn provider_backend_error(error: &ProviderError) -> BackendError {
    let kind = match error {
        ProviderError::Cancelled => ErrorKind::Cancelled,
        ProviderError::DeadlineExceeded => ErrorKind::Timeout,
        ProviderError::InvalidRequest { .. } => ErrorKind::InvalidData,
        ProviderError::Unsupported { .. } => ErrorKind::Unavailable,
        _ => ErrorKind::OperationFailed,
    };
    BackendError::new(kind, error.to_string())
}

fn bounded_provider_error(mut error: ProviderError) -> ProviderError {
    let message = match &mut error {
        ProviderError::InvalidRequest { message }
        | ProviderError::Network { message }
        | ProviderError::Http { message, .. }
        | ProviderError::Parse { message }
        | ProviderError::Upstream { message } => Some(message),
        ProviderError::Unsupported { operation, .. } => Some(operation),
        _ => None,
    };
    if let Some(message) = message
        && message.len() > 2048
    {
        let mut end = 2048;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    error
}

fn provider_json_size(
    value: &ProviderOperationResult,
    context: Option<&ProviderContext>,
) -> Result<usize, ProviderError> {
    struct Counter<'a> {
        bytes: usize,
        context: Option<&'a ProviderContext>,
    }
    impl std::io::Write for Counter<'_> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self.context.is_some_and(|context| context.check().is_err()) {
                return Err(std::io::Error::other(
                    "Provider operation was cancelled or expired",
                ));
            }
            if buffer.len() > PROVIDER_RESULT_BYTES.saturating_sub(self.bytes) {
                return Err(std::io::Error::other(
                    "Provider result exceeds cache capacity",
                ));
            }
            self.bytes += buffer.len();
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, context };
    if serde_json::to_writer(&mut counter, value).is_err() {
        if let Some(context) = context {
            context.check()?;
        }
        return Err(ProviderError::ResponseTooLarge {
            limit: PROVIDER_RESULT_BYTES,
        });
    }
    Ok(counter.bytes)
}
fn lyric_backend_error(error: anyhow::Error) -> BackendError {
    lyric_error_ref(&error)
}
