# python/

Python the runner runs in the user's own environments. Everything here is
compiled into the runner (`src/python.rs`, `include_bytes!`, the list made
by `build.rs` from this folder; a test checks that every file here is listed) and written into the runner's cache
(`python/<version>-<hash>/`) when first needed, so nothing is installed into
the user's environment and nothing else is uploaded to a host.

| Path | What | Licence |
|---|---|---|
| `homology_check.py` | The code check: pyflakes, imports looked up with `importlib.util.find_spec` (never imported), ruff and pylint when installed, and the rules modules of the program serving the runner (`TOOL`, `check(texts)`) | Homology's |
| `homology_complete.py` | Code completion (`code.complete`, #14): jedi reads the code against the environment it runs in, so its installed packages complete | Homology's |
| `vendor/jedi/` | jedi **0.19.2**, from the sdist `jedi-0.19.2.tar.gz`, sha256 `4770dc3de41bde3966b02eb84fbcf557fb33cce26ad23da12c742fb50ecb11f0`: the package unchanged, but for `third_party/django-stubs/` and typeshed's `third_party/` left out to keep the runner small (the environment's own packages complete instead); jedi lists those folders, so `third_party/typeshed/third_party/README` and an empty `third_party/django-stubs/django-stubs/__init__.pyi` stand for them | MIT, `vendor/jedi/LICENSE.txt`; typeshed's stubs Apache-2.0, `vendor/jedi/third_party/typeshed/LICENSE` |
| `vendor/parso/` | parso **0.8.4** (jedi's parser), from the sdist `parso-0.8.4.tar.gz`, sha256 `eb3a7b58240fb99099a345571deecc0f9540ea5f4dd2fe14c2a99d6b281ab92d`, the package unchanged | MIT, `vendor/parso/LICENSE.txt` |
| `vendor/pyflakes/` | pyflakes **3.4.0** (`__init__.py`, `checker.py`, `messages.py`, unchanged), from the sdist `pyflakes-3.4.0.tar.gz`, sha256 `b24f96fafb7d2ab0ec5075b7350b3d2d2218eab42003821c06344973d3ea2f58`. Needs Python 3.9 or later | MIT, `vendor/pyflakes/LICENSE` |

To update pyflakes: download the new sdist, copy the same three files and its
`LICENSE` unchanged, update the version and hash above, and run
`cargo test --test check`.

To update jedi or parso: download the sdist, replace the package folder with
the new one (for jedi, leave out the same stubs as above and keep the two
stand-ins), update the version and hash above, and run
`cargo test --test complete`.
