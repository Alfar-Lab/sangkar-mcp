#!/usr/bin/env python3
"""Compare two parity goldens structurally, the way the Rust harness will.

A golden is `{"args": [...], "stdout": "<exact bytes>", "exit_code": <n>}` (see
`rust/tests/parity/README.md`). Two goldens agree when `args` is the same list, `exit_code` is the
same integer, and the JSON document inside `stdout` has the same key sets at every depth, the same
primitive values and the same array order and length. Nothing here reads the repository, a database
or the clock: dependency-free python3, two files in, one verdict out.

Structural comparison needs `--json` goldens. A TOON or otherwise non-JSON `stdout` cannot be
compared key by key, so it falls back to a byte comparison and says so.

Exit: 0 identical, 1 any difference, 2 usage or an unreadable/invalid golden.
"""

from __future__ import annotations

import json
import sys

USAGE = "usage: compare.py <expected-golden.json> <actual-golden.json>"


def escape(token: str) -> str:
    """JSON-pointer escaping (RFC 6901 §3): `~` then `/`, in that order."""
    return token.replace("~", "~0").replace("/", "~1")


def at(path: str, token: str | int) -> str:
    """One child step of a JSON pointer; the root pointer is the empty string."""
    return f"{path}/{escape(str(token))}"


def describe(value: object) -> str:
    """A primitive's value with its JSON type, so `1` and `"1"` cannot look equal in a report."""
    if isinstance(value, bool):
        return f"bool {str(value).lower()}"
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, (int, float)):
        return f"number {value!r}"
    if value is None:
        return "null"
    if isinstance(value, list):
        return f"list[{len(value)}]"
    if isinstance(value, dict):
        return f"object{{{len(value)} keys}}"
    return repr(value)


def kind(value: object) -> str:
    """The comparison class: booleans are not numbers here, and int/float are one JSON number."""
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    if value is None:
        return "null"
    if isinstance(value, list):
        return "list"
    return "object"


def compare(expected: object, actual: object, path: str, diffs: list[str]) -> None:
    """Walk both values in parallel, appending one line per difference at its JSON pointer."""
    left, right = kind(expected), kind(actual)
    if left != right:
        diffs.append(f"{path or '/'}: type {left} (expected) vs {right} (actual)")
        return

    if left == "object":
        expected_keys = list(expected)
        actual_keys = list(actual)
        for key in expected_keys:
            if key not in actual:
                diffs.append(f"{at(path, key)}: missing in actual ({describe(expected[key])})")
        for key in actual_keys:
            if key not in expected:
                diffs.append(f"{at(path, key)}: unexpected in actual ({describe(actual[key])})")
        for key in expected_keys:
            if key in actual:
                compare(expected[key], actual[key], at(path, key), diffs)
        return

    if left == "list":
        if len(expected) != len(actual):
            diffs.append(
                f"{path or '/'}: length {len(expected)} (expected) vs {len(actual)} (actual)"
            )
        # Index by index, so a reordered pair reports the position that moved: array order is
        # contract (a cursor is a position in a generation's stable ordering).
        for index in range(min(len(expected), len(actual))):
            compare(expected[index], actual[index], at(path, index), diffs)
        return

    if expected != actual:
        diffs.append(f"{path or '/'}: {describe(expected)} (expected) vs {describe(actual)} (actual)")


def load_golden(path: str) -> dict:
    """A golden object with the three contract keys, or a refusal that names what is wrong."""
    try:
        with open(path, "r", encoding="utf-8") as handle:
            document = json.load(handle)
    except OSError as error:
        fail(f"{path}: cannot read the golden: {error}")
    except json.JSONDecodeError as error:
        fail(f"{path}: not JSON: {error}")

    if not isinstance(document, dict):
        fail(f"{path}: a golden is a JSON object, found {kind(document)}")
    for key in ("args", "stdout", "exit_code"):
        if key not in document:
            fail(f"{path}: a golden needs a {key!r} key; found {sorted(document)}")
    if not isinstance(document["args"], list):
        fail(f"{path}: 'args' must be a list")
    if not isinstance(document["stdout"], str):
        fail(f"{path}: 'stdout' must be a string of the exact bytes")
    if not isinstance(document["exit_code"], int) or isinstance(document["exit_code"], bool):
        fail(f"{path}: 'exit_code' must be an integer")
    return document


def fail(message: str) -> None:
    print(f"compare: {message}", file=sys.stderr)
    raise SystemExit(2)


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(USAGE, file=sys.stderr)
        return 2

    expected = load_golden(argv[1])
    actual = load_golden(argv[2])
    diffs: list[str] = []

    compare(expected["args"], actual["args"], "/args", diffs)
    if expected["exit_code"] != actual["exit_code"]:
        diffs.append(
            f"/exit_code: {expected['exit_code']} (expected) vs {actual['exit_code']} (actual)"
        )

    expected_stdout, actual_stdout = expected["stdout"], actual["stdout"]
    try:
        expected_json = json.loads(expected_stdout)
    except json.JSONDecodeError:
        expected_json = None
    try:
        actual_json = json.loads(actual_stdout)
    except json.JSONDecodeError:
        actual_json = None

    if expected_json is None or actual_json is None:
        # The one case structural comparison cannot serve, said out loud instead of guessed at.
        print(
            "compare: stdout is not JSON on both sides, so it cannot be compared structurally; "
            "comparing bytes instead. Structural comparison needs --json goldens.",
            file=sys.stderr,
        )
        if expected_stdout != actual_stdout:
            shared = 0
            while (
                shared < min(len(expected_stdout), len(actual_stdout))
                and expected_stdout[shared] == actual_stdout[shared]
            ):
                shared += 1
            diffs.append(
                f"/stdout: bytes differ at offset {shared} "
                f"({len(expected_stdout)} vs {len(actual_stdout)} bytes)"
            )
    else:
        compare(expected_json, actual_json, "/stdout", diffs)

    if diffs:
        for line in diffs:
            print(line)
        print(f"compare: {len(diffs)} difference(s) between {argv[1]} and {argv[2]}")
        return 1

    print(f"compare: identical: {argv[1]} and {argv[2]}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
