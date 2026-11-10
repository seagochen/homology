//! The check that no password or private key leaks (#1): a password login
//! and a login key's set-up run against a stand-in `ssh` that records all
//! it is given (its arguments, its environment, its stdin) and runs the
//! scripts in a stand-in host's home. Afterwards the password must appear
//! nowhere: not in what `ssh` saw, not in any file here or on the host, not
//! in an error. The private key must never reach the host. Unix: the
//! stand-ins are `python3` scripts.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use homology_ssh::askpass::Password;
use homology_ssh::{keys, ssh, App, Context, SshTarget};

/// The password the user typed: long and unlikely, so finding it anywhere
/// is a leak and not a coincidence.
const PASSWORD: &str = "pw-7f3c9e1a-Seagull!Lantern#42";

/// A password the host refuses.
const WRONG: &str = "pw-wrong-0b9d44c2-Harbor$Quiet";

/// The stand-in `ssh`. It records every run as one JSON line (argv,
/// environment, stdin), collects a password through `SSH_ASKPASS` as the
/// real one does and accepts it only when its sha256 is the one in
/// `$FAKE_DIR/accepted` (the password itself is never given to it), and
/// runs the script with `sh` in the host's home `$FAKE_DIR/host`.
const FAKE_SSH: &str = r#"#!/usr/bin/env python3
import hashlib, json, os, subprocess, sys
fake = os.environ["FAKE_DIR"]
stdin = sys.stdin.buffer.read()
with open(os.path.join(fake, "seen.jsonl"), "a") as seen:
    seen.write(json.dumps({"argv": sys.argv, "env": dict(os.environ), "stdin": stdin.decode("latin-1")}) + "\n")
if os.environ.get("SSH_ASKPASS"):
    answer = subprocess.run([os.environ["SSH_ASKPASS"], "password: "], capture_output=True).stdout
    with open(os.path.join(fake, "accepted")) as accepted:
        if hashlib.sha256(answer.rstrip(b"\n")).hexdigest() != accepted.read().strip():
            sys.stderr.write("user@host: Permission denied (publickey,password).\n")
            sys.exit(255)
script = sys.argv[-1]
host = os.path.join(fake, "host")
done = subprocess.run(["sh", "-c", script], input=stdin, capture_output=True, cwd=host, env={"HOME": host, "PATH": os.environ["PATH"]})
sys.stdout.buffer.write(done.stdout)
sys.stderr.buffer.write(done.stderr)
sys.exit(done.returncode)
"#;

/// The stand-in askpass program: it collects the password from the
/// channel named by `HOMOLOGY_ASKPASS` (`<socket> <token>`), as the
/// program's helper does with `askpass::client_main`.
const FAKE_ASKPASS: &str = r#"#!/usr/bin/env python3
import os, socket, sys
address, token = os.environ["HOMOLOGY_ASKPASS"].rsplit(" ", 1)
channel = socket.socket(socket.AF_UNIX)
channel.connect(address)
channel.sendall(token.encode() + b"\n")
reply = b""
while True:
    part = channel.recv(4096)
    if not part:
        break
    reply += part
sys.stdout.buffer.write(reply)
"#;

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn on_path(program: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|dir| dir.join(program).is_file())
}

/// Every file under `dir`, with its path.
fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                found.push((entry.path(), std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    found
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Fail naming the file when any file under `dir` holds `secret`.
fn assert_nowhere_in(dir: &Path, secret: &[u8], what: &str) {
    for (path, bytes) in files_under(dir) {
        assert!(!contains(&bytes, secret), "{what} found in {}", path.display());
    }
}

/// The sha256 of `bytes` in hex, computed by python as the stand-in does.
fn sha256_hex(bytes: &[u8]) -> String {
    let output = std::process::Command::new("python3")
        .args(["-c", "import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            std::io::Write::write_all(child.stdin.as_mut().unwrap(), bytes)?;
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn no_password_or_private_key_leaks() {
    if !on_path("python3") || !on_path("sh") {
        eprintln!("skipped: no python3 or sh");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let fake = root.path().join("fake");
    let host = fake.join("host");
    let keys_dir = root.path().join("keys");
    let sockets = root.path().join("tmp");
    for dir in [&fake, &host, &sockets] {
        std::fs::create_dir_all(dir).unwrap();
    }
    script(&fake.join("ssh"), FAKE_SSH);
    script(&fake.join("askpass"), FAKE_ASKPASS);
    std::fs::write(fake.join("accepted"), sha256_hex(PASSWORD.as_bytes())).unwrap();
    std::env::set_var("FAKE_DIR", &fake);
    std::env::set_var("HOMOLOGY_SSH", fake.join("ssh"));
    std::env::set_var("HOMOLOGY_ASKPASS_PROGRAM", fake.join("askpass"));
    // The askpass socket is made in the temporary folder: this test's own.
    std::env::set_var("TMPDIR", &sockets);

    let cx = Context::new(App::HOMOLOGY);
    cx.set_keys_dir(keys_dir.clone());
    let target = SshTarget::parse("user@host:/data").unwrap();

    // A password login the host takes, and one it refuses.
    let output = ssh::run_with_password(&cx, &target, "echo logged-in", &Password::new(PASSWORD)).unwrap();
    assert_eq!(output, b"logged-in\n");
    let refused = ssh::run_with_password(&cx, &target, "true", &Password::new(WRONG)).unwrap_err();
    assert_eq!(refused.code, "ssh_password_wrong", "{refused}");
    for shown in [refused.to_string(), format!("{refused:?}")] {
        assert!(!shown.contains(WRONG), "the refused password is in the error: {shown}");
    }
    assert!(!format!("{:?}", Password::new(PASSWORD)).contains(PASSWORD));

    // A login key set up with the password, when there is an ssh-keygen.
    let mut private_key = None;
    if on_path("ssh-keygen") {
        let refused = keys::set_up(&cx, &target, &Password::new(WRONG)).unwrap_err();
        assert!(!refused.to_string().contains(WRONG) && !format!("{refused:?}").contains(WRONG));
        let login = keys::set_up(&cx, &target, &Password::new(PASSWORD)).unwrap();
        let private = std::fs::read(keys_dir.join(&login.key)).unwrap();
        assert!(contains(&private, b"PRIVATE KEY"));
        let mode = std::fs::metadata(keys_dir.join(&login.key)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the private key is readable by others: {mode:o}");
        let authorized = std::fs::read_to_string(host.join(".ssh/authorized_keys")).unwrap();
        assert_eq!(authorized.lines().count(), 1, "only the accepted set-up left a key: {authorized}");
        private_key = Some(private);
    } else {
        eprintln!("login keys not checked: no ssh-keygen");
    }

    // What ssh was given, as it saw it: never the password, in any run.
    let seen = std::fs::read(fake.join("seen.jsonl")).unwrap();
    assert!(seen.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).count() >= 2);
    for secret in [PASSWORD, WRONG] {
        assert!(!contains(&seen, secret.as_bytes()), "ssh's arguments, environment or stdin hold a password");
    }
    // No file here or on the host holds a password; the askpass sockets are gone.
    for secret in [PASSWORD, WRONG] {
        assert_nowhere_in(root.path(), secret.as_bytes(), "a password");
    }
    assert!(std::fs::read_dir(&sockets).unwrap().next().is_none(), "an askpass socket was left behind");
    // The private key stays on this computer: neither ssh nor the host saw any of it.
    if let Some(private) = private_key {
        let body: Vec<u8> = private
            .split(|byte| *byte == b'\n')
            .find(|line| !line.is_empty() && !line.starts_with(b"-----"))
            .unwrap()
            .to_vec();
        assert!(!contains(&seen, &body), "ssh was given the private key");
        assert!(!contains(&seen, b"PRIVATE KEY"), "ssh was given a private key");
        assert_nowhere_in(&host, &body, "the private key");
        assert_nowhere_in(&host, b"PRIVATE KEY", "a private key");
    }
}
