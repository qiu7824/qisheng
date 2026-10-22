use crate::domain::lyric::{LyricBundle, MAX_LYRIC_BYTES};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const PARSER_REVISION: u32 = 1;
const MAX_LINES: usize = 100_000;
const MAX_WORDS: usize = 500_000;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ParserMode {
    /// Fixed LX line-player: decimal tags, normalized labels, global millisecond mode.
    #[default]
    LxCompatible,
    /// Conventional LRC: integer tags allowed; decimal fractions are always scaled.
    Standard,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LyricWord {
    pub start_ms: i64,
    pub duration_ms: u64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LyricLine {
    pub start_ms: i64,
    pub text: String,
    pub words: Vec<LyricWord>,
    pub extended: Vec<String>,
    pub translation: Vec<String>,
    pub romanization: Vec<String>,
    /// Used for exact normalized-label association, never nearest-neighbor matching.
    pub time_label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LyricDocument {
    pub lines: Vec<LyricLine>,
    pub tags: BTreeMap<String, String>,
    pub offset_ms: i64,
    pub line_mode: bool,
    pub parser_revision: u32,
    pub mode: ParserMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    TooLarge,
    TooManyLines,
    TooManyWords,
    InvalidBundle(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("Lyric input exceeds the 2 MiB limit"),
            Self::TooManyLines => f.write_str("Lyric input contains too many timed lines"),
            Self::TooManyWords => f.write_str("Lyric input contains too many words"),
            Self::InvalidBundle(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ParseError {}

pub fn parse_lrc(text: &str) -> Result<LyricDocument, ParseError> {
    parse_with_mode(text, ParserMode::LxCompatible)
}

pub fn parse_lx(text: &str) -> Result<LyricDocument, ParseError> {
    parse_with_mode(text, ParserMode::LxCompatible)
}

pub fn parse_bundle(bundle: &LyricBundle, mode: ParserMode) -> Result<LyricDocument, ParseError> {
    bundle
        .validate()
        .map_err(|error| ParseError::InvalidBundle(error.to_string()))?;
    let main = bundle
        .lxlyric
        .text()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| bundle.lyric.text())
        .unwrap_or_default();
    let mut document = parse_with_mode(main, mode)?;
    if let Some(text) = bundle.tlyric.text() {
        attach_extended(&mut document, text, true);
    }
    if let Some(text) = bundle.rlyric.text() {
        attach_extended(&mut document, text, false);
    }
    Ok(document)
}

pub fn parse_with_mode(text: &str, mode: ParserMode) -> Result<LyricDocument, ParseError> {
    if text.len() > MAX_LYRIC_BYTES {
        return Err(ParseError::TooLarge);
    }
    let mut tags = BTreeMap::new();
    let mut lines: Vec<LyricLine> = Vec::new();
    let mut labels = BTreeMap::<String, usize>::new();
    let is_ms_time = mode == ParserMode::LxCompatible
        && text
            .split(['\r', '\n'])
            .flat_map(|line| leading_tags(line.trim()).0)
            .any(|tag| {
                valid_time_parts(tag, true).is_some_and(|(_, fraction)| fraction.len() == 3)
            });
    let mut word_count = 0usize;
    for input_line in text.split(['\r', '\n']) {
        let (time_tags, content) = leading_tags(input_line.trim().trim_start_matches('\u{feff}'));
        for tag in &time_tags {
            if let Some((key, value)) = tag.split_once(':') {
                let key = key.to_ascii_lowercase();
                if matches!(key.as_str(), "ti" | "ar" | "al" | "offset" | "by") {
                    // LX reads the first metadata tag, case-insensitively.
                    tags.entry(key).or_insert_with(|| value.to_owned());
                }
            }
        }
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        for tag in time_tags {
            let Some((start_ms, label)) = parse_time(tag, mode, is_ms_time) else {
                continue;
            };
            if let Some(&existing) = labels.get(&label) {
                lines[existing].extended.push(content.to_owned());
                continue;
            }
            if lines.len() == MAX_LINES {
                return Err(ParseError::TooManyLines);
            }
            let (display_text, words) = parse_words(content);
            word_count = word_count.saturating_add(words.len());
            if word_count > MAX_WORDS {
                return Err(ParseError::TooManyWords);
            }
            labels.insert(label.clone(), lines.len());
            lines.push(LyricLine {
                start_ms,
                text: display_text,
                words,
                extended: Vec::new(),
                translation: Vec::new(),
                romanization: Vec::new(),
                time_label: label,
            });
        }
    }
    // Stable sort preserves input order when different labels resolve to the same time.
    lines.sort_by_key(|line| line.start_ms);
    let offset_ms = tags
        .get("offset")
        .and_then(|value| parse_integer_prefix(value))
        .unwrap_or(0);
    let line_mode = lines.first().is_none_or(|line| line.words.is_empty());
    Ok(LyricDocument {
        lines,
        tags,
        offset_ms,
        line_mode,
        parser_revision: PARSER_REVISION,
        mode,
    })
}

fn parse_integer_prefix(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let prefix = text
        .char_indices()
        .take_while(|(index, character)| {
            character.is_ascii_digit() || (*index == 0 && matches!(character, '+' | '-'))
        })
        .map(|(_, character)| character)
        .collect::<String>();
    prefix.parse().ok()
}

fn leading_tags(mut text: &str) -> (Vec<&str>, &str) {
    let mut tags = Vec::new();
    while let Some(after_open) = text.strip_prefix('[') {
        let Some(end) = after_open.find(']') else {
            break;
        };
        tags.push(&after_open[..end]);
        text = &after_open[end + 1..];
    }
    (tags, text)
}

fn valid_time_parts(tag: &str, require_decimal: bool) -> Option<(Vec<&str>, &str)> {
    let (whole, fraction) = match tag.split_once('.') {
        Some((whole, fraction))
            if !fraction.is_empty()
                && fraction.len() <= 3
                && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (whole, fraction)
        }
        None if !require_decimal => (tag, ""),
        _ => return None,
    };
    let parts: Vec<_> = whole.split(':').take(4).collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts.iter().any(|part| {
            part.is_empty() || part.len() > 3 || !part.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    Some((parts, fraction))
}

fn normalized_number(value: &str) -> &str {
    let result = value.trim_start_matches('0');
    if result.is_empty() { "0" } else { result }
}

fn parse_time(tag: &str, mode: ParserMode, is_ms_time: bool) -> Option<(i64, String)> {
    let (parts, fraction) = valid_time_parts(tag, mode == ParserMode::LxCompatible)?;
    let mut seconds = 0i64;
    for part in &parts {
        seconds = seconds.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    let normalized_fraction = if fraction.is_empty() {
        ""
    } else {
        normalized_number(fraction)
    };
    let milliseconds = if fraction.is_empty() {
        0
    } else {
        let parsed: i64 = if mode == ParserMode::LxCompatible {
            normalized_fraction.parse().ok()?
        } else {
            fraction.parse().ok()?
        };
        if is_ms_time {
            parsed
        } else {
            let digits = if mode == ParserMode::LxCompatible {
                normalized_fraction.len()
            } else {
                fraction.len()
            };
            parsed.checked_mul(10i64.pow((3 - digits) as u32))?
        }
    };
    let label = parts
        .iter()
        .map(|part| normalized_number(part))
        .collect::<Vec<_>>()
        .join(":");
    let label = if mode == ParserMode::Standard {
        format!("{seconds}.{milliseconds:03}")
    } else if fraction.is_empty() {
        label
    } else {
        format!("{label}.{normalized_fraction}")
    };
    Some((seconds.checked_mul(1000)?.checked_add(milliseconds)?, label))
}

fn attach_extended(document: &mut LyricDocument, text: &str, translation: bool) {
    // Comparison is by label rather than numeric time; .5 and .500 need not match.
    let indices: BTreeMap<_, _> = document
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| (line.time_label.clone(), index))
        .collect();
    for input_line in text.split(['\r', '\n']) {
        let (tags, content) = leading_tags(input_line.trim());
        let content = content.trim();
        if content.is_empty() || content == "//" {
            continue;
        }
        for tag in tags {
            let Some((_, label)) = parse_time(tag, document.mode, false) else {
                continue;
            };
            if let Some(&index) = indices.get(&label) {
                let line = &mut document.lines[index];
                if translation {
                    line.translation.push(content.to_owned());
                } else {
                    line.romanization.push(content.to_owned());
                }
            }
        }
    }
}

fn word_marker(text: &str) -> Option<(usize, i64, u64)> {
    let body = text.strip_prefix('<')?;
    let end = body
        .as_bytes()
        .iter()
        .take(48)
        .position(|byte| *byte == b'>')?;
    let (start, duration) = body[..end].split_once(',')?;
    if start.is_empty()
        || duration.is_empty()
        || start.strip_prefix('-').unwrap_or(start).is_empty()
        || !start
            .strip_prefix('-')
            .unwrap_or(start)
            .bytes()
            .all(|b| b.is_ascii_digit())
        || !duration.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((end + 2, start.parse().ok()?, duration.parse().ok()?))
}

fn parse_words(content: &str) -> (String, Vec<LyricWord>) {
    // A malformed marker falls back to visible text; valid markers retain unicode exactly.
    let mut words: Vec<LyricWord> = Vec::new();
    let mut display = String::new();
    let mut remainder = content;
    while !remainder.is_empty() {
        if let Some((consumed, start_ms, duration_ms)) = word_marker(remainder) {
            words.push(LyricWord {
                start_ms,
                duration_ms,
                text: String::new(),
            });
            remainder = &remainder[consumed..];
        } else {
            let character = remainder.chars().next().unwrap();
            display.push(character);
            if let Some(word) = words.last_mut() {
                word.text.push(character);
            }
            remainder = &remainder[character.len_utf8()..];
        }
    }
    (display, words)
}
