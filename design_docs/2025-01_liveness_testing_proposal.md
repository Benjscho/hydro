# Liveness Testing: Revised Implementation Proposal

## Key Questions Addressed

### 1. State Space Explosion from Drop Decisions

The naive approach — adding a binary "deliver or drop" decision per message per tick — would multiply the state space by 2^N for N pending messages. This is unacceptable for exhaustive search.

**Insight: Lossy networks don't need per-message decisions in the exhaustive explorer.** The existing `StreamHook` already non-deterministically chooses how many items to release (0..=N). Items not released stay in the buffer. For a lossy channel, the only difference is: items not released are *dropped* (removed permanently) rather than retained.

But this still doesn't solve the problem. With `sample_every`, the sender produces an *unbounded* stream of messages. The exhaustive search would never terminate because there's always another message to drop or deliver.

**The real solution: Don't model lossy as a separate per-message decision. Model it as a property of the execution that the scheduler enforces via lasso detection.**

### 2. Why "Deliver At Least One" Is Wrong

My previous proposal said fairness = "force delivery of at least one message." This conflates two things:

- **Fairness** (from the paper): every process that is *continuously enabled* must eventually be *scheduled*. A fair lasso is one where every enabled process in the cycle is also scheduled in the cycle.
- **Liveness violation**: a *hot fair lasso* — the system is stuck in a bad state and fairness alone can't save it.

The simulator shouldn't "force delivery." Instead, it should **detect when the system is in a hot fair lasso** (stuck making no progress despite fair scheduling) and use that to determine whether the test passes or fails.

### 3. How This Maps to Hydro's Simulator

In the paper's formalism:
- **State** = fold/singleton values + pending messages in all buffers
- **Processes** = the set of hooks that `can_make_nontrivial_decision()`
- **Scheduled** = which hook was chosen to make a nontrivial decision
- **Hot** = the test assertion is not yet satisfied (output hasn't yielded expected values)
- **Fair lasso** = a cycle where every hook that had pending messages was eventually given a chance to release them

## Revised Design

### The Lossy Hook Is Just a StreamHook

A lossy network channel uses the **same `StreamHook`** as a fail-stop channel. The difference is entirely in the scheduler's termination condition:

| Mode | Hook behavior | Termination |
|------|--------------|-------------|
| `FailStop` | Items stay in buffer until released | Quiescence when all buffers empty |
| `Lossy` | Items stay in buffer until released | Quiescence when **lasso detected** OR all buffers empty |

With `FailStop`, the scheduler reaches quiescence when no hook has pending items. The test assertion then checks whether the expected output arrived.

With `Lossy`, the scheduler may *never* empty all buffers (because `sample_every` keeps producing). Instead, the scheduler detects a **lasso**: the system state has repeated, meaning further scheduling won't produce new behavior. At that point, quiescence is declared.

### Lasso Detection (from the paper, adapted)

After each quiescence point (all async DFIRs have run, hooks have been resolved), compute a **partial state fingerprint**:

```rust
struct StateFingerprint {
    /// Hash of: for each hook, the types/count of pending messages.
    /// NOT the message contents (too expensive, and partial is sufficient).
    pending_message_shape: u64,
    /// Hash of: which hooks can_make_nontrivial_decision().
    enabled_hooks: u64,
}
```

This is the "partial-state caching" from the paper. We hash:
1. For each hook: number of pending items (not their values)
2. Which hooks are enabled (have pending items)

This is cheap to compute and sufficient to detect cycles in `sample_every` patterns where the same message keeps arriving.

### The Algorithm

```
loop {
    run scheduler normally (pick hooks, make decisions, run ticks)
    
    if quiescent (no hook can make progress):
        // Normal termination — same as today
        break
    
    if all hooks have been given a chance but system is still hot:
        compute fingerprint F
        if F was seen before at position i in the trace:
            // Potential lasso found
            let cycle = trace[i..current]
            if cycle is fair (all enabled hooks were scheduled):
                if cycle is hot (assertion still unsatisfied):
                    // This is a legitimate liveness violation
                    // The test PASSES — the system has a valid execution
                    // where the network drops everything and the assertion
                    // is never satisfied. This is expected for lossy.
                    //
                    // Wait — this is backwards. Let me reconsider.
```

**Wait.** I need to reconsider the semantics. The question is: what does a liveness *test* assert?

### Correcting the Semantics

There are two modes:

**Mode A: "Does this protocol guarantee liveness?"** (what the examples want)
- The test asserts: "under fair scheduling, the output eventually appears"
- A hot fair lasso = the protocol is BROKEN (test fails)
- No hot fair lasso found after exhaustive search = protocol is LIVE (test passes)

**Mode B: "Can the network legitimately prevent liveness?"** (single-send case)
- The test asserts the same thing
- But for a single send with no retry, the simulator finds an execution where the message is dropped and the system reaches quiescence without the assertion being satisfied → test fails

So the semantics are:
- **Test passes** if: in ALL explored executions, the assertion is eventually satisfied (possibly after the scheduler forces the lasso to break by delivering a message)
- **Test fails** if: there EXISTS an execution where the assertion is never satisfied AND that execution is fair

### How Lasso Detection Enables Termination

The problem with `sample_every` + lossy is that the exhaustive search never terminates: the sender keeps producing messages, the hook keeps having items to release, so the scheduler never reaches quiescence.

**Solution: When a lasso is detected, the scheduler knows the system is cycling.** At that point:

1. If the lasso is **hot** (assertion unsatisfied) and **fair** (all enabled hooks were scheduled in the cycle): this execution demonstrates a liveness violation. For a *liveness test*, this means the test should fail.

2. If the lasso is hot but **unfair** (some enabled hook was never scheduled in the cycle): the lasso is spurious. The scheduler should **break the cycle** by forcing the starved hook to make a nontrivial decision (deliver a message). This is exactly what `force_nontrivial` already does.

3. If no lasso is detected within a bound: truncate the execution (same as the paper's max-steps bound B).

### For the `sample_every` + lossy pattern:

1. Sender produces message M₁. Lossy hook buffers it.
2. Scheduler gives lossy hook a turn. It non-deterministically releases 0 items (a drop).
3. Sender produces message M₂ (same value, from `sample_every`). Lossy hook buffers it.
4. Scheduler gives lossy hook a turn. It releases 0 items again.
5. State fingerprint: "lossy hook has 1+ pending items, assertion unsatisfied" — same as step 2.
6. **Lasso detected.** Is it fair? Yes — the lossy hook was scheduled. Is it hot? Yes — assertion unsatisfied.
7. But wait: the lossy hook was scheduled and *chose* to drop. That's a valid scheduling. So this IS a fair hot lasso.

**Problem:** This means `sample_every` + lossy would FAIL, which contradicts the design intent.

### The Missing Piece: What "Fair" Means for Non-Deterministic Hooks

The paper's fairness says: every enabled process must be *scheduled*. But in our system, a hook being "scheduled" doesn't mean it delivers — it means it gets to make a decision, and that decision might be "release 0 items."

**The key insight from the original design doc:** "dropping another message doesn't change how the system behaves if the system state hasn't changed." This is the lasso condition — but the fairness constraint should be: **if a hook is continuously enabled with the same pending state, it must eventually make a *nontrivial* decision (deliver something).**

This is **weak fairness** (WF) from TLA+: if an action is continuously enabled, it must eventually be taken. "Continuously enabled" = the hook keeps having pending items. "Eventually taken" = the hook eventually delivers at least one.

### Revised Algorithm

The scheduler tracks, for each lossy hook, whether it has made a nontrivial decision (delivered ≥1 message) since the last state fingerprint match. If a potential lasso is detected:

1. Check if all lossy hooks with pending items delivered at least once during the cycle.
2. If YES → fair hot lasso → liveness violation (test fails for that execution).
3. If NO → unfair lasso → force the starved hook to deliver, continue execution.

For `sample_every` + lossy:
- Cycle detected: lossy hook was scheduled but always chose to drop.
- Fairness check: the hook never delivered → **unfair** (it was enabled but never took the "deliver" action).
- Scheduler forces delivery → message gets through → assertion satisfied → test passes.

For single-send + lossy:
- Message arrives once. Hook drops it. Buffer is now empty.
- No more messages arrive. System reaches normal quiescence (no hook has pending items).
- Assertion unsatisfied → test fails. ✓

For retry-with-ack + lossy:
- Same as `sample_every`: retries keep feeding the hook, lasso detected, fairness forces delivery. ✓

## Implementation Plan

### Phase 1: Lossy Hook + Lasso Detection in Scheduler

**File: `hydro_lang/src/sim/runtime.rs`**

No new hook type needed. The existing `StreamHook` already supports releasing 0 items. The only change: mark hooks as "lossy" so the scheduler knows to apply fairness constraints to them.

Add a trait method:
```rust
pub trait SimHook {
    // ... existing methods ...
    
    /// Whether this hook represents a lossy channel subject to fairness constraints.
    /// When true, the scheduler will force delivery if a lasso is detected where
    /// this hook was enabled but never delivered.
    fn is_lossy(&self) -> bool { false }
}
```

**File: `hydro_lang/src/sim/compiled.rs`**

Modify the scheduler loop. After each round of hook resolution, if any lossy hook exists with pending items:

1. Compute a partial state fingerprint (hash of: which hooks have pending items + count).
2. Record it in a trace.
3. If the fingerprint matches a previous entry:
   - Extract the cycle.
   - Check fairness: did every lossy hook with pending items deliver at least once in the cycle?
   - If unfair: force the starved lossy hook(s) to deliver on the next round (set `force_nontrivial = true` specifically for them).
   - If fair and hot: this execution has a legitimate liveness violation — declare quiescence (the test will fail because the assertion is unsatisfied).

**File: `hydro_lang/src/sim/builder.rs`**

Replace the `todo!()` at line 1205. Wire up the network the same way as `FailStop` (unbounded channel), but tag the resulting hook as lossy.

### Phase 2: State Fingerprinting

Start with the simplest fingerprint: `(number_of_lossy_hooks_with_pending_items, total_pending_count)`. This is sufficient for `sample_every` patterns where the pending count stabilizes.

If false positives occur (fingerprint matches but system isn't actually cycling), add the paper's replay mechanism: attempt to replay the cycle's scheduling decisions. If replay diverges, discard the potential lasso and continue.

### State Space Impact

**Critical point:** The lossy hook does NOT add new branching decisions to the exhaustive search. It uses the same `StreamHook` with the same `(0..=N)` range for how many items to release. The only new cost is:
- Computing fingerprints (O(number of hooks) per quiescence point)
- Storing the trace (bounded by max-steps)

The exhaustive search terminates because:
- For finite inputs (no `sample_every`): same as today, buffers eventually empty.
- For infinite inputs (`sample_every`): lasso detection triggers, either forcing delivery (breaking the cycle) or declaring quiescence (if the lasso is fair and hot).

## Summary of Changes

| File | Change |
|------|--------|
| `hydro_lang/src/sim/runtime.rs` | Add `fn is_lossy(&self) -> bool` to `SimHook` trait (default false) |
| `hydro_lang/src/sim/compiled.rs` | Add lasso detection to scheduler loop; track fingerprints; force delivery on unfair lassos |
| `hydro_lang/src/sim/builder.rs` | Replace `todo!()` with same wiring as FailStop but mark hook as lossy |
| `hydro_lang/src/sim/tests/liveness.rs` | Remove `#[ignore]` annotations |

## Open Questions

1. **Fingerprint precision:** The paper shows that partial-state caching with just message-type counts works well in practice. Should we hash message contents too? Probably not initially — it's expensive and the paper shows partial hashing with replay is robust.

2. **Max-steps bound:** For `sample_every` patterns, how many ticks should we allow before declaring "no lasso found, truncating"? The paper uses 500 steps. We could use a similar default.

3. **`source_interval` in sim:** Does `source_interval` actually produce messages in the simulator today? If not, that's a prerequisite — the `sample_every` tests need the interval to fire. Looking at the code, it uses `tokio::time::interval` which requires `tokio::time::pause()` or similar in the sim runtime.

4. **Interaction with exhaustive search:** The exhaustive search explores all possible decisions via bolero. With lasso detection, some branches will be pruned (forced delivery). Does this affect completeness? No — the forced delivery is semantically correct (it's what a fair scheduler would do), so pruning unfair executions doesn't miss real bugs.
