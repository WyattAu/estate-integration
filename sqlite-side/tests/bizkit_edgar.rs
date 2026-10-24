// `expect` is how a test asserts that setup succeeded; the deny is a
// production rule and re-expressing every assertion as a `match` would
// only hide intent.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Cross-crate dogfooding for `bizkit-core`: the EDGAR full-text-search
//! parser, the filing store's dedupe-and-velocity semantics, and the idea
//! taxonomy that ties the crawl to trendkit's subject keys.
//!
//! # Findings this round
//!
//! 1. **The EDGAR wire shape is Elasticsearch's, underscores and all**:
//!    `_source`, `_score`, `_id`, with every field `#[serde(default)]`.
//!    The first fixture here invented a snake_case envelope; serde's
//!    defaults swallowed it into empty hits and the parse "passed" while
//!    carrying nothing. The leniency is a real hazard for fixture authors,
//!    which is why the store's dedupe assertion belongs in the same suite:
//!    the two empty rows collapsed into one and made the empties visible.
//! 2. **`top_filers` returns (name, form, count)** -- a tuple of bare
//!    Strings gives no clue which is which, and the natural guess (cik,
//!    name, count) is wrong. Pinned by assertion.
//!
//! # The contract under test
//!
//! `record_filings` is idempotent per document: re-recording updates
//! score and last_seen and still reports the number of rows written, so a
//! daily crawl never duplicates the population. `form_summary` is the
//! M&A velocity report's source, so its counts must match the store.

use bizkit_core::{catalog, parse_fts_json, FilingHit, FtsQuery, InvestmentLevel, Store};

const FTS_BODY: &str = r#"{
  "hits": {
    "hits": [
      {
        "_id": "0001234567-26-000042:acme8k.htm",
        "_score": 12.5,
        "_source": {
          "ciks": ["0001234567"],
          "display_names": ["Acme Industries, Inc."],
          "file_type": "8-K",
          "period_ending": "2026-10-15"
        }
      },
      {
        "_id": "0000766704-26-000011:s1.htm",
        "_score": 4.0,
        "_source": {
          "ciks": ["0000766704"],
          "display_names": ["Xerocopy Ltd."],
          "file_type": "S-1",
          "period_ending": "2026-09-30"
        }
      }
    ]
  }
}"#;

fn hits() -> Vec<FilingHit> {
    parse_fts_json(FTS_BODY).expect("the fixture is the documented FTS shape")
}

#[test]
fn an_fts_body_parses_into_filing_hits() {
    let parsed = hits();
    assert_eq!(parsed.len(), 2);

    let acme = parsed
        .iter()
        .find(|h| h.display_name == "Acme Industries, Inc.")
        .expect("the acme hit");
    assert_eq!(acme.cik, "0001234567");
    assert_eq!(acme.form, "8-K");
    assert_eq!(acme.period_ending, "2026-10-15");
    assert_eq!(acme.doc, "0001234567-26-000042:acme8k.htm");
}

#[test]
fn a_body_that_is_not_the_fts_shape_is_refused() {
    assert!(parse_fts_json("<html>not json</html>").is_err());
    // A body with no hits envelope is a valid EMPTY result set, not an
    // error: every field defaults, so EDGAR's empty pages parse to zero
    // hits. Lenient by design; the empties only become visible downstream.
    let empty = parse_fts_json("{}").expect("an empty page parses");
    assert!(empty.is_empty());
}

#[test]
fn recording_is_idempotent_per_document_and_summary_counts_match() {
    let store = Store::open_in_memory().expect("opens");
    let query = FtsQuery {
        q: "\"acquisition agreement\"".to_string(),
        forms: Some("8-K".to_string()),
        startdt: "2026-10-01".to_string(),
        enddt: "2026-10-31".to_string(),
    };
    let _ = query; // the query label travels with the rows, not the count

    let first = store.record_filings(&hits(), "fts:8-K", 1_760_000_000).expect("records");
    assert_eq!(first, 2, "both hits are new");
    let second = store.record_filings(&hits(), "fts:8-K", 1_760_100_000).expect("re-records");
    assert_eq!(second, 2, "rows written, not rows added: the upsert rewrites");

    let summary = store.form_summary().expect("summarizes");
    assert_eq!(summary.len(), 2, "two form types");
    assert_eq!(summary[0], ("8-K".to_string(), 1, 1_760_000_000, 1_760_100_000));
    assert_eq!(summary[1], ("S-1".to_string(), 1, 1_760_000_000, 1_760_100_000));

    let filers = store.top_filers(10).expect("ranks");
    assert_eq!(filers.len(), 2);
    // The tuple is (display name, form, appearances): repeat filers are the
    // acquisition signal, so the ranking counts documents per filer.
    assert!(
        filers.iter().any(|(name, form, _)| name == "Acme Industries, Inc." && form == "8-K"),
        "the acme 8-K appears under its filer name"
    );
}

#[test]
fn the_catalog_ties_every_idea_to_trendkit_evidence_keys() {
    let catalog = catalog();
    assert!(!catalog.is_empty(), "an empty catalog is a broken build");
    for idea in &catalog {
        assert!(
            !idea.signals.is_empty(),
            "{} has no trendkit signals: demand with no evidence key",
            idea.name
        );
        assert!(!idea.primary_risk.is_empty(), "{} names no primary risk", idea.name);
    }
    // The level enums are ordered by escalation; the ordering is the report's
    // sort key, so it is pinned here rather than trusted.
    assert!(InvestmentLevel::NearZero < InvestmentLevel::Low);
    assert!(InvestmentLevel::Low < InvestmentLevel::High);
}
