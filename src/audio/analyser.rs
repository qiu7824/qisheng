//! Fixed-storage LX pre-EQ spectrum kernel.
//!
//! Frequency calculation follows Web Audio Recommendation 2021-06-17 §§1.8.3,
//! 1.8.5–1.8.6 and 4.5. Native differences are explicit: an absent new PCM frame
//! leaves the snapshot unchanged; silence uses finite −1000 dB for interchange;
//! channel layouts outside mono/stereo/quad/5.1 use a labeled native average.
//! The caller owns cadence, placement, pause/stall policy and snapshot handoff.
//! Prepare and destroy this owner away from the real-time callback.

use super::{
    pcm::{MAX_CHANNELS, MAX_CLOCK_FRAMES, PcmFormat},
    processing::DspReservation,
};
use anyhow::{Context, Result, ensure};
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use std::{fmt, mem::size_of, sync::Arc};

pub const ANALYSER_FFT_SIZE: usize = 256;
pub const ANALYSER_BINS: usize = ANALYSER_FFT_SIZE / 2;
pub const ANALYSER_SMOOTHING: f64 = 0.8;
pub const ANALYSER_MIN_DB: f64 = -100.0;
pub const ANALYSER_MAX_DB: f64 = -30.0;
pub const ANALYSER_SILENT_DB: f64 = -1000.0;
const FFT_PLAN_AND_PREPARATION_BYTES: usize = 256 * 1024;
const MAX_SCRATCH_COMPLEX: usize = 6 * ANALYSER_FFT_SIZE;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct AnalysisStamp {
    pub token: u64,
    pub source_epoch: u64,
    pub media_interval_end: u64,
    /// Fractional source-frame end coordinate in Q0.32.
    pub media_fraction: u32,
}
impl AnalysisStamp {
    fn valid(self) -> bool {
        self.token != 0
            && self.source_epoch != 0
            && (self.media_interval_end < MAX_CLOCK_FRAMES
                || self.media_interval_end == MAX_CLOCK_FRAMES && self.media_fraction == 0)
    }
    fn coordinate(self) -> (u64, u32) {
        (self.media_interval_end, self.media_fraction)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum AnalysisChannelMix {
    Speakers,
    /// Arithmetic mean for native 3/5/7/8-channel PCM. This is not a standardized
    /// Web Audio speaker-layout conversion.
    NativeAverage,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct AnalysisFrame {
    pub sequence: u64,
    pub stamp: AnalysisStamp,
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_mix: AnalysisChannelMix,
    pub fft_size: u16,
    pub valid_window_frames: u16,
    pub accepted_frames: u64,
    #[serde(serialize_with = "serialize_fixed")]
    pub bins: [u8; ANALYSER_BINS],
    /// Finite dB magnitudes; exact zero is represented by ANALYSER_SILENT_DB.
    #[serde(serialize_with = "serialize_fixed")]
    pub decibels: [f64; ANALYSER_BINS],
    pub sanitized_input_samples: u64,
    pub sanitized_frequency_bins: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisError {
    InvalidStamp,
    WrongStream,
    BackwardMediaPosition,
    WrongChannelCount,
    StaleQuantum,
    CounterExhausted,
}
fn serialize_fixed<S: serde::Serializer, T: serde::Serialize, const N: usize>(
    value: &[T; N],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(value.as_slice(), serializer)
}
impl fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Invalid analyser operation: {self:?}")
    }
}
impl std::error::Error for AnalysisError {}

pub struct LxAnalyser {
    format: PcmFormat,
    channel_mix: AnalysisChannelMix,
    forward: Arc<dyn Fft<f64>>,
    scratch: Box<[Complex<f64>]>,
    spectrum: [Complex<f64>; ANALYSER_FFT_SIZE],
    window: [f64; ANALYSER_FFT_SIZE],
    ring: [f64; ANALYSER_FFT_SIZE],
    smoothed: [f64; ANALYSER_BINS],
    next_write: usize,
    valid_frames: usize,
    accepted_frames: u64,
    last_analysed_frames: u64,
    input_stamp: AnalysisStamp,
    last_quantum: Option<u64>,
    sanitized_input_samples: u64,
    sanitized_frequency_bins: u64,
    frame: AnalysisFrame,
    reserved_bytes: usize,
    // Plans, scratch and all inline storage are destroyed before quota release.
    _reservation: DspReservation,
}

impl LxAnalyser {
    pub fn prepare(format: PcmFormat) -> Result<Self> {
        ensure!(
            format.sample_rate > 0
                && format.sample_rate <= 384_000
                && format.channels > 0
                && format.channels as usize <= MAX_CHANNELS,
            "Unsupported analyser PCM format"
        );
        let bytes = FFT_PLAN_AND_PREPARATION_BYTES
            .checked_add(MAX_SCRATCH_COMPLEX * size_of::<Complex<f64>>())
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .context("Analyser reservation overflow")?;
        let reservation = DspReservation::new(bytes)?;
        let mut planner = FftPlanner::<f64>::new();
        let forward = planner.plan_fft_forward(ANALYSER_FFT_SIZE);
        let length = forward.get_inplace_scratch_len();
        ensure!(
            length <= MAX_SCRATCH_COMPLEX,
            "Analyser FFT scratch exceeds its reservation"
        );
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(length)
            .context("Cannot reserve analyser scratch")?;
        scratch.resize(length, Complex::new(0.0, 0.0));
        let mut window = [0.0; ANALYSER_FFT_SIZE];
        for (index, coefficient) in window.iter_mut().enumerate() {
            let angle = std::f64::consts::TAU * index as f64 / ANALYSER_FFT_SIZE as f64;
            *coefficient = 0.42 - 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos();
        }
        let channel_mix = if matches!(format.channels, 1 | 2 | 4 | 6) {
            AnalysisChannelMix::Speakers
        } else {
            AnalysisChannelMix::NativeAverage
        };
        Ok(Self {
            format,
            channel_mix,
            forward,
            scratch: scratch.into_boxed_slice(),
            spectrum: [Complex::new(0.0, 0.0); ANALYSER_FFT_SIZE],
            window,
            ring: [0.0; ANALYSER_FFT_SIZE],
            smoothed: [0.0; ANALYSER_BINS],
            next_write: 0,
            valid_frames: 0,
            accepted_frames: 0,
            last_analysed_frames: 0,
            input_stamp: AnalysisStamp::default(),
            last_quantum: None,
            sanitized_input_samples: 0,
            sanitized_frequency_bins: 0,
            frame: empty_frame(format, channel_mix, AnalysisStamp::default()),
            reserved_bytes: bytes,
            _reservation: reservation,
        })
    }

    pub fn reserved_bytes(&self) -> usize {
        self.reserved_bytes
    }
    /// Changes only the fixed speaker-mix layout; the FFT plan and scratch stay owned.
    pub(crate) fn select_channels(&mut self, channels: u16) -> Result<()> {
        ensure!(
            channels > 0 && channels as usize <= MAX_CHANNELS,
            "Invalid analyser channel layout"
        );
        self.format.channels = channels;
        self.channel_mix = if matches!(channels, 1 | 2 | 4 | 6) {
            AnalysisChannelMix::Speakers
        } else {
            AnalysisChannelMix::NativeAverage
        };
        Ok(())
    }

    /// Clears window and smoothing without replacing or dropping any heap owner.
    /// A new token/epoch or seek position is installed explicitly by the caller.
    pub fn reset(&mut self, stamp: AnalysisStamp) -> Result<AnalysisFrame, AnalysisError> {
        if !stamp.valid() {
            return Err(AnalysisError::InvalidStamp);
        }
        self.ring.fill(0.0);
        self.spectrum.fill(Complex::new(0.0, 0.0));
        self.smoothed.fill(0.0);
        self.next_write = 0;
        self.valid_frames = 0;
        self.accepted_frames = 0;
        self.last_analysed_frames = 0;
        self.input_stamp = stamp;
        self.last_quantum = None;
        self.sanitized_input_samples = 0;
        self.sanitized_frequency_bins = 0;
        self.frame = empty_frame(self.format, self.channel_mix, stamp);
        Ok(self.frame)
    }

    /// Accepts exactly one already-converted pre-EQ PCM frame. It neither reads
    /// a source nor alters PCM. Non-finite input samples become zero and count.
    pub fn push_frame(
        &mut self,
        samples: &[f32],
        stamp: AnalysisStamp,
    ) -> Result<(), AnalysisError> {
        if samples.len() != self.format.channels as usize {
            return Err(AnalysisError::WrongChannelCount);
        }
        if !stamp.valid() {
            return Err(AnalysisError::InvalidStamp);
        }
        if stamp.token != self.input_stamp.token
            || stamp.source_epoch != self.input_stamp.source_epoch
        {
            return Err(AnalysisError::WrongStream);
        }
        if stamp.coordinate() < self.input_stamp.coordinate() {
            return Err(AnalysisError::BackwardMediaPosition);
        }
        let Some(accepted) = self.accepted_frames.checked_add(1) else {
            return Err(AnalysisError::CounterExhausted);
        };
        let mut input = [0.0; MAX_CHANNELS];
        for (destination, sample) in input.iter_mut().zip(samples) {
            if sample.is_finite() {
                *destination = f64::from(*sample);
            } else {
                self.sanitized_input_samples = self.sanitized_input_samples.saturating_add(1);
            }
        }
        let mono = match self.format.channels {
            1 => input[0],
            2 => 0.5 * (input[0] + input[1]),
            4 => 0.25 * (input[0] + input[1] + input[2] + input[3]),
            6 => {
                std::f64::consts::FRAC_1_SQRT_2 * (input[0] + input[1])
                    + input[2]
                    + 0.5 * (input[4] + input[5])
            }
            count => input[..count as usize].iter().sum::<f64>() / f64::from(count),
        };
        self.ring[self.next_write] = mono;
        self.next_write = (self.next_write + 1) % ANALYSER_FFT_SIZE;
        self.valid_frames = (self.valid_frames + 1).min(ANALYSER_FFT_SIZE);
        self.accepted_frames = accepted;
        self.input_stamp = stamp;
        Ok(())
    }

    /// Computes once per explicitly supplied native analysis quantum. A repeated
    /// quantum returns its cached data even if more PCM arrived. Without newly
    /// accepted PCM it returns the coherent old frame and applies no smoothing.
    pub fn analyse(&mut self, quantum: u64) -> Result<&AnalysisFrame, AnalysisError> {
        if self.last_quantum.is_some_and(|previous| quantum < previous) {
            return Err(AnalysisError::StaleQuantum);
        }
        if self.last_quantum == Some(quantum) {
            return Ok(&self.frame);
        }
        if self.accepted_frames == self.last_analysed_frames {
            self.last_quantum = Some(quantum);
            return Ok(&self.frame);
        }
        let Some(sequence) = self.frame.sequence.checked_add(1) else {
            return Err(AnalysisError::CounterExhausted);
        };
        self.last_quantum = Some(quantum);
        for index in 0..ANALYSER_FFT_SIZE {
            let sample = self.ring[(self.next_write + index) % ANALYSER_FFT_SIZE];
            self.spectrum[index] = Complex::new(sample * self.window[index], 0.0);
        }
        self.forward
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for index in 0..ANALYSER_BINS {
            let value = self.spectrum[index].norm() / ANALYSER_FFT_SIZE as f64;
            let magnitude = if value.is_finite() {
                value
            } else {
                self.sanitized_frequency_bins = self.sanitized_frequency_bins.saturating_add(1);
                0.0
            };
            let smoothed =
                ANALYSER_SMOOTHING * self.smoothed[index] + (1.0 - ANALYSER_SMOOTHING) * magnitude;
            self.smoothed[index] = smoothed;
            let decibels = if smoothed > 0.0 {
                (20.0 * smoothed.log10()).max(ANALYSER_SILENT_DB)
            } else {
                ANALYSER_SILENT_DB
            };
            self.frame.decibels[index] = decibels;
            self.frame.bins[index] = (((decibels - ANALYSER_MIN_DB) * 255.0
                / (ANALYSER_MAX_DB - ANALYSER_MIN_DB))
                .floor()
                .clamp(0.0, 255.0)) as u8;
        }
        self.last_analysed_frames = self.accepted_frames;
        self.frame.sequence = sequence;
        self.frame.stamp = self.input_stamp;
        self.frame.valid_window_frames = self.valid_frames as u16;
        self.frame.accepted_frames = self.accepted_frames;
        self.frame.sanitized_input_samples = self.sanitized_input_samples;
        self.frame.sanitized_frequency_bins = self.sanitized_frequency_bins;
        Ok(&self.frame)
    }

    /// Copies fixed inline data. Serialization/publication belongs to the owner.
    pub fn snapshot(&self) -> AnalysisFrame {
        self.frame
    }
}

fn empty_frame(
    format: PcmFormat,
    channel_mix: AnalysisChannelMix,
    stamp: AnalysisStamp,
) -> AnalysisFrame {
    AnalysisFrame {
        sequence: 0,
        stamp,
        sample_rate: format.sample_rate,
        channels: format.channels,
        channel_mix,
        fft_size: ANALYSER_FFT_SIZE as u16,
        valid_window_frames: 0,
        accepted_frames: 0,
        bins: [0; ANALYSER_BINS],
        decibels: [ANALYSER_SILENT_DB; ANALYSER_BINS],
        sanitized_input_samples: 0,
        sanitized_frequency_bins: 0,
    }
}
