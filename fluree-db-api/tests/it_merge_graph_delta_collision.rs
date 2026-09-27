//! Merging a branch that registered more than one named graph.
//!
//! Namespace codes are ledger-global; graph ids have not always been. A merge
//! that folds a branch's commits' graph deltas by id treats two commits that
//! each numbered their first named graph alike as registering one graph, and
//! drops the second IRI from the merge commit. This fork once did exactly that.
//!
//! The collision is between two numberings of the same kind, so no type can
//! rule it out — only a test with two named graphs on the branch, one per
//! commit, can.

#![cfg(feature = "native")]

use crate::support;
use fluree_db_api::{ConflictStrategy, FlureeBuilder};

const G1: &str = "http://example.org/g1";
const G2: &str = "http://example.org/g2";

/// The narrower question first: does merging a branch that registered a
/// **single** named graph work at all?
///
/// `merge` builds its `Sid → GraphId` routing from the *target's* registry
/// (`merge.rs`, `build_reverse_graph`) before the merge commit's `graph_delta`
/// is applied, so a graph only the source knows has no entry. If this fails,
/// the fold collision below is unreachable and the real defect is broader.
#[tokio::test]
async fn a_branch_registering_one_named_graph_merges() {
    let fluree = FlureeBuilder::memory().build_memory();
    let ledger = fluree.create_ledger("onegraph").await.expect("create");

    let base = fluree
        .stage_owned(ledger)
        .upsert_turtle(
            r#"
            @prefix ex: <http://example.org/> .
            ex:seed ex:v "base" .
        "#,
        )
        .execute()
        .await
        .expect("base commit");

    fluree
        .create_branch("onegraph", "dev", None, None)
        .await
        .expect("create dev");

    fluree
        .stage_owned(base.ledger)
        .upsert_turtle(
            r#"
            @prefix ex: <http://example.org/> .
            ex:main-only ex:v "diverged" .
        "#,
        )
        .execute()
        .await
        .expect("main diverges");

    let dev = fluree.ledger("onegraph:dev").await.expect("dev ledger");
    fluree
        .stage_owned(dev)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{G1}> {{ ex:a ex:v "in-g1" . }}
        "#
        ))
        .execute()
        .await
        .expect("dev commit registering g1");

    fluree
        .merge_branch("onegraph", "dev", None, ConflictStrategy::default())
        .await
        .expect("merging a branch that registered one named graph");
}

/// Two named graphs, registered by two separate commits on a branch, both
/// survive the merge.
///
/// Each `insert` is its own transaction, so each numbers its graph 3 — the
/// collision. The merge is forced non-fast-forward by committing on `main`
/// first, which is the path that folds source commits through
/// `collect_from_commits`.
#[tokio::test]
async fn a_branch_registering_two_named_graphs_merges_both() {
    let fluree = FlureeBuilder::memory().build_memory();
    let ledger = fluree.create_ledger("mydb").await.expect("create ledger");

    let base = fluree
        .stage_owned(ledger)
        .upsert_turtle(
            r#"
            @prefix ex: <http://example.org/> .
            ex:seed ex:v "base" .
        "#,
        )
        .execute()
        .await
        .expect("base commit");

    fluree
        .create_branch("mydb", "dev", None, None)
        .await
        .expect("create dev");

    // Diverge main so the merge cannot fast-forward.
    fluree
        .stage_owned(base.ledger)
        .upsert_turtle(
            r#"
            @prefix ex: <http://example.org/> .
            ex:main-only ex:v "diverged" .
        "#,
        )
        .execute()
        .await
        .expect("main diverges");

    // Two commits on dev, each registering one named graph. Both call it 3.
    let dev = fluree.ledger("mydb:dev").await.expect("dev ledger");
    let dev = fluree
        .stage_owned(dev)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{G1}> {{ ex:a ex:v "in-g1" . }}
        "#
        ))
        .execute()
        .await
        .expect("dev commit registering g1");

    fluree
        .stage_owned(dev.ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix ex: <http://example.org/> .
            GRAPH <{G2}> {{ ex:b ex:v "in-g2" . }}
        "#
        ))
        .execute()
        .await
        .expect("dev commit registering g2");

    fluree
        .merge_branch("mydb", "dev", None, ConflictStrategy::default())
        .await
        .expect("merge dev into main");

    let main = fluree.ledger("mydb:main").await.expect("main ledger");
    for (graph, expected) in [(G1, "in-g1"), (G2, "in-g2")] {
        let rows = support::query_sparql_formatted(
            &fluree,
            &main,
            &format!(
                "PREFIX ex: <http://example.org/>
                 SELECT ?o WHERE {{ GRAPH <{graph}> {{ ?s ex:v ?o }} }}"
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("query {graph} after merge: {e}"));

        let got = rows.as_array().map(Vec::as_slice).unwrap_or_default();
        assert!(
            !got.is_empty(),
            "<{graph}> is empty after the merge — its registration was dropped \
             when the source commits' graph deltas were folded on colliding \
             transaction-local ids (expected {expected:?})"
        );
    }
}
