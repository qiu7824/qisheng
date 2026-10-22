//! Stable representation information is admitted only after transport validator checks.
use super::{Block, CacheStore, FileBlock, State, io_worker::Core};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub version: u32,
    pub data_key: String,
    pub strong_etag: String,
    pub total: u64,
    pub block_size: usize,
    pub ledger_epoch: u64,
}
impl Metadata {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2
                && self.data_key.len() == 64
                && self
                    .data_key
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid reusable data identity"
        );
        let etag = self.strong_etag.as_bytes();
        ensure!(
            etag.len() >= 2
                && etag.len() <= 1024
                && etag.first() == Some(&b'"')
                && etag.last() == Some(&b'"')
                && etag[1..etag.len() - 1]
                    .iter()
                    .all(|byte| *byte == 0x21 || (0x23..=0x7e).contains(byte)),
            "reusable representation requires a strong ETag"
        );
        ensure!(
            self.total > 0 && self.block_size > 0 && self.block_size <= 4 * 1024 * 1024,
            "invalid reusable representation interval"
        );
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    pub representation: Metadata,
    pub offset: u64,
    pub body_sha256: String,
}
impl Registration {
    pub fn validate(&self, len: usize) -> Result<()> {
        self.representation.validate()?;
        let metadata = &self.representation;
        ensure!(
            self.offset.is_multiple_of(metadata.block_size as u64)
                && self.offset < metadata.total
                && len == (metadata.total - self.offset).min(metadata.block_size as u64) as usize,
            "reusable block is not a complete verified interval"
        );
        ensure!(
            self.body_sha256.len() == 64
                && self
                    .body_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid reusable body digest"
        );
        Ok(())
    }
}
pub(super) struct Record {
    pub metadata: Metadata,
    pub files: BTreeMap<u64, Arc<FileBlock>>,
}

#[derive(Clone)]
pub struct ReusableRepresentation {
    pub strong_etag: String,
    pub total: u64,
    pub block_size: usize,
    pub verified_bytes: u64,
    pub offsets: Vec<u64>,
    metadata: Metadata,
    files: BTreeMap<u64, Weak<FileBlock>>,
    owner: Weak<Core>,
    epoch: u64,
}
impl std::fmt::Debug for ReusableRepresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReusableRepresentation")
            .field("strong_etag", &self.strong_etag)
            .field("total", &self.total)
            .field("block_size", &self.block_size)
            .field("verified_bytes", &self.verified_bytes)
            .field("offsets", &self.offsets)
            .finish()
    }
}
pub(super) enum Job {
    Lookup {
        data_key: String,
        epoch: u64,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<std::result::Result<Option<ReusableRepresentation>, String>>,
    },
    Register {
        metadata: Metadata,
        object: String,
        generation: u64,
        epoch: u64,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<std::result::Result<bool, String>>,
    },
    Invalidate {
        data_key: String,
        expected: Option<Metadata>,
        epoch: u64,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<std::result::Result<bool, String>>,
    },
}
impl Job {
    pub fn cancel(&self) -> Arc<AtomicBool> {
        match self {
            Self::Lookup { cancel, .. }
            | Self::Register { cancel, .. }
            | Self::Invalidate { cancel, .. } => cancel.clone(),
        }
    }
    pub fn finish_cancelled(self) {
        match self {
            Self::Lookup { reply, .. } => {
                let _ = reply.send(Err("reusable lookup cancelled".into()));
            }
            Self::Register { reply, .. } | Self::Invalidate { reply, .. } => {
                let _ = reply.send(Err("reusable registration cancelled".into()));
            }
        }
    }
}
impl CacheStore {
    pub async fn lookup_reusable_async(
        self: &Arc<Self>,
        data_key: String,
        cancel: Arc<AtomicBool>,
    ) -> Result<Option<ReusableRepresentation>> {
        let (reply, receiver) = oneshot::channel();
        {
            let mut state = self
                .core
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            admit(self, &state, &cancel)?;
            ensure!(
                data_key.len() == 64
                    && data_key
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid reusable data identity"
            );
            let epoch = state.epoch;
            state.reuse_jobs.push_back(Job::Lookup {
                data_key,
                epoch,
                cancel: cancel.clone(),
                reply,
            });
        }
        self.core.wake();
        tokio::select! {value=receiver=>value.map_err(|_|anyhow::anyhow!("reusable lookup worker closed"))?.map_err(anyhow::Error::msg),_=cancelled(&cancel)=>bail!("reusable lookup cancelled")}
    }
    /// The caller must first verify the same strong ETag and total through its approved HTTP request.
    pub fn bind_reusable(
        &self,
        stream_object: &str,
        candidate: &ReusableRepresentation,
    ) -> Result<usize> {
        ensure!(
            candidate.strong_etag == candidate.metadata.strong_etag
                && candidate.total == candidate.metadata.total
                && candidate.block_size == candidate.metadata.block_size
                && candidate.offsets == candidate.files.keys().copied().collect::<Vec<_>>(),
            "reusable candidate public identity was changed"
        );
        ensure!(
            candidate.verified_bytes
                == candidate
                    .files
                    .values()
                    .filter_map(Weak::upgrade)
                    .map(|file| file.len as u64)
                    .sum::<u64>(),
            "reusable candidate byte count was changed"
        );
        ensure!(
            candidate
                .owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.core)),
            "reusable candidate belongs to another cache"
        );
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            !state.closing
                && !self.core.access.closing.load(Ordering::Acquire)
                && state.epoch == candidate.epoch,
            "reusable candidate was cleared or closed"
        );
        ensure!(
            state
                .reusable
                .get(&candidate.metadata.data_key)
                .is_some_and(|record| record.metadata == candidate.metadata
                    && candidate
                        .files
                        .iter()
                        .all(|(offset, file)| file.upgrade().is_some_and(|file| record
                            .files
                            .get(offset)
                            .is_some_and(|current| current.serial == file.serial)))),
            "reusable candidate was invalidated"
        );
        let object = state
            .objects
            .get(stream_object)
            .context("reusable stream lease missing")?;
        ensure!(
            object.clear_pending_epoch.is_none(),
            "reusable stream is deferred by clear"
        );
        let files: Vec<_> = candidate
            .files
            .iter()
            .filter_map(|(offset, file)| file.upgrade().map(|file| (*offset, file)))
            .collect();
        ensure!(
            state.blocks.len().saturating_add(files.len()) <= 8192,
            "reusable alias quota exhausted"
        );
        let mut count = 0;
        for (offset, file) in files {
            let key = (stream_object.to_owned(), offset);
            if state.blocks.contains_key(&key) {
                continue;
            }
            state.serial += 1;
            let touched = state.serial;
            state.blocks.insert(
                key,
                Block {
                    file,
                    ram: None,
                    touched,
                    deferred: false,
                    ready: true,
                },
            );
            count += 1;
        }
        state.objects.get_mut(stream_object).unwrap().reuse = Some(candidate.metadata.clone());
        self.core.refresh(&state);
        drop(state);
        self.core.wake();
        Ok(count)
    }
    pub async fn register_reusable_representation_async(
        self: &Arc<Self>,
        data_key: String,
        stream_object: String,
        strong_etag: String,
        total: u64,
        epoch: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<bool> {
        let metadata = Metadata {
            version: 2,
            data_key,
            strong_etag,
            total,
            block_size: self.options().block_size,
            ledger_epoch: 0,
        };
        metadata.validate()?;
        let (reply, receiver) = oneshot::channel();
        {
            let mut state = self
                .core
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            admit(self, &state, &cancel)?;
            if state.epoch != epoch {
                return Ok(false);
            }
            let object = state
                .objects
                .get(&stream_object)
                .context("reusable stream lease missing")?;
            if object.clear_pending_epoch.is_some() {
                return Ok(false);
            }
            let generation = object.generation;
            state.reuse_jobs.push_back(Job::Register {
                metadata,
                object: stream_object,
                generation,
                epoch,
                cancel: cancel.clone(),
                reply,
            });
        }
        self.core.wake();
        tokio::select! {value=receiver=>value.map_err(|_|anyhow::anyhow!("reusable registration worker closed"))?.map_err(anyhow::Error::msg),_=cancelled(&cancel)=>bail!("reusable registration cancelled")}
    }
    pub async fn invalidate_reusable_async(
        self: &Arc<Self>,
        data_key: String,
        epoch: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<bool> {
        let (reply, receiver) = oneshot::channel();
        {
            let mut state = self
                .core
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            admit(self, &state, &cancel)?;
            if state.epoch != epoch {
                return Ok(false);
            }
            state.reuse_jobs.push_back(Job::Invalidate {
                data_key,
                expected: None,
                epoch,
                cancel: cancel.clone(),
                reply,
            });
        }
        self.core.wake();
        tokio::select! {value=receiver=>value.map_err(|_|anyhow::anyhow!("reusable invalidation worker closed"))?.map_err(anyhow::Error::msg),_=cancelled(&cancel)=>bail!("reusable invalidation cancelled")}
    }
    pub async fn invalidate_reusable_candidate_async(
        self: &Arc<Self>,
        candidate: &ReusableRepresentation,
        cancel: Arc<AtomicBool>,
    ) -> Result<bool> {
        ensure!(
            candidate
                .owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.core)),
            "reusable candidate belongs to another cache"
        );
        let (reply, receiver) = oneshot::channel();
        {
            let mut state = self
                .core
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            admit(self, &state, &cancel)?;
            if state.epoch != candidate.epoch {
                return Ok(false);
            }
            state.reuse_jobs.push_back(Job::Invalidate {
                data_key: candidate.metadata.data_key.clone(),
                expected: Some(candidate.metadata.clone()),
                epoch: candidate.epoch,
                cancel: cancel.clone(),
                reply,
            });
        }
        self.core.wake();
        tokio::select! {value=receiver=>value.map_err(|_|anyhow::anyhow!("reusable invalidation worker closed"))?.map_err(anyhow::Error::msg),_=cancelled(&cancel)=>bail!("reusable invalidation cancelled")}
    }
}
use anyhow::Context;
fn admit(store: &CacheStore, state: &State, cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !state.closing
            && !store.core.access.closing.load(Ordering::Acquire)
            && !cancel.load(Ordering::Acquire),
        "reusable operation cancelled"
    );
    ensure!(
        state.reuse_jobs.len() < 16,
        "reusable operation queue quota exhausted"
    );
    if let Some(error) = &state.init_error {
        bail!("{error}");
    }
    Ok(())
}
async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}
pub(super) fn candidate(
    core: &Arc<Core>,
    metadata: Metadata,
    files: &BTreeMap<u64, Arc<FileBlock>>,
    epoch: u64,
) -> ReusableRepresentation {
    ReusableRepresentation {
        strong_etag: metadata.strong_etag.clone(),
        total: metadata.total,
        block_size: metadata.block_size,
        verified_bytes: files.values().map(|file| file.len as u64).sum(),
        offsets: files.keys().copied().collect(),
        metadata,
        files: files
            .iter()
            .map(|(offset, file)| (*offset, Arc::downgrade(file)))
            .collect(),
        owner: Arc::downgrade(core),
        epoch,
    }
}
