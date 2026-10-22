//! Retained rendered output for an explicitly fenced effect-bank handoff.
//!
//! All storage and the candidate bank are reserved before closing the old
//! callback. Capture owns the returned old bank exclusively. Media already
//! processed by that bank is rendered once; its raw intervals supply labels,
//! never a second input to the candidate. An old tail has no media clock and
//! overlays future output only after the retained media prefix is presented.
//! Tail PCM retains the capture-time spatial pose and old device mapping.
//! Resampling uses the native linear PCM handoff policy.

use super::{
    convolution::ConvolutionStamp,
    effects::{
        EffectsError, EffectsFrame, EffectsFrameKind, EffectsInput, EffectsPitch,
        EffectsPreparation, PreparedEffects,
    },
    pcm::{MAX_CHANNELS, PcmFormat},
    processing::DspReservation,
};
use std::{
    mem::{align_of, size_of},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

const CAPTURE_STEPS: usize = 1024;
static NEXT_HANDOFF: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffError {
    InvalidFormat,
    WrongOwner,
    WrongStream,
    Capacity,
    Quota,
    Allocation,
    Cancelled,
    NotReady,
    MissingBegin,
    InvalidLabel,
    InvalidReceipt,
    MediaPending,
    CounterExhausted,
    Source(EffectsError),
}
impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid effect handoff: {self:?}")
    }
}
impl std::error::Error for HandoffError {}

/// Immutable bounds taken while the preparation owner holds the actual graph.
/// This record is neither a callback-close certificate nor filesystem authority.
pub struct HandoffBounds {
    owner: u64,
    preparation: EffectsPreparation,
    impulse_frames: usize,
    latency: u32,
    _arc_allocation: Option<DspReservation>,
}
impl HandoffBounds {
    pub fn from_graph(graph: &PreparedEffects) -> Self {
        Self {
            owner: graph.handoff_owner(),
            preparation: graph.preparation(),
            impulse_frames: graph.handoff_impulse_frames(),
            latency: graph.latency_frames(),
            _arc_allocation: None,
        }
    }
    pub fn try_arc_from_graph(graph: &PreparedEffects) -> Result<Arc<Self>, HandoffError> {
        let bytes = size_of::<Self>() + 2 * size_of::<usize>() + align_of::<Self>();
        let allocation = DspReservation::new(bytes).map_err(|_| HandoffError::Quota)?;
        let mut bounds = Self::from_graph(graph);
        bounds._arc_allocation = Some(allocation);
        Ok(Arc::new(bounds))
    }
    pub fn arc_reserved_bytes(&self) -> usize {
        if self._arc_allocation.is_some() {
            size_of::<Self>() + 2 * size_of::<usize>() + align_of::<Self>()
        } else {
            0
        }
    }
    pub(crate) fn has_arc_reservation(&self) -> bool {
        self._arc_allocation.is_some()
    }
    pub fn old_format(&self) -> PcmFormat {
        self.preparation.format
    }
    pub(crate) fn preparation(&self) -> EffectsPreparation {
        self.preparation
    }
    pub fn old_device_channels(&self) -> u16 {
        self.preparation.device_channels
    }
}

pub struct ReservationFailure {
    pub error: HandoffError,
    pub new_graph: Option<Box<PreparedEffects>>,
}
pub struct CaptureFailure {
    pub error: HandoffError,
    pub reservation: HandoffReservation,
    pub old_graph: Box<PreparedEffects>,
}
#[derive(Clone, Copy)]
struct MediaLabel {
    input: EffectsInput,
    rendered: Option<EffectsFrame>,
}

/// Minted only after samples and the full corresponding label occupy the FIFO.
pub(crate) struct StoredReceipt {
    owner: u64,
    ordinal: u64,
    serial: Option<u64>,
    preparation: EffectsPreparation,
    frame: EffectsFrame,
    input: Option<EffectsInput>,
    consumed: bool,
}
impl StoredReceipt {
    pub(crate) fn matches(
        &self,
        owner: u64,
        ordinal: u64,
        serial: Option<u64>,
        preparation: EffectsPreparation,
        frame: &EffectsFrame,
        input: Option<EffectsInput>,
    ) -> bool {
        !self.consumed
            && self.owner == owner
            && self.ordinal == ordinal
            && self.serial == serial
            && self.preparation.format == preparation.format
            && self.preparation.device_channels == preparation.device_channels
            && self.preparation.token == preparation.token
            && self.preparation.source_epoch == preparation.source_epoch
            && self.frame == *frame
            && same_input(self.input, input)
    }
    pub(crate) fn consume(&mut self) {
        self.consumed = true;
    }
}

enum BoundsOwner {
    Inline(HandoffBounds),
    Shared(Arc<HandoffBounds>),
}
impl std::ops::Deref for BoundsOwner {
    type Target = HandoffBounds;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Inline(bounds) => bounds,
            Self::Shared(bounds) => bounds,
        }
    }
}

pub struct HandoffReservation {
    bounds: BoundsOwner,
    new_format: PcmFormat,
    device_channels: u16,
    new_graph: Option<Box<PreparedEffects>>,
    samples: Box<[f64]>,
    labels: Box<[Option<MediaLabel>]>,
    same_format: bool,
    frame_capacity: usize,
    owned_bytes: usize,
    _sample_extension: Option<DspReservation>,
    _reservation: DspReservation,
}
impl HandoffReservation {
    /// Runs on the preparation owner before the old callback is fenced/closed.
    pub fn reserve(
        bounds: HandoffBounds,
        new_format: PcmFormat,
        device_channels: u16,
        new_graph: Option<Box<PreparedEffects>>,
        cancel: &AtomicBool,
    ) -> Result<Self, ReservationFailure> {
        Self::reserve_owned(
            BoundsOwner::Inline(bounds),
            new_format,
            device_channels,
            new_graph,
            cancel,
        )
    }
    /// Keeps the actual factory/registry bounds alive across preparation failures.
    /// Only an Arc allocated under its own retained DSP permit is admitted.
    pub fn reserve_shared(
        bounds: Arc<HandoffBounds>,
        new_format: PcmFormat,
        device_channels: u16,
        new_graph: Option<Box<PreparedEffects>>,
        cancel: &AtomicBool,
    ) -> Result<Self, ReservationFailure> {
        if !bounds.has_arc_reservation() {
            return Err(ReservationFailure {
                error: HandoffError::Quota,
                new_graph,
            });
        }
        Self::reserve_owned(
            BoundsOwner::Shared(bounds),
            new_format,
            device_channels,
            new_graph,
            cancel,
        )
    }
    fn reserve_owned(
        bounds: BoundsOwner,
        new_format: PcmFormat,
        device_channels: u16,
        new_graph: Option<Box<PreparedEffects>>,
        cancel: &AtomicBool,
    ) -> Result<Self, ReservationFailure> {
        let same_format = new_format == bounds.preparation.format
            && device_channels == bounds.preparation.device_channels
            && new_graph.is_none();
        let new = new_graph.as_ref().map(|graph| graph.preparation());
        let validation = if cancel.load(Ordering::Acquire) {
            Err(HandoffError::Cancelled)
        } else if !(8000..=384000).contains(&new_format.sample_rate)
            || new_format.channels != bounds.preparation.format.channels
            || !(1..=MAX_CHANNELS as u16).contains(&device_channels)
            || (!same_format && new.is_none())
            || new.is_some_and(|new| {
                new.format != new_format
                    || new.device_channels != device_channels
                    || (new.format == bounds.preparation.format
                        && new.device_channels == bounds.preparation.device_channels
                        && new.sequence <= bounds.preparation.sequence)
            })
        {
            Err(HandoffError::InvalidFormat)
        } else if new.is_some_and(|new| {
            new.token != bounds.preparation.token
                || new.source_epoch != bounds.preparation.source_epoch
        }) {
            Err(HandoffError::WrongStream)
        } else {
            Ok(())
        };
        if let Err(error) = validation {
            return Err(ReservationFailure { error, new_graph });
        }
        // The actual admitted graph capacity includes its delayed media. Reserve
        // an additional final slot and all finite warm-up latency conservatively.
        let media_capacity = if same_format {
            Some(0)
        } else {
            bounds.preparation.replay_capacity.checked_add(1)
        };
        let frame_capacity = if same_format {
            Some(0)
        } else {
            media_capacity
                .and_then(|count| count.checked_add(bounds.impulse_frames.saturating_sub(1)))
                .and_then(|count| count.checked_add(bounds.latency as usize))
        };
        let Some(frame_capacity) = frame_capacity else {
            return Err(ReservationFailure {
                error: HandoffError::Capacity,
                new_graph,
            });
        };
        let sample_count = frame_capacity.checked_mul(bounds.preparation.device_channels as usize);
        let label_count = media_capacity.unwrap_or(0);
        let bytes = sample_count
            .and_then(|count| count.checked_mul(size_of::<f64>()))
            .and_then(|bytes| bytes.checked_add(label_count * size_of::<Option<MediaLabel>>()))
            .and_then(|bytes| bytes.checked_add(size_of::<RetainedHandoff>() + size_of::<Self>()));
        let Some(bytes) = bytes else {
            return Err(ReservationFailure {
                error: HandoffError::Capacity,
                new_graph,
            });
        };
        // Admit the complete worst-case output count without floating-point size arithmetic.
        if output_count(
            frame_capacity,
            bounds.preparation.format.sample_rate,
            new_format.sample_rate,
        )
        .is_none()
        {
            return Err(ReservationFailure {
                error: HandoffError::Capacity,
                new_graph,
            });
        }
        let reservation = match DspReservation::new(bytes) {
            Ok(value) => value,
            Err(_) => {
                return Err(ReservationFailure {
                    error: HandoffError::Quota,
                    new_graph,
                });
            }
        };
        let samples = match fixed(sample_count.unwrap(), 0.0) {
            Ok(value) => value,
            Err(error) => return Err(ReservationFailure { error, new_graph }),
        };
        let labels = match fixed(label_count, None) {
            Ok(value) => value,
            Err(error) => return Err(ReservationFailure { error, new_graph }),
        };
        if cancel.load(Ordering::Acquire) {
            return Err(ReservationFailure {
                error: HandoffError::Cancelled,
                new_graph,
            });
        }
        Ok(Self {
            bounds,
            new_format,
            device_channels,
            new_graph,
            samples,
            labels,
            same_format,
            frame_capacity,
            owned_bytes: bytes,
            _sample_extension: None,
            _reservation: reservation,
        })
    }
    /// Reserves a complete canonical tail before the current bank is detached.
    pub(crate) fn prepare_previous_tail_for_recovery_on_owner(
        &mut self,
        previous: &RetainedHandoff,
        cancel: &AtomicBool,
    ) -> Result<(), HandoffError> {
        if cancel.load(Ordering::Acquire) {
            return Err(HandoffError::Cancelled);
        }
        let plan = self.bounds.preparation;
        let actual = previous
            .reservation
            .new_graph
            .as_ref()
            .ok_or(HandoffError::WrongOwner)?;
        if self.same_format
            || !previous.can_detach_current_for_recovery_on_owner()
            || actual.handoff_owner() != self.bounds.owner
            || actual.preparation().format != plan.format
            || actual.preparation().device_channels != plan.device_channels
            || previous.reservation.new_format != plan.format
            || previous.reservation.device_channels != plan.device_channels
            || actual.preparation().token != plan.token
            || actual.preparation().source_epoch != plan.source_epoch
            || self.new_graph.as_ref().is_none_or(|bank| {
                bank.statistics().accepted_media_frames != 0
                    || bank.pending_output().is_some()
                    || !bank.can_accept_input()
            })
        {
            return Err(HandoffError::WrongStream);
        }
        let required = previous.remaining_tail_frames_on_owner()?;
        if required <= self.frame_capacity {
            return Ok(());
        }
        let width = plan.device_channels as usize;
        let length = required.checked_mul(width).ok_or(HandoffError::Capacity)?;
        let bytes = length
            .checked_mul(size_of::<f64>())
            .ok_or(HandoffError::Capacity)?;
        if output_count(
            required,
            plan.format.sample_rate,
            self.new_format.sample_rate,
        )
        .is_none()
        {
            return Err(HandoffError::Capacity);
        }
        let permit = DspReservation::new(bytes).map_err(|_| HandoffError::Quota)?;
        let samples = fixed(length, 0.0)?;
        if cancel.load(Ordering::Acquire) {
            return Err(HandoffError::Cancelled);
        }
        let old_bytes = self
            .samples
            .len()
            .checked_mul(size_of::<f64>())
            .ok_or(HandoffError::Capacity)?;
        let base_bytes = self
            .owned_bytes
            .checked_sub(old_bytes)
            .ok_or(HandoffError::Capacity)?;
        let owned_bytes = base_bytes
            .checked_add(bytes)
            .ok_or(HandoffError::Capacity)?;
        // Both real buffers are charged until the old buffer is actually destroyed.
        let old = std::mem::replace(&mut self.samples, samples);
        drop(old);
        self._reservation
            .shrink_to(base_bytes)
            .expect("Original labels and envelope remain reserved");
        drop(self._sample_extension.take());
        self._sample_extension = Some(permit);
        self.owned_bytes = owned_bytes;
        self.frame_capacity = required;
        Ok(())
    }
    /// Call only with the actual Box returned by the closed callback.
    /// Failure returns that owner and every reserved object without mutating it.
    #[expect(
        clippy::result_large_err,
        reason = "Rejected capture retains the actual old Box and every precharged FIFO owner without an additional error allocation."
    )]
    pub fn capture(
        mut self,
        old_graph: Box<PreparedEffects>,
        cancel: &AtomicBool,
    ) -> Result<RetainedHandoff, CaptureFailure> {
        let actual = old_graph.preparation();
        let error = if cancel.load(Ordering::Acquire) {
            Some(HandoffError::Cancelled)
        } else if old_graph.handoff_owner() != self.bounds.owner {
            Some(HandoffError::WrongOwner)
        } else if actual.format != self.bounds.preparation.format
            || actual.device_channels != self.bounds.preparation.device_channels
            || actual.token != self.bounds.preparation.token
            || actual.source_epoch != self.bounds.preparation.source_epoch
        {
            Some(HandoffError::WrongStream)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(CaptureFailure {
                error,
                reservation: self,
                old_graph,
            });
        }
        let mut media_count = 0;
        if !self.same_format {
            let count = old_graph.replay_inputs().count();
            if count > self.labels.len() {
                return Err(CaptureFailure {
                    error: HandoffError::Capacity,
                    reservation: self,
                    old_graph,
                });
            }
            if old_graph.replay_inputs().any(|input| input.begin.is_none()) {
                return Err(CaptureFailure {
                    error: HandoffError::MissingBegin,
                    reservation: self,
                    old_graph,
                });
            }
            let mut previous = None;
            let invalid = old_graph.replay_inputs().any(|input| {
                let continuous =
                    previous.is_none_or(|last| last == stamp_point(input.begin.unwrap()));
                previous = Some(stamp_point(input.stamp));
                !continuous
            });
            if invalid {
                return Err(CaptureFailure {
                    error: HandoffError::InvalidLabel,
                    reservation: self,
                    old_graph,
                });
            }
            for input in old_graph.replay_inputs() {
                self.labels[media_count] = Some(MediaLabel {
                    input,
                    rendered: None,
                });
                media_count += 1;
            }
        }
        let identity = match NEXT_HANDOFF.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        }) {
            Ok(value) => value,
            Err(_) => {
                return Err(CaptureFailure {
                    error: HandoffError::CounterExhausted,
                    reservation: self,
                    old_graph,
                });
            }
        };
        let ready = self.same_format;
        let source_rate = self.bounds.preparation.format.sample_rate;
        let target_rate = self.new_format.sample_rate;
        let media_output_limit =
            output_count(media_count, source_rate, target_rate).expect("Preflighted media count");
        Ok(RetainedHandoff {
            reservation: self,
            old_graph: Some(old_graph),
            identity,
            media_count,
            captured_media: 0,
            captured_frames: 0,
            tail_count: 0,
            tail_header: None,
            transfer: None,
            started: false,
            ready,
            media_cursor: 0,
            media_grid_origin: 0,
            media_grid_denominator: target_rate as u128,
            media_grid_step: source_rate as u128,
            media_output_limit,
            tail_cursor: 0,
            last_media_end: None,
            pending: None,
            ordinal: 1,
            recovery_detached_owner: 0,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandoffFrame {
    pub samples: [f64; MAX_CHANNELS],
    pub stamp: Option<ConvolutionStamp>,
    pub begin: Option<ConvolutionStamp>,
    pub token: u64,
    pub source_epoch: u64,
    pub kind: EffectsFrameKind,
    pub pitch: EffectsPitch,
    pub effect_sequence: u64,
    pub spatial_sequence: u64,
    /// Old graph latency, converted to the new output-frame units.
    pub retained_effect_latency_frames: u32,
    pub transition_remaining_frames: u32,
    owner: u64,
    ordinal: u64,
}

pub struct RetainedHandoff {
    reservation: HandoffReservation,
    old_graph: Option<Box<PreparedEffects>>,
    identity: u64,
    media_count: usize,
    captured_media: usize,
    captured_frames: usize,
    tail_count: usize,
    tail_header: Option<EffectsFrame>,
    transfer: Option<StoredReceipt>,
    started: bool,
    ready: bool,
    media_cursor: usize,
    media_grid_origin: u128,
    media_grid_denominator: u128,
    media_grid_step: u128,
    media_output_limit: usize,
    tail_cursor: usize,
    last_media_end: Option<ConvolutionStamp>,
    pending: Option<HandoffFrame>,
    ordinal: u64,
    recovery_detached_owner: u64,
}
impl RetainedHandoff {
    pub(crate) fn can_detach_current_for_recovery_on_owner(&self) -> bool {
        self.ready
            && !self.same_format()
            && self.old_graph.is_none()
            && self.media_prefix_drained()
            && self.reservation.new_graph.is_some()
            && self.recovery_detached_owner == 0
    }
    pub(crate) fn remaining_tail_frames_on_owner(&self) -> Result<usize, HandoffError> {
        if !self.ready || !self.media_prefix_drained() {
            return Err(HandoffError::MediaPending);
        }
        self.tail_output_frames()
            .checked_sub(self.tail_cursor)
            .ok_or(HandoffError::InvalidReceipt)
    }
    pub(crate) fn take_current_for_recovery_on_owner(&mut self) -> Option<Box<PreparedEffects>> {
        if !self.can_detach_current_for_recovery_on_owner() {
            return None;
        }
        let bank = self.reservation.new_graph.take()?;
        self.recovery_detached_owner = bank.handoff_owner();
        Some(bank)
    }
    pub(crate) fn restore_current_after_failed_capture_on_owner(
        &mut self,
        bank: Box<PreparedEffects>,
    ) -> Result<(), Box<PreparedEffects>> {
        let plan = bank.preparation();
        if self.reservation.new_graph.is_some()
            || self.recovery_detached_owner == 0
            || bank.handoff_owner() != self.recovery_detached_owner
            || plan.format != self.reservation.new_format
            || plan.device_channels != self.reservation.device_channels
            || plan.token != self.reservation.bounds.preparation.token
            || plan.source_epoch != self.reservation.bounds.preparation.source_epoch
        {
            return Err(bank);
        }
        self.reservation.new_graph = Some(bank);
        self.recovery_detached_owner = 0;
        Ok(())
    }
    /// Transfer cursor records storage work, never a presentation acknowledgement.
    pub(crate) fn merge_previous_tail_on_owner(
        &mut self,
        previous: &RetainedHandoff,
        cursor: &mut usize,
        cancel: &AtomicBool,
    ) -> Result<bool, HandoffError> {
        if cancel.load(Ordering::Acquire) {
            return Err(HandoffError::Cancelled);
        }
        let plan = self.reservation.bounds.preparation;
        if !self.ready
            || self.same_format()
            || !previous.ready
            || !previous.media_prefix_drained()
            || previous.reservation.new_graph.is_some()
            || previous.old_graph.is_some()
            || previous.recovery_detached_owner != self.reservation.bounds.owner
            || previous.reservation.new_format != plan.format
            || previous.reservation.device_channels != plan.device_channels
            || previous.reservation.bounds.preparation.token != plan.token
            || previous.reservation.bounds.preparation.source_epoch != plan.source_epoch
            || self.media_cursor != 0
            || self.tail_cursor != 0
            || self.pending.is_some()
            || self.reservation.new_graph.as_ref().is_none_or(|bank| {
                bank.statistics().accepted_media_frames != 0 || bank.pending_output().is_some()
            })
        {
            return Err(HandoffError::WrongStream);
        }
        let remaining = previous.remaining_tail_frames_on_owner()?;
        if *cursor > remaining {
            return Err(HandoffError::InvalidReceipt);
        }
        let total = self
            .media_count
            .checked_add(self.tail_count)
            .ok_or(HandoffError::Capacity)?
            .max(remaining);
        if total > self.reservation.frame_capacity {
            return Err(HandoffError::Capacity);
        }
        let width = plan.device_channels as usize;
        let end = cursor.saturating_add(CAPTURE_STEPS).min(remaining);
        while *cursor < end {
            if cancel.load(Ordering::Acquire) {
                return Err(HandoffError::Cancelled);
            }
            let samples = previous.resampled(
                previous.tail_cursor + *cursor,
                previous.media_count,
                previous.tail_count,
            );
            let offset = *cursor * width;
            let mut combined = [0.0; MAX_CHANNELS];
            for lane in 0..width {
                combined[lane] = self.reservation.samples[offset + lane] + samples[lane];
                if !combined[lane].is_finite() {
                    return Err(HandoffError::InvalidLabel);
                }
            }
            self.reservation.samples[offset..offset + width].copy_from_slice(&combined[..width]);
            *cursor += 1;
        }
        if *cursor != remaining {
            return Ok(false);
        }
        if total > self.media_count && self.tail_header.is_none() {
            let mut header = previous.tail_header.ok_or(HandoffError::InvalidLabel)?;
            let old_rate = previous.reservation.bounds.preparation.format.sample_rate as u64;
            let new_rate = plan.format.sample_rate as u64;
            header.pitch.transition_remaining_frames =
                (header.pitch.transition_remaining_frames as u64 * new_rate)
                    .div_ceil(old_rate)
                    .min(u32::MAX as u64) as u32;
            header.transition_remaining_frames = (header.transition_remaining_frames as u64
                * new_rate)
                .div_ceil(old_rate)
                .min(u32::MAX as u64) as u32;
            self.tail_header = Some(header);
        }
        self.tail_count = total - self.media_count;
        self.captured_frames = total;
        Ok(true)
    }
    pub(crate) fn can_retarget_prefix_remainder_on_owner(&self) -> bool {
        self.ready
            && !self.same_format()
            && self.old_graph.is_none()
            && !self.media_prefix_drained()
            && self.tail_cursor == 0
            && self.reservation.new_graph.as_ref().is_some_and(|graph| {
                graph.statistics().accepted_media_frames == 0 && graph.pending_output().is_none()
            })
    }
    pub(crate) fn can_retarget_unpresented_on_owner(&self) -> bool {
        self.ready
            && !self.same_format()
            && self.old_graph.is_none()
            && self.media_cursor == 0
            && self.tail_cursor == 0
            && self.last_media_end.is_none()
            && self.reservation.new_graph.as_ref().is_some_and(|graph| {
                graph.statistics().accepted_media_frames == 0 && graph.pending_output().is_none()
            })
    }
    /// All FIFO PCM retains its original capture rate. Only an unpresented
    /// destination bank and its output-clock mapping are replaced.
    pub(crate) fn retarget_unpresented_on_owner(
        &mut self,
        next: Box<PreparedEffects>,
    ) -> Result<Box<PreparedEffects>, ReservationFailure> {
        self.retarget_bank_on_owner(next, false)
    }
    pub(crate) fn retarget_prefix_remainder_on_owner(
        &mut self,
        next: Box<PreparedEffects>,
    ) -> Result<Box<PreparedEffects>, ReservationFailure> {
        self.retarget_bank_on_owner(next, true)
    }
    fn remaining_grid(&self, rate: u32) -> Option<(u128, u128, u128, usize)> {
        let end = (self.media_count as u128).checked_mul(self.media_grid_denominator)?;
        let point = self
            .media_grid_origin
            .checked_add((self.media_cursor as u128).checked_mul(self.media_grid_step)?)?
            .min(end);
        let reduction = gcd(point, self.media_grid_denominator);
        let numerator = point / reduction;
        let denominator = self.media_grid_denominator / reduction;
        let common = gcd(denominator, rate as u128);
        let denominator_factor = denominator / common;
        let origin = numerator.checked_mul(rate as u128 / common)?;
        let denominator = denominator_factor.checked_mul(rate as u128)?;
        let step = denominator_factor
            .checked_mul(self.reservation.bounds.preparation.format.sample_rate as u128)?;
        let end = (self.media_count as u128).checked_mul(denominator)?;
        let limit = usize::try_from(end.checked_sub(origin)?.div_ceil(step)).ok()?;
        origin.checked_add((limit as u128).checked_mul(step)?)?;
        for label in self.reservation.labels[..self.media_count].iter().flatten() {
            let begin = label.input.begin?;
            let start = (begin.media.end_frame as u128) << 32 | begin.media.fraction as u128;
            let end = (label.input.stamp.media.end_frame as u128) << 32
                | label.input.stamp.media.fraction as u128;
            end.checked_sub(start)?.checked_mul(denominator)?;
        }
        Some((origin, denominator, step, limit))
    }
    fn retarget_bank_on_owner(
        &mut self,
        next: Box<PreparedEffects>,
        prefix_remainder: bool,
    ) -> Result<Box<PreparedEffects>, ReservationFailure> {
        let plan = next.preparation();
        let old = self.reservation.bounds.preparation;
        let ready = if prefix_remainder {
            self.can_retarget_prefix_remainder_on_owner()
        } else {
            self.can_retarget_unpresented_on_owner()
        };
        let grid = self.remaining_grid(plan.format.sample_rate);
        let error = if !ready {
            Some(HandoffError::MediaPending)
        } else if !(8000..=384000).contains(&plan.format.sample_rate)
            || plan.format.channels != old.format.channels
            || !(1..=8).contains(&plan.device_channels)
            || next.statistics().accepted_media_frames != 0
            || next.pending_output().is_some()
        {
            Some(HandoffError::InvalidFormat)
        } else if plan.token != old.token || plan.source_epoch != old.source_epoch {
            Some(HandoffError::WrongStream)
        } else if grid.is_none()
            || output_count(
                self.reservation.frame_capacity,
                old.format.sample_rate,
                plan.format.sample_rate,
            )
            .is_none()
            || output_count(
                self.media_count,
                old.format.sample_rate,
                plan.format.sample_rate,
            )
            .is_none()
            || output_count(
                self.tail_count,
                old.format.sample_rate,
                plan.format.sample_rate,
            )
            .is_none()
        {
            Some(HandoffError::Capacity)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(ReservationFailure {
                error,
                new_graph: Some(next),
            });
        }
        self.reservation.new_format = plan.format;
        self.reservation.device_channels = plan.device_channels;
        let (origin, denominator, step, limit) = grid.expect("Checked remaining SRC grid");
        self.media_grid_origin = origin;
        self.media_grid_denominator = denominator;
        self.media_grid_step = step;
        self.media_output_limit = limit;
        self.media_cursor = 0;
        self.pending = None;
        Ok(self
            .reservation
            .new_graph
            .replace(next)
            .expect("Preflight retains the actual unpresented bank"))
    }
    pub fn ready(&self) -> bool {
        self.ready
    }
    pub fn same_format(&self) -> bool {
        self.reservation.same_format
    }
    pub fn owned_buffer_bytes(&self) -> usize {
        self.reservation.owned_bytes
    }
    pub fn old_graph(&self) -> Option<&PreparedEffects> {
        self.old_graph.as_deref()
    }
    pub fn retained_media_intervals(&self) -> usize {
        self.media_count
    }
    pub fn retained_tail_source_frames(&self) -> usize {
        self.tail_count
    }
    pub fn pending_output(&self) -> Option<HandoffFrame> {
        self.pending
    }
    /// Bounded preparation-owner work. Cancellation leaves the real owners and FIFO held.
    pub fn advance_capture(&mut self, cancel: &AtomicBool) -> Result<bool, HandoffError> {
        if self.ready {
            return Ok(true);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(HandoffError::Cancelled);
        }
        if !self.started {
            self.old_graph
                .as_mut()
                .ok_or(HandoffError::WrongOwner)?
                .begin_eof()
                .map_err(HandoffError::Source)?;
            self.started = true;
        }
        for _ in 0..CAPTURE_STEPS {
            if cancel.load(Ordering::Acquire) {
                return Err(HandoffError::Cancelled);
            }
            if let Some(receipt) = &mut self.transfer {
                self.old_graph
                    .as_mut()
                    .ok_or(HandoffError::WrongOwner)?
                    .transfer_pending_for_handoff(receipt)
                    .map_err(HandoffError::Source)?;
                self.transfer = None;
            }
            let old = self.old_graph.as_mut().ok_or(HandoffError::WrongOwner)?;
            let frame = match old.pending_output() {
                Some(frame) => Some(frame),
                None => old.drain().map_err(HandoffError::Source)?,
            };
            let Some(frame) = frame else {
                if !old.drained() || self.captured_media != self.media_count {
                    return Err(HandoffError::InvalidLabel);
                }
                self.ready = true;
                return Ok(true);
            };
            if frame.kind == EffectsFrameKind::Padding {
                continue;
            }
            self.append(frame)?;
        }
        Ok(false)
    }
    fn append(&mut self, frame: EffectsFrame) -> Result<(), HandoffError> {
        if self.captured_frames == self.reservation.frame_capacity {
            return Err(HandoffError::Capacity);
        }
        let old = self.old_graph.as_ref().ok_or(HandoffError::WrongOwner)?;
        let (ordinal, serial) = old
            .handoff_pending_key(&frame)
            .ok_or(HandoffError::InvalidReceipt)?;
        let input = if frame.kind == EffectsFrameKind::Media {
            if self.tail_count != 0 || self.captured_media == self.media_count {
                return Err(HandoffError::InvalidLabel);
            }
            let label =
                self.reservation.labels[self.captured_media].ok_or(HandoffError::InvalidLabel)?;
            if frame.stamp != Some(label.input.stamp)
                || frame.begin != label.input.begin
                || frame.pitch != label.input.pitch
            {
                return Err(HandoffError::InvalidLabel);
            }
            Some(label.input)
        } else if frame.kind == EffectsFrameKind::Tail
            && frame.stamp.is_none()
            && frame.begin.is_none()
            && self.captured_media == self.media_count
        {
            None
        } else {
            return Err(HandoffError::InvalidLabel);
        };
        let width = self.reservation.bounds.preparation.device_channels as usize;
        if !frame.samples[..width].iter().all(|value| value.is_finite()) {
            return Err(HandoffError::InvalidLabel);
        }
        let begin = self.captured_frames * width;
        self.reservation.samples[begin..begin + width].copy_from_slice(&frame.samples[..width]);
        if input.is_some() {
            self.reservation.labels[self.captured_media]
                .as_mut()
                .unwrap()
                .rendered = Some(frame);
            self.captured_media += 1;
        } else {
            if self.tail_header.is_none() {
                self.tail_header = Some(frame);
            }
            self.tail_count += 1;
        }
        self.captured_frames += 1;
        // No constructor exists outside this successful FIFO append path.
        self.transfer = Some(StoredReceipt {
            owner: old.handoff_owner(),
            ordinal,
            serial,
            preparation: old.preparation(),
            frame,
            input,
            consumed: false,
        });
        Ok(())
    }
    /// Return to the actual owner-side retirement ledger; never drop it in the callback.
    pub fn take_retired_old(&mut self) -> Option<Box<PreparedEffects>> {
        if self.ready && !self.same_format() {
            self.old_graph.take()
        } else {
            None
        }
    }
    /// Identical formats keep the actual complete old DSP bank and its pending frame.
    pub fn whole_graph_mut(&mut self) -> Option<&mut PreparedEffects> {
        if self.same_format() {
            self.old_graph.as_deref_mut()
        } else {
            None
        }
    }
    pub fn take_unused_candidate(&mut self) -> Option<Box<PreparedEffects>> {
        if self.same_format() {
            self.reservation.new_graph.take()
        } else {
            None
        }
    }
    /// Future source/pitch output is admitted only after retained media is presented.
    pub fn new_graph_mut(&mut self) -> Option<&mut PreparedEffects> {
        if self.ready && !self.same_format() && self.media_prefix_drained() {
            self.reservation.new_graph.as_deref_mut()
        } else {
            None
        }
    }
    /// Updates only prepared spatial coefficients; no retained PCM is re-fed.
    pub fn retarget_spatial(&mut self, target: super::spatial::SpatialTarget) -> bool {
        if self.same_format() {
            self.old_graph
                .as_mut()
                .is_some_and(|graph| graph.retarget_spatial(target))
        } else {
            self.reservation
                .new_graph
                .as_mut()
                .is_some_and(|graph| graph.retarget_spatial(target))
        }
    }
    /// Owner-side transfer after every retained output was truly acknowledged.
    /// The caller keeps its existing counted slot with this exact new Box.
    #[cfg(test)]
    pub(crate) fn take_new_on_owner(&mut self) -> Option<Box<PreparedEffects>> {
        self.try_take_new_without_destroy()
    }
    /// Moves only the graph. The caller retains every buffer and permit in Self.
    pub(crate) fn try_take_new_without_destroy(&mut self) -> Option<Box<PreparedEffects>> {
        if self.ready
            && !self.same_format()
            && self.old_graph.is_none()
            && self.media_prefix_drained()
            && self.tail_drained()
            && self.pending.is_none()
        {
            self.reservation.new_graph.take()
        } else {
            None
        }
    }
    pub fn media_prefix_drained(&self) -> bool {
        self.ready
            && self.media_cursor >= self.media_output_frames()
            && self
                .pending
                .is_none_or(|frame| frame.kind != EffectsFrameKind::Media)
    }
    pub fn tail_drained(&self) -> bool {
        self.ready
            && self.tail_cursor >= self.tail_output_frames()
            && self
                .pending
                .is_none_or(|frame| frame.kind != EffectsFrameKind::Tail)
    }
    pub fn next_media(&mut self) -> Result<Option<HandoffFrame>, HandoffError> {
        if !self.ready {
            return Err(HandoffError::NotReady);
        }
        if let Some(frame) = self.pending {
            return if frame.kind == EffectsFrameKind::Media {
                Ok(Some(frame))
            } else {
                Err(HandoffError::MediaPending)
            };
        }
        if self.same_format() || self.media_cursor == self.media_output_frames() {
            return Ok(None);
        }
        let denominator = self.media_grid_denominator;
        let endpoint = (self.media_grid_origin
            + (self.media_cursor as u128 + 1) * self.media_grid_step)
            .min(self.media_count as u128 * denominator);
        let interval = (endpoint.saturating_sub(1) / denominator) as usize;
        let label = self.reservation.labels[interval].ok_or(HandoffError::InvalidLabel)?;
        let rendered = label.rendered.ok_or(HandoffError::InvalidLabel)?;
        let stamp = interpolate(
            label.input.begin.ok_or(HandoffError::MissingBegin)?,
            label.input.stamp,
            endpoint - interval as u128 * denominator,
            denominator,
        )?;
        let original_start = self.reservation.labels[0]
            .ok_or(HandoffError::InvalidLabel)?
            .input
            .begin
            .ok_or(HandoffError::MissingBegin)?;
        let begin = Some(self.last_media_end.map_or(
            ConvolutionStamp {
                media: super::pcm::ProcessingStamp {
                    end_frame: original_start.media.end_frame,
                    fraction: original_start.media.fraction,
                    ..stamp.media
                },
                ..stamp
            },
            |previous| ConvolutionStamp {
                media: super::pcm::ProcessingStamp {
                    end_frame: previous.media.end_frame,
                    fraction: previous.media.fraction,
                    ..stamp.media
                },
                ..stamp
            },
        ));
        let position = self.media_grid_origin + self.media_cursor as u128 * self.media_grid_step;
        let samples = self.resampled_position(position, denominator, 0, self.media_count);
        let result = self.make_frame(
            samples,
            Some(stamp),
            begin,
            rendered,
            EffectsFrameKind::Media,
        )?;
        self.pending = Some(result);
        Ok(Some(result))
    }
    /// A tail overlay is never a replacement MediaStamp or a second source consumer.
    pub fn next_tail(&mut self) -> Result<Option<HandoffFrame>, HandoffError> {
        if !self.ready {
            return Err(HandoffError::NotReady);
        }
        if !self.media_prefix_drained() {
            return Err(HandoffError::MediaPending);
        }
        if let Some(frame) = self.pending {
            return Ok(Some(frame));
        }
        if self.tail_cursor == self.tail_output_frames() {
            return Ok(None);
        }
        let header = self.tail_header.ok_or(HandoffError::InvalidLabel)?;
        let samples = self.resampled(self.tail_cursor, self.media_count, self.tail_count);
        let result = self.make_frame(samples, None, None, header, EffectsFrameKind::Tail)?;
        self.pending = Some(result);
        Ok(Some(result))
    }
    pub fn release_pending(
        &mut self,
        frame: &HandoffFrame,
        presented: bool,
    ) -> Result<(), HandoffError> {
        let pending = self.pending.ok_or(HandoffError::InvalidReceipt)?;
        if frame != &pending {
            return Err(HandoffError::InvalidReceipt);
        }
        if !presented {
            return Ok(());
        }
        if pending.kind == EffectsFrameKind::Media {
            self.media_cursor += 1;
            self.last_media_end = pending.stamp;
        } else {
            self.tail_cursor += 1;
        }
        self.pending = None;
        Ok(())
    }
    fn make_frame(
        &mut self,
        samples: [f64; MAX_CHANNELS],
        stamp: Option<ConvolutionStamp>,
        begin: Option<ConvolutionStamp>,
        source: EffectsFrame,
        kind: EffectsFrameKind,
    ) -> Result<HandoffFrame, HandoffError> {
        if self.ordinal == u64::MAX {
            return Err(HandoffError::CounterExhausted);
        }
        let latency = (u64::from(self.reservation.bounds.latency)
            * u64::from(self.reservation.new_format.sample_rate))
        .div_ceil(u64::from(
            self.reservation.bounds.preparation.format.sample_rate,
        ));
        let result = HandoffFrame {
            samples,
            stamp,
            begin,
            token: source.token,
            source_epoch: source.source_epoch,
            kind,
            pitch: source.pitch,
            effect_sequence: source.effect_sequence,
            spatial_sequence: source.spatial_sequence,
            retained_effect_latency_frames: latency.min(u32::MAX as u64) as u32,
            transition_remaining_frames: (u64::from(source.transition_remaining_frames)
                * u64::from(self.reservation.new_format.sample_rate))
            .div_ceil(u64::from(
                self.reservation.bounds.preparation.format.sample_rate,
            ))
            .min(u32::MAX as u64) as u32,
            owner: self.identity,
            ordinal: self.ordinal,
        };
        self.ordinal += 1;
        Ok(result)
    }
    fn media_output_frames(&self) -> usize {
        self.media_output_limit
    }
    fn tail_output_frames(&self) -> usize {
        output_count(
            self.tail_count,
            self.reservation.bounds.preparation.format.sample_rate,
            self.reservation.new_format.sample_rate,
        )
        .expect("Preflighted tail output count")
    }
    fn resampled(&self, index: usize, offset: usize, count: usize) -> [f64; MAX_CHANNELS] {
        let old_rate = self.reservation.bounds.preparation.format.sample_rate as u64;
        let new_rate = self.reservation.new_format.sample_rate as u64;
        let position = index as u64 * old_rate;
        self.resampled_position(position as u128, new_rate as u128, offset, count)
    }
    fn resampled_position(
        &self,
        position: u128,
        denominator: u128,
        offset: usize,
        count: usize,
    ) -> [f64; MAX_CHANNELS] {
        let left = (position / denominator) as usize;
        let fraction = (position % denominator) as f64 / denominator as f64;
        let first = (left.min(count - 1) + offset)
            * self.reservation.bounds.preparation.device_channels as usize;
        let second = ((left + 1).min(count - 1) + offset)
            * self.reservation.bounds.preparation.device_channels as usize;
        let width = self.reservation.bounds.preparation.device_channels as usize;
        let mut samples = [0.0; MAX_CHANNELS];
        for (lane, sample) in samples.iter_mut().enumerate().take(width) {
            let a = self.reservation.samples[first + lane];
            let b = self.reservation.samples[second + lane];
            *sample = a + (b - a) * fraction;
        }
        map_channels(samples, width, self.reservation.device_channels as usize)
    }
}
fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}
fn output_count(count: usize, old_rate: u32, new_rate: u32) -> Option<usize> {
    count
        .checked_mul(new_rate as usize)
        .map(|value| value.div_ceil(old_rate as usize))
}
fn interpolate(
    begin: ConvolutionStamp,
    end: ConvolutionStamp,
    numerator: u128,
    denominator: u128,
) -> Result<ConvolutionStamp, HandoffError> {
    if begin.token != end.token || begin.source_epoch != end.source_epoch || numerator > denominator
    {
        return Err(HandoffError::InvalidLabel);
    }
    let a = (u128::from(begin.media.end_frame) << 32) + u128::from(begin.media.fraction);
    let b = (u128::from(end.media.end_frame) << 32) + u128::from(end.media.fraction);
    let delta = b.checked_sub(a).ok_or(HandoffError::InvalidLabel)?;
    let point = a
        .checked_add(delta.checked_mul(numerator).ok_or(HandoffError::Capacity)? / denominator)
        .ok_or(HandoffError::Capacity)?;
    Ok(ConvolutionStamp {
        media: super::pcm::ProcessingStamp {
            end_frame: (point >> 32) as u64,
            fraction: point as u32,
            ..end.media
        },
        ..end
    })
}
fn stamp_point(stamp: ConvolutionStamp) -> (u64, u32) {
    (stamp.media.end_frame, stamp.media.fraction)
}
fn same_input(a: Option<EffectsInput>, b: Option<EffectsInput>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.stamp == b.stamp
                && a.begin == b.begin
                && a.pitch == b.pitch
                && a.samples.map(f64::to_bits) == b.samples.map(f64::to_bits)
        }
        _ => false,
    }
}
fn fixed<T: Clone>(count: usize, value: T) -> Result<Box<[T]>, HandoffError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| HandoffError::Allocation)?;
    if values.capacity() > count {
        return Err(HandoffError::Capacity);
    }
    values.resize(count, value);
    Ok(values.into_boxed_slice())
}
fn map_channels(input: [f64; MAX_CHANNELS], from: usize, to: usize) -> [f64; MAX_CHANNELS] {
    let mut output = [0.0; MAX_CHANNELS];
    if from == 1 {
        output[..to].fill(input[0]);
    } else if from <= to {
        output[..from].copy_from_slice(&input[..from]);
    } else {
        let mut counts = [0usize; MAX_CHANNELS];
        for index in 0..from {
            counts[index % to] += 1;
        }
        for index in 0..from {
            output[index % to] += input[index] / counts[index % to] as f64;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{
        effects::{EFFECTS_MIN_REPLAY_FRAMES, EffectsAlignment},
        spatial::{SpatialPlan, SpatialSettings, SpatialStep},
    };

    fn graph(rate: u32) -> Box<PreparedEffects> {
        let plan = SpatialPlan::prepare(SpatialSettings::default(), rate, 1).unwrap();
        Box::new(
            PreparedEffects::prepare(
                EffectsPreparation {
                    format: PcmFormat {
                        sample_rate: rate,
                        channels: 2,
                    },
                    device_channels: 2,
                    main_gain: 1.0,
                    send_gain: 0.0,
                    alignment: EffectsAlignment::Direct,
                    sequence: 1,
                    token: 1,
                    source_epoch: 2,
                    initial_spatial: plan.target(SpatialStep::ORIGIN),
                    replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
                },
                None,
                &AtomicBool::new(false),
            )
            .unwrap(),
        )
    }
    #[test]
    fn fifo_minted_receipt_rejects_changed_bindings_and_consumes_exactly_once() {
        let mut old = graph(8_000);
        for index in 1..=49 {
            let stamp = ConvolutionStamp {
                token: 1,
                source_epoch: 2,
                media: super::super::pcm::ProcessingStamp {
                    end_frame: index,
                    ..Default::default()
                },
            };
            old.frame(Some(EffectsInput {
                samples: [0.1; MAX_CHANNELS],
                stamp,
                begin: Some(ConvolutionStamp {
                    media: super::super::pcm::ProcessingStamp {
                        end_frame: index - 1,
                        ..stamp.media
                    },
                    ..stamp
                }),
                pitch: EffectsPitch {
                    ratio: 1.0,
                    sequence: 1,
                    transition_remaining_frames: 0,
                },
            }))
            .unwrap();
        }
        let candidate = graph(16_000);
        let reservation = HandoffReservation::reserve(
            HandoffBounds::from_graph(&old),
            candidate.preparation().format,
            2,
            Some(candidate),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let mut retained = reservation
            .capture(old, &AtomicBool::new(false))
            .unwrap_or_else(|failure| panic!("{:?}", failure.error));
        let pending = retained
            .old_graph
            .as_ref()
            .unwrap()
            .pending_output()
            .unwrap();
        retained.append(pending).unwrap();
        let mut receipt = retained.transfer.take().unwrap();
        let original_owner = receipt.owner;
        let original_serial = receipt.serial;
        let original_preparation = receipt.preparation;
        let original_input = receipt.input;
        let old = retained.old_graph.as_mut().unwrap();
        let count = old.pending_media_frames();
        let stats = old.statistics();
        for field in 0..5 {
            match field {
                0 => receipt.owner += 1,
                1 => receipt.serial = receipt.serial.map(|serial| serial + 1),
                2 => receipt.preparation.format.sample_rate += 1,
                3 => receipt.frame.samples[0] += 0.01,
                _ => {
                    receipt
                        .input
                        .as_mut()
                        .unwrap()
                        .begin
                        .as_mut()
                        .unwrap()
                        .media
                        .end_frame += 1
                }
            }
            assert_eq!(
                old.transfer_pending_for_handoff(&mut receipt),
                Err(EffectsError::InvalidReceipt)
            );
            assert_eq!(old.pending_output(), Some(pending));
            assert_eq!(old.pending_media_frames(), count);
            assert_eq!(old.statistics(), stats);
            assert!(!receipt.consumed);
            receipt.owner = original_owner;
            receipt.serial = original_serial;
            receipt.preparation = original_preparation;
            receipt.frame = pending;
            receipt.input = original_input;
        }
        old.transfer_pending_for_handoff(&mut receipt).unwrap();
        assert!(receipt.consumed);
        assert!(old.pending_output().is_none());
        assert_eq!(old.pending_media_frames(), count - 1);
        assert_eq!(old.statistics(), stats);
        assert_eq!(
            old.transfer_pending_for_handoff(&mut receipt),
            Err(EffectsError::InvalidReceipt)
        );
        assert_eq!(old.pending_media_frames(), count - 1);
        assert_eq!(old.statistics(), stats);
    }
}
