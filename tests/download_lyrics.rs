use base64::{Engine, engine::general_purpose::STANDARD};
use domain::lyric::{LyricBundle, LyricField, LyricOrigin, MAX_LYRIC_BYTES};
use lx_gpui_player::domain;
use lx_gpui_player::download::lyrics::{
    self, DownloadLyricError, DownloadLyricOptions, LyricArtifactEncoding,
    LyricCompatibilityAdjustment, build_download_lyrics, encode_lyric_artifact,
};

fn field(value: &str) -> LyricField {
    LyricField::Text(value.into())
}
fn bundle() -> LyricBundle {
    LyricBundle {
        lyric: field("[00:01.20]甲\n[00:02.00]乙\n[00:03.00] \n"),
        tlyric: field(
            "[0:1.20]译甲\n[00:02.00][00:09.00]译乙\n[00:03.00]不匹配空行\n[00:08.00]不匹配时间",
        ),
        rlyric: field("[00:01.20]jia\n[00:02.00]yi"),
        lxlyric: field("[0,1000](0,1000,0)甲"),
    }
}
fn flags(container: bool, translation: bool, romanization: bool) -> DownloadLyricOptions {
    DownloadLyricOptions {
        include_awlrc: container,
        translation,
        romanization,
    }
}

#[test]
fn ordinary_only_lyrics_keep_original_whitespace_and_line_ending_bytes() {
    let original = "  [00:01.00]中文\r\n[00:02]第二行  \r\n";
    let value = LyricBundle {
        lyric: field(original),
        ..Default::default()
    };
    for container in [true, false] {
        assert_eq!(
            build_download_lyrics(&value, flags(container, true, true))
                .unwrap()
                .text,
            original
        );
    }
}

#[test]
fn translation_and_romanization_filter_against_nonempty_ordinary_labels_in_original_order() {
    let output = build_download_lyrics(&bundle(), flags(false, true, true)).unwrap();
    assert_eq!(
        output.text,
        "[00:01.20]甲\n[00:02.00]乙\n[00:03.00]\n\n[0:1.20]译甲\n[00:02.00]译乙\n\n[00:01.20]jia\n[00:02.00]yi\n"
    );
    assert!(output.has_ordinary_lyrics);
    assert!(output.adjustments.is_empty());
}

#[test]
fn all_flag_combinations_preserve_all_four_trimmed_awlrc_channels_independently() {
    let value = bundle();
    let expected_container = format!(
        "[awlrc:lrc:{},tlrc:{},rlrc:{},awlrc:{}]",
        STANDARD.encode(value.lyric.text().unwrap().trim()),
        STANDARD.encode(value.tlyric.text().unwrap().trim()),
        STANDARD.encode(value.rlyric.text().unwrap().trim()),
        STANDARD.encode(value.lxlyric.text().unwrap().trim())
    );
    for container in [false, true] {
        for translation in [false, true] {
            for romanization in [false, true] {
                let output =
                    build_download_lyrics(&value, flags(container, translation, romanization))
                        .unwrap();
                assert_eq!(output.text.contains(&expected_container), container);
                let visible = output.text.split("[awlrc:").next().unwrap();
                assert_eq!(visible.contains("[0:1.20]译甲"), translation);
                assert_eq!(visible.contains("[00:01.20]jia"), romanization);
            }
        }
    }
}

#[test]
fn duplicate_labels_line_order_and_fractional_zero_normalization_are_lx_compatible() {
    let value = LyricBundle {
        lyric: field("[00:01.20]a\n[00:02.00]b"),
        tlyric: field("[00:02.00]b1\n[000:001.020]a1\n[00:02.00]b2"),
        ..Default::default()
    };
    assert_eq!(
        build_download_lyrics(&value, flags(false, true, false))
            .unwrap()
            .text,
        "[00:01.20]a\n[00:02.00]b\n\n[00:02.00]b1\n[000:001.020]a1\n[00:02.00]b2\n"
    );
}

#[test]
fn lx_timestamp_matching_requires_a_fraction_and_does_not_retime_labels() {
    let value = LyricBundle {
        lyric: field("[00:01]a\n[00:02.00]b"),
        tlyric: field("[00:01]译a\n[00:02.00]译b"),
        ..Default::default()
    };
    assert_eq!(
        build_download_lyrics(&value, flags(false, true, false))
            .unwrap()
            .text,
        "[00:01]a\n[00:02.00]b\n\n[00:02.00]译b\n"
    );
}

#[test]
fn missing_null_and_whitespace_only_channels_do_not_create_fake_container_content() {
    let value = LyricBundle {
        lyric: LyricField::Null,
        tlyric: field("  "),
        rlyric: LyricField::Missing,
        lxlyric: field(""),
    };
    let output = build_download_lyrics(&value, flags(true, false, false)).unwrap();
    assert_eq!(output.text, "");
    assert!(!output.has_ordinary_lyrics);
    assert_eq!(
        output.adjustments,
        vec![LyricCompatibilityAdjustment::WhitespaceOnlyContainerChannelOmitted]
    );
    let missing = encode_lyric_artifact(
        &LyricBundle::default(),
        flags(false, false, false),
        LyricArtifactEncoding::Utf8,
        LyricOrigin::Raw,
        0,
    )
    .unwrap();
    let null = encode_lyric_artifact(
        &LyricBundle {
            lyric: LyricField::Null,
            ..Default::default()
        },
        flags(false, false, false),
        LyricArtifactEncoding::Utf8,
        LyricOrigin::Raw,
        0,
    )
    .unwrap();
    assert_ne!(missing.bundle_sha256, null.bundle_sha256);
}

#[test]
fn utf8_has_a_real_bom_and_gbk_roundtrips_without_an_invented_bom_or_replacements() {
    let value = LyricBundle {
        lyric: field("[00:01.00]中文歌词"),
        ..Default::default()
    };
    let utf8 = encode_lyric_artifact(
        &value,
        flags(false, false, false),
        LyricArtifactEncoding::Utf8,
        LyricOrigin::Edited,
        17,
    )
    .unwrap();
    assert!(utf8.bytes.starts_with(&[0xef, 0xbb, 0xbf]));
    assert_eq!(
        std::str::from_utf8(&utf8.bytes[3..]).unwrap(),
        value.lyric.text().unwrap()
    );
    assert_eq!(utf8.origin, LyricOrigin::Edited);
    assert_eq!(utf8.snapshot_revision, 17);
    let gbk = encode_lyric_artifact(
        &value,
        flags(false, false, false),
        LyricArtifactEncoding::Gbk,
        LyricOrigin::Raw,
        3,
    )
    .unwrap();
    assert!(!gbk.bytes.starts_with(&[0xef, 0xbb, 0xbf]));
    let (text, _, errors) = encoding_rs::GBK.decode(&gbk.bytes);
    assert!(!errors);
    assert_eq!(text, value.lyric.text().unwrap());
    assert_eq!(gbk.encoding, LyricArtifactEncoding::Gbk);
    assert_eq!(gbk.options, flags(false, false, false));
    assert!(gbk.has_ordinary_lyrics);
    assert!(gbk.adjustments.is_empty());
    let unsupported = LyricBundle {
        lyric: field("[00:01.00]🎵"),
        ..Default::default()
    };
    assert_eq!(
        encode_lyric_artifact(
            &unsupported,
            flags(false, false, false),
            LyricArtifactEncoding::Gbk,
            LyricOrigin::Raw,
            0
        )
        .unwrap_err(),
        DownloadLyricError::UnrepresentableGbk
    );
}

#[test]
fn bundle_and_timestamp_index_bounds_are_enforced_before_unbounded_output() {
    let oversized = LyricBundle {
        lyric: field(&"a".repeat(MAX_LYRIC_BYTES + 1)),
        ..Default::default()
    };
    assert_eq!(
        build_download_lyrics(&oversized, flags(true, true, true)).unwrap_err(),
        DownloadLyricError::InvalidBundle
    );
    let mut ordinary = String::new();
    for index in 0..=lyrics::MAX_DOWNLOAD_TIMESTAMP_LABELS {
        ordinary.push_str(&format!(
            "[{:03}:{:03}.{:03}]x\n",
            index / 1000,
            index % 1000,
            index % 997
        ));
    }
    let indexed = LyricBundle {
        lyric: field(&ordinary),
        tlyric: field("[0:0.0]x"),
        ..Default::default()
    };
    assert_eq!(
        build_download_lyrics(&indexed, DownloadLyricOptions::default()).unwrap_err(),
        DownloadLyricError::TimestampIndexBound
    );
    const { assert!(lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES >= MAX_LYRIC_BYTES) };
}

#[test]
fn all_actual_lx_lyric_flag_goldens_match_except_typed_empty_trimmed_container_content() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/download_reference_goldens.json")).unwrap();
    assert_eq!(
        fixture["upstreamCommit"],
        "ad95d5091c9ed689fa72b5e5c849df65f5a679ce"
    );
    let rows = fixture["lyrics"].as_array().unwrap();
    assert_eq!(rows.len(), 32);
    for row in rows {
        let index = usize::try_from(row["bundleIndex"].as_u64().unwrap()).unwrap();
        let value = LyricBundle::from_value(&fixture["bundles"][index]).unwrap();
        let options = flags(
            row["includeAwlrc"].as_bool().unwrap(),
            row["translation"].as_bool().unwrap(),
            row["romanization"].as_bool().unwrap(),
        );
        let native = build_download_lyrics(&value, options).unwrap();
        let expected = row["expected"].as_str().unwrap();
        if native.adjustments.is_empty() {
            assert_eq!(native.text, expected, "Bundle {index}, options {options:?}");
        } else {
            assert_eq!(index, 3);
            assert!(options.include_awlrc);
            assert_eq!(
                native.adjustments,
                vec![LyricCompatibilityAdjustment::WhitespaceOnlyContainerChannelOmitted]
            );
            assert_eq!(expected, "\n\n[awlrc:tlrc:]\n");
            assert_eq!(native.text, if options.translation { "\n\n\n" } else { "" });
        }
    }
}
