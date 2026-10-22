use lx_gpui_player::{
    domain::lyric::LyricBundle,
    lyrics::{
        parser::{ParserMode, parse_bundle, parse_lrc, parse_lx, parse_with_mode},
        timeline::{AudioClockSnapshot, ClockStatus, LyricTimeline},
    },
};
use serde_json::json;

fn clock(
    generation: u64,
    position_ms: i64,
    status: ClockStatus,
    rate: f64,
    seek_seq: u64,
) -> AudioClockSnapshot {
    AudioClockSnapshot {
        generation,
        position_ms,
        status,
        rate,
        seek_seq,
    }
}

#[test]
fn lx_mixed_fraction_golden_preserves_file_wide_millisecond_quirk() {
    let document =
        parse_lrc("[00:01.5]one\n[00:02.50]two\n[00:03.500]three\n[00:04.050]four").unwrap();
    assert_eq!(
        document
            .lines
            .iter()
            .map(|line| line.start_ms)
            .collect::<Vec<_>>(),
        vec![1005, 2050, 3500, 4050]
    );
    let no_three_digits = parse_lrc("[00:01.5]one\n[00:02.50]two\n[00:03.05]three").unwrap();
    assert_eq!(
        no_three_digits
            .lines
            .iter()
            .map(|line| line.start_ms)
            .collect::<Vec<_>>(),
        vec![1500, 2500, 3500]
    );
    let conventional = parse_with_mode(
        "[00:01.5]one\n[00:02.50]two\n[00:03.05]three",
        ParserMode::Standard,
    )
    .unwrap();
    assert_eq!(
        conventional
            .lines
            .iter()
            .map(|line| line.start_ms)
            .collect::<Vec<_>>(),
        vec![1500, 2500, 3050]
    );
}

#[test]
fn timed_lines_sort_expand_multitags_and_keep_duplicate_labels_as_extensions() {
    let document = parse_lrc("[TI:Original]\r[offset:-120]\r[00:03.000]later\r[00:01.000][00:02.000]你好\r[0:01.000]translation\r[00:04.000] \r[ti:ignored]").unwrap();
    assert_eq!(document.tags["ti"], "Original");
    assert_eq!(document.offset_ms, -120);
    assert_eq!(document.lines.len(), 3);
    assert_eq!(document.lines[0].start_ms, 1000);
    assert_eq!(document.lines[0].extended, vec!["translation"]);
    assert_eq!(document.lines[1].text, "你好");
    assert_eq!(document.lines[2].text, "later");
}

#[test]
fn translation_and_romanization_use_exact_normalized_labels() {
    let bundle = LyricBundle::from_value(&json!({
        "lyric":"[00:01.005]主行\n[00:02.000]第二行",
        "tlyric":"[0:1.5]译文\n[00:01.006]不得错配\n[00:02.000]//",
        "rlyric":"[00:01.005]roman\n[00:09.000]unmatched"
    }))
    .unwrap();
    let document = parse_bundle(&bundle, ParserMode::LxCompatible).unwrap();
    assert_eq!(document.lines[0].translation, vec!["译文"]);
    assert_eq!(document.lines[0].romanization, vec!["roman"]);
    assert!(document.lines[1].translation.is_empty());
}

#[test]
fn standard_parser_does_not_confuse_leading_decimal_zeros_with_fractional_seconds() {
    let bundle = LyricBundle::from_value(&json!({"lyric":"[00:01.005]five milliseconds",
        "tlyric":"[00:01.5]must not attach\n[00:01.005]exact"}))
    .unwrap();
    let document = parse_bundle(&bundle, ParserMode::Standard).unwrap();
    assert_eq!(document.lines[0].start_ms, 1005);
    assert_eq!(document.lines[0].translation, vec!["exact"]);
}

#[test]
fn word_lyrics_keep_emoji_combining_text_zero_duration_and_malformed_markers() {
    let document =
        parse_lx("[00:01.000]<0,500>你🎵<500,0>e\u{301}<700,300>好\n[00:02.000]<bad,2>visible")
            .unwrap();
    assert!(!document.line_mode);
    assert_eq!(document.lines[0].text, "你🎵e\u{301}好");
    assert_eq!(document.lines[0].words[0].text, "你🎵");
    assert_eq!(document.lines[0].words[1].duration_ms, 0);
    assert_eq!(document.lines[0].words[1].text, "e\u{301}");
    assert_eq!(document.lines[1].text, "<bad,2>visible");
    assert!(document.lines[1].words.is_empty());
}

#[test]
fn parser_handles_metadata_only_invalid_labels_and_conventional_integer_tags_explicitly() {
    assert!(
        parse_lrc("[ar:artist]\ntext only\n[aa:bb.xx]bad\n[00:01]integer")
            .unwrap()
            .lines
            .is_empty()
    );
    let standard =
        parse_with_mode("[00:01]integer\n[00:02.1]fraction", ParserMode::Standard).unwrap();
    assert_eq!(standard.lines[0].start_ms, 1000);
    assert_eq!(standard.lines[1].start_ms, 2100);
    assert!(parse_lrc(&"x".repeat(2 * 1024 * 1024 + 1)).is_err());
}

#[test]
fn timeline_uses_engine_position_pauses_and_repositions_on_paused_seek() {
    let document = parse_lx("[00:01.000]<0,500>一<500,500>二\n[00:02.000]<0,1000>三").unwrap();
    let mut timeline = LyricTimeline::new(document, 7);
    let before = timeline
        .update(&clock(7, 999, ClockStatus::Playing, 1.0, 0))
        .unwrap();
    assert_eq!(before.line_index, None);
    let playing = timeline
        .update(&clock(7, 1250, ClockStatus::Playing, 1.0, 0))
        .unwrap();
    assert_eq!(playing.line_index, Some(0));
    assert_eq!(playing.word_index, Some(0));
    assert_eq!(playing.word_progress, 0.5);
    let paused = timeline
        .update(&clock(7, 1250, ClockStatus::Paused, 1.0, 0))
        .unwrap();
    let no_wall_time = timeline
        .update(&clock(7, 1800, ClockStatus::Paused, 1.0, 0))
        .unwrap();
    assert_eq!(paused, no_wall_time);
    let seek = timeline
        .update(&clock(7, 2250, ClockStatus::Paused, 1.0, 1))
        .unwrap();
    assert_eq!(seek.line_index, Some(1));
    assert_eq!(seek.word_progress, 0.25);
    let backwards = timeline
        .update(&clock(7, 1100, ClockStatus::Paused, 1.0, 2))
        .unwrap();
    assert_eq!(backwards.line_index, Some(0));
    assert_eq!(backwards.word_progress, 0.2);
    assert!(
        timeline
            .update(&clock(7, 2500, ClockStatus::Paused, 1.0, 1))
            .is_none()
    );
}

#[test]
fn positive_offsets_advance_negative_offsets_delay_and_rate_only_scales_offset() {
    let document = parse_lrc("[offset:100]\n[00:01.000]one\n[00:02.000]two").unwrap();
    let mut timeline = LyricTimeline::new(document, 1);
    timeline.set_line_compensation(0);
    assert_eq!(
        timeline
            .update(&clock(1, 900, ClockStatus::Playing, 1.0, 0))
            .unwrap()
            .line_index,
        Some(0)
    );
    timeline.set_offset(-200);
    assert_eq!(
        timeline
            .update(&clock(1, 1000, ClockStatus::Playing, 1.0, 0))
            .unwrap()
            .line_index,
        None
    );
    timeline.set_offset(0);
    assert_eq!(
        timeline
            .update(&clock(1, 800, ClockStatus::Playing, 2.0, 0))
            .unwrap()
            .line_index,
        Some(0)
    );
    assert_eq!(
        timeline
            .update(&clock(1, 950, ClockStatus::Playing, 0.5, 0))
            .unwrap()
            .line_index,
        Some(0)
    );
    // A media position of 1000ms at 2x remains 1000ms, not 2000ms.
    assert_eq!(
        timeline
            .update(&clock(1, 1000, ClockStatus::Playing, 2.0, 0))
            .unwrap()
            .line_index,
        Some(0)
    );
    timeline.set_line_compensation(60);
    assert_eq!(
        timeline
            .update(&clock(1, 840, ClockStatus::Playing, 1.0, 0))
            .unwrap()
            .line_index,
        Some(0)
    );
}

#[test]
fn stale_generations_cannot_replace_new_lyrics_and_invalid_clocks_are_rejected() {
    let mut timeline = LyricTimeline::new(parse_lrc("[00:00.000]A").unwrap(), 1);
    assert_eq!(
        timeline
            .update(&clock(1, 0, ClockStatus::Playing, 1.0, 0))
            .unwrap()
            .line_text,
        "A"
    );
    timeline.set_document(parse_lrc("[00:00.000]B").unwrap(), 2);
    assert!(
        timeline
            .update(&clock(1, 0, ClockStatus::Playing, 1.0, 0))
            .is_none()
    );
    assert_eq!(
        timeline
            .update(&clock(2, 0, ClockStatus::Playing, 1.0, 0))
            .unwrap()
            .line_text,
        "B"
    );
    for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(
            timeline
                .update(&clock(2, 0, ClockStatus::Playing, rate, 0))
                .is_none()
        );
    }
    assert!(
        timeline
            .update(&clock(2, 0, ClockStatus::Idle, 1.0, 0))
            .is_none()
    );
}

#[test]
fn word_gaps_overlaps_and_last_word_completion_are_bounded() {
    let mut timeline = LyricTimeline::new(
        parse_lx("[00:01.000]<100,500>A<300,0>B<700,200>C").unwrap(),
        1,
    );
    assert_eq!(
        timeline
            .update(&clock(1, 1050, ClockStatus::Playing, 1.0, 0))
            .unwrap()
            .word_index,
        None
    );
    let overlap = timeline
        .update(&clock(1, 1350, ClockStatus::Playing, 1.0, 0))
        .unwrap();
    assert_eq!(overlap.word_index, Some(1));
    assert_eq!(overlap.word_progress, 1.0);
    let gap = timeline
        .update(&clock(1, 1650, ClockStatus::Playing, 1.0, 0))
        .unwrap();
    assert_eq!(gap.word_index, Some(1));
    let done = timeline
        .update(&clock(1, 2500, ClockStatus::Ended, 1.0, 0))
        .unwrap();
    assert_eq!(done.word_index, Some(2));
    assert_eq!(done.word_progress, 1.0);
    assert_eq!(done.line_progress, 1.0);
}

#[test]
fn bundle_prefers_available_word_track_and_falls_back_from_empty_word_track() {
    let full = LyricBundle::from_value(
        &json!({"lyric":"[00:01.000]普通", "lxlyric":"[00:01.000]<0,1000>逐字"}),
    )
    .unwrap();
    assert!(
        !parse_bundle(&full, ParserMode::LxCompatible)
            .unwrap()
            .line_mode
    );
    let empty = LyricBundle::from_value(&json!({"lyric":"[00:01.000]普通", "lxlyric":""})).unwrap();
    assert_eq!(
        parse_bundle(&empty, ParserMode::LxCompatible)
            .unwrap()
            .lines[0]
            .text,
        "普通"
    );
}
