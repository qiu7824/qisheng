use super::parser::LyricDocument;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClockStatus {
    Idle,
    Resolving,
    Buffering,
    Playing,
    Paused,
    Ended,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AudioClockSnapshot {
    pub generation: u64,
    /// Media position, already accounting for playback rate.
    pub position_ms: i64,
    pub status: ClockStatus,
    pub rate: f64,
    pub seek_seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LyricFrame {
    pub generation: u64,
    pub seek_seq: u64,
    pub line_index: Option<usize>,
    pub word_index: Option<usize>,
    pub word_progress: f64,
    pub line_progress: f64,
    pub line_text: String,
    pub translation: Vec<String>,
    pub romanization: Vec<String>,
    pub extended: Vec<String>,
}

/// No wall-clock timers. Every update is derived from the engine's audio clock.
pub struct LyricTimeline {
    document: LyricDocument,
    generation: u64,
    user_offset_ms: i64,
    line_compensation_ms: i64,
    last_clock: Option<AudioClockSnapshot>,
}

impl LyricTimeline {
    pub fn new(document: LyricDocument, generation: u64) -> Self {
        Self {
            document,
            generation,
            user_offset_ms: 0,
            line_compensation_ms: 60,
            last_clock: None,
        }
    }

    pub fn set_document(&mut self, document: LyricDocument, generation: u64) {
        self.document = document;
        self.generation = generation;
        self.last_clock = None;
    }

    pub fn set_offset(&mut self, offset_ms: i64) {
        self.user_offset_ms = offset_ms;
    }

    pub fn set_line_compensation(&mut self, offset_ms: i64) {
        self.line_compensation_ms = offset_ms;
    }

    pub fn document(&self) -> &LyricDocument {
        &self.document
    }

    pub fn update(&mut self, clock: &AudioClockSnapshot) -> Option<LyricFrame> {
        if clock.generation != self.generation || !clock.rate.is_finite() || clock.rate <= 0.0 {
            return None;
        }
        if matches!(
            clock.status,
            ClockStatus::Idle | ClockStatus::Resolving | ClockStatus::Error
        ) {
            self.last_clock = None;
            return None;
        }
        let mut effective = clock.clone();
        if self
            .last_clock
            .as_ref()
            .is_some_and(|previous| clock.seek_seq < previous.seek_seq)
        {
            return None;
        }
        if matches!(clock.status, ClockStatus::Paused | ClockStatus::Buffering)
            && let Some(previous) = &self.last_clock
            && previous.status == clock.status
            && previous.seek_seq == clock.seek_seq
        {
            effective.position_ms = previous.position_ms;
        }
        self.last_clock = Some(effective.clone());
        // LX applies offset before rate. Position is already media time and is not multiplied again.
        let compensation = if self.document.line_mode {
            self.line_compensation_ms
        } else {
            0
        };
        let offset = self
            .document
            .offset_ms
            .saturating_add(self.user_offset_ms)
            .saturating_add(compensation);
        let current = effective.position_ms as f64 + offset as f64 * effective.rate;
        if !current.is_finite() {
            return None;
        }
        let index = self
            .document
            .lines
            .partition_point(|line| line.start_ms as f64 <= current)
            .checked_sub(1);
        let mut frame = LyricFrame {
            generation: clock.generation,
            seek_seq: clock.seek_seq,
            line_index: index,
            word_index: None,
            word_progress: 0.0,
            line_progress: 0.0,
            line_text: String::new(),
            translation: Vec::new(),
            romanization: Vec::new(),
            extended: Vec::new(),
        };
        if let Some(index) = index {
            let line = &self.document.lines[index];
            frame.line_text = line.text.clone();
            frame.translation = line.translation.clone();
            frame.romanization = line.romanization.clone();
            frame.extended = line.extended.clone();
            let relative = current - line.start_ms as f64;
            // Word order is textual; overlapping timing selects the last begun word.
            frame.word_index = line
                .words
                .iter()
                .enumerate()
                .filter(|(_, word)| word.start_ms as f64 <= relative)
                .map(|(index, _)| index)
                .next_back();
            if let Some(word_index) = frame.word_index {
                let word = &line.words[word_index];
                frame.word_progress = if word.duration_ms == 0 {
                    1.0
                } else {
                    ((relative - word.start_ms as f64) / word.duration_ms as f64).clamp(0.0, 1.0)
                };
            }
            let end = self
                .document
                .lines
                .get(index + 1)
                .map(|next| next.start_ms)
                .or_else(|| {
                    line.words
                        .iter()
                        .map(|word| {
                            line.start_ms
                                .saturating_add(word.start_ms)
                                .saturating_add(word.duration_ms.min(i64::MAX as u64) as i64)
                        })
                        .max()
                });
            if let Some(end) = end {
                let duration = end.saturating_sub(line.start_ms);
                if duration > 0 {
                    frame.line_progress = (relative / duration as f64).clamp(0.0, 1.0);
                }
            }
        }
        Some(frame)
    }
}
