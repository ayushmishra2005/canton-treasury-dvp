# Privacy

Canton projects each transaction node only to the parties entitled to see that node. This document
records the resulting visibility, how it is produced, and — as importantly — what the local tests
do not establish.

## Visibility matrix

Verified against live participant update streams, not assumed from the model.

| Participant | Sees | Does not see |
|---|---|---|
| `p-treasury` | Treasury instrument and holdings, its own eligibility attestations, the Treasury allocation, the Treasury transfer leg, both reassignment events | Any stablecoin contract, the payment amount, the `DvpTrade` payload, the price |
| `p-cash` | Stablecoin instrument and holdings, its own eligibility attestations, the stablecoin allocation, the payment leg | Any Treasury contract, the Treasury quantity, the `DvpTrade` payload, the price, any reassignment event |
| `p-seller` | Proposal, trade, both allocations, the settlement outcome, its Treasury holding, its resulting stablecoins, both reassignment events | The buyer's original stablecoin holding, the outsider's contracts |
| `p-buyer` | Proposal, trade, both allocations, the settlement outcome, its stablecoins, its resulting Treasury units | The seller's pre-settlement Treasury holding, any reassignment event, the outsider's contracts |
| `p-venue` | Proposal approvals, the full trade terms, both allocations, both settlement outcomes | Any eligibility attestation, unrelated holdings, any reassignment event |
| `p-outsider` | Only its own control contract | Every proposal, trade, allocation, holding, eligibility attestation, settlement event, and reassignment event |

Two consequences are worth stating explicitly. Each registry helps settle a trade whose price it
never learns. And the stablecoin registry never learns that the Treasury holding has a reassignment
history at all: it receives no unassigned or assigned event, so the fact of the move — not merely
its contents — stays outside its view.

The buyer's position is the sharpest case. It receives the Treasury units at settlement and
therefore sees the resulting holding, but it never sees the seller's pre-settlement holding and
never sees the reassignment that moved it. Entitlement is per transaction node, not per contract
lineage.

## Five different mechanisms

These are routinely conflated. In this repository they are distinct and are tested as distinct.

**Stakeholder privacy** is what actually decides visibility. A party is an informee of a
transaction node if it is a signatory or observer of the affected contract, or a controller of the
exercised choice. Nothing else grants access to that node.

**Participant hosting** decides *where* data arrives. A participant sees exactly the union of what
its hosted parties are entitled to see. Two parties on one participant do not thereby see each
other's contracts, and a party's data does not reach a participant that does not host it.

**Synchronizer membership** decides which protocol messages a participant is eligible to receive at
all. It is a necessary condition, never a sufficient one: `p-cash`, `p-buyer`, `p-venue`, and
`p-outsider` are all connected to `settlement-sync`, and all four still see different things.

**Explicit disclosure** is how a party submits a command that references a contract it is not a
stakeholder on. The venue is not a stakeholder on the locked holdings, and the seller and buyer are
not stakeholders on each other's eligibility attestations or on the registry rules contracts, so
those contracts are passed as disclosed contracts at submission time. Disclosure is scoped to the
submission; it does not add observers and does not make the contract visible thereafter. No observer
is widened anywhere in this repository to make an integration test pass.

**Package availability** is the ability to validate a transaction, not the right to see one.
`p-outsider` has every settlement-side package vetted and is connected to `settlement-sync`, and
sees nothing. Vetting a package is not entitlement to the contracts created under it.

## How the tests establish this

`canton/scripts/verify-privacy.canton` makes 97 assertions against per-participant update streams.

- Streams are read with `TransactionShape.LEDGER_EFFECTS`, so exercise nodes are observed, not only
  created and archived contracts.
- Each stream is bounded by offset checkpoints recorded at bootstrap and before the pending-
  assignment probe, so every assertion is made over a defined window rather than over whatever
  happened to be on the ledger.
- Positive and negative assertions both name **exact contract IDs**, reassignment IDs, and
  synchronizer IDs. A participant is required to have seen a specific contract, or required not to
  have seen it.
- Reassignment events are read from the reassignment stream and matched on reassignment ID, source
  and target synchronizer IDs, and contract ID.
- The pending-assignment probe runs after the trade and records its own checkpoint first, so its
  extra holdings fall outside the window used for the trade's assertions and cannot weaken them.

No assertion relies on a contract merely being absent from the active contract set. Absence from the
ACS would be satisfied by an archived contract that the participant had watched throughout, which is
precisely the case that must not pass.

## What these tests do not prove

- **Not confidentiality against a compromised participant.** The assertions describe what Canton
  delivers to an honest participant. A participant operator that is malicious, or an attacker who
  has taken one over, is outside this model.
- **Not resistance to metadata inference.** Participants observe timing, transaction sizes, and
  their own view structure. This repository makes no claim about what could be inferred from those.
- **Not organizational independence.** All six participants run in one process tree on one machine.
  This demonstrates protocol behavior, not separation of legal entities or operational control.
- **Not production hardening.** Nodes are in-memory, unauthenticated, and local. There is no TLS, no
  authorization service, and no operational key management.
- **Not a general proof.** The assertions cover this scenario with these parties. They are evidence
  about Canton's projection behavior, not a formal proof of the privacy model.
