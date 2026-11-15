"""Updater rejection checks and disposable native-service restart/rollback tests.

Never modifies an installed binary. Lifecycle checks skip if any real Sharebox
service/tray/update registration exists or no interactive service manager exists.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import tempfile
import urllib.error
import urllib.request

from service import command, existing_registration, tray_running, wait_for


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    system = platform.system()
    if system not in ("Darwin", "Windows"):
        print("SKIP desktop updater: Windows/macOS only")
        return
    if system == "Windows" and existing_registration(system, "update"):
        print("SKIP desktop updater: an updater registration already exists; preserving it")
        return
    suffix = ".exe" if system == "Windows" else ""
    source = Path(os.environ.get("SHAREBOX_BIN", "target/debug/sharebox" + suffix)).resolve()
    assert source.is_file(), "Build Sharebox first"
    with tempfile.TemporaryDirectory(prefix="sharebox updater test ") as directory:
        root = Path(directory).resolve()
        target = root / ("sharebox" + suffix)
        shutil.copy2(source, target)
        original = digest(target)
        stage = root / ".sharebox-update"

        def prepare(candidate=None, **overrides):
            if stage.exists():
                shutil.rmtree(stage)
            stage.mkdir(mode=0o700)
            helper = stage / ("helper" + suffix)
            shutil.copy2(source, helper)
            binary = stage / ("sharebox" + suffix)
            if candidate is None:
                shutil.copy2(source, binary)
            else:
                binary.write_bytes(candidate)
                binary.chmod(0o755)
            plan = dict(target=str(target), tag="v999.0.0", sha256=digest(binary), previous_sha256=digest(target))
            plan.update(overrides)
            path = stage / "plan.json"
            path.write_text(json.dumps(plan))
            return helper, path

        def apply(helper, path):
            return subprocess.run([str(helper), "update", "--apply", str(path)], capture_output=True, text=True, timeout=180)

        # Every rejection occurs before master/worker service inspection or mutation.
        for label, overrides, raw, expected in [
            ("invalid JSON", {}, "{", "Invalid update plan"),
            ("oversized plan", {}, " " * 8193, "Update plan too large"),
            ("changed candidate", {"sha256": "0" * 64}, None, "Staged binary changed"),
            ("changed installed binary", {"previous_sha256": "0" * 64}, None, "Installed binary changed"),
            ("downgrade", {"tag": "v0.0.0"}, None, "newer stable release"),
        ]:
            helper, path = prepare(**overrides)
            if raw is not None:
                path.write_text(raw)
            result = apply(helper, path)
            assert result.returncode != 0, f"Accepted {label}"
            assert expected in result.stderr, f"Wrong rejection for {label}: {result.stderr}"
            assert digest(target) == original, f"Modified target on {label}"
        print("PASS: malformed/oversized plans, changed bytes, and downgrade rejected without replacing binary")

        if system == "Darwin":
            if command(["launchctl", "print", f"gui/{os.getuid()}"]).returncode:
                print("SKIP updater lifecycle: no macOS GUI login domain")
                return
            update_running = command(["launchctl", "print", f"gui/{os.getuid()}/org.ossmalaysia.sharebox.update.{os.getuid()}"]).returncode == 0
        else:
            try:
                if command(["quser.exe", os.environ["USERNAME"]]).returncode:
                    print("SKIP updater lifecycle: no Windows interactive user session")
                    return
            except FileNotFoundError:
                print("SKIP updater lifecycle: cannot inspect interactive session")
                return
            update_running = existing_registration(system, "update")
        if update_running or any(existing_registration(system, role) for role in ["master", "worker", "tray.master", "tray.worker"]):
            print("SKIP updater lifecycle: existing Sharebox registration; preserving it")
            return

        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        origin = f"http://127.0.0.1:{port}"
        data = root / "master data"

        def reachable():
            try:
                with urllib.request.urlopen(origin + "/health", timeout=1) as response:
                    return response.status == 200
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                return False

        def authenticated(token):
            request = urllib.request.Request(origin + "/api/me", headers={"Authorization": "Bearer " + token})
            with urllib.request.urlopen(request, timeout=3) as response:
                assert json.load(response)["name"] == "Local workspace"

        def diagnose():
            status = command([str(target), "service", "status", "master"])
            print("Updater service diagnostics:", status.stdout, status.stderr, flush=True)
            if system == "Windows":
                script = (
                    "$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; "
                    "foreach ($role in @('master','tray-master')) { "
                    "$name='Sharebox-'+$role+'-'+$sid; "
                    "Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue | Select-Object TaskName,State,Actions | Format-List; "
                    "Get-ScheduledTaskInfo -TaskName $name -ErrorAction SilentlyContinue | Select-Object LastRunTime,LastTaskResult | Format-List }; "
                    "Get-CimInstance Win32_Process -Filter \"Name='sharebox.exe'\" | Select-Object ProcessId,ParentProcessId,ExecutablePath | Format-List"
                )
                state = command(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script])
                print(state.stdout, state.stderr, flush=True)
            else:
                state = command(["launchctl", "print", f"gui/{os.getuid()}/org.ossmalaysia.sharebox.tray.master"])
                print(state.stdout, state.stderr, flush=True)
            # A foreground attempt exposes startup errors hidden by the login task.
            # It only opens an icon, uses no credentials, and is always terminated.
            if not tray_running(system):
                probe = subprocess.Popen([str(target), "tray", "--role", "master", "--dashboard", origin], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                try:
                    out, err = probe.communicate(timeout=4)
                    print("Direct tray probe exited:", probe.returncode, out, err, flush=True)
                except subprocess.TimeoutExpired:
                    probe.terminate()
                    out, err = probe.communicate(timeout=10)
                    print("Direct tray probe stayed running until terminated:", out, err, flush=True)

        attempted = False
        try:
            attempted = True
            installed = command([str(target), "service", "install", "master", "--bind", f"127.0.0.1:{port}", "--data", str(data)])
            assert installed.returncode == 0, installed.stdout + installed.stderr
            wait_for(reachable, "disposable master to listen")
            wait_for(lambda: tray_running(system), "disposable tray to start")
            token = (data / "admin-token").read_text().strip()
            for bad_candidate in (False, True):
                helper, path = prepare(candidate=b"not an executable\n" if bad_candidate else None)
                result = apply(helper, path)
                assert (result.returncode != 0) == bad_candidate, result.stdout + result.stderr
                wait_for(reachable, "master restored after update")
                wait_for(lambda: tray_running(system), "tray restored after update")
                assert digest(target) == original, "Wrong executable after update/rollback"
                assert (data / "admin-token").read_text().strip() == token
                assert (data / "state.json").is_file()
                authenticated(token)
                assert existing_registration(system), "Lost startup registration"
                if bad_candidate:
                    assert "rollback" in result.stderr.lower(), result.stderr
            print("PASS: native helper replaces executable, restarts service/tray, preserves credentials, rolls back failed startup")
        except Exception:
            try:
                diagnose()
            except Exception as diagnostic_error:
                print("Updater diagnostics failed:", diagnostic_error, flush=True)
            raise
        finally:
            if attempted:
                cleanup = command([str(target), "service", "uninstall", "master"])
                wait_for(lambda: not reachable(), "disposable master cleanup")
                wait_for(lambda: not tray_running(system), "disposable tray cleanup")
                assert not existing_registration(system), cleanup.stdout + cleanup.stderr
                assert not existing_registration(system, "tray.master"), "Tray registration leaked"


if __name__ == "__main__":
    main()
