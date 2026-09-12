#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Scenario 1 — `auth_stack`: salting + tokenkit + validkit + barbican.
//!
//! Wires a full authentication flow the way a real service would:
//!
//! 1. load the signing secret from a vault-style provider,
//! 2. issue JWTs (HS256, RS256, ES256) and decode them with a validation
//!    config (leeway, audiences, required claims),
//! 3. lift raw claims into `validkit` validated newtypes at the boundary,
//! 4. revoke tokens through a shared revocation store (including the
//!    fail-closed store-failure path),
//! 5. rotate signing keys with `kid`-based selection,
//! 6. serve an axum HTTP API protected by `barbican` extractors and
//!    middleware, with `tokenkit` verifying behind them.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use tokenkit::claims::StandardClaims;
use tokenkit::error::JwtError;
use tokenkit::revocation::{InMemoryRevocationStore, TokenRevocationStore};
use tokenkit::service::{JwtAlgorithm, JwtConfig, JwtService, RotationKey};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Session claims a real service would mint. `email` and `tenant` are raw
/// strings on the wire; the validkit boundary turns them into validated
/// types after decoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionClaims {
    sub: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iss: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aud: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant: Option<String>,
}

impl SessionClaims {
    fn for_user(sub: &str, email: &str, tenant: &str) -> Self {
        Self {
            sub: Some(sub.to_string()),
            exp: Some(unix_now() + 3600),
            iss: Some(ISSUER.to_string()),
            aud: Some(AUDIENCE.to_string()),
            email: Some(email.to_string()),
            tenant: Some(tenant.to_string()),
        }
    }
}

const ISSUER: &str = "estate-integration";
const AUDIENCE: &str = "api";

fn unix_now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

/// Vault-style secret provider (the shape a barbican-like secret store
/// exposes: fetch material by key before constructing crypto config).
trait SecretProvider: Send + Sync {
    fn get(&self, key: &str) -> Result<String, String>;
}

/// In-memory stand-in for the vault.
struct MapSecretProvider {
    entries: std::collections::HashMap<&'static str, &'static str>,
}

impl SecretProvider for MapSecretProvider {
    fn get(&self, key: &str) -> Result<String, String> {
        self.entries
            .get(key)
            .map(|s| (*s).to_string())
            .ok_or_else(|| format!("secret `{key}` not found in provider"))
    }
}

/// HS256 service whose secret is loaded through the provider.
fn hs256_service_from_provider(provider: &dyn SecretProvider) -> Result<JwtService, String> {
    let secret = provider.get("auth/jwt-hs256")?;
    Ok(JwtService::new(JwtConfig {
        issuer: Some(ISSUER.to_string()),
        audience: Some(AUDIENCE.to_string()),
        ..JwtConfig::with_rotation_secrets(JwtAlgorithm::HS256, vec![secret])
    }))
}

/// Adapter so a test and a service can share one revocation store.
struct SharedStore(Arc<InMemoryRevocationStore>);

#[async_trait::async_trait]
impl TokenRevocationStore for SharedStore {
    async fn revoke(&self, jti: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.revoke(jti).await
    }

    async fn is_revoked(
        &self,
        jti: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        self.0.is_revoked(jti).await
    }
}

/// Store whose backend is down: revocation lookups error. tokenkit must
/// fail CLOSED on this (treat the error as revoked).
struct FailingStore;

#[async_trait::async_trait]
impl TokenRevocationStore for FailingStore {
    async fn revoke(&self, _jti: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("store unavailable".into())
    }

    async fn is_revoked(
        &self,
        _jti: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Err("store unavailable".into())
    }
}

const RSA_PRIVATE_PEM: &str = include_str!("keys/rsa_private.pem");
const RSA_PUBLIC_PEM: &str = include_str!("keys/rsa_public.pem");
const EC_PRIVATE_PEM: &str = include_str!("keys/ec_private.pem");
const EC_PUBLIC_PEM: &str = include_str!("keys/ec_public.pem");

fn bind_unused_port() -> (tokio::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    listener
        .set_nonblocking(true)
        .expect("set_nonblocking for tokio");
    let addr = listener.local_addr().expect("local_addr");
    (
        tokio::net::TcpListener::from_std(listener).expect("tokio listener"),
        format!("http://{addr}"),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// HS256 issue → decode, with claims lifted into validkit newtypes at the
/// trust boundary. A claim value that validkit rejects must never reach
/// the domain layer.
#[tokio::test]
async fn hs256_roundtrip_and_validated_claims() {
    let provider = MapSecretProvider {
        entries: std::collections::HashMap::from([("auth/jwt-hs256", "provider-loaded-secret")]),
    };
    let service = hs256_service_from_provider(&provider).expect("secret exists");

    let token = service
        .encode(&SessionClaims::for_user(
            "user-42",
            "wyatt@example.com",
            "acme-corp",
        ))
        .expect("encode");

    let claims: SessionClaims = service.decode(&token).expect("decode");

    // Boundary: raw claim → validated newtype.
    let email = validkit::EmailAddr::parse(claims.email.as_deref().expect("email claim"))
        .expect("claim email is a valid address");
    let tenant = validkit::TenantIdSlug::parse(claims.tenant.as_deref().expect("tenant claim"))
        .expect("claim tenant is a valid slug");
    assert_eq!(email.as_str(), "wyatt@example.com");
    assert_eq!(tenant.as_str(), "acme-corp");

    // A tampered claim value must be rejected by validkit, not trusted.
    let forged = SessionClaims::for_user("user-42", "not-an-email", "acme-corp");
    assert!(validkit::EmailAddr::parse(forged.email.as_deref().unwrap()).is_err());

    // The same token minted under a different secret must not verify.
    let other = hs256_service_from_provider(&MapSecretProvider {
        entries: std::collections::HashMap::from([("auth/jwt-hs256", "some-other-secret")]),
    })
    .expect("secret exists");
    assert!(other.decode::<SessionClaims>(&token).is_err());
}

/// RS256 and ES256 issue/verify roundtrips: sign with the private PEM,
/// verify with the public PEM. Also pins algorithm confusion defense: an
/// HMAC service must refuse an RSA-signed token.
#[tokio::test]
async fn asymmetric_algorithms_roundtrip() {
    let claims = SessionClaims::for_user("user-7", "ops@example.com", "globex");

    // RS256: sign with private, verify with public.
    let signer = JwtService::new(
        JwtConfig {
            secret: RSA_PRIVATE_PEM.to_string(),
            issuer: Some(ISSUER.to_string()),
            audience: Some(AUDIENCE.to_string()),
            ..JwtConfig::default()
        }
        .with_public_key(RSA_PUBLIC_PEM),
    );
    let token = signer.encode(&claims).expect("rs256 encode");
    let decoded: SessionClaims = signer.decode(&token).expect("rs256 decode");
    assert_eq!(decoded.sub.as_deref(), Some("user-7"));

    // An HS256 verifier must reject the RS256 token even with the same
    // issuer/audience (algorithm pinning).
    let hmac_only = JwtService::new(JwtConfig {
        secret: "unrelated-hmac-secret".to_string(),
        issuer: Some(ISSUER.to_string()),
        audience: Some(AUDIENCE.to_string()),
        ..JwtConfig::default()
    });
    assert!(hmac_only.decode::<SessionClaims>(&token).is_err());

    // ES256: sign with private, verify with public.
    let ec_signer = JwtService::new(
        JwtConfig {
            issuer: Some(ISSUER.to_string()),
            audience: Some(AUDIENCE.to_string()),
            ..JwtConfig::from_ec_pem(JwtAlgorithm::ES256, EC_PRIVATE_PEM)
        }
        .with_public_key(EC_PUBLIC_PEM),
    );
    let ec_token = ec_signer.encode(&claims).expect("es256 encode");
    let ec_decoded: SessionClaims = ec_signer.decode(&ec_token).expect("es256 decode");
    assert_eq!(ec_decoded.email.as_deref(), Some("ops@example.com"));

    // Verify-only service (public key, junk private material never used).
    let verifier = JwtService::new(
        JwtConfig {
            issuer: Some(ISSUER.to_string()),
            audience: Some(AUDIENCE.to_string()),
            ..JwtConfig::from_ec_pem(JwtAlgorithm::ES256, "not-a-key")
        }
        .with_public_key(EC_PUBLIC_PEM),
    );
    assert!(verifier.decode::<SessionClaims>(&ec_token).is_ok());
}

/// Validation config: leeway widens expiry tolerance, audiences constrain
/// acceptance, required claims reject structurally-valid-but-incomplete
/// tokens.
#[tokio::test]
async fn decode_validation_config_leeway_audiences_required_claims() {
    let base = |leeway: u64| {
        JwtService::new(JwtConfig {
            secret: "validation-secret".to_string(),
            issuer: Some(ISSUER.to_string()),
            audience: Some(AUDIENCE.to_string()),
            ..JwtConfig::default().with_leeway(leeway)
        })
    };

    // Token expired 30s ago.
    let expired = SessionClaims {
        exp: Some(unix_now().saturating_sub(30)),
        ..SessionClaims::for_user("user-1", "a@example.com", "acme-corp")
    };

    let lenient = base(3600);
    assert!(lenient
        .decode::<SessionClaims>(&lenient.encode(&expired).unwrap())
        .is_ok());

    let strict = base(0);
    let err = strict
        .decode::<SessionClaims>(&strict.encode(&expired).unwrap())
        .expect_err("expired token must be rejected with zero leeway");
    // Integration finding: tokenkit 0.4.0 flattens every jsonwebtoken error
    // into `JwtError::DecodingFailed(String)` — `JwtError::Expired` is never
    // constructed, so downstream code (e.g. barbican's expired→401 mapping)
    // cannot distinguish expiry from other failures. Pin the surfaced shape.
    match &err {
        JwtError::DecodingFailed(msg) => assert!(msg.contains("Expired"), "got: {msg}"),
        other => panic!("expected DecodingFailed, got {other:?}"),
    }

    // Audiences: `aud` must be one of the configured set.
    let multi_aud = JwtService::new(JwtConfig {
        secret: "aud-secret".to_string(),
        issuer: Some(ISSUER.to_string()),
        ..JwtConfig::default().with_audiences(vec!["api-a".to_string(), "api-b".to_string()])
    });
    let good_aud = SessionClaims {
        aud: Some("api-b".to_string()),
        ..SessionClaims::for_user("user-1", "a@example.com", "acme-corp")
    };
    assert!(multi_aud
        .decode::<SessionClaims>(&multi_aud.encode(&good_aud).unwrap())
        .is_ok());
    let bad_aud = SessionClaims {
        aud: Some("api-c".to_string()),
        ..SessionClaims::for_user("user-1", "a@example.com", "acme-corp")
    };
    assert!(multi_aud
        .decode::<SessionClaims>(&multi_aud.encode(&bad_aud).unwrap())
        .is_err());

    // Required claims: a token missing `sub` is rejected even though the
    // signature is valid. (jsonwebtoken only enforces *registered* claims
    // here, so a custom claim name like `role` would be silently ignored.)
    #[derive(Serialize, Deserialize)]
    struct MaybeSub {
        #[serde(skip_serializing_if = "Option::is_none")]
        sub: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exp: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        iss: Option<String>,
        role: Option<String>,
    }
    let strict_service = JwtService::new(JwtConfig {
        secret: "required-secret".to_string(),
        issuer: Some(ISSUER.to_string()),
        ..JwtConfig::default().with_required_claims(vec![
            "exp".to_string(),
            "iss".to_string(),
            "sub".to_string(),
        ])
    });
    let without_role = strict_service
        .encode(&MaybeSub {
            sub: None,
            exp: Some(unix_now() + 3600),
            iss: Some(ISSUER.into()),
            role: Some("editor".into()),
        })
        .unwrap();
    assert!(strict_service.decode::<MaybeSub>(&without_role).is_err());

    let with_role = strict_service
        .encode(&MaybeSub {
            sub: Some("user-1".into()),
            exp: Some(unix_now() + 3600),
            iss: Some(ISSUER.into()),
            role: Some("editor".into()),
        })
        .unwrap();
    let ok = strict_service.decode::<MaybeSub>(&with_role).unwrap();
    assert_eq!(ok.role.as_deref(), Some("editor"));
}

/// Revocation flow: a token is valid until its `jti` is revoked; after
/// revocation `decode_standard` rejects it — including when the revocation
/// store itself is failing (fail closed).
#[tokio::test]
async fn revocation_flow_fail_closed() {
    let store = Arc::new(InMemoryRevocationStore::new());
    let service = JwtService::new(JwtConfig {
        issuer: Some(ISSUER.to_string()),
        ..JwtConfig::with_rotation_secrets(JwtAlgorithm::HS256, vec!["revocation-secret".into()])
    })
    .with_revocation(Box::new(SharedStore(Arc::clone(&store))));

    let claims = StandardClaims {
        sub: Some("user-9".into()),
        iss: Some(ISSUER.into()),
        jti: Some("jti-abc".into()),
        ..Default::default()
    };
    let token = service.encode_standard(claims).expect("encode_standard");

    // Not revoked: decode_standard accepts.
    let decoded = service.decode_standard(&token).await.expect("pre-revoke");
    assert_eq!(decoded.jti.as_deref(), Some("jti-abc"));

    // Revoke through the shared store; the service sees it.
    store.revoke("jti-abc").await.expect("revoke");
    let err = service
        .decode_standard(&token)
        .await
        .expect_err("revoked token must be rejected");
    assert!(matches!(err, JwtError::Revoked));

    // Store failure must fail CLOSED: an unreachable store rejects.
    let fail_service = JwtService::new(JwtConfig {
        issuer: Some(ISSUER.to_string()),
        ..JwtConfig::with_rotation_secrets(JwtAlgorithm::HS256, vec!["revocation-secret".into()])
    })
    .with_revocation(Box::new(FailingStore));
    let err = fail_service
        .decode_standard(&token)
        .await
        .expect_err("failing revocation store must fail closed");
    assert!(matches!(err, JwtError::Revoked));
}

/// Rotation flow: issue under key v1, rotate to v2 — old tokens keep
/// verifying via their `kid`, new tokens are stamped with the new `kid`.
#[tokio::test]
async fn rotation_flow_kid_selection() {
    let old_config = JwtConfig {
        issuer: Some(ISSUER.to_string()),
        audience: Some(AUDIENCE.to_string()),
        ..JwtConfig::with_rotation_keys(
            JwtAlgorithm::HS256,
            vec![RotationKey::new(
                "v1-secret".to_string(),
                Some("v1".to_string()),
            )],
        )
    };
    let old_service = JwtService::new(old_config);
    let legacy_token = old_service
        .encode(&SessionClaims::for_user(
            "user-1",
            "old@example.com",
            "acme-corp",
        ))
        .expect("encode under v1");

    // Rotate: v2 is the active signing key, v1 remains for verification.
    let rotated = JwtService::new(JwtConfig {
        issuer: Some(ISSUER.to_string()),
        audience: Some(AUDIENCE.to_string()),
        ..JwtConfig::with_rotation_keys(
            JwtAlgorithm::HS256,
            vec![
                RotationKey::new("v2-secret".to_string(), Some("v2".to_string())),
                RotationKey::new("v1-secret".to_string(), Some("v1".to_string())),
            ],
        )
    });

    // Old token (kid=v1) still verifies during the rotation window.
    let legacy: SessionClaims = rotated
        .decode(&legacy_token)
        .expect("legacy token verifies");
    assert_eq!(legacy.sub.as_deref(), Some("user-1"));

    // New tokens verify under v2; a v2-only verifier accepts them while a
    // v1-only verifier does not — proving the `kid` routing.
    let fresh = rotated
        .encode(&SessionClaims::for_user(
            "user-2",
            "new@example.com",
            "acme-corp",
        ))
        .expect("encode under v2");
    assert!(rotated.decode::<SessionClaims>(&fresh).is_ok());

    let v2_only = JwtService::new(JwtConfig {
        issuer: Some(ISSUER.to_string()),
        audience: Some(AUDIENCE.to_string()),
        ..JwtConfig::with_rotation_keys(
            JwtAlgorithm::HS256,
            vec![RotationKey::new(
                "v2-secret".to_string(),
                Some("v2".to_string()),
            )],
        )
    });
    assert!(v2_only.decode::<SessionClaims>(&fresh).is_ok());
    assert!(v2_only.decode::<SessionClaims>(&legacy_token).is_err());
}

/// barbican protecting an axum service: the `BearerToken` extractor lifts
/// the raw credential, `auth_middleware_fn` rejects requests before they
/// reach the handler, and tokenkit verifies behind both.
///
/// NOTE (integration finding): barbican 0.2.1's `Claims`/`OptionalAuth`
/// extractors cannot be used here — barbican depends on `tokenkit 0.1`,
/// which is semver-incompatible with the current `tokenkit 0.4`, so the two
/// crates would carry two distinct `JwtService` types in one graph. This
/// test composes through barbican's tokenkit-free surface instead.
#[tokio::test]
async fn barbican_protected_http_stack() {
    let service = Arc::new(JwtService::new(JwtConfig {
        issuer: Some(ISSUER.to_string()),
        audience: Some(AUDIENCE.to_string()),
        ..JwtConfig::with_rotation_secrets(JwtAlgorithm::HS256, vec!["http-stack-secret".into()])
    }));

    // Routes guarded by barbican's BearerToken extractor; the handler
    // verifies the token against the service from state.
    let guarded = Router::new()
        .route(
            "/me",
            get(
                |State(service): State<Arc<JwtService>>,
                 barbican::BearerToken(token): barbican::BearerToken| async move {
                    let claims: SessionClaims = service.decode(&token).map_err(|e| match e {
                        JwtError::Expired => barbican::AuthRejection::TokenExpired,
                        _ => barbican::AuthRejection::InvalidToken("rejected".into()),
                    })?;
                    Ok::<_, barbican::AuthRejection>(format!(
                        "sub={} tenant={}",
                        claims.sub.as_deref().unwrap_or("?"),
                        claims.tenant.as_deref().unwrap_or("?")
                    ))
                },
            ),
        )
        .route(
            "/whoami",
            get(|State(service): State<Arc<JwtService>>, headers: axum::http::HeaderMap| async move {
                let maybe = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .and_then(|t| service.decode::<SessionClaims>(t).ok());
                match maybe {
                    Some(c) => format!("user={}", c.sub.as_deref().unwrap_or("?")),
                    None => "anonymous".to_string(),
                }
            }),
        )
        .with_state(Arc::clone(&service));

    // Route guarded by the middleware-based path (stateless; the validator
    // closure captures the service itself).
    let validate = {
        let service = Arc::clone(&service);
        move |token: String| {
            let service = Arc::clone(&service);
            async move {
                match service.decode::<SessionClaims>(&token) {
                    Ok(_) => Ok(()),
                    Err(JwtError::Expired) => Err(barbican::AuthRejection::TokenExpired),
                    Err(_) => Err(barbican::AuthRejection::InvalidToken("rejected".into())),
                }
            }
        }
    };
    let middleware_guarded = Router::new()
        .route("/admin", get(|| async { "admin-ok" }))
        .route_layer(axum::middleware::from_fn_with_state(
            (),
            barbican::auth_middleware_fn(validate),
        ));

    let app = Router::new().merge(guarded).merge(middleware_guarded);
    let (listener, base) = bind_unused_port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let http = reqwest::Client::new();
    let get = |path: &'static str, token: Option<String>| {
        let base = base.clone();
        let http = http.clone();
        async move {
            let mut req = http.get(format!("{base}{path}"));
            if let Some(t) = token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            let resp = req.send().await.expect("request");
            (resp.status(), resp.text().await.unwrap_or_default())
        }
    };

    // Valid token → handler sees validated claims.
    let token = service
        .encode(&SessionClaims::for_user(
            "user-1",
            "me@example.com",
            "acme-corp",
        ))
        .expect("encode");
    let (status, body) = get("/me", Some(token.clone())).await;
    assert_eq!(status, 200, "valid token should pass: {body}");
    assert_eq!(body, "sub=user-1 tenant=acme-corp");

    // Missing token → 401 (extractor rejection).
    let (status, _) = get("/me", None).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);

    // Garbage token → 401.
    let (status, _) = get("/me", Some("not.a.jwt".to_string())).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);

    // Expired token → 401 through the middleware path.
    let expired = SessionClaims {
        exp: Some(unix_now().saturating_sub(3600)),
        ..SessionClaims::for_user("user-1", "me@example.com", "acme-corp")
    };
    let expired_token = service.encode(&expired).expect("encode expired");
    let (status, _) = get("/admin", Some(expired_token)).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);

    // Middleware: valid token reaches the admin handler.
    let (status, body) = get("/admin", Some(token)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "admin-ok");

    // Middleware: missing token → 401 before the handler runs.
    let (status, _) = get("/admin", None).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);

    // OptionalAuth never rejects: anonymous gets a 200 with a sentinel.
    let (status, body) = get("/whoami", None).await;
    assert_eq!(status, 200);
    assert_eq!(body, "anonymous");
}

// ---------------------------------------------------------------------------
// salting: password hashing at the registration boundary
// ---------------------------------------------------------------------------

/// Minimal user store: validated email → Argon2id PHC hash. The pepper
/// lives outside the store (env/KMS in production; a test constant here),
/// mirroring the barbican-style secret-provider shape above.
struct UserStore {
    pepper: salting::Pepper,
    rows: std::collections::HashMap<String, String>,
}

impl UserStore {
    fn new(pepper_material: &[u8]) -> Result<Self, salting::PasswordError> {
        Ok(Self {
            pepper: salting::Pepper::new(pepper_material)?,
            rows: std::collections::HashMap::new(),
        })
    }

    /// Register: validate the email newtype first, then hash with pepper.
    fn register(&mut self, raw_email: &str, password: &str) -> Result<(), String> {
        let email =
            validkit::EmailAddr::parse(raw_email).map_err(|e| format!("invalid email: {e}"))?;
        let hash = salting::hash_password_with_pepper(password, &self.pepper)
            .map_err(|e| format!("hash failed: {e}"))?;
        self.rows.insert(email.as_str().to_string(), hash);
        Ok(())
    }

    /// Login: look up by validated email, verify the peppered hash.
    fn verify(&self, raw_email: &str, password: &str) -> Result<bool, String> {
        let email =
            validkit::EmailAddr::parse(raw_email).map_err(|e| format!("invalid email: {e}"))?;
        let hash = self
            .rows
            .get(email.as_str())
            .ok_or_else(|| "unknown user".to_string())?;
        salting::verify_password_with_pepper(password, hash, &self.pepper)
            .map_err(|e| format!("verify failed: {e}"))
    }
}

/// salting roundtrip: hash → verify ok, wrong password rejected, pepper
/// mismatch rejected, PHC format stored.
#[test]
fn salting_hash_verify_and_pepper_isolation() {
    let pepper = salting::Pepper::new(b"estate-integration-test-pepper").expect("pepper");
    let hash =
        salting::hash_password_with_pepper("correct horse battery staple", &pepper).expect("hash");
    assert!(hash.starts_with("$argon2id$"), "PHC format, got: {hash}");

    assert!(
        salting::verify_password_with_pepper("correct horse battery staple", &hash, &pepper)
            .expect("verify")
    );
    assert!(
        !salting::verify_password_with_pepper("wrong password", &hash, &pepper).expect("verify")
    );

    // A different pepper must not verify: DB-only leaks stay useless.
    let other = salting::Pepper::new(b"a-different-pepper").expect("pepper");
    assert!(
        !salting::verify_password_with_pepper("correct horse battery staple", &hash, &other)
            .expect("verify")
    );

    // Unpeppered defaults also roundtrip.
    let plain = salting::hash_password("plain-password").expect("hash");
    assert!(salting::verify_password("plain-password", &plain).expect("verify"));
    assert!(!salting::verify_password("nope", &plain).expect("verify"));
}

/// Full register → login → mint-JWT → authenticated-request flow tying
/// salting + validkit + tokenkit together, including every rejection path.
#[tokio::test]
async fn full_register_login_token_roundtrip() {
    let mut users = UserStore::new(b"jwt-stack-pepper").expect("store");

    // Invalid email rejected pre-hash: nothing is stored.
    assert!(users.register("not-an-email", "s3cret!").is_err());
    assert!(users.rows.is_empty());

    // Register, then login.
    users
        .register("wyatt@example.com", "s3cret-pass!")
        .expect("register");
    assert!(users
        .verify("wyatt@example.com", "s3cret-pass!")
        .expect("login"));
    // Wrong password rejected.
    assert!(!users
        .verify("wyatt@example.com", "wrong!")
        .expect("verify bool"));
    // Unknown user rejected.
    assert!(users.verify("ghost@example.com", "s3cret-pass!").is_err());

    // Login success mints a tokenkit JWT (HS256, secret from provider).
    let provider = MapSecretProvider {
        entries: std::collections::HashMap::from([("auth/jwt-hs256", "login-mint-secret")]),
    };
    let service = hs256_service_from_provider(&provider).expect("secret exists");
    let token = service
        .encode(&SessionClaims::for_user(
            "user-42",
            "wyatt@example.com",
            "acme-corp",
        ))
        .expect("mint after verify");

    // Authenticated request: decode + email re-validation at the boundary.
    let claims: SessionClaims = service.decode(&token).expect("decode");
    let email = validkit::EmailAddr::parse(claims.email.as_deref().expect("email"))
        .expect("valid claim email");
    assert_eq!(email.as_str(), "wyatt@example.com");

    // Expired token rejected (expired well beyond any default leeway).
    let strict = JwtService::new(JwtConfig {
        secret: "login-mint-secret".to_string(),
        issuer: Some(ISSUER.to_string()),
        audience: Some(AUDIENCE.to_string()),
        ..JwtConfig::default()
    });
    let expired_claims = SessionClaims {
        exp: Some(unix_now().saturating_sub(86_400)),
        ..SessionClaims::for_user("user-42", "wyatt@example.com", "acme-corp")
    };
    let expired_token = strict.encode(&expired_claims).expect("encode expired");
    assert!(strict.decode::<SessionClaims>(&expired_token).is_err());

    // Revoked token rejected via shared revocation store.
    let store = Arc::new(InMemoryRevocationStore::new());
    let revoking = JwtService::new(JwtConfig {
        issuer: Some(ISSUER.to_string()),
        ..JwtConfig::with_rotation_secrets(JwtAlgorithm::HS256, vec!["revoked-secret".into()])
    })
    .with_revocation(Box::new(SharedStore(Arc::clone(&store))));
    let std_claims = StandardClaims {
        sub: Some("user-42".into()),
        iss: Some(ISSUER.into()),
        jti: Some("login-jti-1".into()),
        ..Default::default()
    };
    let login_token = revoking
        .encode_standard(std_claims)
        .expect("encode_standard");
    assert!(revoking.decode_standard(&login_token).await.is_ok());
    store.revoke("login-jti-1").await.expect("revoke");
    assert!(matches!(
        revoking.decode_standard(&login_token).await,
        Err(JwtError::Revoked)
    ));
}
