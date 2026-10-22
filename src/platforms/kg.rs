//! Kugou's active LX 2.12.6 catalog and share routes.
use super::types::*;
use crate::domain::music::{LxMusicDtoVersion, TrackRecord};
use base64::{Engine, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};

const AUDIO_UA: &str = "Android712-AndroidPhone-11451-376-0-FeeCacheUpdate-wifi";
const LYRIC_UA: &str = "KuGou2012-9020-ExpandSearchManager";
const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 11_0 like Mac OS X) AppleWebKit/604.1.38 (KHTML, like Gecko) Version/11.0 Mobile/15A372 Safari/604.1";
const IPHONE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 9_1 like Mac OS X) AppleWebKit/601.1.46 (KHTML, like Gecko) Version/9.0 Mobile/13B143 Safari/601.1";
const SHARE_REFERER: &str = "https://m3ws.kugou.com/share/index.php";
static HTML_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static HTML_ENTITIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&#(x[0-9a-fA-F]+|\d+);").unwrap());
static COMMENT_TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4})[-/](\d{2})[-/](\d{2})[ T](\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,3}))?(Z|[+-]\d{2}:?\d{2})?$").unwrap()
});

pub struct KgProvider {
    transport: Arc<dyn HttpTransport>,
}
impl KgProvider {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
    fn raw(
        &self,
        request: HttpRequest,
        ctx: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        ctx.check()?;
        let response = self.transport.execute(request, ctx)?;
        ctx.check()?;
        Ok(response)
    }
    fn request(&self, request: HttpRequest, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        let response = self.raw(request, ctx)?;
        if response.status != 200 {
            return Err(ProviderError::Http {
                status: response.status,
                message: "Kugou request failed".into(),
            });
        }
        if let Ok(value) = response.json() {
            return Ok(value);
        }
        let source = response.text()?;
        let source = source.trim();
        if let (Some(start), Some(end)) = (source.find('('), source.rfind(')'))
            && start < end
        {
            return serde_json::from_str(&source[start + 1..end])
                .map_err(|e| ProviderError::parse(e.to_string()));
        }
        Err(ProviderError::parse("invalid Kugou JSON response"))
    }
    fn get(&self, url: String, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        self.request(HttpRequest::get(url), ctx)
    }
    fn data(&self, request: HttpRequest, ctx: &ProviderContext) -> Result<Value, ProviderError> {
        let body = self.request(request, ctx)?;
        success(&body)?;
        Ok(unwrap_data(body))
    }
    fn text(&self, url: &str, ua: &str, ctx: &ProviderContext) -> Result<String, ProviderError> {
        let response = self.raw(HttpRequest::get(url).header("User-Agent", ua), ctx)?;
        if response.status != 200 {
            return Err(ProviderError::Http {
                status: response.status,
                message: "Kugou HTML request failed".into(),
            });
        }
        response.text()
    }
    fn audio_infos(
        &self,
        hashes: &[Value],
        v3: bool,
        ctx: &ProviderContext,
    ) -> Result<Vec<TrackRecord>, ProviderError> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let hashes = hashes
            .iter()
            .filter(|i| seen.insert(text(&i["hash"])))
            .cloned()
            .collect::<Vec<_>>();
        for chunk in hashes.chunks(100) {
            let body = json!({"data":chunk,"area_code":"1","show_privilege":1,"show_album_info":"1","is_publish":"","appid":1005,"clientver":11451,"mid":"1","dfid":"-","clienttime":now_ms(),"key":"OIlwieks28dk2k092lksi2UIkp","fields":if v3{"album_info,author_name,audio_info,ori_audio_name,base,songname,classification"}else{"album_info,author_name,audio_info,ori_audio_name,base,songname"}});
            let request = HttpRequest::post(
                format!(
                    "http://gateway.kugou.com/v{}/album_audio/audio",
                    if v3 { 3 } else { 2 }
                ),
                bytes(&body)?,
            )
            .header("Content-Type", "application/json")
            .header("KG-THash", "13a3164")
            .header("KG-RC", "1")
            .header("KG-Fake", "0")
            .header("KG-RF", "00869891")
            .header("User-Agent", AUDIO_UA)
            .header("x-router", "kmr.service.kugou.com");
            let data = self.data(request, ctx)?;
            for row in array(&data)? {
                let item = if row.is_array() { &row[0] } else { row };
                // The service uses an empty object for a removed/unavailable hash.
                // Other nonempty malformed rows remain protocol errors.
                if !item.is_null() && !item.as_object().is_some_and(|row| row.is_empty()) {
                    result.push(audio_track(item)?);
                }
            }
        }
        if result.is_empty() && !hashes.is_empty() {
            return Err(ProviderError::upstream(
                "Kugou returned no available tracks for requested hashes",
            ));
        }
        let mut seen = HashSet::new();
        result.retain(|r| {
            seen.insert((
                text(&r.platform.song_id),
                r.metadata.get("hash").map(text).unwrap_or_default(),
            ))
        });
        Ok(result)
    }
    fn hydrate(
        &self,
        items: &[Value],
        ctx: &ProviderContext,
    ) -> Result<Vec<TrackRecord>, ProviderError> {
        let hashes = items
            .iter()
            .map(|i| json!({"hash":i["hash"]}))
            .collect::<Vec<_>>();
        self.audio_infos(&hashes, false, ctx)
    }
    fn special(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ProviderError::invalid("invalid Kugou special id"));
        }
        let html = self.text(
            &format!("http://www2.kugou.kugou.com/yueku/v9/special/single/{id}-5-9999.html"),
            MOBILE_UA,
            ctx,
        )?;
        let data = embedded_json(&html, "global.data", '[', ']')?;
        let list = self.hydrate(array(&data)?, ctx)?;
        let total = list.len() as u64;
        let name = capture(&html, r#"name:\s*"([^"\r\n]+)""#);
        let image = capture(&html, r#"pic:\s*"([^"\r\n]+)""#);
        let description=capture(&html,r#"(?s)<div class="pc_specail_text pc_singer_tab_content" id="specailIntroduceWrap">(.*?)</div>"#).map(|v|decode_name(&v));
        let info = Playlist {
            id: format!("id_{id}"),
            name: name.unwrap_or_default(),
            image_url: image,
            author: None,
            description,
            play_count: None,
            track_count: Some(total),
            extra: json!({"specialid":id}),
        };
        Ok(collection(info, list, total, page, limit))
    }
    fn signed_share(
        &self,
        url: &str,
        params: &str,
        time: &str,
        ctx: &ProviderContext,
    ) -> Result<Value, ProviderError> {
        self.data(
            HttpRequest::get(format!(
                "{url}?{params}&signature={}",
                signature_params(params, true, "")
            ))
            .header("mid", time)
            .header("Referer", SHARE_REFERER)
            .header("User-Agent", MOBILE_UA)
            .header("dfid", "-")
            .header("clienttime", time),
            ctx,
        )
    }
    fn global_list(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        if id.is_empty() || id.len() > 1000 {
            return Err(ProviderError::invalid("invalid Kugou global collection id"));
        }
        let params = format!(
            "appid=1058&specialid=0&global_specialid={}&format=jsonp&srcappid=2919&clientver=20000&clienttime=1586163242519&mid=1586163242519&uuid=1586163242519&dfid=-",
            encode(id)
        );
        let info = self.signed_share(
            "https://mobiles.kugou.com/api/v5/special/info_v2",
            &params,
            "1586163242519",
            ctx,
        )?;
        let count = number(&info["songcount"]);
        let mut items = Vec::new();
        let mut fetched = 0u64;
        let mut p = 0;
        while fetched < count {
            ctx.check()?;
            p += 1;
            fetched = fetched.saturating_add(300);
            let params = format!(
                "appid=1058&global_specialid={}&specialid=0&plat=0&version=8000&page={p}&pagesize=300&srcappid=2919&clientver=20000&clienttime=1586163263991&mid=1586163263991&uuid=1586163263991&dfid=-",
                encode(id)
            );
            let data = self.signed_share(
                "https://mobiles.kugou.com/api/v5/special/song_v2",
                &params,
                "1586163263991",
                ctx,
            )?;
            items.extend(array(&data["info"])?.iter().cloned());
        }
        let list = self.hydrate(&items, ctx)?;
        let total = list.len() as u64;
        let mut info = playlist(&info, false);
        info.id = id.into();
        info.image_url = info.image_url.map(|v| v.replace("{size}", "240"));
        Ok(collection(info, list, total, page, limit))
    }
    fn gcid_chain(
        &self,
        id: &str,
        chain: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        let params = |p: u64| {
            format!(
                "srcappid=2919&clientver=20000&clienttime=1789566751657&mid=1789566751657&uuid=1789566751657&dfid=-&listid=15&type=0&pagesize=50&global_collection_id={}&page={p}&share_type=collect&appid=1058&chain={}",
                encode(id),
                encode(chain)
            )
        };
        let data = self.signed_share(
            "https://m3ws.kugou.com/v2/zlist/list",
            &params(1),
            "1586163263991",
            ctx,
        )?;
        let info = array(&data["info"])?
            .first()
            .ok_or_else(|| ProviderError::parse("Kugou chain list info is missing"))?;
        let total = number(&info["count"]);
        let mut items = array(&data["list"]["info"])?.clone();
        for p in 2..=total.div_ceil(50) {
            let data = self.signed_share(
                "https://m3ws.kugou.com/v2/zlist/list",
                &params(p),
                "1586163263991",
                ctx,
            )?;
            items.extend(array(&data["list"]["info"])?.iter().cloned());
        }
        let list = self.hydrate(&items, ctx)?;
        let info = Playlist {
            id: id.into(),
            name: text(&info["name"]),
            image_url: optional(&info["pic"]).map(|s| s.replace("{size}", "240")),
            author: optional(&info["list_create_username"]),
            description: optional(&info["intro"]),
            play_count: None,
            track_count: Some(total),
            extra: info.clone(),
        };
        Ok(collection(info, list, total, page, limit))
    }
    fn decode_gcid(&self, id: &str, ctx: &ProviderContext) -> Result<String, ProviderError> {
        let params = "dfid=-&appid=1005&mid=0&clientver=20109&clienttime=640612895&uuid=-";
        let body = json!({"ret_info":1,"data":[{"id":id,"id_type":2}]});
        let payload = bytes(&body)?;
        let url = format!(
            "https://t.kugou.com/v1/songlist/batch_decode?{params}&signature={}",
            signature_params(params, false, std::str::from_utf8(&payload).unwrap())
        );
        let data=self.data(HttpRequest::post(url,payload).header("Content-Type","application/json").header("User-Agent","Mozilla/5.0 (Linux; Android 10; HUAWEI HMA-AL00) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/83.0.4103.106 Mobile Safari/537.36").header("Referer","https://m.kugou.com/"),ctx)?;
        let id = text(&data["list"][0]["global_collection_id"]);
        if id.is_empty() {
            Err(ProviderError::parse(
                "Kugou gcid decode returned no global id",
            ))
        } else {
            Ok(id)
        }
    }
    fn code_list(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        let body = json!({"appid":1001,"clientver":9020,"mid":"21511157a05844bd085308bc76ef3343","clienttime":640612895,"key":"36164c4015e704673c588ee202b9ecb8","data":id});
        let song = self.data(
            HttpRequest::post("http://t.kugou.com/command/", bytes(&body)?)
                .header("Content-Type", "application/json")
                .header("KG-RC", "1")
                .header("KG-THash", "network_super_call.cpp:3676261689:379")
                .header("User-Agent", ""),
            ctx,
        )?;
        let info = &song["info"];
        let global = text(&info["global_collection_id"]);
        if !global.is_empty() {
            return self.global_list(&global, page, limit, ctx);
        }
        if number(&info["type"]) == 2 {
            return self.special(&text(&info["id"]), page, limit, ctx);
        }
        let items = if !info["userid"].is_null() {
            let body = json!({"appid":1001,"clientver":9020,"mid":"21511157a05844bd085308bc76ef3343","clienttime":640612895,"key":"36164c4015e704673c588ee202b9ecb8","data":{"id":info["id"],"type":3,"userid":info["userid"],"collect_type":0,"page":1,"pagesize":info["count"]}});
            self.data(
                HttpRequest::post(
                    "http://www2.kugou.kugou.com/apps/kucodeAndShare/app/",
                    bytes(&body)?,
                )
                .header("Content-Type", "application/json")
                .header("KG-RC", "1")
                .header("KG-THash", "network_super_call.cpp:3676261689:379")
                .header("User-Agent", ""),
                ctx,
            )?
        } else {
            song["list"].clone()
        };
        let list = self.hydrate(array(&items)?, ctx)?;
        let total = list.len() as u64;
        let info = Playlist {
            id: id.into(),
            name: text(&info["name"]),
            image_url: optional(&info["img_size"])
                .map(|s| s.replace("{size}", "240"))
                .or_else(|| optional(&info["img"])),
            author: optional(&info["username"]),
            description: None,
            play_count: None,
            track_count: Some(total),
            extra: info.clone(),
        };
        Ok(collection(info, list, total, page, limit))
    }
    fn chain_list(
        &self,
        chain: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        let song=self.data(HttpRequest::get(format!("http://m.kugou.com/schain/transfer?pagesize=10000&chain={}&su=1&page=1&n=0.7928855356604456",encode(chain))).header("User-Agent",IPHONE_UA),ctx)?;
        if song["list"].is_array() {
            let list = self.hydrate(array(&song["list"])?, ctx)?;
            let total = list.len() as u64;
            let i = &song["info"];
            let info = Playlist {
                id: chain.into(),
                name: text(&i["name"]),
                image_url: optional(&i["img"]),
                author: optional(&i["username"]),
                description: None,
                play_count: None,
                track_count: Some(total),
                extra: i.clone(),
            };
            return Ok(collection(info, list, total, page, limit));
        }
        let global = text(&song["global_collection_id"]);
        if !global.is_empty() {
            return self
                .gcid_chain(&global, chain, page, limit, ctx)
                .or_else(|e| {
                    ctx.check()?;
                    if matches!(
                        e,
                        ProviderError::Cancelled | ProviderError::DeadlineExceeded
                    ) {
                        Err(e)
                    } else {
                        self.global_list(&global, page, limit, ctx)
                    }
                });
        }
        let html=self.text(&format!("https://m.kugou.com/share/?chain={}&id={}",encode(chain),encode(chain)),"Mozilla/5.0 (iPhone; CPU iPhone OS 13_2_3 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/13.0.3 Mobile/15E148 Safari/604.1",ctx)?;
        let info = embedded_json(&html, "phpParam", '{', '}')?;
        let legacy_tracks = (|| -> Result<Vec<TrackRecord>, ProviderError> {
            let mut items = Vec::new();
            let mut p = 1u64;
            let mut seen = HashSet::new();
            let mut known_total = [
                number(&info["songcount"]),
                number(&info["count"]),
                number(&song["count"]),
            ]
            .into_iter()
            .find(|n| *n > 0);
            loop {
                let params = format!(
                    "srcappid=2919&clientver=20000&appid=1058&type=0&module=playlist&page={p}&pagesize=100&specialid={}",
                    encode(&text(&song["id"]))
                );
                let signature = signature_params(&params, true, "");
                let request=HttpRequest::get(format!("https://pubsongscdn.kugou.com/v2/get_other_list_file?{params}&signature={signature}")).header("Referer",SHARE_REFERER).header("User-Agent",MOBILE_UA).header("dfid","-");
                let data = self.data(request, ctx)?;
                let rows = array(&data["info"])?;
                if known_total.is_none() {
                    known_total = [number(&data["total"]), number(&data["count"])]
                        .into_iter()
                        .find(|n| *n > 0);
                }
                let before = items.len();
                for row in rows {
                    if seen.insert(text(&row["hash"])) {
                        items.push(row.clone());
                    }
                }
                if rows.len() < 100 || known_total.is_some_and(|n| items.len() as u64 >= n) {
                    break;
                }
                if before == items.len() {
                    return Err(ProviderError::upstream(
                        "Kugou share pagination repeated a page",
                    ));
                }
                p += 1;
            }
            self.hydrate(&items, ctx)
        })();
        let list = match legacy_tracks {
            Ok(list) => list,
            Err(error) => {
                ctx.check()?;
                if matches!(
                    error,
                    ProviderError::Cancelled | ProviderError::DeadlineExceeded
                ) {
                    return Err(error);
                }
                let html=self.text(&format!("http://www.kugou.com/share/{}.html",encode(chain)),"Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/86.0.4240.198 Safari/537.36",ctx)?;
                self.hydrate(
                    array(&embedded_json(&html, "dataFromSmarty", '[', ']')?)?,
                    ctx,
                )?
            }
        };
        let total = list.len() as u64;
        let mut info = playlist(&info, false);
        info.id = chain.into();
        info.image_url = info.image_url.map(|s| s.replace("{size}", "240"));
        Ok(collection(info, list, total, page, limit))
    }
    fn share_url(
        &self,
        url: &str,
        page: u32,
        limit: u32,
        depth: u8,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        if depth > 3 {
            return Err(ProviderError::upstream(
                "Kugou share redirect limit exceeded",
            ));
        }
        let url = url.split('#').next().unwrap_or(url);
        if let Some(chain) = query_value(url, "chain") {
            if let Some(global) = query_value(url, "global_collection_id") {
                return self.gcid_chain(&global, &chain, page, limit, ctx);
            }
            return self.chain_list(&chain, page, limit, ctx);
        }
        if let Some(gcid) = capture(url, r"(gcid_\w+)") {
            let id = self.decode_gcid(&gcid, ctx)?;
            return self.global_list(&id, page, limit, ctx);
        }
        if let Some(global) = query_value(url, "global_collection_id") {
            return self.global_list(&global, page, limit, ctx);
        }
        if url.contains(".html")
            && !url.contains("song.html")
            && !url.contains("zlist.html")
            && let Some(chain) = capture(url, r"/(\w+)\.html(?:\?|&|$)")
        {
            return self.chain_list(&chain, page, limit, ctx);
        }
        let url = if url.contains("zlist.html") {
            let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
            page_url(
                &format!("https://m3ws.kugou.com/zlist/list?{query}"),
                1,
                10000,
            )
        } else {
            url.into()
        };
        let response = self.raw(
            HttpRequest::get(&url)
                .header("User-Agent", IPHONE_UA)
                .header("Referer", &url),
            ctx,
        )?;
        if let Some(location) = response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .map(|(_, v)| v)
        {
            return self.share_url(location, page, limit, depth + 1, ctx);
        }
        if response.status != 200 {
            return Err(ProviderError::Http {
                status: response.status,
                message: "Kugou share request failed".into(),
            });
        }
        let body = response.text()?;
        if let Ok(data) = serde_json::from_str::<Value>(&body) {
            success(&data)?;
            let info = array(&data["info"])?
                .first()
                .ok_or_else(|| ProviderError::parse("Kugou share info is missing"))?;
            let count = number(&info["count"]);
            let mut items = Vec::new();
            let mut fetched = 0;
            let mut p = 0;
            while fetched < count {
                p += 1;
                fetched = fetched.saturating_add(90);
                let url = page_url(&url, p, 90);
                let data = self.data(
                    HttpRequest::get(&url)
                        .header("User-Agent", IPHONE_UA)
                        .header("Referer", &url),
                    ctx,
                )?;
                items.extend(array(&data["list"]["info"])?.iter().cloned());
            }
            let list = self.hydrate(&items, ctx)?;
            let total = list.len() as u64;
            let info = Playlist {
                id: url,
                name: text(&info["name"]),
                image_url: optional(&info["pic"]).map(|s| s.replace("{size}", "240")),
                author: optional(&info["list_create_username"]),
                description: None,
                play_count: None,
                track_count: Some(total),
                extra: info.clone(),
            };
            return Ok(collection(info, list, total, page, limit));
        }
        if let Some(global) = capture(&body, r#""global_collection_id"\s*:\s*"(\w+)""#) {
            return self.global_list(&global, page, limit, ctx);
        }
        if let Some(gcid) = capture(&body, r#""(?:encode_gic|encode_src_gid)"\s*:\s*"(\w+)""#) {
            let global = self.decode_gcid(&gcid, ctx)?;
            return self.global_list(&global, page, limit, ctx);
        }
        Err(ProviderError::parse(
            "Kugou share page has no collection id",
        ))
    }
    fn playlist_detail(
        &self,
        id: &str,
        page: u32,
        limit: u32,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        if id.contains("special/single/")
            && let Some(id) = capture(id, r"/(\d+)\.html")
        {
            return self.special(&id, page, limit, ctx);
        }
        if id.starts_with("http://") || id.starts_with("https://") {
            return self.share_url(id, page, limit, 0, ctx);
        }
        if id.starts_with("gcid_") {
            let global = self.decode_gcid(id, ctx)?;
            return self.global_list(&global, page, limit, ctx);
        }
        if let Some(id) = id.strip_prefix("id_") {
            return self.special(id, page, limit, ctx);
        }
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
            return self.code_list(id, page, limit, ctx);
        }
        self.special(id, page, limit, ctx)
    }
}

impl Provider for KgProvider {
    fn platform(&self) -> Platform {
        Platform::Kg
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
                let body=self.get(format!("http://songsearch.kugou.com/song_search_v2?platform=AndroidFilter&iscorrection=1&keyword={}&hifiquality=0&pagesize={limit}&PrivilegeFilter=0&page={page}",encode(&keyword)),ctx)?;
                success(&body)?;
                let mut list = Vec::new();
                let mut seen = HashSet::new();
                for item in array(&body["data"]["lists"])? {
                    for i in
                        std::iter::once(item).chain(item["Grp"].as_array().into_iter().flatten())
                    {
                        let key = (text(&i["Audioid"]), text(&i["FileHash"]));
                        if seen.insert(key) {
                            list.push(search_track(i)?);
                        }
                    }
                }
                Ok(ProviderResponse::Tracks(Page::new(
                    list,
                    number(&body["data"]["total"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::HotSearch => {
                let request=HttpRequest::get("http://gateway.kugou.com/api/v3/search/hot_tab?signature=ee44edb9d7155821412d220bcaf509dd&appid=1005&clientver=10026&plat=0").header("dfid","1ssiv93oVqMp27cirf2CvoF1").header("mid","156798703528610303473757548878786007104").header("clienttime","1584257267").header("x-router","msearch.kugou.com").header("user-agent","Android9-AndroidPhone-10020-130-0-searchrecommendprotocol-wifi").header("kg-rc","1");
                let body = self.request(request, ctx)?;
                success(&body)?;
                let mut keywords = Vec::new();
                for group in array(&body["data"]["list"])? {
                    for item in array(&group["keywords"])? {
                        keywords.push(decode_name(&text(&item["keyword"])));
                    }
                }
                Ok(ProviderResponse::Keywords(keywords))
            }
            ProviderRequest::Detail { ids } => {
                let hashes = ids.iter().map(|id| json!({"hash":id})).collect::<Vec<_>>();
                let list = self.audio_infos(&hashes, true, ctx)?;
                let total = list.len() as u64;
                Ok(ProviderResponse::Tracks(Page::new(
                    list,
                    total,
                    1,
                    ids.len() as u32,
                )))
            }
            ProviderRequest::Charts => {
                let data: Value = serde_json::from_str(include_str!("kg/charts.json"))
                    .map_err(|e| ProviderError::parse(e.to_string()))?;
                Ok(ProviderResponse::Charts(
                    array(&data)?
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
                let id = id.strip_prefix("kg__").unwrap_or(&id);
                let body=self.get(format!("http://mobilecdnbj.kugou.com/api/v3/rank/song?version=9108&ranktype=1&plat=0&pagesize={limit}&area_code=1&page={page}&rankid={}&with_res_tag=0&show_portrait_mv=1",encode(id)),ctx)?;
                success(&body)?;
                let list = array(&body["data"]["info"])?
                    .iter()
                    .map(chart_track)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ProviderResponse::Tracks(Page::new(
                    list,
                    number(&body["data"]["total"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::PlaylistTags => {
                let body = self.get(info_url(""), ctx)?;
                status_success(&body)?;
                let mut tags = Vec::new();
                for (name, group) in body["data"]["tagids"]
                    .as_object()
                    .ok_or_else(|| ProviderError::parse("Kugou playlist tags missing"))?
                {
                    for tag in array(&group["data"])? {
                        tags.push(PlaylistTag {
                            id: text(&tag["id"]),
                            name: text(&tag["name"]),
                            group: name.clone(),
                            extra: tag.clone(),
                        });
                    }
                }
                let hot = &body["data"]["hotTag"];
                if number(&hot["status"]) == 1
                    && let Some(groups) = hot["data"].as_object()
                {
                    for tag in groups.values() {
                        tags.push(PlaylistTag {
                            id: text(&tag["special_id"]),
                            name: text(&tag["special_name"]),
                            group: "hot".into(),
                            extra: json!({"hot":true,"source":"kg"}),
                        });
                    }
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
                let sort = if sort.is_empty() { "5" } else { &sort };
                let body=self.get(format!("http://www2.kugou.kugou.com/yueku/v9/special/getSpecial?is_ajax=1&cdn=cdn&t={}&c={}&p={page}",encode(sort),encode(&tag)),ctx)?;
                status_success(&body)?;
                let mut items = array(&body["special_db"])?
                    .iter()
                    .map(|i| playlist(i, true))
                    .collect::<Vec<_>>();
                let info = self.get(info_url(&tag), ctx)?;
                status_success(&info)?;
                if tag.is_empty() && page == 1 && sort == "5" {
                    let data = json!({"appid":1001,"clienttime":1566798337219u64,"clientver":8275,"key":"f1f93580115bb106680d2375f8032d96","mid":"21511157a05844bd085308bc76ef3343","platform":"pc","userid":"262643156","return_min":6,"return_max":15});
                    let body = self.request(
                        HttpRequest::post(
                            "http://everydayrec.service.kugou.com/guess_special_recommend",
                            bytes(&data)?,
                        )
                        .header("Content-Type", "application/json")
                        .header("User-Agent", "KuGou2012-8275-web_browser_event_handler"),
                        ctx,
                    )?;
                    status_success(&body)?;
                    let mut rec = array(&body["data"]["special_list"])?
                        .iter()
                        .map(|i| playlist(i, true))
                        .collect::<Vec<_>>();
                    rec.append(&mut items);
                    items = rec;
                }
                Ok(ProviderResponse::Playlists(Page::new(
                    items,
                    number(&info["data"]["params"]["total"]),
                    page,
                    number(&info["data"]["params"]["pagesize"]) as u32,
                )))
            }
            ProviderRequest::SearchPlaylists {
                keyword,
                page,
                limit,
            } => {
                page_args(page, limit)?;
                let body=self.get(format!("http://msearchretry.kugou.com/api/v3/search/special?keyword={}&page={page}&pagesize={limit}&showtype=10&filter=0&version=7910&sver=2",encode(&keyword)),ctx)?;
                success(&body)?;
                Ok(ProviderResponse::Playlists(Page::new(
                    array(&body["data"]["info"])?
                        .iter()
                        .map(|i| playlist(i, true))
                        .collect(),
                    number(&body["data"]["total"]),
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
                root_comment_id,
                ..
            } => {
                page_args(page, limit)?;
                let url = if kind == CommentKind::Replies {
                    let root = root_comment_id.ok_or_else(|| {
                        ProviderError::invalid("Kugou replies require root_comment_id")
                    })?;
                    let audio = if id.len() == 32 {
                        let info = self.audio_infos(&[json!({"hash":id})], true, ctx)?;
                        text(
                            &info
                                .first()
                                .ok_or_else(|| {
                                    ProviderError::parse("Kugou reply track is missing")
                                })?
                                .platform
                                .song_id,
                        )
                    } else {
                        id.clone()
                    };
                    format!(
                        "http://comment.service.kugou.com/index.php?r=commentsv2/getReplyWithLike&code=fc4be23b4e972707f36b8a828a93ba8a&p={page}&pagesize={limit}&ver=1.01&clientver=8373&kugouid=687373022&need_show_image=1&appid=1001&childrenid={}&tid={}",
                        encode(&audio),
                        encode(&root)
                    )
                } else {
                    let params = format!(
                        "dfid=0&mid=16249512204336365674023395779019&clienttime={}&uuid=0&extdata={}&appid=1005&code=fc4be23b4e972707f36b8a828a93ba8a&schash={}&clientver=11409&p={page}&clienttoken=&pagesize={limit}&ver=10&kugouid=0",
                        now_ms(),
                        encode(&id),
                        encode(&id)
                    );
                    format!(
                        "http://m.comment.service.kugou.com/r/v1/rank/{}?{params}&signature={}",
                        if kind == CommentKind::Hot {
                            "topliked"
                        } else {
                            "newest"
                        },
                        signature_params(&params, false, "")
                    )
                };
                let ua = if kind == CommentKind::Replies {
                    "Android712-AndroidPhone-8983-18-0-COMMENT-wifi"
                } else {
                    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/107.0.0.0 Safari/537.36 Edg/107.0.1418.24"
                };
                let body = self.request(HttpRequest::get(url).header("User-Agent", ua), ctx)?;
                success(&body)?;
                let list = body["list"]
                    .as_array()
                    .map(|a| a.iter().map(comment).collect())
                    .unwrap_or_default();
                Ok(ProviderResponse::Comments(Page::new(
                    list,
                    number(&body["count"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::Lyrics {
                track,
                word_by_word,
            } => {
                let hash = track.metadata.get("hash").map(text).unwrap_or_default();
                if hash.is_empty() {
                    return Err(ProviderError::invalid("Kugou lyrics require hash"));
                }
                let seconds = track
                    .metadata
                    .get("_interval")
                    .map(number)
                    .or_else(|| track.duration_ms.map(|v| v / 1000))
                    .unwrap_or(0);
                let request = lyric_headers(HttpRequest::get(format!(
                    "http://lyrics.kugou.com/search?ver=1&man=yes&client=pc&keyword={}&hash={}&timelength={seconds}&lrctxt=1",
                    encode(&track.title),
                    encode(&hash)
                )));
                let body = self.request(request, ctx)?;
                let candidate = array(&body["candidates"])?
                    .first()
                    .ok_or_else(|| ProviderError::upstream("Kugou lyric candidates are empty"))?;
                let format = if number(&candidate["krctype"]) == 1
                    && number(&candidate["contenttype"]) != 1
                {
                    "krc"
                } else {
                    "lrc"
                };
                let body=self.request(lyric_headers(HttpRequest::get(format!("http://lyrics.kugou.com/download?ver=1&client=pc&id={}&accesskey={}&fmt={format}&charset=utf8",encode(&text(&candidate["id"])),encode(&text(&candidate["accesskey"]))))),ctx)?;
                let format = body["fmt"].as_str().unwrap_or(format);
                let result = match format {
                    "krc" => {
                        let bundle = crate::lyrics::codec::decode_krc(&text(&body["content"]))
                            .map_err(|e| ProviderError::parse(e.to_string()))?;
                        Lyrics {
                            lyric: bundle.lyric.text().unwrap_or_default().into(),
                            translated: bundle.tlyric.text().map(str::to_owned),
                            romanized: bundle.rlyric.text().map(str::to_owned),
                            word_by_word: if word_by_word {
                                bundle.lxlyric.text().map(str::to_owned)
                            } else {
                                None
                            },
                            extra: Value::Null,
                        }
                    }
                    "lrc" => {
                        let bytes = STANDARD
                            .decode(text(&body["content"]))
                            .map_err(|e| ProviderError::parse(e.to_string()))?;
                        Lyrics {
                            lyric: String::from_utf8(bytes)
                                .map_err(|e| ProviderError::parse(e.to_string()))?,
                            translated: Some(String::new()),
                            romanized: Some(String::new()),
                            word_by_word: word_by_word.then(String::new),
                            extra: Value::Null,
                        }
                    }
                    _ => {
                        return Err(ProviderError::parse(format!(
                            "unknown Kugou lyric format {format}"
                        )));
                    }
                };
                Ok(ProviderResponse::Lyrics(result))
            }
            ProviderRequest::Picture { track } => {
                let hash = track.metadata.get("hash").map(text).unwrap_or_default();
                if hash.is_empty() {
                    return Err(ProviderError::invalid("Kugou picture requires hash"));
                }
                let mut audio = track.platform.song_id.clone();
                if text(&audio).len() == 32 {
                    audio = track
                        .metadata
                        .get("audioId")
                        .map(|i| json!(text(i).split('_').next().unwrap_or_default()))
                        .ok_or_else(|| {
                            ProviderError::invalid("legacy Kugou hash id requires audioId")
                        })?;
                }
                let body = json!({"appid":1001,"area_code":"1","behavior":"play","clientver":"9020","need_hash_offset":1,"relate":1,"resource":[{"album_audio_id":audio,"album_id":track.metadata.get("albumId").cloned().unwrap_or(Value::Null),"hash":hash,"id":0,"name":format!("{} - {}.mp3",track.artists.join("、"),track.title),"type":"audio"}],"token":"","userid":2626431536u64,"vip":1});
                let response = self.request(
                    lyric_headers(
                        HttpRequest::post(
                            "http://media.store.kugou.com/v1/get_res_privilege",
                            bytes(&body)?,
                        )
                        .header("Content-Type", "application/json"),
                    ),
                    ctx,
                )?;
                success(&response)?;
                let info = &response["data"][0]["info"];
                let mut image = text(&info["image"]);
                if let Some(size) = info["imgsize"].as_array().and_then(|a| a.first()) {
                    image = image.replace("{size}", &text(size));
                }
                if image.is_empty() {
                    return Err(ProviderError::parse("Kugou picture is empty"));
                }
                Ok(ProviderResponse::Picture(image))
            }
            other => Err(ProviderError::Unsupported {
                platform: Platform::Kg,
                operation: other.operation().into(),
            }),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn bytes(value: &Value) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(value).map_err(|e| ProviderError::parse(e.to_string()))
}
fn array(value: &Value) -> Result<&Vec<Value>, ProviderError> {
    value
        .as_array()
        .ok_or_else(|| ProviderError::parse("Kugou response list is missing or invalid"))
}
fn success(body: &Value) -> Result<(), ProviderError> {
    let code = body
        .get("error_code")
        .or_else(|| body.get("errcode"))
        .or_else(|| body.get("err_code"));
    if code.is_none() || code.is_some_and(|v| v.as_f64() != Some(0.0) && v.as_str() != Some("0")) {
        Err(ProviderError::upstream(format!(
            "Kugou response code: {}",
            code.unwrap_or(&Value::Null)
        )))
    } else {
        Ok(())
    }
}
fn status_success(body: &Value) -> Result<(), ProviderError> {
    if number(&body["status"]) != 1 {
        Err(ProviderError::upstream("Kugou response status is not 1"))
    } else {
        Ok(())
    }
}
fn unwrap_data(body: Value) -> Value {
    if let Some(data) = body.get("data").filter(|v| !v.is_null()) {
        return data.clone();
    }
    if body["info"].is_array() {
        body
    } else {
        body["info"].clone()
    }
}
fn optional(value: &Value) -> Option<String> {
    let s = decode_name(&text(value));
    (!s.is_empty()).then_some(s)
}
fn decode_name(value: &str) -> String {
    let without_tags = HTML_TAGS.replace_all(value, "");
    let source = without_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    HTML_ENTITIES
        .replace_all(&source, |c: &regex::Captures<'_>| {
            let code = if c[1].starts_with('x') {
                u32::from_str_radix(&c[1][1..], 16).ok()
            } else {
                c[1].parse::<u32>().ok()
            };
            code.and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| c[0].into())
        })
        .into_owned()
}
fn size(value: u64) -> String {
    let mut n = value as f64;
    let mut unit = 0;
    while n >= 1024.0 && unit < 4 {
        n /= 1024.0;
        unit += 1;
    }
    format!("{n:.2} {}", ["B", "KiB", "MiB", "GiB", "TiB"][unit])
}
fn lx_interval(seconds: u64) -> String {
    if seconds == 0 {
        "--/--".into()
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}
fn quality(
    raw: &Value,
    pairs: &[(&str, &str, &str)],
) -> (Vec<Value>, serde_json::Map<String, Value>) {
    let mut qualities = Vec::new();
    let mut map = serde_json::Map::new();
    for (kind, key, hash) in pairs {
        let n = number(&raw[*key]);
        if n > 0 {
            let s = size(n);
            qualities.push(json!({"type":kind,"size":s,"hash":raw[*hash]}));
            map.insert((*kind).into(), json!({"size":s,"hash":raw[*hash]}));
        }
    }
    (qualities, map)
}
fn normalize(dto: Value) -> Result<TrackRecord, ProviderError> {
    TrackRecord::from_lx_dto(dto, LxMusicDtoVersion::Legacy)
        .map_err(|e| ProviderError::parse(e.to_string()))
}
fn search_track(i: &Value) -> Result<TrackRecord, ProviderError> {
    let (qualities, map) = quality(
        i,
        &[
            ("128k", "FileSize", "FileHash"),
            ("320k", "HQFileSize", "HQFileHash"),
            ("flac", "SQFileSize", "SQFileHash"),
            ("flac24bit", "ResFileSize", "ResFileHash"),
        ],
    );
    let singer = i["Singers"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| decode_name(&text(&i["name"])))
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();
    let suffix = text(&i["Suffix"]);
    normalize(
        json!({"source":"kg","name":decode_name(&format!("{}{}",text(&i["OriSongName"]),if suffix.is_empty(){String::new()}else{format!(" {suffix}")})),"singer":singer,"albumName":decode_name(&text(&i["AlbumName"])),"albumId":i["AlbumID"],"songmid":i["Audioid"],"albumAudioId":i["MixSongID"],"interval":lx_interval(number(&i["Duration"])),"_interval":i["Duration"],"hash":i["FileHash"],"img":null,"lrc":null,"otherSource":null,"types":qualities,"_types":map,"typeUrl":{}}),
    )
}
fn chart_track(i: &Value) -> Result<TrackRecord, ProviderError> {
    let (qualities, map) = quality(
        i,
        &[
            ("128k", "filesize", "hash"),
            ("320k", "320filesize", "320hash"),
            ("flac", "sqfilesize", "sqhash"),
            ("flac24bit", "filesize_high", "hash_high"),
        ],
    );
    let singer = i["authors"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| decode_name(&text(&i["author_name"])))
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();
    normalize(
        json!({"source":"kg","name":decode_name(&text(&i["songname"])),"singer":singer,"albumName":decode_name(&text(&i["remark"])),"albumId":i["album_id"],"songmid":i["audio_id"],"albumAudioId":i["album_audio_id"],"interval":lx_interval(number(&i["duration"])),"hash":i["hash"],"img":null,"lrc":null,"otherSource":null,"types":qualities,"_types":map,"typeUrl":{}}),
    )
}
fn audio_track(i: &Value) -> Result<TrackRecord, ProviderError> {
    let audio = &i["audio_info"];
    let (qualities, map) = quality(
        audio,
        &[
            ("128k", "filesize", "hash"),
            ("320k", "filesize_320", "hash_320"),
            ("flac", "filesize_flac", "hash_flac"),
            ("flac24bit", "filesize_high", "hash_high"),
        ],
    );
    normalize(
        json!({"source":"kg","name":decode_name(&text(&i["songname"])),"singer":decode_name(&text(&i["author_name"])),"albumName":decode_name(&text(&i["album_info"]["album_name"])),"albumId":i["album_info"]["album_id"],"songmid":audio["audio_id"],"albumAudioId":audio["audio_group_id"],"interval":lx_interval(number(&audio["timelength"])/1000),"hash":audio["hash"],"img":null,"lrc":null,"otherSource":null,"types":qualities,"_types":map,"typeUrl":{}}),
    )
}
fn playlist(i: &Value, prefix: bool) -> Playlist {
    let id = text(&i["specialid"]);
    Playlist {
        id: if prefix { format!("id_{id}") } else { id },
        name: decode_name(&text(&i["specialname"])),
        image_url: optional(&i["img"]).or_else(|| optional(&i["imgurl"])),
        author: optional(&i["nickname"]),
        description: optional(&i["intro"]),
        play_count: i
            .get("total_play_count")
            .or_else(|| i.get("play_count"))
            .or_else(|| i.get("playcount"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            }),
        track_count: i.get("songcount").map(number),
        extra: i.clone(),
    }
}
fn info_url(tag: &str) -> String {
    if tag.is_empty() {
        "http://www2.kugou.kugou.com/yueku/v9/special/getSpecial?is_smarty=1&".into()
    } else {
        format!(
            "http://www2.kugou.kugou.com/yueku/v9/special/getSpecial?is_smarty=1&cdn=cdn&t=5&c={}",
            encode(tag)
        )
    }
}
fn collection(
    info: Playlist,
    list: Vec<TrackRecord>,
    total: u64,
    page: u32,
    limit: u32,
) -> ProviderResponse {
    let offset = (u64::from(page) - 1) * u64::from(limit);
    ProviderResponse::Collection(Collection {
        info,
        tracks: Page::new(
            list.into_iter()
                .skip(offset.min(usize::MAX as u64) as usize)
                .take(limit as usize)
                .collect(),
            total,
            page,
            limit,
        ),
    })
}
fn lyric_headers(request: HttpRequest) -> HttpRequest {
    request
        .header("KG-RC", "1")
        .header("KG-THash", "expand_search_manager.cpp:852736169:451")
        .header("User-Agent", LYRIC_UA)
}
fn capture(source: &str, pattern: &str) -> Option<String> {
    Regex::new(pattern)
        .unwrap()
        .captures(source)
        .map(|c| c[1].to_owned())
}
fn query_value(url: &str, key: &str) -> Option<String> {
    let pattern = format!(r"[?&]{}=([^&#]+)", regex::escape(key));
    capture(url, &pattern)
}
fn page_url(url: &str, page: u64, size: u64) -> String {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut parts = query
        .split('&')
        .filter(|s| !s.is_empty() && !s.starts_with("pagesize=") && !s.starts_with("page="))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    parts.push(format!("pagesize={size}"));
    parts.push(format!("page={page}"));
    format!("{base}?{}", parts.join("&"))
}
fn embedded_json(
    source: &str,
    label: &str,
    start: char,
    end: char,
) -> Result<Value, ProviderError> {
    let position = source
        .find(label)
        .ok_or_else(|| ProviderError::parse(format!("Kugou HTML lacks {label}")))?;
    let tail = &source[position + label.len()..];
    let offset = tail
        .find('=')
        .ok_or_else(|| ProviderError::parse("invalid Kugou HTML assignment"))?;
    let tail = &tail[offset + 1..];
    let opening = tail
        .find(start)
        .ok_or_else(|| ProviderError::parse("invalid Kugou HTML data"))?;
    let tail = &tail[opening..];
    let mut depth = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (pos, c) in tail.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
            continue;
        }
        if c == '"' {
            quoted = true;
        } else if c == start {
            depth += 1;
        } else if c == end {
            depth -= 1;
            if depth == 0 {
                return serde_json::from_str(&tail[..pos + c.len_utf8()])
                    .map_err(|e| ProviderError::parse(e.to_string()));
            }
        }
    }
    Err(ProviderError::parse("unterminated Kugou HTML JSON"))
}
/// Exact android/web sorted-parameter signing used by LX's active Kugou routes.
pub fn signature_params(params: &str, web: bool, body: &str) -> String {
    let key = if web {
        "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt"
    } else {
        "OIlwieks28dk2k092lksi2UIkp"
    };
    let mut pairs = params.split('&').collect::<Vec<_>>();
    pairs.sort_unstable();
    format!(
        "{:x}",
        Md5::digest(format!("{key}{}{body}{key}", pairs.join("")).as_bytes())
    )
}
fn comment(i: &Value) -> Comment {
    let mut content = text(&i["content"]);
    if let Some(ats) = i["atlist"].as_array() {
        for at in ats {
            content = content.replace(
                &format!("[at={}]", text(&at["id"])),
                &format!("@{} ", text(&at["name"])),
            );
        }
    }
    let timestamp = comment_time(&i["addtime"]);
    let data = Comment {
        id: text(&i["id"]),
        text: decode_name(&content),
        author: text(&i["user_name"]),
        avatar_url: optional(&i["user_pic"]),
        timestamp_ms: timestamp,
        likes: number(&i["like"]["likenum"]),
        replies: Vec::new(),
        extra: i.clone(),
    };
    if !text(&i["pcontent"]).is_empty() {
        Comment {
            id: text(&i["id"]),
            text: decode_name(&text(&i["pcontent"])),
            author: text(&i["puser"]),
            avatar_url: None,
            timestamp_ms: None,
            likes: 0,
            replies: vec![data],
            extra: json!({"userId":i["puser_id"],"replyNum":null}),
        }
    } else {
        data
    }
}

fn comment_time(value: &Value) -> Option<u64> {
    if let Some(n) = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
    {
        return Some(if n < 10_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        });
    }
    let s = value.as_str()?;
    let c = COMMENT_TIME.captures(s)?;
    let year = c[1].parse::<i64>().ok()?;
    let month = c[2].parse::<i64>().ok()?;
    let day = c[3].parse::<i64>().ok()?;
    let hour = c[4].parse::<i64>().ok()?;
    let minute = c[5].parse::<i64>().ok()?;
    let second = c[6].parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = [
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
    ][month as usize - 1];
    if day < 1 || day > max_day {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let timezone = match c.get(8).map(|m| m.as_str()) {
        Some("Z") => 0,
        Some(offset) => {
            let clean = offset[1..].replace(':', "");
            let h = clean[..2].parse::<i64>().ok()?;
            let m = clean[2..].parse::<i64>().ok()?;
            if h > 23 || m > 59 {
                return None;
            }
            let secs = h * 3600 + m * 60;
            if offset.starts_with('-') { -secs } else { secs }
        }
        None => 8 * 3600,
    };
    let fraction = c
        .get(7)
        .map(|m| {
            format!("{}{}", m.as_str(), "0".repeat(3 - m.as_str().len()))
                .parse::<i64>()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let millis = (days * 86400 + hour * 3600 + minute * 60 + second - timezone) * 1000 + fraction;
    u64::try_from(millis).ok()
}
