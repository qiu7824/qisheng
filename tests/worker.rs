//! Executes the real player binary's private worker entry point.
//! The fixtures are original synthetic scripts, with no external source access.

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
}

impl WorkerProcess {
    fn spawn() -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lx-native-cli"));
        command
            .arg("--source-worker")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = mpsc::sync_channel(1);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            responses,
        }
    }

    fn exchange(&mut self, value: Value) -> Value {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).unwrap();
        self.stdin.flush().unwrap();
        self.responses.recv_timeout(Duration::from_secs(8)).expect(
            "Real player worker must return bounded JSON IPC without starting the GPUI window",
        )
    }

    fn initialize(&mut self, script: &str) -> Value {
        self.exchange(json!({"op":"init", "script": script}))
    }

    fn validate(&mut self, script: &str) -> Value {
        self.exchange(json!({"op":"validate", "script": script}))
    }

    fn request(&mut self) -> Value {
        self.exchange(json!({"op":"request","request":{
            "source":"kw", "action":"musicUrl", "info":{
                "type":"128k", "musicInfo":{
                    "name":"Synthetic fixture", "singer":"Test oscillator", "source":"kw",
                    "songmid":"fixture-1", "interval":"00:01", "albumName":"Original fixtures",
                    "img":"", "typeUrl":{}, "albumId":"", "types":[], "_types":{}
                }
            }
        }}))
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const COUNTER_SOURCE: &str = r#"/*
 * @name Original worker fixture
 * @author LX GPUI Player contributors
 */
(() => {
  let calls = 0;
  lx.on(lx.EVENT_NAMES.request, async ({source, action, info}) => {
    if (source !== 'kw' || action !== 'musicUrl' || info.musicInfo.songmid !== 'fixture-1')
      throw new Error('Legacy musicUrl request contract mismatch');
    return 'http://127.0.0.1:9/' + (++calls) + '.wav';
  });
  lx.send(lx.EVENT_NAMES.inited, {
    sources: { kw: { type: 'music', actions: ['musicUrl'], qualitys: ['128k'] } }
  });
})();"#;

#[test]
fn static_validation_in_actual_binary_never_executes_or_initializes_script() {
    let mut worker = WorkerProcess::spawn();
    let script =
        "/* @name Static validation fixture */\nthrow new Error('Import must never execute this');";
    let validated = worker.validate(script);
    assert_eq!(validated["kind"], "validated");
    assert_eq!(validated["source"]["script"], script);
    assert_eq!(validated["source"]["approved"], false);
    assert_eq!(
        validated["source"]["metadata"]["name"],
        "Static validation fixtur..."
    );
    assert_eq!(worker.request()["kind"], "error");
    assert_eq!(
        worker.validate("/* @name Invalid */ const = ;")["kind"],
        "error"
    );
    assert_eq!(worker.initialize(COUNTER_SOURCE)["kind"], "initialized");
    assert_eq!(worker.request()["url"], "http://127.0.0.1:9/1.wav");
}

#[test]
fn actual_binary_preserves_closure_state_and_restarts_cleanly() {
    {
        let mut worker = WorkerProcess::spawn();
        let init = worker.initialize(COUNTER_SOURCE);
        assert_eq!(init["kind"], "initialized");
        assert_eq!(init["manifest"]["sources"]["kw"]["actions"][0], "musicUrl");
        assert_eq!(worker.request()["url"], "http://127.0.0.1:9/1.wav");
        assert_eq!(worker.request()["url"], "http://127.0.0.1:9/2.wav");
        assert_eq!(worker.initialize(COUNTER_SOURCE)["kind"], "error");
        assert_eq!(worker.request()["url"], "http://127.0.0.1:9/3.wav");
    }
    let mut restarted = WorkerProcess::spawn();
    assert_eq!(restarted.initialize(COUNTER_SOURCE)["kind"], "initialized");
    assert_eq!(restarted.request()["url"], "http://127.0.0.1:9/1.wav");
}

#[test]
fn invalid_initialization_and_requests_report_errors_then_recover() {
    let mut worker = WorkerProcess::spawn();
    assert_eq!(worker.request()["kind"], "error");
    let invalid = worker.initialize("/* @name Broken */ const = ;");
    assert_eq!(invalid["kind"], "error");
    assert!(
        invalid["message"]
            .as_str()
            .unwrap()
            .contains("initialization")
    );
    assert_eq!(worker.initialize(COUNTER_SOURCE)["kind"], "initialized");
    assert_eq!(worker.request()["url"], "http://127.0.0.1:9/1.wav");
}

#[test]
fn non_promise_handler_is_an_error_in_the_real_worker() {
    let mut worker = WorkerProcess::spawn();
    let synchronous =
        COUNTER_SOURCE.replace("async ({source, action, info})", "({source, action, info})");
    assert_eq!(worker.initialize(&synchronous)["kind"], "initialized");
    let response = worker.request();
    assert_eq!(response["kind"], "error");
    assert!(response["message"].as_str().unwrap().contains("Promise"));
}

#[test]
fn oversized_script_is_rejected_before_evaluation() {
    let mut worker = WorkerProcess::spawn();
    let oversized = format!("/* @name Oversized */{}", " ".repeat(1024 * 1024));
    let response = worker.initialize(&oversized);
    assert_eq!(response["kind"], "error");
    assert!(response["message"].as_str().unwrap().contains("size limit"));
}
