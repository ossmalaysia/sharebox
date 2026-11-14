#!/bin/sh
# Install an official release binary after checking its SHA-256 manifest.
# VERSION=v0.1.0 SHAREBOX_INSTALL_DIR="$HOME/.local/bin" sh install.sh
set -eu
sharebox_repo='ossmalaysia/sharebox'
sharebox_version=${VERSION:-latest}
sharebox_install_dir=${SHAREBOX_INSTALL_DIR:-"$HOME/.local/bin"}
case "$sharebox_version" in
  latest) sharebox_release="https://github.com/$sharebox_repo/releases/latest/download" ;;
  v[0-9]*)
    case "$sharebox_version" in *[!A-Za-z0-9._-]*) echo 'Invalid VERSION. Use a release tag such as v0.1.0.' >&2; exit 1;; esac
    sharebox_release="https://github.com/$sharebox_repo/releases/download/$sharebox_version" ;;
  *) echo 'Invalid VERSION. Use latest or a release tag such as v0.1.0.' >&2; exit 1 ;;
esac
case "$(uname -s):$(uname -m)" in
  Darwin:arm64|Darwin:aarch64) sharebox_target=aarch64-apple-darwin ;;
  Darwin:x86_64) sharebox_target=x86_64-apple-darwin ;;
  Linux:x86_64) sharebox_target=x86_64-unknown-linux-gnu ;;
  *) echo 'No release binary for this platform. Build from source using cargo build --release.' >&2; exit 1 ;;
esac
for sharebox_command in curl tar awk install mktemp; do
  command -v "$sharebox_command" >/dev/null 2>&1 || { echo "Required command missing: $sharebox_command" >&2; exit 1; }
done
if command -v sha256sum >/dev/null 2>&1; then sharebox_hash=sha256sum
elif command -v shasum >/dev/null 2>&1; then sharebox_hash=shasum
else echo 'sha256sum or shasum is required.' >&2; exit 1
fi
sharebox_tmp=$(mktemp -d)
trap 'rm -rf "$sharebox_tmp"' EXIT HUP INT TERM
sharebox_asset="sharebox-$sharebox_target.tar.gz"
echo "Downloading $sharebox_asset ($sharebox_version)…"
curl --fail --show-error --silent --location --proto '=https' --tlsv1.2 "$sharebox_release/$sharebox_asset" --output "$sharebox_tmp/$sharebox_asset"
curl --fail --show-error --silent --location --proto '=https' --tlsv1.2 "$sharebox_release/SHA256SUMS" --output "$sharebox_tmp/SHA256SUMS"
sharebox_expected=$(awk -v asset="$sharebox_asset" '$2 == asset {print $1; count++} END {if (count != 1) exit 1}' "$sharebox_tmp/SHA256SUMS") || { echo 'Release manifest has no unique checksum for this archive.' >&2; exit 1; }
case "$sharebox_expected" in *[!0-9a-fA-F]*|'') echo 'Invalid release checksum.' >&2; exit 1;; esac
[ "${#sharebox_expected}" -eq 64 ] || { echo 'Invalid release checksum length.' >&2; exit 1; }
if [ "$sharebox_hash" = sha256sum ]; then sharebox_actual=$(sha256sum "$sharebox_tmp/$sharebox_asset" | awk '{print $1}')
else sharebox_actual=$(shasum -a 256 "$sharebox_tmp/$sharebox_asset" | awk '{print $1}')
fi
[ "$sharebox_actual" = "$sharebox_expected" ] || { echo 'Checksum mismatch. Nothing was installed.' >&2; exit 1; }
tar -xzf "$sharebox_tmp/$sharebox_asset" -C "$sharebox_tmp" sharebox
[ -f "$sharebox_tmp/sharebox" ] && [ ! -L "$sharebox_tmp/sharebox" ] || { echo 'Archive does not contain a regular sharebox executable.' >&2; exit 1; }
mkdir -p "$sharebox_install_dir"
install -m 755 "$sharebox_tmp/sharebox" "$sharebox_install_dir/sharebox"
echo "Installed: $sharebox_install_dir/sharebox"
case ":$PATH:" in *":$sharebox_install_dir:"*) ;; *) echo "Add $sharebox_install_dir to PATH, or invoke the installed path directly.";; esac
echo 'Start locally with: sharebox master --bind 127.0.0.1:7331 --data .sharebox'
