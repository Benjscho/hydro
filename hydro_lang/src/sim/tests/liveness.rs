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

/// Example 1: Repeated sampling over a lossy network.
///
/// A singleton value is sampled periodically and sent across a lossy channel.
/// Because the source repeats infinitely, fairness guarantees that at least one
/// sample eventually arrives at the destination.
///
/// This is the canonical "CRDT gossip" pattern: the same state is repeatedly
/// broadcast, so losing any individual message is harmless as long as one
/// eventually gets through.
///
/// ```text
/// ┌─────────┐   sample_every    ┌──────────┐   send (lossy)   ┌─────────┐
/// │ value=  │ ─────────────────► │ stream   │ ────────────────► │ output  │
/// │   123   │   (5s interval)    │ of 123s  │   (may drop)     │ ≥1 123  │
/// └─────────┘                    └──────────┘                   └─────────┘
/// ```
#[cfg(feature = "sim")]
#[test]
#[ignore = "blocked on scheduler architecture: lossy hook observation never becomes ready"]
fn liveness_sample_every_over_lossy() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    // A singleton value that will be repeatedly sampled and sent.
    let value = sender_loc.source_iter(q!(vec![123_u32])).fold(
        q!(|| 0u32),
        q!(|acc, v| *acc = v),
    );

    // Sample the singleton every 5 seconds (in sim, time is virtual).
    // Each sample emits the current value into a stream.
    let samples = value.sample_every(q!(Duration::from_secs(5)), nondet!(/** periodic retry */));

    // Send over a lossy network. Individual messages may be dropped,
    // but fairness ensures at least one eventually arrives.
    let received = samples.send(&receiver_loc, TCP.lossy(nondet!(/** lossy network */)).bincode());

    let out = received.sim_output();

    // This assertion should PASS: the repeated sampling + fairness guarantee
    // means at least one copy of 123 will arrive.
    flow.sim().exhaustive(async || {
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
    let data = sender_loc.source_iter(q!(std::iter::once(123_u32)));

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
/// This demonstrates a more realistic protocol:
/// 1. Sender sends a packet
/// 2. Sender retries every 1s if no ACK received
/// 3. Receiver sends ACK upon receipt
/// 4. Sender stops retrying after ACK
///
/// The liveness guarantee: because the sender retries indefinitely until ACK,
/// fairness ensures that eventually one send AND one ACK both get through.
///
/// ```text
///  Sender                          Receiver
///    │                                │
///    │──── data (may drop) ──────────►│
///    │                                │──── ack (may drop) ────►│
///    │◄─── ack (may drop) ───────────│                         │
///    │                                │                         │
///    │  (retry after 1s if no ack)    │                         │
///    │──── data (may drop) ──────────►│                         │
///    │  ...eventually one gets thru   │                         │
/// ```
#[cfg(feature = "sim")]
#[test]
#[ignore = "blocked on scheduler architecture: lossy hook observation never becomes ready"]
fn liveness_retry_with_ack() {
    let mut flow = FlowBuilder::new();
    let sender_loc = flow.process::<()>();
    let receiver_loc = flow.process::<()>();

    // The payload to send.
    let payload = sender_loc.source_iter(q!(vec![42_u32])).fold(
        q!(|| None::<u32>),
        q!(|acc, v| *acc = Some(v)),
    );

    // Track whether we've received an ACK.
    // In a real implementation this would be a fold over the ack stream.
    // For now, this sketches the pattern:
    //
    // let ack_received = acks.fold(|| false, |acc, _| *acc = true);
    // let should_send = payload.filter_if(ack_received.map(|a| !a));
    // let retries = should_send.sample_every(1s, nondet!(...));
    // let sent = retries.send(&receiver_loc, TCP.lossy(...).bincode());
    // let acks = receiver_loc.source_iter(once(())).send(&sender_loc, TCP.lossy(...).bincode());

    // Simplified: just retry the payload every second over lossy.
    let retries = payload
        .map(q!(|opt| opt.unwrap()))
        .sample_every(q!(Duration::from_secs(1)), nondet!(/** retry interval */));

    let received = retries.send(&receiver_loc, TCP.lossy(nondet!(/** lossy */)).bincode());

    let out = received.sim_output();

    // Should PASS: retries + fairness = eventual delivery.
    flow.sim().exhaustive(async || {
        out.assert_yields([42_u32]).await;
    });
}
