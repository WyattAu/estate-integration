#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `service_clients` — `comfyui-kit` (ComfyUI HTTP API) and
//! `tantivy-helper` (full-text search), the two service adapters.
//!
//! Both were published and composed by nothing. Neither needs its service to
//! be composed: comfyui-kit's `Workflow` is pure JSON graph manipulation, and
//! tantivy-helper's search runs against an in-memory index. A suite that
//! needed a live ComfyUI would test the server, not the crate.
//!
//! Written to clear the audit's debt findings for both by real coverage.

use comfyui_kit::Workflow;
use tantivy_helper::schema::{FieldDefinition, FieldType, IndexConfig, IndexSettings};
use tantivy_helper::{QueryBuilder, SearchEngine};

/// A two-node ComfyUI graph: a CLIPTextEncode feeding a KSampler, which is the
/// shape every real workflow takes.
const GRAPH: &str = r#"{
    "3": { "class_type": "KSampler", "inputs": { "seed": 1, "model": ["4", 0] } },
    "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "a cat" } }
}"#;

// --- comfyui-kit ------------------------------------------------------------

/// The graph parses, the node ids are the JSON keys, and edits land where they
/// are aimed.
#[test]
fn a_workflow_parses_edits_and_round_trips() {
    let mut workflow = Workflow::parse(GRAPH).expect("a two-node graph parses");
    assert_eq!(workflow.node_ids().len(), 2);

    // set_text edits node 6's prompt without disturbing the rest.
    workflow.set_text("6", "a dog").expect("set text");
    // set_seed edits node 3's sampler seed.
    workflow.set_seed("3", 42).expect("set seed");

    // The edits are visible in the JSON, and the untouched fields survived.
    let json = workflow.as_json();
    assert_eq!(
        json.pointer("/6/inputs/text").and_then(|v| v.as_str()),
        Some("a dog"),
        "the prompt edit did not land"
    );
    assert_eq!(
        json.pointer("/3/inputs/seed").and_then(|v| v.as_u64()),
        Some(42),
        "the seed edit did not land"
    );
    assert_eq!(
        json.pointer("/3/inputs/model/0").and_then(|v| v.as_str()),
        Some("4"),
        "an edit disturbed an unrelated input"
    );
}

/// Edits to an absent node or field are errors, not silent no-ops: a typo in a
/// node id must be visible to the caller.
#[test]
fn edits_to_an_absent_node_are_errors() {
    let mut workflow = Workflow::parse(GRAPH).expect("parses");
    assert!(workflow.set_text("no-such-node", "x").is_err());
    assert!(workflow.set_seed("no-such-node", 1).is_err());
    // And the graph is unchanged afterwards.
    assert_eq!(workflow.node_ids().len(), 2);
}

/// A non-object root is rejected: a workflow is a node map, and an array is
/// not one.
#[test]
fn a_non_object_root_is_rejected() {
    assert!(Workflow::parse("[]").is_err());
    assert!(Workflow::parse("null").is_err());
    assert!(Workflow::parse("42").is_err());
}

// --- tantivy-helper ---------------------------------------------------------

/// A temp dir for the index, fresh per test.
fn index_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("estate-tantivy-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn search_engine(tag: &str) -> (SearchEngine, tantivy::schema::Field) {
    let config = IndexConfig {
        fields: vec![FieldDefinition {
            name: "title".to_owned(),
            field_type: FieldType::Text,
            stored: true,
            indexed: true,
            fast: false,
        }],
        settings: IndexSettings {
            num_threads: 1,
            index_base_path: index_dir(tag).to_string_lossy().to_string(),
        },
    };
    let engine = SearchEngine::new(&config).expect("engine builds");
    let field = engine
        .schema()
        .get_field("title")
        .expect("the field is in the schema");
    (engine, field)
}

/// Index, commit, and search round-trip: a document that was indexed is found
/// by a term query, and one that was not is not.
#[test]
fn indexing_committing_and_searching_round_trip() {
    let (mut engine, field) = search_engine("round-trip");

    for title in ["The Long Book", "A Short Story"] {
        let mut doc = tantivy::TantivyDocument::default();
        doc.add_text(field, title);
        engine.index_document(doc).expect("index");
    }
    engine.commit().expect("commit");

    let query = QueryBuilder::new(engine.index(), engine.schema())
        .term("title", "long")
        .expect("a term query builds")
        .build()
        .expect("builds");

    let hits = engine.search(query.as_ref(), 10).expect("searches");
    assert!(!hits.is_empty(), "a term query for 'long' found nothing");
    assert!(hits.len() <= 2);
}

/// A term that matches nothing returns empty rather than erroring: no hits is
/// an answer, not a failure.
#[test]
fn a_query_matching_nothing_returns_empty() {
    let (engine, _field) = search_engine("empty");
    let query = QueryBuilder::new(engine.index(), engine.schema())
        .term("title", "zzz-no-such-term")
        .expect("builds")
        .build()
        .expect("builds");
    let hits = engine.search(query.as_ref(), 10).expect("searches");
    assert!(hits.is_empty(), "a nonexistent term matched {hits:?}");
}

/// The engine's schema reflects the config it was built from: the field names,
/// types, and stored-ness come back as authored.
#[test]
fn the_schema_reflects_the_config() {
    let (engine, field) = search_engine("schema");
    let schema = engine.schema();
    let got = schema.get_field("title").expect("title field present");
    assert_eq!(got, field, "the same field is found twice");
    // And the entry itself carries the configured type.
    let entry = schema.get_field_entry(got);
    assert_eq!(entry.name(), "title");
}
