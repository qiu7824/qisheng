use super::*;
use crate::playback::timeout_stop::TimeoutAction;

impl Runtime {
    pub(super) fn publish_timeout_stop(&self) {
        self.events.lock().unwrap().publish(
            self.revision,
            "timeout_stop",
            None,
            BackendEvent::TimeoutStopChanged {
                state: self.timeout_stop.snapshot(Instant::now()),
            },
        );
    }
    pub(super) fn release_timeout_stop_latch(&mut self) {
        if self.timeout_stop.manual_playback() {
            self.publish_timeout_stop();
        }
    }
    pub(super) fn poll_timeout_stop(&mut self) {
        self.timeout_stop
            .configure_wait(self.data.timeout_stop.wait_for_track_end);
        let audio = self.audio.snapshot();
        let can_finish = audio.track_id.is_some()
            && audio.has_presented_media
            && matches!(
                audio.status,
                PlaybackStatus::Buffering | PlaybackStatus::Playing | PlaybackStatus::Paused
            );
        let action = self.timeout_stop.poll(
            Instant::now(),
            can_finish,
            audio.status == PlaybackStatus::Ended,
        );
        if action == TimeoutAction::None {
            return;
        }
        self.media_retry_pending = None;
        self.skip_pending = None;
        if let Some(promotion) = self.pending_promotion.take() {
            let actual = self.audio.snapshot();
            let has_started = actual.generation > promotion.expected_generation
                && actual.track_id.as_deref() == Some(promotion.record.id.as_str())
                && actual.has_presented_media;
            if !has_started {
                promotion.cancel.cancel();
            }
        }
        self.invalidate_preload(Invalidation::Stop);
        self.play_when_ready = false;
        if action != TimeoutAction::WaitForTrackEnd {
            if action == TimeoutAction::Pause {
                self.audio.pause();
            }
            if let Err(error) = self.save_checkpoint() {
                self.error(format!("Sleep timer checkpoint failed: {error:#}"));
            }
        }
        self.publish_timeout_stop();
    }
}
