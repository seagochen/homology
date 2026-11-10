//! The real runner program, started as a child process the way the app
//! starts it on its own computer: greeting, asking, closing, and every way a
//! runner can break (debug builds misbehave on `ping` through
//! `HOMOLOGY_RUNNER_FAULT`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use homology_runner::protocol::{
    PROTOCOL_MISMATCH, RUNNER_EXITED, RUNNER_INVALID_OUTPUT, RUNNER_START_FAILED, RUNNER_TIMEOUT, UNKNOWN_OP,
};
use homology_runner::server::FAULT_ENV;
use homology_runner::{local_command, Client, PROTOCOL, VERSION};
use serde_json::Value;

const SECOND: Duration = Duration::from_secs(1);

/// The runner program under test: the one Cargo built, or
/// `HOMOLOGY_RUNNER_TEST_BIN` when these tests run on another computer than
/// the one that built them (a Windows build made on Linux).
fn program() -> PathBuf {
    std::env::var_os("HOMOLOGY_RUNNER_TEST_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_homology-runner")), PathBuf::from)
}

fn runner() -> Command {
    local_command(&program())
}

fn faulty(fault: &str) -> Command {
    let mut command = runner();
    command.env(FAULT_ENV, fault);
    command
}

/// Whether process `pid` is gone, waiting a little for it to go.
fn gone(client: &mut Client) -> bool {
    let deadline = Instant::now() + 5 * SECOND;
    while Instant::now() < deadline {
        if !client.running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn greets_answers_and_closes_cleanly() {
    let mut client = Client::start(runner(), 10 * SECOND).unwrap();
    assert_eq!(client.hello()["protocol"].as_u64(), Some(PROTOCOL));
    assert_eq!(client.hello()["version"].as_str(), Some(VERSION));
    assert!(client.hello()["ops"].as_array().unwrap().contains(&Value::from("ping")));
    for _ in 0..3 {
        assert_eq!(client.request("ping", Default::default(), 10 * SECOND).unwrap(), serde_json::json!({}));
    }
    assert!(client.next_event(Duration::from_millis(50)).unwrap().is_none());
    let status = client.close(10 * SECOND).unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn an_unknown_op_is_an_error_and_the_session_goes_on() {
    let mut client = Client::start(runner(), 10 * SECOND).unwrap();
    assert_eq!(client.request("train", Default::default(), 10 * SECOND).unwrap_err().code, UNKNOWN_OP);
    client.request("ping", Default::default(), 10 * SECOND).unwrap();
}

#[test]
fn a_program_that_cannot_start_is_reported() {
    let command = local_command(Path::new("/no/such/homology-runner"));
    assert_eq!(Client::start(command, 10 * SECOND).err().unwrap().code, RUNNER_START_FAILED);
}

#[test]
fn a_runner_that_ends_is_reported_with_its_status() {
    let mut client = Client::start(faulty("exit"), 10 * SECOND).unwrap();
    let error = client.request("ping", Default::default(), 10 * SECOND).unwrap_err();
    assert_eq!(error.code, RUNNER_EXITED, "{error}");
    assert!(error.message.contains('3'), "the exit status: {error}");
    assert!(gone(&mut client));
    assert_eq!(
        client.request("ping", Default::default(), SECOND).unwrap_err().code,
        RUNNER_EXITED,
        "finished for good"
    );
}

#[test]
fn a_runner_that_writes_garbage_is_killed() {
    let mut client = Client::start(faulty("garbage"), 10 * SECOND).unwrap();
    let error = client.request("ping", Default::default(), 10 * SECOND).unwrap_err();
    assert_eq!(error.code, RUNNER_INVALID_OUTPUT, "{error}");
    assert!(error.message.contains("this is not a message"));
    assert!(gone(&mut client), "no runner is left behind");
}

#[test]
fn a_runner_that_does_not_answer_is_killed() {
    let mut client = Client::start(faulty("silent"), 10 * SECOND).unwrap();
    let started = Instant::now();
    let error = client.request("ping", Default::default(), Duration::from_millis(300)).unwrap_err();
    assert_eq!(error.code, RUNNER_TIMEOUT, "{error}");
    assert!(started.elapsed() < 5 * SECOND);
    assert!(gone(&mut client), "no runner is left behind");
}

#[test]
fn a_runner_of_another_protocol_is_refused() {
    let error = Client::start(faulty("protocol"), 10 * SECOND).err().unwrap();
    assert_eq!(error.code, PROTOCOL_MISMATCH, "{error}");
}

#[test]
fn dropping_the_client_ends_the_runner() {
    let client = Client::start(faulty("silent"), 10 * SECOND).unwrap();
    let pid = client.id();
    drop(client);
    // The client killed and waited for it, so not even its entry is left.
    #[cfg(target_os = "linux")]
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "runner {pid} is still there");
    assert!(pid > 0);
}

#[test]
fn version_and_usage() {
    let output = Command::new(program()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(VERSION));
    let output = Command::new(program()).output().unwrap();
    assert_eq!(output.status.code(), Some(64));
}

/// An editor's files through the real program: read with a hash,
/// saved over it, refused over a stale one.
#[test]
fn files_are_read_and_saved_over_what_was_opened() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("train.py"), "x = 1\n").unwrap();
    let root = dir.path().to_string_lossy().to_string();
    let mut client = Client::start(runner(), 10 * SECOND).unwrap();
    assert!(client.hello()["ops"].as_array().unwrap().contains(&Value::from("fs.write")));
    let ask = |client: &mut Client, op: &str, params: Value| {
        client.request(op, params.as_object().unwrap().clone(), 10 * SECOND)
    };
    let listed = ask(&mut client, "fs.list", serde_json::json!({"root": root})).unwrap();
    assert_eq!(listed["files"], serde_json::json!([{"path": "train.py", "size": 6}]));
    let opened = ask(&mut client, "fs.read", serde_json::json!({"root": root, "path": "train.py"})).unwrap();
    ask(
        &mut client,
        "fs.write",
        serde_json::json!({"root": root, "path": "train.py", "text": "x = 2\n", "sha256": opened["sha256"]}),
    )
    .unwrap();
    let stale = ask(
        &mut client,
        "fs.write",
        serde_json::json!({"root": root, "path": "train.py", "text": "x = 3\n", "sha256": opened["sha256"]}),
    )
    .unwrap_err();
    assert_eq!(stale.code, homology_runner::fs::CODE_CONFLICT);
    assert_eq!(std::fs::read_to_string(dir.path().join("train.py")).unwrap(), "x = 2\n");
    client.close(10 * SECOND).unwrap();
}
