use base64::{Engine, engine::general_purpose::STANDARD};
use lx_gpui_player::platforms::{
    kg::{KgProvider, signature_params},
    *,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Mock {
    replies: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}
impl Mock {
    fn with(values: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(
                values
                    .into_iter()
                    .map(|v| HttpResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                        bytes: serde_json::to_vec(&v).unwrap(),
                    })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn text(&self, value: &str) {
        self.replies.lock().unwrap().push_back(HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            bytes: value.as_bytes().to_vec(),
        });
    }
    fn json(&self, value: Value) {
        self.replies.lock().unwrap().push_back(HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            bytes: serde_json::to_vec(&value).unwrap(),
        });
    }
}
impl HttpTransport for Mock {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        self.requests.lock().unwrap().push(request);
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected Kugou request"))
    }
}
fn search_track(id: Value, hash: &str) -> Value {
    json!({"Audioid":id,"MixSongID":6789,"FileHash":hash,"FileSize":1024,"HQFileHash":"high-hash","HQFileSize":2048,"SQFileHash":"flac-hash","SQFileSize":4096,"ResFileHash":"hires-hash","ResFileSize":8192,"OriSongName":"Title &amp; more","Suffix":"Live","Singers":[{"name":"Singer A"},{"name":"Singer B"}],"AlbumName":"Album","AlbumID":"album-string","Duration":125,"Grp":[]})
}
fn audio_info(id: Value, hash: &str) -> Value {
    json!({"songname":"Song","author_name":"Singer","album_info":{"album_name":"Album","album_id":456},"audio_info":{"audio_id":id,"audio_group_id":"group-id","hash":hash,"filesize":"1024","hash_320":"hq-hash","filesize_320":"2048","hash_flac":"flac-hash","filesize_flac":"4096","hash_high":"hires-hash","filesize_high":"8192","timelength":"125000"}})
}
fn run(mock: &Arc<Mock>, request: ProviderRequest) -> ProviderResponse {
    KgProvider::new(mock.clone())
        .execute(request, &ProviderContext::default())
        .unwrap()
}
fn request_body(mock: &Arc<Mock>, index: usize) -> Value {
    serde_json::from_slice(&mock.requests.lock().unwrap()[index].body).unwrap()
}

#[test]
fn kugou_signatures_match_independent_upstream_goldens() {
    assert_eq!(
        signature_params("appid=1005&b=2&a=1", false, r#"{"x":1}"#),
        "479053bc1c7c8c842b13930747453ef7"
    );
    assert_eq!(
        signature_params("appid=1005&b=2&a=1", true, r#"{"x":1}"#),
        "731b5a1ba3f261dfb7ced807dfc86643"
    );
}

#[test]
fn kugou_search_preserves_id_types_hashes_and_group_variants() {
    let mut item = search_track(json!(123), "base-hash");
    item["Grp"] = json!([search_track(json!("124"), "child-hash")]);
    let mock = Mock::with(vec![
        json!({"error_code":0,"data":{"total":77,"lists":[item.clone(),item]}}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Search {
            keyword: "中文 & A".into(),
            page: 2,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items.len(), 2);
    assert_eq!((page.total, page.page, page.limit), (77, 2, 30));
    let track = &page.items[0];
    assert_eq!(track.platform.song_id, json!(123));
    assert_eq!(page.items[1].platform.song_id, json!("124"));
    assert_eq!(track.metadata["hash"], "base-hash");
    assert_eq!(track.metadata["albumAudioId"], 6789);
    assert_eq!(
        track.metadata["_qualitys"]["flac24bit"]["hash"],
        "hires-hash"
    );
    assert_eq!(track.metadata["_qualitys"]["128k"]["size"], "1.00 KiB");
    assert_eq!(track.title, "Title & more Live");
    assert_eq!(track.duration_ms, Some(125000));
    let request = &mock.requests.lock().unwrap()[0];
    assert!(request.url.contains("keyword=%E4%B8%AD%E6%96%87%20%26%20A"));
    assert!(request.url.contains("platform=AndroidFilter"));
    assert!(request.url.ends_with("page=2"));
}

#[test]
fn kugou_detail_batches_hashes_and_uses_original_gateway_headers() {
    let mock = Mock::with(vec![
        json!({"error_code":0,"data":[[audio_info(json!("audio-string"),"h")]]}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Detail {
            ids: vec!["h".into()],
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items[0].platform.song_id, json!("audio-string"));
    assert_eq!(page.items[0].metadata["albumAudioId"], "group-id");
    assert_eq!(
        page.items[0].metadata["_qualitys"]["320k"]["hash"],
        "hq-hash"
    );
    let body = request_body(&mock, 0);
    assert_eq!(body["data"], json!([{"hash":"h"}]));
    assert_eq!(body["appid"], 1005);
    assert_eq!(body["key"], "OIlwieks28dk2k092lksi2UIkp");
    assert!(body["clienttime"].as_u64().unwrap() > 0);
    let requests = mock.requests.lock().unwrap();
    assert!(requests[0].url.ends_with("/v3/album_audio/audio"));
    assert_eq!(requests[0].headers["x-router"], "kmr.service.kugou.com");
    assert_eq!(requests[0].headers["KG-THash"], "13a3164");
}

#[test]
fn kugou_hot_search_static_charts_and_chart_tracks() {
    let chart = json!({"audio_id":123,"album_audio_id":678,"songname":"Chart song","remark":"Album","album_id":"aid","authors":[{"author_name":"Artist"}],"hash":"chart-hash","filesize":1024,"320filesize":2048,"320hash":"320-hash","sqfilesize":0,"sqhash":"","filesize_high":0,"hash_high":"","duration":60});
    let mock = Mock::with(vec![
        json!({"errcode":0,"data":{"list":[{"keywords":[{"keyword":"first"},{"keyword":"A&amp;B"}]}]}}),
        json!({"errcode":0,"data":{"total":501,"info":[chart]}}),
    ]);
    assert_eq!(
        run(&mock, ProviderRequest::HotSearch),
        ProviderResponse::Keywords(vec!["first".into(), "A&B".into()])
    );
    let ProviderResponse::Charts(charts) = run(&mock, ProviderRequest::Charts) else {
        panic!()
    };
    assert_eq!(charts.len(), 51);
    assert_eq!(charts[0].id, "kg__8888");
    assert_eq!(charts.last().unwrap().id, "kg__33161");
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::ChartTracks {
            id: "kg__8888".into(),
            page: 2,
            limit: 100,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.total, 501);
    assert_eq!(page.items[0].metadata["albumAudioId"], 678);
    assert_eq!(
        page.items[0].metadata["_qualitys"]["320k"]["hash"],
        "320-hash"
    );
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0].headers["x-router"], "msearch.kugou.com");
    assert!(requests[1].url.contains("rankid=8888"));
    assert!(requests[1].url.contains("page=2"));
}

#[test]
fn kugou_playlist_tags_search_and_catalog_metadata() {
    let p = json!({"specialid":456,"specialname":"Playlist","nickname":"Creator","imgurl":"https://image/{size}","playcount":123,"songcount":3,"intro":"Intro","grade":9});
    let mock = Mock::with(vec![
        json!({"status":1,"data":{"tagids":{"语言":{"data":[{"parent_id":1,"pname":"语言","id":2,"name":"国语"}]}},"hotTag":{"status":1,"data":{"a":{"special_id":3,"special_name":"热门"}}}}}),
        json!({"errcode":0,"data":{"total":13,"info":[p.clone()]}}),
        json!({"status":1,"special_db":[p]}),
        json!({"status":1,"data":{"params":{"pagesize":20,"p":2,"total":90}}}),
    ]);
    let ProviderResponse::PlaylistTags(tags) = run(&mock, ProviderRequest::PlaylistTags) else {
        panic!()
    };
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].group, "语言");
    assert_eq!(tags[1].extra["hot"], true);
    let ProviderResponse::Playlists(page) = run(
        &mock,
        ProviderRequest::SearchPlaylists {
            keyword: "A&B".into(),
            page: 2,
            limit: 20,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.total, 13);
    assert_eq!(page.items[0].id, "id_456");
    assert_eq!(page.items[0].play_count, Some(123));
    assert_eq!(page.items[0].extra["grade"], 9);
    let ProviderResponse::Playlists(page) = run(
        &mock,
        ProviderRequest::Playlists {
            tag: "2".into(),
            sort: "6".into(),
            page: 2,
            limit: 20,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.total, 90);
    let requests = mock.requests.lock().unwrap();
    assert!(requests[1].url.contains("keyword=A%26B"));
    assert!(requests[2].url.ends_with("t=6&c=2&p=2"));
    assert!(requests[3].url.ends_with("c=2"));
}

#[test]
fn kugou_special_html_detail_hydrates_v2_and_retains_professional_metadata() {
    let mock = Mock::default();
    mock.text(r#"<script>var global = { name: "Special", pic: "https://cover" }; global.data = [{"hash":"h"},{"hash":"h"}];</script><div class="pc_specail_text pc_singer_tab_content" id="specailIntroduceWrap">A&amp;B</div>"#);
    mock.json(json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}));
    let mock = Arc::new(mock);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "id_456".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Special");
    assert_eq!(c.info.description.as_deref(), Some("A&B"));
    assert_eq!(c.tracks.total, 1);
    assert_eq!(c.tracks.items[0].metadata["hash"], "h");
    assert_eq!(request_body(&mock, 1)["data"], json!([{"hash":"h"}]));
    let requests = mock.requests.lock().unwrap();
    assert!(requests[0].url.ends_with("/456-5-9999.html"));
    assert!(requests[1].url.ends_with("/v2/album_audio/audio"));
}

#[test]
fn kugou_global_share_detail_uses_web_signatures_and_jsonp() {
    let mock = Mock::default();
    mock.text(r#"callback({"errcode":0,"data":{"specialname":"Global","imgurl":"https://cover/{size}","nickname":"Author","intro":"Description","playcount":99,"songcount":1}});"#);
    mock.json(json!({"errcode":0,"data":{"info":[{"hash":"h"}]}}));
    mock.json(json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}));
    let mock = Arc::new(mock);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/share/?global_collection_id=global-id".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Global");
    assert_eq!(c.info.image_url.as_deref(), Some("https://cover/240"));
    assert_eq!(c.tracks.items.len(), 1);
    let requests = mock.requests.lock().unwrap();
    for request in &requests[..2] {
        let (_, query) = request.url.split_once('?').unwrap();
        let (params, sign) = query.rsplit_once("&signature=").unwrap();
        assert_eq!(sign, signature_params(params, true, ""));
        assert_eq!(
            request.headers["Referer"],
            "https://m3ws.kugou.com/share/index.php"
        );
    }
    assert!(requests[1].url.contains("pagesize=300"));
}

#[test]
fn kugou_numeric_code_and_chain_routes_are_active() {
    let mock = Mock::with(vec![
        json!({"errcode":0,"data":{"info":{"type":5,"name":"Code list","img":"https://code-cover","username":"User","count":1},"list":[{"hash":"h"}]}}),
        json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}),
        json!({"errcode":0,"data":{"info":{"name":"Chain list","img":"https://chain-cover","username":"User"},"list":[{"hash":"c"}]}}),
        json!({"error_code":0,"data":[[audio_info(json!(124),"c")]]}),
    ]);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "123456".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Code list");
    assert_eq!(request_body(&mock, 0)["data"], "123456");
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/share/?chain=abc".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Chain list");
    assert!(mock.requests.lock().unwrap()[2].url.contains("chain=abc"));
}

#[test]
fn kugou_gcid_decode_and_chain_collection_use_distinct_signing_keys() {
    let info = json!({"specialname":"Decoded","nickname":"User","songcount":0});
    let mock = Mock::with(vec![
        json!({"errcode":0,"data":{"list":[{"global_collection_id":"global-id"}]}}),
        json!({"errcode":0,"data":info}),
        json!({"errcode":0,"data":{"info":[{"name":"Linked","count":1,"pic":"https://cover/{size}","list_create_username":"User","intro":"Intro"}],"list":{"info":[{"hash":"h"}]}}}),
        json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}),
    ]);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "gcid_encoded".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Decoded");
    assert_eq!(c.tracks.total, 0);
    let request = mock.requests.lock().unwrap()[0].clone();
    let (_, query) = request.url.split_once('?').unwrap();
    let (params, sign) = query.rsplit_once("&signature=").unwrap();
    assert_eq!(
        sign,
        signature_params(params, false, std::str::from_utf8(&request.body).unwrap())
    );
    assert_eq!(request_body(&mock, 0)["data"][0]["id_type"], 2);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/share/?chain=abc&global_collection_id=global-id".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Linked");
    assert_eq!(c.info.image_url.as_deref(), Some("https://cover/240"));
    assert_eq!(c.tracks.items.len(), 1);
    let requests = mock.requests.lock().unwrap();
    let (_, query) = requests[2].url.split_once('?').unwrap();
    let (params, sign) = query.rsplit_once("&signature=").unwrap();
    assert_eq!(sign, signature_params(params, true, ""));
    assert!(params.contains("chain=abc"));
}

#[test]
fn kugou_chain_legacy_playlist_fallback_reproduces_infsign_cdn_parameters() {
    let mock = Mock::default();
    mock.json(json!({"errcode":0,"data":{"id":555}}));
    mock.text(r#"<script>var phpParam = {"specialname":"Legacy share","nickname":"User","imgurl":"https://cover/{size}"};</script>"#);
    mock.json(json!({"errcode":0,"data":{"info":[{"hash":"h"}]}}));
    mock.json(json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}));
    let mock = Arc::new(mock);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/share/?chain=legacy".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Legacy share");
    assert_eq!(c.tracks.items.len(), 1);
    let requests = mock.requests.lock().unwrap();
    let (_, query) = requests[2].url.split_once('?').unwrap();
    let (params, sign) = query.rsplit_once("&signature=").unwrap();
    assert_eq!(
        params,
        "srcappid=2919&clientver=20000&appid=1058&type=0&module=playlist&page=1&pagesize=100&specialid=555"
    );
    assert_eq!(sign, signature_params(params, true, ""));
    assert!(!params.contains("clienttime"));
}

#[test]
fn kugou_recommended_playlists_are_preprended_on_first_default_page() {
    let p = |id| json!({"specialid":id,"specialname":"Playlist","play_count":5});
    let mock = Mock::with(vec![
        json!({"status":1,"special_db":[p(1)]}),
        json!({"status":1,"data":{"params":{"total":10,"pagesize":20,"p":1}}}),
        json!({"status":1,"data":{"special_list":[p(2)]}}),
    ]);
    let ProviderResponse::Playlists(page) = run(
        &mock,
        ProviderRequest::Playlists {
            tag: "".into(),
            sort: "5".into(),
            page: 1,
            limit: 20,
        },
    ) else {
        panic!()
    };
    assert_eq!(
        page.items.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        vec!["id_2", "id_1"]
    );
    assert_eq!(request_body(&mock, 2)["return_max"], 15);
    assert_eq!(
        mock.requests.lock().unwrap()[2].headers["User-Agent"],
        "KuGou2012-8275-web_browser_event_handler"
    );
}

fn hash_rows(start: u64, end: u64) -> Vec<Value> {
    (start..end)
        .map(|n| json!({"hash":format!("h{n}")}))
        .collect()
}
fn audio_rows(start: u64, end: u64) -> Value {
    json!({"error_code":0,"data":(start..end).map(|n|json!([audio_info(json!(n),&format!("h{n}"))])).collect::<Vec<_>>()})
}

#[test]
fn kugou_global_share_final_page_keeps_fixed_server_page_size() {
    let mock = Mock::with(vec![
        json!({"errcode":0,"data":{"specialname":"350 tracks","songcount":350}}),
        json!({"errcode":0,"data":{"info":hash_rows(0,300)}}),
        json!({"errcode":0,"data":{"info":hash_rows(300,350)}}),
        audio_rows(0, 100),
        audio_rows(100, 200),
        audio_rows(200, 300),
        audio_rows(300, 350),
    ]);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/share/?global_collection_id=global-id".into(),
            page: 4,
            limit: 100,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.tracks.total, 350);
    assert_eq!(c.tracks.items.len(), 50);
    assert_eq!(c.tracks.items[0].platform.song_id, 300);
    assert_eq!(c.tracks.items.last().unwrap().platform.song_id, 349);
    let requests = mock.requests.lock().unwrap();
    assert!(requests[1].url.contains("page=1&pagesize=300"));
    assert!(requests[2].url.contains("page=2&pagesize=300"));
}

#[test]
fn kugou_legacy_chain_pagination_fetches_all_blocks_before_requested_slice() {
    let mock = Mock::default();
    mock.json(json!({"errcode":0,"data":{"id":555}}));
    mock.text(r#"var phpParam = {"specialname":"101 tracks","songcount":101};"#);
    mock.json(json!({"errcode":0,"data":{"info":hash_rows(0,100)}}));
    mock.json(json!({"errcode":0,"data":{"info":hash_rows(100,101)}}));
    mock.json(audio_rows(0, 100));
    mock.json(audio_rows(100, 101));
    let mock = Arc::new(mock);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/share/?chain=legacy".into(),
            page: 2,
            limit: 100,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.tracks.total, 101);
    assert_eq!(c.tracks.items.len(), 1);
    assert_eq!(c.tracks.items[0].platform.song_id, 100);
    let requests = mock.requests.lock().unwrap();
    assert!(requests[0].url.contains("page=1"));
    assert!(requests[2].url.contains("page=1&pagesize=100"));
    assert!(requests[3].url.contains("page=2&pagesize=100"));
}

#[test]
fn kugou_zlist_final_page_keeps_fixed_size_and_removes_duplicate_pagination_query() {
    let mock = Mock::with(vec![
        json!({"errcode":0,"info":[{"name":"95 tracks","count":95,"pic":"https://image/{size}"}],"list":{"info":[]}}),
        json!({"errcode":0,"data":{"list":{"info":hash_rows(0,90)}}}),
        json!({"errcode":0,"data":{"list":{"info":hash_rows(90,95)}}}),
        audio_rows(0, 95),
    ]);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/zlist.html?foo=1&pagesize=30&page=1".into(),
            page: 2,
            limit: 90,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.tracks.total, 95);
    assert_eq!(c.tracks.items.len(), 5);
    assert_eq!(c.tracks.items[0].platform.song_id, 90);
    let requests = mock.requests.lock().unwrap();
    assert!(
        requests[0]
            .url
            .starts_with("https://m3ws.kugou.com/zlist/list?")
    );
    assert!(requests[1].url.ends_with("pagesize=90&page=1"));
    assert!(requests[2].url.ends_with("pagesize=90&page=2"));
    for r in &requests[..3] {
        assert_eq!(r.url.matches("pagesize=").count(), 1);
        assert_eq!(r.url.matches("page=").count(), 1);
    }
}

#[test]
fn kugou_legacy_chain_invalid_response_shape_uses_pc_share_fallback() {
    let mock = Mock::default();
    mock.json(json!({"errcode":0,"data":{"id":555}}));
    mock.text(r#"var phpParam = {"specialname":"PC fallback"};"#);
    mock.json(json!({"errcode":0,"data":{"info":"invalid"}}));
    mock.text(r#"var dataFromSmarty = [{"hash":"h"}];"#);
    mock.json(json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}));
    let mock = Arc::new(mock);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://m.kugou.com/share/?chain=legacy".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "PC fallback");
    assert_eq!(c.tracks.items.len(), 1);
    assert_eq!(
        mock.requests.lock().unwrap()[3].url,
        "http://www.kugou.com/share/legacy.html"
    );
}

#[test]
fn kugou_normal_hot_and_reply_comments_preserve_mentions_and_parent_thread() {
    let c = json!({"id":123,"content":"hi [at=7]&amp; bye","atlist":[{"id":7,"name":"Friend"}],"addtime":"2023-11-15 06:13:20","user_name":"Child","user_pic":"https://avatar","user_id":9,"like":{"likenum":8},"reply_num":3,"images":[{"url":"https://pic"}],"pcontent":"Parent &amp; text","puser":"ParentUser","puser_id":10});
    let mock = Mock::with(vec![
        json!({"err_code":0,"count":20,"list":[c.clone()]}),
        json!({"err_code":0,"count":1,"list":[c.clone()]}),
        json!({"err_code":0,"list":[c]}),
    ]);
    let req = |kind| ProviderRequest::Comments {
        id: "base-hash".into(),
        kind,
        page: 2,
        limit: 20,
        cursor: None,
        root_comment_id: Some("root-1".into()),
    };
    let ProviderResponse::Comments(page) = run(&mock, req(CommentKind::Normal)) else {
        panic!()
    };
    assert_eq!(page.items[0].text, "Parent & text");
    assert_eq!(page.items[0].replies[0].text, "hi @Friend & bye");
    assert_eq!(
        page.items[0].replies[0].extra["images"][0]["url"],
        "https://pic"
    );
    assert_eq!(page.items[0].replies[0].likes, 8);
    assert_eq!(page.items[0].replies[0].timestamp_ms, Some(1700000000000));
    run(&mock, req(CommentKind::Hot));
    run(&mock, req(CommentKind::Replies));
    let requests = mock.requests.lock().unwrap();
    for request in &requests[..2] {
        let (_, query) = request.url.split_once('?').unwrap();
        let (params, sign) = query.rsplit_once("&signature=").unwrap();
        assert_eq!(sign, signature_params(params, false, ""));
    }
    assert!(requests[0].url.contains("/newest?"));
    assert!(requests[1].url.contains("/topliked?"));
    assert!(requests[2].url.contains("childrenid=base-hash&tid=root-1"));
}

fn krc_fixture() -> String {
    let language=STANDARD.encode(serde_json::to_vec(&json!({"version":1,"content":[{"type":0,"lyricContent":[["ro","ma"],["end"]]},{"type":1,"lyricContent":[["翻译"],["结束"]]}]})).unwrap());
    let text = format!(
        "[language:{language}]\n[1000,700]<0,100,0>Hi<100,300,0> 世界\n[2000,500]<0,500,0>End"
    );
    let mut writer = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    writer.write_all(text.as_bytes()).unwrap();
    let compressed = writer.finish().unwrap();
    let key = [
        0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e,
        0x69,
    ];
    let mut bytes = b"krc1".to_vec();
    bytes.extend(compressed.iter().enumerate().map(|(i, b)| b ^ key[i % 16]));
    STANDARD.encode(bytes)
}

#[test]
fn kugou_network_lyric_search_download_and_native_krc_four_tracks() {
    let mock = Mock::with(vec![
        json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}),
        json!({"candidates":[{"id":"lyric-id","accesskey":"secret-key","krctype":1,"contenttype":0}]}),
        json!({"fmt":"krc","content":krc_fixture()}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Detail {
            ids: vec!["h".into()],
        },
    ) else {
        panic!()
    };
    let ProviderResponse::Lyrics(l) = run(
        &mock,
        ProviderRequest::Lyrics {
            track: page.items[0].clone(),
            word_by_word: true,
        },
    ) else {
        panic!()
    };
    assert_eq!(l.lyric, "[00:01.000]Hi 世界\n[00:02.000]End");
    assert_eq!(
        l.translated.as_deref(),
        Some("[00:01.000]翻译\n[00:02.000]结束")
    );
    assert_eq!(
        l.romanized.as_deref(),
        Some("[00:01.000]roma\n[00:02.000]end")
    );
    assert_eq!(
        l.word_by_word.as_deref(),
        Some("[00:01.000]<0,100>Hi<100,300> 世界\n[00:02.000]<0,500>End")
    );
    let requests = mock.requests.lock().unwrap();
    assert!(requests[1].url.contains("timelength=125"));
    assert!(
        requests[2]
            .url
            .contains("id=lyric-id&accesskey=secret-key&fmt=krc")
    );
    assert_eq!(
        requests[2].headers["KG-THash"],
        "expand_search_manager.cpp:852736169:451"
    );
}

#[test]
fn kugou_network_lrc_and_picture_privilege_route() {
    let mock = Mock::with(vec![
        json!({"error_code":0,"data":[[audio_info(json!(123),"h")]]}),
        json!({"candidates":[{"id":5,"accesskey":"k","krctype":0,"contenttype":1}]}),
        json!({"fmt":"lrc","content":STANDARD.encode("[00:01.00]Line")}),
        json!({"error_code":0,"data":[{"info":{"image":"https://image/{size}","imgsize":[240,500]}}]}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Detail {
            ids: vec!["h".into()],
        },
    ) else {
        panic!()
    };
    let track = page.items[0].clone();
    let ProviderResponse::Lyrics(l) = run(
        &mock,
        ProviderRequest::Lyrics {
            track: track.clone(),
            word_by_word: false,
        },
    ) else {
        panic!()
    };
    assert_eq!(l.lyric, "[00:01.00]Line");
    assert_eq!(l.word_by_word, None);
    assert_eq!(
        run(&mock, ProviderRequest::Picture { track }),
        ProviderResponse::Picture("https://image/240".into())
    );
    let body = request_body(&mock, 3);
    assert_eq!(body["resource"][0]["album_audio_id"], 123);
    assert_eq!(body["resource"][0]["hash"], "h");
    assert_eq!(body["resource"][0]["album_id"], 456);
}

#[test]
fn kugou_failures_cancellation_and_inactive_album_are_explicit() {
    let mock = Mock::with(vec![json!({"error_code":-1,"data":{"lists":[]}})]);
    assert!(matches!(
        KgProvider::new(mock.clone()).execute(
            ProviderRequest::Search {
                keyword: "x".into(),
                page: 1,
                limit: 30
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Upstream { .. })
    ));
    let ctx = ProviderContext::default();
    ctx.cancel.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        KgProvider::new(mock.clone()).execute(ProviderRequest::HotSearch, &ctx),
        Err(ProviderError::Cancelled)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    assert!(matches!(
        KgProvider::new(mock).execute(
            ProviderRequest::Album {
                id: "a".into(),
                page: 1,
                limit: 30
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Unsupported { .. })
    ));
}

#[test]
fn removed_hash_placeholder_does_not_discard_available_playlist_metadata() {
    let mock = Mock::with(vec![
        json!({"status":1,"error_code":0,"data":[audio_info(json!(12),"a"),{},null,[audio_info(json!(13),"b")]]}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Detail {
            ids: vec!["a".into(), "removed".into(), "b".into()],
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].platform.song_id, 12);
    assert_eq!(page.items[1].platform.song_id, 13);
}
#[test]
fn all_removed_hashes_and_nonempty_malformed_rows_are_explicit_failures() {
    let mock = Mock::with(vec![json!({"status":1,"error_code":0,"data":[{},null]})]);
    assert!(matches!(
        KgProvider::new(mock).execute(
            ProviderRequest::Detail {
                ids: vec!["removed".into()]
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Upstream { .. })
    ));
    let mock = Mock::with(vec![
        json!({"status":1,"error_code":0,"data":[{"unexpected":"shape"}]}),
    ]);
    assert!(matches!(
        KgProvider::new(mock).execute(
            ProviderRequest::Detail {
                ids: vec!["bad".into()]
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Parse { .. })
    ));
}
