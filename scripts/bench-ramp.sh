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

# Which arms to run. The fast arm's answer is already in; a re-run that only needs the
# wan arm should not pay for 72 more transfers to get it.
arms=${BENCH_ARMS:-"fast wan"}

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
# The wan arm uploads *many small objects*, not one large one, and that is not a detail.
# The pool caps itself at `max.min(jobs.len())`, so a 32 MiB single-file upload is four
# 8 MiB parts and therefore four workers -- every ramp shape then behaves identically and
# the arm measures nothing. A directory of small objects is both the honest fixture for
# this question and the realistic thin-link workload.
wan_objects=${BENCH_WAN_OBJECTS:-400}
wan_object_kib=${BENCH_WAN_OBJECT_KIB:-128}

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

# The many-object payload for the wan arm.
make_tree() {
  local dir=$1 count=$2 kib=$3
  [ -d "$dir" ] && return 0
  mkdir -p "$dir"
  local i
  for ((i = 0; i < count; i++)); do
    dd if=/dev/urandom of="$dir/obj-$i.bin" bs=1K count="$kib" status=none
  done
}

# One shape, one directory of objects, uploaded. Uploads only: tc shapes egress, so the
# rate limit does not apply to the download direction.
exercise_tree() {
  local link=$1 shape=$2 spec=$3 run=$4
  local tree="$work/tree"
  make_tree "$tree" "$wan_objects" "$wan_object_kib"
  local mib=$((wan_objects * wan_object_kib / 1024))

  local env_flags=() conc_flag=()
  if [ "$shape" = pinned64 ]; then
    conc_flag=(--concurrency 64)
  else
    IFS=: read -r _ start growth patience <<< "$spec"
    env_flags=("AWSC_POOL_START=$start" "AWSC_POOL_GROWTH=$growth" "AWSC_POOL_PATIENCE=$patience")
  fi

  run "$link" "$shape" "$mib" "$run" up-many \
    env ${env_flags[@]+"${env_flags[@]}"} \
    "$awsc" s3 cp "$tree" "s3://$bucket/ramp-tree-$shape/" --recursive \
    ${conc_flag[@]+"${conc_flag[@]}"} --no-progress
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
if [[ " $arms " == *" fast "* ]]; then
echo "=== fast link ==="
for i in $(seq 1 "$repeats"); do
  for mib in "${FAST_SIZES[@]}"; do
    for spec in "${SHAPES[@]}"; do
      exercise fast "${spec%%:*}" "$spec" "$mib" "$i" no
    done
  done
done
fi

if [[ " $arms " == *" wan "* ]]; then
echo "=== wan (40mbit, +100ms) ==="
setup_wan
for i in $(seq 1 "$repeats"); do
  for spec in "${SHAPES[@]}"; do
    exercise_tree wan "${spec%%:*}" "$spec" "$i"
  done
done
teardown_wan
fi

echo
column -t "$results"
