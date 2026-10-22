//! Two-bank ownership handoff; callback moves owners without dropping them.
use super::{
    effect_pin::{EffectPinOwner, EffectPinRender, EffectPinTicket, PinScope},
    effects::PreparedEffects,
    effects_factory::{
        CountedBank, CountedCurrentCaptureFailure, CountedHandoff, CountedHandoffReservation,
        EffectsBankPool, LegacyBank, PreparedBankRecord, RetiredHandoffStorage,
    },
    effects_handoff::{HandoffError, HandoffFrame},
    processing::DspReservation,
    spatial::SpatialTarget,
};
use anyhow::Result as PrepareResult;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

enum EffectBank {
    Counted(CountedBank),
    Legacy(LegacyBank),
}
impl EffectBank {
    fn actual_mut(&mut self) -> &mut PreparedEffects {
        match self {
            Self::Counted(bank) => bank.actual_mut(),
            Self::Legacy(bank) => bank.actual_mut(),
        }
    }
}
impl std::ops::Deref for EffectBank {
    type Target = PreparedEffects;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Counted(bank) => bank.graph(),
            Self::Legacy(bank) => bank.graph(),
        }
    }
}

struct ExchangeState {
    closing: AtomicBool,
    desired_sequence: AtomicU64,
    pool: EffectsBankPool,
    records: Mutex<[Option<Arc<PreparedBankRecord>>; 2]>,
    active_owner: AtomicU64,
    active_handoff: AtomicBool,
    _reservation: DspReservation,
}
pub struct EffectsControl {
    incoming: Producer<EffectBank>,
    returned: Option<Consumer<EffectBank>>,
    returned_handoffs: Option<Consumer<CountedHandoff>>,
    returned_storage: Option<Consumer<RetiredHandoffStorage>>,
    pin: EffectPinOwner,
    state: Arc<ExchangeState>,
}
pub(crate) struct EffectsReclaimer {
    returned: Consumer<EffectBank>,
    returned_handoffs: Consumer<CountedHandoff>,
    returned_storage: Consumer<RetiredHandoffStorage>,
    pin: EffectPinOwner,
    state: Arc<ExchangeState>,
}
pub struct EffectsRenderExchange {
    incoming: Consumer<EffectBank>,
    returned: Producer<EffectBank>,
    active: Option<EffectBank>,
    waiting: Option<EffectBank>,
    handoff: Option<CountedHandoff>,
    failed_capture: Option<CountedCurrentCaptureFailure>,
    returned_handoffs: Producer<CountedHandoff>,
    returned_storage: Producer<RetiredHandoffStorage>,
    pin: EffectPinRender,
    pin_owner: EffectPinOwner,
    pinned_scope: Option<PinScope>,
    state: Arc<ExchangeState>,
}
pub fn effect_exchange() -> PrepareResult<(EffectsControl, EffectsRenderExchange)> {
    // rtrb owns an aligned Arc<RingBuffer> and two pointer slots per queue.
    // Hold this small allocation budget until every queue owner is destroyed.
    let bytes = std::mem::size_of::<ExchangeState>()
        + 2 * std::mem::size_of::<usize>()
        + 2 * (std::mem::size_of::<RingBuffer<EffectBank>>()
            + std::mem::align_of::<RingBuffer<EffectBank>>()
            + 2 * std::mem::size_of::<usize>()
            + 2 * std::mem::size_of::<EffectBank>());
    let bytes = bytes
        + std::mem::size_of::<RingBuffer<CountedHandoff>>()
        + std::mem::align_of::<RingBuffer<CountedHandoff>>()
        + 2 * std::mem::size_of::<usize>()
        + std::mem::size_of::<CountedHandoff>();
    let bytes = bytes
        + std::mem::size_of::<RingBuffer<RetiredHandoffStorage>>()
        + std::mem::align_of::<RingBuffer<RetiredHandoffStorage>>()
        + 2 * std::mem::size_of::<usize>()
        + std::mem::size_of::<RetiredHandoffStorage>();
    let reservation = DspReservation::new(bytes)?;
    let (pin_owner, pin_render) = super::effect_pin::try_new()?;
    let pool = EffectsBankPool::new()?;
    let state = Arc::new(ExchangeState {
        closing: AtomicBool::new(false),
        desired_sequence: AtomicU64::new(0),
        pool,
        records: Mutex::new([None, None]),
        active_owner: AtomicU64::new(0),
        active_handoff: AtomicBool::new(false),
        _reservation: reservation,
    });
    let (incoming_tx, incoming_rx) = RingBuffer::new(2);
    let (returned_tx, returned_rx) = RingBuffer::new(2);
    let (handoff_tx, handoff_rx) = RingBuffer::new(1);
    let (storage_tx, storage_rx) = RingBuffer::new(1);
    Ok((
        EffectsControl {
            state: state.clone(),
            incoming: incoming_tx,
            returned: Some(returned_rx),
            returned_handoffs: Some(handoff_rx),
            returned_storage: Some(storage_rx),
            pin: pin_owner.clone(),
        },
        EffectsRenderExchange {
            state,
            incoming: incoming_rx,
            returned: returned_tx,
            active: None,
            waiting: None,
            handoff: None,
            failed_capture: None,
            returned_handoffs: handoff_tx,
            returned_storage: storage_tx,
            pin: pin_render,
            pin_owner,
            pinned_scope: None,
        },
    ))
}
impl EffectsControl {
    pub fn bank_pool(&self) -> EffectsBankPool {
        self.state.pool.clone()
    }
    pub fn record_for_owner(&self, owner_id: u64) -> Option<Arc<PreparedBankRecord>> {
        if owner_id == 0 {
            return None;
        }
        self.state
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .flatten()
            .find(|record| record.owner_id() == owner_id)
            .cloned()
    }
    pub fn active_owner(&self) -> u64 {
        self.state.active_owner.load(Ordering::Acquire)
    }
    pub fn pin_owner(&self) -> EffectPinOwner {
        self.pin.clone()
    }
    pub fn request_sequence(&self, sequence: u64) -> bool {
        if sequence == 0 || self.state.closing.load(Ordering::Acquire) {
            return false;
        }
        if sequence < self.state.desired_sequence.load(Ordering::Acquire) {
            return false;
        }
        self.state
            .desired_sequence
            .fetch_max(sequence, Ordering::AcqRel);
        true
    }
    pub fn install(&mut self, bank: Box<PreparedEffects>) -> Result<(), Box<PreparedEffects>> {
        if self.state.closing.load(Ordering::Acquire)
            || bank.sequence() != self.state.desired_sequence.load(Ordering::Acquire)
        {
            return Err(bank);
        }
        if self.incoming.is_full() {
            return Err(bank);
        }
        let bank = self.state.pool.adopt_legacy(bank)?;
        // This owner is the sole producer; the consumer can only free capacity.
        if self.incoming.push(EffectBank::Legacy(bank)).is_err() {
            unreachable!("Reserved single-producer legacy admission slot");
        }
        Ok(())
    }
    pub fn install_counted(&mut self, bank: CountedBank) -> Result<(), CountedBank> {
        if self.state.closing.load(Ordering::Acquire)
            || self.incoming.is_full()
            || !bank.belongs_to(&self.state.pool)
            || bank.graph().sequence() != self.state.desired_sequence.load(Ordering::Acquire)
        {
            return Err(bank);
        }
        {
            let mut records = self
                .state
                .records
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(slot) = records.iter_mut().find(|slot| slot.is_none()) else {
                return Err(bank);
            };
            *slot = Some(bank.record().clone());
        }
        if self.incoming.push(EffectBank::Counted(bank)).is_err() {
            unreachable!("Reserved single-producer counted admission slot");
        }
        Ok(())
    }
    /// Call only on the preparation/device owner, never the callback.
    pub fn reclaim(&mut self) -> usize {
        let mut count = 0;
        while let Some(storage) = self
            .returned_storage
            .as_mut()
            .and_then(|returned| returned.pop().ok())
        {
            drop(storage);
        }
        while let Some(handoff) = self
            .returned_handoffs
            .as_mut()
            .and_then(|returned| returned.pop().ok())
        {
            retire_handoff(handoff, &self.state);
            count += 1;
        }
        while let Some(bank) = self
            .returned
            .as_mut()
            .and_then(|returned| returned.pop().ok())
        {
            retire_bank(bank, &self.state);
            count += 1;
        }
        count
    }
    pub fn owned_banks(&self) -> usize {
        self.state.pool.occupied_banks()
    }
    pub fn begin_shutdown(&self) {
        self.state.closing.store(true, Ordering::Release);
        self.state.pool.begin_shutdown();
    }
    pub(crate) fn take_reclaimer(&mut self) -> Option<EffectsReclaimer> {
        self.returned.take().map(|returned| EffectsReclaimer {
            state: self.state.clone(),
            returned,
            returned_handoffs: self
                .returned_handoffs
                .take()
                .expect("One actual handoff reclaimer"),
            returned_storage: self
                .returned_storage
                .take()
                .expect("One actual storage reclaimer"),
            pin: self.pin.clone(),
        })
    }
}
impl EffectsReclaimer {
    pub(crate) fn bank_pool(&self) -> EffectsBankPool {
        self.state.pool.clone()
    }
    pub(crate) fn pin_owner(&self) -> EffectPinOwner {
        self.pin.clone()
    }
    pub(crate) fn active_owner(&self) -> u64 {
        self.state.active_owner.load(Ordering::Acquire)
    }
    pub(crate) fn retained_handoff_active(&self) -> bool {
        self.state.active_handoff.load(Ordering::Acquire)
    }
    pub(crate) fn record(&self, id: u64) -> Option<Arc<PreparedBankRecord>> {
        self.state
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .flatten()
            .find(|record| record.owner_id() == id)
            .cloned()
    }
    pub(crate) fn forget_retired_record(&self, id: u64) {
        forget_record(&self.state, id);
    }
    pub(crate) fn register_record(&self, record: Arc<PreparedBankRecord>) -> bool {
        let mut records = self
            .state
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if records
            .iter()
            .flatten()
            .any(|item| Arc::ptr_eq(item, &record))
        {
            return true;
        }
        let Some(slot) = records.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        *slot = Some(record);
        true
    }
    pub(crate) fn reclaim(&mut self) -> usize {
        let mut count = 0;
        while let Ok(storage) = self.returned_storage.pop() {
            drop(storage);
        }
        while let Ok(handoff) = self.returned_handoffs.pop() {
            retire_handoff(handoff, &self.state);
            count += 1;
        }
        while let Ok(bank) = self.returned.pop() {
            retire_bank(bank, &self.state);
            count += 1;
        }
        count
    }
}
impl Drop for EffectsReclaimer {
    fn drop(&mut self) {
        self.reclaim();
    }
}
impl Drop for EffectsControl {
    fn drop(&mut self) {
        self.begin_shutdown();
        self.reclaim();
    }
}
impl EffectsRenderExchange {
    pub(crate) fn pin_owner_on_owner(&self) -> EffectPinOwner {
        self.pin_owner.clone()
    }
    pub(crate) fn pinned_scope_on_callback(&self) -> Option<PinScope> {
        self.pinned_scope
    }
    pub fn active(&mut self) -> Option<&mut PreparedEffects> {
        if let Some(handoff) = &mut self.handoff {
            return handoff.graph_mut();
        }
        self.active.as_mut().map(EffectBank::actual_mut)
    }
    pub(crate) fn bus_channels(&self) -> Option<u16> {
        self.handoff
            .as_ref()
            .map(|handoff| handoff.expected_new_bus().channels)
            .or_else(|| self.active.as_ref().map(|bank| bank.effect_channels()))
    }
    pub(crate) fn has_handoff(&self) -> bool {
        self.handoff.is_some() || self.failed_capture.is_some()
    }
    pub(crate) fn has_active_on_owner(&self) -> bool {
        self.active.is_some()
    }
    pub(crate) fn owned_record_on_owner(&self) -> Option<Arc<PreparedBankRecord>> {
        if let Some(handoff) = &self.handoff {
            Some(handoff.record().clone())
        } else if let Some(failure) = &self.failed_capture {
            Some(failure.record().clone())
        } else if let Some(EffectBank::Counted(bank)) = &self.active {
            Some(bank.record().clone())
        } else {
            None
        }
    }
    pub(crate) fn can_retarget_handoff_on_owner(&self) -> bool {
        self.handoff.as_ref().is_some_and(|handoff| {
            handoff.can_retarget_unpresented_on_owner()
                || handoff.can_retarget_prefix_remainder_on_owner()
        })
    }
    pub(crate) fn can_capture_handoff_on_owner(&self) -> bool {
        self.failed_capture.is_none()
            && self
                .handoff
                .as_ref()
                .is_some_and(CountedHandoff::can_capture_current_on_owner)
    }
    pub(crate) fn capture_handoff_on_owner(
        &mut self,
        reservation: CountedHandoffReservation,
    ) -> PrepareResult<()> {
        let handoff = self
            .handoff
            .take()
            .expect("Actual returned handoff remains owned");
        match handoff.capture_current_for_recovery_on_owner(reservation, &AtomicBool::new(false)) {
            Ok(captured) => {
                self.handoff = Some(captured);
                Ok(())
            }
            Err(failure) => {
                let message = format!("Effect current capture failed: {:?}", failure.error);
                match failure.recover_old_on_owner() {
                    Ok((old, retired)) => {
                        self.handoff = Some(old);
                        if let Some(id) = retired {
                            forget_record(&self.state, id);
                        }
                    }
                    Err(failure) => self.failed_capture = Some(failure),
                }
                Err(anyhow::anyhow!(message))
            }
        }
    }
    pub(crate) fn retire_queued_on_owner(&mut self) {
        if let Some(bank) = self.waiting.take() {
            retire_bank(bank, &self.state);
        }
        while let Ok(bank) = self.incoming.pop() {
            retire_bank(bank, &self.state);
        }
    }
    pub(crate) fn retarget_handoff_on_owner(&mut self, bank: CountedBank) -> PrepareResult<()> {
        let handoff = self.handoff.as_mut().expect("Returned retained handoff");
        let retarget = if handoff.can_retarget_unpresented_on_owner() {
            handoff.retarget_unpresented_on_owner(bank)
        } else {
            handoff.retarget_prefix_remainder_on_owner(bank)
        };
        let retired = retarget.map_err(|failure| {
            let error = failure.error;
            drop(failure.candidate);
            anyhow::anyhow!("Retained effect prefix retarget failed: {error:?}")
        })?;
        forget_record(&self.state, retired);
        let record = handoff.record().clone();
        let mut records = self
            .state
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let slot = records
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("Actual retired bank frees its registry entry");
        *slot = Some(record);
        Ok(())
    }
    /// Only the owner of a returned RenderState may discard a hard-invalid stream.
    pub(crate) fn invalidate_scope_on_owner(&mut self, token: u64, epoch: u64, hard: bool) {
        if self.failed_capture.as_ref().is_some_and(|failure| {
            let plan = failure.record().bounds().preparation();
            hard || plan.token != token || plan.source_epoch != epoch
        }) {
            retire_capture_failure(self.failed_capture.take().unwrap(), &self.state);
            self.state.active_owner.store(0, Ordering::Release);
        }
        if self.handoff.as_ref().is_some_and(|handoff| {
            let plan = handoff.record().bounds().preparation();
            hard || plan.token != token || plan.source_epoch != epoch
        }) {
            retire_handoff(self.handoff.take().unwrap(), &self.state);
            self.state.active_owner.store(0, Ordering::Release);
        }
        if self.active.as_ref().is_some_and(|bank| {
            let plan = bank.preparation();
            hard || plan.token != token || plan.source_epoch != epoch
        }) {
            retire_bank(self.active.take().unwrap(), &self.state);
            self.state.active_owner.store(0, Ordering::Release);
        }
    }
    pub(crate) fn retained_drained(&self) -> bool {
        self.failed_capture.is_none()
            && self
                .handoff
                .as_ref()
                .is_none_or(CountedHandoff::retained_output_drained)
    }
    pub(crate) fn retained_media_pending(&self) -> bool {
        self.handoff
            .as_ref()
            .is_some_and(|handoff| !handoff.media_prefix_drained())
    }
    pub(crate) fn next_retained_media(&mut self) -> Result<Option<HandoffFrame>, HandoffError> {
        self.handoff
            .as_mut()
            .map_or(Ok(None), CountedHandoff::next_media)
    }
    pub(crate) fn next_retained_tail(&mut self) -> Result<Option<HandoffFrame>, HandoffError> {
        self.handoff
            .as_mut()
            .map_or(Ok(None), CountedHandoff::next_tail)
    }
    pub(crate) fn release_retained(
        &mut self,
        frame: &HandoffFrame,
        presented: bool,
    ) -> Result<(), HandoffError> {
        self.handoff
            .as_mut()
            .ok_or(HandoffError::InvalidReceipt)?
            .release_pending(frame, presented)
    }
    pub(crate) fn retarget_spatial(&mut self, target: SpatialTarget) {
        if let Some(handoff) = &mut self.handoff {
            handoff.retarget_spatial(target);
        } else if let Some(active) = self.active() {
            active.retarget_spatial(target);
        }
    }
    /// Requires the actual RenderState returned by its closed callback.
    pub(crate) fn take_counted_on_owner(
        &mut self,
        pin: &EffectPinTicket,
    ) -> PrepareResult<CountedBank> {
        anyhow::ensure!(
            self.handoff.is_none(),
            "An effect handoff still owns retained output"
        );
        let bank = self
            .active
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Pinned bank is absent"))?;
        let plan = bank.preparation();
        anyhow::ensure!(
            pin.is_pinned_for(PinScope {
                bank_owner: bank.handoff_owner(),
                media_token: plan.token,
                source_epoch: plan.source_epoch,
                sample_rate: plan.format.sample_rate,
                device_channels: plan.device_channels
            }),
            "Actual returned bank does not match its acknowledged pin"
        );
        anyhow::ensure!(
            matches!(bank, EffectBank::Counted(_)),
            "Legacy bank has no immutable preparation recipe"
        );
        let Some(EffectBank::Counted(bank)) = self.active.take() else {
            unreachable!()
        };
        self.state.active_owner.store(0, Ordering::Release);
        Ok(bank)
    }
    pub(crate) fn restore_counted_on_owner(&mut self, bank: CountedBank) {
        assert!(self.active.is_none() && self.handoff.is_none());
        self.active = Some(EffectBank::Counted(bank));
    }
    pub(crate) fn take_parked_counted_on_owner(&mut self) -> PrepareResult<CountedBank> {
        anyhow::ensure!(
            self.handoff.is_none(),
            "Retained output still owns the effect bank"
        );
        anyhow::ensure!(
            matches!(self.active, Some(EffectBank::Counted(_))),
            "Returned bank has no immutable preparation recipe"
        );
        let Some(EffectBank::Counted(bank)) = self.active.take() else {
            unreachable!()
        };
        self.state.active_owner.store(0, Ordering::Release);
        Ok(bank)
    }
    pub(crate) fn install_handoff_on_owner(&mut self, handoff: CountedHandoff) {
        assert!(self.active.is_none() && self.handoff.is_none() && handoff.ready());
        self.handoff = Some(handoff);
    }
    pub(crate) fn install_unfinished_handoff_on_owner(&mut self, handoff: CountedHandoff) {
        assert!(self.active.is_none() && self.handoff.is_none());
        self.handoff = Some(handoff);
    }
    pub(crate) fn advance_handoff_on_owner(&mut self) -> PrepareResult<bool> {
        if let Some(failure) = self.failed_capture.take() {
            match failure.recover_old_on_owner() {
                Ok((old, retired)) => {
                    self.handoff = Some(old);
                    if let Some(id) = retired {
                        forget_record(&self.state, id);
                    }
                }
                Err(failure) => {
                    self.failed_capture = Some(failure);
                    anyhow::bail!("Actual detached capture owners require recovery");
                }
            }
        }
        if let Some((bank, storage)) = self
            .handoff
            .as_mut()
            .and_then(CountedHandoff::try_take_finished_on_callback)
        {
            drop(storage);
            self.handoff = None;
            self.active = Some(EffectBank::Counted(bank));
            return Ok(true);
        }
        let Some(handoff) = &mut self.handoff else {
            return Ok(true);
        };
        if !handoff.ready()
            && !handoff
                .advance_capture(&std::sync::atomic::AtomicBool::new(false))
                .map_err(|error| anyhow::anyhow!("Effect capture failed: {error:?}"))?
        {
            return Ok(false);
        }
        let retired = handoff.retire_old_on_owner();
        let record = handoff.record().clone();
        if let Some(id) = retired {
            forget_record(&self.state, id);
        }
        let mut records = self
            .state
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !records
            .iter()
            .flatten()
            .any(|item| Arc::ptr_eq(item, &record))
        {
            let slot = records
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or_else(|| anyhow::anyhow!("Actual effect registry remains occupied"))?;
            *slot = Some(record);
        }
        Ok(true)
    }
    /// Call at a callback boundary; at most two owners are moved per invocation.
    pub fn apply(&mut self, token: u64, epoch: u64, rate: u32, device_channels: u16) {
        self.apply_with_layout(token, epoch, rate, device_channels, None);
    }
    pub(crate) fn apply_with_layout(
        &mut self,
        token: u64,
        epoch: u64,
        rate: u32,
        device_channels: u16,
        required_channels: Option<u16>,
    ) {
        self.pinned_scope = None;
        let closing = self.state.closing.load(Ordering::Acquire);
        if !self.returned_storage.is_full()
            && let Some(handoff) = &mut self.handoff
            && let Some((bank, storage)) = handoff.try_take_finished_on_callback()
        {
            self.returned_storage
                .push(storage)
                .unwrap_or_else(|_| panic!("Reserved one storage return slot"));
            // Every owning field was moved; dropping this empty scalar
            // shell performs no heap work and never releases a bank slot.
            self.handoff = None;
            self.active = Some(EffectBank::Counted(bank));
        }
        if let Some(handoff) = &self.handoff {
            let plan = handoff.record().bounds().preparation();
            if closing
                || plan.token != token
                || plan.source_epoch != epoch
                || plan.format.sample_rate != rate
                || plan.device_channels != device_channels
            {
                if self.returned_handoffs.is_full() {
                    return;
                }
                self.returned_handoffs
                    .push(self.handoff.take().unwrap())
                    .unwrap_or_else(|_| panic!("Single retained handoff return slot"));
                self.state.active_owner.store(0, Ordering::Release);
            } else {
                let id = handoff.record().owner_id();
                self.state.active_handoff.store(true, Ordering::Release);
                self.state.active_owner.store(id, Ordering::Release);
                let scope = PinScope {
                    bank_owner: id,
                    media_token: token,
                    source_epoch: epoch,
                    sample_rate: rate,
                    device_channels,
                };
                self.pinned_scope = self.pin.poll(scope).then_some(scope);
                return;
            }
        }
        self.state.active_handoff.store(false, Ordering::Release);
        if self.active.as_ref().is_some_and(|active| {
            let plan = active.preparation();
            closing
                || plan.token != token
                || plan.source_epoch != epoch
                || plan.format.sample_rate != rate
                || plan.device_channels != device_channels
        }) {
            if self.returned.is_full() {
                return;
            }
            let bank = self.active.take().unwrap();
            self.returned
                .push(bank)
                .unwrap_or_else(|_| panic!("Single callback producer reserved its return slot"));
        }
        let actual_owner = self.active.as_ref().map_or(0, |bank| bank.handoff_owner());
        self.state
            .active_owner
            .store(actual_owner, Ordering::Release);
        let scope = PinScope {
            bank_owner: actual_owner,
            media_token: token,
            source_epoch: epoch,
            sample_rate: rate,
            device_channels,
        };
        let pinned = self.pin.poll(scope);
        self.pinned_scope = pinned.then_some(scope);
        if pinned {
            // Waiting and incoming are not the pinned actual active owner.
            // Return them without dropping their allocations on the callback.
            for _ in 0..2 {
                if self.returned.is_full() {
                    break;
                }
                let candidate = self.waiting.take().or_else(|| self.incoming.pop().ok());
                let Some(candidate) = candidate else {
                    break;
                };
                self.returned
                    .push(candidate)
                    .unwrap_or_else(|_| panic!("Reserved pin candidate return slot"));
            }
            return;
        }
        for _ in 0..2 {
            if self.returned.is_full() {
                return;
            }
            let bank = match self.waiting.take() {
                Some(bank) => bank,
                None => match self.incoming.pop() {
                    Ok(bank) => bank,
                    Err(_) => return,
                },
            };
            let plan = bank.preparation();
            if closing
                || plan.sequence != self.state.desired_sequence.load(Ordering::Acquire)
                || plan.token != token
                || plan.source_epoch != epoch
                || plan.format.sample_rate != rate
                || plan.device_channels != device_channels
            {
                self.returned.push(bank).unwrap_or_else(|_| {
                    panic!("Single callback producer reserved its return slot")
                });
                continue;
            }
            if required_channels.is_some_and(|channels| channels != plan.format.channels) {
                self.waiting = Some(bank);
                return;
            }
            if let Some(active) = &self.active {
                // Live replacement needs an actual temporal replay policy. Keep
                // both owners until the controller fences/returns the old bank.
                if active.pending_media_frames() != 0
                    || active.pending_output().is_some()
                    || (active.statistics().accepted_media_frames != 0 && !active.drained())
                {
                    self.waiting = Some(bank);
                    return;
                }
            }
            if let Some(active) = self.active.take() {
                self.returned
                    .push(active)
                    .unwrap_or_else(|_| panic!("Reserved old bank return slot"));
            }
            self.active = Some(bank);
            self.state.active_owner.store(
                self.active.as_ref().unwrap().handoff_owner(),
                Ordering::Release,
            );
        }
    }
    /// Call only after callback ownership has returned to its device/engine owner.
    pub fn close_on_owner(mut self) {
        self.pin_owner.close_on_owner();
        self.state.active_owner.store(0, Ordering::Release);
        self.state.closing.store(true, Ordering::Release);
        self.state.pool.begin_shutdown();
        if let Some(failure) = self.failed_capture.take() {
            retire_capture_failure(failure, &self.state);
        }
        if let Some(handoff) = self.handoff.take() {
            retire_handoff(handoff, &self.state);
        }
        if let Some(bank) = self.active.take() {
            retire_bank(bank, &self.state);
        }
        if let Some(bank) = self.waiting.take() {
            retire_bank(bank, &self.state);
        }
        while let Ok(bank) = self.incoming.pop() {
            retire_bank(bank, &self.state);
        }
    }
    pub(crate) fn close_owned(&mut self) {
        self.pin_owner.close_on_owner();
        self.state.active_owner.store(0, Ordering::Release);
        self.state.closing.store(true, Ordering::Release);
        self.state.pool.begin_shutdown();
        if let Some(failure) = self.failed_capture.take() {
            retire_capture_failure(failure, &self.state);
        }
        if let Some(handoff) = self.handoff.take() {
            retire_handoff(handoff, &self.state);
        }
        if let Some(bank) = self.active.take() {
            retire_bank(bank, &self.state);
        }
        if let Some(bank) = self.waiting.take() {
            retire_bank(bank, &self.state);
        }
        while let Ok(bank) = self.incoming.pop() {
            retire_bank(bank, &self.state);
        }
    }
}

fn retire_capture_failure(failure: CountedCurrentCaptureFailure, state: &ExchangeState) {
    let ids = failure.owned_record_ids();
    drop(failure);
    for id in ids.into_iter().flatten() {
        forget_record(state, id);
    }
}
fn retire_bank(bank: EffectBank, state: &ExchangeState) {
    let owner_id = bank.handoff_owner();
    drop(bank);
    forget_record(state, owner_id);
}
fn retire_handoff(handoff: CountedHandoff, state: &ExchangeState) {
    let ids = handoff.owned_record_ids();
    drop(handoff);
    for id in ids.into_iter().flatten() {
        forget_record(state, id);
    }
}
fn forget_record(state: &ExchangeState, owner_id: u64) {
    let mut records = state
        .records
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for record in records.iter_mut() {
        if record
            .as_ref()
            .is_some_and(|record| record.owner_id() == owner_id)
        {
            *record = None;
        }
    }
}
