# PSharp Liveness Checking: Implementation and Soundness

## Overview

PSharp checks **liveness properties** — specifications that "something good eventually happens" — during systematic testing. Unlike safety properties (bad things never happen), liveness violations manifest as infinite executions where the system gets stuck in an undesirable state forever. Since testing explores finite executions, PSharp uses two complementary techniques to detect potential infinite loops:

1. **Temperature Checking** — a simple heuristic that flags monitors stuck in a "hot" state for too long.
2. **Cycle Detection** — a state-caching approach that identifies actual repeated program states forming a fair cycle where a liveness monitor remains perpetually hot.

---

## Core Concepts

### Monitor States: Hot and Cold

Liveness properties are expressed as **monitors** — state machines that observe program events without affecting execution. Monitor states are annotated with temperature attributes:

- **`[Hot]`** — The liveness obligation is *not* currently satisfied. The system is in a state where "something good" still needs to happen (e.g., a request is pending but not yet served).
- **`[Cold]`** — The liveness obligation *is* satisfied. The system has made progress (e.g., the request was served).
- **No annotation** — Neutral; neither violating nor satisfying the property.

A liveness violation occurs when a monitor remains in a hot state *forever* under a fair execution — meaning the system is stuck and will never make the required progress.

### Example

```csharp
class WatchDog : Monitor
{
    [Start]
    [Cold]
    [OnEventGotoState(typeof(Waiting), typeof(CanGetUserInput))]
    [OnEventGotoState(typeof(Computing), typeof(CannotGetUserInput))]
    class CanGetUserInput : MonitorState { }

    [Hot]
    [OnEventGotoState(typeof(Waiting), typeof(CanGetUserInput))]
    [OnEventGotoState(typeof(Computing), typeof(CannotGetUserInput))]
    class CannotGetUserInput : MonitorState { }
}
```

This monitor asserts: "whenever the system starts computing, it must eventually return to waiting for user input." If the monitor stays in `CannotGetUserInput` (hot) forever, the liveness property is violated.

---

## Strategy 1: Temperature Checking

**File:** `TemperatureCheckingStrategy.cs`

### Mechanism

The simplest approach. Each monitor maintains a `LivenessTemperature` counter:

1. At every scheduling step, if the underlying strategy is **fair**, call `CheckLivenessTemperature()` on each monitor.
2. If a monitor is in a hot state, increment its temperature counter.
3. If the counter exceeds `LivenessTemperatureThreshold`, report a potential liveness bug.
4. When a monitor transitions to a cold state, reset its temperature to 0.

### When It Activates

Temperature checking only fires when the nested scheduling strategy reports `IsFair() == true`. This is critical: unfair strategies (like DFS or PCT) can starve machines, creating artificial hot-state durations that don't represent real bugs. Fair strategies (like `RandomStrategy`) guarantee that every enabled operation is eventually scheduled, so prolonged hot states are meaningful.

### Intuition for Soundness

This is a **heuristic**, not a proof of a bug. The reasoning is:
- Under a fair scheduler, every enabled machine gets to run infinitely often.
- If a monitor has been hot for N steps (where N is large), it's increasingly unlikely that the system will ever escape the hot state — the fair scheduler has given every machine many chances to make progress, and none did.
- The threshold is typically set to half the max fair scheduling steps (e.g., 500 steps out of 1000).

**Limitation:** This can produce false positives. A system might legitimately need many steps to satisfy a liveness obligation. The threshold is a tunable parameter trading precision for recall.

---

## Strategy 2: Cycle Detection

**File:** `CycleDetectionStrategy.cs`

### Mechanism

A more principled approach that detects actual infinite loops by finding repeated program states.

#### Phase 1: State Fingerprinting and Cycle Identification

1. At each scheduling step (after a configurable `SafetyPrefixBound`), capture a **fingerprint** of the entire program state.
2. The fingerprint is a hash combining:
   - Each machine's state (current state in the state machine, inbox contents, halted status)
   - Each machine's operation type (send, receive, etc.)
   - Each monitor's state
3. Maintain a map from fingerprints to the schedule step indices where they occurred.
4. When a fingerprint is seen **again**, the trace segment between the two occurrences is a **potential cycle** — the program may be repeating the same sequence of steps forever.

#### Phase 2: Fairness Validation

Not every cycle represents a liveness bug. The cycle must be **fair** to constitute a valid counterexample:

- **Scheduling fairness (`IsSchedulingFair`):** Every machine that was *enabled* at any point during the cycle must have been *scheduled* at least once. This ensures no machine is being starved — if a machine could have made progress but was never given a chance, the cycle is an artifact of unfair scheduling, not a real bug.

- **Nondeterminism fairness (`IsNondeterminismFair`):** For every `FairRandom()` choice point in the cycle, both `true` and `false` outcomes must appear. This prevents false positives from cycles where a fair coin always lands on the same side.

If the first candidate cycle (from the most recent repeated fingerprint) fails fairness checks, the strategy randomly samples up to 3 other cycle start points from the fingerprint history.

#### Phase 3: Hot Monitor Detection

Once a fair cycle is found, check if any monitor is in a hot state throughout the *entire* cycle and never visits a cold state. These are the `HotMonitors` — monitors whose liveness property is violated by this cycle.

If no monitors are perpetually hot, the cycle is benign (the system is looping but making progress on all liveness obligations).

#### Phase 4: Cycle Replay and Confirmation

If hot monitors exist:

1. Switch to **replay mode** (`IsReplayingCycle = true`).
2. Force the scheduler to re-execute the exact sequence of scheduling choices and nondeterministic decisions from the detected cycle, looping back to the start when reaching the end.
3. At each step during replay, verify:
   - The hot monitors haven't transitioned to a cold state (if they do, the cycle was spurious — `EscapeUnfairCycle()`).
   - The program states visited during replay still match the fingerprints from the original cycle (if a new state appears, the cycle has been broken — `EscapeUnfairCycle()`).
4. Increment `LivenessTemperature` each step. The threshold is set to `10 × cycle_length`.
5. If the temperature exceeds the threshold (i.e., the cycle has been replayed ~10 times without escaping), report a definitive liveness violation.

### Intuition for Soundness

The cycle detection approach is sound under these assumptions:

1. **State fingerprint accuracy:** If two states have the same fingerprint, they are (with high probability) the same state. The hash combines machine states, operation types, inbox contents, and monitor states. Hash collisions are possible but unlikely in practice.

2. **Determinism of replay:** Given the same scheduling choices and nondeterministic decisions, the program produces the same state transitions. This is guaranteed by PSharp's controlled execution — all concurrency is mediated through the scheduler.

3. **Fairness implies inevitability:** If a cycle is fair (all enabled machines run, all fair coins flip both ways), then any behavior that *could* break the cycle *would* have broken it. Since the cycle repeats with the same state, the same machines are enabled, and they make the same choices — the system is genuinely stuck.

4. **Replay confirmation eliminates false positives:** Even if the fingerprint-based cycle detection has a false match (hash collision), the replay phase will detect divergence (new states appearing, monitors escaping hot states) and abort. The 10× replay threshold provides high confidence that the cycle is genuine.

The key insight is: **a fair cycle through program states where a liveness monitor is always hot constitutes a valid counterexample to the liveness property.** It demonstrates an infinite fair execution (by repeating the cycle forever) where the "good thing" never happens.

---

## Architecture

```
┌─────────────────────────────────────────────────────┐
│              BugFindingScheduler                      │
│  (asks strategy for next operation at each step)     │
└──────────────────────┬──────────────────────────────┘
                       │
         ┌─────────────┴─────────────┐
         │                           │
┌────────▼─────────┐    ┌───────────▼────────────┐
│ TemperatureCheck │    │  CycleDetectionStrategy │
│    Strategy      │    │                         │
│                  │    │  ┌─────────────────┐    │
│  Wraps inner     │    │  │  StateCache     │    │
│  strategy;       │    │  │  (fingerprints) │    │
│  checks temp     │    │  └─────────────────┘    │
│  each step       │    │  ┌─────────────────┐    │
│                  │    │  │  ScheduleTrace   │    │
│                  │    │  │  (step history)  │    │
└────────┬─────────┘    │  └─────────────────┘    │
         │              └───────────┬─────────────┘
         │                          │
         └──────────┬───────────────┘
                    │
         ┌──────────▼──────────┐
         │  Inner Strategy     │
         │  (Random, PCT,      │
         │   DFS, etc.)        │
         └─────────────────────┘
```

Both liveness strategies are **decorators** around an inner scheduling strategy. They intercept each scheduling decision to perform liveness checks, then delegate the actual scheduling choice to the inner strategy (unless replaying a cycle).

---

## Configuration Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `EnableLivenessChecking` | `true` | Master switch for liveness checking |
| `EnableCycleDetection` | `false` | Use cycle detection instead of simple temperature |
| `LivenessTemperatureThreshold` | `0` (disabled) | Steps in hot state before reporting bug. Auto-set to `MaxFairSteps/2` or `100` (cycle detection) |
| `SafetyPrefixBound` | `0` | Number of initial steps to skip before starting state caching (avoids initialization noise) |

---

## Comparison of Approaches

| Aspect | Temperature Checking | Cycle Detection |
|--------|---------------------|-----------------|
| Precision | Heuristic (may false-positive) | High (state-based proof) |
| Cost | O(1) per step | O(n) memory for state cache |
| Requirements | Fair inner strategy | Any strategy (self-validates fairness) |
| False negatives | Possible if threshold too high | Possible if hash collides or cycle too long |
| Typical use | Quick testing with fair schedulers | Thorough verification with DFS/systematic exploration |

---

## Why This Works: The Theoretical Foundation

Liveness checking in PSharp is grounded in the theory of **ω-regular properties** and **fair cycle detection** from model checking:

1. A liveness property "P eventually holds" is violated iff there exists an infinite execution where P never holds.
2. In a finite-state system, any infinite execution must eventually cycle (pigeonhole principle).
3. Therefore, a liveness violation exists iff there is a **reachable fair cycle** where the liveness monitor is always hot.
4. PSharp's cycle detection directly searches for such cycles during systematic exploration.

The temperature-based approach approximates this: under a fair scheduler, if the system hasn't escaped a hot state after many steps, it's likely stuck in such a cycle even if we haven't explicitly identified it.

Both approaches reduce the undecidable problem of checking infinite behaviors to a decidable check on finite executions — either by bounding (temperature) or by exploiting the finite-state nature of the system under test (cycle detection).

