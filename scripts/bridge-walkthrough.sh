#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
# shellcheck source=bridge-local-stack.sh
source "$repo_root/scripts/bridge-local-stack.sh"

dpm_home="${DPM_HOME:-$HOME/.dpm}"
run_dir="$repo_root/canton/.run-walkthrough"

trap cleanup_bridge_stack EXIT
init_bridge_stack
log="$tmp_dir/ctd-walkthrough.log"
test -d zama/node_modules || fail "zama dependencies are not installed; run: (cd zama && npm ci)"
test -f daml/bridge-gateway/.daml/dist/bridge-gateway-0.1.0.dar || fail "missing bridge-gateway DAR; run: make build"
test -f daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar || fail "missing bridge-tests DAR; run: make build"
canton_jar="$(ls "$dpm_home"/cache/components/canton-open-source/*/lib/canton-open-source-*.jar 2>/dev/null | sort -V | tail -1)"
[[ -n "$canton_jar" ]] || fail "canton runtime not found under $dpm_home/cache/components/canton-open-source"
require_bridge_ports_free

(cd solana && anchor build)
test -f solana/target/deploy/confidential_escrow.so || fail "missing confidential_escrow.so"
"$repo_root/scripts/build-token-2022-zk-ops.sh"
token_2022_so="$repo_root/solana/target/deploy/spl_token_2022_zk_ops.so"
test -f "$token_2022_so" || fail "missing Token-2022 zk-ops program"
require_devnet_matching_validator

ledger_dir="$(mktemp -d "${TMPDIR:-/tmp}/ctd-walk-solana-XXXXXX")"
"$(agave_devnet_validator)" --reset --quiet --ledger "$ledger_dir" --rpc-port 8899 --faucet-port 9900 \
  --bpf-program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb "$token_2022_so" \
  --bpf-program "$RECORD_PROGRAM_ID" "$(record_so)" \
  --bpf-program "$ESCROW_PROGRAM_ID" "$(escrow_so)" >"$tmp_dir/ctd-walk-solana.log" 2>&1 &
started_pids+=("$!")
for _ in $(seq 1 60); do
  solana cluster-version --url http://127.0.0.1:8899 >/dev/null 2>&1 && break
  sleep 1
done
solana cluster-version --url http://127.0.0.1:8899 >/dev/null || fail "solana-test-validator did not start"
solana airdrop 100 --url http://127.0.0.1:8899 >/dev/null
solana account ZkE1Gama1Proof11111111111111111111111111111 --url http://127.0.0.1:8899 >/dev/null \
  || fail "zk-elgamal-proof program is missing on the local validator"
solana account "$RECORD_PROGRAM_ID" --url http://127.0.0.1:8899 >/dev/null \
  || fail "official Record program is missing on the local validator"
require_escrow_loaded

prepare_local_relayer_runtime
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
  || fail "Relayer is system_disabled"

cd zama
npx hardhat node --hostname 127.0.0.1 --port 8545 >"$tmp_dir/ctd-walk-hardhat.log" 2>&1 &
started_pids+=("$!")
cd "$repo_root"
for _ in $(seq 1 60); do
  curl -sf -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' http://127.0.0.1:8545 >/dev/null 2>&1 && break
  sleep 1
done
curl -sf -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' http://127.0.0.1:8545 >/dev/null \
  || fail "hardhat node did not start"
(cd zama && ZAMA_CAPACITY=200000000000 npx hardhat run scripts/deploy.ts --network localhost | tee "$tmp_dir/ctd-walk-zama-deploy.log")
grep -q 'ZAMA_ENGINE ' "$tmp_dir/ctd-walk-zama-deploy.log" || fail "Zama deploy did not print ZAMA_ENGINE"

rm -rf "$run_dir"
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
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN >"$run_dir/bridge-bootstrap.log" 2>&1 \
  || { tail -40 "$run_dir/bridge-bootstrap.log" >&2; fail "bridge bootstrap failed"; }
java -jar "$canton_jar" run canton/scripts/origination.canton \
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN >"$run_dir/origination.log" 2>&1 \
  || { tail -40 "$run_dir/origination.log" >&2; fail "treasury origination failed"; }

python3 - <<'PY' > "$tmp_dir/ctd-walk-input.json"
print('{"lockId":"unused","amount":"100000.000000","digestHex":"unused","payoutDestination":"unused"}')
PY
echo "WALKTHROUGH_TWO_IDENTICAL_OPERATIONS"
dpm script --dar daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar \
  --script-name Tests.Bridge.Runtime:prepare \
  --participant-config "$run_dir/participants.json" \
  --input-file "$tmp_dir/ctd-walk-input.json" \
  --wall-clock-time > "$tmp_dir/ctd-walk-prepare.log" 2>&1 \
  || fail "walkthrough prepare failed"
REASSIGNMENT_CAPABILITY=granted java -jar "$canton_jar" run canton/scripts/reassignment-capability.canton \
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
  > "$run_dir/isolation-capability-grant.log" 2>&1 \
  || fail "walkthrough reassignment grant failed"
java -jar "$canton_jar" run canton/scripts/prepare-isolation-holdings.canton \
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
  > "$run_dir/isolation-holdings.log" 2>&1 \
  || fail "walkthrough isolation holdings failed"
REASSIGNMENT_CAPABILITY=revoked java -jar "$canton_jar" run canton/scripts/reassignment-capability.canton \
  -c canton/remote-console.conf --no-tty --log-level-stdout WARN \
  > "$run_dir/isolation-capability-revoke.log" 2>&1 \
  || fail "walkthrough reassignment revoke failed"
dpm script --dar daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar \
  --script-name Tests.Bridge.LiveIsolation:twoLiveOperations \
  --participant-config "$run_dir/participants.json" \
  --input-file "$tmp_dir/ctd-walk-input.json" \
  --wall-clock-time > "$tmp_dir/ctd-walk-isolation.log" 2>&1 \
  || { tail -40 "$tmp_dir/ctd-walk-isolation.log" >&2; fail "walkthrough two identical-term operations failed"; }
grep -q LIVE_ISOLATION_OK "$tmp_dir/ctd-walk-isolation.log" || fail "two identical-term operations did not finish"
iso_a="$(sed -n 's/.*LIVE_ISOLATION_BINDING_A \([0-9a-fA-F]*\).*/\1/p' "$tmp_dir/ctd-walk-isolation.log" | tail -1)"
iso_b="$(sed -n 's/.*LIVE_ISOLATION_BINDING_B \([0-9a-fA-F]*\).*/\1/p' "$tmp_dir/ctd-walk-isolation.log" | tail -1)"
[[ -n "$iso_a" && -n "$iso_b" && "$iso_a" != "$iso_b" ]] || fail "walkthrough bindings were not distinct"
echo "WALKTHROUGH_CHECKED two_identical_operations $iso_a $iso_b"

: > "$log"
workflow_env=(
  SOLANA_RPC_URL=http://127.0.0.1:8899
  RELAYER_URL=http://127.0.0.1:18080
  RELAYER_API_KEY="$RELAYER_API_KEY"
  RELAYER_ID=solana-local
  ZAMA_RPC_URL=http://127.0.0.1:8545
  ZAMA_ENGINE="$(awk '/ZAMA_ENGINE /{print $2}' "$tmp_dir/ctd-walk-zama-deploy.log" | tail -1)"
  ZAMA_CLIENT="$(awk '/ZAMA_CLIENT /{print $2}' "$tmp_dir/ctd-walk-zama-deploy.log" | tail -1)"
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
  env "${workflow_env[@]}" cargo run --manifest-path bridge/Cargo.toml --quiet -- workflow "${extra[@]}" | tee -a "$log"
  local status=${PIPESTATUS[0]}
  set -e
  if [[ "$status" -ne 0 ]]; then
    fail "walkthrough workflow failed at ${extra[*]:-complete}"
  fi
}
assert_journal_step() {
  python3 - "$1/journal.json" "$2" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
actual = journal.get("completed")
expected = sys.argv[2]
if actual != expected:
    raise SystemExit(f"journal completed is {actual!r}, expected {expected!r}")
PY
}
last_marker() { grep "$1" "$log" | tail -1; }
solana_chain_unix() {
  python3 - <<'PY'
import base64, json, struct, urllib.request
req = urllib.request.Request(
    "http://127.0.0.1:8899",
    data=json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": ["SysvarC1ock11111111111111111111111111111111", {"encoding": "base64"}],
    }).encode(),
    headers={"content-type": "application/json"},
)
acc = json.load(urllib.request.urlopen(req))["result"]["value"]["data"][0]
print(struct.unpack_from("<q", base64.b64decode(acc), 32)[0])
PY
}
wait_chain_clock_past() {
  local expiry="$1" now
  for _ in $(seq 1 120); do
    now="$(solana_chain_unix)"
    if [[ "$now" -ge "$expiry" ]]; then
      return 0
    fi
    sleep 1
  done
  fail "Solana chain clock $now did not reach expiry $expiry"
}

echo "WALKTHROUGH_EXPIRY_BEFORE_SETTLEMENT"
expiry_dir="$run_dir/bridge-expiry"
journal_dir="$run_dir/bridge-op"
rm -rf "$expiry_dir" "$journal_dir"
BRIDGE_MINT_EXPIRY_SECS=20 BRIDGE_JOURNAL_DIR="$expiry_dir" \
  run_workflow --journal "$expiry_dir" --resume --expiry-recovery
grep -q "FAULT_INJECTED expiry_before_settlement" "$log" || fail "expiry before settlement was not injected"
grep -q "RECOVERY_RESULT cancelled" "$log" || fail "expiry before settlement did not cancel"
echo "WALKTHROUGH_CHECKED expiry_before_settlement"

echo "WALKTHROUGH_ATTESTER_DISAGREEMENT"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after locked
assert_journal_step "$journal_dir" locked
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --halt-after-first-approval
assert_journal_step "$journal_dir" locked
[[ "$(last_marker MINT_APPROVAL_BITMAP)" == "MINT_APPROVAL_BITMAP 1" ]] \
  || fail "one-attester state was not recorded: $(last_marker MINT_APPROVAL_BITMAP)"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --inject-attester-disagreement
assert_journal_step "$journal_dir" locked
grep -q "FAULT_INJECTED attester_disagreement" "$log" || fail "attester disagreement was not injected"
grep -q ATTESTER_DISAGREEMENT_REJECTED "$log" || fail "attester disagreement was not rejected"
grep -q "RECOVERY_WAIT_UNBOUNDED operator_or_quorum" "$log" \
  || fail "attester disagreement did not record that quorum wait is unbounded"
grep -q CHAIN_CLOCK "$log" || fail "attester disagreement did not record chain time"
[[ "$(last_marker MINT_APPROVAL_BITMAP)" == "MINT_APPROVAL_BITMAP 1" ]] \
  || fail "conflicting attestation counted toward quorum"
python3 - "$journal_dir/journal.json" <<'PY'
import json, sys
journal = json.load(open(sys.argv[1]))
if journal.get("mint_holding"):
    raise SystemExit("disagreement minted before quorum")
PY
echo "WALKTHROUGH_CHECKED attester_disagreement"

echo "WALKTHROUGH_CRASH_AFTER_REDEMPTION"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after mint_approved
grep -q "RECOVERY_DURATION_CHAIN_SECS" "$log" \
  || fail "attester disagreement recovery did not record chain duration"
for step in canton_minted trade_prepared reassigned settled redeemed; do
  BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --resume --stop-after "$step"
done
assert_journal_step "$journal_dir" redeemed
grep -q "FAULT_INJECTED delayed_release_after_redemption" "$log" \
  || fail "delayed release after redemption was not injected"
grep -q "RECOVERY_WAIT_UNBOUNDED operator_or_quorum" "$log" \
  || fail "delayed release did not record that operator resume wait is unbounded"
echo "WALKTHROUGH_CHECKED crash_after_redemption"

echo "WALKTHROUGH_EXPIRED_RELEASE_AFTER_SETTLEMENT"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_RELEASE_EXPIRY_SECS=15 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after release_approved
assert_journal_step "$journal_dir" release_approved
release_expiry="$(python3 -c "import json; print(json.load(open('$journal_dir/journal.json'))['release_expiry'])")"
wait_chain_clock_past "$release_expiry"
tx_before_release="$(grep -c RELAYER_RELEASE_CONFIRMED "$log" || true)"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_RELEASE_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after released
assert_journal_step "$journal_dir" released
grep -q "FAULT_INJECTED expiry_after_settlement" "$log" || fail "expired release approval was not refreshed"
grep -q RELEASE_REFRESHED_AFTER_CHAIN_EXPIRY "$log" || fail "release was not refreshed from chain time"
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED "$log")" -eq $((tx_before_release + 1)) ]] \
  || fail "expired release path did not release exactly once"
grep -q "RECOVERY_DURATION_CHAIN_SECS" "$log" \
  || fail "delayed release recovery did not record chain duration"
echo "WALKTHROUGH_CHECKED expired_release_after_settlement"

echo "WALKTHROUGH_TWO_COMPLETED_RESUMES"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
assert_journal_step "$journal_dir" zama_redeemed
tx_before="$(grep -c TX_SIZE "$log" || true)"
release_before="$(grep -c RELAYER_RELEASE_CONFIRMED "$log" || true)"
zama_before="$(grep -c ZAMA_REDEEM_OK "$log" || true)"
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
  run_workflow --journal "$journal_dir" --resume --stop-after zama_redeemed
[[ "$(grep -c COMPLETED_RESUME_SKIP_SETUP "$log")" -ge 2 ]] \
  || fail "two completed resumes did not skip setup"
[[ "$(grep -c CANTON_VERIFY_OK "$log")" -ge 2 ]] \
  || fail "two completed resumes did not verify connected Canton history"
[[ "$(grep -c TX_SIZE "$log" || true)" == "$tx_before" ]] \
  || fail "completed resumes submitted another Solana transaction"
[[ "$(grep -c RELAYER_RELEASE_CONFIRMED "$log")" == "$release_before" ]] \
  || fail "completed resumes released again"
[[ "$(grep -c ZAMA_REDEEM_OK "$log")" == "$zama_before" ]] \
  || fail "completed resumes redeemed Zama again"
echo "WALKTHROUGH_CHECKED two_completed_resumes"

echo "WALKTHROUGH_COMPLETE"
