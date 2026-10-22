use base64::Engine;
use flate2::{Compression, write::ZlibEncoder};
use lx_gpui_player::{
    domain::lyric::{LyricField, MAX_LYRIC_BYTES},
    lyrics::codec::*,
};
use std::io::Write;

const QRC: &str = "ee7befb746f93831d42dcdf2e21f0d5dd9fd95a45509a6454e1d4966f9a90d4615e209ab93b120810ad3921741fd44cfa5a0163825ba57fe79858345e1b548ac7c52daf726a03c69c2576b2ac384029bf2e00bbc52b89c8ba0215bccd3cc44f1c98d4aaf555461d7";
const MRC: &str = "b4b2a3da51aaeec40a220e734d6daafa5ab329d576b2b6f5dfd12a464f0ec53d3d69b75b833a01dc0f9a0b536b6e4263";
fn krc(text: &str) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(text.as_bytes()).unwrap();
    let key = [
        0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e,
        0x69,
    ];
    let mut out = b"krc1".to_vec();
    out.extend(
        enc.finish()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % 16]),
    );
    out
}

#[test]
fn qrc_variant_cipher_matches_the_fixed_upstream_js_golden() {
    let expected = "<QrcInfos><LyricInfo LyricContent=\"[1000,700]Hi(900,100) 世界(1100,300)\n[2000,500]End(2000,500)\"/></QrcInfos>";
    let decoded = decode_qrc(QRC).unwrap();
    assert_eq!(decoded, expected);
    assert_eq!(
        qrc_to_lrc(&decoded).unwrap(),
        "[00:01.000]Hi 世界\n[00:02.000]End"
    );
    assert_eq!(
        qrc_to_lx(&decoded).unwrap(),
        "[00:01.000]<0,100>Hi<100,300> 世界\n[00:02.000]<0,500>End"
    );
    let bundle = decode_tx_lyrics(QRC, None, Some(QRC)).unwrap();
    assert_eq!(
        bundle.lyric.text(),
        Some("[00:01.000]Hi 世界\n[00:02.000]End")
    );
    assert_eq!(bundle.rlyric, bundle.lyric);
    assert_eq!(bundle.tlyric, LyricField::Missing);
}
#[test]
fn mrc_signed_word_arithmetic_matches_the_fixed_upstream_js_golden() {
    let plain = decode_mrc(MRC).unwrap();
    assert_eq!(plain, "[1000,500]ABCD(1000,500)");
    assert_eq!(mrc_to_lrc(&plain).unwrap(), "[00:01.000]ABCD");
    assert_eq!(mrc_to_lx(&plain).unwrap(), "[00:01.000]<0,500>ABCD");
    assert_eq!(
        mrc_to_lx("[1000,500]early(900,100)").unwrap(),
        "[00:01.000]<-100,100>early"
    );
    let document =
        lx_gpui_player::lyrics::parser::parse_lx(&mrc_to_lx("[1000,500]early(900,100)").unwrap())
            .unwrap();
    assert_eq!(document.lines[0].words[0].start_ms, -100);
    assert_eq!(decode_mrc("[00:01.0]short").unwrap(), "[00:01.0]short");
}
#[test]
fn krc_container_restores_four_channels_and_keeps_relative_word_times() {
    let language=base64::engine::general_purpose::STANDARD.encode(r#"{"content":[{"type":0,"lyricContent":[["he","llo"]]},{"type":1,"lyricContent":[["你","好"]]}]}"#);
    let raw = format!(
        "[id:$OWNED]\n[language:{language}]\n[1234,600]<0,300,0>Hello&nbsp;<300,300,0>&amp; world"
    );
    let bundle = decode_krc_bytes(&krc(&raw)).unwrap();
    assert_eq!(bundle.lyric.text(), Some("[00:01.234]Hello & world"));
    assert_eq!(
        bundle.lxlyric.text(),
        Some("[00:01.234]<0,300>Hello <300,300>& world")
    );
    assert_eq!(bundle.tlyric.text(), Some("[00:01.234]你好"));
    assert_eq!(bundle.rlyric.text(), Some("[00:01.234]hello"));
    let base64 = base64::engine::general_purpose::STANDARD.encode(krc(&raw));
    assert_eq!(decode_krc(&base64).unwrap(), bundle);
}
#[test]
fn malformed_and_oversized_codec_inputs_have_bounded_explicit_results() {
    assert!(decode_krc_bytes(b"wrong").is_err());
    assert!(decode_krc("%%%=").is_err());
    assert!(decode_krc_bytes(&krc(&"x".repeat(MAX_LYRIC_BYTES + 1))).is_err());
    assert_eq!(decode_qrc("abc").unwrap(), "");
    assert_eq!(decode_qrc("not hex!").unwrap(), "");
    assert!(decode_qrc(&"0".repeat(MAX_LYRIC_BYTES * 2 + 2)).is_err());
    assert!(decode_mrc(&"z".repeat(32)).is_err());
    assert!(decode_mrc(&"0".repeat(MAX_LYRIC_BYTES + 1)).is_err());
    assert!(qrc_to_lrc("<!DOCTYPE q [<!ENTITY x 'e'>]><q LyricContent='&x;'/>").is_err());
    assert!(parse_krc_text("[id:$OWNED]").is_ok());
}
#[test]
fn qrc_xml_attribute_keeps_literal_newlines_instead_of_normalizing_them_to_spaces() {
    let xml = "<q LyricContent='[0,100]A(0,100)\n[100,100]B(100,100)'/>";
    assert_eq!(qrc_to_lrc(xml).unwrap(), "[00:00.000]A\n[00:00.100]B");
}

#[test]
fn selected_local_krc_is_read_before_embedded_and_keeps_the_four_channels() {
    let directory = tempfile::tempdir().unwrap();
    let audio = directory.path().join("original.wav");
    let data = vec![0u8; 1600];
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&8000u32.to_le_bytes());
    wav.extend_from_slice(&16000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(&audio, wav).unwrap();
    let raw = "[0,1000]<0,1000,0>Owned local KRC";
    std::fs::write(audio.with_extension("krc"), krc(raw)).unwrap();
    let lyrics = lx_gpui_player::lyrics::local::read_local_lyrics(&audio)
        .unwrap()
        .unwrap();
    assert_eq!(
        lyrics.bundle.lyric.text(),
        Some("[00:00.000]Owned local KRC")
    );
    assert_eq!(
        lyrics.bundle.lxlyric.text(),
        Some("[00:00.000]<0,1000>Owned local KRC")
    );
    assert!(lyrics.sidecar_path.unwrap().ends_with("original.krc"));
    std::fs::write(audio.with_extension("lrc"), "[00:00.000]LRC priority").unwrap();
    assert_eq!(
        lx_gpui_player::lyrics::local::read_local_lyrics(&audio)
            .unwrap()
            .unwrap()
            .bundle
            .lyric
            .text(),
        Some("[00:00.000]LRC priority")
    );
}
