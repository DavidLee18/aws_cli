#!/usr/bin/env python3
"""Record what the reference's YAML loader makes of a corpus of documents.

Why this exists
---------------
``--cli-input-yaml`` is loaded by ``ruamel.yaml``'s ``YAML(typ='safe', pure=True)``
(``awscli/customizations/cliinput.py``), which is **YAML 1.2**. Almost everything
written about "YAML in Python" describes PyYAML's ``safe_load``, which is YAML
1.1, and the two disagree about what ordinary documents mean: under 1.1 ``no`` is
a boolean and ``017`` is octal 15, under 1.2 they are the string ``"no"`` and the
number 17. A parser written from the wrong version reads every document happily
and sends different values than ``aws`` does.

So the rules are not transcribed from prose. This script runs the same loader the
reference runs and records its answers as a golden corpus, which
``crates/awsc/src/yaml.rs`` is tested against.

Values are recorded as JSON, which is what the Rust reader produces. Two types
have no JSON spelling and are recorded as tagged objects instead:

* ``{"__timestamp__": "..."}`` -- ruamel resolves ISO dates to ``datetime``.
  The Rust reader keeps the original string; the test asserts that, and
  ``docs/divergences.md`` records why.
* ``{"__error__": "ClassName"}`` -- the loader refused the document. The Rust
  reader must refuse it too, though not necessarily for the same stated reason.

Usage:  scripts/extract-yaml-input-cases.py [--output PATH]

Needs ruamel.yaml (`pip install ruamel.yaml`), which is the reference's own
dependency and is NOT vendored here.
"""

from __future__ import annotations

import argparse
import datetime
import json
import pathlib
import sys

# Every case is a YAML document. They are grouped only for readability; the
# corpus is flat. Keep entries small and pointed -- each should fail loudly for
# one reason if the reader regresses.
CASES: list[str] = [
    # -- block mappings and nesting
    "a: 1\nb: two",
    "a:\n  b:\n    c: 1",
    "outer:\n  inner: 1\nsibling: 2",
    "a: 1\n\n\nb: 2",
    "key with spaces: 1",
    "'quoted key': 1",
    "1: one",
    "true: yes",
    # -- sequences
    "- 1\n- 2\n- 3",
    "a:\n- 1\n- 2",
    "a:\n  - 1\n  - 2",
    "- a: 1\n  b: 2",
    "- - 1\n  - 2",
    "a:\n  - x: 1\n    y: 2",
    "a:\n  - - 1\n    - 2",
    "-",
    "- ",
    "a: []\nb: {}",
    # -- flow collections
    "a: [1, two, {b: 3}]",
    '{"a": 1, "b": [2]}',
    "a: [1,\n  2,\n  3]",
    "[a, b,]",
    "{a: 1, b: 2}",
    "{a:1}",
    "a: [1, {b: 2}]",
    '{"a":1}',
    "a: [[1, 2], [3]]",
    # -- scalars, YAML 1.2 resolution
    "a: yes\nb: no\nc: on\nd: off",
    "a: true\nb: True\nc: TRUE\nd: false\ne: False",
    "a: y\nb: n\nc: Y",
    "a: null\nb: ~\nc: NULL\nd:",
    "a: 017\nb: 0o17\nc: 0x1f\nd: 0b101\ne: 00",
    "a: 1_000\nb: 1__0",
    "a: -0\nb: +5\nc: -17",
    "a: 0.5\nb: .5\nc: 5.\nd: 1e3\ne: 1.0e+3\nf: -2.5",
    "a: 1:30",
    "a: 1.2.3",
    "a: 2020-01-01",
    "a: 2020-01-01T10:00:00Z",
    "a: '2020-01-01'",
    "a: v1.2",
    "a: 12345678901234567890123",
    "a: ''\nb: \"\"",
    "a: '012345678901'",
    "a: 012345678901",
    "a: text with spaces",
    "a: :colon",
    "a: b:c",
    "a: x#y",
    "a: x #y",
    "a: '#'",
    "a: 'it''s'",
    'a: "esc\\n\\u0041\\t"',
    'a: "tab\\tseparated"',
    "a: multi\n  line plain",
    "a: 'x\n  y'",
    'a: "x\n  y"',
    "a: -",
    "a: --flag",
    # -- comments
    "key: value # comment",
    "# only a comment\na: 1",
    "a: 1\n# between\nb: 2",
    "a: 'x'  # c",
    # -- block scalars
    "a: |\n  line1\n  line2\n",
    "a: |-\n  x\n",
    "a: |+\n  x\n\n",
    "a: >\n  fold1\n  fold2\n",
    "a: >-\n  x\n  y\n",
    "a: |2\n    x\n",
    "a: |\n  x\n\n  y\n",
    "a: >\n  x\n\n  y\n",
    "a: |\n  # not a comment\n",
    "a: |\n  one\nb: 2",
    # -- anchors, aliases, merges, tags
    "a: &anc 1\nb: *anc",
    "base: &b {x: 1}\nchild:\n  <<: *b\n  y: 2",
    "base: &b {x: 1}\nchild:\n  <<: *b\n  x: 2",
    "a: &anc\n  b: 1\nc: *anc",
    "a: !!str 123",
    "a: !!int '5'",
    "a: !!null ''",
    # -- documents
    "---\na: 1",
    "--- \na: 1",
    "a: 1\n...\n",
    "a: 1\n---\nb: 2",
    "",
    "\n\n",
    "plain scalar",
    "123",
    "[1,2]",
    "null",
    # -- errors
    "a: 1\na: 2",
    "{a: 1, a: 2}",
    "a: 'unterminated",
    "a: *nope",
    "a:\n\tb: 1",
    "a: - 1",
    # -- found by fuzzing against the loader: each of these was read wrongly at first
    "a: @at",
    "a: `tick`",
    "a: %pct",
    "k: x@y",
    "k: 50%",
    "a: >\n  more text",
    "a: |\n  x",
    "a: |+\n  x",
    "a: |+\n  \n",
    "a:\n- x\n  - y",
    "a: text\n  - more",
    "a: text\n\n  more",
    "a: text\n  b1",
    "a: text\n  b: 1",
    "a:\n- x\n  - {k: v}",
    "a:\n- x\n  - [1]",
    "k: ~\n  more",
    "true: 1\n1: 2",
    "1: a\n1.0: b",
    "a: -",
    # -- documents shaped like real CLI input
    "Filters:\n  - Name: tag:Env\n    Values:\n      - prod\nMaxResults: 5",
    "repositoryName: demo\nbranchName: main\nputFiles:\n  - filePath: a.txt\n    fileContent: aGk=\ncommitMessage: |\n  Add a file\n\n  With a body.\n",
    "TableName: t\nKey:\n  id:\n    S: '123'\nConsistentRead: true",
]


def encode(value):
    """JSON-encodable form, with the two types JSON cannot hold tagged."""
    if isinstance(value, (datetime.datetime, datetime.date)):
        return {"__timestamp__": value.isoformat()}
    if isinstance(value, float) and (value != value or value in (float("inf"), float("-inf"))):
        return {"__float__": repr(value)}
    if isinstance(value, (set, frozenset)):
        return {"__set__": sorted(str(v) for v in value)}
    if isinstance(value, bytes):
        return {"__bytes__": value.decode("utf-8", "replace")}
    raise TypeError(f"no JSON spelling for {type(value).__name__}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parent.parent
        / "tests"
        / "golden"
        / "yaml-input-cases.json",
    )
    args = parser.parse_args()

    try:
        from ruamel.yaml import YAML
    except ImportError:
        print(
            "ruamel.yaml is required (pip install ruamel.yaml); it is the loader the "
            "reference CLI uses for --cli-input-yaml",
            file=sys.stderr,
        )
        return 1

    import ruamel.yaml

    cases = []
    for document in CASES:
        # A fresh loader per document. A reused ``YAML()`` instance keeps state across
        # ``load()`` calls -- after one document raises, later ones inherit its recorded
        # keys and are reported as DuplicateKeyError. Sharing one loader produced a
        # corpus that looked plausible and was wrong in a way no single case revealed.
        loader = YAML(typ="safe", pure=True)
        entry: dict[str, object] = {"yaml": document}
        try:
            entry["value"] = json.loads(json.dumps(loader.load(document), default=encode))
        except Exception as exc:  # noqa: BLE001 - any refusal is a recorded refusal
            entry["value"] = {"__error__": type(exc).__name__}
        cases.append(entry)

    payload = {
        "loader": "ruamel.yaml YAML(typ='safe', pure=True)",
        "ruamel_version": ".".join(str(p) for p in ruamel.yaml.version_info[:3]),
        "cases": cases,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n")
    print(f"wrote {len(cases)} cases to {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
