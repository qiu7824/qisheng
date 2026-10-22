use lx_gpui_player::{
    domain::{
        list_tools::*,
        lists::{AddLocation, ListAction, ListInfo},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::Repository,
};
use serde_json::{Value, json};
use std::fs;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/list-tools-goldens.json")).unwrap()
}
fn rows(value: &Value) -> Vec<TrackRecord> {
    value.as_array().unwrap().iter().map(|row|{let mut meta=json!({"songId":row["id"],"qualitys":[],"opaque":{"retained":true}});if let Some(album)=row.get("albumName"){meta["albumName"]=album.clone();}let mut dto=json!({"id":row["id"],"name":row["name"],"singer":row["singer"],"source":row["source"],"meta":meta});if let Some(interval)=row.get("interval"){dto["interval"]=interval.clone();}TrackRecord::from_lx_dto(dto,LxMusicDtoVersion::Modern).unwrap()}).collect()
}
fn profile() -> tempfile::TempDir {
    let base = std::env::temp_dir();
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("tools-")
        .tempdir_in(base)
        .unwrap()
}

#[test]
fn five_fields_four_locales_and_null_intervals_match_node_oracle_without_mutation() {
    let data = fixture();
    assert_eq!(data["runtime"]["icu"], "78.3");
    assert_eq!(data["runtime"]["cldr"], "48.0");
    let list = rows(&data["sort_rows"]);
    let before = list.clone();
    for case in data["sorts"].as_array().unwrap() {
        let locale = case["locale"].as_str().unwrap();
        let comparator = IcuTextComparator::new(locale).unwrap();
        let field: SortField = serde_json::from_value(case["field"].clone()).unwrap();
        let direction = serde_json::from_value(case["direction"].clone()).unwrap();
        let order = sort_order(&list, field, direction, &comparator).unwrap();
        let actual: Vec<_> = order.into_iter().map(|i| list[i].lx_id.as_str()).collect();
        let expected: Vec<_> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap())
            .collect();
        assert_eq!(actual, expected, "{locale} {:?} {:?}", field, direction);
    }
    assert_eq!(list, before);
    let times = rows(&data["time_rows"]);
    let comparator = IcuTextComparator::new("en-US").unwrap();
    for case in data["time_sorts"].as_array().unwrap() {
        let direction = serde_json::from_value(case["direction"].clone()).unwrap();
        let actual: Vec<_> = sort_order(&times, SortField::Interval, direction, &comparator)
            .unwrap()
            .into_iter()
            .map(|i| times[i].lx_id.as_str())
            .collect();
        let expected: Vec<_> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap())
            .collect();
        assert_eq!(actual, expected);
    }
}
#[test]
fn injected_comparator_and_fisher_yates_use_explicit_inputs() {
    let data = fixture();
    let list = rows(
        &json!([{ "id":"a","name":"A","singer":"","source":"kw"},{"id":"b","name":"B","singer":"","source":"kw"}]),
    );
    let comparator = |a: &str, b: &str| b.cmp(a);
    assert_eq!(
        sort_order(&list, SortField::Name, SortDirection::Up, &comparator).unwrap(),
        vec![1, 0]
    );
    for case in data["shuffles"].as_array().unwrap() {
        let mut draws = case["draws"].as_array().unwrap().iter();
        let length = case["length"].as_u64().unwrap() as usize;
        let actual = shuffle_order(length, |upper| {
            let draw = draws.next().expect("draw");
            assert_eq!(upper, draw["upper"].as_u64().unwrap() as usize);
            Ok(draw["index"].as_u64().unwrap() as usize)
        })
        .unwrap();
        assert!(draws.next().is_none());
        assert_eq!(serde_json::to_value(actual).unwrap(), case["expected"]);
    }
    assert!(shuffle_order(2, Ok).is_err());
    assert!(shuffle_order(MAX_LIST_TOOL_ITEMS + 1, |_| Ok(0)).is_err());
    assert!(IcuTextComparator::new("en_US").is_err());
    assert!(IcuTextComparator::new(DEFAULT_LIST_LOCALE).is_ok());
}
#[test]
fn duplicate_groups_return_every_member_and_preserve_discovery_ties() {
    let data = fixture();
    let list = rows(&data["duplicate_rows"]);
    let before = list.clone();
    for case in data["duplicate_cases"].as_array().unwrap() {
        let comparator = IcuTextComparator::new(case["locale"].as_str().unwrap()).unwrap();
        let actual = duplicate_candidates(
            &list,
            case["filter_variants"].as_bool().unwrap(),
            &comparator,
        )
        .unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), case["expected"]);
    }
    assert_eq!(duplicate_key("Song (Live)", true), "song");
    assert_eq!(duplicate_key("Song Live", true), "songlive");
    assert_eq!(duplicate_key("A (x) B (y)", true), "a");
    assert_eq!(duplicate_key("A (Live）", true), "a");
    assert_eq!(duplicate_key("（x）", true), "（x）");
    assert_eq!(duplicate_key("  Song (Live)  ", false), "song (live)");
    assert_eq!(list, before);
}
#[test]
fn search_tiers_utf16_fuzzy_case_scores_and_literal_symbols_match_node() {
    let data = fixture();
    let list = rows(&data["search_rows"]);
    let before = list.clone();
    for case in data["searches"].as_array().unwrap() {
        let query = case["text"].as_str().unwrap();
        let actual = search_list(&list, query, &ListToolLimits::default()).unwrap();
        let expected = case["expected"].as_array().unwrap();
        assert_eq!(actual.len(), expected.len(), "query {query:?}");
        for (hit, row) in actual.iter().zip(expected) {
            assert_eq!(
                hit.index,
                row["index"].as_u64().unwrap() as usize,
                "query {query:?}"
            );
            assert_eq!(list[hit.index].lx_id, row["id"].as_str().unwrap());
            assert_eq!(serde_json::to_value(hit.kind).unwrap(), row["kind"]);
            match (hit.score, row["score"].as_f64()) {
                (None, None) => {}
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-12, "query {query:?}: {a} vs {b}"),
                _ => panic!("score presence {query:?}"),
            }
        }
    }
    assert_eq!(list, before);
}
#[test]
fn search_budgets_reject_work_without_truncating_successful_results() {
    let data = fixture();
    let list = rows(&data["search_rows"]);
    let limits = ListToolLimits {
        max_items: 1,
        ..Default::default()
    };
    assert!(search_list(&list, "ab", &limits).is_err());
    let limits = ListToolLimits {
        max_query_utf16: 1,
        ..Default::default()
    };
    assert!(search_list(&list, "🎵", &limits).is_err());
    let limits = ListToolLimits {
        max_match_cells: 0,
        ..Default::default()
    };
    assert!(search_list(&list, "ab", &limits).is_err());
    let limits = ListToolLimits {
        max_distance_cells: 0,
        ..Default::default()
    };
    assert!(search_list(&list, "ab", &limits).is_err());
    assert_eq!(
        search_list(&list, "", &ListToolLimits::default())
            .unwrap()
            .len(),
        list.len()
    );
}
#[test]
fn sql_pages_and_single_lookup_return_only_list_copies_with_revision() {
    let p = profile();
    let repo = Repository::open(p.path()).unwrap();
    repo.apply_list_action(
        &ListAction::Create {
            position: 0,
            lists: vec![ListInfo::user("paged", "Paged")],
        },
        None,
    )
    .unwrap();
    let tracks:Vec<_>=(0..1505).map(|i|rows(&json!([{"id":format!("page-{i}"),"name":format!("Name {i}"),"singer":"Artist","source":"kw"}])).remove(0)).collect();
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "paged".into(),
            tracks: tracks.clone(),
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let mut edited = tracks[1002].clone();
    edited.title = "List-only metadata".into();
    repo.apply_list_action(
        &ListAction::MusicUpdate {
            updates: vec![lx_gpui_player::domain::lists::ListMusicUpdate {
                list_id: "paged".into(),
                track: edited.clone(),
            }],
        },
        None,
    )
    .unwrap();
    let page = repo.list_tracks_page("paged", 1000, 5).unwrap();
    assert_eq!(page.total, 1505);
    assert_eq!(page.offset, 1000);
    assert_eq!(page.limit, 5);
    assert_eq!(page.revision, repo.revision().unwrap());
    assert_eq!(page.items[2], edited);
    assert_eq!(
        repo.list_track("paged", &edited.id).unwrap(),
        Some(edited.clone())
    );
    assert_eq!(
        repo.list_track("paged", &edited.lx_id).unwrap(),
        Some(edited)
    );
    assert_eq!(
        repo.track(&tracks[1002].id).unwrap(),
        Some(tracks[1002].clone())
    );
    let external = rusqlite::Connection::open(repo.path()).unwrap();
    external
        .execute(
            "UPDATE music_list_items SET document='{}' WHERE list_id='paged' AND music_id=?1",
            [&tracks[0].lx_id],
        )
        .unwrap();
    assert!(repo.list_tracks_page("paged", 1000, 5).is_ok());
    assert!(repo.list_tracks_page("paged", 0, 1).is_err());
    assert!(repo.list_track("paged", &tracks[0].id).is_err());
    assert!(repo.list_track("missing", "id").is_err());
    assert!(repo.list_track("paged", "missing").unwrap().is_none());
    assert!(repo.list_tracks_page("missing", 0, 10).is_err());
    assert!(repo.list_tracks_page("paged", 0, 1001).is_err());
    assert!(repo.list_tracks_page("paged", usize::MAX, 1).is_err());
    assert!(
        repo.list_tracks_page("paged", 1505, 10)
            .unwrap()
            .items
            .is_empty()
    );
    let empty = repo.list_tracks_page("paged", 0, 0).unwrap();
    assert_eq!(empty.total, 1505);
    assert!(empty.items.is_empty());
    repo.integrity_check().unwrap();
}
