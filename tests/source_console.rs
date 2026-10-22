use lx_gpui_player::{source::SourceRequest, worker::SourceWorker};
use serde_json::json;

#[test]
fn console_calls_during_initialization_and_requests_preserve_worker_protocol() {
    let script = r#"
        const methods = ['log','info','warn','error','debug','trace','dir','dirxml',
            'table','group','groupCollapsed','groupEnd','clear','count','countReset',
            'time','timeLog','timeEnd','assert','profile','profileEnd','timeStamp'];
        const hostile = { toString() { throw Error('diagnostic argument formatted'); } };
        const cycle = {}; cycle.self = cycle;
        for (const name of methods) {
            if (typeof console[name] !== 'function') throw Error('missing console.' + name);
            if (console[name]('diagnostic', hostile, cycle) !== undefined) throw Error('wrong console result');
        }
        console.assert(false, hostile);
        console.groupEnd();
        lx.on(lx.EVENT_NAMES.request, async () => {
            console.log('request started', hostile);
            console.warn(cycle);
            console.groupEnd();
            return 'http://127.0.0.1/console.wav';
        });
        lx.send(lx.EVENT_NAMES.inited, { sources: {
            kw: {type:'music',actions:['musicUrl'],qualitys:['128k']}
        }});
    "#;
    let mut worker = SourceWorker::start(script).unwrap();
    assert!(worker.is_initialized());
    let request = SourceRequest {
        source: "kw".into(),
        action: "musicUrl".into(),
        info: json!({"type":"128k","musicInfo":{}}),
    };
    for _ in 0..2 {
        assert_eq!(
            worker.request(&request).unwrap(),
            "http://127.0.0.1/console.wav"
        );
    }
    assert!(!worker.is_stopped());
}
