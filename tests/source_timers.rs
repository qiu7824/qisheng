use lx_gpui_player::{
    domain::lyric::LyricField,
    source::{
        MAX_PENDING_TIMERS, MAX_TIMER_CALLBACKS_PER_OPERATION, MAX_TIMER_DELAY_MS,
        MAX_TIMERS_PER_OPERATION, MIN_TIMER_DELAY_MS, SourceHostEvent, SourceRequest, SourceResult,
        SourceSession,
    },
    worker::SourceWorker,
};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const INIT: &str = "lx.send(lx.EVENT_NAMES.inited,{sources:{kw:{type:'music',actions:['musicUrl'],qualitys:['128k']},local:{type:'music',actions:['lyric','pic','musicUrl'],qualitys:[]}}});";
fn request(recover: bool) -> SourceRequest {
    SourceRequest {
        source: "kw".into(),
        action: "musicUrl".into(),
        info: json!({"type":"128k","musicInfo":{"recover":recover}}),
    }
}
fn script(handler: &str) -> String {
    format!("lx.on(lx.EVENT_NAMES.request,{handler});{INIT}")
}

#[test]
fn real_worker_timer_initialization_and_typed_lyric_promises_complete() {
    let fixture = r#"
        lx.on(lx.EVENT_NAMES.request,async request=>{
            await new Promise(resolve=>setTimeout(resolve,5));
            if(request.action==='lyric') return {lyric:'[00:01.000]Timer lyric',tlyric:null};
            return 'http://127.0.0.1/timer.wav';
        });
        setTimeout((sources)=>lx.send(lx.EVENT_NAMES.inited,{sources}),5,{local:{type:'music',actions:['lyric','pic','musicUrl'],qualitys:[]}});
    "#.to_string();
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert!(worker.is_initialized());
    let result = worker
        .request_result(&SourceRequest {
            source: "local".into(),
            action: "lyric".into(),
            info: json!({"musicInfo":{}}),
        })
        .unwrap();
    let SourceResult::Lyrics(lyrics) = result else {
        panic!()
    };
    assert_eq!(
        lyrics.lyric,
        LyricField::Text("[00:01.000]Timer lyric".into())
    );
    assert_eq!(lyrics.tlyric, LyricField::Null);
    assert!(!worker.is_stopped());
}

#[test]
fn timeout_interval_arguments_receiver_and_cross_clear_match_browser_contract() {
    let fixture = script(
        r#"()=>new Promise((resolve,reject)=>{
        const cancelled=setTimeout(()=>reject(Error('cancelled callback ran')),1);
        clearInterval(String(cancelled));
        let count=0;
        const id=setInterval(function(label,number){
            if(this!==globalThis) return reject(Error('wrong timer receiver'));
            if(++count===3){clearTimeout(id);resolve('http://127.0.0.1/'+label+'-'+number+'-'+count+'.wav');}
        },1,'argument',7);
    })"#,
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/argument-7-3.wav"
    );
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/argument-7-3.wav"
    );
    assert_eq!(
        (
            MIN_TIMER_DELAY_MS,
            MAX_TIMER_DELAY_MS,
            MAX_PENDING_TIMERS,
            MAX_TIMERS_PER_OPERATION,
            MAX_TIMER_CALLBACKS_PER_OPERATION
        ),
        (1, 60000, 64, 256, 512)
    );
}

#[test]
fn promise_microtasks_run_before_timers_and_between_timer_callbacks() {
    let fixture = script(
        "()=>new Promise(resolve=>{const order=[];Promise.resolve().then(()=>order.push('microtask'));setTimeout(()=>{order.push('first');Promise.resolve().then(()=>order.push('after-first'));},1);setTimeout(()=>resolve('http://127.0.0.1/'+order.join('-')+'.wav'),1);})",
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/microtask-first-after-first.wav"
    );
}

#[test]
fn operation_end_clears_late_timers_while_closure_state_survives() {
    let fixture = format!(
        r#"let count=0,late=0;
        lx.on(lx.EVENT_NAMES.request,async()=>{{
            ++count;
            if(count===1) setTimeout(()=>{{late=100;}},10);
            else await new Promise(resolve=>setTimeout(resolve,30));
            return 'http://127.0.0.1/'+count+'-'+late+'.wav';
        }});{INIT}
    "#
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/1-0.wav"
    );
    thread::sleep(Duration::from_millis(20));
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/2-0.wav"
    );
}

#[test]
fn source_map_prototype_changes_do_not_hijack_host_timer_storage_or_cleanup() {
    let fixture = format!(
        "let hijacked=0;Map.prototype.clear=()=>{{++hijacked;}};Map.prototype.set=()=>{{++hijacked;}};Map.prototype.get=()=>{{++hijacked;}};Map.prototype[Symbol.iterator]=()=>{{++hijacked;throw Error('hijacked iterator');}};{}",
        script(
            "async()=>{await new Promise(resolve=>setTimeout(resolve,1));return 'http://127.0.0.1/'+hijacked+'.wav';}"
        )
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/0.wav"
    );
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/0.wav"
    );
}

struct DelayedHttp {
    url: String,
    started: Arc<AtomicUsize>,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl DelayedHttp {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/fixture", listener.local_addr().unwrap());
        let started = Arc::new(AtomicUsize::new(0));
        let requests = started.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = [0u8; 4096];
                        let _ = stream.read(&mut request);
                        requests.fetch_add(1, Ordering::Release);
                        thread::sleep(Duration::from_millis(150));
                        let _=stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlate");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        Self {
            url,
            started,
            stopping,
            thread: Some(thread),
        }
    }
}
impl Drop for DelayedHttp {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn fire_and_forget_http_is_cancelled_at_task_end_and_cannot_callback_in_next_task() {
    let server = DelayedHttp::new();
    let fixture = format!(
        r#"let count=0,late=0;
        lx.on(lx.EVENT_NAMES.request,async()=>{{
            if(++count===1){{lx.request('{}',{{}},()=>{{late=100;}});await new Promise(resolve=>setTimeout(resolve,30));}}
            else await new Promise(resolve=>setTimeout(resolve,200));
            return 'http://127.0.0.1/'+count+'-'+late+'.wav';
        }});{INIT}"#,
        server.url
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/1-0.wav"
    );
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/2-0.wav"
    );
    assert_eq!(server.started.load(Ordering::Acquire), 1);
}

#[test]
fn worker_cancellation_and_source_switch_remove_timer_callbacks_and_late_host_events() {
    let fixture = script(
        "()=>new Promise(resolve=>setTimeout(async()=>{await lx.send(lx.EVENT_NAMES.updateAlert,{log:'late'});resolve('http://127.0.0.1/late.wav');},500))",
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    let cancel = worker.cancel_handle();
    let started = Instant::now();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(40));
        cancel.terminate();
    });
    assert!(worker.request(&request(false)).is_err());
    trigger.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(worker.is_stopped());
    assert!(worker.take_host_events().is_empty());
    let mut replacement =
        SourceWorker::start(&script("async()=> 'http://127.0.0.1/replacement.wav'")).unwrap();
    assert_eq!(
        replacement.request(&request(false)).unwrap(),
        "http://127.0.0.1/replacement.wav"
    );
    assert!(replacement.take_host_events().is_empty());
}

#[test]
fn interval_and_creation_budgets_fail_promptly_and_reset_on_the_next_operation() {
    let fixture = script(
        "request=>request.info.musicInfo.recover?Promise.resolve('http://127.0.0.1/recovered.wav'):new Promise(()=>setInterval(()=>{},1))",
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    let started = Instant::now();
    let error = worker.request(&request(false)).unwrap_err();
    assert!(
        error.to_string().contains("timer callback budget"),
        "{error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        worker.request(&request(true)).unwrap(),
        "http://127.0.0.1/recovered.wav"
    );
    let fixture = script(
        "async request=>{if(!request.info.musicInfo.recover)for(let i=0;i<257;i++){const id=setTimeout(()=>{},1);clearTimeout(id);}return 'http://127.0.0.1/recovered.wav';}",
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    let error = worker.request(&request(false)).unwrap_err();
    assert!(
        error.to_string().contains("timer creation budget"),
        "{error:#}"
    );
    assert_eq!(
        worker.request(&request(true)).unwrap(),
        "http://127.0.0.1/recovered.wav"
    );
}

#[test]
fn pending_timer_limit_and_callback_errors_clear_remaining_work() {
    let fixture = script(
        "async request=>{if(!request.info.musicInfo.recover)for(let i=0;i<65;i++)setTimeout(()=>{},100);return 'http://127.0.0.1/recovered.wav';}",
    );
    let mut session = SourceSession::new(&fixture).unwrap();
    let error = session.request(&request(false)).unwrap_err();
    assert!(
        error.to_string().contains("pending timer limit"),
        "{error:#}"
    );
    assert_eq!(
        session.request(&request(true)).unwrap(),
        "http://127.0.0.1/recovered.wav"
    );
    let fixture = script(
        "request=>request.info.musicInfo.recover?Promise.resolve('http://127.0.0.1/recovered.wav'):new Promise(()=>setTimeout(()=>{throw Error('timer callback failure');},1))",
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert!(
        worker
            .request(&request(false))
            .unwrap_err()
            .to_string()
            .contains("timer callback failure")
    );
    assert_eq!(
        worker.request(&request(true)).unwrap(),
        "http://127.0.0.1/recovered.wav"
    );
    let fixture =
        script("async()=>{setTimeout('process.exit()',1);return 'http://127.0.0.1/a.wav';}");
    assert!(
        SourceSession::new(&fixture)
            .unwrap()
            .request(&request(false))
            .unwrap_err()
            .to_string()
            .contains("Timer callback must be a function")
    );
}

#[test]
fn timer_callbacks_remain_subject_to_engine_cpu_and_promise_job_budgets() {
    let fixture = script("()=>new Promise(()=>setTimeout(()=>{while(true){}},1))");
    let mut worker = SourceWorker::start(&fixture).unwrap();
    let started = Instant::now();
    assert!(worker.request(&request(false)).is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
    let fixture = script(
        "()=>new Promise(()=>setTimeout(()=>{const loop=()=>Promise.resolve().then(loop);loop();},1))",
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    let error = worker.request(&request(false)).unwrap_err();
    assert!(
        error.to_string().contains("Promise job budget"),
        "{error:#}"
    );
}

#[test]
fn official_update_alert_is_once_per_source_and_only_typed_advisory_metadata() {
    let fixture = r#"
        lx.send(lx.EVENT_NAMES.updateAlert,{log:'x'.repeat(2048),updateUrl:'https://updates.invalid/source.js'});
        lx.on(lx.EVENT_NAMES.request,async()=>{
            try{await lx.send(lx.EVENT_NAMES.updateAlert,{log:'again'});throw Error('duplicate alert accepted');}
            catch(error){if(!error.message.includes('only be called once'))throw error;}
            return 'http://127.0.0.1/a.wav';
        });
        lx.send(lx.EVENT_NAMES.inited,{openDevTools:true,sources:{kw:{type:'music',actions:['musicUrl'],qualitys:['128k']}}});
    "#.to_string();
    let mut worker = SourceWorker::start(&fixture).unwrap();
    let events = worker.take_host_events();
    assert_eq!(events.len(), 2);
    let SourceHostEvent::UpdateAlert { log, update_url } = &events[0] else {
        panic!()
    };
    assert_eq!(log.len(), 1027);
    assert!(log.ends_with("..."));
    assert_eq!(
        update_url.as_deref(),
        Some("https://updates.invalid/source.js")
    );
    assert_eq!(events[1], SourceHostEvent::OpenDevToolsRequested);
    assert!(worker.take_host_events().is_empty());
    worker.request(&request(false)).unwrap();
    assert!(worker.take_host_events().is_empty());
}

#[test]
fn update_url_validation_unicode_truncation_and_event_promise_errors_are_bounded() {
    let fixture = format!(
        "lx.send(lx.EVENT_NAMES.updateAlert,{{log:'😀'.repeat(1024),updateUrl:'https://user:secret@updates.invalid/a'}});{}",
        script("async()=> 'http://127.0.0.1/a.wav'")
    );
    let mut session = SourceSession::new(&fixture).unwrap();
    let events = session.take_host_events();
    let SourceHostEvent::UpdateAlert { log, update_url } = &events[0] else {
        panic!()
    };
    assert_eq!(log.encode_utf16().count(), 1027);
    assert_eq!(*update_url, None);
    let fixture = format!(
        "lx.send(lx.EVENT_NAMES.updateAlert,{{}}).catch(error=>{{if(!error.message.includes('log is required'))throw error;}});{}",
        script(
            "async()=>{try{await lx.send(lx.EVENT_NAMES.updateAlert,{log:'valid'});}catch(error){if(!error.message.includes('only be called once'))throw error;}return 'http://127.0.0.1/a.wav';}"
        )
    );
    let mut session = SourceSession::new(&fixture).unwrap();
    session.request(&request(false)).unwrap();
    assert!(session.take_host_events().is_empty());
}

#[test]
fn timer_callback_has_no_process_environment_filesystem_or_module_capability() {
    let fixture = script(
        "()=>new Promise((resolve,reject)=>setTimeout(()=>{try{if(typeof process!=='undefined'||typeof require!=='undefined'||typeof Deno!=='undefined'||typeof __lx_timer_schedule!=='undefined'||typeof __lx_host_event!=='undefined')throw Error('private capability exposed');resolve('http://127.0.0.1/sandbox.wav');}catch(error){reject(error);}},1))",
    );
    let mut worker = SourceWorker::start(&fixture).unwrap();
    assert_eq!(
        worker.request(&request(false)).unwrap(),
        "http://127.0.0.1/sandbox.wav"
    );
}
