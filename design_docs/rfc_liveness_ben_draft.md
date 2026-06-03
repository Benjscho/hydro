# RFC: Liveness Testing in the Hydro Simulator

## Summary 

This doc presents how we plan to add liveness testing to the Hydro simulator.
Hydro's simulator supports safety properties ("something bad doesn't happen"),
but not yet liveness properties ("something good eventually happens").

## Key Decisions

| # | Decision | Discussion |
|---|----------|-----------|
| 1 | Pluggable `LivenessChecker` trait | [Flexibility](#flexibility) |
| 2 | Lasso detection as the default strategy | [Strategy: Lasso Detection](#strategy-lasso-detection-default) |
| 3 | Strong fairness (SF) on fairness-subject hooks | [Ensuring Fairness](#ensuring-fairness) |
| 4 | Temperature checking as an alternative strategy | [Strategy: Temperature Checking](#strategy-temperature-checking) |
| 5 | Adjustable search budget, not mandatory exhaustive exploration | [Exhaustiveness vs. Fuzzing](#exhaustiveness-vs-fuzzing) |

## Motivation

The goal of Hydro is to provide a smooth journey from spec to production while
maintaining correctness. Liveness bugs are common issues in distributed systems
and can be hard to test for.

Here's some classes of liveness bugs that we would like the Hydro simulator
to be able to catch: 

| Bug class | Example | What "good thing" must happen |
|-----------|---------|-------------------------------|
| **Message loss** | Sending messages over unreliable network | Message eventually delivered |
| **Deadlock** | Two processes each waiting for the other's message | At least one process makes progress |
| **Livelock** | Processes keep exchanging messages but never commit | Protocol reaches a decision |
| **Starvation** | One node never gets elected leader or assigned work | Every node eventually participates |
| **Non-convergence** | CRDT gossip that never reaches consistent state | All replicas eventually agree |
| **Drain failure** | Pipeline items cycle between stages forever | All items eventually reach output |
| **Request completion** | Client request stuck in routing/queuing | Response eventually produced |



## Prior Art

To frame the design, it's helpful to look at how other model-checking tools
approach liveness testing.

### TLA+

TLA+ expresses liveness properties using temporal logic operators, primarily <>
(eventually) and [] (always). A liveness property like <>(received = 3) asserts
that in every possible behavior of the system, there exists some future state
where 3 is eventually received. More complex liveness can be expressed with
combinations of expressions. These properties are declared in the model config
as PROPERTY rather than INVARIANT, signaling to TLC that they must hold over
infinite behaviors, not just individual states.

TLC checks liveness by constructing the complete state graph and then searching
for counterexamples. Specifically, it looks for strongly connected components
(loops) in the state graph that are reachable from an initial state and in
which the liveness condition is never satisfied. If TLC finds such a loop,
it reports it as a finite prefix followed by the cycle. 

Fairness conditions are what make liveness properties tractable in TLA+.
Without fairness, TLC will always find a trivial counterexample — the system
could stutter forever and never do anything. Weak fairness (WF) eliminates
behaviors where a continuously enabled action is never taken, while strong
fairness (SF) eliminates behaviors where a repeatedly enabled action is never
taken. 

### P

P checks liveness properties using spec monitors with hot states. A hot state
annotation tells P "the system must not stay here forever" — it encodes a
progress requirement. For example, a monitor that enters a hot WaitForResponse
state after observing a request is asserting that a response must eventually be
received. The checker detects violations in two ways: if the program terminates
while any monitor is in a hot state, that's flagged as a liveness bug; for
non-terminating systems, P uses a temperature heuristic where it counts how
many consecutive steps a monitor stays hot, and flags if the temperature
exceeds a threshold (5× the max-steps bound).

P's default bug-finding mode is randomized, not exhaustive. It runs on the
Coyote runtime, which controls scheduling and explores different interleavings
of concurrent machines. For liveness specifically, P only checks under "fair"
schedulers — those that guarantee every enabled machine eventually gets
scheduled — to avoid false positives from unrealistic starvation scenarios. The
checker explores many random schedules (configurable with --schedules) up to a
step bound (configurable with --max-steps), where each "step" is one event
dequeue-and-handle cycle. This makes it practical for bug-finding in large
systems while losing exhaustive guarantees.

For stronger guarantees, P offers PEx, an exhaustive stateful model checker
that caches visited states and performs systematic exploration. PEx can detect
cycles (lassos) in the state space rather than relying on temperature
heuristics, providing sound verification within bounded instances — at the cost
of significantly higher memory usage and restriction to finite state spaces.

### Coyote / P#

Coyote checks liveness properties using monitors (a variant of Büchi automata)
borrowed from the P language. A monitor declares [Hot] and [Cold] states: a hot
state signals that the system owes progress, and a cold state signals that
progress has been made. A liveness violation is an infinite execution that
remains in hot states without ever reaching a cold state. Since testing cannot
produce infinite executions, Coyote uses a heuristic: it looks for sufficiently
long executions that stay hot based on user-configured thresholds
(--liveness-temperature-threshold or derived from --max-steps). The lasso
detection technique from [Mudduluru et al., FMCAD 2017] is also part of
Coyote's lineage but the primary checker is the temperature-based
approximation.

Coyote records scheduling decisions but does not cache or compare program
states. It explores interleavings by taking over scheduling of all concurrent
workers and executing them one at a time, using an exploration strategy to
decide which worker runs next at each scheduling point. Coyote implements
multiple strategies: random walk (RW), PCT, a novel task-aware PCTt (priorities
per chain of continuations rather than per task), delay bounding (DB),
partial-order sampling (POS), and reinforcement-learning-based (QL). For
liveness specifically, any strategy can be used as long as it is fair — meaning
it does not permanently starve enabled workers. Unfair strategies like PCT are
converted to fair ones by running them for a prefix of the execution and then
switching to RW for the remainder.

### Lasso Testing

The core intuition for lasso testing is that if the system's state repeats, 
provided fair scheduling, then the future is identical to the
past — the system will keep looping forever without making progress.

Finite state implies eventual repetition. When the simulator explores
exhaustively, the state space is finite (finite messages, finite processes,
finite buffer contents). That means any infinite execution must eventually
revisit a state it's seen before. The repeated state plus everything after it
forms a "lasso" — a stem followed by a loop.

To know when we are in a state we have visited before, we need to capture a
"fingerprint". In the [original
paper](https://www.microsoft.com/en-us/research/publication/lasso-detection-using-partial-state-caching/)
the fingerprint is made up of the name of states that each host is in, and the
sequence of message types in their inbox. A parallel for Hydro is to take a
hash of which hooks have pending items and which hooks are enabled. Two states
with the same fingerprint will make the same scheduling decisions in the future
— the system is memoryless given its current state. So if you see the same
fingerprint twice, provided that all the relevant state is captured in the
fingerprint, you've found a loop.

Fairness eliminates spurious counterexamples. A naïve lasso detector could flag
any cycle with a lossy channel, but some cycles are only possible under an
unfair schedule (e.g., "always drop messages from channel X"). By checking
whether every enabled fairness-subject hook delivered at least once during the
cycle, you verify that the loop persists even under a fair schedule. If a hook
was enabled at some point in the cycle and never delivered that's an unfair
execution, so we can force delivery and keep going. But if every such hook
delivered and the system still looped back to the same state — that would be a
genuine violation. Quantifying enabledness over the *whole cycle* ("enabled at
some point" rather than "enabled continuously") is what makes this **strong
fairness**; see [Ensuring Fairness](#ensuring-fairness) for why we make that
choice.

**Soundness caveat: progress is a proxy.** The argument equates "no progress"
with "the fingerprint repeats," but `StateFingerprint` omits application state
(fold accumulators, `unique()` dedup sets, message *values*), so a repeated
fingerprint need not mean a repeated global state. A receiver that accumulates
`K` samples before emitting can cycle the fingerprint while the accumulator
climbs — a false `Violation`; conversely, state that perturbs enabledness every
step blocks any repeat, ending in `BudgetExhausted`. So a `Violation` is sound
only providing the fingerprint captures progress-relevant state. This is true
when progress shows up in channel occupancy, not in general.

So far I've implemented an experimental liveness checker using lasso detection
that can be found [here](https://github.com/Benjscho/hydro/tree/liveness). 
This should serve as a proof of concept that lasso checking can work with 
Hydro.

## Design

### Flexibility

In integrating our liveness testing approach, we're probably going to end up
with something that doesn't work in all situations. To stay flexible, instead
of tying the implementation to the simulator, we should take a modular 
approach.

Broadly, liveness checking approaches will maintain some internal state, 
update it on each tick, and potentially impact the next scheduling decision.

#### Proposed Interface

We will introduce a `LivenessChecker` trait that the scheduler calls at each 
decision point. The trait separates detection from intervention. The scheduler 
retains ownership of the force-delivery execution mechanics and of its own
termination policy.

```rust
/// Identifies a specific hook: (location, cluster member, index in the hook list).
type HookId = (LocationId, Option<u32>, usize);

/// The checker's verdict at a single scheduling step.
enum LivenessVerdict {
    /// No conclusion yet — proceed with normal scheduling.
    Continue,

    /// Fairness requires these hooks to deliver now. This is to break 
    /// a cycle in a case where a hook has continuously dropped messages.
    RequireDelivery(Vec<HookId>),

    /// The checker has detected a cycle. LivenessViolation contains debug
    /// information for the developer.
    Violation(LivenessViolation),

    /// The checker exhausted its budget without a verdict. Not a proven bug,
    /// but progress could not be confirmed either.
    BudgetExhausted,
}

/// State passed to the checker at each decision point.
struct StepContext<'a> {
    /// Internal hook state: pending items and which fairness subjects are enabled.
    hooks: &'a Hooks<LocationId>,
    /// Items emitted to external output ports since the previous decision point.
    /// Nonzero means the system made observable progress.
    output_since_last: usize,
}

/// A pluggable strategy for detecting liveness violations.
trait LivenessChecker {
    /// Called each scheduling step before a non-deterministic decision.
    /// The checker inspects the step context and renders a verdict.
    fn verdict(&mut self, ctx: &StepContext) -> LivenessVerdict;

    /// Notify the checker that a fairness-subject hook delivered an item.
    /// Used for fairness tracking; strategies that measure progress by observable
    /// output (rather than internal delivery) may ignore this.
    fn record_delivery(&mut self, hook: HookId);
}
```

The context carries more than hook state because different strategies measure
"progress" differently. The lasso checker reasons purely about *internal state*
(does the hook configuration repeat?), so it only reads `hooks`. A temperature
checker reasons about how long the system has gone without making progress. It
counts scheduling steps in a "hot" state, which in Hydro translates to
unfulfilled output expectations. Keeping both signals in `StepContext` lets the
trait serve either notion of progress.

The verdict deliberately keeps four distinct outcomes separate. In particular,
`Violation` (a proven/heuristic bug — the test must fail) and `BudgetExhausted`
("I gave up before concluding" — not a bug) need different handling, and the
checker no longer constructs the scheduler's `QuiescenceReason` — that
vocabulary stays internal to the scheduler. `Violation` carries a structured
`LivenessViolation` so the checker, which actually knows *why* it gave up,
supplies the explanation (the lasso checker reports "fair cycle at step N"; the
temperature checker reports "no external output for N steps").

#### Scheduler Integration

The scheduler calls into the checker at each scheduling step and maps the
verdict to its own termination policy:

```rust
// In LaunchedSim::scheduler()
let mut checker: Box<dyn LivenessChecker> = self.make_liveness_checker();

// In the scheduling loop: assemble step context, then dispatch on verdict.
let ctx = StepContext {
    hooks: &self.hooks,
    output_since_last: self.take_output_emitted_count(),
};
match checker.verdict(&ctx) {
    LivenessVerdict::Continue => { /* normal nondeterministic scheduling */ }
    LivenessVerdict::RequireDelivery(hooks) => {
        force_delivery_targets = hooks;
    }
    LivenessVerdict::Violation(v) => {
        self.report_violation(&v); // formats v.summary into the failure message
        self.quiescence.wait_for_resume(QuiescenceReason::LivenessViolation, last_drop).await;
        continue;
    }
    LivenessVerdict::BudgetExhausted => {
        self.quiescence.wait_for_resume(QuiescenceReason::Truncated, last_drop).await;
        continue;
    }
}
```

`QuiescenceReason` stays a scheduler-internal concept; the scheduler translates
each verdict into its own termination and messaging. The force-delivery
execution logic (resolving hooks, running ticks deterministically to feed data
to forced hooks, etc.) is common infrastructure in the scheduler that any
checker can trigger via `RequireDelivery`.

Supplying `output_since_last` requires scheduler plumbing to track items
emitted to external output ports. This is observable through the external-output
channels. The lasso checker doesn't need this signal, so the plumbing is only
strictly required once an output-driven checker (like temperature) is enabled.

#### Strategy: Lasso Detection (Default)

A `LassoDetector` implements the trait, providing soundness for exhaustive
testing: if it declares a liveness violation, the system is stuck in a
cycle under fair scheduling.

```rust
impl LivenessChecker for LassoDetector {
    fn verdict(&mut self, ctx: &StepContext) -> LivenessVerdict {
        // Lasso reasons purely about internal state; it ignores the output signals.
        match self.step(ctx.hooks) {
            LassoResult::Continue => LivenessVerdict::Continue,
            LassoResult::ForceDelivery(targets) => LivenessVerdict::RequireDelivery(targets),
            LassoResult::LivenessViolation => LivenessVerdict::Violation(LivenessViolation {
                summary: format!(
                    "fair cycle: all enabled channels delivered yet state repeated at step {}",
                    self.trace.len()
                ),
                involved: self.cycle_hooks(),
            }),
            LassoResult::Truncated => LivenessVerdict::BudgetExhausted,
        }
    }

    fn record_delivery(&mut self, hook: HookId) {
        // Update the fairness record in the trace entry for this cycle.
        self.record_delivery_internal(hook);
    }
}
```

**Properties:** Sound within bounded state space, modulo the fingerprint
abstraction (see the [Lasso Testing](#lasso-testing) soundness caveat and
[Ensuring Fairness](#ensuring-fairness)). O(trace length) memory. No false
positives under that assumption. Works for exhaustive mode with small state
spaces.

#### Strategy: Temperature Checking

A temperature-based heuristic inspired by P/Coyote. The key adaptation to Hydro:
a P monitor's **hot state** means "the system owes progress," and in Hydro
progress is defined by the production of external output. The system is *hot*
when scheduling steps pass without any output being emitted to external ports,
and it *cools* whenever output is produced. The temperature is a single global
measure of "how many steps have passed without external output," not a per-hook
counter. The test's output assertions (`assert_yields`, etc.) are what give
liveness testing its meaning — they define the "good thing" that must eventually
happen — but the scheduler doesn't need to know about them directly; it only
observes whether output is flowing.

```rust
struct TemperatureChecker {
    /// Consecutive scheduling steps with no external output produced.
    temperature: usize,
    /// Declare a liveness violation once the temperature exceeds this bound.
    violation_threshold: usize,
    /// Once this hot, start forcing enabled fairness subjects to deliver, in
    /// case one of them is gating the expected output.
    force_threshold: usize,
}

impl LivenessChecker for TemperatureChecker {
    fn verdict(&mut self, ctx: &StepContext) -> LivenessVerdict {
        if ctx.output_since_last > 0 {
            // Observable progress was made: the system is cold.
            self.temperature = 0;
            return LivenessVerdict::Continue;
        }

        // Hot: no external output was produced this step.
        self.temperature += 1;

        if self.temperature >= self.violation_threshold {
            return LivenessVerdict::Violation(LivenessViolation {
                summary: format!(
                    "no external output produced for {} consecutive steps",
                    self.temperature
                ),
                involved: enabled_fairness_subjects(ctx.hooks),
            });
        }

        if self.temperature >= self.force_threshold {
            // Nudge the system: force every enabled fairness subject to deliver,
            // since one may be on the path to the expected output.
            let targets = enabled_fairness_subjects(ctx.hooks);
            if !targets.is_empty() {
                return LivenessVerdict::RequireDelivery(targets);
            }
        }

        LivenessVerdict::Continue
    }

    fn record_delivery(&mut self, _hook: HookId) {
        // No-op: this strategy cools on *output* progress (`output_since_last`),
        // not on internal hook delivery. Contrast with the lasso checker, which
        // uses delivery feedback for its fairness record.
    }
}
```

Note how `record_delivery` is a no-op here. Delivering an internal message
isn't the "good thing" — producing external output is. A fairness subject can
deliver every step while the expected output never appears (e.g. the wrong
value loops), and the system is still hot. This is why progress is measured by
output observation, not hook-level delivery.

**Properties:** O(1) state (a single counter) plus O(# hooks) to pick force
targets. Constant per-step cost, no trace storage or hashing. *Output-aware*:
it measures whether the system is producing externally visible results, so it
naturally catches livelocks where internal messages keep flowing but no output
arrives. Can produce false positives (threshold too low — a slow-but-converging
computation) and false negatives (threshold too high). Better suited to fuzz
mode where soundness isn't required but long executions need a bounded runtime.

#### Strategy Selection

Users configure the strategy via `SimFlow`:

```rust
pub enum LivenessStrategy {
    /// Lasso detection with state fingerprinting. Sound for exhaustive testing.
    Lasso { max_steps: usize },
    /// Temperature heuristic. Cheap per step, good for fuzzing.
    Temperature { force_threshold: usize, violation_threshold: usize },
    /// No liveness checking. Use for safety-only testing.
    None,
}

impl SimFlow {
    pub fn liveness_strategy(mut self, strategy: LivenessStrategy) -> Self {
        self.liveness_strategy = strategy;
        self
    }
}
```

The default is `Lasso { max_steps: 200 }`. A convenience method
`max_lasso_steps()` is sugar for
`liveness_strategy(LivenessStrategy::Lasso { .. })`.

#### Future Strategies

The trait is intentionally minimal to accommodate future approaches:

- **Hybrid:** Use temperature as a fast pre-filter during fuzzing, switch to
  lasso for the replay/minimization.
- **Monitor-based:** The temperature checker treats the test's output
  assertion as an implicit hot/cold monitor. A richer variant could let users
  declare explicit progress monitors (like P's hot/cold states) over
  application-level state, rather than relying solely on the output assertion.
- **Partial-order aware:** A checker that understands causal dependencies 
  between hooks and only forces delivery when causally necessary.

### Channel Types

Liveness testing introduces support for new channel types: `lossy` and
`lossy_retry`. (`lossy_delayed_forever` is a preexisting channel type that
only supports safety testing, as it models dropped messages through
an infinite delay.)

| Channel | Drops messages | Retries | Output type | Liveness guarantee |
|---------|--------------|---------|-------------|-------------------|
| `fail_stop()` | No | N/A | `ExactlyOnce, TotalOrder` | Yes (trivially) |
| `lossy(nondet)` | Yes (permanent) | No | `ExactlyOnce, TotalOrder` | No |
| `lossy_retry()` | Yes (transient) | Yes (built-in) | `AtLeastOnce, NoOrder` | Yes (fairness) |
| `lossy_delayed_forever()` | Yes (infinite delay) | No | `ExactlyOnce, NoOrder` | No (safety-only) |

### Ensuring Fairness

**Decision: the simulator will assume _strong fairness_ on hooks
(lossy/retry channels and interval timers).**

Without a fairness assumption every liveness assertion has a trivial
counterexample — the scheduler just drops every message forever. The only
question is *how strong* an assumption to make:

| TLA+ | Hydro equivalent |
|---|---|
| `A` enabled | lossy hook: buffer non-empty; interval: always |
| `A` taken | the hook makes a nontrivial decision (delivers/fires) |
| `WF` | *continuously* enabled => eventually delivers |
| `SF` | *cyclically* enabled => eventually delivers |

We choose SF, because Hydro's retry pattern needs it: producing a value
repeatedly (`sample_every`) over a `lossy()` channel sends a fresh message each
time, so a dropped sample leaves the buffer empty until the next arrives — the
channel is enabled only intermittently. WF would offer no guarantee there,
making "retry forever over a lossy link" not provably live even though our
intuition would say otherwise. This is the check the lasso detector performs
(quantifying cycle enabledness with `any`; see [Lasso
Testing](#lasso-testing)). Across channel types:

- `lossy()` + retry (`sample_every`): live because of SF — enabled infinitely
  often as samples keep arriving.
- `lossy_retry()`: the undelivered message persists in the hook's buffer, so it
  stays continuously enabled and holds even under WF.
- `lossy()` single send: enabled only once, so no guarantee of being received.

### Exhaustiveness vs. Fuzzing

There is divergence in prior art on whether to pursue exhaustive testing or
lean on fuzzing with limited steps. P shows that fuzzing approaches can be
effective at finding bugs while supporting larger state spaces. The state space
grows quickly for liveness, which can lead to exhaustive simulations becoming
intractably expensive.

We support both approaches, so the remaining choice is what should be our
_default_. Following P's example, we set an adjustable search budget.

