//! Combined download lyrics with explicit, lossless output encodings.

use crate::domain::lyric::{LyricBundle, LyricOrigin};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt, sync::OnceLock};

pub const MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_DOWNLOAD_TIMESTAMP_LABELS: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DownloadLyricOptions {
    pub include_awlrc: bool,
    pub translation: bool,
    pub romanization: bool,
}
impl Default for DownloadLyricOptions {
    fn default() -> Self {
        Self {
            include_awlrc: true,
            translation: false,
            romanization: false,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LyricArtifactEncoding {
    Utf8,
    Gbk,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum LyricCompatibilityAdjustment {
    WhitespaceOnlyContainerChannelOmitted,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DownloadLyricText {
    pub text: String,
    pub has_ordinary_lyrics: bool,
    pub adjustments: Vec<LyricCompatibilityAdjustment>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LyricArtifact {
    pub encoding: LyricArtifactEncoding,
    pub bytes: Vec<u8>,
    pub bundle_sha256: String,
    pub origin: LyricOrigin,
    pub snapshot_revision: u64,
    pub options: DownloadLyricOptions,
    pub has_ordinary_lyrics: bool,
    pub adjustments: Vec<LyricCompatibilityAdjustment>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum DownloadLyricError {
    InvalidBundle,
    TimestampIndexBound,
    OutputBound,
    UnrepresentableGbk,
}
impl fmt::Display for DownloadLyricError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidBundle => "Download lyric bundle is invalid",
            Self::TimestampIndexBound => "Download lyric timestamp index exceeds its bound",
            Self::OutputBound => "Download lyric output exceeds its bound",
            Self::UnrepresentableGbk => "Download lyrics cannot be encoded as GBK without loss",
        })
    }
}
impl std::error::Error for DownloadLyricError {}

fn fields() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^(?:\[[0-9:.]+\])+").expect("Fixed lyric field regex"))
}
fn times() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"[0-9]{1,3}(:[0-9]{1,3}){0,2}(?:\.[0-9]{1,3})")
            .expect("Fixed lyric timestamp regex")
    })
}

/// LX compares these labels textually, including zero removal in fractions.
fn normalized_label(label: &str) -> String {
    let mut result = String::with_capacity(label.len());
    let mut start = 0;
    for (index, character) in label.char_indices() {
        if matches!(character, ':' | '.') {
            result.push_str(nonzero_prefix(&label[start..index]));
            result.push(character);
            start = index + 1;
        }
    }
    result.push_str(nonzero_prefix(&label[start..]));
    result
}
fn nonzero_prefix(value: &str) -> &str {
    let trimmed = value.trim_start_matches('0');
    if trimmed.is_empty() && !value.is_empty() {
        "0"
    } else {
        trimmed
    }
}

fn ordinary_labels(ordinary: &str) -> Result<BTreeSet<String>, DownloadLyricError> {
    let mut labels = BTreeSet::new();
    for line in ordinary.split(['\r', '\n']) {
        let line = line.trim();
        let Some(prefix) = fields().find(line) else {
            continue;
        };
        if line[prefix.end()..].trim().is_empty() {
            continue;
        }
        for label in times().find_iter(prefix.as_str()) {
            labels.insert(normalized_label(label.as_str()));
            if labels.len() > MAX_DOWNLOAD_TIMESTAMP_LABELS {
                return Err(DownloadLyricError::TimestampIndexBound);
            }
        }
    }
    Ok(labels)
}
fn filtered_extended(
    extended: &str,
    labels: &BTreeSet<String>,
) -> Result<String, DownloadLyricError> {
    let mut lines = Vec::new();
    let mut timestamp_count = 0usize;
    for line in extended.split(['\r', '\n']) {
        let line = line.trim();
        let Some(prefix) = fields().find(line) else {
            continue;
        };
        let text = line[prefix.end()..].trim();
        if text.is_empty() {
            continue;
        }
        let original: Vec<_> = times()
            .find_iter(prefix.as_str())
            .map(|label| label.as_str())
            .collect();
        timestamp_count = timestamp_count.saturating_add(original.len());
        if timestamp_count > MAX_DOWNLOAD_TIMESTAMP_LABELS {
            return Err(DownloadLyricError::TimestampIndexBound);
        }
        if original.is_empty() {
            continue;
        }
        let matching: Vec<_> = original
            .iter()
            .copied()
            .filter(|label| labels.contains(&normalized_label(label)))
            .collect();
        if matching.len() == original.len() {
            lines.push(line.to_owned());
        } else if !matching.is_empty() {
            lines.push(format!("[{}]{text}", matching.join("][")));
        }
    }
    Ok(lines.join("\n"))
}
fn append_block(result: &mut String, block: &str) {
    *result = result.trim().to_owned();
    result.push_str("\n\n");
    result.push_str(block);
    result.push('\n');
}

pub fn build_download_lyrics(
    bundle: &LyricBundle,
    options: DownloadLyricOptions,
) -> Result<DownloadLyricText, DownloadLyricError> {
    bundle
        .validate()
        .map_err(|_| DownloadLyricError::InvalidBundle)?;
    let ordinary = bundle.lyric.text().unwrap_or("");
    let has_ordinary_lyrics = !ordinary.trim().is_empty();
    let extended_present = [&bundle.tlyric, &bundle.rlyric, &bundle.lxlyric]
        .iter()
        .any(|field| field.text().is_some_and(|text| !text.is_empty()));
    let mut text = ordinary.to_owned();
    let mut adjustments = Vec::new();
    if extended_present {
        let labels = ordinary_labels(ordinary)?;
        if options.translation
            && let Some(translation) = bundle.tlyric.text().filter(|text| !text.is_empty())
        {
            append_block(&mut text, &filtered_extended(translation, &labels)?);
        }
        if options.romanization
            && let Some(romanization) = bundle.rlyric.text().filter(|text| !text.is_empty())
        {
            append_block(&mut text, &filtered_extended(romanization, &labels)?);
        }
        if options.include_awlrc {
            let mut channels = Vec::new();
            for (name, field) in [
                ("lrc", &bundle.lyric),
                ("tlrc", &bundle.tlyric),
                ("rlrc", &bundle.rlyric),
                ("awlrc", &bundle.lxlyric),
            ] {
                if let Some(raw) = field.text().filter(|text| !text.is_empty()) {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        if !adjustments.contains(
                            &LyricCompatibilityAdjustment::WhitespaceOnlyContainerChannelOmitted,
                        ) {
                            adjustments.push(
                                LyricCompatibilityAdjustment::WhitespaceOnlyContainerChannelOmitted,
                            );
                        }
                        continue;
                    }
                    channels.push(format!("{name}:{}", STANDARD.encode(trimmed.as_bytes())));
                }
            }
            if !channels.is_empty() {
                append_block(&mut text, &format!("[awlrc:{}]", channels.join(",")));
            }
        }
    }
    if text.len() > MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES {
        return Err(DownloadLyricError::OutputBound);
    }
    Ok(DownloadLyricText {
        text,
        has_ordinary_lyrics,
        adjustments,
    })
}

/// UTF-8 includes its BOM; GBK never receives a UTF-8 or invented BOM.
pub fn encode_lyric_artifact(
    bundle: &LyricBundle,
    options: DownloadLyricOptions,
    encoding: LyricArtifactEncoding,
    origin: LyricOrigin,
    snapshot_revision: u64,
) -> Result<LyricArtifact, DownloadLyricError> {
    let built = build_download_lyrics(bundle, options)?;
    let bytes = match encoding {
        LyricArtifactEncoding::Utf8 => {
            let mut bytes = Vec::with_capacity(built.text.len() + 3);
            bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
            bytes.extend_from_slice(built.text.as_bytes());
            bytes
        }
        LyricArtifactEncoding::Gbk => {
            let (bytes, _, lost) = encoding_rs::GBK.encode(&built.text);
            if lost {
                return Err(DownloadLyricError::UnrepresentableGbk);
            }
            bytes.into_owned()
        }
    };
    if bytes.len() > MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES + 3 {
        return Err(DownloadLyricError::OutputBound);
    }
    let snapshot = serde_json::to_vec(bundle).map_err(|_| DownloadLyricError::InvalidBundle)?;
    Ok(LyricArtifact {
        encoding,
        bytes,
        bundle_sha256: format!("{:x}", Sha256::digest(snapshot)),
        origin,
        snapshot_revision,
        options,
        has_ordinary_lyrics: built.has_ordinary_lyrics,
        adjustments: built.adjustments,
    })
}
