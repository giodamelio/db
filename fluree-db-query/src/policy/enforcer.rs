//! Policy enforcer for batch filtering
//!
//! Provides the `QueryPolicyEnforcer` which filters flakes by policy with caching.

use super::QueryPolicyExecutor;
use crate::error::Result;
use fluree_db_core::{Flake, GraphId, LedgerSnapshot, OverlayProvider, Sid, Tracker};
use fluree_db_policy::{is_schema_flake, ClassScope, PolicyContext};
use std::sync::Arc;

/// Plan-time-style verdict for a single statically-known scanned predicate under
/// an enforcer's *view* policy.
///
/// Lets a single-predicate fast path skip the per-flake policy filter when the
/// predicate is provably uncovered by the view policy
/// (see [`PolicySet::covers_predicate`](fluree_db_policy::PolicySet::covers_predicate)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateCoverage {
    /// Whether a flake with this predicate is visible depends on the flake;
    /// the per-flake filtered scan fallback is required.
    Covered,
    /// Every flake with this predicate is visible, whatever its subject or
    /// object — no rule reaches it and the default is allow, or only
    /// unconditional allows do — so a fast path may run unfiltered.
    UncoveredAllow,
    /// Every flake with this predicate is hidden, so the result for it is
    /// empty.
    UncoveredDeny,
}

/// Policy enforcer for query execution
///
/// Wraps a `PolicyContext` and provides async batch filtering for flakes.
/// Designed to be used by scan operators for per-leaf filtering.
///
/// # Caching (TODO)
///
/// Future versions will cache f:query results to avoid re-executing
/// the same policy query for every flake.
#[derive(Clone)]
pub struct QueryPolicyEnforcer {
    /// The policy context containing restrictions and identity
    policy: Arc<PolicyContext>,
    // TODO: Add PolicyQueryCache for memoization
    // cache: Arc<PolicyQueryCache>,
}

impl QueryPolicyEnforcer {
    /// Create a new policy enforcer
    pub fn new(policy: Arc<PolicyContext>) -> Self {
        Self { policy }
    }

    /// Get the underlying policy context
    pub fn policy(&self) -> &PolicyContext {
        &self.policy
    }

    /// Check if this is a root policy (bypasses all checks)
    pub fn is_root(&self) -> bool {
        self.policy.wrapper().is_root()
    }

    /// Classify a single statically-known scanned predicate against the *view*
    /// policy, amortizing the wrapper/view/default_allow walk into one call.
    ///
    /// A root enforcer reports [`PredicateCoverage::Covered`]: callers are
    /// expected to short-circuit the no-policy / root case via
    /// [`ExecutionContext::allow_unfiltered`](crate::context::ExecutionContext::allow_unfiltered)
    /// before reaching here, so the root arm is only a defensive fallback (it
    /// forces the filtered path, which is correct — just slower — for root).
    ///
    /// Beyond predicates no rule reaches, a predicate is settled when only
    /// unconditional untargeted rules reach it
    /// ([`PolicySet::static_decision_for_predicate`](fluree_db_policy::PolicySet::static_decision_for_predicate)).
    /// Not while policy is being tracked: the counters report the rules that
    /// ran, so only a predicate no rule reaches may skip running them.
    pub fn classify_view_predicate(&self, predicate: &Sid, tracker: &Tracker) -> PredicateCoverage {
        let wrapper = self.policy.wrapper();
        if wrapper.is_root() {
            return PredicateCoverage::Covered;
        }
        let view = wrapper.view();
        let verdict = if tracker.tracks_policy() {
            (!view.covers_predicate(predicate)).then(|| wrapper.default_allow())
        } else {
            view.static_decision_for_predicate(predicate, wrapper.default_allow())
        };
        match verdict {
            None => PredicateCoverage::Covered,
            Some(true) => PredicateCoverage::UncoveredAllow,
            Some(false) => PredicateCoverage::UncoveredDeny,
        }
    }

    /// Filter a batch of flakes by policy using explicit graph parameters.
    ///
    /// This is the **correct** method for dataset mode - it uses the graph's
    /// db/overlay/to_t, ensuring `f:query` policies run against the same
    /// snapshot that produced the flakes.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The database for this graph
    /// * `g_id` - The graph these flakes belong to, used to look up class
    ///   membership in the graph that actually asserted it
    /// * `overlay` - The overlay provider for this graph
    /// * `to_t` - Target transaction time for this graph
    /// * `tracker` - Fuel tracker for limits
    /// * `flakes` - Flakes to filter
    ///
    /// # Returns
    ///
    /// Filtered flakes that pass policy checks
    pub async fn filter_flakes_for_graph(
        &self,
        snapshot: &LedgerSnapshot,
        g_id: GraphId,
        overlay: &dyn OverlayProvider,
        to_t: i64,
        tracker: &Tracker,
        flakes: Vec<Flake>,
    ) -> Result<Vec<Flake>> {
        // Root policy bypasses all checks
        if self.policy.wrapper().is_root() {
            return Ok(flakes);
        }

        // Create executor using the GRAPH's snapshot/overlay/to_t (not ctx-level!)
        let executor =
            QueryPolicyExecutor::with_overlay(snapshot, overlay, to_t).with_graph_id(g_id);

        let verdicts = self.static_verdicts(&flakes, tracker);

        let subjects: Vec<Sid> = flakes
            .iter()
            .zip(&verdicts)
            .filter(|(_, verdict)| verdict.is_none())
            .map(|(flake, _)| flake.s.clone())
            .collect();
        let scope = self
            .resolve_classes(snapshot, g_id, overlay, to_t, &subjects)
            .await?;

        let mut result = Vec::with_capacity(flakes.len());

        for (flake, verdict) in flakes.into_iter().zip(verdicts) {
            match verdict {
                Some(true) => {
                    result.push(flake);
                    continue;
                }
                Some(false) => continue,
                None => {}
            }

            // Schema flakes always allowed
            if is_schema_flake(&flake.p, &flake.o) {
                result.push(flake);
                continue;
            }

            let subject_classes = self
                .policy
                .get_cached_subject_classes(scope, g_id, &flake.s)
                .unwrap_or_default();

            // Async policy check with f:query support
            match self
                .policy
                .allow_view_flake_async(
                    &flake.s,
                    &flake.p,
                    &flake.o,
                    &subject_classes,
                    &executor,
                    tracker,
                )
                .await
            {
                Ok(true) => result.push(flake),
                Ok(false) => {} // Filtered out by policy (a genuine deny is Ok(false))
                // An Err here is an execution failure (malformed f:query, storage
                // IO, cooperative cancellation, or fuel exhaustion mid-scan), not a
                // policy denial. Propagate it: silently dropping the flake would
                // turn "too expensive"/"transient failure" into a successful
                // response with fewer rows than the identity is authorized to see.
                Err(e) => return Err(crate::error::QueryError::Policy(e.to_string())),
            }
        }

        Ok(result)
    }

    /// Check if a single flake is allowed by policy using explicit graph parameters.
    ///
    /// This is the correct method for dataset mode.
    pub async fn allow_flake_for_graph(
        &self,
        snapshot: &LedgerSnapshot,
        g_id: GraphId,
        overlay: &dyn OverlayProvider,
        to_t: i64,
        tracker: &Tracker,
        flake: &Flake,
    ) -> Result<bool> {
        // Root policy bypasses all checks
        if self.policy.wrapper().is_root() {
            return Ok(true);
        }

        // Schema flakes always allowed
        if is_schema_flake(&flake.p, &flake.o) {
            return Ok(true);
        }

        // Create executor using the GRAPH's snapshot/overlay/to_t
        let executor =
            QueryPolicyExecutor::with_overlay(snapshot, overlay, to_t).with_graph_id(g_id);

        let scope = self
            .resolve_classes(
                snapshot,
                g_id,
                overlay,
                to_t,
                std::slice::from_ref(&flake.s),
            )
            .await?;
        let subject_classes = self
            .policy
            .get_cached_subject_classes(scope, g_id, &flake.s)
            .unwrap_or_default();

        // Async policy check
        self.policy
            .allow_view_flake_async(
                &flake.s,
                &flake.p,
                &flake.o,
                &subject_classes,
                &executor,
                tracker,
            )
            .await
            .map_err(|e| crate::error::QueryError::Policy(e.to_string()))
    }

    /// Each flake's decision where its predicate alone settles it, and `None`
    /// where it has to be evaluated.
    ///
    /// Decided once per predicate in the batch, since a scan batch is usually
    /// one predicate. Not used while policy is being tracked: the per-policy
    /// counters report the rules that ran, and a decision taken without
    /// running them would change what a tracked query says about itself.
    fn static_verdicts(&self, flakes: &[Flake], tracker: &Tracker) -> Vec<Option<bool>> {
        if tracker.tracks_policy() {
            return vec![None; flakes.len()];
        }
        let view = self.policy.wrapper().view();
        let default_allow = self.policy.wrapper().default_allow();
        let mut by_predicate: Vec<(&Sid, Option<bool>)> = Vec::new();
        flakes
            .iter()
            .map(|flake| {
                if let Some((_, verdict)) = by_predicate.iter().find(|(p, _)| **p == flake.p) {
                    return *verdict;
                }
                let verdict = view.static_decision_for_predicate(&flake.p, default_allow);
                by_predicate.push((&flake.p, verdict));
                verdict
            })
            .collect()
    }

    /// Make sure every subject's classes are cached for this snapshot and graph,
    /// and hand back the scope they were cached under.
    ///
    /// Done here rather than left to callers because a miss read as "no
    /// classes" is not conservative: it stops a class-targeted *deny* applying,
    /// and the flake falls through to whatever allows it. Filtering used to
    /// depend on every caller having populated first, under the same key.
    async fn resolve_classes(
        &self,
        snapshot: &LedgerSnapshot,
        g_id: GraphId,
        overlay: &dyn OverlayProvider,
        to_t: i64,
        subjects: &[Sid],
    ) -> Result<ClassScope> {
        fluree_db_policy::populate_class_cache(
            subjects,
            fluree_db_core::GraphDbRef::new(snapshot, g_id, overlay, to_t),
            &self.policy,
        )
        .await
        .map_err(|e| crate::error::QueryError::Policy(e.to_string()))?;
        Ok(ClassScope::new(&snapshot.ledger_id, to_t))
    }

    /// Populate the class cache for subjects using a graph database reference.
    ///
    /// An optimisation only: the filters resolve any subject not cached here.
    pub async fn populate_class_cache_for_graph(
        &self,
        db: fluree_db_core::GraphDbRef<'_>,
        subjects: &[fluree_db_core::Sid],
    ) -> Result<()> {
        fluree_db_policy::populate_class_cache(subjects, db, &self.policy)
            .await
            .map_err(|e| crate::error::QueryError::Policy(e.to_string()))?;
        Ok(())
    }
}

impl std::fmt::Debug for QueryPolicyEnforcer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryPolicyEnforcer")
            .field("is_root", &self.is_root())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluree_db_core::{
        ClassPropertyUsage, ClassStatEntry, FlakeValue, IndexStats, IndexType, NoOverlay,
    };
    use fluree_db_policy::{
        build_policy_set, PolicyAction, PolicyRestriction, PolicySet, PolicyValue, PolicyWrapper,
        TargetMode,
    };
    use fluree_vocab::namespaces::{JSON_LD, RDF, XSD};
    use fluree_vocab::predicates::RDF_TYPE;
    use std::any::Any;
    use std::collections::{HashMap, HashSet};

    struct Flakes(Vec<Flake>);

    impl OverlayProvider for Flakes {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn epoch(&self) -> u64 {
            1
        }

        fn for_each_overlay_flake(
            &self,
            _g_id: GraphId,
            _index: IndexType,
            _first: Option<&Flake>,
            _rhs: Option<&Flake>,
            _leftmost: bool,
            to_t: i64,
            callback: &mut dyn FnMut(&Flake),
        ) {
            self.0.iter().filter(|f| f.t <= to_t).for_each(callback);
        }
    }

    fn sid(name: &str) -> Sid {
        Sid::new(100, name)
    }

    fn typed(subject: &str, class: &str) -> Flake {
        Flake::new(
            sid(subject),
            Sid::new(RDF, RDF_TYPE),
            FlakeValue::Ref(sid(class)),
            Sid::new(JSON_LD, "id"),
            1,
            true,
            None,
        )
    }

    fn secret(subject: &str) -> Flake {
        Flake::new(
            sid(subject),
            sid("secret"),
            FlakeValue::String("hash".into()),
            Sid::new(XSD, "string"),
            1,
            true,
            None,
        )
    }

    fn restriction(
        id: &str,
        mode: TargetMode,
        value: PolicyValue,
        required: bool,
        classes: &[Sid],
    ) -> PolicyRestriction {
        PolicyRestriction {
            id: id.to_string(),
            target_mode: mode,
            targets: HashSet::new(),
            action: PolicyAction::View,
            verbs: None,
            value,
            required,
            message: None,
            class_policy: mode == TargetMode::OnClass,
            for_classes: classes.iter().cloned().collect(),
            class_check_needed: mode == TargetMode::OnClass,
        }
    }

    fn class_using_secret(class: &str) -> ClassStatEntry {
        ClassStatEntry {
            class_sid: sid(class),
            count: 1,
            properties: vec![ClassPropertyUsage {
                property_sid: sid("secret"),
                datatypes: Vec::new(),
                langs: Vec::new(),
                ref_classes: Vec::new(),
            }],
        }
    }

    /// An allow-everything rule and a required deny on instances of `Key` —
    /// the shape a master key has, built through the real indexer rather than
    /// by hand, since the indexes are what decide whether a class check runs.
    ///
    /// A view-set class rule covers the properties the stats say its class
    /// uses, and checks the subject's class only when another class uses them
    /// too. Both `Key` and `Note` using `secret` is what puts the deny on
    /// `secret` *with* a class check, which is the decision under test.
    fn allow_all_but_keys() -> QueryPolicyEnforcer {
        let stats = IndexStats {
            flakes: 4,
            size: 0,
            properties: None,
            classes: Some(vec![class_using_secret("Key"), class_using_secret("Note")]),
            graphs: None,
            historical_since_t: None,
        };
        let view = build_policy_set(
            vec![
                restriction("allow", TargetMode::Default, PolicyValue::Allow, false, &[]),
                restriction(
                    "no-keys",
                    TargetMode::OnClass,
                    PolicyValue::Deny,
                    true,
                    &[sid("Key")],
                ),
            ],
            Some(&stats),
            PolicyAction::View,
            None,
        );
        let wrapper = PolicyWrapper::new(view, PolicySet::new(), false, false, HashMap::new());
        QueryPolicyEnforcer::new(Arc::new(PolicyContext::new(wrapper, None)))
    }

    /// Nothing populated the class cache before this filter ran. It used to
    /// read the miss as "no classes", so the class deny never applied and the
    /// key's secret came back under the allow — every caller had to remember
    /// to populate first, under the same key, and one that did not failed open.
    #[tokio::test]
    async fn a_class_deny_applies_when_nothing_populated_the_cache() {
        let snapshot = LedgerSnapshot::genesis("test:main");
        let overlay = Flakes(vec![typed("key1", "Key"), typed("note1", "Note")]);
        let enforcer = allow_all_but_keys();

        let kept = enforcer
            .filter_flakes_for_graph(
                &snapshot,
                0,
                &overlay,
                1,
                &Tracker::disabled(),
                vec![secret("key1"), secret("note1")],
            )
            .await
            .expect("filter");

        let subjects: Vec<&str> = kept.iter().map(|f| f.s.name.as_ref()).collect();
        assert_eq!(subjects, ["note1"], "the key's flake got past the deny");
    }

    /// A subject with no `rdf:type` has no classes, and that answer is kept.
    /// The second filter sees an overlay in which the subject has since become
    /// a `Key`, so a fresh lookup would deny it; still being allowed shows the
    /// cached answer was used rather than looked up again.
    #[tokio::test]
    async fn a_subject_with_no_classes_is_cached_as_having_none() {
        let snapshot = LedgerSnapshot::genesis("test:main");
        let enforcer = allow_all_but_keys();

        let untyped = enforcer
            .filter_flakes_for_graph(
                &snapshot,
                0,
                &NoOverlay,
                1,
                &Tracker::disabled(),
                vec![secret("key1")],
            )
            .await
            .expect("filter");
        assert_eq!(untyped.len(), 1, "an untyped subject is not a Key");

        let typed_now = Flakes(vec![typed("key1", "Key")]);
        let again = enforcer
            .filter_flakes_for_graph(
                &snapshot,
                0,
                &typed_now,
                1,
                &Tracker::disabled(),
                vec![secret("key1")],
            )
            .await
            .expect("filter");
        assert_eq!(
            again.len(),
            1,
            "the empty answer was not cached, so the subject was looked up again"
        );
    }

    /// A dataset attaches one context to every view in it, so a subject cached
    /// from one ledger must not answer for the same `Sid` in another at the same
    /// `t`. Here ledger A's `key1` is untyped and ledger B's is a `Key`; with
    /// the ledger missing from the key, B's lookup is skipped as already cached
    /// and its secret is read under A's "no classes".
    #[tokio::test]
    async fn a_subject_cached_from_one_ledger_does_not_answer_for_another() {
        let enforcer = allow_all_but_keys();

        let from_a = enforcer
            .filter_flakes_for_graph(
                &LedgerSnapshot::genesis("a:main"),
                0,
                &NoOverlay,
                1,
                &Tracker::disabled(),
                vec![secret("key1")],
            )
            .await
            .expect("filter");
        assert_eq!(from_a.len(), 1, "ledger A's key1 is not a Key");

        let from_b = enforcer
            .filter_flakes_for_graph(
                &LedgerSnapshot::genesis("b:main"),
                0,
                &Flakes(vec![typed("key1", "Key")]),
                1,
                &Tracker::disabled(),
                vec![secret("key1")],
            )
            .await
            .expect("filter");
        assert!(from_b.is_empty(), "ledger B's key got past the deny");
    }

    /// The same collision through a grant rather than a deny, which needs no
    /// empty entry: under default-deny with only `Note` allowed, ledger A caches
    /// `shared` as a `Note`, and ledger B's `shared` — a `Key` — is skipped as
    /// already resolved and shown under A's classes.
    #[tokio::test]
    async fn a_class_grant_in_one_ledger_does_not_reach_another() {
        let stats = IndexStats {
            flakes: 4,
            size: 0,
            properties: None,
            classes: Some(vec![class_using_secret("Key"), class_using_secret("Note")]),
            graphs: None,
            historical_since_t: None,
        };
        let view = build_policy_set(
            vec![restriction(
                "notes",
                TargetMode::OnClass,
                PolicyValue::Allow,
                false,
                &[sid("Note")],
            )],
            Some(&stats),
            PolicyAction::View,
            None,
        );
        let wrapper = PolicyWrapper::new(view, PolicySet::new(), false, false, HashMap::new());
        let enforcer = QueryPolicyEnforcer::new(Arc::new(PolicyContext::new(wrapper, None)));

        let from_a = enforcer
            .filter_flakes_for_graph(
                &LedgerSnapshot::genesis("a:main"),
                0,
                &Flakes(vec![typed("shared", "Note")]),
                1,
                &Tracker::disabled(),
                vec![secret("shared")],
            )
            .await
            .expect("filter");
        assert_eq!(from_a.len(), 1, "ledger A's note is granted");

        let from_b = enforcer
            .filter_flakes_for_graph(
                &LedgerSnapshot::genesis("b:main"),
                0,
                &Flakes(vec![typed("shared", "Key")]),
                1,
                &Tracker::disabled(),
                vec![secret("shared")],
            )
            .await
            .expect("filter");
        assert!(from_b.is_empty(), "ledger B's key was shown as A's note");
    }
}
