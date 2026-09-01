#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
export PATH="$HOME/.dpm/bin:$HOME/.nvm/versions/node/v22.9.0/bin:$PATH"

started_pids=()
started_compose=""
ledger_dir=""
canton_pid=""
dpm_home="${DPM_HOME:-$HOME/.dpm}"
run_dir="$repo_root/canton/.run-bridge"

fail() { echo "$1" >&2; exit 1; }

require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"; }

port_free() {
  local port="$1"
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    fail "port $port is already in use; refusing to start or to kill the occupant"
  fi
}

cleanup() {
  local pid
  for pid in "${started_pids[@]:-}"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  if [[ -n "${canton_pid:-}" ]] && kill -0 "$canton_pid" 2>/dev/null; then
    kill "$canton_pid" 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$canton_pid" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$canton_pid" 2>/dev/null || true
    wait "$canton_pid" 2>/dev/null || true
  fi
  if [[ -n "$started_compose" ]]; then
    docker compose -f "$started_compose" down --remove-orphans >/dev/null 2>&1 || true
  fi
  if [[ -n "$ledger_dir" && -d "$ledger_dir" ]]; then
    rm -rf "$ledger_dir"
  fi
}
trap cleanup EXIT

require_cmd dpm
require_cmd java
require_cmd cargo
require_cmd solana
require_cmd solana-test-validator
require_cmd node
require_cmd docker
require_cmd npx
require_cmd anchor
node -e 'if (process.versions.node.split(".")[0]!=="22") process.exit(1)' || fail "Node 22 is required"
test -d zama/node_modules || fail "zama dependencies are not installed; run: (cd zama && npm ci)"
test -f daml/bridge-gateway/.daml/dist/bridge-gateway-0.1.0.dar || fail "missing bridge-gateway DAR; run: make build"
test -f daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar || fail "missing bridge-tests DAR; run: make build"
canton_jar="$(ls "$dpm_home"/cache/components/canton-open-source/*/lib/canton-open-source-*.jar 2>/dev/null | sort -V | tail -1)"
[[ -n "$canton_jar" ]] || fail "canton runtime not found under $dpm_home/cache/components/canton-open-source"

for port in 8899 8900 8545 18080 16379 5001 5002 5003 5011 5012 5021 5022 5031 5032 5041 5042 5051 5052 5061 5062 5101 5102 5103; do
  port_free "$port"
done

(cd solana && anchor build)
test -f solana/target/deploy/confidential_escrow.so || fail "missing confidential_escrow.so"
"$repo_root/scripts/build-token-2022-zk-ops.sh"
token_2022_so="$repo_root/solana/target/deploy/spl_token_2022_zk_ops.so"
test -f "$token_2022_so" || fail "missing Token-2022 zk-ops program; run scripts/build-token-2022-zk-ops.sh"

ledger_dir="$(mktemp -d "${TMPDIR:-/tmp}/ctd-solana-XXXXXX")"
solana-test-validator --reset --quiet --ledger "$ledger_dir" --rpc-port 8899 --faucet-port 9900 \
  --bpf-program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb "$token_2022_so" >/tmp/ctd-solana.log 2>&1 &
started_pids+=("$!")
for _ in $(seq 1 60); do
  solana cluster-version --url http://127.0.0.1:8899 >/dev/null 2>&1 && break
  sleep 1
done
solana cluster-version --url http://127.0.0.1:8899 >/dev/null || fail "solana-test-validator did not start"
solana airdrop 100 --url http://127.0.0.1:8899 >/dev/null
solana account ZkE1Gama1Proof11111111111111111111111111111 --url http://127.0.0.1:8899 >/dev/null \
  || fail "zk-elgamal-proof program is missing on the local validator"
solana program deploy solana/target/deploy/confidential_escrow.so \
  --program-id solana/target/deploy/confidential_escrow-keypair.json \
  --url http://127.0.0.1:8899 >/tmp/ctd-program-deploy.log \
  || fail "failed to deploy confidential escrow program"

export RELAYER_API_KEY="${RELAYER_API_KEY:-bridge-local-api-key-32chars-min}"
export KEYSTORE_PASSPHRASE="${KEYSTORE_PASSPHRASE:-Bridge-Local-1!}"
mkdir -p bridge/relayer/keys
if [[ ! -f bridge/relayer/keys/local-signer.json ]]; then
  node scripts/write-relayer-keystore.mjs \
    bridge/relayer/keys/local-signer.json "$KEYSTORE_PASSPHRASE"
fi
started_compose="$repo_root/bridge/relayer/docker-compose.yml"
docker compose -f "$started_compose" up -d
for _ in $(seq 1 90); do
  curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-local >/dev/null 2>&1 && break
  sleep 1
done
relayer_json="$(curl -sf -H "Authorization: Bearer $RELAYER_API_KEY" http://127.0.0.1:18080/api/v1/relayers/solana-local)" \
  || fail "OpenZeppelin Relayer 1.5.0 did not become ready"
relayer_addr="$(printf '%s' "$relayer_json" | node -e 'const s=require("fs").readFileSync(0,"utf8"); const j=JSON.parse(s); process.stdout.write(j.data.address)')"
[[ -n "$relayer_addr" ]] || fail "Relayer did not report a Solana address"
solana airdrop 100 "$relayer_addr" --url http://127.0.0.1:8899 >/dev/null \
  || fail "failed to fund Relayer $relayer_addr"
printf '%s' "$relayer_json" | grep -q '"system_disabled":false' \
  || fail "Relayer is system_disabled; RPC from Docker could not reach the validator"

cd zama
npx hardhat node --hostname 127.0.0.1 --port 8545 >/tmp/ctd-hardhat.log 2>&1 &
started_pids+=("$!")
cd "$repo_root"
for _ in $(seq 1 60); do
  curl -sf -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' http://127.0.0.1:8545 >/dev/null 2>&1 && break
  sleep 1
done
curl -sf -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' http://127.0.0.1:8545 >/dev/null \
  || fail "hardhat node did not start"
(cd zama && ZAMA_CAPACITY=200000000000 npx hardhat run scripts/deploy.ts --network localhost | tee /tmp/ctd-zama-deploy.log)
grep -q 'ZAMA_ENGINE ' /tmp/ctd-zama-deploy.log || fail "Zama deploy did not print ZAMA_ENGINE"
grep -q 'ZAMA_CLIENT ' /tmp/ctd-zama-deploy.log || fail "Zama deploy did not print ZAMA_CLIENT"

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

python3 - <<'PY' > /tmp/ctd-live-isolation-input.json
print('{"lockId":"unused","amount":"100000.000000","digestHex":"unused","payoutDestination":"unused"}')
PY
echo "BRIDGE_LIVE_ISOLATION"
dpm script --dar daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar \
  --script-name Tests.Bridge.Runtime:prepare \
  --participant-config "$run_dir/participants.json" \
  --input-file /tmp/ctd-live-isolation-input.json \
  --wall-clock-time > /tmp/ctd-live-prepare.log 2>&1 \
  || { tail -40 /tmp/ctd-live-prepare.log >&2; fail "live isolation prepare failed"; }
REASSIGNMENT_CAPABILITY=granted java -jar "$canton_jar" run canton/scripts/reassignment-capability.canton \
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
  > "$run_dir/isolation-capability-grant.log" 2>&1 \
  || { tail -40 "$run_dir/isolation-capability-grant.log" >&2; fail "isolation reassignment grant failed"; }
java -jar "$canton_jar" run canton/scripts/prepare-isolation-holdings.canton \
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
  > "$run_dir/isolation-holdings.log" 2>&1 \
  || { tail -40 "$run_dir/isolation-holdings.log" >&2; fail "isolation treasury holdings were not issued and reassigned"; }
grep -q ISO_HOLDINGS_READY "$run_dir/isolation-holdings.log" \
  || fail "isolation holdings script did not finish"
iso_hold_a="$(awk '/ISO_HOLDING_A /{print $2}' "$run_dir/isolation-holdings.log" | tail -1)"
iso_hold_b="$(awk '/ISO_HOLDING_B /{print $2}' "$run_dir/isolation-holdings.log" | tail -1)"
[[ -n "$iso_hold_a" && -n "$iso_hold_b" && "$iso_hold_a" != "$iso_hold_b" ]] \
  || fail "isolation holdings were not distinct: $iso_hold_a $iso_hold_b"
REASSIGNMENT_CAPABILITY=revoked java -jar "$canton_jar" run canton/scripts/reassignment-capability.canton \
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
  > "$run_dir/isolation-capability-revoke.log" 2>&1 \
  || { tail -40 "$run_dir/isolation-capability-revoke.log" >&2; fail "isolation reassignment revoke failed"; }
dpm script --dar daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar \
  --script-name Tests.Bridge.LiveIsolation:twoLiveOperations \
  --participant-config "$run_dir/participants.json" \
  --input-file /tmp/ctd-live-isolation-input.json \
  --wall-clock-time > /tmp/ctd-live-isolation.log 2>&1 \
  || { tail -40 /tmp/ctd-live-isolation.log >&2; fail "live isolation of two identical-term operations failed"; }
grep -q LIVE_ISOLATION_OK /tmp/ctd-live-isolation.log \
  || fail "live isolation did not print LIVE_ISOLATION_OK"
grep -q 'LIVE_ISOLATION_BROAD_LOOKUP 2' /tmp/ctd-live-isolation.log \
  || fail "live isolation did not prove the broad party-and-amount lookup"
daml_marker() {
  local file="$1" key="$2"
  sed -n "s/.*${key} \\([0-9a-fA-F]*\\).*/\\1/p" "$file" | tail -1
}
iso_a="$(daml_marker /tmp/ctd-live-isolation.log LIVE_ISOLATION_BINDING_A)"
iso_b="$(daml_marker /tmp/ctd-live-isolation.log LIVE_ISOLATION_BINDING_B)"
[[ -n "$iso_a" && -n "$iso_b" && "$iso_a" != "$iso_b" ]] \
  || fail "live isolation did not bind A and B to different trades: $iso_a $iso_b"
iso_mint_a="$(daml_marker /tmp/ctd-live-isolation.log LIVE_ISOLATION_MINT_A)"
iso_mint_b="$(daml_marker /tmp/ctd-live-isolation.log LIVE_ISOLATION_MINT_B)"
[[ -n "$iso_mint_a" && -n "$iso_mint_b" && "$iso_mint_a" != "$iso_mint_b" ]] \
  || fail "live isolation did not keep distinct mint holdings"
reserved_hold="$(awk -F= '/^reserved=/{print $2}' "$run_dir/isolation-holdings.txt")"
used_holds="$(sed -n 's/.*CANTON_USED_TREASURY_HOLDING \([0-9a-fA-F]*\).*/\1/p' /tmp/ctd-live-isolation.log | sort -u)"
printf '%s\n' "$used_holds" | grep -qx "$iso_hold_a" || fail "live isolation did not use reassigned holding A"
printf '%s\n' "$used_holds" | grep -qx "$iso_hold_b" || fail "live isolation did not use reassigned holding B"
printf '%s\n' "$used_holds" | grep -qx "$reserved_hold" && fail "live isolation consumed the reserved origination holding"

echo "RELAYER_PROOF_REQUIRED confidential PDA release through Relayer 1.5.0"
expiry_dir="$run_dir/bridge-expiry"
journal_dir="$run_dir/bridge-op"
rm -rf "$expiry_dir" "$journal_dir"
: > /tmp/ctd-workflow.log
workflow_env=(
  SOLANA_RPC_URL=http://127.0.0.1:8899
  RELAYER_URL=http://127.0.0.1:18080
  RELAYER_API_KEY="$RELAYER_API_KEY"
  RELAYER_ID=solana-local
  ZAMA_RPC_URL=http://127.0.0.1:8545
  ZAMA_ENGINE="$(awk '/ZAMA_ENGINE /{print $2}' /tmp/ctd-zama-deploy.log | tail -1)"
  ZAMA_CLIENT="$(awk '/ZAMA_CLIENT /{print $2}' /tmp/ctd-zama-deploy.log | tail -1)"
  ZAMA_REQUESTER_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
  ZAMA_SETTLER_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
  CANTON_PARTICIPANTS="$run_dir/participants.json"
  CANTON_RUN_DIR="$run_dir"
  CANTON_JAR="$canton_jar"
  BRIDGE_AMOUNT=100000
)
run_workflow() {
  local extra=("$@")
  set +e
  env "${workflow_env[@]}" cargo run --manifest-path bridge/Cargo.toml --quiet -- workflow "${extra[@]}" | tee -a /tmp/ctd-workflow.log
  local status=${PIPESTATUS[0]}
  set -e
  if [[ "$status" -ne 0 ]]; then
    fail "bridge workflow failed at ${extra[*]:-complete}; last sizes: $(grep TX_SIZE /tmp/ctd-workflow.log || true)"
  fi
}

echo "BRIDGE_EXPIRY_RECOVERY"
BRIDGE_MINT_EXPIRY_SECS=20 BRIDGE_JOURNAL_DIR="$expiry_dir" \
  run_workflow --journal "$expiry_dir" --resume --expiry-recovery
grep -q EXPIRY_RECOVERY_MINT_REJECTED /tmp/ctd-workflow.log \
  || fail "mint approval was not rejected after the original deadline"
grep -q EXPIRY_RECOVERY_CANCEL_CONFIRMED /tmp/ctd-workflow.log \
  || fail "expiry recovery did not cancel through Relayer"
grep -q EXPIRY_RECOVERY_COMPLETE /tmp/ctd-workflow.log \
  || fail "expiry recovery did not complete"
grep -q "FAULT_INJECTED expiry_before_settlement" /tmp/ctd-workflow.log \
  || fail "expiry-before-settlement fault was not recorded"
grep -q "RECOVERY_RESULT cancelled" /tmp/ctd-workflow.log \
  || fail "expiry recovery did not record a cancelled recovery result"
grep -q "RECOVERY_DURATION_CHAIN_SECS" /tmp/ctd-workflow.log \
  || fail "expiry recovery did not record chain-time duration"

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
if journal.get("lock_signature"):
    raise SystemExit("rejected reservation produced a Solana lock")
if journal.get("mint_holding"):
    raise SystemExit("rejected reservation produced a Canton mint")
if journal.get("lock_proof_hex"):
    raise SystemExit("rejected reservation stored a lock proof")
completed = journal.get("completed")
if completed not in (None, "accounts"):
    raise SystemExit(f"rejected reservation advanced to {completed}")
print("REJECT_JOURNAL_CLEAN")
PY
}

last_marker() {
  grep "$1" /tmp/ctd-workflow.log | tail -1
}

solana_chain_unix() {
  python3 - <<'PY'
import base64, json, struct, urllib.request
req = urllib.request.Request(
    "http://127.0.0.1:8899",
    data=json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [
            "SysvarC1ock11111111111111111111111111111111",
            {"encoding": "base64"},
        ],
    }).encode(),
    headers={"content-type": "application/json"},
)
acc = json.load(urllib.request.urlopen(req))["result"]["value"]["data"][0]
print(struct.unpack_from("<q", base64.b64decode(acc), 32)[0])
PY
}

wait_chain_clock_past() {
  local expiry="$1"
  echo "BRIDGE_WAIT_CHAIN_CLOCK_PAST $expiry"
  local now
  for _ in $(seq 1 120); do
    now="$(solana_chain_unix)"
    echo "CHAIN_CLOCK $now RELEASE_ONCHAIN_EXPIRY $expiry"
    if [[ "$now" -ge "$expiry" ]]; then
      echo "BRIDGE_RELEASE_APPROVAL_EXPIRED_ON_CHAIN"
      return 0
    fi
    sleep 1
  done
  fail "Solana chain clock $now did not reach expiry $expiry"
}

reject_dir="$run_dir/bridge-reject"
rm -rf "$reject_dir"
echo "BRIDGE_REJECT_OVER_CAPACITY"
set +e
env "${workflow_env[@]}" BRIDGE_AMOUNT=300000 BRIDGE_JOURNAL_DIR="$reject_dir" \
  cargo run --manifest-path bridge/Cargo.toml --quiet -- workflow \
  --journal "$reject_dir" --reuse-from "$expiry_dir" --resume --stop-after reserved \
  | tee -a /tmp/ctd-workflow.log
reject_status=${PIPESTATUS[0]}
set -e
[[ "$reject_status" -ne 0 ]] || fail "over-capacity reservation should be rejected"
grep -q ZAMA_RESERVATION_REJECTED /tmp/ctd-workflow.log \
  || fail "over-capacity rejection was not recorded"
assert_no_lock_or_mint "$reject_dir"
if grep -q CANTON_MINT_HOLDING /tmp/ctd-workflow.log; then
  fail "Canton mint occurred during over-capacity reject"
fi

echo "BRIDGE_REJECT_RETRY"
set +e
env "${workflow_env[@]}" BRIDGE_AMOUNT=300000 BRIDGE_JOURNAL_DIR="$reject_dir" \
  cargo run --manifest-path bridge/Cargo.toml --quiet -- workflow \
  --journal "$reject_dir" --resume --stop-after locked \
  | tee -a /tmp/ctd-workflow.log
retry_status=${PIPESTATUS[0]}
set -e
[[ "$retry_status" -ne 0 ]] || fail "retry of a rejected reservation must fail"
[[ "$(grep -c ZAMA_RESERVATION_REJECTED /tmp/ctd-workflow.log)" -ge 2 ]] \
  || fail "rejected reservation was not re-checked on retry"
assert_no_lock_or_mint "$reject_dir"
if grep -q RELAYER_RELEASE_CONFIRMED /tmp/ctd-workflow.log; then
  fail "release ran during rejected reservation"
fi
if grep -q CANTON_MINT_HOLDING /tmp/ctd-workflow.log; then
  fail "Canton mint occurred on rejected reservation retry"
fi

zama_rpc() {
  local method="$1"
  local args="$2"
  (cd "$repo_root/zama" && env \
    ZAMA_RPC_URL=http://127.0.0.1:8545 \
    ZAMA_ENGINE="$(awk '/ZAMA_ENGINE /{print $2}' /tmp/ctd-zama-deploy.log | tail -1)" \
    ZAMA_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
    ZAMA_METHOD="$method" \
    ZAMA_ARGS="$args" \
    npx hardhat run scripts/bridge-rpc.ts --network localhost)
}

zama_result_line() {
  zama_rpc "$1" "$2" | awk '/^ZAMA_RESULT /{line=$0} END{print line}'
}

reject_reservation="$(python3 -c "import json; print(json.load(open('$reject_dir/journal.json'))['reservation_hex'])")"
echo "BRIDGE_REJECT_FINALIZE"
zama_rpc finalize "$reject_reservation" >/tmp/ctd-zama-finalize.log
grep -q ZAMA_RESULT /tmp/ctd-zama-finalize.log || fail "finalize of the rejected reservation did not return"
[[ "$(zama_result_line status "$reject_reservation")" == *'"status":2'* ]] \
  || fail "rejected reservation was not finalized: $(zama_result_line status "$reject_reservation")"
[[ "$(zama_result_line approved "$reject_reservation")" == *'"approved":false'* ]] \
  || fail "finalized rejected reservation must stay unapproved: $(zama_result_line approved "$reject_reservation")"

echo "BRIDGE_REJECT_FINALIZED_RESUME"
set +e
env "${workflow_env[@]}" BRIDGE_AMOUNT=300000 BRIDGE_JOURNAL_DIR="$reject_dir" \
  cargo run --manifest-path bridge/Cargo.toml --quiet -- workflow \
  --journal "$reject_dir" --resume --stop-after locked \
  | tee -a /tmp/ctd-workflow.log
finalized_retry=${PIPESTATUS[0]}
set -e
[[ "$finalized_retry" -ne 0 ]] || fail "finalized rejected reservation must not resume into lock"
[[ "$(grep -c ZAMA_RESERVATION_REJECTED /tmp/ctd-workflow.log)" -ge 3 ]] \
  || fail "finalized rejected reservation was not rejected on resume"
assert_no_lock_or_mint "$reject_dir"
if grep -q CANTON_MINT_HOLDING /tmp/ctd-workflow.log; then
  fail "Canton mint occurred after rejected reservation was finalized"
fi

echo "BRIDGE_RESUME_AFTER accounts"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after accounts
echo "BRIDGE_RESUME_AFTER reserved"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after reserved

zama_client="$(awk '/ZAMA_CLIENT /{print $2}' /tmp/ctd-zama-deploy.log | tail -1)"
probe_units=100000000000
fresh_id() { python3 -c "import os; print('0x'+os.urandom(32).hex())"; }
unrelated_id="$(fresh_id)"
live_probe_id="$(fresh_id)"
echo "BRIDGE_CAPACITY_PROBE_WHILE_LIVE"
unrelated_line="$(zama_result_line reserve "$unrelated_id,$zama_client,$probe_units")"
[[ "$unrelated_line" == *'"approved":true'* ]] \
  || fail "unrelated live reservation should be approved: $unrelated_line"
live_probe_line="$(zama_result_line reserve "$live_probe_id,$zama_client,$probe_units")"
[[ "$live_probe_line" == *'"approved":false'* ]] \
  || fail "same-sized probe must be rejected while original exposure is live: $live_probe_line"
zama_rpc cancel "$live_probe_id" >/tmp/ctd-zama-cancel-live-probe.log
[[ "$(zama_result_line approved "$unrelated_id")" == *'"approved":true'* ]] \
  || fail "unrelated live exposure was dropped while the original reservation is active"
[[ "$(zama_result_line status "$unrelated_id")" == *'"status":1'* ]] \
  || fail "unrelated live reservation did not stay reserved"

echo "BRIDGE_RESUME_AFTER locked"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after locked
assert_journal_step "$journal_dir" locked

echo "BRIDGE_ATTESTER_DISAGREEMENT"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --inject-attester-disagreement
assert_journal_step "$journal_dir" locked
grep -q "FAULT_INJECTED attester_disagreement" /tmp/ctd-workflow.log \
  || fail "attester disagreement was not injected"
grep -q "RECOVERY_WAIT_UNBOUNDED operator_or_quorum" /tmp/ctd-workflow.log \
  || fail "attester disagreement did not record that quorum wait is unbounded"
grep -q CHAIN_CLOCK /tmp/ctd-workflow.log \
  || fail "attester disagreement did not record chain time"
[[ "$(last_marker MINT_APPROVAL_BITMAP)" == "MINT_APPROVAL_BITMAP 1" ]] \
  || fail "conflicting attestation counted toward quorum: $(last_marker MINT_APPROVAL_BITMAP)"
[[ "$(last_marker RECEIPT_STATUS)" == "RECEIPT_STATUS 1" ]] \
  || fail "receipt must stay locked without 2-of-3: $(last_marker RECEIPT_STATUS)"
python3 - "$journal_dir/journal.json" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
if journal.get("mint_holding"):
    raise SystemExit("disagreement minted before quorum")
if journal.get("completed") != "locked":
    raise SystemExit(f"disagreement advanced to {journal.get('completed')}")
print("DISAGREEMENT_NO_MINT")
PY

echo "BRIDGE_SECOND_ATTESTATION_BEFORE_SAVE"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after mint_approved --omit-journal-save
assert_journal_step "$journal_dir" locked
grep -q "RECOVERY_DURATION_CHAIN_SECS" /tmp/ctd-workflow.log \
  || fail "attester disagreement recovery did not record chain duration"
grep -q "RECOVERY_WAIT_UNBOUNDED operator_or_quorum" /tmp/ctd-workflow.log \
  || fail "attester disagreement recovery did not state the wait is unbounded"
[[ "$(last_marker MINT_APPROVAL_BITMAP)" == "MINT_APPROVAL_BITMAP 3" ]] \
  || fail "missing attestation was not the only submit: $(last_marker MINT_APPROVAL_BITMAP)"
[[ "$(last_marker RECEIPT_STATUS)" == "RECEIPT_STATUS 2" ]] \
  || fail "receipt was not mint-authorized after both attestations: $(last_marker RECEIPT_STATUS)"

echo "BRIDGE_RECOGNISE_EXISTING_MINT_APPROVAL"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after mint_approved
assert_journal_step "$journal_dir" mint_approved
[[ "$(last_marker MINT_APPROVAL_BITMAP)" == "MINT_APPROVAL_BITMAP 3" ]] \
  || fail "resume did not recognise the existing 2-of-3: $(last_marker MINT_APPROVAL_BITMAP)"
[[ "$(last_marker RECEIPT_STATUS)" == "RECEIPT_STATUS 2" ]] \
  || fail "resume changed the mint-authorized receipt: $(last_marker RECEIPT_STATUS)"

for step in canton_minted trade_prepared reassigned; do
  echo "BRIDGE_RESUME_AFTER $step"
  BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after "$step"
done

echo "BRIDGE_SETTLE_BEFORE_SAVE"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after settled --omit-journal-save
assert_journal_step "$journal_dir" reassigned
grep -q DVP_BUYER_TREASURY /tmp/ctd-workflow.log \
  || fail "settlement did not land before the journal save"
settle_treasury="$(last_marker DVP_BUYER_TREASURY)"
settle_payment="$(last_marker DVP_SELLER_STABLECOIN)"

echo "BRIDGE_RESUME_AFTER_SETTLE_WITHOUT_SAVE"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after settled
assert_journal_step "$journal_dir" settled
[[ "$(last_marker DVP_BUYER_TREASURY)" == "$settle_treasury" ]] \
  || fail "resume after unsaved settlement changed the buyer Treasury holding"
[[ "$(last_marker DVP_SELLER_STABLECOIN)" == "$settle_payment" ]] \
  || fail "resume after unsaved settlement changed the seller stablecoin holding"

echo "BRIDGE_RESUME_AFTER redeemed"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after redeemed
assert_journal_step "$journal_dir" redeemed
grep -q "FAULT_INJECTED delayed_release_after_redemption" /tmp/ctd-workflow.log \
  || fail "delayed release fault was not recorded"
grep -q "LOCKED_STATE solana_vault" /tmp/ctd-workflow.log \
  || fail "delayed release did not record the locked state"
grep -q "RECOVERY_WAIT_UNBOUNDED operator_or_quorum" /tmp/ctd-workflow.log \
  || fail "delayed release did not record that operator resume wait is unbounded"
grep -q CHAIN_CLOCK /tmp/ctd-workflow.log \
  || fail "delayed release did not record chain time at inject"

echo "BRIDGE_RELEASE_APPROVAL"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_RELEASE_EXPIRY_SECS=15 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after release_approved
assert_journal_step "$journal_dir" release_approved
grep -q CHAIN_CLOCK /tmp/ctd-workflow.log \
  || fail "release approval did not read the Solana chain clock"
release_expiry="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['release_expiry'])")"
[[ "$release_expiry" -gt 0 ]] || fail "release expiry was not stored"

echo "BRIDGE_RELEASE_APPROVAL_EXPIRED"
wait_chain_clock_past "$release_expiry"

echo "BRIDGE_RELEASE_BEFORE_SAVE"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_RELEASE_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after released --omit-journal-save
assert_journal_step "$journal_dir" release_approved
grep -q RELEASE_REFRESHED_AFTER_CHAIN_EXPIRY /tmp/ctd-workflow.log \
  || fail "expired release approval was not replaced using Solana chain time"
grep -q "FAULT_INJECTED expiry_after_settlement" /tmp/ctd-workflow.log \
  || fail "expired release approval fault was not recorded"
grep -q RELAYER_RELEASE_CONFIRMED /tmp/ctd-workflow.log \
  || fail "confidential release through Relayer did not confirm"
grep -q "RECOVERY_DURATION_CHAIN_SECS" /tmp/ctd-workflow.log \
  || fail "delayed release recovery did not record chain duration"
[[ "$(last_marker RELEASE_APPROVAL_BITMAP)" == "RELEASE_APPROVAL_BITMAP 3" ]] \
  || fail "expired release approval was not replaced with a fresh 2-of-3: $(last_marker RELEASE_APPROVAL_BITMAP)"
[[ "$(last_marker RECEIPT_STATUS)" == "RECEIPT_STATUS 4" ]] \
  || fail "release did not land on-chain before the journal save: $(last_marker RECEIPT_STATUS)"
[[ "$(last_marker DEST_AVAILABLE)" == "DEST_AVAILABLE 100000000000" ]] \
  || fail "destination available after release/apply is wrong: $(last_marker DEST_AVAILABLE)"
[[ "$(last_marker DEST_PENDING)" == "DEST_PENDING 0" ]] \
  || fail "apply-pending did not settle destination credits before the journal save: $(last_marker DEST_PENDING)"

echo "BRIDGE_RESUME_AFTER_RELEASE_WITHOUT_SAVE"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after released
assert_journal_step "$journal_dir" released
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED /tmp/ctd-workflow.log)" -eq 1 ]] \
  || fail "release was submitted again after the receipt was already released"
[[ "$(last_marker RECEIPT_STATUS)" == "RECEIPT_STATUS 4" ]] \
  || fail "resume did not keep the released receipt: $(last_marker RECEIPT_STATUS)"
[[ "$(last_marker DEST_AVAILABLE)" == "DEST_AVAILABLE 100000000000" ]] \
  || fail "apply-pending ran twice or corrupted the destination balance: $(last_marker DEST_AVAILABLE)"
[[ "$(last_marker DEST_PENDING)" == "DEST_PENDING 0" ]] \
  || fail "destination pending credits were not settled: $(last_marker DEST_PENDING)"

echo "BRIDGE_ZAMA_REDEEM_BEFORE_SAVE"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed --omit-journal-save
assert_journal_step "$journal_dir" released
grep -q ZAMA_REDEEM_OK /tmp/ctd-workflow.log || fail "Zama redemption did not land before the journal save"
main_reservation="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['reservation_hex'])")"
[[ "$(zama_result_line status "$main_reservation")" == *'"status":4'* ]] \
  || fail "Zama redemption did not reach Redeemed: $(zama_result_line status "$main_reservation")"
[[ "$(zama_result_line approved "$main_reservation")" == *'"approved":true'* ]] \
  || fail "redeemed reservation lost its approval bit"

zama_block() {
  curl -sf -X POST -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
    http://127.0.0.1:8545 | python3 -c "import json,sys; print(int(json.load(sys.stdin)['result'],16))"
}

echo "BRIDGE_RESUME_AFTER_ZAMA_REDEEM_WITHOUT_SAVE"
tx_size_before_complete="$(grep -c TX_SIZE /tmp/ctd-workflow.log || true)"
zama_before_complete="$(zama_block)"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
assert_journal_step "$journal_dir" zama_redeemed
grep -q OPERATION_RECORDED_COMPLETE /tmp/ctd-workflow.log \
  || fail "resume did not record the completed operation after Zama redemption"
grep -q COMPLETED_RESUME_SKIP_SETUP /tmp/ctd-workflow.log \
  || fail "completed resume did not skip setup and funding"
grep -q CANTON_VERIFY_OK /tmp/ctd-workflow.log \
  || fail "completed resume did not verify Canton from ledger history"
[[ "$(grep -c ZAMA_REDEEM_OK /tmp/ctd-workflow.log)" -eq 1 ]] \
  || fail "Zama redemption was submitted again after it had already landed"
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED /tmp/ctd-workflow.log)" -eq 1 ]] \
  || fail "release was submitted again while recording completion"
[[ "$(grep -c TX_SIZE /tmp/ctd-workflow.log || true)" == "$tx_size_before_complete" ]] \
  || fail "completed resume submitted another Solana transaction"
[[ "$(last_marker DEST_AVAILABLE)" == "DEST_AVAILABLE 100000000000" ]] \
  || fail "destination balance changed while recording completion: $(last_marker DEST_AVAILABLE)"
[[ "$(last_marker RECEIPT_STATUS)" == "RECEIPT_STATUS 4" ]] \
  || fail "receipt changed while recording completion: $(last_marker RECEIPT_STATUS)"
[[ "$(zama_block)" == "$zama_before_complete" ]] \
  || fail "completed resume submitted another Zama transaction"

echo "BRIDGE_LEDGER_EVIDENCE_NEGATIVE"
lock_id="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['lock_id'])")"
mint_holding="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['mint_holding'])")"
payout="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['payout_destination'])")"
canton_amount="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['canton_amount'])")"
set +e
env CANTON_RUN_DIR="$run_dir" CANTON_JAR="$canton_jar" \
  BRIDGE_LOCK_ID="ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff" \
  BRIDGE_CANTON_AMOUNT="$canton_amount" \
  BRIDGE_TREASURY_AMOUNT=100.000000 \
  BRIDGE_PAYOUT_DEST="$payout" \
  BRIDGE_MINT_HOLDING="$mint_holding" \
  java -jar "$canton_jar" run canton/scripts/verify-bridge-completion.canton \
    -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
    > /tmp/ctd-canton-other-lock.log 2>&1
other_lock_status=$?
set -e
[[ "$other_lock_status" -ne 0 ]] || fail "ledger verify accepted another operation lock"
if ! grep -qE 'CANTON_VERIFY_FAIL|CANTON_HISTORY_OTHER_LOCK|another operation' /tmp/ctd-canton-other-lock.log; then
  fail "other-operation verify did not fail closed: $(tail -20 /tmp/ctd-canton-other-lock.log)"
fi
set +e
env CANTON_RUN_DIR="$run_dir" CANTON_JAR="$canton_jar" \
  BRIDGE_LOCK_ID="$lock_id" \
  BRIDGE_CANTON_AMOUNT="$canton_amount" \
  BRIDGE_TREASURY_AMOUNT=100.000000 \
  BRIDGE_PAYOUT_DEST="$payout" \
  BRIDGE_MINT_HOLDING="$mint_holding" \
  java -jar "$canton_jar" run canton/scripts/does-not-exist.canton \
    -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
    > /tmp/ctd-canton-unreadable.log 2>&1
unreadable_status=$?
set -e
[[ "$unreadable_status" -ne 0 ]] || fail "unreadable ledger verify was treated as success"
if grep -q CANTON_VERIFY_OK /tmp/ctd-canton-unreadable.log; then
  fail "unreadable ledger verify printed success"
fi
python3 - "$journal_dir/journal.json" <<'PY' > /tmp/ctd-canton-missing-input.json
import json, sys
journal = json.load(open(sys.argv[1]))
print(json.dumps({
    "lockId": "missing-lock",
    "amount": journal["canton_amount"],
    "digestHex": "",
    "payoutDestination": journal["payout_destination"],
}))
PY
set +e
dpm script --dar daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar \
  --script-name Tests.Bridge.Runtime:verifyCompletion \
  --participant-config "$run_dir/participants.json" \
  --input-file /tmp/ctd-canton-missing-input.json \
  --wall-clock-time > /tmp/ctd-canton-missing.log 2>&1
missing_status=$?
set -e
[[ "$missing_status" -ne 0 ]] || fail "missing ledger evidence was treated as completion"
if grep -q CANTON_ACS_OK /tmp/ctd-canton-missing.log; then
  fail "missing lock printed ACS success"
fi

echo "BRIDGE_RESUME_COMPLETED_AGAIN"
tx_size_before_second="$(grep -c TX_SIZE /tmp/ctd-workflow.log || true)"
zama_before_second="$(zama_block)"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
assert_journal_step "$journal_dir" zama_redeemed
[[ "$(grep -c OPERATION_RECORDED_COMPLETE /tmp/ctd-workflow.log)" -ge 2 ]] \
  || fail "second resume of a completed operation did not succeed"
[[ "$(grep -c COMPLETED_RESUME_SKIP_SETUP /tmp/ctd-workflow.log)" -ge 2 ]] \
  || fail "second completed resume did not skip setup and funding"
[[ "$(grep -c CANTON_VERIFY_OK /tmp/ctd-workflow.log)" -ge 2 ]] \
  || fail "second completed resume did not verify Canton from ledger history"
[[ "$(grep -c TX_SIZE /tmp/ctd-workflow.log || true)" == "$tx_size_before_second" ]] \
  || fail "second completed resume submitted another Solana transaction"
[[ "$(zama_block)" == "$zama_before_second" ]] \
  || fail "second completed resume submitted another Zama transaction"
[[ "$(grep -c ZAMA_REDEEM_OK /tmp/ctd-workflow.log)" -eq 1 ]] \
  || fail "second resume changed Zama redemption"
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED /tmp/ctd-workflow.log)" -eq 1 ]] \
  || fail "second resume changed Solana release"
[[ "$(last_marker DEST_AVAILABLE)" == "DEST_AVAILABLE 100000000000" ]] \
  || fail "second resume changed destination balance: $(last_marker DEST_AVAILABLE)"
[[ "$(last_marker DEST_PENDING)" == "DEST_PENDING 0" ]] \
  || fail "second resume changed destination pending credits: $(last_marker DEST_PENDING)"
[[ "$(zama_result_line status "$main_reservation")" == *'"status":4'* ]] \
  || fail "second resume changed Zama status"
[[ "$(zama_result_line approved "$reject_reservation")" == *'"approved":false'* ]] \
  || fail "rejected reservation approval changed during recovery"

grep -q CANTON_MINT_HOLDING /tmp/ctd-workflow.log || fail "Canton mint holding was not recorded"
grep -q DVP_BUYER_TREASURY /tmp/ctd-workflow.log || fail "buyer did not receive Treasury"
grep -q DVP_SELLER_STABLECOIN /tmp/ctd-workflow.log || fail "seller did not receive stablecoins"
grep -q DVP_PAYMENT_AMOUNT /tmp/ctd-workflow.log || fail "DvP payment amount was not asserted"
grep -q CANTON_REDEEM /tmp/ctd-workflow.log || fail "seller redemption was not recorded"
grep -q RELAYER_RELEASE_CONFIRMED /tmp/ctd-workflow.log || fail "confidential release through Relayer did not confirm"
grep -q ZAMA_REDEEM_OK /tmp/ctd-workflow.log || fail "Zama redemption did not succeed"
grep -q EXPIRY_RECOVERY_COMPLETE /tmp/ctd-workflow.log || fail "expiry recovery evidence is missing"

python3 - "$journal_dir/journal.json" /tmp/ctd-workflow.log <<'PY'
import json, re, sys
journal = json.load(open(sys.argv[1]))
log = open(sys.argv[2]).read()
mint = re.findall(r"CANTON_MINT_HOLDING (\S+)", log)
consumed = re.findall(r"DVP_CONSUMED_PAYMENT (\S+)", log)
if not mint or not consumed or mint[-1] != consumed[-1]:
    raise SystemExit(f"bridged holding {mint[-1:]!r} did not fund DvP {consumed[-1:]!r}")
if "DVP_TREASURY_AMOUNT 100" not in log and "DVP_TREASURY_AMOUNT 100.0" not in log:
    raise SystemExit("buyer Treasury amount was not 100")
if "DVP_PAYMENT_AMOUNT 100000" not in log:
    raise SystemExit("seller stablecoin amount was not 100000")
if journal.get("completed") != "zama_redeemed":
    raise SystemExit(f"journal completed is {journal.get('completed')}")
if journal.get("base_units") != 100_000_000_000:
    raise SystemExit(f"journal base units are {journal.get('base_units')}")
verified = re.findall(r"CANTON_VERIFY_MINT_CONSUMED (\S+)", log)
if not verified or verified[-1] != consumed[-1] or verified[-1] != journal.get("mint_holding"):
    raise SystemExit(f"ledger verify did not consume recorded holding {journal.get('mint_holding')!r}: {verified!r}")
allocation = re.findall(r"CANTON_VERIFY_PAYMENT_ALLOCATION (\S+)", log)
locked = re.findall(r"CANTON_VERIFY_PAYMENT_LOCKED (\S+)", log)
allocate_upd = re.findall(r"CANTON_VERIFY_ALLOCATE_UPDATE (\S+)", log)
settle_upd = re.findall(r"CANTON_VERIFY_SETTLE_UPDATE (\S+)", log)
redeem_upd = re.findall(r"CANTON_VERIFY_REDEEM_UPDATE (\S+)", log)
treasury = re.findall(r"CANTON_VERIFY_BUYER_TREASURY (\S+)", log)
payment = re.findall(r"CANTON_VERIFY_SELLER_PAYMENT (\S+)", log)
instrument = re.findall(r"CANTON_VERIFY_INSTRUMENT (\S+)", log)
payment_admin = re.findall(r"CANTON_VERIFY_PAYMENT_ADMIN (\S+)", log)
treasury_instrument = re.findall(r"CANTON_VERIFY_TREASURY_INSTRUMENT (\S+)", log)
treasury_admin = re.findall(r"CANTON_VERIFY_TREASURY_ADMIN (\S+)", log)
if not allocation or not locked or allocation[-1] == verified[-1]:
    raise SystemExit(f"payment allocation was not traced from the minted holding: {allocation!r} {locked!r}")
if not allocate_upd or not settle_upd or allocate_upd[-1] == settle_upd[-1]:
    raise SystemExit(f"allocation and settlement must be distinct connected updates: {allocate_upd!r} {settle_upd!r}")
if not redeem_upd or not treasury or not payment:
    raise SystemExit("connected settlement or redemption IDs are missing")
if not instrument or instrument[-1] != "USD-C" or not treasury_instrument or treasury_instrument[-1] != "UST-2028-11":
    raise SystemExit(f"connected instruments were not extracted: {instrument!r} {treasury_instrument!r}")
if not payment_admin or not treasury_admin or "::" not in payment_admin[-1] or "::" not in treasury_admin[-1]:
    raise SystemExit(f"connected instrument admins were not extracted: {payment_admin!r} {treasury_admin!r}")
if log.count("CANTON_VERIFY_OK") < 2:
    raise SystemExit("completed resumes did not verify Canton from ledger history twice")
binding = re.findall(r"CANTON_VERIFY_BINDING_TRADE (\S+)", log)
if not binding:
    raise SystemExit("completed resume did not verify the lock-mint-trade binding")
print("CONNECTED_BINDING_TRADE " + binding[-1])
print("CONNECTED_MINT_HOLDING " + verified[-1])
print("CONNECTED_PAYMENT_ALLOCATION " + allocation[-1])
print("CONNECTED_PAYMENT_LOCKED " + locked[-1])
print("CONNECTED_ALLOCATE_UPDATE " + allocate_upd[-1])
print("CONNECTED_SETTLE_UPDATE " + settle_upd[-1])
print("CONNECTED_BUYER_TREASURY " + treasury[-1])
print("CONNECTED_SELLER_PAYMENT " + payment[-1])
print("CONNECTED_REDEEM_UPDATE " + redeem_upd[-1])
print("LEDGER_HOLDING_FUNDED_DVP " + consumed[-1])
print("LEDGER_JOURNAL_COMPLETE")
PY
[[ "$(last_marker DEST_AVAILABLE)" == "DEST_AVAILABLE 100000000000" ]] \
  || fail "intended destination did not receive 100000000000 base units"
[[ "$(zama_result_line status "$main_reservation")" == *'"status":4'* ]] \
  || fail "main reservation exposure was not redeemed"
[[ "$(zama_result_line approved "$main_reservation")" == *'"approved":true'* ]] \
  || fail "main reservation approval is missing"
[[ "$(zama_result_line approved "$reject_reservation")" == *'"approved":false'* ]] \
  || fail "rejected reservation must stay unapproved"
echo "BRIDGE_CAPACITY_PROBE_AFTER_REDEEM"
after_probe_id="$(fresh_id)"
after_probe_line="$(zama_result_line reserve "$after_probe_id,$zama_client,$probe_units")"
[[ "$after_probe_line" == *'"approved":true'* ]] \
  || fail "same-sized probe must be approved after redemption: $after_probe_line"
leftover_probe_id="$(fresh_id)"
leftover_probe_line="$(zama_result_line reserve "$leftover_probe_id,$zama_client,$probe_units")"
[[ "$leftover_probe_line" == *'"approved":false'* ]] \
  || fail "unrelated live exposure should still reject another same-sized probe: $leftover_probe_line"
zama_rpc cancel "$after_probe_id" >/tmp/ctd-zama-cancel-after-probe.log
zama_rpc cancel "$leftover_probe_id" >/tmp/ctd-zama-cancel-leftover-probe.log
[[ "$(zama_result_line approved "$unrelated_id")" == *'"approved":true'* ]] \
  || fail "unrelated live exposure was dropped after redemption"
[[ "$(zama_result_line status "$unrelated_id")" == *'"status":1'* ]] \
  || fail "unrelated live reservation did not remain counted"
echo "LEDGER_DEST_AVAILABLE 100000000000"
echo "LEDGER_ZAMA_REDEEMED true"
echo "LEDGER_REJECT_UNAPPROVED true"
echo "LEDGER_CAPACITY_RECOVERED true"
echo "BRIDGE_E2E_COMPLETE"
