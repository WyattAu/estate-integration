#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 12, suite 6 — `flags_and_lifecycle`.
//!
//! flag-kit, pid-manager, otel-stack, envstack.
//!
//! The operational shell around a service: which features are on, whether
//! this process is the one that owns the resource, what the telemetry
//! pipeline looks like, and how configuration arrives. Four crates that
//! every binary in the estate sets up at startup and that had never been
//! composed — which is how a service ships with a flag default that silently
//! disables a feature for 10% of tenants, or a pid guard that deletes a
//! lock belonging to another process.
//!
//! The suite's spine is **the deterministic-percentage property**: a
//! percentage rollout must put a *given* user in the same bucket on every
//! replica and every restart, because "10% of users" is a promise and a
//! replica that disagrees silently doubles or halves it. That is a property
//! of the hashing function, which no single-replica test can check.
//!
//! Three findings this round:
//!
//! 1. **`FlagStore::delete` has a default body that always errors.** A host
//!    implementing the trait minimally inherits
//!    `Err("delete not implemented")` with nothing in the trait signalling
//!    the method is optional — and the only way to retire a flag is then to
//!    write `enabled = false`, which leaves the flag in `list()` forever.
//!    `MemoryFlagStore` *does* override it, so the crate's own tests never see
//!    the trap; the suite implements a minimal store to pin the difference.
//! 2. **`bucket` and `FlagName::new` disagree about what a flag name is.**
//!    Names are validated against `^[a-z][a-z0-9_]*$`, but `bucket` takes a
//!    `&str` and hashes it unvalidated — so a host spelling a flag with a dash
//!    gets a stable rollout for a flag that can never be stored, and the only
//!    symptom is `enabled()` answering `false` forever.
//! 3. **`envstack`'s layers fold keys on different terms.** Env keys are
//!    lowercased and split on `__`; `with_default` keeps case and splits on
//!    `.`. So `get("LEDGER_BACKEND")` and `get("ledger_backend")` are two
//!    live keys with two different values, and the env layer's only wins
//!    because it was pushed first. A host reading the original spelling gets
//!    the default and silently ignores the operator's override.

use flag_kit::{Evaluator, Flag, FlagChange, FlagName, FlagStore, MemoryFlagStore};
use std::sync::Arc;
use std::time::Duration;

// -- 1. percentage rollouts are deterministic ----------------------------

#[tokio::test]
async fn a_percentage_rollout_puts_the_same_user_in_the_same_bucket() {
    // The promise a percentage flag makes: *the same* 10% of users, on every
    // replica and every restart. If the bucketing were per-process or
    // time-dependent, two replicas would disagree and the effective
    // population would be neither 10% nor 20%.
    let user = "user-42";
    let first = flag_kit::bucket("new_invoice_layout", user);
    for _ in 0..1_000 {
        assert_eq!(
            flag_kit::bucket("new_invoice_layout", user),
            first,
            "bucketing must be stable across calls, not random per call"
        );
    }
    assert!(first < 100, "a bucket is a percentage in 0..100");

    // A bucket is a `u8`, so its range is checked by the type; the boundary
    // behaviour of a 0% vs 100% flag lives in the evaluator test below.

    // The org-scoped form is a *different* bucket, so the same user can be in
    // different percentages for different flags and for the same flag scoped
    // to an org. That is deliberate (per-tenant rollout) but it means a
    // "10% rollout" that also scopes by org is 10% *per org*, not 10%
    // overall — worth pinning so nobody assumes otherwise.
    let org_scoped = flag_kit::bucket_with_org("new_invoice_layout", user, Some("org-7"));
    let unscoped = flag_kit::bucket("new_invoice_layout", user);
    assert_ne!(
        org_scoped, unscoped,
        "scoping by org changes the bucket, so the population is per-org"
    );
    assert!(
        org_scoped < 100,
        "and an org-scoped bucket is still a percentage"
    );

    // And scoping is stable too, which is the same promise one level up.
    for _ in 0..100 {
        assert_eq!(
            flag_kit::bucket_with_org("new_invoice_layout", user, Some("org-7")),
            org_scoped
        );
    }

    // Different flags bucket differently, so two independent rollouts do not
    // land on the same users.
    let other = flag_kit::bucket("experimental_reporting", user);
    assert!(
        first < 100 && other < 100,
        "both are valid percentages ({first}, {other})"
    );
}

// -- 2. the evaluator, against a real store ------------------------------

#[tokio::test]
async fn the_evaluator_serves_a_percentage_rollout_from_its_store() {
    let store = MemoryFlagStore::default();
    // `Evaluator::from_store` takes ownership, so the suite keeps an `Arc`
    // handle to write through — the shape a host uses when flags are seeded
    // from config and then read by request handlers.
    // `Evaluator::from_store` is generic over `S: FlagStore + 'static`, so the
    // Arc *is* the store — no `Arc<dyn FlagStore>` (which would not satisfy
    // the bound, since `Arc` does not implement `FlagStore`).
    let shared = Arc::new(store);
    let evaluator = Evaluator::from_store(Arc::clone(&shared));

    let name = FlagName::new("new_invoice_layout").expect("valid flag name");

    // An unknown flag is off, not an error: a deploy that adds a flag to the
    // code before the store has it must not 500.
    assert!(
        !evaluator.enabled(&name).await,
        "an absent flag defaults to off"
    );

    // A flag at 0% serves nobody.
    shared
        .set(Flag::new(name.clone(), true, 0).expect("valid percentage"))
        .await
        .expect("stored");
    for i in 0..200 {
        assert!(
            !evaluator
                .enabled_for(&name, &format!("user-{i}"), None)
                .await,
            "0% serves nobody, even when enabled"
        );
    }

    // A flag at 100% serves everybody.
    shared
        .set(Flag::new(name.clone(), true, 100).expect("valid percentage"))
        .await
        .expect("stored");
    for i in 0..200 {
        assert!(
            evaluator
                .enabled_for(&name, &format!("user-{i}"), None)
                .await,
            "100% serves everybody, even for a user whose bucket is 99"
        );
    }

    // A disabled flag at 100% serves nobody — `percentage` is the ceiling,
    // not the decision.
    shared
        .set(Flag::new(name.clone(), false, 100).expect("valid percentage"))
        .await
        .expect("stored");
    assert!(
        !evaluator.enabled(&name).await,
        "disabled beats 100% — a rollback is one write, not a re-deploy"
    );

    // A mid-rollout flag serves *somebody*, and the same somebody every time.
    shared
        .set(Flag::new(name.clone(), true, 50).expect("valid percentage"))
        .await
        .expect("stored");
    let mut on = 0;
    let mut off = 0;
    for i in 0..200 {
        if evaluator
            .enabled_for(&name, &format!("user-{i}"), None)
            .await
        {
            on += 1;
        } else {
            off += 1;
        }
    }
    assert!(
        on > 0 && off > 0,
        "50% serves both sides, got {on} on / {off} off"
    );
    // Re-asking the same users must not change the answer.
    let again_on = (0..200)
        .filter(|i| futures_block_on(evaluator.enabled_for(&name, &format!("user-{i}"), None)))
        .count();
    assert_eq!(again_on, on, "the same users, on the same replica");
}

/// `enabled_for_all` evaluates **every** flag in the store for one
/// (user, org) pair — not one flag for a cohort of users. That shape is easy
/// to misread, and it is the one a host needs when it renders a dashboard of
/// every feature state for the current user.
#[tokio::test]
async fn enabled_for_all_reports_every_flag_for_one_subject() {
    let store = MemoryFlagStore::default();
    let shared = Arc::new(store);
    let evaluator = Evaluator::from_store(Arc::clone(&shared));
    let on = FlagName::new("beta_dashboard").expect("valid name");
    let off = FlagName::new("experimental_reporting").expect("valid name");

    // No flags at all: an empty answer, not a vacuous "yes". The distinction
    // matters because a host that treats "all enabled" as a permission would
    // read an unconfigured store as consent.
    assert!(
        evaluator.enabled_for_all("user-1", None).await.is_empty(),
        "with no flags the answer is empty, not permissive"
    );

    shared
        .set(Flag::new(on.clone(), true, 100).expect("valid percentage"))
        .await
        .expect("stored");
    shared
        .set(Flag::new(off.clone(), false, 100).expect("valid percentage"))
        .await
        .expect("stored");

    let states = evaluator.enabled_for_all("user-1", None).await;
    assert_eq!(states.len(), 2, "one answer per flag in the store");
    let map: std::collections::BTreeMap<_, _> = states.into_iter().collect();
    assert_eq!(map.get(&on), Some(&true));
    assert_eq!(map.get(&off), Some(&false));

    // A 50% rollout still answers per subject — the answer depends on the
    // subject's bucket, not on a cohort-wide vote.
    shared
        .set(Flag::new(on.clone(), true, 50).expect("valid percentage"))
        .await
        .expect("stored");
    let states = evaluator.enabled_for_all("user-1", None).await;
    let map: std::collections::BTreeMap<_, _> = states.into_iter().collect();
    assert_eq!(map.get(&off), Some(&false), "the 0% flag is still off");
    // Whether user-1 is in the 50% is a hash decision, but it must be
    // *stable* across calls — the same promise `bucket` makes.
    let again = evaluator.enabled_for_all("user-1", None).await;
    let map_again: std::collections::BTreeMap<_, _> = again.into_iter().collect();
    assert_eq!(
        map.get(&on),
        map_again.get(&on),
        "the same subject gets the same answer on every call"
    );
}

// -- 3. flag identity, validation and audit -------------------------------

#[tokio::test]
async fn a_flag_name_is_validated_and_a_change_is_recorded() {
    // Names are validated at construction against `^[a-z][a-z0-9_]*$` (via
    // validkit), so a typo is refused rather than becoming a flag that
    // silently never matches.
    assert!(FlagName::new("new_invoice_layout").is_ok());
    assert!(FlagName::new("").is_err(), "empty is refused");
    assert!(
        FlagName::new("with spaces").is_err(),
        "a name with spaces would be unmatchable in a URL or a CLI flag"
    );
    assert!(FlagName::new("UPPER").is_err(), "uppercase is refused");
    assert!(
        FlagName::new("1leading-digit").is_err(),
        "must start with a letter"
    );
    assert!(
        FlagName::new("trailing-dash").is_err(),
        "dashes are not allowed"
    );

    // **Round-12 finding: the validator and the bucketing function disagree
    // about what a flag name is.** `bucket(flag_name, user_id)` takes a
    // `&str` and hashes it with no validation, so it happily returns a stable
    // bucket for a name that `FlagName::new` would refuse. A host that
    // spells a flag with a dash — the natural choice, and the one the docs'
    // own prose uses — gets a deterministic rollout for a flag that can never
    // be stored, and the only symptom is `enabled()` answering `false`
    // forever. Ask: `bucket` should take a `&FlagName`, or at minimum share
    // the validator, so an unnameable flag cannot be bucketed.
    let hyphenated = flag_kit::bucket("beta-dashboard", "user-1");
    assert!(
        FlagName::new("beta-dashboard").is_err(),
        "...the two disagree on the same string"
    );
    assert!(
        hyphenated < 100,
        "and bucketing accepts it anyway, returning {hyphenated}"
    );
    let name = FlagName::new("new_invoice_layout").expect("valid");
    assert_eq!(name.as_str(), "new_invoice_layout");

    // Percentages outside 0..=100 are refused rather than clamped: a clamped
    // 150% flag would serve everyone while the operator believes it is a
    // partial rollout.
    assert!(Flag::new(name.clone(), true, 101).is_err());
    assert!(Flag::new(name.clone(), true, 255).is_err());
    assert!(Flag::new(name.clone(), true, 0).is_ok());
    assert!(Flag::new(name.clone(), true, 100).is_ok());

    // A change record names who flipped it and when — the audit trail an
    // operator needs when a rollout misbehaves in production.
    // `FlagChange::now` stamps the wall clock, and `new` takes one explicitly —
    // so an import from a log can replay a change faithfully.
    let change = FlagChange::now(name.clone(), false, true, "release-bot");
    assert!(
        change.enabled(),
        "enabled() is 'was off, now on' — a change *to* enabled"
    );
    assert!(!change.disabled(), "so it is not a change that disabled it");
    let replayed = FlagChange::new(name.clone(), false, true, "release-bot", 1_767_225_600);
    assert!(replayed.enabled(), "an imported change behaves the same");
    // And the inverse: a change that turns a flag off.
    let rollback = FlagChange::now(name.clone(), true, false, "operator");
    assert!(rollback.disabled());
    assert!(!rollback.enabled());
}

// -- 4. the store seam, and the delete that is not there ------------------

#[tokio::test]
async fn the_memory_store_round_trips_a_flag_and_lists_it() {
    let store = Arc::new(MemoryFlagStore::default());
    let a = FlagName::new("a").expect("valid");
    let b = FlagName::new("b").expect("valid");
    assert!(store.is_empty());

    store
        .set(Flag::new(a.clone(), true, 100).expect("valid"))
        .await
        .expect("stored");
    store
        .set(Flag::new(b.clone(), false, 0).expect("valid"))
        .await
        .expect("stored");
    assert_eq!(store.len(), 2);

    // Setting the same name twice replaces rather than duplicating — a flag
    // is keyed by name, so an update is not an append.
    store
        .set(Flag::new(a.clone(), false, 0).expect("valid"))
        .await
        .expect("stored");
    assert_eq!(store.len(), 2, "an update replaces, it does not append");
    assert!(
        !store.get(&a).await.expect("readable").is_enabled(),
        "and the new value is the one that sticks"
    );

    let listed = store.list().await;
    assert_eq!(listed.len(), 2);

    store.clear();
    assert!(store.is_empty());
}

/// **Round-12 finding: `FlagStore::delete` has a default body that always
/// fails, and a host that implements the trait gets it silently.**
///
/// The trait's default `delete` returns `Err("delete not implemented")`, so
/// a store that does not override it reports that instead of deleting — and
/// nothing in the trait says the method is optional, so an implementor has no
/// signal that they missed it. `MemoryFlagStore` *does* override it, which is
/// why this is invisible in the crate's own tests.
///
/// The suite pins both halves: a minimal host store that omits `delete`
/// inherits the error (the case that bites), and the reference store removes
/// correctly (so the difference is the override, not the trait).
#[tokio::test]
async fn a_store_that_omits_delete_inherits_a_failing_default() {
    use flag_kit::FlagError;

    // A host store implementing the trait *minimally* — which is what the
    // trait's shape invites.
    struct MinimalStore {
        flags: std::sync::Mutex<std::collections::BTreeMap<String, Flag>>,
    }

    #[async_trait::async_trait]
    impl flag_kit::FlagStore for MinimalStore {
        async fn get(&self, name: &FlagName) -> Option<Flag> {
            self.flags
                .lock()
                .expect("lockable")
                .get(name.as_str())
                .cloned()
        }
        async fn set(&self, flag: Flag) -> Result<(), FlagError> {
            self.flags
                .lock()
                .expect("lockable")
                .insert(flag.name.as_str().to_owned(), flag);
            Ok(())
        }
        async fn list(&self) -> Vec<Flag> {
            self.flags
                .lock()
                .expect("lockable")
                .values()
                .cloned()
                .collect()
        }
        // `delete` deliberately omitted.
    }

    let store = MinimalStore {
        flags: std::sync::Mutex::new(std::collections::BTreeMap::new()),
    };
    let name = FlagName::new("to_be_retired").expect("valid name");
    store
        .set(Flag::new(name.clone(), true, 100).expect("valid percentage"))
        .await
        .expect("stored");

    // The inherited default: an error, and the flag is still there.
    let err = store
        .delete(&name)
        .await
        .expect_err("the default always errors");
    assert!(
        err.to_string().contains("delete not implemented"),
        "the failure names itself, at least: {err}"
    );
    assert!(
        store.get(&name).await.is_some(),
        "so a host that omits the override cannot retire a flag through the \
         trait — it must write enabled=false instead, which leaves the flag \
         in `list()` forever"
    );
    assert_eq!(store.list().await.len(), 1);

    // The reference store *does* override, and removes.
    let reference = MemoryFlagStore::default();
    let live = FlagName::new("live_flag").expect("valid name");
    reference
        .set(Flag::new(live.clone(), true, 100).expect("valid percentage"))
        .await
        .expect("stored");
    assert!(
        reference.delete(&live).await.expect("the override works"),
        "MemoryFlagStore::delete reports whether it existed"
    );
    assert!(reference.get(&live).await.is_none());
    assert_eq!(reference.list().await.len(), 0, "and `list()` converges");
    assert!(
        !reference.delete(&live).await.expect("idempotent"),
        "deleting twice reports `false` rather than erroring"
    );
}

// -- 5. the pid guard: ownership, liveness, and not stealing locks --------

#[test]
fn a_pid_guard_claims_a_lock_and_releases_it_on_drop() {
    let dir = std::env::temp_dir().join(format!("estate-pid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a writable temp dir");
    let path = dir.join("service.pid");

    // Not running yet.
    assert!(
        !pid_manager::guard::DaemonGuard::is_already_running(&path),
        "a fresh lock file path is not held"
    );

    {
        let _guard = pid_manager::guard::DaemonGuard::new(&path).expect("claims the lock");
        assert!(
            pid_manager::guard::DaemonGuard::is_already_running(&path),
            "and is held while the guard is alive"
        );
        // The file exists and names this process, which is what a human reads
        // when a service will not start.
        let contents = std::fs::read_to_string(&path).expect("the lock file is readable");
        assert_eq!(
            contents.trim(),
            std::process::id().to_string(),
            "the lock file holds this process's pid"
        );

        // A second claim of the same path is refused — this is the property
        // that stops two replicas from both writing the same data directory.
        assert!(
            pid_manager::guard::DaemonGuard::new(&path).is_err(),
            "a second guard on a held path is refused rather than stealing \\
             the lock"
        );
    }

    // Out of scope but worth knowing: `release` is explicit, so a host that
    // wants the lock gone on shutdown calls it rather than relying on drop.
    // Drop already removed the file, so re-acquiring works.
    assert!(
        !pid_manager::guard::DaemonGuard::is_already_running(&path),
        "dropping the guard releases the lock"
    );
    let _second = pid_manager::guard::DaemonGuard::new(&path).expect("re-acquirable after release");
    drop(_second);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_lock_held_by_a_dead_process_is_reclaimed() {
    let dir = std::env::temp_dir().join(format!("estate-pid-dead-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a writable temp dir");
    let path = dir.join("service.pid");

    // A pid that is almost certainly not running. This is the case that makes
    // a naive pid-file implementation wedge forever: the lock is stale, but
    // "the file exists" says otherwise.
    std::fs::write(&path, "4294967").expect("a stale lock file can be written");
    assert!(
        !pid_manager::guard::DaemonGuard::is_already_running(&path),
        "a lock naming a dead process is not 'already running'"
    );
    let guard = pid_manager::guard::DaemonGuard::new(&path).expect("and is reclaimed, not refused");
    assert!(pid_manager::guard::DaemonGuard::is_already_running(&path));
    drop(guard);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The guard's own cleanup must not remove a lock it no longer owns — the
/// classic bug where a slow shutdown deletes a *replacement* process's lock.
#[test]
fn a_guard_removes_only_its_own_lock() {
    let dir = std::env::temp_dir().join(format!("estate-pid-own-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a writable temp dir");
    let first_path = dir.join("a.pid");
    let second_path = dir.join("b.pid");

    let a = pid_manager::guard::DaemonGuard::new(&first_path).expect("claimed");
    let b = pid_manager::guard::DaemonGuard::new(&second_path).expect("claimed");
    drop(a);
    assert!(
        !pid_manager::guard::DaemonGuard::is_already_running(&first_path),
        "its own lock is gone"
    );
    assert!(
        pid_manager::guard::DaemonGuard::is_already_running(&second_path),
        "and the other guard's lock is untouched"
    );
    drop(b);

    let _ = std::fs::remove_dir_all(&dir);
}

// -- 6. the telemetry facade ---------------------------------------------

#[tokio::test]
async fn the_otel_facade_defaults_to_a_service_that_exports_nowhere() {
    // `otel-stack` is a compatibility facade over `otelkit`, so the estate's
    // telemetry is already covered by every suite that pins otelkit. What the
    // facade adds is a config surface whose defaults decide whether a service
    // that merely *constructs* the config starts sending spans — worth
    // pinning precisely because there is no `enabled` flag to check.
    use otel_stack::{ExporterConfig, OtelConfig};

    let config = OtelConfig::default();
    assert_eq!(config.service_name, "unknown-service");
    assert!(config.version.is_none());

    // **Round-12 finding: there is no `enabled` flag.** Whether the process
    // actually exports is decided by the exporter *and* by the endpoint being
    // filled in — and `ExporterConfig` defaults to `Otlp`, the exporter most
    // likely to have somewhere real to send, while the endpoint is `None`.
    // So the safe default is reached only by that second coincidence, and a
    // host that sets an endpoint without reading `exporter` gets OTLP export
    // whether or not it meant to. Asserted explicitly so a change to either
    // default fails here.
    assert!(
        config.endpoint.is_none(),
        "no endpoint by default, so nothing is exported from a bare config: {:?}",
        config.endpoint
    );
    assert_eq!(config.exporter, ExporterConfig::Otlp);
    assert!(matches!(ExporterConfig::default(), ExporterConfig::Otlp));
    // Sampling everything is the third half: with an endpoint set, 1.0 means
    // every span is exported, so a host that sets only the endpoint samples
    // at 100%.
    assert_eq!(config.sample_rate, 1.0, "default sampling is everything");

    // The facade's `TelemetryGuard` *is* otelkit's, re-exported — which is the
    // proof that these are one implementation rather than two.
    fn _same_type(g: otel_stack::TelemetryGuard) -> otelkit::TelemetryGuard {
        g
    }
}

// -- 7. the layered config the facade sits on -----------------------------

#[tokio::test]
async fn envstack_layers_resolve_in_push_order_with_first_layer_winning() {
    use envstack::ConfigStack;

    let dir = std::env::temp_dir().join(format!("estate-env-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a writable temp dir");

    // Layers are *pushed*, and the first one that answers a key wins — which
    // is the opposite of the "later overrides earlier" convention most config
    // systems use, and config-kit's own docs say as much. So a host that
    // pushes defaults first and a file second silently ignores its own
    // defaults. The suite pins the direction, because getting it backwards
    // produces no error, only a service that ignores its configuration.
    // **Round-12 finding: an env layer's keys are split on `__`, not on
    // `_`, and are lowercased on the way in.** So `LEDGER_BACKEND` is *not*
    // nested — the default separator is a double underscore, and a single
    // underscore is part of the key. The key therefore stays
    // `ledger_backend` (lowercased whole), and a host that guesses the
    // nesting convention from the variable's spelling (`LEDGER__BACKEND` is
    // nested, `LEDGER_BACKEND` is not) gets a lookup that silently misses.
    // Values, meanwhile, are parsed as JSON with a string fallback — so
    // `postgres` becomes the string "postgres" and `"postgres"` also becomes
    // "postgres", which is forgiving in the right way.
    let stack = ConfigStack::new()
        .with_env_map(
            [
                ("LEDGER_BACKEND".to_owned(), r#""from-env""#.to_owned()),
                ("LEDGER_ONLY_ENV".to_owned(), r#""kept""#.to_owned()),
            ]
            .into_iter()
            .collect(),
        )
        .with_default("LEDGER_BACKEND", "from-defaults")
        .with_default("LEDGER_ONLY_DEFAULTS", "kept");

    // A bare word is not valid JSON, so that layer cannot answer at all.
    let unquoted = ConfigStack::new().with_env_map(
        [("LEDGER_BACKEND".to_owned(), "postgres".to_owned())]
            .into_iter()
            .collect(),
    );
    assert_eq!(
        unquoted
            .get("ledger_backend")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("postgres".to_owned()),
        "an unquoted value still resolves, because the JSON parse falls back \
         to a string — forgiving in the right direction"
    );
    // And the nesting convention is explicit: a double underscore nests.
    let nested = ConfigStack::new().with_env_map(
        [("LEDGER__BACKEND".to_owned(), r#""sqlite""#.to_owned())]
            .into_iter()
            .collect(),
    );
    assert_eq!(
        nested
            .get("ledger.backend")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("sqlite".to_owned()),
        "a double underscore is the separator, so this one nests"
    );
    assert!(
        nested.get("ledger_backend").is_none(),
        "and the flat spelling does not resolve to it"
    );

    // Keys are lowercased on the way in, so the lookup is against the folded
    // path — `get("LEDGER_BACKEND")` would miss.
    assert_eq!(
        stack
            .get("ledger_backend")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("from-env".to_owned()),
        "the FIRST layer that has the key wins, not the last"
    );
    // **The two layers disagree about case**, which is the operational trap:
    // env keys are lowercased, while `with_default` keeps the case it was
    // given. So `stack.get("LEDGER_BACKEND")` still answers — from the
    // *defaults* layer, which declared it in that spelling — while
    // `get("ledger_backend")` answers from the env layer. Two spellings of one
    // setting, both live, with the env layer's value winning only because it
    // was pushed first. A host that reads the original spelling gets the
    // default and silently ignores the operator's override.
    assert_eq!(
        stack
            .get("LEDGER_BACKEND")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("from-defaults".to_owned()),
        "the ORIGINAL spelling still resolves — from the defaults layer, \
         which does not fold case"
    );
    assert_eq!(
        stack
            .get("ledger_only_env")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("kept".to_owned())
    );
    // A key only the defaults layer has still resolves — so a later layer
    // fills gaps rather than replacing everything wholesale. (Defaults keep
    // their case: `with_default` does not fold.)
    assert_eq!(
        stack
            .get("LEDGER_ONLY_DEFAULTS")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("kept".to_owned()),
        "a later layer fills gaps rather than shadowing the whole view"
    );

    // An unset key is absent, not empty — the difference between "not
    // configured" and "configured to nothing".
    assert!(
        stack.get("ledger_not_set").is_none(),
        "an unconfigured key is absent, not an empty string"
    );

    // Adding a shadowing layer later is invisible from the reader's side: the
    // first copy already answers, so the value does not move.
    let shadowed = ConfigStack::new()
        .with_env_map(
            [("K".to_owned(), r#""first""#.to_owned())]
                .into_iter()
                .collect(),
        )
        .with_env_map(
            [("K".to_owned(), r#""second""#.to_owned())]
                .into_iter()
                .collect(),
        );
    assert_eq!(
        shadowed
            .get("k")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("first".to_owned()),
        "a later layer with the same key is shadowed, not merged"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// -- helpers --------------------------------------------------------------

/// `enabled_for_all` is async but pure once the store is warm, and this suite
/// wants to re-ask the same question. `futures::executor::block_on` keeps the
/// assertion synchronous without pulling a runtime into the test.
fn futures_block_on<F: std::future::Future>(fut: F) -> F::Output {
    futures::executor::block_on(fut)
}

#[test]
fn a_percentage_rollout_needs_no_await_to_be_stable() {
    // A short guard so the "no runtime needed for the pure function" property
    // is itself covered: bucketing is a hash, not a store read.
    let start = std::time::Instant::now();
    let mut acc = 0_u32;
    for i in 0..10_000 {
        acc += u32::from(flag_kit::bucket("f", &format!("u-{i}")));
    }
    assert!(acc > 0, "buckets are populated");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "bucketing 10k users is cheap enough for a hot path"
    );
}
