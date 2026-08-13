#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

dpm_home="${DPM_HOME:-$HOME/.dpm}"
canton_jar="$(ls "$dpm_home"/cache/components/canton-open-source/*/lib/canton-open-source-*.jar 2>/dev/null | sort -V | tail -1)"
if [[ -z "$canton_jar" ]]; then
  echo "canton runtime not found under $dpm_home/cache/components/canton-open-source" >&2
  exit 1
fi

integration_dar="daml/integration/.daml/dist/canton-treasury-dvp-integration-0.1.0.dar"
if [[ ! -f "$integration_dar" ]]; then
  echo "missing $integration_dar, run: dpm build --all" >&2
  exit 1
fi

run_dir="${CANTON_RUN_DIR:-canton/.run}"
export CANTON_RUN_DIR="$run_dir"
mkdir -p "$run_dir"
canton_log="$run_dir/canton.log"
: > "$canton_log"

cleanup() {
  if [[ -n "${canton_pid:-}" ]] && kill -0 "$canton_pid" 2>/dev/null; then
    kill "$canton_pid" 2>/dev/null || true
    wait "$canton_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

java -jar "$canton_jar" daemon \
  -c canton/settlement-topology.conf \
  --bootstrap canton/scripts/bootstrap.canton \
  --no-tty --log-level-stdout WARN >"$canton_log" 2>&1 &
canton_pid=$!

for _ in $(seq 1 240); do
  grep -q BOOTSTRAP_COMPLETE "$canton_log" && break
  if ! kill -0 "$canton_pid" 2>/dev/null; then
    echo "canton exited during bootstrap" >&2
    tail -30 "$canton_log" >&2
    exit 1
  fi
  sleep 1
done

if ! grep -q BOOTSTRAP_COMPLETE "$canton_log"; then
  echo "bootstrap did not complete" >&2
  tail -30 "$canton_log" >&2
  exit 1
fi

grep -E "^CONNECTED |^VETTED |^CHECKPOINT " "$canton_log"

dpm script \
  --dar "$integration_dar" \
  --script-name Integration.Scenario:dvpAcrossParticipants \
  --participant-config canton/participants.json \
  --wall-clock-time

java -jar "$canton_jar" run canton/scripts/verify-privacy.canton \
  -c canton/remote-console.conf \
  --no-tty --log-level-stdout WARN

echo "INTEGRATION_COMPLETE"
