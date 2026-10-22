use super::types::*;
use crate::domain::music::{LxMusicDtoVersion, TrackRecord};
use aes::{
    Aes128,
    cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::read::ZlibDecoder;
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    io::Read,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct KwProvider {
    transport: Arc<dyn HttpTransport>,
    www_token: Mutex<Option<WwwPublicToken>>,
}
struct WwwPublicToken {
    value: String,
    obtained: Instant,
}
const WWW_PUBLIC_COOKIE: &str = "Hm_Iuvt_cdb524f42f23cer9b268564v7y735ewrq2324";
const WWW_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140.0.0.0 Safari/537.36";
impl KwProvider {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            www_token: Mutex::new(None),
        }
    }

    fn www_public_token(
        &self,
        id: &str,
        refresh: bool,
        context: &ProviderContext,
    ) -> Result<String, ProviderError> {
        if !refresh
            && let Some(token) = self
                .www_token
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
            && token.obtained.elapsed() < Duration::from_secs(300)
        {
            return Ok(token.value.clone());
        }
        let response = self.raw(
            HttpRequest::get(format!("https://www.kuwo.cn/play_detail/{}", encode(id)))
                .header("User-Agent", WWW_USER_AGENT)
                .header("Referer", "https://www.kuwo.cn/"),
            context,
        )?;
        let token = www_public_cookie(&response.headers).ok_or_else(|| {
            ProviderError::upstream("Kuwo official page did not return its anonymous request token")
        })?;
        *self
            .www_token
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(WwwPublicToken {
            value: token.clone(),
            obtained: Instant::now(),
        });
        Ok(token)
    }

    fn detail(&self, id: &str, context: &ProviderContext) -> Result<TrackRecord, ProviderError> {
        let mut body = self.get(
            format!(
                "http://www.kuwo.cn/api/www/music/musicInfo?mid={}",
                encode(id)
            ),
            context,
        )?;
        if www_illegal(&body) {
            for refresh in [false, true] {
                let token = self.www_public_token(id, refresh, context)?;
                let mut random = [0u8; 16];
                getrandom::fill(&mut random).map_err(|error| ProviderError::Network {
                    message: error.to_string(),
                })?;
                let salt = u32::from_le_bytes(random[..4].try_into().unwrap()) % 100_000_000;
                let req_id = uuid_from_random(random);
                let request=HttpRequest::get(format!("https://www.kuwo.cn/api/www/music/musicInfo?mid={}&httpsStatus=1&reqId={req_id}&plat=web_www&from=",encode(id)))
                    .header("User-Agent",WWW_USER_AGENT).header("Referer",format!("https://www.kuwo.cn/play_detail/{}",encode(id)))
                    .header("Cookie",format!("{WWW_PUBLIC_COOKIE}={token}")).header("Secret",build_www_secret(&token,salt)?);
                let response = self.raw(request, context)?;
                body = response.json()?;
                if let Some(token) = www_public_cookie(&response.headers) {
                    *self
                        .www_token
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(WwwPublicToken {
                        value: token,
                        obtained: Instant::now(),
                    });
                }
                if !www_illegal(&body) {
                    break;
                }
            }
        }
        if !matches!(number(&body["code"]), 200 | 2001) {
            return Err(ProviderError::upstream(
                nonempty(&body["msg"])
                    .or_else(|| nonempty(&body["message"]))
                    .unwrap_or_else(|| "Kuwo musicInfo response has no success code".into()),
            ));
        }
        let mut data = body["data"].clone();
        if !data.is_object() || data["name"].as_str().is_none_or(|name| name.is_empty()) {
            return Err(ProviderError::parse("Kuwo musicInfo data/name missing"));
        }
        if data["rid"].is_null() && data["id"].is_null() {
            data["id"] = json!(id);
        }
        let result = make_track(&data, TrackShape::Detail)?;
        if text(&result.platform.song_id) != id {
            return Err(ProviderError::parse(
                "Kuwo musicInfo returned a different track identifier",
            ));
        }
        Ok(result)
    }
    fn get(&self, url: String, context: &ProviderContext) -> Result<Value, ProviderError> {
        json(self.transport.as_ref(), HttpRequest::get(url), context)
    }
    fn raw(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        let result = self.transport.execute(request, context)?;
        context.check()?;
        if result.status != 200 {
            return Err(ProviderError::Http {
                status: result.status,
                message: "Kuwo request failed".into(),
            });
        }
        Ok(result)
    }
    fn loose(&self, url: String, context: &ProviderContext) -> Result<Value, ProviderError> {
        parse_object_text(&self.raw(HttpRequest::get(url), context)?.text()?)
    }
    fn search(
        &self,
        keyword: String,
        page: u32,
        limit: u32,
        context: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        page_args(page, limit)?;
        let url = format!(
            "http://search.kuwo.cn/r.s?client=kt&all={}&pn={}&rn={limit}&uid=794762570&ver=kwplayer_ar_9.2.2.1&vipver=1&show_copyright_off=1&newver=1&ft=music&cluster=0&strategy=2012&encoding=utf8&rformat=json&vermerge=1&mobi=1&issubtitle=1",
            encode(&keyword),
            page - 1
        );
        let mut last = ProviderError::upstream("Kuwo search returned invalid data");
        for _ in 0..3 {
            context.check()?;
            let data = self.get(url.clone(), context)?;
            if text(&data["TOTAL"]) != "0" && text(&data["SHOW"]) == "0" {
                continue;
            }
            let tracks = if data["abslist"].is_null() {
                Ok(Vec::new())
            } else {
                parse_tracks(&data["abslist"], TrackShape::Search)
            };
            match tracks {
                Ok(items) => {
                    return Ok(ProviderResponse::Tracks(Page::new(
                        items,
                        number(&data["TOTAL"]),
                        page,
                        limit,
                    )));
                }
                Err(err) => last = err,
            }
        }
        Err(last)
    }
    fn album(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        context: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        page_args(page, limit)?;
        let body=self.loose(format!("http://search.kuwo.cn/r.s?pn={}&rn={limit}&stype=albuminfo&albumid={}&show_copyright_off=0&encoding=utf&vipver=MUSIC_9.1.0",page-1,encode(id)),context)?;
        let musiclist = body["musiclist"]
            .as_array()
            .ok_or_else(|| ProviderError::parse("Kuwo album musiclist missing"))?;
        let album_name = decode_html(&text(&body["name"]));
        let items = musiclist
            .iter()
            .map(|item| {
                make_track(
                    item,
                    TrackShape::Album {
                        name: &album_name,
                        id: &body["albumid"],
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ProviderResponse::Collection(Collection {
            info: Playlist {
                id: format!("digest-13__{id}"),
                name: album_name,
                image_url: nonempty(&body["img"]).or_else(|| nonempty(&body["hts_img"])),
                author: Some(decode_html(&text(&body["artist"]))),
                description: Some(decode_html(&text(&body["info"]))),
                play_count: None,
                track_count: Some(number(&body["songnum"])),
                extra: body.clone(),
            },
            tracks: Page::new(items, number(&body["songnum"]), page, limit),
        }))
    }
    fn playlist_detail(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        context: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        page_args(page, limit)?;
        if id.contains("/bodian/") {
            return self.bodian(id, page, limit, context);
        }
        let mut id = id.to_owned();
        let mut digest = String::from("8");
        if let Some(rest) = id.strip_prefix("digest-") {
            let (value, key) = rest.split_once("__").ok_or_else(|| {
                ProviderError::invalid("invalid Kuwo digest collection identifier")
            })?;
            digest = value.into();
            id = key.into();
        } else if id.contains(['?', '&', ':', '/']) {
            let re = Regex::new(r"^.+/playlist(?:_detail)?/(\d+)(?:\?.*|&.*|#.*|$)").unwrap();
            id = re
                .captures(&id)
                .and_then(|c| c.get(1))
                .map(|s| s.as_str().to_owned())
                .ok_or_else(|| ProviderError::invalid("invalid Kuwo playlist URL"))?;
        }
        if digest == "13" {
            return self.album(&id, page, limit, context);
        }
        if digest != "8" {
            let body=self.get(format!("http://qukudata.kuwo.cn/q.k?op=query&cont=ninfo&node={}&pn=0&rn=1&fmt=json&src=mbox&level=2",encode(&id)),context)?;
            let children = body["child"]
                .as_array()
                .ok_or_else(|| ProviderError::parse("Kuwo digest-5 child missing"))?;
            id = children
                .first()
                .and_then(|v| nonempty(&v["sourceid"]))
                .ok_or_else(|| {
                    ProviderError::upstream("Kuwo collection node has no source playlist")
                })?;
        }
        let url = if digest == "8" {
            format!(
                "http://nplserver.kuwo.cn/pl.svc?op=getlistinfo&pid={}&pn={}&rn={limit}&encode=utf8&keyset=pl2012&identity=kuwo&pcmp4=1&vipver=MUSIC_9.0.5.0_W1&newver=1",
                encode(&id),
                page - 1
            )
        } else {
            // Preserve the active pinned SDK's digest-5 pagination wire format.
            format!(
                "http://nplserver.kuwo.cn/pl.svc?op=getlistinfo&pid={}&pn={}}}&rn={limit}&encode=utf-8&keyset=pl2012&identity=kuwo&pcmp4=1",
                encode(&id),
                page - 1
            )
        };
        let body = self.get(url, context)?;
        if body["result"] != "ok" {
            return Err(ProviderError::upstream("Kuwo playlist getlistinfo failed"));
        }
        let tracks = parse_tracks(&body["musiclist"], TrackShape::List)?;
        let info = Playlist {
            id,
            name: text(&body["title"]),
            image_url: nonempty(&body["pic"]),
            author: nonempty(&body["uname"]),
            description: nonempty(&body["info"]),
            play_count: Some(number(&body["playnum"])),
            track_count: Some(number(&body["total"])),
            extra: body.clone(),
        };
        Ok(ProviderResponse::Collection(Collection {
            info,
            tracks: Page::new(tracks, number(&body["total"]), page, limit),
        }))
    }
    fn bodian(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        context: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        let url = reqwest::Url::parse(id).map_err(|e| ProviderError::invalid(e.to_string()))?;
        let args = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        let list_id = args
            .get("playlistId")
            .ok_or_else(|| ProviderError::invalid("Bodian playlistId missing"))?;
        let source = args
            .get("source")
            .ok_or_else(|| ProviderError::invalid("Bodian source missing"))?;
        let uid = args.get("uid").unwrap_or(list_id);
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|e| ProviderError::Network {
            message: e.to_string(),
        })?;
        let req_id = entropy
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let request = |url: String| {
            HttpRequest::get(url).header("User-Agent","Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/86.0.4240.198 Safari/537.36").header("plat","h5")
        };
        let body = json(
            self.transport.as_ref(),
            request(format!(
                "https://bd-api.kuwo.cn/api/service/playlist/{}/musicList?reqId={req_id}&source={}&pn={page}&rn={limit}",
                encode(list_id),
                encode(source)
            )),
            context,
        )?;
        require_code(&body)?;
        let data = &body["data"];
        let tracks = parse_tracks(&data["list"], TrackShape::Bodian)?;
        let info_url = match source.as_ref() {
            "4" => Some(format!(
                "https://bd-api.kuwo.cn/api/service/playlist/info/{}?reqId={req_id}&source={}",
                encode(list_id),
                encode(source)
            )),
            "5" => Some(format!(
                "https://bd-api.kuwo.cn/api/ucenter/users/pub/{}?reqId={req_id}",
                encode(uid)
            )),
            _ => None,
        };
        let mut info = Playlist {
            id: id.into(),
            name: String::new(),
            image_url: None,
            author: None,
            description: None,
            play_count: None,
            track_count: Some(number(&data["total"])),
            extra: Value::Null,
        };
        if let Some(url) = info_url {
            match json(self.transport.as_ref(), request(url), context) {
                Ok(metadata) if number(&metadata["code"]) == 200 => {
                    let details = &metadata["data"];
                    if source.as_ref() == "4" {
                        info.name = text(&details["name"]);
                        info.image_url = nonempty(&details["pic"]);
                        info.description = nonempty(&details["description"]);
                        info.author = nonempty(&details["creatorName"]);
                        info.play_count = Some(number(&details["playNum"]));
                    } else {
                        info.name = format!("{}喜欢的音乐", text(&details["userInfo"]["nickname"]));
                        info.author = nonempty(&details["userInfo"]["nickname"]);
                        info.image_url = nonempty(&details["userInfo"]["headImg"]);
                    }
                    info.extra = metadata;
                }
                Err(error @ (ProviderError::Cancelled | ProviderError::DeadlineExceeded)) => {
                    return Err(error);
                }
                _ => {}
            }
        }
        Ok(ProviderResponse::Collection(Collection {
            info,
            tracks: Page::new(tracks, number(&data["total"]), page, limit),
        }))
    }
}

impl Provider for KwProvider {
    fn platform(&self) -> Platform {
        Platform::Kw
    }
    fn execute(
        &self,
        request: ProviderRequest,
        context: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        context.check()?;
        let result = match request {
            ProviderRequest::Search {
                keyword,
                page,
                limit,
            } => self.search(keyword, page, limit, context),
            ProviderRequest::HotSearch => {
                let body=json(self.transport.as_ref(),HttpRequest::get("http://hotword.kuwo.cn/hotword.s?prod=kwplayer_ar_9.3.0.1&corp=kuwo&newver=2&vipver=9.3.0.1&source=kwplayer_ar_9.3.0.1_40.apk&p2p=1&notrace=0&uid=0&plat=kwplayer_ar&rformat=json&encoding=utf8&tabid=1").header("User-Agent","Dalvik/2.1.0 (Linux; U; Android 9;)"),context)?;
                if body["status"] != "ok" {
                    return Err(ProviderError::upstream("Kuwo hotword status failed"));
                }
                Ok(ProviderResponse::Keywords(
                    array(&body["tagvalue"], "tagvalue")?
                        .iter()
                        .map(|v| text(&v["key"]))
                        .collect(),
                ))
            }
            ProviderRequest::Suggest { keyword } => {
                let body=json(self.transport.as_ref(),HttpRequest::get(format!("https://tips.kuwo.cn/t.s?corp=kuwo&newver=3&p2p=1&notrace=0&c=mbox&w={}&encoding=utf8&rformat=json",encode(&keyword))).header("Referer","http://www.kuwo.cn/"),context)?;
                Ok(ProviderResponse::Keywords(
                    array(&body["WORDITEMS"], "WORDITEMS")?
                        .iter()
                        .map(|v| text(&v["RELWORD"]))
                        .collect(),
                ))
            }
            ProviderRequest::Detail { ids } => {
                if ids.is_empty() || ids.len() > 1000 {
                    return Err(ProviderError::invalid("detail requires 1..=1000 IDs"));
                }
                let mut tracks = Vec::new();
                for id in &ids {
                    tracks.push(self.detail(id, context)?);
                }
                Ok(ProviderResponse::Tracks(Page::new(
                    tracks,
                    ids.len() as u64,
                    1,
                    ids.len() as u32,
                )))
            }
            ProviderRequest::Charts => Ok(ProviderResponse::Charts(
                CHARTS
                    .iter()
                    .map(|(id, name)| Chart {
                        id: format!("kw__{id}"),
                        name: (*name).into(),
                        image_url: None,
                        description: None,
                        extra: json!({"bangid":id}),
                    })
                    .collect(),
            )),
            ProviderRequest::ChartTracks { id, page, limit } => {
                page_args(page, limit)?;
                let id = id.strip_prefix("kw__").unwrap_or(&id);
                let request_body = json!({"uid":"","devId":"","sFrom":"kuwo_sdk","user_type":"AP","carSource":"kwplayercar_ar_6.0.1.0_apk_keluze.apk","id":id,"pn":page-1,"rn":limit});
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| ProviderError::invalid(e.to_string()))?
                    .as_millis() as u64;
                let param = build_wbd_param(&request_body, timestamp)?;
                let response = self.raw(
                    HttpRequest::get(format!("https://wbd.kuwo.cn/api/bd/bang/bang_info?{param}")),
                    context,
                )?;
                let body = decode_wbd(&response.text()?)?;
                require_code(&body)?;
                let data = &body["data"];
                Ok(ProviderResponse::Tracks(Page::new(
                    parse_tracks(&data["musiclist"], TrackShape::Chart)?,
                    number(&data["total"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::PlaylistTags => {
                let body=self.get("http://wapi.kuwo.cn/api/pc/classify/playlist/getTagList?cmd=rcm_keyword_playlist&user=0&prod=kwplayer_pc_9.0.5.0&vipver=9.0.5.0&source=kwplayer_pc_9.0.5.0&loginUid=0&loginSid=0&appUid=76039576".into(),context)?;
                require_code(&body)?;
                let mut tags = Vec::new();
                for group in array(&body["data"], "tag groups")? {
                    for item in array(&group["data"], "tag group items")? {
                        tags.push(PlaylistTag {
                            id: format!("{}-{}", text(&item["id"]), text(&item["digest"])),
                            name: text(&item["name"]),
                            group: text(&group["name"]),
                            extra: json!({"tag":item,"parent_id":group["id"],"hot":false}),
                        });
                    }
                }
                let hot=self.get("http://wapi.kuwo.cn/api/pc/classify/playlist/getRcmTagList?loginUid=0&loginSid=0&appUid=76039576".into(),context)?;
                require_code(&hot)?;
                for item in array(&hot["data"][0]["data"], "hot tags")? {
                    tags.push(PlaylistTag {
                        id: format!("{}-{}", text(&item["id"]), text(&item["digest"])),
                        name: text(&item["name"]),
                        group: "热门".into(),
                        extra: json!({"tag":item,"hot":true}),
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
                if !matches!(sort.as_str(), "hot" | "new" | "") {
                    return Err(ProviderError::invalid("Kuwo sort must be hot or new"));
                }
                let (tag_id, digest) = if tag.is_empty() {
                    ("", "")
                } else {
                    tag.split_once('-')
                        .ok_or_else(|| ProviderError::invalid("Kuwo tag requires id-digest"))?
                };
                let url = match digest {
                    "" => format!(
                        "http://wapi.kuwo.cn/api/pc/classify/playlist/getRcmPlayList?loginUid=0&loginSid=0&appUid=76039576&&pn={page}&rn={limit}&order={}",
                        encode(if sort.is_empty() { "hot" } else { &sort })
                    ),
                    "10000" => format!(
                        "http://wapi.kuwo.cn/api/pc/classify/playlist/getTagPlayList?loginUid=0&loginSid=0&appUid=76039576&pn={page}&id={}&rn={limit}",
                        encode(tag_id)
                    ),
                    "43" => format!(
                        "http://mobileinterfaces.kuwo.cn/er.s?type=get_pc_qz_data&f=web&id={}&prod=pc",
                        encode(tag_id)
                    ),
                    _ => {
                        return Err(ProviderError::Unsupported {
                            platform: Platform::Kw,
                            operation: format!("playlists tag digest {digest}"),
                        });
                    }
                };
                let body = self.get(url, context)?;
                if digest == "43" {
                    let mut items = Vec::new();
                    for group in array(&body, "playlist groups")? {
                        for item in array(&group["list"], "playlist group items")? {
                            if matches!(text(&item["type"]).as_str(), "songlist" | "list" | "album")
                            {
                                items.push(parse_playlist(item, false));
                            }
                        }
                    }
                    Ok(ProviderResponse::Playlists(Page::new(
                        items, 1000, page, 1000,
                    )))
                } else {
                    require_code(&body)?;
                    let data = &body["data"];
                    Ok(ProviderResponse::Playlists(Page::new(
                        array(&data["data"], "playlists")?
                            .iter()
                            .map(|v| parse_playlist(v, false))
                            .collect(),
                        number(&data["total"]),
                        page,
                        limit,
                    )))
                }
            }
            ProviderRequest::SearchPlaylists {
                keyword,
                page,
                limit,
            } => {
                page_args(page, limit)?;
                let body=self.loose(format!("http://search.kuwo.cn/r.s?all={}&pn={}&rn={limit}&rformat=json&encoding=utf8&ver=mbox&vipver=MUSIC_8.7.7.0_BCS37&plat=pc&devid=28156413&ft=playlist&pay=0&needliveshow=0",encode(&keyword),page-1),context)?;
                Ok(ProviderResponse::Playlists(Page::new(
                    array(&body["abslist"], "playlist search abslist")?
                        .iter()
                        .map(|v| parse_playlist(v, true))
                        .collect(),
                    number(&body["TOTAL"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::PlaylistDetail { id, page, limit } => {
                self.playlist_detail(&id, page, limit, context)
            }
            ProviderRequest::Album { id, page, limit } => self.album(&id, page, limit, context),
            ProviderRequest::Comments {
                id,
                kind,
                page,
                limit,
                cursor,
                root_comment_id: _,
            } => {
                page_args(page, limit)?;
                if kind == CommentKind::Replies {
                    return Err(ProviderError::Unsupported {
                        platform: Platform::Kw,
                        operation: "comment_replies".into(),
                    });
                }
                if cursor.is_some() {
                    return Err(ProviderError::Unsupported {
                        platform: Platform::Kw,
                        operation: "comments_cursor".into(),
                    });
                }
                let method = if kind == CommentKind::Hot {
                    "get_rec_comment"
                } else {
                    "get_comment"
                };
                let body=json(self.transport.as_ref(),HttpRequest::get(format!("http://ncomment.kuwo.cn/com.s?f=web&type={method}&aapiver=1&prod=kwplayer_ar_10.5.2.0&digest=15&sid={}&start={}&msgflag=1&count={limit}&newver=3&uid=0",encode(&id),(page as u64-1)*limit as u64)).header("User-Agent","Dalvik/2.1.0 (Linux; U; Android 9;)"),context)?;
                require_code(&body)?;
                let (list, total) = if kind == CommentKind::Hot {
                    (&body["hot_comments"], number(&body["hot_comments_counts"]))
                } else {
                    (&body["comments"], number(&body["comments_counts"]))
                };
                let comments = if list.is_null() {
                    Vec::new()
                } else {
                    array(list, "comments")?.iter().map(parse_comment).collect()
                };
                Ok(ProviderResponse::Comments(Page::new(
                    comments, total, page, limit,
                )))
            }
            ProviderRequest::Picture { track } => {
                let response=self.raw(HttpRequest::get(format!("http://artistpicserver.kuwo.cn/pic.web?corp=kuwo&type=rid_pic&pictype=500&size=500&rid={}",encode(&text(&track.platform.song_id)))),context)?;
                let picture = response.text()?.trim().to_owned();
                if !picture.starts_with("http://") && !picture.starts_with("https://") {
                    return Err(ProviderError::upstream("Kuwo picture URL missing"));
                }
                Ok(ProviderResponse::Picture(picture))
            }
            ProviderRequest::Lyrics {
                track,
                word_by_word,
            } => {
                let response = self.raw(
                    HttpRequest::get(format!(
                        "http://mlyric.kuwo.cn/mobi.s?f=web&type=lyric&lrcx={}&rid={}&encode=utf8",
                        u8::from(word_by_word),
                        encode(&text(&track.platform.song_id))
                    )),
                    context,
                )?;
                let content = decode_lyric(&response.bytes, word_by_word)?;
                Ok(ProviderResponse::Lyrics(parse_lyric(&content)?))
            }
        };
        context.check()?;
        result
    }
}

const CHARTS: &[(&str, &str)] = &[
    ("93", "飙升榜"),
    ("17", "新歌榜"),
    ("16", "热歌榜"),
    ("158", "抖音热歌榜"),
    ("292", "铃声榜"),
    ("284", "热评榜"),
    ("290", "ACG新歌榜"),
    ("286", "台湾KKBOX榜"),
    ("279", "冬日暖心榜"),
    ("281", "巴士随身听榜"),
    ("255", "KTV点唱榜"),
    ("280", "家务进行曲榜"),
    ("282", "熬夜修仙榜"),
    ("283", "枕边轻音乐榜"),
    ("278", "古风音乐榜"),
    ("264", "Vlog音乐榜"),
    ("242", "电音榜"),
    ("187", "流行趋势榜"),
    ("204", "现场音乐榜"),
    ("186", "ACG神曲榜"),
    ("185", "最强翻唱榜"),
    ("26", "经典怀旧榜"),
    ("104", "华语榜"),
    ("182", "粤语榜"),
    ("22", "欧美榜"),
    ("184", "韩语榜"),
    ("183", "日语榜"),
    ("145", "会员畅听榜"),
    ("153", "网红新歌榜"),
    ("64", "影视金曲榜"),
    ("176", "DJ嗨歌榜"),
    ("106", "真声音"),
    ("12", "Billboard榜"),
    ("49", "iTunes音乐榜"),
    ("180", "beatport电音榜"),
    ("13", "英国UK榜"),
    ("164", "百大DJ榜"),
    ("246", "YouTube音乐排行榜"),
    ("265", "韩国Genie榜"),
    ("14", "韩国M-net榜"),
    ("8", "香港电台榜"),
    ("15", "日本公信榜"),
    ("151", "腾讯音乐人原创榜"),
];

fn www_illegal(body: &Value) -> bool {
    body["success"] == false
        && body["message"]
            .as_str()
            .is_some_and(|message| message.to_ascii_lowercase().contains("request is illegal"))
}
fn www_public_cookie(headers: &std::collections::BTreeMap<String, String>) -> Option<String> {
    let prefix = format!("{WWW_PUBLIC_COOKIE}=");
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .flat_map(|(_, value)| value.split(','))
        .filter_map(|part| part.trim_start().strip_prefix(&prefix))
        .filter_map(|value| value.split(';').next())
        .find(|value| {
            (16..=128).contains(&value.len())
                && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(str::to_owned)
}
fn uuid_from_random(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Public WWW request-header encoding from the official site's request module.
/// The named cookie is obtained only from a fresh anonymous official-page response.
pub fn build_www_secret(token: &str, salt: u32) -> Result<String, ProviderError> {
    if !(16..=128).contains(&token.len())
        || !token.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || salt >= 100_000_000
    {
        return Err(ProviderError::invalid(
            "invalid Kuwo anonymous token or WWW salt",
        ));
    }
    // JS Number rounding and decimal/scientific string folding for this fixed
    // public key depend on the salt's decimal digit count, rather than its value.
    // These seeds preserve that wire behavior, including salts shorter than 8 digits.
    let seeds = [
        1_248_523_535u64,
        297_365_144,
        297_355_891,
        297_365_144,
        1_248_523_535,
        297_365_144,
        297_365_144,
        297_374_397,
    ];
    let mut seed = seeds[salt.to_string().len() - 1];
    let modulus = 2_147_483_647u64;
    let mut output = String::with_capacity(token.len() * 2 + 8);
    for byte in token.bytes() {
        let encoded = byte ^ ((seed * 255) / modulus) as u8;
        output.push_str(&format!("{encoded:02x}"));
        seed = (9253 * seed + 23) % modulus;
    }
    output.push_str(&format!("{salt:08x}"));
    Ok(output)
}

#[derive(Clone, Copy)]
enum TrackShape<'a> {
    Search,
    List,
    Chart,
    Detail,
    Bodian,
    Album { name: &'a str, id: &'a Value },
}
fn array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>, ProviderError> {
    value
        .as_array()
        .ok_or_else(|| ProviderError::parse(format!("Kuwo {field} must be an array")))
}
fn nonempty(value: &Value) -> Option<String> {
    let value = text(value);
    (!value.is_empty()).then_some(value)
}
fn require_code(body: &Value) -> Result<(), ProviderError> {
    if number(&body["code"]) == 200 {
        Ok(())
    } else {
        Err(ProviderError::upstream(
            nonempty(&body["msg"]).unwrap_or_else(|| format!("Kuwo code {}", text(&body["code"]))),
        ))
    }
}
fn parse_tracks(list: &Value, shape: TrackShape<'_>) -> Result<Vec<TrackRecord>, ProviderError> {
    array(list, "track list")?
        .iter()
        .map(|item| make_track(item, shape))
        .collect()
}

fn make_track(info: &Value, shape: TrackShape<'_>) -> Result<TrackRecord, ProviderError> {
    let null = Value::Null;
    let (id, title, singer, album, album_id, duration, picture, minfo) = match shape {
        TrackShape::Search => (
            json!(text(&info["MUSICRID"]).trim_start_matches("MUSIC_")),
            text(&info["SONGNAME"]),
            text(&info["ARTIST"]),
            text(&info["ALBUM"]),
            json!(decode_html(&text(&info["ALBUMID"]))),
            Some(number(&info["DURATION"])),
            Value::Null,
            &info["N_MINFO"],
        ),
        TrackShape::List => (
            info["id"].clone(),
            text(&info["name"]),
            text(&info["artist"]),
            text(&info["album"]),
            info["albumid"].clone(),
            Some(number(&info["duration"])),
            Value::Null,
            &info["N_MINFO"],
        ),
        TrackShape::Chart => (
            info["id"].clone(),
            text(&info["name"]),
            text(&info["artist"]),
            text(&info["album"]),
            info["albumId"].clone(),
            Some(number(&info["duration"])),
            info["pic"].clone(),
            &info["n_minfo"],
        ),
        TrackShape::Detail => (
            if info["rid"].is_null() {
                info["id"].clone()
            } else {
                info["rid"].clone()
            },
            text(&info["name"]),
            text(&info["artist"]),
            text(&info["album"]),
            info["albumid"].clone(),
            Some(number(&info["duration"])),
            info["pic"].clone(),
            &info["N_MINFO"],
        ),
        TrackShape::Bodian => (
            info["id"].clone(),
            text(&info["name"]),
            array(&info["artists"], "Bodian artists")?
                .iter()
                .map(|a| text(&a["name"]))
                .collect::<Vec<_>>()
                .join("、"),
            text(&info["album"]),
            info["albumId"].clone(),
            Some(number(&info["duration"])),
            info["albumPic"].clone(),
            &null,
        ),
        TrackShape::Album { name, id } => (
            info["id"].clone(),
            text(&info["name"]),
            text(&info["artist"]),
            name.into(),
            id.clone(),
            None,
            info["pic"].clone(),
            &null,
        ),
    };
    if id.is_null() || text(&id).is_empty() {
        return Err(ProviderError::parse("Kuwo track identifier missing"));
    }
    let mut qualities = Vec::<Value>::new();
    let mut detail = Map::new();
    let mut add = |kind: &str, size: Value, upper: bool| {
        let value = if upper {
            size.as_str()
                .map(|s| json!(s.to_uppercase()))
                .unwrap_or(size.clone())
        } else {
            size.clone()
        };
        qualities.push(json!({"type":kind,"size":value}));
        detail.insert(
            kind.into(),
            json!({"size":size.as_str().map(|s|json!(s.to_uppercase())).unwrap_or(size)}),
        );
    };
    match shape {
        TrackShape::Album { .. } => {
            let formats = text(&info["formats"]);
            for (tag, kind) in [
                ("MP3128", "128k"),
                ("MP3H", "320k"),
                ("ALFLAC", "flac"),
                ("HIRFLAC", "flac24bit"),
            ] {
                if formats.split('|').any(|v| v == tag) {
                    add(kind, Value::Null, false);
                }
            }
        }
        TrackShape::Bodian => {
            for audio in array(&info["audios"], "Bodian audio qualities")? {
                if let Some(kind) = quality(&text(&audio["bitrate"])) {
                    add(kind, audio["size"].clone(), true);
                }
            }
            qualities.reverse();
        }
        _ => {
            if minfo.is_null() && !matches!(shape, TrackShape::Detail) {
                return Err(ProviderError::parse("Kuwo N_MINFO missing"));
            }
            let mut seen = BTreeSet::new();
            for entry in text(minfo).split(';') {
                let fields = entry
                    .split(',')
                    .filter_map(|pair| pair.split_once(':'))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let Some(rate) = fields.get("bitrate") else {
                    continue;
                };
                if matches!(shape, TrackShape::Chart) && !seen.insert(*rate) {
                    continue;
                }
                if let Some(kind) = quality(rate) {
                    add(
                        kind,
                        fields.get("size").map(|v| json!(v)).unwrap_or(Value::Null),
                        matches!(shape, TrackShape::Chart),
                    );
                }
            }
            if matches!(shape, TrackShape::Chart) {
                qualities.sort_by_key(|v| match text(&v["type"]).as_str() {
                    "128k" => 1,
                    "320k" => 2,
                    "flac" => 3,
                    "flac24bit" => 4,
                    _ => 0,
                });
            } else {
                qualities.reverse();
            }
        }
    }
    let duration = duration.map(|n| {
        if n == 0 {
            "--/--".to_owned()
        } else {
            format!("{:02}:{:02}", n / 60, n % 60)
        }
    });
    let dto = json!({"name":decode_html(&title),"singer":decode_html(&singer).replace('&',"、"),"source":"kw","songmid":id,"albumId":album_id,"albumName":decode_html(&album),"interval":duration,"img":picture,"lrc":null,"otherSource":null,"types":qualities,"_types":detail,"typeUrl":{},"kwRaw":info});
    TrackRecord::from_lx_dto(dto, LxMusicDtoVersion::Legacy)
        .map_err(|e| ProviderError::parse(e.to_string()))
}
fn quality(rate: &str) -> Option<&'static str> {
    match rate {
        "4000" => Some("flac24bit"),
        "2000" => Some("flac"),
        "320" => Some("320k"),
        "128" => Some("128k"),
        _ => None,
    }
}
fn parse_playlist(v: &Value, search: bool) -> Playlist {
    Playlist {
        id: if search {
            text(&v["playlistid"])
        } else {
            format!("digest-{}__{}", text(&v["digest"]), text(&v["id"]))
        },
        name: decode_html(&text(&v["name"])),
        image_url: nonempty(if search { &v["pic"] } else { &v["img"] }),
        author: nonempty(if search { &v["nickname"] } else { &v["uname"] })
            .map(|s| decode_html(&s)),
        description: nonempty(if search { &v["intro"] } else { &v["desc"] })
            .map(|s| decode_html(&s)),
        play_count: Some(number(if search {
            &v["playcnt"]
        } else {
            &v["listencnt"]
        })),
        track_count: Some(number(if search { &v["songnum"] } else { &v["total"] })),
        extra: v.clone(),
    }
}
fn parse_comment(v: &Value) -> Comment {
    Comment {
        id: text(&v["id"]),
        text: text(&v["msg"]),
        author: text(&v["u_name"]),
        avatar_url: nonempty(&v["u_pic"]),
        timestamp_ms: Some(number(&v["time"]).saturating_mul(1000)),
        likes: number(&v["like_num"]),
        replies: v["child_comments"]
            .as_array()
            .map(|list| list.iter().map(parse_comment).collect())
            .unwrap_or_default(),
        extra: v.clone(),
    }
}

pub fn decode_html(value: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static ENTITIES: OnceLock<Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| Regex::new(r"</?[A-Za-z][^>]*>|<!--.*?-->").unwrap());
    let value = tags.replace_all(value, "");
    let entity = ENTITIES.get_or_init(|| {
        Regex::new(r"&(#x[0-9A-Fa-f]+|#[0-9]+|amp|lt|gt|quot|apos|nbsp);").unwrap()
    });
    entity
        .replace_all(&value, |captures: &regex::Captures<'_>| {
            match &captures[1] {
                "amp" => "&".into(),
                "lt" => "<".into(),
                "gt" => ">".into(),
                "quot" => "\"".into(),
                "apos" => "'".into(),
                "nbsp" => "\u{a0}".into(),
                code => code
                    .strip_prefix("#x")
                    .and_then(|v| u32::from_str_radix(v, 16).ok())
                    .or_else(|| code.strip_prefix('#').and_then(|v| v.parse().ok()))
                    .and_then(char::from_u32)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| captures[0].into()),
            }
        })
        .into_owned()
}

pub fn parse_object_text(input: &str) -> Result<Value, ProviderError> {
    if let Ok(v) = serde_json::from_str(input) {
        return Ok(v);
    }
    // Pinned Kuwo endpoints return an object literal with delimiter-position single quotes.
    let chars = input.chars().collect::<Vec<_>>();
    let mut out = String::new();
    let mut in_double = false;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' && !in_double {
            i += 1;
            let mut value = String::new();
            let mut closed = false;
            while i < chars.len() {
                let ch = chars[i];
                if ch == '\\' && i + 1 < chars.len() {
                    let decoded = match chars[i + 1] {
                        '\'' => Some('\''),
                        '"' => Some('"'),
                        '\\' => Some('\\'),
                        'n' => Some('\n'),
                        'r' => Some('\r'),
                        't' => Some('\t'),
                        _ => None,
                    };
                    if let Some(decoded) = decoded {
                        value.push(decoded);
                        i += 2;
                        continue;
                    }
                    value.push(ch);
                    i += 1;
                    continue;
                }
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace()).copied();
                if ch == '\'' && matches!(next, Some(':' | ',' | '}' | ']') | None) {
                    closed = true;
                    i += 1;
                    break;
                }
                value.push(ch);
                i += 1;
            }
            if !closed {
                return Err(ProviderError::parse(
                    "unterminated Kuwo single-quoted string",
                ));
            }
            out.push_str(
                &serde_json::to_string(&value).map_err(|e| ProviderError::parse(e.to_string()))?,
            );
            continue;
        }
        if ch == '"' && !escaped {
            in_double = !in_double;
        }
        out.push(ch);
        escaped = ch == '\\' && !escaped;
        i += 1;
    }
    serde_json::from_str(&out).map_err(|e| ProviderError::parse(format!("Kuwo object text: {e}")))
}

const WBD_KEY: [u8; 16] = [
    112, 87, 39, 61, 199, 250, 41, 191, 57, 68, 45, 114, 221, 94, 140, 228,
];
fn aes_ecb(data: &[u8], encrypt: bool) -> Result<Vec<u8>, ProviderError> {
    let cipher = Aes128::new(GenericArray::from_slice(&WBD_KEY));
    let mut output = data.to_vec();
    if encrypt {
        let padding = 16 - output.len() % 16;
        output.extend(std::iter::repeat_n(padding as u8, padding));
    }
    if output.is_empty() || !output.len().is_multiple_of(16) {
        return Err(ProviderError::parse("invalid WBD block length"));
    }
    for chunk in output.as_chunks_mut::<16>().0 {
        let block = GenericArray::from_mut_slice(chunk);
        if encrypt {
            cipher.encrypt_block(block);
        } else {
            cipher.decrypt_block(block);
        }
    }
    if !encrypt {
        let padding = *output.last().unwrap() as usize;
        if padding == 0
            || padding > 16
            || output[output.len() - padding..]
                .iter()
                .any(|b| *b != padding as u8)
        {
            return Err(ProviderError::parse("invalid WBD padding"));
        }
        output.truncate(output.len() - padding);
    }
    Ok(output)
}
pub fn build_wbd_param(data: &Value, timestamp: u64) -> Result<String, ProviderError> {
    let data = STANDARD.encode(aes_ecb(
        &serde_json::to_vec(data).map_err(|e| ProviderError::parse(e.to_string()))?,
        true,
    )?);
    let sign = format!(
        "{:X}",
        Md5::digest(format!("y67sprxhhpws{data}{timestamp}"))
    );
    Ok(format!(
        "data={}&time={timestamp}&appId=y67sprxhhpws&sign={sign}",
        encode(&data)
    ))
}
pub fn decode_wbd(data: &str) -> Result<Value, ProviderError> {
    let text = serde_json::from_str::<String>(data).unwrap_or_else(|_| data.to_owned());
    let decoded = percent_decode(text.trim())?;
    let ciphertext = STANDARD
        .decode(decoded)
        .map_err(|e| ProviderError::parse(e.to_string()))?;
    serde_json::from_slice(&aes_ecb(&ciphertext, false)?)
        .map_err(|e| ProviderError::parse(e.to_string()))
}
fn percent_decode(data: &str) -> Result<String, ProviderError> {
    let bytes = data.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(ProviderError::parse("invalid percent encoding"));
            }
            let value = u8::from_str_radix(&data[i + 1..i + 3], 16)
                .map_err(|e| ProviderError::parse(e.to_string()))?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|e| ProviderError::parse(e.to_string()))
}

pub fn decode_lyric(data: &[u8], word_by_word: bool) -> Result<String, ProviderError> {
    if data.trim_ascii().eq_ignore_ascii_case(b"tp=none") {
        return Err(ProviderError::upstream("Kuwo lyrics are unavailable"));
    }
    if data.len() < 10 || !data[..10].eq_ignore_ascii_case(b"tp=content") {
        return Err(ProviderError::parse("Kuwo lyric content envelope missing"));
    }
    let split = data
        .windows(4)
        .position(|s| s == b"\r\n\r\n")
        .ok_or_else(|| ProviderError::parse("Kuwo lyric envelope separator missing"))?;
    let mut output = Vec::new();
    ZlibDecoder::new(&data[split + 4..])
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut output)
        .map_err(|e| ProviderError::parse(e.to_string()))?;
    if output.len() > 8 * 1024 * 1024 {
        return Err(ProviderError::ResponseTooLarge {
            limit: 8 * 1024 * 1024,
        });
    }
    // The server can return an lrcx envelope even for a plain-LRC request.
    // The response header describes the wire encoding; the requested detail does not.
    let encoded = std::str::from_utf8(&data[..split])
        .ok()
        .and_then(|header| {
            header.lines().find_map(|line| {
                let (key, value) = line.trim().split_once('=')?;
                key.eq_ignore_ascii_case("lrcx")
                    .then(|| value.trim() == "1")
            })
        })
        .unwrap_or(word_by_word);
    if encoded {
        let text = String::from_utf8(output).map_err(|e| ProviderError::parse(e.to_string()))?;
        output = STANDARD
            .decode(text.trim())
            .map_err(|e| ProviderError::parse(e.to_string()))?;
        for (i, byte) in output.iter_mut().enumerate() {
            *byte ^= b"yeelion"[i % 7];
        }
    }
    String::from_utf8(output).map_err(|e| ProviderError::parse(e.to_string()))
}

#[cfg(test)]
mod lyric_envelope_tests {
    use super::*;
    use std::io::Write;
    fn envelope(text: &str, encoded: bool) -> Vec<u8> {
        let payload = if encoded {
            STANDARD
                .encode(
                    text.bytes()
                        .enumerate()
                        .map(|(i, b)| b ^ b"yeelion"[i % 7])
                        .collect::<Vec<_>>(),
                )
                .into_bytes()
        } else {
            text.as_bytes().to_vec()
        };
        let mut compressed =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        compressed.write_all(&payload).unwrap();
        let mut data = format!("TP=content\r\nLRCX={}\r\n\r\n", u8::from(encoded)).into_bytes();
        data.extend(compressed.finish().unwrap());
        data
    }
    #[test]
    fn response_encoding_overrides_the_requested_lyric_detail() {
        let text = "[ti:Encoding test]\n[00:01.00]第一句\n[00:02.50]第二句";
        for encoded in [false, true] {
            for requested_words in [false, true] {
                let decoded = decode_lyric(&envelope(text, encoded), requested_words).unwrap();
                assert_eq!(decoded, text);
                let result = parse_lyric(&decoded).unwrap();
                assert!(result.lyric.contains("[00:02.500]第二句"));
            }
        }
    }
    #[test]
    fn invalid_or_missing_lyrics_do_not_become_display_text() {
        assert!(decode_lyric(b"TP=none", false).is_err());
        assert!(decode_lyric(b"TP=content\r\nlrcx=1\r\n\r\ninvalid", false).is_err());
        assert!(parse_lyric("[ti:Title only]").is_err());
    }
}
pub fn parse_lyric(input: &str) -> Result<Lyrics, ProviderError> {
    let time = Regex::new(r"^\[([\d:.]*)\]").unwrap();
    let tags_re = Regex::new(r"\[(ver|ti|ar|al|offset|by|kuwo):\s*\S.*\]").unwrap();
    let words = Regex::new(r"<(-?\d+),(-?\d+)(?:,-?\d+)?>").unwrap();
    let mut tags = Vec::new();
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut translated = Vec::new();
    let mut seen = BTreeSet::new();
    let mut is_word = false;
    for line in input.lines() {
        let line = line.trim();
        if let Some(c) = time.captures(line) {
            let mut at = c[1].to_owned();
            if at.split_once('.').is_some_and(|(_, v)| v.len() == 2) {
                at.push('0');
            }
            let text = line[c[0].len()..].trim().to_owned();
            if seen.contains(&at) {
                if rows.len() < 2 {
                    continue;
                }
                let mut prior = rows.pop().unwrap();
                prior.0 = rows.last().unwrap().0.clone();
                translated.push(prior);
            } else {
                seen.insert(at.clone());
            }
            if text.starts_with('<') && words.is_match(&text) {
                is_word = true;
            }
            rows.push((at, text));
        } else if tags_re.is_match(line) {
            tags.push(line.to_owned());
        }
    }
    if rows.is_empty() {
        return Err(ProviderError::upstream("Kuwo timed lyrics missing"));
    }
    if !is_word
        && translated.len() as f64 > rows.len() as f64 * 0.3
        && rows.len().saturating_sub(translated.len()) > 6
    {
        return Err(ProviderError::parse(
            "Kuwo lyric translation layout invalid",
        ));
    }
    let render = |items: &[(String, String)], strip: bool| {
        format!(
            "{}\n{}",
            tags.join("\n"),
            items
                .iter()
                .map(|(at, content)| format!(
                    "[{at}]{}\n",
                    decode_html(&if strip {
                        words.replace_all(content, "").into_owned()
                    } else {
                        content.clone()
                    })
                ))
                .collect::<String>()
        )
    };
    let lyric = render(&rows, true);
    if !Regex::new(r"\[\d{1,2}:.*\d{1,4}\]")
        .unwrap()
        .is_match(&lyric)
    {
        return Err(ProviderError::upstream("Kuwo timed lyrics missing"));
    }
    let translated = (!translated.is_empty()).then(|| render(&translated, true));
    let word_by_word = if is_word {
        normalize_word_times(&render(&rows, false))
            .ok()
            .filter(|v| !v.is_empty())
    } else {
        None
    };
    Ok(Lyrics {
        lyric,
        translated,
        romanized: None,
        word_by_word,
        extra: json!({"source":"kw","translated_checked":true,"romanized_supported":false,"word_by_word_checked":true}),
    })
}

pub fn normalize_word_times(input: &str) -> Result<String, ProviderError> {
    let tags_re = Regex::new(r"\[(ver|ti|ar|al|offset|by|kuwo):\s*([^\]]+)\]").unwrap();
    let times = Regex::new(r"<(-?\d+),(-?\d+)(?:,-?\d+)?>").unwrap();
    let line_re = Regex::new(r"^\[\d{1,2}:.*\d{1,4}\]").unwrap();
    let mut offset = 1i64;
    let mut offset2 = 1i64;
    let mut tags = Vec::new();
    let mut rows = Vec::new();
    for line in input.lines() {
        if let Some(c) = tags_re.captures(line) {
            if &c[1] == "kuwo" {
                let value = i64::from_str_radix(c[2].trim(), 8)
                    .map_err(|e| ProviderError::parse(e.to_string()))?;
                offset = value / 10;
                offset2 = value % 10;
                if offset == 0 || offset2 == 0 {
                    return Err(ProviderError::parse("Kuwo word timing divisor is zero"));
                }
            } else {
                tags.push(line.to_owned());
            }
            continue;
        }
        if !line_re.is_match(line) {
            continue;
        }
        let mut captures = Vec::new();
        for c in times.captures_iter(line) {
            let a = c[1]
                .parse::<i64>()
                .map_err(|e| ProviderError::parse(e.to_string()))?;
            let b = c[2]
                .parse::<i64>()
                .map_err(|e| ProviderError::parse(e.to_string()))?;
            let start = ((a as i128 + b as i128) / (offset as i128 * 2)).abs();
            let end = ((a as i128 - b as i128) / (offset2 as i128 * 2)).abs() + start;
            captures.push((c.get(0).unwrap().range(), start, end));
        }
        if captures.is_empty() {
            continue;
        }
        for i in 1..captures.len() {
            if captures[i].1 < captures[i - 1].2 {
                captures[i - 1].2 = captures[i].1;
                captures[i - 1].1 = captures[i - 1].1.min(captures[i - 1].2);
            }
        }
        let mut output = String::new();
        let mut prev = 0;
        for (range, start, end) in captures {
            output.push_str(&line[prev..range.start]);
            output.push_str(&format!("<{start},{}>", end - start));
            prev = range.end;
        }
        output.push_str(&line[prev..]);
        rows.push(output);
    }
    if rows.is_empty() {
        return Ok(String::new());
    }
    if tags.is_empty() {
        Ok(rows.join("\n"))
    } else {
        Ok(format!("{}\n{}", tags.join("\n"), rows.join("\n")))
    }
}
