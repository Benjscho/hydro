# Fair Lossy Channel: `TCP.lossy_retry()`

## Summary

Add a new TCP failure policy, `lossy_retry()`, that models a channel where individual messages may be lost but the sender automatically retries, producing an output stream with `AtLeastOnce` retries and `NoOrder` ordering. This captures the most common real-world distributed systems pattern — unreliable delivery with application-level retries — as a first-class channel type, eliminating the need for users to manually wire `sample_every` + `lossy` loops.

**Key files (to be modified/created):**
- `hydro_lang/src/networking/mod.rs` — new `LossyRetry` failure policy
- `hydro_lang/src/sim/builder.rs` — simulator wiring for the new channel type
- `hydro_lang/src/sim/runtime.rs` — new `FairLossyHook` (or reuse `StreamHook` with new mode)
- `hydro_lang/src/live_collections/stream/networking.rs` — `send` return type adjustments
- `hydro_lang/src/sim/tests/liveness.rs` — tests for the new channel

## Problem Statement

### The Gap

Hydro's current channel types don't model the most common real-world networking pattern:

| Real-world scenario | What happens | Current Hydro equivalent |
|---|---|---|
| gRPC with retries | Messages may be lost, sender retries, receiver sees duplicates | Manual: `sample_every` + `lossy` |
| Kafka at-least-once | Messages delivered at least once, possibly duplicated | No direct equivalent |
| TCP reconnection | Connection drops, sender re-establishes and resends | Manual wiring |
| Message queue (SQS standard) | At-least-once delivery, no ordering | No direct equivalent |

The existing `TCP.lossy(nondet)` models permanent message loss with no recovery. To get retry semantics, users must manually build a retry loop:

```rust
let payload = sender.source_iter(q!(vec![42u32]))
    .fold(q!(|| 0u32), q!(|acc, v| *acc = v));
let retries = payload.sample_every(q!(Duration::from_secs(1)), nondet!(/** retry */));
let received = retries.send(&receiver, TCP.lossy(nondet!(/** lossy */)).bincode());
```

This is verbose, error-prone, and doesn't correctly propagate the `AtLeastOnce` type information to the receiver.

### Why This Matters

1. **Type safety**: The receiver of a `lossy` channel currently gets `ExactlyOnce` semantics in the type system, even though retries produce duplicates. The type system should reflect reality.
2. **Ergonomics**: Every protocol that needs reliability (Paxos, 2PC, request-response) would benefit from a one-liner channel declaration.
3. **Simulator accuracy**: The simulator should model the retry behavior automatically, including the fairness guarantee that retries eventually succeed.
4. **Confluence enforcement**: With `AtLeastOnce` on the output, the type system forces downstream consumers to prove idempotence — catching bugs where duplicates would cause incorrect behavior.

## Proposed Semantics

### Channel Behavior

`TCP.lossy_retry().bincode()` models a channel where:

- **Delivery**: Each message is eventually delivered at least once (liveness guarantee)
- **Duplicates**: Messages may be delivered multiple times (retries)
- **Ordering**: No ordering guarantee (retries and reordering break any prefix guarantee)
- **Fairness**: If the sender keeps the message available, the channel will eventually deliver it

### Type-Level Guarantees

```rust
// Input stream on sender side
stream: Stream<T, Process<L1>, Unbounded, O, R>

// After .send(&receiver, TCP.lossy_retry().bincode())
received: Stream<T, Process<L2>, Unbounded, NoOrder, AtLeastOnce>
```

The output is always `NoOrder` (retries break ordering) and `AtLeastOnce` (duplicates possible). This is independent of the input's ordering/retry guarantees — the channel itself introduces both forms of non-determinism.

### Comparison with Existing Channels

| Channel | Output Order | Output Retries | Liveness | NonDet required |
|---------|-------------|----------------|----------|-----------------|
| `fail_stop()` | preserves input | preserves input | Yes (no drops) | No |
| `lossy(nondet)` | `TotalOrder` | preserves input | No (permanent drops) | Yes |
| `lossy_delayed_forever()` | `NoOrder` | preserves input | No (safety-only) | No |
| **`lossy_retry()`** | **`NoOrder`** | **`AtLeastOnce`** | **Yes (fairness)** | **No** |

Note: `lossy_retry()` does **not** require a `NonDet` guard because:
- The output is strictly weaker than the ideal (adds duplicates, removes ordering)
- The liveness guarantee means no information is lost
- The non-determinism (which duplicates, what order) is fully captured by the type system

## API Design

### User-Facing API

```rust
// In hydro_lang/src/networking/mod.rs

/// A TCP failure policy that models lossy delivery with automatic retries.
///
/// Messages may be lost in transit, but the sender automatically retries,
/// guaranteeing eventual delivery (at-least-once semantics). The receiver
/// may observe duplicates and messages may arrive out of order.
///
/// This models the common real-world pattern of unreliable networks with
/// application-level retry logic (gRPC retries, message queues, TCP reconnection).
///
/// The output stream has [`NoOrder`] and [`AtLeastOnce`] guarantees, which means
/// downstream consumers must handle duplicates (e.g., via idempotent operations
/// like `max`, `min`, or `fold` with an idempotence proof).
pub enum LossyRetry {}

impl TcpFailPolicy for LossyRetry {
    type OrderingGuarantee = NoOrder;

    fn tcp_fault() -> TcpFault {
        TcpFault::LossyRetry
    }
}

// On NetworkingConfig<Tcp<()>, S>:
impl<S: ?Sized> NetworkingConfig<Tcp<()>, S> {
    /// Configures the TCP transport to model lossy delivery with automatic retries.
    ///
    /// Individual messages may be lost, but the sender retries automatically,
    /// guaranteeing at-least-once delivery. The output stream will have
    /// [`NoOrder`] ordering and [`AtLeastOnce`] retry semantics.
    ///
    /// This is the recommended channel type for protocols that must tolerate
    /// network partitions and process crashes, such as consensus protocols,
    /// request-response patterns, and event streaming.
    pub const fn lossy_retry(self) -> NetworkingConfig<Tcp<LossyRetry>, S> {
        NetworkingConfig {
            name: self.name,
            _phantom: (PhantomData, PhantomData),
        }
    }
}
```

### Type System Integration

The `send` method's return type must account for the new channel's effect on the `Retries` parameter. Currently, `send` preserves `R`:

```rust
fn send(...) -> Stream<T, ..., <O as MinOrder<N::OrderingGuarantee>>::Min, R>
```

For `lossy_retry()`, the output must always be `AtLeastOnce` regardless of input `R`. This requires either:

**Option A**: A new associated type on `NetworkFor` for the retry guarantee:

```rust
pub trait NetworkFor<T: ?Sized> {
    type OrderingGuarantee: Ordering;
    type RetryGuarantee: Retries;  // NEW
    // ...
}
```

Then `send` returns:
```rust
Stream<T, ..., <O as MinOrder<N::OrderingGuarantee>>::Min, <R as MinRetries<N::RetryGuarantee>>::Min>
```

For `fail_stop` and `lossy`, `RetryGuarantee = ExactlyOnce` (preserves input). For `lossy_retry`, `RetryGuarantee = AtLeastOnce` (always weakens to AtLeastOnce).

**Option B**: A separate `send` variant or trait bound that handles the retry weakening.

**Recommendation**: Option A is cleaner and more composable. The `MinRetries` trait already exists and handles the lattice correctly (`MinRetries<AtLeastOnce>::Min = AtLeastOnce` for any `R`).

### IR-Level Changes

Add a new variant to `TcpFault`:

```rust
pub enum TcpFault {
    FailStop,
    Lossy,
    LossyDelayedForever,
    LossyRetry,  // NEW
}
```

And to `StreamRetry` (or handle via the `CollectionKind` at the network boundary):

The `Network` IR node already carries `networking_info: NetworkingInfo`. The compiler can inspect this to determine the output `StreamRetry`.

## Simulator Behavior

### Hook Design

The `lossy_retry` channel uses a new hook mode: **"fair lossy with duplicates"**. The hook:

1. Buffers incoming messages (same as current lossy hook)
2. On each scheduling decision:
   - Non-deterministically picks 0 or more items from the buffer to deliver
   - Delivered items are **NOT removed** from the buffer (they can be re-delivered = duplicates)
   - Non-deterministically removes 0 or more items from the buffer (modeling "sender gave up" or "ack received" — but since we guarantee at-least-once, at least one copy must have been delivered before removal)
3. Is a **fairness subject** — the lasso detector forces delivery if the system cycles

Actually, a simpler and more correct model:

### Simplified Hook Model

Since `lossy_retry()` guarantees eventual delivery, the simulator can model it as:

1. Messages enter a buffer (never permanently dropped)
2. On each scheduling step, the hook non-deterministically:
   - Delivers a subset of buffered messages (0 to all) — models "some retries succeed"
   - May deliver the same message again later — models duplicates
3. **Fairness**: If a message has been in the buffer for a full lasso cycle without delivery, force it to be delivered
4. Messages are removed from the buffer only after delivery (but may be re-added to model duplicates)

Even simpler — since we want to test that downstream code handles duplicates correctly:

### Recommended Hook Model (Simplest Correct Approach)

```
Buffer: VecDeque<T> (messages waiting to be delivered)
Delivered: Vec<T> (messages that have been delivered at least once)

On each step:
  1. Pick 0..=buffer.len() items from buffer → deliver them, move to Delivered
  2. Pick 0..=delivered.len() items from Delivered → re-deliver them (duplicates)
  3. Fairness: if buffer is non-empty for a full cycle, force delivery of all

Output ordering: NoOrder (items delivered in any order, duplicates interleaved)
```

This correctly models:
- **Eventual delivery**: fairness forces buffer to drain
- **Duplicates**: items in `Delivered` can be re-sent
- **No ordering**: items delivered in arbitrary order

### Interaction with Lasso Detector

The hook is a fairness subject. Its `pending_count()` returns `buffer.len()`. The fingerprint uses `min(1)` so "1 pending" and "5 pending" are the same abstract state. When the lasso forces delivery, all buffered items are delivered.

For duplicates: the lasso detector doesn't need to force duplicates. Duplicates are explored by the fuzzer during normal execution. The key property is that the *buffer eventually drains* (liveness), and *duplicates may occur* (safety testing).

### Exhaustive Mode Considerations

In exhaustive mode, the state space must be bounded. The duplicate re-delivery creates potential for unbounded exploration. To bound it:

- Each message can be re-delivered at most `K` times (e.g., K=2 or K=3)
- After K deliveries, the message is removed from the `Delivered` set
- This is sufficient to test idempotence without infinite state space

Alternatively, model duplicates as a simple boolean per step: "did any duplicate occur?" This keeps branching constant.

**Recommended approach for v1**: Don't model re-delivery of already-delivered messages. Instead, model the channel as:
1. Messages buffer, non-deterministically delivered (like current lossy, but never dropped)
2. The `AtLeastOnce` type annotation forces downstream to prove idempotence
3. Actual duplicate testing is deferred to a follow-up (or handled by the existing `sample_every` pattern on the sender side, which naturally produces duplicates)

This gives us the type safety and ergonomics benefits immediately, with the option to add explicit duplicate injection later.

## Implementation Steps

### Step 1: Add `LossyRetry` Failure Policy

**Files**: `hydro_lang/src/networking/mod.rs`

1. Add `LossyRetry` enum and `TcpFailPolicy` impl
2. Add `TcpFault::LossyRetry` variant
3. Add `lossy_retry()` method on `NetworkingConfig<Tcp<()>, S>`
4. Set `OrderingGuarantee = NoOrder`

### Step 2: Add `RetryGuarantee` to `NetworkFor` Trait

**Files**: `hydro_lang/src/networking/mod.rs`, `hydro_lang/src/live_collections/stream/networking.rs`

1. Add `type RetryGuarantee: Retries` to `NetworkFor` trait
2. Set `RetryGuarantee = ExactlyOnce` for existing channels (preserves current behavior)
3. Set `RetryGuarantee = AtLeastOnce` for `LossyRetry`
4. Update `send` return type to use `MinRetries<N::RetryGuarantee>::Min`
5. Update all `send` variants (process-to-process, process-to-cluster, cluster-to-process, etc.)

### Step 3: Update IR and Compile Layer

**Files**: `hydro_lang/src/compile/ir/mod.rs`, `hydro_lang/src/networking/mod.rs`

1. Add `TcpFault::LossyRetry` to the `TcpFault` enum
2. Ensure the `Network` IR node correctly propagates the `AtLeastOnce` retry through `CollectionKind`
3. Update any match statements on `TcpFault` (deploy graph, maelstrom, etc.)

### Step 4: Simulator Wiring

**Files**: `hydro_lang/src/sim/builder.rs`, `hydro_lang/src/sim/runtime.rs`

1. In `SimBuilder::create_network`, handle `TcpFault::LossyRetry`:
   - Wire like current `Lossy` (buffer + hook + channel)
   - But use a hook that **never permanently drops** — it only delays delivery
   - Mark the hook as a fairness subject (same as lossy)
2. Create a `FairLossyHook` (or add a `fair_lossy` mode to `StreamHook`):
   - `autonomous_decision`: pick 0..=N items to deliver (like `NoOrder` non-lossy hook), but items stay in buffer until delivered at least once
   - `is_fairness_subject() = true`
   - `is_lossy() = false` (it's not lossy in the "permanent drop" sense)
   - When forced by lasso: deliver all buffered items

The simplest implementation: reuse `StreamHook<T, NoOrder>` with `lossy: false`. This already:
- Picks a random subset to deliver
- Never drops items (non-lossy mode drains from buffer into output)
- Is not a fairness subject by default

We just need to make it a fairness subject so the lasso forces progress. Add a new field:

```rust
pub struct StreamHook<T, Order: Ordering> {
    // ... existing fields ...
    pub fair_lossy: bool,  // NEW: fairness subject but never drops
}
```

And update `is_fairness_subject()`:
```rust
fn is_fairness_subject(&self) -> bool {
    self.lossy || self.is_interval || self.fair_lossy
}
```

### Step 5: Deploy Runtime Support

**Files**: `hydro_lang/src/deploy/deploy_graph.rs`, `hydro_lang/src/deploy/deploy_graph_containerized.rs`

For real deployment, `LossyRetry` should use the same TCP connection as `FailStop` — the actual retry logic is handled by the Hydro runtime's connection management (reconnection on failure). The channel type is primarily a *semantic annotation* that affects:
- The type system (output is `AtLeastOnce`, `NoOrder`)
- The simulator (fairness-based delivery)

In production, TCP already provides reliable delivery within a connection. The `LossyRetry` annotation tells the runtime to:
- Reconnect on connection failure
- Buffer and resend messages that were in-flight during disconnection
- Accept that duplicates may occur (e.g., message sent, ack lost, resent)

For v1, deploy can treat `LossyRetry` identically to `FailStop` at the transport level, since TCP handles reliability. The semantic difference is only in the type system and simulator.

### Step 6: Tests

**Files**: `hydro_lang/src/sim/tests/liveness.rs` (or new test file)

1. Basic delivery test: send over `lossy_retry()`, assert message arrives
2. Idempotence enforcement: verify that `fold` on a `lossy_retry()` stream requires idempotence proof
3. Ordering enforcement: verify that `for_each` on a `lossy_retry()` stream requires `assume_ordering`
4. Liveness test: send a single message over `lossy_retry()`, assert it eventually arrives (fairness)
5. Compare with `lossy`: same test with `lossy()` should fail (no retry), with `lossy_retry()` should pass

### Step 7: Documentation

1. Add rustdoc to `lossy_retry()` method
2. Update the networking reference docs
3. Add an example showing a simple request-response protocol using `lossy_retry()`

## Open Questions

1. **Should `lossy_retry()` require a `NonDet` guard?** Current recommendation: No, because the non-determinism (duplicates, reordering) is fully captured by the output type. The channel provides a *stronger* guarantee than `lossy` (eventual delivery), so it's less "unsafe."

2. **Should we model explicit duplicates in the simulator?** For v1, the type system enforcement (requiring idempotence proofs) is sufficient. Explicit duplicate injection can be added later as an enhancement to catch bugs where the idempotence proof is incorrect.

3. **What about the deploy runtime?** For v1, treat as `FailStop` in production. Real retry/reconnection logic can be added incrementally.

4. **Naming**: `lossy_retry()` vs `at_least_once()` vs `reliable()` vs `fair_lossy()`? The name should convey both the failure model (messages can be lost) and the recovery (retries ensure delivery). `lossy_retry()` is explicit about both. `at_least_once()` focuses on the guarantee. Recommendation: `lossy_retry()` for consistency with the existing `lossy()` naming.

5. **Should there be a variant with ordering?** A `lossy_retry_ordered()` that preserves ordering (models TCP with reconnection where sequence numbers ensure ordering) could be useful but adds complexity. Defer to follow-up.
