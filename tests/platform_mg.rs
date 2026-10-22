use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    platforms::{
        mg::{DEVICE_ID, MgProvider, signature},
        *,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, atomic::Ordering},
};

#[derive(Default)]
struct Fixture {
    replies: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}
impl Fixture {
    fn with(values: Vec<Value>) -> Arc<Self> {
        Self::responses(
            values
                .into_iter()
                .map(|v| HttpResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    bytes: v.to_string().into_bytes(),
                })
                .collect(),
        )
    }
    fn responses(replies: Vec<HttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}
impl HttpTransport for Fixture {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        self.requests.lock().unwrap().push(request);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::parse("fixture exhausted"))
    }
}
fn song(id: &str) -> Value {
    json!({"songId":id,"copyrightId":format!("copy-{id}"),"songName":"歌名","name":"歌名","singerList":[{"name":"歌手"}],"artists":[{"name":"歌手"}],"album":"专辑","albumId":"a1","duration":61,"length":"00:01:01","img3":"/cover.jpg","albumImgs":[{"img":"https://p.example/cover.jpg"}],"lrcUrl":"https://l.example/lrc","mrcUrl":"https://l.example/mrc","trcUrl":"https://l.example/trc","audioFormats":[{"formatType":"PQ","asize":1024},{"formatType":"HQ","isize":2048},{"formatType":"SQ","size":4096},{"formatType":"ZQ24","size":8192}],"newRateFormats":[{"formatType":"PQ","size":1024},{"formatType":"HQ","androidSize":2048},{"formatType":"SQ","size":4096},{"formatType":"ZQ","size":8192}]})
}
fn lyric_track(mrc: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"name":"歌曲","singer":"歌手","source":"mg","songmid":"s1","copyrightId":"c1","lrcUrl":"https://l.example/plain","mrcUrl":mrc,"trcUrl":"https://l.example/translation"}),LxMusicDtoVersion::Legacy).unwrap()
}
fn raw(text: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: BTreeMap::new(),
        bytes: text.as_bytes().into(),
    }
}
#[test]
fn signed_search_normalizes_copyright_quality_and_lyric_urls() {
    assert_eq!(
        signature("1700000000000", "周杰伦"),
        "32a64a14bc877d1521fae7854288cc94"
    );
    let f = Fixture::with(vec![
        json!({"code":"000000","songResultData":{"totalCount":"17","resultList":[[song("s1"),song("s1")],[{"songId":"invalid"}]]}}),
        json!({"code":"000000","data":{"hotwords":[{"hotwordList":[{"resourceType":"song","word":"热歌"},{"resourceType":"singer","word":"歌手"}]}]}}),
    ]);
    let p = MgProvider::new(f.clone());
    let c = ProviderContext::default();
    let ProviderResponse::Tracks(page) = p
        .execute(
            ProviderRequest::Search {
                keyword: "周杰伦 & song".into(),
                page: 2,
                limit: 20,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!((page.total, page.page, page.limit), (17, 2, 20));
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].platform.song_id, "s1");
    assert_eq!(page.items[0].metadata["copyrightId"], "copy-s1");
    assert_eq!(page.items[0].metadata["lrcUrl"], "https://l.example/lrc");
    assert_eq!(page.items[0].metadata["mrcUrl"], "https://l.example/mrc");
    assert_eq!(page.items[0].metadata["trcUrl"], "https://l.example/trc");
    assert_eq!(
        page.items[0].platform.qualities,
        vec!["128k", "320k", "flac", "flac24bit"]
    );
    assert_eq!(
        page.items[0].metadata["picUrl"],
        "http://d.musicapp.migu.cn/cover.jpg"
    );
    assert_eq!(page.items[0].duration_ms, Some(61000));
    let r = &f.requests()[0];
    assert!(
        r.url
            .starts_with("https://jadeite.migu.cn/music_search/v3/search/searchAll?isCorrect=0")
    );
    assert!(
        r.url
            .contains("text=%E5%91%A8%E6%9D%B0%E4%BC%A6%20%26%20song")
    );
    assert_eq!(r.headers["deviceId"], DEVICE_ID);
    assert_eq!(
        r.headers["sign"],
        signature(&r.headers["timestamp"], "周杰伦 & song")
    );
    assert_eq!(r.headers["channel"], "0146921");
    assert_eq!(
        p.execute(ProviderRequest::HotSearch, &c).unwrap(),
        ProviderResponse::Keywords(vec!["热歌".into()])
    );
    assert_eq!(
        f.requests()[1].url,
        "http://jadeite.migu.cn:7090/music_search/v3/search/hotword"
    );
}
#[test]
fn details_batch_chart_paging_and_picture() {
    let f = Fixture::with(vec![
        json!({"returnCode":"000000","resource":[song("s1")]}),
        json!({"code":"000000","columnInfo":{"contents":[{"objectInfo":song("s1")},{"objectInfo":song("s2")}]}}),
        json!({"code":"000000","resource":[song("s1")]}),
    ]);
    let p = MgProvider::new(f.clone());
    let c = ProviderContext::default();
    let ProviderResponse::Tracks(detail) = p
        .execute(
            ProviderRequest::Detail {
                ids: vec!["copy-s1".into(), "copy-s2".into()],
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(detail.items[0].album, "专辑");
    assert_eq!(
        detail.items[0].metadata["_qualitys"]["128k"]["size"],
        "1.00 KiB"
    );
    assert_eq!(f.requests()[0].method, "POST");
    assert_eq!(
        String::from_utf8(f.requests()[0].body.clone()).unwrap(),
        "resourceId=copy-s1%7Ccopy-s2"
    );
    let ProviderResponse::Charts(charts) = p.execute(ProviderRequest::Charts, &c).unwrap() else {
        panic!()
    };
    assert_eq!(charts.len(), 10);
    assert_eq!(charts[0].id, "mg__27553319");
    let ProviderResponse::Tracks(chart) = p
        .execute(
            ProviderRequest::ChartTracks {
                id: "mg__27553319".into(),
                page: 2,
                limit: 1,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(chart.total, 2);
    assert_eq!(chart.items[0].platform.song_id, "s2");
    assert_eq!(
        f.requests()[1].url,
        "https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/querycontentbyId.do?columnId=27553319&needAll=0"
    );
    assert_eq!(
        p.execute(
            ProviderRequest::Picture {
                track: lyric_track("")
            },
            &c
        )
        .unwrap(),
        ProviderResponse::Picture("https://p.example/cover.jpg".into())
    );
}
#[test]
fn resource_details_split_at_one_hundred() {
    let f = Fixture::with(vec![
        json!({"code":"000000","resource":[song("s1")]}),
        json!({"code":"000000","resource":[song("s101")]}),
    ]);
    let p = MgProvider::new(f.clone());
    let ids = (1..=101).map(|i| format!("c{i}")).collect();
    let ProviderResponse::Tracks(page) = p
        .execute(ProviderRequest::Detail { ids }, &ProviderContext::default())
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(page.items.len(), 2);
    assert_eq!(f.requests().len(), 2);
    assert_eq!(f.requests()[1].body, b"resourceId=c101");
}
#[test]
fn playlist_tags_tree_cards_search_and_collection() {
    let f = Fixture::with(vec![
        json!({"code":"000000","data":[{"content":[{"texts":["热门标签","t1"]}]},{"header":{"title":"风格"},"content":[{"texts":["流行","t2"]}]}]}),
        json!({"code":"000000","data":{"contents":[{"contents":[{"resType":"2021","resId":"p1","txt":"列表1","txt2":"描述","img":"cover"},{"resType":"2021","resId":"p1","txt":"重复"}]}]}}),
        json!({"code":"000000","data":{"contentItemList":[{}, {"itemList":[{"logEvent":{"contentId":"p2"},"title":"列表2","imageUrl":"cover","barList":[{"title":"40"}]}]}]}}),
        json!({"code":"000000","songListResultData":{"totalCount":"6","result":[{"id":"p3","name":"搜索列表","musicListPicUrl":"cover","userName":"创建者","playNum":"22","musicNum":4}]}}),
        json!({"code":"000000","data":{"songList":[song("s1")],"totalCount":4}}),
        json!({"code":"000000","data":{"title":"正式列表","imgItem":{"img":"cover"},"summary":"描述","ownerName":"创建者","opNumItem":{"playNum":32}}}),
    ]);
    let p = MgProvider::new(f.clone());
    let c = ProviderContext::default();
    let ProviderResponse::PlaylistTags(tags) =
        p.execute(ProviderRequest::PlaylistTags, &c).unwrap()
    else {
        panic!()
    };
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].group, "热门");
    assert_eq!(tags[1].group, "风格");
    let ProviderResponse::Playlists(tree) = p
        .execute(
            ProviderRequest::Playlists {
                tag: String::new(),
                sort: "15127315".into(),
                page: 2,
                limit: 30,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(tree.items.len(), 1);
    assert_eq!(tree.items[0].id, "p1");
    assert!(
        f.requests()[1]
            .url
            .contains("playlist-square-recommend/v1.0?templateVersion=2&pageNo=2")
    );
    let ProviderResponse::Playlists(cards) = p
        .execute(
            ProviderRequest::Playlists {
                tag: "t2".into(),
                sort: String::new(),
                page: 1,
                limit: 30,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(cards.items[0].id, "p2");
    assert_eq!(cards.items[0].play_count, Some(40));
    assert!(f.requests()[2].url.contains("tagId=t2"));
    let ProviderResponse::Playlists(search) = p
        .execute(
            ProviderRequest::SearchPlaylists {
                keyword: "歌单".into(),
                page: 2,
                limit: 20,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(search.total, 6);
    assert_eq!(search.items[0].author.as_deref(), Some("创建者"));
    assert!(f.requests()[3].url.contains("isCorrect=1"));
    assert!(f.requests()[3].url.contains("%22songlist%22%3A1"));
    let ProviderResponse::Collection(col) = p
        .execute(
            ProviderRequest::PlaylistDetail {
                id: "https://music.migu.cn/v5/#/playlist?playlistId=221573417".into(),
                page: 2,
                limit: 50,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(col.info.name, "正式列表");
    assert_eq!(col.tracks.total, 4);
    assert_eq!(col.tracks.page, 2);
    assert_eq!(col.tracks.items[0].platform.song_id, "s1");
    assert!(
        f.requests()[4]
            .url
            .contains("pageNo=2&pageSize=50&playlistId=221573417")
    );
}
#[test]
fn normal_comments_cursor_is_song_specific_and_hot_replies_are_active() {
    fn item(id: &str) -> Value {
        json!({"commentId":id,"commentInfo":"正文","commentTime":"2024-10-04 10:00:00","user":{"nickName":"用户","middleIcon":"avatar","userId":"u1"},"opNumItem":{"thumbNum":2},"replyComments":[{"replyId":"r1","replyInfo":"回复","user":{"nickName":"回复者"}}]})
    }
    let f = Fixture::with(vec![
        json!({"code":"000000","data":{"commentNums":"6","comments":[item("c1")]}}),
        json!({"code":"000000","data":{"commentNums":6,"comments":[item("c2")]}}),
        json!({"code":"000000","data":{"cfgHotCount":2,"hotComments":[item("hot")]}}),
        json!({"code":"000000","data":{"replyTotalCount":2,"mainCommentItem":{"replyComments":[{"commentId":"r2","commentInfo":"楼中楼","user":{"nickName":"乙"},"opNumItem":{"thumbNum":3}},{"replyId":"r3","replyInfo":"兼容回复","user":{"nickName":"丙"}}]}}}),
    ]);
    let p = MgProvider::new(f.clone());
    let c = ProviderContext::default();
    let normal = |id: &str, page, cursor| ProviderRequest::Comments {
        id: id.into(),
        kind: CommentKind::Normal,
        page,
        limit: 2,
        cursor,
        root_comment_id: None,
    };
    assert!(matches!(
        p.execute(normal("s1", 2, None), &c),
        Err(ProviderError::InvalidRequest { .. })
    ));
    let ProviderResponse::Comments(first) = p.execute(normal("s1", 1, None), &c).unwrap() else {
        panic!()
    };
    assert_eq!(first.next_cursor.as_deref(), Some("c1"));
    assert_eq!(first.items[0].replies[0].text, "回复");
    assert_eq!(first.items[0].timestamp_ms, Some(1728007200000));
    assert!(matches!(
        p.execute(normal("s2", 2, None), &c),
        Err(ProviderError::InvalidRequest { .. })
    ));
    let ProviderResponse::Comments(second) = p.execute(normal("s1", 2, None), &c).unwrap() else {
        panic!()
    };
    assert_eq!(second.next_cursor.as_deref(), Some("c2"));
    assert!(f.requests()[1].url.ends_with("commentId=c1"));
    let ProviderResponse::Comments(hot) = p
        .execute(
            ProviderRequest::Comments {
                id: "s1".into(),
                kind: CommentKind::Hot,
                page: 2,
                limit: 2,
                cursor: None,
                root_comment_id: None,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(hot.total, 2);
    assert!(f.requests()[2].url.ends_with("hotCommentStart=2"));
    let ProviderResponse::Comments(reply) = p
        .execute(
            ProviderRequest::Comments {
                id: "s1".into(),
                kind: CommentKind::Replies,
                page: 1,
                limit: 10,
                cursor: None,
                root_comment_id: Some("hot".into()),
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(reply.items[0].id, "r2");
    assert_eq!(reply.items[0].text, "楼中楼");
    assert_eq!(reply.items[0].likes, 3);
    assert_eq!(reply.items[1].text, "兼容回复");
    assert!(f.requests()[3].url.contains("/stack/hot/v1.0?"));
}
#[test]
fn mrc_decryption_and_plain_lyrics_are_returned_with_translation() {
    let encrypted = "b4b2a3da51aaeec40a220e734d6daafa5ab329d576b2b6f5dfd12a464f0ec53d3d69b75b833a01dc0f9a0b536b6e4263";
    let f = Fixture::responses(vec![
        raw(encrypted),
        raw("[00:01.000]翻译"),
        raw("[00:01.000]普通歌词"),
        raw("[00:01.000]译文"),
    ]);
    let p = MgProvider::new(f.clone());
    let c = ProviderContext::default();
    let ProviderResponse::Lyrics(mrc) = p
        .execute(
            ProviderRequest::Lyrics {
                track: lyric_track("https://l.example/mrc"),
                word_by_word: true,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(mrc.lyric, "[00:01.000]ABCD");
    assert_eq!(mrc.word_by_word.as_deref(), Some("[00:01.000]<0,500>ABCD"));
    assert_eq!(mrc.translated.as_deref(), Some("[00:01.000]翻译"));
    assert_eq!(
        f.requests()[0].headers["Referer"],
        "https://app.c.nf.migu.cn/"
    );
    assert_eq!(f.requests()[0].headers["channel"], "0146921");
    let ProviderResponse::Lyrics(plain) = p
        .execute(
            ProviderRequest::Lyrics {
                track: lyric_track(""),
                word_by_word: false,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(plain.lyric, "[00:01.000]普通歌词");
    assert_eq!(plain.translated.as_deref(), Some("[00:01.000]译文"));
    assert_eq!(plain.word_by_word, None);
}
#[test]
fn absent_mrc_metadata_is_fetched_before_lyric() {
    let mut s = song("s1");
    s["mrcUrl"] = json!("");
    s["trcUrl"] = json!("");
    let record = TrackRecord::from_lx_dto(
        json!({"name":"曲","singer":"歌手","source":"mg","songmid":"s1"}),
        LxMusicDtoVersion::Legacy,
    )
    .unwrap();
    let f = Fixture::responses(vec![
        raw(&json!({"code":"000000","data":{"resource":[s]}}).to_string()),
        raw("[00:01.000]词"),
    ]);
    let p = MgProvider::new(f.clone());
    let ProviderResponse::Lyrics(l) = p
        .execute(
            ProviderRequest::Lyrics {
                track: record,
                word_by_word: true,
            },
            &ProviderContext::default(),
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(l.lyric, "[00:01.000]词");
    assert_eq!(f.requests()[0].method, "POST");
    assert_eq!(f.requests()[1].url, "https://l.example/lrc");
}
#[test]
fn upstream_errors_parse_errors_cancel_and_inactive_operations_are_typed() {
    let f = Fixture::with(vec![
        json!({"code":"100001","info":"failure"}),
        json!({"code":"000000","data":{}}),
    ]);
    let p = MgProvider::new(f.clone());
    let c = ProviderContext::default();
    assert!(matches!(
        p.execute(ProviderRequest::HotSearch, &c),
        Err(ProviderError::Upstream { .. })
    ));
    assert!(matches!(
        p.execute(ProviderRequest::HotSearch, &c),
        Err(ProviderError::Parse { .. })
    ));
    assert!(matches!(
        p.execute(
            ProviderRequest::Suggest {
                keyword: "词".into()
            },
            &c
        ),
        Err(ProviderError::Unsupported { .. })
    ));
    assert!(matches!(
        p.execute(
            ProviderRequest::Album {
                id: "a1".into(),
                page: 1,
                limit: 20
            },
            &c
        ),
        Err(ProviderError::Unsupported { .. })
    ));
    c.cancel.store(true, Ordering::Release);
    assert_eq!(
        p.execute(ProviderRequest::Charts, &c),
        Err(ProviderError::Cancelled)
    );
    assert_eq!(f.requests().len(), 2);
}
