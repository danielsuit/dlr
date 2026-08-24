# dlr

An append-log replication + coded-transfer protocol for moving a growing (→50M-token) Claude Code message list to a remote runtime that prunes to ~100k — without re-sending history, without exposing the pruning algorithm, and without bottlenecking on the expensive prune.

This repo is the **implementation** of [`DESIGN.md`](./DESIGN.md). See [`BENCHMARKS.md`](./BENCHMARKS.md) for the theoretical performance model vs gRPC.

## Status

Fully implemented across a Rust workspace; builds clean in release (`cargo build --release`), all unit/property/integration tests pass (`cargo test --release` — including the protocol-lifecycle suite in `dlr-receiver` that drives a real `Shim` against a real `Receiver` over the frame codec, covering steady state, dedup, the full cold-start handshake, out-of-order BULK delivery, post-cold-start resumption, and cold-start WAL durability so a cold-started session survives a receiver restart without a re-transfer), and the demo binary runs the full steady-state + cold-start path end-to-end (`cargo run --release`).

## Workspace layout

| Crate | Role |
|---|---|
| `core` | Semantic content blocks, BLAKE3 `block_id`, append-only Merkle DAG (+ Merkle mountain ranges), content-addressed store, `APPEND`/`RESYNC`/`BULK`/`ACK` frame codec, session bookkeeping, fast-path dedup filters |
| `coding` | The mathematical layer: GF(256) field arithmetic, Fibonacci multiplicative hashing, golden-ratio ring placement, Zeckendorf/Fibonacci wire varints, RaptorQ-style rateless fountain, Random Linear Network Coding, homomorphic hashing, Cayley/circulant overlays, Reed-Solomon, hierarchical (two-layer) coding, Fibonacci backoff |
| `compress` | zstd-with-dictionary compression + **reference-delta** compression (prior block as reference frame) for near-identical file snapshots |
| `transport` | Transport substrates reused, not reinvented: loopback/UDS, QUIC, Multipath QUIC, RoCEv2/RDMA, multicast overlay, a BBR-style bandwidth model, and a staged compress→code→send **pipeline** with backpressure |
| `shim` | The local loopback shim: framing + dedup + coding only, **no prune**, holds no policy |
| `receiver` | Accumulates the per-session log in a content-addressed store, hands the runtime a stable pointer, forks the full log async to a distillation sink |
| `prune` | The secret, heavy, async prune: off the transfer hot path, plus an **incremental** top-K pruner (O(log K)/block, independent of log length) |
| `bin` | A wired demo binary exercising the whole steady-state + cold-start path and every strategy |

## The win (why it beats gRPC)

gRPC over HTTP/2 re-sends the full 50M-token array every turn. dlr ships only the **append delta** (the new turn, KB) in steady state and pays the 200MB bulk transfer **once**, coded and resumable, on cold start. Per-turn wire cost drops ~500×; multicast fan-out is ~1× not N×; the expensive prune runs off the wire path. Details and the order-of-magnitude model are in [`BENCHMARKS.md`](./BENCHMARKS.md).

## Design principle

Do **not** reinvent L4. Reliable delivery, congestion control, loss recovery, and encryption are reused (QUIC/RDMA). Innovation lives at the application layer (log replication) and the coding layer (erasure/network codes), carried over a reused transport substrate.
