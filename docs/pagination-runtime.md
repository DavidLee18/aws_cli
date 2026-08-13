# Pagination runtime semantics

Verified against awscli 2.36.22 source with live arg-table introspection (all 3,279
paginated operations scanned). This is the specification for the pagination half of the
args/runtime crates — the surface layer (which flags exist) is already conformant; this
records the behaviour behind them. File:line references are relative to
`/opt/homebrew/Cellar/awscli/2.36.22/libexec/.../awscli/`.

## Injection (already implemented in surface derivation)

- Qualification: the operation appears in the service's `paginators-1.json`. Nothing
  else — not `can_paginate`, no input-member validation (`customizations/paginate.py:136-143`).
  Keyed by botocore's `service_name` (data-dir name); our overlay keys by CLI name, which
  the extraction join makes equivalent.
- `--starting-token` always; `--max-items` always; `--page-size` iff `limit_key`
  (`paginate.py:155-205`). All three documented (unlike the shadowed model args).
- Injected values serialize into `parameters["PaginationConfig"]` under
  `StartingToken` / `PageSize` / `MaxItems` (`paginate.py:426-432`) — never as top-level
  API params.

## Types — the non-obvious part

`--page-size` takes the limit key's shape type, **and `--max-items` inherits it too**: the
`type_name` variable defaults to `integer` but is overwritten by the limit key's type and
never reset (`paginate.py:167-170, 195-205`). Consequences in 2.36.22 data:

- 52 ops have a `string` limit key (e.g. all `apigatewayv2 get-*`) → `--max-items` parses
  as **string** there.
- 2 ops have `long` (`kinesis-video-archived-media get-images`/`list-fragments`).
- `--page-size` carries the limit key shape's `min`/`max` metadata; `--max-items` does not
  (`paginate.py:371, 381-386`).
- Upstream latent bug, faithfully reproducible: non-positive `--max-items` triggers a
  stderr warning via `int(value)` (`paginate.py:428-429`) — on the string-typed ops a
  non-numeric `--max-items` raises an unhandled `ValueError`.

## Shadowing

Before injection, all `input_token`s **and** the `limit_key` are marked `_UNDOCUMENTED`
(`paginate.py:147, 303-318`) — hidden from help but still in the arg table and still
parseable. `output_token`/`result_key`/`more_results` untouched.

On name collision the injected `PageArgument` **replaces the model arg in place**,
keeping the original's position in the arg-table order (`paginate.py:236-244`); the
original is stashed in `shadowed_args`. Collisions in 2.36.22: `--max-items` 91 ops,
`--page-size` 67, `--starting-token` 11 (all `omics list-*`).

Special case: `rolesanywhere list-crls` (+3 siblings) has a model member `pageSize` but a
paginator with **no** `limit_key` — so `--page-size` there is a genuine documented API
parameter, not an injected flag. Surface-level it looks identical; behaviour differs.

## Auto-disable

Pagination silently turns off (and shadowed model args are restored into the table,
`paginate.py:280-281`) when:

- **Path A** (`operation-args-parsed`): any input-token/limit-key CLI arg is non-None,
  unless its py_name is in the whitelist `['start_token', 'max_items']`
  (`paginate.py:247-268`). That is `start_token`, **not** `starting_token` — an upstream
  typo with observable behaviour: on the 11 `omics list-*` ops whose input token is
  literally `startingToken`, passing `--starting-token` disables auto-pagination and
  sends the value as the raw API param. Reproduce the typo.
- **Path B** (`calling-command`): any *API-namespace* key in the call parameters matches
  an input token or limit key (`paginate.py:227-233, 352-361`) — this is why
  `--cli-input-json '{"NextToken": ...}'` disables pagination. Runs after call-parameter
  building; can only flip the flag, not restore args.
- `--no-paginate` + a **truthy**, non-shadowed pagination arg raises
  `ParamValidationError` (`paginate.py:284-300`). Truthiness, not `is not None`:
  `--no-paginate --max-items 0` does not error. And the check can't tell injected from
  genuine: `rolesanywhere list-crls --no-paginate --page-size 5` errors even though
  `--page-size` is a real API param there.

## Waiters

`wait <x>` subcommands run the same `_create_argument_table` path with the underlying
operation's model (`customizations/waiters.py:228-238`), so their arg tables are
**identical** to the operation's — same injection, same shadowing, same ordering. (Our
surface derivation copies the operation's args to waiter commands; correct.) The
pagination values are functionally dead there — `WaiterCaller` passes params straight to
the operation and a `PaginationConfig` key would be rejected — and waiter help suppresses
all option docs, but the flags parse. Service-specific hooks keyed on
`calling-command.<service>.<op>` do NOT fire for waiters (event is
`calling-command.wait.<name>`).

## Scope boundaries

- Custom `BasicCommand`s never receive injection — they emit `building-arg-table.<name>`
  (note: *arg*, not *argument*), a different event. `ddb select`/`scan` hand-roll their
  own pagination flags (`customizations/dynamodb/subcommands.py:132-158`).
- Service-specific pagination customizations (complete list): kinesis `list-streams`
  (extra undocumented arg + its own auto-disable), ec2 `describe-volumes`/`describe-snapshots`
  (default `PageSize=1000` at call time under whitelist conditions,
  `customizations/ec2/paginate.py`), dynamodb (base64 `LastEvaluatedKey` fixup after
  call). None affect the arg table beyond kinesis's undocumented mark.
- Arg-table order (matters for help output, not parsing): cli-input args land before page
  args, `--generate-cli-skeleton` after; more-specific event handlers run before generic
  ones.
