use super::{FileIdentity, MAX_DOWNLOAD_TASKS, MAX_DOWNLOAD_WORKERS, MAX_OWNED_PART_BYTES};
use anyhow::{Result, ensure};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    objects: Vec<(FileIdentity, u64)>,
}
#[derive(Clone, Default)]
pub(crate) struct PartialQuota(Arc<Mutex<State>>);
impl PartialQuota {
    pub(crate) fn update(&self, identity: &FileIdentity, bytes: u64) -> Result<()> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let existing = state
            .objects
            .iter()
            .position(|(object, _)| object == identity);
        ensure!(
            existing.is_some() || state.objects.len() < MAX_DOWNLOAD_TASKS + MAX_DOWNLOAD_WORKERS,
            "Owned download object bound reached"
        );
        let previous = existing.map_or(0, |index| state.objects[index].1);
        let total: u64 = state.objects.iter().map(|(_, bytes)| bytes).sum();
        ensure!(
            total
                .checked_sub(previous)
                .and_then(|n| n.checked_add(bytes))
                .is_some_and(|n| n <= MAX_OWNED_PART_BYTES),
            "Owned download partial quota exhausted"
        );
        if let Some(index) = existing {
            state.objects[index].1 = bytes;
        } else {
            state.objects.push((identity.clone(), bytes));
        }
        Ok(())
    }
    pub(crate) fn remove(&self, identity: &FileIdentity) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .retain(|(object, _)| object != identity);
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .iter()
            .map(|(_, bytes)| bytes)
            .sum()
    }
}
