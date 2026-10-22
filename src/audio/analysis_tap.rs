//! One render-owner spectrum tap and bounded atomic observation handoff.
use super::{
    analyser::{ANALYSER_BINS, AnalysisChannelMix, AnalysisFrame, AnalysisStamp, LxAnalyser},
    pcm::{PcmFormat, ProcessingStamp},
};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const WORDS: usize = 12 + ANALYSER_BINS * 2;
pub(crate) struct AnalysisMailbox {
    version: AtomicU64,
    valid: AtomicBool,
    configuration: AtomicU64,
    words: [AtomicU64; WORDS],
}
impl AnalysisMailbox {
    pub(crate) fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            valid: AtomicBool::new(false),
            configuration: AtomicU64::new(0),
            words: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    /// Called by the unique callback or after that callback owner has returned.
    pub(crate) fn clear(&self) {
        let previous = self.version.load(Ordering::SeqCst);
        let Some(next) = previous.checked_add(2) else {
            self.valid.store(false, Ordering::SeqCst);
            return;
        };
        self.version.store(previous + 1, Ordering::SeqCst);
        self.valid.store(false, Ordering::SeqCst);
        self.version.store(next, Ordering::SeqCst);
    }
    fn publish(&self, frame: AnalysisFrame, configuration: u64) {
        let previous = self.version.load(Ordering::SeqCst);
        let Some(next) = previous.checked_add(2) else {
            return;
        };
        let mut words = [0u64; WORDS];
        words[..12].copy_from_slice(&[
            frame.sequence,
            frame.stamp.token,
            frame.stamp.source_epoch,
            frame.stamp.media_interval_end,
            u64::from(frame.stamp.media_fraction),
            u64::from(frame.sample_rate),
            u64::from(frame.channels),
            u64::from(matches!(
                frame.channel_mix,
                AnalysisChannelMix::NativeAverage
            )),
            u64::from(frame.valid_window_frames),
            frame.accepted_frames,
            frame.sanitized_input_samples,
            frame.sanitized_frequency_bins,
        ]);
        for index in 0..ANALYSER_BINS {
            words[12 + index] = u64::from(frame.bins[index]);
            words[12 + ANALYSER_BINS + index] = frame.decibels[index].to_bits();
        }
        self.version.store(previous + 1, Ordering::SeqCst);
        for (target, word) in self.words.iter().zip(words) {
            target.store(word, Ordering::SeqCst);
        }
        self.valid.store(true, Ordering::SeqCst);
        self.configuration.store(configuration, Ordering::SeqCst);
        self.version.store(next, Ordering::SeqCst);
    }
    pub(crate) fn read(&self) -> Option<(u64, AnalysisFrame)> {
        for _ in 0..4 {
            let version = self.version.load(Ordering::SeqCst);
            if version == 0 || version & 1 != 0 {
                continue;
            }
            let words: [u64; WORDS] =
                std::array::from_fn(|index| self.words[index].load(Ordering::SeqCst));
            let valid = self.valid.load(Ordering::SeqCst);
            let configuration = self.configuration.load(Ordering::SeqCst);
            if version != self.version.load(Ordering::SeqCst) {
                continue;
            }
            if !valid {
                return None;
            }
            return Some((
                configuration,
                AnalysisFrame {
                    sequence: words[0],
                    stamp: AnalysisStamp {
                        token: words[1],
                        source_epoch: words[2],
                        media_interval_end: words[3],
                        media_fraction: words[4] as u32,
                    },
                    sample_rate: words[5] as u32,
                    channels: words[6] as u16,
                    channel_mix: if words[7] == 0 {
                        AnalysisChannelMix::Speakers
                    } else {
                        AnalysisChannelMix::NativeAverage
                    },
                    fft_size: 256,
                    valid_window_frames: words[8] as u16,
                    accepted_frames: words[9],
                    sanitized_input_samples: words[10],
                    sanitized_frequency_bins: words[11],
                    bins: std::array::from_fn(|index| words[12 + index] as u8),
                    decibels: std::array::from_fn(|index| {
                        f64::from_bits(words[12 + ANALYSER_BINS + index])
                    }),
                },
            ));
        }
        None
    }
}
pub(crate) struct AnalysisTap {
    kernel: LxAnalyser,
    stream: Option<(u64, u64, usize, u64)>,
    due_frames: u32,
    cadence_frames: u32,
    quantum: u64,
}
impl AnalysisTap {
    /// Preparation and owner replacement occur outside device callbacks.
    pub(crate) fn prepare(format: PcmFormat) -> Result<Self> {
        Ok(Self {
            kernel: LxAnalyser::prepare(format)?,
            stream: None,
            due_frames: 0,
            cadence_frames: format.sample_rate.div_ceil(30),
            quantum: 0,
        })
    }
    pub(crate) fn invalidate(&mut self) {
        self.stream = None;
        self.due_frames = 0;
    }
    pub(crate) fn push(
        &mut self,
        samples: &[f32],
        token: u64,
        epoch: u64,
        stamp: ProcessingStamp,
        mailbox: &AnalysisMailbox,
        configuration: u64,
    ) {
        let stamp = AnalysisStamp {
            token,
            source_epoch: epoch,
            media_interval_end: stamp.end_frame,
            media_fraction: stamp.fraction,
        };
        if self.stream != Some((token, epoch, samples.len(), configuration)) {
            if self.kernel.select_channels(samples.len() as u16).is_err() {
                return;
            }
            if let Ok(empty) = self.kernel.reset(stamp) {
                mailbox.publish(empty, configuration);
            } else {
                return;
            }
            self.stream = Some((token, epoch, samples.len(), configuration));
            self.due_frames = 0;
        }
        if self.kernel.push_frame(samples, stamp).is_err() {
            self.invalidate();
            return;
        }
        self.due_frames = self.due_frames.saturating_add(1);
        if self.due_frames >= self.cadence_frames {
            self.due_frames = 0;
            if let Some(quantum) = self.quantum.checked_add(1) {
                self.quantum = quantum;
            } else {
                return;
            }
            if let Ok(frame) = self.kernel.analyse(self.quantum) {
                mailbox.publish(*frame, configuration);
            }
        }
    }
}
