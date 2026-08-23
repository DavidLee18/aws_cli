#!/usr/bin/env bash
# Vendor Smithy JSON AST service models from awslabs/aws-sdk-rust.
#
# These are the same models the Rust SDK is generated from. Unlike botocore's
# service-2.json they carry pagination, waiters, and endpoint rulesets as Smithy traits,
# so one file per service is all we need.
#
# Usage:
#   scripts/fetch-models.sh                # fetch the default protocol-coverage set
#   scripts/fetch-models.sh s3 ec2 lambda  # fetch specific services
#   scripts/fetch-models.sh --all          # fetch the entire catalogue (~200MB)
#   AWS_SDK_RUST_REF=v1.2.3 scripts/fetch-models.sh   # pin to a tag

set -euo pipefail

REF="${AWS_SDK_RUST_REF:-main}"
BASE="https://raw.githubusercontent.com/awslabs/aws-sdk-rust/${REF}/aws-models"
DEST="$(cd "$(dirname "$0")/.." && pwd)/models"

# One service per wire protocol, so the protocol layer always has a real fixture to
# test against. Extend freely; the loader is not specific to these.
# NB: these are aws-sdk-rust model filenames, which are NOT always botocore's service
# names -- botocore calls CloudWatch Logs `logs`, here it is `cloudwatch-logs`. Mapping
# model filename -> `aws <command>` is the conformance harness's job, not this script's.
DEFAULT_SERVICES=(
  sts             # awsQuery
  s3              # restXml
  ec2             # ec2Query
  dynamodb        # awsJson1_0
  cloudwatch-logs # awsJson1_1
  lambda          # restJson1
)

fetch_one() {
  local svc="$1" quiet="${2:-}"
  local url="${BASE}/${svc}.json"
  local tmp
  tmp="$(mktemp)"
  if ! curl -fsSL --max-time 120 -o "$tmp" "$url"; then
    echo "  FAIL  ${svc}  (${url})" >&2
    rm -f "$tmp"
    return 1
  fi
  # Guard against a 200-with-HTML or truncated body being written into models/.
  # Also rejects the repo's non-model JSON (sdk-partitions, sdk-endpoints, ...).
  if ! head -c 200 "$tmp" | grep -q '"smithy"'; then
    echo "  FAIL  ${svc}  (response is not a Smithy model)" >&2
    rm -f "$tmp"
    return 1
  fi
  mv "$tmp" "${DEST}/${svc}.json"
  [ -n "$quiet" ] || printf '  ok    %-10s %s\n' "$svc" "$(du -h "${DEST}/${svc}.json" | cut -f1)"
}

mkdir -p "$DEST"

if [ "${1:-}" = "--all" ]; then
  # Enumerate the catalogue from the GitHub API, then fetch anything not already present.
  # Non-model JSON files fail the smithy guard above and are skipped, not fatal.
  echo "fetching full catalogue from aws-sdk-rust@${REF}"
  names="$(curl -fsSL --max-time 60 \
    "https://api.github.com/repos/awslabs/aws-sdk-rust/contents/aws-models?ref=${REF}" |
    python3 -c 'import json,sys; [print(f["name"][:-5]) for f in json.load(sys.stdin) if f["name"].endswith(".json")]')"
  count=0
  for svc in $names; do
    [ -s "${DEST}/${svc}.json" ] && continue
    fetch_one "$svc" quiet || true
    count=$((count + 1))
  done
  echo "fetched ${count} new model(s); $(ls "$DEST"/*.json | wc -l | tr -d ' ') total in ${DEST}"
  exit 0
fi

services=("$@")
if [ ${#services[@]} -eq 0 ]; then
  services=("${DEFAULT_SERVICES[@]}")
fi

echo "fetching ${#services[@]} model(s) from aws-sdk-rust@${REF}"
for svc in "${services[@]}"; do
  fetch_one "$svc" || exit 1
done
echo "models written to ${DEST}"

# Compile the catalogue into the mapped container the CLI actually reads. Without this
# the binary silently falls back to parsing a whole JSON model per invocation, which
# still works but costs 20-140ms of startup instead of 2-6ms.
echo "compiling ${DEST}/models.bin"
cargo run --release -q -p aws-cli-model --bin compile-models -- "${DEST}"
