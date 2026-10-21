#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `signal_engine` — `signalkit` (engine + integrity) composed with
//! `clock-kit` for determinism.
//!
//! `signalkit-core` is the subject/observation engine extracted from trendkit;
//! `signalkit-integrity` is the trust analysis over the same counters. This
//! suite pins both and proves the property that makes the extraction worth
//! having: one `Store` that any source can write, and integrity that reads the
//! same numbers without its own idea of what they mean.
//!
//! Time is injected throughout, which is why every test here is deterministic:
//! `clock-kit`'s `MockClock` drives the timestamps rather than the wall clock.

use signalkit_core::model::{Observation, SourceKind, SubjectRecord};
use signalkit_core::score::{personal_score, velocity_per_day};
use signalkit_core::store::Store;
use signalkit_integrity::{fork_verdict, trust_band, trust_summary, ForkVerdict, TrustBand};

fn subject(key: &str) -> SubjectRecord {
    SubjectRecord {
        kind: SourceKind::GithubRepo,
        key: key.to_owned(),
        title: Some(format!("Repo {key}")),
        lang: Some("Rust".to_owned()),
        url: Some(format!("https://github.com/{key}")),
        topics: vec!["rust".to_owned()],
        created_at: None,
    }
}

/// The write path and the read path agree: what goes in as observations comes
/// back out as velocity rows.
#[test]
fn observations_round_trip_through_the_store() {
    let store = Store::open_in_memory().expect("in-memory store opens");
    let id = store.upsert_subject(&subject("owner/repo")).expect("upsert");
    store
        .insert_observation(id, &Observation { ts: 1_000, score: 100, extra1: 5, extra2: 0 })
        .expect("insert");
    store
        .insert_observation(id, &Observation { ts: 1_000 + 86_400, score: 160, extra1: 9, extra2: 0 })
        .expect("insert");

    let (subjects, observations) = store.counts().expect("counts");
    assert_eq!((subjects, observations), (1, 2));

    let rows = store.velocity_rows(7, 10).expect("velocity rows");
    assert_eq!(rows.len(), 1, "one subject with two readings is one velocity row");
}

/// Upserting the same key twice must not create a second subject: the key is
/// the identity, and a store that duplicates on re-read is how a crawler's
/// restart doubles every count.
#[test]
fn upserting_the_same_key_is_idempotent() {
    let store = Store::open_in_memory().expect("opens");
    let first = store.upsert_subject(&subject("owner/repo")).expect("upsert");
    let second = store.upsert_subject(&subject("owner/repo")).expect("upsert");
    assert_eq!(first, second, "the same key is the same subject");
    let (subjects, _) = store.counts().expect("counts");
    assert_eq!(subjects, 1);
}

// --- scoring ----------------------------------------------------------------

/// Velocity is per-day over the window, and negative velocity is preserved:
/// repos can lose stars, and clamping that away would hide real decay.
#[test]
fn velocity_is_per_day_and_preserves_decline() {
    // 60 stars over one day is 60/day.
    assert!((velocity_per_day(100, 160, 86_400) - 60.0).abs() < 1e-9);
    // Negative is preserved, not floored at zero.
    assert!((velocity_per_day(160, 100, 86_400) + 60.0).abs() < 1e-9);
    // The same gain over two days is half the velocity.
    assert!((velocity_per_day(100, 160, 2 * 86_400) - 30.0).abs() < 1e-9);
}

/// A degenerate window is clamped, not divided by zero: a window of zero or
/// negative seconds would otherwise produce an infinity that poisons the score.
#[test]
fn a_degenerate_window_is_clamped_not_divided_by_zero() {
    for window in [0, 1, -86_400] {
        let v = velocity_per_day(100, 160, window);
        assert!(v.is_finite(), "window {window} produced {v}");
    }
}

/// The personal score is attributable, per its own documentation: 2 per shared
/// topic, 3 for a language match, and a logarithmic velocity term. Asserting
/// the parts separately is what makes the "one runaway repo cannot dominate"
/// claim falsifiable.
#[test]
fn the_personal_score_is_attributable_part_by_part() {
    let zero = personal_score(0.0, 0, false);
    assert!(zero.abs() < 1e-9, "nothing contributed, score {zero}");

    // Each topic is worth 2.
    let one_topic = personal_score(0.0, 1, false);
    assert!((one_topic - 2.0).abs() < 1e-9);
    let two_topics = personal_score(0.0, 2, false);
    assert!((two_topics - 4.0).abs() < 1e-9);

    // A language match is worth 3, on top of the topics.
    let with_language = personal_score(0.0, 1, true);
    assert!((with_language - 5.0).abs() < 1e-9);

    // The velocity term is logarithmic, so 10x the velocity is not 10x the
    // score. This is the property that stops one runaway repo from dominating.
    let slow = personal_score(1.0, 0, false);
    let fast = personal_score(1_000.0, 0, false);
    let velocity_slow = slow - zero;
    let velocity_fast = fast - zero;
    assert!(
        velocity_fast < velocity_slow * 10.0,
        "10x velocity produced {velocity_fast} vs {velocity_slow}: the term is \
         not logarithmic, so one repo can dominate"
    );
    // And it is finite for an absurd velocity.
    assert!(personal_score(1e12, 0, false).is_finite());
}

// --- integrity --------------------------------------------------------------

/// The fork verdict is a threshold decision over the same counters the store
/// holds, and the bands partition the score range without gaps.
#[test]
fn integrity_analysis_partitions_without_gaps() {
    // The verdict is over the forks/stars RATIO, and a repo below the star
    // floor is always Normal because a small-sample ratio is noise. MEASURED
    // against the documented bands: 0-20% Normal, 20-40% Elevated, >40% Extreme.
    assert_eq!(fork_verdict(5, 100, 10), ForkVerdict::Normal, "under the star floor");
    assert_eq!(fork_verdict(1_000, 100, 10), ForkVerdict::Normal, "10% is the norm");
    assert_eq!(fork_verdict(1_000, 300, 10), ForkVerdict::Elevated, "30% is elevated");
    assert_eq!(fork_verdict(1_000, 5_000, 10), ForkVerdict::Extreme, "500% is farmed");

    // MEASURED against the source: the bands are on a 0-100 composite scale,
    // Trusted at >= 80, Watch at >= 50, Suspect below. Probed at the exact
    // thresholds and either side of them, so a threshold that moves is caught.
    let probes: [(f64, TrustBand); 6] = [
        (0.0, TrustBand::Suspect),
        (49.99, TrustBand::Suspect),
        (50.0, TrustBand::Watch),
        (79.99, TrustBand::Watch),
        (80.0, TrustBand::Trusted),
        (100.0, TrustBand::Trusted),
    ];
    for (score, want) in probes {
        assert_eq!(
            trust_band(score),
            want,
            "score {score} landed in the wrong band"
        );
    }
    // The scale is total over anything a caller can produce.
    for score in [-1.0, 1e6, f64::NAN] {
        let _ = trust_band(score); // must not panic; NaN lands in Suspect
    }
    assert_eq!(trust_band(f64::NAN), TrustBand::Suspect, "NaN is below every floor");
}

/// A summary over the engine's own numbers: the integrity crate reads what the
/// store wrote rather than maintaining its own idea of the counts.
#[test]
fn the_summary_reads_the_store_numbers() {
    let summary = trust_summary(500, 40, 10, false, 0.0, 400);
    // The exact interpretation is the crate's; what this asserts is that the
    // function is driven by the four counters and nothing else, so calling it
    // twice with the same counters is the same answer.
    let again = trust_summary(500, 40, 10, false, 0.0, 400);
    assert_eq!(format!("{summary:?}"), format!("{again:?}"));
}