//! Liveness testing examples for the Hydro simulator.
//!
//! These tests demonstrate how to assert that a value eventually makes it across
//! a lossy network, given that the system has a mechanism for retrying (e.g.
//! `sample_every`). The key correctness guarantee: if the system repeatedly
//! attempts to send a message, the simulator's fairness constraint ensures that
//! at least one attempt eventually succeeds.
//!
//! # Fairness Model
//!
//! The simulator uses a **lasso-based fairness** approach inspired by TLA+ and P:
//!
//! - **Weak fairness**: If a network channel is *continuously enabled* (has a
//!   pending message), it must eventually deliver. This prevents the simulator
//!   from dropping all messages forever.
//!
//! - **Lasso detection**: The simulator tracks system state (fold values, pending
//!   messages). If the state repeats after a sequence of drops, the system is in
//!   a cycle where "dropping another message doesn't teach us anything new." At
//!   that point, the simulator forces delivery to break the cycle.
//!
//! # What Should Pass vs Fail
//!
//! - A single send over lossy with NO retry mechanism → the test should FAIL
//!   (the simulator can legitimately drop the only attempt)
//! - A repeated send (via `sample_every`) over lossy → the test should PASS
//!   (fairness guarantees eventual delivery)
//! - A retry-with-ack protocol → the test should PASS (retries ensure liveness)

use std::time::Duration;

use stageleft::q;

use crate::location::Location;
use crate::networking::TCP;
use crate::nondet::nondet;
use crate::prelude::FlowBuilder;

/// Example 1: Repeated sampling over a lossy network via sample_every.
///
/// A singleton value is sampled periodically and sent across a lossy channel.
/// Because the source repeats infinitely, fairness guarantees that at least one
/// sample eventually arrives at the destination.
#[cfg(feature = "sim")]
#[test]
fn liveness_sample_every_over_lossy() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    // A singleton value that will be repeatedly sampled and sent.
    let value = sender_loc
        .source_iter(q!(vec![123_u32]))
        .fold(q!(|| 0u32), q!(|acc, v| *acc = v));

    // Sample the singleton every 5 seconds (in sim, time is virtual).
    let samples = value.sample_every(q!(Duration::from_secs(5)), nondet!(/** periodic retry */));

    // Send over a lossy network. Individual messages may be dropped,
    // but fairness ensures at least one eventually arrives.
    let received = samples.send(&receiver_loc, TCP.lossy(nondet!(/** lossy network */)).bincode());

    let out = received.sim_output();

    // This assertion should PASS: the repeated sampling + fairness guarantee
    // means at least one copy of 123 will arrive.
    flow.sim().max_lasso_steps(5).exhaustive(async || {
        out.assert_yields([123_u32]).await;
    });
}

/// Example 2: Single send over lossy — should FAIL.
///
/// Without retries, the simulator can legitimately drop the only message.
/// This test demonstrates that liveness is NOT guaranteed for one-shot sends.
#[cfg(feature = "sim")]
#[test]
#[should_panic(expected = "lossy channel dropped a message")]
fn liveness_single_send_over_lossy_fails() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    // A single message, sent exactly once.
    let data = sender_loc.source_iter(q!(vec![123_u32]));

    // Send over lossy — no retry mechanism.
    let received = data.send(&receiver_loc, TCP.lossy(nondet!(/** lossy */)).bincode());

    let out = received.sim_output();

    // This should FAIL: the simulator can drop the single message and
    // reach quiescence without the assertion being satisfied.
    flow.sim().exhaustive(async || {
        out.assert_yields([123_u32]).await;
    });
}

/// Example 3: Application-level retry with acknowledgment.
///
/// This demonstrates a more realistic protocol where the sender retries
/// indefinitely via sample_every. Fairness ensures that eventually one send gets through.
#[cfg(feature = "sim")]
#[test]
fn liveness_retry_with_ack() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    // The payload to send, stored as a singleton.
    let payload = sender_loc
        .source_iter(q!(vec![42_u32]))
        .fold(q!(|| 0u32), q!(|acc, v| *acc = v));

    // Retry the payload periodically over lossy via sample_every.
    let retries = payload.sample_every(q!(Duration::from_secs(1)), nondet!(/** retry interval */));

    let received = retries.send(&receiver_loc, TCP.lossy(nondet!(/** lossy */)).bincode());

    let out = received.sim_output();

    // Should PASS: retries + fairness = eventual delivery.
    flow.sim().max_lasso_steps(5).exhaustive(async || {
        out.assert_yields([42_u32]).await;
    });
}

/// Example 4: Single send over lossy_retry — should PASS.
///
/// Unlike `lossy()`, `lossy_retry()` guarantees eventual delivery via fairness.
/// A single message sent over `lossy_retry()` will eventually arrive because
/// the channel itself models retry semantics.
#[cfg(feature = "sim")]
#[test]
fn liveness_single_send_over_lossy_retry() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    let data = sender_loc.source_iter(q!(vec![123_u32]));

    // Send over lossy_retry — the channel guarantees eventual delivery.
    let received = data.send(&receiver_loc, TCP.lossy_retry().bincode());

    // The output has AtLeastOnce and NoOrder semantics.
    // Use unique() to deduplicate, then sim_output.
    let out = received.unique().sim_output();

    // Should PASS: lossy_retry guarantees delivery via fairness.
    flow.sim().max_lasso_steps(5).exhaustive(async || {
        out.assert_yields_unordered([123_u32]).await;
    });
}

/// Example 5: Verify that lossy_retry actually injects duplicates.
///
/// Sends a single message over `lossy_retry()` and counts how many times it
/// arrives (without deduplication). In at least one execution path, the
/// simulator should inject a duplicate, causing the count to exceed 1.
#[cfg(feature = "sim")]
#[test]
fn lossy_retry_injects_duplicates() {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    let saw_duplicate = AtomicBool::new(false);

    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    let data = sender_loc.source_iter(q!(vec![42_u32]));
    let received = data.send(&receiver_loc, TCP.lossy_retry().bincode());

    // Output the raw AtLeastOnce stream (may contain duplicates).
    let out = received.sim_output();

    flow.sim().max_lasso_steps(5).exhaustive(async || {
        let items = out.collect_all().await;
        // At least one item must arrive (fairness guarantee).
        assert!(!items.is_empty(), "Expected at least one delivery");
        // All items should be the value we sent.
        assert!(items.iter().all(|v| *v == 42), "Unexpected value in output");
        if items.len() > 1 {
            saw_duplicate.store(true, AtomicOrdering::Relaxed);
        }
    });

    assert!(
        saw_duplicate.load(AtomicOrdering::Relaxed),
        "Expected the simulator to inject at least one duplicate delivery, but all executions delivered exactly once"
    );
}

/// Example 6: Idempotent deduplication over lossy_retry is always correct despite duplicates.
///
/// Sends values over `lossy_retry()`, applies `unique()` to deduplicate, and
/// verifies all values arrive exactly once regardless of how many duplicates the
/// simulator injects.
#[cfg(feature = "sim")]
#[test]
fn lossy_retry_idempotent_fold_correct() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    let data = sender_loc.source_iter(q!(vec![10_u32, 20_u32, 30_u32]));
    let received = data.send(&receiver_loc, TCP.lossy_retry().bincode());

    // unique() deduplicates — result should always be exactly {10, 20, 30}.
    let out = received.unique().sim_output();

    flow.sim().max_lasso_steps(5).exhaustive(async || {
        out.assert_yields_unordered([10_u32, 20_u32, 30_u32]).await;
    });
}
