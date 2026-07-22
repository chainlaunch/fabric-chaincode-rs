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

Results from a local run (`./scripts/benchmark.sh 30`, N=30 per operation):

| Chaincode | Op | mean (ms) | p50 | p95 | ops/s |
|---|---|---|---|---|---|
| Go (`chaincode-external`) | query | 40.3 | 36.0 | 100.9 | 24.8 |
| Go | invoke | 43.5 | 43.1 | 51.6 | 23.0 |
| **Rust (`fabric-shim`)** | query | 36.1 | 36.0 | 39.5 | 27.7 |
| **Rust** | invoke | 46.6 | 42.6 | 55.7 | 21.5 |
| TypeScript (`chaincode-typescript`) | query | 36.6 | 35.5 | 40.7 | 27.3 |
| TypeScript | invoke | 45.4 | 44.6 | 52.2 | 22.0 |

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

## What this does *not* claim

- No independent third-party security audit has been performed.
- Differential testing covers the `asset-transfer` reference chaincode's
  surface (core CRUD, range queries, events, transient data, error paths) —
  not yet private data, `invoke_chaincode`, or history queries. Extending
  the differential harness to those is open (see `docs/spec.md` §7.4).
- Fuzzing has run for short, bounded durations (seconds to low hours), not
  the sustained multi-day campaigns a hardened security-critical library
  would eventually want.
- The benchmark (§5) is not automated in CI — it's noisy by nature (CLI
  process spawn overhead, shared-machine variance) and unsuited to a
  pass/fail gate. It's a manually-run, documented artifact, not a
  continuously-enforced one. It also measures CLI-driven latency, not a
  proper concurrent-client load test.

If you find a gap in any of the above, please open an issue — this document
should stay honest about what's actually been checked.
