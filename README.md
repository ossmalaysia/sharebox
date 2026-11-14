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

One executable provides master and worker modes. The master coordinates jobs; each worker runs only the named tasks its owner configured locally. Task commands remain owner-defined on the worker. Source may come from its local checkout or an uploaded immutable snapshot.

## Install a release

Download a [GitHub release](https://github.com/ossmalaysia/sharebox/releases), or use an installer below. Both installers download an official release archive and verify its SHA-256 against that release's `SHA256SUMS` before installing. They do not start a service or modify your shell configuration.

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

## Update

Stop Sharebox before replacing its executable (Ctrl+C for a foreground process, or `sharebox service stop master` / `sharebox service stop worker` for an installed background service). Run the same installer again to download the latest published release and verify its checksum. Keep your existing data directory and worker configuration; reinstalling the binary does not migrate or delete them.

Windows PowerShell:

```powershell
& ([scriptblock]::Create((Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/ossmalaysia/sharebox/main/scripts/install.ps1').Content))
& "$env:LOCALAPPDATA\Sharebox\bin\sharebox.exe" --version
```

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/ossmalaysia/sharebox/main/scripts/install.sh | sh
"$HOME/.local/bin/sharebox" --version
```

Use the same custom installation directory if you originally supplied one. After updating, restart the same master/worker command with the same data/config path, or use `sharebox service start master` / `sharebox service start worker`. Service commands require v0.2.0 or later.

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

After logging in, the dashboard’s **Master LAN address** panel shows private IPv4 addresses detected on the master. Copy an address for another computer, or select the interface that shares its network. Worker pairing commands use that selection even when you opened the dashboard through `localhost`. Use **Refresh addresses** after changing networks. The panel warns when the master listens on loopback only or no private address is available; detection does not prove firewall reachability.

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

## Run automatically in the background

Service support requires v0.2.0 or later. Check `sharebox --version` before using these commands.

Install Sharebox in its permanent location first. Stop the foreground process, then register a background master from the same directory you previously ran it in, so the existing `.sharebox` data and tokens are preserved:

```powershell
& "$env:LOCALAPPDATA\Sharebox\bin\sharebox.exe" service install master --bind 0.0.0.0:7331 --data .sharebox
```

On macOS / Linux:

```sh
"$HOME/.local/bin/sharebox" service install master --bind 0.0.0.0:7331 --data .sharebox
```

The command registers the master and requests its start. Use `sharebox service status master` to inspect the service manager’s state. Open `http://127.0.0.1:7331`; read the administrator token from the selected data directory's `admin-token` file. Keep the firewall limited to your trusted private network. Use `--bind 127.0.0.1:7331` for a master that only accepts connections from its own computer.

For a worker, pair it once using the foreground command above, stop it with Ctrl+C, then install it with the same configuration:

```sh
sharebox service install worker --config worker.json
```

Paths are resolved at installation time, so future startup does not depend on your terminal's working directory. The registered executable must stay at its installed path. Worker task commands still use your local allowlist and account permissions; use absolute executable paths for Node and other tools when possible.

```sh
sharebox service status master
sharebox service stop master
sharebox service start master
sharebox service uninstall master
```

Replace `master` with `worker` to manage that role. Cancel or finish active jobs before stopping, updating, or uninstalling a worker; service-manager termination is not a graceful job cancellation. Uninstall removes the startup registration, preserving master data and worker credentials.

Background output is discarded on Windows and macOS to avoid writing startup credentials or unbounded log files. For startup troubleshooting, stop the service and run the same master/worker command in the foreground. Linux sends standard error to the system journal.

| Platform | Background startup |
| --- | --- |
| Windows | Task Scheduler task for the current user, at login |
| macOS | LaunchAgent for the current user's login session |
| Linux | systemd user service; requires a user service manager |

These run under your account and start when you log in. They are not machine-wide services that run before login. Linux logout behavior depends on the user manager's configuration. Headful browser tests need a desktop session; headless E2E tests are recommended for background workers.

## Menu bar and system tray

Starting with v0.3.0, installing a master or worker service on macOS or Windows also installs a companion status icon. macOS uses the menu bar; Windows uses the system tray (the icon may initially be in the overflow menu). Linux remains a headless user service.

Each installed role has its own icon and menu with service status, Open Dashboard, Start, Stop, Refresh Status, and Quit. Quitting the icon leaves the service running. Stopping a service leaves its icon available to start it again. Status reports the local service process; it does not prove a worker is connected to its master. Check the dashboard for worker connectivity and job results.

For a service installed with v0.2.x, cancel or finish active jobs, uninstall its startup registration, update the binary, then install the service again using the same data/configuration path. This preserves workspace data and worker credentials while adding the icon's login registration:

```sh
sharebox service uninstall worker
# Update the binary with the installer, then:
sharebox service install worker --config /absolute/path/to/worker.json
```

For a master, use `service uninstall master` followed by `service install master --bind 0.0.0.0:7331 --data /absolute/path/to/.sharebox`. Foreground `master` and `worker` commands continue to use the terminal; status icons manage installed services.

## Send source snapshots for tests

Snapshot transfer requires v0.4.0 or later on the uploader, master, and worker. An LLM with local terminal access can run the upload command on the machine containing the project; an MCP connection alone cannot read files from that machine.

Preview the exact selected files before uploading:

```sh
sharebox snapshot inspect --directory /path/to/project
sharebox snapshot upload --directory /path/to/project --server http://MASTER_LAN_IP:7331 --token-env SHAREBOX_MCP_TOKEN
```

`SHAREBOX_MCP_TOKEN` must already contain an MCP token for that master. The CLI reads it from the environment and never accepts a literal token argument. The upload returns a `snapshotId`. The LLM then calls `submit_job` with `workerId`, an owner-enabled task name, and that `snapshotId`. Results identify the snapshot that ran. Reusing the same snapshot ID across workers tests the same initial source bytes even if the uploader's checkout changes afterward.

Ordinary tasks continue to run their local checkout and reject snapshot inputs. To enable a snapshot task, the worker owner edits its local configuration. Use an existing absolute directory dedicated to disposable workspaces for `cwd`; Sharebox creates and cleans a separate job directory beneath it. For **trusted source only**, this native example uses the worker account's permissions:

```json
{
  "description": "Test uploaded trusted Node source",
  "argv": ["node", "--test"],
  "cwd": "/absolute/path/to/sharebox-workspaces",
  "timeoutSeconds": 600,
  "snapshot": { "mode": "native", "prepare": [] }
}
```

For Docker isolation, the owner installs Docker, preloads a suitable image, and changes the snapshot setting. Image, command, preparation steps, and networking are owner-controlled; the LLM cannot supply Docker flags or host paths:

```json
"snapshot": {
  "mode": "docker",
  "image": "node:22-bookworm",
  "network": "none",
  "prepare": []
}
```

Docker images are not pulled automatically. Prefer a pinned image digest for repeatable builds. The container gets a read-only source mount, a 1 GiB temporary work directory, 256 MiB temporary storage, a read-only image filesystem, bounded CPU/memory/PIDs, dropped capabilities, disabled persistent Docker logs, and no inherited host credentials or Docker-socket mount. The image must provide `/bin/sh`, `cp`, and `sleep`; all preparation and test steps share one container. CPU and memory limits also cover the temporary workspace. Docker is required only for Docker-mode tasks. It remains a security-sensitive runtime, not an absolute isolation guarantee.

If a project needs dependencies, the owner can configure a fixed preparation command such as `"prepare": [["npm", "ci", "--ignore-scripts"]]`. Dependencies install inside the job workspace; `node_modules` is never copied from Windows to Mac. Docker networking defaults to `none`; fetching packages needs an explicit owner setting of `"network": "bridge"`. Bridge networking permits normal network access and is not an egress allowlist. Preparation and transfer count toward the job timeout and honor cancellation.

The uploader respects project ignore rules and excludes common credentials, `.git`, `node_modules`, and known caches. Files are staged once, hashed, uploaded in bounded chunks, then verified again by the worker. Symlinks, hard links, special files, unsafe paths, and case collisions are rejected. Version 1 uses ASCII relative filenames, at most 4,096 files, 16 MiB per file, and 128 MiB per snapshot. Exclusions cannot guarantee that source code contains no hardcoded secrets.

Snapshots are private to their uploading MCP principal (the master administrator can manage all snapshots). Workers can download only input assigned to their own active jobs. The master limits snapshot storage to 2 GiB and expires incomplete uploads after 30 minutes. Metadata is also capped at 256 upload/commit receipts, so remove unused snapshots when a quota is reached. Failed container cleanup retains the affected source workspace for investigation; the worker refuses new snapshot work after eight retained entries rather than accumulating them indefinitely. `list_snapshots` lists available input; `delete_snapshot` removes input that no queued or active job needs.

This release transfers immutable snapshots, not a continuously synchronized filesystem. It does not automatically shard tests, synchronize chat sessions, collect arbitrary output artifacts, or provide a dependency cache. Workers still run one job at a time; the LLM can submit separate jobs using the same snapshot to multiple workers.

## Connect an LLM

In the dashboard, select **Connect MCP**, create a token, choose **Codex** or **Claude Code** and your terminal, then click **Copy command**. Paste into PowerShell or a shell on the computer running the agent. The command includes the endpoint and token. If the agent is on a different computer, open the dashboard using the master’s LAN address so that address is used in the command.

The Windows Codex command saves the token in your user environment and current shell, registers Sharebox, and starts Codex. Restart other terminals or desktop clients to pick up the environment change. The macOS/Linux command exports the token for the current terminal session; repeat that export before launching Codex from a new terminal. Keep copied commands private.

For another compatible MCP client, use:

```text
Transport: Streamable HTTP
URL: http://MASTER_LAN_IP:7331/mcp
Authorization: Bearer YOUR_MCP_TOKEN
```

The endpoint uses stateless JSON responses and manually configured bearer tokens. OAuth discovery and browser authorization flows are not implemented. Client-specific configuration varies; use a client that supports a remote Streamable HTTP endpoint and an Authorization header. Do not use a pairing code or a worker credential as the MCP token.

The LLM can discover available workers and permitted tasks, submit jobs, inspect status and logs, and request cancellation through the tools exposed by the server. It does not receive arbitrary shell access to worker machines.

## Execution and network boundaries

- Local-checkout tasks and native snapshot tasks run with the worker account's permissions and are not sandboxes. Use native snapshot mode only for trusted submitted code. Docker snapshot tasks use the explicitly configured container boundary described above.
- The worker owner controls command arguments, working directories, and timeouts. The LLM selects a registered task rather than supplying a command to execute.
- Local-checkout tasks require source and dependencies on the worker. Snapshot tasks transfer verified source into a disposable workspace. Peer-to-peer synchronization, artifact transfer, and automatic test sharding are not implemented.
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
