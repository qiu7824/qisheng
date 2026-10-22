#![cfg(windows)]

#[path = "support/native_producer_helpers.rs"]
mod native;

use anyhow::Result;
use lx_gpui_player::download::{
    ApprovedDownloadUrl, CollisionPolicy, DestinationGrant, DownloadCancel, DownloadContext,
    DownloadExecutionLease, DownloadPlan, DownloadPool, DownloadRepresentationIdentity, OwnedPart,
    OwnedPartRecord, SymphoniaVerifier, TransferJob, TransferOptions, TransferOutcome,
    artifact::{
        OwnedArtifactAllocation, OwnedArtifactIntent, OwnedArtifactJournal, OwnedArtifactKind,
    },
    planning::DownloadQuality,
    run_download_producer,
};
use rusqlite::Connection;
use std::{
    fs::OpenOptions,
    os::windows::fs::OpenOptionsExt,
    sync::{Arc, Mutex},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

struct Case {
    directory: native::FixtureDirectory,
    plan: DownloadPlan,
    context: DownloadContext,
    journal: Arc<native::RepositoryJournal>,
}
impl Case {
    fn new() -> Self {
        let directory = native::fixture_directory().unwrap();
        let grant = DestinationGrant::open_selected(directory.path(), 1).unwrap();
        let plan = grant.plan("authored.wav", CollisionPolicy::Skip).unwrap();
        let context = DownloadContext {
            profile_id: "native-journal-profile".into(),
            task_id: "native-journal-task".into(),
            operation_epoch: 1,
            source_id: "native-journal-source".into(),
            source_content_sha256: "a7".repeat(32),
            policy_epoch: 1,
        };
        let journal = native::journal_for_plan(&plan, &context).unwrap();
        Self {
            directory,
            plan,
            context,
            journal,
        }
    }
    fn assert_held(&self, record: &OwnedPartRecord, bytes: &[u8]) {
        let path = self.directory.path().join(&record.basename);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let error = OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, self.directory.path().join("foreign-name.part")).is_err());
    }
    fn retire(&self, record: OwnedPartRecord) {
        let path = self.directory.path().join(&record.basename);
        assert_eq!(
            OwnedPart::retire_retained(self.plan.clone(), record.clone()).unwrap(),
            record
        );
        assert!(!path.exists());
    }
}
struct FaultJournal {
    repository: Arc<native::RepositoryJournal>,
    observed: Arc<Mutex<Option<OwnedArtifactAllocation>>>,
    panic: bool,
}
impl OwnedArtifactJournal for FaultJournal {
    fn record_intent(&self, intent: &OwnedArtifactIntent) -> Result<()> {
        self.repository.record_intent(intent)
    }
    fn record_created(&self, allocation: &OwnedArtifactAllocation) -> Result<()> {
        *self.observed.lock().unwrap() = Some(allocation.clone());
        assert_eq!(
            std::fs::metadata(
                allocation
                    .working_directory_path
                    .join(&allocation.record.basename)
            )?
            .len(),
            0
        );
        if self.panic {
            panic!("Actual journal callback panic after native creation");
        }
        self.repository.record_created(allocation)
    }
}

#[test]
fn sqlite_creation_ack_refusal_retains_the_original_native_file_and_pending_intent() {
    let case = Case::new();
    let sql = Connection::open(case.journal.repository.path()).unwrap();
    sql.execute_batch("CREATE TRIGGER refuse_actual_creation BEFORE INSERT ON download_owned_artifacts BEGIN SELECT RAISE(ABORT,'native allocation ACK refused'); END;").unwrap();
    let observed = Arc::new(Mutex::new(None));
    let journal = FaultJournal {
        repository: case.journal.clone(),
        observed: observed.clone(),
        panic: false,
    };
    let plan = case.plan.clone();
    let context = case.context.clone();
    let result = run_download_producer(move || {
        OwnedPart::create_journaled(
            plan,
            OwnedArtifactKind::Transfer,
            &context,
            4096,
            &journal,
            &DownloadCancel::default(),
        )
    });
    assert!(format!("{:#}", result.err().unwrap()).contains("native allocation ACK refused"));
    let actual = observed.lock().unwrap().take().unwrap();
    case.assert_held(&actual.record, b"");
    let intents = case
        .journal
        .repository
        .download_allocation_intents(Some(&case.context.task_id))
        .unwrap();
    assert_eq!(intents, vec![OwnedArtifactIntent::from_created(&actual)]);
    assert!(
        case.journal
            .repository
            .download_owned_artifacts(Some(&case.context.task_id))
            .unwrap()
            .is_empty()
    );
    case.retire(actual.record);
    assert_eq!(
        case.journal
            .repository
            .download_allocation_intents(Some(&case.context.task_id))
            .unwrap(),
        intents
    );
}

#[test]
fn actual_native_creation_callback_panic_does_not_drop_the_captured_file() {
    let case = Case::new();
    let observed = Arc::new(Mutex::new(None));
    let journal = FaultJournal {
        repository: case.journal.clone(),
        observed: observed.clone(),
        panic: true,
    };
    let plan = case.plan.clone();
    let context = case.context.clone();
    assert!(
        run_download_producer(move || OwnedPart::create_journaled(
            plan,
            OwnedArtifactKind::Transfer,
            &context,
            4096,
            &journal,
            &DownloadCancel::default()
        ))
        .is_err()
    );
    let actual = observed.lock().unwrap().take().unwrap();
    case.assert_held(&actual.record, b"");
    assert_eq!(
        case.journal
            .repository
            .download_allocation_intents(Some(&case.context.task_id))
            .unwrap(),
        vec![OwnedArtifactIntent::from_created(&actual)]
    );
    case.retire(actual.record);
}

#[test]
fn dropping_a_completed_producer_result_keeps_the_actual_body_until_explicit_retirement() {
    let case = Case::new();
    let mut body = Vec::new();
    body.extend_from_slice(b"RIFF");
    body.extend_from_slice(&(36u32 + 2048).to_le_bytes());
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&8000u32.to_le_bytes());
    body.extend_from_slice(&16000u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&2048u32.to_le_bytes());
    for index in 0..1024 {
        body.extend_from_slice(&(((index as f64 * 0.17).sin() * 10000.0) as i16).to_le_bytes());
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/authored.wav", listener.local_addr().unwrap());
    let response_body = body.clone();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "Actual transfer made no HTTP request"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("{error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() <= 16384);
        }
        assert!(
            String::from_utf8(header)
                .unwrap()
                .starts_with("GET /authored.wav ")
        );
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"authored-native\"\r\nConnection: close\r\n\r\n", response_body.len()).unwrap();
        socket.write_all(&response_body).unwrap();
    });
    let mut task = case
        .journal
        .repository
        .download_task(&case.context.task_id)
        .unwrap()
        .unwrap();
    let old_revision = task.revision;
    task.revision += 1;
    task.url_bookmark = Some(
        lx_gpui_player::download::task_model::PrivateUrlBookmark::new(
            url.clone(),
            case.context.clone(),
        )
        .unwrap(),
    );
    case.journal
        .repository
        .update_download_task(&task, old_revision, task.operation_epoch, None, &|| Ok(()))
        .unwrap();
    let part = case
        .journal
        .create(
            case.plan.clone(),
            OwnedArtifactKind::Transfer,
            case.context.clone(),
            4096,
            DownloadCancel::default(),
        )
        .unwrap();
    let approved =
        ApprovedDownloadUrl::from_exact_approval(&url, &url, &case.context, &case.context).unwrap();
    let representation = DownloadRepresentationIdentity::from_approved_url(
        &approved,
        "37".repeat(32),
        DownloadQuality::Wav,
    )
    .unwrap();
    let pool = DownloadPool::new(1).unwrap();
    let handle = pool
        .submit(TransferJob {
            approved_url: approved,
            representation,
            execution_lease: DownloadExecutionLease::from_approved_context(case.context.clone())
                .unwrap(),
            part,
            checkpoint: None,
            extension: "wav".into(),
            redirect_count: 0,
            options: TransferOptions {
                max_bytes: 4096,
                automatic_retries: 0,
                ..Default::default()
            },
            verifier: Arc::new(SymphoniaVerifier),
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let verified = loop {
        if let Some(outcome) = handle.take_outcome() {
            match outcome {
                TransferOutcome::Verified(audio) => break audio,
                TransferOutcome::Failed { error, .. } => {
                    panic!("Actual WAV transfer failed: {error:?}")
                }
                _ => panic!("Actual WAV transfer did not produce verified audio"),
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Actual WAV producer did not join"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    server.join().unwrap();
    assert!(verified.verification().decoded_frames > 0);
    let record = verified.owned_record().clone();
    let checkpoint = verified.checkpoint().clone();
    case.journal
        .repository
        .verify_download_artifact(
            &record.nonce,
            1,
            checkpoint.bytes,
            &checkpoint.sha256,
            &|| Ok(()),
        )
        .unwrap();
    drop(verified);
    pool.shutdown(std::time::Duration::from_secs(2)).unwrap();
    case.assert_held(&record, &body);
    let row = case
        .journal
        .repository
        .download_owned_artifacts(Some(&case.context.task_id))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(row.phase, "verified");
    assert_eq!(row.actual_bytes, body.len() as u64);
    assert_eq!(row.reserved_bytes, row.actual_bytes);
    case.retire(record);
    let after = case
        .journal
        .repository
        .download_owned_artifacts(Some(&case.context.task_id))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(after.phase, "verified");
    assert_eq!(after.actual_bytes, row.actual_bytes);
}
