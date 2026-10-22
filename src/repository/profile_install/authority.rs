//! Current-profile authority overlay; incoming image declarations are never grants.
use super::*;
use crate::domain::music::TrackRecord;
use rusqlite::{
    Connection, params,
    types::{ToSqlOutput, ValueRef},
};
use std::sync::atomic::{AtomicUsize, Ordering};

// Input accounting is independent of the owned VFS's actual image/sidecar
// limits. These bounds do not reserve or authorize another database image.
const MAX_AUTHORITY_BYTES: usize = 1024 * 1024 * 1024;
const MAX_AUTHORITY_ROWS: usize = 1_000_000;
const MAX_AUTHORITY_ROW_BYTES: usize = 8 * 1024 * 1024 + 64 * 1024;
const MAX_AUTHORITY_COLUMNS: usize = 15;
const MAX_AUTHORITY_CAPTURES: usize = 4;
static AUTHORITY_CAPTURES: AtomicUsize = AtomicUsize::new(0);
struct AuthorityReservation;
impl AuthorityReservation {
    fn acquire() -> Result<Self> {
        AUTHORITY_CAPTURES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_AUTHORITY_CAPTURES).then_some(count + 1)
            })
            .map_err(|_| anyhow::anyhow!("Current authority capture quota exceeded"))?;
        Ok(Self)
    }
}
impl Drop for AuthorityReservation {
    fn drop(&mut self) {
        AUTHORITY_CAPTURES.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(crate) struct CurrentAuthority {
    digest: String,
    _reservation: AuthorityReservation,
}
/// A coherent pre-close SQL cut; only the actual successful connection close
/// can acknowledge it in the continuously held profile guard.
pub(crate) struct CurrentAuthorityCloseCut {
    pub revision: u64,
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalRestoreAuthorityReport {
    pub source_sha256: String,
    pub matched_assets: usize,
    pub preserved_roots: usize,
    pub preserved_rows: usize,
    pub sync_profile_epoch: u64,
    pub sync_sequence: u64,
}

fn retained_work(connection: &Connection) -> Result<()> {
    for (sql, message) in [
        (
            "SELECT EXISTS(SELECT 1 FROM tag_edit_operations WHERE coalesce(json_extract(document,'$.finished'),0)=0)",
            "Physical installation cannot discard retained local-tag work",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM tag_edit_artifacts WHERE reserved_bytes>0 OR actual_bytes>0 OR phase!='retired')",
            "Physical installation cannot discard retained local-tag artifacts",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM download_owned_artifacts WHERE reserved_bytes>0 OR phase IN ('allocated','verified'))",
            "Physical installation cannot discard retained download work",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM download_allocation_intents)",
            "Physical installation cannot discard pending native download allocations",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM download_tasks WHERE state IN ('queued','resolving','downloading') OR json_extract(document,'$.commit_journal') IS NOT NULL)",
            "Physical installation cannot discard active download work",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM physical_snapshot_operations WHERE state!='finished')",
            "Physical installation cannot discard retained snapshot work",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM library_scan_runs WHERE state='running')",
            "Physical installation cannot discard active library scans",
        ),
    ] {
        let pending: bool = connection.query_row(sql, [], |row| row.get(0))?;
        ensure!(!pending, "{message}");
    }
    Ok(())
}
fn hash_value(hash: &mut Sha256, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => hash.update([0]),
        ValueRef::Integer(value) => {
            hash.update([1]);
            hash.update(value.to_le_bytes());
        }
        ValueRef::Real(value) => {
            hash.update([2]);
            hash.update(value.to_bits().to_le_bytes());
        }
        ValueRef::Text(value) => {
            hash.update([3]);
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
        ValueRef::Blob(value) => {
            hash.update([4]);
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
    }
}
const TABLES: &[(&str, &str)] = &[
    ("tracks", "id,document"),
    ("local_assets", "identity_key,track_id,document"),
    (
        "library_roots",
        "root_id,path_key,active,grant_epoch,generation,document",
    ),
    ("library_root_locations", "root_id,location_revision"),
    (
        "library_dirty_state",
        "root_id,watch_generation,received_sequence,covered_sequence,full_rescan_required",
    ),
    (
        "library_watch_preferences",
        "root_id,watch_generation,enabled,document",
    ),
    (
        "local_file_grants",
        "asset_id,epoch,grant_path_key,document",
    ),
    (
        "local_asset_locations",
        "asset_id,path_key,document,binding_revision,physical_identity_key",
    ),
    (
        "library_membership",
        "root_id,asset_id,last_seen_generation",
    ),
    (
        "library_scan_runs",
        "root_id,generation,operation_id,grant_epoch,state,document,scope_document",
    ),
    (
        "library_scan_seen",
        "root_id,generation,path_key,path,asset_id",
    ),
    (
        "library_identity_reservations",
        "path_key,asset_id,track_id,lx_id,physical_identity_key,expires_at_ms,document",
    ),
    (
        "library_relink_candidates",
        "candidate_id,asset_id,root_id,expires_at_ms,document",
    ),
    (
        "library_relink_results",
        "operation_id,request_digest,asset_id,document",
    ),
    (
        "tag_write_approvals",
        "approval_id,asset_id,approval_epoch,revoked,document,byte_size",
    ),
    (
        "tag_edit_operations",
        "operation_id,approval_id,asset_id,operation_epoch,ledger_revision,phase,document,byte_size",
    ),
    (
        "tag_edit_artifacts",
        "operation_id,role,phase,actual_bytes,reserved_bytes,document,byte_size",
    ),
    (
        "download_destination_grants",
        "id,profile_id,epoch,revision,revoked,document",
    ),
    ("download_settings", "singleton,document"),
    (
        "download_tasks",
        "id,profile_id,grant_id,position,task_revision,operation_epoch,state,document,byte_size",
    ),
    (
        "download_owned_artifacts",
        "nonce,task_id,operation_epoch,phase,actual_bytes,reserved_bytes,sha256,document,byte_size",
    ),
    (
        "download_allocation_intents",
        "nonce,task_id,operation_epoch,reserved_bytes,document,byte_size",
    ),
    (
        "physical_snapshot_state",
        "singleton,profile_id,profile_epoch,operation_epoch,selection_epoch",
    ),
    (
        "physical_snapshot_operations",
        "operation_id,profile_id,profile_epoch,operation_epoch,selection_epoch,expected_snapshot_revision,maximum_image_bytes,request_digest,ledger_revision,state,document,byte_size",
    ),
    (
        "sync_peers",
        "peer_id,client_id,auth_epoch,revoked,private_key,document,byte_size",
    ),
    (
        "sync_mode_preferences",
        "peer_id,dataset,auth_epoch,document",
    ),
    ("compat_settings", "singleton,revision,document,byte_size"),
];
fn table_query(name: &str, columns: &str) -> String {
    let filter = if name == "tracks" {
        " WHERE id IN (SELECT track_id FROM local_assets)"
    } else {
        ""
    };
    let order = match name {
        "library_membership"
        | "library_scan_runs"
        | "tag_edit_artifacts"
        | "sync_mode_preferences" => "1,2",
        "library_scan_seen" => "1,2,3",
        _ => "1",
    };
    format!("SELECT {columns} FROM {name}{filter} ORDER BY {order}")
}
fn text(value: ValueRef<'_>) -> Result<&str> {
    match value {
        ValueRef::Text(value) => Ok(std::str::from_utf8(value)?),
        _ => anyhow::bail!("Current authority text cell is invalid"),
    }
}
#[derive(Default)]
struct InputBudget {
    bytes: usize,
    rows: usize,
}
impl InputBudget {
    fn row(&mut self, values: &[ValueRef<'_>]) -> Result<()> {
        let mut row_bytes = 0usize;
        for value in values {
            let payload = match value {
                ValueRef::Text(value) | ValueRef::Blob(value) => value.len(),
                _ => 8,
            };
            row_bytes = row_bytes
                .checked_add(payload)
                .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ValueRef<'_>>()))
                .context("Current authority row size overflow")?;
        }
        ensure!(
            row_bytes <= MAX_AUTHORITY_ROW_BYTES,
            "Current authority row exceeds its streaming bound"
        );
        self.bytes = self
            .bytes
            .checked_add(row_bytes)
            .context("Current authority input size overflow")?;
        self.rows = self
            .rows
            .checked_add(1)
            .context("Current authority row count overflow")?;
        ensure!(
            self.bytes <= MAX_AUTHORITY_BYTES && self.rows <= MAX_AUTHORITY_ROWS,
            "Current authority exceeds its bounded streaming input"
        );
        Ok(())
    }
}
fn insert_sql(name: &str, columns: &str) -> String {
    let marks = (1..=columns.split(',').count())
        .map(|number| format!("?{number}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "INSERT INTO {name} ({columns}) VALUES ({marks}){}",
        match name {
            "tracks" => " ON CONFLICT(id) DO UPDATE SET document=excluded.document",
            "sync_peers" =>
                " ON CONFLICT(peer_id) DO UPDATE SET client_id=excluded.client_id,auth_epoch=excluded.auth_epoch,revoked=excluded.revoked,private_key=excluded.private_key,document=excluded.document,byte_size=excluded.byte_size",
            _ => "",
        }
    )
}
fn refresh_list_track(
    items: &mut rusqlite::Statement<'_>,
    update: &mut rusqlite::Statement<'_>,
    document: &str,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let mut track: TrackRecord = serde_json::from_str(document)?;
    // The cursor borrows one playlist row at a time. Updated columns do not
    // change this index's keys or order, so duplicates need no retained Vec.
    let mut cursor = items.query([&track.id])?;
    while let Some(row) = cursor.next()? {
        check()?;
        let list_id = row.get_ref(0)?;
        let music_id = row.get_ref(1)?;
        track.lx_id = text(music_id)?.to_owned();
        let document = serde_json::to_string(&track)?;
        ensure!(
            document.len() <= MAX_AUTHORITY_ROW_BYTES,
            "Current playlist track exceeds its streaming bound"
        );
        update.execute(params![
            ToSqlOutput::Borrowed(list_id),
            ToSqlOutput::Borrowed(music_id),
            document
        ])?;
    }
    Ok(())
}
fn pairing_at(connection: &Connection) -> Result<(i64, i64, bool, i64, i64)> {
    let pairing = connection.query_row(
        "SELECT profile_epoch,pairing_epoch,pairing_enabled,sequence,journal_floor FROM sync_profile_state WHERE singleton=1",
        [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, bool>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?)),
    )?;
    ensure!(
        pairing.0 > 0 && pairing.1 > 0 && pairing.3 >= 0 && (0..=pairing.3).contains(&pairing.4),
        "Current sync authority clocks are invalid"
    );
    Ok(pairing)
}
impl CurrentAuthority {
    #[cfg(test)]
    pub(crate) fn digest_current(
        current: &Connection,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<String> {
        Ok(Self::capture_close_cut(current, check)?.digest)
    }
    pub(crate) fn capture_close_cut(
        current: &Connection,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<CurrentAuthorityCloseCut> {
        ensure!(
            current.is_autocommit(),
            "Current authority digest requires an idle connection"
        );
        let tx = current.unchecked_transaction()?;
        let revision: i64 = tx.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let digest = Self::digest_snapshot(&tx, check)?;
        tx.commit()?;
        Ok(CurrentAuthorityCloseCut {
            revision: u64::try_from(revision).context("Current repository revision is negative")?,
            digest,
        })
    }
    fn digest_snapshot(current: &Connection, check: &dyn Fn() -> Result<()>) -> Result<String> {
        let pairing = pairing_at(current)?;
        let mut hash = Sha256::new();
        hash.update(b"lx-current-authority-v2\0");
        hash.update(serde_json::to_vec(&pairing)?);
        for &(name, columns) in TABLES {
            hash.update(name.as_bytes());
            hash.update(columns.as_bytes());
            let mut statement = current.prepare(&table_query(name, columns))?;
            let width = statement.column_count();
            let mut cursor = statement.query([])?;
            while let Some(row) = cursor.next()? {
                check()?;
                hash.update([0xfa]);
                for column in 0..width {
                    hash_value(&mut hash, row.get_ref(column)?);
                }
                hash.update([0xfb]);
            }
            hash.update([0xfc]);
        }
        check()?;
        Ok(format!("{:x}", hash.finalize()))
    }

    pub(super) fn digest(&self) -> &str {
        &self.digest
    }
    pub(super) fn copy_into(
        current: &Connection,
        candidate: &mut Connection,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<(Self, PhysicalRestoreAuthorityReport)> {
        ensure!(
            current.is_autocommit() && candidate.is_autocommit(),
            "Current authority capture requires an idle connection"
        );
        let reservation = AuthorityReservation::acquire()?;
        let trace = std::env::var_os("LX_DEBUG").is_some();
        let started = std::time::Instant::now();
        // OFF cannot undo pages spilled by a large overlay. This journal is an
        // owned VFS file charged to the existing operation's actual byte quota.
        candidate.pragma_update(None, "journal_mode", "DELETE")?;
        let journal_mode: String =
            candidate.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        ensure!(
            journal_mode.eq_ignore_ascii_case("delete"),
            "Current authority overlay requires an owned rollback journal"
        );
        let current_tx = current.unchecked_transaction()?;
        retained_work(&current_tx)?;
        let pairing = pairing_at(&current_tx)?;
        ensure!(
            pairing.0 < i64::MAX && pairing.3 < i64::MAX,
            "Current sync epoch or sequence is exhausted"
        );
        let tx = candidate.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for name in [
            "tag_edit_artifacts",
            "tag_edit_operations",
            "tag_write_approvals",
            "download_allocation_intents",
            "download_owned_artifacts",
            "download_tasks",
            "download_destination_grants",
            "local_file_grants",
            "library_membership",
            "library_scan_seen",
            "library_scan_runs",
            "library_relink_candidates",
            "library_relink_results",
            "library_identity_reservations",
            "library_dirty_state",
            "library_watch_preferences",
            "library_root_locations",
            "local_asset_locations",
            "library_roots",
            "sync_mode_preferences",
            "sync_baselines",
            "sync_current",
            "sync_journal",
            "sync_capture_override",
            "sync_snapshots",
            "download_settings",
            "physical_snapshot_operations",
            "physical_snapshot_state",
            "compat_settings",
        ] {
            check()?;
            tx.execute(&format!("DELETE FROM {name}"), [])?;
        }
        let trusted_peers: i64 =
            current_tx.query_row("SELECT count(*) FROM sync_peers", [], |row| row.get(0))?;
        ensure!(
            (0..=8).contains(&trusted_peers),
            "Current approved peer quota exceeded"
        );
        let unpurged: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_peers WHERE revoked!=1 OR private_key IS NOT NULL)",
            [],
            |row| row.get(0),
        )?;
        ensure!(!unpurged, "Incoming peers still carry execution authority");
        // Historical incoming peer names stay revoked and keyless. Current
        // private credentials alone may replace a colliding peer identity.
        tx.execute("DELETE FROM sync_peers WHERE peer_id IN (SELECT peer_id FROM sync_peers ORDER BY peer_id LIMIT -1 OFFSET ?1)", [8-trusted_peers])?;
        let mut budget = InputBudget::default();
        let mut assets = 0usize;
        let mut roots = 0usize;
        let mut hash = Sha256::new();
        hash.update(b"lx-current-authority-v2\0");
        hash.update(serde_json::to_vec(&pairing)?);
        // A single current read transaction spans admission, clocks, all rows,
        // and the complete candidate write. Only this opaque digest is retained.
        let mut list_items = tx.prepare(
            "SELECT list_id,music_id FROM music_list_items WHERE track_id=?1 ORDER BY list_id,music_id",
        )?;
        let mut list_update =
            tx.prepare("UPDATE music_list_items SET document=?3 WHERE list_id=?1 AND music_id=?2")?;
        for &(name, columns) in TABLES {
            if trace {
                super::trace_sqlite_memory(&tx, name);
                eprintln!(
                    "Authority overlay {name}: start {:?}, {} rows",
                    started.elapsed(),
                    budget.rows
                );
            }
            hash.update(name.as_bytes());
            hash.update(columns.as_bytes());
            let mut source = current_tx.prepare(&table_query(name, columns))?;
            let width = source.column_count();
            ensure!(
                width <= MAX_AUTHORITY_COLUMNS,
                "Current authority row is too wide"
            );
            let mut cursor = source.query([])?;
            let mut insert = tx.prepare(&insert_sql(name, columns))?;
            let mut remove_collision =
                tx.prepare("DELETE FROM local_assets WHERE identity_key=?1 OR track_id=?2")?;
            while let Some(row) = cursor.next()? {
                check()?;
                let mut cells = [ValueRef::Null; MAX_AUTHORITY_COLUMNS];
                for (column, cell) in cells[..width].iter_mut().enumerate() {
                    *cell = row.get_ref(column)?;
                }
                let values = &cells[..width];
                budget.row(values)?;
                hash.update([0xfa]);
                for &value in values {
                    hash_value(&mut hash, value);
                }
                hash.update([0xfb]);
                if name == "local_assets" {
                    // Incoming local rows remain inert metadata. Identity and
                    // track collisions are removed before the trusted row.
                    text(values[0])?;
                    remove_collision.execute(rusqlite::params_from_iter(
                        values[..2].iter().copied().map(ToSqlOutput::Borrowed),
                    ))?;
                    assets += 1;
                } else if name == "library_roots" {
                    text(values[0])?;
                    roots += 1;
                }
                insert.execute(rusqlite::params_from_iter(
                    values.iter().copied().map(ToSqlOutput::Borrowed),
                ))?;
                if name == "tracks" {
                    refresh_list_track(&mut list_items, &mut list_update, text(values[1])?, check)?;
                }
            }
            hash.update([0xfc]);
        }
        drop(list_items);
        drop(list_update);
        let sync_epoch = pairing
            .0
            .checked_add(1)
            .context("Current sync profile epoch exhausted")?;
        let sequence = pairing
            .3
            .checked_add(1)
            .context("Current sync sequence exhausted")?;
        tx.execute("UPDATE sync_profile_state SET profile_epoch=?1,pairing_epoch=?2,pairing_enabled=?3,sequence=?4,journal_floor=?4 WHERE singleton=1",
            params![sync_epoch,pairing.1,pairing.2,sequence])?;
        // Flush dirty overlay pages through the owned rollback journal before
        // the full tree walk. This keeps clean pages recyclable while retaining
        // the transaction and its rollback protection until validation succeeds.
        super::trace_sqlite_memory(&tx, "before overlay flush");
        tx.cache_flush()
            .context("Flush current authority overlay")?;
        tx.execute_batch("PRAGMA shrink_memory")?;
        super::trace_sqlite_memory(&tx, "before overlay integrity");
        schema::integrity_check(&tx)?;
        super::trace_sqlite_memory(&tx, "after overlay integrity");
        if trace {
            eprintln!(
                "Authority overlay integrity: {:?}, {} rows",
                started.elapsed(),
                budget.rows
            );
        }
        check()?;
        let digest = format!("{:x}", hash.finalize());
        // Commit the read transaction first: any failed source completion still
        // drops the candidate transaction and rolls its entire overlay back.
        current_tx.commit()?;
        tx.commit()?;
        super::trace_sqlite_memory(candidate, "after overlay commit");
        let report = PhysicalRestoreAuthorityReport {
            source_sha256: digest.clone(),
            matched_assets: assets,
            preserved_roots: roots,
            preserved_rows: budget.rows,
            sync_profile_epoch: sync_epoch as u64,
            sync_sequence: sequence as u64,
        };
        Ok((
            Self {
                digest,
                _reservation: reservation,
            },
            report,
        ))
    }
    pub(super) fn verify_current(
        &self,
        current: &Connection,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            current.is_autocommit(),
            "Current authority verification requires an idle connection"
        );
        let tx = current.unchecked_transaction()?;
        retained_work(&tx)?;
        ensure!(
            Self::digest_snapshot(&tx, check)? == self.digest,
            "Current execution authority changed during physical preparation"
        );
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "authority_tests.rs"]
mod tests;
