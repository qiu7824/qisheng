use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_LYRIC_BYTES: usize = 2 * 1024 * 1024;

/// Missing means not fetched, null means unavailable, and empty text means fetched empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LyricField {
    #[default]
    Missing,
    Null,
    Text(String),
}

impl LyricField {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }
}

impl Serialize for LyricField {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(text) => serializer.serialize_str(text),
            Self::Missing | Self::Null => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for LyricField {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<String>::deserialize(deserializer)
            .map(|text| text.map(Self::Text).unwrap_or(Self::Null))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LyricOrigin {
    Raw,
    Edited,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LyricKind {
    Lyric,
    Tlyric,
    Rlyric,
    Lxlyric,
}

impl LyricKind {
    pub const ALL: [Self; 4] = [Self::Lyric, Self::Tlyric, Self::Rlyric, Self::Lxlyric];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lyric => "lyric",
            Self::Tlyric => "tlyric",
            Self::Rlyric => "rlyric",
            Self::Lxlyric => "lxlyric",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LyricBundle {
    #[serde(default, skip_serializing_if = "LyricField::is_missing")]
    pub lyric: LyricField,
    #[serde(default, skip_serializing_if = "LyricField::is_missing")]
    pub tlyric: LyricField,
    #[serde(default, skip_serializing_if = "LyricField::is_missing")]
    pub rlyric: LyricField,
    #[serde(default, skip_serializing_if = "LyricField::is_missing")]
    pub lxlyric: LyricField,
}

impl LyricBundle {
    pub fn from_value(value: &Value) -> Result<Self> {
        ensure!(value.is_object(), "Lyric response must be an object");
        let bundle: Self = serde_json::from_value(value.clone())?;
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("Lyric bundle is JSON serializable")
    }

    pub fn validate(&self) -> Result<()> {
        let mut bytes = 0usize;
        for kind in LyricKind::ALL {
            if let LyricField::Text(text) = self.field(kind) {
                bytes = bytes.saturating_add(text.len());
                ensure!(!text.contains('\0'), "Lyric text contains a NUL character");
            }
        }
        ensure!(
            bytes <= MAX_LYRIC_BYTES,
            "Lyric text exceeds the 2 MiB limit"
        );
        Ok(())
    }

    pub fn field(&self, kind: LyricKind) -> &LyricField {
        match kind {
            LyricKind::Lyric => &self.lyric,
            LyricKind::Tlyric => &self.tlyric,
            LyricKind::Rlyric => &self.rlyric,
            LyricKind::Lxlyric => &self.lxlyric,
        }
    }

    pub fn set_field(&mut self, kind: LyricKind, value: LyricField) {
        *match kind {
            LyricKind::Lyric => &mut self.lyric,
            LyricKind::Tlyric => &mut self.tlyric,
            LyricKind::Rlyric => &mut self.rlyric,
            LyricKind::Lxlyric => &mut self.lxlyric,
        } = value;
    }
}
