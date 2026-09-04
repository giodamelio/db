//! SHACL across a TBox/ABox graph split — shapes in one named graph, the data
//! they govern in another.
//!
//! `Txn.graph_delta` numbers a transaction's graphs from `FIRST_USER_GRAPH_ID`
//! upward in parse order, fresh per transaction, and says so
//! (`TripleTemplate::graph_id`: "not ledger-stable, must be translated"). The
//! staged overlay and every per-graph index partition are keyed by the ledger's
//! `GraphRegistry` instead — `stage()` translates through
//! `GraphRegistry::provisional_ids`. SHACL was handed the transaction's
//! numbering and passed it straight to `GraphDbRef`, so a write to the ledger's
//! *second* user graph read the *first* one. The focus node came back untyped,
//! no `sh:targetClass` matched, and every write was accepted.
//!
//! The two numberings agree when a transaction names the ledger's first user
//! graphs in the same order, which is why a single-named-graph ledger never
//! showed it. Ordering matters in every test here: the shapes graph is written
//! first so it takes `g_id` 3, leaving the data graph at 4 while the
//! transaction still calls it 3.
//!
//! A second defect sat behind that one, failing in the opposite direction.
//! `sh:class` already resolves a value's `rdf:type` across the
//! `f:shapesSource` graphs, which is what makes the "shared value-set" layout
//! in `guides/cookbook-shacl.md` work. A nested shape reached through `sh:node`
//! reads the value node's own triples, and read them from the focus graph only
//! — so a controlled vocabulary held beside the shapes was invisible and every
//! reference to it was refused as though the term were undeclared.

#![cfg(all(feature = "native", feature = "shacl"))]

use crate::support::genesis_ledger;
use fluree_db_api::{Fluree, FlureeBuilder};
use fluree_db_ledger::LedgerState;

const SHAPES_GRAPH: &str = "http://example.org/shapes";
const DATA_GRAPH: &str = "http://example.org/data";

fn config_graph_iri(ledger_id: &str) -> String {
    format!("urn:fluree:{ledger_id}#config")
}

/// A shape targeting `ex:Person`, plus the config that compiles it from
/// `SHAPES_GRAPH`. Written as one transaction, which registers the shapes graph
/// as the ledger's first user graph.
fn shapes_and_config(ledger_id: &str) -> String {
    let config_iri = config_graph_iri(ledger_id);
    format!(
        r"
        @prefix f: <https://ns.flur.ee/db#> .
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        @prefix ex: <http://example.org/> .

        GRAPH <{SHAPES_GRAPH}> {{
            ex:PersonShape rdf:type sh:NodeShape ;
                           sh:targetClass ex:Person ;
                           sh:property ex:pshape_name .
            ex:pshape_name sh:path ex:name ;
                           sh:minCount 1 ;
                           sh:datatype xsd:string .
        }}

        GRAPH <{config_iri}> {{
            <urn:config:main> rdf:type f:LedgerConfig ;
                              f:shaclDefaults <urn:config:shacl> .
            <urn:config:shacl> f:shaclEnabled true ;
                               f:shapesSource <urn:config:shapes-ref> .
            <urn:config:shapes-ref> rdf:type f:GraphRef ;
                                    f:graphSource <urn:config:shapes-source> .
            <urn:config:shapes-source> f:graphSelector <{SHAPES_GRAPH}> .
        }}
    "
    )
}

async fn with_shapes(ledger_id: &str) -> (Fluree, LedgerState) {
    let fluree = FlureeBuilder::memory().build_memory();
    let ledger = genesis_ledger(&fluree, ledger_id);
    let result = fluree
        .stage_owned(ledger)
        .upsert_turtle(&shapes_and_config(ledger_id))
        .execute()
        .await
        .expect("shapes + config write");
    (fluree, result.ledger)
}

fn is_shacl_violation(err: &fluree_db_api::ApiError) -> bool {
    matches!(
        err,
        fluree_db_api::ApiError::Transact(fluree_db_transact::TransactError::ShaclViolation(_))
    )
}

/// **The graph-id defect, at its narrowest.**
///
/// `ex:Person` with no `ex:name`, written to the ledger's *second* user graph.
/// The transaction calls that graph 3; the registry calls it 4 and calls the
/// shapes graph 3. Reading the focus node at 3 finds the shapes graph, where
/// `ex:carol` does not exist, so it comes back untyped and `sh:targetClass`
/// never fires.
///
/// Accepted here means SHACL is silently inert for every graph but the first.
#[tokio::test]
async fn a_focus_node_in_a_second_named_graph_is_validated_against_the_shapes_graph() {
    let (fluree, ledger) = with_shapes("it/shacl-second-graph:main").await;

    let err = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r"
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:carol rdf:type ex:Person .
            }}
        "
        ))
        .execute()
        .await
        .expect_err("a violating focus node in the second named graph was accepted");

    assert!(
        is_shacl_violation(&err),
        "refused for some other reason than the shape: {err:?}"
    );
}

/// The other half: the shapes must not reject a conforming node either.
///
/// A fix that pointed validation at some *other* wrong graph would still make
/// the test above pass — the focus node would look untyped there too, only in a
/// different partition. This one fails if the focus node's own triples stop
/// resolving.
#[tokio::test]
async fn a_conforming_node_in_a_second_named_graph_still_commits() {
    let (fluree, ledger) = with_shapes("it/shacl-second-graph-ok:main").await;

    fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r#"
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:dave rdf:type ex:Person ;
                        ex:name "Dave" .
            }}
        "#
        ))
        .execute()
        .await
        .expect("a conforming node in the second named graph must commit");
}

/// A controlled vocabulary beside the shapes, reached through `sh:node`.
///
/// `ex:status` is `sh:node`-constrained to `ex:InStatusScheme`, which asks for
/// `ex:inScheme ex:StatusScheme` on the *value*. That triple lives in the shapes
/// graph, the way `cookbook-shacl.md` lays out a shared value-set — the same
/// arrangement `sh:class` already honours.
fn scheme_shapes_and_config(ledger_id: &str) -> String {
    let config_iri = config_graph_iri(ledger_id);
    format!(
        r"
        @prefix f: <https://ns.flur.ee/db#> .
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix ex: <http://example.org/> .

        GRAPH <{SHAPES_GRAPH}> {{
            ex:TaskShape rdf:type sh:NodeShape ;
                         sh:targetClass ex:Task ;
                         sh:property ex:pshape_status .
            ex:pshape_status sh:path ex:status ;
                             sh:minCount 1 ;
                             sh:node ex:InStatusScheme .
            ex:InStatusScheme rdf:type sh:NodeShape ;
                              sh:property ex:pshape_scheme .
            ex:pshape_scheme sh:path ex:inScheme ;
                             sh:hasValue ex:StatusScheme .

            ex:open ex:inScheme ex:StatusScheme .
        }}

        GRAPH <{config_iri}> {{
            <urn:config:main> rdf:type f:LedgerConfig ;
                              f:shaclDefaults <urn:config:shacl> .
            <urn:config:shacl> f:shaclEnabled true ;
                               f:shapesSource <urn:config:shapes-ref> .
            <urn:config:shapes-ref> rdf:type f:GraphRef ;
                                    f:graphSource <urn:config:shapes-source> .
            <urn:config:shapes-source> f:graphSelector <{SHAPES_GRAPH}> .
        }}
    "
    )
}

async fn with_scheme_shapes(ledger_id: &str) -> (Fluree, LedgerState) {
    let fluree = FlureeBuilder::memory().build_memory();
    let ledger = genesis_ledger(&fluree, ledger_id);
    let result = fluree
        .stage_owned(ledger)
        .upsert_turtle(&scheme_shapes_and_config(ledger_id))
        .execute()
        .await
        .expect("scheme shapes + config write");
    (fluree, result.ledger)
}

/// **The value-node defect.**
///
/// `ex:open` is declared only in the shapes graph. Validating `ex:t1` in the
/// data graph means evaluating `ex:open` against `ex:InStatusScheme`, and
/// reading `ex:open` at the data graph finds nothing — so a task with a
/// perfectly good status is refused for `sh:NodeConstraintComponent`.
///
/// Refused here means the documented shared-vocabulary layout is unusable for
/// anything but `sh:class`.
#[tokio::test]
async fn a_value_nodes_vocabulary_may_live_in_the_shapes_graph() {
    let (fluree, ledger) = with_scheme_shapes("it/shacl-value-vocab:main").await;

    fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r"
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:t1 rdf:type ex:Task ;
                      ex:status ex:open .
            }}
        "
        ))
        .execute()
        .await
        .expect("a task citing a status declared in the shapes graph must commit");
}

/// The tripwire for the fix above: falling through to the vocabulary graphs
/// must not become "accept anything".
///
/// `ex:bogus` is declared nowhere, so no graph describes it, and resolution
/// stays on the data graph — where it has no `ex:inScheme` and the nested shape
/// refuses it. If this ever commits, the value-set constraint has stopped
/// constraining anything and the failure is silent.
#[tokio::test]
async fn a_value_node_no_graph_declares_is_still_refused() {
    let (fluree, ledger) = with_scheme_shapes("it/shacl-value-bogus:main").await;

    let err = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r"
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:t2 rdf:type ex:Task ;
                      ex:status ex:bogus .
            }}
        "
        ))
        .execute()
        .await
        .expect_err("an undeclared status value was accepted");

    assert!(
        is_shacl_violation(&err),
        "refused for some other reason than the shape: {err:?}"
    );
}

/// Resolution, not union — the focus graph wins when it describes the value.
///
/// `ex:open` is declared in *both* graphs, and the data graph puts it in the
/// wrong scheme. The nested shape must read the data graph's account and refuse,
/// rather than stitch the two graphs together and find the shapes graph's
/// `ex:StatusScheme` triple.
///
/// Without this, a data graph could never contradict the vocabulary — every
/// term would silently fall back to whatever the shapes graph says.
#[tokio::test]
async fn the_focus_graph_wins_when_it_describes_the_value_node() {
    let (fluree, ledger) = with_scheme_shapes("it/shacl-value-shadowed:main").await;

    let result = fluree
        .stage_owned(ledger)
        .upsert_turtle(&format!(
            r"
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:open ex:inScheme ex:OtherScheme .
            }}
        "
        ))
        .execute()
        .await
        .expect("declaring ex:open in the data graph");

    let err = fluree
        .stage_owned(result.ledger)
        .upsert_turtle(&format!(
            r"
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            @prefix ex: <http://example.org/> .
            GRAPH <{DATA_GRAPH}> {{
                ex:t3 rdf:type ex:Task ;
                      ex:status ex:open .
            }}
        "
        ))
        .execute()
        .await
        .expect_err("the shapes graph's account of ex:open overrode the data graph's");

    assert!(
        is_shacl_violation(&err),
        "refused for some other reason than the shape: {err:?}"
    );
}
