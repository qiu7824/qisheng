use lx_gpui_player::catalog::{Catalog, MAX_AUDIO_BYTES};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn original_wav(path: &Path, data_bytes: u32) {
    let sample_rate = 22_050_u32;
    let mut header = Vec::new();
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16_u32.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    header.extend_from_slice(&2_u16.to_le_bytes());
    header.extend_from_slice(&16_u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());
    let mut file = File::create(path).unwrap();
    file.write_all(&header).unwrap();
    // The zero-filled PCM is an original silent fixture, including large files.
    file.set_len(44 + data_bytes as u64).unwrap();
    file.sync_all().unwrap();
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .unwrap()
}

#[test]
fn preparation_is_invisible_and_duplicate_commit_preserves_revision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("original 音乐.wav");
    original_wav(&path, 22_050 * 2);
    let catalog = Catalog::new().unwrap();
    let prepared = catalog.prepare_local(&path).unwrap();
    assert_eq!(catalog.tracks().len(), 3);
    assert_eq!(catalog.revision(), 0);
    assert_eq!(prepared.path(), std::fs::canonicalize(&path).unwrap());
    let id = prepared.track().id.clone();
    drop(prepared);
    assert_eq!(catalog.tracks().len(), 3);
    assert_eq!(catalog.revision(), 0);

    let track = catalog.commit_local(catalog.prepare_local(&path).unwrap());
    assert_eq!(track.id, id);
    assert_eq!(catalog.tracks().len(), 4);
    assert_eq!(catalog.revision(), 1);
    let alias = directory.path().join(".").join("original 音乐.wav");
    assert_eq!(catalog.import_local(alias).unwrap().id, id);
    assert_eq!(catalog.tracks().len(), 4);
    assert_eq!(catalog.revision(), 1);

    original_wav(&path, 22_050 * 4);
    let updated = catalog.import_local(path).unwrap();
    assert_eq!(updated.id, id);
    assert_eq!(catalog.tracks().len(), 4);
    assert_eq!(catalog.revision(), 2);
    assert!((updated.duration_seconds - 2.0).abs() < 0.01);
}

#[test]
fn local_ids_are_stable_across_import_order_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.wav");
    let second = directory.path().join("second.wav");
    original_wav(&first, 44_100);
    original_wav(&second, 88_200);
    let original = Catalog::new().unwrap();
    let first_id = original.import_local(first.clone()).unwrap().id;
    let second_id = original.import_local(second.clone()).unwrap().id;
    assert_ne!(first_id, second_id);
    drop(original);
    let restored = Catalog::new().unwrap();
    assert_eq!(restored.import_local(second).unwrap().id, second_id);
    assert_eq!(restored.import_local(first).unwrap().id, first_id);
    assert_eq!(restored.tracks()[0].id, "tone-1");
    assert_eq!(restored.tracks()[1].id, "tone-2");
    assert_eq!(restored.tracks()[2].id, "tone-3");
}

#[test]
fn local_files_larger_than_playback_buffer_are_probed_and_served_by_range() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large-original.wav");
    let data_bytes = MAX_AUDIO_BYTES as u32 + 1024;
    original_wav(&path, data_bytes);
    let catalog = Catalog::new().unwrap();
    let track = catalog.import_local(path.clone()).unwrap();
    assert!(track.duration_seconds > 1000.0);
    let total = std::fs::metadata(&path).unwrap().len();
    let client = client();
    let head = client.head(&track.source_url).send().unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(
        head.headers()["content-length"].to_str().unwrap(),
        total.to_string()
    );
    assert_eq!(head.headers()["accept-ranges"], "bytes");
    assert!(head.bytes().unwrap().is_empty());

    let response = client
        .get(&track.source_url)
        .header("Range", "bytes=0-43")
        .send()
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(
        response.headers()["content-range"].to_str().unwrap(),
        format!("bytes 0-43/{total}")
    );
    let etag = response.headers()["etag"].to_str().unwrap().to_string();
    let bytes = response.bytes().unwrap();
    assert_eq!(bytes.len(), 44);
    assert_eq!(&bytes[..4], b"RIFF");

    let suffix = client
        .get(&track.source_url)
        .header("Range", "bytes=-16")
        .header("If-Range", &etag)
        .send()
        .unwrap();
    assert_eq!(suffix.status(), 206);
    assert_eq!(suffix.bytes().unwrap().as_ref(), [0_u8; 16]);
    let open = client
        .get(&track.source_url)
        .header("Range", format!("bytes={}-", total - 8))
        .send()
        .unwrap();
    assert_eq!(open.status(), 206);
    assert_eq!(open.bytes().unwrap().len(), 8);
    for invalid in [
        format!("bytes={total}-"),
        "bytes=2-1".into(),
        "bytes=0-1,4-5".into(),
        "bytes=-0".into(),
    ] {
        let response = client
            .get(&track.source_url)
            .header("Range", invalid)
            .send()
            .unwrap();
        assert_eq!(response.status(), 416);
        assert_eq!(
            response.headers()["content-range"].to_str().unwrap(),
            format!("bytes */{total}")
        );
        assert!(response.bytes().unwrap().is_empty());
    }
}

#[test]
fn committed_file_references_do_not_expose_unselected_paths_and_report_missing_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.wav");
    original_wav(&path, 44_100);
    let catalog = Catalog::new().unwrap();
    let prepared = catalog.prepare_local(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let track = catalog.commit_local(prepared);
    let client = client();
    let response = client.get(&track.source_url).send().unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.text().unwrap(), "Audio file unavailable");
    assert_eq!(catalog.tracks().len(), 4);
    assert_eq!(catalog.revision(), 1);
    assert_eq!(
        client
            .get(format!("{}/../private", catalog.base_url()))
            .send()
            .unwrap()
            .status(),
        404
    );
}
