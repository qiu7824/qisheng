//! Fixed dry/wet alignment for one prepared convolution bank.
use super::{
    convolution::{
        CONVOLUTION_PARTITION, ConvolutionError, ConvolutionFrame, ConvolutionStamp,
        PreparedConvolution,
    },
    pcm::{MAX_CHANNELS, PcmFormat},
    processing::DspReservation,
};
use anyhow::{Result, ensure};
use std::mem::size_of;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixedEffectFrame {
    pub samples: [f64; MAX_CHANNELS],
    pub stamp: Option<ConvolutionStamp>,
    pub valid: bool,
    pub tail: bool,
}
impl MixedEffectFrame {
    const PADDING: Self = Self {
        samples: [0.0; MAX_CHANNELS],
        stamp: None,
        valid: false,
        tail: false,
    };
}
#[derive(Clone, Copy)]
struct DryFrame {
    samples: [f64; MAX_CHANNELS],
    stamp: Option<ConvolutionStamp>,
    valid: bool,
}
impl DryFrame {
    const EMPTY: Self = Self {
        samples: [0.0; MAX_CHANNELS],
        stamp: None,
        valid: false,
    };
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixError {
    WrongLayout,
    InvalidGain,
    DryWetMismatch,
    Input(ConvolutionError),
    CounterExhausted,
    EndOfInput,
}
impl std::fmt::Display for MixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid dry/wet operation: {self:?}")
    }
}
impl std::error::Error for MixError {}
pub struct PreparedEffectMix {
    format: PcmFormat,
    bank: PreparedConvolution,
    dry: Box<[DryFrame; CONVOLUTION_PARTITION]>,
    head: usize,
    main_gain: f64,
    send_gain: f64,
    accepted: u64,
    emitted: u64,
    eof: bool,
    _reservation: DspReservation,
}
impl PreparedEffectMix {
    pub fn prepare(
        format: PcmFormat,
        mut bank: PreparedConvolution,
        main_gain: f64,
        send_gain: f64,
    ) -> Result<Self> {
        ensure!(
            (1..=MAX_CHANNELS as u16).contains(&format.channels),
            "Invalid effect channel layout"
        );
        ensure!(bank.format() == format, "Dry/wet prepared formats differ");
        ensure!(
            main_gain.is_finite()
                && send_gain.is_finite()
                && (0.0..=5.0).contains(&main_gain)
                && (0.0..=5.0).contains(&send_gain),
            "Invalid dry/wet gains"
        );
        bank.tighten_input_bound((main_gain + send_gain).max(1.0) * 2.0)?;
        ensure!(
            bank.latency_frames() == CONVOLUTION_PARTITION as u32,
            "Unsupported convolution alignment"
        );
        let reservation = DspReservation::new(
            size_of::<Self>() + size_of::<[DryFrame; CONVOLUTION_PARTITION]>(),
        )?;
        Ok(Self {
            format,
            bank,
            dry: Box::new([DryFrame::EMPTY; CONVOLUTION_PARTITION]),
            head: 0,
            main_gain,
            send_gain,
            accepted: 0,
            emitted: 0,
            eof: false,
            _reservation: reservation,
        })
    }
    pub fn reset(&mut self, token: u64, epoch: u64) -> std::result::Result<(), MixError> {
        self.bank.reset(token, epoch).map_err(MixError::Input)?;
        self.dry.fill(DryFrame::EMPTY);
        self.head = 0;
        self.accepted = 0;
        self.emitted = 0;
        self.eof = false;
        Ok(())
    }
    pub fn frame(
        &mut self,
        samples: &[f64],
        stamp: ConvolutionStamp,
    ) -> std::result::Result<MixedEffectFrame, MixError> {
        if self.eof {
            return Err(MixError::EndOfInput);
        }
        if samples.len() != self.format.channels as usize {
            return Err(MixError::WrongLayout);
        }
        let next = self
            .accepted
            .checked_add(1)
            .ok_or(MixError::CounterExhausted)?;
        let wet = self.bank.frame(samples, stamp).map_err(MixError::Input)?;
        let dry = self.dry[self.head];
        let mut stored = DryFrame {
            samples: [0.0; MAX_CHANNELS],
            stamp: Some(stamp),
            valid: true,
        };
        stored.samples[..samples.len()].copy_from_slice(samples);
        self.dry[self.head] = stored;
        self.head = (self.head + 1) % CONVOLUTION_PARTITION;
        self.accepted = next;
        self.mix(dry, wet)
    }
    pub fn begin_eof(&mut self) {
        self.eof = true;
        self.bank.begin_eof();
    }
    pub fn drain(&mut self) -> std::result::Result<Option<MixedEffectFrame>, MixError> {
        if !self.eof {
            return Err(MixError::EndOfInput);
        }
        let Some(wet) = self.bank.drain() else {
            return Ok(None);
        };
        let dry = self.dry[self.head];
        self.dry[self.head] = DryFrame::EMPTY;
        self.head = (self.head + 1) % CONVOLUTION_PARTITION;
        self.mix(dry, wet).map(Some)
    }
    pub fn drained(&self) -> bool {
        self.eof && self.bank.drained()
    }
    pub fn latency_frames(&self) -> u32 {
        self.bank.latency_frames()
    }
    pub fn output_channels(&self) -> u16 {
        self.format.channels.max(2)
    }
    pub fn format(&self) -> PcmFormat {
        self.format
    }
    pub fn reserved_bytes(&self) -> usize {
        self.bank.reserved_bytes()
            + size_of::<Self>()
            + size_of::<[DryFrame; CONVOLUTION_PARTITION]>()
    }
    fn mix(
        &mut self,
        dry: DryFrame,
        wet: ConvolutionFrame,
    ) -> std::result::Result<MixedEffectFrame, MixError> {
        if !wet.valid {
            if dry.valid {
                return Err(MixError::DryWetMismatch);
            }
            return Ok(MixedEffectFrame::PADDING);
        }
        if !wet.tail && (!dry.valid || dry.stamp != wet.stamp) {
            return Err(MixError::DryWetMismatch);
        }
        if wet.tail && dry.valid {
            return Err(MixError::DryWetMismatch);
        }
        let mut samples = dry.samples.map(|sample| sample * self.main_gain);
        if self.format.channels == 1 {
            samples[1] = samples[0];
        }
        samples[0] += wet.samples[0] * self.send_gain;
        samples[1] += wet.samples[1] * self.send_gain;
        self.emitted = self
            .emitted
            .checked_add(1)
            .ok_or(MixError::CounterExhausted)?;
        Ok(MixedEffectFrame {
            samples,
            stamp: wet.stamp,
            valid: true,
            tail: wet.tail,
        })
    }
}
