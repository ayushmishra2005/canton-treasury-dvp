# Testing

Two levels. The Daml Script suite proves the model on an in-memory ledger; the Canton integration
suite proves the same workflow across six participants and two synchronizers, where placement,
reassignment, and privacy become observable.

```bash
make verify         # original Canton DvP only
make test           # 194 core Daml Script tests
make integration    # two-synchronizer Canton suite
make bridge-test    # Solana, Zama mock-FHE, bridge Daml, Rust, format, clippy, typecheck
make bridge-verify  # verify plus every confidential-bridge gate
make dep-audit      # production npm audit and RustSec checks
```

## Daml Script coverage

194 tests, all on the in-memory ledger, no Canton node required.

| Area | Tests | What is established |
|---|---|---|
| `Tests/Treasury/Issuance`, `SplitMerge` | 14 | Registry-controlled issuance, split and merge conservation, rejection of locked holdings |
| `Tests/Treasury/Eligibility` | 6 | Only the registry attests, revocation, validity windows, no personal data on ledger |
| `Tests/Treasury/Allocation`, `AllocationLifecycle`, fixture | 38 | Allocation creation and validation, locking, change holdings, execute, cancel, withdraw, expiry |
| `Tests/Stablecoin/MintBurn`, `SplitMerge` | 19 | Registry-controlled mint and burn, conservation, locked-holding rejection |
| `Tests/Stablecoin/Eligibility` | 6 | The same eligibility rules, implemented independently |
| `Tests/Stablecoin/Allocation`, `AllocationLifecycle`, fixture | 39 | The stablecoin allocation lifecycle, symmetric to Treasury |
| `Tests/Dvp/Proposal` | 19 | Progressive approval, self-approval only, no double approval, rejection, joint trade creation |
| `Tests/Dvp/AllocationRequest` | 10 | The `AllocationRequest` view each registry reads, and that it matches what settlement expects |
| `Tests/Dvp/ExactMatch` | 21 | Exact-leg validation: wrong instrument, amount, party, deadline, reference, duplicates, omissions |
| `Tests/Dvp/Settlement`, `SettlementFixture` | 9 | The happy path, final balances, conservation, consumption of trade and allocations |
| `Tests/Dvp/Atomicity` | 8 | Each leg driven into failure, with no partial settlement observable |
| `Tests/Dvp/TradeIsolation` | 5 | Cross-trade isolation regression, described below |

**Atomicity gating.** Each leg is deliberately made to fail — an allocation withdrawn, expired, or
already consumed — and the suite asserts that `DvpTrade_Settle` is rejected as a whole and that
every input is exactly as it was. There is no test that asserts "one leg settled", because there is
no state in which that is possible.

**Cross-trade isolation regression.** Two economically identical trades are created. The tests
assert that their settlement references differ, that an allocation created for trade A is rejected
by trade B, that mixing one allocation from each trade is rejected, and that the
`AllocationRequestView` and the settlement reconstruction derive the same reference. This is a
regression suite: an earlier design used a package-wide constant reference, under which allocations
were interchangeable between trades. The reference is now derived from the originating proposal's
contract ID.

## Integration topology

`make integration` starts two synchronizers and six participants, all in-memory, and runs phases in
sequence. Any phase failing fails the run.

| Phase | Establishes |
|---|---|
| `bootstrap.canton` | Both synchronizers, the connection matrix, exclusive party hosting, per-synchronizer vetting, offset checkpoints |
| `origination.canton` | Treasury instrument and holding created on `treasury-sync`; the wrong-synchronizer test |
| `Integration.Stage1:setup` | Settlement-side setup; Treasury allocation attempted and rejected before reassignment |
| `reassignment-capability.canton` | Grants multi-synchronizer support to `p-treasury` and `p-seller`, and revokes it afterwards |
| `reassign.canton` | Placement assertions and the explicit unassign/assign with full evidence |
| `Integration.Stage2:settle` | Both allocations and the single atomic `DvpTrade_Settle` |
| `probe-pending.canton` | The pending-assignment test and the reassignment round trip |
| `verify-privacy.canton` | 97 participant-specific privacy assertions |

## Wrong-synchronizer test

Two independent checks, both before any reassignment.

A command on the Treasury holding is submitted over the Ledger API with `settlement-sync` explicitly
prescribed. Canton rejects it with `AUTOMATIC_REASSIGNMENT_FOR_TRANSACTION_FAILED`: the router will
not silently move the contract to satisfy a submission. The holding is then re-read and is still
active on `treasury-sync` with its owner and amount unchanged.

Separately, the real Treasury allocation is attempted through the `AllocationFactory` while the
holding is still on `treasury-sync`. It fails; no allocation and no locked holding are created, and
the holding is byte-for-byte unchanged. That the same command, with the same arguments and the same
disclosures, succeeds in `Stage2` after reassignment is what rules out any cause other than the
holding's synchronizer.

## Pending-assignment test

An isolated probe holding, created for this purpose so the trade's assertions are unaffected:

1. Unassign it from `treasury-sync` and stop.
2. Assert it is inactive on both synchronizers and present in the incomplete-unassigned set.
3. Submit a command on it; assert rejection.
4. Assert the contract has not been archived or lost.
5. Complete the assignment; assert the payload is identical.
6. Submit the *same command that was rejected*; assert it now succeeds.

## Reassignment round trip

The probe is then reassigned back to `treasury-sync`. Owner, amount, instrument, and payload are
identical after two moves, and the reassignment counter reads 2. The trade's own reassignment is
checked the same way: same contract ID, unchanged payload, counter incremented exactly once, with
the reassignment ID and the source and target synchronizer IDs recorded at each step.

The probe runs after the DvP completes and records its own offset checkpoint first, so its holdings
fall outside the window used for the trade's privacy assertions.

## Participant-specific assertions

Privacy is verified from per-participant update streams read with `TransactionShape.LEDGER_EFFECTS`
and bounded by recorded offset checkpoints, using exact contract IDs, reassignment IDs, and
synchronizer IDs. Both positive and negative assertions are made for each participant; none relies
on a contract being absent from the active contract set. See [privacy.md](privacy.md) for the
matrix and its limits.

## Cleanup and ports

The harness refuses to start if any of the eighteen topology ports is already bound, rather than
attaching to a foreign node. All nodes are shut down through a single `EXIT` trap, on success and on
failure alike, and the run ends by re-checking every port and printing `PORTS_RELEASED 18`. A failed
run leaves no Canton process behind either.

## Expected results

```
TESTS_PASSED 194
LINT_CLEAN
DARS_VALID
INTEGRATION_COMPLETE
WHITESPACE_CLEAN
VERIFY_COMPLETE
```

The integration suite additionally asserts that the buyer ends with exactly 100 Treasury units, the
seller with exactly 100,000 stablecoins, both allocations and the trade consumed, no duplicate
receiver holding, and both instrument totals conserved.

## Confidential bridge tests

These are outside `make verify`.

| Command | What it checks |
|---|---|
| `cd daml/bridge-tests && dpm test` | Gateway mint and redeem, binding authorization, operation isolation, crash resume, faults, and bridged DvP |
| `cd solana && cargo test --manifest-path programs/confidential-escrow/Cargo.toml` | Token-2022 confidential escrow |
| `cd bridge && cargo test` | Coordinator, journal resume, and connected Canton history |
| `cd zama && npx hardhat test` | ConfidentialRiskEngine under mock FHE |
| `cd zama && npm run typecheck` | TypeScript type-check against generated TypeChain types |
| `scripts/bridge-e2e.sh` | Live local rail: mint-funded DvP, redemption binding, expiry, resume, Zama probes |
| `scripts/bridge-walkthrough.sh` | Separate operator cases, including two identical-term operations |
| `make dep-audit` | Production npm audit and RustSec on both lockfiles |
| `scripts/bridge-secret-scan.sh` | NUL-safe scan for private keys, Solana keypairs, and local secrets |
| `scripts/bridge-license-check.sh` | LICENSE and recorded NOTICE license identifiers |

`make bridge-test` runs the Solana, Zama, bridge Daml, Rust, format, clippy, and type-check gates.
`make bridge-verify` then runs the live E2E script, license, secret, and dependency checks.
