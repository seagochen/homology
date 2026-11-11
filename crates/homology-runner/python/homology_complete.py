"""Homology's code completion (homology#14): what may follow the cursor,
as jedi (bundled in vendor/, MIT, with parso) reads the code in this
environment, so that its installed packages (torch, numpy, ...) complete
as they are here. The code is read, never run; modules jedi cannot read as
source (compiled extensions) it may import to look at, as editors do.

It reads one JSON object on stdin:

    {"text": "...", "line": 1, "column": 0, "path": "a/b.py"?, "root": "/dir"?, "limit": 50?}

`line` counts from 1 and `column` from 0, in characters; `root` is the
folder the code is in (its modules complete too), `path` the file's place
in it. It writes one JSON object on stdout:

    {"completions": [{"name": "path", "complete": "th", "type": "module"}, ...], "python": "3.11.2"}

or `{"error": "..."}` when the position is not in the text.
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "vendor"))

import jedi  # noqa: E402

# What the completion brought along is not the environment's: jedi is
# imported, then the code is read against the environment's own path.
sys.path[:] = [entry for entry in sys.path if os.path.abspath(entry or ".") not in (HERE, os.path.join(HERE, "vendor"))]


def main():
    asked = json.load(sys.stdin)
    root, path = asked.get("root"), asked.get("path")
    project = jedi.Project(root, added_sys_path=[root]) if root else None
    script = jedi.Script(
        asked["text"],
        path=os.path.join(root, path) if root and path else None,
        project=project,
        environment=jedi.InterpreterEnvironment(),
    )
    try:
        found = script.complete(asked["line"], asked["column"])
    except ValueError as error:
        json.dump({"error": str(error)}, sys.stdout)
        return
    limit = max(1, int(asked.get("limit") or 50))
    completions = [{"name": item.name, "complete": item.complete or "", "type": item.type} for item in found[:limit]]
    json.dump({"completions": completions, "python": sys.version.split()[0]}, sys.stdout)


if __name__ == "__main__":
    main()
