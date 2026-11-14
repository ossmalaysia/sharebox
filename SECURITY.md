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
