//! Persistence through to the reports: post to PostgreSQL, read back, and let
//! `report-kit` aggregate.
//!
//! The claim this suite pins: the database is a *faithful* ledger — entries
//! stored, reversed, and re-read produce the same trial balance the in-memory
//! model would, and a reversal recorded in Postgres changes the reports the
//! same way it changes the books. Storage that changes accounting semantics
//! in transit is worse than no storage.
//!
//! Harness: `TEST_DATABASE_URL` if set, else a throwaway container. Skipped
//! cleanly when neither exists — the skip is visible in the output.

#![allow(clippy::expect_used, clippy::panic)]

use double_entry::money::currencies;
use double_entry::{JournalEntry, Line};
use ledger_pg::{ensure_schema, into_ledger, PgLedger};
use report_kit::{BalanceSheet, TrialBalance};

struct PgInstance {
    dsn: String,
    container: Option<String>,
}

impl Drop for PgInstance {
    fn drop(&mut self) {
        if let Some(id) = &self.container {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", id])
                .output();
        }
    }
}

async fn start_postgres() -> Option<PgInstance> {
    if let Ok(dsn) = std::env::var("TEST_DATABASE_URL") {
        return Some(PgInstance {
            dsn,
            container: None,
        });
    }
    let id = format!("estate-ledger-pg-{}", std::process::id());
    let started = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "-d",
            "--name",
            &id,
            "-e",
            "POSTGRES_PASSWORD=test",
            "-p",
            "54331:5432",
            "postgres:16-alpine",
        ])
        .output()
        .ok()?;
    if !started.status.success() {
        eprintln!("TEST SKIPPED: docker unavailable");
        return None;
    }
    let dsn = "postgres://postgres:test@127.0.0.1:54331/postgres";
    let mut attempt = 0;
    loop {
        if sqlx_postgres_connect(dsn).await {
            break;
        }
        attempt += 1;
        if attempt > 60 {
            eprintln!("TEST SKIPPED: postgres did not become ready");
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Some(PgInstance {
        dsn: dsn.to_string(),
        container: Some(id),
    })
}

async fn sqlx_postgres_connect(dsn: &str) -> bool {
    let pool = sqlx_connect(dsn).await;
    matches!(pool, Ok(p) if !p.is_closed())
}

async fn sqlx_connect(dsn: &str) -> Result<sqlx::PgPool, sqlx::Error> {
    // sqlx is available transitively through ledger-pg; use its pool directly.
    ledger_pg_pool(dsn).await
}

async fn ledger_pg_pool(dsn: &str) -> Result<sqlx::PgPool, sqlx::Error> {
    // Re-exported pool type keeps the version graph single.
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect(dsn)
        .await
}

#[tokio::test]
async fn stored_books_aggregate_into_the_same_trial_balance() {
    let Some(instance) = start_postgres().await else {
        return;
    };
    let pool = ledger_pg_pool(&instance.dsn).await.expect("connects");
    ensure_schema(&pool).await.expect("schema");
    let ledger = PgLedger::connect(pool.clone(), "estate-reports")
        .await
        .expect("tenant");

    // A tiny July: capital in, one sale, one cost.
    let entries = [
        (
            "JE-OPEN",
            "2026-07-01",
            vec![
                Line::debit(
                    "1000",
                    double_entry::Amount::minor(500_000, currencies::GBP),
                ),
                Line::credit(
                    "3100",
                    double_entry::Amount::minor(500_000, currencies::GBP),
                ),
            ],
        ),
        (
            "JE-SALE",
            "2026-07-05",
            vec![
                Line::debit(
                    "1000",
                    double_entry::Amount::minor(120_000, currencies::GBP),
                ),
                Line::credit(
                    "4000",
                    double_entry::Amount::minor(120_000, currencies::GBP),
                ),
            ],
        ),
        (
            "JE-FEE",
            "2026-07-09",
            vec![
                Line::debit("5000", double_entry::Amount::minor(1_500, currencies::GBP)),
                Line::credit("1000", double_entry::Amount::minor(1_500, currencies::GBP)),
            ],
        ),
    ];
    for (id, date, lines) in entries {
        ledger
            .post(&JournalEntry::draft(id, date, "2026-07", lines))
            .await
            .expect("posts");
    }

    // Read the books back and report over them. The class chart is supplied
    // — the ledger stores postings, and a class guessed from an id prefix
    // would be an assumption wearing a default.
    let class_of = |id: &str| match id {
        "3100" => double_entry::AccountType::Equity,
        "4000" => double_entry::AccountType::Revenue,
        "5000" => double_entry::AccountType::Expense,
        _ => double_entry::AccountType::Asset,
    };
    let rebuilt = into_ledger(&ledger, class_of).await.expect("rebuilds");
    let tb = TrialBalance::from_ledger(&rebuilt, "2026-07-31");
    assert!(tb.is_balanced(), "the stored books balance");
    assert!(tb.accounting_equation_holds("GBP"));

    // Cash: 500,000 + 120,000 − 1,500.
    let cash = tb
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "1000")
        .expect("bank row");
    assert_eq!(cash.net(), 618_500);

    // And the balance sheet carries the retained result, as always.
    let sheet = BalanceSheet::from_trial_balance(&tb, "GBP");
    assert!(sheet.is_balanced());
    assert_eq!(sheet.retained_result, 118_500);
    pool.close().await;
}

#[tokio::test]
async fn a_reversal_recorded_in_the_database_changes_the_reports() {
    let Some(instance) = start_postgres().await else {
        return;
    };
    let pool = ledger_pg_pool(&instance.dsn).await.expect("connects");
    ensure_schema(&pool).await.expect("schema");
    let ledger = PgLedger::connect(pool.clone(), "estate-reversal")
        .await
        .expect("tenant");

    ledger
        .post(&JournalEntry::draft(
            "JE-SALE",
            "2026-07-05",
            "2026-07",
            vec![
                Line::debit("1000", double_entry::Amount::minor(50_000, currencies::GBP)),
                Line::credit("4000", double_entry::Amount::minor(50_000, currencies::GBP)),
            ],
        ))
        .await
        .expect("posts");

    let before = into_ledger(&ledger, |_| double_entry::AccountType::Asset)
        .await
        .expect("rebuilds");
    let tb_before = TrialBalance::from_ledger(&before, "2026-07-31");
    let revenue_before = tb_before
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "4000")
        .expect("revenue row")
        .net();

    ledger
        .reverse("JE-SALE", "JE-SALE-R")
        .await
        .expect("reverses");

    let after = into_ledger(&ledger, |_| double_entry::AccountType::Asset)
        .await
        .expect("rebuilds");
    let tb_after = TrialBalance::from_ledger(&after, "2026-07-31");
    let revenue_after = tb_after
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "4000")
        .expect("revenue row")
        .net();

    // Two properties, both correct and both worth naming:
    //
    // 1. **The counter is dated today** — a reversal is dated when the books
    //    were corrected, not when the original was — so the 2026-07-31
    //    cut-off shows July's revenue as July reported it. Re-reporting
    //    closed history is what mutable ledgers do.
    // 2. **net() is debit-positive**, so a revenue credit reads negative.
    assert_eq!(revenue_before, -50_000, "July revenue as July reported it");
    assert_eq!(
        revenue_after, -50_000,
        "the correction sits after the cut-off"
    );

    // Re-report with everything in scope: the counter cancels the original,
    // once, to zero.
    let tb_full = TrialBalance::from_ledger(&after, "2026-12-31");
    let revenue_full = tb_full
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "4000")
        .expect("revenue row")
        .net();
    assert_eq!(revenue_full, 0);
    assert!(tb_full.is_balanced());
    pool.close().await;
}
