use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use crate::domain::playback_processing::{MAX_PLAYBACK_RATE, MIN_PLAYBACK_RATE};

pub const MAX_SYSTEM_INPUTS: usize = 32;
pub const MAX_HOTKEYS: usize = 32;
const MAX_TEXT_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum SystemAction {
    Play,
    Pause,
    Toggle,
    Stop,
    Previous,
    Next,
    SeekAbsolute { position_ms: u64 },
    SeekRelative { offset_ms: i64 },
    PlaybackRate { rate: f64 },
    VolumeStep { steps: i8 },
    ToggleMute,
    Love,
    Unlove,
    Dislike,
    PowerEvent { event: SystemPowerEvent },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPowerEvent {
    Suspend,
    ResumeAutomatic,
    ResumeUser,
}
impl SystemAction {
    pub fn validate(self) -> Result<()> {
        match self {
            Self::SeekAbsolute { position_ms } => ensure!(
                position_ms <= i64::MAX as u64 / 10_000,
                "System seek exceeds the native timeline limit"
            ),
            Self::SeekRelative { offset_ms } => ensure!(
                offset_ms != 0 && offset_ms.unsigned_abs() <= 86_400_000,
                "System seek offset exceeds one day"
            ),
            Self::PlaybackRate { rate } => ensure!(
                rate.is_finite() && (MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&rate),
                "System playback rate is invalid"
            ),
            Self::VolumeStep { steps } => ensure!(
                steps != 0 && steps.unsigned_abs() <= 20,
                "System volume step is invalid"
            ),
            _ => (),
        }
        Ok(())
    }
    pub fn requires_generation(self) -> bool {
        matches!(
            self,
            Self::SeekAbsolute { .. }
                | Self::SeekRelative { .. }
                | Self::PlaybackRate { .. }
                | Self::Love
                | Self::Unlove
                | Self::Dislike
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPlaybackState {
    None,
    Stopped,
    Paused,
    Buffering,
    Playing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceArtworkBinding {
    pub generation: Option<u64>,
    pub source_epoch: Option<u64>,
    pub key_sha256: String,
    pub expires_at_ms: u64,
}
struct ArtworkPinState {
    retired: bool,
    active: usize,
}
struct ArtworkLeaseCore {
    bytes: Arc<[u8]>,
    sha256: String,
    binding: SourceArtworkBinding,
    pins: Mutex<ArtworkPinState>,
    physical_owner: Option<Arc<dyn SourceArtworkOwner>>,
}
/// Implementations expose only a revocable RAM gate. Real file handles remain
/// with their physical worker until every lease and native pin is released.
pub(crate) trait SourceArtworkOwner: Send + Sync {
    fn is_current(&self) -> bool;
    fn begin_retirement(&self);
}
impl std::fmt::Debug for ArtworkLeaseCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtworkLease")
            .field("sha256", &self.sha256)
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}
/// Retirement blocks new pins. Existing pins remain counted until the actual
/// native thumbnail has been cleared or replaced and its owner releases them.
#[derive(Clone, Debug)]
pub struct SourceArtworkLease(Arc<ArtworkLeaseCore>);
impl SourceArtworkLease {
    /// The caller must obtain bytes from PictureService::verified_arc after its
    /// current source policy, artwork key and local-grant checks have succeeded.
    pub(crate) fn from_verified_ram(
        bytes: Arc<[u8]>,
        sha256: String,
        binding: SourceArtworkBinding,
    ) -> Result<Self> {
        use sha2::{Digest, Sha256};
        crate::domain::artwork::digest(&sha256)?;
        crate::domain::artwork::digest(&binding.key_sha256)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= crate::domain::artwork::MAX_ARTWORK_BYTES,
            "System artwork exceeds its verified byte limit"
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == sha256,
            "Verified system artwork bytes changed"
        );
        ensure!(
            binding.generation.is_none_or(|g| g > 0) && binding.source_epoch.is_none_or(|e| e > 0),
            "System artwork authority epoch is invalid"
        );
        ensure!(
            binding.expires_at_ms > artwork_now_ms(),
            "System artwork authority expired"
        );
        Ok(Self(Arc::new(ArtworkLeaseCore {
            bytes,
            sha256,
            binding,
            pins: Mutex::new(ArtworkPinState {
                retired: false,
                active: 0,
            }),
            physical_owner: None,
        })))
    }
    pub(crate) fn retain_physical_owner(
        mut self,
        owner: Arc<dyn SourceArtworkOwner>,
    ) -> Result<Self> {
        ensure!(owner.is_current(), "Native artwork physical proof retired");
        let core = Arc::get_mut(&mut self.0).ok_or_else(|| {
            anyhow::anyhow!("Native artwork lease was shared before physical proof attachment")
        })?;
        core.physical_owner = Some(owner);
        Ok(self)
    }
    pub fn binding(&self) -> &SourceArtworkBinding {
        &self.0.binding
    }
    pub fn retire(&self) {
        self.0
            .pins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retired = true;
        if let Some(owner) = &self.0.physical_owner {
            owner.begin_retirement();
        }
    }
    pub fn is_current(&self) -> bool {
        let state = self.0.pins.lock().unwrap_or_else(|e| e.into_inner());
        !state.retired
            && self.0.binding.expires_at_ms > artwork_now_ms()
            && self
                .0
                .physical_owner
                .as_ref()
                .is_none_or(|owner| owner.is_current())
    }
    pub fn active_pins(&self) -> usize {
        self.0.pins.lock().unwrap_or_else(|e| e.into_inner()).active
    }
    pub fn is_drained(&self) -> bool {
        let state = self.0.pins.lock().unwrap_or_else(|e| e.into_inner());
        (state.retired || self.0.binding.expires_at_ms <= artwork_now_ms()) && state.active == 0
    }
    pub(crate) fn pin(&self) -> Result<SourceArtworkPin> {
        let mut state = self.0.pins.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !state.retired && self.0.binding.expires_at_ms > artwork_now_ms(),
            "System artwork authority retired"
        );
        ensure!(
            self.0
                .physical_owner
                .as_ref()
                .is_none_or(|owner| owner.is_current()),
            "Native artwork physical authority retired"
        );
        ensure!(state.active < 4, "System artwork native pin limit exceeded");
        state.active += 1;
        Ok(SourceArtworkPin(self.0.clone()))
    }
}
pub(crate) struct SourceArtworkPin(Arc<ArtworkLeaseCore>);
impl SourceArtworkPin {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0.bytes
    }
    pub(crate) fn matches(&self, artwork: &SystemArtwork) -> bool {
        Arc::ptr_eq(&self.0, &artwork.lease.0)
    }
}
impl Drop for SourceArtworkPin {
    fn drop(&mut self) {
        let mut state = self.0.pins.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
    }
}
fn artwork_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
#[derive(Clone, Debug)]
pub struct SystemArtwork {
    lease: SourceArtworkLease,
}
impl SystemArtwork {
    pub fn from_verified(
        value: &crate::library::artwork_validator::VerifiedArtwork,
    ) -> Result<Self> {
        let sha256 = value.metadata().sha256.clone();
        Self::from_source_lease(SourceArtworkLease::from_verified_ram(
            value.encoded_arc(),
            sha256.clone(),
            SourceArtworkBinding {
                generation: None,
                source_epoch: None,
                key_sha256: sha256,
                expires_at_ms: u64::MAX,
            },
        )?)
    }
    pub fn from_source_lease(lease: SourceArtworkLease) -> Result<Self> {
        ensure!(lease.is_current(), "System artwork authority retired");
        Ok(Self { lease })
    }
    pub fn sha256(&self) -> &str {
        &self.lease.0.sha256
    }
    pub fn binding(&self) -> &SourceArtworkBinding {
        self.lease.binding()
    }
    pub fn is_current(&self) -> bool {
        self.lease.is_current()
    }
    pub(crate) fn pin(&self) -> Result<SourceArtworkPin> {
        self.lease.pin()
    }
}

#[derive(Clone, Debug)]
pub struct SystemProjection {
    pub generation: u64,
    pub media_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub state: SystemPlaybackState,
    pub duration_ms: u64,
    pub position_ms: u64,
    pub playback_rate: f64,
    pub can_previous: bool,
    pub can_next: bool,
    pub can_seek: bool,
    pub artwork: Option<SystemArtwork>,
}
impl Default for SystemProjection {
    fn default() -> Self {
        Self {
            generation: 0,
            media_id: None,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            state: SystemPlaybackState::None,
            duration_ms: 0,
            position_ms: 0,
            playback_rate: 1.0,
            can_previous: false,
            can_next: false,
            can_seek: false,
            artwork: None,
        }
    }
}
impl SystemProjection {
    pub fn validate(&self) -> Result<()> {
        for text in [&self.title, &self.artist, &self.album] {
            ensure!(
                text.len() <= MAX_TEXT_BYTES && !text.contains('\0'),
                "System metadata text exceeds its bound"
            );
        }
        if let Some(id) = &self.media_id {
            ensure!(
                !id.is_empty() && id.len() <= 1024 && !id.contains('\0'),
                "System media identity is invalid"
            );
        }
        ensure!(
            self.duration_ms <= i64::MAX as u64 / 10_000 && self.position_ms <= self.duration_ms,
            "System timeline is invalid"
        );
        ensure!(
            self.playback_rate.is_finite()
                && (MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&self.playback_rate),
            "System projection rate is invalid"
        );
        ensure!(
            self.media_id.is_some()
                || matches!(
                    self.state,
                    SystemPlaybackState::None | SystemPlaybackState::Stopped
                ),
            "Active system playback has no media identity"
        );
        ensure!(
            self.media_id.is_none() || self.generation > 0,
            "System media has no playback generation"
        );
        if let Some(artwork) = &self.artwork {
            ensure!(
                artwork
                    .binding()
                    .generation
                    .is_none_or(|g| g == self.generation),
                "System artwork belongs to another playback generation"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeyChord {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub windows: bool,
    pub virtual_key: u16,
}
impl HotkeyChord {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=254).contains(&self.virtual_key),
            "Hotkey virtual key is invalid"
        );
        ensure!(
            self.control
                || self.alt
                || self.shift
                || self.windows
                || (0xb0..=0xb3).contains(&self.virtual_key),
            "Global hotkey needs a modifier or an explicit media key"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeyBinding {
    pub chord: HotkeyChord,
    pub action: SystemAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemSettings {
    pub enabled: bool,
    pub media_session_enabled: bool,
    pub global_hotkeys_enabled: bool,
    pub power_blocker_enabled: bool,
    pub hotkeys: Vec<HotkeyBinding>,
}
impl Default for SystemSettings {
    fn default() -> Self {
        let chord = |virtual_key| HotkeyChord {
            control: true,
            alt: true,
            shift: false,
            windows: false,
            virtual_key,
        };
        Self {
            enabled: false,
            media_session_enabled: true,
            global_hotkeys_enabled: false,
            power_blocker_enabled: true,
            hotkeys: vec![
                HotkeyBinding {
                    chord: chord(0x74),
                    action: SystemAction::Toggle,
                },
                HotkeyBinding {
                    chord: chord(0x25),
                    action: SystemAction::Previous,
                },
                HotkeyBinding {
                    chord: chord(0x27),
                    action: SystemAction::Next,
                },
                HotkeyBinding {
                    chord: chord(0x26),
                    action: SystemAction::VolumeStep { steps: 4 },
                },
                HotkeyBinding {
                    chord: chord(0x28),
                    action: SystemAction::VolumeStep { steps: -4 },
                },
            ],
        }
    }
}
impl SystemSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.hotkeys.len() <= MAX_HOTKEYS,
            "Global hotkey count exceeds its limit"
        );
        for (index, binding) in self.hotkeys.iter().enumerate() {
            binding.chord.validate()?;
            binding.action.validate()?;
            ensure!(
                !matches!(binding.action, SystemAction::PowerEvent { .. }),
                "Native power notifications cannot be bound to a hotkey"
            );
            ensure!(
                !self.hotkeys[..index]
                    .iter()
                    .any(|old| old.chord == binding.chord),
                "Duplicate global hotkey chord"
            );
            ensure!(
                !(self.media_session_enabled && (0xb0..=0xb3).contains(&binding.chord.virtual_key)),
                "Media keys are owned by the active media session"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemConfigurationPhase {
    #[default]
    Idle,
    Applying,
    RollingBack,
    Retiring,
    AwaitingCommit,
    Closed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemConfigurationOutcome {
    Applied,
    Rejected,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemConfigurationAck {
    pub revision: u64,
    pub requested: SystemSettings,
    pub previous: Option<SystemSettings>,
    pub applied: Option<SystemSettings>,
    pub outcome: SystemConfigurationOutcome,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemConfigurationState {
    pub applied_revision: u64,
    pub applied: Option<SystemSettings>,
    pub pending_revision: Option<u64>,
    pub phase: SystemConfigurationPhase,
    pub terminal: Option<SystemConfigurationAck>,
    pub last_error: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SystemControlView {
    pub platform_supported: bool,
    pub persisted_settings: SystemSettings,
    pub owner_epoch: Option<u64>,
    pub configuration: SystemConfigurationState,
    pub native_ready: bool,
    pub owner_thread_exited: bool,
    pub registered_hotkeys: usize,
    pub media_session_active: bool,
    pub power_request_active: bool,
    pub last_power_event: Option<SystemPowerEvent>,
    pub retiring_owners: usize,
    pub native_error: Option<String>,
    pub dispatcher_error: Option<String>,
    pub persistence_error: Option<String>,
    pub store_recovery_pending: bool,
    pub store_recovery_operation: Option<String>,
}
#[derive(Clone)]
pub(super) struct ConfigurationRequest {
    pub revision: u64,
    pub settings: SystemSettings,
    pub previous: Option<SystemSettings>,
    pub rollback: bool,
}
/// One admitted request and one terminal acknowledgement are retained. A caller
/// must finish its durable settings commit before reopening input admission.
pub(super) struct ConfigurationMailbox {
    next: u64,
    request: Option<ConfigurationRequest>,
    state: SystemConfigurationState,
}
impl ConfigurationMailbox {
    pub(super) fn new(settings: SystemSettings) -> Result<Self> {
        let mut result = Self {
            next: 0,
            request: None,
            state: Default::default(),
        };
        result.admit(settings)?;
        Ok(result)
    }
    pub(super) fn state(&self) -> SystemConfigurationState {
        self.state.clone()
    }
    pub(super) fn admit(&mut self, settings: SystemSettings) -> Result<u64> {
        settings.validate()?;
        ensure!(
            self.state.pending_revision.is_none(),
            "A native configuration acknowledgement is still pending"
        );
        ensure!(
            self.state.phase != SystemConfigurationPhase::Closed,
            "Native system owner has closed"
        );
        let revision = self
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Native configuration revision exhausted"))?;
        self.next = revision;
        self.request = Some(ConfigurationRequest {
            revision,
            settings: settings.clone(),
            previous: self.state.applied.clone(),
            rollback: false,
        });
        self.state.pending_revision = Some(revision);
        self.state.phase = if settings.enabled {
            SystemConfigurationPhase::Applying
        } else {
            SystemConfigurationPhase::Retiring
        };
        self.state.last_error = None;
        Ok(revision)
    }
    pub(super) fn take_request(&mut self) -> Option<ConfigurationRequest> {
        self.request.take()
    }
    pub(super) fn failure(&mut self, revision: u64, error: &str) -> Result<()> {
        self.check(revision)?;
        self.state.phase = SystemConfigurationPhase::RollingBack;
        self.state.last_error = Some(error.chars().take(1024).collect());
        Ok(())
    }
    pub(super) fn finish(
        &mut self,
        request: ConfigurationRequest,
        outcome: SystemConfigurationOutcome,
        error: Option<String>,
    ) -> Result<()> {
        self.check(request.revision)?;
        let applied = match outcome {
            SystemConfigurationOutcome::Applied => {
                self.state.applied_revision = request.revision;
                Some(request.settings.clone())
            }
            SystemConfigurationOutcome::Rejected => request.previous.clone(),
            SystemConfigurationOutcome::Cancelled => None,
        };
        self.state.applied = applied.clone();
        self.state.last_error = error.as_ref().map(|e| e.chars().take(1024).collect());
        self.state.phase = SystemConfigurationPhase::AwaitingCommit;
        self.state.terminal = Some(SystemConfigurationAck {
            revision: request.revision,
            requested: request.settings,
            previous: request.previous,
            applied,
            outcome,
            error: self.state.last_error.clone(),
        });
        Ok(())
    }
    pub(super) fn acknowledge(&mut self, revision: u64) -> Result<SystemConfigurationAck> {
        self.check(revision)?;
        let ack = self
            .state
            .terminal
            .take()
            .ok_or_else(|| anyhow::anyhow!("Native configuration is not complete"))?;
        self.state.pending_revision = None;
        self.state.phase = if ack.applied.as_ref().is_some_and(|s| s.enabled) {
            SystemConfigurationPhase::Idle
        } else {
            SystemConfigurationPhase::Closed
        };
        Ok(ack)
    }
    pub(super) fn rollback_uncommitted(&mut self, revision: u64) -> Result<u64> {
        self.check(revision)?;
        let ack = self
            .state
            .terminal
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Native configuration is not complete"))?;
        ensure!(
            ack.outcome == SystemConfigurationOutcome::Applied && ack.requested.enabled,
            "Native configuration cannot be rolled back on this owner"
        );
        let settings = ack.previous.clone().unwrap_or_default();
        self.state.terminal = None;
        self.state.pending_revision = None;
        let next = self.admit(settings)?;
        self.request
            .as_mut()
            .expect("admitted durable rollback")
            .rollback = true;
        self.state.phase = SystemConfigurationPhase::RollingBack;
        Ok(next)
    }
    pub(super) fn closed(&mut self) {
        self.state.applied = None;
        if let Some(ack) = &mut self.state.terminal
            && ack.applied.as_ref().is_some_and(|s| s.enabled)
        {
            ack.applied = None;
            ack.outcome = SystemConfigurationOutcome::Cancelled;
            ack.error = Some("Native owner closed before configuration commit".into());
        }
        if self.state.pending_revision.is_none() {
            self.state.phase = SystemConfigurationPhase::Closed;
        }
    }
    fn check(&self, revision: u64) -> Result<()> {
        ensure!(
            self.state.pending_revision == Some(revision)
                && self
                    .state
                    .terminal
                    .as_ref()
                    .is_none_or(|ack| ack.revision == revision),
            "Native configuration acknowledgement is stale"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SystemInput {
    pub owner_epoch: u64,
    pub sequence: u64,
    pub generation: Option<u64>,
    pub action: SystemAction,
}
impl SystemInput {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.owner_epoch > 0 && self.sequence > 0,
            "System input identity is invalid"
        );
        self.action.validate()?;
        ensure!(
            !self.action.requires_generation() || self.generation.is_some(),
            "System action requires a media generation fence"
        );
        Ok(())
    }
    pub fn request_id(self) -> String {
        format!("system:{}:{}", self.owner_epoch, self.sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn enabled() -> SystemSettings {
        SystemSettings {
            enabled: true,
            ..Default::default()
        }
    }
    fn running() -> ConfigurationMailbox {
        let mut mailbox = ConfigurationMailbox::new(enabled()).unwrap();
        let request = mailbox.take_request().unwrap();
        mailbox
            .finish(request, SystemConfigurationOutcome::Applied, None)
            .unwrap();
        mailbox.acknowledge(1).unwrap();
        mailbox
    }
    #[test]
    fn failed_configuration_retains_old_applied_settings_until_rollback_ack() {
        let mut mailbox = running();
        let old = mailbox.state().applied.unwrap();
        let candidate = SystemSettings {
            global_hotkeys_enabled: true,
            ..old.clone()
        };
        let revision = mailbox.admit(candidate).unwrap();
        let request = mailbox.take_request().unwrap();
        mailbox
            .failure(revision, "RegisterHotKey returned conflict")
            .unwrap();
        let pending = mailbox.state();
        assert_eq!(pending.phase, SystemConfigurationPhase::RollingBack);
        assert_eq!(pending.applied, Some(old.clone()));
        assert!(pending.terminal.is_none());
        assert!(mailbox.admit(old.clone()).is_err());
        mailbox
            .failure(revision, "Native rollback still owns staged handles")
            .unwrap();
        assert!(mailbox.state().terminal.is_none());
        mailbox
            .finish(
                request,
                SystemConfigurationOutcome::Rejected,
                Some("RegisterHotKey returned conflict".into()),
            )
            .unwrap();
        assert!(mailbox.acknowledge(revision + 1).is_err());
        let ack = mailbox.acknowledge(revision).unwrap();
        assert_eq!(ack.outcome, SystemConfigurationOutcome::Rejected);
        assert_eq!(ack.applied, Some(old));
        assert_eq!(mailbox.state().phase, SystemConfigurationPhase::Idle);
    }
    #[test]
    fn failed_durable_commit_requires_a_new_native_rollback_revision() {
        let mut mailbox = running();
        let old = mailbox.state().applied.unwrap();
        let candidate = SystemSettings {
            media_session_enabled: false,
            ..old.clone()
        };
        let revision = mailbox.admit(candidate.clone()).unwrap();
        let request = mailbox.take_request().unwrap();
        mailbox
            .finish(request, SystemConfigurationOutcome::Applied, None)
            .unwrap();
        assert_eq!(mailbox.state().applied, Some(candidate));
        assert!(mailbox.admit(old.clone()).is_err());
        let rollback = mailbox.rollback_uncommitted(revision).unwrap();
        assert!(rollback > revision);
        assert!(mailbox.state().terminal.is_none());
        assert!(mailbox.acknowledge(revision).is_err());
        let request = mailbox.take_request().unwrap();
        assert_eq!(request.settings, old.clone());
        assert!(request.rollback);
        mailbox
            .failure(
                rollback,
                "Original configuration restoration still owns native resources",
            )
            .unwrap();
        assert_eq!(mailbox.state().phase, SystemConfigurationPhase::RollingBack);
        assert!(mailbox.state().terminal.is_none());
        mailbox
            .finish(request, SystemConfigurationOutcome::Applied, None)
            .unwrap();
        assert_eq!(mailbox.acknowledge(rollback).unwrap().applied, Some(old));
    }
    #[test]
    fn disabled_request_remains_pending_until_native_retirement_and_terminal_ack() {
        let mut mailbox = running();
        let revision = mailbox.admit(SystemSettings::default()).unwrap();
        let request = mailbox.take_request().unwrap();
        assert_eq!(mailbox.state().phase, SystemConfigurationPhase::Retiring);
        assert!(mailbox.acknowledge(revision).is_err());
        assert!(mailbox.state().applied.unwrap().enabled);
        mailbox
            .finish(request, SystemConfigurationOutcome::Applied, None)
            .unwrap();
        mailbox.closed();
        let ack = mailbox.acknowledge(revision).unwrap();
        assert_eq!(ack.outcome, SystemConfigurationOutcome::Applied);
        assert!(!ack.applied.unwrap().enabled);
        assert_eq!(mailbox.state().phase, SystemConfigurationPhase::Closed);
        assert!(mailbox.admit(enabled()).is_err());
    }
    #[test]
    fn shutdown_before_durable_commit_cancels_the_stale_enable_ack() {
        let mut mailbox = ConfigurationMailbox::new(enabled()).unwrap();
        let request = mailbox.take_request().unwrap();
        mailbox
            .finish(request, SystemConfigurationOutcome::Applied, None)
            .unwrap();
        mailbox.closed();
        let ack = mailbox.acknowledge(1).unwrap();
        assert_eq!(ack.outcome, SystemConfigurationOutcome::Cancelled);
        assert!(ack.applied.is_none());
    }
    #[test]
    fn retired_artwork_blocks_new_writers_and_waits_for_actual_native_pin_drop() {
        let bytes: Arc<[u8]> = Arc::from(b"verified fixture bytes".as_slice());
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let lease = SourceArtworkLease::from_verified_ram(
            bytes,
            sha256.clone(),
            SourceArtworkBinding {
                generation: Some(2),
                source_epoch: Some(3),
                key_sha256: sha256,
                expires_at_ms: u64::MAX,
            },
        )
        .unwrap();
        let clone = lease.clone();
        let pin = lease.pin().unwrap();
        assert_eq!(pin.bytes(), b"verified fixture bytes");
        clone.retire();
        assert!(!lease.is_current());
        assert!(lease.pin().is_err());
        assert_eq!(lease.active_pins(), 1);
        assert!(!lease.is_drained());
        drop(pin);
        assert!(lease.is_drained());
    }
    #[test]
    fn artwork_hash_scope_and_actual_backend_rate_bounds_are_validated() {
        let binding = SourceArtworkBinding {
            generation: Some(1),
            source_epoch: None,
            key_sha256: "a".repeat(64),
            expires_at_ms: u64::MAX,
        };
        assert!(
            SourceArtworkLease::from_verified_ram(
                Arc::from(b"changed bytes".as_slice()),
                "b".repeat(64),
                binding
            )
            .is_err()
        );
        assert!(
            SystemAction::PlaybackRate { rate: 0.25 }
                .validate()
                .is_err()
        );
        assert!(SystemAction::PlaybackRate { rate: 4.0 }.validate().is_err());
        SystemAction::PlaybackRate {
            rate: MIN_PLAYBACK_RATE,
        }
        .validate()
        .unwrap();
        SystemAction::PlaybackRate {
            rate: MAX_PLAYBACK_RATE,
        }
        .validate()
        .unwrap();
    }
}
