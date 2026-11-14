# Source snapshot security review

Reviewed 2026-11-15 against the v0.4.0 working tree before release. Scope: source collection, CLI upload, snapshot persistence and authorization, job binding, native execution, and Docker execution. Tests used temporary fixtures; no existing user worker configuration was activated or changed.

## Results

| Check | Result |
| --- | --- |
| cargo-audit 0.23.0 | 66 locked dependencies; zero known vulnerabilities and zero advisory warnings |
| Gitleaks 8.30.1, default rules, redacted output | 45 current source/documentation/test files: no detected secrets |
| Collector tests | Eight passed: exclusions, known credential detection, bounded ignore files, staging integrity, permissions, links/special files, size limits, cleanup |
| CLI tests and real loopback upload | Passed, including empty/multi-chunk files, stable content identity, and no bearer token output |
| Store tests | Eight passed, including ownership, expiry, quota reservation, interrupted writes, deduplication, restart, and receipt limits |
| Worker policy/deadline regressions | Passed: Docker command boundaries, download timeout, and delayed prelaunch heartbeat |
| Native snapshot end-to-end test | Passed locally on macOS arm64 |
| Docker snapshot end-to-end test | Passed locally with Docker Desktop and `node:22-bookworm-slim` |

The end-to-end test covers multi-chunk upload, verified input, preparation and final task in the same disposable workspace, cancellation during preparation, and cleanup. The Docker image was pre-pulled with digest `sha256:e9f565d32859f0cbfb52125eafe8be39390f24dac302dc3cdf566e8ba3ab21ee`. Container image packages were **not** included in the Rust dependency vulnerability scan. Native Windows runtime correctness requires its own CI/runtime evidence.

RustSec database: 1,509 advisories, commit `4ef2df8e806a65e2f1ecc14f56f51249062b23d6`, updated `2026-11-13T13:33:03+01:00`. Audited `Cargo.lock` SHA-256: `2666c58182d60f738a2826ecd4dc501f53a00ad253b91e3aa1d243b6d37a26e8`.

Secret scanning included tracked source plus new Rust, documentation, and test files. It excluded ignored credentials, local worker configurations, `.sharebox/`, dependency/build caches, and local test artifacts. Those private files were not uploaded or printed. Results describe the rules and advisory database used; they do not establish the absence of unknown vulnerabilities or every possible secret.

## Review findings addressed

- **Bound ignore-file reads.** Automatic traversal could read ignore files before source-size validation. Collection now loads ignore rules through bounded, no-follow readers: 64 KiB per file and 1 MiB combined, with bounded pattern counts and directory entries.
- **Detect known credentials despite renaming.** Mandatory filename exclusions cover Sharebox credentials and common secret/cache paths. Both collection and server commit reject the recognizable JSON shape of a paired Sharebox worker, even under another filename. Errors do not print its token. This is a targeted protection, not general content secret scanning.
- **Prevent launches after a delayed heartbeat exhausts the deadline.** Worker limits are rechecked after network operations and immediately before spawning a command. A regression verifies that a delayed prelaunch heartbeat cannot start the task after expiry. Network operations still have finite transport timeouts, so interruption of an already-running process is not guaranteed to occur at an exact millisecond.
- **Bound Docker writes.** The initial writable host workspace mount was replaced by a read-only verified source mount and a 1 GiB container tmpfs workspace. Temporary storage is another bounded tmpfs; container logging is disabled at the Docker log driver, while Sharebox captures bounded output. The fresh container has a non-root user, dropped capabilities, a read-only root, CPU/memory/PID limits, and networking disabled unless the owner explicitly chooses bridge networking.

## Authorization, persistence, and quota conclusions

An MCP token can manage its own snapshot grants. The master administrator can manage all grants. Worker tokens cannot upload, list, or delete snapshots; download endpoints require the matching job/worker assignment and reject cancelled, terminal, expired-lease, or revoked access. Snapshot hashes identify content and do not confer access.

Jobs bind an immutable snapshot ID and its grant owner. Idempotency checks include worker, task, snapshot, and requesting principal. Existing local-checkout tasks reject snapshots; snapshot execution requires an explicit owner-configured task mode. Remote requests do not supply executable arguments or destination paths.

Reservations are persisted before staging data is created. Chunk acknowledgments follow file synchronization; identical retries succeed, conflicting retries fail, and interrupted writes can resume only after matching their existing prefix. Commit verifies the complete manifest, file sizes, hashes, and known credential signature before publishing the committed index. Expired/deleted data retains its reservation until cleanup succeeds. Missing or malformed persisted metadata fails closed.

Application storage accounting is capped at 2 GiB, with two active uploads per principal and a 30-minute incomplete-upload lifetime. This is application accounting, not a filesystem-wide physical disk quota. Source limits are 4,096 files, 16 MiB per file, 128 MiB total, a 512 KiB manifest, and 256 KiB chunks.

Duplicate-upload receipts are included in the global 256-record admission limit; they are not unbounded. A commit can temporarily add a separately accounted garbage record. A near-cap restart regression verifies that retries remain valid, only one physical snapshot copy survives cleanup, quota is preserved, further admissions stop, and deletion releases the receipts. Receipts remain until their snapshot is deleted, so repeated identical uploads can eventually require deleting an unused snapshot.

No additional authorization bypass, arbitrary-path materialization, or quota-reset vulnerability was confirmed in these reviewed paths.

## Remaining trust boundaries

Uploaded tests and package scripts are executable code. Fixed task arguments do not make native source execution safe for untrusted input: native mode trusts the submitting agent and runs with the worker account's filesystem, environment, and network access. Docker reduces host access, but still depends on the trusted owner-selected image, Docker daemon, kernel, and configuration. Bridge networking is not an egress allowlist.

Transport remains unencrypted HTTP for a trusted private LAN. Excluded filenames and known credential detection cannot find secrets embedded elsewhere in source. This review is a targeted pre-release code review and regression/advisory/secret scan, not an independent penetration test or security certification.
