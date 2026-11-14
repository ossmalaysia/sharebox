"""Exercise a real per-user service manager, without replacing existing services.

Run after cargo build. Unsupported or noninteractive runners print an explicit
SKIP. Set SHAREBOX_REQUIRE_SERVICE_TEST=1 to make such skips fail (dedicated hosts).
This temporarily registers the master service and removes it in a finally block.
"""
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def command(args):
    return subprocess.run(args, capture_output=True, text=True, timeout=40)


def skip(reason):
    if os.environ.get("SHAREBOX_REQUIRE_SERVICE_TEST") == "1":
        raise AssertionError(reason)
    print("SKIP service lifecycle: " + reason)


def wait_for(predicate, description, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.2)
    raise AssertionError("Timed out waiting for " + description)


def main():
    system = platform.system()
    if system not in ("Darwin", "Windows", "Linux"):
        return skip("no supported native user service manager")
    if system == "Darwin":
        if command(["launchctl", "print", f"gui/{os.getuid()}"]).returncode:
            return skip("macOS runner has no GUI login domain")
    elif system == "Linux":
        try:
            manager = command(["systemctl", "--user", "show-environment"])
        except FileNotFoundError:
            return skip("systemctl is unavailable")
        if manager.returncode:
            return skip("Linux runner has no systemd user manager")
    else:
        # Interactive tasks cannot run without the owner's logged-in session.
        try:
            session = command(["quser.exe", os.environ["USERNAME"]])
        except FileNotFoundError:
            return skip("cannot verify a Windows interactive user session")
        if session.returncode:
            return skip("Windows runner has no interactive session for its account")

    binary = Path(os.environ.get("SHAREBOX_BIN", "target/debug/sharebox" + (".exe" if os.name == "nt" else ""))).resolve()
    if not binary.is_file():
        raise AssertionError("Build the Sharebox executable before running this test")

    def service(*args, check=True):
        result = command([str(binary), "service", *args])
        if check and result.returncode:
            raise AssertionError(f"service {args[0]} failed: {result.stdout} {result.stderr}")
        return result

    # The native registration check below is deliberately separate from the CLI
    # under test: never uninstall an existing user's service during cleanup.
    if existing_registration(system):
        return skip("a Sharebox master service already exists; preserving it")

    with tempfile.TemporaryDirectory(prefix="sharebox service test ") as directory:
        data = Path(directory) / "master data"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        origin = f"http://127.0.0.1:{port}"

        def reachable():
            try:
                with urllib.request.urlopen(origin + "/health", timeout=1) as response:
                    return response.status == 200
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                return False

        def authenticated(token):
            request = urllib.request.Request(origin + "/api/me", headers={"Authorization": f"Bearer {token}"})
            with urllib.request.urlopen(request, timeout=3) as response:
                assert json.load(response)["name"] == "Local workspace"

        attempted = False
        try:
            attempted = True
            service("install", "master", "--bind", f"127.0.0.1:{port}", "--data", str(data))
            wait_for(reachable, "background master to listen")
            token = (data / "admin-token").read_text().strip()
            assert token, "master must persist its token"
            authenticated(token)
            assert existing_registration(system), "install must persist startup registration"
            service("status", "master")
            service("stop", "master")
            wait_for(lambda: not reachable(), "background master to stop")
            assert existing_registration(system), "stop must preserve startup registration"
            assert (data / "admin-token").read_text().strip() == token
            service("start", "master")
            wait_for(reachable, "background master to restart")
            authenticated(token)
            service("uninstall", "master")
            wait_for(lambda: not reachable(), "uninstalled master to stop")
            assert not existing_registration(system), "uninstall must remove startup registration"
            assert (data / "admin-token").read_text().strip() == token
            assert (data / "state.json").is_file(), "uninstall must preserve workspace state"
            attempted = False
        except Exception:
            # Preserve startup diagnostics before removing the temporary unit.
            diagnostic = service("status", "master", check=False)
            print(diagnostic.stdout, diagnostic.stderr, flush=True)
            if system == "Linux":
                journal = command(["journalctl", "--user", "-u", "sharebox-master.service", "--no-pager", "-n", "30"])
                print(journal.stdout, journal.stderr, flush=True)
            raise
        finally:
            if attempted:
                # Installation may have failed after registering with the OS.
                result = service("uninstall", "master", check=False)
                if existing_registration(system) or reachable():
                    raise AssertionError("Service cleanup failed; inspect sharebox service status master: " + result.stderr)
    print("PASS: native master service install, status, stop, start, uninstall; paths with spaces and persisted credentials")


def existing_registration(system):
    if system == "Darwin":
        definition = Path.home() / "Library/LaunchAgents/org.ossmalaysia.sharebox.master.plist"
        loaded = command(["launchctl", "print", f"gui/{os.getuid()}/org.ossmalaysia.sharebox.master"])
        return definition.exists() or loaded.returncode == 0
    if system == "Linux":
        config = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
        if not config.is_absolute():
            config = Path.home() / ".config"
        definition = config / "systemd/user/sharebox-master.service"
        loaded = command(["systemctl", "--user", "show", "sharebox-master.service", "--property=LoadState", "--value"])
        return definition.exists() or loaded.stdout.strip() not in ("", "not-found")
    script = (
        "$ErrorActionPreference = 'Stop'; "
        "$taskName = 'Sharebox-master-' + [Security.Principal.WindowsIdentity]::GetCurrent().User.Value; "
        "$exists = @(Get-ScheduledTask | Where-Object { $_.TaskName -eq $taskName }).Count -gt 0; "
        "Write-Output $exists"
    )
    result = command(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script])
    if result.returncode or result.stdout.strip() not in ("True", "False"):
        raise AssertionError("Cannot safely inspect existing scheduled tasks: " + result.stderr)
    return result.stdout.strip() == "True"


if __name__ == "__main__":
    main()
