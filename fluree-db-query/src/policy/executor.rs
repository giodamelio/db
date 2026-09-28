//! Policy query executor implementation
//!
//! Implements `PolicyQueryExecutor` using the query engine asynchronously.

use crate::binding::Binding;
use crate::context::ExecutionContext;
use crate::execute::build_where_operators_seeded;
use crate::ir::{GraphName, Pattern, Ref, Term};
use crate::var_registry::VarId;
use crate::var_registry::VarRegistry;
use fluree_db_core::{
    DatatypeConstraint, FlakeValue, GraphId, LedgerSnapshot, OverlayProvider, Sid,
};
use fluree_db_policy::{
    ClassScope, ConditionState, PolicyQuery, PolicyQueryExecutor, PolicyQueryFut,
    PolicyQueryLanguage, Result as PolicyResult, UNBOUND_IDENTITY_PREFIX,
};
use fluree_vocab::namespaces::{EMPTY, RDF, XSD};
use fluree_vocab::{rdf_names, xsd_names};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Policy query executor that runs queries against a database
///
/// This executor converts `PolicyQuery` to the query engine's IR and
/// executes with a root context (no policy filtering).
pub struct QueryPolicyExecutor<'a> {
    /// The database snapshot to query
    pub snapshot: &'a LedgerSnapshot,
    /// Optional overlay provider (for staged flakes)
    pub overlay: Option<&'a dyn OverlayProvider>,
    /// Target transaction time
    pub to_t: i64,
    /// Graph ID for range queries (default: 0 = default graph)
    pub g_id: GraphId,
    /// Post-state overlay for `f:queryState f:postState` conditions:
    /// committed state plus the transaction's staged flakes. Absent on read
    /// paths (no transaction in flight — post-state conditions then evaluate
    /// against current state, which pre and post coincide with).
    pub post_overlay: Option<&'a dyn OverlayProvider>,
    /// Target transaction time for the post-state overlay (the staged t)
    pub post_to_t: i64,
    /// Snapshot to pair with `post_overlay`. A staged view can carry a range
    /// provider whose dictionaries cover the staged flakes; post-state
    /// conditions must read through it, or the binary lane cannot translate
    /// the very subjects the transaction is introducing. Falls back to
    /// `snapshot` when absent.
    pub post_snapshot: Option<&'a LedgerSnapshot>,
    /// What this executor has already worked out about its conditions.
    cache: Arc<ConditionCache>,
    /// The subjects the caller is about to ask about, so that a [`Probe`]
    /// resolves them in one lookup rather than one per call.
    subjects: Vec<Sid>,
}

/// Where a cached answer was read: a ledger at a `t`, and a graph of it.
type ConditionScope = (ClassScope, GraphId);

/// Each subject's IRI values of one predicate.
type SubjectRefs = HashMap<Sid, Vec<Sid>>;

/// Rows of a [`Hoisted`] part, by the state read and the row that seeded it.
type HoistedRows = Vec<(ConditionState, Vec<Binding>, Arc<Vec<Vec<Binding>>>)>;

/// What executors have worked out about their conditions, kept across them.
///
/// An executor is built per filter call, and a join probing one subject at a
/// time makes a filter call per row, so anything kept only as long as an
/// executor was worked out again for every row. The enforcer holds one of
/// these for as long as its policy view lives and hands it to each executor
/// it builds. Every entry is keyed by the ledger, `t` and graph it was read
/// from, like the class cache, so executors reading different snapshots share
/// nothing. An executor reading a transaction's staged state keeps a private
/// one instead, since what is staged is not identified by a `t`.
#[derive(Default)]
pub struct ConditionCache {
    /// SPARQL conditions already lowered, by where and source.
    ///
    /// A condition is asked once per flake it judges, and lowering it was a
    /// quarter of what each ask cost. What lowering produces depends on the
    /// source and on the snapshot IRIs are encoded against — never on the
    /// bindings, which are seeded afterwards as a VALUES row.
    prepared: Mutex<HashMap<(ConditionScope, String), Arc<PreparedSparql>>>,
    /// Each subject's IRI values of a predicate, by where, whether the
    /// post-state was read, and the predicate — what a [`Probe`] reads in
    /// place of running its condition.
    refs: Mutex<HashMap<(ConditionScope, bool, Sid), SubjectRefs>>,
}

impl std::fmt::Debug for ConditionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConditionCache").finish_non_exhaustive()
    }
}

/// A lowered SPARQL condition and the variables its bindings seed.
struct PreparedSparql {
    vars: VarRegistry,
    /// What runs on every call: the whole condition, or what is left of it
    /// once [`Hoisted`] has been taken out.
    patterns: Vec<Pattern>,
    /// The binding names it was prepared for, sorted. A call with any other
    /// set lowers afresh rather than seeding the wrong variables.
    names: Vec<String>,
    /// For each VALUES column, its variable and the binding that fills it.
    /// Two names can register as one variable (`?$this` and `?this`); the
    /// first in sorted order fills it, as it did before preparation.
    columns: Vec<(VarId, usize)>,
    hoisted: Option<Hoisted>,
    probe: Option<Probe>,
}

/// A condition whose per-flake part is one triple, `$this <predicate> O`,
/// answered without running it: it holds when any IRI `$this` has as a
/// `predicate` value is one `O` allows.
///
/// That is what a scoped key comes to once its own projects are hoisted —
/// `$this ex:project ?p` with `?p` from the key's rows — and it is the same
/// question the class cache answers for `f:onClass`, so it is answered the same
/// way: one batched index lookup per filter batch, kept per subject. Running
/// the condition per flake paid a plan and a scan for each; this pays a hash
/// lookup. Only IRIs are looked up, which is exact, since `O` only ever allows
/// IRIs and a literal never equals one.
struct Probe {
    predicate: Sid,
    object: Allowed,
}

/// What a [`Probe`]'s object may be.
enum Allowed {
    /// A row of the hoisted part: its one carried column.
    Carried,
    /// A binding that is the same for every flake, such as `$identity`, by its
    /// index in [`PreparedSparql::names`].
    Binding(usize),
    /// A fixed IRI.
    Iri(Sid),
}

impl Probe {
    /// The probe `rest` comes to, or `None` when it is anything but one plain
    /// triple on `$this` whose object is a variable the hoisted rows or a
    /// constant binding fill, or an IRI.
    fn of(
        rest: &[Pattern],
        hoisted: Option<&Hoisted>,
        columns: &[(VarId, usize)],
        names: &[String],
    ) -> Option<Probe> {
        let [Pattern::Triple(triple)] = rest else {
            return None;
        };
        if triple.dtc.is_some() {
            return None;
        }
        let column_of = |var: VarId| columns.iter().find(|(v, _)| *v == var).map(|&(_, n)| n);
        let this = columns
            .iter()
            .find(|&&(_, name)| names[name] == "?$this")
            .map(|&(var, _)| var)?;
        if triple.s != Ref::Var(this) {
            return None;
        }
        let Ref::Sid(predicate) = &triple.p else {
            return None;
        };
        let carried = hoisted.map_or(&[][..], |h| h.carried.as_slice());
        let object = match &triple.o {
            Term::Var(var) if carried == [*var] => Allowed::Carried,
            Term::Var(var) if *var != this && carried.is_empty() => {
                let name = column_of(*var)?;
                if PER_FLAKE.contains(&names[name].as_str()) {
                    return None;
                }
                Allowed::Binding(name)
            }
            Term::Sid(iri) if carried.is_empty() => Allowed::Iri(iri.clone()),
            _ => return None,
        };
        Some(Probe {
            predicate: predicate.clone(),
            object,
        })
    }
}

/// The binding names whose values change from one call to the next: the flake
/// being judged. Every other binding — the identity, the policy values — is
/// the same for every flake a context judges.
const PER_FLAKE: [&str; 3] = ["?$this", "?$value", "?$op"];

/// The part of a condition that mentions no per-flake variable, run once and
/// joined into each call as rows.
///
/// A scoped key's `GRAPH <sys> { $identity ex:scopeProject ?p }` answers the
/// same for every flake, and asking it again per flake was a third of what a
/// condition cost. Only a plain conjunction is split — triples, `GRAPH` blocks
/// of triples with a fixed name, and filters, which stay behind — because
/// there joining a part's rows back in is the same as running it in place.
/// `OPTIONAL`, `MINUS`, `UNION`, `BIND` and the rest depend on what is joined
/// before them, so a condition holding any of them is not split at all.
struct Hoisted {
    patterns: Vec<Pattern>,
    /// The columns seeding it: those of [`PreparedSparql::columns`] that are
    /// not per-flake.
    columns: Vec<(VarId, usize)>,
    /// What it binds that the rest reads.
    carried: Vec<VarId>,
    /// Rows already computed, by the state read and the seeding row.
    rows: Mutex<HoistedRows>,
}

impl Hoisted {
    /// Split `patterns` into the part to hoist and the rest, or `None` when
    /// the condition is not a plain conjunction or nothing in it can move.
    fn split(
        patterns: Vec<Pattern>,
        columns: &[(VarId, usize)],
        names: &[String],
    ) -> (Vec<Pattern>, Option<Hoisted>) {
        let per_flake: Vec<VarId> = columns
            .iter()
            .filter(|&&(_, name)| PER_FLAKE.contains(&names[name].as_str()))
            .map(|&(var, _)| var)
            .collect();
        let constant: Vec<(VarId, usize)> = columns
            .iter()
            .filter(|(var, _)| !per_flake.contains(var))
            .copied()
            .collect();

        let is_block = |pattern: &Pattern| match pattern {
            Pattern::Triple(_) => true,
            Pattern::Graph {
                name: GraphName::Iri(_),
                patterns,
            } => patterns.iter().all(|p| matches!(p, Pattern::Triple(_))),
            _ => false,
        };
        if !patterns
            .iter()
            .all(|p| is_block(p) || matches!(p, Pattern::Filter(_)))
        {
            return (patterns, None);
        }
        let (hoisted, rest): (Vec<Pattern>, Vec<Pattern>) =
            patterns.into_iter().partition(|pattern| {
                is_block(pattern)
                    && pattern
                        .referenced_vars()
                        .iter()
                        .all(|var| !per_flake.contains(var))
            });
        if hoisted.is_empty() {
            return (rest, None);
        }

        let read: Vec<VarId> = rest.iter().flat_map(Pattern::referenced_vars).collect();
        let mut carried: Vec<VarId> = hoisted
            .iter()
            .flat_map(Pattern::referenced_vars)
            .filter(|var| read.contains(var) && !constant.iter().any(|(c, _)| c == var))
            .collect();
        carried.sort();
        carried.dedup();

        (
            rest,
            Some(Hoisted {
                patterns: hoisted,
                columns: constant,
                carried,
                rows: Mutex::default(),
            }),
        )
    }
}

impl<'a> QueryPolicyExecutor<'a> {
    /// Create a new query executor for the default graph
    pub fn new(snapshot: &'a LedgerSnapshot) -> Self {
        Self {
            snapshot,
            overlay: None,
            to_t: snapshot.t,
            g_id: 0,
            post_overlay: None,
            post_to_t: snapshot.t,
            post_snapshot: None,
            cache: Arc::default(),
            subjects: Vec::new(),
        }
    }

    /// Create a query executor with overlay support for the default graph
    pub fn with_overlay(
        snapshot: &'a LedgerSnapshot,
        overlay: &'a dyn OverlayProvider,
        to_t: i64,
    ) -> Self {
        Self {
            snapshot,
            overlay: Some(overlay),
            to_t,
            g_id: 0,
            post_overlay: None,
            post_to_t: to_t,
            post_snapshot: None,
            cache: Arc::default(),
            subjects: Vec::new(),
        }
    }

    /// Set the graph ID for range queries.
    ///
    /// Policy queries will execute against this graph instead of the default graph.
    pub fn with_graph_id(mut self, g_id: GraphId) -> Self {
        self.g_id = g_id;
        self
    }

    /// Attach a post-state overlay (committed + staged flakes) for
    /// `f:queryState f:postState` conditions, with the staged t.
    ///
    /// The executor keeps its own [`ConditionCache`] from then on, whatever
    /// [`Self::with_cache`] handed it: staged flakes are not identified by a
    /// `t`, so nothing read through them may be shared.
    pub fn with_post_state(mut self, overlay: &'a dyn OverlayProvider, to_t: i64) -> Self {
        self.post_overlay = Some(overlay);
        self.post_to_t = to_t;
        self.cache = Arc::default();
        self
    }

    /// Keep what this executor works out in `cache`, and use what is already
    /// there. Ignored for an executor reading a staged post-state.
    pub fn with_cache(mut self, cache: Arc<ConditionCache>) -> Self {
        if self.post_overlay.is_none() {
            self.cache = cache;
        }
        self
    }

    /// Pair the post-state overlay with the snapshot it should be read
    /// through (the staged view's, once its dictionaries cover the staged
    /// flakes).
    pub fn with_post_state_snapshot(mut self, snapshot: &'a LedgerSnapshot) -> Self {
        self.post_snapshot = Some(snapshot);
        self
    }

    /// Name the subjects this executor is about to be asked about, so a
    /// condition answered by index lookup resolves them all in one.
    pub fn with_subjects(mut self, subjects: Vec<Sid>) -> Self {
        self.subjects = subjects;
        self
    }
}

impl PolicyQueryExecutor for QueryPolicyExecutor<'_> {
    fn evaluate_policy_query<'b>(
        &'b self,
        query: &'b PolicyQuery,
        bindings: &'b HashMap<String, FlakeValue>,
    ) -> PolicyQueryFut<'b> {
        Box::pin(self.evaluate_async(query, bindings))
    }
}

/// Map a JSON-LD special-variable name to its SPARQL registry name.
///
/// JSON-LD policy bindings use `?$this` / `?$identity`; SPARQL has no `$`
/// in variable names — the SHACL-SPARQL-style `$this` lexes as sigil `$` +
/// name `this` and registers as `?this`. So `?$this` maps to `?this`;
/// names without the `$` marker pass through unchanged.
fn sparql_var_name(json_ld_name: &str) -> String {
    match json_ld_name.strip_prefix("?$") {
        Some(rest) => format!("?{rest}"),
        None => json_ld_name.to_string(),
    }
}

/// True when a binding value is the never-match unbound-identity marker.
fn is_unbound_marker(value: &FlakeValue) -> bool {
    matches!(value, FlakeValue::Ref(sid) if sid.name.starts_with(UNBOUND_IDENTITY_PREFIX))
}

/// Object IRI seeded for `?$value` when the flake's object has no faithful
/// binding representation (`Vector`, `GeoPoint`, `Null`). Absent from real
/// data, so a positional `?$value` condition finds no match. See
/// [`binding_for_value`] for why this must not be UNDEF.
const NON_REPRESENTABLE_VALUE_IRI: &str = "urn:fluree:policy:non-representable-value";

/// Convert a binding value to a seeded `Binding` for a policy VALUES row.
///
/// Every special variable seeds a CONCRETE binding — never `Binding::Unbound`.
/// A positional VALUES pattern treats an unbound variable as "matches
/// anything" (VALUES-UNDEF is compatible with every row), so seeding UNDEF for
/// a never-match marker would make a positional condition such as
/// `$identity <ex:user> $this` *vanish* and hold for every row — fail-OPEN.
/// Seeding a concrete never-match value instead fails closed positionally and
/// keeps `FILTER` equality false.
///
/// - Refs — including the never-match unbound-identity marker, whose sentinel
///   IRI is absent from data — seed as `Binding::Sid`.
/// - Literals with a faithful default datatype seed as `Binding::Lit`.
/// - Literals whose kind has no faithful datatype (`Vector`, `GeoPoint`,
///   `Null`) seed the [`NON_REPRESENTABLE_VALUE_IRI`] ref sentinel (fail-closed).
fn binding_for_value(value: &FlakeValue) -> Binding {
    match value {
        FlakeValue::Ref(sid) => Binding::Sid {
            sid: sid.clone(),
            t: None,
            op: None,
        },
        literal => match default_literal_datatype(literal) {
            Some(dt_sid) => Binding::Lit {
                val: literal.clone(),
                dtc: DatatypeConstraint::Explicit(dt_sid),
                t: None,
                op: None,
                p_id: None,
            },
            None => Binding::Sid {
                sid: Sid::new(EMPTY, NON_REPRESENTABLE_VALUE_IRI),
                t: None,
                op: None,
            },
        },
    }
}

/// Default XSD datatype Sid for a literal binding value, for seeding
/// VALUES rows (`Binding::Lit` equality includes the datatype). Mirrors the
/// datatypes the SPARQL literal lowering assigns, so seeded values compare
/// like written literals. Returns `None` only for `Vector` / `GeoPoint` /
/// `Null`, whose object value has no faithful literal datatype for seeding;
/// [`binding_for_value`] then seeds a never-match ref sentinel (fail-closed).
fn default_literal_datatype(value: &FlakeValue) -> Option<Sid> {
    // rdf:JSON lives in the RDF namespace, not XSD — handle before the
    // XSD-namespaced fallthrough below.
    if matches!(value, FlakeValue::Json(_)) {
        return Some(Sid::new(RDF, rdf_names::JSON));
    }
    let name = match value {
        FlakeValue::String(_) => xsd_names::STRING,
        FlakeValue::Boolean(_) => xsd_names::BOOLEAN,
        FlakeValue::Long(_) | FlakeValue::BigInt(_) => xsd_names::INTEGER,
        FlakeValue::Double(_) => xsd_names::DOUBLE,
        FlakeValue::Decimal(_) => xsd_names::DECIMAL,
        FlakeValue::DateTime(_) => xsd_names::DATE_TIME,
        FlakeValue::Date(_) => xsd_names::DATE,
        FlakeValue::Time(_) => xsd_names::TIME,
        FlakeValue::GYear(_) => xsd_names::G_YEAR,
        FlakeValue::GYearMonth(_) => xsd_names::G_YEAR_MONTH,
        FlakeValue::GMonth(_) => xsd_names::G_MONTH,
        FlakeValue::GDay(_) => xsd_names::G_DAY,
        FlakeValue::GMonthDay(_) => xsd_names::G_MONTH_DAY,
        FlakeValue::Duration(_) => xsd_names::DURATION,
        FlakeValue::DayTimeDuration(_) => xsd_names::DAY_TIME_DURATION,
        FlakeValue::YearMonthDuration(_) => xsd_names::YEAR_MONTH_DURATION,
        _ => return None,
    };
    Some(Sid::new(XSD, name))
}

impl<'a> QueryPolicyExecutor<'a> {
    /// Async implementation of policy query evaluation
    async fn evaluate_async(
        &self,
        query: &PolicyQuery,
        bindings: &HashMap<String, FlakeValue>,
    ) -> PolicyResult<bool> {
        let state = query.state;
        match query.language {
            PolicyQueryLanguage::JsonLd => {
                self.evaluate_jsonld(&query.source, bindings, state).await
            }
            PolicyQueryLanguage::Sparql => {
                self.evaluate_sparql(&query.source, bindings, state).await
            }
            PolicyQueryLanguage::Cypher => {
                self.evaluate_cypher(&query.source, bindings, state).await
            }
            // `PolicyQueryLanguage` is non_exhaustive; an unknown language
            // fails closed (error → deny), never open.
            other => Err(fluree_db_policy::PolicyError::QueryExecution {
                message: format!("Unsupported policy query language: {}", other.as_str()),
            }),
        }
    }

    /// Evaluate a Cypher policy query via the registered lowering hook.
    ///
    /// Bindings become Cypher **parameters** (`?$this` → `$this`): refs
    /// carry IRI strings, literals (`$value`) carry their scalar values —
    /// substituted into the AST before lowering, no variable seeding. An
    /// unbound identity substitutes as `null`, which never compares equal,
    /// so identity-referencing conditions cannot hold. Fails closed when no
    /// Cypher support is registered.
    async fn evaluate_cypher(
        &self,
        source: &str,
        bindings: &HashMap<String, FlakeValue>,
        state: ConditionState,
    ) -> PolicyResult<bool> {
        let Some(support) = crate::lang_support::cypher_support() else {
            return Err(fluree_db_policy::PolicyError::QueryExecution {
                message: "Cypher policy support is not registered in this process".to_string(),
            });
        };

        let mut params = serde_json::Map::new();
        for (name, value) in bindings {
            // "?$this" → parameter name "this"; custom "?myVar" → "myVar".
            let key = name
                .strip_prefix("?$")
                .or_else(|| name.strip_prefix('?'))
                .unwrap_or(name)
                .to_string();
            let json = if is_unbound_marker(value) {
                serde_json::Value::Null
            } else {
                match value {
                    FlakeValue::Ref(sid) => {
                        let iri = self
                            .snapshot
                            .decode_sid(sid)
                            .unwrap_or_else(|| sid.name.to_string());
                        serde_json::Value::String(iri)
                    }
                    FlakeValue::String(s) => serde_json::Value::String(s.clone()),
                    FlakeValue::Long(l) => serde_json::Value::from(*l),
                    FlakeValue::Double(d) => serde_json::Value::from(*d),
                    FlakeValue::Boolean(b) => serde_json::Value::from(*b),
                    // No faithful Cypher parameter representation: null never
                    // compares equal, so conditions on it fail closed.
                    _ => serde_json::Value::Null,
                }
            };
            params.insert(key, json);
        }

        let mut vars = VarRegistry::new();
        let patterns = (support.lower_policy_query)(source, self.snapshot, &mut vars, &params)
            .map_err(|e| fluree_db_policy::PolicyError::QueryExecution {
                message: format!("Failed to lower Cypher policy query: {e}"),
            })?;

        self.run_existence_check(&vars, &patterns, state).await
    }

    /// Evaluate a JSON-LD policy query (the historical default).
    async fn evaluate_jsonld(
        &self,
        source: &str,
        bindings: &HashMap<String, FlakeValue>,
        state: ConditionState,
    ) -> PolicyResult<bool> {
        // Parse and lower the policy's f:query using the main query parser/IR.
        //
        // We intentionally do NOT implement a bespoke parser here; this ensures full
        // feature parity (e.g., FILTER patterns) and avoids divergence.
        //
        // Policy queries behave like existence checks, with:
        // - select forced to ["?$this"]
        // - limit forced to 1
        // - VALUES injected into WHERE for special variables (?$this, ?$identity, etc.)
        let mut query_json: serde_json::Value = serde_json::from_str(source).map_err(|e| {
            fluree_db_policy::PolicyError::QueryExecution {
                message: format!("Invalid policy query JSON: {e}"),
            }
        })?;

        let obj = query_json.as_object_mut().ok_or_else(|| {
            fluree_db_policy::PolicyError::QueryExecution {
                message: "Policy query must be a JSON object".to_string(),
            }
        })?;

        // Accept the query language's `ask` form as the preferred spelling
        // of a condition — its value IS a where-pattern, so it normalizes
        // onto the same existence-check path as the legacy `{"where": ...}`
        // form. Both keys together is ambiguous and fails closed (deny).
        if let Some(ask) = obj.remove("ask") {
            if obj.contains_key("where") {
                return Err(fluree_db_policy::PolicyError::QueryExecution {
                    message: "Policy query cannot carry both 'ask' and 'where'".to_string(),
                });
            }
            obj.insert("where".to_string(), ask);
        }

        // Force select + limit for policy queries
        obj.insert(
            "select".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("?$this".to_string())]),
        );
        obj.insert("limit".to_string(), serde_json::Value::from(1));

        // Build VALUES clause JSON for the ref-valued special variables
        // (?$this, ?$identity, custom policy values). Inject VALUES into
        // WHERE BEFORE parsing — this ensures even empty queries (no WHERE)
        // work, the VALUES provides the pattern.
        //
        // ?$value / ?$op are NOT JSON-injected: the flake's object can be a
        // ref whose decoded IRI doesn't round-trip through the strict
        // compact-IRI parser (e.g. ledger-scoped `ledger:...` IRIs), and a
        // literal can carry a datatype JSON can't express. They seed as
        // direct Bindings after parsing (same mechanism as the SPARQL path).
        //
        // Format: ["values", [["?$this", "?$identity", ...], [[iri1, iri2, ...]]]]
        let mut var_names: Vec<String> = bindings
            .keys()
            .filter(|name| *name != "?$value" && *name != "?$op")
            .cloned()
            .collect();
        var_names.sort();

        // Build VALUES row with IRIs for each variable.
        //
        // The unbound-identity marker is a ref carrying its never-match
        // sentinel IRI: emit it as an `{"@id": ...}` just like any other ref
        // (NOT as null/UNDEF — a positional VALUES treats UNDEF as
        // "matches anything", which would make an `$identity`-positioned
        // condition hold for every row; see `binding_for_value`). Its sentinel
        // IRI is absent from data, so the condition finds no match.
        let values_row: Vec<serde_json::Value> = var_names
            .iter()
            .map(|name| {
                let value = bindings.get(name).expect("binding value exists");
                match value {
                    FlakeValue::Ref(sid) => {
                        // Decode SID to IRI for JSON representation
                        let iri = self
                            .snapshot
                            .decode_sid(sid)
                            .unwrap_or_else(|| sid.name.to_string());
                        serde_json::json!({"@id": iri})
                    }
                    FlakeValue::String(s) => serde_json::Value::String(s.clone()),
                    FlakeValue::Long(l) => serde_json::Value::from(*l),
                    FlakeValue::Double(d) => serde_json::Value::from(*d),
                    FlakeValue::Boolean(b) => serde_json::Value::from(*b),
                    // No faithful JSON representation → seed the never-match
                    // ref sentinel (fail-closed), never null/UNDEF.
                    _ => serde_json::json!({"@id": NON_REPRESENTABLE_VALUE_IRI}),
                }
            })
            .collect();

        let values_clause = serde_json::json!(["values", [var_names.clone(), [values_row]]]);

        // Inject VALUES into WHERE clause (or create WHERE if missing)
        let where_clause = obj.get_mut("where");
        match where_clause {
            Some(serde_json::Value::Array(arr)) => {
                // WHERE is an array - prepend VALUES
                arr.insert(0, values_clause);
            }
            Some(serde_json::Value::Object(_)) => {
                // WHERE is an object (single pattern) - wrap in array with VALUES
                let existing = obj.remove("where").unwrap();
                obj.insert(
                    "where".to_string(),
                    serde_json::json!([values_clause, existing]),
                );
            }
            Some(_) | None => {
                // No WHERE or invalid - create with just VALUES
                // This handles empty queries like {}
                obj.insert("where".to_string(), serde_json::json!([values_clause]));
            }
        }

        // Create a variable registry for this query execution
        let mut vars = VarRegistry::new();

        // Pre-register special variables so they are present even if not referenced.
        // This matches the "always ground" behavior.
        for var_name in &var_names {
            vars.get_or_insert(var_name);
        }

        let parsed = crate::parse::parse_query(&query_json, self.snapshot, &mut vars, None)
            .map_err(|e| fluree_db_policy::PolicyError::QueryExecution {
                message: format!("Failed to parse policy query: {e}"),
            })?;

        // Seed ?$value / ?$op as direct Bindings (no JSON round-trip).
        let mut patterns = parsed.patterns;
        let mut extra_vars = Vec::new();
        let mut extra_row = Vec::new();
        for name in ["?$value", "?$op"] {
            let Some(value) = bindings.get(name) else {
                continue;
            };
            extra_vars.push(vars.get_or_insert(name));
            extra_row.push(binding_for_value(value));
        }
        if !extra_vars.is_empty() {
            patterns.insert(
                0,
                Pattern::Values {
                    vars: extra_vars,
                    rows: vec![extra_row],
                },
            );
        }

        self.run_existence_check(&vars, &patterns, state).await
    }

    /// Evaluate a SPARQL policy query (`f:query` stored with the `f:sparql`
    /// datatype).
    ///
    /// SPARQL support is provided by a higher layer via
    /// [`crate::lang_support::register_sparql_support`]; if it is absent this
    /// fails closed (error → deny), never open.
    async fn evaluate_sparql(
        &self,
        source: &str,
        bindings: &HashMap<String, FlakeValue>,
        state: ConditionState,
    ) -> PolicyResult<bool> {
        let mut names: Vec<&String> = bindings.keys().collect();
        names.sort();
        let prepared = self.prepared_sparql(source, &names)?;

        let row_of = |columns: &[(VarId, usize)]| -> Vec<Binding> {
            columns
                .iter()
                .map(|&(_, name)| binding_for_value(&bindings[names[name]]))
                .collect()
        };

        let hoisted_rows = match &prepared.hoisted {
            Some(hoisted) => {
                let rows = self
                    .hoisted_rows(&prepared.vars, hoisted, row_of(&hoisted.columns), state)
                    .await?;
                if rows.is_empty() {
                    return Ok(false);
                }
                Some(rows)
            }
            None => None,
        };

        if let (Some(probe), Some(FlakeValue::Ref(subject))) =
            (&prepared.probe, bindings.get("?$this"))
        {
            let refs = self.subject_refs(state, &probe.predicate, subject).await?;
            let allowed = |iri: &Sid| match &probe.object {
                Allowed::Carried => hoisted_rows.as_deref().is_some_and(|rows| {
                    rows.iter()
                        .any(|row| matches!(&row[0], Binding::Sid { sid, .. } if sid == iri))
                }),
                Allowed::Binding(name) => {
                    matches!(&bindings[names[*name]], FlakeValue::Ref(sid) if sid == iri)
                }
                Allowed::Iri(sid) => sid == iri,
            };
            return Ok(refs.iter().any(allowed));
        }

        // Seed special variables with a VALUES pattern, mirroring the JSON-LD
        // path's injected VALUES clause.
        let mut patterns = Vec::with_capacity(prepared.patterns.len() + 2);
        patterns.push(Pattern::Values {
            vars: prepared.columns.iter().map(|&(var, _)| var).collect(),
            rows: vec![row_of(&prepared.columns)],
        });
        if let (Some(hoisted), Some(rows)) = (&prepared.hoisted, &hoisted_rows) {
            if !hoisted.carried.is_empty() {
                patterns.push(Pattern::Values {
                    vars: hoisted.carried.clone(),
                    rows: rows.to_vec(),
                });
            }
        }
        patterns.extend(prepared.patterns.iter().cloned());

        self.run_existence_check(&prepared.vars, &patterns, state)
            .await
    }

    /// Every row of `hoisted` for this seeding, projected to what the rest of
    /// the condition reads — computed on the first call that seeds it this way
    /// and kept for the executor's life.
    async fn hoisted_rows(
        &self,
        vars: &VarRegistry,
        hoisted: &Hoisted,
        seed: Vec<Binding>,
        state: ConditionState,
    ) -> PolicyResult<Arc<Vec<Vec<Binding>>>> {
        let poisoned = || fluree_db_policy::PolicyError::QueryExecution {
            message: "hoisted condition cache lock poisoned".to_string(),
        };
        let cached = hoisted
            .rows
            .lock()
            .map_err(|_| poisoned())?
            .iter()
            .find(|(s, row, _)| *s == state && *row == seed)
            .map(|(_, _, rows)| Arc::clone(rows));
        if let Some(rows) = cached {
            return Ok(rows);
        }

        let mut patterns = Vec::with_capacity(hoisted.patterns.len() + 1);
        patterns.push(Pattern::Values {
            vars: hoisted.columns.iter().map(|&(var, _)| var).collect(),
            rows: vec![seed.clone()],
        });
        patterns.extend(hoisted.patterns.iter().cloned());

        // Eager, so that what is joined back in is a `Sid` or `Lit` and
        // compares like the rest of the condition's own bindings.
        let mut ctx = self.context(vars, state);
        ctx.eager_materialization = true;
        let mut operator = self.plan(&patterns)?;
        let query_error =
            |e: crate::error::QueryError| fluree_db_policy::PolicyError::QueryExecution {
                message: e.to_string(),
            };
        operator.open(&ctx).await.map_err(query_error)?;
        let mut rows: Vec<Vec<Binding>> = Vec::new();
        loop {
            let batch = match operator.next_batch(&ctx).await {
                Ok(Some(batch)) => batch,
                Ok(None) => break,
                Err(e) => {
                    operator.close();
                    return Err(query_error(e));
                }
            };
            for index in 0..batch.len() {
                let row: Vec<Binding> = hoisted
                    .carried
                    .iter()
                    .map(|&var| batch.get(index, var).cloned().unwrap_or(Binding::Unbound))
                    .collect();
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
        }
        operator.close();

        let rows = Arc::new(rows);
        hoisted
            .rows
            .lock()
            .map_err(|_| poisoned())?
            .push((state, seed, Arc::clone(&rows)));
        Ok(rows)
    }

    /// `source` lowered against this executor's snapshot, with the variables
    /// the bindings `names` seed — from the cache when it was lowered before
    /// for the same names.
    fn prepared_sparql(
        &self,
        source: &str,
        names: &[&String],
    ) -> PolicyResult<Arc<PreparedSparql>> {
        let key = (self.scope(), source.to_string());
        let cached = self
            .cache
            .prepared
            .lock()
            .map_err(|_| fluree_db_policy::PolicyError::QueryExecution {
                message: "SPARQL condition cache lock poisoned".to_string(),
            })?
            .get(&key)
            .cloned();
        if let Some(prepared) = cached.filter(|p| p.names.iter().eq(names.iter().copied())) {
            return Ok(prepared);
        }

        let support = crate::lang_support::sparql_support().ok_or_else(|| {
            fluree_db_policy::PolicyError::QueryExecution {
                message: "SPARQL policy support is not registered in this process; \
                          cannot evaluate f:sparql policy query"
                    .to_string(),
            }
        })?;

        let mut vars = VarRegistry::new();
        let patterns =
            (support.lower_policy_query)(source, self.snapshot, &mut vars).map_err(|e| {
                fluree_db_policy::PolicyError::QueryExecution {
                    message: format!("Failed to parse SPARQL policy query: {e}"),
                }
            })?;

        // Binding keys arrive in JSON-LD form (`?$this`); the SPARQL query
        // references them as `$this`/`?this`, registered as `?this`.
        let mut columns: Vec<(VarId, usize)> = Vec::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            let var = vars.get_or_insert(&sparql_var_name(name));
            if !columns.iter().any(|&(seen, _)| seen == var) {
                columns.push((var, index));
            }
        }

        let names: Vec<String> = names.iter().map(|name| (*name).clone()).collect();
        let (patterns, hoisted) = Hoisted::split(patterns, &columns, &names);
        let probe = Probe::of(&patterns, hoisted.as_ref(), &columns, &names);
        let prepared = Arc::new(PreparedSparql {
            vars,
            patterns,
            names,
            columns,
            hoisted,
            probe,
        });
        self.cache
            .prepared
            .lock()
            .map_err(|_| fluree_db_policy::PolicyError::QueryExecution {
                message: "SPARQL condition cache lock poisoned".to_string(),
            })?
            .insert(key, Arc::clone(&prepared));
        Ok(prepared)
    }

    /// The snapshot and graph this executor reads, as its cache entries are
    /// keyed.
    fn scope(&self) -> ConditionScope {
        (
            ClassScope::new(&self.snapshot.ledger_id, self.to_t),
            self.g_id,
        )
    }

    /// Execute WHERE patterns with a root (policy-free) context and report
    /// whether any solution exists.
    async fn run_existence_check(
        &self,
        vars: &VarRegistry,
        patterns: &[Pattern],
        state: ConditionState,
    ) -> PolicyResult<bool> {
        let ctx = self.context(vars, state);
        let mut operator = self.plan(patterns)?;

        // Execute and check if there's at least one result (existence check)
        operator
            .open(&ctx)
            .await
            .map_err(|e| fluree_db_policy::PolicyError::QueryExecution {
                message: e.to_string(),
            })?;

        let has_results = match operator.next_batch(&ctx).await {
            Ok(Some(batch)) => !batch.is_empty(),
            Ok(None) => false,
            Err(e) => {
                operator.close();
                return Err(fluree_db_policy::PolicyError::QueryExecution {
                    message: e.to_string(),
                });
            }
        };

        operator.close();

        Ok(has_results)
    }

    /// A root (policy-free) context reading the state a condition asks for.
    fn context<'c>(&'c self, vars: &'c VarRegistry, state: ConditionState) -> ExecutionContext<'c> {
        let (snapshot, overlay, to_t) = self.state_view(state);

        // Create the execution context WITHOUT policy (root context)
        // This is critical - policy queries must not be filtered by policy
        if let Some(overlay) = overlay {
            ExecutionContext::with_time_and_overlay(snapshot, vars, to_t, None, overlay)
                .with_graph_id(self.g_id)
        } else {
            ExecutionContext::with_time(snapshot, vars, to_t, None).with_graph_id(self.g_id)
        }
    }

    /// The snapshot, overlay and `t` a condition reads.
    ///
    /// Per-condition state selection: `f:postState` reads through the staged
    /// overlay when one is attached; otherwise (read paths, no transaction in
    /// flight) pre and post coincide with current state.
    fn state_view(
        &self,
        state: ConditionState,
    ) -> (&'a LedgerSnapshot, Option<&'a dyn OverlayProvider>, i64) {
        match state {
            ConditionState::Post => match self.post_overlay {
                Some(post) => (
                    self.post_snapshot.unwrap_or(self.snapshot),
                    Some(post),
                    self.post_to_t,
                ),
                None => (self.snapshot, self.overlay, self.to_t),
            },
            ConditionState::Pre => (self.snapshot, self.overlay, self.to_t),
        }
    }

    /// The IRIs `subject` has as `predicate` values in the state read. On a
    /// miss every subject named by [`Self::with_subjects`] that is not yet
    /// known is resolved with it, in one lookup.
    async fn subject_refs(
        &self,
        state: ConditionState,
        predicate: &Sid,
        subject: &Sid,
    ) -> PolicyResult<Vec<Sid>> {
        let poisoned = || fluree_db_policy::PolicyError::QueryExecution {
            message: "condition lookup cache lock poisoned".to_string(),
        };
        let key = (
            self.scope(),
            state == ConditionState::Post,
            predicate.clone(),
        );
        let missing: Vec<Sid> = {
            let refs = self.cache.refs.lock().map_err(|_| poisoned())?;
            let known = refs.get(&key);
            if let Some(found) = known.and_then(|known| known.get(subject)) {
                return Ok(found.clone());
            }
            let mut missing: Vec<Sid> = self
                .subjects
                .iter()
                .chain(std::iter::once(subject))
                .filter(|s| known.is_none_or(|known| !known.contains_key(*s)))
                .cloned()
                .collect();
            missing.sort();
            missing.dedup();
            missing
        };

        let (snapshot, overlay, to_t) = self.state_view(state);
        let no_overlay = fluree_db_core::NoOverlay;
        let db = fluree_db_core::GraphDbRef::new(
            snapshot,
            self.g_id,
            overlay.unwrap_or(&no_overlay),
            to_t,
        );
        let mut found = fluree_db_policy::lookup_subject_refs(&missing, predicate, db).await?;

        let mut refs = self.cache.refs.lock().map_err(|_| poisoned())?;
        let known = refs.entry(key).or_default();
        for s in missing {
            let values = found.remove(&s).unwrap_or_default();
            known.insert(s, values);
        }
        Ok(known.get(subject).cloned().unwrap_or_default())
    }

    /// Build the where clause operators (VALUES is part of the patterns).
    ///
    /// Root: policy queries always evaluate at the selected state's t for
    /// current state — they're access-control predicates, not history-range
    /// queries. Always plan as `Current`.
    fn plan(&self, patterns: &[Pattern]) -> PolicyResult<crate::operator::BoxedOperator> {
        build_where_operators_seeded(
            None,
            patterns,
            None,
            None,
            &crate::temporal_mode::PlanningContext::current(),
        )
        .map_err(|e| fluree_db_policy::PolicyError::QueryExecution {
            message: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use uuid::Uuid;

    /// The security invariant behind both PR review findings: a policy VALUES
    /// row must never seed `Binding::Unbound` for a special variable. A
    /// positional VALUES treats UNDEF as "matches anything", so an unbound
    /// `$identity` / `$value` would make a positional condition hold for every
    /// row (fail-OPEN). Every kind must seed a concrete never-match-or-exact
    /// binding.
    #[test]
    fn binding_for_value_never_unbound() {
        // Finding 1: the unbound-identity marker seeds a concrete ref, not UNDEF.
        let marker = FlakeValue::Ref(Sid::new(
            EMPTY,
            format!("{UNBOUND_IDENTITY_PREFIX}{}", Uuid::nil()),
        ));
        assert!(
            matches!(binding_for_value(&marker), Binding::Sid { .. }),
            "unbound-identity marker must seed a never-match Sid, not UNDEF"
        );

        // Finding 2: kinds with no faithful datatype seed the ref sentinel.
        for value in [
            FlakeValue::Vector(Arc::from([1.0_f64, 2.0].as_slice())),
            FlakeValue::Null,
        ] {
            match binding_for_value(&value) {
                Binding::Sid { sid, .. } => {
                    assert_eq!(sid.name.as_ref(), NON_REPRESENTABLE_VALUE_IRI);
                }
                other => panic!("non-faithful {value:?} must seed the sentinel, got {other:?}"),
            }
        }

        // Faithful kinds seed concrete literals with the right datatype.
        for (value, ns, name) in [
            (FlakeValue::String("x".into()), XSD, xsd_names::STRING),
            (FlakeValue::Long(1), XSD, xsd_names::INTEGER),
            (FlakeValue::Json("{}".into()), RDF, rdf_names::JSON),
        ] {
            match binding_for_value(&value) {
                Binding::Lit { dtc, .. } => {
                    assert_eq!(
                        dtc.datatype(),
                        &Sid::new(ns, name),
                        "datatype for {value:?}"
                    );
                }
                other => panic!("faithful {value:?} must seed a Lit, got {other:?}"),
            }
        }

        // A regular ref seeds itself.
        let real = FlakeValue::Ref(Sid::new(XSD, "someSubject"));
        assert!(matches!(binding_for_value(&real), Binding::Sid { .. }));
    }

    use crate::ir::{Ref, Term, TriplePattern};
    use fluree_db_core::{Flake, IndexType};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LOWERINGS: AtomicUsize = AtomicUsize::new(0);

    fn triple(s: Ref, p: &str, o: Term) -> Pattern {
        Pattern::Triple(TriplePattern::new(s, Ref::Sid(Sid::new(100, p)), o))
    }

    /// Stands in for the SPARQL layer, counting how often it is asked to lower.
    /// The source names which of these the condition is:
    ///
    /// - `this-ok`: `$this <ok> ?o`
    /// - `scoped`: `$identity <scope> ?p . $this <project> ?p` — a scoped key
    /// - `owner`: `$this <owner> $identity`
    /// - `fixed`: `$this <project> <p1>`
    ///
    /// Any of the last three with `-whole` appended gains an
    /// `OPTIONAL { $this <note> ?n }`. That changes no answer, and makes the
    /// condition something other than a plain conjunction, so it runs whole —
    /// the engine's own answer to compare the split and probed ones against.
    fn lower_stub(
        source: &str,
        _snapshot: &LedgerSnapshot,
        vars: &mut VarRegistry,
    ) -> Result<Vec<Pattern>, String> {
        LOWERINGS.fetch_add(1, Ordering::SeqCst);
        let this = Ref::Var(vars.get_or_insert("?this"));
        match source {
            "this-ok" => Ok(vec![triple(
                this,
                "ok",
                Term::Var(vars.get_or_insert("?o")),
            )]),
            shape => {
                let (shape, whole) = match shape.strip_suffix("-whole") {
                    Some(shape) => (shape, true),
                    None => (shape, false),
                };
                let identity = Ref::Var(vars.get_or_insert("?identity"));
                let mut patterns = match shape {
                    "scoped" => {
                        let p = vars.get_or_insert("?p");
                        vec![
                            triple(identity, "scope", Term::Var(p)),
                            triple(this.clone(), "project", Term::Var(p)),
                        ]
                    }
                    "owner" => {
                        let Ref::Var(identity) = identity else {
                            unreachable!()
                        };
                        vec![triple(this.clone(), "owner", Term::Var(identity))]
                    }
                    "fixed" => vec![triple(
                        this.clone(),
                        "project",
                        Term::Sid(Sid::new(100, "p1")),
                    )],
                    other => return Err(format!("no stub for {other}")),
                };
                if whole {
                    patterns.push(Pattern::Optional(vec![triple(
                        this,
                        "note",
                        Term::Var(vars.get_or_insert("?n")),
                    )]));
                }
                Ok(patterns)
            }
        }
    }

    fn unused_rule_lowering(
        _source: &str,
        _snapshot: &LedgerSnapshot,
    ) -> Result<crate::lang_support::SparqlRuleParts, String> {
        Err("rules are not lowered here".to_string())
    }

    /// Flakes served in index order within the requested bounds.
    struct Flakes(Vec<Flake>);

    impl OverlayProvider for Flakes {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn epoch(&self) -> u64 {
            1
        }

        fn for_each_overlay_flake(
            &self,
            _g_id: GraphId,
            index: IndexType,
            first: Option<&Flake>,
            rhs: Option<&Flake>,
            leftmost: bool,
            to_t: i64,
            callback: &mut dyn FnMut(&Flake),
        ) {
            let mut flakes: Vec<&Flake> = self.0.iter().filter(|f| f.t <= to_t).collect();
            flakes.sort_by(|a, b| index.compare(a, b));
            for flake in flakes {
                let after_first = leftmost
                    || first.is_none_or(|f| index.compare(flake, f) == std::cmp::Ordering::Greater);
                let before_rhs =
                    rhs.is_none_or(|r| index.compare(flake, r) != std::cmp::Ordering::Greater);
                if after_first && before_rhs {
                    callback(flake);
                }
            }
        }
    }

    fn ok(subject: &str) -> Flake {
        Flake::new(
            Sid::new(100, subject),
            Sid::new(100, "ok"),
            FlakeValue::String("yes".into()),
            Sid::new(XSD, xsd_names::STRING),
            1,
            true,
            None,
        )
    }

    fn link(subject: &str, predicate: &str, object: &str) -> Flake {
        link_at(subject, predicate, object, 1)
    }

    fn link_at(subject: &str, predicate: &str, object: &str, t: i64) -> Flake {
        Flake::new(
            Sid::new(100, subject),
            Sid::new(100, predicate),
            FlakeValue::Ref(Sid::new(100, object)),
            Sid::new(fluree_vocab::namespaces::JSON_LD, "id"),
            t,
            true,
            None,
        )
    }

    fn this_is(subject: &str) -> HashMap<String, FlakeValue> {
        HashMap::from([(
            "?$this".to_string(),
            FlakeValue::Ref(Sid::new(100, subject)),
        )])
    }

    /// The bindings a read-side call carries: every special variable, as
    /// `build_policy_values_clause` builds them.
    fn judging(identity: &str, subject: &str) -> HashMap<String, FlakeValue> {
        HashMap::from([
            (
                "?$this".to_string(),
                FlakeValue::Ref(Sid::new(100, subject)),
            ),
            (
                "?$identity".to_string(),
                FlakeValue::Ref(Sid::new(100, identity)),
            ),
            ("?$value".to_string(), FlakeValue::String("v".into())),
            ("?$op".to_string(), FlakeValue::String("assert".into())),
        ])
    }

    async fn ask(
        executor: &QueryPolicyExecutor<'_>,
        query: &PolicyQuery,
        identity: &str,
        todo: &str,
    ) -> bool {
        executor
            .evaluate_policy_query(query, &judging(identity, todo))
            .await
            .expect("evaluate")
    }

    fn condition(source: &str) -> PolicyQuery {
        PolicyQuery {
            source: source.to_string(),
            language: PolicyQueryLanguage::Sparql,
            state: ConditionState::Pre,
        }
    }

    fn register_stub() {
        crate::lang_support::register_sparql_support(crate::lang_support::SparqlSupport {
            lower_policy_query: lower_stub,
            lower_rule: unused_rule_lowering,
        });
    }

    /// Two keys over four todos. `alice` is scoped to `p1` and `p2`, `carol`
    /// to `p3`, `bob` to nothing; `t3` is in two projects and `t4` in none,
    /// though it carries the literal `"p1"` as a project, which no IRI equals.
    /// `t1` is owned by `alice`, `t2` by `carol`, `t4` by `bob`.
    fn scoped_ledger() -> Flakes {
        Flakes(vec![
            link("alice", "scope", "p1"),
            link("alice", "scope", "p2"),
            link("carol", "scope", "p3"),
            link("t1", "project", "p1"),
            link("t2", "project", "p3"),
            link("t3", "project", "p2"),
            link("t3", "project", "p3"),
            Flake::new(
                Sid::new(100, "t4"),
                Sid::new(100, "project"),
                FlakeValue::String("p1".into()),
                Sid::new(XSD, xsd_names::STRING),
                1,
                true,
                None,
            ),
            link("t3", "note", "n1"),
            link("t1", "owner", "alice"),
            link("t2", "owner", "carol"),
            link("t4", "owner", "bob"),
        ])
    }

    /// Counts the overlay reads made through it — how many times the ledger
    /// was consulted, since these tests have no index.
    struct Counted<'o> {
        inner: &'o Flakes,
        reads: AtomicUsize,
    }

    impl OverlayProvider for Counted<'_> {
        fn as_any(&self) -> &dyn std::any::Any {
            self.inner.as_any()
        }

        fn epoch(&self) -> u64 {
            self.inner.epoch()
        }

        fn for_each_overlay_flake(
            &self,
            g_id: GraphId,
            index: IndexType,
            first: Option<&Flake>,
            rhs: Option<&Flake>,
            leftmost: bool,
            to_t: i64,
            callback: &mut dyn FnMut(&Flake),
        ) {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner
                .for_each_overlay_flake(g_id, index, first, rhs, leftmost, to_t, callback);
        }
    }

    /// What each key may see, asked of one executor in an order that changes
    /// key between calls, so a result kept for one identity answering for
    /// another shows up as a wrong row.
    async fn visible(source: &str) -> Vec<(&'static str, &'static str, bool)> {
        let snapshot = LedgerSnapshot::genesis("test:main");
        let overlay = scoped_ledger();
        let executor = QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1)
            .with_subjects(["t1", "t2", "t3", "t4"].map(|t| Sid::new(100, t)).to_vec());
        let query = condition(source);
        let mut seen = Vec::new();
        for identity in ["alice", "bob", "carol", "alice"] {
            for todo in ["t1", "t2", "t3", "t4"] {
                let allowed = executor
                    .evaluate_policy_query(&query, &judging(identity, todo))
                    .await
                    .expect("evaluate");
                seen.push((identity, todo, allowed));
            }
        }
        seen
    }

    fn expected_visibility() -> Vec<(&'static str, &'static str, bool)> {
        let mut rows = Vec::new();
        for identity in ["alice", "bob", "carol", "alice"] {
            for todo in ["t1", "t2", "t3", "t4"] {
                let allowed = matches!(
                    (identity, todo),
                    ("alice", "t1" | "t3") | ("carol", "t2" | "t3")
                );
                rows.push((identity, todo, allowed));
            }
        }
        rows
    }

    /// A condition is lowered once per executor and re-seeded per call, so
    /// the answer must follow each call's bindings rather than the first's,
    /// and a call binding a different set of names must not be seeded through
    /// variables prepared for another.
    #[tokio::test]
    async fn a_prepared_condition_answers_each_call_for_its_own_bindings() {
        register_stub();
        let snapshot = LedgerSnapshot::genesis("test:main");
        let overlay = Flakes(vec![ok("alice")]);
        let executor = QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1);
        let query = condition("this-ok");

        let mut answers = Vec::new();
        for subject in ["alice", "bob", "alice"] {
            answers.push(
                executor
                    .evaluate_policy_query(&query, &this_is(subject))
                    .await
                    .expect("evaluate"),
            );
        }
        assert_eq!(
            answers,
            [true, false, true],
            "an answer followed another call's bindings"
        );
        assert_eq!(
            LOWERINGS.load(Ordering::SeqCst),
            1,
            "the condition was lowered again for the same names"
        );

        let mut wider = this_is("bob");
        wider.insert(
            "?$identity".to_string(),
            FlakeValue::Ref(Sid::new(100, "alice")),
        );
        assert!(
            !executor
                .evaluate_policy_query(&query, &wider)
                .await
                .expect("evaluate"),
            "a call with other names was seeded through the first call's columns"
        );
        assert_eq!(LOWERINGS.load(Ordering::SeqCst), 2);
    }

    /// A scoped key's condition is split: the key's own projects are looked up
    /// once and joined into each call. The answers must be the ones running it
    /// whole would give — for every key, including one scoped to nothing, and
    /// with the key changing between calls on one executor.
    #[tokio::test]
    async fn a_hoisted_condition_answers_as_the_whole_one_would() {
        register_stub();
        assert_eq!(visible("scoped").await, expected_visibility());
    }

    /// The control: an `OPTIONAL` makes the condition something other than a
    /// plain conjunction, so it runs whole, and must still give the same
    /// answers.
    #[tokio::test]
    async fn a_condition_that_is_not_a_plain_conjunction_runs_whole() {
        register_stub();
        assert_eq!(visible("scoped-whole").await, expected_visibility());
    }

    /// Each shape a probe answers — an object from the hoisted rows, from a
    /// binding, or fixed — against the engine running the same condition
    /// whole. The data holds a subject in two projects, one in none, and a
    /// literal where an IRI is looked for.
    #[tokio::test]
    async fn a_probe_answers_as_running_the_condition_would() {
        register_stub();
        for shape in ["scoped", "owner", "fixed"] {
            let probed = visible(shape).await;
            assert!(
                probed.iter().any(|row| row.2) && probed.iter().any(|row| !row.2),
                "{shape}: the data does not tell answers apart"
            );
            assert_eq!(
                probed,
                visible(&format!("{shape}-whole")).await,
                "{shape}: the probe disagreed with the condition run whole"
            );
        }
    }

    /// A probe resolves every subject it was told about on its first miss, so
    /// the rest of the batch is answered without consulting the ledger. A
    /// subject it was not told about is still answered, on its own.
    #[tokio::test]
    async fn a_probe_resolves_its_batch_in_one_pass() {
        register_stub();
        let snapshot = LedgerSnapshot::genesis("test:main");
        let ledger = scoped_ledger();
        let overlay = Counted {
            inner: &ledger,
            reads: AtomicUsize::new(0),
        };
        let executor = QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1)
            .with_subjects(["t1", "t2", "t3", "t4"].map(|t| Sid::new(100, t)).to_vec());
        let query = condition("owner");

        assert!(ask(&executor, &query, "alice", "t1").await);
        let after_first = overlay.reads.load(Ordering::SeqCst);
        for todo in ["t2", "t3", "t4"] {
            assert!(!ask(&executor, &query, "alice", todo).await);
        }
        assert_eq!(
            overlay.reads.load(Ordering::SeqCst),
            after_first,
            "the rest of the batch consulted the ledger again"
        );

        assert!(!ask(&executor, &query, "alice", "t9").await);
        assert!(
            overlay.reads.load(Ordering::SeqCst) > after_first,
            "a subject outside the batch was answered without being looked up"
        );
    }

    /// Which conditions are answered by probe. Each shape the probe knows is;
    /// a `$this` triple whose object nothing constrains is not, nor is
    /// anything that runs whole.
    #[test]
    fn only_a_single_constrained_triple_on_this_becomes_a_probe() {
        let mut names: Vec<String> = judging("alice", "t1").into_keys().collect();
        names.sort();
        let probe = |source: &str| {
            let mut vars = VarRegistry::new();
            let patterns =
                lower_stub(source, &LedgerSnapshot::genesis("test:main"), &mut vars).unwrap();
            let columns: Vec<(VarId, usize)> = names
                .iter()
                .enumerate()
                .map(|(index, name)| (vars.get_or_insert(&sparql_var_name(name)), index))
                .collect();
            let (rest, hoisted) = Hoisted::split(patterns, &columns, &names);
            Probe::of(&rest, hoisted.as_ref(), &columns, &names).map(|probe| probe.object)
        };

        assert!(matches!(probe("scoped"), Some(Allowed::Carried)));
        assert!(matches!(probe("owner"), Some(Allowed::Binding(_))));
        assert!(matches!(probe("fixed"), Some(Allowed::Iri(_))));
        assert!(
            probe("this-ok").is_none(),
            "an unconstrained object was probed"
        );
        assert!(
            probe("scoped-whole").is_none(),
            "a whole condition was probed"
        );
    }

    /// Which conditions are split, and into what. The scoped key's own lookup
    /// moves and carries `?p`; nothing moves out of a condition holding an
    /// `OPTIONAL`, or out of one whose every triple mentions `$this`.
    #[test]
    fn only_the_identity_side_of_a_plain_conjunction_is_hoisted() {
        let mut names: Vec<String> = judging("alice", "t1").into_keys().collect();
        names.sort();
        let split = |source: &str| {
            let mut vars = VarRegistry::new();
            let patterns =
                lower_stub(source, &LedgerSnapshot::genesis("test:main"), &mut vars).unwrap();
            let columns: Vec<(VarId, usize)> = names
                .iter()
                .enumerate()
                .map(|(index, name)| (vars.get_or_insert(&sparql_var_name(name)), index))
                .collect();
            let (rest, hoisted) = Hoisted::split(patterns, &columns, &names);
            (vars, rest, hoisted)
        };

        let (vars, rest, hoisted) = split("scoped");
        let hoisted = hoisted.expect("the scoped key's lookup is hoisted");
        assert_eq!(hoisted.patterns.len(), 1);
        assert_eq!(rest.len(), 1);
        assert_eq!(hoisted.carried, vec![vars.get("?p").unwrap()]);

        let (_, rest, hoisted) = split("scoped-whole");
        assert!(hoisted.is_none(), "a condition with an OPTIONAL was split");
        assert_eq!(rest.len(), 3);

        let (_, rest, hoisted) = split("this-ok");
        assert!(hoisted.is_none(), "a triple on $this was hoisted");
        assert_eq!(rest.len(), 1);
    }

    /// A join probes one subject at a time, and the enforcer builds an
    /// executor per probe. Executors given the same cache must work a
    /// condition out once between them: a subject the first resolved is
    /// answered by the second without reading the ledger.
    #[tokio::test]
    async fn executors_sharing_a_cache_work_a_condition_out_once() {
        register_stub();
        let snapshot = LedgerSnapshot::genesis("test:main");
        let ledger = scoped_ledger();
        let overlay = Counted {
            inner: &ledger,
            reads: AtomicUsize::new(0),
        };
        let cache = Arc::new(ConditionCache::default());
        let query = condition("scoped");

        let first = QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1)
            .with_cache(Arc::clone(&cache))
            .with_subjects(vec![Sid::new(100, "t1"), Sid::new(100, "t2")]);
        assert!(ask(&first, &query, "alice", "t1").await);
        let lowered = LOWERINGS.load(Ordering::SeqCst);
        let read = overlay.reads.load(Ordering::SeqCst);

        let second = QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1)
            .with_cache(Arc::clone(&cache))
            .with_subjects(vec![Sid::new(100, "t2")]);
        assert!(!ask(&second, &query, "alice", "t2").await);
        assert_eq!(
            LOWERINGS.load(Ordering::SeqCst),
            lowered,
            "the second executor lowered the condition again"
        );
        assert_eq!(
            overlay.reads.load(Ordering::SeqCst),
            read,
            "the second executor read the ledger for what the first resolved"
        );
    }

    /// A cache entry says what was true at a `t`. `t2` passes to `alice` at
    /// `t = 2`, so an executor reading `t = 2` through a cache filled at
    /// `t = 1` must see her own it.
    #[tokio::test]
    async fn a_cached_answer_does_not_answer_for_another_t() {
        register_stub();
        let snapshot = LedgerSnapshot::genesis("test:main");
        let mut ledger = scoped_ledger();
        ledger.0.push(link_at("t2", "owner", "alice", 2));
        let cache = Arc::new(ConditionCache::default());
        let query = condition("owner");

        let before =
            QueryPolicyExecutor::with_overlay(&snapshot, &ledger, 1).with_cache(Arc::clone(&cache));
        assert!(!ask(&before, &query, "alice", "t2").await);

        let after =
            QueryPolicyExecutor::with_overlay(&snapshot, &ledger, 2).with_cache(Arc::clone(&cache));
        assert!(
            ask(&after, &query, "alice", "t2").await,
            "an answer cached at t = 1 answered for t = 2"
        );
    }

    /// Staged flakes are not identified by a `t`, so an executor reading a
    /// transaction's post-state keeps its own cache even when handed one —
    /// in either order.
    #[tokio::test]
    async fn an_executor_reading_staged_state_shares_no_cache() {
        register_stub();
        let snapshot = LedgerSnapshot::genesis("test:main");
        let ledger = scoped_ledger();
        let overlay = Counted {
            inner: &ledger,
            reads: AtomicUsize::new(0),
        };
        let cache = Arc::new(ConditionCache::default());
        let query = condition("owner");

        let shared = QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1)
            .with_cache(Arc::clone(&cache));
        assert!(ask(&shared, &query, "alice", "t1").await);

        for staged in [
            QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1)
                .with_post_state(&ledger, 1)
                .with_cache(Arc::clone(&cache)),
            QueryPolicyExecutor::with_overlay(&snapshot, &overlay, 1)
                .with_cache(Arc::clone(&cache))
                .with_post_state(&ledger, 1),
        ] {
            let read = overlay.reads.load(Ordering::SeqCst);
            assert!(ask(&staged, &query, "alice", "t1").await);
            assert!(
                overlay.reads.load(Ordering::SeqCst) > read,
                "an executor with staged state answered from the shared cache"
            );
        }
    }
}
