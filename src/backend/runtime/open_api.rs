use super::*;
use crate::backend::external_control::ExternalSessionGate;
use crate::domain::lists::LOVE_LIST;
use crate::open_api::{
    ArtworkReference, ArtworkResponse, ControlAction, ControlError, ControlErrorKind,
    ControlResult, LxPlayerStatus, PlayerState, StatusSnapshot, StatusUpdate,
};
use std::sync::{RwLock, Weak};

const MAX_STATUS_HISTORY: usize = 64;
const MAX_STATUS_HISTORY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct ExternalControlRuntime {
    epoch: u64,
    session: Option<Weak<ExternalSessionGate>>,
    cursor: u64,
    current: Option<Arc<SharedStatus>>,
    history: VecDeque<(u64, Arc<SharedStatus>, usize)>,
    bytes: usize,
    collection: Option<(String, u64, bool)>,
    lyrics: Option<(Arc<LyricSnapshot>, Arc<[String; 4]>)>,
    artwork: Option<PublishedArtwork>,
    artwork_checked: Option<(Instant, ArtworkProjectionKey)>,
}
#[derive(PartialEq, Eq)]
struct ArtworkProjectionKey {
    session_epoch: u64,
    generation: u64,
    revision: u64,
    catalog_revision: u64,
    source_epoch: u64,
    pictures: (u64, u64),
    track: Option<String>,
}
struct PublishedArtwork {
    reference: ArtworkReference,
    response: ArtworkResponse,
}
struct SharedStatus {
    fields: LxPlayerStatus,
    lyrics: Option<Arc<[String; 4]>>,
}
impl PartialEq for SharedStatus {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
            && match (&self.lyrics, &other.lyrics) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}
impl SharedStatus {
    fn export(&self) -> LxPlayerStatus {
        let mut status = self.fields.clone();
        if let Some(texts) = &self.lyrics {
            status.lyric = texts[0].clone();
            status.tlyric = texts[1].clone();
            status.rlyric = texts[2].clone();
            status.lxlyric = texts[3].clone();
        }
        status
    }
}
impl ExternalControlRuntime {
    fn push(&mut self, status: SharedStatus) {
        if self.current.as_deref() == Some(&status) {
            return;
        }
        let fields = &status.fields;
        let bytes = 1024usize
            .saturating_add(
                [
                    &fields.name,
                    &fields.singer,
                    &fields.album_name,
                    &fields.pic_url,
                    &fields.lyric_line_text,
                    &fields.lyric_line_all_text,
                ]
                .iter()
                .map(|text| text.len().saturating_mul(2))
                .sum::<usize>(),
            )
            .saturating_add(
                status
                    .lyrics
                    .as_ref()
                    .map_or(0, |texts| texts.iter().map(String::len).sum()),
            );
        self.cursor = self.cursor.saturating_add(1);
        let status = Arc::new(status);
        self.current = Some(status.clone());
        while !self.history.is_empty()
            && (self.history.len() >= MAX_STATUS_HISTORY
                || self.bytes.saturating_add(bytes) > MAX_STATUS_HISTORY_BYTES)
        {
            if let Some((_, _, old)) = self.history.pop_front() {
                self.bytes -= old;
            }
        }
        if bytes <= MAX_STATUS_HISTORY_BYTES {
            self.bytes += bytes;
            self.history.push_back((self.cursor, status, bytes));
        }
    }
    pub(super) fn retire(&mut self) {
        self.artwork_checked = None;
        self.retire_artwork();
        if let Some(gate) = self.session.take().and_then(|gate| gate.upgrade()) {
            gate.retire();
        }
    }
    fn retire_artwork(&mut self) {
        if let Some(published) = self.artwork.take() {
            published.response.retired.store(true, Ordering::Release);
        }
    }
}

impl Runtime {
    pub(super) fn open_external_session(
        &mut self,
    ) -> Result<Arc<ExternalSessionGate>, BackendError> {
        if self
            .external_control
            .session
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|gate| *gate.alive.read().unwrap_or_else(|error| error.into_inner()))
        {
            return Err(BackendError::new(
                ErrorKind::Busy,
                "Retire the active endpoint before replacing its session",
            ));
        }
        self.external_control.retire();
        self.external_control.epoch =
            self.external_control.epoch.checked_add(1).ok_or_else(|| {
                BackendError::new(ErrorKind::Closed, "External session epoch exhausted")
            })?;
        let gate = Arc::new(ExternalSessionGate {
            epoch: self.external_control.epoch,
            alive: RwLock::new(true),
            retired: AtomicBool::new(false),
            artwork: RwLock::new(None),
        });
        self.external_control.session = Some(Arc::downgrade(&gate));
        Ok(gate)
    }
    fn check_external_session(&self, gate: &Arc<ExternalSessionGate>) -> Result<(), BackendError> {
        if gate.epoch != self.external_control.epoch
            || !self
                .external_control
                .session
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|active| Arc::ptr_eq(&active, gate))
        {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "External control session changed",
            ));
        }
        Ok(())
    }
    pub(super) fn external_snapshot(
        &mut self,
        gate: &Arc<ExternalSessionGate>,
    ) -> Result<StatusSnapshot, BackendError> {
        self.check_external_session(gate)?;
        let alive = gate.alive.read().unwrap_or_else(|error| error.into_inner());
        if !*alive {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "External session retired",
            ));
        }
        self.external_control.artwork_checked = None;
        self.refresh();
        self.project_external_status().map_err(backend_error)?;
        Ok(StatusSnapshot {
            status: self
                .external_control
                .current
                .as_deref()
                .map(SharedStatus::export)
                .unwrap_or_default(),
            cursor: self.external_control.cursor,
        })
    }
    pub(super) fn external_updates(
        &mut self,
        gate: &Arc<ExternalSessionGate>,
        cursor: u64,
    ) -> Result<Vec<StatusUpdate>, BackendError> {
        let current = self.external_snapshot(gate)?;
        if cursor > current.cursor
            || self
                .external_control
                .history
                .front()
                .is_some_and(|(oldest, _, _)| cursor.saturating_add(1) < *oldest)
        {
            return Err(BackendError::new(
                ErrorKind::CursorExpired,
                "External status cursor expired",
            ));
        }
        Ok(if cursor == current.cursor {
            Vec::new()
        } else {
            vec![StatusUpdate {
                cursor: current.cursor,
                status: current.status,
            }]
        })
    }
    pub(super) fn external_artwork(
        &mut self,
        gate: &Arc<ExternalSessionGate>,
        reference: ArtworkReference,
    ) -> ControlResult<ArtworkResponse> {
        reference.validate()?;
        self.check_external_session(gate)
            .map_err(crate::backend::external_control::control_error)?;
        // Refresh runs on the command owner and revokes changed song/grant/cache
        // publications before selecting bytes for this request.
        self.external_control.artwork_checked = None;
        self.refresh();
        let alive = gate.alive.read().unwrap_or_else(|error| error.into_inner());
        if !*alive || reference.session_epoch != gate.epoch {
            return Err(ControlError::new(
                ControlErrorKind::Unauthorized,
                "External artwork session retired",
            ));
        }
        let published = self
            .external_control
            .artwork
            .as_ref()
            .filter(|published| {
                published.reference == reference
                    && !published.response.retired.load(Ordering::Acquire)
            })
            .ok_or_else(|| {
                ControlError::new(
                    ControlErrorKind::Unavailable,
                    "Current verified artwork changed",
                )
            })?;
        Ok(published.response.clone())
    }
    pub(super) fn project_external_status(&mut self) -> Result<()> {
        let Some(gate) = self
            .external_control
            .session
            .as_ref()
            .and_then(Weak::upgrade)
        else {
            self.external_control.retire_artwork();
            return Ok(());
        };
        // Command execution already holds the live read gate. An atomic sample
        // avoids a recursive read acquisition while retirement waits to write.
        if gate.retired.load(Ordering::Acquire) {
            self.external_control.retire_artwork();
            return Ok(());
        }
        let snapshot = self.shared.lock().unwrap().clone();
        let artwork_key = ArtworkProjectionKey {
            session_epoch: gate.epoch,
            generation: snapshot.playback.generation,
            revision: self.revision,
            catalog_revision: self.catalog.revision(),
            source_epoch: self.source_epoch,
            pictures: self.open_api_picture_revision(),
            track: snapshot.playback.track_id.clone(),
        };
        if !self
            .external_control
            .artwork_checked
            .as_ref()
            .is_some_and(|(checked, key)| {
                key == &artwork_key && checked.elapsed() < Duration::from_millis(100)
            })
        {
            let artwork = self
                .current_record
                .as_ref()
                .filter(|record| snapshot.playback.track_id.as_deref() == Some(record.id.as_str()))
                .and_then(|_| self.current_open_api_artwork().ok().flatten());
            let candidate = artwork.map(|artwork| {
                (
                    ArtworkReference {
                        session_epoch: gate.epoch,
                        generation: snapshot.playback.generation,
                        key_sha256: artwork.key_sha256,
                        sha256: artwork.sha256,
                    },
                    artwork.mime_type,
                    artwork.bytes,
                )
            });
            if self
                .external_control
                .artwork
                .as_ref()
                .map(|published| &published.reference)
                != candidate.as_ref().map(|(reference, _, _)| reference)
            {
                self.external_control.retire_artwork();
                if let Some((reference, mime_type, bytes)) = candidate {
                    let retired = Arc::new(AtomicBool::new(false));
                    *gate
                        .artwork
                        .write()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some(Arc::downgrade(&retired));
                    if gate.retired.load(Ordering::Acquire) {
                        retired.store(true, Ordering::Release);
                    }
                    self.external_control.artwork = Some(PublishedArtwork {
                        reference,
                        response: ArtworkResponse {
                            mime_type,
                            bytes,
                            retired,
                        },
                    });
                }
            }
            self.external_control.artwork_checked = Some((Instant::now(), artwork_key));
        }
        let record = self.current_record.as_ref();
        let mut status = LxPlayerStatus {
            status: match snapshot.playback.status {
                PlaybackStatus::Playing => PlayerState::Playing,
                PlaybackStatus::Paused => PlayerState::Paused,
                PlaybackStatus::Error => PlayerState::Error,
                _ => PlayerState::Stoped,
            },
            progress: if snapshot.playback.position_seconds.is_finite() {
                snapshot.playback.position_seconds.max(0.0)
            } else {
                0.0
            },
            duration: if snapshot.playback.duration_known
                && snapshot.playback.duration_seconds.is_finite()
            {
                snapshot.playback.duration_seconds.max(0.0)
            } else {
                0.0
            },
            playback_rate: self.data.playback_processing.playback_rate,
            volume: saved_volume_percent(self.data.volume),
            mute: self.data.audio_preferences.muted,
            ..Default::default()
        };
        if let Some(record) = record {
            status.name = record.title.clone();
            status.singer = record.artists.join("、");
            status.album_name = record.album.clone();
            status.pic_url = self
                .external_control
                .artwork
                .as_ref()
                .map(|published| published.reference.path())
                .transpose()?
                .unwrap_or_default();
            let cache = &self.external_control.collection;
            status.collect = if let Some((id, revision, collected)) = cache
                && id == &record.id
                && *revision == self.revision
            {
                *collected
            } else {
                let collected = self.repository.list_contains(LOVE_LIST, &record.lx_id)?
                    || self.repository.list_contains(LOVE_LIST, &record.id)?;
                self.external_control.collection =
                    Some((record.id.clone(), self.revision, collected));
                collected
            };
        }
        if let Some(lyrics) = snapshot.lyrics.as_ref().filter(|lyrics| {
            lyrics.generation == snapshot.playback.generation
                && snapshot.playback.track_id.as_deref() == Some(lyrics.track_id.as_str())
        }) {
            if !self
                .external_control
                .lyrics
                .as_ref()
                .is_some_and(|(cached, _)| Arc::ptr_eq(cached, lyrics))
            {
                self.external_control.lyrics = Some((
                    lyrics.clone(),
                    Arc::new([
                        lyrics.bundle.lyric.text().unwrap_or("").to_owned(),
                        lyrics.bundle.tlyric.text().unwrap_or("").to_owned(),
                        lyrics.bundle.rlyric.text().unwrap_or("").to_owned(),
                        lyrics.bundle.lxlyric.text().unwrap_or("").to_owned(),
                    ]),
                ));
            }
            if let Some(frame) = snapshot.lyric_frame.as_ref().filter(|frame| {
                frame.generation == snapshot.playback.generation
                    && frame.seek_seq == snapshot.playback.seek_seq
            }) {
                status.lyric_line_text = frame.line_text.clone();
                status.lyric_line_all_text = std::iter::once(frame.line_text.as_str())
                    .chain(frame.extended.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        } else {
            self.external_control.lyrics = None;
        }
        self.external_control.push(SharedStatus {
            fields: status,
            lyrics: self
                .external_control
                .lyrics
                .as_ref()
                .map(|(_, texts)| texts.clone()),
        });
        Ok(())
    }
    pub(super) fn external_command(
        &mut self,
        gate: &Arc<ExternalSessionGate>,
        action: ControlAction,
        request_id: String,
    ) -> Result<(), BackendError> {
        self.check_external_session(gate)?;
        let alive = gate.alive.read().unwrap_or_else(|error| error.into_inner());
        if !*alive {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "External session retired",
            ));
        }
        if let Some(command) = self.control_action_command(action)? {
            self.execute(CommandEnvelope {
                request_id,
                origin: "open_api".into(),
                expected_revision: None,
                command,
            })?;
        }
        Ok(())
    }
    pub(super) fn control_action_command(
        &self,
        action: ControlAction,
    ) -> Result<Option<BackendCommand>, BackendError> {
        let mut command = match action {
            ControlAction::Play => BackendCommand::Resume,
            ControlAction::Pause => BackendCommand::Pause,
            ControlAction::Toggle => BackendCommand::Toggle,
            ControlAction::Previous => BackendCommand::Previous,
            ControlAction::Next => BackendCommand::Next,
            ControlAction::Stop => BackendCommand::Stop,
            ControlAction::DislikeCurrent => {
                if self.current_record.is_none() {
                    return Ok(None);
                }
                BackendCommand::DislikeCurrent
            }
            ControlAction::SetMute { muted } => BackendCommand::SetMute { muted },
            ControlAction::ToggleMute => BackendCommand::SetMute {
                muted: !self.data.audio_preferences.muted,
            },
            ControlAction::SetVolume { percent } => {
                ensure_external(percent <= 100, "Invalid volume")?;
                BackendCommand::Volume {
                    volume: percent as f32 / 100.0,
                }
            }
            ControlAction::VolumeRelative { percent } => BackendCommand::Volume {
                volume: (self.data.volume + percent as f32 / 100.0).clamp(0.0, 1.0),
            },
            ControlAction::Seek { seconds } => self.external_seek(seconds, seconds)?,
            ControlAction::SeekChecked {
                requested_seconds,
                seconds,
            } => self.external_seek(requested_seconds, seconds)?,
            ControlAction::SeekRelative { seconds } => {
                ensure_external(seconds.is_finite(), "Invalid relative seek")?;
                let audio = self.audio.snapshot();
                ensure_external(audio.duration_known, "Current duration is unknown")?;
                let requested =
                    (audio.position_seconds + seconds).clamp(0.0, audio.duration_seconds);
                self.external_seek(requested, (requested * 1000.0).round() / 1000.0)?
            }
            ControlAction::CollectCurrent { collected } => {
                let Some(record) = self.current_record.as_ref() else {
                    return Ok(None);
                };
                let present = self
                    .repository
                    .list_contains(LOVE_LIST, &record.lx_id)
                    .map_err(backend_error)?
                    || self
                        .repository
                        .list_contains(LOVE_LIST, &record.id)
                        .map_err(backend_error)?;
                if present == collected {
                    return Ok(None);
                }
                BackendCommand::ListAction {
                    action: Box::new(if collected {
                        ListAction::MusicAdd {
                            list_id: LOVE_LIST.into(),
                            tracks: vec![record.as_ref().clone()],
                            location: AddLocation::Top,
                        }
                    } else {
                        ListAction::MusicRemove {
                            list_id: LOVE_LIST.into(),
                            ids: vec![record.lx_id.clone()],
                        }
                    }),
                }
            }
        };
        if matches!(command, BackendCommand::Resume) && self.current_record.is_none() {
            let Some(track) = self.catalog.tracks().first().cloned() else {
                return Ok(None);
            };
            command = BackendCommand::Play { id: track.id };
        }
        Ok(Some(command))
    }
    fn external_seek(&self, requested: f64, seconds: f64) -> Result<BackendCommand, BackendError> {
        ensure_external(
            requested.is_finite()
                && seconds.is_finite()
                && requested >= 0.0
                && (0.0..=30.0 * 24.0 * 60.0 * 60.0).contains(&seconds),
            "Invalid offset",
        )?;
        let audio = self.audio.snapshot();
        let duration = if self.restore_pending {
            self.current_record
                .as_ref()
                .and_then(|record| record.duration_ms)
                .map(|ms| ms as f64 / 1000.0)
        } else {
            audio.duration_known.then_some(audio.duration_seconds)
        };
        ensure_external(
            duration.is_some_and(|duration| requested <= duration),
            "Invalid offset",
        )?;
        Ok(BackendCommand::Seek { seconds })
    }
}
fn ensure_external(condition: bool, message: &str) -> Result<(), BackendError> {
    if condition {
        Ok(())
    } else {
        Err(BackendError::new(ErrorKind::InvalidData, message))
    }
}
fn saved_volume_percent(volume: f32) -> u8 {
    // Project the stored decimal setting through JavaScript's double operation,
    // rather than introducing the extra precision of a direct f32-to-f64 cast.
    let decimal = volume
        .clamp(0.0, 1.0)
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    (decimal * 100.0).trunc() as u8
}
