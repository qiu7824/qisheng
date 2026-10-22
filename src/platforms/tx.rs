//! QQ Music's active LX 2.12.6 catalog routes.
use super::types::*;
use crate::domain::music::{LxMusicDtoVersion, TrackRecord};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::sync::{Arc, LazyLock};

const MUSICU: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const OLD_UA: &str = "Mozilla/5.0 (compatible; MSIE 9.0; Windows NT 6.1; WOW64; Trident/5.0)";
const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/113.0.0.0 Safari/537.36 Edg/113.0.0.0";
static HTML_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static HTML_ENTITIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&#(x[0-9a-fA-F]+|\d+);").unwrap());
static UNKNOWN_EMOJI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[em\]e\d+\[/em\]").unwrap());

pub struct TxProvider {
    transport: Arc<dyn HttpTransport>,
}
impl TxProvider {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
    fn get(&self, url: String, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        json(self.transport.as_ref(), HttpRequest::get(url), ctx)
    }
    fn post(&self, body: Value, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        self.request(
            HttpRequest::post(MUSICU, bytes(&body)?)
                .header("Content-Type", "application/json")
                .header("User-Agent", OLD_UA),
            ctx,
        )
    }
    fn request(&self, request: HttpRequest, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        json(self.transport.as_ref(), request, ctx)
    }
    fn music_get(&self, body: Value, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        self.get(format!("{MUSICU}?loginUin=0&hostUin=0&format=json&inCharset=utf-8&outCharset=utf-8&notice=0&platform=wk_v15.json&needNewCode=0&data={}", encode(&serde_json::to_string(&body).map_err(|e|ProviderError::parse(e.to_string()))?)),ctx)
    }
    fn detail(&self, mid: &str, ctx: &ProviderContext) -> Result<TrackRecord, ProviderError> {
        let body=self.post(json!({"comm":{"ct":"19","cv":"1859","uin":"0"},"req":{"module":"music.pf_song_detail_svr","method":"get_song_detail_yqq","param":{"song_type":0,"song_mid":mid}}}),ctx)?;
        success(&body, Some("req"))?;
        track(&body["req"]["data"]["track_info"])
    }
    /// Fetch a bounded group of exact QQ song MIDs in one catalog request.
    /// Results retain input positions, including individual unavailable songs.
    pub fn detail_batch(
        &self,
        mids: &[String],
        ctx: &ProviderContext,
    ) -> Result<Vec<Result<TrackRecord, ProviderError>>, ProviderError> {
        ctx.check()?;
        if mids.is_empty()
            || mids.len() > 10
            || mids.iter().any(|mid| {
                mid.is_empty()
                    || mid.len() > 64
                    || !mid.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        {
            return Err(ProviderError::invalid(
                "QQ detail batch requires 1..=10 valid song MIDs",
            ));
        }
        let mut request = json!({"comm":{"ct":"19","cv":"1859","uin":"0"}});
        for (index, mid) in mids.iter().enumerate() {
            request[format!("req_{index}")] = json!({"module":"music.pf_song_detail_svr","method":"get_song_detail_yqq","param":{"song_type":0,"song_mid":mid}});
        }
        let response = self.post(request, ctx)?;
        success(&response, None)?;
        Ok(mids
            .iter()
            .enumerate()
            .map(|(index, mid)| {
                let key = format!("req_{index}");
                success(&response, Some(&key))?;
                let item = &response[&key]["data"]["track_info"];
                if item["mid"].as_str() != Some(mid.as_str()) {
                    return Err(ProviderError::parse(
                        "QQ detail response MID does not match request",
                    ));
                }
                track(item)
            })
            .collect())
    }
    fn numeric_id(&self, id: &str, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(id
                .parse::<u64>()
                .map(Value::from)
                .unwrap_or_else(|_| Value::String(id.into())));
        }
        self.detail(id, ctx)?
            .metadata
            .get("id")
            .cloned()
            .ok_or_else(|| ProviderError::parse("QQ track numeric id is missing"))
    }
    fn text_request(&self, url: &str, ctx: &ProviderContext) -> Result<String, ProviderError> {
        ctx.check()?;
        let response = self.transport.execute(HttpRequest::get(url), ctx)?;
        ctx.check()?;
        if response.status != 200 {
            return Err(ProviderError::Http {
                status: response.status,
                message: "QQ HTML request failed".into(),
            });
        }
        response.text()
    }
    fn playlist_id(&self, id: &str, ctx: &ProviderContext) -> Result<String, ProviderError> {
        fn numeric(value: &str) -> bool {
            !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit())
        }
        if numeric(id) {
            return Ok(id.into());
        }
        let mut url = reqwest::Url::parse(id)
            .map_err(|_| ProviderError::invalid("Invalid QQ playlist link"))?;
        let mut seen = std::collections::HashSet::new();
        for hop in 0..=5 {
            ctx.check()?;
            let host = url.host_str().unwrap_or("");
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.port().is_some()
                || !(host == "y.qq.com" || host.ends_with(".y.qq.com"))
            {
                return Err(ProviderError::invalid("Invalid QQ playlist sharing host"));
            }
            if let Some((_, value)) = url
                .query_pairs()
                .find(|(key, value)| (key == "id" || key == "disstid") && numeric(value))
            {
                return Ok(value.into_owned());
            }
            let path = url.path().trim_end_matches('/');
            if let Some(value) = path.rsplit('/').next() {
                let value = value.strip_suffix(".html").unwrap_or(value);
                if numeric(value) {
                    return Ok(value.into());
                }
            }
            if hop == 5 || !seen.insert(url.to_string()) {
                break;
            }
            let response = self
                .transport
                .execute(HttpRequest::get(url.as_str()).without_redirects(), ctx)?;
            if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                break;
            }
            let Some(location) = response
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value)
            else {
                break;
            };
            url = url
                .join(location)
                .map_err(|_| ProviderError::invalid("Invalid QQ playlist redirect"))?;
        }
        Err(ProviderError::invalid(
            "QQ playlist id or share URL is invalid",
        ))
    }
    fn playlist_detail(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        let id = self.playlist_id(id, ctx)?;
        let url = format!(
            "https://c.y.qq.com/qzone/fcg-bin/fcg_ucc_getcdinfo_byids_cp.fcg?type=1&json=1&utf8=1&onlysong=0&new_format=1&disstid={}&loginUin=0&hostUin=0&format=json&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0",
            encode(&id)
        );
        let request = HttpRequest::get(url)
            .header("Origin", "https://y.qq.com")
            .header(
                "Referer",
                format!("https://y.qq.com/n/yqq/playsquare/{id}.html"),
            );
        let body = self.request(request, ctx)?;
        success(&body, None)?;
        let (songs, info, total) = if zero_code(&body["subcode"])
            && body["cdlist"].as_array().is_some_and(|a| !a.is_empty())
        {
            let item = &body["cdlist"][0];
            let songs = array(&item["songlist"])?;
            (
                songs.clone(),
                playlist_info(
                    &id, item, "dissname", "logo", "nickname", "desc", "visitnum",
                ),
                songs.len() as u64,
            )
        } else {
            let request=HttpRequest::post(MUSICU,bytes(&json!({"comm":{"cv":4747474,"ct":24,"format":"json","inCharset":"utf-8","outCharset":"utf-8","platform":"yqq.json","needNewCode":1,"uin":0},"req_1":{"module":"music.srfDissInfo.aiDissInfo","method":"uniform_get_Dissinfo","param":{"disstid":id.parse::<u64>().map_err(|_|ProviderError::invalid("invalid QQ playlist id"))?,"userinfo":1,"tag":1,"orderlist":1,"song_begin":0,"song_num":100000,"onlysonglist":0,"enc_host_uin":""}}}))?).header("Content-Type","application/json").header("Origin","https://y.qq.com").header("Referer",format!("https://y.qq.com/n/yqq/playsquare/{id}.html"));
            let fallback = self.request(request, ctx)?;
            success(&fallback, Some("req_1"))?;
            let data = &fallback["req_1"]["data"];
            (
                array(&data["songlist"])?.clone(),
                playlist_info(
                    &id,
                    &data["dirinfo"],
                    "title",
                    "picurl",
                    "host_nick",
                    "desc",
                    "listennum",
                ),
                number(&data["total_song_num"]),
            )
        };
        let tracks = tracks(&songs)?;
        Ok(ProviderResponse::Collection(Collection {
            info,
            tracks: slice_page(tracks, total, page, limit),
        }))
    }
}

impl Provider for TxProvider {
    fn platform(&self) -> Platform {
        Platform::Tx
    }
    fn execute(
        &self,
        request: ProviderRequest,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        ctx.check()?;
        match request {
            ProviderRequest::Search {
                keyword,
                page,
                limit,
            } => {
                page_args(page, limit)?;
                let body = json!({"comm":{"_channelid":"0","_os_version":"6.2.9200-2","ct":"19","cv":"2151","guid":"1F70E520B2EAA7D25E11760783C53CA9","patch":"118","psrf_access_token_expiresAt":0,"psrf_qqaccess_token":"","psrf_qqopenid":"","psrf_qqunionid":"","tmeAppID":"qqmusic","tmeLoginType":0,"uin":"0","wid":"7223299733393904640"},"music.search.SearchCgiService":{"module":"music.search.SearchCgiService","method":"DoSearchForQQMusicDesktop","param":{"grp":1,"num_per_page":limit,"page_num":page,"query":keyword,"remoteplace":"txt.newclient.top","search_type":0,"searchid":search_id()?}}});
                let payload = bytes(&body)?;
                let sign = zzc_sign(std::str::from_utf8(&payload).unwrap());
                let response = self.request(
                    HttpRequest::post(
                        format!("https://u.y.qq.com/cgi-bin/musics.fcg?sign={sign}"),
                        payload,
                    )
                    .header("Content-Type", "application/json")
                    .header("User-Agent", "QQMusic 14090508(android 12)"),
                    ctx,
                )?;
                let key = if response.get("music.search.SearchCgiService").is_some() {
                    "music.search.SearchCgiService"
                } else {
                    "req"
                };
                success(&response, Some(key))?;
                let data = &response[key]["data"];
                Ok(ProviderResponse::Tracks(Page::new(
                    tracks(array(&data["body"]["song"]["list"])?)?,
                    number(&data["meta"]["sum"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::HotSearch => {
                let body = json!({"comm":{"ct":"19","cv":"1803","guid":"0","patch":"118","psrf_access_token_expiresAt":0,"psrf_qqaccess_token":"","psrf_qqopenid":"","psrf_qqunionid":"","tmeAppID":"qqmusic","tmeLoginType":0,"uin":"0","wid":"0"},"hotkey":{"method":"GetHotkeyForQQMusicPC","module":"tencent_musicsoso_hotkey.HotkeyService","param":{"search_id":"","uin":0}}});
                let response = self.request(
                    HttpRequest::post(MUSICU, bytes(&body)?)
                        .header("Content-Type", "application/json")
                        .header("Referer", "https://y.qq.com/portal/player.html"),
                    ctx,
                )?;
                success(&response, Some("hotkey"))?;
                Ok(ProviderResponse::Keywords(
                    array(&response["hotkey"]["data"]["vec_hotkey"])?
                        .iter()
                        .map(|i| text(&i["query"]))
                        .collect(),
                ))
            }
            ProviderRequest::Detail { ids } => {
                let list = ids
                    .iter()
                    .map(|id| self.detail(id, ctx))
                    .collect::<Result<Vec<_>, _>>()?;
                let total = list.len() as u64;
                Ok(ProviderResponse::Tracks(Page::new(
                    list,
                    total,
                    1,
                    ids.len() as u32,
                )))
            }
            ProviderRequest::Charts => {
                let list: Value = serde_json::from_str(include_str!("tx/charts.json"))
                    .map_err(|e| ProviderError::parse(e.to_string()))?;
                Ok(ProviderResponse::Charts(
                    array(&list)?
                        .iter()
                        .map(|i| Chart {
                            id: text(&i["id"]),
                            name: text(&i["name"]),
                            image_url: None,
                            description: None,
                            extra: i.clone(),
                        })
                        .collect(),
                ))
            }
            ProviderRequest::ChartTracks { id, page, limit } => {
                page_args(page, limit)?;
                let id = id.strip_prefix("tx__").unwrap_or(&id);
                let numeric = id
                    .parse::<u64>()
                    .map_err(|_| ProviderError::invalid("invalid QQ chart id"))?;
                let html = self.text_request("https://c.y.qq.com/node/pc/wk_v15/top.html", ctx)?;
                let re = Regex::new(
                    r#"data-listname="[^"]+"\s+data-tid="[^"]*/([^"/]+)"\s+data-date="([^"]+)""#,
                )
                .unwrap();
                let period = re
                    .captures_iter(&html)
                    .find(|c| &c[1] == id)
                    .map(|c| c[2].to_owned())
                    .ok_or_else(|| ProviderError::parse("QQ chart period not found"))?;
                let body=self.post(json!({"toplist":{"module":"musicToplist.ToplistInfoServer","method":"GetDetail","param":{"topid":numeric,"num":300,"period":period}},"comm":{"uin":0,"format":"json","ct":20,"cv":1859}}),ctx)?;
                success(&body, Some("toplist"))?;
                let list = tracks(array(&body["toplist"]["data"]["songInfoList"])?)?;
                let total = list.len() as u64;
                Ok(ProviderResponse::Tracks(slice_page(
                    list, total, page, limit,
                )))
            }
            ProviderRequest::PlaylistTags => {
                let body=self.music_get(json!({"tags":{"method":"get_all_categories","param":{"qq":""},"module":"playlist.PlaylistAllCategoriesServer"}}),ctx)?;
                success(&body, Some("tags"))?;
                let mut tags = Vec::new();
                for group in array(&body["tags"]["data"]["v_group"])? {
                    for item in array(&group["v_item"])? {
                        tags.push(PlaylistTag{id:text(&item["id"]),name:text(&item["name"]),group:text(&group["group_name"]),extra:json!({"parent_id":group["group_id"],"parent_name":group["group_name"],"source":"tx"})});
                    }
                }
                let html = self.text_request(
                    "https://c.y.qq.com/node/pc/wk_v15/category_playlist.html",
                    ctx,
                )?;
                let re =
                    Regex::new(r#"class="c_bg_link js_tag_item"\s+data-id="(\w+)">([^<]+)</a>"#)
                        .unwrap();
                for c in re.captures_iter(&html) {
                    tags.push(PlaylistTag {
                        id: c[1].into(),
                        name: decode_name(&c[2]),
                        group: "hot".into(),
                        extra: json!({"hot":true,"source":"tx"}),
                    });
                }
                Ok(ProviderResponse::PlaylistTags(tags))
            }
            ProviderRequest::Playlists {
                tag,
                sort,
                page,
                limit,
            } => {
                page_args(page, limit)?;
                let recommend =
                    tag.is_empty() && (sort.is_empty() || sort == "-1" || sort == "recommend");
                let data = if recommend {
                    json!({"comm":{"cv":1602,"ct":20},"playlist":{"module":"music.playlist.PlaylistSquare","method":"GetRecommendWhole","param":{"IsReqFeed":true,"FeedReq":{"From":(u64::from(page)-1)*u64::from(limit),"Size":limit}}}})
                } else {
                    let id = if tag.is_empty() {
                        sort.as_str()
                    } else {
                        tag.as_str()
                    };
                    let id = id
                        .parse::<i64>()
                        .map_err(|_| ProviderError::invalid("invalid QQ playlist category"))?;
                    json!({"comm":{"cv":1602,"ct":20},"playlist":{"method":"get_category_content","param":{"titleid":id,"caller":"0","category_id":id,"size":limit,"page":page-1,"use_page":1},"module":"playlist.PlayListCategoryServer"}})
                };
                let body = self.music_get(data, ctx)?;
                success(&body, Some("playlist"))?;
                let data = &body["playlist"]["data"];
                let (items, total) = if recommend {
                    (
                        array(&data["FeedRsp"]["List"])?
                            .iter()
                            .map(|i| basic_playlist(&i["Playlist"]["basic"]))
                            .collect(),
                        number(&data["FeedRsp"]["FromLimit"]),
                    )
                } else {
                    (
                        array(&data["content"]["v_item"])?
                            .iter()
                            .map(|i| basic_playlist(&i["basic"]))
                            .collect(),
                        number(&data["content"]["total_cnt"]),
                    )
                };
                Ok(ProviderResponse::Playlists(Page::new(
                    items, total, page, limit,
                )))
            }
            ProviderRequest::SearchPlaylists {
                keyword,
                page,
                limit,
            } => {
                page_args(page, limit)?;
                let url = format!(
                    "http://c.y.qq.com/soso/fcgi-bin/client_music_search_songlist?page_no={}&num_per_page={limit}&format=json&query={}&remoteplace=txt.yqq.playlist&inCharset=utf8&outCharset=utf-8",
                    page - 1,
                    encode(&keyword)
                );
                let body = self.request(
                    HttpRequest::get(url)
                        .header("User-Agent", OLD_UA)
                        .header("Referer", "http://y.qq.com/portal/search.html"),
                    ctx,
                )?;
                success(&body, None)?;
                let items = array(&body["data"]["list"])?
                    .iter()
                    .map(|i| {
                        let mut info = playlist_info(
                            &text(&i["dissid"]),
                            i,
                            "dissname",
                            "imgurl",
                            "",
                            "introduction",
                            "listennum",
                        );
                        info.author = some_name(&i["creator"]["name"]);
                        info.track_count = Some(number(&i["song_count"]));
                        info
                    })
                    .collect();
                Ok(ProviderResponse::Playlists(Page::new(
                    items,
                    number(&body["data"]["sum"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::PlaylistDetail { id, page, limit } => {
                page_args(page, limit)?;
                self.playlist_detail(&id, page, limit, ctx)
            }
            ProviderRequest::Comments {
                id,
                kind,
                page,
                limit,
                ..
            } => {
                page_args(page, limit)?;
                if kind == CommentKind::Replies {
                    return Err(ProviderError::Unsupported {
                        platform: Platform::Tx,
                        operation: "replies (QQ returns embedded replies)".into(),
                    });
                }
                let song_id = self.numeric_id(&id, ctx)?;
                let (body, items, total) = if kind == CommentKind::Normal {
                    let form = format!(
                        "uin=0&format=json&cid=205360772&reqtype=2&biztype=1&topid={}&cmd=8&needmusiccrit=1&pagenum={}&pagesize={limit}",
                        encode(&text(&song_id)),
                        page - 1
                    );
                    let body = self.request(
                        HttpRequest::post(
                            "http://c.y.qq.com/base/fcgi-bin/fcg_global_comment_h5.fcg",
                            form.into_bytes(),
                        )
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .header("User-Agent", OLD_UA),
                        ctx,
                    )?;
                    success(&body, None)?;
                    let comments = array(&body["comment"]["commentlist"])?
                        .iter()
                        .map(normal_comment)
                        .collect();
                    let total = number(&body["comment"]["commenttotal"]);
                    (body, comments, total)
                } else {
                    let payload = json!({"comm":{"cv":4747474,"ct":24,"format":"json","inCharset":"utf-8","outCharset":"utf-8","notice":0,"platform":"yqq.json","needNewCode":1,"uin":0},"req":{"module":"music.globalComment.CommentRead","method":"GetHotCommentList","param":{"BizType":1,"BizId":text(&song_id),"LastCommentSeqNo":"","PageSize":limit,"PageNum":page-1,"HotType":1,"WithAirborne":0,"PicEnable":1}}});
                    let body = self.request(
                        HttpRequest::post(MUSICU, bytes(&payload)?)
                            .header("Content-Type", "application/json")
                            .header("User-Agent", WEB_UA)
                            .header("referer", "https://y.qq.com/")
                            .header("origin", "https://y.qq.com"),
                        ctx,
                    )?;
                    success(&body, Some("req"))?;
                    let comments = array(&body["req"]["data"]["CommentList"]["Comments"])?
                        .iter()
                        .map(hot_comment)
                        .collect();
                    let total = number(&body["req"]["data"]["CommentList"]["Total"]);
                    (body, comments, total)
                };
                let _ = body;
                Ok(ProviderResponse::Comments(Page::new(
                    items, total, page, limit,
                )))
            }
            ProviderRequest::Lyrics {
                track,
                word_by_word,
            } => {
                let song_id = track
                    .metadata
                    .get("id")
                    .filter(|i| !i.is_null())
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| self.numeric_id(&text(&track.platform.song_id), ctx))?;
                let payload = json!({"comm":{"ct":"19","cv":"1859","uin":"0"},"req":{"method":"GetPlayLyricInfo","module":"music.musichallSong.PlayLyricInfo","param":{"format":"json","crypt":1,"ct":19,"cv":1873,"interval":0,"lrc_t":0,"qrc":1,"qrc_t":0,"roma":1,"roma_t":0,"songID":song_id,"trans":1,"trans_t":0,"type":-1}}});
                let body=self.request(HttpRequest::post(MUSICU,bytes(&payload)?).header("Content-Type","application/json").header("referer","https://y.qq.com").header("user-agent","Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/86.0.4240.198 Safari/537.36"),ctx)?;
                success(&body, Some("req"))?;
                let data = &body["req"]["data"];
                let result = crate::lyrics::codec::decode_tx_lyrics(
                    &text(&data["lyric"]),
                    data["trans"].as_str(),
                    data["roma"].as_str(),
                )
                .map_err(|e| ProviderError::parse(e.to_string()))?;
                Ok(ProviderResponse::Lyrics(Lyrics {
                    lyric: result.lyric.text().unwrap_or_default().into(),
                    translated: result.tlyric.text().map(str::to_owned),
                    romanized: result.rlyric.text().map(str::to_owned),
                    word_by_word: if word_by_word {
                        result.lxlyric.text().map(str::to_owned)
                    } else {
                        None
                    },
                    extra: Value::Null,
                }))
            }
            ProviderRequest::Picture { track } => {
                let id = track
                    .metadata
                    .get("albumId")
                    .or_else(|| track.metadata.get("albumMid"))
                    .map(text)
                    .unwrap_or_default();
                if id.is_empty() {
                    return Err(ProviderError::invalid("QQ picture requires albumId"));
                }
                Ok(ProviderResponse::Picture(format!(
                    "https://y.gtimg.cn/music/photo_new/T002R500x500M000{id}.jpg"
                )))
            }
            other => Err(ProviderError::Unsupported {
                platform: Platform::Tx,
                operation: other.operation().into(),
            }),
        }
    }
}

fn bytes(value: &Value) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(value).map_err(|e| ProviderError::parse(e.to_string()))
}
fn array(value: &Value) -> Result<&Vec<Value>, ProviderError> {
    value
        .as_array()
        .ok_or_else(|| ProviderError::parse("QQ response list is missing or invalid"))
}
fn success(value: &Value, key: Option<&str>) -> Result<(), ProviderError> {
    if value.get("code").is_none()
        || !zero_code(&value["code"])
        || key.is_some_and(|key| {
            value.get(key).is_none() || value[key].get("code").is_some_and(|c| !zero_code(c))
        })
    {
        return Err(ProviderError::upstream(format!(
            "QQ response code: {}",
            value["code"]
        )));
    }
    Ok(())
}
fn zero_code(value: &Value) -> bool {
    value.as_f64() == Some(0.0) || value.as_str() == Some("0")
}
fn some_name(value: &Value) -> Option<String> {
    let s = decode_name(&text(value));
    (!s.is_empty()).then_some(s)
}
fn decode_name(value: &str) -> String {
    let without_tags = HTML_TAGS.replace_all(value, "");
    let mut out = without_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    out = HTML_ENTITIES
        .replace_all(&out, |c: &regex::Captures<'_>| {
            let code = if c[1].starts_with('x') {
                u32::from_str_radix(&c[1][1..], 16).ok()
            } else {
                c[1].parse::<u32>().ok()
            };
            code.and_then(char::from_u32)
                .map(|v| v.to_string())
                .unwrap_or_else(|| c[0].into())
        })
        .into_owned();
    out
}
fn size(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut n = value as f64;
    let mut unit = 0;
    while n >= 1024.0 && unit < 4 {
        n /= 1024.0;
        unit += 1;
    }
    format!("{n:.2} {}", UNITS[unit])
}
fn lx_interval(seconds: u64) -> Value {
    if seconds == 0 {
        Value::Null
    } else {
        json!(format!("{:02}:{:02}", seconds / 60, seconds % 60))
    }
}
fn track(item: &Value) -> Result<TrackRecord, ProviderError> {
    if text(&item["file"]["media_mid"]).is_empty() {
        return Err(ProviderError::parse("QQ track lacks file.media_mid"));
    }
    let mut qualities = Vec::new();
    let mut quality_map = serde_json::Map::new();
    for (kind, key) in [
        ("128k", "size_128mp3"),
        ("320k", "size_320mp3"),
        ("flac", "size_flac"),
        ("flac24bit", "size_hires"),
    ] {
        let bytes = number(&item["file"][key]);
        if bytes > 0 {
            let s = size(bytes);
            qualities.push(json!({"type":kind,"size":s}));
            quality_map.insert(kind.into(), json!({"size":s}));
        }
    }
    let album = text(&item["album"]["mid"]);
    let singer = item["singer"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| text(&i["name"]))
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();
    let img = if album.is_empty() || album == "空" {
        item["singer"]
            .as_array()
            .and_then(|a| a.first())
            .map(|i| {
                format!(
                    "https://y.gtimg.cn/music/photo_new/T001R500x500M000{}.jpg",
                    text(&i["mid"])
                )
            })
            .unwrap_or_default()
    } else {
        format!("https://y.gtimg.cn/music/photo_new/T002R500x500M000{album}.jpg")
    };
    TrackRecord::from_lx_dto(json!({"source":"tx","name":item["title"],"singer":singer,"albumName":item["album"]["name"],"albumId":item["album"]["mid"],"songId":item["id"],"songmid":item["mid"],"albumMid":item["album"]["mid"],"strMediaMid":item["file"]["media_mid"],"interval":lx_interval(number(&item["interval"])),"img":img,"types":qualities,"_types":quality_map,"typeUrl":{}}),LxMusicDtoVersion::Legacy).map_err(|e|ProviderError::parse(e.to_string()))
}
fn tracks(items: &[Value]) -> Result<Vec<TrackRecord>, ProviderError> {
    items
        .iter()
        .filter(|i| !text(&i["file"]["media_mid"]).is_empty())
        .map(track)
        .collect()
}
fn slice_page<T>(items: Vec<T>, total: u64, page: u32, limit: u32) -> Page<T> {
    let offset = u64::from(page - 1) * u64::from(limit);
    Page::new(
        items
            .into_iter()
            .skip(offset.min(usize::MAX as u64) as usize)
            .take(limit as usize)
            .collect(),
        total,
        page,
        limit,
    )
}
fn playlist_info(
    id: &str,
    i: &Value,
    name: &str,
    img: &str,
    author: &str,
    desc: &str,
    count: &str,
) -> Playlist {
    Playlist {
        id: id.into(),
        name: decode_name(&text(&i[name])),
        image_url: some_name(&i[img]),
        author: some_name(&i[author]),
        description: some_name(&Value::String(
            decode_name(&text(&i[desc])).replace("<br>", "\n"),
        )),
        play_count: i.get(count).map(number),
        track_count: None,
        extra: i.clone(),
    }
}
fn basic_playlist(i: &Value) -> Playlist {
    let mut result = playlist_info(&text(&i["tid"]), i, "title", "", "", "desc", "play_cnt");
    result.image_url =
        some_name(&i["cover"]["medium_url"]).or_else(|| some_name(&i["cover"]["default_url"]));
    result.author = some_name(&i["creator"]["nick"]);
    result.track_count = i.get("song_cnt").map(number);
    result
}
fn timestamp(i: &Value) -> Option<u64> {
    let n = number(i);
    (n >= 1_000_000_000).then(|| n.saturating_mul(1000))
}
fn emoji(value: &Value) -> String {
    let mut s = text(value).replace("\\n", "\n");
    for (key, emoji) in [
        ("e400846", "😘"),
        ("e400874", "😴"),
        ("e400825", "😃"),
        ("e400847", "😙"),
        ("e400835", "😍"),
        ("e400873", "😳"),
        ("e400836", "😎"),
        ("e400867", "😭"),
        ("e400832", "😊"),
        ("e400837", "😏"),
        ("e400875", "😫"),
        ("e400831", "😉"),
        ("e400855", "😡"),
        ("e400823", "😄"),
        ("e400862", "😨"),
        ("e400844", "😖"),
        ("e400841", "😓"),
        ("e400830", "😈"),
        ("e400828", "😆"),
        ("e400833", "😋"),
        ("e400822", "😀"),
        ("e400843", "😕"),
        ("e400829", "😇"),
        ("e400824", "😂"),
        ("e400834", "😌"),
        ("e400877", "😷"),
        ("e400132", "🍉"),
        ("e400181", "🍺"),
        ("e401067", "☕️"),
        ("e400186", "🥧"),
        ("e400343", "🐷"),
        ("e400116", "🌹"),
        ("e400126", "🍃"),
        ("e400613", "💋"),
        ("e401236", "❤️"),
        ("e400622", "💔"),
        ("e400637", "💣"),
        ("e400643", "💩"),
        ("e400773", "🔪"),
        ("e400102", "🌛"),
        ("e401328", "🌞"),
        ("e400420", "👏"),
        ("e400914", "🙌"),
        ("e400408", "👍"),
        ("e400414", "👎"),
        ("e401121", "✋"),
        ("e400396", "👋"),
        ("e400384", "👉"),
        ("e401115", "✊"),
        ("e400402", "👌"),
        ("e400905", "🙈"),
        ("e400906", "🙉"),
        ("e400907", "🙊"),
        ("e400562", "👻"),
        ("e400932", "🙏"),
        ("e400644", "💪"),
        ("e400611", "💉"),
        ("e400185", "🎁"),
        ("e400655", "💰"),
        ("e400325", "🐥"),
        ("e400612", "💊"),
        ("e400198", "🎉"),
        ("e401685", "⚡️"),
        ("e400631", "💝"),
        ("e400768", "🔥"),
        ("e400432", "👑"),
    ] {
        s = s.replace(&format!("[em]{key}[/em]"), emoji);
    }
    UNKNOWN_EMOJI.replace_all(&s, "").into_owned()
}
fn normal_comment(i: &Value) -> Comment {
    let mut replies = i["middlecommentcontent"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(first) = replies.first_mut() {
        first["avatarurl"] = i["avatarurl"].clone();
        first["praisenum"] = i["praisenum"].clone();
    }
    replies.reverse();
    let has_replies = !replies.is_empty();
    let root = text(&i["rootcommentid"]);
    let id = text(&i["commentid"]);
    let time = timestamp(&i["time"]);
    Comment {
        id: format!("{root}_{id}"),
        text: emoji(&i["rootcommentcontent"]),
        author: text(&i["rootcommentnick"]).chars().skip(1).collect(),
        avatar_url: if has_replies {
            None
        } else {
            some_name(&i["avatarurl"])
        },
        timestamp_ms: if root == id { time } else { None },
        likes: if has_replies {
            0
        } else {
            number(&i["praisenum"])
        },
        replies: replies
            .iter()
            .map(|c| Comment {
                id: format!("sub_{root}_{}", text(&c["subcommentid"])),
                text: emoji(&c["subcommentcontent"]),
                author: text(&c["replynick"]).chars().skip(1).collect(),
                avatar_url: some_name(&c["avatarurl"]),
                timestamp_ms: if text(&c["subcommentid"]) == id {
                    time
                } else {
                    None
                },
                likes: number(&c["praisenum"]),
                replies: Vec::new(),
                extra: c.clone(),
            })
            .collect(),
        extra: i.clone(),
    }
}
fn hot_comment(i: &Value) -> Comment {
    Comment {
        id: format!("{}_{}", text(&i["SeqNo"]), text(&i["CmId"])),
        text: emoji(&i["Content"]),
        author: text(&i["Nick"]),
        avatar_url: some_name(&i["Avatar"]),
        timestamp_ms: timestamp(&i["PubTime"]),
        likes: number(&i["PraiseNum"]),
        replies: i["SubComments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| {
                        let mut comment = hot_comment(c);
                        comment.id = format!("sub_{}", comment.id);
                        comment
                    })
                    .collect()
            })
            .unwrap_or_default(),
        extra: i.clone(),
    }
}
fn search_id() -> Result<String, ProviderError> {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).map_err(|e| ProviderError::invalid(e.to_string()))?;
    let mut result = bytes[..16]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<String>();
    result.push_str(&format!(
        "{:05}",
        u32::from_le_bytes(bytes[16..].try_into().unwrap()) % 100000
    ));
    Ok(result)
}
/// LX's PC search signature, including the out-of-range SHA1 index omission.
pub fn zzc_sign(input: &str) -> String {
    let hash = format!("{:x}", Sha1::digest(input.as_bytes()));
    let pick = |indexes: &[usize]| {
        indexes
            .iter()
            .filter_map(|i| hash.as_bytes().get(*i))
            .map(|b| *b as char)
            .collect::<String>()
    };
    let scrambled = [
        89, 39, 179, 150, 218, 82, 58, 252, 177, 52, 186, 123, 120, 64, 242, 133, 143, 161, 121,
        179,
    ]
    .iter()
    .enumerate()
    .map(|(i, key)| key ^ u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16).unwrap())
    .collect::<Vec<_>>();
    format!(
        "zzc{}{}{}",
        pick(&[23, 14, 6, 36, 16, 40, 7, 19]),
        STANDARD
            .encode(scrambled)
            .replace(['\\', '/', '+', '='], ""),
        pick(&[16, 1, 32, 12, 19, 27, 8, 5])
    )
    .to_lowercase()
}

#[cfg(test)]
mod playlist_link_tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex, time::Duration};
    struct Redirects(Mutex<VecDeque<(String, String)>>);
    impl HttpTransport for Redirects {
        fn execute(
            &self,
            request: HttpRequest,
            _: &ProviderContext,
        ) -> Result<HttpResponse, ProviderError> {
            assert!(
                !request.follow_redirects,
                "redirect destinations must be validated before fetching"
            );
            let (url, location) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected network request");
            assert_eq!(request.url, url);
            Ok(HttpResponse {
                status: 302,
                headers: [("Location".into(), location)].into(),
                bytes: Vec::new(),
            })
        }
    }
    #[test]
    fn mobile_and_short_sharing_links_resolve_the_same_id() {
        let transport = Arc::new(Redirects(Mutex::new(VecDeque::from([
            (
                "https://c6.y.qq.com/share?token=example".into(),
                "/next".into(),
            ),
            (
                "https://c6.y.qq.com/next".into(),
                "https://i.y.qq.com/n2/m/share/details/taoge.html?id=123456".into(),
            ),
        ]))));
        let provider = TxProvider::new(transport.clone());
        let ctx = ProviderContext::new(Duration::from_secs(1));
        assert_eq!(
            provider
                .playlist_id("https://c6.y.qq.com/share?token=example", &ctx)
                .unwrap(),
            "123456"
        );
        for link in [
            "https://i2.y.qq.com/n3/other/pages/details/playlist.html?id=123456",
            "https://y.qq.com/n/yqq/playsquare/123456.html",
            "https://y.qq.com/playlist/123456",
            "https://y.qq.com/list?disstid=123456",
        ] {
            assert_eq!(provider.playlist_id(link, &ctx).unwrap(), "123456");
        }
        assert!(transport.0.lock().unwrap().is_empty());
    }
    #[test]
    fn sharing_redirects_reject_other_hosts_credentials_and_cycles() {
        for destination in [
            "https://y.qq.com.evil.test/list?id=123",
            "http://127.0.0.1/?id=123",
            "https://user:pass@y.qq.com/?id=123",
            "https://y.qq.com:9443/?id=123",
            "https://c6.y.qq.com/share",
        ] {
            let transport = Arc::new(Redirects(Mutex::new(VecDeque::from([(
                "https://c6.y.qq.com/share".into(),
                destination.into(),
            )]))));
            assert!(
                TxProvider::new(transport.clone())
                    .playlist_id(
                        "https://c6.y.qq.com/share",
                        &ProviderContext::new(Duration::from_secs(1))
                    )
                    .is_err()
            );
            assert!(transport.0.lock().unwrap().is_empty());
        }
    }
}
