//! Structured explain plans (Oracle PLAN_TABLE, PostgreSQL EXPLAIN JSON) for the tree view.

use crate::drivers::{lit, CollectSink, Dialect, Session};
use crate::model::Cell;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    pub id: i64,
    pub parent_id: Option<i64>,
    /// e.g. "TABLE ACCESS", "Seq Scan"
    pub operation: String,
    /// e.g. "FULL", "BY INDEX ROWID", "Left"
    pub options: String,
    /// "TABLE NES_2007.ADM_OPERATION" / "INDEX HR.EMP_PK"
    pub object: String,
    pub cost: Option<f64>,
    pub bytes: Option<f64>,
    pub cardinality: Option<f64>,
    pub time: Option<String>,
    pub optimizer: String,
    pub access_predicates: String,
    pub filter_predicates: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub nodes: Vec<PlanNode>,
    /// Raw text form (DBMS_XPLAN / EXPLAIN text) for copying.
    pub text: String,
}

async fn rows(s: &dyn Session, sql: &str) -> Result<Vec<Vec<Cell>>> {
    let mut sink = CollectSink::new(0);
    s.run(sql, &mut sink).await?;
    Ok(sink.rows)
}

fn txt(c: Option<&Cell>) -> String {
    c.map(Cell::to_text).unwrap_or_default()
}

fn num(c: Option<&Cell>) -> Option<f64> {
    match c? {
        Cell::Null => None,
        other => other.to_text().parse().ok(),
    }
}

fn strip(sql: &str) -> &str {
    sql.trim().trim_end_matches('/').trim_end().trim_end_matches(';').trim_end()
}

pub async fn explain(s: &dyn Session, sql: &str) -> Result<Plan> {
    match s.dialect() {
        Dialect::Oracle => oracle(s, sql).await,
        Dialect::Postgres => postgres(s, sql).await,
        d => bail!("Tree explain plan is not available for {d:?}"),
    }
}

async fn oracle(s: &dyn Session, sql: &str) -> Result<Plan> {
    let sid = format!("DBF{}", &uuid::Uuid::new_v4().simple().to_string()[..20]);
    let q = lit(&sid);
    s.exec(&format!("EXPLAIN PLAN SET STATEMENT_ID = {q} INTO plan_table FOR {}", strip(sql))).await?;
    let result = async {
        let r = rows(
            s,
            &format!(
                "select id, parent_id, operation, options, object_type, object_owner, object_name, cost, bytes, cardinality,
                        time, optimizer, access_predicates, filter_predicates
                 from plan_table where statement_id = {q} order by id"
            ),
        )
        .await?;
        let nodes = r
            .iter()
            .map(|r| {
                let (otype, owner, name) = (txt(r.get(4)), txt(r.get(5)), txt(r.get(6)));
                let object = match (owner.is_empty(), name.is_empty()) {
                    (_, true) => String::new(),
                    (true, false) => name,
                    (false, false) => format!("{owner}.{name}"),
                };
                let otype = otype.split_whitespace().next().unwrap_or("").to_string();
                PlanNode {
                    id: num(r.first()).unwrap_or(0.0) as i64,
                    parent_id: num(r.get(1)).map(|v| v as i64),
                    operation: txt(r.get(2)),
                    options: txt(r.get(3)),
                    object: if object.is_empty() { object } else { format!("{otype} {object}").trim().to_string() },
                    cost: num(r.get(7)),
                    bytes: num(r.get(8)),
                    cardinality: num(r.get(9)),
                    time: num(r.get(10)).filter(|t| *t > 0.0).map(|t| {
                        let t = t as i64;
                        format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
                    }),
                    optimizer: txt(r.get(11)),
                    access_predicates: txt(r.get(12)),
                    filter_predicates: txt(r.get(13)),
                }
            })
            .collect::<Vec<_>>();
        let text = rows(s, &format!("select plan_table_output from table(dbms_xplan.display('plan_table', {q}, 'TYPICAL'))"))
            .await
            .map(|r| r.iter().map(|x| txt(x.first())).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        anyhow::Ok(Plan { nodes, text })
    }
    .await;
    // PLAN_TABLE is a session temp table in modern Oracle; clean up anyway (never commit user work).
    let _ = s.exec(&format!("delete from plan_table where statement_id = {q}")).await;
    result
}

fn pg_walk(v: &Value, parent: Option<i64>, next: &mut i64, out: &mut Vec<PlanNode>) {
    let id = *next;
    *next += 1;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let f = |k: &str| v.get(k).and_then(Value::as_f64);
    let rel = s("Relation Name");
    let object = if !s("Index Name").is_empty() {
        format!("INDEX {}", s("Index Name"))
    } else if !rel.is_empty() {
        let schema = s("Schema");
        let alias = s("Alias");
        let base = if schema.is_empty() { rel.clone() } else { format!("{schema}.{rel}") };
        if alias.is_empty() || alias == rel { format!("TABLE {base}") } else { format!("TABLE {base} {alias}") }
    } else {
        s("CTE Name").is_empty().then(String::new).unwrap_or_else(|| format!("CTE {}", s("CTE Name")))
    };
    let options = [s("Join Type"), s("Strategy"), s("Scan Direction")]
        .into_iter()
        .filter(|x| !x.is_empty() && x != "Forward" && x != "Plain")
        .collect::<Vec<_>>()
        .join(" ");
    let access = ["Index Cond", "Hash Cond", "Merge Cond", "Join Filter", "Recheck Cond"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str).map(|c| format!("{k}: {c}")))
        .collect::<Vec<_>>()
        .join("; ");
    let rows = f("Plan Rows");
    out.push(PlanNode {
        id,
        parent_id: parent,
        operation: s("Node Type"),
        options,
        object,
        cost: f("Total Cost"),
        bytes: rows.zip(f("Plan Width")).map(|(r, w)| r * w),
        cardinality: rows,
        time: None,
        optimizer: String::new(),
        access_predicates: access,
        filter_predicates: s("Filter"),
    });
    if let Some(children) = v.get("Plans").and_then(Value::as_array) {
        for c in children {
            pg_walk(c, Some(id), next, out);
        }
    }
}

async fn postgres(s: &dyn Session, sql: &str) -> Result<Plan> {
    let r = rows(s, &format!("EXPLAIN (FORMAT JSON) {}", strip(sql))).await?;
    let json: Value = serde_json::from_str(&txt(r.first().and_then(|x| x.first())))?;
    let root = &json[0]["Plan"];
    let mut nodes = vec![];
    let mut next = 0;
    // Synthetic statement root like Oracle's "SELECT STATEMENT".
    let kw = crate::drivers::leading_keyword(sql).to_ascii_uppercase();
    nodes.push(PlanNode {
        id: 0,
        operation: format!("{} STATEMENT", if kw.is_empty() { "SQL".into() } else { kw }),
        cost: root.get("Total Cost").and_then(Value::as_f64),
        cardinality: root.get("Plan Rows").and_then(Value::as_f64),
        bytes: root.get("Plan Rows").and_then(Value::as_f64).zip(root.get("Plan Width").and_then(Value::as_f64)).map(|(a, b)| a * b),
        ..Default::default()
    });
    next += 1;
    pg_walk(root, Some(0), &mut next, &mut nodes);
    let text = rows(s, &format!("EXPLAIN {}", strip(sql)))
        .await
        .map(|r| r.iter().map(|x| txt(x.first())).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    Ok(Plan { nodes, text })
}
