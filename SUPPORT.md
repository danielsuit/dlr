# Support

Start with the [README](README.md), [sidecar guide](docs/SIDECAR.md), and
[protocol design](DESIGN.md). Use GitHub Issues for reproducible bugs and
focused feature proposals.

Include the following in a bug report:

- commit SHA, Rust version, operating system, and architecture;
- deployment topology and sanitized sidecar configuration;
- the smallest reproduction and expected/actual behavior;
- sanitized logs and HTTP status/headers;
- whether the issue occurs on a fresh WAL and on loopback.

Never post provider keys, ingress tokens, WAL files, conversation payloads, or
private gateway logs. Report potential vulnerabilities privately according to
[SECURITY.md](SECURITY.md).
