//! Operation cancellation across the real source process preserves one JavaScript session.
use lx_gpui_player::{
    source::{SourceRequest, SourceRequestCancelled},
    worker::SourceWorker,
};
use serde_json::json;
use std::{
    cell::Cell,
    io::Read,
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn request(mode: &str) -> SourceRequest {
    SourceRequest {
        source: "kw".into(),
        action: "musicUrl".into(),
        info: json!({"type":"128k", "musicInfo":{"songmid":mode}}),
    }
}

fn worker(body: &str) -> SourceWorker {
    let script = format!(
        r#"/* @name Session cancellation fixture
@version 1.0.0
@author LX Native tests */
let calls = 0;
let late = 0;
lx.on('request', ({{info}}) => {{
  const number = ++calls;
  if (info.musicInfo.songmid === 'slow') {{ {body} }}
  return Promise.resolve('http://127.0.0.1/fixture.wav?calls=' + number + '&late=' + late);
}});
lx.send('inited', {{sources: {{kw: {{name:'Fixture',type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});
"#
    );
    let mut worker = SourceWorker::prepare().unwrap();
    worker.initialize(&script).unwrap();
    worker
}

#[test]
fn cancellation_before_dispatch_does_not_execute_or_reset_the_source() {
    let mut worker = worker("return new Promise(resolve => setTimeout(resolve, 60000));");
    let result = worker.request_url_cancellable(&request("slow"), || true);
    assert!(result.unwrap_err().is::<SourceRequestCancelled>());
    assert!(!worker.is_stopped());
    assert!(
        worker
            .request(&request("fast"))
            .unwrap()
            .contains("calls=1&late=0")
    );
}

#[test]
fn cancelling_a_long_timer_preserves_the_closure_and_removes_old_callbacks() {
    let mut worker = worker(
        "return new Promise(resolve => setTimeout(() => {late++; resolve('http://127.0.0.1/late.wav');}, 60000));",
    );
    let started = Instant::now();
    let result = worker.request_url_cancellable(&request("slow"), || {
        started.elapsed() > Duration::from_millis(80)
    });
    assert!(result.unwrap_err().is::<SourceRequestCancelled>());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!worker.is_stopped());
    assert!(
        worker
            .request(&request("fast"))
            .unwrap()
            .contains("calls=2&late=0")
    );
    assert!(
        worker
            .request_url_cancellable(&request("fast"), || false)
            .unwrap()
            .contains("calls=3&late=0")
    );
}

#[test]
fn cancelling_in_flight_http_closes_the_socket_without_invoking_its_callback() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/slow", listener.local_addr().unwrap());
    let (received, request_seen) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            assert_eq!(socket.read(&mut byte).unwrap(), 1);
            header.push(byte[0]);
            assert!(header.len() < 8192);
        }
        received.send(()).unwrap();
        match socket.read(&mut byte) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            other => panic!("cancelled HTTP request must close its socket: {other:?}"),
        }
    });
    let mut worker = worker(&format!(
        "return new Promise((resolve, reject) => lx.request('{url}', {{timeout:60000}}, error => {{late++; if(error)reject(error);else resolve('http://127.0.0.1/late.wav');}}));"
    ));
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = cancelled.clone();
    let task = thread::spawn(move || {
        let result =
            worker.request_cancellable(&request("slow"), || cancellation.load(Ordering::Acquire));
        (worker, result)
    });
    request_seen.recv_timeout(Duration::from_secs(4)).unwrap();
    let started = Instant::now();
    cancelled.store(true, Ordering::Release);
    let (mut worker, result) = task.join().unwrap();
    assert!(result.unwrap_err().is::<SourceRequestCancelled>());
    assert!(started.elapsed() < Duration::from_secs(3));
    server.join().unwrap();
    assert!(!worker.is_stopped());
    assert!(
        worker
            .request(&request("fast"))
            .unwrap()
            .contains("calls=2&late=0")
    );
}

#[test]
fn repeated_request_cancellation_is_scoped_and_later_requests_keep_advancing() {
    let mut worker = worker(
        "return new Promise(resolve => setTimeout(() => {late++; resolve('http://127.0.0.1/late.wav');}, 60000));",
    );
    for cycle in 0..8 {
        let started = Instant::now();
        let result = worker.request_cancellable(&request("slow"), || {
            started.elapsed() > Duration::from_millis(30)
        });
        assert!(result.unwrap_err().is::<SourceRequestCancelled>());
        assert!(!worker.is_stopped());
        let url = worker
            .request_url_cancellable(&request("fast"), || false)
            .unwrap();
        assert!(
            url.contains(&format!("calls={}&late=0", (cycle + 1) * 2)),
            "{url}"
        );
    }
}

#[test]
fn a_cancellable_request_error_does_not_terminate_the_initialized_worker() {
    let mut worker = worker("return Promise.reject(Error('fixture rejection'));");
    let error = worker
        .request_cancellable(&request("slow"), || false)
        .unwrap_err();
    assert!(error.to_string().contains("fixture rejection"));
    assert!(!worker.is_stopped());
    assert!(
        worker
            .request(&request("fast"))
            .unwrap()
            .contains("calls=2&late=0")
    );
}

#[test]
fn a_sent_cancellation_is_latched_even_if_the_callers_predicate_changes() {
    let mut worker = worker("return new Promise(resolve => setTimeout(resolve, 60000));");
    let requested = Cell::new(false);
    let started = Instant::now();
    let result = worker.request_cancellable(&request("slow"), || {
        started.elapsed() > Duration::from_millis(40) && !requested.replace(true)
    });
    assert!(result.unwrap_err().is::<SourceRequestCancelled>());
    assert!(requested.get());
    assert!(!worker.is_stopped());
    assert!(
        worker
            .request(&request("fast"))
            .unwrap()
            .contains("calls=2&late=0")
    );
}
