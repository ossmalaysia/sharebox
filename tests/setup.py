"""Exercise loopback worker setup against a disposable master/configuration.

No real services, worker configurations, or user jobs are changed. Docker absence
is deterministic; an optional second pass can validate a locally preloaded image.
"""
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def wait_for(check, description, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(.1)
    raise AssertionError("Timed out waiting for " + description)


def request(origin, path, data=None, token=None, expected=(200,), headers=None):
    body = data if isinstance(data, bytes) else None if data is None else json.dumps(data).encode()
    hdr = {"Content-Type": "application/json", **(headers or {})}
    if token:
        hdr["Authorization"] = "Bearer " + token
    req = urllib.request.Request(origin + path, data=body, headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=20) as response:
            status, content = response.status, response.read(1024 * 1024)
    except urllib.error.HTTPError as error:
        status, content = error.code, error.read(1024 * 1024)
    assert status in expected, f"{path}: HTTP {status}, expected {expected}: {content[:300]!r}"
    return json.loads(content) if content and content[:1] in (b"{", b"[") else content


def main():
    binary = Path(os.environ.get("SHAREBOX_BIN", "target/debug/sharebox" + (".exe" if os.name == "nt" else ""))).resolve()
    with tempfile.TemporaryDirectory(prefix="sharebox setup test ") as directory:
        root = Path(directory).resolve()
        workspace = root / "workspaces"
        workspace.mkdir()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        master_origin = f"http://127.0.0.1:{port}"
        config_path = root / "worker.json"
        custom = {"argv": ["node", "--version"], "cwd": str(workspace), "timeoutSeconds": 30, "description": "Existing owner task"}
        original = {"name": "Existing owner worker", "server": "", "tasks": {"custom-local": custom}}
        config_path.write_text(json.dumps(original))
        if os.name != "nt":
            config_path.chmod(0o600)
        processes = []
        logs = []

        def launch(args, name, env=None):
            log = open(root / (name + ".log"), "w+")
            logs.append(log)
            process = subprocess.Popen([str(binary), *args], stdout=log, stderr=log, env=env)
            processes.append(process)
            return process, root / (name + ".log")

        try:
            launch(["master", "--bind", f"127.0.0.1:{port}", "--data", str(root / "master")], "master")
            wait_for(lambda: request(master_origin, "/health"), "disposable master")
            admin = (root / "master/admin-token").read_text().strip()
            code = request(master_origin, "/api/pairing", {}, admin, expected=(201,))["code"]
            # Shadow Docker only in this setup child's PATH. Keep native service
            # utilities available and never modify the user's Docker installation.
            empty_path = root / "unavailable-docker"
            empty_path.mkdir()
            fake_docker = empty_path / ("docker.exe" if os.name == "nt" else "docker")
            fake_docker.write_bytes(b"not an executable" if os.name == "nt" else b"#!/bin/sh\nexit 127\n")
            fake_docker.chmod(0o755)
            setup, setup_log = launch(["setup", "--config", str(config_path), "--no-open"], "setup", {**os.environ, "PATH": str(empty_path) + os.pathsep + os.environ.get("PATH", "")})

            def setup_address():
                match = re.search(r"Worker setup: (http://127\.0\.0\.1:\d+)/#token=([a-f0-9]+)", setup_log.read_text())
                return match.groups() if match else None

            origin, token = wait_for(setup_address, "loopback setup address")
            assert setup.poll() is None
            mutation_headers = {"Origin": origin}

            def local(path, data=None, expected=(200,), headers=None):
                return request(origin, path, data, token, expected, headers if headers is not None else mutation_headers if data is not None else None)

            request(origin, "/api/setup/status", expected=(401, 403))
            request(origin, "/api/setup/status", token="wrong", expected=(401, 403))
            local("/api/setup/status", headers={"Host": "attacker.invalid"}, expected=(400, 403))
            page = request(origin, "/")
            assert isinstance(page, bytes) and b"<html" in page.lower()
            status = local("/api/setup/status")
            assert isinstance(status, dict)
            reopened = subprocess.run([str(binary), "setup", "--config", str(config_path), "--no-open"], capture_output=True, text=True, timeout=20)
            assert reopened.returncode == 0, reopened.stderr
            assert origin + "/#token=" + token in reopened.stdout, "Second setup process did not reuse its existing session"
            assert setup.poll() is None
            before = config_path.read_bytes()
            form = {"link": master_origin + "/#pair=" + code, "name": "Setup integration worker", "preset": "node-test", "mode": "native", "image": "node:22-bookworm-slim", "network": "none", "workspace": str(workspace), "allowSnapshots": True}
            local("/api/setup/pair", form, headers={"Origin": "http://attacker.invalid"}, expected=(403,))
            local("/api/setup/pair", form, headers={}, expected=(403,))
            local("/api/setup/pair", b"{", expected=(400, 422))
            local("/api/setup/pair", b" " * 16385, expected=(400, 413))
            for overrides in [
                {"allowSnapshots": False},
                {"preset": "arbitrary-shell"},
                {"argv": ["sh", "-c", "echo forbidden"]},
                {"workspace": "relative/path"},
                {"link": "http://8.8.8.8:7331/#pair=" + code},
                {"link": master_origin + "/?pair=" + code},
                {"mode": "docker"},
            ]:
                local("/api/setup/pair", {**form, **overrides}, expected=(400, 409, 422, 503))
                assert config_path.read_bytes() == before, "Rejected setup request changed owner configuration"
            result = local("/api/setup/pair", form, expected=(200, 201))
            config = json.loads(config_path.read_text())
            assert config["token"] and config["workerId"], "Pairing must be persisted before success"
            assert config["server"] == master_origin
            assert config["tasks"]["custom-local"] == custom, "Owner task was overwritten"
            preset = config["tasks"]["sharebox-tests"]
            assert preset["argv"] == ["node", "--test"]
            assert preset["snapshot"]["mode"] == "native"
            assert Path(preset["cwd"]).resolve() == workspace
            private = config["token"]
            assert private not in json.dumps(result)
            local_status = local("/api/setup/status")
            assert private not in json.dumps(local_status), "Setup status exposed worker credential"
            assert local_status["paired"] is True and local_status["master"]["authenticated"] is True
            registered = request(master_origin, "/api/workspace", token=admin)["workers"]
            assert next(w for w in registered if w["id"] == config["workerId"])["lastPollAt"] == 0, "Pairing or status must not pretend the worker is polling"
            saved = config_path.read_bytes()
            local("/api/setup/pair", form, expected=(400, 409))
            assert config_path.read_bytes() == saved
            # The code was consumed by pairing, despite not starting a worker/service.
            request(master_origin, "/api/worker/pair", {"code": code, "name": "Must not pair", "os": "test", "arch": "test", "tasks": [{"name": "test", "description": "Must never register"}]}, expected=(401,))
            # Sandbox failure must never silently turn into native execution.
            task_form = {key: value for key, value in form.items() if key not in ("link", "name")}
            local("/api/setup/task", {**task_form, "mode": "docker"}, expected=(400, 409, 422, 503))
            assert config_path.read_bytes() == saved
            # Replacing a revoked/lost pairing requires explicit owner intent.
            # A rejected code must leave the old identity and allowlist intact.
            previous = config_path.read_bytes()
            local("/api/setup/pair", {**form, "replacePairing": True, "link": master_origin + "/#pair=" + "0" * 64}, expected=(400,))
            assert config_path.read_bytes() == previous, "Failed replacement destroyed the existing pairing"
            new_code = request(master_origin, "/api/pairing", {}, admin, expected=(201,))["code"]
            local("/api/setup/pair", {**form, "replacePairing": True, "link": master_origin + "/#pair=" + new_code})
            replacement = json.loads(config_path.read_text())
            assert replacement["workerId"] != config["workerId"] and replacement["token"] != private
            assert replacement["tasks"]["custom-local"] == custom
            config, private = replacement, replacement["token"]
            assert private not in json.dumps(local("/api/setup/status"))
            print("PASS: explicit replacement pairing preserves old credentials on failure and custom tasks on success")
            local("/api/setup/task", {**task_form, "preset": "npm-test", "installDependencies": True}, expected=(200,))
            changed = json.loads(config_path.read_text())
            assert changed["token"] == private and changed["workerId"] == config["workerId"]
            assert changed["tasks"]["custom-local"] == custom
            assert changed["tasks"]["sharebox-tests"]["argv"] == ["npm.cmd" if os.name == "nt" else "npm", "test"]
            assert changed["tasks"]["sharebox-tests"]["snapshot"]["mode"] == "native"
            source = root / "source"
            source.mkdir()
            package = {"name": "sharebox-setup-fixture", "version": "1.0.0", "scripts": {"test": "node --test"}}
            (source / "package.json").write_text(json.dumps(package))
            (source / "package-lock.json").write_text(json.dumps({"name": package["name"], "version": "1.0.0", "lockfileVersion": 3, "requires": True, "packages": {"": {"name": package["name"], "version": "1.0.0"}}}))
            agent = request(master_origin, "/api/tokens", {"name": "Setup execution test"}, admin, expected=(201,))["token"]
            sequence = 0

            def tool(name, arguments):
                nonlocal sequence
                sequence += 1
                value = request(master_origin, "/mcp", {"jsonrpc": "2.0", "id": sequence, "method": "tools/call", "params": {"name": name, "arguments": arguments}}, agent)["result"]
                assert not value.get("isError"), value
                return json.loads(value["content"][0]["text"])

            def execute_preset(settings, marker):
                expected_platform = "linux" if settings["mode"] == "docker" else sys.platform
                (source / "setup.test.js").write_text("const test=require('node:test');const assert=require('node:assert/strict');test('uploaded setup snapshot',()=>{assert.equal(process.platform," + json.dumps(expected_platform) + ");console.log(" + json.dumps(marker) + ");});")
                uploaded = subprocess.run([str(binary), "snapshot", "upload", "--directory", str(source), "--server", master_origin, "--token-env", "SHAREBOX_SETUP_TEST_TOKEN"], env={**os.environ, "SHAREBOX_SETUP_TEST_TOKEN": agent}, capture_output=True, text=True, timeout=60)
                assert uploaded.returncode == 0, uploaded.stderr
                snapshot = json.loads(uploaded.stdout)["snapshotId"]

                def advertised():
                    workers = tool("list_workers", {})["workers"]
                    return any(w["id"] == config["workerId"] and w.get("lastPollAt", 0) > 0 and any(t["name"] == "sharebox-tests" and t.get("execution") == settings["mode"] for t in w["tasks"]) for w in workers)

                wait_for(advertised, settings["mode"] + " task advertisement")
                job = tool("submit_job", {"workerId": config["workerId"], "task": "sharebox-tests", "snapshotId": snapshot})

                def finished():
                    value = tool("get_job", {"jobId": job["id"]})
                    return value if value["status"] in ("passed", "failed", "lost", "cancelled") else None

                result = wait_for(finished, "setup " + settings["mode"] + " job", timeout=90)
                output = tool("get_job_logs", {"jobId": job["id"]})["log"]
                assert result["status"] == "passed", (result["status"], output)
                assert result["snapshotId"] == snapshot and marker in output
                if settings.get("installDependencies"):
                    assert "Preparation step 1 of 1" in output, "npm ci preparation did not execute"
                assert not (workspace / "setup.test.js").exists()
                managed = workspace / ".sharebox-snapshots"
                assert not managed.exists() or not any(managed.iterdir()), "Uploaded source/dependencies remained after the job"
                running = local("/api/setup/status")
                assert running["service"]["processRunning"] is True
                assert running["service"]["running"] is False, "Foreground worker must not be presented as an installed service"
                assert running["master"]["authenticated"] is True
                saved = config_path.read_bytes()
                busy = local("/api/setup/task", settings, expected=(400,))
                assert busy["error"].startswith(("This worker configuration is running", "The master recently heard from this worker")), busy
                assert config_path.read_bytes() == saved, "Running worker configuration was changed"
                duplicate = subprocess.run([str(binary), "worker", "--config", str(config_path)], capture_output=True, text=True, timeout=10)
                assert duplicate.returncode != 0 and "already running" in duplicate.stderr

            worker, _ = launch(["worker", "--config", str(config_path)], "native-worker")
            execute_preset({**task_form, "preset": "npm-test", "installDependencies": True}, "SETUP_NATIVE_NPM_SOURCE")
            worker.terminate()
            worker.wait(timeout=10)
            print("PASS: setup's native npm preset runs npm ci and uploaded tests; foreground status and mutation/duplicate-worker guards work")
            print("PASS: authenticated loopback setup, strict origin/host/input checks, registration link, persisted single-use pairing, owner task preservation, Docker fail-closed, credential redaction")
            docker_image = os.environ.get("SHAREBOX_DOCKER_IMAGE")
            if docker_image:
                # Reopen only the disposable setup process with the real Docker CLI.
                setup.terminate()
                setup.wait(timeout=10)
                setup, setup_log = launch(["setup", "--config", str(config_path), "--no-open"], "setup-docker")
                origin, token = wait_for(setup_address, "Docker setup address")
                mutation_headers = {"Origin": origin}
                assert local("/api/setup/check-docker", {})["docker"]["ready"] is True
                # Let the real final heartbeat age out; do not fake master state.
                workers = request(master_origin, "/api/workspace", token=admin)["workers"]
                last_poll = next(w for w in workers if w["id"] == config["workerId"])["lastPollAt"]
                time.sleep(max(0, (last_poll + 30000 - int(time.time() * 1000)) / 1000) + .2)
                local("/api/setup/task", {**task_form, "preset": "node-test", "mode": "docker", "image": docker_image})
                sandbox = json.loads(config_path.read_text())
                assert sandbox["token"] == private
                assert sandbox["tasks"]["custom-local"] == custom
                assert sandbox["tasks"]["sharebox-tests"]["snapshot"]["mode"] == "docker"
                worker, _ = launch(["worker", "--config", str(config_path)], "docker-worker")
                execute_preset({**task_form, "preset": "node-test", "mode": "docker", "image": docker_image}, "SETUP_SANDBOX_EXACT_SOURCE")
                print("PASS: local setup enables Docker, advertises task, and runs exact uploaded source in the sandbox")
            local("/api/setup/close", {})
            assert setup.wait(timeout=10) == 0
            assert not Path(str(config_path) + ".setup-session.json").exists(), "Setup session credential was not removed on close"
            assert request(master_origin, "/health")["ok"] is True
            if docker_image:
                assert worker.poll() is None, "Closing setup stopped the worker"
            print("PASS: setup session is reused and closes without stopping worker/master")
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            for log in logs:
                log.close()


if __name__ == "__main__":
    main()
