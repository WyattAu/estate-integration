#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Living example: validate → MIME → filter-plan evaluation.
//!
//! ```sh
//! cargo run --example mail_compose
//! ```

use mailkit::mime::MimeBuilder;
use sieve_kit::eval::{evaluate_plan, EvalContext, RegexCache};
use sieve_kit::types::{
    Action, Condition, ConditionField, FilterRule, Flag, LogicOp, MailEnvelope, Operator, Vacation,
};

#[tokio::main]
async fn main() {
    let to = validkit::EmailAddr::parse("reader@example.com").expect("valid");
    let mime = MimeBuilder::new()
        .from("sender@example.com")
        .to(to.as_str())
        .subject("Weekly digest")
        .text_body("Hello, plain world")
        .html_body("<h1>Hello, world</h1>")
        .build()
        .await
        .expect("mime");
    println!("MIME bytes: {}", mime.as_bytes().len());

    let rules = vec![FilterRule {
        id: "flag-digests".into(),
        name: "Flag digests".into(),
        enabled: true,
        priority: 0,
        conditions: vec![Condition {
            field: ConditionField::Subject,
            operator: Operator::Contains,
            value: "digest".into(),
            negate: false,
        }],
        condition_logic: LogicOp::And,
        actions: vec![
            Action::Flag(vec![Flag::Flagged]),
            Action::Vacation(Vacation::new("Away this week.").with_days(7)),
        ],
    }];
    let msg = MailEnvelope {
        subject: "Weekly digest".into(),
        ..MailEnvelope::default()
    };
    let outcome = evaluate_plan(
        &rules,
        &msg,
        &EvalContext::default(),
        &RegexCache::default(),
    );
    println!("matched rule: {:?}", outcome.rule_id);
    println!("planned actions: {}", outcome.plan.len());
}
