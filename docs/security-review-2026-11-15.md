# Security review — 2026-11-15

Scope: the Sharebox v0.3.0 working tree, including LAN address discovery, the native tray companion, service registration, HTTP/MCP authorization, worker command selection, and release installers. Checks ran locally on macOS arm64. This review did not scan other machines on the user's network.

## Automated checks

| Check | Result |
| --- | --- |
| `cargo-audit 0.23.0 audit --json` | 46 locked dependencies; zero known vulnerabilities and zero advisory warnings |
| `gitleaks 8.30.1 git --redact` | 22 commits scanned; no detected secrets |
| `gitleaks 8.30.1 dir --redact` | Current source/documentation snapshot, including uncommitted LAN/tray files; no detected secrets |
| HTTP regression tests | All four passed, including a continuous-byte deadline test using real loopback sockets |

The RustSec database contained 1,509 advisories at commit `4ef2df8e806a65e2f1ecc14f56f51249062b23d6`, updated `2026-11-13T13:33:03+01:00`. The audited `Cargo.lock` SHA-256 was `77ee3297671c98a69325605f95d9fde51fec539ea27845bb4f83378d7e0dfa92`; the scan included the new native tray dependencies. Advisory results describe that database snapshot, not a guarantee against unknown vulnerabilities.

Gitleaks used its default rules with redacted output. The working-tree scan used an isolated source snapshot: tracked repository files plus new Rust sources and this report. It excluded ignored credentials, `.sharebox/`, worker configuration, build/dependency caches, and local test artifacts. Those local files were not uploaded or included in scan output. Tools were installed under a temporary directory, without adding application dependencies.

## Confirmed finding and fix

**Medium — slow clients could retain HTTP connections beyond the intended timeout.** `src/http.rs` set a ten-second socket read timeout, but each successful read restarted it. A client could send bytes periodically and retain one of the master's 64 connection slots indefinitely. The same pattern affected the worker's response reader.

A local reproduction sent an incomplete header in small increments. Before the fix, completing it after 12.0 seconds still produced HTTP 200. The reader now enforces a ten-second absolute budget across the complete message, updating the socket timeout to the remaining budget before every read. Existing header/body size limits remain in place. The same reproduction after rebuilding was rejected with HTTP 400 at approximately 10.2 seconds. A focused regression test continuously supplies bytes and verifies that the deadline still terminates the read; its sender is bounded to avoid hanging the test suite.

## Code review observations

- `/api/network` remains admin-only, returns bounded address candidates, and does not expose credentials. LAN detection does not establish firewall permissions or peer reachability.
- Dashboard-generated addresses and commands are escaped for their output context. Pairing does not advertise localhost as a remote worker address.
- Native tray browser actions accept only HTTP origins using localhost or private/loopback IPv4 addresses, with an explicit nonzero port. They pass URLs to native browser-opening APIs without a shell command string or authentication token.
- Service control selects fixed roles/actions. Generated PowerShell, launchd, and systemd configuration escapes local paths and excludes worker tokens and pairing codes. Services run as the user, without requesting elevated privileges.
- Remote job requests select owner-defined task names. Command arguments and working directories still come from the worker's local configuration.
- Release installers verify the archive against its matching SHA-256 manifest before copying the executable. This provides integrity relative to the GitHub release; it is not independent release signing.

## Limits

Sharebox still uses unencrypted HTTP and native, unsandboxed owner-defined tasks on a trusted private LAN. Tokens can be observed by someone who can inspect that network traffic. Task output may itself contain secrets. These are documented deployment boundaries, not capabilities added by this review.

No new authorization bypass or command-injection issue was confirmed in the reviewed paths. This was a targeted code review, advisory scan, secret scan, and local regression test—not a penetration test, comprehensive fuzzing campaign, or proof of security. Native Windows behavior requires its own CI/runtime validation; macOS checks alone do not establish Windows correctness.
