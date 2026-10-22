//! Immutable list operations using explicit locale comparison and UTF-16 matching.
use super::music::{LxMusicDtoVersion, TrackRecord};
use anyhow::{Context, Result, ensure};
use icu_collator::{Collator, CollatorBorrowed, options::CollatorOptions};
use icu_locale_core::Locale;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap, sync::LazyLock};

pub const DEFAULT_LIST_LOCALE: &str = "zh-Hans-CN";
pub const MAX_LIST_TOOL_ITEMS: usize = 100000;
pub trait TextComparator {
    fn compare(&self, left: &str, right: &str) -> Ordering;
}
impl<F: Fn(&str, &str) -> Ordering> TextComparator for F {
    fn compare(&self, left: &str, right: &str) -> Ordering {
        self(left, right)
    }
}
pub struct IcuTextComparator {
    locale: String,
    collator: CollatorBorrowed<'static>,
}
impl IcuTextComparator {
    pub fn new(locale: &str) -> Result<Self> {
        let parsed: Locale = locale
            .parse()
            .map_err(|error| anyhow::anyhow!("Invalid list locale: {error}"))?;
        let collator = Collator::try_new(parsed.into(), CollatorOptions::default())
            .map_err(|error| anyhow::anyhow!("Cannot create list collator: {error}"))?;
        Ok(Self {
            locale: locale.into(),
            collator,
        })
    }
    pub fn locale(&self) -> &str {
        &self.locale
    }
}
impl TextComparator for IcuTextComparator {
    fn compare(&self, left: &str, right: &str) -> Ordering {
        self.collator.compare(left, right)
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SortField {
    #[serde(rename = "name")]
    Name,
    #[serde(rename = "singer")]
    Singer,
    #[serde(rename = "albumName", alias = "album_name")]
    AlbumName,
    #[serde(rename = "interval")]
    Interval,
    #[serde(rename = "source")]
    Source,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Up,
    Down,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortType {
    Up,
    Down,
    Random,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DuplicateCandidate {
    pub index: usize,
    pub id: String,
    pub key: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SearchMatch {
    Name,
    Singer,
    Album,
    Fuzzy,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub index: usize,
    pub kind: SearchMatch,
    pub score: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ListToolLimits {
    pub max_items: usize,
    pub max_query_utf16: usize,
    pub max_text_utf16: usize,
    pub max_distance_cells: u64,
    pub max_match_cells: u64,
}
impl Default for ListToolLimits {
    fn default() -> Self {
        Self {
            max_items: MAX_LIST_TOOL_ITEMS,
            max_query_utf16: 1024,
            max_text_utf16: 32768,
            max_distance_cells: 32_000_000,
            max_match_cells: 128_000_000,
        }
    }
}

struct View {
    name: String,
    singer: Option<String>,
    album: Option<String>,
    source: String,
    interval: Option<f64>,
}
fn view(track: &TrackRecord) -> View {
    let version = track
        .wire
        .as_ref()
        .map(|wire| wire.version)
        .unwrap_or(LxMusicDtoVersion::Modern);
    let dto = track.to_lx_dto(version);
    let album = match version {
        LxMusicDtoVersion::Modern => &dto["meta"]["albumName"],
        LxMusicDtoVersion::Legacy => &dto["albumName"],
    }
    .as_str()
    .map(str::to_owned);
    let interval = dto
        .get("interval")
        .filter(|value| !value.is_null())
        .map(|value| js_interval(value.as_str().unwrap_or("")));
    View {
        name: track.title.clone(),
        singer: dto["singer"].as_str().map(str::to_owned),
        album,
        source: track.platform.source.clone(),
        interval,
    }
}
fn js_interval(raw: &str) -> f64 {
    if raw.is_empty() {
        return 0.0;
    }
    let mut result = 0.0;
    let mut unit = 1.0;
    for part in raw.rsplit(':') {
        result += parse_int(part) * unit;
        unit *= 60.0;
    }
    result
}
fn parse_int(raw: &str) -> f64 {
    let raw = super::lists::trim_dislike(raw);
    let (negative, raw) = if let Some(tail) = raw.strip_prefix('-') {
        (true, tail)
    } else {
        (false, raw.strip_prefix('+').unwrap_or(raw))
    };
    let (radix, raw) = if let Some(tail) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X"))
    {
        (16, tail)
    } else {
        (10, raw)
    };
    let mut number = 0.0;
    let mut any = false;
    for ch in raw.chars() {
        let Some(digit) = ch.to_digit(radix).filter(|_| ch.is_ascii()) else {
            break;
        };
        number = number * radix as f64 + digit as f64;
        any = true;
    }
    if !any {
        f64::NAN
    } else if negative {
        -number
    } else {
        number
    }
}
fn cmp_optional<T>(
    left: Option<T>,
    right: Option<T>,
    direction: SortDirection,
    compare: impl FnOnce(T, T) -> Ordering,
) -> Ordering {
    let ordering = match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => compare(a, b),
    };
    if direction == SortDirection::Down {
        ordering.reverse()
    } else {
        ordering
    }
}
pub fn sort_order(
    list: &[TrackRecord],
    field: SortField,
    direction: SortDirection,
    comparator: &dyn TextComparator,
) -> Result<Vec<usize>> {
    ensure!(
        list.len() <= MAX_LIST_TOOL_ITEMS,
        "List sort exceeds 100000 items"
    );
    let views: Vec<_> = list.iter().map(view).collect();
    let mut order: Vec<_> = (0..list.len()).collect();
    let compare = |a: usize, b: usize| {
        let (a, b) = (&views[a], &views[b]);
        match field {
            SortField::Name => cmp_optional(
                Some(a.name.as_str()),
                Some(b.name.as_str()),
                direction,
                |a, b| comparator.compare(a, b),
            ),
            SortField::Singer => cmp_optional(
                a.singer.as_deref(),
                b.singer.as_deref(),
                direction,
                |a, b| comparator.compare(a, b),
            ),
            SortField::AlbumName => {
                cmp_optional(a.album.as_deref(), b.album.as_deref(), direction, |a, b| {
                    comparator.compare(a, b)
                })
            }
            SortField::Source => cmp_optional(
                Some(a.source.as_str()),
                Some(b.source.as_str()),
                direction,
                |a, b| comparator.compare(a, b),
            ),
            SortField::Interval => cmp_optional(a.interval, b.interval, direction, |a, b| {
                (a - b).partial_cmp(&0.0).unwrap_or(Ordering::Equal)
            }),
        }
    };
    if field == SortField::Interval && views.iter().any(|v| v.interval.is_some_and(f64::is_nan)) {
        // V8's small-array binary insertion semantics tolerate NaN comparisons.
        // Avoid feeding a non-total comparator to Rust's total-order sort.
        let mut shifted = 0usize;
        for index in 1..order.len() {
            let entry = order[index];
            let (mut low, mut high) = (0, index);
            while low < high {
                let mid = (low + high) / 2;
                if compare(order[mid], entry) == Ordering::Greater {
                    high = mid
                } else {
                    low = mid + 1
                }
            }
            shifted = shifted
                .checked_add(index - low)
                .context("Interval sort work overflow")?;
            ensure!(
                shifted <= 32_000_000,
                "Interval NaN sort exceeds its move budget"
            );
            order[low..=index].rotate_right(1);
        }
    } else {
        order.sort_by(|a, b| compare(*a, *b));
    }
    Ok(order)
}
pub fn shuffle_order(
    length: usize,
    mut draw_index: impl FnMut(usize) -> Result<usize>,
) -> Result<Vec<usize>> {
    ensure!(
        length <= MAX_LIST_TOOL_ITEMS,
        "List shuffle exceeds 100000 items"
    );
    let mut order: Vec<_> = (0..length).collect();
    let mut current = length;
    while current != 0 {
        let index = draw_index(current)?;
        ensure!(
            index < current,
            "Shuffle draw exceeds its exclusive upper bound"
        );
        current -= 1;
        order.swap(current, index);
    }
    Ok(order)
}
pub fn random_order(length: usize) -> Result<Vec<usize>> {
    shuffle_order(length, |upper| {
        let upper = upper as u64;
        let cutoff = u64::MAX - u64::MAX % upper;
        loop {
            let mut bytes = [0; 8];
            getrandom::fill(&mut bytes)
                .map_err(|error| anyhow::anyhow!("List random entropy: {error}"))?;
            let value = u64::from_le_bytes(bytes);
            if value < cutoff {
                return Ok((value % upper) as usize);
            }
        }
    })
}
static VARIANT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\(|（)[^\n\r\u{2028}\u{2029}]+(\)|）)").expect("Version brackets")
});
fn whitespace(ch: char) -> bool {
    matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
}
pub fn duplicate_key(name: &str, filter_variants: bool) -> String {
    let lower = name.to_lowercase();
    if !filter_variants {
        return super::lists::trim_dislike(&lower).into();
    }
    let stripped = VARIANT.replace_all(&lower, "");
    let key: String = stripped
        .chars()
        .filter(|ch| !whitespace(*ch) && !"'.,，&\"、()（）`~-<>|/][".contains(*ch))
        .collect();
    if key.is_empty() {
        lower.chars().filter(|ch| !whitespace(*ch)).collect()
    } else {
        key
    }
}
pub fn duplicate_candidates(
    list: &[TrackRecord],
    filter_variants: bool,
    comparator: &dyn TextComparator,
) -> Result<Vec<DuplicateCandidate>> {
    ensure!(
        list.len() <= MAX_LIST_TOOL_ITEMS,
        "Duplicate scan exceeds 100000 items"
    );
    let mut groups = HashMap::<String, Vec<usize>>::new();
    let mut keys = Vec::new();
    for (index, track) in list.iter().enumerate() {
        let key = duplicate_key(&track.title, filter_variants);
        let group = groups.entry(key.clone()).or_default();
        group.push(index);
        if group.len() == 2 {
            keys.push(key.clone());
        }
    }
    keys.sort_by(|a, b| comparator.compare(a, b));
    Ok(keys
        .into_iter()
        .flat_map(|key| {
            groups[&key]
                .iter()
                .map(|index| DuplicateCandidate {
                    index: *index,
                    id: list[*index].lx_id.clone(),
                    key: key.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect())
}
fn regex_unit(unit: u16) -> u16 {
    let Some(ch) = char::from_u32(unit as u32) else {
        return unit;
    };
    let mut uppercase = ch.to_uppercase();
    let Some(first) = uppercase.next() else {
        return unit;
    };
    if uppercase.next().is_some() || first as u32 > 65535 || (unit >= 128 && (first as u32) < 128) {
        unit
    } else {
        first as u16
    }
}
fn line_term(unit: u16) -> bool {
    matches!(unit, 10 | 13 | 0x2028 | 0x2029)
}
fn subsequence(query: &[u16], text: &[u16]) -> bool {
    if query.is_empty() {
        return true;
    }
    let query: Vec<_> = query.iter().copied().map(regex_unit).collect();
    let mut states = vec![false; query.len() + 1];
    states[0] = true;
    for unit in text {
        let folded = regex_unit(*unit);
        for index in (0..query.len()).rev() {
            let active = states[index];
            if index > 0 && line_term(*unit) {
                states[index] = false;
            }
            if active && query[index] == folded {
                if index + 1 == query.len() {
                    return true;
                }
                states[index + 1] = true;
            }
        }
    }
    false
}
fn charge(remaining: &mut u64, amount: u64, label: &str) -> Result<()> {
    *remaining = remaining
        .checked_sub(amount)
        .with_context(|| format!("List search {label} budget exceeded"))?;
    Ok(())
}
fn similarity(left: &[u16], right: &[u16]) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let (a, b) = if left.len() > right.len() {
        (right, left)
    } else {
        (left, right)
    };
    let mut row: Vec<_> = (0..=b.len()).collect();
    for (index, ch) in a.iter().enumerate() {
        let mut prior = row[0];
        row[0] = index + 1;
        for j in 1..=b.len() {
            let old = row[j];
            row[j] = (old + 1)
                .min(row[j - 1] + 1)
                .min(prior + usize::from(*ch != b[j - 1]));
            prior = old;
        }
    }
    1.0 - row[b.len()] as f64 / b.len() as f64
}
pub fn search_list(
    list: &[TrackRecord],
    text: &str,
    limits: &ListToolLimits,
) -> Result<Vec<SearchHit>> {
    ensure!(
        list.len() <= limits.max_items && limits.max_items <= MAX_LIST_TOOL_ITEMS,
        "List search item limit exceeded"
    );
    let query: Vec<_> = text.encode_utf16().collect();
    ensure!(
        query.len() <= limits.max_query_utf16,
        "List search query is too long"
    );
    let lower = text.to_lowercase();
    let mut groups: [Vec<SearchHit>; 4] = std::array::from_fn(|_| Vec::new());
    let mut distance = limits.max_distance_cells;
    let mut matching = limits.max_match_cells;
    for (index, track) in list.iter().enumerate() {
        let v = view(track);
        let singer = v.singer.as_deref().unwrap_or("");
        let album = v.album.as_deref().unwrap_or("");
        let kind = if v.name.to_lowercase().contains(&lower) {
            Some(SearchMatch::Name)
        } else if v.singer.is_some() && singer.to_lowercase().contains(&lower) {
            Some(SearchMatch::Singer)
        } else if v.album.is_some() && album.to_lowercase().contains(&lower) {
            Some(SearchMatch::Album)
        } else {
            None
        };
        if let Some(kind) = kind {
            let tier = match kind {
                SearchMatch::Name => 0,
                SearchMatch::Singer => 1,
                _ => 2,
            };
            groups[tier].push(SearchHit {
                index,
                kind,
                score: None,
            });
            continue;
        }
        let combined = format!("{}{singer}{album}", v.name);
        let units: Vec<_> = combined.encode_utf16().collect();
        ensure!(
            units.len() <= limits.max_text_utf16,
            "List search candidate is too long"
        );
        let cells = (query.len() as u64)
            .checked_mul(units.len() as u64)
            .context("List search work overflow")?;
        charge(&mut matching, cells, "matching")?;
        if subsequence(&query, &units) {
            charge(&mut distance, cells, "distance")?;
            groups[3].push(SearchHit {
                index,
                kind: SearchMatch::Fuzzy,
                score: Some(similarity(&query, &units)),
            });
        }
    }
    groups[3].sort_by(|a, b| {
        b.score
            .expect("Fuzzy score")
            .total_cmp(&a.score.expect("Fuzzy score"))
    });
    Ok(groups.into_iter().flatten().collect())
}
