//! A runner on an SSH host.
//!
//! One look at the host says its system, where this version's runner goes
//! and the hash of the one already there. A missing or different runner is
//! sent in the same kind of connection that checks it: written to a
//! temporary file, its sha256 compared with the package's, then made
//! executable and renamed into place; older versions are removed. Then
//! `ssh … serve` starts a session with it. A runner that turns out to speak
//! another protocol is replaced once.
//!
//! Everything a program leaves on a host for this is in its folder of the
//! host's cache, `$XDG_CACHE_HOME/<cache>/` (`~/.cache/…` when unset,
//! [`Program::cache`]): what can be had again, readable only by the user.

use std::process::Command;
use std::time::Duration;

use homology_runner::client::HELLO_TIMEOUT;
use homology_runner::protocol::{PROTOCOL_MISMATCH, RUNNER_EXITED};
use homology_runner::{Client, RunnerError, RunnerResult};
use homology_ssh::{shell_quote, Context, SshResult, SshTarget};

use crate::bundle::{self, Arch, Bundled};
use crate::{ssh_error, Program, RUNNER_UNSUPPORTED_HOST, RUNNER_UPLOAD_FAILED};

/// How a host is reached: running a script, running one with input, and
/// starting a session. Over SSH ([`Host::ssh`]); a local shell in tests.
pub struct Host<'a> {
    pub run: Run<'a>,
    pub run_with_input: RunWithInput<'a>,
    pub session: Session<'a>,
}

/// Run a POSIX shell script on the host; its stdout.
pub type Run<'a> = Box<dyn Fn(&str) -> SshResult<Vec<u8>> + 'a>;
/// [`Run`] with the bytes given on the script's stdin.
pub type RunWithInput<'a> = Box<dyn Fn(&str, &[u8]) -> SshResult<Vec<u8>> + 'a>;
/// The command that runs a script on the host with its stdio for the caller.
pub type Session<'a> = Box<dyn Fn(&str) -> SshResult<Command> + 'a>;

impl<'a> Host<'a> {
    /// The host `target` names (its path is not used), through the user's
    /// own SSH: their configuration and agent, and the key the program set
    /// up.
    pub fn ssh(cx: &'a Context, target: &'a SshTarget) -> Host<'a> {
        Host {
            run: Box::new(move |script| homology_ssh::run(cx, target, script)),
            run_with_input: Box::new(move |script, input| homology_ssh::run_with_input(cx, target, script, input)),
            session: Box::new(move |script| homology_ssh::session_command(cx, target, script)),
        }
    }
}

/// A runner in session on a host.
pub struct Deployed {
    pub client: Client,
    pub arch: Arch,
    /// The runner's directory on the host (absolute).
    pub dir: String,
    /// Whether this connection placed the runner.
    pub uploaded: bool,
}

/// What one look at the host found.
#[derive(Debug, PartialEq, Eq)]
struct Found {
    system: String,
    machine: String,
    dir: String,
    sha256: Option<String>,
}

/// Lines that set `r` (the runner directory) and `d` (this version's)
/// from the host's cache directory, and `hash` (a file's sha256 with
/// whichever tool the host has).
fn preamble(program: &Program) -> String {
    format!(
        r#"set -u
c="${{XDG_CACHE_HOME:-}}"
case "$c" in /*) ;; *) c="$HOME/.cache" ;; esac
r="$c/"{cache}
d="$r/bin/"{version}
hash() {{ (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1" 2>/dev/null) | cut -d' ' -f1; }}
"#,
        cache = shell_quote(program.cache),
        version = shell_quote(program.version)
    )
}

/// The script that asks a host what it is: `system`, `machine`, the runner's
/// cache `dir`, and the `sha256` of the runner there when there is one.
fn look_script(program: &Program) -> String {
    format!(
        r#"{}printf 'system %s\n' "$(uname -s)"
printf 'machine %s\n' "$(uname -m)"
printf 'dir %s\n' "$d"
if [ -f "$d/"{name} ]; then printf 'sha256 %s\n' "$(hash "$d/"{name})"; fi
"#,
        preamble(program),
        name = shell_quote(program.name)
    )
}

/// Place the runner arriving on stdin, if its hash is `expected`; say
/// `placed <hash>` or `mismatch <hash>`.
fn place_script(program: &Program, expected: &str) -> String {
    format!(
        r#"{}umask 077
mkdir -p "$r" && chmod 700 "$r" && mkdir -p "$d" || exit 1
t="$d/.upload.$$"
cat > "$t" || {{ rm -f "$t"; exit 1; }}
h="$(hash "$t")"
if [ "$h" != {expected} ]; then rm -f "$t"; printf 'mismatch %s\n' "$h"; exit 0; fi
chmod 700 "$t" && mv -f "$t" "$d/"{name} || {{ rm -f "$t"; exit 1; }}
for old in "$r"/bin/*; do [ "$old" = "$d" ] || rm -rf "$old"; done
printf 'placed %s\n' "$h"
"#,
        preamble(program),
        expected = shell_quote(expected),
        name = shell_quote(program.name)
    )
}

/// What [`look_script`] printed (`runner_upload_failed` when a line is missing);
/// a `sha256` that is not 64 characters is taken as none.
fn parse_look(output: &[u8]) -> RunnerResult<Found> {
    let text = String::from_utf8_lossy(output);
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name).and_then(|rest| rest.strip_prefix(' ')).map(str::to_string))
    };
    let missing = |name: &str| {
        RunnerError::new(RUNNER_UPLOAD_FAILED, format!("the host did not say its {name}: {}", text.trim()))
    };
    let sha256 = field("sha256").filter(|hash| hash.len() == 64);
    Ok(Found {
        system: field("system").ok_or_else(|| missing("system"))?,
        machine: field("machine").ok_or_else(|| missing("machine"))?,
        dir: field("dir").ok_or_else(|| missing("cache directory"))?,
        sha256,
    })
}

/// The runner the package has for what the host said it is.
fn runner_for(found: &Found, read: &dyn Fn(Arch) -> RunnerResult<Bundled>) -> RunnerResult<Bundled> {
    match (found.system.as_str(), Arch::from_uname(&found.machine)) {
        ("Linux", Some(arch)) => read(arch),
        (system, _) => Err(RunnerError::new(
            RUNNER_UNSUPPORTED_HOST,
            format!("the host runs {system} on {}; runners are for Linux on x86_64 and aarch64", found.machine),
        )),
    }
}

/// Send `runner` to the host and put it in its cache when it arrived whole
/// (`runner_upload_failed` when it did not, or the host did not confirm it).
fn place(host: &Host<'_>, program: &Program, runner: &Bundled) -> RunnerResult<()> {
    let output = (host.run_with_input)(&place_script(program, &runner.sha256), &runner.bytes).map_err(|error| {
        RunnerError::new(RUNNER_UPLOAD_FAILED, format!("could not place the runner: {}", error.message))
    })?;
    let text = String::from_utf8_lossy(&output);
    match text.lines().last().and_then(|line| line.split_once(' ')) {
        Some(("placed", hash)) if hash == runner.sha256 => Ok(()),
        Some(("mismatch", hash)) => Err(RunnerError::new(
            RUNNER_UPLOAD_FAILED,
            format!(
                "the runner arrived damaged (sha256 {} on the host, {} sent){}",
                if hash.is_empty() { "unknown" } else { hash },
                runner.sha256,
                if hash.is_empty() { "; the host has neither sha256sum nor shasum" } else { "" }
            ),
        )),
        _ => {
            Err(RunnerError::new(RUNNER_UPLOAD_FAILED, format!("the host did not confirm the runner: {}", text.trim())))
        }
    }
}

/// `program`'s runner in session on `host`, placed there first if it is
/// missing or differs from the package's.
pub fn connect(host: &Host<'_>, program: &Program) -> RunnerResult<Deployed> {
    connect_with(host, program, &|arch| bundle::linux(program, arch), HELLO_TIMEOUT)
}

/// [`connect`] with the package's runners read by `read` and `timeout` for
/// the greeting (tests).
pub fn connect_with(
    host: &Host<'_>,
    program: &Program,
    read: &dyn Fn(Arch) -> RunnerResult<Bundled>,
    timeout: Duration,
) -> RunnerResult<Deployed> {
    let found = parse_look(&(host.run)(&look_script(program)).map_err(ssh_error)?)?;
    let runner = runner_for(&found, read)?;
    let mut uploaded = false;
    if found.sha256.as_deref() != Some(runner.sha256.as_str()) {
        place(host, program, &runner)?;
        uploaded = true;
    }
    let serve = format!("exec {} serve", shell_quote(&format!("{}/{}", found.dir, program.name)));
    let start = || -> RunnerResult<Client> { Client::start((host.session)(&serve).map_err(ssh_error)?, timeout) };
    let client = match start() {
        Ok(client) => client,
        // The hash matched, so this cannot be a damaged runner; one of
        // another protocol, or one that cannot run, is placed again once.
        Err(error) if !uploaded && [PROTOCOL_MISMATCH, RUNNER_EXITED].contains(&error.code.as_str()) => {
            place(host, program, &runner)?;
            uploaded = true;
            start()?
        }
        Err(error) => return Err(error),
    };
    Ok(Deployed { client, arch: runner.arch, dir: found.dir, uploaded })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_host_says_is_read_and_a_partial_answer_is_an_error() {
        let found = parse_look(b"system Linux\nmachine x86_64\ndir /home/u/.cache/tool/runner/bin/1.0\nsha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").unwrap();
        assert_eq!((found.system.as_str(), found.machine.as_str()), ("Linux", "x86_64"));
        assert_eq!(found.sha256.as_deref().map(str::len), Some(64));
        let none = parse_look(b"system Linux\nmachine aarch64\ndir /d\nsha256 \n").unwrap();
        assert_eq!(none.sha256, None, "an empty hash (no tool) is no hash");
        assert_eq!(parse_look(b"machine x86_64\n").unwrap_err().code, RUNNER_UPLOAD_FAILED);
    }

    #[test]
    fn only_linux_on_two_architectures_gets_a_runner() {
        let read = |arch: Arch| Ok(Bundled { arch, bytes: Vec::new(), sha256: String::new() });
        let host = |system: &str, machine: &str| Found {
            system: system.into(),
            machine: machine.into(),
            dir: "/d".into(),
            sha256: None,
        };
        assert_eq!(runner_for(&host("Linux", "aarch64"), &read).unwrap().arch, Arch::Aarch64);
        for (system, machine) in [("Darwin", "arm64"), ("Linux", "armv7l"), ("FreeBSD", "amd64")] {
            assert_eq!(
                runner_for(&host(system, machine), &read).err().unwrap().code,
                RUNNER_UNSUPPORTED_HOST,
                "{system} {machine}"
            );
        }
    }

    /// A host that is this computer, its home in a temporary directory:
    /// the same scripts as over SSH, run by `sh`.
    #[cfg(unix)]
    mod on_this_computer {
        use super::*;
        use homology_ssh::SshError;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use std::process::Stdio;

        fn shell(home: &Path, xdg: Option<&Path>, script: &str) -> Command {
            let mut command = Command::new("sh");
            command.arg("-c").arg(script).env("HOME", home);
            match xdg {
                Some(dir) => command.env("XDG_CACHE_HOME", dir),
                None => command.env_remove("XDG_CACHE_HOME"),
            };
            command
        }

        fn finished(output: std::io::Result<std::process::Output>) -> SshResult<Vec<u8>> {
            let output = output.map_err(|error| SshError::new("ssh_failed", error.to_string()))?;
            if output.status.success() {
                Ok(output.stdout)
            } else {
                Err(SshError::new("remote_failed", String::from_utf8_lossy(&output.stderr).to_string()))
            }
        }

        /// `corrupt` damages what is sent, as a broken link would.
        fn host<'a>(home: &'a Path, xdg: Option<&'a Path>, corrupt: bool) -> Host<'a> {
            Host {
                run: Box::new(move |script| finished(shell(home, xdg, script).output())),
                run_with_input: Box::new(move |script, input| {
                    let mut child = shell(home, xdg, script)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    let mut sent = input.to_vec();
                    if corrupt {
                        sent[0] ^= 0xff;
                    }
                    child.stdin.take().unwrap().write_all(&sent).unwrap();
                    finished(child.wait_with_output())
                }),
                session: Box::new(move |script| Ok(shell(home, xdg, script))),
            }
        }

        const PLAIN: Program = Program::PLAIN;

        /// The runner Cargo built beside these tests, as the package's
        /// runner for either architecture.
        fn package() -> tempfile::TempDir {
            let built = homology_runner::testing::built("homology-runner").unwrap();
            let dir = tempfile::tempdir().unwrap();
            for arch in Arch::ALL {
                std::fs::copy(&built, dir.path().join(bundle::file_name(&PLAIN, arch))).unwrap();
            }
            dir
        }

        fn connect_to(host: &Host<'_>, package: &Path) -> RunnerResult<Deployed> {
            connect_with(host, &PLAIN, &|arch| bundle::read(&PLAIN, package, arch), Duration::from_secs(20))
        }

        fn mode(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        fn the_first_connection_places_the_runner_and_the_next_reuses_it() {
            let (home, package) = (tempfile::tempdir().unwrap(), package());
            let old = home.path().join(".cache/homology/runner/bin/0.0.1");
            std::fs::create_dir_all(&old).unwrap();
            let host = host(home.path(), None, false);
            let mut first = connect_to(&host, package.path()).unwrap();
            assert!(first.uploaded);
            let dir = home.path().join(".cache/homology/runner/bin").join(PLAIN.version);
            assert_eq!(first.dir, dir.to_string_lossy());
            assert_eq!(
                (mode(&dir.join("homology-runner")), mode(&dir), mode(&home.path().join(".cache/homology/runner"))),
                (0o700, 0o700, 0o700)
            );
            assert!(!old.exists(), "older versions are removed");
            first.client.request("ping", Default::default(), Duration::from_secs(10)).unwrap();
            first.client.close(Duration::from_secs(10)).unwrap();
            let second = connect_to(&host, package.path()).unwrap();
            assert!(!second.uploaded, "the same runner is not sent again");
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file is left");
        }

        #[test]
        fn a_different_runner_on_the_host_is_replaced() {
            let (home, package) = (tempfile::tempdir().unwrap(), package());
            let host = host(home.path(), None, false);
            let placed = connect_to(&host, package.path()).unwrap();
            drop(placed.client);
            std::fs::write(Path::new(&placed.dir).join("homology-runner"), "#!/bin/sh\nexit 9\n").unwrap();
            assert!(connect_to(&host, package.path()).unwrap().uploaded);
        }

        #[test]
        fn a_runner_that_cannot_run_is_placed_again_once() {
            let (home, package) = (tempfile::tempdir().unwrap(), package());
            let host = host(home.path(), None, false);
            let placed = connect_to(&host, package.path()).unwrap();
            drop(placed.client);
            let runner = Path::new(&placed.dir).join("homology-runner");
            std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o600)).unwrap();
            let mut again = connect_to(&host, package.path()).unwrap();
            assert!(again.uploaded);
            again.client.request("ping", Default::default(), Duration::from_secs(10)).unwrap();
            assert_eq!(mode(&runner), 0o700);
        }

        #[test]
        fn a_damaged_upload_is_refused_and_leaves_nothing() {
            let (home, package) = (tempfile::tempdir().unwrap(), package());
            let error = connect_to(&host(home.path(), None, true), package.path()).err().unwrap();
            assert_eq!(error.code, RUNNER_UPLOAD_FAILED, "{error}");
            assert!(error.message.contains("damaged"), "{error}");
            let dir = home.path().join(".cache/homology/runner/bin").join(PLAIN.version);
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        }

        #[test]
        fn an_absolute_xdg_cache_home_is_used_and_a_relative_one_is_not() {
            let (home, cache, package) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), package());
            let placed = connect_to(&host(home.path(), Some(cache.path()), false), package.path()).unwrap();
            assert!(placed.dir.starts_with(&*cache.path().to_string_lossy()), "{}", placed.dir);
            let placed =
                connect_to(&host(home.path(), Some(Path::new("relative-cache")), false), package.path()).unwrap();
            assert!(placed.dir.starts_with(&*home.path().join(".cache").to_string_lossy()), "{}", placed.dir);
        }
    }
}
