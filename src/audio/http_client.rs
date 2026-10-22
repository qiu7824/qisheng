//! HTTP/1 audio DATA frames and response headers have independent fixed bounds.
use anyhow::{Context, Result, bail, ensure};
use http_body_util::{BodyExt, Empty};
use hyper::{
    Request,
    body::{Body, Bytes, Incoming},
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};
use reqwest::{
    StatusCode, Url,
    header::{ACCEPT_ENCODING, HeaderMap, IF_RANGE, RANGE},
};
use std::{sync::Arc, time::Duration};

pub const HTTP_DATA_FRAME_LIMIT: usize = 8192;
const MAX_RESPONSE_HEADERS: usize = 64;
type HttpClient = Client<HttpsConnector<HttpConnector>, Empty<Bytes>>;

#[derive(Clone)]
pub struct LimitedHttpClient {
    client: HttpClient,
}

impl LimitedHttpClient {
    pub fn new(connect_timeout: Duration) -> Result<Self> {
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context("Cannot configure audio TLS protocol versions")?
        .with_root_certificates(webpki_root_store())
        .with_no_client_auth();
        Self::with_tls_config(connect_timeout, config)
    }

    fn with_tls_config(connect_timeout: Duration, config: rustls::ClientConfig) -> Result<Self> {
        ensure!(
            !connect_timeout.is_zero(),
            "Audio TCP connection timeout must be positive"
        );
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        connector.set_connect_timeout(Some(connect_timeout));
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(config)
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        let mut builder = Client::builder(TokioExecutor::new());
        builder
            .http1_read_buf_exact_size(HTTP_DATA_FRAME_LIMIT)
            .http1_max_headers(MAX_RESPONSE_HEADERS)
            .pool_max_idle_per_host(0)
            .timer(TokioTimer::new());
        Ok(Self {
            client: builder.build(connector),
        })
    }

    /// No proxy, redirects, cookies or content decompression are performed.
    /// The transport owns the response-header deadline and cancellation.
    pub async fn get(
        &self,
        url: &Url,
        range: Option<&str>,
        if_range: Option<&str>,
    ) -> Result<LimitedResponse> {
        let mut request = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .header(ACCEPT_ENCODING, "identity");
        if let Some(range) = range {
            request = request.header(RANGE, range);
        }
        if let Some(validator) = if_range {
            request = request.header(IF_RANGE, validator);
        }
        let request = request
            .body(Empty::new())
            .map_err(|_| anyhow::anyhow!("Cannot encode approved audio request"))?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|_| anyhow::anyhow!("Audio HTTP connection or response failed"))?;
        let (parts, body) = response.into_parts();
        Ok(LimitedResponse {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }
}

fn webpki_root_store() -> rustls::RootCertStore {
    let mut store = rustls::RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    store
}

pub struct LimitedResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Incoming,
}
impl LimitedResponse {
    pub fn status(&self) -> StatusCode {
        self.status
    }
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
    pub fn content_length(&self) -> Option<u64> {
        self.body.size_hint().exact()
    }
    /// Exact HTTP framing bounds the next DATA frame when this returns a length.
    pub fn remaining_body_bytes(&self) -> Option<u64> {
        self.body.size_hint().exact()
    }
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        while let Some(frame) = self.body.frame().await {
            let frame = frame.map_err(|_| anyhow::anyhow!("Audio HTTP DATA framing failed"))?;
            if let Ok(data) = frame.into_data() {
                if data.len() > HTTP_DATA_FRAME_LIMIT {
                    bail!("Audio HTTP DATA frame exceeded its fixed limit");
                }
                if data.is_empty() {
                    continue;
                }
                return Ok(Some(data));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn read_headers(stream: &mut impl Read) -> String {
        let mut bytes = Vec::new();
        let mut byte = [0];
        while !bytes.ends_with(b"\r\n\r\n") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            bytes.push(byte[0]);
            assert!(bytes.len() <= 16384);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn http_server(response: Vec<u8>) -> (Url, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!(
            "http://{}/original?private=fixture",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let request = read_headers(&mut socket);
            let _ = socket.write_all(&response);
            request
        });
        (url, server)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn content_length_data_frames_are_bounded_and_exact_remaining_length_decreases() {
        let body = vec![0x5a; 256 * 1024];
        let mut reply = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-262143/262144\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        reply.extend_from_slice(&body);
        let (url, server) = http_server(reply);
        runtime().block_on(async {
            let client = LimitedHttpClient::new(Duration::from_secs(2)).unwrap();
            let mut response = client
                .get(&url, Some("bytes=0-262143"), Some("\"fixture-v1\""))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.content_length(), Some(body.len() as u64));
            let mut received = Vec::new();
            while let Some(bytes) = response.chunk().await.unwrap() {
                assert!(bytes.len() <= HTTP_DATA_FRAME_LIMIT);
                received.extend_from_slice(&bytes);
                assert_eq!(
                    response.remaining_body_bytes(),
                    Some((body.len() - received.len()) as u64)
                );
            }
            assert_eq!(response.remaining_body_bytes(), Some(0));
            assert_eq!(received, body);
        });
        let request = server.join().unwrap().to_lowercase();
        assert!(request.contains("range: bytes=0-262143\r\n"));
        assert!(request.contains("if-range: \"fixture-v1\"\r\n"));
        assert!(request.contains("accept-encoding: identity\r\n"));
        assert!(!request.contains("cookie:"));
    }

    #[test]
    fn one_large_chunked_wire_chunk_is_delivered_as_bounded_data_frames() {
        let body = vec![0x37; 192 * 1024];
        let mut reply = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            body.len()
        )
        .into_bytes();
        reply.extend_from_slice(&body);
        reply.extend_from_slice(b"\r\n0\r\nX-Fixture: end\r\n\r\n");
        let (url, server) = http_server(reply);
        runtime().block_on(async {
            let client = LimitedHttpClient::new(Duration::from_secs(2)).unwrap();
            let mut response = client.get(&url, None, None).await.unwrap();
            assert_eq!(response.remaining_body_bytes(), None);
            let mut received = Vec::new();
            while let Some(bytes) = response.chunk().await.unwrap() {
                assert!(bytes.len() <= HTTP_DATA_FRAME_LIMIT);
                received.extend_from_slice(&bytes);
            }
            assert_eq!(received, body);
        });
        server.join().unwrap();
    }

    #[test]
    fn oversized_response_headers_fail_without_disclosing_the_approved_url() {
        let reply = format!(
            "HTTP/1.1 200 OK\r\nX-Fixture: {}\r\nContent-Length: 0\r\n\r\n",
            "a".repeat(HTTP_DATA_FRAME_LIMIT + 1024)
        )
        .into_bytes();
        let (url, server) = http_server(reply);
        runtime().block_on(async {
            let client = LimitedHttpClient::new(Duration::from_secs(2)).unwrap();
            let error = match client.get(&url, None, None).await {
                Ok(_) => panic!("HTTP response headers must obey the fixed byte limit"),
                Err(error) => error,
            };
            assert!(!format!("{error:#}").contains("private=fixture"));
        });
        server.join().unwrap();
    }

    #[test]
    fn redirects_are_returned_to_the_transport_without_following_location() {
        let (url, server) = http_server(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/unapproved\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec());
        runtime().block_on(async {
            let client = LimitedHttpClient::new(Duration::from_secs(2)).unwrap();
            let mut response = client.get(&url, None, None).await.unwrap();
            assert_eq!(response.status(), StatusCode::FOUND);
            assert!(response.chunk().await.unwrap().is_none());
        });
        server.join().unwrap();
    }

    fn tls_server(
        success: bool,
    ) -> (
        Url,
        rustls::pki_types::CertificateDer<'static>,
        thread::JoinHandle<()>,
    ) {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = cert.der().clone();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(signing_key.serialize_der().into()),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!(
            "https://localhost:{}/trusted",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            if success {
                let _request = read_headers(&mut stream);
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nowned-tls").unwrap();
                stream.flush().unwrap();
            } else {
                let mut byte = [0];
                assert!(stream.read(&mut byte).is_err());
            }
        });
        (url, certificate, server)
    }

    #[test]
    fn default_tls_rejects_a_project_owned_untrusted_certificate_before_http() {
        let (url, _, server) = tls_server(false);
        runtime().block_on(async {
            let client = LimitedHttpClient::new(Duration::from_secs(2)).unwrap();
            assert!(client.get(&url, None, None).await.is_err());
        });
        server.join().unwrap();
    }

    #[test]
    fn trusted_fixture_tls_delivers_the_original_http_body() {
        let (url, certificate, server) = tls_server(true);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        runtime().block_on(async {
            let client =
                LimitedHttpClient::with_tls_config(Duration::from_secs(2), config).unwrap();
            let mut response = client.get(&url, None, None).await.unwrap();
            let mut received = Vec::new();
            while let Some(data) = response.chunk().await.unwrap() {
                received.extend_from_slice(&data);
            }
            assert_eq!(received, b"owned-tls");
        });
        server.join().unwrap();
    }
}
