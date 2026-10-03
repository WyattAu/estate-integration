#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 8 — `config_tenant`: config-kit 0.1.0.
//!
//! Per-tenant configuration resolution, the way a multi-tenant service
//! ships it: one base config holds the fleet defaults, and each tenant
//! supplies an override file. Resolution is a two-layer `ConfigBuilder`
//! (base → tenant) whose documented **deep merge** applies the
//! tenant's keys key-by-key — a tenant overrides one nested knob
//! without clobbering its siblings, and an untouched key keeps the
//! base value. The same tenant key resolves differently per tenant,
//! which is the entire point.
//!
//! The secrets half rides along: tenant-specific API keys are loaded
//! into `Sensitive<String>` fields, and the redaction contract holds
//! through the merged load — `Debug`/`Display` render `[REDACTED]`,
//! never the value, and the only door is the grep-friendly `.expose()`.

use config_kit::{ConfigBuilder, ConfigLayer, Sensitive};
use serde::Deserialize;
use std::path::PathBuf;

/// The tenant-facing config shape: fleet defaults under the base file,
/// per-tenant values layered on top.
#[derive(Debug, Deserialize)]
struct TenantConfig {
    service: Service,
}

#[derive(Debug, Deserialize)]
struct Service {
    region: String,
    workers: u16,
    /// Nested table — proves the merge recurses below the top level.
    limits: Limits,
    secrets: Secrets,
}

#[derive(Debug, Deserialize)]
struct Limits {
    requests_per_minute: u32,
    burst: u32,
}

#[derive(Debug, Deserialize)]
struct Secrets {
    api_key: Sensitive<String>,
    /// Never overridden anywhere: the base value must survive every
    /// tenant merge untouched (and stay redacted).
    webhook_signing_key: Sensitive<String>,
}

/// Writes a file into the suite tempdir and returns its path.
fn write_toml(dir: &tempfile::TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).expect("write config layer");
    path
}

/// The fleet defaults every tenant starts from.
fn base_config(dir: &tempfile::TempDir) -> PathBuf {
    write_toml(
        dir,
        "base.toml",
        r#"
[service]
region = "eu-west"
workers = 4

[service.limits]
requests_per_minute = 600
burst = 20

[service.secrets]
api_key = "base-api-0000"
webhook_signing_key = "base-whsec-0000"
"#,
    )
}

/// Resolves one tenant: base → tenant-override, in that order.
fn resolve_tenant(base: &PathBuf, tenant: &PathBuf) -> TenantConfig {
    ConfigBuilder::new()
        .layer(ConfigLayer::file(base))
        .layer(ConfigLayer::file(tenant))
        .load()
        .expect("the two-layer merge loads")
}

/// Two tenants over one base: each sees the base defaults merged with
/// exactly its own overrides — deep-merged — and secrets stay redacted
/// through every render path.
#[test]
fn tenants_resolve_the_base_with_their_own_overrides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = base_config(&dir);

    // -- Tenant A: region, worker count, one nested knob, own key.
    let tenant_a = write_toml(
        &dir,
        "tenant-a.toml",
        r#"
[service]
region = "us-east"
workers = 8

[service.limits]
requests_per_minute = 1200

[service.secrets]
api_key = "tenant-a-api-7777"
"#,
    );
    let a = resolve_tenant(&base, &tenant_a);
    assert_eq!(a.service.region, "us-east", "A's override wins");
    assert_eq!(a.service.workers, 8, "A's override wins");
    assert_eq!(
        a.service.limits.requests_per_minute, 1200,
        "A's nested override wins"
    );
    assert_eq!(
        a.service.limits.burst, 20,
        "the untouched nested sibling keeps the base value (deep merge)"
    );
    assert_eq!(a.service.secrets.api_key.expose(), "tenant-a-api-7777");
    assert_eq!(
        a.service.secrets.webhook_signing_key.expose(),
        "base-whsec-0000",
        "a key no tenant overrides survives every merge"
    );

    // -- Tenant B: one nested knob and its own key; everything else is
    //       the fleet default — including what A overrode.
    let tenant_b = write_toml(
        &dir,
        "tenant-b.toml",
        r#"
[service.limits]
burst = 99

[service.secrets]
api_key = "tenant-b-api-8888"
"#,
    );
    let b = resolve_tenant(&base, &tenant_b);
    assert_eq!(b.service.region, "eu-west", "B never overrode the region");
    assert_eq!(b.service.workers, 4, "B never overrode the workers");
    assert_eq!(
        b.service.limits.requests_per_minute, 600,
        "B never touched A's knob"
    );
    assert_eq!(b.service.limits.burst, 99, "B's own knob applies");
    assert_eq!(b.service.secrets.api_key.expose(), "tenant-b-api-8888");

    // -- The same key resolves differently per tenant — the point of
    //       per-tenant resolution.
    assert_ne!(a.service.region, b.service.region);
    assert_ne!(
        a.service.limits.requests_per_minute,
        b.service.limits.requests_per_minute
    );
    assert_ne!(
        a.service.secrets.api_key.expose(),
        b.service.secrets.api_key.expose()
    );

    // -- Redaction holds through the merged load: no render path leaks
    //       either tenant's secret.
    for (tenant, config) in [("A", &a), ("B", &b)] {
        let rendered = format!("{config:?}");
        assert!(
            rendered.contains("[REDACTED]"),
            "tenant {tenant}: Debug must redact: {rendered}"
        );
        for secret in [
            "tenant-a-api-7777",
            "tenant-b-api-8888",
            "base-api-0000",
            "base-whsec-0000",
        ] {
            assert!(
                !rendered.contains(secret),
                "tenant {tenant}: the merged Debug must not leak {secret}: {rendered}"
            );
        }
        assert_eq!(
            format!("{}", config.service.secrets.api_key),
            "[REDACTED]",
            "Display redacts too"
        );
    }
}

/// A typo in a tenant file is a typed rejection under `load_strict` —
/// the strict gate names the offending top-level key, while the lenient
/// `load` the fleet path uses ignores it.
#[test]
fn strict_loading_names_the_tenant_typo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = base_config(&dir);
    let tenant = write_toml(
        &dir,
        "tenant-typo.toml",
        "tennat = \"a\"\n\n[service.limits]\nburst = 5\n",
    );

    // Lenient: the typo is ignored, the overrides still apply.
    let lenient = ConfigBuilder::new()
        .layer(ConfigLayer::file(&base))
        .layer(ConfigLayer::file(&tenant))
        .load::<TenantConfig>()
        .expect("lenient load ignores the unknown key");
    assert_eq!(lenient.service.limits.burst, 5);

    // Strict: a typed UnknownFields naming the typo'd key.
    let error = ConfigBuilder::new()
        .layer(ConfigLayer::file(&base))
        .layer(ConfigLayer::file(&tenant))
        .load_strict::<TenantConfig>()
        .expect_err("the typo'd key must be rejected");
    match &error {
        config_kit::ConfigError::UnknownFields { keys } => {
            assert_eq!(keys, &["tennat".to_owned()], "{keys:?}");
        }
        other => panic!("expected UnknownFields, got {other:?}"),
    }
    assert!(
        error.to_string().contains("tennat"),
        "the error must name the key for CI logs: {error}"
    );

    // The clean tenant file loads strictly.
    let clean = write_toml(&dir, "tenant-clean.toml", "[service]\nworkers = 2\n");
    let strict = ConfigBuilder::new()
        .layer(ConfigLayer::file(&base))
        .layer(ConfigLayer::file(&clean))
        .load_strict::<TenantConfig>()
        .expect("a clean tenant passes the strict gate");
    assert_eq!(strict.service.workers, 2);
    assert_eq!(strict.service.region, "eu-west");
}
