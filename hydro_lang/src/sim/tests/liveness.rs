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

/// Example 1: Repeated send over a lossy network via source_interval.
///
/// A value is sent periodically over a lossy channel. Because the sender
/// retries indefinitely (via interval), fairness guarantees that at least one
/// attempt eventually arrives at the destination.
#[cfg(feature = "sim")]
#[test]
fn liveness_sample_every_over_lossy() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    // source_interval produces a stream of ticks; map each to our payload.
    let retries = sender_loc
        .source_interval(q!(Duration::from_secs(1)), nondet!(/** periodic retry */))
        .map(q!(|_| 123_u32));

    // Send over a lossy network. Individual messages may be dropped,
    // but fairness ensures at least one eventually arrives.
    let received = retries.send(&receiver_loc, TCP.lossy(nondet!(/** lossy network */)).bincode());

    let out = received.sim_output();

    // This assertion should PASS: the repeated sends + fairness guarantee
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
#[should_panic]
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
/// indefinitely. Fairness ensures that eventually one send gets through.
#[cfg(feature = "sim")]
#[test]
fn liveness_retry_with_ack() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    // Retry the payload periodically over lossy.
    let retries = sender_loc
        .source_interval(q!(Duration::from_secs(1)), nondet!(/** retry interval */))
        .map(q!(|_| 42_u32));

    let received = retries.send(&receiver_loc, TCP.lossy(nondet!(/** lossy */)).bincode());

    let out = received.sim_output();

    // Should PASS: retries + fairness = eventual delivery.
    flow.sim().max_lasso_steps(5).exhaustive(async || {
        out.assert_yields([42_u32]).await;
    });
}
