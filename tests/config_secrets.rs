#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 3, suite 1 — `config_secrets`: config-kit 0.1.0.
//!
//! The one-crate flow here is the boundary every estate service shares:
//! a typed service config with a secret field, loaded from layered
//! sources (tempdir TOML file → env vars → in-process overrides), with
//! precedence the docs promise (**overrides > env > file**), secrets that
//! cannot leak through `Debug`/`Display`, and strict loading that names
//! the typo'd key instead of silently ignoring it.
//!
//! Estate feedback pinned in comments where the API surprised us.

use config_kit::{ConfigBuilder, ConfigLayer, Sensitive};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The service config shape every suite-1 test loads: one scalar from the
/// file, one secret, one deployment-tunable (env territory).
#[derive(Debug, Deserialize)]
struct ServiceConfig {
    port: u16,
    debug: bool,
    api_key: Sensitive<String>,
}

/// Writes a base TOML file into a tempdir and returns its path.
fn base_file(dir: &tempfile::TempDir, extra: &str) -> std::path::PathBuf {
    let path = dir.path().join("service.toml");
    std::fs::write(
        &path,
        format!("port = 8080\ndebug = false\napi_key = \"file-secret-1234\"\n{extra}"),
    )
    .expect("write base toml");
    path
}

/// File alone loads, the secret only surfaces through `expose`, and no
/// formatting path leaks it — the load-side half of the redaction
/// contract (`Sensitive<T>` renders `[REDACTED]` for both traits with no
/// `T: Debug` bound, so the derived struct Debug cannot leak either).
#[test]
fn file_layer_loads_and_secret_stays_redacted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = base_file(&dir, "");

    let config: ServiceConfig = ConfigBuilder::new()
        .layer(ConfigLayer::file(&path))
        .load()
        .expect("file layer loads");

    assert_eq!(config.port, 8080);
    assert!(!config.debug);
    // The only door to the secret is the grep-friendly `.expose()`.
    assert_eq!(config.api_key.expose(), "file-secret-1234");

    // Debug and Display of the loaded struct render the redaction, never
    // the value — log-an-accident insurance, asserted on the real load.
    let rendered = format!("{config:?}");
    assert!(rendered.contains("[REDACTED]"), "{rendered}");
    assert!(!rendered.contains("file-secret-1234"), "{rendered}");
    assert_eq!(format!("{}", config.api_key), "[REDACTED]");

    // Display of the whole struct is the same story: nothing to leak.
    let shown = format!("{}", Sensitive::new("surface".to_owned()));
    assert_eq!(shown, "[REDACTED]");
}

/// Precedence ladder from the docs: file → env → overrides, later layers
/// winning on collision while untouched keys keep their earlier-layer
/// values. Env values are *typed* on the way in (`"9090"` → integer,
/// `"true"` → bool), so the target fields type-check without string
/// intermediates.
#[test]
fn env_layer_overrides_file_values() {
    // Unique prefix per test: env is process-global and suites run in
    // parallel threads inside one process.
    const PREFIX: &str = "ESTATE_INTEG_ENV_LAYER_";
    std::env::set_var(format!("{PREFIX}PORT"), "9090");
    std::env::set_var(format!("{PREFIX}DEBUG"), "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = base_file(&dir, "");

    let config: ServiceConfig = ConfigBuilder::new()
        .layer(ConfigLayer::file(&path))
        .layer(ConfigLayer::env_prefix(PREFIX))
        .load()
        .expect("file + env layers load");

    // Env beat the file on both colliding keys…
    assert_eq!(config.port, 9090);
    assert!(config.debug);
    // …and the file kept the keys env never mentioned.
    assert_eq!(config.api_key.expose(), "file-secret-1234");

    std::env::remove_var(format!("{PREFIX}PORT"));
    std::env::remove_var(format!("{PREFIX}DEBUG"));
}

/// The full ladder: in-process overrides (a `BTreeMap<String,
/// toml::Value>`) beat env, env beats file — and the deep merge means an
/// override layer can override one key without clobbering siblings.
#[test]
fn in_process_overrides_beat_env_and_file() {
    const PREFIX: &str = "ESTATE_INTEG_OVERRIDE_LAYER_";
    std::env::set_var(format!("{PREFIX}PORT"), "9090");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = base_file(&dir, "");

    let mut overrides: BTreeMap<String, toml::Value> = BTreeMap::new();
    overrides.insert("port".into(), toml::Value::Integer(9443));

    let config: ServiceConfig = ConfigBuilder::new()
        .layer(ConfigLayer::file(&path))
        .layer(ConfigLayer::env_prefix(PREFIX))
        .layer(ConfigLayer::overrides(overrides))
        .load()
        .expect("all three layers load");

    // overrides > env > file, exactly as documented.
    assert_eq!(config.port, 9443);
    // Nothing overrode `debug`, so the file value survives.
    assert!(!config.debug);

    std::env::remove_var(format!("{PREFIX}PORT"));
}

/// `load_strict` denies unknown *top-level* keys and names the offending
/// key in the typed error — the typo-catch `load` deliberately does not
/// do. The same file loads fine non-strictly, so this is an opt-in gate.
#[test]
fn load_strict_rejects_unknown_top_level_key_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = base_file(&dir, "typo_key = 1\n");

    // Lenient `load`: the unknown key is ignored (documented default).
    let lenient: ServiceConfig = ConfigBuilder::new()
        .layer(ConfigLayer::file(&path))
        .load()
        .expect("lenient load ignores unknown keys");
    assert_eq!(lenient.port, 8080);

    // Strict `load`: a typed `UnknownFields` naming the key.
    let error = ConfigBuilder::new()
        .layer(ConfigLayer::file(&path))
        .load_strict::<ServiceConfig>()
        .expect_err("typo_key must be rejected");
    match &error {
        config_kit::ConfigError::UnknownFields { keys } => {
            assert_eq!(keys, &["typo_key".to_owned()], "{keys:?}");
        }
        other => panic!("expected UnknownFields, got {other:?}"),
    }
    // The Display form carries the offending key name for CI logs.
    assert!(
        error.to_string().contains("typo_key"),
        "error must name the key: {error}"
    );
}
