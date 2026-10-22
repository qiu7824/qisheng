//! Finite recovery preserves the approved URL and original selection authority.
use super::*;

pub(super) struct MediaRetry {
    due: Instant,
    generation: u64,
    audio_generation: u64,
    entry_id: Option<String>,
    original: TrackRecord,
    request: ApprovedAudioRequest,
}

impl Runtime {
    fn approved_request_current(
        &self,
        request: &ApprovedAudioRequest,
        original: &TrackRecord,
    ) -> bool {
        if !self.media_original_current(original, None) {
            return false;
        }
        let identity = &request.identity;
        if !self
            .shared
            .lock()
            .unwrap()
            .approved_audio_identity
            .as_ref()
            .is_some_and(|current| {
                current.url_sha256 == identity.url_sha256
                    && current.approval_id == identity.approval_id
                    && current.approval_epoch == identity.approval_epoch
            })
        {
            return false;
        }
        if identity.source_sha256.is_empty() {
            return self
                .catalog_asset_authority(original)
                .ok()
                .flatten()
                .is_some_and(|(_, url, current)| {
                    current.media_context_sha256 == identity.media_context_sha256
                        && format!("{:x}", Sha256::digest(url.as_bytes())) == identity.url_sha256
                });
        }
        if let Some(media) = &self.active_media {
            let resolved = media.matched_track.as_ref().unwrap_or(original);
            let expected = media::resolution_key(
                original,
                resolved,
                &identity.actual_quality,
                identity,
                &self.source_music_info(original),
            )
            .and_then(|key| key.fingerprint());
            if expected.ok().as_deref() != Some(identity.media_context_sha256.as_str()) {
                return false;
            }
        }
        identity.source_epoch == self.source_epoch
            && self.data.sources.iter().any(|source| {
                self.data.selected.as_deref() == Some(source.sha256.as_str())
                    && source.sha256 == identity.source_sha256
                    && u64::from(source.approval_policy_version) == identity.source_policy_version
                    && source.require_approval().is_ok()
            })
    }

    pub(super) fn reset_media_recovery(&mut self) {
        self.media_retry_pending = None;
        self.media_rate_retries = 0;
        self.media_attempted_sources.clear();
        self.approved_request = None;
    }

    pub(super) fn poll_media_retry(&mut self) {
        if self.timeout_stop.blocks_automatic_playback() {
            return;
        }
        if self
            .media_retry_pending
            .as_ref()
            .is_none_or(|retry| retry.due > Instant::now())
        {
            return;
        }
        let retry = self.media_retry_pending.take().unwrap();
        let audio = self.audio.snapshot();
        if !self.play_when_ready
            || self.busy
            || self.pending.is_some()
            || retry.generation != self.generation
            || retry.audio_generation != audio.generation
            || audio.status != PlaybackStatus::Error
            || retry.entry_id.as_deref()
                != self
                    .queue
                    .current
                    .as_ref()
                    .map(|entry| entry.entry_id.as_str())
            || !self.approved_request_current(&retry.request, &retry.original)
        {
            return;
        }
        self.cancel_lyrics(true);
        self.lyric_start_after_generation = audio.generation;
        self.shared.lock().unwrap().error = None;
        self.poll_timeout_stop();
        if self.timeout_stop.blocks_automatic_playback() || !self.play_when_ready {
            return;
        }
        self.audio.load_approved(retry.request, true);
    }

    pub(super) fn recover_media_failure(
        &mut self,
        audio: &crate::audio::PlaybackSnapshot,
    ) -> Result<bool> {
        let Some(failure) = &audio.failure else {
            return Ok(false);
        };
        let Some(request) = self.approved_request.clone() else {
            return Ok(false);
        };
        let Some(original) = self
            .current_record
            .as_ref()
            .map(|record| record.as_ref().clone())
        else {
            return Ok(false);
        };
        if failure.request_url_sha256.as_deref() != Some(request.identity.url_sha256.as_str())
            || !self.approved_request_current(&request, &original)
        {
            return Ok(false);
        }
        if failure.stage == crate::audio::PlaybackFailureStage::Transport
            && failure.media.as_ref().is_some_and(|media| {
                media.kind == crate::audio::transport::MediaIoErrorKind::Network
                    && media.http_status == Some(429)
            })
        {
            if self.media_rate_retries >= 2 {
                return Ok(false);
            }
            self.media_rate_retries += 1;
            self.media_retry_pending = Some(MediaRetry {
                due: Instant::now() + Duration::from_secs(u64::from(self.media_rate_retries) * 2),
                generation: self.generation,
                audio_generation: audio.generation,
                entry_id: self
                    .queue
                    .current
                    .as_ref()
                    .map(|entry| entry.entry_id.clone()),
                original,
                request,
            });
            return Ok(true);
        }
        if !failure.requires_url_refresh() {
            return Ok(false);
        }
        self.media_retry_pending = None;
        self.media_attempted_sources
            .insert(request.identity.platform.clone());
        if let Some(media) = &self.active_media {
            self.repository.invalidate_resolved_media(
                &media.key,
                crate::domain::media::MediaInvalidationReason::HttpRejected,
            )?;
        }
        if self.media_refresh_attempts < 2 {
            self.refresh_media_url(true)?;
            return Ok(true);
        }
        self.begin_media_fallback()
    }

    pub(super) fn recover_resolution_failure(&mut self, error: &anyhow::Error) -> Result<bool> {
        if error.is::<crate::source::SourceRequestCancelled>()
            || error
                .downcast_ref::<crate::source::SourceRequestFailure>()
                .is_some_and(|failure| {
                    failure.kind == crate::source::SourceFailureKind::RateLimited
                })
            || self.media_refresh_attempts == 0
        {
            return Ok(false);
        }
        if let Some(media) = &self.active_media {
            self.media_attempted_sources.insert(
                media
                    .matched_track
                    .as_ref()
                    .map_or(media.key.original_platform.as_str(), |track| {
                        track.platform.source.as_str()
                    })
                    .to_owned(),
            );
        }
        self.begin_media_fallback()
    }

    fn begin_media_fallback(&mut self) -> Result<bool> {
        if !self.auto_switch_source
            || !self.play_when_ready
            || self.busy
            || self.media_attempted_sources.len() >= 5
        {
            return Ok(false);
        }
        let original = self
            .current_record
            .as_ref()
            .context("No original track for source recovery")?
            .as_ref()
            .clone();
        if original.platform.source == "local" {
            return Ok(false);
        }
        self.invalidate_preload(Invalidation::Playback);
        self.cancel_lyrics(true);
        self.cancel_playback_resolution();
        self.generation = self.generation.wrapping_add(1);
        self.audio.stop();
        self.media_refresh_attempts = 0;
        self.media_rate_retries = 0;
        self.media_retry_pending = None;
        self.approved_request = None;
        self.pending = None;
        {
            let mut snapshot = self.shared.lock().unwrap();
            snapshot.approved_audio_identity = None;
            snapshot.pending_playback_url = None;
            snapshot.resolved_track = None;
            snapshot.error = None;
        }
        let skip_primary = self
            .media_attempted_sources
            .contains(&original.platform.source);
        self.pending_foreground = Some(SourceResolveJob {
            generation: self.generation,
            record: original,
            strict_quality: self.playback_requested_quality.clone(),
            preload: None,
            force_refresh: false,
            resolved_track: None,
            skip_primary,
            excluded_platforms: self.media_attempted_sources.iter().cloned().collect(),
        });
        self.busy = true;
        if let Err(error) = self.start_pending_foreground() {
            self.busy = false;
            return Err(error);
        }
        Ok(true)
    }
}
