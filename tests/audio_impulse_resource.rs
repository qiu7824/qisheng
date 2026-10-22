use lx_gpui_player::{
    audio::{impulse_decode::ImpulseDecodeContext, impulse_resource::prepare_selected_impulse},
    library::authority::AuthorityRegistry,
};
use sha2::{Digest, Sha256};
use std::{fs, time::Duration};
#[path = "support/encoded_audio_fixture.rs"]
#[allow(dead_code)]
mod encoded;
#[test]
fn actual_selected_flac_yields_verified_pcm_and_whole_file_identity() {
    let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = folder.path().join("selected-ir.flac");
    let bytes = encoded::mono_verbatim_flac();
    fs::write(&path, &bytes).unwrap();
    let registry = AuthorityRegistry::default();
    let selected = registry.open_selection(&path).unwrap();
    let expected = selected.identity().clone();
    let result = prepare_selected_impulse(
        &selected,
        "flac",
        &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
    )
    .unwrap();
    assert_eq!(result.selected_identity, expected);
    assert_eq!(result.encoded_bytes, bytes.len() as u64);
    assert_eq!(
        result.encoded_sha256,
        format!("{:x}", Sha256::digest(&bytes))
    );
    assert_eq!(result.decoded.decoded_frames, 512);
    assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
}
#[test]
fn cancellation_and_missing_selection_never_create_an_impulse_file_or_grant() {
    let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let registry = AuthorityRegistry::default();
    assert!(
        registry
            .open_selection(&folder.path().join("absent.flac"))
            .is_err()
    );
    let path = folder.path().join("existing.flac");
    fs::write(&path, encoded::mono_verbatim_flac()).unwrap();
    let selected = registry.open_selection(&path).unwrap();
    let context = ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap();
    context.cancel();
    assert!(prepare_selected_impulse(&selected, "flac", &context).is_err());
    assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
}
