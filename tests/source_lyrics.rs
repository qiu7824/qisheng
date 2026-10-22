//! Original local-source fixtures, exercising Boa and the real CLI worker IPC.

use lx_gpui_player::{
    domain::lyric::{LyricBundle, LyricField},
    source::{SourceRequest, SourceResult, SourceSession, execute_request_result},
    worker::SourceWorker,
};
use serde_json::json;

fn fixture(handler: &str) -> String {
    format!(
        r#"/*
 * @name Local lyric contract fixture
 * @author LX Native contributors
 */
lx.on(lx.EVENT_NAMES.request, {handler});
lx.send(lx.EVENT_NAMES.inited, {{sources:{{local:{{type:'music',actions:['musicUrl','pic','lyric'],qualitys:[]}}}}}});
"#
    )
}

fn request(action: &str) -> SourceRequest {
    SourceRequest {
        source: "local".into(),
        action: action.into(),
        info: json!({
            "type":null, "musicInfo":{
                "name":"原创测试曲目", "singer":"测试", "source":"local", "songmid":"fixture",
                "interval":"0:03", "albumName":"测试", "img":"", "types":[], "_types":{},
                "filePath":"http://127.0.0.1:9/fixture.wav", "ext":"wav"
            }
        }),
    }
}

#[test]
fn local_lyric_object_reaches_typed_rust_without_collapsing_empty_null_and_missing() {
    let script = fixture(
        "async ({action,info}) => {if(action!=='lyric'||info.musicInfo.songmid!=='fixture')throw Error('contract');return {lyric:'[00:01.000]你好🎵',tlyric:null,rlyric:''};}",
    );
    let SourceResult::Lyrics(bundle) = execute_request_result(&script, &request("lyric")).unwrap()
    else {
        panic!("Expected lyrics")
    };
    assert_eq!(
        bundle.to_value(),
        json!({"lyric":"[00:01.000]你好🎵","tlyric":null,"rlyric":""})
    );
    assert_eq!(bundle.lxlyric, LyricField::Missing);
    assert_eq!(bundle.tlyric, LyricField::Null);
    assert_eq!(bundle.rlyric, LyricField::Text(String::new()));
    assert_eq!(
        serde_json::from_value::<SourceResult>(
            serde_json::to_value(SourceResult::Lyrics(bundle.clone())).unwrap()
        )
        .unwrap(),
        SourceResult::Lyrics(bundle)
    );
}

#[test]
fn all_four_lyric_tracks_return_through_the_actual_bounded_worker_ipc() {
    let script = fixture(
        "async ({action}) => action==='lyric'?{lyric:'[00:01.000]原文',tlyric:'[00:01.000]译文',rlyric:'[00:01.000]roman',lxlyric:'[00:01.000]<0,500>原<500,500>文'}:'http://127.0.0.1:9/fixture.wav'",
    );
    let mut worker = SourceWorker::start(&script).unwrap();
    let SourceResult::Lyrics(bundle) = worker.request_result(&request("lyric")).unwrap() else {
        panic!("Expected lyrics")
    };
    assert_eq!(bundle.lyric.text(), Some("[00:01.000]原文"));
    assert_eq!(bundle.tlyric.text(), Some("[00:01.000]译文"));
    assert_eq!(bundle.rlyric.text(), Some("[00:01.000]roman"));
    assert_eq!(
        bundle.lxlyric.text(),
        Some("[00:01.000]<0,500>原<500,500>文")
    );
    assert_eq!(
        worker.request(&request("musicUrl")).unwrap(),
        "http://127.0.0.1:9/fixture.wav"
    );
    assert_eq!(
        worker.request(&request("pic")).unwrap(),
        "http://127.0.0.1:9/fixture.wav"
    );
}

#[test]
fn malformed_lyric_objects_are_explicit_errors_instead_of_coerced_strings() {
    for response in [
        "'text'",
        "[]",
        "null",
        "{}",
        "{lyric:null}",
        "{lyric:42}",
        "{lyric:'ok',tlyric:1}",
        "{lyric:'ok',rlyric:[]}",
        "{lyric:'ok',lxlyric:{}}",
        "{lyric:'ok',future:'unknown'}",
        "{lyric:'nul\\u0000text'}",
    ] {
        let script = fixture(&format!("async()=>({response})"));
        assert!(
            execute_request_result(&script, &request("lyric")).is_err(),
            "{response}"
        );
    }
    let synchronous = fixture("()=>({lyric:'ok'})");
    assert!(
        execute_request_result(&synchronous, &request("lyric"))
            .unwrap_err()
            .to_string()
            .contains("Promise/thenable")
    );
}

#[test]
fn source_lyric_limits_use_utf16_and_strict_optional_thresholds() {
    let allowed = fixture(
        "async()=>({lyric:'🎵'.repeat(25600),tlyric:'x'.repeat(5119),rlyric:null,lxlyric:'x'.repeat(8191)})",
    );
    let SourceResult::Lyrics(bundle) = execute_request_result(&allowed, &request("lyric")).unwrap()
    else {
        panic!("Expected lyrics")
    };
    assert_eq!(bundle.lyric.text().unwrap().encode_utf16().count(), 51200);
    for response in [
        "{lyric:'x'.repeat(51201)}",
        "{lyric:'🎵'.repeat(25601)}",
        "{lyric:'ok',tlyric:'x'.repeat(5120)}",
        "{lyric:'ok',rlyric:'🎵'.repeat(2560)}",
        "{lyric:'ok',lxlyric:'x'.repeat(8192)}",
    ] {
        let script = fixture(&format!("async()=>({response})"));
        assert!(
            execute_request_result(&script, &request("lyric"))
                .unwrap_err()
                .to_string()
                .contains("UTF-16")
        );
    }
}

#[test]
fn worker_lyric_validation_failure_recovers_and_keeps_source_closure_state() {
    let script = fixture(
        "(()=>{let calls=0;return async()=> (++calls===1?{lyric:'ok',tlyric:9}:{lyric:'recovered-'+calls,tlyric:null,lxlyric:''});})()",
    );
    let mut worker = SourceWorker::start(&script).unwrap();
    assert!(worker.request_result(&request("lyric")).is_err());
    let SourceResult::Lyrics(bundle) = worker.request_result(&request("lyric")).unwrap() else {
        panic!("Expected lyrics")
    };
    assert_eq!(bundle.lyric.text(), Some("recovered-2"));
    assert_eq!(bundle.tlyric, LyricField::Null);
    assert_eq!(bundle.rlyric, LyricField::Missing);
    assert_eq!(bundle.lxlyric, LyricField::Text(String::new()));
}

#[test]
fn url_compatibility_wrapper_and_local_only_capability_boundary_remain_explicit() {
    let script =
        fixture("async ({action})=> action==='lyric'?{lyric:'ok'}:'http://127.0.0.1:9/audio.wav'");
    let mut session = SourceSession::new(&script).unwrap();
    assert_eq!(
        session.request(&request("musicUrl")).unwrap(),
        "http://127.0.0.1:9/audio.wav"
    );
    assert_eq!(
        session.request(&request("pic")).unwrap(),
        "http://127.0.0.1:9/audio.wav"
    );
    assert!(
        session
            .request(&request("lyric"))
            .unwrap_err()
            .to_string()
            .contains("typed request_result")
    );
    let mut platform_lyric = request("lyric");
    platform_lyric.source = "wy".into();
    assert!(
        session
            .request_result(&platform_lyric)
            .unwrap_err()
            .to_string()
            .contains("only for local")
    );
}

#[test]
fn source_response_serialization_does_not_execute_inherited_to_json() {
    let script = fixture(
        "async()=>{const result=Object.create({toJSON(){throw Error('must not execute');}});result.lyric='ok';result.tlyric=null;return result;}",
    );
    let result = execute_request_result(&script, &request("lyric")).unwrap();
    assert_eq!(
        result,
        SourceResult::Lyrics(
            LyricBundle::from_value(&json!({"lyric":"ok","tlyric":null})).unwrap()
        )
    );
}
