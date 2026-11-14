# Contributing

Thanks for helping improve Sharebox. Use issues for reproducible bugs and focused feature proposals. For vulnerabilities, follow [SECURITY.md](SECURITY.md) instead of opening a public issue.

## Development

Install stable Rust, Node.js 22 or later, and Python 3. Read [AGENTS.md](AGENTS.md), fork the repository, and create a branch. The master and worker can run as separate processes on one machine for development.

Before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked
python tests/e2e.py
```

For dashboard changes, install the pinned browser test dependency and run the real browser flow:

```sh
npm install --no-save --package-lock=false --ignore-scripts playwright@1.56.1
npx playwright install chromium
node tests/ui.mjs
```

For background startup changes, run `python tests/service.py`. It temporarily registers a native master service, verifies stop/start/uninstall and retained state, and removes the registration. It preserves existing Sharebox services and explicitly skips when a native user session is unavailable. Use `SHAREBOX_REQUIRE_SERVICE_TEST=1` on a dedicated test host to treat skips as failures. A skipped lifecycle test is not evidence that that platform's service startup works.

Add regression tests for behavioral fixes, especially authentication, job ownership, cancellation, persistence, and HTTP parsing. Do not commit tokens, pairing codes, local worker configs, or generated data. Include the operating systems you tested in the pull request.

## Reviews and merging

Keep each pull request focused. Explain the user-visible outcome and relevant tradeoffs. The required `CI passed` check aggregates formatting, linting, and native platform tests. Maintainers should configure branch protection to require this check, one approving review, resolved discussions, and current branch status, while blocking force pushes and deletion. Repository configuration must be verified separately; this document does not activate those settings.

Maintainers can apply the included policy with `.github/configure-repository.sh` after authenticating GitHub CLI and creating `main`. The script also enables private vulnerability reporting. Run it only with authority to manage this repository.

The manually dispatched **Format Rust** workflow formats the selected branch and commits only Rust file changes. Use a feature branch once protection is enabled; it does not bypass branch protection. Commits made with the workflow token do not automatically trigger another workflow, so run **CI** manually on the resulting commit or update the pull request from an ordinary maintainer commit.

## Releases

1. Update the package version and lockfile in a pull request.
2. Merge after review and green CI.
3. Create a matching `vX.Y.Z` tag on that tested `main` commit.
4. The release workflow reruns CI and builds native binaries for Linux x86_64, macOS ARM64/x86_64, and Windows x86_64.
5. It uploads archives and `SHA256SUMS`, then publishes the draft only after all build jobs pass.
6. Verify installers against the published release. Do not advertise an installer as available before a release exists.

The release job has write permission only when publishing. Actions are pinned to commit SHAs, and Dependabot proposes updates. Checksums verify consistency with the release manifest; they are not independent publisher signatures. macOS builds are not notarized and Windows builds are not Authenticode-signed.

By submitting a contribution, you agree to license it under the project's [MIT license](LICENSE). Contributors retain their copyright. Follow the [Code of Conduct](CODE_OF_CONDUCT.md).
