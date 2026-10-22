use lx_gpui_player::platforms::*;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

struct ForbiddenTransport;
impl HttpTransport for ForbiddenTransport {
    fn execute(&self, _: HttpRequest, _: &ProviderContext) -> Result<HttpResponse, ProviderError> {
        panic!("transport must not execute")
    }
}

fn fixture(replies: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/fixture", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let log = received.clone();
    let handle = thread::spawn(move || {
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let body_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + body_length {
                        break;
                    }
                }
            }
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request).into_owned());
            stream.write_all(&reply).unwrap();
        }
    });
    (url, received, handle)
}
fn response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
fn transport(retries: u8, bound: usize) -> ReqwestTransport {
    ReqwestTransport::new(TransportConfig {
        proxy: None,
        max_response_bytes: bound,
        retries,
    })
    .unwrap()
}

#[test]
fn registry_has_exactly_five_platforms_and_cancel_blocks_static_results() {
    let registry = ProviderRegistry::new(Arc::new(ForbiddenTransport));
    assert_eq!(
        registry.platforms(),
        vec![
            Platform::Kw,
            Platform::Tx,
            Platform::Kg,
            Platform::Wy,
            Platform::Mg
        ]
    );
    for platform in registry.platforms() {
        assert_eq!(registry.provider(platform).platform(), platform);
    }
    let context = ProviderContext::default();
    context.cancel.store(true, Ordering::Release);
    assert_eq!(
        registry.execute(Platform::Kw, ProviderRequest::Charts, &context),
        Err(ProviderError::Cancelled)
    );
    assert!(serde_json::from_str::<Platform>("\"unknown\"").is_err());
}

#[test]
fn requests_and_typed_results_round_trip() {
    let request = ProviderRequest::Comments {
        id: "7".into(),
        kind: CommentKind::Replies,
        page: 2,
        limit: 20,
        cursor: Some("next".into()),
        root_comment_id: Some("root".into()),
    };
    assert_eq!(
        serde_json::from_value::<ProviderRequest>(serde_json::to_value(&request).unwrap()).unwrap(),
        request
    );
    let response = ProviderResponse::Comments(Page::new(Vec::new(), 0, 1, 20));
    assert_eq!(
        serde_json::from_value::<ProviderResponse>(serde_json::to_value(&response).unwrap())
            .unwrap(),
        response
    );
    assert_eq!(encode("中文 +&/"), "%E4%B8%AD%E6%96%87%20%2B%26%2F");
    assert!(page_args(0, 20).is_err());
    assert!(page_args(1, 1001).is_err());
}

#[test]
fn real_http_fixture_preserves_method_headers_body_and_response() {
    let (url, received, handle) = fixture(vec![response(200, "{\"ok\":true}")]);
    let response = transport(0, 1024)
        .execute(
            HttpRequest::post(url, b"payload".to_vec())
                .header("Content-Type", "text/plain")
                .header("X-Fixture", "yes"),
            &ProviderContext::default(),
        )
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.json().unwrap()["ok"], true);
    handle.join().unwrap();
    let log = received.lock().unwrap();
    assert!(log[0].starts_with("POST /fixture "));
    assert!(log[0].to_lowercase().contains("x-fixture: yes"));
    assert!(log[0].contains("payload"));
}

#[test]
fn real_transport_has_finite_idempotent_retries() {
    let (url, received, handle) = fixture(vec![
        response(503, "retry"),
        response(503, "retry"),
        response(200, "done"),
    ]);
    let result = transport(2, 1024)
        .execute(HttpRequest::get(url), &ProviderContext::default())
        .unwrap();
    assert_eq!(result.bytes, b"done");
    handle.join().unwrap();
    assert_eq!(received.lock().unwrap().len(), 3);
    let (url, received, handle) = fixture(vec![response(503, "no retry")]);
    assert_eq!(
        transport(2, 1024)
            .execute(
                HttpRequest::post(url, b"x".to_vec()),
                &ProviderContext::default()
            )
            .unwrap()
            .status,
        503
    );
    handle.join().unwrap();
    assert_eq!(received.lock().unwrap().len(), 1);
}

#[test]
fn response_length_and_streaming_bounds_are_enforced() {
    let (url, _, handle) = fixture(vec![response(200, "0123456789")]);
    assert_eq!(
        transport(0, 8).execute(HttpRequest::get(url), &ProviderContext::default()),
        Err(ProviderError::ResponseTooLarge { limit: 8 })
    );
    handle.join().unwrap();
    let (url, _, handle) = fixture(vec![
        b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n0123456789".to_vec(),
    ]);
    assert_eq!(
        transport(0, 8).execute(HttpRequest::get(url), &ProviderContext::default()),
        Err(ProviderError::ResponseTooLarge { limit: 8 })
    );
    handle.join().unwrap();
}

fn slow_fixture() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/slow", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer);
        thread::sleep(Duration::from_millis(300));
        let _ = stream.write_all(&response(200, "late"));
    });
    (url, handle)
}

#[test]
fn in_flight_network_is_cancelled_promptly_and_honors_deadline() {
    let transport = transport(0, 1024);
    let (url, handle) = slow_fixture();
    let context = ProviderContext::new(Duration::from_secs(5));
    let cancel = context.cancel.clone();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        cancel.store(true, Ordering::Release);
    });
    let started = Instant::now();
    assert_eq!(
        transport.execute(HttpRequest::get(url), &context),
        Err(ProviderError::Cancelled)
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    trigger.join().unwrap();
    handle.join().unwrap();
    let (url, handle) = slow_fixture();
    let started = Instant::now();
    assert_eq!(
        transport.execute(
            HttpRequest::get(url),
            &ProviderContext::new(Duration::from_millis(50))
        ),
        Err(ProviderError::DeadlineExceeded)
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    handle.join().unwrap();
}

#[test]
fn explicit_proxy_receives_absolute_url_and_unsupported_url_is_rejected() {
    let (proxy, received, handle) = fixture(vec![response(200, "proxy")]);
    let transport = ReqwestTransport::new(TransportConfig {
        proxy: Some(proxy.trim_end_matches("/fixture").into()),
        max_response_bytes: 1024,
        retries: 0,
    })
    .unwrap();
    assert_eq!(
        transport
            .execute(
                HttpRequest::get("http://fixture.invalid/path?q=1"),
                &ProviderContext::default()
            )
            .unwrap()
            .bytes,
        b"proxy"
    );
    handle.join().unwrap();
    assert!(received.lock().unwrap()[0].starts_with("GET http://fixture.invalid/path?q=1 "));
    assert!(matches!(
        transport.execute(
            HttpRequest::get("file:///private"),
            &ProviderContext::default()
        ),
        Err(ProviderError::InvalidRequest { .. })
    ));
    assert!(matches!(
        transport.execute(
            HttpRequest::get("http://user:password@localhost/private"),
            &ProviderContext::default()
        ),
        Err(ProviderError::InvalidRequest { .. })
    ));
}

#[test]
fn untrusted_local_tls_certificate_is_rejected() {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certified.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "https://localhost:{}/secure",
        listener.local_addr().unwrap().port()
    );
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut tls = rustls::StreamOwned::new(connection, stream);
        let mut request = [0u8; 1024];
        let _ = tls.read(&mut request);
    });
    assert!(matches!(
        transport(0, 1024).execute(HttpRequest::get(url), &ProviderContext::default()),
        Err(ProviderError::Network { .. })
    ));
    handle.join().unwrap();
}

#[test]
fn invalid_transport_configuration_and_completed_deadline_fail_before_io() {
    assert!(
        ReqwestTransport::new(TransportConfig {
            proxy: None,
            max_response_bytes: 0,
            retries: 0
        })
        .is_err()
    );
    assert!(
        ReqwestTransport::new(TransportConfig {
            proxy: None,
            max_response_bytes: 1024,
            retries: 3
        })
        .is_err()
    );
    let expired = ProviderContext {
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        deadline: Instant::now(),
    };
    assert_eq!(
        transport(0, 1024).execute(
            HttpRequest {
                method: "GET".into(),
                url: "http://127.0.0.1:9/".into(),
                follow_redirects: true,
                headers: BTreeMap::new(),
                body: Vec::new()
            },
            &expired
        ),
        Err(ProviderError::DeadlineExceeded)
    );
}
