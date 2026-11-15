# Security policy

## Supported versions

Security fixes target the latest published release. This is an early local-network tool, not a hardened multi-tenant execution service.

## Report privately

Use [GitHub private vulnerability reporting](https://github.com/ossmalaysia/sharebox/security/advisories/new). Include the affected version, platform, reproduction steps, and impact. Do not include live tokens or private project data. If private reporting is unavailable, open a public issue asking for a private reporting channel without disclosing exploit details.

## Trust boundary

The current transport is plain HTTP. Authentication tokens and job output are visible to anyone able to observe that traffic. Use only a trusted private LAN or loopback; do not publish the listening port to the Internet. TLS and Internet-facing hardening are not implemented.

Workers run local owner-defined commands with their OS account's permissions. A named task may execute project code, dependency lifecycle hooks, or tests. It is not a sandbox and should not process untrusted repositories under a privileged account. Separate work directories alone do not provide isolation.

Protect the master data directory, administrator token, MCP tokens, pairing codes, and worker configuration. Revoke credentials when a machine is lost or no longer authorized. Test logs may contain secrets produced by the task itself; review output before sharing it.

Release archives include SHA-256 checksums, which detect corruption or mismatches against the downloaded release manifest. They do not protect against a compromised release account that replaces both archive and manifest. Installers download from the official `ossmalaysia/sharebox` GitHub release and verify checksums before installation.

Review workflow changes carefully. Do not run untrusted pull-request code with deployment or release credentials, and do not use `pull_request_target` to execute contributor code.

## Uploaded source snapshots

Snapshot execution is an explicit worker-owner opt-in. An uploaded test or package script can execute code even though the task's argv is fixed. Native snapshot mode trusts the submitting principal with the worker account's permissions. Existing local-checkout tasks reject snapshot IDs. The remote client cannot choose execution mode, container image, preparation commands, networking, or a host working directory.

Docker mode mounts input read-only and copies it into a size-limited temporary workspace; it does not give the container a writable host checkout, host credentials, or Docker socket. The Docker daemon and owner-selected image remain trusted dependencies. Network access defaults off; enabling bridge networking is not an egress allowlist. Images should be pinned and scanned separately.

Uploads are bounded and principal-scoped, and workers download only input assigned to an active job. File paths, content hashes, and credential-shaped configuration are validated. Default exclusions reduce accidental disclosure but do not guarantee that source files contain no hardcoded secrets. Snapshot transfer still uses HTTP on the trusted LAN. See [snapshot design](docs/source-snapshots.md) and the snapshot security review for implementation and validation details.

## Desktop release updater

The Windows/macOS tray checks the official `ossmalaysia/sharebox` GitHub repository for a newer stable release. Checks are outbound HTTPS requests; they do not send Sharebox tokens, source files, worker configuration, or job logs. GitHub and its download infrastructure still receive ordinary connection metadata such as the client's IP address.

Installing an update requires an explicit local confirmation. The update is downloaded and checked before services stop. Replacing the shared executable may interrupt running tasks on both roles, so finish or cancel those tasks first. This feature does not authorize a remote MCP client to replace binaries, alter task permissions, or choose an update URL.

The updater verifies the native archive against the same release's SHA-256 manifest before extracting the executable. HTTPS and checksums protect transport integrity and archive selection; they are not independent release signatures and do not protect against a compromised publisher account or release pipeline. The native binaries remain unsigned/unnotarized. Existing same-account code can already modify user-owned binaries and service configuration; the updater is not a privilege boundary against that account.

The helper runs without elevation and preserves existing service registrations and data/configuration paths. Configuration and credentials are not part of the replacement payload. Keep backups of important master state independently of executable updates.

## Local worker setup

`sharebox setup` listens only on IPv4 loopback and uses a fresh bearer token for that setup session. Its page URL carries the session token in a fragment, so it is not part of the HTTP request path. Treat the local URL as private while the setup process is running. Setup API requests must use the expected Host; state-changing requests additionally require the exact local Origin. Request bodies are bounded. The API does not use ambient browser cookies and is not exposed by the LAN master or MCP endpoint.

A registration link contains a short-lived, one-time pairing code, not an administrator or MCP token. It selects only the private-network master. The worker owner still chooses uploaded-code permission, a fixed task preset, workspace, container image, and network mode on the worker machine. Setup preserves other existing owner tasks; selecting Docker for its managed preset does not sandbox unrelated native tasks. Docker detection does not grant execution permission, and an unavailable Docker daemon never triggers native fallback.

Image downloads and background service changes are separate explicit local actions. Docker daemon access grants significant capabilities to the local account; the setup UI is not a security boundary against other code already running as that same account. Pairing credentials remain in the private worker configuration and are not returned by the setup status API.
