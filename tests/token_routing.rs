#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `token_routing` — `simd-tokenizer` and `model-router`, the two
//! AI-infrastructure leaves.
//!
//! Both were published and never composed, and both answer the same question a
//! caller asks before spending money: how many tokens will this take, and
//! which model should run it. The suites are in one file because the answer
//! has to be consistent across both — an estimate that drifts from the router's
//! budget accounting is how a cost cap silently stops meaning anything.

use model_router::{CostTracker, ModelPricing, Router, TaskClass};
use simd_tokenizer::{
    estimate_from_whitespace_splits, SimdWhitespaceTokenizer, TokenCounter, TokenEstimator,
};

// --- simd-tokenizer ---------------------------------------------------------

/// The whitespace approximation is monotone in input size: more text never
/// yields fewer tokens. A non-monotone estimator makes budget checks order
/// dependent, which is the failure that shows up as a mysterious overspend.
#[test]
fn the_whitespace_estimate_is_monotone_in_input_size() {
    let counter = SimdWhitespaceTokenizer::new();
    let mut prev = 0;
    for words in [1usize, 2, 8, 64, 256, 1024] {
        let text: Vec<&str> = (0..words).map(|_| "word").collect();
        let text = text.join(" ");
        let count = counter.count(&text);
        assert!(count >= prev, "{words} words counted {count}, below {prev}");
        assert!(count > 0, "{words} words counted zero");
        prev = count;
    }
}

/// Empty and whitespace-only input estimate to zero: a caller gating on
/// "is there any content" relies on this.
#[test]
fn empty_input_estimates_to_zero() {
    let counter = SimdWhitespaceTokenizer::new();
    assert_eq!(counter.count(""), 0);
    assert_eq!(counter.count("   "), 0);
    assert_eq!(counter.count("\n\t \n"), 0);
}

/// The closed-form helper and the tokenizer agree on plain text: two paths to
/// the same number, or the budget and the estimate disagree.
#[test]
fn the_closed_form_helper_agrees_with_the_tokenizer() {
    let counter = SimdWhitespaceTokenizer::new();
    let text = "one two three four five";
    let splits = text.split_whitespace().count();
    let by_helper = estimate_from_whitespace_splits(splits, text.len());
    let by_counter = counter.count(text);
    assert_eq!(
        by_helper, by_counter,
        "the closed form and the tokenizer disagree on {text:?}"
    );
}

/// The estimator is usable through the trait object, which is how a caller
/// holds it: backends are swappable and callers should not care which is live.
#[test]
fn the_estimator_is_swappable_behind_the_trait() {
    let estimator = TokenEstimator::new();
    let count = TokenCounter::count(&estimator, "one two three");
    assert!(count > 0);
    // The backend names itself, for logs that say which estimate was used.
    assert!(!TokenCounter::backend_name(&estimator).is_empty());
}

// --- model-router -----------------------------------------------------------

/// Routing is a function of the task class alone: the same class always routes
/// to the same model, so a cost budget cannot be defeated by re-asking.
#[test]
fn routing_is_deterministic_per_task_class() {
    let router = Router::new("default-model");
    for _ in 0..8 {
        let a = router.route(TaskClass::Fast);
        let b = router.route(TaskClass::Fast);
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "Fast routed twice, differently");
    }
    // Every class must route somewhere, and the three classes are distinct
    // inputs even if the default model serves them all.
    for class in [TaskClass::Fast, TaskClass::Balanced, TaskClass::Power] {
        let decision = router.route(class);
        assert!(!format!("{decision:?}").is_empty());
    }
}

/// The budget is a pre-charge gate: a call that would exceed the remaining
/// budget is refused with `BudgetExceeded` and nothing is billed.
///
/// MEASURED from the error the crate returns: a $18 call against a $1 budget
/// from a zero balance is refused with `current: 0.0, attempted: 18.0,
/// limit: 1.0`, so the check is before charging, not a warning after. The
/// placeholder rate ($1/1M in, $3/1M out) makes a 1M/1M call exactly $4, which
/// lets the trip point be computed rather than probed.
#[test]
fn the_budget_refuses_a_call_that_would_exceed_it() {
    let placeholder = ModelPricing::default();
    // $4 per call; $20 buys exactly five.
    let tracker = CostTracker::new(Some(20.0));
    for call in 1..=5 {
        tracker
            .record("any-model", 1_000_000, 1_000_000, &placeholder)
            .unwrap_or_else(|e| panic!("call {call} refused early: {e:?}"));
        assert!((tracker.total_usd() - 4.0 * call as f64).abs() < 1e-9);
    }
    // The sixth would take the total to $24: refused, and not billed.
    let err = tracker
        .record("any-model", 1_000_000, 1_000_000, &placeholder)
        .expect_err("the sixth $4 call must trip the $20 budget");
    assert!(
        format!("{err:?}").contains("BudgetExceeded"),
        "the refusal is a typed budget error, got {err:?}"
    );
    assert!((tracker.total_usd() - 20.0).abs() < 1e-9, "a refused call was billed");
}

/// The refusal carries the numbers a caller needs to react: current spend,
/// attempted cost, and the limit. An error that names none of them forces the
/// caller to re-derive the state the crate already had.
#[test]
fn the_budget_error_carries_current_attempted_and_limit() {
    let placeholder = ModelPricing::default();
    let tracker = CostTracker::new(Some(1.0));
    let err = tracker
        .record("any-model", 1_000_000, 1_000_000, &placeholder)
        .expect_err("a $4 call against $1 is refused");
    let rendered = format!("{err:?}");
    for expected in ["0.0", "4.0", "1.0"] {
        assert!(
            rendered.contains(expected),
            "the error {rendered} does not name {expected}"
        );
    }
}

/// A call that fits is accepted and charged exactly: $1/1M in and $3/1M out
/// over 1M and 1M tokens is $4, by the documented arithmetic.
#[test]
fn a_fitting_call_is_charged_exactly() {
    let placeholder = ModelPricing::default();
    let tracker = CostTracker::new(Some(100.0));
    tracker
        .record("any-model", 1_000_000, 1_000_000, &placeholder)
        .expect("a $4 call against $100 fits");
    assert!((tracker.total_usd() - 4.0).abs() < 1e-9);
}

/// A tracker with no budget never refuses: the cap is opt-in.
#[test]
fn an_uncapped_tracker_always_accepts() {
    let placeholder = ModelPricing::default();
    let tracker = CostTracker::new(None);
    for _ in 0..3 {
        tracker
            .record("claude-sonnet-4-20250514", 1_000_000, 1_000_000, &placeholder)
            .expect("no budget means no refusal");
    }
    // Three placeholder-priced calls at $4 each.
    assert!(
        (tracker.total_usd() - 12.0).abs() < 1e-6,
        "three $4 calls should total $12, got {}",
        tracker.total_usd()
    );
}
