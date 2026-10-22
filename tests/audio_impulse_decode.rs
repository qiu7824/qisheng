use lx_gpui_player::audio::impulse_decode::{
    ImpulseDecodeContext, ImpulseEncodedInput, decode_impulse,
};
use std::time::Duration;
#[path = "support/encoded_audio_fixture.rs"]
#[allow(dead_code)]
mod encoded_fixture;
fn wave(rate: u32, channels: u16, frames: usize) -> Vec<u8> {
    let length = (frames * channels as usize * 2) as u32;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + length).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    bytes.extend_from_slice(&(channels * 2).to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for index in 0..frames {
        for channel in 0..channels {
            let value = if index == 0 {
                1000i16 + channel as i16
            } else {
                0
            };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}
fn encoded(bytes: &[u8]) -> ImpulseEncodedInput {
    let mut input = ImpulseEncodedInput::prepare(bytes.len()).unwrap();
    for chunk in bytes.chunks(4096) {
        input.append(chunk).unwrap();
    }
    input
}
#[test]
fn actual_lossless_ir_decode_preserves_rate_channels_and_complete_frame_count() {
    for channels in [1, 2, 4] {
        let bytes = wave(48000, channels, 537);
        let decoded = decode_impulse(
            encoded(&bytes),
            "wav",
            &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
        )
        .unwrap();
        assert_eq!(decoded.decoded_frames, 537);
        assert_eq!(decoded.impulse.sample_rate(), 48000);
        assert_eq!(decoded.impulse.channels(), channels);
        assert_eq!(decoded.impulse.frames(), 537);
    }
}
#[test]
fn invalid_layout_truncated_body_and_overlong_ir_never_produce_a_validated_resource() {
    assert!(
        decode_impulse(
            encoded(&wave(8000, 3, 80)),
            "wav",
            &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
        )
        .is_err()
    );
    assert!(
        decode_impulse(
            encoded(&wave(8000, 1, 32001)),
            "wav",
            &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
        )
        .is_err()
    );
    let mut broken = wave(22050, 1, 128);
    broken.truncate(broken.len() - 8);
    assert!(
        decode_impulse(
            encoded(&broken),
            "wav",
            &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
        )
        .is_err()
    );
    assert!(decode_impulse(encoded(&wave(48000, 1, 128)), "wav", &cancelled()).is_err());
    assert!(ImpulseEncodedInput::prepare(8 * 1024 * 1024 + 1).is_err());
    assert!(
        decode_impulse(
            encoded(&wave(48000, 1, 128)),
            "flac",
            &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
        )
        .is_err()
    );
    assert!(
        decode_impulse(
            encoded(b"OggSnot an admitted wave"),
            "wav",
            &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
        )
        .is_err()
    );
    let mut input = ImpulseEncodedInput::prepare(4).unwrap();
    assert!(input.append(b"too many bytes").is_err());
    assert!(input.is_empty());
}
#[test]
fn expired_decode_context_cannot_allocate_a_decoder_or_accept_encoded_input() {
    let context = ImpulseDecodeContext::new(Duration::from_nanos(1)).unwrap();
    std::thread::sleep(Duration::from_millis(1));
    assert!(decode_impulse(encoded(&wave(48000, 1, 128)), "wav", &context).is_err());
    assert!(ImpulseDecodeContext::new(Duration::ZERO).is_err());
    assert!(ImpulseDecodeContext::new(Duration::from_secs(121)).is_err());
}
#[test]
fn actual_flac_streaminfo_is_bounded_before_codec_workspace_allocation() {
    let bytes = encoded_fixture::mono_verbatim_flac();
    let decoded = decode_impulse(
        encoded(&bytes),
        "flac",
        &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
    )
    .unwrap();
    assert_eq!(decoded.decoded_frames, 512);
    let mut oversized = bytes;
    oversized[10..12].copy_from_slice(&65535u16.to_be_bytes());
    assert!(
        decode_impulse(
            encoded(&oversized),
            "flac",
            &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
        )
        .is_err()
    );
}

fn cancelled() -> ImpulseDecodeContext {
    let context = ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap();
    context.cancel();
    context
}
