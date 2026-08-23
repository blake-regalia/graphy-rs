//! SPARQL semantic-token adapter (docs/10 §7). Maps the resilient SPARQL token
//! stream to the shared legend. Tier 1 — lexical only; the parser/algebra tier
//! (variable scope, prefix resolution) lands in M11b.

use graphy_sparql_syntax::{tokenize_resilient, Token, TokenKind};

use crate::legend::SemKind;
use crate::semantic::{push_prefixed_name, SemBuilder, SemToken};

/// Classify a SPARQL token. `None` = emit no semantic token (structural
/// punctuation and error runs). `PNameLn` is handled separately because it
/// splits into two spans.
fn sem_kind(kind: TokenKind) -> Option<SemKind> {
    use TokenKind as T;
    Some(match kind {
        T::IriRef => SemKind::Class,
        T::PNameNs => SemKind::Namespace,
        T::BlankNode | T::Var => SemKind::Variable,
        T::LangTag(_) => SemKind::Decorator,
        T::Integer | T::Decimal | T::Double => SemKind::Number,
        T::True | T::False => SemKind::Keyword,
        T::String(_) => SemKind::String,
        T::A | T::Keyword(_) => SemKind::Keyword,
        // Expression / path / datatype operators.
        T::Eq
        | T::Ne
        | T::Lt
        | T::Le
        | T::Gt
        | T::Ge
        | T::AndAnd
        | T::OrOr
        | T::Bang
        | T::Plus
        | T::Minus
        | T::Star
        | T::Slash
        | T::Pipe
        | T::Caret
        | T::CaretCaret
        | T::Question
        | T::LtLt
        | T::GtGt
        | T::LtLtParen
        | T::RParenGtGt
        | T::Tilde
        | T::LBraceBar
        | T::RBarBrace => SemKind::Operator,
        // Structural punctuation, empty collections, and error runs get no
        // colour (the editor styles brackets; errors get a diagnostic).
        T::PNameLn
        | T::LBrace
        | T::RBrace
        | T::LParen
        | T::RParen
        | T::LBracket
        | T::RBracket
        | T::Semicolon
        | T::Comma
        | T::Dot
        | T::Nil
        | T::Anon
        | T::Error => return None,
    })
}

/// Resolved semantic tokens for a SPARQL query or update.
pub fn sparql_semantic_tokens(src: &str) -> Vec<SemToken> {
    let mut b = SemBuilder::new(src);
    for Token { kind, span } in tokenize_resilient(src) {
        if kind == TokenKind::PNameLn {
            push_prefixed_name(&mut b, src, span.start, span.end);
        } else if let Some(k) = sem_kind(kind) {
            b.push(span.start, span.end, k);
        }
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<(u32, u32, u32, SemKind)> {
        sparql_semantic_tokens(src)
            .into_iter()
            .map(|t| (t.line, t.start, t.len, t.kind))
            .collect()
    }

    #[test]
    fn classifies_a_select() {
        let got = toks("PREFIX ex: <http://e/>\nSELECT ?s WHERE { ?s ex:p ?o }");
        assert_eq!(
            got,
            vec![
                (0, 0, 6, SemKind::Keyword),   // PREFIX
                (0, 7, 3, SemKind::Namespace), // ex:
                (0, 11, 11, SemKind::Class),   // <http://e/>
                (1, 0, 6, SemKind::Keyword),   // SELECT
                (1, 7, 2, SemKind::Variable),  // ?s
                (1, 10, 5, SemKind::Keyword),  // WHERE
                // '{' -> no token
                (1, 18, 2, SemKind::Variable),  // ?s
                (1, 21, 3, SemKind::Namespace), // ex:
                (1, 24, 1, SemKind::Property),  // p
                (1, 26, 2, SemKind::Variable),  // ?o
                                                // '}' -> no token
            ]
        );
    }

    #[test]
    fn expression_operators_and_literals() {
        let got = toks("ASK { FILTER(?x + 1 >= 2 && true) }");
        assert!(got.contains(&(0, 6, 6, SemKind::Keyword))); // FILTER
        let ops = got.iter().filter(|t| t.3 == SemKind::Operator).count();
        assert_eq!(ops, 3); // + >= &&
        assert!(got.iter().any(|t| t.3 == SemKind::Number));
    }

    #[test]
    fn garbage_never_panics_and_keeps_going() {
        // Backtick isn't a SPARQL byte; the query after it still classifies.
        let src = "SELECT ` ?s WHERE { ?s ?p ?o }";
        let got = sparql_semantic_tokens(src);
        // No Error kind is emitted as a semantic token; but scanning continued,
        // so the trailing variables are present.
        let vars = got.iter().filter(|t| t.kind == SemKind::Variable).count();
        assert_eq!(vars, 4); // ?s ?s ?p ?o
    }
}
