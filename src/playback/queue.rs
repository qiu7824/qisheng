//! Deterministic queue rules independent of audio devices, providers and the UI.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
pub const MAX_NORMAL_ENTRIES: usize = 50_000;
pub const MAX_TEMPORARY_ENTRIES: usize = 10_000;
pub const MAX_QUEUE_HISTORY: usize = 1_000;
pub const MAX_QUEUE_ID_BYTES: usize = 1_024;
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlayMode {
    #[default]
    #[serde(rename = "listLoop")]
    ListLoop,
    #[serde(rename = "random")]
    Random,
    #[serde(rename = "list")]
    List,
    #[serde(rename = "singleLoop")]
    SingleLoop,
    #[serde(rename = "none")]
    None,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AdvanceReason {
    Manual,
    Ended,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntry {
    pub entry_id: String,
    pub track_id: String,
    pub list_id: String,
    pub temporary: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct NextPlan {
    revision: u64,
    reason: AdvanceReason,
    entry: Option<QueueEntry>,
    random_reset: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueState {
    pub mode: PlayMode,
    pub revision: u64,
    pub origin_list: String,
    pub entries: Vec<QueueEntry>,
    pub temporary: VecDeque<QueueEntry>,
    pub current: Option<QueueEntry>,
    history: VecDeque<QueueEntry>,
    future: Vec<QueueEntry>,
    played: HashSet<String>,
    normal_cursor: Option<String>,
    /// A deleted cursor leaves an insertion boundary in the unfiltered origin.
    #[serde(default)]
    normal_gap: Option<usize>,
    /// A cleared/removed temporary current may finish, but must not be replayed.
    #[serde(default)]
    retired_current_temporary: Option<String>,
    pending: Option<NextPlan>,
    rng: u64,
    next_id: u64,
}
impl Default for QueueState {
    fn default() -> Self {
        Self::new(1)
    }
}
impl QueueState {
    pub fn new(seed: u64) -> Self {
        Self {
            mode: PlayMode::ListLoop,
            revision: 0,
            origin_list: "default".into(),
            entries: Vec::new(),
            temporary: VecDeque::new(),
            current: None,
            history: VecDeque::new(),
            future: Vec::new(),
            played: HashSet::new(),
            normal_cursor: None,
            normal_gap: None,
            retired_current_temporary: None,
            pending: None,
            rng: seed.max(1),
            next_id: 0,
        }
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.pending = None;
    }
    /// External capability/dislike changes invalidate speculation without consumption.
    pub fn invalidate_plan(&mut self) {
        self.changed();
    }
    /// Clears the current playback reference without choosing another entry.
    /// The normal cursor and navigation snapshots remain available for resume.
    pub fn clear_current(&mut self) {
        self.current = None;
        self.retired_current_temporary = None;
        self.changed();
    }
    fn entry(&mut self, track: String, list: String, temporary: bool) -> QueueEntry {
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("validated queue entry counter");
        QueueEntry {
            entry_id: format!("queue-{}", self.next_id),
            track_id: track,
            list_id: list,
            temporary,
        }
    }
    fn live_normal(&self, id: &str) -> Option<&QueueEntry> {
        self.entries.iter().find(|entry| entry.entry_id == id)
    }
    fn replayable(&self, entry: &QueueEntry) -> bool {
        if entry.temporary {
            self.retired_current_temporary.as_ref() != Some(&entry.entry_id)
        } else {
            self.live_normal(&entry.entry_id)
                .is_some_and(|live| live == entry)
        }
    }
    fn cursor_index(&self) -> Option<usize> {
        self.normal_cursor
            .as_ref()
            .and_then(|id| self.entries.iter().position(|entry| &entry.entry_id == id))
    }
    fn next_boundary(&self) -> usize {
        self.cursor_index()
            .map(|index| index + 1)
            .or(self.normal_gap)
            .unwrap_or(0)
            .min(self.entries.len())
    }
    fn previous_boundary(&self) -> usize {
        self.cursor_index()
            .or(self.normal_gap)
            .unwrap_or(0)
            .min(self.entries.len())
    }
    pub fn set_mode(&mut self, mode: PlayMode) {
        if self.mode != mode {
            self.mode = mode;
            self.played.clear();
            if mode == PlayMode::Random
                && let Some(id) = &self.normal_cursor
                && self.live_normal(id).is_some()
            {
                self.played.insert(id.clone());
            }
            self.future.clear();
            self.changed();
        }
    }
    /// Occurrence identities and nearest surviving neighbors retain a deleted cursor.
    pub fn set_list(&mut self, list_id: String, tracks: Vec<String>) {
        if self.origin_list == list_id
            && self
                .entries
                .iter()
                .map(|entry| &entry.track_id)
                .eq(tracks.iter())
        {
            return;
        }
        let same = self.origin_list == list_id;
        let old = if same {
            std::mem::take(&mut self.entries)
        } else {
            Vec::new()
        };
        let old_boundary = self
            .normal_cursor
            .as_ref()
            .and_then(|id| old.iter().position(|entry| &entry.entry_id == id))
            .or(self.normal_gap)
            .unwrap_or(0)
            .min(old.len());
        let cursor_was_live = self
            .normal_cursor
            .as_ref()
            .is_some_and(|id| old.iter().any(|entry| &entry.entry_id == id));
        let mut by_track: HashMap<String, VecDeque<QueueEntry>> = HashMap::new();
        for entry in &old {
            by_track
                .entry(entry.track_id.clone())
                .or_default()
                .push_back(entry.clone());
        }
        let mut entries = Vec::with_capacity(tracks.len());
        for track in tracks {
            let kept = by_track.get_mut(&track).and_then(VecDeque::pop_front);
            entries.push(kept.unwrap_or_else(|| self.entry(track, list_id.clone(), false)));
        }
        self.origin_list = list_id;
        self.entries = entries;
        let positions: HashMap<_, _> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.entry_id.clone(), i))
            .collect();
        self.played.retain(|id| positions.contains_key(id));
        if !same {
            self.normal_cursor = None;
            self.normal_gap = None;
            self.played.clear();
            self.future.clear();
        } else if self
            .normal_cursor
            .as_ref()
            .is_some_and(|id| positions.contains_key(id))
        {
            self.normal_gap = None;
        } else if self.normal_cursor.is_some() {
            let right = old_boundary + usize::from(cursor_was_live);
            self.normal_gap = old
                .iter()
                .skip(right)
                .find_map(|entry| positions.get(&entry.entry_id).copied())
                .or_else(|| {
                    old.iter()
                        .take(old_boundary)
                        .rev()
                        .find_map(|entry| positions.get(&entry.entry_id).map(|index| index + 1))
                })
                .or(Some(old_boundary.min(self.entries.len())));
        }
        self.future
            .retain(|entry| entry.temporary || positions.contains_key(&entry.entry_id));
        self.changed();
    }
    pub fn select(&mut self, entry_id: &str) -> Option<QueueEntry> {
        let selected = self.live_normal(entry_id)?.clone();
        self.clear_temporary();
        self.future.clear();
        self.commit_selected(selected.clone(), true);
        Some(selected)
    }
    pub fn select_track(&mut self, track_id: &str) -> Option<QueueEntry> {
        let id = self
            .entries
            .iter()
            .find(|entry| entry.track_id == track_id)?
            .entry_id
            .clone();
        self.select(&id)
    }
    pub fn enqueue(&mut self, tracks: Vec<String>, front: bool) {
        let refs = tracks
            .into_iter()
            .map(|track| (self.origin_list.clone(), track))
            .collect();
        self.enqueue_refs(refs, front);
    }
    /// Each tuple is (list_id, track_id); foreign-list identities are retained.
    pub fn enqueue_refs(&mut self, refs: Vec<(String, String)>, front: bool) {
        let mut items: Vec<_> = refs
            .into_iter()
            .map(|(list, track)| self.entry(track, list, true))
            .collect();
        if front {
            while let Some(item) = items.pop() {
                self.temporary.push_front(item);
            }
        } else {
            self.temporary.extend(items);
        }
        self.changed();
    }
    pub fn remove(&mut self, entry_id: &str) -> bool {
        let normal_index = self
            .entries
            .iter()
            .position(|entry| entry.entry_id == entry_id);
        if let Some(index) = normal_index {
            if self.normal_cursor.as_deref() == Some(entry_id) {
                self.normal_gap = Some(index);
            } else if let Some(gap) = &mut self.normal_gap
                && index < *gap
            {
                *gap -= 1;
            }
        }
        let old = self.entries.len()
            + self.temporary.len()
            + self.future.len()
            + self.history.iter().filter(|entry| entry.temporary).count();
        self.entries.retain(|entry| entry.entry_id != entry_id);
        self.temporary.retain(|entry| entry.entry_id != entry_id);
        self.future.retain(|entry| entry.entry_id != entry_id);
        self.history
            .retain(|entry| !entry.temporary || entry.entry_id != entry_id);
        self.played.remove(entry_id);
        if let Some(gap) = &mut self.normal_gap {
            *gap = (*gap).min(self.entries.len());
        }
        let current_temp = self
            .current
            .as_ref()
            .is_some_and(|entry| entry.temporary && entry.entry_id == entry_id);
        if current_temp {
            self.retired_current_temporary = Some(entry_id.into());
        }
        let changed = current_temp
            || old
                != self.entries.len()
                    + self.temporary.len()
                    + self.future.len()
                    + self.history.iter().filter(|entry| entry.temporary).count();
        if changed {
            self.changed();
        }
        changed
    }
    pub fn clear_temporary(&mut self) {
        let changed = !self.temporary.is_empty()
            || self.history.iter().any(|entry| entry.temporary)
            || self.future.iter().any(|entry| entry.temporary);
        self.temporary.clear();
        self.history.retain(|entry| !entry.temporary);
        self.future.retain(|entry| !entry.temporary);
        if let Some(current) = &self.current
            && current.temporary
        {
            self.retired_current_temporary = Some(current.entry_id.clone());
            self.changed();
        } else if changed {
            self.changed();
        }
    }
    fn random_index(&mut self, len: usize) -> usize {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x as usize) % len
    }
    fn forward_normal(
        &self,
        wrap: bool,
        playable: &impl Fn(&QueueEntry) -> bool,
    ) -> Option<QueueEntry> {
        let boundary = self.next_boundary();
        self.entries
            .iter()
            .skip(boundary)
            .chain(self.entries.iter().take(if wrap { boundary } else { 0 }))
            .find(|entry| playable(entry))
            .cloned()
    }
    fn backward_normal(&self, playable: &impl Fn(&QueueEntry) -> bool) -> Option<QueueEntry> {
        let boundary = self.previous_boundary();
        self.entries
            .iter()
            .take(boundary)
            .rev()
            .chain(self.entries.iter().skip(boundary).rev())
            .find(|entry| playable(entry))
            .cloned()
    }
    pub fn preview_next(
        &mut self,
        reason: AdvanceReason,
        playable: impl Fn(&QueueEntry) -> bool,
    ) -> Option<QueueEntry> {
        let failed = self
            .current
            .as_ref()
            .filter(|_| reason == AdvanceReason::Failed)
            .map(|entry| entry.entry_id.clone());
        let eligible =
            |entry: &QueueEntry| playable(entry) && failed.as_ref() != Some(&entry.entry_id);
        if let Some(plan) = &self.pending
            && plan.revision == self.revision
            && plan.reason == reason
            && let Some(entry) = &plan.entry
            && eligible(entry)
            && self.replayable(entry)
        {
            return Some(entry.clone());
        }
        let mut reset = false;
        let next = if let Some(entry) = self.temporary.iter().find(|entry| eligible(entry)) {
            Some(entry.clone())
        } else if let Some(entry) = self
            .future
            .iter()
            .rev()
            .find(|entry| eligible(entry) && self.replayable(entry))
        {
            Some(entry.clone())
        } else {
            let mode = if reason == AdvanceReason::Manual
                && matches!(
                    self.mode,
                    PlayMode::List | PlayMode::SingleLoop | PlayMode::None
                )
                || reason == AdvanceReason::Failed
                    && self.mode == PlayMode::SingleLoop
                    && self.current.as_ref().is_some_and(|entry| !entry.temporary)
            {
                PlayMode::ListLoop
            } else {
                self.mode
            };
            match mode {
                PlayMode::None => None,
                PlayMode::SingleLoop => self
                    .normal_cursor
                    .as_ref()
                    .and_then(|id| self.live_normal(id))
                    .filter(|entry| eligible(entry))
                    .cloned()
                    .or_else(|| self.forward_normal(true, &eligible)),
                PlayMode::List => self.forward_normal(false, &eligible),
                PlayMode::ListLoop => self.forward_normal(true, &eligible),
                PlayMode::Random => {
                    let candidates: Vec<_> = self
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| eligible(entry))
                        .map(|(index, _)| index)
                        .collect();
                    let mut pool: Vec<_> = candidates
                        .iter()
                        .copied()
                        .filter(|index| {
                            !self.played.contains(&self.entries[*index].entry_id)
                                && self.normal_cursor.as_ref()
                                    != Some(&self.entries[*index].entry_id)
                        })
                        .collect();
                    if pool.is_empty() && !candidates.is_empty() {
                        reset = true;
                        pool = candidates
                            .iter()
                            .copied()
                            .filter(|index| {
                                candidates.len() == 1
                                    || self.normal_cursor.as_ref()
                                        != Some(&self.entries[*index].entry_id)
                            })
                            .collect();
                    }
                    if pool.is_empty() {
                        None
                    } else {
                        let index = self.random_index(pool.len());
                        Some(self.entries[pool[index]].clone())
                    }
                }
            }
        };
        self.pending = Some(NextPlan {
            revision: self.revision,
            reason,
            entry: next.clone(),
            random_reset: reset && next.is_some(),
        });
        next
    }
    pub fn advance(
        &mut self,
        reason: AdvanceReason,
        playable: impl Fn(&QueueEntry) -> bool,
    ) -> Option<QueueEntry> {
        let next = self.preview_next(reason, &playable)?;
        if self.pending.as_ref().is_some_and(|plan| plan.random_reset) {
            self.played.clear();
        }
        if next.temporary {
            while let Some(entry) = self.temporary.pop_front() {
                if entry.entry_id == next.entry_id {
                    break;
                }
            }
        }
        if let Some(index) = self
            .future
            .iter()
            .rposition(|entry| entry.entry_id == next.entry_id)
        {
            self.future.truncate(index);
        }
        self.commit_selected(next.clone(), false);
        Some(next)
    }
    fn commit_selected(&mut self, next: QueueEntry, reset_future: bool) {
        if let Some(previous) = self.current.replace(next.clone())
            && previous.entry_id != next.entry_id
            && self.retired_current_temporary.as_ref() != Some(&previous.entry_id)
        {
            self.history.push_back(previous);
            if self.history.len() > MAX_QUEUE_HISTORY {
                self.history.pop_front();
            }
        }
        self.retired_current_temporary = None;
        if !next.temporary {
            self.normal_cursor = Some(next.entry_id.clone());
            self.normal_gap = None;
            if self.mode == PlayMode::Random {
                self.played.insert(next.entry_id.clone());
            }
        }
        if reset_future {
            self.future.clear();
        }
        self.changed();
    }
    fn save_current_for_replay(&mut self) {
        if let Some(current) = self.current.clone()
            && self.replayable(&current)
        {
            if current.temporary
                && !self
                    .temporary
                    .iter()
                    .any(|entry| entry.entry_id == current.entry_id)
            {
                self.temporary.push_front(current.clone());
            }
            self.future.push(current);
            if self.future.len() > MAX_QUEUE_HISTORY {
                self.future.remove(0);
            }
        }
    }
    pub fn previous(&mut self, playable: impl Fn(&QueueEntry) -> bool) -> Option<QueueEntry> {
        let temp = self.current.as_ref().is_some_and(|entry| entry.temporary);
        let replay = self.mode == PlayMode::Random
            || temp
            || self.history.back().is_some_and(|entry| entry.temporary);
        let mut removed_history = false;
        if replay {
            while let Some(previous) = self.history.pop_back() {
                removed_history = true;
                if playable(&previous) && self.replayable(&previous) {
                    self.save_current_for_replay();
                    self.current = Some(previous.clone());
                    self.retired_current_temporary = None;
                    if !previous.temporary {
                        self.normal_cursor = Some(previous.entry_id.clone());
                        self.normal_gap = None;
                    }
                    self.changed();
                    return Some(previous);
                }
            }
        }
        let previous = if temp {
            self.normal_cursor
                .as_ref()
                .and_then(|id| self.live_normal(id))
                .filter(|entry| playable(entry))
                .cloned()
                .or_else(|| {
                    let boundary = self
                        .cursor_index()
                        .or(self.normal_gap)
                        .unwrap_or(0)
                        .min(self.entries.len());
                    self.entries
                        .iter()
                        .skip(boundary)
                        .chain(self.entries.iter().take(boundary))
                        .find(|entry| playable(entry))
                        .cloned()
                })
        } else if self.mode == PlayMode::Random {
            let available: Vec<_> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| playable(entry))
                .map(|(index, _)| index)
                .collect();
            let mut pool: Vec<_> = available
                .iter()
                .copied()
                .filter(|index| {
                    !self.played.contains(&self.entries[*index].entry_id)
                        && self.normal_cursor.as_ref() != Some(&self.entries[*index].entry_id)
                })
                .collect();
            if pool.is_empty() {
                pool = available
                    .iter()
                    .copied()
                    .filter(|index| {
                        available.len() == 1
                            || self.normal_cursor.as_ref() != Some(&self.entries[*index].entry_id)
                    })
                    .collect();
                if !pool.is_empty() {
                    self.played.clear();
                }
            }
            if pool.is_empty() {
                None
            } else {
                let i = self.random_index(pool.len());
                Some(self.entries[pool[i]].clone())
            }
        } else {
            self.backward_normal(&playable)
        };
        if let Some(previous) = previous {
            if temp {
                self.save_current_for_replay();
            }
            self.commit_selected(previous.clone(), false);
            Some(previous)
        } else {
            if removed_history {
                self.changed();
            }
            None
        }
    }
    pub fn history(&self) -> &VecDeque<QueueEntry> {
        &self.history
    }
    pub fn future(&self) -> &[QueueEntry] {
        &self.future
    }
    /// Repository ownership and track existence are checked separately by callers.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.entries.len() <= MAX_NORMAL_ENTRIES,
            "Normal queue exceeds 50000 entries"
        );
        ensure!(
            self.temporary.len() <= MAX_TEMPORARY_ENTRIES,
            "Temporary queue exceeds 10000 entries"
        );
        ensure!(
            self.history.len() <= MAX_QUEUE_HISTORY && self.future.len() <= MAX_QUEUE_HISTORY,
            "Queue navigation history exceeds 1000 entries"
        );
        id(&self.origin_list, "origin list")?;
        ensure!(self.rng != 0, "Queue RNG state must not be zero");
        ensure!(
            self.next_id <= u64::MAX - (MAX_NORMAL_ENTRIES + MAX_TEMPORARY_ENTRIES) as u64,
            "Queue entry counter is exhausted"
        );
        let mut live = HashSet::new();
        let mut records: HashMap<&str, &QueueEntry> = HashMap::new();
        for entry in &self.entries {
            ensure!(
                !entry.temporary && entry.list_id == self.origin_list,
                "Normal entry does not belong to the origin list"
            );
            ensure!(
                live.insert(entry.entry_id.as_str()),
                "Duplicate live queue entry identity"
            );
            record(entry, self.next_id, &mut records)?;
        }
        for entry in &self.temporary {
            ensure!(entry.temporary, "Temporary entry is not marked temporary");
            ensure!(
                live.insert(entry.entry_id.as_str()),
                "Duplicate live queue entry identity"
            );
            record(entry, self.next_id, &mut records)?;
        }
        for entry in self
            .current
            .iter()
            .chain(self.history.iter())
            .chain(self.future.iter())
        {
            record(entry, self.next_id, &mut records)?;
        }
        let normals: HashSet<_> = self
            .entries
            .iter()
            .map(|entry| entry.entry_id.as_str())
            .collect();
        ensure!(
            self.played
                .iter()
                .all(|entry| normals.contains(entry.as_str())),
            "Played identity is absent from the normal queue"
        );
        if let Some(cursor) = &self.normal_cursor {
            sequence(cursor, self.next_id)?;
            if normals.contains(cursor.as_str()) {
                ensure!(
                    self.normal_gap.is_none(),
                    "Live normal cursor has a deleted-position boundary"
                );
            } else {
                ensure!(
                    self.normal_gap.is_some(),
                    "Deleted normal cursor lacks its position boundary"
                );
            }
        } else {
            ensure!(
                self.normal_gap.is_none(),
                "Position boundary lacks a normal cursor"
            );
        }
        if let Some(gap) = self.normal_gap {
            ensure!(
                gap <= self.entries.len(),
                "Deleted normal position is outside the list"
            );
        }
        if let Some(retired) = &self.retired_current_temporary {
            sequence(retired, self.next_id)?;
            ensure!(
                self.current
                    .as_ref()
                    .is_some_and(|entry| entry.temporary && &entry.entry_id == retired),
                "Retired temporary identity is not the current temporary entry"
            );
            ensure!(
                !self
                    .temporary
                    .iter()
                    .chain(self.history.iter())
                    .chain(self.future.iter())
                    .any(|entry| entry.entry_id == *retired),
                "Retired temporary entry remains replayable"
            );
        }
        for entry in &self.future {
            ensure!(
                if entry.temporary {
                    self.retired_current_temporary.as_ref() != Some(&entry.entry_id)
                } else {
                    normals.contains(entry.entry_id.as_str())
                },
                "Future entry is absent or retired"
            );
        }
        if let Some(plan) = &self.pending {
            ensure!(
                plan.revision == self.revision,
                "Cached next plan has a stale revision"
            );
            ensure!(
                !plan.random_reset || self.mode == PlayMode::Random && plan.entry.is_some(),
                "Cached random reset is inconsistent"
            );
            if let Some(entry) = &plan.entry {
                record(entry, self.next_id, &mut records)?;
                let member = if entry.temporary {
                    self.temporary
                        .iter()
                        .chain(self.future.iter())
                        .any(|live| live == entry)
                } else {
                    normals.contains(entry.entry_id.as_str())
                };
                ensure!(
                    member && self.retired_current_temporary.as_ref() != Some(&entry.entry_id),
                    "Cached next plan references an absent or retired entry"
                );
                ensure!(
                    plan.reason != AdvanceReason::Failed
                        || self
                            .current
                            .as_ref()
                            .is_none_or(|current| current.entry_id != entry.entry_id),
                    "Failed next plan repeats the failed current entry"
                );
            }
        }
        Ok(())
    }
}
fn id(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_QUEUE_ID_BYTES && !value.contains('\0'),
        "Invalid queue {label} identity"
    );
    Ok(())
}
fn sequence(value: &str, next: u64) -> Result<()> {
    id(value, "entry")?;
    let parsed = value
        .strip_prefix("queue-")
        .and_then(|value| value.parse::<u64>().ok());
    ensure!(
        parsed.is_some_and(|number| number > 0
            && number <= next
            && value == format!("queue-{number}")),
        "Invalid queue entry sequence"
    );
    Ok(())
}
fn record<'a>(
    entry: &'a QueueEntry,
    next: u64,
    records: &mut HashMap<&'a str, &'a QueueEntry>,
) -> Result<()> {
    sequence(&entry.entry_id, next)?;
    id(&entry.track_id, "track")?;
    id(&entry.list_id, "list")?;
    if let Some(existing) = records.insert(entry.entry_id.as_str(), entry) {
        ensure!(
            existing == entry,
            "One queue identity refers to different entries"
        );
    }
    Ok(())
}
