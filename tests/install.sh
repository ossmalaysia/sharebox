#!/bin/sh
# Offline installer regression tests. Downloads and platform detection are mocked;
# archive extraction, hashing, and installation use the real local commands.
set -eu
sharebox_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$sharebox_root"
python3 - <<'PY'
import hashlib
import os
import pathlib
import subprocess
import sys
import tarfile
import tempfile

installer = pathlib.Path('scripts/install.sh').resolve()
with tempfile.TemporaryDirectory(prefix='sharebox-installer-test-') as directory:
    root = pathlib.Path(directory)
    mocks = root / 'bin'
    fixtures = root / 'fixtures'
    destination = root / 'install with spaces'
    mocks.mkdir()
    fixtures.mkdir()
    payload = root / 'sharebox'
    payload.write_text('#!/bin/sh\necho installer-fixture\n')
    payload.chmod(0o755)
    asset = 'sharebox-aarch64-apple-darwin.tar.gz'
    archive = fixtures / asset
    with tarfile.open(archive, 'w:gz') as handle:
        handle.add(payload, arcname='sharebox')
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    manifest = fixtures / 'SHA256SUMS'
    valid_manifest = digest + '  ' + asset + '\n'
    manifest.write_text(valid_manifest)
    (mocks / 'uname').write_text(
        '#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n'
    )
    (mocks / 'curl').write_text(
        '#!' + sys.executable + '\n'
        'import os, pathlib, shutil, sys\n'
        'args = sys.argv[1:]\n'
        'url = next(arg for arg in args if arg.startswith("https://"))\n'
        'assert url.startswith("https://github.com/ossmalaysia/sharebox/releases/")\n'
        'assert "--proto" in args and "=https" in args\n'
        'output = args[args.index("--output") + 1]\n'
        'shutil.copyfile(pathlib.Path(os.environ["SHAREBOX_TEST_FIXTURES"]) / url.rsplit("/", 1)[-1], output)\n'
    )
    for executable in mocks.iterdir():
        executable.chmod(0o755)
    environment = dict(
        os.environ,
        PATH=str(mocks) + os.pathsep + os.environ['PATH'],
        SHAREBOX_TEST_FIXTURES=str(fixtures),
        SHAREBOX_INSTALL_DIR=str(destination),
        VERSION='v0.1.0',
    )

    def run():
        return subprocess.run(['sh', str(installer)], env=environment, capture_output=True, text=True)

    result = run()
    assert result.returncode == 0, result.stdout + result.stderr
    installed = destination / 'sharebox'
    assert installed.read_bytes() == payload.read_bytes()
    assert installed.stat().st_mode & 0o111
    print('PASS: verified pinned release installs into a directory containing spaces')

    environment['VERSION'] = 'latest'
    result = run()
    assert result.returncode == 0, result.stdout + result.stderr
    print('PASS: latest release installs through the same verification path')

    original = b'existing installation must survive failed verification\n'
    installed.write_bytes(original)
    for case, contents in (
        ('mismatch', '0' * 64 + '  ' + asset + '\n'),
        ('missing entry', digest + '  unrelated-archive.tar.gz\n'),
        ('duplicate entries', valid_manifest * 2),
        ('malformed checksum', 'not-a-checksum  ' + asset + '\n'),
    ):
        manifest.write_text(contents)
        result = run()
        assert result.returncode != 0, case + ': installer unexpectedly succeeded'
        assert installed.read_bytes() == original, case + ': existing installation was changed'
        print('PASS: ' + case + ' is rejected before installation')

    manifest.write_text(valid_manifest)
    environment['VERSION'] = 'v1/../../unexpected'
    result = run()
    assert result.returncode != 0 and installed.read_bytes() == original
    print('PASS: unsafe version path is rejected')

    environment['VERSION'] = 'latest'
    (mocks / 'uname').write_text('#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo unsupported;; esac\n')
    result = run()
    assert result.returncode != 0 and installed.read_bytes() == original
    print('PASS: unsupported platform is rejected')
PY
