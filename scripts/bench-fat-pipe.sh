#!/usr/bin/env bash
#
# The fat-pipe benchmark, meant to run *in-region* on EC2 rather than on a laptop.
#
# Three questions, in the order they have to be answered:
#
#   1. Does a transfer already spread its connections across the several IP addresses S3
#      resolves to? DNS rotates the seven A records per query and hyper resolves per
#      connection, so spreading may already happen for free. Measured, not assumed.
#   2. What part size is right when the link is fast? The 8 MiB default is ~2s of
#      transfer at 4.5 MB/s and ~7ms at 1.2 GB/s, at which point the per-part round trip
#      is the whole cost.
#   3. How does concurrency interact with the above?
#
# Usage: bench-fat-pipe.sh <bucket> <awsc-binary> [size-in-GiB]
#
# It writes one TSV row per run to ./fat-pipe-results.tsv and leaves the bucket's objects
# behind for the caller to delete.
set -euo pipefail

bucket=${1:?usage: bench-fat-pipe.sh <bucket> <awsc> [GiB]}
awsc=${2:?usage: bench-fat-pipe.sh <bucket> <awsc> [GiB]}
gib=${3:-2}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
payload="$work/payload.bin"
results=./fat-pipe-results.tsv

# Incompressible, so nothing in the path can cheat. `openssl rand` is far faster than
# /dev/urandom for gigabyte-scale payloads.
echo "generating ${gib} GiB payload"
openssl rand -out "$payload" $((gib * 1024 * 1024 * 1024))
bytes=$(wc -c < "$payload")

[ -f "$results" ] || printf 'direction\tchunk\tconcurrency\tseconds\tMB_per_s\tdistinct_peers\tpeer_samples\n' > "$results"

# Sample the distinct :443 peers a pid holds, until it exits.
sample_peers() {
  local pid=$1 out=$2
  : > "$out"
  while kill -0 "$pid" 2>/dev/null; do
    # `-a` is load-bearing: lsof ORs its selection criteria, so `-p PID -i` without it
    # reports every connection on the machine, not the process's.
    lsof -a -p "$pid" -i -n -P 2>/dev/null \
      | awk '/ESTABLISHED/ {split($9,a,"->"); split(a[2],b,":"); if (b[2]=="443") print b[1]}' >> "$out"
    sleep 0.2
  done
}

run() {
  local direction=$1 chunk=$2 conc=$3; shift 3
  local peers="$work/peers.txt"
  local start end secs

  start=$(date +%s.%N)
  "$@" > "$work/cmd.log" 2>&1 &
  local pid=$!
  sample_peers "$pid" "$peers" &
  local sampler=$!
  if ! wait "$pid"; then
    wait "$sampler" 2>/dev/null || true
    echo "FAILED: $direction chunk=$chunk conc=$conc"
    tail -3 "$work/cmd.log"
    return 1
  fi
  wait "$sampler" 2>/dev/null || true
  end=$(date +%s.%N)

  secs=$(echo "$end - $start" | bc)
  local mbps distinct samples
  mbps=$(echo "scale=1; $bytes / $secs / 1048576" | bc)
  distinct=$(sort -u "$peers" | grep -c . || true)
  samples=$(wc -l < "$peers" | tr -d ' ')
  printf '%s\t%s\t%s\t%.1f\t%s\t%s\t%s\n' \
    "$direction" "$chunk" "$conc" "$secs" "$mbps" "$distinct" "$samples" | tee -a "$results"
}

for chunk in 8MB 16MB 32MB 64MB 128MB; do
  for conc in default 20 40; do
    conc_flag=()
    [ "$conc" = default ] || conc_flag=(--concurrency "$conc")

    run up "$chunk" "$conc" \
      "$awsc" s3 cp "$payload" "s3://$bucket/bench-$chunk-$conc.bin" \
      --multipart-chunksize "$chunk" --no-progress "${conc_flag[@]}"

    run down "$chunk" "$conc" \
      "$awsc" s3 cp "s3://$bucket/bench-$chunk-$conc.bin" "$work/back.bin" \
      --multipart-chunksize "$chunk" --no-progress "${conc_flag[@]}"

    cmp -s "$payload" "$work/back.bin" || { echo "ROUND TRIP CORRUPTED at $chunk/$conc"; exit 1; }
    rm -f "$work/back.bin"
  done
done

echo
echo "reference CLI, for scale:"
run ref-up 8MB default aws s3 cp "$payload" "s3://$bucket/bench-reference.bin" --no-progress
run ref-down 8MB default aws s3 cp "s3://$bucket/bench-reference.bin" "$work/back.bin" --no-progress

echo
column -t "$results"
