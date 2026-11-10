# python/

Python the runner runs in the user's own environments. Everything here is
compiled into the runner (`src/python.rs`, `include_bytes!`; a test checks
that every file here is listed) and written into the runner's cache
(`python/<version>-<hash>/`) when first needed, so nothing is installed into
the user's environment and nothing else is uploaded to a host.

| Path | What | Licence |
|---|---|---|
| `homology_check.py` | The code check: pyflakes, imports looked up with `importlib.util.find_spec` (never imported), ruff and pylint when installed, and the rules modules of the program serving the runner (`TOOL`, `check(texts)`) | Homology's |
| `vendor/pyflakes/` | pyflakes **3.4.0** (`__init__.py`, `checker.py`, `messages.py`, unchanged), from the sdist `pyflakes-3.4.0.tar.gz`, sha256 `b24f96fafb7d2ab0ec5075b7350b3d2d2218eab42003821c06344973d3ea2f58`. Needs Python 3.9 or later | MIT, `vendor/pyflakes/LICENSE` |

To update pyflakes: download the new sdist, copy the same three files and its
`LICENSE` unchanged, update the version and hash above, and run
`cargo test --test check`.
