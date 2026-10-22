use super::model::{invalid, quota};
use super::{MAX_JSON_BYTES, MAX_WIRE_BYTES, SyncResult};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::io::{Read, Write};
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub enum RpcFrame {
    Request {
        name: String,
        path: Vec<String>,
        data: Vec<Value>,
    },
    Reply {
        name: String,
        error: Option<String>,
        data: Option<Value>,
    },
}
const METHODS: [&str; 15] = [
    "getEnabledFeatures",
    "finished",
    "onFeatureChanged",
    "onListSyncAction",
    "list_sync_get_md5",
    "list_sync_get_sync_mode",
    "list_sync_get_list_data",
    "list_sync_set_list_data",
    "list_sync_finished",
    "onDislikeSyncAction",
    "dislike_sync_get_md5",
    "dislike_sync_get_sync_mode",
    "dislike_sync_get_list_data",
    "dislike_sync_set_list_data",
    "dislike_sync_finished",
];

pub fn encode_message(json: &str) -> SyncResult<String> {
    if json.len() > MAX_JSON_BYTES {
        return Err(quota("Sync JSON exceeds byte budget"));
    }
    if json.encode_utf16().count() <= 1024 {
        return Ok(json.into());
    }
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(json.as_bytes())
        .map_err(|_| invalid("Cannot compress sync JSON"))?;
    let compressed = gzip
        .finish()
        .map_err(|_| invalid("Cannot compress sync JSON"))?;
    let encoded = format!("cg_{}", STANDARD.encode(compressed));
    if encoded.len() > MAX_WIRE_BYTES {
        return Err(quota("Sync wire exceeds byte budget"));
    }
    Ok(encoded)
}
pub fn decode_message(wire: &str) -> SyncResult<String> {
    if wire.len() > MAX_WIRE_BYTES {
        return Err(quota("Sync wire exceeds byte budget"));
    }
    let bytes = if let Some(data) = wire.strip_prefix("cg_") {
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| invalid("Invalid sync base64"))?;
        let mut decoder = MultiGzDecoder::new(bytes.as_slice());
        let mut output = Vec::new();
        decoder
            .by_ref()
            .take((MAX_JSON_BYTES + 1) as u64)
            .read_to_end(&mut output)
            .map_err(|_| invalid("Invalid sync gzip"))?;
        if output.len() > MAX_JSON_BYTES {
            return Err(quota("Sync gzip expands beyond budget"));
        }
        output
    } else {
        if wire.len() > MAX_JSON_BYTES {
            return Err(quota("Sync JSON exceeds byte budget"));
        }
        wire.as_bytes().to_vec()
    };
    String::from_utf8(bytes).map_err(|_| invalid("Sync message is not UTF-8"))
}
pub fn parse_frame(wire: &str) -> SyncResult<RpcFrame> {
    let json = decode_message(wire)?;
    let value = bounded_json(&json)?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("RPC frame must be an object"))?;
    super::model::allowed_keys(object, &["name", "path", "error", "data"])?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.len() <= 256 && !name.contains('\0'))
        .ok_or_else(|| invalid("Invalid RPC correlation name"))?
        .to_owned();
    if let Some(path) = object
        .get("path")
        .and_then(Value::as_array)
        .filter(|path| !path.is_empty())
    {
        if object.contains_key("error") || path.len() != 1 {
            return Err(invalid("Unsupported RPC path"));
        }
        let method = path[0]
            .as_str()
            .filter(|method| METHODS.contains(method))
            .ok_or_else(|| invalid("Unsupported RPC method"))?;
        let data = object
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("RPC parameters must be an array"))?;
        if data.len() > 2 {
            return Err(quota("Too many RPC parameters"));
        }
        validate_parameters(method, data)?;
        Ok(RpcFrame::Request {
            name,
            path: vec![method.into()],
            data: data.clone(),
        })
    } else {
        if object.contains_key("path") {
            return Err(invalid("Empty RPC request path"));
        }
        let error = match object.get("error") {
            None | Some(Value::Null) => None,
            Some(Value::String(error)) if error.len() <= 1024 => Some(error.clone()),
            _ => return Err(invalid("Invalid RPC error")),
        };
        Ok(RpcFrame::Reply {
            name,
            error,
            data: object.get("data").cloned(),
        })
    }
}
fn validate_parameters(method: &str, data: &[Value]) -> SyncResult<()> {
    match method {
        "getEnabledFeatures" => {
            if data.len() != 2
                || !data[0]
                    .as_str()
                    .is_some_and(|kind| ["desktop-app", "server"].contains(&kind))
            {
                return Err(invalid("Invalid feature negotiation"));
            }
            let features = data[1]
                .as_object()
                .ok_or_else(|| invalid("Invalid feature versions"))?;
            super::model::allowed_keys(features, &["list", "dislike"])?;
            if features.values().any(|version| {
                version
                    .as_f64()
                    .is_none_or(|value| value < 0.0 || value.fract() != 0.0 || value > 65535.0)
            }) {
                return Err(invalid("Invalid feature version"));
            }
        }
        "onFeatureChanged" => {
            if data.len() != 1 {
                return Err(invalid("Invalid RPC parameters"));
            }
            super::PeerCapabilities::from_wire(&data[0])?;
        }
        "onListSyncAction" => {
            if data.len() != 1 {
                return Err(invalid("Invalid RPC parameters"));
            }
            super::ListAction::from_wire(&data[0])?;
        }
        "onDislikeSyncAction" => {
            if data.len() != 1 {
                return Err(invalid("Invalid RPC parameters"));
            }
            super::DislikeAction::from_wire(&data[0])?;
        }
        "list_sync_set_list_data" => {
            if data.len() != 1 {
                return Err(invalid("Invalid RPC parameters"));
            }
            super::ListData::from_wire(&data[0])?;
        }
        "dislike_sync_set_list_data" => {
            if data.len() != 1 {
                return Err(invalid("Invalid RPC parameters"));
            }
            super::merge::validate_rules(
                data[0]
                    .as_str()
                    .ok_or_else(|| invalid("Dislike data must be a string"))?,
            )?;
        }
        _ => {
            if !data.is_empty() {
                return Err(invalid("Unexpected RPC parameters"));
            }
        }
    }
    Ok(())
}
pub fn serialize_frame(frame: &RpcFrame) -> SyncResult<String> {
    let mut object = Map::new();
    match frame {
        RpcFrame::Request { name, path, data } => {
            object.insert("name".into(), Value::String(name.clone()));
            object.insert(
                "path".into(),
                Value::Array(path.iter().cloned().map(Value::String).collect()),
            );
            object.insert("data".into(), Value::Array(data.clone()));
        }
        RpcFrame::Reply { name, error, data } => {
            object.insert("name".into(), Value::String(name.clone()));
            object.insert(
                "error".into(),
                error
                    .as_ref()
                    .map(|error| Value::String(error.clone()))
                    .unwrap_or(Value::Null),
            );
            if let Some(data) = data {
                object.insert("data".into(), data.clone());
            }
        }
    }
    let json = js_json(&Value::Object(object))?;
    let encoded = encode_message(&json)?;
    parse_frame(&encoded)?;
    Ok(encoded)
}
/// Matches JSON.stringify number and own-property ordering for finite wire data.
pub(crate) fn js_json(value: &Value) -> SyncResult<String> {
    fn append(value: &Value, out: &mut String, depth: usize) -> SyncResult<()> {
        if depth > 32 {
            return Err(quota("Sync JSON nesting exceeds budget"));
        }
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Value::String(value) => out.push_str(
                &serde_json::to_string(value).map_err(|_| invalid("Invalid JSON string"))?,
            ),
            Value::Number(number) => {
                let number = number
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| invalid("Invalid JSON number"))?;
                out.push_str(ryu_js::Buffer::new().format_finite(number));
            }
            Value::Array(array) => {
                out.push('[');
                for (index, value) in array.iter().enumerate() {
                    if index != 0 {
                        out.push(',');
                    }
                    append(value, out, depth + 1)?;
                }
                out.push(']');
            }
            Value::Object(object) => {
                out.push('{');
                let mut indexed = object
                    .iter()
                    .filter_map(|(key, value)| {
                        key.parse::<u32>()
                            .ok()
                            .filter(|index| *index < u32::MAX && index.to_string() == *key)
                            .map(|index| (index, key, value))
                    })
                    .collect::<Vec<_>>();
                indexed.sort_by_key(|(index, _, _)| *index);
                let mut first = true;
                for (_, key, value) in indexed {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    out.push_str(&serde_json::to_string(key).unwrap());
                    out.push(':');
                    append(value, out, depth + 1)?;
                }
                for (key, value) in object {
                    if key
                        .parse::<u32>()
                        .ok()
                        .is_some_and(|index| index < u32::MAX && index.to_string() == *key)
                    {
                        continue;
                    }
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    out.push_str(&serde_json::to_string(key).unwrap());
                    out.push(':');
                    append(value, out, depth + 1)?;
                }
                out.push('}');
            }
        }
        if out.len() > MAX_JSON_BYTES {
            return Err(quota("Sync JSON exceeds byte budget"));
        }
        Ok(())
    }
    let mut output = String::new();
    append(value, &mut output, 0)?;
    Ok(output)
}
pub(crate) fn bounded_json(json: &str) -> SyncResult<Value> {
    if json.len() > MAX_JSON_BYTES {
        return Err(quota("Sync JSON exceeds byte budget"));
    }
    let mut parser = serde_json::Deserializer::from_str(json);
    let seed = Budget {
        remaining: Rc::new(Cell::new(262144)),
        depth: 0,
    };
    let value = seed
        .deserialize(&mut parser)
        .map_err(|_| invalid("Invalid or oversized sync JSON"))?;
    parser.end().map_err(|_| invalid("Trailing JSON data"))?;
    Ok(value)
}
#[derive(Clone)]
struct Budget {
    remaining: Rc<Cell<usize>>,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Budget {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, parser: D) -> Result<Value, D::Error> {
        if self.depth > 32 || self.remaining.get() == 0 {
            return Err(de::Error::custom("JSON budget"));
        }
        self.remaining.set(self.remaining.get() - 1);
        parser.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Budget {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        self.visit_f64(value as f64)
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        self.visit_f64(value as f64)
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        // JS has one Number type. Canonicalize exactly representable integral
        // values so 1 and 1.0 share a wire/data value without preserving Rust's
        // parser-specific integer-versus-float distinction.
        if value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
            return Ok(Value::from(value as i64));
        }
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("nonfinite JSON"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.into()))
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut result = Vec::new();
        let child = Budget {
            remaining: self.remaining,
            depth: self.depth + 1,
        };
        while let Some(value) = sequence.next_element_seed(child.clone())? {
            result.push(value);
        }
        Ok(Value::Array(result))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Value, A::Error> {
        let mut result = Map::new();
        let child = Budget {
            remaining: self.remaining,
            depth: self.depth + 1,
        };
        while let Some(key) = object.next_key::<String>()? {
            if result.contains_key(&key) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            let value = object.next_value_seed(child.clone())?;
            result.insert(key, value);
        }
        Ok(Value::Object(result))
    }
}
