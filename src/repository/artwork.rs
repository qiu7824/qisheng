//! Content-addressed picture bytes and independently bounded information rows.
use super::{Repository, schema};
use crate::{domain::artwork::*, library::artwork_validator::VerifiedArtwork};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

impl Repository {
    fn artwork_write<T>(&self, operation: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn put_artwork(&self, info: &ArtworkInfo, blob: Option<&VerifiedArtwork>) -> Result<()> {
        info.validate()?;
        let key = info.key.fingerprint()?;
        let document = serde_json::to_string(info)?;
        ensure!(
            document.len() <= MAX_ARTWORK_INFO_BYTES,
            "Artwork information exceeds 8 MiB"
        );
        if let Some(blob) = blob {
            ensure!(
                info.blob.as_ref() == Some(blob.metadata()),
                "Artwork bytes do not match their information"
            );
        }
        self.artwork_write(|tx| {
            if let Some(blob)=blob {
                let metadata=blob.metadata();let summary=serde_json::to_string(metadata)?;
                tx.execute("INSERT INTO artwork_blobs(sha256,byte_size,last_access_ms,document,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(sha256) DO UPDATE SET byte_size=excluded.byte_size,last_access_ms=excluded.last_access_ms,document=excluded.document,data=excluded.data",params![metadata.sha256,metadata.encoded_bytes as i64,info.resolved_at_ms as i64,summary,blob.encoded()])?;
            }
            let blob_sha=info.blob.as_ref().map(|blob|blob.sha256.as_str());
            if let Some(sha)=blob_sha {let stored:Option<String>=tx.query_row("SELECT document FROM artwork_blobs WHERE sha256=?1",[sha],|row|row.get(0)).optional()?;let stored=stored.context("Verified artwork bytes are missing")?;ensure!(stored.len()<=4096,"Artwork summary exceeds its limit");let stored:ArtworkBlobMeta=serde_json::from_str(&stored)?;ensure!(info.blob.as_ref()==Some(&stored),"Artwork information differs from its verified bytes");}
            tx.execute("INSERT INTO artwork_info(cache_key,original_track_id,original_record_sha256,resolved_at_ms,expire_at_ms,last_access_ms,byte_size,blob_sha256,document) VALUES(?1,?2,?3,?4,?5,?4,?6,?7,?8) ON CONFLICT(cache_key) DO UPDATE SET resolved_at_ms=excluded.resolved_at_ms,expire_at_ms=excluded.expire_at_ms,last_access_ms=excluded.last_access_ms,byte_size=excluded.byte_size,blob_sha256=excluded.blob_sha256,document=excluded.document",params![key,info.key.original_track_id,info.key.original_record_sha256,info.resolved_at_ms as i64,info.expire_at_ms as i64,document.len() as i64,blob_sha,document])?;
            enforce_info(tx,&key)?;enforce_blobs(tx,blob_sha)?;Ok(())
        })
    }
    pub fn artwork_info(&self, key: &ArtworkKey, now_ms: u64) -> Result<ArtworkLookup> {
        key.validate()?;
        ensure!(now_ms <= i64::MAX as u64, "Artwork clock exceeds its limit");
        let digest = key.fingerprint()?;
        self.artwork_write(|tx| {
            let row=tx.query_row("SELECT document,byte_size,resolved_at_ms,expire_at_ms,last_access_ms,blob_sha256 FROM artwork_info WHERE cache_key=?1",[&digest],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?,row.get::<_,Option<String>>(5)?))).optional()?;
            let Some((document,length,start,end,accessed,blob_sha))=row else{return Ok(ArtworkLookup::Miss)};
            ensure!(document.len()<=MAX_ARTWORK_INFO_BYTES&&length>0&&length as usize==document.len(),"Stored artwork information length is inconsistent");
            let info:ArtworkInfo=serde_json::from_str(&document)?;info.validate()?;
            ensure!(&info.key==key&&info.resolved_at_ms as i64==start&&info.expire_at_ms as i64==end&&accessed>=start&&info.blob.as_ref().map(|blob|blob.sha256.as_str())==blob_sha.as_deref(),"Stored artwork information identity is inconsistent");
            if now_ms<info.resolved_at_ms||now_ms<accessed as u64 {tx.execute("DELETE FROM artwork_info WHERE cache_key=?1",[&digest])?;return Ok(ArtworkLookup::ClockRollback);}
            if now_ms>=info.expire_at_ms {tx.execute("DELETE FROM artwork_info WHERE cache_key=?1",[&digest])?;return Ok(ArtworkLookup::Expired);}
            tx.execute("UPDATE artwork_info SET last_access_ms=?1 WHERE cache_key=?2",params![now_ms as i64,digest])?;Ok(ArtworkLookup::Hit(Box::new(info)))
        })
    }
    /// Called only by an artwork worker; callers must decode and verify the SHA.
    pub fn stored_artwork_blob(
        &self,
        sha256: &str,
        now_ms: u64,
    ) -> Result<Option<(ArtworkBlobMeta, Vec<u8>)>> {
        digest(sha256)?;
        ensure!(now_ms <= i64::MAX as u64, "Artwork clock exceeds its limit");
        self.artwork_write(|tx| {
            let row = tx
                .query_row(
                    "SELECT document,byte_size,length(data) FROM artwork_blobs WHERE sha256=?1",
                    [sha256],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((document, length, actual)) = row else {
                return Ok(None);
            };
            ensure!(
                length > 0
                    && length <= MAX_ARTWORK_BYTES as i64
                    && actual == length
                    && document.len() <= 4096,
                "Stored artwork byte length is inconsistent"
            );
            let metadata: ArtworkBlobMeta = serde_json::from_str(&document)?;
            metadata.validate()?;
            ensure!(
                metadata.sha256 == sha256 && metadata.encoded_bytes == length as u64,
                "Stored artwork digest is inconsistent"
            );
            let bytes: Vec<u8> = tx.query_row(
                "SELECT data FROM artwork_blobs WHERE sha256=?1",
                [sha256],
                |row| row.get(0),
            )?;
            ensure!(
                bytes.len() as i64 == length,
                "Stored artwork payload length changed"
            );
            tx.execute(
                "UPDATE artwork_blobs SET last_access_ms=?1 WHERE sha256=?2",
                params![now_ms as i64, sha256],
            )?;
            Ok(Some((metadata, bytes)))
        })
    }
    pub fn artwork_info_page(&self, offset: usize, limit: usize) -> Result<Vec<ArtworkInfo>> {
        ensure!(
            limit <= 100 && offset <= MAX_ARTWORK_INFO_ROWS,
            "Artwork information page exceeds its limit"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let mut statement=connection.prepare("SELECT document FROM artwork_info ORDER BY last_access_ms DESC,cache_key LIMIT ?1 OFFSET ?2")?;
        let rows = statement.query_map(params![limit as i64, offset as i64], |row| {
            row.get::<_, String>(0)
        })?;
        let mut bytes = 0;
        let mut result = Vec::new();
        for row in rows {
            let row = row?;
            bytes += row.len();
            ensure!(
                bytes <= 2 * 1024 * 1024,
                "Artwork information page exceeds 2 MiB"
            );
            let info: ArtworkInfo = serde_json::from_str(&row)?;
            info.validate()?;
            result.push(info);
        }
        Ok(result)
    }
    pub fn image_cache_stats(&self) -> Result<ImageCacheStats> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let (info_entries, info_bytes) = connection.query_row(
            "SELECT count(*),coalesce(sum(length(CAST(document AS BLOB))),0) FROM artwork_info",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let (blob_entries, blob_bytes) = connection.query_row(
            "SELECT count(*),coalesce(sum(length(data)),0) FROM artwork_blobs",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        ensure!(
            info_entries >= 0 && info_bytes >= 0 && blob_entries >= 0 && blob_bytes >= 0,
            "Artwork cache statistics are invalid"
        );
        Ok(ImageCacheStats {
            info_entries: info_entries as usize,
            info_bytes: info_bytes as u64,
            blob_entries: blob_entries as usize,
            blob_bytes: blob_bytes as u64,
            ..Default::default()
        })
    }
    pub fn clear_image_cache(&self) -> Result<ImageCacheStats> {
        self.artwork_write(|tx| {
            tx.execute("DELETE FROM artwork_info", [])?;
            tx.execute("DELETE FROM artwork_blobs", [])?;
            Ok(ImageCacheStats::default())
        })
    }
}
fn enforce_info(tx: &Transaction<'_>, protected: &str) -> Result<()> {
    loop {
        let (count, bytes) = tx.query_row(
            "SELECT count(*),coalesce(sum(byte_size),0) FROM artwork_info",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        if count <= MAX_ARTWORK_INFO_ROWS as i64 && bytes <= MAX_ARTWORK_INFO_BYTES as i64 {
            return Ok(());
        }
        let oldest:String=tx.query_row("SELECT cache_key FROM artwork_info WHERE cache_key<>?1 ORDER BY last_access_ms,cache_key LIMIT 1",[protected],|row|row.get(0)).context("Artwork information quota exhausted")?;
        tx.execute("DELETE FROM artwork_info WHERE cache_key=?1", [oldest])?;
    }
}
fn enforce_blobs(tx: &Transaction<'_>, protected: Option<&str>) -> Result<()> {
    loop {
        let (count, bytes) = tx.query_row(
            "SELECT count(*),coalesce(sum(length(data)),0) FROM artwork_blobs",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        if count <= MAX_ARTWORK_BLOB_ROWS as i64 && bytes <= MAX_ARTWORK_BLOB_BYTES as i64 {
            return Ok(());
        }
        let oldest:String=tx.query_row("SELECT sha256 FROM artwork_blobs WHERE sha256<>coalesce(?1,'') ORDER BY last_access_ms,sha256 LIMIT 1",[protected],|row|row.get(0)).context("Artwork byte quota exhausted")?;
        tx.execute("DELETE FROM artwork_info WHERE blob_sha256=?1 AND json_extract(document,'$.url') IS NULL",[&oldest])?;
        tx.execute("UPDATE artwork_info SET document=json_set(document,'$.blob',NULL),blob_sha256=NULL,byte_size=length(CAST(json_set(document,'$.blob',NULL) AS BLOB)) WHERE blob_sha256=?1",[&oldest])?;
        tx.execute("DELETE FROM artwork_blobs WHERE sha256=?1", [oldest])?;
    }
}
