# Security policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's private
vulnerability reporting for this repository:

1. Open the repository's **Security** tab.
2. Select **Report a vulnerability**.
3. Include the affected commit, impact, reproduction, deployment assumptions,
   and any suggested mitigation.

If private reporting is unavailable, contact the maintainers privately through
the Subconscious Systems organization profile. Allow a reasonable remediation
window before public disclosure. Maintainers will coordinate status and
disclosure through the private report.

## Supported versions

Before a stable 1.0 release, security fixes target the latest `main` branch and
the newest published release. Older development snapshots are not maintained.

## Deployment guidance

- Bind development sidecars to loopback.
- For a remote sidecar, require TLS at a trusted ingress, a strong
  `DLR_INGRESS_TOKEN`, upstream allowlisting, network policy, and durable
  storage with one active WAL owner.
- Never place provider credentials or ingress tokens in images, settings,
  command history, traces, or repository files.
- Treat WAL files as sensitive conversation data. Encrypt, back up, rotate,
  retain, and delete them according to the surrounding application's policy.
- Restrict the configured upstream; the sidecar forwards authorization and
  selected tracing/session headers to it.
- Bound request sizes at the ingress as well as in the sidecar.

See [the sidecar guide](docs/SIDECAR.md) for protocol-specific durability and
failure behavior.
