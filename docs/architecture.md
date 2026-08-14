# Architecture

This document describes the structure of the system: what depends on what, who runs what, where
contracts live, and exactly where atomicity begins and ends. The narrative overview is in the
[README](../README.md).

## Package boundaries

Five Daml packages, plus the vendored Canton Token Standard V1 interface packages under `lib/`.

| Package | Depends on | Deliberately does not depend on |
|---|---|---|
| `treasury-registry` | Token Standard V1 metadata, holding, allocation, allocation-instruction | `stablecoin-registry`, `dvp-settlement` |
| `stablecoin-registry` | the same Token Standard V1 interfaces | `treasury-registry`, `dvp-settlement` |
| `dvp-settlement` | Token Standard V1 metadata, holding, allocation, allocation-request | both registries |
| `tests` | all three, for test fixtures only | — |
| `integration` | all three, for the multi-participant scenario only | — |

The two registries share no code. Eligibility, holdings, splitting and merging, and the allocation
factory are implemented twice rather than factored into a common package, because a shared package
would be a shared governance dependency: upgrading it would require both issuers to agree. The
symmetry is intentional; the coupling is not.

`dvp-settlement` depends on neither registry. It expresses what it needs through the standard
interfaces: it implements `AllocationRequest` so each registry can independently discover the legs
it is being asked to fund, and it exercises `Allocation_ExecuteTransfer` through the `Allocation`
interface without knowing which template implements it. This is what makes the composition
late-bound: a registry can be swapped for any other Token Standard V1 implementation without
recompiling the settlement package.

## Parties and participants

Six participants, each hosting exactly one party. Exclusive hosting is asserted during bootstrap;
every command is submitted through the participant that hosts the acting party.

| Participant | Party | Role | Ledger API |
|---|---|---|---|
| `p-treasury` | `treasuryRegistry` | issues and administers the Treasury instrument | 5011 |
| `p-cash` | `cashRegistry` | issues and administers the stablecoin | 5021 |
| `p-seller` | `seller` | delivers Treasury units | 5031 |
| `p-buyer` | `buyer` | pays stablecoins | 5041 |
| `p-venue` | `venue` | initiates and executes settlement | 5051 |
| `p-outsider` | `outsider` | connected and fully vetted, party to nothing | 5061 |

## Synchronizer connections

| Synchronizer | Sequencer / mediator | Connected participants |
|---|---|---|
| `settlement-sync` | 5001 / 5002, mediator 5003 | all six |
| `treasury-sync` | 5101 / 5102, mediator 5103 | `p-treasury`, `p-seller` |

Only the Treasury registry and the seller join `treasury-sync`, because only they are stakeholders
on a Treasury holding at issuance. Package vetting follows the same split: the Treasury package is
vetted on both synchronizers, since both the source and the target of a reassignment must be able to
validate the contract; the stablecoin and settlement packages are vetted on `settlement-sync` only.

## Contract placement

| Contract | Synchronizer | Created by |
|---|---|---|
| `TreasuryInstrument` | `treasury-sync` | console, explicit synchronizer ID |
| Seller's initial `TreasuryHolding` | `treasury-sync` | console, explicit synchronizer ID |
| `TreasuryRules`, Treasury eligibility | `settlement-sync` | Daml Script |
| Stablecoin instrument, rules, eligibility, holdings | `settlement-sync` | Daml Script |
| `DvpProposal`, `DvpTrade` | `settlement-sync` | Daml Script |
| Both allocations and locked holdings | `settlement-sync` | Daml Script |
| The settlement transaction | `settlement-sync` | Daml Script |

Daml Script cannot prescribe a synchronizer, so the contracts that must originate on `treasury-sync`
are created over the Ledger API from the Canton console with an explicit synchronizer ID. Placement
is then asserted for every contract rather than assumed.

Only the Treasury holding the trade consumes is reassigned. The instrument and the eligibility
attestations are not settlement inputs and are never moved.

## Reassignment sequence

A contract is assigned to exactly one synchronizer at a time, and a transaction cannot span two.
The seller's holding is therefore moved before it can be allocated:

1. Confirm active on `treasury-sync`; record the reassignment counter.
2. `submit_unassign` from `treasury-sync` to `settlement-sync`; record the reassignment ID.
3. Confirm the contract is in the incomplete-unassigned set and inactive on the source.
4. `submit_assign` with that reassignment ID.
5. Confirm the same contract ID is active on `settlement-sync`.
6. Confirm owner, amount, instrument, and the full payload are byte-for-byte unchanged.
7. Confirm the counter incremented exactly once.

Unassignment and assignment are two protocol steps with an observable interval between them, during
which the contract belongs to neither synchronizer and cannot be used in any transaction. It is not
lost: it remains in the incomplete-unassigned set and the assignment can still complete.

Cross-synchronizer movement is a topology capability, not an application feature. `p-treasury` and
`p-seller` advertise multi-synchronizer support in their synchronizer trust certificates as a
separate step; the integration run grants that capability only around the explicit reassignment
phases and revokes it immediately afterwards, so no application phase can be rescued by the
automatic synchronizer router.

## Allocation sequence

Each leg is funded independently by its own registry, through its own `AllocationFactory`:

1. The sender exercises `AllocationFactory_Allocate` with the allocation specification taken from
   the trade, passing its eligibility attestation through `ExtraArgs.context`.
2. The factory validates the instrument, the sender, the amount, and eligibility.
3. It consumes the input holding, creates a **locked** holding backing the allocation, and returns
   any change as a new unlocked holding.
4. It creates the `Allocation`, whose specification must equal the leg the trade expects.

Eligibility is checked **only here**. Settlement performs no compliance check, which keeps the
atomic step self-contained: at the moment of commitment there is nothing left to look up, nothing
external to consult, and nothing that can fail for a reason unrelated to the trade itself.

## Authorization flow

1. The seller creates a `DvpProposal`. Approval is progressive: each counterparty approves for
   itself, cannot approve twice, and the venue cannot approve on anyone's behalf. Each approval
   recreates the proposal with the approver added as a signatory.
2. Once seller and buyer have approved, the venue initiates settlement, creating `DvpTrade`, which
   is **jointly signed by venue, seller, and buyer**. No subset can create it.
3. That joint signature is the authority `DvpTrade_Settle` later uses to execute both allocations.
   The venue controls the choice but cannot exceed the authority the counterparties already granted.
4. The settlement reference is derived from the originating proposal's contract ID, so every trade
   has a distinct reference even when its economic terms are identical.

## The atomic boundary

The atomic unit is one Daml transaction on one synchronizer. `DvpTrade_Settle` validates both legs
by exact match — each leg must be backed by exactly one allocation whose full specification,
including the settlement reference, equals what the trade expects — and then exercises both
`Allocation_ExecuteTransfer` choices as consequences of that single choice.

Reassignment is a separate, non-atomic, value-neutral preparation step. Atomicity begins only when
both allocations are available on `settlement-sync` and `DvpTrade_Settle` executes both transfer
legs in one Daml transaction. The two synchronizers never jointly execute anything.

## Failure behavior

| Failure | Result |
|---|---|
| Either transfer leg fails | The whole transaction is rejected; both allocations and the trade remain, and no holding moves |
| An allocation belongs to a different trade | Exact-match validation rejects the settlement before anything executes |
| Both legs are supplied for one side, or a leg is missing or duplicated | Rejected by the same validation |
| The Treasury holding is still on `treasury-sync` | Allocation fails; no allocation and no locked holding are created, and the holding is unchanged |
| A contract is pending assignment | Any command using it is rejected; the contract is not archived and the assignment can still complete |
| An allocation expires or is withdrawn | The locked holding is released to its owner; no partial settlement state exists at any point |

There is no code path that settles one leg and then the other. Both are consequences of one choice,
so Canton either commits both or neither.
