# Performance work

Notes for the optimization pass that replaced the byte-compatibility goal. The point of
this file is that the *measurements* are the expensive part — re-deriving them costs an
hour of real network time, and several of them are counter-intuitive enough that they will
be re-litigated otherwise.

## The constraint that shapes everything

The development machine's uplink saturates at **~4.5 MB/s (~36 Mbps)**. Measured against a
same-region (ap-northeast-2) bucket with a 256 MiB object:

| concurrency | 1 | 4 | 16 |
|---|---|---|---|
| throughput | 3.7 MB/s | 4.5 MB/s | 4.5 MB/s |

Throughput is flat from 4 connections onward and a single connection already reaches 82% of
the ceiling. `awsc` and the reference CLI land within 6% of each other on bulk transfer —
both are sitting on the line rate, not on client overhead.

**Consequence:** bandwidth-oriented work cannot be validated here. Anything whose payoff is
"move the same bytes faster" will measure as noise. Latency-bound and request-count-bound
work is where the wins have all come from.

A second, related trap: an earlier round of benchmarking used a **us-east-1** bucket from
Korea. That link swung 1.7x run to run (27s -> 45s for the same 30 MiB object), which is
wider than any optimization being tested. One single-run comparison on it produced a
confident and completely wrong "the reference is 2x faster" conclusion. Interleave runs,
and never conclude from n=1.

## Landed

| change | effect |
|---|---|
| Pooled hyper/rustls transport, streaming bodies | connection reuse 1:1 -> ~4:1; upload memory 640 MiB -> a chunk buffer |
| Compiled model container (mmap) | `ec2` startup 147ms -> 10.7ms; `s3api` 26ms -> 14ms |
| SSO role-credential caching | `sts get-caller-identity` 1.10s -> 0.24s (reference 0.57s) |
| Parallel prefix listing | `s3 ls --recursive` 25k keys 3.05s -> ~1.0s (reference 3.52s) |
| Batched `DeleteObjects` | `rm --recursive` over 2,500 objects: 2,504 requests -> 7 |

Small-file uploads (300 files) went from 4.38s — 25% *slower* than the reference — to 1.73s,
roughly twice as fast.

## Queued

### 1. ~~Batch deletes with `DeleteObjects`~~ — done

Landed. `rm --recursive`, `sync --delete` and s3-to-s3 `mv` now go through `s3::delete`,
which sends up to 1,000 keys per POST. Deleting 2,500 objects fell from 2,504 requests
(4 listing + 2,500 deletes) to 7. The reference still issues one request per key.

Two things that came out of it and are worth keeping in mind:

- **A 200 from `DeleteObjects` is not success.** The body carries `<Deleted>` and `<Error>`
  entries per key, so a request that refused one key of a thousand still returns 200. Every
  key is reconciled against the response individually, and a key the response mentions in
  *neither* list is reported as a failure — a truncated response must not read as a
  thousand successful deletions.
- **s3-to-s3 `mv` was discarding its delete result entirely** (`let _ = source_conn.send(...)`).
  A move that copied and then failed to remove the source printed `move:` and exited 0. It
  now reports the failure and exits non-zero. This was a correctness bug the batching work
  only happened to walk into.

### 2. Next

Nothing is queued ahead of the deferred items below. The obvious remaining request-count
targets have been taken: listing is parallel, deletes are batched, credentials are cached.
What is left is either bandwidth-bound (and unmeasurable here, see below) or a matter of
API coverage rather than performance.

### 3. Deferred until a fat pipe is available

These are real but unmeasurable on the current link. Do not implement them blind:

- spreading connections across the several IP addresses S3 resolves to
- part sizing adapted from measured bandwidth rather than a fixed 8 MiB
- CRC32C (hardware) checksums in place of SHA-256 where the protocol allows

Validate on EC2 in-region, or any link well above 36 Mbps.

### 4. Smaller

- `HeadBucket` 404s produce `An error occurred (Unknown) ... :` with an empty message.
  `missing_key_message` handles the `HeadObject` equivalent; `HeadBucket` has no counterpart.

## Rejected, with reasons

Recording these so they are not proposed again.

- **Swapping `serde_json` for `sonic-rs` on the response path.** Measured: parsing 12.6 MB
  of listing XML costs ~0.57s, and we are already 13x faster than the reference at it. But
  real S3 pages at 1000 keys, so listing 100k objects is 100 *sequential round trips* —
  3.0s at a good 30ms RTT. Parsing is 3–16% of the total. Halving it buys 1.5–8%. The win
  was in removing round trips, which is what parallel listing did.
- **HTTP/3 / QUIC.** No AWS endpoint tested (s3, sts, dynamodb) accepts even h2 when curl
  offers it, and none advertise `Alt-Svc`. "Prefer h3, fall back" would add a failed QUIC
  handshake or a timeout to every cold connection and never once use h3.
- **ALPN HTTP/1.1-only.** Tried on the theory that h2 negotiation was serializing the first
  connect. It was not — the stall was the SSO credential call. Reverted; it removes h2 for
  the event-stream APIs (Kinesis `SubscribeToShard`, Transcribe streaming) for no gain.
- **Buffering `ls` stdout.** `stdout().lock()` flushes per newline, so 100k lines is 100k
  syscalls. Measured 0.86s vs 0.87s — nothing — and it makes interactive output arrive in
  chunks. Reverted.

## Measuring

`scripts/fake-s3-server.py` carries the instrumentation:

- `/__stats` — connections vs requests. The ratio is how connection pooling is verified;
  it is what showed pooling was not working (40 connections for 38 requests).
- `/__peak` — peak concurrent in-flight requests.
- `/__seed?n=&bucket=` — synthetic objects for listing benchmarks.
- `FAKE_S3_DELAY` — per-request latency, which is what makes round-trip-bound work visible.

It paginates at 1000 keys like real S3. It is not a signing oracle — it never checks a
signature, so signing changes still need real AWS.

For connection behaviour against real S3, sample `lsof -p <pid> -i | grep -c ESTABLISHED`
in a loop. That is what located the SSO stall: one socket to a us-east-1 address held for a
second before any S3 connection opened.
