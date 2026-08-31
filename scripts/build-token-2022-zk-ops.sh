#!/usr/bin/env bash
set -euo pipefail

# Official Token-2022 7.0.0 with zk-ops. Agave 3.1.10 ships Token-2022 without
# zk-ops, so Deposit/Transfer/ApplyPendingBalance return InvalidInstructionData.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIN_REPO="https://github.com/solana-program/token-2022.git"
PIN_TAG="program@v7.0.0"
PIN_COMMIT="ed6f74f960a3c06cf681c6b0a31552f2f4956df3"
src_dir="${TOKEN_2022_SRC:-$repo_root/.cache/token-2022-src}"
out_dir="$repo_root/solana/target/deploy"
out="$out_dir/spl_token_2022_zk_ops.so"

if [[ -f "$out" ]]; then
  echo "TOKEN_2022_ZK_OPS $out"
  exit 0
fi

command -v cargo-build-sbf >/dev/null 2>&1 || {
  echo "cargo-build-sbf is required to build Token-2022 with zk-ops" >&2
  exit 1
}

mkdir -p "$repo_root/.cache" "$out_dir"
if [[ ! -d "$src_dir/.git" ]]; then
  git clone --filter=blob:none --branch "$PIN_TAG" --single-branch "$PIN_REPO" "$src_dir"
fi
git -C "$src_dir" fetch --tags origin "$PIN_TAG"
git -C "$src_dir" checkout --detach "$PIN_COMMIT"
actual="$(git -C "$src_dir" rev-parse HEAD)"
if [[ "$actual" != "$PIN_COMMIT" ]]; then
  echo "Token-2022 checkout $actual does not match $PIN_COMMIT" >&2
  exit 1
fi

(
  cd "$src_dir"
  cargo-build-sbf --manifest-path program/Cargo.toml --sbf-out-dir "$out_dir" --features zk-ops
)
if [[ ! -f "$out_dir/spl_token_2022.so" ]]; then
  echo "cargo-build-sbf did not produce spl_token_2022.so" >&2
  exit 1
fi
cp "$out_dir/spl_token_2022.so" "$out"
echo "TOKEN_2022_ZK_OPS $out"
echo "TOKEN_2022_SOURCE $PIN_REPO $PIN_TAG $PIN_COMMIT"
