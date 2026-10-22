use lx_gpui_player::download::planning::{
    self, DownloadQuality as Q, FilenameFormat, NamingAdjustment, NamingError, QualityPlanError,
    compatibility_filename, compatibility_quality, group_component, safe_filename,
    select_executable_quality, track_quality_capabilities,
};
use lx_gpui_player::{domain, source};
use source::SourceCapabilities;

fn capabilities(qualities: &[Q]) -> SourceCapabilities {
    SourceCapabilities {
        kind: "music".into(),
        actions: vec!["musicUrl".into()],
        qualitys: qualities
            .iter()
            .map(|quality| quality.as_str().to_owned())
            .collect(),
    }
}

#[test]
fn exact_quality_order_extensions_and_upstream_downward_fallback_are_stable() {
    assert_eq!(
        planning::QUALITY_ORDER.map(Q::as_str),
        ["flac24bit", "flac", "wav", "ape", "320k", "192k", "128k"]
    );
    assert_eq!(
        planning::QUALITY_ORDER.map(Q::extension),
        ["flac", "flac", "wav", "ape", "mp3", "mp3", "mp3"]
    );
    assert_eq!(
        compatibility_quality(Q::Flac24Bit, &[Q::Flac, Q::K128], Some(&[Q::Flac, Q::K128])),
        Q::K128
    );
    assert_eq!(
        compatibility_quality(Q::Flac, &[Q::K320, Q::K128], Some(&[Q::Flac, Q::K128])),
        Q::K320
    );
    assert_eq!(compatibility_quality(Q::K320, &[Q::K320], None), Q::K128);
    assert_eq!(
        compatibility_quality(Q::K320, &[], Some(&[Q::K320])),
        Q::K128
    );
}

#[test]
fn executable_quality_rechecks_every_selected_source_capability_on_all_five_platforms() {
    for platform in ["kw", "kg", "tx", "wy", "mg"] {
        let selected = select_executable_quality(
            platform,
            Q::Flac,
            &[Q::K320, Q::K128],
            &capabilities(&[Q::Flac, Q::K128]),
        )
        .unwrap();
        assert_eq!(selected.compatibility, Q::K320);
        assert_eq!(selected.actual, Q::K128);
        assert!(selected.source_rechecked);
    }
    assert_eq!(
        select_executable_quality(
            "kg",
            Q::Flac,
            &[Q::K320],
            &capabilities(&[Q::Flac, Q::K128])
        )
        .unwrap_err(),
        QualityPlanError::EmptyIntersection
    );
    assert_eq!(
        select_executable_quality("bd", Q::K128, &[Q::K128], &capabilities(&[Q::K128]))
            .unwrap_err(),
        QualityPlanError::UnsupportedPlatform
    );
    let mut source = capabilities(&[Q::K128]);
    source.actions.clear();
    assert_eq!(
        select_executable_quality("kw", Q::K128, &[Q::K128], &source).unwrap_err(),
        QualityPlanError::NoMusicUrlCapability
    );
}

#[test]
fn unknown_duplicate_empty_and_oversized_capabilities_cannot_create_an_execution_plan() {
    for qualities in [
        vec![],
        vec!["128k".into(), "128k".into()],
        vec!["unknown".into()],
        vec!["128k".into(); 8],
    ] {
        let source = SourceCapabilities {
            kind: "music".into(),
            actions: vec!["musicUrl".into()],
            qualitys: qualities,
        };
        assert_eq!(
            select_executable_quality("wy", Q::K128, &[Q::K128], &source).unwrap_err(),
            QualityPlanError::InvalidCapabilities
        );
    }
}

#[test]
fn normalized_quality_metadata_keeps_platform_hashes_and_json_truthiness() {
    let record = domain::music::TrackRecord::from_lx_dto(serde_json::json!({"id":"kg-id", "name":"歌", "singer":"歌手", "source":"kg", "meta":{"songId":"song", "hash":"base-hash", "_qualitys":{"flac":{"hash":"flac-hash","albumAudioId":"album"},"320k":{},"192k":false,"128k":0}, "qualitys":[]}}), domain::music::LxMusicDtoVersion::Modern).unwrap();
    let before = record.metadata.clone();
    assert_eq!(track_quality_capabilities(&record), vec![Q::Flac, Q::K320]);
    assert_eq!(record.metadata, before);
    assert_eq!(record.metadata["_qualitys"]["flac"]["hash"], "flac-hash");
}

#[test]
fn naming_preserves_first_only_replacements_and_filter_after_stem_clipping() {
    let result =
        compatibility_filename("歌名甲", "歌手乙", "歌手|歌手|歌名|歌名", Q::K320).unwrap();
    assert_eq!(result.basename, "歌手乙歌手歌名甲歌名.mp3");
    assert_eq!(
        compatibility_filename("a#b/?c", "d:e", "歌名 - 歌手", Q::Flac)
            .unwrap()
            .basename,
        "abc - de.flac"
    );
    let title = format!("{}Z", "#".repeat(150));
    assert_eq!(
        compatibility_filename(&title, "", "歌名", Q::K128)
            .unwrap()
            .basename,
        ".mp3"
    );
    assert_eq!(
        safe_filename(&title, "", FilenameFormat::Title, Q::K128)
            .unwrap()
            .basename,
        "未命名.mp3"
    );
}

#[test]
fn singer_separator_and_utf16_surrogate_boundaries_match_the_bounded_contract() {
    let singer = format!("{}、{}、c", "甲".repeat(40), "乙".repeat(40));
    assert_eq!(
        compatibility_filename("", &singer, "歌手", Q::K128)
            .unwrap()
            .basename,
        format!("{}、{}.mp3", "甲".repeat(40), "乙".repeat(40))
    );
    let title = format!("{}🎵Z", "a".repeat(149));
    let result = compatibility_filename(&title, "", "歌名", Q::K128).unwrap();
    assert_eq!(result.basename, format!("{}.mp3", "a".repeat(149)));
    assert_eq!(
        result.adjustments,
        vec![NamingAdjustment::SurrogateBoundaryAvoided]
    );
    assert_eq!(FilenameFormat::default(), FilenameFormat::TitleArtist);
    assert_eq!(
        safe_filename("歌", "人", FilenameFormat::ArtistTitle, Q::Ape)
            .unwrap()
            .basename,
        "人 - 歌.ape"
    );
}

#[test]
fn native_names_reject_devices_controls_traversal_and_absolute_group_override() {
    for name in ["CON", "nul", "COM1", "LPT9", "COM¹", "CONIN$"] {
        assert_eq!(
            safe_filename(name, "", FilenameFormat::Title, Q::K128).unwrap_err(),
            NamingError::ReservedComponent
        );
    }
    assert_eq!(
        safe_filename("x\u{0001}", "", FilenameFormat::Title, Q::K128).unwrap_err(),
        NamingError::InvalidComponent
    );
    for name in [
        "../escape",
        "foo/../escape",
        "C:\\escape",
        "\\\\server\\share",
        "/root",
    ] {
        assert_eq!(
            group_component(Some(name), "默认列表").unwrap_err(),
            NamingError::InvalidComponent
        );
    }
    assert_eq!(
        group_component(None, "默认列表").unwrap().basename,
        "默认列表"
    );
    assert_eq!(
        group_component(Some("我的/播放#列表"), "默认列表")
            .unwrap()
            .basename,
        "我的播放列表"
    );
    assert_eq!(
        safe_filename("歌... ", "", FilenameFormat::Title, Q::Wav)
            .unwrap()
            .basename,
        "歌.wav"
    );
    assert_eq!(
        safe_filename(&"x".repeat(65 * 1024), "", FilenameFormat::Title, Q::Wav).unwrap_err(),
        NamingError::InputBound
    );
}

#[test]
fn all_actual_lx_filename_goldens_match_except_explicit_unpaired_surrogate_avoidance() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/download_reference_goldens.json")).unwrap();
    assert_eq!(
        fixture["upstreamCommit"],
        "ad95d5091c9ed689fa72b5e5c849df65f5a679ce"
    );
    let rows = fixture["naming"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    for row in rows {
        let quality = match row["ext"].as_str().unwrap() {
            "mp3" => Q::K128,
            "flac" => Q::Flac,
            extension => panic!("Unexpected fixture extension {extension}"),
        };
        let planned = compatibility_filename(
            row["title"].as_str().unwrap(),
            row["singer"].as_str().unwrap(),
            row["format"].as_str().unwrap(),
            quality,
        )
        .unwrap();
        let actual: Vec<u16> = planned.basename.encode_utf16().collect();
        let expected: Vec<u16> = row["expectedUtf16"]
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
            .collect();
        if planned.adjustments.is_empty() {
            assert_eq!(actual, expected);
        } else {
            assert_eq!(
                planned.adjustments,
                vec![NamingAdjustment::SurrogateBoundaryAvoided]
            );
            let unpaired: Vec<_> = expected
                .iter()
                .enumerate()
                .filter_map(|(index, unit)| {
                    ((0xd800..=0xdbff).contains(unit)
                        && expected
                            .get(index + 1)
                            .is_none_or(|next| !(0xdc00..=0xdfff).contains(next)))
                    .then_some(index)
                })
                .collect();
            assert_eq!(unpaired, vec![149]);
            let mut safe_expected = expected;
            safe_expected.remove(unpaired[0]);
            assert_eq!(actual, safe_expected);
        }
    }
}
