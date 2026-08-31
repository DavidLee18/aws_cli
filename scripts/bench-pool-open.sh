#!/usr/bin/env bash
#
# The two measurements the pool's remaining decision waits on. Meant to run *in-region*
# on EC2, against a real bucket — neither question can be answered against a fake server
# or a 4.5 MB/s laptop link.
#
#   1. Does a 64-worker ceiling draw `SlowDown` from real S3?
#      `BACKOFF` is `Step`; `Proportional` holds ~9% fewer connections for the same
#      throughput, which is only worth having if connections provoke throttling.
#      AWSC_RETRY_TRACE=1 is load-bearing here: the pool is told about throttling only
#      after the retry budget is exhausted, so a SlowDown that retry absorbs leaves no
#      other trace. Counting only pool-visible throttles would answer "no" by construction.
#
#   2. Does a single S3 IP cap throughput?
#      macOS puts every socket on one address while EC2 spreads across eight. If one
#      address sustains what eight do, connection spreading is not worth pursuing at any
#      link speed. Pinned via /etc/hosts — SNI and SigV4 still see the hostname, so this
#      changes only which peer the sockets land on.
#
# Both run interleaved and repeated: a single-run comparison over a network has produced
# a confident wrong answer here before.
#
# Usage: bench-pool-open.sh <bucket> <awsc-binary> [GiB] [repeats]
set -euo pipefail

bucket=${1:?usage: bench-pool-open.sh <bucket> <awsc> [GiB] [repeats]}
awsc=${2:?usage: bench-pool-open.sh <bucket> <awsc> [GiB] [repeats]}
gib=${3:-2}
repeats=${4:-3}

# NOT /tmp: it is a RAM-backed tmpfs on Amazon Linux 2023, and a payload plus its round
# trip fills it. Downloads go to /dev/shm on purpose — gp3 EBS tops out around 280 MB/s
# and would measure the disk rather than the client.
work=$(mktemp -d "${BENCH_TMPDIR:-/var/tmp}/pool-open.XXXXXX")
trap 'rm -rf "$work"' EXIT
payload="$work/payload.bin"
results=./pool-open-results.tsv
bytes=$((gib * 1024 * 1024 * 1024))

echo "generating ${gib} GiB payload"
dd if=/dev/urandom of="$payload" bs=1M count=$((gib * 1024)) status=none
[ "$(wc -c < "$payload")" -eq "$bytes" ] || { echo "payload is the wrong size"; exit 1; }

[ -f "$results" ] || printf 'test\tvariant\trun\tdirection\tseconds\tMB_per_s\tslowdowns\tpool_throttles\tdistinct_peers\n' > "$results"

# The host the client actually connects to, taken from the client rather than assumed:
# the endpoint comes out of the vendored ruleset, and guessing it would pin the wrong name.
host=$("$awsc" s3 ls "s3://$bucket" --debug 2>&1 \
  | grep -om1 'https://[^/ ]*' | head -1 | sed 's|https://||')
[ -n "$host" ] || { echo "could not determine the S3 host"; exit 1; }
echo "S3 host: $host"

addresses=$(getent ahostsv4 "$host" | awk '{print $1}' | sort -u)
echo "resolves to:"; echo "$addresses" | sed 's/^/  /'
one_ip=$(echo "$addresses" | head -1)

# One transfer. Records throughput, the SlowDowns retry saw, the throttles the pool saw,
# and how many distinct peers the sockets landed on.
run() {
  local test=$1 variant=$2 run=$3 direction=$4; shift 4
  local log="$work/run.log" peers="$work/peers.txt" start end secs mbps
  : > "$peers"

  start=$(date +%s.%N)
  AWSC_RETRY_TRACE=1 AWSC_POOL_TRACE=1 "$@" > "$log" 2>&1 &
  local pid=$!
  # `-a` is load-bearing: lsof ORs its selection criteria, so `-p PID -i` without it
  # reports every connection on the machine rather than this process's.
  while kill -0 "$pid" 2>/dev/null; do
    lsof -a -p "$pid" -i -n -P 2>/dev/null \
      | awk '/ESTABLISHED/ {split($9,a,"->"); split(a[2],b,":"); if (b[2]=="443") print b[1]}' >> "$peers"
    sleep 0.2
  done
  if ! wait "$pid"; then
    echo "FAILED: $test/$variant/$direction"; tail -5 "$log"; return 1
  fi
  end=$(date +%s.%N)

  secs=$(echo "$end - $start" | bc)
  mbps=$(echo "scale=1; $bytes / $secs / 1048576" | bc)
  printf '%s\t%s\t%s\t%s\t%.1f\t%s\t%s\t%s\t%s\n' \
    "$test" "$variant" "$run" "$direction" "$secs" "$mbps" \
    "$(grep -c 'code SlowDown' "$log" || true)" \
    "$(grep -c 'THROTTLED' "$log" || true)" \
    "$(sort -u "$peers" | grep -c . || true)" | tee -a "$results"
}

# ---------------------------------------------------------------- 1. throttling vs load
# 64 is the shipped ceiling; 16 is what the ceiling used to be on a 4-vCPU box, and is the
# control. If throttling is drawn by connection count, it shows up as a gap between these.
# `adaptive` is what actually ships — pinning the pool switches the controller off, so the
# pinned arms alone would answer a question nobody runs into.
for i in $(seq 1 "$repeats"); do
  for conc in 16 64 adaptive; do
    conc_flag=(--concurrency "$conc")
    [ "$conc" != adaptive ] || conc_flag=()
    run throttle "conc-$conc" "$i" up \
      "$awsc" s3 cp "$payload" "s3://$bucket/pool-$conc.bin" "${conc_flag[@]}" --no-progress
    run throttle "conc-$conc" "$i" down \
      "$awsc" s3 cp "s3://$bucket/pool-$conc.bin" /dev/shm/back.bin "${conc_flag[@]}" --no-progress
    cmp -s "$payload" /dev/shm/back.bin || { echo "ROUND TRIP CORRUPTED at conc=$conc"; exit 1; }
    rm -f /dev/shm/back.bin
  done
done

# ------------------------------------------------------------------- 2. one IP vs many
# Interleaved rather than one block each, so a drift in the link cannot masquerade as an
# effect of the pinning.
for i in $(seq 1 "$repeats"); do
  for variant in many-ip one-ip; do
    if [ "$variant" = one-ip ]; then
      printf '%s %s\n' "$one_ip" "$host" >> /etc/hosts
    fi
    run spread "$variant" "$i" up \
      "$awsc" s3 cp "$payload" "s3://$bucket/spread.bin" --no-progress
    run spread "$variant" "$i" down \
      "$awsc" s3 cp "s3://$bucket/spread.bin" /dev/shm/back.bin --no-progress
    cmp -s "$payload" /dev/shm/back.bin || { echo "ROUND TRIP CORRUPTED at $variant"; exit 1; }
    rm -f /dev/shm/back.bin
    if [ "$variant" = one-ip ]; then
      sed -i "\|^$one_ip $host\$|d" /etc/hosts
    fi
  done
done

echo
column -t "$results"
