#![cfg(windows)]

use lx_gpui_player::{
    backend::{
        BackendConfig, BackendEvent, BackendHandle, BackendQuery, BackendQueryResult,
        RetainedRepositoryOpenFailure, forward_deeplink_to_existing_profile,
    },
    platforms::{
        HttpRequest, HttpResponse, HttpTransport, ProviderContext, ProviderError, ProviderRegistry,
    },
    repository::{ProfileGuard, RepositoryOpenOwner},
    system_control::instance_relay::RelayDisposition,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

static CASES: Mutex<()> = Mutex::new(());

fn busy_guard(directory: &std::path::Path) -> Box<ProfileGuard> {
    let failure = ProfileGuard::acquire(directory).unwrap_err();
    assert!(failure.error.error.chain().any(|cause| matches!(
        cause.downcast_ref::<std::fs::TryLockError>(),
        Some(std::fs::TryLockError::WouldBlock)
    )));
    let owner = failure.owner.unwrap();
    assert!(!owner.is_locked());
    owner
}

#[test]
fn actual_profile_relay_preserves_actor_identity_rejection_and_joined_restart() {
    let _case = CASES.lock().unwrap();
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    for epoch in 0..2 {
        let backend = BackendHandle::start_with_instance_relay(BackendConfig::headless(
            directory.path().into(),
        ))
        .unwrap();
        let guard = busy_guard(directory.path());
        let id = format!("relay-real-pause-{epoch}");
        let before_revision = backend.snapshot().revision;
        let ack = forward_deeplink_to_existing_profile(
            &guard,
            id.clone(),
            "lxmusic://player/pause".into(),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(ack.request_id, id);
        assert_eq!(ack.operation_id.as_deref(), Some(id.as_str()));
        assert_eq!(ack.disposition, RelayDisposition::Committed);
        let committed_revision = backend.snapshot().revision;
        assert!(committed_revision > before_revision);
        let replay = forward_deeplink_to_existing_profile(
            &guard,
            id.clone(),
            "lxmusic://player/pause".into(),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(replay, ack);
        assert_eq!(backend.snapshot().revision, committed_revision);
        // This passes transport validation but fails the real actor's receipt
        // fingerprint. The client must receive a negative ACK rather than Io.
        let rejected = forward_deeplink_to_existing_profile(
            &guard,
            id.clone(),
            "lxmusic://player/play".into(),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(rejected.disposition, RelayDisposition::Rejected);
        assert_eq!(rejected.request_id, id);
        assert!(rejected.operation_id.is_none());
        drop(guard);
        backend.shutdown(Duration::from_secs(8)).unwrap();
        drop(backend);
        let acquired = ProfileGuard::acquire(directory.path()).unwrap();
        acquired.verify().unwrap();
        drop(acquired);
    }
}

struct GatedMetadata {
    opened: Mutex<Option<mpsc::SyncSender<()>>>,
    released: Mutex<bool>,
    wake: Condvar,
}
impl HttpTransport for GatedMetadata {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        assert!(
            reqwest::Url::parse(&request.url)
                .unwrap()
                .host_str()
                .unwrap()
                .contains("kuwo")
        );
        if let Some(sender) = self.opened.lock().unwrap().take() {
            sender.send(()).unwrap();
        }
        let mut released = self.released.lock().unwrap();
        while !*released {
            context.check()?;
            released = self
                .wake
                .wait_timeout(released, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        context.check()?;
        Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            bytes: serde_json::to_vec(&json!({"TOTAL":"0","SHOW":"0","abslist":[]})).unwrap(),
        })
    }
}

#[test]
fn actual_async_actor_receipt_is_accepted_then_completed_when_same_identifier_is_replayed() {
    let _case = CASES.lock().unwrap();
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let (entered, started) = mpsc::sync_channel(1);
    let fixture = Arc::new(GatedMetadata {
        opened: Mutex::new(Some(entered)),
        released: Mutex::new(false),
        wake: Condvar::new(),
    });
    let mut config = BackendConfig::headless(directory.path().into());
    config.provider_registry = Some(Arc::new(ProviderRegistry::new(fixture.clone())));
    let backend = BackendHandle::start_with_instance_relay(config).unwrap();
    let guard = busy_guard(directory.path());
    let cursor = backend.event_cursor();
    let id = "relay-actual-async";
    let link = "lxmusic://music/search/kw/isolated";
    let accepted = forward_deeplink_to_existing_profile(
        &guard,
        id.into(),
        link.into(),
        Duration::from_secs(3),
    )
    .unwrap();
    assert_eq!(accepted.disposition, RelayDisposition::Accepted);
    assert_eq!(accepted.operation_id.as_deref(), Some(id));
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    let page = backend
        .query(BackendQuery::DeepLinkResult {
            operation_id: id.into(),
            offset: 0,
            limit: 10,
        })
        .unwrap();
    let BackendQueryResult::DeepLinkResult(page) = page else {
        panic!("Expected actual deep-link result");
    };
    assert_eq!(serde_json::to_value(page).unwrap()["state"], "running");
    *fixture.released.lock().unwrap() = true;
    fixture.wake.notify_all();
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if backend.events_after(cursor).unwrap().iter().any(|event| matches!(&event.payload, BackendEvent::OperationFinished { operation_id, success: true, .. } if operation_id == id)) { break; }
        assert!(
            Instant::now() < deadline,
            "Real metadata worker did not terminate"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let completed = forward_deeplink_to_existing_profile(
        &guard,
        id.into(),
        link.into(),
        Duration::from_secs(3),
    )
    .unwrap();
    assert_eq!(completed.disposition, RelayDisposition::Completed);
    assert_eq!(completed.operation_id.as_deref(), Some(id));
    assert_eq!(
        serde_json::from_value::<
            lx_gpui_player::system_control::instance_relay::RelayAcknowledgement,
        >(serde_json::to_value(&completed).unwrap())
        .unwrap(),
        completed
    );
    drop(guard);
    backend.shutdown(Duration::from_secs(8)).unwrap();
}

#[test]
fn failed_repository_open_keeps_actual_guard_and_connection_owner_inspectable() {
    let _case = CASES.lock().unwrap();
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let guard = ProfileGuard::acquire(directory.path()).unwrap();
    std::fs::write(
        directory.path().join("native-library.sqlite3"),
        b"invalid SQLite database",
    )
    .unwrap();
    let error =
        BackendHandle::start_with_guard(BackendConfig::headless(directory.path().into()), guard)
            .err()
            .expect("Invalid SQLite must fail");
    let retained = error
        .downcast_ref::<RetainedRepositoryOpenFailure>()
        .expect("Actual opening owner must survive in typed error");
    assert!(retained.has_owner());
    let competing = ProfileGuard::acquire(directory.path()).unwrap_err();
    assert!(
        competing
            .owner
            .as_ref()
            .is_some_and(|guard| !guard.is_locked())
    );
    drop(competing);
    let owner = retained.take_owner().unwrap();
    assert!(!retained.has_owner());
    match &owner {
        RepositoryOpenOwner::Opening(opening) => opening.guard.verify().unwrap(),
        RepositoryOpenOwner::Repository(_) => {
            panic!("Invalid SQLite failed after repository initialization")
        }
    }
    drop(owner);
    drop(error);
    let recovered = ProfileGuard::acquire(directory.path()).unwrap();
    recovered.verify().unwrap();
}
