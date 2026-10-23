use super::{ChangeKind, Repository, RepositoryChange, decode_document, encode_document};
use crate::domain::music::{LocalAsset, PlatformTrackRef, TrackRecord, local_track_id};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub const LEGACY_LOCAL_IMPORT_MARKER: &str = "project-store-v1-local-files";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyLocalImport {
    pub imported: Vec<LocalAsset>,
    pub revision: u64,
    pub already_imported: bool,
}

impl Repository {
    pub fn upsert_track(&self, track: &TrackRecord) -> Result<RepositoryChange> {
        self.upsert_track_at_revision(track, None)
    }

    pub fn upsert_track_at_revision(
        &self,
        track: &TrackRecord,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        validate_track(track)?;
        let document = encode_document(track)?;
        let (_, change) = self.write(expected_revision, |transaction| {
            write_track(transaction, track, &document)?;
            Ok((
                (),
                ChangeKind::TrackUpserted,
                Some(track.id.clone()),
                serde_json::json!({}),
            ))
        })?;
        Ok(change)
    }

    pub fn track(&self, track_id: &str) -> Result<Option<TrackRecord>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let document: Option<String> = connection
            .query_row(
                "SELECT document FROM tracks WHERE id=?1",
                [track_id],
                |row| row.get(0),
            )
            .optional()?;
        document
            .map(|document| decode_document(&document))
            .transpose()
    }

    pub fn tracks(&self) -> Result<Vec<TrackRecord>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let mut statement = connection.prepare("SELECT document FROM tracks ORDER BY id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| decode_document(&row?)).collect()
    }

    /// Track and asset are committed together; a colliding asset identity cannot rebind another track.
    pub fn upsert_local_track(
        &self,
        track: &TrackRecord,
        asset: &LocalAsset,
    ) -> Result<RepositoryChange> {
        validate_track(track)?;
        validate_asset(asset)?;
        ensure!(
            asset.track_id == track.id,
            "Local asset references a different track"
        );
        ensure!(
            track.local_asset_id.as_deref() == Some(asset.id.as_str()),
            "Track references a different local asset"
        );
        let track_document = encode_document(track)?;
        let asset_document = encode_document(asset)?;
        let (_, change) = self.write(None, |transaction| {
            write_track(transaction, track, &track_document)?;
            write_asset(transaction, asset, &asset_document)?;
            super::library::write_explicit_file_grant(transaction, asset)?;
            super::library::check_library_storage(transaction)?;
            Ok((
                (),
                ChangeKind::LocalAssetUpserted,
                Some(track.id.clone()),
                serde_json::json!({"asset_id":asset.id}),
            ))
        })?;
        Ok(change)
    }

    /// Commits a prepared batch with one revision/outbox record, including all local file mappings.
    pub fn upsert_local_tracks(
        &self,
        items: &[(TrackRecord, LocalAsset)],
    ) -> Result<RepositoryChange> {
        ensure!(
            !items.is_empty() && items.len() <= 1000,
            "Local import batch must contain 1 to 1000 tracks"
        );
        let mut encoded = Vec::with_capacity(items.len());
        let mut total_bytes = 0usize;
        for (track, asset) in items {
            validate_track(track)?;
            validate_asset(asset)?;
            ensure!(
                asset.track_id == track.id,
                "Local asset references a different track"
            );
            ensure!(
                track.local_asset_id.as_deref() == Some(asset.id.as_str()),
                "Track references a different local asset"
            );
            let track_document = encode_document(track)?;
            let asset_document = encode_document(asset)?;
            total_bytes = total_bytes
                .checked_add(track_document.len())
                .and_then(|total| total.checked_add(asset_document.len()))
                .context("Local import batch size overflow")?;
            ensure!(
                total_bytes <= 32 * 1024 * 1024,
                "Local import batch exceeds 32 MiB"
            );
            encoded.push((track, asset, track_document, asset_document));
        }
        let (_, change) = self.write(None, |transaction| {
            for (track, asset, track_document, asset_document) in &encoded {
                write_track(transaction, track, track_document)?;
                write_asset(transaction, asset, asset_document)?;
                super::library::write_explicit_file_grant(transaction, asset)?;
            }
            super::library::check_library_storage(transaction)?;
            Ok(((), ChangeKind::LocalAssetsUpserted, None,
                serde_json::json!({"count":items.len(),"track_ids":items.iter().map(|(track,_)|&track.id).collect::<Vec<_>>()})))
        })?;
        Ok(change)
    }

    pub fn local_asset(&self, asset_id: &str) -> Result<Option<LocalAsset>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let document: Option<String> = connection
            .query_row(
                "SELECT document FROM local_assets WHERE identity_key=?1",
                [asset_id],
                |row| row.get(0),
            )
            .optional()?;
        document
            .map(|document| decode_document(&document))
            .transpose()
    }

    pub fn local_asset_for_track(&self, track_id: &str) -> Result<Option<LocalAsset>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let document: Option<String> = connection
            .query_row(
                "SELECT document FROM local_assets WHERE track_id=?1",
                [track_id],
                |row| row.get(0),
            )
            .optional()?;
        document
            .map(|document| decode_document(&document))
            .transpose()
    }

    pub fn local_assets(&self) -> Result<Vec<LocalAsset>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let mut statement =
            connection.prepare("SELECT document FROM local_assets ORDER BY identity_key")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| decode_document(&row?)).collect()
    }

    /// Imports only paths already present in a validated ProjectStore envelope.
    /// The caller retains that file and all source authorization records unchanged.
    pub fn import_legacy_local_files(
        &self,
        paths: &[PathBuf],
        source_digest: &str,
    ) -> Result<LegacyLocalImport> {
        ensure!(
            source_digest.len() == 64 && source_digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Legacy import requires the original file SHA-256"
        );
        if let Some(existing) = self.import_marker(LEGACY_LOCAL_IMPORT_MARKER)? {
            ensure!(
                existing == source_digest,
                "Legacy local-file import was already completed from different data"
            );
            return Ok(LegacyLocalImport {
                imported: self.local_assets()?,
                revision: self.revision()?,
                already_imported: true,
            });
        }
        let mut imports = std::collections::BTreeMap::new();
        for path in paths {
            let (track, asset) = local_track_from_path(path)?;
            if let Some(existing) = self.local_asset(&asset.id)?
                && crate::domain::music::normalize_local_path(&existing.path)
                    != crate::domain::music::normalize_local_path(&asset.path)
            {
                // A legacy JSON path cannot undo an already durable relocation
                // or grant access to a new object reusing the original path.
                continue;
            }
            imports.insert(asset.id.clone(), (track, asset));
        }
        let imported: Vec<LocalAsset> = imports.values().map(|(_, asset)| asset.clone()).collect();
        let (_, change) = self.write(None, |transaction| {
            let existing: Option<String> = transaction.query_row("SELECT source_digest FROM import_markers WHERE name=?1", [LEGACY_LOCAL_IMPORT_MARKER], |row| row.get(0)).optional()?;
            ensure!(existing.is_none(), "Legacy import raced with another operation; reload import state");
            for (track, asset) in imports.values() {
                write_track(transaction, track, &encode_document(track)?)?;
                write_asset(transaction, asset, &encode_document(asset)?)?;
                super::library::write_explicit_file_grant(transaction, asset)?;
            }
            super::library::check_library_storage(transaction)?;
            transaction.execute("INSERT INTO import_markers(name,source_digest,revision) SELECT ?1,?2,revision+1 FROM repository_state WHERE singleton=1", rusqlite::params![LEGACY_LOCAL_IMPORT_MARKER, source_digest])?;
            Ok(((), ChangeKind::LegacyLocalFilesImported, None, serde_json::json!({"count":imports.len(),"source_digest":source_digest})))
        })?;
        Ok(LegacyLocalImport {
            imported,
            revision: change.revision,
            already_imported: false,
        })
    }
}

pub fn local_track_from_path(path: &Path) -> Result<(TrackRecord, LocalAsset)> {
    ensure!(path.is_absolute(), "Local file path must be absolute");
    let metadata = match fs::metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_file(), "Local music path is not a file");
            Some(metadata)
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || exceeds_native_path_limit(&error) =>
        {
            None
        }
        Err(error) => return Err(error).context("Cannot inspect authorized local music file"),
    };
    let normalized = if metadata.is_some() {
        fs::canonicalize(path).context("Cannot normalize local music path")?
    } else {
        lexical_normalize(path)?
    };
    let path_text = normalized
        .to_str()
        .context("Local music path is not valid Unicode")?;
    let id = local_track_id(&normalized);
    let size_bytes = metadata.as_ref().map_or(0, |metadata| metadata.len());
    let mut asset = LocalAsset::new(id.clone(), normalized.clone(), size_bytes);
    asset.modified_ms = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    asset.available = metadata.is_some();
    let track = TrackRecord {
        id,
        lx_id: path_text.to_string(),
        title: normalized
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string(),
        artists: Vec::new(),
        album: String::new(),
        duration_ms: None,
        platform: PlatformTrackRef {
            source: "local".into(),
            song_id: serde_json::Value::String(path_text.to_string()),
            qualities: Vec::new(),
        },
        local_asset_id: Some(asset.id.clone()),
        metadata: serde_json::json!({"songId":path_text,"filePath":path_text,"albumName":""})
            .as_object()
            .expect("local metadata is an object")
            .clone(),
        wire: None,
    };
    Ok((track, asset))
}

// A stored path from another host can exceed this host's native path limit.
// Preserve its identity as unavailable without granting access to any file.
fn exceeds_native_path_limit(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ENAMETOOLONG)
    }
    #[cfg(windows)]
    {
        error.raw_os_error()
            == Some(windows_sys::Win32::Foundation::ERROR_FILENAME_EXCED_RANGE as i32)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                ensure!(normalized.pop(), "Local path escapes its root");
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    ensure!(
        normalized.is_absolute(),
        "Normalized local file path must be absolute"
    );
    Ok(normalized)
}

pub(super) fn validate_track(track: &TrackRecord) -> Result<()> {
    ensure!(
        !track.id.is_empty() && track.id.len() <= 1024,
        "Track identity is empty or too long"
    );
    Ok(())
}

pub(super) fn validate_asset(asset: &LocalAsset) -> Result<()> {
    ensure!(
        !asset.id.is_empty() && asset.id.len() <= 1024 && !asset.track_id.is_empty(),
        "Local asset identity is invalid"
    );
    ensure!(
        asset.path.is_absolute(),
        "Local asset path must be absolute"
    );
    Ok(())
}

pub(super) fn write_track(
    transaction: &Transaction<'_>,
    track: &TrackRecord,
    document: &str,
) -> Result<()> {
    transaction.execute("INSERT INTO tracks(id,document) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET document=excluded.document", rusqlite::params![track.id, document])?;
    Ok(())
}

pub(super) fn write_asset(
    transaction: &Transaction<'_>,
    asset: &LocalAsset,
    document: &str,
) -> Result<()> {
    let existing: Option<String> = transaction
        .query_row(
            "SELECT track_id FROM local_assets WHERE identity_key=?1",
            [&asset.id],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        existing.is_none_or(|existing| existing == asset.track_id),
        "Local asset identity belongs to another track"
    );
    transaction.execute("INSERT INTO local_assets(identity_key,track_id,document) VALUES(?1,?2,?3) ON CONFLICT(identity_key) DO UPDATE SET document=excluded.document", rusqlite::params![asset.id, asset.track_id, document])?;
    Ok(())
}
