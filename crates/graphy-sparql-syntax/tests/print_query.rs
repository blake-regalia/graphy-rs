//! §M13b whole-query/update printing: canonical layout (tab indent, one
//! element per line), used-prefix-only prologue, and sugar reconstruction
//! — collections and `[ … ]` property lists rebuilt from parser-fresh
//! `.`-labels. Every case asserts the exact text and that the output is
//! a print fixpoint.

use graphy_sparql_syntax::{parse_query, parse_update, print_query, print_update};

fn check_query(src: &str, want: &str) {
    let got = print_query(&parse_query(src).expect("parse"));
    assert_eq!(got, want, "print of {src:?}");
    let again = print_query(&parse_query(&got).expect("re-parse"));
    assert_eq!(again, want, "fixpoint of {src:?}");
}

fn check_update(src: &str, want: &str) {
    let got = print_update(&parse_update(src).expect("parse"));
    assert_eq!(got, want, "print of {src:?}");
    let again = print_update(&parse_update(&got).expect("re-parse"));
    assert_eq!(again, want, "fixpoint of {src:?}");
}

// ------------------------------------------------------------ layout

#[test]
fn select_canonical_layout() {
    check_query(
        "PREFIX ex: <http://ex/> PREFIX unused: <http://nope/> \
         SELECT ?s WHERE { ?s ex:p ?o . OPTIONAL { ?s ex:q ?y } FILTER(?o > 1) } \
         ORDER BY ?s LIMIT 10 OFFSET 5",
        "PREFIX ex: <http://ex/>\n\
         SELECT ?s\n\
         WHERE {\n\
         \t?s ex:p ?o .\n\
         \tOPTIONAL {\n\
         \t\t?s ex:q ?y .\n\
         \t}\n\
         \tFILTER(?o > 1)\n\
         }\n\
         ORDER BY ?s\n\
         LIMIT 10\n\
         OFFSET 5\n",
    );
}

#[test]
fn subject_and_object_folding() {
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { ?s ex:p 1 . ?s ex:p 2 . ?s ex:q 3 . ?z ex:p 4 }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t?s ex:p 1 , 2 ; ex:q 3 .\n\
         \t?z ex:p 4 .\n\
         }\n",
    );
}

#[test]
fn union_graph_bind_values() {
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { \
           { ?s a ex:A } UNION { ?s a ex:B } \
           GRAPH ?g { ?s ex:p ?o } \
           BIND(?o + 1 AS ?p1) \
           VALUES (?x ?y) { (1 UNDEF) (ex:c 2) } }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t{\n\
         \t\t?s a ex:A .\n\
         \t} UNION {\n\
         \t\t?s a ex:B .\n\
         \t}\n\
         \tGRAPH ?g {\n\
         \t\t?s ex:p ?o .\n\
         \t}\n\
         \tBIND(?o + 1 AS ?p1)\n\
         \tVALUES (?x ?y) { (1 UNDEF) (ex:c 2) }\n\
         }\n",
    );
}

#[test]
fn subselect_and_aggregates() {
    check_query(
        "PREFIX ex: <http://ex/> SELECT ?s WHERE { \
           { SELECT ?s (COUNT(?o) AS ?n) WHERE { ?s ex:p ?o } GROUP BY ?s HAVING(COUNT(?o) > 2) } }",
        "PREFIX ex: <http://ex/>\n\
         SELECT ?s\n\
         WHERE {\n\
         \t{\n\
         \t\tSELECT ?s (COUNT(?o) AS ?n)\n\
         \t\tWHERE {\n\
         \t\t\t?s ex:p ?o .\n\
         \t\t}\n\
         \t\tGROUP BY ?s\n\
         \t\tHAVING(COUNT(?o) > 2)\n\
         \t}\n\
         }\n",
    );
}

#[test]
fn construct_ask_describe() {
    check_query(
        "PREFIX ex: <http://ex/> CONSTRUCT { ?s ex:q ?o } FROM <http://g/> WHERE { ?s ex:p ?o }",
        "PREFIX ex: <http://ex/>\n\
         CONSTRUCT {\n\
         \t?s ex:q ?o .\n\
         }\n\
         FROM <http://g/>\n\
         WHERE {\n\
         \t?s ex:p ?o .\n\
         }\n",
    );
    check_query(
        "PREFIX ex: <http://ex/> ASK { ?s ex:p ?o }",
        "PREFIX ex: <http://ex/>\nASK\nWHERE {\n\t?s ex:p ?o .\n}\n",
    );
    check_query(
        "PREFIX ex: <http://ex/> DESCRIBE ex:thing ?x",
        "PREFIX ex: <http://ex/>\nDESCRIBE ex:thing ?x\n",
    );
}

#[test]
fn version_and_base_survive() {
    check_query(
        "VERSION \"1.2\" BASE <http://base/> SELECT * WHERE { <rel> ?p ?o }",
        "VERSION \"1.2\"\n\
         BASE <http://base/>\n\
         SELECT *\n\
         WHERE {\n\
         \t<http://base/rel> ?p ?o .\n\
         }\n",
    );
}

// ------------------------------------------------- sugar reconstruction

#[test]
fn collections() {
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { ?s ex:p ( 1 2 3 ) }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t?s ex:p ( 1 2 3 ) .\n\
         }\n",
    );
    // Statement position, empty list object, nesting.
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { ( 1 ( 2 ) ) ex:p () }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t( 1 ( 2 ) ) ex:p () .\n\
         }\n",
    );
}

#[test]
fn bnode_property_lists() {
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { ?s ex:p [ ex:q 1 ; ex:r 2 , 3 ] }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t?s ex:p [ ex:q 1 ; ex:r 2 , 3 ] .\n\
         }\n",
    );
    // Statement position and bare [] object.
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { [ ex:p 1 ] ex:q ?x . ?s ex:r [] }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t[ ex:p 1 ; ex:q ?x ] .\n\
         \t?s ex:r [] .\n\
         }\n",
    );
    // Nested: list of property lists.
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { ?s ex:p ( [ ex:q 1 ] [ ex:q 2 ] ) }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t?s ex:p ( [ ex:q 1 ] [ ex:q 2 ] ) .\n\
         }\n",
    );
}

#[test]
fn user_labels_stay_labels() {
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { _:b ex:p 1 . ?s ex:q _:b }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t_:b ex:p 1 .\n\
         \t?s ex:q _:b .\n\
         }\n",
    );
}

#[test]
fn reification_stays_expanded_but_anonymous() {
    // `<< s p o >>` desugars to a fresh reifier + rdf:reifies; with no
    // rdf: prefix declared the property prints absolute, and the
    // unreferenced fresh reifier folds into an anonymous subject.
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { << ?s ex:p ?o >> ex:said ?who }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t[ <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( ?s ex:p ?o )>> ; ex:said ?who ] .\n\
         }\n",
    );
}

#[test]
fn exists_bodies_reconstruct_sugar_inline() {
    check_query(
        "PREFIX ex: <http://ex/> SELECT * WHERE { ?s ex:p ?o . FILTER(EXISTS { ?s ex:q ( 1 2 ) }) }",
        "PREFIX ex: <http://ex/>\n\
         SELECT *\n\
         WHERE {\n\
         \t?s ex:p ?o .\n\
         \tFILTER(EXISTS { ?s ex:q ( 1 2 ) . })\n\
         }\n",
    );
}

// ------------------------------------------------------------- updates

#[test]
fn update_data_and_graphs() {
    check_update(
        "PREFIX ex: <http://ex/> INSERT DATA { ex:s ex:p 1 . GRAPH ex:g { ex:s ex:q [ ex:r 2 ] } }",
        "PREFIX ex: <http://ex/>\n\
         INSERT DATA {\n\
         \tex:s ex:p 1 .\n\
         \tGRAPH ex:g {\n\
         \t\tex:s ex:q [ ex:r 2 ] .\n\
         \t}\n\
         }\n",
    );
}

#[test]
fn update_modify_and_sequence() {
    check_update(
        "PREFIX ex: <http://ex/> WITH <http://g/> DELETE { ?s ex:old ?o } INSERT { ?s ex:new ?o } \
         USING <http://u/> WHERE { ?s ex:old ?o } ; CLEAR SILENT GRAPH ex:g ; \
         COPY DEFAULT TO GRAPH ex:h",
        "PREFIX ex: <http://ex/>\n\
         WITH <http://g/>\n\
         DELETE {\n\
         \t?s ex:old ?o .\n\
         }\n\
         INSERT {\n\
         \t?s ex:new ?o .\n\
         }\n\
         USING <http://u/>\n\
         WHERE {\n\
         \t?s ex:old ?o .\n\
         }\n\
         ;\n\
         CLEAR SILENT GRAPH ex:g\n\
         ;\n\
         COPY DEFAULT TO GRAPH ex:h\n",
    );
}

#[test]
fn update_load_create_delete_where() {
    check_update(
        "LOAD SILENT <http://d/> INTO GRAPH <http://g/> ; CREATE GRAPH <http://g2/> ; \
         DELETE WHERE { ?s ?p ?o }",
        "LOAD SILENT <http://d/> INTO GRAPH <http://g/>\n\
         ;\n\
         CREATE GRAPH <http://g2/>\n\
         ;\n\
         DELETE WHERE {\n\
         \t?s ?p ?o .\n\
         }\n",
    );
}
