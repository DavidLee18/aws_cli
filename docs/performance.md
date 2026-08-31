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

### 2. Coverage gaps

Not performance work, but the pivot asked for as much of the AWS API as possible.

- ~~**Event streams are not implemented at all.**~~ Done, for response streams.
  `vnd.amazon.eventstream` framing, both checksums, indefinite reassembly across reads,
  and the `eventHeader`/`eventPayload` split. Events print as JSON Lines, flushed per
  event — a stream that may never end (`logs start-live-tail`) cannot be collected into
  one document first.

  Seventeen operations the reference removes *only* because it cannot read an event
  stream are now visible: `kinesis subscribe-to-shard`, `bedrock-runtime converse-stream`
  and `invoke-model-with-response-stream`, `lambda invoke-with-response-stream`,
  `logs get-log-object`, the six bedrock-agent-runtime ones, and the rest.
  `data/customizations.json` still records what botocore does — the override lives in
  `customizations.rs`, with a test asserting every entry names a removal that really
  exists, so a typo cannot silently do nothing.

  **Duplex streams work too.** Request events are read from stdin as JSON Lines in the
  same `{"EventName": {...}}` shape response events print. Each frame carries its own
  signature in a chain seeded by the initial request's, which is why the chain is
  advanced on one thread in order — signing two frames concurrently produces a pair the
  service rejects.

  The threading is worth remembering: both directions need the model, and `Model` is not
  `Sync`, so neither encoding nor interpreting can leave the main thread. The two jobs
  that *are* moved off it are the ones needing only bytes — reading stdin, and running
  the HTTP call. They report into one channel and the main thread runs an event loop over
  it.

  Verified against `scripts/verify-event-signing.py`, which re-implements the signing
  chain from the spec in Python: agreement between it and the Rust is evidence rather
  than a tautology. It also proves the duplex property by timestamp — a reply arrives
  1.8s before the next request frame is sent — and the negative control (signing with the
  wrong secret) is rejected, so the check is not vacuous.

  Blobs inside an event are base64 in **both** directions, unlike blob parameters
  elsewhere in the CLI, which are raw text in and base64 out. That asymmetry is
  botocore's; a stream cannot live with it, because its blobs are audio and model output
  with no text form, and a stream is something you feed back what you were just given.
- ~~**`rpcv2Cbor` is not implemented.**~~ Done. `partnercentral-revenue-measurement`,
  which speaks only CBOR, is now reachable.

  `rpcv2Cbor` is now **first** in `Protocol::TRAIT_TABLE`, which is a preference list
  rather than a lookup, so all 16 services that declare it use it — cloudwatch, gamelift,
  compute-optimizer, snowball, appstream and the rest, which previously took an
  `awsJson` path. CBOR is the more compact encoding of the two they offer: integers are
  integers rather than decimal text, member names are length-prefixed rather than quoted
  and escaped.

  Confirmed twice. `scripts/verify-cbor-requests.py` is a decoder written from RFC 8949
  rather than from the Rust, so a request it can read is evidence rather than a shared
  bug: all 16 round-tripped with correct `smithy-protocol`, `Content-Type` and `Accept`
  headers, the operation in the path, a body that decodes and consumes itself exactly,
  and a CBOR response parsed back. `cloudwatch put-metric-data` exercised a structure
  nested in a list, a float, and a tag-1 timestamp.

  Then against **live AWS**: 13 of the 16 returned real data, and the other three
  answered *modelled business errors* (`InvalidParameterValueException`,
  `ValidationException`) — which equally prove the round trip, since a protocol failure
  looks like `UnknownOperationException` or a 415, not a business error.

  That live pass is what found the endpoint bug below. Only the offline check had been
  run when the flip was committed, and it could not have caught it: a local endpoint
  override bypasses ruleset resolution entirely.

### 3. ~~Operation-level endpoint parameters~~ — done

`smithy.rules#staticContextParams` was not read at all, and that resolves a *different
host* rather than failing. `arc-region-switch list-plans` sets `UseControlPlaneEndpoint`
and belongs on `arc-region-switch-control-plane.<region>.api.aws`; without it the request
went to `arc-region-switch.<region>.api.aws`, which answers `UnknownOperationException`
with an empty message. 297 operations across 8 services carry the trait.

Reading it exposed a second gap immediately: `s3api` never supplied `Bucket` to the
ruleset. That had been harmless, because with `Bucket` unset S3's ruleset fell through to
path-style — but `create-bucket` sets `UseS3ExpressControlEndpoint`, so once static
params were honoured *every* bucket resolved to the S3 Express control endpoint.

Supplying `Bucket` then exposed a third: the endpoint accounts for the bucket (in the host
for virtual-host addressing, in `path_prefix` for a dotted name that no wildcard
certificate matches), while the operation's `smithy.api#http` URI template still starts
with `/{Bucket}` — so the bucket appeared twice and every `s3api` call taking a bucket
returned 404 or `NoSuchKey`. `Client::operation_path` drops the segment, matching only a
whole one so a bucket named `logs` cannot eat the `/logs-2` of an unrelated path.

A fourth followed, found on returning to the branch: the *member-bound* sibling,
`smithy.rules#contextParam`, was not read either. It puts the value of an input argument
into the ruleset — `s3control` binds `AccountId` that way, and every one of its **97
operations** resolves through it, so the entire service answered "AccountId is required but
not set" no matter what was passed. Seven services declare the trait; the other six were
checked live and were already resolving correctly (their parameters only matter for
ARN-routed and multi-region-access-point cases), so `s3control` took the whole of the
damage. Both sources now feed one parameter set, with the operation's constants applied
last.

The lesson worth keeping: three of those four only appeared *after* the previous fix, and
none of them are visible against a local endpoint override — it bypasses ruleset
resolution entirely. Endpoint resolution has to be checked against real AWS, and the whole
sweep re-run after each step rather than just the case being chased.

### 4. The fat-pipe items

All three are now settled, measured on a temporary c7g.xlarge in us-east-1 -- the
development machine's uplink saturates at ~4.5 MB/s (~36 Mbps), which is why they sat
here so long. Two of the three needed no fat pipe in the end, and the one real fix turned
out to be in a fourth place none of them named.

**CRC32C in place of SHA-256 — retired, there is nothing to replace.** The premise was
that a fat pipe would outrun the payload hash. It would: `sha2` measures **202 MB/s**
here (1.6 Gbps, the pure-Rust path — Apple's SHA extensions are not being used), which a
10 Gbps link beats several times over. But the bulk path does not hash payload bytes at
all. A file-backed body signs `UNSIGNED-PAYLOAD` (`http::payload_hash`), and multipart
parts are `Body::FileRange`, so the only payload SHA-256 left is over small in-memory
request bodies, where it is noise. Adding CRC32C would *add* a pass over the data, not
remove one. If payload hashing ever becomes necessary, 202 MB/s is the number that makes
hardware SHA-256 worth wiring up first — that, not CRC32C, is the fix.

**Connection spreading — real on the laptop, absent in production, so not worth code.**
S3 answers with seven or eight A records, rotated per query, and hyper resolves per
connection. Measured with `lsof` (which needs `-a`, or it ORs its filters and reports the
whole machine): on macOS a ten-worker multipart upload put all ten sockets on **one**
address, because the OS resolver caches and hands back a stable order. On EC2 the same
binary spread across **all eight**. So the platform that pins to a single address is the
one whose link is far too slow to care, and the platform that could saturate an address
already spreads. An untested gap remains -- macOS on a genuinely fast non-AWS link -- but
implementing a resolver shim for it, unmeasured, is how a fix becomes a regression.

**Measured, finally.** A c7g.xlarge in us-east-1 gives ~200 MB/s single-stream to S3 and
over 1.2 GB/s with concurrency -- 45x the development link, which is what the section was
waiting for. Two cautions the numbers only gave up under pressure. Downloads flat-lined at
~280 MB/s in every configuration, which looked like a client ceiling and was the gp3 root
volume: repeating each download into `/dev/shm` instead put the same client at 1183 MB/s.
And uploads read from page cache while downloads write to disk, so the two directions are
not comparable unless the disk is taken out of both.

**Part sizing — the knob is worth having; the default was never the problem.** Uploading
2 GiB, MB/s, chunk size against worker count:

| chunk | adaptive | 20 | 40 |
|-------|---------:|---:|---:|
| 8 MB  | 603 | 716 | 1093 |
| 16 MB | 774 | 945 | 1130 |
| 32 MB | 871 | 972 | 1084 |
| 64 MB | 778 | 971 | 1122 |
| 128 MB| 734 | 1090 | 974 |

At 40 workers every size lands within noise of each other; the +44% that 32 MB buys over
8 MB at adaptive concurrency is really just more bytes in flight compensating for too few
workers. Part size is a knob for unusual links, not a default to adapt. It stays at 8 MiB.

**Concurrency was the lever.** Sweeping workers at a fixed 16 MB chunk, 2 GiB per run,
downloads into `/dev/shm` so the disk is out of the path:

| workers | up | down |
|---------|---:|-----:|
| 10  | 607 | 744 |
| 20  | 1049 | 1152 |
| 40  | 1096 | 1165 |
| 80  | 1072 | 1183 |
| 160 | 1049 | 1078 |

The pool was reaching none of that. Its ceiling was `(cores * 4).clamp(8, 64)` -- 16 on a
4-vCPU instance, under the measured optimum -- and cores are the wrong unit for work that
spends its life waiting on a socket. Its ramp then added two workers per 150ms sample, so
a two-second transfer, thirteen samples, reached 36 only as it ended. A flat ceiling of 64
and a ramp that grows by half again took adapting from 603 to 921 MB/s up (+53%) and 744
to 1157 down (+55%), against 1030/1281 for a hand-pinned 40. The remaining ~10% is the
~600ms the ramp costs, which is 30% of a two-second transfer and 3% of a twenty-second
one, so it amortises with size.

The knob itself stays, because sweeping it is how the above was established:
`--multipart-chunksize` and `--multipart-threshold` set part sizing (`8MB`, `8MiB` and
`8388608` all mean 2^23; an unreadable value keeps the default rather than failing the
transfer), clamped to S3's 5 MiB minimum and doubled as needed to stay under 10,000 parts.
`scripts/bench-fat-pipe.sh` runs the sweep; it must run in-region, and its downloads must
land somewhere other than an EBS volume or they measure the disk.

### 4b. The pool follow-ups — done

Three follow-ups came out of the pool fix. All three are now settled, the last two on
in-region measurements against real S3.

**Idle workers no longer poll.** Raising the ceiling to 64 meant up to 54 threads sitting
above the target in a 25ms sleep-loop. Measured on this machine, 64 threads idling for 2s:
**94.6ms of CPU** for the poll loop against **11.4ms** for a condvar with a 250ms backstop,
so ~47ms/s became ~6ms/s. Growth notifies the condvar; the timeout is only a guard against
a missed wakeup. Thread spawning was never the cost -- 64 threads cost 1.96ms per transfer
against 0.59ms for 16.

**Throttling behaves, and the pool is not the main line of defence.** The fake server can
answer `503 SlowDown` over a request window (`FAKE_S3_SLOWDOWN=from:until`). A burst
mid-transfer is absorbed entirely by the retry policy: the transfer completes, the round
trip is byte-identical, and the pool never sees a throttle event at all -- `note_throttle`
fires only once retries are *exhausted*. A burst longer than the retry budget does fail the
transfer, which is correct: `max_attempts` is 3 by default. So the pool's throttle branch
answers *sustained* throttling only.

That is also why `AWSC_RETRY_TRACE=1` exists. It reports every retried response at the one
choke point all transfer requests pass through. Without it the throttling question below
could not be asked honestly: counting only pool-visible throttles would report "no
throttling" no matter what the service did, because retry absorbs the ordinary case.

**Connections do not provoke throttling here, so the back-off shape does not matter.**
Measured on a c7g.xlarge in us-east-1, 2 GiB per transfer, three repeats per arm, every
round trip verified with `cmp`:

| arm | up MB/s | down MB/s | SlowDown responses |
|---|---|---|---|
| pinned 16 | 717 | 1104 | 0 |
| pinned 64 | 970 | 1171 | 0 |
| adaptive | 791 | 1057 | 0 |

**Zero** `SlowDown` responses across 30 transfers, counting the ones retry absorbs. The
proportional back-off was carried for exactly this question -- replaying the controller
against the measured curve, it holds 9% fewer connections for equal throughput at +/-15%
sampling noise -- and fewer connections buy nothing if connections cost nothing. It was
also never able to affect the throttled path, since that branch halves unconditionally
without consulting `shrunk`. Removed rather than left switched off.

**A single S3 address sustains what eight do.** Pinning the bucket's hostname to one
address in `/etc/hosts` (SNI and SigV4 still see the hostname; only the peer changes)
against normal DNS, same instance, interleaved, three repeats:

| variant | up MB/s | down MB/s | distinct peers |
|---|---|---|---|
| normal DNS | 864 | 1061 | 8-16 |
| one address | 920 | 1044 | 1 |

Within noise, and the pinned arm is *faster* on upload. This closes the spreading item for
good: macOS putting every socket on one address is not a handicap worth engineering around
at any link speed.

**What the run turned up instead: the ramp costs more than the back-off ever could.** The
adaptive arm runs 18% below a pinned 64 on upload (791 against 970) and 10% below on
download. The control trace says why -- the pool starts at 10 workers and takes five to
seven 150ms samples to reach the ceiling:

```
pool: workers 10 -> 15 rate 80.0/s
pool: workers 15 -> 22 rate 106.6/s
pool: workers 22 -> 21 rate 93.1/s
pool: workers 21 -> 31 rate 153.2/s
pool: workers 31 -> 30 rate 126.4/s
pool: workers 30 -> 45 rate 153.2/s
pool: workers 45 -> 44 rate 113.3/s
pool: workers 44 -> 64 rate 253.2/s
```

That is roughly 0.9s of a 2.5s transfer spent below the level the link supports, and the
dips (22 -> 21, 31 -> 30) are the degradation branch firing on sampling noise on the way
up. On a long transfer the ramp amortises to nothing; on a short one it is the whole
difference. Not yet acted on -- a faster ramp trades against overshooting on a slow link,
which is the measurement that would have to come first.

### 5. ~~Smaller~~ — done

- **Body-less failures.** Every HEAD operation answers a failure with a status and no
  body, so the error parsers fell through to `An error occurred (Unknown) ... :` with an
  empty message. They now use the status and its reason phrase:
  `An error occurred (404) when calling the HeadBucket operation: Not Found`.
- **`--no-verify-ssl` and `--ca-bundle`** were refused rather than implemented (this
  predates the transport rewrite -- the ureq version refused them identically). Both now
  work, built on a hand-made `rustls::ClientConfig`; the default path still uses
  hyper-rustls's native-root loading unchanged.
- **Connect errors carry their cause.** hyper renders a failed connect as
  `client error (Connect)` and nothing else. The `source()` chain is now appended, which
  is the difference between that and `invalid peer certificate: UnknownIssuer`.

### 6. ~~Request bodies and argument arity~~ — done

Four bugs, all of the same family: something the request needed was silently absent.

**Argument arity was guessed syntactically.** `parse` took exactly one value per flag, so
`--instance-ids i-1 i-2` sent only `i-1` — the rest vanished into positionals with no
error. Taking *every* following token instead would have eaten `get-object`'s trailing
outfile, so arity has to come from the shape: a list member keeps them all, a scalar one,
a boolean none. `parse` now collects greedily and `args::rebalance` hands back what did
not belong, once the model is loaded. It runs before the outfile is read, because the
outfile is exactly such a positional.

The `aws s3` tree answers the same question up front — its flags are hand-written, not
modelled — from a table in `s3::flag_arity`. That fixed a third bug found on the way:
`s3 cp --recursive SRC DST` failed with `Unknown options: SRC`, because the boolean
swallowed it; the flag only worked when written last. A test reads `transfer.rs`'s own
match arms and asserts the table agrees with them, since a flag missing from it silently
gets `Arity::One` and swallows a positional again.

**restXml sent no request body at all.** There was no XML request serializer — the arm
was `_ => (String::new(), None)`. 160 operations across s3, s3-control, cloudfront and
route-53 were affected; `s3api put-bucket-tagging` sent an empty document and the service
saw an empty request. The serializer mirrors the response parser: `xmlName`, wrapped vs
`xmlFlattened` lists, maps, `xmlAttribute` (exactly one member in the whole catalogue —
S3's `Grantee$Type` — but ACL grants are wrong without it), and the service-level
`xmlNamespace` that every S3 body carries.

Bodies were compared byte-for-byte against the reference for six operations. Only member
*order* differed: the reference emits the user's input order, this emits model order.
Checked live rather than assumed — S3 accepted model order and read both documents back
correctly, so it is order-tolerant and the difference does not matter.

**A streaming payload was encoded rather than sent.** `put-object --body <file>` hashed
an empty body and S3 answered `MissingContentLength`. The member's value is a path, and
it is now described as a `Body::FileRange` rather than read, so a 5 GB upload stays a file
handle.

**`Content-MD5` was never sent.** 27 S3 operations and 37 in S3 Control carry
`aws.protocols#httpChecksum` with `requestChecksumRequired`, and refuse the request
without it. Verified live: tagging, lifecycle, CORS, versioning, ownership controls and
`delete-objects` all round-trip through a temporary bucket, which was then deleted.

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
