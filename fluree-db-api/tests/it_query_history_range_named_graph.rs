//! History **ranges** over a named graph.
//!
//! `from`/`to` accept a `graph` selector — `dataset.rs` parses it, and a plain
//! (non-range) query honours it (`it_named_graphs`). A *range* query with the
//! same selector answers with no rows, so the per-flake change log (`@t` /
//! `@op`) is unavailable for anything in a named graph.
//!
//! The two controls below matter as much as the failing case: the same range
//! over the default graph works, and an as-of read of the *same named graph*
//! returns the right historical state. Together they place the defect in
//! range-style history with a graph selector, not in named graphs and not in
//! time travel generally.
//!
//! Everything here is memory-backed and unindexed, so novelty is the only
//! source of events — no sidecar, no binary cursor.

#![cfg(feature = "native")]

use fluree_db_api::{FlureeBuilder, FormatterConfig, ReindexOptions};
use serde_json::json;

const GRAPH: &str = "http://example.org/graphs/data";

fn ctx() -> serde_json::Value {
    json!({ "ex": "http://example.org/" })
}

/// Flatten a formatted row into `(?v, ?t, ?op)`.
fn flatten(row: &serde_json::Value) -> (String, i64, bool) {
    let v = row
        .get("?v")
        .and_then(|x| x.get("@value"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let t = row
        .get("?t")
        .and_then(|x| x.get("@value"))
        .and_then(serde_json::Value::as_i64)
        .or_else(|| row.get("?t").and_then(serde_json::Value::as_i64))
        .unwrap_or_default();
    let op = row
        .get("?op")
        .and_then(|x| x.get("@value"))
        .and_then(serde_json::Value::as_bool)
        .or_else(|| row.get("?op").and_then(serde_json::Value::as_bool))
        .unwrap_or_default();
    (v, t, op)
}

async fn rows(fluree: &fluree_db_api::Fluree, q: &serde_json::Value) -> Vec<(String, i64, bool)> {
    let result = fluree
        .query_from()
        .jsonld(q)
        .format(FormatterConfig::typed_json().with_normalize_arrays())
        .execute_tracked()
        .await
        .expect("history query");
    let value = serde_json::to_value(&result.result).expect("serialize");
    value
        .as_array()
        .expect("rows array")
        .iter()
        .map(flatten)
        .collect()
}

/// Two commits in the **default** graph, then a history range over them.
///
/// The control. If this ever fails, the defect below is not about graphs.
#[tokio::test]
async fn a_history_range_covers_the_default_graph() {
    let fluree = FlureeBuilder::memory().build_memory();
    let ledger_id = "it/history-default:main";
    let ledger = fluree.create_ledger(ledger_id).await.expect("create");

    let r1 = fluree
        .insert(
            ledger,
            &json!({ "@context": ctx(), "@id": "ex:alice", "ex:name": "Alice" }),
        )
        .await
        .expect("t1");
    fluree
        .upsert(
            r1.ledger,
            &json!({ "@context": ctx(), "@id": "ex:alice", "ex:name": "Alice Smith" }),
        )
        .await
        .expect("t2");

    let got = rows(
        &fluree,
        &json!({
            "@context": ctx(),
            "from": format!("{ledger_id}@t:1"),
            "to": format!("{ledger_id}@t:latest"),
            "select": ["?v", "?t", "?op"],
            "where": [{ "@id": "ex:alice", "ex:name": {"@value": "?v", "@t": "?t", "@op": "?op"} }],
            "orderBy": ["?t", "?op", "?v"],
        }),
    )
    .await;

    assert_eq!(
        got,
        vec![
            ("Alice".to_string(), 1, true),
            ("Alice".to_string(), 2, false),
            ("Alice Smith".to_string(), 2, true),
        ],
        "a history range over the default graph must carry both values and mark the retraction"
    );
}

/// Write the same two commits into a named graph.
async fn seed_named_graph(ledger_id: &str) -> fluree_db_api::Fluree {
    let fluree = FlureeBuilder::memory().build_memory();
    let ledger = fluree.create_ledger(ledger_id).await.expect("create");

    let r1 = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{GRAPH}> {{ ex:alice ex:name "Alice" . }}
        "#
        ))
        .execute()
        .await
        .expect("t1");

    fluree
        .stage_owned(r1.ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{GRAPH}> {{ ex:alice ex:name "Alice Smith" . }}
        "#
        ))
        .execute()
        .await
        .expect("t2");

    fluree
}

/// An **as-of** read of the named graph returns the historical state.
///
/// The second control. Time travel reaches the named graph perfectly well when
/// the query is a point in time rather than a range, which is what makes the
/// failure below specific rather than general.
#[tokio::test]
async fn an_as_of_read_covers_a_named_graph() {
    let ledger_id = "it/history-named-asof:main";
    let fluree = seed_named_graph(ledger_id).await;

    let result = fluree
        .query_from()
        .jsonld(&json!({
            "@context": ctx(),
            "from": { "@id": format!("{ledger_id}@t:1"), "graph": GRAPH },
            "select": ["?v"],
            "where": [{ "@id": "ex:alice", "ex:name": "?v" }],
        }))
        .format(FormatterConfig::typed_json().with_normalize_arrays())
        .execute_tracked()
        .await
        .expect("as-of query");

    let value = serde_json::to_value(&result.result).expect("serialize");
    let text = value.to_string();
    assert!(
        text.contains("Alice") && !text.contains("Alice Smith"),
        "an as-of read at t=1 of a named graph must report the value it held then, got {value}"
    );
}

/// **The defect.** The same range as the default-graph control, with a `graph`
/// selector on `from`/`to`, answers with no rows.
///
/// The selector parses — an unknown graph is rejected, and a non-range query
/// with the same selector returns data — so this is not a spelling problem. The
/// range simply yields nothing, which makes a change log unavailable for any
/// data held in a named graph, silently and with a `200`.
#[tokio::test]
async fn a_history_range_covers_a_named_graph() {
    let ledger_id = "it/history-named-range:main";
    let fluree = seed_named_graph(ledger_id).await;

    let got = rows(
        &fluree,
        &json!({
            "@context": ctx(),
            "from": { "@id": format!("{ledger_id}@t:1"), "graph": GRAPH },
            "to": { "@id": format!("{ledger_id}@t:latest"), "graph": GRAPH },
            "select": ["?v", "?t", "?op"],
            "where": [{ "@id": "ex:alice", "ex:name": {"@value": "?v", "@t": "?t", "@op": "?op"} }],
            "orderBy": ["?t", "?op", "?v"],
        }),
    )
    .await;

    assert_eq!(
        got,
        vec![
            ("Alice".to_string(), 1, true),
            ("Alice".to_string(), 2, false),
            ("Alice Smith".to_string(), 2, true),
        ],
        "a history range over a named graph answered with no rows"
    );
}

/// The same range over an **indexed** named graph, where the events come from
/// the persisted sources rather than novelty.
///
/// The test above is memory-backed and unindexed, so `collect_history_flakes`
/// skips the persisted pass entirely (`index_t = -1`) and the novelty walk is
/// the whole event stream. That leaves the sidecar and base-row halves of the
/// three-source merge unproven — they read the same `g_id`, so they *should*
/// follow, but "should" is what put the selector bug here in the first place.
///
/// Indexing between the two writes is what splits the sources: at `index_t = 1`
/// the assert of "Alice" is in base columns; after the upsert and a second
/// index, the retract and the old assert live in the sidecar and the new assert
/// is in base. So all three sources contribute.
#[tokio::test]
async fn a_history_range_covers_an_indexed_named_graph() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().to_str().expect("utf-8 path");
    let fluree = FlureeBuilder::file(path).build().expect("build");
    let ledger_id = "it/history-named-indexed:main";
    let ledger = fluree.create_ledger(ledger_id).await.expect("create");

    let r1 = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{GRAPH}> {{ ex:alice ex:name "Alice" . }}
        "#
        ))
        .execute()
        .await
        .expect("t1");
    assert_eq!(r1.receipt.t, 1);

    fluree
        .reindex(ledger_id, ReindexOptions::default())
        .await
        .expect("index at t=1");

    let r2 = fluree
        .stage_owned(fluree.ledger(ledger_id).await.expect("reload"))
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{GRAPH}> {{ ex:alice ex:name "Alice Smith" . }}
        "#
        ))
        .execute()
        .await
        .expect("t2");
    assert_eq!(r2.receipt.t, 2);

    fluree
        .reindex(ledger_id, ReindexOptions::default())
        .await
        .expect("index at t=2");

    let got = rows(
        &fluree,
        &json!({
            "@context": ctx(),
            "from": { "@id": format!("{ledger_id}@t:1"), "graph": GRAPH },
            "to": { "@id": format!("{ledger_id}@t:latest"), "graph": GRAPH },
            "select": ["?v", "?t", "?op"],
            "where": [{ "@id": "ex:alice", "ex:name": {"@value": "?v", "@t": "?t", "@op": "?op"} }],
            "orderBy": ["?t", "?op", "?v"],
        }),
    )
    .await;

    assert_eq!(
        got,
        vec![
            ("Alice".to_string(), 1, true),
            ("Alice".to_string(), 2, false),
            ("Alice Smith".to_string(), 2, true),
        ],
        "a history range over an indexed named graph did not report its persisted events"
    );
}
