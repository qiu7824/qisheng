//! Owner-side replacement of a live counted effects graph.

use super::{RenderKernel, RenderState, Shared};
use crate::audio::{
    effect_pin::{EffectPinTicket, PinScope},
    effects_factory::{CountedBank, CountedHandoffReservation, PreparedBankRecord},
    processing::DspReservation,
};
use anyhow::{Result, bail, ensure};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// This single retained request is accessed only by preparation/output owners.
/// The callback reads no mutex and never removes or destroys a request.
pub(super) struct LiveEffectsMailbox {
    pending: Mutex<Option<PreparedLiveEffects>>,
    sequence: AtomicU64,
    busy: AtomicBool,
    retry: AtomicBool,
    _reservation: DspReservation,
}

impl LiveEffectsMailbox {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            pending: Mutex::new(None),
            sequence: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            retry: AtomicBool::new(false),
            _reservation: DspReservation::new(std::mem::size_of::<Self>())?,
        })
    }
    pub(super) fn occupied(&self) -> bool {
        self.sequence.load(Ordering::Acquire) != 0
    }
    pub(super) fn retry_requested(&self) -> bool {
        self.retry.swap(false, Ordering::AcqRel)
    }
    pub(super) fn offer(&self, prepared: PreparedLiveEffects) -> Result<()> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            pending.is_none() && !self.occupied(),
            "The actual live effects owner is still occupied"
        );
        let sequence = prepared.sequence();
        *pending = Some(prepared);
        self.sequence.store(sequence, Ordering::Release);
        Ok(())
    }
    pub(super) fn claim(&self) -> Option<PinScope> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let scope = pending.as_ref()?.scope();
        if self.busy.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(scope)
    }
    pub(super) fn take_on_owner(&self) -> Option<PreparedLiveEffects> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
    pub(super) fn defer_on_owner(&self, prepared: PreparedLiveEffects) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(
            pending.is_none(),
            "A single actual live request remains owned"
        );
        *pending = Some(prepared);
        self.busy.store(false, Ordering::Release);
    }
    pub(super) fn finish_on_owner(&self, retry: bool) {
        // The owner has already returned its actual state or retained it for
        // recovery before making admission available to another request.
        drop(self.take_on_owner());
        self.retry.fetch_or(retry, Ordering::AcqRel);
        self.sequence.store(0, Ordering::Release);
        self.busy.store(false, Ordering::Release);
    }
    pub(super) fn cancel_pending_on_owner(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !self.busy.load(Ordering::Acquire) {
            drop(pending.take());
            self.sequence.store(0, Ordering::Release);
        }
    }
}

/// All capture computation runs between Memory callbacks on its sole renderer.
pub(super) fn service_memory_owner(kernel: &mut RenderKernel, shared: &Shared) {
    let Some(scope) = shared.live_effects.claim() else {
        return;
    };
    let pin_owner = kernel
        .state
        .as_ref()
        .expect("The Memory renderer owns its actual state")
        .effects
        .pin_owner_on_owner();
    let Ok(pin) = pin_owner.request(scope) else {
        shared.live_effects.finish_on_owner(true);
        return;
    };
    kernel.render::<f32>(&mut []);
    let Some(prepared) = shared.live_effects.take_on_owner() else {
        drop(pin);
        shared.live_effects.finish_on_owner(true);
        return;
    };
    let mut state = kernel.state.take().expect("One actual Memory render state");
    let mut deferred = None;
    let mut retry = false;
    match prepared.start_on_owner(&mut state, &pin, &shared.shutdown) {
        Ok(LiveEffectsBegin::Deferred(prepared)) => deferred = Some(prepared),
        Ok(LiveEffectsBegin::Capturing(capture)) => loop {
            match capture.advance_on_owner(&mut state, &shared.shutdown) {
                Ok(LiveEffectsProgress::Ready) => break,
                Ok(LiveEffectsProgress::Pending) => std::thread::yield_now(),
                Ok(LiveEffectsProgress::Suspended) => {
                    retry = true;
                    break;
                }
                Err(_) => {
                    retry = true;
                    if validate_scope(&state, scope).is_err() {
                        state.effects.invalidate_scope_on_owner(0, 0, true);
                        break;
                    }
                    shared.errors.fetch_or(8, Ordering::AcqRel);
                    // The actual evolved owners remain in state until recovery
                    // completes, media invalidation occurs or shutdown joins us.
                    std::thread::park_timeout(std::time::Duration::from_millis(2));
                }
            }
        },
        Err(_) => retry = true,
    }
    if shared.shutdown.load(Ordering::Acquire) {
        state.effects.invalidate_scope_on_owner(0, 0, true);
    }
    kernel.state = Some(state);
    drop(pin);
    // Publish the actual candidate owner before the preparation engine may
    // admit another bank; this zero-length callback pulls no source/pitch PCM.
    kernel.render::<f32>(&mut []);
    if let Some(prepared) = deferred {
        shared.live_effects.defer_on_owner(prepared);
    } else {
        shared.live_effects.finish_on_owner(retry);
    }
}

/// The reservation retains the real candidate and the complete old-output FIFO.
/// Construct this on the preparation owner before asking for callback handback.
pub(super) struct PreparedLiveEffects {
    old_record: Arc<PreparedBankRecord>,
    reservation: CountedHandoffReservation,
    scope: PinScope,
    sequence: u64,
    new_owner: u64,
}

/// A deferred request keeps its actual candidate while the existing prefix is
/// presented. The endpoint continues rendering that prefix before another pin.
#[expect(
    clippy::large_enum_variant,
    reason = "The deferred variant retains its inline counted reservation and exact candidate instead of allocating another uncharged owner container."
)]
pub(super) enum LiveEffectsBegin {
    Capturing(LiveEffectsCapture),
    Deferred(PreparedLiveEffects),
}

/// The returned RenderState, rather than this scalar receipt, owns both actual
/// graphs and retained PCM throughout bounded capture and cancellation.
pub(super) struct LiveEffectsCapture {
    scope: PinScope,
    sequence: u64,
    new_owner: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LiveEffectsProgress {
    Pending,
    Suspended,
    Ready,
}

impl PreparedLiveEffects {
    pub(super) fn prepare(
        old_record: Arc<PreparedBankRecord>,
        candidate: CountedBank,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let old = old_record.bounds().preparation();
        let new = candidate.graph().preparation();
        ensure!(
            new.token == old.token
                && new.source_epoch == old.source_epoch
                && new.format == old.format
                && new.device_channels == old.device_channels,
            "Live effects replacement requires the same actual media and endpoint format"
        );
        ensure!(
            new.sequence > old.sequence
                && candidate.graph().statistics().accepted_media_frames == 0
                && candidate.graph().pending_output().is_none()
                && candidate.graph().can_accept_input(),
            "Live effects replacement requires a fresh actual candidate"
        );
        let scope = PinScope {
            bank_owner: old_record.owner_id(),
            media_token: old.token,
            source_epoch: old.source_epoch,
            sample_rate: old.format.sample_rate,
            device_channels: old.device_channels,
        };
        let new_owner = candidate.record().owner_id();
        let reservation = CountedHandoffReservation::reserve(
            old_record.clone(),
            new.format,
            new.device_channels,
            Some(candidate),
            cancel,
        )
        .map_err(|failure| {
            anyhow::anyhow!(
                "Live effects output reservation failed: {:?}",
                failure.retire_candidate_on_owner()
            )
        })?;
        Ok(Self {
            old_record,
            reservation,
            scope,
            sequence: new.sequence,
            new_owner,
        })
    }

    pub(super) fn scope(&self) -> PinScope {
        self.scope
    }

    pub(super) fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Invoke only while the actual RenderState has returned from its callback.
    /// A pin alone never grants mutable access to a still-running callback state.
    pub(super) fn start_on_owner(
        self,
        state: &mut RenderState,
        pin: &EffectPinTicket,
        cancel: &AtomicBool,
    ) -> Result<LiveEffectsBegin> {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Live effects replacement was cancelled before capture"
        );
        ensure!(
            pin.is_pinned_for(self.scope),
            "Live effects replacement lacks its actual callback pin acknowledgement"
        );
        validate_scope(state, self.scope)?;
        ensure!(
            state
                .effects
                .owned_record_on_owner()
                .is_some_and(|record| Arc::ptr_eq(&record, &self.old_record)),
            "Returned live effects owner changed after preparation"
        );
        if state.effects.has_handoff() && !state.effects.can_capture_handoff_on_owner() {
            return Ok(LiveEffectsBegin::Deferred(self));
        }

        let Self {
            old_record: _,
            reservation,
            scope,
            sequence,
            new_owner,
        } = self;
        if state.effects.has_handoff() {
            state.effects.capture_handoff_on_owner(reservation)?;
        } else {
            let old = state.effects.take_counted_on_owner(pin)?;
            let captured = match reservation.capture(old, cancel) {
                Ok(captured) => captured,
                Err(failure) => {
                    let error = failure.error;
                    let (old, _) = failure.recover_old_on_owner();
                    state.effects.restore_counted_on_owner(old);
                    bail!("Live effects capture was rejected: {error:?}");
                }
            };
            state.effects.install_unfinished_handoff_on_owner(captured);
        }

        state.retained_pitch_source_rate = state.pitch.sample_rate();
        state.retained_pitch_source_latency_frames = state.pitch.latency_frames();
        state.retained_pitch_latency_frames = (state.retained_pitch_source_latency_frames as u64
            * state.format.sample_rate as u64)
            .div_ceil(state.retained_pitch_source_rate as u64)
            .min(u32::MAX as u64) as u32;
        Ok(LiveEffectsBegin::Capturing(LiveEffectsCapture {
            scope,
            sequence,
            new_owner,
        }))
    }
}

impl LiveEffectsCapture {
    #[cfg(test)]
    pub(super) fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Advances one existing bounded capture batch. Cancellation leaves the
    /// evolved graph, exact labels, FIFO and both counted owners in RenderState.
    pub(super) fn advance_on_owner(
        &self,
        state: &mut RenderState,
        cancel: &AtomicBool,
    ) -> Result<LiveEffectsProgress> {
        validate_scope(state, self.scope)?;
        ensure!(
            state.effects.owned_record_on_owner().is_some_and(|record| {
                record.owner_id() == self.new_owner
                    && record.bounds().preparation().sequence == self.sequence
            }),
            "Live effects capture no longer owns its actual candidate"
        );
        if cancel.load(Ordering::Acquire) {
            return Ok(LiveEffectsProgress::Suspended);
        }
        Ok(if state.effects.advance_handoff_on_owner()? {
            LiveEffectsProgress::Ready
        } else {
            LiveEffectsProgress::Pending
        })
    }
}

fn validate_scope(state: &RenderState, scope: PinScope) -> Result<()> {
    ensure!(
        state.format.sample_rate == scope.sample_rate
            && state.format.channels == scope.device_channels
            && state.shared.token.load(Ordering::Acquire) == scope.media_token
            && !state.presentation_fence(scope.source_epoch).0,
        "Returned live effects state has a different media or endpoint scope"
    );
    Ok(())
}

#[cfg(test)]
#[path = "live_effects_tests.rs"]
mod tests;
