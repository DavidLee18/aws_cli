# Known divergences

Every divergence the conformance report prints, traced to its cause in the reference CLI.
Regenerate with `cargo run -p aws-cli-conformance`.

## Current state: ZERO surface divergence

| | |
|---|---|
| Operations compared | 19,452 |
| Argument sets matching exactly | **19,452 (100.00%)** |
| Services fully conformant | **427 of 427 compared** |
| Excluded models (in aws-sdk-rust, not shipped by the CLI) | 4 |
| Corpus services with no aws-sdk-rust model | 11 |

The baseline gate is now zero-tolerance (`MAX_DIVERGING_OPERATIONS = 0`,
`MIN_EXACT_ARG_RATIO = 1.0`). New divergence means a regression or upstream drift after
refetching; fix or regenerate the data files rather than raising the gate.

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
only).

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

- **Global arguments** — implemented: `--query`, `--no-sign-request`, `--cli-read-timeout`,
  `--cli-connect-timeout`, plus `--color`/`--no-cli-pager` accepted as genuine no-ops (we
  neither colour nor page). `--no-verify-ssl` and `--ca-bundle` are **refused rather than
  ignored**, since silently verifying when asked not to would misrepresent the request.
  Still missing: `--cli-binary-format`, `--cli-error-format`, `--debug` parity.

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
