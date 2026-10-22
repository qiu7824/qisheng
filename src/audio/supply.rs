//! Read-only counters from the current foreground decoder and encoded stream.

use super::{
    cache::CacheStats,
    decode::{DecoderSnapshot, DecoderStatus},
    pcm::PcmFormat,
    transport::StreamStatus,
};
use crate::domain::playback_processing::PlaybackProcessingSettings;
use serde::Serialize;

#[cfg(test)]
mod feed_tests;

/// Counters are cumulative for the actual decoder owner, including earlier
/// seek epochs. Compare deltas only within the same generation, stream serial
/// and source epoch.
#[derive(Clone, Debug, Serialize)]
pub struct AudioSupplySnapshot {
    pub generation: u64,
    pub source_serial: u64,
    pub track_id: Option<String>,
    pub source_epoch: u64,
    pub decoder_format: Option<PcmFormat>,
    pub decoder_status: DecoderStatus,
    pub decoder_worker_finished: bool,
    pub produced_pcm_frames: u64,
    pub consumed_pcm_frames: u64,
    pub underflow_pcm_frames: u64,
    pub queued_pcm_frames: usize,
    pub capacity_pcm_frames: usize,
    pub pcm_eof: bool,
    pub pcm_drained: bool,
    pub presented_processing_sequence: u64,
    pub presented_processing: PlaybackProcessingSettings,
    pub received_encoded_bytes: u64,
    pub encoded_downloaded_bytes: u64,
    pub encoded_budget_bytes: Option<u64>,
    pub encoded_inflight_bytes: u64,
    pub budget_blocked: bool,
    pub remote_range_supported: bool,
    pub cache_random_access: bool,
    pub transport_eof: bool,
    pub encoded_cursor_bytes: u64,
    pub wanted_encoded_offset: Option<u64>,
    pub published_blocks: u64,
    pub last_publication_micros: u64,
    pub max_publication_micros: u64,
    pub completed_range_loads: u64,
    pub last_range_load_micros: u64,
    pub max_range_load_micros: u64,
    pub cache: CacheStats,
}

impl AudioSupplySnapshot {
    pub(crate) fn observe(
        generation: u64,
        track_id: Option<String>,
        decoded: &DecoderSnapshot,
        stream: &StreamStatus,
    ) -> Self {
        Self {
            generation,
            source_serial: stream.source_serial,
            track_id,
            source_epoch: decoded.pcm.epoch,
            decoder_format: decoded.format,
            decoder_status: decoded.status,
            decoder_worker_finished: decoded.worker_finished,
            produced_pcm_frames: decoded.pcm.produced_frames,
            consumed_pcm_frames: decoded.pcm.consumed_frames,
            underflow_pcm_frames: decoded.pcm.underflow_frames,
            queued_pcm_frames: decoded.pcm.queued_frames,
            capacity_pcm_frames: decoded.pcm.capacity_frames,
            pcm_eof: decoded.pcm.normal_eof,
            pcm_drained: decoded.pcm.drained,
            presented_processing_sequence: decoded.pcm.processing_sequence,
            presented_processing: decoded.pcm.processing_settings,
            received_encoded_bytes: stream.downloaded_bytes,
            encoded_downloaded_bytes: stream.encoded_downloaded_bytes,
            encoded_budget_bytes: stream.encoded_budget_bytes,
            encoded_inflight_bytes: stream.encoded_inflight_bytes,
            budget_blocked: stream.budget_blocked,
            remote_range_supported: stream.range_supported,
            cache_random_access: stream.cache_random_access,
            transport_eof: stream.eof,
            encoded_cursor_bytes: stream.cursor_bytes,
            wanted_encoded_offset: stream.wanted_byte_offset,
            published_blocks: stream.published_blocks,
            last_publication_micros: stream.last_publication_micros,
            max_publication_micros: stream.max_publication_micros,
            completed_range_loads: stream.completed_range_loads,
            last_range_load_micros: stream.last_range_load_micros,
            max_range_load_micros: stream.max_range_load_micros,
            cache: stream.cache,
        }
    }
}
