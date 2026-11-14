# Contributor and coding-agent instructions

## Scope

Sharebox is one Rust executable with master and worker modes for a trusted private LAN. Keep the local dashboard and MCP endpoint aligned with the real implementation. Do not present mock data or planned capabilities as working features.

## Implementation standards

- Preserve owner-defined task allowlists. Remote clients must not supply arbitrary commands, working directories, or credentials to execute.
- Keep network parsing bounded; reject malformed requests and oversized payloads. Keep logs bounded and avoid logging credentials.
- Persist state before reporting success. Preserve unique job ownership, token revocation, pairing-code expiry, timeout, and cancellation behavior.
- Keep macOS, Linux, and Windows branches explicit. Do not claim a platform is tested merely because its code compiles elsewhere.
- Prefer a small dependency set. Explain why a new dependency is needed and commit `Cargo.lock` changes with it.
- Use `cargo fmt`; return meaningful errors instead of panicking on untrusted input.
- Keep `.sharebox/`, worker credential files, build caches, and local test artifacts out of commits.

## Validation

Run checks appropriate to the change. Code changes normally require:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked
python tests/e2e.py
```

The local `scripts/dev.sh` helper can run Cargo with a cached toolchain and dependency registry. Pass `--offline` when network access is unavailable. Document checks that could not run; do not substitute invented results.

## Pull requests and releases

Use focused branches and describe the concrete behavior change plus validation. Do not merge without the required `CI passed` check and the repository's review requirements. Tag releases only after their commit is on `main` and CI is green; `vX.Y.Z` must match `Cargo.toml`. Release automation produces native archives and SHA-256 checksums. Installation must verify the matching checksum before copying a binary.

No cloud deployment, email login, source synchronization, arbitrary shell capability, or public-Internet exposure should be introduced implicitly. These require a deliberate scope change and corresponding security design.
