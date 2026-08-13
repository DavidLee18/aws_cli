#!/usr/bin/env python3
"""Extract the AWS CLI v2 user-facing command surface into a golden JSON corpus.

This drives the reference awscli's own ``CLIDriver`` so that the result reflects
every customization (renamed/removed/added commands, custom arguments such as
``--cli-input-json``, high-level commands like ``aws s3 cp``) rather than the raw
botocore service models.

Run with the reference interpreter:

    /opt/homebrew/Cellar/awscli/2.36.22/libexec/bin/python \
        scripts/extract-reference-surface.py

or with any python3 -- the script prepends the reference site-packages to
``sys.path`` itself.

Output is deterministic: sorted keys, sorted lists, no timestamps.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

# ---------------------------------------------------------------------------
# Reference install (READ-ONLY)
# ---------------------------------------------------------------------------
DEFAULT_AWSCLI_ROOT = "/opt/homebrew/Cellar/awscli/2.36.22"
DEFAULT_SITE_PACKAGES = os.path.join(
    DEFAULT_AWSCLI_ROOT, "libexec/lib/python3.14/site-packages"
)
DEFAULT_OUTPUT = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "tests/golden/reference-surface.json",
)

# Maximum command-tree depth to walk (aws -> service -> operation -> ...).
MAX_DEPTH = 4


def _bootstrap(site_packages: str) -> None:
    if site_packages and os.path.isdir(site_packages):
        if site_packages not in sys.path:
            sys.path.insert(0, site_packages)
    # Keep the extraction hermetic: no user config/credentials/plugins should be
    # able to change the command surface we record.
    os.environ["AWS_CONFIG_FILE"] = os.devnull
    os.environ["AWS_SHARED_CREDENTIALS_FILE"] = os.devnull
    os.environ.pop("AWS_PROFILE", None)
    os.environ.pop("AWS_DEFAULT_PROFILE", None)


def _cli_names(arg_table) -> tuple[list[str], list[str]]:
    """Split an arg table into (--flag names, positional names), both sorted."""
    flags: set[str] = set()
    positionals: set[str] = set()
    for name, arg in arg_table.items():
        cli_name = getattr(arg, "cli_name", None) or f"--{name}"
        if getattr(arg, "positional_arg", False):
            positionals.add(str(cli_name))
        else:
            flags.add(str(cli_name))
    return sorted(flags), sorted(positionals)


def _subcommand_table(command):
    """Return a command's subcommand table, or an empty dict."""
    table = getattr(command, "subcommand_table", None)
    if table is None:
        getter = getattr(command, "_get_command_table", None)
        if getter is not None:
            table = getter()
    return table or {}


def _walk(command, prefix: str, depth: int, out: dict, errors: list, service: str):
    """Recursively record leaf commands under ``out`` keyed by their CLI path."""
    try:
        arg_table = getattr(command, "arg_table", None) or {}
        sub_table = _subcommand_table(command) if depth < MAX_DEPTH else {}
    except Exception as exc:  # pragma: no cover - defensive
        errors.append(
            {"service": service, "command": prefix, "error": f"{type(exc).__name__}: {exc}"}
        )
        return

    if sub_table:
        for name in sorted(sub_table):
            child_path = f"{prefix} {name}".strip()
            try:
                child = sub_table[name]
            except Exception as exc:
                errors.append(
                    {
                        "service": service,
                        "command": child_path,
                        "error": f"{type(exc).__name__}: {exc}",
                    }
                )
                continue
            _walk(child, child_path, depth + 1, out, errors, service)
        return

    flags, positionals = _cli_names(arg_table)
    entry: dict = {"arguments": flags}
    if positionals:
        entry["positional_arguments"] = positionals
    out[prefix] = entry


def extract(site_packages: str) -> dict:
    _bootstrap(site_packages)

    from awscli import __version__ as awscli_version  # noqa: E402
    from awscli.clidriver import create_clidriver  # noqa: E402

    driver = create_clidriver()
    errors: list = []

    services: dict = {}
    command_table = driver._get_command_table()
    for service_name in sorted(command_table):
        try:
            service_command = command_table[service_name]
            operations: dict = {}
            _walk(service_command, "", 0, operations, errors, service_name)
            # A top-level command that is itself a leaf (e.g. `aws login`) has
            # no subcommands and records itself under the empty key. Key it by
            # its own name so its arguments are not lost.
            if "" in operations:
                operations[service_name] = operations.pop("")
            services[service_name] = {"operations": operations}
        except Exception as exc:
            errors.append(
                {
                    "service": service_name,
                    "command": service_name,
                    "error": f"{type(exc).__name__}: {exc}",
                }
            )
            services[service_name] = {"operations": {}}

    global_flags, _ = _cli_names(driver.arg_table)

    return {
        "awscli_version": awscli_version,
        "global_arguments": global_flags,
        "services": services,
        "errors": errors,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--site-packages", default=DEFAULT_SITE_PACKAGES)
    parser.add_argument("--output", default=DEFAULT_OUTPUT)
    args = parser.parse_args()

    corpus = extract(args.site_packages)

    os.makedirs(os.path.dirname(os.path.abspath(args.output)), exist_ok=True)
    with open(args.output, "w", encoding="utf-8") as fh:
        json.dump(corpus, fh, indent=2, sort_keys=True, ensure_ascii=False)
        fh.write("\n")

    n_services = len(corpus["services"])
    n_ops = sum(len(s["operations"]) for s in corpus["services"].values())
    n_args = sum(
        len(op["arguments"]) + len(op.get("positional_arguments", []))
        for s in corpus["services"].values()
        for op in s["operations"].values()
    )
    size = os.path.getsize(args.output)

    print(f"awscli version : {corpus['awscli_version']}", file=sys.stderr)
    print(f"services       : {n_services}", file=sys.stderr)
    print(f"operations     : {n_ops}", file=sys.stderr)
    print(f"arguments      : {n_args}", file=sys.stderr)
    print(f"global args    : {len(corpus['global_arguments'])}", file=sys.stderr)
    print(f"errors         : {len(corpus['errors'])}", file=sys.stderr)
    print(f"output         : {args.output} ({size:,} bytes)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
