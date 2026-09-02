#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
# shellcheck source=bridge-local-stack.sh
source "$repo_root/scripts/bridge-local-stack.sh"

testnet_dir="$repo_root/bridge/.run/testnet"
log_dir="$testnet_dir/logs"
mkdir -p "$log_dir"
evidence="$log_dir/public-hybrid.log"
log() { printf '%s\n' "$*" | tee -a "$evidence"; }

DEVNET_RPC="https://api.devnet.solana.com"
SEPOLIA_RPC="https://ethereum-sepolia-rpc.publicnode.com"
DEVNET_PROGRAM="BkDwMbtMVhDWeQ1nHwvCKmTT2XZhP2RMYGw18c6imnPf"
run_dir="$repo_root/canton/.run-bridge"
journal_dir="$run_dir/bridge-op"
accounts_dir="$testnet_dir/bridge-accounts"
engine_file="$testnet_dir/zama-engine.address"
client_file="$testnet_dir/zama-client.id"

trap cleanup_bridge_stack EXIT
init_bridge_stack
canton_jar="$(ls "$HOME/.dpm"/cache/components/canton-open-source/*/lib/canton-open-source-*.jar 2>/dev/null | sort -V | tail -1)"
[[ -n "$canton_jar" ]] || fail "canton runtime not found"
require_bridge_ports_free
test -f "$journal_dir/journal.json" || fail "missing main-operation journal"

set +x
ZAMA_PRIVATE_KEY="$(tr -d '[:space:]' < "$testnet_dir/sepolia-deployer.key")"
export ZAMA_PRIVATE_KEY
export ZAMA_REQUESTER_KEY="$ZAMA_PRIVATE_KEY"
export ZAMA_SETTLER_KEY="$ZAMA_PRIVATE_KEY"
set -e
export ZAMA_RPC_URL="$SEPOLIA_RPC"
export ZAMA_HARDHAT_NETWORK=sepolia
export HARDHAT_TELEMETRY=false

zama_engine="$(tr -d '[:space:]' < "$engine_file")"
zama_client="$(tr -d '[:space:]' < "$client_file")"
log "PUBLIC_HYBRID_FINISH"

prepare_devnet_relayer_runtime "$testnet_dir/relayer-devnet-signer.json"
started_compose="$repo_root/bridge/relayer/docker-compose.yml"
RELAYER_CONFIG=./config.devnet.json docker compose -f "$started_compose" up -d
for _ in $(seq 1 90); do
  curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-devnet >/dev/null 2>&1 && break
  sleep 1
done
curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-devnet >/dev/null \
  || fail "OpenZeppelin Relayer 1.5.0 did not become ready on Devnet"

mkdir -p "$run_dir"
export CANTON_RUN_DIR="$run_dir"
: > "$run_dir/canton.log"
java -jar "$canton_jar" daemon \
  -c canton/settlement-topology.conf \
  --bootstrap canton/scripts/bootstrap.canton \
  --no-tty --log-level-stdout WARN >"$run_dir/canton.log" 2>&1 &
canton_pid=$!
for _ in $(seq 1 300); do
  grep -q BOOTSTRAP_COMPLETE "$run_dir/canton.log" && break
  if ! kill -0 "$canton_pid" 2>/dev/null; then
    tail -40 "$run_dir/canton.log" >&2
    fail "canton exited during bootstrap"
  fi
  sleep 1
done
grep -q BOOTSTRAP_COMPLETE "$run_dir/canton.log" || fail "canton bootstrap did not complete"
java -jar "$canton_jar" run canton/scripts/bridge-bootstrap.canton \
  -c canton/remote-console.conf \
  --no-tty --log-level-stdout WARN >"$run_dir/bridge-bootstrap.log" 2>&1 \
  || { tail -40 "$run_dir/bridge-bootstrap.log" >&2; fail "bridge bootstrap failed"; }
java -jar "$canton_jar" run canton/scripts/origination.canton \
  -c canton/remote-console.conf \
  --no-tty --log-level-stdout WARN >"$run_dir/origination.log" 2>&1 \
  || { tail -40 "$run_dir/origination.log" >&2; fail "treasury origination failed"; }
log "CANTON_PRIVATE_TOPOLOGY ready"

: > "$tmp_dir/ctd-workflow.log"
workflow_env=(
  SOLANA_RPC_URL="$DEVNET_RPC"
  RELAYER_URL=http://127.0.0.1:18080
  RELAYER_API_KEY="$RELAYER_API_KEY"
  RELAYER_ID=solana-devnet
  BRIDGE_PROGRAM_ID="$DEVNET_PROGRAM"
  BRIDGE_PAYER="$testnet_dir/devnet-deployer-keypair.json"
  ATTESTER_A="$testnet_dir/attester-a-keypair.json"
  ATTESTER_B="$testnet_dir/attester-b-keypair.json"
  ATTESTER_C="$testnet_dir/attester-c-keypair.json"
  ZAMA_RPC_URL="$SEPOLIA_RPC"
  ZAMA_HARDHAT_NETWORK=sepolia
  ZAMA_ENGINE="$zama_engine"
  ZAMA_CLIENT="$zama_client"
  ZAMA_REQUESTER_KEY="$ZAMA_REQUESTER_KEY"
  ZAMA_SETTLER_KEY="$ZAMA_SETTLER_KEY"
  CANTON_PARTICIPANTS="$run_dir/participants.json"
  CANTON_RUN_DIR="$run_dir"
  CANTON_JAR="$canton_jar"
  BRIDGE_AMOUNT=100000
  BRIDGE_ACCOUNT_DIR="$accounts_dir"
)
run_workflow() {
  local extra=("$@")
  set +e
  env "${workflow_env[@]}" cargo run --manifest-path bridge/Cargo.toml --quiet -- workflow "${extra[@]}" \
    | tee -a "$tmp_dir/ctd-workflow.log" | tee -a "$evidence"
  local status=${PIPESTATUS[0]}
  set -e
  if [[ "$status" -ne 0 ]]; then
    fail "bridge workflow failed at ${extra[*]:-complete}"
  fi
}

record_public_evidence() {
  python3 - "$journal_dir/journal.json" <<'PY' | tee -a "$evidence"
import json, sys
journal = json.load(open(sys.argv[1]))
pairs = [
    ("ZAMA_RESERVE_TX", journal.get("zama_reserve_tx", "")),
    ("ZAMA_RESERVE_GAS", journal.get("zama_reserve_gas", "")),
    ("ZAMA_FINALIZE_TX", journal.get("zama_finalize_tx", "")),
    ("ZAMA_FINALIZE_GAS", journal.get("zama_finalize_gas", "")),
    ("ZAMA_REDEEM_TX", journal.get("zama_redeem_tx", "")),
    ("ZAMA_REDEEM_GAS", journal.get("zama_redeem_gas", "")),
    ("MINT_APPROVAL_RELAYER_TX_A", journal.get("mint_approval_tx_a", "")),
    ("MINT_APPROVAL_SIG_A", journal.get("mint_approval_sig_a", "")),
    ("MINT_APPROVAL_RELAYER_TX_B", journal.get("mint_approval_tx_b", "")),
    ("MINT_APPROVAL_SIG_B", journal.get("mint_approval_sig_b", "")),
]
missing = [name for name, value in pairs if not value]
if missing:
    raise SystemExit("missing public evidence: " + ", ".join(missing))
for name, value in pairs:
    print(f"{name} {value}")
for name, value in pairs:
    if name.endswith("_TX") and value.startswith("0x"):
        print(f"{name}_EXPLORER https://sepolia.etherscan.io/tx/{value}")
PY
}

main_completed="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json')).get('completed') or '')")"
if [[ "$main_completed" == "zama_redeemed" ]]; then
  log "BRIDGE_RESUME_COMPLETED"
  BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
  BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
  resume_skips="$(grep -c COMPLETED_RESUME_SKIP_SETUP "$tmp_dir/ctd-workflow.log" || true)"
  resume_recorded="$(grep -c OPERATION_RECORDED_COMPLETE "$tmp_dir/ctd-workflow.log" || true)"
  if [[ "$resume_skips" -lt 2 && "$resume_recorded" -lt 2 ]]; then
    fail "completed operation was not resumed twice"
  fi
  grep -q CANTON_VERIFY_OK "$tmp_dir/ctd-workflow.log" || fail "connected Canton history was not verified"
fi

zama_rpc() {
  local method="$1"
  local args="$2"
  (cd "$repo_root/zama" && env \
    ZAMA_RPC_URL="$SEPOLIA_RPC" \
    ZAMA_ENGINE="$zama_engine" \
    ZAMA_KEY="$ZAMA_SETTLER_KEY" \
    ZAMA_METHOD="$method" \
    ZAMA_ARGS="$args" \
    ZAMA_HARDHAT_NETWORK=sepolia \
    npx hardhat run scripts/bridge-rpc.ts --network sepolia)
}

lock_id="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['lock_id'])")"
mint_holding="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['mint_holding'])")"
payout="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['payout_destination'])")"
canton_amount="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['canton_amount'])")"
log "BRIDGE_WRONG_LOCK_BINDING"
set +e
env CANTON_RUN_DIR="$run_dir" CANTON_JAR="$canton_jar" \
  BRIDGE_LOCK_ID="ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff" \
  BRIDGE_CANTON_AMOUNT="$canton_amount" \
  BRIDGE_TREASURY_AMOUNT=100.000000 \
  BRIDGE_PAYOUT_DEST="$payout" \
  BRIDGE_MINT_HOLDING="$mint_holding" \
  java -jar "$canton_jar" run canton/scripts/verify-bridge-completion.canton \
    -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
    > "$tmp_dir/ctd-wrong-lock.log" 2>&1
wrong_lock=$?
set -e
[[ "$wrong_lock" -ne 0 ]] || fail "wrong lock binding was accepted"
log "FAULT_WRONG_LOCK rejected"

log "BRIDGE_WRONG_TRADE_BINDING"
set +e
env CANTON_RUN_DIR="$run_dir" CANTON_JAR="$canton_jar" \
  BRIDGE_LOCK_ID="$lock_id" \
  BRIDGE_CANTON_AMOUNT="$canton_amount" \
  BRIDGE_TREASURY_AMOUNT=100.000000 \
  BRIDGE_PAYOUT_DEST="$payout" \
  BRIDGE_MINT_HOLDING="0000000000000000000000000000000000000000000000000000000000000000" \
  java -jar "$canton_jar" run canton/scripts/verify-bridge-completion.canton \
    -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
    > "$tmp_dir/ctd-wrong-trade.log" 2>&1
wrong_trade=$?
set -e
[[ "$wrong_trade" -ne 0 ]] || fail "wrong trade binding was accepted"
log "FAULT_WRONG_TRADE rejected"

main_reservation="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['reservation_hex'])")"
set +e
dup_redeem="$(zama_rpc redeem "$main_reservation" 2>&1)"
dup_status=$?
set -e
printf '%s\n' "$dup_redeem" | tee -a "$evidence"
[[ "$dup_status" -ne 0 ]] || fail "duplicate Zama redeem was accepted"
log "FAULT_DUPLICATE_ZAMA_REDEEM rejected"

disagree_dir="$run_dir/bridge-disagree"
if [[ -f "$disagree_dir/journal.json" ]]; then
  completed="$(python3 -c "import json; print(json.load(open('$disagree_dir/journal.json')).get('completed') or '')")"
  if [[ "$completed" == "locked" ]]; then
    log "BRIDGE_RECOVER_LOCKED_DISAGREEMENT"
    BRIDGE_JOURNAL_DIR="$disagree_dir" \
      run_workflow --journal "$disagree_dir" --resume --cancel-locked
  fi
  python3 - "$disagree_dir/journal.json" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
if journal.get("completed") == "locked":
    raise SystemExit("PUBLIC_HYBRID_COMPLETE blocked: disagreement still locked")
print("DISAGREE_TERMINAL", journal.get("completed"))
PY
  if grep -q CANCEL_LOCKED_COMPLETE "$tmp_dir/ctd-workflow.log"; then
    grep -q VAULT_UNLOCKED "$tmp_dir/ctd-workflow.log" \
      || fail "vault still holds locked disagreement value"
  fi
fi
record_public_evidence
python3 - "$journal_dir/journal.json" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
if journal.get("completed") != "zama_redeemed":
    raise SystemExit("PUBLIC_HYBRID_COMPLETE blocked: main operation is %s" % journal.get("completed"))
PY
cp "$tmp_dir/ctd-workflow.log" "$log_dir/public-hybrid-finish.log"
log "PUBLIC_HYBRID_COMPLETE"
echo "PUBLIC_HYBRID_COMPLETE"
