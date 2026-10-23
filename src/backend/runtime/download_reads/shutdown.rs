//! Joined retirement of downloaded read recipients and their original owners.

use super::*;
#[cfg(test)]
#[allow(
    dead_code,
    reason = "The shared encoder also supplies sibling codec fixtures"
)]
#[path = "../../../../tests/support/encoded_audio_fixture.rs"]
mod encoded;

impl Runtime {
    pub(in crate::backend::runtime) fn begin_download_read_shutdown(&mut self) {
        self.download_reads.closing = true;
    }

    pub(in crate::backend::runtime) fn poll_download_read_shutdown_step(&mut self) -> bool {
        if !self.download_reads.closing {
            return false;
        }
        let jobs = std::mem::take(&mut self.download_reads.jobs);
        for (id, mut job) in jobs {
            if self
                .downloads
                .release_finished_stage(&id, job.task.operation_epoch, "library")
                .is_err()
            {
                self.download_reads.jobs.insert(id, job);
                continue;
            }
            if let Some(operation) = job.play_request.take() {
                self.publish_finished(
                    operation,
                    &Err(BackendError::new(ErrorKind::Cancelled, "Backend shutting down").into()),
                );
            }
            if job.retained.is_none() {
                let result = job
                    .result
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(Ok(prepared)) = result {
                    job.retained = Some(prepared);
                }
            }
            let retiring = (|| -> Result<bool> {
                let mut nonces = vec![job.producer.record_binding()?.nonce];
                if let Some((_, object)) = &job.sidecar {
                    nonces.push(object.record_binding()?.nonce);
                }
                if job.shutdown_pins.is_empty() {
                    if let Some(prepared) = &job.retained {
                        job.shutdown_pins.push(prepared.receipt.pin_receipt()?);
                        if let Some((_, receipt)) = &prepared.sidecar {
                            job.shutdown_pins.push(receipt.pin_receipt()?);
                        }
                    }
                }
                self.stage_shutdown_download_pins(&job.task, &nonces, &mut job.shutdown_pins)?;
                self.retire_shutdown_download_read(&job.task, &nonces, &job.shutdown_pins)
            })();
            match retiring {
                Ok(true) => drop(job),
                Ok(false) => {
                    self.download_reads.jobs.insert(id, job);
                }
                Err(error) => {
                    let message: String = format!("{error:#}").chars().take(1024).collect();
                    if job.error.as_ref() != Some(&message) {
                        self.error(format!(
                            "Download read retirement retains its actual owner: {message}"
                        ));
                    }
                    job.error = Some(message);
                    self.download_reads.jobs.insert(id, job);
                }
            }
        }
        let recovering = std::mem::take(&mut self.download_reads.recovering);
        for (id, mut job) in recovering {
            if self
                .downloads
                .release_finished_stage(&id, job.task.operation_epoch, "recovery")
                .is_err()
            {
                self.download_reads.recovering.insert(id, job);
                continue;
            }
            if job.error.is_none() {
                self.publish_finished(
                    job.operation.clone(),
                    &Err(BackendError::new(ErrorKind::Cancelled, "Backend shutting down").into()),
                );
                job.error = Some("Backend shutting down".into());
            }
            if job.retained.is_none() {
                let result = job
                    .result
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(Ok(recovered)) = result {
                    job.retained = Some(recovered);
                }
            }
            let retiring = (|| -> Result<bool> {
                let nonces = self.published_read_nonces(&job.task)?;
                self.stage_shutdown_download_pins(&job.task, &nonces, &mut job.shutdown_pins)?;
                self.retire_shutdown_download_read(&job.task, &nonces, &job.shutdown_pins)
            })();
            match retiring {
                Ok(true) => drop(job),
                Ok(false) => {
                    self.download_reads.recovering.insert(id, job);
                }
                Err(error) => {
                    let message: String = format!("{error:#}").chars().take(1024).collect();
                    if job.error.as_ref() != Some(&message) {
                        self.error(format!(
                            "Cold download read retirement retains its actual owner: {message}"
                        ));
                    }
                    job.error = Some(message);
                    self.download_reads.recovering.insert(id, job);
                }
            }
        }
        #[cfg(test)]
        tests::record_shutdown_state(self);
        self.download_reads.jobs.is_empty() && self.download_reads.recovering.is_empty()
    }

    fn stage_shutdown_download_pins(
        &self,
        task: &DownloadTask,
        nonces: &[String],
        pins: &mut Vec<crate::library::authority::DownloadedPinReceipt>,
    ) -> Result<()> {
        for object in crate::download::retained_read_task_objects_after_join(
            &task.profile_id,
            &task.id,
            nonces,
        )? {
            let covered = object.inspect_retained_pins_after_join(|grant| {
                Ok(pins
                    .iter()
                    .any(|receipt| receipt.matches_grant(grant).is_ok()))
            })?;
            if !covered {
                pins.push(
                    self.library
                        .authority
                        .stage_download_pin_retirement(&object)?,
                );
            }
        }
        Ok(())
    }

    fn retire_shutdown_download_read(
        &mut self,
        task: &DownloadTask,
        nonces: &[String],
        pins: &[crate::library::authority::DownloadedPinReceipt],
    ) -> Result<bool> {
        #[cfg(test)]
        tests::before_close(
            self.repository
                .path()
                .parent()
                .context("Profile directory missing")?,
            &task.id,
        )?;
        if !crate::download::close_read_task_handles_after_join(&task.profile_id, &task.id, nonces)?
        {
            return Ok(false);
        }
        // The runtime returns every true joined stage borrow. The process keeper
        // continues to own the closed original state until its actual pins move.
        for stage in [
            "prepare",
            "postprocess",
            "publication",
            "recovery",
            "library",
        ] {
            self.downloads
                .release_finished_stage(&task.id, task.operation_epoch, stage)?;
        }
        crate::download::retire_read_task_pins_for_recovery(
            &task.profile_id,
            &task.id,
            nonces,
            pins,
        )
    }

    fn published_read_nonces(&self, task: &DownloadTask) -> Result<Vec<String>> {
        let audio = task
            .output_audio
            .as_ref()
            .context("Published download proof missing during retirement")?;
        let allocations = self.repository.download_owned_artifacts(Some(&task.id))?;
        let matches =
            |artifact: &crate::repository::DownloadOwnedArtifact,
             proof: &crate::download::task_model::DownloadArtifactProof| {
                artifact.phase == "published"
                    && artifact.allocation.record.identity == proof.identity
                    && artifact.allocation.record.final_basename == proof.basename
                    && artifact.actual_bytes == proof.bytes
                    && artifact.sha256.as_deref() == Some(proof.sha256.as_str())
            };
        let mut nonces = vec![
            allocations
                .iter()
                .find(|artifact| matches(artifact, audio))
                .context("Actual read publication allocation is unavailable")?
                .allocation
                .record
                .nonce
                .clone(),
        ];
        if let Some(sidecar) = &task.stages.lyrics.artifact {
            nonces.push(
                allocations
                    .iter()
                    .find(|artifact| matches(artifact, sidecar))
                    .context("Actual sidecar publication allocation is unavailable")?
                    .allocation
                    .record
                    .nonce
                    .clone(),
            );
        }
        Ok(nonces)
    }
}

#[cfg(test)]
pub(super) fn after_read_result(profile: &std::path::Path, task: &str, stage: &str) {
    tests::after_result(profile, task, stage);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        BackendConfig, BackendEvent, BackendHandle, BackendQuery, BackendQueryResult,
        CommandEnvelope,
    };
    use std::{
        io::{Read, Write},
        path::Path,
        sync::{
            Condvar, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
    };

    const WAIT: Duration = Duration::from_secs(30);

    #[derive(Default)]
    struct ResultGate {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }
    impl ResultGate {
        fn enter(&self) {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        fn wait_entered(&self) {
            let deadline = Instant::now() + WAIT;
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            while !state.0 {
                assert!(
                    Instant::now() < deadline,
                    "Actual READ producer did not reach its result gate"
                );
                state = self
                    .changed
                    .wait_timeout(state, Duration::from_millis(10))
                    .unwrap_or_else(|error| error.into_inner())
                    .0;
            }
        }
        fn release(&self) {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .1 = true;
            self.changed.notify_all();
        }
    }
    struct Hooks {
        task: String,
        stage: &'static str,
        result: ResultGate,
        close: ResultGate,
        reject_close_once: AtomicBool,
    }
    static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Hooks>>>> = OnceLock::new();
    static DIAGNOSTICS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    fn profile_key(profile: &Path) -> PathBuf {
        std::fs::canonicalize(profile).unwrap_or_else(|_| profile.to_owned())
    }
    pub(super) fn record_shutdown_state(core: &Runtime) {
        let Some(profile) = core.repository.path().parent() else {
            return;
        };
        let profile_id = crate::domain::music::stable_id(
            "download-profile",
            profile.to_string_lossy().as_bytes(),
        );
        let errors = core
            .download_reads
            .jobs
            .values()
            .filter_map(|job| job.error.as_deref())
            .chain(
                core.download_reads
                    .recovering
                    .values()
                    .filter_map(|job| job.error.as_deref()),
            )
            .take(2)
            .map(|error| error.chars().take(256).collect::<String>())
            .collect::<Vec<_>>();
        let state = format!(
            "warm_jobs={}, cold_jobs={}, pin_receipts={}, library_scope={}, profile_native_owners={:?}, errors={errors:?}",
            core.download_reads.jobs.len(),
            core.download_reads.recovering.len(),
            core.download_reads
                .jobs
                .values()
                .map(|job| job.shutdown_pins.len())
                .sum::<usize>()
                + core
                    .download_reads
                    .recovering
                    .values()
                    .map(|job| job.shutdown_pins.len())
                    .sum::<usize>(),
            core.library.authority.retirement_pending(),
            crate::download::has_retained_download_objects(&profile_id)
        );
        let mut diagnostics = DIAGNOSTICS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if diagnostics.contains_key(profile) || diagnostics.len() < 16 {
            diagnostics.insert(profile.to_owned(), state);
        }
    }
    fn diagnostic(profile: &Path) -> String {
        DIAGNOSTICS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&profile_key(profile))
            .cloned()
            .unwrap_or_else(|| "No actor READ retirement snapshot".into())
    }
    pub(super) fn after_result(profile: &Path, task: &str, stage: &str) {
        let hooks = HOOKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&profile_key(profile))
            .cloned();
        if let Some(hooks) = hooks
            && hooks.task == task
            && hooks.stage == stage
        {
            hooks.result.enter();
        }
    }
    pub(super) fn before_close(profile: &Path, task: &str) -> Result<()> {
        let hooks = HOOKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&profile_key(profile))
            .cloned();
        if let Some(hooks) = hooks
            && hooks.task == task
            && hooks.reject_close_once.swap(false, Ordering::AcqRel)
        {
            hooks.close.enter();
            anyhow::bail!("Injected original download close failure");
        }
        Ok(())
    }

    struct Case {
        directory: Option<tempfile::TempDir>,
        backend: Option<BackendHandle>,
        hooks: Option<Arc<Hooks>>,
    }
    struct HttpOwner {
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl HttpOwner {
        fn join(&mut self) {
            self.worker.take().unwrap().join().unwrap();
        }
    }
    impl Drop for HttpOwner {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
    impl Case {
        fn new() -> Self {
            let temporary_root = std::env::temp_dir().join("lx-native-fixtures");
            let root = temporary_root.as_path();
            std::fs::create_dir_all(root).unwrap();
            let directory = tempfile::Builder::new()
                .prefix("download-read-shutdown-")
                .tempdir_in(root)
                .unwrap();
            let backend =
                BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                    .unwrap();
            Self {
                directory: Some(directory),
                backend: Some(backend),
                hooks: None,
            }
        }
        fn profile(&self) -> PathBuf {
            self.directory.as_ref().unwrap().path().join("profile")
        }
        fn backend(&self) -> &BackendHandle {
            self.backend.as_ref().unwrap()
        }
        fn execute(&self, id: &str, command: BackendCommand) -> crate::backend::CommandReceipt {
            self.backend()
                .execute(CommandEnvelope {
                    request_id: id.into(),
                    origin: "download-read-shutdown-test".into(),
                    expected_revision: None,
                    command,
                })
                .unwrap()
        }
        fn terminal_count(&self, id: &str, cursor: u64) -> Vec<bool> {
            self.backend()
                .events_after(cursor)
                .unwrap()
                .into_iter()
                .filter_map(|event| match event.payload {
                    BackendEvent::OperationFinished {
                        operation_id,
                        success,
                        ..
                    } if operation_id == id => Some(success),
                    _ => None,
                })
                .collect()
        }
        fn finished(&self, id: &str, cursor: u64) {
            wait(|| !self.terminal_count(id, cursor).is_empty());
            assert_eq!(
                self.terminal_count(id, cursor),
                [true],
                "Operation {id}: {:?}",
                self.backend().events_after(cursor).unwrap()
            );
        }
        fn tasks(&self) -> Vec<crate::download::task_model::DownloadTaskView> {
            match self.backend().query(BackendQuery::DownloadTasks).unwrap() {
                BackendQueryResult::DownloadTasks(tasks) => tasks,
                _ => panic!("Wrong query kind"),
            }
        }
        fn actual_task(&self, id: &str) -> DownloadTask {
            let connection = rusqlite::Connection::open_with_flags(
                self.profile().join(crate::repository::DATABASE_NAME),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let document: String = connection
                .query_row(
                    "SELECT document FROM download_tasks WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            serde_json::from_str(&document).unwrap()
        }
        fn published_nonce(&self, id: &str) -> String {
            let connection = rusqlite::Connection::open_with_flags(
                self.profile().join(crate::repository::DATABASE_NAME),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            connection.query_row("SELECT nonce FROM download_owned_artifacts WHERE task_id=?1 AND phase='published'", [id], |row| row.get(0)).unwrap()
        }
        fn prepare_gated_download(&mut self) -> (String, PathBuf, Vec<u8>) {
            use crate::domain::{
                lists::{AddLocation, ListAction},
                music::{LxMusicDtoVersion, TrackRecord},
            };
            use crate::download::{
                DownloadState, planning::DownloadQuality, task_model::DownloadSettings,
            };
            let destination = self
                .directory
                .as_ref()
                .unwrap()
                .path()
                .join("selected-output");
            std::fs::create_dir(&destination).unwrap();
            let cursor = self.backend().event_cursor();
            self.execute(
                "read-destination",
                BackendCommand::AddDownloadDestination {
                    path: destination.clone(),
                },
            );
            self.finished("read-destination", cursor);
            let BackendQueryResult::DownloadDestinations(mut destinations) = self
                .backend()
                .query(BackendQuery::DownloadDestinations)
                .unwrap()
            else {
                panic!("Destination query");
            };
            self.execute(
                "read-settings",
                BackendCommand::SetDownloadSettings {
                    settings: DownloadSettings {
                        destination_grant_id: Some(destinations.pop().unwrap().id),
                        embed_cover: false,
                        download_lyrics: false,
                        ..Default::default()
                    },
                },
            );
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/original.flac", listener.local_addr().unwrap());
            let body = encoded::mono_verbatim_flac_frames(32);
            let served = body.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = stop.clone();
            let server = thread::spawn(move || {
                let deadline = Instant::now() + WAIT;
                loop {
                    if stopping.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((mut socket, _)) => {
                            socket.set_nonblocking(false).unwrap();
                            socket
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            let mut headers = Vec::new();
                            while !headers.ends_with(b"\r\n\r\n") {
                                let mut byte = [0];
                                assert_eq!(socket.read(&mut byte).unwrap(), 1);
                                headers.extend(byte);
                            }
                            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"read-shutdown\"\r\nConnection: close\r\n\r\n", served.len()).unwrap();
                            socket.write_all(&served).unwrap();
                            return;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "Actual download did not reach the loopback server"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
            });
            let mut server = HttpOwner {
                stop,
                worker: Some(server),
            };
            let script = format!(
                r#"/** @name Read shutdown fixture */
lx.on('request',async({{action,info}})=>{{if(action!=='musicUrl'||info.type!=='flac')throw Error('invalid');return {};}});
lx.send('inited',{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['flac']}}}}}});"#,
                serde_json::to_string(&url).unwrap()
            );
            let source = crate::source::script_hash(&script);
            for (id, command) in [
                (
                    "read-source-import",
                    BackendCommand::ImportSourceText { script },
                ),
                (
                    "read-source-approve",
                    BackendCommand::ApproveSource { id: source.clone() },
                ),
                (
                    "read-source-enable",
                    BackendCommand::EnableSource { id: source },
                ),
            ] {
                let cursor = self.backend().event_cursor();
                self.execute(id, command);
                self.finished(id, cursor);
            }
            let track = TrackRecord::from_lx_dto(serde_json::json!({"source":"kw","songmid":"read-shutdown-owned",
                "name":"Read shutdown","singer":"Fixture","albumName":"Synthetic","interval":"00:01",
                "types":[{"type":"flac"}],"_types":{"flac":{}}}), LxMusicDtoVersion::Legacy).unwrap();
            self.execute(
                "read-list",
                BackendCommand::ListAction {
                    action: Box::new(ListAction::MusicAdd {
                        list_id: "default".into(),
                        tracks: vec![track.clone()],
                        location: AddLocation::Top,
                    }),
                },
            );
            self.execute(
                "read-add",
                BackendCommand::AddDownloadTasks {
                    list_id: "default".into(),
                    music_ids: vec![track.id],
                    quality: DownloadQuality::Flac,
                    top: true,
                },
            );
            let task = self.tasks().pop().unwrap();
            self.execute(
                "read-resume",
                BackendCommand::ResumeDownloadTask {
                    task_id: task.id.clone(),
                    expected_task_revision: task.revision,
                },
            );
            wait(|| {
                self.tasks()
                    .iter()
                    .any(|task| task.state == DownloadState::AwaitingUrlApproval)
            });
            let BackendQueryResult::DownloadPendingUrl(Some(pending)) = self
                .backend()
                .query(BackendQuery::DownloadPendingUrl {
                    task_id: task.id.clone(),
                })
                .unwrap()
            else {
                panic!("Actual URL approval missing");
            };
            let hooks = Arc::new(Hooks {
                task: task.id.clone(),
                stage: "library",
                result: ResultGate::default(),
                close: ResultGate::default(),
                reject_close_once: AtomicBool::new(false),
            });
            HOOKS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(profile_key(&self.profile()), hooks.clone());
            self.hooks = Some(hooks.clone());
            self.execute(
                "read-url-approved",
                BackendCommand::ApproveDownloadUrl {
                    task_id: task.id.clone(),
                    expected_task_revision: pending.task_revision,
                    operation_epoch: pending.operation_epoch,
                    url: pending.url,
                },
            );
            hooks.result.wait_entered();
            server.join();
            let completed = self
                .tasks()
                .into_iter()
                .find(|row| row.id == task.id)
                .unwrap();
            assert!(completed.audio_available);
            assert_eq!(completed.state, DownloadState::Completed);
            let output = destination.join(&completed.output_audio.as_ref().unwrap().basename);
            let published = std::fs::read(&output).unwrap();
            assert_ne!(
                published, body,
                "Actual postprocessing must have published the tagged output"
            );
            (task.id, output, published)
        }
        fn shutdown(&self) {
            self.backend().shutdown(WAIT).unwrap_or_else(|error| {
                panic!(
                    "Actual READ shutdown failed: {error}; {}",
                    diagnostic(&self.profile())
                )
            });
            let retired = self.backend().retirement_state();
            assert!(
                retired.actor_joined && retired.runtime_retired,
                "{retired:?}"
            );
        }
    }
    impl Drop for Case {
        fn drop(&mut self) {
            if let Some(hooks) = &self.hooks {
                hooks.result.release();
                hooks.close.release();
            }
            if let Some(backend) = self.backend.take()
                && let Err(error) = backend.shutdown(WAIT)
            {
                if let Some(directory) = self.directory.take() {
                    let _ = directory.keep();
                }
                if !thread::panicking() {
                    panic!("Actual download reader retirement failed: {error}");
                }
                return;
            }
            if let Some(registry) = HOOKS.get() {
                registry
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&profile_key(&self.profile()));
            }
            if let Some(diagnostics) = DIAGNOSTICS.get() {
                diagnostics
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&profile_key(&self.profile()));
            }
        }
    }
    fn wait(mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !check() {
            assert!(
                Instant::now() < deadline,
                "Actual READ lifecycle did not reach its expected phase"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn shutdown_waits_for_real_read_thread_exit_and_keeps_the_cold_publication() {
        let mut case = Case::new();
        let (task, output, body) = case.prepare_gated_download();
        let cursor = case.backend().event_cursor();
        let receipt = case.execute(
            "paused-read-play",
            BackendCommand::PlayDownloadTask {
                task_id: task.clone(),
            },
        );
        assert_eq!(receipt.state, crate::backend::ReceiptState::Accepted);
        assert!(case.backend().shutdown(Duration::from_millis(50)).is_err());
        assert!(case.terminal_count("paused-read-play", cursor).is_empty());
        assert!(!case.backend().retirement_state().runtime_retired);
        assert_eq!(std::fs::read(&output).unwrap(), body);
        case.hooks.as_ref().unwrap().result.release();
        case.shutdown();
        assert_eq!(case.terminal_count("paused-read-play", cursor), [false]);
        assert_eq!(std::fs::read(&output).unwrap(), body);
        let repository = Repository::open(case.profile()).unwrap();
        let task_row = repository.download_task(&task).unwrap().unwrap();
        assert!(task_row.audio_available);
        assert_eq!(
            repository
                .download_owned_artifacts(Some(&task))
                .unwrap()
                .iter()
                .filter(|artifact| artifact.phase == "published")
                .count(),
            1
        );
        drop(repository);
        HOOKS
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&profile_key(&case.profile()));
        case.backend = Some(BackendHandle::start(BackendConfig::headless(case.profile())).unwrap());
        case.execute("cold-read-source-disabled", BackendCommand::DisableSource);
        case.execute(
            "cold-read-no-advance",
            BackendCommand::SetPlayMode {
                mode: crate::playback::queue::PlayMode::None,
            },
        );
        case.execute(
            "cold-read-no-error-skip",
            BackendCommand::SetAutoSkipOnError { enabled: false },
        );
        case.execute(
            "cold-read-no-temporary",
            BackendCommand::ClearTemporaryQueue,
        );
        let before_play = case.backend().snapshot().playback.generation;
        let cursor = case.backend().event_cursor();
        let receipt = case.execute(
            "cold-read-play",
            BackendCommand::PlayDownloadTask { task_id: task },
        );
        assert_eq!(receipt.state, crate::backend::ReceiptState::Accepted);
        case.finished("cold-read-play", cursor);
        let BackendQueryResult::LibraryAssets(assets) = case
            .backend()
            .query(BackendQuery::LibraryAssets {
                root_id: None,
                after_asset_id: None,
                limit: 8,
                expected_revision: None,
            })
            .unwrap()
        else {
            panic!("Downloaded asset query");
        };
        let expected_track = assets
            .items
            .iter()
            .find(|view| {
                crate::domain::music::normalize_local_path(&view.asset.path)
                    == crate::domain::music::normalize_local_path(&output)
            })
            .expect("Actual downloaded asset missing")
            .asset
            .track_id
            .clone();
        wait(|| {
            let before = case.backend().snapshot();
            let BackendQueryResult::RenderedPcm(Some(pcm)) =
                case.backend().query(BackendQuery::RenderedPcm).unwrap()
            else {
                return false;
            };
            let after = case.backend().snapshot();
            before.playback.track_id.as_deref() == Some(expected_track.as_str())
                && after.playback.track_id == before.playback.track_id
                && after.playback.generation == before.playback.generation
                && after.playback.generation > before_play
                && after.playback.has_presented_media
                && after.playback.position_seconds > 0.0
                && after.playback.error.is_none()
                && after.playback.failure.is_none()
                && pcm.peak > 0.005
                && pcm.rms > 0.001
        });
        let BackendQueryResult::Queue(queue) = case
            .backend()
            .query(BackendQuery::Queue {
                offset: 0,
                limit: 8,
            })
            .unwrap()
        else {
            panic!("Downloaded queue query");
        };
        assert_eq!(queue.mode, crate::playback::queue::PlayMode::None);
        assert!(!queue.auto_skip_on_error && queue.temporary.is_empty() && queue.next.is_none());
        assert_eq!(
            queue.current_track.as_ref().map(|track| track.id.as_str()),
            Some(expected_track.as_str())
        );
        case.execute("cold-read-stop", BackendCommand::Stop);
        case.shutdown();
        assert_eq!(std::fs::read(output).unwrap(), body);
    }

    #[test]
    fn injected_shutdown_close_failure_keeps_original_and_receiver_until_retry() {
        let mut case = Case::new();
        let (task, output, body) = case.prepare_gated_download();
        let cursor = case.backend().event_cursor();
        case.execute(
            "close-failed-read-play",
            BackendCommand::PlayDownloadTask {
                task_id: task.clone(),
            },
        );
        let hooks = case.hooks.as_ref().unwrap();
        hooks.reject_close_once.store(true, Ordering::Release);
        assert!(case.backend().shutdown(Duration::from_millis(50)).is_err());
        assert!(
            case.terminal_count("close-failed-read-play", cursor)
                .is_empty()
        );
        hooks.result.release();
        hooks.close.wait_entered();
        assert_eq!(
            case.terminal_count("close-failed-read-play", cursor),
            [false]
        );
        let actual_task = case.actual_task(&task);
        let profile_id = actual_task.profile_id;
        assert!(crate::download::has_retained_download_objects(&profile_id).unwrap());
        let objects = crate::download::retained_read_task_objects_after_join(
            &profile_id,
            &task,
            &[case.published_nonce(&task)],
        )
        .unwrap();
        assert_eq!(objects.len(), 1);
        let proof = actual_task.output_audio.unwrap();
        assert_eq!(
            objects[0].record_binding().unwrap().identity,
            proof.identity
        );
        assert_eq!(
            objects[0].native_file().unwrap().metadata().unwrap().len(),
            proof.bytes
        );
        drop(objects);
        assert!(!case.backend().retirement_state().runtime_retired);
        let moved = output.with_extension("unexpected-moved");
        #[cfg(windows)]
        {
            let denied = std::fs::rename(&output, &moved)
                .expect_err("Retained native read/original must pin the published object");
            assert_eq!(denied.raw_os_error(), Some(32));
        }
        #[cfg(unix)]
        {
            std::fs::rename(&output, &moved).unwrap();
            let retained = crate::download::retained_read_task_objects_after_join(
                &profile_id,
                &task,
                &[case.published_nonce(&task)],
            )
            .unwrap();
            assert_eq!(retained.len(), 1);
            assert_eq!(
                retained[0].native_file().unwrap().metadata().unwrap().len(),
                proof.bytes
            );
            assert!(
                retained[0]
                    .verify_published_audio(
                        &proof,
                        "flac",
                        &crate::download::DownloadCancel::default()
                    )
                    .is_err(),
                "A moved Unix file must not retain authority for its old pathname"
            );
            assert_eq!(std::fs::read(&moved).unwrap(), body);
            std::fs::rename(&moved, &output).unwrap();
        }
        assert!(!moved.exists());
        assert_eq!(std::fs::read(&output).unwrap(), body);
        hooks.close.release();
        case.shutdown();
        assert_eq!(
            case.terminal_count("close-failed-read-play", cursor),
            [false]
        );
        assert!(!crate::download::has_retained_download_objects(&profile_id).unwrap());
        assert_eq!(std::fs::read(output).unwrap(), body);
        let repository = Repository::open(case.profile()).unwrap();
        assert!(
            repository
                .download_task(&task)
                .unwrap()
                .unwrap()
                .audio_available
        );
        assert_eq!(
            repository
                .download_owned_artifacts(Some(&task))
                .unwrap()
                .iter()
                .filter(|artifact| artifact.phase == "published")
                .count(),
            1
        );
    }
}
