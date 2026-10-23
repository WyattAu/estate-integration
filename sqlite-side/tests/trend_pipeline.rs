#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::indexing_slicing)]
//! Suite: `trend_pipeline` — `trendkit` composed with the engine it was
//! extracted from.
//!
//! `trendkit-core` depends on `signalkit-core` and re-exports its `clock`,
//! `error`, `model`, and `score` layers. That extraction is the reason there is
//! no duplication here, and it is also the thing that can silently rot: if a
//! copy of `model` or `score` ever reappears inside trendkit-core, both crates
//! compile, both pass their own tests, and a store written by one is no longer
//! readable by the other.
//!
//! So the assertion this suite exists for is *type identity across the
//! re-export*: a `signalkit_core::Observation` written through trendkit's
//! `Store` must read back as the same type, and the two crates' scoring must
//! agree number for number. The compiler proves the first half at this suite's
//! call sites; the runtime half is what a copy would break.
//!
//! `trendkit-profile` is pinned alongside because it is the consumer of the
//! score, and its `Profile` is where the personal ranking lands.


use signalkit_core::model::{Observation, SourceKind, SubjectRecord};
use signalkit_core::source::{CollectCtx, CollectReport, TrendSource};
use signalkit_core::store::Store;
use clock_kit::MockClock;
use std::sync::Arc;
use trendkit_core as tk;

/// The re-export is the real type, not a lookalike: writing through trendkit's
/// alias into the engine's store, then reading back, must be a no-op.
#[test]
fn trendkits_model_is_signalkits_model() {
    // Compile-time identity. If trendkit-core ever re-introduces a copy of
    // `Observation`, these bindings stop type-checking and this test is red.
    let obs: signalkit_core::model::Observation = tk::model::Observation {
        ts: 1_000,
        score: 100,
        extra1: 0,
        extra2: 0,
    };
    assert_eq!(obs.score, 100);

    // Runtime identity through the store.
    let store = Store::open_in_memory().expect("opens");
    let subject = SubjectRecord {
        kind: SourceKind::GithubRepo,
        key: "owner/repo".to_owned(),
        title: Some("Repo".to_owned()),
        lang: Some("Rust".to_owned()),
        url: None,
        topics: vec!["rust".to_owned()],
        created_at: None,
    };
    let id = store.upsert_subject(&subject).expect("upsert");
    store.insert_observation(id, &obs).expect("insert");
    let rows = store.velocity_rows(7, 10).expect("rows");
    assert_eq!(rows.len(), 1);
}

/// The two crates' scoring is the same function. A copied `score` would drift
/// the moment one side is tuned, and the drift would be invisible until a
/// profile disagreed with a velocity row.
#[test]
fn the_two_crates_score_identically() {
    for (prev, now, window) in [(100i64, 160i64, 86_400i64), (0, 1, 60), (500, 0, 3_600)] {
        let a = signalkit_core::score::velocity_per_day(prev, now, window);
        let b = tk::score::velocity_per_day(prev, now, window);
        assert_eq!(a, b, "velocity diverged for {prev}/{now}/{window}");
    }
    for (v, topics, lang) in [(0.0, 0usize, false), (10.0, 3, true), (-4.0, 1, false)] {
        let a = signalkit_core::score::personal_score(v, topics, lang);
        let b = tk::score::personal_score(v, topics, lang);
        assert_eq!(a, b, "personal score diverged for {v}/{topics}/{lang}");
    }
}

/// The documented clamp: a degenerate window cannot divide by zero. Asserted
/// through trendkit's re-export so the copy, if one ever appears, is held to
/// the same rule.
#[test]
fn the_reexported_score_clamps_a_degenerate_window() {
    for window in [0, -1] {
        let v = tk::score::velocity_per_day(100, 160, window);
        assert!(v.is_finite(), "window {window} produced {v}");
    }
}

/// A velocity row's number is reproducible from the observations that produced
/// it, using trendkit's own score function. This is the end-to-end property:
/// store, model, and score agree, through the re-export.
#[test]
fn a_velocity_row_is_reproducible_from_its_observations() {
    let store = Store::open_in_memory().expect("opens");
    let subject = SubjectRecord {
        kind: SourceKind::GithubRepo,
        key: "owner/other".to_owned(),
        title: None,
        lang: None,
        url: None,
        topics: Vec::new(),
        created_at: None,
    };
    let id = store.upsert_subject(&subject).expect("upsert");
    let day: i64 = 86_400;
    store.insert_observation(id, &Observation { ts: day, score: 10, extra1: 0, extra2: 0 }).expect("insert");
    store.insert_observation(id, &Observation { ts: day * 8, score: 70, extra1: 0, extra2: 0 }).expect("insert");

    let rows = store.velocity_rows(14, 10).expect("rows");
    assert_eq!(rows.len(), 1, "one subject");
    let row = &rows[0];

    // Reproduce the number from the raw readings through the re-exported score.
    let expected = tk::score::velocity_per_day(10, 70, day * 7);
    assert!(
        (row.velocity_per_day - expected).abs() < 1e-9,
        "row says {} but the score function says {expected}",
        row.velocity_per_day
    );
    // And the personal score on top of it is finite and attributable.
    let personal = tk::score::personal_score(row.velocity_per_day, 0, false);
    assert!(personal.is_finite());
}

/// The real thing: a source implemented outside the crate, driven through the
/// engine's own context, writing into the store, scored through the re-export.
///
/// `TrendSource::collect` is async-fn-in-trait and deliberately not `Send`
/// (collection is sequential and the SQLite connection stays confined to one
/// task), so the executor is a single-threaded `block_on`. The clock is a
/// `MockClock` and the store is in-memory, so the run is deterministic and
/// touches no network.
#[test]
fn a_caller_supplied_source_collects_through_the_engine() {
    let store = Arc::new(Store::open_in_memory().expect("opens"));
    let ctx = CollectCtx {
        clock: Arc::new(MockClock::new()),
        store: Arc::clone(&store),
        pages: 1,
        window_days: 14,
    };

    struct OneRepo;
    impl TrendSource for OneRepo {
        fn name(&self) -> &'static str {
            "fixed"
        }
        async fn collect(&self, ctx: &CollectCtx) -> tk::Result<CollectReport> {
            let subject = SubjectRecord {
                kind: SourceKind::GithubRepo,
                key: "owner/repo".to_owned(),
                title: Some("Repo".to_owned()),
                lang: Some("Rust".to_owned()),
                url: None,
                topics: vec!["rust".to_owned()],
                created_at: None,
            };
            let id = ctx.store.upsert_subject(&subject).expect("upsert");
            // Two readings one day apart: the velocity is then reproducible.
            for (ts, score) in [(1_000i64, 10i64), (1_000 + 86_400, 70)] {
                ctx.store
                    .insert_observation(id, &Observation { ts, score, extra1: 0, extra2: 0 })
                    .expect("insert");
            }
            Ok(CollectReport { source: "fixed", pages: 1, items: 1 })
        }
    }

    futures::executor::block_on(OneRepo.collect(&ctx)).expect("collection runs");

    // The collected snapshots are in the store, and the number the engine
    // derives is the number the score function produces.
    let rows = store.velocity_rows(14, 10).expect("rows");
    assert_eq!(rows.len(), 1);
    let expected = tk::score::velocity_per_day(10, 70, 86_400);
    assert!(
        (rows[0].velocity_per_day - expected).abs() < 1e-9,
        "engine says {} but the score says {expected}",
        rows[0].velocity_per_day
    );
}