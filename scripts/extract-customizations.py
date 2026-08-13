#!/usr/bin/env python3
"""Extract awscli v2's pure-data surface customizations into a JSON overlay.

Why this exists
---------------
The reference CLI mutates its model-derived surface through a handful of
customization hooks: argument renames (``argrename.py``), removed operations
(``removals.py``), and operation renames/aliases (``signin.py``,
``cloudwatch.py``, ``agenttoolkit/__init__.py``). The Rust port applies these
as data; hand-copying risks transcription errors, so everything here is pulled
out of the reference install programmatically:

* ``argument_renames`` / ``hidden_argument_aliases``: the module-level dicts
  ``ARGUMENT_RENAMES`` and ``HIDDEN_ALIASES`` from ``argrename.py``, dumped
  verbatim after import. Keys are ``<service>.<operation>.<old-cli-arg>``
  where service/operation may be the wildcard ``'*'``.
* ``removed_operations``: ``removals.py`` only expresses its table as
  ``register()`` calls, so ``register_removals`` is driven against a recording
  event handler and each registered hook is invoked with a command table whose
  ``__delitem__`` records the deletion. Keyed by the CLI service name (the
  ``building-command-table.<service>`` event suffix).
* ``operation_renames`` / ``operation_aliases``: the ``signin.py`` and
  ``cloudwatch.py`` hooks are likewise captured by registering them into a
  recorder, then invoking each handler against a synthetic command table
  seeded with the service's real modeled operation names (so conditional
  ``if name in command_table`` branches behave exactly as in the real CLI).
  Old-name-gone => rename; old name kept but marked ``_UNDOCUMENTED`` =>
  hidden alias (both classifications are asserted, not assumed).
* ``agent_toolkit``: the module-level ``MODELED_COMMAND_ALLOWLIST`` and
  ``MODELED_COMMAND_RENAMES`` from ``agenttoolkit/__init__.py``, dumped
  verbatim after import. The customization deletes every modeled command NOT
  in the allowlist, then renames the survivors; these renames are recorded
  here only (not duplicated into ``operation_renames``).

Direction conventions (verified against ``customizations/utils.py``):

* ``hidden_argument_aliases``: key = the documented argument name that exists
  in the built argument table; value = the extra undocumented alias created by
  ``make_hidden_alias`` (the *copy* gets ``_UNDOCUMENTED = True``). Both
  spellings parse; only the key is documented.
* ``operation_aliases``: key = the original (modeled) operation name, which
  still parses but is marked undocumented by ``alias_command``; value = the
  new documented name.
* ``operation_renames``: key = the original modeled name (gone afterwards);
  value = the replacement.

Usage:  scripts/extract-customizations.py [--output PATH]
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
DEFAULT_OUTPUT = REPO_ROOT / "data" / "customizations.json"

_BUILDING_COMMAND_TABLE = "building-command-table."


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


class _Recorder:
    """Stands in for the event emitter; records register() calls."""

    def __init__(self):
        self.registrations: list[tuple[str, object]] = []

    def register(self, event_name, handler, *args, **kwargs):
        self.registrations.append((event_name, handler))


class _DeletionRecordingTable(dict):
    """A command table that records deletions instead of performing them."""

    def __init__(self):
        super().__init__()
        self.deleted: list[str] = []

    def __delitem__(self, key):
        self.deleted.append(key)


class _FakeCommand:
    """Synthetic command-table entry.

    ``origin`` survives both ``rename_command`` (object moved, ``.name``
    reassigned) and ``alias_command`` (object ``copy.copy``'d), letting us
    recover which modeled operation each final table entry came from.
    """

    def __init__(self, name):
        self.name = name
        self.origin = name


def _validate_arg_key(key: str, problems: list[str]) -> None:
    parts = key.split(".")
    if len(parts) != 3:
        problems.append(f"key not <service>.<operation>.<arg>: {key!r}")
        return
    for part in parts:
        if "*" in part and part != "*":
            problems.append(f"partial wildcard (only bare '*' expected): {key!r}")
        if not part:
            problems.append(f"empty component: {key!r}")


def _extract_removals(removals_module) -> dict[str, list[str]]:
    recorder = _Recorder()
    removals_module.register_removals(recorder)

    removed: dict[str, set[str]] = {}
    for event_name, handler in recorder.registrations:
        if not event_name.startswith(_BUILDING_COMMAND_TABLE):
            raise AssertionError(f"unexpected removals event: {event_name}")
        service = event_name[len(_BUILDING_COMMAND_TABLE):]
        table = _DeletionRecordingTable()
        handler(command_table=table)
        removed.setdefault(service, set()).update(table.deleted)
    return {svc: sorted(ops) for svc, ops in removed.items()}


def _modeled_cli_op_names(driver, xform_name, cli_service: str) -> list[str]:
    """Real modeled operation names for a CLI service, in CLI (kebab) form."""
    command = driver._get_command_table()[cli_service]
    botocore_name = command._service_name
    loader = driver.session.get_component("data_loader")
    model = loader.load_service_model(botocore_name, "service-2")
    return sorted(xform_name(op, "-") for op in model["operations"])


def _extract_command_mutations(driver, xform_name, register_fn):
    """Run a building-command-table hook against a synthetic table.

    Returns ``{service: {"renames": {old: new}, "aliases": {old: new}}}``.
    """
    recorder = _Recorder()
    register_fn(recorder)

    results: dict[str, dict[str, dict[str, str]]] = {}
    for event_name, handler in recorder.registrations:
        if not event_name.startswith(_BUILDING_COMMAND_TABLE):
            # e.g. agenttoolkit's before-sign hook; not a surface mutation.
            continue
        service = event_name[len(_BUILDING_COMMAND_TABLE):]
        modeled = _modeled_cli_op_names(driver, xform_name, service)
        table: dict[str, _FakeCommand] = {n: _FakeCommand(n) for n in modeled}
        original = set(table)
        handler(command_table=table)

        renames: dict[str, str] = {}
        aliases: dict[str, str] = {}
        for final_name, cmd in table.items():
            if cmd.origin == final_name:
                continue
            if final_name in original:
                raise AssertionError(
                    f"{service}: new name {final_name!r} collides with a "
                    f"modeled operation"
                )
            if cmd.origin in table:
                # Original entry survived: hidden alias. alias_command marks
                # the ORIGINAL undocumented; the copy is the documented name.
                if not getattr(table[cmd.origin], "_UNDOCUMENTED", False):
                    raise AssertionError(
                        f"{service}: {cmd.origin!r} kept but not marked "
                        f"_UNDOCUMENTED; not a hidden alias?"
                    )
                aliases[cmd.origin] = final_name
            else:
                renames[cmd.origin] = final_name
        entry = results.setdefault(service, {"renames": {}, "aliases": {}})
        entry["renames"].update(renames)
        entry["aliases"].update(aliases)
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()

    driver = load_reference()

    # Importable only after load_reference() puts the reference install on
    # sys.path. NOTE: awscli v2 loads its vendored botocore under two module
    # identities (`awscli.botocore.*` and top-level `botocore.*`); anything
    # that must catch loader exceptions has to import them from the `botocore`
    # instance. Here every model load is for a service known to exist, so
    # failures are left to propagate loudly.
    from awscli.botocore import xform_name  # noqa: E402
    from awscli.customizations import (  # noqa: E402
        agenttoolkit,
        argrename,
        cloudwatch,
        removals,
        signin,
    )

    # --- 1. argrename.py: module-level dicts, dumped verbatim. -------------
    argument_renames = dict(argrename.ARGUMENT_RENAMES)
    hidden_argument_aliases = dict(argrename.HIDDEN_ALIASES)
    key_problems: list[str] = []
    for key in (*argument_renames, *hidden_argument_aliases):
        _validate_arg_key(key, key_problems)

    # --- 2. removals.py: instrument register_removals. ----------------------
    removed_operations = _extract_removals(removals)

    # --- 3. signin.py / cloudwatch.py: observed against synthetic tables. ---
    operation_renames: dict[str, dict[str, str]] = {}
    operation_aliases: dict[str, dict[str, str]] = {}
    for register_fn in (
        signin.register_rename_signin_commands,
        cloudwatch.register_rename_otel_commands,
    ):
        for service, entry in _extract_command_mutations(
            driver, xform_name, register_fn
        ).items():
            if entry["renames"]:
                operation_renames.setdefault(service, {}).update(entry["renames"])
            if entry["aliases"]:
                operation_aliases.setdefault(service, {}).update(entry["aliases"])

    # --- 4. agenttoolkit: module-level data, dumped verbatim. ---------------
    agent_toolkit = {
        "modeled_allowlist": sorted(agenttoolkit.MODELED_COMMAND_ALLOWLIST),
        "renames": dict(agenttoolkit.MODELED_COMMAND_RENAMES),
    }
    for old in agenttoolkit.MODELED_COMMAND_RENAMES:
        if old not in agenttoolkit.MODELED_COMMAND_ALLOWLIST:
            key_problems.append(f"agent-toolkit rename source not allowlisted: {old}")

    payload = {
        "_comment": (
            "Generated by scripts/extract-customizations.py from the awscli "
            "v2 reference install. Do not hand-edit. Directions: "
            "argument_renames maps '<service>.<operation>.<old-cli-arg>' -> "
            "new arg name (old gone; '*' wildcards allowed for service/op). "
            "hidden_argument_aliases: key = the documented argument name "
            "present in the built argument table, value = the extra "
            "UNDOCUMENTED alias created via utils.make_hidden_alias (the "
            "copied argument gets _UNDOCUMENTED=True; both spellings parse). "
            "removed_operations: CLI service -> operations deleted from the "
            "command table. operation_renames: old modeled op -> new name "
            "(old gone, via utils.rename_command). operation_aliases: old "
            "modeled op (still parses, marked undocumented via "
            "utils.alias_command) -> new documented name. agent_toolkit: the "
            "agent-toolkit customization deletes every modeled command NOT "
            "in modeled_allowlist, then applies renames (hard renames; not "
            "duplicated into operation_renames)."
        ),
        "awscli_version": driver.session.user_agent_version,
        "argument_renames": argument_renames,
        "hidden_argument_aliases": hidden_argument_aliases,
        "removed_operations": removed_operations,
        "operation_renames": operation_renames,
        "operation_aliases": operation_aliases,
        "agent_toolkit": agent_toolkit,
    }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with open(args.output, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, indent=2, sort_keys=True)
        fh.write("\n")

    total_removed = sum(len(v) for v in removed_operations.values())
    total_op_renames = sum(len(v) for v in operation_renames.values())
    total_op_aliases = sum(len(v) for v in operation_aliases.values())
    print(f"argument renames         {len(argument_renames)}", file=sys.stderr)
    print(f"hidden argument aliases  {len(hidden_argument_aliases)}", file=sys.stderr)
    print(
        f"removed operations       {total_removed} "
        f"across {len(removed_operations)} services",
        file=sys.stderr,
    )
    print(f"operation renames        {total_op_renames}", file=sys.stderr)
    print(f"operation aliases        {total_op_aliases}", file=sys.stderr)
    print(
        f"agent-toolkit            allowlist "
        f"{len(agent_toolkit['modeled_allowlist'])}, "
        f"renames {len(agent_toolkit['renames'])}",
        file=sys.stderr,
    )
    if key_problems:
        print(f"KEY PROBLEMS {len(key_problems)}", file=sys.stderr)
        for p in key_problems:
            print(f"             {p}", file=sys.stderr)
    print(f"wrote                    {args.output}", file=sys.stderr)
    return 1 if key_problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
