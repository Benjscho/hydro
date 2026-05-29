# RFC: Liveness Testing in the Hydro Simulator

## Summary

Add liveness verification to Hydro's exhaustive simulator. Liveness means "something good eventually happens" — the dual of safety ("nothing bad ever happens"). The core mechanism is a lasso-based weak fairness detector that identifies when the system is stuck in a cycle without making progress, independent of *why* it's stuck. Lossy networks are one important application, but the mechanism is general.

## Motivation

The simulator currently verifies safety: it explores all possible schedules and checks that no execution reaches a bad state. It cannot verify liveness: that every execution *eventually* reaches a good state.

Liveness bugs are common in distributed systems and manifest in many forms:

| Bug class | Example | What "good thing" must happen |
|-----------|---------|-------------------------------|
| **Message loss without retry** | Single send over unreliable network | Message eventually delivered |
| **Deadlock** | Two processes each waiting for the other's message | At least one process makes progress |
| **Livelock** | Processes keep exchanging messages but never commit | Protocol reaches a decision |
| **Starvation** | One node never gets elected leader or assigned work | Every node eventually participates |
| **Non-convergence** | CRDT gossip that never reaches consistent state | All replicas eventually agree |
| **Non-termination** | Fixed-point iteration that never stabilizes | Computation produces final output |
| **Drain failure** | Pipeline items cycle between stages forever | All items eventually reach output |
| **Request completion** | Client request stuck in routing/queuing | Response eventually produced |

All of these share a common structure: the system enters a cycle where it appears to be doing work (or waiting) but never produces the expected output.

### Why the simulator needs this

Without liveness support:
- `fail_stop()` channels are fully reliable — they don't test whether a protocol *recovers* from faults
- `lossy_delayed_forever()` only tests safety (no assertion on eventual delivery)
- There's no way to assert "this fold eventually reaches value X" or "this output stream eventually produces an item"
- Deadlocks manifest as tests that hang forever rather than producing a clear failure

### What liveness testing asserts

A liveness test says: "assuming fair scheduling (every continuously-enabled component eventually runs), does the system eventually satisfy this assertion?"

- If YES in all executions → the protocol guarantees progress (test passes)
- If NO in some fair execution → the protocol has a liveness bug (test fails)
- If the system reaches quiescence without the assertion satisfied → the protocol is broken for that execution (test fails for that path)

## Design

### Core Mechanism: Lasso Detection

We use lasso detection as the core mechanism for liveness testing. The distinction between fair and unfair scheduling originates in fair stateless model checking ([Musuvathi & Qadeer, PLDI 2008](https://www.microsoft.com/en-us/research/publication/fair-stateless-model-checking/)). Building on this, lasso detection — identifying repeated system states to detect infinite cycles — has been used in P#/Coyote ([Mudduluru et al., 2017](https://www.microsoft.com/en-us/research/publication/lasso-detection-using-partial-state-caching/)) for finding liveness violations in asynchronous distributed systems via partial-state caching, and for checking liveness of Byzantine consensus protocols ([Decouchant et al., 2023](https://arxiv.org/abs/2310.09006)). Our lasso detector is **not specific to network losses** — it detects any repeated system state where progress has stalled.

Our design maps onto these scheduler semantics as follows: the simulator's non-deterministic hooks (e.g., lossy network, reordering) act as an unfair scheduler — they may starve certain delivery paths indefinitely. When the lasso detector identifies an unfair cycle (some enabled hook was never delivered), it forces delivery, analogous to switching from an unfair scheduler to a fair one. A fair lasso — where all hooks have delivered but the system still cycles — corresponds to a genuine liveness violation: the program fails to make progress even under fair scheduling.

The scheduler fingerprints system state after each scheduling step:

```rust
struct StateFingerprint {
    pending_counts: u64,  // hash of pending_count().min(1) per fairness-subject hook
    enabled_hooks: u64,   // hash of can_make_nontrivial_decision() per hook
}
```

Using `min(1)` abstracts buffer size to empty/non-empty, ensuring fingerprints stabilize quickly regardless of message accumulation.

When a fingerprint repeats, the scheduler examines the cycle:

| Cycle type | Condition | Action |
|-----------|-----------|--------|
| **Unfair lasso** | Some enabled hook never delivered during the cycle | Force those hooks to deliver |
| **Fair lasso** | All enabled hooks delivered, still no progress | Declare liveness violation (quiescence) |
| **Truncated** | Max steps exceeded without cycle detection | Declare inconclusive (quiescence) |

After force delivery, the lasso detector is **not reset**. The next cycle will be detected as fair → quiescence → test assertion checked.

### Fairness Model: Weak Fairness (WF)

Inspired by TLA+ and P#. If a hook is *continuously enabled* (has pending items or can fire), it must *eventually take a nontrivial action* (deliver or fire).

This is enforced by the lasso detector, not by the hooks themselves. Hooks make autonomous non-deterministic decisions; the scheduler detects when those decisions constitute an unfair cycle and intervenes.

Weak fairness is sufficient for:
- Retry-based protocols (if you keep retrying, the network eventually delivers)
- Timer-driven progress (if a timer is enabled, it eventually fires)
- Any protocol where a continuously-enabled action guarantees progress

Weak fairness does **not** cover:
- Actions that are only intermittently enabled (strong fairness / SF)
- Probabilistic progress guarantees (randomized algorithms)

### Fairness Subjects (Hooks)

Any component whose non-deterministic choices could starve the system is a "fairness subject." The current implementation has three:

**1. Lossy `StreamHook`** (`TCP.lossy(nondet)`)
- Models permanent message loss with no built-in recovery
- Drains exactly 1 item per step, binary deliver/drop decision
- Constant branching (no state space explosion from buffer growth)
- When forced: delivers all buffered items deterministically
- Use case: testing that application-level retry logic works

**2. `IntervalHook`** (`source_interval` / `sample_every`)
- Models time advancement: "a timer eventually fires"
- Always enabled (`can_make_nontrivial_decision() = true`, `pending_count() = 1`)
- Binary fire/don't-fire decision per step
- Models relative time: "eventually fires again" without wall-clock commitment
- When forced: fires once
- Use case: timer-driven progress, periodic retries, timeout-based recovery

**3. `FairLossyHook`** (`TCP.lossy_retry()`)
- Models transient message loss with automatic retry (at-least-once delivery)
- Never permanently drops (items stay in `pending` until delivered)
- NoOrder selection from pending buffer
- Injects bounded duplicates from `delivered` pool (budget K=2 per item)
- When forced: delivers all pending, no forced duplicates
- Use case: modeling real-world channels (gRPC, Kafka, TCP reconnection)

**Extensibility:** Any new hook type can participate in liveness testing by implementing `is_fairness_subject() = true` and the associated trait methods. The lasso detector is hook-agnostic — it only cares about `pending_count()` and `can_make_nontrivial_decision()`.

### How Different Bug Classes Are Caught

**Deadlock/livelock (no fairness subjects involved):**
The system reaches normal quiescence — no hook has pending items, no tick can run. The test assertion is unsatisfied → test fails. No lasso detection needed; the system simply stops.

**Starvation under non-deterministic scheduling:**
If the scheduler's `any()` branching consistently picks one tick over another, the exhaustive explorer will eventually explore the path where the starved tick runs. This is covered by exhaustive search, not fairness.

**Progress blocked by non-deterministic hooks:**
This is where lasso detection activates. The system *could* make progress if the hook chose differently, but the hook keeps choosing the non-progress option. The lasso detector identifies this as an unfair cycle and forces the hook.

**Convergence/termination:**
A fold that should reach a fixed point but doesn't will cycle through the same states. If the cycle involves fairness-subject hooks (e.g., gossip over lossy channels), the lasso detector catches it. If the cycle is purely internal (no hooks involved), the system will either quiesce (assertion fails) or the exhaustive explorer will detect non-termination via its step budget.

### Scheduler Loop

```
loop {
    run all async DFIRs until no progress

    partition ticks/observations into ready vs not-ready

    if nothing ready:
        declare quiescence (Normal or NormalWithDrops)
        wait for new input → test assertion checked here

    if fairness-subject hooks exist and no pending force targets:
        lasso_detector.step(hooks) → Continue | ForceDelivery | LivenessViolation | Truncated

    if force_delivery_targets non-empty:
        resolve forced hooks (deliver all buffered items)
        if forced hook's buffer empty: resolve other hooks to feed data to it
        continue (do NOT reset lasso detector)

    branch via any() → pick a tick or observation to resolve
    resolve hooks (each makes autonomous binary decisions)
    continue
```

### Expected Test Outcomes

| Pattern | Result | Why |
|---------|--------|-----|
| `sample_every` + `lossy` send | PASS | Interval feeds hook, unfair lasso detected, delivery forced |
| Single send + `lossy` (no retry) | FAIL | Message dropped, buffer empties, normal quiescence, assertion unsatisfied |
| Retry-with-ack + `lossy` | PASS | Retries keep feeding, fairness forces delivery |
| Single send + `lossy_retry` | PASS | Channel guarantees delivery via fairness (never permanently drops) |
| Deadlocked processes | FAIL | No hooks enabled, normal quiescence, assertion unsatisfied |
| Livelock with timer-based recovery | PASS | Timer hook forced to fire, breaks the livelock cycle |
| Non-converging gossip over lossy | FAIL (fair lasso) | All hooks delivered but state keeps cycling without reaching agreement |

### Configuration

- `max_lasso_steps(n)` — maximum fingerprint trace length before truncation (default: 200). For simple liveness tests, 5 is sufficient.

## Channel Types

Four TCP failure policies, ordered by strength:

| Channel | Drops messages | Retries | Output type | Liveness guarantee |
|---------|--------------|---------|-------------|-------------------|
| `fail_stop()` | No | N/A | `ExactlyOnce, TotalOrder` | Yes (trivially) |
| `lossy(nondet)` | Yes (permanent) | No | `ExactlyOnce, TotalOrder` | No |
| `lossy_retry()` | Yes (transient) | Yes (built-in) | `AtLeastOnce, NoOrder` | Yes (fairness) |
| `lossy_delayed_forever()` | Yes (infinite delay) | No | `ExactlyOnce, NoOrder` | No (safety-only) |

## Type System Integration

`lossy_retry()` introduces `AtLeastOnce` and `NoOrder` at the type level:

```rust
// Input
stream: Stream<T, Process<L1>, Unbounded, O, R>

// After .send(&receiver, TCP.lossy_retry().bincode())
received: Stream<T, Process<L2>, Unbounded, NoOrder, AtLeastOnce>
```

This forces downstream consumers to handle duplicates (e.g., via `unique()`, idempotent folds, or CRDTs). The `TcpFailPolicy` trait carries a `RetryGuarantee` associated type that the `send` return type uses.

## Key Design Decisions

1. **Lasso detection is hook-agnostic.** The detector doesn't know about networks, timers, or any specific hook semantics. It only observes `pending_count()` and `can_make_nontrivial_decision()`. This makes it extensible to future hook types without modifying the detector.

2. **Constant branching in lossy hooks.** Drain 1 item, binary deliver/drop. The original design (drain 1..=N, deliver 0..=drained) caused O(N²) state space explosion.

3. **No lasso reset after force delivery.** Prevents infinite force-deliver-reset cycles. After delivery, the next cycle is fair → quiescence declared → assertion checked.

4. **`pending_count().min(1)` in fingerprint.** Abstracts buffer size to empty/non-empty. Without this, fingerprints never repeat when messages accumulate (e.g., `sample_every` keeps producing).

5. **Resolve other observations during force delivery.** When the forced hook's buffer is empty, fire other hooks (IntervalHook) to feed data to it. Without this, the scheduler loops without progress when the interval hasn't fired yet.

6. **Relative time model for intervals.** Duration is ignored in exhaustive mode. `sample_every(5s)` means "eventually fires again." This avoids unbounded non-determinism from absolute clock positions.

7. **Bounded duplicate budget (K=2) for `FairLossyHook`.** Each item can be re-delivered at most K times. Catches exactly-once assumptions and off-by-one dedup bugs without exploding state space.

## Files Changed

| File | Role |
|------|------|
| `sim/runtime.rs` | `SimHook` trait (fairness methods), `StreamHook` (lossy mode), `IntervalHook`, `FairLossyHook` |
| `sim/compiled.rs` | `LassoDetector`, `StateFingerprint`, scheduler loop with force delivery |
| `sim/builder.rs` | Wiring for lossy channels and interval hooks |
| `networking/mod.rs` | `TcpFault::Lossy`, `TcpFault::LossyRetry`, `LossyRetry` policy, `RetryGuarantee` |

## Discarded Alternatives

### Alternative A: Drop stuck execution paths instead of forcing delivery

**Idea:** When the lasso detector finds a cycle, discard the execution path as "stuck" rather than intervening. If no non-discarded path violates the assertion, the test passes.

**Why rejected:**

1. **Cannot prove liveness, only absence of counterexample.** Force delivery proves "under fair scheduling, the protocol makes progress." Dropping proves "we didn't find a path where it fails" — strictly weaker. The test result becomes "liveness unverified" rather than "liveness holds."

2. **State space explosion from discarded paths.** In exhaustive mode, the lossy hook has a binary choice per step. Discarding all drop-only paths means exploring an exponential number of them before finding one that delivers. Force delivery prunes the unfair subtree in O(max_lasso_steps) steps.

3. **Vacuous pass risk.** If all paths that exercise the lossy channel get discarded (because they all cycle), the test passes vacuously — no path satisfied the assertion either. The user gets a green test that proved nothing.

4. **No distinction between "protocol broken" and "scheduler unfair."** A fair lasso (all hooks delivered, still no progress) is a genuine liveness violation. An unfair lasso (some hook starved) is a scheduler artifact. Dropping conflates these — you can't tell the user *why* their test failed.

**When dropping might be appropriate (future work):** For complex multi-hop protocols where force delivery at the hook level doesn't have clear semantics (e.g., the forced hook's buffer is empty and no upstream hook can feed it), dropping with an "inconclusive" warning is a reasonable fallback. This could be added as a degradation path when force delivery fails after N attempts.

### Alternative B: Temperature-based heuristic (P# style)

**Idea:** Instead of detecting exact state cycles, count how many steps the assertion has been unsatisfied. If it exceeds a threshold, declare a liveness violation.

**Why rejected:**

1. **False positives.** A protocol might legitimately need many steps to satisfy a liveness obligation (e.g., multi-phase commit). Any fixed threshold trades precision for recall.

2. **No fairness guarantee.** Temperature checking doesn't distinguish fair from unfair executions. A high temperature might just mean the scheduler starved a hook, not that the protocol is broken.

3. **Not compositional.** The threshold must be tuned per-protocol. Lasso detection is protocol-agnostic — it terminates whenever the system cycles, regardless of how many steps that takes.

P# uses temperature checking as a fast heuristic alongside cycle detection. We chose cycle detection only because the exhaustive explorer already provides deterministic replay, making the heuristic unnecessary.

### Alternative C: Per-message non-deterministic drop with full buffer branching

**Idea:** The lossy hook drains 1..=N items from the buffer, then delivers 0..=drained of them. This models "some messages in a batch get through, others don't."

**Why rejected:**

1. **O(N²) branching per step.** For a buffer of size N, this creates N choices for drain count × (drain+1) choices for deliver count. With `sample_every` continuously feeding the buffer, N grows unboundedly.

2. **Redundant exploration.** From a fairness perspective, "delivered 0 of 3" and "delivered 0 of 5" are the same abstract state (hook enabled, nothing delivered). The binary drain-1-deliver/drop model captures the same fairness-relevant behavior with constant branching.

3. **Fingerprint instability.** With variable buffer sizes, `pending_count()` changes every step, preventing fingerprint matches. The `min(1)` abstraction only works if the hook drains at a bounded rate.

### Alternative D: Reset lasso detector after force delivery

**Idea:** After forcing delivery, reset the fingerprint trace so the system gets a "fresh start" to make progress.

**Why rejected:** Creates an infinite loop. For `sample_every` + lossy:
1. Interval fires → message buffered → lasso detects unfair cycle → force delivery
2. Reset detector → interval fires again → message buffered → lasso detects again → force delivery
3. Repeat forever, never reaching quiescence

Without reset, step 2 produces the same fingerprint as step 1 → fair lasso (the hook *did* deliver this time) → quiescence declared → assertion checked. The test terminates.

## Limitations and Future Work

- **Weak fairness only.** No strong fairness (SF) support. Actions that are repeatedly but not continuously enabled are not forced. This is sufficient for retry-based protocols but may miss bugs in protocols with intermittent enabling.

- **No application-state fingerprinting.** The fingerprint only captures hook metadata (pending counts, enabled status), not fold values or application state. A cycle where application state changes but hook state doesn't will not be detected as a lasso. This could be extended by hashing fold/singleton values into the fingerprint.

- **No state-dependent force delivery.** Force delivery delivers all buffered items unconditionally. For complex multi-hop protocols where "force delivery" isn't well-defined at the hook level, this may be insufficient. A future extension could support "drop stuck runs as inconclusive" as a fallback.

- **No absolute time in exhaustive mode.** Relative ordering between intervals of different durations is not preserved. A 1s interval doesn't necessarily fire more often than a 5s interval. Absolute time is fuzz-only.

- **Deadlock detection is implicit.** Deadlocks are caught because the system quiesces with the assertion unsatisfied, not because the detector explicitly identifies a deadlock. A future extension could provide richer diagnostics ("these two processes are waiting on each other").

- **No liveness assertions on intermediate state.** Currently, liveness is checked via `assert_yields` on output streams. There's no way to assert "this fold eventually reaches value X" without routing it to an output. A `assert_eventually` combinator on singletons/folds could be added.

### Open Problem: Competing Fairness Subjects with Different Semantics

Consider a program that races a TCP round-trip against a timeout:

```rust
let input = ... send(TCP.retries()) // round trip to another server
let intervals = ... sleep(5 seconds) // derived from input send
input.merge_unordered(intervals)
    .assume_ordering
    .scan(/* emits a value if it sees the response before the timeout */)
```

Both the lossy network hook (TCP response) and the interval hook (timeout) are fairness subjects. The lasso detector sees both as "enabled with pending items" every step. If the lossy hook keeps dropping, the fingerprint repeats → unfair lasso → force delivery on the lossy hook. But this means the detector *always* resolves the cycle by forcing TCP delivery, never by forcing the timer to fire. The timeout path is never explored as a resolution to the lasso.

The root cause: `IntervalHook` always reports the same fingerprint state (`pending_count = 1`, `enabled = true`) regardless of whether it fires. Firing vs not firing doesn't change the hook-level fingerprint, so the lasso detector can't distinguish "timer was starved" from "timer chose not to fire."

#### Proposed Design: Cascading Resolution

The final design should be a hybridization of techniques, applied in order:

1. **Force lossy operator delivery.** If we can identify a specific lossy hook to force unlossy and that unblocks the simulation, do that (current lasso detection).
2. **Temperature-based detection.** If forcing delivery doesn't unblock, use a "hot state" heuristic — if the system stays in a non-progress state for too long, flag it.
3. **Max depth fallback.** If neither technique resolves, truncate at `max_steps` and declare inconclusive.

#### Approach A: Application-State Fingerprinting

Include application state (fold/singleton values, scan accumulators) in the fingerprint hash. This way, "timer fired, scan saw timeout" produces a *different* fingerprint than "nothing happened," breaking the false cycle.

**What changes:**
- `StateFingerprint::compute` hashes the current values of all folds/singletons/scans in addition to hook metadata.
- A cycle is only detected when *both* hook state and application state repeat — meaning the system is truly stuck, not just making different kinds of progress.

**Tradeoffs:**
- The fingerprint space grows significantly. Cycles take longer to detect because application state may change for many steps before repeating.
- Requires exposing fold/singleton values to the fingerprint computation, which crosses the current abstraction boundary between hooks and dataflow state.
- May never detect a cycle in programs with monotonically growing state (e.g., a counter), requiring a fallback to the step budget (`max_lasso_steps`).

#### Approach B: Logical Clocks / `HydroInstant`

Instead of simulating absolute time (which creates infinite state spaces), restrict the time API to only allow relative comparisons — never reading a `Duration` as a number.

**Key insight:** The problem with `tokio::time` in exhaustive mode is that absolute time is observable. If a program computes `recv_time - send_time`, the result could be any positive number (1s, 1.1s, 1.2s, ...), making the state space infinite. But most programs only *compare* times: "did the response arrive before the timeout?"

**What changes:**
- Introduce a `HydroInstant` type that only supports relative comparisons (`<`, `>`, `==`) but not subtraction to a `Duration` value.
- Time comparisons like `(recv - req) < 5s` become non-deterministic binary choices in the simulator — the result is true or false, explored exhaustively.
- Observations of local time after a comparison are "tainted" — future comparisons must be consistent with prior non-deterministic decisions (e.g., if we decided `recv > req`, then `recv2 > req` must also hold for any `recv2` after `recv`).
- An unsafe escape hatch allows reading absolute time for programs that genuinely need it, but those programs cannot use exhaustive testing.

**Tradeoffs:**
- Elegant: reduces time to ordering decisions, which compose naturally with the existing non-deterministic hook model.
- Restrictive: programs that compute durations (e.g., "log the latency") can't use exhaustive mode. This is acceptable for correctness testing — you don't need to test logging.
- Complex provenance tracking: the simulator must track which time observations are causally related to maintain consistency across non-deterministic decisions.

#### Approach C: Simulated Time Advancement (Turmoil-style)

Advance tokio time by a fixed `tick_duration` each scheduler step. Since all timing goes through tokio, intervals fire naturally when enough simulated time has passed.

**Reference:** [Turmoil's `tick_duration`](https://docs.rs/turmoil/latest/turmoil/struct.Builder.html#method.tick_duration) does exactly this — each tick advances time by a configurable amount, causing `tokio::time::sleep` and `tokio::time::interval` to fire when their deadline is reached.

**What changes:**
- The simulator advances `tokio::time` by a fixed amount each scheduling step.
- `IntervalHook` no longer makes a non-deterministic fire/don't-fire decision — it fires when simulated time reaches its deadline.
- The non-determinism shifts to *ordering*: which tick runs first when multiple timers fire simultaneously?

**Tradeoffs:**
- Simple to implement if all timing goes through Hydro-controlled code (no raw `tokio::time` calls).
- Makes it harder to enforce fairness via logical clocks — absolute time is now observable, so the state space concern returns.
- The `tick_duration` is a tuning parameter that affects test fidelity. Too large and you miss races; too small and tests are slow.
- Requires assuming (and eventually checking) that there are no manual calls to tokio timing functions outside Hydro's control.

#### MVP Decision

For the first pass, **timers/intervals are allowed in exhaustive mode but may spin unfairly**. The lasso detector cannot currently force a timer to fire (unlike lossy hooks where force-delivery is well-defined), so a timer racing against a network response may be starved indefinitely.

However, we can **detect and report** this situation without automatically resolving it. When the lasso detector identifies an unfair cycle involving a timer hook (the timer was enabled for the entire cycle but never fired), it can emit a diagnostic:

> ⚠ Potential timer unfairness: interval hook was enabled but never fired during a repeated state cycle. The exhaustive explorer cannot automatically resolve timer races. Consider using fuzz testing for this program, or restructuring to avoid timer-vs-network races in liveness-critical paths.

This gives the user actionable information — they know the exhaustive explorer hit a limitation, not that their protocol is broken. The `max_lasso_steps` budget still provides a hard termination bound, and the diagnostic explains *why* it was hit.

The current lasso detection (force lossy delivery) remains sufficient for programs where the only fairness subjects are network hooks. The timer-vs-network race is a second-phase concern, to be addressed by one of the approaches above once the core liveness machinery is proven out.
