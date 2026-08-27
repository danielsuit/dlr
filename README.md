# DLR

[![CI](https://github.com/danielsuit/dlr/actions/workflows/ci.yml/badge.svg)](https://github.com/danielsuit/dlr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](https://www.rust-lang.org/)

DLR is an application-layer append-log replication protocol for moving a
growing coding-agent conversation to an OpenAI-compatible gateway without
uploading the complete history on every turn.

The deployable implementation is an HTTP sidecar. It accepts compact DLR
frames, stores conversation state durably, reconstructs an ordinary
`/v1/chat/completions` request beside the gateway, and streams the upstream
response back without buffering. Existing gateways and SGLang deployments do
not need a DLR-specific fork.

> Project status: active development. The HTTP sidecar, append/resync/bulk
> lifecycle, durable WAL, client adapter, container image, and protocol tests
> are implemented. The QUIC, multipath, and RDMA modules are research-oriented
> interfaces or models, not production transports.

## Why DLR

In a long agent session, the transcript grows while each new turn is usually
small. A normal JSON request repeatedly transfers that entire transcript. DLR
transfers the initial history once, then sends only appended message blocks in
steady state.

DLR reduces repeated client-to-edge upload work. It does not make model token
generation faster, and the current sidecar still sends reconstructed JSON over
the short local hop to the existing gateway. DLR complements model-side prefix
or KV caching such as SGLang RadixAttention; it does not replace it.

## Quick start

### Requirements

- Git
- Rust 1.89 or newer, installed with [rustup](https://rustup.rs/)
- An OpenAI-compatible upstream with `/v1/chat/completions`
- Docker only if you want to build the container image

Clone and verify the workspace:

```sh
git clone https://github.com/danielsuit/dlr.git
cd dlr
cargo test --locked --workspace --all-targets
```

Start a development sidecar on loopback:

```sh
export DLR_UPSTREAM_URL=http://127.0.0.1:8080
export DLR_WAL="$PWD/dlr-receiver.wal"
cargo run --release -p dlr-sidecar --bin dlr-sidecar
```

Check readiness and protocol discovery:

```sh
curl --fail http://127.0.0.1:32180/healthz
curl --fail http://127.0.0.1:32180/readyz
curl --fail http://127.0.0.1:32180/v1/dlr/capabilities
```

An ordinary OpenAI client cannot send DLR frames by itself. Use the Rust
`dlr_sidecar::DlrChatClient` and `ChatSession` adapter, or another client that
implements the lifecycle in [the sidecar guide](docs/SIDECAR.md). The terminal
client in
[`subconscious-code`](https://github.com/subconscious-systems/subconscious-code)
includes DLR-first transport with safe JSON fallback.

## How a request moves

```text
DLR-aware client
    │
    │ append frame: new messages + request metadata
    ▼
DLR sidecar + durable WAL
    │
    │ reconstructed OpenAI-compatible JSON
    ▼
existing gateway ──► SGLang or another model runtime
    │
    └──────────── unbuffered response/SSE ───────────► client
```

The sidecar acknowledges receiver state only after its WAL is flushed. A client
retains prepared bytes until it accepts the ACK root. Root conflicts use the
`RESYNC` → `MISSING` → `BULK` repair flow; an accepted model invocation is never
silently replayed through a different transport.

### HTTP surface

| Route | Purpose |
| --- | --- |
| `GET /healthz` | Process liveness |
| `GET /readyz` | WAL and receiver readiness |
| `GET /v1/dlr/capabilities` | Version and feature discovery |
| `POST /v1/dlr/chat/completions` | DLR-aware chat request and streamed upstream response |
| `POST /v1/dlr/frame` | Low-level protocol frame exchange |

The binary envelope is capped at 64 MiB, with request metadata capped at
2 MiB. Upstream gateway body limits still apply to reconstructed requests.

## Container deployment

```sh
docker build -t dlr-sidecar .
docker run --rm -p 32180:32180 \
  -e DLR_UPSTREAM_URL=http://gateway.internal:8080 \
  -e DLR_INGRESS_TOKEN='replace-with-a-secret' \
  -v dlr-state:/var/lib/dlr \
  dlr-sidecar
```

The image runs as a non-root user and persists its WAL under `/var/lib/dlr`.
A non-loopback bind is rejected unless `DLR_INGRESS_TOKEN` is configured. Put
TLS and authentication at a trusted ingress or service mesh and do not expose
the plain HTTP listener directly to an untrusted network.

## Workspace layout

| Crate | Responsibility |
| --- | --- |
| `dlr-core` | Blocks, IDs, Merkle/MMR state, frame codec, content store, and WAL |
| `dlr-coding` | GF(256), fountain/RLNC/RS coding, homomorphic hashing, and placement models |
| `dlr-compress` | zstd and reference-delta compression |
| `dlr-transport` | Transport traits, pipeline, flow model, and research adapters |
| `dlr-shim` | Client-side append framing, deduplication, and synchronization state |
| `dlr-receiver` | Session reconstruction, protocol handling, and durable receiver state |
| `dlr-prune` | Off-hot-path and incremental pruning primitives |
| `dlr-sidecar` | HTTP ingress, request reconstruction, authentication, and SSE proxying |
| `dlr-bin` | End-to-end protocol demonstration |

## Documentation

| Document | Contents |
| --- | --- |
| [Sidecar guide](docs/SIDECAR.md) | Deployment, client lifecycle, wire envelope, durability, and failure handling |
| [Protocol design](DESIGN.md) | Protocol model, invariants, and design rationale |
| [Benchmarks](BENCHMARKS.md) | Analytical performance model and assumptions |
| [Optimizations](OPTIMIZATIONS.md) | Optimization inventory and implemented fast paths |
| [Contributing](CONTRIBUTING.md) | Development setup, required checks, and review expectations |
| [Security policy](SECURITY.md) | Private reporting and production hardening guidance |

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets
cargo doc --locked --workspace --no-deps
cargo build --locked --release --workspace
docker build --tag dlr-sidecar:local .
```

Performance results must name the commit, topology, corpus, repetitions, cache
state, and percentile. See [BENCHMARKS.md](BENCHMARKS.md) before interpreting a
wire-size or TTFT claim.

## Security and support

Never commit provider keys or DLR ingress tokens. Treat WAL files and traces as
sensitive because they can contain conversation content. Report vulnerabilities
privately according to [SECURITY.md](SECURITY.md). For usage questions and bug
reports, see [SUPPORT.md](SUPPORT.md).

DLR is available under the [MIT License](LICENSE).
