use std::fmt;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::sync::{ListData, MusicInfo, SyncError, SyncErrorKind, SyncResult};

const MAX_TREE_DEPTH: usize = 32;
const MAX_TREE_NODES: usize = 262_144;
const MAX_DOM_BYTES: usize = 64 * 1024 * 1024;
const ALLOCATION_OVERHEAD: usize = 32;
const VALUE_BYTES: usize = size_of::<Value>();

#[derive(Clone)]
pub(super) struct Budget {
    inner: Arc<BudgetState>,
}

struct BudgetState {
    limit: usize,
    used: AtomicUsize,
}

/// An owned reservation; dropping it returns exactly its remaining bytes.
pub(super) struct Credit {
    budget: Arc<BudgetState>,
    bytes: usize,
}

impl Budget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(BudgetState {
                limit,
                used: AtomicUsize::new(0),
            }),
        }
    }

    pub(super) fn used(&self) -> usize {
        self.inner.used.load(Ordering::Acquire)
    }

    pub(super) fn reserve(&self, bytes: usize) -> SyncResult<Credit> {
        self.inner
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.inner.limit)
            })
            .map_err(|_| quota("Sync memory budget exhausted"))?;
        Ok(Credit {
            budget: Arc::clone(&self.inner),
            bytes,
        })
    }
}

impl Credit {
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn shrink(&mut self, bytes: usize) -> SyncResult<()> {
        if bytes > self.bytes {
            return Err(SyncError::new(
                SyncErrorKind::InvalidData,
                "A memory credit cannot grow",
            ));
        }
        let returned = self.bytes - bytes;
        self.bytes = bytes;
        self.budget.used.fetch_sub(returned, Ordering::AcqRel);
        Ok(())
    }
}

impl Drop for Credit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

fn quota(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::Quota, message)
}

fn allocation(capacity: usize, element_bytes: usize) -> usize {
    if capacity == 0 {
        0
    } else {
        capacity
            .saturating_mul(element_bytes)
            .saturating_add(ALLOCATION_OVERHEAD)
    }
}

fn growth_slots(length: usize) -> usize {
    if length == 0 {
        0
    } else {
        length
            .checked_next_power_of_two()
            .unwrap_or(usize::MAX)
            .max(4)
    }
}

fn scanned_array_layout(length: usize) -> usize {
    allocation(growth_slots(length).saturating_mul(2), VALUE_BYTES)
}

// serde_json::Map hides IndexMap capacity. Account for its next growth bucket,
// key/value storage, indices and hash-table overhead with twofold headroom.
fn map_layout(length: usize) -> usize {
    let entry_bytes = size_of::<String>()
        .saturating_add(VALUE_BYTES)
        .saturating_add(64);
    allocation(growth_slots(length).saturating_mul(2), entry_bytes)
}

fn scanned_string_layout(length: usize) -> usize {
    length.saturating_mul(2).saturating_add(ALLOCATION_OVERHEAD)
}

/// Includes container capacity and descendant allocations without cloning.
pub(super) fn value_footprint(value: &Value) -> usize {
    value_footprint_at(value, 0)
}

fn value_footprint_at(value: &Value, depth: usize) -> usize {
    if depth > MAX_TREE_DEPTH {
        return usize::MAX;
    }
    let payload = match value {
        Value::String(text) => allocation(text.capacity(), 1),
        Value::Array(values) => values.iter().fold(
            allocation(values.capacity(), VALUE_BYTES),
            |total, value| total.saturating_add(value_footprint_at(value, depth + 1)),
        ),
        Value::Object(values) => {
            values
                .iter()
                .fold(map_layout(values.len()), |total, (key, value)| {
                    total
                        .saturating_add(allocation(key.capacity(), 1))
                        .saturating_add(value_footprint_at(value, depth + 1))
                })
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    };
    VALUE_BYTES.saturating_add(payload)
}

fn music_footprint(music: &Vec<MusicInfo>) -> usize {
    music.iter().fold(
        allocation(music.capacity(), size_of::<MusicInfo>()),
        |total, music| total.saturating_add(value_footprint(music.wire())),
    )
}

pub(super) fn list_footprint(data: &ListData) -> usize {
    let mut bytes = size_of::<ListData>()
        .saturating_add(music_footprint(&data.default_list))
        .saturating_add(music_footprint(&data.love_list))
        .saturating_add(allocation(
            data.user_list.capacity(),
            size_of::<crate::sync::UserList>(),
        ));
    for list in &data.user_list {
        bytes = bytes
            .saturating_add(allocation(list.id.capacity(), 1))
            .saturating_add(value_footprint(&list.name))
            .saturating_add(music_footprint(&list.list));
        for value in [
            &list.source,
            &list.source_list_id,
            &list.location_update_time,
        ]
        .into_iter()
        .flatten()
        {
            bytes = bytes.saturating_add(value_footprint(value));
        }
    }
    bytes
}

struct ScanStats {
    nodes: usize,
    bytes: usize,
    quota_exceeded: bool,
}

impl ScanStats {
    fn reject<E: de::Error>(&mut self, message: &'static str) -> Result<(), E> {
        self.quota_exceeded = true;
        Err(E::custom(message))
    }

    fn enter_node<E: de::Error>(&mut self) -> Result<(), E> {
        if self.nodes >= MAX_TREE_NODES {
            return self.reject("JSON node budget exceeded");
        }
        self.nodes += 1;
        Ok(())
    }

    fn enter_value<E: de::Error>(&mut self, depth: usize) -> Result<(), E> {
        if depth > MAX_TREE_DEPTH {
            return self.reject("JSON nesting budget exceeded");
        }
        self.enter_node()?;
        self.charge(VALUE_BYTES)
    }

    fn charge<E: de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        let Some(total) = self.bytes.checked_add(bytes) else {
            return self.reject("JSON allocation budget exceeded");
        };
        if total > MAX_DOM_BYTES {
            return self.reject("JSON allocation budget exceeded");
        }
        self.bytes = total;
        Ok(())
    }

    fn string<E: de::Error>(&mut self, length: usize) -> Result<(), E> {
        self.charge(scanned_string_layout(length))
    }
}

struct TreeSeed<'a> {
    stats: &'a mut ScanStats,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for TreeSeed<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        self.stats.enter_value(self.depth)?;
        deserializer.deserialize_any(TreeVisitor {
            stats: self.stats,
            depth: self.depth,
        })
    }
}

struct TreeVisitor<'a> {
    stats: &'a mut ScanStats,
    depth: usize,
}

impl<'de> Visitor<'de> for TreeVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, _value: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _value: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _value: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, _value: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        self.stats.string(value.len())
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<(), E> {
        self.visit_str(value)
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<(), E> {
        self.stats.string(value.capacity().max(value.len()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        let Self { stats, depth } = self;
        let mut length = 0usize;
        while sequence
            .next_element_seed(TreeSeed {
                stats: &mut *stats,
                depth: depth + 1,
            })?
            .is_some()
        {
            length += 1;
        }
        stats.charge(scanned_array_layout(length))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let Self { stats, depth } = self;
        let mut length = 0usize;
        while map.next_key_seed(KeySeed { stats: &mut *stats })?.is_some() {
            map.next_value_seed(TreeSeed {
                stats: &mut *stats,
                depth: depth + 1,
            })?;
            length += 1;
        }
        stats.charge(map_layout(length))
    }
}

struct KeySeed<'a> {
    stats: &'a mut ScanStats,
}

impl<'de> DeserializeSeed<'de> for KeySeed<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        self.stats.enter_node()?;
        deserializer.deserialize_str(KeyVisitor { stats: self.stats })
    }
}

struct KeyVisitor<'a> {
    stats: &'a mut ScanStats,
}

impl<'de> Visitor<'de> for KeyVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object key")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        self.stats.charge(size_of::<String>())?;
        self.stats.string(value.len())
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<(), E> {
        self.visit_str(value)
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<(), E> {
        self.stats.charge(size_of::<String>())?;
        self.stats.string(value.capacity().max(value.len()))
    }
}

/// Scans JSON without constructing its value tree. Object keys count toward
/// the node bound; string decoding uses only the deserializer's scratch space.
pub(super) fn estimate_json_tree(json: &str) -> SyncResult<usize> {
    if json.len() > crate::sync::MAX_JSON_BYTES {
        return Err(quota("JSON input budget exceeded"));
    }
    let mut stats = ScanStats {
        nodes: 0,
        bytes: 0,
        quota_exceeded: false,
    };
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let result = TreeSeed {
        stats: &mut stats,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .and_then(|()| deserializer.end());
    match result {
        Ok(()) => Ok(stats.bytes),
        Err(_) if stats.quota_exceeded => Err(quota("JSON tree budget exceeded")),
        Err(_) => Err(SyncError::new(
            SyncErrorKind::InvalidData,
            "Invalid JSON sync payload",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn owned_credits_reserve_shrink_and_return_actual_bytes() {
        let budget = Budget::new(32);
        let mut first = budget.reserve(12).unwrap();
        let second = budget.clone().reserve(20).unwrap();
        assert_eq!(budget.used(), 32);
        assert!(matches!(
            budget.reserve(1),
            Err(SyncError {
                kind: SyncErrorKind::Quota,
                ..
            })
        ));
        assert_eq!(budget.used(), 32);
        first.shrink(4).unwrap();
        assert_eq!(first.bytes(), 4);
        assert_eq!(budget.used(), 24);
        assert!(first.shrink(5).is_err());
        assert_eq!(budget.used(), 24);
        drop(second);
        assert_eq!(budget.used(), 4);
        drop(first);
        assert_eq!(budget.used(), 0);
        let empty = budget.reserve(0).unwrap();
        drop(empty);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_failed_multi_reservation_rolls_back_prior_credits() {
        fn transaction(budget: &Budget) -> SyncResult<()> {
            let _first = budget.reserve(24)?;
            let _second = budget.reserve(24)?;
            Ok(())
        }
        let budget = Budget::new(32);
        assert!(transaction(&budget).is_err());
        assert_eq!(budget.used(), 0);
        let whole = budget.reserve(32).unwrap();
        drop(whole);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_shared_limit() {
        let budget = Budget::new(64);
        let barrier = Arc::new(Barrier::new(9));
        let workers = (0..8)
            .map(|_| {
                let budget = budget.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let credit = budget.reserve(16);
                    let reserved = credit.is_ok();
                    barrier.wait();
                    barrier.wait();
                    drop(credit);
                    reserved
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        barrier.wait();
        assert_eq!(budget.used(), 64);
        barrier.wait();
        let successes = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(successes, 4);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn reservation_addition_cannot_wrap() {
        let budget = Budget::new(usize::MAX);
        let credit = budget.reserve(usize::MAX).unwrap();
        assert!(budget.reserve(1).is_err());
        assert_eq!(budget.used(), usize::MAX);
        drop(credit);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn footprint_counts_reserved_string_and_vector_capacity() {
        let mut string = String::with_capacity(4096);
        string.push('x');
        let string_capacity = string.capacity();
        let mut array = Vec::with_capacity(256);
        array.push(Value::String(string));
        let capacity = array.capacity();
        let value = Value::Array(array);
        assert!(value_footprint(&value) >= VALUE_BYTES + capacity * VALUE_BYTES + string_capacity);
        let map = serde_json::json!({"key": "value"});
        assert!(value_footprint(&map) >= VALUE_BYTES + map_layout(1));
    }

    #[test]
    fn list_footprint_reads_fields_without_building_wire_values() {
        let mut users = Vec::with_capacity(8);
        users.push(crate::sync::UserList {
            id: String::with_capacity(128),
            name: Value::String(String::with_capacity(1024)),
            source: Some(Value::Array(Vec::with_capacity(16))),
            source_list_id: None,
            location_update_time: Some(Value::Number(12.into())),
            list: Vec::with_capacity(64),
        });
        let data = ListData {
            default_list: Vec::with_capacity(32),
            love_list: Vec::new(),
            user_list: users,
        };
        let minimum = size_of::<ListData>()
            + 96 * size_of::<MusicInfo>()
            + 8 * size_of::<crate::sync::UserList>()
            + 16 * VALUE_BYTES
            + 128
            + 1024;
        assert!(list_footprint(&data) >= minimum);
    }

    #[test]
    fn scanner_covers_parsed_dom_including_escaped_keys_and_strings() {
        for json in [
            "null",
            "42",
            "true",
            "[]",
            "{}",
            r#""\u4f60\u597d\nvalue""#,
            r#"{"\u006b\u0065\u0079":[null,"value",{"nested":true}],"n":1.25}"#,
            "[0,1,2,3,4,5,6,7,8]",
        ] {
            let estimate = estimate_json_tree(json).unwrap();
            let parsed: Value = serde_json::from_str(json).unwrap();
            assert!(estimate >= value_footprint(&parsed), "{json}");
        }
        for invalid in ["", "[", "{\"key\":}", "true false", "[NaN]"] {
            assert_eq!(
                estimate_json_tree(invalid).unwrap_err().kind,
                SyncErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn scanner_checks_input_depth_node_and_dom_bounds() {
        let nested = |depth| format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        assert!(estimate_json_tree(&nested(MAX_TREE_DEPTH)).is_ok());
        assert_eq!(
            estimate_json_tree(&nested(MAX_TREE_DEPTH + 1))
                .unwrap_err()
                .kind,
            SyncErrorKind::Quota
        );
        assert_eq!(
            estimate_json_tree(&" ".repeat(crate::sync::MAX_JSON_BYTES + 1))
                .unwrap_err()
                .kind,
            SyncErrorKind::Quota
        );
        let mut nodes = String::from("[");
        for index in 0..MAX_TREE_NODES - 1 {
            if index != 0 {
                nodes.push(',');
            }
            nodes.push_str("null");
        }
        nodes.push(']');
        assert!(estimate_json_tree(&nodes).is_ok());
        nodes.pop();
        nodes.push_str(",null]");
        assert_eq!(
            estimate_json_tree(&nodes).unwrap_err().kind,
            SyncErrorKind::Quota
        );
        let mut maps = String::from("[");
        for index in 0..60_000 {
            if index != 0 {
                maps.push(',');
            }
            maps.push_str(r#"{"a":null}"#);
        }
        maps.push(']');
        assert_eq!(
            estimate_json_tree(&maps).unwrap_err().kind,
            SyncErrorKind::Quota
        );
    }
}
