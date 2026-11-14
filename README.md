# Sharebox

[![CI](https://github.com/ossmalaysia/sharebox/actions/workflows/ci.yml/badge.svg)](https://github.com/ossmalaysia/sharebox/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)

A local Rust control plane and worker for delegating Node.js tasks across your own computers. The master hosts a dashboard and an MCP endpoint. Workers connect to the master, advertise owner-configured tasks, execute accepted jobs, and report their output.

This version runs on your private network. It does not use Cloudflare, email signup, or email OTP. The master creates an administrator token for dashboard access. MCP clients use a separate token created in the dashboard.

## Architecture

```text
LLM / MCP client ── MCP over HTTP ── Master + dashboard
                                         ▲
                                         │ authenticated polling
                                  Mac / Windows workers
                                         │
                                  local Node.js tasks
```

One executable provides master and worker modes. The master coordinates jobs; each worker runs only the named tasks its owner configured locally. Task commands and project files remain on the worker machine.

## Install a release

Installers become usable after the first [GitHub release](https://github.com/ossmalaysia/sharebox/releases) is published. Until then, build from source. Both installers download an official release archive and verify its SHA-256 against that release's `SHA256SUMS` before installing. They do not start a service or modify your shell configuration.

macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/ossmalaysia/sharebox/main/scripts/install.sh | sh
```

The default installation directory is `~/.local/bin`; add it to your `PATH` if needed. For a pinned version or custom directory, download and inspect the installer first:

```sh
curl -fsSL https://raw.githubusercontent.com/ossmalaysia/sharebox/main/scripts/install.sh -o install-sharebox.sh
VERSION=v0.1.0 SHAREBOX_INSTALL_DIR="$HOME/.local/bin" sh install-sharebox.sh
```

Windows PowerShell:

```powershell
& ([scriptblock]::Create((Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/ossmalaysia/sharebox/main/scripts/install.ps1').Content))
```

The default Windows destination is `%LOCALAPPDATA%\Sharebox\bin`. Add it to `PATH`, or run `sharebox.exe` by its full path. To inspect the installer or pin a release:

```powershell
Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/ossmalaysia/sharebox/main/scripts/install.ps1' -OutFile install-sharebox.ps1
./install-sharebox.ps1 -Version v0.1.0
```

Published targets are Linux x86_64 (GNU libc), macOS Apple Silicon and Intel, and Windows x86_64. Other targets require a source build. Release checksums detect download corruption and archive mismatches; they are not independent publisher signatures. macOS binaries are not notarized and Windows binaries are not Authenticode-signed.

## Build

Install the stable Rust toolchain, then run:

```sh
cargo build --release --locked
cargo test --locked
```

The executable is `target/release/sharebox` on macOS/Linux or `target/release/sharebox.exe` on Windows. Node.js and the test tools required by your tasks must also be installed on each worker. Use the installed command `sharebox` instead of the paths below if you installed a release.

For offline development with an existing Rust toolchain and populated dependency cache, the helper discovers that toolchain and uses the checkout's `.cargo-home` cache:

```sh
./scripts/dev.sh build --release --offline
./scripts/dev.sh test --offline
```

The helper does not install or download a Rust toolchain. The copied Cargo cache is local development data and should not be committed.

## Start a master

For a test confined to one computer:

```sh
./target/release/sharebox master --bind 127.0.0.1:7331 --data .sharebox
```

Open `http://127.0.0.1:7331` and use the administrator token printed at startup. The token is also stored in `.sharebox/admin-token`; keep that file private.

For other computers on your private LAN to connect:

```sh
./target/release/sharebox master --bind 0.0.0.0:7331 --data .sharebox
```

Use the master's actual LAN address, for example `http://192.168.1.20:7331`. `0.0.0.0` is a listening address, not an address to give a worker. Permit the listening port through the master's firewall only for the intended private network. Keep the master running while workers are in use.

## Register and start a worker

Copy or build the executable on the target machine. Create a worker configuration:

```sh
./sharebox init --config worker.json
```

The generated configuration includes a local Node.js smoke-test task. Edit its task map to add the project commands that this machine is allowed to execute. A task has this structure:

```json
{
  "description": "Run this checkout's unit tests",
  "argv": ["node", "--test", "test/unit.test.cjs"],
  "cwd": "/absolute/path/to/project",
  "timeoutSeconds": 120
}
```

Use absolute working directories appropriate to the target OS. Arguments are passed directly to the executable rather than interpreted by a shell; shell operators such as `&&` or `|` are not command separators here. Prefer a project script when a task needs multiple setup steps.

In the dashboard, create a one-time worker pairing code. On the target machine, run:

```sh
./sharebox worker --server http://MASTER_LAN_IP:7331 --code PAIR_CODE --config worker.json
```

On Windows PowerShell, use `.\sharebox.exe` in place of `./sharebox`. Pairing saves the worker credential in the configuration and begins polling for work. Restart an already paired worker with:

```sh
./sharebox worker --config worker.json
```

Keep `worker.json` private after pairing: it contains a credential. This version runs one task at a time per worker. Standard output and standard error are captured with a size cap; long output may be truncated. The master can request cancellation, and each task has a configured timeout.

## Connect an LLM

Create an MCP token in the dashboard and configure a compatible client with:

```text
Transport: Streamable HTTP
URL: http://MASTER_LAN_IP:7331/mcp
Authorization: Bearer YOUR_MCP_TOKEN
```

The endpoint uses stateless JSON responses and manually configured bearer tokens. OAuth discovery and browser authorization flows are not implemented. Client-specific configuration varies; use a client that supports a remote Streamable HTTP endpoint and an Authorization header. Do not use a pairing code or a worker credential as the MCP token.

The LLM can discover available workers and permitted tasks, submit jobs, inspect status and logs, and request cancellation through the tools exposed by the server. It does not receive arbitrary shell access to worker machines.

## Execution and network boundaries

- Jobs run natively with the permissions of the worker's OS account. They are not container or VM sandboxes. Use a dedicated account for worker tasks when practical.
- The worker owner controls command arguments, working directories, and timeouts. The LLM selects a registered task rather than supplying a command to execute.
- Project source and dependencies must already exist on the selected worker. Automatic source snapshots, peer-to-peer file synchronization, artifact transfer, and test sharding are not implemented in this version.
- Do not copy `node_modules` between Windows and macOS. Install dependencies locally from the project's lockfile.
- Browser E2E tasks should use their own application ports and test data. Headful tests require an appropriate logged-in desktop session.
- This prototype uses plain HTTP on a trusted private network. Tokens, commands, and logs are not encrypted in transit. Do not expose the service to the public Internet or an untrusted/shared network. For use beyond the trusted LAN, add TLS or an authenticated encrypted private-network transport first.

## Validation scope

The current development environment blocks loopback socket binding, so an end-to-end local network run has not been verified there. Unit/build checks do not establish a working two-computer setup. The CI workflow runs real master/worker/MCP integration checks on Linux, macOS, and Windows; consult the workflow result for the commit or release you use.

To exercise the full flow on a machine that permits local listeners:

```sh
cargo build --locked
python tests/e2e.py
```

Set `SHAREBOX_BIN` to use a different built executable. Even a passing loopback integration test does not establish that your separate Windows PC can connect through its firewall or has the required Node.js dependencies. Verify your intended LAN path with one small task before running larger suites.

## Contributing and releases

See [CONTRIBUTING.md](CONTRIBUTING.md) for checks, review expectations, and release instructions; [AGENTS.md](AGENTS.md) for coding-agent standards; and [SECURITY.md](SECURITY.md) for reporting vulnerabilities. Contributions are licensed under [MIT](LICENSE).

CI checks formatting, Clippy, Rust tests, and the master/worker/MCP flow. A tag matching `Cargo.toml` (for example `v0.1.0`) triggers CI again, builds native release archives, and publishes them with `SHA256SUMS`. Repository branch protection should require `CI passed` and an approving review; documentation alone does not enable those GitHub settings.

## Project layout

- `src/`: Rust master, HTTP handling, MCP tools, and worker implementation.
- `public/`: dashboard assets embedded or served by the master.
- `scripts/dev.sh`: local Cargo/toolchain helper.
- `.sharebox/`: local master state, including private authentication material.
- `.cargo-home/` and `target/`: local build cache and build output.

The live dashboard reflects the capabilities of this local implementation.
