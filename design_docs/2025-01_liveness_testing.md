# Liveness Testing in the Hydro Simulator

## Problem Statement

The Hydro simulator currently supports two network fault models:
- **`fail_stop`**: Fully reliable in the simulator (direct unbounded channel)
- **`lossy_delayed_forever`**: Models drops as infinite delays, requires `test_safety_only()`, only tests safety

The **`lossy`** fault model (preserves `TotalOrder`, may drop messages) is declared in the type system but panics with `todo!()` in the simulator. A naive implementation would allow the simulator to drop *all* packets, making any liveness assertion fail.

**Goal**: Support `TCP.lossy(nondet!(...)).bincode()` in the simulator with a fairness constraint that guarantees eventual delivery when the system retries.

## Correctness Guarantees

A liveness test asserts: "assuming the system is live (networks eventually deliver), does the protocol produce the expected output?"

The key insight: **dropping another message doesn't change how the system behaves if the system state hasn't changed**. If the sender will just re-send the same value, dropping it again teaches us nothing new about the system's behavior.

### What Should Pass
- Repeated sends of the same value (e.g., `sample_every` + `send`) → fairness forces delivery
- Retry-with-ack protocols → retries ensure eventual delivery
- CRDT gossip → idempotent state means any single delivery suffices

### What Should Fail
- Single send with no retry → legitimate to drop the only attempt
- Counter-based protocols where each retry has different state → may need stronger guarantees

## Fairness Model

Inspired by TLA+ and P:

### Weak Fairness (WF)
If a network channel is *continuously enabled* (has a pending message that will be re-sent), it must eventually deliver. This is the minimum needed to prevent infinite dropping.

### Lasso Detection
The simulator detects when the system is in a **lasso** (a cycle where state repeats):

1. After each quiescence point, fingerprint the system state:
   - All fold/singleton values
   - Pending messages in network buffers
   - Which hooks have pending decisions

2. If the same fingerprint appears twice during a sequence of "drop" decisions, the system is looping without learning anything new.

3. At that point, **force delivery** of at least one pending network message.

### State Comparison Strategies

**Strategy 1: Network-channel-only** (simpler)
- Only examine pending messages in network hooks
- If all network hooks have the same pending messages as before a drop, force delivery
- Sufficient for `sample_every` + `send` patterns

**Strategy 2: Full state** (more precise)
- Examine all state (folds, singletons, pending messages)
- Detects more subtle cycles but requires state hashing infrastructure
- Needed for stateful retry protocols

## Time Model

Three layers of time assumptions:

1. **No time** — can't observe/read time. Makes timeouts impossible. Too restrictive.
2. **Relative time** — can compare durations but not read absolute clock. Allows timeouts, breaks cyclicity. **Supported in exhaustive mode.**
3. **Absolute time** — full clock access. Supported in fuzz testing only.

The simulator operates in relative time: `sample_every(5s)` means "eventually fires again" without committing to wall-clock positions.

## Implementation Plan

### Phase 1: Network Hook for Lossy Channels
- Add a `LossyNetworkHook` that buffers messages and non-deterministically drops/delivers
- Wire it into `SimBuilder::create_network` for `TcpFault::Lossy`
- The hook implements `SimHook` with `can_make_nontrivial_decision()` returning true when messages are buffered

### Phase 2: Fairness Constraint
- Track system state fingerprints at quiescence points
- When a lasso is detected (state repeats), force the network hook to deliver
- This is the `force_nontrivial` equivalent for network hooks

### Phase 3: Integration with `sample_every`
- Ensure `source_interval` works in the simulator (virtual time ticks)
- The combination of `sample_every` + lossy network + fairness = liveness guarantee

## API Sketch

```rust
let data = sender.source_iter(q!(once(123))).fold(q!(|| 0), q!(|a, v| *a = v));
let samples = data.sample_every(q!(Duration::from_secs(5)), nondet!(/** ... */));
let received = samples.send(&receiver, TCP.lossy(nondet!(/** ... */)).bincode());
let out = received.sim_output();

flow.sim().exhaustive(async || {
    out.assert_yields([123]).await;
});
```

## Open Questions

1. **Idempotent detection**: Can we automatically detect when `sample_every` feeds into a send of unchanged state? If so, we can treat the network as effectively non-lossy for that path.

2. **Counter-based protocols**: If retry state changes on each attempt (e.g., sequence numbers), the lasso won't trigger. Do we need a stronger fairness notion?

3. **Probabilistic bounds**: For fuzz testing, should we support "message arrives within N attempts with probability P"?

4. **Interaction with `lossy_delayed_forever`**: Should `lossy` subsume `lossy_delayed_forever` once fairness is implemented? Or keep both for different testing goals?
