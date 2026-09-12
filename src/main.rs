#![forbid(unsafe_code)]
//! Scenario registry: what each integration test wires together.
//!
//! The actual tests live in `tests/` — one file per scenario, one `#[test]`
//! or `#[tokio::test]` per behavior. This module exists so `cargo run` can
//! summarize the suite.

/// One cross-crate scenario.
pub struct Scenario {
    /// Short identifier, matching the test file name in `tests/`.
    pub id: &'static str,
    /// What the scenario proves.
    pub summary: &'static str,
    /// Estate crates wired together, with the exact pinned versions.
    pub crates: &'static [&'static str],
}

/// All scenarios in the suite.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        id: "auth_stack",
        summary: "register (validkit email → salting hash) → login (verify → tokenkit JWT) → authenticated request (decode + revocation), plus rotation and barbican HTTP guards",
        crates: &["salting =1.2.1", "tokenkit =0.4.0", "validkit =1.3.0", "barbican =0.2.1"],
    },
    Scenario {
        id: "resilient_api",
        summary: "axum service with per-IP rate limiting + circuit breaking + livez/readyz/startup probes + per-request spans; 429s, open breakers, dependency-driven readiness",
        crates: &["breaker =2.0.0", "healthkit =1.2.0", "throttle-kit =1.1.1", "otelkit =2.0.2", "axum 0.8"],
    },
    Scenario {
        id: "mail_pipeline",
        summary: "validate recipient → build MIME → evaluate inbound filter plan (keep/vacation/flag) → send via mock SendGrid with receipt",
        crates: &["mailkit =0.3.0", "validkit =1.3.0", "sieve-kit =0.2.1"],
    },
    Scenario {
        id: "media_upload",
        summary: "upload bytes → sniff/bomb-guard → EXIF-orient → parallel variants → blobkit store → cas-kit dedup (second identical upload stores nothing)",
        crates: &["media-kit =0.2.1", "blobkit =0.4.1", "cas-kit =0.2.1", "validkit =1.3.0"],
    },
    Scenario {
        id: "sync_client",
        summary: "mail-sync-kit engines against an in-memory MockStore seam; sync events flow over a persistent bus into SQLite and replay after restart",
        crates: &["mail-sync-kit =0.1.0", "eventbus-kit =0.3.5"],
    },
];

fn main() {
    println!("estate-integration — cross-crate dogfooding suite\n");
    for scenario in SCENARIOS {
        println!("  tests/{}.rs", scenario.id);
        println!("    {}", scenario.summary);
        println!("    crates: {}", scenario.crates.join(", "));
        println!();
    }
    println!("run the suite with: cargo test");
}
