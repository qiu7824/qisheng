use super::model::{invalid, quota};
use super::{AddLocation, ListData, MusicInfo, SyncMode, SyncResult, UserList};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct FirstSyncPlan<T> {
    pub data: T,
    pub update_local: bool,
    pub update_remote: bool,
}
pub fn first_sync_lists(
    local: &ListData,
    remote: &ListData,
    mode: SyncMode,
    location: AddLocation,
) -> SyncResult<FirstSyncPlan<ListData>> {
    local.validate()?;
    remote.validate()?;
    if local.is_empty() {
        return Ok(FirstSyncPlan {
            data: remote.clone(),
            update_local: !remote.is_empty(),
            update_remote: false,
        });
    }
    if remote.is_empty() {
        return Ok(FirstSyncPlan {
            data: local.clone(),
            update_local: false,
            update_remote: true,
        });
    }
    let (data, update_local, update_remote) = match mode {
        SyncMode::MergeLocalRemote => (merge_lists(local, remote, location), true, true),
        SyncMode::MergeRemoteLocal => (merge_lists(remote, local, location), true, true),
        SyncMode::OverwriteLocalRemote => (overwrite_lists(local, remote), true, true),
        SyncMode::OverwriteRemoteLocal => (overwrite_lists(remote, local), true, true),
        SyncMode::OverwriteLocalRemoteFull => (local.clone(), false, true),
        SyncMode::OverwriteRemoteLocalFull => (remote.clone(), true, false),
        SyncMode::Cancel => {
            return Err(super::SyncError::new(
                super::SyncErrorKind::Cancelled,
                "Sync cancelled",
            ));
        }
    };
    data.validate()?;
    Ok(FirstSyncPlan {
        data,
        update_local,
        update_remote,
    })
}
fn merge_songs(
    source: &[MusicInfo],
    target: &[MusicInfo],
    baseline: Option<&[MusicInfo]>,
    location: AddLocation,
) -> Vec<MusicInfo> {
    let source_ids: HashSet<_> = source.iter().map(MusicInfo::id).collect();
    let target_ids: HashSet<_> = target.iter().map(MusicInfo::id).collect();
    let removed: HashSet<_> = baseline
        .unwrap_or_default()
        .iter()
        .map(MusicInfo::id)
        .filter(|id| !source_ids.contains(id) || !target_ids.contains(id))
        .collect();
    let combined: Vec<_> = match location {
        AddLocation::Top => target.iter().chain(source).collect(),
        AddLocation::Bottom => source.iter().chain(target).collect(),
    };
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    match location {
        AddLocation::Top => {
            for item in combined.into_iter().rev() {
                if !removed.contains(item.id()) && seen.insert(item.id()) {
                    result.push(item.clone());
                }
            }
            result.reverse();
        }
        AddLocation::Bottom => {
            for item in combined {
                if !removed.contains(item.id()) && seen.insert(item.id()) {
                    result.push(item.clone());
                }
            }
        }
    }
    result
}
fn merge_lists(source: &ListData, target: &ListData, location: AddLocation) -> ListData {
    let mut users = source.user_list.clone();
    for (index, target) in target.user_list.iter().enumerate() {
        if let Some(position) = users.iter().position(|list| list.id == target.id) {
            users[position].list = merge_songs(&users[position].list, &target.list, None, location);
            let source_time = users[position].location_time();
            let target_time = target.location_time();
            if target_time >= source_time {
                continue;
            }
            let mut moved = users.remove(position);
            moved.location_update_time = Some(Value::from(target_time));
            users.insert(index.min(users.len()), moved);
        } else if target.location_time() != 0.0 {
            users.insert(index.min(users.len()), target.clone());
        } else {
            users.push(target.clone());
        }
    }
    ListData {
        default_list: merge_songs(&source.default_list, &target.default_list, None, location),
        love_list: merge_songs(&source.love_list, &target.love_list, None, location),
        user_list: users,
    }
}
fn overwrite_lists(source: &ListData, target: &ListData) -> ListData {
    let mut result = source.clone();
    let ids: HashSet<_> = source
        .user_list
        .iter()
        .map(|list| list.id.as_str())
        .collect();
    for (index, list) in target.user_list.iter().enumerate() {
        if ids.contains(list.id.as_str()) {
            continue;
        }
        if list.location_time() != 0.0 {
            result
                .user_list
                .insert(index.min(result.user_list.len()), list.clone());
        } else {
            result.user_list.push(list.clone());
        }
    }
    result
}
pub fn merge_lists_three_way(
    local: &ListData,
    remote: &ListData,
    baseline: &ListData,
    location: AddLocation,
) -> SyncResult<ListData> {
    local.validate()?;
    remote.validate()?;
    baseline.validate()?;
    let local_map: HashMap<_, _> = local
        .user_list
        .iter()
        .map(|list| (list.id.as_str(), list))
        .collect();
    let remote_map: HashMap<_, _> = remote
        .user_list
        .iter()
        .map(|list| (list.id.as_str(), list))
        .collect();
    let baseline_map: HashMap<_, _> = baseline
        .user_list
        .iter()
        .map(|list| (list.id.as_str(), list))
        .collect();
    let removed: HashSet<_> = baseline
        .user_list
        .iter()
        .map(|list| list.id.as_str())
        .filter(|id| !local_map.contains_key(id) || !remote_map.contains_key(id))
        .collect();
    let mut users = Vec::new();
    for local in &local.user_list {
        if removed.contains(local.id.as_str()) {
            continue;
        }
        let item = if let Some(remote) = remote_map.get(local.id.as_str()) {
            let base = baseline_map.get(local.id.as_str());
            let select = |base: Option<&Value>, local: Option<&Value>, remote: Option<&Value>| {
                if primitive_equal(base, local) {
                    remote.cloned()
                } else {
                    local.cloned()
                }
            };
            UserList {
                id: local.id.clone(),
                name: select(
                    Some(base.map(|base| &base.name).unwrap_or(&Value::Null)),
                    Some(&local.name),
                    Some(&remote.name),
                )
                .unwrap_or(Value::Null),
                source: select(
                    match base {
                        Some(base) => base.source.as_ref(),
                        None => Some(&Value::Null),
                    },
                    local.source.as_ref(),
                    remote.source.as_ref(),
                ),
                source_list_id: select(
                    match base {
                        Some(base) => base.source_list_id.as_ref(),
                        None => Some(&Value::Null),
                    },
                    local.source_list_id.as_ref(),
                    remote.source_list_id.as_ref(),
                ),
                location_update_time: local.location_update_time.clone(),
                list: merge_songs(
                    &local.list,
                    &remote.list,
                    base.map(|base| base.list.as_slice()),
                    location,
                ),
            }
        } else {
            local.clone()
        };
        users.push(item);
    }
    for (index, remote) in remote.user_list.iter().enumerate() {
        if removed.contains(remote.id.as_str()) {
            continue;
        }
        if let Some(local) = local_map.get(remote.id.as_str()) {
            if local.location_time() >= remote.location_time() {
                continue;
            }
            let position = users.iter().position(|list| list.id == remote.id).unwrap();
            let mut moved = users.remove(position);
            moved.location_update_time = Some(Value::from(local.location_time()));
            users.insert(index.min(users.len()), moved);
        } else if remote.location_time() != 0.0 {
            users.insert(index.min(users.len()), remote.clone());
        } else {
            users.push(remote.clone());
        }
    }
    let result = ListData {
        default_list: merge_songs(
            &local.default_list,
            &remote.default_list,
            Some(&baseline.default_list),
            location,
        ),
        love_list: merge_songs(
            &local.love_list,
            &remote.love_list,
            Some(&baseline.love_list),
            location,
        ),
        user_list: users,
    };
    result.validate()?;
    Ok(result)
}
fn primitive_equal(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None | Some(Value::Null), None | Some(Value::Null)) => true,
        (Some(Value::Number(left)), Some(Value::Number(right))) => left.as_f64() == right.as_f64(),
        (Some(Value::Number(number)), Some(Value::String(text)))
        | (Some(Value::String(text)), Some(Value::Number(number))) => number
            .as_f64()
            .is_some_and(|number| number == boa_engine::JsString::from(text.as_str()).to_number()),
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}
pub fn first_sync_dislike(
    local: &str,
    remote: &str,
    mode: SyncMode,
) -> SyncResult<FirstSyncPlan<String>> {
    validate_rules(local)?;
    validate_rules(remote)?;
    if matches!(
        mode,
        SyncMode::OverwriteLocalRemoteFull | SyncMode::OverwriteRemoteLocalFull
    ) {
        return Err(invalid("Dislike does not support full mode"));
    }
    if local.is_empty() {
        return Ok(FirstSyncPlan {
            data: remote.into(),
            update_local: !remote.is_empty(),
            update_remote: false,
        });
    }
    if remote.is_empty() {
        return Ok(FirstSyncPlan {
            data: local.into(),
            update_local: false,
            update_remote: true,
        });
    }
    let (data, update_local, update_remote) = match mode {
        SyncMode::MergeLocalRemote => (
            filter_rules(&format!("{local}\n{remote}")).join("\n"),
            true,
            true,
        ),
        SyncMode::MergeRemoteLocal => (
            filter_rules(&format!("{remote}\n{local}")).join("\n"),
            true,
            true,
        ),
        SyncMode::OverwriteLocalRemote => (local.into(), false, true),
        SyncMode::OverwriteRemoteLocal => (remote.into(), true, false),
        _ => {
            return Err(super::SyncError::new(
                super::SyncErrorKind::Cancelled,
                "Sync cancelled",
            ));
        }
    };
    validate_rules(&data)?;
    Ok(FirstSyncPlan {
        data,
        update_local,
        update_remote,
    })
}
pub fn merge_dislike_three_way(local: &str, remote: &str, baseline: &str) -> SyncResult<String> {
    validate_rules(local)?;
    validate_rules(remote)?;
    validate_rules(baseline)?;
    let source = filter_rules(local);
    let target = filter_rules(remote);
    let source_set: HashSet<_> = source.iter().map(String::as_str).collect();
    let target_set: HashSet<_> = target.iter().map(String::as_str).collect();
    let base = filter_rules(baseline);
    let removed: HashSet<_> = base
        .iter()
        .filter(|rule| !source_set.contains(rule.as_str()) || !target_set.contains(rule.as_str()))
        .map(String::as_str)
        .collect();
    let mut seen = HashSet::new();
    let data = source
        .iter()
        .chain(target.iter())
        .filter(|rule| !removed.contains(rule.as_str()) && seen.insert(rule.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    validate_rules(&data)?;
    Ok(data)
}
pub fn dislike_digest(rules: &str) -> SyncResult<String> {
    use md5::{Digest, Md5};
    validate_rules(rules)?;
    Ok(format!("{:x}", Md5::digest(trim_js(rules).as_bytes())))
}
pub(crate) fn validate_rules(rules: &str) -> SyncResult<()> {
    if rules.len() > 2 * 1024 * 1024 {
        return Err(quota("Dislike dataset exceeds byte budget"));
    }
    if rules.contains('\0') {
        return Err(invalid("Invalid dislike rules"));
    }
    Ok(())
}
fn filter_rules(rules: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for line in rules.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('@');
        let name = fields.next().unwrap_or_default();
        let singer = fields.next().unwrap_or_default();
        let has_name = !name.is_empty();
        let has_singer = !singer.is_empty();
        let name = trim_js(&name.replace('@', "#").to_lowercase()).to_owned();
        let singer = trim_js(&singer.replace('@', "#").to_lowercase()).to_owned();
        let rule = if has_name {
            if has_singer {
                format!("{name}@{singer}")
            } else {
                name
            }
        } else if has_singer {
            format!("@{singer}")
        } else {
            continue;
        };
        if seen.insert(rule.clone()) {
            result.push(rule);
        }
    }
    result
}
pub(crate) fn trim_js(value: &str) -> &str {
    value.trim_matches(|value|matches!(value,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
