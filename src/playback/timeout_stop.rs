//! Monotonic, process-local sleep timer and automatic-playback latch.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::time::{Duration, Instant};

/// Native admission limit, expressed within a signed 32-bit millisecond delay.
pub const MAX_TIMEOUT_STOP_SECONDS: f64 = 2_147_483.647;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TimeoutStopPreferences {
    pub wait_for_track_end: bool,
    pub last_seconds: Option<f64>,
}
impl Default for TimeoutStopPreferences {
    fn default() -> Self {
        Self {
            wait_for_track_end: true,
            last_seconds: None,
        }
    }
}
impl TimeoutStopPreferences {
    pub fn validate(&self) -> Result<()> {
        if let Some(seconds) = self.last_seconds {
            validated_duration(seconds)?;
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TimeoutStopPreferencesWire {
    wait_for_track_end: bool,
    last_seconds: Option<f64>,
}
impl Default for TimeoutStopPreferencesWire {
    fn default() -> Self {
        Self {
            wait_for_track_end: true,
            last_seconds: None,
        }
    }
}
impl<'de> Deserialize<'de> for TimeoutStopPreferences {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let wire = TimeoutStopPreferencesWire::deserialize(deserializer)?;
        let value = Self {
            wait_for_track_end: wire.wait_for_track_end,
            last_seconds: wire.last_seconds,
        };
        value.validate().map_err(D::Error::custom)?;
        Ok(value)
    }
}
fn validated_duration(seconds: f64) -> Result<Duration> {
    ensure!(
        seconds.is_finite() && seconds > 0.0 && seconds <= MAX_TIMEOUT_STOP_SECONDS,
        "Sleep timer seconds must be positive and within the supported delay"
    );
    let duration = Duration::try_from_secs_f64(seconds)
        .context("Sleep timer duration is not representable")?;
    ensure!(
        !duration.is_zero(),
        "Sleep timer duration is below clock resolution"
    );
    Ok(duration)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutStopStatus {
    #[default]
    Idle,
    Armed,
    WaitingForTrackEnd,
    Elapsed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimeoutStopState {
    pub sequence: u64,
    pub status: TimeoutStopStatus,
    pub seconds: Option<f64>,
    pub remaining_millis: Option<u64>,
    pub wait_for_track_end: bool,
    pub blocks_automatic_playback: bool,
}

#[derive(Clone)]
pub(crate) struct TimeoutStop {
    sequence: u64,
    status: TimeoutStopStatus,
    seconds: Option<f64>,
    deadline: Option<Instant>,
    wait_for_track_end: bool,
}
impl Default for TimeoutStop {
    fn default() -> Self {
        Self {
            sequence: 0,
            status: TimeoutStopStatus::Idle,
            seconds: None,
            deadline: None,
            wait_for_track_end: true,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeoutAction {
    None,
    Pause,
    WaitForTrackEnd,
    TrackEnded,
}
impl TimeoutStop {
    pub(crate) fn arm(&mut self, seconds: f64, wait: bool, now: Instant) -> Result<()> {
        let duration = validated_duration(seconds)?;
        let deadline = now
            .checked_add(duration)
            .context("Sleep timer deadline overflow")?;
        let sequence = self
            .sequence
            .checked_add(1)
            .context("Sleep timer sequence exhausted")?;
        self.sequence = sequence;
        self.status = TimeoutStopStatus::Armed;
        self.seconds = Some(seconds);
        self.deadline = Some(deadline);
        self.wait_for_track_end = wait;
        Ok(())
    }
    pub(crate) fn configure_wait(&mut self, wait: bool) {
        if matches!(
            self.status,
            TimeoutStopStatus::Idle | TimeoutStopStatus::Armed
        ) {
            self.wait_for_track_end = wait;
        }
    }
    pub(crate) fn cancel(&mut self) -> Result<()> {
        let sequence = self
            .sequence
            .checked_add(1)
            .context("Sleep timer sequence exhausted")?;
        self.sequence = sequence;
        self.clear();
        Ok(())
    }
    fn clear(&mut self) {
        self.status = TimeoutStopStatus::Idle;
        self.seconds = None;
        self.deadline = None;
    }
    /// An explicit playback intent releases the elapsed latch while an armed
    /// timer continues counting across pause, seek and manual track selection.
    pub(crate) fn manual_playback(&mut self) -> bool {
        if self.blocks_automatic_playback() {
            self.clear();
            true
        } else {
            false
        }
    }
    pub(crate) fn blocks_automatic_playback(&self) -> bool {
        matches!(
            self.status,
            TimeoutStopStatus::WaitingForTrackEnd | TimeoutStopStatus::Elapsed
        )
    }
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        track_can_finish: bool,
        ended: bool,
    ) -> TimeoutAction {
        if self.status == TimeoutStopStatus::Armed
            && self.deadline.is_some_and(|deadline| now >= deadline)
        {
            self.deadline = None;
            if self.wait_for_track_end && track_can_finish && !ended {
                self.status = TimeoutStopStatus::WaitingForTrackEnd;
                return TimeoutAction::WaitForTrackEnd;
            }
            self.status = TimeoutStopStatus::Elapsed;
            return if ended {
                TimeoutAction::TrackEnded
            } else {
                TimeoutAction::Pause
            };
        }
        if self.status == TimeoutStopStatus::WaitingForTrackEnd && (!track_can_finish || ended) {
            self.status = TimeoutStopStatus::Elapsed;
            return TimeoutAction::TrackEnded;
        }
        TimeoutAction::None
    }
    pub(crate) fn snapshot(&self, now: Instant) -> TimeoutStopState {
        let remaining_millis = self.deadline.map(|deadline| {
            let duration = deadline.saturating_duration_since(now);
            let millis =
                duration.as_millis() + u128::from(duration.subsec_nanos() % 1_000_000 != 0);
            millis.min(u64::MAX as u128) as u64
        });
        TimeoutStopState {
            sequence: self.sequence,
            status: self.status,
            seconds: self.seconds,
            remaining_millis,
            wait_for_track_end: self.wait_for_track_end,
            blocks_automatic_playback: self.blocks_automatic_playback(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_wait_option_changes_an_armed_timer_without_restarting_its_clock() {
        let now = Instant::now();
        let mut timer = TimeoutStop::default();
        assert!(timer.snapshot(now).wait_for_track_end);
        timer.arm(10.0, true, now).unwrap();
        timer.configure_wait(false);
        assert_eq!(
            timer.poll(now + Duration::from_secs(10), true, false),
            TimeoutAction::Pause
        );
        let before = timer.snapshot(now);
        timer.configure_wait(true);
        assert_eq!(
            timer.snapshot(now),
            before,
            "Elapsed action retains its original mode"
        );
        let mut prefs = TimeoutStopPreferences::default();
        for seconds in [
            f64::NAN,
            f64::INFINITY,
            0.0,
            -1.0,
            MAX_TIMEOUT_STOP_SECONDS + 1.0,
        ] {
            prefs.last_seconds = Some(seconds);
            assert!(prefs.validate().is_err());
        }
        prefs.last_seconds = Some(1800.0);
        assert!(prefs.validate().is_ok());
        let mut unstarted = TimeoutStop::default();
        unstarted.arm(1.0, true, now).unwrap();
        assert_eq!(
            unstarted.poll(now + Duration::from_secs(1), false, false),
            TimeoutAction::Pause
        );
        assert!(unstarted.blocks_automatic_playback());
    }
    #[test]
    fn replacement_and_invalid_request_keep_the_actual_monotonic_deadline() {
        let now = Instant::now();
        let mut timer = TimeoutStop::default();
        timer.arm(10.0, false, now).unwrap();
        assert_eq!(
            timer.poll(now + Duration::from_secs(5), true, false),
            TimeoutAction::None
        );
        timer.arm(20.0, true, now + Duration::from_secs(5)).unwrap();
        let before = timer.snapshot(now);
        for invalid in [
            f64::NAN,
            f64::INFINITY,
            0.0,
            -1.0,
            MAX_TIMEOUT_STOP_SECONDS + 1.0,
        ] {
            assert!(timer.arm(invalid, false, now).is_err());
            assert_eq!(timer.snapshot(now), before);
        }
        assert_eq!(
            timer.poll(now + Duration::from_secs(24), true, false),
            TimeoutAction::None
        );
        assert_eq!(
            timer.poll(now + Duration::from_secs(25), true, false),
            TimeoutAction::WaitForTrackEnd
        );
        assert!(timer.blocks_automatic_playback());
        assert_eq!(
            timer.poll(now + Duration::from_secs(40), true, false),
            TimeoutAction::None
        );
        assert_eq!(
            timer.poll(now + Duration::from_secs(41), true, true),
            TimeoutAction::TrackEnded
        );
        assert_eq!(
            timer.poll(now + Duration::from_secs(42), true, true),
            TimeoutAction::None
        );
        assert!(timer.manual_playback());
        assert!(!timer.blocks_automatic_playback());
    }
    #[test]
    fn immediate_expiry_is_one_shot_and_cancel_clears_wait_latch() {
        let now = Instant::now();
        let mut timer = TimeoutStop::default();
        timer.arm(0.25, false, now).unwrap();
        assert!(!timer.manual_playback());
        assert_eq!(
            timer
                .snapshot(now + Duration::from_millis(1))
                .remaining_millis,
            Some(249)
        );
        assert_eq!(
            timer.poll(now + Duration::from_secs(1), true, false),
            TimeoutAction::Pause
        );
        assert_eq!(
            timer.poll(now + Duration::from_secs(2), true, false),
            TimeoutAction::None
        );
        timer.arm(0.1, true, now).unwrap();
        assert_eq!(
            timer.poll(now + Duration::from_secs(1), true, false),
            TimeoutAction::WaitForTrackEnd
        );
        timer.cancel().unwrap();
        assert_eq!(timer.snapshot(now).status, TimeoutStopStatus::Idle);
        assert_eq!(
            timer.poll(now + Duration::from_secs(10), true, true),
            TimeoutAction::None
        );
    }
}
