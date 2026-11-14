"""Real HTTP + Rust worker + Node integration. Requires listening sockets (CI/local terminal)."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    binary = Path(os.environ.get("SHAREBOX_BIN", "target/debug/sharebox" + (".exe" if os.name == "nt" else ""))).resolve()
    with tempfile.TemporaryDirectory(prefix="sharebox-e2e-") as directory:
        root = Path(directory)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        origin = f"http://127.0.0.1:{port}"
        log = open(root / "master.log", "w+")
        master = subprocess.Popen([str(binary), "master", "--bind", f"127.0.0.1:{port}", "--data", str(root / "state")], stdout=log, stderr=log)
        worker = None
        def request(path, data=None, token=None, method=None, expected=200, headers=None):
            hdr = {"Content-Type": "application/json", **(headers or {})}
            if token:
                hdr["Authorization"] = f"Bearer {token}"
            req = urllib.request.Request(origin + path, data=None if data is None else json.dumps(data).encode(), headers=hdr, method=method or ("POST" if data is not None else "GET"))
            try:
                with urllib.request.urlopen(req, timeout=10) as response:
                    status, raw = response.status, response.read()
            except urllib.error.HTTPError as error:
                status, raw = error.code, error.read()
            assert status == expected, (path, status, expected, raw)
            return json.loads(raw) if raw else None
        def wait_for(fn, timeout=30):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    result = fn()
                    if result:
                        return result
                except (ConnectionError, urllib.error.URLError, FileNotFoundError):
                    pass
                time.sleep(0.2)
            raise AssertionError("Timed out waiting for integration state")
        try:
            wait_for(lambda: request("/health"))
            admin = (root / "state/admin-token").read_text().strip()
            request("/api/me", expected=401)
            request("/api/me", token=admin, headers={"Origin": "http://evil.example"}, expected=403)
            assert request("/api/me", token=admin)["name"] == "Local workspace"
            request("/api/network", expected=401)
            request("/api/network", token=admin, headers={"Origin": "http://evil.example"}, expected=403)
            network = request("/api/network", token=admin)
            assert network["localOnly"] is True and network["urls"] == [], network
            assert network["bindAddress"] == f"127.0.0.1:{port}", network
            pair = request("/api/pairing", {}, admin, expected=201)
            cfg = root / "worker.json"
            config = {"name": "Integration worker", "server": origin, "tasks": {
                "pass": {"argv": ["node", "-e", "console.log('integration-ok')"], "cwd": str(root), "timeoutSeconds": 20},
                "fail": {"argv": ["node", "-e", "console.error('expected-failure');process.exit(7)"], "cwd": str(root), "timeoutSeconds": 20},
                "slow": {"argv": ["node", "-e", "console.log('started');setTimeout(()=>{},60000)"], "cwd": str(root), "timeoutSeconds": 60},
                "timeout": {"argv": ["node", "-e", "setTimeout(()=>{},60000)"], "cwd": str(root), "timeoutSeconds": 1}
            }}
            cfg.write_text(json.dumps(config))
            if os.name != "nt":
                cfg.chmod(0o600)
            worker_log = open(root / "worker.log", "w+")
            worker = subprocess.Popen([str(binary), "worker", "--server", origin, "--code", pair["code"], "--config", str(cfg)], stdout=worker_log, stderr=worker_log)
            workers = wait_for(lambda: request("/api/workspace", token=admin)["workers"])
            wid = workers[0]["id"]
            request("/api/worker/pair", {"code": pair["code"], "tasks": [{"name": "pass"}]}, expected=401)
            agent = request("/api/tokens", {"name": "Integration MCP"}, admin, expected=201)
            request("/api/pairing", {}, agent["token"], expected=403)
            request("/api/network", token=agent["token"], expected=403)
            counter = 0
            def mcp(method, params):
                nonlocal counter
                counter += 1
                return request("/mcp", {"jsonrpc": "2.0", "id": counter, "method": method, "params": params}, agent["token"])["result"]
            init = mcp("initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
            assert init["serverInfo"]["name"] == "sharebox"
            request("/mcp", {"jsonrpc": "2.0", "method": "notifications/initialized"}, agent["token"], expected=202)
            assert {"list_workers", "list_tasks", "submit_job", "get_job", "get_job_logs", "cancel_job"}.issubset({t["name"] for t in mcp("tools/list", {})["tools"]})
            def tool(name, arguments):
                value = mcp("tools/call", {"name": name, "arguments": arguments})
                assert not value.get("isError"), value
                return json.loads(value["content"][0]["text"])
            assert tool("list_workers", {})["workers"][0]["id"] == wid
            job = tool("submit_job", {"workerId": wid, "task": "pass", "requestKey": "repeat-safe"})
            same = tool("submit_job", {"workerId": wid, "task": "pass", "requestKey": "repeat-safe"})
            assert job["id"] == same["id"]
            def finished(jid):
                j = request(f"/api/jobs/{jid}", token=admin)
                return j if j["status"] in ("passed", "failed", "cancelled", "lost") else None
            assert wait_for(lambda: finished(job["id"]))["status"] == "passed"
            assert "integration-ok" in tool("get_job_logs", {"jobId": job["id"]})["log"]
            for task in ["fail", "timeout"]:
                j = tool("submit_job", {"workerId": wid, "task": task})
                final = wait_for(lambda: finished(j["id"]))
                assert final["status"] == "failed", final
                if task == "fail":
                    assert final["exitCode"] == 7
            j = tool("submit_job", {"workerId": wid, "task": "slow"})
            wait_for(lambda: request(f"/api/jobs/{j['id']}", token=admin)["status"] == "running")
            tool("cancel_job", {"jobId": j["id"]})
            assert wait_for(lambda: finished(j["id"]))["status"] == "cancelled"
            request("/api/jobs", {"workerId": wid, "task": "not-permitted"}, admin, expected=400)
            request(f"/api/tokens/{agent['id']}", token=admin, method="DELETE")
            request("/mcp", {"jsonrpc": "2.0", "id": 99, "method": "ping"}, agent["token"], expected=401)
            request(f"/api/workers/{wid}/revoke", {}, admin)
            wait_for(lambda: worker.poll() is not None)
            print("PASS: real HTTP pairing, MCP, Node success/failure, timeout, cancellation, idempotency, token and worker revocation")
        finally:
            for process in (worker, master):
                if process and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            log.flush()
            if worker:
                worker_log.close()
            log.close()


if __name__ == "__main__":
    main()
