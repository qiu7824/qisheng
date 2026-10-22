use super::*;
use crate::audio::processing::{DSP_QUOTA_BYTES, DspReservation};
use std::time::Duration;

fn context() -> ImpulseDecodeContext {
    ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap()
}

#[test]
fn original_thirteen_wavs_match_exact_manifest_hashes_and_actual_decode_geometry() {
    let expected = [
        (
            "telephone",
            "filter-telephone.wav",
            0.0,
            3.0,
            "4dc96614900e1b3fa85611de2ddfd39f2e26530b3b8336239dc1badbfd89f7e9",
            293344,
            44100,
            2,
            72312,
        ),
        (
            "s2_r4_bd",
            "s2_r4_bd.wav",
            1.8,
            0.9,
            "400e242cb263d1ce83bed6b7fc2e75f498e0695735974f943d0269befcb31b2f",
            772096,
            48000,
            2,
            192000,
        ),
        (
            "bright_hall",
            "bright-hall.wav",
            0.8,
            2.4,
            "4f0d8c51a4a5f7a5061c4037136a314a673abb502a86fab869a3b068a4754f83",
            528384,
            44100,
            4,
            65536,
        ),
        (
            "cinema_diningroom",
            "cinema-diningroom.wav",
            0.6,
            2.3,
            "c515a820912cd2fe54f687f6cee36bb2c0b947a6bc23ad057327a8ae9abe44b0",
            264272,
            44100,
            2,
            65044,
        ),
        (
            "dining_living_true_stereo",
            "dining-living-true-stereo.wav",
            0.6,
            1.8,
            "b0287ec2305edc75d6c484c6a9e55339162acdc2edf7c18b53dfb835563b454f",
            549952,
            44100,
            4,
            68232,
        ),
        (
            "living_bedroom_leveled",
            "living-bedroom-leveled.wav",
            0.6,
            2.1,
            "d0ec29a1ac756ee6fcd20e1d05018792e645eb606cdc8727a38497f99de34592",
            220476,
            44100,
            2,
            54095,
        ),
        (
            "spreader50_65ms",
            "spreader50-65ms.wav",
            1.0,
            2.5,
            "40fffaf053416be74002d7636c7b469eb17c8762a0d096491c986d00f9a36e64",
            356896,
            44100,
            4,
            44100,
        ),
        (
            "s3_r1_bd",
            "s3_r1_bd.wav",
            1.8,
            0.8,
            "3ae4c9ade438e69c23e2e26ecc9bc579bee345be506a6ac30349c34c66177be0",
            772096,
            48000,
            2,
            192000,
        ),
        (
            "matrix_1",
            "matrix-reverb1.wav",
            1.5,
            0.9,
            "333d05904c01c774b28a05a0fae147f8475f6e4f7daa92c3e16d879867f97d0c",
            592576,
            44100,
            2,
            147120,
        ),
        (
            "matrix_2",
            "matrix-reverb2.wav",
            1.3,
            1.0,
            "888b47ee698110b8e0de89aa8f03edfcd76f19485b2c08675d5ff1d72c6c1343",
            503808,
            44100,
            2,
            124928,
        ),
        (
            "cardiod_35_10_spread",
            "cardiod-35-10-spread.wav",
            1.8,
            0.6,
            "cf3ab1550ea86065baa9f63a00c6265de7360e167a0bc6a1d0ca54b3f97050ad",
            637440,
            44100,
            2,
            158336,
        ),
        (
            "tim_omni_35_10_magnetic",
            "tim-omni-35-10-magnetic.wav",
            1.0,
            0.2,
            "d73086428420282fe2452f71e289d8521ac9b118947637e9ef97bc832de710c4",
            677044,
            44100,
            2,
            168237,
        ),
        (
            "feedback_spring",
            "feedback-spring.wav",
            1.8,
            0.8,
            "41d038558169517e38995325de9309c28ebf7c01abda579ec58670043f1961ba",
            709696,
            44100,
            2,
            176400,
        ),
    ];
    let baseline = DspReservation::used_bytes();
    let catalog = presets();
    assert_eq!(catalog.len(), 13);
    assert_eq!(DspReservation::used_bytes(), baseline);
    for (metadata, expected) in catalog.iter().zip(expected) {
        let (id, file, main, send, sha, bytes, rate, channels, frames) = expected;
        assert_eq!(metadata.id, id);
        assert_eq!(metadata.resource_id, ImpulseResourceId::Bundled(id.into()));
        assert_eq!(metadata.file_name, file);
        assert_eq!((metadata.main_gain, metadata.send_gain), (main, send));
        assert_eq!(metadata.sha256, sha);
        assert_eq!(metadata.encoded_bytes, bytes);
        assert_eq!(
            (metadata.sample_rate, metadata.channels, metadata.frames),
            (rate, channels, frames)
        );
        let source = embedded(id).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(source.bytes)), sha);
        let actual = prepare(id, &context()).unwrap();
        assert_eq!(actual.source_id(), &metadata.resource_id);
        assert_eq!(actual.representation_sha256(), sha);
        assert_eq!(actual.validated().sample_rate(), rate);
        assert_eq!(actual.validated().channels(), channels);
        assert_eq!(actual.validated().frames() as u64, frames);
        assert!(DspReservation::used_bytes() > baseline);
        drop(actual);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
}

#[test]
fn unknown_cancelled_expired_and_tampered_inputs_release_all_owned_decode_memory() {
    let baseline = DspReservation::used_bytes();
    assert!(preset("medium_room1").is_err());
    assert!(prepare("../filter-telephone.wav", &context()).is_err());
    let cancelled = context();
    cancelled.cancel();
    assert!(
        prepare("telephone", &cancelled)
            .err()
            .unwrap()
            .to_string()
            .contains("cancelled")
    );
    let expired = ImpulseDecodeContext::new(Duration::from_nanos(1)).unwrap();
    std::thread::sleep(Duration::from_millis(1));
    assert!(
        prepare("telephone", &expired)
            .err()
            .unwrap()
            .to_string()
            .contains("deadline")
    );
    let source = embedded("telephone").unwrap();
    let mut changed = source.bytes.to_vec();
    let final_index = changed.len() - 1;
    changed[final_index] ^= 1;
    assert!(
        decode_asset(source, &changed, &context())
            .err()
            .unwrap()
            .to_string()
            .contains("digest")
    );
    assert!(decode_asset(source, &changed[..final_index], &context()).is_err());
    assert_eq!(DspReservation::used_bytes(), baseline);
}

#[test]
fn embedded_copy_and_actual_pcm_obey_shared_dsp_quota_before_admission() {
    let baseline = DspReservation::used_bytes();
    let filler = DspReservation::new(DSP_QUOTA_BYTES - baseline).unwrap();
    let error = prepare("telephone", &context()).err().unwrap();
    assert!(error.to_string().contains("quota"));
    assert_eq!(DspReservation::used_bytes(), DSP_QUOTA_BYTES);
    drop(filler);
    assert_eq!(DspReservation::used_bytes(), baseline);
    let encoded_room = embedded("telephone").unwrap().bytes.len() + 1024;
    let decode_filler = DspReservation::new(DSP_QUOTA_BYTES - baseline - encoded_room).unwrap();
    let charged = DspReservation::used_bytes();
    let error = prepare("telephone", &context()).err().unwrap();
    assert!(error.to_string().contains("quota"));
    assert_eq!(DspReservation::used_bytes(), charged);
    drop(decode_filler);
    assert_eq!(DspReservation::used_bytes(), baseline);
}
