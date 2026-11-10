"""Homology's code check.

The runner runs this file with the Python of the environment the user
chose, so what it finds is what a run there would meet:

- pyflakes (bundled in vendor/, MIT): syntax errors, undefined names,
  unused imports;
- imports: each top-level module imported is looked up with
  importlib.util.find_spec in this environment, never imported, so none of
  the user's code and no package's code runs; modules of the directory
  itself, relative imports and imports inside `try:` are left out;
- ruff and pylint, when the environment has them (each in a process of its
  own, on the files given); otherwise said to be not installed;
- the rules of the program that serves the runner, when it names any: each
  a module beside this file (imported before the user's code is looked at)
  with a `TOOL` name and `check(texts)`, which is given the files checked
  as {path: text} and answers a list of problems ({file, line, col, code,
  message, severity}), or None when none of the files is its to check. A
  rules module reads code; it never runs it.

It reads one JSON object on stdin:

    {"root": "<folder holding the files>", "files": ["src/train.py"],
     "listing": ["src/train.py", "src/utils.py", ...], "timeout": 60,
     "rules": ["some_rules"]}

and writes one on stdout:

    {"problems": [{"file", "line", "col", "code", "message", "tool", "severity"}],
     "tools": {"pyflakes": "ran", "imports": "ran", "ruff": "not_installed", ...},
     "python": "3.11.4"}
"""

import ast
import importlib
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "vendor"))

from pyflakes import checker as flakes  # noqa: E402

sys.path.insert(0, HERE)

ERRORS = {"UndefinedName", "UndefinedExport", "UndefinedLocal", "ReturnOutsideFunction", "YieldOutsideFunction", "ContinueOutsideLoop", "BreakOutsideLoop", "DuplicateArgument"}


PYLINT_IMPORTS = {"import-error", "no-name-in-module", "relative-beyond-top-level"}

def problem(file, line, col, code, message, tool, severity):
    return {"file": file, "line": max(int(line or 1), 1), "col": max(int(col or 1), 1), "code": code, "message": message, "tool": tool, "severity": severity}


def pyflakes_check(path, text):
    """The file's syntax tree (None when it does not parse) and what pyflakes says."""
    try:
        tree = ast.parse(text, filename=path)
    except SyntaxError as error:
        return None, [problem(path, error.lineno, error.offset, "SyntaxError", error.msg, "pyflakes", "error")]
    found = []
    for message in flakes.Checker(tree, filename=path, withDoctest=False).messages:
        name = type(message).__name__
        found.append(problem(path, message.lineno, getattr(message, "col", 0) + 1, name, message.message % message.message_args, "pyflakes", "error" if name in ERRORS else "warning"))
    return tree, found


def local_modules(path, listing):
    """Top-level names a module of the directory answers to, seen
    from `path`: the files beside it and at the directory's root."""
    names = set()
    folders = {"", os.path.dirname(path).replace("\\", "/")}
    for listed in listing:
        parent, _, rest = listed.rpartition("/")
        for folder in folders:
            prefix = folder + "/" if folder else ""
            if not listed.startswith(prefix):
                continue
            inside = listed[len(prefix):]
            first = inside.split("/", 1)[0]
            if "/" in inside:
                names.add(first)  # a package or namespace folder
            elif first.endswith((".py", ".pyi", ".so", ".pyd")):
                names.add(first.split(".", 1)[0])
    return names


class Imports(ast.NodeVisitor):
    """The absolute imports of a module, outside `try:` blocks."""

    def __init__(self):
        self.found = []
        self.guarded = 0

    def visit_Try(self, node):
        self.guarded += 1
        for child in node.body:
            self.visit(child)
        self.guarded -= 1
        for child in node.handlers + node.orelse + node.finalbody:
            self.visit(child)

    visit_TryStar = visit_Try

    def visit_Import(self, node):
        if not self.guarded:
            for alias in node.names:
                self.found.append((alias.name.split(".")[0], node.lineno, node.col_offset + 1))

    def visit_ImportFrom(self, node):
        if not self.guarded and node.level == 0 and node.module:
            self.found.append((node.module.split(".")[0], node.lineno, node.col_offset + 1))


def imports_check(path, tree, listing):
    import importlib.util

    local = local_modules(path, listing)
    visitor = Imports()
    visitor.visit(tree)
    found, known = [], {}
    for name, line, col in visitor.found:
        if name == "__future__" or name in local or name in sys.builtin_module_names:
            continue
        if name not in known:
            try:
                # Finds without importing: nothing of the module runs.
                known[name] = importlib.util.find_spec(name) is not None
            except (ImportError, ValueError, AttributeError):
                known[name] = True
        if not known[name]:
            found.append(problem(path, line, col, "ModuleNotFound", "No module named %r in this environment (%s)" % (name, sys.executable), "imports", "error"))
    return found


def installed(name):
    import importlib.util

    try:
        return importlib.util.find_spec(name) is not None
    except (ImportError, ValueError):
        return False


def tool_run(args, root, timeout):
    """Run a tool on the files; its output, or why there is none."""
    try:
        done = subprocess.run([sys.executable, "-m"] + args, cwd=root, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None, "timed_out"
    except OSError as error:
        return None, "failed: %s" % error
    return done.stdout, None


def relative(root, name):
    full = os.path.normpath(os.path.join(root, name))
    return os.path.relpath(full, root).replace("\\", "/")


def ruff_check(root, files, timeout):
    out, why = tool_run(["ruff", "check", "--output-format=json", "--no-cache", "--exit-zero", "--"] + files, root, timeout)
    if why:
        return [], why
    try:
        found = json.loads(out or "[]")
    except ValueError:
        return [], "failed: ruff said something that is not JSON"
    problems = []
    for item in found:
        code = item.get("code") or "ruff"
        location = item.get("location") or {}
        severity = "error" if code.startswith("E9") or code in ("F821", "F822", "F823") or item.get("code") is None else "warning"
        problems.append(problem(relative(root, item.get("filename", "")), location.get("row"), location.get("column"), code, item.get("message", ""), "ruff", severity))
    return problems, "ran"


def pylint_check(root, files, timeout):
    # The user's pylint settings are not read: they may name plugins, and
    # loading a plugin runs code.
    empty = os.path.join(root, ".homology-pylintrc")
    with open(empty, "w") as handle:
        handle.write("[MAIN]\n")
    out, why = tool_run(["pylint", "--rcfile", empty, "--output-format=json", "--exit-zero", "--score=n", "--"] + files, root, timeout)
    if why:
        return [], why
    try:
        found = json.loads(out or "[]")
    except ValueError:
        return [], "failed: pylint said something that is not JSON"
    problems = []
    for item in found:
        # pylint sees only the files checked, not the rest of the
        # directory: its import errors are left to the imports check.
        if item.get("symbol") in PYLINT_IMPORTS:
            continue
        severity = "error" if item.get("type") in ("error", "fatal") else "warning"
        code = "%s(%s)" % (item.get("message-id", ""), item.get("symbol", ""))
        problems.append(problem(relative(root, item.get("path", "")), item.get("line"), (item.get("column") or 0) + 1, code, item.get("message", ""), "pylint", severity))
    return problems, "ran"


def rules_check(rules, texts):
    """What each rules module says of the files; its tool's state."""
    problems, tools = [], {}
    for module in rules:
        found = module.check(texts)
        if found is None:
            tools[module.TOOL] = "not_available"
            continue
        tools[module.TOOL] = "ran"
        for item in found:
            problems.append(problem(item.get("file", ""), item.get("line"), item.get("col"), item.get("code", module.TOOL), item.get("message", ""), module.TOOL, item.get("severity", "warning")))
    return problems, tools


def main():
    asked = json.load(sys.stdin)
    rules = [importlib.import_module(name) for name in asked.get("rules") or []]
    # What the checker brought along is not the environment's: its own
    # modules are imported, and the user's imports are looked up without it.
    sys.path[:] = [entry for entry in sys.path if os.path.abspath(entry or ".") not in (HERE, os.path.join(HERE, "vendor"))]
    root, files, listing = asked["root"], asked["files"], asked.get("listing") or asked["files"]
    timeout = asked.get("timeout", 60)
    problems = []
    tools = {}
    texts = {}
    for path in files:
        with open(os.path.join(root, path), encoding="utf-8", errors="replace") as handle:
            text = handle.read()
        texts[path] = text
        tree, found = pyflakes_check(path, text)
        problems += found
        if tree is not None:
            problems += imports_check(path, tree, listing)
    tools["pyflakes"] = "ran"
    tools["imports"] = "ran"
    for name, run in (("ruff", ruff_check), ("pylint", pylint_check)):
        if not installed(name):
            tools[name] = "not_installed"
            continue
        found, tools[name] = run(root, files, timeout)
        problems += found
    found, said = rules_check(rules, texts)
    problems += found
    tools.update(said)
    problems.sort(key=lambda item: (item["file"], item["line"], item["col"]))
    json.dump({"problems": problems, "tools": tools, "python": "%d.%d.%d" % sys.version_info[:3]}, sys.stdout)


if __name__ == "__main__":
    main()
