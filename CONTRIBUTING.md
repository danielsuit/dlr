# Contributing

Bug reports, protocol tests, documentation fixes, benchmark improvements, and
focused implementations are welcome. By participating, you agree to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

Use [Support](SUPPORT.md) for usage questions and report vulnerabilities
privately according to [SECURITY.md](SECURITY.md).

## Development setup

Install Git and Rust 1.89 or newer, then clone and test the workspace:

```sh
git clone https://github.com/subconscious-systems/dlr.git
cd dlr
cargo test --locked --workspace --all-targets
```

Docker is optional and is only required to validate the production image.

## Required checks

Run the checks that apply to your change before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets
cargo doc --locked --workspace --no-deps
cargo build --locked --release --workspace
docker build --tag dlr-sidecar:local .
```

CI also verifies that the complete workspace compiles on Rust 1.89.

## Change expectations

- Keep wire-format changes explicit and versioned. Add encode/decode,
  malformed-input, restart, lost-ACK, and compatibility tests.
- Preserve the durability rule: do not expose an ACK as durable before the WAL
  operation it represents is durably flushed.
- Preserve idempotency and never introduce an automatic retry that can invoke
  the upstream model twice after an ambiguous response.
- Keep production transport claims separate from research interfaces. QUIC,
  multipath, and RDMA modules are not deployable until their data paths exist.
- Include topology, corpus, repetitions, percentiles, and commit SHA with
  performance claims.
- Update the README or a focused guide when changing configuration, endpoints,
  headers, limits, deployment, or failure behavior.
- Never commit provider credentials, DLR ingress tokens, WAL contents, private
  conversations, or proprietary traces.

## Pull requests

Explain the problem, solution, compatibility impact, and checks run. Call out
changes to the frame format, persisted WAL, synchronization lifecycle,
authentication, upstream forwarding, or resource limits. Small, reviewable
changes are preferred; discuss major protocol changes in an issue first.

Contributions are accepted under the repository's MIT License.
