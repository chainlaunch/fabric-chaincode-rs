# Verification: Why to Trust This Implementation

*Explanation — the case for this shim's correctness, written for reviewers
who are (rightly) skeptical of AI-assisted Rust reimplementations of
protocol-level infrastructure.*

## Provenance

This crate was built with AI assistance (Claude, via Claude Code), under the
direction and review of [David Viejo](https://github.com/dviejokfs), CTO at
KFS and creator of [Bevel Operator
Fabric](https://github.com/hyperledger/bevel-operator-fabric) (a Hyperledger
Foundation project) — six years of production Hyperledger Fabric work
predates this repo. We say this plainly rather than let it be discovered: AI
assistance means more surface area got built and tested than one person
typically has time for, not that verification was skipped. The rest of this
document is the evidence that substitutes for "trust us."

## 1. Differential testing against the official Go reference chaincode

The strongest evidence: [`scripts/differential-test.sh`](../scripts/differential-test.sh)
stands up a **real, vanilla Fabric 3.1.5 network** (`fabric-samples`
test-network — no ChainLaunch involved anywhere in this test), deploys the
**unmodified, official** `asset-transfer-basic/chaincode-external` Go
chaincode from `hyperledger/fabric-samples` side by side with this repo's
Rust `asset-transfer` example, and runs an identical sequence of
invokes/queries against both — asserting the JSON results are **byte-for-byte
identical**.

Run it yourself: `./scripts/differential-test.sh` (needs Docker; downloads
Fabric binaries/images and a pinned `fabric-samples` commit into a temp
directory, tears everything down on exit). CI runs this on every push (job
`differential-fabric` in [`ci.yml`](../.github/workflows/ci.yml)).

What it checks, and confirmed results from a live run:

- **`InitLedger`-seeded state matches**: `ReadAsset("asset1")` returns
  `{"ID":"asset1","color":"blue","size":5,"owner":"Tomoko","appraisedValue":300}`
  from both implementations, identically.
- **`CreateAsset` + `ReadAsset`** on a fresh key: identical JSON.
- **`TransferAsset`**: identical post-transfer state.
- **`GetAllAssets`**: identical JSON array, including key ordering (LevelDB
  range iteration order matches between implementations) and the exact
  `{"Key":..., "Record":...}` wrapper shape the Go sample uses.
- **Error path**: querying a missing key is rejected by both (nonzero exit),
  confirming equivalent failure behavior, not just equivalent happy-path
  behavior.

**A real mismatch this test found and fixed:** the first run of this exact
test caught a genuine bug — this repo's `GetAllAssets` originally returned a
bare `Vec<Asset>`, while the official Go sample wraps each entry in
`{"Key": ..., "Record": ...}`. That's not a shim protocol issue (state
get/put, range iteration, and JSON marshaling were already correct) — it was
an application-level convention mismatch in the example chaincode, caught
specifically because the test compares against the real upstream
implementation instead of only checking self-consistency. Fixed by adding a
`QueryResult` wrapper type matching Go's exact JSON shape.

## 2. Fuzzing the protocol boundary

`fuzz/` (via [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz))
targets the code paths that parse **peer-controlled input** — the one thing
a chaincode process cannot choose to trust:

| Target | What it fuzzes |
|---|---|
| `decode_chaincode_message` | Top-level `ChaincodeMessage` protobuf decode — the first thing parsed off the wire |
| `decode_proposal_chain` | The full nested decode chain `ChaincodeStub::new` runs: `SignedProposal → Proposal → (Header, ChaincodeProposalPayload) → (ChannelHeader, SignatureHeader)` |
| `composite_key_roundtrip` | `create_composite_key`/`split_composite_key` round-trip on arbitrary (including invalid-UTF-8-derived) input |

**A real bug this found and fixed, day one:** within the first 45-second
fuzz run, `composite_key_roundtrip` found that `create_composite_key("", &[])`
succeeded but the resulting key could not be parsed back by
`split_composite_key` — an empty `object_type` was silently swallowed by the
trailing-delimiter filter, breaking the round-trip invariant. Fixed by
rejecting empty components in `validate_composite_component` (see
`fabric-shim/src/stub.rs`), with a regression test added and the fix
confirmed by replaying the original crash input (now passes) and re-running
all three targets crash-free.

CI runs a bounded 60-second smoke-fuzz per target on every push (job `fuzz`
in `ci.yml`) — enough to catch an obvious regression, not exhaustive. Run
longer locally for deeper coverage:

```bash
cargo +nightly fuzz run composite_key_roundtrip -- -max_total_time=3600
```

## 3. Supply-chain and memory-safety posture

- **`#![forbid(unsafe_code)]`** on every crate (`fabric-shim`,
  `fabric-shim-protos`, `fabric-shim-macros`, and the `asset-transfer`
  example) — enforced by the compiler, not a claim in a README. Verified
  zero `unsafe` blocks exist anywhere in this codebase (including the
  committed generated protobuf bindings) before adding the attribute.
- **`cargo audit`** — scans `Cargo.lock` against the RustSec advisory
  database (1166 advisories as of this writing). Clean across all 118
  dependencies. Runs in CI on every push.
- **`cargo deny check`** — enforces license policy (`deny.toml`: only
  MIT/Apache-2.0/BSD-3-Clause/Unicode-3.0 dependencies allowed), bans, and
  source allowlisting (crates.io only, no arbitrary git/registry
  dependencies). Runs in CI on every push.

## 4. Standard test suite

- 19 tests in `fabric-shim/tests/mock_peer.rs`: an in-process mock peer (a
  real gRPC client, exactly like the Fabric peer in CCaaS mode) drives the
  handshake, ledger request/response round trips, error surfacing, range
  query pagination, interleaved concurrent transactions (proving response
  routing is correct under concurrency), panic isolation (a panicking
  handler must not kill the connection), `GetMetadata` interception, and the
  `#[contract]`/`#[derive(DataType)]` macro's routing and argument parsing.
- All of the above run in CI on every push and pull request — see
  [`ci.yml`](../.github/workflows/ci.yml) for the exact jobs and commands.

## 5. Performance vs. the Go and TypeScript reference chaincodes

[`scripts/benchmark.sh`](../scripts/benchmark.sh) extends the same real
Fabric 3.1.5 network setup used for differential testing to a third
chaincode — the official, unmodified `asset-transfer-basic/chaincode-typescript`
— and times `peer chaincode invoke`/`query` latency against all three.

**Read this caveat before the numbers**: this measures **end-to-end latency
through the `peer chaincode` CLI** — process spawn, TLS handshake, gRPC to
the peer, endorsement, the peer-to-chaincode RPC, and the response. It is
*not* a microbenchmark of chaincode execution time alone; CLI/network
overhead dominates at this scale, for a simple get/put workload. Treat the
results as "these three chaincodes perform comparably under real Fabric
traffic," not as a precise ranking of language execution speed. A rigorous
benchmark would drive load from a persistent client (e.g. Hyperledger
Caliper or a `fabric-gateway` SDK loop) instead of spawning a CLI process
per call — that's a documented gap, not a hidden one.

Results from a local run (`./scripts/benchmark.sh 30`, N=30 per operation —
the same run used for the infra numbers in §6 below):

| Chaincode | Op | mean (ms) | p50 | p95 | ops/s |
|---|---|---|---|---|---|
| Go (`chaincode-external`) | query | 36.8 | 34.8 | 44.5 | 27.2 |
| Go | invoke | 42.3 | 41.0 | 49.3 | 23.7 |
| **Rust (`fabric-shim`)** | query | 37.9 | 38.1 | 43.1 | 26.4 |
| **Rust** | invoke | 43.2 | 42.6 | 49.0 | 23.1 |
| TypeScript (`chaincode-typescript`) | query | 39.1 | 37.6 | 44.6 | 25.6 |
| TypeScript | invoke | 46.4 | 46.0 | 50.7 | 21.6 |

All three cluster within noise of each other — no chaincode shows a
meaningful edge at this workload and sample size.

**A real bug this found and fixed**: the first benchmark run showed Rust
~1.6-2.6x *slower* than Go/TypeScript (query mean 59.5ms vs ~42ms, invoke
mean 110.3ms vs ~45-48ms) — worth investigating rather than either hiding
or shipping. The cause: `fabric-shim`'s server binds its own `TcpListener`
and hands it to `tonic` via `serve_with_incoming_shutdown`, which bypasses
tonic's usual listener setup — including its `tcp_nodelay` option. Nagle's
algorithm was therefore left enabled on accepted connections, and this
shim's request/response pattern (small message, expects an immediate reply)
is exactly what Nagle-plus-delayed-ACK interaction stalls, typically by
~40ms per round trip. Fixed by calling `set_nodelay(true)` on each accepted
connection explicitly (see `fabric-shim/src/server.rs`); the numbers above
are post-fix. Pre-fix numbers are preserved in this section instead of
quietly dropped.

## 6. Infrastructure footprint: image size, startup, CPU/memory

Same benchmark run also captures what actually matters for running these
chaincodes at scale on real infrastructure — not just per-call latency.

**Image size** (`docker images`, same build used for the latency run):

| Chaincode | Image size |
|---|---|
| Go (`chaincode-external`) | 840 MB |
| **Rust (`fabric-shim`)** | **52.5 MB** |
| TypeScript (`chaincode-typescript`) | 1.66 GB |

Rust's image is ~16x smaller than Go's and ~32x smaller than TypeScript's —
a static-ish binary in a slim runtime layer vs. the Go toolchain's base
image and Node's `node_modules`, respectively.

**Container CPU/memory** (`docker stats`, sampled every 0.3s: once idle
before any load, then continuously during the query/invoke loop):

| Chaincode | idle MB | avg MB (under load) | peak MB | avg CPU% | peak CPU% |
|---|---|---|---|---|---|
| Go | 12.2 | 13.1 | 13.3 | 1.5 | 3.1 |
| **Rust** | **1.8** | **2.1** | **2.1** | **0.7** | **1.4** |
| TypeScript | 173.7 | 177.4 | 177.5 | 6.9 | 13.8 |

Rust uses roughly 1/6th the memory of Go's already-lean footprint, and
~85x less than the Node.js runtime. This is the one metric in this
document where the language runtime itself, not CLI/network overhead,
is what's being measured — and it's not close.

**Container readiness time** (elapsed from `docker run` to the first
successful `peer chaincode query` against that container — proof the
REGISTER/READY handshake with the peer completed): all three chaincodes
measured **0.07s** in the same run. Read this as a measurement-floor
result, not a real tie: the probe's granularity is bounded below by the
`peer chaincode query` CLI process's own spawn time (roughly the same
order of magnitude), so it cannot resolve true readiness differences
smaller than that floor. Take it as "all three register with the peer
fast enough that CLI overhead dominates," not as evidence they're
identical.

**A further safe optimization applied while investigating this**: beyond
the `TCP_NODELAY` latency fix (§5), the release profile now sets
`lto = "fat"`, `codegen-units = 1`, and `strip = true`
(`Cargo.toml`, `[profile.release]`) — LTO and single-codegen-unit for
better cross-crate inlining, `strip` to drop debug symbols from the
shipped binary. `panic = "abort"` was deliberately *not* used: the
shim's panic-isolation guarantee (a panicking chaincode handler fails
only its own transaction, not the whole process — see
`panicking_chaincode_returns_500_and_stream_survives` in
`fabric-shim/tests/mock_peer.rs`) depends on `panic = "unwind"` plus
catching `tokio::spawn`'s `JoinError`; `abort` would silently break that
guarantee for a marginal size win. Verified behavior-preserving via the
full suite in both dev and release mode
(`cargo test --workspace` / `--release`, 19/19 both), plus
`cargo clippy` and `cargo fmt --check` clean. Effect on the release
binary itself:

| Build | `asset-transfer` binary size |
|---|---|
| Baseline (no LTO/strip) | 4,360,416 bytes |
| `+ lto = "fat", codegen-units = 1` | 3,083,872 bytes (-29%) |
| `+ strip = true` | 2,442,688 bytes (-44% from baseline) |

The Docker image shrank from 52.5 MB to 51.4 MB with `strip` added — a
smaller win at the image level than at the binary level, since the base
runtime layer dominates total image size. Build time increased from
~13.5s to ~28-30s (LTO's cost); this only affects release builds used for
deployment, not the inner dev loop.

**A second optimization pass, this time on allocations in the invoke/query
hot path**: an independent audit (Claude Fable 5, read-only, no code
access beyond `Read`/`Grep`) of `server.rs`, `handler.rs`, `stub.rs`, and
the `#[contract]` macro's generated dispatch, looking specifically for
allocation/copy overhead between a peer RPC arriving and the shim's
response going out. Applied findings:

- **Redundant copies of buffers the shim already owned.** `get_data`
  (`stub.rs`) returned `reply.payload.to_vec()` — an extra memcpy of an
  owned `Vec<u8>` — instead of just `reply.payload`. Same pattern in the
  range/rich-query iterator (`iterators.rs`, once per result row) and in
  `ChaincodeStub::new` (`stub.rs`, re-copying the creator, transient map,
  and every arg/decoration on every transaction instead of moving the
  already-owned `Vec<u8>`s out of the decoded proposal). All four are now
  moves, not copies.
- **An avoidable clone building the ledger-request routing key**
  (`handler.rs`): `PeerLink::request` built the `(channel_id, txid)` key
  once, then cloned it again just to keep a copy for the rare
  connection-closed error path. Now the key is moved into the pending-map
  insert on the common path, and only rebuilt (cheaply, from the same
  borrowed `&str`s) if the send actually fails.
- **Two String allocations in the macro-generated dispatch**
  (`fabric-shim-macros`): `invoke()` did
  `from_utf8_lossy(...).into_owned()` then `.rsplit(':')...to_string()` to
  get a `&str` to match on. `from_utf8_lossy` already borrows for free
  when the bytes are valid UTF-8 (the normal case); matching directly on
  the resulting `&str` needs neither allocation.
- **HTTP/2 flow-control window**: the server left tonic's default ~64KB
  stream window in place despite advertising a 100MiB max message size —
  a multi-MB `get_state` response would stall on window updates across
  extra round trips. Enabled `http2_adaptive_window(Some(true))` so the
  window grows with observed bandwidth instead of a fixed guess.

**Honest framing, not oversold**: none of this is expected to move the
§5 benchmark's 38-43ms end-to-end numbers — CLI process spawn and the TLS
handshake dominate that measurement, and the shim's own contribution was
almost certainly already under a millisecond. These fixes are correct and
free (no behavior change, same 19/19 tests pass in dev and release, clean
clippy/fmt), and they matter for chaincode moving larger values or
running at higher sustained throughput than this benchmark's tiny
asset-transfer payloads exercise — but they are not a fix for a measured
regression, because none was found. One candidate fix (deduplicating a
second, necessary string clone in the peer-response routing path) was
deliberately *not* made: the full `ChaincodeMessage` has to survive
intact past that point to be delivered to the waiting caller, so avoiding
the clone would require a bigger restructuring (a custom borrowed-key
type) for a gain the audit itself judged unmeasurable — not worth the
added complexity.

That prediction was checked, not just asserted: re-ran
`./scripts/benchmark.sh 30` after these fixes. Rust: query mean 36.7ms /
27.3 ops/s, invoke mean 42.9ms / 23.3 ops/s, idle memory 1.9MB — against
the pre-fix numbers in the table above (37.9ms / 26.4 ops/s query, 43.2ms
/ 23.1 ops/s invoke, 1.8MB idle memory). The difference in both directions
is within ordinary run-to-run noise, not a trend — confirming, rather than
just asserting, that these allocation fixes don't move a CLI-dominated
benchmark.

## What this does *not* claim

- No independent third-party security audit has been performed.
- Differential testing covers the `asset-transfer` reference chaincode's
  surface (core CRUD, range queries, events, transient data, error paths) —
  not yet private data, `invoke_chaincode`, or history queries. Extending
  the differential harness to those is open (see `docs/spec.md` §7.4).
- Fuzzing has run for short, bounded durations (seconds to low hours), not
  the sustained multi-day campaigns a hardened security-critical library
  would eventually want.
- The benchmark (§5, §6) is not automated in CI — it's noisy by nature (CLI
  process spawn overhead, shared-machine variance) and unsuited to a
  pass/fail gate. It's a manually-run, documented artifact, not a
  continuously-enforced one. It also measures CLI-driven latency, not a
  proper concurrent-client load test.
- The readiness-time measurement (§6) is bounded below by the probing
  CLI's own spawn overhead, as noted there — it shows all three
  chaincodes register with the peer quickly, not a precise ranking of
  startup speed.

If you find a gap in any of the above, please open an issue — this document
should stay honest about what's actually been checked.
