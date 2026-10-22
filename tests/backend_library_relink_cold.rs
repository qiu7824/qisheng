#![cfg(windows)]

#[path = "support/library_watch_fixture.rs"]
mod fixture;

use fixture::{Running, binding, content_evidence, named, options, set_watch, settled, wave};
use lx_gpui_player::{
    backend::{BackendCommand, BackendQuery, BackendQueryResult},
    domain::{
        lists::{AddLocation, ListAction},
        music::{LxMusicDtoVersion, TrackRecord, normalize_local_path},
    },
};
use std::{fs, path::Path, sync::Mutex};

static CASES: Mutex<()> = Mutex::new(());
fn listed_record(running: &Running, track_id: &str) -> TrackRecord {
    match running
        .backend
        .query(BackendQuery::ListTrack {
            list_id: "default".into(),
            music_id: track_id.into(),
        })
        .unwrap()
    {
        BackendQueryResult::ListTrack(Some(track)) => *track,
        other => panic!("Cold fixture list identity was not retained: {other:?}"),
    }
}
fn retain_user_identity(
    running: &Running,
    asset_id: &str,
    track_id: &str,
    path: &Path,
    request: &str,
) -> TrackRecord {
    let mut track = TrackRecord::from_lx_dto(serde_json::json!({
        "id": format!("wire:{request}"), "name":"Cold namespace fixture", "singer":null,
        "source":"local", "interval":"00:06", "retainedTop":{"marker":17},
        "meta":{"filePath":path,"songId":path,"albumName":"Generated corpus","retainedMeta":true}
    }), LxMusicDtoVersion::Modern).unwrap();
    track.id = track_id.into();
    track.local_asset_id = Some(asset_id.into());
    running.execute(
        request,
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![track],
                location: AddLocation::Bottom,
            }),
        },
    );
    listed_record(running, track_id)
}
fn same_path(left: &Path, right: &Path) -> bool {
    normalize_local_path(left) == normalize_local_path(right)
}

#[test]
fn cold_old_path_reuse_before_the_second_batch_preserves_the_renamed_original_identity_and_hash() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let running = Running::new();
    let directory = running.folder("cold-late-original");
    let original = directory.join("000-reused.wav");
    let moved = directory.join("zzz-relocated-original.wav");
    wave(&original, 997.0, 6.0);
    let root = running.add_root("cold-late-root-add", directory.clone(), true);
    set_watch(
        &running,
        "cold-late-initial-baseline",
        &root,
        true,
        options(true),
    );
    let old = named(&running.backend, "000-reused.wav");
    let frozen = content_evidence(&running, &old.asset.id);
    let user = retain_user_identity(
        &running,
        &old.asset.id,
        &old.asset.track_id,
        &original,
        "cold-late-user-record",
    );
    set_watch(&running, "cold-late-watch-off", &root, false, options(true));
    // A data handle that denies writes would make this actual open fail.
    drop(
        fs::OpenOptions::new()
            .write(true)
            .open(&original)
            .expect("Cold fixture retained a live original data FH"),
    );
    for index in 0..201 {
        wave(
            &directory.join(format!("100-filler-{index:03}.wav")),
            440.0,
            0.04,
        );
    }
    fs::rename(&original, &moved).unwrap();
    wave(&original, 660.0, 6.0);
    let order = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let moved_position = order
        .iter()
        .position(|name| name == moved.file_name().unwrap())
        .unwrap();
    assert!(
        moved_position >= 200,
        "Actual NTFS discovery did not put the original beyond the first 200-file batch: {moved_position}"
    );
    set_watch(
        &running,
        "cold-late-full-baseline",
        &root,
        true,
        options(true),
    );
    settled(&running, &root, |rows| {
        rows.len() == 203
            && rows.iter().any(|row| {
                row.asset.id == old.asset.id
                    && same_path(&row.asset.path, &moved)
                    && row.asset.available
            })
    });
    let relocated = named(&running.backend, "zzz-relocated-original.wav");
    let replacement = named(&running.backend, "000-reused.wav");
    assert_eq!(relocated.asset.id, old.asset.id);
    assert_eq!(relocated.asset.track_id, old.asset.track_id);
    assert_ne!(replacement.asset.id, old.asset.id);
    assert_ne!(replacement.asset.track_id, old.asset.track_id);
    assert!(!replacement.explicit_file_grant);
    let committed = binding(&running.backend, &old.asset.id);
    assert_eq!(committed.file_identity, frozen.file_identity);
    assert_eq!(committed.content_sha256, frozen.content_sha256);
    assert_eq!(committed.binding_revision, frozen.binding_revision + 1);
    assert!(same_path(&committed.path, &moved));
    let projected = listed_record(&running, &old.asset.track_id);
    assert_eq!(projected.id, user.id);
    assert_eq!(projected.lx_id, user.lx_id);
    assert_eq!(projected.local_asset_id, user.local_asset_id);
    assert_eq!(
        projected.to_lx_dto(LxMusicDtoVersion::Modern)["retainedTop"],
        serde_json::json!({"marker":17})
    );
    running.play("cold-late-play-original", &old.asset.track_id, 997.0);
    fixture::stop(&running, "cold-late-stop-original");
    running.play(
        "cold-late-play-replacement",
        &replacement.asset.track_id,
        660.0,
    );
    fixture::stop(&running, "cold-late-stop-replacement");
}

#[test]
fn cold_atomic_same_path_replacement_without_a_relocated_original_updates_the_same_logical_slot() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let running = Running::new();
    let directory = running.folder("cold-atomic-replacement");
    let original = directory.join("slot.wav");
    let staged = running.directory.path().join("replacement-staging.wav");
    wave(&original, 997.0, 6.0);
    let root = running.add_root("cold-atomic-root-add", directory.clone(), true);
    set_watch(
        &running,
        "cold-atomic-initial-baseline",
        &root,
        true,
        options(true),
    );
    let old = named(&running.backend, "slot.wav");
    let frozen = content_evidence(&running, &old.asset.id);
    let user = retain_user_identity(
        &running,
        &old.asset.id,
        &old.asset.track_id,
        &original,
        "cold-atomic-user-record",
    );
    set_watch(
        &running,
        "cold-atomic-watch-off",
        &root,
        false,
        options(true),
    );
    drop(
        fs::OpenOptions::new()
            .write(true)
            .open(&original)
            .expect("Atomic fixture retained a live original data FH"),
    );
    // Allocate the replacement while the old object still exists so its actual
    // file identity cannot be a recycled identity of that deleted object.
    wave(&staged, 660.0, 6.0);
    fs::remove_file(&original).unwrap();
    fs::rename(&staged, &original).unwrap();
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    set_watch(
        &running,
        "cold-atomic-complete-baseline",
        &root,
        true,
        options(true),
    );
    settled(&running, &root, |rows| {
        rows.len() == 1 && rows[0].asset.available && rows[0].asset.id == old.asset.id
    });
    let current = named(&running.backend, "slot.wav");
    assert_eq!(current.asset.id, old.asset.id);
    assert_eq!(current.asset.track_id, old.asset.track_id);
    let committed = content_evidence(&running, &old.asset.id);
    assert_ne!(committed.file_identity, frozen.file_identity);
    assert_ne!(committed.content_sha256, frozen.content_sha256);
    assert!(committed.binding_revision > frozen.binding_revision);
    assert!(same_path(&committed.path, &frozen.path));
    let projected = listed_record(&running, &old.asset.track_id);
    assert_eq!(projected.id, user.id);
    assert_eq!(projected.lx_id, user.lx_id);
    assert_eq!(projected.local_asset_id, user.local_asset_id);
    assert!(!current.explicit_file_grant);
    running.play("cold-atomic-play-replacement", &old.asset.track_id, 660.0);
    fixture::stop(&running, "cold-atomic-stop-replacement");
}
