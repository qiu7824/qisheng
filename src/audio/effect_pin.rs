//! Callback acknowledgement of an exact active-bank scope before owner handback.
//!
//! A pin freezes bank replacement, not PCM rendering or hard media invalidation.
//! Cancellation requests an unpin; only a callback acknowledgement or actual
//! owner-side closure permits the next operation. Render handles are destroyed
//! only after their RenderState has returned to the device or engine owner.

use super::{pcm::MAX_CHANNELS, processing::DspReservation};
use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
    },
};

const REPLY_BITS: u32 = 3;
const MAX_SERIAL: u64 = u64::MAX >> REPLY_BITS;
const PINNED: u64 = 1;
const REJECTED: u64 = 2;
const RELEASED: u64 = 3;
const CLOSED: u64 = 4;

/// Exact live callback identity. Zero bank ownership denotes no effects bank;
/// zero media token and epoch together denote an idle output endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinScope {
    pub bank_owner: u64,
    pub media_token: u64,
    pub source_epoch: u64,
    pub sample_rate: u32,
    pub device_channels: u16,
}
impl PinScope {
    fn validate(self) -> Result<()> {
        ensure!(
            (8000..=384000).contains(&self.sample_rate)
                && (1..=MAX_CHANNELS as u16).contains(&self.device_channels),
            "Invalid output pin format"
        );
        ensure!(
            (self.media_token == 0) == (self.source_epoch == 0),
            "Output pin media token and epoch must share idle state"
        );
        ensure!(
            self.bank_owner == 0 || self.media_token != 0,
            "An effect bank requires a live media scope"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinIdentity {
    pub serial: u64,
    pub scope: PinScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinStatus {
    Requested,
    Pinned,
    CancelRequested,
    Rejected,
    Released,
    Closed,
}

struct OwnerState {
    serial: u64,
    admitted: Option<PinIdentity>,
    closed: bool,
}
struct Shared {
    owner: Mutex<OwnerState>,
    closed: AtomicBool,
    request_serial: AtomicU64,
    bank_owner: AtomicU64,
    media_token: AtomicU64,
    source_epoch: AtomicU64,
    sample_rate: AtomicU32,
    device_channels: AtomicU16,
    cancel_serial: AtomicU64,
    // Serial and response state commit in one atomic word. Payload fields remain
    // immutable until the corresponding terminal callback acknowledgement.
    reply: AtomicU64,
    // Includes the Arc allocation headers and stays held through final Arc drop.
    _reservation: DspReservation,
}

#[derive(Clone)]
pub struct EffectPinOwner {
    shared: Arc<Shared>,
}
pub struct EffectPinRender {
    shared: Arc<Shared>,
    observed_serial: u64,
    scope: Option<PinScope>,
    phase: RenderPhase,
}
#[derive(Clone, Copy)]
enum RenderPhase {
    Dormant,
    Pinned,
    Rejected,
    Released,
}

/// Sole request ticket. Dropping it requests cancellation and never fabricates
/// acknowledgement of callback release.
pub struct EffectPinTicket {
    shared: Arc<Shared>,
    identity: PinIdentity,
}

pub fn try_new() -> Result<(EffectPinOwner, EffectPinRender)> {
    let reservation = DspReservation::new(reservation_bytes())?;
    let shared = Arc::new(Shared {
        owner: Mutex::new(OwnerState {
            serial: 0,
            admitted: None,
            closed: false,
        }),
        closed: AtomicBool::new(false),
        request_serial: AtomicU64::new(0),
        bank_owner: AtomicU64::new(0),
        media_token: AtomicU64::new(0),
        source_epoch: AtomicU64::new(0),
        sample_rate: AtomicU32::new(0),
        device_channels: AtomicU16::new(0),
        cancel_serial: AtomicU64::new(0),
        reply: AtomicU64::new(0),
        _reservation: reservation,
    });
    Ok((
        EffectPinOwner {
            shared: shared.clone(),
        },
        EffectPinRender {
            shared,
            observed_serial: 0,
            scope: None,
            phase: RenderPhase::Dormant,
        },
    ))
}

fn reservation_bytes() -> usize {
    size_of::<Shared>() + 2 * size_of::<usize>()
}
fn reply(serial: u64, code: u64) -> u64 {
    (serial << REPLY_BITS) | code
}
fn acknowledged(word: u64, serial: u64) -> bool {
    word >> REPLY_BITS == serial && matches!(word & 7, REJECTED | RELEASED | CLOSED)
}

impl EffectPinOwner {
    pub fn reserved_bytes(&self) -> usize {
        reservation_bytes()
    }
    pub fn request(&self, scope: PinScope) -> Result<EffectPinTicket> {
        scope.validate()?;
        let mut owner = self
            .shared
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(!owner.closed, "Output pin endpoint is closed");
        if let Some(identity) = owner.admitted {
            ensure!(
                acknowledged(self.shared.reply.load(Ordering::SeqCst), identity.serial),
                "Output pin awaits actual callback release"
            );
        }
        let serial = owner
            .serial
            .checked_add(1)
            .filter(|serial| *serial <= MAX_SERIAL)
            .context("Output pin serial exhausted")?;
        let identity = PinIdentity { serial, scope };
        self.shared
            .bank_owner
            .store(scope.bank_owner, Ordering::SeqCst);
        self.shared
            .media_token
            .store(scope.media_token, Ordering::SeqCst);
        self.shared
            .source_epoch
            .store(scope.source_epoch, Ordering::SeqCst);
        self.shared
            .sample_rate
            .store(scope.sample_rate, Ordering::SeqCst);
        self.shared
            .device_channels
            .store(scope.device_channels, Ordering::SeqCst);
        owner.serial = serial;
        owner.admitted = Some(identity);
        self.shared.request_serial.store(serial, Ordering::SeqCst);
        Ok(EffectPinTicket {
            shared: self.shared.clone(),
            identity,
        })
    }
    /// Owner-side observation retains admission until a real terminal reply.
    pub fn reap(&self) -> Option<PinIdentity> {
        let mut owner = self
            .shared
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let identity = owner.admitted?;
        if !acknowledged(self.shared.reply.load(Ordering::SeqCst), identity.serial) {
            return None;
        }
        owner.admitted.take()
    }
    pub fn current_ticket_identity(&self) -> Option<PinIdentity> {
        self.shared
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .admitted
    }
    /// Call only after the actual RenderState has returned and cannot be polled
    /// by a callback. Closure is terminal for every owner handle and ticket.
    pub fn close_on_owner(&self) {
        let mut owner = self
            .shared
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        owner.closed = true;
        self.shared.closed.store(true, Ordering::SeqCst);
        if let Some(identity) = owner.admitted.take() {
            self.shared
                .reply
                .store(reply(identity.serial, CLOSED), Ordering::SeqCst);
        }
    }
}

impl EffectPinTicket {
    pub fn identity(&self) -> PinIdentity {
        self.identity
    }
    pub fn cancel(&self) {
        // Old tickets cannot cancel a later serial, including after rejection
        // makes its admission reusable while the old ticket is still alive.
        self.shared
            .cancel_serial
            .fetch_max(self.identity.serial, Ordering::SeqCst);
    }
    pub fn cancel_requested(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
            || self.shared.cancel_serial.load(Ordering::SeqCst) >= self.identity.serial
    }
    pub fn status(&self) -> PinStatus {
        if self.shared.closed.load(Ordering::SeqCst) {
            return PinStatus::Closed;
        }
        let word = self.shared.reply.load(Ordering::SeqCst);
        if word >> REPLY_BITS == self.identity.serial {
            match word & 7 {
                REJECTED => return PinStatus::Rejected,
                RELEASED => return PinStatus::Released,
                CLOSED => return PinStatus::Closed,
                PINNED if !self.cancel_requested() => return PinStatus::Pinned,
                _ => {}
            }
        } else if self.shared.request_serial.load(Ordering::SeqCst) > self.identity.serial {
            // Request publication proves the predecessor's terminal callback
            // acknowledgement was already observed by the serialized owner.
            return PinStatus::Released;
        }
        if self.cancel_requested() {
            PinStatus::CancelRequested
        } else {
            PinStatus::Requested
        }
    }
    /// A current observation, not authority to omit validation of the actual
    /// returned graph. Hard media changes can invalidate a pin after this read.
    pub fn is_pinned_for(&self, actual: PinScope) -> bool {
        actual == self.identity.scope
            && self.shared.request_serial.load(Ordering::SeqCst) == self.identity.serial
            && self.status() == PinStatus::Pinned
            && !self.cancel_requested()
            && self.shared.reply.load(Ordering::SeqCst) == reply(self.identity.serial, PINNED)
    }
}
impl Drop for EffectPinTicket {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl EffectPinRender {
    /// Callback-only, bounded and allocation-free. A true result freezes
    /// incoming bank replacement while current media rendering continues.
    pub fn poll(&mut self, actual: PinScope) -> bool {
        if self.shared.closed.load(Ordering::SeqCst) {
            return false;
        }
        let serial = self.shared.request_serial.load(Ordering::SeqCst);
        if serial == 0 {
            return false;
        }
        if self.observed_serial != serial {
            let scope = PinScope {
                bank_owner: self.shared.bank_owner.load(Ordering::SeqCst),
                media_token: self.shared.media_token.load(Ordering::SeqCst),
                source_epoch: self.shared.source_epoch.load(Ordering::SeqCst),
                sample_rate: self.shared.sample_rate.load(Ordering::SeqCst),
                device_channels: self.shared.device_channels.load(Ordering::SeqCst),
            };
            if self.shared.request_serial.load(Ordering::SeqCst) != serial {
                return false;
            }
            self.observed_serial = serial;
            self.scope = Some(scope);
            self.phase = RenderPhase::Dormant;
        }
        // Terminal responses are never rewritten or resurrected. Once published,
        // the owner can already be filling the next request's fixed payload.
        if matches!(self.phase, RenderPhase::Rejected | RenderPhase::Released) {
            return false;
        }
        if self.shared.cancel_serial.load(Ordering::SeqCst) >= serial {
            self.phase = RenderPhase::Released;
            self.shared
                .reply
                .store(reply(serial, RELEASED), Ordering::SeqCst);
            return false;
        }
        if self.scope != Some(actual) {
            self.phase = RenderPhase::Rejected;
            self.shared
                .reply
                .store(reply(serial, REJECTED), Ordering::SeqCst);
            return false;
        }
        if !matches!(self.phase, RenderPhase::Pinned) {
            self.phase = RenderPhase::Pinned;
            self.shared
                .reply
                .store(reply(serial, PINNED), Ordering::SeqCst);
        }
        if self.shared.closed.load(Ordering::SeqCst)
            || self.shared.request_serial.load(Ordering::SeqCst) != serial
        {
            return false;
        }
        if self.shared.cancel_serial.load(Ordering::SeqCst) >= serial {
            self.phase = RenderPhase::Released;
            self.shared
                .reply
                .store(reply(serial, RELEASED), Ordering::SeqCst);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> PinScope {
        PinScope {
            bank_owner: 0,
            media_token: 0,
            source_epoch: 0,
            sample_rate: 48000,
            device_channels: 2,
        }
    }
    #[test]
    fn reservation_survives_every_handle_until_the_final_ticket_drop() {
        let baseline = DspReservation::used_bytes();
        let (owner, render) = try_new().unwrap();
        let bytes = owner.reserved_bytes();
        let another_owner = owner.clone();
        let ticket = owner.request(idle()).unwrap();
        assert_eq!(DspReservation::used_bytes(), baseline + bytes);
        drop(owner);
        drop(another_owner);
        drop(render);
        assert_eq!(DspReservation::used_bytes(), baseline + bytes);
        drop(ticket);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
    #[test]
    fn quota_exhaustion_rejects_before_creating_shared_pin_storage() {
        let baseline = DspReservation::used_bytes();
        let remaining = super::super::processing::DSP_QUOTA_BYTES - baseline;
        let held = DspReservation::new(remaining).unwrap();
        assert!(try_new().is_err());
        assert_eq!(
            DspReservation::used_bytes(),
            super::super::processing::DSP_QUOTA_BYTES
        );
        drop(held);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
    #[test]
    fn exhausted_serial_rejects_without_changing_an_acknowledged_packet() {
        let (owner, mut render) = try_new().unwrap();
        let ticket = owner.request(idle()).unwrap();
        ticket.cancel();
        assert!(!render.poll(idle()));
        owner.shared.owner.lock().unwrap().serial = MAX_SERIAL;
        let packet = owner.shared.request_serial.load(Ordering::SeqCst);
        assert!(owner.request(idle()).is_err());
        assert_eq!(owner.shared.request_serial.load(Ordering::SeqCst), packet);
        assert_eq!(ticket.status(), PinStatus::Released);
        owner.close_on_owner();
    }
}
