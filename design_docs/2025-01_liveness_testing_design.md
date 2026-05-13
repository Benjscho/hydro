# Liveness Testing: Design & Implementation

## Summary

The Hydro simulator supports liveness testing via lossy network channels with fairness-based lasso detection. The simulator detects when the system is cycling without progress and forces message delivery to break the cycle, guaranteeing termination for protocols that retry.

**Key files:**
- `hydro_lang/src/sim/compiled.rs` — `LassoDetector`, `StateFingerprint`, scheduler loop
- `hydro_lang/src/sim/runtime.rs` — `StreamHook` (lossy mode), `IntervalHook`
- `hydro_lang/src/sim/builder.rs` — lossy network wiring, interval source emission
- `hydro_lang/src/sim/tests/liveness.rs` — test suite

## How It Works

### Core Mechanism

1. **Lossy hooks** (`StreamHook` with `lossy: true`) non-deterministically deliver or drop one message at a time (binary choice per item).
2. **Interval hooks** (`IntervalHook`) non-deterministically fire or don't fire (binary choice).
3. Both are **fairness subjects** — the lasso detector tracks whether they've delivered/fired.
4. The **lasso detector** fingerprints the system state at each scheduling decision. When a fingerprint repeats:
   - If all enabled fairness-subject hooks delivered during the cycle → **fair lasso** → declare quiescence (liveness violation if test assertion unsatisfied)
   - If some enabled hooks never delivered → **unfair lasso** → force those hooks to deliver
5. After force delivery, the lasso detector is **not reset**. On the next cycle, the same fingerprint appears with all hooks having delivered → fair lasso → quiescence. The test assertion is then checked.

### State Fingerprinting

```rust
struct StateFingerprint {
    pending_counts: u64,  // hash of pending_count().min(1) per fairness-subject hook
    enabled_hooks: u64,   // hash of can_make_nontrivial_decision() per hook
}
```

Using `min(1)` collapses buffer sizes ≥1 into the same abstract state, ensuring the fingerprint stabilizes quickly regardless of how many items accumulate.

### Lossy Hook Decision Model

The lossy `StreamHook` makes a **constant-branching** decision:
- Drain exactly 1 item from the buffer
- Binary choice: deliver it or drop it

When `force_nontrivial = true` (lasso forcing delivery), all buffered items are delivered deterministically with no branching.

This keeps the exhaustive state space bounded. The previous design (drain 1..=N items, then deliver 0..=drained) created branching proportional to buffer size, causing state space explosion.

### Force Delivery Flow

When the lasso detects an unfair cycle:
1. Set `force_delivery_targets` to the starved hooks
2. If the forced observation has items → resolve immediately with forced delivery (all items delivered, no branching)
3. If the forced observation's buffer is empty → deterministically resolve other observations (e.g., fire the IntervalHook) or run ticks to feed data to it
4. After force delivery succeeds, do NOT reset the lasso detector
5. On the next scheduling step, the fingerprint repeats → fair lasso (all hooks delivered) → quiescence declared
6. Test assertion is checked; if the forced delivery satisfied it, the test passes

### Scheduler Loop (simplified)

```
loop {
    run all async DFIRs until no progress

    partition observations/ticks into ready vs not-ready

    if nothing ready:
        signal quiescence → test assertion checked
        wait for new input (or end)

    if fairness-subject hooks exist and no pending force targets:
        lasso_detector.step(hooks) → Continue | ForceDelivery | LivenessViolation | Truncated

    if force_delivery_targets non-empty:
        if forced observation has items → resolve with forced delivery
        else → resolve other observations/ticks deterministically to feed data
        continue

    branch via any() → pick a tick or observation to resolve
    resolve hooks (each makes autonomous decisions via driver)
    continue
```

### Expected Behavior

| Pattern | Result |
|---------|--------|
| `source_interval` + lossy send | PASS — interval feeds the hook, unfair lasso detected, delivery forced |
| Single send + lossy (no retry) | FAIL — message dropped, buffer empties, normal quiescence, assertion unsatisfied |
| Retry-with-ack + lossy | PASS — retries keep feeding, fairness forces delivery |

### Configuration

- `max_lasso_steps(n)` — maximum fingerprint trace length before truncation (default: 200). For simple liveness tests, 5 is sufficient. The lasso typically detects within 2-3 steps.

## Test Suite

**File**: `hydro_lang/src/sim/tests/liveness.rs`

- `liveness_single_send_over_lossy_fails` — `#[should_panic]`, verifies that a single send over lossy fails with "Stream ended early" (not a compilation error)
- `liveness_sample_every_over_lossy` — `source_interval` → `map` → `send(lossy)`, asserts delivery via fairness
- `liveness_retry_with_ack` — same pattern with different payload, demonstrates retry protocol

All three tests complete in ~19s total (including trybuild compilation).

## Implementation Details

### Hooks and Fairness Subjects

The `SimHook` trait has:
- `is_fairness_subject()` — returns true for lossy `StreamHook`s and `IntervalHook`s
- `pending_count()` — used by fingerprinting; `IntervalHook` always returns 1
- `can_make_nontrivial_decision()` — `IntervalHook` always true; lossy `StreamHook` true when buffer non-empty

### Lossy Network Wiring (SimBuilder)

For P→P lossy sends:
- Sender's async DFIR serializes and sends to an unbounded channel
- Receiver's async DFIR reads from the channel into a `VecDeque` buffer
- A lossy `StreamHook` on the receiver's location controls release from the buffer
- The hook is registered as a fairness subject

### IntervalHook (source_interval in sim)

- Duration is ignored (sim uses abstract time)
- Creates an `IntervalHook` that non-deterministically fires or doesn't fire
- When it fires, sends `()` to a channel that the async DFIR reads via `source_stream`
- Marked as a fairness subject so the lasso forces it to fire if the system is cycling

### Key Design Decisions

1. **No lasso reset after force delivery** — prevents infinite force-deliver-reset cycles. After delivery, the next cycle is detected as fair → quiescence.
2. **Constant branching in lossy hooks** — drain 1 item, binary deliver/drop. Prevents state space explosion from buffer accumulation.
3. **Resolve other observations during force delivery** — when the forced hook's buffer is empty, fire other hooks (e.g., IntervalHook) to feed data to it, rather than only handling ticks.
4. **`pending_count().min(1)` in fingerprint** — abstracts buffer size to empty/non-empty, ensuring fingerprints stabilize quickly.

## Resolved Issues

### Compilation error in test (std::iter::once)

`std::iter::once(123_u32)` produces type `core::iter::sources::once::Once<u32>` which references a private module in generated staged code. Fixed by using `vec![123_u32]` in `source_iter`.

### State space explosion from lossy hook branching

The original lossy hook drained `(1..=N)` items then delivered `(0..=drained)`, creating O(N²) branches. Fixed by draining exactly 1 item with a binary deliver/drop decision.

### Infinite force-deliver-reset loop

After force delivery, the lasso detector was reset. The interval would fire again, the lasso would detect again, force again, reset again — never reaching quiescence. Fixed by not resetting the detector; the next cycle is fair → quiescence declared.

### Force delivery with empty buffer

When the forced lossy hook's buffer was empty and there were no ticks to run, the scheduler looped without progress. Fixed by also resolving other observations (e.g., IntervalHook) to feed data to the forced hook.
