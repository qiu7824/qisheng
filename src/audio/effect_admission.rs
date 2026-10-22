//! Latest-request admission while real candidate/retiring owners remain tracked.
use super::pcm::PcmFormat;
use crate::domain::audio_effects::EffectsSettings;
use anyhow::{Context, Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub struct EffectPreparationTicket {
    sequence: u64,
    stream_epoch: u64,
    format: PcmFormat,
    cancelled: Arc<AtomicBool>,
}
impl EffectPreparationTicket {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn stream_epoch(&self) -> u64 {
        self.stream_epoch
    }
    pub fn format(&self) -> PcmFormat {
        self.format
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
#[derive(Default)]
pub struct EffectAdmission {
    sequence: u64,
    desired: EffectsSettings,
    candidate: Option<EffectPreparationTicket>,
    active_sequence: Option<u64>,
    retiring_sequence: Option<u64>,
    closing: bool,
}
impl EffectAdmission {
    /// A newer request cancels a candidate, but does not release its resources.
    pub fn request(&mut self, settings: EffectsSettings) -> Result<u64> {
        ensure!(!self.closing, "Effects admission is closing");
        settings.validate()?;
        let sequence = self
            .sequence
            .checked_add(1)
            .context("Effect request sequence exhausted")?;
        if let Some(candidate) = &self.candidate {
            candidate.cancelled.store(true, Ordering::Release);
        }
        self.sequence = sequence;
        self.desired = settings;
        Ok(sequence)
    }
    pub fn desired(&self) -> &EffectsSettings {
        &self.desired
    }
    pub fn requested_sequence(&self) -> u64 {
        self.sequence
    }
    pub fn active_sequence(&self) -> Option<u64> {
        self.active_sequence
    }
    /// Total active/candidate/retiring banks never exceeds two. Busy is retained
    /// until the owning worker reports actual drop, not merely cancellation.
    pub fn begin_preparation(
        &mut self,
        stream_epoch: u64,
        format: PcmFormat,
    ) -> Result<EffectPreparationTicket> {
        ensure!(
            !self.closing && self.sequence > 0 && stream_epoch > 0,
            "Invalid effect preparation lifetime"
        );
        ensure!(
            (8000..=384000).contains(&format.sample_rate) && (1..=8).contains(&format.channels),
            "Unsupported effect format"
        );
        ensure!(
            self.candidate.is_none() && self.retiring_sequence.is_none(),
            "An effect owner is still preparing or retiring"
        );
        let ticket = EffectPreparationTicket {
            sequence: self.sequence,
            stream_epoch,
            format,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.candidate = Some(ticket.clone());
        Ok(ticket)
    }
    pub fn can_install(
        &self,
        ticket: &EffectPreparationTicket,
        stream_epoch: u64,
        format: PcmFormat,
    ) -> bool {
        !self.closing
            && !ticket.cancelled()
            && ticket.sequence == self.sequence
            && ticket.stream_epoch == stream_epoch
            && ticket.format == format
            && self.retiring_sequence.is_none()
            && self
                .candidate
                .as_ref()
                .is_some_and(|actual| Arc::ptr_eq(&actual.cancelled, &ticket.cancelled))
    }
    /// Called only after the candidate's actual preparation succeeds. Old bank
    /// remains retiring until its callback handback and destructor complete.
    pub fn installed(
        &mut self,
        ticket: &EffectPreparationTicket,
        stream_epoch: u64,
        format: PcmFormat,
    ) -> Result<()> {
        ensure!(
            self.can_install(ticket, stream_epoch, format),
            "Effect candidate is stale or retired"
        );
        self.retiring_sequence = self.active_sequence.take();
        self.active_sequence = Some(ticket.sequence);
        self.candidate = None;
        Ok(())
    }
    pub fn preparation_dropped(&mut self, ticket: &EffectPreparationTicket) -> Result<()> {
        ensure!(
            self.candidate
                .as_ref()
                .is_some_and(|actual| Arc::ptr_eq(&actual.cancelled, &ticket.cancelled)),
            "Effect candidate owner does not match"
        );
        self.candidate = None;
        Ok(())
    }
    pub fn retired_dropped(&mut self, sequence: u64) -> Result<()> {
        ensure!(
            self.retiring_sequence == Some(sequence),
            "Effect retirement owner does not match"
        );
        self.retiring_sequence = None;
        Ok(())
    }
    pub fn active_dropped(&mut self, sequence: u64) -> Result<()> {
        ensure!(
            self.active_sequence == Some(sequence),
            "Active effect owner does not match"
        );
        self.active_sequence = None;
        Ok(())
    }
    pub fn is_drained(&self) -> bool {
        self.candidate.is_none()
            && self.active_sequence.is_none()
            && self.retiring_sequence.is_none()
    }
    pub fn begin_shutdown(&mut self) {
        self.closing = true;
        if let Some(candidate) = &self.candidate {
            candidate.cancelled.store(true, Ordering::Release);
        }
    }
}
