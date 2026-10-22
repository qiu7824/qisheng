use super::*;
use crate::domain::media::{
    CachedMediaUrl, DEFAULT_RESOLVED_MEDIA_TTL_MS, DEFAULT_SOURCE_MATCH_TTL_MS,
    MediaInvalidationReason, ResolvedMediaKey, SourceMatchKey, track_record_fingerprint,
};
use crate::platforms::matching::MatchSet;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) struct SourceResolution {
    pub url: String,
    pub identity: CacheIdentity,
    pub cache_entry: Option<ResolvedMedia>,
    pub matches: Option<SourceMatches>,
    pub cache_hit: bool,
    pub attempted_platforms: Vec<String>,
}

pub(super) struct ResolveInputs<'a> {
    pub repository: &'a Repository,
    pub matching: &'a MatchingService,
    pub platforms: Vec<Platform>,
    pub preferred: &'a str,
    pub allow_switch: bool,
    pub timeout: Duration,
    pub deadline: Instant,
    pub identity: CacheIdentity,
    pub original_info: Value,
    pub local_fallback: Option<String>,
    pub cancel: Arc<AtomicBool>,
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}

fn cache_matches(key: SourceMatchKey, found: MatchSet) -> Result<SourceMatches> {
    let ttl = if found.failures.is_empty() {
        DEFAULT_SOURCE_MATCH_TTL_MS
    } else {
        30_000
    };
    let mut value = SourceMatches::new(key, found.candidates, now_ms(), ttl)?;
    value.searched_platforms = found
        .searched_platforms
        .into_iter()
        .map(|platform| platform.as_str().to_owned())
        .collect();
    value.failed_platforms = found
        .failures
        .into_iter()
        .map(|failure| failure.platform.as_str().to_owned())
        .collect();
    value.validate()?;
    Ok(value)
}

fn rate_limited(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::source::SourceRequestFailure>()
        .is_some_and(|failure| failure.kind == crate::source::SourceFailureKind::RateLimited)
}

fn check_cancel(cancelled: &impl Fn() -> bool) -> Result<()> {
    ensure!(!cancelled(), crate::source::SourceRequestCancelled);
    Ok(())
}

fn cancellable_delay(duration: Duration, cancelled: &impl Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        check_cancel(cancelled)?;
        thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    check_cancel(cancelled)
}

pub(super) fn resolution_key(
    original: &TrackRecord,
    resolved: &TrackRecord,
    quality: &str,
    identity: &CacheIdentity,
    original_info: &Value,
) -> Result<ResolvedMediaKey> {
    let info = if original.id == resolved.id && original.platform.source == resolved.platform.source
    {
        original_info.clone()
    } else {
        serde_json::json!({"original":original_info,"resolved":resolved.to_source_music_info()})
    };
    ResolvedMediaKey::new(
        original,
        quality,
        &identity.source_sha256,
        identity.source_policy_version,
        &info,
    )
}

fn resolved_value(
    value: ResolvedMedia,
    mut identity: CacheIdentity,
    matches: Option<SourceMatches>,
) -> Result<SourceResolution> {
    identity.media_context_sha256 = value.key.fingerprint()?;
    identity.actual_quality = value.key.actual_quality.clone();
    if let Some(track) = &value.matched_track {
        identity.platform = track.platform.source.clone();
    }
    let url = value.url.as_str().to_owned();
    identity.url_sha256 = format!("{:x}", Sha256::digest(url.as_bytes()));
    Ok(SourceResolution {
        url,
        identity,
        cache_entry: Some(value),
        matches,
        cache_hit: true,
        attempted_platforms: Vec::new(),
    })
}

pub(super) fn resolve(
    worker: &mut SourceWorker,
    job: &SourceResolveJob,
    inputs: ResolveInputs<'_>,
) -> Result<SourceResolution> {
    let ResolveInputs {
        repository,
        matching,
        platforms,
        preferred,
        allow_switch,
        timeout,
        deadline,
        identity,
        original_info,
        local_fallback,
        cancel,
    } = inputs;
    let cancelled = || {
        cancel.load(Ordering::Acquire)
            || job
                .preload
                .as_ref()
                .is_some_and(|action| action.cancel.is_cancelled())
            || Instant::now() >= deadline
    };
    check_cancel(&cancelled)?;
    if job.record.platform.source == "local"
        && worker
            .manifest()
            .sources
            .get("local")
            .is_none_or(|caps| !caps.actions.iter().any(|action| action == "musicUrl"))
    {
        let url = local_fallback.context("Local track has no catalog asset")?;
        let mut identity = identity;
        identity.actual_quality = "local".into();
        identity.url_sha256 = format!("{:x}", Sha256::digest(url.as_bytes()));
        return Ok(SourceResolution {
            url,
            identity,
            cache_entry: None,
            matches: None,
            cache_hit: false,
            attempted_platforms: Vec::new(),
        });
    }
    let resolved = job.resolved_track.as_ref().unwrap_or(&job.record);
    let quality_result = Runtime::selected_quality(
        resolved,
        worker.manifest(),
        preferred,
        job.strict_quality.as_deref(),
    );
    let quality = match &quality_result {
        Ok(value) => value.clone(),
        Err(_) if allow_switch && job.record.platform.source != "local" => job
            .strict_quality
            .as_deref()
            .unwrap_or(preferred)
            .to_owned(),
        Err(_) => return Err(quality_result.unwrap_err()),
    };
    let match_key = (job.record.platform.source != "local")
        .then(|| SourceMatchKey::new(&job.record))
        .transpose()?;
    let mut matches = match_key
        .as_ref()
        .map(|key| repository.source_matches(key, now_ms()))
        .transpose()?
        .and_then(|lookup| lookup.hit());
    let manifest = worker.manifest().clone();
    let candidate_quality = |candidate: &TrackRecord| {
        Runtime::selected_quality(
            candidate,
            &manifest,
            preferred,
            job.strict_quality.as_deref(),
        )
        .ok()
        .filter(|quality| candidate.platform.qualities.contains(quality))
    };
    if !job.force_refresh && job.record.platform.source != "local" {
        let key = resolution_key(&job.record, resolved, &quality, &identity, &original_info)?;
        if !job.skip_primary
            && !job.excluded_platforms.contains(&resolved.platform.source)
            && quality_result.is_ok()
            && let Some(value) = repository.resolved_media(&key, now_ms())?.hit()
        {
            check_cancel(&cancelled)?;
            return resolved_value(value, identity, matches);
        }
        if allow_switch && let Some(cached_matches) = &matches {
            for candidate in &cached_matches.candidates {
                if job
                    .excluded_platforms
                    .contains(&candidate.track.platform.source)
                {
                    continue;
                }
                let Some(actual_quality) = candidate_quality(&candidate.track) else {
                    continue;
                };
                let key = resolution_key(
                    &job.record,
                    &candidate.track,
                    &actual_quality,
                    &identity,
                    &original_info,
                )?;
                if let Some(value) = repository.resolved_media(&key, now_ms())?.hit() {
                    check_cancel(&cancelled)?;
                    return resolved_value(value, identity, matches);
                }
            }
        }
    }
    let mut attempted_platforms = HashSet::new();
    let mut request_one = |worker: &mut SourceWorker,
                           target: &TrackRecord,
                           actual_quality: &str|
     -> Result<String> {
        ensure!(
            !job.excluded_platforms.contains(&target.platform.source),
            BackendError::new(
                ErrorKind::Unavailable,
                "Source platform already failed for this selection"
            )
        );
        attempted_platforms.insert(target.platform.source.clone());
        let info =
            if target.id == job.record.id && target.platform.source == job.record.platform.source {
                original_info.clone()
            } else {
                target.to_source_music_info()
            };
        let request = SourceRequest {
            source: target.platform.source.clone(),
            action: "musicUrl".into(),
            info: serde_json::json!({"type":if target.platform.source == "local" {Value::Null} else {serde_json::json!(actual_quality)},"musicInfo":info}),
        };
        let mut busy_attempt = 0u8;
        loop {
            check_cancel(&cancelled)?;
            match worker.request_url_cancellable(&request, cancelled) {
                Err(error) if rate_limited(&error) && busy_attempt < 2 => {
                    busy_attempt += 1;
                    cancellable_delay(
                        Duration::from_secs(u64::from(busy_attempt) * 2),
                        &cancelled,
                    )?;
                }
                result => return result,
            }
        }
    };
    let first = if job.skip_primary {
        Err(BackendError::new(
            ErrorKind::Unavailable,
            "Original media platform was rejected",
        )
        .into())
    } else {
        match quality_result {
            Ok(_) => request_one(worker, resolved, &quality),
            Err(error) => Err(error),
        }
    };
    let (target, url, quality) = match first {
        Ok(url) => (resolved.clone(), url, quality),
        Err(error)
            if error.is::<crate::source::SourceRequestCancelled>()
                || rate_limited(&error)
                || !allow_switch
                || job.force_refresh
                || job.record.platform.source == "local" =>
        {
            return Err(error);
        }
        Err(first_error) => {
            if matches.is_none() {
                let context = ProviderContext {
                    cancel: cancel.clone(),
                    deadline: (Instant::now() + timeout).min(deadline),
                };
                let found = matching.search(&job.record, &platforms, &context)?;
                check_cancel(&cancelled)?;
                matches = Some(cache_matches(
                    match_key.clone().context("Missing match identity")?,
                    found,
                )?);
            }
            let mut selected = None;
            let mut attempted = HashSet::from([resolved.platform.source.clone()]);
            for candidate in &matches.as_ref().unwrap().candidates {
                check_cancel(&cancelled)?;
                if job
                    .excluded_platforms
                    .contains(&candidate.track.platform.source)
                {
                    continue;
                }
                let Some(actual_quality) = candidate_quality(&candidate.track) else {
                    continue;
                };
                if !attempted.insert(candidate.track.platform.source.clone()) {
                    continue;
                }
                match request_one(worker, &candidate.track, &actual_quality) {
                    Ok(url) => {
                        selected = Some((candidate.track.clone(), url, actual_quality));
                        break;
                    }
                    Err(error)
                        if error.is::<crate::source::SourceRequestCancelled>()
                            || rate_limited(&error) =>
                    {
                        return Err(error);
                    }
                    Err(_) => (),
                }
            }
            selected.context(first_error)?
        }
    };
    check_cancel(&cancelled)?;
    let mut identity = identity;
    identity.platform = target.platform.source.clone();
    identity.actual_quality = quality.clone();
    identity.url_sha256 = format!("{:x}", Sha256::digest(url.as_bytes()));
    let cache_entry = if job.record.platform.source == "local" {
        None
    } else {
        Some(ResolvedMedia::new(
            resolution_key(&job.record, &target, &quality, &identity, &original_info)?,
            CachedMediaUrl::new(url.clone())?,
            (target.id != job.record.id || target.platform.source != job.record.platform.source)
                .then_some(target),
            now_ms(),
            DEFAULT_RESOLVED_MEDIA_TTL_MS,
        )?)
    };
    if let Some(media) = &cache_entry {
        identity.media_context_sha256 = media.key.fingerprint()?;
    }
    Ok(SourceResolution {
        url,
        identity,
        cache_entry,
        matches,
        cache_hit: false,
        attempted_platforms: attempted_platforms.into_iter().collect(),
    })
}

impl Runtime {
    pub(super) fn current_matched_record(&self, record: &TrackRecord) -> Option<TrackRecord> {
        let media = self.active_media.as_ref()?;
        let matched = media.matched_track.as_ref()?;
        if media.key.original_track_id != record.id
            || media.key.original_platform != record.platform.source
        {
            return None;
        }
        let source = self.data.sources.iter().find(|source| {
            self.data.selected.as_deref() == Some(source.sha256.as_str())
                && source.sha256 == media.key.source_sha256
                && u64::from(source.approval_policy_version) == media.key.source_policy_version
        })?;
        source.require_approval().ok()?;
        let identity = CacheIdentity {
            source_sha256: source.sha256.clone(),
            source_policy_version: u64::from(source.approval_policy_version),
            ..Default::default()
        };
        let key = resolution_key(
            record,
            matched,
            &media.key.actual_quality,
            &identity,
            &self.source_music_info(record),
        )
        .ok()?;
        (key == media.key).then(|| matched.clone())
    }

    pub(super) fn begin_matches(&mut self, list_id: String, music_id: String) -> Result<()> {
        ensure!(
            self.tasks.len() < 4,
            BackendError::new(ErrorKind::Busy, "Too many pending operations")
        );
        let original = self
            .repository
            .list_track(&list_id, &music_id)?
            .context("Track is unavailable")?;
        let key = SourceMatchKey::new(&original)?;
        if self
            .repository
            .source_matches(&key, now_ms())?
            .hit()
            .is_some_and(|matches| matches.failed_platforms.is_empty())
        {
            return Ok(());
        }
        let context = ProviderContext::new(self.provider_timeout);
        let operation = self.request_id.clone();
        let cancel = context.cancel.clone();
        let job_operation = operation.clone();
        let matching = self.matching_service.clone();
        let allowed = self.provider_registry.platforms();
        let tx = self.completed_tx.clone();
        let cache_epoch = self.media_cache_epoch;
        self.tasks.push(
            thread::Builder::new()
                .name("lx-source-matching".into())
                .spawn(move || {
                    let result = matching
                        .search(&original, &allowed, &context)
                        .map_err(anyhow::Error::from)
                        .and_then(|found| cache_matches(key, found));
                    let _ = tx.send(Completion::Matched {
                        operation: job_operation,
                        list_id,
                        original,
                        cache_epoch,
                        result,
                    });
                })?,
        );
        self.file_cancels.insert(operation, cancel);
        Ok(())
    }

    pub(super) fn refresh_media_url(&mut self, automatic: bool) -> Result<()> {
        ensure!(
            !self.busy && self.pending.is_none(),
            "Playback resolution is already pending"
        );
        let record = self
            .current_record
            .as_ref()
            .context("No current track")?
            .as_ref()
            .clone();
        ensure!(
            record.platform.source != "local",
            "Local playback does not have a remote URL cache"
        );
        let current = self.active_media.clone();
        if automatic {
            ensure!(self.media_refresh_attempts < 2, "URL refresh limit reached");
            self.media_refresh_attempts += 1;
        }
        if let Some(media) = &current {
            self.repository.invalidate_resolved_media(
                &media.key,
                if automatic {
                    MediaInvalidationReason::HttpRejected
                } else {
                    MediaInvalidationReason::Manual
                },
            )?;
        }
        self.pending_promotion = None;
        self.invalidate_preload(Invalidation::Playback);
        self.cancel_lyrics(true);
        self.cancel_playback_resolution();
        self.generation = self.generation.wrapping_add(1);
        self.audio.stop();
        self.shared.lock().unwrap().approved_audio_identity = None;
        self.shared.lock().unwrap().resolved_track = None;
        self.shared.lock().unwrap().error = None;
        self.pending_foreground = Some(SourceResolveJob {
            generation: self.generation,
            record,
            strict_quality: current
                .as_ref()
                .map(|media| media.key.actual_quality.clone()),
            preload: None,
            force_refresh: true,
            resolved_track: current.and_then(|media| media.matched_track),
            skip_primary: false,
            excluded_platforms: Vec::new(),
        });
        self.busy = true;
        if let Err(error) = self.start_pending_foreground() {
            self.busy = false;
            return Err(error);
        }
        self.mark_source_async_started();
        Ok(())
    }

    pub(super) fn media_original_current(
        &self,
        original: &TrackRecord,
        preload: Option<&str>,
    ) -> bool {
        let current = if let Some(operation) = preload {
            let state = self.preload.snapshot();
            if state.operation_id.as_deref() != Some(operation) {
                return false;
            }
            state
                .candidate
                .as_ref()
                .and_then(|entry| self.queue_record(entry).ok())
        } else {
            self.queue
                .current
                .as_ref()
                .and_then(|entry| self.queue_record(entry).ok())
                .or_else(|| {
                    self.current_record
                        .as_ref()
                        .map(|record| record.as_ref().clone())
                })
        };
        current.is_some_and(|current| {
            match (
                track_record_fingerprint(&current),
                track_record_fingerprint(original),
            ) {
                (Ok(current), Ok(original)) => current == original,
                _ => false,
            }
        })
    }
}
