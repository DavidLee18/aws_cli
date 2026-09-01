# Known divergences

Every divergence the conformance report prints, traced to its cause in the reference CLI.
Regenerate with `cargo run -p aws-cli-conformance`.

## Current state: ZERO surface divergence, and a deliberate superset

| | |
|---|---|
| Operations compared | 19,452 |
| Argument sets matching exactly | **19,452 (100.00%)** |
| Operations the reference has and we lack | **0** |
| Services fully conformant | **427 of 427 compared** |
| Operations we expose that the reference hides, by design | 22 |
| Excluded models (in aws-sdk-rust, not shipped by the CLI) | 4 |
| Corpus services with no aws-sdk-rust model | 11 |

Nothing the reference offers is missing, and every argument set matches. The 22 extras are
the event-stream operations botocore removes only because it cannot decode
`vnd.amazon.eventstream` — `lambda invoke-with-response-stream`, `kinesis
subscribe-to-shard`, `bedrock-runtime converse-stream` and the rest. We can decode it, so
we keep them; they are listed in `customizations::EVENT_STREAM_OPERATIONS`.

Three gates, and it is worth being clear about what each one does *not* cover:

- `conformance_does_not_regress` counts diverging **argument sets** only
  (`MAX_DIVERGING_OPERATIONS = 0`, `MIN_EXACT_ARG_RATIO = 1.0`).
- `no_operations_are_missing_or_unexpected` covers whole operations, which the first gate
  cannot see: it only compares operations present on both sides, so a command could vanish
  entirely and leave it green. Extras are permitted only where they *equal* the
  event-stream table — a real extra cannot hide inside the exemption, and a table entry
  that stops being derived is caught too.
- The report binary (`cargo run -p aws-cli-conformance`) prints the extras separately and
  exits zero when nothing else diverges, so it is safe to gate CI on.

New divergence means a regression or upstream drift after refetching; fix or regenerate
the data files rather than raising a gate.

Surface derivation pipeline (all inputs generated from the reference, none hand-edited):

1. Generic model derivation — members via `xform_name`, boolean `--no-` pairs, EC2
   single-bool-struct pull-up, streaming-blob `outfile` + universal-flag suppression,
   universal + pagination injections (`data/paginators.json`)
2. Waiters from botocore's catalogue (`data/custom-surface.json .waiters`) with
   pre-patch args
3. Removals, renames, aliases (`data/customizations.json`), then per-op patches
   (`data/custom-surface.json .modeled_arg_patches`)
4. Custom `BasicCommand`s (`data/custom-surface.json .custom_commands`)

Still unmeasured by this harness: the 8 model-less top-level commands (`s3`, `configure`,
`ddb`, `history`, `login`, `logout`, `update`, `cli-dev`), argument *types*/shorthand
behaviour (needs the `awsc` binary), and `_UNDOCUMENTED` marks (corpus captures names
only). Of those eight, `s3` and `configure` are implemented and verified by direct
comparison against the reference binary instead — see "The `aws configure` tree" below.

---

## Runtime divergences (the `awsc` binary)

Surface conformance says nothing about execution. These are behaviours verified against
the reference while building the STS vertical slice.

### Matched

- **SigV4** — canonical request, string-to-sign and signature reproduce botocore
  byte-for-byte, pinned offline by `tests/golden/sigv4-sts-get-caller-identity.json`.
  Only `content-type`, `host`, `x-amz-date` (and `x-amz-security-token` when present)
  are signed; user-agent and the `amz-sdk-*` retry headers are sent unsigned.
- **Request body** — `Action=GetCallerIdentity&Version=2011-06-15`, byte-identical.
- **Service error format** — `aws: [ERROR]: An error occurred (Code) when calling the
  Op operation: Message`, exit 254.
- **Exit codes** — the taxonomy in `awscli/constants.py:14-17`: 0 success, 252 parameter
  validation, 253 configuration, 254 client/service, 255 general. Note bare `awsc` with
  no arguments is **252**, while an explicit `help` is 0.
- **Missing region** — exit 253 with the reference's exact wording, *and* only for
  services that actually need one (see below).

- **Endpoint resolution** — `smithy.rules#endpointRuleSet` is fully interpreted, and the
  implementation passes **AWS's own conformance suite: 14,112 of 14,112 cases across all
  431 services** (`cargo test -p aws-cli-runtime --test endpoint_rules`). That covers
  dualstack, FIPS, global endpoints, per-partition DNS suffixes, S3 virtual-host vs
  path-style, and the auth-scheme overrides where the signing region differs from the
  endpoint region.

  Two things the rulesets do *not* encode, both taken from botocore instead:

  - **`aws.partition` data** — vendored from `partitions.json` into `data/partitions.json`.
  - **The no-region fallback.** With no region configured, botocore substitutes a
    service's `partitionEndpoint` from the legacy `endpoints.json` and only raises
    NoRegionError if the service has none (`botocore/regions.py:274-281`). This is why
    `sts get-caller-identity` succeeds with no region — resolving `aws-global` to
    `https://sts.amazonaws.com` while signing `us-east-1` — but `ec2 describe-regions`
    does not. 57 service/partition pairs across 7 partitions; kept per-partition because
    the pseudo-region genuinely differs (`aws-global` vs `aws-cn-global`).

  Not yet wired: service-specific ruleset parameters beyond the four universal builtins
  (S3 path-style/accelerate, `AWS::Auth::AccountIdEndpointMode`, `UseGlobalEndpoint`)
  take their declared defaults, which is what the reference uses absent explicit
  configuration.

- **Credential chain** — implemented: environment variables, static profile keys,
  **SSO** (both the `sso-session` and legacy inline forms), **`credential_process`**,
  **ECS/EKS container roles**, and **IMDSv2**. Verified end to end: a live
  `awsc sts get-caller-identity --profile <credential_process profile>` produces output
  **byte-identical** to the reference.

  Chain order follows botocore's `create_credential_resolver` exactly, including three
  behaviours that are easy to get backwards:

  - An **explicitly-selected profile removes the environment provider entirely**
    (`credentials.py:92` + `:151-171`). Without this, `--profile foo` would silently
    authenticate as whatever `AWS_ACCESS_KEY_ID` happened to be exported.
  - **`role_arn` outranks static keys in the same profile** — assume-role sits at chain
    position 2, ahead of every profile-based provider.
  - Static keys in `~/.aws/credentials` beat `credential_process`, which beats static
    keys in `~/.aws/config` (positions 5/7/8). The two files are therefore kept separate
    rather than merged, since a merged view cannot express that ordering.

  **assume-role** is implemented: `source_profile` (recursive, so role chaining works,
  guarded against cycles), `credential_source` (`Environment`/`EcsContainer`/
  `Ec2InstanceMetadata`, matched case-insensitively), and web identity (unsigned
  `AssumeRoleWithWebIdentity`). Session name defaults to `botocore-session-<epoch>` and is
  excluded from the cache key when generated. Results are cached in `~/.aws/cli/cache`
  using botocore's key — `sha1` over JSON with **Python's** `", "`/`": "` separators —
  so the cache is shared with the reference in both directions.

  **SSO token refresh** is implemented: a token inside the 15-minute window is renewed via
  unsigned `sso-oidc:CreateToken` against `sso_region` and written back to the shared
  cache.

  Both of the reference's distinct SSO failure messages are reproduced verbatim, because
  which one appears depends on the path: a locally-detected expiry gives botocore's
  `TokenRetrievalError` wording, while a portal 401/403 gives `UnauthorizedSSOTokenError`.

  Verified byte-identical to the reference (stdout, stderr and exit code) across five
  paths: assume-role failure, `credential_process` success, expired SSO, unknown profile,
  and rejected static credentials.

  Still outstanding: the initial SSO **login** flow (`aws sso login`; the cache is shared,
  so the reference's login works), `aws login` / `login_session`, and MFA input is not
  hidden the way the reference's `getpass` hides it.

### Outstanding

- **Protocols** — all six AWS wire protocols are implemented and verified
  **byte-identical against live AWS**: `awsQuery` (sts), `ec2Query` (ec2),
  `awsJson1_0` (dynamodb), `awsJson1_1` (logs), `restJson1` (lambda), `restXml` (s3api).
  `rpcv2Cbor` is refused by name.

  Three details that would silently corrupt requests if guessed:

  - **Request bodies are rendered with Python's `json.dumps` separators** (`", "` /
    `": "`), not compact JSON. The body is hashed into the SigV4 signature, so a compact
    encoding signs a different request.
  - **`targetPrefix` cannot be derived.** The Smithy models omit it; the service shape
    name matches for 149 of 152 awsJson services but not for `cloudtrail`,
    `codeconnections` or `codestar-connections`, which use a fully-qualified prefix.
    Vendored in `data/protocol-metadata.json`.
  - **S3 requires an explicit `x-amz-content-sha256` header**, unlike every other
    service, which accepts the payload hash appearing only inside the canonical request.

  Reproduced botocore behaviours that differ from the Smithy spec: `awsJson` ignores the
  `X-Amzn-Errortype` header (only `restJson1` reads it); `restXml` sends no
  `Content-Type`; error codes are normalised colon-first then hash-last. Blobs are NOT
  base64-decoded on output, because the CLI replaces botocore's decoder with `identity`.

- **Pagination** ✅ — auto-pagination for the 3,279 operations that need it, verified
  byte-identical live including `--no-paginate`, `--max-items`, `--page-size` and
  `--starting-token`.

  Details that are not guessable:

  - `non_aggregate_keys` come from the **first page only** and are emitted even when
    absent, which is why `s3api list-buckets` prints `"Prefix": null`.
  - Result keys accumulate **by type**: lists concatenate, integers **sum** (dynamodb
    `Count`), strings **concatenate** (rds `LogFileData`), maps and structures keep the
    first page and drop the rest.
  - Only `result_keys[0]` — the primary — counts against `--max-items` or gets truncated;
    secondary keys accumulate in full.
  - `NextToken` is **botocore's own token**, `base64(json.dumps({input_token: value, ...,
    boto_truncate_amount: n}))`, not the raw service token. Verified interoperable in both
    directions: the tokens are byte-identical and each CLI resumes from the other's.
  - `boto_truncate_amount` is how many items of the cut page were already returned; a
    resume skips exactly those, and offsets compose across successive resumes.

- **Argument layer** — implemented: the **shorthand parser** (a port of the
  recursive-descent grammar, including the backtracking that makes `foo=a,b,c=d` split
  into `foo=[a,b]` plus `c=d`), `file://`/`fileb://` expansion **before** shorthand or
  JSON parsing, the JSON short-circuit (a value starting with `[` or `{` disables
  shorthand entirely, with no fallback if the JSON then fails), model-driven scalar
  coercion, `--cli-input-json`/`--cli-input-yaml`, and `--generate-cli-skeleton input`.

  `--cli-input-*` is a **shallow, top-level-key-only, non-clobbering fill**: command-line
  arguments win, and a key an argument set discards the document's value wholesale rather
  than deep-merging.

  `--generate-cli-skeleton output` now works too, including the quirk that makes it a
  *checking* mode: the reference stubs the generated skeleton as the response and the
  stubber validates it against the output shape, so a placeholder that violates that
  shape's own constraints fails. `sts get-caller-identity --generate-cli-skeleton output`
  is exactly that — the generated `Arn: "Arn"` is 3 characters against a minimum of 20.

  Refused rather than approximated: `--generate-cli-skeleton yaml-input`, which annotates
  every member with `# [REQUIRED] <documentation>`. The two hardcoded shorthand
  back-compat cases (the `firehose`/`workspaces`/`elb` list-of-single-member expansion,
  and the `{"Value": x}` form) are also not ported.

- **Parameter validation** ✅ — required members, types, and minimum length/range, with
  every error collected rather than just the first.

  Two behaviours worth stating because they are counter-intuitive:

  - **Only `min` is ever checked, never `max`.** botocore's `range_check` tests the lower
    bound and returns; an over-long or over-large value goes to the service.
  - **Required flags are enforced at the argument-parsing layer**, before model
    validation, with argparse's wording (`the following arguments are required:
    --role-arn`) and a usage block — a different message from the model-level "Missing
    required parameter" that would otherwise fire. Suppressed when `--cli-input-*` or
    `--generate-cli-skeleton` is present, since those supply the parameters.

- **Retries** are not implemented (the reference defaults to `standard` mode).



- **Output formats** ✅ — all six (`json`, `text`, `table`, `yaml`, `yaml-stream`, `off`),
  verified byte-identical live across a 20-case format matrix.

  Non-obvious details reproduced:

  - **table**: the title is the **API** operation name (`GetCallerIdentity`), not the CLI
    spelling. The terminal width does not cap the table — it only decides whether a wide
    single-row section is reformatted into two-column `[header, value]` rows; after that
    the table renders at its natural width and can overflow. A dict with exactly one
    scalar key gets that vertical form with no header row.
  - **yaml**: keys sorted, sequence dashes **flush with the parent key** rather than
    indented under it, single quotes (with `''` doubling), timestamp-shaped strings
    quoted so they do not read back as dates, and plain scalars folded at the first space
    **past** column 80 with continuations at the scalar's own indent.

  `text` reproduces the unobvious rules: keys are never printed, scalars are emitted
  **sorted by key name** (not model order), a nested container is labelled with its own
  key **uppercased** with no depth encoding, a dict with no scalar members emits no label
  line at all, and scalars use Python's spellings (`True`/`False`/`None`). Empty
  containers emit nothing, not even a newline.

  **Known divergence:** the reference's text formatter is *unbuffered* — it formats each
  page separately and then formats the resume token as a pseudo-page. With
  `--output text --max-items N --query ...`, `--query` is applied to that pseudo-page too
  and prints a spurious `None` line. We merge pages first and format once, so we omit it.
  Everything else about `text` matches, including with `--query`.

- **`--query`** ✅ — JMESPath via the `jmespath` crate. Applied after the pagination merge
  and after `ResponseMetadata` removal, matching the reference's ordering, and validated
  at parse time so a bad expression fails before any API call.

  One compatibility shim: a JMESPath literal is `` `json` ``, so a string literal is
  strictly `` `"us-east-1"` ``. Python's implementation also accepts the unquoted
  `` `us-east-1` ``, which most published AWS CLI examples use. Literals whose contents
  are not valid JSON are quoted before compiling.

- **Global arguments** ✅ — every one the reference declares is now accepted. `--query`,
  `--no-sign-request`, `--cli-read-timeout`, `--cli-connect-timeout`, `--debug`,
  `--version`, plus `--color`/`--no-cli-pager`/`--no-cli-auto-prompt` accepted as genuine
  no-ops (we neither colour, page, nor prompt).

  Refused rather than ignored, because obeying the flag is the whole point of it:
  `--no-verify-ssl` and `--ca-bundle` (silently verifying when asked not to would
  misrepresent the request), and `--cli-auto-prompt` (answering as though it were absent
  would run a command the user expected to edit first).

- **`--cli-binary-format`** ✅ — and this was a live *correctness* bug, not just an absent
  flag. v2 defaults to `base64`: a blob argument is already base64 and is **decoded** on
  the way in. We were implementing v1's `raw-in-base64-out` unconditionally, so
  `kms encrypt --plaintext <base64>` and `lambda invoke --payload <base64>` both sent the
  base64 of the base64.

  Blob inputs are now normalised to base64 text at the argument layer — the reference's
  `Base64DecodeVisitor`, with the one change that a `serde_json::Value` cannot hold bytes,
  so the canonical form is the encoded text and the protocols that want raw bytes (CBOR,
  and a blob payload) decode it back. Three things fell out of it:

  - `fileb://` no longer corrupts binary. It was read through `String::from_utf8_lossy`,
    which turns every non-UTF-8 byte into U+FFFD; it is base64-encoded from the raw bytes
    now, and skipped by the normalisation exactly as the reference skips it (`fileb://`
    yields `bytes`, and its visitor only touches `str`).
  - A blob value no longer takes the JSON short-circuit. `--payload '{"a":1}'` is not a
    document to parse but a bad base64 string, and the reference says so —
    `Invalid base64: "{"a":1}"`, exit **255**, because that error matches no handler and
    falls through to the general one.
  - Streaming members are untouched: their argument is a path to send from, not a value.

  Verified by capturing the reference's own request bytes: `kms encrypt` in both formats,
  and `lambda invoke --payload` as `fileb://`, as literal base64, and as invalid base64,
  all reproduce byte for byte.

- **`--cli-error-format`** ✅ — all six styles (`legacy`, `json`, `yaml`, `text`, `table`,
  `enhanced`), resolved from the flag, then `AWS_CLI_ERROR_FORMAT`, then the profile's
  `cli_error_format`, then `enhanced`. All six verified byte-identical against the
  reference.

  Two things make it smaller than six styles suggests. The four data formats are the
  ordinary output formatters pointed at stderr with the pseudo-operation name `error`. And
  an error with no structured record — anything reaching the general handler — ignores the
  setting entirely and prints the plain line, which is why `--cli-error-format json` cannot
  swallow a general failure.

  `legacy` is *not* `enhanced` without the extras block: it prints the record's own
  message, where `enhanced` prints it behind the `An error occurred (Code): ` wrapper. The
  two agree only for a service error, where `ClientErrorHandler` overrides the fallback.

  Known shortfall: the reference also copies across fields the *error shape* models beyond
  code and message (`RetryAfterSeconds` and the like), which our error parser does not
  retain. Those fields are absent rather than wrong, and `enhanced`'s "Additional error
  details" block is therefore only ever emitted for records that do carry them.

  This also exposed a gap in the YAML formatter: it folded plain scalars at `best_width`
  but not quoted ones, so a long quoted message ran past column 80. The emitter applies the
  same rule to every quoting style, and it does now too.

Historical note: the six-model sample reported 96.9% — misleadingly high, because its
dominant divergence causes had already been fixed. Divergences cluster in services with
unported customizations, so ratios from partial coverage overstate conformance.

Reference paths below are relative to
`/opt/homebrew/Cellar/awscli/2.36.22/libexec/lib/python3.14/site-packages/awscli/`.

---

## Resolved

### Service command names are not derivable ✅ fixed

The `aws <command>` name is botocore's **data-directory name**
(`clidriver.py:491-500` → `botocore/loaders.py:260-290`), which Smithy models don't carry.
Measured against all 430 services:

| candidate rule | correct |
|---|---|
| `endpointPrefix` | 308/430 (71.6%) |
| `sdkId` lowercased, spaces removed | 273/430 (63.5%) |
| `sdkId` lowercased, spaces → hyphens | 378/430 (87.9%) |

`endpointPrefix` — our original choice — would have renamed `aws cloudwatch` to
`aws monitoring` and been wrong for 122 services. Fixed by generating an
`sdkId` → command mapping from the reference (`scripts/extract-service-names.py` →
`data/service-names.json`). `serviceId` is unique across all 430 services and equals
Smithy's `sdkId`, so the join is exact. This also captured the CLI-layer renames for free.

### Universal and pagination flags ✅ fixed

`--cli-input-json` / `--cli-input-yaml` (`customizations/cliinput.py:23-41`) and
`--generate-cli-skeleton` (`customizations/generatecliskeleton.py:27-67`) are injected into
every operation — they are **not** global args. Paginated operations additionally get
`--starting-token` / `--max-items` / `--page-size`
(`customizations/paginate.py:78-231`). Now derived from `smithy.api#paginated`.

### `wait` subcommands ✅ fixed

`customizations/waiters.py:25-44` injects a `wait` command per waiter, named
`xform_name(waiter_name, '-')`. Derived from `smithy.waiters#waitable`.

### Resource lifecycle operations ✅ fixed (found by the full sweep)

Smithy resources bind operations through dedicated lifecycle slots (`create`, `put`,
`read`, `update`, `delete`, `list`), not just `operations`/`collectionOperations`. The
loader skipped them, silently losing **2,249 operations** across the catalogue — invisible
in the six-model sample, whose services attach operations directly. Missing operations
dropped 2,356 → 107 when fixed.

### Smithy prelude shapes ✅ fixed (found by the full sweep)

`smithy.api#Unit`, `smithy.api#String`, `smithy.api#Boolean`, etc. are implicit prelude
shapes — referenced ~14,000 times across the catalogue, never defined in any model file.
Unresolved, they broke 32 models outright (`Unit`) and cost ~300 boolean flags their
`--no-` negative forms (`Boolean`/`PrimitiveBoolean`). The loader now injects the prelude
into the shape index; `Unit` is special-cased as "no input/output" instead.

---

## Outstanding — ordered by leverage

Full-catalogue flag-level attribution of the 227 diverging operations:

| cause | flag instances | fix |
|---|---|---|
| Streaming output (`outfile` + suppressed input flags) | 64 + 70×3 | item 2 |
| `argrename.py` families (`--version` 49, `--lorawan`, `--template-version`, …) | ~120 | item 1 |
| Per-service customizations (EC2 secgroup, bundle, `--count`, rekognition `--image-bytes`, …) | remainder | items 5–6 |

### 1. Argument renames — `customizations/argrename.py`

88 entries keyed `<service>.<operation>.<old-cli-arg>` → `<new>`, with `*` wildcards. The
old name is **removed**, not aliased. Accounts for the largest share of remaining
divergences and is pure data — the highest-leverage item on this list.

Observed in our diff:

| operation | we emit | reference | rename rule |
|---|---|---|---|
| `ec2 create-image` | `--no-no-reboot` | `--reboot` | `ec2.create-image.no-no-reboot` |
| `ec2 create-network-acl-entry` | `--no-egress` | `--ingress` | `ec2.*.no-egress` |
| `ec2 modify-instance-attribute` | — | `--no-disable-api-termination` | `ec2.*.no-disable-api-termination` → `enable-api-termination` |

Note the pattern: several rename the **auto-generated negative boolean form**, so renames
must be applied *after* `--no-` expansion.

Also worth porting alongside: `HIDDEN_ALIASES` (`argrename.py:117-122`, 4 entries) where
both names work — e.g. `--source-server-ids` ↔ `--source-server-i-ds`.

### 2. Streaming output ⇒ positional `outfile`, and no input flags

`customizations/streamingoutputarg.py:25-37` adds a **positional** `outfile` for
streaming-output operations. Critically, `cliinput.py:36-41` and
`generatecliskeleton.py:31-38` then *skip* those operations — so streaming ops have
neither `--cli-input-json` nor `--generate-cli-skeleton`.

Explains `lambda invoke` (missing `outfile`, and we wrongly add all three input flags) and
`logs start-live-tail`. Derivable from Smithy's `streaming` trait on the output member.

### 3. ~~Pagination metadata disagrees between botocore and Smithy~~ ✅ fixed

A genuine **model-dialect divergence** — botocore's `paginators-1.json` and Smithy's
`smithy.api#paginated` disagree about which operations paginate (e.g. botocore has no
paginator for `dynamodb list-imports` or `logs get-log-events` despite the Smithy trait,
and does paginate `dynamodb list-backups` where Smithy says nothing). Unresolvable from
Smithy alone; this was the real cost of the Smithy-models decision.

Fixed by vendoring botocore's paginator data as an overlay:
`scripts/extract-paginators.py` → `data/paginators.json` (353 services, 3,279 entries,
3,104 with `limit_key`, keyed by CLI names, configs verbatim). Surface derivation now
consults the overlay only — never `smithy.api#paginated` — and emits `--page-size` iff
the paginator has a `limit_key`. Result: pagination flag divergence went from
349 missing + 1,455 extra to **zero**, and the full paginator configs (`input_token`,
`output_token`, `result_key`, …) are already in place for the pagination runtime.

Extraction gotcha worth remembering: awscli v2 loads its vendored botocore twice
(`awscli.botocore.*` and, via an import hook, top-level `botocore.*`), producing distinct
exception classes — `botocore.exceptions.UnknownServiceError` is *not* an instance of
`awscli.botocore.exceptions.DataNotFoundError`. Catch from the right module instance.

The injection *semantics* behind these flags (types, shadowing, auto-disable rules,
waiter interaction) were verified separately and are specified in
`docs/pagination-runtime.md` for the args/runtime crates.

### 4. Operation removals — `customizations/removals.py:28-124`

31 operations across 16 services are deleted from the command table. Pure data.

Confirmed in our diff: `ec2 import-instance`, `ec2 import-volume` appeared as "extra ops",
exactly matching the table. Also covers `lambda invoke-with-response-stream` and
`logs get-log-object`.

### 5. EC2 structure-of-single-boolean pull-up — `customizations/toplevelbool.py:35-96`

A `structure` with exactly one boolean member named `Value` becomes `--opt` / `--no-opt`.
EC2 only. Explains the missing `--no-disable-api-stop`, `--no-ebs-optimized`,
`--no-ena-support`, `--no-source-dest-check` on `modify-instance-attribute`.

We currently emit `--no-X` only for genuine `boolean` shapes, so these are missed.

### 6. Per-service argument additions

Hand-written, service-specific. Each is small; there are many.

| customization | adds |
|---|---|
| `ec2/secgroupsimplify.py:31-51` | `--protocol --port --cidr --source-group --group-owner` on `authorize-security-group-{ingress,egress}` |
| `ec2/bundleinstance.py:75-98` | `--bucket --prefix --owner-akid --owner-sak --policy` |
| `ec2/decryptpassword.py:45` | `--priv-launch-key` on `get-password-data` |
| `ec2/addcount.py:44-46` | adds `--count`, **deletes** `--min-count`/`--max-count` |
| `awslambda.py:33-93` | hoists `--zip-file` on `create-function` / `publish-layer-version` |

All five appear verbatim in our current diff.

### 7. The `s3` / `s3api` split

`s3api` carries the ~116 modeled operations. The high-level `s3` is a **separate command
tree** with 9 subcommands (`cp ls mb mv presign rb rm sync website`) and no model
operations at all (`customizations/s3/s3.py:46-47`). They are unrelated trees, not two
views of one service. Deferred to the customization phase.

---

## Not yet measured

- **4 model files map to no reference service**: `cloudwatch-events`, `elastic-transcoder`,
  `sagemaker-runtime-http2`, `transcribe-streaming` — services in aws-sdk-rust that the
  CLI does not ship (superseded or streaming-only). They should be excluded, not shipped.
- **~11 corpus services still lack a vendored model** (438 corpus vs 427 compared) —
  services present in botocore but absent from aws-sdk-rust's model set.
- **8 custom top-level commands** have no model and so cannot be checked this way at all:
  `s3`, `configure`, `ddb`, `history`, `login`, `logout`, `update`, `cli-dev`.
- **107 missing / 79 extra operations** — the custom `BasicCommand` additions
  (`ecr get-login-password`, `eks update-kubeconfig`, `logs tail`, `cloudformation
  deploy/package`, …) and the `removals.py` deletions, respectively.
- **`_UNDOCUMENTED` arguments still parse and work.** They are hidden from help, not
  removed, so a drop-in replacement must keep accepting them. The corpus captures them.
- **Argument *types*** — shorthand syntax, `nargs`, blob/file handling. The corpus records
  flag names only; behavioural conformance needs the `awsc` binary.

---

## Custom commands (first tranche)

Six custom commands are now implemented, each verified by byte-diffing our stdout/stderr
and exit code against the reference. `scripts/compare-custom-commands.sh` reproduces the
comparison; it pins our clock to the reference's via `AWSC_FIXED_TIME`, because presigned
URLs embed a timestamp and would otherwise never compare equal.

| Command | Verified against reference |
|---|---|
| `ecr get-login-password` | live call required; logic pinned to `customizations/ecr.py` |
| `ecr-public get-login-password` | as above; response shape differs (structure, not list) |
| `rds generate-db-auth-token` | byte-identical, 6 cases incl. session token |
| `codecommit credential-helper get/store/erase` | byte-identical, 6 cases |
| `eks get-token` | presigned URL byte-identical; document byte-identical in json/text/yaml/query |
| `configservice get-status` | format strings ported from `getstatus.py`; needs a live account to diff |

Facts worth recording, because each contradicts a reasonable assumption:

- **`ecr-public` does not pin `us-east-1`.** Its ruleset is purely region-templated; the
  `us-east-1` that appears everywhere comes from the *documentation examples*.
- **`generate-db-auth-token` signs for `rds-db`**, not `rds`, and always appends the port
  to the URL — but omits `:443` from the *signed* host, so those two strings differ.
- **Presigned URLs emit and canonicalize their parameters in different orders.** botocore
  appends the auth parameters in a fixed insertion order, then recomputes the canonical
  query by sorting the encoded pairs — and `X-Amz-Security-Token` sorts before
  `X-Amz-SignedHeaders`. Emitting in canonical order produces a plausible URL that fails
  to authenticate.
- **`codecommit credential-helper` is not SigV4.** The canonical request uses the literal
  method `GIT`, an empty canonical query, and an *empty payload-hash field* rather than a
  SHA-256; the timestamp inside the string-to-sign carries no trailing `Z`. The `Z` is
  added only when the timestamp is concatenated with the signature.
- **`eks get-token` prints a trailing blank line**, because the command adds a newline on
  top of the formatter's. Its output does go through `--output`/`--query`, unlike the
  other custom commands here.
- **`eks get-token` with neither cluster flag exits 1**, not 252: the reference *returns*
  a `ValueError` instead of raising it, so Python prints it bare with no `aws: [ERROR]:`
  prefix.

### Found while scoping, and dropped

`s3api get-bucket-lifecycle`, `put-bucket-lifecycle`, `get-bucket-notification` and
`put-bucket-notification` are **not custom commands** — they are ordinary modeled
operations marked `deprecated`, which only hides them from help. They already worked.
`customizations/s3endpoint.py` does not exist in v2 at all.

### Still outstanding

`cloudformation deploy/package`, `logs tail`, `eks update-kubeconfig`,
`configservice subscribe`, and the `s3` tree (`cp`/`sync`/`mv`/`rm`/`ls`), which needs a
concurrent transfer manager with multipart support and is a subsystem rather than a
command.

---

## Custom commands (second tranche)

`configservice subscribe` and `logs tail` are implemented.

`logs tail` notes:

- It is the only custom command that streams, and it writes straight to stdout —
  `--output` and `--query` are ignored, matching the reference.
- The three formats render the timestamp three different ways: `short` has no offset and
  no fraction, `detailed` always shows six fractional digits, and `json` drops the
  fraction entirely when the event lands on a whole second.
- The `--follow` dedup map is keyed by raw epoch millis and pruned to only the newest
  timestamp after each response. That is not a cache: `startTime` is advanced to that same
  millisecond and the bound is inclusive, so without it those events repeat forever.
- `rstrip()` runs on the *assembled* line, so a message ending in whitespace loses it,
  while interior newlines survive and multi-line events stay multi-line.

### Known divergence: `--since` with a naive timestamp

The reference sends anything non-relative to `dateutil`, which reads a timestamp with no
UTC offset (`2026-08-01T10:00:00`) in the machine's **local** timezone — while relative
offsets such as `5m` are computed in UTC. We support relative offsets, epoch seconds, and
ISO 8601 *with* an explicit offset, and refuse a naive timestamp with an explanatory
error. Assuming UTC would silently shift the query window by the local offset, which is
worse than refusing. Closing this needs a timezone database the binary does not carry.
Unparseable values match the reference's wording exactly.

---

## Deferred, with reasons

These were researched in full and deliberately not attempted, because each rests on a
subsystem that does not exist yet. Building a half-version would be worse than the current
honest "unknown operation".

- **`cloudformation package`** needs a YAML parser *and* emitter that round-trips
  CloudFormation's intrinsic short forms (`!Ref`, `!GetAtt`), applies YAML 1.1 boolean
  quoting, preserves key order and flattens aliases; plus jmespath set-by-path, a
  deterministic zip writer, and multipart S3 upload. Note the S3 key is the MD5 of the
  artifact, so for *directory* artifacts the key is not reproducible across machines
  anyway — zip entries carry mtimes and mode bits.
- **`cloudformation deploy`** additionally needs three botocore waiters
  (`change_set_create_complete`, `stack_create_complete`, `stack_update_complete`), whose
  acceptor definitions are a further data extraction.
- **`eks update-kubeconfig`** needs a general YAML parser, because it *rewrites the user's
  existing `~/.kube/config` in place*, non-atomically and with no backup. A partial parser
  that mis-reads an unusual but valid kubeconfig would destroy a file the user depends on.
  This one should not be attempted until the YAML work above is done and tested.

---

## The `aws s3` tree

Separate from `s3api`: it has no model of its own, builds its requests by hand, and writes
plain text to stdout. `--output` and `--query` are ignored throughout (the reference says
so in `ls`'s own description).

Implemented: `ls`, `mb`, `rb`, `presign`, `website`. `presign` is byte-identical to the
reference across five cases including a session token, UTF-8 and space in the key, and
path-style fallback; the argument-handling and error paths of all five match across
fourteen cases.

### Two bugs this surfaced in existing code

- **S3 needs virtual-host addressing.** The endpoint ruleset takes `Bucket` as a *named*
  parameter, not a builtin, so we were never supplying it and always resolved the generic
  `https://s3.<region>.amazonaws.com`. The reference produces
  `https://<bucket>.s3.<region>.amazonaws.com` — a different host, and the host is signed.
- **A resolved endpoint can carry a path, and that path is signed.** For a bucket whose
  name contains a dot, no wildcard certificate matches the virtual-host form, so the
  ruleset falls back to putting the bucket in the *path*. We signed only the request path,
  authorising a different resource than the one requested. `Endpoint` now carries
  `path_prefix` and the signer includes it.

Both were invisible until a byte-diff of `presign`, because a wrong signature only shows up
against the live service.

### Notes

- `ls` prints timestamps in the machine's **local** timezone. Rust's standard library
  carries no timezone data, so this asks the C library for the UTC offset rather than
  assuming UTC and being silently wrong by the local offset. That is the one new
  dependency (`libc`, unix only), and it also lets `logs tail --since` accept naive
  timestamps in future.
- `mb` and `rb` are unusual: they catch every error themselves and exit **1** with an
  undecorated `make_bucket failed: ...` on stderr, rather than the usual `aws: [ERROR]:`
  at 254. Reproduced.
- `ls` with a prefix that matches nothing exits **1** silently; with no key at all
  (bare bucket, or no path) an empty result is still 0.
- Integer arguments (`--page-size`, `--expires-in`) are converted with a bare `int()`
  upstream, so a bad value is an uncaught `ValueError` at **255**, not parameter
  validation at 252.
- `--page-size` in this tree is a per-command argument, not the injected pagination
  control, so the global parser no longer consumes it for `s3`. `--no-paginate` and
  `--output` genuinely are accepted-and-ignored there.
- `presign s3://bucket` with no key fails parameter validation: the reference validates
  the underlying `GetObject` parameters and `Key` has a minimum length of 1.

### Still to come

`cp`, `mv`, `rm` and `sync` — the transfer engine. Research is captured: 8 MiB multipart
threshold and chunk size, 10 concurrent requests, the `--exclude`/`--include` chain where
the *last* matching rule wins and `*` crosses `/`, and the sync comparator's asymmetric
time rule (upload skips when `dest >= src`, download skips when `local <= s3`, with no
tolerance). Exit codes there are their own rule: 1 for any failure, 2 for warnings only.

### `cp`, `mv`, `rm`

Verified end to end against a local S3 server (`scripts/fake-s3-server.py`): recursive
upload, download and s3→s3 copy all round-trip with identical SHA-256, including a 25 MiB
object that exercises the multipart and ranged-download paths. Argument handling, filter
semantics and error output match the reference across twenty cases.

**Three deliberate UI departures**, requested and worth stating plainly:

1. **The source is scanned in full before any transfer starts**, so progress totals are
   exact. The reference streams its listing into the transfer and shows `~`-prefixed
   estimates plus `(calculating...)` until listing finishes. Ours costs one extra listing
   pass; it buys a real percentage from the first frame.
2. **Parts of a single large object transfer concurrently.** The reference parallelises
   across files but walks one file's parts in order, so a single big file is slow. Ours
   gives a large object the whole pool.
3. **The progress line is clamped to the terminal width.** The reference pads to the
   previous line's length and relies on `\r`; on a narrow terminal that line wraps, `\r`
   returns to the start of only the last screen row, and the bar smears down the screen
   leaving duplicate rows. Measuring the width (via `ioctl`, then `COLUMNS`, then 80) and
   truncating means exactly one row is rewritten in place.

Bugs this work surfaced in code already committed:

- **`object_path` double-counted the endpoint's path prefix.** The transport builds its
  URL from `endpoint.url`, which already contains the path-style bucket, and the signer
  adds the prefix separately — so including it in the request path too sent
  `/bucket/bucket/key`. Objects uploaded fine and then could not be found.
- **Filter patterns are anchored to the source root**, not matched against the relative
  key. `--exclude "sub/*"` excludes only the `sub/` directly beneath the source. Getting
  this wrong silently transferred files the user excluded.
- **`abspath`, not `canonicalize`.** Resolving symlinks rewrites `/tmp` to `/private/tmp`
  on macOS, after which no pattern matched anything, because the scanned paths keep the
  name the user typed.

`rb --force` empties the bucket via `rm --recursive` first and refuses to delete the
bucket if anything failed, matching the reference. Verified on real S3: `mb`, upload,
`rb` failing with `BucketNotEmpty`, then `rb --force` succeeding and the bucket
disappearing.

Known gaps: `--sse-c`, `--grants`, `--metadata`, `--metadata-directive`, `--copy-props`,
`--follow-symlinks`, and the streaming forms (`cp - s3://...`).

### Concurrency: one queue, adaptive workers

Small objects and individual parts of large ones sit in the **same** work queue. An earlier
version ran them in separate phases, which left the pool idle whenever the current phase
was thin — a single large file among small ones got the pool to itself only after every
small file had finished. Now a 40 MiB object's five parts and forty small files are all
just jobs.

The worker count **adapts**. It starts at the reference's fixed ten, samples throughput
every 150 ms, and ramps while throughput is still improving, up to a ceiling derived from
the machine (`available_parallelism × 4`, clamped to 8–64). It never drops below ten
unless the service returns `SlowDown`, which lifts the floor restriction entirely — the
service is the authority. `--concurrency N` pins it and disables the ramp.

Two mistakes worth recording, both caught by measurement rather than reading:

- **The controller first steered on bytes/second.** That is a useless signal for a
  directory of small files: throughput stays near zero however many workers run, so the
  controller concluded it was over-provisioned and ratcheted itself down to a single
  worker — making a 40-file upload *three times slower* than the reference. It now steers
  on completed jobs per second, which rises with concurrency in both regimes.
- **A pool with no floor is a pool that can be worse than a constant.** Adapting has to be
  strictly an improvement on not adapting.

Measured against the local server with 100 ms of injected latency, release build:

| workload | ours | reference |
|---|---|---|
| 40 small files | 0.65 s (peak 24 in flight) | 1.29 s (peak 10) |
| 40 MiB upload, 5 parts | 2.41 s (peak 6) | 2.36 s (peak 5) |
| 40 MiB download | 0.44 s (peak 6) | 2.69 s (peak 1) |

Note the debug build is *not* representative for uploads: SigV4 hashes the payload, and
unoptimised SHA-256 over 40 MiB dominates everything else (7.6 s versus 2.4 s).

### A 9-second bug on every s3 command

`models/` is named by aws-sdk-rust's conventions, so `s3api` lives in `s3.json` and `logs`
in `cloudwatch-logs.json`. When the obvious filename missed, `load_model` scanned the
whole directory — 431 models, 200 MB of JSON, each parsed until the right one appeared.
That cost **9 seconds of CPU on every invocation** of `s3`, `logs` and `configservice`,
and it was invisible while only argument handling was being tested.

The scan result is now cached in `models/.awsc-model-index.json` and rebuilt whenever a
lookup misses, so it is self-healing if models are added or replaced. First call 9 s,
every call after 0.28 s.

---

## What only real AWS could catch

Everything above was verified against a local server that does not check signatures and
resolves one endpoint. Pointing the binary at real S3 immediately found three bugs that
had passed every local test.

### 1. Query values need stricter encoding than paths

SigV4 canonicalises query parameters with `quote(safe='-._~')`, so `/` becomes `%2F`. We
encoded query values with the *path* encoder, which deliberately leaves `/` alone. A
`prefix` of `AWSLogs/`, or `delimiter=/`, was therefore signed one way and sent another:

```
An error occurred (SignatureDoesNotMatch) when calling the ListObjectsV2 operation
```

`ls --recursive` passed throughout, because at a bucket root the prefix is empty and no
delimiter is sent — the failure only appeared with a nested prefix or a non-recursive
listing. `encode_key` (paths, `/` preserved) and `encode_query` (values, `/` escaped) are
now separate, with a test stating the distinction.

### 2. The profile's `region` was never read

`resolve_region` accepted a `profile_region` argument and every caller passed `None`, so
the `region = us-east-1` in `~/.aws/config` was ignored. For most services that would be a
loud `NoRegion` error; for S3 and STS it *silently* resolved the legacy global endpoint —
`bucket.s3.amazonaws.com` where the reference uses `bucket.s3.us-east-1.amazonaws.com`.
Both work, so `ls` output matched and nothing looked wrong until a presigned URL was
compared host by host.

### 3. A failed download left a full-size sparse file

Large downloads preallocate the destination with `set_len` before fetching any range. On
failure — an object in Glacier, say — that left a file of the right size full of zeroes,
looking exactly like a successful download. The reference leaves nothing. The partial file
is now removed.

### Verified against real S3

- `sts get-caller-identity`, `s3 ls` in eight forms, `s3api list-buckets`,
  `ec2 describe-regions`: byte-identical, exit codes included. (The only difference
  anywhere was S3's opaque pagination token, which the service regenerates per call.)
- A 24 MB Standard-tier object downloaded through the ranged path: **identical SHA-256**,
  8.7 s against the reference's 11.4 s.
- A key containing Korean characters resolved correctly — both CLIs returned the same
  `InvalidObjectState` for a Glacier object, which proves the key was encoded right.

### Uploads, verified against real S3

Run against a scratch prefix and cleaned up afterwards:

- Single-object upload, 25 MiB multipart upload, and recursive upload of a tree — all
  round-trip with **identical SHA-256**.
- The strongest check: an object uploaded by *our* multipart path downloads with an
  identical checksum using the **reference CLI**, so the parts were assembled correctly
  server-side rather than merely being self-consistent.
- A listing of our upload matches a listing of the reference's upload of the same tree,
  keys and sizes alike (only the upload timestamps differ).
- `ContentType` is inferred from the extension identically (`application/json` for
  `b.json`).
- `s3 -> s3` recursive copy, `mv` (source correctly deleted), `rm --dryrun` (touches
  nothing) and `rm --recursive` all behave.
- Cleanup verified: the scratch prefix lists zero objects afterwards from both CLIs, and
  the rest of the bucket is untouched.

### `sync`

A sorted merge-join over both listings, keyed on each entry's path relative to its root.
Actions come out in key order, so deletes interleave with transfers rather than forming a
separate phase — which is what the reference does.

Three rules that are easy to get backwards, all covered by tests:

- **The time test is asymmetric.** An upload is skipped when the destination is *at least
  as new* as the source (`dest - src >= 0`); a download is skipped when the local file is
  *no newer* than the object (`dest - src <= 0`). A local file newer than the object
  therefore triggers a download — surprising, but correct.
- **There is no tolerance.** Not a second, not a millisecond.
- **Downloads stamp the local mtime to the object's `LastModified`.** Without it a clean
  download leaves the local file newer than the object and the next sync repeats the whole
  transfer.

`--size-only` replaces the comparison entirely; `--exact-timestamps` tightens *only* the
download case to require equality, leaving uploads alone. When both are given the
reference lets `--exact-timestamps` win, which is reproduced. `--delete` removes objects on
an upload and local *files* on a download, and only for entries that survived the filters.

### A prefix is not a path

`ListObjectsV2` takes a raw string prefix, so `s3://bucket/mut` also matches `mut2/...`.
Every `dir_op` command — `cp --recursive`, `rm --recursive`, and all of `sync` — must
append the separator. Without it a sync of one prefix silently pulled in a sibling's
objects and wrote them into the destination. Found by giving two prefixes names where one
was a prefix of the other; a test corpus of unrelated names would never have shown it.

### Verified against real S3

Both directions, with mutual agreement checked at each step — after our sync the reference
reports nothing to do, and vice versa:

| case | result |
|---|---|
| first sync uploads everything | 3 uploads |
| repeat is a no-op | 0, and the reference agrees |
| one file modified | exactly 1 re-uploaded |
| sibling prefix isolation | 3 files, not 6 |
| download round-trip | trees identical (`diff -r`) |
| `--delete` up / down | object removed / local file removed |
| `--exact-timestamps` | older local file downloads; default does not |
| `--size-only` | mtime change ignored |

The local fake server could not settle these: it returned a fixed `LastModified`, and once
that was fixed the reference's own sync-download no-ops against it. Real S3 was the only
usable oracle.

---

## Removals and argument renames

Two tables that were already extracted into `data/customizations.json` and already applied
by the conformance harness — but never by the binary.

- **Removals.** v2 deletes 37 commands across 16 services. We accepted every one of them:
  `ses delete-verified-email-address` ran for us and is unknown to the reference. A
  drop-in replacement being a *superset* is its own kind of wrong. Now rejected with
  argparse's own wording, `argument operation: Found invalid choice '...'`.
- **Argument renames.** 87 `service.operation.argument` rules. `sns subscribe` takes
  `--notification-endpoint`, `route53 get-traffic-policy` takes
  `--traffic-policy-version`. These now apply in three places that all had to agree:
  binding a flag to a member, the required-flag check, and skeleton generation. The
  required-flag check was the one that would have been missed — it demanded `--version`
  for a command whose flag had been renamed away.

While wiring these up, unknown *services* and unknown *operations* also gained the
reference's exact wording (`argument command:` and `argument operation:` respectively,
each followed by two blank lines and the usage block).

### The overlay was already there

The first attempt wrote a fresh extractor and a second JSON file before noticing
`data/customizations.json` already carried both tables with identical content. Deleted
in favour of embedding the existing file, with a test asserting the embedded copy and the
one the harness loads from disk are the same table — otherwise the binary and the
conformance report could quietly disagree about the surface.

### The harness could not have caught this

`ServiceDiff` computes `operations_unexpected` and `is_clean` checks it, so the surface
comparison was sound. The gap was that the *surface builder* applied removals while the
*binary* did not — two code paths deriving the same thing, only one of them checked. The
`ses` line read `69 ops matched` and was accurate; nothing was comparing what the binary
would actually accept.

Worth remembering: a conformance harness that shares no code with the thing it checks can
only report on what it re-derives.

---

## One command table, two consumers

The binary and the conformance harness were deriving the command surface independently.
Both were individually reasonable and they disagreed, and the harness reported "no
divergences" the whole time — a harness that re-derives the surface can only check its own
derivation.

`aws_cli_model::command_table` is now that derivation, applying removals, operation
renames, aliases and replacements. The binary resolves commands through it and the surface
builder enumerates through it, so they agree by construction rather than by two
implementations happening to match.

`tests/binary_agrees_with_surface.rs` is the guard: for every vendored model it asserts
that every command the binary would accept is one the reference has. It found a fifth
instance the moment it ran — `rds modify-option-group`, which v2 replaces with
`add-option-to-option-group` and `remove-option-from-option-group`.

Fixed by this:

| command | before | now |
|---|---|---|
| `signin create-oauth2-token-with-iam` | rejected | accepted |
| `signin create-o-auth2-token-with-iam` | accepted | rejected (rename replaces) |
| `cloudwatch get-otel-enrichment` | rejected | accepted, output byte-identical live |
| `cloudwatch get-o-tel-enrichment` | accepted | still accepted (alias keeps both) |
| `rds modify-option-group` | accepted | rejected (replaced) |

### A mistake worth recording

The first version keyed the table to *wire* names, which broke every command in the
binary: `Model::operation` is indexed by the derived CLI name, not the wire name. Caught
immediately by running real commands rather than trusting the unit tests, which had
asserted the wrong thing in the same breath as the code.

### Still outstanding

- `rds add-option-to-option-group` / `remove-option-from-option-group`: the two commands
  that replace `modify-option-group`. They proxy `ModifyOptionGroup` with
  `--options-to-include` / `--options-to-remove` both renamed to `--options`.
- The universal streaming `outfile` positional (`customizations/streamingoutputarg.py`):
  any operation whose output carries a streaming blob gains a required positional naming
  the file to write. `polly synthesize-speech`, `s3api get-object`, `lambda invoke` and
  `kms decrypt` are unusable without it.
  `Model::operation_has_streaming_blob_output` already identifies them.

---

## Streaming output, and the response headers nobody was reading

`customizations/streamingoutputarg.py` gives every operation whose output carries a
streaming blob a **required trailing positional** naming the file to write. That is why
`s3api get-object BUCKET KEY out.bin` and `polly synthesize-speech ... out.mp3` take a
filename with no flag. We rejected it as an unexpected positional, so those commands were
unusable. The body now goes to the file and the headers become the printed document.

Implementing it exposed three bugs underneath, none of them about streaming:

- **`rest*` responses bind members to headers, and we only ever parsed bodies.**
  `head-object` prints nine fields, every one of them from a header, and returns no body
  at all — so we had nothing to parse and failed outright. Header binding now merges with
  the body in *model member order*, since that order is what the CLI prints.
- **ureq transparently gunzips.** Its `gzip` feature is on by default, so a gzip-encoded
  object would have been silently decoded on the way to disk — `s3api get-object` must
  write the stored bytes unchanged. It also made an empty gzip body fail as "unexpected
  end of file", which is what first drew attention to it. Now `default-features = false`.
- **A HEAD response has no body.** It carries the `Content-Length` of the body it
  describes, so reading one waits for bytes that never arrive.

Verified against real S3: `head-object` and `head-bucket` are byte-identical, and
`get-object` writes byte-identical file content with matching metadata.

### Known remaining, all pre-existing and separate

- `get-object` omits `ChecksumCRC64NVME`/`ChecksumType`: the reference sends
  `x-amz-checksum-mode: ENABLED` by default and we do not.

---

## `xmlFlattened` is a member trait

`s3api list-objects-v2` returned `"Contents": []` for a bucket full of objects. The parser
tested `xmlFlattened` on the list *shape*; Smithy puts it on the **member**. S3's
`ListObjectsV2Output$Contents` carries the trait while `ObjectList` does not, so every
flattened list looked absent.

Silently-empty is the worst shape a bug can take here: an error stops a script, an empty
list makes it act. And it was invisible from the high-level `s3` tree, which parses S3's
XML directly rather than through the modelled path — `s3 ls` worked perfectly throughout.

Both spellings are now accepted, since a few models do annotate the shape. Verified live:
`s3api list-objects-v2`, `list-objects`, `route53 list-hosted-zones`, `iam list-users` and
`sqs list-queues` are all byte-identical to the reference.

### Remaining on S3 list operations

Done — see below.

---

## EncodingType, a self-closing root, and the rds proxies

**`EncodingType=url`.** botocore injects it into `ListObjects`, `ListObjectsV2` and
`ListObjectVersions` and percent-decodes `Key`, `Prefix`, `Delimiter`, `KeyMarker`,
`NextKeyMarker` and `StartAfter` coming back. Both halves are now done, and they had to be
done together: the request alone changes the output, and the response alone would decode
text that was never encoded. Without either, a key containing characters XML cannot carry
comes back wrong.

**A self-closing root element was dropped.** `Event::Empty` with nothing open above it is
the whole document, but the parser only ever attached such an element to a parent — so
`get-bucket-location` in us-east-1, which answers `<LocationConstraint/>`, was reported as
having no root at all. A single-member output serialised as the document element itself is
now read as that member, which is how `{"LocationConstraint": null}` appears.

**`rds add-option-to-option-group` / `remove-option-from-option-group`.** Both proxy
`ModifyOptionGroup`. Each exposes one `--options` — renamed from `--options-to-include` or
`--options-to-remove` respectively — and hides the opposite list entirely, so the command
takes one obvious flag rather than two opposites. Verified against real RDS.

`s3api get-bucket-location`, `list-objects`, `list-objects-v2`, `head-object` and both rds
proxies are byte-identical to the reference.

### Still outstanding

- `get-object` omits the checksum fields, since we do not send
  `x-amz-checksum-mode: ENABLED`.

---

## Suggestions, and the `s3` tree's own encoding

**`Maybe you meant:`.** `argparser.py` calls `difflib.get_close_matches(value, choices,
cutoff=0.8)`, leaving `n` at 3. The suggestions are printed, so the similarity measure is
user-visible and an approximation would show a different list. `close_matches.rs` is a
port of difflib's `SequenceMatcher.ratio` — `2*M/T` over the recursively-longest matching
blocks — validated against Python's own output. `rds modify-option-group` now suggests
`copy-option-group`, exactly as the reference does.

Note the two newline counts differ: argparse's first message part keeps its own trailing
newline, so a message *without* suggestions sits one line further from the usage block
than one with them. Both are reproduced.

**Unknown flags** are now reported as `Unknown options: --a, x` after the usage block —
argparse prints its usage first and the message second, the opposite order from every
other error the CLI reports.

**`EncodingType=url` in the `s3` tree.** The high-level tree builds its own listings, so it
needed the treatment separately. Verified with keys containing spaces and non-ASCII:
`s3 ls` matches the reference byte for byte, and `cp --recursive` round-trips them.

Two bugs that only a key with a space could show:

- S3 encodes a space as `+` under `encoding-type=url`, not `%20`. Decoding only percent
  escapes left `a+b.txt`, which then made `rm --recursive` delete nothing and report
  success — it was asking for keys that did not exist. A literal `+` arrives as `%2B`, so
  the substitution cannot lose one.
- The same decode was missing from the modelled path's `Key`/`Prefix` fields.

### A divergence left deliberately

With **three or more** unknown flags the reference emits them in an order that comes out of
argparse's own extras accumulation: `--aa 1 --bb 2 --cc 3` prints
`--aa, --bb, 2, --cc, 3, 1`. One and two unknown flags match exactly; beyond that the same
set appears in a different order. Reproducing it means porting argparse's option-scanning
loop, which is a lot of machinery for the ordering of an error message nobody parses.

---

## The remaining `s3` transfer flags

`--metadata`, `--metadata-directive`, `--cache-control`, `--content-disposition`,
`--content-encoding`, `--content-language`, `--expires`, `--website-redirect`, `--grants`,
`--sse-kms-key-id`, `--sse-c`, `--sse-c-key`, `--follow-symlinks` /
`--no-follow-symlinks`, and the streaming forms `cp - s3://...` and `cp s3://... -`.

Verified against real S3: uploading with `--metadata Key1=v1,Key2=v2 --content-type
text/plain --cache-control max-age=60` produces a `head-object` identical to the
reference's, and the streaming forms round-trip stdin to stdout.

**`--grants`** is verified end to end against a scratch bucket with ACLs enabled: uploading
with `full=id=<canonical-id>` and with `read=uri=.../AllUsers` produces object ACLs
byte-identical to the reference's for both grantee forms.

**`--sse-c`** cannot be verified in this account. SSE-C uploads are blocked by policy at
the *account* level, not per bucket — a freshly created bucket refuses them the same way,
and the reference CLI fails with the identical `AccessDenied`. That identical failure is
itself evidence the request is equivalent, but it is not a round trip, and this is the one
flag whose correctness rests on reading the reference rather than observing it. SSE-C
sends the key base64-encoded with its MD5 alongside, which is how S3 detects a key mangled
in transit.

Implementing it did surface a real gap: the SSE-C headers were only being sent on
**uploads**. `GetObject`, the ranged download and `HeadObject` all need the same customer
key, or an encrypted object cannot be read back at all. Downloads now carry them.

### `DirEntry::metadata` does not follow symlinks

`std::fs::DirEntry::metadata` deliberately does *not* traverse symlinks, unlike
`std::fs::metadata`. Following is the CLI's **default**, so a symlinked file was being
skipped from every recursive upload: the reference copied two files where we copied one.
The link is now resolved explicitly, and a broken link is skipped rather than failing the
whole walk.

This was only visible by counting files against the reference — the transfer succeeded and
reported success either way.

### Still outstanding

- `--sse-c-copy-source` / `--sse-c-copy-source-key` for `s3 -> s3` copies of
  customer-encrypted objects.
- `--request-payer`, `--checksum-algorithm`, `--checksum-mode`, `--expected-size`.

---

## Multipart copy

A `CopyObject` is capped at 5 GiB, and one request for a large object occupies a single
worker for the whole transfer. Objects at or above the 8 MiB threshold are now copied with
`UploadPartCopy`, the parts sharing the pool.

Three things a single `CopyObject` gets for free and this has to do explicitly:

- **Pin the source.** Every part carries `x-amz-copy-source-if-match` with the source's
  ETag, so a source replaced mid-copy fails the transfer instead of silently stitching two
  different objects together. A `PreconditionFailed` is reported as the reference words
  it, naming the object rather than the raw condition.
- **Carry the properties across.** A server-side multipart copy inherits nothing, so
  content type, the content-* headers and `x-amz-meta-*` are read from the source and set
  on `CreateMultipartUpload`. This is what `--copy-props` governs in the reference, whose
  default is to preserve them.
- **Keep the conditionals off the create.** `CreateMultipartUpload` rejects the
  copy-source conditionals, so they go only on the part requests.

Verified against real S3 with a 30 MiB object: content SHA-256 identical, metadata
identical to the reference's own copy of the same object, and the destination ETag is
`...-4` — the same part count and therefore the same part boundaries the reference
produced. Recursive copy and `mv` over the threshold both behave.

### Still outstanding on copies

- `--sse-c-copy-source` / `--sse-c-copy-source-key`, for copying an object encrypted with
  a *different* customer key than the destination.
- Object tags are not carried across a multipart copy. `--copy-props` in its `default` and
  `all` modes fetches them with `GetObjectTagging` and reapplies them; ours preserves
  metadata but not tags.


---

## The `aws configure` tree

`list`, `get`, `set` and `list-profiles` are implemented. **35 invocations verified
identical against the reference** — stdout, stderr, exit code, *and* the resulting
`~/.aws/config` and `~/.aws/credentials` byte for byte.

Not implemented, and refused by name rather than approximated: `sso`, `sso-session`,
`mfa-login`, `wizard`, `import`, `add-model`, `export-credentials`, `agent-toolkit`, and
the bare interactive `aws configure` prompt. Naming them individually matters — an
"invalid choice" error would tell the user they had made a typo.

### The writer is not an INI serialiser

`configure set` edits the file as **lines of text**. Comments, blank lines, unusual spacing
and the order of everything it was not asked to change all survive, because nothing is
re-rendered from a parsed model. A round-tripping parser cannot promise that, and a config
file is something people maintain by hand.

Behaviours that had to be taken from the reference rather than guessed:

- An updated key keeps its position and is rewritten as `key = value`; a *new* key lands
  after the last option line of its section, not at the top and not in the next section.
- A new section is appended with **no blank line** before it — one is added only when the
  file does not already end in a newline.
- Writing a credential variable goes to `~/.aws/credentials`, where the section is the
  bare profile name; that file has no `[profile x]` spelling.
- A newline in a key, value or section name is refused, because it would split one setting
  into two lines and the second could be read back as a section header. The message names
  the key but never echoes the value, which is usually a secret.
- After writing the credentials file, a warning goes to stderr if any group or other
  permission bit is set. This is the one place the CLI puts a long-lived secret on disk.

### Section names are shell-quoted, and that cuts both ways

`aws configure set --profile "my dev"` writes `[profile 'my dev']` — **single** quotes,
from `shlex.quote`. Reading it back is `shlex.split` with a hard requirement of *exactly
two words* (`configloader._parse_section`), which has two consequences a plain whitespace
split gets wrong in opposite directions:

- `[profile 'my dev']` is the profile `my dev`; the quotes are shell quoting, not part of
  the name. We wrote this form correctly but could not read it back — so a profile created
  by our own `configure set` was invisible to every other command.
- `[profile my dev]` is **not a profile at all**. Three words, so botocore drops it, and
  treating it as `my dev` invents a profile the reference cannot see.

Verified in both directions: the reference reads back what we write, and we read back what
it writes.

### Three more things that are not what they look like

- **`configure get` exits 1 with no output** when the value is absent — not an error line.
  Scripts branch on that code.
- **The unqualified path is the only one that validates the profile.** `configure get
  region --profile nosuch` fails with exit 255, while `configure get profile.nosuch.region
  --profile alsonosuch` exits 1: the first resolves through the *scoped* config, which
  raises, and the second reads the whole config and simply finds nothing. An asymmetry in
  the reference, kept because scripts branch on the code.
- **Deep nesting is refused in a sub-section and silently truncated on the profile path.**
  `configure set a.b.c v --services x` errors; `configure set a.b.c v` writes `a = v` and
  drops the rest. Matching that matters more than improving on it — a script that has been
  getting away with `a.b.c` would start failing against a stricter replacement.

`list-profiles` prints in **file order**, not sorted: botocore lists its profile dict in
insertion order, so the config file's order comes first and credentials-only profiles
follow. Sorting looks tidier and is a visible difference.

### Two pre-existing bugs this surfaced

- **The INI parser silently discarded nested sub-keys.** An indented block under `s3 =`
  was skipped entirely, so `configure get s3.endpoint_url --services custom` could never
  have worked, and no profile-level nested setting was readable. They are now kept under a
  dotted name (`s3.endpoint_url`); a flat key never contains a dot, so the two namespaces
  cannot collide.
- **`Credentials` did not record which provider produced it.** `configure list`'s TYPE
  column is the provider name (`shared-credentials-file`, `sso`, `iam-role`, …), never
  where the value was looked up — which is also why its LOCATION column is empty for
  credentials. There is no way to derive that after the fact, so the field is now carried
  through the chain.


---

## `aws sso login` and `aws sso logout`

Custom commands on the modelled `sso` service, so neither is an operation on it and both
are dispatched before the model is consulted. The provider could already *use* a cached
token and refresh one about to lapse; what it could not do was obtain the first one.

**`sso logout` is verified identical** — same stdout, same exit code, and the same files
surviving in both `~/.aws/sso/cache` and `~/.aws/cli/cache`. It sweeps by content, not by
name: a cache entry is a token if it has an `accessToken` and a credential if its
`ProviderType` is `sso`, so a client registration, an assume-role credential and a file
that is not JSON at all are all left alone. Each token is invalidated at the service with
`sso:Logout` *before* the local copy is removed — deleting the file alone leaves the
session alive until it expires on its own.

**`sso login` implements the device authorization grant**: `RegisterClient`,
`StartDeviceAuthorization`, then `CreateToken` polled until the user approves. Its three
"not yet" answers are control signals rather than failures — `authorization_pending` means
keep waiting, `slow_down` adds five seconds to the interval, and `expired_token` means the
user took too long. One `CreateToken` is attempted *before* anything is printed, because a
pre-authorized client needs no code shown.

### Interoperability is the point, so the cache keys are exact

A token this command writes lands in `~/.aws/sso/cache` in botocore's format under
botocore's key, and is picked up by the reference CLI — and vice versa. Nobody should have
to choose which CLI to authenticate with. Two keys have to be reproduced exactly:

- The **token** key is `sha1` of the sso-session name, or of the start URL for the legacy
  inline form.
- The **registration** key is `sha1` over a JSON object with sorted keys and Python's
  `", "` / `": "` separators, including its literal `"tool": "botocore"` entry. A
  different key is not a failure — it is a second client registration for the same
  session, visible in the IAM Identity Center console.

The registration is also named `botocore-client-<session>` for the same reason: renaming
it would show up in that console as an unfamiliar second client.

### Known divergence: which grant is used

The reference uses the **authorization-code grant with PKCE** for a modern `sso-session`,
falling back to the device grant only for a legacy profile or when `--use-device-code` is
given. We always use the device grant. The token that results is identical and cached
identically, so nothing downstream can tell the difference — but the interaction is: the
reference redirects a browser back to a local listener, while we show a code to type.
`--use-device-code` is accepted and selects what we already do.

### Not verified end to end

Everything checkable without a live IAM Identity Center portal has been checked: both cache
keys against pinned digests, the configuration errors byte-for-byte against the reference
(a missing `sso_region`, an sso-session that does not exist, with the extra "run `aws
configure sso`" sentence that only the legacy form gets), and the whole of `sso logout`.
The device flow itself — register, authorize, poll, cache — has not been run against a real
portal, because doing so registers a client in someone's account and needs a human to
approve it in a browser.
