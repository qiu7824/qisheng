//! Approved-script HTTP bridge. No Boa value leaves the JavaScript thread.
//! Requests run on an owned Tokio runtime; cancellation drops the request future.

#[cfg(test)]
use anyhow::bail;
use anyhow::{Context as _, Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use boa_engine::{Context, JsResult, JsString, JsValue, NativeFunction};
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    time::Duration,
};
use tokio::sync::mpsc::{self, Receiver, Sender};

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_HTTP_TIMEOUT_MS: u64 = 60_000;
const MAX_CONCURRENT: usize = 8;
const MAX_STARTED_PER_OPERATION: usize = 128;
const MAX_OPTIONS_BYTES: usize = 8 * 1024 * 1024;

pub struct HttpBridge {
    executor: Option<tokio::runtime::Runtime>,
    remote_allowed: bool,
    next_id: u64,
    started: usize,
    pending: HashMap<u64, tokio::task::JoinHandle<()>>,
    sender: Sender<(u64, Result<Value>)>,
    receiver: Receiver<(u64, Result<Value>)>,
}

pub fn register(context: &mut Context, remote_allowed: bool) -> Result<Rc<RefCell<HttpBridge>>> {
    let (sender, receiver) = mpsc::channel(MAX_CONCURRENT);
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let bridge = Rc::new(RefCell::new(HttpBridge {
        executor: Some(executor),
        remote_allowed,
        next_id: 1,
        started: 0,
        pending: HashMap::new(),
        sender,
        receiver,
    }));
    context.insert_data(bridge.clone());
    for (name, length, function) in [
        (
            "__lx_http_start",
            2,
            native_start as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
        ),
        ("__lx_http_cancel", 1, native_cancel),
        ("__lx_http_poll", 0, native_poll),
    ] {
        context
            .register_global_callable(
                JsString::from(name),
                length,
                NativeFunction::from_fn_ptr(function),
            )
            .map_err(|error| anyhow!("Could not register asynchronous HTTP bridge: {error}"))?;
    }
    Ok(bridge)
}

impl HttpBridge {
    pub fn begin_operation(&mut self) {
        self.cancel_all();
        self.started = 0;
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn cancel_all(&mut self) {
        for (_, task) in self.pending.drain() {
            task.abort();
        }
        while self.receiver.try_recv().is_ok() {}
    }

    fn cancel(&mut self, id: u64) {
        if let Some(task) = self.pending.remove(&id) {
            task.abort();
        }
    }

    fn start(&mut self, url: &str, options: &str) -> Result<u64> {
        ensure!(
            self.pending.len() < MAX_CONCURRENT,
            "HTTP concurrent request limit (8) exceeded"
        );
        ensure!(
            self.started < MAX_STARTED_PER_OPERATION,
            "HTTP operation request limit (128) exceeded"
        );
        let specification = RequestSpec::parse(url, options, self.remote_allowed)?;
        let id = self.next_id;
        self.next_id += 1;
        self.started += 1;
        let sender = self.sender.clone();
        let task = self
            .executor
            .as_ref()
            .context("HTTP runtime is closed")?
            .spawn(async move {
                let deadline = Duration::from_millis(specification.timeout_ms);
                let result = match tokio::time::timeout(deadline, perform(specification)).await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("HTTP request timed out")),
                };
                let _ = sender.send((id, result)).await;
            });
        self.pending.insert(id, task);
        Ok(id)
    }

    /// Delivers one completion at a time to bound transient JS/JSON memory.
    fn poll(&mut self) -> Value {
        while let Ok((id, result)) = self.receiver.try_recv() {
            if self.pending.remove(&id).is_none() {
                continue;
            }
            return match result {
                Ok(response) => json!([{ "id": id, "response": response }]),
                Err(error) => json!([{ "id": id, "error": format!("{error:#}") }]),
            };
        }
        json!([])
    }
}

impl Drop for HttpBridge {
    fn drop(&mut self) {
        self.cancel_all();
        if let Some(executor) = self.executor.take() {
            executor.shutdown_background();
        }
    }
}

fn bridge(context: &Context) -> Result<Rc<RefCell<HttpBridge>>> {
    context
        .get_data::<Rc<RefCell<HttpBridge>>>()
        .cloned()
        .context("HTTP bridge is unavailable")
}

fn json_string(value: Value) -> JsValue {
    JsValue::from(JsString::from(value.to_string()))
}

fn native_start(_: &JsValue, arguments: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let result = (|| {
        let url = arguments
            .first()
            .and_then(JsValue::as_string)
            .context("lx.request URL must be a string")?
            .to_std_string_escaped();
        let options = arguments
            .get(1)
            .and_then(JsValue::as_string)
            .context("lx.request options must be an object")?
            .to_std_string_escaped();
        bridge(context)?.borrow_mut().start(&url, &options)
    })();
    Ok(json_string(match result {
        Ok(id) => json!({ "id": id }),
        Err(error) => json!({ "error": format!("{error:#}") }),
    }))
}

fn native_cancel(_: &JsValue, arguments: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = arguments
        .first()
        .and_then(JsValue::as_number)
        .filter(|id| *id >= 1.0)
        && let Ok(bridge) = bridge(context)
    {
        bridge.borrow_mut().cancel(id as u64);
    }
    Ok(JsValue::undefined())
}

fn native_poll(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(json_string(match bridge(context) {
        Ok(bridge) => bridge.borrow_mut().poll(),
        Err(_) => json!([]),
    }))
}

struct RequestSpec {
    url: Url,
    method: Method,
    headers: HeaderMap,
    payload: Payload,
    timeout_ms: u64,
}
enum Payload {
    Empty,
    Bytes(Vec<u8>),
}

impl RequestSpec {
    fn parse(url: &str, options: &str, remote_allowed: bool) -> Result<Self> {
        ensure!(url.len() <= 8192, "HTTP URL exceeds 8192-byte limit");
        let mut url = Url::parse(url).context("Invalid lx.request URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "Only HTTP(S) requests are supported"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "HTTP URL credentials are forbidden"
        );
        let local = matches!(
            url.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        );
        ensure!(
            remote_allowed || local,
            "Network access denied: this runtime only permits loopback requests"
        );
        if url.host_str() == Some("localhost") {
            url.set_host(Some("127.0.0.1"))?;
        }
        if let Ok(ip) = url
            .host_str()
            .unwrap()
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
        {
            ensure!(
                ip.is_loopback() || public_address(ip),
                "Private, link-local or special-purpose HTTP destinations are forbidden"
            );
        }
        ensure!(
            options.len() <= MAX_OPTIONS_BYTES,
            "HTTP options exceed 8 MiB serialization limit"
        );
        let options: Value = serde_json::from_str(options).context("Invalid HTTP options")?;
        ensure!(options.is_object(), "lx.request options must be an object");
        let method = options
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("get")
            .to_ascii_uppercase();
        ensure!(
            matches!(
                method.as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
            ),
            "Unsupported HTTP method {method}"
        );
        let method = Method::from_bytes(method.as_bytes())?;
        let timeout_ms = options
            .get("timeout")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.min(MAX_HTTP_TIMEOUT_MS as f64).max(1.0) as u64)
            .unwrap_or(MAX_HTTP_TIMEOUT_MS);
        let mut headers = HeaderMap::new();
        if let Some(given) = options.get("headers").filter(|value| !value.is_null()) {
            let given = given
                .as_object()
                .context("HTTP headers must be an object")?;
            ensure!(given.len() <= 128, "HTTP header count exceeds 128");
            let mut length = 0;
            for (name, value) in given {
                let value = value
                    .as_str()
                    .context("HTTP header values must be strings")?;
                length += name.len() + value.len();
                ensure!(length <= 32768, "HTTP headers exceed 32 KiB");
                ensure!(
                    !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "host"
                            | "content-length"
                            | "connection"
                            | "proxy-authorization"
                            | "proxy-connection"
                            | "transfer-encoding"
                            | "upgrade"
                            | "te"
                            | "trailer"
                    ),
                    "HTTP transport header override is forbidden: {name}"
                );
                headers.insert(
                    HeaderName::from_bytes(name.as_bytes())?,
                    HeaderValue::from_str(value)?,
                );
            }
        }
        // Match the upstream fork's JS truthiness selection, including empty object/array.
        let payload =
            if let Some(body) = options.get("body").filter(|value| js_truthy(value)) {
                let byte_body = binary(body)?;
                let is_binary = byte_body.is_some();
                let is_json = headers
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    == Some("application/json");
                let bytes = if let Some(bytes) = byte_body {
                    bytes
                } else if method != Method::GET && body.is_string() {
                    body.as_str().unwrap().as_bytes().to_vec()
                } else if is_json {
                    if let Some(text) = body.as_str() {
                        text.as_bytes().to_vec()
                    } else {
                        serde_json::to_vec(body)?
                    }
                } else {
                    form_encode(body)?.into_bytes()
                };
                ensure!(
                    bytes.len() <= MAX_REQUEST_BYTES,
                    "HTTP request body exceeds 1 MiB"
                );
                if method == Method::GET && !is_json && !is_binary {
                    url.set_query(Some(std::str::from_utf8(&bytes)?));
                    Payload::Empty
                } else {
                    if !is_json {
                        headers.entry(reqwest::header::CONTENT_TYPE).or_insert(
                            HeaderValue::from_static("application/x-www-form-urlencoded"),
                        );
                    }
                    Payload::Bytes(bytes)
                }
            } else if let Some(form) = options
                .get("form")
                .filter(|value| js_truthy(value))
                .or_else(|| options.get("formData").filter(|value| js_truthy(value)))
            {
                let encoded = if method != Method::GET && form.is_string() {
                    form.as_str().unwrap().as_bytes().to_vec()
                } else {
                    form_encode(form)?.into_bytes()
                };
                ensure!(
                    encoded.len() <= MAX_REQUEST_BYTES,
                    "HTTP form exceeds 1 MiB"
                );
                if method == Method::GET {
                    url.set_query(Some(std::str::from_utf8(&encoded)?));
                    Payload::Empty
                } else {
                    headers.entry(reqwest::header::CONTENT_TYPE).or_insert(
                        HeaderValue::from_static("application/x-www-form-urlencoded"),
                    );
                    Payload::Bytes(encoded)
                }
            } else {
                Payload::Empty
            };
        Ok(Self {
            url,
            method,
            headers,
            payload,
            timeout_ms,
        })
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}
fn form_encode(value: &Value) -> Result<String> {
    fn escape(value: &str) -> String {
        let mut output = String::new();
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                output.push(byte as char);
            } else {
                output.push_str(&format!("%{byte:02X}"));
            }
        }
        output
    }
    fn flatten(
        value: &Value,
        prefix: Option<String>,
        depth: usize,
        output: &mut Vec<String>,
    ) -> Result<()> {
        ensure!(depth <= 32, "HTTP form nesting exceeds 32 levels");
        match value {
            Value::Object(values) => {
                for (key, value) in values {
                    let key = match &prefix {
                        Some(prefix) => format!("{prefix}[{}]", escape(key)),
                        None => escape(key),
                    };
                    flatten(value, Some(key), depth + 1, output)?;
                }
            }
            Value::Array(values) => {
                for value in values {
                    flatten(
                        value,
                        prefix.as_ref().map(|prefix| format!("{prefix}[]")),
                        depth + 1,
                        output,
                    )?;
                }
            }
            Value::String(value) if prefix.is_none() => {
                ensure!(
                    value.contains('='),
                    "Top-level form string must contain '='"
                );
                output.push(value.clone());
            }
            value => {
                let prefix =
                    prefix.context("Top-level form value must be an object or query string")?;
                let text = if value.is_null() {
                    String::new()
                } else if let Some(value) = value.as_str() {
                    value.to_owned()
                } else {
                    value.to_string()
                };
                output.push(format!("{prefix}={}", escape(&text)));
            }
        }
        ensure!(output.len() <= 16384, "HTTP form field count exceeds 16384");
        Ok(())
    }
    let mut output = Vec::new();
    flatten(value, None, 0, &mut output)?;
    Ok(output.join("&"))
}
fn binary(value: &Value) -> Result<Option<Vec<u8>>> {
    let data = value.get("__lxBytes").or_else(|| {
        if value.get("type").and_then(Value::as_str) == Some("Buffer") {
            value.get("data")
        } else {
            None
        }
    });
    let Some(data) = data else {
        return Ok(None);
    };
    let data = data
        .as_array()
        .context("Binary HTTP body must contain a byte array")?;
    ensure!(
        data.len() <= MAX_REQUEST_BYTES,
        "Binary HTTP body exceeds 1 MiB"
    );
    data.iter()
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value <= 255)
                .map(|value| value as u8)
                .context("Invalid HTTP body byte")
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || octets[0] == 0
                || octets[0] >= 240
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || ip.is_documentation())
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_address(IpAddr::V4(ip));
            }
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000
                && !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && (segments[0] & 0xfe00) != 0xfc00
                && (segments[0] & 0xffc0) != 0xfe80
                && !(segments[0] == 0x2001 && segments[1] == 0xdb8)
        }
    }
}

async fn perform(specification: RequestSpec) -> Result<Value> {
    let host = specification
        .url
        .host_str()
        .context("HTTP host is missing")?
        .trim_matches(['[', ']']);
    let port = specification
        .url
        .port_or_known_default()
        .context("HTTP port is missing")?;
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .timeout(Duration::from_millis(specification.timeout_ms));
    if host.parse::<IpAddr>().is_err() {
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .context("HTTP DNS lookup failed")?
            .collect();
        ensure!(
            !addresses.is_empty() && addresses.iter().all(|address| public_address(address.ip())),
            "DNS resolved to a private, link-local or special-purpose destination"
        );
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let client = builder.build()?;
    let mut request = client
        .request(specification.method, specification.url)
        .headers(specification.headers);
    match specification.payload {
        Payload::Empty => {}
        Payload::Bytes(bytes) => request = request.body(bytes),
    }
    let mut response = request
        .send()
        .await
        .context("HTTP request failed (TLS verification remains enabled)")?;
    if let Some(length) = response.content_length() {
        ensure!(
            length <= MAX_RESPONSE_BYTES as u64,
            "HTTP response exceeds 2 MiB"
        );
    }
    let status = response.status();
    let mut headers = serde_json::Map::new();
    for name in response.headers().keys() {
        let values: Vec<String> = response
            .headers()
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap_or("").to_owned())
            .collect();
        let value = if name.as_str() == "set-cookie" {
            Value::Array(values.into_iter().map(Value::String).collect())
        } else {
            Value::String(values.join(if name.as_str() == "cookie" {
                "; "
            } else {
                ", "
            }))
        };
        headers.insert(name.to_string(), value);
    }
    let mut raw = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            raw.len() + chunk.len() <= MAX_RESPONSE_BYTES,
            "HTTP response exceeds 2 MiB"
        );
        raw.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok(
        json!({ "statusCode": status.as_u16(), "statusMessage": status.canonical_reason().unwrap_or(""),
        "headers": headers, "bytes": raw.len(), "raw": STANDARD.encode(&raw), "body": body }),
    )
}

#[cfg(test)]
pub fn test_request(url: &str, options: &str) -> Result<Value> {
    let mut context = Context::default();
    let bridge = register(&mut context, false)?;
    bridge.borrow_mut().start(url, options)?;
    loop {
        if let Some(event) = bridge
            .borrow_mut()
            .poll()
            .as_array()
            .and_then(|events| events.first())
            .cloned()
        {
            if let Some(error) = event.get("error").and_then(Value::as_str) {
                bail!("{error}");
            }
            return Ok(event["response"].clone());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{SourceRequest, SourceSession};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, mpsc},
        thread,
        time::Instant,
    };

    fn server(
        body: &str,
        status: &str,
        headers: &str,
        delay_ms: u64,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let body = body.to_owned();
        let status = status.to_owned();
        let headers = headers.to_owned();
        let handle = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request = read_request(&mut socket);
            thread::sleep(Duration::from_millis(delay_ms));
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
            request
        });
        (base, handle)
    }

    fn read_request(socket: &mut std::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = socket.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn request() -> SourceRequest {
        SourceRequest {
            source: "local".into(),
            action: "musicUrl".into(),
            info: json!({"type":null,"musicInfo":{}}),
        }
    }
    fn source(handler: &str) -> String {
        format!(
            "lx.on('request',{handler}); lx.send('inited',{{sources:{{local:{{type:'music',actions:['musicUrl'],qualitys:[]}}}}}});"
        )
    }

    #[test]
    fn post_json_body_has_priority_and_explicit_headers_arrive() {
        let (base, received) = server(
            "{\"ok\":true}",
            "200 OK",
            "Content-Type: application/json\r\n",
            0,
        );
        let response = test_request(&format!("{base}/post"), r#"{"method":"post","headers":{"Content-Type":"application/json","Authorization":"fixture-public-token","X-Fixture":"yes"},"body":{"message":"original"},"form":{"ignored":"field"},"timeout":1000}"#).unwrap();
        assert_eq!(response["body"]["ok"], true);
        let received = received.join().unwrap();
        assert!(received.starts_with("POST /post HTTP/1.1"));
        assert!(
            received
                .to_ascii_lowercase()
                .contains("authorization: fixture-public-token")
        );
        assert!(received.ends_with("{\"message\":\"original\"}"));
        assert!(!received.contains("ignored"));
    }

    #[test]
    fn fork_form_and_formdata_use_nested_urlencoded_fields() {
        assert_eq!(
            form_encode(&json!({"a":"x y","n":null,"tags":["甲","b"],"nested":{"c d":true}}))
                .unwrap(),
            "a=x%20y&n=&tags[]=%E7%94%B2&tags[]=b&nested[c%20d]=true"
        );
        for field in ["form", "formData"] {
            let (base, received) = server("ok", "200 OK", "", 0);
            let options =
                json!({"method":"POST", field:{"message":"x y","empty":null},"timeout":1000});
            test_request(&base, &options.to_string()).unwrap();
            let received = received.join().unwrap();
            assert!(
                received
                    .to_ascii_lowercase()
                    .contains("content-type: application/x-www-form-urlencoded")
            );
            assert!(received.ends_with("message=x%20y&empty="));
        }
    }

    #[test]
    fn get_form_replaces_query_and_buffer_body_remains_raw() {
        let (base, received) = server("1", "200 OK", "", 0);
        test_request(
            &format!("{base}/get?old=discard"),
            r#"{"form":{"q":"x y"},"timeout":1000}"#,
        )
        .unwrap();
        assert!(
            received
                .join()
                .unwrap()
                .starts_with("GET /get?q=x%20y HTTP/1.1")
        );
        let (base, received) = server("ok", "200 OK", "", 0);
        test_request(
            &base,
            r#"{"method":"POST","body":{"__lxBytes":[65,0,66]},"timeout":1000}"#,
        )
        .unwrap();
        assert!(received.join().unwrap().ends_with("A\0B"));
        let (base, received) = server("ok", "200 OK", "", 0);
        test_request(
            &base,
            r#"{"method":"POST","body":"original plain text","timeout":1000}"#,
        )
        .unwrap();
        assert!(received.join().unwrap().ends_with("original plain text"));
    }

    #[test]
    fn timeout_aborts_socket_and_error_callback_is_asynchronous() {
        let (base, received) = server("late", "200 OK", "", 150);
        let fixture = source(&format!(
            "()=>new Promise((resolve,reject)=>{{let returned=false;lx.request('{base}/slow',{{timeout:20}},(error,response,body)=>{{if(!returned||!error||response!==null||body!==null)return reject(Error('bad timeout callback'));resolve('{base}/tone.wav');}});returned=true;}})"
        ));
        let started = Instant::now();
        assert_eq!(
            SourceSession::new(&fixture)
                .unwrap()
                .request(&request())
                .unwrap(),
            format!("{base}/tone.wav")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        received.join().unwrap();
    }

    #[test]
    fn in_flight_cancel_discards_callback_and_closes_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (slow_seen, slow_received) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let slow_seen = slow_seen.clone();
                handles.push(thread::spawn(move || {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let received = read_request(&mut socket);
                    if received.starts_with("GET /slow ") {
                        slow_seen.send(()).unwrap();
                        let mut byte = [0];
                        assert_eq!(socket.read(&mut byte).unwrap(), 0);
                    } else {
                        thread::sleep(Duration::from_millis(40));
                        write!(
                            socket,
                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                        )
                        .unwrap();
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
        let fixture = source(&format!(
            "()=>new Promise((resolve,reject)=>{{const cancel=lx.request('{base}/slow',{{timeout:1000}},()=>reject(Error('cancelled callback invoked')));lx.request('{base}/fast',{{timeout:1000}},error=>{{cancel();cancel();if(error)reject(error);else resolve('{base}/tone.wav');}});}})"
        ));
        assert_eq!(
            SourceSession::new(&fixture)
                .unwrap()
                .request(&request())
                .unwrap(),
            format!("{base}/tone.wav")
        );
        slow_received.recv_timeout(Duration::from_secs(2)).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn completed_then_cancelled_results_and_cancel_storm_are_bounded() {
        let (base, server) = server("ok", "200 OK", "", 0);
        let mut context = Context::default();
        let bridge = register(&mut context, false).unwrap();
        let id = bridge
            .borrow_mut()
            .start(&base, "{\"timeout\":1000}")
            .unwrap();
        server.join().unwrap();
        for _ in 0..200 {
            if !bridge.borrow().receiver.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(bridge.borrow().receiver.len(), 1);
        bridge.borrow_mut().cancel(id);
        assert_eq!(bridge.borrow_mut().poll(), json!([]));
        bridge.borrow_mut().begin_operation();
        let own_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let own_url = format!("http://{}/", own_listener.local_addr().unwrap());
        for _ in 0..128 {
            let id = bridge
                .borrow_mut()
                .start(&own_url, "{\"timeout\":20}")
                .unwrap();
            bridge.borrow_mut().cancel(id);
        }
        assert!(bridge.borrow().pending.is_empty());
        assert!(bridge.borrow().receiver.len() <= MAX_CONCURRENT);
        assert!(bridge.borrow_mut().start(&own_url, "{}").is_err());
        assert_eq!(bridge.borrow_mut().poll(), json!([]));
    }

    #[test]
    fn concurrent_cap_and_request_response_limits_are_enforced() {
        let mut context = Context::default();
        let bridge = register(&mut context, false).unwrap();
        let own_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let own_url = format!("http://{}/", own_listener.local_addr().unwrap());
        for _ in 0..8 {
            bridge
                .borrow_mut()
                .start(&own_url, "{\"timeout\":20}")
                .unwrap();
        }
        assert!(bridge.borrow_mut().start(&own_url, "{}").is_err());
        bridge.borrow_mut().cancel_all();
        let options = json!({"method":"POST","headers":{"Content-Type":"application/json"},"body":"a".repeat(MAX_REQUEST_BYTES+1)});
        assert!(RequestSpec::parse("http://127.0.0.1:1/", &options.to_string(), false).is_err());
        let (base, received) = server(&"a".repeat(MAX_RESPONSE_BYTES + 1), "200 OK", "", 0);
        assert!(test_request(&base, "{\"timeout\":1000}").is_err());
        received.join().unwrap();
    }

    #[test]
    fn redirects_return_status_without_contacting_location_and_raw_is_wire_bytes() {
        let (base, received) = server(
            "{\"redirect\":true}",
            "302 Found",
            "Location: http://192.168.1.1/private\r\n",
            0,
        );
        assert_eq!(
            test_request(&base, "{\"timeout\":1000}").unwrap()["statusCode"],
            302
        );
        received.join().unwrap();
        let (base, received) = server(
            "compressed-fixture",
            "200 OK",
            "Content-Encoding: gzip\r\n",
            0,
        );
        let response = test_request(&base, "{\"timeout\":1000}").unwrap();
        assert_eq!(response["body"], "compressed-fixture");
        assert_eq!(
            STANDARD.decode(response["raw"].as_str().unwrap()).unwrap(),
            b"compressed-fixture"
        );
        received.join().unwrap();
    }

    #[test]
    fn authorization_is_explicit_and_private_network_destinations_are_denied() {
        for url in [
            "http://169.254.169.254/",
            "http://10.0.0.1/",
            "http://192.168.0.1/",
            "http://[fe80::1]/",
            "http://[::ffff:192.168.0.1]/",
            "http://user:pass@127.0.0.1/",
        ] {
            assert!(RequestSpec::parse(url, "{}", true).is_err(), "{url}");
        }
        assert!(RequestSpec::parse("https://example.com/", "{}", false).is_err());
        // Parsing authorization causes no DNS, network or script execution.
        assert!(RequestSpec::parse("https://example.com/", "{}", true).is_ok());
        for header in ["Host", "Content-Length", "Connection"] {
            assert!(
                RequestSpec::parse(
                    "http://127.0.0.1/",
                    &json!({"headers":{header:"bad"}}).to_string(),
                    false
                )
                .is_err()
            );
        }
        assert_eq!(
            RequestSpec::parse("http://127.0.0.1/", "{\"timeout\":70000}", false)
                .unwrap()
                .timeout_ms,
            60_000
        );
    }

    #[test]
    fn https_rejects_project_owned_self_signed_certificate() {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let provider = rustls::crypto::ring::default_provider();
        let configuration = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(signing_key.serialize_der().into()),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "https://localhost:{}/",
            listener.local_addr().unwrap().port()
        );
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(configuration)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let mut byte = [0];
            assert!(stream.read(&mut byte).is_err());
        });
        let error = test_request(&url, "{\"timeout\":1000}").unwrap_err();
        assert!(
            format!("{error:#}").contains("certificate") || format!("{error:#}").contains("TLS")
        );
        server.join().unwrap();
    }

    #[test]
    fn cancellation_after_poll_before_js_microtask_suppresses_callback() {
        let (base, server) = server("ok", "200 OK", "", 0);
        let mut context = Context::default();
        let bridge = register(&mut context, false).unwrap();
        crate::source_utils::register(&mut context).unwrap();
        context
            .eval(boa_engine::Source::from_bytes(
                "globalThis.__lx_script_info={};",
            ))
            .unwrap();
        context
            .eval(boa_engine::Source::from_bytes(include_str!(
                "source_bootstrap.js"
            )))
            .unwrap();
        context.eval(boa_engine::Source::from_bytes(&format!("let called=false;const cancel=lx.request('{base}',{{timeout:1000}},()=>{{called=true;}});"))).unwrap();
        server.join().unwrap();
        for _ in 0..200 {
            if !bridge.borrow().receiver.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(bridge.borrow().receiver.len(), 1);
        context
            .eval(boa_engine::Source::from_bytes(
                "__lxHost.pollRequests();cancel();cancel();",
            ))
            .unwrap();
        context.run_jobs().unwrap();
        assert_eq!(
            context
                .eval(boa_engine::Source::from_bytes("called"))
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn chunked_response_limit_is_enforced_without_declared_length() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            read_request(&mut socket);
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
                MAX_RESPONSE_BYTES + 1
            );
            let _ = socket.write_all(&vec![b'a'; MAX_RESPONSE_BYTES + 1]);
            let _ = socket.write_all(b"\r\n0\r\n\r\n");
        });
        let error = test_request(&base, "{\"timeout\":2000}").unwrap_err();
        assert!(format!("{error:#}").contains("2 MiB"));
        server.join().unwrap();
    }
}
