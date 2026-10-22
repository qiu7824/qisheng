//! Device-independent command, query and event boundary for the player.
mod data_paths;
mod external_control;
mod runtime;
mod sync_service;
pub use external_control::{BackendExternalSession, BackendOpenApiServer};
pub use runtime::backup_export::{BackupExportEncoding, BackupExportFormat, BackupExportView};
pub use runtime::deeplink::{DeepLinkOperationPage, DeepLinkOperationState, DeepLinkPageMetadata};
pub use runtime::download_cleanup::DownloadCleanupView;
pub use runtime::effects_control::{
    EffectsBackendState, EffectsOperationView, EffectsPreferences, ImpulseAbortSelection,
    ImpulseNamespaceSelection,
};
#[cfg(windows)]
pub use runtime::instance_relay::forward_deeplink_to_existing_profile;
pub use runtime::instance_relay::{
    RetainedProfileGuardStartupFailure, RetainedRepositoryInitializationFailure,
    RetainedRepositoryOpenFailure,
};
pub use runtime::native_restore::{NativeBackupImportView, NativeBackupPreviewView};
pub use runtime::physical_snapshot::{PhysicalSnapshotRecoveryMode, PhysicalSnapshotView};
pub use runtime::profile_handover::{
    ProfileCheckpointSeal, ProfileHandover, ProfileQuiesceFailure, ProfileQuiesceScope,
    ProfileQuiesceTicket, ProfileQuiesceView,
};
pub use runtime::tag_edits::{
    LocalTagEditPatch, LocalTagReadView, TagEditField, TagEditRecoveryAction, TagEditView,
};
pub use sync_service::{BackendSyncNetwork, BackendSyncSession, SyncPeerView};

use crate::platforms::{
    Platform, ProviderError, ProviderRegistry, ProviderRequest, ProviderResponse,
};
use crate::{audio::PlaybackSnapshot, catalog::Track};
use crate::{
    domain::lyric::{LyricBundle, LyricOrigin},
    lyrics::{
        parser::LyricDocument,
        service::{LyricExport, LyricExportFormat, LyricProvenance},
        timeline::LyricFrame,
    },
};
use crate::{
    domain::{
        lists::{
            AddLocation, DislikeAction, DislikeRules, ListAction, ListInfo, ListPositionInfo,
            ListUpdateInfo, ListUpdateInfoMap,
        },
        music::TrackRecord,
    },
    playback::queue::{PlayMode, QueueEntry},
    repository::PlaybackCheckpoint,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const COMMAND_CAPACITY: usize = 64;
pub const EVENT_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendAudioMode {
    #[default]
    Device,
    Memory,
}

#[derive(Clone)]
pub struct BackendConfig {
    pub data_dir: PathBuf,
    pub audio_mode: BackendAudioMode,
    pub provider_registry: Option<Arc<ProviderRegistry>>,
    pub provider_timeout: Duration,
}
impl std::fmt::Debug for BackendConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendConfig")
            .field("data_dir", &self.data_dir)
            .field("audio_mode", &self.audio_mode)
            .field("provider_registry", &self.provider_registry.is_some())
            .field("provider_timeout", &self.provider_timeout)
            .finish()
    }
}
impl BackendConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            audio_mode: BackendAudioMode::Device,
            provider_registry: None,
            provider_timeout: Duration::from_secs(20),
        }
    }
    pub fn headless(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            audio_mode: BackendAudioMode::Memory,
            provider_registry: None,
            provider_timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum BackendCommand {
    ReadLocalTags {
        asset_id: String,
        tag_operation_id: String,
    },
    ApproveTagWrite {
        approval_id: String,
        selection: crate::library::tag_install::TagWriteSelection,
    },
    RevokeTagWrite {
        approval_id: String,
        expected_approval_epoch: u64,
    },
    ApproveTagRecovery {
        approval_id: String,
        tag_operation_id: String,
        selection: crate::library::tag_install::TagWriteSelection,
    },
    EditLocalTags {
        approval_id: String,
        expected_approval_epoch: u64,
        budget: crate::repository::TagEditBudget,
        patch: LocalTagEditPatch,
    },
    RecoverTagEdit {
        tag_operation_id: String,
        action: TagEditRecoveryAction,
        #[serde(default)]
        approval_id: Option<String>,
        #[serde(default)]
        expected_approval_epoch: Option<u64>,
    },
    CancelTagEdit {
        tag_operation_id: String,
    },
    SetSystemControlSettings {
        settings: crate::system_control::SystemSettings,
    },
    RetrySystemControl,
    StartTimeoutStop {
        seconds: f64,
        #[serde(default)]
        wait_for_track_end: Option<bool>,
    },
    SetTimeoutStopPreferences {
        settings: crate::playback::timeout_stop::TimeoutStopPreferences,
    },
    CancelTimeoutStop,
    SetSpatialSettings {
        settings: crate::audio::spatial::SpatialSettings,
    },
    SetEffects {
        settings: crate::domain::audio_effects::EffectsSettings,
    },
    ApplyBundledImpulsePreset {
        id: String,
    },
    ResetEffects,
    ImportImpulseResource {
        path: PathBuf,
        #[serde(default)]
        file_read_approved: bool,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    RecoverImpulseImport {
        token: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    RecoverImpulseNamespace {
        selection: ImpulseNamespaceSelection,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    AbortImpulseImport {
        token: String,
        #[serde(default)]
        selection: Option<ImpulseAbortSelection>,
    },
    RetireImpulseResource {
        resource_id: crate::domain::audio_effects::ImpulseResourceId,
    },
    RecoverImpulseRetirement {
        token: String,
    },
    CancelEffectsOperation {
        operation_id: String,
    },
    SetAudioAnalysisEnabled {
        enabled: bool,
    },
    DispatchDeepLink {
        url: String,
    },
    PreviewLxBackup {
        path: PathBuf,
        #[serde(default)]
        replace_tracks: bool,
        #[serde(default)]
        copy_list_id: Option<String>,
    },
    CommitLxBackup {
        preview_operation_id: String,
    },
    PreviewNativeBackup {
        path: PathBuf,
        #[serde(default)]
        replace_tracks: bool,
    },
    CommitNativeBackup {
        preview_operation_id: String,
        input_digest: String,
    },
    ExportPhysicalSnapshot {
        directory: PathBuf,
        basename: String,
        maximum_image_bytes: u64,
    },
    RecoverPhysicalSnapshot {
        snapshot_operation_id: String,
        directory: PathBuf,
        action: PhysicalSnapshotRecoveryMode,
    },
    ExportBackup {
        directory: PathBuf,
        basename: String,
        format: BackupExportFormat,
        #[serde(default)]
        encoding: BackupExportEncoding,
        #[serde(default)]
        include_sources: bool,
        #[serde(default)]
        include_downloads: bool,
        #[serde(default)]
        include_edited_lyrics: bool,
    },
    RetryBackupExport {
        export_operation_id: String,
    },
    CleanupBackupExport {
        export_operation_id: String,
        directory: PathBuf,
    },
    AddDownloadDestination {
        path: PathBuf,
    },
    RevokeDownloadDestination {
        grant_id: String,
        expected_grant_revision: u64,
    },
    SetDownloadSettings {
        settings: crate::download::task_model::DownloadSettings,
    },
    AddDownloadTasks {
        list_id: String,
        music_ids: Vec<String>,
        quality: crate::download::planning::DownloadQuality,
        #[serde(default)]
        top: bool,
    },
    RemoveDownloadTask {
        task_id: String,
        expected_task_revision: u64,
        operation_epoch: u64,
    },
    ResumeDownloadTask {
        task_id: String,
        expected_task_revision: u64,
    },
    RetryDownloadPostprocessing {
        task_id: String,
    },
    PlayDownloadTask {
        task_id: String,
    },
    PauseDownloadTask {
        task_id: String,
        expected_task_revision: u64,
    },
    RecoverDownloadCommit {
        task_id: String,
        expected_task_revision: u64,
    },
    RecoverDownloadCleanup {
        task_id: String,
        expected_task_revision: u64,
    },
    CancelDownloadCleanup {
        task_id: String,
        expected_task_revision: u64,
    },
    ApproveDownloadUrl {
        task_id: String,
        expected_task_revision: u64,
        operation_epoch: u64,
        url: String,
    },
    SetLibraryWatch {
        root_id: String,
        enabled: bool,
        #[serde(default)]
        options: crate::domain::library_watch::LibraryWatchOptions,
    },
    RelinkLocalAsset {
        asset_id: String,
        path: PathBuf,
        expected_binding_revision: u64,
        #[serde(default)]
        allow_content_change: bool,
    },
    ConfirmLibraryRelink {
        candidate_id: String,
        expected_binding_revision: u64,
        #[serde(default)]
        allow_content_change: bool,
    },
    RebindLibraryRoot {
        root_id: String,
        path: PathBuf,
        expected_grant_epoch: u64,
        expected_location_revision: u64,
    },
    AddLibraryRoot {
        path: PathBuf,
        #[serde(default = "default_library_recursive")]
        recursive: bool,
    },
    RemoveLibraryRoot {
        root_id: String,
    },
    ScanLibraryRoot {
        root_id: String,
        #[serde(default)]
        options: crate::domain::library::LibraryScanOptions,
    },
    SetPlaybackProcessing {
        settings: crate::domain::playback_processing::PlaybackProcessingSettings,
    },
    SetPlaybackRate {
        rate: f64,
    },
    SetPreservesPitch {
        enabled: bool,
    },
    SetPitchRatio {
        ratio: f64,
    },
    ResetPlaybackProcessing,
    SetEqualizer {
        settings: crate::domain::equalizer::EqualizerSettings,
    },
    SetEqualizerPreset {
        preset: crate::domain::equalizer::EqualizerPreset,
    },
    ResetEqualizer,
    SetAudioOutputDevice {
        selection: crate::domain::audio_output::OutputSelection,
    },
    RefreshAudioDevices,
    SetMute {
        muted: bool,
    },
    SetPauseOnDeviceChange {
        enabled: bool,
    },
    ResolvePicture {
        list_id: String,
        music_id: String,
    },
    ApprovePictureUrl {
        operation_id: String,
        url: String,
    },
    CancelPicture {
        operation_id: String,
    },
    ClearImageCache,
    ListAction {
        action: Box<ListAction>,
    },
    DislikeAction {
        action: DislikeAction,
    },
    SelectList {
        list_id: String,
    },
    PlayListTrack {
        list_id: String,
        music_id: String,
        #[serde(default)]
        quality: Option<String>,
    },
    SaveProviderTracks {
        operation_id: String,
        list_id: String,
        #[serde(default)]
        music_ids: Vec<String>,
        #[serde(default)]
        location: AddLocation,
    },
    Enqueue {
        items: Vec<QueueItemRef>,
        #[serde(default)]
        front: bool,
    },
    RemoveQueueEntry {
        entry_id: String,
    },
    ClearTemporaryQueue,
    SetPlayMode {
        mode: PlayMode,
    },
    SetPlaybackQuality {
        quality: String,
    },
    SetAutoSkipOnError {
        enabled: bool,
    },
    SetAutoSwitchSource {
        enabled: bool,
    },
    FindMusicMatches {
        list_id: String,
        music_id: String,
    },
    RefreshPlaybackUrl,
    ClearMediaCache,
    DislikeCurrent,
    SetListPosition {
        list_id: String,
        position: f64,
    },
    SetListUpdateInfo {
        list_id: String,
        info: ListUpdateInfo,
    },
    RecordSearch {
        keyword: String,
    },
    RemoveSearchHistory {
        keyword: String,
    },
    ClearSearchHistory,
    ClearAudioCache,
    ImportSource {
        path: PathBuf,
    },
    ImportSourceText {
        script: String,
    },
    ImportAudio {
        paths: Vec<PathBuf>,
    },
    AddDemoSource,
    ApproveSource {
        id: String,
    },
    EnableSource {
        id: String,
    },
    DisableSource,
    DeleteSource {
        id: String,
    },
    Play {
        id: String,
    },
    Pause,
    Resume,
    Toggle,
    Previous,
    Next,
    Stop,
    Seek {
        seconds: f64,
    },
    Volume {
        volume: f32,
    },
    ClearError,
    ApprovePlaybackUrl,
    CancelPlaybackUrl,
    ApprovePreloadUrl {
        operation_id: String,
        url: String,
    },
    CancelPreload {
        operation_id: String,
    },
    CancelOperation {
        operation_id: String,
    },
    LoadLyrics {
        id: String,
    },
    SaveLyrics {
        id: String,
        bundle: LyricBundle,
    },
    RestoreLyrics {
        id: String,
    },
    ClearRawLyrics {
        id: String,
    },
    SetLyricOffset {
        milliseconds: i64,
    },
    ProviderRequest {
        platform: Platform,
        request: Box<ProviderRequest>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueItemRef {
    pub list_id: String,
    pub track_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum BackendQuery {
    LocalTags {
        operation_id: String,
    },
    TagWriteApproval {
        approval_id: String,
    },
    TagEdit {
        operation_id: String,
    },
    TagEdits,
    SystemControlSettings,
    SystemControlState,
    TimeoutStopPreferences,
    TimeoutStop,
    DeepLinkResult {
        operation_id: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    LxBackupPreview,
    LxBackupImportResult {
        operation_id: String,
    },
    NativeBackupPreview,
    NativeBackupImportResult {
        operation_id: String,
    },
    NativeBackupArchive {
        operation_id: String,
    },
    BackupExport {
        operation_id: String,
    },
    PhysicalSnapshot {
        operation_id: String,
    },
    PhysicalSnapshots,
    DownloadSettings,
    DownloadCleanup {
        task_id: String,
    },
    DownloadDestinations,
    DownloadTasks,
    DownloadTask {
        task_id: String,
    },
    DownloadPendingUrl {
        task_id: String,
    },
    LibraryWatch {
        root_id: String,
    },
    LibraryWatches {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    LibraryAssetBinding {
        asset_id: String,
    },
    LibraryRootBinding {
        root_id: String,
    },
    LibraryRelinkCandidates {
        #[serde(default)]
        root_id: Option<String>,
        #[serde(default)]
        after_candidate_id: Option<String>,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    LibraryRelinkResult {
        operation_id: String,
    },
    LibraryRoots {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    LibraryRoot {
        root_id: String,
    },
    LibraryRootHealth {
        root_id: String,
    },
    LibraryOperations {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    LibraryOperation {
        operation_id: String,
    },
    LibraryAssets {
        #[serde(default)]
        root_id: Option<String>,
        #[serde(default)]
        after_asset_id: Option<String>,
        #[serde(default = "default_query_limit")]
        limit: usize,
        #[serde(default)]
        expected_revision: Option<u64>,
    },
    PlaybackProcessingSettings,
    PlaybackProcessingState,
    EqualizerSettings,
    EqualizerPresets,
    EqualizerState,
    AudioSettings,
    AudioOutputState,
    AudioOutputRenderTiming,
    AudioSupply,
    AudioDevices,
    PictureInfo {
        list_id: String,
        music_id: String,
    },
    PictureOperations {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    PictureCacheInfo {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    PictureCachedBytes {
        sha256: String,
        #[serde(default)]
        offset: u64,
        #[serde(default = "default_picture_byte_limit")]
        limit: usize,
    },
    ImageCacheStats,
    MediaCacheStats,
    MusicMatches {
        list_id: String,
        music_id: String,
    },
    RenderedPcm,
    AudioAnalysis,
    AudioAnalysisEnabled,
    SpatialSettings,
    SpatialState,
    EffectsSettings,
    EffectsState,
    BundledImpulsePresets,
    ImpulseResources,
    EffectsOperations,
    AudioCacheStats,
    Preload,
    ListInfos,
    ListTracks {
        list_id: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    ListTrack {
        list_id: String,
        music_id: String,
    },
    ListPositions,
    ListUpdateInfos,
    DislikeRules,
    SearchHistory,
    Queue {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_query_limit")]
        limit: usize,
    },
    PlaybackCheckpoint,
}
fn default_query_limit() -> usize {
    100
}
fn default_library_recursive() -> bool {
    true
}
fn default_picture_byte_limit() -> usize {
    crate::domain::artwork::MAX_ARTWORK_PAGE_BYTES
}
#[derive(Clone, Debug, Serialize)]
pub struct ListTrackPage {
    pub items: Vec<TrackRecord>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct QueueSnapshot {
    pub revision: u64,
    pub repository_revision: u64,
    pub mode: PlayMode,
    pub origin_list: String,
    pub current: Option<QueueEntry>,
    pub current_track: Option<TrackSummary>,
    pub entries: Vec<QueueEntry>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub temporary: Vec<QueueEntry>,
    pub temporary_total: usize,
    pub next: Option<QueueEntry>,
    pub restore_pending: bool,
    pub position_ms: u64,
    pub quality: String,
    pub auto_skip_on_error: bool,
    pub auto_switch_source: bool,
    pub skip_pending: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BackendQueryResult {
    LocalTags(Option<LocalTagReadView>),
    TagWriteApproval(Option<crate::repository::TagWriteApproval>),
    TagEdit(Option<Box<TagEditView>>),
    TagEdits(Vec<TagEditView>),
    SystemControlSettings(crate::system_control::SystemSettings),
    SystemControlState(crate::system_control::SystemControlView),
    TimeoutStopPreferences(crate::playback::timeout_stop::TimeoutStopPreferences),
    TimeoutStop(crate::playback::timeout_stop::TimeoutStopState),
    DeepLinkResult(Box<DeepLinkOperationPage>),
    LxBackupPreview(Option<BackupPreviewView>),
    LxBackupImportResult(Option<BackupImportView>),
    NativeBackupPreview(Option<NativeBackupPreviewView>),
    NativeBackupImportResult(Option<NativeBackupImportView>),
    NativeBackupArchive(Option<crate::repository::NativeBackupArchive>),
    BackupExport(Option<Box<BackupExportView>>),
    PhysicalSnapshot(Option<PhysicalSnapshotView>),
    PhysicalSnapshots(Vec<PhysicalSnapshotView>),
    DownloadSettings(crate::download::task_model::DownloadSettings),
    DownloadCleanup(DownloadCleanupView),
    DownloadDestinations(Vec<crate::download::task_model::StoredDestinationGrant>),
    DownloadTasks(Vec<crate::download::task_model::DownloadTaskView>),
    DownloadTask(Option<crate::download::task_model::DownloadTaskView>),
    DownloadPendingUrl(Option<DownloadUrlApprovalRequest>),
    LibraryWatch(Option<crate::domain::library_watch::LibraryWatchView>),
    LibraryWatches(LibraryWatchRuntimePage),
    LibraryAssetBinding(Option<crate::domain::library_watch::LibraryAssetBinding>),
    LibraryRootBinding(Option<crate::domain::library_watch::LibraryRootBinding>),
    LibraryRelinkCandidates(crate::domain::library_watch::LibraryRelinkCandidatePage),
    LibraryRelinkResult(Option<crate::domain::library_watch::LibraryRelinkResult>),
    LibraryRoots(crate::domain::library::LibraryRootPage),
    LibraryRoot(Option<crate::domain::library::LibraryRoot>),
    LibraryRootHealth(Option<LibraryRootHealthState>),
    LibraryOperations(crate::domain::library::LibraryOperationPage),
    LibraryOperation(Option<Box<crate::domain::library::LibraryOperationView>>),
    LibraryAssets(crate::domain::library::LibraryAssetPage),
    PlaybackProcessingSettings(crate::domain::playback_processing::PlaybackProcessingSettings),
    PlaybackProcessingState(crate::domain::playback_processing::PlaybackProcessingSnapshot),
    EqualizerSettings(crate::domain::equalizer::EqualizerSettings),
    EqualizerPresets(Vec<crate::domain::equalizer::EqualizerPresetInfo>),
    EqualizerState(crate::domain::equalizer::EqualizerSnapshot),
    AudioSettings(crate::domain::audio_output::AudioPreferences),
    AudioOutputState(crate::domain::audio_output::AudioOutputState),
    AudioOutputRenderTiming(Option<crate::audio::output::OutputRenderTiming>),
    AudioSupply(Option<crate::audio::supply::AudioSupplySnapshot>),
    AudioDevices(Vec<crate::domain::audio_output::AudioDeviceInfo>),
    PictureInfo(crate::domain::artwork::ArtworkLookup),
    PictureOperations(Vec<crate::domain::artwork::PictureOperationView>),
    PictureCacheInfo(Vec<crate::domain::artwork::ArtworkInfo>),
    PictureCachedBytes(crate::domain::artwork::PictureBytePage),
    ImageCacheStats(crate::domain::artwork::ImageCacheStats),
    MediaCacheStats(crate::repository::MediaCacheStats),
    MusicMatches(crate::domain::media::MediaCacheLookup<crate::domain::media::SourceMatches>),
    RenderedPcm(Option<crate::audio::RenderedPcm>),
    AudioAnalysis(Option<Box<crate::audio::analyser::AnalysisFrame>>),
    AudioAnalysisEnabled(bool),
    SpatialSettings(crate::audio::spatial::SpatialSettings),
    SpatialState(crate::domain::audio_effects::SpatialRuntimeState),
    EffectsSettings(crate::domain::audio_effects::EffectsSettings),
    EffectsState(Box<EffectsBackendState>),
    BundledImpulsePresets(Vec<crate::audio::bundled_impulses::BundledImpulsePreset>),
    ImpulseResources(EffectsPreferences),
    EffectsOperations(Vec<EffectsOperationView>),
    AudioCacheStats(Option<crate::audio::cache::CacheStats>),
    Preload(Option<Box<BackendPreloadSnapshot>>),
    ListInfos(Vec<ListInfo>),
    ListTracks(ListTrackPage),
    ListTrack(Option<Box<TrackRecord>>),
    ListPositions(ListPositionInfo),
    ListUpdateInfos(ListUpdateInfoMap),
    DislikeRules(DislikeRules),
    SearchHistory(Vec<String>),
    Queue(Box<QueueSnapshot>),
    PlaybackCheckpoint(Option<Box<PlaybackCheckpoint>>),
}

#[derive(Clone, Debug, Serialize)]
pub struct LibraryRootHealthState {
    pub root_id: String,
    pub grant_epoch: u64,
    pub health: crate::library::authority::RootHealth,
}
#[derive(Clone, Serialize)]
pub struct DownloadUrlApprovalRequest {
    pub task_id: String,
    pub task_revision: u64,
    pub operation_epoch: u64,
    pub source_content_sha256: String,
    pub source_policy_epoch: u64,
    pub url: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct BackupPreviewView {
    pub operation_id: String,
    pub expected_revision: u64,
    pub input_digest: String,
    pub imported_lists: usize,
    pub local_references: usize,
    pub settings_present: bool,
    pub requires_activation: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct BackupImportView {
    pub operation_id: String,
    pub input_digest: String,
    pub revision: u64,
    pub replayed: bool,
}
impl std::fmt::Debug for DownloadUrlApprovalRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DownloadUrlApprovalRequest")
            .field("task_id", &self.task_id)
            .field("task_revision", &self.task_revision)
            .field("operation_epoch", &self.operation_epoch)
            .field("url", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LibraryWatchRuntimePage {
    pub items: Vec<crate::domain::library_watch::LibraryWatchView>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderOperationState {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderOperationResult {
    pub operation_id: String,
    pub platform: Platform,
    pub operation: String,
    pub state: ProviderOperationState,
    pub response: Option<Arc<ProviderResponse>>,
    pub error: Option<ProviderError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub request_id: String,
    #[serde(default = "default_origin")]
    pub origin: String,
    #[serde(default)]
    pub expected_revision: Option<u64>,
    #[serde(flatten)]
    pub command: BackendCommand,
}
fn default_origin() -> String {
    "local".into()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Applied,
    Committed,
    Accepted,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub request_id: String,
    pub operation_id: String,
    pub revision: u64,
    pub state: ReceiptState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidData,
    Conflict,
    Busy,
    Cancelled,
    Closed,
    Timeout,
    Storage,
    Unavailable,
    OperationFailed,
    CursorExpired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendError {
    pub kind: ErrorKind,
    pub message: String,
    pub retryable: bool,
}
impl BackendError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: matches!(
                kind,
                ErrorKind::Busy | ErrorKind::Timeout | ErrorKind::Unavailable
            ),
        }
    }
}
impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for BackendError {}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrackSummary {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
}
impl From<&Track> for TrackSummary {
    fn from(track: &Track) -> Self {
        Self {
            id: track.id.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            duration: track.duration_seconds,
        }
    }
}
impl From<&TrackRecord> for TrackSummary {
    fn from(track: &TrackRecord) -> Self {
        Self {
            id: track.id.clone(),
            title: track.title.clone(),
            artist: track.artists.join("、"),
            album: track.album.clone(),
            duration: track.duration_ms.unwrap_or(0) as f64 / 1000.0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SourceSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub hash: String,
    pub enabled: bool,
    pub approved: bool,
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackendSnapshot {
    pub library: crate::domain::library::LibrarySummary,
    pub protocol_version: u32,
    pub revision: u64,
    pub stopped: bool,
    pub tracks: Arc<Vec<TrackSummary>>,
    pub queue: Arc<Vec<TrackSummary>>,
    pub sources: Vec<SourceSummary>,
    pub now_playing: Option<TrackSummary>,
    pub resolved_track: Option<TrackSummary>,
    pub playback: PlaybackSnapshot,
    pub audio_output: crate::domain::audio_output::AudioOutputSnapshot,
    pub equalizer: crate::domain::equalizer::EqualizerSnapshot,
    pub playback_processing: crate::domain::playback_processing::PlaybackProcessingSnapshot,
    pub effects: crate::domain::audio_effects::EffectsSnapshot,
    pub busy: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub pending_playback_url: Option<String>,
    pub approved_audio_identity: Option<crate::audio::transport::CacheIdentity>,
    pub preload: Option<BackendPreloadSnapshot>,
    pub lyrics: Option<Arc<LyricSnapshot>>,
    pub lyric_frame: Option<Arc<LyricFrame>>,
    pub lyrics_error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct LyricSnapshot {
    pub track_id: String,
    pub generation: u64,
    pub bundle: LyricBundle,
    pub document: LyricDocument,
    pub provenance: LyricProvenance,
}
impl Default for BackendSnapshot {
    fn default() -> Self {
        Self {
            library: Default::default(),
            protocol_version: PROTOCOL_VERSION,
            revision: 0,
            stopped: false,
            tracks: Arc::new(Vec::new()),
            queue: Arc::new(Vec::new()),
            sources: Vec::new(),
            now_playing: None,
            resolved_track: None,
            playback: PlaybackSnapshot::default(),
            audio_output: crate::domain::audio_output::AudioOutputSnapshot::default(),
            equalizer: crate::domain::equalizer::EqualizerSnapshot::default(),
            playback_processing: Default::default(),
            effects: Default::default(),
            busy: false,
            error: None,
            notice: None,
            pending_playback_url: None,
            approved_audio_identity: None,
            preload: None,
            lyrics: None,
            lyric_frame: None,
            lyrics_error: None,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct BackendPreloadSnapshot {
    pub state: String,
    pub operation_id: String,
    pub candidate: crate::playback::queue::QueueEntry,
    pub actual_quality: String,
    pub pending_url: Option<String>,
    pub identity: Option<crate::audio::transport::CacheIdentity>,
    pub failure: Option<String>,
}
impl std::fmt::Debug for BackendPreloadSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackendPreloadSnapshot")
            .field("state", &self.state)
            .field("operation_id", &self.operation_id)
            .field("candidate", &self.candidate)
            .field("actual_quality", &self.actual_quality)
            .field(
                "pending_url",
                &self.pending_url.as_ref().map(|_| "[redacted]"),
            )
            .field("identity", &self.identity)
            .field("failure", &self.failure)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BackendEvent {
    TagWriteApprovalChanged {
        approval: Box<crate::repository::TagWriteApproval>,
    },
    TagEditChanged {
        view: Box<TagEditView>,
    },
    SystemControlChanged {
        state: Box<crate::system_control::SystemControlView>,
    },
    TimeoutStopChanged {
        state: crate::playback::timeout_stop::TimeoutStopState,
    },
    LibraryWatchChanged {
        state: Box<crate::domain::library_watch::LibraryWatchView>,
    },
    LibraryWatchOverflow {
        root_id: String,
        watch_generation: u64,
        dropped_events: u64,
    },
    LibraryRelinked {
        result: Box<crate::domain::library_watch::LibraryRelinkResult>,
    },
    LibraryRootLocationChanged {
        root_id: String,
        grant_epoch: u64,
        location_revision: u64,
    },
    LibraryRootChanged {
        root_id: String,
        removed: bool,
    },
    LibraryScanProgress {
        state: Box<crate::domain::library::LibraryOperationView>,
    },
    LibraryScanFinished {
        state: Box<crate::domain::library::LibraryOperationView>,
    },
    PlaybackProcessingChanged {
        state: Box<crate::domain::playback_processing::PlaybackProcessingSnapshot>,
    },
    EqualizerChanged {
        state: Box<crate::domain::equalizer::EqualizerSnapshot>,
    },
    AudioOutputChanged {
        output: Box<crate::domain::audio_output::AudioOutputSnapshot>,
    },
    PictureChanged {
        operation_id: String,
        state: crate::domain::artwork::PictureState,
        sha256: Option<String>,
    },
    PreloadChanged {
        operation_id: String,
        state: String,
        candidate_entry_id: String,
        actual_quality: String,
    },
    PreloadPromoted {
        operation_id: String,
        entry_id: String,
        track_id: String,
        previous_audio_generation: u64,
    },
    ListsChanged {
        revision: u64,
        action: String,
    },
    DislikeChanged {
        revision: u64,
    },
    QueueChanged {
        queue_revision: u64,
        current_entry_id: Option<String>,
        mode: PlayMode,
    },
    PlaybackSelectionChanged {
        entry_id: String,
        list_id: String,
        track_id: String,
    },
    TrackSkipped {
        entry_id: String,
        reason: String,
    },
    SourceHost {
        source_id: String,
        payload: crate::source::SourceHostEvent,
    },
    ProviderFinished {
        operation_id: String,
        platform: Platform,
        state: ProviderOperationState,
        error: Option<ProviderError>,
    },
    StateChanged {
        generation: u64,
        seek_seq: u64,
    },
    OperationFinished {
        operation_id: String,
        success: bool,
        error: Option<BackendError>,
    },
    AudioCacheCleared {
        operation_id: String,
        stats: crate::audio::cache::CacheStats,
    },
    Stopped,
    LyricChanged {
        track_id: String,
        generation: u64,
    },
    LyricFrameChanged {
        generation: u64,
        seek_seq: u64,
        line_index: Option<usize>,
        word_index: Option<usize>,
        word_progress: f64,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct EventEnvelope {
    pub event_id: u64,
    pub repository_revision: u64,
    pub operation_id: Option<String>,
    pub origin: String,
    pub payload: BackendEvent,
}
#[derive(Default)]
pub(super) struct EventJournal {
    pub next_id: u64,
    pub events: VecDeque<EventEnvelope>,
}
impl EventJournal {
    pub fn publish(
        &mut self,
        revision: u64,
        origin: &str,
        operation_id: Option<String>,
        payload: BackendEvent,
    ) {
        self.next_id += 1;
        self.events.push_back(EventEnvelope {
            event_id: self.next_id,
            repository_revision: revision,
            operation_id,
            origin: origin.into(),
            payload,
        });
        if self.events.len() > EVENT_CAPACITY {
            self.events.pop_front();
        }
    }
}

pub(super) enum RuntimeMessage {
    ProfileQuiesce {
        request: runtime::profile_handover::ProfileQuiesceRequest,
        slot: Arc<runtime::profile_handover::ProfileQuiesceSlot>,
        admission: ProfileAdmission,
    },
    #[cfg(test)]
    TestActorPanic,
    #[cfg(test)]
    TestGatedTask {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    },
    #[cfg(test)]
    TestGatedPanicTask {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    },
    RelayInput {
        request: crate::system_control::instance_relay::RelayRequest,
        fence: crate::system_control::instance_relay::RelayFence,
        reply: mpsc::SyncSender<
            Result<
                crate::system_control::instance_relay::RelayAcknowledgement,
                crate::system_control::instance_relay::RelayError,
            >,
        >,
    },
    SystemInput {
        input: crate::system_control::SystemInput,
        fence: crate::system_control::SystemInputFence,
        cancel: Arc<AtomicBool>,
        reply: mpsc::SyncSender<Result<(), BackendError>>,
    },
    OpenSyncSession {
        cancel: Arc<AtomicBool>,
        reply: mpsc::SyncSender<Result<Arc<sync_service::SyncOwnerGate>, BackendError>>,
    },
    SyncRequest {
        gate: Arc<sync_service::SyncOwnerGate>,
        cancel: Arc<AtomicBool>,
        request: sync_service::SyncRequest,
        reply: mpsc::SyncSender<Result<sync_service::SyncResponse, BackendError>>,
    },
    SyncManagement {
        request: sync_service::SyncManagementRequest,
        cancel: Arc<AtomicBool>,
        reply: mpsc::SyncSender<Result<sync_service::SyncManagementResponse, BackendError>>,
    },
    OpenExternalSession {
        cancel: Arc<AtomicBool>,
        reply: mpsc::SyncSender<Result<Arc<external_control::ExternalSessionGate>, BackendError>>,
    },
    ExternalSnapshot {
        gate: Arc<external_control::ExternalSessionGate>,
        reply: mpsc::SyncSender<Result<crate::open_api::StatusSnapshot, BackendError>>,
    },
    ExternalUpdates {
        gate: Arc<external_control::ExternalSessionGate>,
        cursor: u64,
        reply: mpsc::SyncSender<Result<Vec<crate::open_api::StatusUpdate>, BackendError>>,
    },
    ExternalArtwork {
        gate: Arc<external_control::ExternalSessionGate>,
        reference: crate::open_api::ArtworkReference,
        reply: mpsc::SyncSender<crate::open_api::ControlResult<crate::open_api::ArtworkResponse>>,
    },
    ExternalCommand {
        gate: Arc<external_control::ExternalSessionGate>,
        action: crate::open_api::ControlAction,
        request_id: String,
        reply: mpsc::SyncSender<Result<(), BackendError>>,
    },
    Query {
        query: BackendQuery,
        reply: mpsc::SyncSender<Result<BackendQueryResult, BackendError>>,
    },
    QueryProviderResult {
        operation_id: String,
        reply: mpsc::SyncSender<Result<Arc<ProviderOperationResult>, BackendError>>,
    },
    Command {
        envelope: Box<CommandEnvelope>,
        reply: mpsc::SyncSender<Result<CommandReceipt, BackendError>>,
    },
    Wake,
    QueryLyrics {
        track_id: String,
        origin: Option<LyricOrigin>,
        reply: mpsc::SyncSender<Result<Option<LyricBundle>, BackendError>>,
    },
    ExportLyrics {
        track_id: String,
        format: LyricExportFormat,
        reply: mpsc::SyncSender<Result<LyricExport, BackendError>>,
    },
    SnapshotWithCursor {
        reply: mpsc::SyncSender<Result<(BackendSnapshot, u64), BackendError>>,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct BackendRetirementView {
    pub actor_joined: bool,
    pub runtime_retired: bool,
    pub error: Option<BackendError>,
}
pub(super) struct ProfileAdmission {
    mutation: Arc<AtomicBool>,
    external: Arc<AtomicBool>,
    sync: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}
impl ProfileAdmission {
    fn reopen_if_running(&self) {
        if !self.shutdown.load(Ordering::Acquire) {
            self.external.store(false, Ordering::Release);
            self.sync.store(false, Ordering::Release);
            self.mutation.store(false, Ordering::Release);
        }
    }
}
struct BackendInner {
    sync_admission_closed: Arc<AtomicBool>,
    sync_session_open: Mutex<()>,
    sync_endpoints: Mutex<Vec<sync_service::RegisteredSyncEndpoint>>,
    external_admission_closed: Arc<AtomicBool>,
    mutation_admission_closed: Arc<AtomicBool>,
    profile_admission: Mutex<()>,
    profile_slot: Mutex<Option<Arc<runtime::profile_handover::ProfileQuiesceSlot>>>,
    external_session_open: Mutex<()>,
    external_endpoints: Mutex<Vec<external_control::RegisteredEndpoint>>,
    commands: mpsc::SyncSender<RuntimeMessage>,
    snapshot: Arc<Mutex<BackendSnapshot>>,
    events: Arc<Mutex<EventJournal>>,
    shutdown: Arc<AtomicBool>,
    handover_pending: Arc<AtomicBool>,
    lifecycle: runtime::startup_ownership::ActorLifecycle,
    shutdown_outcome: Arc<Mutex<Option<Result<(), BackendError>>>>,
}
impl Drop for BackendInner {
    fn drop(&mut self) {
        for endpoint in self
            .sync_endpoints
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            endpoint.begin_close();
        }
        for endpoint in self
            .external_endpoints
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            endpoint.begin_close();
        }
        self.shutdown.store(true, Ordering::Release);
        let _ = self.commands.try_send(RuntimeMessage::Wake);
        // The bounded lifecycle pool retains the actual actor handle and whole
        // Runtime until its real service joins. Drop only requests retirement.
        let _ = self.lifecycle.poll();
    }
}
#[derive(Clone)]
pub struct BackendHandle {
    inner: Arc<BackendInner>,
}
impl BackendHandle {
    pub fn query(&self, query: BackendQuery) -> Result<BackendQueryResult, BackendError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::Query { query, reply })?;
        receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| BackendError::new(ErrorKind::Timeout, "Backend query timed out"))?
    }
    pub fn provider_result(
        &self,
        operation_id: &str,
    ) -> Result<Arc<ProviderOperationResult>, BackendError> {
        if operation_id.is_empty() || operation_id.len() > 128 {
            return Err(BackendError::new(
                ErrorKind::InvalidData,
                "operation_id must be a bounded identifier",
            ));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::QueryProviderResult {
            operation_id: operation_id.into(),
            reply,
        })?;
        receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| BackendError::new(ErrorKind::Timeout, "Provider result query timed out"))?
    }
    pub fn start(config: BackendConfig) -> anyhow::Result<Self> {
        let parts = runtime::start(config)?;
        Ok(Self::from_parts(parts))
    }
    pub fn start_with_guard(
        config: BackendConfig,
        guard: crate::repository::ProfileGuard,
    ) -> anyhow::Result<Self> {
        Ok(Self::from_parts(runtime::start_with_guard(
            config, guard, false,
        )?))
    }
    pub fn start_with_instance_relay(config: BackendConfig) -> anyhow::Result<Self> {
        let guard = crate::repository::ProfileGuard::acquire(&config.data_dir)
            .map_err(anyhow::Error::new)?;
        Self::start_with_guard_and_instance_relay(config, guard)
    }
    pub fn start_with_guard_and_instance_relay(
        config: BackendConfig,
        guard: crate::repository::ProfileGuard,
    ) -> anyhow::Result<Self> {
        Ok(Self::from_parts(runtime::start_with_guard(
            config, guard, true,
        )?))
    }
    fn from_parts(parts: runtime::RuntimeParts) -> Self {
        Self {
            inner: Arc::new(BackendInner {
                sync_admission_closed: Arc::new(AtomicBool::new(false)),
                sync_session_open: Mutex::new(()),
                sync_endpoints: Mutex::new(Vec::new()),
                commands: parts.commands,
                snapshot: parts.snapshot,
                events: parts.events,
                shutdown: parts.shutdown,
                handover_pending: parts.handover_pending,
                lifecycle: parts.lifecycle,
                mutation_admission_closed: Arc::new(AtomicBool::new(false)),
                profile_admission: Mutex::new(()),
                profile_slot: Mutex::new(None),
                external_session_open: Mutex::new(()),
                external_endpoints: Mutex::new(Vec::new()),
                external_admission_closed: Arc::new(AtomicBool::new(false)),
                shutdown_outcome: parts.shutdown_outcome,
            }),
        }
    }
    pub fn snapshot(&self) -> BackendSnapshot {
        self.inner
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// The actor projects state before capturing its event cursor, so clients can
    /// resume events without skipping a mutation published ahead of its snapshot.
    pub fn snapshot_with_cursor(&self) -> Result<(BackendSnapshot, u64), BackendError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::SnapshotWithCursor { reply })?;
        receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| BackendError::new(ErrorKind::Timeout, "Snapshot query timed out"))?
    }
    pub fn lyrics(
        &self,
        track_id: &str,
        origin: Option<LyricOrigin>,
    ) -> Result<Option<LyricBundle>, BackendError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::QueryLyrics {
            track_id: track_id.into(),
            origin,
            reply,
        })?;
        receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| BackendError::new(ErrorKind::Timeout, "Lyric query timed out"))?
    }
    pub fn export_lyrics(
        &self,
        track_id: &str,
        format: LyricExportFormat,
    ) -> Result<LyricExport, BackendError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::ExportLyrics {
            track_id: track_id.into(),
            format,
            reply,
        })?;
        receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| BackendError::new(ErrorKind::Timeout, "Lyric export timed out"))?
    }
    fn send_query(&self, message: RuntimeMessage) -> Result<(), BackendError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(BackendError::new(
                ErrorKind::Closed,
                "Backend is shutting down",
            ));
        }
        self.inner
            .commands
            .try_send(message)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    BackendError::new(ErrorKind::Busy, "Command queue is full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    BackendError::new(ErrorKind::Closed, "Backend has stopped")
                }
            })
    }
    pub fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, BackendError> {
        if self.inner.shutdown.load(Ordering::Acquire)
            || self.inner.mutation_admission_closed.load(Ordering::Acquire)
        {
            return Err(BackendError::new(
                ErrorKind::Closed,
                "Backend is shutting down",
            ));
        }
        if envelope.request_id.is_empty()
            || envelope.request_id.len() > 128
            || envelope.origin.len() > 128
        {
            return Err(BackendError::new(
                ErrorKind::InvalidData,
                "request_id and origin must be bounded identifiers",
            ));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.inner
            .commands
            .try_send(RuntimeMessage::Command {
                envelope: Box::new(envelope),
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    BackendError::new(ErrorKind::Busy, "Command queue is full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    BackendError::new(ErrorKind::Closed, "Backend has stopped")
                }
            })?;
        receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => BackendError::new(
                    ErrorKind::Timeout,
                    "Command acknowledgement timed out; query operation state before retrying",
                ),
                mpsc::RecvTimeoutError::Disconnected => BackendError::new(
                    ErrorKind::Closed,
                    "Backend stopped before acknowledging the command",
                ),
            })?
    }
    pub fn events_after(&self, cursor: u64) -> Result<Vec<EventEnvelope>, BackendError> {
        let journal = self.inner.events.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(first) = journal.events.front()
            && cursor.saturating_add(1) < first.event_id
        {
            return Err(BackendError::new(
                ErrorKind::CursorExpired,
                "Event cursor expired; obtain a fresh snapshot",
            ));
        }
        Ok(journal
            .events
            .iter()
            .filter(|event| event.event_id > cursor)
            .cloned()
            .collect())
    }
    pub fn event_cursor(&self) -> u64 {
        self.inner
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_id
    }
    pub fn tracks(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TrackPage, BackendError> {
        if limit == 0 || limit > 1000 {
            return Err(BackendError::new(
                ErrorKind::InvalidData,
                "limit must be between 1 and 1000",
            ));
        }
        let snapshot = self.snapshot();
        let needle = query.trim().to_lowercase();
        let matches = snapshot.tracks.iter().filter(|track| {
            needle.is_empty()
                || [&track.title, &track.artist, &track.album]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&needle))
        });
        let total = matches.clone().count();
        Ok(TrackPage {
            revision: snapshot.revision,
            total,
            offset,
            items: matches.skip(offset).take(limit).cloned().collect(),
        })
    }
    pub fn retirement_state(&self) -> BackendRetirementView {
        self.inner.lifecycle.view()
    }
    pub fn profile_quiesce_state(&self) -> Option<ProfileQuiesceView> {
        self.inner
            .profile_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|slot| slot.view())
    }
    /// Closes mutation admission and transfers the actual stopped profile to an
    /// owned ticket. Observation timeouts leave that same owner available.
    pub fn begin_profile_quiesce(
        &self,
        operation_id: String,
        expected_revision: u64,
    ) -> Result<ProfileQuiesceTicket, BackendError> {
        let request =
            runtime::profile_handover::ProfileQuiesceRequest::new(operation_id, expected_revision)
                .map_err(|error| BackendError::new(ErrorKind::InvalidData, error.to_string()))?;
        let _admission = self
            .inner
            .profile_admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.inner.shutdown.load(Ordering::Acquire)
            || self.inner.mutation_admission_closed.load(Ordering::Acquire)
        {
            return Err(BackendError::new(
                ErrorKind::Unavailable,
                "Profile lifecycle admission is closed",
            ));
        }
        self.inner
            .mutation_admission_closed
            .store(true, Ordering::Release);
        self.inner
            .external_admission_closed
            .store(true, Ordering::Release);
        self.inner
            .sync_admission_closed
            .store(true, Ordering::Release);
        let admission = ProfileAdmission {
            mutation: self.inner.mutation_admission_closed.clone(),
            external: self.inner.external_admission_closed.clone(),
            sync: self.inner.sync_admission_closed.clone(),
            shutdown: self.inner.shutdown.clone(),
        };
        let slot = Arc::new(runtime::profile_handover::ProfileQuiesceSlot::default());
        if let Err(error) = slot.capture_endpoints(&request.scope, &self.inner) {
            admission.reopen_if_running();
            return Err(BackendError::new(
                ErrorKind::OperationFailed,
                error.to_string(),
            ));
        }
        let ticket = ProfileQuiesceTicket::new(
            request.scope.clone(),
            slot.clone(),
            self.inner.lifecycle.clone(),
        );
        self.inner.handover_pending.store(true, Ordering::Release);
        match self
            .inner
            .commands
            .try_send(RuntimeMessage::ProfileQuiesce {
                request,
                slot: slot.clone(),
                admission,
            }) {
            Ok(()) => {
                *self
                    .inner
                    .profile_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(slot);
                Ok(ticket)
            }
            Err(error) => {
                self.inner.handover_pending.store(false, Ordering::Release);
                let message = match error {
                    mpsc::TrySendError::Full(message)
                    | mpsc::TrySendError::Disconnected(message) => message,
                };
                if let RuntimeMessage::ProfileQuiesce { admission, .. } = message {
                    admission.reopen_if_running();
                }
                Err(BackendError::new(
                    ErrorKind::Busy,
                    "Profile quiesce request was not admitted to the actor",
                ))
            }
        }
    }
    pub fn shutdown(&self, timeout: Duration) -> Result<(), BackendError> {
        let admission = self
            .inner
            .profile_admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner
            .mutation_admission_closed
            .store(true, Ordering::Release);
        let _ = self.inner.commands.try_send(RuntimeMessage::Wake);
        self.inner
            .sync_admission_closed
            .store(true, Ordering::Release);
        self.inner
            .external_admission_closed
            .store(true, Ordering::Release);
        drop(admission);
        let deadline = Instant::now() + timeout;
        let sync_endpoints = self
            .inner
            .sync_endpoints
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for endpoint in &sync_endpoints {
            endpoint.begin_shutdown();
        }
        let endpoints = self
            .inner
            .external_endpoints
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for endpoint in &endpoints {
            endpoint.begin_shutdown();
        }
        for endpoint in &endpoints {
            if !endpoint.wait(deadline.saturating_duration_since(Instant::now()))? {
                return Err(BackendError::new(
                    ErrorKind::Timeout,
                    "External endpoint shutdown exceeded its deadline",
                ));
            }
        }
        for endpoint in &sync_endpoints {
            if !endpoint.wait(deadline.saturating_duration_since(Instant::now()))? {
                return Err(BackendError::new(
                    ErrorKind::Timeout,
                    "Sync endpoint shutdown exceeded its deadline",
                ));
            }
        }
        self.inner.shutdown.store(true, Ordering::Release);
        let _ = self.inner.commands.try_send(RuntimeMessage::Wake);
        loop {
            if self
                .inner
                .lifecycle
                .poll()
                .map_err(|error| BackendError::new(ErrorKind::OperationFailed, error.to_string()))?
            {
                return self
                    .inner
                    .shutdown_outcome
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| {
                        Err(BackendError::new(
                            ErrorKind::OperationFailed,
                            "Backend stopped without a shutdown outcome",
                        ))
                    });
            }
            if Instant::now() >= deadline {
                return Err(BackendError::new(
                    ErrorKind::Timeout,
                    "Backend shutdown exceeded its deadline; the lifecycle still owns its runtime",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct TrackPage {
    pub revision: u64,
    pub total: usize,
    pub offset: usize,
    pub items: Vec<TrackSummary>,
}

pub fn default_data_dir(portable: bool) -> anyhow::Result<PathBuf> {
    data_paths::resolve(portable)
}
