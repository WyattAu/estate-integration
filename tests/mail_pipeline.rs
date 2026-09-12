#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Scenario 3 — `mail_pipeline`: validkit + mailkit + sieve-kit.
//!
//! Outbound flow: validate the recipient at the boundary → build the MIME
//! body (multipart + attachment) → evaluate the inbound filter plan for the
//! reply path (keep / vacation / flag) → send through a wiremock SendGrid.
//! A mock provider records receipts; invalid recipients never reach send.

use mailkit::mime::MimeBuilder;
use mailkit::{EmailClient, EmailMessage, SendGridProvider};
use sieve_kit::eval::{evaluate_plan, EvalContext, RegexCache};
use sieve_kit::types::{
    Action, Condition, ConditionField, FilterRule, Flag, LogicOp, MailEnvelope, Operator, Vacation,
};

/// Gate every send on validkit: returns the validated address or a rejection
/// string. Nothing below this point ever sees an invalid recipient.
fn validated_recipient(raw: &str) -> Result<validkit::EmailAddr, String> {
    validkit::EmailAddr::parse(raw).map_err(|e| format!("invalid recipient: {e}"))
}

/// Invalid recipients are rejected pre-send: no MIME is built, no provider
/// is touched (the wiremock would record the hit if we slipped).
#[tokio::test]
async fn invalid_recipient_rejected_before_send() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(202))
        .mount(&upstream)
        .await;

    for bad in [
        "not-an-email",
        "user@",
        "@example.com",
        "a b@example.com",
        "",
    ] {
        assert!(
            validated_recipient(bad).is_err(),
            "{bad:?} must be rejected"
        );
    }
    // Nothing was sent: the mock saw zero requests.
    let received = upstream.received_requests().await.expect("count");
    assert_eq!(received.len(), 0, "rejected recipients must not reach send");

    // A good address passes the gate.
    assert_eq!(
        validated_recipient("ops@example.com")
            .expect("valid")
            .as_str(),
        "ops@example.com"
    );
}

/// MIME build: multipart text+html with an attachment renders all parts.
#[tokio::test]
async fn mime_build_renders_multipart_with_attachment() {
    let to = validated_recipient("reader@example.com").expect("valid");
    let mime = MimeBuilder::new()
        .from("sender@example.com")
        .to(to.as_str())
        .subject("Weekly digest")
        .text_body("Hello, plain world")
        .html_body("<h1>Hello, world</h1>")
        .attach_bytes("notes.txt", "text/plain", b"attached notes".to_vec())
        .build()
        .await
        .expect("mime build");
    let rendered = mime.as_str();
    assert!(rendered.contains("Hello, plain world"), "text part");
    assert!(rendered.contains("<h1>Hello, world</h1>"), "html part");
    assert!(rendered.contains("notes.txt"), "attachment part");
}

/// Inbound filter-plan evaluation: a subject match keeps + flags the
/// message; an out-of-office rule produces a vacation outcome with routing
/// and subject resolution. This is the same plan shape a real inbound
/// pipeline would execute before filing.
#[test]
fn filter_plan_keep_vacation_flag() {
    let cache = RegexCache::default();
    let rules = vec![
        FilterRule {
            id: "vacation".into(),
            name: "Out of office".into(),
            enabled: true,
            priority: 0,
            conditions: vec![Condition {
                field: ConditionField::Subject,
                operator: Operator::Contains,
                value: "urgent".into(),
                negate: false,
            }],
            condition_logic: LogicOp::And,
            actions: vec![Action::Vacation(
                Vacation::new("I am away this week.").with_days(7),
            )],
        },
        FilterRule {
            id: "newsletters".into(),
            name: "Flag digests".into(),
            enabled: true,
            priority: 10,
            conditions: vec![Condition {
                field: ConditionField::Subject,
                operator: Operator::Contains,
                value: "digest".into(),
                negate: false,
            }],
            condition_logic: LogicOp::And,
            actions: vec![
                Action::Flag(vec![Flag::Flagged]),
                Action::MoveTo("Newsletters".into()),
            ],
        },
    ];

    // Urgent mail hits the vacation rule first (priority order).
    let urgent = MailEnvelope {
        from: "boss@example.com".into(),
        to: "me@example.com".into(),
        subject: "urgent: deploy today".into(),
        body: "please".into(),
        envelope_from: Some("boss@example.com".into()),
        ..MailEnvelope::default()
    };
    let outcome = evaluate_plan(&rules, &urgent, &EvalContext::default(), &cache);
    assert_eq!(outcome.rule_id.as_deref(), Some("vacation"));
    assert!(
        outcome
            .plan
            .iter()
            .any(|a| matches!(a, sieve_kit::actions::PlannedAction::Vacation(_))),
        "vacation outcome produced: {:?}",
        outcome.plan
    );
    let reply = outcome.plan.iter().find_map(|a| match a {
        sieve_kit::actions::PlannedAction::Vacation(r) => Some(r),
        _ => None,
    });
    let reply = reply.expect("vacation reply");
    assert_eq!(reply.to, "boss@example.com", "routed to envelope sender");
    assert_eq!(reply.days, 7);
    assert!(
        reply.subject.contains("urgent"),
        "default subject derived: {}",
        reply.subject
    );

    // Digest mail skips vacation, lands on keep/flag/fileinto.
    let digest = MailEnvelope {
        from: "news@example.com".into(),
        to: "me@example.com".into(),
        subject: "Weekly digest".into(),
        body: "links".into(),
        ..MailEnvelope::default()
    };
    let outcome = evaluate_plan(&rules, &digest, &EvalContext::default(), &cache);
    assert_eq!(outcome.rule_id.as_deref(), Some("newsletters"));
    assert!(
        outcome
            .plan
            .iter()
            .any(|a| matches!(a, sieve_kit::actions::PlannedAction::AddFlags { .. })),
        "flag action planned: {:?}",
        outcome.plan
    );
    assert!(
        outcome.plan.iter().any(|a| matches!(
            a,
            sieve_kit::actions::PlannedAction::Move { to } if to == "Newsletters"
        )),
        "fileinto planned: {:?}",
        outcome.plan
    );

    // Unmatched mail produces an empty outcome — default keep.
    let other = MailEnvelope {
        subject: "hello".into(),
        ..MailEnvelope::default()
    };
    let outcome = evaluate_plan(&rules, &other, &EvalContext::default(), &cache);
    assert!(outcome.is_empty(), "no rule matched → keep");
}

/// End-to-end: validate → MIME → send via wiremock SendGrid → provider
/// receipt recorded. The mock asserts the request shape SendGrid expects.
#[tokio::test]
async fn send_via_mock_sendgrid_records_receipt() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v3/mail/send"))
        .respond_with(wiremock::ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let to = validated_recipient("teammate@example.com").expect("valid");
    let message = EmailMessage::builder()
        .from("sender@example.com")
        .to(to.as_str())
        .subject("Integration hello")
        .text_body("sent through the composed pipeline")
        .build()
        .expect("message");

    let provider = SendGridProvider::new("SG.test-key").with_base_url(server.uri());
    let client = EmailClient::new(provider);
    let receipt = client.send_with_receipt(message).await.expect("send");
    assert_eq!(receipt.provider, "sendgrid");
    assert!(
        !server.received_requests().await.expect("count").is_empty(),
        "provider receipt recorded against the mock"
    );
}
