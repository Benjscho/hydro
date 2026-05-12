# Liveness Testing: Design Doc

## Summary

Add liveness testing to the Hydro simulator by implementing lossy network channels with fairness-based lasso detection. The simulator detects when the system is cycling without progress and forces message delivery to break the cycle, guaranteeing termination for protocols that retry.

## Background

The simulator currently supports two network fault models:
- **`FailStop`**: Reliable delivery (direct unbounded channel)
- **`LossyDelayedForever`**: Models drops as infinite delays, safety-only testing

The **`Lossy`** fault model (`TCP.lossy(nondet!(...))`) is declared in the type system but panics with `todo!()` in `builder.rs`. A naive implementation that allows dropping all packets would make any liveness assertion fail.

## Design

### Core Insight

Dropping another message doesn't change system behavior if the system state hasn't changed. If the sender will re-send the same value, dropping it again teaches us nothing new. The simulator should detect this cycling and force delivery.

### Fairness Model: Weak Fairness via Lasso Detection

Adapted from TLA+ weak fairness (WF) and the P# cycle detection approach:

- **Weak fairness**: If a lossy hook is *continuously enabled* (has pending messages from a retrying sender), it must eventually deliver at least one message.
- **Lasso**: A repeated state fingerprint indicating the system is cycling. A lasso is **fair** only if every continuously-enabled lossy hook delivered at least once during the cycle. An **unfair** lasso triggers forced delivery.

### How It Maps to Hydro's Simulator

| Concept | Hydro equivalent |
|---------|-----------------|
| State | Fold/singleton values + pending messages in all buffers |
| Process | A hook that `can_make_nontrivial_decision()` |
| Scheduled | Which hook was chosen to make a nontrivial decision |
| Hot | Test assertion not yet satisfied |
| Fair lasso | Cycle where every lossy hook with pending items delivered ≥1 |

### Algorithm

```
loop {
    run scheduler normally (pick hooks, make decisions, run ticks)

    if quiescent (no hook can make progress):
        break  // normal termination

    if lossy hooks exist with pending items:
        compute fingerprint F = hash(pending_counts_per_hook, enabled_hooks)
        if F seen before at position i:
            cycle = trace[i..current]
            if every lossy hook with pending items delivered ≥1 in cycle:
                // Fair hot lasso → genuine liveness violation
                break  // test will fail (assertion unsatisfied)
            else:
                // Unfair lasso → force starved hooks to deliver
                set force_nontrivial on starved lossy hooks
                continue

    if steps > max_steps_bound:
        break  // truncate (same as paper's bound B)
}
```

### Expected Behavior

| Pattern | Result |
|---------|--------|
| `sample_every` + lossy send | PASS — retries feed the hook, unfair lasso detected, delivery forced |
| Single send + lossy (no retry) | FAIL — message dropped, buffer empties, normal quiescence, assertion unsatisfied |
| Retry-with-ack + lossy | PASS — retries keep feeding, fairness forces delivery |

### State Fingerprinting

Start with a minimal fingerprint (from P# partial-state caching):

```rust
struct StateFingerprint {
    /// Hash of: for each lossy hook, number of pending items
    pending_counts: u64,
    /// Hash of: which lossy hooks can_make_nontrivial_decision()
    enabled_hooks: u64,
}
```

This is cheap (O(num_hooks) per check) and sufficient for `sample_every` patterns. If false positives occur, add replay confirmation (attempt to replay the cycle's decisions; if replay diverges, discard the lasso).

### Learnings from P# Implementation

P# uses two complementary strategies. We adopt the more principled one (cycle detection) adapted to our hook-based scheduler:

1. **Temperature checking** (simple heuristic): count steps a monitor stays "hot"; flag if threshold exceeded. Useful as a fallback but produces false positives.

2. **Cycle detection** (what we implement): fingerprint state at each step, detect repeated fingerprints, validate fairness of the cycle, confirm via replay. This is sound under the assumption that same fingerprint = same state (with replay as confirmation).

Key P# design choices we adopt:
- **Safety prefix bound**: Skip initial steps before starting fingerprint collection (avoids initialization noise). Default: 0.
- **Replay confirmation**: When a cycle candidate is found, replay it to confirm the system actually repeats. If replay diverges, discard.
- **Fairness = all enabled processes scheduled**: In our case, "scheduled and delivered" for lossy hooks specifically (weak fairness on the deliver action, not just the schedule action).

Key difference from P#: P# monitors are explicit state machines with hot/cold annotations. In Hydro, "hot" simply means the test assertion hasn't yielded expected values yet. No separate monitor infrastructure needed.

### State Space Impact

The lossy hook does NOT add new branching decisions to the exhaustive search. It reuses `StreamHook` with the same `(0..=N)` release range. New costs:
- Computing fingerprints: O(num_hooks) per quiescence point
- Storing trace: bounded by max_steps

Termination guarantee:
- Finite inputs (no `sample_every`): buffers eventually empty, same as today
- Infinite inputs (`sample_every`): lasso detection triggers, either forcing delivery or declaring quiescence

## Task Breakdown

### Task 1: Mark hooks as lossy

**File**: `hydro_lang/src/sim/runtime.rs`

Add to `SimHook` trait:
```rust
fn is_lossy(&self) -> bool { false }
```

No new hook struct needed. `StreamHook` already supports releasing 0 items (a "drop"). The lossy flag tells the scheduler to apply fairness constraints.

### Task 2: Wire up lossy network in SimBuilder

**File**: `hydro_lang/src/sim/builder.rs`

Replace the `todo!()` at line 1205 with the same wiring as `FailStop` (unbounded channel + `StreamHook`), but mark the resulting hook as lossy. The hook is registered with `is_lossy() = true`.

### Task 3: State fingerprinting infrastructure

**File**: `hydro_lang/src/sim/compiled.rs` (new struct, possibly in a new file)

```rust
struct StateFingerprint { pending_counts: u64, enabled_hooks: u64 }

struct LassoDetector {
    trace: Vec<(StateFingerprint, FairnessRecord)>,
    seen: HashMap<StateFingerprint, Vec<usize>>,
    max_steps: usize,
}

struct FairnessRecord {
    /// For each lossy hook index: did it deliver ≥1 item since last fingerprint?
    delivered: Vec<bool>,
}
```

### Task 4: Lasso detection in scheduler loop

**File**: `hydro_lang/src/sim/compiled.rs`

After each round of hook resolution, if any lossy hook has pending items:
1. Compute fingerprint
2. Check for repeated fingerprint in trace
3. If repeated: validate fairness of the cycle
   - Unfair → set `force_nontrivial = true` on starved lossy hooks for next round
   - Fair + hot → declare quiescence (liveness violation found)
4. If max_steps exceeded → declare quiescence (truncate)

### Task 5: Enable liveness tests

**File**: `hydro_lang/src/sim/tests/liveness.rs`

Remove `#[ignore]` annotations from the three test functions.

### Task 6: `source_interval` in sim (prerequisite check)

Verify that `sample_every` / `source_interval` actually produces messages in the simulator. If it uses `tokio::time::interval`, ensure the sim runtime advances virtual time. If not working, this is a prerequisite blocker.

## Open Questions

1. **Max-steps bound**: Default value for truncation. P# uses ~500 steps. Start with 200 and make configurable.

2. **Fingerprint precision**: Start with pending-count-only. Add message-content hashing only if false positives appear in practice.

3. **`source_interval` in sim**: Does virtual time advance correctly? If not, Task 6 becomes a prerequisite implementation task.

4. **Interaction with exhaustive search**: Forced delivery prunes unfair executions. This is semantically correct (unfair executions aren't valid counterexamples) and doesn't affect completeness.
