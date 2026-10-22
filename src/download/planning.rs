//! Pure quality and filename planning, independent of source or file authority.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

const MAX_NAME_INPUT_BYTES: usize = 64 * 1024;
const MAX_CAPABILITY_VALUES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum DownloadQuality {
    #[serde(rename = "flac24bit")]
    Flac24Bit,
    #[serde(rename = "flac")]
    Flac,
    #[serde(rename = "wav")]
    Wav,
    #[serde(rename = "ape")]
    Ape,
    #[serde(rename = "320k")]
    K320,
    #[serde(rename = "192k")]
    K192,
    #[serde(rename = "128k")]
    K128,
}
pub const QUALITY_ORDER: [DownloadQuality; 7] = [
    DownloadQuality::Flac24Bit,
    DownloadQuality::Flac,
    DownloadQuality::Wav,
    DownloadQuality::Ape,
    DownloadQuality::K320,
    DownloadQuality::K192,
    DownloadQuality::K128,
];
impl DownloadQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flac24Bit => "flac24bit",
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Ape => "ape",
            Self::K320 => "320k",
            Self::K192 => "192k",
            Self::K128 => "128k",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Flac24Bit | Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Ape => "ape",
            _ => "mp3",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        QUALITY_ORDER
            .into_iter()
            .find(|quality| quality.as_str() == value)
    }
    fn index(self) -> usize {
        QUALITY_ORDER
            .iter()
            .position(|quality| *quality == self)
            .expect("Closed download quality")
    }
}

/// Preserves LX's fallback, including its lack of source-capability rechecking.
pub fn compatibility_quality(
    requested: DownloadQuality,
    track: &[DownloadQuality],
    source: Option<&[DownloadQuality]>,
) -> DownloadQuality {
    let Some(source) = source else {
        return DownloadQuality::K128;
    };
    let start = if source.contains(&requested) {
        requested.index()
    } else {
        source.last().map_or(6, |quality| quality.index())
    };
    QUALITY_ORDER[start..]
        .iter()
        .find(|quality| track.contains(quality))
        .copied()
        .unwrap_or(DownloadQuality::K128)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum QualityPlanError {
    UnsupportedPlatform,
    InvalidCapabilities,
    NoMusicUrlCapability,
    EmptyIntersection,
}
impl fmt::Display for QualityPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedPlatform => "Download platform is unsupported",
            Self::InvalidCapabilities => "Download quality capabilities are invalid",
            Self::NoMusicUrlCapability => "Selected source has no musicUrl capability",
            Self::EmptyIntersection => {
                "Track and source have no executable download quality intersection"
            }
        })
    }
}
impl std::error::Error for QualityPlanError {}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExecutableQualityPlan {
    pub requested: DownloadQuality,
    pub compatibility: DownloadQuality,
    pub actual: DownloadQuality,
    pub source_rechecked: bool,
}

/// Capabilities describe availability only; execution still needs source approval.
pub fn select_executable_quality(
    platform: &str,
    requested: DownloadQuality,
    track: &[DownloadQuality],
    source: &crate::source::SourceCapabilities,
) -> Result<ExecutableQualityPlan, QualityPlanError> {
    if !matches!(platform, "kw" | "kg" | "tx" | "wy" | "mg") {
        return Err(QualityPlanError::UnsupportedPlatform);
    }
    if source.actions.len() > MAX_CAPABILITY_VALUES
        || source.qualitys.len() > QUALITY_ORDER.len()
        || track.len() > QUALITY_ORDER.len()
        || source.qualitys.is_empty()
    {
        return Err(QualityPlanError::InvalidCapabilities);
    }
    if !source.actions.iter().any(|action| action == "musicUrl") {
        return Err(QualityPlanError::NoMusicUrlCapability);
    }
    let qualities: Vec<_> = source
        .qualitys
        .iter()
        .map(|quality| DownloadQuality::parse(quality).ok_or(QualityPlanError::InvalidCapabilities))
        .collect::<Result<_, _>>()?;
    if qualities.iter().copied().collect::<BTreeSet<_>>().len() != qualities.len()
        || track.iter().copied().collect::<BTreeSet<_>>().len() != track.len()
    {
        return Err(QualityPlanError::InvalidCapabilities);
    }
    let compatibility = compatibility_quality(requested, track, Some(&qualities));
    let start = if qualities.contains(&requested) {
        requested.index()
    } else {
        qualities
            .last()
            .expect("Validated nonempty source qualities")
            .index()
    };
    let actual = QUALITY_ORDER[start..]
        .iter()
        .copied()
        .find(|quality| track.contains(quality) && qualities.contains(quality))
        .ok_or(QualityPlanError::EmptyIntersection)?;
    Ok(ExecutableQualityPlan {
        requested,
        compatibility,
        actual,
        source_rechecked: actual != compatibility,
    })
}

/// Reads only normalized metadata; platform-specific hashes and IDs stay intact.
pub fn track_quality_capabilities(
    record: &crate::domain::music::TrackRecord,
) -> Vec<DownloadQuality> {
    let Some(qualities) = record.metadata.get("_qualitys").and_then(Value::as_object) else {
        return Vec::new();
    };
    QUALITY_ORDER
        .into_iter()
        .filter(|quality| qualities.get(quality.as_str()).is_some_and(json_truthy))
        .collect()
}
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilenameFormat {
    #[default]
    TitleArtist,
    ArtistTitle,
    Title,
}
impl FilenameFormat {
    pub fn lx_value(self) -> &'static str {
        match self {
            Self::TitleArtist => "歌名 - 歌手",
            Self::ArtistTitle => "歌手 - 歌名",
            Self::Title => "歌名",
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum NamingAdjustment {
    SurrogateBoundaryAvoided,
    EmptyNameFallback,
    TrailingDotOrSpaceRemoved,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FilenamePlan {
    pub basename: String,
    pub adjustments: Vec<NamingAdjustment>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum NamingError {
    InputBound,
    InvalidComponent,
    ReservedComponent,
}
impl fmt::Display for NamingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InputBound => "Download name input exceeds its bound",
            Self::InvalidComponent => "Download name is not a safe relative component",
            Self::ReservedComponent => "Download name is a reserved filesystem component",
        })
    }
}
impl std::error::Error for NamingError {}

fn check_input(value: &str) -> Result<(), NamingError> {
    if value.len() > MAX_NAME_INPUT_BYTES {
        Err(NamingError::InputBound)
    } else {
        Ok(())
    }
}
fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}
fn clip_utf16(value: &str, limit: usize, adjustments: &mut Vec<NamingAdjustment>) -> String {
    let mut units = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        if units + character.len_utf16() > limit {
            if character.len_utf16() == 2 && units < limit {
                adjustments.push(NamingAdjustment::SurrogateBoundaryAvoided);
            }
            break;
        }
        units += character.len_utf16();
        end = index + character.len_utf8();
    }
    value[..end].to_owned()
}
fn clip_singer(value: &str) -> String {
    if utf16_len(value) <= 80 || !value.contains('、') {
        return value.to_owned();
    }
    let mut names = value.split('、');
    let mut current = names.next().unwrap_or("").to_owned();
    for name in names {
        if utf16_len(&current) + utf16_len(name) > 80 {
            break;
        }
        current.push('、');
        current.push_str(name);
    }
    current
}
fn filter_filename(value: &str) -> String {
    value
        .chars()
        .filter(|character| !"\\/:*?#\"<>|".contains(*character))
        .collect()
}

/// First-only placeholder replacement and filtering occur in LX's original order.
pub fn compatibility_filename(
    title: &str,
    singer: &str,
    format: &str,
    quality: DownloadQuality,
) -> Result<FilenamePlan, NamingError> {
    for value in [title, singer, format] {
        check_input(value)?;
    }
    let value = format
        .replacen("歌手", &clip_singer(singer), 1)
        .replacen("歌名", title, 1);
    let mut adjustments = Vec::new();
    let stem = clip_utf16(&value, 150, &mut adjustments);
    Ok(FilenamePlan {
        basename: filter_filename(&format!("{stem}.{}", quality.extension())),
        adjustments,
    })
}
pub fn safe_filename(
    title: &str,
    singer: &str,
    format: FilenameFormat,
    quality: DownloadQuality,
) -> Result<FilenamePlan, NamingError> {
    let mut plan = compatibility_filename(title, singer, format.lx_value(), quality)?;
    let extension = format!(".{}", quality.extension());
    let stem = plan
        .basename
        .strip_suffix(&extension)
        .expect("Closed download extension");
    let trimmed = stem.trim_end_matches(['.', ' ']);
    if trimmed != stem {
        plan.adjustments
            .push(NamingAdjustment::TrailingDotOrSpaceRemoved);
    }
    let stem = if trimmed.is_empty() {
        plan.adjustments.push(NamingAdjustment::EmptyNameFallback);
        "未命名"
    } else {
        trimmed
    };
    plan.basename = format!("{stem}{extension}");
    validate_component(&plan.basename)?;
    Ok(plan)
}
pub fn group_component(
    list_name: Option<&str>,
    default_list_name: &str,
) -> Result<FilenamePlan, NamingError> {
    check_input(default_list_name)?;
    if let Some(name) = list_name {
        check_input(name)?;
    }
    let name = list_name
        .filter(|name| !name.is_empty())
        .unwrap_or(default_list_name);
    if name.starts_with(['/', '\\'])
        || name.contains(':')
        || name
            .split(['/', '\\'])
            .any(|part| matches!(part, "." | ".."))
    {
        return Err(NamingError::InvalidComponent);
    }
    let filtered = filter_filename(name);
    let mut adjustments = Vec::new();
    let clipped = clip_utf16(&filtered, 150, &mut adjustments);
    let trimmed = clipped.trim_end_matches(['.', ' ']);
    if trimmed != clipped {
        adjustments.push(NamingAdjustment::TrailingDotOrSpaceRemoved);
    }
    let basename = if trimmed.is_empty() {
        adjustments.push(NamingAdjustment::EmptyNameFallback);
        "未命名".to_owned()
    } else {
        trimmed.to_owned()
    };
    validate_component(&basename)?;
    Ok(FilenamePlan {
        basename,
        adjustments,
    })
}
fn validate_component(value: &str) -> Result<(), NamingError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.ends_with(['.', ' '])
        || value
            .chars()
            .any(|character| character.is_control() || "\\/:*?#\"<>|".contains(character))
    {
        return Err(NamingError::InvalidComponent);
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let device_suffix = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || device_suffix.is_some_and(|suffix| {
        matches!(
            suffix,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    }) {
        return Err(NamingError::ReservedComponent);
    }
    Ok(())
}
