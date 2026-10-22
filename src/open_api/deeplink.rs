//! Parsing produces remote metadata and typed intentions, never file/network grants.
use super::protocol::{invalid, js_parse_int, percent_decode};
use super::{ControlAction, ControlResult};
use serde_json::{Map, Value};

const SOURCES: [&str; 5] = ["kw", "kg", "tx", "wy", "mg"];
#[derive(Clone, Debug, PartialEq)]
pub struct DeepLinkMusicInfo {
    pub source: String,
    pub wire: Value,
}
#[derive(Clone, Debug, PartialEq)]
pub enum DeepLinkAction {
    Player(ControlAction),
    Search {
        keywords: String,
        source: Option<String>,
    },
    MusicPlay {
        music: DeepLinkMusicInfo,
    },
    SearchPlay {
        name: String,
        singer: String,
        album_name: String,
        interval: Option<String>,
        play_later: bool,
    },
    SonglistOpen {
        source: String,
        id: Option<String>,
        url: Option<String>,
    },
    SonglistPlay {
        source: String,
        id: Option<String>,
        url: Option<String>,
        index: Option<usize>,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedDeepLink {
    pub action: DeepLinkAction,
}

pub fn parse_deeplink(link: &str) -> ControlResult<ParsedDeepLink> {
    if link.len() > 64 * 1024 || link.contains(['\0', '#', '\r', '\n']) {
        return Err(invalid("Invalid deeplink"));
    }
    let rest = link
        .strip_prefix("lxmusic://")
        .ok_or_else(|| invalid("Invalid deeplink scheme"))?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if path.contains('@') || path.contains('\\') {
        return Err(invalid("Invalid deeplink authority"));
    }
    let mut segments = path.split('/');
    let kind = segments.next().unwrap_or_default();
    let action = segments
        .next()
        .ok_or_else(|| invalid("Missing deeplink action"))?;
    let paths: Vec<_> = segments
        .map(|segment| percent_decode(segment, false))
        .collect::<ControlResult<_>>()?;
    if paths.len() > 16 {
        return Err(invalid("Too many deeplink paths"));
    }
    let mut data = None;
    if query.split('&').count() > 32 {
        return Err(invalid("Too many deeplink values"));
    }
    for pair in query.split('&') {
        // The fixed upstream parser uses the first two '=' segments.
        let mut pair = pair.split('=');
        if pair.next() == Some("data") {
            let decoded = percent_decode(pair.next().unwrap_or_default(), false)?;
            if decoded.len() > 48 * 1024 {
                return Err(invalid("Deeplink data too large"));
            }
            let value: Value =
                serde_json::from_str(&decoded).map_err(|_| invalid("Invalid deeplink JSON"))?;
            if json_depth(&value) > 32 {
                return Err(invalid("Deeplink JSON too deep"));
            }
            if !value.is_object() {
                return Err(invalid("Deeplink data must be an object"));
            }
            data = Some(value);
        }
    }
    let action = match kind {
        "player" => DeepLinkAction::Player(match action {
            "play" => ControlAction::Play,
            "pause" => ControlAction::Pause,
            "skipNext" => ControlAction::Next,
            "skipPrev" => ControlAction::Previous,
            "togglePlay" => ControlAction::Toggle,
            "collect" => ControlAction::CollectCurrent { collected: true },
            "uncollect" => ControlAction::CollectCurrent { collected: false },
            "dislike" => ControlAction::DislikeCurrent,
            _ => return Err(invalid("Unknown player action")),
        }),
        "music" => match action {
            "search" => {
                let (keywords, source) = if let Some(data) = data.as_ref() {
                    (
                        string_or_number(data, "keywords", true, 128)?.unwrap(),
                        data.get("source")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    )
                } else {
                    let keyword = if paths.len() > 1 {
                        &paths[1]
                    } else {
                        paths.first().ok_or_else(|| invalid("Keyword missing"))?
                    };
                    (
                        truncate_utf16(keyword, 128),
                        (paths.len() > 1).then(|| paths[0].clone()),
                    )
                };
                DeepLinkAction::Search {
                    keywords,
                    source: source
                        .filter(|source| SOURCES.contains(&source.as_str()) || source == "all"),
                }
            }
            "play" => DeepLinkAction::MusicPlay {
                music: parse_music(data.as_ref().ok_or_else(|| invalid("Music data missing"))?)?,
            },
            "searchPlay" => {
                let path_data;
                let info = if let Some(path) = paths.first() {
                    let mut parts = path.trim().split('-');
                    let name = parts.next().unwrap_or_default().trim();
                    let singer = parts.next().unwrap_or_default().trim();
                    path_data = serde_json::json!({"name":name,"singer":singer});
                    &path_data
                } else {
                    data.as_ref()
                        .ok_or_else(|| invalid("Search music data missing"))?
                };
                DeepLinkAction::SearchPlay {
                    name: text(info, "name", true, 200)?.unwrap(),
                    singer: text(info, "singer", false, 200)?.unwrap_or_default(),
                    album_name: text(info, "albumName", false, 200)?.unwrap_or_default(),
                    interval: text(info, "interval", false, 64)?,
                    play_later: boolean(info, "playLater")?.unwrap_or(false),
                }
            }
            _ => return Err(invalid("Unknown music action")),
        },
        "songlist" => {
            let (source, id, url, index) = if let Some(info) = data.as_ref() {
                let source = text(info, "source", true, 64)?.unwrap();
                let id = string_or_number(info, "id", false, 64)?.filter(|_| {
                    !info
                        .get("id")
                        .and_then(Value::as_f64)
                        .is_some_and(|value| value == 0.0)
                });
                let url = text(info, "url", false, 500)?;
                let index = match info.get("index").filter(|value| !value.is_null()) {
                    Some(value) => Some(
                        value
                            .as_u64()
                            .filter(|value| *value <= 1_000_000)
                            .ok_or_else(|| invalid("Invalid songlist index"))?
                            as usize,
                    ),
                    None => None,
                };
                (source, id, url, index)
            } else {
                let source = paths
                    .first()
                    .ok_or_else(|| invalid("Songlist source missing"))?
                    .clone();
                let url = paths
                    .get(1)
                    .ok_or_else(|| invalid("Songlist URL missing"))?
                    .clone();
                let index = paths
                    .get(2)
                    .and_then(|value| js_parse_int(value))
                    .map(|value| {
                        usize::try_from(value)
                            .ok()
                            .filter(|value| *value <= 1_000_000)
                            .ok_or_else(|| invalid("Invalid songlist index"))
                    })
                    .transpose()?;
                (source, None, Some(url), index)
            };
            if !SOURCES.contains(&source.as_str())
                || id.as_ref().is_none_or(String::is_empty)
                    && url.as_ref().is_none_or(String::is_empty)
            {
                return Err(invalid("Invalid songlist source or identity"));
            }
            if let Some(url) = url.as_ref() {
                metadata_url(url)?;
            }
            match action {
                "open" => DeepLinkAction::SonglistOpen { source, id, url },
                "play" => DeepLinkAction::SonglistPlay {
                    source,
                    id,
                    url,
                    index,
                },
                _ => return Err(invalid("Unknown songlist action")),
            }
        }
        _ => return Err(invalid("Unknown deeplink type")),
    };
    Ok(ParsedDeepLink { action })
}
fn parse_music(info: &Value) -> ControlResult<DeepLinkMusicInfo> {
    let source = text(info, "source", true, 64)?.unwrap();
    if !SOURCES.contains(&source.as_str()) {
        return Err(invalid("Unknown remote music source"));
    }
    let mut wire = Map::new();
    wire.insert("source".into(), Value::String(source.clone()));
    for (key, max, required) in [
        ("name", 200, true),
        ("singer", 200, true),
        ("img", 1024, false),
        ("interval", 64, false),
        ("albumName", 200, false),
    ] {
        if let Some(mut value) = text(info, key, required, max)? {
            if ["name", "singer", "albumName"].contains(&key) {
                value = super::html_text::body_text(&value)?;
            }
            if key == "img" && !value.is_empty() {
                metadata_url(&value)?;
            }
            wire.insert(key.into(), Value::String(value));
        }
    }
    for (key, required) in [("songmid", true), ("albumId", false)] {
        if string_or_number(info, key, required, 64)?.is_some() {
            wire.insert(key.into(), info[key].clone());
        }
    }
    let extras: &[(&str, bool, bool)] = match source.as_str() {
        "kg" => &[("hash", true, false)],
        "tx" => &[("strMediaMid", true, false), ("albumMid", false, false)],
        "mg" => &[
            ("copyrightId", true, true),
            ("lrcUrl", false, false),
            ("trcUrl", false, false),
            ("mrcUrl", false, false),
        ],
        _ => &[],
    };
    for &(key, required, number) in extras {
        let max = if key.ends_with("Url") { 1024 } else { 64 };
        let value = if number {
            string_or_number(info, key, required, max)?
        } else {
            text(info, key, required, max)?
        };
        if let Some(value) = value {
            if key.ends_with("Url") && !value.is_empty() {
                metadata_url(&value)?;
            }
            wire.insert(key.into(), info[key].clone());
        }
    }
    if source == "kg"
        && let Some(value) = info.get("_interval").filter(|value| !value.is_null())
    {
        if !value.is_number() || value.to_string().len() > 64 {
            return Err(invalid("Invalid music interval"));
        }
        wire.insert("_interval".into(), value.clone());
    }
    let types = info
        .get("types")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("Music quality missing"))?;
    if types.len() > 128 {
        return Err(invalid("Too many music qualities"));
    }
    let mut qualities = Vec::new();
    for quality in types {
        let Some(kind) = quality
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| ["128k", "320k", "flac", "flac24bit"].contains(value))
        else {
            continue;
        };
        let mut selected = Map::new();
        selected.insert("type".into(), Value::String(kind.into()));
        if let Some(size) = text(quality, "size", false, 4096)? {
            selected.insert("size".into(), Value::String(size));
        }
        if source == "kg" {
            let hash = text(quality, "hash", true, 4096)?.unwrap();
            selected.insert("hash".into(), Value::String(hash));
        } else if let Some(hash) = quality.get("hash").filter(|value| !value.is_null()) {
            if !hash.is_string() {
                return Err(invalid("Invalid quality hash"));
            }
            selected.insert("hash".into(), hash.clone());
        }
        qualities.push(Value::Object(selected));
    }
    if qualities.is_empty() {
        return Err(invalid("No supported music quality"));
    }
    wire.insert("types".into(), Value::Array(qualities));
    Ok(DeepLinkMusicInfo {
        source,
        wire: Value::Object(wire),
    })
}
fn text(info: &Value, key: &str, required: bool, max: usize) -> ControlResult<Option<String>> {
    let Some(value) = info.get(key).filter(|value| !value.is_null()) else {
        return if required {
            Err(invalid("Required deeplink field missing"))
        } else {
            Ok(None)
        };
    };
    let value = value
        .as_str()
        .ok_or_else(|| invalid("Invalid deeplink field type"))?;
    if value.encode_utf16().count() > max || value.contains('\0') {
        return Err(invalid("Invalid deeplink field length"));
    }
    Ok(Some(value.into()))
}
fn string_or_number(
    info: &Value,
    key: &str,
    required: bool,
    max: usize,
) -> ControlResult<Option<String>> {
    match info.get(key).filter(|value| !value.is_null()) {
        Some(Value::Number(number)) => {
            let value = ryu_js::Buffer::new()
                .format(
                    number
                        .as_f64()
                        .ok_or_else(|| invalid("Invalid numeric deeplink identity"))?,
                )
                .to_owned();
            if value.len() > max {
                return Err(invalid("Invalid numeric deeplink identity"));
            }
            Ok(Some(value))
        }
        _ => text(info, key, required, max),
    }
}
fn boolean(info: &Value, key: &str) -> ControlResult<Option<bool>> {
    info.get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| invalid("Invalid deeplink boolean"))
        })
        .transpose()
}
fn metadata_url(value: &str) -> ControlResult<()> {
    let url = reqwest::Url::parse(value).map_err(|_| invalid("Invalid metadata URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(invalid("Unsafe metadata URL"));
    }
    Ok(())
}
fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}
fn truncate_utf16(value: &str, max: usize) -> String {
    let mut count = 0;
    value
        .chars()
        .take_while(|value| {
            count += value.len_utf16();
            count <= max
        })
        .collect()
}
