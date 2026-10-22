//! Bounded native LX HTTP framing. Parsed fields never grant authorization.
use crate::sync::{SyncError, SyncErrorKind, SyncResult};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::collections::HashSet;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_TARGET_BYTES: usize = 8 * 1024;
const MAX_AUTH_BYTES: usize = 16 * 1024;
const MAX_CLIENT_ID_BYTES: usize = 256;
const MAX_RESPONSE_WIRE_BYTES: usize = MAX_HEADER_BYTES + MAX_AUTH_BYTES * 2;
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct Request {
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) is_upgrade: bool,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("header_count", &self.headers.len())
            .field("is_upgrade", &self.is_upgrade)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Hello,
    Id,
    Auth,
    Socket,
    Unknown,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn route(&self, base_path: &str) -> Route {
        if base_path.contains("//") {
            return Route::Unknown;
        }
        let base = base_path.trim_end_matches('/');
        if !canonical_base(base) {
            return Route::Unknown;
        }
        let path = self
            .target
            .split_once('?')
            .map_or(self.target.as_str(), |(path, _)| path);
        match path.strip_prefix(base) {
            Some("/hello") if !self.is_upgrade => Route::Hello,
            Some("/id") if !self.is_upgrade => Route::Id,
            Some("/ah") if !self.is_upgrade => Route::Auth,
            Some("/socket") if self.is_upgrade => Route::Socket,
            _ => Route::Unknown,
        }
    }

    pub(crate) fn socket_auth(&self) -> SyncResult<(String, String)> {
        if !self.is_upgrade {
            return Err(invalid("WebSocket upgrade required"));
        }
        let query = self
            .target
            .split_once('?')
            .map(|(_, query)| query)
            .ok_or_else(|| invalid("Missing socket authentication"))?;
        let mut client_id = None;
        let mut token = None;
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode_query_component(key)?;
            if key != "i" && key != "t" {
                continue;
            }
            let value = decode_query_component(value)?;
            let slot = if key == "i" {
                &mut client_id
            } else {
                &mut token
            };
            if slot.replace(value).is_some() {
                return Err(invalid("Duplicate socket authentication field"));
            }
        }
        let client_id = client_id.ok_or_else(|| invalid("Missing client identifier"))?;
        let token = token.ok_or_else(|| invalid("Missing socket token"))?;
        valid_client_id(&client_id)?;
        if token.is_empty() || token.len() > MAX_AUTH_BYTES || token.chars().any(char::is_control) {
            return Err(invalid("Invalid socket token"));
        }
        Ok((client_id, token))
    }
}

fn canonical_base(base: &str) -> bool {
    base.is_empty()
        || (base.starts_with('/')
            && !base.contains(['?', '#', '\\'])
            && !base.contains("//")
            && base
                .split('/')
                .skip(1)
                .all(|part| !matches!(part, "" | "." | ".."))
            && base.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            && valid_percent_encoding(base))
}

pub(crate) fn parse_request(bytes: &[u8]) -> SyncResult<Request> {
    let end = header_end(bytes)?.ok_or_else(|| invalid("Incomplete HTTP headers"))?;
    let (line, headers) = parse_headers(&bytes[..end])?;
    let mut parts = line.split(' ');
    if parts.next() != Some("GET") {
        return Err(invalid("Only GET is supported"));
    }
    let target = parts
        .next()
        .ok_or_else(|| invalid("Invalid HTTP request line"))?;
    if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(invalid("Invalid HTTP request version"));
    }
    if target.len() > MAX_TARGET_BYTES {
        return Err(quota("HTTP target too large"));
    }
    if !target.starts_with('/')
        || target.starts_with("//")
        || target.contains(['#', '\\'])
        || !target.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        || !valid_percent_encoding(target)
    {
        return Err(invalid("Invalid HTTP request target"));
    }
    let mut request = Request {
        target: target.to_owned(),
        headers,
        is_upgrade: false,
    };
    let host = request
        .header("host")
        .ok_or_else(|| invalid("HTTP Host is required"))?;
    if host.is_empty()
        || host.bytes().any(|byte| byte.is_ascii_whitespace())
        || host.contains(['@', '/', '\\', '?', '#', ','])
        || reqwest::Url::parse(&format!("http://{host}/")).is_err()
    {
        return Err(invalid("Invalid HTTP Host"));
    }
    if request.header("transfer-encoding").is_some() {
        return Err(invalid("HTTP request bodies are not supported"));
    }
    if let Some(length) = request.header("content-length")
        && content_length(length)? != 0
    {
        return Err(invalid("HTTP request bodies are not supported"));
    }
    if let Some(id) = request.header("i") {
        valid_client_id(id)?;
    }
    if let Some(message) = request.header("m") {
        if message.len() > MAX_AUTH_BYTES {
            return Err(quota("Authentication header too large"));
        }
        if message.is_empty() || STANDARD.decode(message).is_err() {
            return Err(invalid("Invalid authentication header encoding"));
        }
    }
    reject_browser_request(&request)?;
    let connection = request
        .header("connection")
        .map(connection_tokens)
        .transpose()?
        .unwrap_or_default();
    let upgrade = request.header("upgrade");
    if let Some(upgrade) = upgrade {
        if !upgrade.eq_ignore_ascii_case("websocket")
            || !connection.iter().any(|token| token == "upgrade")
            || request.header("sec-websocket-version") != Some("13")
        {
            return Err(invalid("Invalid WebSocket upgrade"));
        }
        let key = request
            .header("sec-websocket-key")
            .ok_or_else(|| invalid("Missing WebSocket key"))?;
        if STANDARD.decode(key).map_or(true, |key| key.len() != 16) {
            return Err(invalid("Invalid WebSocket key"));
        }
        request.is_upgrade = true;
    } else if connection.iter().any(|token| token == "upgrade")
        || request
            .headers
            .iter()
            .any(|(name, _)| name.starts_with("sec-websocket-"))
    {
        return Err(invalid("Incomplete WebSocket upgrade"));
    }
    if !request.is_upgrade && bytes.len() != end {
        return Err(invalid("Unexpected HTTP request bytes"));
    }
    Ok(request)
}

fn reject_browser_request(request: &Request) -> SyncResult<()> {
    if request.header("origin").is_some() {
        return Err(invalid("Browser Origin is not accepted"));
    }
    // An m field permits the caller to reach the cryptographic authentication
    // step. Its presence is neither authentication nor a capability grant.
    let native_auth_attempt = request.header("m").is_some();
    let metadata = request.headers.iter().any(|(name, _)| {
        name.starts_with("sec-fetch-") || matches!(name.as_str(), "referer" | "referrer" | "ref")
    });
    let browser_agent = request.header("user-agent").is_some_and(|agent| {
        let agent = agent.to_ascii_lowercase();
        [
            "mozilla", "chrome", "chromium", "firefox", "safari", "opera", "edg/",
        ]
        .iter()
        .any(|marker| agent.contains(marker))
    });
    if !native_auth_attempt && (metadata || browser_agent) {
        return Err(invalid("Browser request metadata is not accepted"));
    }
    Ok(())
}

fn valid_client_id(id: &str) -> SyncResult<()> {
    if id.is_empty() || id.len() > MAX_CLIENT_ID_BYTES || id.chars().any(char::is_control) {
        Err(invalid("Invalid client identifier"))
    } else {
        Ok(())
    }
}

fn connection_tokens(value: &str) -> SyncResult<Vec<String>> {
    let mut tokens = Vec::new();
    for token in value.split(',') {
        let token = token.trim_matches([' ', '\t']);
        if token.is_empty() || !token.bytes().all(header_name_byte) {
            return Err(invalid("Invalid Connection header"));
        }
        let token = token.to_ascii_lowercase();
        if matches!(
            token.as_str(),
            "host" | "i" | "m" | "origin" | "content-length" | "transfer-encoding"
        ) || tokens.contains(&token)
        {
            return Err(invalid("Ambiguous Connection header"));
        }
        tokens.push(token);
    }
    Ok(tokens)
}

pub(crate) async fn read_request<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> SyncResult<(Request, Vec<u8>)> {
    tokio::time::timeout(READ_TIMEOUT, async {
        let bytes = read_header_prefix(stream).await?;
        let request = parse_request(&bytes)?;
        Ok((request, bytes))
    })
    .await
    .map_err(|_| cancelled("HTTP request timed out"))?
}

pub(crate) fn response(code: u16, body: &str) -> Vec<u8> {
    let (code, body) = if body.len() > MAX_AUTH_BYTES {
        (413, "Response too large")
    } else if !(200..=599).contains(&code) {
        (500, "Invalid response status")
    } else {
        (code, body)
    };
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Response",
    };
    let mut bytes = format!(
        "HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n",
        body.len()
    ).into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

#[derive(Clone, Copy)]
enum BodyFraming {
    Length(usize),
    Chunked,
}

fn response_head(bytes: &[u8]) -> SyncResult<(u16, usize, BodyFraming)> {
    let end = header_end(bytes)?.ok_or_else(|| invalid("Incomplete HTTP response headers"))?;
    let (line, headers) = parse_headers(&bytes[..end])?;
    let mut parts = line.splitn(3, ' ');
    if !matches!(parts.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
        return Err(invalid("Invalid HTTP response version"));
    }
    let code = parts
        .next()
        .ok_or_else(|| invalid("Missing HTTP response status"))?;
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("Invalid HTTP response status"));
    }
    let code: u16 = code
        .parse()
        .map_err(|_| invalid("Invalid HTTP response status"))?;
    if !(200..=599).contains(&code) {
        return Err(invalid("Unsupported HTTP response status"));
    }
    let field = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let length = field("content-length");
    let transfer = field("transfer-encoding");
    let framing = match (length, transfer) {
        (Some(length), None) => {
            let length = content_length(length)?;
            if length > MAX_AUTH_BYTES {
                return Err(quota("HTTP response body too large"));
            }
            BodyFraming::Length(length)
        }
        (None, Some(transfer)) if transfer.eq_ignore_ascii_case("chunked") => BodyFraming::Chunked,
        (None, None) => return Err(invalid("HTTP response framing is required")),
        _ => return Err(invalid("Ambiguous HTTP response framing")),
    };
    Ok((code, end, framing))
}

#[cfg(test)]
pub(crate) fn parse_response(bytes: &[u8]) -> SyncResult<(u16, Vec<u8>)> {
    if bytes.len() > MAX_RESPONSE_WIRE_BYTES {
        return Err(quota("HTTP response too large"));
    }
    let (code, end, framing) = response_head(bytes)?;
    let body = framed_body(&bytes[end..], framing)?
        .ok_or_else(|| invalid("Incomplete HTTP response body"))?;
    Ok((code, body))
}

pub(crate) async fn read_response<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> SyncResult<(u16, Vec<u8>)> {
    tokio::time::timeout(READ_TIMEOUT, async {
        let mut bytes = read_header_prefix(stream).await?;
        let (code, end, framing) = response_head(&bytes)?;
        loop {
            if let Some(body) = framed_body(&bytes[end..], framing)? {
                return Ok((code, body));
            }
            if bytes.len() >= MAX_RESPONSE_WIRE_BYTES {
                return Err(quota("HTTP response too large"));
            }
            let mut buffer = [0_u8; 4096];
            let capacity = buffer.len().min(MAX_RESPONSE_WIRE_BYTES - bytes.len());
            let count = stream
                .read(&mut buffer[..capacity])
                .await
                .map_err(|_| invalid("HTTP response read failed"))?;
            if count == 0 {
                return Err(invalid("Incomplete HTTP response body"));
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
    })
    .await
    .map_err(|_| cancelled("HTTP response timed out"))?
}

fn framed_body(bytes: &[u8], framing: BodyFraming) -> SyncResult<Option<Vec<u8>>> {
    match framing {
        BodyFraming::Length(length) => {
            if bytes.len() > length {
                return Err(invalid("Unexpected HTTP response bytes"));
            }
            Ok((bytes.len() == length).then(|| bytes.to_vec()))
        }
        BodyFraming::Chunked => chunked_body(bytes),
    }
}

fn chunked_body(bytes: &[u8]) -> SyncResult<Option<Vec<u8>>> {
    if bytes.len() > MAX_AUTH_BYTES * 2 {
        return Err(quota("Chunked response framing too large"));
    }
    let mut offset = 0;
    let mut body = Vec::new();
    loop {
        let remaining = &bytes[offset..];
        let Some(line_end) = remaining.windows(2).position(|pair| pair == b"\r\n") else {
            if remaining.len() > 1024 {
                return Err(quota("HTTP chunk line too large"));
            }
            return Ok(None);
        };
        if line_end > 1024 {
            return Err(quota("HTTP chunk line too large"));
        }
        let line = &remaining[..line_end];
        if !line.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
            return Err(invalid("Invalid HTTP chunk size"));
        }
        let digits = line.split(|byte| *byte == b';').next().unwrap_or_default();
        if digits.is_empty() || digits.len() > 8 || !digits.iter().all(u8::is_ascii_hexdigit) {
            return Err(invalid("Invalid HTTP chunk size"));
        }
        let digits = std::str::from_utf8(digits).map_err(|_| invalid("Invalid HTTP chunk size"))?;
        let size =
            usize::from_str_radix(digits, 16).map_err(|_| invalid("Invalid HTTP chunk size"))?;
        offset += line_end + 2;
        if size == 0 {
            let trailers = &bytes[offset..];
            let end = if trailers.starts_with(b"\r\n") {
                Some(2)
            } else {
                trailers
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .map(|end| end + 4)
            };
            let Some(end) = end else {
                if trailers.len() > MAX_HEADER_BYTES {
                    return Err(quota("HTTP trailers too large"));
                }
                return Ok(None);
            };
            if end > MAX_HEADER_BYTES {
                return Err(quota("HTTP trailers too large"));
            }
            if end != trailers.len() {
                return Err(invalid("Unexpected chunked response bytes"));
            }
            validate_trailers(&trailers[..end])?;
            return Ok(Some(body));
        }
        if size > MAX_AUTH_BYTES - body.len() {
            return Err(quota("HTTP response body too large"));
        }
        if bytes.len() - offset < size + 2 {
            return Ok(None);
        }
        if &bytes[offset + size..offset + size + 2] != b"\r\n" {
            return Err(invalid("Invalid HTTP chunk terminator"));
        }
        body.extend_from_slice(&bytes[offset..offset + size]);
        offset += size + 2;
    }
}

fn validate_trailers(bytes: &[u8]) -> SyncResult<()> {
    if bytes == b"\r\n" {
        return Ok(());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("Invalid HTTP trailers"))?;
    let lines = text
        .strip_suffix("\r\n\r\n")
        .ok_or_else(|| invalid("Invalid HTTP trailers"))?;
    if lines.split("\r\n").count() > MAX_HEADERS {
        return Err(quota("Too many HTTP trailers"));
    }
    for line in lines.split("\r\n") {
        let (name, _) = parse_header_line(line)?;
        if security_header(&name) {
            return Err(invalid("Security fields are not accepted in HTTP trailers"));
        }
    }
    Ok(())
}

async fn read_header_prefix<S: AsyncRead + Unpin>(stream: &mut S) -> SyncResult<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        if header_end(&bytes)?.is_some() {
            return Ok(bytes);
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(quota("HTTP headers too large"));
        }
        let mut buffer = [0_u8; 4096];
        let capacity = buffer.len().min(MAX_HEADER_BYTES - bytes.len());
        let count = stream
            .read(&mut buffer[..capacity])
            .await
            .map_err(|_| invalid("HTTP header read failed"))?;
        if count == 0 {
            return Err(invalid("Incomplete HTTP headers"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn header_end(bytes: &[u8]) -> SyncResult<Option<usize>> {
    let search = &bytes[..bytes.len().min(MAX_HEADER_BYTES)];
    if let Some(end) = search.windows(4).position(|part| part == b"\r\n\r\n") {
        return Ok(Some(end + 4));
    }
    if bytes.len() >= MAX_HEADER_BYTES {
        Err(quota("HTTP headers too large"))
    } else {
        Ok(None)
    }
}

fn parse_headers(bytes: &[u8]) -> SyncResult<(String, Vec<(String, String)>)> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("Invalid HTTP header text"))?;
    let text = text
        .strip_suffix("\r\n\r\n")
        .ok_or_else(|| invalid("Invalid HTTP headers"))?;
    let mut lines = text.split("\r\n");
    let first = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| invalid("Missing HTTP start line"))?;
    if !first.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(invalid("Invalid HTTP start line"));
    }
    let mut headers = Vec::new();
    let mut security = HashSet::new();
    for line in lines {
        if headers.len() >= MAX_HEADERS {
            return Err(quota("Too many HTTP headers"));
        }
        let (name, value) = parse_header_line(line)?;
        if security_header(&name) && !security.insert(name.clone()) {
            return Err(invalid("Duplicate HTTP security header"));
        }
        headers.push((name, value));
    }
    Ok((first.to_owned(), headers))
}

fn parse_header_line(line: &str) -> SyncResult<(String, String)> {
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| invalid("Invalid HTTP header"))?;
    if name.is_empty()
        || !name.bytes().all(header_name_byte)
        || !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
    {
        return Err(invalid("Invalid HTTP header"));
    }
    Ok((
        name.to_ascii_lowercase(),
        value.trim_matches([' ', '\t']).to_owned(),
    ))
}

fn security_header(name: &str) -> bool {
    matches!(
        name,
        "i" | "m"
            | "host"
            | "origin"
            | "content-length"
            | "transfer-encoding"
            | "upgrade"
            | "connection"
            | "referer"
            | "referrer"
            | "ref"
            | "user-agent"
            | "useragent"
    ) || name.starts_with("sec-websocket-")
        || name.starts_with("sec-fetch-")
}

fn header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn content_length(value: &str) -> SyncResult<usize> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("Invalid HTTP Content-Length"));
    }
    value
        .parse()
        .map_err(|_| quota("HTTP Content-Length too large"))
}

fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            if bytes.len() - offset < 3
                || !bytes[offset + 1..offset + 3]
                    .iter()
                    .all(u8::is_ascii_hexdigit)
            {
                return false;
            }
            offset += 3;
        } else {
            offset += 1;
        }
    }
    true
}

fn decode_query_component(value: &str) -> SyncResult<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        match bytes[offset] {
            b'%' => {
                if bytes.len() - offset < 3 {
                    return Err(invalid("Invalid query encoding"));
                }
                let hex = std::str::from_utf8(&bytes[offset + 1..offset + 3])
                    .map_err(|_| invalid("Invalid query encoding"))?;
                output.push(
                    u8::from_str_radix(hex, 16).map_err(|_| invalid("Invalid query encoding"))?,
                );
                offset += 3;
            }
            b'+' => {
                output.push(b' ');
                offset += 1;
            }
            byte => {
                output.push(byte);
                offset += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| invalid("Invalid query text"))
}

fn invalid(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::InvalidData, message)
}

fn quota(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::Quota, message)
}

fn cancelled(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::Cancelled, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn get(target: &str, extra: &str) -> Vec<u8> {
        format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n{extra}\r\n").into_bytes()
    }

    fn upgrade(target: &str) -> Vec<u8> {
        get(
            target,
            "Upgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n",
        )
    }

    #[test]
    fn routes_are_exact_and_do_not_normalize_paths() {
        for (target, base, route) in [
            ("/hello", "", Route::Hello),
            ("/id", "/", Route::Id),
            ("/native/ah?ignored=1", "/native", Route::Auth),
            ("/native/hello", "/native/", Route::Hello),
            ("/native/hello/", "/native", Route::Unknown),
            ("/native/../hello", "/native", Route::Unknown),
            ("/native/%68ello", "/native", Route::Unknown),
            ("/native2/hello", "/native", Route::Unknown),
            ("/socket", "", Route::Unknown),
        ] {
            assert_eq!(parse_request(&get(target, "")).unwrap().route(base), route);
        }
        assert_eq!(
            parse_request(&upgrade("/native/socket?i=client&t=token"))
                .unwrap()
                .route("/native"),
            Route::Socket
        );
    }

    #[test]
    fn all_security_header_duplicates_are_rejected_case_insensitively() {
        for header in [
            "i",
            "m",
            "Host",
            "Origin",
            "Content-Length",
            "Transfer-Encoding",
            "Upgrade",
            "Connection",
            "Sec-WebSocket-Key",
            "Sec-WebSocket-Version",
            "Sec-WebSocket-Protocol",
            "Sec-Fetch-Site",
            "Referer",
            "Ref",
            "User-Agent",
        ] {
            let bytes = get(
                "/hello",
                &format!("{header}: A\r\n{}: B\r\n", header.to_ascii_uppercase()),
            );
            let error = parse_request(&bytes).unwrap_err();
            assert_eq!(error.message, "Duplicate HTTP security header", "{header}");
        }
    }

    #[test]
    fn rejects_request_smuggling_and_invalid_auth_fields() {
        for bytes in [
            get("/hello", "Content-Length: 1\r\n"),
            get("/hello", "Transfer-Encoding: chunked\r\n"),
            get("/hello", "Connection: i\r\n"),
            get("/hello", "Content-Length : 0\r\n"),
            get("/hello", " m: YQ==\r\n"),
            get("/hello", "i: \r\n"),
            get("/hello", "m: !!!\r\n"),
            get("/hello", "X-Test: bad\nInjected: true\r\n"),
            get("http://localhost/hello", ""),
            get("//localhost/hello", ""),
            get("/hello#fragment", ""),
            get("/hello%0", ""),
        ] {
            assert!(
                parse_request(&bytes).is_err(),
                "{:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
        let mut pipeline = get("/hello", "Content-Length: 0\r\n");
        pipeline.extend(get("/id", ""));
        assert!(parse_request(&pipeline).is_err());
        assert!(parse_request(b"POST /ah HTTP/1.1\r\nHost: x\r\n\r\n").is_err());
        assert!(parse_request(b"GET /hello HTTP/1.1\r\n\r\n").is_err());
    }

    #[test]
    fn enforces_header_target_identifier_and_count_limits() {
        let target = format!("/{}", "x".repeat(MAX_TARGET_BYTES - 1));
        assert!(parse_request(&get(&target, "")).is_ok());
        let overhead = get("/hello", "X-Pad: \r\n").len();
        let exact = get(
            "/hello",
            &format!("X-Pad: {}\r\n", "x".repeat(MAX_HEADER_BYTES - overhead)),
        );
        assert_eq!(exact.len(), MAX_HEADER_BYTES);
        assert!(parse_request(&exact).is_ok());
        let target = format!("/{}", "x".repeat(MAX_TARGET_BYTES));
        assert_eq!(
            parse_request(&get(&target, "")).unwrap_err().kind,
            SyncErrorKind::Quota
        );
        let headers = (0..MAX_HEADERS)
            .map(|index| format!("X-{index}: x\r\n"))
            .collect::<String>();
        assert_eq!(
            parse_request(&get("/hello", &headers)).unwrap_err().kind,
            SyncErrorKind::Quota
        );
        let oversized = get(
            "/hello",
            &format!("X-Test: {}\r\n", "x".repeat(MAX_HEADER_BYTES)),
        );
        assert_eq!(
            parse_request(&oversized).unwrap_err().kind,
            SyncErrorKind::Quota
        );
        assert!(
            parse_request(&get(
                "/ah",
                &format!("i: {}\r\n", "x".repeat(MAX_CLIENT_ID_BYTES + 1))
            ))
            .is_err()
        );
    }

    #[test]
    fn browser_metadata_cannot_bypass_origin_or_crypto() {
        for metadata in [
            "Origin: null\r\n",
            "Sec-Fetch-Site: cross-site\r\n",
            "Referer: https://example.test/\r\n",
            "User-Agent: Mozilla/5.0\r\n",
        ] {
            assert!(parse_request(&get("/hello", metadata)).is_err());
        }
        assert!(
            parse_request(&get(
                "/ah",
                "m: YQ==\r\nUser-Agent: Mozilla/5.0\r\nSec-Fetch-Site: cross-site\r\n"
            ))
            .is_ok()
        );
        assert!(parse_request(&get("/ah", "m: YQ==\r\nOrigin: https://example.test\r\n")).is_err());
        assert!(parse_request(&get("/hello", "User-Agent: lx-native\r\n")).is_ok());
    }

    #[test]
    fn websocket_query_decodes_once_and_rejects_duplicate_credentials() {
        let request = parse_request(&upgrade("/socket?i=client%2B1&t=a%2Fb%3D&other=1")).unwrap();
        assert_eq!(
            request.socket_auth().unwrap(),
            ("client+1".into(), "a/b=".into())
        );
        for query in [
            "i=one&i=two&t=t",
            "i=one&%69=two&t=t",
            "i=one&t=a&t=b",
            "i=one",
            "t=t",
            "i=&t=t",
            "i=%00&t=t",
            "i=one&t=%FF",
        ] {
            assert!(
                parse_request(&upgrade(&format!("/socket?{query}")))
                    .unwrap()
                    .socket_auth()
                    .is_err(),
                "{query}"
            );
        }
    }

    #[test]
    fn websocket_prefetched_frame_bytes_are_not_parsed_as_http() {
        let mut bytes = upgrade("/socket?i=client&t=token");
        bytes.extend_from_slice(&[0x82, 0x80, 0x01, 0x02, 0x03, 0x04]);
        assert!(parse_request(&bytes).unwrap().is_upgrade);
        let invalid = get(
            "/socket",
            "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: YQ==\r\nSec-WebSocket-Version: 13\r\n",
        );
        assert!(parse_request(&invalid).is_err());
    }

    #[test]
    fn response_has_utf8_byte_length_and_bounded_fixed_headers() {
        let bytes = response(200, "中文");
        assert!(String::from_utf8_lossy(&bytes).contains("Content-Length: 6\r\n"));
        assert_eq!(
            parse_response(&bytes).unwrap(),
            (200, "中文".as_bytes().to_vec())
        );
        let bytes = response(200, &"x".repeat(MAX_AUTH_BYTES + 1));
        assert_eq!(
            parse_response(&bytes).unwrap(),
            (413, b"Response too large".to_vec())
        );
    }

    #[test]
    fn chunked_response_is_complete_bounded_and_preserves_all_data() {
        let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;example=yes\r\nabc\r\n2\r\nde\r\n0\r\nX-Trace: done\r\n\r\n";
        assert_eq!(parse_response(bytes).unwrap(), (200, b"abcde".to_vec()));
        for body in [
            "3\r\nab",
            "3\r\nabc!!0\r\n\r\n",
            "0\r\n\r\nextra",
            "0\r\nContent-Length: 0\r\n\r\n",
        ] {
            let bytes = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}");
            assert!(parse_response(bytes.as_bytes()).is_err());
        }
        let oversized = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4001\r\n";
        assert_eq!(
            parse_response(oversized).unwrap_err().kind,
            SyncErrorKind::Quota
        );
    }

    #[test]
    fn response_framing_rejects_truncation_ambiguity_and_extra_bytes() {
        for bytes in [
            b"HTTP/1.1 200 OK\r\n\r\nbody".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nab".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nab".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\na".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\na"
                .as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 16385\r\n\r\n".as_slice(),
        ] {
            assert!(parse_response(bytes).is_err());
        }
    }

    #[tokio::test]
    async fn async_request_reader_preserves_the_entire_upgrade_prefix() {
        let mut bytes = upgrade("/socket?i=client&t=token");
        bytes.extend_from_slice(&[0x82, 0x80, 0, 1, 2, 3]);
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        writer.write_all(&bytes).await.unwrap();
        let (request, prefix) = read_request(&mut reader).await.unwrap();
        assert!(request.is_upgrade);
        assert_eq!(prefix, bytes);
    }

    #[tokio::test]
    async fn async_response_reader_handles_fragmented_chunked_body() {
        let (mut writer, mut reader) = tokio::io::duplex(7);
        let write = async move {
            for part in [
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
                b"3\r\na",
                b"bc\r\n",
                b"0\r\n\r\n",
            ] {
                writer.write_all(part).await.unwrap();
            }
            writer.shutdown().await.unwrap();
        };
        let (result, ()) = tokio::join!(read_response(&mut reader), write);
        assert_eq!(result.unwrap(), (200, b"abc".to_vec()));
    }

    #[tokio::test]
    async fn async_readers_reject_incomplete_eof() {
        let mut request = b"GET /hello HTTP/1.1\r\nHost: localhost\r\n".as_slice();
        assert!(read_request(&mut request).await.is_err());
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na".as_slice();
        assert!(read_response(&mut response).await.is_err());
    }
}
