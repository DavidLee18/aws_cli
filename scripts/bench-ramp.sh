#!/usr/bin/env bash
#
# What shape should the pool's ramp be?
#
# The pool starts at 10 workers and grows x1.5 per 150ms sample. In-region that costs 18%
# on a 2.5s upload against a pinned 64 -- the pool arrives at the right worker count just
# as the work runs out. Replaying the controller against the measured curve says a start
# of 32 recovers most of it, but the replay is a lower bound: it assumes throughput
# follows the worker count within one sample, while real workers must open a TLS
# connection first.
#
# The reason this cannot be settled on the fast link alone is that every shape which helps
# there is a shape that opens more connections sooner, which is exactly what hurts on a
# thin, high-latency link -- where the same binary also has to run well. So each shape is
# measured under two conditions on one instance:
#
#   fast  the instance's own link, ~1 GB/s
#   wan   netem: 40 Mbit and 100ms of added delay, roughly the developer link this client
#         was originally written against. Egress-shaped, so uploads only -- the rate limit
#         does not apply to the download direction, and claiming otherwise would be false.
#
# Usage: bench-ramp.sh <bucket> <awsc-binary> [repeats]
set -Eeuo pipefail

bucket=${1:?usage: bench-ramp.sh <bucket> <awsc> [repeats]}
awsc=${2:?usage: bench-ramp.sh <bucket> <awsc> [repeats]}
repeats=${3:-3}

work=$(mktemp -d "${BENCH_TMPDIR:-/var/tmp}/ramp.XXXXXX")
trap 'rm -rf "$work"; teardown_wan 2>/dev/null || true' EXIT
trap 'rc=$?; echo "bench-ramp: FAILED at line $LINENO, exit $rc, command: $BASH_COMMAND" >&2' ERR

results=./ramp-results.tsv
logs=./ramp-logs
mkdir -p "$logs"
shm=${BENCH_SHM:-/dev/shm}
# By the token after `dev`, not by field position: the route line has an extra field
# when there is a gateway, and picking $5 silently yields an address instead of a name.
iface=$(ip route get 1.1.1.1 2>/dev/null \
  | awk '{for (i = 1; i < NF; i++) if ($i == "dev") { print $(i + 1); exit }}')
iface=${iface:-eth0}

# Sized so the ramp is visible rather than amortised away: at ~1 GB/s a 512 MiB transfer
# is about four 150ms samples, which is the regime where the ramp is the whole cost. The
# 2 GiB arm is the control -- the same shapes should be indistinguishable there.
# Overridable so the whole script can be rehearsed locally in seconds; the instance
# uses the defaults. Rehearsing off the instance is what stops a mute shell bug costing a
# 15-minute run, which it has done twice.
read -r -a FAST_SIZES <<< "${BENCH_FAST_SIZES:-512 2048}"
# At 40 Mbit a 32 MiB upload is about 7s, long enough for the ramp to complete and for
# over-opening to show up if it is going to.
wan_size=${BENCH_WAN_SIZE:-32}

# name:AWSC_POOL_START:AWSC_POOL_GROWTH:AWSC_POOL_PATIENCE, or `pinned` for the reference.
declare -a SHAPES=(
  "shipped:10:150:1"
  "start32:32:150:1"
  "growth200:10:200:1"
  "patience2:10:150:2"
  "start32-patience2:32:150:2"
  "pinned64:::"
)

[ -f "$results" ] || printf 'link\tshape\tMiB\trun\tdirection\tseconds\tMB_per_s\tpeak_workers\tslowdowns\n' > "$results"

setup_wan() {
  tc qdisc add dev "$iface" root netem delay 100ms rate 40mbit
  echo "wan shaping on $iface: 40mbit, +100ms"
}
teardown_wan() {
  tc qdisc del dev "$iface" root 2>/dev/null || true
}

run() {
  local link=$1 shape=$2 mib=$3 run=$4 direction=$5; shift 5
  local log="$logs/$link-$shape-$mib-$run-$direction.log"
  local start end secs mbps bytes=$((mib * 1024 * 1024))

  start=$(date +%s.%N)
  AWSC_RETRY_TRACE=1 AWSC_POOL_TRACE=1 "$@" > "$log" 2>&1
  end=$(date +%s.%N)

  secs=$(echo "$end - $start" | bc)
  mbps=$(echo "scale=1; $bytes / $secs / 1048576" | bc)
  # The highest target the controller actually reached, which is what distinguishes the
  # shapes; a pinned run has no decisions to report, hence the fallback.
  local peak
  peak=$( { grep -o 'workers [0-9]* -> [0-9]*' "$log" | awk '{print $4}' | sort -n | tail -1; } || true)
  printf '%s\t%s\t%s\t%s\t%s\t%.1f\t%s\t%s\t%s\n' \
    "$link" "$shape" "$mib" "$run" "$direction" "$secs" "$mbps" "${peak:--}" \
    "$(grep -c 'code SlowDown' "$log" || true)" | tee -a "$results"
}

# One shape, one payload, one direction pair, verified.
exercise() {
  local link=$1 shape=$2 spec=$3 mib=$4 run=$5 uploads_only=$6
  local payload="$work/payload-$mib.bin"
  [ -f "$payload" ] || dd if=/dev/urandom of="$payload" bs=1M count="$mib" status=none

  local env_flags=() conc_flag=()
  if [ "$shape" = pinned64 ]; then
    conc_flag=(--concurrency 64)
  else
    IFS=: read -r _ start growth patience <<< "$spec"
    env_flags=("AWSC_POOL_START=$start" "AWSC_POOL_GROWTH=$growth" "AWSC_POOL_PATIENCE=$patience")
  fi

  run "$link" "$shape" "$mib" "$run" up \
    env ${env_flags[@]+"${env_flags[@]}"} \
    "$awsc" s3 cp "$payload" "s3://$bucket/ramp-$shape-$mib.bin" \
    ${conc_flag[@]+"${conc_flag[@]}"} --no-progress

  [ "$uploads_only" = yes ] && return 0

  run "$link" "$shape" "$mib" "$run" down \
    env ${env_flags[@]+"${env_flags[@]}"} \
    "$awsc" s3 cp "s3://$bucket/ramp-$shape-$mib.bin" "$shm/back.bin" \
    ${conc_flag[@]+"${conc_flag[@]}"} --no-progress
  cmp -s "$payload" "$shm/back.bin" || { echo "ROUND TRIP CORRUPTED at $link/$shape/$mib"; exit 1; }
  rm -f "$shm/back.bin"
}

# Interleaved by repeat rather than run in blocks, so link drift cannot be mistaken for an
# effect of the shape.
echo "=== fast link ==="
for i in $(seq 1 "$repeats"); do
  for mib in "${FAST_SIZES[@]}"; do
    for spec in "${SHAPES[@]}"; do
      exercise fast "${spec%%:*}" "$spec" "$mib" "$i" no
    done
  done
done

echo "=== wan (40mbit, +100ms) ==="
setup_wan
for i in $(seq 1 "$repeats"); do
  for spec in "${SHAPES[@]}"; do
    exercise wan "${spec%%:*}" "$spec" "$wan_size" "$i" yes
  done
done
teardown_wan

echo
column -t "$results"
