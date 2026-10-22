//! Migu operations exported by LX Music v2.12.6.
use super::types::*;
use crate::domain::music::{LxMusicDtoVersion, TrackRecord};
use md5::{Digest, Md5};
use serde_json::{Value, json as j};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 13_2_3 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/13.0.3 Mobile/15E148 Safari/604.1";
const ANDROID_UA: &str = "Mozilla/5.0 (Linux; U; Android 11.0.0; zh-cn; MI 11 Build/OPR1.170623.032) AppleWebKit/534.30 (KHTML, like Gecko) Version/4.0 Mobile Safari/534.30";
const LYRIC_UA: &str = "Mozilla/5.0 (Linux; Android 5.1.1; Nexus 6 Build/LYZ28E) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/59.0.3071.115 Mobile Safari/537.36";
pub const DEVICE_ID: &str = "963B7AA0D21511ED807EE5846EC87D20";
pub fn signature(timestamp: &str, keyword: &str) -> String {
    format!(
        "{:x}",
        Md5::digest(format!(
            "{keyword}6cdc72a439cef99a3418d2a78aa28c73yyapp2d16148780a1dcc7408e06336b98cfd50{DEVICE_ID}{timestamp}"
        ))
    )
}
type CommentCursors = HashMap<(String, u32, u32), (u64, String)>;
struct CommentRequest<'a> {
    id: &'a str,
    kind: CommentKind,
    page: u32,
    limit: u32,
    cursor: Option<String>,
    root: Option<String>,
}
pub struct MgProvider {
    transport: Arc<dyn HttpTransport>,
    cursors: Mutex<CommentCursors>,
    generation: AtomicU64,
}
impl MgProvider {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            cursors: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
        }
    }
    fn fetch(&self, request: HttpRequest, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        let v = json(self.transport.as_ref(), request, ctx)?;
        let code = v.get("code").or_else(|| v.get("returnCode"));
        if code.and_then(Value::as_str) != Some("000000") {
            return Err(ProviderError::upstream(format!(
                "Migu: {} ({})",
                text(&v["info"]),
                code.map(text).unwrap_or_default()
            )));
        }
        Ok(v)
    }
    fn mobile(&self, url: String) -> HttpRequest {
        HttpRequest::get(url)
            .header("User-Agent", MOBILE_UA)
            .header("Referer", "https://m.music.migu.cn/")
    }
    fn search_request(
        keyword: &str,
        page: u32,
        limit: u32,
        playlists: bool,
        timestamp: &str,
    ) -> HttpRequest {
        let switch = if playlists {
            r#"{"song":0,"album":0,"singer":0,"tagSong":0,"mvSong":0,"bestShow":0,"songlist":1,"lyricSong":0}"#
        } else {
            r#"{"song":1,"album":0,"singer":0,"tagSong":1,"mvSong":0,"bestShow":1,"songlist":0,"lyricSong":0}"#
        };
        HttpRequest::get(format!("https://jadeite.migu.cn/music_search/v3/search/searchAll?isCorrect={}&isCopyright=1&searchSwitch={}&pageSize={limit}&text={}&pageNo={page}&sort=0&sid=USS",u8::from(playlists),encode(switch),encode(keyword)))
            .header("uiVersion","A_music_3.6.1").header("deviceId",DEVICE_ID).header("timestamp",timestamp).header("sign",signature(timestamp,keyword)).header("channel","0146921").header("User-Agent",ANDROID_UA)
    }
    fn details(
        &self,
        ids: &[String],
        ctx: &ProviderContext,
    ) -> Result<Vec<TrackRecord>, ProviderError> {
        let mut records = Vec::new();
        let mut seen = HashSet::new();
        for chunk in ids.chunks(100) {
            let request = HttpRequest::post(
                "https://c.musicapp.migu.cn/MIGUM2.0/v1.0/content/resourceinfo.do?resourceType=2",
                format!("resourceId={}", encode(&chunk.join("|"))).into_bytes(),
            )
            .header("Content-Type", "application/x-www-form-urlencoded");
            let v = self.fetch(request, ctx)?;
            let data = v.get("data").unwrap_or(&v);
            for song in array(&data["resource"], "resource")? {
                if song["songId"].is_null() {
                    continue;
                }
                let id = text(&song["songId"]);
                if seen.insert(id) {
                    records.push(track(song, false)?);
                }
            }
        }
        Ok(records)
    }
    fn collection(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<Collection, ProviderError> {
        let (page, limit) = page_args(page, limit)?;
        let id = parse_id(id)?;
        let songs=self.fetch(self.mobile(format!("https://app.c.nf.migu.cn/MIGUM3.0/resource/playlist/song/v2.0?pageNo={page}&pageSize={limit}&playlistId={}",encode(&id))),ctx)?;
        let v = self.fetch(
            self.mobile(format!(
                "https://c.musicapp.migu.cn/MIGUM3.0/resource/playlist/v2.0?playlistId={}",
                encode(&id)
            )),
            ctx,
        )?;
        let info = &v["data"];
        let tracks = normalize_tracks(
            array(&songs["data"]["songList"], "data.songList")?,
            true,
            false,
        )?;
        Ok(Collection {
            info: Playlist {
                id,
                name: text(&info["title"]),
                image_url: opt(&info["imgItem"]["img"]),
                author: opt(&info["ownerName"]),
                description: opt(&info["summary"]),
                play_count: info["opNumItem"].get("playNum").map(number),
                track_count: songs["data"].get("totalCount").map(number),
                extra: info.clone(),
            },
            tracks: Page::new(tracks, number(&songs["data"]["totalCount"]), page, limit),
        })
    }
    fn comments(
        &self,
        request: CommentRequest<'_>,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        let CommentRequest {
            id,
            kind,
            page,
            limit,
            cursor,
            root,
        } = request;
        let (page, limit) = page_args(page, limit)?;
        if id.trim().is_empty() {
            return Err(ProviderError::invalid("Migu song id is empty"));
        }
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let mut cursor_cache = self
            .cursors
            .lock()
            .map_err(|_| ProviderError::parse("Migu cursor lock"))?;
        let base = "https://app.c.nf.migu.cn/MIGUM3.0/user/comment/stack";
        let common = format!(
            "pageSize={limit}&queryType={}&resourceId={}&resourceType=2",
            if kind == CommentKind::Normal { 1 } else { 2 },
            encode(id)
        );
        let url = match kind {
            CommentKind::Normal => {
                if page == 1 {
                    cursor_cache.retain(|(song, _, _), _| song != id);
                }
                let previous = if page == 1 {
                    String::new()
                } else {
                    cursor
                        .or_else(|| {
                            cursor_cache
                                .get(&(id.into(), limit, page))
                                .map(|(_, value)| value.clone())
                        })
                        .filter(|c| !c.is_empty())
                        .ok_or_else(|| {
                            ProviderError::invalid(
                                "Migu comments require the preceding page or its cursor",
                            )
                        })?
                };
                format!("{base}/v1.0?{common}&commentId={}", encode(&previous))
            }
            CommentKind::Hot => format!(
                "{base}/v1.0?{common}&hotCommentStart={}",
                (page as u64 - 1) * limit as u64
            ),
            CommentKind::Replies => {
                let root = root
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| ProviderError::invalid("root_comment_id is required"))?;
                format!(
                    "{base}/{}/v1.0?{common}&start={}",
                    encode(&root),
                    (page as u64 - 1) * limit as u64
                )
            }
        };
        drop(cursor_cache);
        let v = self.fetch(HttpRequest::get(url).header("User-Agent", MOBILE_UA), ctx)?;
        let data = &v["data"];
        let (items, total) = match kind {
            CommentKind::Normal => (&data["comments"], number(&data["commentNums"])),
            CommentKind::Hot => (&data["hotComments"], number(&data["cfgHotCount"])),
            CommentKind::Replies => (
                &data["mainCommentItem"]["replyComments"],
                number(&data["replyTotalCount"]),
            ),
        };
        let comments = array(items, "comments")?
            .iter()
            .map(|i| comment(i, kind == CommentKind::Replies && i["commentId"].is_null()))
            .collect::<Vec<_>>();
        let mut page_result = Page::new(comments, total, page, limit);
        if kind == CommentKind::Normal {
            let next = page_result
                .items
                .last()
                .map(|c| c.id.clone())
                .filter(|id| !id.is_empty());
            if let Some(value) = &next {
                let mut cache = self
                    .cursors
                    .lock()
                    .map_err(|_| ProviderError::parse("Migu cursor lock"))?;
                let key = (id.into(), limit, page + 1);
                if cache.get(&key).is_none_or(|(old, _)| *old < generation) {
                    if cache.len() >= 1024
                        && !cache.contains_key(&key)
                        && let Some(old) = cache.keys().next().cloned()
                    {
                        cache.remove(&old);
                    }
                    cache.insert(key, (generation, value.clone()));
                }
            }
            page_result.next_cursor = next;
        }
        Ok(ProviderResponse::Comments(page_result))
    }
    fn lyric_text(&self, url: &str, ctx: &ProviderContext) -> Result<String, ProviderError> {
        ctx.check()?;
        let response = self.transport.execute(
            HttpRequest::get(url)
                .header("Referer", "https://app.c.nf.migu.cn/")
                .header("User-Agent", LYRIC_UA)
                .header("channel", "0146921"),
            ctx,
        )?;
        ctx.check()?;
        if response.status != 200 {
            return Err(ProviderError::Http {
                status: response.status,
                message: "Migu lyric request failed".into(),
            });
        }
        response.text()
    }
    fn lyrics(
        &self,
        record: TrackRecord,
        word_by_word: bool,
        ctx: &ProviderContext,
    ) -> Result<Lyrics, ProviderError> {
        let info = if record.metadata.get("mrcUrl").is_none_or(Value::is_null) {
            let id = text(&record.platform.song_id);
            self.details(&[id], ctx)?
                .into_iter()
                .next()
                .ok_or_else(|| ProviderError::upstream("Migu lyric metadata unavailable"))?
        } else {
            record
        };
        let field = |key: &str| {
            info.metadata
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let mut result = Lyrics::default();
        if let Some(url) = field("mrcUrl") {
            let encrypted = self.lyric_text(url, ctx)?;
            let plain = crate::lyrics::codec::decode_mrc(&encrypted)
                .map_err(|e| ProviderError::parse(e.to_string()))?;
            result.lyric = crate::lyrics::codec::mrc_to_lrc(&plain)
                .map_err(|e| ProviderError::parse(e.to_string()))?;
            if word_by_word {
                result.word_by_word = Some(
                    crate::lyrics::codec::mrc_to_lx(&plain)
                        .map_err(|e| ProviderError::parse(e.to_string()))?,
                );
            }
        } else if let Some(url) = field("lrcUrl") {
            result.lyric = self.lyric_text(url, ctx)?;
        } else {
            return Err(ProviderError::upstream("Migu lyrics unavailable"));
        }
        result.translated = Some(if let Some(url) = field("trcUrl") {
            self.lyric_text(url, ctx)?
        } else {
            String::new()
        });
        result.extra = j!({"source":"mg","lrcUrl":info.metadata.get("lrcUrl"),"mrcUrl":info.metadata.get("mrcUrl"),"trcUrl":info.metadata.get("trcUrl")});
        Ok(result)
    }
}
impl Provider for MgProvider {
    fn platform(&self) -> Platform {
        Platform::Mg
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
                let (page, limit) = page_args(page, limit)?;
                let v = self.fetch(
                    Self::search_request(&keyword, page, limit, false, &now_ms().to_string()),
                    ctx,
                )?;
                let data = &v["songResultData"];
                if data.is_null() {
                    return Ok(ProviderResponse::Tracks(Page::new(
                        Vec::new(),
                        0,
                        page,
                        limit,
                    )));
                }
                let mut songs = Vec::new();
                for group in array(&data["resultList"], "songResultData.resultList")? {
                    songs.extend(array(group, "search result group")?.iter().cloned());
                }
                let tracks = normalize_tracks(&songs, true, true)?;
                Ok(ProviderResponse::Tracks(Page::new(
                    tracks,
                    number(&data["totalCount"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::HotSearch => {
                let v = self.fetch(
                    HttpRequest::get("http://jadeite.migu.cn:7090/music_search/v3/search/hotword"),
                    ctx,
                )?;
                Ok(ProviderResponse::Keywords(
                    array(
                        &v["data"]["hotwords"][0]["hotwordList"],
                        "data.hotwords[0].hotwordList",
                    )?
                    .iter()
                    .filter(|w| w["resourceType"] == "song")
                    .map(|w| text(&w["word"]))
                    .collect(),
                ))
            }
            ProviderRequest::Detail { ids } => {
                let list = self.details(&ids, ctx)?;
                let count = list.len();
                Ok(ProviderResponse::Tracks(Page::new(
                    list,
                    count as u64,
                    1,
                    ids.len().max(1) as u32,
                )))
            }
            ProviderRequest::Charts => Ok(ProviderResponse::Charts(charts())),
            ProviderRequest::ChartTracks { id, page, limit } => {
                let (page, limit) = page_args(page, limit)?;
                let id = id.trim_start_matches("mg__");
                let v=self.fetch(HttpRequest::get(format!("https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/querycontentbyId.do?columnId={}&needAll=0",encode(id))),ctx)?;
                let songs: Vec<_> = array(&v["columnInfo"]["contents"], "columnInfo.contents")?
                    .iter()
                    .map(|i| i["objectInfo"].clone())
                    .collect();
                let all = normalize_tracks(&songs, false, false)?;
                let count = all.len();
                Ok(ProviderResponse::Tracks(Page::new(
                    all.into_iter()
                        .skip((page as usize - 1) * limit as usize)
                        .take(limit as usize)
                        .collect(),
                    count as u64,
                    page,
                    limit,
                )))
            }
            ProviderRequest::PlaylistTags => {
                let v = self.fetch(
                    self.mobile(
                        "https://app.c.nf.migu.cn/pc/v1.0/template/musiclistplaza-taglist/release"
                            .into(),
                    ),
                    ctx,
                )?;
                let mut tags = Vec::new();
                for (index, group) in array(&v["data"], "data")?.iter().enumerate() {
                    let title = if index == 0 {
                        "热门".into()
                    } else {
                        text(&group["header"]["title"])
                    };
                    for item in array(&group["content"], "tag content")? {
                        tags.push(PlaylistTag {
                            id: text(&item["texts"][1]),
                            name: text(&item["texts"][0]),
                            group: title.clone(),
                            extra: item.clone(),
                        });
                    }
                }
                Ok(ProviderResponse::PlaylistTags(tags))
            }
            ProviderRequest::Playlists {
                tag, page, limit, ..
            } => {
                let (page, limit) = page_args(page, limit)?;
                let url = if tag.is_empty() {
                    format!(
                        "https://app.c.nf.migu.cn/pc/bmw/page-data/playlist-square-recommend/v1.0?templateVersion=2&pageNo={page}"
                    )
                } else {
                    format!(
                        "https://app.c.nf.migu.cn/pc/v1.0/template/musiclistplaza-listbytag/release?pageNumber={page}&templateVersion=2&tagId={}",
                        encode(&tag)
                    )
                };
                let v = self.fetch(self.mobile(url), ctx)?;
                let data = &v["data"];
                let list = if let Some(contents) = data["contents"].as_array() {
                    let mut list = Vec::new();
                    playlist_tree(contents, &mut list, &mut HashSet::new());
                    list
                } else {
                    array(
                        &data["contentItemList"][1]["itemList"],
                        "data.contentItemList[1].itemList",
                    )?
                    .iter()
                    .map(playlist_card)
                    .collect()
                };
                // The active upstream API has no total count; LX uses this bound.
                let total = data.get("totalCount").map(number).unwrap_or(99999);
                Ok(ProviderResponse::Playlists(Page::new(
                    list, total, page, limit,
                )))
            }
            ProviderRequest::SearchPlaylists {
                keyword,
                page,
                limit,
            } => {
                let (page, limit) = page_args(page, limit)?;
                let v = self.fetch(
                    Self::search_request(&keyword, page, limit, true, &now_ms().to_string()),
                    ctx,
                )?;
                let body = v.get("data").unwrap_or(&v);
                let data = &body["songListResultData"];
                let list = array(&data["result"], "songListResultData.result")?
                    .iter()
                    .filter(|p| !p["id"].is_null())
                    .map(|p| Playlist {
                        id: text(&p["id"]),
                        name: text(&p["name"]),
                        image_url: opt(&p["musicListPicUrl"]),
                        author: opt(&p["userName"]),
                        description: None,
                        play_count: p.get("playNum").map(number),
                        track_count: p.get("musicNum").map(number),
                        extra: p.clone(),
                    })
                    .collect();
                Ok(ProviderResponse::Playlists(Page::new(
                    list,
                    number(&data["totalCount"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::PlaylistDetail { id, page, limit } => Ok(
                ProviderResponse::Collection(self.collection(&id, page, limit, ctx)?),
            ),
            ProviderRequest::Comments {
                id,
                kind,
                page,
                limit,
                cursor,
                root_comment_id,
            } => self.comments(
                CommentRequest {
                    id: &id,
                    kind,
                    page,
                    limit,
                    cursor,
                    root: root_comment_id,
                },
                ctx,
            ),
            ProviderRequest::Lyrics {
                track,
                word_by_word,
            } => Ok(ProviderResponse::Lyrics(self.lyrics(
                track,
                word_by_word,
                ctx,
            )?)),
            ProviderRequest::Picture { track } => {
                let id = text(&track.platform.song_id);
                let info = self
                    .details(&[id], ctx)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| ProviderError::upstream("Migu song unavailable"))?;
                let img = info
                    .metadata
                    .get("picUrl")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::upstream("Migu picture unavailable"))?;
                Ok(ProviderResponse::Picture(img.into()))
            }
            other => Err(ProviderError::Unsupported {
                platform: Platform::Mg,
                operation: other.operation().into(),
            }),
        }
    }
}
fn array<'a>(v: &'a Value, name: &str) -> Result<&'a Vec<Value>, ProviderError> {
    v.as_array()
        .ok_or_else(|| ProviderError::parse(format!("missing {name} array")))
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn opt(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}
fn parse_id(raw: &str) -> Result<String, ProviderError> {
    if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()) {
        return Ok(raw.into());
    }
    for marker in ["?playlistId=", "&playlistId=", "?id=", "&id=", "/playlist/"] {
        if let Some((_, tail)) = raw.split_once(marker) {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                return Ok(digits);
            }
        }
    }
    Err(ProviderError::invalid("unsupported Migu playlist link"))
}
fn size(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let size = number(v);
    if size == 0 {
        return j!("0 B");
    }
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let power = ((size as f64).log(1024.0).floor() as usize).min(4);
    j!(format!(
        "{:.2} {}",
        size as f64 / 1024f64.powi(power as i32),
        units[power]
    ))
}
fn track(song: &Value, v5: bool) -> Result<TrackRecord, ProviderError> {
    if song["songId"].is_null() {
        return Err(ProviderError::parse("Migu songId missing"));
    }
    let formats = if v5 {
        &song["audioFormats"]
    } else {
        &song["newRateFormats"]
    };
    let mut types = Vec::new();
    let mut detail = serde_json::Map::new();
    if let Some(formats) = formats.as_array() {
        for f in formats {
            let quality = match f["formatType"].as_str() {
                Some("PQ") => "128k",
                Some("HQ") => "320k",
                Some("SQ") => "flac",
                Some("ZQ" | "ZQ24") => "flac24bit",
                _ => continue,
            };
            let bytes = ["asize", "isize", "size", "androidSize"]
                .iter()
                .find_map(|key| f.get(key).filter(|value| !value.is_null()))
                .unwrap_or(&Value::Null);
            let s = size(bytes);
            types.push(j!({"type":quality,"size":s}));
            detail.insert(quality.into(), j!({"size":s}));
        }
    }
    let singers = if v5 {
        &song["singerList"]
    } else {
        &song["artists"]
    };
    let singer = singers
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    s.get("name")
                        .or_else(|| s.get("singerName"))
                        .map(text)
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();
    let name = song
        .get("songName")
        .or_else(|| song.get("name"))
        .map(text)
        .unwrap_or_default();
    let img = if v5 {
        song.get("img3")
            .filter(|v| !text(v).is_empty())
            .or_else(|| song.get("img2").filter(|v| !text(v).is_empty()))
            .or_else(|| song.get("img1"))
    } else {
        song["albumImgs"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a.get("img"))
    }
    .cloned()
    .unwrap_or(Value::Null);
    let img = img
        .as_str()
        .map(|s| {
            if s.is_empty() || s.starts_with("http:") || s.starts_with("https:") {
                s.to_owned()
            } else {
                format!("http://d.musicapp.migu.cn{s}")
            }
        })
        .map(Value::String)
        .unwrap_or(Value::Null);
    let duration = if v5 {
        j!(playtime(number(&song["duration"])))
    } else {
        song.get("length")
            .and_then(Value::as_str)
            .and_then(|s| {
                let bytes = s.as_bytes();
                if bytes.len() >= 5 {
                    let b = &bytes[bytes.len() - 5..];
                    if b[0].is_ascii_digit()
                        && b[1].is_ascii_digit()
                        && b[2] == b':'
                        && b[3].is_ascii_digit()
                        && b[4].is_ascii_digit()
                    {
                        return Some(j!(std::str::from_utf8(b).unwrap()));
                    }
                }
                None
            })
            .unwrap_or(Value::Null)
    };
    let mrc = song
        .get("mrcUrl")
        .or_else(|| song.get("mrcurl"))
        .cloned()
        .unwrap_or(Value::Null);
    TrackRecord::from_lx_dto(j!({"name":name,"singer":singer,"albumName":text(&song["album"]),"albumId":song["albumId"],"songmid":song["songId"],"copyrightId":song["copyrightId"],"source":"mg","interval":duration,"img":img,"lrc":null,"lrcUrl":song["lrcUrl"],"mrcUrl":mrc,"trcUrl":song["trcUrl"],"types":types,"_types":detail,"typeUrl":{}}),LxMusicDtoVersion::Legacy).map_err(|e|ProviderError::parse(e.to_string()))
}
fn normalize_tracks(
    songs: &[Value],
    v5: bool,
    by_copyright: bool,
) -> Result<Vec<TrackRecord>, ProviderError> {
    let mut list = Vec::new();
    let mut seen = HashSet::new();
    for song in songs {
        if song["songId"].is_null()
            || text(&song["songId"]).is_empty()
            || (by_copyright && text(&song["copyrightId"]).is_empty())
        {
            continue;
        }
        let id = text(if by_copyright {
            &song["copyrightId"]
        } else {
            &song["songId"]
        });
        if seen.insert(id) {
            list.push(track(song, v5)?);
        }
    }
    Ok(list)
}
fn playtime(seconds: u64) -> String {
    if seconds == 0 {
        "--/--".into()
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}
fn playlist_tree(items: &[Value], out: &mut Vec<Playlist>, seen: &mut HashSet<String>) {
    for item in items {
        if let Some(contents) = item["contents"].as_array() {
            playlist_tree(contents, out, seen);
        } else if text(&item["resType"]) == "2021" {
            let id = text(&item["resId"]);
            if seen.insert(id.clone()) {
                out.push(Playlist {
                    id,
                    name: text(&item["txt"]),
                    image_url: opt(&item["img"]),
                    author: Some(String::new()),
                    description: opt(&item["txt2"]),
                    play_count: None,
                    track_count: None,
                    extra: item.clone(),
                });
            }
        }
    }
}
fn playlist_card(item: &Value) -> Playlist {
    Playlist {
        id: text(&item["logEvent"]["contentId"]),
        name: text(&item["title"]),
        image_url: opt(&item["imageUrl"]),
        author: Some(String::new()),
        description: None,
        play_count: item["barList"][0]
            .get("title")
            .and_then(|v| text(v).parse().ok()),
        track_count: None,
        extra: item.clone(),
    }
}
fn comment(item: &Value, reply: bool) -> Comment {
    let user = &item["user"];
    let avatar = ["middleIcon", "bigIcon", "smallIcon"]
        .iter()
        .find_map(|key| {
            user.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_owned);
    let (id, body, time) = if reply {
        ("replyId", "replyInfo", "replyTime")
    } else {
        ("commentId", "commentInfo", "commentTime")
    };
    Comment {
        id: text(&item[id]),
        text: text(&item[body]),
        author: text(&user["nickName"]),
        avatar_url: avatar,
        timestamp_ms: timestamp(&item[time]),
        likes: if reply {
            0
        } else {
            number(&item["opNumItem"]["thumbNum"])
        },
        replies: item["replyComments"]
            .as_array()
            .map(|list| list.iter().map(|i| comment(i, true)).collect())
            .unwrap_or_default(),
        extra: item.clone(),
    }
}
fn timestamp(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    let raw = value.as_str()?;
    if let Ok(number) = raw.parse::<u64>() {
        return Some(number);
    }
    let date = raw.get(..10)?;
    let date: Vec<i64> = date
        .split('-')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if date.len() != 3 {
        return None;
    }
    let (year, month, day) = (date[0], date[1], date[2]);
    if !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > month_days[(month - 1) as usize] {
        return None;
    }
    let clock: Vec<i64> = raw
        .get(11..19)?
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if clock.len() != 3
        || !(0..24).contains(&clock[0])
        || !(0..60).contains(&clock[1])
        || !(0..60).contains(&clock[2])
    {
        return None;
    }
    let mut suffix = raw.get(19..)?.trim();
    let mut millis = 0i64;
    if let Some(fraction) = suffix.strip_prefix('.') {
        let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
        let part: String = digits.chars().take(3).collect();
        if part.is_empty() {
            return None;
        }
        millis = part.parse::<i64>().ok()? * 10i64.pow(3 - part.len() as u32);
        suffix = &fraction[digits.len()..];
    }
    // Migu's unqualified commentTime/replyTime strings use China Standard Time.
    let offset = if suffix.is_empty() {
        8 * 60
    } else if matches!(suffix, "Z" | "z") {
        0
    } else {
        let sign = if suffix.starts_with('+') {
            1
        } else if suffix.starts_with('-') {
            -1
        } else {
            return None;
        };
        let zone = &suffix[1..];
        let (hour, minute) = if let Some(parts) = zone.split_once(':') {
            parts
        } else {
            (zone.get(..2)?, zone.get(2..)?)
        };
        let hour = hour.parse::<i64>().ok()?;
        let minute = minute.parse::<i64>().ok()?;
        if hour > 23 || minute > 59 {
            return None;
        }
        sign * (hour * 60 + minute)
    };
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let shifted = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted + 2) / 5 + day - 1;
    let days = era * 146097 + (yoe * 365 + yoe / 4 - yoe / 100 + doy) - 719468;
    u64::try_from(
        (days * 86400 + clock[0] * 3600 + clock[1] * 60 + clock[2] - offset * 60) * 1000 + millis,
    )
    .ok()
}
fn charts() -> Vec<Chart> {
    [
        ("27553319", "新歌榜"),
        ("27186466", "热歌榜"),
        ("27553408", "原创榜"),
        ("75959118", "音乐风向榜"),
        ("76557036", "彩铃分贝榜"),
        ("76557745", "会员臻爱榜"),
        ("23189800", "港台榜"),
        ("23189399", "内地榜"),
        ("19190036", "欧美榜"),
        ("83176390", "国风金曲榜"),
    ]
    .into_iter()
    .map(|(id, name)| Chart {
        id: format!("mg__{id}"),
        name: name.into(),
        image_url: None,
        description: None,
        extra: j!({"bangid":id}),
    })
    .collect()
}
