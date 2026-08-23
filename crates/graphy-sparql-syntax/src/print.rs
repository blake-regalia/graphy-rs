//! SPARQL concrete-syntax emission from the AST (plan §M13a/b):
//! [`print_query`]/[`print_update`] emit whole documents in the canonical
//! layout (tab indent, one element per line, prologue restricted to the
//! prefixes the body actually used), built on term/path/expression
//! printers with precedence-driven parenthesization.
//!
//! The tree stores IRIs absolute (the parser resolves the prologue), so
//! the printer re-compresses them against a caller-supplied prefix map —
//! normally [`crate::ast::Query::prefixes`] — falling back to `<…>` when
//! the tail is not expressible as a `PN_LOCAL`. Output re-parses to the
//! same tree shape: parenthesization mirrors the expression/path grammar
//! levels exactly, so associativity that required parentheses in source
//! requires them in output and flat chains stay flat.
//!
//! Sugar comes back on the way out: parser-fresh (`.`-leading) blank
//! labels whose every occurrence sits in one triples run are rebuilt into
//! the construct that minted them — `rdf:first`/`rdf:rest` spines as
//! `( … )` collections (statement or object position, extra head
//! properties honored), everything else as `[ … ]` property lists, with
//! `rdf:nil` as `()`. Any shape deviation (multiple references, a
//! reference inside a triple term or another run) falls back to the
//! label form, which is always semantics-preserving. Reification stays
//! expanded (`rdf:reifies` + triple term), its fresh reifier folding into
//! an anonymous subject.

use std::fmt::Write as _;

use graphy_core::text::{is_forbidden_iri_byte, is_pn_chars, is_pn_chars_u, is_pn_local_esc};
use graphy_core::vocab;

use crate::ast::{
    Aggregate, Builtin, CmpOp, DatasetClause, Expr, ExprKind, GraphOrDefault, GraphTarget,
    GroupCondition, GroupElement, GroupPattern, LiteralKind, Path, Projection, Quad, Query,
    QueryForm, SelectClause, SolutionModifiers, SubSelect, Term, TermKind, TriplePattern, UpdateOp,
    UpdateRequest, ValuesBlock, Verb,
};
use crate::token::Dir;

/// Print a whole query in the canonical layout: `VERSION`/`BASE` and the
/// *used* subset of the prefix declarations, the query form, multi-line
/// `WHERE`, solution modifiers, trailing `VALUES` — with collections and
/// blank-node property lists reconstructed from the parser's `.`-fresh
/// labels (§M13b).
pub fn print_query(q: &Query) -> String {
    let mut p = Printer::new(&q.prefixes);
    p.count_query(q);
    p.query(q);
    p.out.push('\n');
    p.assemble(q.version.as_deref(), q.base.as_deref())
}

/// Print a whole update request (operations `;`-separated), same
/// conventions as [`print_query`].
pub fn print_update(u: &UpdateRequest) -> String {
    let mut p = Printer::new(&u.prefixes);
    p.count_update(u);
    p.update(u);
    p.out.push('\n');
    p.assemble(u.version.as_deref(), None)
}

/// Print one expression (top level, no outer parentheses) against a
/// prefix map in declaration order (later shadows earlier).
pub fn print_expr(expr: &Expr, prefixes: &[(String, String)]) -> String {
    let mut p = Printer::new(prefixes);
    p.count_expr(expr);
    p.expr(expr);
    p.finish()
}

/// Print one property path (top level).
pub fn print_path(path: &Path, prefixes: &[(String, String)]) -> String {
    let mut p = Printer::new(prefixes);
    p.path(path);
    p.finish()
}

/// Print one term.
pub fn print_term(term: &Term, prefixes: &[(String, String)]) -> String {
    let mut p = Printer::new(prefixes);
    p.term(term);
    p.finish()
}

/// Stateful emitter: owns the output buffer and the resolved
/// namespace→prefix table. The building block for §M13b's query/update
/// printers; the `pub` methods append at the named grammar level.
#[derive(Debug)]
pub struct Printer {
    out: String,
    /// `(namespace, prefix)`, longest namespace first (so compression
    /// takes the most specific declaration).
    by_ns: Vec<(String, String)>,
    /// Resolved declarations `(prefix, namespace)` in source order — the
    /// header emits the `used` subset of these.
    decls: Vec<(String, String)>,
    /// Prefixes actually taken by a compression.
    used: std::collections::BTreeSet<String>,
    /// Occurrences of each parser-fresh (`.`-leading) blank label across
    /// the whole printed unit. Sugar reconstruction consumes a label only
    /// when its run holds *every* occurrence — a label referenced from
    /// another run (impossible from the parser, possible in a hand-built
    /// tree) keeps its printed-label form everywhere.
    fresh: std::collections::HashMap<String, usize>,
    /// Every user blank label in the unit (from the count pass) plus the
    /// names already handed to fresh labels — the collision set for
    /// [`Printer::blank`]'s fresh renaming.
    taken: std::collections::HashSet<String>,
    /// Stable printed name per fresh label.
    fresh_names: std::collections::HashMap<String, String>,
}

impl Printer {
    /// `prefixes` in declaration order, `(prefix, namespace)` pairs: the
    /// last declaration of a prefix wins (SPARQL prologue semantics);
    /// when several prefixes name one namespace the first survivor wins.
    pub fn new(prefixes: &[(String, String)]) -> Printer {
        let mut resolved: Vec<(&str, &str)> = Vec::new();
        for (pfx, ns) in prefixes {
            match resolved.iter_mut().find(|(p, _)| *p == pfx) {
                Some(slot) => slot.1 = ns,
                None => resolved.push((pfx, ns)),
            }
        }
        let mut by_ns: Vec<(String, String)> = Vec::new();
        for (pfx, ns) in &resolved {
            if !by_ns.iter().any(|(n, _)| n == ns) {
                by_ns.push(((*ns).to_owned(), (*pfx).to_owned()));
            }
        }
        by_ns.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Printer {
            out: String::new(),
            by_ns,
            decls: resolved
                .into_iter()
                .map(|(p, n)| (p.to_owned(), n.to_owned()))
                .collect(),
            used: std::collections::BTreeSet::new(),
            fresh: std::collections::HashMap::new(),
            taken: std::collections::HashSet::new(),
            fresh_names: std::collections::HashMap::new(),
        }
    }

    pub fn finish(self) -> String {
        self.out
    }

    /// Prepend the prologue (`VERSION`/`BASE`/used `PREFIX` lines) to the
    /// printed body. `BASE` is semantics-carrying — runtime `IRI()`
    /// resolves against it (§17.4.2.8) — so it survives even though every
    /// emitted IRI is absolute or prefixed.
    fn assemble(self, version: Option<&str>, base: Option<&str>) -> String {
        let mut head = Printer::new(&[]);
        if let Some(v) = version {
            head.out.push_str("VERSION ");
            head.quoted(v);
            head.out.push('\n');
        }
        if let Some(b) = base {
            head.out.push_str("BASE ");
            head.iriref(b);
            head.out.push('\n');
        }
        for (pfx, ns) in &self.decls {
            if self.used.contains(pfx) {
                head.out.push_str("PREFIX ");
                head.out.push_str(pfx);
                head.out.push_str(": ");
                head.iriref(ns);
                head.out.push('\n');
            }
        }
        head.out + &self.out
    }

    /// Run `f` against a side buffer and return what it wrote (prefix
    /// usage still accumulates on `self`).
    fn capture(&mut self, f: impl FnOnce(&mut Self)) -> String {
        let saved = std::mem::take(&mut self.out);
        f(self);
        std::mem::replace(&mut self.out, saved)
    }

    fn line(&mut self, depth: usize) {
        self.out.push('\n');
        for _ in 0..depth {
            self.out.push('\t');
        }
    }

    fn trim_spaces(&mut self) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
    }

    // ------------------------------------------------------------- terms

    /// An IRI, as a prefixed name when a declared namespace covers it and
    /// the tail is `PN_LOCAL`-expressible, else `<…>`.
    pub fn iri(&mut self, iri: &str) {
        for (ns, pfx) in &self.by_ns {
            if let Some(rest) = iri.strip_prefix(ns.as_str()) {
                if let Some(local) = pname_local(rest) {
                    self.out.push_str(pfx);
                    self.out.push(':');
                    self.out.push_str(&local);
                    let pfx = pfx.clone();
                    self.used.insert(pfx);
                    return;
                }
            }
        }
        self.iriref(iri);
    }

    fn iriref(&mut self, iri: &str) {
        self.out.push('<');
        for c in iri.chars() {
            // IRIREF has no escape mechanism; a forbidden byte here means
            // the value was never a printable IRI — percent-encode it
            // rather than emit unparseable text.
            if c.is_ascii() && is_forbidden_iri_byte(c as u8) {
                let _ = write!(self.out, "%{:02X}", c as u32);
            } else {
                self.out.push(c);
            }
        }
        self.out.push('>');
    }

    /// Blank node. User labels print verbatim (identity — so printing is
    /// idempotent). A parser-fresh label (leading `.`, from desugared
    /// sugar) that could not be reconstructed takes the first
    /// `_`-prefixed candidate not claimed by a user label in this unit
    /// (the count pass fills the collision set) nor by another fresh
    /// label — deterministic, collision-free, and stable across
    /// re-prints (the renamed label is a plain user label next round).
    pub fn blank(&mut self, label: &str) {
        self.out.push_str("_:");
        if let Some(rest) = label.strip_prefix('.') {
            if let Some(name) = self.fresh_names.get(label) {
                let name = name.clone();
                self.out.push_str(&name);
                return;
            }
            let mut candidate = format!("_{rest}");
            while self.taken.contains(&candidate) {
                candidate.insert(0, '_');
            }
            self.taken.insert(candidate.clone());
            self.fresh_names.insert(label.to_owned(), candidate.clone());
            self.out.push_str(&candidate);
        } else {
            self.out.push_str(label);
        }
    }

    pub fn term(&mut self, t: &Term) {
        match &t.kind {
            TermKind::Iri(i) => self.iri(i),
            TermKind::BlankNode(label) => self.blank(label),
            TermKind::Literal { lexical, kind } => self.literal(lexical, kind),
            TermKind::Var(name) => {
                self.out.push('?');
                self.out.push_str(name);
            }
            TermKind::TripleTerm(tp) => {
                self.out.push_str("<<( ");
                self.triple(tp);
                self.out.push_str(" )>>");
            }
        }
    }

    fn literal(&mut self, lexical: &str, kind: &LiteralKind) {
        match kind {
            LiteralKind::Plain => self.quoted(lexical),
            LiteralKind::Lang { tag, dir } => {
                self.quoted(lexical);
                self.out.push('@');
                self.out.push_str(tag);
                match dir {
                    Some(Dir::Ltr) => self.out.push_str("--ltr"),
                    Some(Dir::Rtl) => self.out.push_str("--rtl"),
                    None => {}
                }
            }
            LiteralKind::Typed(dt) => {
                let bare = match dt.as_str() {
                    vocab::XSD_INTEGER => is_integer_lexical(lexical),
                    vocab::XSD_DECIMAL => is_decimal_lexical(lexical),
                    vocab::XSD_DOUBLE => is_double_lexical(lexical),
                    vocab::XSD_BOOLEAN => lexical == "true" || lexical == "false",
                    _ => false,
                };
                if bare {
                    self.out.push_str(lexical);
                } else {
                    self.quoted(lexical);
                    self.out.push_str("^^");
                    self.iri(dt);
                }
            }
        }
    }

    fn quoted(&mut self, s: &str) {
        self.out.push('"');
        for c in s.chars() {
            match c {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                '\u{8}' => self.out.push_str("\\b"),
                '\u{C}' => self.out.push_str("\\f"),
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }

    // ----------------------------------------------------- triples/paths

    pub fn triple(&mut self, tp: &TriplePattern) {
        self.term(&tp.s);
        self.out.push(' ');
        self.verb(&tp.p);
        self.out.push(' ');
        self.term(&tp.o);
    }

    pub fn verb(&mut self, v: &Verb) {
        match v {
            Verb::Term(t) => match &t.kind {
                TermKind::Iri(i) if i == vocab::RDF_TYPE => self.out.push('a'),
                _ => self.term(t),
            },
            Verb::Path(p) => self.path(p),
        }
    }

    pub fn path(&mut self, p: &Path) {
        self.path_prec(p, 1);
    }

    /// Grammar levels: `|` 1 < `/` 2 < `^` 3 < postfix `* + ?` 4 <
    /// primary 5. A node prints parenthesized when its level is below
    /// what its context requires, mirroring the grammar exactly — e.g.
    /// postfix operands must be primary (`(a/b)*`, `(a*)*`), and `^`
    /// takes a whole PathElt (`^a*` = `^(a*)`).
    fn path_prec(&mut self, p: &Path, min: u8) {
        let lvl = match p {
            Path::Alt(..) => 1,
            Path::Seq(..) => 2,
            Path::Inverse(_) => 3,
            Path::ZeroOrMore(_) | Path::OneOrMore(_) | Path::ZeroOrOne(_) => 4,
            Path::Iri(_) | Path::Nps(_) => 5,
        };
        let parens = lvl < min;
        if parens {
            self.out.push('(');
        }
        match p {
            Path::Alt(a, b) => {
                self.path_prec(a, 1);
                self.out.push('|');
                self.path_prec(b, 2);
            }
            Path::Seq(a, b) => {
                self.path_prec(a, 2);
                self.out.push('/');
                self.path_prec(b, 3);
            }
            Path::Inverse(x) => {
                self.out.push('^');
                self.path_prec(x, 4);
            }
            Path::ZeroOrMore(x) => {
                self.path_prec(x, 5);
                self.out.push('*');
            }
            Path::OneOrMore(x) => {
                self.path_prec(x, 5);
                self.out.push('+');
            }
            Path::ZeroOrOne(x) => {
                self.path_prec(x, 5);
                self.out.push('?');
            }
            Path::Iri(i) => self.path_iri(i),
            Path::Nps(entries) => {
                self.out.push_str("!(");
                for (i, (iri, inverse)) in entries.iter().enumerate() {
                    if i > 0 {
                        self.out.push('|');
                    }
                    if *inverse {
                        self.out.push('^');
                    }
                    self.path_iri(iri);
                }
                self.out.push(')');
            }
        }
        if parens {
            self.out.push(')');
        }
    }

    fn path_iri(&mut self, iri: &str) {
        if iri == vocab::RDF_TYPE {
            self.out.push('a');
        } else {
            self.iri(iri);
        }
    }

    // ------------------------------------------------------- expressions

    pub fn expr(&mut self, e: &Expr) {
        self.expr_prec(e, 1);
    }

    /// Grammar levels: `||` 1 < `&&` 2 < relational/`IN` 3 < `+ -` 4 <
    /// `* /` 5 < unary 6 < primary 7. Binary operators are left
    /// associative (right child requires one level higher); relational is
    /// non-associative (both sides require additive); unary operands must
    /// be primary (`!!x` is not grammatical — `!(!x)`).
    fn expr_prec(&mut self, e: &Expr, min: u8) {
        let lvl = match &*e.kind {
            ExprKind::Or(..) => 1,
            ExprKind::And(..) => 2,
            ExprKind::Cmp(..) | ExprKind::In { .. } => 3,
            ExprKind::Add(..) | ExprKind::Sub(..) => 4,
            ExprKind::Mul(..) | ExprKind::Div(..) => 5,
            ExprKind::Not(_) | ExprKind::UnaryMinus(_) | ExprKind::UnaryPlus(_) => 6,
            _ => 7,
        };
        let parens = lvl < min;
        if parens {
            self.out.push('(');
        }
        match &*e.kind {
            ExprKind::Or(a, b) => {
                self.expr_prec(a, 1);
                self.out.push_str(" || ");
                self.expr_prec(b, 2);
            }
            ExprKind::And(a, b) => {
                self.expr_prec(a, 2);
                self.out.push_str(" && ");
                self.expr_prec(b, 3);
            }
            ExprKind::Cmp(op, a, b) => {
                self.expr_prec(a, 4);
                self.out.push_str(match op {
                    CmpOp::Eq => " = ",
                    CmpOp::Ne => " != ",
                    CmpOp::Lt => " < ",
                    CmpOp::Le => " <= ",
                    CmpOp::Gt => " > ",
                    CmpOp::Ge => " >= ",
                });
                self.expr_prec(b, 4);
            }
            ExprKind::In {
                expr,
                list,
                negated,
            } => {
                self.expr_prec(expr, 4);
                self.out
                    .push_str(if *negated { " NOT IN (" } else { " IN (" });
                self.comma_exprs(list);
                self.out.push(')');
            }
            ExprKind::Add(a, b) => {
                self.expr_prec(a, 4);
                self.out.push_str(" + ");
                self.expr_prec(b, 5);
            }
            ExprKind::Sub(a, b) => {
                self.expr_prec(a, 4);
                self.out.push_str(" - ");
                self.expr_prec(b, 5);
            }
            ExprKind::Mul(a, b) => {
                self.expr_prec(a, 5);
                self.out.push_str(" * ");
                self.expr_prec(b, 6);
            }
            ExprKind::Div(a, b) => {
                self.expr_prec(a, 5);
                self.out.push_str(" / ");
                self.expr_prec(b, 6);
            }
            ExprKind::Not(x) => {
                self.out.push('!');
                self.expr_prec(x, 7);
            }
            ExprKind::UnaryMinus(x) => {
                self.unary_sign('-', x);
            }
            ExprKind::UnaryPlus(x) => {
                self.unary_sign('+', x);
            }
            ExprKind::Builtin(b, args) => {
                self.out.push_str(builtin_name(*b));
                self.out.push('(');
                self.comma_exprs(args);
                self.out.push(')');
            }
            ExprKind::Function {
                iri,
                args,
                distinct,
            } => {
                self.iri(iri);
                self.out.push('(');
                if *distinct {
                    self.out.push_str("DISTINCT ");
                }
                self.comma_exprs(args);
                self.out.push(')');
            }
            ExprKind::Exists(g) => {
                self.out.push_str("EXISTS ");
                self.group(g);
            }
            ExprKind::NotExists(g) => {
                self.out.push_str("NOT EXISTS ");
                self.group(g);
            }
            ExprKind::Aggregate {
                func,
                distinct,
                expr,
                separator,
            } => {
                self.out.push_str(match func {
                    Aggregate::Count => "COUNT",
                    Aggregate::Sum => "SUM",
                    Aggregate::Min => "MIN",
                    Aggregate::Max => "MAX",
                    Aggregate::Avg => "AVG",
                    Aggregate::Sample => "SAMPLE",
                    Aggregate::GroupConcat => "GROUP_CONCAT",
                });
                self.out.push('(');
                if *distinct {
                    self.out.push_str("DISTINCT ");
                }
                match expr {
                    Some(e) => self.expr_prec(e, 1),
                    None => self.out.push('*'),
                }
                if let Some(sep) = separator {
                    self.out.push_str("; SEPARATOR = ");
                    self.quoted(sep);
                }
                self.out.push(')');
            }
            ExprKind::Term(t) => self.term(t),
        }
        if parens {
            self.out.push(')');
        }
    }

    /// `-`/`+` glue guard: a numeric-shorthand operand would lex as a
    /// sign-attached literal (`-2` is one token), changing the tree — a
    /// space keeps the unary operator its own token.
    fn unary_sign(&mut self, sign: char, x: &Expr) {
        self.out.push(sign);
        if let ExprKind::Term(t) = &*x.kind {
            if let TermKind::Literal {
                kind: LiteralKind::Typed(dt),
                lexical,
            } = &t.kind
            {
                let bare = match dt.as_str() {
                    vocab::XSD_INTEGER => is_integer_lexical(lexical),
                    vocab::XSD_DECIMAL => is_decimal_lexical(lexical),
                    vocab::XSD_DOUBLE => is_double_lexical(lexical),
                    _ => false,
                };
                if bare {
                    self.out.push(' ');
                }
            }
        }
        self.expr_prec(x, 7);
    }

    fn comma_exprs(&mut self, list: &[Expr]) {
        for (i, e) in list.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.expr_prec(e, 1);
        }
    }

    // ---------------------------------------------------------- patterns
    //
    // Single-line skeleton (enough for EXISTS bodies and round-tripping;
    // §M13b owns the canonical multi-line layout and sugar
    // reconstruction).

    pub fn group(&mut self, g: &GroupPattern) {
        self.out.push('{');
        for el in &g.elements {
            self.out.push(' ');
            self.element(el);
        }
        self.out.push_str(" }");
    }

    fn element(&mut self, el: &GroupElement) {
        match el {
            GroupElement::Triples(ts) => {
                let sentences = self.run_sentences(ts);
                for (i, s) in sentences.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(" . ");
                    }
                    self.out.push_str(s);
                }
                self.out.push_str(" .");
            }
            GroupElement::Filter(e) => {
                self.out.push_str("FILTER(");
                self.expr(e);
                self.out.push(')');
            }
            GroupElement::Optional(g) => {
                self.out.push_str("OPTIONAL ");
                self.group(g);
            }
            GroupElement::Minus(g) => {
                self.out.push_str("MINUS ");
                self.group(g);
            }
            GroupElement::Union(gs) => {
                for (i, g) in gs.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(" UNION ");
                    }
                    self.group(g);
                }
            }
            GroupElement::Graph(t, g) => {
                self.out.push_str("GRAPH ");
                self.term(t);
                self.out.push(' ');
                self.group(g);
            }
            GroupElement::Service {
                silent,
                target,
                pattern,
            } => {
                self.out.push_str("SERVICE ");
                if *silent {
                    self.out.push_str("SILENT ");
                }
                self.term(target);
                self.out.push(' ');
                self.group(pattern);
            }
            GroupElement::Bind { expr, var, .. } => {
                self.out.push_str("BIND(");
                self.expr(expr);
                self.out.push_str(" AS ?");
                self.out.push_str(var);
                self.out.push(')');
            }
            GroupElement::Values(vb) => self.values(vb),
            // A subselect is always the sole element of its group — the
            // enclosing group printer supplies the braces.
            GroupElement::SubSelect(ss) => self.subselect(ss),
        }
    }

    pub fn values(&mut self, vb: &ValuesBlock) {
        self.out.push_str("VALUES (");
        for (i, v) in vb.vars.iter().enumerate() {
            if i > 0 {
                self.out.push(' ');
            }
            self.out.push('?');
            self.out.push_str(v);
        }
        self.out.push_str(") {");
        for row in &vb.rows {
            self.out.push_str(" (");
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    self.out.push(' ');
                }
                match cell {
                    Some(t) => self.term(t),
                    None => self.out.push_str("UNDEF"),
                }
            }
            self.out.push(')');
        }
        self.out.push_str(" }");
    }

    pub fn subselect(&mut self, ss: &SubSelect) {
        self.select_clause(&ss.select);
        self.out.push_str("WHERE ");
        self.group(&ss.pattern);
        self.modifiers(&ss.modifiers);
        if let Some(vb) = &ss.values {
            self.out.push(' ');
            self.values(vb);
        }
    }

    pub fn select_clause(&mut self, sc: &SelectClause) {
        self.out.push_str("SELECT ");
        if sc.distinct {
            self.out.push_str("DISTINCT ");
        }
        if sc.reduced {
            self.out.push_str("REDUCED ");
        }
        if sc.projection.is_empty() {
            self.out.push_str("* ");
        } else {
            for p in &sc.projection {
                match p {
                    Projection::Var(v) => {
                        self.out.push('?');
                        self.out.push_str(v);
                    }
                    Projection::Expr(e, v) => {
                        self.out.push('(');
                        self.expr(e);
                        self.out.push_str(" AS ?");
                        self.out.push_str(v);
                        self.out.push(')');
                    }
                }
                self.out.push(' ');
            }
        }
    }

    pub fn modifiers(&mut self, m: &SolutionModifiers) {
        if !m.group_by.is_empty() {
            self.out.push_str(" GROUP BY");
            for c in &m.group_by {
                self.out.push(' ');
                match c {
                    GroupCondition::Var(v) => {
                        self.out.push('?');
                        self.out.push_str(v);
                    }
                    GroupCondition::Expr(e, alias) => {
                        self.out.push('(');
                        self.expr(e);
                        if let Some(v) = alias {
                            self.out.push_str(" AS ?");
                            self.out.push_str(v);
                        }
                        self.out.push(')');
                    }
                }
            }
        }
        if !m.having.is_empty() {
            self.out.push_str(" HAVING");
            for h in &m.having {
                self.out.push('(');
                self.expr(h);
                self.out.push(')');
            }
        }
        if !m.order_by.is_empty() {
            self.out.push_str(" ORDER BY");
            for c in &m.order_by {
                self.out.push(' ');
                if c.descending {
                    self.out.push_str("DESC(");
                    self.expr(&c.expr);
                    self.out.push(')');
                } else if let ExprKind::Term(t) = &*c.expr.kind {
                    if matches!(t.kind, TermKind::Var(_)) {
                        self.term(t);
                    } else {
                        self.out.push_str("ASC(");
                        self.expr(&c.expr);
                        self.out.push(')');
                    }
                } else {
                    self.out.push_str("ASC(");
                    self.expr(&c.expr);
                    self.out.push(')');
                }
            }
        }
        if let Some(n) = m.limit {
            let _ = write!(self.out, " LIMIT {n}");
        }
        if let Some(n) = m.offset {
            let _ = write!(self.out, " OFFSET {n}");
        }
    }

    // ------------------------------------- sugar reconstruction (§M13b)

    /// One triples run → sentences (no trailing `.`): consecutive
    /// same-subject triples fold into `;`/`,` property lists, and
    /// parser-fresh nodes whose every occurrence lives in this run come
    /// back as the sugar that minted them — `( … )` collections from
    /// `rdf:first`/`rdf:rest` spines, `[ … ]` property lists elsewhere.
    /// Anything that deviates from the sugar shapes (multiply referenced,
    /// referenced inside a triple term or from another run) keeps its
    /// mapped label — the fallback is always semantics-preserving.
    fn run_sentences(&mut self, triples: &[TriplePattern]) -> Vec<String> {
        let mut rv = RunView::build(triples);
        let mut sentences = Vec::new();
        // Two passes: desugared triples precede their reference site in
        // parse order, so a first pass must leave inlinable fresh
        // subjects alone for the reference to consume; a defensive second
        // pass prints anything a malformed shape left behind.
        for defer_inlinable in [true, false] {
            let mut i = 0;
            while i < triples.len() {
                if rv.consumed[i] {
                    i += 1;
                    continue;
                }
                let subject = &triples[i].s;
                if let TermKind::BlankNode(label) = &subject.kind {
                    if is_fresh(label) {
                        if defer_inlinable && self.obj_inline_ok(&rv, triples, label) {
                            i += 1;
                            continue;
                        }
                        let idxs = rv.subj.get(label).cloned().unwrap_or_default();
                        if self.stmt_anon_ok(&rv, label) && self.span_clean(&rv, triples, &idxs) {
                            let s = self.stmt_fresh_sentence(triples, &mut rv, label);
                            sentences.push(s);
                            i += 1;
                            continue;
                        }
                    }
                }
                // Plain subject: fold every following consecutive
                // unconsumed triple with the same rendered subject into
                // one sentence.
                let subj = self.capture(|p| p.term(subject));
                let mut idxs = vec![i];
                let mut j = i + 1;
                while j < triples.len() {
                    if rv.consumed[j] {
                        j += 1;
                        continue;
                    }
                    // Deferred sugar triples sit between the sentences
                    // that reference them — transparent to folding.
                    if defer_inlinable {
                        if let TermKind::BlankNode(l) = &triples[j].s.kind {
                            if is_fresh(l) && self.obj_inline_ok(&rv, triples, l) {
                                j += 1;
                                continue;
                            }
                        }
                    }
                    if self.capture(|p| p.term(&triples[j].s)) == subj {
                        idxs.push(j);
                        j += 1;
                    } else {
                        break;
                    }
                }
                let props = self.prop_groups(triples, &mut rv, &idxs);
                sentences.push(format!("{subj} {props}"));
                i += 1;
            }
        }
        sentences
    }

    /// Statement-position fresh subject: `[ props ]`, `( items )`, or
    /// `( items ) props` when the list head carries extra properties.
    fn stmt_fresh_sentence(
        &mut self,
        triples: &[TriplePattern],
        rv: &mut RunView,
        label: &str,
    ) -> String {
        let idxs = rv.subj.get(label).cloned().unwrap_or_default();
        if let Some((fi, ri, extras)) = rv.head_split(triples, label) {
            if let Some(chain) = self.chain_from(triples, rv, fi, ri) {
                for &(f, r) in &chain {
                    rv.consumed[f] = true;
                    rv.consumed[r] = true;
                }
                let items = self.chain_items(triples, rv, &chain);
                return if extras.is_empty() {
                    format!("( {items} )")
                } else {
                    let props = self.prop_groups(triples, rv, &extras);
                    format!("( {items} ) {props}")
                };
            }
        }
        let props = self.prop_groups(triples, rv, &idxs);
        format!("[ {props} ]")
    }

    /// `p o ; p o , o` — renders (and consumes) the given triples as a
    /// property list, folding consecutive equal predicates into `,`.
    fn prop_groups(
        &mut self,
        triples: &[TriplePattern],
        rv: &mut RunView,
        idxs: &[usize],
    ) -> String {
        let mut out = String::new();
        let mut prev_verb: Option<String> = None;
        for &i in idxs {
            if rv.consumed[i] {
                continue;
            }
            rv.consumed[i] = true;
            let verb = self.capture(|p| p.verb(&triples[i].p));
            let object = self.pat_term(triples, rv, &triples[i].o);
            match &prev_verb {
                Some(v) if *v == verb => {
                    out.push_str(" , ");
                }
                Some(_) => {
                    out.push_str(" ; ");
                    out.push_str(&verb);
                    out.push(' ');
                }
                None => {
                    out.push_str(&verb);
                    out.push(' ');
                }
            }
            out.push_str(&object);
            prev_verb = Some(verb);
        }
        out
    }

    /// A term in pattern/template position: fresh nodes inline as their
    /// sugar when the shape allows, `rdf:nil` prints `()`.
    fn pat_term(&mut self, triples: &[TriplePattern], rv: &mut RunView, t: &Term) -> String {
        match &t.kind {
            TermKind::Iri(i) if i == vocab::RDF_NIL => "()".to_owned(),
            TermKind::BlankNode(label) if is_fresh(label) => {
                if self.obj_inline_ok(rv, triples, label) {
                    if let Some((fi, ri)) = rv.pure_pair(triples, label) {
                        if let Some(chain) = self.chain_from(triples, rv, fi, ri) {
                            for &(f, r) in &chain {
                                rv.consumed[f] = true;
                                rv.consumed[r] = true;
                            }
                            let items = self.chain_items(triples, rv, &chain);
                            return format!("( {items} )");
                        }
                    } else {
                        let idxs = rv.subj.get(label).cloned().unwrap_or_default();
                        if self.span_clean(rv, triples, &idxs) {
                            let props = self.prop_groups(triples, rv, &idxs);
                            if props.is_empty() {
                                return "[]".to_owned();
                            }
                            return format!("[ {props} ]");
                        }
                    }
                }
                self.capture(|p| p.term(t))
            }
            _ => self.capture(|p| p.term(t)),
        }
    }

    /// Occurrence-count half of [`Printer::obj_inline_ok`]: every
    /// occurrence is in this run, exactly one of them is a plain object
    /// position, and none sit inside a triple term.
    fn obj_inline_shape(&self, rv: &RunView, label: &str) -> bool {
        let local = rv.local.get(label).copied().unwrap_or(0);
        self.fresh.get(label).copied().unwrap_or(0) == local
            && rv.obj_at.get(label).copied().unwrap_or(0) == 1
            && local == rv.subj.get(label).map_or(0, Vec::len) + 1
    }

    /// [`Printer::obj_inline_shape`] plus the span guard: gathering the
    /// subject triples must not hoist them over a foreign statement.
    fn obj_inline_ok(&self, rv: &RunView, triples: &[TriplePattern], label: &str) -> bool {
        self.obj_inline_shape(rv, label)
            && rv
                .subj
                .get(label)
                .is_none_or(|idxs| self.span_clean(rv, triples, idxs))
    }

    /// Never referenced at all (its sole appearance is as a subject).
    fn stmt_anon_ok(&self, rv: &RunView, label: &str) -> bool {
        let local = rv.local.get(label).copied().unwrap_or(0);
        self.fresh.get(label).copied().unwrap_or(0) == local
            && rv.obj_at.get(label).copied().unwrap_or(0) == 0
            && local == rv.subj.get(label).map_or(0, Vec::len)
    }

    /// Gathering `idxs` into one construct must not hoist them over a
    /// foreign statement: every triple inside the index span has to
    /// belong to the construct or to sugar it will inline — otherwise the
    /// re-parse would see a different triple order than the original run
    /// (BGP order is semantics-free, but the printer promises tree-shape
    /// fidelity).
    fn span_clean(&self, rv: &RunView, triples: &[TriplePattern], idxs: &[usize]) -> bool {
        let (Some(&lo), Some(&hi)) = (idxs.iter().min(), idxs.iter().max()) else {
            return true;
        };
        for (j, t) in triples.iter().enumerate().take(hi + 1).skip(lo) {
            if idxs.contains(&j) {
                continue;
            }
            match &t.s.kind {
                TermKind::BlankNode(l) if is_fresh(l) && self.obj_inline_shape(rv, l) => {}
                _ => return false,
            }
        }
        true
    }

    /// Walk a `first`/`rest` spine starting from the given pair; every
    /// interior node must be a pure two-triple list node referenced only
    /// by its predecessor's `rest`. Returns the `(first, rest)` triple
    /// indices in list order.
    fn chain_from(
        &self,
        triples: &[TriplePattern],
        rv: &RunView,
        first: usize,
        rest: usize,
    ) -> Option<Vec<(usize, usize)>> {
        let mut chain = vec![(first, rest)];
        let mut tail = &triples[rest].o;
        loop {
            match &tail.kind {
                TermKind::Iri(i) if i == vocab::RDF_NIL => return Some(chain),
                TermKind::BlankNode(next) if is_fresh(next) => {
                    if !self.obj_inline_ok(rv, triples, next) || chain.len() > triples.len() {
                        return None;
                    }
                    let (fi, ri) = rv.pure_pair(triples, next)?;
                    chain.push((fi, ri));
                    tail = &triples[ri].o;
                }
                _ => return None,
            }
        }
    }

    fn chain_items(
        &mut self,
        triples: &[TriplePattern],
        rv: &mut RunView,
        chain: &[(usize, usize)],
    ) -> String {
        let items: Vec<String> = chain
            .iter()
            .map(|&(f, _)| self.pat_term(triples, rv, &triples[f].o))
            .collect();
        items.join(" ")
    }

    // --------------------------------------- multi-line layout (§M13b)

    /// Multi-line group at the given indent depth: `{`, one element per
    /// line one level deeper, `}` back at `depth`.
    pub fn group_ml(&mut self, g: &GroupPattern, depth: usize) {
        if g.elements.is_empty() {
            self.out.push_str("{ }");
            return;
        }
        self.out.push('{');
        for el in &g.elements {
            self.element_ml(el, depth + 1);
        }
        self.line(depth);
        self.out.push('}');
    }

    fn element_ml(&mut self, el: &GroupElement, depth: usize) {
        match el {
            GroupElement::Triples(ts) => {
                let sentences = self.run_sentences(ts);
                for s in sentences {
                    self.line(depth);
                    self.out.push_str(&s);
                    self.out.push_str(" .");
                }
            }
            GroupElement::Filter(e) => {
                self.line(depth);
                self.out.push_str("FILTER(");
                self.expr(e);
                self.out.push(')');
            }
            GroupElement::Optional(g) => {
                self.line(depth);
                self.out.push_str("OPTIONAL ");
                self.group_ml(g, depth);
            }
            GroupElement::Minus(g) => {
                self.line(depth);
                self.out.push_str("MINUS ");
                self.group_ml(g, depth);
            }
            GroupElement::Union(gs) => {
                self.line(depth);
                for (i, g) in gs.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(" UNION ");
                    }
                    self.group_ml(g, depth);
                }
            }
            GroupElement::Graph(t, g) => {
                self.line(depth);
                self.out.push_str("GRAPH ");
                self.term(t);
                self.out.push(' ');
                self.group_ml(g, depth);
            }
            GroupElement::Service {
                silent,
                target,
                pattern,
            } => {
                self.line(depth);
                self.out.push_str("SERVICE ");
                if *silent {
                    self.out.push_str("SILENT ");
                }
                self.term(target);
                self.out.push(' ');
                self.group_ml(pattern, depth);
            }
            GroupElement::Bind { expr, var, .. } => {
                self.line(depth);
                self.out.push_str("BIND(");
                self.expr(expr);
                self.out.push_str(" AS ?");
                self.out.push_str(var);
                self.out.push(')');
            }
            GroupElement::Values(vb) => {
                self.line(depth);
                self.values(vb);
            }
            // The enclosing group's braces are the subselect's braces.
            GroupElement::SubSelect(ss) => {
                self.line(depth);
                self.select_clause(&ss.select);
                self.trim_spaces();
                self.line(depth);
                self.out.push_str("WHERE ");
                self.group_ml(&ss.pattern, depth);
                self.modifier_lines(&ss.modifiers, depth);
                if let Some(vb) = &ss.values {
                    self.line(depth);
                    self.values(vb);
                }
            }
        }
    }

    /// Solution modifiers, one per line at `depth`.
    fn modifier_lines(&mut self, m: &SolutionModifiers, depth: usize) {
        if !m.group_by.is_empty() {
            self.line(depth);
            self.out.push_str("GROUP BY");
            for c in &m.group_by {
                self.out.push(' ');
                match c {
                    GroupCondition::Var(v) => {
                        self.out.push('?');
                        self.out.push_str(v);
                    }
                    GroupCondition::Expr(e, alias) => {
                        self.out.push('(');
                        self.expr(e);
                        if let Some(v) = alias {
                            self.out.push_str(" AS ?");
                            self.out.push_str(v);
                        }
                        self.out.push(')');
                    }
                }
            }
        }
        if !m.having.is_empty() {
            self.line(depth);
            self.out.push_str("HAVING");
            for (i, h) in m.having.iter().enumerate() {
                if i > 0 {
                    self.out.push(' ');
                }
                self.out.push('(');
                self.expr(h);
                self.out.push(')');
            }
        }
        if !m.order_by.is_empty() {
            self.line(depth);
            self.out.push_str("ORDER BY");
            for c in &m.order_by {
                self.out.push(' ');
                if c.descending {
                    self.out.push_str("DESC(");
                    self.expr(&c.expr);
                    self.out.push(')');
                } else if let ExprKind::Term(t) = &*c.expr.kind {
                    if matches!(t.kind, TermKind::Var(_)) {
                        self.term(t);
                    } else {
                        self.out.push_str("ASC(");
                        self.expr(&c.expr);
                        self.out.push(')');
                    }
                } else {
                    self.out.push_str("ASC(");
                    self.expr(&c.expr);
                    self.out.push(')');
                }
            }
        }
        if let Some(n) = m.limit {
            self.line(depth);
            let _ = write!(self.out, "LIMIT {n}");
        }
        if let Some(n) = m.offset {
            self.line(depth);
            let _ = write!(self.out, "OFFSET {n}");
        }
    }

    // -------------------------------------------- whole queries (§M13b)

    /// The query body (everything but the prologue — [`print_query`]
    /// assembles the document).
    pub fn query(&mut self, q: &Query) {
        match &q.form {
            QueryForm::Select(sc) => {
                self.select_clause(sc);
                self.trim_spaces();
            }
            QueryForm::Construct(template) => {
                // `CONSTRUCT WHERE { … }` short form: the parser fills the
                // template from the pattern's sole triples run, so the same
                // blank labels sit in both — printing the full form would
                // duplicate them across template and WHERE (which the
                // cross-BGP label rule rejects). Detect the clone and emit
                // the short form back.
                let short = match q.pattern.elements.as_slice() {
                    // Keep an explicitly empty template in the full form.
                    // `CONSTRUCT WHERE {}` is the short form whose WHERE
                    // triples are also its template; treating the empty
                    // full form as short changes the AST on reparse.
                    [] => false,
                    [GroupElement::Triples(ts)] => {
                        ts.len() == template.len()
                            && ts.iter().zip(template).all(|(a, b)| {
                                self.capture(|p| p.triple(a)) == self.capture(|p| p.triple(b))
                            })
                    }
                    _ => false,
                };
                if short {
                    self.out.push_str("CONSTRUCT");
                } else {
                    self.out.push_str("CONSTRUCT {");
                    let sentences = self.run_sentences(template);
                    for s in sentences {
                        self.line(1);
                        self.out.push_str(&s);
                        self.out.push_str(" .");
                    }
                    self.line(0);
                    self.out.push('}');
                }
            }
            QueryForm::Describe { targets, star } => {
                self.out.push_str("DESCRIBE");
                if *star {
                    self.out.push_str(" *");
                }
                for t in targets {
                    self.out.push(' ');
                    self.term(t);
                }
            }
            QueryForm::Ask => self.out.push_str("ASK"),
        }
        for d in &q.dataset {
            self.line(0);
            match d {
                DatasetClause::Default(g) => {
                    self.out.push_str("FROM ");
                    self.iri(g);
                }
                DatasetClause::Named(g) => {
                    self.out.push_str("FROM NAMED ");
                    self.iri(g);
                }
            }
        }
        let skip_where =
            matches!(&q.form, QueryForm::Describe { .. }) && q.pattern.elements.is_empty();
        if !skip_where {
            self.line(0);
            self.out.push_str("WHERE ");
            self.group_ml(&q.pattern, 0);
        }
        self.modifier_lines(&q.modifiers, 0);
        if let Some(vb) = &q.values {
            self.line(0);
            self.values(vb);
        }
    }

    /// The update body: operations separated by `;` lines.
    pub fn update(&mut self, u: &UpdateRequest) {
        for (i, op) in u.operations.iter().enumerate() {
            if i > 0 {
                self.line(0);
                self.out.push(';');
                self.line(0);
            }
            self.update_op(op);
        }
    }

    fn update_op(&mut self, op: &UpdateOp) {
        match op {
            UpdateOp::InsertData(quads) => {
                self.out.push_str("INSERT DATA ");
                self.quad_block(quads, 0);
            }
            UpdateOp::DeleteData(quads) => {
                self.out.push_str("DELETE DATA ");
                self.quad_block(quads, 0);
            }
            UpdateOp::DeleteWhere(quads) => {
                self.out.push_str("DELETE WHERE ");
                self.quad_block(quads, 0);
            }
            UpdateOp::Modify {
                with,
                delete,
                insert,
                using,
                pattern,
            } => {
                if let Some(g) = with {
                    self.out.push_str("WITH ");
                    self.iri(g);
                    self.line(0);
                }
                if let Some(quads) = delete {
                    self.out.push_str("DELETE ");
                    self.quad_block(quads, 0);
                    self.line(0);
                }
                if let Some(quads) = insert {
                    self.out.push_str("INSERT ");
                    self.quad_block(quads, 0);
                    self.line(0);
                }
                for d in using {
                    match d {
                        DatasetClause::Default(g) => {
                            self.out.push_str("USING ");
                            self.iri(g);
                        }
                        DatasetClause::Named(g) => {
                            self.out.push_str("USING NAMED ");
                            self.iri(g);
                        }
                    }
                    self.line(0);
                }
                self.out.push_str("WHERE ");
                self.group_ml(pattern, 0);
            }
            UpdateOp::Load {
                silent,
                source,
                into,
            } => {
                self.out.push_str("LOAD ");
                if *silent {
                    self.out.push_str("SILENT ");
                }
                self.iri(source);
                if let Some(g) = into {
                    self.out.push_str(" INTO GRAPH ");
                    self.iri(g);
                }
            }
            UpdateOp::Clear { silent, target } => {
                self.out.push_str("CLEAR ");
                self.graph_target(*silent, target);
            }
            UpdateOp::Drop { silent, target } => {
                self.out.push_str("DROP ");
                self.graph_target(*silent, target);
            }
            UpdateOp::Create { silent, graph } => {
                self.out.push_str("CREATE ");
                if *silent {
                    self.out.push_str("SILENT ");
                }
                self.out.push_str("GRAPH ");
                self.iri(graph);
            }
            UpdateOp::Add { silent, from, to } => self.graph_pair_op("ADD", *silent, from, to),
            UpdateOp::Move { silent, from, to } => self.graph_pair_op("MOVE", *silent, from, to),
            UpdateOp::Copy { silent, from, to } => self.graph_pair_op("COPY", *silent, from, to),
        }
    }

    fn graph_target(&mut self, silent: bool, t: &GraphTarget) {
        if silent {
            self.out.push_str("SILENT ");
        }
        match t {
            GraphTarget::Graph(g) => {
                self.out.push_str("GRAPH ");
                self.iri(g);
            }
            GraphTarget::Default => self.out.push_str("DEFAULT"),
            GraphTarget::Named => self.out.push_str("NAMED"),
            GraphTarget::All => self.out.push_str("ALL"),
        }
    }

    fn graph_pair_op(
        &mut self,
        kw: &str,
        silent: bool,
        from: &GraphOrDefault,
        to: &GraphOrDefault,
    ) {
        self.out.push_str(kw);
        self.out.push(' ');
        if silent {
            self.out.push_str("SILENT ");
        }
        self.graph_or_default(from);
        self.out.push_str(" TO ");
        self.graph_or_default(to);
    }

    fn graph_or_default(&mut self, g: &GraphOrDefault) {
        match g {
            GraphOrDefault::Default => self.out.push_str("DEFAULT"),
            GraphOrDefault::Graph(g) => {
                self.out.push_str("GRAPH ");
                self.iri(g);
            }
        }
    }

    /// `{ … }` of quads: consecutive same-graph quads share one `GRAPH`
    /// wrapper (or none, for the default graph), each run getting the
    /// full sugar reconstruction.
    fn quad_block(&mut self, quads: &[Quad], depth: usize) {
        if quads.is_empty() {
            self.out.push_str("{ }");
            return;
        }
        self.out.push('{');
        let mut i = 0;
        while i < quads.len() {
            let key = quads[i].graph.as_ref().map(|g| self.capture(|p| p.term(g)));
            let mut j = i + 1;
            while j < quads.len() {
                let k = quads[j].graph.as_ref().map(|g| self.capture(|p| p.term(g)));
                if k == key {
                    j += 1;
                } else {
                    break;
                }
            }
            let run: Vec<TriplePattern> = quads[i..j].iter().map(|q| q.triple.clone()).collect();
            match &key {
                None => {
                    let sentences = self.run_sentences(&run);
                    for s in sentences {
                        self.line(depth + 1);
                        self.out.push_str(&s);
                        self.out.push_str(" .");
                    }
                }
                Some(g) => {
                    self.line(depth + 1);
                    self.out.push_str("GRAPH ");
                    self.out.push_str(g);
                    self.out.push_str(" {");
                    let sentences = self.run_sentences(&run);
                    for s in sentences {
                        self.line(depth + 2);
                        self.out.push_str(&s);
                        self.out.push_str(" .");
                    }
                    self.line(depth + 1);
                    self.out.push('}');
                }
            }
            i = j;
        }
        self.line(depth);
        self.out.push('}');
    }

    // ------------------------------------------ fresh-label pre-scans

    /// Count every occurrence of parser-fresh labels in the unit about to
    /// print, so runs can prove they own a label before consuming it.
    pub fn count_query(&mut self, q: &Query) {
        match &q.form {
            QueryForm::Select(sc) => {
                for p in &sc.projection {
                    if let Projection::Expr(e, _) = p {
                        self.count_expr(e);
                    }
                }
            }
            QueryForm::Construct(template) => {
                for t in template {
                    self.count_triple(t);
                }
            }
            QueryForm::Describe { targets, .. } => {
                for t in targets {
                    self.count_term(t);
                }
            }
            QueryForm::Ask => {}
        }
        self.count_group(&q.pattern);
        self.count_modifiers(&q.modifiers);
    }

    pub fn count_update(&mut self, u: &UpdateRequest) {
        for op in &u.operations {
            match op {
                UpdateOp::InsertData(quads)
                | UpdateOp::DeleteData(quads)
                | UpdateOp::DeleteWhere(quads) => {
                    for q in quads {
                        self.count_triple(&q.triple);
                    }
                }
                UpdateOp::Modify {
                    delete,
                    insert,
                    pattern,
                    ..
                } => {
                    for quads in [delete, insert].into_iter().flatten() {
                        for q in quads {
                            self.count_triple(&q.triple);
                        }
                    }
                    self.count_group(pattern);
                }
                _ => {}
            }
        }
    }

    fn count_group(&mut self, g: &GroupPattern) {
        for el in &g.elements {
            match el {
                GroupElement::Triples(ts) => {
                    for t in ts {
                        self.count_triple(t);
                    }
                }
                GroupElement::Filter(e) => self.count_expr(e),
                GroupElement::Optional(g) | GroupElement::Minus(g) => self.count_group(g),
                GroupElement::Union(gs) => {
                    for g in gs {
                        self.count_group(g);
                    }
                }
                GroupElement::Graph(t, g) => {
                    self.count_term(t);
                    self.count_group(g);
                }
                GroupElement::Service {
                    target, pattern, ..
                } => {
                    self.count_term(target);
                    self.count_group(pattern);
                }
                GroupElement::Bind { expr, .. } => self.count_expr(expr),
                GroupElement::Values(_) => {}
                GroupElement::SubSelect(ss) => {
                    for p in &ss.select.projection {
                        if let Projection::Expr(e, _) = p {
                            self.count_expr(e);
                        }
                    }
                    self.count_group(&ss.pattern);
                    self.count_modifiers(&ss.modifiers);
                }
            }
        }
    }

    fn count_modifiers(&mut self, m: &SolutionModifiers) {
        for c in &m.group_by {
            if let GroupCondition::Expr(e, _) = c {
                self.count_expr(e);
            }
        }
        for e in &m.having {
            self.count_expr(e);
        }
        for c in &m.order_by {
            self.count_expr(&c.expr);
        }
    }

    fn count_expr(&mut self, e: &Expr) {
        match &*e.kind {
            ExprKind::Or(a, b)
            | ExprKind::And(a, b)
            | ExprKind::Cmp(_, a, b)
            | ExprKind::Add(a, b)
            | ExprKind::Sub(a, b)
            | ExprKind::Mul(a, b)
            | ExprKind::Div(a, b) => {
                self.count_expr(a);
                self.count_expr(b);
            }
            ExprKind::In { expr, list, .. } => {
                self.count_expr(expr);
                for e in list {
                    self.count_expr(e);
                }
            }
            ExprKind::Not(x) | ExprKind::UnaryMinus(x) | ExprKind::UnaryPlus(x) => {
                self.count_expr(x);
            }
            ExprKind::Builtin(_, args) | ExprKind::Function { args, .. } => {
                for e in args {
                    self.count_expr(e);
                }
            }
            ExprKind::Exists(g) | ExprKind::NotExists(g) => self.count_group(g),
            ExprKind::Aggregate { expr, .. } => {
                if let Some(e) = expr {
                    self.count_expr(e);
                }
            }
            ExprKind::Term(t) => self.count_term(t),
        }
    }

    fn count_triple(&mut self, t: &TriplePattern) {
        self.count_term(&t.s);
        if let Verb::Term(v) = &t.p {
            self.count_term(v);
        }
        self.count_term(&t.o);
    }

    fn count_term(&mut self, t: &Term) {
        match &t.kind {
            TermKind::BlankNode(l) if is_fresh(l) => {
                *self.fresh.entry(l.clone()).or_insert(0) += 1;
            }
            TermKind::BlankNode(l) => {
                self.taken.insert(l.clone());
            }
            TermKind::TripleTerm(tp) => self.count_triple(tp),
            _ => {}
        }
    }
}

fn is_fresh(label: &str) -> bool {
    label.starts_with('.')
}

/// Per-run occurrence index over one triples run (see
/// [`Printer::run_sentences`]).
#[derive(Debug)]
struct RunView {
    /// Fresh label → indices of triples with it as subject, in order.
    subj: std::collections::HashMap<String, Vec<usize>>,
    /// Fresh label → count of plain (non-triple-term) object positions.
    obj_at: std::collections::HashMap<String, usize>,
    /// Fresh label → total occurrences in this run (subject + object +
    /// inside triple terms).
    local: std::collections::HashMap<String, usize>,
    consumed: Vec<bool>,
}

impl RunView {
    fn build(triples: &[TriplePattern]) -> RunView {
        let mut rv = RunView {
            subj: std::collections::HashMap::new(),
            obj_at: std::collections::HashMap::new(),
            local: std::collections::HashMap::new(),
            consumed: vec![false; triples.len()],
        };
        for (i, t) in triples.iter().enumerate() {
            if let TermKind::BlankNode(l) = &t.s.kind {
                if is_fresh(l) {
                    rv.subj.entry(l.clone()).or_default().push(i);
                    *rv.local.entry(l.clone()).or_insert(0) += 1;
                }
            }
            rv.count_object(&t.o, true);
        }
        rv
    }

    fn count_object(&mut self, t: &Term, top: bool) {
        match &t.kind {
            TermKind::BlankNode(l) if is_fresh(l) => {
                *self.local.entry(l.clone()).or_insert(0) += 1;
                if top {
                    *self.obj_at.entry(l.clone()).or_insert(0) += 1;
                }
            }
            TermKind::TripleTerm(tp) => {
                // Triple-term interiors reference but can never host
                // sugar — count as non-inlinable occurrences.
                if let TermKind::BlankNode(l) = &tp.s.kind {
                    if is_fresh(l) {
                        *self.local.entry(l.clone()).or_insert(0) += 1;
                    }
                }
                self.count_object(&tp.o, false);
            }
            _ => {}
        }
    }

    /// The label's subject triples are exactly one `rdf:first` and one
    /// `rdf:rest` — a pure interior list node.
    fn pure_pair(&self, triples: &[TriplePattern], label: &str) -> Option<(usize, usize)> {
        let idxs = self.subj.get(label)?;
        if idxs.len() != 2 {
            return None;
        }
        self.split_first_rest(triples, idxs)
            .and_then(|(f, r, extras)| {
                if extras.is_empty() {
                    Some((f, r))
                } else {
                    None
                }
            })
    }

    /// Exactly one `rdf:first` and one `rdf:rest` among the label's
    /// subject triples; anything else is returned as `extras` (a
    /// statement-position list head may carry its own properties).
    fn head_split(
        &self,
        triples: &[TriplePattern],
        label: &str,
    ) -> Option<(usize, usize, Vec<usize>)> {
        let idxs = self.subj.get(label)?;
        self.split_first_rest(triples, idxs)
    }

    fn split_first_rest(
        &self,
        triples: &[TriplePattern],
        idxs: &[usize],
    ) -> Option<(usize, usize, Vec<usize>)> {
        let (mut first, mut rest, mut extras) = (None, None, Vec::new());
        for &i in idxs {
            match &triples[i].p {
                Verb::Term(t) => match &t.kind {
                    TermKind::Iri(p) if p == vocab::RDF_FIRST && first.is_none() => {
                        first = Some(i);
                    }
                    TermKind::Iri(p) if p == vocab::RDF_REST && rest.is_none() => rest = Some(i),
                    _ => extras.push(i),
                },
                Verb::Path(_) => extras.push(i),
            }
        }
        Some((first?, rest?, extras))
    }
}

// ------------------------------------------------------------ lexical aid

/// Render an IRI tail as an escaped `PN_LOCAL`, or `None` when a
/// character is neither legal raw at its position nor `PN_LOCAL_ESC`-
/// escapable. `%HH` passes through as `PERCENT` (SPARQL does not decode
/// it — the pname denotes the IRI with the `%HH` verbatim); a bare `%`
/// escapes as `\%`.
fn pname_local(rest: &str) -> Option<String> {
    let chars: Vec<char> = rest.chars().collect();
    let mut out = String::with_capacity(rest.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let first = i == 0;
        let last = i == chars.len() - 1;
        // PERCENT: only when a full %HH triple is present.
        if c == '%'
            && i + 2 < chars.len()
            && chars[i + 1].is_ascii_hexdigit()
            && chars[i + 2].is_ascii_hexdigit()
        {
            out.push('%');
            out.push(chars[i + 1]);
            out.push(chars[i + 2]);
            i += 3;
            continue;
        }
        let raw_ok = if first {
            is_pn_chars_u(c) || c == ':' || c.is_ascii_digit()
        } else if last {
            is_pn_chars(c) || c == ':'
        } else {
            is_pn_chars(c) || c == ':' || c == '.'
        };
        if raw_ok {
            out.push(c);
        } else if c.is_ascii() && is_pn_local_esc(c as u8) {
            out.push('\\');
            out.push(c);
        } else {
            return None;
        }
        i += 1;
    }
    Some(out)
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn strip_sign(s: &str) -> &str {
    s.strip_prefix(['+', '-']).unwrap_or(s)
}

/// `INTEGER` (with optional sign — sign-attached numerics lex as one
/// token, so signed lexicals round-trip bare).
fn is_integer_lexical(s: &str) -> bool {
    digits(strip_sign(s))
}

/// `DECIMAL`: `[0-9]* '.' [0-9]+` — a trailing-dot lexical like `1.`
/// is not expressible bare and keeps its typed form.
fn is_decimal_lexical(s: &str) -> bool {
    let s = strip_sign(s);
    match s.split_once('.') {
        Some((int, frac)) => (int.is_empty() || digits(int)) && digits(frac) && !frac.contains('.'),
        None => false,
    }
}

/// `DOUBLE`: mantissa (with at most one dot, at least one digit) and a
/// mandatory exponent.
fn is_double_lexical(s: &str) -> bool {
    let s = strip_sign(s);
    let Some(epos) = s.find(['e', 'E']) else {
        return false;
    };
    let (mantissa, exp) = (&s[..epos], &s[epos + 1..]);
    if !digits(strip_sign(exp)) {
        return false;
    }
    match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) && (frac.is_empty() || digits(frac))) || (int.is_empty() && digits(frac))
        }
        None => digits(mantissa),
    }
}

fn builtin_name(b: Builtin) -> &'static str {
    match b {
        Builtin::Str => "STR",
        Builtin::Lang => "LANG",
        Builtin::LangMatches => "LANGMATCHES",
        Builtin::Datatype => "DATATYPE",
        Builtin::Bound => "BOUND",
        Builtin::Iri => "IRI",
        Builtin::BNode => "BNODE",
        Builtin::Rand => "RAND",
        Builtin::Abs => "ABS",
        Builtin::Ceil => "CEIL",
        Builtin::Floor => "FLOOR",
        Builtin::Round => "ROUND",
        Builtin::Concat => "CONCAT",
        Builtin::StrLen => "STRLEN",
        Builtin::UCase => "UCASE",
        Builtin::LCase => "LCASE",
        Builtin::EncodeForUri => "ENCODE_FOR_URI",
        Builtin::Contains => "CONTAINS",
        Builtin::StrStarts => "STRSTARTS",
        Builtin::StrEnds => "STRENDS",
        Builtin::StrBefore => "STRBEFORE",
        Builtin::StrAfter => "STRAFTER",
        Builtin::Year => "YEAR",
        Builtin::Month => "MONTH",
        Builtin::Day => "DAY",
        Builtin::Hours => "HOURS",
        Builtin::Minutes => "MINUTES",
        Builtin::Seconds => "SECONDS",
        Builtin::Timezone => "TIMEZONE",
        Builtin::Tz => "TZ",
        Builtin::Now => "NOW",
        Builtin::Uuid => "UUID",
        Builtin::StrUuid => "STRUUID",
        Builtin::Md5 => "MD5",
        Builtin::Sha1 => "SHA1",
        Builtin::Sha256 => "SHA256",
        Builtin::Sha384 => "SHA384",
        Builtin::Sha512 => "SHA512",
        Builtin::Coalesce => "COALESCE",
        Builtin::If => "IF",
        Builtin::StrLang => "STRLANG",
        Builtin::StrDt => "STRDT",
        Builtin::SameTerm => "sameTerm",
        Builtin::IsIri => "isIRI",
        Builtin::IsBlank => "isBLANK",
        Builtin::IsLiteral => "isLITERAL",
        Builtin::IsNumeric => "isNUMERIC",
        Builtin::Regex => "REGEX",
        Builtin::Substr => "SUBSTR",
        Builtin::Replace => "REPLACE",
        Builtin::Triple => "TRIPLE",
        Builtin::Subject => "SUBJECT",
        Builtin::Predicate => "PREDICATE",
        Builtin::Object => "OBJECT",
        Builtin::IsTriple => "isTRIPLE",
        Builtin::LangDir => "LANGDIR",
        Builtin::HasLang => "hasLANG",
        Builtin::HasLangDir => "hasLANGDIR",
        Builtin::StrLangDir => "STRLANGDIR",
    }
}
