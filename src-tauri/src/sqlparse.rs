//! SQL analysis for the editor (completion + inspections), built on `sqlparser`.
//!
//! For each statement it reports the parse error (if any) and, from the syntax tree, the
//! tables it references (with aliases), the CTEs / derived tables it defines (with their
//! output columns) and the qualified column references (`alias.column`), all with positions
//! relative to the statement text (1-based lines and columns).

use serde::Serialize;
use sqlparser::ast::{Expr, ObjectName, ObjectNamePart, Query, SelectItem, SetExpr, Statement, TableFactor, Visit, Visitor};
use sqlparser::dialect::{
    ClickHouseDialect, Dialect, GenericDialect, MsSqlDialect, MySqlDialect, OracleDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Span;
use std::collections::HashSet;
use std::ops::ControlFlow;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Analysis {
    pub error: Option<ParseError>,
    pub tables: Vec<TableRef>,
    pub derived: Vec<Derived>,
    pub columns: Vec<ColumnRef>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParseError {
    pub message: String,
    pub line: u64,
    pub column: u64,
}

#[derive(Debug, Serialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "camelCase")]
pub struct Pos {
    pub line: u64,
    pub column: u64,
    pub end_line: u64,
    /// Exclusive.
    pub end_column: u64,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TableRef {
    /// Name as written, unquoted, parts joined with `.`.
    pub name: String,
    pub alias: Option<String>,
    /// The name refers to a CTE of the statement.
    pub cte: bool,
    pub pos: Pos,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Derived {
    /// CTE name or subquery alias.
    pub name: String,
    /// "cte" | "subquery"
    pub kind: &'static str,
    /// Output columns that have a name (`expr AS x`, plain or qualified column references).
    pub columns: Vec<String>,
    /// The projection has `*` / `t.*`: its columns also include those of `sources`.
    pub star: bool,
    /// Tables in the defining query's own FROM clause (for expanding `*`).
    pub sources: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ColumnRef {
    pub qualifier: String,
    pub name: String,
    pub pos: Pos,
}

fn dialect_for(name: &str) -> Box<dyn Dialect> {
    match name {
        "postgres" => Box::new(PostgreSqlDialect {}),
        "mysql" => Box::new(MySqlDialect {}),
        "sqlite" => Box::new(SQLiteDialect {}),
        "mssql" => Box::new(MsSqlDialect {}),
        "oracle" => Box::new(OracleDialect {}),
        "clickhouse" => Box::new(ClickHouseDialect {}),
        _ => Box::new(GenericDialect {}),
    }
}

fn pos(span: Span) -> Pos {
    Pos { line: span.start.line, column: span.start.column, end_line: span.end.line, end_column: span.end.column }
}

fn name_parts(n: &ObjectName) -> Vec<&sqlparser::ast::Ident> {
    n.0.iter()
        .filter_map(|p| match p {
            ObjectNamePart::Identifier(i) => Some(i),
            _ => None,
        })
        .collect()
}

fn name_text(n: &ObjectName) -> String {
    name_parts(n).iter().map(|i| i.value.as_str()).collect::<Vec<_>>().join(".")
}

fn name_pos(n: &ObjectName) -> Pos {
    let parts = name_parts(n);
    match (parts.first(), parts.last()) {
        (Some(a), Some(b)) => pos(Span::new(a.span.start, b.span.end)),
        _ => Pos::default(),
    }
}

/// Output column names of a query (first SELECT of a set operation decides, as in SQL).
fn projection(q: &Query) -> (Vec<String>, bool, Vec<String>) {
    let mut body = q.body.as_ref();
    loop {
        match body {
            SetExpr::SetOperation { left, .. } => body = left,
            SetExpr::Query(q) => body = q.body.as_ref(),
            _ => break,
        }
    }
    let SetExpr::Select(sel) = body else { return (vec![], false, vec![]) };
    let mut cols = vec![];
    let mut star = false;
    for item in &sel.projection {
        match item {
            SelectItem::UnnamedExpr(Expr::Identifier(i)) => cols.push(i.value.clone()),
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(ids)) => cols.extend(ids.last().map(|i| i.value.clone())),
            SelectItem::ExprWithAlias { alias, .. } => cols.push(alias.value.clone()),
            SelectItem::ExprWithAliases { aliases, .. } => cols.extend(aliases.iter().map(|a| a.value.clone())),
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => star = true,
            _ => {}
        }
    }
    let mut sources = vec![];
    for t in &sel.from {
        for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
            if let TableFactor::Table { name, args: None, .. } = f {
                sources.push(name_text(name));
            }
        }
    }
    (cols, star, sources)
}

#[derive(Default)]
struct Collector {
    out: Analysis,
    ctes: HashSet<String>,
    /// Start positions of names already recorded through their table factor.
    seen: HashSet<(u64, u64)>,
}

impl Collector {
    /// `function`: a table-valued function call (`generate_series(1, 9)`) — not a table, but its
    /// name is still marked as seen so the relation visit does not report it.
    fn push_table(&mut self, name: &ObjectName, alias: Option<String>, function: bool) {
        let p = name_pos(name);
        // Names without identifier parts have no position (and nothing to check).
        if p.line == 0 || !self.seen.insert((p.line, p.column)) || function {
            return;
        }
        let text = name_text(name);
        let cte = !text.contains('.') && self.ctes.contains(&text.to_lowercase());
        self.out.tables.push(TableRef { name: text, alias, cte, pos: p });
    }
}

impl Visitor for Collector {
    type Break = ();

    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
        if let Some(with) = &q.with {
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.clone();
                self.ctes.insert(name.to_lowercase());
                let (mut columns, star, sources) = projection(&cte.query);
                if !cte.alias.columns.is_empty() {
                    columns = cte.alias.columns.iter().map(|c| c.name.value.clone()).collect();
                }
                self.out.derived.push(Derived { name, kind: "cte", columns, star: star && cte.alias.columns.is_empty(), sources });
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, f: &TableFactor) -> ControlFlow<()> {
        match f {
            TableFactor::Table { name, alias, args, .. } => self.push_table(name, alias.as_ref().map(|a| a.name.value.clone()), args.is_some()),
            TableFactor::Derived { subquery, alias: Some(alias), .. } => {
                let (mut columns, star, sources) = projection(subquery);
                if !alias.columns.is_empty() {
                    columns = alias.columns.iter().map(|c| c.name.value.clone()).collect();
                }
                self.out.derived.push(Derived {
                    name: alias.name.value.clone(),
                    kind: "subquery",
                    columns,
                    star: star && alias.columns.is_empty(),
                    sources,
                });
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// INSERT INTO / DELETE FROM … names that are not table factors.
    fn pre_visit_relation(&mut self, r: &ObjectName) -> ControlFlow<()> {
        self.push_table(r, None, false);
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        if let Expr::CompoundIdentifier(ids) = e {
            if ids.len() >= 2 {
                let (q, c) = (&ids[ids.len() - 2], &ids[ids.len() - 1]);
                self.out.columns.push(ColumnRef { qualifier: q.value.clone(), name: c.value.clone(), pos: pos(c.span) });
            }
        }
        ControlFlow::Continue(())
    }
}

/// "Expected: …, found: x at Line: 1, Column: 16" → message + position.
fn split_location(msg: &str) -> (String, u64, u64) {
    if let Some(i) = msg.rfind(" at Line: ") {
        let loc = &msg[i + " at Line: ".len()..];
        if let Some((l, c)) = loc.split_once(", Column: ") {
            if let (Ok(l), Ok(c)) = (l.trim().parse(), c.trim().parse()) {
                return (msg[..i].to_string(), l, c);
            }
        }
    }
    (msg.to_string(), 1, 1)
}

pub fn analyze(dialect: &str, sql: &str) -> Analysis {
    let d = dialect_for(dialect);
    let stmts: Vec<Statement> = match Parser::parse_sql(d.as_ref(), sql) {
        Ok(s) => s,
        Err(e) => {
            let text = e.to_string();
            let text = text.strip_prefix("sql parser error: ").unwrap_or(&text);
            let (message, line, column) = split_location(text);
            return Analysis { error: Some(ParseError { message, line, column }), ..Default::default() };
        }
    };
    let mut c = Collector::default();
    for s in &stmts {
        let _ = s.visit(&mut c);
    }
    // CTEs are collected while walking, so references seen before their WITH get fixed here.
    for t in &mut c.out.tables {
        t.cte = !t.name.contains('.') && c.ctes.contains(&t.name.to_lowercase());
    }
    c.out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_aliases_and_ctes() {
        let a = analyze(
            "postgres",
            "WITH recent AS (SELECT o.id, o.customer_id AS cid, total FROM orders o)\n\
             SELECT r.cid, c.name, x.n FROM recent r JOIN public.customers c ON c.id = r.cid\n\
             CROSS JOIN (SELECT count(*) AS n, * FROM items) x",
        );
        assert!(a.error.is_none());
        let t: Vec<(&str, Option<&str>, bool)> = a.tables.iter().map(|t| (t.name.as_str(), t.alias.as_deref(), t.cte)).collect();
        assert!(t.contains(&("orders", Some("o"), false)));
        assert!(t.contains(&("recent", Some("r"), true)));
        assert!(t.contains(&("public.customers", Some("c"), false)));
        let recent = a.derived.iter().find(|d| d.name == "recent").unwrap();
        assert_eq!(recent.columns, vec!["id", "cid", "total"]);
        let x = a.derived.iter().find(|d| d.name == "x").unwrap();
        assert_eq!((x.kind, x.columns.clone(), x.star, x.sources.clone()), ("subquery", vec!["n".to_string()], true, vec!["items".to_string()]));
        let c = a.columns.iter().find(|c| c.qualifier == "c" && c.name == "name").unwrap();
        assert_eq!((c.pos.line, c.pos.column, c.pos.end_column), (2, 17, 21));
        let cust = a.tables.iter().find(|t| t.name == "public.customers").unwrap();
        assert_eq!((cust.pos.line, cust.pos.column, cust.pos.end_column), (2, 46, 62));
    }

    #[test]
    fn dml_tables_and_errors() {
        let a = analyze("mysql", "INSERT INTO logs (a) SELECT b FROM src s WHERE s.x = 1");
        let names: Vec<&str> = a.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["logs", "src"]);
        let a = analyze("postgres", "SELECT g FROM generate_series(1, 3) g JOIN t ON t.id = g");
        let names: Vec<&str> = a.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["t"]);
        let a = analyze("postgres", "SELECT a FROM t WHERE");
        let e = a.error.unwrap();
        assert_eq!(e.line, 1);
        assert!(e.message.contains("Expected"), "{}", e.message);
        let a = analyze("postgres", "SELECT * FROM t WHERE x = 'abc");
        assert!(a.error.unwrap().message.contains("Unterminated string"));
    }
}
