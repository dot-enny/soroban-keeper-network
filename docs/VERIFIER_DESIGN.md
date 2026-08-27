# Verifier Design (E04)

This is the design document for the `IKeeperVerifier` interface — the
decision record the other issues in E04 (0072–0096) implement against. No
contract code changes are made by this document; it exists so the
interface is agreed on paper before several PRs build on top of it.

## Context

The MVP (wave 1) trusts the claiming keeper to submit an honest `proof` —
`execute_task` records it but never checks it against anything (see the
README's Known Design Decisions section). E04 replaces "trust the keeper"
with an **optional**, per-task, on-chain verification callback: a task
owner can attach a verifier contract at registration time, and
`execute_task` calls it before crediting the keeper.

## 1. Interface shape

```rust
/// Implemented by any contract a task owner wants to use as a per-task
/// proof verifier. Registered per-task via `register_task`'s optional
/// `verifier` parameter (see §4, Attachment timing).
pub trait IKeeperVerifier {
    /// Returns `true` if `proof` is a valid witness that `keeper` correctly
    /// executed `task_id`'s off-chain work, `false` otherwise.
    ///
    /// Must not panic on a merely-invalid proof — return `false`. A panic
    /// is reserved for the verifier being fundamentally broken (see §2),
    /// and `execute_task` treats it as equivalent to `false` regardless.
    fn verify(env: Env, task_id: u64, keeper: Address, proof: Bytes) -> bool;
}
```

**`keeper` is part of the signature.** A verifier that only receives
`task_id` and `proof` cannot bind the proof to the specific keeper
claiming credit — anyone who observes a valid `(task_id, proof)` pair
on-chain (proofs are logged in the `exec` event, per wave 1's issue #4)
could resubmit it under their own claim on a *different* task the same
verifier is attached to, if the proof format doesn't happen to encode
enough context itself. Requiring the registry to pass `keeper` explicitly
means every reference verifier the registry ships (§7–9) can and should
check the proof against that specific address, rather than relying on
every third-party verifier author to remember to do so unprompted.

**No return-value error detail.** `verify` returns a plain `bool`, not a
`Result<bool, E>` with a typed reason. A verifier that wants to
communicate *why* a proof failed (for off-chain debugging, e.g. by a
keeper bot deciding whether a proof is worth resubmitting) should emit its
own event before returning `false` — the registry's `VerificationFailed`
event (§8) intentionally does not attempt to relay a verifier-specific
reason code, since that would couple the registry's ABI to every
verifier's own error taxonomy.

## 2. Failure semantics

**If the verifier call panics, `execute_task` catches it and returns a
typed error rather than reverting the whole transaction.**

Soroban's host provides exactly this primitive:
`Env::try_invoke_contract` catches a callee panic (and any other callee
error) and surfaces it as a `Result` to the caller, as opposed to
`Env::invoke_contract`, which propagates the callee's failure straight
through and aborts the calling transaction too. `execute_task` uses
`try_invoke_contract`, mapping any panic *or* an explicit `false` return
to the same outcome: the execution attempt fails with
`KeeperError::VerificationFailed`, task state is unchanged (no partial
credit, no status transition — enforced the same way every other
rejection path in `execute_task` already works, per I-3/I-5 in
`docs/ARCHITECTURE.md`), and the keeper is free to retry (their claim
lock is untouched) or the task can still expire/be cancelled normally by
its other paths.

This was a real design choice, not a foregone conclusion: propagating the
panic (aborting the whole transaction) is *also* safe from a funds
perspective — no state changes were persisted, since Soroban transactions
are atomic — but it would mean a single misbehaving or buggy verifier
contract could make `execute_task` unusable for every task attached to
it, with no way to recover the escrow except waiting for the deadline and
falling back to `expire_task`. Returning a typed error instead gives the
keeper (or the task owner, via `cancel_task` once the lock lapses, or
anyone via `expire_task` once the deadline passes) every existing recovery
path immediately, rather than only the slowest one.

## 3. Resource budget

**No in-contract budget ceiling can be reserved or enforced for the verifier
sub-call; the whole transaction's resource footprint (set by whoever submits it)
is the only limit. The calling keeper bears the full resource cost of the
attached verifier.**

### 3.1 Investigation: In-band sub-call budget capping in Soroban

We investigated whether Soroban currently exposes any host function, SDK
primitive, or transaction mechanism allowing a caller contract to bound or
cap the gas/CPU/memory/storage resources consumed by an individual cross-contract
sub-call (distinct from the transaction's overall resource limit).

**Conclusion: No such mechanism exists in Soroban today.**

*Evidence & Technical Constraints:*
1. **Host invocation model:** In Soroban, cross-contract calls dispatched via
   `Env::invoke_contract` or `Env::try_invoke_contract` execute synchronously within
   the host environment sharing the active transaction's monolithic execution
   budget (`Budget`). The host meters instruction counters and memory allocations
   globally against the transaction envelope.
2. **SDK Budget types:** The `soroban_sdk::Env::cost_estimate().budget()` and
   `Budget` interfaces exist exclusively under `testutils` for local measurement
   and harness assertions. They are not compiled into the on-chain WASM guest
   environment and provide no runtime controls to meter or restrict callees.
3. **No gas/stipend forwarding parameter:** Unlike Ethereum's EVM (which allows
   passing explicit gas stipends to sub-calls, e.g. `call{gas: g}()`), Soroban's
   host invocation interface does not take a gas limit parameter.
4. **All-or-nothing resource metering:** If a verifier's execution exceeds the
   transaction's configured resource limits or ledger network limits, the entire
   host invocation runs out of budget, resulting in a transaction-level failure.

### 3.2 Economic impact: Who bears the cost?

Because the verifier sub-call executes as part of `execute_task`, **the executing
keeper bears the entire transaction fee and resource cost** incurred by running
whatever verifier contract the task owner attached.

An expensive or intentionally resource-intensive verifier directly increases the
CPU instructions and memory footprint charged to the keeper's execution
transaction. If uninspected, this cost can easily exceed the task reward, turning
a seemingly profitable task into a net financial loss for the keeper.

### 3.3 Keeper bot cost estimation & Simulation ordering (Pre-claim vs. Post-claim)

To make an informed profitability decision, a keeper bot must be able to estimate
the execution resource footprint **before claiming** a task.

#### Simulation via `simulateTransaction`
Soroban RPC's `simulateTransaction` endpoint evaluates a contract invocation
against current ledger state and returns exact estimated resource usage
(`minResourceFee`, `cpuInsns`, `memBytes`, and read/write footprints).

#### Impact of `Claimed` status precondition
In `KeeperRegistry::execute_task`, there is a strict state precondition:
```rust
if task.status != TaskStatus::Claimed {
    return Err(KeeperError::InvalidTaskStatus);
}
if task.claimer.as_ref() != Some(&keeper) {
    return Err(KeeperError::NotTaskClaimer);
}
```
If a bot attempts to simulate `execute_task` directly against a `Pending` (unclaimed)
task on-chain, `execute_task` will abort early with `KeeperError::InvalidTaskStatus`
before reaching the verifier invocation.

#### Recommended Keeper-Bot Strategy:
1. **Direct Verifier Simulation (Pre-claim estimation):**
   Before committing to `claim_task`, the keeper bot knows:
   - `task_id`
   - its own `keeper` address
   - the candidate `proof`
   - the attached `verifier` address (retrieved via `get_task`)
   
   If `task.verifier` is `Some(verifier_address)`, the bot can issue a
   `simulateTransaction` call directly to `IKeeperVerifier::verify(task_id, keeper, proof)`
   on the verifier contract. This yields the verifier's isolated resource
   consumption before the bot spends any transaction fees claiming the task.
2. **Known Verifier Baselines / Catalog:**
   For standard reference verifiers (e.g. signature verification, oracle checks),
   the bot can refer to benchmarked gas deltas (documented in `docs/VERIFIERS.md`)
   to compute an immediate heuristic cost without an extra simulation round-trip.
3. **Post-Claim Verification:**
   After claiming the task (which locks the verifier against further changes per §4),
   the bot can simulate the full `execute_task` transaction to finalize the fee
   bid before broadcast.

### 3.4 Requirements Handoff to Issue 0091 (Keeper-Bot Task Selection)

Issue 0091 (bot-side task selection and profitability checking) must implement the
following requirements based on these findings:
1. **Inspect `task.verifier` on discovery:** Fetch full task info via `get_task`
   upon receiving a `TaskRegistered` event.
2. **Pre-claim cost estimation:** If `verifier` is present:
   - Option A: Simulate `verifier.verify(task_id, keeper_address, proof)` via RPC
     to get the verifier sub-call footprint.
   - Option B: Look up the verifier address in a local allowlist/catalog of known
     costs. If unknown and unsimulated, treat the task with high risk or skip it.
3. **Net Profitability Calculation:**
   Only claim the task if:
   $$\text{Expected Reward Net} > \text{Estimated Gas}(\text{claim\_task}) + \text{Estimated Gas}(\text{execute\_task baseline} + \text{verifier}) + \text{Off-chain Compute Cost} + \text{Profit Margin}$$
4. **Untrusted verifier policy:** If a task specifies an unknown, high-gas, or
   failing verifier during pre-claim simulation, the bot should decline to claim
   the task.

## 4. Attachment timing

**A verifier is chosen at `register_task` time and is immutable once a
keeper has claimed the task (per 0082); the owner may still change it
while the task is `Pending`.**

Rationale: a keeper decides whether a task is worth claiming partly based
on how hard/expensive it'll be to produce a satisfying proof — that
decision is made against whatever verifier is attached *at claim time*.
Letting the owner swap the verifier out from under an already-claimed
task (after the keeper has done off-chain work matching the old verifier)
would let an owner grief a keeper by attaching an impossible-to-satisfy
verifier post-claim, with no way for the keeper to recover except waiting
out the lock window. Locking the verifier at claim time closes that; still
allowing changes pre-claim keeps it consistent with every other
owner-adjustable field on a `Pending` task (`increase_reward`,
`extend_deadline` already work this way).

## 5. Trust model

**Permissionless: any address may be used as a verifier, consistent with
the registry's existing trust model** (keepers are permissionless;
correctness is enforced by contract logic, not a whitelist — see
`docs/ARCHITECTURE.md`'s Trust model section).

A registry-level admin-curated allow-list (630's fork, tracked separately
as issue 0092) is explicitly *not* part of this baseline design — adding
one is a strictly separate, optional extension an operator could layer on
top (e.g. a wrapper contract that only forwards to allow-listed
verifiers), not a change to `IKeeperVerifier` or `execute_task` itself.
Baking an allow-list into the core registry would mean every dApp using
the registry inherits whichever admin's curation policy, which cuts
against the "admin can never gate ordinary task/keeper activity" property
I-5 already establishes for fee sweeping — extending that same principle,
an admin should not get to gate *which verifiers are usable* either,
without an explicit, separately-designed extension opting into that
tradeoff.

## 6. Backward compatibility

**A task with no verifier attached behaves identically to every existing
task today** — `execute_task` performs the verifier call only when
`Task.verifier` is `Some(_)`; when it's `None`, execution proceeds exactly
as it does on `main` right now, with no additional call, no additional
gas cost, and no behavior change. Existing tasks registered before this
epic ships have no `verifier` field populated (backward-compatible
storage migration: `Task.verifier: Option<Address>` defaults to `None`
for any task read that predates this field, the same pattern the existing
`Task` struct already handles for schema evolution elsewhere in this
contract). Any dApp integration written against the current ABI continues
to work with zero changes required — attaching a verifier is opt-in per
task, not a new required parameter with no default.

## Summary of decisions

| Question | Decision |
|---|---|
| Interface shape | `fn verify(env, task_id, keeper, proof) -> bool` — `keeper` included to bind the proof to the specific claim |
| Failure semantics | `execute_task` uses `try_invoke_contract`; a panicking or `false`-returning verifier both map to `KeeperError::VerificationFailed`, never a transaction-wide revert |
| Resource budget | No in-contract sub-call cap is technically possible on Soroban today; calling keeper bears full verifier cost — keeper bots must simulate verifier pre-claim (handoff to 0091) |
| Attachment timing | Chosen at `register_task`, owner-changeable while `Pending`, immutable once claimed |
| Trust model | Permissionless — any address may be a verifier; an admin allow-list is an optional, separate extension (0092), not baseline |
| Backward compatibility | `Task.verifier: Option<Address>`, `None` behaves identically to today, zero-cost when absent |

## Status

Proposed. Per this issue's own acceptance criteria, 0072–0096 should wait
for a maintainer to review and lock these decisions (or request changes)
before building against them — this document is the basis for that
review, not a substitute for it.
