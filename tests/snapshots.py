"""Real CLI upload -> master -> opt-in native worker; never uses a user's checkout."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    binary = Path(os.environ.get('SHAREBOX_BIN', 'target/debug/sharebox' + ('.exe' if os.name == 'nt' else ''))).resolve()
    node = shutil.which('node')
    assert node, 'Node.js is required'
    docker_image = os.environ.get('SHAREBOX_DOCKER_IMAGE')
    run_node = 'node' if docker_image else node
    snapshot_mode = {'mode': 'docker', 'image': docker_image} if docker_image else {'mode': 'native'}
    with tempfile.TemporaryDirectory(prefix='sharebox-snapshot-e2e-') as directory:
        root = Path(directory)
        source = root / 'source'; source.mkdir()
        workspace = root / 'workspaces'; workspace.mkdir()
        payload = b'content-addressed source\0' * 16000  # more than one chunk
        (source / 'payload.bin').write_bytes(payload)
        code = "const fs=require('node:fs');const assert=require('node:assert/strict');assert.equal(fs.readFileSync('prepared','utf8'),'yes');assert.equal(require('node:crypto').createHash('sha256').update(fs.readFileSync('payload.bin')).digest('hex'),'%s');console.log('SNAPSHOT_V1 '+process.platform);" % hashlib.sha256(payload).hexdigest()
        (source / 'test.js').write_text(code)
        (source / '.env').write_text('SECRET=must-never-transfer')
        (source / '.gitignore').write_text('ignored.txt\n')
        (source / 'ignored.txt').write_text('must-not-transfer')
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
        origin = f'http://127.0.0.1:{port}'
        master_log = open(root / 'master.log', 'w+')
        master = subprocess.Popen([str(binary), 'master', '--bind', f'127.0.0.1:{port}', '--data', str(root / 'state')], stdout=master_log, stderr=master_log)
        worker = None
        def request(path, data=None, token=None, method=None, expected=200, raw=False):
            body = data if isinstance(data, bytes) else None if data is None else json.dumps(data).encode()
            headers = {'Content-Type': 'application/octet-stream' if isinstance(data, bytes) else 'application/json'}
            if token: headers['Authorization'] = 'Bearer ' + token
            req = urllib.request.Request(origin + path, data=body, headers=headers, method=method or ('POST' if data is not None else 'GET'))
            try:
                with urllib.request.urlopen(req, timeout=15) as response: status, content = response.status, response.read()
            except urllib.error.HTTPError as error: status, content = error.code, error.read()
            assert status == expected, (path, status, expected, content[:1000])
            return content if raw else json.loads(content) if content else None
        def wait_for(check, timeout=45):
            end = time.monotonic() + timeout
            while time.monotonic() < end:
                try:
                    result = check()
                    if result: return result
                except (ConnectionError, urllib.error.URLError, FileNotFoundError): pass
                time.sleep(.15)
            raise AssertionError('Snapshot test timed out')
        try:
            wait_for(lambda: request('/health'))
            admin = (root / 'state/admin-token').read_text().strip()
            agent = request('/api/tokens', {'name': 'snapshot uploader'}, admin, expected=201)
            other = request('/api/tokens', {'name': 'other uploader'}, admin, expected=201)
            env = {**os.environ, 'SHAREBOX_TEST_TOKEN': agent['token']}
            def upload():
                result = subprocess.run([str(binary), 'snapshot', 'upload', '--directory', str(source), '--server', origin, '--token-env', 'SHAREBOX_TEST_TOKEN'], env=env, capture_output=True, text=True, timeout=60)
                assert result.returncode == 0, result.stderr
                return json.loads(result.stdout)
            uploaded = upload(); sid = uploaded['snapshotId']
            assert len(sid) == 64
            pair = request('/api/pairing', {}, admin, expected=201)
            prepare = [[run_node, '-e', "require('node:fs').writeFileSync('prepared','yes')"]]
            config = {'name': 'Snapshot test worker', 'server': origin, 'tasks': {
                'snapshot-node': {'argv': [run_node, 'test.js'], 'cwd': str(workspace), 'timeoutSeconds': 30, 'snapshot': {**snapshot_mode, 'prepare': prepare}},
                'snapshot-slow': {'argv': [run_node, 'test.js'], 'cwd': str(workspace), 'timeoutSeconds': 30, 'snapshot': {**snapshot_mode, 'prepare': [[run_node, '-e', "console.log('PREP_STARTED');setTimeout(()=>{},60000)"]]}},
                'local-only': {'argv': [node, '-e', "console.log('local')"], 'cwd': str(workspace), 'timeoutSeconds': 10}
            }}
            config_path = root / 'worker.json'; config_path.write_text(json.dumps(config))
            if os.name != 'nt': config_path.chmod(0o600)
            worker_log = open(root / 'worker.log', 'w+')
            worker = subprocess.Popen([str(binary), 'worker', '--server', origin, '--code', pair['code'], '--config', str(config_path)], stdout=worker_log, stderr=worker_log)
            workers = wait_for(lambda: request('/api/workspace', token=admin)['workers'])
            wid = workers[0]['id']
            sequence = 0
            def tool(name, arguments, token=None, fail=False):
                nonlocal sequence
                sequence += 1
                result = request('/mcp', {'jsonrpc': '2.0', 'id': sequence, 'method': 'tools/call', 'params': {'name': name, 'arguments': arguments}}, token or agent['token'])['result']
                if fail:
                    assert result.get('isError'), result
                    return result
                assert not result.get('isError'), result
                return json.loads(result['content'][0]['text'])
            # Edit the client checkout after upload: the old immutable input must run.
            (source / 'test.js').write_text(code.replace('SNAPSHOT_V1', 'SNAPSHOT_V2'))
            job = tool('submit_job', {'workerId': wid, 'task': 'snapshot-node', 'snapshotId': sid, 'requestKey': 'snapshot-repeat'})
            def final(jid):
                value = tool('get_job', {'jobId': jid})
                return value if value['status'] in ('passed', 'failed', 'cancelled', 'lost') else None
            result = wait_for(lambda: final(job['id']))
            assert result['status'] == 'passed', (result, tool('get_job_logs', {'jobId': job['id']}))
            assert result['snapshotId'] == sid
            log = tool('get_job_logs', {'jobId': job['id']})['log']
            assert 'SNAPSHOT_V1' in log and 'SNAPSHOT_V2' not in log
            assert tool('submit_job', {'workerId': wid, 'task': 'snapshot-node', 'snapshotId': sid, 'requestKey': 'snapshot-repeat'})['id'] == job['id']
            newer = upload()['snapshotId']; assert newer != sid
            tool('submit_job', {'workerId': wid, 'task': 'snapshot-node', 'snapshotId': newer, 'requestKey': 'snapshot-repeat'}, fail=True)
            tool('submit_job', {'workerId': wid, 'task': 'local-only', 'snapshotId': sid}, fail=True)
            tool('submit_job', {'workerId': wid, 'task': 'snapshot-node'}, fail=True)
            tool('submit_job', {'workerId': wid, 'task': 'snapshot-node', 'snapshotId': sid}, token=other['token'], fail=True)
            slow = tool('submit_job', {'workerId': wid, 'task': 'snapshot-slow', 'snapshotId': sid})
            wait_for(lambda: 'PREP_STARTED' in tool('get_job_logs', {'jobId': slow['id']})['log'])
            tool('cancel_job', {'jobId': slow['id']})
            assert wait_for(lambda: final(slow['id']))['status'] == 'cancelled'
            # No uploaded source or prepared files may remain in the owner's checkout.
            assert not (workspace / 'test.js').exists()
            managed = workspace / '.sharebox-snapshots'
            if managed.exists(): assert not any(managed.iterdir()), 'Disposable job workspaces must be removed'
            print('Execution mode:', snapshot_mode['mode'])
            print('PASS: multi-chunk CLI upload, exact immutable input, owner-opted execution, snapshot-aware idempotency, principal isolation, preparation cancellation and cleanup')
        finally:
            for proc in (worker, master):
                if proc and proc.poll() is None:
                    proc.terminate()
                    try: proc.wait(timeout=10)
                    except subprocess.TimeoutExpired: proc.kill(); proc.wait(timeout=5)
            master_log.close()
            if worker: worker_log.close()


if __name__ == '__main__':
    main()
