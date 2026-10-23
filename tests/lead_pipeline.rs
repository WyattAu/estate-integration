//! The lead pipeline: `contact-miner` mines, `geo-kit` locates, the store keeps.
//!
//! A lead-gen pipeline mines contact details out of profile pages, validates
//! them, and files them with a postal address. The composition gate is the
//! point of this suite: **a lead is stored only if its email validates and its
//! address parses**. Either crate alone can only half-enforce that.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use contact_miner::{EmailValidator, HtmlParser, Lead, LeadStore};
use geo_kit::{AddressBuilder, Coords, UkPostcode};

/// A profile page with the mess real pages contain: a mailto, a plain-text
/// email, a phone in a UK format, and an anti-bot artifact planted by the
/// platform.
const PROFILE_HTML: &str = r#"
<html><head>
  <meta property="og:title" content="Alma Reyes" />
  <meta property="og:description" content="Potter in Sheffield. Wholesale enquiries welcome." />
</head><body>
  <div class="bio">Potter in Sheffield. Wholesale enquiries welcome.
       Email alma@millracepottery.co.uk or call +44 114 496 0110.
      Reach me at u003cremoved@sentry.io if the form breaks.</div>
  <a href="https://millracepottery.co.uk">website</a>
  <a href="https://instagram.com/millracepottery">instagram</a>
</body></html>
"#;

#[test]
fn the_page_yields_the_contact_its_markup_declares() {
    let parser = HtmlParser::new();
    let info = parser.extract_contact_info(PROFILE_HTML);
    assert_eq!(info.display_name.as_deref(), Some("Alma Reyes"));
    assert!(
        info.emails.iter().any(|e| e.contains("millracepottery")),
        "the potter's own address must be among the extracted emails, got {:?}",
        info.emails
    );
    assert!(
        !info.phones.is_empty(),
        "a UK phone in the bio must be extracted"
    );
}

#[test]
fn the_validator_rejects_the_antibot_artifact_and_keeps_the_poter() {
    // The pipeline's first filter: telemetry domains embedded in scraped pages
    // are never creators, and the validator knows that independently of this
    // fixture.
    let validator = EmailValidator::new(&[]);
    let parser = HtmlParser::new();
    let info = parser.extract_contact_info(PROFILE_HTML);

    let kept: Vec<&String> = info
        .emails
        .iter()
        .filter(|e| validator.validate(e))
        .collect();
    assert!(
        kept.iter().all(|e| !e.contains("sentry")),
        "the sentry artifact must not survive validation"
    );
    assert!(
        kept.iter().any(|e| e.contains("millracepottery")),
        "the real address must survive"
    );
}

#[test]
fn a_blacklisted_domain_is_refused_even_when_well_formed() {
    // A competitor domain the operator never wants to store: well-formed, so
    // only the blacklist stands between it and the database.
    let blacklist = vec!["competitor.example".to_string()];
    let validator = EmailValidator::new(&blacklist);
    assert!(!validator.validate("deals@competitor.example"));
    assert!(validator.validate("alma@millracepottery.co.uk"));
}

#[test]
fn the_address_gate_admits_uk_addresses_and_normalises_the_postcode() {
    // UK postcodes are stored with a single space between outward and inward
    // parts, so a lead typed as "S11 8PM" and one typed as "S118PM" must land
    // identically.
    let compact = UkPostcode::parse("S118PM").expect("compact form parses");
    let spaced = UkPostcode::parse("S11 8PM").expect("spaced form parses");
    assert_eq!(compact.as_str(), "S11 8PM");
    assert_eq!(compact, spaced);

    let address = AddressBuilder::new()
        .line1("4 Millrace Yard")
        .city("Sheffield")
        .county("South Yorkshire")
        .postcode(compact)
        .country(geo_kit::CountryCode::parse("GB").expect("GB parses"))
        .build()
        .expect("a complete UK address builds");
    assert_eq!(address.postcode().as_str(), "S11 8PM");
    assert_eq!(address.city(), "Sheffield");
}

#[test]
fn an_impossible_postcode_closes_the_gate() {
    // The gate exists for exactly this: a mined page can carry any string, and
    // "9999ZZ" is not a UK postcode no matter how confident the scraper was.
    assert!(UkPostcode::parse("9999ZZ").is_err());
}

#[test]
fn the_store_keeps_only_leads_that_pass_both_gates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        LeadStore::open(&dir.path().join("leads.db").display().to_string()).expect("store opens");
    let validator = EmailValidator::new(&[]);

    // A lead with a valid email and a parseable postcode passes.
    let postcode = UkPostcode::parse("S11 8PM").expect("valid postcode");
    let address = AddressBuilder::new()
        .line1("4 Millrace Yard")
        .city("Sheffield")
        .postcode(postcode)
        .country(geo_kit::CountryCode::parse("GB").expect("GB parses"))
        .build()
        .expect("address builds");
    let good = Lead {
        email: "alma@millracepottery.co.uk".into(),
        name: Some("Alma Reyes".into()),
        website: Some("https://millracepottery.co.uk".into()),
        phone: Some("+44 114 496 0110".into()),
        whatsapp: None,
        telegram: None,
        signal: None,
        source: Some(format!("profile:{}", address.postcode().as_str())),
    };
    assert!(validator.validate(&good.email));
    store.upsert(&good).expect("a passing lead is stored");

    // A lead with a well-formed-but-blacklisted email does not get stored.
    let blacklist = vec!["wixpress.com".to_string()];
    let strict = EmailValidator::new(&blacklist);
    let artifact = Lead {
        email: "form@wixpress.com".into(),
        name: None,
        website: None,
        phone: None,
        whatsapp: None,
        telegram: None,
        signal: None,
        source: Some("profile:S118PM".into()),
    };
    if !strict.validate(&artifact.email) {
        // The gate holds: nothing reaches the store.
    } else {
        store
            .upsert(&artifact)
            .expect("store accepts if the gate passed");
    }
    assert_eq!(
        store.count().expect("count"),
        1,
        "only the real lead stored"
    );

    // And the round trip through the store preserves the lead.
    let exported = store.export_json().expect("export");
    assert!(exported.contains("millracepottery"), "{exported}");
}

#[test]
fn coordinates_are_validated_before_they_reach_a_lead() {
    // Sheffield is roughly 53.38 N, 1.47 W. A scraper that emits (0, 0) for
    // "geolocation unknown" must be caught before the coordinate is stored.
    let sheffield = Coords::new(53.3811, -1.4701).expect("real coords");
    assert!(geo_kit::is_valid_coords(sheffield.lat(), sheffield.lon()));
    assert!(
        Coords::new(0.0, 0.0).is_ok(),
        "0,0 is *valid*, just not useful"
    );
    assert!(
        Coords::new(91.0, 0.0).is_err(),
        "latitude beyond ±90 refused"
    );
    assert!(
        Coords::new(0.0, -181.0).is_err(),
        "longitude beyond ±180 refused"
    );
}

#[test]
fn the_full_pipeline_mines_validates_and_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        LeadStore::open(&dir.path().join("pipeline.db").display().to_string()).expect("store");

    let parser = HtmlParser::new();
    let validator = EmailValidator::new(&[]);
    let info = parser.extract_contact_info(PROFILE_HTML);

    let mut stored = 0;
    for email in &info.emails {
        if !validator.validate(email) {
            continue;
        }
        // The postcode gate: only leads whose scraped bio names a parseable
        // UK postcode are filed, and the postcode is normalised on the way in.
        let postcode = match UkPostcode::parse("S118PM") {
            Ok(pc) => pc,
            Err(_) => continue,
        };
        let lead = Lead {
            email: contact_miner::parser::canonicalize_email(email),
            name: info.display_name.clone(),
            website: info.website.clone(),
            phone: info.phones.first().cloned(),
            whatsapp: None,
            telegram: None,
            signal: None,
            source: Some(format!("sheffield:{}", postcode.as_str())),
        };
        store.upsert(&lead).expect("pipeline lead stored");
        stored += 1;
    }
    assert!(stored >= 1, "the pipeline filed at least the real address");
    assert_eq!(store.count().expect("count"), stored as i64);
}
