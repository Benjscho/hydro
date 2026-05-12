# `sample_every` and Time in the Simulator

## Problem Statement

The liveness testing infrastructure (Tasks 1–4) is complete, but the two key liveness tests (`liveness_sample_every_over_lossy`, `liveness_retry_with_ack`) are blocked because `sample_every` / `source_interval` does not produce messages in the simulator.

### Root Cause

`source_interval` creates a `tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(duration))`. This requires tokio's **time driver** to be enabled. The simulator builds its tokio runtime with:

```rust
tokio::runtime::Builder::new_current_thread()
    .build()  // no .enable_time()
    .unwrap()
```

Without `.enable_time()`, any call to `tokio::time::interval` will panic with "time driver not enabled." Even if the time driver were enabled, the simulator's scheduler loop (`LaunchedSim::scheduler`) manually drives execution via `run_tick()` and `yield_now()` — it never yields long enough for tokio to auto-advance paused time.

### How `sample_every` Works Today

For a `Singleton::sample_every(interval, nondet)`:

```rust
let samples = self.location.source_interval(interval, nondet);
sliced! {
    let snapshot = use(self, nondet);
    let sample_batch = use(samples, nondet);
    snapshot.filter_if(sample_batch.first().is_some()).into_stream()
}
```

This creates a `sliced!` block with two inputs:
1. The singleton value (snapshotted into the tick)
2. The interval stream (batched into the tick)

The tick only fires when **both** hooks have pending items. If the interval stream never produces items, the tick never fires, and no samples are ever emitted.

### How the Simulator Scheduler Works

The scheduler loop in `LaunchedSim::scheduler()`:

1. Runs top-level async DFIRs via `dfir.run_tick().await` — these are the "always running" dataflows that consume `source_stream(IntervalStream(...))`.
2. When an async DFIR makes progress, its child ticks become "possibly ready."
3. Ticks are scheduled when their hooks have pending items.
4. The scheduler reaches quiescence when no DFIR makes progress and no tick is ready.

The interval stream is consumed by a top-level async DFIR. If the interval never fires, the async DFIR never makes progress, and the downstream tick (which contains the `sample_every` logic) is never scheduled.

## Time Model: Three Layers

The simulator should support three layers of time assumption, each with different guarantees:

### Layer 1: No Time

Cannot observe or read time. Makes timeouts impossible. Too restrictive for real protocols — almost every distributed system needs some form of timeout or periodic action.

**Not viable** for `sample_every` or `timeout`.

### Layer 2: Relative Time

Can compare durations (A happens before B, this fires "eventually again") but cannot read absolute clock positions. Allows timeouts and periodic actions but doesn't commit to wall-clock positions. **Breaks cyclicity** — time always moves forward, so a system that "waits for a timeout" will eventually get one.

**This is the layer that exhaustive mode should support.** The key semantic: `sample_every(5s)` means "eventually fires again" — not "fires at exactly t=5, t=10, t=15." The duration is a hint for deployment but is abstracted away in the simulator.

In this model:
- Intervals are non-deterministic events subject to weak fairness (they must eventually fire)
- Relative ordering between intervals of different durations is **not** preserved (a 1s interval doesn't necessarily fire more often than a 5s interval in the simulator)
- Timeouts are non-deterministic: they may or may not fire at any given step, but fairness guarantees they eventually fire if continuously enabled

### Layer 3: Absolute Time

Full clock access. Intervals fire at specific wall-clock positions. Multiple intervals respect their relative rates. `Instant::now()` returns meaningful values.

**Supported in fuzz testing only.** The exhaustive explorer cannot handle absolute time because it introduces unbounded non-determinism (what exact instant does the clock read?).

In this model:
- Enable tokio's time driver with `pause()` + auto-advance
- Intervals fire deterministically based on their durations
- Useful for testing timing-sensitive bugs but not for exhaustive verification

## Design for Exhaustive Mode (Layer 2: Relative Time)

### Core Idea

Replace `source_interval` with a **non-deterministic hook** in the simulator. The interval becomes a `StreamHook` that:
1. Is always "enabled" (can always produce an item — time can always advance)
2. Non-deterministically produces 0 or 1 items per scheduler step
3. Is subject to lasso-based weak fairness (must eventually fire)

This is the same model as lossy network hooks: the scheduler non-deterministically decides whether the interval fires, and the lasso detector forces it to fire if the system is cycling without progress.

### Why This Works for Liveness

For `sample_every` + lossy network:
1. The interval hook may or may not fire each step (non-deterministic)
2. When it fires, it produces a sample that feeds into the lossy network hook
3. The lossy hook may drop the sample (non-deterministic)
4. If both hooks keep choosing "don't fire" / "drop," the state fingerprint repeats
5. The lasso detector detects the unfair cycle and forces both to make progress
6. Eventually: interval fires → sample produced → network delivers → assertion satisfied

For single-send (no retry):
1. Message sent once, lossy hook drops it, buffer empties
2. No interval hook exists (no `sample_every`), so no retry mechanism
3. System reaches normal quiescence with assertion unsatisfied → test fails ✓

### Implementation

#### Step 1: Mark interval sources in the IR

Add a variant to `HydroSource` to distinguish interval sources from arbitrary streams:

```rust
enum HydroSource {
    Stream(DebugExpr),
    Iter(DebugExpr),
    ExternalNetwork(),
    Interval(DebugExpr),  // NEW: periodic timer, expr is the Duration
}
```

The `Location::source_interval` method would emit `HydroSource::Interval(duration_expr)` instead of `HydroSource::Stream(IntervalStream::new(...))`.

#### Step 2: Emit an interval hook in the sim builder

When the sim builder encounters `HydroSource::Interval`, instead of emitting `source_stream(IntervalStream::new(...))`, it emits a hook-driven source:

```rust
// Always-replenishing buffer (represents "time can always advance")
let __interval_buf_N = Rc::new(RefCell::new(VecDeque::from([()])));

let (__interval_send_N, __interval_recv_N) = unbounded_channel();

// Hook: non-deterministically fires 0 or 1 times per step
StreamHook {
    input: __interval_buf_N.clone(),
    to_release: None,
    output: __interval_send_N,
    lossy: false,
    is_interval: true,  // subject to fairness, self-replenishing
    ...
}
```

The hook is **self-replenishing**: after releasing an item, it immediately pushes a new item into its buffer. This models the infinite nature of time — there's always a "next tick" available.

#### Step 3: Generalize fairness tracking

Rename/generalize the lasso detector's concept from "lossy hooks" to "fairness-subject hooks." A hook is fairness-subject if `is_lossy() || is_interval()`. The lasso detector treats both the same way:
- Track whether each fairness-subject hook has made a nontrivial decision in the current cycle
- If a cycle repeats and a fairness-subject hook never fired → unfair lasso → force it

Concretely, add to `SimHook`:

```rust
/// Whether this hook is subject to weak fairness constraints.
/// Covers both lossy network hooks and interval/timer hooks.
fn is_fairness_subject(&self) -> bool {
    self.is_lossy() || self.is_interval()
}
```

#### Step 4: Handle the deploy path

For the deploy path (non-sim), `HydroSource::Interval(duration_expr)` lowers to the existing `source_stream(IntervalStream::new(tokio::time::interval(duration_expr)))` — no change in behavior for deployed programs.

### State Space Impact

The interval hook adds one binary decision per scheduler step (fire or don't fire). This is bounded:
- For programs with one interval: 2× branching per step
- Lasso detection terminates cycles where the interval never fires
- In practice, the interval fires early in most branches (the exhaustive explorer tries "fire" first), so the state space increase is modest

### What About the `Duration` Parameter?

In Layer 2 (relative time), the duration is **ignored** by the simulator. All intervals are equivalent: they're non-deterministic events that eventually fire. This is correct because:
- The simulator doesn't model wall-clock time
- "5 seconds" vs "1 second" is a deployment concern, not a correctness concern
- What matters for correctness is: "does the protocol work assuming the interval eventually fires?"

If relative ordering between intervals matters for a specific protocol, that's a Layer 3 concern (fuzz testing with real time).

## Design for Fuzz Mode (Layer 3: Absolute Time)

For fuzz testing, enable real tokio time:

1. Build the runtime with `.enable_time()`
2. Call `tokio::time::pause()` at startup
3. Let tokio auto-advance time when all tasks are waiting

This gives:
- Intervals fire at their specified rates
- Multiple intervals respect relative ordering
- `Instant::now()` returns meaningful (but deterministic) values
- The fuzzer controls non-determinism through other means (message ordering, batching)

No lasso detection needed — the fuzzer has a fixed iteration budget and will terminate regardless.

## Implementation Plan

### Phase 1: IR Change

**Files**: `hydro_lang/src/compile/ir/mod.rs`, `hydro_lang/src/location/mod.rs`

Add `HydroSource::Interval(DebugExpr)` variant. Update `Location::source_interval` to emit it instead of wrapping in `IntervalStream`. Update the deploy builder's lowering of `HydroSource` to emit `source_stream(IntervalStream::new(tokio::time::interval(expr)))` for the `Interval` variant — preserving existing deploy behavior.

### Phase 2: Sim Builder — Interval Hook

**Files**: `hydro_lang/src/sim/builder.rs`, `hydro_lang/src/sim/runtime.rs`

When the sim builder encounters `HydroSource::Interval`:
- Emit a self-replenishing `StreamHook<(), TotalOrder>` with `is_interval: true`
- Buffer starts with one `()` item (representing "the next tick is available")
- Hook non-deterministically releases 0 or 1 items per step
- After releasing 1, immediately replenish the buffer to 1

Add `is_interval: bool` field to `StreamHook`. The `autonomous_decision` for an interval hook:
- If `force_nontrivial`: release 1, replenish
- Otherwise: non-deterministically release 0 or 1; if 1, replenish

### Phase 3: Generalize Lasso Detection

**Files**: `hydro_lang/src/sim/runtime.rs`, `hydro_lang/src/sim/compiled.rs`

- Add `fn is_fairness_subject(&self) -> bool` to `SimHook` (default: `is_lossy()`)
- Override in `StreamHook` to return `self.lossy || self.is_interval`
- Replace all `is_lossy()` checks in the lasso detector and `run_hooks_with_force` with `is_fairness_subject()`

### Phase 4: Enable Time for Fuzz Mode

**Files**: `hydro_lang/src/sim/compiled.rs`

- In the fuzz-mode runtime builder, add `.enable_time()`
- Call `tokio::time::pause()` at the start of the scheduler in fuzz mode
- The `HydroSource::Interval` variant in the deploy builder already emits `IntervalStream`, so fuzz mode uses real (paused) tokio time

### Phase 5: Remove `#[ignore]` from Liveness Tests

**File**: `hydro_lang/src/sim/tests/liveness.rs`

Once Phases 1–3 are complete, remove `#[ignore]` from `liveness_sample_every_over_lossy` and `liveness_retry_with_ack`.

## Retry Mechanics: Worked Example

For `singleton(123).sample_every(5s).send(lossy)`:

```
[singleton hook: always has 123]  ──┐
                                    ├── tick ── emits 123 ──→ [lossy hook: buffer] ──→ output
[interval hook: buffer=[()]]     ──┘
```

1. Tick is ready (both hooks have items). Scheduler picks it.
2. Interval releases 1 `()`, singleton releases 123. Tick runs → 123 enters lossy buffer. **Interval replenishes to `[()]`.**
3. Scheduler picks lossy hook. Non-deterministically releases 0 (drops 123).
4. Tick is *still* ready — singleton has 123, interval has `[()]`.
5. Scheduler picks tick again → 123 enters lossy buffer. Interval replenishes.
6. Lasso detector: fingerprint = "lossy has 1 pending, interval has 1 pending" — same as step 2.
7. Fairness check: lossy hook never delivered → **unfair lasso** → force delivery.
8. Lossy hook delivers 123 → assertion satisfied ✓

The self-replenishing buffer is what enables the retry: after each tick firing, the interval immediately has another item, so the tick can fire again on the next step, re-emitting the singleton value into the lossy network.

## Open Questions

1. **Self-replenishing semantics**: Should the interval buffer be replenished immediately after release (allowing it to fire again on the very next step), or should there be a one-step delay? Immediate replenishment is simpler and correct for weak fairness.

2. **Interaction with `timeout`**: `Stream::timeout` uses `source_interval` internally. Under Layer 2, a timeout is a non-deterministic event that eventually fires. This means the exhaustive explorer will test both "timeout fires before response" and "response arrives before timeout" — which is exactly what we want for correctness testing.

3. **`source_interval_delayed`**: The initial delay could be modeled as "the interval hook starts with an empty buffer and only becomes self-replenishing after the first scheduler cycle." Or it could be ignored (same as `source_interval` in the sim). The delay is a Layer 3 concern.

4. **Multiple intervals with different durations**: Under Layer 2, all intervals are equivalent. If a protocol's correctness depends on "interval A fires more often than interval B," that's a property that can only be tested in Layer 3 (fuzz mode). This seems acceptable — most protocols don't depend on relative timer rates for correctness.

5. **Exhaustive termination bound**: The lasso detector's max-steps (200) should be sufficient. Each step where the interval fires but the network drops produces the same fingerprint as the previous such step, so the lasso is detected quickly (typically within 2–3 cycles).
