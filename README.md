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
homology-ssh      lib         the system ssh in batch mode: targets, scripts and sessions, stable error codes,
                              host keys the user confirms, a password used once through askpass,
                              login keys for hosts that take only a password
```

A program gives `homology-ssh` its names (`App`: its askpass helper, its
login keys' comment, the prefix of the variables it reads). Coming in the
next steps: `homology` (deploying the runner, one session per server, the
console).

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
