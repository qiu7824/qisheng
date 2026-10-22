#[path = "backend_recovery.rs"]
#[expect(
    dead_code,
    reason = "The shared real HTTP/source fixture also exposes media recovery cases exercised by its separate integration target."
)]
mod recovery;

use lx_gpui_player::{
    backend::{
        BackendCommand, BackendError, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, DeepLinkOperationPage, ErrorKind, ReceiptState,
    },
    backup::{BackupEncoding, NativeBackup, NativeBackupOptions, encode_native},
    domain::lists::ListData,
    store::StoreData,
};
use serde_json::json;
use std::{
    fs,
    sync::{Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

static CASES: Mutex<()> = Mutex::new(());
const ORIGIN: &str = "operation-scope-fixture";

struct HeldSource<'a>(&'a recovery::Server);
impl<'a> HeldSource<'a> {
    fn new(server: &'a recovery::Server) -> Self {
        server.shared.hold_source.store(true, Ordering::Release);
        Self(server)
    }
    fn release(&self) {
        self.0.shared.hold_source.store(false, Ordering::Release);
    }
}
impl Drop for HeldSource<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug)]
struct Terminal {
    event_id: u64,
    success: bool,
    error: Option<BackendError>,
}

fn terminals(backend: &BackendHandle, cursor: u64, id: &str) -> Vec<Terminal> {
    backend
        .events_after(cursor)
        .unwrap()
        .into_iter()
        .filter_map(|event| {
            let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
            else {
                return None;
            };
            if operation_id != id {
                return None;
            }
            assert_eq!(event.operation_id.as_deref(), Some(id));
            assert_eq!(event.origin, ORIGIN);
            Some(Terminal {
                event_id: event.event_id,
                success,
                error,
            })
        })
        .collect()
}

fn wait_terminal(backend: &BackendHandle, cursor: u64, id: &str) -> Terminal {
    let deadline = Instant::now() + Duration::from_secs(18);
    loop {
        let mut finished = terminals(backend, cursor, id);
        if !finished.is_empty() {
            assert_eq!(
                finished.len(),
                1,
                "Duplicate terminal for {id}: {finished:?}"
            );
            return finished.remove(0);
        }
        assert!(
            Instant::now() < deadline,
            "Missing actual terminal for {id}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn execute(backend: &BackendHandle, id: &str, command: BackendCommand) -> ReceiptState {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: ORIGIN.into(),
            expected_revision: None,
            command,
        })
        .unwrap()
        .state
}

fn deeplink_result(backend: &BackendHandle, id: &str) -> DeepLinkOperationPage {
    let BackendQueryResult::DeepLinkResult(page) = backend
        .query(BackendQuery::DeepLinkResult {
            operation_id: id.into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("Missing deep-link result");
    };
    *page
}

fn delayed_child(running: &recovery::Running, server: &recovery::Server, cursor: u64) -> String {
    assert_eq!(
        execute(
            &running.backend,
            "scope-parent",
            BackendCommand::DispatchDeepLink {
                url: format!(
                    "lxmusic://music/searchPlay?data={}",
                    lx_gpui_player::platforms::encode(
                        &json!({
                            "name":"Original recovery tone", "singer":"Fixture composer",
                            "albumName":"Original recordings", "interval":"00:12",
                            "playLater":false
                        })
                        .to_string()
                    )
                ),
            },
        ),
        ReceiptState::Accepted
    );
    let parent = wait_terminal(&running.backend, cursor, "scope-parent");
    assert!(parent.success && parent.error.is_none(), "{parent:?}");
    let page = deeplink_result(&running.backend, "scope-parent");
    assert_eq!(serde_json::to_value(page.state).unwrap(), "completed");
    assert_eq!(page.total, 1);
    assert_eq!(page.tracks[0].platform.source, "tx");
    assert!(page.effect.temporary_list_applied && page.effect.queue_enqueued);
    assert!(page.effect.playback_requested);
    assert!(page.effect.playback_error.is_none());
    let child = page.effect.playback_operation_id.unwrap();
    assert_eq!(
        child,
        lx_gpui_player::domain::music::stable_id("deeplink-play", b"scope-parent")
    );
    assert_ne!(child, "scope-parent");

    // The real SourceWorker must have reached its HTTP resolver before Stop.
    // The server retains that actual request until the test releases its gate.
    recovery::wait_source(server, 1);
    let calls = server.source_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].platform, "tx");
    assert_eq!(calls[0].dto["songmid"], "actual-mid");
    assert!(server.shared.hold_source.load(Ordering::Acquire));
    assert!(running.backend.snapshot().busy);
    assert!(terminals(&running.backend, cursor, &child).is_empty());
    assert!(server.media_calls().is_empty());
    child
}

fn stop_and_verify(
    running: &recovery::Running,
    server: &recovery::Server,
    gate: &HeldSource<'_>,
    cursor: u64,
    child: &str,
    preview: Option<&str>,
) {
    execute(&running.backend, "scope-stop", BackendCommand::Stop);
    let cancelled = wait_terminal(&running.backend, cursor, child);
    assert!(!cancelled.success);
    assert_eq!(cancelled.error.as_ref().unwrap().kind, ErrorKind::Cancelled);
    let parent = terminals(&running.backend, cursor, "scope-parent");
    assert_eq!(parent.len(), 1);
    assert!(parent[0].success && parent[0].error.is_none());
    assert!(parent[0].event_id < cancelled.event_id);

    // Repeated cancellation cannot manufacture a second terminal.
    execute(
        &running.backend,
        "scope-cancel-again",
        BackendCommand::CancelOperation {
            operation_id: child.into(),
        },
    );
    execute(&running.backend, "scope-stop-again", BackendCommand::Stop);
    gate.release();
    running.backend.shutdown(Duration::from_secs(18)).unwrap();

    // Actual shutdown joins the source request before the final journal check.
    let child_events = terminals(&running.backend, cursor, child);
    assert_eq!(child_events.len(), 1);
    assert_eq!(child_events[0].event_id, cancelled.event_id);
    assert_eq!(terminals(&running.backend, cursor, "scope-parent").len(), 1);
    if let Some(preview) = preview {
        let completed = terminals(&running.backend, cursor, preview);
        assert_eq!(completed.len(), 1);
        assert!(completed[0].success && completed[0].error.is_none());
        assert!(completed[0].event_id < cancelled.event_id);
    }
    assert_eq!(server.source_calls().len(), 1);
    assert!(server.media_calls().is_empty());
    let state = running.backend.snapshot();
    assert!(state.pending_playback_url.is_none());
    assert!(state.approved_audio_identity.is_none());
}

#[test]
fn delayed_deeplink_source_child_cancels_once_without_reterminating_its_parent() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = recovery::Server::new();
    let running = recovery::Running::new(&server, &["128k"]);
    let gate = HeldSource::new(&server);
    let cursor = running.backend.event_cursor();
    let child = delayed_child(&running, &server, cursor);
    stop_and_verify(&running, &server, &gate, cursor, &child, None);
}

#[test]
fn completed_actual_native_preview_preserves_the_still_blocked_source_child_owner() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = recovery::Server::new();
    let running = recovery::Running::new(&server, &["128k"]);
    let gate = HeldSource::new(&server);
    let cursor = running.backend.event_cursor();
    let child = delayed_child(&running, &server, cursor);
    let path = running
        ._directory
        .path()
        .join("selected-native-backup.json");
    let backup = NativeBackup::from_snapshot(
        &StoreData::default(),
        &ListData::default(),
        "",
        &[],
        &[],
        NativeBackupOptions::default(),
    )
    .unwrap();
    fs::write(&path, encode_native(&backup, BackupEncoding::Json).unwrap()).unwrap();
    assert_eq!(
        execute(
            &running.backend,
            "scope-independent-preview",
            BackendCommand::PreviewNativeBackup {
                path,
                replace_tracks: false,
            },
        ),
        ReceiptState::Accepted
    );
    let preview = wait_terminal(&running.backend, cursor, "scope-independent-preview");
    assert!(preview.success && preview.error.is_none(), "{preview:?}");
    let BackendQueryResult::NativeBackupPreview(Some(view)) = running
        .backend
        .query(BackendQuery::NativeBackupPreview)
        .unwrap()
    else {
        panic!("Missing actual native preview");
    };
    assert_eq!(view.operation_id, "scope-independent-preview");
    assert!(server.shared.hold_source.load(Ordering::Acquire));
    assert!(running.backend.snapshot().busy);
    assert!(terminals(&running.backend, cursor, &child).is_empty());
    stop_and_verify(
        &running,
        &server,
        &gate,
        cursor,
        &child,
        Some("scope-independent-preview"),
    );
}
