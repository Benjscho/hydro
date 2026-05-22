# Fair Lossy Channel: `TCP.lossy_retry()`

## Status

**v2 implemented.** The type system (`LossyRetry` policy, `AtLeastOnce`/`NoOrder` output), API (`TCP.lossy_retry().bincode()`), simulator wiring (dedicated `FairLossyHook` with bounded duplicate injection), deploy support, and tests (liveness, duplicate injection verification, deduplication correctness) are all in place.

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

The `lossy_retry` channel uses a dedicated `FairLossyHook<T>` that guarantees eventual delivery (fairness) without permanent drops, and injects bounded duplicates to test idempotence.

Key design decisions:
- `can_make_nontrivial_decision()` only considers `pending` items (not `delivered`). Duplicates are injected opportunistically when the hook is already scheduled for pending deliveries. This prevents the scheduler from spinning on duplicate injection alone.
- Each delivered item has a bounded duplicate budget (default K=2). After K re-deliveries, the item is removed from `delivered`.
- The binary "inject duplicate?" choice adds at most 1 bit of branching per step.

#### Planned v2 Enhancement: Standalone Duplicate Injection

The current design only injects duplicates when there are also pending items to deliver. A future enhancement could allow the hook to inject duplicates even when `pending` is empty (by making `can_make_nontrivial_decision` consider `delivered`), but this requires careful integration with the scheduler to avoid infinite loops.

Since `lossy_retry()` guarantees eventual delivery, the simulator models it as:

1. Messages enter a buffer (never permanently dropped)
2. On each scheduling step, the hook non-deterministically:
   - Delivers a subset of buffered messages (0 to all) — models "some retries succeed"
   - May deliver the same message again later — models duplicates
3. **Fairness**: If a message has been in the buffer for a full lasso cycle without delivery, force it to be delivered
4. Messages are removed from the buffer only after delivery (but may be re-added to model duplicates)

### Where Duplicates Fit in the Flow

The data flow for a `lossy_retry` channel is:

```
Sender async DFIR  →  unbounded channel  →  Receiver async DFIR  →  hook buffer
                                                                         ↓
                                                              hook autonomous_decision
                                                                         ↓
                                                              hoff_send (output channel)
                                                                         ↓
                                                              Receiver async DFIR (source_stream)
                                                                         ↓
                                                              batch hook buffer (normal batch boundary)
                                                                         ↓
                                                              Tick DFIR (application logic)
```

The `fair_lossy` hook sits at the **network delivery boundary** — between the raw network buffer and the receiver's async dataflow. This is the correct place to inject duplicates because:

1. It models "the network delivered this message twice" — the most accurate real-world semantics
2. Duplicates flow through the normal `batch` mechanism, so they're visible to tick-level code exactly as they would be in production
3. The hook already has access to the buffer contents and controls what gets sent downstream
4. It doesn't interfere with the lasso detector's fairness logic (duplicates are orthogonal to "has the buffer drained?")

### Hook Model: Fair Lossy with Duplicate Injection

The hook maintains two data structures:

```rust
struct FairLossyState<T> {
    /// Messages waiting for first delivery. Fairness forces these to eventually drain.
    pending: VecDeque<T>,
    /// Messages that have been delivered at least once. Can be re-delivered as duplicates.
    /// Bounded to at most `max_duplicates_per_item` re-deliveries per item.
    delivered: Vec<(T, u8)>,  // (item, remaining_duplicate_budget)
}
```

On each scheduling step, `autonomous_decision` does:

```
1. From `pending`: pick 0..=pending.len() items to deliver (NoOrder selection)
   - Delivered items move to `delivered` with budget = K (e.g., K=2)
   - Fairness: if force_nontrivial, deliver all pending items

2. From `delivered`: non-deterministically pick 0 or 1 items to re-deliver (duplicate)
   - Decrement the item's budget
   - When budget reaches 0, remove from `delivered`
   - This is a binary choice (duplicate or not) to keep branching constant

3. Combine fresh deliveries + duplicate into the output batch
```

Key properties:
- **Eventual delivery**: fairness forces `pending` to drain (liveness)
- **Duplicates are bounded**: each item can be re-delivered at most K times (finite state space)
- **Constant branching for duplicates**: binary choice "inject a duplicate this step or not" — doesn't explode the state space
- **Duplicates are interleaved with fresh deliveries**: models real-world behavior where a retry arrives mixed with new messages

### Interaction with Lasso Detector

- `pending_count()` returns `pending.len()` — only undelivered items count for fairness
- `can_make_nontrivial_decision()` returns `true` only if `pending` is non-empty (duplicates are opportunistic, not forced)
- The fingerprint uses `pending.len().min(1)` — duplicates in `delivered` don't affect the fingerprint since they don't represent "stuck" state
- When forced by lasso: deliver all `pending` items, do NOT force duplicates (duplicates are a safety concern, not a liveness concern)

### Exhaustive Mode Considerations

The duplicate budget K bounds the state space:
- Each message contributes at most K+1 deliveries total (1 original + K duplicates)
- The binary "inject duplicate?" choice adds at most 1 bit of branching per step
- For K=2, a message sent once can appear 1, 2, or 3 times at the receiver — sufficient to catch most idempotence bugs

**Recommended K=2** for the default. This catches:
- Code that assumes exactly-once delivery (breaks on first duplicate)
- Code with off-by-one errors in deduplication logic (breaks on second duplicate)
- Without creating excessive state space (K=2 means at most 3 total deliveries per message)

### Why Not Inject Duplicates Elsewhere?

**At the sender side** (before the network): Wrong abstraction. The sender sends once; it's the *channel* that retries. Injecting at the sender would require the sender's code to be aware of retries.

**At the batch boundary** (between async and tick): Too late. The batch hook already has its own non-determinism (batch size selection). Injecting duplicates there would conflate two independent sources of non-determinism and make the simulator harder to reason about.

**At the tick level** (inside the tick DFIR): Way too late. The tick processes a batch atomically — injecting duplicates mid-tick doesn't model any real-world scenario.

The network delivery hook is the only place that correctly models "the network delivered this message more than once."

## Implementation Steps

### Step 1: Add `LossyRetry` Failure Policy ✅

**Files**: `hydro_lang/src/networking/mod.rs`

1. Add `LossyRetry` enum and `TcpFailPolicy` impl
2. Add `TcpFault::LossyRetry` variant
3. Add `lossy_retry()` method on `NetworkingConfig<Tcp<()>, S>`
4. Set `OrderingGuarantee = NoOrder`

### Step 2: Add `RetryGuarantee` to `NetworkFor` Trait ✅

**Files**: `hydro_lang/src/networking/mod.rs`, `hydro_lang/src/live_collections/stream/networking.rs`

1. Add `type RetryGuarantee: Retries` to `NetworkFor` trait (and `TcpFailPolicy`, `TransportKind`)
2. Set `RetryGuarantee = ExactlyOnce` for existing channels (preserves current behavior)
3. Set `RetryGuarantee = AtLeastOnce` for `LossyRetry`
4. Update `send` return type to use `MinRetries<N::RetryGuarantee>::Min`
5. Update all `send` variants (process-to-process, process-to-cluster, cluster-to-process, etc.)

### Step 3: Update IR and Compile Layer ✅

**Files**: `hydro_lang/src/compile/ir/mod.rs`, `hydro_lang/src/networking/mod.rs`

1. Add `TcpFault::LossyRetry` to the `TcpFault` enum
2. Ensure the `Network` IR node correctly propagates the `AtLeastOnce` retry through `CollectionKind`
3. Update any match statements on `TcpFault` (deploy graph, maelstrom, etc.)

### Step 4: Simulator Wiring with Duplicate Injection ✅

**Files**: `hydro_lang/src/sim/builder.rs`, `hydro_lang/src/sim/runtime.rs`

Implemented a dedicated `FairLossyHook<T>` struct that replaces the v1 `fair_lossy: bool` stopgap on `StreamHook`:

```rust
pub struct FairLossyHook<T> {
    pub pending: Rc<RefCell<VecDeque<T>>>,
    pub delivered: Vec<(T, u8)>,  // (item, remaining_duplicate_budget)
    pub to_release: Option<Vec<T>>,
    pub output: UnboundedSender<T>,
    pub batch_location: HookLocationMeta,
    pub format_item_debug: fn(&T) -> Option<String>,
    pub max_duplicates: u8,  // default: 2
}
```

`SimHook` implementation:
- `is_fairness_subject() = true`
- `is_lossy() = false` (never permanently drops)
- `pending_count()` = `pending.len()`
- `can_make_nontrivial_decision()` = `!pending.is_empty()` (only pending items count — duplicates are opportunistic)
- `autonomous_decision`:
  - From `pending`: NoOrder selection (same pattern as `StreamHook<T, NoOrder>`)
  - Delivered items move to `delivered` with budget = `max_duplicates`
  - Binary choice: inject one duplicate from `delivered` (decrement budget, remove if 0)
  - When `force_nontrivial`: deliver all `pending`, no forced duplicates
- `release_decision`: send all items in `to_release` to `output`

In `SimBuilder::create_network`, `TcpFault::LossyRetry` emits `FairLossyHook` (the `fair_lossy` field was removed from `StreamHook`).

### Step 5: Deploy Runtime Support ✅

**Files**: `hydro_lang/src/deploy/deploy_graph.rs`, `hydro_lang/src/deploy/deploy_graph_containerized.rs`

For real deployment, `LossyRetry` should use the same TCP connection as `FailStop` — the actual retry logic is handled by the Hydro runtime's connection management (reconnection on failure). The channel type is primarily a *semantic annotation* that affects:
- The type system (output is `AtLeastOnce`, `NoOrder`)
- The simulator (fairness-based delivery)

In production, TCP already provides reliable delivery within a connection. The `LossyRetry` annotation tells the runtime to:
- Reconnect on connection failure
- Buffer and resend messages that were in-flight during disconnection
- Accept that duplicates may occur (e.g., message sent, ack lost, resent)

For v1, deploy treats `LossyRetry` identically to `FailStop` at the transport level, since TCP handles reliability. The semantic difference is only in the type system and simulator.

### Step 6: Tests ✅

**Files**: `hydro_lang/src/sim/tests/liveness.rs`

1. ~~Basic delivery test~~ ✅ (`liveness_single_send_over_lossy_retry`)
2. Idempotence enforcement: verify that `fold` on a `lossy_retry()` stream requires idempotence proof (enforced by type system — compile-time)
3. Ordering enforcement: verify that `for_each` on a `lossy_retry()` stream requires `assume_ordering` (enforced by type system — compile-time)
4. ~~Liveness test~~ ✅ (`liveness_single_send_over_lossy_retry`)
5. Compare with `lossy`: same test with `lossy()` should fail (no retry), with `lossy_retry()` should pass (covered by existing `liveness_single_send_over_lossy_fails`)
6. ~~Duplicate safety test~~ ✅ (`lossy_retry_injects_duplicates`) — verifies the simulator actually injects duplicates
7. ~~Duplicate correctness test~~ ✅ (`lossy_retry_idempotent_fold_correct`) — verifies `unique()` correctly deduplicates despite duplicates

### Step 7: Documentation ✅ (partial)

1. ~~Add rustdoc to `lossy_retry()` method~~ ✅
2. Update the networking reference docs
3. Add an example showing a simple request-response protocol using `lossy_retry()`

## Open Questions

1. **Should `lossy_retry()` require a `NonDet` guard?** ✅ Resolved: No. The non-determinism (duplicates, reordering) is fully captured by the output type. The channel provides a *stronger* guarantee than `lossy` (eventual delivery), so it's less "unsafe."

2. **What about the deploy runtime?** ✅ Resolved: For v1, treat as `FailStop` in production. Real retry/reconnection logic can be added incrementally.

3. **Naming**: ✅ Resolved: `lossy_retry()` for consistency with the existing `lossy()` naming.

4. **Should there be a variant with ordering?** A `lossy_retry_ordered()` that preserves ordering (models TCP with reconnection where sequence numbers ensure ordering) could be useful but adds complexity. Defer to follow-up.

5. **Should `T: Clone` be required for `FairLossyHook`?** The hook needs to re-deliver items from `delivered`, which requires cloning. Since the hook operates on serialized `Bytes` (not the user's type `T`), and `Bytes` is cheaply cloneable (reference-counted), this is not a concern in practice. The hook type parameter is `Bytes`, not the user's `T`.
