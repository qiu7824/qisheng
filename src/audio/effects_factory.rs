//! Preparation ownership for at most two actual effect banks.
//!
//! A slot is reserved before coefficient/FFT/bank preparation. Cancellation
//! keeps the preparation owner and any completed Box charged until actual drop.
//! Immutable recipes retain validated IR PCM without granting file authority.
//! Callback exchange, registry pinning and device migration use these owners;
//! they do not release slots merely by moving a bank between containers.

use super::{
    convolution::{PreparedConvolution, ValidatedImpulse},
    effects::{
        EFFECTS_MAX_REPLAY_FRAMES, EFFECTS_MIN_REPLAY_FRAMES, EffectsAlignment, EffectsError,
        EffectsFrame, EffectsInput, EffectsPreparation, PreparedEffects,
    },
    effects_handoff::{
        HandoffBounds, HandoffError, HandoffFrame, HandoffReservation, RetainedHandoff,
    },
    pcm::{MAX_CHANNELS, PcmFormat},
    processing::DspReservation,
    spatial::{SpatialPlan, SpatialStep, SpatialTarget},
};
use crate::domain::audio_effects::{EffectsSettings, ImpulseResourceId};
use std::{
    fmt,
    mem::{align_of, size_of},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FactoryError {
    Busy,
    Closed,
    Unavailable,
    InvalidRecipe,
    InvalidRequest,
    Cancelled,
    Quota,
    Preparation,
}
impl fmt::Display for FactoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Effect preparation failed: {self:?}")
    }
}
impl std::error::Error for FactoryError {}

/// The producer binds a representation digest to actual validated decoded PCM.
/// Only the memory representation is retained; this type provides no file open.
pub struct OwnedImpulse {
    actual: Arc<ValidatedImpulse>,
    source_id: ImpulseResourceId,
    representation_sha256: String,
    metadata_bytes: usize,
    _reservation: DspReservation,
}
impl OwnedImpulse {
    pub fn from_validated(
        actual: ValidatedImpulse,
        source_id: ImpulseResourceId,
        representation_sha256: String,
    ) -> Result<Arc<Self>, FactoryError> {
        source_id
            .validate()
            .map_err(|_| FactoryError::InvalidRecipe)?;
        if representation_sha256.len() != 64
            || !representation_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(FactoryError::InvalidRecipe);
        }
        let bytes = arc_bytes::<Self>()
            .checked_add(arc_bytes::<ValidatedImpulse>())
            .and_then(|bytes| bytes.checked_add(id_capacity(&source_id)))
            .and_then(|bytes| bytes.checked_add(representation_sha256.capacity()))
            .ok_or(FactoryError::InvalidRecipe)?;
        let reservation = DspReservation::new(bytes).map_err(|_| FactoryError::Quota)?;
        let actual = Arc::new(actual);
        Ok(Arc::new(Self {
            actual,
            source_id,
            representation_sha256,
            metadata_bytes: bytes,
            _reservation: reservation,
        }))
    }
    pub fn source_id(&self) -> &ImpulseResourceId {
        &self.source_id
    }
    pub fn representation_sha256(&self) -> &str {
        &self.representation_sha256
    }
    pub fn validated(&self) -> &ValidatedImpulse {
        &self.actual
    }
    pub fn metadata_owned_bytes(&self) -> usize {
        self.metadata_bytes
    }
    pub fn actual_strong_owners(&self) -> usize {
        Arc::strong_count(&self.actual)
    }
}

pub struct EffectsRecipe {
    settings: EffectsSettings,
    sequence: u64,
    effect_channels: u16,
    normalize: bool,
    // The actual Arc is private. The backing owner keeps its Arc allocation
    // reservation alive, including when a recipe outlives its external source.
    actual_impulse: Option<Arc<ValidatedImpulse>>,
    impulse_owner: Option<Arc<OwnedImpulse>>,
    metadata_bytes: usize,
    _reservation: DspReservation,
}
impl EffectsRecipe {
    pub fn prepare(
        settings: EffectsSettings,
        sequence: u64,
        effect_channels: u16,
        normalize: bool,
        impulse_owner: Option<Arc<OwnedImpulse>>,
    ) -> Result<Arc<Self>, FactoryError> {
        settings
            .validate()
            .map_err(|_| FactoryError::InvalidRecipe)?;
        if sequence == 0 || !(2..=MAX_CHANNELS as u16).contains(&effect_channels) {
            return Err(FactoryError::InvalidRecipe);
        }
        match (&settings.convolution.impulse, &impulse_owner) {
            (None, None) => {}
            (Some(id), Some(owner)) if *id == owner.source_id => {}
            (Some(_), None) => return Err(FactoryError::Unavailable),
            _ => return Err(FactoryError::InvalidRecipe),
        }
        let bytes = arc_bytes::<Self>()
            .checked_add(settings.convolution.impulse.as_ref().map_or(0, id_capacity))
            .ok_or(FactoryError::InvalidRecipe)?;
        let reservation = DspReservation::new(bytes).map_err(|_| FactoryError::Quota)?;
        let actual_impulse = impulse_owner.as_ref().map(|owner| owner.actual.clone());
        Ok(Arc::new(Self {
            settings,
            sequence,
            effect_channels,
            normalize,
            actual_impulse,
            impulse_owner,
            metadata_bytes: bytes,
            _reservation: reservation,
        }))
    }
    pub fn settings(&self) -> &EffectsSettings {
        &self.settings
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn effect_channels(&self) -> u16 {
        self.effect_channels
    }
    pub fn normalize(&self) -> bool {
        self.normalize
    }
    pub fn validated_impulse(&self) -> Option<&ValidatedImpulse> {
        self.actual_impulse.as_deref()
    }
    pub fn impulse_source(&self) -> Option<&OwnedImpulse> {
        self.impulse_owner.as_deref()
    }
    pub fn metadata_owned_bytes(&self) -> usize {
        self.metadata_bytes
    }
}

struct PoolState {
    occupied: AtomicUsize,
    closed: AtomicBool,
    metadata_bytes: usize,
    _reservation: DspReservation,
}
#[derive(Clone)]
pub struct EffectsBankPool {
    state: Arc<PoolState>,
}
impl EffectsBankPool {
    pub fn new() -> Result<Self, FactoryError> {
        let bytes = arc_bytes::<PoolState>();
        let reservation = DspReservation::new(bytes).map_err(|_| FactoryError::Quota)?;
        Ok(Self {
            state: Arc::new(PoolState {
                occupied: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                metadata_bytes: bytes,
                _reservation: reservation,
            }),
        })
    }
    pub fn occupied_banks(&self) -> usize {
        self.state.occupied.load(Ordering::Acquire)
    }
    pub fn metadata_owned_bytes(&self) -> usize {
        self.state.metadata_bytes
    }
    pub fn begin_shutdown(&self) {
        self.state.closed.store(true, Ordering::Release);
    }
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }
    pub fn begin(
        &self,
        recipe: Arc<EffectsRecipe>,
        request: BankRequest,
    ) -> Result<BankPreparation, FactoryError> {
        if self.is_closed() {
            return Err(FactoryError::Closed);
        }
        request.validate(&recipe)?;
        let slot = self.reserve_slot()?;
        Ok(BankPreparation {
            candidate: None,
            record: None,
            recipe,
            request,
            slot,
        })
    }
    fn reserve_slot(&self) -> Result<BankSlot, FactoryError> {
        if self.is_closed() {
            return Err(FactoryError::Closed);
        }
        self.state
            .occupied
            .try_update(Ordering::AcqRel, Ordering::Acquire, |occupied| {
                (occupied < 2 && !self.state.closed.load(Ordering::Acquire)).then_some(occupied + 1)
            })
            .map_err(|_| {
                if self.is_closed() {
                    FactoryError::Closed
                } else {
                    FactoryError::Busy
                }
            })?;
        let slot = BankSlot {
            state: self.state.clone(),
        };
        if self.is_closed() {
            drop(slot);
            return Err(FactoryError::Closed);
        }
        Ok(slot)
    }
    /// Compatibility adoption never supplies a recipe or migration authority.
    pub(crate) fn adopt_legacy(
        &self,
        actual: Box<PreparedEffects>,
    ) -> Result<LegacyBank, Box<PreparedEffects>> {
        match self.reserve_slot() {
            Ok(slot) => Ok(LegacyBank {
                actual,
                _slot: slot,
            }),
            Err(_) => Err(actual),
        }
    }
}
/// Not cloneable and never separately returned by a bank/preparation owner.
pub struct BankSlot {
    state: Arc<PoolState>,
}
impl Drop for BankSlot {
    fn drop(&mut self) {
        self.state.occupied.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BankRequest {
    pub format: PcmFormat,
    pub device_channels: u16,
    pub token: u64,
    pub source_epoch: u64,
    pub replay_capacity: usize,
    pub initial_spatial: Option<SpatialTarget>,
}
impl BankRequest {
    fn validate(&self, recipe: &EffectsRecipe) -> Result<(), FactoryError> {
        if !(8000..=384000).contains(&self.format.sample_rate)
            || self.format.channels != recipe.effect_channels
            || !(1..=MAX_CHANNELS as u16).contains(&self.device_channels)
            || self.token == 0
            || self.source_epoch == 0
            || !(EFFECTS_MIN_REPLAY_FRAMES..=EFFECTS_MAX_REPLAY_FRAMES)
                .contains(&self.replay_capacity)
            || self.initial_spatial.is_some_and(|target| {
                target.sample_rate() != self.format.sample_rate || target.sequence() == 0
            })
        {
            return Err(FactoryError::InvalidRequest);
        }
        Ok(())
    }
}

pub struct PreparedBankRecord {
    owner_id: u64,
    preparation: EffectsPreparation,
    recipe: Arc<EffectsRecipe>,
    bounds: Arc<HandoffBounds>,
    pool: Arc<PoolState>,
    metadata_bytes: usize,
    _reservation: DspReservation,
}
impl PreparedBankRecord {
    pub fn preparation(&self) -> EffectsPreparation {
        self.preparation
    }
    pub fn owner_id(&self) -> u64 {
        self.owner_id
    }
    pub fn recipe(&self) -> &Arc<EffectsRecipe> {
        &self.recipe
    }
    pub fn bounds(&self) -> &Arc<HandoffBounds> {
        &self.bounds
    }
    pub fn metadata_owned_bytes(&self) -> usize {
        self.metadata_bytes
    }
}

pub struct BankPreparation {
    // Fields drop in this order: every actual Box and record precedes the slot.
    candidate: Option<Box<PreparedEffects>>,
    record: Option<Arc<PreparedBankRecord>>,
    recipe: Arc<EffectsRecipe>,
    request: BankRequest,
    slot: BankSlot,
}
pub struct PreparationFailure {
    pub error: FactoryError,
    pub owner: BankPreparation,
}
impl fmt::Debug for PreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparationFailure")
            .field("error", &self.error)
            .field("has_actual_bank", &self.owner.candidate.is_some())
            .finish()
    }
}
impl BankPreparation {
    pub fn has_actual_bank(&self) -> bool {
        self.candidate.is_some()
    }
    pub fn recipe(&self) -> &Arc<EffectsRecipe> {
        &self.recipe
    }
    pub fn request(&self) -> BankRequest {
        self.request
    }
    #[expect(
        clippy::result_large_err,
        reason = "Failure returns the exact charged preparation owner without a second fallible allocation."
    )]
    pub fn prepare(mut self, cancel: &AtomicBool) -> Result<CountedBank, PreparationFailure> {
        if let Err(error) = self.prepare_owned(cancel) {
            return Err(PreparationFailure { error, owner: self });
        }
        let Self {
            candidate,
            record,
            recipe: _,
            request: _,
            slot,
        } = self;
        Ok(CountedBank {
            actual: candidate.expect("Completed preparation owns actual bank"),
            record: record.expect("Completed preparation owns bound record"),
            _slot: slot,
        })
    }
    fn prepare_owned(&mut self, cancel: &AtomicBool) -> Result<(), FactoryError> {
        if self.slot.state.closed.load(Ordering::Acquire) {
            return Err(FactoryError::Closed);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(FactoryError::Cancelled);
        }
        if self.candidate.is_none() {
            let target = if let Some(target) = self.request.initial_spatial {
                target
            } else {
                SpatialPlan::prepare(
                    self.recipe.settings.spatial,
                    self.request.format.sample_rate,
                    self.recipe.sequence,
                )
                .map_err(|_| FactoryError::Preparation)?
                .target(SpatialStep::ORIGIN)
            };
            let convolution = self
                .recipe
                .actual_impulse
                .as_ref()
                .map(|impulse| {
                    PreparedConvolution::prepare(
                        self.request.format,
                        impulse,
                        self.recipe.normalize,
                        cancel,
                    )
                })
                .transpose()
                .map_err(|_| {
                    if cancel.load(Ordering::Acquire) {
                        FactoryError::Cancelled
                    } else {
                        FactoryError::Preparation
                    }
                })?;
            let preparation = EffectsPreparation {
                format: self.request.format,
                device_channels: self.request.device_channels,
                main_gain: self.recipe.settings.convolution.main_gain,
                send_gain: self.recipe.settings.convolution.send_gain,
                alignment: if convolution.is_some() {
                    EffectsAlignment::Partition512
                } else {
                    EffectsAlignment::Direct
                },
                sequence: self.recipe.sequence,
                token: self.request.token,
                source_epoch: self.request.source_epoch,
                initial_spatial: target,
                replay_capacity: self.request.replay_capacity,
            };
            let actual =
                PreparedEffects::prepare(preparation, convolution, cancel).map_err(|_| {
                    if cancel.load(Ordering::Acquire) {
                        FactoryError::Cancelled
                    } else {
                        FactoryError::Preparation
                    }
                })?;
            self.candidate = Some(Box::new(actual));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(FactoryError::Cancelled);
        }
        if self.slot.state.closed.load(Ordering::Acquire) {
            return Err(FactoryError::Closed);
        }
        if self.record.is_none() {
            let actual = self.candidate.as_ref().unwrap();
            let bounds =
                HandoffBounds::try_arc_from_graph(actual).map_err(|_| FactoryError::Quota)?;
            let bytes = arc_bytes::<PreparedBankRecord>() + size_of::<CountedBank>();
            let reservation = DspReservation::new(bytes).map_err(|_| FactoryError::Quota)?;
            self.record = Some(Arc::new(PreparedBankRecord {
                owner_id: actual.handoff_owner(),
                preparation: actual.preparation(),
                recipe: self.recipe.clone(),
                bounds,
                pool: self.slot.state.clone(),
                metadata_bytes: bytes,
                _reservation: reservation,
            }));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(FactoryError::Cancelled);
        }
        if self.slot.state.closed.load(Ordering::Acquire) {
            return Err(FactoryError::Closed);
        }
        Ok(())
    }
}

/// Unique actual bank, immutable provenance record, and its inseparable slot.
pub struct CountedBank {
    actual: Box<PreparedEffects>,
    record: Arc<PreparedBankRecord>,
    _slot: BankSlot,
}
impl CountedBank {
    pub(crate) fn belongs_to(&self, pool: &EffectsBankPool) -> bool {
        Arc::ptr_eq(&self._slot.state, &pool.state)
    }
    pub(crate) fn actual_mut(&mut self) -> &mut PreparedEffects {
        &mut self.actual
    }
    pub fn graph(&self) -> &PreparedEffects {
        &self.actual
    }
    pub fn record(&self) -> &Arc<PreparedBankRecord> {
        &self.record
    }
    /// Per-bank storage; shared recipe/IR storage is accounted by its own owners.
    pub fn bank_owned_bytes(&self) -> usize {
        self.actual.owned_bytes()
            + self.record.metadata_owned_bytes()
            + self.record.bounds.arc_reserved_bytes()
    }
    pub fn frame(&mut self, input: Option<EffectsInput>) -> Result<EffectsFrame, EffectsError> {
        self.actual.frame(input)
    }
    pub fn pending_output(&self) -> Option<EffectsFrame> {
        self.actual.pending_output()
    }
    pub fn release_pending(
        &mut self,
        frame: &EffectsFrame,
        presented: bool,
    ) -> Result<(), EffectsError> {
        self.actual.release_pending(frame, presented)
    }
    pub fn begin_eof(&mut self) -> Result<(), EffectsError> {
        self.actual.begin_eof()
    }
    pub fn drain(&mut self) -> Result<Option<EffectsFrame>, EffectsError> {
        self.actual.drain()
    }
    pub fn drained(&self) -> bool {
        self.actual.drained()
    }
    pub fn retarget_spatial(&mut self, target: SpatialTarget) -> bool {
        self.actual.retarget_spatial(target)
    }
}

/// An already prepared compatibility Box has no retained IR/recipe record.
/// Field destruction drops the actual bank before returning its pool slot.
pub(crate) struct LegacyBank {
    actual: Box<PreparedEffects>,
    _slot: BankSlot,
}
impl LegacyBank {
    pub(crate) fn graph(&self) -> &PreparedEffects {
        &self.actual
    }
    pub(crate) fn actual_mut(&mut self) -> &mut PreparedEffects {
        &mut self.actual
    }
    #[cfg(test)]
    pub(crate) fn belongs_to(&self, pool: &EffectsBankPool) -> bool {
        Arc::ptr_eq(&self._slot.state, &pool.state)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CountedHandoffError {
    Closed,
    ForeignPool,
    WrongOwner,
    NotReady,
    Quota,
    Handoff(HandoffError),
}
pub(crate) struct CountedReserveFailure {
    pub(crate) error: CountedHandoffError,
    pub(crate) candidate: Option<CountedBank>,
}
impl CountedReserveFailure {
    /// The device owner retires the actual graph before returning its bank slot.
    pub(crate) fn retire_candidate_on_owner(self) -> CountedHandoffError {
        let Self { error, candidate } = self;
        drop(candidate);
        error
    }
}
pub(crate) struct CountedCaptureFailure {
    pub(crate) error: CountedHandoffError,
    pub(crate) reservation: CountedHandoffReservation,
    pub(crate) old: CountedBank,
}
pub(crate) struct CountedRetargetFailure {
    pub(crate) error: CountedHandoffError,
    pub(crate) candidate: CountedBank,
}
pub(crate) struct CountedCurrentCaptureFailure {
    pub(crate) error: CountedHandoffError,
    detached: Option<CountedBank>,
    pub(crate) old: CountedHandoff,
    pub(crate) reservation: CountedHandoffReservation,
}
impl CountedCurrentCaptureFailure {
    pub(crate) fn record(&self) -> &Arc<PreparedBankRecord> {
        &self.reservation.old_record
    }
    fn restore_detached(&mut self) -> bool {
        let Some(old) = self.detached.take() else {
            return true;
        };
        match self.old.restore_detached_current_on_owner(old) {
            Ok(()) => true,
            Err(old) => {
                self.detached = Some(old);
                false
            }
        }
    }
    #[cfg(test)]
    #[expect(
        clippy::result_large_err,
        reason = "The unit recovery helper preserves both actual owners and all reserved FIFO storage without allocating a replacement error container."
    )]
    pub(crate) fn retry_capture(mut self, cancel: &AtomicBool) -> Result<CountedHandoff, Self> {
        if !self.restore_detached() {
            return Err(self);
        }
        self.old
            .capture_current_for_recovery_on_owner(self.reservation, cancel)
    }
    pub(crate) fn owned_record_ids(&self) -> [Option<u64>; 2] {
        [
            self.detached
                .as_ref()
                .map(|bank| bank.record.owner_id)
                .or_else(|| self.old.new_record.as_ref().map(|record| record.owner_id)),
            self.reservation
                .candidate_record
                .as_ref()
                .map(|record| record.owner_id),
        ]
    }
    #[expect(
        clippy::result_large_err,
        reason = "Recovery returns all inline bank, slot and FIFO owners unchanged when detachment cannot be restored."
    )]
    pub(crate) fn recover_old_on_owner(mut self) -> Result<(CountedHandoff, Option<u64>), Self> {
        if !self.restore_detached() {
            return Err(self);
        }
        let candidate_id = self
            .reservation
            .candidate_record
            .as_ref()
            .map(|record| record.owner_id);
        drop(self.reservation);
        Ok((self.old, candidate_id))
    }
}
impl CountedCaptureFailure {
    /// Initial capture rejection has not evolved the old graph. Retire only
    /// the real candidate on its owner, returning the unchanged counted old bank.
    pub(crate) fn recover_old_on_owner(self) -> (CountedBank, Option<u64>) {
        let candidate_id = self
            .reservation
            .candidate_record
            .as_ref()
            .map(|record| record.owner_id);
        drop(self.reservation);
        (self.old, candidate_id)
    }
}
/// The callback retains its old owner while this actual candidate is prepared.
pub(crate) struct CountedHandoffReservation {
    // Actual candidate Box is destroyed before its slot on every failure path.
    actual: HandoffReservation,
    old_record: Arc<PreparedBankRecord>,
    candidate_record: Option<Arc<PreparedBankRecord>>,
    candidate_slot: Option<BankSlot>,
    same_format: bool,
    _reservation: DspReservation,
}
impl CountedHandoffReservation {
    pub(crate) fn reserve(
        old_record: Arc<PreparedBankRecord>,
        format: PcmFormat,
        device_channels: u16,
        candidate: Option<CountedBank>,
        cancel: &AtomicBool,
    ) -> Result<Self, CountedReserveFailure> {
        if old_record.pool.closed.load(Ordering::Acquire) {
            return Err(CountedReserveFailure {
                error: CountedHandoffError::Closed,
                candidate,
            });
        }
        if candidate
            .as_ref()
            .is_some_and(|candidate| !Arc::ptr_eq(&candidate._slot.state, &old_record.pool))
        {
            return Err(CountedReserveFailure {
                error: CountedHandoffError::ForeignPool,
                candidate,
            });
        }
        let bytes = counted_handoff_owner_bytes();
        let reservation = match DspReservation::new(bytes) {
            Ok(value) => value,
            Err(_) => {
                return Err(CountedReserveFailure {
                    error: CountedHandoffError::Quota,
                    candidate,
                });
            }
        };
        let has_candidate = candidate.is_some();
        let (actual, candidate_record, candidate_slot) = match candidate {
            Some(CountedBank {
                actual,
                record,
                _slot,
            }) => (Some(actual), Some(record), Some(_slot)),
            None => (None, None, None),
        };
        let same_format = format == old_record.bounds.old_format()
            && device_channels == old_record.bounds.old_device_channels()
            && !has_candidate;
        match HandoffReservation::reserve_shared(
            old_record.bounds.clone(),
            format,
            device_channels,
            actual,
            cancel,
        ) {
            Ok(actual) => Ok(Self {
                actual,
                old_record,
                candidate_record,
                candidate_slot,
                same_format,
                _reservation: reservation,
            }),
            Err(failure) => {
                let candidate = match (failure.new_graph, candidate_record, candidate_slot) {
                    (Some(actual), Some(record), Some(slot)) => Some(CountedBank {
                        actual,
                        record,
                        _slot: slot,
                    }),
                    (None, None, None) => None,
                    _ => unreachable!("Reservation preserves the exact actual candidate owner"),
                };
                Err(CountedReserveFailure {
                    error: CountedHandoffError::Handoff(failure.error),
                    candidate,
                })
            }
        }
    }
    /// Only after actual callback handback; exact record identity is mandatory.
    #[expect(
        clippy::result_large_err,
        reason = "Capture rejection preserves the precharged reservation and actual old bank without allocating an error box."
    )]
    pub(crate) fn capture(
        self,
        old: CountedBank,
        cancel: &AtomicBool,
    ) -> Result<CountedHandoff, CountedCaptureFailure> {
        if self.old_record.pool.closed.load(Ordering::Acquire) {
            return Err(CountedCaptureFailure {
                error: CountedHandoffError::Closed,
                reservation: self,
                old,
            });
        }
        if !Arc::ptr_eq(&old._slot.state, &self.old_record.pool) {
            return Err(CountedCaptureFailure {
                error: CountedHandoffError::ForeignPool,
                reservation: self,
                old,
            });
        }
        if !Arc::ptr_eq(&old.record, &self.old_record) {
            return Err(CountedCaptureFailure {
                error: CountedHandoffError::WrongOwner,
                reservation: self,
                old,
            });
        }
        if cancel.load(Ordering::Acquire) {
            return Err(CountedCaptureFailure {
                error: CountedHandoffError::Handoff(HandoffError::Cancelled),
                reservation: self,
                old,
            });
        }
        if self.same_format {
            let Self {
                actual,
                old_record,
                candidate_record: _,
                candidate_slot: _,
                same_format: _,
                _reservation,
            } = self;
            // This reservation contains no candidate; keep the CountedBank intact.
            drop(actual);
            return Ok(CountedHandoff {
                body: Some(CountedHandoffBody::Whole(old)),
                previous_storage: None,
                merge_cursor: 0,
                old_record: None,
                old_slot: None,
                new_record: Some(old_record),
                new_slot: None,
                _reservation: Some(_reservation),
            });
        }
        let CountedBank {
            actual,
            record,
            _slot,
        } = old;
        let Self {
            actual: reservation,
            old_record,
            candidate_record,
            candidate_slot,
            same_format: _,
            _reservation,
        } = self;
        match reservation.capture(actual, cancel) {
            Ok(actual) => Ok(CountedHandoff {
                body: Some(CountedHandoffBody::Retained(actual)),
                previous_storage: None,
                merge_cursor: 0,
                old_record: Some(old_record),
                old_slot: Some(_slot),
                new_record: candidate_record,
                new_slot: candidate_slot,
                _reservation: Some(_reservation),
            }),
            Err(failure) => Err(CountedCaptureFailure {
                error: CountedHandoffError::Handoff(failure.error),
                reservation: Self {
                    actual: failure.reservation,
                    old_record,
                    candidate_record,
                    candidate_slot,
                    same_format: false,
                    _reservation,
                },
                old: CountedBank {
                    actual: failure.old_graph,
                    record,
                    _slot,
                },
            }),
        }
    }
}
#[expect(
    clippy::large_enum_variant,
    reason = "Inline retained state moves through preallocated callback queues; boxing would add an uncharged allocation and alter teardown."
)]
enum CountedHandoffBody {
    Whole(CountedBank),
    Retained(RetainedHandoff),
}
/// Every actual Box drops before its associated private slot, including cancellation.
pub(crate) struct CountedHandoff {
    body: Option<CountedHandoffBody>,
    previous_storage: Option<RetiredHandoffStorage>,
    merge_cursor: usize,
    old_record: Option<Arc<PreparedBankRecord>>,
    old_slot: Option<BankSlot>,
    new_record: Option<Arc<PreparedBankRecord>>,
    new_slot: Option<BankSlot>,
    _reservation: Option<DspReservation>,
}
/// No bank or slot remains in this envelope. Its buffers and allocation permit
/// are moved to the preallocated owner-return queue, then destroyed by the owner.
pub(crate) struct RetiredHandoffStorage {
    retained: Option<RetainedHandoff>,
    _extra_record: Option<Arc<PreparedBankRecord>>,
    _reservation: Option<DspReservation>,
}
impl RetiredHandoffStorage {
    #[cfg(test)]
    pub(crate) fn owned_buffer_bytes(&self) -> usize {
        self.retained
            .as_ref()
            .map_or(0, RetainedHandoff::owned_buffer_bytes)
            + if self._reservation.is_some() {
                counted_handoff_owner_bytes()
            } else {
                0
            }
    }
    #[cfg(test)]
    pub(crate) fn retained_record(&self) -> Option<&Arc<PreparedBankRecord>> {
        self._extra_record.as_ref()
    }
}
impl CountedHandoff {
    pub(crate) fn can_capture_current_on_owner(&self) -> bool {
        self.previous_storage.is_none()
            && self.old_record.is_none()
            && self.old_slot.is_none()
            && self.new_slot.is_some()
            && self
                .new_record
                .as_ref()
                .is_some_and(|record| !record.pool.closed.load(Ordering::Acquire))
            && match self.body.as_ref() {
                Some(CountedHandoffBody::Retained(actual)) => {
                    actual.can_detach_current_for_recovery_on_owner()
                }
                _ => false,
            }
    }
    /// Owner-side only. No presentation ACK is fabricated, and the current
    /// actual bank carries its existing slot into one fresh capture.
    #[expect(
        clippy::result_large_err,
        reason = "A failed recapture must return both actual counted owners and the complete retained FIFO under the original reservation."
    )]
    pub(crate) fn capture_current_for_recovery_on_owner(
        mut self,
        mut reservation: CountedHandoffReservation,
        cancel: &AtomicBool,
    ) -> Result<Self, CountedCurrentCaptureFailure> {
        let current = self.new_record.as_ref();
        let error = if current.is_some_and(|record| record.pool.closed.load(Ordering::Acquire)) {
            Some(CountedHandoffError::Closed)
        } else if !self.can_capture_current_on_owner() || reservation.same_format {
            Some(CountedHandoffError::NotReady)
        } else if !Arc::ptr_eq(&current.unwrap().pool, &reservation.old_record.pool) {
            Some(CountedHandoffError::ForeignPool)
        } else if !Arc::ptr_eq(current.unwrap(), &reservation.old_record) {
            Some(CountedHandoffError::WrongOwner)
        } else if cancel.load(Ordering::Acquire) {
            Some(CountedHandoffError::Handoff(HandoffError::Cancelled))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(CountedCurrentCaptureFailure {
                error,
                detached: None,
                old: self,
                reservation,
            });
        }
        let Some(CountedHandoffBody::Retained(previous)) = self.body.as_mut() else {
            unreachable!()
        };
        if let Err(error) = reservation
            .actual
            .prepare_previous_tail_for_recovery_on_owner(previous, cancel)
        {
            return Err(CountedCurrentCaptureFailure {
                error: CountedHandoffError::Handoff(error),
                detached: None,
                old: self,
                reservation,
            });
        }
        let Some(actual) = previous.take_current_for_recovery_on_owner() else {
            return Err(CountedCurrentCaptureFailure {
                error: CountedHandoffError::NotReady,
                detached: None,
                old: self,
                reservation,
            });
        };
        let old = CountedBank {
            actual,
            record: self
                .new_record
                .take()
                .expect("Preflight holds actual A record"),
            _slot: self.new_slot.take().expect("Preflight holds actual A slot"),
        };
        match reservation.capture(old, cancel) {
            Ok(mut captured) => {
                let Some(CountedHandoffBody::Retained(previous)) = self.body.take() else {
                    unreachable!()
                };
                captured.previous_storage = Some(RetiredHandoffStorage {
                    retained: Some(previous),
                    _extra_record: captured.old_record.as_ref().map(Arc::clone),
                    _reservation: self._reservation.take(),
                });
                captured.merge_cursor = 0;
                Ok(captured)
            }
            Err(failure) => {
                let detached = self.restore_detached_current_on_owner(failure.old).err();
                Err(CountedCurrentCaptureFailure {
                    error: failure.error,
                    detached,
                    old: self,
                    reservation: failure.reservation,
                })
            }
        }
    }
    fn restore_detached_current_on_owner(&mut self, old: CountedBank) -> Result<(), CountedBank> {
        if self.new_record.is_some() || self.new_slot.is_some() {
            return Err(old);
        }
        let Some(CountedHandoffBody::Retained(previous)) = self.body.as_mut() else {
            return Err(old);
        };
        let CountedBank {
            actual,
            record,
            _slot,
        } = old;
        match previous.restore_current_after_failed_capture_on_owner(actual) {
            Ok(()) => {
                self.new_record = Some(record);
                self.new_slot = Some(_slot);
                Ok(())
            }
            Err(actual) => Err(CountedBank {
                actual,
                record,
                _slot,
            }),
        }
    }
    pub(crate) fn owned_record_ids(&self) -> [Option<u64>; 2] {
        [
            self.old_record.as_ref().map(|record| record.owner_id),
            self.new_record.as_ref().map(|record| record.owner_id),
        ]
    }
    pub(crate) fn can_retarget_unpresented_on_owner(&self) -> bool {
        self.previous_storage.is_none()
            && self.old_slot.is_none()
            && self.old_record.is_none()
            && self.new_slot.is_some()
            && self
                .new_record
                .as_ref()
                .is_some_and(|record| !record.pool.closed.load(Ordering::Acquire))
            && match self.body.as_ref() {
                Some(CountedHandoffBody::Retained(actual)) => {
                    actual.can_retarget_unpresented_on_owner()
                }
                _ => false,
            }
    }
    /// Owner-only replacement. The preserved FIFO/labels retain their original
    /// Fs; actual A is destroyed before A's slot is released, while B's already
    /// reserved slot travels unchanged with the actual new Box.
    pub(crate) fn retarget_unpresented_on_owner(
        &mut self,
        candidate: CountedBank,
    ) -> Result<u64, CountedRetargetFailure> {
        let Some(current) = self.new_record.as_ref() else {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::NotReady,
                candidate,
            });
        };
        if current.pool.closed.load(Ordering::Acquire) {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::Closed,
                candidate,
            });
        }
        if !Arc::ptr_eq(&candidate._slot.state, &current.pool) {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::ForeignPool,
                candidate,
            });
        }
        if !self.can_retarget_unpresented_on_owner() {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::NotReady,
                candidate,
            });
        }
        let plan = candidate.actual.preparation();
        if candidate.actual.handoff_owner() != candidate.record.owner_id {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::WrongOwner,
                candidate,
            });
        }
        if plan.token != current.preparation.token
            || plan.source_epoch != current.preparation.source_epoch
            || !candidate.actual.can_accept_input()
            || candidate.actual.statistics().accepted_media_frames != 0
            || candidate.actual.pending_output().is_some()
        {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::Handoff(HandoffError::WrongStream),
                candidate,
            });
        }
        let retired_id = current.owner_id;
        let CountedBank {
            actual,
            record,
            _slot,
        } = candidate;
        let Some(CountedHandoffBody::Retained(retained)) = self.body.as_mut() else {
            unreachable!()
        };
        match retained.retarget_unpresented_on_owner(actual) {
            Ok(previous) => {
                drop(previous);
                drop(self.new_slot.take());
                self.new_slot = Some(_slot);
                self.new_record = Some(record);
                Ok(retired_id)
            }
            Err(failure) => Err(CountedRetargetFailure {
                error: CountedHandoffError::Handoff(failure.error),
                candidate: CountedBank {
                    actual: failure.new_graph.expect("Retarget preserves actual B"),
                    record,
                    _slot,
                },
            }),
        }
    }
    pub(crate) fn can_retarget_prefix_remainder_on_owner(&self) -> bool {
        self.previous_storage.is_none()
            && self.old_slot.is_none()
            && self.old_record.is_none()
            && self.new_slot.is_some()
            && self
                .new_record
                .as_ref()
                .is_some_and(|record| !record.pool.closed.load(Ordering::Acquire))
            && match self.body.as_ref() {
                Some(CountedHandoffBody::Retained(actual)) => {
                    actual.can_retarget_prefix_remainder_on_owner()
                }
                _ => false,
            }
    }
    /// Replaces only a fresh destination bank while the original media prefix
    /// still remains. Its exact presentation progress and FIFO stay in the core.
    pub(crate) fn retarget_prefix_remainder_on_owner(
        &mut self,
        candidate: CountedBank,
    ) -> Result<u64, CountedRetargetFailure> {
        let Some(current) = self.new_record.as_ref() else {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::NotReady,
                candidate,
            });
        };
        if current.pool.closed.load(Ordering::Acquire) {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::Closed,
                candidate,
            });
        }
        if !Arc::ptr_eq(&candidate._slot.state, &current.pool) {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::ForeignPool,
                candidate,
            });
        }
        if !self.can_retarget_prefix_remainder_on_owner() {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::NotReady,
                candidate,
            });
        }
        let plan = candidate.actual.preparation();
        if candidate.actual.handoff_owner() != candidate.record.owner_id {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::WrongOwner,
                candidate,
            });
        }
        if plan.token != current.preparation.token
            || plan.source_epoch != current.preparation.source_epoch
            || !candidate.actual.can_accept_input()
            || candidate.actual.statistics().accepted_media_frames != 0
            || candidate.actual.pending_output().is_some()
        {
            return Err(CountedRetargetFailure {
                error: CountedHandoffError::Handoff(HandoffError::WrongStream),
                candidate,
            });
        }
        let retired_id = current.owner_id;
        let CountedBank {
            actual,
            record,
            _slot,
        } = candidate;
        let Some(CountedHandoffBody::Retained(retained)) = self.body.as_mut() else {
            unreachable!()
        };
        match retained.retarget_prefix_remainder_on_owner(actual) {
            Ok(previous) => {
                drop(previous);
                drop(self.new_slot.take());
                self.new_slot = Some(_slot);
                self.new_record = Some(record);
                Ok(retired_id)
            }
            Err(failure) => Err(CountedRetargetFailure {
                error: CountedHandoffError::Handoff(failure.error),
                candidate: CountedBank {
                    actual: failure
                        .new_graph
                        .expect("Prefix retarget preserves actual B"),
                    record,
                    _slot,
                },
            }),
        }
    }
    /// Caller first reserves an empty return-queue slot. No heap allocation,
    /// cloning, bank destruction, buffer destruction, or slot release occurs.
    pub(crate) fn try_take_finished_on_callback(
        &mut self,
    ) -> Option<(CountedBank, RetiredHandoffStorage)> {
        if self._reservation.is_none()
            || self.previous_storage.is_some()
            || self.old_slot.is_some()
            || self.old_record.is_some()
        {
            return None;
        }
        match self.body.as_ref()? {
            CountedHandoffBody::Whole(_) => {
                if self.new_slot.is_some() || self.new_record.is_none() {
                    return None;
                }
            }
            CountedHandoffBody::Retained(actual) => {
                if !actual.ready()
                    || !actual.media_prefix_drained()
                    || !actual.tail_drained()
                    || actual.pending_output().is_some()
                    || self.new_slot.is_none()
                    || self.new_record.is_none()
                {
                    return None;
                }
            }
        }
        if matches!(self.body, Some(CountedHandoffBody::Whole(_))) {
            let Some(CountedHandoffBody::Whole(bank)) = self.body.take() else {
                unreachable!()
            };
            let storage = RetiredHandoffStorage {
                retained: None,
                _extra_record: self.new_record.take(),
                _reservation: self._reservation.take(),
            };
            return Some((bank, storage));
        }
        let Some(CountedHandoffBody::Retained(actual)) = self.body.as_mut() else {
            unreachable!()
        };
        let actual_bank = actual.try_take_new_without_destroy()?;
        let bank = CountedBank {
            actual: actual_bank,
            record: self.new_record.take().unwrap(),
            _slot: self.new_slot.take().unwrap(),
        };
        let Some(CountedHandoffBody::Retained(retained)) = self.body.take() else {
            unreachable!()
        };
        let storage = RetiredHandoffStorage {
            retained: Some(retained),
            _extra_record: None,
            _reservation: self._reservation.take(),
        };
        Some((bank, storage))
    }
    pub(crate) fn ready(&self) -> bool {
        self.previous_storage.is_none()
            && match self.body.as_ref() {
                Some(CountedHandoffBody::Whole(_)) => true,
                Some(CountedHandoffBody::Retained(actual)) => actual.ready(),
                None => false,
            }
    }
    pub(crate) fn expected_new_bus(&self) -> PcmFormat {
        self.record().preparation.format
    }
    #[cfg(test)]
    pub(crate) fn device_channels(&self) -> u16 {
        self.record().preparation.device_channels
    }
    pub(crate) fn media_prefix_drained(&self) -> bool {
        self.previous_storage.is_none()
            && match self.body.as_ref().unwrap() {
                CountedHandoffBody::Whole(_) => true,
                CountedHandoffBody::Retained(actual) => actual.media_prefix_drained(),
            }
    }
    #[cfg(test)]
    pub(crate) fn pending_output(&self) -> Option<HandoffFrame> {
        match self.body.as_ref().unwrap() {
            CountedHandoffBody::Whole(_) => None,
            CountedHandoffBody::Retained(actual) => actual.pending_output(),
        }
    }
    pub(crate) fn record(&self) -> &Arc<PreparedBankRecord> {
        self.new_record.as_ref().unwrap()
    }
    #[cfg(test)]
    pub(crate) fn same_format(&self) -> bool {
        matches!(self.body, Some(CountedHandoffBody::Whole(_)))
    }
    pub(crate) fn advance_capture(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<bool, CountedHandoffError> {
        if self.record().pool.closed.load(Ordering::Acquire) {
            return Err(CountedHandoffError::Closed);
        }
        let Some(CountedHandoffBody::Retained(actual)) = self.body.as_mut() else {
            return Ok(true);
        };
        if !actual
            .advance_capture(cancel)
            .map_err(CountedHandoffError::Handoff)?
        {
            return Ok(false);
        }
        if let Some(previous) = &self.previous_storage {
            let previous = previous
                .retained
                .as_ref()
                .ok_or(CountedHandoffError::NotReady)?;
            if !actual
                .merge_previous_tail_on_owner(previous, &mut self.merge_cursor, cancel)
                .map_err(CountedHandoffError::Handoff)?
            {
                return Ok(false);
            }
            // Owner-side only: the previous FIFO and metadata stay charged
            // until the bounded merge has transferred all remaining samples.
            drop(self.previous_storage.take());
        }
        Ok(true)
    }
    /// The actual owner drops the retired old Box before its slot. Returns its real id.
    pub(crate) fn retire_old_on_owner(&mut self) -> Option<u64> {
        if !self.ready() {
            return None;
        }
        let CountedHandoffBody::Retained(actual) = self.body.as_mut()? else {
            return None;
        };
        let old = actual.take_retired_old()?;
        let id = self.old_record.as_ref()?.owner_id;
        drop(old);
        drop(self.old_slot.take());
        self.old_record = None;
        Some(id)
    }
    pub(crate) fn graph_mut(&mut self) -> Option<&mut PreparedEffects> {
        if self.previous_storage.is_some() {
            return None;
        }
        match self.body.as_mut().unwrap() {
            CountedHandoffBody::Whole(bank) => Some(bank.actual_mut()),
            CountedHandoffBody::Retained(actual) => actual.new_graph_mut(),
        }
    }
    pub(crate) fn retarget_spatial(&mut self, target: SpatialTarget) -> bool {
        match self.body.as_mut().unwrap() {
            CountedHandoffBody::Whole(bank) => bank.retarget_spatial(target),
            CountedHandoffBody::Retained(actual) => actual.retarget_spatial(target),
        }
    }
    pub(crate) fn next_media(&mut self) -> Result<Option<HandoffFrame>, HandoffError> {
        if self.previous_storage.is_some() {
            return Err(HandoffError::NotReady);
        }
        match self.body.as_mut().unwrap() {
            CountedHandoffBody::Whole(_) => Ok(None),
            CountedHandoffBody::Retained(actual) => actual.next_media(),
        }
    }
    pub(crate) fn next_tail(&mut self) -> Result<Option<HandoffFrame>, HandoffError> {
        if self.previous_storage.is_some() {
            return Err(HandoffError::NotReady);
        }
        match self.body.as_mut().unwrap() {
            CountedHandoffBody::Whole(_) => Ok(None),
            CountedHandoffBody::Retained(actual) => actual.next_tail(),
        }
    }
    pub(crate) fn release_pending(
        &mut self,
        frame: &HandoffFrame,
        presented: bool,
    ) -> Result<(), HandoffError> {
        if self.previous_storage.is_some() {
            return Err(HandoffError::NotReady);
        }
        match self.body.as_mut().unwrap() {
            CountedHandoffBody::Whole(_) => Err(HandoffError::InvalidReceipt),
            CountedHandoffBody::Retained(actual) => actual.release_pending(frame, presented),
        }
    }
    pub(crate) fn retained_output_drained(&self) -> bool {
        self.previous_storage.is_none()
            && match self.body.as_ref().unwrap() {
                CountedHandoffBody::Whole(_) => true,
                CountedHandoffBody::Retained(actual) => {
                    actual.media_prefix_drained()
                        && actual.tail_drained()
                        && actual.pending_output().is_none()
                }
            }
    }
    /// Reattach an existing counted owner; do not acquire/install a second slot.
    #[cfg(test)]
    #[expect(
        clippy::result_large_err,
        reason = "An unfinished unit handoff returns its exact inline graph, slot and retained output owners so ownership and byte assertions remain intact."
    )]
    pub(crate) fn finish_on_owner(mut self) -> Result<CountedBank, Self> {
        if self.previous_storage.is_some() {
            return Err(self);
        }
        if matches!(self.body, Some(CountedHandoffBody::Whole(_))) {
            let Some(CountedHandoffBody::Whole(bank)) = self.body.take() else {
                unreachable!()
            };
            return Ok(bank);
        }
        if self.old_slot.is_some() || !self.retained_output_drained() {
            return Err(self);
        }
        let Some(CountedHandoffBody::Retained(actual)) = self.body.as_mut() else {
            unreachable!()
        };
        let Some(bank) = actual.take_new_on_owner() else {
            return Err(self);
        };
        Ok(CountedBank {
            actual: bank,
            record: self.new_record.take().unwrap(),
            _slot: self.new_slot.take().unwrap(),
        })
    }
}
fn counted_handoff_owner_bytes() -> usize {
    size_of::<CountedHandoffReservation>()
        + size_of::<CountedHandoff>()
        + size_of::<RetiredHandoffStorage>()
        + size_of::<CountedCurrentCaptureFailure>()
}
fn arc_bytes<T>() -> usize {
    size_of::<T>() + 2 * size_of::<usize>() + align_of::<T>()
}
fn id_capacity(id: &ImpulseResourceId) -> usize {
    match id {
        ImpulseResourceId::Bundled(value) | ImpulseResourceId::Imported(value) => value.capacity(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::processing::DSP_QUOTA_BYTES;

    fn recipe() -> Arc<EffectsRecipe> {
        EffectsRecipe::prepare(EffectsSettings::default(), 1, 2, false, None).unwrap()
    }
    fn request() -> BankRequest {
        BankRequest {
            format: PcmFormat {
                sample_rate: 8_000,
                channels: 2,
            },
            device_channels: 2,
            token: 1,
            source_epoch: 2,
            replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
            initial_spatial: None,
        }
    }
    #[test]
    fn actual_bounds_arc_allocation_stays_charged_until_its_last_owner_drops() {
        let baseline = DspReservation::used_bytes();
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let bank = pool
            .begin(recipe.clone(), request())
            .unwrap()
            .prepare(&AtomicBool::new(false))
            .unwrap();
        let bounds = bank.record().bounds().clone();
        let bytes = bounds.arc_reserved_bytes();
        assert!(bytes > 0);
        drop(bank);
        assert_eq!(pool.occupied_banks(), 0);
        drop(recipe);
        drop(pool);
        assert_eq!(DspReservation::used_bytes(), baseline + bytes);
        let last = bounds.clone();
        drop(bounds);
        assert_eq!(DspReservation::used_bytes(), baseline + bytes);
        drop(last);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
    #[test]
    fn completed_box_cancellation_keeps_slot_and_all_storage_until_actual_drop() {
        let baseline = DspReservation::used_bytes();
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut holder = pool.begin(recipe.clone(), request()).unwrap();
        holder.prepare_owned(&AtomicBool::new(false)).unwrap();
        assert!(holder.has_actual_bank());
        assert!(holder.record.is_some());
        let charged = DspReservation::used_bytes();
        assert!(charged > baseline);
        let failure = holder.prepare(&AtomicBool::new(true)).err().unwrap();
        assert_eq!(failure.error, FactoryError::Cancelled);
        assert!(failure.owner.has_actual_bank());
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(DspReservation::used_bytes(), charged);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 0);
        assert!(DspReservation::used_bytes() < charged);
        drop(recipe);
        drop(pool);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
    #[test]
    fn preparation_quota_failure_retains_the_slot_owned_holder_for_real_retirement() {
        let baseline = DspReservation::used_bytes();
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let owner = pool.begin(recipe.clone(), request()).unwrap();
        let filler = DspReservation::new(DSP_QUOTA_BYTES - DspReservation::used_bytes()).unwrap();
        let failure = owner.prepare(&AtomicBool::new(false)).err().unwrap();
        assert_eq!(failure.error, FactoryError::Preparation);
        assert!(!failure.owner.has_actual_bank());
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(DspReservation::used_bytes(), DSP_QUOTA_BYTES);
        drop(filler);
        assert_eq!(pool.occupied_banks(), 1);
        let completed = failure.owner.prepare(&AtomicBool::new(false)).unwrap();
        assert_eq!(pool.occupied_banks(), 1);
        drop(completed);
        assert_eq!(pool.occupied_banks(), 0);
        drop(recipe);
        drop(pool);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
    fn legacy_box() -> Box<PreparedEffects> {
        let target = SpatialPlan::prepare(Default::default(), 8_000, 1)
            .unwrap()
            .target(SpatialStep::ORIGIN);
        Box::new(
            PreparedEffects::prepare(
                EffectsPreparation {
                    format: PcmFormat {
                        sample_rate: 8_000,
                        channels: 2,
                    },
                    device_channels: 2,
                    main_gain: 1.0,
                    send_gain: 0.0,
                    alignment: EffectsAlignment::Direct,
                    sequence: 1,
                    token: 1,
                    source_epoch: 2,
                    initial_spatial: target,
                    replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
                },
                None,
                &AtomicBool::new(false),
            )
            .unwrap(),
        )
    }
    #[test]
    fn legacy_busy_or_closed_adoption_returns_the_exact_actual_box() {
        let pool = EffectsBankPool::new().unwrap();
        let first = pool
            .adopt_legacy(legacy_box())
            .unwrap_or_else(|_| panic!("admission"));
        let second = pool
            .adopt_legacy(legacy_box())
            .unwrap_or_else(|_| panic!("admission"));
        let original = legacy_box();
        let identity = original.handoff_owner();
        let pointer = &*original as *const PreparedEffects;
        let returned = match pool.adopt_legacy(original) {
            Ok(_) => panic!("third legacy"),
            Err(bank) => bank,
        };
        assert_eq!(returned.handoff_owner(), identity);
        assert_eq!(&*returned as *const PreparedEffects, pointer);
        assert_eq!(pool.occupied_banks(), 2);
        drop(first);
        assert_eq!(pool.occupied_banks(), 1);
        pool.begin_shutdown();
        let returned = match pool.adopt_legacy(returned) {
            Ok(_) => panic!("closed legacy"),
            Err(bank) => bank,
        };
        assert_eq!(returned.handoff_owner(), identity);
        assert_eq!(&*returned as *const PreparedEffects, pointer);
        assert_eq!(pool.occupied_banks(), 1);
        drop(second);
        assert_eq!(pool.occupied_banks(), 0);
        drop(returned);
    }
    #[test]
    fn counted_and_legacy_pool_identity_follows_actual_shared_state() {
        let pool = EffectsBankPool::new().unwrap();
        let same = pool.clone();
        let foreign = EffectsBankPool::new().unwrap();
        let mut counted = pool
            .begin(recipe(), request())
            .unwrap()
            .prepare(&AtomicBool::new(false))
            .unwrap();
        assert!(counted.belongs_to(&pool));
        assert!(counted.belongs_to(&same));
        assert!(!counted.belongs_to(&foreign));
        assert_eq!(counted.actual_mut().preparation().token, 1);
        let mut legacy = pool
            .adopt_legacy(legacy_box())
            .unwrap_or_else(|_| panic!("admission"));
        assert!(legacy.belongs_to(&pool));
        assert!(legacy.belongs_to(&same));
        assert!(!legacy.belongs_to(&foreign));
        assert_eq!(legacy.graph().preparation().token, 1);
        assert_eq!(legacy.actual_mut().preparation().source_epoch, 2);
        assert_eq!(legacy.actual_mut().effect_channels(), 2);
        assert_eq!(foreign.occupied_banks(), 0);
        drop(counted);
        assert_eq!(pool.occupied_banks(), 1);
        drop(legacy);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn closed_completed_preparation_retains_its_box_and_slot() {
        let pool = EffectsBankPool::new().unwrap();
        let mut holder = pool.begin(recipe(), request()).unwrap();
        holder.prepare_owned(&AtomicBool::new(false)).unwrap();
        let bytes = DspReservation::used_bytes();
        pool.begin_shutdown();
        let failure = holder.prepare(&AtomicBool::new(false)).err().unwrap();
        assert_eq!(failure.error, FactoryError::Closed);
        assert!(failure.owner.has_actual_bank());
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(DspReservation::used_bytes(), bytes);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 0);
        assert!(DspReservation::used_bytes() < bytes);
    }
    fn counted_candidate(
        pool: &EffectsBankPool,
        recipe: Arc<EffectsRecipe>,
        rate: u32,
        device: u16,
    ) -> CountedBank {
        let mut requested = request();
        requested.format.sample_rate = rate;
        requested.device_channels = device;
        pool.begin(recipe, requested)
            .unwrap()
            .prepare(&AtomicBool::new(false))
            .unwrap()
    }
    fn pulse(bank: &mut CountedBank) {
        let stamp = super::super::convolution::ConvolutionStamp {
            token: 1,
            source_epoch: 2,
            media: super::super::pcm::ProcessingStamp {
                end_frame: 1,
                ..Default::default()
            },
        };
        bank.frame(Some(EffectsInput {
            samples: [0.1; MAX_CHANNELS],
            stamp,
            begin: Some(super::super::convolution::ConvolutionStamp {
                media: super::super::pcm::ProcessingStamp {
                    end_frame: 0,
                    ..stamp.media
                },
                ..stamp
            }),
            pitch: super::super::effects::EffectsPitch {
                ratio: 1.0,
                sequence: 1,
                transition_remaining_frames: 0,
            },
        }))
        .unwrap();
    }
    #[test]
    fn counted_handoff_keeps_both_slots_until_actual_old_box_retirement_and_preserves_new_slot() {
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut old = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        pulse(&mut old);
        let old_id = old.record().owner_id();
        let record = old.record().clone();
        let candidate = counted_candidate(&pool, recipe.clone(), 16_000, 1);
        let new_id = candidate.record().owner_id();
        let reservation = CountedHandoffReservation::reserve(
            record,
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        assert_eq!(pool.occupied_banks(), 2);
        assert!(matches!(
            pool.begin(recipe, request()),
            Err(FactoryError::Busy)
        ));
        let mut handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        while !handoff.advance_capture(&AtomicBool::new(false)).unwrap() {}
        assert_eq!(pool.occupied_banks(), 2);
        assert!(handoff.graph_mut().is_none());
        assert_eq!(handoff.retire_old_on_owner(), Some(old_id));
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(handoff.retire_old_on_owner(), None);
        assert_eq!(pool.occupied_banks(), 1);
        let target = SpatialPlan::prepare(Default::default(), 16_000, 20)
            .unwrap()
            .target(SpatialStep::ORIGIN);
        assert!(handoff.retarget_spatial(target));
        assert!(handoff.graph_mut().is_none());
        let held = handoff.next_media().unwrap().unwrap();
        handoff.release_pending(&held, false).unwrap();
        assert_eq!(pool.occupied_banks(), 1);
        handoff = handoff.finish_on_owner().err().unwrap();
        handoff.release_pending(&held, true).unwrap();
        while let Some(frame) = handoff.next_media().unwrap() {
            handoff.release_pending(&frame, true).unwrap();
        }
        assert!(handoff.retained_output_drained());
        let bank = handoff
            .finish_on_owner()
            .unwrap_or_else(|_| panic!("complete handoff"));
        assert_eq!(bank.record().owner_id(), new_id);
        assert_eq!(pool.occupied_banks(), 1);
        assert!(bank.belongs_to(&pool));
        drop(bank);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn same_format_counted_handoff_preserves_the_actual_whole_bank_and_single_slot() {
        let pool = EffectsBankPool::new().unwrap();
        let mut old = counted_candidate(&pool, recipe(), 8_000, 2);
        pulse(&mut old);
        let id = old.record().owner_id();
        let stats = old.graph().statistics();
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            request().format,
            2,
            None,
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        assert!(handoff.same_format());
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(handoff.graph_mut().unwrap().statistics(), stats);
        assert!(handoff.next_media().unwrap().is_none());
        let restored = handoff
            .finish_on_owner()
            .unwrap_or_else(|_| panic!("whole owner"));
        assert_eq!(restored.record().owner_id(), id);
        assert_eq!(restored.graph().statistics(), stats);
        assert_eq!(pool.occupied_banks(), 1);
        drop(restored);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn foreign_pool_and_reservation_cancel_return_the_actual_candidate_with_its_slot() {
        let pool = EffectsBankPool::new().unwrap();
        let foreign = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let old = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        let candidate = counted_candidate(&foreign, recipe.clone(), 16_000, 1);
        let id = candidate.record().owner_id();
        let failure = CountedHandoffReservation::reserve(
            old.record().clone(),
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert_eq!(failure.error, CountedHandoffError::ForeignPool);
        assert_eq!(failure.candidate.as_ref().unwrap().record().owner_id(), id);
        assert_eq!(foreign.occupied_banks(), 1);
        assert_eq!(
            failure.retire_candidate_on_owner(),
            CountedHandoffError::ForeignPool
        );
        assert_eq!(foreign.occupied_banks(), 0);
        let candidate = counted_candidate(&pool, recipe, 16_000, 1);
        let id = candidate.record().owner_id();
        let failure = CountedHandoffReservation::reserve(
            old.record().clone(),
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(true),
        )
        .err()
        .unwrap();
        assert_eq!(
            failure.error,
            CountedHandoffError::Handoff(HandoffError::Cancelled)
        );
        assert_eq!(failure.candidate.as_ref().unwrap().record().owner_id(), id);
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(
            failure.retire_candidate_on_owner(),
            CountedHandoffError::Handoff(HandoffError::Cancelled)
        );
        assert_eq!(pool.occupied_banks(), 1);
        drop(old);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn actual_handback_wrong_record_or_cancel_keeps_old_and_candidate_owners_for_retry() {
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let previous = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        let stale_record = previous.record().clone();
        drop(previous);
        let old = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        let id = old.record().owner_id();
        let candidate = counted_candidate(&pool, recipe.clone(), 16_000, 1);
        let reservation = CountedHandoffReservation::reserve(
            stale_record,
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let failure = reservation
            .capture(old, &AtomicBool::new(false))
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::WrongOwner);
        assert_eq!(failure.old.record().owner_id(), id);
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 0);
        let old = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        let candidate = counted_candidate(&pool, recipe, 16_000, 1);
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let failure = reservation
            .capture(old, &AtomicBool::new(true))
            .err()
            .unwrap();
        assert_eq!(
            failure.error,
            CountedHandoffError::Handoff(HandoffError::Cancelled)
        );
        assert_eq!(pool.occupied_banks(), 2);
        let mut retried = failure
            .reservation
            .capture(failure.old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        while !retried.advance_capture(&AtomicBool::new(false)).unwrap() {}
        assert_eq!(pool.occupied_banks(), 2);
        drop(retried);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn counted_ir_handoff_cannot_reattach_or_release_new_slot_before_tail_actual_ack() {
        let pool = EffectsBankPool::new().unwrap();
        let mut builder = crate::audio::convolution::ImpulseBuilder::prepare(8_000, 1, 3).unwrap();
        for value in [1.0, 0.0, 0.5] {
            builder.push_frame(&[value]).unwrap();
        }
        let source = OwnedImpulse::from_validated(
            builder.finish().unwrap(),
            ImpulseResourceId::Imported("unit-ir".into()),
            "a".repeat(64),
        )
        .unwrap();
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(source.source_id().clone());
        settings.convolution.main_gain = 0.0;
        settings.convolution.send_gain = 1.0;
        let recipe = EffectsRecipe::prepare(settings, 1, 2, false, Some(source)).unwrap();
        let mut old = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        pulse(&mut old);
        let candidate = counted_candidate(&pool, recipe, 16_000, 1);
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        while !handoff.advance_capture(&AtomicBool::new(false)).unwrap() {}
        handoff.retire_old_on_owner().unwrap();
        assert_eq!(pool.occupied_banks(), 1);
        while let Some(frame) = handoff.next_media().unwrap() {
            handoff.release_pending(&frame, true).unwrap();
        }
        handoff = handoff.finish_on_owner().err().unwrap();
        let mut count = 0;
        while let Some(frame) = handoff.next_tail().unwrap() {
            assert!(frame.stamp.is_none());
            assert_eq!(pool.occupied_banks(), 1);
            handoff.release_pending(&frame, true).unwrap();
            count += 1;
        }
        assert_eq!(count, 4);
        let actual = handoff
            .finish_on_owner()
            .unwrap_or_else(|_| panic!("acked tail"));
        assert_eq!(pool.occupied_banks(), 1);
        drop(actual);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn callback_finish_moves_exact_bank_and_retained_storage_without_releasing_any_bytes() {
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut old = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        pulse(&mut old);
        let candidate = counted_candidate(&pool, recipe, 16_000, 1);
        let new_id = candidate.record().owner_id();
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        assert!(handoff.try_take_finished_on_callback().is_none());
        while !handoff.advance_capture(&AtomicBool::new(false)).unwrap() {}
        assert!(handoff.try_take_finished_on_callback().is_none());
        assert_eq!(pool.occupied_banks(), 2);
        handoff.retire_old_on_owner().unwrap();
        let frame = handoff.next_media().unwrap().unwrap();
        handoff.release_pending(&frame, false).unwrap();
        let held = handoff.pending_output();
        let bytes = DspReservation::used_bytes();
        assert!(handoff.try_take_finished_on_callback().is_none());
        assert_eq!(handoff.pending_output(), held);
        assert_eq!(DspReservation::used_bytes(), bytes);
        handoff.release_pending(&frame, true).unwrap();
        while let Some(frame) = handoff.next_media().unwrap() {
            handoff.release_pending(&frame, true).unwrap();
        }
        let bytes = DspReservation::used_bytes();
        let (bank, storage) = handoff.try_take_finished_on_callback().unwrap();
        assert_eq!(bank.record().owner_id(), new_id);
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(DspReservation::used_bytes(), bytes);
        assert!(storage.owned_buffer_bytes() > 0);
        assert!(handoff.try_take_finished_on_callback().is_none());
        drop(handoff);
        assert_eq!(DspReservation::used_bytes(), bytes);
        drop(storage);
        assert!(DspReservation::used_bytes() < bytes);
        assert_eq!(pool.occupied_banks(), 1);
        drop(bank);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn whole_callback_finish_moves_original_owner_and_returns_metadata_to_owner() {
        let pool = EffectsBankPool::new().unwrap();
        let mut old = counted_candidate(&pool, recipe(), 8_000, 2);
        pulse(&mut old);
        let id = old.record().owner_id();
        let before = old.graph().statistics();
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            request().format,
            2,
            None,
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let bytes = DspReservation::used_bytes();
        let (bank, storage) = handoff.try_take_finished_on_callback().unwrap();
        assert_eq!(bank.record().owner_id(), id);
        assert_eq!(bank.graph().statistics(), before);
        assert_eq!(storage.retained_record().unwrap().owner_id(), id);
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(DspReservation::used_bytes(), bytes);
        drop(handoff);
        assert_eq!(DspReservation::used_bytes(), bytes);
        drop(storage);
        assert_eq!(pool.occupied_banks(), 1);
        drop(bank);
        assert_eq!(pool.occupied_banks(), 0);
    }
    fn unpresented_handoff(pool: &EffectsBankPool, recipe: Arc<EffectsRecipe>) -> CountedHandoff {
        let mut old = counted_candidate(pool, recipe.clone(), 8_000, 2);
        pulse(&mut old);
        let candidate = counted_candidate(pool, recipe, 16_000, 1);
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        while !handoff.advance_capture(&AtomicBool::new(false)).unwrap() {}
        handoff.retire_old_on_owner().unwrap();
        handoff
    }
    #[test]
    fn unpresented_retarget_moves_real_spsc_candidate_then_retires_actual_a_before_slot_release() {
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut handoff = unpresented_handoff(&pool, recipe.clone());
        let a = handoff.record().owner_id();
        let old_copy = handoff.next_media().unwrap().unwrap();
        handoff.release_pending(&old_copy, false).unwrap();
        assert!(handoff.can_retarget_unpresented_on_owner());
        let b = counted_candidate(&pool, recipe.clone(), 24_000, 2);
        let b_id = b.record().owner_id();
        let (mut send, mut receive) = rtrb::RingBuffer::new(1);
        assert!(send.push(b).is_ok());
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(handoff.pending_output(), Some(old_copy));
        let b = receive
            .pop()
            .unwrap_or_else(|_| panic!("actual SPSC owner"));
        assert_eq!(
            handoff
                .retarget_unpresented_on_owner(b)
                .unwrap_or_else(|failure| panic!("{:?}", failure.error)),
            a
        );
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(handoff.owned_record_ids(), [None, Some(b_id)]);
        assert_eq!(handoff.expected_new_bus().sample_rate, 24_000);
        assert!(handoff.pending_output().is_none());
        assert_eq!(
            handoff.release_pending(&old_copy, true),
            Err(HandoffError::InvalidReceipt)
        );
        let c = counted_candidate(&pool, recipe, 8_000, 1);
        let c_id = c.record().owner_id();
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(
            handoff
                .retarget_unpresented_on_owner(c)
                .unwrap_or_else(|failure| panic!("{:?}", failure.error)),
            b_id
        );
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(handoff.record().owner_id(), c_id);
        let frame = handoff.next_media().unwrap().unwrap();
        assert_eq!(frame.begin.unwrap().media.end_frame, 0);
        assert_eq!(frame.stamp.unwrap().media.end_frame, 1);
        assert_eq!(frame.stamp.unwrap().media.fraction, 0);
        handoff.release_pending(&frame, true).unwrap();
        assert!(handoff.next_media().unwrap().is_none());
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn retarget_foreign_scope_and_error_holder_preserve_original_pending_fifo_and_both_slots() {
        let pool = EffectsBankPool::new().unwrap();
        let foreign = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut handoff = unpresented_handoff(&pool, recipe.clone());
        let original = handoff.next_media().unwrap().unwrap();
        let id = handoff.record().owner_id();
        let foreign_b = counted_candidate(&foreign, recipe.clone(), 24_000, 1);
        let foreign_id = foreign_b.record().owner_id();
        let failure = handoff
            .retarget_unpresented_on_owner(foreign_b)
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::ForeignPool);
        assert_eq!(failure.candidate.record().owner_id(), foreign_id);
        assert_eq!(handoff.pending_output(), Some(original));
        assert_eq!(handoff.record().owner_id(), id);
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(foreign.occupied_banks(), 1);
        drop(failure);
        assert_eq!(foreign.occupied_banks(), 0);
        let mut b = counted_candidate(&pool, recipe, 24_000, 1);
        let b_id = b.record().owner_id();
        b.actual_mut().reset(1, 3).unwrap();
        let failure = handoff.retarget_unpresented_on_owner(b).err().unwrap();
        assert_eq!(
            failure.error,
            CountedHandoffError::Handoff(HandoffError::WrongStream)
        );
        assert_eq!(failure.candidate.record().owner_id(), b_id);
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(handoff.pending_output(), Some(original));
        assert_eq!(handoff.record().owner_id(), id);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn any_real_media_ack_including_mute_refuses_retarget_and_preserves_candidate() {
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut handoff = unpresented_handoff(&pool, recipe.clone());
        let frame = handoff.next_media().unwrap().unwrap();
        let muted_device_slot = [0.0_f32; MAX_CHANNELS];
        std::hint::black_box(muted_device_slot);
        handoff.release_pending(&frame, true).unwrap();
        assert!(!handoff.can_retarget_unpresented_on_owner());
        let b = counted_candidate(&pool, recipe, 24_000, 1);
        let id = b.record().owner_id();
        let failure = handoff.retarget_unpresented_on_owner(b).err().unwrap();
        assert_eq!(failure.error, CountedHandoffError::NotReady);
        assert_eq!(failure.candidate.record().owner_id(), id);
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn hard_owner_drop_reports_both_real_record_ids_and_releases_both_actual_boxes() {
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let old = counted_candidate(&pool, recipe.clone(), 8_000, 2);
        let old_id = old.record().owner_id();
        let b = counted_candidate(&pool, recipe, 16_000, 1);
        let b_id = b.record().owner_id();
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            b.graph().preparation().format,
            1,
            Some(b),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        assert_eq!(handoff.owned_record_ids(), [Some(old_id), Some(b_id)]);
        assert_eq!(pool.occupied_banks(), 2);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    fn captured_tail_only(pool: &EffectsBankPool) -> (CountedHandoff, Arc<EffectsRecipe>) {
        let mut builder = crate::audio::convolution::ImpulseBuilder::prepare(8_000, 1, 3).unwrap();
        for value in [1.0, 0.0, 0.5] {
            builder.push_frame(&[value]).unwrap();
        }
        let source = OwnedImpulse::from_validated(
            builder.finish().unwrap(),
            ImpulseResourceId::Imported("tail-ir".into()),
            "b".repeat(64),
        )
        .unwrap();
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(source.source_id().clone());
        let recipe = EffectsRecipe::prepare(settings, 1, 2, false, Some(source)).unwrap();
        let mut old = counted_candidate(pool, recipe.clone(), 8_000, 2);
        pulse(&mut old);
        old.begin_eof().unwrap();
        loop {
            let frame = old.drain().unwrap().unwrap();
            if frame.kind == super::super::effects::EffectsFrameKind::Media {
                old.release_pending(&frame, true).unwrap();
                break;
            }
            assert_eq!(frame.kind, super::super::effects::EffectsFrameKind::Padding);
        }
        let candidate = counted_candidate(pool, recipe.clone(), 16_000, 1);
        let reservation = CountedHandoffReservation::reserve(
            old.record().clone(),
            candidate.graph().preparation().format,
            1,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut handoff = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        while !handoff.advance_capture(&AtomicBool::new(false)).unwrap() {}
        handoff.retire_old_on_owner().unwrap();
        (handoff, recipe)
    }
    #[test]
    fn any_tail_ack_refuses_retarget_even_without_media_cursor_progress() {
        let pool = EffectsBankPool::new().unwrap();
        let (mut handoff, recipe) = captured_tail_only(&pool);
        assert!(handoff.media_prefix_drained());
        assert!(handoff.can_retarget_unpresented_on_owner());
        let tail = handoff.next_tail().unwrap().unwrap();
        assert!(tail.stamp.is_none());
        handoff.release_pending(&tail, true).unwrap();
        assert!(!handoff.can_retarget_unpresented_on_owner());
        let candidate = counted_candidate(&pool, recipe, 24_000, 1);
        let id = candidate.record().owner_id();
        let failure = handoff
            .retarget_unpresented_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::NotReady);
        assert_eq!(failure.candidate.record().owner_id(), id);
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn accepting_one_future_input_refuses_retarget_even_before_any_future_output() {
        let pool = EffectsBankPool::new().unwrap();
        let (mut handoff, recipe) = captured_tail_only(&pool);
        assert!(handoff.media_prefix_drained());
        assert!(handoff.can_retarget_unpresented_on_owner());
        let stamp = super::super::convolution::ConvolutionStamp {
            token: 1,
            source_epoch: 2,
            media: super::super::pcm::ProcessingStamp {
                end_frame: 2,
                ..Default::default()
            },
        };
        let frame = handoff
            .graph_mut()
            .unwrap()
            .frame(Some(EffectsInput {
                samples: [0.0; MAX_CHANNELS],
                stamp,
                begin: Some(super::super::convolution::ConvolutionStamp {
                    media: super::super::pcm::ProcessingStamp {
                        end_frame: 1,
                        ..stamp.media
                    },
                    ..stamp
                }),
                pitch: super::super::effects::EffectsPitch {
                    ratio: 1.0,
                    sequence: 1,
                    transition_remaining_frames: 0,
                },
            }))
            .unwrap();
        assert_eq!(frame.kind, super::super::effects::EffectsFrameKind::Padding);
        assert!(!handoff.can_retarget_unpresented_on_owner());
        let candidate = counted_candidate(&pool, recipe, 24_000, 1);
        let id = candidate.record().owner_id();
        let failure = handoff
            .retarget_unpresented_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::NotReady);
        assert_eq!(failure.candidate.record().owner_id(), id);
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    fn partial_prefix_handoff(
        pool: &EffectsBankPool,
        recipe: Arc<EffectsRecipe>,
    ) -> CountedHandoff {
        let mut handoff = unpresented_handoff(pool, recipe);
        let first = handoff.next_media().unwrap().unwrap();
        assert_eq!(first.begin.unwrap().media.end_frame, 0);
        assert_eq!(first.begin.unwrap().media.fraction, 0);
        assert_eq!(first.stamp.unwrap().media.end_frame, 0);
        assert_eq!(first.stamp.unwrap().media.fraction, 1 << 31);
        handoff.release_pending(&first, false).unwrap();
        assert_eq!(handoff.pending_output(), Some(first));
        let muted_device_slot = [0.0_f32; MAX_CHANNELS];
        std::hint::black_box(muted_device_slot);
        handoff.release_pending(&first, true).unwrap();
        assert!(!handoff.can_retarget_unpresented_on_owner());
        assert!(handoff.can_retarget_prefix_remainder_on_owner());
        assert!(handoff.graph_mut().is_none());
        handoff
    }
    #[test]
    fn partial_prefix_retarget_uses_real_spsc_and_retains_actual_ack_progress_across_two_rates() {
        let baseline = DspReservation::used_bytes();
        let pool = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut handoff = partial_prefix_handoff(&pool, recipe.clone());
        let a_id = handoff.record().owner_id();
        let old_copy = handoff.next_media().unwrap().unwrap();
        handoff.release_pending(&old_copy, false).unwrap();
        let b = counted_candidate(&pool, recipe.clone(), 24_000, 2);
        let b_id = b.record().owner_id();
        let (mut send, mut receive) = rtrb::RingBuffer::new(1);
        let charged = DspReservation::used_bytes();
        assert!(send.push(b).is_ok());
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(DspReservation::used_bytes(), charged);
        let b = receive
            .pop()
            .unwrap_or_else(|_| panic!("actual SPSC owner"));
        assert_eq!(DspReservation::used_bytes(), charged);
        assert_eq!(
            handoff
                .retarget_prefix_remainder_on_owner(b)
                .unwrap_or_else(|failure| panic!("{:?}", failure.error)),
            a_id
        );
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(handoff.owned_record_ids(), [None, Some(b_id)]);
        assert_eq!(handoff.expected_new_bus().sample_rate, 24_000);
        assert_eq!(handoff.device_channels(), 2);
        assert!(handoff.pending_output().is_none());
        assert_eq!(
            handoff.release_pending(&old_copy, true),
            Err(HandoffError::InvalidReceipt)
        );
        let first_b = handoff.next_media().unwrap().unwrap();
        assert_eq!(first_b.begin.unwrap().media.end_frame, 0);
        assert_eq!(first_b.begin.unwrap().media.fraction, 1 << 31);
        assert_eq!(first_b.stamp.unwrap().media.end_frame, 0);
        assert_eq!(
            first_b.stamp.unwrap().media.fraction,
            ((5_u128 << 32) / 6) as u32
        );
        handoff.release_pending(&first_b, true).unwrap();
        assert!(handoff.can_retarget_prefix_remainder_on_owner());
        let pending_b = handoff.next_media().unwrap().unwrap();
        let c = counted_candidate(&pool, recipe.clone(), 8_000, 1);
        let c_id = c.record().owner_id();
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(
            handoff
                .retarget_prefix_remainder_on_owner(c)
                .unwrap_or_else(|failure| panic!("{:?}", failure.error)),
            b_id
        );
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(handoff.owned_record_ids(), [None, Some(c_id)]);
        assert_eq!(
            handoff.release_pending(&pending_b, true),
            Err(HandoffError::InvalidReceipt)
        );
        let final_c = handoff.next_media().unwrap().unwrap();
        assert_eq!(
            final_c.begin.unwrap().media.end_frame,
            first_b.stamp.unwrap().media.end_frame
        );
        assert_eq!(
            final_c.begin.unwrap().media.fraction,
            first_b.stamp.unwrap().media.fraction
        );
        assert_eq!(final_c.stamp.unwrap().media.end_frame, 1);
        assert_eq!(final_c.stamp.unwrap().media.fraction, 0);
        handoff.release_pending(&final_c, true).unwrap();
        assert!(handoff.next_media().unwrap().is_none());
        assert!(!handoff.can_retarget_prefix_remainder_on_owner());
        let bank = handoff
            .finish_on_owner()
            .unwrap_or_else(|_| panic!("actual remaining prefix drained"));
        assert_eq!(bank.record().owner_id(), c_id);
        assert_eq!(bank.graph().statistics().accepted_media_frames, 0);
        assert_eq!(pool.occupied_banks(), 1);
        drop(bank);
        assert_eq!(pool.occupied_banks(), 0);
        drop(recipe);
        drop(pool);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
    #[test]
    fn partial_prefix_foreign_scope_and_closed_errors_preserve_complete_candidates_and_pending() {
        let pool = EffectsBankPool::new().unwrap();
        let foreign = EffectsBankPool::new().unwrap();
        let recipe = recipe();
        let mut handoff = partial_prefix_handoff(&pool, recipe.clone());
        let pending = handoff.next_media().unwrap().unwrap();
        let a_id = handoff.record().owner_id();
        let candidate = counted_candidate(&foreign, recipe.clone(), 24_000, 1);
        let candidate_id = candidate.record().owner_id();
        let pointer = candidate.graph() as *const PreparedEffects;
        let failure = handoff
            .retarget_prefix_remainder_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::ForeignPool);
        assert_eq!(failure.candidate.record().owner_id(), candidate_id);
        assert_eq!(failure.candidate.graph() as *const PreparedEffects, pointer);
        assert_eq!(handoff.pending_output(), Some(pending));
        assert_eq!(handoff.record().owner_id(), a_id);
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(foreign.occupied_banks(), 1);
        drop(failure);
        assert_eq!(foreign.occupied_banks(), 0);
        let mut candidate = counted_candidate(&pool, recipe.clone(), 24_000, 1);
        let candidate_id = candidate.record().owner_id();
        candidate.actual_mut().reset(1, 3).unwrap();
        let failure = handoff
            .retarget_prefix_remainder_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(
            failure.error,
            CountedHandoffError::Handoff(HandoffError::WrongStream)
        );
        assert_eq!(failure.candidate.record().owner_id(), candidate_id);
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(handoff.pending_output(), Some(pending));
        assert_eq!(handoff.record().owner_id(), a_id);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        let mut candidate = counted_candidate(&pool, recipe.clone(), 24_000, 1);
        let candidate_id = candidate.record().owner_id();
        pulse(&mut candidate);
        assert_eq!(candidate.graph().statistics().accepted_media_frames, 1);
        let failure = handoff
            .retarget_prefix_remainder_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(
            failure.error,
            CountedHandoffError::Handoff(HandoffError::WrongStream)
        );
        assert_eq!(failure.candidate.record().owner_id(), candidate_id);
        assert_eq!(
            failure.candidate.graph().statistics().accepted_media_frames,
            1
        );
        assert_eq!(handoff.pending_output(), Some(pending));
        assert_eq!(handoff.record().owner_id(), a_id);
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        let candidate = counted_candidate(&pool, recipe, 24_000, 1);
        let candidate_id = candidate.record().owner_id();
        pool.begin_shutdown();
        let failure = handoff
            .retarget_prefix_remainder_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::Closed);
        assert_eq!(failure.candidate.record().owner_id(), candidate_id);
        assert_eq!(handoff.pending_output(), Some(pending));
        assert_eq!(handoff.record().owner_id(), a_id);
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn prefix_remainder_retarget_refuses_partial_tail_ack_and_keeps_the_real_pending_tail() {
        let pool = EffectsBankPool::new().unwrap();
        let (mut handoff, recipe) = captured_tail_only(&pool);
        let first = handoff.next_tail().unwrap().unwrap();
        handoff.release_pending(&first, true).unwrap();
        let pending = handoff.next_tail().unwrap().unwrap();
        handoff.release_pending(&pending, false).unwrap();
        assert!(pending.stamp.is_none());
        assert!(!handoff.can_retarget_prefix_remainder_on_owner());
        let a_id = handoff.record().owner_id();
        let candidate = counted_candidate(&pool, recipe, 24_000, 1);
        let candidate_id = candidate.record().owner_id();
        let failure = handoff
            .retarget_prefix_remainder_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::NotReady);
        assert_eq!(failure.candidate.record().owner_id(), candidate_id);
        assert_eq!(handoff.pending_output(), Some(pending));
        assert_eq!(handoff.record().owner_id(), a_id);
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn prefix_remainder_retarget_refuses_actual_future_media_without_destroying_either_bank() {
        let pool = EffectsBankPool::new().unwrap();
        let (mut handoff, recipe) = captured_tail_only(&pool);
        let a_id = handoff.record().owner_id();
        let stamp = super::super::convolution::ConvolutionStamp {
            token: 1,
            source_epoch: 2,
            media: super::super::pcm::ProcessingStamp {
                end_frame: 2,
                ..Default::default()
            },
        };
        let frame = handoff
            .graph_mut()
            .unwrap()
            .frame(Some(EffectsInput {
                samples: [0.0; MAX_CHANNELS],
                stamp,
                begin: Some(super::super::convolution::ConvolutionStamp {
                    media: super::super::pcm::ProcessingStamp {
                        end_frame: 1,
                        ..stamp.media
                    },
                    ..stamp
                }),
                pitch: super::super::effects::EffectsPitch {
                    ratio: 1.0,
                    sequence: 1,
                    transition_remaining_frames: 0,
                },
            }))
            .unwrap();
        assert_eq!(frame.kind, super::super::effects::EffectsFrameKind::Padding);
        assert_eq!(
            handoff
                .graph_mut()
                .unwrap()
                .statistics()
                .accepted_media_frames,
            1
        );
        assert!(!handoff.can_retarget_prefix_remainder_on_owner());
        let candidate = counted_candidate(&pool, recipe, 24_000, 1);
        let candidate_id = candidate.record().owner_id();
        let failure = handoff
            .retarget_prefix_remainder_on_owner(candidate)
            .err()
            .unwrap();
        assert_eq!(failure.error, CountedHandoffError::NotReady);
        assert_eq!(failure.candidate.record().owner_id(), candidate_id);
        assert_eq!(handoff.record().owner_id(), a_id);
        assert_eq!(
            handoff
                .graph_mut()
                .unwrap()
                .statistics()
                .accepted_media_frames,
            1
        );
        assert_eq!(pool.occupied_banks(), 2);
        drop(failure);
        assert_eq!(pool.occupied_banks(), 1);
        drop(handoff);
        assert_eq!(pool.occupied_banks(), 0);
    }
    fn future_input(end: u64, begin: bool) -> EffectsInput {
        let stamp = super::super::convolution::ConvolutionStamp {
            token: 1,
            source_epoch: 2,
            media: super::super::pcm::ProcessingStamp {
                end_frame: end,
                ..Default::default()
            },
        };
        EffectsInput {
            samples: [0.0; MAX_CHANNELS],
            stamp,
            begin: begin.then_some(super::super::convolution::ConvolutionStamp {
                media: super::super::pcm::ProcessingStamp {
                    end_frame: end - 1,
                    ..stamp.media
                },
                ..stamp
            }),
            pitch: super::super::effects::EffectsPitch {
                ratio: 1.0,
                sequence: 1,
                transition_remaining_frames: 0,
            },
        }
    }
    fn long_previous_tail(pool: &EffectsBankPool) -> (CountedHandoff, Arc<EffectsRecipe>) {
        let mut builder =
            crate::audio::convolution::ImpulseBuilder::prepare(8_000, 1, 2_051).unwrap();
        for index in 0..2_051 {
            builder
                .push_frame(&[match index {
                    0 => 1.0,
                    2_050 => 0.5,
                    _ => 0.0,
                }])
                .unwrap();
        }
        let source = OwnedImpulse::from_validated(
            builder.finish().unwrap(),
            ImpulseResourceId::Imported("previous-long-ir".into()),
            "d".repeat(64),
        )
        .unwrap();
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(source.source_id().clone());
        settings.convolution.main_gain = 0.0;
        settings.convolution.send_gain = 1.0;
        let previous_recipe = EffectsRecipe::prepare(settings, 1, 2, false, Some(source)).unwrap();
        let mut old = counted_candidate(pool, previous_recipe, 8_000, 2);
        pulse(&mut old);
        old.begin_eof().unwrap();
        loop {
            let frame = old.drain().unwrap().unwrap();
            if frame.kind == super::super::effects::EffectsFrameKind::Media {
                old.release_pending(&frame, true).unwrap();
                break;
            }
            assert_eq!(frame.kind, super::super::effects::EffectsFrameKind::Padding);
        }
        // A is an actual admitted new recipe without IR; its remaining DSP
        // bound is smaller than the old IR tail, so recovery must grow a FIFO.
        let current_recipe = recipe();
        let a = counted_candidate(pool, current_recipe.clone(), 16_000, 1);
        let reserved = CountedHandoffReservation::reserve(
            old.record().clone(),
            a.graph().preparation().format,
            1,
            Some(a),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut old = reserved
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        while !old.advance_capture(&AtomicBool::new(false)).unwrap() {}
        old.retire_old_on_owner().unwrap();
        let first = old.next_tail().unwrap().unwrap();
        old.release_pending(&first, true).unwrap();
        let next = old.next_tail().unwrap().unwrap();
        old.release_pending(&next, false).unwrap();
        assert!(old.can_capture_current_on_owner());
        (old, current_recipe)
    }
    fn current_recovery_reservation(
        old: &CountedHandoff,
        pool: &EffectsBankPool,
        recipe: Arc<EffectsRecipe>,
    ) -> CountedHandoffReservation {
        let b = counted_candidate(pool, recipe, 24_000, 2);
        let (mut send, mut receive) = rtrb::RingBuffer::new(1);
        assert!(send.push(b).is_ok());
        assert_eq!(pool.occupied_banks(), 2);
        let b = receive.pop().unwrap_or_else(|_| panic!("actual SPSC bank"));
        CountedHandoffReservation::reserve(
            old.record().clone(),
            b.graph().preparation().format,
            2,
            Some(b),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error))
    }
    fn advance_until_merging(captured: &mut CountedHandoff) {
        for _ in 0..128 {
            let core_ready = match captured.body.as_ref().unwrap() {
                CountedHandoffBody::Retained(core) => core.ready(),
                _ => panic!("changed-format actual capture"),
            };
            if core_ready && captured.previous_storage.is_some() && captured.merge_cursor > 0 {
                assert!(!captured.ready());
                assert!(!captured.can_capture_current_on_owner());
                assert_eq!(captured.next_media(), Err(HandoffError::NotReady));
                assert_eq!(captured.next_tail(), Err(HandoffError::NotReady));
                return;
            }
            assert!(!captured.advance_capture(&AtomicBool::new(false)).unwrap());
        }
        panic!("bounded long-tail merge did not retain its actual progress");
    }
    #[test]
    fn current_recovery_merges_long_previous_tail_with_future_media_and_keeps_two_actual_slots() {
        let baseline = DspReservation::used_bytes();
        let pool = EffectsBankPool::new().unwrap();
        let (mut old, recipe) = long_previous_tail(&pool);
        let previous_copy = old.pending_output().unwrap();
        let a_id = old.record().owner_id();
        for end in 2..=3 {
            old.graph_mut()
                .unwrap()
                .frame(Some(future_input(end, true)))
                .unwrap();
        }
        assert_eq!(
            old.graph_mut().unwrap().statistics().accepted_media_frames,
            2
        );
        let reserved = current_recovery_reservation(&old, &pool, recipe.clone());
        let b_id = reserved.candidate_record.as_ref().unwrap().owner_id();
        let mut captured = old
            .capture_current_for_recovery_on_owner(reserved, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        assert_eq!(captured.owned_record_ids(), [Some(a_id), Some(b_id)]);
        assert_eq!(pool.occupied_banks(), 2);
        assert_eq!(
            captured
                .previous_storage
                .as_ref()
                .unwrap()
                .retained
                .as_ref()
                .unwrap()
                .pending_output(),
            Some(previous_copy)
        );
        assert!(!captured.ready());
        assert!(captured.graph_mut().is_none());
        assert!(captured.retire_old_on_owner().is_none());
        assert!(captured.try_take_finished_on_callback().is_none());
        advance_until_merging(&mut captured);
        let retained_bytes = DspReservation::used_bytes();
        while !captured.advance_capture(&AtomicBool::new(false)).unwrap() {}
        assert!(captured.previous_storage.is_none());
        assert!(DspReservation::used_bytes() < retained_bytes);
        assert_eq!(pool.occupied_banks(), 2);
        assert!(matches!(
            pool.begin(recipe.clone(), request()),
            Err(FactoryError::Busy)
        ));
        assert_eq!(captured.retire_old_on_owner(), Some(a_id));
        assert_eq!(pool.occupied_banks(), 1);
        assert_eq!(captured.owned_record_ids(), [None, Some(b_id)]);
        assert_eq!(
            captured.release_pending(&previous_copy, true),
            Err(HandoffError::InvalidReceipt)
        );
        let mut end = (1, 0);
        let mut media = 0;
        while let Some(frame) = captured.next_media().unwrap() {
            let begin = frame.begin.unwrap().media;
            assert_eq!((begin.end_frame, begin.fraction), end);
            let stamp = frame.stamp.unwrap().media;
            end = (stamp.end_frame, stamp.fraction);
            captured.release_pending(&frame, true).unwrap();
            media += 1;
        }
        assert_eq!(media, 3);
        assert_eq!(end, (3, 0));
        assert_eq!(
            captured
                .graph_mut()
                .unwrap()
                .statistics()
                .accepted_media_frames,
            0
        );
        let mut audible_previous_tail = false;
        let mut tail = 0;
        while let Some(frame) = captured.next_tail().unwrap() {
            assert!(frame.stamp.is_none() && frame.begin.is_none());
            audible_previous_tail |= frame.samples.iter().any(|sample| sample.abs() > 1.0e-6);
            captured.release_pending(&frame, true).unwrap();
            tail += 1;
        }
        assert!(tail > 1_024);
        assert!(audible_previous_tail);
        let bank = captured
            .finish_on_owner()
            .unwrap_or_else(|_| panic!("actual merged output ACK"));
        assert_eq!(bank.record().owner_id(), b_id);
        assert_eq!(pool.occupied_banks(), 1);
        drop(bank);
        assert_eq!(pool.occupied_banks(), 0);
        drop(recipe);
        drop(pool);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
    #[test]
    fn current_recovery_quota_and_mid_merge_cancel_keep_old_tail_and_both_real_banks_for_retry() {
        let pool = EffectsBankPool::new().unwrap();
        let (mut old, recipe) = long_previous_tail(&pool);
        old.graph_mut()
            .unwrap()
            .frame(Some(future_input(2, true)))
            .unwrap();
        let a_id = old.record().owner_id();
        let previous_copy = old.pending_output().unwrap();
        let reserved = current_recovery_reservation(&old, &pool, recipe);
        let b_id = reserved.candidate_record.as_ref().unwrap().owner_id();
        let filler = DspReservation::new(DSP_QUOTA_BYTES - DspReservation::used_bytes()).unwrap();
        let failure = old
            .capture_current_for_recovery_on_owner(reserved, &AtomicBool::new(false))
            .err()
            .unwrap();
        assert_eq!(
            failure.error,
            CountedHandoffError::Handoff(HandoffError::Quota)
        );
        assert!(failure.detached.is_none());
        assert_eq!(failure.owned_record_ids(), [Some(a_id), Some(b_id)]);
        assert_eq!(failure.old.pending_output(), Some(previous_copy));
        assert_eq!(pool.occupied_banks(), 2);
        drop(filler);
        let mut captured = failure
            .retry_capture(&AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        advance_until_merging(&mut captured);
        let cursor = captured.merge_cursor;
        let charged = DspReservation::used_bytes();
        assert_eq!(
            captured.advance_capture(&AtomicBool::new(true)),
            Err(CountedHandoffError::Handoff(HandoffError::Cancelled))
        );
        assert_eq!(captured.merge_cursor, cursor);
        assert_eq!(DspReservation::used_bytes(), charged);
        assert_eq!(captured.owned_record_ids(), [Some(a_id), Some(b_id)]);
        assert_eq!(pool.occupied_banks(), 2);
        assert!(!captured.ready());
        assert!(captured.retire_old_on_owner().is_none());
        assert_eq!(
            captured
                .previous_storage
                .as_ref()
                .unwrap()
                .retained
                .as_ref()
                .unwrap()
                .pending_output(),
            Some(previous_copy)
        );
        while !captured.advance_capture(&AtomicBool::new(false)).unwrap() {}
        assert_eq!(captured.retire_old_on_owner(), Some(a_id));
        assert_eq!(pool.occupied_banks(), 1);
        drop(captured);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn initial_current_capture_failure_restores_exact_actual_a_before_candidate_retirement() {
        let pool = EffectsBankPool::new().unwrap();
        let (mut old, recipe) = long_previous_tail(&pool);
        let a_id = old.record().owner_id();
        let pointer = old.graph_mut().unwrap() as *mut PreparedEffects;
        old.graph_mut()
            .unwrap()
            .frame(Some(future_input(2, false)))
            .unwrap();
        let previous_copy = old.pending_output().unwrap();
        let reserved = current_recovery_reservation(&old, &pool, recipe);
        let b_id = reserved.candidate_record.as_ref().unwrap().owner_id();
        let mut failure = old
            .capture_current_for_recovery_on_owner(reserved, &AtomicBool::new(false))
            .err()
            .unwrap();
        assert_eq!(
            failure.error,
            CountedHandoffError::Handoff(HandoffError::MissingBegin)
        );
        assert!(failure.detached.is_none());
        assert_eq!(failure.owned_record_ids(), [Some(a_id), Some(b_id)]);
        assert_eq!(
            failure.old.graph_mut().unwrap() as *mut PreparedEffects,
            pointer
        );
        assert_eq!(failure.old.pending_output(), Some(previous_copy));
        assert_eq!(
            failure
                .old
                .graph_mut()
                .unwrap()
                .statistics()
                .accepted_media_frames,
            1
        );
        assert_eq!(pool.occupied_banks(), 2);
        let (old, retired) = failure
            .recover_old_on_owner()
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        assert_eq!(retired, Some(b_id));
        assert_eq!(old.record().owner_id(), a_id);
        assert_eq!(old.pending_output(), Some(previous_copy));
        assert_eq!(pool.occupied_banks(), 1);
        drop(old);
        assert_eq!(pool.occupied_banks(), 0);
    }
    #[test]
    fn hard_retirement_during_current_tail_merge_reports_both_ids_and_drops_all_storage() {
        let baseline = DspReservation::used_bytes();
        let pool = EffectsBankPool::new().unwrap();
        let (mut old, recipe) = long_previous_tail(&pool);
        old.graph_mut()
            .unwrap()
            .frame(Some(future_input(2, true)))
            .unwrap();
        let a_id = old.record().owner_id();
        let reserved = current_recovery_reservation(&old, &pool, recipe.clone());
        let b_id = reserved.candidate_record.as_ref().unwrap().owner_id();
        let mut captured = old
            .capture_current_for_recovery_on_owner(reserved, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        advance_until_merging(&mut captured);
        assert_eq!(captured.owned_record_ids(), [Some(a_id), Some(b_id)]);
        assert!(captured.previous_storage.is_some());
        assert_eq!(pool.occupied_banks(), 2);
        drop(captured);
        assert_eq!(pool.occupied_banks(), 0);
        drop(recipe);
        drop(pool);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
}
