#!/usr/bin/env python3
"""Extract awscli v2's remaining surface customizations into a JSON overlay.

Why this exists
---------------
After service names, paginators, argrename/removals/renames
(``data/customizations.json``) and pagination flags, the Rust port still
diverges from the reference CLI in three ways, all rooted in hand-written
customizations or botocore-vs-Smithy dialect gaps:

* ``modeled_arg_patches``: per-operation argument add/remove lists for modeled
  operations whose ``building-argument-table`` hooks inject or delete flags
  (EC2 security-group shorthands, rekognition ``--image-bytes``, ``--source-region``,
  streaming ``--*-outfile`` variants, EMR command replacements, ...). Scope is
  exactly the flag-level worklist in ``docs/remaining-divergences.txt``; every
  worklist flag is verified against the *live* argument table built by the
  reference machinery (and against the golden corpus) before being emitted.
  Flags that fail verification are reported and NOT emitted.
* ``custom_commands``: commands the reference adds beyond the model --
  BasicCommand trees (``deploy push``, ``codecommit credential-helper get``,
  ``logs tail``, wizards, ...), model-derived commands under non-model names
  (``rds add-option-to-option-group``), botocore-modeled operations absent
  from the Smithy models (``s3api get-bucket-lifecycle``), and modeled
  operations whose CLI name depends on botocore's seeded ``_xform_cache``
  special cases (mturk ``list-hits-...``, storagegateway ``*-iscsi-*``,
  socialmessaging ``*whatsapp*``). Argument lists are read from the live
  command/arg tables, never transcribed from source. Keys are space-joined
  for nested commands (``credential-helper get``), matching the golden
  corpus. Flags keep their ``--`` prefix; positionals are bare names.
* ``waiters``: the full botocore ``waiters-2`` catalogue for every modeled
  service, mapped ``xform_name(waiter, '-') -> xform_name(operation, '-')``.
  This is the reference truth that replaces Smithy's ``smithy.waiters``
  (the dialects disagree, e.g. Smithy's autoscaling waiters do not exist in
  botocore, hence not in the reference CLI).
* ``replaced_operations``: modeled operations the Rust port derives but must
  drop, either because a customization deletes them from the reference table
  (rds ``modify-option-group``) or because the port's plain xform derivation
  yields a name botocore special-cases away (the naive spellings such as
  ``list-hi-ts-for-qualification-type``). Naive spellings are computed by
  running botocore's own xform regexes without the seeded ``_xform_cache``.

'wait' subcommand trees are NEVER emitted as custom commands -- the
``waiters`` section is the single source of truth for them. The 8 model-less
top-level commands (s3, configure, ddb, history, login, logout, update,
cli-dev) are out of scope entirely.

Usage:  scripts/extract-custom-surface.py [--output PATH]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

REFERENCE_SITE_PACKAGES = (
    "/opt/homebrew/Cellar/awscli/2.36.22/libexec/lib/python3.14/site-packages"
)
REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUTPUT = REPO_ROOT / "data" / "custom-surface.json"
WORKLIST = REPO_ROOT / "docs" / "remaining-divergences.txt"
CORPUS = REPO_ROOT / "tests" / "golden" / "reference-surface.json"
CUSTOMIZATIONS = REPO_ROOT / "data" / "customizations.json"

MAX_DEPTH = 4  # same command-tree depth limit as extract-reference-surface.py


def load_reference():
    """Import awscli from the reference install, isolated from local AWS config."""
    if not os.path.isdir(REFERENCE_SITE_PACKAGES):
        sys.exit(
            f"reference install not found at {REFERENCE_SITE_PACKAGES}\n"
            "install awscli v2, or edit REFERENCE_SITE_PACKAGES."
        )
    sys.path.insert(0, REFERENCE_SITE_PACKAGES)

    # Keep local profiles, plugins and env from perturbing the surface.
    os.environ["AWS_CONFIG_FILE"] = os.devnull
    os.environ["AWS_SHARED_CREDENTIALS_FILE"] = os.devnull
    os.environ.pop("AWS_PROFILE", None)
    os.environ.pop("AWS_DEFAULT_PROFILE", None)

    from awscli.clidriver import create_clidriver  # noqa: E402

    return create_clidriver()


# ---------------------------------------------------------------------------
# Worklist parsing
# ---------------------------------------------------------------------------


def parse_worklist(path: Path):
    """Parse docs/remaining-divergences.txt.

    Returns (arg_diffs, ops_missing, ops_extra) where
    arg_diffs = {(svc, op): {"missing": [...], "extra": [...]}} and
    ops_missing/ops_extra = {svc: (tokens, truncated_count_or_None)}.
    Token streams are kept raw because multi-word command names
    ('credential-helper get') are not splittable without the live table.
    """
    arg_diffs: dict[tuple[str, str], dict[str, list[str]]] = {}
    ops_missing: dict[str, tuple[list[str], int | None]] = {}
    ops_extra: dict[str, tuple[list[str], int | None]] = {}
    current: tuple[str, str] | None = None

    def _ops_line(rest: str):
        tokens = rest.split()
        truncated = None
        if "..." in tokens:
            i = tokens.index("...")
            if i != len(tokens) - 2 or not tokens[i + 1].startswith("+"):
                sys.exit(f"worklist: cannot parse truncation in {tokens!r}")
            truncated = int(tokens[i + 1].lstrip("+"))
            tokens = tokens[:i]
        return tokens, truncated

    for raw in path.read_text().splitlines():
        if not raw.strip():
            continue
        if raw.startswith((" ", "\t")):
            kind, _, flags = raw.strip().partition(":")
            if kind not in ("missing", "extra") or current is None:
                sys.exit(f"worklist: cannot parse line {raw!r}")
            arg_diffs.setdefault(current, {}).setdefault(kind, []).extend(
                flags.split()
            )
            continue
        parts = raw.split(None, 1)
        svc = parts[0]
        rest = parts[1] if len(parts) > 1 else ""
        if rest.startswith("OPS-missing:"):
            ops_missing[svc] = _ops_line(rest[len("OPS-missing:"):])
            current = None
        elif rest.startswith("OPS-extra:"):
            ops_extra[svc] = _ops_line(rest[len("OPS-extra:"):])
            current = None
        else:
            if len(parts) != 2 or " " in rest:
                sys.exit(f"worklist: cannot parse line {raw!r}")
            current = (svc, rest)
            arg_diffs.setdefault(current, {})
    return arg_diffs, ops_missing, ops_extra


# ---------------------------------------------------------------------------
# Live-table helpers (mirroring extract-reference-surface.py conventions)
# ---------------------------------------------------------------------------


def _cli_names(arg_table) -> tuple[set[str], set[str]]:
    """Split an arg table into (--flag names, positional names)."""
    flags: set[str] = set()
    positionals: set[str] = set()
    for name, arg in (arg_table or {}).items():
        cli_name = getattr(arg, "cli_name", None) or f"--{name}"
        if getattr(arg, "positional_arg", False):
            positionals.add(str(cli_name))
        else:
            flags.add(str(cli_name))
    return flags, positionals


def _subcommand_table(command):
    table = getattr(command, "subcommand_table", None)
    if table is None:
        getter = getattr(command, "_get_command_table", None)
        if getter is not None:
            table = getter()
    return table or {}


def _collect_leaves(command, prefix: str, depth: int, out: dict[str, list[str]]):
    """Record leaf arg lists keyed by space-joined CLI path (flags + bare
    positionals in one sorted list, exactly as the arg renders)."""
    sub = _subcommand_table(command) if depth < MAX_DEPTH else {}
    if sub:
        for name in sorted(sub):
            _collect_leaves(sub[name], f"{prefix} {name}".strip(), depth + 1, out)
        return
    flags, positionals = _cli_names(getattr(command, "arg_table", None))
    out[prefix] = sorted(flags | positionals)


# ---------------------------------------------------------------------------
# Worklist OPS-line coverage checks
# ---------------------------------------------------------------------------


def _check_ops_line(svc, tokens, truncated, names, wait_covered, problems, label):
    """Verify a worklist OPS-missing/OPS-extra token stream is fully explained
    by the emitted `names` plus waiter coverage. Reports, never papers over."""
    matched: set[str] = set()
    wait_seen = 0
    i = 0
    while i < len(tokens):
        if tokens[i] == "wait" and i + 1 < len(tokens):
            if wait_covered(tokens[i + 1]):
                wait_seen += 1
            else:
                problems.append(
                    f"{label} {svc}: 'wait {tokens[i + 1]}' not covered by "
                    f"the waiters catalogue"
                )
            i += 2
            continue
        for k in (3, 2, 1):
            cand = " ".join(tokens[i : i + k])
            if cand in names:
                matched.add(cand)
                i += k
                break
        else:
            problems.append(f"{label} {svc}: worklist entry {tokens[i]!r} not emitted")
            i += 1
    leftover = names - matched
    if truncated is not None:
        if len(leftover) != truncated:
            problems.append(
                f"{label} {svc}: worklist truncated '+{truncated}' but "
                f"{len(leftover)} additional entries emitted: {sorted(leftover)}"
            )
    elif leftover:
        problems.append(
            f"{label} {svc}: emitted entries not in worklist: {sorted(leftover)}"
        )
    return wait_seen


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--no-merge",
        action="store_true",
        help=(
            "Rebuild from scratch instead of merging into an existing output. "
            "The default merges because the worklist (docs/remaining-divergences.txt) "
            "shrinks as divergences are fixed: a re-run scoped to the residual "
            "worklist would silently drop every previously-extracted patch. Only "
            "use --no-merge with a worklist regenerated against a build that has "
            "custom-surface data disabled."
        ),
    )
    args = parser.parse_args()

    arg_diffs, wl_ops_missing, wl_ops_extra = parse_worklist(WORKLIST)
    corpus = json.loads(CORPUS.read_text())["services"]
    customizations = json.loads(CUSTOMIZATIONS.read_text())

    driver = load_reference()

    # Importable only after load_reference() puts the reference install on
    # sys.path. awscli v2 vendors botocore under `awscli.botocore` AND aliases
    # it to top-level `botocore`, producing two DISTINCT module instances; the
    # loader raises exception classes from the `botocore.exceptions` instance,
    # so waiter-file misses must be caught from there.
    import awscli.botocore as vendored_botocore  # noqa: E402
    from awscli.botocore import xform_name  # noqa: E402
    from awscli.clidriver import ServiceOperation  # noqa: E402
    from botocore.exceptions import DataNotFoundError  # noqa: E402

    def naive_kebab(name: str) -> str:
        """botocore's xform_name('-') algorithm WITHOUT the seeded
        _xform_cache -- i.e. what a faithful regex-only port derives.
        Regexes are taken from the vendored module, not retyped."""
        special = vendored_botocore._special_case_transform.search(name)
        if special is not None:
            matched = special.group()
            name = name[: -len(matched)] + "-" + matched.lower()
        s1 = vendored_botocore._first_cap_regex.sub(r"\1-\2", name)
        return vendored_botocore._end_cap_regex.sub(r"\1-\2", s1).lower()

    session = driver.session
    loader = session.get_component("data_loader")
    command_table = driver._get_command_table()

    problems: list[str] = []
    notes: list[str] = []

    # Already-handled command-table mutations (data/customizations.json):
    # subtracted so this overlay never duplicates them.
    handled_removed = {
        svc: set(ops) for svc, ops in customizations["removed_operations"].items()
    }
    handled_rename_old: dict[str, set[str]] = {}
    handled_new_names: dict[str, set[str]] = {}
    for svc, renames in customizations["operation_renames"].items():
        handled_rename_old.setdefault(svc, set()).update(renames)
        handled_new_names.setdefault(svc, set()).update(renames.values())
    for svc, aliases in customizations["operation_aliases"].items():
        handled_new_names.setdefault(svc, set()).update(aliases.values())
    at = customizations["agent_toolkit"]

    # --- Walk every modeled service once. --------------------------------
    modeled_kebab: dict[str, set[str]] = {}
    live_tables: dict[str, dict] = {}
    xform_divergent: dict[str, dict[str, str]] = {}  # svc -> {actual: naive}
    waiters: dict[str, dict[str, str]] = {}
    colliding_customs: list[str] = []

    for cli_name, command in sorted(command_table.items()):
        botocore_name = getattr(command, "_service_name", None)
        if not botocore_name:
            continue  # model-less top-level command: out of scope
        model = loader.load_service_model(botocore_name, "service-2")
        ops = set(model["operations"])
        modeled = {xform_name(op, "-") for op in ops}
        modeled_kebab[cli_name] = modeled
        table = _subcommand_table(command)
        live_tables[cli_name] = table

        for op in sorted(ops):
            actual, naive = xform_name(op, "-"), naive_kebab(op)
            if actual != naive:
                xform_divergent.setdefault(cli_name, {})[actual] = naive
                if actual not in table:
                    problems.append(
                        f"xform special case {cli_name} {actual}: not in live table"
                    )

        try:
            wmodel = loader.load_service_model(botocore_name, "waiters-2")
        except DataNotFoundError:
            wmodel = None
        if wmodel:
            waiters[cli_name] = {
                xform_name(wname, "-"): xform_name(wconf["operation"], "-")
                for wname, wconf in wmodel["waiters"].items()
            }

        for name, sub in table.items():
            if name in modeled and not isinstance(sub, ServiceOperation):
                colliding_customs.append(f"{cli_name} {name} ({type(sub).__name__})")

    # --- PART 1: modeled_arg_patches -------------------------------------
    modeled_arg_patches: dict[str, dict[str, dict[str, list[str]]]] = {}
    for (svc, op), diff in sorted(arg_diffs.items()):
        table = live_tables.get(svc)
        if table is None or op not in table:
            problems.append(f"arg patch {svc} {op}: not found in live command table")
            continue
        flags, positionals = _cli_names(getattr(table[op], "arg_table", None))
        live = flags | positionals
        corpus_op = corpus.get(svc, {}).get("operations", {}).get(op)
        if corpus_op is None:
            problems.append(f"arg patch {svc} {op}: not in golden corpus")
        else:
            corpus_args = set(corpus_op["arguments"]) | set(
                corpus_op.get("positional_arguments", [])
            )
            if corpus_args != live:
                problems.append(
                    f"arg patch {svc} {op}: live table != corpus "
                    f"(live-only {sorted(live - corpus_args)}, "
                    f"corpus-only {sorted(corpus_args - live)})"
                )
        add: list[str] = []
        remove: list[str] = []
        for flag in diff.get("missing", []):
            if flag in live:
                add.append(flag)
            elif flag.startswith("--") and flag[2:] in positionals:
                add.append(flag[2:])  # rendered as a positional: record bare
            else:
                problems.append(
                    f"arg patch {svc} {op}: worklist missing-flag {flag} "
                    f"NOT in live arg table; not emitted"
                )
        for flag in diff.get("extra", []):
            if flag not in live:
                remove.append(flag)
            else:
                problems.append(
                    f"arg patch {svc} {op}: worklist extra-flag {flag} "
                    f"IS in live arg table; not emitted"
                )
        if add or remove:
            modeled_arg_patches.setdefault(svc, {})[op] = {
                "add": sorted(add),
                "remove": sorted(remove),
            }

    # --- PART 2 + replaced_operations -------------------------------------
    custom_commands: dict[str, dict[str, list[str]]] = {}
    replaced_operations: dict[str, list[str]] = {}

    for svc, table in sorted(live_tables.items()):
        modeled = modeled_kebab[svc]
        new_ok = handled_new_names.get(svc, set())
        if svc == "agent-toolkit":
            new_ok = new_ok | set(at["renames"].values())

        # (a) table entries under non-model names (BasicCommands, wizards,
        #     renamed ServiceOperations like rds add-option-to-option-group).
        candidates = [
            n
            for n in sorted(table)
            if n not in modeled and n != "wait" and n not in new_ok
        ]
        # (b) modeled names the plain-xform port cannot derive.
        candidates += sorted(xform_divergent.get(svc, {}))
        # (c) worklist entries not otherwise caught (botocore-modeled ops
        #     absent from Smithy, e.g. the deprecated s3api lifecycle quartet).
        for tok in wl_ops_missing.get(svc, ([], None))[0]:
            if tok in table and tok not in candidates and tok != "wait":
                candidates.append(tok)
                notes.append(
                    f"custom {svc} {tok}: modeled in botocore but listed "
                    f"OPS-missing (absent from the Smithy dialect)"
                )

        leaves: dict[str, list[str]] = {}
        for name in candidates:
            try:
                _collect_leaves(table[name], name, 1, leaves)
            except Exception as exc:  # loud, but keep going
                problems.append(f"custom {svc} {name}: {type(exc).__name__}: {exc}")
        if leaves:
            custom_commands[svc] = leaves

        # Deleted modeled operations not already handled elsewhere.
        deleted = modeled - set(table) - handled_removed.get(svc, set())
        deleted -= handled_rename_old.get(svc, set())
        if svc == "agent-toolkit":
            deleted -= modeled - set(at["modeled_allowlist"])
            deleted -= set(at["renames"])
        for op in sorted(deleted):
            replaced_operations.setdefault(svc, []).append(op)
            notes.append(f"replaced {svc} {op}: deleted by a customization")
        # Naive spellings the port derives for special-cased names.
        for actual, naive in sorted(xform_divergent.get(svc, {}).items()):
            replaced_operations.setdefault(svc, []).append(naive)
        if svc in replaced_operations:
            replaced_operations[svc] = sorted(set(replaced_operations[svc]))

    # --- Verification ------------------------------------------------------

    # custom command args must match the golden corpus exactly.
    for svc, cmds in custom_commands.items():
        corpus_ops = corpus.get(svc, {}).get("operations", {})
        for path, cli_args in cmds.items():
            entry = corpus_ops.get(path)
            if entry is None:
                problems.append(f"custom {svc} {path}: not in golden corpus")
                continue
            expect = sorted(
                set(entry["arguments"]) | set(entry.get("positional_arguments", []))
            )
            if expect != cli_args:
                problems.append(
                    f"custom {svc} {path}: args != corpus "
                    f"(ours-only {sorted(set(cli_args) - set(expect))}, "
                    f"corpus-only {sorted(set(expect) - set(cli_args))})"
                )

    # arg-patch adds/removes vs corpus (approximates raw-model + patch).
    for svc, ops in modeled_arg_patches.items():
        for op, patch in ops.items():
            entry = corpus.get(svc, {}).get("operations", {}).get(op, {})
            corpus_args = set(entry.get("arguments", [])) | set(
                entry.get("positional_arguments", [])
            )
            for f in patch["add"]:
                if f not in corpus_args and f"--{f}" not in corpus_args:
                    problems.append(f"arg patch {svc} {op}: add {f} not in corpus")
            for f in patch["remove"]:
                if f in corpus_args:
                    problems.append(f"arg patch {svc} {op}: remove {f} in corpus")

    # worklist OPS lines fully explained?
    for svc, (tokens, truncated) in sorted(wl_ops_missing.items()):
        names = set(custom_commands.get(svc, {}))
        _check_ops_line(
            svc,
            tokens,
            truncated,
            names,
            lambda w, s=svc: w in waiters.get(s, {}),
            problems,
            "OPS-missing",
        )
    for svc, (tokens, truncated) in sorted(wl_ops_extra.items()):
        names = set(replaced_operations.get(svc, []))
        _check_ops_line(
            svc,
            tokens,
            truncated,
            names,
            # An extra Smithy-derived waiter is covered iff botocore truly
            # lacks it: the port adopts this waiters catalogue wholesale.
            lambda w, s=svc: w not in waiters.get(s, {}),
            problems,
            "OPS-extra",
        )

    # custom commands found in services with NO worklist OPS-missing line at
    # all: the live machinery knows more than the worklist. Reported loudly
    # (but emitted -- they are verified reference truth), not exit-failing:
    # the harness may simply not compare that service (e.g. no Smithy model).
    worklist_gaps: list[str] = []
    for svc in sorted(set(custom_commands) - set(wl_ops_missing)):
        worklist_gaps.append(
            f"custom {svc}: not in worklist at all, emitted "
            f"{sorted(custom_commands[svc])}"
        )
    for svc in sorted(set(replaced_operations) - set(wl_ops_extra)):
        worklist_gaps.append(
            f"replaced {svc}: not in worklist at all, emitted "
            f"{replaced_operations[svc]}"
        )

    # every 'wait' entry in the golden corpus must be reproduced by waiters.
    corpus_wait_mismatch = 0
    for svc, entry in corpus.items():
        corpus_waits = {
            k.split(" ", 1)[1] for k in entry.get("operations", {}) if k.startswith("wait ")
        }
        ours = set(waiters.get(svc, {}))
        if corpus_waits != ours:
            corpus_wait_mismatch += 1
            problems.append(
                f"waiters {svc}: corpus has {sorted(corpus_waits - ours)} "
                f"extra, we have {sorted(ours - corpus_waits)} extra"
            )

    dyn = waiters.get("dynamodb", {})
    dynamodb_ok = (
        dyn.get("table-exists") == "describe-table"
        and dyn.get("table-not-exists") == "describe-table"
    )
    if not dynamodb_ok:
        problems.append(f"waiters dynamodb: table-exists spot check failed: {dyn}")

    # Merge with the existing output unless told otherwise. The worklist only names
    # *current* divergences, so a fresh run knows nothing about patches extracted in
    # earlier runs; without the merge they would be silently dropped.
    if not args.no_merge and args.output.exists():
        with open(args.output, encoding="utf-8") as fh:
            prior = json.load(fh)

        for svc, ops in prior.get("modeled_arg_patches", {}).items():
            for op, patch in ops.items():
                mine = modeled_arg_patches.setdefault(svc, {}).setdefault(
                    op, {"add": [], "remove": []}
                )
                mine["add"] = sorted(set(mine["add"]) | set(patch.get("add", [])))
                mine["remove"] = sorted(set(mine["remove"]) | set(patch.get("remove", [])))
        for svc, cmds in prior.get("custom_commands", {}).items():
            for cmd, arg_list in cmds.items():
                # Live extraction wins for commands seen this run; prior fills gaps.
                custom_commands.setdefault(svc, {}).setdefault(cmd, arg_list)
        for svc, ws in prior.get("waiters", {}).items():
            for waiter, op in ws.items():
                waiters.setdefault(svc, {}).setdefault(waiter, op)
        for svc, ops in prior.get("replaced_operations", {}).items():
            merged = set(replaced_operations.get(svc, [])) | set(ops)
            replaced_operations[svc] = sorted(merged)

    payload = {
        "_comment": (
            "Generated by scripts/extract-custom-surface.py from the awscli v2 "
            "reference install. Do not hand-edit. modeled_arg_patches: per "
            "modeled operation, CLI flags the reference's customization hooks "
            "add/remove relative to plain model derivation (positionals appear "
            "as bare names); scope is exactly the verified worklist in "
            "docs/remaining-divergences.txt. custom_commands: reference-only "
            "commands with their full argument lists ('--' flags, bare "
            "positionals; nested commands use space-joined keys). waiters: the "
            "botocore waiters-2 catalogue, waiter-cli-name -> underlying "
            "operation-cli-name; replaces Smithy waiter derivation wholesale. "
            "replaced_operations: operation names the port derives but the "
            "reference does not expose (customization deletions and the naive "
            "regex spellings of botocore _xform_cache special cases)."
        ),
        "awscli_version": driver.session.user_agent_version,
        "modeled_arg_patches": modeled_arg_patches,
        "custom_commands": custom_commands,
        "waiters": waiters,
        "replaced_operations": replaced_operations,
    }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with open(args.output, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, indent=2, sort_keys=True)
        fh.write("\n")

    # --- Summary -----------------------------------------------------------
    n_patch_ops = sum(len(v) for v in modeled_arg_patches.values())
    n_add = sum(len(p["add"]) for v in modeled_arg_patches.values() for p in v.values())
    n_rem = sum(len(p["remove"]) for v in modeled_arg_patches.values() for p in v.values())
    n_custom = sum(len(v) for v in custom_commands.values())
    n_waiters = sum(len(v) for v in waiters.values())
    n_replaced = sum(len(v) for v in replaced_operations.values())
    n_xform = sum(len(v) for v in xform_divergent.values())

    p = lambda *a: print(*a, file=sys.stderr)  # noqa: E731
    p(f"arg patches       {n_patch_ops} ops in {len(modeled_arg_patches)} services "
      f"(+{n_add} / -{n_rem} flags)")
    p(f"custom commands   {n_custom} in {len(custom_commands)} services")
    p(f"waiters           {n_waiters} in {len(waiters)} services "
      f"(corpus 'wait' mismatch: {corpus_wait_mismatch})")
    p(f"replaced ops      {n_replaced} in {len(replaced_operations)} services "
      f"({n_xform} from xform special cases)")
    p(f"autoscaling       waiters-2: "
      f"{'ABSENT (no wait command in reference)' if 'autoscaling' not in waiters else sorted(waiters['autoscaling'])}")
    p(f"dynamodb waiters  table-exists/table-not-exists -> describe-table: "
      f"{'OK' if dynamodb_ok else 'FAILED'}")
    p(f"colliding customs (modeled name replaced in place; patched only if "
      f"worklisted): {len(colliding_customs)}")
    for c in colliding_customs:
        p(f"                  {c}")
    if notes:
        p(f"notes             {len(notes)}")
        for n in notes:
            p(f"                  {n}")
    if worklist_gaps:
        p(f"WORKLIST GAPS     {len(worklist_gaps)} (live machinery beyond worklist)")
        for g in worklist_gaps:
            p(f"                  {g}")
    if problems:
        p(f"PROBLEMS          {len(problems)}")
        for pr in problems:
            p(f"                  {pr}")
    p(f"wrote             {args.output} ({os.path.getsize(args.output):,} bytes)")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
