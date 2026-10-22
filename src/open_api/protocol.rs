use super::{
    ArtworkReference, ControlAction, ControlError, ControlErrorKind, ControlResult, LxPlayerStatus,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const STATUS_FIELDS: [&str; 17] = [
    "status",
    "name",
    "singer",
    "albumName",
    "picUrl",
    "progress",
    "duration",
    "playbackRate",
    "lyricLineText",
    "lyricLineAllText",
    "lyric",
    "tlyric",
    "rlyric",
    "lxlyric",
    "collect",
    "volume",
    "mute",
];
pub const DEFAULT_FILTER: [&str; 8] = [
    "status",
    "name",
    "singer",
    "albumName",
    "lyricLineText",
    "duration",
    "progress",
    "playbackRate",
];
pub(crate) const MAX_STATUS_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    fields: Vec<&'static str>,
}
impl Filter {
    pub fn fields(&self) -> &[&'static str] {
        &self.fields
    }
    pub(crate) fn includes(&self, field: &str) -> bool {
        self.fields.contains(&field)
    }
    pub fn project(&self, status: &LxPlayerStatus) -> ControlResult<Vec<u8>> {
        let value = status_value(status)?;
        let mut selected = String::from("{");
        for field in &self.fields {
            if selected.len() > 1 {
                selected.push(',');
            }
            selected.push_str(&serde_json::to_string(field).map_err(json_error)?);
            selected.push(':');
            selected.push_str(&json_scalar(&value[*field])?);
            if selected.len() > MAX_STATUS_BYTES {
                return Err(too_large());
            }
        }
        selected.push('}');
        if selected.len() > MAX_STATUS_BYTES {
            return Err(too_large());
        }
        Ok(selected.into_bytes())
    }
    pub fn frames(
        &self,
        status: &LxPlayerStatus,
        previous: Option<&LxPlayerStatus>,
    ) -> ControlResult<Vec<(String, Vec<u8>)>> {
        let value = status_value(status)?;
        let old = previous.map(status_value).transpose()?;
        let mut bytes = 0;
        let mut result = Vec::new();
        for field in STATUS_FIELDS {
            if !self.includes(field) || old.as_ref().is_some_and(|old| old[field] == value[field]) {
                continue;
            }
            let scalar = json_scalar(&value[field])?;
            let frame = format!("event: {field}\ndata: {scalar}\n\n").into_bytes();
            bytes += frame.len();
            if bytes > MAX_STATUS_BYTES {
                return Err(too_large());
            }
            result.push((field.into(), frame));
        }
        Ok(result)
    }
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            fields: DEFAULT_FILTER.to_vec(),
        }
    }
}
pub fn parse_filter(value: Option<&str>) -> Filter {
    let Some(value) = value else {
        return Filter::default();
    };
    let fields: Vec<_> = STATUS_FIELDS
        .into_iter()
        .filter(|field| value.split(',').any(|requested| requested == *field))
        .collect();
    if fields.is_empty() {
        Filter::default()
    } else {
        Filter { fields }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Status(Filter),
    Lyric,
    LyricAll,
    Subscribe(Filter),
    Control(ControlAction),
    Artwork(ArtworkReference),
    Unknown,
}
pub fn parse_route(target: &str) -> ControlResult<Route> {
    if target.len() > 8192 || target.contains(['\0', '\r', '\n', '#']) || !target.starts_with('/') {
        return Err(invalid("Invalid request target"));
    }
    if target.starts_with("/artwork/") {
        let parts: Vec<_> = target.split('/').collect();
        if parts.len() != 7 || parts[2] != "v1" {
            return Err(invalid("Invalid artwork reference"));
        }
        let number = |value: &str| -> ControlResult<u64> {
            if value.is_empty()
                || value.len() > 20
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid("Invalid artwork reference"));
            }
            let parsed = value
                .parse::<u64>()
                .map_err(|_| invalid("Invalid artwork reference"))?;
            if parsed.to_string() != value {
                return Err(invalid("Invalid artwork reference"));
            }
            Ok(parsed)
        };
        let reference = ArtworkReference {
            session_epoch: number(parts[3])?,
            generation: number(parts[4])?,
            key_sha256: parts[5].into(),
            sha256: parts[6].into(),
        };
        reference.validate()?;
        return Ok(Route::Artwork(reference));
    }
    // LX selects the last slash segment before splitting its query.
    let segment = target.rsplit('/').next().unwrap_or_default();
    let (name, query) = segment.split_once('?').unwrap_or((segment, ""));
    let query = parse_query(query)?;
    let numeric = |key| {
        query
            .get(key)
            .map(|values| values.join(","))
            .unwrap_or_default()
    };
    let filter = || {
        parse_filter(
            query
                .get("filter")
                .filter(|values| values.len() == 1)
                .map(|values| values[0].as_str()),
        )
    };
    Ok(match name {
        "status" => Route::Status(filter()),
        "lyric" => Route::Lyric,
        "lyric-all" => Route::LyricAll,
        "subscribe-player-status" => Route::Subscribe(filter()),
        "play" => Route::Control(ControlAction::Play),
        "pause" => Route::Control(ControlAction::Pause),
        "skip-next" => Route::Control(ControlAction::Next),
        "skip-prev" => Route::Control(ControlAction::Previous),
        "collect" => Route::Control(ControlAction::CollectCurrent { collected: true }),
        "uncollect" => Route::Control(ControlAction::CollectCurrent { collected: false }),
        "seek" => {
            let value = js_parse_float(&numeric("offset"))
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| invalid("Invalid offset"))?;
            Route::Control(ControlAction::SeekChecked {
                requested_seconds: value,
                seconds: js_round_millis(value),
            })
        }
        "volume" => {
            let value = js_parse_int(&numeric("volume"))
                .filter(|v| (0..=100).contains(v))
                .ok_or_else(|| invalid("Invalid volume"))?;
            Route::Control(ControlAction::SetVolume {
                percent: value as u8,
            })
        }
        "mute" => {
            let value = query
                .get("mute")
                .filter(|v| v.len() == 1)
                .and_then(|v| match v[0].as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                })
                .ok_or_else(|| invalid("Invalid mute value"))?;
            Route::Control(ControlAction::SetMute { muted: value })
        }
        _ => Route::Unknown,
    })
}
pub(crate) fn status_value(status: &LxPlayerStatus) -> ControlResult<Value> {
    if !status.progress.is_finite()
        || !status.duration.is_finite()
        || !status.playback_rate.is_finite()
        || status.progress < 0.0
        || status.duration < 0.0
        || status.playback_rate <= 0.0
        || status.volume > 100
    {
        return Err(invalid("Invalid player status"));
    }
    // Validate before serde's allocation. Large unselected lyric bodies can still
    // be returned by /lyric; selected SSE/projected output has a separate budget.
    if [
        &status.name,
        &status.singer,
        &status.album_name,
        &status.pic_url,
        &status.lyric_line_text,
        &status.lyric_line_all_text,
        &status.lyric,
        &status.tlyric,
        &status.rlyric,
        &status.lxlyric,
    ]
    .iter()
    .map(|s| s.len())
    .sum::<usize>()
        > 2 * 1024 * 1024
    {
        return Err(too_large());
    }
    let mut value = serde_json::to_value(status).map_err(json_error)?;
    for field in ["progress", "duration", "playbackRate"] {
        if let Some(number) = value[field].as_f64()
            && number.fract() == 0.0
            && number.abs() <= 9_007_199_254_740_991.0
        {
            value[field] = Value::from(number as i64);
        }
    }
    Ok(value)
}
pub(crate) fn lyrics(status: &LxPlayerStatus) -> ControlResult<Vec<u8>> {
    status_value(status)?;
    let mut value = Map::new();
    for (name, lyric) in [
        ("lyric", &status.lyric),
        ("tlyric", &status.tlyric),
        ("rlyric", &status.rlyric),
        ("lxlyric", &status.lxlyric),
    ] {
        value.insert(name.into(), Value::String(lyric.clone()));
    }
    bounded_json(&Value::Object(value))
}
fn bounded_json(value: &Value) -> ControlResult<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(json_error)?;
    if bytes.len() > MAX_STATUS_BYTES {
        return Err(too_large());
    }
    Ok(bytes)
}
pub(crate) fn too_large() -> ControlError {
    invalid("Response exceeds resource budget")
}
fn json_error(_: serde_json::Error) -> ControlError {
    invalid("Invalid player status")
}
fn json_scalar(value: &Value) -> ControlResult<String> {
    if let Value::Number(number) = value {
        let number = number
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(|| invalid("Invalid player status"))?;
        return Ok(ryu_js::Buffer::new().format_finite(number).to_owned());
    }
    serde_json::to_string(value).map_err(json_error)
}
pub(crate) fn invalid(message: &str) -> ControlError {
    ControlError::new(ControlErrorKind::InvalidData, message)
}

fn parse_query(query: &str) -> ControlResult<BTreeMap<String, Vec<String>>> {
    let mut result = BTreeMap::<String, Vec<String>>::new();
    if query.is_empty() {
        return Ok(result);
    }
    if query.split('&').count() > 32 {
        return Err(invalid("Too many query values"));
    }
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key, true)?;
        let value = percent_decode(value, true)?;
        if key.len() > 4096 || value.len() > 4096 {
            return Err(invalid("Query value too large"));
        }
        result.entry(key).or_default().push(value);
    }
    Ok(result)
}
pub(crate) fn percent_decode(input: &str, plus_space: bool) -> ControlResult<String> {
    let bytes = input.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut pos = 0;
    while pos < bytes.len() {
        match bytes[pos] {
            b'%' => {
                if pos + 2 >= bytes.len() {
                    return Err(invalid("Invalid URI encoding"));
                }
                let hi = hex(bytes[pos + 1]).ok_or_else(|| invalid("Invalid URI encoding"))?;
                let lo = hex(bytes[pos + 2]).ok_or_else(|| invalid("Invalid URI encoding"))?;
                result.push(hi * 16 + lo);
                pos += 3;
            }
            b'+' if plus_space => {
                result.push(b' ');
                pos += 1;
            }
            value => {
                result.push(value);
                pos += 1;
            }
        }
    }
    let result = String::from_utf8(result).map_err(|_| invalid("Invalid URI encoding"))?;
    if result.contains('\0') {
        return Err(invalid("Invalid URI encoding"));
    }
    Ok(result)
}
fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
pub(crate) fn js_parse_float(value: &str) -> Option<f64> {
    let value = value.trim_start_matches(js_whitespace);
    if value.starts_with("Infinity") || value.starts_with("+Infinity") {
        return Some(f64::INFINITY);
    }
    if value.starts_with("-Infinity") {
        return Some(f64::NEG_INFINITY);
    }
    let bytes = value.as_bytes();
    let mut pos = usize::from(bytes.first().is_some_and(|b| *b == b'+' || *b == b'-'));
    let mut digits = 0;
    while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
        pos += 1;
        digits += 1;
    }
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if bytes.get(pos).is_some_and(|b| *b == b'e' || *b == b'E') {
        let exponent = pos;
        pos += 1;
        if bytes.get(pos).is_some_and(|b| *b == b'+' || *b == b'-') {
            pos += 1;
        }
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == start {
            pos = exponent;
        }
    }
    value[..pos].parse().ok()
}
pub(crate) fn js_parse_int(value: &str) -> Option<i64> {
    let value = value.trim_start_matches(js_whitespace);
    let (negative, value) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let (radix, value) = if value.starts_with("0x") || value.starts_with("0X") {
        (16u32, &value[2..])
    } else {
        (10, value)
    };
    let mut number = 0i64;
    let mut any = false;
    for byte in value.bytes() {
        let Some(digit) = hex(byte).filter(|digit| u32::from(*digit) < radix) else {
            break;
        };
        number = number
            .checked_mul(i64::from(radix))?
            .checked_add(i64::from(digit))?;
        any = true;
    }
    any.then_some(if negative { -number } else { number })
}
fn js_round_millis(value: f64) -> f64 {
    ryu_js::Buffer::new()
        .format_to_fixed(value, 3)
        .parse()
        .unwrap_or(value)
}
fn js_whitespace(value: char) -> bool {
    matches!(value, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
