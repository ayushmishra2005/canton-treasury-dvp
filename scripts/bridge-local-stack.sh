# Shared local-stack helpers for bridge-e2e.sh and bridge-walkthrough.sh.
# Source this file after setting repo_root.

export PATH="${HOME}/.dpm/bin:${PATH}"
node22_bin="$(ls -d "${HOME}/.nvm/versions/node"/v22.*/bin 2>/dev/null | sort -V | tail -1 || true)"
if [[ -n "$node22_bin" ]]; then
  export PATH="${node22_bin}:${PATH}"
fi
export HARDHAT_TELEMETRY=false
export HARDHAT_DISABLE_TELEMETRY=1

BRIDGE_STACK_PORTS=(
  8899 8900 8545 18080 16379
  5001 5002 5003
  5011 5012 5021 5022 5031 5032 5041 5042 5051 5052 5061 5062
  5101 5102 5103
)

started_pids=()
started_compose=""
ledger_dir=""
canton_pid=""
tmp_dir=""
stack_failed=0
relayer_runtime_key=""
generated_local_signer=0

fail() {
  echo "$1" >&2
  stack_failed=1
  exit 1
}

require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"; }

require_node_22() {
  require_cmd node
  node -e 'if (process.versions.node.split(".")[0]!=="22") process.exit(1)' \
    || fail "Node 22 is required on PATH"
}

port_in_use() {
  lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1
}

port_free() {
  local port="$1"
  if port_in_use "$port"; then
    fail "port $port is already in use; refusing to start or to kill the occupant"
  fi
}

require_bridge_ports_free() {
  local port
  for port in "${BRIDGE_STACK_PORTS[@]}"; do
    port_free "$port"
  done
}

assert_bridge_ports_released() {
  local port leftover=()
  for port in "${BRIDGE_STACK_PORTS[@]}"; do
    if port_in_use "$port"; then
      leftover+=("$port")
    fi
  done
  if [[ ${#leftover[@]} -gt 0 ]]; then
    echo "required ports still bound: ${leftover[*]}" >&2
    return 1
  fi
  return 0
}

preserve_failure_output() {
  [[ "$stack_failed" -eq 1 ]] || return 0
  [[ -n "$tmp_dir" && -d "$tmp_dir" ]] || return 0
  echo "---- preserved local-stack logs from $tmp_dir ----" >&2
  local file
  for file in "$tmp_dir"/*.log; do
    [[ -f "$file" ]] || continue
    echo "==== $(basename "$file") ====" >&2
    tail -40 "$file" >&2 || true
  done
}

wait_for_exit() {
  local pid="$1"
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    local _
    for _ in $(seq 1 30); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}

wait_bridge_ports_released() {
  local _
  for _ in $(seq 1 30); do
    assert_bridge_ports_released && return 0
    sleep 1
  done
  assert_bridge_ports_released
}

cleanup_bridge_stack() {
  local rc=$?
  local pid
  [[ $rc -ne 0 ]] && stack_failed=1
  preserve_failure_output
  for pid in "${started_pids[@]:-}"; do
    wait_for_exit "$pid"
  done
  wait_for_exit "${canton_pid:-}"
  if [[ -n "$started_compose" ]]; then
    docker compose -f "$started_compose" down --remove-orphans >/dev/null 2>&1 || true
  fi
  if [[ -n "${relayer_runtime_key:-}" && -f "$relayer_runtime_key" ]]; then
    rm -f "$relayer_runtime_key"
  fi
  if [[ "${generated_local_signer:-0}" -eq 1 ]]; then
    rm -f "$repo_root/bridge/relayer/keys/local-signer.json"
  fi
  if [[ -n "$ledger_dir" && -d "$ledger_dir" ]]; then
    rm -rf "$ledger_dir"
  fi
  if [[ -n "$tmp_dir" && -d "$tmp_dir" ]]; then
    rm -rf "$tmp_dir"
  fi
  if ! wait_bridge_ports_released; then
    echo "required ports were still bound after cleanup" >&2
    stack_failed=1
  fi
  if [[ "$stack_failed" -ne 0 && "$rc" -eq 0 ]]; then
    exit 1
  fi
}

ESCROW_PROGRAM_ID="9Yuvt4HxfbGCL9gPk3ygMLV3UdrMFgAJsyhdoJvbKcUD"
RECORD_PROGRAM_ID="recr1L3PCGKLbckBqMNcJhuuyU1zgo8nBhfLVsJNwr5"
AGAVE_DEVNET_VALIDATOR_VERSION="v4.3.0-beta.3"

escrow_so() {
  echo "${repo_root}/solana/target/deploy/confidential_escrow.so"
}

record_so() {
  echo "${repo_root}/solana/target/deploy/spl_record.so"
}

agave_devnet_validator() {
  echo "${repo_root}/.cache/agave-${AGAVE_DEVNET_VALIDATOR_VERSION}/solana-release/bin/solana-test-validator"
}

require_devnet_matching_validator() {
  "$repo_root/scripts/fetch-agave-devnet-validator.sh"
  "$repo_root/scripts/build-spl-record.sh"
  test -x "$(agave_devnet_validator)" || fail "missing Devnet-matching solana-test-validator"
  test -f "$(record_so)" || fail "missing official Record program"
}

require_escrow_loaded() {
  solana program show "$ESCROW_PROGRAM_ID" --url http://127.0.0.1:8899 >/dev/null \
    || fail "confidential escrow $ESCROW_PROGRAM_ID is not loaded"
}

prepare_generated_relayer_secrets() {
  if [[ -z "${RELAYER_API_KEY:-}" ]]; then
    RELAYER_API_KEY="$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
    export RELAYER_API_KEY
  fi
  if [[ -z "${KEYSTORE_PASSPHRASE:-}" ]]; then
    KEYSTORE_PASSPHRASE="$(python3 -c 'import secrets; print("Tmp-" + secrets.token_urlsafe(18) + "!")')"
    export KEYSTORE_PASSPHRASE
  fi
}

prepare_local_relayer_runtime() {
  prepare_generated_relayer_secrets
  mkdir -p "$repo_root/bridge/relayer/keys"
  rm -f "$repo_root/bridge/relayer/keys/local-signer.json"
  node "$repo_root/scripts/write-relayer-keystore.mjs" \
    "$repo_root/bridge/relayer/keys/local-signer.json" "$KEYSTORE_PASSPHRASE"
  chmod 600 "$repo_root/bridge/relayer/keys/local-signer.json"
  generated_local_signer=1
}

prepare_devnet_relayer_runtime() {
  local source_key="$1"
  if [[ -z "${RELAYER_API_KEY:-}" ]]; then
    RELAYER_API_KEY="$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
    export RELAYER_API_KEY
  fi
  [[ -n "${KEYSTORE_PASSPHRASE:-}" ]] \
    || fail "KEYSTORE_PASSPHRASE is required to open the ignored Relayer signer"
  test -f "$source_key" || fail "missing ignored Relayer signer"
  chmod 600 "$source_key"
  mkdir -p "$repo_root/bridge/relayer/keys"
  relayer_runtime_key="$repo_root/bridge/relayer/keys/devnet-signer.json"
  cp "$source_key" "$relayer_runtime_key"
  chmod 600 "$relayer_runtime_key"
}

init_bridge_stack() {
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/ctd-bridge-XXXXXX")"
  require_cmd dpm
  require_cmd java
  require_cmd cargo
  require_cmd solana
  require_cmd solana-test-validator
  require_cmd docker
  require_cmd npx
  require_cmd anchor
  require_node_22
}
