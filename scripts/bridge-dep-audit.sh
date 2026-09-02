#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

known_file=scripts/known-rustsec.txt
[[ -f "$known_file" ]] || { echo "missing $known_file" >&2; exit 1; }

echo "=== production npm audit (zama, omit=dev) ==="
(cd zama && npm audit --omit=dev)
echo "NPM_PRODUCTION_AUDIT_OK"

echo "=== Hardhat / mock-FHE development advisories (reported, not a production gate) ==="
set +e
(cd zama && npm audit)
dev_status=$?
set -e
if [[ "$dev_status" -eq 0 ]]; then
  echo "NPM_DEV_AUDIT_CLEAN"
else
  echo "NPM_DEV_AUDIT_REPORTED"
fi

require_cargo_audit() {
  if command -v cargo-audit >/dev/null 2>&1; then
    return 0
  fi
  echo "cargo-audit is required for RustSec checks; install with: cargo install cargo-audit --locked" >&2
  exit 1
}

audit_lockfile() {
  local lock="$1"
  local label="$2"
  echo "=== RustSec $label ($lock) ==="
  python3 - "$lock" "$known_file" "$label" <<'PY'
import json, subprocess, sys

lock, known_file, label = sys.argv[1], sys.argv[2], sys.argv[3]
known = {
    line.strip()
    for line in open(known_file, encoding="utf-8")
    if line.strip() and not line.startswith("#")
}
proc = subprocess.run(
    ["cargo", "audit", "--json", "--file", lock],
    capture_output=True,
    text=True,
)
payload = proc.stdout.strip() or proc.stderr.strip()
try:
    data = json.loads(payload[payload.find("{") :])
except json.JSONDecodeError:
    sys.stderr.write(proc.stdout)
    sys.stderr.write(proc.stderr)
    raise SystemExit(f"cargo audit did not return JSON for {lock}")

vulnerabilities = data.get("vulnerabilities", {})
if isinstance(vulnerabilities, dict):
    items = vulnerabilities.get("list", [])
else:
    items = vulnerabilities
found = []
for item in items:
    advisory = item.get("advisory") or item
    found.append(advisory.get("id", ""))
found = [item for item in found if item]
recorded = sorted(set(found) & known)
unexpected = sorted(set(found) - known)
print(f"RUSTSEC_{label}_RECORDED " + (" ".join(recorded) if recorded else "none"))
print(f"RUSTSEC_{label}_UNEXPECTED " + (" ".join(unexpected) if unexpected else "none"))
if unexpected:
    raise SystemExit(f"unrecorded RustSec advisories in {lock}: {' '.join(unexpected)}")
print(f"These recorded advisories remain because of the pinned Solana stack and are not fixed: {' '.join(recorded) if recorded else 'none'}")
PY
}

require_cargo_audit
audit_lockfile bridge/Cargo.lock BRIDGE
audit_lockfile bridge/devnet-zk/Cargo.lock DEVNET_ZK
audit_lockfile solana/Cargo.lock SOLANA
echo "DEP_AUDIT_OK"
