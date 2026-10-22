//! Provider-backed deep-link metadata and guarded temporary-queue effects.
//! Metadata results never establish source, media-URL or local-file authority.

use super::*;
use crate::{
    domain::{
        list_tools::{DEFAULT_LIST_LOCALE, IcuTextComparator, TextComparator},
        lists::TEMP_LIST,
    },
    open_api::DeepLinkAction,
    platforms::{Collection, Page, Playlist},
};
use serde::Serialize;
use std::io::{self, Write};

const MAX_PENDING: usize = 4;
const MAX_RESULTS: usize = 32;
const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
const MAX_DATA_BYTES: usize = MAX_RESULT_BYTES - 32 * 1024;
const MAX_QUERY_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PLAYLIST_TRACKS: usize = 50_000;
const PLAYLIST_PAGE_SIZE: u32 = 1000;
const SEARCH_PAGE_SIZE: u32 = 25;
const SOURCES: [Platform; 5] = [
    Platform::Kw,
    Platform::Kg,
    Platform::Tx,
    Platform::Wy,
    Platform::Mg,
];

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeepLinkOperationState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeepLinkSearchPage {
    pub platform: Platform,
    pub page: Option<Page<TrackRecord>>,
    pub error: Option<ProviderError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeepLinkSonglistDetails {
    pub platform: Platform,
    pub requested_link: String,
    pub info: Playlist,
    pub tracks: Vec<TrackRecord>,
    pub reported_total: u64,
    pub pages_loaded: u32,
    pub complete: bool,
    pub selected_index: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeepLinkData {
    Search {
        keywords: String,
        requested_source: Option<String>,
        native_default_query: bool,
        pages: Vec<DeepLinkSearchPage>,
    },
    SearchPlay {
        candidates: Vec<TrackRecord>,
        selected: Option<TrackRecord>,
        play_later: bool,
        failures: Vec<DeepLinkSearchPage>,
    },
    Songlist(DeepLinkSonglistDetails),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeepLinkEffect {
    pub temporary_list_applied: bool,
    pub queue_enqueued: bool,
    pub playback_requested: bool,
    pub early_page_applied: bool,
    pub playback_operation_id: Option<String>,
    pub playback_error: Option<BackendError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeepLinkOperationResult {
    pub operation_id: String,
    pub action: String,
    pub state: DeepLinkOperationState,
    pub data: Option<Arc<DeepLinkData>>,
    pub effect: DeepLinkEffect,
    pub error: Option<BackendError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeepLinkProviderPageInfo {
    pub platform: Platform,
    pub fetched: usize,
    pub reported_total: Option<u64>,
    pub error: Option<ProviderError>,
}
#[derive(Clone, Debug, Serialize)]
pub struct DeepLinkPlaylistMetadata {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub play_count: Option<u64>,
    pub track_count: Option<u64>,
    pub display_metadata_truncated: bool,
    /// Raw provider extra data remains inside the bounded internal result; it is
    /// omitted here because some providers embed the complete song array in it.
    pub extra_cached: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeepLinkPageMetadata {
    Search {
        keywords: String,
        requested_source: Option<String>,
        native_default_query: bool,
        providers: Vec<DeepLinkProviderPageInfo>,
    },
    SearchPlay {
        selected_track_id: Option<String>,
        play_later: bool,
        failures: Vec<DeepLinkProviderPageInfo>,
    },
    Songlist {
        platform: Platform,
        requested_link: String,
        info: DeepLinkPlaylistMetadata,
        reported_total: u64,
        pages_loaded: u32,
        complete: bool,
        selected_index: Option<usize>,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct DeepLinkOperationPage {
    pub operation_id: String,
    pub action: String,
    pub state: DeepLinkOperationState,
    pub effect: DeepLinkEffect,
    pub error: Option<BackendError>,
    pub metadata: Option<DeepLinkPageMetadata>,
    /// Number of cached, actually fetched tracks across all searched providers.
    /// This is distinct from each provider's remote reported catalog total.
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: usize,
    pub has_more: bool,
    pub tracks: Vec<TrackRecord>,
}

pub(super) enum DeepLinkCompletion {
    FirstPage(DeepLinkSonglistDetails),
    Finished(Result<DeepLinkData, ProviderError>),
}

#[derive(Clone)]
struct EffectFence {
    temp_fingerprint: String,
    temp_mutation_sequence: u64,
    queue_revision: u64,
    source_epoch: u64,
    generation: u64,
}
struct DeepLinkJob {
    action: DeepLinkAction,
    context: ProviderContext,
    origin: String,
    fence: EffectFence,
    effect: DeepLinkEffect,
    terminal_reported: bool,
}
#[derive(Default)]
pub(super) struct DeepLinkRuntime {
    pending: HashMap<String, DeepLinkJob>,
    results: VecDeque<(Arc<DeepLinkOperationResult>, usize)>,
    result_bytes: usize,
    temp_mutation_sequence: u64,
}
impl DeepLinkRuntime {
    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }
    pub(super) fn begin_shutdown(&mut self) {
        for job in self.pending.values() {
            job.context.cancel.store(true, Ordering::Release);
        }
    }
    /// Call after an actual list transaction, so an identical-content TEMP edit
    /// still fences an older request. Unrelated settings/list changes do not.
    pub(super) fn note_temp_mutation(&mut self, action: &ListAction) {
        let touches = match action {
            ListAction::DataOverwrite { data } => data.temp_list.is_some(),
            ListAction::MusicOverwrite { list_id, .. }
            | ListAction::MusicAdd { list_id, .. }
            | ListAction::MusicRemove { list_id, .. }
            | ListAction::MusicUpdatePosition { list_id, .. } => list_id == TEMP_LIST,
            ListAction::MusicMove { from_id, to_id, .. } => {
                from_id == TEMP_LIST || to_id == TEMP_LIST
            }
            ListAction::MusicUpdate { updates } => {
                updates.iter().any(|update| update.list_id == TEMP_LIST)
            }
            ListAction::MusicClear { list_ids } => list_ids.iter().any(|id| id == TEMP_LIST),
            ListAction::Update { lists } => lists.iter().any(|list| list.id == TEMP_LIST),
            _ => false,
        };
        if touches {
            self.temp_mutation_sequence = self.temp_mutation_sequence.saturating_add(1);
        }
    }
    pub(super) fn result(&self, operation: &str) -> Option<Arc<DeepLinkOperationResult>> {
        self.results
            .iter()
            .find(|(result, _)| result.operation_id == operation)
            .map(|(result, _)| result.clone())
    }
    fn remove_result(&mut self, operation: &str) {
        if let Some(index) = self
            .results
            .iter()
            .position(|(result, _)| result.operation_id == operation)
        {
            let (_, bytes) = self.results.remove(index).unwrap();
            self.result_bytes -= bytes;
        }
    }
    fn cache(&mut self, result: DeepLinkOperationResult) -> Result<()> {
        let bytes = json_size(&result, MAX_RESULT_BYTES, None)?;
        if let Some(index) = self
            .results
            .iter()
            .position(|(previous, _)| previous.operation_id == result.operation_id)
        {
            let (_, old) = self.results.remove(index).unwrap();
            self.result_bytes -= old;
        }
        while self.results.len() >= MAX_RESULTS
            || bytes > MAX_RESULT_BYTES.saturating_sub(self.result_bytes)
        {
            let index = self
                .results
                .iter()
                .position(|(result, _)| result.state != DeepLinkOperationState::Running)
                .context("Deep-link result capacity is occupied by active operations")?;
            let (_, old) = self.results.remove(index).unwrap();
            self.result_bytes -= old;
        }
        self.result_bytes += bytes;
        self.results.push_back((Arc::new(result), bytes));
        Ok(())
    }
}

impl Runtime {
    pub(super) fn begin_deeplink_provider(&mut self, action: DeepLinkAction) -> Result<()> {
        ensure!(
            matches!(
                action,
                DeepLinkAction::Search { .. }
                    | DeepLinkAction::SearchPlay { .. }
                    | DeepLinkAction::SonglistOpen { .. }
                    | DeepLinkAction::SonglistPlay { .. }
            ),
            "Deep-link action has no provider workflow"
        );
        ensure!(
            self.deep_links.pending_count() < MAX_PENDING
                && self.provider_jobs.len() + self.deep_links.pending_count()
                    < PROVIDER_JOB_CAPACITY
                && self.provider_tasks.len() < PROVIDER_JOB_CAPACITY,
            BackendError::new(ErrorKind::Busy, "Too many pending provider operations")
        );
        let operation = self.request_id.clone();
        ensure!(
            !self.deep_links.pending.contains_key(&operation),
            "Deep-link operation is already pending"
        );
        let timeout = self.provider_timeout.min(Duration::from_secs(120));
        let context = ProviderContext::new(timeout);
        let fence = self.capture_deeplink_fence()?;
        self.deep_links.cache(DeepLinkOperationResult {
            operation_id: operation.clone(),
            action: action_name(&action).into(),
            state: DeepLinkOperationState::Running,
            data: None,
            effect: DeepLinkEffect::default(),
            error: None,
        })?;
        let registry = self.provider_registry.clone();
        let completed = self.completed_tx.clone();
        let worker_action = action.clone();
        let worker_context = context.clone();
        let worker_operation = operation.clone();
        let worker = thread::Builder::new()
            .name("lx-deeplink-provider".into())
            .spawn(move || {
                let first_page = |details: DeepLinkSonglistDetails| {
                    worker_context.check()?;
                    completed
                        .send(Completion::DeepLink {
                            operation: worker_operation.clone(),
                            event: DeepLinkCompletion::FirstPage(details),
                        })
                        .map_err(|_| ProviderError::Cancelled)
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute_provider_workflow(
                        &registry,
                        &worker_action,
                        &worker_context,
                        &first_page,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(ProviderError::upstream(
                        "Deep-link provider worker panicked",
                    ))
                });
                let result = worker_context
                    .check()
                    .and(result)
                    .map_err(bounded_provider_error);
                // Terminal delivery is attempted even after cancellation/deadline.
                let _ = completed.send(Completion::DeepLink {
                    operation: worker_operation,
                    event: DeepLinkCompletion::Finished(result),
                });
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                self.deep_links.remove_result(&operation);
                return Err(error.into());
            }
        };
        self.deep_links.pending.insert(
            operation.clone(),
            DeepLinkJob {
                action: action.clone(),
                context,
                origin: self.origin.clone(),
                fence,
                effect: DeepLinkEffect::default(),
                terminal_reported: false,
            },
        );
        self.provider_tasks.push(worker);
        Ok(())
    }

    pub(super) fn deeplink_result_page(
        &self,
        operation: &str,
        offset: usize,
        limit: usize,
    ) -> Result<DeepLinkOperationPage> {
        ensure!(
            (1..=1000).contains(&limit),
            "Deep-link result page must contain1..=1000 tracks"
        );
        let result = self
            .deep_links
            .result(operation)
            .context("Deep-link operation is unknown or evicted")?;
        let mut rows: Box<dyn Iterator<Item = &TrackRecord> + '_> = Box::new(std::iter::empty());
        let mut total = 0usize;
        let mut metadata = None;
        if let Some(data) = result.data.as_deref() {
            match data {
                DeepLinkData::Search {
                    keywords,
                    requested_source,
                    native_default_query,
                    pages,
                } => {
                    let fetched_rows = pages
                        .iter()
                        .filter_map(|page| page.page.as_ref())
                        .flat_map(|page| &page.items);
                    total = fetched_rows.clone().count();
                    rows = Box::new(fetched_rows);
                    metadata = Some(DeepLinkPageMetadata::Search {
                        keywords: keywords.clone(),
                        requested_source: requested_source.clone(),
                        native_default_query: *native_default_query,
                        providers: pages.iter().map(search_page_info).collect(),
                    });
                }
                DeepLinkData::SearchPlay {
                    candidates,
                    selected,
                    play_later,
                    failures,
                } => {
                    total = candidates.len();
                    rows = Box::new(candidates.iter());
                    metadata = Some(DeepLinkPageMetadata::SearchPlay {
                        selected_track_id: selected.as_ref().map(|track| track.id.clone()),
                        play_later: *play_later,
                        failures: failures.iter().map(search_page_info).collect(),
                    });
                }
                DeepLinkData::Songlist(details) => {
                    total = details.tracks.len();
                    rows = Box::new(details.tracks.iter());
                    metadata = Some(DeepLinkPageMetadata::Songlist {
                        platform: details.platform,
                        requested_link: details.requested_link.clone(),
                        info: playlist_metadata(&details.info),
                        reported_total: details.reported_total,
                        pages_loaded: details.pages_loaded,
                        complete: details.complete,
                        selected_index: details.selected_index,
                    });
                }
            }
        }
        let mut page = DeepLinkOperationPage {
            operation_id: result.operation_id.clone(),
            action: result.action.clone(),
            state: result.state,
            effect: result.effect.clone(),
            error: result.error.clone(),
            metadata,
            total,
            offset,
            limit,
            next_offset: offset.min(total),
            has_more: offset < total,
            tracks: Vec::new(),
        };
        // Size borrowed records first. Reserve small cursor/envelope digit growth;
        // clone only the records actually admitted to this response page.
        let mut bytes = json_size(&page, MAX_QUERY_PAGE_BYTES, None).map_err(|_| {
            BackendError::new(
                ErrorKind::Busy,
                "Deep-link query metadata exceeds its2MiB page budget",
            )
        })? + 128;
        for record in rows.skip(offset).take(limit) {
            let record_bytes = json_size(record, MAX_QUERY_PAGE_BYTES, None).map_err(|_| {
                BackendError::new(
                    ErrorKind::Busy,
                    "Deep-link record exceeds its2MiB page budget",
                )
            })?;
            let added = record_bytes + usize::from(!page.tracks.is_empty());
            if added > MAX_QUERY_PAGE_BYTES.saturating_sub(bytes) {
                ensure!(
                    !page.tracks.is_empty(),
                    BackendError::new(
                        ErrorKind::Busy,
                        "Deep-link record cannot fit the2MiB response quota"
                    )
                );
                break;
            }
            bytes += added;
            page.tracks.push(record.clone());
        }
        page.next_offset = offset.saturating_add(page.tracks.len()).min(total);
        page.has_more = page.next_offset < total;
        json_size(&page, MAX_QUERY_PAGE_BYTES, None).map_err(|_| {
            BackendError::new(
                ErrorKind::Busy,
                "Deep-link response exceeds its2MiB page budget",
            )
        })?;
        Ok(page)
    }

    fn capture_deeplink_fence(&self) -> Result<EffectFence> {
        Ok(EffectFence {
            temp_fingerprint: temp_fingerprint(&self.repository)?,
            temp_mutation_sequence: self.deep_links.temp_mutation_sequence,
            queue_revision: self.queue.revision,
            source_epoch: self.source_epoch,
            generation: self.generation,
        })
    }
    fn check_deeplink_fence(&self, job: &DeepLinkJob) -> Result<()> {
        job.context.check()?;
        ensure!(
            job.fence.temp_mutation_sequence == self.deep_links.temp_mutation_sequence
                && job.fence.queue_revision == self.queue.revision
                && job.fence.source_epoch == self.source_epoch
                && job.fence.generation == self.generation
                && job.fence.temp_fingerprint == temp_fingerprint(&self.repository)?,
            BackendError::new(
                ErrorKind::Conflict,
                "Deep-link temporary list, queue or playback context changed"
            )
        );
        Ok(())
    }

    pub(super) fn complete_deeplink_provider(
        &mut self,
        operation: String,
        event: DeepLinkCompletion,
    ) {
        let Some(mut job) = self.deep_links.pending.remove(&operation) else {
            return;
        };
        if job.terminal_reported {
            if matches!(event, DeepLinkCompletion::FirstPage(_)) {
                self.deep_links.pending.insert(operation, job);
            }
            return;
        }
        let saved_id = self.request_id.clone();
        let saved_origin = self.origin.clone();
        self.request_id = operation.clone();
        self.origin = job.origin.clone();
        let result = match event {
            DeepLinkCompletion::FirstPage(details) => {
                let result = (|| -> Result<()> {
                    let data = Arc::new(DeepLinkData::Songlist(details.clone()));
                    self.deep_links.cache(DeepLinkOperationResult {
                        operation_id: operation.clone(),
                        action: action_name(&job.action).into(),
                        state: DeepLinkOperationState::Running,
                        data: Some(data.clone()),
                        effect: job.effect.clone(),
                        error: None,
                    })?;
                    if let DeepLinkAction::SonglistPlay { index, .. } = job.action
                        && (index.is_none() || details.tracks.len() > index.unwrap_or(0))
                    {
                        self.check_deeplink_fence(&job)?;
                        self.apply_deeplink_songlist(&operation, &details, true, &mut job.effect)?;
                        job.fence = self.capture_deeplink_fence()?;
                    }
                    self.deep_links.cache(DeepLinkOperationResult {
                        operation_id: operation.clone(),
                        action: action_name(&job.action).into(),
                        state: DeepLinkOperationState::Running,
                        data: Some(data),
                        effect: job.effect.clone(),
                        error: None,
                    })?;
                    Ok(())
                })();
                if let Err(error) = result {
                    job.context.cancel.store(true, Ordering::Release);
                    job.terminal_reported = true;
                    self.cache_deeplink_terminal(&operation, &job, None, Err(error));
                }
                self.deep_links.pending.insert(operation.clone(), job);
                None
            }
            DeepLinkCompletion::Finished(result) => {
                let result = result.map_err(anyhow::Error::from).and_then(|data| {
                    job.context.check()?;
                    let data = Arc::new(data);
                    self.deep_links.cache(DeepLinkOperationResult {
                        operation_id: operation.clone(),
                        action: action_name(&job.action).into(),
                        state: DeepLinkOperationState::Running,
                        data: Some(data.clone()),
                        effect: job.effect.clone(),
                        error: None,
                    })?;
                    match data.as_ref() {
                        DeepLinkData::Search { .. } => (),
                        DeepLinkData::SearchPlay {
                            selected,
                            play_later,
                            ..
                        } => {
                            if let Some(record) = selected {
                                self.check_deeplink_fence(&job)?;
                                self.apply_deeplink_selected(
                                    &operation,
                                    record,
                                    *play_later,
                                    &mut job.effect,
                                )?;
                            }
                        }
                        DeepLinkData::Songlist(details) => {
                            if matches!(job.action, DeepLinkAction::SonglistPlay { .. }) {
                                self.check_deeplink_fence(&job)?;
                                self.apply_deeplink_songlist(
                                    &operation,
                                    details,
                                    !job.effect.early_page_applied,
                                    &mut job.effect,
                                )?;
                            }
                        }
                    }
                    Ok(data)
                });
                let (data, outcome) = match result {
                    Ok(data) => (Some(data), Ok(())),
                    Err(error) => (None, Err(error)),
                };
                self.cache_deeplink_terminal(&operation, &job, data, outcome);
                Some(())
            }
        };
        self.request_id = saved_id;
        self.origin = saved_origin;
        let _ = result;
    }

    fn cache_deeplink_terminal(
        &mut self,
        operation: &str,
        job: &DeepLinkJob,
        data: Option<Arc<DeepLinkData>>,
        outcome: Result<()>,
    ) {
        let error = outcome.as_ref().err().map(|error| {
            error
                .downcast_ref::<BackendError>()
                .cloned()
                .unwrap_or_else(|| {
                    error
                        .downcast_ref::<ProviderError>()
                        .map(provider_backend_error)
                        .unwrap_or_else(|| {
                            BackendError::new(
                                ErrorKind::OperationFailed,
                                format!("{error:#}").chars().take(2048).collect::<String>(),
                            )
                        })
                })
        });
        let state = if error
            .as_ref()
            .is_some_and(|error| error.kind == ErrorKind::Cancelled)
        {
            DeepLinkOperationState::Cancelled
        } else if error.is_some() {
            DeepLinkOperationState::Failed
        } else {
            DeepLinkOperationState::Completed
        };
        let retained = data.or_else(|| {
            self.deep_links
                .result(operation)
                .and_then(|previous| previous.data.clone())
        });
        let result = DeepLinkOperationResult {
            operation_id: operation.into(),
            action: action_name(&job.action).into(),
            state,
            data: retained,
            effect: job.effect.clone(),
            error,
        };
        if let Err(cache_error) = self.deep_links.cache(result) {
            let _ = self.deep_links.cache(DeepLinkOperationResult {
                operation_id: operation.into(),
                action: action_name(&job.action).into(),
                state: DeepLinkOperationState::Failed,
                data: None,
                effect: job.effect.clone(),
                error: Some(BackendError::new(
                    ErrorKind::Busy,
                    "Deep-link result cache is full",
                )),
            });
            self.error(format!("{cache_error:#}"));
        }
        self.publish_finished(operation.into(), &outcome);
    }

    fn request_deeplink_play(
        &mut self,
        operation: &str,
        command: BackendCommand,
        effect: &mut DeepLinkEffect,
    ) {
        let child = crate::domain::music::stable_id("deeplink-play", operation.as_bytes());
        let previous = self.request_id.clone();
        let origin = self.origin.clone();
        self.request_id = child.clone();
        self.operation_origins.insert(child.clone(), origin.clone());
        effect.playback_operation_id = Some(child.clone());
        match self.handle(command) {
            Ok(_) => effect.playback_requested = true,
            Err(error) => {
                effect.playback_error = Some(
                    error
                        .downcast_ref::<BackendError>()
                        .cloned()
                        .unwrap_or_else(|| {
                            BackendError::new(
                                ErrorKind::OperationFailed,
                                format!("{error:#}").chars().take(2048).collect::<String>(),
                            )
                        }),
                );
                self.publish_finished(child, &Err(error));
            }
        }
        self.request_id = previous;
        self.origin = origin;
    }
    fn apply_deeplink_selected(
        &mut self,
        operation: &str,
        record: &TrackRecord,
        play_later: bool,
        effect: &mut DeepLinkEffect,
    ) -> Result<()> {
        let had_current = self.queue.current.is_some() || self.current_record.is_some();
        self.revision = self.repository.revision()?;
        self.apply_lists(&ListAction::MusicAdd {
            list_id: TEMP_LIST.into(),
            tracks: vec![record.clone()],
            location: AddLocation::Bottom,
        })?;
        effect.temporary_list_applied = true;
        self.enqueue_refs(
            vec![QueueItemRef {
                list_id: TEMP_LIST.into(),
                track_id: record.id.clone(),
            }],
            !play_later,
        )?;
        effect.queue_enqueued = true;
        if !play_later || !had_current {
            self.request_deeplink_play(operation, BackendCommand::Next, effect);
        }
        Ok(())
    }
    fn apply_deeplink_songlist(
        &mut self,
        operation: &str,
        details: &DeepLinkSonglistDetails,
        request_play: bool,
        effect: &mut DeepLinkEffect,
    ) -> Result<()> {
        let action = ListAction::MusicOverwrite {
            list_id: TEMP_LIST.into(),
            tracks: details.tracks.clone(),
        };
        crate::repository::encode_document(&action)?;
        self.revision = self.repository.revision()?;
        self.apply_lists(&action)?;
        effect.temporary_list_applied = true;
        if !details.complete {
            effect.early_page_applied = true;
        }
        if request_play
            && let Some(index) = details.selected_index
            && let Some(record) = details.tracks.get(index)
        {
            self.request_deeplink_play(
                operation,
                BackendCommand::PlayListTrack {
                    list_id: TEMP_LIST.into(),
                    music_id: record.id.clone(),
                    quality: None,
                },
                effect,
            );
        }
        Ok(())
    }
    pub(super) fn cancel_deeplink_provider(&mut self, operation: &str) -> bool {
        let Some(mut job) = self.deep_links.pending.remove(operation) else {
            return false;
        };
        job.context.cancel.store(true, Ordering::Release);
        if !job.terminal_reported {
            job.terminal_reported = true;
            self.cache_deeplink_terminal(
                operation,
                &job,
                None,
                Err(ProviderError::Cancelled.into()),
            );
        }
        self.deep_links.pending.insert(operation.into(), job);
        true
    }
}

fn action_name(action: &DeepLinkAction) -> &'static str {
    match action {
        DeepLinkAction::Search { .. } => "search",
        DeepLinkAction::SearchPlay { .. } => "search_play",
        DeepLinkAction::SonglistOpen { .. } => "songlist_open",
        DeepLinkAction::SonglistPlay { .. } => "songlist_play",
        _ => "unsupported",
    }
}
fn platform(source: &str) -> Result<Platform, ProviderError> {
    SOURCES
        .into_iter()
        .find(|platform| platform.as_str() == source)
        .ok_or_else(|| ProviderError::invalid("Unsupported deep-link provider"))
}
fn temp_fingerprint(repository: &Repository) -> Result<String> {
    let records = repository.list_tracks(TEMP_LIST)?;
    ensure!(
        records.len() <= MAX_PLAYLIST_TRACKS,
        "Temporary list exceeds deep-link effect budget"
    );
    let info = repository
        .list_infos()?
        .into_iter()
        .find(|info| info.id == TEMP_LIST)
        .context("Temporary list is missing")?;
    let mut hash = Sha256::new();
    hash.update(crate::repository::encode_document(&info)?.as_bytes());
    for record in records {
        hash.update(crate::repository::encode_document(&record)?.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn execute_provider_workflow(
    registry: &ProviderRegistry,
    action: &DeepLinkAction,
    context: &ProviderContext,
    first_page: &dyn Fn(DeepLinkSonglistDetails) -> Result<(), ProviderError>,
) -> Result<DeepLinkData, ProviderError> {
    context.check()?;
    match action {
        DeepLinkAction::Search { keywords, source } => {
            let providers = if source.as_deref() == Some("all") {
                SOURCES.to_vec()
            } else {
                vec![
                    source
                        .as_deref()
                        .map(platform)
                        .transpose()?
                        .unwrap_or(Platform::Kw),
                ]
            };
            let pages = search_pages(registry, keywords, &providers, context)?;
            Ok(DeepLinkData::Search {
                keywords: keywords.clone(),
                requested_source: source.clone(),
                native_default_query: source.is_none(),
                pages,
            })
        }
        DeepLinkAction::SearchPlay {
            name,
            singer,
            album_name,
            interval,
            play_later,
        } => {
            let keyword = format!(
                "{} {}",
                crate::domain::lists::trim_dislike(name),
                crate::domain::lists::trim_dislike(singer)
            )
            .trim()
            .to_owned();
            let pages = search_pages(registry, &keyword, &SOURCES, context)?;
            let candidates = select_search_play(
                name,
                singer,
                album_name,
                interval.as_deref().filter(|value| !value.is_empty()),
                &pages,
            )?;
            let selected = candidates.first().cloned();
            let failures = pages
                .into_iter()
                .filter(|page| page.error.is_some())
                .collect();
            Ok(DeepLinkData::SearchPlay {
                candidates,
                selected,
                play_later: *play_later,
                failures,
            })
        }
        DeepLinkAction::SonglistOpen { source, id, url }
        | DeepLinkAction::SonglistPlay {
            source, id, url, ..
        } => {
            let source = platform(source)?;
            let link = id
                .as_ref()
                .or(url.as_ref())
                .ok_or_else(|| ProviderError::invalid("Deep-link playlist id or URL is missing"))?;
            let index = match action {
                DeepLinkAction::SonglistPlay { index, .. } => *index,
                _ => None,
            };
            let first = fetch_collection(registry, source, link, 1, context)?;
            let total = first.tracks.total;
            if total > MAX_PLAYLIST_TRACKS as u64 {
                return Err(ProviderError::invalid(
                    "Playlist exceeds the 50000-track budget",
                ));
            }
            let max_page = total.div_ceil(PLAYLIST_PAGE_SIZE as u64).max(1) as u32;
            let info = first.info;
            let mut tracks = deduplicate(first.tracks.items);
            let mut seen: HashSet<_> = tracks.iter().map(|track| track.lx_id.clone()).collect();
            let play = matches!(action, DeepLinkAction::SonglistPlay { .. });
            let early_index = if play && (index.is_none() || tracks.len() > index.unwrap_or(0)) {
                selected_index(tracks.len(), index)
            } else {
                None
            };
            let details = DeepLinkSonglistDetails {
                platform: source,
                requested_link: link.clone(),
                info: info.clone(),
                selected_index: early_index,
                reported_total: total,
                tracks: tracks.clone(),
                pages_loaded: 1,
                complete: max_page == 1,
            };
            json_size(&details, MAX_DATA_BYTES, Some(context))?;
            if max_page > 1 && matches!(action, DeepLinkAction::SonglistPlay { .. }) {
                first_page(details)?;
            }
            for page in 2..=max_page {
                let next = fetch_collection(registry, source, link, page, context)?;
                if next.info.id != info.id || next.tracks.total != total {
                    return Err(ProviderError::parse(
                        "Playlist identity or total changed during pagination",
                    ));
                }
                let before = tracks.len();
                for track in next.tracks.items {
                    if seen.insert(track.lx_id.clone()) {
                        tracks.push(track);
                    }
                }
                if tracks.len() == before {
                    return Err(ProviderError::parse(
                        "Playlist pagination repeated an already received page",
                    ));
                }
                if tracks.len() > MAX_PLAYLIST_TRACKS {
                    return Err(ProviderError::invalid(
                        "Playlist exceeds the 50000-track budget",
                    ));
                }
                json_size(&tracks, MAX_DATA_BYTES, Some(context))?;
            }
            let details = DeepLinkSonglistDetails {
                platform: source,
                requested_link: link.clone(),
                info,
                selected_index: if play {
                    selected_index(tracks.len(), index)
                } else {
                    None
                },
                reported_total: total,
                tracks,
                pages_loaded: max_page,
                complete: true,
            };
            json_size(&details, MAX_DATA_BYTES, Some(context))?;
            Ok(DeepLinkData::Songlist(details))
        }
        _ => Err(ProviderError::invalid(
            "Deep-link action has no provider workflow",
        )),
    }
}

fn search_pages(
    registry: &ProviderRegistry,
    keyword: &str,
    providers: &[Platform],
    context: &ProviderContext,
) -> Result<Vec<DeepLinkSearchPage>, ProviderError> {
    let mut pages = Vec::new();
    for source in providers {
        context.check()?;
        let response = registry.execute(
            *source,
            ProviderRequest::Search {
                keyword: keyword.into(),
                page: 1,
                limit: SEARCH_PAGE_SIZE,
            },
            context,
        );
        let response = response.and_then(|response| {
            let ProviderResponse::Tracks(page) = response else {
                return Err(ProviderError::parse(
                    "Deep-link search returned unexpected metadata",
                ));
            };
            let maximum = if *source == Platform::Kg {
                100
            } else {
                SEARCH_PAGE_SIZE as usize
            };
            if page.page != 1
                || page.limit != SEARCH_PAGE_SIZE
                || page.items.len() > maximum
                || page.items.iter().any(|track| {
                    track.platform.source != source.as_str()
                        || track.local_asset_id.is_some()
                        || track.title.is_empty()
                })
            {
                return Err(ProviderError::parse(
                    "Deep-link search page metadata is inconsistent",
                ));
            }
            json_size(&page, MAX_DATA_BYTES, Some(context))?;
            Ok(page)
        });
        match response {
            Ok(page) => pages.push(DeepLinkSearchPage {
                platform: *source,
                page: Some(page),
                error: None,
            }),
            Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
            Err(ProviderError::DeadlineExceeded) => return Err(ProviderError::DeadlineExceeded),
            Err(error) => pages.push(DeepLinkSearchPage {
                platform: *source,
                page: None,
                error: Some(bounded_provider_error(error)),
            }),
        }
        json_size(&pages, MAX_DATA_BYTES, Some(context))?;
    }
    if pages.iter().all(|page| page.page.is_none()) {
        return Err(ProviderError::upstream(
            "All deep-link metadata providers failed",
        ));
    }
    Ok(pages)
}
fn fetch_collection(
    registry: &ProviderRegistry,
    source: Platform,
    link: &str,
    page: u32,
    context: &ProviderContext,
) -> Result<Collection, ProviderError> {
    context.check()?;
    let response = registry.execute(
        source,
        ProviderRequest::PlaylistDetail {
            id: link.into(),
            page,
            limit: PLAYLIST_PAGE_SIZE,
        },
        context,
    )?;
    let ProviderResponse::Collection(collection) = response else {
        return Err(ProviderError::parse(
            "Deep-link playlist returned unexpected metadata",
        ));
    };
    if collection.tracks.page != page
        || collection.tracks.limit != PLAYLIST_PAGE_SIZE
        || collection.tracks.items.len() > PLAYLIST_PAGE_SIZE as usize
        || collection.tracks.items.iter().any(|track| {
            track.platform.source != source.as_str()
                || track.local_asset_id.is_some()
                || track.title.is_empty()
        })
        || collection.info.id.is_empty()
    {
        return Err(ProviderError::parse(
            "Deep-link playlist page metadata is inconsistent",
        ));
    }
    if collection.tracks.items.is_empty()
        && (page - 1) as u64 * (PLAYLIST_PAGE_SIZE as u64) < collection.tracks.total
    {
        return Err(ProviderError::parse(
            "Playlist pagination ended before its declared total",
        ));
    }
    json_size(&collection, MAX_DATA_BYTES, Some(context))?;
    Ok(collection)
}
fn selected_index(length: usize, requested: Option<usize>) -> Option<usize> {
    (length != 0).then(|| requested.unwrap_or(1).max(1).min(length) - 1)
}
fn deduplicate(tracks: Vec<TrackRecord>) -> Vec<TrackRecord> {
    let mut seen = HashSet::new();
    tracks
        .into_iter()
        .filter(|track| seen.insert(track.lx_id.clone()))
        .collect()
}

struct SizeWriter<'a> {
    bytes: usize,
    maximum: usize,
    context: Option<&'a ProviderContext>,
}
impl Write for SizeWriter<'_> {
    fn write(&mut self, value: &[u8]) -> io::Result<usize> {
        if let Some(context) = self.context {
            context
                .check()
                .map_err(|_| io::Error::other("Deep-link provider cancelled or expired"))?;
        }
        if value.len() > self.maximum.saturating_sub(self.bytes) {
            return Err(io::Error::other("Deep-link result budget exhausted"));
        }
        self.bytes += value.len();
        Ok(value.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn json_size(
    value: &impl Serialize,
    maximum: usize,
    context: Option<&ProviderContext>,
) -> Result<usize, ProviderError> {
    let mut writer = SizeWriter {
        bytes: 0,
        maximum,
        context,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| ProviderError::ResponseTooLarge { limit: maximum })?;
    if let Some(context) = context {
        context.check()?;
    }
    Ok(writer.bytes)
}

fn select_search_play(
    name: &str,
    singer: &str,
    album: &str,
    interval: Option<&str>,
    pages: &[DeepLinkSearchPage],
) -> Result<Vec<TrackRecord>, ProviderError> {
    let comparator = IcuTextComparator::new(DEFAULT_LIST_LOCALE)
        .map_err(|error| ProviderError::invalid(error.to_string()))?;
    let wanted_name = filter_match_text(name).to_lowercase();
    let wanted_singer = sorted_singer(singer, &comparator);
    let wanted_album = filter_match_text(album).to_lowercase();
    let wanted_duration = duration_seconds(interval);
    let mut selected = Vec::new();
    for page in pages {
        let Some(page) = &page.page else {
            continue;
        };
        let records: Vec<_> = page
            .items
            .iter()
            .filter_map(|track| {
                let interval = track
                    .to_lx_dto(crate::domain::music::LxMusicDtoVersion::Modern)
                    .get("interval")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let duration = duration_seconds(interval.as_deref());
                let left = if wanted_duration == 0.0 || wanted_duration.is_nan() {
                    duration
                } else {
                    wanted_duration
                };
                let right = if duration == 0.0 || duration.is_nan() {
                    wanted_duration
                } else {
                    duration
                };
                if (left - right)
                    .abs()
                    .partial_cmp(&5.0)
                    .is_none_or(|order| order.is_gt())
                {
                    return None;
                }
                Some(SearchCandidate {
                    track: track.clone(),
                    name: filter_match_text(&track.title).to_lowercase(),
                    singer: sorted_singer(&track.artists.join("、"), &comparator),
                    album: filter_match_text(&track.album).to_lowercase(),
                    interval,
                })
            })
            .collect();
        let singer_contains = |candidate: &SearchCandidate| {
            wanted_singer.is_empty()
                || wanted_singer.contains(&candidate.singer)
                || candidate.singer.contains(&wanted_singer)
        };
        let name_contains = |candidate: &SearchCandidate| {
            (wanted_name.contains(&candidate.name) || candidate.name.contains(&wanted_name))
                && VERSION_WORDS
                    .iter()
                    .all(|word| wanted_name.contains(word) == candidate.name.contains(word))
        };
        let candidate = records
            .iter()
            .find(|candidate| candidate.name == wanted_name && singer_contains(candidate))
            .or_else(|| {
                records
                    .iter()
                    .find(|candidate| candidate.singer == wanted_singer && name_contains(candidate))
            })
            .or_else(|| {
                records.iter().find(|candidate| {
                    (wanted_album.is_empty() || candidate.album == wanted_album)
                        && singer_contains(candidate)
                        && name_contains(candidate)
                })
            });
        if let Some(candidate) = candidate {
            selected.push(candidate.clone());
        }
    }
    let mut ordered = Vec::new();
    for pass in 0..9 {
        let mut index = 0;
        while index < selected.len() {
            let candidate = &selected[index];
            let n = candidate.name == wanted_name;
            let s = candidate.singer == wanted_singer;
            let a = candidate.album == wanted_album;
            let i = candidate.interval.as_deref() == interval;
            let matches = match pass {
                0 => s && n && i,
                1 => n && s && a,
                2 => s && n,
                3 => n && i,
                4 => s && i,
                5 => i,
                6 => n,
                7 => s,
                _ => a,
            };
            if matches {
                ordered.push(selected.remove(index).track);
            } else {
                index += 1;
            }
        }
    }
    ordered.extend(selected.into_iter().map(|candidate| candidate.track));
    Ok(ordered)
}
#[derive(Clone)]
struct SearchCandidate {
    track: TrackRecord,
    name: String,
    singer: String,
    album: String,
    interval: Option<String>,
}
fn sorted_singer(value: &str, comparator: &IcuTextComparator) -> String {
    let mut names: Vec<_> = value
        .split(['、', '&', ';', '；', '/', ',', '，', '|'])
        .collect();
    names.sort_by(|left, right| comparator.compare(left, right));
    filter_match_text(&names.join("、")).to_lowercase()
}
fn filter_match_text(value: &str) -> String {
    value.chars().filter(|character| !matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}' | '\'' | '.' | ',' | '，' | '&' | '"' | '、' | '(' | ')' | '（' | '）' | '`' | '~' | '-' | '<' | '>' | '|' | '/' | ']' | '[' | '!' | '！')).collect()
}
fn duration_seconds(value: Option<&str>) -> f64 {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return 0.0;
    };
    value
        .split(':')
        .fold(0.0, |sum, part| sum * 60.0 + js_integer_prefix(part))
}
fn js_integer_prefix(value: &str) -> f64 {
    let mut value = crate::domain::lists::trim_dislike(value);
    let mut sign = 1.0;
    if let Some(rest) = value.strip_prefix('-') {
        sign = -1.0;
        value = rest;
    } else if let Some(rest) = value.strip_prefix('+') {
        value = rest;
    }
    let (hex, value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or((false, value), |rest| (true, rest));
    let radix = if hex { 16 } else { 10 };
    let mut count = 0;
    let mut number = 0.0;
    for character in value.chars() {
        let Some(digit) = character.to_digit(radix).filter(|_| character.is_ascii()) else {
            break;
        };
        number = number * f64::from(radix) + f64::from(digit);
        count += 1;
    }
    if count == 0 { f64::NAN } else { number * sign }
}
fn search_page_info(page: &DeepLinkSearchPage) -> DeepLinkProviderPageInfo {
    DeepLinkProviderPageInfo {
        platform: page.platform,
        fetched: page.page.as_ref().map_or(0, |page| page.items.len()),
        reported_total: page.page.as_ref().map(|page| page.total),
        error: page.error.clone(),
    }
}
fn clip_display(value: &str, maximum: usize) -> (String, bool) {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].into(), end < value.len())
}
fn playlist_metadata(info: &Playlist) -> DeepLinkPlaylistMetadata {
    let (name, mut truncated) = clip_display(&info.name, 512);
    let image_url = info.image_url.as_deref().map(|value| {
        let (value, cut) = clip_display(value, 2048);
        truncated |= cut;
        value
    });
    let author = info.author.as_deref().map(|value| {
        let (value, cut) = clip_display(value, 512);
        truncated |= cut;
        value
    });
    let description = info.description.as_deref().map(|value| {
        let (value, cut) = clip_display(value, 4096);
        truncated |= cut;
        value
    });
    DeepLinkPlaylistMetadata {
        id: info.id.clone(),
        name,
        image_url,
        author,
        description,
        play_count: info.play_count,
        track_count: info.track_count,
        display_metadata_truncated: truncated,
        extra_cached: !info.extra.is_null(),
    }
}
const VERSION_WORDS: &[&str] = &[
    "instrumental",
    "inst",
    "karaoke",
    "backing track",
    "minus one",
    "minus vocal",
    "no vocal",
    "off vocal",
    "vocal removed",
    "伴奏",
    "伴唱",
    "卡拉ok",
    "纯音乐",
    "无人声",
    "去人声",
    "消音",
    "オフボーカル",
    "インスト",
    "반주",
    "노래방",
    "live",
    "concert",
    "tour",
    "festival",
    "stage",
    "session",
    "现场",
    "演唱会",
    "巡演",
    "音乐会",
    "ライブ",
    "공연",
    "라이브",
    "cover",
    "tribute",
    "rendition",
    "remake",
    "翻唱",
    "致敬",
    "改编",
    "カバー",
    "커버",
    "remix",
    "mix",
    "club",
    "dance",
    "extended",
    "house",
    "edm",
    "dj",
    "dub",
    "trap",
    "混音",
    "重混",
    "电音",
    "リミックス",
    "리믹스",
    "acoustic",
    "unplugged",
    "stripped",
    "原声",
    "不插电",
    "アコースティック",
    "어쿠스틱",
    "demo",
    "小样",
    "试听",
    "デモ",
    "데모",
    "slowed",
    "slowed reverb",
    "sped up",
    "speed up",
    "nightcore",
    "慢速",
    "加速",
    "变速",
    "ナイトコア",
    "piano",
    "guitar",
    "orchestra",
    "symphony",
    "violin",
    "钢琴",
    "吉他",
    "管弦乐",
    "小提琴",
    "ピアノ",
    "ギター",
    "피아노",
    "기타",
];
