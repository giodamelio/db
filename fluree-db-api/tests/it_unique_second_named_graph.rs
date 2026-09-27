//! `f:enforceUnique` against a named graph that is not the ledger's first.
//!
//! Same root cause as the SHACL focus-graph defect (see
//! `it_shapes_named_focus_graph`): a transaction numbers its graphs privately,
//! from `FIRST_USER_GRAPH_ID` upward in parse order, while the staged overlay
//! and every per-graph index partition are keyed by the ledger's
//! `GraphRegistry`. `enforce_unique_constraints` resolved each staged flake's
//! graph through `Txn.graph_delta` and then handed that number to
//! `range_with_overlay`, so the duplicate scan ran against whichever graph the
//! registry had filed under the same id.
//!
//! Both directions were reachable. The dangerous one is fail-open: the real
//! duplicate sits in the partition that is never scanned, so fewer than two
//! subjects come back and the write commits. The staged side missed too, so
//! even a duplicate introduced within one transaction slipped through.
//!
//! Every test here writes an unrelated named graph *first*, so it takes `g_id`
//! 3 and the graph under test lands at 4 while the transaction still calls it
//! 3. Without that ordering the two numberings agree and nothing reproduces.

#![cfg(feature = "native")]

use crate::support::genesis_ledger;
use fluree_db_api::{Fluree, FlureeBuilder};
use fluree_db_ledger::LedgerState;
use serde_json::json;

const DECOY_GRAPH: &str = "http://example.org/decoy";
const DATA_GRAPH: &str = "http://example.org/data";

fn config_graph_iri(ledger_id: &str) -> String {
    format!("urn:fluree:{ledger_id}#config")
}

/// Enable uniqueness ledger-wide, annotate `ex:email`, and burn `g_id` 3 on a
/// graph nothing else touches.
///
/// The annotation goes in the default graph, which is where
/// `resolve_per_graph_unique_sids` reads constraint annotations from when no
/// `f:constraintsSource` is configured.
async fn with_unique_config(ledger_id: &str) -> (Fluree, LedgerState) {
    let fluree = FlureeBuilder::memory().build_memory();
    let ledger = genesis_ledger(&fluree, ledger_id);
    let config_iri = config_graph_iri(ledger_id);

    let result = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix f: <https://ns.flur.ee/db#> .
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            @prefix ex: <http://example.org/> .

            ex:email f:enforceUnique true .

            GRAPH <{DECOY_GRAPH}> {{
                ex:ignored ex:note "burns the first user g_id" .
            }}

            GRAPH <{config_iri}> {{
                <urn:config:main> rdf:type f:LedgerConfig ;
                                  f:transactDefaults <urn:config:transact> .
                <urn:config:transact> f:uniqueEnabled true .
            }}
        "#
        ))
        .execute()
        .await
        .expect("annotation + decoy graph + config");

    (fluree, result.ledger)
}

/// **The defect.** Two subjects, one `ex:email`, both in the second named
/// graph, across two transactions.
///
/// The first write commits and lands in the registry's `g_id` 4. The second
/// scans `g_id` 3 — the decoy graph — finds nothing, and commits the duplicate.
#[tokio::test]
async fn a_duplicate_in_a_second_named_graph_is_refused() {
    let (fluree, ledger) = with_unique_config("it/unique-second-graph:main").await;

    let first = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:alice ex:email "alice@example.com" .
            }}
        "#
        ))
        .execute()
        .await
        .expect("the first email commits");

    let err = fluree
        .stage_owned(first.ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:bob ex:email "alice@example.com" .
            }}
        "#
        ))
        .execute()
        .await
        .expect_err("a duplicate in the second named graph was accepted");

    assert!(
        matches!(
            err,
            fluree_db_api::ApiError::Transact(
                fluree_db_transact::TransactError::UniqueConstraintViolation { .. }
            )
        ),
        "refused for some other reason than uniqueness: {err:?}"
    );
}

/// The staged side of the same scan: both subjects in **one** transaction.
///
/// Nothing is committed yet, so the duplicate exists only in the staged
/// overlay — which filters by the registry's id. Scanning at the transaction's
/// number saw an empty overlay and let both through.
#[tokio::test]
async fn a_duplicate_within_one_transaction_is_refused() {
    let (fluree, ledger) = with_unique_config("it/unique-second-graph-staged:main").await;

    let err = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:alice ex:email "dup@example.com" .
                ex:bob   ex:email "dup@example.com" .
            }}
        "#
        ))
        .execute()
        .await
        .expect_err("a duplicate staged in one transaction was accepted");

    assert!(
        matches!(
            err,
            fluree_db_api::ApiError::Transact(
                fluree_db_transact::TransactError::UniqueConstraintViolation { .. }
            )
        ),
        "refused for some other reason than uniqueness: {err:?}"
    );
}

/// The other direction, which is why "scan some graph" is not good enough: two
/// subjects sharing a value across *different* graphs must both commit.
///
/// Uniqueness is per-graph. Reading the wrong partition can invent a violation
/// as easily as miss one — and the error message would name the graph that was
/// written to, not the one the conflict came from.
#[tokio::test]
async fn the_same_value_in_two_graphs_is_not_a_duplicate() {
    let (fluree, ledger) = with_unique_config("it/unique-cross-graph:main").await;

    let first = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{DECOY_GRAPH}> {{
                ex:carol ex:email "shared@example.com" .
            }}
        "#
        ))
        .execute()
        .await
        .expect("the decoy graph's email commits");

    fluree
        .stage_owned(first.ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:dave ex:email "shared@example.com" .
            }}
        "#
        ))
        .execute()
        .await
        .expect("the same value in a different graph is not a duplicate");
}

/// The default graph keeps enforcing while named graphs are in play — `g_id` 0
/// is the same in both numberings, so this passed throughout and guards against
/// a fix that re-keys everything to the wrong place.
#[tokio::test]
async fn the_default_graph_still_enforces() {
    let (fluree, ledger) = with_unique_config("it/unique-default-graph:main").await;

    let first = fluree
        .insert(
            ledger,
            &json!({
                "@context": {"ex": "http://example.org/"},
                "@id": "ex:erin",
                "ex:email": "erin@example.com"
            }),
        )
        .await
        .expect("the first default-graph email commits");

    let err = fluree
        .insert(
            first.ledger,
            &json!({
                "@context": {"ex": "http://example.org/"},
                "@id": "ex:frank",
                "ex:email": "erin@example.com"
            }),
        )
        .await
        .expect_err("a duplicate in the default graph was accepted");

    assert!(
        matches!(
            err,
            fluree_db_api::ApiError::Transact(
                fluree_db_transact::TransactError::UniqueConstraintViolation { .. }
            )
        ),
        "refused for some other reason than uniqueness: {err:?}"
    );
}
