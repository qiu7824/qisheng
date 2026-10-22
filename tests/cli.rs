//! Exercises the real CLI process and its interactive JSON Lines protocol.
use lx_gpui_player::repository::Repository;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct CliSession {
    child: Child,
    input: Option<ChildStdin>,
    responses: mpsc::Receiver<Result<Value, String>>,
    reader: Option<thread::JoinHandle<()>>,
    directory: TempDir,
}

impl CliSession {
    fn start() -> Self {
        Self::start_with_relay(false)
    }

    fn start_with_relay(instance_relay: bool) -> Self {
        let base = std::env::var_os("TMP")
            .or_else(|| std::env::var_os("TEMP"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("lx-native-cli-test-")
            .tempdir_in(base)
            .unwrap();
        let stderr = fs::File::create(directory.path().join("cli-stderr.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lx-native-cli"));
        command
            .args(["--json-lines", "--audio", "memory", "--data-dir"])
            .arg(directory.path())
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        if instance_relay {
            command.arg("--instance-relay");
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                let result = reader
                    .by_ref()
                    .take(4 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut bytes);
                match result {
                    Ok(0) => break,
                    Ok(_) if bytes.len() <= 4 * 1024 * 1024 => {
                        let response = serde_json::from_slice(&bytes)
                            .map_err(|error| format!("Invalid CLI JSON response: {error}"));
                        if sender.send(response).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {
                        let _ = sender.send(Err("CLI response exceeded 4 MiB".into()));
                        break;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(format!("Cannot read CLI response: {error}")));
                        break;
                    }
                }
            }
        });
        Self {
            child,
            input: Some(input),
            responses,
            reader: Some(reader),
            directory,
        }
    }

    fn send(&mut self, value: &Value) -> Value {
        self.send_raw(&serde_json::to_string(value).unwrap())
    }

    fn send_raw(&mut self, line: &str) -> Value {
        let input = self.input.as_mut().expect("CLI input was closed");
        writeln!(input, "{line}").unwrap();
        input.flush().unwrap();
        self.responses
            .recv_timeout(Duration::from_secs(8))
            .unwrap_or_else(|error| {
                panic!(
                    "CLI did not acknowledge an input line: {error}; stderr: {}",
                    fs::read_to_string(self.directory.path().join("cli-stderr.log"))
                        .unwrap_or_default()
                )
            })
            .unwrap()
    }

    fn status(&mut self) -> Value {
        let response = self.send(&json!({"query":"status"}));
        assert_eq!(response["ok"], true, "CLI status failed: {response}");
        response["result"].clone()
    }

    fn wait_operation(&mut self, cursor: &mut u64, operation: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let response = self.send(&json!({"query":"events","cursor":*cursor}));
            assert_eq!(response["ok"], true, "CLI event query failed: {response}");
            for event in response["result"].as_array().unwrap() {
                *cursor = event["event_id"].as_u64().unwrap();
                if event["payload"]["event"] == "operation_finished"
                    && event["payload"]["operation_id"] == operation
                {
                    return event["payload"].clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "CLI operation {operation} never terminated"
            );
            thread::sleep(Duration::from_millis(15));
        }
    }

    fn finish(&mut self) -> ExitStatus {
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not shut down after normal stdin EOF"
            );
            thread::sleep(Duration::from_millis(15));
        };
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        assert!(
            status.success(),
            "CLI exited with {status}; stderr: {}",
            fs::read_to_string(self.directory.path().join("cli-stderr.log")).unwrap_or_default()
        );
        status
    }
}

impl Drop for CliSession {
    fn drop(&mut self) {
        drop(self.input.take());
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn endpoint_http(address: &str, target: &str, token: Option<&str>) -> String {
    use std::net::TcpStream;
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    write!(
        socket,
        "GET {target} HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn json_lines_endpoint_is_explicit_authenticated_coherent_and_joined_on_stdin_eof() {
    let mut cli = CliSession::start();
    let disabled = cli.send(&json!({"query":"open_api"}));
    assert_eq!(disabled["result"]["state"], "disabled");
    let started = cli.send(&json!({"service":"open_api_start","config":{"port":0}}));
    assert_eq!(started["ok"], true);
    assert_eq!(started["result"]["state"], "listening");
    let address = started["result"]["address"].as_str().unwrap().to_owned();
    let token = started["result"]["bearer_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(token.len(), 64);
    assert!(endpoint_http(&address, "/status?filter=volume", None).starts_with("HTTP/1.1 401"));
    assert!(
        endpoint_http(&address, "/status?filter=volume", Some(&"0".repeat(64)))
            .starts_with("HTTP/1.1 401")
    );
    assert_eq!(
        cli.send(&json!({"request_id":"cli-endpoint-volume","command":"volume","volume":0.5}))["ok"],
        true
    );
    let response = endpoint_http(&address, "/status?filter=volume", Some(&token));
    assert!(response.starts_with("HTTP/1.1 200"));
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["volume"], 50);
    let view = cli.send(&json!({"query":"open_api"}));
    assert!(view["result"].get("bearer_token").is_none());
    assert!(!view.to_string().contains(&token));
    let before = Instant::now();
    cli.finish();
    assert!(before.elapsed() < Duration::from_secs(5));
    let rebound = TcpListener::bind(&address).unwrap();
    drop(rebound);
    let repo = Repository::open(cli.directory.path()).unwrap();
    repo.integrity_check().unwrap();
    drop(repo);
}

#[test]
fn endpoint_rotation_rejects_old_token_and_invalid_configuration_preserves_the_session() {
    let mut cli = CliSession::start();
    let old_token = "a".repeat(64);
    let new_token = "b".repeat(64);
    let started =
        cli.send(&json!({"service":"open_api_start","config":{"port":0,"bearer_token":old_token}}));
    assert_eq!(started["ok"], true);
    let address = started["result"]["address"].as_str().unwrap().to_owned();
    let port: std::net::SocketAddr = address.parse().unwrap();
    let invalid=cli.send(&json!({"service":"open_api_rotate","config":{"port":port.port(),"bind_lan":true,"legacy_loopback":true}}));
    assert_eq!(invalid["ok"], false);
    assert!(endpoint_http(&address, "/status", Some(&old_token)).starts_with("HTTP/1.1 200"));
    let rotated=cli.send(&json!({"service":"open_api_rotate","config":{"port":port.port(),"bearer_token":new_token}}));
    assert_eq!(rotated["ok"], true);
    assert_eq!(rotated["result"]["address"], address);
    assert!(
        rotated["result"]["session_epoch"].as_u64() > started["result"]["session_epoch"].as_u64()
    );
    assert!(endpoint_http(&address, "/status", Some(&old_token)).starts_with("HTTP/1.1 401"));
    assert!(endpoint_http(&address, "/status", Some(&new_token)).starts_with("HTTP/1.1 200"));
    let stopped = cli.send(&json!({"service":"open_api_stop"}));
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["result"]["state"], "disabled");
    let rebound = TcpListener::bind(address).unwrap();
    drop(rebound);
    cli.finish();
}

#[test]
fn rejected_endpoint_options_do_not_echo_the_token_or_activate_a_listener() {
    let mut cli = CliSession::start();
    let token = "d".repeat(64);
    let response=cli.send(&json!({"service":"open_api_start","config":{"port":0,"bearer_token":token,"unexpected":true}}));
    assert_eq!(response["ok"], false);
    assert!(!response.to_string().contains(&token));
    let ambiguous =
        cli.send(&json!({"query":"status","service":"open_api_start","config":{"port":0}}));
    assert_eq!(ambiguous["ok"], false);
    assert_eq!(
        cli.send(&json!({"query":"open_api"}))["result"]["state"],
        "disabled"
    );
    cli.finish();
}

#[test]
fn persistent_download_and_backup_queries_are_exposed_without_activating_services() {
    let mut cli = CliSession::start();
    let capabilities = cli.send(&json!({"query":"capabilities"}));
    assert_eq!(capabilities["ok"], true);
    for command in [
        "dispatch_deep_link",
        "preview_lx_backup",
        "add_download_tasks",
        "approve_download_url",
    ] {
        assert!(
            capabilities["result"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == command)
        );
    }
    for query in [
        "download_settings",
        "download_destinations",
        "download_tasks",
        "lx_backup_preview",
    ] {
        let response = cli.send(&json!({"query":query}));
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["result"]["kind"], query);
    }
    assert_eq!(
        cli.send(&json!({"query":"open_api"}))["result"]["state"],
        "disabled"
    );
    assert!(cli.status()["sources"].as_array().unwrap().is_empty());
    cli.finish();
}

#[test]
fn json_lines_status_search_and_invalid_line_keep_service_alive_until_eof() {
    let mut cli = CliSession::start();
    let status = cli.status();
    assert_eq!(status["protocol_version"], 1);
    assert_eq!(status["tracks"].as_array().unwrap().len(), 3);
    assert_eq!(status["playback"]["status"], "Idle");
    let page = cli.send(&json!({"query":"tracks","text":"  VIOLET  ","offset":0,"limit":1}));
    assert_eq!(page["ok"], true);
    assert_eq!(page["result"]["total"], 1);
    assert_eq!(page["result"]["items"][0]["id"], "tone-2");
    let invalid = cli.send_raw("{ malformed JSON }");
    assert_eq!(invalid["ok"], false);
    assert!(invalid["error"].is_string());
    assert!(cli.status()["error"].is_null());
    let invalid_query = cli.send(&json!({"query":"tracks","limit":0}));
    assert_eq!(invalid_query["ok"], false);
    let capabilities = cli.send(&json!({"query":"capabilities"}));
    assert_eq!(capabilities["ok"], true);
    assert_eq!(capabilities["result"]["headless"], true);
    assert_eq!(capabilities["result"]["complete_lx_backend"], false);
    cli.finish();
    let repository = Repository::open(cli.directory.path()).unwrap();
    repository.integrity_check().unwrap();
}

#[test]
fn json_lines_static_import_never_executes_and_completed_replay_returns_promptly() {
    let mut cli = CliSession::start();
    let request = json!({
        "request_id":"cli-static-import", "origin":"cli-test", "command":"import_source_text",
        "script":"/* @name CLI static import fixture */\nthrow new Error('must never execute during import');"
    });
    let response = cli.send(&request);
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["receipt"]["state"], "accepted");
    let mut cursor = 0;
    let finished = cli.wait_operation(&mut cursor, "cli-static-import");
    assert_eq!(finished["success"], true);
    let status = cli.status();
    assert!(status["error"].is_null());
    assert_eq!(status["sources"].as_array().unwrap().len(), 1);
    assert_eq!(status["sources"][0]["approved"], false);
    assert_eq!(status["sources"][0]["enabled"], false);
    let replay = cli.send(&request);
    assert_eq!(replay["ok"], true);
    assert_eq!(replay["result"]["receipt"]["state"], "completed");
    assert_eq!(
        replay["result"]["receipt"]["operation_id"],
        "cli-static-import"
    );
    assert_eq!(cli.status()["sources"].as_array().unwrap().len(), 1);
    let mut conflicting = request;
    conflicting["script"] = json!("/* @name Different bytes for the same request */\n0;");
    assert_eq!(cli.send(&conflicting)["ok"], false);
    assert_eq!(cli.status()["sources"].as_array().unwrap().len(), 1);
    cli.finish();
}

struct DelayedInitializer {
    url: String,
    started: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DelayedInitializer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/init", listener.local_addr().unwrap());
        let (started_tx, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(12);
            let mut connection = loop {
                match listener.accept() {
                    Ok((connection, _)) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if released.try_recv().is_ok() || Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            };
            connection.set_nonblocking(false).unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match connection.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => request.extend_from_slice(&buffer[..count]),
                }
                if request.len() > 8192 {
                    return;
                }
            }
            let _ = started_tx.send(());
            let _ = released.recv_timeout(Duration::from_secs(12));
            let _ = connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });
        Self {
            url,
            started,
            release,
            thread: Some(thread),
        }
    }

    fn script(&self) -> String {
        r#"/* @name CLI delayed initialization fixture */
lx.on(lx.EVENT_NAMES.request, async () => 'http://127.0.0.1:9/original.wav');
lx.request('__URL__', {method:'get',timeout:60000}, (error) => {
  if (error) throw new Error('Original delayed initialization failed');
  lx.send(lx.EVENT_NAMES.inited, {sources:{local:{type:'music',actions:['musicUrl'],qualitys:[]}}});
});"#
            .replace("__URL__", &self.url)
    }
}

impl Drop for DelayedInitializer {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn json_lines_accepts_cancel_on_same_connection_while_initializer_is_pending() {
    let mut cli = CliSession::start();
    let server = DelayedInitializer::new();
    let response = cli.send(&json!({"request_id":"import-slow","command":"import_source_text","script":server.script()}));
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["receipt"]["state"], "accepted");
    let mut cursor = 0;
    assert_eq!(
        cli.wait_operation(&mut cursor, "import-slow")["success"],
        true
    );
    let id = cli.status()["sources"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let accepted = cli.send(&json!({"request_id":"approve-slow","origin":"cli-test","command":"approve_source","id":id}));
    assert_eq!(accepted["ok"], true);
    assert_eq!(accepted["result"]["receipt"]["state"], "accepted");
    server
        .started
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(cli.status()["busy"], true);
    let cancelled = cli.send(&json!({"request_id":"cancel-slow","command":"cancel_operation","operation_id":"approve-slow"}));
    assert_eq!(cancelled["ok"], true);
    let finished = cli.wait_operation(&mut cursor, "approve-slow");
    assert_eq!(finished["success"], false);
    assert_eq!(finished["error"]["kind"], "cancelled");
    let status = cli.status();
    assert_eq!(status["busy"], false);
    assert!(
        status["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["enabled"] == false)
    );
    server.release.send(()).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(
        cli.status()["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["enabled"] == false)
    );
    cli.finish();
    let repository = Repository::open(cli.directory.path()).unwrap();
    repository.integrity_check().unwrap();
}

#[test]
fn cli_backup_export_queries_actual_receipt_and_recovers_conflict_before_eof() {
    use lx_gpui_player::backup::{BackupEncoding, decode_lx};
    let mut cli = CliSession::start();
    let directory = cli.directory.path().join("导出");
    fs::create_dir(&directory).unwrap();
    let capabilities = cli.send(&json!({"query":"capabilities"}));
    assert!(
        capabilities["result"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "export_backup")
    );
    let accepted=cli.send(&json!({"request_id":"cli-export","command":"export_backup","directory":directory,"basename":"settings.lxmc","format":{"kind":"lx_settings"},"encoding":"gzip"}));
    assert_eq!(accepted["ok"], true, "{accepted}");
    let mut cursor = 0;
    assert_eq!(
        cli.wait_operation(&mut cursor, "cli-export")["success"],
        true
    );
    let response = cli.send(&json!({"query":"backup_export","operation_id":"cli-export"}));
    assert_eq!(response["ok"], true);
    let view = &response["result"]["data"];
    assert_eq!(view["state"], "published");
    let path = view["receipt"]["path"].as_str().unwrap();
    let bytes = fs::read(path).unwrap();
    assert_eq!(
        view["receipt"]["bytes"].as_u64().unwrap(),
        bytes.len() as u64
    );
    assert!(
        decode_lx(&bytes, BackupEncoding::Auto)
            .unwrap()
            .settings
            .is_some()
    );
    fs::write(directory.join("foreign.json"), b"user original").unwrap();
    assert_eq!(cli.send(&json!({"request_id":"cli-conflict","command":"export_backup","directory":directory,"basename":"foreign.json","format":{"kind":"native"}}))["ok"],true);
    assert_eq!(
        cli.wait_operation(&mut cursor, "cli-conflict")["success"],
        false
    );
    let failed = cli.send(&json!({"query":"backup_export","operation_id":"cli-conflict"}));
    assert_eq!(failed["result"]["data"]["owned_part"]["published"], false);
    assert_eq!(cli.send(&json!({"request_id":"cli-clean","command":"cleanup_backup_export","export_operation_id":"cli-conflict","directory":directory}))["ok"],true);
    assert_eq!(
        cli.wait_operation(&mut cursor, "cli-clean")["success"],
        true
    );
    assert_eq!(
        fs::read(directory.join("foreign.json")).unwrap(),
        b"user original"
    );
    let before = Instant::now();
    cli.finish();
    assert!(before.elapsed() < Duration::from_secs(5));
}

#[test]
fn cli_sync_listener_is_explicit_redacts_pairing_on_query_and_closes_before_eof() {
    let mut cli = CliSession::start();
    let disabled = cli.send(&json!({"query":"sync"}));
    assert_eq!(disabled["result"]["state"], "disabled");
    assert_eq!(
        cli.send(&json!({"query":"sync_peers"}))["result"],
        json!([])
    );
    let rejected=cli.send(&json!({"service":"sync_server_start","query":"sync","config":{"server_id":"cli-sync","server_name":"CLI sync","pairing_epoch":5}}));
    assert_eq!(rejected["ok"], false);
    assert_eq!(
        cli.send(&json!({"query":"sync"}))["result"]["state"],
        "disabled"
    );
    let started=cli.send(&json!({"service":"sync_server_start","config":{"server_id":"cli-sync","server_name":"CLI sync","pairing_epoch":5}}));
    assert_eq!(started["ok"], true, "{started}");
    let code = started["result"]["pairing_code"].as_str().unwrap();
    assert_eq!(code.len(), 6);
    let address = started["result"]["address"].as_str().unwrap().to_owned();
    let view = cli.send(&json!({"query":"sync"}));
    assert_eq!(view["result"]["state"], "listening");
    assert!(view["result"].get("pairing_code").is_none());
    assert!(!view.to_string().contains(code));
    let before = Instant::now();
    cli.finish();
    assert!(before.elapsed() < Duration::from_secs(5));
    drop(TcpListener::bind(address).unwrap());
    let repo = Repository::open(cli.directory.path()).unwrap();
    assert!(!repo.sync_profile_state().unwrap().pairing_enabled);
    repo.integrity_check().unwrap();
}

#[test]
fn cli_physical_snapshot_queries_start_empty_without_destination_authority() {
    let mut cli = CliSession::start();
    let capabilities = cli.send(&json!({"query":"capabilities"}));
    for name in ["export_physical_snapshot", "recover_physical_snapshot"] {
        assert!(
            capabilities["result"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command == name)
        );
    }
    let view = cli.send(&json!({"query":"physical_snapshots"}));
    assert_eq!(view["ok"], true, "{view}");
    assert_eq!(view["result"]["data"], json!([]));
    let absent = cli.send(&json!({"query":"physical_snapshot","operation_id":"unselected"}));
    assert_eq!(absent["ok"], true);
    assert!(absent["result"]["data"].is_null());
    assert!(cli.finish().success());
}

#[test]
fn json_lines_reads_default_system_control_and_actual_audio_projection_queries() {
    let mut cli = CliSession::start();
    let capabilities = cli.send(&json!({"query":"capabilities"}));
    for query in [
        "system_control_settings",
        "system_control_state",
        "audio_supply",
        "audio_output_render_timing",
    ] {
        assert!(
            capabilities["result"]["queries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == query)
        );
        let response = cli.send(&json!({"query":query}));
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["result"]["kind"], query);
    }
    let settings = cli.send(&json!({"query":"system_control_settings"}));
    assert_eq!(settings["result"]["data"]["enabled"], false);
    let state = cli.send(&json!({"query":"system_control_state"}));
    let view = &state["result"]["data"];
    assert_eq!(view["native_ready"], false);
    assert!(view["owner_epoch"].is_null());
    assert_eq!(view["registered_hotkeys"], 0);
    assert_eq!(view["media_session_active"], false);
    assert_eq!(view["power_request_active"], false);
    assert!(cli.finish().success());
}

#[test]
fn cli_tag_protocol_advertises_routes_and_rejects_incomplete_consent_before_submission() {
    let mut cli = CliSession::start();
    let capabilities = cli.send(&json!({"query":"capabilities"}));
    for name in [
        "read_local_tags",
        "approve_tag_write",
        "revoke_tag_write",
        "approve_tag_recovery",
        "edit_local_tags",
        "recover_tag_edit",
        "cancel_tag_edit",
    ] {
        assert!(
            capabilities["result"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == name),
            "Missing CLI tag command {name}"
        );
    }
    assert_eq!(capabilities["result"]["complete_lx_backend"], false);
    assert_eq!(capabilities["result"]["max_command_bytes"], 2 * 1024 * 1024);
    for request in [
        json!({"query":"local_tags","operation_id":"unselected-read"}),
        json!({"query":"tag_write_approval","approval_id":"unselected-approval"}),
        json!({"query":"tag_edit","operation_id":"unselected-tag"}),
        json!({"query":"tag_edits"}),
    ] {
        let name = request["query"].as_str().unwrap();
        assert!(
            capabilities["result"]["queries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == name)
        );
        let view = cli.send(&request);
        assert_eq!(view["ok"], true, "{view}");
        assert_eq!(view["result"]["kind"], name);
        assert_eq!(
            view["result"]["data"],
            if name == "tag_edits" {
                json!([])
            } else {
                Value::Null
            }
        );
    }
    for request in [
        json!({"request_id":"bad-tag-read","command":"read_local_tags","asset_id":"unselected-asset","tag_operation_id":"unselected-tag","path":"unapproved-path.flac"}),
        json!({"request_id":"bad-tag-approve","command":"approve_tag_write","approval_id":"unselected-approval"}),
        json!({"request_id":"bad-tag-recovery-approve","command":"approve_tag_recovery","approval_id":"unselected-recovery","tag_operation_id":"unselected-tag","selection":{}}),
        json!({"request_id":"bad-tag-revoke","command":"revoke_tag_write","approval_id":"unselected-approval","expected_approval_epoch":0}),
        json!({"request_id":"bad-tag-edit","command":"edit_local_tags","approval_id":"unselected-approval","expected_approval_epoch":1,"budget":{"original_bytes":1,"journal_bytes":1,"backup_bytes":1,"candidate_bytes":1},"patch":{"title":{"action":"set","value":""}}}),
        json!({"request_id":"bad-tag-recover","command":"recover_tag_edit","tag_operation_id":"unselected-tag","action":"retire_artifacts","approval_id":"unpaired-approval"}),
        json!({"request_id":"bad-tag-cancel","command":"cancel_tag_edit"}),
    ] {
        let rejected = cli.send(&request);
        assert_eq!(
            rejected["ok"], false,
            "Incomplete tag consent must be rejected: {rejected}"
        );
    }
    assert_eq!(
        cli.send(&json!({"query":"tag_edits"}))["result"]["data"],
        json!([])
    );
    assert_eq!(
        cli.send(&json!({"query":"local_tags","operation_id":"unknown","path":"unapproved"}))["ok"],
        false
    );
    assert!(cli.finish().success());
}

#[test]
fn cli_effects_protocol_rejects_invalid_consent_and_identifiers_without_native_allocation() {
    let mut cli = CliSession::start();
    cli.status();
    let capabilities = cli.send(&json!({"query":"capabilities"}));
    for name in [
        "set_effects",
        "reset_effects",
        "import_impulse_resource",
        "recover_impulse_import",
        "recover_impulse_namespace",
        "abort_impulse_import",
        "retire_impulse_resource",
        "recover_impulse_retirement",
        "cancel_effects_operation",
    ] {
        assert!(
            capabilities["result"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == name),
            "Missing CLI effect command {name}"
        );
    }
    let selected = cli.directory.path().join("unapproved.wav");
    for request in [
        json!({"request_id":"effect-consent","command":"import_impulse_resource","path":selected,"file_read_approved":false}),
        json!({"request_id":"effect-unknown-field","command":"reset_effects","path":selected}),
        json!({"request_id":"effect-invalid-gain","command":"set_effects","settings":{"convolution":{"main_gain":6.0}}}),
        json!({"request_id":"effect-path-id","command":"retire_impulse_resource","resource_id":{"kind":"imported","id":"../owner.lock"}}),
        json!({"request_id":"effect-invalid-token","command":"recover_impulse_import","token":"../owner.lock"}),
        json!({"request_id":"namespace-no-consent","command":"recover_impulse_namespace","selection":{"directory_path":selected,"directory_read_approved":true,"directory_write_approved":false}}),
        json!({"request_id":"namespace-no-owner-consent","command":"recover_impulse_namespace","selection":{"directory_path":selected,"directory_read_approved":true,"directory_write_approved":true,"owner":{"path":selected,"file_read_approved":true,"file_write_approved":false}}}),
        json!({"request_id":"namespace-json-not-grant","command":"recover_impulse_namespace","selection":{"directory_path":selected,"directory_read_approved":true,"directory_write_approved":true},"creation":{"namespace":"0".repeat(32)}}),
        json!({"request_id":"namespace-unknown-owner-field","command":"recover_impulse_namespace","selection":{"directory_path":selected,"directory_read_approved":true,"directory_write_approved":true,"owner":{"path":selected,"file_read_approved":true,"file_write_approved":true,"identity":"serialized-id"}}}),
        json!({"request_id":"namespace-invalid-timeout","command":"recover_impulse_namespace","selection":{"directory_path":selected,"directory_read_approved":true,"directory_write_approved":true},"timeout_ms":0}),
    ] {
        let response = cli.send(&request);
        assert_eq!(response["ok"], false, "{response}");
    }
    assert!(!cli.directory.path().join(".lx-impulse-vault").exists());
    let inventory = cli.send(&json!({"query":"impulse_resources"}));
    assert_eq!(inventory["ok"], true, "{inventory}");
    assert_eq!(inventory["result"]["data"]["resources"], json!([]));
    assert_eq!(inventory["result"]["data"]["imports"], json!([]));
    assert_eq!(
        cli.send(&json!({"query":"effects_operations"}))["result"]["data"],
        json!([])
    );
    assert!(cli.finish().success());
}

#[cfg(windows)]
#[test]
fn cli_cold_namespace_recovery_uses_fresh_actual_selections_and_durable_current_intent() {
    use lx_gpui_player::{
        backend::EffectsPreferences,
        library::{authority::RootAuthority, scan::ScanContext},
        repository::{ImpulseFileIdentity, ImpulseVaultCreationFact, ImpulseVaultCreationPhase},
        store::ProjectStore,
    };
    for with_owner in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        let store = ProjectStore::open(profile.clone()).unwrap();
        let context = ScanContext::new(Duration::from_secs(30));
        let profile_authority = RootAuthority::open(&profile, false, &context).unwrap();
        let actual_profile = profile_authority.draft().anchor;
        let convert =
            |identity: lx_gpui_player::domain::library::FileIdentity| ImpulseFileIdentity {
                device: u64::from_str_radix(&identity.volume, 16).unwrap(),
                object: identity.file_id,
            };
        let namespace_token = if with_owner {
            "a".repeat(32)
        } else {
            "b".repeat(32)
        };
        let fact = ImpulseVaultCreationFact {
            namespace: namespace_token.clone(),
            profile_identity: convert(actual_profile),
            directory_identity: None,
            owner_identity: None,
            reserved_bytes: 48,
            phase: ImpulseVaultCreationPhase::Intended,
        };
        let mut data = store.load().unwrap();
        data.effects = Some(EffectsPreferences {
            creation: Some(fact.clone()),
            ..Default::default()
        });
        store.save(&data).unwrap();
        // This fixture retains a current creation obligation, then allocates
        // actual objects without a FID ACK. The public command must obtain all
        // write authority again from the fresh selected descriptors.
        let namespace = profile.join(".lx-impulse-vault");
        fs::create_dir(&namespace).unwrap();
        let owner = namespace.join("owner.lock");
        if with_owner {
            fs::write(&owner, []).unwrap();
        }
        let actual_directory = convert(
            RootAuthority::open(&namespace, false, &context)
                .unwrap()
                .draft()
                .anchor,
        );
        let foreign = temp.path().join("foreign.lock");
        fs::write(&foreign, b"keep the foreign file").unwrap();
        let mut selection = json!({"directory_path":namespace,"directory_read_approved":true,"directory_write_approved":true});
        if with_owner {
            selection["owner"] =
                json!({"path":foreign,"file_read_approved":true,"file_write_approved":true});
            let denied = run_secondary(&profile, &["--command".into(), json!({"request_id":"cli-foreign-owner","command":"recover_impulse_namespace","selection":selection}).to_string()]);
            let denied: Value = serde_json::from_slice(&denied.stdout).unwrap();
            assert_eq!(denied["ok"], false, "{denied}");
            assert_eq!(denied["result"]["completed"], true, "{denied}");
            assert_eq!(denied["result"]["success"], false, "{denied}");
            assert!(!denied["result"]["error"].is_null());
            assert_eq!(fs::read(&foreign).unwrap(), b"keep the foreign file");
            assert_eq!(fs::metadata(&owner).unwrap().len(), 0);
            assert_eq!(
                store.load().unwrap().effects.unwrap().creation.as_ref(),
                Some(&fact)
            );
            selection["owner"] =
                json!({"path":owner,"file_read_approved":true,"file_write_approved":true});
        }
        let completed = run_secondary(&profile, &["--command".into(), json!({"request_id":"cli-recover-namespace","command":"recover_impulse_namespace","selection":selection}).to_string()]);
        assert!(
            completed.status.success(),
            "{}",
            String::from_utf8_lossy(&completed.stderr)
        );
        let completed: Value = serde_json::from_slice(&completed.stdout).unwrap();
        assert_eq!(completed["ok"], true, "{completed}");
        assert_eq!(completed["result"]["completed"], true, "{completed}");
        assert_eq!(completed["result"]["success"], true, "{completed}");
        let durable = store.load().unwrap().effects.unwrap();
        assert!(durable.creation.is_none());
        let receipt = durable.vault.unwrap();
        assert_eq!(receipt.namespace, namespace_token);
        assert_eq!(receipt.directory_identity, actual_directory);
        assert_eq!(
            fs::read(&owner).unwrap(),
            [b"LXIMPULSEVAULT01".as_slice(), namespace_token.as_bytes()].concat()
        );
        let query = run_secondary(
            &profile,
            &[
                "--command".into(),
                json!({"query":"impulse_resources"}).to_string(),
            ],
        );
        assert!(query.status.success());
        let query: Value = serde_json::from_slice(&query.stdout).unwrap();
        assert_eq!(
            query["result"]["data"]["vault"]["namespace"],
            namespace_token
        );
        assert_eq!(query["result"]["data"]["resources"], json!([]));
        assert_eq!(query["result"]["data"]["imports"], json!([]));
    }
}

#[cfg(windows)]
#[test]
fn cli_imported_impulse_survives_source_removal_and_restart_then_retires_actual_body() {
    let mut cli = CliSession::start();
    let selected = cli.directory.path().join("selected.wav");
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&40_u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&8000_u32.to_le_bytes());
    wav.extend_from_slice(&16000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&4_u32.to_le_bytes());
    wav.extend_from_slice(&16384_i16.to_le_bytes());
    wav.extend_from_slice(&0_i16.to_le_bytes());
    fs::write(&selected, wav).unwrap();
    let mut cursor = cli.send(&json!({"query":"event_cursor"}))["result"]["cursor"]
        .as_u64()
        .unwrap();
    let imported = cli.send(&json!({"request_id":"cli-effect-import","command":"import_impulse_resource","path":selected,"file_read_approved":true}));
    assert_eq!(imported["ok"], true, "{imported}");
    let operation = imported["result"]["receipt"]["operation_id"]
        .as_str()
        .unwrap();
    let terminal = cli.wait_operation(&mut cursor, operation);
    assert_eq!(terminal["success"], true, "{terminal}");
    let inventory = cli.send(&json!({"query":"impulse_resources"}));
    let resource = inventory["result"]["data"]["resources"][0].clone();
    assert_eq!(
        inventory["result"]["data"]["resources"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(inventory["result"]["data"]["imports"], json!([]));
    let effects = cli.send(&json!({"query":"effects_state"}));
    assert_eq!(effects["result"]["data"]["audio"]["applied_sequence"], 0);
    fs::remove_file(&selected).unwrap();
    assert!(cli.finish().success());
    let restarted = run_secondary(
        cli.directory.path(),
        &[
            "--command".into(),
            json!({"query":"impulse_resources"}).to_string(),
        ],
    );
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let restarted: Value = serde_json::from_slice(&restarted.stdout).unwrap();
    assert_eq!(
        restarted["result"]["data"]["resources"],
        json!([resource.clone()])
    );
    let retired = run_secondary(
        cli.directory.path(),
        &["--command".into(), json!({"request_id":"cli-effect-retire","command":"retire_impulse_resource","resource_id":resource["resource_id"]}).to_string()],
    );
    assert!(
        retired.status.success(),
        "{}",
        String::from_utf8_lossy(&retired.stderr)
    );
    let retired: Value = serde_json::from_slice(&retired.stdout).unwrap();
    assert_eq!(retired["result"]["completed"], true, "{retired}");
    assert_eq!(retired["result"]["success"], true, "{retired}");
    let entries = fs::read_dir(cli.directory.path().join(".lx-impulse-vault"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("owner.lock")]);
}

#[test]
fn cli_bundled_presets_apply_across_actual_restart_without_a_private_vault() {
    let mut cli = CliSession::start();
    let queried = cli.send(&json!({"query":"bundled_impulse_presets"}));
    assert_eq!(queried["ok"], true, "{queried}");
    assert_eq!(queried["result"]["kind"], "bundled_impulse_presets");
    let presets = queried["result"]["data"].as_array().unwrap();
    assert_eq!(presets.len(), 13);
    let ids = presets
        .iter()
        .map(|preset| preset["id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 13);
    assert!(
        presets
            .iter()
            .all(|preset| preset["resource_id"]["kind"] == "bundled")
    );
    assert_eq!(
        cli.send(&json!({"query":"bundled_impulse_presets","path":"unexpected"}))["ok"],
        false
    );
    assert_eq!(cli.send(&json!({"request_id":"unknown-bundled","command":"apply_bundled_impulse_preset","id":"unshipped"}))["ok"], false);
    assert_eq!(cli.send(&json!({"request_id":"bundle-extra","command":"apply_bundled_impulse_preset","id":"telephone","file_read_approved":true}))["ok"], false);
    let spatial = json!({"enabled":false,"radius":0.37,"speed":25});
    assert_eq!(cli.send(&json!({"request_id":"bundle-spatial","command":"set_spatial_settings","settings":spatial}))["ok"], true);
    assert!(!cli.directory.path().join(".lx-impulse-vault").exists());
    assert!(cli.finish().success());
    let profile = cli.directory.path();
    let alias = run_secondary(profile, &["effects-presets".into()]);
    assert!(alias.status.success());
    let alias: Value = serde_json::from_slice(&alias.stdout).unwrap();
    assert_eq!(alias["result"]["data"].as_array().unwrap().len(), 13);
    // Each command starts a real CLI process. The second process restores the
    // previous selected impulse before admitting a different preset.
    for id in ["telephone", "bright_hall"] {
        let applied = run_secondary(profile, &["apply-effect-preset".into(), id.into()]);
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let applied: Value = serde_json::from_slice(&applied.stdout).unwrap();
        assert_eq!(applied["ok"], true, "{applied}");
        assert_eq!(applied["result"]["completed"], true, "{applied}");
        assert_eq!(applied["result"]["success"], true, "{applied}");
        let state = run_secondary(
            profile,
            &[
                "--command".into(),
                json!({"query":"effects_settings"}).to_string(),
            ],
        );
        assert!(state.status.success());
        let state: Value = serde_json::from_slice(&state.stdout).unwrap();
        assert_eq!(
            state["result"]["data"]["convolution"]["impulse"],
            json!({"kind":"bundled","id":id})
        );
        assert_eq!(state["result"]["data"]["spatial"], spatial);
        assert!(!profile.join(".lx-impulse-vault").exists());
    }
    let reset = run_secondary(
        profile,
        &[
            "--command".into(),
            json!({"request_id":"bundle-reset","command":"reset_effects"}).to_string(),
        ],
    );
    assert!(reset.status.success());
    let reset: Value = serde_json::from_slice(&reset.stdout).unwrap();
    assert_eq!(reset["ok"], true, "{reset}");
    assert!(!profile.join(".lx-impulse-vault").exists());
}

fn run_secondary(directory: &std::path::Path, arguments: &[String]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lx-native-cli"));
    command
        .args(["--audio", "memory", "--data-dir"])
        .arg(directory)
        .args(arguments)
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    command.output().unwrap()
}

#[test]
fn malformed_one_shot_deeplink_is_rejected_before_any_profile_directory_is_created() {
    let base = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let profile = base.path().join("unopened-profile");
    let result = run_secondary(
        &profile,
        &[
            "deeplink".into(),
            "https://unapproved.invalid/audio.mp3".into(),
        ],
    );
    assert!(!result.status.success());
    assert!(!profile.exists());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("unapproved.invalid"));
}

#[cfg(windows)]
#[test]
fn second_cli_deeplink_ack_preserves_explicit_ids_and_rejects_cas_without_another_writer() {
    let mut cli = CliSession::start_with_relay(true);
    cli.status(); // The live actor, writer guard and opt-in listener exist.
    let explicit = json!({"request_id":"cli-explicit-relay","origin":"caller","command":"dispatch_deep_link","url":"lxmusic://player/pause"});
    let arguments = vec!["--command".into(), explicit.to_string()];
    let first = run_secondary(cli.directory.path(), &arguments);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["ok"], true);
    assert_eq!(first["result"]["completed"], true);
    assert_eq!(first["result"]["relay"]["request_id"], "cli-explicit-relay");
    assert_eq!(
        first["result"]["relay"]["operation_id"],
        "cli-explicit-relay"
    );
    assert_eq!(first["result"]["relay"]["disposition"], "committed");
    let replay = run_secondary(cli.directory.path(), &arguments);
    assert!(replay.status.success());
    let replay: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay, first);
    let mut generated = Vec::new();
    for _ in 0..2 {
        let result = run_secondary(
            cli.directory.path(),
            &["deeplink".into(), "lxmusic://player/pause".into()],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let result: Value = serde_json::from_slice(&result.stdout).unwrap();
        let id = result["result"]["relay"]["request_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!id.is_empty() && id.len() <= 128);
        generated.push(id);
    }
    assert_ne!(generated[0], generated[1]);
    let revision = cli.status()["revision"].as_u64().unwrap();
    let cas = json!({"request_id":"cli-cas-must-not-forward","expected_revision":revision,"command":"dispatch_deep_link","url":"lxmusic://player/pause"});
    let refused = run_secondary(cli.directory.path(), &["--command".into(), cas.to_string()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("revision-constrained"));
    assert_eq!(cli.status()["revision"].as_u64(), Some(revision));
    assert!(Repository::open(cli.directory.path()).is_err());
    assert!(cli.finish().success());
    Repository::open(cli.directory.path()).unwrap();
}

#[cfg(windows)]
#[test]
fn cli_physical_snapshot_uses_actual_sqlite_receipt_and_preserves_collision_target() {
    use sha2::{Digest, Sha256};
    let mut cli = CliSession::start();
    let directory = cli.directory.path().join("SQLite 备份");
    fs::create_dir(&directory).unwrap();
    let mut cursor = 0;
    let accepted = cli.send(
        &json!({"request_id":"cli-physical","command":"export_physical_snapshot",
        "directory":directory,"basename":"profile.sqlite3","maximum_image_bytes":1024*1024}),
    );
    assert_eq!(accepted["ok"], true, "{accepted}");
    assert_eq!(
        cli.wait_operation(&mut cursor, "cli-physical")["success"],
        true
    );
    let response = cli.send(&json!({"query":"physical_snapshot","operation_id":"cli-physical"}));
    assert_eq!(response["ok"], true);
    let view = &response["result"]["data"];
    assert_eq!(view["state"], "published");
    assert_eq!(view["pending_recovery"], false);
    assert_eq!(view["has_live_owner"], false);
    let receipt = &view["receipt"];
    let path = receipt["path"].as_str().unwrap();
    let bytes = fs::read(path).unwrap();
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    assert_eq!(receipt["bytes"].as_u64(), Some(bytes.len() as u64));
    assert_eq!(receipt["sha256"], format!("{:x}", Sha256::digest(&bytes)));
    let snapshot =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        snapshot
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        snapshot
            .query_row(
                "SELECT count(*) FROM physical_snapshot_operations",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    drop(snapshot);
    let foreign = directory.join("existing.sqlite3");
    fs::write(&foreign, b"preserved existing backup").unwrap();
    assert_eq!(
        cli.send(
            &json!({"request_id":"cli-physical-conflict","command":"export_physical_snapshot",
        "directory":directory,"basename":"existing.sqlite3","maximum_image_bytes":1024*1024})
        )["ok"],
        true
    );
    assert_eq!(
        cli.wait_operation(&mut cursor, "cli-physical-conflict")["success"],
        false
    );
    let failed =
        cli.send(&json!({"query":"physical_snapshot","operation_id":"cli-physical-conflict"}));
    assert_eq!(failed["result"]["data"]["pending_recovery"], true);
    assert_eq!(cli.send(&json!({"request_id":"cli-physical-clean","command":"recover_physical_snapshot",
        "snapshot_operation_id":"cli-physical-conflict","directory":directory,"action":"cleanup_private"}))["ok"], true);
    assert_eq!(
        cli.wait_operation(&mut cursor, "cli-physical-clean")["success"],
        true
    );
    assert_eq!(fs::read(&foreign).unwrap(), b"preserved existing backup");
    let cleaned =
        cli.send(&json!({"query":"physical_snapshot","operation_id":"cli-physical-conflict"}));
    assert_eq!(cleaned["result"]["data"]["pending_recovery"], false);
    let before = Instant::now();
    assert!(cli.finish().success());
    assert!(before.elapsed() < Duration::from_secs(5));
}
