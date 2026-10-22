//! Lyric acquisition is prepared off the actor thread and committed at its generation barrier.

use super::{
    local::{LocalLyricProvenance, read_authorized_local_lyrics},
    parser::{LyricDocument, ParserMode, parse_bundle},
};
use crate::{
    domain::{
        lyric::{LyricBundle, LyricField, LyricOrigin},
        music::TrackRecord,
    },
    repository::{Repository, RepositoryChange},
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug)]
pub struct LyricRequest {
    /// The actor's immutable list-item snapshot; it never updates the global track.
    pub record_override: Option<Arc<TrackRecord>>,
    pub platform_resolver: bool,
    pub track_id: String,
    pub generation: u64,
    pub cancel: Arc<AtomicBool>,
    /// Explicit resolver refresh bypasses edited/file/cache selection without deleting them.
    pub force_refresh: bool,
    pub parser_mode: ParserMode,
}

impl LyricRequest {
    pub fn new(track_id: impl Into<String>, generation: u64) -> Self {
        Self {
            record_override: None,
            platform_resolver: false,
            track_id: track_id.into(),
            generation,
            cancel: Arc::new(AtomicBool::new(false)),
            force_refresh: false,
            parser_mode: ParserMode::LxCompatible,
        }
    }

    pub fn check_cancelled(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(LyricServiceError::Cancelled.into());
        }
        Ok(())
    }
}

/// The caller supplies this only after verifying the selected source's current approval.
/// The service never starts, approves, or restores source execution on its own.
pub type AuthorizedLyricResolver<'a> =
    dyn FnMut(&TrackRecord, &LyricRequest) -> Result<LyricBundle> + 'a;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LyricProvenance {
    Edited,
    LocalSidecar,
    LocalEmbedded,
    RawCache,
    AuthorizedSource,
    PlatformSdk,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LyricServiceError {
    Cancelled,
    StaleGeneration,
    ChangedTrack,
    RevisionConflict,
    Unavailable,
}

impl fmt::Display for LyricServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "Lyric operation was cancelled",
            Self::StaleGeneration => "Lyric result belongs to an old playback generation",
            Self::ChangedTrack => "Lyric result belongs to a different current track",
            Self::RevisionConflict => {
                "Lyric preparation conflicts with a newer repository revision"
            }
            Self::Unavailable => "No available lyric content for this track",
        })
    }
}
impl std::error::Error for LyricServiceError {}

#[derive(Debug)]
pub struct PreparedLyrics {
    pub track_id: String,
    pub generation: u64,
    pub bundle: LyricBundle,
    pub document: LyricDocument,
    pub provenance: LyricProvenance,
    pub warnings: Vec<String>,
    cancel: Arc<AtomicBool>,
    repository_revision: u64,
    raw_to_store: Option<LyricBundle>,
}

impl PreparedLyrics {
    pub fn repository_revision(&self) -> u64 {
        self.repository_revision
    }
}

#[derive(Debug)]
pub struct CommittedLyrics {
    pub track_id: String,
    pub generation: u64,
    pub bundle: LyricBundle,
    pub document: LyricDocument,
    pub provenance: LyricProvenance,
    pub warnings: Vec<String>,
    pub revision: u64,
    pub change: Option<RepositoryChange>,
}

#[derive(Clone)]
pub struct LyricService {
    repository: Arc<Repository>,
    authority: crate::library::authority::AuthorityRegistry,
}

impl LyricService {
    pub fn new(repository: Arc<Repository>) -> Self {
        Self::with_authority(
            repository,
            crate::library::authority::AuthorityRegistry::default(),
        )
    }
    pub fn with_authority(
        repository: Arc<Repository>,
        authority: crate::library::authority::AuthorityRegistry,
    ) -> Self {
        Self {
            repository,
            authority,
        }
    }

    /// No writes. Every fetched result remains private until commit validates its generation.
    pub fn prepare(
        &self,
        request: LyricRequest,
        mut resolver: Option<&mut AuthorizedLyricResolver<'_>>,
    ) -> Result<PreparedLyrics> {
        request.check_cancelled()?;
        let revision = self.repository.revision()?;
        let track = match &request.record_override {
            Some(track) => {
                ensure!(
                    track.id == request.track_id,
                    "Lyric record snapshot belongs to a different track"
                );
                track.as_ref().clone()
            }
            None => self
                .repository
                .track(&request.track_id)?
                .context("Lyric track does not exist")?,
        };
        let mut warnings = Vec::new();
        let mut selection: Option<(LyricBundle, LyricProvenance, bool)> = None;
        if !request.force_refresh {
            if let Some(edited) = self.repository.lyrics(&track.id, LyricOrigin::Edited)?
                && usable_edited(&edited, request.parser_mode)?
            {
                selection = Some((edited, LyricProvenance::Edited, false));
            }
            request.check_cancelled()?;
            if selection.is_none()
                && track.platform.source == "local"
                && let Some(asset) = self.repository.local_asset_for_track(&track.id)?
            {
                let local = (|| -> Result<_> {
                    ensure!(asset.available, "Local lyric asset is unavailable");
                    let intent = self
                        .repository
                        .local_asset_authority(&asset.id)?
                        .context("Local lyric file has no current grant")?;
                    ensure!(
                        intent.explicit_file_epoch.is_some() || !intent.roots.is_empty(),
                        "Local lyric file has no current grant"
                    );
                    let authorization = self
                        .authority
                        .install(
                            &intent,
                            self.repository.local_asset_file_identity(&asset.id)?,
                        )?
                        .with_cancel(request.cancel.clone())?;
                    read_authorized_local_lyrics(&authorization)
                })();
                match local {
                    Ok(Some(local)) => {
                        warnings.extend(local.warnings);
                        local.bundle.validate()?;
                        if has_file_content(&local.bundle) {
                            let provenance = match local.provenance {
                                LocalLyricProvenance::Sidecar => LyricProvenance::LocalSidecar,
                                LocalLyricProvenance::Embedded => LyricProvenance::LocalEmbedded,
                            };
                            selection = Some((local.bundle, provenance, true));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => warnings.push(format!("Local lyric read failed: {error:#}")),
                }
            }
            request.check_cancelled()?;
            if selection.is_none()
                && let Some(raw) = self.repository.lyrics(&track.id, LyricOrigin::Raw)?
                && usable_raw(&raw, request.parser_mode)?
            {
                selection = Some((raw, LyricProvenance::RawCache, false));
            }
        }
        request.check_cancelled()?;
        if selection.is_none()
            && let Some(resolve) = resolver.as_mut()
        {
            let bundle = resolve(&track, &request)?;
            request.check_cancelled()?;
            // An authorized local-source response is still subject to the LX result contract.
            if request.platform_resolver {
                bundle.validate()?;
            } else {
                crate::source::validate_source_lyrics(&bundle)?;
            }
            selection = Some((
                bundle,
                if request.platform_resolver {
                    LyricProvenance::PlatformSdk
                } else {
                    LyricProvenance::AuthorizedSource
                },
                true,
            ));
        }
        request.check_cancelled()?;
        let (bundle, provenance, save_raw) = selection.ok_or(LyricServiceError::Unavailable)?;
        let document = parse_bundle(&bundle, request.parser_mode)?;
        ensure!(
            self.repository.revision()? == revision,
            LyricServiceError::RevisionConflict
        );
        let raw_to_store = if save_raw {
            // Re-reading an unchanged sidecar does not produce a new repository event.
            let previous = self.repository.lyrics(&track.id, LyricOrigin::Raw)?;
            (previous.as_ref() != Some(&bundle)).then(|| bundle.clone())
        } else {
            None
        };
        request.check_cancelled()?;
        ensure!(
            self.repository.revision()? == revision,
            LyricServiceError::RevisionConflict
        );
        Ok(PreparedLyrics {
            track_id: track.id,
            generation: request.generation,
            bundle,
            document,
            provenance,
            warnings,
            cancel: request.cancel,
            repository_revision: revision,
            raw_to_store,
        })
    }

    /// Called only at the actor's current-track/generation barrier. Persistence precedes events.
    pub fn commit(
        &self,
        prepared: PreparedLyrics,
        current_track: &str,
        generation: u64,
    ) -> Result<CommittedLyrics> {
        ensure!(
            !prepared.cancel.load(Ordering::Acquire),
            LyricServiceError::Cancelled
        );
        ensure!(
            prepared.generation == generation,
            LyricServiceError::StaleGeneration
        );
        ensure!(
            prepared.track_id == current_track,
            LyricServiceError::ChangedTrack
        );
        ensure!(
            self.repository.revision()? == prepared.repository_revision,
            LyricServiceError::RevisionConflict
        );
        let change = prepared
            .raw_to_store
            .as_ref()
            .map(|bundle| {
                self.repository.replace_lyrics_at_revision(
                    &prepared.track_id,
                    LyricOrigin::Raw,
                    bundle,
                    Some(prepared.repository_revision),
                )
            })
            .transpose()?;
        let revision = change
            .as_ref()
            .map_or(prepared.repository_revision, |change| change.revision);
        Ok(CommittedLyrics {
            track_id: prepared.track_id,
            generation: prepared.generation,
            bundle: prepared.bundle,
            document: prepared.document,
            provenance: prepared.provenance,
            warnings: prepared.warnings,
            revision,
            change,
        })
    }

    pub fn save_edited(
        &self,
        track_id: &str,
        bundle: &LyricBundle,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        bundle.validate()?;
        ensure!(
            matches!(bundle.lyric, LyricField::Text(_)),
            "Edited main lyric must be text, including an intentional empty string"
        );
        // Parser validation happens before the transaction. Untimed edits remain exportable,
        // but only a timed nonempty main track overrides synchronized playback lyrics.
        parse_bundle(bundle, ParserMode::LxCompatible)?;
        self.repository.replace_lyrics_at_revision(
            track_id,
            LyricOrigin::Edited,
            bundle,
            expected_revision,
        )
    }

    pub fn restore_raw(&self, track_id: &str) -> Result<RepositoryChange> {
        self.repository.restore_raw_lyrics(track_id)
    }

    pub fn clear_raw(&self, track_id: &str) -> Result<RepositoryChange> {
        self.repository.clear_raw_lyrics(track_id)
    }

    /// Exports existing data only; does not read files or activate a resolver.
    pub fn export_cached(&self, track_id: &str, format: LyricExportFormat) -> Result<LyricExport> {
        let edited = self.repository.lyrics(track_id, LyricOrigin::Edited)?;
        let selected = match edited {
            Some(bundle) if bundle.lyric.text().is_some() => Some(bundle),
            _ => self.repository.lyrics(track_id, LyricOrigin::Raw)?,
        }
        .ok_or(LyricServiceError::Unavailable)?;
        export_bundle(&selected, format)
    }
}

fn has_file_content(bundle: &LyricBundle) -> bool {
    // A selected empty sidecar is an explicit available result, not a cache miss.
    bundle.lyric.text().is_some() || bundle.lxlyric.text().is_some()
}

fn usable_edited(bundle: &LyricBundle, mode: ParserMode) -> Result<bool> {
    Ok(bundle
        .lyric
        .text()
        .is_some_and(|text| !text.trim().is_empty())
        && !parse_bundle(bundle, mode)?.lines.is_empty())
}

fn usable_raw(bundle: &LyricBundle, mode: ParserMode) -> Result<bool> {
    // Fetched empty is a valid negative cache entry, unlike missing/null.
    if bundle.lyric.text() == Some("") {
        bundle.validate()?;
        return Ok(true);
    }
    Ok(bundle.lyric.text().is_some() && !parse_bundle(bundle, mode)?.lines.is_empty())
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LyricExportFormat {
    Lrc,
    Lx,
    BundleJson,
    Awlrc,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LyricExport {
    pub extension: String,
    pub mime: String,
    pub text: String,
}

/// The LX awlrc container preserves available channel text; JSON also preserves null/missing.
pub fn export_bundle(bundle: &LyricBundle, format: LyricExportFormat) -> Result<LyricExport> {
    bundle.validate()?;
    let (extension, mime, text) = match format {
        LyricExportFormat::Lrc => (
            "lrc",
            "text/plain; charset=utf-8",
            bundle
                .lyric
                .text()
                .ok_or(LyricServiceError::Unavailable)?
                .to_owned(),
        ),
        LyricExportFormat::Lx => (
            "lrc",
            "text/plain; charset=utf-8",
            bundle
                .lxlyric
                .text()
                .or_else(|| bundle.lyric.text())
                .ok_or(LyricServiceError::Unavailable)?
                .to_owned(),
        ),
        LyricExportFormat::BundleJson => (
            "json",
            "application/json; charset=utf-8",
            serde_json::to_string_pretty(bundle)?,
        ),
        LyricExportFormat::Awlrc => {
            let fields = [
                ("lrc", &bundle.lyric),
                ("tlrc", &bundle.tlyric),
                ("rlrc", &bundle.rlyric),
                ("awlrc", &bundle.lxlyric),
            ];
            let parts = fields
                .iter()
                .filter_map(|(name, field)| {
                    field.text().map(|text| {
                        format!(
                            "{name}:{}",
                            base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
                        )
                    })
                })
                .collect::<Vec<_>>();
            ensure!(!parts.is_empty(), LyricServiceError::Unavailable);
            (
                "lrc",
                "text/plain; charset=utf-8",
                format!("[awlrc:{}]", parts.join(",")),
            )
        }
    };
    Ok(LyricExport {
        extension: extension.into(),
        mime: mime.into(),
        text,
    })
}
