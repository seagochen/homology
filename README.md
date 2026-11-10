# Homology

Work on folders, files and code where they are: on this computer, or on the
user's own SSH server. Reading, writing, searching, checking code and the
console run on the machine that holds the data, through no service of anyone's.

[AnnotaryCat](https://github.com/seagochen/annotary) and
[Crescent](https://github.com/seagochen/crescent) share it. The plan and its
steps are in [#1](https://github.com/seagochen/homology/issues/1).

## Crates

```
homology-sys      lib         the system helpers the others share (the standard library alone)
homology-runner   lib + bin   the protocol (JSON lines, a hello handshake, stable error codes)
                              the server: the operations every runner has, and those a program adds (server::Runner)
                              the client: over the pipes of any child process (this computer, or ssh … serve)
                              fs.*: list, read (with sha256), write only over what was opened, search
                              env.*: Python environments found (conda, venv, pyenv, uv, the system's) and probed
                              code.check: pyflakes (bundled), imports, ruff and pylint, and a program's own rules
                              toolchain.*: C/C++, CUDA, Rust and Node toolchains found (PATH, ~/.cargo/bin,
                              CUDA's folders) without running them, and probed for their version
                              code.exec: a command run in a folder, exit code and output back, its process
                              group ended past its time limit
homology-ssh      lib         the system ssh in batch mode: targets, scripts and sessions, stable error codes,
                              host keys the user confirms, a password used once through askpass,
                              login keys for hosts that take only a password
homology          lib         the caller's side: a program's runners carried for SSH hosts, placed on a
                              host by their hash and talked to over ssh … serve, one session per host
                              console: a shell or an interactive Python on this computer or a host,
                              read back by offset, interrupted as Ctrl+C would
```

A program gives `homology-ssh` its names (`App`: its askpass helper, its
login keys' comment, the prefix of the variables it reads), and `homology`
the runner it deploys (`Program`: its name, version, folder in a host's
cache, the prefix of the variables that point at other runners). A console
on a host takes the program's own session type, any that holds
`homology-ssh`'s `Context` (`AsRef`).

Files are written so that a reader sees the old content or the new, never
half of it: through a temporary file beside them with a random name, made
only where nothing is yet, so a link someone left in the folder is never
written through (#3).

## The runner on its own

`homology-runner` works without any program of its own: `Program::PLAIN`
deploys it, with the operations every runner has.

```sh
scripts/build-runners.sh            # target/runners/homology-runner-linux-{x86_64,aarch64}, SHA256SUMS
```

The script builds static Linux programs (musl) for x86_64 and aarch64,
linked by the `rust-lld` Rust ships (`.cargo/config.toml`), so Linux, macOS
and Windows (Git Bash) build them with nothing but `rustup`. They are
written without the executable bit; `homology::deploy` makes one
executable only on the host it sends it to, in
`$XDG_CACHE_HOME/homology/runner/bin/<version>/`, and replaces it when its
hash differs. A program finds them beside itself (`runner/`, or
`../Resources/runner` in a macOS bundle) or where `HOMOLOGY_RUNNER_DIR`
says; this computer's runner beside itself or where `HOMOLOGY_RUNNER` says.

```text
homology-runner serve       one session on stdin/stdout (JSON lines)
homology-runner --version   the version and protocol
```

## Testing

`cargo test --workspace` also checks properties on random inputs
(`crates/homology-runner/src/properties.rs`, proptest): search never cuts a
line inside a character, no file operation reaches outside its folder
through any link, the protocol's lines read back as written, lines are
read whole and bounded, and a session answers each request once.

Coverage-guided fuzzing of what the runner reads from others is in
`fuzz/` (its own workspace, never part of a runner), with cargo-fuzz on a
nightly toolchain:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz list                                      # request_parse, message_parse, serve, program_output
cargo +nightly fuzz run serve -- -max_total_time=600
```

The `serve` target never performs what its input asks of this computer:
every request but `hello`, `ping` and `shutdown` is renamed first.

CI (`.github/workflows/ci.yml`) checks formatting, lints and tests on
Linux, Windows and macOS, and builds the runners on Linux and macOS.

## Protocol

One JSON object per line, UTF-8, over the runner's stdin and stdout:

```text
{"id": 7, "op": "fs.read", "params": {"root": "/srv/m", "path": "a.py"}}   request
{"id": 7, "ok": true, "result": {...}}                                     reply
{"id": 7, "ok": false, "error": {"code": "...", "message": "..."}}         failure
{"event": "...", "data": {...}}                                            event (no id)
```

The first request is `hello {protocol, version}`; the answer is
`{protocol, name, version, ops}`.

## Use

```bash
cargo build --release -p homology-runner
homology-runner serve        # one session on stdin/stdout
homology-runner --version
```

As a library, a program serves the runner under its own name and version
through `server::Runner`: its own operations, its cache folder, and rules for
the code check (Python modules with a `TOOL` name and `check(texts)`, see
`crates/homology-runner/python/homology_check.py`), as AnnotaryCat's
`annotarycat-runner` does.

## Development

See [AGENTS.md](AGENTS.md).
