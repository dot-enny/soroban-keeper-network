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

**No documented budget ceiling is reserved for the verifier call; the
whole transaction's resource footprint (set by whoever submits it) is
the only limit.**

Soroban does not give a contract an in-band way to sub-allocate a
resource budget to a specific cross-contract call and enforce it — the
`Budget` type in `soroban-sdk`'s `testutils` is a test-only
measurement/reset tool, not a runtime limiting mechanism a contract can
invoke against a callee. The actual ceiling on a cross-contract call's
CPU/memory cost is the calling transaction's own resource footprint,
declared by whoever submits it (a keeper bot, in this case) before
simulation/submission.

Practically, this means: a keeper choosing to execute a task with an
expensive verifier attached pays for that cost in their own transaction's
resource footprint, and an excessively expensive verifier simply makes the
transaction fail at the network's resource-limit boundary (the same
failure mode as any other transaction that tries to do too much) — not a
distinct, registry-specific error. `docs/FUZZING.md`'s target-status table
and this repo's keeper-bot example (`examples/keeper-bot`) should document
the practical implication: a keeper bot integrating with a
verifier-gated task should simulate the transaction first (standard
Soroban RPC `simulateTransaction`) to estimate the real cost before
committing to a fee, exactly as it should for any other execution — this
isn't a new burden E04 introduces, just one that becomes newly relevant
once a verifier call is in the path.

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

## 5. Trust model: Permissionless vs. Admin-Curated Verifiers (#117)

### 5.1 The Tension

The registry must balance two competing architectural forces regarding verifier addresses:

1. **Permissionless Participation (Protocol Philosophy)**:
   - Consistent with task registration, keeper participation, and fee invariants (I-5 in `docs/ARCHITECTURE.md`), the registry does not gate participation. Any dApp or user can register a task with arbitrary calldata or attach custom verification logic.
   - Enforcing an admin whitelist in the core `register_task` / `execute_task` pipeline would centralize authority, introduce governance bottlenecks for new verifier deployments, and break the core invariant that administrators cannot gate ordinary task/keeper operations.

2. **Keeper Protection & DoS / Griefing Surface**:
   - Unlike task payloads, an attached verifier executes cross-contract code on `execute_task` whose gas is paid by the claiming keeper.
   - An unvetted or malicious verifier could consume excessive resources, revert unexpectedly, or contain logic impossible to satisfy, griefing claiming keepers (wasting keeper claim gas or locking keeper capital).
   - Keepers and user interfaces need a dependable trust signal to differentiate between reference/audited verifiers and untrusted third-party contracts.

### 5.2 The Decision: Permissionless Execution Path with Optional Advisory Curated Registry

We adopt the **advisory middle ground**:

1. **Core Registry is Fully Permissionless**:
   - The core contract allows *any* address implementing `IKeeperVerifier` to be attached at `register_task`. `register_task` and `execute_task` do **not** enforce an allow-list or require admin approval.
   - This keeps the execution hot path simple, gas-efficient, and permissionless without admin intervention or single-point-of-failure governance.

2. **Advisory "Vetted Verifier" Registry (Follow-Up Extension)**:
   - Expose an on-chain, admin-curated registry of vetted verifier addresses (either as dedicated view functions on the registry or as a companion registry contract).
   - This list serves strictly as an **advisory trust signal** for off-chain keeper bots and dApp UIs:
     - **Keeper Bots**: Can consult the curated list or local config to prioritize tasks with vetted verifiers while skipping or applying stricter simulation thresholds to uncurated verifiers (per issue #116).
     - **Frontends / dApps**: Can display verification badges or warning prompts when users interact with unvetted verifiers.
   - The on-chain execution logic in `execute_task` remains completely unblocked regardless of whether a verifier is in the curated registry.

### 5.3 Follow-up Implementation Scope

A dedicated follow-up issue will specify and implement the advisory vetted verifier registry:
- **Title**: `feat(registry): implement advisory vetted verifier list and query views`
- **Scope**:
  - Admin functions: `add_vetted_verifier(admin, verifier_address)` and `remove_vetted_verifier(admin, verifier_address)`.
  - Read-only query: `is_vetted_verifier(verifier_address) -> bool`.
  - Events: `VerifierVetted(verifier, added_by)` and `VerifierUnvetted(verifier, removed_by)`.
  - Unit tests verifying admin-only authorization and non-interference with permissionless `execute_task`.

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
| Resource budget | No in-contract ceiling reserved; the calling transaction's own resource footprint is the only limit — keeper bots should simulate first |
| Attachment timing | Chosen at `register_task`, owner-changeable while `Pending`, immutable once claimed |
| Trust model | Permissionless execution path (any address implementing `IKeeperVerifier` can be attached); optional on-chain advisory curated list for bot/UI trust signals (#117) |
| Backward compatibility | `Task.verifier: Option<Address>`, `None` behaves identically to today, zero-cost when absent |

## Status

Proposed. Per this issue's own acceptance criteria, 0072–0096 should wait
for a maintainer to review and lock these decisions (or request changes)
before building against them — this document is the basis for that
review, not a substitute for it.
