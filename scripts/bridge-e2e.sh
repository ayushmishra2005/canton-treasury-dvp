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

for step in accounts reserved locked mint_approved canton_minted trade_prepared reassigned settled redeemed release_approved released zama_redeemed; do
  echo "BRIDGE_RESUME_AFTER $step"
  BRIDGE_MINT_EXPIRY_SECS=90 BRIDGE_JOURNAL_DIR="$journal_dir" \
    run_workflow --journal "$journal_dir" --reuse-from "$expiry_dir" --resume --stop-after "$step"
done

grep -q CANTON_MINT_HOLDING /tmp/ctd-workflow.log || fail "Canton mint holding was not recorded"
grep -q DVP_BUYER_TREASURY /tmp/ctd-workflow.log || fail "buyer did not receive Treasury"
grep -q DVP_SELLER_STABLECOIN /tmp/ctd-workflow.log || fail "seller did not receive stablecoins"
grep -q DVP_PAYMENT_AMOUNT /tmp/ctd-workflow.log || fail "DvP payment amount was not asserted"
grep -q CANTON_REDEEM /tmp/ctd-workflow.log || fail "seller redemption was not recorded"
grep -q RELAYER_RELEASE_CONFIRMED /tmp/ctd-workflow.log || fail "confidential release through Relayer did not confirm"
grep -q ZAMA_REDEEM_OK /tmp/ctd-workflow.log || fail "Zama redemption did not succeed"
grep -q EXPIRY_RECOVERY_COMPLETE /tmp/ctd-workflow.log || fail "expiry recovery evidence is missing"
echo "BRIDGE_E2E_COMPLETE"
