use lx_gpui_player::{
    backend::{BackendCommand, BackendConfig, BackendEvent, BackendHandle, CommandEnvelope},
    source::SourceHostEvent,
};
use std::{
    thread,
    time::{Duration, Instant},
};

fn execute(backend: &BackendHandle, id: &str, command: BackendCommand) {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "fixture".into(),
            expected_revision: None,
            command,
        })
        .unwrap();
}
#[test]
fn actual_worker_host_metadata_reaches_backend_events_without_opening_anything() {
    let directory = tempfile::tempdir().unwrap();
    let backend = BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
    let cursor = backend.event_cursor();
    let script = r#"/* @name Original host event fixture */
lx.on(lx.EVENT_NAMES.request,async({info})=>info.musicInfo.filePath);
setTimeout(async()=>{
 await lx.send(lx.EVENT_NAMES.updateAlert,{log:'Original release details',updateUrl:'https://example.invalid/owned-update'});
 await lx.send(lx.EVENT_NAMES.inited,{openDevTools:true,sources:{local:{type:'music',actions:['musicUrl'],qualitys:[]}}});
},10);"#;
    execute(
        &backend,
        "import",
        BackendCommand::ImportSourceText {
            script: script.into(),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    while backend.snapshot().sources.is_empty() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let id = backend.snapshot().sources[0].id.clone();
    execute(
        &backend,
        "approve",
        BackendCommand::ApproveSource { id: id.clone() },
    );
    let mut update = false;
    let mut tools = false;
    while !update || !tools {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::SourceHost { source_id, payload } = event.payload {
                assert_eq!(source_id, id);
                match payload {
                    SourceHostEvent::UpdateAlert { log, update_url } => {
                        assert_eq!(log, "Original release details");
                        assert_eq!(
                            update_url.as_deref(),
                            Some("https://example.invalid/owned-update")
                        );
                        update = true;
                    }
                    SourceHostEvent::OpenDevToolsRequested => tools = true,
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "Expected typed source events: {:?}",
            backend.snapshot()
        );
        thread::sleep(Duration::from_millis(10));
    }
    backend.shutdown(Duration::from_secs(5)).unwrap();
}
