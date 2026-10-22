//! Bounded test-only publication stage observations, outside audio callbacks.
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

static NEXT_CACHE: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Copy)]
pub(crate) enum PublicationStage {
    Queue,
    DirectoryPublish,
    PersistBlock,
    RegisterBefore,
    RegisterAfter,
    PublicationAudits,
    ReservationSnapshot,
    OwnershipHeader,
    PayloadWriteSync,
}
impl PublicationStage {
    fn index(self) -> usize {
        self as usize
    }
    fn name(self) -> &'static str {
        match self {
            Self::Queue => "enqueue_dequeue",
            Self::DirectoryPublish => "directory_publish",
            Self::PersistBlock => "persist_block",
            Self::RegisterBefore => "register_before",
            Self::RegisterAfter => "register_after",
            Self::PublicationAudits => "publication_audits",
            Self::ReservationSnapshot => "reservation_snapshot",
            Self::OwnershipHeader => "ownership_header_sync",
            Self::PayloadWriteSync => "payload_write_sync",
        }
    }
}
#[derive(Clone, Copy, Default)]
struct Timing {
    count: u64,
    last_micros: u64,
    max_micros: u64,
}
pub(super) struct PublicationDiagnostics {
    pub(super) unwind_after_header: AtomicBool,
    id: u64,
    timing: Mutex<[Timing; 9]>,
}
impl Default for PublicationDiagnostics {
    fn default() -> Self {
        Self {
            unwind_after_header: AtomicBool::new(false),
            id: NEXT_CACHE.fetch_add(1, Ordering::Relaxed),
            timing: Mutex::new([Timing::default(); 9]),
        }
    }
}
impl PublicationDiagnostics {
    pub(super) fn id(&self) -> u64 {
        self.id
    }
    pub(super) fn record(
        &self,
        stage: PublicationStage,
        serial: Option<u64>,
        source_serial: Option<u64>,
        offset: u64,
        elapsed: Duration,
    ) {
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        let timing = {
            let mut state = self
                .timing
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let timing = &mut state[stage.index()];
            timing.count = timing.count.saturating_add(1);
            timing.last_micros = micros;
            timing.max_micros = timing.max_micros.max(micros);
            *timing
        };
        // Keep the entire per-cache trace bounded even for long-stream tests.
        if timing.count <= 32 {
            eprintln!(
                "cache publication stage: cache_id={} stage={} serial={serial:?} source_serial={source_serial:?} offset={offset} count={} last_us={} max_us={}",
                self.id,
                stage.name(),
                timing.count,
                timing.last_micros,
                timing.max_micros
            );
        }
    }
    pub(super) fn span(
        &self,
        stage: PublicationStage,
        serial: Option<u64>,
        source_serial: Option<u64>,
        offset: u64,
    ) -> PublicationSpan<'_> {
        PublicationSpan {
            diagnostic: self,
            stage,
            serial,
            source_serial,
            offset,
            started: Instant::now(),
        }
    }
}
pub(super) struct PublicationSpan<'a> {
    diagnostic: &'a PublicationDiagnostics,
    stage: PublicationStage,
    serial: Option<u64>,
    source_serial: Option<u64>,
    offset: u64,
    started: Instant,
}
impl Drop for PublicationSpan<'_> {
    fn drop(&mut self) {
        self.diagnostic.record(
            self.stage,
            self.serial,
            self.source_serial,
            self.offset,
            self.started.elapsed(),
        );
    }
}

pub(super) fn source_serial(object: &str) -> Option<u64> {
    // Transport's object is its 64-hex cache identity followed by its actual
    // stream serial. Neither the identity nor any caller-supplied string logs.
    let (key, serial) = object.rsplit_once('-')?;
    if key.len() == 64 && key.bytes().all(|value| value.is_ascii_hexdigit()) {
        serial.parse().ok()
    } else {
        None
    }
}
