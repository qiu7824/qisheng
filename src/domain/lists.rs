use super::music::TrackRecord;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub const DEFAULT_LIST: &str = "default";
pub const LOVE_LIST: &str = "love";
pub const TEMP_LIST: &str = "temp";
pub const SEARCH_HISTORY_LIMIT: usize = 15;
pub type ListPositionInfo = BTreeMap<String, f64>;
pub type ListUpdateInfoMap = BTreeMap<String, ListUpdateInfo>;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ListInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(
        default,
        alias = "sourceListId",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_list_id: Option<String>,
    #[serde(default, alias = "locationUpdateTime")]
    pub location_update_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}
impl ListInfo {
    pub fn user(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            ..Self::default()
        }
    }
    pub fn is_builtin(&self) -> bool {
        is_builtin(&self.id)
    }
}
pub fn is_builtin(id: &str) -> bool {
    matches!(id, DEFAULT_LIST | LOVE_LIST | TEMP_LIST)
}
pub fn builtin_lists() -> Vec<ListInfo> {
    [
        (DEFAULT_LIST, "list__name_default"),
        (LOVE_LIST, "list__name_love"),
        (TEMP_LIST, "临时列表"),
    ]
    .into_iter()
    .map(|(id, name)| ListInfo::user(id, name))
    .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UserList {
    pub info: ListInfo,
    pub tracks: Vec<TrackRecord>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ListData {
    #[serde(alias = "defaultList")]
    pub default_list: Vec<TrackRecord>,
    #[serde(alias = "loveList")]
    pub love_list: Vec<TrackRecord>,
    #[serde(alias = "userList")]
    pub user_list: Vec<UserList>,
    /// An omitted temporary list preserves the existing temporary tracks.
    #[serde(default, alias = "tempList", skip_serializing_if = "Option::is_none")]
    pub temp_list: Option<Vec<TrackRecord>>,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AddLocation {
    Top,
    #[default]
    Bottom,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ListMusicUpdate {
    pub list_id: String,
    pub track: TrackRecord,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ListAction {
    DataOverwrite {
        data: ListData,
    },
    Create {
        position: i64,
        lists: Vec<ListInfo>,
    },
    Remove {
        ids: Vec<String>,
    },
    Update {
        lists: Vec<ListInfo>,
    },
    UpdatePosition {
        ids: Vec<String>,
        position: i64,
    },
    MusicOverwrite {
        list_id: String,
        tracks: Vec<TrackRecord>,
    },
    MusicAdd {
        list_id: String,
        tracks: Vec<TrackRecord>,
        location: AddLocation,
    },
    MusicMove {
        from_id: String,
        to_id: String,
        tracks: Vec<TrackRecord>,
        location: AddLocation,
    },
    MusicRemove {
        list_id: String,
        ids: Vec<String>,
    },
    MusicUpdate {
        updates: Vec<ListMusicUpdate>,
    },
    MusicClear {
        list_ids: Vec<String>,
    },
    MusicUpdatePosition {
        list_id: String,
        ids: Vec<String>,
        position: i64,
    },
}
impl ListAction {
    pub fn name(&self) -> &'static str {
        match self {
            Self::DataOverwrite { .. } => "list_data_overwrite",
            Self::Create { .. } => "list_create",
            Self::Remove { .. } => "list_remove",
            Self::Update { .. } => "list_update",
            Self::UpdatePosition { .. } => "list_update_position",
            Self::MusicOverwrite { .. } => "list_music_overwrite",
            Self::MusicAdd { .. } => "list_music_add",
            Self::MusicMove { .. } => "list_music_move",
            Self::MusicRemove { .. } => "list_music_remove",
            Self::MusicUpdate { .. } => "list_music_update",
            Self::MusicClear { .. } => "list_music_clear",
            Self::MusicUpdatePosition { .. } => "list_music_update_position",
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListUpdateInfo {
    #[serde(alias = "updateTime")]
    pub update_time: u64,
    #[serde(alias = "isAutoUpdate")]
    pub is_auto_update: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DislikeMusicInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub singer: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DislikeAction {
    Overwrite { rules: String },
    Add { infos: Vec<DislikeMusicInfo> },
    Clear,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DislikeRules {
    pub rules: String,
    pub music_names: BTreeSet<String>,
    pub singer_names: BTreeSet<String>,
    pub names: BTreeSet<String>,
}
fn dislike_name(value: &str) -> String {
    trim_dislike(&value.replace('@', "#").to_lowercase()).to_owned()
}
pub(crate) fn trim_dislike(value: &str) -> &str {
    value.trim_matches(|ch|matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
impl DislikeRules {
    pub fn parse(raw: &str) -> Self {
        let mut out = Self::default();
        let mut ordered = Vec::new();
        let mut seen = HashSet::new();
        for line in raw
            .split('\n')
            .filter(|line| !trim_dislike(line).is_empty())
        {
            let mut fields = line.split('@');
            let name = fields.next().unwrap_or("");
            let singer = fields.next().unwrap_or("");
            let rule = if !name.is_empty() {
                let name = dislike_name(name);
                if !singer.is_empty() {
                    let rule = format!("{name}@{}", dislike_name(singer));
                    out.names.insert(rule.clone());
                    rule
                } else {
                    out.music_names.insert(name.clone());
                    name
                }
            } else if !singer.is_empty() {
                let singer = dislike_name(singer);
                out.singer_names.insert(singer.clone());
                format!("@{singer}")
            } else {
                continue;
            };
            if seen.insert(rule.clone()) {
                ordered.push(rule);
            }
        }
        out.rules = ordered.join("\n");
        out
    }
    pub fn matches(&self, name: &str, singer: &str) -> bool {
        let name = dislike_name(name);
        let singer = dislike_name(singer);
        self.music_names.contains(&name)
            || self.singer_names.contains(&singer)
            || self.names.contains(&format!("{name}@{singer}"))
    }
    pub fn has_dislike(&self, track: &TrackRecord) -> bool {
        self.matches(&track.title, &track.artists.join("、"))
    }
}
