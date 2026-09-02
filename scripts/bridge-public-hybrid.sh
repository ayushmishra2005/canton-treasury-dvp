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

log "ZAMA_ROLES one_wallet requester_settler_deployer"
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

journal_dir="$run_dir/bridge-connected"
accounts_dir="$testnet_dir/bridge-accounts"
test -f "$accounts_dir/journal.json" || fail "missing reused bridge accounts"
test -f "$accounts_dir/secrets.json" || fail "missing reused bridge secrets"
rm -rf "$journal_dir"
mkdir -p "$accounts_dir"
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
  local hybrid_rc=${PIPESTATUS[0]}
  set -e
  if [[ "$hybrid_rc" -ne 0 ]]; then
    fail "bridge workflow failed at ${extra[*]:-complete}"
  fi
}

last_marker() { grep "$1" "$tmp_dir/ctd-workflow.log" | tail -1; }

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

log "BRIDGE_CONNECTED_OPERATION"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_RELEASE_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$accounts_dir" --reverse-endpoints --resume
assert_journal_step "$journal_dir" zama_redeemed
grep -q "REVERSED_ENDPOINTS source=9oQFsjme2n5w4qSxcwSxqnC2ZzifiHLJMuxNVw9fKXeV payout=658qCJawAVGBmqRbUWCvAv2xkX5vSkagHHX2s3mreAvD" \
  "$tmp_dir/ctd-workflow.log" || fail "source and destination were not reversed onto the funded account"
grep -q DVP_BUYER_TREASURY "$tmp_dir/ctd-workflow.log" || fail "buyer did not receive Treasury"
grep -q DVP_SELLER_STABLECOIN "$tmp_dir/ctd-workflow.log" || fail "seller did not receive stablecoins"
grep -q RELAYER_RELEASE_CONFIRMED "$tmp_dir/ctd-workflow.log" \
  || fail "confidential release through Relayer did not confirm"
grep -q ZAMA_REDEEM_OK "$tmp_dir/ctd-workflow.log" || fail "Zama redemption did not succeed"
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "duplicate Relayer release"
[[ "$(grep -c ZAMA_REDEEM_OK "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "duplicate Zama redemption"
grep -q "ACTION_COUNTS mint=1 settle=1 burn=1 release=1 zama=1" "$tmp_dir/ctd-workflow.log" \
  || fail "action counts are not mint=1 settle=1 burn=1 release=1 zama=1"
[[ "$(last_marker DEST_AVAILABLE)" == "DEST_AVAILABLE 100000000000" ]] \
  || fail "destination did not hold the released value"
[[ "$(last_marker DEST_PENDING)" == "DEST_PENDING 0" ]] \
  || fail "destination still has pending credit"

log "BRIDGE_RESUME_COMPLETED"
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
BRIDGE_MINT_EXPIRY_SECS=1800 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
resume_skips="$(grep -c COMPLETED_RESUME_SKIP_SETUP "$tmp_dir/ctd-workflow.log" || true)"
resume_recorded="$(grep -c OPERATION_RECORDED_COMPLETE "$tmp_dir/ctd-workflow.log" || true)"
verify_ok="$(grep -c CANTON_VERIFY_OK "$tmp_dir/ctd-workflow.log" || true)"
if [[ "$resume_skips" -lt 2 || "$resume_recorded" -lt 2 ]]; then
  fail "completed operation was not resumed twice"
fi
[[ "$verify_ok" -ge 2 ]] || fail "CANTON_VERIFY_OK was printed $verify_ok times"
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "resume repeated release"
[[ "$(grep -c ZAMA_REDEEM_OK "$tmp_dir/ctd-workflow.log")" -eq 1 ]] \
  || fail "resume repeated Zama redeem"

record_public_evidence
grep -q ZAMA_RESERVE_TX "$evidence" || fail "main Zama reserve hash is missing"
grep -q ZAMA_FINALIZE_TX "$evidence" || fail "main Zama finalize hash is missing"
grep -q ZAMA_REDEEM_TX "$evidence" || fail "main Zama redeem hash is missing"
grep -q MINT_APPROVAL_RELAYER_TX_A "$evidence" || fail "mint approval Relayer tx A is missing"
grep -q MINT_APPROVAL_RELAYER_TX_B "$evidence" || fail "mint approval Relayer tx B is missing"
cp "$tmp_dir/ctd-workflow.log" "$log_dir/public-hybrid-workflow.log"
log "PUBLIC_HYBRID_COMPLETE"
echo "PUBLIC_HYBRID_COMPLETE"
