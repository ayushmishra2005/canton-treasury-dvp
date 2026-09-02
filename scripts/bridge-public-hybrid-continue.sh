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
ZAMA_WALLET="0x8a02c36B1c468eC02Db82f99b0D126646AE0Df93"
run_dir="$repo_root/canton/.run-bridge"
journal_dir="$run_dir/bridge-op"
accounts_dir="$testnet_dir/bridge-accounts"
engine_file="$testnet_dir/zama-engine.address"
client_file="$testnet_dir/zama-client.id"

trap cleanup_bridge_stack EXIT
init_bridge_stack
test -d zama/node_modules || fail "zama dependencies are not installed; run: (cd zama && npm ci)"
test -f daml/bridge-gateway/.daml/dist/bridge-gateway-0.1.0.dar || fail "missing bridge-gateway DAR; run: make build"
test -f daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar || fail "missing bridge-tests DAR; run: make build"
canton_jar="$(ls "$HOME/.dpm"/cache/components/canton-open-source/*/lib/canton-open-source-*.jar 2>/dev/null | sort -V | tail -1)"
[[ -n "$canton_jar" ]] || fail "canton runtime not found"
require_bridge_ports_free
test -f "$journal_dir/journal.json" || fail "missing main-operation journal"
test -f "$journal_dir/secrets.json" || fail "missing main-operation secrets"
test -f "$engine_file" || fail "missing reused Zama engine address"
test -f "$client_file" || fail "missing reused Zama client id"

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
log "PUBLIC_HYBRID_CONTINUE"
log "ZAMA_ENGINE_REUSED $zama_engine"

prepare_devnet_relayer_runtime "$testnet_dir/relayer-devnet-signer.json"
started_compose="$repo_root/bridge/relayer/docker-compose.yml"
RELAYER_CONFIG=./config.devnet.json docker compose -f "$started_compose" up -d
for _ in $(seq 1 90); do
  curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-devnet >/dev/null 2>&1 && break
  sleep 1
done
relayer_json="$(curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-devnet)" \
  || fail "OpenZeppelin Relayer 1.5.0 did not become ready on Devnet"
relayer_addr="$(printf '%s' "$relayer_json" | node -e 'const s=require("fs").readFileSync(0,"utf8"); const j=JSON.parse(s); process.stdout.write(j.data.address)')"
log "RELAYER_DEVNET_ADDRESS $relayer_addr"
printf '%s' "$relayer_json" | grep -q '"system_disabled":false' \
  || fail "Relayer is system_disabled; it is not pointing at Solana Devnet"

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
grep -q BRIDGE_BOOTSTRAP_COMPLETE "$run_dir/bridge-bootstrap.log" || fail "bridge bootstrap did not complete"
java -jar "$canton_jar" run canton/scripts/origination.canton \
  -c canton/remote-console.conf \
  --no-tty --log-level-stdout WARN >"$run_dir/origination.log" 2>&1 \
  || { tail -40 "$run_dir/origination.log" >&2; fail "treasury origination failed"; }
grep -q ORIGINATION_COMPLETE "$run_dir/origination.log" || fail "treasury origination did not complete"
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

assert_journal_step() {
  local dir="$1"
  local step="$2"
  python3 - "$dir/journal.json" "$step" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
actual = journal.get("completed")
expected = sys.argv[2]
if actual != expected:
    raise SystemExit(f"journal completed is {actual!r}, expected {expected!r}")
PY
}

solana_chain_unix() {
  python3 - "$DEVNET_RPC" <<'PY'
import json, struct, sys, urllib.request, base64
rpc=sys.argv[1]
req=urllib.request.Request(rpc, data=json.dumps({
    "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
    "params":["SysvarC1ock11111111111111111111111111111111",{"encoding":"base64"}],
}).encode(), headers={"content-type":"application/json"})
acc=json.load(urllib.request.urlopen(req, timeout=20))["result"]["value"]["data"][0]
print(struct.unpack_from("<q", base64.b64decode(acc), 32)[0])
PY
}

wait_chain_clock_past() {
  local expiry="$1"
  local now
  for _ in $(seq 1 180); do
    now="$(solana_chain_unix)"
    log "CHAIN_CLOCK $now RELEASE_ONCHAIN_EXPIRY $expiry"
    if [[ "$now" -ge "$expiry" ]]; then
      log "BRIDGE_RELEASE_APPROVAL_EXPIRED_ON_CHAIN"
      return 0
    fi
    sleep 2
  done
  fail "Solana chain clock $now did not reach expiry $expiry"
}

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

zama_result_line() {
  zama_rpc "$1" "$2" | tee -a "$evidence" | awk '/^ZAMA_RESULT /{line=$0} END{print line}'
}

require_unlocked_before_complete() {
  local disagree_dir="$run_dir/bridge-disagree"
  if [[ -f "$disagree_dir/journal.json" ]]; then
    python3 - "$disagree_dir/journal.json" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
if journal.get("completed") == "locked":
    raise SystemExit("PUBLIC_HYBRID_COMPLETE blocked: disagreement still locked")
PY
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

completed="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json')).get('completed') or '')")"
if [[ "$completed" == "locked" ]]; then
  log "BRIDGE_UNKNOWN_ATTESTER"
  BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --resume --inject-unknown-attester --halt-after-first-approval
  grep -q "FAULT_INJECTED unknown_attester" "$tmp_dir/ctd-workflow.log" || fail "unknown attester was not injected"
  grep -q UNKNOWN_ATTESTER_REJECTED "$tmp_dir/ctd-workflow.log" || fail "unknown attester was not rejected"
  log "BRIDGE_ATTESTER_DISAGREEMENT"
  BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --resume --inject-attester-disagreement
  grep -q "FAULT_INJECTED attester_disagreement" "$tmp_dir/ctd-workflow.log" \
    || fail "attester disagreement was not injected"
  grep -q ATTESTER_DISAGREEMENT_REJECTED "$tmp_dir/ctd-workflow.log" \
    || fail "attester disagreement was not rejected"
  assert_journal_step "$journal_dir" locked
  grep -q "RECOVERY_RESULT no_mint_without_quorum" "$tmp_dir/ctd-workflow.log" \
    || fail "conflicting vote was not proven below quorum"
fi

log "BRIDGE_SECOND_ATTESTATION"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after mint_approved
assert_journal_step "$journal_dir" mint_approved

for step in canton_minted trade_prepared reassigned; do
  log "BRIDGE_RESUME_AFTER $step"
  BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --resume --stop-after "$step"
done

log "BRIDGE_SETTLE"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after settled
assert_journal_step "$journal_dir" settled
grep -q DVP_BUYER_TREASURY "$tmp_dir/ctd-workflow.log" || fail "buyer did not receive Treasury"
grep -q DVP_SELLER_STABLECOIN "$tmp_dir/ctd-workflow.log" || fail "seller did not receive stablecoins"

log "BRIDGE_RESUME_AFTER redeemed"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after redeemed
assert_journal_step "$journal_dir" redeemed
grep -q "FAULT_INJECTED delayed_release_after_redemption" "$tmp_dir/ctd-workflow.log" \
  || fail "delayed release fault was not recorded"

log "BRIDGE_RELEASE_APPROVAL"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_RELEASE_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after release_approved
assert_journal_step "$journal_dir" release_approved
release_expiry="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['release_expiry'])")"
wait_chain_clock_past "$release_expiry"

log "BRIDGE_RELEASE"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_RELEASE_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after released
assert_journal_step "$journal_dir" released
grep -q RELEASE_REFRESHED_AFTER_CHAIN_EXPIRY "$tmp_dir/ctd-workflow.log" \
  || fail "expired release approval was not replaced"
grep -q RELAYER_RELEASE_CONFIRMED "$tmp_dir/ctd-workflow.log" \
  || fail "confidential release through Relayer did not confirm"
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "duplicate Relayer release"

log "BRIDGE_ZAMA_REDEEM"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
assert_journal_step "$journal_dir" zama_redeemed
grep -q ZAMA_REDEEM_OK "$tmp_dir/ctd-workflow.log" || fail "Zama redemption did not succeed"
[[ "$(grep -c ZAMA_REDEEM_OK "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "duplicate Zama redemption"

log "BRIDGE_RESUME_COMPLETED"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
resume_skips="$(grep -c COMPLETED_RESUME_SKIP_SETUP "$tmp_dir/ctd-workflow.log" || true)"
resume_recorded="$(grep -c OPERATION_RECORDED_COMPLETE "$tmp_dir/ctd-workflow.log" || true)"
if [[ "$resume_skips" -lt 2 && "$resume_recorded" -lt 2 ]]; then
  fail "completed operation was not resumed twice"
fi
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "resume repeated release"
[[ "$(grep -c ZAMA_REDEEM_OK "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "resume repeated Zama redeem"
grep -q CANTON_VERIFY_OK "$tmp_dir/ctd-workflow.log" || fail "connected Canton history was not verified"

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
[[ "$(zama_result_line status "$main_reservation")" == *'"status":4'* ]] \
  || fail "main reservation was not redeemed"
set +e
dup_redeem="$(zama_rpc redeem "$main_reservation" 2>&1)"
dup_status=$?
set -e
printf '%s\n' "$dup_redeem" | tee -a "$evidence"
[[ "$dup_status" -ne 0 ]] || fail "duplicate Zama redeem was accepted"
log "FAULT_DUPLICATE_ZAMA_REDEEM rejected"

record_public_evidence
require_unlocked_before_complete
grep -q ZAMA_RESERVE_TX "$evidence" || fail "main Zama reserve hash is missing"
grep -q ZAMA_FINALIZE_TX "$evidence" || fail "main Zama finalize hash is missing"
grep -q ZAMA_REDEEM_TX "$evidence" || fail "main Zama redeem hash is missing"
cp "$tmp_dir/ctd-workflow.log" "$log_dir/public-hybrid-workflow.log"
log "PUBLIC_HYBRID_COMPLETE"
echo "PUBLIC_HYBRID_COMPLETE"
