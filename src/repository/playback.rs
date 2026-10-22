use super::{ChangeKind, Repository, RepositoryChange, schema};
use crate::playback::queue::QueueState;
use anyhow::{Result, ensure};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

pub const MAX_PLAYBACK_CHECKPOINT_BYTES: usize = 32 * 1024 * 1024;

/// Durable queue and user controls. Opening a checkpoint never starts audio.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackCheckpoint {
    pub queue: QueueState,
    pub position_ms: u64,
    #[serde(default)]
    pub lyric_offset_ms: i64,
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default = "default_auto_skip")]
    pub auto_skip_on_error: bool,
    #[serde(default)]
    pub auto_switch_source: bool,
}
fn default_quality() -> String {
    "128k".into()
}
fn default_auto_skip() -> bool {
    true
}
impl Default for PlaybackCheckpoint {
    fn default() -> Self {
        Self {
            queue: QueueState::default(),
            position_ms: 0,
            lyric_offset_ms: 0,
            quality: default_quality(),
            auto_skip_on_error: true,
            auto_switch_source: false,
        }
    }
}
impl PlaybackCheckpoint {
    pub fn validate(&self) -> Result<()> {
        self.queue.validate()?;
        ensure!(
            self.position_ms <= 30 * 24 * 60 * 60 * 1000,
            "Playback position exceeds thirty days"
        );
        ensure!(
            self.lyric_offset_ms.unsigned_abs() <= 24 * 60 * 60 * 1000,
            "Lyric offset exceeds one day"
        );
        ensure!(
            matches!(
                self.quality.as_str(),
                "128k" | "320k" | "flac" | "flac24bit"
            ),
            "Playback quality identifier is invalid"
        );
        Ok(())
    }
}
impl Repository {
    pub fn playback_checkpoint(&self) -> Result<Option<PlaybackCheckpoint>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let document: Option<String> = connection
            .query_row(
                "SELECT document FROM playback_state WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        document
            .map(|document| {
                ensure!(
                    document.len() <= MAX_PLAYBACK_CHECKPOINT_BYTES,
                    "Playback checkpoint exceeds 32 MiB"
                );
                let checkpoint: PlaybackCheckpoint = serde_json::from_str(&document)?;
                checkpoint.validate()?;
                Ok(checkpoint)
            })
            .transpose()
    }
    pub fn save_playback_checkpoint(
        &self,
        checkpoint: &PlaybackCheckpoint,
        expected_revision: Option<u64>,
        origin: &str,
    ) -> Result<RepositoryChange> {
        checkpoint.validate()?;
        ensure!(
            !origin.is_empty() && origin.len() <= 256 && !origin.contains('\0'),
            "Playback origin is invalid"
        );
        let document = serde_json::to_string(checkpoint)?;
        ensure!(
            document.len() <= MAX_PLAYBACK_CHECKPOINT_BYTES,
            "Playback checkpoint exceeds 32 MiB"
        );
        let current = checkpoint.queue.current.as_ref();
        let (_, change) = self.write(expected_revision, |tx| {
            tx.execute("INSERT INTO playback_state(singleton,document) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET document=excluded.document", [&document])?;
            Ok(((), ChangeKind::PlaybackCheckpointChanged, current.map(|entry| entry.track_id.clone()), serde_json::json!({ "origin":origin, "queue_revision":checkpoint.queue.revision, "list_id":checkpoint.queue.origin_list, "mode":checkpoint.queue.mode, "current_entry":current.map(|entry| &entry.entry_id), "position_ms":checkpoint.position_ms })))
        })?;
        Ok(change)
    }
}
