use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    platforms::{
        wy::{WyProvider, crypto, normalize_lyrics},
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
        Arc::new(Self {
            replies: Mutex::new(
                values
                    .into_iter()
                    .map(|v| HttpResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                        bytes: v.to_string().into_bytes(),
                    })
                    .collect(),
            ),
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
fn song(id: u64) -> Value {
    json!({"id":id,"name":"歌曲","ar":[{"name":"甲"},{"name":"乙"}],"al":{"id":7,"name":"专辑","picUrl":"https://p.example/cover.jpg"},"dt":123000,"l":{"size":1024},"h":{"size":2048},"sq":{"size":4096},"hr":{"size":8192}})
}
fn privilege(id: u64) -> Value {
    json!({"id":id,"maxbr":999000,"maxBrLevel":"hires"})
}
fn record() -> TrackRecord {
    TrackRecord::from_lx_dto(
        json!({"name":"曲","singer":"歌手","source":"wy","songmid":1}),
        LxMusicDtoVersion::Legacy,
    )
    .unwrap()
}
fn eapi_plain(request: &HttpRequest) -> String {
    use aes::{
        Aes128,
        cipher::{Block, BlockDecrypt, KeyInit},
    };
    let body = std::str::from_utf8(&request.body).unwrap();
    let hex = body.strip_prefix("params=").unwrap();
    let mut bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let cipher = Aes128::new_from_slice(b"e82ckenh8dichen8").unwrap();
    for b in bytes.as_chunks_mut::<16>().0 {
        cipher.decrypt_block(Block::<Aes128>::from_mut_slice(b));
    }
    let pad = *bytes.last().unwrap() as usize;
    bytes.truncate(bytes.len() - pad);
    String::from_utf8(bytes).unwrap()
}
#[test]
fn independent_crypto_vectors() {
    assert_eq!(
        crypto::weapi_with_secret(
            &json!({"c":"[{\"id\":1}]","ids":"[1]"}),
            *b"abcdefghijklmnop"
        ),
        "params=stcT8hbEcBipoi7sPoOj8WOV48KxmIKxY1qhUi9JuvW8jRcg0PmuDdot93bST4OhlFNmoWgrUYBP1eLUsVa%2BINDQJ4E3MOWu2lKL9pqOEW4%3D&encSecKey=d15a1683c992095d0c234c19966605c5c5964911268bbeda8cb8d08d834913e59d53b32358903a121b5fca784c1f5ae44951fd02524df58ecc98e52cc7cf8689b42c2e93ddf05b0592512d87f5960467e2f086c018849d76014d323500e30f13ef4cafbb0cf5a66731a3f1776c75ca35d0062dac70a3e33245afabcf47938487"
    );
    assert_eq!(
        crypto::eapi("/api/song/lyric/v1", &json!({"id":1,"cp":false})),
        "params=04AE33D34A93FE3EC22DA8FA305D290AB337D0FE5F36D211DE0D338CC6AA89D05DB0A4497F686ACB842B1DF6644E2CD30FDF7EE2EFA256BBDC51EAF5003E8DF06BAB864C3E38996E7FB05F0B8C5E2E3B842C8AB4ED76BC78B926D3E1DEE61DF7"
    );
    assert_eq!(
        crypto::linuxapi(
            &json!({"method":"POST","url":"https://music.163.com/api/v3/playlist/detail","params":{"id":"1","n":100000,"s":8}})
        ),
        "eparams=A0D9583F4C5FF68DE851D2893A49DE98FAFB24399F27B4F7E74C64B6FC49A965CFA972FA5EA3D6247CD6247C8198CB87193DFC37756896DF3666AE409ED93B3934F099F173733DD2A2271FA12552055950039207856C3B1C32DC38BA94DC6CB3ACC7848017CDDEEF310A8E9F4DC7D007"
    );
}
#[test]
fn search_and_hotword_protocol_and_numeric_quality_metadata() {
    let mut s = song(100001);
    s["privilege"] = privilege(100001);
    let f = Fixture::with(vec![
        json!({"code":200,"data":{"resources":[{"baseInfo":{"simpleSongData":s}}],"totalCount":90}}),
        json!({"code":200,"data":{"itemList":[{"searchWord":"热词"}]}}),
    ]);
    let provider = WyProvider::new(f.clone());
    let ProviderResponse::Tracks(page) = provider
        .execute(
            ProviderRequest::Search {
                keyword: "你好 & 世界".into(),
                page: 2,
                limit: 30,
            },
            &ProviderContext::default(),
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!((page.page, page.limit, page.total), (2, 30, 90));
    assert_eq!(page.items[0].platform.song_id, json!(100001));
    assert_eq!(
        page.items[0].platform.qualities,
        vec!["128k", "320k", "flac", "flac24bit"]
    );
    assert_eq!(page.items[0].metadata["privilege"]["maxbr"], 999000);
    assert_eq!(page.items[0].artists, vec!["甲、乙"]);
    let req = &f.requests()[0];
    assert_eq!(req.url, "http://interface.music.163.com/eapi/batch");
    assert_eq!(req.headers["origin"], "https://music.163.com");
    assert_eq!(
        req.headers["Content-Type"],
        "application/x-www-form-urlencoded"
    );
    let plain = eapi_plain(req);
    let pieces: Vec<_> = plain.split("-36cd479b6b5-").collect();
    assert_eq!(pieces[0], "/api/search/song/list/page");
    let body: Value = serde_json::from_str(pieces[1]).unwrap();
    assert_eq!(body["offset"], 30);
    assert_eq!(body["keyword"], "你好 & 世界");
    assert_eq!(body["total"], false);
    assert_eq!(
        provider
            .execute(ProviderRequest::HotSearch, &ProviderContext::default())
            .unwrap(),
        ProviderResponse::Keywords(vec!["热词".into()])
    );
    assert!(eapi_plain(&f.requests()[1]).contains("HOT_SEARCH_SONG#@#"));
}
#[test]
fn detail_charts_collections_and_playlist_operations() {
    let collection = json!({"code":200,"playlist":{"id":55,"name":"歌单","coverImgUrl":"https://p.example/list.jpg","creator":{"nickname":"创建者"},"trackIds":[{"id":1},{"id":2}],"tracks":[song(1),song(2)],"trackCount":2,"playCount":50},"privileges":[privilege(1),privilege(2)]});
    let f = Fixture::with(vec![
        json!({"code":200,"songs":[song(2),song(1)],"privileges":[privilege(1),privilege(2)]}),
        collection.clone(),
        collection.clone(),
        json!({"code":200,"sub":[{"name":"华语","category":0}],"categories":{"0":"语种"}}),
        json!({"code":200,"tags":[{"playlistTag":{"name":"流行"}}]}),
        json!({"code":200,"playlists":[collection["playlist"].clone()],"total":8}),
        json!({"code":200,"result":{"playlists":[collection["playlist"].clone()],"playlistCount":3}}),
    ]);
    let p = WyProvider::new(f.clone());
    let c = ProviderContext::default();
    let ProviderResponse::Tracks(detail) = p
        .execute(
            ProviderRequest::Detail {
                ids: vec!["1".into(), "2".into()],
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(detail.items[0].platform.song_id, 1);
    let ProviderResponse::Charts(charts) = p.execute(ProviderRequest::Charts, &c).unwrap() else {
        panic!()
    };
    assert_eq!(charts.len(), 42);
    assert_eq!(charts[0].id, "wy__19723756");
    let ProviderResponse::Tracks(chart) = p
        .execute(
            ProviderRequest::ChartTracks {
                id: "wy__55".into(),
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
    assert_eq!(chart.items[0].platform.song_id, 2);
    let ProviderResponse::Collection(list) = p
        .execute(
            ProviderRequest::PlaylistDetail {
                id: "https://music.163.com/#/playlist?id=55###session".into(),
                page: 1,
                limit: 1,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(list.info.name, "歌单");
    assert_eq!(list.tracks.items.len(), 1);
    assert_eq!(f.requests()[2].headers["Cookie"], "MUSIC_U=session");
    assert_eq!(
        f.requests()[2].url,
        "https://music.163.com/api/linux/forward"
    );
    let ProviderResponse::PlaylistTags(tags) =
        p.execute(ProviderRequest::PlaylistTags, &c).unwrap()
    else {
        panic!()
    };
    assert_eq!(tags[0].group, "语种");
    assert_eq!(tags[1].group, "热门");
    let ProviderResponse::Playlists(lists) = p
        .execute(
            ProviderRequest::Playlists {
                tag: "华语".into(),
                sort: "hot".into(),
                page: 1,
                limit: 30,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(lists.total, 8);
    let ProviderResponse::Playlists(search) = p
        .execute(
            ProviderRequest::SearchPlaylists {
                keyword: "词".into(),
                page: 2,
                limit: 20,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(search.total, 3);
    let plain = eapi_plain(&f.requests()[6]);
    assert!(plain.contains("/api/cloudsearch/pc"));
    assert!(plain.contains("\"type\":1000"));
}
#[test]
fn playlist_missing_tracks_are_hydrated_in_order() {
    let f = Fixture::with(vec![
        json!({"code":200,"playlist":{"id":1,"name":"列表","trackIds":[{"id":3},{"id":2},{"id":1}],"tracks":[song(3)]},"privileges":[privilege(3)]}),
        json!({"code":200,"songs":[song(1)],"privileges":[privilege(1)]}),
    ]);
    let p = WyProvider::new(f.clone());
    let ProviderResponse::Collection(c) = p
        .execute(
            ProviderRequest::PlaylistDetail {
                id: "1".into(),
                page: 2,
                limit: 2,
            },
            &ProviderContext::default(),
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(c.tracks.total, 3);
    assert_eq!(c.tracks.items.len(), 1);
    assert_eq!(c.tracks.items[0].platform.song_id, 1);
    assert_eq!(f.requests().len(), 2);
}
#[test]
fn comment_cursor_forward_backward_replay_and_explicit_restore() {
    fn comments(cursor: &str) -> Value {
        json!({"code":200,"data":{"cursor":cursor,"totalCount":10,"comments":[{"commentId":7,"content":"好听[爱心]","time":123,"user":{"nickname":"甲","avatarUrl":"pic"},"likedCount":9,"beReplied":[{"beRepliedCommentId":6,"content":"前文","user":{"nickname":"乙"}}]}]}})
    }
    let f = Fixture::with(vec![
        comments("c1"),
        comments("c3"),
        comments("back"),
        comments("replay"),
        json!({"code":200,"total":2,"hotComments":[]}),
    ]);
    let p = WyProvider::new(f.clone());
    let c = ProviderContext::default();
    let request = |id: &str, page, cursor| ProviderRequest::Comments {
        id: id.into(),
        kind: CommentKind::Normal,
        page,
        limit: 2,
        cursor,
        root_comment_id: None,
    };
    assert!(matches!(
        p.execute(request("9", 2, None), &c),
        Err(ProviderError::InvalidRequest { .. })
    ));
    let ProviderResponse::Comments(first) = p.execute(request("9", 1, None), &c).unwrap() else {
        panic!()
    };
    assert_eq!(first.items[0].id, "7");
    assert_eq!(first.items[0].extra["rootId"], 6);
    assert_eq!(first.items[0].replies[0].text, "好听❤️");
    let ProviderResponse::Comments(next) = p.execute(request("9", 3, None), &c).unwrap() else {
        panic!()
    };
    let state: Value = serde_json::from_str(next.next_cursor.as_ref().unwrap()).unwrap();
    assert_eq!(state["previous"], "c1");
    assert_eq!(state["order"], 1);
    assert_eq!(state["offset"], 2);
    let ProviderResponse::Comments(back) = p.execute(request("9", 1 + 1, None), &c).unwrap() else {
        panic!()
    };
    let state: Value = serde_json::from_str(back.next_cursor.as_ref().unwrap()).unwrap();
    assert_eq!(state["order"], 0);
    assert_eq!(state["previous"], "c3");
    let ProviderResponse::Comments(replay) = p.execute(request("9", 2, None), &c).unwrap() else {
        panic!()
    };
    let state: Value = serde_json::from_str(replay.next_cursor.as_ref().unwrap()).unwrap();
    assert_eq!(state["order"], 0);
    assert_eq!(state["previous"], "c3");
    let response = p
        .execute(
            ProviderRequest::Comments {
                id: "9".into(),
                kind: CommentKind::Hot,
                page: 2,
                limit: 2,
                cursor: None,
                root_comment_id: None,
            },
            &c,
        )
        .unwrap();
    assert!(matches!(response, ProviderResponse::Comments(_)));
    assert!(f.requests().last().unwrap().url.ends_with("/R_SO_4_9"));
    let f2 = Fixture::with(vec![comments("c2")]);
    let p2 = WyProvider::new(f2);
    let ProviderResponse::Comments(restored) =
        p2.execute(request("9", 2, first.next_cursor), &c).unwrap()
    else {
        panic!()
    };
    assert_eq!(restored.page, 2);
}
#[test]
fn dynamic_lyrics_and_time_alignment_are_returned_by_provider() {
    let lyrics = json!({"code":200,"lrc":{"lyric":"[00:01.000]fallback"},"yrc":{"lyric":"{\"t\":0,\"c\":[{\"tx\":\"作者\"}]}\n[offset:25]\n[1000,600](900,300,0)你(1300,300,0)好"},"ytlrc":{"lyric":"[00:01.050]Hello"},"yromalrc":{"lyric":"[00:01.020]ni hao"}});
    let f = Fixture::with(vec![lyrics.clone(), json!({"code":200,"songs":[song(1)]})]);
    let p = WyProvider::new(f.clone());
    let c = ProviderContext::default();
    let ProviderResponse::Lyrics(l) = p
        .execute(
            ProviderRequest::Lyrics {
                track: record(),
                word_by_word: true,
            },
            &c,
        )
        .unwrap()
    else {
        panic!()
    };
    assert!(l.lyric.contains("[00:01.000]你好"));
    assert!(l.lyric.contains("[00:00.000]作者"));
    assert_eq!(
        l.word_by_word.as_deref(),
        Some("[offset:25]\n[00:01.000]<0,300>你<300,300>好")
    );
    assert_eq!(l.translated.as_deref(), Some("[00:01.000]Hello"));
    assert_eq!(l.romanized.as_deref(), Some("[00:01.000]ni hao"));
    let req = &f.requests()[0];
    assert_eq!(
        req.url,
        "https://interface3.music.163.com/eapi/song/lyric/v1"
    );
    let plaintext = eapi_plain(req);
    assert!(plaintext.starts_with("/api/song/lyric/v1"));
    assert!(plaintext.contains("\"id\":1"));
    assert_eq!(
        p.execute(ProviderRequest::Picture { track: record() }, &c)
            .unwrap(),
        ProviderResponse::Picture("https://p.example/cover.jpg".into())
    );
    assert_eq!(
        f.requests()[1].headers["Referer"],
        "https://music.163.com/song?id=1"
    );
    let fallback = normalize_lyrics(
        &json!({"lrc":{"lyric":"[01:02:34]字"},"tlyric":{"lyric":"[01:02:34]word"}}),
    )
    .unwrap();
    assert_eq!(fallback.lyric, "[01:02.34]字");
    let punctuation = normalize_lyrics(
        &json!({"yrc":{"lyric":"[1000,2000](1000,1000,0)Hello (world)(2000,1000,0)!"}}),
    )
    .unwrap();
    assert_eq!(punctuation.lyric, "[00:01.000]Hello (world)!");
    assert_eq!(
        punctuation.word_by_word.as_deref(),
        Some("[00:01.000]<0,1000>Hello (world)<1000,1000>!")
    );
}
#[test]
fn invalid_upstream_cancelled_and_unsupported_are_typed() {
    let f = Fixture::with(vec![
        json!({"code":500,"message":"busy"}),
        json!({"code":200,"data":{}}),
    ]);
    let p = WyProvider::new(f.clone());
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
            ProviderRequest::Detail {
                ids: vec!["not-id".into()]
            },
            &c
        ),
        Err(ProviderError::InvalidRequest { .. })
    ));
    c.cancel.store(true, Ordering::Release);
    assert_eq!(
        p.execute(ProviderRequest::Charts, &c),
        Err(ProviderError::Cancelled)
    );
    assert_eq!(f.requests().len(), 2);
}
