use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    platforms::{
        kw::{
            KwProvider, build_wbd_param, build_www_secret, decode_lyric, decode_wbd,
            normalize_word_times, parse_lyric, parse_object_text,
        },
        *,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    sync::{Arc, Mutex},
};

struct Fixture {
    replies: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}
impl Fixture {
    fn http(replies: Vec<HttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn bytes(replies: Vec<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|bytes| HttpResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                        bytes,
                    })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn json(replies: Vec<Value>) -> Arc<Self> {
        Self::bytes(
            replies
                .into_iter()
                .map(|v| serde_json::to_vec(&v).unwrap())
                .collect(),
        )
    }
    fn request(&self, index: usize) -> HttpRequest {
        self.requests.lock().unwrap()[index].clone()
    }
    fn done(&self) {
        assert!(self.replies.lock().unwrap().is_empty());
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
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected HTTP request"))
    }
}
fn execute(fixture: &Arc<Fixture>, request: ProviderRequest) -> ProviderResponse {
    KwProvider::new(fixture.clone())
        .execute(request, &ProviderContext::default())
        .unwrap()
}
fn track() -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"source":"kw","name":"Fixture","singer":"Singer","songmid":"7","albumName":"Album","types":[],"_types":{},"interval":"03:05"}),LxMusicDtoVersion::Legacy).unwrap()
}
fn n_minfo() -> &'static str {
    "level:lossless,bitrate:4000,format:flac,size:55.5m;level:lossless,bitrate:2000,format:flac,size:30m;level:exhigh,bitrate:320,format:mp3,size:8m;level:standard,bitrate:128,format:mp3,size:3m"
}
fn list_track() -> Value {
    json!({"id":7,"name":"List &amp; Song","artist":"One&Two","album":"Record","albumid":8,"duration":"185","N_MINFO":n_minfo(),"unknown":"retained"})
}

#[test]
fn search_query_and_quality_contract_match_pinned_sdk() {
    let fixture = Fixture::json(vec![
        json!({"TOTAL":"1","SHOW":"1","abslist":[{"MUSICRID":"MUSIC_7","SONGNAME":"<b>Song</b> &amp; Tune","ARTIST":"One&Two","ALBUM":"Record","ALBUMID":"8","DURATION":"185","N_MINFO":n_minfo(),"extra":"raw"}]}),
    ]);
    let result = execute(
        &fixture,
        ProviderRequest::Search {
            keyword: "中文 &".into(),
            page: 2,
            limit: 30,
        },
    );
    assert_eq!(
        fixture.request(0).url,
        "http://search.kuwo.cn/r.s?client=kt&all=%E4%B8%AD%E6%96%87%20%26&pn=1&rn=30&uid=794762570&ver=kwplayer_ar_9.2.2.1&vipver=1&show_copyright_off=1&newver=1&ft=music&cluster=0&strategy=2012&encoding=utf8&rformat=json&vermerge=1&mobi=1&issubtitle=1"
    );
    let ProviderResponse::Tracks(page) = result else {
        panic!()
    };
    assert_eq!(page.total, 1);
    assert_eq!(page.page, 2);
    assert_eq!(page.items[0].title, "Song & Tune");
    assert_eq!(page.items[0].artists, vec!["One、Two"]);
    assert_eq!(page.items[0].duration_ms, Some(185000));
    assert_eq!(
        page.items[0].platform.qualities,
        vec!["128k", "320k", "flac", "flac24bit"]
    );
    assert_eq!(
        page.items[0].metadata["_qualitys"]["flac24bit"]["size"],
        "55.5M"
    );
    assert_eq!(page.items[0].metadata["kwRaw"]["extra"], "raw");
    fixture.done();
}

#[test]
fn semantic_search_retries_are_bounded_and_not_reported_as_empty_success() {
    let invalid = json!({"TOTAL":"1","SHOW":"0"});
    let fixture = Fixture::json(vec![invalid.clone(), invalid.clone(), invalid]);
    assert!(matches!(
        KwProvider::new(fixture.clone()).execute(
            ProviderRequest::Search {
                keyword: "x".into(),
                page: 1,
                limit: 30
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Upstream { .. })
    ));
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    fixture.done();
    let invalid = json!({"TOTAL":"1","SHOW":"1","abslist":[{"MUSICRID":"MUSIC_7"}]});
    let fixture = Fixture::json(vec![invalid.clone(), invalid.clone(), invalid]);
    assert!(matches!(
        KwProvider::new(fixture.clone()).execute(
            ProviderRequest::Search {
                keyword: "x".into(),
                page: 1,
                limit: 30
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Parse { .. })
    ));
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
}

#[test]
fn only_active_tip_search_and_hotword_endpoints_are_called() {
    let fixture = Fixture::json(vec![
        json!({"WORDITEMS":[{"RELWORD":"Keyword"}]}),
        json!({"status":"ok","tagvalue":[{"key":"Hot"}]}),
    ]);
    assert_eq!(
        execute(
            &fixture,
            ProviderRequest::Suggest {
                keyword: "a +".into()
            }
        ),
        ProviderResponse::Keywords(vec!["Keyword".into()])
    );
    assert_eq!(
        fixture.request(0).url,
        "https://tips.kuwo.cn/t.s?corp=kuwo&newver=3&p2p=1&notrace=0&c=mbox&w=a%20%2B&encoding=utf8&rformat=json"
    );
    assert_eq!(fixture.request(0).headers["Referer"], "http://www.kuwo.cn/");
    assert_eq!(
        execute(&fixture, ProviderRequest::HotSearch),
        ProviderResponse::Keywords(vec!["Hot".into()])
    );
    assert!(
        fixture
            .request(1)
            .url
            .contains("hotword.s?prod=kwplayer_ar_9.3.0.1")
    );
    assert_eq!(
        fixture.request(1).headers["User-Agent"],
        "Dalvik/2.1.0 (Linux; U; Android 9;)"
    );
    fixture.done();
}

#[test]
fn details_call_exported_music_info_and_preserve_raw_metadata() {
    let fixture = Fixture::json(vec![
        json!({"code":200,"data":{"rid":7,"name":"Detail","artist":"A&B","album":"Record","pic":"https://image.invalid/7","duration":185,"opaque":4}}),
    ]);
    let ProviderResponse::Tracks(page) = execute(
        &fixture,
        ProviderRequest::Detail {
            ids: vec!["7".into()],
        },
    ) else {
        panic!()
    };
    assert_eq!(
        fixture.request(0).url,
        "http://www.kuwo.cn/api/www/music/musicInfo?mid=7"
    );
    assert_eq!(page.items[0].title, "Detail");
    assert!(page.items[0].platform.qualities.is_empty());
    assert_eq!(page.items[0].metadata["kwRaw"]["opaque"], 4);
    fixture.done();
}

fn guarded_detail_response() -> HttpResponse {
    HttpResponse {status:200,headers:BTreeMap::new(),bytes:br#"{"success":false,"message":"The request is illegal!","now":"2026-10-06T03:01:44.857Z"}"#.to_vec()}
}
fn official_page_response(token: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: BTreeMap::from([(
            "Set-Cookie".into(),
            format!(
                "MUSIC_U=unrelated-cookie; path=/, Hm_Iuvt_cdb524f42f23cer9b268564v7y735ewrq2324={token}; path=/; expires=Tue, 13 Oct 2026 03:01:44 GMT"
            ),
        )]),
        bytes: b"<!doctype html><html></html>".to_vec(),
    }
}
fn current_official_music_info() -> Value {
    // Official WWW musicInfo metadata response, 2026-10-06.
    json!({"code":200,"data":{"musicrid":"MUSIC_655917654","artist":"Carmelo","rid":655917654,"name":"test(Explicit)","album":"test (Explicit)","albumid":104414341,"duration":190,"songTimeMinutes":"03:10","pic":"https://img2.kuwo.cn/star/albumcover/500/s4s8/65/3450331578.jpg","artistid":10865229,"hasLossless":true,"releaseDate":"2026-10-05","isListenFee":false,"online":1,"payInfo":{"play":"1100","nplay":"001111111111","down":"1111","ndown":"111111111111","cannotDownload":0,"cannotOnlinePlay":0,"feeType":{"song":"1","vip":"1"},"paytagindex":{"S":2,"F":3,"ZP":6,"H":1,"HR":4,"L":0}}},"msg":"success","profileId":"site"})
}

#[test]
fn current_www_guard_handshake_uses_only_anonymous_official_token_and_returns_real_detail() {
    let token = "0123456789ABCDEFGHIJKLMNOPQRSTUV";
    let rotated = "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
    let success = HttpResponse {
        status: 200,
        headers: BTreeMap::from([(
            "set-cookie".into(),
            format!("Hm_Iuvt_cdb524f42f23cer9b268564v7y735ewrq2324={rotated}; path=/"),
        )]),
        bytes: serde_json::to_vec(&current_official_music_info()).unwrap(),
    };
    let fixture = Fixture::http(vec![
        guarded_detail_response(),
        official_page_response(token),
        success.clone(),
        guarded_detail_response(),
        success,
    ]);
    let provider = KwProvider::new(fixture.clone());
    for _ in 0..2 {
        let ProviderResponse::Tracks(page) = provider
            .execute(
                ProviderRequest::Detail {
                    ids: vec!["655917654".into()],
                },
                &ProviderContext::default(),
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(page.items.len(), 1);
        let track = &page.items[0];
        assert_eq!(track.title, "test(Explicit)");
        assert_eq!(track.artists, vec!["Carmelo"]);
        assert_eq!(track.album, "test (Explicit)");
        assert_eq!(track.duration_ms, Some(190000));
        assert_eq!(track.platform.song_id, 655917654);
        assert_eq!(track.metadata["albumId"], 104414341);
        assert_eq!(track.metadata["kwRaw"]["payInfo"]["nplay"], "001111111111");
        assert!(track.platform.qualities.is_empty());
        assert!(!serde_json::to_string(track).unwrap().contains("Hm_Iuvt"));
    }
    assert_eq!(
        fixture.request(0).url,
        "http://www.kuwo.cn/api/www/music/musicInfo?mid=655917654"
    );
    assert_eq!(
        fixture.request(1).url,
        "https://www.kuwo.cn/play_detail/655917654"
    );
    assert!(!fixture.request(1).headers.contains_key("Cookie"));
    let req_id_pattern =
        regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
            .unwrap();
    for (index, token) in [(2, token), (4, rotated)] {
        let signed = fixture.request(index);
        let url = reqwest::Url::parse(&signed.url).unwrap();
        assert_eq!(url.host_str(), Some("www.kuwo.cn"));
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.path(), "/api/www/music/musicInfo");
        let params = url.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(params["mid"], "655917654");
        assert_eq!(params["httpsStatus"], "1");
        assert_eq!(params["plat"], "web_www");
        assert_eq!(params["from"], "");
        assert!(req_id_pattern.is_match(&params["reqId"]));
        assert_eq!(
            signed.headers["Cookie"],
            format!("Hm_Iuvt_cdb524f42f23cer9b268564v7y735ewrq2324={token}")
        );
        assert!(!signed.headers["Cookie"].contains("MUSIC_U"));
        let secret = &signed.headers["Secret"];
        let salt = u32::from_str_radix(&secret[secret.len() - 8..], 16).unwrap();
        assert_eq!(secret, &build_www_secret(token, salt).unwrap());
        assert_eq!(
            signed.headers["Referer"],
            "https://www.kuwo.cn/play_detail/655917654"
        );
    }
    fixture.done();
}

#[test]
fn anonymous_www_guard_refresh_is_finite_and_failure_is_not_empty_success() {
    let fixture = Fixture::http(vec![
        guarded_detail_response(),
        official_page_response("0123456789ABCDEFGHIJKLMNOPQRSTUV"),
        guarded_detail_response(),
        official_page_response("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"),
        guarded_detail_response(),
    ]);
    let error = KwProvider::new(fixture.clone())
        .execute(
            ProviderRequest::Detail {
                ids: vec!["655917654".into()],
            },
            &ProviderContext::default(),
        )
        .unwrap_err();
    assert_eq!(error, ProviderError::upstream("The request is illegal!"));
    assert_eq!(fixture.requests.lock().unwrap().len(), 5);
    fixture.done();
    let fixture = Fixture::http(vec![
        guarded_detail_response(),
        HttpResponse {
            status: 200,
            headers: BTreeMap::from([(
                "set-cookie".into(),
                "MUSIC_U=unrelated-cookie; path=/".into(),
            )]),
            bytes: b"<html></html>".to_vec(),
        },
    ]);
    assert!(
        matches!(KwProvider::new(fixture.clone()).execute(ProviderRequest::Detail {ids:vec!["655917654".into()]},&ProviderContext::default()),Err(ProviderError::Upstream {message}) if message.contains("anonymous request token"))
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    fixture.done();
    for body in [
        json!({"code":200,"data":{}}),
        json!({"code":200,"data":{"rid":8,"name":"Different song"}}),
    ] {
        let fixture = Fixture::json(vec![body]);
        assert!(matches!(
            KwProvider::new(fixture.clone()).execute(
                ProviderRequest::Detail {
                    ids: vec!["7".into()]
                },
                &ProviderContext::default()
            ),
            Err(ProviderError::Parse { .. })
        ));
        fixture.done();
    }
    let fixture = Fixture::json(vec![
        json!({"success":false,"message":"Resource unavailable"}),
    ]);
    assert_eq!(
        KwProvider::new(fixture.clone())
            .execute(
                ProviderRequest::Detail {
                    ids: vec!["7".into()]
                },
                &ProviderContext::default()
            )
            .unwrap_err(),
        ProviderError::upstream("Resource unavailable")
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[test]
fn official_www_secret_js_number_folding_matches_independent_goldens_for_every_salt_length() {
    let token = "0123456789ABCDEFGHIJKLMNOPQRSTUV";
    for (salt, golden) in [
        (
            0,
            "a4a607d14e4670e18492f12192556675211bbd818da097b8a34470604044371500000000",
        ),
        (
            10,
            "137790d7404c4266fd338d88409b5d54691b6f1147ccaded82ae010e3470bd050000000a",
        ),
        (
            100,
            "130d8814e2feb74095d7b941f84138b3129f1394ab4a680ede2c75721b95394c00000064",
        ),
        (
            1000,
            "137790d7404c4266fd338d88409b5d54691b6f1147ccaded82ae010e3470bd05000003e8",
        ),
        (
            10000,
            "a4a607d14e4670e18492f12192556675211bbd818da097b8a34470604044371500002710",
        ),
        (
            100000,
            "137790d7404c4266fd338d88409b5d54691b6f1147ccaded82ae010e3470bd05000186a0",
        ),
        (
            1000000,
            "137790d7404c4266fd338d88409b5d54691b6f1147ccaded82ae010e3470bd05000f4240",
        ),
        (
            10000000,
            "1361b99125125e1ce61ce1d308fef46b4f87b89d7cb6d14946d02acbd5d230da00989680",
        ),
        (
            99999999,
            "1361b99125125e1ce61ce1d308fef46b4f87b89d7cb6d14946d02acbd5d230da05f5e0ff",
        ),
    ] {
        assert_eq!(build_www_secret(token, salt).unwrap(), golden);
    }
    assert!(build_www_secret("bad\r\nvalue", 1).is_err());
    assert!(build_www_secret(token, 100000000).is_err());
}

#[test]
fn album_digest_is_13_and_album_formats_have_sdk_quality_order() {
    let body = "{'name':'Album &amp; Name','albumid':'8','songnum':'1','artist':'Singer','img':'https://image.invalid/8','info':'Description','musiclist':[{'id':7,'name':'Song','artist':'A&B','pic':'https://image.invalid/7','formats':'MP3128|MP3H|ALFLAC|HIRFLAC'}]}";
    let fixture = Fixture::bytes(vec![body.as_bytes().to_vec(), body.as_bytes().to_vec()]);
    let ProviderResponse::Collection(collection) = execute(
        &fixture,
        ProviderRequest::PlaylistDetail {
            id: "digest-13__8".into(),
            page: 1,
            limit: 1000,
        },
    ) else {
        panic!()
    };
    assert_eq!(
        fixture.request(0).url,
        "http://search.kuwo.cn/r.s?pn=0&rn=1000&stype=albuminfo&albumid=8&show_copyright_off=0&encoding=utf&vipver=MUSIC_9.1.0"
    );
    assert_eq!(collection.info.id, "digest-13__8");
    assert_eq!(collection.info.name, "Album & Name");
    assert_eq!(
        collection.tracks.items[0].platform.qualities,
        vec!["128k", "320k", "flac", "flac24bit"]
    );
    assert_eq!(collection.tracks.items[0].duration_ms, None);
    assert_eq!(
        collection.tracks.items[0].metadata["_qualitys"]["flac"]["size"],
        Value::Null
    );
    assert!(matches!(
        execute(
            &fixture,
            ProviderRequest::Album {
                id: "8".into(),
                page: 1,
                limit: 1000
            }
        ),
        ProviderResponse::Collection(_)
    ));
    fixture.done();
}

#[test]
fn tags_call_both_exports_and_playlists_preserve_digest_and_sort() {
    let fixture = Fixture::json(vec![
        json!({"code":200,"data":[{"id":"g","name":"Style","data":[{"id":2,"digest":10000,"name":"Pop"}]}]}),
        json!({"code":200,"data":[{"data":[{"id":3,"digest":43,"name":"Hot"}]}]}),
        json!({"code":200,"data":{"data":[{"id":8,"digest":13,"name":"Album","uname":"Singer","total":2,"listencnt":50,"img":"https://image.invalid/8"}],"total":1,"pn":1,"rn":36}}),
        json!([{"list":[{"id":9,"digest":8,"name":"List","type":"songlist"},{"id":1,"digest":4,"name":"Artist","type":"artist"}]}]),
    ]);
    let ProviderResponse::PlaylistTags(tags) = execute(&fixture, ProviderRequest::PlaylistTags)
    else {
        panic!()
    };
    assert_eq!(tags[0].id, "2-10000");
    assert_eq!(tags[0].group, "Style");
    assert_eq!(tags[1].id, "3-43");
    assert_eq!(tags[1].extra["hot"], true);
    assert!(
        fixture
            .request(0)
            .url
            .contains("getTagList?cmd=rcm_keyword_playlist")
    );
    assert!(fixture.request(1).url.contains("getRcmTagList?"));
    let ProviderResponse::Playlists(page) = execute(
        &fixture,
        ProviderRequest::Playlists {
            tag: "2-10000".into(),
            sort: "new".into(),
            page: 1,
            limit: 36,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items[0].id, "digest-13__8");
    assert_eq!(page.items[0].play_count, Some(50));
    assert_eq!(
        fixture.request(2).url,
        "http://wapi.kuwo.cn/api/pc/classify/playlist/getTagPlayList?loginUid=0&loginSid=0&appUid=76039576&pn=1&id=2&rn=36"
    );
    let ProviderResponse::Playlists(page) = execute(
        &fixture,
        ProviderRequest::Playlists {
            tag: "3-43".into(),
            sort: "hot".into(),
            page: 1,
            limit: 36,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "digest-8__9");
    assert_eq!(
        fixture.request(3).url,
        "http://mobileinterfaces.kuwo.cn/er.s?type=get_pc_qz_data&f=web&id=3&prod=pc"
    );
    fixture.done();
    let fixture = Fixture::json(vec![json!({"code":200,"data":{"data":[],"total":0}})]);
    execute(
        &fixture,
        ProviderRequest::Playlists {
            tag: "".into(),
            sort: "new".into(),
            page: 2,
            limit: 36,
        },
    );
    assert_eq!(
        fixture.request(0).url,
        "http://wapi.kuwo.cn/api/pc/classify/playlist/getRcmPlayList?loginUid=0&loginSid=0&appUid=76039576&&pn=2&rn=36&order=new"
    );
}

#[test]
fn playlist_search_single_quote_response_matches_active_contract() {
    let fixture=Fixture::bytes(vec![b"{'TOTAL':'1','abslist':[{'playlistid':9,'name':'List &amp; Name','nickname':'O\'Neil','songnum':2,'playcnt':20,'pic':'https://image.invalid/9','intro':'Description'}]}".to_vec()]);
    let ProviderResponse::Playlists(page) = execute(
        &fixture,
        ProviderRequest::SearchPlaylists {
            keyword: "a&b".into(),
            page: 3,
            limit: 20,
        },
    ) else {
        panic!()
    };
    assert_eq!(
        fixture.request(0).url,
        "http://search.kuwo.cn/r.s?all=a%26b&pn=2&rn=20&rformat=json&encoding=utf8&ver=mbox&vipver=MUSIC_8.7.7.0_BCS37&plat=pc&devid=28156413&ft=playlist&pay=0&needliveshow=0"
    );
    assert_eq!(page.items[0].id, "9");
    assert_eq!(page.items[0].name, "List & Name");
    assert_eq!(page.items[0].author, Some("O'Neil".into()));
    fixture.done();
}

#[test]
fn digest_8_and_5_detail_follow_exported_request_chain() {
    let body = json!({"result":"ok","musiclist":[list_track()],"total":1,"rn":1000,"title":"Collection","pic":"https://image.invalid/list","uname":"Author","info":"Description","playnum":42});
    let fixture = Fixture::json(vec![body.clone(), json!({"child":[{"sourceid":99}]}), body]);
    let ProviderResponse::Collection(collection) = execute(
        &fixture,
        ProviderRequest::PlaylistDetail {
            id: "http://www.kuwo.cn/playlist_detail/9?t=x".into(),
            page: 2,
            limit: 1000,
        },
    ) else {
        panic!()
    };
    assert_eq!(collection.info.name, "Collection");
    assert_eq!(
        collection.tracks.items[0].platform.qualities,
        vec!["128k", "320k", "flac", "flac24bit"]
    );
    assert_eq!(
        fixture.request(0).url,
        "http://nplserver.kuwo.cn/pl.svc?op=getlistinfo&pid=9&pn=1&rn=1000&encode=utf8&keyset=pl2012&identity=kuwo&pcmp4=1&vipver=MUSIC_9.0.5.0_W1&newver=1"
    );
    execute(
        &fixture,
        ProviderRequest::PlaylistDetail {
            id: "digest-5__10".into(),
            page: 2,
            limit: 1000,
        },
    );
    assert_eq!(
        fixture.request(1).url,
        "http://qukudata.kuwo.cn/q.k?op=query&cont=ninfo&node=10&pn=0&rn=1&fmt=json&src=mbox&level=2"
    );
    assert_eq!(
        fixture.request(2).url,
        "http://nplserver.kuwo.cn/pl.svc?op=getlistinfo&pid=99&pn=1}&rn=1000&encode=utf-8&keyset=pl2012&identity=kuwo&pcmp4=1"
    );
    fixture.done();
}

#[test]
fn bodian_playlist_and_public_user_info_branches_are_active() {
    let data = json!({"code":200,"data":{"list":[{"id":7,"name":"Song","artists":[{"name":"Singer"}],"album":"Album","albumId":8,"duration":185,"albumPic":"https://image.invalid/7","audios":[{"bitrate":"320","size":"8m"},{"bitrate":"128","size":"3m"}],"releaseDate":"2020-01-01"}],"pageSize":1000,"total":1}});
    let fixture = Fixture::json(vec![
        data.clone(),
        json!({"code":200,"data":{"name":"Bodian","pic":"https://image.invalid/list","description":"Description","creatorName":"Creator","playNum":99}}),
        data,
        json!({"code":200,"data":{"userInfo":{"nickname":"User","headImg":"https://image.invalid/user"}}}),
    ]);
    let ProviderResponse::Collection(collection)=execute(&fixture,ProviderRequest::PlaylistDetail{id:"https://h5app.kuwo.cn/m/bodian/collection.html?uid=12&playlistId=9&source=4&ownerId=0".into(),page:1,limit:1000}) else {panic!()};
    assert_eq!(collection.info.name, "Bodian");
    assert_eq!(
        collection.tracks.items[0].platform.qualities,
        vec!["128k", "320k"]
    );
    assert!(
        fixture
            .request(0)
            .url
            .starts_with("https://bd-api.kuwo.cn/api/service/playlist/9/musicList?reqId=")
    );
    assert!(fixture.request(0).url.ends_with("&source=4&pn=1&rn=1000"));
    assert_eq!(fixture.request(0).headers["plat"], "h5");
    assert!(
        fixture
            .request(1)
            .url
            .contains("/api/service/playlist/info/9?")
    );
    let ProviderResponse::Collection(collection) = execute(
        &fixture,
        ProviderRequest::PlaylistDetail {
            id: "https://h5app.kuwo.cn/m/bodian/collection.html?uid=12&playlistId=9&source=5"
                .into(),
            page: 1,
            limit: 1000,
        },
    ) else {
        panic!()
    };
    assert_eq!(collection.info.name, "User喜欢的音乐");
    assert!(
        fixture
            .request(3)
            .url
            .contains("/api/ucenter/users/pub/12?")
    );
    fixture.done();
}

const WBD_GOLDEN: &str = "data=WkMIYYRh9KUzeRRawjXXaoMHen4RKh1V8Fy%2BlzIzNADTo666ORiMDeHjlF3tGF1DepboQy7YV9oPoe2hryX1o7hmVXa4ujFXpqu5jktJwVK3WTe8ueldViwbQG5VjKWlpXwLzByjOVdZyaNRJv4Q8gZfYm1aQwnCTUVKDFqcB3ZUKWavA4WIWz0nJvDyuZXc&time=1700000000000&appId=y67sprxhhpws&sign=B5CE50C9B4746B3D464E92F908DBF1D8";
const WBD_RESPONSE: &str = "QsrPX+VuDR5X4TJ7Rh3hrhRblHIgeVm+9VhrHjYx7TZU1PtbVjuO/NBVZzldGZY779shsYGswM30ldxIgij9dx6Dl77ex+6CUA3UaFxHn0deVkgRfiWRcfuxpsz2kx3+uyrxK8GijljTArBP7K/wYHMwaf00y5/yBkYi0HHAqtxMLjGlirFOx/F7ARv+zDmSREF0kfAZ0OyqOLfNHbzKUGJaZBH2DCxd7arPG4flQu9b5Qp+Nk8aPxyTv4iY6/RQ6Mgix5Lt3IsZjWXLKK46TJOzIgE9dL+jjesV9cSIgZs1Kb9GG97xXizluJrLuadqJNUJmfE7jN/7yeOYxJuVPoDdGW3SNrp1VGfNTFJ2Wc68OktXC9cIAAzMXrSDvqNqlnxUTIk7GyWpLw62vmMF8Q==";

#[test]
fn charts_use_fixed_exported_list_and_encrypted_wbd_wire_golden() {
    let data = json!({"uid":"","devId":"","sFrom":"kuwo_sdk","user_type":"AP","carSource":"kwplayercar_ar_6.0.1.0_apk_keluze.apk","id":"16","pn":0,"rn":100});
    assert_eq!(build_wbd_param(&data, 1700000000000).unwrap(), WBD_GOLDEN);
    assert_eq!(decode_wbd(WBD_RESPONSE).unwrap()["data"]["total"], 1);
    let fixture = Fixture::bytes(vec![WBD_RESPONSE.as_bytes().to_vec()]);
    let ProviderResponse::Charts(charts) = execute(&fixture, ProviderRequest::Charts) else {
        panic!()
    };
    assert_eq!(charts.len(), 43);
    assert_eq!(charts[0].id, "kw__93");
    assert_eq!(charts.last().unwrap().id, "kw__151");
    assert!(fixture.requests.lock().unwrap().is_empty());
    let ProviderResponse::Tracks(page) = execute(
        &fixture,
        ProviderRequest::ChartTracks {
            id: "kw__16".into(),
            page: 1,
            limit: 100,
        },
    ) else {
        panic!()
    };
    assert!(
        fixture
            .request(0)
            .url
            .starts_with("https://wbd.kuwo.cn/api/bd/bang/bang_info?data=")
    );
    assert!(fixture.request(0).url.contains("&appId=y67sprxhhpws&sign="));
    assert_eq!(page.items[0].platform.qualities, vec!["128k", "320k"]);
    assert_eq!(page.items[0].metadata["_qualitys"]["320k"]["size"], "8M");
    fixture.done();
}

#[test]
fn ordinary_and_hot_comments_keep_nested_replies_and_reject_unexported_reply_cursor() {
    let item = json!({"id":1,"msg":"Text","u_name":"User","u_pic":"https://image.invalid/user","u_id":3,"time":"1700000000","like_num":"4","mpic":"https%3A%2F%2Fimage.invalid%2F1","child_comments":[{"id":2,"msg":"Reply","u_name":"ReplyUser","time":1700000001,"like_num":1}]});
    let fixture = Fixture::json(vec![
        json!({"code":"200","comments_counts":25,"comments":[item.clone()]}),
        json!({"code":"200","hot_comments_counts":1,"hot_comments":[item]}),
    ]);
    let ProviderResponse::Comments(page) = execute(
        &fixture,
        ProviderRequest::Comments {
            id: "7".into(),
            kind: CommentKind::Normal,
            page: 2,
            limit: 20,
            cursor: None,
            root_comment_id: None,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.items[0].timestamp_ms, Some(1700000000000));
    assert_eq!(page.items[0].replies[0].text, "Reply");
    assert_eq!(page.items[0].likes, 4);
    assert_eq!(
        fixture.request(0).url,
        "http://ncomment.kuwo.cn/com.s?f=web&type=get_comment&aapiver=1&prod=kwplayer_ar_10.5.2.0&digest=15&sid=7&start=20&msgflag=1&count=20&newver=3&uid=0"
    );
    let ProviderResponse::Comments(page) = execute(
        &fixture,
        ProviderRequest::Comments {
            id: "7".into(),
            kind: CommentKind::Hot,
            page: 1,
            limit: 100,
            cursor: None,
            root_comment_id: None,
        },
    ) else {
        panic!()
    };
    assert_eq!(page.total, 1);
    assert!(fixture.request(1).url.contains("type=get_rec_comment"));
    fixture.done();
    let provider = KwProvider::new(fixture.clone());
    assert!(matches!(
        provider.execute(
            ProviderRequest::Comments {
                id: "7".into(),
                kind: CommentKind::Replies,
                page: 1,
                limit: 20,
                cursor: None,
                root_comment_id: Some("1".into())
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Unsupported { .. })
    ));
    assert!(matches!(
        provider.execute(
            ProviderRequest::Comments {
                id: "7".into(),
                kind: CommentKind::Normal,
                page: 1,
                limit: 20,
                cursor: Some("x".into()),
                root_comment_id: None
            },
            &ProviderContext::default()
        ),
        Err(ProviderError::Unsupported { .. })
    ));
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
}

fn lyric_fixture(text: &str, word_by_word: bool) -> Vec<u8> {
    let content = if word_by_word {
        STANDARD
            .encode(
                text.as_bytes()
                    .iter()
                    .enumerate()
                    .map(|(i, b)| *b ^ b"yeelion"[i % 7])
                    .collect::<Vec<_>>(),
            )
            .into_bytes()
    } else {
        text.as_bytes().to_vec()
    };
    let mut compressed = ZlibEncoder::new(Vec::new(), Compression::default());
    compressed.write_all(&content).unwrap();
    let mut output = b"tp=content\r\nContent-Type: text\r\n\r\n".to_vec();
    output.extend(compressed.finish().unwrap());
    output
}

#[test]
fn active_picture_and_binary_lyric_chain_is_executed_with_translation_and_word_timing() {
    let word_text = "[ti:Fixture]\n[kuwo:13]\n[00:00.00]<60,20>Hello<120,40>world\n[00:01.00]Translation\n[00:01.00]<200,60>Next";
    let fixture = Fixture::bytes(vec![
        b"https://image.invalid/7".to_vec(),
        lyric_fixture(word_text, true),
        lyric_fixture("[ti:Fixture]\n[00:01.00]Line", false),
    ]);
    assert_eq!(
        execute(&fixture, ProviderRequest::Picture { track: track() }),
        ProviderResponse::Picture("https://image.invalid/7".into())
    );
    assert_eq!(
        fixture.request(0).url,
        "http://artistpicserver.kuwo.cn/pic.web?corp=kuwo&type=rid_pic&pictype=500&size=500&rid=7"
    );
    let ProviderResponse::Lyrics(lyrics) = execute(
        &fixture,
        ProviderRequest::Lyrics {
            track: track(),
            word_by_word: true,
        },
    ) else {
        panic!()
    };
    assert_eq!(
        fixture.request(1).url,
        "http://mlyric.kuwo.cn/mobi.s?f=web&type=lyric&lrcx=1&rid=7&encode=utf8"
    );
    assert!(lyrics.lyric.contains("[00:00.000]Helloworld"));
    assert!(
        lyrics
            .translated
            .unwrap()
            .contains("[00:00.000]Translation")
    );
    assert!(
        lyrics
            .word_by_word
            .unwrap()
            .contains("<40,20>Hello<80,40>world")
    );
    let ProviderResponse::Lyrics(lyrics) = execute(
        &fixture,
        ProviderRequest::Lyrics {
            track: track(),
            word_by_word: false,
        },
    ) else {
        panic!()
    };
    assert!(fixture.request(2).url.contains("lrcx=0"));
    assert!(lyrics.lyric.contains("[00:01.000]Line"));
    assert_eq!(lyrics.word_by_word, None);
    fixture.done();
}

#[test]
fn codec_invalid_envelopes_and_zero_word_divisors_fail_and_object_apostrophes_survive() {
    assert!(decode_lyric(b"invalid", true).is_err());
    assert!(decode_lyric(b"tp=content\r\n\r\ninvalid", true).is_err());
    assert!(parse_lyric("[ti:No lyric]").is_err());
    assert!(normalize_word_times("[kuwo:0]\n[00:01.00]<1,2>Line").is_err());
    assert!(decode_wbd("invalid").is_err());
    assert_eq!(
        parse_object_text("{'artist':'O'Neil'}").unwrap()["artist"],
        "O'Neil"
    );
    assert_eq!(
        normalize_word_times("[kuwo:15]\n[00:01.00]<60,20>A").unwrap(),
        "[00:01.00]<40,6>A"
    );
}

#[test]
fn missing_kuwo_lyric_sentinel_is_an_availability_error_and_not_an_empty_success() {
    for sentinel in [b"TP=none\n".as_slice(), b"tp=none\r\n".as_slice()] {
        assert!(matches!(
            decode_lyric(sentinel, true),
            Err(ProviderError::Upstream { .. })
        ));
    }
    assert!(matches!(
        decode_lyric(b"bad envelope", true),
        Err(ProviderError::Parse { .. })
    ));
}
