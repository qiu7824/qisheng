use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub type SyncResult<T> = Result<T, SyncError>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncErrorKind {
    InvalidData,
    Quota,
    Authentication,
    Unsupported,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncError {
    pub kind: SyncErrorKind,
    pub message: String,
}
impl SyncError {
    pub fn new(kind: SyncErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into().chars().take(1024).collect(),
        }
    }
}
impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for SyncError {}
pub(crate) fn invalid(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::InvalidData, message)
}
pub(crate) fn quota(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::Quota, message)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dataset {
    List,
    Dislike,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddLocation {
    #[default]
    Top,
    Bottom,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    MergeLocalRemote,
    MergeRemoteLocal,
    OverwriteLocalRemote,
    OverwriteRemoteLocal,
    OverwriteLocalRemoteFull,
    OverwriteRemoteLocalFull,
    Cancel,
}
#[derive(Clone, Debug, PartialEq)]
pub struct PeerCapabilities {
    pub list: Option<bool>,
    pub dislike: Option<bool>,
}
impl PeerCapabilities {
    pub fn from_wire(value: &Value) -> SyncResult<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("Features must be an object"))?;
        if object.keys().any(|key| key != "list" && key != "dislike") {
            return Err(SyncError::new(
                SyncErrorKind::Unsupported,
                "Unsupported sync feature",
            ));
        }
        let config = |key| -> SyncResult<Option<bool>> {
            match object.get(key) {
                None | Some(Value::Bool(false)) => Ok(None),
                Some(Value::Object(config)) if config.len() == 1 => config
                    .get("skipSnapshot")
                    .and_then(Value::as_bool)
                    .map(Some)
                    .ok_or_else(|| invalid("Invalid feature options")),
                _ => Err(invalid("Invalid feature options")),
            }
        };
        Ok(Self {
            list: config("list")?,
            dislike: config("dislike")?,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MusicKind {
    Remote,
    LocalReference,
}
/// Validated LX metadata. LocalReference strings never construct a LocalAsset,
/// canonicalize/open a path, or transfer a filesystem permission.
#[derive(Clone, Debug, PartialEq)]
pub struct MusicInfo {
    wire: Value,
}
impl MusicInfo {
    pub fn from_wire(wire: Value) -> SyncResult<Self> {
        let object = wire
            .as_object()
            .ok_or_else(|| invalid("Music must be an object"))?;
        if super::codec::js_json(&wire)?.len() > 256 * 1024 {
            return Err(quota("Music metadata too large"));
        }
        for name in ["id", "name", "singer", "source"] {
            let value = object
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Invalid music field"))?;
            if value.len() > 16384 || value.contains('\0') || name == "id" && value.is_empty() {
                return Err(invalid("Invalid music field"));
            }
        }
        if !["kw", "kg", "tx", "wy", "mg", "local"].contains(&object["source"].as_str().unwrap()) {
            return Err(SyncError::new(
                SyncErrorKind::Unsupported,
                "Unsupported music source",
            ));
        }
        if object.keys().any(|key| {
            !["id", "name", "singer", "source", "interval", "meta"].contains(&key.as_str())
        }) {
            return Err(invalid("Unknown music fields"));
        }
        if object
            .get("interval")
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return Err(invalid("Invalid music interval"));
        }
        let meta = object
            .get("meta")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("Music meta missing"))?;
        if !meta
            .get("songId")
            .is_some_and(|value| value.is_string() || value.is_number())
            || !meta.get("albumName").is_some_and(Value::is_string)
        {
            return Err(invalid("Invalid music identity"));
        }
        if let Some(picture) = meta.get("picUrl")
            && !picture.is_null()
            && !picture.is_string()
        {
            return Err(invalid("Invalid picture metadata"));
        }
        if object["source"] == "local" {
            let path = meta
                .get("filePath")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty() && path.len() <= 16384 && !path.contains('\0'))
                .ok_or_else(|| invalid("Invalid local reference path"))?;
            let _ = path;
            if !meta
                .get("ext")
                .and_then(Value::as_str)
                .is_some_and(|extension| extension.len() <= 64 && !extension.contains('\0'))
            {
                return Err(invalid("Invalid local reference extension"));
            }
            if let Some(toggle) = meta.get("toggleMusicInfo").filter(|value| !value.is_null())
                && Self::from_wire(toggle.clone())?.is_local_reference()
            {
                return Err(invalid("Toggle metadata must be remote"));
            }
            reject_authority_fields(&wire, Some(meta))?;
            return Ok(Self { wire });
        }
        let qualities = meta
            .get("qualitys")
            .and_then(Value::as_array)
            .filter(|qualities| qualities.len() <= 128)
            .ok_or_else(|| invalid("Invalid music qualities"))?;
        let quality_map = meta
            .get("_qualitys")
            .and_then(Value::as_object)
            .filter(|qualities| qualities.len() <= 128)
            .ok_or_else(|| invalid("Invalid music quality map"))?;
        let kg = object["source"] == "kg";
        let quality = |value: &Value| -> SyncResult<()> {
            let value = value
                .as_object()
                .ok_or_else(|| invalid("Invalid music quality"))?;
            if value
                .get("size")
                .is_some_and(|size| !size.is_null() && !size.is_string())
                || kg && !value.get("hash").is_some_and(Value::is_string)
            {
                return Err(invalid("Invalid quality metadata"));
            }
            Ok(())
        };
        for value in qualities {
            if !value.get("type").is_some_and(Value::is_string) {
                return Err(invalid("Quality type missing"));
            }
            quality(value)?;
        }
        for value in quality_map.values() {
            quality(value)?;
        }
        let required = match object["source"].as_str().unwrap() {
            "kg" => Some("hash"),
            "tx" => Some("strMediaMid"),
            "mg" => Some("copyrightId"),
            _ => None,
        };
        if required.is_some_and(|field| !meta.get(field).is_some_and(Value::is_string)) {
            return Err(invalid("Platform metadata missing"));
        }
        if let Some(toggle) = meta.get("toggleMusicInfo").filter(|value| !value.is_null())
            && Self::from_wire(toggle.clone())?.is_local_reference()
        {
            return Err(invalid("Toggle metadata must be remote"));
        }
        reject_authority_fields(&wire, None)?;
        Ok(Self { wire })
    }
    pub fn id(&self) -> &str {
        self.wire["id"].as_str().unwrap()
    }
    pub fn wire(&self) -> &Value {
        &self.wire
    }
    pub fn kind(&self) -> MusicKind {
        if self.wire["source"] == "local" {
            MusicKind::LocalReference
        } else {
            MusicKind::Remote
        }
    }
    pub fn is_local_reference(&self) -> bool {
        self.kind() == MusicKind::LocalReference
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct UserList {
    pub id: String,
    pub name: Value,
    pub source: Option<Value>,
    pub source_list_id: Option<Value>,
    pub location_update_time: Option<Value>,
    pub list: Vec<MusicInfo>,
}
impl UserList {
    pub fn from_wire(value: &Value) -> SyncResult<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("User list must be an object"))?;
        allowed_keys(
            object,
            &[
                "id",
                "name",
                "source",
                "sourceListId",
                "locationUpdateTime",
                "list",
            ],
        )?;
        let id = identifier(object.get("id").ok_or_else(|| invalid("List id missing"))?)?;
        if ["default", "love", "temp", "download"].contains(&id.as_str()) {
            return Err(invalid("Reserved user list id"));
        }
        let name = object
            .get("name")
            .filter(|value| value.is_string())
            .cloned()
            .ok_or_else(|| invalid("List name missing"))?;
        let source = object.get("source").cloned();
        let source_list_id = object.get("sourceListId").cloned();
        let location_update_time = object.get("locationUpdateTime").cloned();
        if source
            .as_ref()
            .is_some_and(|value| !value.is_null() && !value.is_string())
            || source_list_id
                .as_ref()
                .is_some_and(|value| !value.is_null() && !value.is_string() && !value.is_number())
            || location_update_time.as_ref().is_some_and(|value| {
                !value.is_null()
                    && value
                        .as_f64()
                        .is_none_or(|number| !number.is_finite() || number < 0.0)
            })
        {
            return Err(invalid("Invalid user list metadata"));
        }
        let list = music_array(
            object
                .get("list")
                .ok_or_else(|| invalid("List music missing"))?,
        )?;
        Ok(Self {
            id,
            name,
            source,
            source_list_id,
            location_update_time,
            list,
        })
    }
    pub fn wire(&self) -> Value {
        let mut object = Map::new();
        object.insert("id".into(), Value::String(self.id.clone()));
        object.insert("name".into(), self.name.clone());
        if let Some(value) = &self.source {
            object.insert("source".into(), value.clone());
        }
        if let Some(value) = &self.source_list_id {
            object.insert("sourceListId".into(), value.clone());
        }
        if let Some(value) = &self.location_update_time {
            object.insert("locationUpdateTime".into(), value.clone());
        }
        object.insert(
            "list".into(),
            Value::Array(self.list.iter().map(|item| item.wire.clone()).collect()),
        );
        Value::Object(object)
    }
    pub(crate) fn location_time(&self) -> f64 {
        self.location_update_time
            .as_ref()
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListData {
    pub default_list: Vec<MusicInfo>,
    pub love_list: Vec<MusicInfo>,
    pub user_list: Vec<UserList>,
}
impl ListData {
    pub fn from_wire(value: &Value) -> SyncResult<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("List data must be an object"))?;
        allowed_keys(object, &["defaultList", "loveList", "userList"])?;
        let default_list = object
            .get("defaultList")
            .map(music_array)
            .transpose()?
            .unwrap_or_default();
        let love_list = object
            .get("loveList")
            .map(music_array)
            .transpose()?
            .unwrap_or_default();
        if object
            .get("userList")
            .is_some_and(|value| !value.is_array())
        {
            return Err(invalid("User lists must be an array"));
        }
        let users = object
            .get("userList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if users.len() > 1000 {
            return Err(quota("Too many user lists"));
        }
        let user_list = users
            .iter()
            .map(UserList::from_wire)
            .collect::<SyncResult<Vec<_>>>()?;
        let data = Self {
            default_list,
            love_list,
            user_list,
        };
        data.validate()?;
        Ok(data)
    }
    pub fn validate(&self) -> SyncResult<()> {
        if self.user_list.len() > 1000
            || self.default_list.len()
                + self.love_list.len()
                + self
                    .user_list
                    .iter()
                    .map(|list| list.list.len())
                    .sum::<usize>()
                > 100000
        {
            return Err(quota("List dataset exceeds item budget"));
        }
        let mut ids = HashSet::new();
        if self.user_list.iter().any(|list| !ids.insert(&list.id)) {
            return Err(invalid("Duplicate user list id"));
        }
        if super::codec::js_json(&self.wire())?.len() > super::MAX_JSON_BYTES {
            return Err(quota("List dataset exceeds byte budget"));
        }
        Ok(())
    }
    pub fn wire(&self) -> Value {
        let mut object = Map::new();
        object.insert(
            "defaultList".into(),
            Value::Array(
                self.default_list
                    .iter()
                    .map(|item| item.wire.clone())
                    .collect(),
            ),
        );
        object.insert(
            "loveList".into(),
            Value::Array(
                self.love_list
                    .iter()
                    .map(|item| item.wire.clone())
                    .collect(),
            ),
        );
        object.insert(
            "userList".into(),
            Value::Array(self.user_list.iter().map(UserList::wire).collect()),
        );
        Value::Object(object)
    }
    pub fn digest(&self) -> SyncResult<String> {
        use md5::{Digest, Md5};
        Ok(format!(
            "{:x}",
            Md5::digest(super::codec::js_json(&self.wire())?.as_bytes())
        ))
    }
    pub fn is_empty(&self) -> bool {
        self.default_list.is_empty() && self.love_list.is_empty() && self.user_list.is_empty()
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum ListAction {
    DataOverwrite(ListData),
    Create {
        position: i64,
        lists: Vec<Value>,
    },
    Remove(Vec<String>),
    Update(Vec<Value>),
    UpdatePosition {
        position: i64,
        ids: Vec<String>,
    },
    MusicAdd {
        id: String,
        music: Vec<MusicInfo>,
        location: AddLocation,
    },
    MusicMove {
        from_id: String,
        to_id: String,
        music: Vec<MusicInfo>,
        location: AddLocation,
    },
    MusicRemove {
        list_id: String,
        ids: Vec<String>,
    },
    MusicUpdate(Vec<(String, MusicInfo)>),
    MusicUpdatePosition {
        list_id: String,
        position: i64,
        ids: Vec<String>,
    },
    MusicOverwrite {
        list_id: String,
        music: Vec<MusicInfo>,
    },
    MusicClear(Vec<String>),
}
#[derive(Clone, Debug, PartialEq)]
pub enum DislikeAction {
    DataOverwrite(String),
    MusicAdd(Vec<(String, String)>),
    MusicClear,
}
impl ListAction {
    pub fn from_wire(value: &Value) -> SyncResult<Self> {
        let envelope = value
            .as_object()
            .ok_or_else(|| invalid("Action must be an object"))?;
        allowed_keys(envelope, &["action", "data"])?;
        let action = envelope
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Action missing"))?;
        let data = envelope
            .get("data")
            .ok_or_else(|| invalid("Action data missing"))?;
        let object = || {
            data.as_object()
                .ok_or_else(|| invalid("Action data must be an object"))
        };
        let list_id = |object: &Map<String, Value>, key| -> SyncResult<String> {
            let id = identifier(object.get(key).ok_or_else(|| invalid("List id missing"))?)?;
            if ["temp", "download"].contains(&id.as_str()) {
                return Err(invalid("Private list cannot be synchronized"));
            }
            Ok(id)
        };
        Ok(match action {
            "list_data_overwrite" => Self::DataOverwrite(ListData::from_wire(data)?),
            "list_create" => {
                let object = object()?;
                allowed_keys(object, &["position", "listInfos"])?;
                Self::Create {
                    position: position(object)?,
                    lists: list_infos(
                        object
                            .get("listInfos")
                            .ok_or_else(|| invalid("List infos missing"))?,
                    )?,
                }
            }
            "list_remove" => {
                let ids = list_ids(data)?;
                if ids
                    .iter()
                    .any(|id| ["default", "love"].contains(&id.as_str()))
                {
                    return Err(invalid("Built-in list cannot be removed"));
                }
                Self::Remove(ids)
            }
            "list_update" => Self::Update(list_infos(data)?),
            "list_update_position" => {
                let object = object()?;
                allowed_keys(object, &["position", "ids"])?;
                let ids = list_ids(object.get("ids").ok_or_else(|| invalid("Ids missing"))?)?;
                if ids
                    .iter()
                    .any(|id| ["default", "love"].contains(&id.as_str()))
                {
                    return Err(invalid("Built-in list cannot be moved"));
                }
                Self::UpdatePosition {
                    position: position(object)?,
                    ids,
                }
            }
            "list_music_add" => {
                let object = object()?;
                allowed_keys(object, &["id", "musicInfos", "addMusicLocationType"])?;
                Self::MusicAdd {
                    id: list_id(object, "id")?,
                    music: music_array(
                        object
                            .get("musicInfos")
                            .ok_or_else(|| invalid("Music infos missing"))?,
                    )?,
                    location: location(object)?,
                }
            }
            "list_music_move" => {
                let object = object()?;
                allowed_keys(
                    object,
                    &["fromId", "toId", "musicInfos", "addMusicLocationType"],
                )?;
                let from_id = list_id(object, "fromId")?;
                let to_id = list_id(object, "toId")?;
                if from_id == to_id {
                    return Err(invalid("Move requires two lists"));
                }
                Self::MusicMove {
                    from_id,
                    to_id,
                    music: music_array(
                        object
                            .get("musicInfos")
                            .ok_or_else(|| invalid("Music infos missing"))?,
                    )?,
                    location: location(object)?,
                }
            }
            "list_music_remove" => {
                let object = object()?;
                allowed_keys(object, &["listId", "ids"])?;
                Self::MusicRemove {
                    list_id: list_id(object, "listId")?,
                    ids: ids(object.get("ids").ok_or_else(|| invalid("Ids missing"))?)?,
                }
            }
            "list_music_update" => {
                let updates = data
                    .as_array()
                    .filter(|updates| updates.len() <= 100000)
                    .ok_or_else(|| invalid("Invalid music updates"))?;
                let mut result = Vec::new();
                for update in updates {
                    let object = update
                        .as_object()
                        .ok_or_else(|| invalid("Update must be an object"))?;
                    allowed_keys(object, &["id", "musicInfo"])?;
                    result.push((
                        list_id(object, "id")?,
                        MusicInfo::from_wire(
                            object
                                .get("musicInfo")
                                .ok_or_else(|| invalid("Music info missing"))?
                                .clone(),
                        )?,
                    ));
                }
                Self::MusicUpdate(result)
            }
            "list_music_update_position" => {
                let object = object()?;
                allowed_keys(object, &["listId", "position", "ids"])?;
                Self::MusicUpdatePosition {
                    list_id: list_id(object, "listId")?,
                    position: position(object)?,
                    ids: ids(object.get("ids").ok_or_else(|| invalid("Ids missing"))?)?,
                }
            }
            "list_music_overwrite" => {
                let object = object()?;
                allowed_keys(object, &["listId", "musicInfos"])?;
                Self::MusicOverwrite {
                    list_id: list_id(object, "listId")?,
                    music: music_array(
                        object
                            .get("musicInfos")
                            .ok_or_else(|| invalid("Music infos missing"))?,
                    )?,
                }
            }
            "list_music_clear" => Self::MusicClear(list_ids(data)?),
            _ => return Err(invalid("Unknown list action")),
        })
    }
}
impl DislikeAction {
    pub fn from_wire(value: &Value) -> SyncResult<Self> {
        let envelope = value
            .as_object()
            .ok_or_else(|| invalid("Action must be an object"))?;
        allowed_keys(envelope, &["action", "data"])?;
        match envelope.get("action").and_then(Value::as_str) {
            Some("dislike_music_clear") if !envelope.contains_key("data") => Ok(Self::MusicClear),
            Some("dislike_data_overwrite") => {
                let data = envelope
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("Dislike data must be a string"))?;
                super::merge::validate_rules(data)?;
                Ok(Self::DataOverwrite(data.into()))
            }
            Some("dislike_music_add") => {
                let array = envelope
                    .get("data")
                    .and_then(Value::as_array)
                    .filter(|array| array.len() <= 100000)
                    .ok_or_else(|| invalid("Invalid dislike array"))?;
                let mut result = Vec::new();
                let mut bytes = 0;
                for item in array {
                    let object = item
                        .as_object()
                        .ok_or_else(|| invalid("Dislike info must be an object"))?;
                    allowed_keys(object, &["name", "singer"])?;
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("Dislike name missing"))?;
                    let singer = object
                        .get("singer")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("Dislike singer missing"))?;
                    bytes += name.len() + singer.len() + 2;
                    if bytes > 2 * 1024 * 1024 || name.contains('\0') || singer.contains('\0') {
                        return Err(quota("Dislike additions exceed budget"));
                    }
                    result.push((name.into(), singer.into()));
                }
                Ok(Self::MusicAdd(result))
            }
            _ => Err(invalid("Unknown dislike action")),
        }
    }
}
fn position(object: &Map<String, Value>) -> SyncResult<i64> {
    let value = object
        .get("position")
        .and_then(Value::as_f64)
        .filter(|number| {
            number.is_finite() && number.fract() == 0.0 && (-1.0..=100000.0).contains(number)
        })
        .ok_or_else(|| invalid("Invalid position"))?;
    Ok(value as i64)
}
fn location(object: &Map<String, Value>) -> SyncResult<AddLocation> {
    match object.get("addMusicLocationType").and_then(Value::as_str) {
        Some("top") => Ok(AddLocation::Top),
        Some("bottom") => Ok(AddLocation::Bottom),
        _ => Err(invalid("Invalid add location")),
    }
}
fn ids(value: &Value) -> SyncResult<Vec<String>> {
    let array = value
        .as_array()
        .filter(|array| array.len() <= 100000)
        .ok_or_else(|| invalid("Invalid ids"))?;
    array.iter().map(identifier).collect()
}
fn list_ids(value: &Value) -> SyncResult<Vec<String>> {
    let values = ids(value)?;
    if values
        .iter()
        .any(|id| ["temp", "download"].contains(&id.as_str()))
    {
        return Err(invalid("Private list cannot be synchronized"));
    }
    Ok(values)
}
fn list_infos(value: &Value) -> SyncResult<Vec<Value>> {
    let array = value
        .as_array()
        .filter(|array| array.len() <= 1000)
        .ok_or_else(|| invalid("Invalid list infos"))?;
    array
        .iter()
        .map(|value| {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("List info must be an object"))?;
            allowed_keys(
                object,
                &["id", "name", "source", "sourceListId", "locationUpdateTime"],
            )?;
            let mut full = object.clone();
            full.insert("list".into(), Value::Array(Vec::new()));
            UserList::from_wire(&Value::Object(full))?;
            Ok(value.clone())
        })
        .collect()
}
pub(crate) fn allowed_keys(object: &Map<String, Value>, keys: &[&str]) -> SyncResult<()> {
    if object.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err(invalid("Unknown sync fields"));
    }
    Ok(())
}
pub(crate) fn identifier(value: &Value) -> SyncResult<String> {
    let id = value
        .as_str()
        .ok_or_else(|| invalid("Identifier must be a string"))?;
    if id.is_empty() || id.len() > 1024 || id.contains('\0') {
        return Err(invalid("Invalid identifier"));
    }
    Ok(id.into())
}
pub(crate) fn music_array(value: &Value) -> SyncResult<Vec<MusicInfo>> {
    let list = value
        .as_array()
        .ok_or_else(|| invalid("Music list must be an array"))?;
    if list.len() > 100000 {
        return Err(quota("Too many songs"));
    }
    list.iter().cloned().map(MusicInfo::from_wire).collect()
}
fn reject_authority_fields(
    value: &Value,
    local_meta: Option<&Map<String, Value>>,
) -> SyncResult<()> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if [
                    "lyrics",
                    "lyric",
                    "tlyric",
                    "rlyric",
                    "lxlyric",
                    "playbackClock",
                    "playback_checkpoint",
                    "permissions",
                    "sourcePermissions",
                    "local_asset_id",
                    "fileProof",
                    "rootGrant",
                    "grant_epoch",
                    "authority",
                    "filePath",
                    "grant",
                    "approvedUrl",
                    "authorization",
                    "cookie",
                    "headers",
                    "script",
                ]
                .contains(&key.as_str())
                    && !(key == "filePath"
                        && local_meta.is_some_and(|meta| std::ptr::eq(meta, object)))
                {
                    return Err(invalid("Sync cannot carry authority or private datasets"));
                }
                reject_authority_fields(value, local_meta)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                reject_authority_fields(value, local_meta)?;
            }
        }
        _ => {}
    }
    Ok(())
}
