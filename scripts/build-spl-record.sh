#!/usr/bin/env bash
set -euo pipefail

# Official SPL Record 0.4.0. Local Agave 3.1.10 does not ship this program;
# confidential-transfer range proofs stage through it. Devnet already has it.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIN_REPO="https://github.com/solana-program/record.git"
PIN_TAG="program@v0.4.0"
PIN_COMMIT="c91de89de5497d0109df1954f6fae751e18d7b1d"
src_dir="${SPL_RECORD_SRC:-$repo_root/.cache/record}"
out_dir="$repo_root/solana/target/deploy"
out="$out_dir/spl_record.so"

if [[ -f "$out" ]]; then
  echo "SPL_RECORD $out"
  exit 0
fi

command -v cargo-build-sbf >/dev/null 2>&1 || {
  echo "cargo-build-sbf is required to build the official Record program" >&2
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
  echo "Record checkout $actual does not match $PIN_COMMIT" >&2
  exit 1
fi

(
  cd "$src_dir"
  cargo-build-sbf --manifest-path program/Cargo.toml --sbf-out-dir "$out_dir"
)
if [[ ! -f "$out" ]]; then
  echo "cargo-build-sbf did not produce spl_record.so" >&2
  exit 1
fi
echo "SPL_RECORD $out"
echo "SPL_RECORD_SOURCE $PIN_REPO $PIN_TAG $PIN_COMMIT"
