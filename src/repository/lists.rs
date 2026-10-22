use super::{ChangeKind, Repository, RepositoryChange, decode_document, encode_document, schema};
use crate::domain::{lists::*, music::TrackRecord};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction, params};
use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ListTracksPage {
    pub items: Vec<TrackRecord>,
    pub total: u64,
    pub offset: usize,
    pub limit: usize,
    pub revision: u64,
}

impl Repository {
    pub fn list_track(&self, list_id: &str, id: &str) -> Result<Option<TrackRecord>> {
        use rusqlite::OptionalExtension;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        require_list(&connection, list_id)?;
        let document:Option<String>=connection.query_row("SELECT document FROM music_list_items WHERE list_id=?1 AND (track_id=?2 OR music_id=?2) ORDER BY CASE WHEN track_id=?2 THEN 0 ELSE 1 END,position,music_id LIMIT 1",params![list_id,id],|row|row.get(0)).optional()?;
        document.map(|doc| decode_document(&doc)).transpose()
    }
    pub fn list_tracks_page(
        &self,
        list_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<ListTracksPage> {
        ensure!(limit <= 1000, "List page exceeds 1000 tracks");
        let sql_offset =
            i64::try_from(offset).context("List page offset exceeds supported range")?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction()?;
        require_list(&tx, list_id)?;
        let revision: i64 = tx.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let total: i64 = tx.query_row(
            "SELECT count(*) FROM music_list_items WHERE list_id=?1",
            [list_id],
            |row| row.get(0),
        )?;
        let items = {
            let mut statement=tx.prepare("SELECT document FROM music_list_items WHERE list_id=?1 ORDER BY position,music_id LIMIT ?2 OFFSET ?3")?;
            statement
                .query_map(params![list_id, limit as i64, sql_offset], |row| {
                    row.get::<_, String>(0)
                })?
                .map(|row| decode_document(&row?))
                .collect::<Result<Vec<_>>>()?
        };
        tx.commit()?;
        Ok(ListTracksPage {
            items,
            total: total as u64,
            offset,
            limit,
            revision: revision as u64,
        })
    }
    pub fn list_infos(&self) -> Result<Vec<ListInfo>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        query_infos(&connection, false)
    }
    pub fn list_tracks(&self, id: &str) -> Result<Vec<TrackRecord>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        require_list(&connection, id)?;
        query_tracks(&connection, id)
    }
    pub fn list_data(&self) -> Result<ListData> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction()?;
        let user_list = query_infos(&transaction, true)?
            .into_iter()
            .map(|info| {
                Ok(UserList {
                    tracks: query_tracks(&transaction, &info.id)?,
                    info,
                })
            })
            .collect::<Result<_>>()?;
        let data = ListData {
            default_list: query_tracks(&transaction, DEFAULT_LIST)?,
            love_list: query_tracks(&transaction, LOVE_LIST)?,
            temp_list: Some(query_tracks(&transaction, TEMP_LIST)?),
            user_list,
        };
        transaction.commit()?;
        Ok(data)
    }
    pub fn list_contains(&self, list_id: &str, music_id: &str) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        require_list(&connection, list_id)?;
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM music_list_items WHERE list_id=?1 AND (music_id=?2 OR track_id=?2))",params![list_id,music_id],|row|row.get(0))?)
    }
    pub fn music_list_ids(&self, music_id: &str) -> Result<Vec<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let mut statement=connection.prepare("SELECT DISTINCT l.id FROM music_lists l JOIN music_list_items m ON m.list_id=l.id WHERE m.music_id=?1 OR m.track_id=?1 ORDER BY CASE l.kind WHEN 'builtin' THEN 0 ELSE 1 END,l.position,l.id")?;
        statement
            .query_map([music_id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }
    pub fn apply_list_action(
        &self,
        action: &ListAction,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        self.apply_list_action_with_origin(action, expected_revision, "local")
    }
    pub fn apply_list_action_with_origin(
        &self,
        action: &ListAction,
        expected_revision: Option<u64>,
        origin: &str,
    ) -> Result<RepositoryChange> {
        ensure!(
            !origin.is_empty() && origin.len() <= 256,
            "List origin is invalid"
        );
        let details = serde_json::json!({"action":action,"name":action.name(),"origin":origin});
        encode_document(&details)?;
        let (_, change) = self.write(expected_revision, |transaction| {
            apply(transaction, action)?;
            Ok(((), ChangeKind::ListsChanged, None, details))
        })?;
        Ok(change)
    }
    pub fn list_positions(&self) -> Result<ListPositionInfo> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let mut statement = connection.prepare("SELECT id,scroll_position FROM music_lists")?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }
    pub fn set_list_position(
        &self,
        id: &str,
        position: f64,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        ensure!(
            position.is_finite() && position >= 0.0,
            "List scroll position must be finite and non-negative"
        );
        let (_, change) = self.write(expected_revision, |tx| {
            require_list(tx, id)?;
            tx.execute(
                "UPDATE music_lists SET scroll_position=?2 WHERE id=?1",
                params![id, position],
            )?;
            Ok((
                (),
                ChangeKind::ListPositionChanged,
                None,
                serde_json::json!({"list_id":id,"position":position}),
            ))
        })?;
        Ok(change)
    }
    pub fn list_update_infos(&self) -> Result<ListUpdateInfoMap> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let mut statement = connection.prepare(
            "SELECT id,update_document FROM music_lists WHERE update_document IS NOT NULL",
        )?;
        statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .map(|row| {
                let (id, doc) = row?;
                Ok((id, decode_document(&doc)?))
            })
            .collect()
    }
    pub fn set_list_update_info(
        &self,
        id: &str,
        info: &ListUpdateInfo,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        let document = encode_document(info)?;
        let (_, change) = self.write(expected_revision, |tx| {
            require_list(tx, id)?;
            tx.execute(
                "UPDATE music_lists SET update_document=?2 WHERE id=?1",
                params![id, document],
            )?;
            Ok((
                (),
                ChangeKind::ListUpdateInfoChanged,
                None,
                serde_json::json!({"list_id":id,"info":info}),
            ))
        })?;
        Ok(change)
    }
}

pub(super) fn query_infos(connection: &Connection, user_only: bool) -> Result<Vec<ListInfo>> {
    let sql = if user_only {
        "SELECT document FROM music_lists WHERE kind='user' ORDER BY position,id"
    } else {
        "SELECT document FROM music_lists ORDER BY CASE kind WHEN 'builtin' THEN 0 ELSE 1 END,position,id"
    };
    let mut statement = connection.prepare(sql)?;
    statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| decode_document(&row?))
        .collect()
}
pub(super) fn query_tracks(connection: &Connection, id: &str) -> Result<Vec<TrackRecord>> {
    let mut statement = connection.prepare(
        "SELECT document FROM music_list_items WHERE list_id=?1 ORDER BY position,music_id",
    )?;
    statement
        .query_map([id], |row| row.get::<_, String>(0))?
        .map(|row| decode_document(&row?))
        .collect()
}
fn require_list(connection: &Connection, id: &str) -> Result<()> {
    ensure!(
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM music_lists WHERE id=?1)",
            [id],
            |row| row.get::<_, bool>(0)
        )?,
        "List does not exist: {id}"
    );
    Ok(())
}
fn validate_info(info: &ListInfo, user: bool) -> Result<()> {
    ensure!(
        !info.id.is_empty() && info.id.len() <= 1024 && !info.id.contains('\0'),
        "List identity is invalid"
    );
    ensure!(
        !user || !info.is_builtin(),
        "Built-in list identity is reserved"
    );
    ensure!(
        info.name.len() <= 4096 && !info.name.contains('\0'),
        "List name is invalid"
    );
    for field in [&info.source, &info.source_list_id].into_iter().flatten() {
        ensure!(
            field.len() <= 4096 && !field.contains('\0'),
            "List source metadata is invalid"
        );
    }
    encode_document(info)?;
    Ok(())
}
fn validate_tracks(tracks: &[TrackRecord]) -> Result<()> {
    ensure!(tracks.len() <= 100000, "List exceeds 100000 tracks");
    for track in tracks {
        ensure!(
            !track.id.is_empty()
                && track.id.len() <= 1024
                && !track.lx_id.is_empty()
                && track.lx_id.len() <= 1024,
            "List track identity is invalid"
        );
        encode_document(track)?;
    }
    Ok(())
}
fn write_info(tx: &Transaction<'_>, info: &ListInfo, position: usize, create: bool) -> Result<()> {
    validate_info(info, true)?;
    let document = encode_document(info)?;
    if create {
        tx.execute(
            "INSERT INTO music_lists(id,kind,position,document) VALUES(?1,'user',?2,?3)",
            params![info.id, position as i64, document],
        )?;
    } else {
        tx.execute(
            "UPDATE music_lists SET position=?2,document=?3 WHERE id=?1 AND kind='user'",
            params![info.id, position as i64, document],
        )?;
    }
    Ok(())
}
fn reorder_infos(tx: &Transaction<'_>, infos: &[ListInfo]) -> Result<()> {
    for (position, info) in infos.iter().enumerate() {
        write_info(tx, info, position, false)?;
    }
    Ok(())
}
fn replace_tracks(tx: &Transaction<'_>, id: &str, tracks: &[TrackRecord]) -> Result<()> {
    require_list(tx, id)?;
    validate_tracks(tracks)?;
    tx.execute("DELETE FROM music_list_items WHERE list_id=?1", [id])?;
    for (position, track) in tracks.iter().enumerate() {
        let document = encode_document(track)?;
        tx.execute(
            "INSERT INTO tracks(id,document) VALUES(?1,?2) ON CONFLICT(id) DO NOTHING",
            params![track.id, document],
        )?;
        tx.execute("INSERT INTO music_list_items(list_id,music_id,track_id,position,document) VALUES(?1,?2,?3,?4,?5)",params![id,track.lx_id,track.id,position as i64,document])?;
    }
    Ok(())
}
fn add_tracks(
    existing: Vec<TrackRecord>,
    incoming: &[TrackRecord],
    location: AddLocation,
) -> Vec<TrackRecord> {
    let mut seen: HashSet<_> = existing.iter().map(|t| t.lx_id.clone()).collect();
    let new: Vec<_> = incoming
        .iter()
        .filter(|t| seen.insert(t.lx_id.clone()))
        .cloned()
        .collect();
    match location {
        AddLocation::Top => new.into_iter().chain(existing).collect(),
        AddLocation::Bottom => existing.into_iter().chain(new).collect(),
    }
}
fn selected(track: &TrackRecord, ids: &HashSet<&str>) -> bool {
    ids.contains(track.lx_id.as_str()) || ids.contains(track.id.as_str())
}
fn splice_index(position: i64, length: usize) -> usize {
    if position < 0 {
        (length as i128 + position as i128).max(0) as usize
    } else {
        (position as u64).min(length as u64) as usize
    }
}
pub(super) fn apply(tx: &Transaction<'_>, action: &ListAction) -> Result<()> {
    let mut resolved = action.clone();
    resolve_local_action(tx, &mut resolved)?;
    apply_resolved(tx, &resolved)
}

fn resolve_local_action(tx: &Transaction<'_>, action: &mut ListAction) -> Result<()> {
    let resolve = |tracks: &mut Vec<TrackRecord>| -> Result<()> {
        for track in tracks {
            *track = super::library_binding::resolve_local_list_track(tx, track)?;
        }
        Ok(())
    };
    match action {
        ListAction::DataOverwrite { data } => {
            resolve(&mut data.default_list)?;
            resolve(&mut data.love_list)?;
            if let Some(tracks) = &mut data.temp_list {
                resolve(tracks)?;
            }
            for list in &mut data.user_list {
                resolve(&mut list.tracks)?;
            }
        }
        ListAction::MusicOverwrite { tracks, .. }
        | ListAction::MusicAdd { tracks, .. }
        | ListAction::MusicMove { tracks, .. } => resolve(tracks)?,
        ListAction::MusicUpdate { updates } => {
            for update in updates {
                update.track = super::library_binding::resolve_local_list_track(tx, &update.track)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn apply_resolved(tx: &Transaction<'_>, action: &ListAction) -> Result<()> {
    match action {
        ListAction::DataOverwrite { data } => {
            let mut incoming = HashSet::new();
            for user in &data.user_list {
                validate_info(&user.info, true)?;
                validate_tracks(&user.tracks)?;
                ensure!(
                    incoming.insert(user.info.id.clone()),
                    "Duplicate user list identity"
                );
            }
            let existing: HashSet<_> = query_infos(tx, true)?
                .into_iter()
                .map(|info| info.id)
                .collect();
            for id in &existing {
                if !incoming.contains(id) {
                    tx.execute("DELETE FROM music_lists WHERE id=?1", [id])?;
                }
            }
            for (index, user) in data.user_list.iter().enumerate() {
                write_info(tx, &user.info, index, !existing.contains(&user.info.id))?;
                replace_tracks(tx, &user.info.id, &user.tracks)?;
            }
            replace_tracks(tx, DEFAULT_LIST, &data.default_list)?;
            replace_tracks(tx, LOVE_LIST, &data.love_list)?;
            if let Some(tracks) = &data.temp_list {
                replace_tracks(tx, TEMP_LIST, tracks)?;
            }
        }
        ListAction::Create { position, lists } => {
            let mut all = query_infos(tx, true)?;
            let index = if *position < 0 || *position as u64 >= all.len() as u64 {
                all.len()
            } else {
                *position as usize
            };
            for info in lists {
                write_info(tx, info, 0, true)?;
            }
            all.splice(index..index, lists.clone());
            reorder_infos(tx, &all)?;
        }
        ListAction::Remove { ids } => {
            for id in ids {
                ensure!(!is_builtin(id), "Built-in lists cannot be removed");
                tx.execute("DELETE FROM music_lists WHERE id=?1 AND kind='user'", [id])?;
            }
            reorder_infos(tx, &query_infos(tx, true)?)?;
        }
        ListAction::Update { lists } => {
            for info in lists {
                validate_info(info, false)?;
                if info.id == TEMP_LIST {
                    let mut current = query_infos(tx, false)?
                        .into_iter()
                        .find(|i| i.id == TEMP_LIST)
                        .context("Temporary list is missing")?;
                    current.meta = info.meta.clone();
                    tx.execute(
                        "UPDATE music_lists SET document=?2 WHERE id=?1",
                        params![info.id, encode_document(&current)?],
                    )?;
                } else if !info.is_builtin() {
                    tx.execute(
                        "UPDATE music_lists SET document=?2 WHERE id=?1 AND kind='user'",
                        params![info.id, encode_document(info)?],
                    )?;
                }
            }
        }
        ListAction::UpdatePosition { ids, position } => {
            let mut all = query_infos(tx, true)?;
            let ids: HashSet<_> = ids.iter().map(String::as_str).collect();
            let mut moved = Vec::new();
            for index in (0..all.len()).rev() {
                if ids.contains(all[index].id.as_str()) {
                    let mut info = all.remove(index);
                    info.location_update_time = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    );
                    moved.push(info);
                }
            }
            let index = splice_index(*position, all.len());
            all.splice(index..index, moved);
            reorder_infos(tx, &all)?;
        }
        ListAction::MusicOverwrite { list_id, tracks } => replace_tracks(tx, list_id, tracks)?,
        ListAction::MusicAdd {
            list_id,
            tracks,
            location,
        } => {
            require_list(tx, list_id)?;
            validate_tracks(tracks)?;
            replace_tracks(
                tx,
                list_id,
                &add_tracks(query_tracks(tx, list_id)?, tracks, *location),
            )?;
        }
        ListAction::MusicMove {
            from_id,
            to_id,
            tracks,
            location,
        } => {
            require_list(tx, from_id)?;
            require_list(tx, to_id)?;
            validate_tracks(tracks)?;
            ensure!(from_id != to_id, "MusicMove requires two different lists");
            let ids: HashSet<_> = tracks
                .iter()
                .flat_map(|t| [t.id.as_str(), t.lx_id.as_str()])
                .collect();
            let source = query_tracks(tx, from_id)?
                .into_iter()
                .filter(|t| !selected(t, &ids))
                .collect::<Vec<_>>();
            let target = add_tracks(query_tracks(tx, to_id)?, tracks, *location);
            replace_tracks(tx, from_id, &source)?;
            replace_tracks(tx, to_id, &target)?;
        }
        ListAction::MusicRemove { list_id, ids } => {
            require_list(tx, list_id)?;
            let ids: HashSet<_> = ids.iter().map(String::as_str).collect();
            let tracks = query_tracks(tx, list_id)?
                .into_iter()
                .filter(|t| !selected(t, &ids))
                .collect::<Vec<_>>();
            replace_tracks(tx, list_id, &tracks)?;
        }
        ListAction::MusicUpdate { updates } => {
            for update in updates {
                require_list(tx, &update.list_id)?;
                validate_tracks(std::slice::from_ref(&update.track))?;
                let mut tracks = query_tracks(tx, &update.list_id)?;
                if let Some(existing) = tracks.iter_mut().find(|t| t.lx_id == update.track.lx_id) {
                    *existing = update.track.clone();
                    replace_tracks(tx, &update.list_id, &tracks)?;
                }
            }
        }
        ListAction::MusicClear { list_ids } => {
            for id in list_ids {
                require_list(tx, id)?;
                tx.execute("DELETE FROM music_list_items WHERE list_id=?1", [id])?;
            }
        }
        ListAction::MusicUpdatePosition {
            list_id,
            ids,
            position,
        } => {
            require_list(tx, list_id)?;
            let mut tracks = query_tracks(tx, list_id)?;
            let mut moved = Vec::new();
            let mut seen = HashSet::new();
            for id in ids {
                if let Some(index) = tracks.iter().position(|t| t.lx_id == *id || t.id == *id) {
                    let track = tracks.remove(index);
                    let fresh = seen.insert(track.lx_id.clone());
                    seen.insert(track.id.clone());
                    if fresh {
                        moved.push(track);
                    }
                } else if !seen.contains(id) {
                    anyhow::bail!("Music position references a missing track: {id}");
                }
            }
            let index = splice_index(*position, tracks.len());
            tracks.splice(index..index, moved);
            replace_tracks(tx, list_id, &tracks)?;
        }
    }
    Ok(())
}
