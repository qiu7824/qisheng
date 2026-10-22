//! Information caches use independent transactions and never publish library revisions.
use super::{Repository, decode_document, encode_document, schema};
use crate::domain::media::{
    MATCHING_ALGORITHM_VERSION, MediaCacheLookup, MediaFailureKind, MediaInvalidationReason,
    ResolvedMedia, ResolvedMediaKey, SourceMatchKey, SourceMatches,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

pub const MAX_MEDIA_CACHE_ROWS: usize = 4096;
pub const MAX_MEDIA_CACHE_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaCacheStats {
    pub resolved_entries: usize,
    pub match_entries: usize,
    pub resolved_bytes: u64,
    pub match_bytes: u64,
}

struct StoredRow {
    document: String,
    start: i64,
    end: i64,
    accessed: i64,
    bytes: i64,
}
impl StoredRow {
    fn validate(&self, start: u64, end: u64) -> Result<()> {
        ensure!(
            self.start >= 0
                && self.end > self.start
                && self.accessed >= 0
                && self.start as u64 == start
                && self.end as u64 == end
                && self.bytes > 0
                && self.bytes as usize == self.document.len(),
            "Stored media cache index is inconsistent"
        );
        Ok(())
    }
    fn stale(&self, now: u64) -> Option<MediaInvalidationReason> {
        if now < self.start as u64 || now < self.accessed as u64 {
            Some(MediaInvalidationReason::ClockRollback)
        } else if now >= self.end as u64 {
            Some(MediaInvalidationReason::Expired)
        } else {
            None
        }
    }
}

impl Repository {
    fn cache_write<T>(&self, operation: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&transaction)?;
        transaction
            .commit()
            .context("Cannot commit media cache transaction")?;
        Ok(result)
    }

    pub fn put_resolved_media(&self, value: &ResolvedMedia) -> Result<()> {
        value.validate()?;
        let cache_key = value.key.fingerprint()?;
        let document = encode_document(value)?;
        reserve_invalidation_bytes(&document)?;
        self.cache_write(|tx| {
            tx.execute("INSERT INTO resolved_media(cache_key,original_track_id,original_platform,actual_quality,source_sha256,source_policy_version,music_info_sha256,resolved_at_ms,expire_at_ms,last_access_ms,byte_size,document) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?8,?10,?11) ON CONFLICT(cache_key) DO UPDATE SET resolved_at_ms=excluded.resolved_at_ms,expire_at_ms=excluded.expire_at_ms,last_access_ms=excluded.last_access_ms,byte_size=excluded.byte_size,document=excluded.document",
                params![cache_key, value.key.original_track_id, value.key.original_platform, value.key.actual_quality,
                    value.key.source_sha256, value.key.source_policy_version as i64, value.key.music_info_sha256,
                    value.resolved_at_ms as i64, value.expire_at_ms as i64, document.len() as i64, document])?;
            enforce_quota(tx, "resolved_media", &cache_key, value.resolved_at_ms)?;
            Ok(())
        })
    }

    /// A hit is resolution information only; this method performs no HTTP or consent operation.
    pub fn resolved_media(
        &self,
        key: &ResolvedMediaKey,
        now_ms: u64,
    ) -> Result<MediaCacheLookup<ResolvedMedia>> {
        key.validate()?;
        checked_time(now_ms)?;
        let cache_key = key.fingerprint()?;
        self.cache_write(|tx| {
            let row = tx.query_row("SELECT document,resolved_at_ms,expire_at_ms,last_access_ms,byte_size FROM resolved_media WHERE cache_key=?1", [&cache_key], stored_row).optional()?;
            let Some(row) = row else { return Ok(MediaCacheLookup::Miss); };
            let mut value: ResolvedMedia = decode_document(&row.document)?;
            value.validate()?;
            ensure!(value.key == *key, "Stored resolved media identity does not match its key");
            row.validate(value.resolved_at_ms, value.expire_at_ms)?;
            if let Some(reason) = value.invalidation { return Ok(MediaCacheLookup::Invalidated(reason)); }
            if let Some(reason) = row.stale(now_ms) {
                value.invalidation = Some(reason);
                update_document(tx, "resolved_media", &cache_key, &encode_document(&value)?)?;
                return Ok(MediaCacheLookup::Invalidated(reason));
            }
            tx.execute("UPDATE resolved_media SET last_access_ms=?1 WHERE cache_key=?2", params![now_ms as i64,cache_key])?;
            Ok(MediaCacheLookup::Hit(value))
        })
    }

    pub fn invalidate_resolved_media(
        &self,
        key: &ResolvedMediaKey,
        reason: MediaInvalidationReason,
    ) -> Result<bool> {
        key.validate()?;
        let cache_key = key.fingerprint()?;
        self.cache_write(|tx| invalidate_resolved(tx, &cache_key, reason))
    }

    pub fn record_resolved_media_failure(
        &self,
        key: &ResolvedMediaKey,
        failure: MediaFailureKind,
    ) -> Result<bool> {
        key.validate()?;
        let cache_key = key.fingerprint()?;
        self.cache_write(|tx| {
            let document: Option<String> = tx
                .query_row(
                    "SELECT document FROM resolved_media WHERE cache_key=?1",
                    [&cache_key],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(document) = document else {
                return Ok(false);
            };
            let mut value: ResolvedMedia = decode_document(&document)?;
            value.validate()?;
            ensure!(
                value.key == *key,
                "Stored resolved media identity does not match its key"
            );
            value.failure = Some(failure);
            value.invalidation = Some(if failure == MediaFailureKind::HttpRejected {
                MediaInvalidationReason::HttpRejected
            } else {
                MediaInvalidationReason::ResolutionFailed
            });
            update_document(tx, "resolved_media", &cache_key, &encode_document(&value)?)?;
            Ok(true)
        })
    }

    pub fn put_source_matches(&self, value: &SourceMatches) -> Result<()> {
        value.validate()?;
        let cache_key = value.key.fingerprint()?;
        let document = encode_document(value)?;
        reserve_invalidation_bytes(&document)?;
        self.cache_write(|tx| {
            tx.execute("DELETE FROM source_matches WHERE algorithm_version<>?1", [MATCHING_ALGORITHM_VERSION])?;
            tx.execute("INSERT INTO source_matches(cache_key,original_track_id,original_platform,original_record_sha256,algorithm_version,matched_at_ms,expire_at_ms,last_access_ms,byte_size,document) VALUES(?1,?2,?3,?4,?5,?6,?7,?6,?8,?9) ON CONFLICT(cache_key) DO UPDATE SET matched_at_ms=excluded.matched_at_ms,expire_at_ms=excluded.expire_at_ms,last_access_ms=excluded.last_access_ms,byte_size=excluded.byte_size,document=excluded.document",
                params![cache_key,value.key.original_track_id,value.key.original_platform,value.key.original_record_sha256,
                    value.key.algorithm_version,value.matched_at_ms as i64,value.expire_at_ms as i64,document.len() as i64,document])?;
            enforce_quota(tx, "source_matches", &cache_key, value.matched_at_ms)?;
            Ok(())
        })
    }

    pub fn source_matches(
        &self,
        key: &SourceMatchKey,
        now_ms: u64,
    ) -> Result<MediaCacheLookup<SourceMatches>> {
        key.validate()?;
        checked_time(now_ms)?;
        let cache_key = key.fingerprint()?;
        self.cache_write(|tx| {
            let row = tx.query_row("SELECT document,matched_at_ms,expire_at_ms,last_access_ms,byte_size,algorithm_version FROM source_matches WHERE cache_key=?1", [&cache_key], |row| Ok((stored_row(row)?,row.get::<_,u32>(5)?))).optional()?;
            let Some((row, algorithm_version)) = row else { return Ok(MediaCacheLookup::Miss); };
            let mut value: SourceMatches = decode_document(&row.document)?;
            if algorithm_version != MATCHING_ALGORITHM_VERSION || value.key.algorithm_version != MATCHING_ALGORITHM_VERSION {
                tx.execute("DELETE FROM source_matches WHERE cache_key=?1", [&cache_key])?;
                return Ok(MediaCacheLookup::Invalidated(MediaInvalidationReason::UnsupportedVersion));
            }
            value.validate()?;
            ensure!(value.key == *key, "Stored source matches identity does not match its key");
            row.validate(value.matched_at_ms, value.expire_at_ms)?;
            if let Some(reason) = value.invalidation { return Ok(MediaCacheLookup::Invalidated(reason)); }
            if let Some(reason) = row.stale(now_ms) {
                value.invalidation = Some(reason);
                update_document(tx, "source_matches", &cache_key, &encode_document(&value)?)?;
                return Ok(MediaCacheLookup::Invalidated(reason));
            }
            tx.execute("UPDATE source_matches SET last_access_ms=?1 WHERE cache_key=?2", params![now_ms as i64,cache_key])?;
            Ok(MediaCacheLookup::Hit(value))
        })
    }

    pub fn invalidate_source_matches(
        &self,
        key: &SourceMatchKey,
        reason: MediaInvalidationReason,
    ) -> Result<bool> {
        key.validate()?;
        let cache_key = key.fingerprint()?;
        self.cache_write(|tx| invalidate_matches(tx, &cache_key, reason))
    }

    pub fn invalidate_media_for_track(
        &self,
        track_id: &str,
        reason: MediaInvalidationReason,
    ) -> Result<usize> {
        ensure!(
            !track_id.is_empty() && track_id.len() <= 1024,
            "Media cache track identity is invalid"
        );
        self.cache_write(|tx| {
            let mut count = 0;
            for key in keys(tx, "resolved_media", "original_track_id", track_id)? {
                count += usize::from(invalidate_resolved(tx, &key, reason)?);
            }
            for key in keys(tx, "source_matches", "original_track_id", track_id)? {
                count += usize::from(invalidate_matches(tx, &key, reason)?);
            }
            Ok(count)
        })
    }

    pub fn invalidate_media_for_source(
        &self,
        source_sha256: &str,
        reason: MediaInvalidationReason,
    ) -> Result<usize> {
        ensure!(
            source_sha256.len() == 64
                && source_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "Media cache source identity is invalid"
        );
        self.cache_write(|tx| {
            let mut count = 0;
            for key in keys(tx, "resolved_media", "source_sha256", source_sha256)? {
                count += usize::from(invalidate_resolved(tx, &key, reason)?);
            }
            Ok(count)
        })
    }

    pub fn invalidate_media_for_source_policy(
        &self,
        source_sha256: &str,
        source_policy_version: u64,
        reason: MediaInvalidationReason,
    ) -> Result<usize> {
        ensure!(
            source_sha256.len() == 64
                && source_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "Media cache source identity is invalid"
        );
        ensure!(
            source_policy_version > 0 && source_policy_version <= i64::MAX as u64,
            "Media cache source policy is invalid"
        );
        self.cache_write(|tx| {
            let matched = {
                let mut statement = tx.prepare("SELECT cache_key FROM resolved_media WHERE source_sha256=?1 AND source_policy_version=?2 ORDER BY cache_key")?;
                let rows = statement.query_map(params![source_sha256,source_policy_version as i64], |row| row.get::<_,String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut count = 0;
            for key in matched { count += usize::from(invalidate_resolved(tx, &key, reason)?); }
            Ok(count)
        })
    }

    pub fn clear_media_cache(&self) -> Result<MediaCacheStats> {
        self.cache_write(|tx| {
            let before = statistics(tx)?;
            tx.execute("DELETE FROM resolved_media", [])?;
            tx.execute("DELETE FROM source_matches", [])?;
            Ok(before)
        })
    }
    pub fn media_cache_stats(&self) -> Result<MediaCacheStats> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        statistics(&connection)
    }
}

fn stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok(StoredRow {
        document: row.get(0)?,
        start: row.get(1)?,
        end: row.get(2)?,
        accessed: row.get(3)?,
        bytes: row.get(4)?,
    })
}
fn checked_time(now_ms: u64) -> Result<()> {
    ensure!(
        now_ms <= i64::MAX as u64,
        "Media cache time exceeds its supported range"
    );
    Ok(())
}
fn reserve_invalidation_bytes(document: &str) -> Result<()> {
    ensure!(
        document.len() <= MAX_MEDIA_CACHE_BYTES - 256,
        "Media cache entry exceeds its byte limit including invalidation state"
    );
    Ok(())
}
fn update_document(tx: &Transaction<'_>, table: &str, key: &str, document: &str) -> Result<()> {
    tx.execute(
        &format!("UPDATE {table} SET document=?1,byte_size=?2 WHERE cache_key=?3"),
        params![document, document.len() as i64, key],
    )?;
    // Invalidation adds a few bytes; keep that growth under the same total quota.
    enforce_quota(tx, table, key, 0)
}
fn invalidate_resolved(
    tx: &Transaction<'_>,
    key: &str,
    reason: MediaInvalidationReason,
) -> Result<bool> {
    let document: Option<String> = tx
        .query_row(
            "SELECT document FROM resolved_media WHERE cache_key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(document) = document else {
        return Ok(false);
    };
    let mut value: ResolvedMedia = decode_document(&document)?;
    value.validate()?;
    ensure!(
        value.key.fingerprint()? == key,
        "Stored resolved media identity does not match its key"
    );
    value.invalidation = Some(reason);
    if reason == MediaInvalidationReason::HttpRejected {
        value.failure = Some(MediaFailureKind::HttpRejected);
    }
    if reason == MediaInvalidationReason::ResolutionFailed {
        value.failure = Some(MediaFailureKind::SourceFailed);
    }
    update_document(tx, "resolved_media", key, &encode_document(&value)?)?;
    Ok(true)
}
fn invalidate_matches(
    tx: &Transaction<'_>,
    key: &str,
    reason: MediaInvalidationReason,
) -> Result<bool> {
    let document: Option<String> = tx
        .query_row(
            "SELECT document FROM source_matches WHERE cache_key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(document) = document else {
        return Ok(false);
    };
    let mut value: SourceMatches = decode_document(&document)?;
    value.validate()?;
    ensure!(
        value.key.fingerprint()? == key,
        "Stored source matches identity does not match its key"
    );
    value.invalidation = Some(reason);
    update_document(tx, "source_matches", key, &encode_document(&value)?)?;
    Ok(true)
}
fn keys(tx: &Transaction<'_>, table: &str, column: &str, identity: &str) -> Result<Vec<String>> {
    let mut statement = tx.prepare(&format!(
        "SELECT cache_key FROM {table} WHERE {column}=?1 ORDER BY cache_key"
    ))?;
    let rows = statement.query_map([identity], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(row?)).collect()
}
fn enforce_quota(tx: &Transaction<'_>, table: &str, keep: &str, now_ms: u64) -> Result<()> {
    loop {
        let (count, bytes): (i64, i64) = tx.query_row(
            &format!("SELECT count(*),COALESCE(sum(byte_size),0) FROM {table}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count <= MAX_MEDIA_CACHE_ROWS as i64 && bytes <= MAX_MEDIA_CACHE_BYTES as i64 {
            return Ok(());
        }
        let victim: Option<String> = tx.query_row(&format!("SELECT cache_key FROM {table} WHERE cache_key<>?1 ORDER BY (expire_at_ms<=?2) DESC,last_access_ms,cache_key LIMIT 1"), params![keep,now_ms as i64], |row| row.get(0)).optional()?;
        let victim = victim.context("Media cache entry cannot fit within its 8 MiB quota")?;
        tx.execute(&format!("DELETE FROM {table} WHERE cache_key=?1"), [victim])?;
    }
}
fn statistics(connection: &rusqlite::Connection) -> Result<MediaCacheStats> {
    let (resolved_entries, resolved_bytes) = connection.query_row(
        "SELECT count(*),COALESCE(sum(byte_size),0) FROM resolved_media",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let (match_entries, match_bytes) = connection.query_row(
        "SELECT count(*),COALESCE(sum(byte_size),0) FROM source_matches",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    Ok(MediaCacheStats {
        resolved_entries: resolved_entries as usize,
        match_entries: match_entries as usize,
        resolved_bytes: resolved_bytes as u64,
        match_bytes: match_bytes as u64,
    })
}
