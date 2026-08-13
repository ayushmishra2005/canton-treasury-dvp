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

required_ports=(5001 5002 5003 5011 5012 5021 5022 5031 5032 5041 5042 5051 5052 5061 5062 5101 5102 5103)
occupied=()
for port in "${required_ports[@]}"; do
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    occupied+=("$port")
  fi
done
if [[ ${#occupied[@]} -gt 0 ]]; then
  echo "ports already in use: ${occupied[*]}" >&2
  exit 1
fi

cleanup() {
  if [[ -n "${console_pid:-}" ]] && kill -0 "$console_pid" 2>/dev/null; then
    kill -9 "$console_pid" 2>/dev/null || true
  fi
  if [[ -n "${canton_pid:-}" ]] && kill -0 "$canton_pid" 2>/dev/null; then
    kill "$canton_pid" 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$canton_pid" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$canton_pid" 2>/dev/null || true
    wait "$canton_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

java -jar "$canton_jar" daemon \
  -c canton/settlement-topology.conf \
  --bootstrap canton/scripts/bootstrap.canton \
  --no-tty --log-level-stdout WARN >"$canton_log" 2>&1 &
canton_pid=$!

for _ in $(seq 1 300); do
  grep -q BOOTSTRAP_COMPLETE "$canton_log" && break
  if ! kill -0 "$canton_pid" 2>/dev/null; then
    echo "canton exited during bootstrap" >&2
    tail -40 "$canton_log" >&2
    exit 1
  fi
  sleep 1
done

if ! grep -q BOOTSTRAP_COMPLETE "$canton_log"; then
  echo "bootstrap did not complete" >&2
  tail -40 "$canton_log" >&2
  exit 1
fi

grep -E "^CONNECTED |^VETTED |^SYNCHRONIZER |^PARTY |^HOSTED_EXCLUSIVELY |^CHECKPOINT " "$canton_log"

console_phase() {
  local script="$1"
  local log="$run_dir/$(basename "${script%.canton}").log"
  java -jar "$canton_jar" run "$script" \
    -c canton/remote-console.conf \
    --no-tty --log-level-stdout WARN >"$log" 2>&1 &
  console_pid=$!
  local status=0
  wait "$console_pid" || status=$?
  console_pid=""
  if [[ $status -ne 0 ]]; then
    echo "console phase $script failed" >&2
    tail -40 "$log" >&2
    exit 1
  fi
  grep -E "^[A-Z_]+ |^[A-Z_]+$" "$log" || true
}

script_phase() {
  local name="$1"
  dpm script \
    --dar "$integration_dar" \
    --script-name "$name" \
    --participant-config "$run_dir/participants.json" \
    --wall-clock-time >"$run_dir/$(echo "$name" | tr ':.' '__').log" 2>&1 || {
      echo "daml script phase $name failed" >&2
      tail -20 "$run_dir/$(echo "$name" | tr ':.' '__').log" >&2
      exit 1
    }
  echo "SCRIPT_OK $name"
}

capability_phase() {
  REASSIGNMENT_CAPABILITY="$1" console_phase canton/scripts/reassignment-capability.canton
}

console_phase canton/scripts/origination.canton
if ! grep -q "AUTOMATIC_REASSIGNMENT_FOR_TRANSACTION_FAILED" "$run_dir/origination.log"; then
  echo "wrong-synchronizer prescription did not fail for the expected reason" >&2
  exit 1
fi
echo "WRONG_SYNCHRONIZER_REASON AUTOMATIC_REASSIGNMENT_FOR_TRANSACTION_FAILED"

script_phase Integration.Stage1:setup

capability_phase granted
console_phase canton/scripts/reassign.canton
capability_phase revoked

script_phase Integration.Stage2:settle

capability_phase granted
console_phase canton/scripts/probe-pending.canton
capability_phase revoked

console_phase canton/scripts/verify-privacy.canton

cleanup
trap - EXIT

for port in "${required_ports[@]}"; do
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "port $port is still bound after shutdown" >&2
    exit 1
  fi
done
echo "PORTS_RELEASED ${#required_ports[@]}"

echo "INTEGRATION_COMPLETE"
