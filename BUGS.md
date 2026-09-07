# Bugs and issues in this fork

Found while making SHACL shapes in one named graph validate data in another.
Most of what follows shares a root cause: Fluree numbers graphs in two spaces,
and until `1fd2ac723609` both were plain `u16`.

- **Transaction-local** — assigned fresh per transaction from
  `FIRST_USER_GRAPH_ID` (3) in parse order, carried in `Txn.graph_delta` /
  `Commit.graph_delta`.
- **Ledger** — `GraphRegistry`, which keys the staged overlay, novelty, and
  every per-graph index partition. `GraphDbRef` takes one of these.

Reading per-graph data with a transaction id returns **another graph's data**,
never an error. The two coincide only when a transaction names the ledger's
first user graphs in the same order, so a ledger with a single named graph
never reproduces any of it.

## Outstanding

### Bugs

- **`derive_graph_routing` may fabricate a colliding graph id.**
  `fluree-db-api/src/commit_transfer.rs:806` assigns ids to overlay-only graphs
  as `max_g_id + 1..`, where `max_g_id` covers only graphs *resolved in that
  commit* rather than the registry's `next_id`. A fabricated id can therefore
  land on a graph the registry already knows, and base reads at that id would
  return the other graph's data. Same family as the fixed defects below, but a
  different mechanism, so it is not covered by `TxnGraphId`.
  **Unconfirmed** — no test, and reachability on the commit-replay path is not
  established.

### Issues

- **Pre-existing clippy failure.** `fluree-db-core/src/storage/file.rs:557`
  trips `clippy::manual_is_variant_and` under the current toolchain
  (`.ok().is_some_and(..)` → `is_ok_and`). Unrelated to any change here; blocks
  a clean `cargo clippy --workspace --all-features --all-targets`.

- **`cargo test` cannot run this suite correctly; use `cargo nextest run`.**
  `it_sync_graph::whole_graph_scan_backstop_fails_loud_before_materializing`
  sets `FLUREE_MAX_GRAPH_SCAN_FLAKES=2` and relies on nextest's
  process-per-test isolation, which its own doc comment states. Under plain
  `cargo test` the variable leaks into concurrently-running tests in the same
  binary, and `sync_does_not_touch_other_graphs` /
  `policy_gated_sync_targets_the_named_graph` fail with
  `WholeGraphScanTooLarge { limit: 2 }`. CI already uses nextest.

- **`bm25_auto_sync::commit_advances_the_index_without_an_explicit_sync` is
  load-sensitive.** Failed once under a full 11k-test parallel run (5.4s, vs
  0.16s solo), passes reliably in isolation, and did not recur.

- **`--all-features` tests do not fit on a machine with ~50 GB free.** The
  build consumed 27 GB and died mid-compile. Compile coverage via
  `cargo check --workspace --all-features --all-targets` is cheap; running the
  tests is not.

## Fixed

- **A history range over a named graph answered with no rows.**
  `from`/`to` accept a `graph` selector, and every other query path applies it
  — `load_view_from_source` re-selects the view after resolving the time spec,
  which is why an as-of read of a named graph returns the right historical
  state. A history range takes an early return in
  `build_dataset_view_from_spec!` that builds its view with
  `GraphDb::from_ledger_state` (the default graph) and never reads
  `spec.default_graphs[0].graph_selector`; `HistoryTimeRange` carries only the
  ledger identifier, so the selector had no other reader on that branch. It was
  parsed, validated, stored, and dropped, and every range over a named graph
  scanned g_id 0 — a `200` with an empty list rather than an error.

  Not a graph-id-space bug, despite the family resemblance: the selector was
  discarded rather than mistranslated.

  The defect sits upstream of the three-source merge in
  `BinaryHistoryScanOperator`, so sidecar, base rows and novelty were all
  reading the same wrong partition and all three are fixed together.
  → `6c5ffa56bf66` *fix(query): honour the graph selector on a history range*
  Tests: `it_query_history_range_named_graph::a_history_range_covers_a_named_graph`,
  `::a_history_range_covers_an_indexed_named_graph` (reindexes between the two
  writes so each source contributes), plus two controls that must stay green —
  `::a_history_range_covers_the_default_graph` (which passed throughout only
  because g_id 0 is correct there by coincidence) and
  `::an_as_of_read_covers_a_named_graph`.

  **Downstream:** cthulhu's `kb::history` issues this query directly, so its
  HTTP route *and* its MCP tool were both returning empty lists. Once the rev
  is bumped, `entities::a_history_is_empty_for_an_entity_in_a_named_graph`
  becomes an intended-red tripwire and its `#[ignore]`d twin
  `a_history_carries_retractions_as_well_as_assertions` is what should replace
  it.

- **SHACL validated focus nodes against the wrong graph.**
  `apply_shacl_policy_to_staged_view` took `graph_delta` verbatim and
  `validate_staged_nodes` passed the transaction's number straight to
  `GraphDbRef`. Writing to a ledger's *second* named graph read the *first*
  one's partition, so the focus node came back untyped, no `sh:targetClass`
  matched, and every write was accepted — shapes present, config correct,
  nothing enforcing. The per-graph policy map
  (`build_per_graph_shacl_policy`) was keyed the same way. Failed **open**.
  → `fa9bbdb75ffa` *fix(shacl): validate a focus node in the graph the ledger filed it under*
  Tests: `it_shapes_named_focus_graph::a_focus_node_in_a_second_named_graph_is_validated_against_the_shapes_graph`,
  `::a_conforming_node_in_a_second_named_graph_still_commits`.

- **A value node's vocabulary in the shapes graph was invisible.**
  `sh:class` already unions the `f:shapesSource` graphs into a value's
  `rdf:type` lookup, which is what makes the shared value-set layout in
  `guides/cookbook-shacl.md` work. A nested shape reached through `sh:node` /
  `sh:and` / `sh:or` / `sh:not` / `sh:xone` reads the value node's own triples
  instead, and read them from the focus graph alone — so a controlled
  vocabulary held beside the shapes failed as though every term were
  undeclared. Splitting a TBox into its own graph therefore traded a silent
  accept-everything for a loud refuse-everything. Failed **closed**.
  Resolution, not union: the focus graph wins whenever it describes the node,
  so a data graph can still contradict the vocabulary and an undeclared value
  is still refused. Node-level `sh:node` is deliberately untouched — it applies
  to the focus node being written, which must keep reading its own graph.
  → `16c333da8b61` *fix(shacl): resolve a value node against the graph that describes it*
  Tests: `it_shapes_named_focus_graph::a_value_nodes_vocabulary_may_live_in_the_shapes_graph`,
  `::a_value_node_no_graph_declares_is_still_refused`,
  `::the_focus_graph_wins_when_it_describes_the_value_node`.

- **`f:enforceUnique` scanned the wrong graph.**
  `enforce_unique_constraints` resolved each staged flake's graph through
  `Txn.graph_delta` and handed that number to `range_with_overlay`. The real
  duplicate sat in the partition never scanned, so it committed; the staged
  overlay was invisible too, so even a duplicate inside a single transaction
  slipped through. The reverse is also reachable — two unrelated subjects
  sharing a `(p, o)` in whichever graph the number happened to name invent a
  violation, and the error names the graph you *wrote to*, not the one the
  conflict came from. Failed **open** (mostly). Caught by the compiler once the
  newtype landed.
  → `1fd2ac723609` *refactor(core): make the transaction-local graph id a type, not a u16*
  Tests: `it_unique_second_named_graph::a_duplicate_in_a_second_named_graph_is_refused`,
  `::a_duplicate_within_one_transaction_is_refused`,
  `::the_same_value_in_two_graphs_is_not_a_duplicate`,
  `::the_default_graph_still_enforces`.

- **Merging a branch that created a named graph failed outright.**
  `merge` built its `Sid → GraphId` routing from the target's registry alone,
  before the merge commit registers the source's graphs, so the source's flakes
  had nowhere to route:
  `staged flake has unknown graph Sid '[13:g1]' not in reverse_graph`. Every
  non-fast-forward merge of a branch that created a named graph hit it. Failed
  **closed**.
  → `7a484d736b7c` *fix(merge): carry a source branch's named graphs into the target*

- **Merging two named graphs dropped one registration.**
  `collect_from_commits` folded each source commit's `graph_delta` with
  `entry(g_id).or_insert(iri)` — correct for the `namespace_delta` beside it,
  since namespace codes are ledger-global, but wrong for graph ids. Two commits
  that each registered their first named graph both arrive keyed `3` for
  different IRIs, and the second was dropped. Masked entirely by the bug above
  until that was fixed.
  **This is the one instance `TxnGraphId` cannot catch** — both sides of the
  collision are transaction-local, so the types agree and only the meaning is
  wrong. It needs a test, not a type.
  → `7a484d736b7c` *fix(merge): carry a source branch's named graphs into the target*
  Tests: `it_merge_graph_delta_collision::a_branch_registering_one_named_graph_merges`,
  `::a_branch_registering_two_named_graphs_merges_both`. With only the routing
  fix applied, the two-graph case still fails on `g2` specifically.

- **Structural fix: the two graph-id spaces are now distinct types.**
  `TxnGraphId` in `fluree-db-core/src/ids.rs` is a `#[repr(transparent)]`
  newtype, matching `PredicateId` / `TxnT` in the same module; `GraphId` stays
  a `u16` alias. Making only one side nominal is enough for a mix to stop
  compiling, and leaves ~129 ledger-space read sites untouched.
  `GraphRegistry::ledger_graph_delta` is the one blessed crossing. The wire
  format is unchanged — the newtype is transparent over the `u16` the commit
  codec always wrote.
  Two paths turned out to key `graph_delta` by a *registry's* numbering rather
  than a transaction's — bulk import (shared graph allocator, so envelope and
  index agree) and the merge simulation. Both now say so at the conversion
  instead of blending into a shared `u16`.
  → `1fd2ac723609` *refactor(core): make the transaction-local graph id a type, not a u16*
