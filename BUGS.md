# Bugs and issues in this fork

These turned up while making SHACL shapes in one named graph validate data in
another, and later while making access policy carry per-key auth. Commits are
named by title, because rebasing onto upstream changes their hashes.

Most of the graph bugs share a root cause: Fluree numbers graphs in two spaces.

- **Transaction-local ids** are assigned fresh per transaction, from
  `FIRST_USER_GRAPH_ID` (3), in parse order.
- **Ledger ids** come from `GraphRegistry`, which keys the staged overlay,
  novelty, and every per-graph index partition. `GraphDbRef` takes one of these.

Reading per-graph data with a transaction id returns **another graph's data**,
never an error. The two numberings agree only when a transaction names the
ledger's first user graphs in the same order, so a ledger with a single named
graph never reproduces any of it.

This fork's first answer was a type, `TxnGraphId`, that made mixing the two
fail to compile. Upstream's answer is structural: `stage_with_graph_delta`
returns the staged delta already keyed by ledger id, so most of the places the
two met no longer exist. The type was dropped when the fork was rebased onto
that. Its regression tests were kept, and pass against upstream's fix.

## Outstanding

### Bugs

- **`derive_graph_routing` may fabricate a colliding graph id.**
  - What happens: `fluree-db-api/src/commit_transfer.rs` assigns ids to
    overlay-only graphs from `max_g_id + 1` upward. `max_g_id` covers only the
    graphs resolved in that commit, not the registry's `next_id`, so a
    fabricated id can land on a graph the registry already knows. Base reads at
    that id would then return the other graph's data.
  - Status: still present in upstream's code. **Unconfirmed**: there is no
    test, and it is not established that the commit-replay path reaches it.

### Issues

- **`cargo test` cannot run this suite correctly; use `cargo nextest run`.**
  - `it_sync_graph::whole_graph_scan_backstop_fails_loud_before_materializing`
    sets `FLUREE_MAX_GRAPH_SCAN_FLAKES=2` and relies on nextest's
    process-per-test isolation.
  - Under plain `cargo test` the variable leaks into other tests in the same
    binary.

- **`it_exists_semijoin_correlation` does not compile under rustc 1.98.**
  - The compiler reports that queries overflow the depth limit.
  - `rust-toolchain.toml` pins 1.97.0, which a nix devShell does not honour.
  - Build the test groups you need rather than every target.

- **`--all-features` tests do not fit on a machine with about 50 GB free.**
  The build consumed 27 GB and died mid-compile. Compile coverage via
  `cargo check --workspace --all-features --all-targets` is cheap.

## Fixed in this fork, not upstream

- **A history range over a named graph answered with no rows.**
  - What happened: every other query path applies the `graph` selector. A
    history range takes an early return in `build_dataset_view_from_spec!` that
    builds its view on the default graph and never reads the selector. Every
    range over a named graph scanned graph 0, and returned a `200` with an
    empty list rather than an error.
  - Fix: *fix(query): honour the graph selector on a history range*.
  - Tests (in `it_query_history_range_named_graph`):
    - `a_history_range_covers_a_named_graph`
    - `a_history_range_covers_an_indexed_named_graph`
    - Controls that stay green throughout: `a_history_range_covers_the_default_graph`
      and `an_as_of_read_covers_a_named_graph`.

- **A value node's vocabulary in the shapes graph was invisible.**
  - What happened: a nested shape reached through `sh:node`, `sh:and`, `sh:or`,
    `sh:not` or `sh:xone` read the value node's triples from the focus graph
    alone. A controlled vocabulary held beside the shapes therefore failed as
    though every term were undeclared. This failed **closed**.
  - Fix: resolution, not union. The focus graph wins whenever it describes the
    node, and only a node it says nothing about falls through to the vocabulary
    graphs. *fix(shacl): resolve a value node against the graph that describes it*.
  - Tests: in `it_shapes_named_focus_graph`.

- **A BM25 indexing query could not index a named graph.**
  - What happened: `execute_bm25_indexing_query` builds its view on graph 0,
    hard-coded, so a graph selector in the indexing query matched nothing.
  - Fix: *fix(bm25): honour a graph selector on an indexing query*.

- **Publishing an index renumbered the graph registry.**
  - What happened: graphs registered since the index were carried forward by
    IRI through `apply_delta`, which sorts them. That reordered ids handed out
    one per commit. Novelty is keyed by graph id, so rows were read back as
    another graph's, misfiled rather than lost.
  - Fix: `merge_preserving_ids` carries the ids, and the indexer's
    `apply_graph_delta` numbers graphs in the registry's order.
    *fix(ledger): keep graph ids when an index reseeds the registry*.

- **A read-side policy condition read the default graph.**
  - What happened: the enforcer built its condition executor without the
    flake's graph id, while the write path passed it. Every `f:query` over
    named-graph data consulted graph 0.
  - Fix: *fix(query): read a policy condition from its flake's named graph*.
  - Tests: `it_policy_named_graphs::policy_condition_reads_the_named_graph_of_its_flake`.

## Fixed upstream; this fork's regression test kept

Each of these was fixed here first, and upstream fixed it independently. The
fork's fix was dropped in the rebase, and its test stays as evidence that
upstream's fix covers the case.

- **SHACL validated focus nodes against the wrong graph.** Writes to a ledger's
  second named graph were accepted with shapes present. Failed **open**.
  Upstream's staged ledger-space delta fixes it.
  Tests: `it_shapes_named_focus_graph`.

- **`f:enforceUnique` scanned the wrong graph.**
  - What happened: it missed real duplicates, including within one
    transaction, and could invent one from an unrelated subject.
  - Upstream's staged ledger-space delta fixes it.
  - Tests: `it_unique_second_named_graph`.

- **Merging a branch that created a named graph failed, and two named graphs
  collapsed into one.** Tests: `it_merge_graph_delta_collision`, which covers
  one named graph and two.

- **A dictionary-miss diagnostic panicked on a multi-byte character.** Upstream
  now truncates by character.
