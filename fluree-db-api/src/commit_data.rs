//! Accumulate flakes and `namespace_delta` / `graph_delta` from a sequence
//! of commits into one [`CollectedCommitData`].
//!
//! Used by the merge and revert paths to bundle multiple source commits into
//! a single new commit. The two paths differ only in how each commit's
//! flakes are transformed before they're appended (identity for merge,
//! `flake.invert_at(0)` for revert), so the loop body — and especially how the
//! namespace and graph deltas combine — is shared.

use fluree_db_core::graph_registry::FIRST_USER_GRAPH_ID;
use fluree_db_core::{Commit, Flake, TxnGraphId};
use std::collections::HashMap;

/// Flakes and metadata accumulated from a sequence of commits.
#[derive(Default)]
pub(crate) struct CollectedCommitData {
    /// All flakes from the input commits, in order, after `flake_transform`.
    pub(crate) flakes: Vec<Flake>,
    /// Union of namespace deltas; earlier commits win on key collisions.
    pub(crate) namespace_delta: HashMap<u16, String>,
    /// Union of every graph IRI the source commits registered, re-numbered.
    ///
    /// Deliberately *not* a fold over the incoming keys. Each commit numbers
    /// its own graphs from `FIRST_USER_GRAPH_ID`, so two commits that each
    /// registered their first named graph both arrive keyed 3 for different
    /// IRIs — an `or_insert` on the id silently drops one, and the graph never
    /// gets registered on the target. The IRIs are the payload; the keys are
    /// re-derived here so they are at least self-consistent.
    pub(crate) graph_delta: HashMap<TxnGraphId, String>,
}

/// Fold `commits` into a [`CollectedCommitData`].
///
/// Commits must be supplied in **oldest-first** order so that earlier commits
/// take precedence on namespace delta codes (matching the historical
/// `or_insert` semantics in `merge.rs::collect_commit_data`) and so the graph
/// re-numbering below is stable.
///
/// `flake_transform` is applied to every flake before it's appended. Use
/// [`std::convert::identity`] to keep flakes as-is (merge), or
/// `|f| f.invert_at(0)` to flip assertions ⇄ retractions (revert).
pub(crate) fn collect_from_commits<I, F>(commits: I, mut flake_transform: F) -> CollectedCommitData
where
    I: IntoIterator<Item = Commit>,
    F: FnMut(Flake) -> Flake,
{
    let mut data = CollectedCommitData::default();
    // Distinct graph IRIs in first-seen order, so the re-numbering below is
    // deterministic for a given commit sequence.
    let mut graph_iris: Vec<String> = Vec::new();
    for commit in commits {
        data.flakes
            .extend(commit.flakes.into_iter().map(&mut flake_transform));
        for (code, prefix) in commit.namespace_delta {
            data.namespace_delta.entry(code).or_insert(prefix);
        }
        // Collect by IRI, not by the commit's own graph id — see
        // `CollectedCommitData::graph_delta`.
        for (_g_id, iri) in commit.graph_delta {
            if !graph_iris.contains(&iri) {
                graph_iris.push(iri);
            }
        }
    }
    data.graph_delta = graph_iris
        .into_iter()
        .enumerate()
        .map(|(i, iri)| {
            let offset = u16::try_from(i).expect("graph count exceeds u16");
            (TxnGraphId(FIRST_USER_GRAPH_ID + offset), iri)
        })
        .collect();
    data
}
