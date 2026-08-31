#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
hits=$(git ls-files --cached --others --exclude-standard \
  | grep -Ev '^(bridge/target/|solana/target/|zama/node_modules/|zama/artifacts/|zama/cache/|scripts/bridge-secret-scan.sh)' \
  | xargs grep -nE 'BEGIN (RSA |OPENSSH )?PRIVATE KEY|KEYSTORE_PASSPHRASE=[A-Za-z0-9]{8,}|RELAYER_API_KEY=[A-Za-z0-9]{16,}' \
  || true)
if [[ -n "$hits" ]]; then
  echo "$hits" >&2
  exit 1
fi
echo "SECRET_SCAN_OK"
