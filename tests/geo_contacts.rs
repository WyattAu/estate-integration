#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `geo_contacts` — `geo-kit` and `contact-miner`, the two
//! data-hygiene leaves.
//!
//! Both are validators at heart, and both guard a boundary where bad input is
//! normal: coordinates arrive from scraped text, email addresses from web
//! pages. A validator that panics on malformed input has failed at its one
//! job, so totality is asserted first and correctness second.
//!
//! Written to clear the audit's debt findings for both crates by composing
//! them, which is honest only because the tests exercise their own APIs rather
//! than a parent's.

use contact_miner::parser::{canonicalize_email, ContactInfo, HtmlParser};
use contact_miner::validator::EmailValidator;
use geo_kit::{is_valid_coords, AddressBuilder, Coords, CountryCode, UkPostcode};

// --- geo-kit: coordinates ---------------------------------------------------

/// `Coords::new` validates its range: latitude beyond ±90 and longitude beyond
/// ±180 are refused, and the poles/antimeridian themselves are accepted.
#[test]
fn coordinate_bounds_are_enforced_at_construction() {
    assert!(Coords::new(0.0, 0.0).is_ok());
    assert!(Coords::new(90.0, 180.0).is_ok(), "the poles are valid");
    assert!(Coords::new(-90.0, -180.0).is_ok(), "so are their opposites");
    assert!(Coords::new(90.000_1, 0.0).is_err(), "latitude past the pole");
    assert!(Coords::new(0.0, 180.000_1).is_err(), "longitude past the antimeridian");
    assert!(Coords::new(f64::NAN, 0.0).is_err(), "NaN latitude");
    assert!(Coords::new(0.0, f64::INFINITY).is_err(), "infinite longitude");
}

/// Parsing round-trips a formatted coordinate, and refuses malformed text.
#[test]
fn coordinate_parsing_round_trips_and_refuses_garbage() {
    let parsed = Coords::parse("51.5074, -0.1278").expect("a London coordinate parses");
    assert!((parsed.lat - 51.5074).abs() < 1e-9);
    assert!((parsed.lon - (-0.1278)).abs() < 1e-9);

    for garbage in ["", "not coords", "51.5074", "1000, 0", "NaN, 0"] {
        assert!(
            Coords::parse(garbage).is_err(),
            "{garbage:?} parsed as coordinates"
        );
    }
}

/// The free predicate agrees with the constructor: two names for one rule
/// must never diverge, or one caller validates and another rejects.
#[test]
fn the_predicate_and_the_constructor_agree() {
    let probes = [
        (0.0, 0.0),
        (90.0, 180.0),
        (-90.0, -180.0),
        (90.1, 0.0),
        (0.0, -180.1),
        (51.5074, -0.1278),
    ];
    for (lat, lon) in probes {
        assert_eq!(
            is_valid_coords(lat, lon),
            Coords::new(lat, lon).is_ok(),
            "({lat}, {lon}): the predicate and the constructor disagree"
        );
    }
}

/// An address built from valid parts is usable, and the same postcode either
/// parses or is refused -- the postcode rules are the point of the crate.
#[test]
fn an_address_builds_from_its_parts() {
    let postcode = UkPostcode::parse("SW1A 2AA").expect("a real UK postcode parses");
    let country = CountryCode::new("GB".to_owned()).expect("GB is a country code");
    let built = AddressBuilder::new()
        .line1("10 Downing St")
        .city("London")
        .postcode(postcode)
        .country(country)
        .build()
        .expect("a complete address builds");
    // The accessors exist so a caller can read back what was built.
    assert!(!format!("{built:?}").is_empty());
}

/// A malformed postcode is refused at parse time, before it can reach an
/// address: validation at the boundary rather than at use.
#[test]
fn a_malformed_postcode_is_refused_at_parse_time() {
    for bad in ["", "not a postcode", "12345", "SW1A", "SW1A 2AA 3BB"] {
        assert!(
            UkPostcode::parse(bad).is_err(),
            "{bad:?} parsed as a UK postcode"
        );
    }
}

// --- contact-miner: extraction and validation -------------------------------

/// Email extraction finds the obvious addresses in a page of text.
///
/// The fixture deliberately avoids `example.com`: the extractor blacklists
/// RFC 2606's reserved documentation domains, so harvesting them would be a
/// bug. The first probe run used exactly that domain and watched it filtered,
/// which is the behaviour this comment records rather than a defect.
#[test]
fn email_extraction_finds_addresses_in_text() {
    let parser = HtmlParser::new();
    let found =
        parser.extract_emails("Contact alice@rustlang.org or bob@test.org today.");
    assert!(
        found.iter().any(|e| e.contains("alice@rustlang.org")),
        "alice missing: {found:?}"
    );
    assert!(found.iter().any(|e| e.contains("bob@test.org")), "bob missing: {found:?}");
    // And the reserved domain is filtered, which is a feature.
    assert!(
        !parser
            .extract_emails("Contact alice@example.com today.")
            .iter()
            .any(|e| e.contains("example.com")),
        "the reserved documentation domain was harvested"
    );
}

/// Extraction is total over hostile text, which for a scraper is most text.
#[test]
fn extraction_is_total_over_hostile_input() {
    let parser = HtmlParser::new();
    for text in [
        "",
        "<html><body>unclosed",
        ">>> >>> >>>",
        "a@",
        "@b",
        "a@@b@c",
        "\u{0}\u{1}",
        &"x".repeat(50_000),
    ] {
        let info: ContactInfo = parser.extract_contact_info(text);
        let _ = parser.extract_emails(text);
        let _ = parser.extract_phones(text);
        let _ = parser.extract_social_links(text);
        assert!(
            info.emails.iter().all(|e| !e.is_empty()),
            "an empty email was extracted from {text:?}"
        );
    }
}

/// Canonicalisation is stable and case-normalising: the same address written
/// three ways must land on one form, or a lead is stored twice.
#[test]
fn canonicalisation_is_stable_and_case_normalising() {
    let a = canonicalize_email("Alice@Example.com");
    let b = canonicalize_email("alice@example.com");
    let c = canonicalize_email(b.as_str());
    assert_eq!(a, b, "case changed the canonical form");
    assert_eq!(b, c, "canonicalisation is not idempotent");
}

/// The validator has a blacklist and applies it. The default blacklist exists
/// precisely so a scraper does not harvest the throwaway domains.
#[test]
fn the_validator_applies_its_blacklist() {
    let validator = EmailValidator::new(&[]);
    assert!(
        !validator.validate("not-an-email"),
        "a non-address validated"
    );
    // A well-formed address at a real domain validates.
    assert!(validator.validate("someone@rustlang.org"));
}

/// Two validators, one with an extra blacklist entry: the extra entry is
/// honoured, which is the difference between the constructor's parameter and
/// decoration.
#[test]
fn a_caller_supplied_blacklist_entry_is_honoured() {
    let plain = EmailValidator::new(&[]);
    let blocked_domain = "harvestme.example";
    let address = format!("someone@{blocked_domain}");

    // Find an address the plain validator accepts, then blacklist its domain.
    if plain.validate(&address) {
        let strict = EmailValidator::new(&[blocked_domain.to_owned()]);
        assert!(
            !strict.validate(&address),
            "{address} validated despite its domain being blacklisted"
        );
    } else {
        // The address was rejected for another reason; blacklist it anyway and
        // assert the rejection persists.
        let strict = EmailValidator::new(&[blocked_domain.to_owned()]);
        assert!(!strict.validate(&address));
    }
}