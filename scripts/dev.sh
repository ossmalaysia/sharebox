#!/bin/sh
# Use the system Rust toolchain, or an existing rustup toolchain when its shims
# are not on PATH. Keep Cargo's writable cache inside this checkout.
set -eu
sharebox_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if command -v cargo >/dev/null 2>&1; then
  sharebox_cargo=$(command -v cargo)
else
  sharebox_cargo=""
  for sharebox_toolchain in "${RUSTUP_HOME:-$HOME/.rustup}"/toolchains/stable-*/bin; do
    if [ -x "$sharebox_toolchain/cargo" ] && [ -x "$sharebox_toolchain/rustc" ]; then
      sharebox_cargo="$sharebox_toolchain/cargo"
      PATH="$sharebox_toolchain:$PATH"
      export PATH
      break
    fi
  done
  if [ -z "$sharebox_cargo" ]; then
    echo 'Rust is not installed. Install the stable Rust toolchain, then retry.' >&2
    exit 1
  fi
fi
CARGO_HOME="$sharebox_root/.cargo-home"
export CARGO_HOME
cd "$sharebox_root"
if [ "$#" -eq 0 ]; then
  set -- build --release --offline
fi
exec "$sharebox_cargo" "$@"
