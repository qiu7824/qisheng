use lx_gpui_player::platforms::{
    tx::{TxProvider, zzc_sign},
    *,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
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
            .expect("unexpected QQ request"))
    }
}
fn raw_track() -> Value {
    json!({"id":123456,"mid":"qq-song-mid","title":"Song &amp; title","interval":125,"album":{"mid":"album-mid","name":"Album"},"singer":[{"name":"Singer A","mid":"singer-mid"},{"name":"Singer B","mid":"singer-b"}],"file":{"media_mid":"media-mid","size_128mp3":1024,"size_320mp3":4096,"size_flac":8192,"size_hires":16384}})
}
fn run(mock: &Arc<Mock>, request: ProviderRequest) -> ProviderResponse {
    TxProvider::new(mock.clone())
        .execute(request, &ProviderContext::default())
        .unwrap()
}
fn request_body(mock: &Arc<Mock>, index: usize) -> Value {
    serde_json::from_slice(&mock.requests.lock().unwrap()[index].body).unwrap()
}

#[test]
fn qq_search_signature_matches_the_upstream_golden() {
    assert_eq!(
        zzc_sign(r#"{"hello":"world"}"#),
        "zzcfd289f8e29duxr4kcugtolk6gscjssr9v0924a8bae"
    );
}

#[test]
fn qq_search_signed_desktop_request_retains_all_identifiers_and_qualities() {
    let mock = Mock::with(vec![
        json!({"code":0,"music.search.SearchCgiService":{"code":0,"data":{"body":{"song":{"list":[raw_track(),{"file":{}}]}},"meta":{"sum":73}}}}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Search {
            keyword: "周杰伦 & song".into(),
            page: 2,
            limit: 50,
        },
    ) else {
        panic!()
    };
    assert_eq!(
        (page.total, page.page, page.limit, page.items.len()),
        (73, 2, 50, 1)
    );
    let track = &page.items[0];
    assert_eq!(track.platform.song_id, json!("qq-song-mid"));
    assert_eq!(track.metadata["id"], json!(123456));
    assert_eq!(track.metadata["strMediaMid"], "media-mid");
    assert_eq!(track.metadata["albumMid"], "album-mid");
    assert_eq!(
        track.platform.qualities,
        vec!["128k", "320k", "flac", "flac24bit"]
    );
    assert_eq!(track.duration_ms, Some(125000));
    assert_eq!(track.title, "Song &amp; title");
    assert_eq!(track.metadata["_qualitys"]["128k"]["size"], "1.00 KiB");
    let body = request_body(&mock, 0);
    let param = &body["music.search.SearchCgiService"]["param"];
    assert_eq!(param["page_num"], 2);
    assert_eq!(param["query"], "周杰伦 & song");
    assert_eq!(param["searchid"].as_str().unwrap().len(), 37);
    assert!(
        param["searchid"]
            .as_str()
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    );
    assert_eq!(body["comm"]["guid"], "1F70E520B2EAA7D25E11760783C53CA9");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[0].headers["User-Agent"],
        "QQMusic 14090508(android 12)"
    );
    assert_eq!(
        requests[0].url,
        format!(
            "https://u.y.qq.com/cgi-bin/musics.fcg?sign={}",
            zzc_sign(std::str::from_utf8(&requests[0].body).unwrap())
        )
    );
}

#[test]
fn qq_detail_hot_search_and_static_chart_catalog() {
    let mock = Mock::with(vec![
        json!({"code":0,"req":{"code":0,"data":{"track_info":raw_track()}}}),
        json!({"code":0,"hotkey":{"data":{"vec_hotkey":[{"query":"first"},{"query":"second"}]}}}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Detail {
            ids: vec!["qq-song-mid".into()],
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items[0].metadata["id"], 123456);
    assert_eq!(
        request_body(&mock, 0)["req"]["param"]["song_mid"],
        "qq-song-mid"
    );
    assert_eq!(
        run(&mock, ProviderRequest::HotSearch),
        ProviderResponse::Keywords(vec!["first".into(), "second".into()])
    );
    assert_eq!(
        request_body(&mock, 1)["hotkey"]["method"],
        "GetHotkeyForQQMusicPC"
    );
    let ProviderResponse::Charts(charts) = run(&mock, ProviderRequest::Charts) else {
        panic!()
    };
    assert_eq!(charts.len(), 25);
    assert_eq!(charts[0].id, "tx__4");
    assert_eq!(charts.last().unwrap().id, "tx__131");
    assert_eq!(mock.requests.lock().unwrap().len(), 2);
}

#[test]
fn qq_chart_period_route_and_pagination() {
    let mock = Mock::default();
    mock.text(r#"<i class="play_cover__btn c_tx_link js_icon_play" data-listkey="k" data-listname="流行指数榜" data-tid="top/4" data-date="2026-10-06" data-any="1"></i>"#);
    mock.json(
        json!({"code":0,"toplist":{"code":0,"data":{"songInfoList":[raw_track(),raw_track()]}}}),
    );
    let mock = Arc::new(mock);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::ChartTracks {
            id: "tx__4".into(),
            page: 2,
            limit: 1,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.total, 2);
    let body = request_body(&mock, 1);
    assert_eq!(
        body["toplist"]["param"],
        json!({"topid":4,"num":300,"period":"2026-10-06"})
    );
}

#[test]
fn qq_playlist_tags_include_categories_and_html_hot_tags() {
    let mock = Mock::with(vec![
        json!({"code":0,"tags":{"data":{"v_group":[{"group_name":"语言","group_id":1,"v_item":[{"id":2,"name":"国语"}]}]}}}),
    ]);
    mock.text(r#"<a class="c_bg_link js_tag_item" data-id="3">轻&amp;柔</a>"#);
    let ProviderResponse::PlaylistTags(tags) = run(&mock, ProviderRequest::PlaylistTags) else {
        panic!()
    };
    assert_eq!(tags.len(), 2);
    assert_eq!((tags[0].id.as_str(), tags[0].group.as_str()), ("2", "语言"));
    assert_eq!(tags[1].name, "轻&柔");
    assert_eq!(tags[1].extra["hot"], true);
    let requests = mock.requests.lock().unwrap();
    assert!(
        requests[0]
            .url
            .contains("playlist.PlaylistAllCategoriesServer")
    );
    assert!(requests[1].url.ends_with("category_playlist.html"));
}

#[test]
fn qq_playlist_recommendation_category_and_search_maps() {
    let basic = json!({"tid":777,"title":"A &amp; B","play_cnt":123456,"creator":{"nick":"Creator"},"cover":{"medium_url":"https://cover","default_url":"https://fallback"},"desc":"one&lt;br&gt;two","song_cnt":8});
    let mock = Mock::with(vec![
        json!({"code":0,"playlist":{"data":{"FeedRsp":{"List":[{"Playlist":{"basic":basic.clone()}}],"FromLimit":100}}}}),
        json!({"code":0,"playlist":{"data":{"content":{"v_item":[{"basic":basic}],"total_cnt":19}}}}),
        json!({"code":0,"data":{"sum":13,"list":[{"dissid":999,"dissname":"S &amp; T","creator":{"name":"Author"},"imgurl":"https://cover","listennum":12,"introduction":"A&lt;br&gt;B","song_count":2}]}}),
    ]);
    let ProviderResponse::Playlists(page) = run(
        &mock,
        ProviderRequest::Playlists {
            tag: "".into(),
            sort: "-1".into(),
            page: 2,
            limit: 36,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items[0].id, "777");
    assert_eq!(page.items[0].description.as_deref(), Some("one\ntwo"));
    assert_eq!(page.items[0].track_count, Some(8));
    let ProviderResponse::Playlists(page) = run(
        &mock,
        ProviderRequest::Playlists {
            tag: "12".into(),
            sort: "3".into(),
            page: 3,
            limit: 36,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.total, 19);
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
    assert_eq!(page.items[0].id, "999");
    assert_eq!(page.items[0].name, "S & T");
    assert_eq!(page.items[0].author.as_deref(), Some("Author"));
    let requests = mock.requests.lock().unwrap();
    assert!(requests[0].url.contains("GetRecommendWhole"));
    assert!(requests[1].url.contains("get_category_content"));
    assert!(requests[2].url.contains("page_no=1"));
    assert!(requests[2].url.contains("query=A%26B"));
}

#[test]
fn qq_playlist_primary_and_fallback_preserve_collection_info() {
    let mock = Mock::with(vec![
        json!({"code":0,"subcode":0,"cdlist":[{"dissname":"Primary","logo":"https://cover","desc":"a<br>b","nickname":"Author","visitnum":12,"songlist":[raw_track(),raw_track()]}]}),
        json!({"code":0,"subcode":1}),
        json!({"code":0,"req_1":{"code":0,"data":{"total_song_num":1,"songlist":[raw_track()],"dirinfo":{"title":"Fallback","picurl":"https://cover2","desc":"d","host_nick":"Author2","listennum":99}}}}),
    ]);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "https://y.qq.com/n/ryqq/playlist/777".into(),
            page: 2,
            limit: 1,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Primary");
    assert_eq!(c.tracks.total, 2);
    assert_eq!(c.tracks.items.len(), 1);
    let ProviderResponse::Collection(c) = run(
        &mock,
        ProviderRequest::PlaylistDetail {
            id: "888".into(),
            page: 1,
            limit: 30,
        },
    ) else {
        panic!()
    };
    assert_eq!(c.info.name, "Fallback");
    assert_eq!(c.info.play_count, Some(99));
    assert_eq!(request_body(&mock, 2)["req_1"]["param"]["song_num"], 100000);
}

#[test]
fn qq_normal_and_hot_comments_include_embedded_replies() {
    let mock = Mock::with(vec![
        json!({"code":0,"comment":{"commenttotal":30,"commentlist":[{"rootcommentid":"root","commentid":"child","rootcommentcontent":"hello[em]e400822[/em]\\nline","rootcommentnick":"@Root","avatarurl":"https://child-avatar","praisenum":4,"time":1700000000,"middlecommentcontent":[{"subcommentid":"child","subcommentcontent":"reply","replynick":"@Child"}]}]}}),
        json!({"code":0,"req":{"code":0,"data":{"CommentList":{"Total":1,"Comments":[{"SeqNo":"s","CmId":"c","Content":"hot","PubTime":1700000000,"Nick":"HotUser","PraiseNum":9,"Pic":"https://photo","SubComments":[{"SeqNo":"s2","CmId":"c2","Content":"sub","Nick":"Sub","PraiseNum":2}]}]}}}}),
    ]);
    let request = |kind| ProviderRequest::Comments {
        id: "123456".into(),
        kind,
        page: 2,
        limit: 20,
        cursor: None,
        root_comment_id: None,
    };
    let ProviderResponse::Comments(page) = run(&mock, request(CommentKind::Normal)) else {
        panic!()
    };
    let c = &page.items[0];
    assert_eq!(c.text, "hello😀\nline");
    assert_eq!(c.author, "Root");
    assert_eq!(c.timestamp_ms, None);
    assert_eq!(c.replies[0].author, "Child");
    assert_eq!(
        c.replies[0].avatar_url.as_deref(),
        Some("https://child-avatar")
    );
    assert_eq!(c.replies[0].timestamp_ms, Some(1700000000000));
    let r = mock.requests.lock().unwrap()[0].clone();
    assert_eq!(
        r.headers["Content-Type"],
        "application/x-www-form-urlencoded"
    );
    assert!(String::from_utf8(r.body).unwrap().contains("topid=123456"));
    let ProviderResponse::Comments(page) = run(&mock, request(CommentKind::Hot)) else {
        panic!()
    };
    assert_eq!(page.items[0].replies[0].id, "sub_s2_c2");
    assert_eq!(page.items[0].extra["Pic"], "https://photo");
    assert_eq!(request_body(&mock, 1)["req"]["param"]["PageNum"], 1);
}

#[test]
fn qq_picture_and_inactive_routes_are_explicit() {
    let mock = Mock::with(vec![
        json!({"code":0,"req":{"code":0,"data":{"track_info":raw_track()}}}),
    ]);
    let ProviderResponse::Tracks(page) = run(
        &mock,
        ProviderRequest::Detail {
            ids: vec!["qq-song-mid".into()],
        },
    ) else {
        panic!()
    };
    assert_eq!(
        run(
            &mock,
            ProviderRequest::Picture {
                track: page.items[0].clone()
            }
        ),
        ProviderResponse::Picture(
            "https://y.gtimg.cn/music/photo_new/T002R500x500M000album-mid.jpg".into()
        )
    );
    assert!(matches!(
        TxProvider::new(mock).execute(
            ProviderRequest::Album {
                id: "album".into(),
                page: 1,
                limit: 30
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Unsupported { .. })
    ));
}

#[test]
fn qq_network_lyrics_use_numeric_id_and_decode_official_qrc_golden() {
    let hex = "ee7befb746f93831d42dcdf2e21f0d5dd9fd95a45509a6454e1d4966f9a90d4615e209ab93b120810ad3921741fd44cfa5a0163825ba57fe79858345e1b548ac7c52daf726a03c69c2576b2ac384029bf2e00bbc52b89c8ba0215bccd3cc44f1c98d4aaf555461d7";
    let mock = Mock::with(vec![
        json!({"code":0,"req":{"code":0,"data":{"track_info":raw_track()}}}),
        json!({"code":0,"req":{"code":0,"data":{"lyric":hex,"trans":"","roma":hex}}}),
    ]);
    let ProviderResponse::Tracks(tracks) = run(
        &mock,
        ProviderRequest::Detail {
            ids: vec!["qq-song-mid".into()],
        },
    ) else {
        panic!()
    };
    let ProviderResponse::Lyrics(lyrics) = run(
        &mock,
        ProviderRequest::Lyrics {
            track: tracks.items[0].clone(),
            word_by_word: true,
        },
    ) else {
        panic!()
    };
    assert_eq!(lyrics.lyric, "[00:01.000]Hi 世界\n[00:02.000]End");
    assert_eq!(
        lyrics.word_by_word.as_deref(),
        Some("[00:01.000]<0,100>Hi<100,300> 世界\n[00:02.000]<0,500>End")
    );
    assert_eq!(
        lyrics.romanized.as_deref(),
        Some("[00:01.000]Hi 世界\n[00:02.000]End")
    );
    assert_eq!(request_body(&mock, 1)["req"]["param"]["songID"], 123456);
    assert_eq!(request_body(&mock, 1)["req"]["param"]["crypt"], 1);
}

#[test]
fn qq_upstream_error_and_context_cancellation_do_not_emit_partial_data() {
    let mock = Mock::with(vec![json!({"code":-1,"req":{"code":-1}})]);
    assert!(matches!(
        TxProvider::new(mock.clone()).execute(
            ProviderRequest::Detail {
                ids: vec!["mid".into()]
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Upstream { .. })
    ));
    let ctx = ProviderContext::default();
    ctx.cancel.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        TxProvider::new(mock.clone()).execute(ProviderRequest::HotSearch, &ctx),
        Err(ProviderError::Cancelled)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}
