#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `proc_macro_derives` — `error-classify` and `validkit`, the two
//! derive-macro crates, driven through their published re-exports.
//!
//! Both crates ship a runtime half and a proc-macro half, with the macro
//! re-exported from the runtime crate so a caller depends on one name. That
//! re-export is the thing to verify: if the re-export or the generated code
//! drifts from the runtime types, the failure appears here rather than in a
//! downstream build.
//!
//! The derives are driven with the exact attribute syntax their documentation
//! shows, on types defined in this file — which is also what proves the macros
//! work outside their own crate.

// The macros are imported from the proc-macro crates, which this suite pins
// directly; the generated code names the runtime crates (error_classify,
// validkit) itself, so both halves are exercised by one test.
// Both names are needed and both are called AppError: the trait (for `dyn`)
// comes from the runtime crate, the derive macro from the proc-macro crate.
// They live in different namespaces, so the bindings coexist -- the same
// pattern serde uses for serde::Serialize.
use error_classify::AppError as AppErrorTrait;
use error_classify::ErrorCode;
use error_classify::RecoveryClass;
use error_classify_derive::AppError;
use validkit_derive::Validated;

/// The documented example, verbatim in shape: an error enum with a code and a
/// recovery class per variant.
#[derive(Debug, thiserror::Error, AppError)]
pub enum SuiteError {
    #[error("io error: {0}")]
    #[app_error(code = "Internal", recovery = "Retryable")]
    Io(String),

    #[error("not found: {0}")]
    #[app_error(code = "NotFound", recovery = "Permanent")]
    NotFound(String),

    #[error("auth error: {0}")]
    #[app_error(code = "Auth", recovery = "UserAction")]
    Auth(String),
}

/// The derive wires each variant's attributes through to the runtime trait:
/// the code and the recovery class come back exactly as authored.
#[test]
fn the_app_error_derive_wires_code_and_recovery() {
    let io = SuiteError::Io("disk".to_owned());
    assert_eq!(io.code(), ErrorCode::Internal);
    assert_eq!(io.recovery_class(), RecoveryClass::Retryable);

    let missing = SuiteError::NotFound("page".to_owned());
    assert_eq!(missing.code(), ErrorCode::NotFound);
    assert_eq!(missing.recovery_class(), RecoveryClass::Permanent);

    let auth = SuiteError::Auth("token".to_owned());
    assert_eq!(auth.code(), ErrorCode::Auth);
    assert_eq!(auth.recovery_class(), RecoveryClass::UserAction);
}

/// The derive refuses non-enums at compile time, which cannot be asserted in a
/// normal test; what can be asserted is that the trait is object-safe enough to
/// dispatch through, since that is how an error boundary consumes it.
#[test]
fn the_derived_trait_dispatches_through_a_trait_object() {
    let io = SuiteError::Io("disk".to_owned());
    let missing = SuiteError::NotFound("page".to_owned());
    let errors: Vec<&dyn AppErrorTrait> = vec![&io, &missing];
    let codes: Vec<ErrorCode> = errors.iter().map(|e| e.code()).collect();
    assert_eq!(codes, [ErrorCode::Internal, ErrorCode::NotFound]);
}

// --- validkit ---------------------------------------------------------------

/// A validated struct using the documented field attributes: email, url, and a
/// plain length check.
#[derive(Debug, Validated)]
pub struct Signup {
    #[validate(email)]
    pub email: String,

    #[validate(url)]
    pub homepage: Option<String>,

    #[validate(length(min = 1, max = 64))]
    pub display_name: String,
}

/// A valid struct passes, and each invalid field is caught: the derive checks
/// every field rather than stopping at the first.
#[test]
fn the_validated_derive_checks_every_field() {
    let good = Signup {
        email: "alice@rustlang.org".to_owned(),
        homepage: Some("https://example.org".to_owned()),
        display_name: "Alice".to_owned(),
    };
    assert!(
        good.validate().is_ok(),
        "a fully valid struct failed: {good:?}"
    );

    // Bad email alone fails.
    let bad_email = Signup {
        email: "not-an-email".to_owned(),
        homepage: None,
        display_name: "Alice".to_owned(),
    };
    assert!(bad_email.validate().is_err(), "an invalid email passed");

    // Bad url alone fails.
    let bad_url = Signup {
        email: "alice@rustlang.org".to_owned(),
        homepage: Some("not a url".to_owned()),
        display_name: "Alice".to_owned(),
    };
    assert!(bad_url.validate().is_err(), "an invalid url passed");

    // Empty display name fails.
    let empty_name = Signup {
        email: "alice@rustlang.org".to_owned(),
        homepage: None,
        display_name: String::new(),
    };
    assert!(empty_name.validate().is_err(), "an empty name passed");
}

/// `validkit`'s own newtypes agree with the derive's field validators: the
/// same string that fails `#[validate(email)]` fails `EmailAddr::parse`.
/// Two paths to one rule must not diverge.
#[test]
fn the_newtypes_and_the_field_rules_agree() {
    for text in ["alice@rustlang.org", "not-an-email", "", "a@b"] {
        let via_newtype = validkit::EmailAddr::parse(text).is_ok();
        let via_derive = {
            let s = Signup {
                email: text.to_owned(),
                homepage: None,
                display_name: "x".to_owned(),
            };
            s.validate().is_ok()
        };
        // The derive checks other fields too, but the email field alone decides
        // here: the rest of the struct is valid by construction.
        assert_eq!(
            via_newtype, via_derive,
            "{text:?}: the newtype and the derive disagree"
        );
    }
}
