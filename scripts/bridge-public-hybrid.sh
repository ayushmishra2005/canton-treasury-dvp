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
: > "$evidence"
log() { printf '%s\n' "$*" | tee -a "$evidence"; }

DEVNET_RPC="https://api.devnet.solana.com"
SEPOLIA_RPC="https://ethereum-sepolia-rpc.publicnode.com"
DEVNET_PROGRAM="BkDwMbtMVhDWeQ1nHwvCKmTT2XZhP2RMYGw18c6imnPf"
DEVNET_MINT="HLiwyBuuG2XS53Eg6RHVWDYkzDiuT4VTgKut1fMYQaja"
ZAMA_WALLET="0x8a02c36B1c468eC02Db82f99b0D126646AE0Df93"
run_dir="$repo_root/canton/.run-bridge"

trap cleanup_bridge_stack EXIT
init_bridge_stack
test -d zama/node_modules || fail "zama dependencies are not installed; run: (cd zama && npm ci)"
test -f daml/bridge-gateway/.daml/dist/bridge-gateway-0.1.0.dar || fail "missing bridge-gateway DAR; run: make build"
test -f daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar || fail "missing bridge-tests DAR; run: make build"
canton_jar="$(ls "$HOME/.dpm"/cache/components/canton-open-source/*/lib/canton-open-source-*.jar 2>/dev/null | sort -V | tail -1)"
[[ -n "$canton_jar" ]] || fail "canton runtime not found"
require_bridge_ports_free
test -f "$testnet_dir/devnet-deployer-keypair.json" || fail "missing ignored Devnet deployer"
test -f "$testnet_dir/sepolia-deployer.key" || fail "missing ignored Zama key"
test -f "$testnet_dir/attester-a-keypair.json" || fail "missing ignored attester A"

log "PUBLIC_HYBRID_COMMIT $(git rev-parse HEAD)"
log "PUBLIC_HYBRID_PROGRAM $DEVNET_PROGRAM"
solana program show "$DEVNET_PROGRAM" --url "$DEVNET_RPC" >/dev/null \
  || fail "Devnet escrow program is missing"
solana account "$DEVNET_MINT" --url "$DEVNET_RPC" >/dev/null \
  || fail "recorded Token-2022 mint is missing"
deployer_sol="$(solana balance 9EMA7SScSpniQFELnrEdQyYCozxfDhv6t6CKnc8ytdkF --url "$DEVNET_RPC")"
log "DEVNET_DEPLOYER_SOL $deployer_sol"

zama_balance_hex="$(curl -sS --max-time 20 -X POST -H 'content-type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$ZAMA_WALLET\",\"latest\"]}" \
  "$SEPOLIA_RPC" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"])')"
zama_chain="$(curl -sS --max-time 15 -X POST -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
  "$SEPOLIA_RPC" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')"
[[ "$zama_chain" == "11155111" ]] || fail "Sepolia chain id is $zama_chain"
python3 - "$zama_balance_hex" <<'PY' || fail "Zama wallet is not funded"
import sys
wei=int(sys.argv[1],16)
if wei < 10**16:
    raise SystemExit(1)
print("ZAMA_START_WEI", wei)
print("ZAMA_START_ETH", wei/1e18)
PY
log "ZAMA_WALLET $ZAMA_WALLET"

set +x
ZAMA_PRIVATE_KEY="$(tr -d '[:space:]' < "$testnet_dir/sepolia-deployer.key")"
export ZAMA_PRIVATE_KEY
export ZAMA_REQUESTER_KEY="$ZAMA_PRIVATE_KEY"
export ZAMA_SETTLER_KEY="$ZAMA_PRIVATE_KEY"
set -e

export ZAMA_RPC_URL="$SEPOLIA_RPC"
export ZAMA_HARDHAT_NETWORK=sepolia
export HARDHAT_TELEMETRY=false

log "ZAMA_VERSIONS @fhevm/solidity=0.11.1 plugin=0.4.2 relayer-sdk=0.4.1 oz-confidential=0.5.1"
log "ZAMA_OFFICIAL_LATEST solidity=0.13.3 plugin=0.4.2 relayer-sdk=0.4.4 oz-confidential=0.5.3"
log "ZAMA_PIN_REASON plugin-0.4.2-peers-solidity-caret-0.11.1"

(cd zama && npx hardhat compile --network sepolia) | tee -a "$evidence"
(cd zama && npx hardhat run scripts/check-sepolia-fhe.ts --network sepolia) | tee -a "$evidence"
grep -q ZAMA_FHE_CHECK_OK "$evidence" || fail "Sepolia FHE check failed"
grep -q 'ZAMA_MOCK_FHE false' "$evidence" || fail "mock FHE is not disabled"
(cd zama && npx hardhat fhevm resolve-fhevm-config \
  --acl 0xf0Ffdc93b7E186bC2f8CB3dAA75D86d1930A433D \
  --kms 0xbE0E383937d564D7FF0BC3b46c51f0bF8d5C311A \
  --network sepolia) | tee -a "$evidence"

engine_file="$testnet_dir/zama-engine.address"
client_file="$testnet_dir/zama-client.id"
if [[ -f "$engine_file" ]]; then
  existing="$(tr -d '[:space:]' < "$engine_file")"
  code="$(curl -sS --max-time 20 -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getCode\",\"params\":[\"$existing\",\"latest\"]}" \
    "$SEPOLIA_RPC" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"])')"
  if [[ "$code" != "0x" && "$code" != "0x0" ]]; then
    log "ZAMA_ENGINE_REUSED $existing"
    printf '%s\n' "ZAMA_ENGINE $existing" >> "$tmp_dir/ctd-zama-deploy.log"
    if [[ -f "$client_file" ]]; then
      printf '%s\n' "ZAMA_CLIENT $(tr -d '[:space:]' < "$client_file")" >> "$tmp_dir/ctd-zama-deploy.log"
    fi
  fi
fi
if ! grep -q 'ZAMA_ENGINE ' "$tmp_dir/ctd-zama-deploy.log" 2>/dev/null; then
  (cd zama && ZAMA_CAPACITY=200000000000 npx hardhat run scripts/deploy.ts --network sepolia) \
    | tee "$tmp_dir/ctd-zama-deploy.log" | tee -a "$evidence"
fi
grep -q 'ZAMA_ENGINE ' "$tmp_dir/ctd-zama-deploy.log" || fail "Zama deploy did not print ZAMA_ENGINE"
grep -q 'ZAMA_CLIENT ' "$tmp_dir/ctd-zama-deploy.log" || fail "Zama deploy did not print ZAMA_CLIENT"
zama_engine="$(awk '/ZAMA_ENGINE /{print $2}' "$tmp_dir/ctd-zama-deploy.log" | tail -1)"
zama_client="$(awk '/ZAMA_CLIENT /{print $2}' "$tmp_dir/ctd-zama-deploy.log" | tail -1)"
printf '%s\n' "$zama_engine" > "$engine_file"
printf '%s\n' "$zama_client" > "$client_file"
(cd zama && npx hardhat fhevm check-fhevm-compatibility --address "$zama_engine" --network sepolia) \
  | tee -a "$evidence"

export RELAYER_API_KEY="${RELAYER_API_KEY:-bridge-local-api-key-32chars-min}"
export KEYSTORE_PASSPHRASE="${KEYSTORE_PASSPHRASE:-Bridge-Local-1!}"
mkdir -p bridge/relayer/keys
if [[ ! -f "$testnet_dir/relayer-devnet-signer.json" ]]; then
  node scripts/write-relayer-keystore.mjs \
    "$testnet_dir/relayer-devnet-signer.json" "$KEYSTORE_PASSPHRASE"
fi
cp "$testnet_dir/relayer-devnet-signer.json" bridge/relayer/keys/devnet-signer.json
started_compose="$repo_root/bridge/relayer/docker-compose.yml"
RELAYER_CONFIG=./config.devnet.json docker compose -f "$started_compose" up -d
for _ in $(seq 1 90); do
  curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-devnet >/dev/null 2>&1 && break
  sleep 1
done
relayer_json="$(curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-devnet)" \
  || fail "OpenZeppelin Relayer 1.5.0 did not become ready on Devnet"
relayer_addr="$(printf '%s' "$relayer_json" | node -e 'const s=require("fs").readFileSync(0,"utf8"); const j=JSON.parse(s); process.stdout.write(j.data.address)')"
[[ -n "$relayer_addr" ]] || fail "Relayer did not report a Solana address"
log "RELAYER_DEVNET_ADDRESS $relayer_addr"
printf '%s' "$relayer_json" | grep -q '"system_disabled":false' \
  || fail "Relayer is system_disabled; it is not pointing at Solana Devnet"
relayer_bal="$(solana balance "$relayer_addr" --url "$DEVNET_RPC" | awk '{print $1}')"
python3 - "$relayer_bal" <<'PY' || true
import sys
print(sys.argv[1])
PY
if python3 - "$relayer_bal" <<'PY'
import sys
raise SystemExit(0 if float(sys.argv[1]) >= 0.4 else 1)
PY
then
  log "RELAYER_ALREADY_FUNDED $relayer_bal"
else
  solana transfer "$relayer_addr" 1 \
    --from "$testnet_dir/devnet-deployer-keypair.json" \
    --url "$DEVNET_RPC" \
    --allow-unfunded-recipient \
    --output json | tee -a "$evidence"
fi

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

expiry_dir="$run_dir/bridge-expiry"
journal_dir="$run_dir/bridge-op"
accounts_dir="$testnet_dir/bridge-accounts"
mkdir -p "$accounts_dir"
rm -rf "$expiry_dir" "$journal_dir"
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

assert_no_lock_or_mint() {
  local dir="$1"
  python3 - "$dir/journal.json" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
if journal.get("lock_signature") or journal.get("mint_holding") or journal.get("lock_proof_hex"):
    raise SystemExit("rejected reservation produced lock or mint evidence")
completed = journal.get("completed")
if completed not in (None, "accounts"):
    raise SystemExit(f"rejected reservation advanced to {completed}")
print("REJECT_JOURNAL_CLEAN")
PY
}

last_marker() { grep "$1" "$tmp_dir/ctd-workflow.log" | tail -1; }

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

log "BRIDGE_UNCONFIGURED_CLIENT"
fresh_id() { python3 -c "import os; print('0x'+os.urandom(32).hex())"; }
unconfigured_id="$(fresh_id)"
set +e
unconfigured_out="$(zama_rpc reserve "$unconfigured_id,$(python3 -c 'print("0x"+("11"*32))'),100000000000" 2>&1)"
unconfigured_status=$?
set -e
printf '%s\n' "$unconfigured_out" | tee -a "$evidence"
if [[ "$unconfigured_status" -eq 0 ]] && grep -q '"approved":true' <<<"$unconfigured_out"; then
  fail "unconfigured Zama client was approved"
fi
log "FAULT_UNCONFIGURED_CLIENT rejected"

log "BRIDGE_EXPIRY_RECOVERY"
BRIDGE_MINT_EXPIRY_SECS=300 BRIDGE_JOURNAL_DIR="$expiry_dir" \
  run_workflow --journal "$expiry_dir" --resume --expiry-recovery
grep -q EXPIRY_RECOVERY_COMPLETE "$tmp_dir/ctd-workflow.log" || fail "expiry recovery did not complete"
grep -q "FAULT_INJECTED expiry_before_settlement" "$tmp_dir/ctd-workflow.log" \
  || fail "expiry-before-settlement fault was not recorded"

reject_dir="$run_dir/bridge-reject"
rm -rf "$reject_dir"
log "BRIDGE_REJECT_OVER_CAPACITY"
set +e
env "${workflow_env[@]}" BRIDGE_AMOUNT=300000 BRIDGE_JOURNAL_DIR="$reject_dir" \
  cargo run --manifest-path bridge/Cargo.toml --quiet -- workflow \
  --journal "$reject_dir" --reuse-from "$expiry_dir" --resume --stop-after reserved \
  | tee -a "$tmp_dir/ctd-workflow.log" | tee -a "$evidence"
reject_status=${PIPESTATUS[0]}
set -e
[[ "$reject_status" -ne 0 ]] || fail "over-capacity reservation should be rejected"
grep -q ZAMA_RESERVATION_REJECTED "$tmp_dir/ctd-workflow.log" || fail "over-capacity rejection was not recorded"
assert_no_lock_or_mint "$reject_dir"

log "BRIDGE_RESUME_AFTER accounts"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after accounts
log "BRIDGE_RESUME_AFTER reserved"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after reserved
log "BRIDGE_RESUME_AFTER locked"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after locked
assert_journal_step "$journal_dir" locked

log "BRIDGE_UNKNOWN_ATTESTER"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --inject-unknown-attester --halt-after-first-approval
grep -q "FAULT_INJECTED unknown_attester" "$tmp_dir/ctd-workflow.log" || fail "unknown attester was not injected"
grep -q UNKNOWN_ATTESTER_REJECTED "$tmp_dir/ctd-workflow.log" || fail "unknown attester was not rejected"

log "BRIDGE_ONE_ATTESTER"
[[ "$(last_marker MINT_APPROVAL_BITMAP)" == "MINT_APPROVAL_BITMAP 1" ]] \
  || fail "one-attester state was not recorded: $(last_marker MINT_APPROVAL_BITMAP)"

log "BRIDGE_ATTESTER_DISAGREEMENT"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --inject-attester-disagreement
grep -q "FAULT_INJECTED attester_disagreement" "$tmp_dir/ctd-workflow.log" \
  || fail "attester disagreement was not injected"
grep -q ATTESTER_DISAGREEMENT_REJECTED "$tmp_dir/ctd-workflow.log" \
  || fail "attester disagreement was not rejected"
assert_journal_step "$journal_dir" locked

log "BRIDGE_SECOND_ATTESTATION"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after mint_approved
assert_journal_step "$journal_dir" mint_approved

for step in canton_minted trade_prepared reassigned; do
  log "BRIDGE_RESUME_AFTER $step"
  BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after "$step"
done

log "BRIDGE_SETTLE"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after settled
assert_journal_step "$journal_dir" settled
grep -q DVP_BUYER_TREASURY "$tmp_dir/ctd-workflow.log" || fail "buyer did not receive Treasury"
grep -q DVP_SELLER_STABLECOIN "$tmp_dir/ctd-workflow.log" || fail "seller did not receive stablecoins"

log "BRIDGE_RESUME_AFTER redeemed"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after redeemed
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

cp "$tmp_dir/ctd-workflow.log" "$log_dir/public-hybrid-workflow.log"
log "PUBLIC_HYBRID_COMPLETE"
echo "PUBLIC_HYBRID_COMPLETE"
