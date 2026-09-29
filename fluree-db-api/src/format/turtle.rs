//! Turtle graph serializer (`text/turtle`)
//!
//! This formatter is intended for SPARQL CONSTRUCT/DESCRIBE (graph results).
//! It serializes the instantiated construct graph as Turtle, the way
//! [`super::rdf_xml`] serializes it as RDF/XML, and shares the IRI, prefix and
//! escaping rules of the Turtle *export* (`crate::export`) so that a query and an
//! export of the same triples read the same.
//!
//! Prefixes come from the query's `@context` — the same one the JSON-LD graph
//! output compacts with — so a ledger's default context yields the prefixed
//! names a reader already sees everywhere else. Unlike RDF/XML, Turtle can write
//! any IRI in full, so there is no predicate this formatter has to refuse.

use super::config::FormatterConfig;
use super::construct::instantiate_construct_graph;
use super::iri::IriCompactor;
use super::{FormatError, Result};
use crate::export::{
    write_escaped_iri, write_escaped_ntriples_string, write_prefix_declarations, write_turtle_iri,
    PrefixMap,
};
use crate::QueryResult;

use fluree_graph_ir::{Graph, Term};
use fluree_vocab::{rdf, xsd};

use std::collections::BTreeMap;
use std::io::{self, Write};

/// The XSD namespace, which a literal's datatype almost always sits in.
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";

pub fn format(
    result: &QueryResult,
    compactor: &IriCompactor,
    _config: &FormatterConfig,
) -> Result<String> {
    if result.output.construct_template().is_none() {
        return Err(FormatError::InvalidBinding(
            "Turtle is only valid for graph results (SPARQL CONSTRUCT/DESCRIBE)".to_string(),
        ));
    }

    let mut graph = instantiate_construct_graph(result, compactor)?;
    // Sort for deterministic output, and apply RDF set semantics — see the
    // matching call in `construct::format`. Sorting SPO is also what lets
    // `format_graph` group a subject's triples with `;` in a single pass.
    graph.canonicalize();

    let prefixes = result
        .orig_context
        .as_ref()
        .map(PrefixMap::from_context)
        .unwrap_or_else(|| PrefixMap::from_map(BTreeMap::new()));
    format_graph(&graph, &with_xsd(prefixes))
}

/// Add `xsd:` when the context does not already name that namespace.
///
/// Every non-string literal carries an XSD datatype, and a context written for
/// data rarely declares it, so without this each one is spelled out in full —
/// `"1"^^<http://www.w3.org/2001/XMLSchema#integer>` on every line. A prefix the
/// context already binds to something else is left alone rather than
/// overwritten, since that would silently change what its names mean.
fn with_xsd(prefixes: PrefixMap) -> PrefixMap {
    let mut map: BTreeMap<String, String> = prefixes
        .iter()
        .map(|(p, ns)| (p.to_string(), ns.to_string()))
        .collect();
    let declared = map.values().any(|ns| ns == XSD_NS);
    if !declared && !map.contains_key("xsd") {
        map.insert("xsd".to_string(), XSD_NS.to_string());
    }
    PrefixMap::from_map(map)
}

fn format_graph(graph: &Graph, prefixes: &PrefixMap) -> Result<String> {
    let mut out = Vec::new();
    write_graph(graph, prefixes, &mut out).map_err(io_error)?;
    String::from_utf8(out)
        .map_err(|err| FormatError::InvalidBinding(format!("Turtle output was not UTF-8: {err}")))
}

/// The export's layout (`crate::export::write_turtle_batch`), so a query and an
/// export of the same triples read the same: the subject alone on a line, then
/// one `predicate object` per line joined with `;`. A predicate with several
/// objects is repeated rather than folded into a `,` list — one value per line
/// whatever the count, which is what a reader scanning or diffing it wants.
///
/// Relies on the graph being sorted SPO, so a subject's triples are adjacent.
fn write_graph<W: Write>(graph: &Graph, prefixes: &PrefixMap, w: &mut W) -> io::Result<()> {
    write_prefix_declarations(prefixes, w)?;

    let mut current: Option<&Term> = None;
    for triple in types_first(graph) {
        let (s, p, o) = (triple.subject(), triple.predicate(), triple.object());
        if current == Some(s) {
            w.write_all(b" ;\n    ")?;
        } else {
            if current.is_some() {
                w.write_all(b" .\n")?;
            }
            write_node(w, s, prefixes)?;
            w.write_all(b"\n    ")?;
            current = Some(s);
        }
        write_predicate(w, p, prefixes)?;
        w.write_all(b" ")?;
        write_object(w, o, prefixes)?;
    }
    if current.is_some() {
        w.write_all(b" .\n")?;
    }
    Ok(())
}

/// The graph's triples, sorted SPO except that each subject's `rdf:type` comes
/// first.
///
/// Sorting by IRI puts `rdf:type` wherever `http://www.w3.org/…` happens to fall
/// among the other predicates — usually last — and a block that opens with what
/// the thing *is* is the convention every hand-written Turtle file follows. A
/// stable sort within each subject's run keeps everything else in SPO order.
fn types_first(graph: &Graph) -> Vec<&fluree_graph_ir::Triple> {
    let mut triples: Vec<_> = graph.iter().collect();
    let mut start = 0;
    while start < triples.len() {
        let subject = triples[start].subject();
        let end = triples[start..]
            .iter()
            .position(|t| t.subject() != subject)
            .map_or(triples.len(), |n| start + n);
        triples[start..end].sort_by_key(|t| t.predicate().as_iri() != Some(rdf::TYPE));
        start = end;
    }
    triples
}

/// `rdf:type` as `a`, the one abbreviation every Turtle reader expects.
fn write_predicate<W: Write>(w: &mut W, predicate: &Term, prefixes: &PrefixMap) -> io::Result<()> {
    match predicate.as_iri() {
        Some(iri) if iri == rdf::TYPE => w.write_all(b"a"),
        _ => write_node(w, predicate, prefixes),
    }
}

/// An IRI or a blank node — anything but a literal.
///
/// An IRI decoded from the ledger can itself spell a blank node as `_:label`,
/// which is how the export sees them too, so it is written as one rather than
/// as the IRI `<_:label>`. Labels are written verbatim, for the reason
/// `crate::export` gives: every label Fluree currently mints is valid Turtle,
/// and rewriting one could merge two distinct nodes.
fn write_node<W: Write>(w: &mut W, term: &Term, prefixes: &PrefixMap) -> io::Result<()> {
    match term {
        Term::Iri(iri) if iri.starts_with("_:") => w.write_all(iri.as_bytes()),
        Term::Iri(iri) => write_turtle_iri(w, iri, prefixes),
        Term::BlankNode(id) => w.write_all(id.to_ntriples().as_bytes()),
        Term::Literal { .. } => write_object(w, term, prefixes),
    }
}

fn write_object<W: Write>(w: &mut W, object: &Term, prefixes: &PrefixMap) -> io::Result<()> {
    let Term::Literal {
        value,
        datatype,
        language,
    } = object
    else {
        return write_node(w, object, prefixes);
    };

    let lexical = value.lexical();
    let dt = datatype.as_iri();

    // The two shorthands that are unambiguous whatever the lexical form holds:
    // a bare integer or boolean is read back as exactly this datatype. Decimal
    // and double shorthands depend on the lexical form's shape, so those keep
    // the explicit `^^` form rather than risk reading back as something else.
    if language.is_none() && dt == xsd::BOOLEAN && (lexical == "true" || lexical == "false") {
        return w.write_all(lexical.as_bytes());
    }
    if language.is_none() && dt == xsd::INTEGER && is_turtle_integer(&lexical) {
        return w.write_all(lexical.as_bytes());
    }

    w.write_all(b"\"")?;
    write_escaped_ntriples_string(w, &lexical)?;
    w.write_all(b"\"")?;
    if let Some(lang) = language {
        w.write_all(b"@")?;
        w.write_all(lang.as_bytes())
    } else if datatype.is_xsd_string() {
        Ok(())
    } else {
        w.write_all(b"^^")?;
        if prefixes.compact(dt).is_some() {
            write_turtle_iri(w, dt, prefixes)
        } else {
            w.write_all(b"<")?;
            write_escaped_iri(w, dt)?;
            w.write_all(b">")
        }
    }
}

/// Turtle's `INTEGER` production: an optional sign and at least one digit.
fn is_turtle_integer(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn io_error(err: io::Error) -> FormatError {
    FormatError::InvalidBinding(format!("Turtle serialization failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluree_graph_ir::{Datatype, LiteralValue, Triple};

    fn ex(local: &str) -> Term {
        Term::iri(format!("http://example.org/{local}"))
    }

    fn render(graph: &mut Graph, context: serde_json::Value) -> String {
        graph.canonicalize();
        format_graph(graph, &with_xsd(PrefixMap::from_context(&context))).unwrap()
    }

    /// The reason the formatter exists: a DESCRIBE reads as the Turtle a person
    /// would have written, with the context's prefixes and `a` for the type.
    #[test]
    fn a_subject_reads_as_one_block_with_prefixed_names() {
        let mut g = Graph::new();
        g.add(Triple::new(ex("alice"), Term::iri(rdf::TYPE), ex("Person")));
        g.add(Triple::new(ex("alice"), ex("name"), Term::string("Alice")));
        g.add(Triple::new(ex("alice"), ex("knows"), ex("bob")));
        g.add(Triple::new(ex("alice"), ex("knows"), ex("carol")));
        g.add(Triple::new(ex("bob"), ex("name"), Term::string("Bob")));

        let ttl = render(&mut g, serde_json::json!({"ex": "http://example.org/"}));
        assert_eq!(
            ttl,
            "@prefix ex: <http://example.org/> .\n\
             @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
             \n\
             ex:alice\n    \
             a ex:Person ;\n    \
             ex:knows ex:bob ;\n    \
             ex:knows ex:carol ;\n    \
             ex:name \"Alice\" .\n\
             ex:bob\n    \
             ex:name \"Bob\" .\n"
        );
    }

    /// Without a matching prefix an IRI is written whole. RDF/XML refuses a
    /// predicate it cannot split into a QName; Turtle never has to.
    #[test]
    fn an_iri_no_prefix_covers_is_written_in_full() {
        let mut g = Graph::new();
        g.add(Triple::new(
            Term::iri("urn:x:1"),
            Term::iri("http://example.org/p/1"),
            Term::iri("urn:x:2"),
        ));

        let ttl = render(&mut g, serde_json::json!({}));
        assert!(
            ttl.contains("<urn:x:1>\n    <http://example.org/p/1> <urn:x:2> ."),
            "{ttl}"
        );
    }

    /// A literal must survive a round trip exactly: quotes and newlines escaped,
    /// the language tag kept, and a datatype written unless it is `xsd:string`.
    #[test]
    fn literals_keep_their_escapes_language_and_datatype() {
        let mut g = Graph::new();
        let s = ex("s");
        g.add(Triple::new(
            s.clone(),
            ex("a"),
            Term::string("say \"hi\"\nthen go"),
        ));
        g.add(Triple::new(
            s.clone(),
            ex("b"),
            Term::lang_string("bonjour", "fr"),
        ));
        g.add(Triple::new(
            s.clone(),
            ex("c"),
            Term::typed("2026-09-29T19:28:12Z", Datatype::from_iri(xsd::DATE_TIME)),
        ));
        g.add(Triple::new(s.clone(), ex("d"), Term::integer(42)));
        g.add(Triple::new(s.clone(), ex("e"), Term::boolean(true)));
        g.add(Triple::new(
            s,
            ex("f"),
            Term::Literal {
                value: LiteralValue::string("1.5"),
                datatype: Datatype::from_iri(xsd::DECIMAL),
                language: None,
            },
        ));

        let ttl = render(&mut g, serde_json::json!({"ex": "http://example.org/"}));
        assert!(ttl.contains(r#"ex:a "say \"hi\"\nthen go""#), "{ttl}");
        assert!(ttl.contains(r#"ex:b "bonjour"@fr"#), "{ttl}");
        assert!(
            ttl.contains(r#"ex:c "2026-09-29T19:28:12Z"^^xsd:dateTime"#),
            "{ttl}"
        );
        assert!(ttl.contains("ex:d 42"), "{ttl}");
        assert!(ttl.contains("ex:e true"), "{ttl}");
        assert!(ttl.contains(r#"ex:f "1.5"^^xsd:decimal"#), "{ttl}");
    }

    /// A context that binds `xsd` to something else keeps its meaning; the
    /// datatype is spelled out instead of being given a prefix that lies.
    #[test]
    fn a_context_that_claims_xsd_is_not_overridden() {
        let mut g = Graph::new();
        g.add(Triple::new(
            ex("s"),
            ex("when"),
            Term::typed("2026-09-29", Datatype::from_iri(xsd::DATE)),
        ));

        let ttl = render(
            &mut g,
            serde_json::json!({"ex": "http://example.org/", "xsd": "http://example.org/not-xsd#"}),
        );
        assert!(
            ttl.contains(r#""2026-09-29"^^<http://www.w3.org/2001/XMLSchema#date>"#),
            "{ttl}"
        );
    }

    /// A blank node is written as one, whether the graph holds it as a blank
    /// node or as a ledger IRI spelled `_:label`.
    #[test]
    fn blank_nodes_are_not_written_as_iris() {
        let mut g = Graph::new();
        g.add(Triple::new(Term::blank("cst0"), ex("p"), Term::iri("_:b1")));

        let ttl = render(&mut g, serde_json::json!({"ex": "http://example.org/"}));
        assert!(ttl.contains("_:cst0\n    ex:p _:b1 ."), "{ttl}");
        assert!(!ttl.contains("<_:"), "{ttl}");
    }

    #[test]
    fn an_empty_graph_is_an_empty_document_apart_from_prefixes() {
        let ttl = render(&mut Graph::new(), serde_json::json!({}));
        assert_eq!(
            ttl,
            "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n"
        );
    }
}
