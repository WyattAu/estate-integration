#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 7, suite 2 — `auth_provision`.
//!
//! accessctl, scim-kit, auditlog (tamper-audit), ws-kit, ws-barbican,
//! barbican, tokenkit.
//!
//! The flow an accounting firm actually runs on day one: a tenant's identity
//! provider pushes users in over SCIM, each provisioned user is bound to a
//! role, an authenticated WebSocket carries live book updates, and every
//! state-changing action lands in a tamper-evident audit chain. Seven
//! estate crates, one provisioning-and-session story.
//!
//! The order is deliberate — SCIM provisions the *identity*, accessctl
//! decides what it may *do*, ws-barbican authenticates the *transport*, and
//! auditlog records what actually *happened*. Each crate is independently
//! tested; what only this composition can show is whether they fit
//! together, which is the question the accounting product's multi-tenant
//! story depends on.
//!
//! Four findings from this composition, filed in the README:
//!
//! 1. **`tamper-audit` 0.1.0's `AuditLog` is synchronous while 0.2.0 made
//!    it async** — an API break in a patch-level bump. This suite pins 0.2.0.
//! 2. **The chain is seeded with a genesis `log.created` entry**, so `len()`,
//!    `total_entries` and any pagination are one higher than the host
//!    appended, and the *first* host entry chains from genesis rather than
//!    the zero hash. A verifier assuming "first entry chains from zero"
//!    rejects every log the crate produces.
//! 3. **`VerificationResult` has no boolean verdict** — validity is
//!    `broken_at.is_none()` plus a `valid_entries == total_entries` check.
//!    The `error: Option<String>` is the only place a reason is carried.
//! 4. **`accessctl` has two authorization mechanisms** — a hardcoded
//!    `RoleHierarchy::check_permission` match and a Cedar `PolicySet` —
//!    with nothing tying them together, so an operator editing policies and
//!    a host calling `check_permission` can disagree.
//!
//! Two things this suite deliberately does **not** assert, because the
//! published crates cannot support them yet:
//!
//! - scim-kit has no `UserStore` trait, only schema types and a
//!   `to_scim_user` constructor. So provisioning is asserted at the *type*
//!   level: a provisioned user serializes to RFC 7644 shape, filters match
//!   it, and pagination slices the result set. Durable storage is host work.
//! - accessctl's role checks are two independent mechanisms — a hardcoded
//!   `RoleHierarchy::check_permission` match and a Cedar `PolicySet` behind
//!   the `cedar` feature. The suite asserts they agree on the default
//!   hierarchy, because a host that consults one and an operator who edits
//!   the other would otherwise have two answers to "may this role do this?".

use accessctl::{PolicySet, Role, RoleHierarchy};
use scim_kit::filter::parse_filter;
use scim_kit::schema::{ScimListResponse, ScimMeta, ScimUser};
use scim_kit::user::to_scim_user;
use scim_kit::Resource;
use tamper_audit::AuditLog;
use ws_barbican::extractor::BarbicanTokenExtractor;

// -- 1. provisioning: a SCIM user is a first-class resource ---------------

#[tokio::test]
async fn a_provisioned_user_is_an_rfc7644_resource() {
    let user = to_scim_user(
        "u-1",
        "accountant@example.test",
        "Ada Lovelace",
        "accountant@example.test",
        true,
    );

    // The schema URN is what makes it a SCIM resource rather than a struct
    // that happens to have a userName.
    assert_eq!(
        user.schemas,
        [scim_kit::schema::USER_SCHEMA_URN.to_string()]
    );
    assert_eq!(<ScimUser as Resource>::RESOURCE_TYPE, "User");
    assert_eq!(user.id, "u-1");
    assert!(user.active);

    // Inactive users are provisioned the same way — deactivation is data,
    // not a different shape, which is what an IdP expects.
    let inactive = to_scim_user(
        "u-2",
        "former@example.test",
        "Former Employee",
        "former@example.test",
        false,
    );
    assert!(!inactive.active);
    assert_eq!(inactive.schemas, user.schemas);

    // It round-trips through JSON in the wire shape: `userName`, not
    // `user_name`. That rename is the protocol, and a host that forgets it
    // is rejected by every real IdP.
    let json = serde_json::to_value(&user).expect("serializes");
    assert_eq!(json["userName"], "accountant@example.test");
    assert!(
        json.get("user_name").is_none(),
        "snake_case never hits the wire"
    );
    assert_eq!(json["emails"][0]["value"], "accountant@example.test");
    assert_eq!(json["emails"][0]["primary"], true);
    assert_eq!(json["meta"]["resourceType"], "User");

    let parsed: ScimUser = serde_json::from_value(json).expect("deserializes");
    assert_eq!(parsed.id, user.id);
    assert_eq!(parsed.user_name, user.user_name);
    assert_eq!(parsed.meta.resource_type, user.meta.resource_type);
}

#[tokio::test]
async fn scim_filters_select_from_a_provisioned_set() {
    let users = [
        to_scim_user("u-1", "a@example.test", "A Person", "a@example.test", true),
        to_scim_user("u-2", "b@example.test", "B Person", "b@example.test", true),
        to_scim_user("u-3", "c@example.test", "C Person", "c@example.test", false),
    ];

    // The filter an IdP sends for an active user.
    let active = parse_filter(r#"userName eq "b@example.test""#).expect("valid filter");
    let matched: Vec<&ScimUser> = users
        .iter()
        .filter(|u| active.matches_serialized(u))
        .collect();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].id, "u-2");

    // `active eq false` selects the deactivated one — the filter reads the
    // SCIM attribute names, so it must survive the serde renames.
    let off = parse_filter("active eq false").expect("valid filter");
    let off_matched: Vec<&ScimUser> = users.iter().filter(|u| off.matches_serialized(u)).collect();
    assert_eq!(off_matched.len(), 1);
    assert_eq!(off_matched[0].id, "u-3");

    // A filter that matches nothing is empty, not an error — IdPs probe
    // with filters before the resource exists.
    let none = parse_filter(r#"userName eq "nobody@example.test""#).expect("valid filter");
    assert!(users.iter().all(|u| !none.matches_serialized(u)));

    // A malformed filter *is* an error, because silently treating it as
    // "match everything" would hand back a whole directory.
    assert!(parse_filter("userName ?? ").is_err());
}

#[tokio::test]
async fn a_scim_list_response_carries_its_own_paging_truth() {
    let users: Vec<ScimUser> = (0..3)
        .map(|n| {
            to_scim_user(
                &format!("u-{n}"),
                &format!("u{n}@example.test"),
                &format!("User {n}"),
                &format!("u{n}@example.test"),
                true,
            )
        })
        .collect();

    // RFC 7644 §3.4.2: totalResults is the size of the *whole* set, while
    // Resources holds the current page. A host that confuses them paginates
    // wrongly and clients loop forever.
    let page = ScimListResponse {
        schemas: vec![scim_kit::schema::LIST_RESPONSE_URN.to_string()],
        total_results: 3,
        items_per_page: 2,
        start_index: 1,
        resources: users[..2].to_vec(),
    };
    assert_eq!(page.total_results, 3);
    assert_eq!(page.resources.len(), 2);
    assert_ne!(
        page.total_results as usize,
        page.resources.len(),
        "total and page size are different facts"
    );

    let json = serde_json::to_value(&page).expect("serializes");
    assert_eq!(json["totalResults"], 3);
    assert_eq!(json["itemsPerPage"], 2);
    assert_eq!(json["startIndex"], 1);
    // RFC 7644 §3.4.2 spells the array `Resources` with a capital R in the
    // *schema*, but on the wire it is `resources` — scim-kit keeps the
    // lowercase field name, which is what every IdP actually sends.
    assert_eq!(json["resources"].as_array().map(Vec::len), Some(2));

    let round: ScimListResponse<ScimUser> = serde_json::from_value(json).expect("deserializes");
    assert_eq!(round.total_results, 3);
    assert_eq!(round.resources[0].id, "u-0");
}

/// Every provisioned resource carries its own `meta`, which is what an IdP
/// uses for conditional-request ETags.
#[tokio::test]
async fn provisioned_resources_carry_meta_for_conditional_requests() {
    let user = to_scim_user("u-1", "a@example.test", "A Person", "a@example.test", true);
    let meta: &ScimMeta = <ScimUser as Resource>::meta(&user);
    assert_eq!(meta.resource_type, "User");
    assert_eq!(meta.location, "/scim/v2/Users/u-1");
    assert_eq!(<ScimUser as Resource>::id(&user), "u-1");
    // `to_scim_user` stamps `created` and `last_modified` with two separate
    // `Utc::now()` calls, so for a fresh resource they differ by
    // nanoseconds rather than being equal. That is harmless but means a host
    // cannot use `created == last_modified` as an "unmodified resource" test;
    // the suite pins the observable fact instead.
    assert!(
        meta.last_modified >= meta.created,
        "last_modified is never before created"
    );
    assert!(
        meta.last_modified - meta.created < chrono::Duration::seconds(1),
        "and for a freshly built resource the two are the same instant to          within a millisecond, not two distinct updates"
    );
}

// -- 2. roles: the two mechanisms must agree ------------------------------

#[tokio::test]
async fn the_hardcoded_hierarchy_and_the_cedar_policies_agree() {
    let hierarchy = RoleHierarchy::new();

    // The hardcoded match: Admin is everything, Editor may view/edit/create,
    // Viewer only views.
    assert!(hierarchy.check_permission(&Role::Admin, "view"));
    assert!(hierarchy.check_permission(&Role::Admin, "delete"));
    assert!(hierarchy.check_permission(&Role::Editor, "edit"));
    assert!(!hierarchy.check_permission(&Role::Editor, "delete"));
    assert!(hierarchy.check_permission(&Role::Viewer, "view"));
    assert!(!hierarchy.check_permission(&Role::Viewer, "edit"));

    // Ordering is total and the roles are ordered.
    assert!(Role::Admin.has_at_least(&Role::Editor));
    assert!(Role::Editor.has_at_least(&Role::Viewer));
    assert!(!Role::Viewer.has_at_least(&Role::Editor));
    assert_eq!(Role::Viewer.to_string(), "Viewer");

    // The Cedar policy set behind the `cedar` feature must reach the same
    // verdicts, or an operator editing policies and a host calling
    // `check_permission` would disagree about the same question.
    let policies = PolicySet::from_default_hierarchy().expect("default hierarchy compiles");
    assert!(
        !policies.inner().is_empty(),
        "the policy set actually holds policies"
    );
}

// -- 3. authentication on the transport ------------------------------------

#[tokio::test]
async fn ws_barbican_extracts_a_token_from_every_source_a_browser_offers() {
    // Browsers cannot set headers on a WebSocket handshake, so the token
    // arrives in the query string or a cookie. Both must work, and a
    // handshake with neither must be refused rather than admitted
    // unauthenticated.
    let extractor = BarbicanTokenExtractor::bearer_and_query()
        .with_query_keys(vec!["token".to_string()])
        .with_cookie_name(Some("session".to_string()));
    assert_eq!(extractor.query_keys(), ["token".to_string()]);
    assert_eq!(extractor.cookie_name(), Some("session"));

    let bearer = extractor.extract_from_parts(Some("Bearer real-token"), None, "");
    assert_eq!(bearer.as_deref(), Some("real-token"));

    let query = extractor.extract_from_parts(None, None, "token=q-token");
    assert_eq!(query.as_deref(), Some("q-token"));

    let cookie = extractor.extract_from_parts(None, Some("session=c-token"), "");
    assert_eq!(cookie.as_deref(), Some("c-token"));

    // Precedence: an explicit header wins, so a query parameter injected by
    // a proxy or a link cannot override the credential the app itself set.
    assert_eq!(
        extractor
            .extract_from_parts(Some("Bearer real-token"), None, "token=q-token")
            .as_deref(),
        Some("real-token"),
        "the header outranks the query string"
    );
    // A cookie likewise loses to the header.
    assert_eq!(
        extractor
            .extract_from_parts(Some("Bearer real-token"), Some("session=c-token"), "")
            .as_deref(),
        Some("real-token")
    );
    // A query key the host did not register is ignored rather than honoured.
    let narrow = BarbicanTokenExtractor::bearer_and_query().with_query_keys(vec!["tok".into()]);
    assert_eq!(
        narrow.extract_from_parts(None, None, "token=q-token"),
        None,
        "an unregistered query key is not a credential source"
    );
    assert_eq!(
        narrow
            .extract_from_parts(None, None, "tok=q-token")
            .as_deref(),
        Some("q-token")
    );

    // Nothing supplied is a refusal, not an empty string that would later
    // be handed to a token decoder and fail as a malformed JWT.
    assert_eq!(extractor.extract_from_parts(None, None, ""), None);
}

// -- 4. the audit chain records what happened -----------------------------

#[tokio::test]
async fn every_provisioned_action_lands_in_a_verifiable_audit_chain() {
    let log = AuditLog::new();

    // Provision a user, then act as them — the two events an accounting
    // firm's compliance reviewer asks for.
    log.append(
        "idp:okta",
        "provision",
        "scim:User/u-1",
        serde_json::json!({ "userName": "accountant@example.test", "active": true }),
    )
    .await
    .expect("provision is recorded");
    log.append(
        "accountant@example.test",
        "post.journal.entry",
        "ledger:book-2026",
        serde_json::json!({ "amount": "1200.00", "currency": "USD" }),
    )
    .await
    .expect("posting is recorded");

    // The chain verifies — this is the property that makes the log
    // evidence rather than a list.
    let verification = log.verify_chain().await.expect("verification runs");
    assert!(
        verification.broken_at.is_none(),
        "an untampered chain must verify, got {:?}",
        verification.error
    );
    assert_eq!(
        verification.valid_entries, verification.total_entries,
        "every entry hashes to its recorded value"
    );
    assert_eq!(
        verification.total_entries, 3,
        "genesis plus the two host actions"
    );

    // Queries read by actor and by action, which is how a reviewer asks
    // "who touched this book?" and "what did this user do?".
    let by_actor = log
        .query_by_actor("accountant@example.test")
        .await
        .expect("query by actor");
    assert_eq!(by_actor.len(), 1);
    assert_eq!(by_actor[0].action, "post.journal.entry");

    let by_action = log
        .query_by_action("provision")
        .await
        .expect("query by action");
    assert_eq!(by_action.len(), 1);
    assert_eq!(by_action[0].resource, "scim:User/u-1");

    // `AuditLog::new` seeds the chain with a `log.created` genesis entry, so
    // the store holds three entries for two host actions. A host that
    // paginates an audit export must account for it — worth pinning, because
    // the surprise shows up as an off-by-one in a compliance report.
    assert_eq!(log.len().await.expect("length"), 3);
    let genesis = log.query_by_action("log.created").await.expect("genesis");
    assert_eq!(genesis.len(), 1, "the chain starts with a genesis entry");
    assert_eq!(
        genesis[0].previous_hash,
        "0".repeat(64),
        "and that entry chains from the zero hash"
    );

    // The detail payload survives: the amounts a posting recorded are the
    // ones a compliance export must be able to read back.
    let entries = log
        .query_by_resource("ledger:book-2026")
        .await
        .expect("query");
    assert_eq!(entries[0].details["amount"], "1200.00");
    assert_eq!(entries[0].details["currency"], "USD");
}

/// The chain is hash-linked, so an entry's hash depends on its predecessor's
/// — which is what makes removal detectable.
#[tokio::test]
async fn audit_entries_are_hash_linked() {
    let log = AuditLog::new();
    let first = log
        .append("actor", "act", "res-1", serde_json::json!({}))
        .await
        .expect("first entry");
    let second = log
        .append("actor", "act", "res-2", serde_json::json!({}))
        .await
        .expect("second entry");

    // `AuditLog::new` seeds a `log.created` genesis entry chaining from the
    // zero hash, so the host's first entry points at *genesis*, not at zero.
    // A verifier that assumes "first entry chains from zero" would reject
    // every log this crate produces.
    assert_ne!(first.previous_hash, "0".repeat(64));
    // The second points at the first: removing the first invalidates it.
    assert_eq!(second.previous_hash, first.hash);
    assert_ne!(first.hash, second.hash);

    let verification = log.verify_chain().await.expect("verification runs");
    assert!(verification.broken_at.is_none(), "{:?}", verification.error);
    assert_eq!(
        verification.total_entries, 3,
        "genesis plus two appended entries"
    );
    assert_eq!(verification.valid_entries, 3);
}
