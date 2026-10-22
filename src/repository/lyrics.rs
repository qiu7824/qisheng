use super::{ChangeKind, Repository, RepositoryChange};
use crate::domain::lyric::{LyricBundle, LyricField, LyricKind, LyricOrigin};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction};

const KINDS: [LyricKind; 4] = [
    LyricKind::Lyric,
    LyricKind::Tlyric,
    LyricKind::Rlyric,
    LyricKind::Lxlyric,
];
pub const MAX_LYRIC_BUNDLE_BYTES: usize = 10 * 1024 * 1024;

impl Repository {
    pub fn replace_lyrics(
        &self,
        track_id: &str,
        origin: LyricOrigin,
        bundle: &LyricBundle,
    ) -> Result<RepositoryChange> {
        self.replace_lyrics_at_revision(track_id, origin, bundle, None)
    }

    pub fn replace_lyrics_at_revision(
        &self,
        track_id: &str,
        origin: LyricOrigin,
        bundle: &LyricBundle,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        bundle.validate()?;
        let bytes = KINDS
            .iter()
            .map(|kind| match bundle.field(*kind) {
                LyricField::Text(text) => text.len(),
                _ => 0,
            })
            .sum::<usize>();
        ensure!(
            bytes <= MAX_LYRIC_BUNDLE_BYTES,
            "Lyric bundle exceeds 10 MiB"
        );
        let origin_name = origin_name(origin);
        let (_, change) = self.write(expected_revision, |transaction| {
            replace_bundle(transaction, track_id, origin_name, bundle)?;
            Ok((
                (),
                ChangeKind::LyricsReplaced,
                Some(track_id.into()),
                serde_json::json!({"origin":origin_name}),
            ))
        })?;
        Ok(change)
    }

    /// An absent bundle differs from a fetched bundle containing four missing fields.
    pub fn lyrics(&self, track_id: &str, origin: LyricOrigin) -> Result<Option<LyricBundle>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let origin = origin_name(origin);
        let exists: Option<i32> = connection
            .query_row(
                "SELECT 1 FROM lyric_bundles WHERE track_id=?1 AND origin=?2",
                rusqlite::params![track_id, origin],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(None);
        }
        let mut bundle = LyricBundle::default();
        for kind in KINDS {
            let row: Option<(String, Option<String>)> = connection.query_row("SELECT state,text FROM lyric_fields WHERE track_id=?1 AND origin=?2 AND kind=?3", rusqlite::params![track_id, origin, kind_name(kind)], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
            let (state, text) = row.context("Stored lyric bundle has a missing channel record")?;
            let field = match state.as_str() {
                "missing" if text.is_none() => LyricField::Missing,
                "null" if text.is_none() => LyricField::Null,
                "text" => LyricField::Text(text.context("Stored lyric text is null")?),
                _ => anyhow::bail!("Stored lyric channel state is invalid"),
            };
            bundle.set_field(kind, field);
        }
        Ok(Some(bundle))
    }

    /// Deletes edited channels together; raw channels remain untouched.
    pub fn restore_raw_lyrics(&self, track_id: &str) -> Result<RepositoryChange> {
        self.remove_lyrics(
            track_id,
            LyricOrigin::Edited,
            ChangeKind::EditedLyricsRemoved,
        )
    }

    /// Clears fetched channels without deleting user edits.
    pub fn clear_raw_lyrics(&self, track_id: &str) -> Result<RepositoryChange> {
        self.remove_lyrics(track_id, LyricOrigin::Raw, ChangeKind::RawLyricsCleared)
    }

    fn remove_lyrics(
        &self,
        track_id: &str,
        origin: LyricOrigin,
        kind: ChangeKind,
    ) -> Result<RepositoryChange> {
        let (_, change) = self.write(None, |transaction| {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
                [track_id],
                |row| row.get(0),
            )?;
            ensure!(exists, "Track does not exist");
            transaction.execute(
                "DELETE FROM lyric_bundles WHERE track_id=?1 AND origin=?2",
                rusqlite::params![track_id, origin_name(origin)],
            )?;
            Ok((
                (),
                kind,
                Some(track_id.into()),
                serde_json::json!({"origin":origin_name(origin)}),
            ))
        })?;
        Ok(change)
    }
}

fn replace_bundle(
    transaction: &Transaction<'_>,
    track_id: &str,
    origin: &str,
    bundle: &LyricBundle,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO lyric_bundles(track_id,origin) VALUES(?1,?2) ON CONFLICT DO NOTHING",
        rusqlite::params![track_id, origin],
    )?;
    transaction.execute(
        "DELETE FROM lyric_fields WHERE track_id=?1 AND origin=?2",
        rusqlite::params![track_id, origin],
    )?;
    for kind in KINDS {
        let (state, text): (&str, Option<&str>) = match bundle.field(kind) {
            LyricField::Missing => ("missing", None),
            LyricField::Null => ("null", None),
            LyricField::Text(text) => ("text", Some(text)),
        };
        transaction.execute(
            "INSERT INTO lyric_fields(track_id,origin,kind,state,text) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![track_id, origin, kind_name(kind), state, text],
        )?;
    }
    Ok(())
}

fn origin_name(origin: LyricOrigin) -> &'static str {
    match origin {
        LyricOrigin::Raw => "raw",
        LyricOrigin::Edited => "edited",
    }
}

fn kind_name(kind: LyricKind) -> &'static str {
    match kind {
        LyricKind::Lyric => "lyric",
        LyricKind::Tlyric => "tlyric",
        LyricKind::Rlyric => "rlyric",
        LyricKind::Lxlyric => "lxlyric",
    }
}
