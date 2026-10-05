#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 11, suite 5 — `api_errors`.
//!
//! error-codes, error-classify, typed-id-new, api-types, api-paginate,
//! json-envelope.
//!
//! The API surface layer: a typed error taxonomy, a recovery classification,
//! RFC 9457 problem details, typed UUID newtypes, and three different
//! response envelopes. Six crates that every service in the estate returns
//! from a handler, and that had never been composed — which is how a service
//! ends up returning two mutually incompatible body shapes from two endpoints,
//! or retrying a 404 forever because it trusted the status instead of the
//! recovery class.
//!
//! The suite's spine is a single error travelling every hop and being checked
//! against the previous one: variant → `ErrorCode` → HTTP status → recovery
//! class → problem document → envelope → pagination metadata. Each hop is
//! tested in its own crate; what only composition shows is whether the hops
//! agree *with each other* — and several do not.
//!
//! Five findings:
//!
//! 1. **Two crates ship a response envelope whose types collide by name.**
//!    `api-types` and `json-envelope` each export `ApiResponse<T>` and
//!    `PaginationMeta`; they are different types with incompatible
//!    constructors (`json-envelope` only offers `error` for `ApiResponse<()>`,
//!    so a generic host cannot be written against both). A service that mixes
//!    them returns two body shapes. `api-paginate` adds a third list wrapper.
//! 2. **`api-types`' `ApiError::details` is the only optional member in the
//!    crate that serializes as `null` instead of being omitted** — no
//!    `skip_serializing_if` on it, while the envelope's own `data`, `error`,
//!    `pagination` and the list wrapper's five members all omit. So the two
//!    envelopes differ on the most common response there is: a failure with
//!    no extra detail.
//! 3. **The error-code string has two casings.** `error-codes`, which owns
//!    the taxonomy, uses SCREAMING_SNAKE (`NOT_FOUND`); `api-types` and
//!    `json-envelope` take an arbitrary string and their own examples use
//!    lower_snake (`not_found`). Neither envelope validates or normalizes it,
//!    so a client switching on the code must handle both.
//! 4. **`ErrorCode::type_uri` is `https://httpstatuses.com/<status>`** —
//!    the URI RFC 9457 §3.1.2 names as an *example* that "SHOULD NOT be
//!    dereferenced". As a problem `type` it is legal but useless: `Auth` and
//!    `Unauthorized` share one URI, and nothing can be looked up.
//! 5. **The recovery class is the axis a retry loop needs, and it is not
//!    visible in the HTTP response.** `RateLimited` (429) is `Retryable`
//!    while `Forbidden` (403) is `UserAction` — same status class, opposite
//!    handling. A client that buckets by status retries a 403 forever. The
//!    class only appears in the body, and nothing standardises putting it
//!    there.

use error_classify::{AppError, CommonError, RecoveryClass};
use error_codes::{ErrorCode, HttpError, ProblemDetail};
use typed_id_new::TypedId;
use uuid::Uuid;

/// A typed id, exactly as the derive generates it: a newtype over a UUID with
/// `new` / `as_uuid` / `parse` / `nil` / `is_nil` and transparent serde.
/// `InvoiceId` and `AccountId` are distinct types, so a signature taking one
/// cannot be handed the other — that is the derive's whole point, and it is
/// invisible to a test that never mixes them.
#[derive(
    TypedId, Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct InvoiceId(Uuid);

/// A second id kind, to make the distinctness concrete.
#[derive(
    TypedId, Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct AccountId(Uuid);

// -- 1. the taxonomy: codes, statuses, recovery ---------------------------

#[test]
fn every_error_code_agrees_with_its_status_and_its_type_uri() {
    // The three things a client can branch on — the numeric status, the
    // machine-readable code, and the problem type URI — come from one enum,
    // so they cannot drift apart. This table is the contract.
    // The wire slug is SCREAMING_SNAKE, which is worth pinning: it is the
    // string a client switches on, so its case is part of the contract.
    let cases = [
        (ErrorCode::BadRequest, 400, "BAD_REQUEST"),
        (ErrorCode::Auth, 401, "AUTH"),
        (ErrorCode::Unauthorized, 401, "UNAUTHORIZED"),
        (ErrorCode::Forbidden, 403, "FORBIDDEN"),
        (ErrorCode::NotFound, 404, "NOT_FOUND"),
        (ErrorCode::Conflict, 409, "CONFLICT"),
        (ErrorCode::Validation, 422, "VALIDATION"),
        (ErrorCode::RateLimited, 429, "RATE_LIMITED"),
        (ErrorCode::Internal, 500, "INTERNAL"),
        (ErrorCode::Unavailable, 503, "UNAVAILABLE"),
    ];

    for (code, status, slug) in cases {
        assert_eq!(code.status(), status, "{code:?} status");
        assert_eq!(
            code.status_code(),
            status,
            "{code:?} status_code alias (kept for http-errors compatibility)"
        );
        assert_eq!(code.as_str(), slug, "{code:?} slug");
        // The trait is implemented for the same enum, so a generic host that
        // accepts `impl HttpError` sees identical answers.
        assert_eq!(code.status_code(), status, "{code:?} via the trait");
        assert_eq!(code.error_code(), slug, "{code:?} code via the trait");
        // -- Round-11 finding: `type_uri` is `https://httpstatuses.com/<status>`,
        // which RFC 9457 §3.1.2 explicitly names as an *example* URI that
        // "SHOULD NOT be dereferenced". As a `type` member it is therefore
        // legal but useless: it says nothing about which error occurred
        // beyond the number (so `Auth` and `Unauthorized` share a URI, and
        // they are different codes), and it cannot be dereferenced for
        // documentation. A deployment-specific `urn:` (e.g.
        // `urn:wyatt:ledger:not-found`) is what RFC 9457 asks for when the
        // sender wants the type to identify the problem class.
        assert!(
            code.type_uri().starts_with("https://httpstatuses.com/"),
            "{code:?} uses the RFC's example URI, which cannot identify the \
             problem class: {}",
            code.type_uri()
        );
        assert_eq!(
            ErrorCode::Auth.type_uri(),
            ErrorCode::Unauthorized.type_uri(),
            "and two distinct codes share one type URI, so a client cannot \
             distinguish them from the problem document alone"
        );
        // `public_message` is the sanitized reason phrase — it exists so a
        // handler cannot leak an internal message by accident.
        assert!(
            !code.public_message().is_empty(),
            "{code:?} has a public message"
        );
    }
}

#[test]
fn the_error_taxonomy_and_its_recovery_class_partition_sensibly() {
    // The pairing a host depends on: a retry loop must not trust the status
    // alone (a 404 is permanent) and must not trust the class alone (a bug is
    // not retryable). Both are asserted, per variant.
    let cases = [
        (
            CommonError::not_found("invoice"),
            404,
            RecoveryClass::Permanent,
        ),
        (
            CommonError::conflict("invoice"),
            409,
            RecoveryClass::Permanent,
        ),
        (
            CommonError::validation("tax_rate"),
            422,
            RecoveryClass::UserAction,
        ),
        (
            CommonError::timeout("ledger"),
            503,
            RecoveryClass::Retryable,
        ),
        (CommonError::rate_limited(30), 429, RecoveryClass::Retryable),
        (CommonError::internal("bug"), 500, RecoveryClass::Bug),
    ];

    for (err, status, recovery) in cases {
        assert_eq!(err.code().status(), status, "the code's status for {err:?}");
        assert_eq!(
            err.recovery_class(),
            recovery,
            "the recovery class for {err:?}"
        );
        // `kind()` is the stable log identifier — a host must be able to
        // group errors without matching on the Display string, which
        // embeds the message and is therefore not stable.
        assert!(!err.kind().is_empty(), "{err:?} has a log kind");
        assert!(
            !err.user_message().is_empty(),
            "{err:?} has a user-facing message"
        );

        match recovery {
            RecoveryClass::Retryable => assert!(
                status == 429 || (500..600).contains(&status),
                "{err:?} is retryable, so 429 or 5xx, got {status}"
            ),
            RecoveryClass::Permanent | RecoveryClass::UserAction => assert!(
                (400..500).contains(&status),
                "{err:?} is not retryable, so 4xx, got {status}"
            ),
            RecoveryClass::Bug => assert!(
                (500..600).contains(&status),
                "{err:?} is a bug, so 5xx, got {status}"
            ),
            // A reconciliation error needs data repair, so it is neither a
            // client mistake nor a plain retry — it is its own class, and the
            // suite pins that it exists rather than being folded into `Bug`.
            RecoveryClass::Reconciliation => assert!(
                (500..600).contains(&status),
                "{err:?} needs reconciliation, so 5xx"
            ),
        }
    }

    // -- Round-11 finding: the status axis alone is not enough to classify an
    // error. `Timeout` maps to `Unavailable` (503) and `Internal` to 500, so
    // they differ by status *and* by recovery class — which is the correct
    // arrangement. But `RateLimited` (429) is `Retryable` and `Forbidden`
    // (403) is `UserAction`, both 4xx, so any client that buckets errors by
    // status class loses the distinction that matters: a 429 deserves a
    // `Retry-After`, a 403 does not deserve a retry at all. The recovery
    // class is the axis a retry loop must use, and it is not visible in the
    // HTTP response — only in the body.
    assert_eq!(
        CommonError::rate_limited(30).code().status(),
        429,
        "a throttled request is a 4xx..."
    );
    assert_eq!(
        CommonError::rate_limited(30).recovery_class(),
        RecoveryClass::Retryable,
        "...and it is the 4xx that genuinely is retryable, unlike 403/404"
    );
    assert_ne!(
        CommonError::forbidden("nope").recovery_class(),
        CommonError::rate_limited(30).recovery_class(),
        "so status class cannot stand in for the recovery class"
    );
}

// -- 2. RFC 9457 problem details ------------------------------------------

#[test]
fn a_problem_detail_is_a_well_formed_rfc9457_document() {
    let problem = ProblemDetail::new(ErrorCode::Validation)
        .with_detail("tax_rate 120 is outside 0..=100")
        .with_instance("/books/2026/entries/42");

    // The core members, all derived from the one enum.
    assert_eq!(problem.status, 422);
    assert_eq!(problem.title, ErrorCode::Validation.reason());
    assert_eq!(
        problem.type_uri,
        ErrorCode::Validation.type_uri(),
        "the type member is derived from the code, not written per call site"
    );

    // The wire form is what a generated TS client consumes, so the exact key
    // names matter more than the Rust fields.
    let json: serde_json::Value = serde_json::from_str(&problem.to_json()).expect("valid JSON");
    assert_eq!(json["status"], 422);
    assert_eq!(json["title"], ErrorCode::Validation.reason());
    assert!(
        json["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("outside 0..=100"),
        "the detail carries the specifics: {json}"
    );
    assert_eq!(json["instance"], "/books/2026/entries/42");

    // And it round-trips, because a proxy may re-emit it verbatim.
    assert_eq!(ProblemDetail::new(ErrorCode::NotFound).status, 404);
    assert_eq!(
        ProblemDetail::new(ErrorCode::NotFound).type_uri,
        ErrorCode::NotFound.type_uri(),
        "the type URI is derived, not hand-written per call site"
    );
}

// -- 3. typed ids ---------------------------------------------------------

#[test]
fn a_typed_id_round_trips_and_refuses_nonsense() {
    let raw = Uuid::now_v7();
    let id = InvoiceId::new(raw);
    assert_eq!(id.as_uuid(), raw, "the inner uuid is retrievable");
    assert_eq!(InvoiceId::new(raw), id, "equality is by value");

    // `Display` is the canonical hyphenated form and `parse` reads it back, so
    // an id survives a round trip through a URL path or a log line.
    let rendered = id.to_string();
    assert_eq!(rendered, raw.hyphenated().to_string());
    assert_eq!(InvoiceId::parse(&rendered), Some(id));

    // Anything that is not a uuid is refused. `parse` returns an `Option`, so
    // a caller cannot use the value without deciding.
    assert_eq!(InvoiceId::parse(""), None);
    assert_eq!(InvoiceId::parse("   "), None);
    assert_eq!(InvoiceId::parse("inv-2026-0001"), None, "not a uuid");
    assert_eq!(InvoiceId::parse("not-a-uuid"), None);

    // The nil id is representable and detectable — a real hazard when an id
    // comes from a default rather than the database.
    assert!(InvoiceId::nil().is_nil());
    assert!(!id.is_nil());

    // Transparent serde: the wire form is the bare uuid string, not a
    // `{"0": …}` wrapper, so a client sees the id it sent.
    assert_eq!(
        serde_json::to_value(id).expect("serializes"),
        serde_json::json!(rendered)
    );
    assert_eq!(
        serde_json::from_value::<InvoiceId>(serde_json::json!(rendered)).expect("deserializes"),
        id
    );

    // Two id kinds are distinct types over distinct values; nothing in the
    // signatures lets one be passed where the other belongs.
    let account = AccountId::new(Uuid::now_v7());
    assert_ne!(account.as_uuid(), id.as_uuid());
}

// -- 4. the envelopes: three shapes ship ---------------------------------

#[test]
fn the_api_types_envelope_is_the_one_to_build_on() {
    use api_types::{ApiListResponse, ApiResponse, PaginationMeta};

    // Success: `success: true`, `data` present, `error` absent entirely —
    // `skip_serializing_if` means the key is missing, not null, which is what
    // a strict client validator expects.
    let ok: ApiResponse<u64> = ApiResponse::success(42);
    let json = serde_json::to_value(&ok).expect("serializes");
    assert_eq!(json["success"], true);
    assert_eq!(json["data"], 42);
    assert!(
        json.get("error").is_none(),
        "an absent member is not a null member: {json}"
    );

    // Failure: `error` present, `data` absent.
    let failed: ApiResponse<u64> = ApiResponse::error_with("not_found", "invoice not found");
    let json = serde_json::to_value(&failed).expect("serializes");
    assert_eq!(json["success"], false);
    assert!(json.get("data").is_none());
    assert_eq!(json["error"]["code"], "not_found");
    assert_eq!(json["error"]["message"], "invoice not found");

    // Pagination metadata: `total` is the size of the whole set, not of the
    // page — the classic off-by-one a client trusts blindly.
    let page = ApiListResponse::success(vec![1_u64, 2, 3], 2, 3, 17, 6);
    let json = serde_json::to_value(&page).expect("serializes");
    assert_eq!(json["total"], 17);
    assert_eq!(json["page"], 2);
    assert_eq!(json["total_pages"], 6, "17 items at 3 per page is 6 pages");
    assert_eq!(json["data"].as_array().map(Vec::len), Some(3));
    assert_ne!(
        Some(json["total"].as_u64().expect("a number")),
        json["data"].as_array().map(|a| a.len() as u64),
        "total and page length are different facts"
    );

    // The pagination meta also rides on a single-object response, so a client
    // has one shape to read.
    let paginated: ApiResponse<Vec<u64>> = ApiResponse::paginated(
        vec![1, 2],
        PaginationMeta {
            page: 1,
            per_page: 2,
            total: 2,
            total_pages: 1,
        },
    );
    let json = serde_json::to_value(&paginated).expect("serializes");
    assert_eq!(json["pagination"]["total"], 2);
}

/// **Round-11 finding: the estate ships three response envelopes**, and they
/// are not interchangeable. `api-types` and `json-envelope` each export a
/// struct called `ApiResponse<T>` and one called `PaginationMeta`, and
/// `api-paginate` exports a third list wrapper. They are *different types*
/// that serialize to compatible-but-distinct JSON, so a service that mixes
/// them returns two body shapes from two endpoints and neither a strict
/// client nor a generated TS client can validate both.
///
/// The suite pins the difference rather than pretending it is not there.
#[test]
fn three_response_envelopes_ship_and_two_of_them_collide_by_name() {
    // Same names, different types.
    assert_eq!(
        std::any::type_name::<api_types::ApiResponse<u64>>(),
        "api_types::response::ApiResponse<u64>"
    );
    assert_eq!(
        std::any::type_name::<json_envelope::ApiResponse<u64>>(),
        "json_envelope::envelope::ApiResponse<u64>"
    );
    assert_ne!(
        std::any::type_name::<api_types::ApiResponse<u64>>(),
        std::any::type_name::<json_envelope::ApiResponse<u64>>(),
        "two crates export a type with the same name and they are not the \
         same type — a host that mixes them returns two body shapes"
    );
    // The pagination struct collides too.
    assert_ne!(
        std::any::type_name::<api_types::PaginationMeta>(),
        std::any::type_name::<json_envelope::PaginationMeta>(),
        "PaginationMeta collides the same way"
    );

    // And the wire forms differ: `api-types` wraps the error in an object
    // with a code, `json-envelope` uses a bare string. A client written
    // against one silently mis-reads the other.
    use json_envelope::ApiResponse as EnvelopeResponse;

    let from_api_types = serde_json::to_value(api_types::ApiResponse::<u64>::error_with(
        "NOT_FOUND",
        "gone",
    ))
    .expect("serializes");
    // json-envelope only offers `error` for `ApiResponse<()>` — an error
    // response can never carry a payload type — while api-types offers
    // `error`/`error_with` for every `T`. Same concept, incompatible
    // signatures: a generic host cannot be written against both, which is the
    // concrete cost of shipping two envelopes.
    let from_json_envelope =
        serde_json::to_value(EnvelopeResponse::<()>::error("not_found", "gone"))
            .expect("serializes");

    // Both wrap the error in an object with `code` + `message`, so they are
    // closer than expected — but json-envelope adds a third `details` member
    // that api-types does not have, and only json-envelope's struct carries
    // the optional `pagination` field. A strict client generated from one
    // rejects the other.
    assert_eq!(
        from_api_types["error"]["code"], "NOT_FOUND",
        "whatever string the caller passed is stored verbatim — neither \
         envelope validates or normalizes it, so the casing is the caller's \
         problem"
    );
    assert_eq!(
        from_json_envelope["error"]["code"], "not_found",
        "json-envelope's own examples use lower_snake, while error-codes \
         uses SCREAMING_SNAKE — two conventions for the same field"
    );
    // With `details` unset both omit the key, so a plain failure serializes
    // identically. The divergence is the *declarations*: json-envelope's
    // struct carries an optional `pagination` member that api-types' does
    // not, so a client generated from api-types has no member to read even
    // though the value is reachable.
    assert!(
        from_json_envelope["error"].get("details").is_none(),
        "an unset `details` is omitted, not null: {from_json_envelope}"
    );
    assert_eq!(
        from_api_types["error"]["details"],
        serde_json::Value::Null,
        "**Round-11 finding**: api-types' `ApiError::details` is the only \
         optional member in the crate with no `skip_serializing_if`, so it \
         serializes as an explicit `null`: {from_api_types}. Every other \
         optional member in the same crate (the envelope's own `data`, \
         `error`, `pagination`, and the list wrapper's five) is omitted \
         instead. So one field's absence is a `null` while all the others' \
         absence is a missing key — and a client generated from the schema \
         sees a *nullable* `details` rather than an absent one."
    );
    assert_ne!(
        from_json_envelope, from_api_types,
        "which makes the two envelopes differ on the most common response \
         there is: a failure with no extra details"
    );
    assert_eq!(
        from_json_envelope.as_object().map(serde_json::Map::len),
        from_api_types.as_object().map(serde_json::Map::len),
        "same top-level member count, different bytes inside `error`"
    );
    // The structural difference is in the types, not the values: only
    // json-envelope's response has a `pagination` member at all.
    assert_eq!(
        std::any::type_name::<json_envelope::ApiResponse<u64>>(),
        "json_envelope::envelope::ApiResponse<u64>",
        "and json-envelope is the one that models pagination on the \
         response; api-types keeps it in a separate list wrapper"
    );

    // **Ask: pick one envelope and retire the other two.** `api-types` is the
    // strongest (structured errors, OpenAPI derives, list wrapper) and is
    // what the accounting product should use; `json-envelope` predates it.
}

/// Cursor pagination is the form that survives concurrent writes, and it is
/// invisible in the body: offset responses carry `total`/`page`, cursor
/// responses carry `hasMore`/`nextCursor`, and nothing tells a client which it
/// got.
#[test]
fn cursor_and_offset_responses_are_distinguishable_only_by_shape() {
    // Offset form: page/per_page/total, and the members a client computes
    // pages from.
    let offset = api_paginate::PaginatedResponse::new(vec![1_u64, 2], 2, 17, 10);
    let offset_json = serde_json::to_value(&offset).expect("serializes");
    assert_eq!(offset_json["total"], 17);
    assert_eq!(offset_json["page"], 2);
    assert_eq!(offset_json["per_page"], 10);
    assert!(
        offset_json.get("has_more").is_none(),
        "the offset form has no continuation token: {offset_json}"
    );

    // Cursor form: items plus an opaque continuation token.
    let cursor =
        api_paginate::CursorResponse::new(vec![1_u64, 2], Some("eyJvIjoyfQ".to_owned()), true);
    let cursor_json = serde_json::to_value(&cursor).expect("serializes");
    assert_eq!(cursor_json["has_more"], true);
    assert_eq!(cursor_json["cursor"], "eyJvIjoyfQ");
    assert_eq!(cursor_json["items"].as_array().map(Vec::len), Some(2));
    assert!(
        cursor_json.get("total").is_none(),
        "the cursor form has no total — which is the point, since a total \
         shifts as rows are inserted: {cursor_json}"
    );

    // So the two forms share no members beyond `items`/`data` and a client
    // must probe to find out which it got. And the *member names* differ too:
    // offset uses `items`, cursor uses `items` but the api-types list wrapper
    // uses `data` — three spellings of "the array" across three crates.
    assert_eq!(offset_json["items"].as_array().map(Vec::len), Some(2));
    assert_ne!(
        offset_json.as_object().map(serde_json::Map::len),
        cursor_json.as_object().map(serde_json::Map::len),
        "the two forms do not even have the same member count, so a strict \
         schema for one rejects the other"
    );
}

// -- 5. one error, every hop, in order ------------------------------------

/// The assertion that catches a status disagreeing with its own body: one
/// error, every hop, each checked against the last.
#[test]
fn one_error_survives_the_whole_path_consistently() {
    use api_types::ApiResponse;

    // Hop 1: the service's own error type.
    let err = CommonError::not_found("inv-404");

    // Hop 2: the code, and its status.
    let code = err.code();
    assert_eq!(code, ErrorCode::NotFound);
    assert_eq!(code.status(), 404);

    // Hop 3: the problem document agrees with the status it travels under —
    // derived from the same enum, so it cannot drift.
    let detail = ProblemDetail::new(code).with_detail(err.to_string());
    assert_eq!(detail.status, code.status());

    // Hop 4: the envelope a client parses carries the same code and status.
    let envelope = ApiResponse::<()>::error_with(code.as_str(), &detail.title);
    let json = serde_json::to_value(&envelope).expect("serializes");
    assert_eq!(json["success"], false);
    assert_eq!(json["error"]["code"], "NOT_FOUND");
    assert_eq!(detail.status, 404);
    assert_eq!(
        code.as_str(),
        "NOT_FOUND",
        "and the code under test agrees with what the envelope was given, \
         because the suite passed ErrorCode::as_str() through"
    );

    // Hop 5: the id survives the round trip, so a retry of the same request
    // refers to the same invoice.
    let raw = Uuid::now_v7();
    let id = InvoiceId::new(raw);
    assert_eq!(InvoiceId::parse(&id.to_string()), Some(id));
    assert!(err.to_string().contains("inv-404"));
}
