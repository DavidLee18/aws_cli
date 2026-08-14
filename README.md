# aws-cli-rs

A port of the AWS CLI v2 from Python to Rust, targeting drop-in compatibility.

## Why this is tractable

The AWS CLI is mostly not hand-written. `aws ec2 describe-instances`, `aws iam list-roles`
and ~15,000 other operations across 400+ services don't exist as Python source. botocore
synthesises them at runtime from JSON service models: argument parsing, request
serialisation, response parsing and pagination are all model-driven.

That splits the port into two halves with very different shapes:

| | Surface | Code | Nature |
|---|---|---|---|
| **Generic engine** | ~95% | ~5% | One model interpreter, reused by every service |
| **Customisations** | ~5% | ~50% | 49 hand-written modules: `s3` sync/cp, `configure`, `sso`, `ecr get-login-password`, … |

Getting the engine right makes 400+ services work at once. The customisations are then a
long but finite list of ordinary porting jobs.

### The substrate decision

`aws-sdk-rust` is deliberately **not** the base for the generic engine. It is ~400 crates
of build-time-generated typed builders — you cannot reflectively drive `describe-instances`
from a runtime string, and compiling the whole catalogue would be punishing. We take its
*models* and write the runtime ourselves.

The original plan was to borrow `aws-config`/`aws-sigv4`/`aws-smithy-runtime` for the
layer beneath. In practice all three are hand-rolled, for the same reason each time: the
goal is not "works" but "agrees with botocore", and each piece turned out to have an
oracle that makes agreement checkable — a captured reference signature for sigv4, AWS's
own 14,112-case suite for endpoints, and for credentials a token cache shared with the
reference plus a chain order that is fully documented in botocore's source. Pinning a
direct implementation against an oracle is easier than pinning an adapter over someone
else's API, and the divergences that matter here (an explicit profile disabling the env
provider; `role_arn` outranking static keys) are exactly the ones a general-purpose crate
has no reason to reproduce.

The dependency list is correspondingly small: `serde`, `quick-xml`, `hmac`/`sha1`/`sha2`,
`ureq`, `indexmap`, `regex`.

### Models

Models come from [`awslabs/aws-sdk-rust/aws-models`][models] as Smithy 2.0 JSON AST
(434 available). Unlike botocore's `service-2.json`, these carry pagination
(`smithy.api#paginated`), waiters (`smithy.waiters#waitable`) and endpoint rulesets
(`smithy.rules#endpointRuleSet`) as traits, so one file per service suffices.

Known divergence to reconcile: Smithy model **filenames are not botocore service names**
(botocore's `logs` is `cloudwatch-logs` here), and botocore applies CLI-specific argument
renaming that the Smithy models don't describe. The conformance harness is what settles
these, rather than guesswork.

[models]: https://github.com/awslabs/aws-sdk-rust/tree/main/aws-models

## Layout

Crates are added as they are implemented.

| Crate | Status | Responsibility |
|---|---|---|
| `aws-cli-model` | **implemented** | Smithy AST loader, shape index, botocore-compatible naming, overlays |
| `aws-cli-conformance` | **implemented** | Differential testing against the reference CLI |
| `aws-cli-protocol` | **all six** ✅ | awsQuery, ec2Query, awsJson 1.0/1.1, restJson1, restXml — each verified byte-identical live |
| `aws-cli-runtime` | **partial** | sigv4 ✅, endpoint rulesets ✅ (14,112/14,112 AWS conformance cases), credentials ✅ (env, static, SSO + refresh, assume-role, credential_process, IMDSv2, container) |
| `aws-cli-output` | **json** | `text`/`table`/`yaml` fail loudly rather than silently emitting JSON |
| `awsc` | **runs** | The binary: dispatch, global args, exit codes |
| `aws-cli-args` | planned | Shorthand syntax, `--cli-input-json`, `--generate-cli-skeleton` |
| `aws-cli-custom` | planned | The hand-written customisations as behaviour |

```console
$ awsc sts get-caller-identity
{
    "UserId": "AIDACKCEVSQ6C2EXAMPLE",
    "Account": "123456789012",
    "Arn": "arn:aws:iam::123456789012:user/example"
}
```

## Compatibility approach

The reference install is treated as an executable specification, not as documentation.
Behaviour is captured by *running* it rather than reading it — this already caught a real
bug: the initial `xform_name` port included a number-splitting regex from an older
botocore, which turned `s3` into `s-3`. Current botocore has only two regex passes.

The same method pins genuine quirks we must reproduce: botocore really does expose
`IPv6Address` as `i-pv6-address`, and a drop-in replacement has to agree.

Reference: `/opt/homebrew/Cellar/awscli/2.36.22/libexec/.../awscli/`
(432 service models, 49 customisation modules).

### The conformance harness

Two halves, of which the first exists today:

**Surface conformance** — offline, no credentials, covers the whole catalogue.
`scripts/extract-reference-surface.py` drives the reference CLI's own `CLIDriver` to dump
every service, operation and `--flag` into `tests/golden/reference-surface.json`.
`aws-cli-conformance` derives the same surface from Smithy models and diffs the two:

```sh
cargo run -p aws-cli-conformance     # divergence report; non-zero exit if any
```

Driving the real `CLIDriver` rather than reading raw models matters: the command table it
produces already has the customizations applied, which is precisely the layer the Smithy
models don't describe. Every divergence the report prints is either a bug in our engine
or a customization still to port — which is what turns "400 services" into an ordered
worklist.

**Behavioural conformance** — partially in place, all offline:

- `tests/golden/sigv4-sts-get-caller-identity.json` pins the signer byte-for-byte against
  a request captured from the reference (sigv4 is deterministic given credentials,
  timestamp and request, so this needs neither network nor real credentials).
- `cargo test -p aws-cli-runtime --test endpoint_rules` runs AWS's own endpoint suite —
  14,112 cases across 431 services.
- Exit codes and error wording are compared against the reference directly.

Still to come: driving identical argv through both binaries and diffing stdout/stderr.

## Development

```sh
scripts/fetch-models.sh          # vendor the protocol-coverage model set into models/
scripts/fetch-models.sh s3 ec2   # or specific services
cargo test                       # unit + integration tests
cargo run -p aws-cli-conformance # divergence report
```

Generated artefacts, and whether they are checked in:

| Path | Checked in | Produced by | Why |
|---|---|---|---|
| `models/` | no (~110MB) | `scripts/fetch-models.sh` | Large and reproducible; pin with `AWS_SDK_RUST_REF` |
| `data/service-names.json` | **yes** | `scripts/extract-service-names.py` | `include_str!`'d into the binary — required to build |
| `data/paginators.json` | **yes** (642KB) | `scripts/extract-paginators.py` | botocore paginator overlay; which ops paginate is *not* derivable from Smithy |
| `data/partitions.json` | **yes** (7KB) | `scripts/extract-partitions.py` | `aws.partition` table + the no-region global-endpoint fallback |
| `data/protocol-metadata.json` | **yes** | `scripts/extract-protocol-metadata.py` | awsJson `targetPrefix`; not derivable from the Smithy models |
| `data/customizations.json` | **yes** (9KB) | `scripts/extract-customizations.py` | argrename/removals/alias tables, extracted from the customization modules |
| `data/custom-surface.json` | **yes** (57KB) | `scripts/extract-custom-surface.py` | per-op arg patches, custom commands, botocore waiter catalogue (re-runs merge; see `--no-merge`) |
| `tests/golden/reference-surface.json` | **yes** (5.9MB) | `scripts/extract-reference-surface.py` | Lets CI run conformance without an awscli install, and makes surface changes reviewable in diffs |

Every `scripts/extract-*.py` reads the reference install read-only and pins
`AWS_CONFIG_FILE`/`AWS_SHARED_CREDENTIALS_FILE` to `/dev/null`, so local profiles and
plugins cannot perturb what is captured. All are deterministic — re-running produces
byte-identical output.

Tests that need `models/` skip cleanly when it is absent, so a fresh clone passes
`cargo test` before you fetch anything.

## Licence and attribution

This project is Apache-2.0 (see `LICENSE`).

The checked-in files under `data/` and `tests/golden/` are **generated from AWS's own
tooling** and are derivative of it:

- `data/partitions.json`, `data/paginators.json`, `data/customizations.json`,
  `data/custom-surface.json`, `data/service-names.json` and
  `tests/golden/reference-surface.json` are extracted from
  [botocore / AWS CLI v2](https://github.com/aws/aws-cli) (Apache-2.0).
- `models/` (not checked in) comes from
  [awslabs/aws-sdk-rust](https://github.com/awslabs/aws-sdk-rust) (Apache-2.0).

Each generated file carries a `_comment` naming the script that produced it. The
credentials in `tests/golden/sigv4-sts-get-caller-identity.json` are the example key pair
from AWS's published SigV4 documentation, not real credentials.

This is an independent project and is not affiliated with or endorsed by AWS.

## Roadmap

1. ~~Smithy model loader~~ ✅
2. ~~Differential conformance harness against the Python CLI~~ ✅
3. ~~Surface conformance: services, operations, flags~~ ✅ — **100.00% (19,452/19,452
   operations across 427 services)**; zero-divergence gate in CI
4. ~~Vertical slice: `sts get-caller-identity` end to end~~ ✅ — awsQuery + sigv4 (pinned
   byte-for-byte against the reference) + endpoint + credentials + XML parse + JSON out
5. ~~Endpoint ruleset interpreter (`smithy.rules#endpointRuleSet`)~~ ✅ — passes AWS's
   own suite, **14,112/14,112 cases across all 431 services**
6. ~~Credential chain~~ ✅ — env, static, SSO (+ OIDC refresh), assume-role (chaining,
   `credential_source`, web identity), `credential_process`, IMDSv2, container
7. ~~Remaining five protocols~~ ✅ — all six verified byte-identical against live AWS
8. Output formatters and `--query`
9. Pagination/waiter *runtimes* (specs: `docs/pagination-runtime.md`; data already vendored)
10. Customisations as behaviour (surface already data-complete), `s3` transfer manager first
