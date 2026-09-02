#!/usr/bin/env bash
set -euo pipefail

# Official Agave build that matches current Solana Devnet (4.3.0-beta.3).
# Used only for the local test-validator so ZK ElGamal proofs generated
# with solana-zk-sdk 6.0.1 verify the same way they do on Devnet.
# This does not replace the active Solana CLI.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="v4.3.0-beta.3"
dest="$repo_root/.cache/agave-${VERSION}"
validator="$dest/solana-release/bin/solana-test-validator"

if [[ -x "$validator" ]]; then
  echo "AGAVE_TEST_VALIDATOR $validator"
  exit 0
fi

arch="$(uname -m)"
case "$arch" in
  arm64|aarch64) asset="solana-release-aarch64-apple-darwin.tar.bz2" ;;
  x86_64) asset="solana-release-x86_64-apple-darwin.tar.bz2" ;;
  *)
    echo "unsupported architecture $arch for the Devnet-matching test validator" >&2
    exit 1
    ;;
esac

url="https://github.com/anza-xyz/agave/releases/download/${VERSION}/${asset}"
mkdir -p "$dest"
archive="$dest/${asset}"
if [[ ! -f "$archive" ]]; then
  curl -fsSL "$url" -o "$archive"
fi
tar -xjf "$archive" -C "$dest"
if [[ ! -x "$validator" ]]; then
  echo "extracted Agave $VERSION but $validator is missing" >&2
  exit 1
fi
echo "AGAVE_TEST_VALIDATOR $validator"
echo "AGAVE_SOURCE $url"
